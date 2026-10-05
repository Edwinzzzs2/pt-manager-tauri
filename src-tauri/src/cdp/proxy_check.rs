use super::{dedicated_profile_dir, recovery_profile_dir, BrowserKind, CdpWebSocket};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
#[cfg(not(target_os = "linux"))]
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
#[cfg(not(target_os = "linux"))]
use std::time::Instant;

#[derive(Clone, Deserialize)]
struct ProxySettings {
    server: Option<String>,
    forced_direct: bool,
    other_proxy: bool,
    user_data_dir: Option<String>,
}

static SETTINGS: OnceLock<Mutex<HashMap<String, ProxySettings>>> = OnceLock::new();

/// 不启用浏览器的自动化提示条，也要确认已有浏览器仍使用本次任务要求的代理。
pub(super) fn matches(
    version: &serde_json::Value,
    expected: Option<&str>,
    timeout: Duration,
) -> Result<bool, String> {
    let settings = process_settings(version, timeout)?;
    Ok(settings.server.as_deref() == expected
        && !settings.other_proxy
        && (expected.is_none() || !settings.forced_direct))
}

/// 只允许收尾流程关闭使用 PT Manager 专用 Profile 的浏览器。
pub(super) fn uses_managed_profile(
    version: &serde_json::Value,
    browser: BrowserKind,
    timeout: Duration,
) -> Result<bool, String> {
    let settings = process_settings(version, timeout)?;
    let Some(actual) = settings.user_data_dir.as_deref() else {
        return Ok(false);
    };
    let actual =
        std::fs::canonicalize(actual).map_err(|_| "无法确认浏览器的 Profile 路径".to_string())?;
    Ok([
        dedicated_profile_dir(browser),
        recovery_profile_dir(browser),
    ]
    .iter()
    .filter_map(|path| std::fs::canonicalize(path).ok())
    .any(|expected| same_profile_path(&actual, &expected)))
}

fn same_profile_path(actual: &Path, expected: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        actual
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy())
    }
    #[cfg(not(target_os = "windows"))]
    {
        actual == expected
    }
}

fn process_settings(
    version: &serde_json::Value,
    timeout: Duration,
) -> Result<ProxySettings, String> {
    let url = version
        .get("webSocketDebuggerUrl")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "浏览器没有提供调试连接".to_string())?;
    let cache = SETTINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache
        .lock()
        .map_err(|_| "浏览器代理检查状态不可用".to_string())?
        .get(url)
        .cloned();
    let settings = if let Some(settings) = cached {
        settings
    } else {
        let response = CdpWebSocket::connect(url, timeout)?
            .call("SystemInfo.getProcessInfo", serde_json::json!({}))?;
        // 从实际 CDP 浏览器获取 PID，避免把同名的普通浏览器或渲染进程当作保活浏览器。
        let process_id = response
            .get("result")
            .and_then(|result| result.get("processInfo"))
            .and_then(|processes| processes.as_array())
            .and_then(|processes| {
                processes.iter().find(|process| {
                    process.get("type").and_then(|value| value.as_str()) == Some("browser")
                })
            })
            .and_then(|process| process.get("id"))
            .and_then(|id| id.as_f64())
            .filter(|id| *id > 0.0 && *id <= u32::MAX as f64 && id.fract() == 0.0)
            .ok_or_else(|| "无法确认浏览器主进程，请关闭专用浏览器后重试".to_string())?
            as u32;
        let settings = read_process_settings(process_id)?;
        let mut cache = cache
            .lock()
            .map_err(|_| "浏览器代理检查状态不可用".to_string())?;
        // 启动参数在进程存活期间不变。按浏览器 WebSocket 的唯一标识缓存，重启后自动重查。
        if cache.len() >= 32 {
            cache.clear();
        }
        cache.insert(url.to_string(), settings.clone());
        settings
    };
    Ok(settings)
}

