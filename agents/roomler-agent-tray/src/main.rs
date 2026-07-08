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
            commands::cmd_get_pending_consents,
        ])
        .setup(|app| {
            // Install the tray icon + menu. The main window starts
            // hidden (visible:false in tauri.conf.json); operator
            // opens it from the tray menu.
            tray::install(app.handle())?;
            // Phase 3 — watch the shared consent dir; when the agent drops a new
            // `.pending` marker (a remote session awaiting approval), surface the
            // window so the operator sees the Approve/Deny modal the SPA renders
            // from `cmd_get_pending_consents`.
            let handle = app.handle().clone();
            std::thread::spawn(move || consent_watch_loop(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running roomler-agent-tray");
}

/// Poll the shared consent dir; when a NEW `.pending` marker appears (a session
/// the operator hasn't been shown yet), surface the tray window so the SPA's
/// consent modal is visible. Runs on its own OS thread — a 750 ms filesystem
/// scan is cheap and needs no async runtime. The SPA does the actual
/// render/approve/deny via `cmd_get_pending_consents` + the existing
/// approve/deny commands; this loop's only job is "bring the window forward when
/// something new needs a decision."
fn consent_watch_loop(app: tauri::AppHandle) {
    use std::collections::HashSet;

    let Ok(dir) = roomler_agent::consent::ConsentBroker::default_sentinel_dir() else {
        return;
    };
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(750));
        let mut current: HashSet<String> = HashSet::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("pending")
                    && let Some(name) = p.file_stem().and_then(|s| s.to_str())
                {
                    current.insert(name.to_string());
                }
            }
        }
        // A newly-appeared pending (not in `seen`) → bring the window forward.
        if current.difference(&seen).next().is_some()
            && let Some(win) = app.get_webview_window("main")
        {
            let _ = win.show();
            let _ = win.set_focus();
        }
        seen = current;
    }
}
