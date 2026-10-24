// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! P4-4-2 — a session's proposals, the server's half (the FR-90 spec, §3e
//! "P4-4, a session's reads and proposals, as designed"; decisions 39, 50 and
//! 51): `rc:knowhow.propose` from a session's toolbelt, carried by its device
//! and believed as its questions are ([`crate::session::identify`]).
//!
//! A proposal adds; it never changes what is there. One new entity, one fact
//! at a path no fact holds, or one link between two live entities, written
//! [`STATUS_PROPOSED`] with source [`SOURCE_SESSION`]: the session named on
//! the row, on each fact and on its history row, and its `why` on the history
//! row. A person confirms it, or a probe does (P4-4-3, P4-5).
//!
//! | What is checked | Refused |
//! |---|---|
//! | one of `entity`, `fact` or `link`, held as a person's write is, and a `why` | `bad_request` |
//! | no text of it in a credential's shape | `refused` |
//! | 50 a session, over its whole life (decision 51) | `ceiling` |
//! | never a device's part: a machine, a scan's key, a path the fleet or a scan keeps, a link a scan makes | `reserved` |
//! | nothing there already by what it names | `exists` |
//! | a fact's entity, a link's two ends, live | `not_found` |
//! | the map's room: its entities, its links, an entity's facts | `full` |
//!
//! ⚠️ Still a map, not a gate: a proposal changes nothing a session or a
//! person relies on, and nothing that decides a reach reads it.

use bson::{Document, doc, oid::ObjectId};
use roomler_ai_remote_control::knowhow::{SessionRefusal, holds_credential, session_limits};
use roomler_ai_services::dao::base::DaoError;
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::warn;

use crate::KnowhowState;
use crate::dao::{MAX_EDGES, MAX_ENTITIES};
use crate::intake::{devices_key, scans_link};
use crate::model::{
    KnowhowEdge, KnowhowEntity, MAX_ATTRS_BYTES, MAX_EDGE_ATTRS_BYTES, MAX_FACTS, OP_CREATE,
    OP_UPDATE, SOURCE_SESSION, STATUS_PROPOSED, SYNCED_KINDS, TARGET_EDGE, TARGET_ENTITY,
    check_attrs, check_edge_kind, check_env, check_fact_path, check_fact_value, check_key,
    check_kind, check_kind_word, check_name, merge_facts_from,
};
use crate::routes::{persons_facts, persons_room, reserved, scans_path};
use crate::write::{Stamp, record};

/// The writes a fact's proposal tries, the entity read again each time,
/// before it answers `busy`: its entity changed under it each time.
const ATTEMPTS: usize = 3;

/// What a session proposes, as its frame carries it: one key.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Proposal {
    Entity(EntityProposal),
    Fact(FactProposal),
    Link(LinkProposal),
}

/// A new entity, its facts with it.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct EntityProposal {
    kind: String,
    key: String,
    name: String,
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    attrs: Option<Value>,
    #[serde(default)]
    facts: Option<Map<String, Value>>,
}

/// A fact at a path no fact of the entity holds.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct FactProposal {
    kind: String,
    key: String,
    path: String,
    value: Value,
}

/// An entity, by the name a chip and a session use for it.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Named {
    kind: String,
    key: String,
}

/// A link between two live entities: `from` `kind` `to`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct LinkProposal {
    from: Named,
    kind: String,
    to: Named,
    #[serde(default)]
    attrs: Option<Value>,
}

/// What a proposal wrote: what it was, the entity or link written (a fact's
/// entity), and the proposals the session has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Proposed {
    pub what: &'static str,
    pub id: ObjectId,
    pub left: u32,
}

/// One proposal of `session`, which [`crate::session::identify`] believed:
/// checked, then written, or refused with nothing written.
pub(crate) async fn propose(
    state: &KnowhowState,
    tenant_id: ObjectId,
    session: ObjectId,
    raw: &Value,
    why: &str,
) -> Result<Proposed, SessionRefusal> {
    let why = check_why(why)?;
    let proposal = read(raw)?;
    if holds_credential(&why) || credential_in(raw) {
        return Err(SessionRefusal::Refused);
    }
    let made = state
        .history
        .count_by_session(tenant_id, session)
        .await
        .map_err(|e| unavailable(tenant_id, "its proposals were not counted", e))?;
    let most = u64::from(session_limits::MAX_PROPOSALS);
    if made >= most {
        return Err(SessionRefusal::Ceiling);
    }
    let left = u32::try_from(most - made - 1).unwrap_or(0);
    let stamp = Stamp::session(tenant_id, session, why);
    let (what, id) = match proposal {
        Proposal::Entity(p) => ("entity", entity(state, &stamp, p).await?),
        Proposal::Fact(p) => ("fact", fact(state, &stamp, p).await?),
        Proposal::Link(p) => ("link", link(state, &stamp, p).await?),
    };
    Ok(Proposed { what, id, left })
}

