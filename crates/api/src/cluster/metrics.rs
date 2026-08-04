//! C-6 — cluster observability: the counters every rehome/fallback path
//! increments, and the snapshot the `/api/cluster/status` route serves.
//!
//! Counters are process-local statics (per-pod by construction — exactly
//! the attribution we want; the operator queries each pod through the
//! LB's `?tid` pinning or the pod IPs directly). Gauges are computed at
//! snapshot time from the live registries — never stored, so they can't
//! go stale. The media gauges (rooms / participants / consumers) are the
//! trigger inputs for the deferred PipeTransport stage: revisit when any
//! room sustains ≥12–15 AV participants (≈450+ consumers vs the
//! ~500/worker ceiling) or pod aggregate >60% capacity.

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
/// Split-evidence observations (rc hub-miss with fresh foreign record,
/// tunnel relay drop with foreign session record) (A2b).
pub static SPLIT_EVIDENCE_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Relay grants issued on a NON-default region (multi-region PoPs).
pub static RELAY_REGION_PICK_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Snapshot every counter + live gauge for one pod.
pub async fn snapshot(state: &crate::state::AppState) -> serde_json::Value {
    // Media gauges: per-room participant/consumer counts (the
    // PipeTransport trigger inputs).
    let mut media_rooms = Vec::new();
    let mut participants_total = 0usize;
    let mut consumers_total = 0usize;
    for room in state.room_manager.rooms_ref().iter() {
        let participants = room.participants.len();
        let consumers: usize = room.participants.iter().map(|p| p.consumers.len()).sum();
        participants_total += participants;
        consumers_total += consumers;
        media_rooms.push(serde_json::json!({
            "room_id": room.key().to_hex(),
            "participants": participants,
            "consumers": consumers,
        }));
    }

    // Alive pods per the advisory directory records.
    let pods_alive: Vec<String> = match &state.cluster_directory {
        Some(dir) => dir
            .scan_keys("roomler:pod-alive:*")
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|k| k.rsplit(':').next().map(str::to_string))
            .collect(),
        None => Vec::new(),
    };

    serde_json::json!({
        "pod": {
            "pod_id": state.pod.pod_id,
            "epoch": state.pod.epoch,
            "origin": state.pod.origin(),
        },
        "cluster": {
            "directory": state.cluster_directory.is_some(),
            "bus_alive": state
                .cluster_bus
                .as_ref()
                .map(|b| b.sub_alive.load(Ordering::Relaxed))
                .unwrap_or(false),
            "pods_alive": pods_alive,
        },
        "counters": {
            "rc_rehome_total": RC_REHOME_TOTAL.load(Ordering::Relaxed),
            "tunnel_rehome_total": TUNNEL_REHOME_TOTAL.load(Ordering::Relaxed),
            "agent_nudge_total": AGENT_NUDGE_TOTAL.load(Ordering::Relaxed),
            "bus_deadline_total": BUS_DEADLINE_TOTAL.load(Ordering::Relaxed),
            "media_fold_total": MEDIA_FOLD_TOTAL.load(Ordering::Relaxed),
            "media_belt_fallback_total":
                crate::ws::media_cluster::MEDIA_BELT_FALLBACK_TOTAL.load(Ordering::Relaxed),
            "derp_rehome_close_total": DERP_REHOME_CLOSE_TOTAL.load(Ordering::Relaxed),
            "derp_rehome_stuck_total":
                crate::ws::derp_cluster::DERP_REHOME_STUCK_TOTAL.load(Ordering::Relaxed),
            "split_evidence_total": SPLIT_EVIDENCE_TOTAL.load(Ordering::Relaxed),
            "relay_region_pick_total": RELAY_REGION_PICK_TOTAL.load(Ordering::Relaxed),
        },
        "local": {
            "agents_online": state.rc_hub.online_agents().len(),
            "tunnel_sessions": state.tunnel_clients_by_session.len(),
            "derp_registrations": state.derp_registry.len(),
            "media_rooms": media_rooms.len(),
            "media_participants": participants_total,
            "media_consumers": consumers_total,
            "media_rooms_detail": media_rooms,
        },
    })
}
