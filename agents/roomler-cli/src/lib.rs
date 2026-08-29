//! `roomler` library surface.
//!
//! Re-export of `tunnel_core` so consumers of the binary have one
//! import path, plus the CLI-specific helpers used by [`cli`].
//! End-to-end tests in `crates/tests/` drive this lib in-process
//! against a `TestApp`, mirroring how `roomlerd` is driven from
//! `crates/tests/src/remote_control.rs`.

/// The whole `roomler` command surface (clap definitions + dispatch).
///
/// Lives in the LIB, not in `main.rs`, so it has two callers: the
/// standalone `roomler` binary (tunnel-only hosts) and `roomlerd cli`
/// (daemon hosts, via the `roomler` shim). Before P3e lever D the MSI
/// shipped a second full binary for the latter — 22 MiB of which ~92 %
/// duplicated crates the daemon already contains.
pub mod cli;
pub mod config;
pub mod forward;
/// Thin-client read verbs (`status`/`peers`/`flows`) over the daemon LocalAPI.
pub mod localclient;
pub mod mesh;
pub mod sshcmd;
pub mod update;

pub use tunnel_core::forward as core_forward;
pub use tunnel_core::{auth, mux, policy, signaling, socks5, transport, udp};