/// A proposal's `why`: trimmed, not empty, at most
/// [`session_limits::MAX_WHY_CHARS`], no control character but a line's end
/// or a tab.
fn check_why(raw: &str) -> Result<String, SessionRefusal> {
    let why = raw.trim();
    let bad = why.is_empty()
        || why.chars().count() > session_limits::MAX_WHY_CHARS
        || why
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t');
    if bad {
        return Err(SessionRefusal::BadRequest);
    }
    Ok(why.to_string())
}

/// The proposal a frame carries: an object of one key, read whole.
fn read(raw: &Value) -> Result<Proposal, SessionRefusal> {
    if raw.as_object().map(Map::len) != Some(1) {
        return Err(SessionRefusal::BadRequest);
    }
    serde_json::from_value(raw.clone()).map_err(|_| SessionRefusal::BadRequest)
}

/// Whether any text of `v`, a name or a value, holds a credential's shape.
fn credential_in(v: &Value) -> bool {
    match v {
        Value::String(s) => holds_credential(s),
        Value::Array(items) => items.iter().any(credential_in),
        Value::Object(map) => map
            .iter()
            .any(|(name, child)| holds_credential(name) || credential_in(child)),
        _ => false,
    }
}

fn unavailable(tenant_id: ObjectId, what: &str, e: DaoError) -> SessionRefusal {
    warn!(tenant = %tenant_id, %e, "knowhow: a session's proposal stopped: {what}");
    SessionRefusal::Unavailable
}

fn bad<T, E>(r: Result<T, E>) -> Result<T, SessionRefusal> {
    r.map_err(|_| SessionRefusal::BadRequest)
}

/// A live entity by its kind and key, or `not_found`.
async fn named(
    state: &KnowhowState,
    tid: ObjectId,
    kind: &str,
    key: &str,
) -> Result<KnowhowEntity, SessionRefusal> {
    match state.entities.find_live_by_key(tid, kind, key).await {
        Ok(Some(e)) if e.id.is_some() => Ok(e),
        Ok(_) => Err(SessionRefusal::NotFound),
        Err(e) => Err(unavailable(tid, "an entity it names was not read", e)),
    }
}

/// A new entity, never a machine nor at a key a device's scans keep.
async fn entity(
    state: &KnowhowState,
    stamp: &Stamp,
    p: EntityProposal,
) -> Result<ObjectId, SessionRefusal> {
    let tid = stamp.tid;
    let kind = bad(check_kind(&p.kind))?;
    if SYNCED_KINDS.contains(&kind) {
        return Err(SessionRefusal::Reserved);
    }
    let key = bad(check_key(&p.key))?;
    if devices_key(key) {
        return Err(SessionRefusal::Reserved);
    }
    let name = bad(check_name(&p.name))?;
    let env = match p.env.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => Some(bad(check_env(e))?.to_string()),
        None => None,
    };
    let attrs = match &p.attrs {
        Some(v) => bad(check_attrs(v, MAX_ATTRS_BYTES))?,
        None => Document::new(),
    };
    let changes = facts(p.facts.as_ref())?;
    let (facts, said) = merge_facts_from(&[], &changes, stamp.provenance(), stamp.at);
    match state.entities.find_live_by_key(tid, kind, key).await {
        Ok(Some(_)) => return Err(SessionRefusal::Exists),
        Ok(None) => {}
        Err(e) => return Err(unavailable(tid, "its key was not looked up", e)),
    }
    let live = state
        .entities
        .count_live(tid)
        .await
        .map_err(|e| unavailable(tid, "the map's entities were not counted", e))?;
    if live >= MAX_ENTITIES {
        return Err(SessionRefusal::Full);
    }
    let entity = KnowhowEntity {
        id: None,
        tenant_id: tid,
        kind: kind.into(),
        key: key.into(),
        name,
        env,
        owner_id: None,
        attrs,
        facts,
        status: STATUS_PROPOSED.into(),
        source: SOURCE_SESSION.into(),
        device: None,
        session: stamp.session,
        confirmed: None,
        rev: 1,
        created_by: None,
        created_at: stamp.at,
        updated_by: None,
        updated_at: stamp.at,
        deleted_at: None,
    };
    let id = match state.entities.create(&entity).await {
        Ok(id) => id,
        Err(DaoError::DuplicateKey(_)) => return Err(SessionRefusal::Exists),
        Err(e) => return Err(unavailable(tid, "the entity was not written", e)),
    };
    let set = doc! {
        "kind": entity.kind.as_str(), "key": entity.key.as_str(),
        "name": entity.name.as_str(), "env": entity.env.clone(),
        "attrs": entity.attrs.clone(), "facts": said,
    };
    let row = stamp.row(TARGET_ENTITY, id, vec![id], 1, OP_CREATE, set);
    record(state, vec![row]).await;
    Ok(id)
}

