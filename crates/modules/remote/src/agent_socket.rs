// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `remote`'s half of the agent socket (FR-69 P6): registered on the core's
//! `AgentSocketRegistry` under this module's id at init, so the fleet
//! module's socket dispatches the `Owner::Remote` messages here without
//! naming this crate.

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::{RecordingActivityEvent, RecordingActivityKind};
use roomler_ai_remote_control::permissions::Permissions;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_core::{AgentCtx, AgentMsgHandler};
use tracing::{debug, warn};

use crate::RemoteState;

/// `remote`'s half of the agent socket: the session-stats merge and (FR-85
/// P3) remote-recording activity. Everything else remote-owned (session
/// request, SDP, ICE, terminate) is the Hub's own dispatch, which the socket
/// runs on whatever a handler hands back.
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
            ClientMsg::RecordingActivity {
                session_id,
                kind,
                name,
                bytes,
                duration_ms,
                reason,
            } => {
                self.record_activity(ctx, session_id, kind, name, bytes, duration_ms, reason)
                    .await;
                None
            }
            other => Some(other),
        }
    }
}

impl RemoteAgentSocket {
    /// FR-85 P3 — record what a device reports about a remote recording,
    /// but ONLY for a session of THIS agent whose grant held `RECORD`: a
    /// device must not be able to write activity against a session that
    /// could not record, or against another device's. The live session is
    /// checked first; after it has ended (a recording's last report arrives
    /// after its session is gone) the stored `remote_sessions` row decides.
    #[allow(clippy::too_many_arguments)]
    async fn record_activity(
        &self,
        ctx: &AgentCtx,
        session_id: ObjectId,
        kind: RecordingActivityKind,
        name: Option<String>,
        bytes: Option<u64>,
        duration_ms: Option<u64>,
        reason: Option<String>,
    ) {
        let grant = match self.state.fleet.rc_hub.session_grant(session_id) {
            Some(g) => Some(g),
            None => self
                .state
                .remote_sessions
                .find_in_tenant(ctx.tenant_id, session_id)
                .await
                .ok()
                .map(|s| (s.agent_id, s.controller_user_id, s.permissions)),
        };
        let Some((agent_id, controller_user_id, permissions)) = grant else {
            debug!(agent = %ctx.agent_id, %session_id, "recording activity for an unknown session — dropped");
            return;
        };
        if agent_id != ctx.agent_id || !permissions.contains(Permissions::RECORD) {
            warn!(
                agent = %ctx.agent_id,
                %session_id,
                "recording activity for a session this device could not record — dropped"
            );
            return;
        }
        let clip = |s: Option<String>| {
            s.map(|t| t.chars().take(RecordingActivityEvent::MAX_TEXT).collect())
        };
        let event = RecordingActivityEvent {
            id: None,
            tenant_id: ctx.tenant_id,
            agent_id: ctx.agent_id,
            session_id,
            controller_user_id,
            kind,
            name: clip(name),
            bytes,
            duration_ms,
            reason: clip(reason),
            at: bson::DateTime::now(),
        };
        if let Err(e) = self.state.recording_activity.record(event).await {
            // Best-effort, like every activity log: never gates the session.
            debug!(agent = %ctx.agent_id, %e, "recording activity write failed");
        }
    }
}
