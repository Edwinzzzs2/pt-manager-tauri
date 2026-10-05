use crate::store::{self, BrowserProxyConfig, LogEntry};
use base64::Engine;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod https;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 256;
const IO_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(45);
static RELAY: OnceLock<Mutex<Option<Relay>>> = OnceLock::new();
static LOGS: OnceLock<Arc<tokio::sync::Mutex<Vec<LogEntry>>>> = OnceLock::new();
static LAST_ERROR_AT: AtomicU64 = AtomicU64::new(0);

struct Relay {
    config: BrowserProxyConfig,
    address: String,
    stopped: Arc<AtomicBool>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

pub fn set_logs(logs: Arc<tokio::sync::Mutex<Vec<LogEntry>>>) {
    let _ = LOGS.set(logs);
}

pub fn normalize_config(config: &mut BrowserProxyConfig) -> Result<(), String> {
    config.server_url = config.server_url.trim().trim_end_matches('/').to_string();
    config.username = config.username.trim().to_string();
    if !config.enabled {
        return Ok(());
    }
    let url = reqwest::Url::parse(&config.server_url)
        .map_err(|_| "浏览器代理地址格式错误，请填写 http:// 或 https://地址:端口".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.port_or_known_default().is_none()
    {
        return Err(
            "浏览器代理支持 HTTP 和 HTTPS，请填写 http:// 或 https://地址:端口".to_string(),
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("请将代理用户名和密码填写到独立的认证输入框".to_string());
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err("浏览器代理地址不能包含路径、查询参数或片段".to_string());
    }
    if config.username.contains(':') || (config.username.is_empty() && !config.password.is_empty())
    {
        return Err("代理用户名不能含冒号；填写代理密码时也必须填写用户名".to_string());
    }
    config.server_url = url.as_str().trim_end_matches('/').to_string();
    Ok(())
}

/// Chromium 启动参数不能直接提供代理密码。只在回环地址转发，并为上游请求补认证。
pub fn ensure_address(config: &BrowserProxyConfig) -> Result<Option<String>, String> {
    if !config.enabled {
        return Ok(None);
    }
    let mut config = config.clone();
    normalize_config(&mut config)?;
    let mut current = RELAY
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "浏览器代理状态不可用".to_string())?;
    if let Some(relay) = current
        .as_ref()
        .filter(|relay| relay.config == config && !relay.stopped.load(Ordering::Relaxed))
    {
        return Ok(Some(relay.address.clone()));
    }
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|err| format!("启动浏览器本地代理失败：{err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let address = format!(
        "http://{}",
        listener.local_addr().map_err(|err| err.to_string())?
    );
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::clone(&stopped);
    let upstream_config = config.clone();
    thread::Builder::new()
        .name("browser-proxy".to_string())
        .spawn(move || accept_connections(listener, upstream_config, stop_signal))
        .map_err(|err| format!("启动浏览器代理线程失败：{err}"))?;
    *current = Some(Relay {
        config,
        address: address.clone(),
        stopped,
    });
    Ok(Some(address))
}

/// 总览状态轮询只读当前转发地址，不能重建监听器或切换其他流程正在使用的代理。
pub fn active_address(config: &BrowserProxyConfig) -> Option<String> {
    let mut normalized = config.clone();
    normalize_config(&mut normalized).ok()?;
    let current = RELAY.get()?.lock().ok()?;
    current
        .as_ref()
        .filter(|relay| relay.config == normalized && !relay.stopped.load(Ordering::Relaxed))
        .map(|relay| relay.address.clone())
}

/// 用独立的回环监听器走实际浏览器转发路径，测试草稿配置时不替换正在使用的代理。
pub fn test_connection(mut config: BrowserProxyConfig) -> Result<String, String> {
    config.enabled = true;
    normalize_config(&mut config)?;
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|err| format!("创建代理测试通道失败：{err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("设置代理测试通道失败：{err}"))?;
    let address = format!(
        "http://{}",
        listener.local_addr().map_err(|err| err.to_string())?
    );
    let proxy = reqwest::Proxy::all(&address).map_err(|err| err.to_string())?;
    let client = reqwest::blocking::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("创建代理测试请求失败：{err}"))?;
    let (result_sender, result_receiver) = mpsc::channel();
    let (accepted_sender, accepted_receiver) = mpsc::channel();
    thread::Builder::new()
        .name("proxy-test".to_string())
        .spawn(move || {
            // 测试请求可能在连接前失败；限时等待可避免后台线程永久卡在 accept。
            let deadline = Instant::now() + Duration::from_secs(22);
            let result = loop {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        let _ = accepted_sender.send(());
                        let result = forward_request(&mut connection, &config);
                        if result.is_err() {
                            send_error(&mut connection);
                        }
                        break result;
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(err) => break Err(format!("接收代理测试请求失败：{err}")),
                }
            };
            let _ = result_sender.send(result);
        })
        .map_err(|err| format!("启动代理测试失败：{err}"))?;

    let response = client
        .get("https://ifconfig.me/ip")
        .send()
        .map_err(|err| test_request_error(err, &result_receiver))?;
    // 显式确认请求进入回环转发器，避免系统直连产生看似成功的结果。
    if accepted_receiver.try_recv().is_err() {
        return Err("测试请求没有经过程序的浏览器代理".to_string());
    }
    if !response.status().is_success() {
        if let Ok(Err(message)) = result_receiver.recv_timeout(Duration::from_millis(200)) {
            return Err(message);
        }
        return Err(format!("代理出口检测失败：HTTP {}", response.status()));
    }
    let ip = response
        .text()
        .map_err(|err| test_request_error(err, &result_receiver))?
        .trim()
        .parse::<IpAddr>()
        .map_err(|_| "代理出口检测服务未返回有效 IP".to_string())?;
    Ok(format!("代理连接成功，出口 IP：{ip}"))
}

fn test_request_error(
    error: reqwest::Error,
    result_receiver: &mpsc::Receiver<Result<(), String>>,
) -> String {
    if let Ok(Err(message)) = result_receiver.recv_timeout(Duration::from_millis(200)) {
        return message;
    }
    format!("代理出口检测失败：{error}")
}

fn accept_connections(listener: TcpListener, config: BrowserProxyConfig, stopped: Arc<AtomicBool>) {
    let active = Arc::new(AtomicUsize::new(0));
    while !stopped.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut client, _)) => {
                if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    send_error(&mut client);
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let count = Arc::clone(&active);
                let config = config.clone();
                let result = thread::Builder::new()
                    .name("proxy-connection".to_string())
                    .spawn(move || {
                        if let Err(message) = forward_request(&mut client, &config) {
                            log_error(message);
                            send_error(&mut client);
                        }
                        count.fetch_sub(1, Ordering::Relaxed);
                    });
                if result.is_err() {
                    active.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                log_error(format!("浏览器本地代理监听失败：{err}"));
                stopped.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
}

fn connect_upstream(config: &BrowserProxyConfig) -> Result<TcpStream, String> {
    let url = reqwest::Url::parse(&config.server_url).map_err(|_| "代理地址无效".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "代理地址缺少主机".to_string())?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let addresses = (host, url.port_or_known_default().unwrap_or(80))
        .to_socket_addrs()
        .map_err(|_| "无法解析浏览器代理服务器地址".to_string())?;
    for address in addresses {
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(8)) {
            stream
                .set_read_timeout(Some(IO_TIMEOUT))
                .map_err(|err| err.to_string())?;
            stream
                .set_write_timeout(Some(IO_TIMEOUT))
                .map_err(|err| err.to_string())?;
            return if url.scheme() == "https" {
                https::connect(stream, host)
            } else {
                Ok(stream)
            };
        }
    }
    Err("无法连接浏览器代理服务器，请检查代理地址、端口和服务器状态".to_string())
}

