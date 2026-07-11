use crate::auth;
use crate::store::{self, LogEntry};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_CDP_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize, Debug)]
pub struct CdpTab {
    pub id: String,
    pub url: Option<String>,
    #[serde(rename = "type")]
    pub tab_type: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
}

struct HttpResponse {
    status: u16,
    body: String,
}

pub struct CdpClient {
    port: u16,
}

pub struct CdpLaunchResult {
    pub port: u16,
    pub message: String,
    pub opened_initial_urls: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpCookieParam {
    pub name: String,
    pub value: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CdpLocalStorageEntry {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct CdpLocalStorageParam {
    pub host: String,
    pub origin: String,
    pub items: Vec<CdpLocalStorageEntry>,
}

pub const CDP_CANCELLED: &str = "CDP 操作已终止";

#[derive(Clone)]
pub struct CdpProgress {
    logs: Arc<Mutex<Vec<LogEntry>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl CdpProgress {
    pub fn new(logs: Arc<Mutex<Vec<LogEntry>>>, cancel_requested: Arc<AtomicBool>) -> Self {
        Self {
            logs,
            cancel_requested,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    async fn info(&self, message: impl Into<String>) {
        store::push_log(&self.logs, LogEntry::info(message)).await;
    }
}

impl CdpClient {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// 返回当前可用的 CDP 端口；自动模式会从 Chrome 写出的 DevToolsActivePort 中读取真实端口。
    pub async fn available_port(&self) -> Option<u16> {
        if self
            .is_available_with_timeout(Duration::from_millis(600))
            .await
        {
            return Some(self.port);
        }

        for profile_dir in [dedicated_profile_dir(), recovery_profile_dir()] {
            let Some(port) = read_devtools_port(&profile_dir) else {
                continue;
            };
            if CdpClient::new(port)
                .is_available_with_timeout(Duration::from_millis(600))
                .await
            {
                return Some(port);
            }
        }

        None
    }

    /// 检测 Chrome 是否以调试模式运行。
    pub async fn is_available(&self) -> bool {
        self.is_available_with_timeout(Duration::from_secs(3)).await
    }

    async fn is_available_with_timeout(&self, timeout: Duration) -> bool {
        self.request("GET", "/json/version", timeout)
            .map(|response| (200..300).contains(&response.status))
            .unwrap_or(false)
    }

    /// 确保 Chrome 已开放 CDP 端口；自动启动时优先使用配置端口，冲突时再退回随机端口。
    pub async fn ensure_available_with_progress(
        &self,
        initial_urls: &[String],
        progress: &CdpProgress,
    ) -> Result<CdpLaunchResult, String> {
        self.ensure_available_inner(initial_urls, Some(progress))
            .await
    }

    async fn ensure_available_inner(
        &self,
        initial_urls: &[String],
        progress: Option<&CdpProgress>,
    ) -> Result<CdpLaunchResult, String> {
        log_progress(
            progress,
            format!("检测配置端口 localhost:{} 是否已有 CDP 响应", self.port),
        )
        .await;
        if self.is_available().await {
            check_cancel(progress)?;
            log_progress(progress, format!("配置端口 localhost:{} 已响应", self.port)).await;
            let opened_initial_urls = self.ensure_initial_urls(initial_urls, progress).await?;
            return Ok(CdpLaunchResult {
                port: self.port,
                message: connected_message("Chrome CDP 已连接", self.port, opened_initial_urls),
                opened_initial_urls,
            });
        }
        check_cancel(progress)?;

        let profile_dir = dedicated_profile_dir();
        log_progress(progress, format!("检查专用 Chrome Profile：{}", profile_dir.display())).await;
        if profile_dir_in_use(&profile_dir) {
            log_progress(
                progress,
                "专用 Chrome Profile 正在被占用，但没有可用 CDP；跳过复用，改用备用 Profile",
            )
            .await;
        } else if self.port > 0 && port_is_free(self.port) {
            log_progress(
                progress,
                format!("配置端口 localhost:{} 可用，优先按该端口启动", self.port),
            )
            .await;
            if let Some(result) =
                launch_and_wait(&profile_dir, false, initial_urls, Some(self.port), progress)
                    .await?
            {
                return Ok(result);
            }

            check_cancel(progress)?;
            log_progress(
                progress,
                format!(
                    "配置端口 localhost:{} 启动后未响应，准备改用随机端口",
                    self.port
                ),
            )
            .await;
        } else {
            log_progress(
                progress,
                format!("配置端口 localhost:{} 已被占用，准备改用随机端口", self.port),
            )
            .await;
            if let Some(result) = launch_and_wait(&profile_dir, false, initial_urls, None, progress)
                .await?
            {
                return Ok(result);
            }

            check_cancel(progress)?;
            log_progress(progress, "随机端口启动后未响应，准备尝试备用 Profile").await;
        }

        let recovery_dir = recovery_profile_dir();
        if let Some(result) =
            launch_and_wait(&recovery_dir, true, initial_urls, None, progress).await?
        {
            return Ok(result);
        }

        Err(format!(
            "已尝试自动启动 Chrome，但 CDP 仍未连接。请关闭刚打开的专用 Chrome 后重试，或在设置里更换 CDP 端口。原端口：{}",
            self.port
        ))
    }

    /// 在 Chrome 中打开新标签页，返回 tab ID。
    pub async fn open_tab(&self, url: &str) -> Result<String, String> {
        let encoded = encode_cdp_target_url(url);
        let response = self.request(
            "PUT",
            &format!("/json/new?{}", encoded),
            Duration::from_secs(10),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }

        let tab = serde_json::from_str::<CdpTab>(&response.body).map_err(|err| err.to_string())?;
        Ok(tab.id)
    }

    /// 查找已经打开到相同站点的标签页，避免启动 Chrome 后再重复打开同一个站点。
    pub async fn find_tab_for_url(&self, url: &str) -> Option<String> {
        let response = self
            .request("GET", "/json/list", Duration::from_secs(5))
            .ok()?;
        if !(200..300).contains(&response.status) {
            return None;
        }

        let tabs = serde_json::from_str::<Vec<CdpTab>>(&response.body).ok()?;
        let expected_host = host_from_url(url)?;
        tabs.into_iter()
            .filter(|tab| tab.tab_type.as_deref() == Some("page"))
            .find(|tab| {
                tab.url
                    .as_deref()
                    .and_then(host_from_url)
                    .map(|host| host == expected_host)
                    .unwrap_or(false)
            })
            .map(|tab| tab.id)
    }

    pub async fn set_cookies(
        &self,
        cookies: &[CdpCookieParam],
    ) -> Result<Vec<CdpCookieParam>, String> {
        if cookies.is_empty() {
            return Ok(Vec::new());
        }

        let websocket_url = self.page_websocket_url().await?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let mut imported = Vec::new();

        for cookie in cookies {
            let params = serde_json::to_value(cookie)
                .map_err(|err| format!("Cookie 参数序列化失败：{}", err))?;
            let response = websocket.call("Network.setCookie", params)?;
            let success = response
                .get("result")
                .and_then(|value| value.get("success"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if success {
                imported.push(cookie.clone());
            }
        }

        Ok(imported)
    }

    /// 写入 Local Storage，并返回本次为写入数据而新打开的标签页 ID。
    /// 调用方可据此延迟关闭标签页，不影响用户原本已经打开的页面。
    pub async fn set_local_storage_with_opened_tabs(
        &self,
        storages: &[CdpLocalStorageParam],
    ) -> Result<(Vec<CdpLocalStorageParam>, Vec<String>), String> {
        if storages.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let mut imported = Vec::new();
        let mut opened_tab_ids = Vec::new();
        for storage in storages {
            let tab_id = match self.find_tab_for_url(&storage.origin).await {
                Some(tab_id) => tab_id,
                None => {
                    let tab_id = self.open_tab(&storage.origin).await?;
                    opened_tab_ids.push(tab_id.clone());
                    tab_id
                }
            };
            self.wait_for_tab_host(&tab_id, &storage.host).await;
            let Some(websocket_url) = self.websocket_url_for_tab(&tab_id)? else {
                continue;
            };
            let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
            ensure_storage_page_ready(&mut websocket, storage).await;
            let _ = websocket.call("DOMStorage.enable", serde_json::json!({}));
            let mut imported_items = set_local_storage_items(&mut websocket, storage, &storage.items);
            let missing_items = storage
                .items
                .iter()
                .filter(|item| !imported_items.iter().any(|written| written.name == item.name))
                .cloned()
                .collect::<Vec<_>>();
            if !missing_items.is_empty() {
                tokio::time::sleep(Duration::from_millis(600)).await;
                imported_items.extend(set_local_storage_items(&mut websocket, storage, &missing_items));
            }
            if !imported_items.is_empty() {
                imported_items.sort_by(|a, b| a.name.cmp(&b.name));
                imported_items.dedup_by(|a, b| a.name == b.name);
                imported.push(CdpLocalStorageParam {
                    host: storage.host.clone(),
                    origin: storage.origin.clone(),
                    items: imported_items,
                });
            }
        }

        Ok((imported, opened_tab_ids))
    }

    /// Cookie 写入后，对 Chrome 中已打开的目标站点页面执行刷新，
    /// 使新 Cookie 立即生效——否则用户看到的仍是旧的未登录状态。
    pub async fn reload_tabs_for_sites(&self, site_urls: &[String]) {
        let response = match self.request("GET", "/json/list", Duration::from_secs(5)) {
            Ok(resp) => resp,
            Err(_) => return,
        };
        if !(200..300).contains(&response.status) {
            return;
        }
        let tabs = match serde_json::from_str::<Vec<CdpTab>>(&response.body) {
            Ok(tabs) => tabs,
            Err(_) => return,
        };

        for tab in tabs
            .iter()
            .filter(|tab| tab.tab_type.as_deref() == Some("page"))
        {
            let Some(tab_url) = tab.url.as_deref() else {
                continue;
            };
            let Some(tab_host) = host_from_url(tab_url) else {
                continue;
            };
            let matches = site_urls.iter().any(|site_url| {
                host_from_url(site_url)
                    .map(|site_host| site_host == tab_host)
                    .unwrap_or(false)
            });
            if !matches {
                continue;
            }

            let Some(ws_url) = tab.web_socket_debugger_url.as_deref() else {
                continue;
            };
            if let Ok(mut ws) = CdpWebSocket::connect(ws_url, Duration::from_secs(5)) {
                // 忽略刷新失败，不影响主流程
                let _ = ws.call("Page.reload", serde_json::json!({}));
            }
        }
    }

    /// 通过 CDP 清除专用浏览器数据，给 CookieCloud 重新同步前准备干净环境。
    pub async fn clear_browser_data(&self, site_urls: &[String]) -> Result<(), String> {
        let websocket_url = self.page_websocket_url().await?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        websocket.call("Network.clearBrowserCookies", serde_json::json!({}))?;
        websocket.call("Network.clearBrowserCache", serde_json::json!({}))?;
        for origin in unique_origins(site_urls) {
            websocket.call(
                "Storage.clearDataForOrigin",
                serde_json::json!({
                    "origin": origin,
                    "storageTypes": "all"
                }),
            )?;
        }
        Ok(())
    }

    async fn page_websocket_url(&self) -> Result<String, String> {
        if let Some(url) = self.find_page_websocket_url()? {
            return Ok(url);
        }

        self.open_tab("about:blank").await?;
        self.find_page_websocket_url()?
            .ok_or_else(|| "未找到可用的 Chrome 页面调试通道".to_string())
    }

    fn find_page_websocket_url(&self) -> Result<Option<String>, String> {
        let response = self.request("GET", "/json/list", Duration::from_secs(5))?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }

        let tabs =
            serde_json::from_str::<Vec<CdpTab>>(&response.body).map_err(|err| err.to_string())?;
        Ok(tabs
            .into_iter()
            .filter(|tab| tab.tab_type.as_deref() == Some("page"))
            .find_map(|tab| tab.web_socket_debugger_url))
    }

    fn websocket_url_for_tab(&self, tab_id: &str) -> Result<Option<String>, String> {
        let response = self.request("GET", "/json/list", Duration::from_secs(5))?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }

        let tabs =
            serde_json::from_str::<Vec<CdpTab>>(&response.body).map_err(|err| err.to_string())?;
        Ok(tabs
            .into_iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.web_socket_debugger_url))
    }

    async fn wait_for_tab_host(&self, tab_id: &str, expected_host: &str) {
        for _ in 0..20 {
            if self
                .tab_host(tab_id)
                .map(|host| host == expected_host)
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    fn tab_host(&self, tab_id: &str) -> Option<String> {
        let response = self.request("GET", "/json/list", Duration::from_secs(5)).ok()?;
        if !(200..300).contains(&response.status) {
            return None;
        }

        serde_json::from_str::<Vec<CdpTab>>(&response.body)
            .ok()?
            .into_iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.url)
            .and_then(|url| host_from_url(&url))
    }

    async fn ensure_initial_urls(
        &self,
        initial_urls: &[String],
        progress: Option<&CdpProgress>,
    ) -> Result<usize, String> {
        let mut opened_count = 0;

        for url in initial_urls
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            check_cancel(progress)?;
            log_progress(progress, format!("检查站点标签页：{}", url)).await;
            if self.find_tab_for_url(url).await.is_some() {
                log_progress(progress, format!("站点已在 Chrome 中打开：{}", url)).await;
                opened_count += 1;
                continue;
            }

            log_progress(progress, format!("通过 CDP 新建标签页：{}", url)).await;
            self.open_tab(url)
                .await
                .map_err(|err| format!("CDP 已连接，但打开站点 {} 失败：{}", url, err))?;
            opened_count += 1;
        }

        Ok(opened_count)
    }

    /// 关闭指定标签页。
    pub async fn close_tab(&self, tab_id: &str) -> Result<(), String> {
        let response = self.request(
            "GET",
            &format!("/json/close/{}", tab_id),
            Duration::from_secs(10),
        )?;
        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(format!("CDP 返回 HTTP {}", response.status))
        }
    }

    /// 在 M-Team 登录页填写凭据并提交。返回 false 表示当前页面不需要登录。
    pub async fn login_mteam(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_code: Option<&str>,
    ) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 M-Team 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;

        let mut state = None;
        let mut stable_non_login = 0usize;
        for _ in 0..24 {
            state = login_page_state(&mut websocket);
            if let Some(current) = state.as_ref() {
                if current.host == "kp.m-team.cc" && current.ready && current.has_login_form {
                    break;
                }
                if current.host == "kp.m-team.cc"
                    && current.ready
                    && current.path != "/login"
                    && !current.has_login_form
                {
                    stable_non_login += 1;
                    if stable_non_login >= 12 {
                        break;
                    }
                } else {
                    stable_non_login = 0;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let Some(current) = state else {
            return Err("无法读取 M-Team 登录页状态".to_string());
        };
        if current.host != "kp.m-team.cc" {
            return Ok(false);
        }
        if !current.has_login_form {
            if current.path == "/login" {
                return Err("M-Team 已进入登录页，但登录表单未加载完成".to_string());
            }
            return Ok(false);
        }

        human_delay(550, 1250).await;
        if !type_runtime_input(&mut websocket, "#username", username, 45, 115).await {
            return Err("未找到 M-Team 用户名输入框".to_string());
        }
        human_delay(450, 1050).await;
        if !type_runtime_input(&mut websocket, "#password", password, 55, 135).await {
            return Err("未找到 M-Team 密码输入框".to_string());
        }
        human_delay(700, 1550).await;
        if !click_runtime_element(&mut websocket, "button[type=\"submit\"]") {
            return Err("未找到 M-Team 登录按钮".to_string());
        }

        let mut otp_submitted = false;
        let mut stable_logged_in = 0usize;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(current) = login_page_state(&mut websocket) else {
                continue;
            };
            if current.has_otp && !otp_submitted {
                let code = totp_code.ok_or_else(|| "M-Team 要求 2FA，但站点未配置 2FA 密钥".to_string())?;
                if !submit_otp(&mut websocket, code).await {
                    return Err("检测到 M-Team 2FA 验证，但未能填写或提交验证码".to_string());
                }
                otp_submitted = true;
                stable_logged_in = 0;
                continue;
            }
            if current.path != "/login" && !current.has_login_form {
                stable_logged_in += 1;
                if stable_logged_in >= 4 {
                    return Ok(true);
                }
            } else {
                stable_logged_in = 0;
            }
        }
        Err("M-Team 登录后仍停留在登录页，请检查账号、密码、2FA 或验证码要求".to_string())
    }

    /// 等待 HDKylin 的正常安全检测放行后，以带间隔的方式填写并提交登录表单。
    pub async fn login_hdkylin(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
    ) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 HDKylin 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let mut login_ready = false;
        let mut stable_logged_in = 0usize;

        for _ in 0..240 {
            if let Some(current) = hdk_login_page_state(&mut websocket) {
                if current.blocked_debug {
                    return Err("雷池 WAF 检测到调试环境，需要在专用 Chrome 中人工完成验证".to_string());
                }
                if current.has_captcha {
                    return Err("HDKylin 登录页要求图形验证码，需要人工完成".to_string());
                }
                if current.has_login_form {
                    login_ready = true;
                    break;
                }
                if current.ready && !current.challenge && current.path != "/login.php" {
                    stable_logged_in += 1;
                    if stable_logged_in >= 6 {
                        return Ok(false);
                    }
                } else {
                    stable_logged_in = 0;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !login_ready {
            return Err("等待 HDKylin 安全检测放行超时（120 秒）".to_string());
        }

        human_delay(650, 1400).await;
        if !type_runtime_input(
            &mut websocket,
            "#username, input[name=\"username\"], input[autocomplete=\"username\"]",
            username,
            45,
            120,
        )
        .await
        {
            return Err("未找到 HDKylin 用户名输入框".to_string());
        }
        human_delay(500, 1200).await;
        if !type_runtime_input(
            &mut websocket,
            "#password, input[name=\"password\"], input[type=\"password\"]",
            password,
            55,
            140,
        )
        .await
        {
            return Err("未找到 HDKylin 密码输入框".to_string());
        }
        human_delay(750, 1650).await;
        if !click_runtime_element(
            &mut websocket,
            "button[type=\"submit\"], input[type=\"submit\"], input[name=\"login\"]",
        ) {
            return Err("未找到 HDKylin 登录按钮".to_string());
        }

        let mut otp_submitted = false;
        stable_logged_in = 0;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(current) = hdk_login_page_state(&mut websocket) else {
                continue;
            };
            if current.blocked_debug {
                return Err("登录过程中被雷池 WAF 拦截，需要人工完成验证".to_string());
            }
            if current.has_captcha {
                return Err("HDKylin 要求图形验证码，需要人工完成".to_string());
            }
            if current.has_otp && !otp_submitted {
                let secret = totp_secret
                    .ok_or_else(|| "HDKylin 要求 2FA，但站点未配置 2FA 密钥".to_string())?;
                let code = auth::current_totp(secret)?;
                human_delay(550, 1200).await;
                if !submit_otp(&mut websocket, &code).await {
                    return Err("检测到 HDKylin 2FA，但未能填写或提交验证码".to_string());
                }
                otp_submitted = true;
                stable_logged_in = 0;
                continue;
            }
            if current.ready && !current.challenge && !current.has_login_form {
                stable_logged_in += 1;
                if stable_logged_in >= 4 {
                    return Ok(true);
                }
            } else {
                stable_logged_in = 0;
            }
        }
        Err("HDKylin 登录后仍停留在登录页，请检查凭据或人工验证要求".to_string())
    }

    /// 等待 Audiences 的 Cloudflare 自动验证，并填写可自动处理的登录字段。
    /// 通用 NexusPHP 站点的登录逻辑（包含可选的 TOTP、Cloudflare 绕过、以及自动 OCR 验证码识别）
    pub async fn login_nexusphp(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
        min_remaining_attempts: u32,
        ocr_config: Option<(String, u8)>,
        progress: Option<&CdpProgress>,
        site_name: &str,
    ) -> Result<(bool, Option<u32>), (String, Option<u32>)> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id).map_err(|err| (err, None))? else {
            return Err((format!("无法连接 {} 标签页", site_name), None));
        };
        
        let ocr_cfg = ocr_config.clone();
        let max_attempts = ocr_config.as_ref().map(|(_, count)| *count as usize).unwrap_or(1);
        let mut last_remaining_attempts = None;
        
        for attempt in 0..max_attempts {
            if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
            }
            if attempt > 0 {
                if let Some(p) = progress {
                    p.info(format!("前一次登录尝试失败，准备点击获取新的图片代码进行第 {}/{} 次重试...", attempt + 1, max_attempts)).await;
                }
                self.prepare_audiences_captcha_retry(tab_id)
                    .await
                    .map_err(|err| (err, last_remaining_attempts))?;
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }

            let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10)).map_err(|err| (err.to_string(), last_remaining_attempts))?;
            let mut login_state = None;
            let mut stable_logged_in = 0usize;

            for _ in 0..240 {
                if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                    return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
                }
                if let Some(current) = nexus_login_page_state(&mut websocket) {
                    if let Some(rem) = current.remaining_attempts {
                        last_remaining_attempts = Some(rem);
                    }
                    if current.has_login_form {
                        login_state = Some(current);
                        break;
                    }
                    if current.ready && !current.challenge && current.logged_in {
                        stable_logged_in += 1;
                        if stable_logged_in >= 3 {
                            return Ok((false, last_remaining_attempts)); // 已经登录，直接返回
                        }
                    } else {
                        stable_logged_in = 0;
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            
            let Some(login_state) = login_state else {
                return Err((format!("等待 {} Cloudflare 验证放行超时（120 秒）", site_name), last_remaining_attempts));
            };
            
            if let Some(remaining) = login_state.remaining_attempts {
                last_remaining_attempts = Some(remaining);
                if remaining <= min_remaining_attempts {
                    return Err((format!(
                        "{} 当前仅剩 {} 次登录机会，已达到安全阈值 {}，停止自动登录与重试",
                        site_name, remaining, min_remaining_attempts
                    ), last_remaining_attempts));
                }
            }

            human_delay(650, 1350).await;
            if !type_runtime_input(
                &mut websocket,
                "input[name=\"username\"], input[name=\"email\"], input[name=\"login\"], input[autocomplete=\"username\"]",
                username,
                45,
                120,
            )
            .await
            {
                return Err((format!("未找到 {} 用户名输入框", site_name), last_remaining_attempts));
            }
            human_delay(500, 1150).await;
            if !type_runtime_input(
                &mut websocket,
                "input[name=\"password\"], input[type=\"password\"]",
                password,
                55,
                140,
            )
            .await
            {
                return Err((format!("未找到 {} 密码输入框", site_name), last_remaining_attempts));
            }
            if let Some(secret) = totp_secret {
                if login_state.has_two_factor {
                    let code = auth::current_totp(secret).map_err(|err| (err, last_remaining_attempts))?;
                    human_delay(450, 950).await;
                    if !type_runtime_input(
                        &mut websocket,
                        "input[name=\"two_factor\"], input[name=\"twofactor\"], input[name=\"otp\"], input[name=\"two_factor_code\"], input[name=\"2fa_secret\"], input[name=\"2fa\"], input[autocomplete=\"one-time-code\"]",
                        &code,
                        70,
                        145,
                    )
                    .await
                    {
                        return Err((format!("未找到 {} 两步验证码输入框", site_name), last_remaining_attempts));
                    }
                }
            }

            let has_ocr = ocr_config.is_some();
            let current = nexus_login_page_state(&mut websocket)
                .ok_or_else(|| (format!("无法读取 {} 登录表单状态", site_name), last_remaining_attempts))?;
            
            if let Some(rem) = current.remaining_attempts {
                last_remaining_attempts = Some(rem);
            }
            
            if current.has_captcha {
                if let Some((ocr_server_url, ocr_retry_count)) = ocr_cfg.clone() {
                    if let Some(p) = progress {
                        p.info("检测到图片验证码，正在获取图片数据并进行自动 OCR 识别...").await;
                    }
                    
                    let expression = r#"(() => {
                        const image = document.querySelector('img[alt="CAPTCHA"], img[src*="captcha" i], img[src*="image.php" i]');
                        if (!image || !image.complete || !image.naturalWidth) return { ok: false };
                        try {
                            const scale = 3;
                            const padding = 8;
                            const canvas = document.createElement('canvas');
                            canvas.width = image.naturalWidth * scale + padding * 2;
                            canvas.height = image.naturalHeight * scale + padding * 2;
                            const context = canvas.getContext('2d', { alpha: false });
                            context.fillStyle = '#ffffff';
                            context.fillRect(0, 0, canvas.width, canvas.height);
                            context.imageSmoothingEnabled = false;
                            context.filter = 'contrast(140%) saturate(120%)';
                            context.drawImage(
                                image,
                                padding,
                                padding,
                                image.naturalWidth * scale,
                                image.naturalHeight * scale
                            );
                            context.filter = 'none';
                            return {
                                ok: true,
                                image: canvas.toDataURL('image/png').split(',')[1],
                                width: canvas.width,
                                height: canvas.height
                            };
                        } catch (_) {
                            return { ok: false };
                        }
                    })()"#;
                    
                    let response = websocket
                        .call(
                            "Runtime.evaluate",
                            serde_json::json!({
                                "expression": expression,
                                "returnByValue": true
                            }),
                        )
                        .map_err(|err| (format!("截取验证码失败：{}", err), last_remaining_attempts))?;
                    
                    let value = response
                        .get("result")
                        .and_then(|value| value.get("result"))
                        .and_then(|value| value.get("value"))
                        .ok_or_else(|| ("未读取到验证码图片数据".to_string(), last_remaining_attempts))?;
                    
                    if !value.get("ok").and_then(|value| value.as_bool()).unwrap_or(false) {
                        return Err(("验证码图片尚未加载或无法截取".to_string(), last_remaining_attempts));
                    }
                    
                    let image_base64 = value
                        .get("image")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| ("验证码截图数据为空".to_string(), last_remaining_attempts))?;

                    if let Some(p) = progress {
                        p.info(format!("获取验证码图片成功，准备发送给 OCR 服务，Base64 为：{}", image_base64)).await;
                    }

                    let ocr_server_url_clone = ocr_server_url.clone();
                    let image_base64_clone = image_base64.to_string();
                    let recognition = tauri::async_runtime::spawn_blocking(move || {
                        crate::ocr::recognize(&ocr_server_url_clone, &image_base64_clone, ocr_retry_count)
                    })
                    .await
                    .map_err(|err| (err.to_string(), last_remaining_attempts))?;

                    let recognition = match recognition {
                        Ok(res) => res,
                        Err(err) => return Err((format!("验证码自动识别失败：{}", err), last_remaining_attempts)),
                    };

                    if let Some(p) = progress {
                        p.info(format!("验证码识别成功：{}，尝试次数：{}/{}。正在自动填入...", recognition.text, recognition.attempts, ocr_retry_count)).await;
                    }

                    human_delay(500, 1000).await;
                    if !type_runtime_input(
                        &mut websocket,
                        "input[name=\"imagestring\"]",
                        &recognition.text,
                        70,
                        145,
                    )
                    .await
                    {
                        return Err((format!("未找到 {} 图片验证码输入框", site_name), last_remaining_attempts));
                    }
                } else {
                    let attempts = current
                        .remaining_attempts
                        .map(|value| format!("，当前剩余 {} 次尝试", value))
                        .unwrap_or_default();
                    return Err((format!(
                        "{} 登录信息已填写{}，请人工输入图片验证码并点击登录",
                        site_name, attempts
                    ), last_remaining_attempts));
                }
            }

            let mut captcha_solved = false;
            let mut logged_challenge_msg = false;
            
            for _ in 0..120 { // 最多等待 60 秒
                if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                    return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
                }
                
                let eval_expr = r#"(() => {
                    const fields = document.querySelectorAll('textarea[name="cf-turnstile-response"], [name="g-recaptcha-response"], [name="h-captcha-response"]');
                    if (fields.length === 0) return { hasChallenge: false, solved: true };
                    let solved = false;
                    for (const f of fields) {
                        if (f.value && f.value.trim().length > 10) {
                            solved = true;
                            break;
                        }
                    }
                    return { hasChallenge: true, solved: solved };
                })()"#;
                
