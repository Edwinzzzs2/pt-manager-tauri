use chrono::{DateTime, Local, LocalResult, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::Mutex;

const DEFAULT_LOG_RETENTION: usize = 500;
const MIN_LOG_RETENTION: usize = 50;
const MAX_LOG_RETENTION: usize = 5000;
const LOGIN_ATTEMPT_RESET_SECONDS: i64 = 24 * 60 * 60;
static LOG_RETENTION: AtomicUsize = AtomicUsize::new(DEFAULT_LOG_RETENTION);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub totp_secret: String,
    #[serde(default = "default_auto_login")]
    pub auto_login: bool,
    #[serde(default)]
    pub login_attempts_remaining: Option<u32>,
    #[serde(default)]
    pub login_attempts_recorded_at: Option<i64>,
    #[serde(default)]
    pub login_success_recorded_at: Option<i64>,
    #[serde(default = "default_auto_keepalive")]
    pub auto_keepalive: bool,
    #[serde(default)]
    pub auto_signin: bool,
    #[serde(default)]
    pub auto_daily_bonus: bool,
    #[serde(default = "default_cookiecloud_upload")]
    pub cookiecloud_upload: bool,
}

#[derive(Clone, Copy, Deserialize)]
pub struct SiteAutomationUpdate {
    pub auto_login: bool,
    pub auto_keepalive: bool,
    pub auto_signin: bool,
    pub cookiecloud_upload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub sites: Vec<Site>,
    #[serde(default)]
    pub site_groups: Vec<SiteGroup>,
    #[serde(default)]
    pub startup_group_id: Option<String>,
    /// 仅用于一次任务的运行配置，不能由导入文件指定。
    #[serde(skip)]
    pub profile_group: Option<String>,
    #[serde(skip)]
    cookiecloud_blocked_sites: Vec<String>,
    pub cron: String,
    /// Cron 触发后随机延迟的最大分钟数；0 表示不延迟。
    #[serde(default)]
    pub cron_offset_minutes: i64,
    pub cdp_port: u16,
    #[serde(default)]
    pub browser: BrowserKind,
    #[serde(default)]
    pub browser_proxy: BrowserProxyConfig,
    pub visit_duration: u64,
    pub random_delay: bool,
    #[serde(default = "default_auto_launch")]
    pub auto_launch: bool,
    #[serde(default = "default_log_retention")]
    pub log_retention: usize,
    #[serde(default)]
    pub auto_sync_cookie: bool,
    #[serde(default)]
    pub auto_sync_cookie_after_keepalive: bool,
    #[serde(default = "default_ocr_server_url")]
    pub ocr_server_url: String,
    #[serde(default = "default_ocr_retry_count")]
    pub ocr_retry_count: u8,
    #[serde(default = "default_min_login_attempts_remaining")]
    pub min_login_attempts_remaining: u8,
    #[serde(default = "default_auto_check_updates")]
    pub auto_check_updates: bool,
    #[serde(default = "default_update_check_interval_hours")]
    pub update_check_interval_hours: u16,
    #[serde(default)]
    pub update_proxy_url: String,
    #[serde(default)]
    pub update_proxy_password: String,
    #[serde(default)]
    pub cookiecloud: CookieCloudConfig,
    #[serde(default)]
    pub gotify: GotifyConfig,
    #[serde(default)]
    pub bark: BarkConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserKind {
    #[default]
    Chrome,
    Edge,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserProxyConfig {
    pub enabled: bool,
    pub server_url: String,
    pub username: String,
    pub password: String,
}

// 配置可能包含代理密码；Debug 输出只保留开关，避免诊断日志泄漏凭据。
impl std::fmt::Debug for BrowserProxyConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("BrowserProxyConfig")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl BrowserKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Edge => "Edge",
        }
    }

    pub fn profile_name(self) -> &'static str {
        match self {
            Self::Chrome => "chrome-cdp-profile-auto",
            Self::Edge => "edge-cdp-profile-auto",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupCookieMode {
    #[default]
    Manual,
    Inherit,
    Custom,
    #[serde(skip_serializing)]
    Off,
    #[serde(skip_serializing)]
    Automatic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteGroup {
    pub id: String,
    pub name: String,
    pub cdp_port: u16,
    #[serde(default)]
    pub browser_proxy: BrowserProxyConfig,
    #[serde(default)]
    pub cookie_mode: GroupCookieMode,
    #[serde(default)]
    pub cookiecloud: CookieCloudConfig,
    #[serde(default)]
    pub cookie_auto_import: bool,
    #[serde(default)]
    pub cookie_auto_upload: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookieCloudConfig {
    pub server_url: String,
    pub uuid: String,
    pub password: String,
}

impl AppConfig {
    pub fn with_batch_site_settings(
        &self,
        updates: &std::collections::HashMap<String, SiteAutomationUpdate>,
        move_to: Option<Option<&str>>,
    ) -> Result<Self, String> {
        if let Some(Some(group_id)) = move_to {
            self.group(group_id)?;
        }
        let mut next = self.clone();
        let mut moved = Vec::new();
        let mut matched = 0;
        for site in &mut next.sites {
            let Some(update) = updates.get(&site.id) else { continue };
            matched += 1;
            site.auto_login = update.auto_login;
            site.auto_keepalive = update.auto_keepalive;
            site.auto_signin = update.auto_signin;
            site.cookiecloud_upload = update.cookiecloud_upload;
            if let Some(group_id) = move_to {
                if site.group_id.as_deref() != group_id {
                    site.group_id = group_id.map(str::to_owned);
                    moved.push(site.id.clone());
                }
            }
        }
        if matched != updates.len() {
            return Err("部分所选站点已不存在，请重新选择".to_string());
        }
        // 检查最终分组，包含同一次移动的站点；任何冲突都不保存本次更改。
        for site in next.sites.iter().filter(|site| moved.contains(&site.id)) {
            next.validate_account_group(site.group_id.as_deref(), &site.url, &site.username, Some(&site.id))
                .map_err(|err| format!("「{}」：{err}", site.name))?;
        }
        Ok(next)
    }

    pub fn group_ids(&self) -> impl Iterator<Item = Option<&str>> {
        std::iter::once(None).chain(self.site_groups.iter().map(|group| Some(group.id.as_str())))
    }

    pub fn global_cookiecloud_group_ids(&self) -> impl Iterator<Item = Option<&str>> {
        std::iter::once(None).chain(
            self.site_groups.iter()
                .filter(|group| group.cookie_mode == GroupCookieMode::Inherit)
                .map(|group| Some(group.id.as_str())),
        )
    }

    pub fn group(&self, id: &str) -> Result<&SiteGroup, String> {
        self.site_groups
            .iter()
            .find(|group| group.id == id)
            .ok_or_else(|| "所选站点分组已不存在".to_string())
    }

    pub fn for_group(&self, id: Option<&str>) -> Result<Self, String> {
        let mut scoped = self.clone();
        scoped.profile_group = None;
        scoped.cookiecloud_blocked_sites.clear();
        scoped.sites.retain(|site| site.group_id.as_deref() == id);
        if let Some(id) = id {
            let group = self.group(id)?;
            // 配置文件中的 ID 也必须验证后才能用于浏览器目录名。
            if !valid_group_id(id) {
                return Err("站点分组 ID 无效".to_string());
            }
            scoped.profile_group = Some(group.id.clone());
            scoped.cdp_port = group.cdp_port;
            scoped.browser_proxy = group.browser_proxy.clone();
            match group.cookie_mode {
                GroupCookieMode::Manual | GroupCookieMode::Off => {
                    scoped.cookiecloud = CookieCloudConfig::default();
                    scoped.auto_sync_cookie = false;
                    scoped.auto_sync_cookie_after_keepalive = false;
                }
                GroupCookieMode::Inherit => {}
                GroupCookieMode::Custom | GroupCookieMode::Automatic => {
                    scoped.cookiecloud = group.cookiecloud.clone();
                    scoped.auto_sync_cookie = group.cookie_auto_import || group.cookie_mode == GroupCookieMode::Automatic;
                    scoped.auto_sync_cookie_after_keepalive = group.cookie_auto_upload || group.cookie_mode == GroupCookieMode::Automatic;
                }
            }
            if group.cookie_mode == GroupCookieMode::Inherit {
                scoped.cookiecloud_blocked_sites = scoped.sites.iter()
                    .filter(|site| self.shares_cookiecloud_account_host(site))
                    .map(|site| site.id.clone())
                    .collect();
            }
        }
        Ok(scoped)
    }

    pub fn can_sync_site(&self, site: &Site) -> bool {
        !self.cookiecloud_blocked_sites.contains(&site.id)
    }

    fn shares_cookiecloud_account_host(&self, site: &Site) -> bool {
        let Some(host) = account_host(&site.url) else { return false };
        self.sites.iter().any(|other| {
            other.group_id != site.group_id
                && (site.username.trim().is_empty() || site.username.trim() != other.username.trim())
                && account_host(&other.url).as_deref() == Some(host.as_str())
                && match other.group_id.as_deref() {
                    None => true,
                    Some(id) => self.group(id).is_ok_and(|group| group.cookie_mode == GroupCookieMode::Inherit),
                }
        })
    }

    pub fn validate_account_group(
        &self,
        group_id: Option<&str>,
        site_url: &str,
        username: &str,
        editing_id: Option<&str>,
    ) -> Result<(), String> {
        if let Some(id) = group_id {
            self.group(id)?;
        }
        if username.trim().is_empty() {
            return Ok(());
        }
        let Some(host) = account_host(site_url) else { return Ok(()) };
        let conflicts = self.sites.iter().any(|site| {
            Some(site.id.as_str()) != editing_id
                && site.group_id.as_deref() == group_id
                && !site.username.trim().is_empty()
                && site.username.trim() != username.trim()
                && account_host(&site.url).as_deref() == Some(&host)
        });
        if conflicts {
            return Err("同一站点的不同账号共用登录状态，请把这个账号放到其他分组".to_string());
        }
        Ok(())
    }
}

fn account_host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url.trim()).ok()?;
    Some(url.host_str()?.trim_start_matches("www.").to_owned())
}

fn valid_group_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

pub fn normalize_site_groups(config: &mut AppConfig) -> Result<(), String> {
    use std::collections::HashSet;

    let mut ids = HashSet::new();
    let mut names = HashSet::from(["未分组".to_string()]);
    let mut ports = HashSet::from([config.cdp_port]);
    let default_uuid = config.cookiecloud.uuid.trim().to_ascii_lowercase();
    let mut cookie_uuids = HashSet::new();
    if !default_uuid.is_empty() {
        cookie_uuids.insert(default_uuid);
    }
    let default_server = config.cookiecloud.server_url.trim();
    if let Ok(url) = reqwest::Url::parse(default_server).or_else(|_| reqwest::Url::parse(&format!("http://{default_server}"))) {
        if let Some((_, uuid)) = url.path().to_ascii_lowercase().split_once("/get/") {
            cookie_uuids.insert(uuid.split('/').next().unwrap_or_default().to_owned());
        }
    }
    for group in &mut config.site_groups {
        group.migrate_cookie_mode();
        group.name = group.name.trim().to_string();
        if !valid_group_id(&group.id) || !ids.insert(group.id.as_str()) {
            return Err("站点分组 ID 无效或重复".to_string());
        }
        if group.name.is_empty() || !names.insert(group.name.to_lowercase()) {
            return Err("分组名称不能为空或重复".to_string());
        }
        if group.cdp_port == 0 || !ports.insert(group.cdp_port) {
            return Err(format!("分组「{}」的调试端口无效或已被其他分组使用", group.name));
        }
        crate::browser_proxy::normalize_config(&mut group.browser_proxy)
            .map_err(|err| format!("分组「{}」：{err}", group.name))?;
        if group.cookie_mode == GroupCookieMode::Custom {
            group.cookiecloud.server_url = group.cookiecloud.server_url.trim().trim_end_matches('/').to_string();
            group.cookiecloud.uuid = group.cookiecloud.uuid.trim().to_string();
            if group.cookiecloud.server_url.is_empty()
                || group.cookiecloud.uuid.is_empty()
                || group.cookiecloud.password.is_empty()
            {
                return Err(format!("请填写分组「{}」的 CookieCloud 服务地址、UUID 和密码", group.name));
            }
            let server = reqwest::Url::parse(&group.cookiecloud.server_url).ok().filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && !url.path().to_ascii_lowercase().contains("/get/")
            });
            let Some(server) = server else {
                return Err(format!("分组「{}」请填写 CookieCloud 服务地址，UUID 请单独填写", group.name));
            };
            group.cookiecloud.server_url = server.as_str().trim_end_matches('/').to_string();
            if !cookie_uuids.insert(group.cookiecloud.uuid.to_ascii_lowercase()) {
                return Err(format!("分组「{}」必须使用独立的 CookieCloud UUID", group.name));
            }
        }
    }
    if config.sites.iter().any(|site| {
        site.group_id.as_deref().is_some_and(|id| !ids.contains(id))
    }) {
        return Err("仍有站点属于要删除的分组，请先移走这些站点".to_string());
    }
    if config.startup_group_id.as_deref().is_some_and(|id| !ids.contains(id)) {
        config.startup_group_id = None;
    }
    Ok(())
}

