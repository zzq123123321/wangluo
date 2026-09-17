// 阶段 5 集成测试：本机环回（127.0.0.1 + 随机空闲端口）验证 NetworkManager 全链路。
// 正式运行逻辑仍绑定 ZeroTier IP；此处仅测试允许使用环回地址。
// 对端（client）用裸 TcpStream + protocol 编解码模拟，测试身份/密钥均为测试数据。
use cliplink_lib::identity::hex_encode;
use cliplink_lib::network::{ConnectionStatusEvent, ManagerConfig, NetworkManager, StatusSink};
use cliplink_lib::protocol::{
    self, ClipboardPayload, DisconnectPayload, HelloPayload, Message, PingPayload, PongPayload,
};
use cliplink_lib::state::ConnectionStatus;
use sha2::{Digest, Sha256};
use std::marker::Unpin;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const LOCAL_DEVICE_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const PEER_DEVICE_ID: &str = "6ba7b810-9dad-41d1-80b4-00c04fd430c8";

struct CollectSink {
    events: Mutex<Vec<ConnectionStatusEvent>>,
}

impl CollectSink {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
    fn last(&self) -> Option<ConnectionStatusEvent> {
        self.events.lock().unwrap().last().cloned()
    }
}

impl StatusSink for CollectSink {
    fn on_status(&self, ev: &ConnectionStatusEvent) {
        self.events.lock().unwrap().push(ev.clone());
    }
}

