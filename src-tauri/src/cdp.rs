use crate::store::{self, AppConfig, BrowserEnvironment, BrowserKind, LogEntry};
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

// 登录适配器作为 CDP 的子模块，可以复用内部 WebSocket，同时不向业务层暴露传输细节。
mod environment;
mod jying;
mod login;
mod qingwa_bonus;
mod proxy_check;
mod signin;
mod traffic;

pub use login::{LoginRequest, LoginState, SiteAdapter};
pub use qingwa_bonus::is_qingwa_site;
pub use signin::{SigninResult, SigninStatus};
pub use traffic::SiteTraffic;

const MAX_CDP_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
// 导航可能等待站点响应或重定向，不能沿用本机 CDP 连接的 5～10 秒超时。
const CDP_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(90);

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

#[derive(Clone)]
pub struct CdpClient {
    port: u16,
    browser: BrowserKind,
    // 外层 None 表示只做 CDP 操作；Some 表示必须校验浏览器当前的代理参数。
    proxy_address: Option<Option<String>>,
    profile_group: Option<String>,
    require_managed_profile: bool,
    environment: BrowserEnvironment,
}

pub struct CdpLaunchResult {
    pub port: u16,
    pub message: String,
    pub opened_initial_urls: usize,
    pub launched: bool,
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

    async fn ocr(&self, message: impl Into<String>, image: &str, recognized_text: Option<&str>) {
        store::push_ocr_log(&self.logs, LogEntry::info(message), image, recognized_text).await;
    }
}

impl CdpClient {
    pub fn new(port: u16) -> Self {
        Self::with_browser(port, BrowserKind::Chrome)
    }

    pub fn with_browser(port: u16, browser: BrowserKind) -> Self {
        Self {
            port,
            browser,
            proxy_address: None,
            profile_group: None,
            require_managed_profile: false,
            environment: BrowserEnvironment::default(),
        }
    }

    pub fn for_profile(config: &AppConfig) -> Self {
        Self {
            profile_group: config.profile_group.clone(),
            require_managed_profile: config.profile_group.is_some()
                || !config.site_groups.is_empty()
                || !config.browser_environment.is_default(),
            environment: config.browser_environment.clone(),
            ..Self::with_browser(config.cdp_port, config.browser)
        }
    }

    pub fn with_config(config: &AppConfig) -> Result<Self, String> {
        let address = crate::browser_proxy::ensure_address(&config.browser_proxy, config.profile_group.as_deref())?;
        Ok(Self { proxy_address: Some(address), ..Self::for_profile(config) })
    }

