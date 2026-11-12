use super::{CdpTab, CdpWebSocket};
use crate::store::BrowserEnvironment;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::{Shutdown, TcpStream};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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

pub(super) fn navigate(
    tab: &CdpTab,
    settings: &BrowserEnvironment,
    url: &str,
) -> Result<(), String> {
    let endpoint = tab
        .web_socket_debugger_url
        .as_deref()
        .ok_or_else(|| "浏览器未提供标签页调试连接".to_string())?;
    let mut websocket = CdpWebSocket::connect(endpoint, Duration::from_secs(5))?;
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
            let response = websocket.call("Page.navigate", json!({"url": url.trim()}))?;
            if let Some(error) = response
                .pointer("/result/errorText")
                .and_then(Value::as_str)
            {
                return Err(format!("打开站点失败：{error}"));
            }
            return Ok(());
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
