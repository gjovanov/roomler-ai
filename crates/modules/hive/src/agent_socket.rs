// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `hive`'s half of the agent socket: the device's `rc:hive.start_ack` and
//! `rc:hive.state`, dispatched here by `ClientMsg::namespace()`, and the
//! reconcile a device gets when it connects.
//!
//! # Ordered, and off the read loop
//!
//! A session's reports must apply in the order the device sent them —
//! `running`, `awaiting_approval`, `running`, `idle` arrive within
//! milliseconds of each other — and a database write must never stall the
//! socket's read loop, whose receive-liveness deadline would then reap a
//! healthy device. So each CONNECTION gets one queue and one task that drains
//! it in order; the handler only enqueues. (`conn_id`, never `agent_id`: two
//! connections of one device overlap during a displacement.)
//!
//! # Reconcile on connect
//!
//! The only delivery path for a device that was not connected when it was
//! told something — the remote-config rule: an offline device converges
//! through the same code as an online one. On every connection the device is
//! re-sent the stops it has not confirmed, and the starts it never answered
//! (within [`hive_limits::START_REDELIVERY_WINDOW_SECS`]; older ones are
//! `lost`, not launched an hour after someone gave up on them). A device that
//! reconnects as a build without Hive cannot be running anything: its
//! pending stops end and its pending starts are lost.

use async_trait::async_trait;
use bson::{DateTime, oid::ObjectId};
use dashmap::DashMap;
use roomler_ai_remote_control::{
    hive::{HiveRefusal, HiveRunState, hive_limits},
    signaling::{ClientMsg, ServerMsg},
};
use roomler_core::{AgentCtx, AgentMsgHandler, AgentSocketLifecycle};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::HiveState;
use crate::model::SessionStatus;

/// Reports queued per connection before new ones are dropped. A device
/// sends a handful per turn; a full queue means a flood or a stalled
/// database, and dropping is better than holding the read loop.
const REPORT_QUEUE: usize = 256;

/// Longest local account name kept from a device's answer.
const MAX_ACCOUNT_LEN: usize = 128;

enum Report {
    StartAck {
        session_id: ObjectId,
        fence: u64,
        refused: Option<HiveRefusal>,
        account: Option<String>,
        detail: Option<String>,
    },
    State {
        session_id: ObjectId,
        fence: u64,
        state: Option<HiveRunState>,
        detail: Option<String>,
    },
}

