// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! C-6 — cluster observability: the counters every rehome/fallback path
//! increments.
//!
//! Counters are process-local statics (per-pod by construction — exactly
//! the attribution we want; the operator queries each pod through the
//! LB's `?tid` pinning or the pod IPs directly).
//!
//! FR-69 P1b — the counters moved here with the cluster bus that bumps
//! them; the per-pod **snapshot** the `/api/cluster/status` route serves
//! stayed in the api crate, because it also reads module-owned counters and
//! registries (media rooms, the RC hub, the DERP registry). The api crate
//! re-exports everything here under its old `crate::cluster::metrics` path.

use std::sync::atomic::{AtomicU64, Ordering};

/// rc `SessionRequest` misses answered with `agent_on_other_pod` (C-2).
pub static RC_REHOME_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Tunnel opens rejected with `agent_on_other_pod` (C-3).
pub static TUNNEL_REHOME_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Idle-agent nudges EXECUTED (WS cycled so both ends re-hash) (C-2).
pub static AGENT_NUDGE_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Bus RPCs that hit their deadline (owner presumed dead) (C-1).
pub static BUS_DEADLINE_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Media islands folded (claim lost / belt-era split resolution) (C-4).
pub static MEDIA_FOLD_TOTAL: AtomicU64 = AtomicU64::new(0);
/// DERP sockets closed for cluster convergence (C-5).
pub static DERP_REHOME_CLOSE_TOTAL: AtomicU64 = AtomicU64::new(0);
/// FR-19 — total bytes this pod has relayed over `/derp` (peer-to-peer WG
/// carrier that crossed the control plane). This is the quantity a peer relay
/// takes OFF the pod: when a tenant moves a pair onto `relay:org/udp`, that
/// pair's carrier no longer traverses `/derp`, so this counter's growth rate
/// falls for the moved traffic. ⚠️ `derp_registrations` must NOT fall with it —
/// the DERP floor is never torn down (§7); a falling registration count is a
/// regression, a falling BYTE rate is the win.
pub static DERP_BYTES_RELAYED_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Split-evidence observations (rc hub-miss with fresh foreign record,
/// tunnel relay drop with foreign session record) (A2b).
pub static SPLIT_EVIDENCE_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Relay grants issued on a NON-default region (multi-region PoPs).
pub static RELAY_REGION_PICK_TOTAL: AtomicU64 = AtomicU64::new(0);
/// PR-1 rehome — cross-pod rc/tunnel misses resolved by MOVING THE
/// CONTROLLER (mis-keyed / stale / ambiguous dial): rehome reply sent,
/// agent nudge deliberately suppressed. The 2026-08-04 incident class.
pub static RC_REHOME_CONTROLLER_TOTAL: AtomicU64 = AtomicU64::new(0);
/// PR-1 rehome — owner-side nudge refusals (rc/tunnel/origin busy). A
/// persistently growing value = a parked-busy population that only
/// converges on idle; the deferred-fire follow-up feeds on this signal.
pub static AGENT_NUDGE_REFUSED_TOTAL: AtomicU64 = AtomicU64::new(0);
/// PR-1 rehome — nudges suppressed by the cooldown cap (split evidence:
/// repeated convergence attempts are not converging).
pub static AGENT_NUDGE_STUCK_TOTAL: AtomicU64 = AtomicU64::new(0);
/// PR-2 — rc frames successfully relayed to the owner pod (cross-pod
/// sessions WORKING rather than converging; sustained growth means a
/// stable co-location gap worth investigating).
pub static RC_RELAY_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}
