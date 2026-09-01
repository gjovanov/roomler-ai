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

pub mod derp;
pub mod derp_acl;
pub mod derp_cluster;
pub mod device_presence;
pub mod dispatcher;
pub mod ephemeral;
pub mod handler;
pub mod media_cluster;
pub mod org_relay;
pub mod overlay;
pub mod rc_cluster;
pub mod rc_relay;
pub mod redis_pubsub;
pub mod remote_control;
pub mod storage;
pub mod tunnel;
