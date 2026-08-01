//! Route ownership + defense — split out of `runtime.rs` (rc.284, pure move):
//! the 2 s route-guard cadence, the peer `/32` drop helper, subnet-route
//! install, and the P5 exit-node split-default state machine (reconcile /
//! teardown / readiness). The route-war doctrine (re-assert peer `/32`s and
//! defend our own `/32` against a full-tunnel VPN) is anchored here.

use super::*;

/// How often to re-assert per-peer `/32` routes on the overlay NIC (rc.146).
/// A full-tunnel VPN (Check Point) keeps re-installing a competing `/32` for
/// each overlay IP via its own NIC that swallows overlay traffic; the route
/// table flaps between it and ours. Re-asserting UNCONDITIONALLY on a tight
/// cadence — not gated on the carrier's traffic counters, because a captured
/// route means our packets never reach the WG device so `tx` stays flat and a
/// traffic-gated check would never fire — keeps the overlay winning the route
/// war. Cheap (a couple of route commands per peer) and 2 s bounds the capture
/// window to a couple of dropped pings.
pub(super) const ROUTE_GUARD_TICK: Duration = Duration::from_secs(2);

/// Whether a teardown should also drop the peer's OS `/32`. `Keep` is for a
/// RE-INSTALL of the SAME node (a WG key rotation): the route is about to be
/// re-added for the same address, so dropping it only flaps the host route —
/// and on Windows every add/del is a slow IP-Helper round-trip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PeerRoute {
    Drop,
    Keep,
}

/// Drop a peer's OS `/32` — UNLESS the address is still claimed by ANOTHER
/// installed peer.
///
/// Overlay addresses are RECYCLED: the server returns a removed device's host
/// number to the tenant's pool, so by the time we reap a stale peer its address
/// may already belong to a live one. Deleting the route unconditionally would
/// blackhole the new owner with nothing to re-install it (`install_peers` skips
/// a node that is already in `by_node`).
///
/// Call AFTER the removed peer is out of `by_node`, so the scan sees only
/// survivors.
pub(super) async fn del_peer_route_if_unowned(
    tun: &Arc<dyn TunIo>,
    by_node: &HashMap<ObjectId, Installed>,
    ip: Ipv4Addr,
) {
    if by_node.values().any(|e| e.overlay_ip == ip) {
        debug!(%ip, "overlay: keeping the OS route — the address was recycled to a live peer");
        return;
    }
    tun.del_peer_route(ip).await;
}

/// P5 exit-node — the two IPv4 split-default halves. Installing these (as OS
/// routes via the overlay NIC + as the exit peer's WG `allowed_ips`) beats the
/// host's `0.0.0.0/0` default by longest-prefix WITHOUT deleting it, so the OS
/// default self-heals the instant the overlay routes go away (a crash / kill
/// can't strand the host offline — see A2/D3 in the P5 plan). `pub(crate)` so the
/// crash-safety purge ([`crate::overlay::tun::purge_split_default`]) removes EXACTLY what
/// the installer installs — one source of truth, symmetric by construction.
pub(crate) const SPLIT_DEFAULT_V4: [&str; 2] = ["0.0.0.0/1", "128.0.0.0/1"];
/// P5 exit-node — the two IPv6 halves, installed via the overlay NIC as a
/// FAIL-CLOSED measure: the crypto-router drops any non-derived-ULA v6
/// destination, so routing `::/1` + `8000::/1` into the overlay blackholes ALL
/// v6 internet egress. Without this a dual-stack host would send v4 through the
/// exit but leak v6 straight out its uplink (silent AAAA deanonymisation). Full
/// v6 exit egress is a follow-up (S3b); this is the minimum-safe stance (A5).
pub(crate) const SPLIT_DEFAULT_V6: [&str; 2] = ["::/1", "8000::/1"];

/// P5 — resolve the operator's chosen exit-node selector (a [`NetmapPeer`]'s
/// `name` or a node-id hex string) to a concrete node in the current netmap.
/// Pure, so the name-vs-hex match is unit-tested directly. `None` when no peer
/// matches (the chosen exit isn't in the mesh yet — reconcile defers rather than
/// blackholing egress waiting for it).
pub(super) fn resolve_exit_peer(
    selector: &str,
    peers: &HashMap<ObjectId, NetmapPeer>,
) -> Option<ObjectId> {
    let selector = selector.trim();
    peers
        .values()
        .find(|np| np.name == selector || np.node_id.to_hex() == selector)
        .map(|np| np.node_id)
}

