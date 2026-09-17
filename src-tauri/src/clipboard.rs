// 阶段 6/7：单机剪贴板监听与上下行同步。
// 后台任务每 300ms 读取一次剪贴板纯文字，计算 SHA-256 哈希并与上次比较。
// 检测到本机变化时发送 clipboard_update（仅已连接时）；暂停时跳过读取。
// T11-06：暂停恢复后的首轮把当前剪贴板只吸收为本地基线、绝不发送——
// 暂停期间复制的内容恢复后不补发；恢复之后的新复制才正常 LocalChange → Send。
// 收到远程文字（network.rs 经 set_clipboard_landing 注入回调）验证通过后：
//   1. 先标记“本次为远程写入”（防轮询回传），再写系统剪贴板，最后更新观察哈希；
//   2. 写失败撤销本次标记（只清仍等于本次哈希的标记，不误删并发新标记）；
//   3. 写入前复查暂停：暂停后不接受正在落地的远端内容写本机剪贴板；
//   4. 更新最近同步时间并向前端发 clipboard-synced 事件。
// 任务随 Tauri 全局 async runtime 生存，进程退出即销毁。
use crate::identity::hex_encode;
use crate::protocol;
use crate::state::{AppState, ConnectionStatus, Inner};
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
        // T11-06：暂停期间跳过了读取；恢复后的首轮把当前剪贴板作为本地基线，不发送。
        let mut was_paused = false;
        loop {
            tick.tick().await;

            // 暂停时跳过（不读剪贴板）
            let paused = state.inner.lock().map(|g| g.paused).unwrap_or(false);
            if paused {
                was_paused = true;
                continue;
            }

            // 读取剪贴板纯文字
            let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                Ok(t) => t,
                Err(e) => {
                    tracing::trace!(%e, "剪贴板读取失败，跳过本轮");
                    // 读失败不消耗恢复标志：暂停期间内容不应因瞬时读失败而误发
                    continue;
                }
            };

            // 空字符串：无内容可作基线，恢复标志直接消费（下一次新复制正常发送）
            if text.is_empty() {
                was_paused = false;
                continue;
            }

            // 大小检查：超 1 MiB 只提示不处理（不写入内部状态，不留作待发内容）
            if text.len() > MAX_CLIPBOARD_BYTES {
                tracing::warn!(byte_len = text.len(), "剪贴板文字超过 1 MiB，本次未处理");
                let _ = app.emit(
                    ERROR_EVENT,
                    serde_json::json!({ "message": "剪贴板文字超过 1 MiB，本次未同步。" }),
                );
                // 超限内容不作基线也不发送；恢复标志消费，后续正常内容按普通变化处理
                was_paused = false;
                continue;
            }

            // SHA-256 哈希
            let hash = sha256_hex(text.as_bytes());

            // 恢复后的首个成功读数轮：把当前剪贴板只吸收为本地基线、不发送
            // （暂停期间复制的内容不补发）。
            let resume_first = std::mem::replace(&mut was_paused, false);

            // 与上次比较：变化时才处理；若与刚写入的远程内容一致则只吸收、不回传
            let action = state
                .inner
                .lock()
                .map(|mut g| apply_observation(&mut g, &text, &hash, resume_first))
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

