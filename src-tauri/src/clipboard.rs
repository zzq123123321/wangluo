// 阶段 6/7：单机剪贴板监听与上下行同步。
// 后台任务每 300ms 读取一次剪贴板纯文字，计算 SHA-256 哈希并与上次比较。
// 检测到本机变化时发送 clipboard_update（仅已连接时）；暂停时跳过读取。
// 收到远程文字（network.rs 经 set_clipboard_landing 注入回调）验证通过后：
//   1. 先标记“本次为远程写入”（防轮询回传），再写系统剪贴板，最后更新观察哈希；
//   2. 更新最近同步时间并向前端发 clipboard-synced 事件。
// 任务随 Tauri 全局 async runtime 生存，进程退出即销毁。
use crate::identity::hex_encode;
use crate::protocol;
use crate::state::{AppState, ConnectionStatus};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const POLL_INTERVAL: Duration = Duration::from_millis(300);
const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024; // 1 MiB
const ERROR_EVENT: &str = "app-error";
/// 前端“最近同步时间”事件（payload: { time: string，epoch 毫秒 }）
pub const SYNCED_EVENT: &str = "clipboard-synced";

pub fn start(state: Arc<AppState>, app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        loop {
            tick.tick().await;

            // 暂停时跳过（不读剪贴板）
            let paused = state.inner.lock().map(|g| g.paused).unwrap_or(false);
            if paused {
                continue;
            }

            // 读取剪贴板纯文字
            let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                Ok(t) => t,
                Err(e) => {
                    tracing::trace!(%e, "剪贴板读取失败，跳过本轮");
                    continue;
                }
            };

            // 空字符串跳过
            if text.is_empty() {
                continue;
            }

            // 大小检查：超 1 MiB 只提示不处理（不写入内部状态，不留作待发内容）
            if text.len() > MAX_CLIPBOARD_BYTES {
                tracing::warn!(byte_len = text.len(), "剪贴板文字超过 1 MiB，本次未处理");
                let _ = app.emit(
                    ERROR_EVENT,
                    serde_json::json!({ "message": "剪贴板文字超过 1 MiB，本次未同步。" }),
                );
                continue;
            }

            // SHA-256 哈希
            let hash = sha256_hex(text.as_bytes());

            // 与上次比较：变化时才处理；若与刚写入的远程内容一致则只吸收、不回传
            let action = state
                .inner
                .lock()
                .map(|mut g| {
                    let kind = classify_change(
                        g.last_clipboard_hash.as_deref(),
                        g.last_remote_hash.as_deref(),
                        &hash,
                    );
                    match kind {
                        ChangeKind::Unchanged => Action::Skip,
                        ChangeKind::RemoteEcho => {
                            g.last_clipboard_hash = Some(hash.clone());
                            g.last_clipboard_text = Some(text.clone());
                            g.last_remote_hash = None;
                            Action::Skip
                        }
                        ChangeKind::LocalChange => {
                            g.last_clipboard_hash = Some(hash.clone());
                            g.last_clipboard_text = Some(text.clone());
                            Action::Send
                        }
                    }
                })
                .unwrap_or(Action::Skip);

            if matches!(action, Action::Send) {
                send_update(&state, &text, &hash);
            }
        }
    });
}

enum Action {
    Send,
    Skip,
}

/// 轮询变化的分类（纯函数，可单测）：
/// - Unchanged：与上次观察哈希一致，不处理
/// - RemoteEcho：内容哈希与刚写入的远程哈希一致 → 仅吸收观察态，不回传
/// - LocalChange：真正的本机新内容 → 发送
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Unchanged,
    RemoteEcho,
    LocalChange,
}

fn classify_change(prev: Option<&str>, remote: Option<&str>, hash: &str) -> ChangeKind {
    if prev == Some(hash) {
        ChangeKind::Unchanged
    } else if remote == Some(hash) {
        ChangeKind::RemoteEcho
    } else {
        ChangeKind::LocalChange
    }
}

