use crate::store::GotifyConfig;
use std::time::Duration;

pub async fn send_login_summary(
    config: &GotifyConfig,
    successful_sites: &[String],
    failed_sites: &[(String, String)],
) -> Result<(), String> {
    if !config.enabled {
        return Ok(());
    }

    let server_url = config.server_url.trim().trim_end_matches('/');
    let token = config.token.trim();
    if server_url.is_empty() || token.is_empty() {
        return Err("Gotify 已启用，但服务地址或应用 Token 未配置".to_string());
    }

    let mut sections = Vec::new();
    if successful_sites.is_empty() && failed_sites.is_empty() {
        sections.push("保活任务已完成，本次未执行自动登录。".to_string());
    }
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
            "title": if config.title.trim().is_empty() {
                "PT Manager 保活结果"
            } else {
                config.title.trim()
            },
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

pub async fn send_test(config: &GotifyConfig) -> Result<(), String> {
    let server_url = config.server_url.trim().trim_end_matches('/');
    let token = config.token.trim();
    if server_url.is_empty() || token.is_empty() {
        return Err("请先填写 Gotify 服务地址和应用 Token".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| "Gotify HTTP 客户端初始化失败".to_string())?;
    let response = client
        .post(format!("{}/message", server_url))
        .query(&[("token", token)])
        .json(&serde_json::json!({
            "title": if config.title.trim().is_empty() {
                "PT Manager 测试通知"
            } else {
                config.title.trim()
            },
            "message": "Gotify 连接测试成功，通知配置可用。",
            "priority": 5
        }))
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                "Gotify 测试失败：请求超时".to_string()
            } else {
                format!("Gotify 测试失败：无法连接服务（{}）", err)
            }
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(if detail.trim().is_empty() {
            format!("Gotify 测试失败：HTTP {}", status)
        } else {
            format!("Gotify 测试失败：HTTP {}，{}", status, detail.trim())
        });
    }

    Ok(())
}
