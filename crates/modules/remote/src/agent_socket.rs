// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `remote`'s half of the agent socket (FR-69 P6): registered on the core's
//! `AgentSocketRegistry` under this module's id at init, so the fleet
//! module's socket dispatches the `Owner::Remote` messages here without
//! naming this crate.

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_core::{AgentCtx, AgentMsgHandler};
use tracing::debug;

use crate::RemoteState;

/// `remote`'s half of the agent socket: the session-stats merge. Everything
/// else remote-owned (session request, SDP, ICE, terminate) is the Hub's own
/// dispatch, which the socket runs on whatever a handler hands back.
pub struct RemoteAgentSocket {
    state: RemoteState,
}

impl RemoteAgentSocket {
    pub fn new(state: RemoteState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl AgentMsgHandler for RemoteAgentSocket {
    async fn handle(&self, ctx: &AgentCtx, msg: ClientMsg) -> Option<ClientMsg> {
        match msg {
            ClientMsg::SessionStats {
                session_id,
                bytes_sent,
                bytes_recv,
                fps,
                rtt_ms,
                keyframe_requests,
                input_events,
                shared_seconds,
                mixed_dial_seconds,
            } => {
                if self.state.settings.stats.enabled
                    && let Ok(sid) = ObjectId::parse_str(&session_id)
                {
                    let stats = roomler_ai_remote_control::models::SessionStats {
                        bytes_sent,
                        bytes_recv,
                        peak_fps: fps,
                        avg_rtt_ms: rtt_ms,
                        keyframe_requests,
                        input_events,
                        shared_seconds,
                        mixed_dial_seconds,
                    };
                    if let Err(e) = self
                        .state
                        .remote_sessions
                        .merge_live_stats(sid, ctx.agent_id, &stats)
                        .await
                    {
                        debug!(agent = %ctx.agent_id, %e, "session stats merge failed");
                    }
                }
                None
            }
            // FR-52 P3c — the device's answers to a relayed login. Delivered
            // only to a waiter registered for THIS agent (`ctx.agent_id` is the
            // authenticated socket's, never the frame's); anything else lands
            // nowhere. Consumed either way — never handed to the Hub.
            ClientMsg::ExtauthKe2 { attempt_id, ke2 } => {
                let answer = crate::extauth::DeviceAnswer::Ke2(ke2);
                if !self
                    .state
                    .extauth
                    .deliver(&attempt_id, ctx.agent_id, answer)
                {
                    debug!(agent = %ctx.agent_id, %attempt_id, "extauth: KE2 for no waiting step");
                }
                None
            }
            ClientMsg::ExtauthOutcome {
                attempt_id,
                refused,
                retry_after_secs,
            } => {
                let answer = crate::extauth::DeviceAnswer::Outcome {
                    refused,
                    retry_after_secs,
                };
                if !self
                    .state
                    .extauth
                    .deliver(&attempt_id, ctx.agent_id, answer)
                {
                    debug!(agent = %ctx.agent_id, %attempt_id, "extauth: outcome for no waiting step");
                }
                None
            }
            other => Some(other),
        }
    }
}
