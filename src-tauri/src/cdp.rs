use crate::store::{self, LogEntry};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_CDP_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize, Debug)]
pub struct CdpTab {
    pub id: String,
    pub url: Option<String>,
    #[serde(rename = "type")]
    pub tab_type: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
}

struct HttpResponse {
    status: u16,
    body: String,
}

pub struct CdpClient {
    port: u16,
}

pub struct CdpLaunchResult {
    pub port: u16,
    pub message: String,
    pub opened_initial_urls: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpCookieParam {
    pub name: String,
    pub value: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
}

pub const CDP_CANCELLED: &str = "CDP 操作已终止";

#[derive(Clone)]
pub struct CdpProgress {
    logs: Arc<Mutex<Vec<LogEntry>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl CdpProgress {
    pub fn new(logs: Arc<Mutex<Vec<LogEntry>>>, cancel_requested: Arc<AtomicBool>) -> Self {
        Self {
            logs,
            cancel_requested,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    async fn info(&self, message: impl Into<String>) {
        store::push_log(&self.logs, LogEntry::info(message)).await;
    }
}

impl CdpClient {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// 返回当前可用的 CDP 端口；自动模式会从 Chrome 写出的 DevToolsActivePort 中读取真实端口。
    pub async fn available_port(&self) -> Option<u16> {
        if self
            .is_available_with_timeout(Duration::from_millis(600))
            .await
        {
            return Some(self.port);
        }

        for profile_dir in [dedicated_profile_dir(), recovery_profile_dir()] {
            let Some(port) = read_devtools_port(&profile_dir) else {
                continue;
            };
            if CdpClient::new(port)
                .is_available_with_timeout(Duration::from_millis(600))
                .await
            {
                return Some(port);
            }
        }

        None
    }

    /// 检测 Chrome 是否以调试模式运行。
    pub async fn is_available(&self) -> bool {
        self.is_available_with_timeout(Duration::from_secs(3)).await
    }

    async fn is_available_with_timeout(&self, timeout: Duration) -> bool {
        self.request("GET", "/json/version", timeout)
            .map(|response| (200..300).contains(&response.status))
            .unwrap_or(false)
    }

    /// 确保 Chrome 已开放 CDP 端口；自动启动时优先使用配置端口，冲突时再退回随机端口。
    pub async fn ensure_available_with_progress(
        &self,
        initial_urls: &[String],
        progress: &CdpProgress,
    ) -> Result<CdpLaunchResult, String> {
        self.ensure_available_inner(initial_urls, Some(progress))
            .await
    }

    async fn ensure_available_inner(
        &self,
        initial_urls: &[String],
        progress: Option<&CdpProgress>,
    ) -> Result<CdpLaunchResult, String> {
        log_progress(
            progress,
            format!("检测配置端口 localhost:{} 是否已有 CDP 响应", self.port),
        )
        .await;
        if self.is_available().await {
            check_cancel(progress)?;
            log_progress(progress, format!("配置端口 localhost:{} 已响应", self.port)).await;
            let opened_initial_urls = self.ensure_initial_urls(initial_urls, progress).await?;
            return Ok(CdpLaunchResult {
                port: self.port,
                message: connected_message("Chrome CDP 已连接", self.port, opened_initial_urls),
                opened_initial_urls,
            });
        }
        check_cancel(progress)?;

        let profile_dir = dedicated_profile_dir();
        log_progress(progress, format!("检查专用 Chrome Profile：{}", profile_dir.display())).await;
        if profile_dir_in_use(&profile_dir) {
            log_progress(
                progress,
                "专用 Chrome Profile 正在被占用，但没有可用 CDP；跳过复用，改用备用 Profile",
            )
            .await;
        } else if self.port > 0 && port_is_free(self.port) {
            log_progress(
                progress,
                format!("配置端口 localhost:{} 可用，优先按该端口启动", self.port),
            )
            .await;
            if let Some(result) =
                launch_and_wait(&profile_dir, false, initial_urls, Some(self.port), progress)
                    .await?
            {
                return Ok(result);
            }

            check_cancel(progress)?;
            log_progress(
                progress,
                format!(
                    "配置端口 localhost:{} 启动后未响应，准备改用随机端口",
                    self.port
                ),
            )
            .await;
        } else {
            log_progress(
                progress,
                format!("配置端口 localhost:{} 已被占用，准备改用随机端口", self.port),
            )
            .await;
            if let Some(result) = launch_and_wait(&profile_dir, false, initial_urls, None, progress)
                .await?
            {
                return Ok(result);
            }

            check_cancel(progress)?;
            log_progress(progress, "随机端口启动后未响应，准备尝试备用 Profile").await;
        }

        let recovery_dir = recovery_profile_dir();
        if let Some(result) =
            launch_and_wait(&recovery_dir, true, initial_urls, None, progress).await?
        {
            return Ok(result);
        }

        Err(format!(
            "已尝试自动启动 Chrome，但 CDP 仍未连接。请关闭刚打开的专用 Chrome 后重试，或在设置里更换 CDP 端口。原端口：{}",
            self.port
        ))
    }

    /// 在 Chrome 中打开新标签页，返回 tab ID。
    pub async fn open_tab(&self, url: &str) -> Result<String, String> {
        let encoded = encode_cdp_target_url(url);
        let response = self.request(
            "PUT",
            &format!("/json/new?{}", encoded),
            Duration::from_secs(10),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }

        let tab = serde_json::from_str::<CdpTab>(&response.body).map_err(|err| err.to_string())?;
        Ok(tab.id)
    }

    /// 查找已经打开到相同站点的标签页，避免启动 Chrome 后再重复打开同一个站点。
    pub async fn find_tab_for_url(&self, url: &str) -> Option<String> {
        let response = self
            .request("GET", "/json/list", Duration::from_secs(5))
            .ok()?;
        if !(200..300).contains(&response.status) {
            return None;
        }

        let tabs = serde_json::from_str::<Vec<CdpTab>>(&response.body).ok()?;
        let expected_host = host_from_url(url)?;
        tabs.into_iter()
            .filter(|tab| tab.tab_type.as_deref() == Some("page"))
            .find(|tab| {
                tab.url
                    .as_deref()
                    .and_then(host_from_url)
                    .map(|host| host == expected_host)
                    .unwrap_or(false)
            })
            .map(|tab| tab.id)
    }

    pub async fn set_cookies(&self, cookies: &[CdpCookieParam]) -> Result<usize, String> {
        if cookies.is_empty() {
            return Ok(0);
        }

        let websocket_url = self.page_websocket_url().await?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let mut imported = 0;

        for cookie in cookies {
            let params = serde_json::to_value(cookie)
                .map_err(|err| format!("Cookie 参数序列化失败：{}", err))?;
            let response = websocket.call("Network.setCookie", params)?;
            let success = response
                .get("result")
                .and_then(|value| value.get("success"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if success {
                imported += 1;
            }
        }

        Ok(imported)
    }

    async fn page_websocket_url(&self) -> Result<String, String> {
        if let Some(url) = self.find_page_websocket_url()? {
            return Ok(url);
        }

        self.open_tab("about:blank").await?;
        self.find_page_websocket_url()?
            .ok_or_else(|| "未找到可用的 Chrome 页面调试通道".to_string())
    }

    fn find_page_websocket_url(&self) -> Result<Option<String>, String> {
        let response = self.request("GET", "/json/list", Duration::from_secs(5))?;
        if !(200..300).contains(&response.status) {
            return Err(format!("CDP 返回 HTTP {}", response.status));
        }

        let tabs =
            serde_json::from_str::<Vec<CdpTab>>(&response.body).map_err(|err| err.to_string())?;
        Ok(tabs
            .into_iter()
            .filter(|tab| tab.tab_type.as_deref() == Some("page"))
            .find_map(|tab| tab.web_socket_debugger_url))
    }

    async fn ensure_initial_urls(
        &self,
        initial_urls: &[String],
        progress: Option<&CdpProgress>,
    ) -> Result<usize, String> {
        let mut opened_count = 0;

        for url in initial_urls
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            check_cancel(progress)?;
            log_progress(progress, format!("检查站点标签页：{}", url)).await;
            if self.find_tab_for_url(url).await.is_some() {
                log_progress(progress, format!("站点已在 Chrome 中打开：{}", url)).await;
                opened_count += 1;
                continue;
            }

            log_progress(progress, format!("通过 CDP 新建标签页：{}", url)).await;
            self.open_tab(url)
                .await
                .map_err(|err| format!("CDP 已连接，但打开站点 {} 失败：{}", url, err))?;
            opened_count += 1;
        }

        Ok(opened_count)
    }

    /// 关闭指定标签页。
    pub async fn close_tab(&self, tab_id: &str) -> Result<(), String> {
        let response = self.request(
            "GET",
            &format!("/json/close/{}", tab_id),
            Duration::from_secs(10),
        )?;
        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(format!("CDP 返回 HTTP {}", response.status))
        }
    }

    fn request(&self, method: &str, path: &str, timeout: Duration) -> Result<HttpResponse, String> {
        let addr = ("127.0.0.1", self.port)
            .to_socket_addrs()
            .map_err(|err| err.to_string())?
            .next()
            .ok_or_else(|| "无法解析 localhost 地址".to_string())?;
        let mut stream =
            TcpStream::connect_timeout(&addr, timeout).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;

        let request = format!(
            "{} {} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
            method, path, self.port
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;

        let raw = read_http_response(&mut stream)?;
        parse_http_response(&raw)
    }
}

pub fn chrome_installed() -> bool {
    find_chrome_executable().is_some()
}

fn read_http_response(stream: &mut TcpStream) -> Result<String, String> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];

    // Chrome 的 CDP HTTP 端口通常会返回 Content-Length，但不保证立刻关闭连接。
    // 因此不能用 read_to_string 等 EOF，而要按响应头声明的长度读完整个 body。
    let header_end = loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|err| format!("读取 CDP HTTP 响应失败：{}", err))?;
        if read == 0 {
            break header_end_index(&raw).ok_or_else(|| "CDP HTTP 响应缺少响应头".to_string())?;
        }

        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > MAX_CDP_HTTP_RESPONSE_BYTES {
            return Err("CDP HTTP 响应过大".to_string());
        }

        if let Some(index) = header_end_index(&raw) {
            break index;
        }
    };

