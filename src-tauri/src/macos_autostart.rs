//! macOS 13+ 直接注册主应用登录项，旧系统保留带应用关联的 LaunchAgent。
//! 不重置系统后台项目数据库，也不强行覆盖用户在系统设置中的禁用选择。

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use plist::Value;
use std::ffi::{CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;
use tauri_plugin_autostart::ManagerExt;

// 动态查找类：10.15–12 没有 SMAppService，不能静态引用该类符号。
#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

const NOT_REGISTERED: isize = 0;
const ENABLED: isize = 1;
const REQUIRES_APPROVAL: isize = 2;
const NOT_FOUND: isize = 3;
const LEGACY_NAMES: [&str; 2] = ["PT Manager", "pt-manager"];

fn native_service() -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"SMAppService")?;
    // SAFETY: mainAppService 返回当前应用的 SMAppService，objc2 管理返回值的引用计数。
    Some(unsafe { msg_send![class, mainAppService] })
}

fn status(service: &AnyObject) -> isize {
    // SAFETY: service 仅由 SMAppService.mainAppService 创建；status 返回 NSInteger。
    unsafe { msg_send![service, status] }
}

/// None 表示旧系统或尚未迁移的旧注册；false 包括系统禁用和等待授权。
pub fn native_enabled() -> Result<Option<bool>, String> {
    // 若迁移曾失败，不能用尚未注册的原生状态覆盖仍在工作的旧自启偏好。
    let directory = launch_agents_dir()?;
    if LEGACY_NAMES.iter().any(|name| directory.join(format!("{name}.plist")).exists()) {
        return Ok(None);
    }
    autoreleasepool(|_| Ok(native_service().map(|service| status(&service) == ENABLED)))
}

fn set_native(service: &AnyObject, enabled: bool) -> Result<(), String> {
    let current = status(service);
    if enabled && current == REQUIRES_APPROVAL {
        return Err("请在系统设置 → 通用 → 登录项中允许 PT Manager 开机启动".to_string());
    }
    let mut error: *mut AnyObject = ptr::null_mut();
    // SAFETY: 两个方法都接收 NSError** 并返回 Objective-C BOOL；error 的存储在调用期间有效。
    let result: Bool = unsafe {
        if enabled {
            if current == ENABLED { return Ok(()); }
            msg_send![service, registerAndReturnError: &mut error]
        } else {
            if matches!(current, NOT_REGISTERED | NOT_FOUND) { return Ok(()); }
            msg_send![service, unregisterAndReturnError: &mut error]
        }
    };
    if !result.as_bool() {
        let detail = if error.is_null() {
            "系统未提供错误详情".to_string()
        } else {
            // SAFETY: 失败返回的 NSError 在当前 autoreleasepool 内有效。
            unsafe {
                let description: *mut AnyObject = msg_send![error, localizedDescription];
                let text: *const std::os::raw::c_char = msg_send![description, UTF8String];
                if text.is_null() { "系统未提供错误详情".to_string() }
                else { CStr::from_ptr(text).to_string_lossy().into_owned() }
            }
        };
        return Err(format!("{}开机自启失败：{detail}", if enabled { "启用" } else { "关闭" }));
    }
    match (enabled, status(service)) {
        (true, ENABLED) | (false, NOT_REGISTERED | NOT_FOUND) => Ok(()),
        (true, REQUIRES_APPROVAL) => Err("请在系统设置 → 通用 → 登录项中允许 PT Manager 开机启动".to_string()),
        _ => Err("系统登录项状态尚未生效，请稍后重试".to_string()),
    }
}

fn bundled_app(executable: &Path) -> Option<&Path> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then_some(bundle)
}

fn bundle_identifier(executable: &Path) -> Option<String> {
    Value::from_file(bundled_app(executable)?.join("Contents/Info.plist"))
        .ok()?
        .as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(str::to_string)
}

