// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 — the hub's half of a remote session that outlives its device's
//! control socket (`docs/fr/FR-138-the-session-outlives-its-control-socket.md`
//! §4.4). The hub keeps nothing across a gap: the session's row is its state
//! between pods, and what is here is the in-memory side of each step.
//!
//! * **The detach** ([`Hub::detach_agent_sessions`]): a connection that was
//!   announced a window loses its socket. Each LIVE session leaves this hub
//!   without ending: its row is marked `detached_at` and `resume_by`, fenced
//!   at the number the hub held; its FR-115 slot is kept; its viewer is told
//!   `rc:session.detached`. A session not yet live, and an external one
//!   (D3), end as they always have.
//! * **The claim's bookkeeping**: registration holds a place in the device's
//!   capacity for each session the hello claims back ([`Registration`]). The
//!   module that owns remote sessions decides each claim from the row and the
//!   gate, and answers it here: it rebuilds the session
//!   ([`Hub::resume_claimed`]) and then confirms it ([`Hub::confirm_resumed`])
//!   or, when an end won the race, drops it ([`Hub::drop_resumed`]); or it
//!   refuses it ([`Hub::refuse_claim`]). Whatever is still unanswered once
//!   the hello's lifecycles have run is refused by the socket
//!   ([`Hub::refuse_unanswered_claims`]).
//! * **An end for a session no hub holds** ([`Hub::record_end`]): the claim's
//!   refusals and the registration read write the row through it.
//!
//! The hub stays DB-agnostic: every write here goes through the audit sink,
//! whose projection applies the fencing (`roomler_ai_remote_control::audit`).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use bson::DateTime;
use bson::oid::ObjectId;
use parking_lot::Mutex;
use roomler_ai_remote_control::models::{AuditKind, EndReason, EndSource, SessionPhase};
use roomler_ai_remote_control::permissions::Permissions;
use roomler_ai_remote_control::session::{ClientTx, LiveSession};
use roomler_ai_remote_control::signaling::ServerMsg;
use roomler_core::cluster::session_slots::Lease;
use tokio::sync::mpsc;
use tracing::info;

use super::{AgentCancel, Hub, ptr_eq};

/// The most sessions one hello may claim, whatever capacity the device
/// advertises: each claim costs a few reads of its row and the gate.
pub(super) const MAX_CLAIMS: usize = 16;

/// How many ids of a hello's `kept_sessions` are looked at at all, each
/// answered: a device never keeps more than its capacity, so past this a
/// hello is not a device's. Under half the connection's channel, so the
/// answers never crowd out what follows them.
const MAX_KEPT_READ: usize = 32;

/// What a hello said about resuming, as the agent socket read it.
#[derive(Debug, Clone, Default)]
pub struct ResumeHello {
    /// The device advertises `rc-resume` (equality-matched).
    pub advertised: bool,
    /// The window this connection is announced, in seconds: `Some` only for a
    /// device that advertises the verb and is in the resume scope while
    /// `rc.session_resume_secs` is above 0.
    pub window: Option<u32>,
    /// The sessions the hello claims back (`kept_sessions`).
    pub kept: Vec<ObjectId>,
}

/// What [`Hub::register_agent_resuming`] registered.
pub struct Registration {
    /// As [`Hub::register_agent`] answers them.
    pub tx: ClientTx,
    pub cancel: Arc<AgentCancel>,
    pub rx: mpsc::Receiver<ServerMsg>,
    /// The claims the hub holds a place for, in the hello's order, each once.
    pub claims: Vec<ObjectId>,
    /// The claims refused at once, each owed `rc:terminate`: no window was
    /// announced, or the hello claimed more than the device's capacity.
    pub refused: Vec<ObjectId>,
    /// The sessions the hub held for an earlier connection of the device and
    /// ended at this registration (its in-memory reap).
    pub reaped: Vec<ObjectId>,
    /// The hub's clock at the registration.
    pub registered_at: DateTime,
}

/// A session a claim committed, as the hub rebuilds it: from its row and the
/// gate the claim ran again.
#[derive(Debug, Clone)]
pub struct ResumedSession {
    pub session_id: ObjectId,
    pub tenant_id: ObjectId,
    pub controller_user_id: ObjectId,
    pub permissions: Permissions,
    pub created_at: DateTime,
    pub started_at: DateTime,
    /// The fencing number the claim committed.
    pub attach_seq: u32,
    /// What is left of the org's maximum length, counted from `started_at`,
    /// and the limit itself; `None` = no limit.
    pub max_duration: Option<(Duration, Duration)>,
    /// The FR-115 lease the claim took again under the session's own id.
    pub slot: Option<Lease>,
}

