use crate::auth;
use crate::cdp::{self, CdpClient, CdpLocalStorageParam, CdpProgress};
use crate::cookiecloud;
use crate::gotify;
use crate::ocr;
use crate::scheduler;
use crate::store::{self, AppConfig, LogEntry};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::process::Command;
use std::time::Duration;
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
    pub matched_local_storages: usize,
    pub imported_local_storages: usize,
}

#[derive(Debug, Deserialize)]
struct ImportedSite {
    name: String,
    url: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    totp_secret: String,
    #[serde(default)]
    auto_login: bool,
}

#[derive(Debug, Serialize)]
pub struct SiteImportResult {
    pub config: AppConfig,
    pub imported: usize,
    pub skipped: usize,
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    let config = state.config.lock().await;
    Ok(config.clone())
}

#[tauri::command]
pub async fn save_config(state: State<'_, AppState>, mut config: AppConfig) -> Result<(), String> {
    config.log_retention = store::normalize_log_retention(config.log_retention);
    config.ocr_server_url = config.ocr_server_url.trim().trim_end_matches('/').to_string();
    config.ocr_retry_count = config.ocr_retry_count.clamp(1, 5);
    config.min_login_attempts_remaining = config.min_login_attempts_remaining.clamp(1, 20);
    config.gotify.server_url = config
        .gotify
        .server_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    config.gotify.token = config.gotify.token.trim().to_string();
    if config.gotify.enabled
        && (config.gotify.server_url.is_empty() || config.gotify.token.is_empty())
    {
        return Err("启用 Gotify 通知前，请填写服务地址和应用 Token".to_string());
    }
    if (config.auto_sync_cookie || config.auto_sync_cookie_after_keepalive)
        && (config.cookiecloud.server_url.trim().is_empty()
            || config.cookiecloud.uuid.trim().is_empty()
            || config.cookiecloud.password.is_empty())
    {
        return Err("启用 CookieCloud 自动同步前，请填写服务地址、UUID 和密码".to_string());
    }
    if !config.ocr_server_url.is_empty() {
        let ocr_server_url = config.ocr_server_url.clone();
        tauri::async_runtime::spawn_blocking(move || ocr::ensure_initialized(&ocr_server_url))
            .await
            .map_err(|err| err.to_string())??;
    }
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
pub async fn test_gotify(config: crate::store::GotifyConfig) -> Result<String, String> {
    gotify::send_test(&config).await?;
    Ok("Gotify 测试通知已发送，请检查接收端".to_string())
}

#[tauri::command]
pub async fn add_site(
    state: State<'_, AppState>,
    name: String,
    url: String,
    username: String,
    password: String,
    totp_secret: String,
    auto_login: bool,
) -> Result<AppConfig, String> {
    let mut config = state.config.lock().await;
    let site = store::Site {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        url,
        username,
        password,
        totp_secret,
        auto_login,
        login_attempts_remaining: None,
        auto_keepalive: true,
    };
    config.sites.push(site);
    store::save_config(&state.app_handle, &config);
    let next = config.clone();
    drop(config);
    restart_scheduler(&state, next.clone()).await;
    Ok(next)
}

#[tauri::command]
pub async fn import_sites_from_json(
    state: State<'_, AppState>,
    path: String,
) -> Result<SiteImportResult, String> {
    let content = fs::read_to_string(PathBuf::from(path))
        .map_err(|err| format!("读取 JSON 文件失败：{}", err))?;
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|err| format!("JSON 格式错误：{}", err))?;
    let sites_value = value.get("sites").cloned().unwrap_or(value);
    let imported_sites: Vec<ImportedSite> = serde_json::from_value(sites_value)
        .map_err(|_| "JSON 应为站点数组，或包含 sites 数组；每项需有 name 和 url".to_string())?;

    let mut config = state.config.lock().await;
    let mut imported = 0usize;
    let mut skipped = 0usize;
    for site in imported_sites {
        let name = site.name.trim();
        let url = site.url.trim();
        let valid_url = (url.starts_with("http://") || url.starts_with("https://"))
            && url.split_once("://").is_some_and(|(_, rest)| !rest.is_empty());
        let duplicate = config
            .sites
            .iter()
            .any(|existing| existing.url.trim_end_matches('/').eq_ignore_ascii_case(url.trim_end_matches('/')));
        if name.is_empty() || !valid_url || duplicate {
            skipped += 1;
            continue;
        }
        config.sites.push(store::Site {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            url: url.to_string(),
            username: site.username.trim().to_string(),
            password: site.password,
            totp_secret: site.totp_secret.trim().replace(' ', ""),
            auto_login: site.auto_login,
            login_attempts_remaining: None,
            auto_keepalive: true,
        });
        imported += 1;
    }

    store::save_config(&state.app_handle, &config);
    let next = config.clone();
    drop(config);
    restart_scheduler(&state, next.clone()).await;
    Ok(SiteImportResult {
        config: next,
        imported,
        skipped,
    })
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
pub async fn remove_sites(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> Result<AppConfig, String> {
    let ids = ids.into_iter().collect::<std::collections::HashSet<_>>();
    let mut config = state.config.lock().await;
    config.sites.retain(|site| !ids.contains(&site.id));
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
    username: String,
    password: String,
    totp_secret: String,
    auto_login: bool,
    auto_keepalive: bool,
) -> Result<AppConfig, String> {
    let mut config = state.config.lock().await;
    if let Some(site) = config.sites.iter_mut().find(|s| s.id == id) {
        site.name = name;
        site.url = url;
        site.username = username;
        site.password = password;
        site.totp_secret = totp_secret.trim().replace(' ', "");
        site.auto_login = auto_login;
        site.auto_keepalive = auto_keepalive;
    }
    store::save_config(&state.app_handle, &config);
    let next = config.clone();
    drop(config);
    restart_scheduler(&state, next.clone()).await;
    Ok(next)
}

#[tauri::command]
pub async fn test_site_login(
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    state.task_cancel_requested.store(false, Ordering::SeqCst);
    let config = state.config.lock().await.clone();
    let site = config
        .sites
        .iter()
        .find(|site| site.id == id)
        .cloned()
        .ok_or_else(|| "未找到要测试的站点".to_string())?;
    let site_url = site.url.to_ascii_lowercase();
    let is_mteam = site_url.contains("kp.m-team.cc");
    let is_hdkylin = site_url.contains("hdkyl.in");
    let is_nexusphp = !is_mteam && !is_hdkylin;
    if site.username.trim().is_empty() || site.password.is_empty() {
        return Err("请先配置登录用户名和密码".to_string());
    }

    let mut cdp = CdpClient::new(config.cdp_port);
    if let Some(active_port) = cdp.available_port().await {
        cdp = CdpClient::new(active_port);
    } else {
        let progress = CdpProgress::new(
            Arc::clone(&state.logs),
            Arc::clone(&state.task_cancel_requested),
        );
        let result = cdp
            .ensure_available_with_progress(&[site.url.clone()], &progress)
            .await?;
        cdp = CdpClient::new(result.port);
    }

    let tab_id = match cdp.find_tab_for_url(&site.url).await {
        Some(tab_id) => tab_id,
        None => cdp.open_tab(&site.url).await?,
    };
    if is_nexusphp && !config.ocr_server_url.is_empty() {
        push_log(&state.logs, LogEntry::info("正在检查并初始化 OCR 服务...")).await;
        let ocr_server_url = config.ocr_server_url.clone();
        let init_result = tauri::async_runtime::spawn_blocking(move || {
            ocr::ensure_initialized(&ocr_server_url)
        })
        .await
        .unwrap_or_else(|err| Err(err.to_string()));
        match init_result {
            Ok(_) => {
                push_log(&state.logs, LogEntry::success("OCR 服务检查/初始化成功")).await;
            }
            Err(err) => {
                let message = format!("OCR 服务检查/初始化失败：{}", err);
                push_log(&state.logs, LogEntry::error(message.clone())).await;
                return Err(message);
            }
        }
    }

    let login_result = if is_mteam {
        let totp = if site.totp_secret.trim().is_empty() {
            None
        } else {
            match auth::current_totp(&site.totp_secret) {
                Ok(code) => Some(code),
                Err(err) => {
                    let message = format!("{} 登录测试失败：{}", site.name, err);
                    push_log(&state.logs, LogEntry::error(message.clone())).await;
                    return Err(message);
                }
            }
        };
        cdp.login_mteam(&tab_id, &site.username, &site.password, totp.as_deref())
            .await
            .map(|logged_in| (logged_in, None))
            .map_err(|err| (err, None))
    } else if is_hdkylin {
        let secret = (!site.totp_secret.trim().is_empty()).then_some(site.totp_secret.as_str());
        cdp.login_hdkylin(&tab_id, &site.username, &site.password, secret)
            .await
            .map(|logged_in| (logged_in, None))
            .map_err(|err| (err, None))
    } else {
        let secret = (!site.totp_secret.trim().is_empty()).then_some(site.totp_secret.as_str());
        let progress = CdpProgress::new(
            Arc::clone(&state.logs),
            Arc::clone(&state.task_cancel_requested),
        );
        let ocr_cfg = (!config.ocr_server_url.is_empty()).then(|| (config.ocr_server_url.clone(), config.ocr_retry_count));
        cdp.login_nexusphp(
            &tab_id,
            &site.username,
            &site.password,
            secret,
            config.min_login_attempts_remaining as u32,
            ocr_cfg,
            Some(&progress),
            &site.name,
        )
        .await
    };

    let (success, remaining) = match login_result {
        Ok((logged_in, remaining)) => (Ok(logged_in), remaining),
        Err((err, remaining)) => (Err(err), remaining),
    };

    if is_nexusphp {
        if let Some(val) = remaining {
            let mut current = state.config.lock().await;
            if let Some(saved_site) = current.sites.iter_mut().find(|saved| saved.id == site.id) {
                saved_site.login_attempts_remaining = Some(val);
            }
            store::save_config(&state.app_handle, &current);
            drop(current);

            push_log(
                &state.logs,
                LogEntry::info(format!(
                    "{} 当前剩余登录尝试次数：{}（安全阈值：{}）",
                    site.name, val, config.min_login_attempts_remaining
                )),
            )
            .await;
        }
    }

    let message = match success {
        Ok(true) => format!("{} 自动登录测试成功", site.name),
        Ok(false) => format!("{} 当前已处于登录状态", site.name),
        Err(err) => {
            let message = format!("{} 登录测试失败：{}", site.name, err);
            push_log(&state.logs, LogEntry::error(message.clone())).await;
            return Err(message);
        }
    };
    push_log(&state.logs, LogEntry::success(message.clone())).await;
    Ok(message)
}

#[tauri::command]
pub async fn recognize_site_captcha(
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let config = state.config.lock().await.clone();
    let site = config
        .sites
        .iter()
        .find(|site| site.id == id)
        .cloned()
        .ok_or_else(|| "未找到要识别验证码的站点".to_string())?;
    let site_url = site.url.to_ascii_lowercase();
    let is_mteam = site_url.contains("kp.m-team.cc");
    let is_hdkylin = site_url.contains("hdkyl.in");
    let is_nexusphp = !is_mteam && !is_hdkylin;
    if !is_nexusphp {
        return Err("当前验证码识别仅支持 NexusPHP 架构的站点".to_string());
    }
    if config.ocr_server_url.trim().is_empty() {
        return Err("请先在设置中配置 OCR 服务地址".to_string());
    }
    push_log(&state.logs, LogEntry::info("正在检查并初始化 OCR 服务...")).await;
    let ocr_server_url = config.ocr_server_url.clone();
    let init_result = tauri::async_runtime::spawn_blocking(move || {
        ocr::ensure_initialized(&ocr_server_url)
    })
    .await
    .unwrap_or_else(|err| Err(err.to_string()));
    match init_result {
        Ok(_) => {
            push_log(&state.logs, LogEntry::success("OCR 服务检查/初始化成功")).await;
        }
        Err(err) => {
            let message = format!("OCR 服务检查/初始化失败：{}", err);
            push_log(&state.logs, LogEntry::error(message.clone())).await;
            return Err(message);
        }
    }
    let mut cdp = CdpClient::new(config.cdp_port);
    let active_port = cdp
        .available_port()
        .await
        .ok_or_else(|| "专用 Chrome 未连接，请先点击该站点的测试按钮".to_string())?;
    cdp = CdpClient::new(active_port);
    let tab_id = cdp
        .find_tab_for_url(&site.url)
        .await
        .ok_or_else(|| format!("未找到 {} 登录页，请先点击测试按钮", site.name))?;
    let renewed = cdp.prepare_audiences_captcha_retry(&tab_id).await?;
    let remaining = cdp.audiences_remaining_attempts(&tab_id).await?;
    {
        let mut current = state.config.lock().await;
        if let Some(saved_site) = current.sites.iter_mut().find(|saved| saved.id == site.id) {
            if remaining.is_some() {
                saved_site.login_attempts_remaining = remaining;
            }
        }
        store::save_config(&state.app_handle, &current);
    }
    if let Some(remaining) = remaining {
        push_log(
            &state.logs,
            LogEntry::info(format!(
                "{} 当前剩余登录尝试次数：{}（安全阈值：{}）",
                site.name, remaining, config.min_login_attempts_remaining
            )),
        )
        .await;
        if remaining <= config.min_login_attempts_remaining as u32 {
            let message = format!(
                "{} 当前仅剩 {} 次登录机会，已达到安全阈值 {}，停止验证码识别与登录重试",
                site.name, remaining, config.min_login_attempts_remaining
            );
            push_log(&state.logs, LogEntry::error(message.clone())).await;
            return Err(message);
        }
    }
    if renewed {
        push_log(
            &state.logs,
            LogEntry::info(format!(
                "{} 检测到上次验证码无效，已获取新验证码，准备再次尝试登录",
                site.name
            )),
        )
        .await;
        if site.username.trim().is_empty() || site.password.is_empty() {
            return Err("已获取新的图片验证码，但站点用户名或密码未配置".to_string());
        }
        let secret = (!site.totp_secret.trim().is_empty()).then_some(site.totp_secret.as_str());
        let res = cdp
            .login_audiences(
                &tab_id,
                &site.username,
                &site.password,
                secret,
                config.min_login_attempts_remaining as u32,
                None,
                None,
            )
            .await;

        let remaining_after = match &res {
            Ok((_, rem)) => *rem,
            Err((_, rem)) => *rem,
        };

        if let Some(val) = remaining_after {
            let mut current = state.config.lock().await;
            if let Some(saved_site) = current.sites.iter_mut().find(|saved| saved.id == site.id) {
                saved_site.login_attempts_remaining = Some(val);
            }
            store::save_config(&state.app_handle, &current);
        }

        match res {
            Err((err, _)) if err.contains("登录信息已填写") => {}
            Err((err, _)) => return Err(format!("重新填写 Audiences 登录信息失败：{}", err)),
            Ok(_) => return Err("Audiences 当前不再需要图片验证码".to_string()),
        }
    } else {
        push_log(
            &state.logs,
            LogEntry::info(format!(
                "{} 未检测到验证码失败页，本次不执行登录重试，仅识别当前验证码",
                site.name
            )),
        )
        .await;
    }
    let image = cdp.audiences_captcha_base64(&tab_id).await?;
    push_log(
        &state.logs,
        LogEntry::info(format!(
            "{} 获取验证码图片成功，准备发送给 OCR 服务，Base64 为：{}",
            site.name, image
        )),
    )
    .await;
    let image_for_ocr = image.clone();
    let ocr_server_url = config.ocr_server_url.clone();
    let ocr_retry_count = config.ocr_retry_count;
    let recognition = tauri::async_runtime::spawn_blocking(move || {
        ocr::recognize(&ocr_server_url, &image_for_ocr, ocr_retry_count)
    })
        .await
        .map_err(|err| err.to_string())?;
    let recognition = match recognition {
        Ok(result) => result,
        Err(err) => {
            let message = format!("{} OCR 识别失败：{}", site.name, err);
            push_log(&state.logs, LogEntry::error(message.clone())).await;
            return Err(message);
        }
    };
    push_log(
        &state.logs,
        LogEntry::info(format!(
            "{} OCR 识别：第 {}/{} 次成功，验证码：{}",
            site.name,
            recognition.attempts,
            ocr_retry_count,
            recognition.text
        )),
    )
    .await;
    cdp.fill_audiences_captcha(&tab_id, &recognition.text).await?;
    let message = format!("{} 验证码已识别并填入，请在浏览器中确认后点击登录", site.name);
    push_log(&state.logs, LogEntry::info(message.clone())).await;
    Ok(message)
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
    let payload = tauri::async_runtime::spawn_blocking(move || {
        cookiecloud::fetch_sync_payload(&cookiecloud_config)
    })
    .await
    .map_err(|err| err.to_string())??;

    import_cookiecloud_cookies(&state, &config, payload).await
}

async fn import_cookiecloud_cookies(
    state: &State<'_, AppState>,
    config: &AppConfig,
    payload: serde_json::Value,
) -> Result<CookieCloudSyncResult, String> {
    let sync_data = cookiecloud::sync_data_from_cookiecloud(payload, &config.sites)?;
    if sync_data.cookies.is_empty() && sync_data.local_storages.is_empty() {
        let message = "CookieCloud 未解析到可同步的 Cookie 或 Local Storage".to_string();
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

    let mut write_port = active_port;
    let imported_cookie_params = match CdpClient::new(write_port).set_cookies(&sync_data.cookies).await {
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
            write_port = result.port;
            CdpClient::new(write_port)
                .set_cookies(&sync_data.cookies)
                .await
                .map_err(|retry_err| format!("Cookie 已拉取，但写入 Chrome 失败：{}", retry_err))?
        }
        Err(err) => {
            return Err(format!("Cookie 已拉取，但写入 Chrome 失败：{}", err));
        }
    };
    let (imported_local_storages, opened_sync_tabs) = CdpClient::new(write_port)
        .set_local_storage_with_opened_tabs(&sync_data.local_storages)
        .await
        .map_err(|err| format!("Local Storage 已拉取，但写入 Chrome 失败：{}", err))?;

    // Cookie/Local Storage 写入成功后，刷新 Chrome 中已打开的站点页面，使新登录态立即生效。
    let site_urls: Vec<String> = config.sites.iter().map(|s| s.url.clone()).collect();
    CdpClient::new(write_port)
        .reload_tabs_for_sites(&site_urls)
        .await;

    if config.auto_close_sync_tabs && !opened_sync_tabs.is_empty() {
        let logs = Arc::clone(&state.logs);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let cdp = CdpClient::new(write_port);
            let mut closed = 0usize;
            for tab_id in opened_sync_tabs {
                if cdp.close_tab(&tab_id).await.is_ok() {
                    closed += 1;
                }
            }
            if closed > 0 {
                push_log(
                    &logs,
                    LogEntry::info(format!("Cookie 同步自动打开的 {} 个标签页已关闭", closed)),
                )
                .await;
            }
        });
    }

    let result = CookieCloudSyncResult {
        matched_cookies: sync_data.cookies.len(),
        imported_cookies: imported_cookie_params.len(),
        matched_local_storages: local_storage_item_count(&sync_data.local_storages),
        imported_local_storages: local_storage_item_count(&imported_local_storages),
    };
    // 匹配站点按 CookieCloud 解析命中统计；写入数量另算，方便区分“没匹配到”和“匹配到但写入失败”。
    let site_match = site_match_summary(&config.sites, &sync_data.cookies, &sync_data.local_storages);
    let (_, detail) = cookie_summary(&imported_cookie_params);
    let storage_detail = local_storage_summary(&imported_local_storages);
    push_log(
        &state.logs,
        LogEntry::success(format!(
            "CookieCloud 同步完成：匹配 {} 个站点，共解析 {} 条 Cookie / {} 条 Local Storage，成功写入 {} 条 Cookie / {} 条 Local Storage；{}；{}；{}",
            site_match.matched_count,
            result.matched_cookies,
            result.matched_local_storages,
            result.imported_cookies,
            result.imported_local_storages,
            site_match.detail,
            detail,
            storage_detail
        )),
    )
    .await;
    Ok(result)
}

