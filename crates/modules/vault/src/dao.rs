// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The vault's three collections.
//!
//! ⚠️ A metadata read never loads a sealed value: the version listing
//! projects `ciphertext`, `nonce` and `dek_wrapped` away, so describing a
//! secret cannot leak what it describes even through a log of the row.

use bson::{DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Database, options::ReturnDocument};
use roomler_ai_services::dao::base::{BaseDao, DaoError, DaoResult, escape_regex};
use serde::{Deserialize, Serialize};

use crate::model::{VaultAuditEvent, VaultSecret, VaultVersion};

/// The most secrets one listing answers with.
pub const MAX_LIST: i64 = 1000;
/// The most versions one verify opens.
pub const MAX_VERIFY: i64 = 10_000;

pub struct VaultSecretDao {
    pub base: BaseDao<VaultSecret>,
}

impl VaultSecretDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, VaultSecret::COLLECTION),
        }
    }

    /// A new secret. `DuplicateKey` when a live secret holds its path.
    pub async fn create(&self, secret: &VaultSecret) -> DaoResult<ObjectId> {
        self.base.insert_one(secret).await
    }

    /// The organization's live secrets, by path; under `prefix` when one is
    /// given (a folder: `staging/` lists what is in it).
    pub async fn list_live(
        &self,
        tenant_id: ObjectId,
        prefix: Option<&str>,
    ) -> DaoResult<Vec<VaultSecret>> {
        let mut filter = doc! { "tenant_id": tenant_id, "deleted_at": null };
        if let Some(p) = prefix.filter(|p| !p.is_empty()) {
            filter.insert("path", doc! { "$regex": format!("^{}", escape_regex(p)) });
        }
        let mut cursor = self
            .base
            .collection()
            .find(filter)
            .sort(doc! { "path": 1 })
            .limit(MAX_LIST)
            .await?;
        let mut out = Vec::new();
        while let Some(s) = cursor.try_next().await? {
            out.push(s);
        }
        Ok(out)
    }

    /// A live secret of this organization: `NotFound` for a tombstone, a
    /// foreign id or a bogus one alike.
    pub async fn get_live(&self, tenant_id: ObjectId, id: ObjectId) -> DaoResult<VaultSecret> {
        self.base.find_live_by_id_in_tenant(tenant_id, id).await
    }

    /// Set metadata on a live secret. `false` when there is none.
    pub async fn update_live(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
        mut set: Document,
    ) -> DaoResult<bool> {
        set.insert("updated_at", DateTime::now());
        let r = self
            .base
            .collection()
            .update_one(
                doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null },
                doc! { "$set": set },
            )
            .await?;
        Ok(r.matched_count > 0)
    }

    /// Tombstone a live secret. A compare-and-set: `true` only for the call
    /// that moved it from live — two concurrent deletes cannot both act on
    /// one secret.
    pub async fn tombstone(&self, tenant_id: ObjectId, id: ObjectId) -> DaoResult<bool> {
        let now = DateTime::now();
        let r = self
            .base
            .collection()
            .update_one(
                doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null },
                doc! { "$set": { "deleted_at": now, "updated_at": now } },
            )
            .await?;
        Ok(r.modified_count > 0)
    }

    /// The next version number of `field` on a live secret, handed out by
    /// an `$inc` — two writers never get one number. `None` when the secret
    /// is not live. `field` must be checked ([`crate::rrn::check_field`]):
    /// it is a key in the document.
    pub async fn next_version(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
        field: &str,
    ) -> DaoResult<Option<i64>> {
        let seq = format!("fields.{field}.seq");
        let after = self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null },
                doc! { "$inc": { seq.as_str(): 1_i64 } },
            )
            .return_document(ReturnDocument::After)
            .await?;
        Ok(after.and_then(|s| s.fields.get(field).map(|f| f.seq)))
    }

    /// Point `field` at version `n` once its row is stored — `$max`, so a
    /// slower writer of an older number never moves it back. `false` when
    /// the secret stopped being live meanwhile.
    pub async fn mark_current(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
        field: &str,
        n: i64,
    ) -> DaoResult<bool> {
        let now = DateTime::now();
        let r = self
            .base
            .collection()
            .update_one(
                doc! { "_id": id, "tenant_id": tenant_id, "deleted_at": null },
                doc! {
                    "$max": { format!("fields.{field}.current"): n },
                    "$set": { format!("fields.{field}.updated_at"): now, "updated_at": now },
                },
            )
            .await?;
        Ok(r.matched_count > 0)
    }

    pub async fn count_live(&self, tenant_id: ObjectId) -> DaoResult<u64> {
        self.base
            .count(doc! { "tenant_id": tenant_id, "deleted_at": null })
            .await
    }
}

