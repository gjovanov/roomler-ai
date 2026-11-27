// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Admin CRUD for the overlay L3 ACL.
//!
//! Mirrors the tunnel-policy admin surface (`routes/tunnel.rs`) but drives a
//! different enforcement point: these rows shape the NETMAP each node receives
//! — which peers it sees and which of their approved routes it may install —
//! rather than gating individual tunnel flows.
//!
//! The tenant-wide posture lives on `OverlayNetwork.acl_mode`
//! (`off` → `warn` → `enforce`) and is edited through
//! `PUT /overlay-acl/mode`. It defaults to `off`, so adding policies is
//! inert until an admin deliberately turns enforcement on — the feature can
//! never black-hole a live mesh by merely being deployed.
//!
//! Everything here requires `MANAGE_AGENTS`: overlay traffic bypasses the
//! tunnel ACL entirely, so an overlay grant is at least as powerful as an
//! agent-management action.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
};
use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::models::{
    OverlayAclMode, OverlayPolicy, OverlayRule, OverlaySelector, OverlayTarget,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};

use roomler_core::guards::require_permission;
use roomler_core::{ApiError, extractors::auth::AuthUser};

use crate::NetworkState;

#[derive(Debug, Serialize)]
pub struct OverlayPolicyResponse {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub enabled: bool,
    pub sources: Vec<OverlaySelectorView>,
    pub via: Vec<OverlayTargetView>,
    pub destinations: Vec<OverlayRule>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<OverlayPolicy> for OverlayPolicyResponse {
    fn from(p: OverlayPolicy) -> Self {
        Self {
            id: p.id.map(|i| i.to_hex()).unwrap_or_default(),
            tenant_id: p.tenant_id.to_hex(),
            name: p.name,
            enabled: p.enabled,
            sources: p.sources.into_iter().map(Into::into).collect(),
            via: p.via.into_iter().map(Into::into).collect(),
            destinations: p.destinations,
            created_at: p.created_at.try_to_rfc3339_string().unwrap_or_default(),
            updated_at: p.updated_at.try_to_rfc3339_string().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OverlayPolicyListResponse {
    pub items: Vec<OverlayPolicyResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    /// The tenant's current posture, so the UI can render the mode selector
    /// and the "rules are not being enforced yet" banner from one round-trip.
    pub mode: OverlayAclMode,
}

#[derive(Debug, Deserialize)]
pub struct UpsertOverlayPolicyRequest {
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub sources: Vec<OverlaySelector>,
    pub via: Vec<OverlayTarget>,
    pub destinations: Vec<OverlayRule>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct SetAclModeRequest {
    pub mode: OverlayAclMode,
}

#[derive(Debug, Serialize)]
pub struct AclModeResponse {
    pub mode: OverlayAclMode,
}

/// Reject rules that can never match, so an admin never authors a silent
/// no-op. Every CIDR must parse, and the port range must be sane.
fn validate(body: &UpsertOverlayPolicyRequest) -> Result<(), ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name is required".into()));
    }
    if body.sources.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one source is required".into(),
        ));
    }
    if body.via.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one via node is required".into(),
        ));
    }
    if body.destinations.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one destination is required".into(),
        ));
    }
    for d in &body.destinations {
        if d.cidr.trim().parse::<ipnet::IpNet>().is_err() {
            return Err(ApiError::BadRequest(format!(
                "destination '{}' is not a valid CIDR (a bare address needs a prefix, e.g. 10.0.0.5/32)",
                d.cidr
            )));
        }
        if d.port_range.low == 0 || d.port_range.high < d.port_range.low {
            return Err(ApiError::BadRequest(format!(
                "destination '{}' has an invalid port range",
                d.cidr
            )));
        }
    }
    Ok(())
}

/// The role ids a rule's `RoleId` sources name.
fn source_role_ids(sources: &[OverlaySelector]) -> Vec<ObjectId> {
    sources
        .iter()
        .filter_map(|s| match s {
            OverlaySelector::RoleId { role_id } => Some(*role_id),
            _ => None,
        })
        .collect()
}

