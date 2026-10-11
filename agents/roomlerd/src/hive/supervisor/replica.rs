// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2c-3b — the device's half of membership: its own gates on
//! `rc:hive.replica.join`, the membership its store keeps, and what it tells
//! the server back, each copy's tip (spec §3b).
//!
//! | Gate, in order | Refused with | Why |
//! |---|---|---|
//! | the primary enrollment's connection | `secondary_org` | a device holds copies for its primary organization only: another org's admin must not place its sessions on the owner's disk |
//! | the owner's `hive_replica` | `replica_disabled` | the gate that survives a compromised server |
//! | a role this build can name | `other` | a newer server's word is still a refusal, never a copy held for a reason nobody here read |
//! | the owner's `hive_archive`, for an archive | `archive_disabled` | an administrator designates; the device's owner offers |
//! | the store, open | `other` | |
//! | `hive_store_quota_mib` | `quota` | |
//! | not purged here | `purged` | removal is final |
//! | not below the floor | `stale_fence` | a join from before a promotion must not take the session back |
//!
//! A join that passes raises the floor to its fence and keeps the membership,
//! in one transaction (`Store::join`). The copy itself arrives with the
//! carrier (P2d).

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{
    HiveCheckpointMark, HiveJoinRefusal, HiveReplicaManifestEntry, HiveReplicaRole, replica_limits,
};
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::GENESIS;
use roomler_hive_node::store::StoreError;
use tracing::{info, warn};

use super::Supervisor;
use crate::hive::gates::HiveConfig;
use crate::hive::store::{StoreHandle, WriterError};

const MIB: u64 = 1024 * 1024;

/// The device's answer to a join: no refusal means it holds the session now;
/// otherwise the gate, and a few words for the owner.
pub(super) type JoinAnswer = (Option<HiveJoinRefusal>, Option<String>);

/// The device's own gates on a join, in order, then the join itself.
pub(super) async fn decide(
    cfg: &HiveConfig,
    store: Result<&StoreHandle, &String>,
    primary: bool,
    session: ObjectId,
    fence: u64,
    role: Option<HiveReplicaRole>,
) -> JoinAnswer {
    let refuse = |word: HiveJoinRefusal, why: String| (Some(word), Some(why));
    if !primary {
        return refuse(
            HiveJoinRefusal::SecondaryOrg,
            "this device holds copies for its primary organization only".into(),
        );
    }
    if !cfg.replica {
        return refuse(
            HiveJoinRefusal::ReplicaDisabled,
            "hive_replica is off on this device".into(),
        );
    }
    let Some(role) = role else {
        return refuse(
            HiveJoinRefusal::Other,
            "a role this build cannot name".into(),
        );
    };
    if role == HiveReplicaRole::Archive && !cfg.archive {
        return refuse(
            HiveJoinRefusal::ArchiveDisabled,
            "hive_archive is off on this device".into(),
        );
    }
    let store = match store {
        Ok(s) => s,
        Err(e) => {
            return refuse(
                HiveJoinRefusal::Other,
                format!("the replica store is unavailable: {e}"),
            );
        }
    };
    if let Some(quota) = cfg.store_quota_mib {
        match store.size_bytes().await {
            Ok(size) if size >= u64::from(quota) * MIB => {
                return refuse(
                    HiveJoinRefusal::Quota,
                    format!("the replica store is at its quota ({quota} MiB)"),
                );
            }
            Ok(_) => {}
            Err(e) => {
                return refuse(
                    HiveJoinRefusal::Other,
                    format!("the replica store's size could not be read: {e}"),
                );
            }
        }
    }
    match store.join(&session.to_hex(), role.as_str(), fence).await {
        Ok(_) => (None, None),
        Err(WriterError::Store(StoreError::Purged { .. })) => refuse(
            HiveJoinRefusal::Purged,
            "this device purged the session, and removal is final".into(),
        ),
        Err(WriterError::Store(StoreError::StaleJoin { floor, .. })) => refuse(
            HiveJoinRefusal::StaleFence,
            format!("this device holds the session at fence {floor}"),
        ),
        Err(e) => refuse(
            HiveJoinRefusal::Other,
            format!("the replica store refused the join: {e}"),
        ),
    }
}

