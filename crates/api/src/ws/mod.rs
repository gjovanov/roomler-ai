// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-69 P7b — the frame ceiling is core's (`roomler_core::ws::upgrade`),
// shared by every upgrade whoever owns it; re-exported so the paths in this
// crate read as before.
pub use roomler_core::ws::upgrade::MAX_WS_MESSAGE_BYTES;

// FR-69 P5c — the host's transitional `network` half of the agent
// socket, registered on the core's `AgentSocketRegistry` under that id (the
// `remote` half is the remote module's since P6).
pub mod agent_socket_host;
pub mod derp;
pub mod derp_cluster;
pub mod ephemeral;
pub mod handler;
pub mod remote_control;
pub mod tunnel;

// FR-69 P1b — the connection registry, the fan-out primitives and the Redis
// pub/sub layer are core (`roomler_core::ws`); re-exported here so every
// `crate::ws::{storage, dispatcher, redis_pubsub}` path in this crate reads
// as before.
pub use roomler_core::ws::{dispatcher, redis_pubsub, storage};

// FR-69 P5a — device presence and the agent-nudge machinery are the fleet
// module's; re-exported so every `crate::ws::{device_presence, rc_cluster}`
// path in this crate reads as before. Their functions take the module's
// state: host code hands them `AppState::fleet()`.
pub use roomler_ai_mod_fleet::nudge as rc_cluster;
pub use roomler_ai_mod_fleet::presence as device_presence;
