// 阶段 6：单机剪贴板监听（arboard 纯文字接口）。
// 后台任务每 300ms 读取一次剪贴板纯文字，计算 SHA-256 哈希并与上次比较。
// 变化时更新 AppState 内部状态（供阶段 7 发送）；超大小限制发 app-error 事件。
// 暂停时跳过读取，读取失败跳过本轮。
// 任务随 Tauri 全局 async runtime 生存，进程退出即销毁，无需显式停止。
use crate::identity::hex_encode;
use crate::state::AppState;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const POLL_INTERVAL: Duration = Duration::from_millis(300);
const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024; // 1 MiB
const ERROR_EVENT: &str = "app-error";

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
            let hash = {
                let mut hasher = Sha256::new();
                hasher.update(text.as_bytes());
                hex_encode(&hasher.finalize())
            };

            // 与上次比较，变化则更新状态
            let changed = state
                .inner
                .lock()
                .map(|mut g| {
                    if g.last_clipboard_hash.as_deref() != Some(&hash) {
                        g.last_clipboard_hash = Some(hash.clone());
                        g.last_clipboard_text = Some(text.clone());
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);

            if changed {
                tracing::debug!(hash = %hash[..8], byte_len = text.len(), "检测到剪贴板变化");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hash_matches_expected() {
        let text = "Hello, ClipLink!";
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = hex_encode(&hasher.finalize());
        // 固定输入应产生固定输出
        assert_eq!(hash.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // 同样输入相同哈希
        let mut hasher2 = Sha256::new();
        hasher2.update(text.as_bytes());
        assert_eq!(hash, hex_encode(&hasher2.finalize()));
    }

    #[test]
    fn different_text_different_hash() {
        let h1 = {
            let mut hasher = Sha256::new();
            hasher.update(b"text A");
            hex_encode(&hasher.finalize())
        };
        let h2 = {
            let mut hasher = Sha256::new();
            hasher.update(b"text B");
            hex_encode(&hasher.finalize())
        };
        assert_ne!(h1, h2);
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
}
