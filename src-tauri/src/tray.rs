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
        MENU_QUIT => app.exit(0),
        _ => {}
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

fn reconnect(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    tauri::async_runtime::spawn(async move {
        let ip = state.inner.lock().ok().and_then(|g| g.last_peer_ip.clone());
        let Some(ip) = ip else {
            tracing::warn!("托盘重新连接：没有已保存的对方 IP");
            return;
        };
        if let Err(e) = commands::do_connect(&state, &ip).await {
            tracing::warn!(%e, "托盘重新连接失败");
        }
    });
}
