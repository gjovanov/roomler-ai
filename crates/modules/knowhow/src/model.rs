// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Knowhow's rows (design §9.1), and the rules a write is held to.
//!
//! An entity is one thing an organization has — a machine, a repository, a
//! database — named by its `kind` and `key`, unique among the live of its
//! kind. An edge says how two are related (`service runs_on machine`). Every
//! write to either is a compare-and-set on its `rev`, appended to
//! `knowhow_history` ([`crate::dao`]).
//!
//! ⚠️ A kind, an edge kind, a status and a source are strings in the row,
//! never enums. A newer server writes values this build has never heard of,
//! and a reader shows an unknown one rather than refusing the row; only a
//! WRITE is held to the lists below.
//!
//! ⚠️ One spelling per name, as the vault's paths have: a key is lowercase by
//! rule, not folded. Two keys that differ only in case would be one thing to
//! a person and two to the unique index.

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every kind of entity a write may name (design §9.1).
pub const KINDS: &[&str] = &[
    "machine",
    "repo",
    "checkout",
    "database",
    "cluster",
    "namespace",
    "service",
    "endpoint",
    "bucket",
    "cloud_account",
    "domain",
    "environment",
    "team",
    "doc",
];

/// The kinds only the server writes. A machine mirrors a device, and it is
/// P4-2's sync that records one, with its node id and overlay address beside
/// its name (design §9.2: labels move and are reused). A person describes a
/// host that is no device as an `endpoint`.
pub const SYNCED_KINDS: &[&str] = &["machine"];

/// Every kind of edge a write may name (design §9.1). No rule pairs an edge
/// kind with the kinds at its ends in P4-1.
pub const EDGE_KINDS: &[&str] = &[
    "runs_on",
    "checked_out_at",
    "deployed_to",
    "depends_on",
    "reads",
    "writes",
    "part_of",
    "owned_by",
    "documented_in",
    "authenticates_with",
    "reachable_via",
];

/// A live entity, edge or fact, believed.
pub const STATUS_ACTIVE: &str = "active";
/// A session's claim, until a probe or a person confirms it (P4-4).
pub const STATUS_PROPOSED: &str = "proposed";
/// Unverified for longer than the organization allows (P4-5).
pub const STATUS_STALE: &str = "stale";
/// Tombstoned: kept as a record, no longer on the map.
pub const STATUS_ARCHIVED: &str = "archived";

/// A person's write, through the routes (P4-1).
pub const SOURCE_PERSON: &str = "person";
/// Fleet and network, mirrored by the server (P4-2).
pub const SOURCE_SYNC: &str = "sync";
/// A device's own report of what its owner's roots hold (P4-3): its claim.
pub const SOURCE_SCAN: &str = "scan";
/// A session's proposal (P4-4).
pub const SOURCE_SESSION: &str = "session";

/// The longest key, in bytes.
pub const MAX_KEY_LEN: usize = 255;
/// The longest segment of a key.
pub const MAX_SEGMENT_LEN: usize = 128;
/// The longest name, in characters.
pub const MAX_NAME_CHARS: usize = 200;
/// The longest environment.
pub const MAX_ENV_LEN: usize = 32;
/// The longest kind a read may name.
pub const MAX_KIND_LEN: usize = 32;
/// The longest attribute name.
pub const MAX_ATTR_KEY_LEN: usize = 64;
/// How deep an attribute or a fact's value may nest.
pub const MAX_DEPTH: usize = 8;
/// The most items in one array of an attribute or a fact's value.
pub const MAX_ARRAY_ITEMS: usize = 100;
/// An entity's attributes, stored, at most.
pub const MAX_ATTRS_BYTES: usize = 16 * 1024;
/// An edge's attributes, stored, at most.
pub const MAX_EDGE_ATTRS_BYTES: usize = 4 * 1024;
/// The most facts an entity holds.
pub const MAX_FACTS: usize = 64;
/// The longest fact path.
pub const MAX_FACT_PATH_LEN: usize = 128;
/// One fact's value, stored, at most.
pub const MAX_FACT_BYTES: usize = 2 * 1024;