fn launch_agents_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/LaunchAgents"))
        .ok_or_else(|| "无法确定用户登录项目录".to_string())
}

fn legacy_executable(value: &Value) -> Option<&str> {
    let dictionary = value.as_dictionary()?;
    let label = dictionary.get("Label")?.as_string()?;
    if !LEGACY_NAMES.contains(&label) { return None; }
    dictionary.get("Program").and_then(Value::as_string).or_else(|| {
        dictionary.get("ProgramArguments")?.as_array()?.first()?.as_string()
    })
}

/// 只检查历史版本实际使用的两个文件名，并核对它们引用的应用标识。
fn owned_legacy_entries(identifier: &str) -> Result<Vec<(PathBuf, Value)>, String> {
    let mut entries = Vec::new();
    for name in LEGACY_NAMES {
        let path = launch_agents_dir()?.join(format!("{name}.plist"));
        let value = match Value::from_file(&path) {
            Ok(value) => value,
            Err(_) if !path.exists() => continue,
            Err(error) => return Err(format!("读取旧自启配置失败：{error}")),
        };
        let owned = legacy_executable(&value)
            .and_then(|executable| bundle_identifier(Path::new(executable)))
            .as_deref() == Some(identifier);
        if !owned {
            return Err("发现无法确认归属的同名自启配置，已保留该文件".to_string());
        }
        entries.push((path, value));
    }
    Ok(entries)
}

fn remove_legacy(entries: &[(PathBuf, Value)]) -> Result<(), String> {
    for (path, _) in entries {
        match fs::remove_file(path) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("迁移旧自启配置失败：{error}")),
        }
    }
    // 不 bootout 当前任务，避免旧 LaunchAgent 恰好管理着正在运行的主进程而导致应用退出。
    Ok(())
}

fn legacy_allowed(path: &Path) -> Result<bool, String> {
    let path = CString::new(path.to_str().ok_or("旧自启配置路径编码无效")?)
        .map_err(|_| "旧自启配置路径无效".to_string())?;
    let string_class = AnyClass::get(c"NSString").ok_or("系统字符串服务不可用")?;
    let url_class = AnyClass::get(c"NSURL").ok_or("系统路径服务不可用")?;
    let service_class = AnyClass::get(c"SMAppService").ok_or("系统登录项服务不可用")?;
    // SAFETY: 路径是有效 NUL 结尾的本机 UTF-8 字符串，方法签名来自 Foundation/ServiceManagement。
    let legacy_status: isize = unsafe {
        let string: Retained<AnyObject> = msg_send![string_class, stringWithUTF8String: path.as_ptr()];
        let url: Retained<AnyObject> = msg_send![url_class, fileURLWithPath: &*string];
        msg_send![service_class, statusForLegacyURL: &*url]
    };
    Ok(legacy_status == ENABLED)
}

fn should_migrate(configured: bool, legacy_enabled: bool, native_status: isize) -> bool {
    configured && legacy_enabled && native_status == NOT_REGISTERED
}

fn prepare(app: &tauri::AppHandle) -> Result<Vec<(PathBuf, Value)>, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    if bundle_identifier(&executable).as_deref() != Some(app.config().identifier.as_str()) {
        return Err("请安装并从 PT Manager.app 启动后再设置开机自启".to_string());
    }
    owned_legacy_entries(&app.config().identifier)
}

pub fn apply(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    autoreleasepool(|_| {
        let entries = prepare(app)?;
        if let Some(service) = native_service() {
            set_native(&service, enabled)?;
            remove_legacy(&entries)
        } else {
            apply_legacy(app, enabled, &entries)
        }
    })
}

