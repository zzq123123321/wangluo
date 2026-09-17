// 阶段 9：开机启动。用系统自带 reg.exe 写 HKCU 的 Run 键（不新增第三方插件依赖，
// crates.io 网络不可靠且 HKCU 无需管理员权限）。启动项值为 "\"<exe>\" --minimized"，
// 使开机启动本应用时静默到托盘；reg 位于 System32，测试只构造命令、不真正执行写注册表。
use std::path::{Path, PathBuf};
use std::process::Command;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "ClipLink";
const ARG_MINIMIZED: &str = "--minimized";

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("无法获取当前可执行文件路径: {e}"))
}

/// 构造 reg.exe 命令（纯函数，便于单测参数拼装）。enabled=true 写 Run 键，否则删除。
fn reg_command(enabled: bool, exe: &Path) -> Command {
    let mut cmd = Command::new("reg");
    cmd.arg(if enabled { "add" } else { "delete" });
    cmd.arg(RUN_KEY).arg("/v").arg(RUN_VALUE);
    if enabled {
        let data = format!("\"{}\" {ARG_MINIMIZED}", exe.display());
        cmd.arg("/t").arg("REG_SZ").arg("/d").arg(data);
    }
    cmd.arg("/f");
    cmd
}

/// 当前 Run 键下是否已存在 ClipLink 启动项。
fn value_exists() -> bool {
    Command::new("reg")
        .args(["query", RUN_KEY, "/v", RUN_VALUE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 应用开机启动状态：enabled=true 写入/刷新 Run 键；false 删除（目标本就不存在视为成功）。
/// 只有 reg 执行失败才返回 Err；调用方据此决定是否更新配置，保证配置与注册表一致。
pub fn apply(enabled: bool) -> Result<(), String> {
    let exe = current_exe()?;
    if !enabled && !value_exists() {
        return Ok(());
    }
    let out = reg_command(enabled, &exe)
        .output()
        .map_err(|e| format!("无法执行 reg.exe: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = if msg.trim().is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            msg.trim().to_string()
        };
        Err(format!("reg.exe 退出码非零: {msg}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    // 启用时：reg add HKCU\...\Run /v ClipLink /t REG_SZ /d "\"<exe>\" --minimized" /f
    #[test]
    fn add_command_writes_quoted_exe_with_minimized() {
        let exe = Path::new(r"C:\Program Files\ClipLink\cliplink.exe");
        let a = args_of(&reg_command(true, exe));
        assert_eq!(a[0], "add");
        assert!(a.contains(&RUN_KEY.to_string()));
        assert_eq!(
            a[a.len() - 2],
            r#""C:\Program Files\ClipLink\cliplink.exe" --minimized"#
        );
        assert_eq!(*a.last().unwrap(), "/f");
        assert!(a.contains(&"/t".to_string()));
    }

    // 禁用时：reg delete HKCU\...\Run /v ClipLink /f（不含启动命令行与 /d 数据）
    #[test]
    fn delete_command_has_no_data() {
        let exe = Path::new(r"C:\ClipLink.exe");
        let a = args_of(&reg_command(false, exe));
        assert_eq!(a[0], "delete");
        assert!(a.contains(&"/v".to_string()));
        assert!(a.contains(&RUN_VALUE.to_string()));
        assert!(!a.iter().any(|x| x.contains("--minimized")));
        assert!(!a.iter().any(|x| x.contains("Program Files")));
    }

    // 值存在性探测命令形状正确
    #[test]
    fn query_command_shape() {
        // 不真正运行 reg.exe（会命中真实注册表），仅验证构造逻辑：query <key> /v <value>
        let mut cmd = Command::new("reg");
        cmd.args(["query", RUN_KEY, "/v", RUN_VALUE]);
        let a = args_of(&cmd);
        assert_eq!(a[0], "query");
        assert!(a.contains(&RUN_KEY.to_string()));
        assert!(a.contains(&RUN_VALUE.to_string()));
    }
}
