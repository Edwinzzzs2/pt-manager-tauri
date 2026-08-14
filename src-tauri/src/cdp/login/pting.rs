//! PTing（蜂巢）论坛登录流程。

use super::common::{click_runtime_element, human_delay, submit_otp, type_runtime_input};
use crate::auth;
use crate::cdp::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PtingLoginState {
    ready: bool,
    path: String,
    logged_in: bool,
    has_login_form: bool,
    has_otp: bool,
    has_verification: bool,
    error_message: String,
}

impl CdpClient {
    /// PTing 是 Next.js 论坛，登录表单和登录态都不同于 NexusPHP。
    pub(super) async fn login_pting(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
        progress: Option<&CdpProgress>,
    ) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 PTing 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;

        let initial = wait_for_pting_state(&mut websocket, progress, 120, false).await?;
        if initial.logged_in {
            return Ok(false);
        }

        if initial.path != "/login" {
            websocket.call("Page.enable", serde_json::json!({}))?;
            websocket.call(
                "Page.navigate",
                serde_json::json!({ "url": "https://pting.club/login" }),
            )?;
        }

        let login_page = wait_for_pting_state(&mut websocket, progress, 160, true).await?;
        if login_page.logged_in {
            return Ok(false);
        }
        if !login_page.has_login_form {
            return Err("PTing 登录页已打开，但登录表单未加载完成".to_string());
        }
        if login_page.has_verification {
            return Err(
                "PTing 登录页要求验证码或工作量证明，需要先在专用 Chrome 中人工完成验证"
                    .to_string(),
            );
        }

        human_delay(550, 1250).await;
        if !type_runtime_input(
            &mut websocket,
            "#login-identity, input[name=\"login\"], input[autocomplete=\"username\"]",
            username,
            45,
            115,
        )
        .await
        {
            return Err("未找到 PTing 邮箱、用户名或手机号输入框".to_string());
        }
        human_delay(450, 1050).await;
        if !type_runtime_input(
            &mut websocket,
            "#login-password, input[name=\"password\"], input[type=\"password\"]",
            password,
            55,
            135,
        )
        .await
        {
            return Err("未找到 PTing 密码输入框".to_string());
        }
        human_delay(700, 1550).await;
        if !click_runtime_element(
            &mut websocket,
            "form button[type=\"submit\"], button[type=\"submit\"]",
        ) {
            return Err("未找到 PTing 登录按钮".to_string());
        }

        let mut otp_submitted = false;
        let mut stable_error = String::new();
        let mut stable_error_steps = 0usize;
        for _ in 0..160 {
            check_cancel(progress)?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(state) = pting_login_state(&mut websocket) else {
                continue;
            };
            if state.logged_in {
                return Ok(true);
            }
            if state.has_otp && !otp_submitted {
                let secret = totp_secret
                    .ok_or_else(|| "PTing 要求 2FA，但站点未配置 2FA 密钥".to_string())?;
                let code = auth::current_totp(secret)?;
                if !submit_otp(&mut websocket, &code).await {
                    return Err("检测到 PTing 2FA，但未能填写或提交验证码".to_string());
                }
                otp_submitted = true;
                stable_error_steps = 0;
                continue;
            }
            if state.has_verification {
                return Err(
                    "PTing 登录要求验证码或工作量证明，需要先在专用 Chrome 中人工完成验证"
                        .to_string(),
                );
            }
            if !state.error_message.is_empty() && state.has_login_form {
                if stable_error == state.error_message {
                    stable_error_steps += 1;
                } else {
                    stable_error = state.error_message;
                    stable_error_steps = 1;
                }
                if stable_error_steps >= 3 {
                    return Err(format!("PTing 登录失败：{stable_error}"));
                }
            }
        }

        Err("PTing 登录后未检测到用户状态，请检查账号、密码、2FA 或验证要求".to_string())
    }
}

async fn wait_for_pting_state(
    websocket: &mut CdpWebSocket,
    progress: Option<&CdpProgress>,
    steps: usize,
    require_login_surface: bool,
) -> Result<PtingLoginState, String> {
    let mut last_state = None;
    for _ in 0..steps {
        check_cancel(progress)?;
        if let Some(state) = pting_login_state(websocket) {
            if state.ready
                && (!require_login_surface
                    || state.logged_in
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
    last_state.ok_or_else(|| "无法读取 PTing 页面状态".to_string())
}

fn pting_login_state(websocket: &mut CdpWebSocket) -> Option<PtingLoginState> {
    let expression = r#"(() => {
        const clean = (value) => String(value || '').replace(/\s+/g, ' ').trim();
        const visible = (element) => {
            if (!element) return false;
            const style = getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.display !== 'none' && style.visibility !== 'hidden'
                && rect.width > 0 && rect.height > 0;
        };
        const inputs = Array.from(document.querySelectorAll('input')).filter(visible);
        const hint = (input) => clean([
            input.id,
            input.name,
            input.placeholder,
            input.autocomplete,
            input.getAttribute('aria-label')
        ].filter(Boolean).join(' ')).toLowerCase();
        const username = document.querySelector('#login-identity, input[name="login"], input[autocomplete="username"]');
        const password = document.querySelector('#login-password, input[name="password"], input[type="password"]');
        const submit = document.querySelector('form button[type="submit"], button[type="submit"]');
        const hasOtp = inputs.some((input) => /(otp|totp|2fa|one.?time|两步|二次验证)/i.test(hint(input)));
        const hasVerification = inputs.some((input) =>
            !hasOtp && /(captcha|图形验证码|工作量证明|pow)/i.test(hint(input))
        ) || Boolean(document.querySelector(
            'iframe[src*="challenges.cloudflare.com"], iframe[src*="recaptcha"], .cf-turnstile, .g-recaptcha'
        ));
        const errors = Array.from(document.querySelectorAll(
            '[role="alert"], [data-sonner-toast], [data-type="error"], .text-destructive, .toast-error'
        )).filter(visible).map((element) => clean(element.textContent)).filter((text) =>
            text && text.length < 240 && /(失败|错误|无效|不存在|密码|账号|验证码|error|invalid|incorrect)/i.test(text)
        );
        return {
            ready: document.readyState === 'interactive' || document.readyState === 'complete',
            path: location.pathname,
            loggedIn: Boolean(document.querySelector('button[aria-label="打开用户菜单"]')),
            hasLoginForm: Boolean(username && password && submit),
            hasOtp,
            hasVerification,
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