                let val_res = websocket.call("Runtime.evaluate", serde_json::json!({
                    "expression": eval_expr,
                    "returnByValue": true
                }));
                
                if let Ok(response) = val_res {
                    if let Some(val) = response.get("result").and_then(|r| r.get("result")).and_then(|r| r.get("value")) {
                        let has_challenge = val.get("hasChallenge").and_then(|v| v.as_bool()).unwrap_or(false);
                        let solved = val.get("solved").and_then(|v| v.as_bool()).unwrap_or(true);
                        
                        if !has_challenge || solved {
                            captcha_solved = true;
                            break;
                        }
                        
                        if !logged_challenge_msg {
                            if let Some(p) = progress {
                                p.info("检测到页面存在人机验证（如 Cloudflare Turnstile），请在浏览器中完成验证...").await;
                            }
                            logged_challenge_msg = true;
                        }
                    }
                }
                
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            if !captcha_solved {
                return Err((format!("等待 {} 人机验证通过超时", site_name), last_remaining_attempts));
            }

            let delay_min = if has_ocr { 1200 } else { 750 };
            let delay_max = if has_ocr { 2500 } else { 1550 };
            human_delay(delay_min, delay_max).await;
            if !click_runtime_element(&mut websocket, "input[type=\"submit\"][value=\"登录\"], input[type=\"submit\"][value=\"进入！\"], input[type=\"submit\"][value=\"Login\"], button[type=\"submit\"]") {
                if !click_runtime_element(&mut websocket, "input[type=\"submit\"], button") {
                    return Err((format!("未找到 {} 登录按钮", site_name), last_remaining_attempts));
                }
            }

