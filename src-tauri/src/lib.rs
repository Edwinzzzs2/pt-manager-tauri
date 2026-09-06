mod auth;
mod cdp;
mod commands;
mod cookiecloud;
mod gotify;
mod ocr;
mod scheduler;
mod store;
mod updater;

use chrono::{DateTime, Local};
use commands::AppState;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri::WindowEvent;
use tokio::sync::Mutex;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Prevent launching multiple desktop instances. A second launch reuses
        // and focuses the existing main window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // 加载配置
            let config = store::load_config(&app.handle());
            store::set_log_retention(config.log_retention);
            let _ = commands::apply_auto_launch(&app.handle(), config.auto_launch);
            let logs = Arc::new(Mutex::new(store::load_logs()));
            let task_running = Arc::new(Mutex::new(false));
            let task_cancel_requested = Arc::new(AtomicBool::new(false));
            let next_run: Arc<Mutex<Option<DateTime<Local>>>> = Arc::new(Mutex::new(None));
            let mut scheduler = scheduler::Scheduler::new(Arc::clone(&next_run));
            let config_state = Arc::new(Mutex::new(config.clone()));
            scheduler.start(
                config.clone(),
                Arc::clone(&logs),
                Arc::clone(&task_running),
                Arc::clone(&task_cancel_requested),
                Some(app.handle().clone()),
                Some(Arc::clone(&config_state)),
            );

            // 初始化全局状态
            let state = AppState {
                config: config_state,
                logs,
                scheduler: Arc::new(Mutex::new(scheduler)),
                task_running,
                task_cancel_requested,
                app_handle: app.handle().clone(),
            };
            app.manage(state);

            // 设置系统托盘
            setup_tray(app)?;
            setup_window_close_behavior(app);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::test_gotify,
            commands::add_site,
            commands::import_sites_from_json,
            commands::remove_site,
            commands::remove_sites,
            commands::update_site,
            commands::test_site_login,
            commands::recognize_site_captcha,
            commands::check_cdp,
            commands::ensure_cdp,
            commands::sync_cookiecloud_cookies,
            commands::sync_cookiecloud_from_config,
            commands::get_status,
            commands::run_task,
            commands::stop_task,
            commands::get_logs,
            commands::clear_logs,
            commands::clear_browser_data,
            commands::open_chrome_download,
            commands::export_config,
            commands::import_config,
            updater::check_for_app_update,
            updater::download_and_install_app_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItemBuilder::with_id("show", "打开主界面").build(app)?;
    let run = MenuItemBuilder::with_id("run", "立即执行保活").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&show, &run, &quit]).build()?;

    #[cfg(target_os = "windows")]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    #[cfg(not(target_os = "windows"))]
    let icon = app.default_window_icon().unwrap().clone();

    let _tray = TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .menu(&menu)
        .tooltip("PT Manager — 保活运行中")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "run" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    let config = state.config.lock().await.clone();
                    scheduler::run_with_flag(
                        &config,
                        &state.logs,
                        &state.task_running,
                        &state.task_cancel_requested,
                        false,
                        Some(&state.app_handle),
                        Some(&state.config),
                    )
                    .await;
                });
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            #[cfg(target_os = "windows")]
            if matches!(&event, TrayIconEvent::Enter { .. }) {
                refresh_windows_tray(tray);
            }

            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        })
        .build(app)?;

    #[cfg(target_os = "windows")]
    {
        let app = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            loop {
                // Sleep after each refresh to avoid catch-up bursts after suspend.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let Some(tray) = app.tray_by_id("main-tray") else {
                    break;
                };
                refresh_windows_tray(&tray);
            }
        });
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn refresh_windows_tray(tray: &tauri::tray::TrayIcon) {
    // Recreate the native icon from embedded pixels and ask Explorer to repaint.
    // This is a recovery measure for stale tray rendering, not a task status update.
    let result = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))
        .and_then(|icon| tray.set_icon(Some(icon)));
    if let Err(error) = result {
        eprintln!("Failed to refresh tray icon: {error}");
    }
}

fn setup_window_close_behavior(app: &tauri::App) {
    if let Some(window) = app.get_webview_window("main") {
        let window_to_hide = window.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window_to_hide.hide();
            }
        });
    }
}