/// One thing on the map.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KnowhowEntity {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// One of [`KINDS`] when this build wrote it.
    pub kind: String,
    /// Unique among the live of its kind in the organization ([`check_key`]).
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub env: Option<String>,
    /// The member who answers for it.
    #[serde(default)]
    pub owner_id: Option<ObjectId>,
    #[serde(default)]
    pub attrs: Document,
    /// What is known of it, each fact with its own provenance, by path.
    #[serde(default)]
    pub facts: Vec<Fact>,
    /// [`STATUS_ACTIVE`] for a person's; [`STATUS_PROPOSED`] for a session's
    /// (P4-4-2); [`STATUS_ARCHIVED`] once tombstoned.
    pub status: String,
    /// Where it came from: [`SOURCE_PERSON`] in P4-1.
    pub source: String,
    /// P4-3a-2 — the device whose scan made it, for a scan's entity: the one
    /// device whose later scans keep it, change it and retire it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<ObjectId>,
    /// P4-4-2 — the session whose proposal made it, for a session's entity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<ObjectId>,
    /// Bumped by every write; a write names the rev it read.
    pub rev: i64,
    #[serde(default)]
    pub created_by: Option<ObjectId>,
    pub created_at: DateTime,
    #[serde(default)]
    pub updated_by: Option<ObjectId>,
    pub updated_at: DateTime,
    /// An explicit null while live: the partial unique index matches
    /// `$type: "null"`, which an absent field does not.
    #[serde(default)]
    pub deleted_at: Option<DateTime>,
}

impl KnowhowEntity {
    pub const COLLECTION: &'static str = "knowhow_entities";
}

/// One fact of an entity (design §9.1): a path, its value, and where the
/// value came from. A person's change to one fact never restamps another.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Fact {
    pub path: String,
    pub value: Bson,
    /// [`SOURCE_PERSON`] in P4-1.
    pub source: String,
    /// The person, for a person's fact.
    #[serde(default)]
    pub by: Option<ObjectId>,
    /// P4-3a-2 — the device whose scan said it, for a scan's fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<ObjectId>,
    /// P4-4-2 — the session that proposed it, for a session's fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<ObjectId>,
    pub observed_at: DateTime,
    /// When a probe last confirmed it (P4-5).
    #[serde(default)]
    pub verified_at: Option<DateTime>,
    /// [`STATUS_ACTIVE`] for a person's; a session's is
    /// [`STATUS_PROPOSED`] until a probe or a person confirms it (P4-4).
    pub status: String,
}

/// How two entities are related: `from` `kind` `to`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KnowhowEdge {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub from: ObjectId,
    pub to: ObjectId,
    /// One of [`EDGE_KINDS`] when this build wrote it.
    pub kind: String,
    #[serde(default)]
    pub attrs: Document,
    pub status: String,
    pub source: String,
    /// P4-3a-2 — the device whose scan made it, for a scan's link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<ObjectId>,
    /// P4-4-2 — the session whose proposal made it, for a session's link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<ObjectId>,
    pub rev: i64,
    #[serde(default)]
    pub created_by: Option<ObjectId>,
    pub created_at: DateTime,
    #[serde(default)]
    pub updated_by: Option<ObjectId>,
    pub updated_at: DateTime,
    /// An explicit null while live, as an entity's.
    #[serde(default)]
    pub deleted_at: Option<DateTime>,
}

impl KnowhowEdge {
    pub const COLLECTION: &'static str = "knowhow_edges";
}

/// What one write did, appended after it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KnowhowHistory {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// [`TARGET_ENTITY`] or [`TARGET_EDGE`].
    pub target: String,
    pub target_id: ObjectId,
    /// The entity, or the edge's two ends: what an entity's history is read
    /// by, its edges' writes included.
    pub entity_ids: Vec<ObjectId>,
    /// The rev the write made.
    pub rev: i64,
    /// [`OP_CREATE`], [`OP_UPDATE`] or [`OP_ARCHIVE`].
    pub op: String,
    pub source: String,
    #[serde(default)]
    pub by: Option<ObjectId>,
    /// P4-3a-2 — the device whose scan wrote, for a scan's write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<ObjectId>,
    /// P4-4-2 — the session whose proposal wrote, for a session's write:
    /// what its ceiling counts (decision 51).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<ObjectId>,
    /// P4-4-2 — why the session proposed it, in its own words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub at: DateTime,
    /// What the write set: a create's whole body, an update's changed fields
    /// (facts as `{path, value}` rows, a null value for one removed), an
    /// archive's cause.
    #[serde(default)]
    pub set: Document,
}

