//! 六六云（WHMCS）登录流程。

use super::common::{click_runtime_element, human_delay, type_runtime_input};
use crate::cdp::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SixCloudLoginState {
    ready: bool,
    host: String,
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

        let mut initial = wait_for_sixcloud_state(&mut websocket, progress, 120).await?;
        if initial.host != "666clouds.com" && initial.host != "www.666clouds.com" {
            return Err("六六云页面加载失败：当前标签页域名不匹配".to_string());
        }
        if initial.logged_in {
            return Ok(false);
        }

        if !initial.has_login_form {
            navigate(&mut websocket, "https://www.666clouds.com/clientarea.php")?;
            initial = wait_for_sixcloud_state(&mut websocket, progress, 160).await?;
        }
        if initial.logged_in {
            return_to_target(&mut websocket, target_url, progress).await?;
            return Ok(false);
        }
        if initial.has_verification {
            return Err("六六云登录页要求人机验证，请先在专用 Chrome 中人工完成验证".to_string());
        }
        if !initial.has_login_form {
            return Err("六六云登录页已打开，但未找到登录表单".to_string());
        }

        human_delay(550, 1250).await;
        if !type_runtime_input(
            &mut websocket,
            "#inputEmail, input[name=\"username\"][type=\"email\"]",
            username,
            45,
            115,
        )
        .await
        {
            return Err("未找到六六云邮箱输入框".to_string());
        }
        human_delay(450, 1050).await;
        if !type_runtime_input(
            &mut websocket,
            "#inputPassword, input[name=\"password\"], input[type=\"password\"]",
            password,
            55,
            135,
        )
        .await
        {
            return Err("未找到六六云密码输入框".to_string());
        }
        human_delay(700, 1550).await;
        if !click_runtime_element(
            &mut websocket,
            "form.login-form #login, form.login-form input[type=\"submit\"]",
        ) {
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
            if state.logged_in {
                return_to_target(&mut websocket, target_url, progress).await?;
                return Ok(true);
            }
            if state.has_verification {
                return Err("六六云登录要求人机验证，请先在专用 Chrome 中人工完成验证".to_string());
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
) -> Result<SixCloudLoginState, String> {
    let mut last_state = None;
    for _ in 0..steps {
        check_cancel(progress)?;
        if let Some(state) = sixcloud_login_state(websocket) {
            if state.ready
                && (state.logged_in
                    || state.has_login_form
                    || state.has_verification
                    || !state.error_message.is_empty())
            {
                return Ok(state);
            }
            last_state = Some(state);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    last_state.ok_or_else(|| "无法读取六六云页面状态".to_string())
}

fn sixcloud_login_state(websocket: &mut CdpWebSocket) -> Option<SixCloudLoginState> {
    let expression = r#"(() => {
        const clean = (value) => String(value || '').replace(/\s+/g, ' ').trim();
        const visible = (element) => {
            if (!element) return false;
            const style = getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.display !== 'none' && style.visibility !== 'hidden'
                && rect.width > 0 && rect.height > 0;
        };
        const username = document.querySelector('#inputEmail, input[name="username"][type="email"]');
        const password = document.querySelector('#inputPassword, input[name="password"], input[type="password"]');
        const submit = document.querySelector('form.login-form #login, form.login-form input[type="submit"]');
        const verification = document.querySelector(
            'iframe[src*="recaptcha"], iframe[src*="challenges.cloudflare.com"], .g-recaptcha, .cf-turnstile'
        );
        const errors = Array.from(document.querySelectorAll(
            '.alert-danger, .alert-error, [role="alert"], .providerLinkingFeedback'
        )).filter(visible).map((element) => clean(element.textContent)).filter((text) =>
            text && text.length < 240 && /(失败|错误|无效|密码|账户|账号|邮箱|incorrect|invalid|failed)/i.test(text)
        );
        return {
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            host: location.hostname.toLowerCase(),
            loggedIn: Boolean(document.querySelector('a[href*="logout.php"], a[href*="/logout"], #trafficout, #trafficin')),
            hasLoginForm: Boolean(username && password && submit),
            hasVerification: Boolean(verification && visible(verification)),
            errorMessage: errors[0] || ''
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
    let value = response.get("result")?.get("result")?.get("value")?.clone();
    serde_json::from_value(value).ok()
}

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress.is_some_and(CdpProgress::is_cancelled) {
        Err(CDP_CANCELLED.to_string())
    } else {
        Ok(())
    }
}
