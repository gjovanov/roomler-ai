// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-19 P3c — the org-relay MINT: the server's decision to route one pair of
//! nodes through a third, tenant-owned node (`docs/fr/FR-19-peer-relays.md`
//! §1–§7).
//!
//! The server decides and pushes; it never carries a byte. A mint is three
//! frames — `rc:overlay.relay_serve` to the relay with both members' secrets,
//! `rc:overlay.relay_session` to each member with its own — and then the
//! server's involvement ends: the members bind at the relay over a path the
//! server is not on. That is why every refusal is enumerated
//! ([`PeerRelayDenyReason`]) and written to `peer_relay_audit`: the asker never
//! gets a synchronous answer, so the row IS the answer.
//!
//! ## Gates, in order
//!
//! 1. `peer_relay_mode` — `off` ⇒ nothing: no rows, no reads past the cached
//!    mode; `warn` ⇒ decide and audit exactly as `on` would, push nothing;
//!    `on` ⇒ mint.
//! 2. Both ends advertised `supports_org_relay` and joined from their PRIMARY
//!    org. An absent flag is treated as "not primary": fail closed.
//! 3. The overlay ACL grants EACH member the relay node — an affirmative
//!    capability evaluated regardless of `acl_mode` (§4, decision "taken"),
//!    read through [`try_load_acl`] so an unreadable policy set is a refusal
//!    with its own reason and never a grant.
//! 4. A relay candidate: approved (`peer_relay_policy.serve`), advertising
//!    `relay-server` on its last hello, online on this pod, joined from its
//!    primary org, with at least one public endpoint, under its session cap.
//! 5. The per-(requester, relay) mint ceiling — after the identity gates, so
//!    a refusal is attributable.
//!
//! ## What is pod-local, and why that is right
//!
//! Sessions, VNI cursors, the generation clock, join extras and probe reports
//! live in this process. Tenant affinity puts a tenant's nodes — requester,
//! peer, relay — on ONE pod (`CLAUDE.md`, S6), so the mint and every party's
//! WS share a process, exactly as `relay_pair_churn` does. After a pod restart
//! the relay's own table is the truth: its sessions run out their
//! `max_lifetime`, the members re-request, and a fresh mint issues a fresh VNI.
//! Bounding that window is what `MAX_LIFETIME_SECS` is for.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{DateTime, oid::ObjectId};
use dashmap::DashMap;
use rand::RngCore;
use roomler_ai_remote_control::{
    models::{
        NodeRef, OverlayNode, PeerRelayAuditAction, PeerRelayAuditEvent, PeerRelayDenyReason,
        PeerRelayMode, RpcCap, peer_relay_limits as limits,
    },
    signaling::{RelayMemberWire, ServerMsg},
};
use tracing::{debug, info, warn};
use tunnel_core::policy::{OverlayPeerRef, OverlaySource, evaluate_overlay};

use crate::NetworkState;
use crate::overlay::{
    NodeIdentity, PolicyLoad, current_node, pair_key, send_to_node, send_to_node_ref, try_load_acl,
    try_overlay_source_of,
};
use roomler_core::net::is_global_unicast;

/// How long a cached `peer_relay_mode` read stands. The org switch route
/// invalidates on write (same pod, by tenant affinity), so this only bounds
/// the window after an out-of-band edit.
const MODE_CACHE_TTL: Duration = Duration::from_secs(30);
/// Reports per (reporter, relay) kept; older ones fall off.
const PROBES_PER_KEY: usize = 8;

/// What a join tells the mint that the persisted row does not carry.
#[derive(Clone, Copy, Default)]
pub struct JoinExtras {
    /// `Some(true)` = joined from the device's primary org. `None` = the
    /// build did not say; treated as NOT primary everywhere it matters.
    pub org_primary: Option<bool>,
    /// The node's org-relay listening port, when it serves.
    pub relay_port: Option<u16>,
}

#[derive(Clone)]
pub struct SessionParty {
    pub node_id: ObjectId,
    pub node_ref: NodeRef,
}

#[derive(Clone)]
pub struct SessionMember {
    pub party: SessionParty,
    pub wg_public_key: String,
    /// base64 of the 32-byte bind secret. Kept so an idempotent re-request
    /// re-pushes the SAME session instead of minting a second one.
    pub bind_secret: String,
}

#[derive(Clone)]
pub struct OrgRelaySession {
    pub tenant_id: ObjectId,
    pub pair_key: String,
    pub vni: u32,
    pub generation: u64,
    pub relay: SessionParty,
    /// The relay's backing agent — the device an audit row is ABOUT.
    pub relay_agent_id: ObjectId,
    pub members: [SessionMember; 2],
    pub endpoints: Vec<String>,
    pub expires_at: Instant,
}