/// 启动时只迁移现存旧注册，其余情况以系统状态为准，避免重新开启用户已禁用的登录项。
pub fn reconcile(app: &tauri::AppHandle, configured: bool) -> Result<bool, String> {
    autoreleasepool(|_| {
        let entries = prepare(app)?;
        let Some(service) = native_service() else {
            apply_legacy(app, configured, &entries)?;
            return Ok(configured);
        };
        let legacy_enabled = entries.iter().any(|(_, value)| {
            let dictionary = value.as_dictionary().unwrap();
            dictionary.get("RunAtLoad").and_then(Value::as_boolean) == Some(true)
                && dictionary.get("Disabled").and_then(Value::as_boolean) != Some(true)
        });
        let mut allowed = false;
        for (path, _) in &entries {
            allowed |= legacy_allowed(path)?;
        }
        let current = status(&service);
        if configured && legacy_enabled && allowed && current == NOT_FOUND {
            return Err("系统尚无法识别主应用登录项，已保留旧自启配置，请确认应用已安装到应用程序目录".to_string());
        }
        if should_migrate(configured, legacy_enabled && allowed, current) {
            // 新注册成功后才删除旧文件；注册失败时保留旧自启能力。
            set_native(&service, true)?;
        }
        remove_legacy(&entries)?;
        Ok(status(&service) == ENABLED)
    })
}

fn apply_legacy(app: &tauri::AppHandle, enabled: bool, entries: &[(PathBuf, Value)]) -> Result<(), String> {
    if !enabled { return remove_legacy(entries); }
    let manager = app.autolaunch();
    // 插件保留当前文件名和 Label；已启用也必须补齐应用关联，不能提前返回。
    if !manager.is_enabled().map_err(|error| error.to_string())? {
        manager.enable().map_err(|error| error.to_string())?;
    }
    let path = launch_agents_dir()?.join(format!("{}.plist", app.package_info().name));
    let mut value = Value::from_file(&path).map_err(|error| error.to_string())?;
    let dictionary = value.as_dictionary_mut().ok_or_else(|| "自启配置格式不正确".to_string())?;
    let association = Value::Array(vec![Value::String(app.config().identifier.clone())]);
    if dictionary.get("AssociatedBundleIdentifiers") != Some(&association) {
        dictionary.insert("AssociatedBundleIdentifiers".to_string(), association);
        value.to_file_xml(&path).map_err(|error| error.to_string())?;
    }
    // 只清理本程序旧名称的重复项，保留插件当前使用的配置。
    let duplicates: Vec<_> = entries.iter().filter(|(old, _)| old != &path).cloned().collect();
    remove_legacy(&duplicates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Dictionary;

    #[test]
    fn only_migrates_an_enabled_legacy_registration_once() {
        assert!(should_migrate(true, true, NOT_REGISTERED));
        assert!(!should_migrate(false, true, NOT_REGISTERED));
        assert!(!should_migrate(true, false, NOT_REGISTERED));
        for state in [ENABLED, REQUIRES_APPROVAL, NOT_FOUND] {
            assert!(!should_migrate(true, true, state));
        }
    }

    #[test]
    fn accepts_only_executables_in_standard_app_bundles() {
        assert_eq!(bundled_app(Path::new("/Applications/PT Manager.app/Contents/MacOS/pt-manager")), Some(Path::new("/Applications/PT Manager.app")));
        assert!(bundled_app(Path::new("/tmp/target/debug/pt-manager")).is_none());
        assert!(bundled_app(Path::new("/Applications/Other.app/Resources/pt-manager")).is_none());
    }

    #[test]
    fn only_migrates_known_legacy_labels() {
        let mut dictionary = Dictionary::new();
        dictionary.insert("Label".into(), "unrelated.agent".into());
        dictionary.insert("ProgramArguments".into(), Value::Array(vec!["/Applications/PT Manager.app/Contents/MacOS/pt-manager".into()]));
        assert!(legacy_executable(&Value::Dictionary(dictionary.clone())).is_none());
        dictionary.insert("Label".into(), "PT Manager".into());
        assert_eq!(legacy_executable(&Value::Dictionary(dictionary)), Some("/Applications/PT Manager.app/Contents/MacOS/pt-manager"));
    }
}
