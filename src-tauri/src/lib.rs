mod autostart;
mod clipboard;
mod commands;
pub mod config;
pub mod error;
pub mod identity;
pub mod network;
pub mod protocol;
pub mod state;
mod tray;
pub mod zerotier;

use state::AppState;
use std::sync::Arc;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cliplink_lib=debug".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // 阶段 9：关闭窗口只隐藏到托盘，进程与同步继续；退出只能走托盘“退出 ClipLink”。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .setup(|app| {
            // 阶段 4：配置与本机身份必须先于依赖配置的后台任务就绪。
            // 初始化失败时 setup 返回 Err，进程带清晰错误退出，不带着未初始化身份继续运行。
            let data_dir = app.path().app_data_dir().map_err(|e| {
                Box::<dyn std::error::Error>::from(format!("无法获取应用数据目录: {e}"))
            })?;
            std::fs::create_dir_all(&data_dir).map_err(|e| {
                Box::<dyn std::error::Error>::from(format!("无法创建应用数据目录: {e}"))
            })?;
            let store = Arc::new(config::ConfigStore::new(data_dir.clone()));
            let cfg = store
                .load()
                .map_err(|e| Box::<dyn std::error::Error>::from(format!("配置初始化失败: {e}")))?;
            // 阶段 9：cfg 随后移入 AppState，先取出启动时需要的开机启动偏好
            let startup_autostart = cfg.autostart;
            // 日志只记录非敏感信息：目录、device_id、device_name；device_secret/shared_key 不得出现
            tracing::info!(
                dir = %data_dir.display(),
                device_id = %cfg.identity.device_id,
                device_name = %cfg.identity.device_name,
                "配置已加载，本机身份就绪"
            );
            // 阶段 5：网络管理器（listener + 单一活动连接 + 心跳）。
            // 连接状态经 TauriStatusSink 单一出口写入 AppState.inner 并 emit 事件；
            // zt_provider 在 AppState 创建后注入，打破 manager 与 AppState 的循环依赖。
            let net = Arc::new(network::NetworkManager::new(
                cfg.identity.device_id.clone(),
                cfg.identity.device_name.clone(),
                network::ManagerConfig::production(cfg.listen_port),
                Arc::new(network::TauriStatusSink::new(app.handle().clone())),
            ));
            let state = Arc::new(AppState::new(store, cfg, net.clone()));
            let state_ref = state.clone();
            net.set_zt_provider(Arc::new(move || {
                state_ref.inner.lock().ok()?.zerotier_ip.clone()
            }));
            // 阶段 8：自动重连开关 + 重连目标（保存的对方 IP）从配置/AppState 注入。
            let reconnect_enabled = {
                let c = state
                    .config
                    .lock()
                    .map_err(|_| Box::<dyn std::error::Error>::from("状态锁被污染"))?;
                c.auto_reconnect
            };
            net.set_reconnect(reconnect_enabled);
            let state_for_reconnect = state.clone();
            net.set_peer_ip_provider(Arc::new(move || {
                state_for_reconnect.inner.lock().ok()?.last_peer_ip.clone()
            }));
            // 阶段 7：远程剪贴板落地回调（写系统剪贴板 + 更新状态 + 前端同步事件）
            let state_for_landing = state.clone();
            let app_for_landing = app.handle().clone();
            net.set_clipboard_landing(Arc::new(move |payload| {
                clipboard::land_remote(state_for_landing.clone(), app_for_landing.clone(), payload);
            }));
            let state_for_clipboard = state.clone();
            app.manage(state);

            // 阶段 9：系统托盘（依赖已管理的 AppState 读取初始状态）。
            // TrayHandle 由 app.manage 持有到进程退出，防止托盘图标被 Drop 移除。
            let tray = tray::setup(app.handle())?;
            app.manage(tray);

            // 阶段 9：开机启动的静默到托盘（注册表启动项携带 --minimized）。
            // 顺带做一次注册表与配置的一致性修复：配置开启但注册项缺失时（如被手动删除）补写。
            if std::env::args().any(|a| a == "--minimized") {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
            if startup_autostart {
                if let Err(e) = autostart::apply(true) {
                    tracing::warn!(%e, "开机启动注册项修复失败");
                }
            }

            // 阶段 6：剪贴板监听（每 300ms 读一次纯文字，检测变化）
            clipboard::start(state_for_clipboard, app.handle().clone());

            // 阶段 3：启动后立即检测一次，之后每 10 秒复查。
            // 检测结果变化时由 detect_and_notify 内部触发 listener 启停/重绑定（阶段 5 复用同一路径，无新增轮询器）。
            // 任务跑在 tauri 全局 async runtime（独立于主线程）；进程退出时随 runtime 销毁，
            // listener 任务结束、45888 端口随之释放。
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(
                    zerotier::POLL_INTERVAL_SECS,
                ));
                loop {
                    tick.tick().await;
                    match zerotier::detect_and_notify(&handle) {
                        Ok(r) => tracing::debug!(?r, "ZeroTier 复查完成"),
                        Err(e) => tracing::warn!(%e, "ZeroTier 检测失败，下轮重试"),
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_app_snapshot,
            commands::refresh_zerotier_ip,
            commands::connect_peer,
            commands::disconnect_peer,
            commands::update_settings,
            commands::get_device_identity_summary
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