/// Where this device's copy of `session` ends, as the replica frames carry
/// it: the tip, and the newest checkpoint. A member that holds nothing yet
/// says `seq` 0 at its floor, which is how the server learns it joined and
/// waits for the carrier.
pub(super) async fn tip_of(
    store: &StoreHandle,
    session: ObjectId,
) -> Option<HiveReplicaManifestEntry> {
    let sid = session.to_hex();
    let (fence, seq, hash) = match store.chain_tip(&sid).await {
        Ok(Some(t)) => (t.fence, t.seq, t.hash),
        Ok(None) => (store.floor(&sid).await.ok().flatten()?, 0, GENESIS),
        Err(e) => {
            warn!(session = %session, %e, "hive: a copy's tip could not be read");
            return None;
        }
    };
    let checkpoint = store
        .last_of_kind(&sid, "checkpoint")
        .await
        .ok()
        .flatten()
        .map(|env| HiveCheckpointMark {
            seq: env.seq,
            hash: hex::encode(env.hash()),
        });
    Some(HiveReplicaManifestEntry {
        session_id: session,
        fence,
        seq,
        hash: hex::encode(hash),
        checkpoint,
    })
}

impl Supervisor {
    /// The device's answer to a join, from its config and store, logged.
    pub(super) async fn answer_join(
        &self,
        primary: bool,
        session: ObjectId,
        fence: u64,
        role: Option<HiveReplicaRole>,
    ) -> JoinAnswer {
        let answer = decide(
            &self.cfg,
            self.store.as_ref(),
            primary,
            session,
            fence,
            role,
        )
        .await;
        info!(
            session = %session, fence, role = role.map(HiveReplicaRole::as_str),
            refused = answer.0.map(HiveJoinRefusal::as_str), detail = answer.1.as_deref(),
            "hive: a join answered"
        );
        answer
    }

    /// Whether this device has copies to tell the server about: its owner
    /// lets it hold copies, or it runs a replicated session. Every other
    /// device sends no replica frame, so a server from before P2c-3 never
    /// hears a word it cannot read.
    pub(super) fn reports_copies(&self) -> bool {
        self.cfg.replica
            || self
                .live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .any(|l| l.replicated)
    }

