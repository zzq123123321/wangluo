// 阶段 9：系统托盘与菜单。
// 关闭窗口时只隐藏不退出（见 lib.rs on_window_event）；托盘菜单提供：
// 当前状态（实时）、打开主界面、暂停/恢复同步、重新连接、开机启动与退出 ClipLink。
// 只有“退出 ClipLink”结束进程；左键单击/双击托盘图标打开主界面。
// 连接状态变化（TauriStatusSink）与托盘内设置切换都会调用 sync_from_state 刷新菜单项。
use crate::commands;
use crate::state::AppState;
use crate::MainShown;
use std::sync::Arc;
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

/// 托盘修改设置后广播给前端的事件（payload = commands::AppSnapshot）
pub(crate) const SETTINGS_CHANGED_EVENT: &str = "settings-changed";

const MENU_STATE: &str = "tray-state";
const MENU_OPEN: &str = "tray-open";
const MENU_PAUSE: &str = "tray-pause";
const MENU_RECONNECT: &str = "tray-reconnect";
const MENU_AUTOSTART: &str = "tray-autostart";
const MENU_QUIT: &str = "tray-quit";

/// 托盘资源持有：管理到应用退出，防止 Drop 移除托盘图标；menu 供运行期更新菜单项。
pub(crate) struct TrayHandle {
    _tray: TrayIcon,
    menu: tauri::menu::Menu<Wry>,
}

pub(crate) fn setup(app: &AppHandle) -> tauri::Result<TrayHandle> {
    let state = app.state::<Arc<AppState>>();
    let (status_text, paused, autostart) = read_state(&state);

    let status_item = MenuItemBuilder::with_id(MENU_STATE, status_text)
        .enabled(false)
        .build(app)?;
    let open_item = MenuItemBuilder::with_id(MENU_OPEN, "打开主界面").build(app)?;
    let pause_item = CheckMenuItemBuilder::with_id(
        MENU_PAUSE,
        if paused {
            "恢复同步"
        } else {
            "暂停同步"
        },
    )
    .checked(paused)
    .build(app)?;
    let reconnect_item = MenuItemBuilder::with_id(MENU_RECONNECT, "重新连接").build(app)?;
    let autostart_item = CheckMenuItemBuilder::with_id(MENU_AUTOSTART, "开机启动")
        .checked(autostart)
        .build(app)?;
    let quit_item = MenuItemBuilder::with_id(MENU_QUIT, "退出 ClipLink").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[
            &status_item,
            &open_item,
            &pause_item,
            &reconnect_item,
            &autostart_item,
            &quit_item,
        ])
        .build()?;

    let tray = TrayIconBuilder::with_id("main")
        .tooltip("ClipLink")
        .menu(&menu)
        // 左键单击打开窗口而非弹出菜单（菜单经右键弹出）
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| handle_menu(app, event))
        .on_tray_icon_event(|tray, event| handle_tray_icon(tray.app_handle(), event))
        .build(app)?;

    Ok(TrayHandle { _tray: tray, menu })
}

/// 从 AppState 读取菜单需要展示的运行态：状态文案、是否暂停、开机启动偏好。
fn read_state(state: &AppState) -> (String, bool, bool) {
    let (text, paused) = state
        .inner
        .lock()
        .map(|g| (g.status_text.clone(), g.paused))
        .unwrap_or_default();
    let autostart = state.config.lock().map(|c| c.autostart).unwrap_or(false);
    (text, paused, autostart)
}

/// 用 AppState 刷新托盘菜单项（状态文案 / 暂停文案与勾选 / 开机启动勾选）。
/// 在 TauriStatusSink 每次状态变化与托盘设置切换后调用。
pub(crate) fn sync_from_state(app: &AppHandle) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let Some(handle) = app.try_state::<TrayHandle>() else {
        return;
    };
    let (status_text, paused, autostart) = read_state(&state);
    if let Some(item) = handle.menu.get(MENU_STATE) {
        if let Some(mi) = item.as_menuitem() {
            let _ = mi.set_text(status_text);
        }
    }
    if let Some(item) = handle.menu.get(MENU_PAUSE) {
        if let Some(ci) = item.as_check_menuitem() {
            let _ = ci.set_checked(paused);
            let _ = ci.set_text(if paused {
                "恢复同步"
            } else {
                "暂停同步"
            });
        }
    }
    if let Some(item) = handle.menu.get(MENU_AUTOSTART) {
        if let Some(ci) = item.as_check_menuitem() {
            let _ = ci.set_checked(autostart);
        }
    }
}

