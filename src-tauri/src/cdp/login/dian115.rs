//! 癫影门户登录。登录表单使用 Vue 控制的邮箱和密码输入框。

use super::common::{click_runtime_element, type_runtime_input};
use crate::cdp::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageState {
    ready: bool,
    path: String,
    logged_in: bool,
    has_login_form: bool,
    has_verification: bool,
    error_message: String,
}

impl CdpClient {
    pub(super) async fn login_dian115(
        &self,
        tab_id: &str,
        email: &str,
        password: &str,
        progress: Option<&CdpProgress>,
    ) -> Result<bool, String> {
        let websocket_url = self.websocket_url_for_tab(tab_id)?
            .ok_or_else(|| "无法连接癫影标签页".to_string())?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let initial = wait_for_state(&mut websocket, progress, 120, false).await?;
        if initial.logged_in { return Ok(false); }

        if initial.path != "/login" {
            websocket.call("Page.enable", serde_json::json!({}))?;
            websocket.call("Page.navigate", serde_json::json!({"url": "https://m.dian115.com/login"}))?;
        }
        let page = wait_for_state(&mut websocket, progress, 160, true).await?;
        if page.logged_in { return Ok(false); }
        if page.has_verification {
            return Err("癫影登录页要求人机验证，请在专用浏览器中完成验证".to_string());
        }
        if !page.has_login_form {
            return Err("癫影登录页未找到邮箱和密码表单".to_string());
        }
        if !type_runtime_input(&mut websocket, "input[type=\"email\"][autocomplete=\"email\"]", email, 45, 110).await {
            return Err("未找到癫影邮箱输入框".to_string());
        }
        if !type_runtime_input(&mut websocket, "input[type=\"password\"][autocomplete=\"current-password\"]", password, 45, 110).await {
            return Err("未找到癫影密码输入框".to_string());
        }
        if !click_runtime_element(&mut websocket, "form button[type=\"submit\"]") {
            return Err("未找到癫影登录按钮".to_string());
        }
        let mut last_error = String::new();
        let mut repeated_error = 0;
        for _ in 0..160 {
            check_cancel(progress)?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(state) = page_state(&mut websocket) else { continue };
            if state.logged_in { return Ok(true); }
            if state.has_verification {
                return Err("癫影登录要求人机验证，请在专用浏览器中完成验证".to_string());
            }
            if state.has_login_form && !state.error_message.is_empty() {
                if last_error == state.error_message { repeated_error += 1; }
                else { last_error = state.error_message; repeated_error = 1; }
                if repeated_error >= 3 { return Err(format!("癫影登录失败：{last_error}")); }
            }
        }
        Err("癫影登录后未检测到账户状态，请检查账号、密码或验证要求".to_string())
    }
}

async fn wait_for_state(
    websocket: &mut CdpWebSocket,
    progress: Option<&CdpProgress>,
    steps: usize,
    require_login_surface: bool,
) -> Result<PageState, String> {
    for _ in 0..steps {
        check_cancel(progress)?;
        if let Some(state) = page_state(websocket) {
            if state.ready && (!require_login_surface || state.logged_in || state.has_login_form || state.has_verification) {
                return Ok(state);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err("等待癫影登录页加载超时".to_string())
}

fn page_state(websocket: &mut CdpWebSocket) -> Option<PageState> {
    let expression = r#"(() => {
        const email = document.querySelector('input[type="email"][autocomplete="email"]');
        const password = document.querySelector('input[type="password"][autocomplete="current-password"]');
        const submit = document.querySelector('form button[type="submit"]');
        const account = document.querySelector('a[href="/me"], a[href^="/me/"], a[href="https://m.dian115.com/me"], button .lucide-wallet-icon');
        const verification = document.querySelector('iframe[src*="challenges.cloudflare.com"], iframe[src*="recaptcha"], .cf-turnstile, .g-recaptcha');
        const errors = Array.from(document.querySelectorAll('[role="alert"], [data-sonner-toast], .toast-error, .text-destructive'))
            .map(el => (el.innerText || '').trim()).filter(text => text && text.length < 240);
        return {
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            path: location.pathname,
            loggedIn: Boolean(account) && location.pathname !== '/login',
            hasLoginForm: Boolean(email && password && submit),
            hasVerification: Boolean(verification),
            errorMessage: errors[0] || ''
        };
    })()"#;
    let response = websocket.call("Runtime.evaluate", serde_json::json!({
        "expression": expression,
        "returnByValue": true
    })).ok()?;
    serde_json::from_value(response.get("result")?.get("result")?.get("value")?.clone()).ok()
}

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress.is_some_and(CdpProgress::is_cancelled) { Err(CDP_CANCELLED.to_string()) }
    else { Ok(()) }
}
