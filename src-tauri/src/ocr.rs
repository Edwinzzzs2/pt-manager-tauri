use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{DynamicImage, ImageFormat};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::Cursor,
    sync::{LazyLock, Mutex},
    time::Duration,
};

const CAPTCHA_LENGTH: usize = 6;
const BINARY_THRESHOLD: u8 = 80;
const CAPTCHA_CHARSET: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

static BETA_INITIALIZED_SERVERS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub struct RecognitionResult {
    pub text: String,
}

pub struct RecognitionError {
    summary: String,
    pub candidate: Option<String>,
}

impl From<String> for RecognitionError {
    fn from(summary: String) -> Self {
        Self { summary, candidate: None }
    }
}

impl std::fmt::Display for RecognitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.summary)
    }
}

pub fn recognize(server_url: &str, image_base64: &str) -> Result<RecognitionResult, RecognitionError> {
    ensure_initialized(server_url)?;
    let image_base64 = preprocess_captcha(image_base64)?;
    // 同一张图只识别一次；位数不符或识别失败时，由调用方刷新页面换图。
    let request = serde_json::json!({
        "image": &image_base64,
        "png_fix": false,
        "probability": false,
        "charset_range": CAPTCHA_CHARSET
    });
    let payload = request_json(
        server_url,
        "POST",
        "/ocr",
        Some(request),
        Duration::from_secs(30),
    )?;
    if payload.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("OCR 服务未能识别验证码".to_string().into());
    }
    let text = payload
        .get("data")
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "OCR 未返回识别文本".to_string())?;
    // 拒绝缺字和非英文数字的结果，避免无效识别消耗登录次数。
    if text.len() != CAPTCHA_LENGTH || !text.chars().all(|char| char.is_ascii_alphanumeric()) {
        return Err(RecognitionError {
            summary: format!(
                "OCR 结果格式异常（长度 {}，要求 {} 位英文或数字）",
                text.chars().count(),
                CAPTCHA_LENGTH
            ),
            candidate: Some(text.chars().take(64).collect()),
        });
    }
    Ok(RecognitionResult {
        text: text.to_string(),
    })
}

pub fn match_slider(
    server_url: &str,
    piece_base64: &str,
    background_base64: &str,
) -> Result<(f64, f64), String> {
    let piece = normalize_image_base64(piece_base64)?;
    let bytes = STANDARD.decode(&piece).map_err(|_| "滑块图片 Base64 格式无效".to_string())?;
    let image = image::load_from_memory(&bytes).map_err(|_| "滑块图片格式无效".to_string())?;
    let payload = request_json(
        server_url,
        "POST",
        "/slide-match",
        Some(serde_json::json!({
            "target_image": piece,
            "background_image": normalize_image_base64(background_base64)?,
            "simple_target": false,
        })),
        Duration::from_secs(12),
    )?;
    slider_target(&payload, (image.width(), image.height()))
}

fn slider_target(payload: &Value, (width, height): (u32, u32)) -> Result<(f64, f64), String> {
    if payload.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("OCR 服务未能匹配滑块，请确认服务已启用滑块识别".to_string());
    }
    let target = payload.pointer("/data/target").and_then(Value::as_array)
        .filter(|coordinates| matches!(coordinates.len(), 2 | 4))
        .ok_or_else(|| "OCR 未返回有效的滑块缺口坐标".to_string())?;
    // 新版接口返回缺口中心；旧版 ddddocr 返回边框和裁去透明边缘的偏移。
    if target.len() == 2 {
        let center_x = target[0].as_f64();
        let center_y = target[1].as_f64();
        return center_x.zip(center_y)
            .map(|(x, y)| (x - f64::from(width) / 2.0, y - f64::from(height) / 2.0))
            .ok_or_else(|| "OCR 未返回有效的滑块缺口坐标".to_string());
    }
    let x = target[0].as_f64().zip(payload.pointer("/data/target_x").and_then(Value::as_f64));
    let y = target[1].as_f64().zip(payload.pointer("/data/target_y").and_then(Value::as_f64));
    let (Some((left, inset_x)), Some((top, inset_y))) = (x, y) else {
        return Err("OCR 未返回有效的滑块缺口坐标".to_string());
    };
    Ok((left - inset_x, top - inset_y))
}

#[cfg(test)]
mod slider_tests {
    use super::slider_target;
    use serde_json::json;

    #[test]
    fn converts_center_coordinates_from_the_running_ocr_service() {
        let response = json!({
            "success": true,
            "data": { "target": [227, 65], "target_x": 227, "target_y": 65 }
        });
        assert_eq!(slider_target(&response, (59, 59)).unwrap(), (197.5, 35.5));
    }

