use crate::cdp::{self, CdpClient, CdpProgress};
use crate::cookiecloud;
use crate::scheduler;
use crate::store::{self, AppConfig, LogEntry};
use chrono::{DateTime, Local};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::process::Command;
use tauri::State;
use tauri_plugin_autostart::ManagerExt;
use tokio::sync::Mutex;

/// 应用全局状态
pub struct AppState {
    pub config: Arc<Mutex<AppConfig>>,
    pub logs: Arc<Mutex<Vec<LogEntry>>>,
    pub scheduler: Arc<Mutex<scheduler::Scheduler>>,
    pub task_running: Arc<Mutex<bool>>,
    pub task_cancel_requested: Arc<AtomicBool>,
    pub app_handle: tauri::AppHandle,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppStatus {
    pub cdp_connected: bool,
    pub chrome_installed: bool,
    pub active_cdp_port: Option<u16>,
    pub next_run: Option<DateTime<Local>>,
    pub last_result: Option<LogEntry>,
    pub is_running: bool,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CookieCloudSyncResult {
    pub matched_cookies: usize,
    pub imported_cookies: usize,
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    let config = state.config.lock().await;
    Ok(config.clone())
}

#[tauri::command]
pub async fn save_config(state: State<'_, AppState>, mut config: AppConfig) -> Result<(), String> {
    config.log_retention = store::normalize_log_retention(config.log_retention);
    let auto_launch_changed = state.config.lock().await.auto_launch != config.auto_launch;
    if auto_launch_changed {
        apply_auto_launch(&state.app_handle, config.auto_launch)?;
    }
    store::set_log_retention(config.log_retention);
    {
        let mut current = state.config.lock().await;
        *current = config.clone();
    }
    store::save_config(&state.app_handle, &config);
    store::apply_log_retention(&state.logs).await;
    restart_scheduler(&state, config).await;
    Ok(())
}

#[tauri::command]
pub async fn add_site(
    state: State<'_, AppState>,
    name: String,
    url: String,
) -> Result<AppConfig, String> {
    let mut config = state.config.lock().await;
    let site = store::Site {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        url,
    };
    config.sites.push(site);
    store::save_config(&state.app_handle, &config);
    let next = config.clone();
    drop(config);
    restart_scheduler(&state, next.clone()).await;
    Ok(next)
}

#[tauri::command]
pub async fn remove_site(state: State<'_, AppState>, id: String) -> Result<AppConfig, String> {
    let mut config = state.config.lock().await;
    config.sites.retain(|s| s.id != id);
    store::save_config(&state.app_handle, &config);
    let next = config.clone();
    drop(config);
    restart_scheduler(&state, next.clone()).await;
    Ok(next)
}

#[tauri::command]
pub async fn update_site(
    state: State<'_, AppState>,
    id: String,
    name: String,
    url: String,
) -> Result<AppConfig, String> {
    let mut config = state.config.lock().await;
    if let Some(site) = config.sites.iter_mut().find(|s| s.id == id) {
        site.name = name;
        site.url = url;
    }
    store::save_config(&state.app_handle, &config);
    let next = config.clone();
    drop(config);
    restart_scheduler(&state, next.clone()).await;
    Ok(next)
}

#[tauri::command]
pub async fn check_cdp(state: State<'_, AppState>) -> Result<bool, String> {
    let cdp_port = state.config.lock().await.cdp_port;
    let cdp = CdpClient::new(cdp_port);
    Ok(cdp.available_port().await.is_some())
}

#[tauri::command]
pub async fn ensure_cdp(state: State<'_, AppState>) -> Result<bool, String> {
    let (cdp_port, initial_urls) = {
        let config = state.config.lock().await;
        let urls = config
            .sites
            .iter()
            .map(|site| site.url.clone())
            .collect::<Vec<_>>();
        (config.cdp_port, urls)
    };
    let cdp = CdpClient::new(cdp_port);
    let progress = CdpProgress::new(
        Arc::clone(&state.logs),
        Arc::clone(&state.task_cancel_requested),
    );
    let result = cdp
        .ensure_available_with_progress(&initial_urls, &progress)
        .await?;
    push_log(&state.logs, LogEntry::info(result.message)).await;
    Ok(true)
}

#[tauri::command]
pub async fn sync_cookiecloud_cookies(
    state: State<'_, AppState>,
    cookies: serde_json::Value,
) -> Result<CookieCloudSyncResult, String> {
    let config = { state.config.lock().await.clone() };
    import_cookiecloud_cookies(&state, &config, cookies).await
}

#[tauri::command]
pub async fn sync_cookiecloud_from_config(
    state: State<'_, AppState>,
    config: AppConfig,
) -> Result<CookieCloudSyncResult, String> {
    let cookiecloud_config = config.cookiecloud.clone();
    let cookies = tauri::async_runtime::spawn_blocking(move || {
        cookiecloud::fetch_cookie_data(&cookiecloud_config)
    })
    .await
    .map_err(|err| err.to_string())??;

    import_cookiecloud_cookies(&state, &config, cookies).await
}

async fn import_cookiecloud_cookies(
    state: &State<'_, AppState>,
    config: &AppConfig,
    cookies: serde_json::Value,
) -> Result<CookieCloudSyncResult, String> {
    let cookie_params = cookiecloud::cookies_from_cookiecloud(cookies, &config.sites)?;
    if cookie_params.is_empty() {
        let message = "CookieCloud 未解析到可同步的 Cookie".to_string();
        push_log(&state.logs, LogEntry::error(message.clone())).await;
        return Err(message);
    }

    let cdp = CdpClient::new(config.cdp_port);
    let active_port = match cdp.available_port().await {
        Some(port) => port,
        None => {
            let progress = CdpProgress::new(
                Arc::clone(&state.logs),
                Arc::clone(&state.task_cancel_requested),
            );
            let result = cdp.ensure_available_with_progress(&[], &progress).await?;
            push_log(&state.logs, LogEntry::info(result.message)).await;
            result.port
        }
    };

    let imported_cookie_params = match CdpClient::new(active_port).set_cookies(&cookie_params).await {
        Ok(cookies) => cookies,
        Err(err) if is_connection_refused(&err) => {
            push_log(
                &state.logs,
                LogEntry::info("Cookie 已拉取，但 Chrome CDP 连接失效，正在重新准备后重试"),
            )
            .await;
            let progress = CdpProgress::new(
                Arc::clone(&state.logs),
                Arc::clone(&state.task_cancel_requested),
            );
            let result = cdp.ensure_available_with_progress(&[], &progress).await?;
            CdpClient::new(result.port)
                .set_cookies(&cookie_params)
                .await
                .map_err(|retry_err| format!("Cookie 已拉取，但写入 Chrome 失败：{}", retry_err))?
        }
        Err(err) => {
            return Err(format!("Cookie 已拉取，但写入 Chrome 失败：{}", err));
        }
    };

    // Cookie 写入成功后，刷新 Chrome 中已打开的站点页面，使新 Cookie 立即生效。
    let site_urls: Vec<String> = config.sites.iter().map(|s| s.url.clone()).collect();
    CdpClient::new(active_port)
        .reload_tabs_for_sites(&site_urls)
        .await;

    let result = CookieCloudSyncResult {
        matched_cookies: cookie_params.len(),
        imported_cookies: imported_cookie_params.len(),
    };
    // 日志里的站点数按成功写入的 Cookie 计算，避免把 CDP 拒绝写入的 Cookie 也报成可用。
    let site_match = cookie_site_match_summary(&config.sites, &imported_cookie_params);
    let (_, detail) = cookie_summary(&imported_cookie_params);
    push_log(
        &state.logs,
        LogEntry::success(format!(
            "CookieCloud 同步完成：匹配 {} 个站点，共解析 {} 条 Cookie，成功写入 {} 条；{}；{}",
            site_match.matched_count,
            result.matched_cookies,
            result.imported_cookies,
            site_match.detail,
            detail
        )),
    )
    .await;
    Ok(result)
}

struct CookieSiteMatchSummary {
    matched_count: usize,
    detail: String,
}

fn cookie_site_match_summary(
    sites: &[crate::store::Site],
    cookies: &[crate::cdp::CdpCookieParam],
) -> CookieSiteMatchSummary {
    let matched = sites
        .iter()
        .filter(|site| site_has_cookie_match(site, cookies))
        .map(|site| site.name.clone())
        .collect::<Vec<_>>();
    let unmatched = sites
        .iter()
        .filter(|site| !site_has_cookie_match(site, cookies))
        .map(|site| site.name.clone())
        .collect::<Vec<_>>();

    let mut parts = Vec::new();
    if !matched.is_empty() {
        parts.push(format!("匹配站点：{}", matched.join(", ")));
    }
    if !unmatched.is_empty() {
        parts.push(format!("未匹配站点：{}", unmatched.join(", ")));
    }

    CookieSiteMatchSummary {
        matched_count: matched.len(),
        detail: parts.join("；"),
    }
}

fn site_has_cookie_match(site: &crate::store::Site, cookies: &[crate::cdp::CdpCookieParam]) -> bool {
    let Some(site_host) = host_from_url(&site.url) else {
        return false;
    };

    cookies.iter().any(|cookie| {
        let Some(cookie_host) = cookie_host(cookie) else {
            return false;
        };
        site_host == cookie_host
            || site_host.ends_with(&format!(".{}", cookie_host))
            || cookie_host.ends_with(&format!(".{}", site_host))
    })
}

fn cookie_host(cookie: &crate::cdp::CdpCookieParam) -> Option<String> {
    let raw = cookie
        .domain
        .as_deref()
        .unwrap_or_else(|| cookie.url.as_str());
    normalize_host(raw)
}

fn host_from_url(url: &str) -> Option<String> {
    let without_scheme = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    normalize_host(without_scheme)
}

fn normalize_host(value: &str) -> Option<String> {
    let host = value
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches('.')
        .split(['/', ':', '?', '#'])
        .next()?
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// 返回 (站点域名数, 详情文本)。按纯 host 分组（去掉 scheme），避免同域的 http/https Cookie 分成两条。
fn cookie_summary(cookies: &[crate::cdp::CdpCookieParam]) -> (usize, String) {
    let mut grouped = std::collections::BTreeMap::<String, Vec<String>>::new();
    for cookie in cookies {
        // 优先用 domain 字段；host_only Cookie 没有 domain，用 url 但只取 host 部分。
        let raw = cookie
            .domain
            .as_deref()
            .unwrap_or_else(|| cookie.url.as_str());
        // 去掉 scheme（http:// / https://）、前缀点、端口和路径，只保留纯 host。
        let host = raw
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches('.')
            .split(['/', ':', '?', '#'])
            .next()
            .unwrap_or(raw)
            .to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }
        grouped
            .entry(host)
            .or_default()
            .push(cookie.name.clone());
    }

    let site_count = grouped.len();
    let details = grouped
        .into_iter()
        .map(|(domain, mut names)| {
            names.sort();
            names.dedup();
            format!("{}：{}", domain, names.join(", "))
        })
        .collect::<Vec<_>>()
        .join("；");
    let detail = if details.is_empty() {
        "导入 Cookie：无".to_string()
    } else {
        format!("导入 Cookie：{}", details)
    };
    (site_count, detail)
}

fn is_connection_refused(message: &str) -> bool {
    message.contains("10061")
        || message.contains("积极拒绝")
        || message.to_ascii_lowercase().contains("connection refused")
}

#[tauri::command]
pub async fn get_status(state: State<'_, AppState>) -> Result<AppStatus, String> {
    let config = state.config.lock().await.clone();
    let cdp = CdpClient::new(config.cdp_port);
    let active_cdp_port = cdp.available_port().await;
    let cdp_connected = active_cdp_port.is_some();
    let chrome_installed = cdp::chrome_installed();
    let next_run = state.scheduler.lock().await.next_run().await;
    let is_running = *state.task_running.lock().await;
    let cancel_requested = state.task_cancel_requested.load(Ordering::SeqCst);
    let last_result = state.logs.lock().await.iter().last().cloned();

    Ok(AppStatus {
        cdp_connected,
        chrome_installed,
        active_cdp_port,
        next_run,
        last_result,
        is_running,
        cancel_requested,
    })
}

#[tauri::command]
pub async fn run_task(state: State<'_, AppState>) -> Result<(), String> {
    let config = { state.config.lock().await.clone() };
    let logs = Arc::clone(&state.logs);
    let task_running = Arc::clone(&state.task_running);
    let task_cancel_requested = Arc::clone(&state.task_cancel_requested);
    tauri::async_runtime::spawn(async move {
        scheduler::run_with_flag(&config, &logs, &task_running, &task_cancel_requested, false)
            .await;
    });
    Ok(())
}

#[tauri::command]
pub async fn stop_task(state: State<'_, AppState>) -> Result<(), String> {
    let is_running = *state.task_running.lock().await;
    if !is_running {
        push_log(&state.logs, LogEntry::info("当前没有正在执行的保活任务")).await;
        return Ok(());
    }

    state.task_cancel_requested.store(true, Ordering::SeqCst);
    push_log(
        &state.logs,
        LogEntry::info("已请求终止保活任务，正在等待当前步骤收尾"),
    )
    .await;
    Ok(())
}

#[tauri::command]
pub async fn get_logs(state: State<'_, AppState>) -> Result<Vec<LogEntry>, String> {
    let logs = state.logs.lock().await;
    Ok(logs.clone())
}

#[tauri::command]
pub async fn clear_logs(state: State<'_, AppState>) -> Result<(), String> {
    let mut logs = state.logs.lock().await;
    logs.clear();
    store::clear_log_file();
    Ok(())
}

#[tauri::command]
pub async fn open_chrome_download() -> Result<(), String> {
    open_url("https://www.google.com/chrome/").map_err(|err| err.to_string())
}

async fn restart_scheduler(state: &State<'_, AppState>, config: AppConfig) {
    state.scheduler.lock().await.start(
        config,
        Arc::clone(&state.logs),
        Arc::clone(&state.task_running),
        Arc::clone(&state.task_cancel_requested),
    );
}

async fn push_log(logs: &Arc<Mutex<Vec<LogEntry>>>, entry: LogEntry) {
    store::push_log(logs, entry).await;
}

pub fn apply_auto_launch(app_handle: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let manager = app_handle.autolaunch();
    if manager.is_enabled().map_err(|err| err.to_string()).ok() == Some(enabled) {
        return Ok(());
    }

    if enabled {
        manager.enable().map_err(|err| err.to_string())
    } else {
        match manager.disable() {
            Ok(()) => Ok(()),
            Err(err) => {
                let message = err.to_string();
                if is_auto_launch_entry_missing(&message) {
                    Ok(())
                } else {
                    Err(message)
                }
            }
        }
    }
}

fn is_auto_launch_entry_missing(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("os error 2")
        || lower.contains("not found")
        || message.contains("找不到指定的文件")
}

fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        return Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map(|_| ());
    }

    #[cfg(target_os = "macos")]
    {
        return Command::new("open").arg(url).spawn().map(|_| ());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return Command::new("xdg-open").arg(url).spawn().map(|_| ());
    }

    #[allow(unreachable_code)]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前系统不支持自动打开 Chrome 下载页",
    ))
}