impl KnowhowHistory {
    pub const COLLECTION: &'static str = "knowhow_history";
}

pub const TARGET_ENTITY: &str = "entity";
pub const TARGET_EDGE: &str = "edge";
pub const OP_CREATE: &str = "create";
pub const OP_UPDATE: &str = "update";
pub const OP_ARCHIVE: &str = "archive";

/// P4-3a — one scan asked of a device (the FR-90 spec, §3e "P4-3, scans, as
/// designed"): who asked and when, where it is, and how it ended. The row is
/// the bus: the pod holding the device's connection delivers it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KnowhowScan {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub device_id: ObjectId,
    pub requested_by: ObjectId,
    pub requested_at: DateTime,
    /// [`SCAN_REQUESTED`] → [`SCAN_DELIVERED`] → [`SCAN_DONE`],
    /// [`SCAN_NO_ROOTS`] or [`SCAN_FAILED`]; or [`SCAN_EXPIRED`].
    pub state: String,
    /// `true` while requested or delivered, and absent once it ended: a
    /// device's one pending scan, held unique by a partial index on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
    #[serde(default)]
    pub delivered_at: Option<DateTime>,
    #[serde(default)]
    pub answered_at: Option<DateTime>,
    /// What its answer held, by kind.
    #[serde(default)]
    pub counts: ScanCounts,
    /// The device stopped short of all it found.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub detail: Option<String>,
    /// P4-3a-2 — what its answer wrote onto the map; absent until written,
    /// and for an answer that writes nothing (`failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<ScanWritten>,
}

/// P4-3a-2 — what a scan wrote onto the map (decision 42).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanWritten {
    /// The device's checkouts and listeners the scan holds on the map.
    #[serde(default)]
    pub checkouts: u32,
    #[serde(default)]
    pub endpoints: u32,
    /// Shared repositories and clusters it made, bare.
    #[serde(default)]
    pub repos: u32,
    #[serde(default)]
    pub clusters: u32,
    /// Links it made.
    #[serde(default)]
    pub links: u32,
    /// What its device's earlier scans wrote and this one archived.
    #[serde(default)]
    pub retired: u32,
    /// The map was full: what did not fit was not written.
    #[serde(default)]
    pub full: bool,
}

impl KnowhowScan {
    pub const COLLECTION: &'static str = "knowhow_scans";
}

/// A scan's answer, counted by kind.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCounts {
    #[serde(default)]
    pub checkouts: u32,
    #[serde(default)]
    pub contexts: u32,
    #[serde(default)]
    pub listeners: u32,
    #[serde(default)]
    pub tunnel_routes: u32,
    /// Of a kind this build does not know.
    #[serde(default)]
    pub unknown: u32,
    /// Refused: malformed, past a bound, or holding a credential's shape.
    #[serde(default)]
    pub refused: u32,
}

pub const SCAN_REQUESTED: &str = "requested";
pub const SCAN_DELIVERED: &str = "delivered";
pub const SCAN_DONE: &str = "done";
pub const SCAN_NO_ROOTS: &str = "no_roots";
pub const SCAN_FAILED: &str = "failed";
pub const SCAN_EXPIRED: &str = "expired";

// ─── the rules a write is held to ───────────────────────────────────────────

/// A kind a write may name.
pub fn check_kind(raw: &str) -> Result<&'static str, String> {
    KINDS
        .iter()
        .copied()
        .find(|k| *k == raw)
        .ok_or_else(|| format!("kind {raw:?} is not one of: {}", KINDS.join(", ")))
}

/// An edge kind a write may name.
pub fn check_edge_kind(raw: &str) -> Result<&'static str, String> {
    EDGE_KINDS
        .iter()
        .copied()
        .find(|k| *k == raw)
        .ok_or_else(|| format!("edge kind {raw:?} is not one of: {}", EDGE_KINDS.join(", ")))
}

/// A kind as a READ names it — a search's filter, a lookup: one lowercase
/// word, known to this build or not, because a newer server may have written
/// a kind this one has never heard of.
pub fn check_kind_word(raw: &str) -> Result<&str, String> {
    check_word(raw, MAX_KIND_LEN, b"_").map_err(|why| {
        format!("kind {raw:?} {why} (a-z 0-9 _, starting with a letter or a digit)")
    })?;
    Ok(raw)
}

