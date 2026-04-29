//! Library crate for `roomler-agent`. The binary at `src/main.rs` is a thin
//! CLI shell around these modules; exposing them here lets integration
//! tests drive the agent in-process against a `TestApp` server.

pub mod capture;
#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod config;
pub mod displays;
pub mod encode;
pub mod enrollment;
pub mod files;
pub mod indicator;
pub mod input;
pub mod instance_lock;
pub mod logging;
pub mod machine;
pub mod notify;
pub mod peer;
pub mod post_install;
pub mod service;
pub mod signaling;
pub mod updater;
pub mod watchdog;
