//! 通用 NexusPHP 登录、剩余次数保护、人机验证和图片验证码流程。

use super::common::{
    click_runtime_element, human_delay, nexus_captcha_page_state, nexus_login_page_state,
    type_runtime_input,
};
use crate::cdp::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use std::time::Duration;

impl CdpClient {
    /// 通用 NexusPHP 站点的登录逻辑（包含可选的 TOTP、Cloudflare 绕过、以及自动 OCR 验证码识别）
    pub(super) async fn login_nexusphp(
        &self,
        tab_id: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
        min_remaining_attempts: u32,
        ocr_config: Option<(String, u8)>,
        progress: Option<&CdpProgress>,
        site_name: &str,
    ) -> Result<(bool, Option<u32>), (String, Option<u32>)> {
        let Some(websocket_url) = self
            .websocket_url_for_tab(tab_id)
            .map_err(|err| (err, None))?
        else {
            return Err((format!("无法连接 {} 标签页", site_name), None));
        };

        let ocr_cfg = ocr_config.clone();
        let max_attempts = ocr_config
            .as_ref()
            .map(|(_, count)| *count as usize)
            .unwrap_or(1);
        let mut last_remaining_attempts = None;

        for attempt in 0..max_attempts {
            if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
            }
            if attempt > 0 {
                if let Some(p) = progress {
                    p.info(format!(
                        "前一次登录尝试失败，准备点击获取新的图片代码进行第 {}/{} 次重试...",
                        attempt + 1,
                        max_attempts
                    ))
                    .await;
                }
                self.prepare_nexusphp_captcha_retry(tab_id)
                    .await
                    .map_err(|err| (err, last_remaining_attempts))?;
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }

            let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))
                .map_err(|err| (err.to_string(), last_remaining_attempts))?;
            let mut login_state = None;
            let mut stable_logged_in = 0usize;

            for _ in 0..240 {
                if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                    return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
                }
                if let Some(current) = nexus_login_page_state(&mut websocket) {
                    if let Some(rem) = current.remaining_attempts {
                        last_remaining_attempts = Some(rem);
                    }
                    if current.has_login_form {
                        login_state = Some(current);
                        break;
                    }
                    if current.ready && !current.challenge && current.logged_in {
                        stable_logged_in += 1;
                        if stable_logged_in >= 3 {
                            return Ok((false, last_remaining_attempts)); // 已经登录，直接返回
                        }
                    } else {
                        stable_logged_in = 0;
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            let Some(login_state) = login_state else {
                return Err((
                    format!("等待 {} Cloudflare 验证放行超时（120 秒）", site_name),
                    last_remaining_attempts,
                ));
            };

            if let Some(remaining) = login_state.remaining_attempts {
                last_remaining_attempts = Some(remaining);
                if remaining <= min_remaining_attempts {
                    return Err((
                        format!(
                            "{} 当前仅剩 {} 次登录机会，已达到安全阈值 {}，停止自动登录与重试",
                            site_name, remaining, min_remaining_attempts
                        ),
                        last_remaining_attempts,
                    ));
                }
            }

            human_delay(650, 1350).await;
            if !type_runtime_input(
                &mut websocket,
                "input[name=\"username\"], input[name=\"email\"], input[name=\"login\"], input[autocomplete=\"username\"]",
                username,
                45,
                120,
            )
            .await
            {
                return Err((format!("未找到 {} 用户名输入框", site_name), last_remaining_attempts));
            }
            human_delay(500, 1150).await;
            if !type_runtime_input(
                &mut websocket,
                "input[name=\"password\"], input[type=\"password\"]",
                password,
                55,
                140,
            )
            .await
            {
                return Err((
                    format!("未找到 {} 密码输入框", site_name),
                    last_remaining_attempts,
                ));
            }
            if let Some(secret) = totp_secret {
                if login_state.has_two_factor {
                    let code =
                        auth::current_totp(secret).map_err(|err| (err, last_remaining_attempts))?;
                    human_delay(450, 950).await;
                    if !type_runtime_input(
                        &mut websocket,
                        "input[name=\"two_factor\"], input[name=\"twofactor\"], input[name=\"two_step_code\"], input[name=\"otp\"], input[name=\"two_factor_code\"], input[name=\"2fa_secret\"], input[name=\"2fa\"], input[autocomplete=\"one-time-code\"]",
                        &code,
                        70,
                        145,
                    )
                    .await
                    {
                        return Err((format!("未找到 {} 两步验证码输入框", site_name), last_remaining_attempts));
                    }
                }
            }

            let has_ocr = ocr_config.is_some();
            let current = nexus_login_page_state(&mut websocket).ok_or_else(|| {
                (
                    format!("无法读取 {} 登录表单状态", site_name),
                    last_remaining_attempts,
                )
            })?;

            if let Some(rem) = current.remaining_attempts {
                last_remaining_attempts = Some(rem);
            }

            if current.has_captcha {
                if let Some((ocr_server_url, ocr_retry_count)) = ocr_cfg.clone() {
                    if let Some(p) = progress {
                        p.info("检测到图片验证码，正在获取图片数据并进行自动 OCR 识别...")
                            .await;
                    }

                    let expression = r#"(() => {
                        const image = document.querySelector('img[alt="CAPTCHA"], img[src*="captcha" i], img[src*="image.php" i]');
                        if (!image || !image.complete || !image.naturalWidth) return { ok: false };
                        try {
                            const scale = 3;
                            const padding = 8;
                            const canvas = document.createElement('canvas');
                            canvas.width = image.naturalWidth * scale + padding * 2;
                            canvas.height = image.naturalHeight * scale + padding * 2;
                            const context = canvas.getContext('2d', { alpha: false });
                            context.fillStyle = '#ffffff';
                            context.fillRect(0, 0, canvas.width, canvas.height);
                            context.imageSmoothingEnabled = false;
                            context.filter = 'contrast(140%) saturate(120%)';
                            context.drawImage(
                                image,
                                padding,
                                padding,
                                image.naturalWidth * scale,
                                image.naturalHeight * scale
                            );
                            context.filter = 'none';
                            return {
                                ok: true,
                                image: canvas.toDataURL('image/png').split(',')[1],
                                width: canvas.width,
                                height: canvas.height
                            };
                        } catch (_) {
                            return { ok: false };
                        }
                    })()"#;

                    let response = websocket
                        .call(
                            "Runtime.evaluate",
                            serde_json::json!({
                                "expression": expression,
                                "returnByValue": true
                            }),
                        )
                        .map_err(|err| {
                            (format!("截取验证码失败：{}", err), last_remaining_attempts)
                        })?;

                    let value = response
                        .get("result")
                        .and_then(|value| value.get("result"))
                        .and_then(|value| value.get("value"))
                        .ok_or_else(|| {
                            (
                                "未读取到验证码图片数据".to_string(),
                                last_remaining_attempts,
                            )
                        })?;

                    if !value
                        .get("ok")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false)
                    {
                        return Err((
                            "验证码图片尚未加载或无法截取".to_string(),
                            last_remaining_attempts,
                        ));
                    }

                    let image_base64 = value
                        .get("image")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| {
                            ("验证码截图数据为空".to_string(), last_remaining_attempts)
                        })?;

                    if let Some(p) = progress {
                        p.info(format!(
                            "获取验证码图片成功，准备发送给 OCR 服务，Base64 为：{}",
                            image_base64
                        ))
                        .await;
                    }

                    let ocr_server_url_clone = ocr_server_url.clone();
                    let image_base64_clone = image_base64.to_string();
                    let recognition = tauri::async_runtime::spawn_blocking(move || {
                        crate::ocr::recognize(
                            &ocr_server_url_clone,
                            &image_base64_clone,
                            ocr_retry_count,
                        )
                    })
                    .await
                    .map_err(|err| (err.to_string(), last_remaining_attempts))?;

                    let recognition = match recognition {
                        Ok(res) => res,
                        Err(err) => {
                            return Err((
                                format!("验证码自动识别失败：{}", err),
                                last_remaining_attempts,
                            ))
                        }
                    };

                    if let Some(p) = progress {
                        p.info(format!(
                            "验证码识别成功：{}，尝试次数：{}/{}。正在自动填入...",
                            recognition.text, recognition.attempts, ocr_retry_count
                        ))
                        .await;
                    }

                    human_delay(500, 1000).await;
                    if !type_runtime_input(
                        &mut websocket,
                        "input[name=\"imagestring\"]",
                        &recognition.text,
                        70,
                        145,
                    )
                    .await
                    {
                        return Err((
                            format!("未找到 {} 图片验证码输入框", site_name),
                            last_remaining_attempts,
                        ));
                    }
                } else {
                    let attempts = current
                        .remaining_attempts
                        .map(|value| format!("，当前剩余 {} 次尝试", value))
                        .unwrap_or_default();
                    return Err((
                        format!(
                            "{} 登录信息已填写{}，请人工输入图片验证码并点击登录",
                            site_name, attempts
                        ),
                        last_remaining_attempts,
                    ));
                }
            }

            let mut captcha_solved = false;
            let mut logged_challenge_msg = false;
            let mut stable_solved_samples = 0usize;

            for _ in 0..120 {
                // 最多等待 60 秒
                if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                    return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
                }

                let eval_expr = r#"(() => {
                    const fields = document.querySelectorAll('[name="cf-turnstile-response"], [name="g-recaptcha-response"], [name="h-captcha-response"]');
                    const challengeElement = document.querySelector([
                        '.cf-turnstile',
                        '.g-recaptcha',
                        '.h-captcha',
                        'iframe[src*="challenges.cloudflare.com"]',
                        'iframe[src*="recaptcha"]',
                        'iframe[src*="hcaptcha.com"]'
                    ].join(','));
                    const challengeScript = Array.from(document.scripts).some((script) => {
                        const src = (script.src || '').toLowerCase();
                        return src.includes('challenges.cloudflare.com/turnstile')
                            || src.includes('recaptcha/api.js')
                            || src.includes('hcaptcha.com/1/api.js');
                    });
                    const hasChallenge = fields.length > 0 || Boolean(challengeElement) || challengeScript;
                    const solved = Array.from(fields).some((field) =>
                        typeof field.value === 'string' && field.value.trim().length > 10
                    );
                    return { hasChallenge, solved };
                })()"#;

                let val_res = websocket.call(
                    "Runtime.evaluate",
                    serde_json::json!({
                        "expression": eval_expr,
                        "returnByValue": true
                    }),
                );

                if let Ok(response) = val_res {
                    if let Some(val) = response
                        .get("result")
                        .and_then(|r| r.get("result"))
                        .and_then(|r| r.get("value"))
                    {
                        let has_challenge = val
                            .get("hasChallenge")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let solved = val.get("solved").and_then(|v| v.as_bool()).unwrap_or(true);

                        if !has_challenge {
                            captcha_solved = true;
                            break;
                        }

                        if solved {
                            stable_solved_samples += 1;
                            if stable_solved_samples >= 2 {
                                captcha_solved = true;
                                break;
                            }
                        } else {
                            stable_solved_samples = 0;
                        }

                        if !logged_challenge_msg {
                            if let Some(p) = progress {
                                p.info("检测到页面存在人机验证（如 Cloudflare Turnstile），请在浏览器中完成验证...").await;
                            }
                            logged_challenge_msg = true;
                        }
                    }
                }

                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            if !captcha_solved {
                return Err((
                    format!("等待 {} 人机验证通过超时", site_name),
                    last_remaining_attempts,
                ));
            }

            let delay_min = if has_ocr { 1200 } else { 750 };
            let delay_max = if has_ocr { 2500 } else { 1550 };
            human_delay(delay_min, delay_max).await;
            let submit_expr = r#"(() => {
                const passwordInput = document.querySelector('input[type="password"]');
                if (!passwordInput) return false;
                const form = passwordInput.form || passwordInput.closest('form');
                if (!form) return false;

                // Some NexusPHP sites use challenge-response authentication. Their
                // visible login button runs an async script that fills `response`
                // before submitting, so requestSubmit()/form.submit() must not bypass it.
                if (form.querySelector('input[name="response"]')) {
                    const challengeSubmit = form.querySelector(
                        '#submit-btn, input[type="button"][value="登录"], input[type="button"][value="Login"]'
                    );
                    if (!challengeSubmit) return false;
                    challengeSubmit.click();
                    return true;
                }

                const submitBtn = form.querySelector('input[type="submit"], button[type="submit"]');
                if (submitBtn) {
                    submitBtn.click();
                    return true;
                }

                if (typeof form.requestSubmit === 'function') {
                    form.requestSubmit();
                    return true;
                }

                form.submit();
                return true;
            })()"#;

            let mut submitted = false;
            if let Ok(response) = websocket.call(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": submit_expr,
                    "returnByValue": true
                }),
            ) {
                if let Some(val) = response
                    .get("result")
                    .and_then(|r| r.get("result"))
                    .and_then(|r| r.get("value"))
                {
                    submitted = val.as_bool().unwrap_or(false);
                }
            }

            if !submitted {
                if !click_runtime_element(&mut websocket, "input[type=\"submit\"][value=\"登录\"], input[type=\"submit\"][value=\"进入！\"], input[type=\"submit\"][value=\"Login\"], button[type=\"submit\"]") {
                    if !click_runtime_element(&mut websocket, "input[type=\"submit\"]") {
                        return Err((format!("未找到 {} 登录按钮", site_name), last_remaining_attempts));
                    }
                }
            }

            let mut login_success = false;
            let mut captcha_failed = false;
            for _ in 0..40 {
                if progress.map(|p| p.is_cancelled()).unwrap_or(false) {
                    return Err((CDP_CANCELLED.to_string(), last_remaining_attempts));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(current) = nexus_login_page_state(&mut websocket) else {
                    continue;
                };
                if let Some(rem) = current.remaining_attempts {
                    last_remaining_attempts = Some(rem);
                }
                if current.ready && !current.challenge {
                    if current.logged_in {
                        login_success = true;
                        break;
                    }
                    if current.is_failure_page {
                        captcha_failed = true;
                        break;
                    }
                }
            }

            if login_success {
                return Ok((true, last_remaining_attempts));
            }
            if captcha_failed {
                if attempt + 1 >= max_attempts {
                    return Err((
                        format!(
                            "{} 自动登录失败：图片验证码错误，已达到最大重试次数 {} 次",
                            site_name, max_attempts
                        ),
                        last_remaining_attempts,
                    ));
                }
                continue;
            }

            let final_state = nexus_login_page_state(&mut websocket);
            if let Some(state) = final_state {
                if let Some(rem) = state.remaining_attempts {
                    last_remaining_attempts = Some(rem);
                }
                if !state.has_login_form && !state.logged_in {
                    return Err((
                        format!(
                            "{} 自动登录失败：跳转到了非预期页面（可能是用户名或密码错误）",
                            site_name
                        ),
                        last_remaining_attempts,
                    ));
                }
            }

            return Err((
                format!("{} 登录后仍停留在登录页，请检查凭据", site_name),
                last_remaining_attempts,
            ));
        }

        Err((
            format!(
                "{} 自动登录失败：已尝试 {} 次均未成功",
                site_name, max_attempts
            ),
            last_remaining_attempts,
        ))
    }

    pub async fn nexusphp_remaining_attempts(&self, tab_id: &str) -> Result<Option<u32>, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 NexusPHP 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        nexus_captcha_page_state(&mut websocket)
            .map(|state| state.remaining_attempts)
            .ok_or_else(|| "无法读取 NexusPHP 剩余登录次数".to_string())
    }

    pub async fn nexusphp_captcha_base64(&self, tab_id: &str) -> Result<String, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 NexusPHP 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let expression = r#"(() => {
            const image = document.querySelector('img[alt="CAPTCHA"], img[src*="captcha" i], img[src*="image.php" i]');
            if (!image || !image.complete || !image.naturalWidth) return { ok: false };
            try {
                const scale = 3;
                const padding = 8;
                const canvas = document.createElement('canvas');
                canvas.width = image.naturalWidth * scale + padding * 2;
                canvas.height = image.naturalHeight * scale + padding * 2;
                const context = canvas.getContext('2d', { alpha: false });
                context.fillStyle = '#ffffff';
                context.fillRect(0, 0, canvas.width, canvas.height);
                context.imageSmoothingEnabled = false;
                context.filter = 'contrast(140%) saturate(120%)';
                context.drawImage(
                    image,
                    padding,
                    padding,
                    image.naturalWidth * scale,
                    image.naturalHeight * scale
                );
                context.filter = 'none';
                return {
                    ok: true,
                    image: canvas.toDataURL('image/png').split(',')[1],
                    width: canvas.width,
                    height: canvas.height
                };
            } catch (_) {
                return { ok: false };
            }
        })()"#;
        let response = websocket
            .call(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "returnByValue": true
                }),
            )
            .map_err(|err| format!("截取验证码失败：{}", err))?;
        let value = response
            .get("result")
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("value"))
            .ok_or_else(|| "未读取到验证码图片".to_string())?;
        if !value
            .get("ok")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return Err("验证码图片尚未加载或无法截取".to_string());
        }
        value
            .get("image")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| "验证码截图数据为空".to_string())
    }

    /// 如果当前是图片代码无效页面，进入新的 NexusPHP 验证码登录页。
    pub async fn prepare_nexusphp_captcha_retry(&self, tab_id: &str) -> Result<bool, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 NexusPHP 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let expression = r#"(() => {
            if (document.querySelector('input[name="imagestring"]')) {
                return { ok: true, renewed: false };
            }
            const body = document.body?.innerText || '';
            if (!body.includes('图片代码无效')) return { ok: false, renewed: false };
            const link = Array.from(document.querySelectorAll('a')).find((element) =>
                (element.textContent || '').includes('获取新的图片代码')
                || (element.getAttribute('href') || '').includes('login.php')
            );
            if (!link) return { ok: false, renewed: false };
            link.click();
            return { ok: true, renewed: true };
        })()"#;
        let response = websocket
            .call(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "returnByValue": true,
                    "userGesture": true
                }),
            )
            .map_err(|err| format!("检查 NexusPHP 验证码页面失败：{}", err))?;
        let value = response
            .get("result")
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("value"))
            .ok_or_else(|| "无法读取 NexusPHP 验证码页面状态".to_string())?;
        if !value
            .get("ok")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return Err("当前页面没有可识别的验证码，也不是图片代码无效页面".to_string());
        }
        let renewed = value
            .get("renewed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if renewed {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if nexus_captcha_page_state(&mut websocket)
                    .is_some_and(|state| state.has_login_form && state.has_captcha)
                {
                    return Ok(true);
                }
            }
            return Err("已请求新的图片验证码，但登录页加载超时".to_string());
        }
        Ok(false)
    }

    /// 获取新验证码后重新填写登录表单，但把最终提交留给手动识别流程处理。
    pub async fn refill_nexusphp_login_for_captcha(
        &self,
        tab_id: &str,
        site_name: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
        min_remaining_attempts: u32,
    ) -> Result<Option<u32>, String> {
        match self
            .login_nexusphp(
                tab_id,
                username,
                password,
                totp_secret,
                min_remaining_attempts,
                None,
                None,
                site_name,
            )
            .await
        {
            Err((message, remaining_attempts)) if message.contains("登录信息已填写") => {
                Ok(remaining_attempts)
            }
            Err((message, _)) => Err(format!("重新填写 NexusPHP 登录信息失败：{}", message)),
            Ok(_) => Err("NexusPHP 当前不再需要图片验证码".to_string()),
        }
    }

    pub async fn fill_nexusphp_captcha(&self, tab_id: &str, code: &str) -> Result<(), String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("无法连接 NexusPHP 标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        if type_runtime_input(&mut websocket, "input[name=\"imagestring\"]", code, 70, 145).await {
            Ok(())
        } else {
            Err("未找到 NexusPHP 图片验证码输入框".to_string())
        }
    }
}