    pub fn for_status(config: &AppConfig) -> Option<Self> {
        let address = if config.browser_proxy.enabled {
            Some(crate::browser_proxy::active_address(&config.browser_proxy, config.profile_group.as_deref())?)
        } else {
            None
        };
        Some(Self { proxy_address: Some(address), ..Self::for_profile(config) })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn at_port(&self, port: u16) -> Self {
        Self { port, ..self.clone() }
    }

    fn primary_profile(&self) -> PathBuf {
        let default = dedicated_profile_dir(self.browser);
        if let Some(id) = &self.profile_group {
            let mut name = default.file_name().unwrap_or_default().to_os_string();
            name.push(format!("-group-{id}"));
            default.with_file_name(name)
        } else {
            default
        }
    }

    fn profiles(&self) -> Vec<PathBuf> {
        let primary = self.primary_profile();
        if self.profile_group.is_some() {
            vec![primary]
        } else {
            vec![primary, recovery_profile_dir(self.browser)]
        }
    }

    /// 只复用所选浏览器的 CDP，避免端口相同时接入另一种浏览器。
    pub async fn available_port(&self) -> Option<u16> {
        if self
            .is_available_with_timeout(Duration::from_millis(600))
            .await
        {
            self.prepare_environment().ok()?;
            return Some(self.port);
        }

        if self.profile_group.is_some() {
            return None;
        }
        for profile_dir in self.profiles() {
            let Some(port) = read_devtools_port(&profile_dir) else {
                continue;
            };
            let candidate = Self { port, ..self.clone() };
            if candidate
                .is_available_with_timeout(Duration::from_millis(600))
                .await
            {
                candidate.prepare_environment().ok()?;
                return Some(port);
            }
        }

        None
    }

    /// 检测所选浏览器是否以调试模式运行。
    pub async fn is_available(&self) -> bool {
        self.is_available_with_timeout(Duration::from_secs(3)).await
    }

    fn prepare_environment(&self) -> Result<(), String> {
        if self.environment.is_default() {
            return Ok(());
        }
        environment::ensure(self.port, &self.browser_session()?, &self.environment)
    }

    async fn is_available_with_timeout(&self, timeout: Duration) -> bool {
        self.browser_version(timeout).is_some_and(|version| {
            (!self.require_managed_profile
                || proxy_check::uses_managed_profile(&version, &self.profiles(), timeout)
                    .unwrap_or(false))
                && self.proxy_matches(&version, timeout).unwrap_or(false)
        })
    }

    fn browser_version(&self, timeout: Duration) -> Option<serde_json::Value> {
        self.request("GET", "/json/version", timeout)
            .ok()
            .and_then(|response| {
                if !(200..300).contains(&response.status) {
                    return None;
                }
                let value = serde_json::from_str::<serde_json::Value>(&response.body).ok()?;
                let product = value
                    .get("Browser")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let browser_matches = match self.browser {
                    BrowserKind::Chrome => {
                        product.starts_with("Chrome/") || product.starts_with("Chromium/")
                    }
                    BrowserKind::Edge => product.starts_with("Edg/"),
                };
                browser_matches.then_some(value)
            })
    }

    fn proxy_matches(&self, version: &serde_json::Value, timeout: Duration) -> Result<bool, String> {
        let Some(expected) = &self.proxy_address else { return Ok(true) };
        let require_direct = self.profile_group.is_some() && expected.is_none();
        proxy_check::matches(version, expected.as_deref(), require_direct, timeout)
    }

    /// 在复用前确认代理参数，不能因已有 CDP 连接而悄悄使用另一个网络出口。
    async fn validate_existing_proxy(&self) -> Result<(), String> {
        if self.proxy_address.is_none() { return Ok(()) }
        let mut ports = vec![self.port];
        for profile in self.profiles() {
            if let Some(port) = read_devtools_port(&profile) { ports.push(port); }
        }
        ports.sort_unstable();
        ports.dedup();
        for port in ports {
            let raw = Self::with_browser(port, self.browser);
            if !raw.is_available_with_timeout(Duration::from_millis(600)).await { continue; }
            let response = raw.request("GET", "/json/version", Duration::from_secs(2))?;
            let version = serde_json::from_str(&response.body).map_err(|err| format!("读取浏览器信息失败：{err}"))?;
            if self.require_managed_profile
                && !proxy_check::uses_managed_profile(&version, &self.profiles(), Duration::from_secs(2))?
            {
                continue;
            }
            let matches = self.proxy_matches(&version, Duration::from_secs(2))
                .map_err(|err| format!("{} 代理配置检查失败：{err}", self.browser.name()))?;
            if !matches {
                return Err(format!("{} 已运行，但代理设置与当前配置不匹配。请关闭专用浏览器后重试，程序会按新代理设置启动", self.browser.name()));
            }
        }
        Ok(())
    }

    /// 确保所选浏览器已开放 CDP 端口；端口冲突时退回随机端口。
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
        // 自定义环境先启动空白浏览器，站点由调用方通过 open_tab 应用设置后再打开。
        let initial_urls = if self.environment.is_default() { initial_urls } else { &[] };
        self.validate_existing_proxy().await?;
        let proxy_address = self.proxy_address.as_ref().and_then(|address| address.as_deref());
        if proxy_address.is_some() {
            log_progress(progress, "浏览器代理已启用，代理认证由程序自动处理").await;
        }
        log_progress(
            progress,
            format!("检测配置端口 localhost:{} 是否已有 CDP 响应", self.port),
        )
        .await;
        if self.is_available().await {
            check_cancel(progress)?;
            self.prepare_environment()?;
            log_progress(progress, format!("配置端口 localhost:{} 已响应", self.port)).await;
            let opened_initial_urls = self.ensure_initial_urls(initial_urls, progress).await?;
            return Ok(CdpLaunchResult {
                port: self.port,
                message: connected_message(
                    &format!("{} CDP 已连接", self.browser.name()),
                    self.port,
                    opened_initial_urls,
                ),
                opened_initial_urls,
                launched: false,
            });
        }
        check_cancel(progress)?;

        if self.profile_group.is_some() {
            let profile_dir = self.primary_profile();
            if profile_dir_in_use(&profile_dir) || !port_is_free(self.port) {
                return Err(format!(
                    "分组浏览器正在运行或端口 {} 已被占用，请关闭该组浏览器或更换端口",
                    self.port
                ));
            }
            return launch_and_wait(self, &profile_dir, false, initial_urls, Some(self.port), proxy_address, progress)
                .await?
                .ok_or_else(|| format!("未能连接分组浏览器 localhost:{}", self.port));
        }

        let profile_dir = dedicated_profile_dir(self.browser);
        log_progress(
            progress,
            format!(
                "检查专用 {} Profile：{}",
                self.browser.name(),
                profile_dir.display()
            ),
        )
        .await;
        if profile_dir_in_use(&profile_dir) {
            log_progress(
                progress,
                format!(
                    "专用 {} Profile 正在被占用，但没有可用 CDP；跳过复用，改用备用 Profile",
                    self.browser.name()
                ),
            )
            .await;
        } else if self.port > 0 && port_is_free(self.port) {
            log_progress(
                progress,
                format!("配置端口 localhost:{} 可用，优先按该端口启动", self.port),
            )
            .await;
            if let Some(result) = launch_and_wait(
                self,
                &profile_dir,
                false,
                initial_urls,
                Some(self.port),
                proxy_address,
                progress,
            )
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
                format!(
                    "配置端口 localhost:{} 已被占用，准备改用随机端口",
                    self.port
                ),
            )
            .await;
            if let Some(result) = launch_and_wait(
                self,
                &profile_dir,
                false,
                initial_urls,
                None,
                proxy_address,
                progress,
            )
            .await?
            {
                return Ok(result);
            }

            check_cancel(progress)?;
            log_progress(progress, "随机端口启动后未响应，准备尝试备用 Profile").await;
        }

