// Clippy's `doc_lazy_continuation` / `doc_list_item_indent` lints flag
// rustdoc indentation we wrote for prose continuation lines (slash-
// separated menu items, signatures). The intent is plain English
// rather than nested markdown lists; silencing avoids reformatting
// every module-level doc-comment in the crate.
#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

//! Roomler Agent Tray — small Tauri 2 companion app.
//!
//! Provides the onboarding GUI (paste enrollment token + device name)
//! + a status window (service running, agent version, attention
//! sentinel) + a system-tray icon with a right-click menu (Open
//! Status / Onboarding / Check for Updates / Open Logs Folder /
//! Quit Tray).
//!
//! Architecture (per rc.18 plan):
//! - The tray is a **thin orchestration layer**.
//! - Enrollment goes through `roomler_agent::enrollment::enroll` as a
//!   direct lib call (no subprocess).
//! - Service control / self-update / consent decisions shell out to
//!   the agent CLI (`roomler-agent service install`, `roomler-agent
//!   self-update`, `roomler-agent consent ...`).
//! - No socket / HTTP IPC — file sentinel pattern (already used by
//!   the agent's own ConsentBroker + needs-attention.txt) suffices.
//!
//! See `agents/roomler-agent-tray/Cargo.toml` for the dep wiring.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod commands;
mod tray;

use tauri::Manager;

fn main() {
    // Lightweight logging (the agent has its own persistent rolling
    // log; the tray's log is for tray-side issues like a failed
    // enrollment HTTP call). Stderr only — no file rotation.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second invocation: focus the existing window instead
            // of starting a new tray. Prevents "10 trays running"
            // when an operator double-clicks the launcher.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::cmd_status,
            commands::cmd_enroll,
            commands::cmd_re_enroll,
            commands::cmd_set_device_name,
            commands::cmd_default_device_name,
            commands::cmd_check_update,
            commands::cmd_apply_update,
            commands::cmd_service_install,
            commands::cmd_service_uninstall,
            commands::cmd_service_status,
            commands::cmd_open_log_dir,
            commands::cmd_open_config_dir,
            commands::cmd_consent_approve,
            commands::cmd_consent_deny,
        ])
        .setup(|app| {
            // Install the tray icon + menu. The main window starts
            // hidden (visible:false in tauri.conf.json); operator
            // opens it from the tray menu.
            tray::install(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running roomler-agent-tray");
}