impl OrgRelaySession {
    fn member_index(&self, node_id: ObjectId) -> Option<usize> {
        self.members.iter().position(|m| m.party.node_id == node_id)
    }
    fn involves(&self, node_id: ObjectId) -> bool {
        self.relay.node_id == node_id || self.member_index(node_id).is_some()
    }
}

#[derive(Clone)]
pub struct ProbeRecord {
    pub endpoint: String,
    pub reachable: bool,
    pub rtt_ms: Option<u32>,
    pub at: Instant,
}

/// One member's measured standing toward one relay, for ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeVerdict {
    /// A fresh report says at least one endpoint answered.
    Reachable(u32),
    /// Every fresh report says no.
    Unreachable,
    /// Never measured, or only stale reports.
    Unknown,
}

#[derive(Default)]
pub struct OrgRelayState {
    sessions: DashMap<String, OrgRelaySession>,
    next_vni: DashMap<ObjectId, u32>,
    /// The server-wide logical clock the reference design calls `LamportID`:
    /// monotonic across every (pair, relay), so it is monotonic per one too.
    lamport: AtomicU64,
    join_extras: DashMap<ObjectId, JoinExtras>,
    probes: DashMap<(ObjectId, ObjectId), Vec<ProbeRecord>>,
    mode_cache: DashMap<ObjectId, (PeerRelayMode, Instant)>,
}

impl OrgRelayState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what a join said beyond the row. Overwrites on every rejoin so a
    /// build that stops advertising a port, or an org that stops being
    /// primary, is not remembered as it was.
    pub fn note_join(&self, node_id: ObjectId, extras: JoinExtras) {
        self.join_extras.insert(node_id, extras);
    }

    pub fn forget_node(&self, node_id: ObjectId) {
        self.join_extras.remove(&node_id);
        self.probes.retain(|(reporter, _), _| *reporter != node_id);
    }

    pub fn join_extras(&self, node_id: ObjectId) -> JoinExtras {
        self.join_extras
            .get(&node_id)
            .map(|e| *e)
            .unwrap_or_default()
    }

    pub fn invalidate_mode(&self, tenant_id: ObjectId) {
        self.mode_cache.remove(&tenant_id);
    }

    /// The live session for a pair, if any. Expired sessions are reaped on
    /// the way out so a stale entry can never be re-pushed.
    pub fn active_session(&self, pair_key: &str) -> Option<OrgRelaySession> {
        let now = Instant::now();
        let s = self.sessions.get(pair_key).map(|e| e.value().clone())?;
        if s.expires_at <= now {
            self.sessions.remove(pair_key);
            return None;
        }
        Some(s)
    }

    pub fn sessions_on_relay(&self, relay_node_id: ObjectId) -> usize {
        let now = Instant::now();
        self.sessions
            .iter()
            .filter(|e| e.relay.node_id == relay_node_id && e.expires_at > now)
            .count()
    }

    /// Next free 24-bit VNI on a relay — globally unique PER RELAY NODE, never
    /// per tenant (§2: the Geneve header has no tenant field, so a shared
    /// node's demux key must be unambiguous across tenants). Skips 0 and the
    /// STUN magic cookie (which the relay never mints, so a STUN packet can
    /// never alias a session), and any VNI a live session on that relay
    /// still holds.
    fn alloc_vni(&self, relay_node_id: ObjectId) -> Option<u32> {
        let in_use: Vec<u32> = self
            .sessions
            .iter()
            .filter(|e| e.relay.node_id == relay_node_id)
            .map(|e| e.vni)
            .collect();
        let mut cursor = self.next_vni.entry(relay_node_id).or_insert(1);
        // At most `MAX_SESSIONS_PER_RELAY` live VNIs plus the two reserved
        // values can stand in the way; bound the walk accordingly.
        for _ in 0..(limits::MAX_SESSIONS_PER_RELAY + 4) {
            let candidate = *cursor;
            *cursor = if candidate >= limits::VNI_MAX {
                1
            } else {
                candidate + 1
            };
            if candidate == 0 || candidate == limits::STUN_COOKIE_VNI {
                continue;
            }
            if in_use.contains(&candidate) {
                continue;
            }
            return Some(candidate);
        }
        None
    }

    /// Accept a reachability report ONLY about a relay + endpoint the server
    /// minted for that node: a device cannot inject claims about arbitrary
    /// targets, and the server never asked it to probe anything else.
    pub fn record_probe(
        &self,
        reporter: ObjectId,
        relay_node_id: ObjectId,
        endpoint: String,
        reachable: bool,
        rtt_ms: Option<u32>,
    ) -> bool {
        let minted_for_it = self.sessions.iter().any(|e| {
            e.relay.node_id == relay_node_id
                && e.member_index(reporter).is_some()
                && e.endpoints.contains(&endpoint)
        });
        if !minted_for_it {
            return false;
        }
        let mut v = self.probes.entry((reporter, relay_node_id)).or_default();
        v.push(ProbeRecord {
            endpoint,
            reachable,
            rtt_ms,
            at: Instant::now(),
        });
        if v.len() > PROBES_PER_KEY {
            let excess = v.len() - PROBES_PER_KEY;
            v.drain(..excess);
        }
        true
    }

    fn probe_verdict(
        &self,
        reporter: ObjectId,
        relay_node_id: ObjectId,
        now: Instant,
    ) -> ProbeVerdict {
        let Some(v) = self.probes.get(&(reporter, relay_node_id)) else {
            return ProbeVerdict::Unknown;
        };
        let ttl = Duration::from_secs(limits::PROBE_TTL_SECS);
        let fresh: Vec<&ProbeRecord> = v
            .iter()
            .filter(|p| now.duration_since(p.at) < ttl)
            .collect();
        if fresh.is_empty() {
            return ProbeVerdict::Unknown;
        }
        let best = fresh
            .iter()
            .filter(|p| p.reachable)
            .map(|p| p.rtt_ms.unwrap_or(0))
            .min();
        match best {
            Some(rtt) => ProbeVerdict::Reachable(rtt),
            None => ProbeVerdict::Unreachable,
        }
    }
}

