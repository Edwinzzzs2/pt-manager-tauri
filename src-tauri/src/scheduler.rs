use crate::cdp::{CdpClient, CdpProgress, CDP_CANCELLED};
use crate::cookiecloud;
use crate::store::{self, AppConfig, LogEntry};
use chrono::{DateTime, Datelike, Local, LocalResult, TimeZone};
use rand::Rng;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::async_runtime::JoinHandle;
use tokio::sync::Mutex;

pub struct Scheduler {
    handle: Option<JoinHandle<()>>,
    next_run: Arc<Mutex<Option<DateTime<Local>>>>,
}

impl Scheduler {
    pub fn new(next_run: Arc<Mutex<Option<DateTime<Local>>>>) -> Self {
        Self {
            handle: None,
            next_run,
        }
    }

    /// 停止当前调度
    pub fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }

    /// 启动 cron 调度；配置保存后会重新调用本方法，使新配置立即生效。
    pub fn start(
        &mut self,
        config: AppConfig,
        logs: Arc<Mutex<Vec<LogEntry>>>,
        task_running: Arc<Mutex<bool>>,
        task_cancel_requested: Arc<AtomicBool>,
    ) {
        self.stop();

        let next_run = Arc::clone(&self.next_run);
        let handle = tauri::async_runtime::spawn(async move {
            loop {
                let Some(next) = next_run_from_cron(&config.cron) else {
                    {
                        let mut guard = next_run.lock().await;
                        *guard = None;
                    }
                    push_log(
                        &logs,
                        LogEntry::error(format!("Cron 表达式无效：{}", config.cron)),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                };

                {
                    let mut guard = next_run.lock().await;
                    *guard = Some(next);
                }

                let wait = next
                    .signed_duration_since(Local::now())
                    .to_std()
                    .unwrap_or_else(|_| Duration::from_secs(0));
                tokio::time::sleep(wait).await;

                run_with_flag(&config, &logs, &task_running, &task_cancel_requested, true).await;
            }
        });

        self.handle = Some(handle);
    }

    pub async fn next_run(&self) -> Option<DateTime<Local>> {
        self.next_run.lock().await.clone()
    }
}

pub async fn run_with_flag(
    config: &AppConfig,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    task_running: &Arc<Mutex<bool>>,
    task_cancel_requested: &Arc<AtomicBool>,
    allow_random_delay: bool,
) {
    {
        let mut running = task_running.lock().await;
        if *running {
            drop(running);
            push_log(logs, LogEntry::info("已有保活任务正在执行，跳过本次触发")).await;
            return;
        }
        *running = true;
        task_cancel_requested.store(false, Ordering::SeqCst);
    }

    let canceled =
        run_keepalive_inner(config, logs, task_cancel_requested, allow_random_delay).await;
    if canceled {
        push_log(logs, LogEntry::info("保活任务已终止")).await;
    }

    let mut running = task_running.lock().await;
    *running = false;
    task_cancel_requested.store(false, Ordering::SeqCst);
}

pub fn next_run_from_cron(expr: &str) -> Option<DateTime<Local>> {
    let cron = CronSpec::parse(expr)?;
    cron.next_after(Local::now())
}

