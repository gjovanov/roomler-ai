//! FR-27 — the live remote-control session registry.
//!
//! "Who is watching my screen, and how do I stop them" existed only inside the
//! Windows-native overlay: the daemon knew, but nothing else could ask. There
//! was no LocalAPI verb for a live session, so the desktop companion could not
//! render a banner on any platform and no thin client could offer a Disconnect.
//!
//! This is the shared handle that fixes that. One instance per daemon, created
//! in `run_cmd` and given to BOTH the LocalAPI's `DaemonState` (which reads it)
//! and every signalling loop's [`crate::indicator::ViewerIndicator`] (which
//! writes it, at the same two call sites that already raise and clear the
//! on-screen indicator — so a session cannot appear in one and not the other).
//!
//! ⚠️ Each entry carries its OWN loop's kill channel, not a process-global one.
//! A multi-org daemon runs one signalling loop per enrollment, and a session
//! belongs to exactly one of them; a single shared sender would have to guess.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bson::oid::ObjectId;
use tunnel_core::localapi::RcSessionInfo;

/// The channel a signalling loop polls for "the person at this device wants
/// this session gone" — the same one the Windows overlay's Disconnect button
/// fires through, so a LocalAPI disconnect and an overlay click take the
/// identical teardown path.
pub type KillSender = tokio::sync::mpsc::Sender<ObjectId>;

#[derive(Clone)]
struct Entry {
    controller_name: String,
    permissions: String,
    org: String,
    started_at_ms: u64,
    kill: KillSender,
}

/// Cheap to clone; every clone sees the same map.
#[derive(Clone, Default)]
pub struct RcSessionRegistry {
    inner: Arc<Mutex<HashMap<ObjectId, Entry>>>,
}

impl RcSessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A session became visible on this device. Idempotent — a re-announce
    /// replaces the entry rather than duplicating it, matching
    /// `ViewerIndicator::show_session`.
    pub fn insert(
        &self,
        session: ObjectId,
        controller_name: String,
        permissions: String,
        org: String,
        kill: KillSender,
    ) {
        let started_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.inner.lock().unwrap().insert(
            session,
            Entry {
                controller_name,
                permissions,
                org,
                started_at_ms,
                kill,
            },
        );
    }

    pub fn remove(&self, session: &ObjectId) {
        self.inner.lock().unwrap().remove(session);
    }

    /// Snapshot for the LocalAPI, oldest first — a banner listing several
    /// viewers should not reorder itself between polls.
    pub fn list(&self) -> Vec<RcSessionInfo> {
        let mut out: Vec<(u64, RcSessionInfo)> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| {
                (
                    e.started_at_ms,
                    RcSessionInfo {
                        session_id: id.to_hex(),
                        controller_name: e.controller_name.clone(),
                        permissions: e.permissions.clone(),
                        org: e.org.clone(),
                        started_at_ms: e.started_at_ms,
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.session_id.cmp(&b.1.session_id))
        });
        out.into_iter().map(|(_, s)| s).collect()
    }

    /// Ask the owning signalling loop to tear `session` down. `false` = no such
    /// live session (already gone, or never on this device).
    ///
    /// Deliberately does NOT remove the entry: the signalling loop owns the
    /// lifecycle and clears it through `hide_session` once the peer is
    /// actually closed. Removing it here would make the banner disappear
    /// before the session had, which is the wrong way round for a control the
    /// operator is watching for an effect.
    pub fn disconnect(&self, session: &ObjectId) -> bool {
        let Some(kill) = self
            .inner
            .lock()
            .unwrap()
            .get(session)
            .map(|e| e.kill.clone())
        else {
            return false;
        };
        // `try_send` on purpose: this runs on the LocalAPI's sync dispatch, and
        // a full 4-slot kill queue means several teardowns are already in
        // flight — reporting that honestly beats blocking the control socket.
        match kill.try_send(*session) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(%session, %e, "rc disconnect could not be queued");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg_with(
        n: usize,
    ) -> (
        RcSessionRegistry,
        Vec<ObjectId>,
        tokio::sync::mpsc::Receiver<ObjectId>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let reg = RcSessionRegistry::new();
        let ids: Vec<ObjectId> = (0..n).map(|_| ObjectId::new()).collect();
        for (i, id) in ids.iter().enumerate() {
            reg.insert(
                *id,
                format!("viewer{i}"),
                "VIEW | INPUT".into(),
                String::new(),
                tx.clone(),
            );
        }
        (reg, ids, rx)
    }

    #[tokio::test]
    async fn disconnect_routes_to_the_owning_loop() {
        let (reg, ids, mut rx) = reg_with(2);
        assert!(reg.disconnect(&ids[1]));
        assert_eq!(rx.recv().await, Some(ids[1]));
        // The entry SURVIVES the request — the signalling loop clears it once
        // the peer is really closed, so the banner does not vanish early.
        assert_eq!(reg.list().len(), 2);
    }

    #[tokio::test]
    async fn disconnecting_an_unknown_session_is_a_clean_false() {
        let (reg, _ids, _rx) = reg_with(1);
        assert!(!reg.disconnect(&ObjectId::new()));
    }

    /// A dead loop (its receiver dropped) must report failure rather than
    /// silently swallowing the click — the operator is watching for an effect.
    #[tokio::test]
    async fn a_closed_kill_channel_reports_failure() {
        let (reg, ids, rx) = reg_with(1);
        drop(rx);
        assert!(!reg.disconnect(&ids[0]));
    }

    #[test]
    fn list_is_stable_and_carries_the_grant() {
        let (reg, ids, _rx) = reg_with(3);
        let a = reg.list();
        let b = reg.list();
        assert_eq!(a, b, "two polls must not reorder the banner");
        assert_eq!(a.len(), 3);
        assert!(a.iter().all(|s| s.permissions == "VIEW | INPUT"));
        let listed: std::collections::HashSet<String> =
            a.iter().map(|s| s.session_id.clone()).collect();
        for id in &ids {
            assert!(listed.contains(&id.to_hex()));
        }
    }

    #[test]
    fn remove_drops_the_session() {
        let (reg, ids, _rx) = reg_with(2);
        reg.remove(&ids[0]);
        let left = reg.list();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].session_id, ids[1].to_hex());
    }
}