/// An entity's key: one or more segments joined by `/`, each starting with
/// `[a-z0-9]` and going on in `[a-z0-9._:@-]` — `github.com/acme/api`,
/// `prod/payments`, `db-1:5432`. No empty segment, no `.` or `..`, nothing a
/// person would read two ways.
pub fn check_key(raw: &str) -> Result<&str, String> {
    if raw.is_empty() {
        return Err("key must not be empty".into());
    }
    if raw.len() > MAX_KEY_LEN {
        return Err(format!("key is longer than {MAX_KEY_LEN} bytes"));
    }
    for segment in raw.split('/') {
        check_word(segment, MAX_SEGMENT_LEN, b"._:@-").map_err(|why| {
            format!(
                "key segment {segment:?} {why} (lowercase segments of a-z 0-9 . _ : @ -, \
                 starting with a letter or a digit, joined by /)"
            )
        })?;
    }
    Ok(raw)
}

/// An environment: one lowercase word (`prod`, `staging`, `dev-eu`).
pub fn check_env(raw: &str) -> Result<&str, String> {
    check_word(raw, MAX_ENV_LEN, b"_-").map_err(|why| {
        format!("env {raw:?} {why} (a-z 0-9 _ -, starting with a letter or a digit)")
    })?;
    Ok(raw)
}

/// A name, as a person reads it: trimmed, not empty, no control character.
pub fn check_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("name is longer than {MAX_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("name holds a control character".into());
    }
    Ok(name.to_string())
}

/// A fact's path: `[a-z0-9]` then `[a-z0-9._-]` — `port`,
/// `tls.fingerprint`. It is stored as a value beside the fact, never as a
/// key in the document, so a `.` is safe here.
pub fn check_fact_path(raw: &str) -> Result<&str, String> {
    check_word(raw, MAX_FACT_PATH_LEN, b"._-").map_err(|why| {
        format!("fact path {raw:?} {why} (a-z 0-9 . _ -, starting with a letter or a digit)")
    })?;
    Ok(raw)
}

/// Attributes: a JSON object whose names are words of `[A-Za-z0-9_-]` —
/// each is a key in the stored document, so never a `.` path or a `$`
/// operator — at most [`MAX_DEPTH`] deep and `max_bytes` stored.
pub fn check_attrs(value: &Value, max_bytes: usize) -> Result<Document, String> {
    if !value.is_object() {
        return Err("attrs must be a JSON object".into());
    }
    check_value(value, 0)?;
    let doc = bson::to_document(value).map_err(|e| format!("attrs cannot be stored: {e}"))?;
    let len = stored_len(&doc)?;
    if len > max_bytes {
        return Err(format!(
            "attrs are {len} bytes stored, more than {max_bytes}"
        ));
    }
    Ok(doc)
}

/// A fact's value: any JSON but null — null removes a fact, in an update —
/// its objects held to the attribute rules, at most [`MAX_FACT_BYTES`].
pub fn check_fact_value(value: &Value) -> Result<Bson, String> {
    if value.is_null() {
        return Err("a fact's value must not be null (null removes a fact, in an update)".into());
    }
    check_value(value, 1)?;
    let b = bson::to_bson(value).map_err(|e| format!("the value cannot be stored: {e}"))?;
    let len = stored_len(&doc! { "v": b.clone() })?;
    if len > MAX_FACT_BYTES {
        return Err(format!(
            "the value is {len} bytes stored, more than {MAX_FACT_BYTES}"
        ));
    }
    Ok(b)
}

fn stored_len(doc: &Document) -> Result<usize, String> {
    bson::to_vec(doc)
        .map(|v| v.len())
        .map_err(|e| format!("cannot be stored: {e}"))
}

fn check_value(value: &Value, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("nests deeper than {MAX_DEPTH}"));
    }
    match value {
        Value::Object(map) => {
            for (name, child) in map {
                check_attr_name(name)?;
                check_value(child, depth + 1)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            if items.len() > MAX_ARRAY_ITEMS {
                return Err(format!("an array holds more than {MAX_ARRAY_ITEMS} items"));
            }
            items.iter().try_for_each(|i| check_value(i, depth + 1))
        }
        _ => Ok(()),
    }
}