async fn run_keepalive_inner(
    config: &AppConfig,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    task_cancel_requested: &Arc<AtomicBool>,
    allow_random_delay: bool,
) -> bool {
    let mut cdp = CdpClient::new(config.cdp_port);

    if config.sites.is_empty() {
        push_log(logs, LogEntry::info("暂无站点配置，保活任务结束")).await;
        return false;
    }

    if config.auto_sync_cookie {
        match sync_cookiecloud_before_keepalive(config, logs, task_cancel_requested).await {
            Ok((matched, imported)) => {
                push_log(
                    logs,
                    LogEntry::success(format!(
                        "保活前 CookieCloud 自动同步完成：匹配 {} 个，写入 {} 个",
                        matched, imported
                    )),
                )
                .await;
            }
            Err(err) => {
                if err == CDP_CANCELLED || task_cancel_requested.load(Ordering::SeqCst) {
                    return true;
                }
                push_log(
                    logs,
                    LogEntry::error(format!("保活前 CookieCloud 自动同步失败：{}，继续执行保活", err)),
                )
                .await;
            }
        }
    }

    // 手动保活会在启动 Chrome 时带上全部站点；定时任务保持顺序访问，避免随机延迟前先打开页面。
    let initial_urls = if allow_random_delay {
        Vec::new()
    } else {
        config
            .sites
            .iter()
            .map(|site| site.url.clone())
            .collect::<Vec<_>>()
    };
    let mut launched_with_initial_sites = false;
    if let Some(active_port) = cdp.available_port().await {
        if active_port != config.cdp_port {
            push_log(
                logs,
                LogEntry::info(format!("已复用自动 CDP 端口 localhost:{}", active_port)),
            )
            .await;
            cdp = CdpClient::new(active_port);
        }
    } else {
        push_log(
            logs,
            LogEntry::info("Chrome CDP 未连接，正在尝试自动启动 Chrome"),
        )
        .await;
        let cdp_progress = CdpProgress::new(Arc::clone(logs), Arc::clone(task_cancel_requested));
        match cdp
            .ensure_available_with_progress(&initial_urls, &cdp_progress)
            .await
        {
            Ok(result) => {
                cdp = CdpClient::new(result.port);
                launched_with_initial_sites = result.opened_initial_urls > 0;
                push_log(logs, LogEntry::info(result.message)).await;
            }
            Err(err) => {
                if err == CDP_CANCELLED || task_cancel_requested.load(Ordering::SeqCst) {
                    return true;
                }
                push_log(logs, LogEntry::error(err)).await;
                return false;
            }
        }
    }

    if task_cancel_requested.load(Ordering::SeqCst) {
        return true;
    }

    {
        let entry = LogEntry::info(format!("开始保活任务，共 {} 个站点", config.sites.len()));
        push_log(logs, entry).await;
    }

    // 随机延迟
    if allow_random_delay && config.random_delay {
        let delay_secs: u64 = rand::thread_rng().gen_range(0..1800); // 0~30分钟随机延迟
        {
            let entry = LogEntry::info(format!("随机延迟 {} 秒", delay_secs));
            push_log(logs, entry).await;
        }
        if sleep_with_cancel(
            logs,
            task_cancel_requested,
            Duration::from_secs(delay_secs),
            "随机延迟",
        )
        .await
        {
            return true;
        }
    }

    // 手动点击“立即保活”时批量打开所有站点，避免用户误以为只执行了第一个站点。
    if !allow_random_delay {
        return run_keepalive_batch(
            config,
            logs,
            &cdp,
            task_cancel_requested,
            launched_with_initial_sites,
        )
        .await;
    }

    for site in config.sites.iter() {
        if task_cancel_requested.load(Ordering::SeqCst) {
            return true;
        }

        {
            let entry = LogEntry::info(format!("正在访问: {} ({})", site.name, site.url));
            push_log(logs, entry).await;
        }

        let opened_tab = if launched_with_initial_sites {
            match cdp.find_tab_for_url(&site.url).await {
                Some(tab_id) => Ok(tab_id),
                None => cdp.open_tab(&site.url).await,
            }
        } else {
            cdp.open_tab(&site.url).await
        };

        match opened_tab {
            Ok(tab_id) => {
                // 等待页面加载 + 随机抖动
                let jitter: u64 = rand::thread_rng().gen_range(0..10);
                let wait = config.visit_duration + jitter;
                push_log(
                    logs,
                    LogEntry::info(format!("{} 停留 {} 秒，保持登录态", site.name, wait)),
                )
                .await;
                if sleep_with_cancel(
                    logs,
                    task_cancel_requested,
                    Duration::from_secs(wait),
                    &format!("{} 的停留等待", site.name),
                )
                .await
                {
                    if let Err(e) = cdp.close_tab(&tab_id).await {
                        push_log(
                            logs,
                            LogEntry::error(format!("终止时关闭标签页失败: {}", e)),
                        )
                        .await;
                    }
                    return true;
                }

                // 关闭标签页
                if let Err(e) = cdp.close_tab(&tab_id).await {
                    let entry = LogEntry::error(format!("关闭标签页失败: {}", e));
                    push_log(logs, entry).await;
                }

                let entry = LogEntry::success(format!("{} 保活完成", site.name));
                push_log(logs, entry).await;
            }
            Err(e) => {
                let entry = LogEntry::error(format!("{} 访问失败: {}", site.name, e));
                push_log(logs, entry).await;
            }
        }

        // 站点间隔 5~15 秒
        let interval: u64 = rand::thread_rng().gen_range(5..15);
        push_log(
            logs,
            LogEntry::info(format!("站点间隔等待 {} 秒", interval)),
        )
        .await;
        if sleep_with_cancel(
            logs,
            task_cancel_requested,
            Duration::from_secs(interval),
            "站点间隔等待",
        )
        .await
        {
            return true;
        }
    }

    {
        let entry = LogEntry::success("保活任务全部完成".to_string());
        push_log(logs, entry).await;
    }
    false
}

