use super::{CdpClient, CdpProgress, CdpWebSocket, CDP_CANCELLED};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const SIGNIN_WAIT_STEPS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SigninStatus {
    Success,
    AlreadySigned,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct SigninResult {
    pub status: SigninStatus,
    pub message: String,
    pub reward: Option<String>,
    pub total_days: Option<u32>,
    pub consecutive_days: Option<u32>,
}

impl SigninResult {
    pub fn successful(&self) -> bool {
        matches!(
            self.status,
            SigninStatus::Success | SigninStatus::AlreadySigned
        )
    }

    pub fn summary(&self) -> String {
        let mut parts = vec![self.message.clone()];
        if let Some(days) = self.consecutive_days {
            parts.push(format!("连续签到 {days} 天"));
        }
        if let Some(total) = self.total_days {
            parts.push(format!("累计签到 {total} 次"));
        }
        if let Some(reward) = self.reward.as_deref().filter(|value| !value.is_empty()) {
            parts.push(format!("本次奖励 {reward}"));
        }
        parts.join("，")
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            status: SigninStatus::Failed,
            message: message.into(),
            reward: None,
            total_days: None,
            consecutive_days: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageState {
    ready: bool,
    current_url: String,
    entry_url: Option<String>,
    nexusphp_like: bool,
    home_already_signed: bool,
    success: bool,
    already_signed: bool,
    has_login_form: bool,
    has_challenge: bool,
    challenge_ready: bool,
    has_interactive_question: bool,
    has_action: bool,
    action_label: String,
    action_key: String,
    explicit_failure: bool,
    message: String,
    reward: Option<String>,
    total_days: Option<u32>,
    consecutive_days: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SigninAdapter {
    Audiences,
    Hdfans,
    PterClub,
    Yema,
    Hares,
    Rousi,
    Pting,
    Generic,
}

#[derive(Debug, Clone, Copy)]
enum ApiSigninKind {
    PterClub,
    Yema,
    Hares,
    Rousi,
    Pting,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiSigninState {
    success: bool,
    already_signed: bool,
    message: String,
    reward: Option<String>,
    total_days: Option<u32>,
    consecutive_days: Option<u32>,
}

impl SigninAdapter {
    fn from_url(url: &str) -> Self {
        if url.to_ascii_lowercase().contains("audiences.me") {
            Self::Audiences
        } else if url.to_ascii_lowercase().contains("hdfans.org") {
            Self::Hdfans
        } else if url.to_ascii_lowercase().contains("pterclub.") {
            Self::PterClub
        } else if url.to_ascii_lowercase().contains("yemapt.org") {
            Self::Yema
        } else if url.to_ascii_lowercase().contains("club.hares.top") {
            Self::Hares
        } else if url.to_ascii_lowercase().contains("rousi.pro") {
            Self::Rousi
        } else if url.to_ascii_lowercase().contains("pting.club") {
            Self::Pting
        } else {
            Self::Generic
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Audiences => "观众站点适配",
            Self::Hdfans => "红豆饭站点适配",
            Self::PterClub => "PTerClub 站点适配",
            Self::Yema => "野马站点适配",
            Self::Hares => "白兔站点适配",
            Self::Rousi => "Rousi 站点适配",
            Self::Pting => "PTing 论坛适配",
            Self::Generic => "通用规则",
        }
    }

    fn api_fallback(self) -> Option<ApiSigninKind> {
        match self {
            Self::PterClub => Some(ApiSigninKind::PterClub),
            Self::Yema => Some(ApiSigninKind::Yema),
            Self::Hares => Some(ApiSigninKind::Hares),
            Self::Rousi => Some(ApiSigninKind::Rousi),
            Self::Pting => Some(ApiSigninKind::Pting),
            _ => None,
        }
    }

    fn fallback_url(self, site_url: &str) -> String {
        let cleaned = site_url
            .trim()
            .split(['?', '#'])
            .next()
            .unwrap_or(site_url)
            .trim_end_matches('/');
        if cleaned.to_ascii_lowercase().ends_with("attendance.php") {
            return cleaned.to_string();
        }

        if self == Self::Rousi {
            let authority_start = cleaned
                .find("://")
                .map(|index| index + 3)
                .unwrap_or(0);
            let origin_end = cleaned[authority_start..]
                .find('/')
                .map(|index| authority_start + index)
                .unwrap_or(cleaned.len());
            return format!("{}/account/economy", &cleaned[..origin_end]);
        }

        let authority_start = cleaned.find("://").map(|index| index + 3).unwrap_or(0);
        let path_start = cleaned[authority_start..]
            .find('/')
            .map(|index| authority_start + index);
        let base = match path_start {
            Some(_)
                if cleaned
                    .rsplit('/')
                    .next()
                    .is_some_and(|part| part.contains('.')) =>
            {
                cleaned
                    .rsplit_once('/')
                    .map(|(root, _)| root)
                    .unwrap_or(cleaned)
            }
            _ => cleaned,
        };
        format!("{base}/attendance.php")
    }
}

impl CdpClient {
    pub async fn signin_site(
        &self,
        tab_id: &str,
        site_name: &str,
        site_url: &str,
        progress: Option<&CdpProgress>,
    ) -> Result<SigninResult, String> {
        let adapter = SigninAdapter::from_url(site_url);
        if adapter == SigninAdapter::Pting {
            if let Some(progress) = progress {
                progress
                    .info(format!("{site_name} 开始自动签到（{}）", adapter.label()))
                    .await;
            }
            return match self.signin_site_by_api(tab_id, ApiSigninKind::Pting).await {
                Ok(result) => Ok(result),
                Err(err) => Ok(SigninResult::failure(err)),
            };
        }

        let page_result = self
            .signin_site_by_page(tab_id, site_name, site_url, progress)
            .await?;
        if page_result.successful() {
            return Ok(page_result);
        }

        // Rousi 新版签到由页面流程维护授权状态，不再请求已经废弃的旧签到接口。
        if adapter == SigninAdapter::Rousi {
            return Ok(page_result);
        }

        let Some(api_kind) = adapter.api_fallback() else {
            return Ok(page_result);
        };
        if let Some(progress) = progress {
            progress
                .info(format!(
                    "{site_name} 页面签到未确认，正在尝试{}接口兜底",
                    adapter.label()
                ))
                .await;
        }
        match self.signin_site_by_api(tab_id, api_kind).await {
            Ok(result) if result.successful() => Ok(result),
            Ok(result) => Ok(SigninResult::failure(format!(
                "{}；接口兜底失败：{}",
                page_result.message, result.message
            ))),
            Err(err) => Ok(SigninResult::failure(format!(
                "{}；接口兜底失败：{}",
                page_result.message, err
            ))),
        }
    }

    async fn signin_site_by_page(
        &self,
        tab_id: &str,
        site_name: &str,
        site_url: &str,
        progress: Option<&CdpProgress>,
    ) -> Result<SigninResult, String> {
        check_cancel(progress)?;
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Ok(SigninResult::failure("未找到站点标签页"));
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let adapter = SigninAdapter::from_url(site_url);

        let initial = inspect_page(&mut websocket)?;
        if initial.success {
            return Ok(result_from_state(SigninStatus::Success, initial));
        }
        // Rousi 首页只显示连续天数，进入经济页后才能同时读取新版累计签到统计。
        if initial.already_signed && adapter != SigninAdapter::Rousi {
            return Ok(result_from_state(SigninStatus::AlreadySigned, initial));
        }
        let initially_already_signed = initial.home_already_signed;

        let target_url = match initial.entry_url.filter(|url| same_site(url, site_url)) {
            Some(url) => url,
            None if initial.nexusphp_like || adapter == SigninAdapter::Rousi => {
                adapter.fallback_url(site_url)
            }
            None => {
                return Ok(SigninResult::failure(
                    "页面中未找到站内签到入口，且当前站点不是通用 NexusPHP 签到结构，需要添加站点专用适配",
                ));
            }
        };
        if let Some(progress) = progress {
            progress
                .info(format!(
                    "{site_name} 开始自动签到（{}）：{target_url}",
                    adapter.label()
                ))
                .await;
        }

        websocket.call("Page.enable", serde_json::json!({}))?;
        websocket.call("Page.navigate", serde_json::json!({ "url": target_url }))?;

        let mut challenge_logged = false;
        let mut clicked_action_key = String::new();
        let mut ready_steps = 0usize;
        for _ in 0..SIGNIN_WAIT_STEPS {
            check_cancel(progress)?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let state = match inspect_page(&mut websocket) {
                Ok(state) => state,
                Err(_) => continue,
            };
            if !state.ready {
                continue;
            }
            ready_steps += 1;

            if state.success {
                let status = if initially_already_signed {
                    SigninStatus::AlreadySigned
                } else {
                    SigninStatus::Success
                };
                return Ok(result_from_state(status, state));
            }
            if state.already_signed || state.home_already_signed {
                return Ok(result_from_state(SigninStatus::AlreadySigned, state));
            }
            if state.has_login_form {
                return Ok(SigninResult::failure(
                    "登录状态已失效，签到页跳转到了登录页面",
                ));
            }
            if state.explicit_failure {
                return Ok(SigninResult::failure(if state.message.is_empty() {
                    "站点返回签到失败".to_string()
                } else {
                    state.message
                }));
            }
            if state.has_challenge && !state.challenge_ready {
                if !challenge_logged {
                    challenge_logged = true;
                    if let Some(progress) = progress {
                        progress
                            .info(format!(
                                "{site_name} 签到页需要人机验证，正在等待浏览器正常完成"
                            ))
                            .await;
                    }
                }
                continue;
            }
            if state.has_interactive_question {
                return Ok(SigninResult::failure(
                    "签到页包含答题或选项，需要添加站点专用适配",
                ));
            }
            if ready_steps >= 3
                && state.has_action
                && state.action_key != clicked_action_key
            {
                // Rousi 新版需要先切到“签到”页签，再点击“立即签到”，按控件去重可兼容这种多阶段操作。
                let clicked = click_signin_action(&mut websocket);
                if clicked {
                    clicked_action_key = state.action_key.clone();
                    if let Some(progress) = progress {
                        progress
                            .info(format!(
                                "{site_name} 已点击签到操作：{}",
                                state.action_label
                            ))
                            .await;
                    }
                    ready_steps = 0;
                    continue;
                }
            }
            if ready_steps >= 16 && !state.has_challenge {
                return Ok(SigninResult::failure(format!(
                    "未识别到签到结果（当前页面：{}），需要添加站点专用适配",
                    state.current_url
                )));
            }
        }

        Ok(SigninResult::failure(if challenge_logged {
            "等待人机验证或签到结果超时"
        } else {
            "等待签到结果超时"
        }))
    }

    async fn signin_site_by_api(
        &self,
        tab_id: &str,
        kind: ApiSigninKind,
    ) -> Result<SigninResult, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("未找到站点标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(15))?;
        let (path, kind_name) = match kind {
            ApiSigninKind::PterClub => ("/attendance-ajax.php", "pterclub"),
            ApiSigninKind::Yema => ("/api/consumer/checkIn", "yema"),
            ApiSigninKind::Hares => ("/attendance.php?action=sign", "hares"),
            ApiSigninKind::Rousi => ("/api/points/attendance", "rousi"),
            ApiSigninKind::Pting => ("/api/check-in", "pting"),
        };
        let config = serde_json::to_string(&serde_json::json!({
            "path": path,
            "kind": kind_name
        }))
        .map_err(|err| err.to_string())?;
        let expression = API_SIGNIN_EXPRESSION.replace("__CONFIG__", &config);
        let response = websocket.call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true
            }),
        )?;
        let value = response
            .get("result")
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("value"))
            .cloned()
            .ok_or_else(|| "签到接口未返回可解析结果".to_string())?;
        let state: ApiSigninState =
            serde_json::from_value(value).map_err(|err| format!("解析签到接口结果失败：{err}"))?;
        let status = if state.success {
            SigninStatus::Success
        } else if state.already_signed {
            SigninStatus::AlreadySigned
        } else {
            SigninStatus::Failed
        };
        Ok(SigninResult {
            status,
            message: if state.message.is_empty() {
                match status {
                    SigninStatus::Success => "接口签到成功".to_string(),
                    SigninStatus::AlreadySigned => "今日已签到".to_string(),
                    SigninStatus::Failed => "签到接口返回失败".to_string(),
                }
            } else {
                state.message
            },
            reward: state.reward,
            total_days: state.total_days,
            consecutive_days: state.consecutive_days,
        })
    }
}

fn result_from_state(status: SigninStatus, state: PageState) -> SigninResult {
    let default_message = match status {
        SigninStatus::Success => "签到成功",
        SigninStatus::AlreadySigned => "今日已签到",
        SigninStatus::Failed => "签到失败",
    };
    SigninResult {
        status,
        message: if state.message.is_empty() {
            default_message.to_string()
        } else {
            state.message
        },
        reward: state.reward,
        total_days: state.total_days,
        consecutive_days: state.consecutive_days,
    }
}

fn same_site(candidate: &str, site_url: &str) -> bool {
    host_from_url(candidate)
        .zip(host_from_url(site_url))
        .is_some_and(|(left, right)| left == right)
}

fn host_from_url(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    Some(
        rest.split(['/', '?', '#'])
            .next()?
            .split(':')
            .next()?
            .to_ascii_lowercase(),
    )
}

fn inspect_page(websocket: &mut CdpWebSocket) -> Result<PageState, String> {
    let response = websocket.call(
        "Runtime.evaluate",
        serde_json::json!({
            "expression": SIGNIN_STATE_EXPRESSION,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true
        }),
    )?;
    let value = response
        .get("result")
        .and_then(|value| value.get("result"))
        .and_then(|value| value.get("value"))
        .cloned()
        .ok_or_else(|| "读取签到页面状态失败".to_string())?;
    serde_json::from_value(value).map_err(|err| format!("解析签到页面状态失败：{err}"))
}

fn click_signin_action(websocket: &mut CdpWebSocket) -> bool {
    websocket
        .call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": r#"(() => {
                    const element = document.querySelector('[data-pt-manager-signin-action="true"]');
                    if (!element) return false;
                    element.click();
                    return true;
                })()"#,
                "returnByValue": true,
                "userGesture": true
            }),
        )
        .ok()
        .and_then(|value| {
            value
                .get("result")?
                .get("result")?
                .get("value")?
                .as_bool()
        })
        .unwrap_or(false)
}