/// The tenant's peer-relay posture, cached briefly. A read failure is `Off`:
/// the wrong answer here costs one un-minted relay session (the pair stays
/// on TURN/DERP), while the wrong answer in the other direction would mint
/// against a posture nobody set.
pub async fn peer_relay_mode(state: &NetworkState, tenant_id: ObjectId) -> PeerRelayMode {
    let now = Instant::now();
    if let Some(e) = state.org_relay.mode_cache.get(&tenant_id)
        && now.duration_since(e.1) < MODE_CACHE_TTL
    {
        return e.0;
    }
    let mode = match state.overlay_networks.get_or_create(tenant_id).await {
        Ok(n) => n.peer_relay_mode,
        Err(e) => {
            warn!(%tenant_id, %e, "org-relay: network read failed; treating peer_relay_mode as off");
            return PeerRelayMode::Off;
        }
    };
    state.org_relay.mode_cache.insert(tenant_id, (mode, now));
    mode
}

/// A relay chosen for a pair, with everything the frames need.
struct Plan {
    relay: OverlayNode,
    relay_agent_id: ObjectId,
    endpoints: Vec<String>,
}

/// The relay's dialable `ip:port`s: its own public addresses (srflx first —
/// the mapping its NAT actually exposes — then a public NIC, then the relay
/// trickle bucket), each paired with the relay-server port, then the admin's
/// static endpoints. `Err` = a static endpoint failed the SSRF rule, which
/// refuses the mint outright rather than quietly dropping the entry: the
/// admin wrote it, and a mint that silently ignores policy is worse than one
/// that says why it did not happen.
fn relay_endpoints(
    node: &OverlayNode,
    port: u16,
    static_endpoints: &[String],
) -> Result<Vec<String>, ()> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for cand in node
        .srflx_endpoints
        .iter()
        .chain(node.lan_endpoints.iter())
        .chain(node.endpoints.iter())
    {
        if let Ok(sa) = cand.parse::<SocketAddr>()
            && is_global_unicast(&sa.ip())
        {
            push(SocketAddr::new(sa.ip(), port).to_string());
        }
    }
    for s in static_endpoints {
        let sa: SocketAddr = s.parse().map_err(|_| ())?;
        if !is_global_unicast(&sa.ip()) {
            return Err(());
        }
        push(sa.to_string());
    }
    Ok(out)
}

/// Is `s` a public `ip:port` literal? The approval route's validator — a
/// name is refused too, because a name can resolve to something else by the
/// time the mint re-checks it.
pub fn valid_static_endpoint(s: &str) -> bool {
    s.parse::<SocketAddr>()
        .map(|sa| sa.port() != 0 && is_global_unicast(&sa.ip()))
        .unwrap_or(false)
}

