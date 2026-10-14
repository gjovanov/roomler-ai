// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P8 — machine keys for the growth surface (`growth_keys`, D12).
//!
//! A platform admin mints a key of a KIND, scoped to that kind's routes
//! only: `ingest` (P8) authenticates the promotion pipeline's pushes to
//! `POST /api/admin/growth/ingest/*` and nothing else. (P10's `read` key is
//! the second kind this table is shaped for.)
//!
//! * **Only a hash is stored**: `hex(SHA-256(key))`, plus a short display
//!   prefix (`rgi_AbCd1234`) to recognise a key in a list. The key itself is
//!   answered ONCE, at mint, and exists nowhere on the server after that.
//!   The secret is 32 characters of nanoid's 64-symbol alphabet (192 bits),
//!   so an unsalted hash is enough — the same shape as purestat's `ps_` keys.
//! * **A miss is a miss**: an unknown, malformed or revoked key all look the
//!   same to the caller (the routes answer 404 for every one of them); this
//!   module only says whether a live key of the kind has this hash.
//! * Revocation is a timestamp, never a delete: the row stays as the record
//!   of what existed and when it stopped working.

use bson::{DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::base::DaoResult;

pub const GROWTH_KEYS: &str = "growth_keys";
/// Random characters after the kind's prefix.
pub const KEY_SECRET_LEN: usize = 32;
/// Characters of the key kept for display (the prefix plus 8).
pub const KEY_DISPLAY_EXTRA: usize = 8;
/// The longest label a key carries.
pub const MAX_LABEL_LEN: usize = 80;

/// What a key may do. The stored spelling is the snake_case variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthKeyKind {
    /// P8 — push posts and metric snapshots to the ingest routes.
    Ingest,
}

impl GrowthKeyKind {
    pub const ALL: [GrowthKeyKind; 1] = [GrowthKeyKind::Ingest];

    pub fn as_str(self) -> &'static str {
        match self {
            GrowthKeyKind::Ingest => "ingest",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ingest" => Some(GrowthKeyKind::Ingest),
            _ => None,
        }
    }

    /// The key's visible prefix: what a key of this kind starts with.
    pub fn prefix(self) -> &'static str {
        match self {
            GrowthKeyKind::Ingest => "rgi_",
        }
    }
}

/// One key, as stored. Never the key itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrowthKey {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    pub kind: GrowthKeyKind,
    pub label: String,
    /// The first characters of the key (`rgi_AbCd1234`): enough to recognise
    /// it in a list, nowhere near enough to use it.
    pub prefix: String,
    /// `hex(SHA-256(key))`.
    pub hash: String,
    /// The platform admin who minted it (an id, never an address).
    pub created_by: ObjectId,
    pub created_at: DateTime,
    #[serde(default)]
    pub revoked_at: Option<DateTime>,
    #[serde(default)]
    pub last_used_at: Option<DateTime>,
}

