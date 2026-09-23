// 外部状态文件：把 ClipLink 已有的连接状态/对端/RTT 以本机 JSON 暴露给 AI Relay。
// 纯辅助文件（%LOCALAPPDATA%\ClipLink\status.json），写入失败只 warn、绝不 panic、
// 绝不影响网络连接。RTT 复用现有 ping/pong 心跳，不重新实现；代次过滤复用 network 层。
use crate::protocol;
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

/// 状态文件结构：字段固定，顺序与示例一致；只含非敏感信息（无 secret/key/剪贴板内容）。
#[derive(Serialize)]
struct StatusFile {
    version: u32,
    status: String,
    peer_name: Option<String>,
    peer_ip: Option<String>,
    latency_ms: Option<u64>,
    generation: u64,
    updated_at: u64,
}

/// 状态文件写入器：记住“最近一次 RTT 及其代次”，据此决定状态文件里的 latency 是否清空。
pub struct ExternalStatusWriter {
    path: PathBuf,
    last_latency_ms: Option<u64>,
    last_latency_generation: u64,
}

impl ExternalStatusWriter {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_latency_ms: None,
            last_latency_generation: 0,
        }
    }

    /// 默认位置：%LOCALAPPDATA%\ClipLink\status.json（本机固定，与 Tauri app_data 目录无关）。
    pub fn default_path() -> PathBuf {
        let base =
            std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| std::ffi::OsString::from("."));
        PathBuf::from(base).join("ClipLink").join("status.json")
    }

    /// 收到连接状态事件：非 connected/paused 一律清空 latency；connected/paused 仅当
    /// 与最近一次 RTT 属同一代次时才保留，否则（新代次尚未收到首笔 pong）先置 null。
    pub fn record_status(
        &mut self,
        status: &str,
        is_connected_or_paused: bool,
        peer_name: Option<&str>,
        peer_ip: Option<&str>,
        generation: u64,
    ) {
        let latency = if is_connected_or_paused && self.last_latency_generation == generation {
            self.last_latency_ms
        } else {
            None
        };
        self.write(status, peer_name, peer_ip, latency, generation);
    }

    /// 收到有效 RTT 事件：更新最近 RTT 与代次，并连同当前状态/对端写文件。
    pub fn record_latency(
        &mut self,
        latency_ms: u64,
        status: &str,
        peer_name: Option<&str>,
        peer_ip: Option<&str>,
        generation: u64,
    ) {
        self.last_latency_ms = Some(latency_ms);
        self.last_latency_generation = generation;
        self.write(status, peer_name, peer_ip, Some(latency_ms), generation);
    }

    fn write(
        &self,
        status: &str,
        peer_name: Option<&str>,
        peer_ip: Option<&str>,
        latency_ms: Option<u64>,
        generation: u64,
    ) {
        let file = StatusFile {
            version: 1,
            status: status.to_string(),
            peer_name: peer_name.map(str::to_string),
            peer_ip: peer_ip.map(str::to_string),
            latency_ms,
            generation,
            updated_at: protocol::now_millis(),
        };
        let bytes = match serde_json::to_vec_pretty(&file) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "外部状态文件序列化失败，跳过写入");
                return;
            }
        };
        if let Err(e) = atomic_write(&self.path, &bytes) {
            tracing::warn!(path = %self.path.display(), %e, "外部状态文件写入失败（辅助文件，不影响连接）");
        }
    }
}

/// 远程剪贴板到达事件：ClipLink 每成功接收一次远端 A 的 clipboard_update 写一份，
/// 供 AI Relay 以 event_id 去重（绝不靠“系统剪贴板变了”猜来源）。单槽文件，
/// 只含非敏感信息（无 secret/key/token），此处允许把 A 端原文落本机。
#[derive(Serialize)]
struct RemoteClipboardEvent {
    version: u32,
    event_id: String,
    text: String,
    content_hash: String,
    updated_at: u64,
}

/// 远程剪贴板到达事件写入器：记住事件文件路径；每次 record 生成全新 event_id 并原子写。
/// 写失败只 warn、绝不 panic、绝不影响剪贴板同步。
pub struct RemoteClipboardWriter {
    path: PathBuf,
}

