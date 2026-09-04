// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Unified device list — agents + tunnel clients as ONE server-paginated,
//! server-searched, server-sorted feed for the devices grid, joined to their
//! overlay nodes (address + MagicDNS label) in memory.
//!
//! In-memory compose, deliberately: fleets are tens of devices (the scale
//! every netmap fan-out already loads), the repo has no aggregation-pipeline
//! precedent, and `q` must match ACROSS the join (overlay ip, MagicDNS fqdn)
//! — which a per-collection Mongo query cannot.
//!
//! ⚠️ The overlay network is resolved with `find_for_tenant`, NEVER
//! `get_or_create`: the create half inserts a network row and (under
//! `ROOMLER__OVERLAY__BLOCKS_ENABLED`) carves a global P2b `/22` block that
//! is quarantined forever once freed — a resource allocation no GET may
//! perform. A tenant with no network simply lists devices with no overlay
//! columns.

use std::net::Ipv4Addr;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::{Agent, AgentStatus, NodeRef, OsKind, TunnelClient};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler_ai_mod_fleet::agent::{AgentPresence, agent_presence_batch, derive_agent_presence};

/// Fields are declared INLINE, deliberately not `#[serde(flatten)]
/// PaginationParams` — axum's `Query` deserializes via serde_urlencoded,
/// which hands every value to serde as a string, and `flatten` forces the
/// fields behind it through `deserialize_any`, where `u64` then rejects
/// `"1"`. The postmortem lives on `agent_exec.rs`'s `AuditQuery`.
#[derive(Debug, Deserialize)]
pub struct DeviceListQuery {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_per_page")]
    pub per_page: u64,
    /// Case-insensitive substring across name, display_name, tags,
    /// machine_id, os, version, overlay ip and MagicDNS fqdn.
    pub q: Option<String>,
    /// One of: name | kind | os | status | version | overlay_ip | magic_dns
    /// | last_seen_at | created_at. Unknown values 400 rather than silently
    /// sorting by something else. ABSENT = the compound default: online →
    /// stale → offline, then name within each bucket (FR-11).
    pub sort: Option<String>,
    /// `asc` (default) | `desc`.
    pub dir: Option<String>,
    /// Restrict to `agent` | `tunnel_client`.
    pub kind: Option<String>,
}

fn default_page() -> u64 {
    1
}
fn default_per_page() -> u64 {
    25
}
const MAX_PER_PAGE: u64 = 100;

#[derive(Debug, Serialize)]
pub struct DeviceRow {
    /// `agent` | `tunnel_client`.
    pub kind: &'static str,
    pub id: String,
    pub owner_user_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub machine_id: String,
    pub os: OsKind,
    /// agent_version / client_version, unified.
    pub version: String,
    pub status: AgentStatus,
    /// Agents: the three-state hub+Redis+heartbeat derivation. Tunnel
    /// clients: raw stored status mapped online/offline — they have no hub
    /// registration or freshness trail (the known tunnel.rs "T2" gap), so a
    /// `stale` state cannot be derived honestly.
    pub presence: AgentPresence,
    pub is_online: bool,
    pub last_seen_at: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_node_id: Option<String>,
    /// The node's MagicDNS label (bare, no domain).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magic_dns_name: Option<String>,
    /// `<label>.<tenant magic_dns_domain>` — None when the tenant has no
    /// MagicDNS domain configured or the device has no overlay node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magic_dns_fqdn: Option<String>,
    /// FR-40 — the node's overlay (WireGuard) PUBLIC key and its epoch, from
    /// the node row. Shown so an operator can SEE a rotation land instead of
    /// trusting a chip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_public_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_key_epoch: Option<u32>,
    /// FR-51 — enrolled as temporary; the grid badges it so the vanishing is
    /// never a surprise. Serialised only when true (tunnel clients and every
    /// permanent device omit it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
}

