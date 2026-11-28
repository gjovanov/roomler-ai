// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1e — the work a token rotation's commit starts, and the answer
//! asked again (`docs/fr/FR-155-one-row-new-credentials.md`, "The hook
//! `agent_credentials_rotated`").
//!
//! * **The work runs to its end** ([`run`]). Once `AgentDao::rotate_token`
//!   commits G+1, `self_rotate` (`crate::agent`) spawns the work the commit
//!   starts and awaits its handle before it mints the answer. The work is, in
//!   order: this pod's sweep (`Hub::end_below_generation`), the control event
//!   `end_below_generation` for every other pod, the `first` audit row, the
//!   hook `agent_credentials_rotated` with its failure path, and last the
//!   row's mark, `token_rotation.settled_at`. A spawned task runs to its end
//!   whatever becomes of the request that spawned it, and the route answers
//!   only after it, so every effect comes before the device holds the token
//!   of G+1 from the route.
//! * **An answer asked again waits for the work** ([`wait_for_the_work`]).
//!   `answer_again` gives the token of G+1 once the work of its commit has
//!   ended: on this pod, by the task's own end ([`InFlight`]); on any other,
//!   by the row's mark, read on the primary every [`REPLAY_POLL_MS`]. Either
//!   wait is bounded by [`REPLAY_WAIT_SECS`].
//!
//! **Why an answer asked again waits, on every pod.** Two of the work's
//! effects are not judged by generation: the overlay node's Offline mark
//! with its fan-out (the network module's `handle_overlay_leave`), and the
//! key order's push, which reaches only a connection on the pod that places
//! the order. Each comes before the device's first connection at G+1 only if
//! every answer that carries G+1 comes after the work. The route's own answer
//! does; an answer asked again does too, on the pod that committed by the
//! task's end, and on any other pod by the mark, which the work writes last.
//! The work's other effects are judged by generation: an end at G+1 leaves a
//! connection at G+1 alone, on every pod.
//!
//! **The bound.** Once the row is at G+1, the token an answer carries is the
//! device's only credential the server admits, so no wait keeps it from the
//! device for long. Past [`REPLAY_WAIT_SECS`], which is under the agent's own
//! request timeout (20 s), the answer is given without the work's end, with a
//! warning and `credential_rotation_replay_unsettled_total` in
//! `GET /api/cluster/status`. A pod built before P1e writes no mark, so an
//! answer asked again for its rotation waits the bound once. Whatever the
//! timing, every pod's own check ends a connection below its row's generation
//! within one period (`crate::generation_check`).
//!
//! **The mark** is written only while the row's rotation is the one the work
//! ran for, its generation and its key's hash
//! (`AgentDao::mark_rotation_settled`), so an earlier work never marks a
//! later rotation's record. `AgentDao::rotate_token` writes each rotation's
//! record anew, without the mark.
//!
//! With the rotation off (`Hub::token_rotation`), nothing of this runs: no
//! rotation commits, so no work starts, and an answer asked again reads
//! nothing new and is given at once, as before.
//!
//! [`CommitSeam`] is where the integration cells hold the work, right after
//! the commit. Unheld, it costs one lock per commit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_remote_control::models::Agent;
use tokio::sync::watch;

use crate::FleetState;
use crate::agent::RotationAudit;
use crate::auth_agent::RotationCaller;

/// How long an answer asked again waits for the work of its commit, at most.
/// Under the agent's own request timeout (`token_renewal.rs`'s 20 s), so the
/// device reads the answer of the request that waited.
pub const REPLAY_WAIT_SECS: u64 = 10;

/// How often an answer asked again, on a pod that does not run the work,
/// reads the row for the work's mark.
pub const REPLAY_POLL_MS: u64 = 250;

/// The works of commits running on this pod: per tenant and agent, the
/// generation the commit made and what tells the work's end.
#[derive(Default)]
pub struct InFlight {
    works: DashMap<(ObjectId, ObjectId), (u32, watch::Receiver<bool>)>,
}

