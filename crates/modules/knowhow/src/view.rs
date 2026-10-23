// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! What the routes answer: an entity and its card, a listing's node, an
//! edge, a history row. Ids as hex, times as RFC 3339, attributes and fact
//! values as the JSON they were written as.

use bson::{Bson, DateTime, Document, oid::ObjectId};
use serde::Serialize;
use serde_json::Value;

use crate::dao::EntityNode;
use crate::model::{Fact, KnowhowEdge, KnowhowEntity, KnowhowHistory, KnowhowScan, ScanCounts};

pub(crate) fn rfc3339(t: DateTime) -> String {
    t.try_to_rfc3339_string().unwrap_or_default()
}

fn hex(id: Option<ObjectId>) -> String {
    id.map(|i| i.to_hex()).unwrap_or_default()
}

fn json(doc: &Document) -> Value {
    Bson::Document(doc.clone()).into_relaxed_extjson()
}

/// One entity, whole.
#[derive(Serialize, Debug)]
pub struct EntityView {
    pub id: String,
    pub kind: String,
    pub key: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    pub attrs: Value,
    pub facts: Vec<FactView>,
    pub status: String,
    pub source: String,
    pub rev: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    pub updated_at: String,
}

impl EntityView {
    pub fn of(e: &KnowhowEntity) -> Self {
        Self {
            id: hex(e.id),
            kind: e.kind.clone(),
            key: e.key.clone(),
            name: e.name.clone(),
            env: e.env.clone(),
            owner_id: e.owner_id.map(|o| o.to_hex()),
            attrs: json(&e.attrs),
            facts: e.facts.iter().map(FactView::of).collect(),
            status: e.status.clone(),
            source: e.source.clone(),
            rev: e.rev,
            created_by: e.created_by.map(|u| u.to_hex()),
            created_at: rfc3339(e.created_at),
            updated_by: e.updated_by.map(|u| u.to_hex()),
            updated_at: rfc3339(e.updated_at),
        }
    }
}

/// One fact, with where it came from.
#[derive(Serialize, Debug)]
pub struct FactView {
    pub path: String,
    pub value: Value,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    pub observed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    pub status: String,
}

impl FactView {
    pub fn of(f: &Fact) -> Self {
        Self {
            path: f.path.clone(),
            value: f.value.clone().into_relaxed_extjson(),
            source: f.source.clone(),
            by: f.by.map(|u| u.to_hex()),
            observed_at: rfc3339(f.observed_at),
            verified_at: f.verified_at.map(rfc3339),
            status: f.status.clone(),
        }
    }
}

/// An entity as a listing shows it: what it is and where, no attributes and
/// no facts.
#[derive(Serialize, Debug, Clone)]
pub struct NodeView {
    pub id: String,
    pub kind: String,
    pub key: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    pub status: String,
    pub rev: i64,
    pub updated_at: String,
}

impl From<&EntityNode> for NodeView {
    fn from(n: &EntityNode) -> Self {
        Self {
            id: n.id.to_hex(),
            kind: n.kind.clone(),
            key: n.key.clone(),
            name: n.name.clone(),
            env: n.env.clone(),
            status: n.status.clone(),
            rev: n.rev,
            updated_at: rfc3339(n.updated_at),
        }
    }
}

impl From<&KnowhowEntity> for NodeView {
    fn from(e: &KnowhowEntity) -> Self {
        Self {
            id: hex(e.id),
            kind: e.kind.clone(),
            key: e.key.clone(),
            name: e.name.clone(),
            env: e.env.clone(),
            status: e.status.clone(),
            rev: e.rev,
            updated_at: rfc3339(e.updated_at),
        }
    }
}

/// One edge.
#[derive(Serialize, Debug)]
pub struct EdgeView {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub attrs: Value,
    pub status: String,
    pub source: String,
    pub rev: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    pub updated_at: String,
}

impl EdgeView {
    pub fn of(e: &KnowhowEdge) -> Self {
        Self {
            id: hex(e.id),
            from: e.from.to_hex(),
            to: e.to.to_hex(),
            kind: e.kind.clone(),
            attrs: json(&e.attrs),
            status: e.status.clone(),
            source: e.source.clone(),
            rev: e.rev,
            created_by: e.created_by.map(|u| u.to_hex()),
            created_at: rfc3339(e.created_at),
            updated_by: e.updated_by.map(|u| u.to_hex()),
            updated_at: rfc3339(e.updated_at),
        }
    }
}

/// An entity's link, as its card shows it: `out` when the entity is the
/// edge's `from`, `in` when it is its `to`, and the entity at the other end.
#[derive(Serialize, Debug)]
pub struct LinkView {
    pub direction: &'static str,
    pub edge: EdgeView,
    pub other: NodeView,
}

/// An entity's card: itself, and its links to live entities.
#[derive(Serialize, Debug)]
pub struct CardView {
    pub entity: EntityView,
    pub links: Vec<LinkView>,
    /// More links than one card shows.
    pub truncated: bool,
}

/// One history row.
#[derive(Serialize, Debug)]
pub struct HistoryView {
    pub id: String,
    pub target: String,
    pub target_id: String,
    pub entity_ids: Vec<String>,
    pub rev: i64,
    pub op: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    pub at: String,
    pub set: Value,
}

impl HistoryView {
    pub fn of(h: &KnowhowHistory) -> Self {
        Self {
            id: hex(h.id),
            target: h.target.clone(),
            target_id: h.target_id.to_hex(),
            entity_ids: h.entity_ids.iter().map(|i| i.to_hex()).collect(),
            rev: h.rev,
            op: h.op.clone(),
            source: h.source.clone(),
            by: h.by.map(|u| u.to_hex()),
            at: rfc3339(h.at),
            set: json(&h.set),
        }
    }
}

/// P4-3a — one scan asked of a device: where it is, and what its answer held.
#[derive(Serialize, Debug)]
pub struct ScanView {
    pub id: String,
    pub device_id: String,
    pub requested_by: String,
    pub requested_at: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
    pub counts: ScanCounts,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ScanView {
    pub fn of(s: &KnowhowScan) -> Self {
        Self {
            id: hex(s.id),
            device_id: s.device_id.to_hex(),
            requested_by: s.requested_by.to_hex(),
            requested_at: rfc3339(s.requested_at),
            state: s.state.clone(),
            delivered_at: s.delivered_at.map(rfc3339),
            answered_at: s.answered_at.map(rfc3339),
            counts: s.counts,
            truncated: s.truncated,
            detail: s.detail.clone(),
        }
    }
}