    let headers = std::str::from_utf8(&raw[..header_end])
        .map_err(|err| format!("CDP HTTP 响应头不是 UTF-8：{}", err))?;
    if let Some(content_length) = content_length_from_headers(headers) {
        let expected_len = header_end + content_length;
        while raw.len() < expected_len {
            let read = stream
                .read(&mut buffer)
                .map_err(|err| format!("读取 CDP HTTP 响应体失败：{}", err))?;
            if read == 0 {
                return Err("CDP HTTP 响应体提前结束".to_string());
            }

            raw.extend_from_slice(&buffer[..read]);
            if raw.len() > MAX_CDP_HTTP_RESPONSE_BYTES {
                return Err("CDP HTTP 响应过大".to_string());
            }
        }
        raw.truncate(expected_len);
    }

    String::from_utf8(raw).map_err(|err| format!("CDP HTTP 响应不是 UTF-8：{}", err))
}

fn header_end_index(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn content_length_from_headers(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn parse_http_response(raw: &str) -> Result<HttpResponse, String> {
    let mut parts = raw.splitn(2, "\r\n\r\n");
    let headers = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default().to_string();
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "无法解析 CDP HTTP 响应".to_string())?;

    Ok(HttpResponse { status, body })
}

struct CdpWebSocket {
    stream: TcpStream,
    next_id: u64,
}

impl CdpWebSocket {
    fn connect(url: &str, timeout: Duration) -> Result<Self, String> {
        let endpoint = parse_ws_url(url)?;
        let addr = (endpoint.host.as_str(), endpoint.port)
            .to_socket_addrs()
            .map_err(|err| err.to_string())?
            .next()
            .ok_or_else(|| "无法解析 CDP WebSocket 地址".to_string())?;
        let mut stream =
            TcpStream::connect_timeout(&addr, timeout).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|err| err.to_string())?;

        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: cHRtYW5hZ2VyY2RwMTIzNA==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            endpoint.path, endpoint.host, endpoint.port
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;
        let headers = read_websocket_headers(&mut stream)?;
        if !headers.starts_with("HTTP/1.1 101") {
            return Err("CDP WebSocket 握手失败".to_string());
        }

        Ok(Self { stream, next_id: 1 })
    }

    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_frame(0x1, request.to_string().as_bytes())?;

        loop {
            let message = self.read_message()?;
            let value = serde_json::from_str::<serde_json::Value>(&message)
                .map_err(|err| format!("CDP WebSocket 响应解析失败：{}", err))?;
            if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(format!("CDP 方法 {} 调用失败：{}", method, error));
                }
                return Ok(value);
            }
        }
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }

        let mask = [0x13, 0x57, 0x9b, 0xdf];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| *byte ^ mask[index % mask.len()]),
        );
        self.stream
            .write_all(&frame)
            .map_err(|err| format!("发送 CDP WebSocket 帧失败：{}", err))
    }

    fn read_message(&mut self) -> Result<String, String> {
        loop {
            let mut header = [0_u8; 2];
            self.stream
                .read_exact(&mut header)
                .map_err(|err| format!("读取 CDP WebSocket 帧失败：{}", err))?;
            let opcode = header[0] & 0x0f;
            let masked = header[1] & 0x80 != 0;
            let mut len = (header[1] & 0x7f) as u64;
            if len == 126 {
                let mut bytes = [0_u8; 2];
                self.stream
                    .read_exact(&mut bytes)
                    .map_err(|err| err.to_string())?;
                len = u16::from_be_bytes(bytes) as u64;
            } else if len == 127 {
                let mut bytes = [0_u8; 8];
                self.stream
                    .read_exact(&mut bytes)
                    .map_err(|err| err.to_string())?;
                len = u64::from_be_bytes(bytes);
            }

            let mut mask = [0_u8; 4];
            if masked {
                self.stream
                    .read_exact(&mut mask)
                    .map_err(|err| err.to_string())?;
            }

            let mut payload = vec![0_u8; len as usize];
            self.stream
                .read_exact(&mut payload)
                .map_err(|err| err.to_string())?;
            if masked {
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % mask.len()];
                }
            }

            match opcode {
                0x1 => {
                    return String::from_utf8(payload)
                        .map_err(|err| format!("CDP WebSocket 文本不是 UTF-8：{}", err));
                }
                0x8 => return Err("CDP WebSocket 已关闭".to_string()),
                0x9 => self.write_frame(0xA, &payload)?,
                _ => {}
            }
        }
    }
}