/// Gates 2–5 for one request. Pure over what it reads; the caller audits the
/// verdict and, under `on`, mints.
async fn plan_mint(
    state: &NetworkState,
    requester: &OverlayNode,
    peer: &OverlayNode,
) -> Result<Plan, PeerRelayDenyReason> {
    use PeerRelayDenyReason as D;
    let tenant_id = requester.tenant_id;
    if peer.tenant_id != tenant_id {
        return Err(D::CrossTenant);
    }
    if !requester.supports_org_relay {
        return Err(D::RequesterUnsupported);
    }
    if !peer.supports_org_relay {
        return Err(D::PeerUnsupported);
    }
    let (Some(req_id), Some(peer_id)) = (requester.id, peer.id) else {
        return Err(D::RequesterUnsupported);
    };
    if state.org_relay.join_extras(req_id).org_primary != Some(true)
        || state.org_relay.join_extras(peer_id).org_primary != Some(true)
    {
        return Err(D::SecondaryOrg);
    }

    // Gate 2's inputs — every read fails CLOSED, each with the reason that
    // distinguishes "the rules said no" from "the rules could not be read".
    let acl = try_load_acl(state, tenant_id, PolicyLoad::Always)
        .await
        .map_err(|e| {
            warn!(%tenant_id, %e, "org-relay: policies unreadable; refusing");
            D::PolicyUnreadable
        })?;
    let req_src = try_overlay_source_of(state, requester).await.map_err(|e| {
        warn!(%tenant_id, node = %req_id, %e, "org-relay: requester identity unreadable; refusing");
        D::PolicyUnreadable
    })?;
    let peer_src = try_overlay_source_of(state, peer).await.map_err(|e| {
        warn!(%tenant_id, node = %peer_id, %e, "org-relay: peer identity unreadable; refusing");
        D::PolicyUnreadable
    })?;
    let approved = state
        .fleet
        .agents
        .list_relay_approved(tenant_id)
        .await
        .map_err(|e| {
            warn!(%tenant_id, %e, "org-relay: approved-relay list unreadable; refusing");
            D::PolicyUnreadable
        })?;

    let now = Instant::now();
    let mut candidates: Vec<Plan> = Vec::new();
    let mut acl_denied = 0usize;
    let mut bad_static = false;
    for agent in approved {
        let Some(agent_id) = agent.id else { continue };
        // Gate 4 as the device last advertised it, and liveness on this pod.
        if !agent.capabilities.has_rpc(RpcCap::RelayServer)
            || !state.fleet.rc_hub.is_agent_online(agent_id)
        {
            continue;
        }
        let Ok(Some(node)) = state
            .overlay_nodes
            .find_live_by_tenant_and_machine(tenant_id, &agent.machine_id)
            .await
        else {
            continue;
        };
        let Some(relay_id) = node.id else { continue };
        if relay_id == req_id || relay_id == peer_id {
            continue;
        }
        let extras = state.org_relay.join_extras(relay_id);
        // Serving is primary-only: a UDP listener is host-global.
        if extras.org_primary != Some(true) {
            continue;
        }
        if state.org_relay.sessions_on_relay(relay_id) >= limits::MAX_SESSIONS_PER_RELAY {
            continue;
        }
        // Gate 2 — BOTH members must be granted this relay; each binds to it.
        let relay_ref = OverlayPeerRef {
            node_id: relay_id,
            overlay_ip: &node.overlay_ip,
            approved_routes: &node.approved_routes,
        };
        if !(evaluate_overlay(&acl.policies, &req_src, relay_ref).visible
            && evaluate_overlay(&acl.policies, &peer_src, relay_ref).visible)
        {
            acl_denied += 1;
            continue;
        }
        let port = extras.relay_port.unwrap_or(limits::DEFAULT_RELAY_PORT);
        let endpoints =
            match relay_endpoints(&node, port, &agent.peer_relay_policy.static_endpoints) {
                Ok(e) => e,
                Err(()) => {
                    bad_static = true;
                    continue;
                }
            };
        if endpoints.is_empty() {
            debug!(%tenant_id, relay = %relay_id, "org-relay: candidate has no public endpoint; skipped");
            continue;
        }
        candidates.push(Plan {
            relay: node,
            relay_agent_id: agent_id,
            endpoints,
        });
    }
    if candidates.is_empty() {
        return Err(if bad_static {
            D::NonRoutableEndpoint
        } else if acl_denied > 0 {
            D::AclDenied
        } else {
            D::NoRelay
        });
    }

    // Rank: a relay BOTH members have measured reachable wins, by RTT; never
    // measured next; one a member measured unreachable last — last, not
    // excluded, because a stale negative must not starve the only relay.
    candidates.sort_by_key(|c| {
        let relay_id = c.relay.id.unwrap_or_default();
        let a = state.org_relay.probe_verdict(req_id, relay_id, now);
        let b = state.org_relay.probe_verdict(peer_id, relay_id, now);
        let class = match (a, b) {
            (ProbeVerdict::Unreachable, _) | (_, ProbeVerdict::Unreachable) => 2u8,
            (ProbeVerdict::Reachable(_), ProbeVerdict::Reachable(_)) => 0,
            _ => 1,
        };
        let rtt = |v: ProbeVerdict| match v {
            ProbeVerdict::Reachable(r) => r,
            _ => 0,
        };
        (
            class,
            rtt(a) + rtt(b),
            state.org_relay.sessions_on_relay(relay_id),
        )
    });
    let plan = candidates.swap_remove(0);
    let relay_id = plan.relay.id.unwrap_or_default();

    // Gate 5 — after the identity gates, keyed as the spec prescribes.
    if !state
        .relay_rate_limiter
        .check(req_id, relay_id, limits::MINT_RATE_LIMIT_PER_MINUTE)
    {
        return Err(D::RateLimited);
    }
    Ok(plan)
}

