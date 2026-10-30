//! 六六云新版客户中心登录，兼容旧版 WHMCS 表单及入口。

use super::common::{click_runtime_element, human_delay, type_runtime_input};
use crate::cdp::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use serde::Deserialize;
use std::time::Duration;

const USERNAME_SELECTOR: &str =
    "form input[name=\"email\"], form #inputEmail, form input[name=\"username\"][type=\"email\"]";
const PASSWORD_SELECTOR: &str =
    "form input[name=\"password\"], form #inputPassword, form input[type=\"password\"]";
const SUBMIT_SELECTOR: &str =
    "form:has(input[name=\"email\"]) button[type=\"submit\"], form.login-form #login, form.login-form input[type=\"submit\"]";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SixCloudLoginState {
    ready: bool,
    host: String,
    path: String,
    logged_in: bool,
    has_login_form: bool,
    has_verification: bool,
    error_message: String,
}

impl CdpClient {
    pub(super) async fn login_sixcloud(
        &self,
        tab_id: &str,
        target_url: &str,
        username: &str,
        password: &str,
        progress: Option<&CdpProgress>,
    ) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接六六云标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;

        let mut initial = wait_for_sixcloud_state(&mut websocket, progress, 120, false).await?;
        if initial.logged_in {
            return Ok(false);
        }

        if !initial.has_login_form {
            // 新版 /portal 会检查现有会话，未登录时自动转到 /login?next=/portal。
            navigate(&mut websocket, "https://www.666clouds.com/portal")?;
            initial = wait_for_sixcloud_state(&mut websocket, progress, 160, true).await?;
        }
        if initial.logged_in {
            return_to_target(&mut websocket, target_url, progress).await?;
            return Ok(false);
        }
        if initial.has_verification {
            return Err("六六云登录页要求人机验证，请先在专用浏览器中人工完成验证".to_string());
        }
        if !initial.has_login_form {
            return Err("六六云登录页已打开，但未找到登录表单".to_string());
        }

        human_delay(550, 1250).await;
        check_cancel(progress)?;
        ensure_current_site(&mut websocket)?;
        if !type_runtime_input(
            &mut websocket,
            USERNAME_SELECTOR,
            username.trim(),
            45,
            115,
        )
        .await
        {
            return Err("未找到六六云邮箱输入框".to_string());
        }
        human_delay(450, 1050).await;
        check_cancel(progress)?;
        ensure_current_site(&mut websocket)?;
        if !type_runtime_input(
            &mut websocket,
            PASSWORD_SELECTOR,
            password,
            55,
            135,
        )
        .await
        {
            return Err("未找到六六云密码输入框".to_string());
        }
        human_delay(700, 1550).await;
        check_cancel(progress)?;
        ensure_current_site(&mut websocket)?;
        if !click_runtime_element(&mut websocket, SUBMIT_SELECTOR) {
            return Err("未找到六六云登录按钮".to_string());
        }

        let mut stable_error = String::new();
        let mut stable_error_steps = 0usize;
        for _ in 0..160 {
            check_cancel(progress)?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(state) = sixcloud_login_state(&mut websocket) else {
                continue;
            };
            ensure_site(&state)?;
            if state.logged_in {
                return_to_target(&mut websocket, target_url, progress).await?;
                return Ok(true);
            }
            if state.has_verification {
                return Err("六六云登录要求人机验证，请先在专用浏览器中人工完成验证".to_string());
            }
            if !state.error_message.is_empty() && state.has_login_form {
                if stable_error == state.error_message {
                    stable_error_steps += 1;
                } else {
                    stable_error = state.error_message;
                    stable_error_steps = 1;
                }
                if stable_error_steps >= 3 {
                    return Err(format!("六六云登录失败：{stable_error}"));
                }
            } else {
                stable_error.clear();
                stable_error_steps = 0;
            }
        }

        Err("六六云登录后未检测到账户状态，请检查账号、密码或验证要求".to_string())
    }
}

async fn return_to_target(
    websocket: &mut CdpWebSocket,
    target_url: &str,
    progress: Option<&CdpProgress>,
) -> Result<(), String> {
    navigate(websocket, target_url)?;
    for _ in 0..160 {
        check_cancel(progress)?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let Some(state) = sixcloud_login_state(websocket) else {
            continue;
        };
        ensure_site(&state)?;
        if state.ready && state.logged_in {
            return Ok(());
        }
    }
    Err("六六云登录成功，但返回产品详情页超时".to_string())
}

fn navigate(websocket: &mut CdpWebSocket, url: &str) -> Result<(), String> {
    websocket.call("Page.enable", serde_json::json!({}))?;
    websocket.call("Page.navigate", serde_json::json!({ "url": url }))?;
    Ok(())
}