fn check_cancel(progress: Option<&CdpProgress>) -> Result<(), String> {
    if progress.is_some_and(CdpProgress::is_cancelled) {
        Err(CDP_CANCELLED.to_string())
    } else {
        Ok(())
    }
}

const SIGNIN_STATE_EXPRESSION: &str = r#"(() => {
    const visible = (el) => {
        if (!el) return false;
        const style = getComputedStyle(el);
        const rect = el.getBoundingClientRect();
        return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
    };
    const clean = (value) => String(value || '').replace(/\s+/g, ' ').trim();
    const bodyText = clean(document.body?.innerText || '');
    const elements = Array.from(document.querySelectorAll('a, button, input[type="submit"], input[type="button"]'));
    const textOf = (el) => clean(
        el.innerText
        || el.value
        || el.getAttribute('aria-label')
        || el.title
        || el.querySelector?.('img[alt]')?.alt
        || ''
    );
    const signedPattern = /(今日|今天|本日)?已[签到簽到]|签到已得|簽到已得/i;
    const successPattern = /(签到|簽到|打卡)(成功|完成)|签到已得|簽到已得/i;
    const failurePattern = /(签到|簽到|打卡).{0,8}(失败|失敗|错误|錯誤)/i;
    const entryPattern = /(签到|簽到|打卡|check[ -]?in|attendance|daily.?sign)/i;
    const exactActionPattern = /^(立即)?(签到|簽到|打卡|check[ -]?in|attendance)$/i;

    const links = elements.filter((el) => el.tagName === 'A' && visible(el)).map((el) => {
        const text = textOf(el);
        const href = el.href || '';
        let score = 0;
        if (/attendance\.php/i.test(href)) score += 20;
        if (entryPattern.test(text)) score += 10;
        if (/(attendance|check.?in|daily.?sign)/i.test(href)) score += 6;
        if (exactActionPattern.test(text)) score += 4;
        if (signedPattern.test(text)) score += 2;
        return { el, text, href, score };
    }).filter((item) => item.score > 0).sort((a, b) => b.score - a.score);

    const homeSigned = elements.some((el) => visible(el) && signedPattern.test(textOf(el)));
    const actionCandidates = elements.map((el, index) => ({ el, index, text: textOf(el) })).filter(({ el, text }) => {
        if (!visible(el) || el.tagName === 'A') return false;
        const selectedNavigation = el.getAttribute('aria-pressed') === 'true'
            || el.getAttribute('aria-selected') === 'true'
            || el.dataset?.state === 'active';
        return exactActionPattern.test(text)
            && !/(登录|登入|login)/i.test(text)
            && !selectedNavigation;
    }).sort((left, right) => Number(/^立即/i.test(right.text)) - Number(/^立即/i.test(left.text)));
    const action = actionCandidates[0]?.el || null;
    const actionKey = actionCandidates[0]
        ? `${actionCandidates[0].text}|${actionCandidates[0].index}`
        : '';
    document.querySelectorAll('[data-pt-manager-signin-action]').forEach((el) => el.removeAttribute('data-pt-manager-signin-action'));
    if (action) action.setAttribute('data-pt-manager-signin-action', 'true');

    const statValue = (labelPattern) => {
        const candidates = Array.from(document.querySelectorAll('span, div, td, th, p, li'));
        const label = candidates.find((el) => labelPattern.test(clean(el.textContent)) && clean(el.textContent).length < 40);
        if (!label) return null;
        const labelText = clean(label.textContent);
        const inlineMatch = labelText.match(/[+\-]?\d[\d,.]*/);
        if (inlineMatch) return inlineMatch[0];
        const parent = label.parentElement;
        const preferred = parent?.querySelector('.attendance-stat__num, .stat-value, .value, strong, b');
        const source = clean(preferred?.textContent || parent?.textContent || label.textContent);
        const match = source.match(/[+\-]?\d[\d,.]*/);
        return match ? match[0] : null;
    };
    const numberStat = (pattern) => {
        const value = statValue(pattern);
        if (!value) return null;
        const parsed = Number(value.replace(/[^\d]/g, ''));
        return Number.isFinite(parsed) ? parsed : null;
    };
    const textNumber = (pattern) => {
        const match = bodyText.match(pattern);
        if (!match?.[1]) return null;
        const parsed = Number(match[1].replace(/[^\d]/g, ''));
        return Number.isFinite(parsed) ? parsed : null;
    };
    const textReward = () => {
        const match = bodyText.match(/本次(?:签到|簽到)?.{0,8}(?:获得|獲得|奖励|獎勵)\s*([+\-]?\d[\d,.]*)\s*([^，。；\s]{0,8})?/i);
        if (!match?.[1]) return null;
        return clean([match[1], match[2]].filter(Boolean).join(' '));
    };

    const challengeResponse = document.querySelector('input[name="cf-turnstile-response"], input[name="g-recaptcha-response"], textarea[name="g-recaptcha-response"]');
    const hasChallenge = Boolean(challengeResponse)
        || Boolean(document.querySelector('iframe[src*="challenges.cloudflare.com"], iframe[src*="recaptcha"], .cf-turnstile, .g-recaptcha'));
    const challengeReady = !hasChallenge || clean(challengeResponse?.value).length > 10;
    const hasLoginForm = Boolean(document.querySelector('input[type="password"]'))
        && Boolean(document.querySelector('form[action*="login" i], input[name="username" i], input[name="uid" i]'));
    const nexusphpLike = Boolean(document.querySelector(
        'a[href*="torrents.php"], a[href*="userdetails.php"], a[href*="attendance.php"], a[href*="logout.php"]'
    ));
    const hasInteractiveQuestion = Boolean(document.querySelector('input[type="radio"], select[name*="answer" i], input[name*="answer" i]'));
    const completedAttendancePattern = /第\s*\d+\s*次(?:签到|簽到).{0,80}(?:已)?(?:连续|連續)(?:签到|簽到)\s*\d+\s*(?:天|日)/i;
    const success = successPattern.test(bodyText) || completedAttendancePattern.test(bodyText);
    const alreadySigned = !success && signedPattern.test(bodyText);
    const explicitFailure = failurePattern.test(bodyText);
    const resultMessage = Array.from(document.querySelectorAll('h1, h2, h3, .message, .alert, .attendance-card__title'))
        .map((el) => clean(el.textContent))
        .find((text) => text && text.length < 120
            && (successPattern.test(text) || signedPattern.test(text) || failurePattern.test(text))) || '';

    return {
        ready: document.readyState === 'interactive' || document.readyState === 'complete',
        currentUrl: location.href,
        entryUrl: links[0]?.href || null,
        nexusphpLike,
        homeAlreadySigned: homeSigned && !/attendance\.php/i.test(location.pathname),
        success,
        alreadySigned,
        hasLoginForm,
        hasChallenge,
        challengeReady,
        hasInteractiveQuestion,
        hasAction: Boolean(action),
        actionLabel: action ? textOf(action) : '',
        actionKey,
        explicitFailure,
        message: resultMessage,
        reward: textReward() || statValue(/本次.{0,8}(获得|獲得|奖励|獎勵|魔力|爆米花)/i),
        totalDays: textNumber(/第\s*(\d+)\s*次(?:签到|簽到)/i)
            ?? textNumber(/(?:累计|累計)\s*(\d+)\s*(?:天|日|次)/i)
            ?? numberStat(/(累计|累計).{0,8}(签到|簽到).{0,4}(次数|次數)?/i),
        consecutiveDays: textNumber(/(?:已)?(?:连续|連續)(?:签到|簽到)?\s*(\d+)\s*(?:天|日)/i)
            ?? numberStat(/(连续|連續).{0,8}(签到|簽到).{0,4}(天数|天數|日数|日數)?/i)
    };
})()"#;