async fn sync_cookiecloud_before_keepalive(
    config: &AppConfig,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    task_cancel_requested: &Arc<AtomicBool>,
) -> Result<(usize, usize), String> {
    push_log(logs, LogEntry::info("保活前自动同步 CookieCloud")).await;

    let cookiecloud_config = config.cookiecloud.clone();
    let cookie_data = tauri::async_runtime::spawn_blocking(move || {
        cookiecloud::fetch_cookie_data(&cookiecloud_config)
    })
    .await
    .map_err(|err| err.to_string())??;
    let cookie_params = cookiecloud::cookies_for_sites(cookie_data, &config.sites)?;
    if cookie_params.is_empty() {
        return Err("CookieCloud 未匹配到当前站点的 Cookie".to_string());
    }

    let cdp = CdpClient::new(config.cdp_port);
    let active_port = match cdp.available_port().await {
        Some(port) => port,
        None => {
            let progress = CdpProgress::new(Arc::clone(logs), Arc::clone(task_cancel_requested));
            cdp.ensure_available_with_progress(&[], &progress).await?.port
        }
    };
    let imported = CdpClient::new(active_port).set_cookies(&cookie_params).await?;
    Ok((cookie_params.len(), imported))
}

async fn run_keepalive_batch(
    config: &AppConfig,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    cdp: &CdpClient,
    task_cancel_requested: &Arc<AtomicBool>,
    launched_with_initial_sites: bool,
) -> bool {
    let mut opened_tabs: Vec<(String, String)> = Vec::new();

    for site in config.sites.iter() {
        if task_cancel_requested.load(Ordering::SeqCst) {
            close_opened_tabs(cdp, logs, opened_tabs).await;
            return true;
        }

        push_log(
            logs,
            LogEntry::info(format!("正在打开: {} ({})", site.name, site.url)),
        )
        .await;

        let opened_tab = if launched_with_initial_sites {
            match cdp.find_tab_for_url(&site.url).await {
                Some(tab_id) => Ok(tab_id),
                None => cdp.open_tab(&site.url).await,
            }
        } else {
            cdp.open_tab(&site.url).await
        };

        match opened_tab {
            Ok(tab_id) => {
                push_log(logs, LogEntry::info(format!("{} 已打开", site.name))).await;
                opened_tabs.push((site.name.clone(), tab_id));
            }
            Err(e) => {
                push_log(
                    logs,
                    LogEntry::error(format!("{} 打开失败: {}", site.name, e)),
                )
                .await;
            }
        }
    }

    if opened_tabs.is_empty() {
        push_log(logs, LogEntry::error("没有成功打开任何站点，保活任务结束")).await;
        return false;
    }

    push_log(
        logs,
        LogEntry::info(format!(
            "已打开 {} 个站点，停留 {} 秒",
            opened_tabs.len(),
            config.visit_duration
        )),
    )
    .await;
    if sleep_with_cancel(
        logs,
        task_cancel_requested,
        Duration::from_secs(config.visit_duration),
        "批量停留等待",
    )
    .await
    {
        close_opened_tabs(cdp, logs, opened_tabs).await;
        return true;
    }

    for (site_name, tab_id) in opened_tabs {
        if let Err(e) = cdp.close_tab(&tab_id).await {
            push_log(
                logs,
                LogEntry::error(format!("{} 关闭标签页失败: {}", site_name, e)),
            )
            .await;
            continue;
        }
        push_log(logs, LogEntry::success(format!("{} 保活完成", site_name))).await;
    }

    push_log(logs, LogEntry::success("保活任务全部完成".to_string())).await;
    false
}

