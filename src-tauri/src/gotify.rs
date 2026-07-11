use crate::store::GotifyConfig;
use std::time::Duration;

pub async fn send_login_summary(
    config: &GotifyConfig,
    successful_sites: &[String],
    failed_sites: &[(String, String)],
) -> Result<(), String> {
    if !config.enabled || (successful_sites.is_empty() && failed_sites.is_empty()) {
        return Ok(());
    }

    let server_url = config.server_url.trim().trim_end_matches('/');
    let token = config.token.trim();
    if server_url.is_empty() || token.is_empty() {
        return Err("Gotify 已启用，但服务地址或应用 Token 未配置".to_string());
    }

    let mut sections = Vec::new();
    if !successful_sites.is_empty() {
        sections.push(format!(
            "登录成功（{}）\n{}",
            successful_sites.len(),
            successful_sites
                .iter()
                .map(|name| format!("- {}", name))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !failed_sites.is_empty() {
        sections.push(format!(
            "登录失败（{}）\n{}",
            failed_sites.len(),
            failed_sites
                .iter()
                .map(|(name, reason)| format!("- {}：{}", name, reason))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| "Gotify HTTP 客户端初始化失败".to_string())?;
    let response = client
        .post(format!("{}/message", server_url))
        .query(&[("token", token)])
        .json(&serde_json::json!({
            "title": "PT Manager 登录结果",
            "message": sections.join("\n\n"),
            "priority": if failed_sites.is_empty() { 2 } else { 5 }
        }))
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                "Gotify 通知发送失败：请求超时".to_string()
            } else {
                "Gotify 通知发送失败：无法连接服务".to_string()
            }
        })?;

    if !response.status().is_success() {
        return Err(format!("Gotify 通知发送失败：HTTP {}", response.status()));
    }

    Ok(())
}
