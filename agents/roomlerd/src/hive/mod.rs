// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0c — agent sessions on this device (cargo feature `hive`).
//!
//! The device half of `rc:hive.*`. The server asks for a session to start or
//! stop; this module applies the DEVICE's own gates — `hive_enabled`, the
//! account `hive_accounts` maps the starter to, the folder confined to
//! `hive_roots`, capacity, a harness to run — answers `rc:hive.start_ack`,
//! launches Claude Code headless as that account through the one privilege
//! path exec and SSH already share, keeps the transcript in the replica store,
//! and reports each session's lifecycle with `rc:hive.state`. Only metadata
//! crosses the control WS (`docs/roomler-hive-design.md` §3.3).
//!
//! # What P0 covers
//!
//! Linux, a named account (`RunAs::Named`), the harness as the daemon's child.
//! The re-exec'd `hive-host` — Windows' console user, macOS, a session that
//! outlives a daemon restart — is P1; until then a daemon restart ends the
//! sessions it ran, and they report `ended`. Elsewhere the build does not
//! advertise `hive`, so no server sends it a start.
//!
//! # Whose frames count
//!
//! Only the PRIMARY enrollment's. `hive_enabled` and the account map belong to
//! the device's owner; a secondary org's admin must not start sessions on a
//! device that org merely borrows — the rule `rc:agent.update` already follows.

//!
//! # Reading a session from a browser (P0d-2)
//!
//! [`view`]: a view grant from the server, a data-only WebRTC peer to the
//! browser, and the transcript — pages, the live feed, prompts from a viewer
//! that may drive — over its one DataChannel. The server relays the
//! handshake and never sees what flows over the peer.

pub mod framing;
pub mod gates;
mod sidecar;
mod store;
mod supervisor;
pub mod view;

#[cfg(feature = "hive-test-launcher")]
pub use supervisor::init_as_daemon;
pub use supervisor::{
    Author, StartOrder, Supervisor, global, handle_start, handle_stop, init, on_connected,
};
pub use view::{
    ViewGrant, handle_view_close, handle_view_grant, handle_view_ice, handle_view_offer,
    handle_view_renew,
};