/// The gate on every route here: `MANAGE_AGENTS` in the path org, decided
/// FIRST, before a policy id is parsed, a body read or a policy looked up
/// (#4187), as its tunnel-policy twin `tunnel_policy_manager` is. A member
/// without the bit gets one 403 `forbidden` whatever the body is, so the
/// writes take their body as `Result<Json<T>, JsonRejection>` and read it
/// after this; a holder is answered one that does not parse as before
/// (`ApiError::JsonBody`).
async fn gate(
    state: &NetworkState,
    tenant_id: &str,
    auth: &AuthUser,
) -> Result<ObjectId, ApiError> {
    let tid = ObjectId::parse_str(tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    require_permission(
        state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    Ok(tid)
}

pub async fn list(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<OverlayPolicyListResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let page = state.overlay_policies.list_for_tenant(tid, &params).await?;
    let mode = state.overlay_networks.get_or_create(tid).await?.acl_mode;
    Ok(Json(OverlayPolicyListResponse {
        items: page.items.into_iter().map(Into::into).collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        mode,
    }))
}

pub async fn create(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    body: Result<Json<UpsertOverlayPolicyRequest>, JsonRejection>,
) -> Result<Json<OverlayPolicyResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let Json(body) = body?;
    validate(&body)?;
    // A role source names a role of this org, or the body is a 400.
    roomler_core::guards::require_org_roles(&state, tid, &source_role_ids(&body.sources), &[])
        .await?;
    let created = state
        .overlay_policies
        .create(
            tid,
            body.name.trim().to_string(),
            body.enabled,
            body.sources,
            body.via,
            body.destinations,
        )
        .await?;
    refan_tenant(&state, tid).await;
    Ok(Json(created.into()))
}

pub async fn get(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
) -> Result<Json<OverlayPolicyResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    // LIVE: a deleted policy is a 404 here exactly as it is absent from the
    // listing and from the netmap compiler's set (#1829).
    Ok(Json(
        state
            .overlay_policies
            .find_live_in_tenant(tid, pid)
            .await?
            .into(),
    ))
}

/// A deleted policy is a 404 (#1829): the lookup is live, the write under it
/// is live-scoped on its own, and the tenant is re-fanned only when a row
/// actually changed — before this, a `PUT` on a tombstone wrote `enabled:
/// true` onto it, re-fanned every node for an edit the compiler could not
/// see, and then served the edited tombstone back.
pub async fn update(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
    body: Result<Json<UpsertOverlayPolicyRequest>, JsonRejection>,
) -> Result<Json<OverlayPolicyResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    // Read straight after the gate, before the id: a holder is answered a
    // body that does not parse as before, whatever the id (#4187).
    let Json(body) = body?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    // Resolve the target LIVE before touching it — a removed policy is a 404
    // whatever the body says, like a bogus or foreign id.
    let stored = state.overlay_policies.find_live_in_tenant(tid, pid).await?;
    validate(&body)?;
    // A role source names a role of this org; one the stored rule already
    // names is accepted back (`require_org_roles`).
    roomler_core::guards::require_org_roles(
        &state,
        tid,
        &source_role_ids(&body.sources),
        &source_role_ids(&stored.sources),
    )
    .await?;
    let changed = state
        .overlay_policies
        .update(
            tid,
            pid,
            Some(body.name.trim().to_string()),
            Some(body.enabled),
            Some(body.sources),
            Some(body.via),
            Some(body.destinations),
        )
        .await?;
    if changed {
        refan_tenant(&state, tid).await;
    }
    // The refetch is live too: a removal that raced the write is a 404, not
    // a 200 carrying the tombstone.
    Ok(Json(
        state
            .overlay_policies
            .find_live_in_tenant(tid, pid)
            .await?
            .into(),
    ))
}