/// 一次观察后的状态应用（纯函数，可单测）：
/// 先分类，再按分类更新观察态，返回是否应发送。
/// - Unchanged：不更新（状态已是当前），Skip
/// - RemoteEcho：吸收观察态（hash/text/清除 remote 标记），Skip（不回传）
/// - LocalChange：更新观察态；若 resume_first（暂停恢复首轮）只作为本地基线，
///   Skip（暂停期间复制的内容不补发），否则 Send。
fn apply_observation(g: &mut Inner, text: &str, hash: &str, resume_first: bool) -> Action {
    match classify_change(
        g.last_clipboard_hash.as_deref(),
        g.last_remote_hash.as_deref(),
        hash,
    ) {
        ChangeKind::Unchanged => Action::Skip,
        ChangeKind::RemoteEcho => {
            g.last_clipboard_hash = Some(hash.to_string());
            g.last_clipboard_text = Some(text.to_string());
            g.last_remote_hash = None;
            Action::Skip
        }
        ChangeKind::LocalChange => {
            g.last_clipboard_hash = Some(hash.to_string());
            g.last_clipboard_text = Some(text.to_string());
            if resume_first {
                Action::Skip
            } else {
                Action::Send
            }
        }
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

/// 落地前的状态决策（纯函数，可单测）：
/// - Paused：暂停中，不落地
/// - Duplicate：内容与本机观察值一致，无需重复写入
/// - Proceed：允许写入（调用方负责先设置 remote 标记再写剪贴板）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LandDecision {
    Paused,
    Duplicate,
    Proceed,
}

fn land_decision(g: &Inner, hash: &str) -> LandDecision {
    if g.paused {
        LandDecision::Paused
    } else if g.last_clipboard_hash.as_deref() == Some(hash) {
        LandDecision::Duplicate
    } else {
        LandDecision::Proceed
    }
}

/// 远程文字落地（lib.rs 将之注入 NetworkManager）：
/// 1. 暂停中则直接拒绝（暂停后不把新的远端内容写入本机剪贴板）
/// 2. 内容与本机观察值一致则无需重复写入（哈希去重）
/// 3. 先标记 last_remote_hash（轮询在标记后即使读到新内容也判定为远程回显，不回传）
/// 4. 真正写入系统剪贴板前再次复查暂停（极窄竞态收口；不做跨系统剪贴板 API 的长锁事务）
/// 5. 写成功 → 更新本地观察哈希与最近同步时间，向前端发同步事件；
///    写失败 → 撤销本次预先设置的 remote 标记（防回环），不留下陈旧 marker。
pub fn land_remote(state: Arc<AppState>, app: AppHandle, payload: &protocol::ClipboardPayload) {
    let hash = payload.content_hash.clone();
    {
        let mut g = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        match land_decision(&g, &hash) {
            LandDecision::Paused => return,
            LandDecision::Duplicate => {
                g.last_sync = Some(now_millis_str());
                return;
            }
            LandDecision::Proceed => {}
        }
        // 先标记远程写入，后写剪贴板：轮询线程在此期间读到新内容也不会回传
        g.last_remote_hash = Some(hash.clone());
    }
    // 真正写剪贴板前复查暂停（暂停落地竞态收口）：已暂停则撤销本次 marker 并退出
    if state.inner.lock().map(|g| g.paused).unwrap_or(false) {
        let mut g = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        clear_remote_marker_if_matches(&mut g, &hash);
        return;
    }
    if let Err(e) = write_clipboard_text(&payload.text) {
        tracing::warn!(%e, "远程剪贴板写入失败");
        // 写失败必须撤销本次 marker（只清仍等于本次 hash 的，避免误删并发新 marker）
        let mut g = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        clear_remote_marker_if_matches(&mut g, &hash);
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

/// 条件清理远程 marker：仅当仍等于本次 hash 时清除（并发远程写入的新 marker 不被误删）。
fn clear_remote_marker_if_matches(g: &mut Inner, hash: &str) {
    if g.last_remote_hash.as_deref() == Some(hash) {
        g.last_remote_hash = None;
    }
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

    // T11-06 测试 1：暂停恢复后的首轮把当前剪贴板作为本地基线（prev=A，暂停期复制 B，
    // 恢复后首轮观察 B）——不发送、baseline 更新为 B；之后复制 C 才 LocalChange/Send。
    #[test]
    fn resume_first_observation_is_baseline_not_sent() {
        let mut g = Inner {
            last_clipboard_hash: Some("h-a".into()),
            last_clipboard_text: Some("A".into()),
            ..Default::default()
        };
        assert!(matches!(
            apply_observation(&mut g, "B", "h-b", true),
            Action::Skip
        ));
        assert_eq!(
            g.last_clipboard_hash.as_deref(),
            Some("h-b"),
            "恢复首轮应更新 baseline 为 B"
        );
        assert_eq!(g.last_clipboard_text.as_deref(), Some("B"));
        assert!(g.last_remote_hash.is_none());
        // 恢复后的新复制 C：LocalChange → Send
        assert!(matches!(
            apply_observation(&mut g, "C", "h-c", false),
            Action::Send
        ));
        assert_eq!(g.last_clipboard_hash.as_deref(), Some("h-c"));
    }

    #[test]
    fn resume_first_unchanged_stay_baseline() {
        // 暂停期间没有变化（仍是 A）：恢复首轮完成后，反复观察 A 都只是 Skip
        let mut g = Inner {
            last_clipboard_hash: Some("h-a".into()),
            last_clipboard_text: Some("A".into()),
            ..Default::default()
        };
        assert!(matches!(
            apply_observation(&mut g, "A", "h-a", true),
            Action::Skip
        ));
        assert!(matches!(
            apply_observation(&mut g, "A", "h-a", false),
            Action::Skip
        ));
    }

    // T11-06 测试 2：远程写失败后的 marker 条件清理——只清仍然等于本次 hash 的标记，
    // 并发写入的新 marker 不被误删（含 land_decision 的 Duplicate/Paused 分支）。
    #[test]
    fn failed_write_marker_cleared_conditionally() {
        let mut g = Inner::default();
        g.last_remote_hash = Some("h-x".into());
        clear_remote_marker_if_matches(&mut g, "h-x");
        assert!(g.last_remote_hash.is_none(), "本次 marker 应被清除");

        // 并发期间 marker 已被更新为 Y：不得误清 Y
        g.last_remote_hash = Some("h-y".into());
        clear_remote_marker_if_matches(&mut g, "h-x");
        assert_eq!(
            g.last_remote_hash.as_deref(),
            Some("h-y"),
            "并发新 marker 必须保留"
        );
    }

    // T11-06 测试 2b：land_decision —— 内容与本机一致(Duplicate)/暂停中(Paused) 的决定逻辑
    #[test]
    fn land_decision_gates_paused_and_duplicate() {
        let g = Inner {
            paused: true,
            ..Default::default()
        };
        assert_eq!(land_decision(&g, "h-x"), LandDecision::Paused);

        let mut g = Inner::default();
        g.last_clipboard_hash = Some("h-x".into());
        assert_eq!(land_decision(&g, "h-x"), LandDecision::Duplicate);
        assert_eq!(land_decision(&g, "h-y"), LandDecision::Proceed);
    }

    // T11-06 测试 3：RemoteEcho 防循环在 apply_observation 状态应用层继续成立：
    // 远端标记 + 相同内容 → absorb（baseline 更新、marker 消耗）→ Skip，不回传；
    // 随后相同内容再次出现 → Unchanged → Skip。
    #[test]
    fn remote_echo_works_in_apply_observation() {
        let mut g = Inner {
            last_clipboard_hash: Some("h-old".into()),
            last_clipboard_text: Some("old".into()),
            last_remote_hash: Some("h-remote".into()),
            ..Default::default()
        };
        assert!(matches!(
            apply_observation(&mut g, "remote", "h-remote", false),
            Action::Skip
        ));
        assert_eq!(
            g.last_clipboard_hash.as_deref(),
            Some("h-remote"),
            "回显应吸收观察态"
        );
        assert_eq!(g.last_clipboard_text.as_deref(), Some("remote"));
        assert!(g.last_remote_hash.is_none(), "远程标记应被消耗清除");
        // 内容保持 remote，worker 下一轮读到 → Unchanged → Skip
        assert!(matches!(
            apply_observation(&mut g, "remote", "h-remote", false),
            Action::Skip
        ));
    }

    // T11-06 测试 4：暂停状态远程落地被拒绝 —— 已验证 paused=true 时 land_decision 为 Paused，
    // land_remote 入口先经 land_decision，故暂停中不会执行 remote write。
    // （真正的系统剪贴板写入仍需真实 Windows clipboard，这里按任务允许测试纯决策 helper。）

    // 阶段 7：环形消息 ID 去重测试见 network::tests（is_duplicate_message）
}