/// What [`Hub::resume_claimed`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resumed {
    /// Built from the row: no pod held it.
    Rebuilt,
    /// This hub still held it for the device's earlier connection (a socket a
    /// middlebox kept half-open): it keeps its controller, at the new number.
    Reattached,
    /// The claim is no longer this connection's to answer: the connection
    /// was displaced or closed while the claim was decided.
    NotPending,
}

/// How an end is written to the session's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndWrite {
    /// An end someone asked for: the write every end made before FR-138, at
    /// any fencing number.
    Asked,
    /// An end the hub decided on its own: fenced at the number it holds.
    Own,
    /// No write: the row already holds the end this one follows.
    Nothing,
}

/// How the row of a session no hub holds is ended ([`Hub::record_end`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowEnd {
    /// `Some(n)`: an end the hub decided on its own, landing only while the
    /// row is open and still at `n`. `None`: an end someone asked for, at any
    /// number.
    pub attach_seq: Option<u32>,
    /// An end nobody saw: the row's own last clock, and who inferred it.
    pub inferred: Option<(DateTime, EndSource)>,
}

impl RowEnd {
    /// An end someone asked for (an admin's, the controller's): P1b-2's
    /// write for a session no hub holds.
    pub fn asked() -> Self {
        Self {
            attach_seq: None,
            inferred: None,
        }
    }

    /// An end decided now, at the number `seq` the decider read.
    pub fn fenced(seq: u32) -> Self {
        Self {
            attach_seq: Some(seq),
            inferred: None,
        }
    }

    /// An end nobody saw, at the number read, dated `at` (the row's own last
    /// clock) and marked as `source`'s inference.
    pub fn inferred(seq: u32, at: DateTime, source: EndSource) -> Self {
        Self {
            attach_seq: Some(seq),
            inferred: Some((at, source)),
        }
    }
}

/// Split a hello's claims into the ones the hub holds a place for and the
/// ones it refuses at once. With no window every claim is refused: the server
/// keeps nothing it did not announce. Otherwise each id once, in the hello's
/// order, up to the device's capacity (and [`MAX_CLAIMS`]); the rest are
/// refused.
pub(super) fn sort_claims(
    window: Option<u32>,
    kept: Vec<ObjectId>,
    max_sessions: u8,
) -> (Vec<ObjectId>, Vec<ObjectId>) {
    let mut seen = HashSet::new();
    let mut unique: Vec<ObjectId> = kept
        .into_iter()
        .take(MAX_KEPT_READ)
        .filter(|sid| seen.insert(*sid))
        .collect();
    if window.is_none() {
        return (Vec::new(), unique);
    }
    let room = usize::from(max_sessions.max(1)).min(MAX_CLAIMS);
    let refused = if unique.len() > room {
        unique.split_off(room)
    } else {
        Vec::new()
    };
    (unique, refused)
}

impl Hub {
    /// FR-138 — the drop of a connection that was announced a window. Each
    /// LIVE session of the device leaves this hub without ending (the device
    /// keeps its peer), until `resume_by` = now plus the window; a session not
    /// yet live, and an external one (D3), end as they always have.
    pub(super) fn detach_agent_sessions(&self, agent_id: ObjectId, window_secs: u32) {
        // Collected first: the map is written below.
        let mine: Vec<(ObjectId, bool)> = self
            .inner
            .sessions
            .iter()
            .filter_map(|e| {
                let s = e.value().lock();
                if s.agent_id == agent_id {
                    Some((*e.key(), s.phase == SessionPhase::Active && !s.external))
                } else {
                    None
                }
            })
            .collect();
        if mine.is_empty() {
            return;
        }
        let resume_by = DateTime::from_millis(
            DateTime::now()
                .timestamp_millis()
                .saturating_add(i64::from(window_secs) * 1000),
        );
        for (session_id, live) in mine {
            if live {
                self.detach(session_id, resume_by);
            } else {
                let _ = self.end_own(session_id, EndReason::AgentDisconnect, None);
            }
        }
    }

