use super::{CdpClient, CdpWebSocket};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const BONUS_URL: &str = "https://www.qingwapt.com/bonusshop.php";

#[derive(Deserialize)]
struct BonusState {
    status: String,
    detail: String,
}

pub fn is_qingwa_site(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    matches!(
        authority.to_ascii_lowercase().as_str(),
        "qingwapt.com" | "www.qingwapt.com"
    )
}

impl CdpClient {
    pub async fn purchase_qingwa_daily_bonus(&self, tab_id: &str) -> Result<String, String> {
        let websocket_url = self
            .websocket_url_for_tab(tab_id)?
            .ok_or_else(|| "未找到青蛙站点标签页".to_string())?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        websocket.call("Page.enable", json!({}))?;
        websocket.call("Page.navigate", json!({ "url": BONUS_URL }))?;

        // 等待商店脚本渲染商品。只在商品名称、兑换说明、限购和价格都匹配时才允许点击。
        let mut last_detail = String::new();
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let state = match inspect_bonus(&mut websocket) {
                Ok(state) => state,
                Err(err) => {
                    last_detail = err;
                    continue;
                }
            };
            match state.status.as_str() {
                "ready" => return complete_purchase(&mut websocket).await,
                "already" => {
                    return Ok(format!(
                        "每日福利已领取或今日已尝试，跳过：{}",
                        state.detail
                    ))
                }
                "login" => return Err("福利商店跳转到登录页，请检查登录状态".to_string()),
                _ => last_detail = state.detail,
            }
        }
        Err(format!("未找到可安全购买的每日福利，已跳过：{last_detail}"))
    }
}

async fn complete_purchase(websocket: &mut CdpWebSocket) -> Result<String, String> {
    if evaluate(websocket, BONUS_OPEN_DIALOG_EXPRESSION)?.as_bool() != Some(true) {
        return Err("每日福利商品在打开确认弹窗前发生变化，已跳过".to_string());
    }

    // 商店需要在商品卡片和确认弹窗中各点一次购买，提交前再次核对商品、价格和数量。
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let modal = inspect_expression(websocket, BONUS_MODAL_EXPRESSION)?;
        match modal.status.as_str() {
            "ready" => break,
            "mismatch" => return Err(format!("每日福利确认弹窗不匹配：{}", modal.detail)),
            _ => continue,
        }
    }
    let modal = inspect_expression(websocket, BONUS_MODAL_EXPRESSION)?;
    if modal.status != "ready" {
        return Err("未出现每日福利购买确认弹窗，已跳过提交".to_string());
    }
    if evaluate(websocket, BONUS_CONFIRM_EXPRESSION)?.as_bool() != Some(true) {
        return Err("每日福利确认弹窗在提交前发生变化，已跳过".to_string());
    }

    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let result = match inspect_expression(websocket, BONUS_RESULT_EXPRESSION) {
            Ok(result) => result,
            Err(_) => continue,
        };
        match result.status.as_str() {
            "success" => return Ok(format!("每日福利购买成功：{}", result.detail)),
            "already" => return Ok(format!("每日福利今日已达限购：{}", result.detail)),
            "failed" => return Err(format!("每日福利购买失败：{}", result.detail)),
            _ => continue,
        }
    }
    Err("已提交每日福利购买，但未读取到站点结果；今日不自动重试，请检查订单记录".to_string())
}

fn inspect_bonus(websocket: &mut CdpWebSocket) -> Result<BonusState, String> {
    inspect_expression(websocket, BONUS_STATE_EXPRESSION)
}

fn inspect_expression(
    websocket: &mut CdpWebSocket,
    expression: &str,
) -> Result<BonusState, String> {
    let value = evaluate(websocket, expression)?;
    serde_json::from_value(value).map_err(|err| format!("解析青蛙福利页面失败：{err}"))
}

