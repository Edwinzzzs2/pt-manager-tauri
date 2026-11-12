use super::{check_cancel, inspect_page, PageState};
use crate::cdp::{CdpProgress, CdpWebSocket};
use reqwest::Url;
use serde_json::json;
use std::time::Duration;

const VERIFICATION_REQUIRED: &str = "杜比两步验证未确认通过，请在专用浏览器完成验证后重试签到";

fn site_url(value: &str) -> Option<Url> {
    Url::parse(value).ok().filter(|url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && matches!(url.host_str(), Some("uhddolby.com" | "www.uhddolby.com"))
    })
}

pub(super) fn is_site(value: &str) -> bool {
    site_url(value).is_some()
}

pub(super) fn is_verification_page(value: &str) -> bool {
    site_url(value).is_some_and(|url| url.path() == "/take2fa.php")
}

pub(super) fn same_page(left: &str, right: &str) -> bool {
    site_url(left)
        .zip(site_url(right))
        .is_some_and(|(left, right)| left.path() == right.path())
}

pub(super) async fn verify(
    websocket: &mut CdpWebSocket,
    secret: Option<&str>,
    progress: Option<&CdpProgress>,
) -> Result<PageState, String> {
    check_cancel(progress)?;
    let secret = secret
        .filter(|secret| !secret.trim().is_empty())
        .ok_or_else(|| {
            "杜比要求两步验证，请先在站点设置填写 2FA 密钥，或在专用浏览器手动验证".to_string()
        })?;

    for _ in 0..20 {
        check_cancel(progress)?;
        if verification_form(websocket, None).unwrap_or(false) {
            let code = crate::auth::fresh_totp(secret).await?;
            check_cancel(progress)?;
            if !verification_form(websocket, Some(&code))? {
                return Err(VERIFICATION_REQUIRED.to_string());
            }
            if let Some(progress) = progress {
                progress.info("杜比已提交两步验证，正在等待站点确认").await;
            }
            return wait_for_verification(websocket, progress).await;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err("未找到杜比两步验证表单，请在专用浏览器完成验证后重试签到".to_string())
}

async fn wait_for_verification(
    websocket: &mut CdpWebSocket,
    progress: Option<&CdpProgress>,
) -> Result<PageState, String> {
    for _ in 0..40 {
        check_cancel(progress)?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let Ok(state) = inspect_page(websocket) else {
            continue;
        };
        if !state.ready || state.has_challenge || is_verification_page(&state.current_url) {
            continue;
        }
        if !is_site(&state.current_url) || state.has_login_form || state.explicit_failure {
            break;
        }
        if let Some(progress) = progress {
            progress.info("杜比两步验证通过，继续签到").await;
        }
        return Ok(state);
    }
    Err(VERIFICATION_REQUIRED.to_string())
}

fn verification_form(websocket: &mut CdpWebSocket, code: Option<&str>) -> Result<bool, String> {
    let expression = format!("{VERIFICATION_FORM}({})", json!(code));
    let response = websocket
        .call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "userGesture": code.is_some()
            }),
        )
        .map_err(|_| VERIFICATION_REQUIRED.to_string())?;
    Ok(response
        .pointer("/result/result/value")
        .and_then(|value| value.as_bool())
        == Some(true))
}

const VERIFICATION_FORM: &str = r#"(code => {
    if (!['https://uhddolby.com', 'https://www.uhddolby.com'].includes(location.origin)
        || location.pathname !== '/take2fa.php') return false;
    const input = document.querySelector('input[name="2fa"]');
    const form = input?.form;
    // NexusPHP 将表单控件放在表格行里。它们归属于表单，但不一定是表单的后代节点。
    const submit = form && Array.from(form.elements).find(element => element.type === 'submit');
    if (!form || !submit || input.matches(':disabled, [readonly]') || submit.matches(':disabled')) return false;

    const action = new URL(submit.getAttribute('formaction') || form.action, document.baseURI);
    const method = submit.getAttribute('formmethod') || form.method;
    if (action.origin !== location.origin || action.pathname !== '/take2fa.php'
        || method.toLowerCase() !== 'post') return false;
    if (code === null) return true;

    input.value = code;
    submit.click();
    return true;
})"#;
