use chrono::{DateTime, Local, LocalResult, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::Mutex;

const DEFAULT_LOG_RETENTION: usize = 500;
const MIN_LOG_RETENTION: usize = 50;
const MAX_LOG_RETENTION: usize = 5000;
static LOG_RETENTION: AtomicUsize = AtomicUsize::new(DEFAULT_LOG_RETENTION);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    pub id: String,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub sites: Vec<Site>,
    pub cron: String,
    pub cdp_port: u16,
    pub visit_duration: u64,
    pub random_delay: bool,
    pub auto_launch: bool,
    #[serde(default = "default_log_retention")]
    pub log_retention: usize,
    #[serde(default)]
    pub auto_sync_cookie: bool,
    #[serde(default)]
    pub cookiecloud: CookieCloudConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CookieCloudConfig {
    pub server_url: String,
    pub uuid: String,
    pub password: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            sites: vec![],
            cron: "0 9 * * *".to_string(),
            cdp_port: 9222,
            visit_duration: 30,
            random_delay: true,
            auto_launch: false,
            log_retention: DEFAULT_LOG_RETENTION,
            auto_sync_cookie: false,
            cookiecloud: CookieCloudConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Local>,
    pub level: String,
    pub message: String,
}

impl LogEntry {
    pub fn info(msg: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now(),
            level: "INFO".to_string(),
            message: msg.into(),
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now(),
            level: "ERROR".to_string(),
            message: msg.into(),
        }
    }

    pub fn success(msg: impl Into<String>) -> Self {
        Self {
            timestamp: Local::now(),
            level: "SUCCESS".to_string(),
            message: msg.into(),
        }
    }
}

fn local_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            let dir = PathBuf::from(root).join("pt-manager");
            fs::create_dir_all(&dir).ok();
            return dir;
        }
    }

    let dir = std::env::temp_dir().join("pt-manager");
    fs::create_dir_all(&dir).ok();
    dir
}

pub fn log_file_path() -> PathBuf {
    local_data_dir().join("run.log")
}

fn default_log_retention() -> usize {
    DEFAULT_LOG_RETENTION
}

pub fn normalize_log_retention(value: usize) -> usize {
    value.clamp(MIN_LOG_RETENTION, MAX_LOG_RETENTION)
}

pub fn set_log_retention(value: usize) {
    LOG_RETENTION.store(normalize_log_retention(value), Ordering::Relaxed);
}

pub fn current_log_retention() -> usize {
    normalize_log_retention(LOG_RETENTION.load(Ordering::Relaxed))
}

pub fn append_log(entry: &LogEntry) {
    let path = log_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", format_log_line(entry));
    }
}

pub fn load_logs() -> Vec<LogEntry> {
    let data = fs::read_to_string(log_file_path()).unwrap_or_default();
    let mut logs = data
        .lines()
        .filter_map(parse_log_line)
        .collect::<Vec<_>>();
    if trim_logs(&mut logs, current_log_retention()) {
        write_log_file(&logs);
    }
    logs
}

fn parse_log_line(line: &str) -> Option<LogEntry> {
    let (timestamp_text, rest) = line.split_once(" [")?;
    let (level, message) = rest.split_once("] ")?;
    let naive = NaiveDateTime::parse_from_str(timestamp_text, "%Y-%m-%d %H:%M:%S").ok()?;
    // 日志文件只有本地时间文本，恢复时按当前本地时区解释即可。
    let timestamp = match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(earliest, _) => earliest,
        LocalResult::None => return None,
    };

    Some(LogEntry {
        timestamp,
        level: level.to_string(),
        message: message.to_string(),
    })
}

pub fn clear_log_file() {
    let _ = fs::write(log_file_path(), "");
}

pub async fn push_log(logs: &Arc<Mutex<Vec<LogEntry>>>, entry: LogEntry) {
    append_log(&entry);
    let mut guard = logs.lock().await;
    guard.push(entry);
    if trim_logs(&mut guard, current_log_retention()) {
        write_log_file(&guard);
    }
}

pub async fn apply_log_retention(logs: &Arc<Mutex<Vec<LogEntry>>>) {
    let mut guard = logs.lock().await;
    if trim_logs(&mut guard, current_log_retention()) {
        write_log_file(&guard);
    }
}

fn trim_logs(logs: &mut Vec<LogEntry>, retention: usize) -> bool {
    let retention = normalize_log_retention(retention);
    if logs.len() <= retention {
        return false;
    }

    let overflow = logs.len() - retention;
    logs.drain(0..overflow);
    true
}

fn write_log_file(logs: &[LogEntry]) {
    let content = logs
        .iter()
        .map(format_log_line)
        .collect::<Vec<_>>()
        .join("\n");
    let suffix = if content.is_empty() { "" } else { "\n" };
    let _ = fs::write(log_file_path(), format!("{}{}", content, suffix));
}

fn format_log_line(entry: &LogEntry) -> String {
    format!(
        "{} [{}] {}",
        entry.timestamp.format("%Y-%m-%d %H:%M:%S"),
        entry.level,
        entry.message
    )
}

/// 获取配置文件路径
fn config_path(app_handle: &tauri::AppHandle) -> PathBuf {
    let dir = app_handle
        .path()
        .app_data_dir()
        .expect("failed to get app data dir");
    fs::create_dir_all(&dir).ok();
    dir.join("config.json")
}

/// 从磁盘加载配置
pub fn load_config(app_handle: &tauri::AppHandle) -> AppConfig {
    let path = config_path(app_handle);
    if path.exists() {
        let data = fs::read_to_string(&path).unwrap_or_default();
        let mut config = serde_json::from_str::<AppConfig>(&data).unwrap_or_default();
        config.log_retention = normalize_log_retention(config.log_retention);
        config
    } else {
        let config = AppConfig::default();
        save_config(app_handle, &config);
        config
    }
}

/// 保存配置到磁盘
pub fn save_config(app_handle: &tauri::AppHandle, config: &AppConfig) {
    let path = config_path(app_handle);
    if let Ok(data) = serde_json::to_string_pretty(config) {
        fs::write(&path, data).ok();
    }
}