/// An entity proposal's facts: path → value, each held as a person's is.
fn facts(
    raw: Option<&Map<String, Value>>,
) -> Result<Vec<(String, Option<bson::Bson>)>, SessionRefusal> {
    let Some(map) = raw else {
        return Ok(Vec::new());
    };
    if map.len() > MAX_FACTS {
        return Err(SessionRefusal::BadRequest);
    }
    map.iter()
        .map(|(path, value)| {
            let path = bad(check_fact_path(path))?.to_string();
            Ok((path, Some(bad(check_fact_value(value))?)))
        })
        .collect()
}

/// A fact at a path no fact of its entity holds, and neither the fleet nor a
/// scan keeps there.
async fn fact(
    state: &KnowhowState,
    stamp: &Stamp,
    p: FactProposal,
) -> Result<ObjectId, SessionRefusal> {
    let tid = stamp.tid;
    let kind = bad(check_kind_word(&p.kind))?;
    let key = bad(check_key(&p.key))?;
    let path = bad(check_fact_path(&p.path))?.to_string();
    let value = bad(check_fact_value(&p.value))?;
    for _ in 0..ATTEMPTS {
        let current = named(state, tid, kind, key).await?;
        let Some(id) = current.id else {
            return Err(SessionRefusal::NotFound);
        };
        if reserved(&current, &path) || scans_path(&current, &path) {
            return Err(SessionRefusal::Reserved);
        }
        if current.facts.iter().any(|f| f.path == path) {
            return Err(SessionRefusal::Exists);
        }
        if persons_facts(&current, &current.facts) >= persons_room(&current) {
            return Err(SessionRefusal::Full);
        }
        let change = [(path.clone(), Some(value.clone()))];
        let (facts, said) = merge_facts_from(&current.facts, &change, stamp.provenance(), stamp.at);
        let facts = bson::to_bson(&facts).map_err(|_| SessionRefusal::Unavailable)?;
        let mut set = doc! { "facts": facts };
        set.extend(stamp.touched());
        match state.entities.update_at(tid, id, current.rev, set).await {
            Ok(Some(after)) => {
                let row = stamp.row(
                    TARGET_ENTITY,
                    id,
                    vec![id],
                    after.rev,
                    OP_UPDATE,
                    doc! { "facts": said },
                );
                record(state, vec![row]).await;
                return Ok(id);
            }
            // Changed since it was read: read it again.
            Ok(None) => continue,
            Err(e) => return Err(unavailable(tid, "the fact was not written", e)),
        }
    }
    Err(SessionRefusal::Busy)
}

