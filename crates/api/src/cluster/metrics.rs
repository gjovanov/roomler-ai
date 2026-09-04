// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! C-6 — cluster observability: the snapshot the `/api/cluster/status`
//! route serves.
//!
//! FR-69 P1b — the counters themselves live in
//! `roomler_core::cluster::metrics` (they moved with the cluster bus that
//! bumps them) and are re-exported here under their old paths. Gauges are
//! computed at snapshot time from the live registries — never stored, so
//! they can't go stale. The media gauges (rooms / participants / consumers)
//! are the trigger inputs for the deferred PipeTransport stage: revisit when
//! any room sustains ≥12–15 AV participants (≈450+ consumers vs the
//! ~500/worker ceiling) or pod aggregate >60% capacity.

use std::sync::atomic::Ordering;

pub use roomler_core::cluster::metrics::*;

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
            "derp_bytes_relayed_total": DERP_BYTES_RELAYED_TOTAL.load(Ordering::Relaxed),
            "derp_rehome_stuck_total":
                crate::ws::derp_cluster::DERP_REHOME_STUCK_TOTAL.load(Ordering::Relaxed),
            "split_evidence_total": SPLIT_EVIDENCE_TOTAL.load(Ordering::Relaxed),
            "relay_region_pick_total": RELAY_REGION_PICK_TOTAL.load(Ordering::Relaxed),
            "rc_rehome_controller_total": RC_REHOME_CONTROLLER_TOTAL.load(Ordering::Relaxed),
            "agent_nudge_refused_total": AGENT_NUDGE_REFUSED_TOTAL.load(Ordering::Relaxed),
            "agent_nudge_stuck_total": AGENT_NUDGE_STUCK_TOTAL.load(Ordering::Relaxed),
            "rc_relay_total": RC_RELAY_TOTAL.load(Ordering::Relaxed),
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