/// P5 — did a NAME selector drift to a DIFFERENT machine than the one it first
/// resolved to?
///
/// A name is not a stable identity: removing a device releases its MagicDNS name
/// back to the network, so the same label can later belong to another machine.
/// Following it would silently redirect this host's ENTIRE internet egress. Once
/// we have captured the default route through a named peer we refuse to follow
/// the label elsewhere without operator action; a node-id hex selector is
/// unambiguous by construction and never drifts. Pure.
pub(super) fn exit_selector_drifted(
    pinned: Option<ObjectId>,
    selector: &str,
    resolved: ObjectId,
) -> bool {
    selector.trim() != resolved.to_hex() && pinned.is_some_and(|p| p != resolved)
}

/// P5 — is `peer` an admin-APPROVED exit node? The client only routes its
/// default egress through a peer whose netmap `routes` carry a default route
/// (`0.0.0.0/0`); the server only ever puts one there via the dedicated
/// exit-node approval (A6), so this is the client-side half of the admin gate —
/// naming a peer that wasn't approved as an exit node stays inert. Pure.
pub(super) fn peer_is_approved_exit(peer: &NetmapPeer) -> bool {
    peer.routes
        .iter()
        .filter_map(|r| crate::overlay::router::Cidr::parse(r))
        .any(|c| c.is_default_route())
}

/// P5 — the carrier-critical endpoint IPs that MUST bypass the split-default
/// (pinned via the ORIGINAL gateway) for the mesh to survive exit routing: the
/// coordination server's resolved IPs, every live RELAY carrier's coturn worker
/// IPs (both our own allocation `relay_local` and the peer's `relay_dst`), AND
/// (Phase A) every PUBLIC-DIRECT carrier's peer address. A SAME-LAN direct
/// carrier is on-link (a connected route more specific than a `/1`), so it needs
/// no exemption — but a public-direct carrier crosses the internet via the
/// default route, so without pinning its dst the split-default would swallow the
/// path to the exit itself and self-wedge. Pure, so the set arithmetic is
/// unit-tested against synthetic carriers.
pub(super) fn exit_exemption_set(
    server_ips: &[IpAddr],
    by_node: &HashMap<ObjectId, Installed>,
) -> HashSet<IpAddr> {
    let mut set: HashSet<IpAddr> = server_ips.iter().copied().collect();
    for inst in by_node.values() {
        if let Some(a) = inst.relay_local {
            set.insert(a.ip());
        }
        if let Some(a) = inst.relay_dst {
            set.insert(a.ip());
        }
        if let Some(a) = inst.public_direct_dst {
            set.insert(a.ip());
        }
    }
    // P7 — a DERP carrier's `relay_parts` hold SYNTHETIC `127.x.y.z`
    // placeholder addresses (pubkey-derived, non-routable); they must never
    // become exemption routes via the physical gateway. DERP's real transport
    // is the server WS, whose IPs are already in `server_ips`.
    set.retain(|ip| !ip.is_loopback());
    set
}

/// P5 — the exit peer's WG `allowed_ips` while it carries this node's default
/// egress: its own real (non-default) advertised subnets UNIONed with the two
/// v4 split-default halves, so packets to any non-overlay v4 destination
/// encapsulate to it (the `/1`s) while its `/32` host route + any real subnets
/// keep winning by longest-prefix for their own ranges. A peer that advertised
/// only `0.0.0.0/0` yields exactly the two `/1`s. Pure.
pub(super) fn exit_peer_allowed_ips(exit: &NetmapPeer) -> Vec<crate::overlay::router::Cidr> {
    let mut cidrs: Vec<crate::overlay::router::Cidr> = peer_config_from_netmap(exit)
        .map(|c| c.subnets)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| !c.is_default_route())
        .collect();
    // unwrap: both are valid /1 CIDR literals (const-correct, covered by tests).
    cidrs.push(crate::overlay::router::Cidr::parse(SPLIT_DEFAULT_V4[0]).unwrap());
    cidrs.push(crate::overlay::router::Cidr::parse(SPLIT_DEFAULT_V4[1]).unwrap());
    cidrs
}