struct CookieSiteMatchSummary {
    matched_count: usize,
    detail: String,
}

fn site_match_summary(
    sites: &[crate::store::Site],
    cookies: &[crate::cdp::CdpCookieParam],
    local_storages: &[CdpLocalStorageParam],
) -> CookieSiteMatchSummary {
    let matched = sites
        .iter()
        .filter(|site| site_has_sync_match(site, cookies, local_storages))
        .map(|site| site.name.clone())
        .collect::<Vec<_>>();
    let unmatched = sites
        .iter()
        .filter(|site| !site_has_sync_match(site, cookies, local_storages))
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

fn site_has_sync_match(
    site: &crate::store::Site,
    cookies: &[crate::cdp::CdpCookieParam],
    local_storages: &[CdpLocalStorageParam],
) -> bool {
    let Some(site_host) = host_from_url(&site.url) else {
        return false;
    };

    let cookie_matched = cookies.iter().any(|cookie| {
        let Some(cookie_host) = cookie_host(cookie) else {
            return false;
        };
        hosts_match(&site_host, &cookie_host)
    });
    let storage_matched = local_storages
        .iter()
        .any(|storage| hosts_match(&site_host, &storage.host));

    cookie_matched || storage_matched
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

fn hosts_match(site_host: &str, data_host: &str) -> bool {
    match_host(site_host) == match_host(data_host)
}

fn match_host(value: &str) -> String {
    let host = value
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches('.')
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
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

fn local_storage_item_count(local_storages: &[CdpLocalStorageParam]) -> usize {
    local_storages
        .iter()
        .map(|storage| storage.items.len())
        .sum()
}

fn local_storage_summary(local_storages: &[CdpLocalStorageParam]) -> String {
    if local_storages.is_empty() {
        return "导入 Local Storage：无".to_string();
    }

    let details = local_storages
        .iter()
        .map(|storage| {
            let mut names = storage
                .items
                .iter()
                .map(|item| item.name.clone())
                .collect::<Vec<_>>();
            names.sort();
            names.dedup();
            format!("{}：{}", storage.host, names.join(", "))
        })
        .collect::<Vec<_>>()
        .join("；");
    format!("导入 Local Storage：{}", details)
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
    let app_handle = state.app_handle.clone();
    let config_state = Arc::clone(&state.config);
    tauri::async_runtime::spawn(async move {
        scheduler::run_with_flag(
            &config,
            &logs,
            &task_running,
            &task_cancel_requested,
            false,
            Some(&app_handle),
            Some(&config_state),
        )
        .await;
    });
    Ok(())
}

#[tauri::command]
pub async fn stop_task(state: State<'_, AppState>) -> Result<(), String> {
    state.task_cancel_requested.store(true, Ordering::SeqCst);
    let is_running = *state.task_running.lock().await;
    if !is_running {
        push_log(&state.logs, LogEntry::info("已请求终止测试/保活任务")).await;
        return Ok(());
    }

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
pub async fn clear_browser_data(state: State<'_, AppState>) -> Result<(), String> {
    if *state.task_running.lock().await {
        return Err("保活任务执行中，请等待结束后再清除浏览器数据".to_string());
    }

    let config = state.config.lock().await.clone();
    let site_urls = config
        .sites
        .iter()
        .map(|site| site.url.clone())
        .collect::<Vec<_>>();
    let cdp = CdpClient::new(config.cdp_port);
    let message = if let Some(active_port) = cdp.available_port().await {
        CdpClient::new(active_port)
            .clear_browser_data(&site_urls)
            .await?;
        format!("已通过 CDP 清除专用 Chrome 浏览器数据：localhost:{}", active_port)
    } else {
        let cleared = cdp::clear_dedicated_profile_data()?;
        if cleared == 0 {
            "专用 Chrome 浏览器数据为空，无需清除".to_string()
        } else {
            "已清除专用 Chrome Profile，Cookie、Local Storage 和缓存将在下次启动时重新生成".to_string()
        }
    };

    push_log(&state.logs, LogEntry::success(message)).await;
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
        Some(state.app_handle.clone()),
        Some(Arc::clone(&state.config)),
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

#[tauri::command]
pub async fn export_config(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let config = state.config.lock().await;
    let json_content = serde_json::to_string_pretty(&*config)
        .map_err(|err| format!("序列化配置失败: {}", err))?;
    std::fs::write(&path, json_content)
        .map_err(|err| format!("写入配置文件失败: {}", err))?;
    Ok(())
}

#[tauri::command]
pub async fn import_config(state: State<'_, AppState>, path: String) -> Result<AppConfig, String> {
    let json_content = std::fs::read_to_string(&path)
        .map_err(|err| format!("读取配置文件失败: {}", err))?;
    let new_config: AppConfig = serde_json::from_str(&json_content)
        .map_err(|err| format!("解析配置文件失败（文件格式可能不正确）: {}", err))?;
    
    let mut config = state.config.lock().await;
    *config = new_config.clone();
    
    store::save_config(&state.app_handle, &config);
    
    store::set_log_retention(config.log_retention);
    let _ = apply_auto_launch(&state.app_handle, config.auto_launch);
    
    restart_scheduler(&state, config.clone()).await;
    
    Ok(new_config)
}
