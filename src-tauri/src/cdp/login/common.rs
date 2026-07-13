//! 各站点适配器共享的页面状态读取、拟人输入、点击和 OTP 提交能力。

use crate::cdp::CdpWebSocket;
use rand::Rng;
use std::time::Duration;

pub(super) struct LoginPageState {
    pub(super) host: String,
    pub(super) path: String,
    pub(super) ready: bool,
    pub(super) has_login_form: bool,
    pub(super) has_otp: bool,
    pub(super) logged_in: bool,
}

pub(super) struct HdkLoginPageState {
    pub(super) path: String,
    pub(super) ready: bool,
    pub(super) has_login_form: bool,
    pub(super) has_otp: bool,
    pub(super) has_captcha: bool,
    pub(super) challenge: bool,
    pub(super) blocked_debug: bool,
}

#[allow(dead_code)]
pub(super) struct NexusCaptchaPageState {
    pub(super) path: String,
    pub(super) ready: bool,
    pub(super) has_login_form: bool,
    pub(super) has_captcha: bool,
    pub(super) challenge: bool,
    pub(super) remaining_attempts: Option<u32>,
}

pub(super) fn nexus_captcha_page_state(
    websocket: &mut CdpWebSocket,
) -> Option<NexusCaptchaPageState> {
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
    Some(NexusCaptchaPageState {
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
pub(super) struct NexusLoginPageState {
    pub(super) path: String,
    pub(super) ready: bool,
    pub(super) has_login_form: bool,
    pub(super) has_captcha: bool,
    pub(super) has_two_factor: bool,
    pub(super) challenge: bool,
    pub(super) logged_in: bool,
    pub(super) is_failure_page: bool,
    pub(super) remaining_attempts: Option<u32>,
}

pub(super) fn nexus_login_page_state(websocket: &mut CdpWebSocket) -> Option<NexusLoginPageState> {
    let expression = r#"(() => {
        const username = document.querySelector('input[name="username"], input[name="email"], input[name="login"], input[autocomplete="username"]');
        const password = document.querySelector('input[name="password"], input[type="password"]');
        const captcha = document.querySelector('input[name="imagestring"]');
        const two_factor = document.querySelector('input[name="two_factor"], input[name="twofactor"], input[name="two_step_code"], input[name="otp"], input[name="two_factor_code"], input[name="2fa_secret"], input[name="2fa"], input[autocomplete="one-time-code"]');
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

pub(super) fn hdk_login_page_state(websocket: &mut CdpWebSocket) -> Option<HdkLoginPageState> {
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

pub(super) async fn human_delay(min_millis: u64, max_millis: u64) {
    let delay = rand::thread_rng().gen_range(min_millis..=max_millis);
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

pub(super) async fn type_runtime_input(
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

pub(super) fn fill_runtime_input(
    websocket: &mut CdpWebSocket,
    selector: &str,
    value: &str,
) -> bool {
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

pub(super) fn click_runtime_element(websocket: &mut CdpWebSocket, selector: &str) -> bool {
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

pub(super) fn login_page_state(websocket: &mut CdpWebSocket) -> Option<LoginPageState> {
    let expression = r#"(() => {
        const inputs = Array.from(document.querySelectorAll('input'));
        const rawBody = (document.body?.innerText || '').slice(0, 10000);
        const body = rawBody.toLowerCase();
        const logout = document.querySelector('a[href*="logout"], a[href*="signout"]');
        const hasUpload = rawBody.includes('上传') || rawBody.includes('上傳') || body.includes('uploaded');
        const hasDownload = rawBody.includes('下载') || rawBody.includes('下載') || body.includes('downloaded');
        const hasRatio = rawBody.includes('分享率') || body.includes('ratio');
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
            hasOtp,
            loggedIn: Boolean(logout)
                || (hasUpload && hasDownload)
                || (hasRatio && (hasUpload || hasDownload))
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
        logged_in: value.get("loggedIn")?.as_bool()?,
    })
}

pub(super) fn runtime_object_bool(
    websocket: &mut CdpWebSocket,
    expression: &str,
    key: &str,
) -> Option<bool> {
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

pub(super) async fn submit_otp(websocket: &mut CdpWebSocket, code: &str) -> bool {
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
