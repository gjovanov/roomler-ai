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
pub mod progress;
pub mod wizard_state;

// P4a: relocated to the shared wizard core (`crates/roomler-setup-core`);
// re-exported here so `wizard_core::msi_runner::…` / `crate::msi_runner`
// paths (commands, orchestrator) stay valid while this legacy wizard
// ships. Retired with the whole crate in P4c.
pub use wizard_shared::msi_runner;

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
