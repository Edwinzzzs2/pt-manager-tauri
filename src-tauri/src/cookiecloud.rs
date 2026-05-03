use crate::cdp::CdpCookieParam;
use crate::store::{CookieCloudConfig, Site};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_COOKIECLOUD_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct CookieCloudCookie {
    name: String,
    value: String,
    domain: Option<String>,
    #[serde(rename = "hostOnly", alias = "host_only")]
    host_only: Option<bool>,
    path: Option<String>,
    secure: Option<bool>,
    #[serde(rename = "httpOnly", alias = "http_only")]
    http_only: Option<bool>,
    #[serde(rename = "expirationDate", alias = "expiration_date")]
    expiration_date: Option<f64>,
    #[serde(rename = "sameSite", alias = "same_site")]
    same_site: Option<String>,
}

struct HttpUrl {
    host: String,
    port: u16,
    path: String,
}

pub fn fetch_cookie_data(config: &CookieCloudConfig) -> Result<Value, String> {
    let server_url = config.server_url.trim();
    let uuid = config.uuid.trim();
    let password = config.password.as_str();
    if server_url.is_empty() || uuid.is_empty() || password.is_empty() {
        return Err("请先填写 CookieCloud 地址、UUID 和密码".to_string());
    }

    let endpoints = build_endpoint_candidates(server_url, uuid);
    let mut errors = Vec::new();
    for endpoint in endpoints {
        match request_cookiecloud_payload(&endpoint, password) {
            Ok(payload) => {
                if let Some(cookie_data) = payload.get("cookie_data").cloned() {
                    return Ok(cookie_data);
                }
                if payload.get("encrypted").is_some() {
                    return Err(
                        "CookieCloud 服务端返回了密文，请确认服务端支持 password 解密接口"
                            .to_string(),
                    );
                }
                errors.push(format!("{}：返回数据缺少 cookie_data", mask_uuid(&endpoint, uuid)));
            }
            Err(err) => errors.push(format!("{}：{}", mask_uuid(&endpoint, uuid), err)),
        }
    }

    Err(format!(
        "CookieCloud 无法连接，请确认服务地址和协议是否与浏览器插件一致。最近一次错误：{}",
        errors.last().cloned().unwrap_or_else(|| "未知错误".to_string())
    ))
}

/// CookieCloud 返回的数据按域名分组；这里只挑出当前站点需要的 Cookie，再转换成 CDP 入参。
pub fn cookies_for_sites(
    cookie_data: Value,
    sites: &[Site],
) -> Result<Vec<CdpCookieParam>, String> {
    let data = serde_json::from_value::<HashMap<String, Vec<CookieCloudCookie>>>(cookie_data)
        .map_err(|err| format!("CookieCloud cookie_data 格式解析失败：{}", err))?;
    let site_hosts = sites
        .iter()
        .filter_map(|site| host_from_url(&site.url))
        .collect::<Vec<_>>();
    if site_hosts.is_empty() {
        return Err("请先配置至少一个有效站点 URL".to_string());
    }

    let mut result = Vec::new();
    for (domain_key, cookies) in data {
        for cookie in cookies {
            let domain = cookie.domain.clone().unwrap_or_else(|| domain_key.clone());
            if !site_hosts
                .iter()
                .any(|site_host| domain_matches(site_host, &domain))
            {
                continue;
            }

            let Some(url) = cookie_url(
                &domain,
                cookie.secure.unwrap_or(false),
                cookie.path.as_deref(),
            ) else {
                continue;
            };
            result.push(CdpCookieParam {
                name: cookie.name,
                value: cookie.value,
                url,
                domain: if cookie.host_only.unwrap_or(false) {
                    None
                } else {
                    Some(domain)
                },
                path: cookie.path,
                secure: cookie.secure,
                http_only: cookie.http_only,
                same_site: normalize_same_site(cookie.same_site.as_deref()),
                expires: cookie.expiration_date,
            });
        }
    }

    Ok(result)
}

fn request_cookiecloud_payload(endpoint: &str, password: &str) -> Result<Value, String> {
    let body = serde_json::json!({ "password": password }).to_string();
    let response = http_request(endpoint, "POST", Some(&body))?;
    if (200..300).contains(&response.status) {
        return parse_payload(&response.body);
    }

    let fallback = format!(
        "{}{}password={}",
        endpoint,
        if endpoint.contains('?') { "&" } else { "?" },
        percent_encode(password)
    );
    let response = http_request(&fallback, "GET", None)?;
    if (200..300).contains(&response.status) {
        return parse_payload(&response.body);
    }

    Err(format!("HTTP {}", response.status))
}

fn build_endpoint_candidates(server_url: &str, uuid: &str) -> Vec<String> {
    let clean = server_url.trim().trim_end_matches('/');
    let encoded_uuid = percent_encode(uuid);
    let mut bases = Vec::new();
    if clean.starts_with("http://") || clean.starts_with("https://") {
        bases.push(clean.to_string());
    } else {
        bases.push(format!("http://{}", clean));
    }

    bases
        .into_iter()
        .map(|base| {
            if base.to_ascii_lowercase().contains("/get/") {
                base
            } else if base.to_ascii_lowercase().ends_with("/get") {
                format!("{}/{}", base, encoded_uuid)
            } else {
                format!("{}/get/{}", base, encoded_uuid)
            }
        })
        .collect()
}

