use super::{
    check_cancel, host_from_url, log_progress, CdpProgress, CdpTab, CdpWebSocket,
    CDP_NAVIGATION_TIMEOUT,
};
use crate::store::BrowserEnvironment;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::{Shutdown, TcpStream};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct BrowserSession {
    endpoint: String,
    settings: BrowserEnvironment,
    socket: TcpStream,
    worker: Option<JoinHandle<()>>,
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

static BROWSERS: OnceLock<Mutex<HashMap<u16, BrowserSession>>> = OnceLock::new();

pub(super) fn ensure(
    port: u16,
    endpoint: &str,
    settings: &BrowserEnvironment,
) -> Result<(), String> {
    let mut browsers = BROWSERS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "浏览器环境会话不可用".to_string())?;
    if browsers.get(&port).is_some_and(|session| {
        session.endpoint == endpoint
            && session.settings == *settings
            && session
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }) {
        return Ok(());
    }
    browsers.remove(&port);
    if settings.is_default() {
        return Ok(());
    }

    let mut websocket = CdpWebSocket::connect(endpoint, Duration::from_secs(10))?;
    websocket.attachment_events = Some(VecDeque::new());
    let socket = websocket
        .stream
        .try_clone()
        .map_err(|err| err.to_string())?;
    let environment = settings.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name(format!("browser-environment-{port}"))
        .spawn(move || {
            let result = (|| {
                websocket.call("Target.setAutoAttach", auto_attach_params())?;
                // setAutoAttach 的应答之前会收到已有页面的附加事件，先全部处理再报告就绪。
                while let Some(event) = next_pending_attachment(&mut websocket) {
                    configure_target(&mut websocket, &event, &environment)?;
                }
                Ok::<(), String>(())
            })();
            let initialized = result.is_ok();
            if ready_tx.send(result).is_err() || !initialized {
                return;
            }
            // 空闲时保持连接。关闭浏览器或应用释放连接时，read_message 会退出。
            loop {
                let event = if let Some(event) = next_pending_attachment(&mut websocket) {
                    event
                } else {
                    if websocket.stream.set_read_timeout(None).is_err() {
                        break;
                    }
                    let Ok(message) = websocket.read_message() else {
                        break;
                    };
                    let Ok(event) = serde_json::from_str::<Value>(&message) else {
                        break;
                    };
                    event
                };
                if event.get("method").and_then(Value::as_str) != Some("Target.attachedToTarget") {
                    continue;
                }
                if websocket
                    .stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .is_err()
                    || configure_target(&mut websocket, &event, &environment).is_err()
                {
                    break;
                }
            }
            // 最后一个浏览器会话关闭后，Chrome 会释放暂停的页面及附加关系。
            let _ = websocket.stream.shutdown(Shutdown::Both);
        })
        .map_err(|err| format!("启动浏览器环境会话失败：{err}"))?;
    let session = BrowserSession {
        endpoint: endpoint.to_string(),
        settings: settings.clone(),
        socket,
        worker: Some(worker),
    };
    ready_rx
        .recv_timeout(Duration::from_secs(15))
        .map_err(|_| "应用浏览器环境超时，请关闭该组浏览器后重试".to_string())??;
    browsers.insert(port, session);
    Ok(())
}

fn next_pending_attachment(websocket: &mut CdpWebSocket) -> Option<Value> {
    websocket.attachment_events.as_mut()?.pop_front()
}

pub(super) async fn navigate(
    tab: &CdpTab,
    settings: &BrowserEnvironment,
    url: &str,
    progress: Option<&CdpProgress>,
) -> Result<(), String> {
    let websocket = prepare_navigation(tab, settings)?;
    navigate_with_retry(
        websocket,
        url,
        progress,
        CDP_NAVIGATION_TIMEOUT,
        Duration::from_secs(2),
    )
    .await
}

