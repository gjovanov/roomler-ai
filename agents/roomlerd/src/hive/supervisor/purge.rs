// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2g-1 — `rc:hive.purge`: a session purged from this device, answered
//! per part (spec §3b, "Purges and retention").
//!
//! | Step | What | Part |
//! |---|---|---|
//! | 1 | a run of it here is fenced out at once (no model call, prompt or approval answer is taken) and stopped, and its task is waited for until it lets go | none: nothing is deleted while it runs |
//! | 2 | a terminal that mirrors it stops being mirrored, and what was kept of its ended terminal goes | `store` |
//! | 3 | the daemon's own copy: its events, index, tips, floors, membership, substitutes and blobs (`Store::purge`, which keeps its id, so a join, start or event for it is refused from then on), its runtime directory, what the device hosts, holds back and keeps in memory of it, and its replica streams | `store` |
//! | 4 | as the session's account, by this device's own record of where it was worked on (the debt `Store::purge` kept): its state directory, and the refs and index its checkpoints left in the folder's repository ([`super::super::purger`]) | `account` |
//!
//! ⚠️ What is deleted is this device's own record of the session, never the
//! frame's word: the frame names a session id and nothing else.
//!
//! ⚠️ Idempotent: a purge delivered again finds the store part done and the
//! account's debt where a failure left it, and retries only that.

use std::sync::Arc;

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{HivePurgePart, hive_limits};
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::store::WorkplaceRecord;
use tracing::{info, warn};

use super::{Input, SUPERSEDE_WAIT, Supervisor};
use crate::hive::purger;

impl Supervisor {
    /// `rc:hive.purge`: purge `session` here, and the answer, per part.
    pub(crate) async fn purge(
        self: &Arc<Self>,
        purge_id: ObjectId,
        session: ObjectId,
    ) -> ClientMsg {
        let (store, account, detail) = self.purge_parts(session).await;
        info!(
            purge = %purge_id, session = %session, store = store.as_str(),
            account = account.as_str(), detail = detail.as_deref().unwrap_or(""),
            "hive: purged"
        );
        ClientMsg::HivePurgeAck {
            purge_id,
            session_id: session,
            store,
            account,
            detail: detail.map(|d| d.chars().take(hive_limits::MAX_DETAIL_LEN).collect()),
        }
    }

    async fn purge_parts(
        self: &Arc<Self>,
        session: ObjectId,
    ) -> (HivePurgePart, HivePurgePart, Option<String>) {
        use HivePurgePart::{Done, Failed, NotThere};
        let sid = session.to_hex();
        let store = match &self.store {
            Ok(s) => s.clone(),
            Err(e) => return (Failed, Failed, Some(format!("this device's store: {e}"))),
        };
        // 1. Nothing is deleted while a run of it here goes on.
        if let Err(why) = self.stop_for_purge(session).await {
            return (Failed, Failed, Some(why));
        }
        // 2. A terminal that mirrors it.
        let adopted = self.adopt_purge(session);
        // 3. The daemon's own copy.
        let (held, already) = match (store.holds(&sid).await, store.is_purged(&sid).await) {
            (Ok(h), Ok(p)) => (h, p),
            (Err(e), _) | (_, Err(e)) => {
                return (Failed, Failed, Some(format!("this device's store: {e}")));
            }
        };
        if let Err(e) = store.purge(&sid).await {
            return (Failed, Failed, Some(format!("this device's store: {e}")));
        }
        let ran = match remove_run_dir(&self.runtime.join(&sid)) {
            Ok(ran) => ran,
            Err(e) => return (Failed, Failed, Some(e)),
        };
        let hosted = self.forget_hosted(session);
        self.forget_in_memory(session);
        let store_part = if held || already || ran || hosted || adopted {
            Done
        } else {
            NotThere
        };
        // 4. What its account holds of it, by this device's own record.
        match store.purge_debt(&sid).await {
            Ok(None) => (store_part, NotThere, None),
            Ok(Some(w)) => match self.purge_account(&sid, &w).await {
                Ok(done) => {
                    if let Err(e) = store.clear_purge_debt(&sid).await {
                        // Done, said so; the next purge of it finds nothing
                        // there and settles the debt then.
                        warn!(session = %sid, %e, "hive: the purge's account part was not marked done");
                    }
                    (store_part, if done.any() { Done } else { NotThere }, None)
                }
                Err(why) => (store_part, Failed, Some(why)),
            },
            Err(e) => (
                store_part,
                Failed,
                Some(format!("what the account holds of it: {e}")),
            ),
        }
    }