/// GET /api/tenant/{tenant_id}/device — membership-gated like every other
/// fleet list (mutations stay behind MANAGE_AGENTS on their own routes).
pub async fn list_devices(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<DeviceListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    // FR-11: NO sort param = the compound default — online first, then
    // name (the sidebar's exact order). An explicit `sort=name` stays a
    // pure name sort; the two are distinguishable only by keeping the
    // Option here instead of the old `unwrap_or("name")`.
    let sort_key = params.sort.as_deref();
    const SORT_KEYS: &[&str] = &[
        "name",
        "kind",
        "os",
        "status",
        "version",
        "overlay_ip",
        "magic_dns",
        "last_seen_at",
        "created_at",
    ];
    if let Some(k) = sort_key
        && !SORT_KEYS.contains(&k)
    {
        return Err(ApiError::BadRequest(format!("Unknown sort key: {k}")));
    }
    let desc = match params.dir.as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(other) => {
            return Err(ApiError::BadRequest(format!("Unknown dir: {other}")));
        }
    };
    let kind_filter = match params.kind.as_deref() {
        None => None,
        Some(k @ ("agent" | "tunnel_client")) => Some(k.to_string()),
        Some(other) => {
            return Err(ApiError::BadRequest(format!("Unknown kind: {other}")));
        }
    };
    let per_page = params.per_page.clamp(1, MAX_PER_PAGE);
    let page = params.page.max(1);

    // ── Fetch + join ────────────────────────────────────────────
    let agents = if kind_filter.as_deref() == Some("tunnel_client") {
        Vec::new()
    } else {
        state.agents.list_all_active_for_tenant(tid).await?
    };
    let clients = if kind_filter.as_deref() == Some("agent") {
        Vec::new()
    } else {
        state
            .network()
            .tunnel_clients
            .list_all_active_for_tenant(tid)
            .await?
    };

    let (nodes, dns_domain) = match state
        .network()
        .overlay_networks
        .find_for_tenant(tid)
        .await?
    {
        Some(net) => {
            let nodes = match net.id {
                Some(nid) => {
                    state
                        .network()
                        .overlay_nodes
                        .list_active_in_network(tid, nid)
                        .await?
                }
                None => Vec::new(),
            };
            let domain = state
                .tenants
                .base
                .find_by_id(tid)
                .await
                .ok()
                .and_then(|t| t.settings.magic_dns_domain);
            (nodes, domain)
        }
        None => (Vec::new(), None),
    };
    let mut node_by_agent = std::collections::HashMap::new();
    let mut node_by_client = std::collections::HashMap::new();
    for n in &nodes {
        match &n.node_ref {
            NodeRef::Agent { agent_id } => {
                node_by_agent.insert(*agent_id, n);
            }
            NodeRef::TunnelClient { tunnel_client_id } => {
                node_by_client.insert(*tunnel_client_id, n);
            }
        }
    }

    let fresh = agent_presence_batch(state.fleet(), &agents).await;

    let mut rows: Vec<DeviceRow> = Vec::with_capacity(agents.len() + clients.len());
    for a in agents {
        let redis_fresh = a.id.map(|i| fresh.contains(&i)).unwrap_or(false);
        let (presence, is_online) = derive_agent_presence(state.fleet(), &a, redis_fresh);
        let node = a.id.and_then(|i| node_by_agent.get(&i).copied());
        rows.push(agent_row(
            a,
            presence,
            is_online,
            node,
            dns_domain.as_deref(),
        ));
    }
    for c in clients {
        let node = c.id.and_then(|i| node_by_client.get(&i).copied());
        rows.push(client_row(c, node, dns_domain.as_deref()));
    }

    // ── Filter ──────────────────────────────────────────────────
    if let Some(kind) = &kind_filter {
        rows.retain(|r| r.kind == kind);
    }
    if let Some(q) = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let needle = q.to_lowercase();
        rows.retain(|r| row_matches(r, &needle));
    }

    // ── Sort (stable; id tiebreak keeps pages disjoint) ─────────
    rows.sort_by(|a, b| {
        let ord = match sort_key {
            // FR-11 default: online → stale → offline, name within each
            // bucket. `dir` still applies to an explicit-sort request only;
            // the default ignores `desc` (there is no param to flip it).
            None => presence_rank(&a.presence)
                .cmp(&presence_rank(&b.presence))
                .then_with(|| effective_name(a).cmp(&effective_name(b))),
            Some(k) => {
                let ord = cmp_rows(a, b, k);
                if desc { ord.reverse() } else { ord }
            }
        };
        ord.then_with(|| a.id.cmp(&b.id))
    });

    // ── Slice + envelope ────────────────────────────────────────
    let total = rows.len() as u64;
    let total_pages = total.div_ceil(per_page).max(1);
    let start = ((page - 1) * per_page) as usize;
    let items: Vec<DeviceRow> = if start >= rows.len() {
        Vec::new()
    } else {
        rows.into_iter()
            .skip(start)
            .take(per_page as usize)
            .collect()
    };

    Ok(Json(serde_json::json!({
        "items": items,
        "total": total,
        "page": page,
        "per_page": per_page,
        "total_pages": total_pages,
    })))
}

fn agent_row(
    a: Agent,
    presence: AgentPresence,
    is_online: bool,
    node: Option<&roomler_ai_remote_control::models::OverlayNode>,
    dns_domain: Option<&str>,
) -> DeviceRow {
    let (overlay_ip, overlay_node_id, magic_dns_name, magic_dns_fqdn) = node_bits(node, dns_domain);
    DeviceRow {
        kind: "agent",
        id: a.id.map(|i| i.to_hex()).unwrap_or_default(),
        owner_user_id: a.owner_user_id.to_hex(),
        name: a.name,
        display_name: a.display_name,
        tags: a.tags,
        machine_id: a.machine_id,
        os: a.os,
        version: a.agent_version,
        status: a.status,
        presence,
        is_online,
        last_seen_at: a.last_seen_at.try_to_rfc3339_string().unwrap_or_default(),
        created_at: a.created_at.try_to_rfc3339_string().unwrap_or_default(),
        overlay_ip,
        overlay_node_id,
        magic_dns_name,
        magic_dns_fqdn,
        overlay_public_key: node.map(|n| n.wg_public_key.clone()),
        overlay_key_epoch: node.map(|n| n.key_epoch),
        ephemeral: a.ephemeral,
    }
}

