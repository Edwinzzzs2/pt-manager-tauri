use super::{CdpTab, CdpWebSocket};
use crate::store::BrowserEnvironment;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct TabEnvironment {
    settings: BrowserEnvironment,
    opened_at: Instant,
    // Chrome restores the defaults when the DevTools session disconnects.
    _session: CdpWebSocket,
}

type BrowserTabs = HashMap<String, TabEnvironment>;
static BROWSERS: OnceLock<Mutex<HashMap<u16, BrowserTabs>>> = OnceLock::new();

pub(super) fn navigate(
    port: u16,
    tab: &CdpTab,
    settings: &BrowserEnvironment,
    url: &str,
) -> Result<(), String> {
    let endpoint = tab
        .web_socket_debugger_url
        .as_deref()
        .ok_or_else(|| "浏览器未提供标签页调试连接".to_string())?;
    let mut session = CdpWebSocket::connect(endpoint, Duration::from_secs(10))?;
    if !settings.timezone.is_empty() {
        session
            .call(
                "Emulation.setTimezoneOverride",
                json!({ "timezoneId": settings.timezone }),
            )
            .map_err(|err| format!("应用浏览器时区失败：{err}"))?;
    }
    if !settings.language.is_empty() {
        session
            .call(
                "Emulation.setLocaleOverride",
                json!({ "locale": settings.language.replace('-', "_") }),
            )
            .map_err(|err| format!("应用浏览器区域格式失败：{err}"))?;
        session
            .call(
                "Emulation.setUserAgentOverride",
                json!({
                    "userAgent": "",
                    "acceptLanguage": settings.language,
                }),
            )
            .map_err(|err| format!("应用浏览器语言失败：{err}"))?;
    }
    let response = session.call("Page.navigate", json!({ "url": url.trim() }))?;
    if let Some(error) = response
        .pointer("/result/errorText")
        .and_then(|value| value.as_str())
    {
        return Err(format!("打开站点失败：{error}"));
    }
    BROWSERS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "浏览器环境会话不可用".to_string())?
        .entry(port)
        .or_default()
        .insert(
            tab.id.clone(),
            TabEnvironment {
                settings: settings.clone(),
                opened_at: Instant::now(),
                _session: session,
            },
        );
    Ok(())
}

pub(super) fn matches(port: u16, tab_id: &str, settings: &BrowserEnvironment) -> bool {
    BROWSERS
        .get()
        .and_then(|browsers| browsers.lock().ok())
        .and_then(|browsers| {
            browsers
                .get(&port)
                .and_then(|tabs| tabs.get(tab_id))
                .map(|tab| &tab.settings == settings)
        })
        .unwrap_or_else(|| settings.is_default())
}

pub(super) fn retain_open_tabs(port: u16, tabs: &[CdpTab], requested_at: Instant) {
    if let Some(mut browsers) = BROWSERS.get().and_then(|browsers| browsers.lock().ok()) {
        if let Some(sessions) = browsers.get_mut(&port) {
            sessions.retain(|id, tab| {
                tab.opened_at >= requested_at || tabs.iter().any(|open| &open.id == id)
            });
        }
    }
}

pub(super) fn remove_tab(port: u16, tab_id: &str) {
    if let Some(mut browsers) = BROWSERS.get().and_then(|browsers| browsers.lock().ok()) {
        if let Some(tabs) = browsers.get_mut(&port) {
            tabs.remove(tab_id);
        }
    }
}

pub(super) fn remove_browser(port: u16) {
    if let Some(mut browsers) = BROWSERS.get().and_then(|browsers| browsers.lock().ok()) {
        browsers.remove(&port);
    }
}