const API_SIGNIN_EXPRESSION: &str = r#"(async () => {
    const config = __CONFIG__;
    const clean = (value) => String(value || '').replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim();
    const numberFrom = (text, pattern) => {
        const match = text.match(pattern);
        if (!match?.[1]) return null;
        const value = Number(match[1].replace(/[^\d]/g, ''));
        return Number.isFinite(value) ? value : null;
    };
    try {
        const headers = { Accept: 'application/json, text/plain, */*' };
        const request = {
            method: ['rousi', 'pting'].includes(config.kind) ? 'POST' : 'GET',
            credentials: 'include',
            headers
        };
        if (config.kind === 'rousi') {
            const token = localStorage.getItem('token');
            if (!token) {
                return {
                    success: false,
                    alreadySigned: false,
                    message: '浏览器登录令牌不存在或已失效',
                    reward: null,
                    totalDays: null,
                    consecutiveDays: null
                };
            }
            headers.Authorization = token.startsWith('Bearer ') ? token : `Bearer ${token}`;
            headers['Content-Type'] = 'application/json';
            request.body = JSON.stringify({ mode: 'fixed' });
        }
        if (config.kind === 'pting') {
            headers['Content-Type'] = 'application/json';
            request.body = JSON.stringify({ action: 'check-in' });
        }
        const response = await fetch(new URL(config.path, location.origin), request);
        const raw = await response.text();
        let payload = {};
        try { payload = JSON.parse(raw); } catch (_) {}
        const data = payload?.data && typeof payload.data === 'object' ? payload.data : payload;
        const text = clean([
            payload.message,
            payload.msg,
            payload.data,
            payload.errorMessage,
            raw
        ].filter(Boolean).join(' '));
        let already = /(今日|今天).{0,8}已.{0,4}(签到|簽到)|已经.{0,4}(签到|簽到)|重[复複].{0,4}(签到|簽到)/i.test(text);
        let success = false;
        if (config.kind === 'pterclub') {
            success = String(payload.status) === '1';
            already ||= String(payload.status) === '0';
        }
        if (config.kind === 'yema') success = payload.success === true;
        if (config.kind === 'hares') {
            success = Number(payload.code) === 0;
            already ||= Number(payload.code) === 1;
        }
        if (config.kind === 'rousi') {
            success = response.status === 200 && Number(payload.code) === 0;
            already ||= response.status === 400 && Number(payload.code) === 1;
        }
        if (config.kind === 'pting') {
            success = response.ok && !already;
        }
        let stats = {};
        if (config.kind === 'rousi' && (success || already)) {
            try {
                const statsResponse = await fetch(new URL('/api/points/attendance/stats', location.origin), {
                    method: 'GET',
                    credentials: 'include',
                    headers
                });
                const statsPayload = await statsResponse.json();
                stats = statsPayload?.data && typeof statsPayload.data === 'object'
                    ? statsPayload.data
                    : statsPayload;
            } catch (_) {}
        }
        const rewardMatch = text.match(/本次(?:签到|簽到)?.{0,8}(?:获得|獲得|奖励|獎勵)\s*([+\-]?\d[\d,.]*)\s*([^，。；\s]{0,8})?/i);
        const rousiBonus = data?.bonus === null || data?.bonus === undefined ? NaN : Number(data.bonus);
        const rousiStreakBonus = data?.streak_bonus === null || data?.streak_bonus === undefined
            ? NaN
            : Number(data.streak_bonus);
        const rousiReward = config.kind === 'rousi' && Number.isFinite(rousiBonus)
            ? `${rousiBonus}${Number.isFinite(rousiStreakBonus) && rousiStreakBonus > 0 ? `（含连续奖励 ${rousiStreakBonus}）` : ''} 魔力值`
            : null;
        const ptingRewardMatch = config.kind === 'pting'
            ? text.match(/(?:获得|奖励|增加)\s*([+\-]?\d[\d,.]*)\s*(积分|花粉|金币)?/i)
            : null;
        const firstNumber = (...values) => {
            for (const value of values) {
                if (value === null || value === undefined || value === '') continue;
                const number = Number(value);
                if (Number.isFinite(number) && number >= 0) return Math.trunc(number);
            }
            return null;
        };
        return {
            success,
            alreadySigned: !success && already,
            message: success
                ? (config.kind === 'pting'
                    ? (clean(payload.message || payload.msg) || '签到成功')
                    : '接口签到成功')
                : (already ? '今日已签到' : (response.status === 401
                    ? (config.kind === 'pting' ? '登录状态已失效' : 'Authorization 已失效')
                    : (text || `HTTP ${response.status}`))),
            reward: rousiReward || (ptingRewardMatch?.[1]
                ? [ptingRewardMatch[1], ptingRewardMatch[2]].filter(Boolean).join(' ')
                : null) || (rewardMatch?.[1]
                ? [rewardMatch[1], rewardMatch[2]].filter(Boolean).join(' ')
                : null),
            totalDays: firstNumber(
                stats?.total_days,
                stats?.total_attendance,
                stats?.attendance_count,
                stats?.total_count,
                numberFrom(text, /第\s*(\d+)\s*次(?:签到|簽到)/i)
            ),
            consecutiveDays: firstNumber(
                data?.current_streak,
                stats?.current_streak,
                numberFrom(text, /(?:已)?(?:连续|連續)(?:签到|簽到)\s*(\d+)\s*(?:天|日)/i)
            )
        };
    } catch (error) {
        return {
            success: false,
            alreadySigned: false,
            message: `接口请求失败：${error?.message || error}`,
            reward: null,
            totalDays: null,
            consecutiveDays: null
        };
    }
})()"#;

