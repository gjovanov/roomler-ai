//! Roomler Tunnel Installer — lib surface.
//!
//! The wizard's binary entry point lives in `src/main.rs`; everything
//! else (Tauri invoke handlers, asset resolver, archive extractor,
//! per-platform integration, install orchestrator, progress streaming,
//! wizard-state persistence) lives here so unit tests can execute
//! without spawning the Tauri webview EXE.
//!
//! The agent installer (`agents/roomler-installer/`) uses the same
//! shape; this crate is its tunnel-flavoured sibling. Differences from
//! the agent wizard:
//!
//! - **Cross-platform** (Win/Linux/macOS), not Windows-only. Each
//!   per-OS branch is `#[cfg(target_os = "…")]`-gated.
//! - **Archive extraction**, not msiexec — the tunnel CLI ships as a
//!   `.zip` / `.tar.gz`, not a `.msi`. No UAC, no Windows Installer.
//! - **No service registration** — `roomler-tunnel` is interactive
//!   only (foreground process driven by `forward` / `run` subcommands).
//! - **TunnelClient JWT audience**, not Agent. Enrollment endpoint is
//!   `POST /api/tunnel-client/enroll`.
//! - **First-forward step + policy hint** in Phase B (not v1). v1
//!   stops at install + enroll + Done.

#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

pub mod asset_resolver;
pub mod commands;
pub mod enroll;
pub mod extract;
pub mod install_orchestrator;
pub mod integration;
pub mod progress;
pub mod wizard_state;

use std::sync::atomic::AtomicBool;

/// `true` while a `cmd_install` future is in flight. Set by the
/// command on entry; cleared on success / cancel / error. The
/// single-instance callback in `main.rs` consults this to decide
/// whether to silently surface a "wizard busy" snackbar
/// (in-progress) or focus the existing window (idle).
///
/// Static-with-atomic rather than `OnceLock<Arc<AtomicBool>>` — it's
/// referenced from both the Tauri-runtime thread (callback) and the
/// cmd_install async future (different tokio task). Atomic-only is
/// the right primitive; no need for the extra Arc layer. Same pattern
/// as the agent installer's `wizard_core::INSTALL_IN_PROGRESS`.
pub static INSTALL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