fn evaluate(websocket: &mut CdpWebSocket, expression: &str) -> Result<serde_json::Value, String> {
    let response = websocket
        .call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": true, "userGesture": true }),
        )?;
    if let Some(exception) = response
        .get("result")
        .and_then(|result| result.get("exceptionDetails"))
    {
        let description = exception
            .get("exception")
            .and_then(|value| value.get("description"))
            .and_then(|value| value.as_str())
            .or_else(|| exception.get("text").and_then(|value| value.as_str()))
            .unwrap_or("未知页面异常");
        return Err(format!(
            "青蛙福利页面脚本异常：{}",
            description.chars().take(180).collect::<String>()
        ));
    }
    response
        .get("result")
        .and_then(|result| result.get("result"))
        .and_then(|result| result.get("value"))
        .cloned()
        .ok_or_else(|| "读取青蛙福利页面状态失败".to_string())
}

const BONUS_STATE_EXPRESSION: &str = r#"(async () => {
    if (!['qingwapt.com', 'www.qingwapt.com'].includes(location.hostname)) {
        if (document.readyState !== 'complete') return { status: 'waiting', detail: location.href };
        return { status: 'login', detail: location.href };
    }
    if (location.pathname.includes('login.php') || document.querySelector('input[type="password"]'))
        return { status: 'login', detail: location.pathname };
    const cards = [...document.querySelectorAll('#items .item')].filter(card =>
        card.querySelector('.name')?.textContent.trim() === '每日福利：1000蝌蚪');
    if (cards.length === 0) return { status: 'waiting', detail: '等待福利商品加载' };
    if (cards.length !== 1) return { status: 'mismatch', detail: `商品卡片数量 ${cards.length}` };
    const card = cards[0];
    const description = card.querySelector('.description')?.innerText || '';
    const price = card.querySelector('.info li[title="价格"]')?.innerText.trim();
    const remaining = card.querySelector('.info li[title="限购"]')?.innerText.trim();
    const buttons = [...card.querySelectorAll('.info button')].filter(button => button.innerText.trim() === '购买');
    if (!/1\s*蝌蚪\s*兑换\s*1000\s*蝌蚪/.test(description) ||
        !/每日限购数量\s*[：:]\s*1/.test(description) || price !== '1蝌蚪' || buttons.length !== 1)
        return { status: 'mismatch', detail: `商品页面信息不符：价格 ${price}，购买按钮 ${buttons.length} 个` };
    let items;
    try {
        const response = await fetch('/api/bonus-shop/getItems');
        items = await response.json();
    } catch {
        return { status: 'waiting', detail: '读取福利库存失败' };
    }
    if (!Array.isArray(items)) return { status: 'mismatch', detail: '商品接口未返回列表' };
    const matches = items.filter(item => item.name === '每日福利：1000蝌蚪');
    if (matches.length !== 1) return { status: 'mismatch', detail: `商品接口匹配数量 ${matches.length}` };
    const item = matches[0];
    if (item.a_type !== 'bonus' || Number(item.a_amount) !== 1 ||
        item.b_type !== 'bonus' || Number(item.b_amount) !== 1000 ||
        Number(item.limit_amount) !== 1 || Number(item.stock) < 1)
        return { status: 'mismatch', detail: '商品接口中的价格、奖励、库存或每日限购已变化' };
    if (Number(item.limit) <= 0 || remaining === '0')
        return { status: 'already', detail: '站点显示今日剩余限购为 0' };
    if (remaining !== String(item.limit))
        return { status: 'mismatch', detail: `页面限购 ${remaining} 与接口限购 ${item.limit} 不一致` };
    const day = new Date().toLocaleDateString('sv-SE');
    if (localStorage.getItem('pt-manager-qingwa-daily-bonus') === day)
        return { status: 'already', detail: '本地记录今日已尝试购买' };
    const button = buttons[0];
    if (button.disabled || button.getAttribute('aria-disabled') === 'true')
        return { status: 'already', detail: '购买按钮不可用' };
    button.dataset.ptManagerQingwaBonus = 'true';
    window.__ptManagerQingwaItemId = String(item.id);
    return { status: 'ready', detail: '商品名称、价格、奖励和剩余限购已核对' };
})()"#;

