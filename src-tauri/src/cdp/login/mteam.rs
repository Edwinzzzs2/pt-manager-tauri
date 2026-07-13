//! M-Team 登录页和二次验证流程。

use super::common::{
    click_runtime_element, human_delay, login_page_state, submit_otp, type_runtime_input,
};
use crate::cdp::{CdpClient, CdpWebSocket};
use std::time::Duration;

impl CdpClient {
    /// 在 M-Team 登录页填写凭据并提交。返回 false 表示当前页面不需要登录。
    pub(super) async fn login_mteam(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_code: Option<&str>,
    ) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 M-Team 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;

        let mut state = None;
        for _ in 0..120 {
            state = login_page_state(&mut websocket);
            if let Some(current) = state.as_ref() {
                if current.host == "kp.m-team.cc"
                    && current.ready
                    && (current.has_login_form || current.logged_in)
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let Some(current) = state else {
            return Err("无法读取 M-Team 登录页状态".to_string());
        };
        if current.host != "kp.m-team.cc" {
            return Err("M-Team 页面加载超时，未能进入站点页面".to_string());
        }
        if current.logged_in {
            return Ok(false);
        }
        if !current.has_login_form {
            if current.path == "/login" {
                return Err("M-Team 已进入登录页，但登录表单未加载完成".to_string());
            }
            return Err("M-Team 登录状态检测超时：未发现账户信息或登录表单".to_string());
        }

        human_delay(550, 1250).await;
        if !type_runtime_input(&mut websocket, "#username", username, 45, 115).await {
            return Err("未找到 M-Team 用户名输入框".to_string());
        }
        human_delay(450, 1050).await;
        if !type_runtime_input(&mut websocket, "#password", password, 55, 135).await {
            return Err("未找到 M-Team 密码输入框".to_string());
        }
        human_delay(700, 1550).await;
        if !click_runtime_element(&mut websocket, "button[type=\"submit\"]") {
            return Err("未找到 M-Team 登录按钮".to_string());
        }

        let mut otp_submitted = false;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(current) = login_page_state(&mut websocket) else {
                continue;
            };
            if current.has_otp && !otp_submitted {
                let code = totp_code
                    .ok_or_else(|| "M-Team 要求 2FA，但站点未配置 2FA 密钥".to_string())?;
                if !submit_otp(&mut websocket, code).await {
                    return Err("检测到 M-Team 2FA 验证，但未能填写或提交验证码".to_string());
                }
                otp_submitted = true;
                continue;
            }
            if current.logged_in {
                return Ok(true);
            }
        }
        Err("M-Team 登录后未检测到账户信息，请检查账号、密码、2FA 或验证码要求".to_string())
    }
}
