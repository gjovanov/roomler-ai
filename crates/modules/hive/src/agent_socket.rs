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
//! reconnects as a build without Hive cannot be running anything: what it ran
//! ends, its pending stops end and its pending starts are lost. Each
//! connection decides on its own caps only: one a newer connection displaced
//! decides nothing.
//!
//! # What is over stays over
//!
//! A session the server ended while its device could not hear — its starter
//! removed, its org archived — is not pending anything, so reconcile sends it
//! nothing. The device learns from its own reports instead: one that says it
//! RUNS a session whose record is over is answered with a stop
//! ([`stop_if_over`]). That covers the replay a device sends on every
//! connection, so a removed member's session cannot outlive one reconnect.
//!
//! # What the device no longer runs (P1b)
//!
//! The opposite case: the record says a session runs, the device does not
//! run it. A restarted daemon holds no replay of what it ran, so every
//! connection also carries `rc:hive.manifest`, the sessions the device runs
//! NOW; the server ends each one it holds as running there that the list
//! leaves out ([`end_what_the_device_does_not_run`]). An unanswered start is
//! not "running" — reconcile owns it — and no manifest at all from a build
//! that runs Hive (an older one) changes nothing. A build WITHOUT Hive sends
//! none either, and runs none of them: reconcile ends them as an empty
//! manifest would ([`reconcile_on_connect`]).

use std::collections::HashSet;

use async_trait::async_trait;
use bson::{DateTime, doc, oid::ObjectId};
use dashmap::DashMap;
use roomler_ai_remote_control::{
    hive::{
        HiveJoinRefusal, HiveManifestEntry, HiveRefusal, HiveReplicaManifestEntry, HiveRunState,
        HiveTurnStatus, hive_limits,
    },
    signaling::{ClientMsg, ServerMsg},
};
use roomler_core::{AgentCtx, AgentMsgHandler, AgentSocketLifecycle};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::model::SessionStatus;
use crate::{HiveState, room};

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
    Turn {
        session_id: ObjectId,
        fence: u64,
        report: room::TurnReport,
    },
    /// P1a-2 — an approval opened or ended (`rc:hive.approval`).
    Approval {
        session_id: ObjectId,
        fence: u64,
        report: room::ApprovalReport,
    },
    /// P1b — the sessions the device runs now (`rc:hive.manifest`).
    Manifest(Vec<HiveManifestEntry>),
    /// P1j — a terminal session the device offers for a record
    /// (`rc:hive.adopt`).
    Adopt(crate::adopt::Offer),
    /// P2c-3 — a member's answer to its join (`rc:hive.replica.join_ack`).
    JoinAck {
        session_id: ObjectId,
        fence: u64,
        refused: Option<HiveJoinRefusal>,
        detail: Option<String>,
    },
    /// P2c-3 — where a member's copy ends (`rc:hive.replica.tip`).
    Tip(HiveReplicaManifestEntry),
    /// P2c-3 — every session the device holds a copy of
    /// (`rc:hive.replica.manifest`).
    ReplicaManifest(Vec<HiveReplicaManifestEntry>),
    /// P0d-2 — a viewer-peer frame (`rc:hive.view.*`). In the same queue:
    /// the device's answer must reach the browser before its candidates.
    View(ClientMsg),
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
            ClientMsg::HiveTurn {
                session_id,
                fence,
                turn,
                status,
                prompted_by,
                steps,
                duration_ms,
                cost_usd,
            } => Report::Turn {
                session_id,
                fence,
                report: room::TurnReport {
                    turn,
                    status,
                    prompted_by,
                    steps,
                    duration_ms,
                    cost_usd,
                },
            },
            ClientMsg::HiveApproval {
                session_id,
                fence,
                approval_id,
                turn,
                status,
                answered_by,
            } => Report::Approval {
                session_id,
                fence,
                report: room::ApprovalReport {
                    approval_id,
                    turn,
                    status,
                    answered_by,
                },
            },
            ClientMsg::HiveManifest { sessions } => Report::Manifest(sessions),
            ClientMsg::HiveAdopt {
                adopt_id,
                harness_session,
                keys,
                account,
                folder,
            } => Report::Adopt(crate::adopt::Offer {
                adopt_id,
                harness_session,
                keys,
                account,
                folder,
            }),
            ClientMsg::HiveReplicaJoinAck {
                session_id,
                fence,
                refused,
                detail,
            } => Report::JoinAck {
                session_id,
                fence,
                refused,
                detail,
            },
            ClientMsg::HiveReplicaTip {
                session_id,
                fence,
                seq,
                hash,
                checkpoint,
            } => Report::Tip(HiveReplicaManifestEntry {
                session_id,
                fence,
                seq,
                hash,
                checkpoint,
            }),
            ClientMsg::HiveReplicaManifest { sessions } => Report::ReplicaManifest(sessions),
            view @ (ClientMsg::HiveViewGrantAck { .. }
            | ClientMsg::HiveViewAnswer { .. }
            | ClientMsg::HiveViewIce { .. }
            | ClientMsg::HiveViewClosed { .. }) => Report::View(view),
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
        // This connection's sender in the hub: reconcile reads what THIS
        // connection advertised, never a newer one's entry.
        let conn = ctx.tx.clone();
        tokio::spawn(async move {
            if state.scope.serves(tenant_id) {
                reconcile_on_connect(&state, tenant_id, device_id, &conn).await;
                // P2c-3 — and the joins it has still to answer, if this
                // connection holds copies.
                crate::replica::reconcile_joins(&state, tenant_id, device_id, &conn).await;
            } else {
                end_unserved(&state, tenant_id, device_id).await;
            }
            drop(conn);
            apply_reports(&state, tenant_id, device_id, rx).await;
        });
    }

    /// Dropping the sender ends the drain task once the queue is empty.
    async fn closing(&self, ctx: &AgentCtx) {
        self.conns.remove(&ctx.conn_id);
    }
}