struct WsEndpoint {
    host: String,
    port: u16,
    path: String,
}

fn parse_ws_url(url: &str) -> Result<WsEndpoint, String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| "仅支持本地 ws:// CDP 地址".to_string())?;
    let (host_port, path) = rest
        .split_once('/')
        .ok_or_else(|| "CDP WebSocket 地址缺少路径".to_string())?;
    let (host, port) = parse_ws_host_port(host_port)?;
    let normalized_host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if normalized_host != "127.0.0.1"
        && normalized_host != "localhost"
        && normalized_host != "::1"
    {
        return Err("只允许连接本机 CDP WebSocket".to_string());
    }

    Ok(WsEndpoint {
        // Chrome 启动时绑定 127.0.0.1；localhost 在 Windows 上可能先解析到 ::1，导致写 Cookie 时 10061。
        host: "127.0.0.1".to_string(),
        port,
        path: format!("/{}", path),
    })
}

fn parse_ws_host_port(host_port: &str) -> Result<(String, u16), String> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| "CDP WebSocket IPv6 地址格式无效".to_string())?;
        return Ok((
            host.to_string(),
            port.parse::<u16>()
                .map_err(|err| format!("CDP WebSocket 端口无效：{}", err))?,
        ));
    }

    match host_port.rsplit_once(':') {
        Some((host, port)) => Ok((
            host.to_string(),
            port.parse::<u16>()
                .map_err(|err| format!("CDP WebSocket 端口无效：{}", err))?,
        )),
        None => Ok((host_port.to_string(), 80)),
    }
}