async fn wait_for(sink: &CollectSink, f: impl Fn(&ConnectionStatusEvent) -> bool) -> bool {
    for _ in 0..120 {
        if let Some(ev) = sink.last() {
            if f(&ev) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn test_manager(sink: Arc<CollectSink>) -> NetworkManager {
    let m = NetworkManager::new(
        LOCAL_DEVICE_ID.to_string(),
        "TEST-A".to_string(),
        ManagerConfig {
            port: 0,
            heartbeat_secs: 1,
            handshake_timeout: Duration::from_millis(500),
            connect_timeout: Duration::from_millis(500),
            reconnect: false,
        },
        sink,
    );
    // 固定 ZT IP 提供器：用户主动断开后 Offline 文案确定（“等待输入对方 IP。”）
    m.set_zt_provider(Arc::new(|| Some("192.168.191.180".to_string())));
    m
}

fn peer_hello() -> Message {
    Message::new(
        "hello",
        PEER_DEVICE_ID,
        &HelloPayload {
            device_id: PEER_DEVICE_ID.to_string(),
            device_name: "TEST-PEER-B".to_string(),
            protocol_version: 1,
        },
    )
    .unwrap()
}

async fn client_read(stream: &mut (impl AsyncReadExt + Unpin)) -> Message {
    tokio::time::timeout(Duration::from_secs(3), protocol::read_message(stream))
        .await
        .expect("3 秒内未收到帧")
        .expect("读取帧出错")
}

async fn client_send(stream: &mut (impl AsyncWriteExt + Unpin), msg: &Message) {
    let b = protocol::encode(msg).unwrap();
    stream.write_all(&b).await.unwrap();
}

/// 等待指定类型帧；期间对管理端的 ping 自动回 pong（保持心跳），其他类型视为异常。
async fn client_await(
    rd: &mut (impl AsyncReadExt + Unpin),
    wr: &mut (impl AsyncWriteExt + Unpin),
    want: &str,
) -> Message {
    loop {
        let m = client_read(rd).await;
        if m.msg_type == want {
            return m;
        }
        if m.msg_type == "ping" {
            let p: PingPayload = serde_json::from_value(m.payload).unwrap();
            client_send(
                wr,
                &Message::new(
                    "pong",
                    PEER_DEVICE_ID,
                    &PongPayload {
                        ping_id: p.ping_id,
                        sent_at: p.sent_at,
                    },
                )
                .unwrap(),
            )
            .await;
            continue;
        }
        panic!("收到意外帧类型: {}", m.msg_type);
    }
}

/// 建立入站连接并完成双向 hello（返回 client 的读写半部、监听地址与 client 本地地址——
/// 管理端记录的 peer.ip 是 client 侧地址，不是监听地址）。
async fn establish_inbound(
    m: &NetworkManager,
) -> (
    tokio::net::tcp::OwnedReadHalf,
    tokio::net::tcp::OwnedWriteHalf,
    SocketAddr,
    SocketAddr,
) {
    m.sync_listener(Some("127.0.0.1")).await;
    let addr = m.listener_local_addr().unwrap();
    let stream = TcpStream::connect(addr)
        .await
        .expect("client 连接监听端口失败");
    let peer_addr = stream.local_addr().unwrap();
    let (mut rd, mut wr) = stream.into_split();
    client_send(&mut wr, &peer_hello()).await;
    let mhello = client_await(&mut rd, &mut wr, "hello").await;
    protocol::validate_hello(&mhello, PEER_DEVICE_ID).expect("管理端 hello 校验失败");
    let hp: HelloPayload = serde_json::from_value(mhello.payload).unwrap();
    assert_eq!(hp.device_id, LOCAL_DEVICE_ID);
    assert_eq!(hp.device_name, "TEST-A");
    assert_eq!(hp.protocol_version, 1);
    (rd, wr, addr, peer_addr)
}

// 1/2/3：listener 与 client 建立 TCP，双方交换 hello，双方获得对方设备摘要；
// ping/pong 正常；hello 成功后进入 Connected。
#[test]
fn inbound_hello_ping_pong() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let (mut crd, mut cwr, laddr, paddr) = establish_inbound(&m).await;
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Connected
                    && e.peer.as_ref().is_some_and(|p| {
                        p.device_name == "TEST-PEER-B" && p.ip == paddr.to_string()
                    })
            })
            .await,
            "未进入 Connected: {:?}",
            sink.last()
        );
        let ev = sink.last().unwrap();
        assert_eq!(ev.status, ConnectionStatus::Connected);

        // 客户端 ping → 管理端返回对应 pong
        client_send(
            &mut cwr,
            &Message::new(
                "ping",
                PEER_DEVICE_ID,
                &PingPayload {
                    ping_id: "it-ping-1".into(),
                    sent_at: 1789550000000,
                },
            )
            .unwrap(),
        )
        .await;
        let pong = client_await(&mut crd, &mut cwr, "pong").await;
        let pp: PongPayload = serde_json::from_value(pong.payload).unwrap();
        assert_eq!(pp.ping_id, "it-ping-1");
        assert_eq!(pp.sent_at, 1789550000000);

        // 管理端心跳 ping → 客户端回 pong，连接保持 Connected
        let hp = client_await(&mut crd, &mut cwr, "ping").await;
        let p: PingPayload = serde_json::from_value(hp.payload).unwrap();
        client_send(
            &mut cwr,
            &Message::new(
                "pong",
                PEER_DEVICE_ID,
                &PongPayload {
                    ping_id: p.ping_id,
                    sent_at: p.sent_at,
                },
            )
            .unwrap(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sink.last().unwrap().status, ConnectionStatus::Connected);

        // 对方正常 disconnect → 本端退出（Offline，对方已断开）
        client_send(
            &mut cwr,
            &Message::new(
                "disconnect",
                PEER_DEVICE_ID,
                &DisconnectPayload {
                    reason: "user_requested".into(),
                },
            )
            .unwrap(),
        )
        .await;
        drop(cwr);
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Offline && e.status_text.contains("对方已断开")
            })
            .await
        );
        assert!(!m.is_busy());
        m.shutdown().await;
        // 端口已释放
        let reb = TcpListener::bind(laddr).await.unwrap();
        drop(reb);
    });
}

