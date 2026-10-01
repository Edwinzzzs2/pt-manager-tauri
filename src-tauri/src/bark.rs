use crate::cdp::{SigninResult, SiteTraffic};
use crate::gotify;
use crate::store::BarkConfig;
use serde_json::{json, Value};
use std::time::Duration;

pub fn validate_config(config: &BarkConfig) -> Result<(), String> {
    let server_url = config.server_url.trim().trim_end_matches('/');
    if server_url.is_empty() || config.device_key.trim().is_empty() {
        return Err("启用 Bark 通知前，请填写服务地址和设备 Key".to_string());
    }
    let url = reqwest::Url::parse(server_url).map_err(|_| "Bark 服务地址格式无效".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Bark 服务地址须为不含账号和查询参数的 HTTP(S) 地址".to_string());
    }
    Ok(())
}

async fn send(config: &BarkConfig, title: &str, body: &str) -> Result<(), String> {
    validate_config(config)?;
    let endpoint = format!("{}/push", config.server_url.trim().trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| "Bark HTTP 客户端初始化失败".to_string())?;
    // Key 放在请求体中，不拼入 URL，避免代理日志记录设备凭据。
    let response = client
        .post(endpoint)
        .json(&json!({
            "device_key": config.device_key.trim(),
            "title": title,
            "body": body,
            "group": "PT Manager"
        }))
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                "Bark 通知发送失败：请求超时".to_string()
            } else {
                "Bark 通知发送失败：无法连接服务".to_string()
            }
        })?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|_| format!("Bark 通知发送失败：服务返回非 JSON 内容（HTTP {status}）"))?;
    let code = payload.get("code").and_then(Value::as_i64);
    if !status.is_success() || code != Some(200) {
        let detail = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("服务未返回错误详情");
        return Err(format!(
            "Bark 通知发送失败：HTTP {status}，{}",
            detail.chars().take(160).collect::<String>()
        ));
    }
    Ok(())
}

pub async fn send_login_summary(
    config: &BarkConfig,
    successful_sites: &[String],
    failed_sites: &[(String, String)],
    signin_results: &[(String, SigninResult)],
    traffic_results: &[(String, SiteTraffic)],
) -> Result<(), String> {
    if !config.enabled {
        return Ok(());
    }
    let body = gotify::build_plain_report(
        successful_sites,
        failed_sites,
        signin_results,
        traffic_results,
    );
    send(config, config.title.trim(), &body).await
}

pub async fn send_test(config: &BarkConfig) -> Result<(), String> {
    let title = if config.title.trim().is_empty() {
        "PT Manager 测试通知"
    } else {
        config.title.trim()
    };
    send(config, title, "Bark 连接测试成功，通知配置可用。").await
}