            let mut login_success = false;
            let mut captcha_failed = false;
            for _ in 0..40 {
                if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                    return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(current) = nexus_login_page_state(&mut websocket) else {
                    continue;
                };
                if let Some(rem) = current.remaining_attempts {
                    last_remaining_attempts = Some(rem);
                }
                if current.ready && !current.challenge {
                    if current.logged_in {
                        login_success = true;
                        break;
                    }
                    if current.is_failure_page {
                        captcha_failed = true;
                        break;
                    }
                }
            }

            if login_success {
                return Ok((true, last_remaining_attempts));
            }
            if captcha_failed {
                if attempt + 1 >= max_attempts {
                    return Err((format!("{} 自动登录失败：图片验证码错误，已达到最大重试次数 {} 次", site_name, max_attempts), last_remaining_attempts));
                }
                continue;
            }
            
            let final_state = nexus_login_page_state(&mut websocket);
            if let Some(state) = final_state {
                if let Some(rem) = state.remaining_attempts {
                    last_remaining_attempts = Some(rem);
                }
                if !state.has_login_form && !state.logged_in {
                    return Err((format!("{} 自动登录失败：跳转到了非预期页面（可能是用户名或密码错误）", site_name), last_remaining_attempts));
                }
            }
            
            return Err((format!("{} 登录后仍停留在登录页，请检查凭据", site_name), last_remaining_attempts));
        }

        Err((format!("{} 自动登录失败：已尝试 {} 次均未成功", site_name, max_attempts), last_remaining_attempts))
    }

    /// 等待 Audiences 的 Cloudflare 自动验证，并填写可自动处理的登录字段。
    pub async fn login_audiences(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
        min_remaining_attempts: u32,
        ocr_config: Option<(String, u8)>,
        progress: Option<&CdpProgress>,
    ) -> Result<(bool, Option<u32>), (String, Option<u32>)> {
        self.login_nexusphp(
            tab_id,
            username,
            password,
            totp_secret,
            min_remaining_attempts,
            ocr_config,
            progress,
            "Audiences",
        )
        .await
    }

    pub async fn audiences_remaining_attempts(
        &self,
        tab_id: &str,
    ) -> Result<Option<u32>, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 Audiences 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        audiences_login_page_state(&mut websocket)
            .map(|state| state.remaining_attempts)
            .ok_or_else(|| "无法读取 Audiences 剩余登录次数".to_string())
    }

    pub async fn audiences_captcha_base64(&self, tab_id: &str) -> Result<String, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 Audiences 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let expression = r#"(() => {
            const image = document.querySelector('img[alt="CAPTCHA"], img[src*="captcha" i], img[src*="image.php" i]');
            if (!image || !image.complete || !image.naturalWidth) return { ok: false };
            try {
                const scale = 3;
                const padding = 8;
                const canvas = document.createElement('canvas');
                canvas.width = image.naturalWidth * scale + padding * 2;
                canvas.height = image.naturalHeight * scale + padding * 2;
                const context = canvas.getContext('2d', { alpha: false });
                context.fillStyle = '#ffffff';
                context.fillRect(0, 0, canvas.width, canvas.height);
                context.imageSmoothingEnabled = false;
                context.filter = 'contrast(140%) saturate(120%)';
                context.drawImage(
                    image,
                    padding,
                    padding,
                    image.naturalWidth * scale,
                    image.naturalHeight * scale
                );
                context.filter = 'none';
                return {
                    ok: true,
                    image: canvas.toDataURL('image/png').split(',')[1],
                    width: canvas.width,
                    height: canvas.height
                };
            } catch (_) {
                return { ok: false };
            }
        })()"#;
        let response = websocket
            .call(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "returnByValue": true
                }),
            )
            .map_err(|err| format!("截取验证码失败：{}", err))?;
        let value = response
            .get("result")
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("value"))
            .ok_or_else(|| "未读取到验证码图片".to_string())?;
        if !value.get("ok").and_then(|value| value.as_bool()).unwrap_or(false) {
            return Err("验证码图片尚未加载或无法截取".to_string());
        }
        value
            .get("image")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| "验证码截图数据为空".to_string())
    }

    /// 如果当前是 Audiences 图片代码无效页面，进入新的验证码登录页。
    pub async fn prepare_audiences_captcha_retry(&self, tab_id: &str) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 Audiences 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let expression = r#"(() => {
            if (document.querySelector('input[name="imagestring"]')) {
                return { ok: true, renewed: false };
            }
            const body = document.body?.innerText || '';
            if (!body.includes('图片代码无效')) return { ok: false, renewed: false };
            const link = Array.from(document.querySelectorAll('a')).find((element) =>
                (element.textContent || '').includes('获取新的图片代码')
                || (element.getAttribute('href') || '').includes('login.php')
            );
            if (!link) return { ok: false, renewed: false };
            link.click();
            return { ok: true, renewed: true };
        })()"#;
        let response = websocket
            .call(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "returnByValue": true,
                    "userGesture": true
                }),
            )
            .map_err(|err| format!("检查 Audiences 验证码页面失败：{}", err))?;
        let value = response
            .get("result")
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("value"))
            .ok_or_else(|| "无法读取 Audiences 验证码页面状态".to_string())?;
        if !value.get("ok").and_then(|value| value.as_bool()).unwrap_or(false) {
            return Err("当前页面没有可识别的验证码，也不是图片代码无效页面".to_string());
        }
        let renewed = value
            .get("renewed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if renewed {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if audiences_login_page_state(&mut websocket)
                    .is_some_and(|state| state.has_login_form && state.has_captcha)
                {
                    return Ok(true);
                }
            }
            return Err("已请求新的图片验证码，但登录页加载超时".to_string());
        }
        Ok(false)
    }

    pub async fn fill_audiences_captcha(&self, tab_id: &str, code: &str) -> Result<(), String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 Audiences 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        if type_runtime_input(
            &mut websocket,
            "input[name=\"imagestring\"]",
            code,
            70,
            145,
        )
        .await
        {
            Ok(())
        } else {
            Err("未找到 Audiences 图片验证码输入框".to_string())
        }
    }

    fn request(&self, method: &str, path: &str, timeout: Duration) -> Result<HttpResponse, String> {
        let addr = ("127.0.0.1", self.port)
            .to_socket_addrs()
            .map_err(|err| err.to_string())?
            .next()
            .ok_or_else(|| "无法解析 localhost 地址".to_string())?;
        let mut stream =
            TcpStream::connect_timeout(&addr, timeout).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;

        let request = format!(
            "{} {} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
            method, path, self.port
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;

        let raw = read_http_response(&mut stream)?;
        parse_http_response(&raw)
    }
}

