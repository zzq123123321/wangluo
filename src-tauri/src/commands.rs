use crate::config;
use crate::error::AppError;
use crate::network;
use crate::state::{AppState, ConnectionStatus, Inner};
use crate::zerotier;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::{AppHandle, State};

/// 前端快照：只含非敏感字段。device_secret 等认证材料
/// 不在此结构中，因此不会进入前端、事件或序列化结果。
#[derive(Debug, Clone, Serialize)]
pub struct AppSnapshot {
    pub device_id: String,
    pub device_name: String,
    pub app_version: String,
    pub listen_port: u16,
    pub autostart: bool,
    /// 阶段 8：断线后自动重连是否开启
    pub auto_reconnect: bool,
    pub last_peer_ip: Option<String>,
    pub zerotier_ip: Option<String>,
    pub zerotier_hint: String,
    pub hint_warn: bool,
    pub status: ConnectionStatus,
    pub status_text: String,
    /// 对方摘要（设备名 + IP）；连接成功后的运行期对方信息，非敏感
    pub peer: Option<crate::state::PeerInfo>,
    pub paused: bool,
    pub last_sync: Option<String>,
}

/// 由配置 + 运行态组装快照（纯函数，便于单元测试敏感字段隔离）。
fn snapshot_from(cfg: &config::AppConfig, g: &Inner) -> AppSnapshot {
    AppSnapshot {
        device_id: cfg.identity.device_id.clone(),
        device_name: cfg.identity.device_name.clone(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        listen_port: cfg.listen_port,
        autostart: cfg.autostart,
        auto_reconnect: cfg.auto_reconnect,
        last_peer_ip: g.last_peer_ip.clone(),
        zerotier_ip: g.zerotier_ip.clone(),
        zerotier_hint: g.zerotier_hint.clone(),
        hint_warn: g.hint_warn,
        status: g.status,
        status_text: g.status_text.clone(),
        peer: g.peer.clone(),
        paused: g.paused,
        last_sync: g.last_sync.clone(),
    }
}

#[tauri::command]
pub fn get_app_snapshot(state: State<'_, Arc<AppState>>) -> Result<AppSnapshot, AppError> {
    let cfg = state
        .config
        .lock()
        .map_err(|e| AppError::new(e.to_string()))?;
    let g = state
        .inner
        .lock()
        .map_err(|e| AppError::new(e.to_string()))?;
    Ok(snapshot_from(&cfg, &g))
}

/// 设置更新输入：未传字段（None）表示不修改；last_peer_ip 空字符串表示清除已保存的对方 IP。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdate {
    /// 开机启动偏好值（阶段 4 仅保存，系统开机启动插件在阶段 11 接入）
    pub autostart: Option<bool>,
    /// 暂停同步
    pub sync_paused: Option<bool>,
    /// 断线后自动重连开关（阶段 8 配置项，此刻接入设置入口）
    pub auto_reconnect: Option<bool>,
    /// 对方 IP（有效 IPv4）；空字符串 = 清除；None = 不修改
    pub last_peer_ip: Option<String>,
}

/// 先持久化、成功后再更新内存状态：保存失败时内存不变，保证内存与磁盘一致。
/// 保存经 ConfigStore 专用锁串行化，磁盘 IO 在 config/inner 锁外，持锁期间无 await。
fn apply_settings(state: &AppState, settings: &SettingsUpdate) -> Result<AppSnapshot, AppError> {
    let new_cfg = {
        let g = state
            .config
            .lock()
            .map_err(|e| AppError::new(e.to_string()))?;
        let mut c = g.clone();
        if let Some(v) = settings.autostart {
            c.autostart = v;
        }
        if let Some(v) = settings.sync_paused {
            c.sync_paused = v;
        }
        if let Some(v) = settings.auto_reconnect {
            c.auto_reconnect = v;
        }
        if let Some(ip) = &settings.last_peer_ip {
            if !ip.is_empty() && !config::is_valid_ipv4_str(ip) {
                return Err(AppError::new("last_peer_ip 不是有效的 IPv4 地址"));
            }
            c.last_peer_ip = if ip.is_empty() {
                None
            } else {
                Some(ip.clone())
            };
        }
        c
    };
    state.store.save(&new_cfg)?;
    {
        *state
            .config
            .lock()
            .map_err(|e| AppError::new(e.to_string()))? = new_cfg.clone();
        let mut g = state
            .inner
            .lock()
            .map_err(|e| AppError::new(e.to_string()))?;
        if let Some(v) = settings.sync_paused {
            g.paused = v;
            // 暂停/恢复仅在 Connected↔Paused 之间切换状态与文案；
            // 其他状态（离线/重连中/错误）下暂停标记生效但不改写既有状态文案。
            if v {
                if g.status == ConnectionStatus::Connected {
                    g.status = ConnectionStatus::Paused;
                    g.status_text = crate::zerotier::STATUS_PAUSED.to_string();
                }
            } else if g.status == ConnectionStatus::Paused {
                g.status = ConnectionStatus::Connected;
                g.status_text = crate::zerotier::STATUS_CONNECTED.to_string();
            }
        }
        if let Some(ip) = &settings.last_peer_ip {
            g.last_peer_ip = if ip.is_empty() {
                None
            } else {
                Some(ip.clone())
            };
        }
    }
    // auto_reconnect 需要同步到网络管理器的重连开关（运行在 manager 内部，非 AppState 状态）
    if let Some(v) = settings.auto_reconnect {
        state.net.set_reconnect(v);
    }
    let cfg = state
        .config
        .lock()
        .map_err(|e| AppError::new(e.to_string()))?;
    let g = state
        .inner
        .lock()
        .map_err(|e| AppError::new(e.to_string()))?;
    Ok(snapshot_from(&cfg, &g))
}

