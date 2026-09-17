// 阶段 4：配置读写（应用数据目录 JSON；临时文件 + 原子替换保存；损坏时备份原文件并恢复默认）。
use crate::error::AppError;
use crate::identity::{self, LocalIdentity};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const CONFIG_FILE: &str = "config.json";
pub const SCHEMA_VERSION: u32 = 1;
/// 默认监听端口：当前版本固定 45888/TCP。
pub const DEFAULT_LISTEN_PORT: u16 = 45888;

/// 应用配置。schema_version 必须存在；缺失的非关键字段由 serde 默认值补齐（前向兼容），
/// 出现但语义无效的关键字段（端口 0、非法 IPv4、无效身份）整体按损坏配置处理。
/// 未知字段：serde 默认策略为忽略（低版本读取高版本新增字段不报错）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub schema_version: u32,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default)]
    pub autostart: bool,
    /// 阶段 8：断线后按保存的对方 IP 自动重连。旧配置缺省视为开启。
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    #[serde(default)]
    pub sync_paused: bool,
    #[serde(default)]
    pub last_peer_ip: Option<String>,
    pub identity: LocalIdentity,
}

fn default_listen_port() -> u16 {
    DEFAULT_LISTEN_PORT
}

fn default_true() -> bool {
    true
}

impl AppConfig {
    /// 默认配置 + 新本机身份（首次启动或损坏恢复时使用）。
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            listen_port: DEFAULT_LISTEN_PORT,
            autostart: false,
            auto_reconnect: true,
            sync_paused: false,
            last_peer_ip: None,
            identity: identity::new_identity(),
        }
    }

    /// 关键字段语义校验。
    pub fn validate(&self) -> Result<(), AppError> {
        if self.listen_port == 0 {
            return Err(AppError::new("listen_port 必须为有效非零端口"));
        }
        if let Some(ip) = &self.last_peer_ip {
            if !is_valid_ipv4_str(ip) {
                return Err(AppError::new(format!("last_peer_ip 不是有效 IPv4: {ip}")));
            }
        }
        identity::validate_identity(&self.identity)
    }
}

/// 合法 IPv4 点分十进制（每段 0–255、1–3 位数字）。
/// 当前产品只存 ZeroTier IPv4；不排除特殊网段，连接前另有校验。
pub fn is_valid_ipv4_str(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| n <= 255)
    })
}

/// 配置读写入口。测试经 `new(临时目录)` 指向任意目录，不触碰真实用户配置。
pub struct ConfigStore {
    dir: PathBuf,
    /// 串行化完整保存流程（写临时文件 + 替换），防止并发保存互相踩踏同一个临时文件。
    save_lock: Mutex<()>,
}

impl ConfigStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            save_lock: Mutex::new(()),
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    /// 启动加载：
    /// - 文件不存在 → 生成默认配置与本机身份，立即保存并返回；
    /// - 文件存在且有效 → 直接返回；
    /// - JSON 无法解析或关键身份字段无效 → 原文件先备份为 .bak（不覆盖旧备份），
    ///   再生成新默认配置原子保存；警告日志不含配置正文与密钥；原文件不会被静默删除。
    pub fn load(&self) -> Result<AppConfig, AppError> {
        let path = self.config_path();
        if !path.exists() {
            let cfg = AppConfig::new();
            self.save(&cfg)?;
            tracing::info!(dir = %self.dir.display(), "首次启动：已创建默认配置和本机身份");
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| AppError::new(format!("读取配置文件失败: {e}")))?;
        let parsed: Result<AppConfig, String> = serde_json::from_str(&raw)
            .map_err(|e| e.to_string())
            .and_then(|cfg: AppConfig| cfg.validate().map(|_| cfg).map_err(|e| e.to_string()));
        match parsed {
            Ok(cfg) => Ok(cfg),
            Err(reason) => {
                let bak = self.backup_corrupt(&path)?;
                tracing::warn!(reason = %reason, backup = %bak.display(), "配置文件损坏：原文件已备份，恢复默认配置");
                let cfg = AppConfig::new();
                self.save(&cfg)?;
                Ok(cfg)
            }
        }
    }

    /// 损坏原文件备份到同目录 config.json.bak-<unix秒>；冲突时追加计数，永不覆盖旧备份。
    fn backup_corrupt(&self, path: &Path) -> Result<PathBuf, AppError> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut n = 0u32;
        loop {
            let name = if n == 0 {
                format!("config.json.bak-{ts}")
            } else {
                format!("config.json.bak-{ts}-{n}")
            };
            let target = self.dir.join(name);
            if !target.exists() {
                std::fs::copy(path, &target)
                    .map_err(|e| AppError::new(format!("备份损坏配置失败: {e}")))?;
                return Ok(target);
            }
            n += 1;
        }
    }

    /// 原子保存：序列化 → 同目录临时文件（写 + sync_all）→ 替换正式文件。
    /// 失败时删除临时文件并返回错误，正式配置不会留下半截内容。
    pub fn save(&self, cfg: &AppConfig) -> Result<(), AppError> {
        let json = serde_json::to_string_pretty(cfg)
            .map_err(|e| AppError::new(format!("序列化配置失败: {e}")))?;
        let _guard = self
            .save_lock
            .lock()
            .map_err(|e| AppError::new(format!("保存锁被破坏: {e}")))?;
        atomic_write(&self.dir, CONFIG_FILE, &json)
            .map_err(|e| AppError::new(format!("保存配置文件失败: {e}")))
    }
}