fn check_attr_name(name: &str) -> Result<(), String> {
    let b = name.as_bytes();
    let ok = b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.len() <= MAX_ATTR_KEY_LEN
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "the name {name:?} is not a word of A-Z a-z 0-9 _ - (at most \
             {MAX_ATTR_KEY_LEN}, starting with a letter or a digit)"
        ))
    }
}

fn check_word(word: &str, max: usize, more: &[u8]) -> Result<(), &'static str> {
    let bytes = word.as_bytes();
    let Some(first) = bytes.first() else {
        return Err("is empty");
    };
    if bytes.len() > max {
        return Err("is too long");
    }
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err("must start with a lowercase letter or a digit");
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || more.contains(b))
    {
        return Err("holds a character outside the allowed set");
    }
    Ok(())
}

/// A change to an entity's facts, said by `source` (and `by`, a person):
/// each path set to a value, or (`None`) removed.
///
/// ⚠️ The facts it does not name keep their provenance — who said them, from
/// where, when — and so does one it sets to the value it already holds while
/// that fact is active. Restamping every fact on every edit would make a
/// person the source of what a probe or a device said.
///
/// Returns the facts by path, and what changed as `{path, value}` rows (a
/// null value for one removed): empty when nothing did.
pub fn merge_facts(
    current: &[Fact],
    changes: &[(String, Option<Bson>)],
    source: &str,
    by: Option<ObjectId>,
    now: DateTime,
) -> (Vec<Fact>, Vec<Document>) {
    let who = Provenance {
        source,
        by,
        device: None,
        session: None,
    };
    merge_facts_from(current, changes, who, now)
}

/// Who says what a write sets: its source, and the person or the device
/// behind it (P4-3a-2: a scan's facts name their device; P4-4-2: a session's
/// name their session).
#[derive(Debug, Clone, Copy)]
pub struct Provenance<'a> {
    pub source: &'a str,
    pub by: Option<ObjectId>,
    pub device: Option<ObjectId>,
    pub session: Option<ObjectId>,
}