async fn navigate_with_retry(
    mut websocket: CdpWebSocket,
    url: &str,
    progress: Option<&CdpProgress>,
    timeout: Duration,
    retry_delay: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let site = host_from_url(url).unwrap_or_else(|| "站点".to_string());
    let mut attempts = 0_u32;
    let mut last_error = "等待导航响应".to_string();
    loop {
        check_cancel(progress)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "打开站点失败：{last_error}（等待 {} 秒，尝试 {attempts} 次）",
                timeout.as_secs()
            ));
        }
        attempts += 1;
        let socket = websocket
            .stream
            .try_clone()
            .map_err(|err| err.to_string())?;
        let params = json!({"url": url.trim()});
        // 网络等待不能占用异步任务线程，否则取消按钮及其他站点也会被阻塞。
        let mut worker = tauri::async_runtime::spawn_blocking(move || {
            let response = websocket.call_with_timeout("Page.navigate", params, remaining);
            (websocket, response)
        });
        let (returned_socket, response) = loop {
            if let Err(error) = check_cancel(progress) {
                let _ = socket.shutdown(Shutdown::Both);
                let _ = worker.await;
                return Err(error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = socket.shutdown(Shutdown::Both);
                let _ = worker.await;
                return Err(format!(
                    "打开站点失败：{last_error}（等待 {} 秒，尝试 {attempts} 次）",
                    timeout.as_secs()
                ));
            }
            // 超时只结束本次轮询，后台导航仍继续等待同一个响应。
            if let Ok(result) =
                tokio::time::timeout(remaining.min(Duration::from_millis(100)), &mut worker).await
            {
                break result.map_err(|err| format!("页面导航任务失败：{err}"))?;
            }
        };
        websocket = returned_socket;
        let response = response?;
        let error = response
            .pointer("/result/errorText")
            .and_then(Value::as_str);
        let Some(error) = error.filter(|error| !error.is_empty()) else {
            if attempts > 1 {
                log_progress(
                    progress,
                    format!("{site} 页面连接已恢复，第 {attempts} 次打开成功"),
                )
                .await;
            }
            return Ok(());
        };
        if !retryable_navigation_error(error) {
            return Err(format!("打开站点失败：{error}"));
        }
        last_error = error.to_string();
        let delay = (retry_delay * (1_u32 << (attempts - 1).min(3)))
            .min(Duration::from_secs(10))
            .min(deadline.saturating_duration_since(Instant::now()));
        log_progress(
            progress,
            format!(
                "{site} 页面连接暂时失败：{error}，{} 秒后重试（第 {} 次，总等待上限 {} 秒）",
                delay.as_secs(),
                attempts + 1,
                timeout.as_secs()
            ),
        )
        .await;
        sleep_with_cancellation(delay, progress).await?;
    }
}

fn retryable_navigation_error(error: &str) -> bool {
    matches!(
        error,
        "net::ERR_CONNECTION_CLOSED"
            | "net::ERR_CONNECTION_RESET"
            | "net::ERR_CONNECTION_ABORTED"
            | "net::ERR_CONNECTION_TIMED_OUT"
            | "net::ERR_TIMED_OUT"
            | "net::ERR_EMPTY_RESPONSE"
            | "net::ERR_NETWORK_CHANGED"
    )
}

async fn sleep_with_cancellation(
    delay: Duration,
    progress: Option<&CdpProgress>,
) -> Result<(), String> {
    let deadline = Instant::now() + delay;
    loop {
        check_cancel(progress)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
    }
}

fn prepare_navigation(tab: &CdpTab, settings: &BrowserEnvironment) -> Result<CdpWebSocket, String> {
    let endpoint = tab
        .web_socket_debugger_url
        .as_deref()
        .ok_or_else(|| "浏览器未提供标签页调试连接".to_string())?;
    let websocket = CdpWebSocket::connect(endpoint, Duration::from_secs(5))?;
    if settings.is_default() {
        return Ok(websocket);
    }
    wait_for_environment(websocket, settings)
}

fn wait_for_environment(
    mut websocket: CdpWebSocket,
    settings: &BrowserEnvironment,
) -> Result<CdpWebSocket, String> {
    let expression = format!(
        "({timezone} === '' || Intl.DateTimeFormat().resolvedOptions().timeZone === {timezone}) && ({language} === '' || navigator.language === {language})",
        timezone = json!(settings.timezone),
        language = json!(settings.language),
    );
    // 程序打开的站点须等环境会话处理完空白页后再导航。
    for _ in 0..40 {
        let response = websocket.call(
            "Runtime.evaluate",
            json!({
                "expression": expression, "returnByValue": true
            }),
        )?;
        if response
            .pointer("/result/result/value")
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Ok(websocket);
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err("浏览器环境尚未生效，请重试".to_string())
}

fn auto_attach_params() -> Value {
    json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": true,
        "flatten": true,
        "filter": [{"type": "page"}, {"type": "iframe"}, {"exclude": true}]
    })
}