/// 同目录临时文件 `<filename>.tmp` 写入 + sync_all，再替换 `<filename>`。
/// Windows 用 MoveFileExW(MOVEFILE_REPLACE_EXISTING)——std::fs::rename 在 Windows 上
/// 目标已存在会报错，不能直接假设 Unix 行为；MoveFileExW 非零返回表示成功。
/// 其他平台用 rename。失败时清理临时文件。
pub fn atomic_write(dir: &Path, filename: &str, data: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!("{filename}.tmp"));
    let dst = dir.join(filename);
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        use std::io::Write;
        file.write_all(data.as_bytes())?;
        file.sync_all()?;
        std::mem::drop(file);
        replace_file_retried(&tmp, &dst)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 替换步骤重试：Windows Defender 等可能瞬时占用新写入的临时文件（拒绝访问），
/// 对 PermissionDenied 做短重试；其他错误立即返回。
fn replace_file_retried(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut attempt = 0u32;
    loop {
        match replace_file(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 5 && e.kind() == std::io::ErrorKind::PermissionDenied => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(windows)]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winbase::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
    let to_w = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let s = to_w(src);
    let d = to_w(dst);
    let rc = unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    if rc == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::rename(src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("cliplink-test-{}-{ts}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn read_cfg(dir: &Path) -> AppConfig {
        let raw = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    // 首次启动创建配置文件；UUID v4 有效；secret 解码 32 字节；二次加载 device_id / device_secret 不变
    #[test]
    fn first_run_creates_config_and_identity_is_stable() {
        let dir = tmp_dir();
        let store = ConfigStore::new(dir.clone());
        let first = store.load().unwrap();
        assert!(store.config_path().exists());
        let u = uuid::Uuid::parse_str(&first.identity.device_id).unwrap();
        assert_eq!(u.as_bytes()[6] >> 4, 4, "device_id 应为 UUID v4");
        assert_eq!(
            identity::hex_decode(&first.identity.device_secret)
                .unwrap()
                .len(),
            32
        );
        let second = ConfigStore::new(dir).load().unwrap();
        assert_eq!(second.identity.device_id, first.identity.device_id);
        assert_eq!(second.identity.device_secret, first.identity.device_secret);
    }

    // 默认 listen_port = 45888；默认 last_peer_ip 为 null
    #[test]
    fn defaults() {
        let c = AppConfig::new();
        assert_eq!(c.listen_port, 45888);
        assert_eq!(c.schema_version, 1);
        assert!(!c.autostart);
        assert!(c.auto_reconnect);
        assert!(!c.sync_paused);
        assert!(c.last_peer_ip.is_none());
    }

    // 有效 last_peer_ip 保存并可重新加载
    #[test]
    fn valid_last_peer_ip_persists() {
        let dir = tmp_dir();
        let store = ConfigStore::new(dir.clone());
        let mut cfg = store.load().unwrap();
        cfg.last_peer_ip = Some("10.147.17.36".to_string());
        store.save(&cfg).unwrap();
        let re = ConfigStore::new(dir).load().unwrap();
        assert_eq!(re.last_peer_ip.as_deref(), Some("10.147.17.36"));
    }

    // 非法 IPv4 与端口 0 被拒绝
    #[test]
    fn invalid_ipv4_and_zero_port_rejected() {
        for bad in ["999.1.1.1", "1.2.3", "a.b.c.d", "1.2.3.4.5", " ", ""] {
            let mut c = AppConfig::new();
            c.last_peer_ip = Some(bad.to_string());
            assert!(c.validate().is_err(), "应拒绝 {bad:?}");
        }
        assert!(is_valid_ipv4_str("192.168.191.180"));
        assert!(is_valid_ipv4_str("10.0.0.1"));
        let mut c = AppConfig::new();
        c.listen_port = 0;
        assert!(c.validate().is_err());
    }

    // JSON 损坏：原文件被备份（备份内容 = 损坏原文），随后生成新的有效配置
    #[test]
    fn corrupt_json_backed_up_and_recovered() {
        let dir = tmp_dir();
        let store = ConfigStore::new(dir.clone());
        let good = store.load().unwrap();
        let corrupted = "{ not valid json";
        std::fs::write(store.config_path(), corrupted).unwrap();
        let recovered = ConfigStore::new(dir.clone()).load().unwrap();
        let baks: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config.json.bak"))
            .collect();
        assert_eq!(baks.len(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.join(&baks[0])).unwrap(),
            corrupted
        );
        assert!(recovered.validate().is_ok());
        let on_disk = read_cfg(&dir);
        assert!(on_disk.validate().is_ok());
        assert_ne!(on_disk.identity.device_id, good.identity.device_id);
    }

    // 无效 UUID 被识别为损坏配置
    #[test]
    fn invalid_uuid_is_corrupt() {
        let dir = tmp_dir();
        let store = ConfigStore::new(dir.clone());
        let cfg = store.load().unwrap();
        let mut v = serde_json::to_value(&cfg).unwrap();
        v["identity"]["device_id"] = serde_json::Value::String("not-a-uuid".into());
        std::fs::write(store.config_path(), serde_json::to_string(&v).unwrap()).unwrap();
        let recovered = ConfigStore::new(dir.clone()).load().unwrap();
        uuid::Uuid::parse_str(&recovered.identity.device_id).unwrap();
        assert_ne!(recovered.identity.device_id, "not-a-uuid");
    }

    // 无效或错误长度的 device_secret 被识别为损坏配置
    #[test]
    fn bad_secret_is_corrupt() {
        for bad in ["zz", "abc", &identity::hex_encode(&[0u8; 31])] {
            let dir = tmp_dir();
            let store = ConfigStore::new(dir.clone());
            let cfg = store.load().unwrap();
            let mut v = serde_json::to_value(&cfg).unwrap();
            v["identity"]["device_secret"] = serde_json::Value::String(bad.to_string());
            std::fs::write(store.config_path(), serde_json::to_string(&v).unwrap()).unwrap();
            let recovered = ConfigStore::new(dir).load().unwrap();
            assert_eq!(
                identity::hex_decode(&recovered.identity.device_secret)
                    .unwrap()
                    .len(),
                32
            );
            assert_ne!(&recovered.identity.device_secret, bad);
        }
    }

    // 临时文件写入失败（临时路径被目录占用）时不破坏原配置；障碍解除后可继续保存
    #[test]
    fn tmp_write_failure_keeps_original() {
        let dir = tmp_dir();
        let store = ConfigStore::new(dir.clone());
        let cfg = store.load().unwrap();
        let before = std::fs::read_to_string(store.config_path()).unwrap();
        std::fs::create_dir(dir.join("config.json.tmp")).unwrap();
        let mut new = cfg.clone();
        new.autostart = true;
        assert!(store.save(&new).is_err());
        assert_eq!(
            std::fs::read_to_string(store.config_path()).unwrap(),
            before
        );
        std::fs::remove_dir(dir.join("config.json.tmp")).unwrap();
        store.save(&new).unwrap();
        assert_eq!(read_cfg(&dir).autostart, true);
    }

    // 连续多次保存后文件仍可解析且字段正确
    #[test]
    fn repeated_saves_stay_parseable() {
        let dir = tmp_dir();
        let store = ConfigStore::new(dir.clone());
        let mut cfg = store.load().unwrap();
        for i in 0..3u8 {
            cfg.sync_paused = i % 2 == 1;
            cfg.last_peer_ip = Some(format!("10.0.0.{i}"));
            store.save(&cfg).unwrap();
            let re = read_cfg(&dir);
            assert!(re.validate().is_ok());
            assert_eq!(
                re.last_peer_ip.as_deref(),
                Some(format!("10.0.0.{i}")).as_deref()
            );
            assert_eq!(re.sync_paused, i % 2 == 1);
        }
    }

    // 缺失可默认字段时按既定兼容策略加载（listen_port 回默认 45888，其余回 null/false；
    // auto_reconnect 旧配置视为开启）
    #[test]
    fn missing_optional_fields_get_defaults() {
        let dir = tmp_dir();
        let cfg = AppConfig::new();
        let mut v = serde_json::to_value(&cfg).unwrap();
        for k in [
            "listen_port",
            "autostart",
            "auto_reconnect",
            "sync_paused",
            "last_peer_ip",
        ] {
            v.as_object_mut().unwrap().remove(k);
        }
        std::fs::write(dir.join(CONFIG_FILE), serde_json::to_string(&v).unwrap()).unwrap();
        let loaded = ConfigStore::new(dir).load().unwrap();
        assert_eq!(loaded.listen_port, 45888);
        assert!(!loaded.autostart);
        assert!(loaded.auto_reconnect);
        assert!(!loaded.sync_paused);
        assert!(loaded.last_peer_ip.is_none());
    }

    // 并发保存（专用锁串行化）后文件仍可解析，无半截 JSON
    #[test]
    fn concurrent_saves_stay_valid() {
        let dir = tmp_dir();
        let store = std::sync::Arc::new(ConfigStore::new(dir.clone()));
        std::thread::scope(|s| {
            for t in 0..8u8 {
                let store = store.clone();
                s.spawn(move || {
                    for i in 0..5u8 {
                        let mut cfg = store.load().unwrap();
                        cfg.last_peer_ip = Some(format!("10.9.{t}.{i}"));
                        store.save(&cfg).unwrap();
                    }
                });
            }
        });
        let re = read_cfg(&dir);
        assert!(re.validate().is_ok());
    }
}
