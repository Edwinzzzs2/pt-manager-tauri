//! 直接读取 macOS 原始 argv，保留路径内的空格及引号。
//! ps 的 command 列已将参数边界转换为空格，不能用来还原 Profile 路径。

use std::mem::size_of;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;

const CTL_KERN: c_int = 1;
const KERN_ARGMAX: c_int = 8;
const KERN_PROCARGS2: c_int = 49;
const INVALID_ARGUMENTS: &str = "无法解析浏览器原始进程参数";

extern "C" {
    fn sysctl(
        name: *mut c_int,
        namelen: c_uint,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
}

pub(super) fn process_arguments(process_id: u32) -> Result<Vec<String>, String> {
    let process_id = c_int::try_from(process_id)
        .map_err(|_| "浏览器进程编号无效".to_string())?;
    let mut argmax: c_int = 0;
    let mut size = size_of::<c_int>();
    let mut name = [CTL_KERN, KERN_ARGMAX];
    // SAFETY: name、argmax 和 size 都是有效且大小正确的可写缓冲区；不写入系统参数。
    let result = unsafe {
        sysctl(
            name.as_mut_ptr(),
            name.len() as c_uint,
            (&mut argmax as *mut c_int).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != size_of::<c_int>() || !(1..=16 * 1024 * 1024).contains(&argmax) {
        return Err("无法读取系统进程参数长度".to_string());
    }

    let mut buffer = vec![0u8; argmax as usize];
    size = buffer.len();
    let mut name = [CTL_KERN, KERN_PROCARGS2, process_id];
    // SAFETY: buffer 在调用期间保持有效；size 指定其容量，内核返回实际写入长度。
    let result = unsafe {
        sysctl(
            name.as_mut_ptr(),
            name.len() as c_uint,
            buffer.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size > buffer.len() {
        return Err("无法读取浏览器原始进程参数，请确认专用浏览器仍在运行".to_string());
    }
    buffer.truncate(size);
    parse_arguments(&buffer)
}

fn parse_arguments(buffer: &[u8]) -> Result<Vec<String>, String> {
    let argc = buffer
        .get(..size_of::<c_int>())
        .and_then(|bytes| bytes.try_into().ok())
        .map(c_int::from_ne_bytes)
        .filter(|argc| *argc > 0 && (*argc as usize) <= buffer.len())
        .ok_or_else(|| INVALID_ARGUMENTS.to_string())? as usize;
    let mut rest = &buffer[size_of::<c_int>()..];
    // KERN_PROCARGS2: argc、可执行文件路径、NUL 填充、argc 个 NUL 结尾的参数。
    let executable_end = rest.iter().position(|byte| *byte == 0)
        .ok_or_else(|| INVALID_ARGUMENTS.to_string())?;
    rest = &rest[executable_end + 1..];
    let argument_start = rest.iter().position(|byte| *byte != 0)
        .ok_or_else(|| INVALID_ARGUMENTS.to_string())?;
    rest = &rest[argument_start..];

    let mut arguments = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = rest.iter().position(|byte| *byte == 0)
            .ok_or_else(|| INVALID_ARGUMENTS.to_string())?;
        let argument = std::str::from_utf8(&rest[..end])
            .map_err(|_| "浏览器进程参数编码无效".to_string())?;
        arguments.push(argument.to_string());
        rest = &rest[end + 1..];
    }
    // argc 之后是环境变量；不能将其作为浏览器启动参数识别或输出。
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::parse_arguments;

    fn procargs(arguments: &[&str], environment: &[&str]) -> Vec<u8> {
        let mut buffer = (arguments.len() as i32).to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\0\0\0");
        for value in arguments.iter().chain(environment) {
            buffer.extend_from_slice(value.as_bytes());
            buffer.push(0);
        }
        buffer
    }

    #[test]
    fn preserves_spaces_quotes_and_empty_arguments() {
        let arguments = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "--user-data-dir=/Users/测试 user/Library/Application Support/Chrome \"Profile\"",
            "",
            "--proxy-server=http://127.0.0.1:1234",
        ];
        assert_eq!(parse_arguments(&procargs(&arguments, &[])).unwrap(), arguments);
    }

    #[test]
    fn ignores_environment_after_argv() {
        assert_eq!(
            parse_arguments(&procargs(&["chrome"], &["--user-data-dir=/tmp/not-argv"])).unwrap(),
            ["chrome"]
        );
    }

    #[test]
    fn rejects_missing_header_and_truncated_arguments() {
        assert!(parse_arguments(&[]).is_err());
        assert!(parse_arguments(&[0, 0, 0, 0]).is_err());
        let mut buffer = procargs(&["chrome", "--user-data-dir=/tmp/profile"], &[]);
        buffer.pop();
        assert!(parse_arguments(&buffer).is_err());
    }
}