    /// A run of `session` here stops for the purge: fenced out at once, then
    /// waited for until its task has let go, as a newer fence's start waits
    /// ([`Self::supersede`]). `Err` when it has not.
    async fn stop_for_purge(&self, session: ObjectId) -> Result<(), String> {
        let (input, mut gone) = {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            let Some(l) = live.get_mut(&session) else {
                return Ok(());
            };
            l.fenced = true;
            (l.input.clone(), l.gone.clone())
        };
        info!(session = %session, "hive: purged — its run here stops first");
        if input
            .try_send(Input::Stop {
                reason: "purged from this device".into(),
            })
            .is_err()
        {
            warn!(session = %session, "hive: the purge's stop could not reach the session task");
        }
        // Nothing ever sends on it: it changes only by closing.
        match tokio::time::timeout(SUPERSEDE_WAIT, gone.changed()).await {
            Ok(_) => Ok(()),
            Err(_) => Err(format!(
                "its run here did not stop within {} s: nothing was deleted",
                SUPERSEDE_WAIT.as_secs()
            )),
        }
    }

    /// What the device hosts or holds back of `session` for a later start:
    /// forgotten. Whether there was any.
    fn forget_hosted(&self, session: ObjectId) -> bool {
        let sid = session.to_hex();
        let hosted = self
            .hosted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove_upto(&sid, u64::MAX);
        let queued = self.take_from_resume(session, u64::MAX);
        hosted || queued
    }

    /// What the device keeps of `session` in memory: its moves, memory and
    /// teleport prepares kept for a start, the server's lease words, the
    /// reports and frames held for a reconnect, its replica streams and its
    /// follow.
    fn forget_in_memory(&self, session: ObjectId) {
        fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
            m.lock().unwrap_or_else(|e| e.into_inner())
        }
        lock(&self.moves).retain(|(s, _), _| *s != session);
        lock(&self.memory).retain(|(s, _), _| *s != session);
        lock(&self.teleports).remove(&session);
        lock(&self.lease_words).retain(|(s, _), _| *s != session);
        lock(&self.reports).remove(&session);
        lock(&self.turns).remove(&session);
        lock(&self.approval_frames).remove(&session);
        lock(&self.follows).forget(session);
        let grants: Vec<ObjectId> = lock(&self.streams)
            .iter()
            .filter(|(_, s)| s.session_id == session)
            .map(|(g, _)| *g)
            .collect();
        for grant in grants {
            self.end_stream(grant, "purged");
        }
    }

    /// The account's part: run as the account the workplace record names,
    /// deleting what it names there.
    async fn purge_account(&self, sid: &str, w: &WorkplaceRecord) -> Result<purger::Done, String> {
        let who = self.who_for(&w.account)?;
        let req = purger::Request {
            v: purger::REQUEST_V,
            session: sid.to_string(),
            state_dir: w.state_dir.clone().into(),
            folder: w.folder.clone().into(),
        };
        purger::purge(&self.checkpoint_runner(), &who, &req).await
    }
}

/// A purged session's runtime directory, the daemon's: removed whole, never
/// through a link. Whether it was there.
fn remove_run_dir(dir: &std::path::Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", dir.display())),
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            // Not one this daemon made: removed as itself, never followed.
            std::fs::remove_file(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            Ok(true)
        }
        Ok(_) => {
            std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            Ok(true)
        }
    }
}