fn random_secret() -> String {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    BASE64.encode(b)
}

/// The entry point, called from `handle_overlay_relay_request` after its own
/// cross-tenant and ACL-enforce checks. Never blocks the TURN grant: the
/// client cascade (P4) picks Org over Turn/Derp when a session arrives.
pub async fn maybe_mint(state: &NetworkState, requester: &OverlayNode, peer: &OverlayNode) {
    let tenant_id = requester.tenant_id;
    let mode = peer_relay_mode(state, tenant_id).await;
    if mode == PeerRelayMode::Off {
        // Gate 1 — zero rows, zero further reads.
        return;
    }
    let (Some(req_id), Some(peer_id)) = (requester.id, peer.id) else {
        return;
    };
    let warn_only = mode == PeerRelayMode::Warn;
    let pk = pair_key(req_id, peer_id);

    // Idempotent per pair: a live session is re-pushed to the ASKER only —
    // the peer and the relay already hold it. Not audited: nothing was
    // decided, and a retrying client must not fill the log.
    if !warn_only && let Some(s) = state.org_relay.active_session(&pk) {
        if let Some(i) = s.member_index(req_id) {
            push_member(state, &s, i).await;
            debug!(%tenant_id, %pk, vni = s.vni, "org-relay: re-pushed the live session to a re-requesting member");
        }
        return;
    }

    let verdict = plan_mint(state, requester, peer).await;
    let plan = match verdict {
        Ok(plan) => plan,
        Err(reason) => {
            info!(%tenant_id, requester = %req_id, peer = %peer_id, ?reason, warn_only, "org-relay: mint refused");
            audit_mint(
                state,
                tenant_id,
                req_id,
                peer_id,
                None,
                None,
                warn_only,
                Some(reason),
            )
            .await;
            return;
        }
    };
    let relay_id = plan.relay.id.unwrap_or_default();
    let Some(vni) = state.org_relay.alloc_vni(relay_id) else {
        // Every VNI on that relay is held: it is full in a way the session
        // count did not show. Same answer as no relay at all.
        audit_mint(
            state,
            tenant_id,
            req_id,
            peer_id,
            Some((plan.relay_agent_id, relay_id)),
            None,
            warn_only,
            Some(PeerRelayDenyReason::NoRelay),
        )
        .await;
        return;
    };
    let generation = state.org_relay.lamport.fetch_add(1, Ordering::Relaxed) + 1;
    let session = OrgRelaySession {
        tenant_id,
        pair_key: pk.clone(),
        vni,
        generation,
        relay: SessionParty {
            node_id: relay_id,
            node_ref: plan.relay.node_ref.clone(),
        },
        relay_agent_id: plan.relay_agent_id,
        members: [
            SessionMember {
                party: SessionParty {
                    node_id: req_id,
                    node_ref: requester.node_ref.clone(),
                },
                wg_public_key: requester.wg_public_key.clone(),
                bind_secret: random_secret(),
            },
            SessionMember {
                party: SessionParty {
                    node_id: peer_id,
                    node_ref: peer.node_ref.clone(),
                },
                wg_public_key: peer.wg_public_key.clone(),
                bind_secret: random_secret(),
            },
        ],
        endpoints: plan.endpoints.clone(),
        expires_at: Instant::now() + Duration::from_secs(u64::from(limits::MAX_LIFETIME_SECS)),
    };
    audit_mint(
        state,
        tenant_id,
        req_id,
        peer_id,
        Some((plan.relay_agent_id, relay_id)),
        Some(vni),
        warn_only,
        None,
    )
    .await;
    if warn_only {
        info!(%tenant_id, requester = %req_id, peer = %peer_id, relay = %relay_id, vni, "org-relay [warn]: would mint; pushing nothing");
        return;
    }
    state.org_relay.sessions.insert(pk.clone(), session.clone());
    info!(%tenant_id, requester = %req_id, peer = %peer_id, relay = %relay_id, vni, generation,
        endpoints = ?session.endpoints, "org-relay: minted");

    // The relay FIRST, so it can verify the binds by the time the members
    // arrive; then each member with its own secret.
    send_to_node(
        state,
        &plan.relay,
        ServerMsg::OverlayRelayServe {
            vni,
            generation,
            members: session
                .members
                .iter()
                .map(|m| RelayMemberWire {
                    wg_public_key: m.wg_public_key.clone(),
                    bind_secret: m.bind_secret.clone(),
                })
                .collect(),
            bind_secs: limits::BIND_SECS,
            idle_secs: limits::IDLE_SECS,
            max_lifetime_secs: limits::MAX_LIFETIME_SECS,
        },
    )
    .await;
    push_member(state, &session, 0).await;
    push_member(state, &session, 1).await;
}

