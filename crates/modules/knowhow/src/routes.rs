// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The people's routes (P4-1): the map, read and changed.
//!
//! | Route | Does | Needs |
//! |---|---|---|
//! | `GET …/knowhow` | how much the map holds | `KNOWHOW_VIEW` |
//! | `GET …/knowhow/search` | live entities by text, kind and environment | `KNOWHOW_VIEW` |
//! | `GET …/knowhow/entity?kind=&key=` | one entity's card, by its name | `KNOWHOW_VIEW` |
//! | `GET …/knowhow/entity/{id}` | one entity's card: itself and its links | `KNOWHOW_VIEW` |
//! | `GET …/knowhow/map/{id}` | its neighbourhood, one or two steps out | `KNOWHOW_VIEW` |
//! | `GET …/knowhow/history` | the writes, newest first; one entity's with `?entity=` | `KNOWHOW_VIEW` |
//! | `POST …/knowhow/entity` | put one on the map | `KNOWHOW_EDIT` |
//! | `PATCH …/knowhow/entity/{id}` | change it, at the rev it was read | `KNOWHOW_EDIT` |
//! | `DELETE …/knowhow/entity/{id}?rev=` | archive it, and its links with it | `KNOWHOW_EDIT` |
//! | `POST …/knowhow/edge` | link two | `KNOWHOW_EDIT` |
//! | `PATCH …/knowhow/edge/{id}` | change a link's attributes, at its rev | `KNOWHOW_EDIT` |
//! | `DELETE …/knowhow/edge/{id}?rev=` | archive a link | `KNOWHOW_EDIT` |
//!
//! `KNOWHOW_EDIT` includes reading: who may change the map may read it.
//! Membership comes first, as everywhere: someone outside an organization
//! learns nothing, not even whether its map is kept. Every id resolves
//! within the organization, so another's entity answers as a bogus id does.

use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use bson::{Bson, Document, doc, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_services::dao::base::DaoError;
use roomler_core::{
    ApiError,
    extractors::auth::AuthUser,
    guards::{parse_tid, require_permission},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::KnowhowState;
use crate::dao::{MAX_EDGES, MAX_ENTITIES, Search};
use crate::model::{
    Fact, KnowhowEdge, KnowhowEntity, MAX_ATTRS_BYTES, MAX_EDGE_ATTRS_BYTES, MAX_FACTS, OP_ARCHIVE,
    OP_CREATE, OP_UPDATE, SOURCE_PERSON, SOURCE_SYNC, STATUS_ACTIVE, STATUS_ARCHIVED, SYNCED_KINDS,
    TARGET_EDGE, TARGET_ENTITY, check_attrs, check_edge_kind, check_env, check_fact_path,
    check_fact_value, check_key, check_kind, check_kind_word, check_name, merge_facts,
};
use crate::sync::SYNCED_FACTS;
use crate::view::{CardView, EdgeView, EntityView, HistoryView, LinkView, NodeView};
use crate::write::{Stamp, archive_entity as archive_at, record};

/// What a member of an organization knowhow does not keep a map for is
/// told, on every route.
pub const NOT_SERVED: &str = "knowhow is not available to this organization";

/// A listing's size when none is asked for, and the most one answers.
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;
/// The most links one card shows.
pub const MAX_LINKS: usize = 200;
/// The most edges one neighbourhood holds.
pub const MAX_MAP_EDGES: usize = 500;
/// The longest search text, in characters.
pub const MAX_QUERY_CHARS: usize = 200;

// ─── the gate ───────────────────────────────────────────────────────────────

/// The organization a route names, once the caller is shown to be in it
/// and knowhow keeps its map.
pub(crate) async fn member_tenant(
    state: &KnowhowState,
    tenant_id: &str,
    auth: &AuthUser,
) -> Result<ObjectId, ApiError> {
    let tid = parse_tid(tenant_id)?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::NotAMember);
    }
    if !state.scope.serves(tid) {
        return Err(ApiError::NotFound(NOT_SERVED.into()));
    }
    Ok(tid)
}