/// 用户设置入口（前端命令与系统托盘共用）：
/// 若本次修改了 autostart，先落系统开机启动注册项，成功后才走 apply_settings 持久化配置；
/// 保证注册表失败时配置不会宣称“已开启开机启动”，配置与注册表始终一致。
pub(crate) fn apply_user_settings(
    state: &AppState,
    settings: &SettingsUpdate,
) -> Result<AppSnapshot, AppError> {
    if let Some(v) = settings.autostart {
        crate::autostart::apply(v).map_err(AppError::new)?;
    }
    apply_settings(state, settings)
}

#[tauri::command]
pub fn update_settings(
    state: State<'_, Arc<AppState>>,
    settings: SettingsUpdate,
) -> Result<AppSnapshot, AppError> {
    apply_user_settings(&state, &settings)
}

/// 本机身份摘要：只返回 device_id 与 device_name，不含 device_secret。
#[derive(Debug, Serialize)]
pub struct IdentitySummary {
    pub device_id: String,
    pub device_name: String,
}

fn identity_summary_from(cfg: &config::AppConfig) -> IdentitySummary {
    IdentitySummary {
        device_id: cfg.identity.device_id.clone(),
        device_name: cfg.identity.device_name.clone(),
    }
}

#[tauri::command]
pub fn get_device_identity_summary(
    state: State<'_, Arc<AppState>>,
) -> Result<IdentitySummary, AppError> {
    let cfg = state
        .config
        .lock()
        .map_err(|e| AppError::new(e.to_string()))?;
    Ok(identity_summary_from(&cfg))
}

/// 手动刷新：立即执行一次检测（与后台轮询共用 detect_and_notify），
/// 返回结构化结果；检测不到地址是正常业务状态，只有系统 API 失败才返回 AppError。
#[tauri::command]
pub async fn refresh_zerotier_ip(app: AppHandle) -> Result<zerotier::ZtResult, AppError> {
    zerotier::detect_and_notify(&app)
}

/// 主动连接对方。校验输入 → 保存 last_peer_ip（连接前保存）→ 交给 NetworkManager。
/// last_peer_ip 采用“连接前保存”：保存失败仅记录日志并继续连接，内存与磁盘均保持原值，
/// 不产生无法解释的状态差异。失败后不自动重连，用户/托盘可再次触发。
pub(crate) async fn do_connect(state: &AppState, raw_ip: &str) -> Result<(), AppError> {
    let ip = raw_ip.trim().to_string();
    let local_zt = state
        .inner
        .lock()
        .map_err(|e| AppError::new(e.to_string()))?
        .zerotier_ip
        .clone();
    // 校验：合法 IPv4 + 排除特殊段 + 禁止本机 ZeroTier IP
    let _target = network::validate_peer_target(&ip, local_zt.as_deref())?;
    let port = state
        .config
        .lock()
        .map_err(|e| AppError::new(e.to_string()))?
        .listen_port;
    // 连接前保存 last_peer_ip（与 update_settings 相同的“先落盘成功再更新内存”规则；
    // 注意：Mutex 不可重入，clone 后必须先释放锁再落盘，最后在新作用域 swap）
    let need_save = {
        let g = state
            .config
            .lock()
            .map_err(|e| AppError::new(e.to_string()))?;
        g.last_peer_ip.as_deref() != Some(ip.as_str())
    };
    if need_save {
        let c = {
            let g = state
                .config
                .lock()
                .map_err(|e| AppError::new(e.to_string()))?;
            let mut c = g.clone();
            c.last_peer_ip = Some(ip.clone());
            c
        };
        if let Err(e) = state.store.save(&c) {
            tracing::warn!(%e, "保存 last_peer_ip 失败，继续连接（内存与磁盘保持原值）");
        } else {
            *state
                .config
                .lock()
                .map_err(|e| AppError::new(e.to_string()))? = c;
            if let Ok(mut inner) = state.inner.lock() {
                inner.last_peer_ip = Some(ip.clone());
            }
        }
    }
    state.net.connect(&ip, port).await
}

