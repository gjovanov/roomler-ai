// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-83 — the server's half of the SSH grant acknowledgement.
//!
//! `decide()` registers a waiter here BEFORE it pushes `rc:ssh.grant`, and the
//! target's `rc:ssh.grant_ack` resolves it. Until the ack arrives the caller is
//! not told where to dial: "queued for the device" is not "the device has it",
//! and a caller whose control path was faster than the target's used to dial
//! into a device that had not heard of it yet (#1597).
//!
//! A waiter is keyed by the grant id and BOUND to the agent the grant was
//! pushed to. Grant ids are ObjectIds — structured, not secret — so an ack
//! from any other socket confirms nothing and does not even consume the slot
//! (otherwise a device could cancel someone else's wait).
//!
//! Pod-local, like the Hub's exec waiters: the push only succeeds when the
//! target's socket is on this pod, and its ack arrives on that same socket.

use std::sync::Arc;
use std::time::Duration;

use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_remote_control::models::SshGrantRefusal;
use tokio::sync::oneshot;

/// What became of one grant, as far as the server can know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// The device holds the grant; a connection with its key now succeeds.
    Recorded,
    /// The device will never honour it, and said why.
    Refused(SshGrantRefusal),
    /// No answer within the bound — or the waiter was abandoned.
    Unconfirmed,
}

struct Waiter {
    agent_id: ObjectId,
    tx: oneshot::Sender<Option<SshGrantRefusal>>,
}

/// In-flight grants awaiting their target's acknowledgement.
#[derive(Default)]
pub struct SshGrantAcks {
    waiters: DashMap<String, Waiter>,
}

impl SshGrantAcks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park on `agent_id`'s answer to `grant_id`.
    ///
    /// Register BEFORE the push: an ack that beats the caller back to this
    /// table must find somewhere to land. The returned guard deregisters on
    /// every exit path — a failed push, a legacy agent that will never ack, a
    /// timeout — so an abandoned slot cannot outlive its request.
    pub fn expect(self: &Arc<Self>, grant_id: &str, agent_id: ObjectId) -> PendingAck {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .insert(grant_id.to_string(), Waiter { agent_id, tx });
        PendingAck {
            acks: Arc::clone(self),
            grant_id: grant_id.to_string(),
            rx,
        }
    }

    /// Hand an ack to whoever is parked on it. `false` = nobody was: the
    /// caller already gave up, never waited (a legacy path), or the ack came
    /// from a socket other than the target's — which is refused WITHOUT
    /// removing the slot, so the real target can still answer.
    pub fn deliver(
        &self,
        grant_id: &str,
        from_agent: ObjectId,
        refused: Option<SshGrantRefusal>,
    ) -> bool {
        match self
            .waiters
            .remove_if(grant_id, |_, w| w.agent_id == from_agent)
        {
            Some((_, w)) => w.tx.send(refused).is_ok(),
            None => false,
        }
    }

    /// Grants currently awaiting an ack. Tests and diagnostics only.
    pub fn pending(&self) -> usize {
        self.waiters.len()
    }
}

/// One caller parked on one grant. Dropping it deregisters the slot.
pub struct PendingAck {
    acks: Arc<SshGrantAcks>,
    grant_id: String,
    rx: oneshot::Receiver<Option<SshGrantRefusal>>,
}

impl PendingAck {
    /// Wait at most `bound` for the target's answer.
    pub async fn wait(mut self, bound: Duration) -> AckOutcome {
        match tokio::time::timeout(bound, &mut self.rx).await {
            Ok(Ok(None)) => AckOutcome::Recorded,
            Ok(Ok(Some(r))) => AckOutcome::Refused(r),
            // Sender dropped without an answer, or the bound expired.
            Ok(Err(_)) | Err(_) => AckOutcome::Unconfirmed,
        }
    }
}

impl Drop for PendingAck {
    fn drop(&mut self) {
        self.acks.waiters.remove(&self.grant_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(200);

    #[tokio::test]
    async fn the_targets_ack_resolves_the_wait() {
        let acks = Arc::new(SshGrantAcks::new());
        let target = ObjectId::new();
        let pending = acks.expect("g1", target);
        assert!(acks.deliver("g1", target, None));
        assert_eq!(pending.wait(SHORT).await, AckOutcome::Recorded);
        assert_eq!(acks.pending(), 0);
    }

    #[tokio::test]
    async fn a_refusal_carries_its_reason() {
        let acks = Arc::new(SshGrantAcks::new());
        let target = ObjectId::new();
        let pending = acks.expect("g1", target);
        assert!(acks.deliver("g1", target, Some(SshGrantRefusal::SshDisabled)));
        assert_eq!(
            pending.wait(SHORT).await,
            AckOutcome::Refused(SshGrantRefusal::SshDisabled)
        );
    }

    /// AC5 — grant ids are ObjectIds: structured, not secret. An ack from any
    /// socket but the target's must confirm nothing AND leave the slot for
    /// the real answer; consuming it would let one device cancel another's
    /// wait.
    #[tokio::test]
    async fn an_ack_from_another_agent_confirms_nothing_and_leaves_the_slot() {
        let acks = Arc::new(SshGrantAcks::new());
        let (target, stranger) = (ObjectId::new(), ObjectId::new());
        let pending = acks.expect("g1", target);

        assert!(!acks.deliver("g1", stranger, None), "a stranger's ack");
        assert_eq!(acks.pending(), 1, "the slot survives the stranger");

        assert!(acks.deliver("g1", target, None));
        assert_eq!(pending.wait(SHORT).await, AckOutcome::Recorded);
    }

    /// AC3 — silence is `Unconfirmed`, and the slot does not outlive it.
    #[tokio::test]
    async fn silence_is_unconfirmed_and_the_slot_is_released() {
        let acks = Arc::new(SshGrantAcks::new());
        let pending = acks.expect("g1", ObjectId::new());
        assert_eq!(pending.wait(SHORT).await, AckOutcome::Unconfirmed);
        assert_eq!(acks.pending(), 0);
    }

    /// A caller that stopped waiting (legacy agent, failed push) must not
    /// leave a slot a late ack could land in.
    #[tokio::test]
    async fn dropping_the_guard_deregisters_and_a_late_ack_lands_nowhere() {
        let acks = Arc::new(SshGrantAcks::new());
        let target = ObjectId::new();
        drop(acks.expect("g1", target));
        assert_eq!(acks.pending(), 0);
        assert!(!acks.deliver("g1", target, None));
    }

    /// An ack that arrives BEFORE the caller starts waiting — the device
    /// answered faster than `decide()` got from the push to the await — is
    /// kept, not lost.
    #[tokio::test]
    async fn an_ack_that_beats_the_wait_is_not_lost() {
        let acks = Arc::new(SshGrantAcks::new());
        let target = ObjectId::new();
        let pending = acks.expect("g1", target);
        assert!(acks.deliver("g1", target, None));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pending.wait(SHORT).await, AckOutcome::Recorded);
    }
}