/// 一次读取可能同时包含响应头和隧道数据，必须保留多读的字节，避免 TLS 握手损坏。
fn read_headers(stream: &mut TcpStream) -> Result<(String, Vec<u8>), String> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|err| format!("读取浏览器代理请求或响应失败：{err}（{:?}）", err.kind()))?;
        if read == 0 {
            return Err("浏览器代理连接提前关闭".to_string());
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(index) = data.windows(4).position(|part| part == b"\r\n\r\n") {
            if index + 4 > MAX_HEADER_BYTES {
                return Err("浏览器代理请求头过大".to_string());
            }
            let tail = data.split_off(index + 4);
            // HTTP 头允许非 UTF-8 字节；这里只支持浏览器常见的 ASCII/UTF-8 头。
            let headers =
                String::from_utf8(data).map_err(|_| "浏览器代理请求头编码无效".to_string())?;
            return Ok((headers, tail));
        }
        if data.len() > MAX_HEADER_BYTES {
            return Err("浏览器代理请求头过大".to_string());
        }
    }
}

fn browser_closed(client: &mut TcpStream) -> bool {
    // 只窥视连接是否已关闭，不消费浏览器可能紧接着发送的 TLS 数据。
    if client.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0_u8; 1];
    let result = client.peek(&mut byte);
    let restored = client.set_nonblocking(false).is_ok();
    restored
        && match result {
            Ok(0) => true,
            Err(err) => is_browser_disconnect(err.kind()),
            _ => false,
        }
}