/// One work on this pod's [`InFlight`], from the commit to its end. Its drop
/// tells the work's end to every answer that waits for it, also a drop on a
/// panic's unwind.
pub(crate) struct Running {
    works: Arc<InFlight>,
    key: (ObjectId, ObjectId),
    generation: u32,
    ended: watch::Sender<bool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.ended.send(true);
        let generation = self.generation;
        self.works
            .works
            .remove_if(&self.key, |_, (running, _)| *running == generation);
    }
}

impl InFlight {
    /// Record the work of `agent_id`'s commit to `generation` as running on
    /// this pod, until the returned [`Running`] drops. Called before the work
    /// is spawned, so an answer asked again on this pod finds it from then on.
    pub(crate) fn start(
        self: &Arc<Self>,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        generation: u32,
    ) -> Running {
        let (ended, end) = watch::channel(false);
        self.works.insert((tenant_id, agent_id), (generation, end));
        Running {
            works: self.clone(),
            key: (tenant_id, agent_id),
            generation,
            ended,
        }
    }

    /// The end of the work of the commit to `generation`, while it runs on
    /// this pod.
    fn running(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        generation: u32,
    ) -> Option<watch::Receiver<bool>> {
        self.works
            .get(&(tenant_id, agent_id))
            .filter(|work| work.0 == generation)
            .map(|work| work.1.clone())
    }
}

/// The work a commit to `running`'s generation starts, in its order (the
/// module doc). The caller awaits it before it answers; it runs to its end
/// either way.
pub(crate) async fn run(
    state: FleetState,
    caller: RotationCaller,
    client_ip: Option<String>,
    key_sha256: String,
    running: Running,
) {
    let next_gen = running.generation;
    // Dropped last, after the mark: this pod's answers asked again wait for it.
    let _running = running;
    state.rotation_seam.pass().await;
    let audit = RotationAudit {
        state: &state,
        caller,
        client_ip,
    };

    // FR-155 P1b — what the commit ends: the device's connections admitted
    // below the new generation, on this pod and, through the control
    // channel, on every other, each pod keeping a floor that refuses a late
    // registration below it (`hub::rotation`).
    state
        .rc_hub
        .end_below_generation(caller.agent_id, caller.tenant_id, next_gen);
    crate::ctrl::publish_rc_ctrl(
        &state.core,
        "end_below_generation",
        serde_json::json!({
            "agent_id": caller.agent_id.to_hex(),
            "tenant_id": caller.tenant_id.to_hex(),
            "generation": next_gen,
        }),
    )
    .await;
    audit.record("first", None, Some(next_gen)).await;
    // FR-155 P1d — what the commit changes outside fleet: the overlay node
    // goes offline, the DERP registrations below the new generation end, and
    // the device is ordered a new overlay key
    // (`FleetLifecycle::agent_credentials_rotated`). Once, here, before the
    // answer: the device connects at the new generation only once it has the
    // token. A failure is logged, counted and audited, and the token is
    // answered all the same.
    let failed = state
        .hooks
        .agent_credentials_rotated(caller.tenant_id, caller.agent_id, next_gen)
        .await;
    if !failed.is_empty() {
        roomler_core::cluster::metrics::bump(
            &roomler_core::cluster::metrics::CREDENTIAL_ROTATION_HOOK_FAILED_TOTAL,
        );
        for (module, e) in &failed {
            tracing::warn!(
                tenant_id = %caller.tenant_id, agent_id = %caller.agent_id,
                generation = next_gen, module, error = %e,
                "fr-155: a module could not apply the token rotation; the new token is answered"
            );
        }
        let modules = failed
            .iter()
            .map(|(module, _)| *module)
            .collect::<Vec<_>>()
            .join(",");
        audit
            .record("hook_failed", Some(modules.as_str()), Some(next_gen))
            .await;
    }

    // FR-155 P1e — the work's mark, last: an answer asked again on another
    // pod waits for it.
    match state
        .agents
        .mark_rotation_settled(caller.tenant_id, caller.agent_id, next_gen, &key_sha256)
        .await
    {
        Ok(true) => {}
        Ok(false) => tracing::info!(
            tenant_id = %caller.tenant_id, agent_id = %caller.agent_id, generation = next_gen,
            "fr-155: the row records another rotation than this work's; no mark written"
        ),
        Err(e) => tracing::warn!(
            tenant_id = %caller.tenant_id, agent_id = %caller.agent_id, generation = next_gen,
            error = %e,
            "fr-155: the end of a rotation's work could not be marked on the row; an answer \
             asked again on another pod waits its bound"
        ),
    }
}