struct LoginPageState {
    host: String,
    path: String,
    ready: bool,
    has_login_form: bool,
    has_otp: bool,
}

struct HdkLoginPageState {
    path: String,
    ready: bool,
    has_login_form: bool,
    has_otp: bool,
    has_captcha: bool,
    challenge: bool,
    blocked_debug: bool,
}

#[allow(dead_code)]
struct AudiencesLoginPageState {
    path: String,
    ready: bool,
    has_login_form: bool,
    has_captcha: bool,
    challenge: bool,
    remaining_attempts: Option<u32>,
}

fn audiences_login_page_state(websocket: &mut CdpWebSocket) -> Option<AudiencesLoginPageState> {
    let expression = r#"(() => {
        const username = document.querySelector('input[name="username"]');
        const password = document.querySelector('input[name="password"]');
        const submit = document.querySelector('input[type="submit"][value="登录"]');
        const captcha = document.querySelector('input[name="imagestring"]');
        const rawBody = (document.body?.innerText || '').slice(0, 5000);
        const body = rawBody.toLowerCase();
        const attempts = rawBody.match(/你还有\s*\[(\d+)\]\s*次尝试机会/);
        return {
            path: location.pathname,
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            hasLoginForm: Boolean(username && password && submit),
            hasCaptcha: Boolean(captcha && captcha.type !== 'hidden'),
            challenge: /just a moment|checking your browser|cloudflare|请稍候/i.test(document.title || '') || Boolean(document.querySelector('#challenge-stage, #challenge-running')),
            remainingAttempts: attempts ? Number(attempts[1]) : null
        };
    })()"#;
    let response = websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true
            }),
        )
        .ok()?;
    let value = response.get("result")?.get("result")?.get("value")?;
    Some(AudiencesLoginPageState {
        path: value.get("path")?.as_str()?.to_string(),
        ready: value.get("ready")?.as_bool()?,
        has_login_form: value.get("hasLoginForm")?.as_bool()?,
        has_captcha: value.get("hasCaptcha")?.as_bool()?,
        challenge: value.get("challenge")?.as_bool()?,
        remaining_attempts: value
            .get("remainingAttempts")
            .and_then(|attempts| attempts.as_u64())
            .map(|attempts| attempts as u32),
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct NexusLoginPageState {
    path: String,
    ready: bool,
    has_login_form: bool,
    has_captcha: bool,
    has_two_factor: bool,
    challenge: bool,
    logged_in: bool,
    is_failure_page: bool,
    remaining_attempts: Option<u32>,
}

fn nexus_login_page_state(websocket: &mut CdpWebSocket) -> Option<NexusLoginPageState> {
    let expression = r#"(() => {
        const username = document.querySelector('input[name="username"], input[name="email"], input[name="login"], input[autocomplete="username"]');
        const password = document.querySelector('input[name="password"], input[type="password"]');
        const captcha = document.querySelector('input[name="imagestring"]');
        const two_factor = document.querySelector('input[name="two_factor"], input[name="twofactor"], input[name="otp"], input[name="two_factor_code"], input[name="2fa_secret"], input[name="2fa"], input[autocomplete="one-time-code"]');
        const logout = document.querySelector('a[href*="logout"], a[href*="signout"]');
        const rawBody = (document.body?.innerText || '').slice(0, 5000);
        const body = rawBody.toLowerCase();
        const attempts = rawBody.match(/你还有\s*\[(\d+)\]\s*次尝试机会/);
        const isFailurePage = rawBody.includes('图片代码无效') || rawBody.includes('验证码错误') || rawBody.includes('验证码无效') || rawBody.includes('验证码不正确') || location.pathname.includes('takelogin.php');
        return {
            path: location.pathname,
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            hasLoginForm: Boolean(username && password),
            hasCaptcha: Boolean(captcha && captcha.type !== 'hidden'),
            hasTwoFactor: Boolean(two_factor),
            challenge: /just a moment|checking your browser|cloudflare|请稍候/i.test(document.title || '') || Boolean(document.querySelector('#challenge-stage, #challenge-running')),
            loggedIn: Boolean(logout) || body.includes('分享率') || body.includes('上传') || body.includes('上傳') || body.includes('下载') || body.includes('下載') || body.includes('魔力') || body.includes('ratio') || body.includes('uploaded') || body.includes('downloaded'),
            isFailurePage: Boolean(isFailurePage),
            remainingAttempts: attempts ? Number(attempts[1]) : null
        };
    })()"#;
    let response = websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true
            }),
        )
        .ok()?;
    let value = response.get("result")?.get("result")?.get("value")?;
    Some(NexusLoginPageState {
        path: value.get("path")?.as_str()?.to_string(),
        ready: value.get("ready")?.as_bool()?,
        has_login_form: value.get("hasLoginForm")?.as_bool()?,
        has_captcha: value.get("hasCaptcha")?.as_bool()?,
        has_two_factor: value.get("hasTwoFactor")?.as_bool()?,
        challenge: value.get("challenge")?.as_bool()?,
        logged_in: value.get("loggedIn")?.as_bool()?,
        is_failure_page: value.get("isFailurePage")?.as_bool()?,
        remaining_attempts: value
            .get("remainingAttempts")
            .and_then(|attempts| attempts.as_u64())
            .map(|attempts| attempts as u32),
    })
}