fn is_browser_disconnect(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
    )
}

fn write_browser_response(
    client: &mut TcpStream,
    data: &[u8],
    stage: &str,
) -> Result<bool, String> {
    match client.write_all(data) {
        Ok(()) => Ok(true),
        Err(err) if is_browser_disconnect(err.kind()) => Ok(false),
        Err(err) => Err(format!("{stage}：{err}（{:?}）", err.kind())),
    }
}

fn upstream_headers(headers: &str, config: &BrowserProxyConfig, tunnel: bool) -> String {
    let mut lines = headers.split("\r\n");
    let mut result = format!("{}\r\n", lines.next().unwrap_or_default());
    for line in lines.filter(|line| !line.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().trim();
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
            || (!tunnel && name.eq_ignore_ascii_case("connection"))
        {
            continue;
        }
        result.push_str(line);
        result.push_str("\r\n");
    }
    if !config.username.is_empty() {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", config.username, config.password));
        result.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    // HTTP 每个请求都要补认证，要求上游关闭连接，避免下一次请求沿用未经处理的头。
    if !tunnel {
        result.push_str("Connection: close\r\nProxy-Connection: close\r\n");
    }
    result.push_str("\r\n");
    result
}

fn forward_request(client: &mut TcpStream, config: &BrowserProxyConfig) -> Result<(), String> {
    // Windows 接收的连接会继承监听器的非阻塞模式；转发使用阻塞读写，必须显式恢复。
    // 否则请求尚未到达或 TLS 数据分批到达时，会立即报 WouldBlock 并断开连接。
    client
        .set_nonblocking(false)
        .map_err(|err| format!("设置浏览器代理连接模式失败：{err}"))?;
    client
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|err| err.to_string())?;
    client
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|err| err.to_string())?;
    let (headers, tail) =
        read_headers(client).map_err(|err| format!("接收浏览器代理请求失败：{err}"))?;
    let first_line = headers.lines().next().unwrap_or_default();
    let tunnel = first_line.starts_with("CONNECT ");
    let target = first_line.split_whitespace().nth(1).unwrap_or("未知目标");
    let mut upstream = connect_upstream(config)?;
    upstream
        .write_all(upstream_headers(&headers, config, tunnel).as_bytes())
        .map_err(|_| "发送浏览器代理请求失败".to_string())?;
    upstream
        .write_all(&tail)
        .map_err(|_| "发送浏览器代理数据失败".to_string())?;
    if !tunnel {
        return forward_http_response(client, upstream);
    }
    // CONNECT 只需等待代理建立目标连接；后续隧道读写仍沿用较长的普通超时。
    upstream
        .set_read_timeout(Some(CONNECT_RESPONSE_TIMEOUT))
        .map_err(|err| err.to_string())?;
    let (response_headers, response_tail) = match read_headers(&mut upstream) {
        Ok(response) => response,
        Err(_) if browser_closed(client) => return Ok(()),
        Err(err) => return Err(format!("代理隧道 {target} 等待上游响应失败：{err}")),
    };
    upstream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|err| err.to_string())?;
    let status = response_headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1));
    if status == Some("407") {
        return Err("浏览器代理认证失败，请检查代理用户名和密码".to_string());
    }
    if !write_browser_response(
        client,
        response_headers.as_bytes(),
        "返回浏览器代理响应失败",
    )? || !write_browser_response(client, &response_tail, "返回浏览器代理数据失败")?
    {
        return Ok(());
    }
    if tunnel && status != Some("200") {
        log_error(format!(
            "浏览器代理隧道 {target} 建立失败，上游返回 HTTP {}",
            status.unwrap_or("未知状态")
        ));
        let _ = std::io::copy(&mut upstream, client);
        return Ok(());
    }
    relay_streams(client, upstream);
    Ok(())
}

