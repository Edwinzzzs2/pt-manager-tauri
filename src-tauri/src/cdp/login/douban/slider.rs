use super::check_cancel;
use crate::cdp::{CdpProgress, CdpWebSocket};
use rand::Rng;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::time::Duration;

const SLIDER: &str = include_str!("slider.js");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Geometry {
    key: String,
    width: f64,
    height: f64,
    piece_x: f64,
    piece_y: f64,
    piece_width: f64,
    piece_height: f64,
    handle_x: f64,
    handle_y: f64,
    viewport_width: f64,
    viewport_height: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Challenge {
    geometry: Geometry,
    background_image: String,
    piece_image: String,
    #[serde(skip)]
    frame_id: Option<String>,
}

pub(super) fn capture(websocket: &mut CdpWebSocket) -> Option<Challenge> {
    if let Some(challenge) = evaluate_slider(websocket, None, true) {
        return Some(challenge);
    }
    let response = websocket
        .call(
            "Target.getTargets",
            json!({ "filter": [{ "type": "iframe" }] }),
        )
        .ok()?;
    response
        .pointer("/result/targetInfos")?
        .as_array()?
        .iter()
        .find_map(|target| {
            let url = reqwest::Url::parse(target.get("url")?.as_str()?).ok()?;
            if url.scheme() != "https" || !captcha_host(url.host_str()?) {
                return None;
            }
            let frame_id = target.get("targetId")?.as_str()?;
            frame_box(websocket, frame_id)?;
            let mut challenge: Challenge = evaluate_slider(websocket, Some(frame_id), true)?;
            challenge.frame_id = Some(frame_id.to_owned());
            Some(challenge)
        })
}

pub(super) async fn solve(
    websocket: &mut CdpWebSocket,
    challenge: Challenge,
    ocr_url: &str,
    progress: Option<&CdpProgress>,
) -> Result<(), String> {
    let Challenge {
        geometry,
        background_image,
        piece_image,
        frame_id,
    } = challenge;
    let url = ocr_url.to_owned();
    let target = tauri::async_runtime::spawn_blocking(move || {
        crate::ocr::match_slider(&url, &piece_image, &background_image)
    })
    .await
    .map_err(|_| "OCR 滑块请求未能完成".to_string())??;
    check_cancel(progress)?;

    let current: Geometry = evaluate_slider(websocket, frame_id.as_deref(), false)
        .ok_or_else(|| "豆瓣滑块已关闭或更换".to_string())?;
    if current.key != geometry.key
        || current.width != geometry.width
        || current.height != geometry.height
        || (current.piece_x - geometry.piece_x).abs() > 1.0
        || (current.piece_y - geometry.piece_y).abs() > 1.0
    {
        return Err("豆瓣滑块已更换，本次坐标已失效".to_string());
    }
    let distance = drag_distance(&current, target)
        .ok_or_else(|| "OCR 返回的滑块坐标超出拼图范围".to_string())?;
    let (mut x, mut y, mut distance) = (current.handle_x, current.handle_y, distance);
    if let Some(id) = frame_id.as_deref() {
        let [left, top, width, height] =
            frame_box(websocket, id).ok_or_else(|| "无法定位豆瓣滑块窗口".to_string())?;
        let scale_x = width / current.viewport_width;
        let scale_y = height / current.viewport_height;
        x = left + x * scale_x;
        y = top + y * scale_y;
        distance *= scale_x;
    }
    drag(websocket, x, y, distance, progress).await
}

fn drag_distance(geometry: &Geometry, (x, y): (f64, f64)) -> Option<f64> {
    let distance = x - geometry.piece_x;
    (x.is_finite()
        && y.is_finite()
        && distance >= 4.0
        && x + geometry.piece_width <= geometry.width + 2.0
        && y >= 0.0
        && y + geometry.piece_height <= geometry.height + 2.0
        && (y - geometry.piece_y).abs() <= 12.0
        && geometry.handle_x + distance < geometry.viewport_width)
        .then_some(distance)
}

#[cfg(test)]
mod tests {
    use super::{captcha_host, drag_distance, Geometry};

    #[test]
    fn computes_distance_after_the_images_have_been_scaled_to_css_pixels() {
        let geometry = Geometry {
            key: String::new(),
            width: 330.0,
            height: 192.0,
            piece_x: 24.5,
            piece_y: 35.3,
            piece_width: 59.0,
            piece_height: 59.0,
            handle_x: 60.0,
            handle_y: 235.0,
            viewport_width: 800.0,
            viewport_height: 600.0,
        };
        assert_eq!(drag_distance(&geometry, (197.5, 35.5)), Some(173.0));
        assert_eq!(drag_distance(&geometry, (0.0, 35.5)), None);
        assert_eq!(drag_distance(&geometry, (197.5, 90.0)), None);
        assert_eq!(drag_distance(&geometry, (300.0, 35.5)), None);
        assert_eq!(drag_distance(&geometry, (f64::NAN, 35.5)), None);
    }

    #[test]
    fn only_accepts_the_captcha_provider_domains() {
        assert!(captcha_host("t.captcha.qq.com"));
        assert!(captcha_host("turing.captcha.qcloud.com"));
        assert!(captcha_host("global.captcha.gtimg.com"));
        assert!(!captcha_host("captcha.qq.com.evil.example"));
        assert!(!captcha_host("notcaptcha.qq.com"));
    }
}

async fn drag(
    websocket: &mut CdpWebSocket,
    start_x: f64,
    start_y: f64,
    distance: f64,
    progress: Option<&CdpProgress>,
) -> Result<(), String> {
    let mouse = |kind, x, y, pressed| {
        json!({
            "type": kind, "x": x, "y": y,
            "button": "left", "buttons": if pressed { 1 } else { 0 },
            "clickCount": 1,
        })
    };
    check_cancel(progress)?;
    websocket.call(
        "Input.dispatchMouseEvent",
        mouse("mousePressed", start_x, start_y, true),
    )?;
    let mut x = start_x;
    let mut y = start_y;
    let mut result = Ok(());
    let steps = rand::thread_rng().gen_range(28..=36);
    for step in 1..=steps {
        if let Err(error) = check_cancel(progress) {
            result = Err(error);
            break;
        }
        let t = f64::from(step) / f64::from(steps);
        let wobble = (std::f64::consts::PI * t).sin();
        let (offset_x, offset_y, pause) = {
            let mut rng = rand::thread_rng();
            (
                rng.gen_range(-0.4..0.4),
                rng.gen_range(-1.2..1.2),
                rng.gen_range(18..=34),
            )
        };
        x = start_x + distance * t * t * (3.0 - 2.0 * t) + offset_x * wobble;
        y = start_y + offset_y * wobble;
        if let Err(error) =
            websocket.call("Input.dispatchMouseEvent", mouse("mouseMoved", x, y, true))
        {
            result = Err(error);
            break;
        }
        tokio::time::sleep(Duration::from_millis(pause)).await;
    }
    let released = websocket.call(
        "Input.dispatchMouseEvent",
        mouse("mouseReleased", x, y, false),
    );
    result.and(released.map(|_| ()))
}

fn captcha_host(host: &str) -> bool {
    ["captcha.qq.com", "captcha.gtimg.com", "captcha.qcloud.com"]
        .iter()
        .any(|domain| {
            host == *domain
                || host
                    .strip_suffix(domain)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

// 在顶层会话里读取跨域 iframe 的实际内容区，把帧内坐标换算到浏览器视口。
fn frame_box(websocket: &mut CdpWebSocket, frame_id: &str) -> Option<[f64; 4]> {
    let owner = websocket
        .call("DOM.getFrameOwner", json!({ "frameId": frame_id }))
        .ok()?;
    let backend_node_id = owner.pointer("/result/backendNodeId")?.as_u64()?;
    let box_model = websocket
        .call(
            "DOM.getBoxModel",
            json!({ "backendNodeId": backend_node_id }),
        )
        .ok()?;
    let quad = box_model
        .pointer("/result/model/content")?
        .as_array()
        .filter(|quad| quad.len() == 8)?;
    let (left, top) = (quad[0].as_f64()?, quad[1].as_f64()?);
    let (width, height) = (quad[2].as_f64()? - left, quad[5].as_f64()? - top);
    (left >= 0.0 && top >= 0.0 && width > 0.0 && height > 0.0).then_some([left, top, width, height])
}

fn evaluate_slider<T: DeserializeOwned>(
    websocket: &mut CdpWebSocket,
    frame_id: Option<&str>,
    capture: bool,
) -> Option<T> {
    let session = if let Some(frame_id) = frame_id {
        let attached = websocket
            .call(
                "Target.attachToTarget",
                json!({ "targetId": frame_id, "flatten": true }),
            )
            .ok()?;
        Some(attached.pointer("/result/sessionId")?.as_str()?.to_owned())
    } else {
        None
    };
    let origin_check = if session.is_some() {
        r#"location.protocol === "https:" && /(^|\.)captcha\.(qq|gtimg|qcloud)\.com$/.test(location.hostname)"#
    } else {
        r#"location.origin === "https://accounts.douban.com""#
    };
    let response = websocket.call_in_session(
        "Runtime.evaluate",
        json!({
            "expression": format!("({origin_check}) ? ({SLIDER})({capture}) : null"),
            "awaitPromise": true,
            "returnByValue": true,
        }),
        session.as_deref(),
    );
    if let Some(session_id) = session {
        let _ = websocket.call(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        );
    }
    let value = response.ok()?.pointer("/result/result/value")?.clone();
    if value == Value::Null {
        return None;
    }
    serde_json::from_value(value).ok()
}