fn configure_target(
    websocket: &mut CdpWebSocket,
    event: &Value,
    settings: &BrowserEnvironment,
) -> Result<(), String> {
    let session = event
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| "浏览器页面未提供环境会话".to_string())?;
    let result: Result<(), String> = (|| {
        if !settings.timezone.is_empty() {
            websocket
                .call_in_session(
                    "Emulation.setTimezoneOverride",
                    json!({"timezoneId": settings.timezone}),
                    Some(session),
                )
                .map_err(|_| "应用浏览器时区失败，请检查时区名称".to_string())?;
        }
        if !settings.language.is_empty() {
            websocket
                .call_in_session(
                    "Emulation.setUserAgentOverride",
                    json!({"userAgent": "", "acceptLanguage": settings.language}),
                    Some(session),
                )
                .map_err(|_| "应用浏览器语言失败".to_string())?;
        }
        // 页面内的跨域 iframe 也有自己的 Target，需要递归附加。
        websocket.call_in_session("Target.setAutoAttach", auto_attach_params(), Some(session))?;
        Ok(())
    })();
    // 包括应用设置失败的情况，避免新标签页一直暂停。
    let resumed =
        websocket.call_in_session("Runtime.runIfWaitingForDebugger", json!({}), Some(session));
    result?;
    resumed.map(|_| ())
}

pub(super) fn remove_browser(port: u16) {
    if let Some(mut browsers) = BROWSERS.get().and_then(|browsers| browsers.lock().ok()) {
        browsers.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::super::CDP_CANCELLED;
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // 用本地 CDP 对端模拟网络错误，避免测试依赖真实站点及账号。
    fn mock_browser(
        outcomes: Vec<Option<&'static str>>,
    ) -> (CdpWebSocket, Arc<AtomicUsize>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&attempts);
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut peer = CdpWebSocket {
                stream,
                next_id: 1,
                attachment_events: None,
            };
            for outcome in outcomes {
                let Ok(message) = peer.read_message() else {
                    break;
                };
                let request: Value = serde_json::from_str(&message).unwrap();
                assert_eq!(request["method"], "Page.navigate");
                count.fetch_add(1, Ordering::SeqCst);
                let Some(error) = outcome else { continue };
                let result = if error.is_empty() {
                    json!({"frameId": "test"})
                } else {
                    json!({"errorText": error})
                };
                let reply = json!({"id": request["id"], "result": result}).to_string();
                assert!(reply.len() < 126);
                peer.stream.write_all(&[0x81, reply.len() as u8]).unwrap();
                peer.stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        (
            CdpWebSocket {
                stream,
                next_id: 1,
                attachment_events: None,
            },
            attempts,
            worker,
        )
    }

    #[test]
    fn transient_disconnects_recover_without_submitting_other_commands() {
        let (websocket, attempts, worker) = mock_browser(vec![
            Some("net::ERR_CONNECTION_CLOSED"),
            Some("net::ERR_CONNECTION_RESET"),
            Some(""),
        ]);
        let result = tauri::async_runtime::block_on(navigate_with_retry(
            websocket,
            "https://example.com/",
            None,
            Duration::from_secs(2),
            Duration::from_millis(1),
        ));
        worker.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn certificate_errors_and_aborted_navigations_are_not_retried() {
        for error in ["net::ERR_CERT_AUTHORITY_INVALID", "net::ERR_ABORTED"] {
            let (websocket, attempts, worker) = mock_browser(vec![Some(error), Some("")]);
            let result = tauri::async_runtime::block_on(navigate_with_retry(
                websocket,
                "https://example.com/",
                None,
                Duration::from_secs(2),
                Duration::from_millis(1),
            ));
            worker.join().unwrap();
            assert!(result.unwrap_err().contains(error));
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn repeated_failures_share_one_deadline() {
        let (websocket, attempts, worker) =
            mock_browser(vec![Some("net::ERR_CONNECTION_CLOSED"); 100]);
        let started = Instant::now();
        let result = tauri::async_runtime::block_on(navigate_with_retry(
            websocket,
            "https://example.com/",
            None,
            Duration::from_millis(60),
            Duration::from_millis(5),
        ));
        worker.join().unwrap();
        assert!(result.unwrap_err().contains("ERR_CONNECTION_CLOSED"));
        assert!(started.elapsed() >= Duration::from_millis(60));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(attempts.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn cancellation_interrupts_an_in_flight_navigation() {
        let (websocket, attempts, worker) = mock_browser(vec![None, None]);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = CdpProgress::new(
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            Arc::clone(&cancel),
        );
        let started = Instant::now();
        let result = tauri::async_runtime::block_on(async {
            let count = Arc::clone(&attempts);
            let cancellation = tauri::async_runtime::spawn(async move {
                while count.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                cancel.store(true, Ordering::SeqCst);
            });
            let result = navigate_with_retry(
                websocket,
                "https://example.com/",
                Some(&progress),
                Duration::from_secs(5),
                Duration::from_millis(1),
            )
            .await;
            cancellation.await.unwrap();
            result
        });
        worker.join().unwrap();
        assert_eq!(result.unwrap_err(), CDP_CANCELLED);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
