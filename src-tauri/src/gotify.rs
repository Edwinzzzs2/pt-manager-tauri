use crate::cdp::{SigninResult, SigninStatus, SiteTraffic};
use crate::store::GotifyConfig;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

const REPORT_SCHEMA: &str = "pt-manager.keepalive.report";
const REPORT_VERSION: u8 = 1;

fn human_value(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    compact
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "\\<")
        .replace('>', "\\>")
        .replace('#', "\\#")
        .replace('|', "\\|")
}

fn human_optional(value: Option<&str>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .map(human_value)
        .unwrap_or_else(|| "未获取".to_string())
}

fn signin_status_code(status: SigninStatus) -> &'static str {
    match status {
        SigninStatus::Success => "success",
        SigninStatus::AlreadySigned => "already_signed",
        SigninStatus::Failed => "failed",
    }
}

fn signin_status_label(status: SigninStatus) -> &'static str {
    match status {
        SigninStatus::Success => "签到成功",
        SigninStatus::AlreadySigned => "今日已签到",
        SigninStatus::Failed => "签到失败",
    }
}

fn build_report(
    successful_sites: &[String],
    failed_sites: &[(String, String)],
    signin_results: &[(String, SigninResult)],
    traffic_results: &[(String, SiteTraffic)],
) -> (String, Value) {
    let mut seen_login_sites = HashSet::new();
    let mut login_success_sites = Vec::new();
    for name in successful_sites
        .iter()
        .chain(traffic_results.iter().map(|(name, _)| name))
    {
        if seen_login_sites.insert(name.clone()) {
            login_success_sites.push(name);
        }
    }

    let successful_signins = signin_results
        .iter()
        .filter(|(_, result)| result.successful())
        .collect::<Vec<_>>();
    let failed_signins = signin_results
        .iter()
        .filter(|(_, result)| !result.successful())
        .collect::<Vec<_>>();

    let login_success_names = if login_success_sites.is_empty() {
        "无".to_string()
    } else {
        login_success_sites
            .iter()
            .map(|name| human_value(name))
            .collect::<Vec<_>>()
            .join("、")
    };
    let login_failed_names = if failed_sites.is_empty() {
        "无".to_string()
    } else {
        failed_sites
            .iter()
            .map(|(name, _)| human_value(name))
            .collect::<Vec<_>>()
            .join("、")
    };
    let signin_success_names = if successful_signins.is_empty() {
        "无".to_string()
    } else {
        successful_signins
            .iter()
            .map(|(name, _)| human_value(name))
            .collect::<Vec<_>>()
            .join("、")
    };
    let signin_failed_names = if failed_signins.is_empty() {
        "无".to_string()
    } else {
        failed_signins
            .iter()
            .map(|(name, _)| human_value(name))
            .collect::<Vec<_>>()
            .join("、")
    };

    let mut lines = vec![
        "## 1. 登录与流量".to_string(),
        format!(
            "✅ **成功 {}**：{}",
            login_success_sites.len(),
            login_success_names
        ),
        format!("❌ **失败 {}**：{}", failed_sites.len(), login_failed_names),
        String::new(),
        "**流量明细**".to_string(),
    ];
    if login_success_sites.is_empty() {
        lines.push("- 无".to_string());
    } else {
        for name in &login_success_sites {
            let traffic = traffic_results
                .iter()
                .find(|(traffic_name, _)| traffic_name == *name)
                .map(|(_, traffic)| traffic);
            lines.push(format!(
                "- **{}**　↑{}　↓{}　比 {}",
                human_value(name),
                human_optional(traffic.and_then(|value| value.upload.as_deref())),
                human_optional(traffic.and_then(|value| value.download.as_deref())),
                human_optional(traffic.and_then(|value| value.ratio.as_deref()))
            ));
        }
    }
    lines.push(String::new());
    lines.push("**登录失败原因**".to_string());
    if failed_sites.is_empty() {
        lines.push("- 无".to_string());
    } else {
        lines.extend(
            failed_sites.iter().map(|(name, reason)| {
                format!("- **{}**：{}", human_value(name), human_value(reason))
            }),
        );
    }

    lines.push(String::new());
    lines.push("## 2. 签到".to_string());
    lines.push(format!(
        "✅ **成功 {}**：{}",
        successful_signins.len(),
        signin_success_names
    ));
    lines.push(format!(
        "❌ **失败 {}**：{}",
        failed_signins.len(),
        signin_failed_names
    ));
    lines.push(String::new());
    lines.push("**签到成功明细**".to_string());
    if successful_signins.is_empty() {
        lines.push("- 无".to_string());
    } else {
        for (name, result) in &successful_signins {
            lines.push(format!(
                "- **{}**　{}　连续 {}　累计 {}",
                human_value(name),
                signin_status_label(result.status),
                result
                    .consecutive_days
                    .map(|days| format!("{days} 天"))
                    .unwrap_or_else(|| "未获取".to_string()),
                result
                    .total_days
                    .map(|days| format!("{days} 天"))
                    .unwrap_or_else(|| "未获取".to_string())
            ));
        }
    }
    lines.push(String::new());
    lines.push("**签到失败原因**".to_string());
    if failed_signins.is_empty() {
        lines.push("- 无".to_string());
    } else {
        lines.extend(failed_signins.iter().map(|(name, result)| {
            format!(
                "- **{}**：{}",
                human_value(name),
                human_value(&result.message)
            )
        }));
    }

    let login_success_json = login_success_sites
        .iter()
        .map(|name| {
            let traffic = traffic_results
                .iter()
                .find(|(traffic_name, _)| traffic_name == *name)
                .map(|(_, traffic)| traffic);
            json!({
                "site": name,
                "upload": traffic.and_then(|value| value.upload.as_deref()),
                "download": traffic.and_then(|value| value.download.as_deref()),
                "ratio": traffic.and_then(|value| value.ratio.as_deref())
            })
        })
        .collect::<Vec<_>>();
    let login_failed_json = failed_sites
        .iter()
        .map(|(name, reason)| json!({ "site": name, "reason": reason }))
        .collect::<Vec<_>>();
    let signin_success_json = successful_signins
        .iter()
        .map(|(name, result)| {
            json!({
                "site": name,
                "status": signin_status_code(result.status),
                "consecutive_days": result.consecutive_days,
                "total_days": result.total_days,
                "reward": result.reward,
                "message": result.message
            })
        })
        .collect::<Vec<_>>();
    let signin_failed_json = failed_signins
        .iter()
        .map(|(name, result)| {
            json!({
                "site": name,
                "status": signin_status_code(result.status),
                "reason": result.message
            })
        })
        .collect::<Vec<_>>();
    let report = json!({
        "schema": REPORT_SCHEMA,
        "version": REPORT_VERSION,
        "login": {
            "success_count": login_success_json.len(),
            "failure_count": login_failed_json.len(),
            "success": login_success_json,
            "failure": login_failed_json
        },
        "signin": {
            "success_count": signin_success_json.len(),
            "failure_count": signin_failed_json.len(),
            "success": signin_success_json,
            "failure": signin_failed_json
        }
    });

    (lines.join("\n"), report)
}

