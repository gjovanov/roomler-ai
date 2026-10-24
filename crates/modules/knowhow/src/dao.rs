// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Knowhow's three collections: entities, edges, and the history of every
//! write to either.
//!
//! ⚠️ A write is a compare-and-set on `rev`: its filter names the rev the
//! writer read, its update `$inc`s it, and the row after is returned. A
//! writer that lost the race matches nothing and is told so (409), rather
//! than overwriting what it never saw.
//!
//! ⚠️ The history is appended AFTER the write it records, never before.
//! Without a transaction, a row written first could name a write that then
//! lost its compare-and-set; a row appended after can only go missing, and
//! the entity itself still names who changed it last.

use bson::{DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database, options::ReturnDocument};
use roomler_ai_services::dao::base::{BaseDao, DaoResult, escape_regex};
use serde::{Deserialize, Serialize};

use crate::model::{
    KnowhowEdge, KnowhowEntity, KnowhowHistory, KnowhowScan, SCAN_DELIVERED, SCAN_EXPIRED,
    SCAN_REQUESTED, SOURCE_SCAN, STATUS_ARCHIVED, ScanWritten,
};

/// The most live entities an organization's map holds.
pub const MAX_ENTITIES: u64 = 10_000;
/// The most live edges an organization's map holds.
pub const MAX_EDGES: u64 = 50_000;

/// An entity as a listing shows it: no attributes, no facts — a listing of
/// hundreds stays small whatever each entity holds.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EntityNode {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    pub kind: String,
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub env: Option<String>,
    pub status: String,
    pub rev: i64,
    pub updated_at: DateTime,
}

fn node_projection() -> Document {
    doc! { "kind": 1, "key": 1, "name": 1, "env": 1, "status": 1, "rev": 1, "updated_at": 1 }
}

/// What a search asks for: text in a key or a name, a kind, an environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct Search<'a> {
    pub text: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub env: Option<&'a str>,
}

pub struct EntityDao {
    pub base: BaseDao<KnowhowEntity>,
    nodes: Collection<EntityNode>,
}

impl EntityDao {
    pub fn new(db: &Database) -> Self {
        let base = BaseDao::new(db, KnowhowEntity::COLLECTION);
        let nodes = base.collection().clone_with_type::<EntityNode>();
        Self { base, nodes }
    }

    /// A new entity. `DuplicateKey` when a live one holds its kind and key.
    pub async fn create(&self, entity: &KnowhowEntity) -> DaoResult<ObjectId> {
        self.base.insert_one(entity).await
    }

    /// A live entity of this organization: `None` for an archived one, a
    /// foreign id and a bogus one alike.
    pub async fn get_live(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<Option<KnowhowEntity>> {
        self.base
            .find_one(doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null })
            .await
    }

    /// The live entity of this kind and key: what a chip and a session name.
    pub async fn find_live_by_key(
        &self,
        tenant_id: ObjectId,
        kind: &str,
        key: &str,
    ) -> DaoResult<Option<KnowhowEntity>> {
        self.base
            .find_one(doc! { "tenant_id": tenant_id, "kind": kind, "key": key, "deleted_at": null })
            .await
    }