/// Soft-delete. A repeat `DELETE` (or a bogus / foreign id) is a 404, the
/// same answer the tunnel-policy sibling gives and the one that leaves the
/// state alone (#1829): the write is live-scoped, so a tombstone matches
/// nothing — before this it re-stamped the removal time, answered
/// `deleted: true` and re-fanned the tenant for a removal someone else did.
pub async fn delete(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    let removed = state.overlay_policies.soft_delete(tid, pid).await?;
    if !removed {
        return Err(ApiError::NotFound("Overlay policy not found".into()));
    }
    refan_tenant(&state, tid).await;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

pub async fn get_mode(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<AclModeResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let mode = state.overlay_networks.get_or_create(tid).await?.acl_mode;
    Ok(Json(AclModeResponse { mode }))
}

pub async fn set_mode(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    body: Result<Json<SetAclModeRequest>, JsonRejection>,
) -> Result<Json<AclModeResponse>, ApiError> {
    let tid = gate(&state, &tenant_id, &auth).await?;
    let Json(body) = body?;
    state.overlay_networks.set_acl_mode(tid, body.mode).await?;
    refan_tenant(&state, tid).await;
    Ok(Json(AclModeResponse { mode: body.mode }))
}

/// Re-fan the org after a change of who holds which role
/// (`MembershipHooks::org_roles_changed`): role-scoped rules are compiled into
/// each node's netmap, the DERP allow table and the relay grants, so a role
/// given, taken or deleted takes effect on the nodes only when they are
/// recompiled. Only for an org that has an overlay network whose ACL is not
/// `off`, the only postures in which a role-scoped rule shapes anything; the
/// lookup never creates a network row.
pub(crate) async fn refan_for_role_change(state: &NetworkState, tenant_id: ObjectId) {
    let network = match state.overlay_networks.find_for_tenant(tenant_id).await {
        Ok(Some(n)) => n,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(%tenant_id, %e,
                "overlay acl: roles changed, but the network could not be read; nodes keep their \
                 netmaps until the next re-fan");
            return;
        }
    };
    if matches!(network.acl_mode, OverlayAclMode::Off) {
        return;
    }
    refan_tenant(state, tenant_id).await;
}

/// Re-fan every node in the tenant so a policy edit takes effect immediately,
/// and refresh the DERP relay allow table with it.
///
/// `refan_node` is single-node; a policy change can alter what EVERY node
/// sees of every other, so the whole network has to be re-evaluated. Nodes are
/// re-fanned one at a time (each fan-out is itself per-recipient), which is
/// fine at fleet scale and keeps the write path simple — netmap edits are rare.
///
/// The DERP rebuild is NOT optional here: reshaping netmaps alone would leave a
/// denied pair still able to relay by pubkey, which is the bypass the gate
/// exists to close. It runs AFTER the fan so the two never disagree in the
/// permissive direction — a peer loses its netmap entry before it loses relay.
async fn refan_tenant(state: &NetworkState, tenant_id: ObjectId) {
    let Ok(network) = state.overlay_networks.get_or_create(tenant_id).await else {
        return;
    };
    let Some(network_id) = network.id else { return };
    let Ok(nodes) = state
        .overlay_nodes
        .list_active_in_network(tenant_id, network_id)
        .await
    else {
        return;
    };
    for n in &nodes {
        crate::overlay::refan_node(state, n).await;
    }
    crate::derp_acl::rebuild(state, tenant_id, network_id).await;
    // FR-19 — a policy edit can un-grant a live relay session; revoke those.
    crate::org_relay::reconcile_acl(state, tenant_id).await;
}

/// An overlay rule's source as an overlay-acl answer carries it (#3636): its
/// id the 24-hex string `docs/api.md` promises, never bson's `{"$oid": …}`,
/// by which the Overlay ACL tab could name no node, member or role. The kinds
/// and the `id` key are the model's (`OverlaySelector`), so what a page saves
/// back is what `UpsertOverlayPolicyRequest` reads. Serialize only, built at
/// the route: what Mongo stores and what the netmap compile reads stay the
/// model's.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlaySelectorView {
    NodeId {
        #[serde(rename = "id")]
        node_id: String,
    },
    UserId {
        #[serde(rename = "id")]
        user_id: String,
    },
    RoleId {
        #[serde(rename = "id")]
        role_id: String,
    },
    AllNodes,
}

/// No `_` arm and no `..`: a source kind or field the model gains does not
/// compile until its answer is written here.
impl From<OverlaySelector> for OverlaySelectorView {
    fn from(source: OverlaySelector) -> Self {
        match source {
            OverlaySelector::NodeId { node_id } => Self::NodeId {
                node_id: node_id.to_hex(),
            },
            OverlaySelector::UserId { user_id } => Self::UserId {
                user_id: user_id.to_hex(),
            },
            OverlaySelector::RoleId { role_id } => Self::RoleId {
                role_id: role_id.to_hex(),
            },
            OverlaySelector::AllNodes => Self::AllNodes,
        }
    }
}