impl SiteGroup {
    fn migrate_cookie_mode(&mut self) {
        if self.cookie_mode == GroupCookieMode::Off {
            self.cookie_mode = GroupCookieMode::Manual;
            self.cookiecloud = CookieCloudConfig::default();
            self.cookie_auto_import = false;
            self.cookie_auto_upload = false;
        } else if self.cookie_mode == GroupCookieMode::Automatic {
            self.cookie_mode = GroupCookieMode::Custom;
            self.cookie_auto_import = true;
            self.cookie_auto_upload = true;
        } else if self.cookie_mode == GroupCookieMode::Manual && !self.cookiecloud.uuid.trim().is_empty() {
            // 旧版手动模式自带独立凭据，升级后保留在自定义模式。
            self.cookie_mode = GroupCookieMode::Custom;
            self.cookie_auto_import = false;
            self.cookie_auto_upload = false;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GotifyConfig {
    pub enabled: bool,
    pub server_url: String,
    pub token: String,
    #[serde(default = "default_gotify_title")]
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BarkConfig {
    pub enabled: bool,
    pub server_url: String,
    pub device_key: String,
    #[serde(default = "default_gotify_title")]
    pub title: String,
}

impl Default for BarkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: "https://api.day.app".to_string(),
            device_key: String::new(),
            title: default_gotify_title(),
        }
    }
}

impl Default for GotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: String::new(),
            token: String::new(),
            title: default_gotify_title(),
        }
    }
}

