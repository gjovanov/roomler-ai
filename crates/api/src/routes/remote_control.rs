// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-69 P5a — the agent routes that lived here are the `fleet` module's
//! (`roomler_ai_mod_fleet::agent`) and the session/TURN/relay half is
//! `routes/remote_session.rs`. What remains is the re-export the network and
//! remote route files still import from this path: the permission guard,
//! which has been core's since P3.
pub use roomler_core::guards::require_permission;
