// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2d-0 — the daemon's side of the device's seams
//! ([`crate::hive::device`]): the supervisor is what the device's parts reach
//! through [`ControlLink`] and [`DeviceSeams`], each method today's own, and
//! the connections a part's reply names, numbered ([`Conns`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::HiveRunState;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::checkpoint::WorkspaceSnap;
use tokio::sync::mpsc;
use tracing::warn;
use tunnel_core::transport::derp::DerpTunnelHandle;

use super::{Author, StartOrder, Supervisor};
use crate::hive::device::{Conn, ControlLink, Device, DeviceSeams, Unsent, speaks};
use crate::hive::toolbelt::Decision;

/// The primary connections the device's parts may answer on, each by the
/// number it was given as a frame of it was first handed over ([`Conn`]).
/// Held weakly here: a connection is the signalling loop's, kept open by it
/// and (in process) by the `Conn` of each frame not answered yet, and one
/// that closed is dropped from here at the next one numbered.
#[derive(Default)]
pub(crate) struct Conns {
    next: AtomicU64,
    held: Mutex<Vec<(u64, mpsc::WeakSender<ClientMsg>)>>,
}

impl Conns {
    fn held(&self) -> std::sync::MutexGuard<'_, Vec<(u64, mpsc::WeakSender<ClientMsg>)>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The connection `tx` is, by the number it was given or the next,
    /// held open by the `Conn` for as long as that lives: what the frame's
    /// sender itself did before P8-2d-0.
    pub(crate) fn of(&self, tx: mpsc::Sender<ClientMsg>) -> Conn {
        let mut held = self.held();
        let known = held
            .iter()
            .find(|(_, w)| w.upgrade().is_some_and(|s| s.same_channel(&tx)))
            .map(|(n, _)| *n);
        let n = match known {
            Some(n) => n,
            None => {
                held.retain(|(_, w)| w.upgrade().is_some_and(|s| !s.is_closed()));
                let n = self.next.fetch_add(1, Ordering::Relaxed);
                held.push((n, tx.downgrade()));
                n
            }
        };
        Conn::held(n, tx)
    }

    /// The connection numbered `n`, while it is open.
    fn get(&self, n: u64) -> Option<mpsc::Sender<ClientMsg>> {
        self.held()
            .iter()
            .find(|(m, _)| *m == n)
            .and_then(|(_, w)| w.upgrade())
    }

    /// `msg` on the connection `on` names, by its number, waiting while its
    /// queue is full: [`Unsent::Gone`] once it has closed. `on` is let go
    /// once answered.
    async fn reply(&self, on: Conn, msg: ClientMsg) -> Result<(), Unsent> {
        let tx = self.get(on.number()).ok_or(Unsent::Gone)?;
        let sent = tx.send(msg).await;
        drop(on);
        sent.map_err(|_| Unsent::Gone)
    }
}

impl Supervisor {
    /// FR-90 P8-2d-0 — the device's parts, as the daemon hands them a frame:
    /// what they hold, and this supervisor as their seams. Made for each
    /// hand-over, so a part's task holds the supervisor as it did when its
    /// code ran on it, and no cycle is held.
    pub(crate) fn device(self: &Arc<Self>) -> Arc<Device> {
        Arc::new(Device::new(
            Arc::clone(&self.parts),
            Arc::clone(self) as Arc<dyn ControlLink>,
            Arc::clone(self) as Arc<dyn DeviceSeams>,
        ))
    }

