// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The start route's wait for a device's `rc:hive.start_ack`.
//!
//! FR-83's shape (`network::ssh_grant_acks`) with one difference that
//! matters: the SESSION RECORD is the source of truth, not this table. The
//! agent socket writes every answer to the record first and only then wakes
//! the waiter, which re-reads the record. So a late answer — past the bound,
//! after a reconnect to another pod, after the caller gave up — still lands
//! where it belongs, and the caller that timed out was told `pending`, never
//! a guess.
//!
//! A waiter is keyed by session id and BOUND to the device the start was
//! pushed to; an answer from any other socket wakes nobody and leaves the
//! slot for the real one.

use std::sync::Arc;
use std::time::Duration;

use bson::oid::ObjectId;
use dashmap::DashMap;
use tokio::sync::oneshot;

struct Waiter {
    device_id: ObjectId,
    tx: oneshot::Sender<()>,
}

/// Start requests waiting for their device's answer. Pod-local, like the
/// Hub's exec waiters: the push only succeeds when the device's socket is on
/// this pod, and its answer arrives on that same socket.
#[derive(Default)]
pub struct StartAcks {
    waiters: DashMap<ObjectId, Waiter>,
}

impl StartAcks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park on `device_id`'s answer about `session_id`. Register BEFORE the
    /// push, so an answer that beats the caller back finds somewhere to land;
    /// the guard deregisters on every exit path.
    pub fn expect(self: &Arc<Self>, session_id: ObjectId, device_id: ObjectId) -> PendingStart {
        let (tx, rx) = oneshot::channel();
        self.waiters.insert(session_id, Waiter { device_id, tx });
        PendingStart {
            acks: Arc::clone(self),
            session_id,
            rx,
        }
    }

    /// Wake whoever waits on `session_id`, if `from_device` is the device it
    /// waits on. `false` = nobody was waiting (late, or never waited), or the
    /// answer came from another device — refused WITHOUT consuming the slot.
    pub fn deliver(&self, session_id: ObjectId, from_device: ObjectId) -> bool {
        match self
            .waiters
            .remove_if(&session_id, |_, w| w.device_id == from_device)
        {
            Some((_, w)) => w.tx.send(()).is_ok(),
            None => false,
        }
    }

    /// Starts currently waiting. Tests and diagnostics only.
    pub fn pending(&self) -> usize {
        self.waiters.len()
    }
}

/// One start request waiting on one device. Dropping it deregisters.
pub struct PendingStart {
    acks: Arc<StartAcks>,
    session_id: ObjectId,
    rx: oneshot::Receiver<()>,
}

impl PendingStart {
    /// Wait at most `bound`. `true` = the device answered (read the record
    /// for what it said); `false` = no answer in time.
    pub async fn wait(mut self, bound: Duration) -> bool {
        matches!(tokio::time::timeout(bound, &mut self.rx).await, Ok(Ok(())))
    }
}

impl Drop for PendingStart {
    fn drop(&mut self) {
        self.acks.waiters.remove(&self.session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(200);

    #[tokio::test]
    async fn the_devices_answer_wakes_the_wait() {
        let acks = Arc::new(StartAcks::new());
        let (sid, dev) = (ObjectId::new(), ObjectId::new());
        let pending = acks.expect(sid, dev);
        assert!(acks.deliver(sid, dev));
        assert!(pending.wait(SHORT).await);
        assert_eq!(acks.pending(), 0);
    }

    /// Session ids are not secret: another device's answer wakes nobody and
    /// does not consume the slot.
    #[tokio::test]
    async fn another_devices_answer_wakes_nobody_and_leaves_the_slot() {
        let acks = Arc::new(StartAcks::new());
        let (sid, dev, stranger) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let pending = acks.expect(sid, dev);
        assert!(!acks.deliver(sid, stranger));
        assert_eq!(acks.pending(), 1);
        assert!(acks.deliver(sid, dev));
        assert!(pending.wait(SHORT).await);
    }

    #[tokio::test]
    async fn silence_is_no_answer_and_the_slot_is_released() {
        let acks = Arc::new(StartAcks::new());
        let pending = acks.expect(ObjectId::new(), ObjectId::new());
        assert!(!pending.wait(SHORT).await);
        assert_eq!(acks.pending(), 0);
    }

    #[tokio::test]
    async fn an_answer_that_beats_the_wait_is_kept() {
        let acks = Arc::new(StartAcks::new());
        let (sid, dev) = (ObjectId::new(), ObjectId::new());
        let pending = acks.expect(sid, dev);
        assert!(acks.deliver(sid, dev));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(pending.wait(SHORT).await);
    }

    #[tokio::test]
    async fn a_dropped_guard_leaves_nothing_for_a_late_answer() {
        let acks = Arc::new(StartAcks::new());
        let (sid, dev) = (ObjectId::new(), ObjectId::new());
        drop(acks.expect(sid, dev));
        assert_eq!(acks.pending(), 0);
        assert!(!acks.deliver(sid, dev));
    }
}
