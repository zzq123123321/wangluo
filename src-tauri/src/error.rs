// 阶段 5：统一错误类型。枚举变体对应稳定错误码（code），序列化给前端的是
// 固定、不含内部错误链与秘密材料的用户文案。AppError::new 归入 Other（透传文案）。
use serde::{Serialize, Serializer};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    /// 连接目标不是合法 IPv4（含回环/链路本地/广播/组播/保留/0.0.0.0）
    InvalidPeerIp,
    /// 连接目标是本机当前 ZeroTier IPv4
    SelfConnection,
    /// 已有活动连接（或正在建立），新连接请求被拒绝
    AlreadyConnected,
    /// TCP 连接超时（5 秒）
    ConnectTimeout,
    /// TCP 连接被拒绝等失败
    ConnectFailed,
    /// 监听端口绑定失败
    BindFailed,
    /// 单条消息超过 1 MiB 上限
    ProtocolMessageTooLarge,
    /// 长度前缀为 0
    ProtocolInvalidLength,
    /// 正文不是合法 UTF-8
    ProtocolInvalidUtf8,
    /// 正文不是合法 JSON
    ProtocolInvalidJson,
    /// 协议版本不是 1
    ProtocolVersionMismatch,
    /// hello 内容校验失败（UUID/设备名/版本/本机相同）
    InvalidHello,
    /// 握手（hello 交换）超时
    HandshakeTimeout,
    /// 连接被对端关闭（EOF/半帧）
    ConnectionClosed,
    /// 心跳连续 3 次无有效 pong
    HeartbeatTimeout,
    /// 后台网络任务异常
    NetworkTaskFailed,
    /// 其他（配置/系统调用等，透传文案）
    Other(String),
}

impl AppError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// 展示给前端的固定文案：稳定、可理解，不含内部错误链、秘密或完整原始消息。
    pub fn user_text(&self) -> String {
        match self {
            Self::InvalidPeerIp => "不是有效的 IPv4 地址".into(),
            Self::SelfConnection => "不能连接本机自己的 ZeroTier IP".into(),
            Self::AlreadyConnected => "已有活动连接，请先断开".into(),
            Self::ConnectTimeout => "无法连接，请确认对方程序已经启动。".into(),
            Self::ConnectFailed => "对方没有运行 ClipLink，或被防火墙阻止。".into(),
            Self::BindFailed => "本机监听启动失败，请确认 45888 端口未被占用。".into(),
            Self::ProtocolMessageTooLarge => "对方发送的消息超过 1 MiB 上限，连接已关闭。".into(),
            Self::ProtocolInvalidLength => "协议错误：消息长度非法，连接已关闭。".into(),
            Self::ProtocolInvalidUtf8 => "协议错误：消息不是合法 UTF-8，连接已关闭。".into(),
            Self::ProtocolInvalidJson => "协议错误：消息无法解析，连接已关闭。".into(),
            Self::ProtocolVersionMismatch => "双方版本不兼容，请更新 ClipLink。".into(),
            Self::InvalidHello => "与对方的设备信息校验不一致，请重试。".into(),
            Self::HandshakeTimeout => "无法连接，请确认对方程序已经启动。".into(),
            Self::ConnectionClosed => "连接已中断。".into(),
            Self::HeartbeatTimeout => "连接已中断。".into(),
            Self::NetworkTaskFailed => "连接任务异常退出，请重试。".into(),
            Self::Other(s) => s.clone(),
        }
    }

    /// 稳定错误码：用于日志与前端事件（error_code），不含内部细节。
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPeerIp => "invalid_peer_ip",
            Self::SelfConnection => "self_connection",
            Self::AlreadyConnected => "already_connected",
            Self::ConnectTimeout => "connect_timeout",
            Self::ConnectFailed => "connect_failed",
            Self::BindFailed => "bind_failed",
            Self::ProtocolMessageTooLarge => "protocol_message_too_large",
            Self::ProtocolInvalidLength => "protocol_invalid_length",
            Self::ProtocolInvalidUtf8 => "protocol_invalid_utf8",
            Self::ProtocolInvalidJson => "protocol_invalid_json",
            Self::ProtocolVersionMismatch => "protocol_version_mismatch",
            Self::InvalidHello => "invalid_hello",
            Self::HandshakeTimeout => "handshake_timeout",
            Self::ConnectionClosed => "connection_closed",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::NetworkTaskFailed => "network_task_failed",
            Self::Other(_) => "other",
        }
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.user_text())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_text())
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

impl From<&str> for AppError {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 序列化结果是稳定用户文案而非变体名；错误码稳定
    #[test]
    fn error_serializes_to_stable_text() {
        let json = serde_json::to_string(&AppError::ConnectTimeout).unwrap();
        assert_eq!(json, "\"无法连接，请确认对方程序已经启动。\"");
        assert_eq!(AppError::HeartbeatTimeout.code(), "heartbeat_timeout");
        assert_eq!(
            AppError::AlreadyConnected.user_text(),
            "已有活动连接，请先断开"
        );
        let other = AppError::new("保存配置文件失败: x");
        assert_eq!(other.code(), "other");
        assert_eq!(other.to_string(), "保存配置文件失败: x");
    }
}
