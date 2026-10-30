// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-69 P5a — the host's TRANSITIONAL hook implementations: what `network`
//! will register through `Module::hooks` once it is a module, implemented
//! over the host's own overlay code until then and registered under the
//! `network` id, so the fleet module's cascades already run in
//! [`roomler_core::hooks::HOOK_ORDER`] through the core registry.
//!
//! When `network` is extracted (P7) this file goes with it — the `impl`
//! moves, the registration becomes the module's own.

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::NodeRef;
use roomler_ai_remote_control::signaling::CapVectorWire;
use roomler_core::hooks::{
    FleetLifecycle, ReleasedLease, RenamePropagation, TenantArchived, TenantLifecycle,
};

use crate::NetworkState;

/// The overlay lease + MagicDNS label holder, over the host's overlay code.
pub struct NetworkHooks {
    pub state: NetworkState,
}

#[async_trait]
impl FleetLifecycle for NetworkHooks {
    /// Release the overlay lease BEFORE the row delete and BEFORE the kick
    /// (the kick's WS teardown runs `handle_overlay_leave`, which must find an
    /// already-tombstoned node rather than race the release CAS).
    async fn agent_removed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        machine_id: &str,
        reason: &str,
    ) -> anyhow::Result<Option<ReleasedLease>> {
        let released = crate::overlay::release_overlay_node_for(
            &self.state,
            tenant_id,
            machine_id,
            &NodeRef::Agent { agent_id },
            reason,
        )
        .await;
        Ok(released.map(|r| ReleasedLease {
            overlay_ip: r.overlay_ip,
        }))
    }

    /// Best-effort propagation onto the live overlay node: peers see the new
    /// MagicDNS label immediately (delta re-fan); the device itself re-learns
    /// its self-name on its next reconnect.
    async fn agent_renamed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        name: &str,
    ) -> anyhow::Result<RenamePropagation> {
        let Some(node) = self
            .state
            .overlay_nodes
            .find_live_by_agent(tenant_id, agent_id)
            .await?
        else {
            return Ok(RenamePropagation::NoLiveNode);
        };
        Ok(
            match crate::overlay::propagate_node_rename(&self.state, &node, name).await {
                Some(label) => RenamePropagation::Propagated(label),
                None => RenamePropagation::Failed,
            },
        )
    }

    /// P7b — the tunnel sessions this node holds FOR the agent: the ones
    /// targeting it, and (PR-1) the ones it originated through declared
    /// routes. The per-connection session map is invisible to fleet's nudge
    /// handler, so without this answer a routes-only agent read as IDLE and
    /// got its WS cycled — tearing every declared route plus its overlay
    /// carriers. The reason string is what the nudge reply carries.
    async fn agent_busy(&self, agent_id: ObjectId) -> Option<&'static str> {
        let target_busy = self
            .state
            .tunnel_sessions_by_target_agent
            .get(&agent_id)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let origin_busy = self
            .state
            .tunnel_sessions_by_origin_agent
            .get(&agent_id)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if origin_busy && !target_busy {
            Some("origin_busy")
        } else if target_busy || origin_busy {
            Some("tunnel_busy")
        } else {
            None
        }
    }

    /// FR-128 — both devices' far-TURN reach, from each live overlay node's
    /// last netcheck behind the same freshness gate every consumer uses. A
    /// device with no live node, or no fresh vector, is not measured; a
    /// lookup that fails reads the same way, never as "dead".
    async fn far_turn_udp(&self, tenant_id: ObjectId, a: ObjectId, b: ObjectId) -> Option<bool> {
        let mut ends = Vec::with_capacity(2);
        for agent_id in [a, b] {
            let node = self
                .state
                .overlay_nodes
                .find_live_by_agent(tenant_id, agent_id)
                .await
                .ok()
                .flatten();
            ends.push(node.as_ref().and_then(crate::overlay::fresh_caps));
        }
        far_turn_verdict(ends[0].as_ref(), ends[1].as_ref())
    }
}

