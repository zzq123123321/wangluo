// 阶段 5/6/7：长度前缀 JSON 协议。
// 帧格式：4 字节无符号大端整数（后续 UTF-8 JSON 的字节长度）+ N 字节 UTF-8 JSON。
// 接收端先验证长度（0 拒绝；>1 MiB 拒绝）再按长度分配缓冲，绝不按对方声明的任意长度直接分配。
// 未知字段：serde 默认忽略（低版本读高版本新增字段不报错）。
// 未知消息类型：解码不失败，由调用方安全忽略（不崩溃）。
// 本模块只负责协议数据结构、编码、解码和校验；连接生命周期见 network.rs。
use crate::error::AppError;
use crate::identity::{hex_encode, DEVICE_NAME_MAX_LEN};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;

pub const PROTOCOL_VERSION: u32 = 1;
/// 单条消息（JSON 正文）上限：1 MiB。
pub const MAX_MESSAGE_BYTES: u32 = 1024 * 1024;

pub const MSG_HELLO: &str = "hello";
pub const MSG_PING: &str = "ping";
pub const MSG_PONG: &str = "pong";
pub const MSG_DISCONNECT: &str = "disconnect";
pub const MSG_ERROR: &str = "error";
pub const MSG_CLIPBOARD_UPDATE: &str = "clipboard_update";

pub const REASON_USER_REQUESTED: &str = "user_requested";
pub const REASON_SHUTDOWN: &str = "shutdown";
pub const REASON_PROTOCOL_ERROR: &str = "protocol_error";
pub const REASON_VERSION_MISMATCH: &str = "version_mismatch";

/// hello 载荷：只含非敏感设备信息；device_secret / shared_key / 完整配置不得进入。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloPayload {
    pub device_id: String,
    pub device_name: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PingPayload {
    pub ping_id: String,
    pub sent_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PongPayload {
    pub ping_id: String,
    pub sent_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisconnectPayload {
    /// 非敏感原因：user_requested / shutdown / protocol_error
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorPayload {
    /// 非敏感原因类别：version_mismatch / protocol_error
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardPayload {
    /// 同步的剪贴板文字（已验证 ≤1 MiB、非空）
    pub text: String,
    /// text 的 SHA-256 hex：接收端校验完整性，非对端篡改
    pub content_hash: String,
}

/// 公共消息结构。`type`/`payload` 保留为原始值：
/// 未知消息类型与未知字段都能安全解码，由调用方决定忽略。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub version: u32,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub message_id: String,
    pub timestamp: u64,
    pub device_id: String,
    pub payload: serde_json::Value,
    /// 阶段 5 尚未认证：hello/ping/pong/disconnect 恒为 null；不得伪造 HMAC。
    pub auth: Option<String>,
}

impl Message {
    pub fn new(
        msg_type: &str,
        device_id: &str,
        payload: &impl Serialize,
    ) -> Result<Self, AppError> {
        let payload = serde_json::to_value(payload).map_err(|e| AppError::new(e.to_string()))?;
        Ok(Self {
            version: PROTOCOL_VERSION,
            msg_type: msg_type.to_string(),
            message_id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_millis(),
            device_id: device_id.to_string(),
            payload,
            auth: None,
        })
    }

    /// 置入固定 message_id（去重/重放测试用；生产消息由 new 自动生成 UUID）。
    pub fn with_message_id(mut self, id: String) -> Self {
        self.message_id = id;
        self
    }
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 编码：先完成 JSON 序列化并检查最终字节长度（>1 MiB 拒绝），再加 4 字节大端长度前缀。
pub fn encode(msg: &Message) -> Result<Vec<u8>, AppError> {
    let json = serde_json::to_vec(msg).map_err(|e| AppError::new(e.to_string()))?;
    if (json.len() as u32) > MAX_MESSAGE_BYTES {
        return Err(AppError::ProtocolMessageTooLarge);
    }
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&((json.len() as u32).to_be_bytes()));
    out.extend_from_slice(&json);
    Ok(out)
}

/// 解码一帧：先读 4 字节长度头并验证（0 与 >1 MiB 拒绝，先于正文缓冲分配），
/// 再按准确长度读取正文，UTF-8 与 JSON 校验。EOF/半帧统一为 ConnectionClosed。
pub async fn read_message<R>(reader: &mut R) -> Result<Message, AppError>
where
    R: AsyncRead + Unpin,
{
    let mut hdr = [0u8; 4];
    reader
        .read_exact(&mut hdr)
        .await
        .map_err(|_| AppError::ConnectionClosed)?;
    let len = u32::from_be_bytes(hdr);
    if len == 0 {
        return Err(AppError::ProtocolInvalidLength);
    }
    if len > MAX_MESSAGE_BYTES {
        return Err(AppError::ProtocolMessageTooLarge);
    }
    let mut body = vec![0u8; len as usize];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|_| AppError::ConnectionClosed)?;
    let text = String::from_utf8(body).map_err(|_| AppError::ProtocolInvalidUtf8)?;
    let msg: Message = serde_json::from_str(&text).map_err(|_| AppError::ProtocolInvalidJson)?;
    Ok(msg)
}

pub fn parse_ping(msg: &Message) -> Option<PingPayload> {
    serde_json::from_value(msg.payload.clone()).ok()
}

pub fn parse_pong(msg: &Message) -> Option<PongPayload> {
    serde_json::from_value(msg.payload.clone()).ok()
}

/// hello 校验：版本为 1、type 为 hello、payload 协议版本为 1、device_id 为合法 UUID、
/// device_name 非空且不超长、对方 device_id 不等于本机。
pub fn validate_hello(msg: &Message, local_device_id: &str) -> Result<HelloPayload, AppError> {
    if msg.version != PROTOCOL_VERSION {
        return Err(AppError::ProtocolVersionMismatch);
    }
    if msg.msg_type != MSG_HELLO {
        return Err(AppError::InvalidHello);
    }
    let payload: HelloPayload =
        serde_json::from_value(msg.payload.clone()).map_err(|_| AppError::InvalidHello)?;
    if payload.protocol_version != PROTOCOL_VERSION {
        return Err(AppError::InvalidHello);
    }
    if uuid::Uuid::parse_str(&payload.device_id).is_err() {
        return Err(AppError::InvalidHello);
    }
    let name = payload.device_name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidHello);
    }
    if name.chars().count() > DEVICE_NAME_MAX_LEN {
        return Err(AppError::InvalidHello);
    }
    if payload.device_id == local_device_id {
        return Err(AppError::InvalidHello);
    }
    Ok(payload)
}