/// P5 — is the chosen exit peer ready to carry this node's default egress?
/// `Ok((id, np, pubkey))` when it is present in the netmap, admin-APPROVED
/// (`peer_is_approved_exit`), AND has a live carrier; else `Err(reason)` — the
/// operator-facing split-tunnel reason surfaced in [`ExitNodeStatus`] (S4). Pure,
/// so the reason mapping is unit-tested directly.
pub(super) fn exit_readiness(
    selector: &str,
    current_peers: &HashMap<ObjectId, NetmapPeer>,
    by_node: &HashMap<ObjectId, Installed>,
) -> Result<(ObjectId, NetmapPeer, [u8; 32]), &'static str> {
    let id = resolve_exit_peer(selector, current_peers)
        .ok_or("exit node not visible in the mesh yet")?;
    let np = current_peers
        .get(&id)
        .ok_or("exit node not visible in the mesh yet")?;
    if !peer_is_approved_exit(np) {
        return Err("not an admin-approved exit node (no 0.0.0.0/0 approved)");
    }
    let inst = by_node
        .get(&id)
        .ok_or("exit node has no live carrier yet")?;
    Ok((id, np.clone(), inst.pubkey))
}

/// P5 — the LocalAPI [`ExitNodeStatus`] for the daemon view (S4), or `None` when
/// this node isn't an exit-node client. `active` mirrors the installed
/// split-default; `withheld_reason` is surfaced only while inactive (a stale
/// reason left on the state is suppressed once routing is active). Pure.
pub(super) fn exit_node_status(
    selector: Option<&str>,
    state: &ExitRoutingState,
) -> Option<ExitNodeStatus> {
    let selector = selector?.to_string();
    Some(ExitNodeStatus {
        selector,
        active: state.split_default_installed,
        withheld_reason: if state.split_default_installed {
            None
        } else {
            state.withheld_reason.clone()
        },
        // S3b — global IPv6 also routes through the exit only when v4 is active
        // AND v6 egress was enabled (v6 exemptions pinned). Otherwise v6 is
        // fail-closed even while v4 egress is active.
        v6_active: state.split_default_installed && state.v6_active == Some(true),
        // S4b — DNS steered through the exit only while v4 egress is active.
        dns_steered: state.split_default_installed && state.dns_steered,
    })
}

/// S4 — record the split-tunnel WITHHELD reason on the state and log it ONCE per
/// reason change (dedup on `state.withheld_reason`), so a persistently-withheld
/// exit config doesn't spam the log every reconcile while still surfacing each
/// distinct cause. The live reason is also exposed via [`ExitNodeStatus`] for
/// `roomler status`.
pub(super) fn note_withheld(state: &mut ExitRoutingState, selector: &str, reason: &'static str) {
    if state.withheld_reason.as_deref() != Some(reason) {
        warn!(
            exit = %selector, reason,
            "overlay exit-node: default routing WITHHELD (split-tunnel safety) — egress stays on the local uplink"
        );
        state.withheld_reason = Some(reason.to_string());
    }
}

/// P5 exit-node — live state of default-route capture, owned by [`run`]'s loop.
#[derive(Default)]
pub(super) struct ExitRoutingState {
    /// The exit peer currently carrying our egress, once chosen + reachable +
    /// carriered + approved. `None` when inactive.
    pub(super) active_peer: Option<ObjectId>,
    /// `/32` (host) exemptions currently pinned via the original gateway — so we
    /// add only NEW ones per reconcile and revert exactly on teardown.
    pub(super) exemptions: HashSet<IpAddr>,
    /// Whether the v4 split-default (`0.0.0.0/1`+`128.0.0.0/1`) is installed.
    pub(super) split_default_installed: bool,
    /// S4 — why default routing is currently WITHHELD (the split-tunnel signal),
    /// surfaced in [`ExitNodeStatus`]. `None` when active or not configured. Also
    /// the dedup key for the withhold WARN (log only on a reason change).
    pub(super) withheld_reason: Option<String>,
    /// S3b — global IPv6 egress state: `None` = undecided; `Some(true)` = v6
    /// routes through the exit; `Some(false)` = v6 fail-closed (no v6 uplink to
    /// exempt the coordination server, or a Windows exit). Reset to `None` on
    /// teardown. Independent of `split_default_installed` (v4) per BLOCKER-1 — v6
    /// never gates v4. Also the transition-log dedup key.
    pub(super) v6_active: Option<bool>,
    /// S4b — exit-node DNS steering context + state. `dns_magic_domain`: `Some` =
    /// MagicDNS on (steer "." at the local resolver `dns_target` == self_v4, which
    /// forwards to the network upstream via the exit); `None` = MagicDNS off (steer
    /// "." at `dns_target` == the public upstream directly). `dns_bound`: the local
    /// resolver bound :53 (only gates the MagicDNS-on steer — steering at a dead
    /// resolver would blackhole ALL DNS). `dns_steered`: the "." catch-all steer is
    /// currently installed (⇒ `split_default_installed`, locked by a debug-assert).
    pub(super) dns_magic_domain: Option<String>,
    pub(super) dns_target: Option<Ipv4Addr>,
    pub(super) dns_bound: bool,
    pub(super) dns_steered: bool,
    /// The node a NAME selector first resolved to, this process lifetime.
    /// Overlay names are RELEASED when a device is removed and can be taken by a
    /// different machine, so once we have routed our whole egress through one we
    /// refuse to follow the same label elsewhere — see
    /// [`exit_selector_drifted`]. `None` for a hex selector (unambiguous) or
    /// before the first successful resolution.
    pub(super) pinned_exit: Option<ObjectId>,
}