/// FR-128 — one device reaches the far TURN over UDP when its netcheck found
/// a public mapping (`stun_udp`): a relay↔relay pair needs only that port.
/// `relay_band_udp` is the dialer's bit, never read here: read as "dead" it
/// put a session on DERP while the far TURN carried others (spec §8, 10-30).
fn reaches_far_turn(caps: &CapVectorWire) -> bool {
    caps.stun_udp
}

/// FR-128 — the pair's verdict: `Some(false)` when either end measured the
/// far TURN dead, `Some(true)` when both measured it alive, else `None`.
fn far_turn_verdict(a: Option<&CapVectorWire>, b: Option<&CapVectorWire>) -> Option<bool> {
    match (a.map(reaches_far_turn), b.map(reaches_far_turn)) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// Network's side of the tenant cascade (P7b, from the host's archive
/// route): release every overlay node — which pools its address and tells
/// the peers — then quarantine the tenant's block, never re-issue it (a
/// device that never saw the archive still thinks it owns an address in that
/// range).
pub struct NetworkTenantHooks {
    pub state: NetworkState,
}

#[async_trait]
impl TenantLifecycle for NetworkTenantHooks {
    async fn tenant_archived(
        &self,
        tenant_id: ObjectId,
        reason: &str,
    ) -> anyhow::Result<TenantArchived> {
        let state = &self.state;
        let network = state
            .overlay_networks
            .find_for_tenant(tenant_id)
            .await
            .ok()
            .flatten();
        let mut nodes_released = 0u64;
        if let Some(net) = &network
            && let Some(net_id) = net.id
        {
            let nodes = state
                .overlay_nodes
                .list_active_in_network(tenant_id, net_id)
                .await
                .unwrap_or_default();
            for n in &nodes {
                if crate::overlay::release_overlay_node(state, n, reason)
                    .await
                    .is_some()
                {
                    nodes_released += 1;
                }
            }
        }

        let mut block_quarantined = None;
        if let Some(net_id) = network.as_ref().and_then(|n| n.id)
            && let Ok(Some(block)) = state
                .overlay_networks
                .blocks()
                .find_assigned_for_network(net_id)
                .await
            && let Some(bid) = block.id
            && state
                .overlay_networks
                .blocks()
                .quarantine(bid, reason)
                .await
                .unwrap_or(false)
        {
            block_quarantined = Some(block.cidr);
        }

        Ok(TenantArchived {
            devices_revoked: 0,
            nodes_released,
            block_quarantined,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(stun_udp: bool, relay_band_udp: Option<bool>) -> CapVectorWire {
        CapVectorWire {
            stun_udp,
            relay_band_udp,
            ..Default::default()
        }
    }

    /// FR-128 — the far-TURN verdict for a pair. One end with no UDP to coturn
    /// decides `false` whatever the other says; both reaching it is `true`;
    /// any end not measured, with the other alive, is `None`, which the agent
    /// reads as "it works". The relay band is not read: on 2026-10-30 a lab
    /// session rode the far TURN with the band measured blocked at both ends.
    #[test]
    fn far_turn_verdict_is_dead_if_either_end_measured_it_dead() {
        let alive = caps(true, Some(true));
        let unmeasured_band = caps(true, None);
        let band_blocked = caps(true, Some(false));
        let no_udp = caps(false, None);

        assert_eq!(far_turn_verdict(Some(&alive), Some(&alive)), Some(true));
        assert_eq!(
            far_turn_verdict(Some(&alive), Some(&unmeasured_band)),
            Some(true),
            "a band netcheck could not measure is not a blocked band"
        );
        assert_eq!(
            far_turn_verdict(Some(&band_blocked), Some(&band_blocked)),
            Some(true),
            "a blocked relay band still lets a relay↔relay pair through coturn's port"
        );
        assert_eq!(far_turn_verdict(Some(&no_udp), Some(&alive)), Some(false));
        assert_eq!(far_turn_verdict(None, Some(&no_udp)), Some(false));
        assert_eq!(far_turn_verdict(None, Some(&band_blocked)), None);
        assert_eq!(far_turn_verdict(Some(&alive), None), None);
        assert_eq!(far_turn_verdict(None, None), None);
    }
}
