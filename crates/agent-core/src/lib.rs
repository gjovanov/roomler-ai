//! Daemon-free agent building blocks (P3e lever E).
//!
//! Everything a THIN client needs to cohabit with the daemon — config
//! load/save, enrollment, machine-id derivation, log files + tailing, the
//! needs-attention sentinel — without any of the daemon's data plane
//! (capture, encode, input, webrtc, overlay). `roomler-agent` re-exports
//! each module under its old `crate::` path; the desktop companion depends
//! on this crate directly.
//!
//! Module notes:
//! * [`notify`] carries the sentinel primitives; the rc.53 worker-aware
//!   wrappers stayed in `roomler-agent` (they probe the SystemContext
//!   worker role — daemon-only machinery).
//! * [`apps_config`] carries ONLY the `[virtual_desktop_apps]` serde shapes
//!   that [`config::AgentConfig`] embeds; the launch machinery stays in
//!   `roomler-agent::apps` and re-exports them.
//! * [`acl`] evaluates over shapes canonical in
//!   `roomler_ai_remote_control::models` (where `dst_matches` also lives
//!   since this split).

pub mod acl;
pub mod appdirs;
pub mod apps_config;
pub mod config;
pub mod config_surface;
pub mod crash_recorder;
pub mod enrollment;
pub mod logging;
pub mod logs_upload;
pub mod machine;
pub mod notify;