    /// One live session out of this hub, kept: its row is marked detached
    /// (fenced at the number held), its viewer told, and its slot left held.
    fn detach(&self, session_id: ObjectId, resume_by: DateTime) {
        let Some((_, arc)) = self.inner.sessions.remove(&session_id) else {
            return;
        };
        // FR-115 — the slot stays the session's: only an end gives one back.
        // Nobody renews it from here. The claim takes it again under the
        // session's own id, and a lease nobody renews lapses on its own
        // (`SLOT_TTL_SECS`, 90 s, longer than D1's 60 s window).
        self.inner.slotted.remove(&session_id);
        let (agent_id, tenant_id, attach_seq, controller_tx) = {
            let s = arc.lock();
            (
                s.agent_id,
                s.tenant_id,
                s.attach_seq,
                s.controller_tx.clone(),
            )
        };
        let resume_by_ms = resume_by.timestamp_millis();
        if let Some(tx) = controller_tx {
            let _ = tx.try_send(ServerMsg::SessionDetached {
                session_id,
                resume_by: u64::try_from(resume_by_ms).unwrap_or(0),
            });
        }
        self.audit(
            session_id,
            agent_id,
            tenant_id,
            AuditKind::SessionDetached {
                attach_seq,
                resume_by,
            },
        );
        info!(
            session = %session_id.to_hex(),
            %agent_id,
            attach_seq,
            resume_by_ms,
            "rc session detached: the device's control socket dropped, and it may claim the session back until resume_by"
        );
    }

    /// FR-138 — announce `secs` to the device's connection on this pod, and
    /// remember it on the entry; 0 withdraws it, so the connection's loss
    /// ends its sessions again. `false` when this pod does not hold the
    /// device's socket, or the connection does not advertise `rc-resume`
    /// (it could not read the frame). The socket announces at registration;
    /// P1b-2's re-announce on a scope change calls this on every pod.
    pub fn announce_resume_window(&self, agent_id: ObjectId, secs: u32) -> bool {
        let Some(mut a) = self.inner.agents.get_mut(&agent_id) else {
            return false;
        };
        if !a.advertises_resume {
            return false;
        }
        a.resume_window = (secs > 0).then_some(secs);
        let _ = a.tx.try_send(ServerMsg::AgentResumeWindow { secs });
        true
    }

    /// FR-138 — the window announced to the device's connection on this pod.
    pub fn agent_resume_window(&self, agent_id: ObjectId) -> Option<u32> {
        self.inner
            .agents
            .get(&agent_id)
            .and_then(|a| a.resume_window)
    }

    /// The session, when this hub holds it for `agent_id`.
    fn held_for(
        &self,
        session_id: ObjectId,
        agent_id: ObjectId,
    ) -> Option<Arc<Mutex<LiveSession>>> {
        let arc = self
            .inner
            .sessions
            .get(&session_id)
            .map(|e| e.value().clone())?;
        let mine = arc.lock().agent_id == agent_id;
        mine.then_some(arc)
    }

    /// Take `session_id` out of `conn`'s unanswered claims; `None` when it is
    /// not one (the connection was displaced or closed, or it was answered).
    /// `count` moves its place into the device's count of live sessions.
    fn take_claim(
        &self,
        agent_id: ObjectId,
        conn: &ClientTx,
        session_id: ObjectId,
        count: bool,
    ) -> Option<ObjectId> {
        let mut a = self.inner.agents.get_mut(&agent_id)?;
        if !ptr_eq(&a.tx, conn) {
            return None;
        }
        let pos = a.pending_claims.iter().position(|s| *s == session_id)?;
        a.pending_claims.remove(pos);
        if count {
            a.active_sessions = a.active_sessions.saturating_add(1);
        }
        Some(a.tenant_id)
    }