#[cfg(target_os = "windows")]
fn read_process_settings(process_id: u32) -> Result<ProxySettings, String> {
    use std::os::windows::process::CommandExt;

    // 只传入 CDP 返回并验证过的数字 PID；不执行浏览器参数，也不输出网址或账号信息。
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$browserProcess = Get-CimInstance Win32_Process -Filter 'ProcessId = {process_id}'
$browserArguments = $browserProcess.CommandLine
if ([string]::IsNullOrWhiteSpace($browserArguments)) {{ throw 'Browser command line unavailable' }}
$server = $null
$serverMatch = [regex]::Match($browserArguments, '(?:^|\s)"?--proxy-server=(?:"([^"\r\n]*)"|([^\s"]+))')
if ($serverMatch.Success) {{
    $server = if ($serverMatch.Groups[1].Success) {{ $serverMatch.Groups[1].Value }} else {{ $serverMatch.Groups[2].Value }}
}}
$profile = $null
$profileMatch = [regex]::Match($browserArguments, '(?:^|\s)(?:"--user-data-dir=([^"\r\n]+)"|--user-data-dir=(?:"([^"\r\n]+)"|([^\s"]+)))')
if ($profileMatch.Success) {{
    $profile = @($profileMatch.Groups[1].Value, $profileMatch.Groups[2].Value, $profileMatch.Groups[3].Value) | Where-Object {{ $_ }} | Select-Object -First 1
}}
[ordered]@{{
    server = $server
    forced_direct = [regex]::IsMatch($browserArguments, '(?:^|\s)"?--no-proxy-server(?:"?(?:\s|$))')
    other_proxy = [regex]::IsMatch($browserArguments, '(?:^|\s)"?--proxy-(?:pac-url|auto-detect)(?:=|"?(?:\s|$))')
    user_data_dir = $profile
}} | ConvertTo-Json -Compress"#
    );
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .creation_flags(0x08000000); // CREATE_NO_WINDOW：查询进程时不弹出额外的终端窗口。
    let output = query_process(&mut command)?;
    serde_json::from_slice(&output)
        .map_err(|_| "读取浏览器代理启动参数失败，请关闭专用浏览器后重试".to_string())
}

#[cfg(not(target_os = "windows"))]
fn read_process_settings(process_id: u32) -> Result<ProxySettings, String> {
    #[cfg(target_os = "linux")]
    let arguments = std::fs::read(format!("/proc/{process_id}/cmdline"))
        .map_err(|_| "无法读取浏览器进程参数".to_string())?;
    #[cfg(not(target_os = "linux"))]
    let arguments = query_process(Command::new("/bin/ps").args([
        "-ww",
        "-p",
        &process_id.to_string(),
        "-o",
        "command=",
    ]))?;
    let line = String::from_utf8(arguments).map_err(|_| "浏览器进程参数编码无效".to_string())?;
    #[cfg(target_os = "linux")]
    let arguments: Vec<&str> = line.split('\0').collect();
    #[cfg(not(target_os = "linux"))]
    let arguments: Vec<&str> = line
        .split_whitespace()
        .map(|value| value.trim_matches('"'))
        .collect();
    Ok(ProxySettings {
        server: arguments
            .iter()
            .find_map(|value| value.strip_prefix("--proxy-server=").map(str::to_string)),
        forced_direct: arguments.contains(&"--no-proxy-server"),
        other_proxy: arguments
            .iter()
            .any(|value| value.starts_with("--proxy-pac-url=") || *value == "--proxy-auto-detect"),
        user_data_dir: arguments
            .iter()
            .find_map(|value| value.strip_prefix("--user-data-dir=").map(str::to_string)),
    })
}

/// 查询进程参数设置等待上限，避免系统查询卡住后阻塞保活；错误日志不包含完整命令行。
#[cfg(not(target_os = "linux"))]
fn query_process(command: &mut Command) -> Result<Vec<u8>, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "无法启动浏览器进程查询".to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|_| "读取浏览器进程查询结果失败".to_string())?;
                return if output.status.success() {
                    Ok(output.stdout)
                } else {
                    Err("无法读取浏览器代理参数，请关闭专用浏览器后重试".to_string())
                };
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(if result.is_err() {
                    "浏览器进程查询失败".to_string()
                } else {
                    "浏览器进程查询超时，请稍后重试".to_string()
                });
            }
        }
    }
}