    /// Every session this device holds a copy of, as a member or as the
    /// replicated session's primary, each with its tip: what
    /// `rc:hive.replica.manifest` carries. Newest first, at most
    /// [`replica_limits::MAX_REPLICA_MANIFEST`].
    pub(super) async fn replica_manifest(&self) -> Vec<HiveReplicaManifestEntry> {
        let Ok(store) = &self.store else {
            return Vec::new();
        };
        let mut ids: Vec<ObjectId> = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, l)| l.replicated)
            .map(|(id, _)| *id)
            .collect();
        match store.memberships().await {
            Ok(held) => ids.extend(
                held.iter()
                    .filter_map(|m| ObjectId::parse_str(&m.session).ok()),
            ),
            Err(e) => warn!(%e, "hive: the memberships could not be read for the manifest"),
        }
        // An ObjectId sorts by when it was made: newest first.
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.dedup();
        ids.truncate(replica_limits::MAX_REPLICA_MANIFEST);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(tip) = tip_of(store, id).await {
                out.push(tip);
            }
        }
        out
    }

    /// A replicated session's checkpoint was kept: tell the server where its
    /// copy ends now (`rc:hive.replica.tip`).
    pub(super) async fn report_tip(&self, session: ObjectId) {
        let Ok(store) = &self.store else {
            return;
        };
        let Some(tip) = tip_of(store, session).await else {
            return;
        };
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ClientMsg::HiveReplicaTip {
                session_id: tip.session_id,
                fence: tip.fence,
                seq: tip.seq,
                hash: tip.hash,
                checkpoint: tip.checkpoint,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_hive_node::{EventEnvelope, TranscriptEvent};

    fn cfg(replica: bool, archive: bool, quota: Option<u32>) -> HiveConfig {
        let mut agent = crate::config::test_fixture();
        agent.hive_replica = replica;
        agent.hive_archive = archive;
        agent.hive_store_quota_mib = quota;
        HiveConfig::from_agent(&agent)
    }

    async fn answer(
        cfg: &HiveConfig,
        store: &StoreHandle,
        primary: bool,
        session: ObjectId,
        fence: u64,
        role: Option<HiveReplicaRole>,
    ) -> Option<HiveJoinRefusal> {
        decide(cfg, Ok(store), primary, session, fence, role)
            .await
            .0
    }

    /// Each gate in order, and a join that passes them keeps the membership
    /// and raises the floor.
    #[tokio::test]
    async fn a_join_passes_the_devices_own_gates_or_names_the_one_it_failed() {
        let store = StoreHandle::spawn(None).unwrap();
        let sid = ObjectId::new();
        let owner = Some(HiveReplicaRole::Owner);
        let archive = Some(HiveReplicaRole::Archive);
        let on = cfg(true, false, None);
        assert_eq!(
            answer(&on, &store, false, sid, 1, owner).await,
            Some(HiveJoinRefusal::SecondaryOrg)
        );
        assert_eq!(
            answer(&cfg(false, false, None), &store, true, sid, 1, owner).await,
            Some(HiveJoinRefusal::ReplicaDisabled)
        );
        assert_eq!(
            answer(&on, &store, true, sid, 1, None).await,
            Some(HiveJoinRefusal::Other),
            "a role this build cannot name"
        );
        assert_eq!(
            answer(&on, &store, true, sid, 1, archive).await,
            Some(HiveJoinRefusal::ArchiveDisabled)
        );
        let missing = "no store".to_string();
        assert_eq!(
            decide(&on, Err(&missing), true, sid, 1, owner).await.0,
            Some(HiveJoinRefusal::Other)
        );
        assert_eq!(
            answer(&cfg(true, false, Some(0)), &store, true, sid, 1, owner).await,
            Some(HiveJoinRefusal::Quota),
            "a quota the store is already at"
        );
        assert!(
            store.memberships().await.unwrap().is_empty(),
            "nothing refused was kept"
        );

        assert_eq!(answer(&on, &store, true, sid, 2, owner).await, None);
        assert_eq!(store.floor(&sid.to_hex()).await.unwrap(), Some(2));
        assert_eq!(
            answer(&cfg(true, true, None), &store, true, sid, 2, archive).await,
            None,
            "an archive whose owner offers it"
        );
        assert_eq!(
            answer(&on, &store, true, sid, 1, owner).await,
            Some(HiveJoinRefusal::StaleFence),
            "a join from before the floor"
        );
        store.purge(&sid.to_hex()).await.unwrap();
        assert_eq!(
            answer(&on, &store, true, sid, 3, owner).await,
            Some(HiveJoinRefusal::Purged)
        );
    }

    /// A member that holds nothing yet says `seq` 0 at its floor; one that
    /// holds events says its tip and its newest checkpoint.
    #[tokio::test]
    async fn a_copys_tip_is_its_chain_end_and_newest_checkpoint() {
        let store = StoreHandle::spawn(None).unwrap();
        let sid = ObjectId::new();
        assert!(tip_of(&store, sid).await.is_none(), "not held at all");
        store.join(&sid.to_hex(), "owner", 3).await.unwrap();
        let empty = tip_of(&store, sid).await.unwrap();
        assert_eq!(
            (
                empty.fence,
                empty.seq,
                empty.hash.as_str(),
                empty.checkpoint
            ),
            (3, 0, "0".repeat(64).as_str(), None)
        );

        let first = EventEnvelope::next(
            &sid.to_hex(),
            None,
            3,
            1,
            &TranscriptEvent::Note { text: "one".into() },
        );
        let tip = store.apply(first).await.unwrap();
        let held = tip_of(&store, sid).await.unwrap();
        assert_eq!((held.fence, held.seq), (3, 1));
        assert_eq!(held.hash, hex::encode(tip.hash));
        assert!(roomler_ai_remote_control::hive::is_hash_hex(&held.hash));
        assert_eq!(held.checkpoint, None, "no checkpoint yet");
    }
}