fn hdk_login_page_state(websocket: &mut CdpWebSocket) -> Option<HdkLoginPageState> {
    let expression = r#"(() => {
        const inputs = Array.from(document.querySelectorAll('input'));
        const hints = (input) => [input.id, input.name, input.placeholder, input.autocomplete]
            .filter(Boolean).join(' ').toLowerCase();
        const username = document.querySelector('#username, input[name="username"], input[autocomplete="username"]');
        const password = document.querySelector('#password, input[name="password"], input[type="password"]');
        const submit = document.querySelector('button[type="submit"], input[type="submit"], input[name="login"]');
        const body = (document.body?.innerText || '').slice(0, 5000).toLowerCase();
        return {
            path: location.pathname,
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            hasLoginForm: Boolean(username && password && submit),
            hasOtp: inputs.some((input) => /(otp|totp|2fa|auth.*code|verification.*code|驗證碼|验证码)/.test(hints(input))),
            hasCaptcha: inputs.some((input) => /(imagestring|captcha)/.test(hints(input))),
            challenge: /(just a moment|checking your browser|cloudflare|安全检测|雷池|客户端异常)/.test(body),
            blockedDebug: /(当前环境正在被调试|environment is being debugged)/.test(body)
        };
    })()"#;
    let response = websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true
            }),
        )
        .ok()?;
    let value = response.get("result")?.get("result")?.get("value")?;
    Some(HdkLoginPageState {
        path: value.get("path")?.as_str()?.to_string(),
        ready: value.get("ready")?.as_bool()?,
        has_login_form: value.get("hasLoginForm")?.as_bool()?,
        has_otp: value.get("hasOtp")?.as_bool()?,
        has_captcha: value.get("hasCaptcha")?.as_bool()?,
        challenge: value.get("challenge")?.as_bool()?,
        blocked_debug: value.get("blockedDebug")?.as_bool()?,
    })
}

async fn human_delay(min_millis: u64, max_millis: u64) {
    let delay = rand::thread_rng().gen_range(min_millis..=max_millis);
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

async fn type_runtime_input(
    websocket: &mut CdpWebSocket,
    selector: &str,
    value: &str,
    min_key_delay: u64,
    max_key_delay: u64,
) -> bool {
    if !fill_runtime_input(websocket, selector, "") {
        return false;
    }
    let mut typed = String::new();
    for (index, character) in value.chars().enumerate() {
        typed.push(character);
        if !fill_runtime_input(websocket, selector, &typed) {
            return false;
        }
        human_delay(min_key_delay, max_key_delay).await;
        if index > 0 && index % 6 == 0 {
            human_delay(120, 320).await;
        }
    }
    true
}

fn fill_runtime_input(websocket: &mut CdpWebSocket, selector: &str, value: &str) -> bool {
    let selector_json = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let value_json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    let expression = format!(
        r#"(() => {{
            const input = document.querySelector({selector});
            if (!input) return {{ ok: false }};
            input.focus();
            const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
            setter.call(input, {value});
            input.dispatchEvent(new Event('input', {{ bubbles: true }}));
            input.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return {{ ok: true }};
        }})()"#,
        selector = selector_json,
        value = value_json,
    );
    runtime_object_bool(websocket, &expression, "ok").unwrap_or(false)
}

