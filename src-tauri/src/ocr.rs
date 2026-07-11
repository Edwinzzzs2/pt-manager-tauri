use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

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
    let image_base64 = normalize_image_base64(image_base64)?;
    let attempts = retry_count.clamp(1, 5);
    let mut last_error = "OCR 未返回识别文本".to_string();
    for attempt in 0..attempts {
        let request = if attempt == 0 {
            serde_json::json!({
                "image": &image_base64,
                "png_fix": false,
                "probability": false,
                "charset_range": 6
            })
        } else {
            serde_json::json!({
                "image": &image_base64,
                "png_fix": true,
                "probability": false
            })
        };
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
                    if text.len() <= 16 && text.chars().all(|char| char.is_ascii_alphanumeric()) {
                        return Ok(RecognitionResult {
                            text: text.to_string(),
                            attempts: attempt + 1,
                        });
                    }
                    last_error = "OCR 结果格式异常".to_string();
                } else {
                    last_error = "OCR 未返回识别文本".to_string();
                }
            }
            Ok((_, payload)) => last_error = api_message(&payload, "OCR 识别失败"),
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

pub fn ensure_initialized(server_url: &str) -> Result<(), String> {
    let (_, status) = request_json(server_url, "GET", "/status", None, Duration::from_secs(15))?;
    if ocr_ready(&status) {
        return Ok(());
    }

    initialize(server_url)
}

fn initialize(server_url: &str) -> Result<(), String> {
    let (_, initialized) = request_json(
        server_url,
        "POST",
        "/initialize",
        Some(serde_json::json!({ "ocr": true, "det": false })),
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
    let endpoint = parse_endpoint(server_url, path)?;
    let addr = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|err| format!("无法解析 OCR 服务地址：{}", err))?
        .next()
        .ok_or_else(|| "无法解析 OCR 服务地址".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|err| format!("OCR 服务连接失败：{}", err))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let body = body.map(|value| value.to_string()).unwrap_or_default();
    let request = format!(
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        endpoint.path,
        endpoint.host,
        endpoint.port,
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("发送 OCR 请求失败：{}", err))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|err| format!("读取 OCR 响应失败：{}", err))?;
    let response = String::from_utf8(response).map_err(|_| "OCR 响应不是 UTF-8".to_string())?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "OCR 返回了无效 HTTP 响应".to_string())?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "OCR 返回了无效 HTTP 状态".to_string())?;
    let payload: Value = serde_json::from_str(body)
        .map_err(|err| format!("OCR JSON 解析失败：{}", err))?;
    if !(200..300).contains(&status) {
        return Err(format!("OCR 服务返回 HTTP {}：{}", status, api_message(&payload, "请求失败")));
    }
    Ok((status, payload))
}

struct Endpoint {
    host: String,
    port: u16,
    path: String,
}

fn parse_endpoint(server_url: &str, route: &str) -> Result<Endpoint, String> {
    let input = server_url.trim().trim_end_matches('/');
    let rest = input
        .strip_prefix("http://")
        .ok_or_else(|| "OCR 地址目前仅支持 http:// 协议".to_string())?;
    let (authority, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>().map_err(|_| "OCR 地址端口无效".to_string())?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err("OCR 地址缺少主机名".to_string());
    }
    let prefix = prefix.trim_matches('/');
    let route = route.trim_start_matches('/');
    let path = if prefix.is_empty() {
        format!("/{}", route)
    } else {
        format!("/{}/{}", prefix, route)
    };
    Ok(Endpoint {
        host: host.to_string(),
        port,
        path,
    })
}