pub async fn send_login_summary(
    config: &GotifyConfig,
    successful_sites: &[String],
    failed_sites: &[(String, String)],
    signin_results: &[(String, SigninResult)],
    traffic_results: &[(String, SiteTraffic)],
) -> Result<(), String> {
    if !config.enabled {
        return Ok(());
    }

    let server_url = config.server_url.trim().trim_end_matches('/');
    let token = config.token.trim();
    if server_url.is_empty() || token.is_empty() {
        return Err("Gotify 已启用，但服务地址或应用 Token 未配置".to_string());
    }

    let (message, report) = build_report(
        successful_sites,
        failed_sites,
        signin_results,
        traffic_results,
    );

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
            "message": message,
            "priority": if failed_sites.is_empty() && signin_results.iter().all(|(_, result)| result.successful()) { 2 } else { 5 },
            "extras": {
                "client::display": {
                    "contentType": "text/markdown"
                },
                "ptmanager::report": report
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_human_layout_and_machine_v1_fields_are_stable() {
        let successful_sites = vec!["观众".to_string()];
        let failed_sites = vec![("失败站".to_string(), "登录超时".to_string())];
        let signin_results = vec![
            (
                "观众".to_string(),
                SigninResult {
                    status: SigninStatus::Success,
                    message: "签到成功".to_string(),
                    reward: Some("10 魔力".to_string()),
                    total_days: Some(30),
                    consecutive_days: Some(7),
                },
            ),
            (
                "失败站".to_string(),
                SigninResult::failure("未找到签到入口"),
            ),
        ];
        let traffic_results = vec![(
            "观众".to_string(),
            SiteTraffic {
                upload: Some("10 TB".to_string()),
                download: Some("5 TB".to_string()),
                ratio: Some("2".to_string()),
            },
        )];

        let (message, report) = build_report(
            &successful_sites,
            &failed_sites,
            &signin_results,
            &traffic_results,
        );

        assert_eq!(
            message,
            concat!(
                "## 1. 登录与流量\n",
                "✅ **成功 1**：观众\n",
                "❌ **失败 1**：失败站\n",
                "\n",
                "**流量明细**\n",
                "- **观众**　↑10 TB　↓5 TB　比 2\n",
                "\n",
                "**登录失败原因**\n",
                "- **失败站**：登录超时\n",
                "\n",
                "## 2. 签到\n",
                "✅ **成功 1**：观众\n",
                "❌ **失败 1**：失败站\n",
                "\n",
                "**签到成功明细**\n",
                "- **观众**　签到成功　连续 7 天　累计 30 天\n",
                "\n",
                "**签到失败原因**\n",
                "- **失败站**：未找到签到入口"
            )
        );
        assert_eq!(report["schema"], REPORT_SCHEMA);
        assert_eq!(report["version"], REPORT_VERSION);
        assert_eq!(report["login"]["success"][0]["upload"], "10 TB");
        assert_eq!(report["signin"]["success"][0]["consecutive_days"], 7);
        assert_eq!(report["signin"]["failure"][0]["reason"], "未找到签到入口");
    }
}