/// A link between two live entities, never one a device's scans make.
async fn link(
    state: &KnowhowState,
    stamp: &Stamp,
    p: LinkProposal,
) -> Result<ObjectId, SessionRefusal> {
    let tid = stamp.tid;
    let kind = bad(check_edge_kind(&p.kind))?;
    for end in [&p.from, &p.to] {
        bad(check_kind_word(&end.kind))?;
        bad(check_key(&end.key))?;
    }
    let attrs = match &p.attrs {
        Some(v) => bad(check_attrs(v, MAX_EDGE_ATTRS_BYTES))?,
        None => Document::new(),
    };
    let from = named(state, tid, &p.from.kind, &p.from.key).await?;
    let to = named(state, tid, &p.to.kind, &p.to.key).await?;
    let (Some(from_id), Some(to_id)) = (from.id, to.id) else {
        return Err(SessionRefusal::NotFound);
    };
    if from_id == to_id {
        return Err(SessionRefusal::BadRequest);
    }
    if scans_link(&from, kind, &to) {
        return Err(SessionRefusal::Reserved);
    }
    match state.edges.find_live(tid, from_id, kind, to_id).await {
        Ok(Some(_)) => return Err(SessionRefusal::Exists),
        Ok(None) => {}
        Err(e) => return Err(unavailable(tid, "its link was not looked up", e)),
    }
    let live = state
        .edges
        .count_live(tid)
        .await
        .map_err(|e| unavailable(tid, "the map's links were not counted", e))?;
    if live >= MAX_EDGES {
        return Err(SessionRefusal::Full);
    }
    let edge = KnowhowEdge {
        id: None,
        tenant_id: tid,
        from: from_id,
        to: to_id,
        kind: kind.into(),
        attrs,
        status: STATUS_PROPOSED.into(),
        source: SOURCE_SESSION.into(),
        device: None,
        session: stamp.session,
        confirmed: None,
        rev: 1,
        created_by: None,
        created_at: stamp.at,
        updated_by: None,
        updated_at: stamp.at,
        deleted_at: None,
    };
    let id = match state.edges.create(&edge).await {
        Ok(id) => id,
        Err(DaoError::DuplicateKey(_)) => return Err(SessionRefusal::Exists),
        Err(e) => return Err(unavailable(tid, "the link was not written", e)),
    };
    let set = doc! {
        "from": from_id, "to": to_id, "kind": kind, "attrs": edge.attrs.clone(),
    };
    let row = stamp.row(TARGET_EDGE, id, vec![from_id, to_id], 1, OP_CREATE, set);
    record(state, vec![row]).await;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A token's shape, built here and never written down whole: a scanner
    /// reading this repository must not take a test for a leak.
    fn token() -> String {
        format!("ghp_{}", "a1B2".repeat(9))
    }

    #[test]
    fn a_proposal_is_one_of_three_and_read_whole() {
        for ok in [
            json!({"entity": {"kind": "service", "key": "payments", "name": "Payments"}}),
            json!({"fact": {"kind": "database", "key": "prod-mongo", "path": "port",
                            "value": 27017}}),
            json!({"link": {"from": {"kind": "service", "key": "payments"}, "kind": "reads",
                            "to": {"kind": "database", "key": "prod-mongo"}}}),
        ] {
            assert!(read(&ok).is_ok(), "{ok}");
        }
        for bad in [
            Value::Null,
            json!({}),
            json!([]),
            json!({"entity": {"kind": "service", "key": "payments", "name": "Payments"},
                   "link": {}}),
            json!({"teleport": {}}),
            json!({"fact": {"kind": "database", "key": "prod-mongo", "path": "port"}}),
            json!({"entity": {"kind": "service", "key": "payments", "name": "Payments",
                              "status": "active"}}),
            json!({"link": {"from": {"kind": "service", "key": "payments", "id": "x"},
                            "kind": "reads", "to": {"kind": "database", "key": "db"}}}),
        ] {
            assert_eq!(read(&bad).err(), Some(SessionRefusal::BadRequest), "{bad}");
        }
    }

    #[test]
    fn a_why_is_said_in_words_within_its_bound() {
        assert_eq!(
            check_why("  the compose file names it\nand its port  "),
            Ok("the compose file names it\nand its port".into())
        );
        for bad in [
            String::new(),
            "   ".into(),
            "a".repeat(session_limits::MAX_WHY_CHARS + 1),
            "bell\u{7}".into(),
        ] {
            assert_eq!(check_why(&bad), Err(SessionRefusal::BadRequest), "{bad:?}");
        }
        assert!(check_why(&"é".repeat(session_limits::MAX_WHY_CHARS)).is_ok());
    }

    #[test]
    fn a_credential_anywhere_in_a_proposal_is_seen() {
        let t = token();
        let url = format!("https://dev:{}@api.example.test/", "s3cret");
        let mut named_by_it = Map::new();
        named_by_it.insert(t.clone(), json!(1));
        for held in [
            json!({"entity": {"kind": "service", "key": "api", "name": t}}),
            json!({"entity": {"kind": "service", "key": "api", "name": "API",
                              "attrs": {"env": {"TOKEN": [1, t]}}}}),
            json!({"fact": {"kind": "service", "key": "api", "path": "url", "value": url}}),
            json!({"entity": {"kind": "service", "key": "api", "name": "API",
                              "facts": named_by_it}}),
        ] {
            assert!(credential_in(&held), "{held}");
        }
        assert!(!credential_in(&json!({"link": {
            "from": {"kind": "service", "key": "api"}, "kind": "reads",
            "to": {"kind": "database", "key": "prod-mongo"}}})));
    }
}