struct RawHttpResponse {
    status: u16,
    body: String,
}

fn http_request(url: &str, method: &str, body: Option<&str>) -> Result<RawHttpResponse, String> {
    let parsed = parse_http_url(url)?;
    let addr = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .map_err(|err| err.to_string())?
        .next()
        .ok_or_else(|| "无法解析 CookieCloud 服务地址".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(12))
        .map_err(|err| readable_network_error(err.to_string()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|err| err.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(12)))
        .map_err(|err| err.to_string())?;

    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}",
        method = method,
        path = parsed.path,
        host = parsed.host,
        length = body.as_bytes().len(),
        body = body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| err.to_string())?;

    parse_http_response(read_response_bytes(&mut stream)?)
}

/// 内置同步只为局域网自建服务兜底，所以先支持明文 HTTP；HTTPS 仍交给前端 fetch 兜底。
fn parse_http_url(url: &str) -> Result<HttpUrl, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "当前内置同步仅支持 http:// CookieCloud 地址".to_string())?;
    let (host_port, path) = match rest.split_once('/') {
        Some((host_port, path)) => (host_port, format!("/{}", path)),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>()
                .map_err(|_| "CookieCloud 端口格式不正确".to_string())?,
        ),
        None => (host_port.to_string(), 80),
    };
    if host.is_empty() {
        return Err("CookieCloud 服务地址缺少主机名".to_string());
    }
    Ok(HttpUrl { host, port, path })
}

fn read_response_bytes(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                raw.extend_from_slice(&buffer[..size]);
                if raw.len() > MAX_COOKIECLOUD_RESPONSE_BYTES {
                    return Err("CookieCloud 响应过大，已停止读取".to_string());
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(raw)
}

fn parse_http_response(raw: Vec<u8>) -> Result<RawHttpResponse, String> {
    let split_at = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "CookieCloud 返回了无效 HTTP 响应".to_string())?;
    let header_text = String::from_utf8_lossy(&raw[..split_at]).to_string();
    let mut lines = header_text.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "CookieCloud 返回了无效 HTTP 状态".to_string())?;
    let chunked = header_text
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding: chunked"));
    let body_bytes = &raw[split_at + 4..];
    let body = if chunked {
        decode_chunked_body(body_bytes)?
    } else {
        body_bytes.to_vec()
    };
    Ok(RawHttpResponse {
        status,
        body: String::from_utf8(body).map_err(|err| err.to_string())?,
    })
}

fn decode_chunked_body(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut cursor = 0;
    let mut decoded = Vec::new();
    loop {
        let line_end = body[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| "CookieCloud 分块响应格式不完整".to_string())?
            + cursor;
        let size_text = String::from_utf8_lossy(&body[cursor..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "CookieCloud 分块大小格式不正确".to_string())?;
        cursor = line_end + 2;
        if size == 0 {
            break;
        }
        if cursor + size > body.len() {
            return Err("CookieCloud 分块响应长度不完整".to_string());
        }
        decoded.extend_from_slice(&body[cursor..cursor + size]);
        if cursor + size + 2 > body.len() {
            return Err("CookieCloud 分块响应结尾不完整".to_string());
        }
        cursor += size + 2;
    }
    Ok(decoded)
}

fn parse_payload(text: &str) -> Result<Value, String> {
    let parsed = serde_json::from_str::<Value>(text)
        .map_err(|err| format!("CookieCloud 返回 JSON 解析失败：{}", err))?;
    if let Some(inner) = parsed.as_str() {
        serde_json::from_str::<Value>(inner)
            .map_err(|err| format!("CookieCloud 内层 JSON 解析失败：{}", err))
    } else {
        Ok(parsed)
    }
}

fn host_from_url(url: &str) -> Option<String> {
    let without_scheme = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let host = without_scheme
        .split(['/', ':', '?', '#'])
        .next()?
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

fn domain_matches(site_host: &str, cookie_domain: &str) -> bool {
    let normalized = cookie_domain
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    !normalized.is_empty()
        && (site_host == normalized || site_host.ends_with(&format!(".{}", normalized)))
}

fn cookie_url(domain: &str, secure: bool, path: Option<&str>) -> Option<String> {
    let host = domain.trim().trim_start_matches('.');
    if host.is_empty() {
        return None;
    }

    let scheme = if secure { "https" } else { "http" };
    let path = path.unwrap_or("/");
    Some(format!("{}://{}{}", scheme, host, path))
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{:02X}", byte).chars().collect::<Vec<_>>(),
        })
        .collect()
}

fn readable_network_error(message: String) -> String {
    if message.contains("10061") || message.contains("积极拒绝") {
        "目标地址没有 CookieCloud 服务在监听".to_string()
    } else {
        message
    }
}

fn mask_uuid(value: &str, uuid: &str) -> String {
    value.replace(uuid, "<uuid>")
}

fn normalize_same_site(value: Option<&str>) -> Option<String> {
    match value?.to_ascii_lowercase().as_str() {
        "lax" => Some("Lax".to_string()),
        "strict" => Some("Strict".to_string()),
        "none" | "no_restriction" => Some("None".to_string()),
        _ => None,
    }
}