impl OverlayRuntime {
    /// Phase 1 — register a peer's approved subnet routes in the crypto-router
    /// (so packets to those CIDRs encapsulate to it) and install the matching OS
    /// routes via the overlay NIC. No-op when the peer advertised none.
    pub(super) async fn install_subnets(
        &self,
        wg: &mut WgDevice,
        tun: &Arc<dyn TunIo>,
        node_id: ObjectId,
        pubkey: [u8; 32],
        subnets: &[crate::overlay::router::Cidr],
    ) {
        // P5/A1 — the generic subnet-install path NEVER installs a default route.
        // Approving `0.0.0.0/0` on an exit node fans it into every peer's netmap
        // `routes`; without this filter each client would install it here
        // unconditionally — into the crypto-router's allowed_ips AND an OS default
        // route — hijacking the whole fleet's egress with zero opt-in. Default
        // routing toward a CHOSEN exit node is a separate, opt-in path
        // (split-default `/1`s + carrier-endpoint exemptions) that never flows
        // through this generic installer.
        let filtered: Vec<crate::overlay::router::Cidr> = subnets
            .iter()
            .copied()
            .filter(|c| !c.is_default_route())
            .collect();
        if filtered.len() != subnets.len() {
            warn!(
                peer = %node_id,
                "overlay: dropped advertised default route(s) from a peer's subnets \
                 (exit-node routing is opt-in — a /0 is never auto-installed)"
            );
        }
        wg.set_peer_subnets(pubkey, &filtered);
        for c in &filtered {
            let cidr = c.to_string();
            if let Err(e) = tun.add_cidr_route(&cidr).await {
                debug!(peer = %node_id, %cidr, %e, "overlay: subnet route not installed");
            } else {
                info!(peer = %node_id, %cidr, "overlay: subnet route installed (router peer)");
            }
        }
    }