impl RemoteClipboardWriter {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 默认位置：%LOCALAPPDATA%\ClipLink\remote_clipboard.json（本机固定，与 Tauri app_data 目录无关）。
    pub fn default_path() -> PathBuf {
        let base =
            std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| std::ffi::OsString::from("."));
        PathBuf::from(base)
            .join("ClipLink")
            .join("remote_clipboard.json")
    }

    /// 记录一次“远端剪贴板到达”：生成全新 event_id 并原子写文件。写失败只 warn。
    pub fn record(&self, text: &str, content_hash: &str) {
        let file = RemoteClipboardEvent {
            version: 1,
            event_id: uuid::Uuid::new_v4().to_string(),
            text: text.to_string(),
            content_hash: content_hash.to_string(),
            updated_at: protocol::now_millis(),
        };
        let bytes = match serde_json::to_vec_pretty(&file) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "远程剪贴板事件序列化失败，跳过写入");
                return;
            }
        };
        if let Err(e) = atomic_write(&self.path, &bytes) {
            tracing::warn!(path = %self.path.display(), %e, "远程剪贴板事件写入失败（辅助文件，不影响剪贴板同步）");
        }
    }
}

/// 原子写：先写 <name>.tmp 并 flush/sync，再 rename 覆盖目标，避免 AI Relay 读到半截 JSON。
/// 目标父目录不存在时先创建。任一步失败返回 Err（调用方只 warn）。
fn atomic_write(target: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_name = format!(
        "{}.tmp",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("status.json")
    );
    let tmp = target.with_file_name(tmp_name);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.flush()?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fresh_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cliplink_extst_{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn read_file(target: &Path) -> serde_json::Value {
        let s = std::fs::read_to_string(target).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn connected_with_peer_writes_name_ip_and_null_latency() {
        let d = fresh_dir("t1");
        let target = d.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        w.record_status(
            "connected",
            true,
            Some("DESKTOP-A"),
            Some("192.168.191.95"),
            5,
        );
        let v = read_file(&target);
        assert_eq!(v["version"], 1);
        assert_eq!(v["status"], "connected");
        assert_eq!(v["peer_name"], "DESKTOP-A");
        assert_eq!(v["peer_ip"], "192.168.191.95");
        assert!(v["latency_ms"].is_null()); // 尚未收到首笔 pong
        assert_eq!(v["generation"], 5);
        assert!(v["updated_at"].as_u64().unwrap() > 0);
    }

    #[test]
    fn latency_event_writes_latency_ms() {
        let d = fresh_dir("t2");
        let target = d.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        w.record_status("connected", true, Some("A"), Some("1.2.3.4"), 5);
        w.record_latency(18, "connected", Some("A"), Some("1.2.3.4"), 5);
        let v = read_file(&target);
        assert_eq!(v["latency_ms"], 18);
        assert_eq!(v["generation"], 5);
        assert_eq!(v["status"], "connected");
    }

    #[test]
    fn connected_new_generation_clears_old_latency() {
        let d = fresh_dir("t3");
        let target = d.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        w.record_status("connected", true, Some("A"), Some("1.2.3.4"), 5);
        w.record_latency(18, "connected", Some("A"), Some("1.2.3.4"), 5);
        // 新代次 6 刚建立，尚未收到 pong：旧 latency 必须清空
        w.record_status("connected", true, Some("B"), Some("2.3.4.5"), 6);
        let v = read_file(&target);
        assert!(v["latency_ms"].is_null());
        assert_eq!(v["generation"], 6);
        assert_eq!(v["peer_name"], "B");
    }

    #[test]
    fn reconnecting_clears_latency() {
        let d = fresh_dir("t4");
        let target = d.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        w.record_status("connected", true, Some("A"), Some("1.2.3.4"), 5);
        w.record_latency(18, "connected", Some("A"), Some("1.2.3.4"), 5);
        w.record_status("reconnecting", false, None, None, 6);
        let v = read_file(&target);
        assert_eq!(v["status"], "reconnecting");
        assert!(v["latency_ms"].is_null());
        assert!(v["peer_name"].is_null());
    }

    #[test]
    fn offline_clears_latency() {
        let d = fresh_dir("t5");
        let target = d.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        w.record_status("connected", true, Some("A"), Some("1.2.3.4"), 5);
        w.record_latency(18, "connected", Some("A"), Some("1.2.3.4"), 5);
        w.record_status("offline", false, None, None, 6);
        let v = read_file(&target);
        assert_eq!(v["status"], "offline");
        assert!(v["latency_ms"].is_null());
    }

    #[test]
    fn paused_keeps_peer_but_new_status_event_not_inherit_old_latency() {
        let d = fresh_dir("t6");
        let target = d.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        w.record_status("connected", true, Some("A"), Some("1.2.3.4"), 5);
        w.record_latency(18, "connected", Some("A"), Some("1.2.3.4"), 5);
        // paused 作为新状态事件（新代次）：对端保留，但不继承旧 latency
        w.record_status("paused", true, Some("A"), Some("1.2.3.4"), 6);
        let v = read_file(&target);
        assert_eq!(v["status"], "paused");
        assert_eq!(v["peer_name"], "A");
        assert_eq!(v["peer_ip"], "1.2.3.4");
        assert!(v["latency_ms"].is_null());
    }

    #[test]
    fn file_is_valid_json_with_expected_keys() {
        let d = fresh_dir("t7");
        let target = d.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        w.record_status("connected", true, Some("A"), Some("1.2.3.4"), 1);
        w.record_latency(12, "connected", Some("A"), Some("1.2.3.4"), 1);
        let v = read_file(&target);
        assert!(v.is_object());
        for key in [
            "version",
            "status",
            "peer_name",
            "peer_ip",
            "latency_ms",
            "generation",
            "updated_at",
        ] {
            assert!(v.get(key).is_some(), "缺少字段 {key}");
        }
        // 不含敏感字段
        for key in [
            "token",
            "secret",
            "clipboard",
            "device_secret",
            "shared_key",
        ] {
            assert!(v.get(key).is_none(), "意外出现敏感字段 {key}");
        }
    }

    #[test]
    fn write_failure_does_not_panic() {
        let d = fresh_dir("t8");
        // 让目标父目录无法创建：blocker 是文件，blocker/status.json 的父目录 blocker 建不了
        let blocker = d.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("status.json");
        let mut w = ExternalStatusWriter::new(target.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            w.record_status("connected", true, Some("A"), Some("1.2.3.4"), 1);
        }));
        assert!(result.is_ok(), "写状态文件失败不应 panic");
        assert!(!target.exists(), "写失败时目标文件不应被创建");
    }

    // L04-03：remote_clipboard.json 写入器测试（只写 temp 目录，不碰真实 %LOCALAPPDATA%\ClipLink）
    #[test]
    fn remote_event_file_schema_correct() {
        let d = fresh_dir("r1");
        let target = d.join("remote_clipboard.json");
        let w = RemoteClipboardWriter::new(target.clone());
        w.record("A端原文", "abc123");
        let v = read_file(&target); // 原子写后应可正常解析为 JSON
        assert!(v.is_object());
        for key in ["version", "event_id", "text", "content_hash", "updated_at"] {
            assert!(v.get(key).is_some(), "缺少字段 {key}");
        }
        assert_eq!(v["version"], 1);
        assert_eq!(v["content_hash"], "abc123");
        assert!(v["updated_at"].as_u64().unwrap() > 0);
        assert!(!v["event_id"].as_str().unwrap().is_empty());
        // 不含敏感字段
        for key in [
            "token",
            "device_secret",
            "shared_key",
            "pairing_secret",
            "secret",
            "key",
        ] {
            assert!(v.get(key).is_none(), "意外出现敏感字段 {key}");
        }
    }

    #[test]
    fn remote_event_text_preserved_verbatim() {
        let d = fresh_dir("r2");
        let target = d.join("remote_clipboard.json");
        let w = RemoteClipboardWriter::new(target.clone());
        let original = "第一行\n第二行\t制表\n中文：剪贴板同步";
        w.record(original, "deadbeef");
        let v = read_file(&target);
        assert_eq!(
            v["text"].as_str().unwrap(),
            original,
            "文本须原样保存（含换行/制表/中文）"
        );
    }

    #[test]
    fn remote_event_content_hash_preserved() {
        let d = fresh_dir("r3");
        let target = d.join("remote_clipboard.json");
        let w = RemoteClipboardWriter::new(target.clone());
        w.record("x", "cafebabe64");
        let v = read_file(&target);
        assert_eq!(v["content_hash"].as_str().unwrap(), "cafebabe64");
    }

    #[test]
    fn remote_event_id_differs_each_write() {
        let d = fresh_dir("r4");
        let target = d.join("remote_clipboard.json");
        let w = RemoteClipboardWriter::new(target.clone());
        w.record("same", "h1");
        let e1 = read_file(&target)["event_id"].as_str().unwrap().to_string();
        w.record("same", "h1"); // 相同内容再次写入
        let e2 = read_file(&target)["event_id"].as_str().unwrap().to_string();
        assert_ne!(e1, e2, "每次写入应生成新的 event_id");
    }

    #[test]
    fn remote_event_write_failure_does_not_panic() {
        let d = fresh_dir("r5");
        // 父目录无法创建（blocker 是文件）→ 写失败
        let blocker = d.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("remote_clipboard.json");
        let w = RemoteClipboardWriter::new(target.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            w.record("hello", "h");
        }));
        assert!(result.is_ok(), "写事件文件失败不应 panic");
        assert!(!target.exists(), "写失败时目标文件不应被创建");
    }
}
