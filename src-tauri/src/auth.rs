use data_encoding::BASE32_NOPAD_NOCASE;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

pub fn current_totp(secret: &str) -> Result<String, String> {
    let normalized = secret
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '=')
        .collect::<String>();
    let key = BASE32_NOPAD_NOCASE
        .decode(normalized.as_bytes())
        .map_err(|_| "2FA 密钥不是有效的 Base32 格式".to_string())?;
    let unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时间无效，无法生成 2FA 验证码".to_string())?
        .as_secs();
    let counter = unix_seconds / 30;
    let mut mac = HmacSha1::new_from_slice(&key)
        .map_err(|_| "无法使用当前 2FA 密钥".to_string())?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let value = (((digest[offset] & 0x7f) as u32) << 24
        | (digest[offset + 1] as u32) << 16
        | (digest[offset + 2] as u32) << 8
        | digest[offset + 3] as u32)
        % 1_000_000;
    Ok(format!("{:06}", value))
}
