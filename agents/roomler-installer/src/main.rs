//! Roomler Agent Installer — 5-step Tauri 2 wizard.
//!
//! Walks the operator from "I have a downloaded EXE" to "the agent
//! is installed, enrolled, and running" without requiring an
//! elevated PowerShell session. Replaces the rc.18-era manual ritual:
//!
//!   1. Pick MSI flavour (perUser vs perMachine — operators get it wrong)
//!   2. Run msiexec (silent install, no progress UI)
//!   3. Elevated PowerShell: `roomler-agent service install --as-service`
//!   4. Elevated PowerShell: `reg add … ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1`
//!   5. CLI: `roomler-agent enroll --server … --token …`
//!
//! The wizard owns all five steps and surfaces progress + recovery
//! UI for each. Architecture:
//!
//! - **Window-only**, no system-tray icon. Wizard exits on Finish or
//!   Cancel; it doesn't sit around after install.
//! - **Single-instance** via `tauri-plugin-single-instance`: a second
//!   launch focuses the existing window. While `cmd_install` is in
//!   flight (B9 + H4 from the plan critique), the second launch
//!   instead surfaces a snackbar event and exits without flashing a
//!   window, so the operator can't accidentally double-trigger the
//!   MSI.
//! - **State persistence** to `%LOCALAPPDATA%\roomler\roomler-installer\
//!   wizard-state.json` so a force-killed wizard resumes mid-flow.
//!   Token is NEVER persisted in plaintext (H5).
//! - **Progress streaming** via `tauri::ipc::Channel<ProgressEvent>`
//!   from inside `cmd_install` so the front-end SPA renders the
//!   live step-by-step state. Replay log catches any events emitted
//!   before the listener attached (H1).
//!
//! See `C:\Users\goran\.claude\plans\rc27-rc28-install-wizard.md`
//! §rc.28 for the full plan, and the rc.27 lib surface in
//! `roomler_agent::{install_detect, jwt_introspect, win_service,
//! enrollment, config}` that this binary builds on.

#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use tauri::{Emitter, Manager};
use wizard_core::{INSTALL_IN_PROGRESS, commands};

fn main() {
    // Stderr-only tracing. The wizard is a foreground EXE the
    // operator launches manually; persistent logging is the agent's
    // job, not ours. Operators who want wizard-side logs run from a
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
            // Per the plan's B9 fix: differentiate "install in flight"
            // from "idle re-launch". An in-flight cmd_install must
            // NOT pop a fresh window — operator could accidentally
            // trigger a duplicate msiexec via Cancel-then-Retry.
            //
            // Callback runs in the FIRST process per
            // `tauri-plugin-single-instance` docs, so the atomic load
            // here observes the first-process's flag.
            if INSTALL_IN_PROGRESS.load(std::sync::atomic::Ordering::SeqCst) {
                // Surface a snackbar on the existing window via the
                // SPA's `installer-already-running` listener.
                // Front-end shows "Wizard already running"; new EXE
                // exits silently.
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
            commands::cmd_force_kill_msi,
            commands::cmd_install_progress_replay,
            commands::cmd_exit_wizard,
        ])
        .run(tauri::generate_context!())
        .expect("error while running roomler-installer");
}