#[cfg(test)]
mod tests {
    use super::{ApiSigninKind, SigninAdapter};

    #[test]
    fn selects_rousi_page_adapter() {
        let adapter = SigninAdapter::from_url("https://rousi.pro/");
        assert_eq!(adapter, SigninAdapter::Rousi);
        assert_eq!(
            adapter.fallback_url("https://rousi.pro/"),
            "https://rousi.pro/account/economy"
        );
    }

    #[test]
    fn selects_pting_forum_api() {
        let adapter = SigninAdapter::from_url("https://pting.club/");
        assert_eq!(adapter, SigninAdapter::Pting);
        assert!(matches!(adapter.api_fallback(), Some(ApiSigninKind::Pting)));
    }

    #[test]
    fn fallback_url_keeps_the_site_origin() {
        assert_eq!(
            SigninAdapter::Generic.fallback_url("https://monikadesign.uk/"),
            "https://monikadesign.uk/attendance.php"
        );
        assert_eq!(
            SigninAdapter::Generic.fallback_url("https://hdfans.org/index.php"),
            "https://hdfans.org/attendance.php"
        );
        assert_eq!(
            SigninAdapter::Generic.fallback_url("https://example.com/tracker/index.php"),
            "https://example.com/tracker/attendance.php"
        );
    }
}
