//! Consent flow.
//!
//! When a controller requests a session, the server creates a oneshot channel
//! and stores its sender in the session's pending state. The agent receives
//! `rc:request`, prompts the user, and replies with `rc:consent`. The server
//! resolves the oneshot. If the agent doesn't reply within the timeout, the
//! server resolves with `Timeout` and tears down the session.

use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentOutcome {
    Granted,
    Denied,
    Timeout,
}

pub const DEFAULT_CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Channel used by the hub to deliver the agent's consent decision.
pub struct ConsentSlot {
    tx: oneshot::Sender<bool>,
}

impl ConsentSlot {
    pub fn new() -> (Self, ConsentWaiter) {
        let (tx, rx) = oneshot::channel();
        (Self { tx }, ConsentWaiter { rx })
    }

    /// Called by the signaling layer when the agent's `rc:consent` arrives.
    /// Returns Err if the waiter already gave up.
    pub fn resolve(self, granted: bool) -> Result<()> {
        self.tx
            .send(granted)
            .map_err(|_| Error::BadMessage("consent waiter dropped"))
    }
}

pub struct ConsentWaiter {
    rx: oneshot::Receiver<bool>,
}

impl ConsentWaiter {
    pub async fn wait(self, dur: Duration) -> ConsentOutcome {
        match timeout(dur, self.rx).await {
            Ok(Ok(true)) => ConsentOutcome::Granted,
            Ok(Ok(false)) => ConsentOutcome::Denied,
            Ok(Err(_)) => ConsentOutcome::Timeout, // sender dropped
            Err(_) => ConsentOutcome::Timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn granted() {
        let (slot, waiter) = ConsentSlot::new();
        slot.resolve(true).unwrap();
        assert_eq!(
            waiter.wait(Duration::from_millis(50)).await,
            ConsentOutcome::Granted
        );
    }

    #[tokio::test]
    async fn denied() {
        let (slot, waiter) = ConsentSlot::new();
        slot.resolve(false).unwrap();
        assert_eq!(
            waiter.wait(Duration::from_millis(50)).await,
            ConsentOutcome::Denied
        );
    }

    #[tokio::test]
    async fn times_out() {
        let (_slot, waiter) = ConsentSlot::new();
        let t0 = std::time::Instant::now();
        let outcome = waiter.wait(Duration::from_millis(20)).await;
        assert_eq!(outcome, ConsentOutcome::Timeout);
        assert!(t0.elapsed() < Duration::from_millis(100));
    }
}
