//! Multi-org P2b — tenant-block addressing: status + the renumber migration.
//!
//! Every tenant shares `100.64.0.0/10` today, each network's host cursor
//! seeded at 1. Tenant A's `100.64.0.7` and tenant B's `100.64.0.7` are
//! therefore the SAME address — fine while a daemon carries exactly one org,
//! fatal the moment one host is enrolled in two (one interface, one routing
//! table, two claimants for the same /32).
//!
//! Blocks carve each tenant a disjoint slice out of the `/10`. New networks
//! get one at creation (gated by `overlay.blocks_enabled`); existing ones move
//! via the renumber endpoint here, which is deliberately explicit and
//! staged-per-tenant because it must:
//!
//! 1. refuse to run over a fleet below the P2a forward-compat floor (an older
//!    daemon purges its OWN on-link route at boot — host-wide mesh blackhole),
//! 2. rewrite every live node's address in one pass, and
//! 3. CYCLE every agent's WS, because a node's `self_ip` is bound once when
//!    its overlay session establishes.
//!
//! `dry_run` (the default) runs 1 and 2's planner and returns the full
//! before/after mapping without writing anything.

use axum::{
    Json,
    extract::{Path, State},
};
use bson::{doc, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::models::{
    NodeRef, OVERLAY_BLOCK_MAX_PREFIX, OVERLAY_BLOCK_MIN_PREFIX, OverlayNetwork, OverlayNode,
    cidr_max_host, overlay_host, overlay_ip,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

use crate::{
    error::ApiError, extractors::auth::AuthUser, routes::remote_control::require_permission,
    state::AppState,
};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct BlockStatusResponse {
    /// The range the tenant's overlay currently leases from.
    pub cidr: String,
    /// `true` while the tenant is still on the shared legacy `/10`.
    pub legacy: bool,
    /// Highest host ordinal the current range can lease.
    pub capacity: u32,
    /// Live nodes in the network.
    pub nodes: u32,
    /// IPAM cursor — how far the tenant has walked into its range.
    pub next_host: u32,
    /// Registry rows for this tenant, newest block first (the assigned one
    /// plus its quarantined predecessors).
    pub blocks: Vec<BlockRow>,
    /// Are new networks being carved on this deployment?
    pub carving_enabled: bool,
    /// Minimum device version a renumber requires.
    pub version_floor: String,
    /// Devices below the floor right now — a renumber refuses while non-empty.
    pub below_floor: Vec<DeviceVersion>,
}

#[derive(Debug, Serialize)]
pub struct BlockRow {
    pub cidr: String,
    pub slot: u32,
    pub slots: u32,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_reason: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DeviceVersion {
    pub name: String,
    pub kind: &'static str,
    /// The reported version, or `""` when the device has never checked in
    /// with one (treated as below the floor — fail closed).
    pub version: String,
    pub online: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct RenumberRequest {
    /// Plan only. Defaults to TRUE: a renumber is disruptive, so the
    /// destructive form must be asked for explicitly.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Block width to carve, `/16` … `/22`. Defaults to the deployment's
    /// `overlay.block_prefix`.
    #[serde(default)]
    pub prefix: Option<u8>,
    /// Cycle every agent's WS after the write so the fleet re-binds
    /// immediately. Default true; `false` leaves nodes on their old
    /// addresses until they reconnect on their own.
    #[serde(default = "default_true")]
    pub cycle: bool,
    /// Run even though devices sit below the version floor. Their mesh will
    /// black-hole at their next daemon start until they update.
    #[serde(default)]
    pub force: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct RenumberResponse {
    pub dry_run: bool,
    pub applied: bool,
    pub old_cidr: String,
    pub new_cidr: String,
    pub next_host: u32,
    /// Per-node old → new mapping.
    pub moves: Vec<NodeMove>,
    /// Nodes whose current address doesn't parse under the OLD cidr (leased
    /// under a since-changed range). They are re-based like the rest — listed
    /// separately because their ordinal could not be preserved.
    pub reseated: Vec<String>,
    /// Devices below the version floor (empty unless `force`).
    pub below_floor: Vec<DeviceVersion>,
    /// Agents whose WS this pod published a cycle for.
    pub cycled: Vec<String>,
    /// Nodes that must reconnect on their own for the new address to bind
    /// (tunnel clients — the server has no cycle primitive for them).
    pub reconnect_required: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct NodeMove {
    pub node_id: String,
    pub name: String,
    pub kind: &'static str,
    pub old_ip: String,
    pub new_ip: String,
    /// The ordinal was carried over rather than compacted.
    pub ordinal_preserved: bool,
}

// ---------------------------------------------------------------------------
// Pure planning (unit-tested below; no DB, no state)
// ---------------------------------------------------------------------------

/// One node's identity as far as the planner cares.
#[derive(Debug, Clone)]
pub struct PlanNode {
    pub id: ObjectId,
    pub name: String,
    pub kind: &'static str,
    pub ip: String,
}

#[derive(Debug)]
pub struct RenumberPlan {
    pub moves: Vec<NodeMove>,
    pub reseated: Vec<String>,
    pub next_host: u32,
}

/// Map every live node from `old_cidr` into `new_cidr`.
///
/// Ordinals are PRESERVED where they fit (`100.64.0.7` → `100.65.0.7`), which
/// keeps hand-written notes, `known_hosts` entries and dashboards readable
/// across the migration. An ordinal that doesn't fit the new block — or that
/// can't be recovered from the address at all — is COMPACTED onto the lowest
/// free one. Preservation runs as a first pass over the whole set so a
/// compacted node can never steal an ordinal a later node would have kept.
pub fn plan_renumber(
    nodes: &[PlanNode],
    old_cidr: &str,
    new_cidr: &str,
) -> Result<RenumberPlan, String> {
    let max_host = cidr_max_host(new_cidr)
        .ok_or_else(|| format!("{new_cidr} cannot lease any host addresses"))?;
    if nodes.len() as u64 > max_host as u64 {
        return Err(format!(
            "{} live nodes do not fit in {new_cidr} (capacity {max_host}); \
             renumber with a wider prefix",
            nodes.len()
        ));
    }

    // Pass 1 — who keeps their ordinal. Sorted by id so the plan is
    // deterministic (a dry-run must predict what the apply will do).
    let mut ordered: Vec<&PlanNode> = nodes.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));

    let mut taken: HashSet<u32> = HashSet::new();
    let mut keep: HashMap<ObjectId, u32> = HashMap::new();
    for n in &ordered {
        if let Some(h) = overlay_host(old_cidr, &n.ip)
            && h >= 1
            && h <= max_host
            && taken.insert(h)
        {
            keep.insert(n.id, h);
        }
    }

    // Pass 2 — everyone else takes the lowest free ordinal.
    let mut moves = Vec::with_capacity(ordered.len());
    let mut reseated = Vec::new();
    let mut cursor = 1u32;
    let mut highest = 0u32;
    for n in ordered {
        let (host, preserved) = match keep.get(&n.id) {
            Some(h) => (*h, true),
            None => {
                while cursor <= max_host && !taken.insert(cursor) {
                    cursor += 1;
                }
                if cursor > max_host {
                    return Err(format!("{new_cidr} exhausted while compacting"));
                }
                reseated.push(n.name.clone());
                (cursor, false)
            }
        };
        highest = highest.max(host);
        let new_ip = overlay_ip(new_cidr, host)
            .ok_or_else(|| format!("host {host} does not render inside {new_cidr}"))?;
        moves.push(NodeMove {
            node_id: n.id.to_hex(),
            name: n.name.clone(),
            kind: n.kind,
            old_ip: n.ip.clone(),
            new_ip,
            ordinal_preserved: preserved,
        });
    }

    Ok(RenumberPlan {
        moves,
        reseated,
        // The cursor resumes above the highest ordinal actually placed — the
        // gaps below it are recovered by the free pool, not by the cursor.
        next_host: highest.saturating_add(1).max(1),
    })
}

/// Parse a `MAJOR.MINOR.PATCH[-rc.N]` version into a comparable tuple.
/// A release (no `-rc`) sorts ABOVE every rc of the same version, which is
/// the usual semver pre-release rule.
fn parse_version(v: &str) -> Option<(u32, u32, u32, u32)> {
    let v = v.trim();
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    let rc = match pre {
        // `rc.238` / `rc238` — the fleet has only ever used the dotted form,
        // but tolerate both rather than mis-sorting a stray tag.
        Some(p) => {
            let digits: String = p.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                0
            } else {
                digits.parse().unwrap_or(0)
            }
        }
        // No pre-release ⇒ final, sorts above every rc.
        None => u32::MAX,
    };
    Some((major, minor, patch, rc))
}

/// Is `version` at or above `floor`?
///
/// An unparseable or EMPTY version fails closed (`false`): a device that has
/// never reported one is exactly the device most likely to be running
/// something ancient, and the cost of guessing wrong is a blackholed mesh.
pub fn version_meets_floor(version: &str, floor: &str) -> bool {
    let Some(f) = parse_version(floor) else {
        // A malformed FLOOR must not block every migration — treat the gate
        // as unconfigured rather than universally failing.
        return true;
    };
    parse_version(version).is_some_and(|v| v >= f)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/tenant/{tenant_id}/overlay-block — the tenant's address-block
/// posture: current range, usage, registry trail and fleet readiness.
pub async fn get_block(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<BlockStatusResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    let network = state.overlay_networks.get_or_create(tid).await?;
    let network_id = network
        .id
        .ok_or_else(|| ApiError::Internal("overlay network missing _id".into()))?;
    let nodes = state
        .overlay_nodes
        .list_active_in_network(tid, network_id)
        .await?;
    let versions = resolve_versions(&state, tid, &nodes).await;
    let floor = state.settings.overlay.block_version_floor.clone();
    let below_floor = versions
        .into_iter()
        .filter(|d| !version_meets_floor(&d.version, &floor))
        .collect();

    let blocks = state
        .overlay_networks
        .blocks()
        .list_for_tenant(tid)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|b| BlockRow {
            cidr: b.cidr,
            slot: b.slot,
            slots: b.slots,
            state: match b.state {
                roomler_ai_remote_control::models::OverlayBlockState::Assigned => "assigned",
                roomler_ai_remote_control::models::OverlayBlockState::Quarantined => "quarantined",
            }
            .to_string(),
            released_reason: b.released_reason,
        })
        .collect();

    Ok(Json(BlockStatusResponse {
        legacy: network.cidr == OverlayNetwork::DEFAULT_CIDR,
        capacity: network.max_host(),
        nodes: nodes.len() as u32,
        next_host: network.next_host,
        cidr: network.cidr,
        blocks,
        carving_enabled: state.overlay_networks.block_prefix().is_some(),
        version_floor: floor,
        below_floor,
    }))
}

/// POST /api/tenant/{tenant_id}/overlay-block/renumber — plan (default) or
/// perform the tenant's migration onto its own block.
pub async fn renumber(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<RenumberRequest>,
) -> Result<Json<RenumberResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    let prefix = body.prefix.unwrap_or(state.settings.overlay.block_prefix);
    if !(OVERLAY_BLOCK_MIN_PREFIX..=OVERLAY_BLOCK_MAX_PREFIX).contains(&prefix) {
        return Err(ApiError::BadRequest(format!(
            "block prefix must be between /{OVERLAY_BLOCK_MIN_PREFIX} and \
             /{OVERLAY_BLOCK_MAX_PREFIX} (got /{prefix})"
        )));
    }

    let network = state.overlay_networks.get_or_create(tid).await?;
    let network_id = network
        .id
        .ok_or_else(|| ApiError::Internal("overlay network missing _id".into()))?;
    let nodes = state
        .overlay_nodes
        .list_active_in_network(tid, network_id)
        .await?;

    // --- Gate 1: the fleet version floor -----------------------------------
    let floor = state.settings.overlay.block_version_floor.clone();
    let below_floor: Vec<DeviceVersion> = resolve_versions(&state, tid, &nodes)
        .await
        .into_iter()
        .filter(|d| !version_meets_floor(&d.version, &floor))
        .collect();
    if !below_floor.is_empty() && !body.force && !body.dry_run {
        return Err(ApiError::BadRequest(format!(
            "{} device(s) are below the {floor} floor for tenant-block addressing \
             ({}). Their daemon purges its own on-link route at boot, which \
             black-holes that host's mesh. Update them, or re-send with \
             force=true to accept the breakage.",
            below_floor.len(),
            below_floor
                .iter()
                .map(|d| format!(
                    "{} @ {}",
                    d.name,
                    if d.version.is_empty() {
                        "unknown"
                    } else {
                        &d.version
                    }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // --- Plan --------------------------------------------------------------
    // Dry runs must not consume a block, so the plan is rendered against the
    // range the allocator WOULD hand out. The apply path re-allocates for
    // real — the two can differ if another tenant migrates in between, which
    // is why the response always echoes the cidr it actually used.
    let plan_nodes: Vec<PlanNode> = nodes
        .iter()
        .filter_map(|n| {
            Some(PlanNode {
                id: n.id?,
                name: n.name.clone(),
                kind: node_kind(n),
                ip: n.overlay_ip.clone(),
            })
        })
        .collect();

    let mut warnings = Vec::new();
    if body.dry_run {
        let preview = state
            .overlay_networks
            .blocks()
            .preview_next_cidr(prefix)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        let plan =
            plan_renumber(&plan_nodes, &network.cidr, &preview).map_err(ApiError::BadRequest)?;
        if !below_floor.is_empty() {
            warnings.push(format!(
                "{} device(s) below the {floor} floor — the apply will refuse \
                 unless force=true",
                below_floor.len()
            ));
        }
        warnings.push(
            "preview only: the block is allocated by the apply, so the actual \
             range can differ if another tenant migrates first"
                .to_string(),
        );
        return Ok(Json(RenumberResponse {
            dry_run: true,
            applied: false,
            old_cidr: network.cidr,
            new_cidr: preview,
            next_host: plan.next_host,
            moves: plan.moves,
            reseated: plan.reseated,
            below_floor,
            cycled: Vec::new(),
            reconnect_required: Vec::new(),
            warnings,
        }));
    }

    // --- Apply -------------------------------------------------------------
    // Quarantine BEFORE allocating: the registry's partial-unique index
    // allows one assigned block per network, and a block that is retired but
    // whose replacement failed is harmless (its slots simply stay out of
    // circulation) — whereas the reverse order cannot even be attempted.
    let old_block = state
        .overlay_networks
        .blocks()
        .find_assigned_for_network(network_id)
        .await?;
    if let Some(b) = &old_block
        && let Some(bid) = b.id
    {
        state
            .overlay_networks
            .blocks()
            .quarantine(bid, "renumber")
            .await?;
    }
    let block = state
        .overlay_networks
        .blocks()
        .allocate(tid, network_id, prefix)
        .await?;
    let plan =
        plan_renumber(&plan_nodes, &network.cidr, &block.cidr).map_err(ApiError::BadRequest)?;

    // Node rows first, then the network. A crash between them leaves nodes on
    // the new addresses with the network still describing the old range: the
    // next join re-reads both and the operator can simply re-run the
    // renumber, whereas the reverse order would hand a joiner an address
    // inside a range whose live rows still sit outside it.
    for m in &plan.moves {
        let Ok(nid) = ObjectId::parse_str(&m.node_id) else {
            continue;
        };
        if let Err(e) = state
            .overlay_nodes
            .set_overlay_ip(nid, network_id, &m.new_ip)
            .await
        {
            warn!(%tid, node = %m.node_id, %e, "renumber: node address write failed");
            warnings.push(format!("{}: address write failed ({e})", m.name));
        }
    }
    state
        .overlay_networks
        .apply_renumber(network_id, &block.cidr, plan.next_host)
        .await?;
    info!(
        tenant = %tid, old = %network.cidr, new = %block.cidr, nodes = plan.moves.len(),
        "overlay renumber applied"
    );

    // --- Cycle -------------------------------------------------------------
    // A node's self_ip is bound once, when its overlay session establishes,
    // so the fleet keeps its OLD addresses until each socket re-establishes.
    let mut cycled = Vec::new();
    let mut reconnect_required = Vec::new();
    for n in &nodes {
        match n.node_ref {
            NodeRef::Agent { agent_id } => {
                if body.cycle {
                    // Local first (the common single-pod case), then the ctrl
                    // lane for whichever pod actually holds the socket.
                    state.rc_hub.cycle_agent_ws(agent_id);
                    crate::ws::remote_control::publish_rc_ctrl(
                        &state,
                        "overlay_cycle",
                        serde_json::json!({ "agent_id": agent_id.to_hex() }),
                    )
                    .await;
                    cycled.push(n.name.clone());
                }
            }
            // The server has no cycle primitive for tunnel-client sockets
            // (they are not hub-registered), so those nodes pick the new
            // address up on their next reconnect.
            NodeRef::TunnelClient { .. } => reconnect_required.push(n.name.clone()),
        }
    }
    if !body.cycle {
        warnings.push(
            "cycle=false: nodes keep their OLD self_ip until they reconnect on \
             their own — peers will already have installed the new addresses"
                .to_string(),
        );
    }
    if !reconnect_required.is_empty() {
        warnings.push(format!(
            "{} tunnel-client node(s) must reconnect for the new address to bind",
            reconnect_required.len()
        ));
    }
    if !below_floor.is_empty() {
        warnings.push(format!(
            "forced over {} device(s) below the {floor} floor",
            below_floor.len()
        ));
    }

    Ok(Json(RenumberResponse {
        dry_run: false,
        applied: true,
        old_cidr: network.cidr,
        new_cidr: block.cidr,
        next_host: plan.next_host,
        moves: plan.moves,
        reseated: plan.reseated,
        below_floor,
        cycled,
        reconnect_required,
        warnings,
    }))
}

fn node_kind(n: &OverlayNode) -> &'static str {
    match n.node_ref {
        NodeRef::Agent { .. } => "agent",
        NodeRef::TunnelClient { .. } => "tunnel_client",
    }
}

/// Resolve each node's backing device version in two queries. A node whose
/// device row is gone is skipped: it can't rejoin, so it can't be broken by
/// the migration.
async fn resolve_versions(
    state: &AppState,
    tenant_id: ObjectId,
    nodes: &[OverlayNode],
) -> Vec<DeviceVersion> {
    let mut agent_ids = Vec::new();
    let mut client_ids = Vec::new();
    for n in nodes {
        match n.node_ref {
            NodeRef::Agent { agent_id } => agent_ids.push(agent_id),
            NodeRef::TunnelClient { tunnel_client_id } => client_ids.push(tunnel_client_id),
        }
    }
    let mut out = Vec::new();
    if !agent_ids.is_empty()
        && let Ok(rows) = state
            .agents
            .base
            .find_many(
                doc! { "_id": { "$in": &agent_ids }, "tenant_id": tenant_id, "deleted_at": null },
                None,
            )
            .await
    {
        out.extend(rows.into_iter().map(|a| DeviceVersion {
            name: a.name,
            kind: "agent",
            version: a.agent_version,
            online: matches!(
                a.status,
                roomler_ai_remote_control::models::AgentStatus::Online
            ),
        }));
    }
    if !client_ids.is_empty()
        && let Ok(rows) = state
            .tunnel_clients
            .base
            .find_many(
                doc! { "_id": { "$in": &client_ids }, "tenant_id": tenant_id, "deleted_at": null },
                None,
            )
            .await
    {
        out.extend(rows.into_iter().map(|c| DeviceVersion {
            name: c.name,
            kind: "tunnel_client",
            version: c.client_version,
            online: matches!(
                c.status,
                roomler_ai_remote_control::models::AgentStatus::Online
            ),
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u8, ip: &str) -> PlanNode {
        let mut raw = [0u8; 12];
        raw[11] = id;
        PlanNode {
            id: ObjectId::from_bytes(raw),
            name: format!("node-{id}"),
            kind: "agent",
            ip: ip.to_string(),
        }
    }

    #[test]
    fn ordinals_survive_the_move_when_they_fit() {
        let nodes = vec![
            node(1, "100.64.0.1"),
            node(2, "100.64.0.7"),
            node(3, "100.64.1.0"), // ordinal 256
        ];
        let plan = plan_renumber(&nodes, "100.64.0.0/10", "100.65.0.0/22").expect("plans");
        let ips: Vec<&str> = plan.moves.iter().map(|m| m.new_ip.as_str()).collect();
        assert_eq!(ips, vec!["100.65.0.1", "100.65.0.7", "100.65.1.0"]);
        assert!(plan.moves.iter().all(|m| m.ordinal_preserved));
        assert!(plan.reseated.is_empty());
        // The cursor resumes above the HIGHEST placed ordinal, not the count.
        assert_eq!(plan.next_host, 257);
    }

    #[test]
    fn ordinals_past_the_block_are_compacted() {
        let nodes = vec![
            node(1, "100.64.0.5"),  // fits
            node(2, "100.64.10.0"), // ordinal 2560 — past a /22
            node(3, "100.64.20.0"), // ordinal 5120 — past a /22
        ];
        let plan = plan_renumber(&nodes, "100.64.0.0/10", "100.65.0.0/22").expect("plans");
        assert_eq!(plan.moves[0].new_ip, "100.65.0.5");
        assert!(plan.moves[0].ordinal_preserved);
        // Compacted onto the lowest free ordinals — and NOT onto 5, which the
        // first node kept.
        assert_eq!(plan.moves[1].new_ip, "100.65.0.1");
        assert_eq!(plan.moves[2].new_ip, "100.65.0.2");
        assert!(!plan.moves[1].ordinal_preserved);
        assert_eq!(plan.reseated, vec!["node-2", "node-3"]);
    }

    /// The ordering trap: a node that must be compacted must never be handed
    /// an ordinal that a LATER node in the list is entitled to keep.
    #[test]
    fn compaction_never_steals_a_preserved_ordinal() {
        let nodes = vec![
            node(1, "100.64.99.0"), // compacted — sorts FIRST by id
            node(2, "100.64.0.1"),  // keeps ordinal 1
            node(3, "100.64.0.2"),  // keeps ordinal 2
        ];
        let plan = plan_renumber(&nodes, "100.64.0.0/10", "100.65.0.0/22").expect("plans");
        assert_eq!(
            plan.moves[0].new_ip, "100.65.0.3",
            "took the first FREE one"
        );
        assert_eq!(plan.moves[1].new_ip, "100.65.0.1");
        assert_eq!(plan.moves[2].new_ip, "100.65.0.2");
    }

    #[test]
    fn an_unparseable_address_is_reseated_not_dropped() {
        let nodes = vec![node(1, "10.9.9.9"), node(2, "100.64.0.4")];
        let plan = plan_renumber(&nodes, "100.64.0.0/10", "100.65.0.0/22").expect("plans");
        assert_eq!(plan.moves.len(), 2);
        assert_eq!(plan.moves[0].new_ip, "100.65.0.1");
        assert!(!plan.moves[0].ordinal_preserved);
        assert_eq!(plan.moves[1].new_ip, "100.65.0.4");
        assert_eq!(plan.reseated, vec!["node-1"]);
    }

    #[test]
    fn a_block_that_cannot_hold_the_fleet_is_refused() {
        // /22 leases 1022; ask it to hold 1023.
        let nodes: Vec<PlanNode> = (0..1023)
            .map(|i| PlanNode {
                id: ObjectId::from_bytes([
                    (i / 256) as u8,
                    (i % 256) as u8,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ]),
                name: format!("n{i}"),
                kind: "agent",
                ip: format!("100.64.{}.{}", i / 256, i % 256),
            })
            .collect();
        let err = plan_renumber(&nodes, "100.64.0.0/10", "100.65.0.0/22").unwrap_err();
        assert!(err.contains("do not fit"), "{err}");
        // …and a /20 takes them.
        assert!(plan_renumber(&nodes, "100.64.0.0/10", "100.65.0.0/20").is_ok());
    }

    #[test]
    fn plans_are_deterministic() {
        let nodes = vec![
            node(3, "100.64.7.7"),
            node(1, "100.64.0.1"),
            node(2, "10.0.0.1"),
        ];
        let a = plan_renumber(&nodes, "100.64.0.0/10", "100.65.0.0/22").expect("plans");
        let mut shuffled = nodes.clone();
        shuffled.reverse();
        let b = plan_renumber(&shuffled, "100.64.0.0/10", "100.65.0.0/22").expect("plans");
        assert_eq!(a.moves, b.moves, "input order must not change the plan");
    }

    #[test]
    fn version_floor_compares_rc_numbers_numerically() {
        let floor = "0.3.0-rc.301";
        assert!(version_meets_floor("0.3.0-rc.301", floor));
        assert!(version_meets_floor("0.3.0-rc.306", floor));
        // The trap a string compare falls into: "rc.99" > "rc.301" lexically.
        assert!(!version_meets_floor("0.3.0-rc.99", floor));
        assert!(!version_meets_floor("0.3.0-rc.300", floor));
        assert!(!version_meets_floor("0.2.9-rc.999", floor));
        assert!(version_meets_floor("0.4.0-rc.1", floor));
        // A final release outranks every rc of the same version.
        assert!(version_meets_floor("0.3.0", floor));
    }

    #[test]
    fn an_unknown_version_fails_closed() {
        let floor = "0.3.0-rc.301";
        assert!(!version_meets_floor("", floor));
        assert!(!version_meets_floor("dev", floor));
        assert!(!version_meets_floor("0.3", floor));
        // …but a broken FLOOR must not block every migration.
        assert!(version_meets_floor("0.1.0", "not-a-version"));
    }
}