/// A rule's via node as an answer carries it; see [`OverlaySelectorView`].
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayTargetView {
    NodeId {
        #[serde(rename = "id")]
        node_id: String,
    },
    AllNodes,
}

impl From<OverlayTarget> for OverlayTargetView {
    fn from(target: OverlayTarget) -> Self {
        match target {
            OverlayTarget::NodeId { node_id } => Self::NodeId {
                node_id: node_id.to_hex(),
            },
            OverlayTarget::AllNodes => Self::AllNodes,
        }
    }
}

#[cfg(test)]
mod tests {
    //! #3636 — what an overlay-ACL answer carries on the wire. Every id is the
    //! 24-hex string `docs/api.md` promises: the Overlay ACL tab names a
    //! source or a via node by it, and its editor sends it back when it saves.

    use super::*;
    use bson::DateTime;
    use roomler_ai_remote_control::models::{PortRange, ProtocolKind};
    use serde_json::{Value, json};

    /// A stored rule that names every source and via kind there is: a node,
    /// a user and a role as sources, then the catch-all; the node as the via,
    /// then the catch-all. The three ids, in that order.
    fn every_kind() -> (OverlayPolicy, [ObjectId; 3]) {
        let ids = [ObjectId::new(), ObjectId::new(), ObjectId::new()];
        let [node, user, role] = ids;
        let now = DateTime::now();
        let policy = OverlayPolicy {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            name: "every kind".into(),
            enabled: true,
            sources: vec![
                OverlaySelector::NodeId { node_id: node },
                OverlaySelector::UserId { user_id: user },
                OverlaySelector::RoleId { role_id: role },
                OverlaySelector::AllNodes,
            ],
            via: vec![
                OverlayTarget::NodeId { node_id: node },
                OverlayTarget::AllNodes,
            ],
            destinations: vec![OverlayRule {
                cidr: "192.0.2.1/32".into(),
                port_range: PortRange { low: 9, high: 9 },
                proto: ProtocolKind::Tcp,
            }],
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        (policy, ids)
    }

    fn is_hex24(s: &str) -> bool {
        s.len() == 24 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    /// What is wrong with `entries` as the browser reads them, against
    /// `want`: `(kind, the id)`, `None` for a catch-all. An id must be the
    /// 24-hex string of the stored one; a catch-all is the single key `kind`;
    /// nothing is lost, added or reordered.
    fn wire_problems(
        what: &str,
        entries: &Value,
        want: &[(&str, Option<ObjectId>)],
    ) -> Vec<String> {
        let Some(got) = entries.as_array() else {
            return vec![format!("{what}: not an array: {entries}")];
        };
        let mut wrong = Vec::new();
        if got.len() != want.len() {
            wrong.push(format!(
                "{what}: {} entries, want {}: {entries}",
                got.len(),
                want.len()
            ));
        }
        for (i, (entry, (kind, id))) in got.iter().zip(want).enumerate() {
            let at = format!("{what}[{i}] ({kind})");
            if entry["kind"] != *kind {
                wrong.push(format!("{at}: the kind is {}", entry["kind"]));
            }
            let keys = entry.as_object().map_or(0, |o| o.len());
            match id {
                None if keys != 1 => wrong.push(format!(
                    "{at}: a catch-all is the single key `kind`: {entry}"
                )),
                None => {}
                Some(id) => match entry["id"].as_str() {
                    None => wrong.push(format!("{at}: the id is not a string: {entry}")),
                    Some(s) if !is_hex24(s) => {
                        wrong.push(format!("{at}: the id is not 24 lowercase hex: {s}"))
                    }
                    Some(s) if s != id.to_hex() => {
                        wrong.push(format!("{at}: the id {s} is not the stored {id}"))
                    }
                    Some(_) if keys != 2 => {
                        wrong.push(format!("{at}: `kind` and `id` only: {entry}"))
                    }
                    Some(_) => {}
                },
            }
        }
        wrong
    }

    /// The answer of every overlay-acl route that returns a rule (create,
    /// list, get, update) is an `OverlayPolicyResponse`: its source and via
    /// ids are hex strings, never bson's extended JSON `{"$oid": …}`, by
    /// which the Overlay ACL tab could name no node, user or role.
    #[test]
    fn an_overlay_policy_answer_names_every_source_and_via_id_as_a_hex_string() {
        let (policy, [node, user, role]) = every_kind();
        let answer = serde_json::to_value(OverlayPolicyResponse::from(policy)).expect("serialises");
        let mut wrong = wire_problems(
            "sources",
            &answer["sources"],
            &[
                ("node_id", Some(node)),
                ("user_id", Some(user)),
                ("role_id", Some(role)),
                ("all_nodes", None),
            ],
        );
        wrong.extend(wire_problems(
            "via",
            &answer["via"],
            &[("node_id", Some(node)), ("all_nodes", None)],
        ));
        let text = answer.to_string();
        if text.contains("$oid") {
            wrong.push(format!("the answer carries `$oid`: {text}"));
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// Every source kind, with the JSON an answer carries for it, written
    /// out: the kind strings are a compatibility surface the web app matches.
    fn every_source() -> Vec<(OverlaySelector, Value)> {
        let [node, user, role] = [(); 3].map(|()| ObjectId::new());
        vec![
            (
                OverlaySelector::NodeId { node_id: node },
                json!({ "kind": "node_id", "id": node.to_hex() }),
            ),
            (
                OverlaySelector::UserId { user_id: user },
                json!({ "kind": "user_id", "id": user.to_hex() }),
            ),
            (
                OverlaySelector::RoleId { role_id: role },
                json!({ "kind": "role_id", "id": role.to_hex() }),
            ),
            (OverlaySelector::AllNodes, json!({ "kind": "all_nodes" })),
        ]
    }

    /// Every via kind, as [`every_source`].
    fn every_via() -> Vec<(OverlayTarget, Value)> {
        let node = ObjectId::new();
        vec![
            (
                OverlayTarget::NodeId { node_id: node },
                json!({ "kind": "node_id", "id": node.to_hex() }),
            ),
            (OverlayTarget::AllNodes, json!({ "kind": "all_nodes" })),
        ]
    }

    fn source_answer(source: &OverlaySelector) -> Value {
        serde_json::to_value(OverlaySelectorView::from(source.clone())).expect("serialises")
    }

    fn via_answer(via: &OverlayTarget) -> Value {
        serde_json::to_value(OverlayTargetView::from(via.clone())).expect("serialises")
    }

    #[test]
    fn each_source_and_via_kind_answers_its_kind_and_its_hex_id() {
        for (source, want) in every_source() {
            assert_eq!(source_answer(&source), want, "{source:?}");
        }
        for (via, want) in every_via() {
            assert_eq!(via_answer(&via), want, "{via:?}");
        }
    }

    /// The view writes the model's own kind and keys, so a kind the model
    /// renamed or a key it moved cannot leave the answers behind.
    #[test]
    fn an_answer_names_the_kind_and_the_keys_the_model_writes() {
        fn keys(v: &Value) -> Vec<&String> {
            v.as_object().expect("an object").keys().collect()
        }
        for (source, _) in every_source() {
            let (answer, model) = (
                source_answer(&source),
                serde_json::to_value(&source).unwrap(),
            );
            assert_eq!(answer["kind"], model["kind"], "{source:?}");
            assert_eq!(keys(&answer), keys(&model), "{source:?}");
        }
        for (via, _) in every_via() {
            let (answer, model) = (via_answer(&via), serde_json::to_value(&via).unwrap());
            assert_eq!(answer["kind"], model["kind"], "{via:?}");
            assert_eq!(keys(&answer), keys(&model), "{via:?}");
        }
    }

    /// What an answer carries reads back through the request type as the
    /// stored value it came from: the editor's save is a round trip.
    #[test]
    fn an_answer_reads_back_as_the_source_or_via_it_came_from() {
        for (source, _) in every_source() {
            let back: OverlaySelector = serde_json::from_value(source_answer(&source)).unwrap();
            assert_eq!(back, source);
        }
        for (via, _) in every_via() {
            let back: OverlayTarget = serde_json::from_value(via_answer(&via)).unwrap();
            assert_eq!(back, via);
        }
    }
}