fn read_websocket_headers(stream: &mut TcpStream) -> Result<String, String> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|err| format!("读取 CDP WebSocket 握手响应失败：{}", err))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if header_end_index(&raw).is_some() {
            break;
        }
        if raw.len() > 64 * 1024 {
            return Err("CDP WebSocket 握手响应过大".to_string());
        }
    }

    String::from_utf8(raw).map_err(|err| format!("CDP WebSocket 握手响应不是 UTF-8：{}", err))
}

fn encode_cdp_target_url(value: &str) -> String {
    let mut encoded = String::new();

    // /json/new 把整个 query 当作目标 URL，不能把 : / ? & 这些 URL 分隔符全部转义。
    for byte in value.trim().as_bytes() {
        match byte {
            b'\t' | b'\n' | b'\r' | b' ' | b'"' | b'<' | b'>' | b'`' => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
            0x00..=0x1F | 0x7F..=0xFF => encoded.push_str(&format!("%{:02X}", byte)),
            _ => encoded.push(*byte as char),
        }
    }

    encoded
}

fn unique_urls(urls: &[String]) -> Vec<String> {
    let mut result = Vec::new();

    for url in urls
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if result.iter().any(|existing| existing == url) {
            continue;
        }
        result.push(url.to_string());
    }

    result
}