/// A reader's organization: `KNOWHOW_VIEW`, or `KNOWHOW_EDIT`, which
/// includes it.
async fn reader(
    state: &KnowhowState,
    tenant_id: &str,
    auth: &AuthUser,
) -> Result<ObjectId, ApiError> {
    let tid = member_tenant(state, tenant_id, auth).await?;
    let perms = state
        .tenants
        .get_member_permissions(tid, auth.user_id)
        .await?;
    if permissions::has(perms, permissions::KNOWHOW_VIEW)
        || permissions::has(perms, permissions::KNOWHOW_EDIT)
    {
        return Ok(tid);
    }
    Err(ApiError::Forbidden(
        "Missing KNOWHOW_VIEW permission".into(),
    ))
}

/// A writer's organization: `KNOWHOW_EDIT`.
async fn writer(
    state: &KnowhowState,
    tenant_id: &str,
    auth: &AuthUser,
) -> Result<ObjectId, ApiError> {
    let tid = member_tenant(state, tenant_id, auth).await?;
    require_permission(
        &state.core,
        tid,
        auth.user_id,
        permissions::KNOWHOW_EDIT,
        "KNOWHOW_EDIT",
    )
    .await?;
    Ok(tid)
}

fn parse_id(raw: &str, what: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(raw).map_err(|_| ApiError::BadRequest(format!("Invalid {what}")))
}

fn bad(why: String) -> ApiError {
    ApiError::BadRequest(why)
}

fn limit_of(raw: Option<i64>) -> Result<i64, ApiError> {
    match raw {
        None => Ok(DEFAULT_LIMIT),
        Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
        Some(_) => Err(bad(format!("limit is 1 to {MAX_LIMIT}"))),
    }
}

