mod common;
mod hdkylin;
mod mteam;
mod nexusphp;

use crate::auth;
use crate::cdp::{CdpClient, CdpProgress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteAdapter {
    MTeam,
    Hdkylin,
    NexusPhp,
}

impl SiteAdapter {
    /// 特殊站点优先按域名匹配，其余站点统一走 NexusPHP 兼容流程。
    pub fn from_url(url: &str) -> Self {
        let normalized = url.to_ascii_lowercase();
        if normalized.contains("kp.m-team.cc") {
            Self::MTeam
        } else if normalized.contains("hdkyl.in") {
            Self::Hdkylin
        } else {
            Self::NexusPhp
        }
    }
}

pub struct LoginRequest<'a> {
    pub site_name: &'a str,
    pub site_url: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub totp_secret: Option<&'a str>,
    pub min_remaining_attempts: u32,
    pub ocr_config: Option<(String, u8)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    LoggedIn,
    AlreadyLoggedIn,
}

pub struct LoginOutcome {
    pub state: LoginState,
    pub remaining_attempts: Option<u32>,
}

pub struct LoginError {
    pub message: String,
    pub remaining_attempts: Option<u32>,
}

impl CdpClient {
    /// 业务层只调用这一入口，站点识别、TOTP 处理和返回值归一化都由登录模块负责。
    pub async fn login_site(
        &self,
        tab_id: &str,
        request: LoginRequest<'_>,
        progress: Option<&CdpProgress>,
    ) -> Result<LoginOutcome, LoginError> {
        match SiteAdapter::from_url(request.site_url) {
            SiteAdapter::MTeam => {
                // M-Team 旧流程会在进入适配器前校验已配置的密钥，这里保持相同行为。
                let totp_code = request
                    .totp_secret
                    .map(auth::current_totp)
                    .transpose()
                    .map_err(|message| LoginError {
                        message,
                        remaining_attempts: None,
                    })?;
                self.login_mteam(
                    tab_id,
                    request.username,
                    request.password,
                    totp_code.as_deref(),
                )
                .await
                .map(|logged_in| LoginOutcome {
                    state: if logged_in {
                        LoginState::LoggedIn
                    } else {
                        LoginState::AlreadyLoggedIn
                    },
                    remaining_attempts: None,
                })
                .map_err(|message| LoginError {
                    message,
                    remaining_attempts: None,
                })
            }
            SiteAdapter::Hdkylin => self
                .login_hdkylin(
                    tab_id,
                    request.username,
                    request.password,
                    request.totp_secret,
                )
                .await
                .map(|logged_in| LoginOutcome {
                    state: if logged_in {
                        LoginState::LoggedIn
                    } else {
                        LoginState::AlreadyLoggedIn
                    },
                    remaining_attempts: None,
                })
                .map_err(|message| LoginError {
                    message,
                    remaining_attempts: None,
                }),
            SiteAdapter::NexusPhp => self
                .login_nexusphp(
                    tab_id,
                    request.username,
                    request.password,
                    request.totp_secret,
                    request.min_remaining_attempts,
                    request.ocr_config,
                    progress,
                    request.site_name,
                )
                .await
                .map(|(logged_in, remaining_attempts)| LoginOutcome {
                    state: if logged_in {
                        LoginState::LoggedIn
                    } else {
                        LoginState::AlreadyLoggedIn
                    },
                    remaining_attempts,
                })
                .map_err(|(message, remaining_attempts)| LoginError {
                    message,
                    remaining_attempts,
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SiteAdapter;

    #[test]
    fn selects_special_adapters_before_nexusphp_fallback() {
        assert_eq!(
            SiteAdapter::from_url("https://kp.m-team.cc/login"),
            SiteAdapter::MTeam
        );
        assert_eq!(
            SiteAdapter::from_url("https://hdkyl.in/login.php"),
            SiteAdapter::Hdkylin
        );
        assert_eq!(
            SiteAdapter::from_url("https://pt.example.com/login.php"),
            SiteAdapter::NexusPhp
        );
    }
}