/// Apply one connection's reports, in order, until it closes.
async fn apply_reports(
    state: &HiveState,
    tenant_id: ObjectId,
    device_id: ObjectId,
    mut rx: mpsc::Receiver<Report>,
) {
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
                        // And say so in the session's room (P0d).
                        if let Ok(Some(s)) = state.sessions.find(session_id).await {
                            let text = match refused {
                                None => room::started_note(&s),
                                Some(_) => room::refused_note(&s),
                            };
                            room::note(state, &s, text).await;
                        }
                        // P2c-3 — it runs: its members may hold copies now.
                        if refused.is_none() {
                            crate::replica::send_joins(state, session_id).await;
                        }
                    }
                    Ok(false) => {
                        debug!(
                            session = %session_id, device = %device_id, fence,
                            "hive: a start answer matched no unanswered start — a duplicate, \
                             a stale fence, or not this device's session"
                        );
                        // It launched something the record gave up on (ended
                        // while the start was in flight, or `lost`).
                        if refused.is_none() {
                            stop_if_over(state, session_id, device_id, fence).await;
                        }
                    }
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
                        debug!(session = %session_id, state = run.as_str(), "hive: session state");
                        if run == HiveRunState::Ended
                            && let Ok(Some(s)) = state.sessions.find(session_id).await
                        {
                            room::note(state, &s, room::ended_note(&s)).await;
                            // The device withdraws its own open approvals as
                            // it ends; this covers a frame of theirs it lost.
                            room::withdraw_approvals(state, &s).await;
                        }
                    }
                    Ok(false) => {
                        debug!(
                            session = %session_id, device = %device_id, fence, state = run.as_str(),
                            "hive: a state report matched no live session at that fence on this device"
                        );
                        if run != HiveRunState::Ended {
                            stop_if_over(state, session_id, device_id, fence).await;
                        }
                    }
                    Err(e) => {
                        warn!(session = %session_id, %e, "hive: a state report was not recorded")
                    }
                }
            }
            Report::Turn {
                session_id,
                fence,
                report,
            } => {
                // The same ownership rule as every device report: only the
                // session's own device, at its current fence.
                match state.sessions.find(session_id).await {
                    Ok(Some(s))
                        if s.location.device_id == device_id
                            && u64::try_from(s.fence).ok() == Some(fence) =>
                    {
                        if !s.status.is_terminal() {
                            room::turn_stub(state, &s, report).await;
                            continue;
                        }
                        // Over on the record. The device may still FINISH the
                        // stub it has (the turn its stop interrupted), never
                        // open a new one; a turn it says is running, it stops.
                        let running = matches!(report.status, Some(HiveTurnStatus::Running) | None);
                        if running {
                            stop_if_over(state, session_id, device_id, fence).await;
                        } else if s.last_turn.is_some_and(|t| t.turn == report.turn) {
                            room::turn_stub(state, &s, report).await;
                        }
                    }
                    Ok(_) => debug!(
                        session = %session_id, device = %device_id, fence,
                        "hive: a turn report for no session of this device at that fence"
                    ),
                    Err(e) => warn!(session = %session_id, %e, "hive: a turn report was not read"),
                }
            }
            Report::Approval {
                session_id,
                fence,
                report,
            } => {
                // The ownership rule again: only the session's own device, at
                // its current fence. An opening in a session that is over is
                // refused by `room::approval`; an end always applies, so a
                // stub cannot be left saying "needs approval".
                match state.sessions.find(session_id).await {
                    Ok(Some(s))
                        if s.location.device_id == device_id
                            && u64::try_from(s.fence).ok() == Some(fence) =>
                    {
                        room::approval(state, &s, report).await;
                    }
                    Ok(_) => debug!(
                        session = %session_id, device = %device_id, fence,
                        "hive: an approval report for no session of this device at that fence"
                    ),
                    Err(e) => {
                        warn!(session = %session_id, %e, "hive: an approval report was not read")
                    }
                }
            }
            Report::Manifest(sessions) => {
                end_what_the_device_does_not_run(state, tenant_id, device_id, &sessions).await;
            }
            Report::JoinAck {
                session_id,
                fence,
                refused,
                detail,
            } => {
                crate::replica::on_join_ack(state, device_id, session_id, fence, refused, detail)
                    .await
            }
            Report::Tip(tip) => crate::replica::on_tip(state, device_id, tip).await,
            Report::ReplicaManifest(sessions) => {
                crate::replica::on_manifest(state, device_id, sessions).await
            }
            Report::Adopt(offer) => crate::adopt::offer(state, tenant_id, device_id, offer).await,
            Report::View(msg) => crate::view::on_device_frame(state, device_id, msg).await,
        }
    }
}

