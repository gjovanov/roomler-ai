//! System-tray icon + right-click menu. Built atop Tauri 2's
//! `TrayIcon` API (which wraps the `tray-icon` crate on the OS
//! layer).
//!
//! Menu items:
//!   - Open Status        — show the main window (status SPA)
//!   - Onboarding…        — show the main window navigated to the
//!                           enrollment SPA
//!   - Check for Updates  — invoke `cmd_check_update` and surface
//!                           the result in a system dialog
//!   - Open Logs Folder   — invoke `cmd_open_log_dir`
//!   - Quit Tray          — exit the tray process; agent keeps
//!                           running unaffected.

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Runtime};

pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    // Build the menu. IDs are inspected in `on_menu_event` below.
    let open_status = MenuItem::with_id(app, "open_status", "Open Status", true, None::<&str>)?;
    let onboarding = MenuItem::with_id(app, "onboarding", "Onboarding…", true, None::<&str>)?;
    let check_updates_item = MenuItem::with_id(
        app,
        "check_updates",
        "Check for Updates",
        true,
        None::<&str>,
    )?;
    let open_logs = MenuItem::with_id(app, "open_logs", "Open Logs Folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Tray", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open_status,
            &onboarding,
            &check_updates_item,
            &open_logs,
            &quit,
        ],
    )?;

    let _tray = TrayIconBuilder::with_id("roomler-agent-tray")
        .tooltip("Roomler Agent")
        .menu(&menu)
        .show_menu_on_left_click(false) // left-click brings up the main window; right-click is the menu
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open_status" => show_window(app, "index.html"),
            "onboarding" => show_window(app, "enrollment.html"),
            "check_updates" => check_updates(app),
            "open_logs" => {
                let _ = crate::commands::cmd_open_log_dir();
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Left-click on the tray icon opens the status window
            // for parity with operators who expect the icon itself
            // to do something useful.
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle(), "index.html");
            }
        })
        .build(app)?;
    Ok(())
}

fn show_window<R: Runtime>(app: &AppHandle<R>, path: &str) {
    if let Some(window) = app.get_webview_window("main") {
        // Navigate (no-op when already on the same path).
        let _ = window.eval(format!("window.location.hash = '#{path}'"));
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn check_updates<R: Runtime>(app: &AppHandle<R>) {
    match crate::commands::cmd_check_update() {
        Ok(stdout) => {
            // Forward to the main window so the SPA can render the
            // update banner. Sets a global flag the SPA polls.
            if let Some(window) = app.get_webview_window("main") {
                let payload = serde_json::json!({ "check": stdout }).to_string();
                let _ = window.eval(format!(
                    "window.__roomler_check_update_result = {payload}; window.dispatchEvent(new Event('roomler-update-check'))"
                ));
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        Err(e) => {
            tracing::warn!(%e, "check-update failed");
        }
    }
}