fn launch_urls(urls: &[String]) -> Vec<String> {
    let urls = unique_urls(urls);
    if urls.is_empty() {
        vec!["about:blank".to_string()]
    } else {
        urls
    }
}

async fn launch_and_wait(
    profile_dir: &Path,
    recovery: bool,
    initial_urls: &[String],
    fixed_port: Option<u16>,
    progress: Option<&CdpProgress>,
) -> Result<Option<CdpLaunchResult>, String> {
    let launch_urls = launch_urls(initial_urls);
    let mode = if recovery {
        "备用专用 Chrome"
    } else {
        "专用 Chrome"
    };
    log_progress(
        progress,
        format!(
            "准备启动{}，Profile：{}，端口：{}，初始页面 {} 个",
            mode,
            profile_dir.display(),
            fixed_port
                .map(|port| format!("localhost:{}", port))
                .unwrap_or_else(|| "随机".to_string()),
            launch_urls.len()
        ),
    )
    .await;
    let _ = fs::remove_file(devtools_port_path(profile_dir));
    launch_chrome(profile_dir, &launch_urls, fixed_port)?;
    log_progress(progress, format!("{} 进程已启动，等待 CDP 响应", mode)).await;

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut attempt = 0;
    while Instant::now() < deadline {
        attempt += 1;
        check_cancel(progress)?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let port = if let Some(port) = fixed_port {
            port
        } else {
            let Some(port) = read_devtools_port(profile_dir) else {
                if attempt % 4 == 0 {
                    log_progress(
                        progress,
                        format!("等待 Chrome 写入 CDP 端口... 已等待 {} 秒", attempt / 2),
                    )
                    .await;
                }
                continue;
            };
            port
        };

        if fixed_port.is_none() {
            log_progress(
                progress,
                format!("读取到 CDP 端口 localhost:{}，正在检测响应", port),
            )
            .await;
        }
        let cdp = CdpClient::new(port);
        if cdp
            .is_available_with_timeout(Duration::from_millis(800))
            .await
        {
            check_cancel(progress)?;
            log_progress(progress, format!("CDP localhost:{} 已响应", port)).await;
            let opened_initial_urls = unique_urls(initial_urls).len();
            if opened_initial_urls > 0 {
                log_progress(
                    progress,
                    format!("Chrome 启动参数已打开 {} 个初始站点", opened_initial_urls),
                )
                .await;
            }
            let prefix = if recovery { "已启动备用专用调试 Chrome" } else { "已启动专用调试 Chrome" };
            let message = connected_message(prefix, port, opened_initial_urls);
            return Ok(Some(CdpLaunchResult {
                port,
                message,
                opened_initial_urls,
            }));
        }

        if attempt % 4 == 0 {
            log_progress(progress, format!("端口 localhost:{} 尚未响应 CDP", port)).await;
        }
    }

    log_progress(progress, format!("{} 在 15 秒内未提供可用 CDP", mode)).await;
    Ok(None)
}