fn click_runtime_element(websocket: &mut CdpWebSocket, selector: &str) -> bool {
    let selector_json = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let expression = format!(
        r#"(() => {{
            const element = document.querySelector({selector});
            if (!element) return {{ ok: false }};
            element.focus();
            element.click();
            return {{ ok: true }};
        }})()"#,
        selector = selector_json,
    );
    runtime_object_bool(websocket, &expression, "ok").unwrap_or(false)
}

fn login_page_state(websocket: &mut CdpWebSocket) -> Option<LoginPageState> {
    let expression = r#"(() => {
        const inputs = Array.from(document.querySelectorAll('input'));
        const hasOtp = inputs.some((input) => {
            const hint = [input.id, input.name, input.placeholder, input.autocomplete]
                .filter(Boolean).join(' ').toLowerCase();
            return /(otp|totp|2fa|auth.*code|verification.*code|驗證碼|验证码)/.test(hint);
        });
        return {
            host: location.hostname.toLowerCase(),
            path: location.pathname,
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            hasLoginForm: Boolean(document.querySelector('#username')
                && document.querySelector('#password')
                && document.querySelector('button[type="submit"]')),
            hasOtp
        };
    })()"#;
    let response = websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true
            }),
        )
        .ok()?;
    let value = response.get("result")?.get("result")?.get("value")?;
    Some(LoginPageState {
        host: value.get("host")?.as_str()?.to_string(),
        path: value.get("path")?.as_str()?.to_string(),
        ready: value.get("ready")?.as_bool()?,
        has_login_form: value.get("hasLoginForm")?.as_bool()?,
        has_otp: value.get("hasOtp")?.as_bool()?,
    })
}

fn runtime_object_bool(websocket: &mut CdpWebSocket, expression: &str, key: &str) -> Option<bool> {
    websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true
            }),
        )
        .ok()?
        .get("result")?
        .get("result")?
        .get("value")?
        .get(key)?
        .as_bool()
}

async fn submit_otp(websocket: &mut CdpWebSocket, code: &str) -> bool {
    let locate_expression = r#"(() => {
            const input = Array.from(document.querySelectorAll('input')).find((element) => {
                const hint = [element.id, element.name, element.placeholder, element.autocomplete]
                    .filter(Boolean).join(' ').toLowerCase();
                return /(otp|totp|2fa|auth.*code|verification.*code|驗證碼|验证码)/.test(hint);
            });
            if (!input) return { ok: false };
            input.setAttribute('data-pt-manager-otp', 'true');
            return { ok: true };
        })()"#;
    if !runtime_object_bool(websocket, locate_expression, "ok").unwrap_or(false) {
        return false;
    }
    if !type_runtime_input(
        websocket,
        "input[data-pt-manager-otp=\"true\"]",
        code,
        70,
        145,
    )
    .await
    {
        return false;
    }
    human_delay(550, 1200).await;
    let submit_expression = r#"(() => {
            const input = document.querySelector('input[data-pt-manager-otp="true"]');
            if (!input) return { ok: false };
            const form = input.closest('form');
            const submit = form?.querySelector('button[type="submit"]')
                || document.querySelector('.ant-modal button.ant-btn-primary')
                || document.querySelector('button[type="submit"]');
            if (!submit) return { ok: false };
            submit.click();
            return { ok: true };
        })()"#;
    runtime_object_bool(websocket, submit_expression, "ok").unwrap_or(false)
}

fn set_dom_storage_item(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
    item: &CdpLocalStorageEntry,
) -> bool {
    for storage_id in dom_storage_ids(websocket, storage) {
        let dom_storage_params = serde_json::json!({
            "storageId": storage_id,
            "key": &item.name,
            "value": &item.value
        });
        if websocket
            .call("DOMStorage.setDOMStorageItem", dom_storage_params)
            .is_ok()
            && local_storage_item_matches(websocket, storage, item)
        {
            return true;
        }
    }

    false
}

fn set_local_storage_items(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
    items: &[CdpLocalStorageEntry],
) -> Vec<CdpLocalStorageEntry> {
    let mut imported = runtime_set_local_storage_items(websocket, storage, items);
    let missing_items = items
        .iter()
        .filter(|item| !imported.iter().any(|written| written.name == item.name))
        .cloned()
        .collect::<Vec<_>>();

    imported.extend(
        missing_items
            .iter()
            .filter(|item| set_dom_storage_item(websocket, storage, item))
            .cloned(),
    );
    imported.sort_by(|a, b| a.name.cmp(&b.name));
    imported.dedup_by(|a, b| a.name == b.name);
    imported
}

async fn ensure_storage_page_ready(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
) {
    if wait_for_storage_host_ready(websocket, storage).await {
        return;
    }

    let _ = websocket.call("Page.enable", serde_json::json!({}));
    let _ = websocket.call(
        "Page.navigate",
        serde_json::json!({
            "url": storage.origin
        }),
    );
    let _ = wait_for_storage_host_ready(websocket, storage).await;
}

async fn wait_for_storage_host_ready(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
) -> bool {
    for _ in 0..20 {
        if page_is_storage_host_ready(websocket, storage) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

fn page_is_storage_host_ready(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
) -> bool {
    let Some(location) = current_page_location(websocket) else {
        return false;
    };
    location.host == storage.host
        && (location.ready_state == "interactive" || location.ready_state == "complete")
}

fn current_page_location(websocket: &mut CdpWebSocket) -> Option<PageLocation> {
    let response = websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "({ host: location.hostname.toLowerCase(), readyState: document.readyState })",
                "returnByValue": true
            }),
        )
        .ok()?;
    let value = response
        .get("result")?
        .get("result")?
        .get("value")?;
    Some(PageLocation {
        host: value.get("host")?.as_str()?.to_string(),
        ready_state: value.get("readyState")?.as_str()?.to_string(),
    })
}

struct PageLocation {
    host: String,
    ready_state: String,
}

fn runtime_set_local_storage_items(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
    items: &[CdpLocalStorageEntry],
) -> Vec<CdpLocalStorageEntry> {
    let entries = items
        .iter()
        .map(|item| {
            serde_json::json!({
                "name": &item.name,
                "value": &item.value
            })
        })
        .collect::<Vec<_>>();
    let entries_json = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
    let target_host_json =
        serde_json::to_string(&storage.host).unwrap_or_else(|_| "\"\"".to_string());
    let expression = format!(
        r#"(() => {{
            const targetHost = {target_host};
            const entries = {entries};
            const written = [];
            if (location.hostname.toLowerCase() !== targetHost) {{
                return {{ host: location.hostname.toLowerCase(), written }};
            }}
            for (const item of entries) {{
                try {{
                    localStorage.setItem(item.name, item.value);
                    if (localStorage.getItem(item.name) === item.value) {{
                        written.push(item.name);
                    }}
                }} catch (_) {{}}
            }}
            return {{ host: location.hostname.toLowerCase(), written }};
        }})()"#,
        target_host = target_host_json,
        entries = entries_json
    );
    let params = serde_json::json!({
        "expression": expression,
        "returnByValue": true,
        "awaitPromise": true,
        "userGesture": true,
        "allowUnsafeEvalBlockedByCSP": true
    });
    let written_names = websocket
        .call("Runtime.evaluate", params)
        .ok()
        .and_then(runtime_written_names)
        .unwrap_or_default();
    items
        .iter()
        .filter(|item| written_names.iter().any(|name| name == &item.name))
        .cloned()
        .collect()
}

fn local_storage_item_matches(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
    item: &CdpLocalStorageEntry,
) -> bool {
    // 优先用 DOMStorage 直接读回目标 origin，避免页面脚本上下文未就绪或被站点重定向时误判写入失败。
    dom_storage_item_matches(websocket, storage, item)
        || runtime_local_storage_item_matches(websocket, item)
}

fn dom_storage_id(storage: &CdpLocalStorageParam) -> serde_json::Value {
    serde_json::json!({
        "securityOrigin": storage.origin,
        "isLocalStorage": true
    })
}

fn dom_storage_ids(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
) -> Vec<serde_json::Value> {
    let mut ids = Vec::new();
    if let Some(storage_key) = current_storage_key(websocket) {
        ids.push(serde_json::json!({
            "storageKey": storage_key,
            "isLocalStorage": true
        }));
    }
    ids.push(dom_storage_id(storage));
    ids.push(serde_json::json!({
        "storageKey": format!("{}/", storage.origin.trim_end_matches('/')),
        "isLocalStorage": true
    }));

    let mut unique = Vec::new();
    for id in ids {
        if !unique.iter().any(|existing| existing == &id) {
            unique.push(id);
        }
    }
    unique
}