        let recovery_dir = recovery_profile_dir(self.browser);
        if let Some(result) = launch_and_wait(
            self,
            &recovery_dir,
            true,
            initial_urls,
            None,
            proxy_address,
            progress,
        )
        .await?
        {
            return Ok(result);
        }

        Err(format!(
            "已尝试自动启动 {}，但 CDP 仍未连接。请关闭刚打开的专用浏览器后重试，或在设置里更换 CDP 端口。原端口：{}",
            self.browser.name(), self.port
        ))
    }

    /// 在所选浏览器中打开新标签页，返回 tab ID。
    pub async fn open_tab(&self, url: &str) -> Result<String, String> {
        self.open_tab_with_progress(url, None).await
    }

    pub async fn open_tab_with_progress(
        &self,
        url: &str,
        progress: Option<&CdpProgress>,
    ) -> Result<String, String> {
        check_cancel(progress)?;
        self.prepare_environment()?;
        // 统一从空白页导航，默认环境也能检查网络错误并重试。
        let encoded = encode_cdp_target_url("about:blank");
        let response = self.request(
            "PUT",
            &format!("/json/new?{}", encoded),
            Duration::from_secs(10),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }

        let tab = serde_json::from_str::<CdpTab>(&response.body).map_err(|err| err.to_string())?;
        if url.trim() != "about:blank" {
            if let Err(error) = environment::navigate(&tab, &self.environment, url, progress).await {
                let _ = self.close_tab(&tab.id).await;
                return Err(error);
            }
        }
        Ok(tab.id)
    }

    fn list_tabs(&self) -> Result<Vec<CdpTab>, String> {
        let response = self.request("GET", "/json/list", Duration::from_secs(5))?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }
        let tabs: Vec<CdpTab> = serde_json::from_str(&response.body).map_err(|err| err.to_string())?;
        Ok(tabs)
    }

    /// 查找已经打开到相同站点的标签页，避免启动后重复打开同一个站点。
    pub async fn find_tab_for_url(&self, url: &str) -> Option<String> {
        self.prepare_environment().ok()?;
        self.find_site_tab(url)
    }

    fn find_site_tab(&self, url: &str) -> Option<String> {
        let expected_host = host_from_url(url)?;
        self.list_tabs().ok()?.into_iter()
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

    pub async fn get_all_cookies(&self) -> Result<Vec<serde_json::Value>, String> {
        let websocket_url = self.page_websocket_url().await?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let response = websocket.call("Storage.getCookies", serde_json::json!({}))?;
        Ok(response
            .get("result")
            .and_then(|value| value.get("cookies"))
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// 读取已打开目标站点的 Local Storage，用于把 M-Team 等无登录 Cookie 的站点同步到 CookieCloud。
    pub async fn get_local_storage_for_urls(
        &self,
        site_urls: &[String],
    ) -> Result<Vec<CdpLocalStorageParam>, String> {
        let mut storages = Vec::new();
        for origin in unique_origins(site_urls) {
            let Some(host) = host_from_url(&origin) else {
                continue;
            };
            let Some(tab_id) = self.find_site_tab(&origin) else {
                continue;
            };
            let Some(websocket_url) = self.websocket_url_for_tab(&tab_id)? else {
                continue;
            };
            let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
            let response = websocket.call(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "Object.entries(localStorage)",
                    "returnByValue": true
                }),
            )?;
            let mut items = response
                .get("result")
                .and_then(|value| value.get("result"))
                .and_then(|value| value.get("value"))
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let pair = entry.as_array()?;
                    Some(CdpLocalStorageEntry {
                        name: pair.first()?.as_str()?.to_string(),
                        value: pair.get(1)?.as_str()?.to_string(),
                    })
                })
                .collect::<Vec<_>>();
            if items.is_empty() {
                continue;
            }
            items.sort_by(|a, b| a.name.cmp(&b.name));
            items.dedup_by(|a, b| a.name == b.name);
            storages.push(CdpLocalStorageParam {
                host,
                origin,
                items,
            });
        }
        Ok(storages)
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
            let mut imported_items =
                set_local_storage_items(&mut websocket, storage, &storage.items);
            let missing_items = storage
                .items
                .iter()
                .filter(|item| {
                    !imported_items
                        .iter()
                        .any(|written| written.name == item.name)
                })
                .cloned()
                .collect::<Vec<_>>();
            if !missing_items.is_empty() {
                tokio::time::sleep(Duration::from_millis(600)).await;
                imported_items.extend(set_local_storage_items(
                    &mut websocket,
                    storage,
                    &missing_items,
                ));
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

    /// Cookie 写入后，对浏览器中已打开的目标站点页面执行刷新，
    /// 使新 Cookie 立即生效——否则用户看到的仍是旧的未登录状态。
    pub async fn reload_tabs_for_sites(&self, site_urls: &[String]) {
        let Ok(tabs) = self.list_tabs() else {
            return;
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

    pub async fn clear_managed_browser_data(&self, site_urls: &[String]) -> Result<bool, String> {
        let mut cleared = false;
        let mut failures = Vec::new();
        for profile in self.profiles() {
            match self.clear_profile_data(&profile, site_urls).await {
                Ok(result) => cleared |= result,
                Err(err) => failures.push(err),
            }
        }
        if failures.is_empty() {
            Ok(cleared)
        } else {
            Err(failures.join("；"))
        }
    }

    async fn clear_profile_data(
        &self,
        profile: &PathBuf,
        site_urls: &[String],
    ) -> Result<bool, String> {
        match fs::symlink_metadata(profile) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err("专用浏览器数据路径不是普通目录".to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(format!("无法读取专用浏览器数据目录：{err}")),
        }

        let mut ports = vec![self.port];
        if let Some(port) = read_devtools_port(profile).filter(|port| *port != self.port) {
            ports.push(port);
        }
        let timeout = Duration::from_millis(600);
        let mut ownership_unconfirmed = false;
        for port in ports {
            let cdp = Self::with_browser(port, self.browser);
            let Some(version) = cdp.browser_version(timeout) else {
                continue;
            };
            match proxy_check::uses_managed_profile(&version, std::slice::from_ref(profile), timeout) {
                Ok(true) => {
                    cdp.clear_connected_browser_data(site_urls).await?;
                    return Ok(true);
                }
                Ok(false) => {}
                Err(_) => ownership_unconfirmed = true,
            }
        }

        if ownership_unconfirmed || profile_dir_in_use(profile) {
            return Err(format!(
                "无法确认专用 {} 已关闭，请关闭该浏览器后再清除",
                self.browser.name()
            ));
        }
        fs::remove_dir_all(profile)
            .map_err(|err| format!("清除专用 {} 数据目录失败：{err}", self.browser.name()))?;
        Ok(true)
    }

    async fn clear_connected_browser_data(&self, site_urls: &[String]) -> Result<(), String> {
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
            .ok_or_else(|| "未找到可用的浏览器页面调试通道".to_string())
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
        let response = self
            .request("GET", "/json/list", Duration::from_secs(5))
            .ok()?;
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
                log_progress(progress, format!("站点已在浏览器中打开：{}", url)).await;
                opened_count += 1;
                continue;
            }

            log_progress(progress, format!("通过 CDP 新建标签页：{}", url)).await;
            self.open_tab_with_progress(url, progress)
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

    /// 关闭当前 CDP 浏览器实例；调用方需先确认它属于程序的专用 Profile。
    pub async fn close_browser(&self) -> Result<(), String> {
        let mut websocket = CdpWebSocket::connect(&self.browser_session()?, Duration::from_secs(10))?;
        websocket.call("Browser.close", serde_json::json!({}))?;
        environment::remove_browser(self.port);
        Ok(())
    }

    /// 同一个调试端口可能被下次启动复用，延时收尾需确认仍是原来的浏览器。
    pub fn browser_session(&self) -> Result<String, String> {
        #[derive(Deserialize)]
        struct CdpVersion {
            #[serde(rename = "webSocketDebuggerUrl")]
            web_socket_debugger_url: String,
        }

        let response = self.request("GET", "/json/version", Duration::from_secs(5))?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }
        let version = serde_json::from_str::<CdpVersion>(&response.body)
            .map_err(|err| format!("解析浏览器版本信息失败: {}", err))?;
        Ok(version.web_socket_debugger_url)
    }

    /// 收尾时重新检查配置端口和两个专用 Profile 的端口，只关闭确认为程序 Profile 的实例。
    pub async fn close_running_dedicated_browsers(&self) -> Vec<Result<u16, String>> {
        let mut ports = vec![self.port];
        let profiles = self.profiles();
        for profile in &profiles {
            if let Some(port) = read_devtools_port(&profile) {
                ports.push(port);
            }
        }
        ports.sort_unstable();
        ports.dedup();
        let mut results = Vec::new();
        for port in ports.into_iter().filter(|port| *port > 0) {
            let candidate = Self::with_browser(port, self.browser);
            if !candidate
                .is_available_with_timeout(Duration::from_millis(600))
                .await
            {
                continue;
            }
            let managed = (|| {
                let response = candidate.request("GET", "/json/version", Duration::from_secs(2))?;
                let version = serde_json::from_str(&response.body)
                    .map_err(|err| format!("读取浏览器信息失败：{err}"))?;
                proxy_check::uses_managed_profile(&version, &profiles, Duration::from_secs(2))
            })();
            match managed {
                Ok(true) => {
                    let close_result = candidate.close_browser().await;
                    // Browser.close 的应答可能早于进程退出；等 CDP 端口实际消失后才报告已关闭。
                    let mut closed = false;
                    for _ in 0..10 {
                        if !candidate
                            .is_available_with_timeout(Duration::from_millis(300))
                            .await
                        {
                            closed = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    if closed {
                        results.push(Ok(port));
                    } else {
                        results.push(Err(format!(
                            "专用 {} localhost:{port} 仍在运行：{}",
                            self.browser.name(),
                            close_result
                                .err()
                                .unwrap_or_else(|| "关闭指令已发送，但浏览器没有退出".to_string())
                        )));
                    }
                }
                Ok(false) => {}
                Err(err) => results.push(Err(format!(
                    "检查 {} localhost:{port} 是否为专用浏览器失败：{err}",
                    self.browser.name()
                ))),
            }
        }
        results
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

async fn ensure_storage_page_ready(websocket: &mut CdpWebSocket, storage: &CdpLocalStorageParam) {
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
    let value = response.get("result")?.get("result")?.get("value")?;
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
    dom_storage_ids(websocket, storage)
        .into_iter()
        .any(|storage_id| {
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

pub fn browser_installed(browser: BrowserKind) -> bool {
    find_browser_executable(browser).is_some()
}

fn read_http_response(stream: &mut TcpStream) -> Result<String, String> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];

    // Chromium 系浏览器的 CDP HTTP 端口通常会返回 Content-Length，但不保证立刻关闭连接。
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
    attachment_events: Option<std::collections::VecDeque<serde_json::Value>>,
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

        Ok(Self { stream, next_id: 1, attachment_events: None })
    }

    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if method != "Page.navigate" {
            return self.call_in_session(method, params, None);
        }

        self.call_with_timeout(method, params, CDP_NAVIGATION_TIMEOUT)
    }

    fn call_with_timeout(
        &mut self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let previous_timeout = self.stream.read_timeout().map_err(|err| err.to_string())?;
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;
        let result = self.call_in_session(method, params, None);
        // 包括导航失败的情况，后续页面状态读取仍使用原来的短超时。
        let restored = self.stream.set_read_timeout(previous_timeout);
        match result {
            Ok(response) => {
                restored.map_err(|err| err.to_string())?;
                Ok(response)
            }
            Err(error) => Err(error),
        }
    }

    fn call_in_session(
        &mut self,
        method: &str,
        params: serde_json::Value,
        session_id: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            request["sessionId"] = serde_json::json!(session_id);
        }
        self.write_frame(0x1, request.to_string().as_bytes())?;

        loop {
            let message = self
                .read_message()
                .map_err(|err| format!("{method}：{err}"))?;
            let value = serde_json::from_str::<serde_json::Value>(&message)
                .map_err(|err| format!("CDP WebSocket 响应解析失败：{}", err))?;
            if value.get("method").and_then(|method| method.as_str()) == Some("Target.attachedToTarget") {
                if let Some(events) = &mut self.attachment_events {
                    events.push_back(value);
                    continue;
                }
            }
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
                .map_err(|err| match err.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                        "等待浏览器 CDP 响应超时".to_string()
                    }
                    _ => format!("读取 CDP WebSocket 帧失败：{err}"),
                })?;
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
    if normalized_host != "127.0.0.1" && normalized_host != "localhost" && normalized_host != "::1"
    {
        return Err("只允许连接本机 CDP WebSocket".to_string());
    }

    Ok(WsEndpoint {
        // 浏览器启动时绑定 127.0.0.1；localhost 在 Windows 上可能先解析到 ::1，导致写 Cookie 时 10061。
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
    client: &CdpClient,
    profile_dir: &Path,
    recovery: bool,
    initial_urls: &[String],
    fixed_port: Option<u16>,
    proxy_address: Option<&str>,
    progress: Option<&CdpProgress>,
) -> Result<Option<CdpLaunchResult>, String> {
    let browser = client.browser;
    let launch_urls = launch_urls(initial_urls);
    let mode = if recovery {
        format!("备用专用 {}", browser.name())
    } else {
        format!("专用 {}", browser.name())
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
    launch_browser(
        browser, profile_dir, &launch_urls, fixed_port, proxy_address,
        client.profile_group.is_some(), &client.environment,
    )?;
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
                        format!(
                            "等待 {} 写入 CDP 端口... 已等待 {} 秒",
                            browser.name(),
                            attempt / 2
                        ),
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
        let cdp = if client.require_managed_profile {
            CdpClient { port, ..client.clone() }
        } else {
            CdpClient::with_browser(port, browser)
        };
        if cdp
            .is_available_with_timeout(Duration::from_millis(800))
            .await
        {
            check_cancel(progress)?;
            cdp.prepare_environment()?;
            log_progress(progress, format!("CDP localhost:{} 已响应", port)).await;
            let opened_initial_urls = unique_urls(initial_urls).len();
            if opened_initial_urls > 0 {
                log_progress(
                    progress,
                    format!(
                        "{} 启动参数已打开 {} 个初始站点",
                        browser.name(),
                        opened_initial_urls
                    ),
                )
                .await;
            }
            let prefix = if recovery {
                format!("已启动备用专用调试 {}", browser.name())
            } else {
                format!("已启动专用调试 {}", browser.name())
            };
            let message = connected_message(&prefix, port, opened_initial_urls);
            return Ok(Some(CdpLaunchResult {
                port,
                message,
                opened_initial_urls,
                launched: true,
            }));
        }

        if attempt % 4 == 0 {
            log_progress(progress, format!("端口 localhost:{} 尚未响应 CDP", port)).await;
        }
    }

    log_progress(progress, format!("{} 在 15 秒内未提供可用 CDP", mode)).await;
    Ok(None)
}

fn launch_browser(
    browser: BrowserKind,
    profile_dir: &Path,
    urls: &[String],
    fixed_port: Option<u16>,
    proxy_address: Option<&str>,
    force_direct: bool,
    environment: &BrowserEnvironment,
) -> Result<(), String> {
    let browser_path = find_browser_executable(browser).ok_or_else(|| {
        format!(
            "未检测到 {}。请在总览点击安装按钮，安装完成后再重试。",
            browser.name()
        )
    })?;

    let mut command = Command::new(browser_path);
    fs::create_dir_all(profile_dir)
        .map_err(|err| format!("创建 {} 专用 Profile 失败：{}", browser.name(), err))?;

    command
        .arg(format!(
            "--remote-debugging-port={}",
            fixed_port.unwrap_or(0)
        ))
        .arg("--remote-debugging-address=127.0.0.1")
        // CDP 保活不需要 --enable-automation；代理参数改从进程读取，避免显示控制提示条。
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--new-window");

    if !environment.language.is_empty() {
        // 首个导航的请求可能早于 CDP 页面附加，语言从浏览器启动时就统一设置。
        command.arg(format!("--accept-lang={}", environment.language));
    }

    if let Some(address) = proxy_address {
        // 命令行只包含本地转发地址，上游代理密码不会进入进程参数或浏览器 Profile。
        command.arg(format!("--proxy-server={address}"))
            .arg("--disable-quic")
            .arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp");
    } else if force_direct {
        command.arg("--no-proxy-server");
    }

    for url in urls {
        command.arg(url);
    }

    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("自动启动 {} 失败：{}", browser.name(), err))
}

fn dedicated_profile_dir(browser: BrowserKind) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(root)
                .join("pt-manager")
                .join(browser.profile_name());
        }
    }

    #[cfg(target_os = "macos")]
    {
        // Profile 保存登录态，不能放在可能被 macOS 自动清理的临时目录。
        if let Some(root) = env::var_os("HOME") {
            return PathBuf::from(root)
                .join("Library/Application Support/com.ptmanager.app")
                .join(browser.profile_name());
        }
    }

    env::temp_dir().join(format!("pt-manager-{}", browser.profile_name()))
}

fn recovery_profile_dir(browser: BrowserKind) -> PathBuf {
    env::temp_dir().join(format!(
        "pt-manager-{}-cdp-recovery-{}",
        browser.name().to_ascii_lowercase(),
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

fn find_browser_executable(browser: BrowserKind) -> Option<PathBuf> {
    browser_candidates(browser).into_iter().find_map(|path| {
        if path.exists() {
            return Some(path);
        }
        find_in_path(&path)
    })
}

fn browser_candidates(browser: BrowserKind) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // 优先使用显式路径；默认候选只查找所选浏览器，避免缺失 Edge 时回退到 Chrome。
    let override_name = match browser {
        BrowserKind::Chrome => "CHROME",
        BrowserKind::Edge => "EDGE",
    };
    if let Ok(value) = env::var(override_name) {
        paths.push(PathBuf::from(value));
    }

    #[cfg(target_os = "windows")]
    {
        match browser {
            BrowserKind::Chrome => {
                for key in ["LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)"] {
                    if let Ok(root) = env::var(key) {
                        paths.push(
                            PathBuf::from(root).join("Google\\Chrome\\Application\\chrome.exe"),
                        );
                    }
                }
                paths.push(PathBuf::from("chrome.exe"));
            }
            BrowserKind::Edge => {
                for key in ["LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)"] {
                    if let Ok(root) = env::var(key) {
                        paths.push(
                            PathBuf::from(root).join("Microsoft\\Edge\\Application\\msedge.exe"),
                        );
                    }
                }
                paths.push(PathBuf::from("msedge.exe"));
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let app_path = match browser {
            BrowserKind::Chrome => "Google Chrome.app/Contents/MacOS/Google Chrome",
            BrowserKind::Edge => "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        };
        paths.push(PathBuf::from("/Applications").join(app_path));
        if let Some(root) = env::var_os("HOME") {
            paths.push(PathBuf::from(root).join("Applications").join(app_path));
        }
    }

    #[cfg(target_os = "linux")]
    {
        let commands: &[&str] = match browser {
            BrowserKind::Chrome => &[
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
            ],
            BrowserKind::Edge => &["microsoft-edge", "microsoft-edge-stable"],
        };
        paths.extend(commands.iter().map(|name| PathBuf::from(*name)));
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

#[cfg(test)]
mod browser_data_tests {
    use super::*;

    #[test]
    fn offline_cleanup_only_removes_the_requested_profile() {
        let root = env::temp_dir().join(format!("pt-manager-clear-test-{}", uuid::Uuid::new_v4()));
        let selected = root.join("selected");
        let other = root.join("other");
        fs::create_dir_all(&selected).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(selected.join("Cookies"), "test cookies").unwrap();
        fs::write(other.join("Cookies"), "other group cookies").unwrap();

        let cdp = CdpClient::new(0);
        let result = tauri::async_runtime::block_on(cdp.clear_profile_data(&selected, &[]));
        let selected_removed = !selected.exists();
        let other_cookies = fs::read_to_string(other.join("Cookies"));
        fs::remove_dir_all(root).unwrap();

        assert!(result.unwrap());
        assert!(selected_removed);
        assert_eq!(other_cookies.unwrap(), "other group cookies");
    }
}
