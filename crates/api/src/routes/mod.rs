// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-69 — the host's remaining route files. The collaboration routes are the
// `chat` and `conference` modules' (P3, P4); device management (agents,
// enrollment, consent, exec, remote config, releases and the installer
// proxies, logs, crashes) is the `fleet` module's (P5a). What is left here is
// core's, `remote`'s and `network`'s, until their own PRs.
pub mod auth;
pub mod background_task;
pub mod capabilities;
pub mod cluster;
pub mod cost;
// The device listing joins agents (fleet) with tunnel clients and overlay
// nodes (network): a cross-pillar view that stays here until `network` exists.
pub mod integration;
pub mod invite;
pub mod notification;
pub mod oauth;
pub mod push;
pub mod role;
pub mod stats;
pub mod tenant;
pub mod usage;

pub mod user;
