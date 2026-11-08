//! 定位 UBits 登录表单内的 Turnstile 复选框，并发送一次浏览器点击。

use crate::cdp::CdpWebSocket;
use serde_json::{json, Value};

const CHECKBOX_POINT: &str = r#"(() => {
    const box = document.querySelector('input[type="checkbox"]:enabled:not(:checked), [role="checkbox"][aria-checked="false"]:not([aria-disabled="true"])');
    if (!box) return null;
    const rect = box.getBoundingClientRect();
    if (!rect.width || !rect.height) return null;
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    const hit = document.elementFromPoint(x, y);
    if (!hit || !(box.contains(hit) || box.closest('label')?.contains(hit))) return null;
    return { x, y };
})()"#;

const PAGE_POINT: &str = r#"async function (point) {
    if (location.protocol !== 'https:' || !['ubits.club', 'www.ubits.club'].includes(location.hostname)) return null;
    const form = document.querySelector('input[name="password"]')?.form;
    const widget = this.closest('.cf-turnstile') || this.getRootNode().host?.closest('.cf-turnstile');
    const verified = () => form?.querySelector('[name="cf-turnstile-response"]')?.value.trim();
    if (!widget || !form?.contains(widget) || verified()) return null;

    const before = this.getBoundingClientRect();
    if (before.top < 0 || before.left < 0 || before.bottom > innerHeight || before.right > innerWidth) {
        this.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'instant' });
        // 跨域 iframe 的点击区域在滚动后的绘制帧更新。后台标签页可能暂停 rAF。
        await new Promise(resolve => {
            const timeout = setTimeout(resolve, 250);
            requestAnimationFrame(() => requestAnimationFrame(() => {
                clearTimeout(timeout);
                resolve();
            }));
        });
        if (!this.isConnected || verified()) return null;
    }

    const rect = this.getBoundingClientRect();
    if (point.x < 0 || point.y < 0 || point.x >= this.clientWidth || point.y >= this.clientHeight) return null;
    const x = rect.left + (this.clientLeft + point.x) * rect.width / this.offsetWidth;
    const y = rect.top + (this.clientTop + point.y) * rect.height / this.offsetHeight;
    const host = this.getRootNode().host || this;
    return document.elementFromPoint(x, y) === host ? { x, y } : null;
}"#;

pub(super) fn click_turnstile_checkbox(websocket: &mut CdpWebSocket) -> bool {
    let Ok(response) = websocket.call(
        "Target.getTargets",
        json!({ "filter": [{ "type": "iframe" }] }),
    ) else {
        return false;
    };
    let Some(targets) = response
        .pointer("/result/targetInfos")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let point = targets.iter().find_map(|target| {
        let url = reqwest::Url::parse(target.get("url")?.as_str()?).ok()?;
        if url.scheme() != "https" || url.host_str() != Some("challenges.cloudflare.com") {
            return None;
        }
        checkbox_in_page(websocket, target.get("targetId")?.as_str()?)
    });
    let Some((x, y)) = point else {
        return false;
    };

    if websocket
        .call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1,
            }),
        )
        .is_err()
    {
        return false;
    }
    let _ = websocket.call(
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1,
        }),
    );
    true
}

fn checkbox_in_page(websocket: &mut CdpWebSocket, frame_id: &str) -> Option<(f64, f64)> {
    // 页面会把 Cloudflare 的跨域 iframe 放进 closed shadow root，普通 DOM 查询看不到。
    let owner = websocket
        .call("DOM.getFrameOwner", json!({ "frameId": frame_id }))
        .ok()?;
    let backend_node_id = owner.pointer("/result/backendNodeId")?.as_u64()?;
    let attached = websocket
        .call(
            "Target.attachToTarget",
            json!({
                "targetId": frame_id, "flatten": true,
            }),
        )
        .ok()?;
    let session_id = attached.pointer("/result/sessionId")?.as_str()?;
    let checkbox = websocket.call_in_session(
        "Runtime.evaluate",
        json!({
            "expression": CHECKBOX_POINT, "returnByValue": true,
        }),
        Some(session_id),
    );
    let _ = websocket.call(
        "Target.detachFromTarget",
        json!({ "sessionId": session_id }),
    );
    let checkbox = checkbox.ok()?;
    let point = checkbox
        .pointer("/result/result/value")
        .filter(|point| point.is_object())?;

    let owner = websocket
        .call(
            "DOM.resolveNode",
            json!({ "backendNodeId": backend_node_id }),
        )
        .ok()?;
    let object_id = owner.pointer("/result/object/objectId")?.as_str()?;
    let position = websocket.call(
        "Runtime.callFunctionOn",
        json!({
            "objectId": object_id,
            "functionDeclaration": PAGE_POINT,
            "arguments": [{ "value": point }],
            "returnByValue": true,
            "awaitPromise": true,
        }),
    );
    let _ = websocket.call("Runtime.releaseObject", json!({ "objectId": object_id }));
    let position = position.ok()?;
    let point = position.pointer("/result/result/value")?;
    Some((point.get("x")?.as_f64()?, point.get("y")?.as_f64()?))
}