// 4/5：出站连接 + hello 交换 + 用户主动断开（管理端发送 disconnect 消息）
#[test]
fn outbound_connect_and_user_disconnect() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listen.local_addr().unwrap().port();
        m.connect("127.0.0.1", port).await.unwrap();
        let (stream, _) = listen.accept().await.unwrap();
        let (mut crd, mut cwr) = stream.into_split();
        // 管理端（出站）先发送 hello
        let mhello = client_await(&mut crd, &mut cwr, "hello").await;
        protocol::validate_hello(&mhello, PEER_DEVICE_ID).unwrap();
        client_send(&mut cwr, &peer_hello()).await;
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Connected
                    && e.peer
                        .as_ref()
                        .is_some_and(|p| p.device_name == "TEST-PEER-B")
            })
            .await
        );
        // 用户主动断开：管理端发送 disconnect 并回到 Offline
        // （心跳 ping 可能先于 disconnect 到达，client_await 会自动回 pong 并继续等待）
        m.disconnect().await;
        let dc = client_await(&mut crd, &mut cwr, "disconnect").await;
        assert_eq!(dc.msg_type, "disconnect");
        let dp: DisconnectPayload = serde_json::from_value(dc.payload).unwrap();
        assert_eq!(dp.reason, "user_requested");
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Offline && e.error_code.is_none()
            })
            .await
        );
        assert!(!m.is_busy());
        m.shutdown().await;
    });
}

// 6：超长帧（长度 > 1 MiB）使连接被安全关闭，不崩溃
#[test]
fn overlong_frame_closes_connection() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let (crd, mut cwr, addr, _paddr) = establish_inbound(&m).await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Connected).await);
        let mut frame = vec![0x00, 0x10, 0x00, 0x01]; // 长度 = 1 MiB + 1
        frame.push(b'x'); // 正文不完整也没关系：长度先被拒绝
        cwr.write_all(&frame).await.unwrap();
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Error
                    && e.error_code.as_deref() == Some("protocol_message_too_large")
            })
            .await
        );
        assert!(!m.is_busy());
        drop(crd);
        m.shutdown().await;
        let reb = TcpListener::bind(addr).await.unwrap();
        drop(reb);
    });
}

// 7：非法 JSON / 非 UTF-8 帧不导致进程崩溃，连接被安全关闭
#[test]
fn invalid_json_and_utf8_no_crash() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let (_crd, mut cwr, _addr, _paddr) = establish_inbound(&m).await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Connected).await);
        let body = b"this is not json";
        let mut frame = Vec::new();
        frame.extend_from_slice(&((body.len() as u32).to_be_bytes()));
        frame.extend_from_slice(body);
        cwr.write_all(&frame).await.unwrap();
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Error
                    && e.error_code.as_deref() == Some("protocol_invalid_json")
            })
            .await
        );
    });
    // 第二台管理器：非 UTF-8
    let sink2 = Arc::new(CollectSink::new());
    let m2 = test_manager(sink2.clone());
    tauri::async_runtime::block_on(async move {
        let (_crd, mut cwr, _addr, _paddr) = establish_inbound(&m2).await;
        assert!(wait_for(&sink2, |e| e.status == ConnectionStatus::Connected).await);
        let mut frame = vec![0, 0, 0, 3, 0xFF, 0xFE, 0xFD];
        cwr.write_all(&mut frame).await.unwrap();
        assert!(
            wait_for(&sink2, |e| {
                e.status == ConnectionStatus::Error
                    && e.error_code.as_deref() == Some("protocol_invalid_utf8")
            })
            .await
        );
        m2.shutdown().await;
    });
}