    /// P5 exit-node — reconcile default-route capture toward the chosen exit
    /// peer. No-op unless `exit_node` is configured. Idempotent + safe to call
    /// after every carrier change: it pins any newly-needed carrier exemptions
    /// FIRST, and only once EVERY required endpoint is exempted does it install
    /// the split-default — so a missing exemption can never sever the very tunnel
    /// that carries the mesh + coordination path (the load-bearing bootstrap
    /// safety, R1/D3). Withdraws the capture (egress reverts to the never-deleted
    /// OS default) if the chosen peer leaves, loses its carrier/approval, or an
    /// exemption can't be pinned.
    pub(super) async fn reconcile_exit_routing(
        &self,
        wg: &mut WgDevice,
        tun: &Arc<dyn TunIo>,
        by_node: &HashMap<ObjectId, Installed>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
        state: &mut ExitRoutingState,
    ) {
        let Some(selector) = self.exit_node.as_deref() else {
            return; // not an exit-node client — inert
        };

        // The chosen exit must be present, reachable-with-a-live-carrier, AND an
        // admin-approved exit node. Any miss → withdraw the capture, record the
        // (distinct) split-tunnel reason for `roomler status`, and wait.
        let (exit_id, exit_np, exit_pubkey) = match exit_readiness(selector, current_peers, by_node)
        {
            Ok(v) => v,
            Err(reason) => {
                if state.split_default_installed {
                    self.teardown_exit_routing(wg, tun, by_node, current_peers, state)
                        .await;
                }
                note_withheld(state, selector, reason);
                return;
            }
        };

        // A NAME selector that now resolves somewhere else. Overlay names are
        // released on device removal and reusable, so following the label would
        // hand this host's whole internet egress to a machine the operator never
        // chose. Fail closed and say what to do about it.
        if exit_selector_drifted(state.pinned_exit, selector, exit_id) {
            if state.split_default_installed {
                self.teardown_exit_routing(wg, tun, by_node, current_peers, state)
                    .await;
            }
            note_withheld(
                state,
                selector,
                "exit-node name now resolves to a DIFFERENT machine — set overlay_exit_node to the node-id hex to pin it",
            );
            return;
        }
        state.pinned_exit = Some(exit_id);

        // Pin any exemptions we don't yet hold, BEFORE (re)installing the /1s —
        // the coturn set grows as relay carriers appear, so re-run on churn.
        let want = exit_exemption_set(&self.exit_server_ips, by_node);
        for ip in &want {
            if state.exemptions.contains(ip) {
                continue;
            }
            match tun.add_host_exemption(*ip).await {
                Ok(()) => {
                    state.exemptions.insert(*ip);
                    info!(%ip, "overlay exit-node: pinned carrier-endpoint exemption via the original uplink");
                }
                Err(e) => {
                    warn!(%ip, %e, "overlay exit-node: FAILED to pin a carrier exemption — withholding default routing to avoid a self-wedge");
                }
            }
        }

        // BLOCKER-1 safety gate — the v4 split-default gates ONLY on the v4
        // exemptions (server A-records + coturn workers). A v6-exemption failure
        // (e.g. roomler.ai has an AAAA but this host has no v6 default route, so
        // its `/128` can't pin) must NEVER withhold v4 exit — v6 is handled
        // separately below and simply stays fail-closed. Without this split, a
        // pure-v6 feature would regress shipped v4 the moment roomler.ai gained
        // an AAAA.
        let v4_ok = want
            .iter()
            .filter(|ip| ip.is_ipv4())
            .all(|ip| state.exemptions.contains(ip));
        if !v4_ok {
            if state.split_default_installed {
                self.teardown_exit_routing(wg, tun, by_node, current_peers, state)
                    .await;
            }
            note_withheld(
                state,
                selector,
                "carrier-endpoint exemption unavailable (no original default route?)",
            );
            return;
        }

        // Install (or move) the split-default toward the exit peer (idempotent):
        // the two v4 /1 halves into its WG allowed_ips + as OS routes, plus the
        // two v6 /1 halves as OS routes into the overlay NIC. The v6 halves either
        // FORWARD (v6 egress) or blackhole (fail-closed) depending on `v6_exit`,
        // set below — the routes themselves are identical either way.
        if !(state.split_default_installed && state.active_peer == Some(exit_id)) {
            let allowed = exit_peer_allowed_ips(&exit_np);
            wg.set_peer_subnets(exit_pubkey, &allowed);
            for cidr in SPLIT_DEFAULT_V4.iter().chain(SPLIT_DEFAULT_V6.iter()) {
                if let Err(e) = tun.add_cidr_route(cidr).await {
                    warn!(%cidr, %e, "overlay exit-node: split-default route not installed");
                }
            }
            state.split_default_installed = true;
            state.active_peer = Some(exit_id);
            state.withheld_reason = None;
            info!(peer = %exit_id, exit = %selector, "overlay exit-node: v4 default egress now routes through the exit peer");
        }

        // S3b — global IPv6 egress, INDEPENDENT of v4 and re-asserted every
        // reconcile so a `remove_peer`-clear during a relay↔direct carrier
        // reinstall self-repairs (MAJOR-3). Enable only when EVERY v6 exemption
        // (the coordination server's AAAA) is pinned, so the WS-over-v6 control
        // channel stays direct (MAJOR-1). Otherwise `v6_exit=None` keeps the `::/1`
        // routes as a fail-closed blackhole — v6 never leaks, and v4 is unaffected.
        let v6_ok = want
            .iter()
            .filter(|ip| ip.is_ipv6())
            .all(|ip| state.exemptions.contains(ip));
        if v6_ok {
            wg.set_v6_exit(Some(exit_pubkey));
            if state.v6_active != Some(true) {
                state.v6_active = Some(true);
                info!(peer = %exit_id, "overlay exit-node: global IPv6 egress now routes through the exit peer");
            }
        } else {
            wg.set_v6_exit(None);
            if state.v6_active != Some(false) {
                state.v6_active = Some(false);
                warn!(exit = %selector, "overlay exit-node: IPv6 egress WITHHELD (no v6 uplink to exempt the coordination server) — v6 stays fail-closed while v4 routes through the exit");
            }
        }

        // S4b — exit-node DNS steering, coupled to the v4 split-default so DNS can
        // never route to the exit while egress doesn't. Idempotent (`!dns_steered`).
        // When MagicDNS is on, gated on a live local resolver (`dns_bound`, known
        // before the first reconcile) — steering "." at a dead :53 would blackhole
        // ALL DNS. A not-bound resolver is left unsteered (working local DNS beats a
        // blackhole) and surfaced via `dns_steered=false` in `roomler status`.
        if state.split_default_installed
            && !state.dns_steered
            && (state.dns_magic_domain.is_none() || state.dns_bound)
            && let Some(target) = state.dns_target
        {
            if dns::steer_default_dns(target, state.dns_magic_domain.as_deref()).await {
                state.dns_steered = true;
                info!(exit = %selector, "overlay exit-node: DNS now steers all queries through the exit (no DNS leak)");
            } else {
                debug!(exit = %selector, "overlay exit-node: DNS steer command failed (resolvectl/NRPT unavailable?) — DNS NOT steered");
            }
        }
        debug_assert!(
            !state.dns_steered
                || (state.split_default_installed
                    && (state.dns_magic_domain.is_none() || state.dns_bound)),
            "exit-node DNS steered without an active split-default + a live local resolver"
        );
    }