/// P1b — the device says which sessions it runs now. Every session the
/// server holds as running there that the list leaves out is over — the
/// daemon restarted, or the harness ended and the word was lost past its
/// replay — and nothing else would ever say so (finding 7). Ended here, with
/// its room told and its open approvals withdrawn. An oversized list is not
/// a device's, and changes nothing: the safe direction. A connection without
/// `hive` gets the empty list from [`reconcile_on_connect`].
async fn end_what_the_device_does_not_run(
    state: &HiveState,
    tenant_id: ObjectId,
    device_id: ObjectId,
    manifest: &[HiveManifestEntry],
) {
    if manifest.len() > hive_limits::MAX_MANIFEST {
        warn!(device = %device_id, entries = manifest.len(), "hive: an oversized manifest — ignored");
        return;
    }
    let runs: HashSet<(ObjectId, u64)> = manifest.iter().map(|e| (e.session_id, e.fence)).collect();
    let held = match state.sessions.running_on_device(tenant_id, device_id).await {
        Ok(held) => held,
        Err(e) => {
            warn!(device = %device_id, %e, "hive: the device's sessions were not read for its manifest");
            return;
        }
    };
    for s in held {
        let Some(sid) = s.id else { continue };
        if runs.contains(&(sid, u64::try_from(s.fence).unwrap_or_default())) {
            continue;
        }
        let stopping = s.status == SessionStatus::Stopping;
        match state
            .sessions
            .end_not_on_device(sid, device_id, s.fence, stopping)
            .await
        {
            Ok(true) => {
                info!(session = %sid, device = %device_id, "hive: the device no longer runs a session — ended");
                room::note_ended(state, sid).await;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(session = %sid, %e, "hive: a session the device no longer runs was not ended")
            }
        }
    }
}

/// The device says it RUNS a session — `session_id` at `fence` — that the
/// record says is over: tell it to stop. Nothing else would ever reach that
/// process (reconcile re-sends only what is pending), and the record is the
/// source of truth. A device answers a stop for a session it does not run with
/// `ended`, which changes nothing here, so this cannot loop.
async fn stop_if_over(state: &HiveState, session_id: ObjectId, device_id: ObjectId, fence: u64) {
    let s = match state.sessions.find(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            warn!(session = %session_id, %e, "hive: a report's session was not read");
            return;
        }
    };
    // Only the session's own device is told — a report naming someone
    // else's session stops nothing.
    if s.location.device_id != device_id || !s.status.is_terminal() {
        return;
    }
    let msg = ServerMsg::HiveStop {
        session_id,
        fence,
        reason: s.end_reason.unwrap_or_else(|| "ended".to_string()),
    };
    match state.fleet.rc_hub.push_hive(device_id, s.tenant_id, msg) {
        Ok(()) => info!(
            session = %session_id, device = %device_id, fence,
            "hive: the device still runs a session that is over — told to stop"
        ),
        Err(e) => {
            debug!(session = %session_id, %e, "hive: a stop for a session that is over was not sent")
        }
    }
}