// 8：TCP 建立后不发 hello → 握手超时（入站与出站）
#[test]
fn no_hello_handshake_timeout() {
    // 入站
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        m.sync_listener(Some("127.0.0.1")).await;
        let addr = m.listener_local_addr().unwrap();
        let _stream = TcpStream::connect(addr).await.unwrap(); // 连接后不发任何帧
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Error
                    && e.error_code.as_deref() == Some("handshake_timeout")
            })
            .await
        );
        assert!(!m.is_busy());
        m.shutdown().await;
    });
    // 出站
    let sink2 = Arc::new(CollectSink::new());
    let m2 = test_manager(sink2.clone());
    tauri::async_runtime::block_on(async move {
        let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listen.local_addr().unwrap().port();
        m2.connect("127.0.0.1", port).await.unwrap();
        let (_stream, _) = listen.accept().await.unwrap(); // 接受后不发 hello
        assert!(
            wait_for(&sink2, |e| {
                e.status == ConnectionStatus::Error
                    && e.error_code.as_deref() == Some("handshake_timeout")
            })
            .await
        );
        assert!(!m2.is_busy());
        m2.shutdown().await;
    });
}

// 9：对端关闭 socket 后 reader 正常结束（Error：连接已中断）
#[test]
fn peer_socket_close_detected() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let (_crd, cwr, _addr, _paddr) = establish_inbound(&m).await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Connected).await);
        drop(cwr);
        drop(_crd);
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Error).await);
        assert!(!m.is_busy());
        m.shutdown().await;
    });
}

// 10：已有活动连接时，新的入站连接被拒绝/关闭
#[test]
fn inbound_rejected_when_active() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let (_crd, _cwr, addr, _paddr) = establish_inbound(&m).await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Connected).await);
        // 第二个 client 连接 → 被管理端立即关闭
        let second = TcpStream::connect(addr).await.unwrap();
        let (mut srd, _swr) = second.into_split();
        let _ = _swr;
        let mut buf = [0u8; 16];
        let r = tokio::time::timeout(Duration::from_secs(2), srd.read(&mut buf)).await;
        assert!(r.is_ok(), "第二个入站连接未在 2 秒内被关闭");
        drop(srd);
        m.disconnect().await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Offline).await);
        m.shutdown().await;
    });
}

// 11：出站连接被拒绝 → Error（对方没有运行 ClipLink / 被防火墙阻止）
#[test]
fn outbound_connect_refused() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listen.local_addr().unwrap().port();
        drop(listen); // 释放端口 → 连接将被拒绝
        m.connect("127.0.0.1", port).await.unwrap();
        assert!(
            wait_for(&sink, |e| {
                e.status == ConnectionStatus::Error
                    && e.error_code
                        .as_deref()
                        .is_some_and(|c| c == "connect_failed" || c == "connect_timeout")
            })
            .await
        );
        assert!(!m.is_busy());
        m.shutdown().await;
    });
}

// 12：不可达地址 → 超时或失败，不永久卡住
#[test]
fn outbound_unreachable_fails_bounded() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        m.connect("192.0.2.1", 45888).await.unwrap(); // TEST-NET-1：不可达
        assert!(
            wait_for(&sink, |e| e.status == ConnectionStatus::Error).await,
            "不可达地址未在限定时间内得到结果: {:?}",
            sink.last()
        );
        assert!(!m.is_busy());
        m.shutdown().await;
    });
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex_encode(&hasher.finalize())
}

fn clipboard_msg_with_id(message_id: &str, text: &str) -> Message {
    Message::new(
        "clipboard_update",
        PEER_DEVICE_ID,
        &ClipboardPayload {
            text: text.to_string(),
            content_hash: sha256_hex(text),
        },
    )
    .unwrap()
    .with_message_id(message_id.to_string())
}

