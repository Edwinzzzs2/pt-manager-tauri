use crate::commands::AppState;
use serde::Serialize;
use tauri::{State, Url};
use tauri_plugin_updater::{Update, UpdaterExt};

const UPDATE_ENDPOINT: &str =
    "https://github.com/Edwinzzzs2/pt-manager-tauri/releases/latest/download/latest.json";

#[derive(Debug, Serialize)]
pub struct AppUpdateInfo {
    pub version: String,
}

pub fn normalize_proxy_url(value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return Ok(String::new());
    }

    let url = Url::parse(value).map_err(|_| "更新代理地址格式不正确".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("更新代理地址必须是有效的 HTTP 或 HTTPS 地址".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("更新代理地址不能包含查询参数或锚点".to_string());
    }

    Ok(value.to_string())
}

#[tauri::command]
pub async fn check_for_app_update(
    state: State<'_, AppState>,
) -> Result<Option<AppUpdateInfo>, String> {
    let proxy_url = state.config.lock().await.update_proxy_url.clone();
    let update = find_update(&state.app_handle, &proxy_url).await?;
    Ok(update.map(|update| AppUpdateInfo {
        version: update.version,
    }))
}

#[tauri::command]
pub async fn download_and_install_app_update(
    state: State<'_, AppState>,
    expected_version: String,
) -> Result<(), String> {
    let proxy_url = state.config.lock().await.update_proxy_url.clone();
    let update = find_update(&state.app_handle, &proxy_url)
        .await?
        .ok_or_else(|| "当前已是最新版本".to_string())?;

    if update.version != expected_version {
        return Err(format!(
            "可用版本已从 {expected_version} 变更为 {}，请重新检查更新",
            update.version
        ));
    }

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| format!("下载或安装更新失败：{err}"))
}

async fn find_update(
    app_handle: &tauri::AppHandle,
    proxy_url: &str,
) -> Result<Option<Update>, String> {
    let endpoint = Url::parse(UPDATE_ENDPOINT).map_err(|err| err.to_string())?;
    let endpoint = prepend_proxy_url(proxy_url, &endpoint)?;
    let updater = app_handle
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|err| format!("更新地址配置失败：{err}"))?
        .build()
        .map_err(|err| format!("更新器初始化失败：{err}"))?;

    let Some(mut update) = updater
        .check()
        .await
        .map_err(|err| format!("检查更新失败：{err}"))?
    else {
        return Ok(None);
    };

    // 更新清单里的安装包仍是 GitHub 原地址，必须再次加代理前缀，才能保证客户端全程不直连 GitHub。
    update.download_url = prepend_proxy_url(proxy_url, &update.download_url)?;
    Ok(Some(update))
}

fn prepend_proxy_url(proxy_url: &str, target_url: &Url) -> Result<Url, String> {
    let proxy_url = normalize_proxy_url(proxy_url)?;
    if proxy_url.is_empty() {
        return Ok(target_url.clone());
    }

    Url::parse(&format!("{proxy_url}/{}", target_url.as_str()))
        .map_err(|_| "无法生成更新代理地址".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_original_url_when_proxy_is_empty() {
        let target = Url::parse(UPDATE_ENDPOINT).unwrap();
        assert_eq!(prepend_proxy_url("", &target).unwrap(), target);
    }

    #[test]
    fn prepends_proxy_to_github_url() {
        let target = Url::parse(UPDATE_ENDPOINT).unwrap();
        let result = prepend_proxy_url("https://vercel-proxy.decoffee.top/", &target).unwrap();
        assert_eq!(
            result.as_str(),
            "https://vercel-proxy.decoffee.top/https://github.com/Edwinzzzs2/pt-manager-tauri/releases/latest/download/latest.json"
        );
    }
}
