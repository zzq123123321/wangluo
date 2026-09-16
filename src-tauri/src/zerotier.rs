// 阶段 3：枚举 Windows 网卡（Win32 GetAdaptersAddresses），筛选 ZeroTier 适配器，读取有效 IPv4。
// 纯选择逻辑（is_zt_adapter / is_valid_ipv4 / select_ip / ip_changed）与 Win32 调用解耦，便于单元测试注入模拟数据。
use crate::error::AppError;
use crate::state::AppState;
use serde::Serialize;
use std::ptr;
use tauri::{AppHandle, Emitter, Manager};
use winapi::um::iphlpapi::GetAdaptersAddresses;
use winapi::um::iptypes::{IP_ADAPTER_ADDRESSES, IP_ADAPTER_UNICAST_ADDRESS_LH};

pub const ZT_IP_CHANGED_EVENT: &str = "zerotier-ip-changed";
pub const POLL_INTERVAL_SECS: u64 = 10;

pub const HINT_DETECTING: &str = "正在检测 ZeroTier 网络……";
pub const HINT_FOUND: &str = "ZeroTier 已连接";
pub const HINT_NOT_FOUND: &str =
    "未发现 ZeroTier IP，请确认 ZeroTier 已启动，并且本机已经加入并获准访问网络。";

pub const STATUS_WAITING_INPUT: &str = "等待输入对方 IP。";
pub const STATUS_NO_ZT: &str = "未发现 ZeroTier IP。";

/// 检测结果：能区分"未发现网卡"与"有网卡但没有有效 IPv4"，
/// 但 ZeroTier 未启动 / 未入网 / 未授权在 GetAdaptersAddresses 层面无法可靠区分，统一归入前两者。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZtResult {
    Found,
    NoAdapter,
    NoValidIpv4,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZtIpEvent {
    pub ip: Option<String>,
}

/// 单个网络适配器（显示名/描述 + 候选 IPv4，网络字节序）
pub struct ZtAdapter {
    pub name: String,
    pub description: String,
    pub ipv4s: Vec<u32>,
}

pub fn is_zt_adapter(a: &ZtAdapter) -> bool {
    a.description.to_ascii_lowercase().contains("zerotier")
        || a.name.to_ascii_lowercase().contains("zerotier")
}

/// 有效 IPv4（网络字节序）：排除 0.0.0.0、回环 127/8、链路本地 169.254/16、广播、组播 224/4 与保留段 240/4。
pub fn is_valid_ipv4(ip: u32) -> bool {
    if ip == 0 || ip == 0xFFFF_FFFF {
        return false;
    }
    let a = (ip >> 24) & 0xFF;
    if a == 127 {
        return false;
    }
    if a == 169 && ((ip >> 16) & 0xFF) == 254 {
        return false;
    }
    if (224..=255).contains(&a) {
        return false;
    }
    true
}

/// 稳定选择规则：取所有 ZeroTier 适配器的有效 IPv4 中数值最小者（去重、升序），
/// 保证多次轮询结果确定，不会在多个候选间跳变。
pub fn select_ip(adapters: &[ZtAdapter]) -> Option<String> {
    let mut cands: Vec<u32> = adapters
        .iter()
        .filter(|a| is_zt_adapter(a))
        .flat_map(|a| a.ipv4s.iter().copied())
        .filter(|&ip| is_valid_ipv4(ip))
        .collect();
    cands.sort_unstable();
    cands.dedup();
    cands.first().copied().map(ip_to_string)
}

pub fn ip_to_string(ip: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (ip >> 24) & 0xFF,
        (ip >> 16) & 0xFF,
        (ip >> 8) & 0xFF,
        ip & 0xFF
    )
}

fn ip_changed(old: &Option<String>, new: &Option<String>) -> bool {
    old != new
}

fn read_wstr(mut p: *const u16) -> String {
    let mut v = Vec::new();
    unsafe {
        while !p.is_null() && *p != 0 {
            v.push(*p);
            p = p.add(1);
        }
    }
    String::from_utf16_lossy(&v)
}

