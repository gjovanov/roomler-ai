//! Consent flow.
//!
//! When a controller requests a session, the server creates a oneshot channel
//! and stores its sender in the session's pending state. The agent receives
//! `rc:request`, prompts the user, and replies with `rc:consent`. The server
//! resolves the oneshot. If the agent doesn't reply within the timeout, the
//! server resolves with `Timeout` and tears down the session.
//!
//! FR-27 — a `granted: false` is not one thing. "The person at the machine
//! pressed Deny", "the prompt stood there and nobody answered" and "no surface
//! on that host could raise a prompt at all" have different causes and
//! different fixes, and collapsing them was actively misleading: the agent's
//! own prompt timeout produced `granted: false`, which the hub terminated as
//! `UserDenied`, so the controller was told a human had refused them when in
//! fact nobody had been asked. [`ConsentDenyReason`] rides `rc:consent`
//! alongside `granted` so the three stay distinguishable end to end.

use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentOutcome {
    Granted,
    /// A human at the device refused.
    Denied,
    /// The window closed with no answer — either the agent said so, or our own
    /// waiter expired.
    Timeout,
    /// The device could raise no prompt at all (no desktop session, no
    /// companion, nothing). Distinct from `Timeout` because nobody was ever
    /// asked, so "try again and answer it" is the wrong advice.
    NoPromptSurface,
}

/// Why an agent answered `granted: false`.
///
/// Carried on the wire as a plain string (`ClientMsg::Consent::reason`), for
/// the same reason `AgentCaps.rpc` is a `Vec<String>`: agents in the field span
/// many releases, and a newer one must be able to name a reason this server has
/// never heard of without the frame failing to parse. Both sides go through
/// this type so neither can misspell one — the `RpcCap` pattern.
///
/// ⚠️ The wire spellings are a compatibility surface. Renaming one does not
/// fail loudly; it silently turns every report from a deployed agent into
/// `None`, i.e. back into "the user denied you". Locked by
/// `deny_reason_wire_strings_are_locked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentDenyReason {
    /// The on-host prompt was raised and its window closed unanswered.
    HostTimeout,
    /// No prompt surface was reachable, so no human was ever asked.
    NoPromptSurface,
}

impl ConsentDenyReason {
    /// Every variant — a test walks this so a new one cannot be added without
    /// a wire spelling.
    pub const ALL: &'static [ConsentDenyReason] = &[
        ConsentDenyReason::HostTimeout,
        ConsentDenyReason::NoPromptSurface,
    ];

    pub fn wire(self) -> &'static str {
        match self {
            ConsentDenyReason::HostTimeout => "timeout",
            ConsentDenyReason::NoPromptSurface => "no_prompt_surface",
        }
    }

    /// `None` for an absent or unrecognised reason — which correctly reads as
    /// "an ordinary deny", the pre-FR-27 meaning of a bare `granted: false`.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.wire() == s)
    }

    fn outcome(self) -> ConsentOutcome {
        match self {
            ConsentDenyReason::HostTimeout => ConsentOutcome::Timeout,
            ConsentDenyReason::NoPromptSurface => ConsentOutcome::NoPromptSurface,
        }
    }
}

/// Resolve a wire `(granted, reason)` pair into the outcome the hub acts on.
/// One place, so the "absent reason ⇒ explicit deny" default cannot drift
/// between the WS dispatcher and the approve-link route.
pub fn outcome_of(granted: bool, reason: Option<&str>) -> ConsentOutcome {
    if granted {
        return ConsentOutcome::Granted;
    }
    match reason.and_then(ConsentDenyReason::from_wire) {
        Some(r) => r.outcome(),
        None => ConsentOutcome::Denied,
    }
}

/// The attended window: how long an on-host prompt stands, and how long the
/// hub waits for it.
pub const DEFAULT_CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The window for a mode whose approval reaches a HUMAN somewhere else — email
/// link, push card. Minutes rather than seconds, because the owner has to be
/// reached, read it and act.
pub const ASYNC_CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