fn default_gotify_title() -> String {
    "PT Manager 保活结果".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            sites: vec![],
            site_groups: vec![],
            startup_group_id: None,
            profile_group: None,
            cookiecloud_blocked_sites: vec![],
            cron: "0 9 * * *".to_string(),
            cron_offset_minutes: 30,
            cdp_port: 9222,
            browser: BrowserKind::default(),
            browser_proxy: BrowserProxyConfig::default(),
            visit_duration: 30,
            random_delay: true,
            auto_launch: default_auto_launch(),
            log_retention: DEFAULT_LOG_RETENTION,
            auto_sync_cookie: false,
            auto_sync_cookie_after_keepalive: false,
            ocr_server_url: default_ocr_server_url(),
            ocr_retry_count: default_ocr_retry_count(),
            min_login_attempts_remaining: default_min_login_attempts_remaining(),
            auto_check_updates: default_auto_check_updates(),
            update_check_interval_hours: default_update_check_interval_hours(),
            update_proxy_url: String::new(),
            update_proxy_password: String::new(),
            cookiecloud: CookieCloudConfig::default(),
            gotify: GotifyConfig::default(),
            bark: BarkConfig::default(),
        }
    }
}

fn default_auto_launch() -> bool {
    false
}

fn default_ocr_server_url() -> String {
    "https://ocr-read.decoffee.top".to_string()
}

