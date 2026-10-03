//! 聚影 Vue 登录页：使用页面输入事件和登录按钮，等待真实账户入口出现。

use super::common::{click_runtime_element, type_runtime_input};
use crate::cdp::jying::dismiss_announcement;
use crate::cdp::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use serde::Deserialize;
use std::time::Duration;

const USERNAME_SELECTOR: &str = ".auth-card input[placeholder=\"请输入用户名或邮箱\"]";
const PASSWORD_SELECTOR: &str = ".auth-card input[type=\"password\"][placeholder=\"请输入密码\"]";
const SUBMIT_SELECTOR: &str = ".auth-card button.n-button--primary-type";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginPageState {
    ready: bool,
    host: String,
    path: String,
    logged_in: bool,
    has_login_form: bool,
    has_verification: bool,
    error_message: String,
}

impl CdpClient {
    pub(super) async fn login_jying(
        &self,
        tab_id: &str,
        site_url: &str,
        username: &str,
        password: &str,
        progress: Option<&CdpProgress>,
    ) -> Result<bool, String> {
        let websocket_url = self.websocket_url_for_tab(tab_id)?
            .ok_or_else(|| "无法连接聚影标签页".to_string())?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let initial = wait_for_page(&mut websocket, progress, false).await?;
        if initial.logged_in {
            return Ok(false);
        }
        if initial.path != "/login" {
            let login_url = reqwest::Url::parse(site_url)
                .and_then(|url| url.join("/login"))
                .map_err(|err| format!("聚影站点地址无效：{err}"))?;
            if let Some(p) = progress {
                p.info("聚影正在打开登录页".to_string()).await;
            }
            websocket.call("Page.enable", serde_json::json!({}))?;
            websocket.call("Page.navigate", serde_json::json!({"url": login_url.as_str()}))?;
        }
        let page = wait_for_page(&mut websocket, progress, true).await?;
        if page.logged_in {
            return Ok(false);
        }
        if page.has_verification {
            return Err("聚影登录页要求人机验证，请在专用浏览器中完成验证".to_string());
        }
        if !page.has_login_form || page.path != "/login" {
            return Err("聚影登录页未找到用户名、密码和登录按钮".to_string());
        }
        check_cancel(progress)?;
        if !type_runtime_input(&mut websocket, USERNAME_SELECTOR, username.trim(), 45, 110).await {
            return Err("未找到聚影用户名或邮箱输入框".to_string());
        }
        check_cancel(progress)?;
        if !type_runtime_input(&mut websocket, PASSWORD_SELECTOR, password, 45, 110).await {
            return Err("未找到聚影密码输入框".to_string());
        }
        // Naive UI 的登录按钮不是 form submit，必须点击它触发 Vue 登录处理器。
        check_cancel(progress)?;
        if !click_runtime_element(&mut websocket, SUBMIT_SELECTOR) {
            return Err("聚影登录按钮未找到或不可点击".to_string());
        }
        if let Some(p) = progress {
            p.info("聚影已点击登录，正在等待账户状态".to_string()).await;
        }
        for _ in 0..160 {
            check_cancel(progress)?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(state) = page_state(&mut websocket) else { continue };
            ensure_site(&state)?;
            if state.ready {
                dismiss_announcement(&mut websocket, progress).await?;
            }
            if state.logged_in {
                return Ok(true);
            }
            if state.has_verification {
                return Err("聚影登录要求人机验证，请在专用浏览器中完成验证".to_string());
            }
            if !state.error_message.is_empty() {
                return Err(format!("聚影登录失败：{}", state.error_message));
            }
        }
        Err("聚影登录后未检测到个人中心入口，请检查账户或站点提示".to_string())
    }
}

async fn wait_for_page(
    websocket: &mut CdpWebSocket,
    progress: Option<&CdpProgress>,
    require_login_surface: bool,
) -> Result<LoginPageState, String> {
    for _ in 0..160 {
        check_cancel(progress)?;
        if let Some(state) = page_state(websocket) {
            ensure_site(&state)?;
            if state.ready {
                dismiss_announcement(websocket, progress).await?;
            }
            // Vue 路由和表单会在 HTML 加载后挂载，不能只看 document.readyState。
            if state.ready && (state.logged_in || state.has_login_form || state.has_verification) {
                return Ok(state);
            }
            if state.ready && !require_login_surface && state.path != "/login" {
                return Ok(state);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err("等待聚影登录表单加载超时".to_string())
}

fn ensure_site(state: &LoginPageState) -> Result<(), String> {
    if matches!(state.host.as_str(), "jying.top" | "www.jying.top") {
        Ok(())
    } else {
        Err("聚影页面跳转到了其他域名，已停止填写登录信息".to_string())
    }
}

fn page_state(websocket: &mut CdpWebSocket) -> Option<LoginPageState> {
    let response = websocket.call("Runtime.evaluate", serde_json::json!({
        "expression": LOGIN_STATE_EXPRESSION,
        "returnByValue": true
    })).ok()?;
    serde_json::from_value(response.get("result")?.get("result")?.get("value")?.clone()).ok()
}

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress.is_some_and(CdpProgress::is_cancelled) { Err(CDP_CANCELLED.to_string()) }
    else { Ok(()) }
}

const LOGIN_STATE_EXPRESSION: &str = r#"(() => {
    const clean = el => String(el?.textContent || '').replace(/\s+/g, ' ').trim();
    const username = document.querySelector('.auth-card input[placeholder="请输入用户名或邮箱"]');
    const password = document.querySelector('.auth-card input[type="password"][placeholder="请输入密码"]');
    const submit = document.querySelector('.auth-card button.n-button--primary-type');
    const account = document.querySelector('header a[href="/profile"]');
    const errors = Array.from(document.querySelectorAll('.n-message--error-type, .n-alert--error-type'))
        .map(clean).filter(text => text && text.length < 240);
    return {
        ready: document.readyState === 'interactive' || document.readyState === 'complete',
        host: location.hostname.toLowerCase(),
        path: location.pathname,
        loggedIn: Boolean(account) && location.pathname !== '/login' && !password,
        hasLoginForm: Boolean(username && password && submit),
        hasVerification: Boolean(document.querySelector(
            'iframe[src*="challenges.cloudflare.com"], iframe[src*="recaptcha"], .cf-turnstile, .g-recaptcha, #challenge-stage'
        )),
        errorMessage: errors[0] || ''
    };
})()"#;
