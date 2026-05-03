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
    let cookie_params = cookiecloud::cookies_for_sites(cookies, &config.sites)?;
    if cookie_params.is_empty() {
        let message = "CookieCloud 未匹配到当前站点的 Cookie".to_string();
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

    let imported_cookies = match CdpClient::new(active_port).set_cookies(&cookie_params).await {
        Ok(count) => count,
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
    let result = CookieCloudSyncResult {
        matched_cookies: cookie_params.len(),
        imported_cookies,
    };
    push_log(
        &state.logs,
        LogEntry::success(format!(
            "CookieCloud 同步完成：匹配 {} 个，写入 {} 个；{}",
            result.matched_cookies,
            result.imported_cookies,
            cookie_summary(&cookie_params)
        )),
    )
    .await;
    Ok(result)
}

fn cookie_summary(cookies: &[crate::cdp::CdpCookieParam]) -> String {
    let mut grouped = std::collections::BTreeMap::<String, Vec<String>>::new();
    for cookie in cookies {
        // 只记录 Cookie 名称和域名，避免把敏感的 Cookie 值写进日志。
        let domain = cookie
            .domain
            .as_deref()
            .unwrap_or_else(|| cookie.url.as_str())
            .trim_start_matches('.')
            .to_string();
        grouped
            .entry(domain)
            .or_default()
            .push(cookie.name.clone());
    }

    let details = grouped
        .into_iter()
        .map(|(domain, mut names)| {
            names.sort();
            names.dedup();
            format!("{}：{}", domain, names.join(", "))
        })
        .collect::<Vec<_>>()
        .join("；");
    format!("详情：{}", details)
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