fn default_ocr_retry_count() -> u8 {
    2
}

fn default_min_login_attempts_remaining() -> u8 {
    5
}

fn default_auto_check_updates() -> bool {
    true
}

fn default_update_check_interval_hours() -> u16 {
    1
}

fn default_auto_keepalive() -> bool {
    true
}

fn default_auto_login() -> bool {
    true
}

fn default_cookiecloud_upload() -> bool {
    true
}

/// 已确认的 NexusPHP 站点登录失败上限。站点可能调整策略，下一次读取到登录页时会以实际值纠正。
pub fn known_login_attempt_limit(site_url: &str) -> Option<u32> {
    let normalized = site_url.trim().to_ascii_lowercase();
    let authority = normalized
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(normalized.as_str())
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_start_matches("www.");

    match host {
        "audiences.me" | "hdfans.org" | "hdkyl.in" | "pt.btschool.club" | "si-qi.xyz" => Some(20),
        "xingtan.one" | "cspt.top" | "pandapt.net" | "hxpt.org" | "pttime.org" | "cyanbug.net" => {
            Some(10)
        }
        "ptskit.org" => Some(5),
        "open.cd" => Some(11),
        _ => None,
    }
}

/// 对登录上限较低的站点至少保留一次可尝试机会，避免配置阈值等于站点上限时永远无法登录。
pub fn effective_login_attempt_threshold(site_url: &str, configured: u32) -> u32 {
    known_login_attempt_limit(site_url)
        .map(|limit| configured.min(limit.saturating_sub(1).max(1)))
        .unwrap_or(configured)
}