    /// FR-138 — a claim committed: rebuild the session on this hub, counted
    /// in the device's capacity from here (its place moves from the claims),
    /// its slot renewed from here, and its maximum length armed again from
    /// `started_at`. Nothing is sent yet: the caller reads the row once more
    /// and then confirms or drops it, so an end that landed meanwhile wins.
    pub fn resume_claimed(
        &self,
        agent_id: ObjectId,
        conn: &ClientTx,
        r: ResumedSession,
    ) -> Resumed {
        // Read before the agent's entry is locked: the entry's guard is never
        // held while a session's lock is taken.
        let held = self.held_for(r.session_id, agent_id);
        match self.take_claim(agent_id, conn, r.session_id, true) {
            Some(tenant_id) if tenant_id == r.tenant_id => {}
            Some(_) => {
                // Never: the claim read the row in the device's tenant. Give
                // the place back and answer nothing; the socket refuses it.
                if let Some(mut a) = self.inner.agents.get_mut(&agent_id) {
                    a.active_sessions = a.active_sessions.saturating_sub(1);
                }
                return Resumed::NotPending;
            }
            None => return Resumed::NotPending,
        }
        let outcome = match held {
            Some(arc) => {
                arc.lock().attach_seq = r.attach_seq;
                Resumed::Reattached
            }
            None => {
                let live = LiveSession::resumed(
                    r.session_id,
                    agent_id,
                    r.tenant_id,
                    r.controller_user_id,
                    r.permissions,
                    r.created_at,
                    r.started_at,
                    r.attach_seq,
                );
                self.inner
                    .sessions
                    .insert(r.session_id, Arc::new(Mutex::new(live)));
                Resumed::Rebuilt
            }
        };
        if let Some(lease) = r.slot {
            self.inner.slotted.insert(r.session_id, lease);
        }
        if let Some((left, limit)) = r.max_duration {
            self.arm_max_duration(r.session_id, r.attach_seq, left, limit);
        }
        outcome
    }

    /// FR-138 — the resume stands (the row was still open once the session
    /// was rebuilt): tell the device, and the viewer when this hub still had
    /// one, `rc:session.resumed`, and audit it. `false` when the session ended
    /// meanwhile (a kick reached it), whose `rc:terminate` is already sent.
    pub fn confirm_resumed(
        &self,
        agent_id: ObjectId,
        conn: &ClientTx,
        session_id: ObjectId,
        attach_seq: u32,
    ) -> bool {
        let Some(arc) = self.held_for(session_id, agent_id) else {
            return false;
        };
        let (tenant_id, held_seq, controller_tx) = {
            let s = arc.lock();
            (s.tenant_id, s.attach_seq, s.controller_tx.clone())
        };
        if held_seq != attach_seq {
            return false;
        }
        let msg = ServerMsg::SessionResumed { session_id };
        let _ = conn.try_send(msg.clone());
        if let Some(tx) = controller_tx {
            let _ = tx.try_send(msg);
        }
        self.audit(
            session_id,
            agent_id,
            tenant_id,
            AuditKind::SessionResumed { attach_seq },
        );
        info!(session = %session_id.to_hex(), %agent_id, attach_seq, "rc session resumed: the device claimed it back");
        true
    }

    /// FR-138 — an end won the race: the row ended after the claim committed
    /// (an end written while no hub held the session). The rebuilt session
    /// leaves this hub, both ends are told `reason`, and nothing more is
    /// written: the row holds that end.
    pub fn drop_resumed(&self, session_id: ObjectId, reason: EndReason) {
        let _ = self.end_session(session_id, reason, None, EndWrite::Nothing);
    }

    /// FR-138 — refuse a claim: its place in the capacity is given back and
    /// the device is told `rc:terminate` with `reason` (and `limit`). When
    /// this hub still held the session for the device's earlier connection,
    /// that copy ends, fenced at the number it holds; otherwise `row`, when
    /// the caller found the row still open, is written as its end. `false`
    /// when the claim is no longer this connection's to answer.
    pub fn refuse_claim(
        &self,
        agent_id: ObjectId,
        conn: &ClientTx,
        session_id: ObjectId,
        reason: EndReason,
        limit: Option<&'static str>,
        row: Option<RowEnd>,
    ) -> bool {
        let held = self.held_for(session_id, agent_id);
        // A copy held for the earlier connection was never counted on this
        // one: count it, so that its end below gives back what it takes.
        let Some(tenant_id) = self.take_claim(agent_id, conn, session_id, held.is_some()) else {
            return false;
        };
        info!(session = %session_id.to_hex(), %agent_id, ?reason, "rc session claim refused");
        if held.is_some() {
            let _ = self.end_session(session_id, reason, limit, EndWrite::Own);
            return true;
        }
        let _ = conn.try_send(ServerMsg::Terminate {
            session_id,
            reason,
            limit: limit.map(str::to_string),
        });
        if let Some(end) = row {
            self.record_end(session_id, agent_id, tenant_id, reason, limit, end);
        }
        true
    }