    /// The live entities among `ids`, as a listing shows them.
    pub async fn live_nodes(
        &self,
        tenant_id: ObjectId,
        ids: &[ObjectId],
    ) -> DaoResult<Vec<EntityNode>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let cursor = self
            .nodes
            .find(
                doc! { "_id": { "$in": ids.to_vec() }, "tenant_id": tenant_id, "deleted_at": null },
            )
            .projection(node_projection())
            .await?;
        Ok(cursor.try_collect().await?)
    }

    /// The live entities a search matches, by kind and key, at most `limit`.
    /// The text is matched as written, never as a pattern
    /// ([`escape_regex`]), in a key or a name, case aside.
    pub async fn search(
        &self,
        tenant_id: ObjectId,
        query: Search<'_>,
        limit: i64,
    ) -> DaoResult<Vec<EntityNode>> {
        let mut filter = doc! { "tenant_id": tenant_id, "deleted_at": null };
        if let Some(kind) = query.kind {
            filter.insert("kind", kind);
        }
        if let Some(env) = query.env {
            filter.insert("env", env);
        }
        if let Some(text) = query.text {
            let pattern = doc! { "$regex": escape_regex(text), "$options": "i" };
            filter.insert(
                "$or",
                vec![doc! { "key": pattern.clone() }, doc! { "name": pattern }],
            );
        }
        let cursor = self
            .nodes
            .find(filter)
            .projection(node_projection())
            .sort(doc! { "kind": 1, "key": 1 })
            .limit(limit)
            .await?;
        Ok(cursor.try_collect().await?)
    }

    pub async fn count_live(&self, tenant_id: ObjectId) -> DaoResult<u64> {
        self.base
            .count(doc! { "tenant_id": tenant_id, "deleted_at": null })
            .await
    }

    /// P4-2a — every live entity of one kind in an organization, as a
    /// listing shows it: the machines a sync compares with the fleet's
    /// devices. Bounded by the map's own cap.
    pub async fn live_of_kind(
        &self,
        tenant_id: ObjectId,
        kind: &str,
    ) -> DaoResult<Vec<EntityNode>> {
        let cursor = self
            .nodes
            .find(doc! { "tenant_id": tenant_id, "deleted_at": null, "kind": kind })
            .projection(node_projection())
            .limit(MAX_ENTITIES as i64)
            .await?;
        Ok(cursor.try_collect().await?)
    }

    /// P4-3a-2 — the live entities a device's scans own (source `scan`, this
    /// device): what its next complete scan compares with what it found.
    /// Bounded by the map's own cap.
    pub async fn live_of_device(
        &self,
        tenant_id: ObjectId,
        device: ObjectId,
    ) -> DaoResult<Vec<KnowhowEntity>> {
        let cursor = self
            .base
            .collection()
            .find(doc! {
                "tenant_id": tenant_id, "device": device, "source": SOURCE_SCAN, "deleted_at": null,
            })
            .limit(MAX_ENTITIES as i64)
            .await?;
        Ok(cursor.try_collect().await?)
    }

    /// P4-2a — the organizations whose map holds a live entity of `kind`.
    pub async fn tenants_with(&self, kind: &str) -> DaoResult<Vec<ObjectId>> {
        Ok(self
            .base
            .collection()
            .distinct("tenant_id", doc! { "deleted_at": null, "kind": kind })
            .await?
            .into_iter()
            .filter_map(|b| b.as_object_id())
            .collect())
    }

    /// Set fields on a live entity, at the rev its writer read, and bump it.
    /// `None` when it changed since, or is not live.
    pub async fn update_at(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
        rev: i64,
        set: Document,
    ) -> DaoResult<Option<KnowhowEntity>> {
        Ok(self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null, "rev": rev },
                doc! { "$set": set, "$inc": { "rev": 1_i64 } },
            )
            .return_document(ReturnDocument::After)
            .await?)
    }
}

pub struct EdgeDao {
    pub base: BaseDao<KnowhowEdge>,
}