pub fn observe_login_attempts(site: &mut Site, remaining: Option<u32>) {
    if let Some(remaining) = remaining {
        site.login_attempts_remaining = Some(remaining);
        if known_login_attempt_limit(&site.url).is_some_and(|limit| remaining >= limit) {
            site.login_success_recorded_at = None;
        }
    }
}

/// 保存一次登录流程的结果。失败时只采用页面返回的实际剩余次数；
/// 确认处于登录状态后启动 24 小时显示保留计时。
pub fn record_login_outcome(
    site: &mut Site,
    remaining: Option<u32>,
    logged_in: bool,
) -> Option<u32> {
    observe_login_attempts(site, remaining);
    if logged_in {
        site.login_success_recorded_at = Some(Local::now().timestamp());
    }
    site.login_attempts_recorded_at = None;
    site.login_attempts_remaining
}

/// 仅在确认成功登录满 24 小时后，清除页面上显示的剩余次数。
pub fn refresh_login_attempt_resets(config: &mut AppConfig) -> bool {
    let now = Local::now().timestamp();
    let mut changed = false;
    for site in &mut config.sites {
        // 旧版本的时间戳可能由失败登录产生，不能再作为恢复依据。
        changed |= site.login_attempts_recorded_at.take().is_some();

        let Some(success_at) = site.login_success_recorded_at else {
            continue;
        };
        if now.saturating_sub(success_at) < LOGIN_ATTEMPT_RESET_SECONDS {
            continue;
        }

        site.login_attempts_remaining = None;
        site.login_success_recorded_at = None;
        changed = true;
    }
    changed
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Local>,
    pub level: String,
    pub message: String,
}

impl LogEntry {
    pub fn info(msg: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now(),
            level: "INFO".to_string(),
            message: msg.into(),
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now(),
            level: "ERROR".to_string(),
            message: msg.into(),
        }
    }

    pub fn success(msg: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now(),
            level: "SUCCESS".to_string(),
            message: msg.into(),
        }
    }
}

fn local_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            let dir = PathBuf::from(root).join("pt-manager");
            fs::create_dir_all(&dir).ok();
            return dir;
        }
    }

    #[cfg(target_os = "macos")]
    {
        // 临时目录会被系统清理；日志必须与配置一样放进用户的持久应用数据目录。
        if let Some(root) = std::env::var_os("HOME") {
            let dir = PathBuf::from(root).join("Library/Application Support/com.ptmanager.app");
            fs::create_dir_all(&dir).ok();
            return dir;
        }
    }

    let dir = std::env::temp_dir().join("pt-manager");
    fs::create_dir_all(&dir).ok();
    dir
}

pub fn log_file_path() -> PathBuf {
    local_data_dir().join("run.log")
}

fn default_log_retention() -> usize {
    DEFAULT_LOG_RETENTION
}

pub fn normalize_log_retention(value: usize) -> usize {
    value.clamp(MIN_LOG_RETENTION, MAX_LOG_RETENTION)
}

pub fn set_log_retention(value: usize) {
    LOG_RETENTION.store(normalize_log_retention(value), Ordering::Relaxed);
}

pub fn current_log_retention() -> usize {
    normalize_log_retention(LOG_RETENTION.load(Ordering::Relaxed))
}