fn forward_http_response(client: &mut TcpStream, mut upstream: TcpStream) -> Result<(), String> {
    let mut request_client = client.try_clone().map_err(|err| err.to_string())?;
    let mut request_upstream = upstream.try_clone().map_err(|err| err.to_string())?;
    // POST 请求体可能大于首个缓冲区；读取响应时必须同时发送剩余请求体，避免互相等待。
    thread::scope(|scope| {
        scope.spawn(move || {
            let _ = std::io::copy(&mut request_client, &mut request_upstream);
            let _ = request_upstream.shutdown(Shutdown::Write);
        });
        let result = (|| {
            let (headers, tail) = read_headers(&mut upstream)
                .map_err(|err| format!("读取上游 HTTP 代理响应失败：{err}"))?;
            let status = headers
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1));
            if status == Some("407") {
                return Err("浏览器代理认证失败，请检查代理用户名和密码".to_string());
            }
            if !write_browser_response(client, headers.as_bytes(), "返回浏览器代理响应失败")?
                || !write_browser_response(client, &tail, "返回浏览器代理数据失败")?
            {
                return Ok(());
            }
            let _ = std::io::copy(&mut upstream, client);
            Ok(())
        })();
        let _ = client.shutdown(Shutdown::Read);
        result
    })
}

fn relay_streams(client: &mut TcpStream, mut upstream: TcpStream) {
    let (Ok(mut outgoing_client), Ok(mut outgoing_upstream)) =
        (client.try_clone(), upstream.try_clone())
    else {
        return;
    };
    // 两个方向独立传输，并在 EOF 时半关闭写端，让对方能继续发送最后的响应数据。
    thread::scope(|scope| {
        scope.spawn(move || {
            let _ = std::io::copy(&mut outgoing_client, &mut outgoing_upstream);
            let _ = outgoing_upstream.shutdown(Shutdown::Write);
        });
        let _ = std::io::copy(&mut upstream, client);
        let _ = client.shutdown(Shutdown::Write);
        let _ = client.shutdown(Shutdown::Read);
    });
}

fn send_error(client: &mut TcpStream) {
    let _ = client.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = client
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
}

