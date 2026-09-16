use crate::config::{AppConfig, ConfigStore};
use crate::network::NetworkManager;
use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    #[default]
    Offline,
    Connecting,
    AwaitingPairing,
    Connected,
    Reconnecting,
    Paused,
    Error,
}

/// 运行期对方信息（阶段 5 连接成功后更新）；也用作 paired_peer 的非敏感摘要（不含 shared_key）。
#[derive(Debug, Clone, Serialize)]
pub struct PeerInfo {
    pub device_name: String,
    pub ip: String,
}

#[derive(Debug, Default)]
pub struct Inner {
    pub zerotier_ip: Option<String>,
    pub zerotier_hint: String,
    pub hint_warn: bool,
    pub status: ConnectionStatus,
    pub status_text: String,
    pub peer: Option<PeerInfo>,
    pub paused: bool,
    pub last_sync: Option<String>,
    pub pairing_code: Option<String>,
    /// 已保存的对方 IP（来自配置，供前端输入框回显）
    pub last_peer_ip: Option<String>,
}

/// 完整配置（含 device_secret 等敏感字段）只在 Rust 侧持有；
/// 前端快照（commands::AppSnapshot）只暴露非敏感字段。
/// 锁内只允许短读写，配置文件 IO 与网络 IO 一律在锁外（网络经 NetworkManager 专用锁/任务）。
/// TcpStream 不在此结构中暴露：连接生命周期全部收敛在 NetworkManager 的任务里。
pub struct AppState {
    pub store: Arc<ConfigStore>,
    pub config: Mutex<AppConfig>,
    pub inner: Mutex<Inner>,
    /// 网络管理器（listener + 单一活动连接 + 心跳）。以 Arc 持有，可跨任务共享。
    pub net: Arc<NetworkManager>,
}

impl AppState {
    /// 启动时初始化：配置必须已加载（见 lib.rs），且先于依赖配置的后台任务。
    pub fn new(store: Arc<ConfigStore>, config: AppConfig, net: Arc<NetworkManager>) -> Self {
        let inner = Inner {
            zerotier_hint: crate::zerotier::HINT_DETECTING.to_string(),
            status_text: crate::zerotier::HINT_DETECTING.to_string(),
            paused: config.sync_paused,
            last_peer_ip: config.last_peer_ip.clone(),
            peer: config.paired_peer.as_ref().map(|p| PeerInfo {
                device_name: p.device_name.clone(),
                ip: p.ip.clone(),
            }),
            ..Inner::default()
        };
        Self {
            store,
            config: Mutex::new(config),
            inner: Mutex::new(inner),
            net,
        }
    }
}