fn current_storage_key(websocket: &mut CdpWebSocket) -> Option<String> {
    websocket
        .call("Storage.getStorageKey", serde_json::json!({}))
        .ok()
        .and_then(|response| {
            response
                .get("result")
                .and_then(|value| value.get("storageKey"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn dom_storage_item_matches(
    websocket: &mut CdpWebSocket,
    storage: &CdpLocalStorageParam,
    item: &CdpLocalStorageEntry,
) -> bool {
    dom_storage_ids(websocket, storage).into_iter().any(|storage_id| {
        let params = serde_json::json!({
            "storageId": storage_id
        });
        websocket
            .call("DOMStorage.getDOMStorageItems", params)
            .ok()
            .and_then(dom_storage_response_has_item(item))
            .unwrap_or(false)
    })
}

fn runtime_local_storage_item_matches(
    websocket: &mut CdpWebSocket,
    item: &CdpLocalStorageEntry,
) -> bool {
    let params = serde_json::json!({
        "expression": format!(
            "localStorage.getItem({})",
            serde_json::to_string(&item.name).unwrap_or_else(|_| "\"\"".to_string())
        ),
        "returnByValue": true
    });
    websocket
        .call("Runtime.evaluate", params)
        .ok()
        .and_then(|response| {
            response
                .get("result")
                .and_then(|value| value.get("result"))
                .and_then(|value| value.get("value"))
                .and_then(|value| value.as_str())
                .map(|value| value == item.value)
        })
        .unwrap_or(false)
}

fn runtime_written_names(response: serde_json::Value) -> Option<Vec<String>> {
    if response
        .get("result")
        .and_then(|value| value.get("exceptionDetails"))
        .is_some()
    {
        return None;
    }
    response
        .get("result")?
        .get("result")?
        .get("value")?
        .get("written")?
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
}

fn dom_storage_response_has_item(
    item: &CdpLocalStorageEntry,
) -> impl FnOnce(serde_json::Value) -> Option<bool> + '_ {
    |response| {
        response
            .get("result")
            .and_then(|value| value.get("entries"))
            .and_then(|value| value.as_array())
            .map(|entries| {
                entries.iter().any(|entry| {
                    let Some(pair) = entry.as_array() else {
                        return false;
                    };
                    let key = pair.first().and_then(|value| value.as_str());
                    let value = pair.get(1).and_then(|value| value.as_str());
                    key == Some(item.name.as_str()) && value == Some(item.value.as_str())
                })
            })
    }
}

pub fn chrome_installed() -> bool {
    find_chrome_executable().is_some()
}

pub fn clear_dedicated_profile_data() -> Result<usize, String> {
    let profile_dir = dedicated_profile_dir();
    if profile_dir_in_use(&profile_dir) {
        return Err("专用 Chrome 正在运行，请关闭专用 Chrome 后再清除离线浏览器数据".to_string());
    }
    if !profile_dir.exists() {
        return Ok(0);
    }

    fs::remove_dir_all(&profile_dir)
        .map_err(|err| format!("清除专用 Chrome Profile 失败：{}", err))?;
    Ok(1)
}

fn read_http_response(stream: &mut TcpStream) -> Result<String, String> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];

    // Chrome 的 CDP HTTP 端口通常会返回 Content-Length，但不保证立刻关闭连接。
    // 因此不能用 read_to_string 等 EOF，而要按响应头声明的长度读完整个 body。
    let header_end = loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|err| format!("读取 CDP HTTP 响应失败：{}", err))?;
        if read == 0 {
            break header_end_index(&raw).ok_or_else(|| "CDP HTTP 响应缺少响应头".to_string())?;
        }

        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > MAX_CDP_HTTP_RESPONSE_BYTES {
            return Err("CDP HTTP 响应过大".to_string());
        }

        if let Some(index) = header_end_index(&raw) {
            break index;
        }
    };

    let headers = std::str::from_utf8(&raw[..header_end])
        .map_err(|err| format!("CDP HTTP 响应头不是 UTF-8：{}", err))?;
    if let Some(content_length) = content_length_from_headers(headers) {
        let expected_len = header_end + content_length;
        while raw.len() < expected_len {
            let read = stream
                .read(&mut buffer)
                .map_err(|err| format!("读取 CDP HTTP 响应体失败：{}", err))?;
            if read == 0 {
                return Err("CDP HTTP 响应体提前结束".to_string());
            }

            raw.extend_from_slice(&buffer[..read]);
            if raw.len() > MAX_CDP_HTTP_RESPONSE_BYTES {
                return Err("CDP HTTP 响应过大".to_string());
            }
        }
        raw.truncate(expected_len);
    }

    String::from_utf8(raw).map_err(|err| format!("CDP HTTP 响应不是 UTF-8：{}", err))
}

fn header_end_index(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn content_length_from_headers(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn parse_http_response(raw: &str) -> Result<HttpResponse, String> {
    let mut parts = raw.splitn(2, "\r\n\r\n");
    let headers = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default().to_string();
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "无法解析 CDP HTTP 响应".to_string())?;

    Ok(HttpResponse { status, body })
}

struct CdpWebSocket {
    stream: TcpStream,
    next_id: u64,
}

impl CdpWebSocket {
    fn connect(url: &str, timeout: Duration) -> Result<Self, String> {
        let endpoint = parse_ws_url(url)?;
        let addr = (endpoint.host.as_str(), endpoint.port)
            .to_socket_addrs()
            .map_err(|err| err.to_string())?
            .next()
            .ok_or_else(|| "无法解析 CDP WebSocket 地址".to_string())?;
        let mut stream =
            TcpStream::connect_timeout(&addr, timeout).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;

        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: cHRtYW5hZ2VyY2RwMTIzNA==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            endpoint.path, endpoint.host, endpoint.port
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;
        let headers = read_websocket_headers(&mut stream)?;
        if !headers.starts_with("HTTP/1.1 101") {
            return Err("CDP WebSocket 握手失败".to_string());
        }

        Ok(Self { stream, next_id: 1 })
    }

    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_frame(0x1, request.to_string().as_bytes())?;

        loop {
            let message = self.read_message()?;
            let value = serde_json::from_str::<serde_json::Value>(&message)
                .map_err(|err| format!("CDP WebSocket 响应解析失败：{}", err))?;
            if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(format!("CDP 方法 {} 调用失败：{}", method, error));
                }
                return Ok(value);
            }
        }
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }

        let mask = [0x13, 0x57, 0x9b, 0xdf];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| *byte ^ mask[index % mask.len()]),
        );
        self.stream
            .write_all(&frame)
            .map_err(|err| format!("发送 CDP WebSocket 帧失败：{}", err))
    }

    fn read_message(&mut self) -> Result<String, String> {
        loop {
            let mut header = [0_u8; 2];
            self.stream
                .read_exact(&mut header)
                .map_err(|err| format!("读取 CDP WebSocket 帧失败：{}", err))?;
            let opcode = header[0] & 0x0f;
            let masked = header[1] & 0x80 != 0;
            let mut len = (header[1] & 0x7f) as u64;
            if len == 126 {
                let mut bytes = [0_u8; 2];
                self.stream
                    .read_exact(&mut bytes)
                    .map_err(|err| err.to_string())?;
                len = u16::from_be_bytes(bytes) as u64;
            } else if len == 127 {
                let mut bytes = [0_u8; 8];
                self.stream
                    .read_exact(&mut bytes)
                    .map_err(|err| err.to_string())?;
                len = u64::from_be_bytes(bytes);
            }

            let mut mask = [0_u8; 4];
            if masked {
                self.stream
                    .read_exact(&mut mask)
                    .map_err(|err| err.to_string())?;
            }

            let mut payload = vec![0_u8; len as usize];
            self.stream
                .read_exact(&mut payload)
                .map_err(|err| err.to_string())?;
            if masked {
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % mask.len()];
                }
            }

            match opcode {
                0x1 => {
                    return String::from_utf8(payload)
                        .map_err(|err| format!("CDP WebSocket 文本不是 UTF-8：{}", err));
                }
                0x8 => return Err("CDP WebSocket 已关闭".to_string()),
                0x9 => self.write_frame(0xA, &payload)?,
                _ => {}
            }
        }
    }
}

struct WsEndpoint {
    host: String,
    port: u16,
    path: String,
}

fn parse_ws_url(url: &str) -> Result<WsEndpoint, String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| "仅支持本地 ws:// CDP 地址".to_string())?;
    let (host_port, path) = rest
        .split_once('/')
        .ok_or_else(|| "CDP WebSocket 地址缺少路径".to_string())?;
    let (host, port) = parse_ws_host_port(host_port)?;
    let normalized_host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if normalized_host != "127.0.0.1"
        && normalized_host != "localhost"
        && normalized_host != "::1"
    {
        return Err("只允许连接本机 CDP WebSocket".to_string());
    }

    Ok(WsEndpoint {
        // Chrome 启动时绑定 127.0.0.1；localhost 在 Windows 上可能先解析到 ::1，导致写 Cookie 时 10061。
        host: "127.0.0.1".to_string(),
        port,
        path: format!("/{}", path),
    })
}