fn log_error(message: String) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let previous = LAST_ERROR_AT.load(Ordering::Relaxed);
    // 页面会同时请求多个资源，同一轮网络故障只提示一次，避免日志被刷屏。
    if now.saturating_sub(previous) < 10
        || LAST_ERROR_AT
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    if let Some(logs) = LOGS.get() {
        let logs = Arc::clone(logs);
        tauri::async_runtime::spawn(async move {
            store::push_log(&logs, LogEntry::error(message)).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(port: u16) -> BrowserProxyConfig {
        BrowserProxyConfig {
            enabled: true,
            server_url: format!("http://127.0.0.1:{port}"),
            username: "unit-user".to_string(),
            password: "unit-password".to_string(),
        }
    }

    #[test]
    fn validates_proxy_and_defaults_old_configs_to_disabled() {
        let legacy: BrowserProxyConfig = serde_json::from_str("{}").unwrap();
        assert!(!legacy.enabled);
        let mut config = test_config(3128);
        normalize_config(&mut config).unwrap();
        for invalid in [
            "socks5://proxy.example:3128",
            "http://user:secret@proxy.example:3128",
            "http://proxy.example:3128/path",
        ] {
            config.server_url = invalid.to_string();
            assert!(normalize_config(&mut config).is_err());
        }
        config.server_url = "https://proxy.example:443/".to_string();
        normalize_config(&mut config).unwrap();
        assert_eq!(config.server_url, "https://proxy.example");
    }

    #[test]
    fn http_post_sends_body_after_headers_and_injects_proxy_auth() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = test_config(upstream.local_addr().unwrap().port());
        let body = vec![b'x'; 128 * 1024];
        let expected = body.clone();
        let upstream_worker = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (headers, mut received) = read_headers(&mut stream).unwrap();
            let token = base64::engine::general_purpose::STANDARD.encode("unit-user:unit-password");
            assert!(headers.contains(&format!("Proxy-Authorization: Basic {token}\r\n")));
            assert!(!headers.contains("obsolete-token"));
            assert!(headers.contains("Connection: close\r\n"));
            let tail_start = received.len();
            received.resize(expected.len(), 0);
            stream.read_exact(&mut received[tail_start..]).unwrap();
            assert_eq!(received, expected);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        });
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let local_address = local.local_addr().unwrap();
        let proxy_worker = thread::spawn(move || {
            let (mut stream, _) = local.accept().unwrap();
            forward_request(&mut stream, &config).unwrap();
        });
        let mut browser = TcpStream::connect(local_address).unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        browser.write_all(format!("POST http://example.test/login HTTP/1.1\r\nHost: example.test\r\nContent-Length: {}\r\nProxy-Authorization: obsolete-token\r\n\r\n", body.len()).as_bytes()).unwrap();
        // 分开发送头和大请求体，覆盖先等待响应导致登录 POST 卡住的情况。
        thread::sleep(Duration::from_millis(50));
        browser.write_all(&body).unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).unwrap();
        assert!(response.ends_with("\r\n\r\nOK"));
        upstream_worker.join().unwrap();
        proxy_worker.join().unwrap();
    }

    #[test]
    fn connect_preserves_prefetched_tunnel_bytes_and_both_directions() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = test_config(upstream.local_addr().unwrap().port());
        let upstream_worker = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (headers, tail) = read_headers(&mut stream).unwrap();
            assert!(headers.starts_with("CONNECT example.test:443 HTTP/1.1\r\n"));
            assert!(headers.contains("Proxy-Authorization: Basic "));
            assert!(tail.is_empty());
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nserver-prefix")
                .unwrap();
            let mut payload = [0_u8; 4];
            stream.read_exact(&mut payload).unwrap();
            assert_eq!(&payload, b"ping");
            stream.write_all(b"pong").unwrap();
        });
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        // 与正式监听器保持一致，覆盖 Windows 继承非阻塞模式后误断开请求的情况。
        local.set_nonblocking(true).unwrap();
        let local_address = local.local_addr().unwrap();
        let mut browser = TcpStream::connect(local_address).unwrap();
        let (accepted, ready) = std::sync::mpsc::sync_channel(1);
        let proxy_worker = thread::spawn(move || {
            let (mut stream, _) = local.accept().unwrap();
            accepted.send(()).unwrap();
            forward_request(&mut stream, &config).unwrap();
        });
        browser
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // 确保先接收连接，再发送请求，避免数据提前到达掩盖 WouldBlock 问题。
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(50));
        browser
            .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
            .unwrap();
        let (headers, mut payload) = read_headers(&mut browser).unwrap();
        assert!(headers.starts_with("HTTP/1.1 200"));
        browser.write_all(b"ping").unwrap();
        browser.read_to_end(&mut payload).unwrap();
        assert_eq!(&payload, b"server-prefixpong");
        upstream_worker.join().unwrap();
        proxy_worker.join().unwrap();
    }
}