/// 枚举全部网络适配器及其 IPv4 候选地址。系统 API 失败时返回 AppError。
fn enumerate_adapters() -> Result<Vec<ZtAdapter>, AppError> {
    unsafe {
        // 第一次调用只查询所需缓冲区大小，返回 111/12065 均属正常
        let mut buf_len: u32 = 0;
        let _ = GetAdaptersAddresses(0, 0, ptr::null_mut(), ptr::null_mut(), &mut buf_len);
        let mut buf = vec![0u8; buf_len as usize];
        let rc = GetAdaptersAddresses(
            0,
            0,
            ptr::null_mut(),
            buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES,
            &mut buf_len,
        );
        if rc != 0 {
            return Err(AppError::new(format!(
                "枚举网络适配器失败（GetAdaptersAddresses 错误码 {rc}）"
            )));
        }
        let mut out = Vec::new();
        let mut a = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES;
        while !a.is_null() {
            let d = &*a;
            let name = read_wstr(d.FriendlyName);
            let description = read_wstr(d.Description);
            let mut ipv4s = Vec::new();
            let mut u: *mut IP_ADAPTER_UNICAST_ADDRESS_LH = d.FirstUnicastAddress;
            while !u.is_null() {
                let un = &*u;
                // sa_family == AF_INET(2)；IPv4 地址取 sockaddr 的 sa_data[2..6]（网络字节序）
                if let Some(sa) = un.Address.lpSockaddr.as_ref() {
                    if sa.sa_family == 2 {
                        let b = &sa.sa_data;
                        let ip = ((b[2] as u8) as u32) << 24
                            | ((b[3] as u8) as u32) << 16
                            | ((b[4] as u8) as u32) << 8
                            | (b[5] as u8) as u32;
                        ipv4s.push(ip);
                    }
                }
                u = un.Next;
            }
            out.push(ZtAdapter {
                name,
                description,
                ipv4s,
            });
            a = d.Next;
        }
        Ok(out)
    }
}

