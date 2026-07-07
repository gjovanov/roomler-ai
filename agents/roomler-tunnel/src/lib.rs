//! `roomler-tunnel` library surface.
//!
//! Re-export of `tunnel_core` so consumers of the binary have one
//! import path, plus the CLI-specific helpers used by `main.rs`.
//! End-to-end tests in `crates/tests/` drive this lib in-process
//! against a `TestApp`, mirroring how `roomler-agent` is driven from
//! `crates/tests/src/remote_control.rs`.

pub mod config;
pub mod forward;
pub mod mesh;
pub mod socks5;

pub use tunnel_core::forward as core_forward;
pub use tunnel_core::{auth, mux, policy, signaling, transport};