/// [`merge_facts`], with the whole provenance a fact keeps. A session's fact
/// is [`STATUS_PROPOSED`] (P4-4-2), every other source's [`STATUS_ACTIVE`].
pub fn merge_facts_from(
    current: &[Fact],
    changes: &[(String, Option<Bson>)],
    who: Provenance<'_>,
    now: DateTime,
) -> (Vec<Fact>, Vec<Document>) {
    let mut facts = current.to_vec();
    let mut changed = Vec::new();
    for (path, value) in changes {
        let at = facts.iter().position(|f| &f.path == path);
        match (value, at) {
            (None, Some(i)) => {
                facts.remove(i);
                changed.push(doc! { "path": path.as_str(), "value": Bson::Null });
            }
            (None, None) => {}
            (Some(v), Some(i)) if &facts[i].value == v && facts[i].status == STATUS_ACTIVE => {}
            (Some(v), at) => {
                let status = if who.source == SOURCE_SESSION {
                    STATUS_PROPOSED
                } else {
                    STATUS_ACTIVE
                };
                let fact = Fact {
                    path: path.clone(),
                    value: v.clone(),
                    source: who.source.into(),
                    by: who.by,
                    device: who.device,
                    session: who.session,
                    observed_at: now,
                    verified_at: None,
                    status: status.into(),
                };
                match at {
                    Some(i) => facts[i] = fact,
                    None => facts.push(fact),
                }
                changed.push(doc! { "path": path.as_str(), "value": v.clone() });
            }
        }
    }
    facts.sort_by(|a, b| a.path.cmp(&b.path));
    (facts, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_lowercase_segments() {
        for ok in [
            "db",
            "prod-mongo",
            "github.com/acme/api",
            "prod/payments",
            "db-1:5432",
            "team@acme",
            "9lives",
        ] {
            assert_eq!(check_key(ok), Ok(ok), "{ok}");
        }
        for bad in [
            "",
            "/db",
            "db/",
            "prod//db",
            "Prod/db",
            "prod/DB",
            "./db",
            "../db",
            "prod/../db",
            ".hidden",
            "a b",
            "db\u{0}",
            "https://acme.test",
            "é",
        ] {
            assert!(check_key(bad).is_err(), "{bad:?} was let through");
        }
        assert!(check_key(&"a".repeat(MAX_KEY_LEN + 1)).is_err());
    }

    #[test]
    fn a_write_names_a_known_kind_and_a_read_any_word() {
        for k in KINDS {
            assert_eq!(check_kind(k), Ok(*k));
            assert_eq!(check_kind_word(k), Ok(*k));
        }
        for e in EDGE_KINDS {
            assert_eq!(check_edge_kind(e), Ok(*e));
        }
        assert!(check_kind("server").is_err());
        assert!(check_edge_kind("hosts").is_err());
        // A newer server's kind is read, never refused.
        assert_eq!(check_kind_word("queue"), Ok("queue"));
        assert!(check_kind_word("Queue").is_err() && check_kind_word("").is_err());
        // The lists hold each word once, and the synced kinds are kinds.
        for list in [KINDS, EDGE_KINDS] {
            for (i, a) in list.iter().enumerate() {
                assert!(!list[i + 1..].contains(a), "{a} twice");
            }
        }
        assert!(SYNCED_KINDS.iter().all(|k| KINDS.contains(k)));
    }

    #[test]
    fn names_and_environments() {
        assert_eq!(check_name("  Prod Mongo  "), Ok("Prod Mongo".to_string()));
        assert!(check_name("   ").is_err());
        assert!(check_name("a\u{7}b").is_err());
        assert!(check_name(&"n".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert_eq!(
            check_name(&"ž".repeat(MAX_NAME_CHARS)).map(|n| n.chars().count()),
            Ok(200)
        );
        for ok in ["prod", "staging", "dev-eu", "qa_2"] {
            assert_eq!(check_env(ok), Ok(ok));
        }
        for bad in ["", "Prod", "-x", "a.b", "a/b"] {
            assert!(check_env(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn attribute_names_are_never_a_path_or_an_operator() {
        assert!(
            check_attrs(
                &json!({ "image": "x", "ports": [80, 443] }),
                MAX_ATTRS_BYTES
            )
            .is_ok()
        );
        assert!(
            check_attrs(
                &json!({ "nested": { "dockerImage": "x" } }),
                MAX_ATTRS_BYTES
            )
            .is_ok()
        );
        for bad in [
            json!({ "a.b": 1 }),
            json!({ "$set": 1 }),
            json!({ "ok": { "$where": "1" } }),
            json!({ "ok": [{ "x.y": 1 }] }),
            json!({ "": 1 }),
            json!([1, 2]),
            json!("text"),
        ] {
            assert!(
                check_attrs(&bad, MAX_ATTRS_BYTES).is_err(),
                "{bad} was let through"
            );
        }
        let mut deep = json!(1);
        for _ in 0..=MAX_DEPTH {
            deep = json!({ "d": deep });
        }
        assert!(check_attrs(&deep, MAX_ATTRS_BYTES).is_err(), "too deep");
        let big = json!({ "blob": "x".repeat(MAX_ATTRS_BYTES) });
        assert!(check_attrs(&big, MAX_ATTRS_BYTES).is_err(), "too big");
        let wide = json!({ "many": vec![0; MAX_ARRAY_ITEMS + 1] });
        assert!(check_attrs(&wide, MAX_ATTRS_BYTES).is_err(), "too wide");
    }

    #[test]
    fn a_facts_value_is_bounded_and_never_null() {
        assert_eq!(check_fact_value(&json!(27017)), Ok(Bson::Int64(27017)));
        assert!(check_fact_value(&json!({ "a.b": 1 })).is_err());
        assert!(check_fact_value(&Value::Null).is_err());
        assert!(check_fact_value(&json!("x".repeat(MAX_FACT_BYTES))).is_err());
        for ok in ["port", "tls.fingerprint", "a-b_c"] {
            assert_eq!(check_fact_path(ok), Ok(ok));
        }
        for bad in ["", ".port", "Port", "a/b", "$x"] {
            assert!(check_fact_path(bad).is_err(), "{bad:?}");
        }
    }

    fn fact(path: &str, value: Bson, source: &str, status: &str) -> Fact {
        Fact {
            path: path.into(),
            value,
            source: source.into(),
            by: None,
            device: None,
            session: None,
            observed_at: DateTime::from_millis(1),
            verified_at: Some(DateTime::from_millis(2)),
            status: status.into(),
        }
    }

    /// P4-4-2 — a session's fact is a proposal, naming its session, and a
    /// person's is believed: the same merge, told apart by its source.
    #[test]
    fn a_sessions_fact_is_proposed_and_names_its_session() {
        let session = ObjectId::new();
        let now = DateTime::from_millis(7);
        let who = Provenance {
            source: SOURCE_SESSION,
            by: None,
            device: None,
            session: Some(session),
        };
        let (facts, changed) =
            merge_facts_from(&[], &[("port".into(), Some(Bson::Int64(27017)))], who, now);
        assert_eq!(changed, vec![doc! { "path": "port", "value": 27017_i64 }]);
        let f = &facts[0];
        assert_eq!(
            (f.source.as_str(), f.status.as_str(), f.session, f.by),
            (SOURCE_SESSION, STATUS_PROPOSED, Some(session), None)
        );
        let (facts, _) = merge_facts(
            &[],
            &[("port".into(), Some(Bson::Int64(27017)))],
            SOURCE_PERSON,
            Some(ObjectId::new()),
            now,
        );
        assert_eq!(
            (facts[0].status.as_str(), facts[0].session),
            (STATUS_ACTIVE, None)
        );
    }

    #[test]
    fn a_persons_change_to_one_fact_keeps_every_others_provenance() {
        let scanned = fact("port", Bson::Int64(5432), SOURCE_SCAN, STATUS_ACTIVE);
        let synced = fact(
            "os",
            Bson::String("linux".into()),
            SOURCE_SYNC,
            STATUS_ACTIVE,
        );
        let current = vec![synced.clone(), scanned.clone()];
        let me = ObjectId::new();
        let now = DateTime::from_millis(99);
        let (facts, changed) = merge_facts(
            &current,
            &[
                ("version".into(), Some(Bson::String("16".into()))),
                // The value it already holds, while active: left as it was.
                ("port".into(), Some(Bson::Int64(5432))),
                ("gone".into(), None),
            ],
            SOURCE_PERSON,
            Some(me),
            now,
        );
        assert_eq!(changed, vec![doc! { "path": "version", "value": "16" }]);
        let by_path = |p: &str| facts.iter().find(|f| f.path == p).unwrap().clone();
        assert_eq!(
            by_path("port"),
            scanned,
            "an untouched fact keeps its source"
        );
        assert_eq!(by_path("os"), synced);
        let v = by_path("version");
        assert_eq!(
            (v.source.as_str(), v.by, v.observed_at, v.verified_at),
            (SOURCE_PERSON, Some(me), now, None)
        );
        assert_eq!(
            facts.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["os", "port", "version"],
            "by path"
        );

        // A new value, or a removal, is the person's.
        let (facts, changed) = merge_facts(
            &facts,
            &[
                ("port".into(), Some(Bson::Int64(6432))),
                ("os".into(), None),
            ],
            SOURCE_PERSON,
            Some(me),
            now,
        );
        assert_eq!(
            changed,
            vec![
                doc! { "path": "port", "value": 6432_i64 },
                doc! { "path": "os", "value": Bson::Null },
            ]
        );
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].path, "port");
        assert_eq!(facts[0].source, SOURCE_PERSON);

        // ⚠️ The same value said by a person turns a session's proposal into
        // the person's fact: a person's word confirms it (design §9.2).
        let proposed = vec![fact(
            "port",
            Bson::Int64(1),
            SOURCE_SESSION,
            STATUS_PROPOSED,
        )];
        let (facts, changed) = merge_facts(
            &proposed,
            &[("port".into(), Some(Bson::Int64(1)))],
            SOURCE_PERSON,
            Some(me),
            now,
        );
        assert_eq!(changed.len(), 1);
        assert_eq!(
            (facts[0].source.as_str(), facts[0].status.as_str()),
            (SOURCE_PERSON, STATUS_ACTIVE)
        );

        // P4-2a — the sync says its facts with no person behind them.
        let (facts, _) = merge_facts(
            &facts,
            &[("os".into(), Some(Bson::String("linux".into())))],
            SOURCE_SYNC,
            None,
            now,
        );
        let os = facts.iter().find(|f| f.path == "os").unwrap();
        assert_eq!((os.source.as_str(), os.by), (SOURCE_SYNC, None));
    }
}