/// P1g — agent sessions no longer serve this device's organization (it left
/// `hive.tenants`): nothing is re-sent, and what the device still runs for it
/// ends and is told to stop. The device's manifest, read next, stops anything
/// the push did not reach.
async fn end_unserved(state: &HiveState, tenant_id: ObjectId, device_id: ObjectId) {
    match crate::hooks::end_and_tell(
        state,
        doc! { "tenant_id": tenant_id, "location.device_id": device_id },
        "hive_not_enabled",
    )
    .await
    {
        Ok(0) => {}
        Ok(ended) => info!(
            tenant = %tenant_id,
            device = %device_id,
            ended,
            "hive: the organization is not served — its sessions on the device ended"
        ),
        Err(e) => {
            warn!(device = %device_id, %e, "hive: an unserved organization's sessions were not ended")
        }
    }
}

/// Re-send what a device missed while it was not connected here. `conn` is
/// this connection's sender in the hub ([`AgentCtx::tx`]).
pub(crate) async fn reconcile_on_connect(
    state: &HiveState,
    tenant_id: ObjectId,
    device_id: ObjectId,
    conn: &mpsc::Sender<ServerMsg>,
) {
    // Read once, for THIS connection. The hub records a connection's caps
    // before its hello runs (`fleet/src/socket.rs`), and answers only while
    // the slot is still this connection's. `None` = it is not: a newer
    // connection displaced it (and may not have recorded its caps yet), or it
    // is gone. Then this connection decides nothing, and the one that holds
    // the slot reconciles for itself.
    let supports_hive = state.fleet.rc_hub.agent_supports_hive_on(device_id, conn);
    // ⚠️ A build without `hive` runs no session, and sends no manifest to say
    // so: what the record holds as running here would read `idle` for ever.
    // Field, 2026-10-10: a Windows device ran an accepted session, then
    // updated itself to 0.4.123, whose build has no `hive`. A crash-loop
    // rollback to an older build, or a build made without `hive`, does the
    // same. So it gets the empty manifest, before what is pending is read: a
    // session ended here is pending nothing, and `end_now` and `mark_lost`
    // below miss one already ended, so none is noted twice. Only on
    // `Some(false)`: unknown is not "no".
    if supports_hive == Some(false) {
        end_what_the_device_does_not_run(state, tenant_id, device_id, &[]).await;
    }
    // ⚠️ `None` is not "no `hive`" here either. Read that way (the old
    // `unwrap_or(false)`), it ended the pending stops and lost the pending
    // starts of a device whose agent runs Hive: one whose socket dropped at
    // once, or whose newer connection had not recorded its caps yet. A
    // connection that no longer owns the hub's entry does nothing; one that
    // genuinely lacks `hive` still ends its stops and loses its starts below.
    let Some(runs_hive) = supports_hive else {
        return;
    };
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
    let now_ms = DateTime::now().timestamp_millis();

    for s in pending {
        let Some(sid) = s.id else { continue };
        let fence = u64::try_from(s.fence).unwrap_or_default();
        match s.status {
            SessionStatus::Stopping if !runs_hive => {
                if let Ok(true) = state
                    .sessions
                    .end_now(
                        sid,
                        "stopped",
                        "the device's agent no longer runs agent sessions",
                    )
                    .await
                {
                    room::note_ended(state, sid).await;
                }
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
                if let Ok(true) = state
                    .sessions
                    .mark_lost(
                        sid,
                        "the device reconnected with an agent that does not run agent sessions",
                    )
                    .await
                {
                    room::note_ended(state, sid).await;
                }
            }
            SessionStatus::Starting => {
                let age_ms = now_ms.saturating_sub(s.created_at.timestamp_millis());
                if age_ms > hive_limits::START_REDELIVERY_WINDOW_SECS * 1000 {
                    if let Ok(true) = state
                        .sessions
                        .mark_lost(
                            sid,
                            "the device came back after the start's redelivery window",
                        )
                        .await
                    {
                        room::note_ended(state, sid).await;
                    }
                    continue;
                }
                let Ok(owner) = state.users.base.find_by_id(s.owner_id).await else {
                    warn!(session = %sid, "hive: start not re-sent — its owner could not be read");
                    continue;
                };
                // P1e — its core memory first, the same frozen snapshot the
                // first start carried; a device without `hive-memory` gets
                // none, as it got none then.
                match state.brain.snapshot_of(sid).await {
                    Ok(Some(m)) => {
                        let frame = crate::routes::memory_frame(&m, fence);
                        let _ = state
                            .fleet
                            .rc_hub
                            .push_hive_memory(device_id, tenant_id, frame);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(session = %sid, %e, "hive: core memory not re-sent")
                    }
                }
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
                    replicated: crate::replica::replicated(&s),
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