async fn wait_for_sixcloud_state(
    websocket: &mut CdpWebSocket,
    progress: Option<&CdpProgress>,
    steps: usize,
    require_login_surface: bool,
) -> Result<SixCloudLoginState, String> {
    let mut last_state = None;
    for _ in 0..steps {
        check_cancel(progress)?;
        if let Some(state) = sixcloud_login_state(websocket) {
            ensure_site(&state)?;
            if state.ready
                && (state.logged_in
                    || state.has_login_form
                    || state.has_verification
                    || !state.error_message.is_empty())
            {
                return Ok(state);
            }
            // 公开首页不包含登录状态标记，不必等满 30 秒才打开客户中心。
            if state.ready && !require_login_surface && state.path != "/login" {
                return Ok(state);
            }
            last_state = Some(state);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    last_state.ok_or_else(|| "无法读取六六云页面状态".to_string())
}

fn sixcloud_login_state(websocket: &mut CdpWebSocket) -> Option<SixCloudLoginState> {
    let expression = format!(
        "({LOGIN_STATE_FUNCTION})({})",
        serde_json::json!({
            "username": USERNAME_SELECTOR,
            "password": PASSWORD_SELECTOR,
            "submit": SUBMIT_SELECTOR
        })
    );
    let response = websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true
            }),
        )
        .ok()?;
    let value = response.get("result")?.get("result")?.get("value")?.clone();
    serde_json::from_value(value).ok()
}

fn ensure_site(state: &SixCloudLoginState) -> Result<(), String> {
    if matches!(state.host.as_str(), "666clouds.com" | "www.666clouds.com") {
        Ok(())
    } else {
        Err("六六云页面跳转到了其他域名，已停止填写登录信息".to_string())
    }
}

fn ensure_current_site(websocket: &mut CdpWebSocket) -> Result<(), String> {
    let state = sixcloud_login_state(websocket)
        .ok_or_else(|| "无法确认六六云当前页面，已停止填写登录信息".to_string())?;
    ensure_site(&state)
}

const LOGIN_STATE_FUNCTION: &str = r#"({ username: usernameSelector, password: passwordSelector, submit: submitSelector }) => {
        const clean = (value) => String(value || '').replace(/\s+/g, ' ').trim();
        const visible = (element) => {
            if (!element) return false;
            const style = getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.display !== 'none' && style.visibility !== 'hidden'
                && rect.width > 0 && rect.height > 0;
        };
        const username = document.querySelector(usernameSelector);
        const password = document.querySelector(passwordSelector);
        const submit = document.querySelector(submitSelector);
        const hasLoginForm = Boolean(username && password && submit
            && username.form === password.form && password.form === submit.form);
        const hasVerification = Array.from(document.querySelectorAll(
            'iframe[src*="recaptcha"], iframe[src*="challenges.cloudflare.com"], .g-recaptcha, .cf-turnstile, #challenge-stage'
        )).some(visible);
        const logout = document.querySelector(
            'a[href*="logout.php"], a[href="/logout"], a[href="/api/auth/logout"], form[action="/logout"], form[action="/api/auth/logout"], #trafficout, #trafficin'
        ) || Array.from(document.querySelectorAll('button')).find(element =>
            visible(element) && /^(退出登录|退出|注销|登出|log\s*out|sign\s*out)$/i.test(clean(element.textContent))
        );
        // 新客户中心将退出操作放在账户菜单中，首屏不一定存在退出按钮。
        // 受保护的 /portal 页面和账户、服务导航同时出现时，才确认已有会话。
        const portal = /^\/portal(?:\/|$)/.test(location.pathname)
            && Boolean(document.querySelector('a[href="/portal/profile"]'))
            && Boolean(document.querySelector('a[href="/portal/services"]'));
        const errors = Array.from(document.querySelectorAll(
            '.alert-danger, .alert-error, [role="alert"], .providerLinkingFeedback, form .text-danger, form .text-red-500, form .text-red-600, form .text-red-700, form [data-error]'
        )).filter(visible).map((element) => clean(element.textContent)).filter((text) =>
            text && text.length < 240 && /(失败|错误|无效|密码|账户|账号|邮箱|不正确|锁定|禁用|incorrect|invalid|failed|too many)/i.test(text)
        );
        return {
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            host: location.hostname.toLowerCase(),
            path: location.pathname,
            loggedIn: Boolean(logout || portal) && !hasLoginForm && !hasVerification,
            hasLoginForm,
            hasVerification,
            errorMessage: errors[0] || ''
        };
    }"#;

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress.is_some_and(CdpProgress::is_cancelled) {
        Err(CDP_CANCELLED.to_string())
    } else {
        Ok(())
    }
}