impl EdgeDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, KnowhowEdge::COLLECTION),
        }
    }

    /// A new edge. `DuplicateKey` when a live one already links the two by
    /// its kind.
    pub async fn create(&self, edge: &KnowhowEdge) -> DaoResult<ObjectId> {
        self.base.insert_one(edge).await
    }

    /// A live edge of this organization, as [`EntityDao::get_live`].
    pub async fn get_live(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<Option<KnowhowEdge>> {
        self.base
            .find_one(doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null })
            .await
    }

    pub async fn count_live(&self, tenant_id: ObjectId) -> DaoResult<u64> {
        self.base
            .count(doc! { "tenant_id": tenant_id, "deleted_at": null })
            .await
    }

    /// The live edges from or to any of `ids`, oldest first, at most
    /// `limit`: one step of a neighbourhood, both ways, each way on its own
    /// index.
    pub async fn touching(
        &self,
        tenant_id: ObjectId,
        ids: &[ObjectId],
        limit: i64,
    ) -> DaoResult<Vec<KnowhowEdge>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let filter = doc! { "$or": [
            { "tenant_id": tenant_id, "from": { "$in": ids.to_vec() }, "deleted_at": null },
            { "tenant_id": tenant_id, "to": { "$in": ids.to_vec() }, "deleted_at": null },
        ] };
        let cursor = self
            .base
            .collection()
            .find(filter)
            .sort(doc! { "_id": 1 })
            .limit(limit)
            .await?;
        Ok(cursor.try_collect().await?)
    }

    /// Set fields on a live edge at the rev its writer read, as
    /// [`EntityDao::update_at`].
    pub async fn update_at(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
        rev: i64,
        set: Document,
    ) -> DaoResult<Option<KnowhowEdge>> {
        Ok(self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null, "rev": rev },
                doc! { "$set": set, "$inc": { "rev": 1_i64 } },
            )
            .return_document(ReturnDocument::After)
            .await?)
    }

    /// P4-3a-2 — the live link `from` `kind` `to`, if there is one.
    pub async fn find_live(
        &self,
        tenant_id: ObjectId,
        from: ObjectId,
        kind: &str,
        to: ObjectId,
    ) -> DaoResult<Option<KnowhowEdge>> {
        self.base
            .find_one(doc! {
                "tenant_id": tenant_id, "from": from, "kind": kind, "to": to, "deleted_at": null,
            })
            .await
    }

    /// P4-3a-2 — the live links a device's scans own, as
    /// [`EntityDao::live_of_device`].
    pub async fn live_of_device(
        &self,
        tenant_id: ObjectId,
        device: ObjectId,
    ) -> DaoResult<Vec<KnowhowEdge>> {
        let cursor = self
            .base
            .collection()
            .find(doc! {
                "tenant_id": tenant_id, "device": device, "source": SOURCE_SCAN, "deleted_at": null,
            })
            .limit(MAX_EDGES as i64)
            .await?;
        Ok(cursor.try_collect().await?)
    }

    /// Archive a live edge whatever its rev: an entity's archive takes its
    /// links with it. `None` when it was not live. `by` is the person, or
    /// none for the sync.
    pub async fn archive_live(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
        by: Option<ObjectId>,
        now: DateTime,
    ) -> DaoResult<Option<KnowhowEdge>> {
        Ok(self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null },
                doc! {
                    "$set": { "status": STATUS_ARCHIVED, "deleted_at": now,
                              "updated_by": by, "updated_at": now },
                    "$inc": { "rev": 1_i64 },
                },
            )
            .return_document(ReturnDocument::After)
            .await?)
    }
}

pub struct HistoryDao {
    pub base: BaseDao<KnowhowHistory>,
}

impl HistoryDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, KnowhowHistory::COLLECTION),
        }
    }

    /// Append what writes did, after them.
    pub async fn append(&self, rows: &[KnowhowHistory]) -> DaoResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.base.collection().insert_many(rows).await?;
        Ok(())
    }

    /// P4-4-2 — how many proposals a session made, over its whole life and
    /// every fence: its writes in the history, one a proposal (decision 51).
    pub async fn count_by_session(&self, tenant_id: ObjectId, session: ObjectId) -> DaoResult<u64> {
        Ok(self
            .base
            .collection()
            .count_documents(doc! { "tenant_id": tenant_id, "session": session })
            .await?)
    }

    /// An organization's writes, newest first, at most `limit`; one
    /// entity's when it is named — its links' included.
    pub async fn list(
        &self,
        tenant_id: ObjectId,
        entity: Option<ObjectId>,
        limit: i64,
    ) -> DaoResult<Vec<KnowhowHistory>> {
        let mut filter = doc! { "tenant_id": tenant_id };
        if let Some(e) = entity {
            filter.insert("entity_ids", e);
        }
        let cursor = self
            .base
            .collection()
            .find(filter)
            .sort(doc! { "at": -1, "_id": -1 })
            .limit(limit)
            .await?;
        Ok(cursor.try_collect().await?)
    }
}

/// How long a scan waits for its answer before it is `expired`, in seconds.
pub const SCAN_ANSWER_SECS: i64 = 24 * 60 * 60;
/// How long a scan's record is kept, in seconds: 90 days.
pub const SCAN_KEEP_SECS: u64 = 90 * 24 * 60 * 60;

/// P4-3a — the scans asked of devices. Each step is a compare-and-set on the
/// row's state, so a device whose connection moved between pods is told once
/// and answered once.
pub struct ScanDao {
    pub base: BaseDao<KnowhowScan>,
}