/// 执行一次检测：Win32 调用在锁外完成，仅比对/更新时短暂持锁，事件在锁外发送。
/// 仅当结果与 AppState 中现有值不同时更新并发送 `zerotier-ip-changed` 事件。
/// AppState 未就绪时（首个轮询任务可能先于 app.manage 被调度）跳过本轮，不 panic。
pub fn detect_and_notify(app: &AppHandle) -> Result<ZtResult, AppError> {
    let adapters = enumerate_adapters()?;
    let new_ip = select_ip(&adapters);
    let result = if new_ip.is_some() {
        ZtResult::Found
    } else if adapters.iter().any(is_zt_adapter) {
        ZtResult::NoValidIpv4
    } else {
        ZtResult::NoAdapter
    };

    let Some(state) = app.try_state::<std::sync::Arc<AppState>>() else {
        tracing::debug!("AppState 尚未就绪，跳过本次 ZeroTier 检测");
        return Ok(result);
    };
    // 有活动/建立中连接时，本机 ZT IP 变化不覆盖连接状态（已建立的 TCP 通道不受影响）
    let busy = state.net.is_busy();
    let changed;
    {
        let mut g = state
            .inner
            .lock()
            .map_err(|e| AppError::new(e.to_string()))?;
        changed = ip_changed(&g.zerotier_ip, &new_ip);
        if changed {
            g.zerotier_ip = new_ip.clone();
            match &new_ip {
                Some(_) => {
                    g.zerotier_hint = HINT_FOUND.to_string();
                    g.hint_warn = false;
                }
                None => {
                    g.zerotier_hint = HINT_NOT_FOUND.to_string();
                    g.hint_warn = true;
                }
            }
            if !busy {
                g.status = crate::state::ConnectionStatus::Offline;
                g.status_text = match &new_ip {
                    Some(_) => STATUS_WAITING_INPUT.to_string(),
                    None => STATUS_NO_ZT.to_string(),
                };
            }
        }
    }
    if changed {
        app.emit(ZT_IP_CHANGED_EVENT, ZtIpEvent { ip: new_ip.clone() })
            .map_err(|e| AppError::new(e.to_string()))?;
        // 复用同一检测结果更新路径触发 listener 启停/重绑定（幂等；无新增轮询器）：
        // 从无→有 启动监听；IP 变化 停旧绑新；有→无 停止监听失效地址。
        let net = state.net.clone();
        let ip = new_ip.clone();
        tauri::async_runtime::spawn(async move {
            net.sync_listener(ip.as_deref()).await;
        });
        tracing::info!(ip = ?new_ip, "ZeroTier IP 变化，已通知前端并同步监听");
    } else {
        tracing::debug!(?result, "ZeroTier 复查：结果无变化");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> u32 {
        let mut o = [0u8; 4];
        for (i, p) in s.split('.').enumerate() {
            o[i] = p.parse().unwrap();
        }
        ((o[0] as u32) << 24) | ((o[1] as u32) << 16) | ((o[2] as u32) << 8) | o[3] as u32
    }

    fn ad(name: &str, desc: &str, ips: &[&str]) -> ZtAdapter {
        ZtAdapter {
            name: name.to_string(),
            description: desc.to_string(),
            ipv4s: ips.iter().map(|s| ip(s)).collect(),
        }
    }

    // 1. 正常 ZeroTier 网卡带一个有效 IPv4
    #[test]
    fn finds_valid_zt_ip() {
        let a = [ad("ZeroTier One", "ZeroTier One", &["192.168.191.180"])];
        assert_eq!(select_ip(&a).as_deref(), Some("192.168.191.180"));
    }

    // 2. 没有 ZeroTier 网卡
    #[test]
    fn no_zt_adapter() {
        let a = [
            ad("WLAN", "Intel Wi-Fi 6", &["192.168.1.5"]),
            ad("以太网", "Realtek GbE", &["10.0.0.2"]),
        ];
        assert_eq!(select_ip(&a), None);
    }

    // 3. 存在 ZeroTier 网卡但没有 IPv4
    #[test]
    fn zt_adapter_without_ipv4() {
        let a = [ad("ZeroTier One", "ZeroTier One", &[])];
        assert_eq!(select_ip(&a), None);
    }

    // 4. ZeroTier 网卡只有 169.254.x.x
    #[test]
    fn zt_only_link_local() {
        let a = [ad("ZeroTier One", "ZeroTier One", &["169.254.10.11"])];
        assert_eq!(select_ip(&a), None);
    }

    // 5. 只有回环或无效地址
    #[test]
    fn zt_only_invalid() {
        let a = [ad(
            "ZeroTier One",
            "ZeroTier One",
            &["127.0.0.1", "0.0.0.0", "255.255.255.255", "224.0.0.5"],
        )];
        assert_eq!(select_ip(&a), None);
    }

    // 6. 与 Wi-Fi、以太网并存，不误选其他地址
    #[test]
    fn coexists_with_other_nics() {
        let a = [
            ad("WLAN", "Intel Wi-Fi 6", &["192.168.1.5"]),
            ad("以太网", "Realtek GbE", &["10.0.0.2"]),
            ad("ZeroTier One", "ZeroTier One", &["192.168.191.180"]),
        ];
        assert_eq!(select_ip(&a).as_deref(), Some("192.168.191.180"));
    }

    // 7. 多个大小写不同的 ZeroTier 网卡都匹配
    #[test]
    fn case_insensitive_match() {
        assert!(is_zt_adapter(&ad("ZeroTier One", "ZeroTier One", &[])));
        assert!(is_zt_adapter(&ad("zt0", "zerotier virtual", &[])));
        assert!(is_zt_adapter(&ad("zt1", "Zerotier X", &[])));
        assert!(!is_zt_adapter(&ad("WLAN", "Intel Wi-Fi 6", &[])));
    }

    // 8. 同一网卡多个候选 IPv4，选择稳定（数值最小者）
    #[test]
    fn stable_multi_candidate() {
        let a = [ad(
            "ZeroTier One",
            "ZeroTier One",
            &["192.168.191.180", "10.147.17.36"],
        )];
        assert_eq!(select_ip(&a).as_deref(), Some("10.147.17.36"));
        // 顺序打乱后结果不变
        let b = [ad(
            "ZeroTier One",
            "ZeroTier One",
            &["10.147.17.36", "192.168.191.180"],
        )];
        assert_eq!(select_ip(&b), select_ip(&a));
    }

    // 9. 地址由旧值变为新值：判定为变化
    #[test]
    fn ip_old_to_new_is_change() {
        let old = Some("192.168.191.180".to_string());
        let new = Some("192.168.191.200".to_string());
        assert!(ip_changed(&old, &new));
    }

    // 10. 由有效值变为无地址：旧值被清除（判定为变化）
    #[test]
    fn ip_valid_to_none_is_change() {
        let old = Some("192.168.191.180".to_string());
        let new: Option<String> = None;
        assert!(ip_changed(&old, &new));
    }

    // 11. 相同结果重复检测：不判定为变化（不重复发事件）
    #[test]
    fn same_result_no_change() {
        let old = Some("192.168.191.180".to_string());
        let new = Some("192.168.191.180".to_string());
        assert!(!ip_changed(&old, &new));
        let none_old: Option<String> = None;
        let none_new: Option<String> = None;
        assert!(!ip_changed(&none_old, &none_new));
    }

    // 字节序：sa_data 到网络字节序 u32 的转换
    #[test]
    fn ip_to_string_roundtrip() {
        assert_eq!(ip_to_string(ip("192.168.191.180")), "192.168.191.180");
        assert_eq!(ip_to_string(ip("10.147.17.36")), "10.147.17.36");
    }
}