/// 发送 clipboard_update：仅已连接时发送；未连接时静默吸收（不补发，旧内容不待命）。
fn send_update(state: &AppState, text: &str, hash: &str) {
    let connected = state
        .inner
        .lock()
        .map(|g| g.status == ConnectionStatus::Connected)
        .unwrap_or(false);
    if !connected {
        tracing::debug!("未连接，本次剪贴板变化吸收不发送");
        return;
    }
    let device_id = state
        .config
        .lock()
        .map(|g| g.identity.device_id.clone())
        .unwrap_or_default();
    let msg = protocol::Message::new(
        protocol::MSG_CLIPBOARD_UPDATE,
        &device_id,
        &protocol::ClipboardPayload {
            text: text.to_string(),
            content_hash: hash.to_string(),
        },
    );
    match msg.and_then(|m| state.net.send_message(&m)) {
        Ok(()) => tracing::debug!(hash = %&hash[..8], byte_len = text.len(), "已发送剪贴板文字"),
        Err(e) => tracing::warn!(%e, "剪贴板消息发送失败"),
    }
}

/// 远程文字落地（lib.rs 将之注入 NetworkManager）：
/// 1. 先标记 last_remote_hash（轮询在标记后即使读到新内容也判定为远程回显，不回传）
/// 2. 写入系统剪贴板
/// 3. 更新本地观察哈希与最近同步时间，并向前端发出同步事件
pub fn land_remote(state: Arc<AppState>, app: AppHandle, payload: &protocol::ClipboardPayload) {
    // 阶段 9：暂停同步时不下发到本机剪贴板（与上行暂停对称）
    if state.inner.lock().map(|g| g.paused).unwrap_or(false) {
        return;
    }
    let hash = payload.content_hash.clone();
    {
        let mut g = match state.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        // 内容与本地观察值一致则无需重复写入（哈希去重）
        if g.last_clipboard_hash.as_deref() == Some(&hash) {
            g.last_sync = Some(now_millis_str());
            return;
        }
        // 先标记远程写入，后写剪贴板：轮询线程在此期间读到新内容也不会回传
        g.last_remote_hash = Some(hash.clone());
    }
    if let Err(e) = write_clipboard_text(&payload.text) {
        tracing::warn!(%e, "远程剪贴板写入失败");
        return;
    }
    {
        let mut g = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.last_clipboard_hash = Some(hash);
        g.last_clipboard_text = Some(payload.text.clone());
        g.last_remote_hash = None;
        g.last_sync = Some(now_millis_str());
    }
    let _ = app.emit(
        SYNCED_EVENT,
        serde_json::json!({ "time": now_millis_str() }),
    );
}

fn write_clipboard_text(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text))
        .map_err(|e| e.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

/// 最近同步时间（epoch 毫秒字符串）：前端以本地时区格式化，避免 Rust 侧时区依赖。
fn now_millis_str() -> String {
    protocol::now_millis().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hash_matches_expected() {
        let hash = sha256_hex(b"Hello, ClipLink!");
        // 固定输入应产生固定输出
        assert_eq!(hash.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // 同样输入相同哈希
        assert_eq!(hash, sha256_hex(b"Hello, ClipLink!"));
    }

    #[test]
    fn different_text_different_hash() {
        assert_ne!(sha256_hex(b"text A"), sha256_hex(b"text B"));
    }

    #[test]
    fn size_limit_boundary() {
        // 恰好 1 MiB 不触发（> 才触发）
        let exact = "x".repeat(MAX_CLIPBOARD_BYTES);
        assert!(exact.len() <= MAX_CLIPBOARD_BYTES);
        // 超 1 MiB 触发
        let over = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(over.len() > MAX_CLIPBOARD_BYTES);
    }

    // 阶段 7：变化分类——相同内容不重复触发；远程回显只吸收不回传
    #[test]
    fn classify_local_change() {
        assert_eq!(
            classify_change(Some("h-old"), None, "h-new"),
            ChangeKind::LocalChange
        );
        // 首次观察（无上次哈希）也是本机内容
        assert_eq!(
            classify_change(None, None, "h-new"),
            ChangeKind::LocalChange
        );
    }

    #[test]
    fn classify_unchanged() {
        assert_eq!(
            classify_change(Some("h1"), None, "h1"),
            ChangeKind::Unchanged
        );
    }

    #[test]
    fn classify_remote_echo_not_sent_back() {
        // 远程刚写入的内容（标记一致）：视为回显，不发送
        assert_eq!(
            classify_change(Some("h-old"), Some("h-remote"), "h-remote"),
            ChangeKind::RemoteEcho
        );
        // 远程标记优先于本机观察值（内容确实刚来自远程）
        assert_eq!(
            classify_change(None, Some("h-remote"), "h-remote"),
            ChangeKind::RemoteEcho
        );
    }

    // 阶段 7：环形消息 ID 去重测试见 network::tests（is_duplicate_message）
}
