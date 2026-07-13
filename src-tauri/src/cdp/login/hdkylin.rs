//! HDKylin 登录、雷池 WAF 状态检查和二次验证流程。

use super::common::{
    click_runtime_element, hdk_login_page_state, human_delay, submit_otp, type_runtime_input,
};
use crate::auth;
use crate::cdp::{CdpClient, CdpWebSocket};
use std::time::Duration;

impl CdpClient {
    /// 等待 HDKylin 的正常安全检测放行后，以带间隔的方式填写并提交登录表单。
    pub(super) async fn login_hdkylin(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
    ) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 HDKylin 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let mut login_ready = false;
        let mut stable_logged_in = 0usize;

        for _ in 0..240 {
            if let Some(current) = hdk_login_page_state(&mut websocket) {
                if current.blocked_debug {
                    return Err(
                        "雷池 WAF 检测到调试环境，需要在专用 Chrome 中人工完成验证".to_string()
                    );
                }
                if current.has_captcha {
                    return Err("HDKylin 登录页要求图形验证码，需要人工完成".to_string());
                }
                if current.has_login_form {
                    login_ready = true;
                    break;
                }
                if current.ready && !current.challenge && current.path != "/login.php" {
                    stable_logged_in += 1;
                    if stable_logged_in >= 6 {
                        return Ok(false);
                    }
                } else {
                    stable_logged_in = 0;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !login_ready {
            return Err("等待 HDKylin 安全检测放行超时（120 秒）".to_string());
        }

        human_delay(650, 1400).await;
        if !type_runtime_input(
            &mut websocket,
            "#username, input[name=\"username\"], input[autocomplete=\"username\"]",
            username,
            45,
            120,
        )
        .await
        {
            return Err("未找到 HDKylin 用户名输入框".to_string());
        }
        human_delay(500, 1200).await;
        if !type_runtime_input(
            &mut websocket,
            "#password, input[name=\"password\"], input[type=\"password\"]",
            password,
            55,
            140,
        )
        .await
        {
            return Err("未找到 HDKylin 密码输入框".to_string());
        }
        human_delay(750, 1650).await;
        if !click_runtime_element(
            &mut websocket,
            "button[type=\"submit\"], input[type=\"submit\"], input[name=\"login\"]",
        ) {
            return Err("未找到 HDKylin 登录按钮".to_string());
        }

        let mut otp_submitted = false;
        stable_logged_in = 0;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(current) = hdk_login_page_state(&mut websocket) else {
                continue;
            };
            if current.blocked_debug {
                return Err("登录过程中被雷池 WAF 拦截，需要人工完成验证".to_string());
            }
            if current.has_captcha {
                return Err("HDKylin 要求图形验证码，需要人工完成".to_string());
            }
            if current.has_otp && !otp_submitted {
                let secret = totp_secret
                    .ok_or_else(|| "HDKylin 要求 2FA，但站点未配置 2FA 密钥".to_string())?;
                let code = auth::current_totp(secret)?;
                human_delay(550, 1200).await;
                if !submit_otp(&mut websocket, &code).await {
                    return Err("检测到 HDKylin 2FA，但未能填写或提交验证码".to_string());
                }
                otp_submitted = true;
                stable_logged_in = 0;
                continue;
            }
            if current.ready && !current.challenge && !current.has_login_form {
                stable_logged_in += 1;
                if stable_logged_in >= 4 {
                    return Ok(true);
                }
            } else {
                stable_logged_in = 0;
            }
        }
        Err("HDKylin 登录后仍停留在登录页，请检查凭据或人工验证要求".to_string())
    }
}