/// An optional filter as a query string carries it: absent and empty alike.
fn given(raw: &Option<String>) -> Option<&str> {
    raw.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

async fn live_entity(
    state: &KnowhowState,
    tid: ObjectId,
    id: ObjectId,
) -> Result<KnowhowEntity, ApiError> {
    state
        .entities
        .get_live(tid, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such entity".into()))
}

/// The member who answers for an entity: a member of its organization.
async fn check_owner(state: &KnowhowState, tid: ObjectId, raw: &str) -> Result<ObjectId, ApiError> {
    let owner = parse_id(raw, "owner_id")?;
    if !state.tenants.is_member(tid, owner).await? {
        return Err(bad("the owner must be a member of the organization".into()));
    }
    Ok(owner)
}

fn stale(what: &str, asked: i64) -> ApiError {
    ApiError::Conflict(format!(
        "the {what} changed or was archived since rev {asked} — read it again"
    ))
}

/// ⚠️ What the fleet says of a machine is the fleet's (P4-2a): its name, its
/// owner and the facts the sync owns ([`SYNCED_FACTS`]), the network's among
/// them since P4-2b. A person's change to one would be undone by the next
/// sync, so it is refused, and so is archiving a machine, which leaves the
/// map when its device is removed. A person may still give a machine an
/// environment, attributes and facts of their own.
fn the_fleets(current: &KnowhowEntity, body: &UpdateEntity) -> Result<(), ApiError> {
    if current.source != SOURCE_SYNC {
        return Ok(());
    }
    let mut named: Vec<&str> = Vec::new();
    if body.name.is_some() {
        named.push("name");
    }
    if body.owner_id.is_some() {
        named.push("owner_id");
    }
    if let Some(facts) = &body.facts {
        named.extend(
            facts
                .keys()
                .map(String::as_str)
                .filter(|p| SYNCED_FACTS.contains(p)),
        );
    }
    if named.is_empty() {
        return Ok(());
    }
    Err(bad(format!(
        "the fleet says this of a {}: {} — change the device instead",
        current.kind,
        named.join(", ")
    )))
}

/// P4-2b — how many facts of a person's an entity holds. A synced one keeps
/// room for every fact the sync owns, so the sync never takes it past
/// [`MAX_FACTS`]: a person's own are held to what is left.
fn persons_room(current: &KnowhowEntity) -> usize {
    if current.source == SOURCE_SYNC {
        MAX_FACTS - SYNCED_FACTS.len()
    } else {
        MAX_FACTS
    }
}

/// The facts of `facts` that count against a person's room: on a synced
/// entity, those at a path the sync does not own.
fn persons_facts(current: &KnowhowEntity, facts: &[Fact]) -> usize {
    if current.source != SOURCE_SYNC {
        return facts.len();
    }
    facts
        .iter()
        .filter(|f| !SYNCED_FACTS.contains(&f.path.as_str()))
        .count()
}

/// A body's `facts`: path → value, or — in an update only — null to remove
/// that fact.
fn fact_changes(
    raw: Option<&Map<String, Value>>,
    null_removes: bool,
) -> Result<Vec<(String, Option<Bson>)>, ApiError> {
    let Some(map) = raw else {
        return Ok(Vec::new());
    };
    if map.len() > MAX_FACTS {
        return Err(bad(format!("at most {MAX_FACTS} facts")));
    }
    map.iter()
        .map(
            |(path, value)| -> Result<(String, Option<Bson>), ApiError> {
                let path = check_fact_path(path).map_err(bad)?.to_string();
                if value.is_null() && null_removes {
                    return Ok((path, None));
                }
                let value =
                    check_fact_value(value).map_err(|why| bad(format!("fact {path:?}: {why}")))?;
                Ok((path, Some(value)))
            },
        )
        .collect()
}

fn facts_bson<T: serde::Serialize>(facts: &T) -> Result<Bson, ApiError> {
    bson::to_bson(facts).map_err(|e| ApiError::Internal(e.to_string()))
}

// ─── reads ──────────────────────────────────────────────────────────────────

/// `GET /api/tenant/{tenant_id}/knowhow` — how much the map holds.
pub async fn status(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    Ok(Json(json!({
        "entities": state.entities.count_live(tid).await?,
        "edges": state.edges.count_live(tid).await?,
    })))
}

#[derive(Deserialize, Debug)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/tenant/{tenant_id}/knowhow/search` — the live entities whose
/// key or name holds `q` (as written, case aside), of a `kind`, in an `env`,
/// by kind and key. A kind this build does not know is searched for, not
/// refused: a newer server may have written it.
pub async fn search(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    let limit = limit_of(query.limit)?;
    let text = given(&query.q);
    if text.is_some_and(|q| q.chars().count() > MAX_QUERY_CHARS) {
        return Err(bad(format!(
            "q is longer than {MAX_QUERY_CHARS} characters"
        )));
    }
    let kind = given(&query.kind)
        .map(check_kind_word)
        .transpose()
        .map_err(bad)?;
    let env = given(&query.env).map(check_env).transpose().map_err(bad)?;
    let mut found = state
        .entities
        .search(tid, Search { text, kind, env }, limit + 1)
        .await?;
    let truncated = found.len() as i64 > limit;
    found.truncate(limit as usize);
    Ok(Json(json!({
        "entities": found.iter().map(NodeView::from).collect::<Vec<_>>(),
        "truncated": truncated,
    })))
}

#[derive(Deserialize, Debug)]
pub struct LookupQuery {
    pub kind: String,
    pub key: String,
}

/// `GET /api/tenant/{tenant_id}/knowhow/entity?kind=…&key=…` — one live
/// entity's card, by the name a chip and a session use for it.
pub async fn lookup(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(query): Query<LookupQuery>,
) -> Result<Json<CardView>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    let kind = check_kind_word(&query.kind).map_err(bad)?;
    let key = check_key(&query.key).map_err(bad)?;
    let entity = state
        .entities
        .find_live_by_key(tid, kind, key)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such entity".into()))?;
    Ok(Json(card(&state, tid, entity).await?))
}

/// `GET /api/tenant/{tenant_id}/knowhow/entity/{entity_id}` — one live
/// entity's card.
pub async fn get_entity(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path((tenant_id, entity_id)): Path<(String, String)>,
) -> Result<Json<CardView>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    let id = parse_id(&entity_id, "entity_id")?;
    let entity = live_entity(&state, tid, id).await?;
    Ok(Json(card(&state, tid, entity).await?))
}

