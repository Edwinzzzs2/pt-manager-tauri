use std::io::{Read, Write};
use std::net::IpAddr;

pub(super) fn connect(
    stream: &mut (impl Read + Write),
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<(), String> {
    // RFC 1928: only offer the method selected by the user's configuration.
    let method = if username.is_empty() { 0 } else { 2 };
    stream.write_all(&[5, 1, method]).map_err(io_error)?;
    let mut selection = [0; 2];
    stream.read_exact(&mut selection).map_err(io_error)?;
    if selection != [5, method] {
        return Err("SOCKS5 代理不支持当前认证方式，请检查用户名和密码设置".to_string());
    }
    if method == 2 {
        authenticate(stream, username, password)?;
    }

    let mut request = vec![5, 1, 0];
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend(ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend(ip.octets());
        }
        Err(_) => {
            let len = u8::try_from(host.len()).map_err(|_| "SOCKS5 目标域名过长".to_string())?;
            request.extend([3, len]);
            request.extend(host.as_bytes());
        }
    }
    request.extend(port.to_be_bytes());
    stream.write_all(&request).map_err(io_error)?;

    let mut reply = [0; 4];
    stream.read_exact(&mut reply).map_err(io_error)?;
    if reply[0] != 5 || reply[2] != 0 {
        return Err("SOCKS5 代理返回了无效的连接响应".to_string());
    }
    if reply[1] != 0 {
        let reason = match reply[1] {
            2 => "代理规则拒绝连接",
            3 => "目标网络不可达",
            4 => "目标主机不可达",
            5 => "目标拒绝连接",
            6 => "连接超时",
            _ => "代理无法建立连接",
        };
        return Err(format!("SOCKS5 {reason}（状态码 {}）", reply[1]));
    }

    // Consume just the bound address and port. The next bytes belong to the target.
    let address_len = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0];
            stream.read_exact(&mut len).map_err(io_error)?;
            usize::from(len[0])
        }
        _ => return Err("SOCKS5 代理返回了未知的地址类型".to_string()),
    };
    stream
        .read_exact(&mut [0; 257][..address_len + 2])
        .map_err(io_error)
}

fn authenticate(
    stream: &mut (impl Read + Write),
    username: &str,
    password: &str,
) -> Result<(), String> {
    // RFC 1929 uses one-byte lengths and requires both fields.
    let user_len = u8::try_from(username.len()).map_err(|_| "SOCKS5 用户名过长".to_string())?;
    let password_len = u8::try_from(password.len()).map_err(|_| "SOCKS5 密码过长".to_string())?;
    let mut request = vec![1, user_len];
    request.extend(username.as_bytes());
    request.push(password_len);
    request.extend(password.as_bytes());
    stream.write_all(&request).map_err(io_error)?;
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).map_err(io_error)?;
    if reply != [1, 0] {
        return Err("SOCKS5 代理认证失败，请检查用户名和密码".to_string());
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> String {
    format!("SOCKS5 握手失败：{error}")
}
