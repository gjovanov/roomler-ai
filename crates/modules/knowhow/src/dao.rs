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

use crate::model::{KnowhowEdge, KnowhowEntity, KnowhowHistory, STATUS_ARCHIVED};

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

    /// Archive a live edge whatever its rev: an entity's archive takes its
    /// links with it. `None` when it was not live.
    pub async fn archive_live(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
        by: ObjectId,
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
