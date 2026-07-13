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
    pub attempts: u8,
}

pub fn recognize(
    server_url: &str,
    image_base64: &str,
    retry_count: u8,
) -> Result<RecognitionResult, String> {
    ensure_initialized(server_url)?;
    let image_base64 = preprocess_captcha(image_base64)?;
    let attempts = retry_count.clamp(1, 5);
    let mut last_error = "OCR 未返回识别文本".to_string();
    for attempt in 0..attempts {
        // 重新初始化会重置字符范围，所以每次重试都要传递英数限制。
        let request = serde_json::json!({
            "image": &image_base64,
            "png_fix": attempt > 0,
            "probability": false,
            "charset_range": CAPTCHA_CHARSET
        });
        match request_json(
            server_url,
            "POST",
            "/ocr",
            Some(request),
            Duration::from_secs(30),
        ) {
            Ok((_, payload)) if payload.get("success").and_then(Value::as_bool) == Some(true) => {
                if let Some(text) = payload
                    .get("data")
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    // 站点验证码固定为六位，拒绝缺字结果，避免自动填写后消耗登录次数。
                    if text.len() == CAPTCHA_LENGTH
                        && text.chars().all(|char| char.is_ascii_alphanumeric())
                    {
                        return Ok(RecognitionResult {
                            text: text.to_string(),
                            attempts: attempt + 1,
                        });
                    }
                    last_error = format!(
                        "OCR 结果格式异常 (识别文本: \"{}\", 长度: {}, 要求长度: {}, 仅含英文数字: {}, 完整响应: {})",
                        text,
                        text.len(),
                        CAPTCHA_LENGTH,
                        text.chars().all(|c| c.is_ascii_alphanumeric()),
                        payload
                    );
                } else {
                    last_error = format!("OCR 未返回识别文本 (完整响应: {})", payload);
                }
            }
            Ok((_, payload)) => {
                last_error = format!(
                    "{} (完整响应: {})",
                    api_message(&payload, "OCR 识别失败"),
                    payload
                );
            }
            Err(err) => last_error = err,
        }
        if attempt == 0 && attempts > 1 {
            initialize(server_url)?;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("{}，已尝试 {} 次", last_error, attempts))
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
    let (_, status) = request_json(server_url, "GET", "/status", None, Duration::from_secs(15))?;
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
    let (_, initialized) = request_json(
        server_url,
        "POST",
        "/initialize",
        Some(serde_json::json!({ "ocr": true, "det": false, "beta": true })),
        Duration::from_secs(120),
    )?;
    if initialized.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(api_message(&initialized, "OCR 服务初始化失败"));
    }

    let (_, status) = request_json(server_url, "GET", "/status", None, Duration::from_secs(15))?;
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

fn api_message(payload: &Value, fallback: &str) -> String {
    payload
        .get("message")
        .or_else(|| payload.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn request_json(
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    let base_url = server_url.trim().trim_end_matches('/');
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err("OCR 地址必须以 http:// 或 https:// 开头".to_string());
    }
    let url = format!("{}/{}", base_url, path.trim_start_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| format!("OCR HTTP 客户端初始化失败：{}", err))?;
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
            format!("OCR 服务连接失败：{}", err)
        } else {
            format!("OCR 请求失败：{}", err)
        }
    })?;
    let status = response.status().as_u16();
    let response_text = response
        .text()
        .map_err(|err| format!("读取 OCR 响应失败：{}", err))?;
    let payload: Value = serde_json::from_str(&response_text)
        .map_err(|err| format!("OCR JSON 解析失败：{} (原始响应: {})", err, response_text))?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "OCR 服务返回 HTTP {}：{} (完整响应: {})",
            status,
            api_message(&payload, "请求失败"),
            payload
        ));
    }
    Ok((status, payload))
}