impl GrowthKey {
    pub fn live(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// `hex(SHA-256(raw))` — the only form a key is stored or looked up in.
pub fn hash_key(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// A fresh key of `kind`: its prefix and [`KEY_SECRET_LEN`] random
/// characters of nanoid's URL-safe alphabet.
pub fn new_raw_key(kind: GrowthKeyKind) -> String {
    format!("{}{}", kind.prefix(), nanoid::nanoid!(KEY_SECRET_LEN))
}

/// Whether `raw` has the shape of a key of `kind` — checked before any
/// lookup, so garbage costs no database read. Shape only: a well-formed
/// key can still be unknown or revoked.
pub fn well_formed(kind: GrowthKeyKind, raw: &str) -> bool {
    let Some(secret) = raw.strip_prefix(kind.prefix()) else {
        return false;
    };
    secret.len() == KEY_SECRET_LEN
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The display prefix of a raw key.
pub fn display_prefix(kind: GrowthKeyKind, raw: &str) -> String {
    raw.chars()
        .take(kind.prefix().len() + KEY_DISPLAY_EXTRA)
        .collect()
}

pub struct GrowthKeyDao {
    db: Database,
}

impl GrowthKeyDao {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    fn keys(&self) -> Collection<GrowthKey> {
        self.db.collection::<GrowthKey>(GROWTH_KEYS)
    }

    /// Mint a key: store its hash and prefix, answer the key itself ONCE.
    pub async fn mint(
        &self,
        kind: GrowthKeyKind,
        label: &str,
        created_by: ObjectId,
        now: DateTime,
    ) -> DaoResult<(GrowthKey, String)> {
        let raw = new_raw_key(kind);
        let key = GrowthKey {
            id: ObjectId::new(),
            kind,
            label: label.to_string(),
            prefix: display_prefix(kind, &raw),
            hash: hash_key(&raw),
            created_by,
            created_at: now,
            revoked_at: None,
            last_used_at: None,
        };
        self.keys().insert_one(&key).await?;
        Ok((key, raw))
    }

    /// Every key, newest first — revoked ones included: a dead key is a
    /// record, not clutter.
    pub async fn list(&self) -> DaoResult<Vec<GrowthKey>> {
        let keys: Vec<GrowthKey> = self
            .keys()
            .find(doc! {})
            .sort(doc! { "created_at": -1 })
            .await?
            .try_collect()
            .await?;
        Ok(keys)
    }

    /// The live keys of a kind.
    pub async fn count_live(&self, kind: GrowthKeyKind) -> DaoResult<u64> {
        Ok(self
            .db
            .collection::<Document>(GROWTH_KEYS)
            .count_documents(doc! { "kind": kind.as_str(), "revoked_at": bson::Bson::Null })
            .await?)
    }

    /// Revoke a key. `Ok(None)` when no key has this id; otherwise whether
    /// THIS call revoked it (a second revoke is a no-op, not an error).
    pub async fn revoke(&self, id: ObjectId, now: DateTime) -> DaoResult<Option<bool>> {
        let coll = self.db.collection::<Document>(GROWTH_KEYS);
        let r = coll
            .update_one(
                doc! { "_id": id, "revoked_at": bson::Bson::Null },
                doc! { "$set": { "revoked_at": now } },
            )
            .await?;
        if r.modified_count == 1 {
            return Ok(Some(true));
        }
        Ok(coll.find_one(doc! { "_id": id }).await?.map(|_| false))
    }

    /// The live key of `kind` this raw key hashes to, if any. Malformed
    /// input is a miss without a read.
    pub async fn find_live(&self, kind: GrowthKeyKind, raw: &str) -> DaoResult<Option<GrowthKey>> {
        if !well_formed(kind, raw) {
            return Ok(None);
        }
        Ok(self
            .keys()
            .find_one(doc! {
                "hash": hash_key(raw),
                "kind": kind.as_str(),
                "revoked_at": bson::Bson::Null,
            })
            .await?)
    }

    /// Record a use. Best-effort by the caller: a failed touch never fails
    /// the request it records.
    pub async fn touch(&self, id: ObjectId, now: DateTime) -> DaoResult<()> {
        self.db
            .collection::<Document>(GROWTH_KEYS)
            .update_one(doc! { "_id": id }, doc! { "$set": { "last_used_at": now } })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_has_its_kinds_prefix_and_a_long_random_secret() {
        let a = new_raw_key(GrowthKeyKind::Ingest);
        let b = new_raw_key(GrowthKeyKind::Ingest);
        assert!(a.starts_with("rgi_"), "{a}");
        assert_eq!(a.len(), 4 + KEY_SECRET_LEN);
        assert_ne!(a, b, "two mints, two keys");
        assert!(well_formed(GrowthKeyKind::Ingest, &a));
        assert_eq!(display_prefix(GrowthKeyKind::Ingest, &a).len(), 12);
        assert!(a.starts_with(&display_prefix(GrowthKeyKind::Ingest, &a)));
    }

    /// Only the hash is ever compared or stored, and it is stable.
    #[test]
    fn the_hash_is_the_keys_sha256_and_never_the_key() {
        let raw = "rgi_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
        let h = hash_key(raw);
        assert_eq!(h.len(), 64);
        assert_eq!(h, hash_key(raw));
        assert_ne!(h, hash_key("rgi_ABCDEFGHIJKLMNOPQRSTUVWXYZ012346"));
        assert!(!h.contains("ABCDEFGH"));
    }

    #[test]
    fn a_malformed_key_is_refused_before_any_lookup() {
        let ok = format!("rgi_{}", "a".repeat(KEY_SECRET_LEN));
        assert!(well_formed(GrowthKeyKind::Ingest, &ok));
        for bad in [
            String::new(),
            "rgi_".to_string(),
            format!("rgi_{}", "a".repeat(KEY_SECRET_LEN - 1)),
            format!("rgi_{}", "a".repeat(KEY_SECRET_LEN + 1)),
            format!("ps_{}", "a".repeat(KEY_SECRET_LEN)),
            format!("RGI_{}", "a".repeat(KEY_SECRET_LEN)),
            format!("rgi_{}!", "a".repeat(KEY_SECRET_LEN - 1)),
            format!("rgi_{} ", "a".repeat(KEY_SECRET_LEN - 1)),
        ] {
            assert!(!well_formed(GrowthKeyKind::Ingest, &bad), "{bad:?}");
        }
    }

    #[test]
    fn kind_spellings_are_locked() {
        assert_eq!(GrowthKeyKind::Ingest.as_str(), "ingest");
        assert_eq!(GrowthKeyKind::parse("ingest"), Some(GrowthKeyKind::Ingest));
        assert_eq!(
            GrowthKeyKind::parse("read"),
            None,
            "P10's kind is not minted yet"
        );
        assert_eq!(
            serde_json::to_string(&GrowthKeyKind::Ingest).unwrap(),
            "\"ingest\""
        );
    }

    #[test]
    fn a_stored_key_carries_no_secret() {
        let raw = new_raw_key(GrowthKeyKind::Ingest);
        let key = GrowthKey {
            id: ObjectId::new(),
            kind: GrowthKeyKind::Ingest,
            label: "pipeline".into(),
            prefix: display_prefix(GrowthKeyKind::Ingest, &raw),
            hash: hash_key(&raw),
            created_by: ObjectId::new(),
            created_at: DateTime::now(),
            revoked_at: None,
            last_used_at: None,
        };
        let doc = bson::to_document(&key).unwrap();
        let text = doc.to_string();
        assert!(!text.contains(&raw), "{text}");
        assert!(
            !text.contains(&raw[12..]),
            "no part past the display prefix"
        );
        let back: GrowthKey = bson::from_document(doc).unwrap();
        assert_eq!(back, key);
        assert!(back.live());
    }
}