/// FR-27 — how long the ON-HOST half of `prompt_then_email` stands.
///
/// Deliberately NOT [`ASYNC_CONSENT_TIMEOUT`], which is what the agent used to
/// be handed: a modal that sits on someone's screen for five minutes is not a
/// prompt, it is an obstruction, and the host is not the party that needs the
/// long window. The host gets the attended window; the emailed link keeps the
/// full async one. A host timeout therefore hands over to the owner rather than
/// ending the session — see `Hub::deliver_consent`.
pub const HOST_PROMPT_TIMEOUT: Duration = DEFAULT_CONSENT_TIMEOUT;

/// Channel used by the hub to deliver the agent's consent decision.
pub struct ConsentSlot {
    tx: oneshot::Sender<ConsentOutcome>,
}

impl ConsentSlot {
    pub fn new() -> (Self, ConsentWaiter) {
        let (tx, rx) = oneshot::channel();
        (Self { tx }, ConsentWaiter { rx })
    }

    /// Called by the signaling layer when the agent's `rc:consent` arrives.
    /// Returns Err if the waiter already gave up.
    pub fn resolve(self, outcome: ConsentOutcome) -> Result<()> {
        self.tx
            .send(outcome)
            .map_err(|_| Error::BadMessage("consent waiter dropped"))
    }
}

pub struct ConsentWaiter {
    rx: oneshot::Receiver<ConsentOutcome>,
}

impl ConsentWaiter {
    pub async fn wait(self, dur: Duration) -> ConsentOutcome {
        match timeout(dur, self.rx).await {
            Ok(Ok(outcome)) => outcome,
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
        slot.resolve(ConsentOutcome::Granted).unwrap();
        assert_eq!(
            waiter.wait(Duration::from_millis(50)).await,
            ConsentOutcome::Granted
        );
    }

    #[tokio::test]
    async fn denied() {
        let (slot, waiter) = ConsentSlot::new();
        slot.resolve(ConsentOutcome::Denied).unwrap();
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

    /// FR-27 — the defect this type exists to close: an agent-side prompt
    /// timeout used to arrive as a bare `granted: false` and be reported to the
    /// controller as "the user denied your request".
    #[test]
    fn a_bare_false_is_a_deny_but_a_reasoned_one_is_not() {
        assert_eq!(outcome_of(false, None), ConsentOutcome::Denied);
        assert_eq!(outcome_of(false, Some("timeout")), ConsentOutcome::Timeout);
        assert_eq!(
            outcome_of(false, Some("no_prompt_surface")),
            ConsentOutcome::NoPromptSurface
        );
    }

    /// A reason from a NEWER agent that this server has never heard of must
    /// degrade to the historical meaning, never fail the frame.
    #[test]
    fn an_unknown_reason_degrades_to_deny() {
        assert_eq!(
            outcome_of(false, Some("invented_in_a_later_release")),
            ConsentOutcome::Denied
        );
        assert!(ConsentDenyReason::from_wire("").is_none());
    }

    /// `granted: true` wins regardless of what else is on the frame — a reason
    /// alongside a grant is nonsense, and it must not be able to deny.
    #[test]
    fn a_grant_ignores_any_reason() {
        assert_eq!(outcome_of(true, Some("timeout")), ConsentOutcome::Granted);
        assert_eq!(outcome_of(true, None), ConsentOutcome::Granted);
    }

    /// Renaming a spelling doesn't fail loudly — it silently turns every
    /// deployed agent's report back into "denied". Pin them.
    #[test]
    fn deny_reason_wire_strings_are_locked() {
        assert_eq!(ConsentDenyReason::HostTimeout.wire(), "timeout");
        assert_eq!(
            ConsentDenyReason::NoPromptSurface.wire(),
            "no_prompt_surface"
        );
        for r in ConsentDenyReason::ALL {
            assert_eq!(ConsentDenyReason::from_wire(r.wire()), Some(*r));
        }
    }

    /// The host half of `prompt_then_email` must be the ATTENDED window, not
    /// the async one — a five-minute modal is an obstruction, and it is the
    /// owner, not the host, who needs the long window.
    #[test]
    fn the_host_prompt_window_is_not_the_async_one() {
        assert_eq!(HOST_PROMPT_TIMEOUT, DEFAULT_CONSENT_TIMEOUT);
        assert!(HOST_PROMPT_TIMEOUT < ASYNC_CONSENT_TIMEOUT);
    }
}