fn launch_chrome(profile_dir: &Path, urls: &[String], fixed_port: Option<u16>) -> Result<(), String> {
    let chrome_path = find_chrome_executable()
        .ok_or_else(|| "未检测到 Google Chrome。请在总览点击“安装 Chrome”，安装完成后再重试。".to_string())?;

    let mut command = Command::new(chrome_path);
    fs::create_dir_all(profile_dir)
        .map_err(|err| format!("创建 Chrome 专用 Profile 失败：{}", err))?;

    command
        .arg(format!(
            "--remote-debugging-port={}",
            fixed_port.unwrap_or(0)
        ))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--new-window");

    for url in urls {
        command.arg(url);
    }

    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("自动启动 Chrome 失败：{}", err))
}

fn dedicated_profile_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(root).join("pt-manager\\chrome-cdp-profile-auto");
        }
    }

    env::temp_dir().join("pt-manager-chrome-cdp-profile-auto")
}

fn recovery_profile_dir() -> PathBuf {
    env::temp_dir().join(format!(
        "pt-manager-chrome-cdp-recovery-{}",
        std::process::id()
    ))
}

fn devtools_port_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("DevToolsActivePort")
}

fn read_devtools_port(profile_dir: &Path) -> Option<u16> {
    let data = fs::read_to_string(devtools_port_path(profile_dir)).ok()?;
    data.lines().next()?.trim().parse::<u16>().ok()
}

fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn profile_dir_in_use(profile_dir: &Path) -> bool {
    ["SingletonLock", "SingletonCookie", "SingletonSocket"]
        .iter()
        .any(|name| profile_dir.join(name).exists())
}

fn connected_message(prefix: &str, port: u16, opened_initial_urls: usize) -> String {
    match opened_initial_urls {
        0 => format!("{}：localhost:{}", prefix, port),
        1 => format!(
            "{}：localhost:{}，已打开 1 个站点。首次使用请在该窗口登录站点。",
            prefix, port
        ),
        count => format!(
            "{}：localhost:{}，已打开 {} 个站点。首次使用请在该窗口登录站点。",
            prefix, port, count
        ),
    }
}

async fn log_progress(progress: Option<&CdpProgress>, message: impl Into<String>) {
    if let Some(progress) = progress {
        progress.info(message).await;
    }
}

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress
        .map(|progress| progress.is_cancelled())
        .unwrap_or(false)
    {
        Err(CDP_CANCELLED.to_string())
    } else {
        Ok(())
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

fn find_chrome_executable() -> Option<PathBuf> {
    chrome_candidates().into_iter().find_map(|path| {
        if path.exists() {
            return Some(path);
        }
        find_in_path(&path)
    })
}

fn chrome_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(value) = env::var("CHROME") {
        paths.push(PathBuf::from(value));
    }

    #[cfg(target_os = "windows")]
    {
        for key in ["LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)"] {
            if let Ok(root) = env::var(key) {
                paths.push(PathBuf::from(root).join("Google\\Chrome\\Application\\chrome.exe"));
            }
        }
        paths.push(PathBuf::from("chrome.exe"));
    }

    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
    }

    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from("google-chrome"));
        paths.push(PathBuf::from("google-chrome-stable"));
        paths.push(PathBuf::from("chromium"));
        paths.push(PathBuf::from("chromium-browser"));
    }

    paths
}

fn find_in_path(command: &PathBuf) -> Option<PathBuf> {
    let file_name = command.file_name()?;
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(file_name))
        .find(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp_target_url_keeps_url_separators() {
        let url = "https://example.com/a/b?x=1&next=https://pt.test/#top";

        assert_eq!(encode_cdp_target_url(url), url);
    }

    #[test]
    fn cdp_target_url_escapes_spaces_without_double_encoding_url_syntax() {
        assert_eq!(
            encode_cdp_target_url(" https://example.com/a path?q=hello world "),
            "https://example.com/a%20path?q=hello%20world"
        );
    }

    #[test]
    fn launch_urls_uses_all_unique_configured_sites() {
        let urls = vec![
            "https://one.test".to_string(),
            " https://two.test ".to_string(),
            "https://one.test".to_string(),
        ];

        assert_eq!(
            launch_urls(&urls),
            vec!["https://one.test", "https://two.test"]
        );
    }

    #[test]
    fn content_length_header_is_case_insensitive() {
        let headers = "HTTP/1.1 200 OK\r\ncontent-length: 424\r\n\r\n";

        assert_eq!(content_length_from_headers(headers), Some(424));
    }
}