pub fn append_log(entry: &LogEntry) {
    let path = log_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", format_log_line(entry));
    }
}

pub fn load_logs() -> Vec<LogEntry> {
    let data = fs::read_to_string(log_file_path()).unwrap_or_default();
    let mut logs = data.lines().filter_map(parse_log_line).collect::<Vec<_>>();
    if trim_logs(&mut logs, current_log_retention()) {
        write_log_file(&logs);
    }
    logs
}

fn parse_log_line(line: &str) -> Option<LogEntry> {
    let (timestamp_text, rest) = line.split_once(" [")?;
    let (level, message) = rest.split_once("] ")?;
    let naive = NaiveDateTime::parse_from_str(timestamp_text, "%Y-%m-%d %H:%M:%S").ok()?;
    // 日志文件只有本地时间文本，恢复时按当前本地时区解释即可。
    let timestamp = match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(earliest, _) => earliest,
        LocalResult::None => return None,
    };

    Some(LogEntry {
        timestamp,
        level: level.to_string(),
        message: message.to_string(),
    })
}

pub fn clear_log_file() {
    let _ = fs::write(log_file_path(), "");
}

pub async fn push_log(logs: &Arc<Mutex<Vec<LogEntry>>>, entry: LogEntry) {
    append_log(&entry);
    let mut guard = logs.lock().await;
    guard.push(entry);
    if trim_logs(&mut guard, current_log_retention()) {
        write_log_file(&guard);
    }
}

pub async fn apply_log_retention(logs: &Arc<Mutex<Vec<LogEntry>>>) {
    let mut guard = logs.lock().await;
    if trim_logs(&mut guard, current_log_retention()) {
        write_log_file(&guard);
    }
}

fn trim_logs(logs: &mut Vec<LogEntry>, retention: usize) -> bool {
    let retention = normalize_log_retention(retention);
    if logs.len() <= retention {
        return false;
    }

    let overflow = logs.len() - retention;
    logs.drain(0..overflow);
    true
}

fn write_log_file(logs: &[LogEntry]) {
    let content = logs
        .iter()
        .map(format_log_line)
        .collect::<Vec<_>>()
        .join("\n");
    let suffix = if content.is_empty() { "" } else { "\n" };
    let _ = fs::write(log_file_path(), format!("{}{}", content, suffix));
}

fn format_log_line(entry: &LogEntry) -> String {
    format!(
        "{} [{}] {}",
        entry.timestamp.format("%Y-%m-%d %H:%M:%S"),
        entry.level,
        entry.message
    )
}

/// 获取配置文件路径
fn config_path(app_handle: &tauri::AppHandle) -> PathBuf {
    let dir = app_handle
        .path()
        .app_data_dir()
        .expect("failed to get app data dir");
    fs::create_dir_all(&dir).ok();
    dir.join("config.json")
}

/// 从磁盘加载配置
pub fn load_config(app_handle: &tauri::AppHandle) -> AppConfig {
    let path = config_path(app_handle);
    if path.exists() {
        let data = fs::read_to_string(&path).unwrap_or_default();
        let mut config = serde_json::from_str::<AppConfig>(&data).unwrap_or_default();
        for group in &mut config.site_groups {
            group.migrate_cookie_mode();
        }
        config.log_retention = normalize_log_retention(config.log_retention);
        config.ocr_retry_count = config.ocr_retry_count.clamp(1, 5);
        config.min_login_attempts_remaining = config.min_login_attempts_remaining.clamp(1, 20);
        config.update_check_interval_hours = config.update_check_interval_hours.clamp(1, 168);
        if refresh_login_attempt_resets(&mut config) {
            save_config(app_handle, &config);
        }
        config
    } else {
        let config = AppConfig::default();
        save_config(app_handle, &config);
        config
    }
}

/// 保存配置到磁盘
pub fn save_config(app_handle: &tauri::AppHandle, config: &AppConfig) {
    let path = config_path(app_handle);
    if let Ok(data) = serde_json::to_string_pretty(config) {
        fs::write(&path, data).ok();
    }
}

#[cfg(test)]
mod site_group_tests {
    use super::*;

    const PROXY_GROUP: &str = "00000000-0000-4000-8000-000000000001";

    fn site(name: &str, username: &str) -> Site {
        serde_json::from_value(serde_json::json!({
            "id": name, "name": name, "url": "https://example.com/", "username": username
        })).unwrap()
    }