fn handle_menu(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        MENU_OPEN => show_main(app),
        MENU_PAUSE => toggle_sync_paused(app),
        MENU_RECONNECT => reconnect(app),
        MENU_AUTOSTART => toggle_autostart(app),
        MENU_QUIT => quit(app),
        _ => {}
    }
}

/// 真正退出：先停止网络生命周期（listener / 活动连接 / 自动重连），再退出进程。
/// T11-04 的退出门槛（exiting）只会在 shutdown() 中置位——这里必须真实调用，
/// 否则用户点击“退出 ClipLink”时后台连接/重连任务没有任何清理机会。
/// 2 秒超时保护：shutdown 内部任务取消不阻塞 UI，锁异常/超时都不阻碍最终 exit。
/// 仅此一处发起 exit，不重复调用。
fn quit(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let net = state.net.clone();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), net.shutdown()).await;
            app.exit(0);
        });
    } else {
        app.exit(0);
    }
}

fn handle_tray_icon(app: &AppHandle, event: TrayIconEvent) {
    match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => show_main(app),
        _ => {}
    }
}

/// 恢复主界面（托盘“打开主界面”与呼吸灯单击共用入口）：
/// 先还原最小化态，再显示并聚焦；同时隐藏状态呼吸灯。
pub(crate) fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        // T11-03B-FIX1：恢复主窗口即视为“已正式展示”，此后用户最小化
        // 才触发呼吸灯接管（--minimized 启动也由这条路解锁）。
        if let Some(gate) = app.try_state::<Arc<MainShown>>() {
            gate.mark();
        }
    }
    if let Some(ind) = app.get_webview_window("indicator") {
        let _ = ind.hide();
    }
}

fn toggle_sync_paused(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let target = state.inner.lock().map(|g| !g.paused).unwrap_or(false);
    let settings = commands::SettingsUpdate {
        autostart: None,
        sync_paused: Some(target),
        last_peer_ip: None,
        auto_reconnect: None,
    };
    apply_and_notify(app, &state, settings);
}

fn toggle_autostart(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let target = state.config.lock().map(|c| !c.autostart).unwrap_or(false);
    let settings = commands::SettingsUpdate {
        autostart: Some(target),
        sync_paused: None,
        last_peer_ip: None,
        auto_reconnect: None,
    };
    apply_and_notify(app, &state, settings);
}

/// 走 update_settings 同一条“先注册表后配置”路径；成功后刷新托盘并通知前端。
fn apply_and_notify(app: &AppHandle, state: &AppState, settings: commands::SettingsUpdate) {
    match commands::apply_user_settings(state, &settings) {
        Ok(snap) => {
            sync_from_state(app);
            let _ = app.emit(SETTINGS_CHANGED_EVENT, snap);
        }
        Err(e) => tracing::warn!(%e, "托盘设置切换失败"),
    }
}

/// 托盘“重新连接”：按已保存的对方 IP 重连。若当前已有活动/建立中连接，
/// 先 disconnect（取消自动重连并释放槽位）再 connect——满足“重新连接”语义，
/// 避免在 Connected 状态下直接 connect 命中 AlreadyConnected 报错或制造并发连接。
/// 断线与重连在同一任务内顺序 await，无自动重连 race（disconnect 为 User 关闭，
/// 不会触发 start_reconnect；随后 do_connect 重新占用单一槽位）。
pub(crate) async fn reconnect_peer(state: &AppState) {
    let ip = state.inner.lock().ok().and_then(|g| g.last_peer_ip.clone());
    let Some(ip) = ip else {
        tracing::warn!("托盘重新连接：没有已保存的对方 IP");
        return;
    };
    if state.net.is_busy() {
        state.net.disconnect().await;
    }
    if let Err(e) = commands::do_connect(state, &ip).await {
        tracing::warn!(%e, "托盘重新连接失败");
    }
}

fn reconnect(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    tauri::async_runtime::spawn(async move {
        reconnect_peer(&state).await;
    });
}

#[cfg(test)]
mod tests {
    use super::reconnect_peer;
    use crate::config::ConfigStore;
    use crate::network::{self, ConnectionStatusEvent};
    use crate::state::{AppState, ConnectionStatus};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct VecSink(Mutex<Vec<ConnectionStatusEvent>>);