    #[test]
    fn preserves_legacy_bounding_box_offsets() {
        let response = json!({
            "success": true,
            "data": { "target": [204, 42, 251, 89], "target_x": 6, "target_y": 6 }
        });
        assert_eq!(slider_target(&response, (59, 59)).unwrap(), (198.0, 36.0));
        assert!(slider_target(&json!({"success": false}), (59, 59)).is_err());
        assert!(slider_target(&json!({"success": true, "data": {"target": [0, "bad"]}}), (59, 59)).is_err());
    }
}

fn normalize_image_base64(value: &str) -> Result<String, String> {
    let encoded = value
        .trim()
        .split_once(',')
        .filter(|(prefix, _)| prefix.to_ascii_lowercase().contains(";base64"))
        .map(|(_, data)| data)
        .unwrap_or(value);
    let mut normalized = encoded
        .chars()
        .filter(|character| !character.is_whitespace())
        .map(|character| match character {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect::<String>();
    while normalized.len() % 4 != 0 {
        normalized.push('=');
    }
    let image = STANDARD
        .decode(normalized.as_bytes())
        .map_err(|err| format!("验证码截图 Base64 格式无效：{}", err))?;
    if image.is_empty() {
        return Err("验证码截图为空".to_string());
    }
    Ok(STANDARD.encode(image))
}

fn preprocess_captcha(value: &str) -> Result<String, String> {
    let normalized = normalize_image_base64(value)?;
    let image_bytes = STANDARD
        .decode(normalized.as_bytes())
        .map_err(|err| format!("验证码图片 Base64 格式无效：{}", err))?;
    let mut grayscale = image::load_from_memory(&image_bytes)
        .map_err(|err| format!("验证码图片解码失败：{}", err))?
        .to_luma8();

    // 低阈值保留深色字符，同时清除彩色背景块和大部分噪点。
    for pixel in grayscale.pixels_mut() {
        pixel[0] = if pixel[0] <= BINARY_THRESHOLD { 0 } else { 255 };
    }

    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(grayscale)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|err| format!("验证码图片预处理失败：{}", err))?;
    Ok(STANDARD.encode(output.into_inner()))
}

pub fn ensure_initialized(server_url: &str) -> Result<(), String> {
    let status = request_json(server_url, "GET", "/status", None, Duration::from_secs(15))?;
    let normalized_url = server_url.trim().trim_end_matches('/');
    let beta_ready = BETA_INITIALIZED_SERVERS
        .lock()
        .map_err(|_| "OCR 模型初始化状态已损坏".to_string())?
        .contains(normalized_url);
    if ocr_ready(&status) && beta_ready {
        return Ok(());
    }

    initialize(server_url)?;
    BETA_INITIALIZED_SERVERS
        .lock()
        .map_err(|_| "OCR 模型初始化状态已损坏".to_string())?
        .insert(normalized_url.to_string());
    Ok(())
}

fn initialize(server_url: &str) -> Result<(), String> {
    let initialized = request_json(
        server_url,
        "POST",
        "/initialize",
        Some(serde_json::json!({ "ocr": true, "det": false, "beta": true })),
        Duration::from_secs(120),
    )?;
    if initialized.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("OCR 服务初始化失败".to_string());
    }

    let status = request_json(server_url, "GET", "/status", None, Duration::from_secs(15))?;
    if ocr_ready(&status) {
        Ok(())
    } else {
        Err("OCR 初始化完成，但状态中未发现可用的 OCR 模型".to_string())
    }
}

fn ocr_ready(status: &Value) -> bool {
    status.get("service_status").and_then(Value::as_str) == Some("running")
        && string_array_contains(status.get("loaded_models"), "ocr")
        && string_array_contains(status.get("enabled_features"), "ocr")
}

fn string_array_contains(value: Option<&Value>, expected: &str) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(expected)))
}

fn request_json(
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    timeout: Duration,
) -> Result<Value, String> {
    let base_url = server_url.trim().trim_end_matches('/');
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err("OCR 地址必须以 http:// 或 https:// 开头".to_string());
    }
    let url = format!("{}/{}", base_url, path.trim_start_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| format!("OCR HTTP 客户端初始化失败：{}", err.without_url()))?;
    let mut request = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        _ => return Err(format!("OCR 不支持的请求方法：{}", method)),
    };
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().map_err(|err| {
        if err.is_timeout() {
            "OCR 服务请求超时".to_string()
        } else if err.is_connect() {
            format!("OCR 服务连接失败：{}", err.without_url())
        } else {
            format!("OCR 请求失败：{}", err.without_url())
        }
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("OCR 服务返回 HTTP {status}"));
    }
    let response_text = response
        .text()
        .map_err(|err| format!("读取 OCR 响应失败：{}", err.without_url()))?;
    serde_json::from_str(&response_text).map_err(|_| "OCR 服务返回了无效的 JSON".to_string())
}
