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

    /// FR-128 — the far-TURN reach of the session's two ends: the target, and
    /// the browser through every device it runs on (D8, [`browser_reach`]).
    async fn far_turn_udp(
        &self,
        tenant_id: ObjectId,
        target: ObjectId,
        viewers: &[ObjectId],
    ) -> Option<bool> {
        let target_reach = self.far_turn_reach(tenant_id, target).await;
        let mut viewer_reach = Vec::with_capacity(viewers.len());
        for &viewer in viewers {
            viewer_reach.push(self.far_turn_reach(tenant_id, viewer).await);
        }
        far_turn_verdict(target_reach, browser_reach(viewer_reach))
    }
}

impl NetworkHooks {
    /// FR-128 — one device's far-TURN reach, from its live overlay node's last
    /// netcheck behind the same freshness gate every consumer uses. A device
    /// with no live node, or no fresh vector, is not measured (`None`); a
    /// lookup that fails reads the same way, never as "dead".
    async fn far_turn_reach(&self, tenant_id: ObjectId, agent_id: ObjectId) -> Option<bool> {
        let node = self
            .state
            .overlay_nodes
            .find_live_by_agent(tenant_id, agent_id)
            .await
            .ok()
            .flatten();
        node.as_ref()
            .and_then(crate::overlay::fresh_caps)
            .map(|caps| reaches_far_turn(&caps))
    }
}

/// FR-128 — one device reaches the far TURN over UDP when its netcheck found
/// a public mapping (`stun_udp`): a relay↔relay pair needs only that port.
/// `relay_band_udp` is the dialer's bit, never read here: read as "dead" it
/// put a session on DERP while the far TURN carried others (spec §8, 10-30).
fn reaches_far_turn(caps: &CapVectorWire) -> bool {
    caps.stun_udp
}

/// FR-128 D8 — the browser's reach, over every device it runs on. One that
/// measured the far TURN alive decides: on a Windows host, its WSL agent
/// measuring no UDP out says nothing about the browser beside it. Else one
/// that measured it dead decides, so a device not measured cannot hide a
/// measured "dead" (#2781's corp laptop). Else not measured.
fn browser_reach(devices: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let mut reach = None;
    for device in devices {
        match device {
            Some(true) => return Some(true),
            Some(false) => reach = Some(false),
            None => {}
        }
    }
    reach
}

/// FR-128 — the pair's verdict: `Some(false)` when either end measured the
/// far TURN dead, `Some(true)` when both measured it alive, else `None`.
fn far_turn_verdict(target: Option<bool>, browser: Option<bool>) -> Option<bool> {
    match (target, browser) {
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

    /// One device's reach as the hook reads it: `None` when not measured.
    fn reach(c: Option<&CapVectorWire>) -> Option<bool> {
        c.map(reaches_far_turn)
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
        let verdict = |a: Option<&CapVectorWire>, b: Option<&CapVectorWire>| {
            far_turn_verdict(reach(a), reach(b))
        };

        assert_eq!(verdict(Some(&alive), Some(&alive)), Some(true));
        assert_eq!(
            verdict(Some(&alive), Some(&unmeasured_band)),
            Some(true),
            "a band netcheck could not measure is not a blocked band"
        );
        assert_eq!(
            verdict(Some(&band_blocked), Some(&band_blocked)),
            Some(true),
            "a blocked relay band still lets a relay↔relay pair through coturn's port"
        );
        assert_eq!(verdict(Some(&no_udp), Some(&alive)), Some(false));
        assert_eq!(verdict(None, Some(&no_udp)), Some(false));
        assert_eq!(verdict(None, Some(&band_blocked)), None);
        assert_eq!(verdict(Some(&alive), None), None);
        assert_eq!(verdict(None, None), None);
    }

    /// FR-128 D8 — the browser's end is its best device. One measured alive
    /// decides, wherever it sits in the list (NEO16's WSL agent, which has no
    /// UDP out, answered first on 2026-10-23); a measured "dead" outranks a
    /// device not measured; nothing measured, or no device, is `None`.
    #[test]
    fn the_browsers_reach_is_its_best_device() {
        assert_eq!(browser_reach([Some(false), Some(true)]), Some(true));
        assert_eq!(browser_reach([Some(true), Some(false)]), Some(true));
        assert_eq!(browser_reach([None, Some(true)]), Some(true));
        assert_eq!(
            browser_reach([Some(false), None]),
            Some(false),
            "a device not measured cannot hide a measured dead one"
        );
        assert_eq!(browser_reach([None, Some(false)]), Some(false));
        assert_eq!(browser_reach([None, None]), None);
        assert_eq!(browser_reach(std::iter::empty()), None);

        // The pair with a target the hint cannot measure, such as a Mac's GUI
        // worker: the browser's best device alone cannot make it `false`.
        assert_eq!(
            far_turn_verdict(None, browser_reach([Some(false), Some(true)])),
            None
        );
        assert_eq!(
            far_turn_verdict(Some(true), browser_reach([Some(false), Some(true)])),
            Some(true)
        );
        assert_eq!(
            far_turn_verdict(Some(false), browser_reach([Some(true)])),
            Some(false),
            "a target measured dead decides whatever the browser reaches"
        );
    }
}