/// An entity and its links to live entities — at most [`MAX_LINKS`]. A link
/// whose other end is archived is not shown: it leads nowhere.
async fn card(
    state: &KnowhowState,
    tid: ObjectId,
    entity: KnowhowEntity,
) -> Result<CardView, ApiError> {
    let Some(id) = entity.id else {
        return Err(ApiError::Internal("an entity read without its id".into()));
    };
    let mut edges = state
        .edges
        .touching(tid, &[id], MAX_LINKS as i64 + 1)
        .await?;
    let truncated = edges.len() > MAX_LINKS;
    edges.truncate(MAX_LINKS);
    let other = |e: &KnowhowEdge| if e.from == id { e.to } else { e.from };
    let ids: Vec<ObjectId> = edges
        .iter()
        .map(other)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let nodes: HashMap<ObjectId, NodeView> = state
        .entities
        .live_nodes(tid, &ids)
        .await?
        .iter()
        .map(|n| (n.id, NodeView::from(n)))
        .collect();
    let links = edges
        .iter()
        .filter_map(|e| {
            nodes.get(&other(e)).map(|n| LinkView {
                direction: if e.from == id { "out" } else { "in" },
                edge: EdgeView::of(e),
                other: n.clone(),
            })
        })
        .collect();
    Ok(CardView {
        entity: EntityView::of(&entity),
        links,
        truncated,
    })
}

#[derive(Deserialize, Debug)]
pub struct MapQuery {
    #[serde(default)]
    pub depth: Option<u8>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub env: Option<String>,
}

/// `GET /api/tenant/{tenant_id}/knowhow/map/{entity_id}` — an entity's
/// neighbourhood: the live entities one step out (`depth=1`, the default) or
/// two, of a `kind` and in an `env` when asked — the centre is always shown —
/// and the links between them.
///
/// ⚠️ Bounded at the source: each step is one indexed read of at most
/// [`MAX_MAP_EDGES`] links, and `truncated` says when one ran out. The
/// design named `$graphLookup`, whose traversal has no limit of its own: a
/// hub two steps out would load most of the map before any cap applied.
pub async fn map(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path((tenant_id, entity_id)): Path<(String, String)>,
    Query(query): Query<MapQuery>,
) -> Result<Json<Value>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    let id = parse_id(&entity_id, "entity_id")?;
    let depth = match query.depth {
        None => 1,
        Some(d @ 1..=2) => d,
        Some(_) => return Err(bad("depth is 1 or 2".into())),
    };
    let kind = given(&query.kind)
        .map(check_kind_word)
        .transpose()
        .map_err(bad)?;
    let env = given(&query.env).map(check_env).transpose().map_err(bad)?;
    let center = live_entity(&state, tid, id).await?;

    let mut order = vec![id];
    let mut nodes: HashMap<ObjectId, NodeView> = HashMap::from([(id, NodeView::from(&center))]);
    let mut edges: Vec<KnowhowEdge> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut frontier = vec![id];
    let mut truncated = false;
    for _ in 0..depth {
        let found = state
            .edges
            .touching(tid, &frontier, MAX_MAP_EDGES as i64 + 1)
            .await?;
        if found.len() > MAX_MAP_EDGES {
            truncated = true;
        }
        let fresh: Vec<KnowhowEdge> = found
            .into_iter()
            .take(MAX_MAP_EDGES)
            .filter(|e| e.id.is_some_and(|i| !seen.contains(&i)))
            .collect();
        let unknown: Vec<ObjectId> = fresh
            .iter()
            .flat_map(|e| [e.from, e.to])
            .filter(|i| !nodes.contains_key(i))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut next = Vec::new();
        for n in state.entities.live_nodes(tid, &unknown).await? {
            if kind.is_some_and(|k| n.kind != k) || env.is_some_and(|e| n.env.as_deref() != Some(e))
            {
                continue;
            }
            next.push(n.id);
            order.push(n.id);
            nodes.insert(n.id, NodeView::from(&n));
        }
        for e in fresh {
            if !(nodes.contains_key(&e.from) && nodes.contains_key(&e.to)) {
                continue;
            }
            if edges.len() == MAX_MAP_EDGES {
                truncated = true;
                break;
            }
            if let Some(i) = e.id {
                seen.insert(i);
            }
            edges.push(e);
        }
        if truncated || next.is_empty() {
            break;
        }
        frontier = next;
    }
    let entities: Vec<NodeView> = order.iter().filter_map(|i| nodes.remove(i)).collect();
    Ok(Json(json!({
        "center": id.to_hex(),
        "entities": entities,
        "edges": edges.iter().map(EdgeView::of).collect::<Vec<_>>(),
        "truncated": truncated,
    })))
}