async fn sleep_with_cancel(
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    task_cancel_requested: &Arc<AtomicBool>,
    duration: Duration,
    stage: &str,
) -> bool {
    let mut elapsed = Duration::from_secs(0);

    while elapsed < duration {
        if task_cancel_requested.load(Ordering::SeqCst) {
            push_log(
                logs,
                LogEntry::info(format!("收到终止请求，正在中断{}", stage)),
            )
            .await;
            return true;
        }

        let remaining = duration.saturating_sub(elapsed);
        let step = remaining.min(Duration::from_secs(1));
        tokio::time::sleep(step).await;
        elapsed += step;
    }

    task_cancel_requested.load(Ordering::SeqCst)
}

async fn close_opened_tabs(
    cdp: &CdpClient,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    opened_tabs: Vec<(String, String)>,
) {
    for (site_name, tab_id) in opened_tabs {
        if let Err(e) = cdp.close_tab(&tab_id).await {
            push_log(
                logs,
                LogEntry::error(format!("终止时关闭 {} 标签页失败: {}", site_name, e)),
            )
            .await;
        }
    }
}

struct CronSpec {
    seconds: Vec<u32>,
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
}

impl CronSpec {
    fn parse(expr: &str) -> Option<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        let fields = match parts.len() {
            5 => vec!["0", parts[0], parts[1], parts[2], parts[3], parts[4]],
            6 => parts,
            _ => return None,
        };

        Some(Self {
            seconds: parse_cron_field(fields[0], 0, 59)?,
            minutes: parse_cron_field(fields[1], 0, 59)?,
            hours: parse_cron_field(fields[2], 0, 23)?,
            days: parse_cron_field(fields[3], 1, 31)?,
            months: parse_cron_field(fields[4], 1, 12)?,
            weekdays: parse_cron_field(fields[5], 0, 7)?,
        })
    }

    /// 从当前时间向后扫描一年内的候选时间，覆盖 MVP 所需的日/周/分钟级调度。
    fn next_after(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        let today = now.date_naive();

        for day_offset in 0..=366 {
            let date = today.checked_add_days(chrono::Days::new(day_offset))?;
            if !self.matches_date(
                date.month(),
                date.day(),
                date.weekday().num_days_from_sunday(),
            ) {
                continue;
            }

            for hour in &self.hours {
                for minute in &self.minutes {
                    for second in &self.seconds {
                        let candidate = match Local.with_ymd_and_hms(
                            date.year(),
                            date.month(),
                            date.day(),
                            *hour,
                            *minute,
                            *second,
                        ) {
                            LocalResult::Single(value) => value,
                            _ => continue,
                        };

                        if candidate > now {
                            return Some(candidate);
                        }
                    }
                }
            }
        }

        None
    }

    fn matches_date(&self, month: u32, day: u32, weekday: u32) -> bool {
        let weekday_matches =
            self.weekdays.contains(&weekday) || (weekday == 0 && self.weekdays.contains(&7));

        self.months.contains(&month) && self.days.contains(&day) && weekday_matches
    }
}

fn parse_cron_field(field: &str, min: u32, max: u32) -> Option<Vec<u32>> {
    let mut values = Vec::new();

    for raw_part in field.split(',') {
        let part = raw_part.trim();
        if part.is_empty() {
            return None;
        }

        let (range_part, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok()?),
            None => (part, 1),
        };

        if step == 0 {
            return None;
        }

        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((start, end)) = range_part.split_once('-') {
            (start.parse::<u32>().ok()?, end.parse::<u32>().ok()?)
        } else {
            let value = range_part.parse::<u32>().ok()?;
            (value, value)
        };

        if start < min || end > max || start > end {
            return None;
        }

        let mut value = start;
        while value <= end {
            values.push(value);
            value = value.saturating_add(step);
            if value == u32::MAX {
                break;
            }
        }
    }

    values.sort_unstable();
    values.dedup();
    Some(values)
}

async fn push_log(logs: &Arc<Mutex<Vec<LogEntry>>>, entry: LogEntry) {
    store::push_log(logs, entry).await;
}