    fn grouped_config() -> AppConfig {
        let mut proxied = site("代理账号", "alice");
        proxied.group_id = Some(PROXY_GROUP.to_string());
        AppConfig {
            sites: vec![site("默认账号", "bob"), proxied],
            cookiecloud: CookieCloudConfig {
                server_url: "https://cookie.example.net".to_string(),
                uuid: "default-vault".to_string(),
                password: "test".to_string(),
            },
            site_groups: vec![SiteGroup {
                id: PROXY_GROUP.to_string(),
                name: "代理组".to_string(),
                cdp_port: 9223,
                browser_proxy: BrowserProxyConfig {
                    enabled: true,
                    server_url: "http://proxy.example.net:8080".to_string(),
                    ..Default::default()
                },
                cookie_mode: GroupCookieMode::Custom,
                cookiecloud: CookieCloudConfig {
                    server_url: "https://cookie.example.net".to_string(),
                    uuid: "proxy-vault".to_string(),
                    password: "test".to_string(),
                },
                cookie_auto_import: false,
                cookie_auto_upload: false,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn startup_group_survives_reload_and_falls_back_when_missing() {
        let mut config = grouped_config();
        config.startup_group_id = Some(PROXY_GROUP.to_string());
        let mut saved = serde_json::to_value(config).unwrap();
        let mut reloaded: AppConfig = serde_json::from_value(saved.clone()).unwrap();
        normalize_site_groups(&mut reloaded).unwrap();
        assert_eq!(reloaded.startup_group_id.as_deref(), Some(PROXY_GROUP));

        reloaded.startup_group_id = Some("00000000-0000-4000-8000-000000000002".to_string());
        normalize_site_groups(&mut reloaded).unwrap();
        assert!(reloaded.startup_group_id.is_none());

        saved.as_object_mut().unwrap().remove("startup_group_id");
        let legacy: AppConfig = serde_json::from_value(saved).unwrap();
        assert!(legacy.startup_group_id.is_none());
    }

    #[test]
    fn same_domain_accounts_use_their_own_runtime_config() {
        let mut config = grouped_config();
        normalize_site_groups(&mut config).unwrap();
        let default = config.for_group(None).unwrap();
        let proxied = config.for_group(Some(PROXY_GROUP)).unwrap();
        assert_eq!(default.sites.len(), 1);
        assert_eq!(default.sites[0].username, "bob");
        assert_eq!(proxied.sites.len(), 1);
        assert_eq!(proxied.sites[0].username, "alice");
        assert_eq!(default.cdp_port, 9222);
        assert_eq!(proxied.cdp_port, 9223);
        assert!(!default.browser_proxy.enabled);
        assert!(proxied.browser_proxy.enabled);
        assert_eq!(proxied.profile_group.as_deref(), Some(PROXY_GROUP));
        assert_eq!(proxied.cookiecloud.uuid, "proxy-vault");
        assert!(!proxied.auto_sync_cookie);
        assert!(!proxied.auto_sync_cookie_after_keepalive);

        config.site_groups[0].cookie_auto_import = true;
        let automatic = config.for_group(Some(PROXY_GROUP)).unwrap();
        assert!(automatic.auto_sync_cookie);
        assert!(!automatic.auto_sync_cookie_after_keepalive);
    }

    #[test]
    fn shared_cookiecloud_modes_follow_settings_without_cross_account_sync() {
        let mut config = grouped_config();
        assert_eq!(config.global_cookiecloud_group_ids().collect::<Vec<_>>(), vec![None]);
        config.auto_sync_cookie = true;
        config.site_groups[0].cookiecloud = CookieCloudConfig::default();
        config.site_groups[0].cookie_mode = GroupCookieMode::Manual;
        let manual = config.for_group(Some(PROXY_GROUP)).unwrap();
        assert!(manual.cookiecloud.uuid.is_empty());
        assert!(!manual.auto_sync_cookie && !manual.auto_sync_cookie_after_keepalive);
        assert_eq!(config.global_cookiecloud_group_ids().collect::<Vec<_>>(), vec![None]);

        config.site_groups[0].cookie_mode = GroupCookieMode::Inherit;
        let inherited = config.for_group(Some(PROXY_GROUP)).unwrap();
        assert_eq!(config.global_cookiecloud_group_ids().collect::<Vec<_>>(), vec![None, Some(PROXY_GROUP)]);
        assert_eq!(inherited.cookiecloud.uuid, "default-vault");
        assert!(inherited.auto_sync_cookie);
        assert!(!inherited.auto_sync_cookie_after_keepalive);
        assert!(!inherited.can_sync_site(&inherited.sites[0]));
        let default = config.for_group(None).unwrap();
        assert!(default.can_sync_site(&default.sites[0]));

        config.sites[1].url = "https://different.example.net/".to_string();
        let inherited = config.for_group(Some(PROXY_GROUP)).unwrap();
        assert!(inherited.can_sync_site(&inherited.sites[0]));
    }

    #[test]
    fn legacy_cookie_modes_preserve_their_settings() {
        let mut config = grouped_config();
        config.site_groups[0].cookie_mode = GroupCookieMode::Manual;
        normalize_site_groups(&mut config).unwrap();
        assert_eq!(config.site_groups[0].cookie_mode, GroupCookieMode::Custom);
        assert_eq!(config.site_groups[0].cookiecloud.uuid, "proxy-vault");
        assert!(!config.site_groups[0].cookie_auto_import);

        config.site_groups[0].cookie_mode = GroupCookieMode::Automatic;
        normalize_site_groups(&mut config).unwrap();
        assert_eq!(config.site_groups[0].cookie_mode, GroupCookieMode::Custom);
        assert!(config.site_groups[0].cookie_auto_import);
        assert!(config.site_groups[0].cookie_auto_upload);

        config.site_groups[0].cookie_mode = GroupCookieMode::Off;
        normalize_site_groups(&mut config).unwrap();
        assert_eq!(config.site_groups[0].cookie_mode, GroupCookieMode::Manual);
        assert!(config.site_groups[0].cookiecloud.uuid.is_empty());
    }

    #[test]
    fn batch_move_validates_the_entire_selection_before_saving() {
        let mut config = grouped_config();
        let update = SiteAutomationUpdate {
            auto_login: false,
            auto_keepalive: false,
            auto_signin: true,
            cookiecloud_upload: false,
        };
        let updates = config.sites.iter().map(|site| (site.id.clone(), update)).collect();
        assert!(config.with_batch_site_settings(&updates, Some(Some(PROXY_GROUP))).is_err());
        assert!(config.sites[0].group_id.is_none());
        assert!(config.sites[0].auto_keepalive);

        config.sites[0].url = "https://other.example.net/".to_string();
        let next = config.with_batch_site_settings(&updates, Some(Some(PROXY_GROUP))).unwrap();
        assert!(next.sites.iter().all(|site| site.group_id.as_deref() == Some(PROXY_GROUP)));
        assert!(next.sites.iter().all(|site| site.auto_signin && !site.auto_keepalive));
        let default = next.with_batch_site_settings(&updates, Some(None)).unwrap();
        assert!(default.sites.iter().all(|site| site.group_id.is_none()));
        let unchanged = next.with_batch_site_settings(&updates, None).unwrap();
        assert_eq!(unchanged.sites[0].group_id, next.sites[0].group_id);
    }

    #[test]
    fn refuses_port_cookie_vault_and_same_group_account_collisions() {
        let mut config = grouped_config();
        config.site_groups[0].cdp_port = config.cdp_port;
        assert!(normalize_site_groups(&mut config).unwrap_err().contains("端口"));
        config.site_groups[0].cdp_port = 9223;
        config.site_groups[0].cookiecloud.uuid = config.cookiecloud.uuid.clone();
        assert!(normalize_site_groups(&mut config).unwrap_err().contains("独立"));
        config.site_groups[0].cookiecloud.uuid = "proxy-vault".to_string();
        config.cookiecloud.server_url = "https://cookie.example.net/get/proxy-vault".to_string();
        assert!(normalize_site_groups(&mut config).unwrap_err().contains("独立"));
        assert!(config.validate_account_group(None, "https://www.example.com/login", "alice", None).is_err());
        assert!(config.validate_account_group(Some(PROXY_GROUP), "https://example.com/login", "alice", None).is_ok());
    }
}