// 阶段 7：对方发送 clipboard_update → 本端校验（哈希/大小/消息 ID 去重）→ 回调落地；
// 重复 ID 不重复落地；非法载荷不中断连接；中文/Emoji/多行文字保持不变。
#[test]
fn clipboard_update_flow_and_dedup() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    // 拦截落地回调：记录收到的载荷（不触碰真实系统剪贴板）
    let landed: Arc<Mutex<Vec<ClipboardPayload>>> = Arc::new(Mutex::new(Vec::new()));
    let landed2 = landed.clone();
    m.set_clipboard_landing(Arc::new(move |p| {
        landed2.lock().unwrap().push(p.clone());
    }));
    tauri::async_runtime::block_on(async move {
        let (_crd, mut cwr, _addr, _paddr) = establish_inbound(&m).await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Connected).await);

        // 中文 + Emoji + 多行：内容原样到达
        let unicode = "你好，世界 🎉\n第二行\tTab";
        let msg = clipboard_msg_with_id("cb-1", unicode);
        client_send(&mut cwr, &msg).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        {
            let got = landed.lock().unwrap();
            assert_eq!(got.len(), 1, "应落地 1 次");
            assert_eq!(got[0].text, unicode);
            assert_eq!(got[0].content_hash, sha256_hex(unicode));
        }

        // 同一消息 ID 重复投递：不再落地
        client_send(&mut cwr, &msg).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(landed.lock().unwrap().len(), 1, "重复 ID 不应再次落地");

        // 不同 ID、相同内容：仍是新消息（内容哈希去重在 state 层），此处落地一次
        let msg2 = clipboard_msg_with_id("cb-2", unicode);
        client_send(&mut cwr, &msg2).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(landed.lock().unwrap().len(), 2, "新 ID 同内容应落地");

        // 非法载荷（空文本）：忽略且不中断连接
        let bad = Message::new(
            "clipboard_update",
            PEER_DEVICE_ID,
            &ClipboardPayload {
                text: String::new(),
                content_hash: sha256_hex(""),
            },
        )
        .unwrap()
        .with_message_id("cb-bad".to_string());
        client_send(&mut cwr, &bad).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(landed.lock().unwrap().len(), 2, "空文本不应落地");
        assert_eq!(
            sink.last().unwrap().status,
            ConnectionStatus::Connected,
            "非法载荷不应断开连接"
        );

        // 哈希不匹配的载荷：忽略且不中断连接
        let tampered = Message::new(
            "clipboard_update",
            PEER_DEVICE_ID,
            &ClipboardPayload {
                text: "real-content".to_string(),
                content_hash: "deadbeef".to_string(),
            },
        )
        .unwrap()
        .with_message_id("cb-tampered".to_string());
        client_send(&mut cwr, &tampered).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(landed.lock().unwrap().len(), 2, "哈希不匹配不应落地");
        assert_eq!(sink.last().unwrap().status, ConnectionStatus::Connected);

        m.disconnect().await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Offline).await);
        m.shutdown().await;
    });
}

// 阶段 7：本端 send_message 发送 clipboard_update → 对方实际收到同一内容（A→B 方向）
#[test]
fn outbound_clipboard_update_reaches_peer() {
    let sink = Arc::new(CollectSink::new());
    let m = test_manager(sink.clone());
    tauri::async_runtime::block_on(async move {
        let (mut crd, mut cwr, _addr, _paddr) = establish_inbound(&m).await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Connected).await);

        let text = "A → B 同步 🎉";
        let msg = Message::new(
            "clipboard_update",
            LOCAL_DEVICE_ID,
            &ClipboardPayload {
                text: text.to_string(),
                content_hash: sha256_hex(text),
            },
        )
        .unwrap();
        m.send_message(&msg).unwrap();
        // 对方读取到 clipboard_update，内容一致（期间心跳 ping 自动处理）
        let got = client_await(&mut crd, &mut cwr, "clipboard_update").await;
        let p: ClipboardPayload = serde_json::from_value(got.payload).unwrap();
        assert_eq!(p.text, text);
        assert_eq!(p.content_hash, sha256_hex(text));

        m.disconnect().await;
        assert!(wait_for(&sink, |e| e.status == ConnectionStatus::Offline).await);
        m.shutdown().await;
    });
}
