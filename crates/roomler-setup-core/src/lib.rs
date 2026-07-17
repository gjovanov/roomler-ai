//! Shared install-wizard machinery for the unified `roomler-setup`
//! app — and, until P4c retires them, the two legacy wizard crates
//! (`roomler-installer`, `roomler-tunnel-installer`), which re-export
//! these modules through path-compatible shims so their behaviour
//! stays byte-identical.
//!
//! Layering rule (also spelled out in Cargo.toml): this crate is the
//! EVENT-SHAPE-FREE mechanics layer. No `tauri`, no `roomler-agent`,
//! no `roomler-tunnel` — the apps own the Tauri surface, the
//! ProgressEvent wire shapes, and the agent/tunnel lib calls.
//! Mechanics here communicate through plain callbacks (`FnMut(u64)`
//! byte ticks) and returned reports, which is what lets one copy
//! serve wizards with different (frozen) wire contracts.

#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

pub mod asset_resolver;
pub mod extract;
pub mod integration;
pub mod msi_runner;
pub mod tunnel_enroll;