/// A device's words, kept short and on one line: they are shown to a person
/// and written to logs, and the device may be compromised.
pub(crate) fn clamp_words(s: Option<String>, max_chars: usize) -> Option<String> {
    let s = s?;
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect();
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

pub struct HiveAgentSocket {
    state: HiveState,
    conns: DashMap<String, mpsc::Sender<Report>>,
}

impl HiveAgentSocket {
    pub fn new(state: HiveState) -> Self {
        Self {
            state,
            conns: DashMap::new(),
        }
    }
}

#[async_trait]
impl AgentMsgHandler for HiveAgentSocket {
    async fn handle(&self, ctx: &AgentCtx, msg: ClientMsg) -> Option<ClientMsg> {
        let report = match msg {
            ClientMsg::HiveStartAck {
                session_id,
                fence,
                refused,
                account,
                detail,
            } => Report::StartAck {
                session_id,
                fence,
                refused,
                account,
                detail,
            },
            ClientMsg::HiveState {
                session_id,
                fence,
                state,
                detail,
            } => Report::State {
                session_id,
                fence,
                state,
                detail,
            },
            other => return Some(other),
        };
        let tx = self.conns.get(&ctx.conn_id).map(|e| e.value().clone());
        match tx {
            Some(tx) => {
                if tx.try_send(report).is_err() {
                    warn!(
                        agent_id = %ctx.agent_id,
                        "hive: report queue full or closed — a device report was dropped"
                    );
                }
            }
            None => warn!(
                agent_id = %ctx.agent_id,
                "hive: report on a connection that never said hello — dropped"
            ),
        }
        None
    }
}

#[async_trait]
impl AgentSocketLifecycle for HiveAgentSocket {
    async fn hello(&self, ctx: &AgentCtx) {
        let (tx, rx) = mpsc::channel(REPORT_QUEUE);
        self.conns.insert(ctx.conn_id.clone(), tx);
        let state = self.state.clone();
        let (tenant_id, device_id) = (ctx.tenant_id, ctx.agent_id);
        tokio::spawn(async move {
            reconcile_on_connect(&state, tenant_id, device_id).await;
            apply_reports(&state, device_id, rx).await;
        });
    }

    /// Dropping the sender ends the drain task once the queue is empty.
    async fn closing(&self, ctx: &AgentCtx) {
        self.conns.remove(&ctx.conn_id);
    }
}

/// Apply one connection's reports, in order, until it closes.
async fn apply_reports(state: &HiveState, device_id: ObjectId, mut rx: mpsc::Receiver<Report>) {
    while let Some(report) = rx.recv().await {
        match report {
            Report::StartAck {
                session_id,
                fence,
                refused,
                account,
                detail,
            } => {
                let detail = clamp_words(detail, hive_limits::MAX_DETAIL_LEN);
                let applied = match refused {
                    None => {
                        let account = clamp_words(account, MAX_ACCOUNT_LEN);
                        state
                            .sessions
                            .accept(session_id, device_id, fence, account)
                            .await
                    }
                    Some(word) => {
                        state
                            .sessions
                            .refuse(session_id, device_id, fence, word.as_str(), detail)
                            .await
                    }
                };
                match applied {
                    Ok(true) => {
                        info!(
                            session = %session_id, device = %device_id,
                            refused = refused.map(HiveRefusal::as_str),
                            "hive: the device answered a start"
                        );
                        // AFTER the write: the woken caller re-reads the record.
                        state.start_acks.deliver(session_id, device_id);
                    }
                    Ok(false) => debug!(
                        session = %session_id, device = %device_id, fence,
                        "hive: a start answer matched no unanswered start — a duplicate, \
                         a stale fence, or not this device's session"
                    ),
                    Err(e) => {
                        warn!(session = %session_id, %e, "hive: a start answer was not recorded")
                    }
                }
            }
            Report::State {
                session_id,
                fence,
                state: run,
                detail,
            } => {
                let Some(run) = run else {
                    debug!(
                        session = %session_id,
                        "hive: a run state this build cannot name — keeping what it knew"
                    );
                    continue;
                };
                let applied = match run {
                    HiveRunState::Ended => {
                        let detail = clamp_words(detail, hive_limits::MAX_DETAIL_LEN);
                        state
                            .sessions
                            .report_ended(session_id, device_id, fence, detail)
                            .await
                    }
                    other => {
                        state
                            .sessions
                            .report_state(session_id, device_id, fence, other)
                            .await
                    }
                };
                match applied {
                    Ok(true) => {
                        debug!(session = %session_id, state = run.as_str(), "hive: session state")
                    }
                    Ok(false) => debug!(
                        session = %session_id, device = %device_id, fence, state = run.as_str(),
                        "hive: a state report matched no live session at that fence on this device"
                    ),
                    Err(e) => {
                        warn!(session = %session_id, %e, "hive: a state report was not recorded")
                    }
                }
            }
        }
    }
}

/// Re-send what a device missed while it was not connected here.
pub(crate) async fn reconcile_on_connect(
    state: &HiveState,
    tenant_id: ObjectId,
    device_id: ObjectId,
) {
    let pending = match state.sessions.needing_delivery(tenant_id, device_id).await {
        Ok(p) => p,
        Err(e) => {
            warn!(device = %device_id, %e, "hive: reconcile-on-connect could not read sessions");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }
    let runs_hive = state
        .fleet
        .rc_hub
        .agent_supports_hive(device_id)
        .unwrap_or(false);
    let now_ms = DateTime::now().timestamp_millis();

    for s in pending {
        let Some(sid) = s.id else { continue };
        let fence = u64::try_from(s.fence).unwrap_or_default();
        match s.status {
            SessionStatus::Stopping if !runs_hive => {
                let _ = state
                    .sessions
                    .end_now(
                        sid,
                        "stopped",
                        "the device's agent no longer runs agent sessions",
                    )
                    .await;
            }
            SessionStatus::Stopping => {
                let msg = ServerMsg::HiveStop {
                    session_id: sid,
                    fence,
                    reason: "owner".to_string(),
                };
                match state.fleet.rc_hub.push_hive(device_id, tenant_id, msg) {
                    Ok(()) => {
                        info!(session = %sid, device = %device_id, "hive: stop re-sent on connect")
                    }
                    Err(e) => debug!(session = %sid, %e, "hive: stop not re-sent"),
                }
            }
            SessionStatus::Starting if !runs_hive => {
                let _ = state
                    .sessions
                    .mark_lost(
                        sid,
                        "the device reconnected with an agent that does not run agent sessions",
                    )
                    .await;
            }
            SessionStatus::Starting => {
                let age_ms = now_ms.saturating_sub(s.created_at.timestamp_millis());
                if age_ms > hive_limits::START_REDELIVERY_WINDOW_SECS * 1000 {
                    let _ = state
                        .sessions
                        .mark_lost(
                            sid,
                            "the device came back after the start's redelivery window",
                        )
                        .await;
                    continue;
                }
                let Ok(owner) = state.users.base.find_by_id(s.owner_id).await else {
                    warn!(session = %sid, "hive: start not re-sent — its owner could not be read");
                    continue;
                };
                // Idempotent on the device by session id + fence: one that
                // launched it and lost only the answer says `accepted` again.
                let msg = ServerMsg::HiveStart {
                    session_id: sid,
                    harness: s.harness.id.clone(),
                    harness_session: s.harness.session.clone(),
                    fence,
                    folder: s.location.folder.clone(),
                    user_id: s.owner_id,
                    user_email: owner.email,
                    caller: owner.display_name,
                    resume: false,
                };
                match state.fleet.rc_hub.push_hive(device_id, tenant_id, msg) {
                    Ok(()) => {
                        info!(session = %sid, device = %device_id, "hive: unanswered start re-sent on connect")
                    }
                    Err(e) => debug!(session = %sid, %e, "hive: start not re-sent"),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_devices_words_are_one_short_line_or_nothing() {
        assert_eq!(
            clamp_words(Some("no such dir\n\u{1b}[2Jcleared".into()), 100).as_deref(),
            Some("no such dir  [2Jcleared")
        );
        assert_eq!(clamp_words(Some("x".repeat(600)), 512).unwrap().len(), 512);
        assert_eq!(clamp_words(Some(" \n\t ".into()), 100), None);
        assert_eq!(clamp_words(None, 100), None);
        // Cut on characters, never inside one.
        assert_eq!(clamp_words(Some("ééé".into()), 2).as_deref(), Some("éé"));
    }
}
