// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/// Ceiling on a single inbound WebSocket frame/message.
///
/// axum inherits tungstenite's defaults (64 MiB message, 16 MiB frame), and
/// nothing else bounds a post-upgrade frame: `tower_governor` is HTTP
/// middleware and never sees them, so an authenticated peer could make the
/// server buffer multi-MiB messages on every connection it opens.
///
/// Everything that legitimately crosses these sockets is control-plane —
/// signalling JSON, a netmap, an MTU-sized DERP packet — so 8 MiB is orders of
/// magnitude above real traffic while removing that amplification. Deliberately
/// generous rather than tight: a cap that is merely large is safe, whereas one
/// tuned close to the real maximum silently drops a big-but-valid netmap on the
/// day a fleet grows.
pub const MAX_WS_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

// FR-69 P5c — the host's transitional `remote`/`network` halves of the agent
// socket, registered on the core's `AgentSocketRegistry` under those ids.
pub mod agent_socket_host;
pub mod derp;
pub mod derp_acl;
pub mod derp_cluster;
pub mod ephemeral;
pub mod handler;
pub mod org_relay;
pub mod overlay;
pub mod rc_relay;
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