    /// The primary connection that is up now: where reports go.
    fn reporter_now(&self) -> Option<mpsc::Sender<ClientMsg>> {
        self.reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// A frame the device's parts do not speak, refused before it goes
/// (decision 69): the daemon speaks its own frames itself.
fn not_theirs(msg: &ClientMsg) -> Result<(), Unsent> {
    if speaks(msg) {
        return Ok(());
    }
    warn!(frame = ?std::mem::discriminant(msg), "hive: a frame the device's parts do not speak — not sent");
    Err(Unsent::NotTheirs)
}

#[async_trait::async_trait]
impl ControlLink for Supervisor {
    async fn reply(&self, on: Conn, msg: ClientMsg) -> Result<(), Unsent> {
        not_theirs(&msg)?;
        self.conns.reply(on, msg).await
    }

    async fn deliver(&self, msg: ClientMsg) -> Result<(), Unsent> {
        not_theirs(&msg)?;
        match self.reporter_now() {
            Some(tx) => tx.send(msg).await.map_err(|_| Unsent::Gone),
            None => Err(Unsent::Gone),
        }
    }

    fn send(&self, msg: ClientMsg) -> Result<(), Unsent> {
        not_theirs(&msg)?;
        match self.reporter_now() {
            Some(tx) => tx.try_send(msg).map_err(|_| Unsent::Full),
            None => Err(Unsent::Gone),
        }
    }
}

#[async_trait::async_trait]
impl DeviceSeams for Supervisor {
    fn holds_live(&self, session: ObjectId) -> bool {
        Supervisor::holds_live(self, session)
    }

    fn session_live(&self, session: ObjectId) -> bool {
        Supervisor::session_live(self, session)
    }

    fn run_state(&self, session: ObjectId) -> Option<HiveRunState> {
        Supervisor::run_state(self, session)
    }

    fn pending_approvals(&self, session: ObjectId) -> Vec<String> {
        Supervisor::pending_approvals(self, session)
    }

    fn prompt(
        &self,
        session: ObjectId,
        author: Option<Author>,
        text: String,
    ) -> Result<(), String> {
        Supervisor::prompt(self, session, author, text)
    }

    fn answer_approval(
        &self,
        session: ObjectId,
        approval: &str,
        decision: Decision,
        by: Author,
    ) -> Result<(), String> {
        Supervisor::answer_approval(self, session, approval, decision, by)
    }

    fn teleport_admitted(&self, session: ObjectId, fence: u64) -> bool {
        Supervisor::teleport_admitted(self, session, fence)
    }

    fn have_want_place(&self, session: ObjectId) -> Option<(StartOrder, String, PathBuf)> {
        Supervisor::have_want_place(self, session)
    }

    async fn haves_for(
        &self,
        order: &StartOrder,
        account: &str,
        folder: &Path,
        ws: &WorkspaceSnap,
    ) -> Option<Vec<String>> {
        Supervisor::haves_for(self, order, account, folder, ws).await
    }

    async fn pack_for_member(
        &self,
        session: ObjectId,
        sid: &str,
        commit: &str,
        haves: &[String],
    ) -> Result<(Vec<String>, Vec<u8>), &'static str> {
        Supervisor::pack_for_member(self, session, sid, commit, haves).await
    }

    fn derp_handle(&self) -> Option<DerpTunnelHandle> {
        Supervisor::derp_handle(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(grant: u8) -> ClientMsg {
        ClientMsg::HiveReplicaClose {
            grant_id: ObjectId::from_bytes([grant; 12]),
            reason: "test".into(),
        }
    }

    /// A connection keeps its number while it is open, another is given the
    /// next, and a reply goes on the one it names: not on the newest.
    #[tokio::test]
    async fn a_reply_goes_on_the_connection_it_names() {
        let conns = Conns::default();
        let (old, mut on_old) = mpsc::channel(4);
        let (new, mut on_new) = mpsc::channel(4);
        let a = conns.of(old.clone());
        let b = conns.of(new.clone());
        assert_ne!(a.number(), b.number());
        assert_eq!(
            conns.of(old.clone()).number(),
            a.number(),
            "the same connection, the same number"
        );
        conns.reply(a, closed(1)).await.unwrap();
        assert!(matches!(
            on_old.try_recv(),
            Ok(ClientMsg::HiveReplicaClose { grant_id, .. }) if grant_id == ObjectId::from_bytes([1; 12])
        ));
        assert!(
            on_new.try_recv().is_err(),
            "nothing on the newer connection"
        );
    }

    /// A `Conn` holds its connection open as the frame's own sender did: one
    /// handed its only sender stays open until it is answered on, and closes
    /// then; one let go unanswered (a grant replayed) closes at once.
    #[tokio::test]
    async fn a_conn_holds_its_connection_open_until_answered() {
        let conns = Conns::default();
        let (tx, mut rx) = mpsc::channel(4);
        let on = conns.of(tx);
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err() && !rx.is_closed(), "held open");
        conns.reply(on, closed(1)).await.unwrap();
        assert!(rx.recv().await.is_some(), "the answer");
        assert!(rx.recv().await.is_none(), "let go once answered");

        let (tx, mut rx) = mpsc::channel(4);
        drop(conns.of(tx));
        assert!(rx.recv().await.is_none(), "let go unanswered");
    }

    /// A connection that closed is gone to a reply, as a send on its own
    /// queue fails; so is a number never given. A closed one is forgotten at
    /// the next number given.
    #[tokio::test]
    async fn a_reply_on_a_closed_connection_is_gone() {
        let conns = Conns::default();
        let (tx, rx) = mpsc::channel(4);
        let on = conns.of(tx);
        drop(rx);
        assert_eq!(conns.reply(on, closed(1)).await, Err(Unsent::Gone));
        let (other, _keep) = mpsc::channel(4);
        let _ = conns.of(other);
        assert_eq!(conns.held().len(), 1, "the closed one is forgotten");
        assert_eq!(
            conns.reply(Conn::numbered(u64::MAX), closed(1)).await,
            Err(Unsent::Gone),
            "a number never given"
        );
    }

    /// Decision 69, on the daemon's own link: a frame the device's parts
    /// speak goes on the primary connection up now; one of the daemon's is
    /// refused, on every way of sending, and never goes — the frame sent
    /// after it arrives, and it did not before it.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_daemon_s_link_carries_the_parts_frames_alone() {
        use std::time::Duration;

        let mut r = super::super::tests::rig(true, 1);
        let theirs_not = ObjectId::new();
        let refused = || ClientMsg::HiveState {
            session_id: theirs_not,
            fence: 1,
            state: Some(HiveRunState::Idle),
            detail: None,
        };
        let link: &Supervisor = &r.sup;
        assert_eq!(ControlLink::send(link, refused()), Err(Unsent::NotTheirs));
        assert_eq!(
            ControlLink::deliver(link, refused()).await,
            Err(Unsent::NotTheirs)
        );
        assert_eq!(
            ControlLink::reply(link, Conn::numbered(0), refused()).await,
            Err(Unsent::NotTheirs)
        );
        assert_eq!(ControlLink::send(link, closed(42)), Ok(()));
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), r.reports.recv())
                .await
                .expect("the frame the parts speak, within 10 s")
                .expect("the connection is open");
            match msg {
                ClientMsg::HiveReplicaClose { grant_id, .. }
                    if grant_id == ObjectId::from_bytes([42; 12]) =>
                {
                    break;
                }
                ClientMsg::HiveState { session_id, .. } if session_id == theirs_not => {
                    panic!("a frame the device's parts do not speak was sent")
                }
                _ => {}
            }
        }
    }
}