/// `rc:overlay.relay_session` to member `i`, naming the OTHER member as its
/// peer and carrying only its own secret.
async fn push_member(state: &NetworkState, s: &OrgRelaySession, i: usize) {
    let me = &s.members[i];
    let other = &s.members[1 - i];
    send_to_node_ref(
        state,
        &me.party.node_ref,
        ServerMsg::OverlayRelaySession {
            vni: s.vni,
            generation: s.generation,
            peer_node_id: other.party.node_id,
            relay_node_id: s.relay.node_id,
            relay_endpoints: s.endpoints.clone(),
            bind_secret: me.bind_secret.clone(),
            bind_secs: limits::BIND_SECS,
            max_lifetime_secs: limits::MAX_LIFETIME_SECS,
        },
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn audit_mint(
    state: &NetworkState,
    tenant_id: ObjectId,
    requester: ObjectId,
    peer: ObjectId,
    relay: Option<(ObjectId, ObjectId)>,
    vni: Option<u32>,
    warn_only: bool,
    denied: Option<PeerRelayDenyReason>,
) {
    let event = PeerRelayAuditEvent {
        id: None,
        tenant_id,
        action: PeerRelayAuditAction::Mint,
        agent_id: relay.map(|r| r.0),
        user_id: None,
        requester_node_id: Some(requester),
        peer_node_id: Some(peer),
        relay_node_id: relay.map(|r| r.1),
        serve: None,
        vni,
        warn_only,
        at: DateTime::now(),
        denied,
        reason: None,
    };
    if let Err(e) = state.peer_relay_audit.record(event).await {
        warn!(%e, "org-relay: audit write failed");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Revocation — a push, never an expiry (§7)
// ─────────────────────────────────────────────────────────────────────────────

/// Tear down every live session `pred` selects: `rc:overlay.relay_revoke` to
/// the relay and both members, the record dropped, one audit row per session
/// naming the trigger. Returns how many were revoked.
pub async fn revoke_where(
    state: &NetworkState,
    reason: &str,
    pred: impl Fn(&OrgRelaySession) -> bool,
) -> usize {
    let victims: Vec<OrgRelaySession> = state
        .org_relay
        .sessions
        .iter()
        .filter(|e| pred(e.value()))
        .map(|e| e.value().clone())
        .collect();
    for s in &victims {
        state.org_relay.sessions.remove(&s.pair_key);
        let msg = ServerMsg::OverlayRelayRevoke { vni: s.vni };
        send_to_node_ref(state, &s.relay.node_ref, msg.clone()).await;
        for m in &s.members {
            send_to_node_ref(state, &m.party.node_ref, msg.clone()).await;
        }
        info!(tenant_id = %s.tenant_id, vni = s.vni, relay = %s.relay.node_id, reason, "org-relay: session revoked");
        let event = PeerRelayAuditEvent {
            id: None,
            tenant_id: s.tenant_id,
            action: PeerRelayAuditAction::Revoke,
            agent_id: Some(s.relay_agent_id),
            user_id: None,
            requester_node_id: Some(s.members[0].party.node_id),
            peer_node_id: Some(s.members[1].party.node_id),
            relay_node_id: Some(s.relay.node_id),
            serve: None,
            vni: Some(s.vni),
            warn_only: false,
            at: DateTime::now(),
            denied: None,
            reason: Some(reason.to_string()),
        };
        if let Err(e) = state.peer_relay_audit.record(event).await {
            warn!(%e, "org-relay: audit write failed");
        }
    }
    victims.len()
}

/// Trigger 1 — the org switched peer relays off.
pub async fn revoke_tenant(state: &NetworkState, tenant_id: ObjectId, reason: &str) -> usize {
    revoke_where(state, reason, |s| s.tenant_id == tenant_id).await
}

/// Trigger 3 — a relay's approval was cleared.
pub async fn revoke_relay_agent(
    state: &NetworkState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    reason: &str,
) -> usize {
    revoke_where(state, reason, |s| {
        s.tenant_id == tenant_id && s.relay_agent_id == agent_id
    })
    .await
}

/// Trigger 4 — a party (member or relay) was removed from the overlay.
pub async fn revoke_node(state: &NetworkState, node_id: ObjectId, reason: &str) -> usize {
    state.org_relay.forget_node(node_id);
    revoke_where(state, reason, |s| s.involves(node_id)).await
}

/// Trigger 2 — the tenant's policies changed: re-run gate 2 for every live
/// session and revoke the pairs no longer granted their relay.
///
/// A read failure here does NOTHING, deliberately. Refusing a NEW mint on a
/// blip costs one session; tearing down every LIVE session on a blip is the
/// "spurious deny takes the mesh down" failure `load_acl` fails open to avoid.
pub async fn reconcile_acl(state: &NetworkState, tenant_id: ObjectId) {
    let acl = match try_load_acl(state, tenant_id, PolicyLoad::Always).await {
        Ok(a) => a,
        Err(e) => {
            warn!(%tenant_id, %e, "org-relay: policies unreadable during reconcile; leaving sessions as they are");
            return;
        }
    };
    let live: Vec<OrgRelaySession> = state
        .org_relay
        .sessions
        .iter()
        .filter(|e| e.tenant_id == tenant_id)
        .map(|e| e.value().clone())
        .collect();
    for s in live {
        let Ok(relay) = state.overlay_nodes.base.find_by_id(s.relay.node_id).await else {
            continue;
        };
        let relay_ref = OverlayPeerRef {
            node_id: s.relay.node_id,
            overlay_ip: &relay.overlay_ip,
            approved_routes: &relay.approved_routes,
        };
        let mut granted = true;
        for m in &s.members {
            let Ok(node) = state.overlay_nodes.base.find_by_id(m.party.node_id).await else {
                granted = false;
                break;
            };
            let src: OverlaySource = match try_overlay_source_of(state, &node).await {
                Ok(src) => src,
                Err(_) => continue, // unreadable identity: not evidence of revocation
            };
            if !evaluate_overlay(&acl.policies, &src, relay_ref).visible {
                granted = false;
                break;
            }
        }
        if !granted {
            let pk = s.pair_key.clone();
            revoke_where(state, "acl_revoked", |x| x.pair_key == pk).await;
        }
    }
}

/// `rc:overlay.relay_probe` — a member's own measurement toward a relay the
/// server minted for it. Accepted only for (relay, endpoint) pairs that
/// appear in one of its sessions.
pub async fn handle_relay_probe(
    state: &NetworkState,
    ident: NodeIdentity,
    relay_node_id: ObjectId,
    endpoint: String,
    reachable: bool,
    rtt_ms: Option<u32>,
) {
    let Some(node) = current_node(state, ident).await else {
        return;
    };
    let Some(node_id) = node.id else { return };
    let accepted =
        state
            .org_relay
            .record_probe(node_id, relay_node_id, endpoint.clone(), reachable, rtt_ms);
    debug!(reporter = %node_id, relay = %relay_node_id, %endpoint, reachable, ?rtt_ms, accepted,
        "org-relay: reachability report");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_with(srflx: &[&str], lan: &[&str], endpoints: &[&str]) -> OverlayNode {
        let mut n: OverlayNode = serde_json::from_value(serde_json::json!({
            "tenant_id": ObjectId::new(),
            "node_ref": { "kind": "agent", "id": ObjectId::new() },
            "network_id": ObjectId::new(),
            "machine_id": "m",
            "name": "n",
            "overlay_ip": "100.64.0.9",
            "wg_public_key": "k",
            "status": "online",
            "last_seen_at": DateTime::now(),
            "created_at": DateTime::now(),
            "updated_at": DateTime::now(),
        }))
        .expect("a minimal node row deserialises");
        n.srflx_endpoints = srflx.iter().map(|s| s.to_string()).collect();
        n.lan_endpoints = lan.iter().map(|s| s.to_string()).collect();
        n.endpoints = endpoints.iter().map(|s| s.to_string()).collect();
        n
    }

    /// Public addresses are re-paired with the relay port, private ones are
    /// dropped, and a static entry keeps its own port.
    #[test]
    fn relay_endpoints_pairs_public_addresses_with_the_relay_port() {
        let n = node_with(
            &["62.210.194.66:41641"],
            &["192.168.1.7:41641", "62.210.194.66:41641"],
            &["10.10.10.11:12000"],
        );
        let out = relay_endpoints(&n, 3478, &["8.8.8.8:5000".to_string()]).unwrap();
        assert_eq!(out, vec!["62.210.194.66:3478", "8.8.8.8:5000"]);
    }

    /// A static endpoint that fails the SSRF rule refuses the whole plan —
    /// the metadata service, RFC1918, the overlay range, a name.
    #[test]
    fn a_non_routable_static_endpoint_is_an_error_not_a_skip() {
        let n = node_with(&["62.210.194.66:41641"], &[], &[]);
        for bad in [
            "169.254.169.254:80",
            "10.0.0.5:3478",
            "100.64.0.1:3478",
            "127.0.0.1:3478",
            "relay.example.com:3478",
        ] {
            assert!(
                relay_endpoints(&n, 3478, &[bad.to_string()]).is_err(),
                "{bad} must refuse"
            );
            assert!(!valid_static_endpoint(bad), "{bad} must not validate");
        }
        assert!(valid_static_endpoint("62.210.194.66:3478"));
        assert!(!valid_static_endpoint("62.210.194.66:0"));
    }

    /// VNIs are per relay, never 0 or the STUN cookie, and never one a live
    /// session on that relay still holds.
    #[test]
    fn vni_allocation_skips_reserved_and_in_use_values() {
        let st = OrgRelayState::new();
        let relay = ObjectId::new();
        // Park the cursor just below the STUN cookie and check it is skipped.
        st.next_vni.insert(relay, limits::STUN_COOKIE_VNI - 1);
        assert_eq!(st.alloc_vni(relay), Some(limits::STUN_COOKIE_VNI - 1));
        assert_eq!(st.alloc_vni(relay), Some(limits::STUN_COOKIE_VNI + 1));
        // Wrap at the 24-bit ceiling, skipping 0.
        st.next_vni.insert(relay, limits::VNI_MAX);
        assert_eq!(st.alloc_vni(relay), Some(limits::VNI_MAX));
        assert_eq!(st.alloc_vni(relay), Some(1));
        // Another relay has its own space.
        assert_eq!(st.alloc_vni(ObjectId::new()), Some(1));
    }

    /// A report is accepted only about a relay + endpoint the server minted
    /// for that node.
    #[test]
    fn probes_are_accepted_only_for_minted_targets() {
        let st = OrgRelayState::new();
        let (a, b, relay) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let party = |id| SessionParty {
            node_id: id,
            node_ref: NodeRef::Agent { agent_id: id },
        };
        st.sessions.insert(
            "pk".into(),
            OrgRelaySession {
                tenant_id: ObjectId::new(),
                pair_key: "pk".into(),
                vni: 7,
                generation: 1,
                relay: party(relay),
                relay_agent_id: relay,
                members: [
                    SessionMember {
                        party: party(a),
                        wg_public_key: "a".into(),
                        bind_secret: "s".into(),
                    },
                    SessionMember {
                        party: party(b),
                        wg_public_key: "b".into(),
                        bind_secret: "t".into(),
                    },
                ],
                endpoints: vec!["8.8.8.8:3478".into()],
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        assert!(st.record_probe(a, relay, "8.8.8.8:3478".into(), true, Some(12)));
        assert!(
            !st.record_probe(a, relay, "8.8.4.4:3478".into(), true, None),
            "an endpoint never minted"
        );
        assert!(
            !st.record_probe(ObjectId::new(), relay, "8.8.8.8:3478".into(), true, None),
            "a stranger"
        );
        assert_eq!(
            st.probe_verdict(a, relay, Instant::now()),
            ProbeVerdict::Reachable(12)
        );
        assert_eq!(
            st.probe_verdict(b, relay, Instant::now()),
            ProbeVerdict::Unknown
        );
        assert!(st.record_probe(b, relay, "8.8.8.8:3478".into(), false, None));
        assert_eq!(
            st.probe_verdict(b, relay, Instant::now()),
            ProbeVerdict::Unreachable
        );
    }
}