/// 校验 clipboard_update 消息：text 非空、不超 1 MiB、content_hash 为 text 的合法 SHA-256 hex。
pub fn validate_clipboard(msg: &Message) -> Result<ClipboardPayload, AppError> {
    if msg.version != PROTOCOL_VERSION {
        return Err(AppError::ProtocolVersionMismatch);
    }
    if msg.msg_type != MSG_CLIPBOARD_UPDATE {
        return Err(AppError::ProtocolInvalidJson);
    }
    let p: ClipboardPayload =
        serde_json::from_value(msg.payload.clone()).map_err(|_| AppError::ProtocolInvalidJson)?;
    if p.text.is_empty() {
        return Err(AppError::ProtocolInvalidJson);
    }
    if p.text.len() > MAX_MESSAGE_BYTES as usize {
        return Err(AppError::ProtocolMessageTooLarge);
    }
    let mut hasher = Sha256::new();
    hasher.update(p.text.as_bytes());
    let actual = hex_encode(&hasher.finalize());
    if !p.content_hash.eq_ignore_ascii_case(&actual) {
        return Err(AppError::ProtocolInvalidJson);
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;

    /// 测试用异步读取器：每次 poll 最多返回 chunk 字节，模拟 TCP 分片；耗尽后 EOF。
    struct FragReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl FragReader {
        fn new(data: Vec<u8>, chunk: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk: chunk.max(1),
            }
        }
    }

    impl AsyncRead for FragReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let me = self.get_mut();
            if me.pos >= me.data.len() {
                return Poll::Ready(Ok(()));
            }
            // 只能放入 ReadBuf 剩余空间内的字节（模拟真实读取器的行为）
            let n = buf.remaining().min(me.chunk).min(me.data.len() - me.pos);
            if n == 0 {
                return Poll::Ready(Ok(()));
            }
            buf.put_slice(&me.data[me.pos..me.pos + n]);
            me.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    const LOCAL_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const PEER_ID: &str = "650e8400-e29b-41d4-a716-446655440001";

    fn hello_msg() -> Message {
        Message::new(
            MSG_HELLO,
            LOCAL_ID,
            &HelloPayload {
                device_id: PEER_ID.into(),
                device_name: "DESKTOP-B".into(),
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap()
    }

    fn ping_msg() -> Message {
        Message::new(
            MSG_PING,
            LOCAL_ID,
            &PingPayload {
                ping_id: "p-1".into(),
                sent_at: 1789550000000,
            },
        )
        .unwrap()
    }

    fn pong_msg() -> Message {
        Message::new(
            MSG_PONG,
            LOCAL_ID,
            &PongPayload {
                ping_id: "p-1".into(),
                sent_at: 1789550000000,
            },
        )
        .unwrap()
    }

    fn disconnect_msg() -> Message {
        Message::new(
            MSG_DISCONNECT,
            LOCAL_ID,
            &DisconnectPayload {
                reason: REASON_USER_REQUESTED.into(),
            },
        )
        .unwrap()
    }

    fn clipboard_msg(text: &str, hash: &str) -> Message {
        Message::new(
            MSG_CLIPBOARD_UPDATE,
            LOCAL_ID,
            &ClipboardPayload {
                text: text.into(),
                content_hash: hash.into(),
            },
        )
        .unwrap()
    }

    /// 与发送端相同方式计算 SHA-256 hex（用于构建合法校验 payload）。
    fn sha256_hex(text: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hex_encode(&hasher.finalize())
    }

    // 1/2/3/4：hello / ping / pong / disconnect 正常往返（分片读取）
    #[test]
    fn roundtrip_all_message_types() {
        let rt = runtime();
        for (i, m) in [hello_msg(), ping_msg(), pong_msg(), disconnect_msg()]
            .into_iter()
            .enumerate()
        {
            let bytes = encode(&m).unwrap();
            let want_type = m.msg_type.clone();
            let want_auth = m.auth.clone();
            let want_payload = m.payload.clone();
            let want_id = m.message_id.clone();
            rt.block_on(async {
                let mut r = FragReader::new(bytes, 3);
                let got = read_message(&mut r).await.unwrap();
                assert_eq!(got.msg_type, want_type, "case {i}");
                assert_eq!(got.auth, want_auth, "case {i}");
                assert_eq!(got.payload, want_payload, "case {i}");
                assert_eq!(got.message_id, want_id, "case {i}");
                assert_eq!(got.version, PROTOCOL_VERSION);
                assert_eq!(got.device_id, LOCAL_ID);
            });
        }
    }

    // 5：长度前缀为 4 字节无符号大端
    #[test]
    fn length_prefix_is_big_endian() {
        let m = ping_msg();
        let bytes = encode(&m).unwrap();
        let json_len = bytes.len() - 4;
        assert_eq!(
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            json_len as u32
        );
        // 前缀后的正文与同一消息的 JSON 序列化逐字节一致
        assert_eq!(&bytes[4..], &serde_json::to_vec(&m).unwrap()[..]);
    }

    // 1/6：clipboard_update 消息加后缀时文本不变（分片往返 + 校验）
    #[test]
    fn clipboard_roundtrip_and_validate() {
        let rt = runtime();
        let m = clipboard_msg(
            "你好，世界 🎉\n第二行",
            &sha256_hex("你好，世界 🎉\n第二行"),
        );
        let bytes = encode(&m).unwrap();
        rt.block_on(async {
            let mut r = FragReader::new(bytes, 3);
            let got = read_message(&mut r).await.unwrap();
            assert_eq!(got.msg_type, MSG_CLIPBOARD_UPDATE);
            let p = validate_clipboard(&got).unwrap();
            assert_eq!(p.text, "你好，世界 🎉\n第二行");
            assert_eq!(p.content_hash, sha256_hex("你好，世界 🎉\n第二行"));
        });
    }

    // 阶段 7：空文本 / 超 1 MiB / 类型不匹配 / 哈希不匹配均被拒绝
    #[test]
    fn clipboard_validation_rejects_bad_input() {
        assert!(matches!(
            validate_clipboard(&clipboard_msg("hello", &sha256_hex("hello"))),
            Ok(p) if p.text == "hello"
        ));
        // 空文本
        let e = validate_clipboard(&clipboard_msg("", &sha256_hex(""))).unwrap_err();
        assert_eq!(e.code(), AppError::ProtocolInvalidJson.code());
        // 超 1 MiB 文本
        let big = "x".repeat(MAX_MESSAGE_BYTES as usize + 1);
        let e = validate_clipboard(&clipboard_msg(&big, &sha256_hex(&big))).unwrap_err();
        assert_eq!(e.code(), AppError::ProtocolMessageTooLarge.code());
        // 空 content_hash
        let e = validate_clipboard(&clipboard_msg("hello", "")).unwrap_err();
        assert_eq!(e.code(), AppError::ProtocolInvalidJson.code());
        // content_hash 与实际 SHA-256 不匹配
        let e = validate_clipboard(&clipboard_msg("hello", "not-a-hash")).unwrap_err();
        assert_eq!(e.code(), AppError::ProtocolInvalidJson.code());
        // 类型必须是 clipboard_update
        let ping = ping_msg();
        let e = validate_clipboard(&ping).unwrap_err();
        assert_eq!(e.code(), AppError::ProtocolInvalidJson.code());
    }
    #[test]
    fn fragmented_reads_succeed() {
        let bytes = encode(&hello_msg()).unwrap();
        let rt = runtime();
        for chunk in [1usize, 2, 7, 4096] {
            rt.block_on(async {
                let mut r = FragReader::new(bytes.clone(), chunk);
                let m = read_message(&mut r).await.unwrap();
                assert_eq!(m.msg_type, MSG_HELLO);
            });
        }
    }

    // 7：长度为 0 被拒绝
    #[test]
    fn zero_length_rejected() {
        let rt = runtime();
        rt.block_on(async {
            let mut r = FragReader::new(vec![0, 0, 0, 0], 4);
            assert!(matches!(
                read_message(&mut r).await,
                Err(AppError::ProtocolInvalidLength)
            ));
        });
    }

    // 8：长度超过 1 MiB 被拒绝，且不会按声明长度分配（头后无正文也立即返回）
    #[test]
    fn overlong_length_rejected_without_allocation() {
        let rt = runtime();
        rt.block_on(async {
            let mut r = FragReader::new(vec![0xFF, 0xFF, 0xFF, 0xFE], 4);
            assert!(matches!(
                read_message(&mut r).await,
                Err(AppError::ProtocolMessageTooLarge)
            ));
            let mut r = FragReader::new(vec![0xFF, 0xFF, 0xFF, 0xFF], 1);
            assert!(matches!(
                read_message(&mut r).await,
                Err(AppError::ProtocolMessageTooLarge)
            ));
        });
    }

    // 9：正文截断（长度头声称 100 字节，实际只给 3 字节）被识别为连接结束
    #[test]
    fn truncated_body_detected() {
        let rt = runtime();
        rt.block_on(async {
            let mut data = vec![0, 0, 0, 100, 1, 2, 3];
            let mut r = FragReader::new(std::mem::take(&mut data), 2);
            assert!(matches!(
                read_message(&mut r).await,
                Err(AppError::ConnectionClosed)
            ));
        });
    }

    // 10：非 UTF-8 正文被拒绝
    #[test]
    fn invalid_utf8_rejected() {
        let rt = runtime();
        rt.block_on(async {
            let mut data = vec![0, 0, 0, 3, 0xFF, 0xFE, 0xFD];
            let mut r = FragReader::new(std::mem::take(&mut data), 2);
            assert!(matches!(
                read_message(&mut r).await,
                Err(AppError::ProtocolInvalidUtf8)
            ));
        });
    }

    // 11：合法 UTF-8 但非法 JSON 被拒绝（不 panic）
    #[test]
    fn invalid_json_rejected() {
        let rt = runtime();
        rt.block_on(async {
            let body = b"this is not json {";
            let mut data = Vec::new();
            data.extend_from_slice(&((body.len() as u32).to_be_bytes()));
            data.extend_from_slice(body);
            let mut r = FragReader::new(data, 5);
            assert!(matches!(
                read_message(&mut r).await,
                Err(AppError::ProtocolInvalidJson)
            ));
        });
    }

    // 12：未知消息类型解码不失败（安全处理留给调用方忽略）
    #[test]
    fn unknown_message_type_decodes_safely() {
        let rt = runtime();
        let m = Message {
            version: PROTOCOL_VERSION,
            msg_type: "pair_request".into(),
            message_id: uuid::Uuid::new_v4().to_string(),
            timestamp: 1,
            device_id: PEER_ID.into(),
            payload: serde_json::json!({"future": true}),
            auth: None,
        };
        let bytes = encode(&m).unwrap();
        rt.block_on(async {
            let mut r = FragReader::new(bytes, 11);
            let got = read_message(&mut r).await.unwrap();
            assert_eq!(got.msg_type, "pair_request");
        });
    }

    // 13：协议版本不是 1 被拒绝（validate_hello 报 VersionMismatch）
    #[test]
    fn version_mismatch_rejected() {
        let mut m = hello_msg();
        m.version = 2;
        let e = validate_hello(&m, LOCAL_ID).unwrap_err();
        assert!(matches!(e, AppError::ProtocolVersionMismatch));
    }

    // 14：编码后超过 1 MiB 上限被拒绝
    #[test]
    fn oversized_encode_rejected() {
        let big = "x".repeat(MAX_MESSAGE_BYTES as usize + 1);
        let m = Message::new(
            "clipboard_update",
            LOCAL_ID,
            &serde_json::json!({"text": big}),
        )
        .unwrap();
        assert!(matches!(encode(&m), Err(AppError::ProtocolMessageTooLarge)));
    }

    // 15：auth 为 null 时序列化与反序列化正确（字段存在且为 null）
    #[test]
    fn null_auth_roundtrip() {
        let m = ping_msg();
        assert!(m.auth.is_none());
        let text = serde_json::to_string(&m).unwrap();
        assert!(text.contains("\"auth\":null"));
        let back: Message = serde_json::from_str(&text).unwrap();
        assert!(back.auth.is_none());
    }

    // 16：合法 hello 通过校验
    #[test]
    fn valid_hello_passes() {
        let p = validate_hello(&hello_msg(), LOCAL_ID).unwrap();
        assert_eq!(p.device_id, PEER_ID);
        assert_eq!(p.device_name, "DESKTOP-B");
        assert_eq!(p.protocol_version, PROTOCOL_VERSION);
    }

    // 17：非法 UUID 被拒绝
    #[test]
    fn hello_invalid_uuid_rejected() {
        let m = Message::new(
            MSG_HELLO,
            LOCAL_ID,
            &HelloPayload {
                device_id: "not-a-uuid".into(),
                device_name: "B".into(),
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert!(matches!(
            validate_hello(&m, LOCAL_ID),
            Err(AppError::InvalidHello)
        ));
    }

    // 18：空设备名被拒绝
    #[test]
    fn hello_empty_name_rejected() {
        for name in ["", "   "] {
            let m = Message::new(
                MSG_HELLO,
                LOCAL_ID,
                &HelloPayload {
                    device_id: PEER_ID.into(),
                    device_name: name.into(),
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .unwrap();
            assert!(matches!(
                validate_hello(&m, LOCAL_ID),
                Err(AppError::InvalidHello)
            ));
        }
    }

    // 19：超长设备名被拒绝
    #[test]
    fn hello_long_name_rejected() {
        let name = "N".repeat(DEVICE_NAME_MAX_LEN + 1);
        let m = Message::new(
            MSG_HELLO,
            LOCAL_ID,
            &HelloPayload {
                device_id: PEER_ID.into(),
                device_name: name,
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert!(matches!(
            validate_hello(&m, LOCAL_ID),
            Err(AppError::InvalidHello)
        ));
        // 恰好 64 字符通过
        let ok = Message::new(
            MSG_HELLO,
            LOCAL_ID,
            &HelloPayload {
                device_id: PEER_ID.into(),
                device_name: "N".repeat(DEVICE_NAME_MAX_LEN),
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert!(validate_hello(&ok, LOCAL_ID).is_ok());
    }

    // 20：对方 device_id 与本机相同被拒绝
    #[test]
    fn hello_same_device_id_rejected() {
        let m = Message::new(
            MSG_HELLO,
            LOCAL_ID,
            &HelloPayload {
                device_id: LOCAL_ID.into(),
                device_name: "B".into(),
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert!(matches!(
            validate_hello(&m, LOCAL_ID),
            Err(AppError::InvalidHello)
        ));
    }

    // 21：payload 协议版本不匹配被拒绝
    #[test]
    fn hello_payload_version_mismatch_rejected() {
        let m = Message::new(
            MSG_HELLO,
            LOCAL_ID,
            &HelloPayload {
                device_id: PEER_ID.into(),
                device_name: "B".into(),
                protocol_version: 2,
            },
        )
        .unwrap();
        assert!(matches!(
            validate_hello(&m, LOCAL_ID),
            Err(AppError::InvalidHello)
        ));
    }
}
