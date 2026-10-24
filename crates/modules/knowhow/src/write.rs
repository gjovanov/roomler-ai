// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! What every write to the map shares, a person's (the routes), the fleet's
//! (the sync), a device's (a scan) or a session's (a proposal): the stamp
//! its history row carries, the row appended after it, and an archive that
//! takes an entity's links with it.

use bson::{DateTime, Document, doc, oid::ObjectId};
use roomler_ai_services::dao::base::DaoResult;
use tracing::error;

use crate::KnowhowState;
use crate::dao::MAX_EDGES;
use crate::model::{
    KnowhowHistory, OP_ARCHIVE, Provenance, SOURCE_PERSON, SOURCE_SCAN, SOURCE_SESSION,
    SOURCE_SYNC, STATUS_ARCHIVED, TARGET_EDGE, TARGET_ENTITY,
};

/// The links an archive takes with it, per read.
const ARCHIVE_BATCH: usize = 500;

/// What every history row of one write shares: its organization, who wrote
/// (a person, the device whose scan it was, the session that proposed it, or
/// nobody for the sync), from which source, and when.
pub(crate) struct Stamp {
    pub tid: ObjectId,
    pub by: Option<ObjectId>,
    /// P4-3a-2 — the device, for a scan's write.
    pub device: Option<ObjectId>,
    /// P4-4-2 — the session, for a session's proposal, and why it proposed.
    pub session: Option<ObjectId>,
    pub why: Option<String>,
    pub source: &'static str,
    pub at: DateTime,
}

impl Stamp {
    /// A person's write, through the routes.
    pub(crate) fn person(tid: ObjectId, by: ObjectId) -> Self {
        Self {
            tid,
            by: Some(by),
            device: None,
            session: None,
            why: None,
            source: SOURCE_PERSON,
            at: DateTime::now(),
        }
    }

    /// The fleet's, mirrored by the sync (P4-2a): nobody wrote it.
    pub(crate) fn sync(tid: ObjectId) -> Self {
        Self {
            tid,
            by: None,
            device: None,
            session: None,
            why: None,
            source: SOURCE_SYNC,
            at: DateTime::now(),
        }
    }

    /// A device's scan (P4-3a-2): the device said it, and no person.
    pub(crate) fn scan(tid: ObjectId, device: ObjectId) -> Self {
        Self {
            tid,
            by: None,
            device: Some(device),
            session: None,
            why: None,
            source: SOURCE_SCAN,
            at: DateTime::now(),
        }
    }

    /// A session's proposal (P4-4-2): the session said it, and why, and no
    /// person.
    pub(crate) fn session(tid: ObjectId, session: ObjectId, why: String) -> Self {
        Self {
            tid,
            by: None,
            device: None,
            session: Some(session),
            why: Some(why),
            source: SOURCE_SESSION,
            at: DateTime::now(),
        }
    }

    /// The provenance a fact this write sets keeps.
    pub(crate) fn provenance(&self) -> Provenance<'static> {
        Provenance {
            source: self.source,
            by: self.by,
            device: self.device,
            session: self.session,
        }
    }

    /// The row for one write: to `target` `target_id`, touching
    /// `entity_ids`, making rev `rev`.
    pub(crate) fn row(
        &self,
        target: &str,
        target_id: ObjectId,
        entity_ids: Vec<ObjectId>,
        rev: i64,
        op: &str,
        set: Document,
    ) -> KnowhowHistory {
        KnowhowHistory {
            id: None,
            tenant_id: self.tid,
            target: target.into(),
            target_id,
            entity_ids,
            rev,
            op: op.into(),
            source: self.source.into(),
            by: self.by,
            device: self.device,
            session: self.session,
            why: self.why.clone(),
            at: self.at,
            set,
        }
    }

    /// The fields a write sets beside its own: who changed the row last, and
    /// when. Null `updated_by` for the sync.
    pub(crate) fn touched(&self) -> Document {
        doc! { "updated_by": self.by, "updated_at": self.at }
    }
}

/// Appended after the writes they record. A row that cannot be written is
/// said in the log, and the write stands: it happened, and the row it
/// changed still names who changed it.
pub(crate) async fn record(state: &KnowhowState, rows: Vec<KnowhowHistory>) {
    if let Err(e) = state.history.append(&rows).await {
        error!(%e, rows = rows.len(), "knowhow: writes stand but their history could not be appended");
    }
}

/// Archive a live entity at the rev it was read — off the map, its key free
/// for another, kept as a record — and every live link to it, each in the
/// history. `None` when it changed since, or is not live; otherwise how many
/// links went with it. Whatever the links' archive met, the rows it made are
/// recorded.
pub(crate) async fn archive_entity(
    state: &KnowhowState,
    stamp: &Stamp,
    id: ObjectId,
    rev: i64,
) -> DaoResult<Option<usize>> {
    let mut set = doc! { "status": STATUS_ARCHIVED, "deleted_at": stamp.at };
    set.extend(stamp.touched());
    let Some(after) = state.entities.update_at(stamp.tid, id, rev, set).await? else {
        return Ok(None);
    };
    let mut rows = vec![stamp.row(
        TARGET_ENTITY,
        id,
        vec![id],
        after.rev,
        OP_ARCHIVE,
        doc! { "status": STATUS_ARCHIVED },
    )];
    let links = archive_links(state, stamp, id, &mut rows).await;
    record(state, rows).await;
    links.map(Some)
}

/// Every live link of an archived entity, archived with it.
async fn archive_links(
    state: &KnowhowState,
    stamp: &Stamp,
    id: ObjectId,
    rows: &mut Vec<KnowhowHistory>,
) -> DaoResult<usize> {
    let mut archived = 0;
    for _ in 0..=(MAX_EDGES as usize / ARCHIVE_BATCH) {
        let batch = state
            .edges
            .touching(stamp.tid, &[id], ARCHIVE_BATCH as i64)
            .await?;
        if batch.is_empty() {
            break;
        }
        for e in batch {
            let Some(eid) = e.id else { continue };
            let archived_edge = state
                .edges
                .archive_live(stamp.tid, eid, stamp.by, stamp.at)
                .await?;
            if let Some(after) = archived_edge {
                archived += 1;
                rows.push(stamp.row(
                    TARGET_EDGE,
                    eid,
                    vec![after.from, after.to],
                    after.rev,
                    OP_ARCHIVE,
                    doc! { "status": STATUS_ARCHIVED, "cause": "entity_archived" },
                ));
            }
        }
    }
    Ok(archived)
}
