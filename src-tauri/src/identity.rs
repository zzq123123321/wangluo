// 阶段 4：本机身份（device_id / device_name / device_secret，首次启动生成，之后保持稳定）。
use crate::error::AppError;
use serde::{Deserialize, Serialize};

/// device_secret 长度（字节）：系统安全随机源生成。
pub const SECRET_LEN: usize = 32;
/// 回退设备名：计算机名读取失败或为空时使用。
pub const DEVICE_NAME_FALLBACK: &str = "Windows-PC";
/// 设备名长度上限（字符）：设备名只用于界面显示与端到端 hello 交换，超长截断。
pub const DEVICE_NAME_MAX_LEN: usize = 64;

/// 本机身份：首次启动生成并持久化到配置；之后启动复用，不得重新生成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalIdentity {
    /// UUID v4 字符串
    pub device_id: String,
    /// Windows 计算机名（回退与长度限制后）
    pub device_name: String,
    /// 32 字节安全随机密钥，小写十六进制持久化（64 字符）。敏感材料：不得进入日志/前端/事件。
    pub device_secret: String,
}

pub fn new_identity() -> LocalIdentity {
    LocalIdentity {
        device_id: uuid::Uuid::new_v4().to_string(),
        device_name: read_device_name(),
        device_secret: generate_device_secret(),
    }
}

/// 读取 Windows 计算机名（hostname 在 Windows 上返回计算机名），经回退与长度限制处理。
pub fn read_device_name() -> String {
    let raw = hostname::get()
        .ok()
        .and_then(|h| h.to_str().map(|s| s.to_string()))
        .unwrap_or_default();
    normalize_device_name(&raw)
}

/// 纯函数（可测试）：首尾去空白 → 超过长度上限截断 → 仍为空则回退 DEVICE_NAME_FALLBACK。
pub fn normalize_device_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEVICE_NAME_FALLBACK.to_string();
    }
    trimmed.chars().take(DEVICE_NAME_MAX_LEN).collect()
}

/// 系统安全随机源（getrandom，Windows 上为 BCrypt/CryptGenRandom）生成 32 字节密钥，返回小写十六进制。
/// 不用时间戳、伪随机数、MAC 地址或 ZeroTier IP。
pub fn generate_device_secret() -> String {
    let mut buf = [0u8; SECRET_LEN];
    getrandom::getrandom(&mut buf).expect("系统安全随机源不可用");
    hex_encode(&buf)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, AppError> {
    if s.len() % 2 != 0 {
        return Err(AppError::new("十六进制编码长度为奇数"));
    }
    let val = |c: u8| -> Result<u8, AppError> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(AppError::new("十六进制编码含非十六进制字符")),
        }
    };
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.as_bytes().chunks(2) {
        out.push((val(pair[0])?) << 4 | val(pair[1])?);
    }
    Ok(out)
}

/// 身份语义校验：device_id 可解析为 UUID；device_secret 解码后长度必须为 SECRET_LEN；device_name 非空。
pub fn validate_identity(id: &LocalIdentity) -> Result<(), AppError> {
    uuid::Uuid::parse_str(&id.device_id)
        .map_err(|_| AppError::new("identity.device_id 不是有效 UUID"))?;
    let secret = hex_decode(&id.device_secret)?;
    if secret.len() != SECRET_LEN {
        return Err(AppError::new(format!(
            "identity.device_secret 解码后应为 {SECRET_LEN} 字节，实际 {}",
            secret.len()
        )));
    }
    if id.device_name.trim().is_empty() {
        return Err(AppError::new("identity.device_name 不能为空"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_uuid_is_v4() {
        let u = uuid::Uuid::new_v4();
        assert_eq!(u.as_bytes()[6] >> 4, 4);
        assert!(uuid::Uuid::parse_str(&u.to_string()).is_ok());
    }

    // device_secret 解码后长度 32 字节、十六进制格式，两次生成不同
    #[test]
    fn secret_is_32_byte_hex() {
        let s = generate_device_secret();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hex_decode(&s).unwrap().len(), SECRET_LEN);
        assert_ne!(generate_device_secret(), s);
    }

    #[test]
    fn hex_roundtrip() {
        let b = [0u8, 1, 0xab, 0xff, 128];
        assert_eq!(hex_decode(&hex_encode(&b)).unwrap(), b.to_vec());
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
    }

    // 设备名：空/全空白回退；去空白；超长截断且不为空
    #[test]
    fn device_name_fallback_and_truncation() {
        assert_eq!(normalize_device_name(""), DEVICE_NAME_FALLBACK);
        assert_eq!(normalize_device_name("   "), DEVICE_NAME_FALLBACK);
        assert_eq!(normalize_device_name("  DESKTOP-A  "), "DESKTOP-A");
        let long: String = std::iter::repeat('N').take(100).collect();
        let n = normalize_device_name(&long);
        assert_eq!(n.chars().count(), DEVICE_NAME_MAX_LEN);
        assert!(!n.is_empty());
    }

    // 本机读取计算机名：非空（真实计算机名或回退名）
    #[test]
    fn device_name_read_non_empty() {
        assert!(!read_device_name().trim().is_empty());
    }

    #[test]
    fn validate_rejects_bad_identity() {
        let id = new_identity();
        assert!(validate_identity(&id).is_ok());

        let mut bad_id = id.clone();
        bad_id.device_id = "not-a-uuid".into();
        assert!(validate_identity(&bad_id).is_err());

        let mut bad_secret = id.clone();
        bad_secret.device_secret = "ff".into();
        assert!(validate_identity(&bad_secret).is_err());

        let mut bad_secret2 = id.clone();
        bad_secret2.device_secret = "zz".into();
        assert!(validate_identity(&bad_secret2).is_err());

        let mut bad_name = id.clone();
        bad_name.device_name = "   ".into();
        assert!(validate_identity(&bad_name).is_err());
    }
}