    /// FR-138 — every claim of `conn` still unanswered once the hello's
    /// lifecycles have run: refused with `agent_disconnect`, and nothing
    /// written (nothing read the row). The module that answers claims may not
    /// be mounted, or may have failed part way. The ids refused.
    pub fn refuse_unanswered_claims(&self, agent_id: ObjectId, conn: &ClientTx) -> Vec<ObjectId> {
        let pending: Vec<ObjectId> = match self.inner.agents.get(&agent_id) {
            Some(a) if ptr_eq(&a.tx, conn) => a.pending_claims.clone(),
            _ => return Vec::new(),
        };
        pending
            .into_iter()
            .filter(|sid| {
                self.refuse_claim(agent_id, conn, *sid, EndReason::AgentDisconnect, None, None)
            })
            .collect()
    }

    /// FR-138 — write an end to the row of a session this hub does not hold,
    /// through the audit sink like every end, so the projection applies the
    /// fencing (`end`). The claim's refusals and the registration read use
    /// it; P1b-2's end for a session no hub holds (an admin's, the
    /// controller's) is [`RowEnd::asked`].
    pub fn record_end(
        &self,
        session_id: ObjectId,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        reason: EndReason,
        limit: Option<&'static str>,
        end: RowEnd,
    ) {
        let (ended_at, end_source) = match end.inferred {
            Some((at, source)) => (Some(at), Some(source)),
            None => (None, None),
        };
        self.audit(
            session_id,
            agent_id,
            tenant_id,
            AuditKind::SessionEnded {
                reason,
                limit: limit.map(str::to_string),
                attach_seq: end.attach_seq,
                ended_at,
                end_source,
            },
        );
    }

