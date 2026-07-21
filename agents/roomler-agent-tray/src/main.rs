// Clippy's `doc_lazy_continuation` / `doc_list_item_indent` lints flag
// rustdoc indentation we wrote for prose continuation lines (slash-
// separated menu items, signatures). The intent is plain English
// rather than nested markdown lists; silencing avoids reformatting
// every module-level doc-comment in the crate.
#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

//! Roomler desktop (`roomler-desktop`) — the node stack's control
//! surface. Tauri 2, lives in the system tray, one window with a
//! sidebar SPA (`src/front/`): Overview / Devices / Tunnels /
//! Settings / Onboarding + the remote-control consent modal.
//!
//! Architecture:
//! - The desktop app is a **thin client**: live node/peer/flow/route
//!   state comes from the running daemon over the LocalAPI
//!   (`tunnel_core::localapi::Client`); consent decisions go the same
//!   way (P2b — the daemon owns the profile-correct sentinel dir).
//! - Enrollment goes through `roomler_agent::enrollment::enroll` as a
//!   direct lib call (no subprocess).
//! - Service control / self-update shell out to the daemon CLI
//!   (`roomlerd service install`, `roomlerd self-update`).
//! - Closing the window HIDES it (close-to-hide below) — the tray icon
//!   and the consent watcher must outlive the window, since surfacing
//!   consent prompts is this app's safety-relevant job.
//!
//! See `agents/roomler-agent-tray/Cargo.toml` for the dep wiring (the
//! package keeps its historical name; the output binary is
//! `roomler-desktop` since P3d).

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
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // Second invocation: focus the existing window instead
            // of starting a new tray. Prevents "10 trays running"
            // when an operator double-clicks the launcher.
            if let Some(window) = app.get_webview_window("main") {
                // Deep-link: `roomler-desktop --view=<name>` routes the SPA
                // to that view (shortcuts / scripts / smoke tests). The name
                // is whitelisted to ascii-alphanumeric before it goes near
                // eval; the router maps anything unknown to Overview.
                if let Some(view) = args.iter().find_map(|a| a.strip_prefix("--view="))
                    && !view.is_empty()
                    && view.chars().all(|c| c.is_ascii_alphanumeric())
                {
                    let _ = window.eval(format!("window.location.hash = '#/{view}'"));
                }
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::cmd_status,
            commands::cmd_device_view,
            commands::cmd_ping,
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
            commands::cmd_route_list,
            commands::cmd_route_add,
            commands::cmd_route_remove,
            commands::cmd_route_set_enabled,
            commands::cmd_flows,
        ])
        .on_window_event(|window, event| {
            // Close-to-hide: the window is a view over a resident tray app.
            // Letting the close destroy the last window would exit the
            // process (Tauri default) and take the consent watcher down with
            // it — remote-control prompts would silently stop surfacing.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
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
            tauri::async_runtime::spawn(consent_watch_loop(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running roomler-desktop");
}

/// Ask the daemon (over the LocalAPI) which sessions await consent; when a NEW
/// one appears (not shown yet), surface the tray window so the SPA's consent
/// modal is visible. P2b: polling the daemon — not a profile-specific sentinel
/// dir — means this also works when the agent runs as SYSTEM (the dir would be
/// in the SYSTEM profile, unreachable to this interactive-user process). Runs on
/// Tauri's async runtime; a 750 ms poll over the local pipe is cheap. The SPA
/// does the actual render/approve/deny via `cmd_get_pending_consents` + the
/// approve/deny commands; this loop's only job is "bring the window forward when
/// something new needs a decision."
async fn consent_watch_loop(app: tauri::AppHandle) {
    use std::collections::HashSet;

    let mut seen: HashSet<String> = HashSet::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        let current: HashSet<String> = match tunnel_core::localapi::connect().await {
            Ok(mut c) => c
                .consent_pending()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.session_id)
                .collect(),
            // Daemon down / pipe absent ⇒ nothing pending (stay quiet).
            Err(_) => HashSet::new(),
        };
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