#[derive(Deserialize, Debug)]
pub struct HistoryQuery {
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/tenant/{tenant_id}/knowhow/history` — the map's writes, newest
/// first; one entity's with `?entity=`, its links' included, archived or
/// not: a history outlives what it describes.
pub async fn history(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let tid = reader(&state, &tenant_id, &auth).await?;
    let limit = limit_of(query.limit)?;
    let entity = given(&query.entity)
        .map(|e| parse_id(e, "entity"))
        .transpose()?;
    let rows = state.history.list(tid, entity, limit).await?;
    Ok(Json(json!({
        "history": rows.iter().map(HistoryView::of).collect::<Vec<_>>(),
    })))
}

// ─── writes ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CreateEntity {
    pub kind: String,
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub attrs: Option<Value>,
    /// path → value.
    #[serde(default)]
    pub facts: Option<Map<String, Value>>,
}

/// `POST /api/tenant/{tenant_id}/knowhow/entity` — put an entity on the
/// map, its facts the caller's.
///
/// ⚠️ Never a `machine`: a machine mirrors a device, and the fleet's sync
/// (P4-2) records it with its node id and overlay address beside its name.
/// A host that is no device is an `endpoint`.
pub async fn create_entity(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<CreateEntity>,
) -> Result<(StatusCode, Json<EntityView>), ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let kind = check_kind(&body.kind).map_err(bad)?;
    if SYNCED_KINDS.contains(&kind) {
        return Err(bad(format!(
            "a {kind} mirrors a device: the fleet's sync records it, never a person \
             (a host that is no device is an endpoint)"
        )));
    }
    let key = check_key(&body.key).map_err(bad)?;
    let name = check_name(&body.name).map_err(bad)?;
    let env = given(&body.env)
        .map(check_env)
        .transpose()
        .map_err(bad)?
        .map(str::to_string);
    let owner_id = match given(&body.owner_id) {
        Some(raw) => Some(check_owner(&state, tid, raw).await?),
        None => None,
    };
    let attrs = match &body.attrs {
        Some(v) => check_attrs(v, MAX_ATTRS_BYTES).map_err(bad)?,
        None => Document::new(),
    };
    let stamp = Stamp::person(tid, auth.user_id);
    let now = stamp.at;
    let changes = fact_changes(body.facts.as_ref(), false)?;
    let (facts, said) = merge_facts(&[], &changes, SOURCE_PERSON, Some(auth.user_id), now);
    if state.entities.count_live(tid).await? >= MAX_ENTITIES {
        return Err(ApiError::Conflict(format!(
            "the map holds its most entities ({MAX_ENTITIES})"
        )));
    }
    let mut entity = KnowhowEntity {
        id: None,
        tenant_id: tid,
        kind: kind.into(),
        key: key.into(),
        name,
        env,
        owner_id,
        attrs,
        facts,
        status: STATUS_ACTIVE.into(),
        source: SOURCE_PERSON.into(),
        rev: 1,
        created_by: Some(auth.user_id),
        created_at: now,
        updated_by: Some(auth.user_id),
        updated_at: now,
        deleted_at: None,
    };
    let id = match state.entities.create(&entity).await {
        Ok(id) => id,
        Err(DaoError::DuplicateKey(_)) => {
            return Err(ApiError::Conflict(format!(
                "a {kind} with the key {key} is already on the map"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    entity.id = Some(id);
    let set = doc! {
        "kind": entity.kind.as_str(), "key": entity.key.as_str(),
        "name": entity.name.as_str(), "env": entity.env.clone(),
        "owner_id": entity.owner_id, "attrs": entity.attrs.clone(), "facts": said,
    };
    let row = stamp.row(TARGET_ENTITY, id, vec![id], 1, OP_CREATE, set);
    record(&state, vec![row]).await;
    Ok((StatusCode::CREATED, Json(EntityView::of(&entity))))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct UpdateEntity {
    /// The rev the change was made against.
    pub rev: i64,
    #[serde(default)]
    pub name: Option<String>,
    /// `""` clears it.
    #[serde(default)]
    pub env: Option<String>,
    /// `""` clears it.
    #[serde(default)]
    pub owner_id: Option<String>,
    /// The whole of them, replaced.
    #[serde(default)]
    pub attrs: Option<Value>,
    /// path → value, or null to remove that fact; the facts it does not
    /// name are left as they are.
    #[serde(default)]
    pub facts: Option<Map<String, Value>>,
}

/// `PATCH /api/tenant/{tenant_id}/knowhow/entity/{entity_id}` — change an
/// entity at the rev it was read; a change that changes nothing writes
/// nothing. Its kind and key never change (unknown fields here): a chip and
/// a session name it by them, so another name is another entity.
pub async fn update_entity(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path((tenant_id, entity_id)): Path<(String, String)>,
    Json(body): Json<UpdateEntity>,
) -> Result<Json<EntityView>, ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let id = parse_id(&entity_id, "entity_id")?;
    if body.name.is_none()
        && body.env.is_none()
        && body.owner_id.is_none()
        && body.attrs.is_none()
        && body.facts.is_none()
    {
        return Err(bad("nothing to change".into()));
    }
    let current = live_entity(&state, tid, id).await?;
    if current.rev != body.rev {
        return Err(stale("entity", body.rev));
    }
    the_fleets(&current, &body)?;
    let stamp = Stamp::person(tid, auth.user_id);
    let now = stamp.at;
    // What the row gets, and what the history says it got.
    let mut set = Document::new();
    let mut said = Document::new();
    if let Some(raw) = &body.name {
        let name = check_name(raw).map_err(bad)?;
        if name != current.name {
            set.insert("name", name.as_str());
            said.insert("name", name);
        }
    }
    if let Some(raw) = &body.env {
        let env = match raw.trim() {
            "" => None,
            e => Some(check_env(e).map_err(bad)?.to_string()),
        };
        if env != current.env {
            set.insert("env", env.clone());
            said.insert("env", env);
        }
    }
    if let Some(raw) = &body.owner_id {
        let owner = match raw.trim() {
            "" => None,
            o => Some(check_owner(&state, tid, o).await?),
        };
        if owner != current.owner_id {
            set.insert("owner_id", owner);
            said.insert("owner_id", owner);
        }
    }
    if let Some(v) = &body.attrs {
        let attrs = check_attrs(v, MAX_ATTRS_BYTES).map_err(bad)?;
        if attrs != current.attrs {
            set.insert("attrs", attrs.clone());
            said.insert("attrs", attrs);
        }
    }
    let changes = fact_changes(body.facts.as_ref(), true)?;
    if !changes.is_empty() {
        let (facts, changed) = merge_facts(
            &current.facts,
            &changes,
            SOURCE_PERSON,
            Some(auth.user_id),
            now,
        );
        // An edit that adds none of a person's passes: a path the sync took
        // over later can leave a machine past its room, and tidying it must
        // stay possible.
        let room = persons_room(&current);
        let held = persons_facts(&current, &facts);
        if held > room && held > persons_facts(&current, &current.facts) {
            return Err(bad(format!(
                "a {} holds at most {room} facts of a person's",
                current.kind
            )));
        }
        if !changed.is_empty() {
            set.insert("facts", facts_bson(&facts)?);
            said.insert("facts", changed);
        }
    }
    if set.is_empty() {
        return Ok(Json(EntityView::of(&current)));
    }
    set.extend(stamp.touched());
    let Some(after) = state.entities.update_at(tid, id, body.rev, set).await? else {
        return Err(stale("entity", body.rev));
    };
    let row = stamp.row(TARGET_ENTITY, id, vec![id], after.rev, OP_UPDATE, said);
    record(&state, vec![row]).await;
    Ok(Json(EntityView::of(&after)))
}

/// `?rev=` — the rev an archive was asked against.
#[derive(Deserialize, Debug)]
pub struct RevQuery {
    pub rev: i64,
}

/// `DELETE /api/tenant/{tenant_id}/knowhow/entity/{entity_id}?rev=…` —
/// archive an entity at the rev it was read: off the map, its key free for
/// another, kept as a record; and every link to it archived with it, each in
/// the history.
pub async fn archive_entity(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path((tenant_id, entity_id)): Path<(String, String)>,
    Query(query): Query<RevQuery>,
) -> Result<Json<Value>, ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let id = parse_id(&entity_id, "entity_id")?;
    // P4-2a — a machine leaves the map when its device is removed, never by a
    // person's hand: the next sync would only put it back.
    if let Some(current) = state.entities.get_live(tid, id).await?
        && current.source == SOURCE_SYNC
    {
        return Err(bad(format!(
            "a {} leaves the map when its device is removed",
            current.kind
        )));
    }
    let stamp = Stamp::person(tid, auth.user_id);
    match archive_at(&state, &stamp, id, query.rev).await? {
        Some(links) => Ok(Json(json!({ "archived": true, "links_archived": links }))),
        None => Err(match state.entities.get_live(tid, id).await? {
            Some(_) => stale("entity", query.rev),
            None => ApiError::NotFound("no such entity".into()),
        }),
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CreateEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default)]
    pub attrs: Option<Value>,
}

/// `POST /api/tenant/{tenant_id}/knowhow/edge` — link two live entities of
/// the organization: `from` `kind` `to`. Once per kind between the two.
pub async fn create_edge(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<CreateEdge>,
) -> Result<(StatusCode, Json<EdgeView>), ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let kind = check_edge_kind(&body.kind).map_err(bad)?;
    let from = parse_id(&body.from, "from")?;
    let to = parse_id(&body.to, "to")?;
    if from == to {
        return Err(bad("an entity is not linked to itself".into()));
    }
    let attrs = match &body.attrs {
        Some(v) => check_attrs(v, MAX_EDGE_ATTRS_BYTES).map_err(bad)?,
        None => Document::new(),
    };
    for (end, eid) in [("from", from), ("to", to)] {
        if state.entities.get_live(tid, eid).await?.is_none() {
            return Err(ApiError::NotFound(format!("no such entity ({end})")));
        }
    }
    if state.edges.count_live(tid).await? >= MAX_EDGES {
        return Err(ApiError::Conflict(format!(
            "the map holds its most links ({MAX_EDGES})"
        )));
    }
    let stamp = Stamp::person(tid, auth.user_id);
    let now = stamp.at;
    let mut edge = KnowhowEdge {
        id: None,
        tenant_id: tid,
        from,
        to,
        kind: kind.into(),
        attrs,
        status: STATUS_ACTIVE.into(),
        source: SOURCE_PERSON.into(),
        rev: 1,
        created_by: Some(auth.user_id),
        created_at: now,
        updated_by: Some(auth.user_id),
        updated_at: now,
        deleted_at: None,
    };
    let id = match state.edges.create(&edge).await {
        Ok(id) => id,
        Err(DaoError::DuplicateKey(_)) => {
            return Err(ApiError::Conflict(format!(
                "the two are already linked by {kind}"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    edge.id = Some(id);
    let set = doc! { "from": from, "to": to, "kind": kind, "attrs": edge.attrs.clone() };
    let row = stamp.row(TARGET_EDGE, id, vec![from, to], 1, OP_CREATE, set);
    record(&state, vec![row]).await;
    Ok((StatusCode::CREATED, Json(EdgeView::of(&edge))))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct UpdateEdge {
    /// The rev the change was made against.
    pub rev: i64,
    /// The whole of them, replaced.
    pub attrs: Value,
}

/// `PATCH /api/tenant/{tenant_id}/knowhow/edge/{edge_id}` — change a link's
/// attributes at the rev it was read. Its ends and its kind never change: a
/// different link is archived and made again.
pub async fn update_edge(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path((tenant_id, edge_id)): Path<(String, String)>,
    Json(body): Json<UpdateEdge>,
) -> Result<Json<EdgeView>, ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let id = parse_id(&edge_id, "edge_id")?;
    let attrs = check_attrs(&body.attrs, MAX_EDGE_ATTRS_BYTES).map_err(bad)?;
    let current = state
        .edges
        .get_live(tid, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such link".into()))?;
    if current.rev != body.rev {
        return Err(stale("link", body.rev));
    }
    if attrs == current.attrs {
        return Ok(Json(EdgeView::of(&current)));
    }
    let stamp = Stamp::person(tid, auth.user_id);
    let set = doc! { "attrs": attrs.clone(), "updated_by": stamp.by, "updated_at": stamp.at };
    let Some(after) = state.edges.update_at(tid, id, body.rev, set).await? else {
        return Err(stale("link", body.rev));
    };
    let ends = vec![after.from, after.to];
    let row = stamp.row(
        TARGET_EDGE,
        id,
        ends,
        after.rev,
        OP_UPDATE,
        doc! { "attrs": attrs },
    );
    record(&state, vec![row]).await;
    Ok(Json(EdgeView::of(&after)))
}

/// `DELETE /api/tenant/{tenant_id}/knowhow/edge/{edge_id}?rev=…` — archive
/// a link at the rev it was read.
pub async fn archive_edge(
    State(state): State<KnowhowState>,
    auth: AuthUser,
    Path((tenant_id, edge_id)): Path<(String, String)>,
    Query(query): Query<RevQuery>,
) -> Result<Json<Value>, ApiError> {
    let tid = writer(&state, &tenant_id, &auth).await?;
    let id = parse_id(&edge_id, "edge_id")?;
    let stamp = Stamp::person(tid, auth.user_id);
    let set = doc! {
        "status": STATUS_ARCHIVED, "deleted_at": stamp.at,
        "updated_by": stamp.by, "updated_at": stamp.at,
    };
    let Some(after) = state.edges.update_at(tid, id, query.rev, set).await? else {
        return Err(match state.edges.get_live(tid, id).await? {
            Some(_) => stale("link", query.rev),
            None => ApiError::NotFound("no such link".into()),
        });
    };
    let ends = vec![after.from, after.to];
    let row = stamp.row(
        TARGET_EDGE,
        id,
        ends,
        after.rev,
        OP_ARCHIVE,
        doc! { "status": STATUS_ARCHIVED },
    );
    record(&state, vec![row]).await;
    Ok(Json(json!({ "archived": true })))
}