    /// FR-101 P4b — end the session at the org's maximum length (`limit`),
    /// `after` from now: armed when the session goes live, and again by a
    /// claim that resumes it, from `started_at`. FR-138 — it fires only while
    /// this hub holds the session at the number it was armed at: a session id
    /// survives a resume, and the resume arms its own.
    pub(super) fn arm_max_duration(
        &self,
        session_id: ObjectId,
        attach_seq: u32,
        after: Duration,
        limit: Duration,
    ) {
        let hub = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let held_at = hub
                .inner
                .sessions
                .get(&session_id)
                .map(|e| e.value().lock().attach_seq);
            if held_at == Some(attach_seq) {
                info!(
                    session = %session_id.to_hex(),
                    limit_secs = limit.as_secs(),
                    "rc session reached the org's maximum length — ending it"
                );
                let _ = hub.end_own(session_id, EndReason::IdleTimeout, Some("max_duration"));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::Client;
    use roomler_ai_remote_control::audit::AuditSink;
    use roomler_ai_remote_control::models::{ConsentMode, OsKind};

    async fn hub() -> Hub {
        // As the hub's own tests: Mongo is never contacted unless the sink
        // flushes, and a failed write there is only logged.
        let client = Client::with_uri_str("mongodb://localhost:27017")
            .await
            .expect("mongo for tests");
        let (audit, _h) = AuditSink::spawn(client.database("rc_test"));
        Hub::new(audit, None)
    }

    fn hello(window: Option<u32>, kept: Vec<ObjectId>) -> ResumeHello {
        ResumeHello {
            advertised: true,
            window,
            kept,
        }
    }

    fn register(
        hub: &Hub,
        agent: ObjectId,
        tenant: ObjectId,
        max: u8,
        h: ResumeHello,
    ) -> Registration {
        hub.register_agent_resuming(
            agent,
            tenant,
            ObjectId::new(),
            OsKind::Linux,
            max,
            true,
            false,
            h,
        )
    }

    /// A session requested by `user` and its controller's receiver.
    fn open(hub: &Hub, agent: ObjectId, user: ObjectId) -> (ObjectId, mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = mpsc::channel(32);
        let sid = hub
            .create_session(
                agent,
                user,
                "Viewer".into(),
                tx,
                Permissions::default(),
                Vec::new(),
                None,
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        (sid, rx)
    }

    async fn live(hub: &Hub, sid: ObjectId) {
        hub.deliver_consent(sid, true, None).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        hub.with_session(sid, |s| s.transition(SessionPhase::Active))
            .unwrap();
    }

    fn drain(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<ServerMsg> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    fn terminated(msgs: &[ServerMsg], sid: ObjectId) -> Option<EndReason> {
        msgs.iter().find_map(|m| match m {
            ServerMsg::Terminate {
                session_id, reason, ..
            } if *session_id == sid => Some(*reason),
            _ => None,
        })
    }

    #[test]
    fn claims_are_held_only_under_a_window_once_each_and_up_to_the_capacity() {
        let ids: Vec<ObjectId> = (0..4).map(|_| ObjectId::new()).collect();
        let (held, refused) = sort_claims(None, ids.clone(), 2);
        assert!(held.is_empty(), "no window holds nothing");
        assert_eq!(refused, ids, "and refuses every claim");

        let twice = vec![ids[0], ids[1], ids[0]];
        let (held, refused) = sort_claims(Some(60), twice, 2);
        assert_eq!(held, vec![ids[0], ids[1]], "each once, in order");
        assert!(refused.is_empty(), "a repeat is no claim of its own");

        let (held, refused) = sort_claims(Some(60), ids.clone(), 2);
        assert_eq!(held, ids[..2].to_vec());
        assert_eq!(refused, ids[2..].to_vec(), "past the capacity");

        let many: Vec<ObjectId> = (0..40).map(|_| ObjectId::new()).collect();
        let (held, _) = sort_claims(Some(60), many, u8::MAX);
        assert_eq!(held.len(), MAX_CLAIMS);
    }

    /// The drop of an announced connection keeps its live session (out of
    /// the hub, the viewer told `rc:session.detached`, never `rc:terminate`)
    /// and ends the rest as today: one not yet live, and an external one.
    #[tokio::test]
    async fn an_announced_drop_detaches_the_live_session_and_ends_the_rest() {
        let hub = hub().await;
        let (agent, tenant, user) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let reg = register(&hub, agent, tenant, 4, hello(Some(60), Vec::new()));
        let (live_sid, mut live_rx) = open(&hub, agent, user);
        live(&hub, live_sid).await;
        let (ext_sid, mut ext_rx) = open(&hub, agent, user);
        live(&hub, ext_sid).await;
        hub.with_session(ext_sid, |s| {
            s.external = true;
            Ok(())
        })
        .unwrap();
        let (pending_sid, mut pending_rx) = open(&hub, agent, user);

        assert!(hub.unregister_agent(agent, Some(&reg.tx)));

        assert!(hub.session_snapshot(live_sid).is_none(), "no pod holds it");
        let msgs = drain(&mut live_rx);
        assert_eq!(terminated(&msgs, live_sid), None, "{msgs:?}");
        let told = msgs.iter().find_map(|m| match m {
            ServerMsg::SessionDetached {
                session_id,
                resume_by,
            } if *session_id == live_sid => Some(*resume_by),
            _ => None,
        });
        let now = u64::try_from(DateTime::now().timestamp_millis()).unwrap();
        let by = told.expect("the viewer is told it is kept");
        assert!((now + 59_000..=now + 60_500).contains(&by), "{by} vs {now}");

        assert_eq!(
            terminated(&drain(&mut ext_rx), ext_sid),
            Some(EndReason::AgentDisconnect),
            "D3: an external session ends as today"
        );
        assert_eq!(
            terminated(&drain(&mut pending_rx), pending_sid),
            Some(EndReason::AgentDisconnect),
            "a session not yet live ends as today"
        );
        assert_eq!(hub.live_sessions(), 0);
    }

    /// With no window announced the drop ends every session as it always
    /// has, the live one included.
    #[tokio::test]
    async fn a_drop_with_no_window_ends_the_live_session_as_today() {
        let hub = hub().await;
        let (agent, tenant, user) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let reg = register(&hub, agent, tenant, 2, hello(None, Vec::new()));
        let (sid, mut rx) = open(&hub, agent, user);
        live(&hub, sid).await;
        assert!(hub.unregister_agent(agent, Some(&reg.tx)));
        assert_eq!(
            terminated(&drain(&mut rx), sid),
            Some(EndReason::AgentDisconnect)
        );
    }

    /// A claim holds its place from the registration: a request that would
    /// take it is refused busy until the claim is answered, and a refusal
    /// gives the place back. Unanswered claims are refused at the end.
    #[tokio::test]
    async fn a_claim_holds_its_place_until_it_is_answered() {
        let hub = hub().await;
        let (agent, tenant, user) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let (kept_a, kept_b) = (ObjectId::new(), ObjectId::new());
        let mut reg = register(
            &hub,
            agent,
            tenant,
            2,
            hello(Some(60), vec![kept_a, kept_b]),
        );
        assert_eq!(reg.claims, vec![kept_a, kept_b]);

        let (tx, _rx) = mpsc::channel(8);
        let busy = hub.create_session(
            agent,
            user,
            "Viewer".into(),
            tx,
            Permissions::default(),
            Vec::new(),
            None,
            None,
            None,
            false,
            ConsentMode::Prompt,
            None,
            None,
            None,
            None,
        );
        assert!(
            matches!(
                busy,
                Err(roomler_ai_remote_control::error::Error::AgentBusy)
            ),
            "{busy:?}"
        );

        assert!(hub.refuse_claim(
            agent,
            &reg.tx,
            kept_a,
            EndReason::AgentDisconnect,
            None,
            None
        ));
        assert!(
            !hub.refuse_claim(
                agent,
                &reg.tx,
                kept_a,
                EndReason::AgentDisconnect,
                None,
                None
            ),
            "a claim is answered once"
        );
        let (sid, _ctl) = open(&hub, agent, user);
        assert!(hub.session_snapshot(sid).is_some(), "the place came back");

        assert_eq!(hub.refuse_unanswered_claims(agent, &reg.tx), vec![kept_b]);
        let to_agent = drain(&mut reg.rx);
        assert_eq!(
            terminated(&to_agent, kept_a),
            Some(EndReason::AgentDisconnect)
        );
        assert_eq!(
            terminated(&to_agent, kept_b),
            Some(EndReason::AgentDisconnect)
        );
    }

    /// A claim rebuilds the session the hub no longer holds; the resume is
    /// told only once confirmed, and an end that won the race drops it with
    /// the row's reason and gives its place back.
    #[tokio::test]
    async fn a_claim_rebuilds_the_session_and_an_end_that_won_drops_it() {
        let hub = hub().await;
        let (agent, tenant, user) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let (sid, gone) = (ObjectId::new(), ObjectId::new());
        let mut reg = register(&hub, agent, tenant, 2, hello(Some(60), vec![sid, gone]));
        let resumed = |session_id| ResumedSession {
            session_id,
            tenant_id: tenant,
            controller_user_id: user,
            permissions: Permissions::VIEW,
            created_at: DateTime::now(),
            started_at: DateTime::now(),
            attach_seq: 1,
            max_duration: None,
            slot: None,
        };
        assert_eq!(
            hub.resume_claimed(agent, &reg.tx, resumed(sid)),
            Resumed::Rebuilt
        );
        assert_eq!(hub.session_snapshot(sid), Some((tenant, user)));
        assert!(
            !drain(&mut reg.rx)
                .iter()
                .any(|m| matches!(m, ServerMsg::SessionResumed { .. })),
            "nothing is told before the row is read again"
        );
        assert!(hub.confirm_resumed(agent, &reg.tx, sid, 1));
        assert!(
            drain(&mut reg.rx).iter().any(
                |m| matches!(m, ServerMsg::SessionResumed { session_id } if *session_id == sid)
            )
        );
        assert_eq!(
            hub.resume_claimed(agent, &reg.tx, resumed(sid)),
            Resumed::NotPending,
            "a claim is answered once"
        );

        assert_eq!(
            hub.resume_claimed(agent, &reg.tx, resumed(gone)),
            Resumed::Rebuilt
        );
        hub.drop_resumed(gone, EndReason::AdminTerminated);
        assert!(hub.session_snapshot(gone).is_none());
        assert_eq!(
            terminated(&drain(&mut reg.rx), gone),
            Some(EndReason::AdminTerminated)
        );
        assert!(!hub.confirm_resumed(agent, &reg.tx, gone, 1));
        // One live session counted (sid), so one place is left.
        let (other, _ctl) = open(&hub, agent, user);
        assert!(hub.session_snapshot(other).is_some());
    }

    /// The device's earlier connection on this pod is half-open and still
    /// holds the session: the reap spares the claimed session, the claim
    /// re-attaches it at the new number with its controller, and the
    /// controller is told the resume too.
    #[tokio::test]
    async fn a_session_still_held_for_the_earlier_connection_is_reattached() {
        let hub = hub().await;
        let (agent, tenant, user) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let _first = register(&hub, agent, tenant, 2, hello(Some(60), Vec::new()));
        let (sid, mut ctl) = open(&hub, agent, user);
        live(&hub, sid).await;
        let (other, mut other_ctl) = open(&hub, agent, user);
        live(&hub, other).await;
        drain(&mut ctl);

        let second = register(&hub, agent, tenant, 2, hello(Some(60), vec![sid]));
        assert_eq!(
            second.reaped,
            vec![other],
            "only the unclaimed one is reaped"
        );
        assert_eq!(
            terminated(&drain(&mut other_ctl), other),
            Some(EndReason::AgentDisconnect)
        );
        assert!(drain(&mut ctl).is_empty(), "the claimed one is spared");

        let r = ResumedSession {
            session_id: sid,
            tenant_id: tenant,
            controller_user_id: user,
            permissions: Permissions::VIEW,
            created_at: DateTime::now(),
            started_at: DateTime::now(),
            attach_seq: 1,
            max_duration: None,
            slot: None,
        };
        assert_eq!(
            hub.resume_claimed(agent, &second.tx, r),
            Resumed::Reattached
        );
        assert!(hub.confirm_resumed(agent, &second.tx, sid, 1));
        assert!(
            drain(&mut ctl).iter().any(
                |m| matches!(m, ServerMsg::SessionResumed { session_id } if *session_id == sid)
            ),
            "the viewer this pod still had hears the resume"
        );
        assert_eq!(hub.with_session(sid, |s| Ok(s.attach_seq)).unwrap(), 1);
    }

    /// A refusal of a session still held for the earlier connection ends that
    /// copy, telling both ends, and leaves the device's count right.
    #[tokio::test]
    async fn refusing_a_session_still_held_ends_it_and_keeps_the_count() {
        let hub = hub().await;
        let (agent, tenant, user) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let _first = register(&hub, agent, tenant, 1, hello(Some(60), Vec::new()));
        let (sid, mut ctl) = open(&hub, agent, user);
        live(&hub, sid).await;
        drain(&mut ctl);

        let mut second = register(&hub, agent, tenant, 1, hello(Some(60), vec![sid]));
        assert!(hub.refuse_claim(
            agent,
            &second.tx,
            sid,
            EndReason::AdminTerminated,
            None,
            Some(RowEnd::fenced(0)),
        ));
        assert!(hub.session_snapshot(sid).is_none());
        assert_eq!(
            terminated(&drain(&mut ctl), sid),
            Some(EndReason::AdminTerminated)
        );
        assert_eq!(
            terminated(&drain(&mut second.rx), sid),
            Some(EndReason::AdminTerminated)
        );
        let (next, _c) = open(&hub, agent, user);
        assert!(
            hub.session_snapshot(next).is_some(),
            "the device's one place is free again"
        );
    }

    /// The announce reaches only a connection that advertises the verb, and
    /// 0 withdraws the window, so the next drop ends as today.
    #[tokio::test]
    async fn an_announce_needs_the_verb_and_zero_withdraws_it() {
        let hub = hub().await;
        let (agent, tenant) = (ObjectId::new(), ObjectId::new());
        let mut reg = register(
            &hub,
            agent,
            tenant,
            2,
            ResumeHello {
                advertised: false,
                window: None,
                kept: Vec::new(),
            },
        );
        assert!(!hub.announce_resume_window(agent, 60));
        assert!(drain(&mut reg.rx).is_empty(), "never sent blind");

        let mut reg = register(&hub, agent, tenant, 2, hello(Some(60), Vec::new()));
        assert_eq!(hub.agent_resume_window(agent), Some(60));
        assert!(hub.announce_resume_window(agent, 0));
        assert_eq!(hub.agent_resume_window(agent), None);
        assert!(
            drain(&mut reg.rx)
                .iter()
                .any(|m| matches!(m, ServerMsg::AgentResumeWindow { secs: 0 }))
        );
    }
}