impl ScanDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, KnowhowScan::COLLECTION),
        }
    }

    /// A new scan. `DuplicateKey` when the device already has one pending.
    pub async fn create(&self, scan: &KnowhowScan) -> DaoResult<ObjectId> {
        self.base.insert_one(scan).await
    }

    /// The device's pending scan, requested or delivered.
    pub async fn pending_for(
        &self,
        tenant_id: ObjectId,
        device_id: ObjectId,
    ) -> DaoResult<Option<KnowhowScan>> {
        self.base
            .find_one(doc! { "tenant_id": tenant_id, "device_id": device_id, "pending": true })
            .await
    }

    /// The scans asked of these devices and not yet delivered.
    pub async fn requested_for(&self, devices: &[ObjectId]) -> DaoResult<Vec<KnowhowScan>> {
        if devices.is_empty() {
            return Ok(Vec::new());
        }
        self.base
            .find_many(
                doc! { "device_id": { "$in": devices.to_vec() }, "state": SCAN_REQUESTED },
                Some(doc! { "requested_at": 1 }),
            )
            .await
    }

    /// Requested → delivered: `true` for the one caller that moved it, who
    /// then sends it.
    pub async fn deliver(&self, id: ObjectId, now: DateTime) -> DaoResult<bool> {
        let r = self
            .base
            .collection()
            .update_one(
                doc! { "_id": id, "state": SCAN_REQUESTED },
                doc! { "$set": { "state": SCAN_DELIVERED, "delivered_at": now } },
            )
            .await?;
        Ok(r.modified_count == 1)
    }

    /// Delivered → requested, for a scan that could not go out: the next
    /// sweep sends it.
    pub async fn undeliver(&self, id: ObjectId) -> DaoResult<()> {
        self.base
            .collection()
            .update_one(
                doc! { "_id": id, "state": SCAN_DELIVERED },
                doc! { "$set": { "state": SCAN_REQUESTED }, "$unset": { "delivered_at": "" } },
            )
            .await?;
        Ok(())
    }

    /// A delivered scan sent again (a hello, the connection it went to gone).
    pub async fn redelivered(&self, id: ObjectId, now: DateTime) -> DaoResult<()> {
        self.base
            .collection()
            .update_one(
                doc! { "_id": id, "state": SCAN_DELIVERED },
                doc! { "$set": { "delivered_at": now } },
            )
            .await?;
        Ok(())
    }

    /// The device's answer, once: `None` when the scan is not this device's,
    /// not pending, or answered already.
    pub async fn answer(
        &self,
        id: ObjectId,
        tenant_id: ObjectId,
        device_id: ObjectId,
        set: Document,
    ) -> DaoResult<Option<KnowhowScan>> {
        Ok(self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": id, "tenant_id": tenant_id, "device_id": device_id, "pending": true },
                doc! { "$set": set, "$unset": { "pending": "" } },
            )
            .return_document(ReturnDocument::After)
            .await?)
    }

    /// P4-3a-2 — what an answered scan wrote onto the map.
    pub async fn written(&self, id: ObjectId, written: &ScanWritten) -> DaoResult<()> {
        self.base
            .collection()
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "written": bson::to_bson(written)? } },
            )
            .await?;
        Ok(())
    }

    /// Every pending scan asked before `cutoff`: expired.
    pub async fn expire_before(&self, cutoff: DateTime) -> DaoResult<u64> {
        let r = self
            .base
            .collection()
            .update_many(
                doc! { "pending": true, "requested_at": { "$lt": cutoff } },
                doc! { "$set": { "state": SCAN_EXPIRED }, "$unset": { "pending": "" } },
            )
            .await?;
        Ok(r.modified_count)
    }

    /// A device's scans, newest first, at most `limit`.
    pub async fn list_for(
        &self,
        tenant_id: ObjectId,
        device_id: ObjectId,
        limit: i64,
    ) -> DaoResult<Vec<KnowhowScan>> {
        let cursor = self
            .base
            .collection()
            .find(doc! { "tenant_id": tenant_id, "device_id": device_id })
            .sort(doc! { "requested_at": -1, "_id": -1 })
            .limit(limit)
            .await?;
        Ok(cursor.try_collect().await?)
    }
}
