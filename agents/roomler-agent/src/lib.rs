//! Library crate for `roomler-agent`. The binary at `src/main.rs` is a thin
//! CLI shell around these modules; exposing them here lets integration
//! tests drive the agent in-process against a `TestApp` server.

pub mod capture;
#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod config;
pub mod displays;
#[cfg(target_os = "windows")]
pub mod dpi;
pub mod encode;
pub mod enrollment;
pub mod files;
pub mod indicator;
pub mod input;
pub mod instance_lock;
pub mod lock_state;
pub mod logging;
pub mod machine;
pub mod notify;
pub mod peer;
pub mod post_install;
pub mod preflight;
pub mod service;
pub mod signaling;
pub mod updater;
pub mod watchdog;
#[cfg(target_os = "windows")]
pub mod win_service;