const BONUS_OPEN_DIALOG_EXPRESSION: &str = r#"(() => {
    const button = document.querySelector('#items .item .info button[data-pt-manager-qingwa-bonus="true"]');
    const card = button?.closest('.item');
    if (!button || !card || button.disabled ||
        !['qingwapt.com', 'www.qingwapt.com'].includes(location.hostname) ||
        card.querySelector('.name')?.textContent.trim() !== '每日福利：1000蝌蚪' ||
        card.querySelector('li[title="价格"]')?.innerText.trim() !== '1蝌蚪' ||
        card.querySelector('li[title="限购"]')?.innerText.trim() !== '1') return false;
    button.click();
    return true;
})()"#;

const BONUS_MODAL_EXPRESSION: &str = r#"(() => {
    const form = document.querySelector('#exchange_form');
    const modal = form?.closest('.layui-layer');
    if (!modal) return { status: 'waiting', detail: '等待确认弹窗' };
    const title = modal.querySelector('.layui-layer-title')?.innerText.replace(/\s+/g, ' ').trim();
    const itemId = form.querySelector('input[name="id"]')?.value;
    const amount = form.querySelector('input[name="amount"]')?.value;
    const price = modal.querySelector('#v1')?.innerText.trim();
    const quantity = modal.querySelector('#v2')?.innerText.trim();
    const buttons = [...modal.querySelectorAll('button[onclick="exchange(this)"]')];
    if (title !== '购买 每日福利：1000蝌蚪' || itemId !== window.__ptManagerQingwaItemId ||
        amount !== '1' || price !== '1蝌蚪' || quantity !== '1' || buttons.length !== 1)
        return { status: 'mismatch', detail: `弹窗标题 ${title}，价格 ${price}，数量 ${amount}，按钮 ${buttons.length} 个` };
    buttons[0].dataset.ptManagerQingwaConfirm = 'true';
    return { status: 'ready', detail: '确认弹窗信息已核对' };
})()"#;

const BONUS_CONFIRM_EXPRESSION: &str = r#"(() => {
    const form = document.querySelector('#exchange_form');
    const modal = form?.closest('.layui-layer');
    const button = modal?.querySelector('button[data-pt-manager-qingwa-confirm="true"]');
    if (!button || modal.querySelector('.layui-layer-title')?.innerText.replace(/\s+/g, ' ').trim() !== '购买 每日福利：1000蝌蚪' ||
        form.querySelector('input[name="id"]')?.value !== window.__ptManagerQingwaItemId ||
        form.querySelector('input[name="amount"]')?.value !== '1' ||
        modal.querySelector('#v1')?.innerText.trim() !== '1蝌蚪') return false;
    const day = new Date().toLocaleDateString('sv-SE');
    if (localStorage.getItem('pt-manager-qingwa-daily-bonus') === day) return false;
    // 提交前记录尝试；即使响应丢失，下次保活也不会再次扣蝌蚪。
    localStorage.setItem('pt-manager-qingwa-daily-bonus', day);
    sessionStorage.removeItem('pt-manager-qingwa-daily-bonus-result');
    const originalFetch = window.fetch;
    window.fetch = function (...args) {
        const response = originalFetch.apply(this, args);
        const url = typeof args[0] === 'string' ? args[0] : args[0]?.url;
        if (url?.includes('/api/bonus-shop/exchange')) {
            response.then(res => res.clone().json()).then(data => {
                sessionStorage.setItem('pt-manager-qingwa-daily-bonus-result', JSON.stringify({
                    success: Boolean(data.success), message: String(data.msg || '').slice(0, 160)
                }));
            }).catch(() => {});
        }
        return response;
    };
    try { button.click(); } finally { window.fetch = originalFetch; }
    return true;
})()"#;

const BONUS_RESULT_EXPRESSION: &str = r#"(() => {
    const raw = sessionStorage.getItem('pt-manager-qingwa-daily-bonus-result');
    if (!raw) return { status: 'waiting', detail: '等待站点购买结果' };
    try {
        const result = JSON.parse(raw);
        const status = result.success ? 'success' :
            result.message.includes('超过限购数量') ? 'already' : 'failed';
        return { status, detail: result.message || '站点未给出详情' };
    } catch {
        return { status: 'failed', detail: '站点购买结果格式错误' };
    }
})()"#;
