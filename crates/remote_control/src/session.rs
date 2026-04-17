//! Per-session state machine.
//!
//! Each `RemoteSession` has a lightweight in-memory state object held by the
//! `Hub` while the session is non-terminal. The MongoDB `RemoteSession`
//! document is the durable record; this struct is the live cache.

use bson::{DateTime, oid::ObjectId};
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::consent::ConsentSlot;
use crate::error::{Error, Result};
use crate::models::{EndReason, SessionPhase, SessionStats};
use crate::permissions::Permissions;
use crate::signaling::ServerMsg;

/// Channel used to push messages to a connected client (agent or controller).
/// The WS layer owns the receiving half and forwards to the socket.
pub type ClientTx = mpsc::Sender<ServerMsg>;

pub struct LiveSession {
    pub id: ObjectId,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub controller_user_id: ObjectId,
    pub permissions: Permissions,
    pub phase: SessionPhase,
    pub created_at: DateTime,
    pub started_at: Option<DateTime>,
    pub stats: SessionStats,

    /// One-shot consent slot, taken when the agent replies.
    pub consent_slot: Option<ConsentSlot>,

    /// Watchers (view-only). user_id → tx
    pub watchers: HashMap<ObjectId, ClientTx>,

    /// The active controller's tx. None if disconnected.
    pub controller_tx: Option<ClientTx>,
}

impl LiveSession {
    pub fn new(
        id: ObjectId,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        controller_user_id: ObjectId,
        permissions: Permissions,
        controller_tx: ClientTx,
    ) -> (Self, crate::consent::ConsentWaiter) {
        let (slot, waiter) = ConsentSlot::new();
        let s = Self {
            id,
            agent_id,
            tenant_id,
            controller_user_id,
            permissions,
            phase: SessionPhase::Pending,
            created_at: DateTime::now(),
            started_at: None,
            stats: SessionStats::default(),
            consent_slot: Some(slot),
            watchers: HashMap::new(),
            controller_tx: Some(controller_tx),
        };
        (s, waiter)
    }

    /// Transition guard: only certain transitions are legal.
    pub fn transition(&mut self, to: SessionPhase) -> Result<()> {
        use SessionPhase::*;
        let ok = matches!(
            (self.phase, to),
            (Pending, AwaitingConsent)
                | (AwaitingConsent, Negotiating)
                | (AwaitingConsent, Closed)
                | (Negotiating, Active)
                | (Negotiating, Closed)
                | (Active, Closed)
                | (Pending, Closed)
        );
        if !ok {
            return Err(Error::BadPhase(self.id.to_hex(), phase_name(self.phase)));
        }
        self.phase = to;
        if to == SessionPhase::Active {
            self.started_at = Some(DateTime::now());
        }
        Ok(())
    }

    pub fn is_terminal(&self) -> bool {
        self.phase == SessionPhase::Closed
    }
}

fn phase_name(p: SessionPhase) -> &'static str {
    match p {
        SessionPhase::Pending => "pending",
        SessionPhase::AwaitingConsent => "awaiting_consent",
        SessionPhase::Negotiating => "negotiating",
        SessionPhase::Active => "active",
        SessionPhase::Closed => "closed",
    }
}

/// Why a session ended (used by the Hub to write final Mongo state).
#[derive(Debug, Clone)]
pub struct CloseRecord {
    pub session_id: ObjectId,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub reason: EndReason,
    pub stats: SessionStats,
    pub ended_at: DateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> LiveSession {
        let (tx, _rx) = mpsc::channel(8);
        let (s, _) = LiveSession::new(
            ObjectId::new(),
            ObjectId::new(),
            ObjectId::new(),
            ObjectId::new(),
            Permissions::default(),
            tx,
        );
        s
    }

    #[test]
    fn legal_path() {
        let mut s = fixture();
        s.transition(SessionPhase::AwaitingConsent).unwrap();
        s.transition(SessionPhase::Negotiating).unwrap();
        s.transition(SessionPhase::Active).unwrap();
        s.transition(SessionPhase::Closed).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn cannot_skip_phases() {
        let mut s = fixture();
        assert!(s.transition(SessionPhase::Active).is_err());
    }

    #[test]
    fn closed_is_dead_end() {
        let mut s = fixture();
        s.transition(SessionPhase::Closed).unwrap();
        assert!(s.transition(SessionPhase::Active).is_err());
    }
}
