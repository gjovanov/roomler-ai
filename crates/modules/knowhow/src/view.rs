// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! What the routes answer: an entity and its card, a listing's node, an
//! edge, a history row. Ids as hex, times as RFC 3339, attributes and fact
//! values as the JSON they were written as.

use bson::{Bson, DateTime, Document, oid::ObjectId};
use serde::Serialize;
use serde_json::Value;

use crate::dao::EntityNode;
use crate::model::{
    Fact, KnowhowEdge, KnowhowEntity, KnowhowHistory, KnowhowProbe, KnowhowScan, Measured,
    ProbeTlsSeen, ScanCounts, ScanWritten, Verified,
};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// P4-4-2 — the session whose proposal made it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// P4-4-3 — the person who confirmed the proposal, and when.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<String>,
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
            device: e.device.map(|d| d.to_hex()),
            session: e.session.map(|s| s.to_hex()),
            confirmed_by: e.confirmed.map(|c| c.by.to_hex()),
            confirmed_at: e.confirmed.map(|c| rfc3339(c.at)),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// P4-4-2 — the session that proposed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// P4-4-3 — the person who confirmed it, and when.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<String>,
    pub observed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    /// P4-5 — what the last probe that measured it found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<VerifiedView>,
    pub status: String,
}

impl FactView {
    pub fn of(f: &Fact) -> Self {
        Self {
            path: f.path.clone(),
            value: f.value.clone().into_relaxed_extjson(),
            source: f.source.clone(),
            by: f.by.map(|u| u.to_hex()),
            device: f.device.map(|d| d.to_hex()),
            session: f.session.map(|s| s.to_hex()),
            confirmed_by: f.confirmed.map(|c| c.by.to_hex()),
            confirmed_at: f.confirmed.map(|c| rfc3339(c.at)),
            observed_at: rfc3339(f.observed_at),
            verified_at: f.verified_at.map(rfc3339),
            verified: f.verified.as_ref().map(VerifiedView::of),
            status: f.status.clone(),
        }
    }
}

/// P4-5 — what one probe measured of a fact.
#[derive(Serialize, Debug)]
pub struct VerifiedView {
    pub at: String,
    pub probe: String,
    pub from_device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    pub ok: bool,
}

impl VerifiedView {
    pub fn of(v: &Verified) -> Self {
        Self {
            at: rfc3339(v.at),
            probe: v.probe.to_hex(),
            from_device: v.from_device.to_hex(),
            rtt_ms: v.rtt_ms,
            carrier: v.carrier.clone(),
            ok: v.ok,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// P4-4-2 — the session whose proposal made it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// P4-4-3 — the person who confirmed the proposal, and when.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<String>,
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
            device: e.device.map(|d| d.to_hex()),
            session: e.session.map(|s| s.to_hex()),
            confirmed_by: e.confirmed.map(|c| c.by.to_hex()),
            confirmed_at: e.confirmed.map(|c| rfc3339(c.at)),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// P4-4-2 — the session whose proposal wrote, and why it proposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
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
            device: h.device.map(|d| d.to_hex()),
            session: h.session.map(|s| s.to_hex()),
            why: h.why.clone(),
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
    /// P4-3a-2 — what its answer wrote onto the map, once written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written: Option<ScanWritten>,
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
            written: s.written,
        }
    }
}

/// P4-5 — one probe asked of a device: its target as the server read it,
/// where it is, and what it measured. "The device may not reach it"
/// (`not_allowed`) and "it did not answer" (`unreachable`) are two states.
#[derive(Serialize, Debug)]
pub struct ProbeView {
    pub id: String,
    pub entity_id: String,
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub requested_at: String,
    pub kind: String,
    pub target: TargetView,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<ProbeTlsSeen>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measured: Vec<Measured>,
}

/// A probe's target: the host, the fact it was read from, the port, and a
/// `tls` probe's server name.
#[derive(Serialize, Debug)]
pub struct TargetView {
    pub host: String,
    pub from: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sni: Option<String>,
}

impl ProbeView {
    pub fn of(p: &KnowhowProbe) -> Self {
        Self {
            id: hex(p.id),
            entity_id: p.entity_id.to_hex(),
            device_id: p.device_id.to_hex(),
            requested_by: p.requested_by.map(|u| u.to_hex()),
            session: p.session.map(|s| s.to_hex()),
            requested_at: rfc3339(p.requested_at),
            kind: p.kind.clone(),
            target: TargetView {
                host: p.target.host.clone(),
                from: p.target.host_path.clone(),
                port: p.target.port,
                sni: p.target.sni.clone(),
            },
            state: p.state.clone(),
            delivered_at: p.delivered_at.map(rfc3339),
            answered_at: p.answered_at.map(rfc3339),
            rtt_ms: p.rtt_ms,
            address: p.address.clone(),
            carrier: p.carrier.clone(),
            tls: p.tls.clone(),
            detail: p.detail.clone(),
            measured: p.measured.clone(),
        }
    }
}