/// A version as a metadata read sees it: everything but the sealed bytes.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VersionMeta {
    pub field: String,
    pub n: i64,
    pub kek: String,
    pub created_by: ObjectId,
    pub created_at: DateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime>,
}

pub struct VaultVersionDao {
    pub base: BaseDao<VaultVersion>,
}

impl VaultVersionDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, VaultVersion::COLLECTION),
        }
    }

    pub async fn insert(&self, v: &VaultVersion) -> DaoResult<ObjectId> {
        self.base.insert_one(v).await
    }

    /// A secret's versions, newest first per field — never their bytes.
    pub async fn list_meta(&self, secret_id: ObjectId) -> DaoResult<Vec<VersionMeta>> {
        let mut cursor = self
            .base
            .collection()
            .clone_with_type::<VersionMeta>()
            .find(doc! { "secret_id": secret_id })
            .projection(doc! { "ciphertext": 0, "nonce": 0, "dek_wrapped": 0 })
            .sort(doc! { "field": 1, "n": -1 })
            .await?;
        let mut out = Vec::new();
        while let Some(v) = cursor.try_next().await? {
            out.push(v);
        }
        Ok(out)
    }

    /// Every version of a secret, gone: what deleting it destroys.
    pub async fn delete_for_secret(&self, secret_id: ObjectId) -> DaoResult<u64> {
        self.base.hard_delete(doc! { "secret_id": secret_id }).await
    }

    pub async fn delete_one(&self, id: ObjectId) -> DaoResult<u64> {
        self.base.hard_delete(doc! { "_id": id }).await
    }

    /// Versions of `field` below `keep_from`, gone: a field keeps its last
    /// few, and an old value is no less sensitive for being old.
    pub async fn prune(&self, secret_id: ObjectId, field: &str, keep_from: i64) -> DaoResult<u64> {
        self.base
            .hard_delete(doc! { "secret_id": secret_id, "field": field, "n": { "$lt": keep_from } })
            .await
    }

    pub async fn count(&self, tenant_id: ObjectId) -> DaoResult<u64> {
        self.base.count(doc! { "tenant_id": tenant_id }).await
    }

    /// Versions whose data key was wrapped to another KEK than `recipient`.
    pub async fn count_other_kek(&self, tenant_id: ObjectId, recipient: &str) -> DaoResult<u64> {
        self.base
            .count(doc! { "tenant_id": tenant_id, "kek": { "$ne": recipient } })
            .await
    }

    /// The organization's versions, sealed, for a verify: at most
    /// [`MAX_VERIFY`].
    pub async fn scan(&self, tenant_id: ObjectId) -> DaoResult<Vec<VaultVersion>> {
        let mut cursor = self
            .base
            .collection()
            .find(doc! { "tenant_id": tenant_id })
            .sort(doc! { "_id": 1 })
            .limit(MAX_VERIFY)
            .await?;
        let mut out = Vec::new();
        while let Some(v) = cursor.try_next().await? {
            out.push(v);
        }
        Ok(out)
    }
}

pub struct VaultAuditDao {
    pub base: BaseDao<VaultAuditEvent>,
}

impl VaultAuditDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, VaultAuditEvent::COLLECTION),
        }
    }

    pub async fn record(&self, event: &VaultAuditEvent) -> DaoResult<ObjectId> {
        self.base.insert_one(event).await
    }

    /// The organization's decisions, newest first, before `before` when
    /// given (the next page).
    pub async fn recent(
        &self,
        tenant_id: ObjectId,
        before: Option<DateTime>,
        limit: i64,
    ) -> DaoResult<Vec<VaultAuditEvent>> {
        let mut filter = doc! { "tenant_id": tenant_id };
        if let Some(b) = before {
            filter.insert("at", doc! { "$lt": b });
        }
        let mut cursor = self
            .base
            .collection()
            .find(filter)
            .sort(doc! { "at": -1, "_id": -1 })
            .limit(limit)
            .await?;
        let mut out = Vec::new();
        while let Some(e) = cursor.try_next().await? {
            out.push(e);
        }
        Ok(out)
    }
}

/// `DuplicateKey` on the path index, told apart from any other collision.
pub fn is_path_taken(e: &DaoError) -> bool {
    matches!(e, DaoError::DuplicateKey(msg)
        if roomler_ai_services::dao::base::duplicate_key_is_on(msg, "path"))
}