    impl VecSink {
        fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }
        fn all(&self) -> Vec<ConnectionStatusEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl network::StatusSink for VecSink {
        fn on_status(&self, ev: &ConnectionStatusEvent) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }

    fn test_state() -> (AppState, Arc<VecSink>) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("cliplink-test-tray-{}-{ts}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ConfigStore::new(dir));
        let cfg = store.load().unwrap();
        let sink = Arc::new(VecSink::new());
        let net = Arc::new(network::NetworkManager::new(
            cfg.identity.device_id.clone(),
            cfg.identity.device_name.clone(),
            network::ManagerConfig {
                port: 0,
                heartbeat_secs: 1,
                handshake_timeout: Duration::from_millis(300),
                connect_timeout: Duration::from_secs(5),
                // 关闭自动重连：让本测试只验证“重新连接”语义，
                // 排除后台自动重连对事件序列的干扰
                reconnect: false,
            },
            sink.clone(),
        ));
        let state = AppState::new(store, cfg, net);
        (state, sink)
    }

    // T11-07 测试 4：已有活动连接时点击“重新连接”——先断开旧连接（Offline），
    // 再建立新连接（Connecting），不制造双连接、不触发自动重连、不覆盖为错误状态。
    #[test]
    fn tray_reconnect_with_active_conn_restarts_cleanly() {
        let (state, sink) = test_state();
        {
            let mut g = state.inner.lock().unwrap();
            g.zerotier_ip = Some("192.168.191.180".into());
            g.last_peer_ip = Some("192.0.2.1".into());
        }
        // 先占满唯一槽位（模拟已有连接）
        let _c1 = state.net.claim_conn().unwrap();
        assert!(state.net.is_busy());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            reconnect_peer(&state).await;
        });

        // 修复前：已连接时直接 do_connect 会命中 AlreadyConnected，不产生任何状态事件
        // （旧连接保持 Connected，没有 Offline、没有 Connecting）。
        // 修复后：先断开旧连接（Offline）再重连（Connecting），且全程无自动重连事件。
        let statuses = sink
            .all()
            .into_iter()
            .map(|e| (e.status, e.generation))
            .collect::<Vec<_>>();
        let offline_gen = statuses
            .iter()
            .find(|(s, _)| *s == ConnectionStatus::Offline)
            .map(|(_, g)| *g);
        assert!(
            offline_gen.is_some(),
            "重新连接应先断开旧连接，实际事件：{statuses:?}"
        );
        assert!(
            statuses
                .iter()
                .any(|(s, _)| *s == ConnectionStatus::Connecting),
            "重新连接后应进入 Connecting，实际事件：{statuses:?}"
        );
        assert!(
            offline_gen.is_some_and(|g| statuses
                .iter()
                .any(|(s, gen)| *s == ConnectionStatus::Connecting && *gen > g)),
            "新连接代次应大于旧连接代次，实际事件：{statuses:?}"
        );
        assert!(
            !statuses
                .iter()
                .any(|(s, _)| *s == ConnectionStatus::Reconnecting),
            "用户主动重新连接不得触发自动重连事件：{statuses:?}"
        );
    }

    // 没有已保存的对方 IP 时，重新连接应安全返回（空操作、不误触发任何连接）。
    #[test]
    fn tray_reconnect_without_peer_ip_is_noop() {
        let (state, sink) = test_state();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            reconnect_peer(&state).await;
        });
        assert!(!state.net.is_busy());
        assert!(sink.all().is_empty(), "无目标 IP 时不得产生任何状态事件");
    }

    // T11-07 测试 4b：退出路径先调用 shutdown —— 之后不再安排/执行自动重连，
    // 且 listener/连接槽位被清空（真实托盘退出路径的共享内部行为）。
    #[test]
    fn tray_quit_path_shuts_network_down_first() {
        let (state, sink) = test_state();
        {
            let mut g = state.inner.lock().unwrap();
            g.last_peer_ip = Some("192.0.2.1".into());
        }
        let _c1 = state.net.claim_conn().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            state.net.shutdown().await;
        });
        assert!(!state.net.is_busy(), "退出后连接槽位应清空");
        assert!(
            state.net.listener_local_addr().is_none(),
            "退出后 listener 应释放"
        );
        // shutdown 后不得再发起任何连接（无 Connecting 事件）
        assert!(
            !sink
                .all()
                .iter()
                .any(|e| e.status == ConnectionStatus::Connecting),
            "退出后不得再发起连接"
        );
    }
}