/// An answer asked again for `row`'s rotation: wait until the work of its
/// commit has ended, at most [`REPLAY_WAIT_SECS`] (the module doc). Returns
/// at once when the row carries the work's mark.
pub(crate) async fn wait_for_the_work(
    state: &FleetState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    row: &Agent,
) {
    let Some(rotation) = row.token_rotation.as_ref() else {
        return;
    };
    if rotation.settled_at.is_some() {
        return;
    }
    let generation = rotation.generation;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(REPLAY_WAIT_SECS);
    let ended = match state
        .rotations_in_flight
        .running(tenant_id, agent_id, generation)
    {
        // This pod runs the work: its end. A closed channel is an end too.
        Some(mut end) => {
            let waited = tokio::time::timeout_at(deadline, end.wait_for(|ended| *ended)).await;
            waited.is_ok()
        }
        // Another pod runs it, or ran it: the row's mark.
        None => marked_by(state, tenant_id, agent_id, generation, deadline).await,
    };
    if !ended {
        roomler_core::cluster::metrics::bump(
            &roomler_core::cluster::metrics::CREDENTIAL_ROTATION_REPLAY_UNSETTLED_TOTAL,
        );
        tracing::warn!(
            %tenant_id, %agent_id, generation, wait_secs = REPLAY_WAIT_SECS,
            "fr-155: an answer asked again is given before the work of its commit marked its \
             end, after the bound"
        );
    }
}

/// Read the row on the primary every [`REPLAY_POLL_MS`] until it carries
/// the mark of the work for `generation`, or records another rotation, or
/// `deadline` passes (`false`). A failed read is read again.
async fn marked_by(
    state: &FleetState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    generation: u32,
    deadline: tokio::time::Instant,
) -> bool {
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let pause = Duration::from_millis(REPLAY_POLL_MS).min(deadline - now);
        tokio::time::sleep(pause).await;
        match state
            .agents
            .find_any_in_tenant_on_the_primary(tenant_id, agent_id)
            .await
        {
            Ok(row) => {
                let settled = row
                    .token_rotation
                    .as_ref()
                    .is_none_or(|r| r.generation != generation || r.settled_at.is_some());
                if settled {
                    return true;
                }
            }
            Err(roomler_ai_services::dao::base::DaoError::NotFound) => return true,
            Err(e) => tracing::debug!(
                %agent_id, error = %e,
                "fr-155: reading the row for the mark of a rotation's work; reading again"
            ),
        }
    }
}

/// The point right after a rotation's commit, where a cell can hold the work
/// the commit starts (`crates/tests/src/token_rotation_tests.rs`, T24 and
/// T25). The precedent is the remote module's `resume_gate_runs`.
#[doc(hidden)]
#[derive(Default)]
pub struct CommitSeam {
    /// While a cell holds the seam: the gate the work waits on until it opens.
    gate: Mutex<Option<watch::Receiver<bool>>>,
    /// How many works wait at the seam now.
    waiting: AtomicUsize,
}

/// A held seam. [`Self::release`] or a drop opens it.
#[doc(hidden)]
pub struct SeamHold(watch::Sender<bool>);

impl SeamHold {
    /// Open the seam: each work waiting at it goes on.
    pub fn release(self) {}
}

impl Drop for SeamHold {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// Counts one work waiting at the seam while it lives.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CommitSeam {
    /// Hold the seam: the work of each commit on this pod waits at it until
    /// the hold is released or dropped.
    pub fn hold(&self) -> SeamHold {
        let (open, gate) = watch::channel(false);
        *self.gate.lock().unwrap_or_else(PoisonError::into_inner) = Some(gate);
        SeamHold(open)
    }

    /// How many works wait at the seam now.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// The work's side: returns at once unless a cell holds the seam, and
    /// otherwise once the hold ends.
    pub(crate) async fn pass(&self) {
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(mut gate) = gate else {
            return;
        };
        let open = *gate.borrow();
        if open {
            return;
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&self.waiting);
        let _ = gate.wait_for(|open| *open).await;
    }
}