fn client_row(
    c: TunnelClient,
    node: Option<&roomler_ai_remote_control::models::OverlayNode>,
    dns_domain: Option<&str>,
) -> DeviceRow {
    let (overlay_ip, overlay_node_id, magic_dns_name, magic_dns_fqdn) = node_bits(node, dns_domain);
    let online = matches!(c.status, AgentStatus::Online);
    DeviceRow {
        kind: "tunnel_client",
        id: c.id.map(|i| i.to_hex()).unwrap_or_default(),
        owner_user_id: c.owner_user_id.to_hex(),
        name: c.name,
        display_name: c.display_name,
        tags: c.tags,
        machine_id: c.machine_id,
        os: c.os,
        version: c.client_version,
        status: c.status,
        presence: if online {
            AgentPresence::Online
        } else {
            AgentPresence::Offline
        },
        is_online: online,
        last_seen_at: c.last_seen_at.try_to_rfc3339_string().unwrap_or_default(),
        created_at: c.created_at.try_to_rfc3339_string().unwrap_or_default(),
        overlay_ip,
        overlay_node_id,
        magic_dns_name,
        magic_dns_fqdn,
        overlay_public_key: node.map(|n| n.wg_public_key.clone()),
        overlay_key_epoch: node.map(|n| n.key_epoch),
        // Tunnel clients have no ephemeral kind (FR-51 P5 decides theirs).
        ephemeral: false,
    }
}

type NodeBits = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn node_bits(
    node: Option<&roomler_ai_remote_control::models::OverlayNode>,
    dns_domain: Option<&str>,
) -> NodeBits {
    let Some(n) = node else {
        return (None, None, None, None);
    };
    let label = (!n.name.is_empty()).then(|| n.name.clone());
    let fqdn = match (&label, dns_domain) {
        (Some(l), Some(d)) => Some(format!("{l}.{d}")),
        _ => None,
    };
    (
        Some(n.overlay_ip.clone()),
        n.id.map(|i| i.to_hex()),
        label,
        fqdn,
    )
}

/// The label the grid actually shows — display_name over name.
fn effective_name(r: &DeviceRow) -> String {
    r.display_name.as_deref().unwrap_or(&r.name).to_lowercase()
}

fn row_matches(r: &DeviceRow, needle: &str) -> bool {
    let os = format!("{:?}", r.os).to_lowercase();
    r.name.to_lowercase().contains(needle)
        || r.display_name
            .as_deref()
            .is_some_and(|d| d.to_lowercase().contains(needle))
        || r.tags.iter().any(|t| t.to_lowercase().contains(needle))
        || r.machine_id.to_lowercase().contains(needle)
        || os.contains(needle)
        || r.version.to_lowercase().contains(needle)
        || r.overlay_ip
            .as_deref()
            .is_some_and(|ip| ip.contains(needle))
        || r.magic_dns_fqdn
            .as_deref()
            .is_some_and(|f| f.to_lowercase().contains(needle))
        || r.magic_dns_name
            .as_deref()
            .is_some_and(|f| f.to_lowercase().contains(needle))
}

/// `None` sorts LAST ascending — a grid sorted by overlay ip should lead
/// with the devices that HAVE one (plain `Option` ordering puts None first).
fn none_last<T: Ord>(v: Option<T>) -> (bool, Option<T>) {
    (v.is_none(), v)
}

fn presence_rank(p: &AgentPresence) -> u8 {
    match p {
        AgentPresence::Online => 0,
        AgentPresence::Stale => 1,
        AgentPresence::Offline => 2,
    }
}

fn cmp_rows(a: &DeviceRow, b: &DeviceRow, key: &str) -> std::cmp::Ordering {
    match key {
        "kind" => a.kind.cmp(b.kind),
        "os" => format!("{:?}", a.os).cmp(&format!("{:?}", b.os)),
        "status" => presence_rank(&a.presence).cmp(&presence_rank(&b.presence)),
        "version" => a.version.cmp(&b.version),
        "overlay_ip" => none_last(
            a.overlay_ip
                .as_deref()
                .and_then(|s| s.parse::<Ipv4Addr>().ok()),
        )
        .cmp(&none_last(
            b.overlay_ip
                .as_deref()
                .and_then(|s| s.parse::<Ipv4Addr>().ok()),
        )),
        "magic_dns" => {
            none_last(a.magic_dns_fqdn.as_deref()).cmp(&none_last(b.magic_dns_fqdn.as_deref()))
        }
        "last_seen_at" => a.last_seen_at.cmp(&b.last_seen_at),
        "created_at" => a.created_at.cmp(&b.created_at),
        // Default + "name".
        _ => effective_name(a).cmp(&effective_name(b)),
    }
}
