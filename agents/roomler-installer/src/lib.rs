//! Roomler Agent Installer — lib surface.
//!
//! The wizard's binary entry point lives in `src/main.rs`; everything
//! else (Tauri invoke handlers, MSI runner, future asset resolver +
//! progress streaming) lives here so unit tests can execute without
//! spawning the Tauri webview EXE (which would require elevation on
//! Windows when run from a non-admin shell).

#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

pub mod asset_resolver;
pub mod commands;
pub mod install_orchestrator;
pub mod msi_runner;
pub mod progress;
pub mod wizard_state;

use std::sync::atomic::AtomicBool;

/// `true` while a `cmd_install` future is in flight. Set by the
/// command on entry; cleared on success / cancel / error. The
/// single-instance callback in `main.rs` consults this to decide
/// whether to silently surface a "wizard busy" snackbar
/// (in-progress) or focus the existing window (idle).
///
/// Static-with-atomic rather than `OnceLock<Arc<AtomicBool>>` because
/// it's referenced from both the Tauri-runtime thread (callback) and
/// the cmd_install async future (different tokio task) — atomic-only
/// is the right primitive. No need for the extra Arc layer.
pub static INSTALL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