#[tauri::command]
pub async fn connect_peer(state: State<'_, Arc<AppState>>, ip: String) -> Result<(), AppError> {
    do_connect(&state, &ip).await
}

/// 用户主动断开：发送 disconnect、停止心跳/读写任务、回到 Offline。
/// 不删除配置中的 last_peer_ip（连接目标由用户下次输入决定）。
async fn do_disconnect(state: &AppState) -> Result<(), AppError> {
    state.net.disconnect().await;
    Ok(())
}

#[tauri::command]
pub async fn disconnect_peer(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    do_disconnect(&state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigStore;
    use std::sync::Arc;

    struct TestSink;

    impl network::StatusSink for TestSink {
        fn on_status(&self, _ev: &network::ConnectionStatusEvent) {}
    }

    fn test_state() -> AppState {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("cliplink-test-cmd-{}-{ts}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ConfigStore::new(dir));
        let cfg = store.load().unwrap();
        let net = Arc::new(network::NetworkManager::new(
            cfg.identity.device_id.clone(),
            cfg.identity.device_name.clone(),
            network::ManagerConfig::production(45888),
            Arc::new(TestSink),
        ));
        AppState::new(store, cfg, net)
    }

    // 敏感字段（device_secret）不出现在前端快照序列化结果中；
    // 非敏感身份与设置正常出现。仅用测试专用值，不输出真实值。
    #[test]
    fn snapshot_contains_no_secrets() {
        let state = test_state();
        let test_device_secret = "ab".repeat(32);
        let cfg = {
            let mut g = state.config.lock().unwrap();
            g.identity.device_secret = test_device_secret.clone();
            g.clone()
        };
        let inner = Inner {
            peer: Some(crate::state::PeerInfo {
                device_name: "SNAP-TEST".into(),
                ip: "10.147.17.36".into(),
            }),
            ..Inner::default()
        };
        let snap = snapshot_from(&cfg, &inner);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains(&test_device_secret));
        assert!(json.contains("SNAP-TEST"));
        assert!(json.contains(&cfg.identity.device_id));
        assert!(json.contains("10.147.17.36"));
    }

    // update_settings：非法输入拒绝且内存不变；有效写入先落盘再更新内存；空字符串清除
    #[test]
    fn update_settings_persists_and_validates() {
        let state = test_state();

        let bad = SettingsUpdate {
            autostart: None,
            sync_paused: None,
            auto_reconnect: None,
            last_peer_ip: Some("999.1.1.1".into()),
        };
        assert!(apply_settings(&state, &bad).is_err());
        assert!(state.inner.lock().unwrap().last_peer_ip.is_none());

        let ok = SettingsUpdate {
            autostart: Some(true),
            sync_paused: Some(true),
            auto_reconnect: None,
            last_peer_ip: Some("10.147.17.36".into()),
        };
        let snap = apply_settings(&state, &ok).unwrap();
        assert!(snap.autostart);
        assert!(snap.paused);
        assert_eq!(snap.last_peer_ip.as_deref(), Some("10.147.17.36"));
        let path = state.store.config_path();
        let disk: config::AppConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(disk.autostart);
        assert!(disk.sync_paused);
        assert_eq!(disk.last_peer_ip.as_deref(), Some("10.147.17.36"));
        assert_eq!(
            state.config.lock().unwrap().last_peer_ip.as_deref(),
            Some("10.147.17.36")
        );

        let clear = SettingsUpdate {
            autostart: None,
            sync_paused: None,
            auto_reconnect: None,
            last_peer_ip: Some(String::new()),
        };
        let snap = apply_settings(&state, &clear).unwrap();
        assert!(snap.last_peer_ip.is_none());
        assert!(state.store.config_path().exists());
    }

    // 身份摘要只含 device_id / device_name，序列化结果不含 secret
    #[test]
    fn identity_summary_has_no_secret() {
        let state = test_state();
        let secret = state.config.lock().unwrap().identity.device_secret.clone();
        let sum = identity_summary_from(&state.config.lock().unwrap());
        let json = serde_json::to_string(&sum).unwrap();
        assert!(!json.contains(&secret));
        assert_eq!(
            sum.device_id,
            state.config.lock().unwrap().identity.device_id
        );
    }

    // 36/37/38/39：connect_peer 输入校验（命令层）
    #[test]
    fn connect_peer_validates_input() {
        let state = test_state();
        {
            let mut g = state.inner.lock().unwrap();
            g.zerotier_ip = Some("192.168.191.180".into());
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(matches!(
                do_connect(&state, "abc").await,
                Err(AppError::InvalidPeerIp)
            ));
            assert!(matches!(
                do_connect(&state, "999.1.1.1").await,
                Err(AppError::InvalidPeerIp)
            ));
            assert!(matches!(
                do_connect(&state, "127.0.0.1").await,
                Err(AppError::InvalidPeerIp)
            ));
            assert!(matches!(
                do_connect(&state, "224.0.0.5").await,
                Err(AppError::InvalidPeerIp)
            ));
            assert!(matches!(
                do_connect(&state, "192.168.191.180").await,
                Err(AppError::SelfConnection)
            ));
            // 合法 IP 提交连接（后台任务在运行时内完成；对 TEST-NET 不可达地址会超时/失败，
            // 这里只断言命令层不报错，失败经状态事件呈现）
            assert!(do_connect(&state, "192.0.2.1").await.is_ok());
            // 已有活动/建立中连接时，新请求被拒绝
            assert!(matches!(
                do_connect(&state, "192.0.2.2").await,
                Err(AppError::AlreadyConnected)
            ));
            do_disconnect(&state).await.unwrap();
        });
    }

    // 26：用户主动断开回到 Offline（无活动连接时为空操作，不报错）
    #[test]
    fn disconnect_idle_is_noop() {
        let state = test_state();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            do_disconnect(&state).await.unwrap();
            assert_eq!(
                state.inner.lock().unwrap().status,
                ConnectionStatus::Offline
            );
        });
    }

    // 阶段 9：暂停/恢复仅在 Connected↔Paused 之间切换状态与文案
    #[test]
    fn pause_transitions_connected_state_and_text() {
        let state = test_state();
        {
            let mut g = state.inner.lock().unwrap();
            g.status = ConnectionStatus::Connected;
            g.status_text = crate::zerotier::STATUS_CONNECTED.into();
        }
        let pause = SettingsUpdate {
            autostart: None,
            sync_paused: Some(true),
            auto_reconnect: None,
            last_peer_ip: None,
        };
        let snap = apply_settings(&state, &pause).unwrap();
        assert!(snap.paused);
        assert_eq!(snap.status, ConnectionStatus::Paused);
        assert_eq!(snap.status_text, crate::zerotier::STATUS_PAUSED);

        let resume = SettingsUpdate {
            autostart: None,
            sync_paused: Some(false),
            auto_reconnect: None,
            last_peer_ip: None,
        };
        let snap = apply_settings(&state, &resume).unwrap();
        assert!(!snap.paused);
        assert_eq!(snap.status, ConnectionStatus::Connected);
        assert_eq!(snap.status_text, crate::zerotier::STATUS_CONNECTED);
    }

    // 阶段 8：暂停时若本处于离线/错误状态，不改写既有状态文案
    #[test]
    fn pause_keeps_non_connected_status_text() {
        let state = test_state();
        {
            let mut g = state.inner.lock().unwrap();
            g.status = ConnectionStatus::Error;
            g.status_text = "分散服务异常".into();
        }
        let pause = SettingsUpdate {
            autostart: None,
            sync_paused: Some(true),
            auto_reconnect: None,
            last_peer_ip: None,
        };
        let snap = apply_settings(&state, &pause).unwrap();
        assert!(snap.paused);
        assert_eq!(snap.status, ConnectionStatus::Error);
        assert_eq!(snap.status_text, "分散服务异常");
    }

    // 阶段 8：auto_reconnect 持久化到配置并落入快照（网络管理器开关无法从外部读取，
    // 通过快照间接验证命令层已应用）
    #[test]
    fn auto_reconnect_persists_to_config() {
        let state = test_state();
        assert!(state.config.lock().unwrap().auto_reconnect);
        let off = SettingsUpdate {
            autostart: None,
            sync_paused: None,
            auto_reconnect: Some(false),
            last_peer_ip: None,
        };
        let snap = apply_settings(&state, &off).unwrap();
        assert!(!snap.auto_reconnect);
        assert!(!state.config.lock().unwrap().auto_reconnect);
        let disk: config::AppConfig =
            serde_json::from_str(&std::fs::read_to_string(state.store.config_path()).unwrap())
                .unwrap();
        assert!(!disk.auto_reconnect);
    }
}
