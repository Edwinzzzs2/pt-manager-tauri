//! 聚影每日签到：只点击签到区的按钮，使用按钮和今日状态共同确认结果。

use super::{check_cancel, CdpClient, CdpProgress, CdpWebSocket, SigninResult, SigninStatus};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckinPageState {
    ready: bool,
    host: String,
    path: String,
    has_login_form: bool,
    loaded: bool,
    already_signed: bool,
    can_click: bool,
    error_message: String,
    total_days: Option<u32>,
    reward_points: Option<u32>,
}

impl CdpClient {
    pub(super) async fn signin_jying(
        &self,
        tab_id: &str,
        site_url: &str,
        progress: Option<&CdpProgress>,
    ) -> Result<SigninResult, String> {
        check_cancel(progress)?;
        let websocket_url = self.websocket_url_for_tab(tab_id)?
            .ok_or_else(|| "无法连接聚影签到标签页".to_string())?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let target = reqwest::Url::parse(site_url)
            .and_then(|url| url.join("/checkin"))
            .map_err(|err| format!("聚影签到地址无效：{err}"))?;
        if let Some(p) = progress {
            p.info("聚影正在打开每日签到页面".to_string()).await;
        }
        websocket.call("Page.enable", serde_json::json!({}))?;
        websocket.call("Page.navigate", serde_json::json!({"url": target.as_str()}))?;

        let mut clicked = false;
        for _ in 0..120 {
            check_cancel(progress)?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let Some(state) = inspect_page(&mut websocket) else { continue };
            if !matches!(state.host.as_str(), "jying.top" | "www.jying.top") {
                return Ok(SigninResult::failure("聚影签到页跳转到其他域名，已停止操作"));
            }
            if !state.ready { continue; }
            if state.has_login_form || state.path == "/login" {
                return Ok(SigninResult::failure("聚影登录状态已失效，请先登录"));
            }
            if !state.error_message.is_empty() {
                return Ok(SigninResult::failure(format!("聚影签到失败：{}", state.error_message)));
            }
            if state.path != "/checkin" || !state.loaded { continue; }
            if state.already_signed {
                // 进入页面就已完成属于此前签到；本次点击后完成才记为本次成功。
                return Ok(SigninResult {
                    status: if clicked { SigninStatus::Success } else { SigninStatus::AlreadySigned },
                    message: if clicked { "聚影签到成功" } else { "聚影今天已经签到" }.to_string(),
                    reward: if clicked { state.reward_points.map(|points| format!("{points} 积分")) } else { None },
                    total_days: state.total_days,
                    consecutive_days: None,
                });
            }
            if !clicked && state.can_click {
                check_cancel(progress)?;
                clicked = click_checkin(&mut websocket);
                if clicked {
                    if let Some(p) = progress {
                        p.info("聚影已点击立即签到，正在确认今日状态".to_string()).await;
                    }
                }
            }
        }
        Ok(SigninResult::failure(if clicked {
            "聚影已点击签到，但等待今日状态更新超时，请检查浏览器提示"
        } else {
            "聚影签到页未加载出可用的签到按钮或统计，请检查登录状态及站点提示"
        }))
    }
}

fn inspect_page(websocket: &mut CdpWebSocket) -> Option<CheckinPageState> {
    let response = websocket.call("Runtime.evaluate", serde_json::json!({
        "expression": CHECKIN_STATE_EXPRESSION,
        "returnByValue": true
    })).ok()?;
    serde_json::from_value(response.get("result")?.get("result")?.get("value")?.clone()).ok()
}

fn click_checkin(websocket: &mut CdpWebSocket) -> bool {
    websocket.call("Runtime.evaluate", serde_json::json!({
        "expression": CLICK_CHECKIN_EXPRESSION,
        "returnByValue": true,
        "userGesture": true
    })).ok()
        .and_then(|response| response.get("result")?.get("result")?.get("value")?.as_bool())
        .unwrap_or(false)
}

const CHECKIN_STATE_EXPRESSION: &str = r#"(() => {
    const clean = el => String(el?.textContent || '').replace(/\s+/g, ' ').trim();
    const stats = Array.from(document.querySelectorAll('.checkin-grid .soft-stat'));
    const stat = label => clean(stats.find(el => clean(el.querySelector('small')) === label)?.querySelector('strong'));
    const number = label => { const value = stat(label).replace(/,/g, ''); return /^\d+$/.test(value) ? Number(value) : null; };
    const button = document.querySelector('.checkin-cta button');
    const label = clean(button);
    const errors = Array.from(document.querySelectorAll('.n-message--error-type, .n-alert--error-type'))
        .map(clean).filter(text => text && text.length < 240);
    return {
        ready: document.readyState === 'interactive' || document.readyState === 'complete',
        host: location.hostname.toLowerCase(),
        path: location.pathname,
        hasLoginForm: Boolean(document.querySelector('.auth-card input[type="password"]')),
        // 签到页加载统计前也会出现按钮，必须等今日状态加载完成后再决定是否点击。
        loaded: Boolean(button) && ['未签到', '已完成'].includes(stat('今日状态')),
        alreadySigned: stat('今日状态') === '已完成' && label === '今天已经签到' && Boolean(button?.disabled),
        canClick: stat('今日状态') === '未签到' && label === '立即签到'
            && !button?.disabled && !button?.classList.contains('n-button--loading'),
        errorMessage: errors[0] || '',
        totalDays: number('累计签到'),
        rewardPoints: number('今日奖励')
    };
})()"#;

const CLICK_CHECKIN_EXPRESSION: &str = r#"(() => {
    // 页面还有幸运奖池等操作，只允许点击每日签到区的“立即签到”。
    const button = document.querySelector('.checkin-cta button');
    if (!button || button.disabled || button.classList.contains('n-button--loading')
        || button.textContent.trim() !== '立即签到') return false;
    button.click();
    return true;
})()"#;
