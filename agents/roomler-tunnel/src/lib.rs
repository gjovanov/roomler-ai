//! `roomler-tunnel` library surface.
//!
//! Re-export of `tunnel_core` so consumers of the binary have one
//! import path. End-to-end tests in `crates/tests/` drive this lib
//! in-process against a `TestApp`, mirroring how `roomler-agent` is
//! driven from `crates/tests/src/remote_control.rs`.

pub use tunnel_core::{auth, forward, mux, policy, signaling, transport};
