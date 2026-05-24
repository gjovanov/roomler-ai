//! Roomler Tunnel Installer — 4-step Tauri 2 wizard.
//!
//! Walks the operator from "I have a downloaded EXE" to "the tunnel
//! client is installed, enrolled, and on PATH" without requiring a
//! terminal session. Replaces the manual ritual:
//!
//!   1. Download the CLI archive
//!   2. Extract somewhere
//!   3. Add to PATH
//!   4. `roomler-tunnel enroll --server … --token …`
//!
//! The wizard owns every step and surfaces progress + recovery UI
//! per failure mode. Architecture mirrors the agent installer's
//! rc.28 wizard:
//!
//! - **Window-only**, no tray icon. Exits on Finish or Cancel.
//! - **Single-instance** via `tauri-plugin-single-instance`: second
//!   launch focuses the existing window. While `cmd_install` is in
//!   flight the second launch surfaces a snackbar event and exits
//!   without flashing a window — operator can't accidentally double-
//!   trigger the download.
//! - **State persistence** to `<config-dir>/wizard-state.json` so a
//!   force-killed wizard resumes mid-flow. **Token NEVER persisted.**
//! - **Progress streaming** via `tauri::ipc::Channel<ProgressEvent>`.
//!
//! See `tunnel_wizard_core::install_orchestrator` for the pipeline
//! and `src/front/app.js` for the SPA driver.

#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use tauri::{Emitter, Manager};
use tunnel_wizard_core::{INSTALL_IN_PROGRESS, commands};

fn main() {
    // Stderr-only tracing. The wizard is a foreground EXE the operator
    // launches manually; persistent logging is the agent + CLI's job,
    // not ours. Operators who want wizard-side logs run from a
    // terminal and observe stderr.
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
            // Differentiate "install in flight" from "idle re-launch".
            // An in-flight cmd_install must NOT pop a fresh window —
            // operator could accidentally re-trigger the download.
            //
            // Callback runs in the FIRST process per the plugin docs,
            // so the atomic load here observes the first-process's
            // flag.
            if INSTALL_IN_PROGRESS.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.emit("installer-already-running", ());
                }
                return;
            }
            // Idle: focus the existing window + leave the persisted
            // step intact. Wizard state JSON drives the resume.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::cmd_detect_install,
            commands::cmd_default_device_name,
            commands::cmd_default_server_url,
            commands::cmd_validate_token,
            commands::cmd_load_state,
            commands::cmd_save_state,
            commands::cmd_install,
            commands::cmd_cancel_in_progress,
            commands::cmd_install_progress_replay,
            commands::cmd_exit_wizard,
        ])
        .run(tauri::generate_context!())
        .expect("error while running roomler-tunnel-installer");
}