fn parse_ws_host_port(host_port: &str) -> Result<(String, u16), String> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| "CDP WebSocket IPv6 地址格式无效".to_string())?;
        return Ok((
            host.to_string(),
            port.parse::<u16>()
                .map_err(|err| format!("CDP WebSocket 端口无效：{}", err))?,
        ));
    }

    match host_port.rsplit_once(':') {
        Some((host, port)) => Ok((
            host.to_string(),
            port.parse::<u16>()
                .map_err(|err| format!("CDP WebSocket 端口无效：{}", err))?,
        )),
        None => Ok((host_port.to_string(), 80)),
    }
}

fn read_websocket_headers(stream: &mut TcpStream) -> Result<String, String> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|err| format!("读取 CDP WebSocket 握手响应失败：{}", err))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if header_end_index(&raw).is_some() {
            break;
        }
        if raw.len() > 64 * 1024 {
            return Err("CDP WebSocket 握手响应过大".to_string());
        }
    }

    String::from_utf8(raw).map_err(|err| format!("CDP WebSocket 握手响应不是 UTF-8：{}", err))
}

fn encode_cdp_target_url(value: &str) -> String {
    let mut encoded = String::new();

    // /json/new 把整个 query 当作目标 URL，不能把 : / ? & 这些 URL 分隔符全部转义。
    for byte in value.trim().as_bytes() {
        match byte {
            b'\t' | b'\n' | b'\r' | b' ' | b'"' | b'<' | b'>' | b'`' => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
            0x00..=0x1F | 0x7F..=0xFF => encoded.push_str(&format!("%{:02X}", byte)),
            _ => encoded.push(*byte as char),
        }
    }

    encoded
}

fn unique_urls(urls: &[String]) -> Vec<String> {
    let mut result = Vec::new();

    for url in urls
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if result.iter().any(|existing| existing == url) {
            continue;
        }
        result.push(url.to_string());
    }

    result
}

fn launch_urls(urls: &[String]) -> Vec<String> {
    let urls = unique_urls(urls);
    if urls.is_empty() {
        vec!["about:blank".to_string()]
    } else {
        urls
    }
}

async fn launch_and_wait(
    profile_dir: &Path,
    recovery: bool,
    initial_urls: &[String],
    fixed_port: Option<u16>,
    progress: Option<&CdpProgress>,
) -> Result<Option<CdpLaunchResult>, String> {
    let launch_urls = launch_urls(initial_urls);
    let mode = if recovery {
        "备用专用 Chrome"
    } else {
        "专用 Chrome"
    };
    log_progress(
        progress,
        format!(
            "准备启动{}，Profile：{}，端口：{}，初始页面 {} 个",
            mode,
            profile_dir.display(),
            fixed_port
                .map(|port| format!("localhost:{}", port))
                .unwrap_or_else(|| "随机".to_string()),
            launch_urls.len()
        ),
    )
    .await;
    let _ = fs::remove_file(devtools_port_path(profile_dir));
    launch_chrome(profile_dir, &launch_urls, fixed_port)?;
    log_progress(progress, format!("{} 进程已启动，等待 CDP 响应", mode)).await;

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut attempt = 0;
    while Instant::now() < deadline {
        attempt += 1;
        check_cancel(progress)?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let port = if let Some(port) = fixed_port {
            port
        } else {
            let Some(port) = read_devtools_port(profile_dir) else {
                if attempt % 4 == 0 {
                    log_progress(
                        progress,
                        format!("等待 Chrome 写入 CDP 端口... 已等待 {} 秒", attempt / 2),
                    )
                    .await;
                }
                continue;
            };
            port
        };

        if fixed_port.is_none() {
            log_progress(
                progress,
                format!("读取到 CDP 端口 localhost:{}，正在检测响应", port),
            )
            .await;
        }
        let cdp = CdpClient::new(port);
        if cdp
            .is_available_with_timeout(Duration::from_millis(800))
            .await
        {
            check_cancel(progress)?;
            log_progress(progress, format!("CDP localhost:{} 已响应", port)).await;
            let opened_initial_urls = unique_urls(initial_urls).len();
            if opened_initial_urls > 0 {
                log_progress(
                    progress,
                    format!("Chrome 启动参数已打开 {} 个初始站点", opened_initial_urls),
                )
                .await;
            }
            let prefix = if recovery { "已启动备用专用调试 Chrome" } else { "已启动专用调试 Chrome" };
            let message = connected_message(prefix, port, opened_initial_urls);
            return Ok(Some(CdpLaunchResult {
                port,
                message,
                opened_initial_urls,
            }));
        }

        if attempt % 4 == 0 {
            log_progress(progress, format!("端口 localhost:{} 尚未响应 CDP", port)).await;
        }
    }

    log_progress(progress, format!("{} 在 15 秒内未提供可用 CDP", mode)).await;
    Ok(None)
}

fn launch_chrome(profile_dir: &Path, urls: &[String], fixed_port: Option<u16>) -> Result<(), String> {
    let chrome_path = find_chrome_executable()
        .ok_or_else(|| "未检测到 Google Chrome。请在总览点击“安装 Chrome”，安装完成后再重试。".to_string())?;

    let mut command = Command::new(chrome_path);
    fs::create_dir_all(profile_dir)
        .map_err(|err| format!("创建 Chrome 专用 Profile 失败：{}", err))?;

    command
        .arg(format!(
            "--remote-debugging-port={}",
            fixed_port.unwrap_or(0)
        ))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--new-window");

    for url in urls {
        command.arg(url);
    }

    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("自动启动 Chrome 失败：{}", err))
}

fn dedicated_profile_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(root).join("pt-manager\\chrome-cdp-profile-auto");
        }
    }

    env::temp_dir().join("pt-manager-chrome-cdp-profile-auto")
}

fn recovery_profile_dir() -> PathBuf {
    env::temp_dir().join(format!(
        "pt-manager-chrome-cdp-recovery-{}",
        std::process::id()
    ))
}

fn devtools_port_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("DevToolsActivePort")
}

fn read_devtools_port(profile_dir: &Path) -> Option<u16> {
    let data = fs::read_to_string(devtools_port_path(profile_dir)).ok()?;
    data.lines().next()?.trim().parse::<u16>().ok()
}

fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn profile_dir_in_use(profile_dir: &Path) -> bool {
    ["SingletonLock", "SingletonCookie", "SingletonSocket"]
        .iter()
        .any(|name| profile_dir.join(name).exists())
}

fn connected_message(prefix: &str, port: u16, opened_initial_urls: usize) -> String {
    match opened_initial_urls {
        0 => format!("{}：localhost:{}", prefix, port),
        1 => format!(
            "{}：localhost:{}，已打开 1 个站点。首次使用请在该窗口登录站点。",
            prefix, port
        ),
        count => format!(
            "{}：localhost:{}，已打开 {} 个站点。首次使用请在该窗口登录站点。",
            prefix, port, count
        ),
    }
}

async fn log_progress(progress: Option<&CdpProgress>, message: impl Into<String>) {
    if let Some(progress) = progress {
        progress.info(message).await;
    }
}

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress
        .map(|progress| progress.is_cancelled())
        .unwrap_or(false)
    {
        Err(CDP_CANCELLED.to_string())
    } else {
        Ok(())
    }
}

fn host_from_url(url: &str) -> Option<String> {
    let without_scheme = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let host = without_scheme
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

fn origin_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else {
        return None;
    };
    let host_port = rest.split(['/', '?', '#']).next()?.trim();
    if host_port.is_empty() {
        None
    } else {
        Some(format!("{}://{}", scheme, host_port.to_ascii_lowercase()))
    }
}

fn unique_origins(urls: &[String]) -> Vec<String> {
    let mut origins = Vec::new();
    for origin in urls.iter().filter_map(|url| origin_from_url(url)) {
        if !origins.iter().any(|existing| existing == &origin) {
            origins.push(origin);
        }
    }
    origins
}

fn find_chrome_executable() -> Option<PathBuf> {
    chrome_candidates().into_iter().find_map(|path| {
        if path.exists() {
            return Some(path);
        }
        find_in_path(&path)
    })
}

fn chrome_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(value) = env::var("CHROME") {
        paths.push(PathBuf::from(value));
    }

    #[cfg(target_os = "windows")]
    {
        for key in ["LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)"] {
            if let Ok(root) = env::var(key) {
                paths.push(PathBuf::from(root).join("Google\\Chrome\\Application\\chrome.exe"));
            }
        }
        paths.push(PathBuf::from("chrome.exe"));
    }

    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
    }

    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from("google-chrome"));
        paths.push(PathBuf::from("google-chrome-stable"));
        paths.push(PathBuf::from("chromium"));
        paths.push(PathBuf::from("chromium-browser"));
    }

    paths
}

fn find_in_path(command: &PathBuf) -> Option<PathBuf> {
    let file_name = command.file_name()?;
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(file_name))
        .find(|path| path.exists())
}
