//! 豆瓣账号密码登录，支持腾讯拼图验证码和手动验证。

mod slider;

use super::common::{click_runtime_element, type_runtime_input};
use crate::cdp::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

const ACCOUNT_TAB: &str = ".account-tab-account";
const USERNAME: &str = "input[name='username']";
const PASSWORD: &str = "input[type='password'][name='password']";
const SUBMIT: &str = ".btn-account";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageState {
    host: String,
    ready: bool,
    account_page: bool,
    logged_in: bool,
    password_tab: bool,
    password_form: bool,
    captcha: bool,
    additional_verification: bool,
    form_error: bool,
}

impl CdpClient {
    pub(super) async fn login_douban(
        &self,
        tab_id: &str,
        site_url: &str,
        username: &str,
        password: &str,
        ocr_url: Option<&str>,
        progress: Option<&CdpProgress>,
    ) -> Result<bool, String> {
        let websocket_url = self
            .websocket_url_for_tab(tab_id)?
            .ok_or_else(|| "无法连接豆瓣标签页".to_string())?;
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let page = wait_for_page(&mut websocket, false, progress).await?;
        if page.logged_in {
            return Ok(false);
        }
        if !page.account_page {
            let mut login_url = reqwest::Url::parse("https://accounts.douban.com/passport/login")
                .map_err(|_| "豆瓣登录地址无效".to_string())?;
            login_url.query_pairs_mut().append_pair("redir", site_url);
            let result = websocket.call("Page.navigate", json!({ "url": login_url.as_str() }))?;
            if result.pointer("/result/errorText").is_some() {
                return Err("无法打开豆瓣登录页，请检查网络".to_string());
            }
        }
        let page = wait_for_page(&mut websocket, true, progress).await?;
        if page.logged_in {
            return Ok(false);
        }
        if !page.password_form && !click_runtime_element(&mut websocket, ACCOUNT_TAB) {
            return Err("找不到豆瓣的密码登录选项".to_string());
        }
        check_cancel(progress)?;
        if !type_runtime_input(&mut websocket, USERNAME, username.trim(), 45, 105).await {
            return Err("找不到豆瓣的手机号或邮箱输入框".to_string());
        }
        check_cancel(progress)?;
        if !page_state(&mut websocket)?.is_some_and(|page| page.account_page && page.password_form)
        {
            return Err("豆瓣登录表单发生变化，已停止填写密码".to_string());
        }
        if !type_runtime_input(&mut websocket, PASSWORD, password, 45, 105).await {
            return Err("找不到豆瓣的密码输入框".to_string());
        }
        check_cancel(progress)?;
        if !click_runtime_element(&mut websocket, SUBMIT) {
            return Err("找不到豆瓣的登录按钮".to_string());
        }

        let mut deadline = Instant::now() + Duration::from_secs(25);
        let mut verification_started = None;
        let mut attempted_slide = false;
        let mut manual = false;
        while Instant::now() < deadline {
            check_cancel(progress)?;
            if let Some(page) = page_state(&mut websocket)? {
                if page.logged_in {
                    return Ok(true);
                }
                if !manual && (page.captcha || page.additional_verification) {
                    if verification_started.is_none() {
                        let _ = websocket.call("Page.bringToFront", json!({}));
                    }
                    let started = *verification_started.get_or_insert_with(Instant::now);
                    if let Some(url) = ocr_url
                        .filter(|url| !attempted_slide && page.captcha && !url.trim().is_empty())
                    {
                        if let Some(challenge) = slider::capture(&mut websocket) {
                            attempted_slide = true;
                            if let Some(p) = progress {
                                p.info("豆瓣检测到拼图滑块，正在用 OCR 匹配缺口").await;
                            }
                            if let Err(error) =
                                slider::solve(&mut websocket, challenge, url, progress).await
                            {
                                check_cancel(progress)?;
                                if let Some(p) = progress {
                                    p.info(format!("豆瓣滑块识别未完成：{error}")).await;
                                }
                            } else {
                                deadline = Instant::now() + Duration::from_secs(20);
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                continue;
                            }
                        } else if started.elapsed() < Duration::from_secs(5) {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            continue;
                        } else if let Some(p) = progress {
                            p.info("豆瓣滑块图片未能读取，可能是验证组件变化或图片加载失败")
                                .await;
                        }
                    }
                    manual = true;
                    deadline = Instant::now() + Duration::from_secs(60);
                    let _ = websocket.call("Page.bringToFront", json!({}));
                    if let Some(p) = progress {
                        p.info("请在专用浏览器中完成豆瓣验证，程序会等待 60 秒")
                            .await;
                    }
                }
                if page.form_error && !page.captcha && !page.additional_verification {
                    return Err("豆瓣拒绝了本次登录，请检查账号密码或浏览器中的提示".to_string());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if verification_started.is_some() {
            Err("豆瓣验证后未确认登录，请在专用浏览器中完成验证后重试".to_string())
        } else {
            Err("豆瓣登录后未检测到已登录状态，请检查浏览器中的提示".to_string())
        }
    }
}

async fn wait_for_page(
    websocket: &mut CdpWebSocket,
    require_account_page: bool,
    progress: Option<&CdpProgress>,
) -> Result<PageState, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        check_cancel(progress)?;
        if let Some(page) = page_state(websocket)? {
            if page.ready
                && (page.logged_in
                    || page.password_tab
                    || page.password_form
                    || (!page.account_page && !require_account_page))
            {
                return Ok(page);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err("等待豆瓣登录页面加载超时".to_string())
}

fn page_state(websocket: &mut CdpWebSocket) -> Result<Option<PageState>, String> {
    let Ok(response) = websocket.call(
        "Runtime.evaluate",
        json!({
            "expression": PAGE_STATE,
            "returnByValue": true,
        }),
    ) else {
        return Ok(None);
    };
    let Some(page) = response
        .pointer("/result/result/value")
        .and_then(|value| serde_json::from_value::<PageState>(value.clone()).ok())
    else {
        return Ok(None);
    };
    if page.host.is_empty() {
        return Ok(None);
    }
    if page.host == "chromewebdata" {
        return Err("豆瓣页面未加载，请检查浏览器网络".to_string());
    }
    if page.host != "douban.com" && !page.host.ends_with(".douban.com") {
        return Err("豆瓣登录页跳转到了其他域名，已停止自动登录".to_string());
    }
    Ok(Some(page))
}

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress.is_some_and(CdpProgress::is_cancelled) {
        Err(CDP_CANCELLED.to_string())
    } else {
        Ok(())
    }
}

const PAGE_STATE: &str = r#"(() => {
    const visible = e => Boolean(e?.getClientRects().length && getComputedStyle(e).visibility === 'visible');
    const accountPage = location.origin === 'https://accounts.douban.com';
    const password = document.querySelector("input[type='password'][name='password']");
    const captcha = Array.from(document.querySelectorAll(
        '#tcaptcha_transform_dy, #tcaptcha_iframe, #tcaptcha_iframe_dy, iframe[src*=".captcha.qq.com"], iframe[src*=".captcha.qcloud.com"], iframe[src*=".captcha.gtimg.com"]'
    )).some(visible);
    return {
        host: location.hostname,
        ready: document.readyState !== 'loading',
        accountPage,
        loggedIn: Boolean(document.querySelector('#db-global-nav a[href*="/accounts/logout"]')),
        passwordTab: accountPage && visible(document.querySelector('.account-tab-account')),
        passwordForm: accountPage && visible(password) && visible(document.querySelector("input[name='username']")),
        captcha: accountPage && captcha,
        additionalVerification: accountPage && !visible(password) && (
            /\/(?:verify|challenge|security)(?:\/|$)/.test(location.pathname) ||
            Array.from(document.querySelectorAll('input[autocomplete="one-time-code"], input[name="code"]')).some(visible)
        ),
        formError: accountPage && Array.from(document.querySelectorAll('.account-form-error'))
            .some(element => visible(element) && element.textContent.trim()),
    };
})()"#;