    /// P5 exit-node — revert everything [`reconcile_exit_routing`] installed:
    /// drop the split-default OS routes, reset the (former) exit peer's WG
    /// `allowed_ips` back to its real subnets (so it keeps working as a normal /
    /// subnet-router peer), and remove the carrier exemptions. Idempotent. NB:
    /// `process::exit` paths (watchdog stall, self-update) bypass this — a
    /// synchronous pre-exit cleanup + a boot-time stale-route reconciler are the
    /// A2 follow-up (S3.5); the split-default self-heals regardless (the OS
    /// default was never deleted).
    pub(super) async fn teardown_exit_routing(
        &self,
        wg: &mut WgDevice,
        tun: &Arc<dyn TunIo>,
        by_node: &HashMap<ObjectId, Installed>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
        state: &mut ExitRoutingState,
    ) {
        if !state.split_default_installed
            && state.exemptions.is_empty()
            && state.active_peer.is_none()
            && !state.dns_steered
        {
            return;
        }
        for cidr in SPLIT_DEFAULT_V4.iter().chain(SPLIT_DEFAULT_V6.iter()) {
            tun.del_cidr_route(cidr).await;
        }
        // Reset the former exit peer's allowed_ips to its real subnets, if it's
        // still installed + in the netmap (else its Tunn is already gone).
        if let Some(id) = state.active_peer
            && let (Some(inst), Some(np)) = (by_node.get(&id), current_peers.get(&id))
        {
            let real: Vec<crate::overlay::router::Cidr> = peer_config_from_netmap(np)
                .map(|c| c.subnets)
                .unwrap_or_default()
                .into_iter()
                .filter(|c| !c.is_default_route())
                .collect();
            wg.set_peer_subnets(inst.pubkey, &real);
        }
        for ip in state.exemptions.drain() {
            tun.del_host_exemption(ip).await;
        }
        // S3b — stop routing global v6 to the (now former) exit; global v6 reverts
        // to the physical uplink once the `::/1` routes above are removed.
        wg.set_v6_exit(None);
        // S4b — revert DNS steering (drop the "." catch-all). With MagicDNS on the
        // P2 suffix rule stays, so overlay names keep resolving; otherwise the
        // physical resolver is restored.
        if state.dns_steered {
            dns::unsteer_default_dns(state.dns_magic_domain.as_deref()).await;
            state.dns_steered = false;
        }
        state.split_default_installed = false;
        state.active_peer = None;
        state.v6_active = None;
        info!("overlay exit-node: default routing torn down; egress reverted to the local uplink");
    }
}
