//! Peer-establishment half of [`OverlayRuntime`] — split out of `runtime.rs`
//! (rc.284, pure move): the direct-tier LAN context, the netmap
//! install/evict pipeline, make-before-break upgrade probes, the
//! direct/public/srflx installers, and the carrier-health sweep. A child
//! module of `runtime`, so the moved code keeps private-field access and
//! `use super::*` inherits the parent's import block unchanged.

use super::*;

/// rc.131/132/143 — direct LAN carrier context: one UDP socket per LAN
/// interface (each bound to that interface IP — rc.143), this node's LAN IPs
/// across ALL interfaces (for the same-subnet test), and the `IP:port`
/// endpoints we advertise (one per interface socket) so a multi-homed peer can
/// reach us on whichever subnet it shares with us.
pub(super) struct DirectCtx {
    /// One UDP socket bound to EACH usable LAN interface IP (rc.143 — NOT
    /// `0.0.0.0`). Binding to the specific address forces egress out that NIC,
    /// so a same-subnet peer is reached over the LAN even when a full-tunnel VPN
    /// has hijacked the default route (a `0.0.0.0` socket sent the reply out the
    /// VPN and the peer never got it). A peer is served by the socket whose
    /// interface IP shares its /24.
    pub(super) socks: Vec<(Ipv4Addr, Arc<UdpSocket>)>,
    pub(super) my_ips: Vec<Ipv4Addr>,
    pub(super) endpoints: Vec<String>,
    /// Phase A (`public_direct_enabled`) — a single `0.0.0.0:0` socket used to
    /// DIAL a peer's public endpoint. Unbound to any interface so the OS routing
    /// table picks the egress NIC for each public destination (a per-interface
    /// socket bound to a private LAN IP would need us to know which NIC holds
    /// the default route on a multi-homed host). Its demux loop catches the
    /// exit's replies (keyed by the exit's public source). `None` when the
    /// public-direct tier is off. We do NOT advertise this socket's address (a
    /// peer reaches US on our per-interface PUBLIC socket, already advertised in
    /// `endpoints` since a public NIC IP passes `is_usable_lan_ipv4`).
    pub(super) public_sock: Option<Arc<UdpSocket>>,
    /// Phase C — the interface socket that owns our FIRST advertised srflx
    /// candidate (`srflx_endpoints[0]`), paired with that candidate string. To
    /// hole-punch a NAT'd peer we must dial its srflx from THIS socket, so our
    /// outbound WG INITs ride the same NAT mapping we advertised (opening our
    /// filter toward the peer). Distinct from `public_sock` (the Phase A
    /// public-NIC dialer, an unbound `0.0.0.0` socket): a punch requires the
    /// mapping-owning socket, not an arbitrary egress one. `None` when the srflx
    /// tier is off or no public srflx was gathered. Set after the startup
    /// srflx gather (which returns each candidate with its socket).
    pub(super) punch: Option<(String, Arc<UdpSocket>)>,
    /// Phase C — OUR probed NAT mapping type (`"cone"` / `"symmetric"`), or
    /// `None` when unknown. Set at the startup gather (probing the punch socket
    /// against two STUN targets). `install_peers` reads it to skip a srflx punch
    /// only when BOTH ends are symmetric.
    pub(super) my_nat: Option<String>,
}

/// rc.225 — did a netmap upsert change the peer's DIRECT-dial endpoints (its
/// LAN addresses or srflx mappings)? Deliberately ignores `endpoints` (the
/// relay-advert bucket — it churns on every re-allocation and says nothing
/// about direct reachability). A `true` here resets the peer's monitor state
/// (`PathMonitor::on_endpoint_change` — penalties, strikes AND quality): new
/// ports/addresses = new dial conditions, so the old evidence is stale. Pure.
pub(super) fn direct_endpoints_changed(old: &NetmapPeer, new: &NetmapPeer) -> bool {
    old.lan_endpoints != new.lan_endpoints || old.srflx_endpoints != new.srflx_endpoints
}

/// P3 PR-B — one peer's direct-tier candidates (extracted verbatim from
/// `install_peers`, where PR-A computed them inline). PR-E: these ARE the
/// dial targets now — the monitor's `decide()` is the only selection filter
/// on top. Endpoint / feature-flag / hairpin checks are availability, never
/// score. Pure reads.
pub(super) struct DirectCandidates {
    /// Same-subnet LAN endpoint (highest-priority tier): (our matching
    /// interface IP, the peer's dst). rc.204 — scans the provenance-pure
    /// `lan_endpoints` bucket (the peer's join-time NIC sockets), NOT the
    /// `endpoints` union: the union also carries the peer's trickled
    /// coturn-RELAYED addresses, and on this fleet the coturn workers ride
    /// the hosts' own public IPs, so a fleet host same-/24-matched a peer's
    /// *relay allocation* and "LAN"-dialed coturn forever (field-observed
    /// 2026-07-21: mars dialing NEO16's relayed 94.130.141.74:* as a LAN
    /// endpoint).
    pub(super) lan: Option<(Ipv4Addr, std::net::SocketAddr)>,
    /// Phase A — the peer's PUBLIC NIC endpoint (its join-time bucket),
    /// dialable WITHOUT STUN because the peer's NIC holds a public IP.
    /// Dialed over the shared `public_sock` (arbitrary egress is fine — the
    /// peer has no NAT filter). Gated by the feature flag + the egress
    /// socket's existence.
    pub(super) public: Option<std::net::SocketAddr>,
    /// Phase C — the peer's srflx (its STUN-learned public NAT mapping),
    /// dialed over the PUNCH socket (the one that owns OUR advertised srflx)
    /// so our INITs ride our advertised mapping and open our NAT's filter
    /// toward the peer — the mutual hole-punch. Skipped when BOTH ends are
    /// symmetric (a punch can't work — save the futile 12 s attempt + the
    /// strike) and for the P8 same-NAT hairpin with a LAN candidate present.
    /// Lowest direct tier.
    pub(super) srflx: Option<std::net::SocketAddr>,
}

pub(super) fn resolve_direct_candidates(
    direct_ctx: Option<&DirectCtx>,
    cfg: &PeerConfig,
) -> DirectCandidates {
    let lan = direct_ctx
        .and_then(|ctx| direct::pick_same_subnet_endpoint(&ctx.my_ips, &cfg.lan_endpoints));
    let public = direct_ctx.and_then(|ctx| {
        (direct::public_direct_enabled() && ctx.public_sock.is_some())
            .then(|| direct::pick_public_endpoint(&ctx.my_ips, &cfg.lan_endpoints))
            .flatten()
    });
    let srflx = direct_ctx.and_then(|ctx| {
        (direct::srflx_enabled()
            && ctx.punch.is_some()
            && direct::srflx_punch_worth_trying(ctx.my_nat.as_deref(), cfg.srflx_nat.as_deref())
            && !direct::srflx_hairpin_pointless(
                ctx.punch.as_ref().map(|(s, _)| s.as_str()),
                &cfg.srflx_endpoints,
                direct::pick_same_subnet_endpoint(&ctx.my_ips, &cfg.lan_endpoints).is_some(),
            ))
        .then(|| direct::pick_public_endpoint(&ctx.my_ips, &cfg.srflx_endpoints))
        .flatten()
    });
    DirectCandidates { lan, public, srflx }
}

/// bind-to-interface-by-route (Phase 1, `OVERLAY_BIND_BY_ROUTE`) — pick the
/// egress socket for a LAN direct carrier/probe to `dst`. Tailscale's
/// `net/netns` `bindToInterfaceByRoute` adapted to roomler's per-interface
/// sockets: when the gate is on, the OS route table is consulted via the
/// `connect()`-trick ([`direct::os_src_ip_for`]) + [`direct::classify_egress`]
/// so the chosen socket matches the interface that actually reaches `dst` (an
/// on-link `/24` beats a full-tunnel VPN's `/1` split-default), and it is
/// re-pinned to its CURRENT ifindex (fresh — a VPN connect since startup can't
/// leave a stale `IP_UNICAST_IF` pin). Falls back to the same-subnet pick
/// (today's behaviour) when the gate is off, the query is inconclusive, or the
/// OS routes `dst` off our interfaces; the working relay + the MBB probe
/// machinery catch a one-way path from there, so this never skips direct.
/// `None` only when no socket is bound for the subnet at all.
pub(super) async fn lan_egress_socket(
    ctx: &DirectCtx,
    local_ip: Ipv4Addr,
    dst: std::net::SocketAddr,
) -> Option<Arc<UdpSocket>> {
    let pick = |ip: Ipv4Addr| {
        ctx.socks
            .iter()
            .find(|(i, _)| *i == ip)
            .map(|(_, s)| s.clone())
    };
    let same_subnet = pick(local_ip);
    if !direct::bind_by_route_enabled() {
        return same_subnet;
    }
    let socket_ips: Vec<Ipv4Addr> = ctx.socks.iter().map(|(ip, _)| *ip).collect();
    let chosen_ip = match direct::classify_egress(direct::os_src_ip_for(dst).await, &socket_ips) {
        direct::Egress::Use(ip) => {
            if ip != local_ip {
                info!(%local_ip, chosen = %ip, %dst, "overlay: bind-by-route picked the OS-routed egress interface (≠ the same-subnet pick)");
            }
            ip
        }
        direct::Egress::Foreign => {
            info!(%local_ip, %dst, "overlay: bind-by-route — OS routes this LAN dst off our interfaces (VPN-captured?); keeping same-subnet socket (relay/MBB catches a one-way path)");
            local_ip
        }
        direct::Egress::Loop => {
            info!(%local_ip, %dst, "overlay: bind-by-route — route resolves into our own overlay TUN; keeping same-subnet socket");
            local_ip
        }
        direct::Egress::Unknown => local_ip,
    };
    let sock = pick(chosen_ip).or(same_subnet)?;
    // Re-pin egress to the CURRENT ifindex of the chosen interface (freshly
    // read — the pin computed at `setup_direct` can go stale after a VPN
    // connect). No-op off Windows / when `if-addrs` supplies no index.
    if let Some(ix) = direct::ifindex_for(chosen_ip) {
        direct::force_egress_interface(&sock, ix);
    }
    Some(sock)
}

/// rc.208 — an in-flight make-before-break upgrade probe. The candidate direct
/// carrier lives as a shadow `Tunn` in [`WgDevice::probes`] (keyed by `pubkey`);
/// THIS is the runtime-side metadata the promote/expire sweep needs. While it is
/// present, `by_node[node]` still points at the peer's ACTIVE (relay) carrier —
/// routing is untouched — and [`OverlayRuntime::sweep_upgrade_probes`] either
/// promotes it (handshake latched → cut over to direct) or drops it (past the
/// tier's [`DirectTier::handshake_deadline`] → keep relay). See
/// [`crate::overlay::direct::make_before_break_enabled`].
pub(super) struct UpgradeProbe {
    pub(super) pubkey: [u8; 32],
    pub(super) overlay_ip: Ipv4Addr,
    /// The direct endpoint the probe dials (the promoted carrier's off-link
    /// exit-exemption dst for `Public`/`Srflx`).
    pub(super) dst: std::net::SocketAddr,
    /// Which direct tier is being probed — drives the deadline + CC1 cooldown.
    pub(super) tier: DirectTier,
    /// When the probe was started — for the tier handshake deadline.
    pub(super) since: Instant,
    /// rc.276 diagnostics — `true` for an outbound MBB upgrade probe (we
    /// dialed), `false` for an accepted inbound dial held as a probe. Carried
    /// into `Installed.initiated` on promote.
    pub(super) initiated: bool,
    /// rc.276 diagnostics — the probe socket's local address, carried into
    /// `Installed.carrier_local` on promote.
    pub(super) local: Option<std::net::SocketAddr>,
}

impl OverlayRuntime {
    /// rc.137/139 — find carriers that are one-way / dead and repair them.
    /// Health is LOCK-FREE: each sweep snapshots `(tx, rx)` (atomic reads — no
    /// `Tunn` lock, so it can't stall the packet path like the rc.136
    /// handshake-age check did); a carrier where **tx climbed but rx stayed
    /// flat** for [`BAD_SWEEPS_TO_FALLBACK`] consecutive sweeps is dead (we're
    /// sending, nothing comes back). The repair depends on the carrier kind:
    /// - **direct** → fall back to relay (the LAN path only LOOKED viable —
    ///   corp VPN route hijack, Wi-Fi AP/client isolation, asymmetric firewall);
    ///   the monitor's suppression penalty keeps the next netmap from
    ///   re-upgrading it (PR-E: `PathMonitor::on_death` is the bookkeeping).
    /// - **relay** (rc.139) → refresh it: the peer almost certainly
    ///   re-allocated its coturn port (restart/churn) and we're dialing a stale
    ///   one. Re-request so we re-allocate + re-dial the peer's CURRENT address
    ///   ([`RELAY_REFRESH_COOLDOWN`] bounds two ends ping-ponging).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn sweep_carrier_health(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        relay_refresh_cooldown: &mut HashMap<ObjectId, Instant>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
    ) {
        let now = Instant::now();
        // P3 PR-A — the shadow monitor consumes the same env read the legacy
        // escalation table does, at the same moment.
        let mbb = crate::overlay::direct::make_before_break_enabled();
        let mut dead: Vec<(ObjectId, DirectTier, DeathReason)> = Vec::new();
        for (nid, e) in by_node.iter_mut() {
            let Some((tx, rx)) = wg.peer_traffic(&e.pubkey) else {
                continue;
            };
            let (last_tx, last_rx) = e.last_traffic;
            e.last_traffic = (tx, rx);
            // P3b-3 / rc.206 — "last heard from this peer" advances on ANY
            // authenticated inbound packet, INCLUDING content-free WG keepalives.
            // The IP-data `rx` counter alone froze on a mostly-idle-but-alive
            // carrier (its only inbound is keepalives → `TunnResult::Done`, which
            // never touches `rx`), so the rx-staleness rule would have reaped a
            // healthy idle link. `peer_take_rx_any` drains the
            // keepalive-inclusive liveness counter (single-consumer; the sweep is
            // the only reader). Advance BEFORE building the tick inputs so a
            // freshly installed peer's first inbound already registers.
            let heard = wg.peer_take_rx_any(&e.pubkey) > 0;
            if heard {
                e.last_rx_at = now;
            }
            // The peer exists (peer_traffic answered just above, no await in
            // between), so the latch read always answers too — the `else` is
            // unreachable belt-and-braces.
            let Some(handshake_done) = wg.peer_handshake_done(&e.pubkey) else {
                continue;
            };
            // P2 — every rule that can kill this carrier lives in ONE pure
            // transition (`lifecycle::carrier_tick`): the per-tier handshake
            // deadline (Phase C / rc.204 / rc.223), the rc.206 rx-staleness
            // backstop, the rc.137 one-way counter, the rc.181 hard-dead fast
            // path, the warm-up grace, and the relay refresh holdoff. This
            // loop only gathers inputs and applies the verdict.
            let v = carrier_tick(&HealthInputs {
                tier: e.tier,
                is_direct: e.is_direct,
                hard_dead: wg.peer_carrier_dead(&e.pubkey).unwrap_or(false),
                handshake_done,
                since_install: e.since.elapsed(),
                since_last_rx: now.saturating_duration_since(e.last_rx_at),
                traffic: (tx, rx),
                last_traffic: (last_tx, last_rx),
                bad_sweeps: e.bad_sweeps,
                relay_refresh_held: relay_refresh_cooldown
                    .get(nid)
                    .is_some_and(|&until| until > now),
            });
            // P3 PR-A — feed the sweep's evidence to the shadow monitor:
            // heard-this-sweep (Q credit), a stored one-way strike (Q debit),
            // and the strike-clear (a direct carrier genuinely receiving also
            // grades any recent monitor refusal of this tier as harmful).
            {
                let tier = e.tier;
                let old_bad = e.bad_sweeps;
                self.shadow(|s| {
                    if heard {
                        s.mon.on_heard(nid, tier);
                    }
                    if v.bad_sweeps > old_bad {
                        s.mon.on_bad_sweep(nid, tier);
                    }
                    if v.clear_tier_strikes {
                        s.mon.on_healthy_rx(nid, tier);
                        s.establishment(nid, tier, now);
                    }
                });
            }
            e.bad_sweeps = v.bad_sweeps;
            // rc.275 honesty — stamp the display verdict (CLI `stalled`);
            // reap decisions stay with `carrier_tick` below.
            e.stalled = crate::overlay::lifecycle::carrier_stalled(
                e.since.elapsed(),
                handshake_done,
                v.bad_sweeps,
            );
            // rc.276 diagnostics — mirror the latch for the peers debug view.
            e.hs_done = handshake_done;
            // PR-E — the strike-clear (CC1: the carrier's OWN tier only)
            // lives in the monitor now (`on_healthy_rx`, fed just above).
            if let Some(reason) = v.death {
                dead.push((*nid, e.tier, reason));
            }
        }
        for (nid, tier, reason) in dead {
            let Some(e) = by_node.remove(&nid) else {
                continue;
            };
            // P3 PR-A — a death is path-selection evidence: the monitor books
            // the strike + suppression penalty (≡ the cooldown insert below)
            // and slams the tier's quality. The tripwire then proves the
            // penalty math actually made the tier ineligible — the sweep-
            // surface half of the shadow comparison (a full decide() compare
            // happens at the next install_peers walk, which is where legacy
            // itself re-selects).
            self.shadow(|s| {
                s.mon.on_death(&nid, tier, reason, mbb, now);
                if tier.is_direct() {
                    s.assert_ineligible(&nid, tier, now);
                }
            });
            wg.remove_peer(&e.pubkey).await;
            // A carrier REPAIR, not an eviction (the relay is re-requested
            // below), so it deliberately does NOT go through `evict_peer` —
            // forgetting the monitor evidence/probe here would undo proven behaviour.
            // It does need the recycled-address guard.
            del_peer_route_if_unowned(tun, by_node, e.overlay_ip).await;
            if tier.is_direct() {
                // PR-E — `PathMonitor::on_death` (fed above) IS the failure
                // bookkeeping now: strike + suppression penalty on the
                // carrier's OWN tier (CC1), escalating to the sticky regime
                // at the same thresholds the legacy maps used. Only the
                // operator-greppable log lines remain here, keyed off the
                // monitor's strike state — strings preserved verbatim.
                let tier_name = match tier {
                    DirectTier::Srflx => "srflx",
                    DirectTier::Public => "public",
                    _ => "LAN",
                };
                let (fails, sticky) = self
                    .shadow(|s| (s.mon.strikes(&nid, tier), s.mon.is_sticky(&nid, tier, mbb)))
                    .unwrap_or((0, false));
                if sticky {
                    warn!(
                        peer = %nid, tier = tier_name, fails,
                        "overlay: direct carrier failed repeatedly — pinning this peer to relay for the session"
                    );
                } else if reason == DeathReason::RxStale {
                    // rc.206 — an ESTABLISHED direct carrier that went silent
                    // (peer roamed / NAT rebind / path died mid-session), not a
                    // never-punched one. Distinct message so field logs separate
                    // "died" from "never established". A re-upgrade re-punches
                    // once the cooldown clears; the fail count usually clears on
                    // the first receiving sweep after that, so a one-off death
                    // doesn't march toward the sticky pin.
                    warn!(
                        peer = %nid, tier = tier_name,
                        "overlay: established direct carrier went silent (no keepalive within the rx-stale deadline — peer roamed / NAT rebind / path died) — rebuilding via relay"
                    );
                } else {
                    warn!(
                        peer = %nid, tier = tier_name,
                        "overlay: direct carrier didn't establish (firewall / VPN / AP-isolation / unpunchable NAT?) — falling back to relay"
                    );
                }
            } else {
                relay_refresh_cooldown.insert(nid, now + RELAY_REFRESH_COOLDOWN);
                // P2 — relay side: HardDead > RxStale > else ("one-way" covers
                // both OneWay and a relay HandshakeDeadline, as pre-P2).
                if reason == DeathReason::HardDead {
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier send hard-errored (TURNS/TCP reset / QUIC-over-TURN lost) — re-allocating"
                    );
                } else if reason == DeathReason::RxStale {
                    // rc.206 — a relay carrier that stopped delivering with no
                    // send-error to trip `hard_dead` (silently-dropped coturn
                    // allocation / a dead worker the send path can't detect).
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier went silent (no keepalive within the rx-stale deadline — coturn allocation dropped?) — re-allocating"
                    );
                } else {
                    warn!(
                        peer = %nid,
                        "overlay: relay carrier one-way (stale coturn port?) — re-allocating"
                    );
                }
            }
            // (Re)request the relay now (don't wait for the next netmap). For a
            // relay refresh we first forget the stale allocation so a fresh one
            // is made; a direct→relay fall has no prior allocation to forget.
            if let (Some(coord), Some(np)) = (relay.as_mut(), current_peers.get(&nid))
                && let Some(cfg) = peer_config_from_netmap(np)
            {
                if !tier.is_direct() {
                    coord.forget(&nid);
                }
                coord.request(nid, cfg).await;
            }
        }
        // P3 PR-A — the 10-min shadow summary rides the 5 s sweep cadence.
        self.shadow(|s| s.maybe_summary(now));
    }

    /// rc.131 — bind the shared direct-carrier socket + discover our LAN
    /// endpoint. Only in Relay mode (Direct mode is the loopback test/helper
    /// path) and when `ROOMLER_AGENT_OVERLAY_DIRECT` isn't disabled. `None` if
    /// disabled, not relay mode, the bind fails, or there's no usable LAN IP
    /// (offline / CGNAT-only) — the node then stays relay-only as before.
    pub(super) async fn setup_direct(&self) -> Option<DirectCtx> {
        if !matches!(self.mode, CarrierMode::Relay) || !direct::direct_enabled() {
            return None;
        }
        let ifaces = direct::gather_lan_interfaces();
        let my_ips: Vec<Ipv4Addr> = ifaces.iter().map(|(ip, _)| *ip).collect();
        if my_ips.is_empty() {
            info!("overlay: no usable LAN interface; direct path off (relay only)");
            return None;
        }
        // rc.143 — bind ONE socket per interface IP (to that IP, not 0.0.0.0);
        // rc.144 — ALSO pin egress to that NIC via IP_UNICAST_IF, because on
        // Windows a source-IP bind alone doesn't force the egress interface (the
        // route does), so a full-tunnel VPN's default route otherwise steals the
        // send and same-WiFi direct oscillates. Advertise each socket's own
        // `ip:port`; the peer dials whichever shares its subnet, and both sides
        // then send/receive over that interface's pinned socket.
        let mut socks: Vec<(Ipv4Addr, Arc<UdpSocket>)> = Vec::new();
        let mut endpoints: Vec<String> = Vec::new();
        for (ip, ifindex) in &ifaces {
            match UdpSocket::bind((*ip, 0)).await {
                Ok(s) => {
                    if let Some(idx) = ifindex {
                        direct::force_egress_interface(&s, *idx);
                    }
                    match s.local_addr() {
                        Ok(local) => {
                            endpoints.push(format!("{ip}:{}", local.port()));
                            socks.push((*ip, Arc::new(s)));
                        }
                        Err(e) => {
                            warn!(%ip, %e, "overlay: direct socket local_addr failed; skipping")
                        }
                    }
                }
                Err(e) => {
                    warn!(%ip, %e, "overlay: bind direct socket on interface failed; skipping")
                }
            }
        }
        if socks.is_empty() {
            info!("overlay: no bindable LAN interface; direct path off (relay only)");
            return None;
        }
        info!(
            endpoints = ?endpoints,
            "overlay: advertising direct LAN endpoints (per-interface sockets; same-subnet peers dial direct)"
        );
        // Phase A/B — a single unbound socket to DIAL peers' public endpoints
        // (the OS picks egress per-destination). Shared by the public-direct
        // tier (peer's public NIC) AND the srflx tier (peer's NAT mapping), so
        // it's bound when EITHER is on. Best-effort: a bind failure just leaves
        // both public-dial tiers off (relay still works).
        let public_sock = if direct::public_direct_enabled() || direct::srflx_enabled() {
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
                Ok(s) => {
                    // VPN-bypass: pin this `0.0.0.0` public/srflx dialer's egress
                    // to the physical uplink so it leaves the real NIC instead
                    // of a full-tunnel corp VPN's captured default.
                    if let Some(ix) = direct::vpn_bypass_ifindex() {
                        direct::force_egress_interface(&s, ix);
                        info!(
                            ifindex = ix,
                            "overlay: VPN-bypass — public-dial egress pinned to the physical uplink"
                        );
                    }
                    info!(
                        public_direct = direct::public_direct_enabled(),
                        srflx = direct::srflx_enabled(),
                        "overlay: public-dial egress socket ON (NAT-traversal Phase A/B)"
                    );
                    Some(Arc::new(s))
                }
                Err(e) => {
                    warn!(%e, "overlay: public-dial egress socket bind failed; public/srflx tiers off");
                    None
                }
            }
        } else {
            None
        };
        Some(DirectCtx {
            socks,
            my_ips,
            endpoints,
            public_sock,
            // Set after the startup srflx gather (Phase C) once we know which
            // interface socket owns our first advertised srflx candidate + the
            // NAT-type probe on it.
            punch: None,
            my_nat: None,
        })
    }

    /// Tear one peer's carrier down: WG peer, crypto-routes, OS `/32`, relay
    /// coordination, queued builds/allocations and any in-flight upgrade probe.
    ///
    /// THE single teardown for every eviction path (delta `removes`, the
    /// full-netmap prune, resume-from-suspend) so the three can't drift apart.
    /// Returns the removed [`Installed`], if the peer was installed at all.
    ///
    /// `by_node.remove` happens FIRST so [`del_peer_route_if_unowned`] sees only
    /// survivors when it decides whether the OS route is still claimed.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn remove_peer_state(
        &self,
        node_id: ObjectId,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        relay: &mut Option<RelayCoordinator>,
        relay_bq: &mut RelayBuildQueue,
        alloc_q: Option<&mut RelayAllocQueue>,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        os_route: PeerRoute,
    ) -> Option<Installed> {
        let removed = by_node.remove(&node_id);
        if let Some(e) = &removed {
            wg.remove_peer(&e.pubkey).await;
            if os_route == PeerRoute::Drop {
                del_peer_route_if_unowned(tun, by_node, e.overlay_ip).await;
            }
        }
        if let Some(r) = relay.as_mut() {
            r.forget(&node_id);
        }
        // rc.211 — drop any in-flight off-loop relay build (stale on arrival).
        relay_bq.invalidate(&node_id);
        if let Some(q) = alloc_q {
            q.invalidate(&node_id);
        }
        // rc.208 — drop any in-flight make-before-break probe (its shadow
        // carrier + demux registration).
        if let Some(pr) = upgrade_probes.remove(&node_id) {
            wg.drop_direct_probe(&pr.pubkey).await;
        }
        removed
    }

    /// A full EVICTION: [`Self::remove_peer_state`] plus the netmap/monitor
    /// bookkeeping that only makes sense when the peer is GONE (as opposed to
    /// being re-installed).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn evict_peer(
        &self,
        node_id: ObjectId,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        relay: &mut Option<RelayCoordinator>,
        relay_bq: &mut RelayBuildQueue,
        alloc_q: &mut RelayAllocQueue,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        current_peers: &mut HashMap<ObjectId, NetmapPeer>,
        relay_refresh_cooldown: &mut HashMap<ObjectId, Instant>,
    ) {
        current_peers.remove(&node_id);
        // P3 PR-A — drop the monitor's per-peer state.
        self.shadow(|s| s.mon.on_peer_removed(&node_id));
        let removed = self
            .remove_peer_state(
                node_id,
                wg,
                by_node,
                tun,
                relay,
                relay_bq,
                Some(alloc_q),
                upgrade_probes,
                PeerRoute::Drop,
            )
            .await;
        // Keyed by node id and never pruned otherwise, so a long-lived daemon
        // accumulated an entry per peer that ever churned (the monitor's
        // per-peer state is dropped by `on_peer_removed` above).
        relay_refresh_cooldown.remove(&node_id);
        if removed.is_some() {
            info!(peer = %node_id, "overlay: peer removed");
        }
    }

    /// Reconcile the netmap into installed peers. NOT-yet-installed: Direct
    /// mode → build the loopback/test carrier; Relay mode → a DIRECT LAN
    /// carrier when the peer advertises a same-subnet endpoint (rc.131/134 — N
    /// peers share one socket via the device's source-address demux), else the
    /// coturn relay coordination. ALREADY-installed on RELAY but a same-subnet
    /// endpoint has since appeared → UPGRADE to direct (rc.134 re-evaluation:
    /// a peer first seen before its endpoint arrived would otherwise stay on
    /// relay forever). A peer whose direct tier is monitor-suppressed (it just
    /// failed — rc.136) is kept on relay regardless of a same-subnet endpoint.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn install_peers(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        peers: &[NetmapPeer],
        direct_ctx: Option<&DirectCtx>,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        relay_bq: &mut RelayBuildQueue,
        trigger: &'static str,
    ) {
        let now = Instant::now();
        // rc.208 — make-before-break: probe a relay→direct upgrade instead of
        // tearing the relay down speculatively. Read once per call.
        let make_before_break = crate::overlay::direct::make_before_break_enabled();
        for np in peers {
            // P9 — presence: never dial / probe / relay-request a peer the
            // server marked unreachable (ghost enrollment, stale heartbeat,
            // clean leave). An already-installed carrier is left alone — the
            // data plane outlives a control-plane hiccup and the health sweep
            // owns its lifecycle; removal still arrives via the leave delta.
            if !np.reachable {
                continue;
            }
            let Some(cfg) = peer_config_from_netmap(np) else {
                continue;
            };
            // PR-E — the candidates ARE the dial targets: tier suppression
            // (rc.136 + CC1 in monitor form — per-tier penalties that never
            // cross-poison, escalating at the same thresholds) lives entirely
            // in `decide()`'s eligibility plane, applied via the action
            // gating below. This also closes the soak-#2 residual coupling
            // (a legacy cooldown withholding a dst the monitor held
            // eligible).
            let cands = resolve_direct_candidates(direct_ctx, &cfg);
            let direct_dst = cands.lan;
            let phase_a_dst = cands.public;
            let srflx_dst = cands.srflx;

            // Copy-out the installed carrier's shape (all Copy), so the by_node
            // borrow ends before any mutation below.
            let installed = by_node
                .get(&np.node_id)
                .map(|e| (e.is_direct, e.pubkey, e.tier, e.public_direct_dst));
            // The node's WG identity CHANGED under a stable node_id: the daemon
            // rotated its key, or the netmap row is the stale remains of a
            // different machine. Either way everything we hold — Tunn,
            // crypto-route, demux registration, relay coordination, in-flight
            // probe — is keyed to the OLD key and is dead. Tear it down and fall
            // through to a clean install; without this the already-installed
            // short-circuits below kept the orphaned peer alive AND kept the old
            // key accepted inbound forever.
            let installed = match installed {
                Some((_, pk, _, _)) if pk != cfg.public_key => {
                    warn!(peer = %np.node_id, overlay_ip = %cfg.overlay_ip,
                        "overlay: peer's WG public key changed — reinstalling its carrier");
                    // No alloc_q here (install_peers doesn't take one) and none
                    // needed: `relay.forget` inside makes a landing `AllocDone`
                    // a no-op for this node.
                    self.remove_peer_state(
                        np.node_id,
                        wg,
                        by_node,
                        tun,
                        relay,
                        relay_bq,
                        None,
                        upgrade_probes,
                        PeerRoute::Keep,
                    )
                    .await;
                    None
                }
                other => other,
            };
            // P3 PR-A — the shadow monitor's answer to the SAME question this
            // walk is about to answer, computed up-front; every decision exit
            // below records the legacy outcome against it (`record`). Legacy
            // stays authoritative — this is instrumentation only. Inert in
            // Direct/test carrier mode and when the monitor is off.
            let avail = path::TierAvailability {
                lan: cands.lan.is_some(),
                public: cands.public.is_some(),
                srflx: cands.srflx.is_some(),
            };
            let incumbent = match installed {
                Some((true, _, tier, _)) => path::Incumbent::Direct(tier),
                Some((false, _, _, _)) => path::Incumbent::Relay,
                None => path::Incumbent::None,
            };
            let monitor_action = matches!(self.mode, CarrierMode::Relay)
                .then(|| {
                    self.shadow(|s| {
                        // B2 — demotion executes only in `on` mode; shadow
                        // counts separately below.
                        let demote_on = s.demote == path::DemoteMode::On;
                        s.mon.decide(
                            &np.node_id,
                            incumbent,
                            avail,
                            make_before_break,
                            now,
                            demote_on,
                        )
                    })
                })
                .flatten();
            let record = |surface: &'static str, legacy: path::PathAction| {
                if let Some(m) = monitor_action {
                    self.shadow(|s| {
                        s.compare(surface, trigger, &np.node_id, Some(legacy), Some(m), now)
                    });
                }
            };
            // P3 PR-C — ON mode: the monitor's decision is AUTHORITATIVE. It
            // gates the legacy walk below instead of duplicating it: for a
            // committed single-tier action only that tier stays dialable, so
            // the same install/probe machinery executes exactly the monitor's
            // choice; Keep/Relay clear every direct tier (fall through to
            // relay / no-op). Two deliberate exemptions:
            // * already-direct incumbents keep their dsts — the D10 re-dial
            //   is execution-layer (dial-target freshness, not selection);
            // * P9 probe-first fresh-LAN keeps ALL dsts — the walk needs the
            //   LAN candidate for the probe AND the fallback tiers for the
            //   working carrier, exactly like legacy.
            // The legacy cooldown state keeps being written throughout (the
            // shadow-revert safety rail); `record` still runs, so pilot
            // volume shows in the summaries (compares are monitor-vs-monitor
            // there — the fleet's shadow mode carries the real divergence
            // telemetry until PR-D).
            // PR-E — the monitor's decision gates the walk UNCONDITIONALLY
            // (there is no legacy selector any more; `PathMonMode` is
            // telemetry-only).
            let (direct_dst, phase_a_dst, srflx_dst) = if monitor_action.is_some() {
                match (incumbent, monitor_action) {
                    (path::Incumbent::Direct(_), _) => (direct_dst, phase_a_dst, srflx_dst),
                    (path::Incumbent::None, Some(path::PathAction::Probe(DirectTier::Lan))) => {
                        (direct_dst, phase_a_dst, srflx_dst)
                    }
                    (_, Some(path::PathAction::Probe(DirectTier::Lan)))
                    | (_, Some(path::PathAction::Install(DirectTier::Lan))) => {
                        (direct_dst, None, None)
                    }
                    (_, Some(path::PathAction::Probe(DirectTier::Public)))
                    | (_, Some(path::PathAction::Install(DirectTier::Public))) => {
                        (None, phase_a_dst, None)
                    }
                    (_, Some(path::PathAction::Probe(DirectTier::Srflx)))
                    | (_, Some(path::PathAction::Install(DirectTier::Srflx))) => {
                        (None, None, srflx_dst)
                    }
                    _ => (None, None, None),
                }
            } else {
                (direct_dst, phase_a_dst, srflx_dst)
            };
            match installed {
                Some((true, pk, tier, inst_dst)) => {
                    // D10 — a zombie srflx punch (installed but never handshook)
                    // whose advertised srflx has since CHANGED: re-dial the fresh
                    // mapping NOW, without booking a strike (the old dst is
                    // known-stale — not evidence the pair can't punch). Otherwise
                    // a srflx re-trickle sits ignored on an already-direct peer
                    // until the handshake deadline tears it down (~100 s later),
                    // and books a bogus strike doing so.
                    if tier == DirectTier::Srflx
                        && !wg.peer_handshake_done(&pk).unwrap_or(true)
                        && let (Some(ctx), Some(fresh)) = (direct_ctx, srflx_dst)
                        && inst_dst != Some(fresh)
                    {
                        // P3 PR-C (soak #1 finding) — a D10 re-dial refreshes
                        // the DIAL TARGET of the current tier (Srflx→Srflx),
                        // so it is NOT a tier-selection event: counted, never
                        // compared (PR-A's compare here produced 50+/host/45 h
                        // of false "divergence"). Permanently execution-layer.
                        self.shadow(|s| s.stats.d10_redials += 1);
                        info!(peer = %np.node_id, old = ?inst_dst, new = %fresh, "overlay: srflx changed under a pending punch — re-dialing fresh mapping");
                        wg.remove_peer(&pk).await;
                        self.install_srflx_direct(wg, by_node, tun, ctx, np.node_id, &cfg, fresh)
                            .await;
                    } else if let Some(path::PathAction::Probe(target)) = monitor_action
                        && target != tier
                        && !wg.has_direct_probe(&pk)
                    {
                        // B2 (on) — voluntary demotion: the monitor found a
                        // sustained margin-sized score deficit against an
                        // eligible alternative. MBB-probe it — the incumbent
                        // keeps carrying traffic until the probe latches
                        // (`sweep_upgrade_probes` promotes tier-agnostically).
                        let probe_target = match target {
                            DirectTier::Lan => {
                                if let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst)
                                {
                                    lan_egress_socket(ctx, local_ip, dst)
                                        .await
                                        .map(|s| (s, dst))
                                } else {
                                    None
                                }
                            }
                            DirectTier::Public => {
                                if let (Some(ctx), Some(dst)) = (direct_ctx, phase_a_dst) {
                                    ctx.public_sock.clone().map(|s| (s, dst))
                                } else {
                                    None
                                }
                            }
                            DirectTier::Srflx => {
                                if let (Some(ctx), Some(dst)) = (direct_ctx, srflx_dst) {
                                    ctx.punch.clone().map(|(_, s)| (s, dst))
                                } else {
                                    None
                                }
                            }
                            DirectTier::Relay => None,
                        };
                        if let Some((sock, dst)) = probe_target {
                            record("install_peers:demote", path::PathAction::Probe(target));
                            info!(
                                peer = %np.node_id, from = ?tier, to = ?target,
                                "overlay pathmon[demote]: sustained score deficit — probing better tier (incumbent held until latch)"
                            );
                            self.start_upgrade_probe(
                                wg,
                                upgrade_probes,
                                np.node_id,
                                &cfg,
                                sock,
                                dst,
                                target,
                                now,
                            )
                            .await;
                        } else {
                            record("install_peers:keep_direct", path::PathAction::Keep);
                        }
                    } else {
                        // B2 shadow — count the would-be demotion without
                        // acting (its own by_class lane + rate-limited log).
                        // The Relay class stays advisory even in `on` mode
                        // (decide returns it, but execution is deferred —
                        // the death path owns actual direct→relay moves).
                        if let path::Incumbent::Direct(cur) = incumbent {
                            self.shadow(|s| {
                                let advisory_relay =
                                    matches!(monitor_action, Some(path::PathAction::Relay));
                                if (s.demote == path::DemoteMode::Shadow || advisory_relay)
                                    && let Some(would) =
                                        s.mon.demote_candidate(&np.node_id, cur, avail, now)
                                {
                                    s.note_shadow_demote(&np.node_id, would, now);
                                }
                            });
                        }
                        // P3 PR-A (F14) — the already-direct fall-through IS a
                        // decision (keep the incumbent); record it so the
                        // shadow compare covers this arm too.
                        record("install_peers:keep_direct", path::PathAction::Keep);
                    }
                    continue; // already direct (LAN / public / srflx)
                }
                Some((false, pk, _, _)) => {
                    // Installed on RELAY — upgrade to the best available direct
                    // tier now that an endpoint has appeared: LAN > public-NIC >
                    // srflx punch.
                    //
                    // rc.208 make-before-break: when enabled, install the
                    // candidate direct carrier as a SHADOW PROBE (keyed by the
                    // same pubkey; its own `Tunn` in `WgDevice::probes`) while the
                    // working relay keeps routing. `sweep_upgrade_probes` cuts
                    // over only once the probe's handshake latches (proof the path
                    // works both ways), and drops it — leaving the relay
                    // untouched — if it never does. This kills the ~15-38 s
                    // per-upgrade freeze the destructive path below caused on a
                    // peer that can only relay (same-NAT AP-isolation / no
                    // hairpin). Skip if a probe for this peer is already in flight.
                    if make_before_break {
                        // Resolve the best available direct tier's (socket, dst):
                        // LAN > public-NIC > srflx punch — same precedence as the
                        // destructive path. Skip if a probe is already in flight.
                        let probe_target = if wg.has_direct_probe(&pk) {
                            None
                        } else if let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst)
                        {
                            // LAN tier — bind-by-route egress selection (Phase 1).
                            lan_egress_socket(ctx, local_ip, dst)
                                .await
                                .map(|s| (s, dst, DirectTier::Lan))
                        } else if let (Some(ctx), Some(dst)) = (direct_ctx, phase_a_dst) {
                            ctx.public_sock
                                .clone()
                                .map(|s| (s, dst, DirectTier::Public))
                        } else if let (Some(ctx), Some(dst)) = (direct_ctx, srflx_dst) {
                            ctx.punch.clone().map(|(_, s)| (s, dst, DirectTier::Srflx))
                        } else {
                            None
                        };
                        if let Some((sock, dst, tier)) = probe_target {
                            record("install_peers:upgrade", path::PathAction::Probe(tier));
                            self.start_upgrade_probe(
                                wg,
                                upgrade_probes,
                                np.node_id,
                                &cfg,
                                sock,
                                dst,
                                tier,
                                now,
                            )
                            .await;
                        } else {
                            // P3 PR-A — no probe target (all tiers cooling /
                            // unavailable, or one already in flight) = keep
                            // the relay; record the decision.
                            record("install_peers:upgrade", path::PathAction::Keep);
                        }
                        continue;
                    }
                    // Pre-rc.208 destructive upgrade (break-before-make): tears the
                    // relay down first, then handshakes over the (unproven) direct
                    // path. Kept as the default until make-before-break is
                    // field-proven per-host.
                    if let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst) {
                        record(
                            "install_peers:upgrade",
                            path::PathAction::Install(DirectTier::Lan),
                        );
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to direct LAN carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, local_ip, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, phase_a_dst) {
                        record(
                            "install_peers:upgrade",
                            path::PathAction::Install(DirectTier::Public),
                        );
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to direct-to-public carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_public_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, srflx_dst) {
                        record(
                            "install_peers:upgrade",
                            path::PathAction::Install(DirectTier::Srflx),
                        );
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to srflx hole-punch carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_srflx_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else {
                        // P3 PR-A — nothing dialable: the relay stays.
                        record("install_peers:upgrade", path::PathAction::Keep);
                    }
                    continue;
                }
                None => {}
            }

            match &self.mode {
                CarrierMode::Direct(links) => {
                    let Some(carrier) = links.build_carrier(&cfg).await else {
                        debug!(peer = %np.node_id, "overlay: no carrier built; retry next netmap");
                        continue;
                    };
                    self.install_ready(
                        wg,
                        by_node,
                        tun,
                        ReadyLink {
                            node_id: np.node_id,
                            public_key: cfg.public_key,
                            overlay_ip: cfg.overlay_ip,
                            carrier,
                            relay_parts: None,
                            supports_quic: cfg.supports_quic,
                            single_relay: None,
                            relay_kind: RelayKind::Turn,
                            subnets: cfg.subnets.clone(),
                        },
                        relay_bq,
                    )
                    .await;
                }
                CarrierMode::Relay => {
                    // P9 — fresh-install LAN is PROBE-FIRST under make-before-
                    // break: a same-/24 match is only a HINT the peer is on
                    // this LAN. Two sites on a vendor-default subnet
                    // (192.168.68.0/24 Deco, 192.168.1.0/24 …) false-match, and
                    // the "same subnet" dial is then dead air — field
                    // 2026-07-28: corp peers advertised another city's
                    // 192.168.68.x and every fresh install burned the 12 s LAN
                    // deadline with NO carrier, repeatedly. So: shadow-probe
                    // the LAN candidate and keep walking the fallback chain
                    // (public → srflx → relay) for the working carrier in the
                    // SAME pass; `sweep_upgrade_probes` cuts over the moment
                    // the probe latches (ms on a genuine LAN) and books CC1 if
                    // it never does. The destructive install remains when
                    // nothing else is dialable (airgapped pure-LAN mesh — a
                    // wrong guess costs nothing there) and when MBB is
                    // env-disabled.
                    let lan_probe_first = make_before_break
                        && direct_dst.is_some()
                        && (phase_a_dst.is_some() || srflx_dst.is_some() || relay.is_some());
                    let mut lan_probing = false;
                    if lan_probe_first {
                        if wg.has_direct_probe(&cfg.public_key) {
                            // In flight from an earlier pass — walk the
                            // fallback chain quietly (the monitor holds Keep
                            // while a probe runs; no compare on this pass).
                            lan_probing = true;
                        } else if let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst)
                            && let Some(sock) = ctx
                                .socks
                                .iter()
                                .find(|(ip, _)| *ip == local_ip)
                                .map(|(_, s)| s.clone())
                        {
                            record(
                                "install_peers:fresh",
                                path::PathAction::Probe(DirectTier::Lan),
                            );
                            self.start_upgrade_probe(
                                wg,
                                upgrade_probes,
                                np.node_id,
                                &cfg,
                                sock,
                                dst,
                                DirectTier::Lan,
                                now,
                            )
                            .await;
                            lan_probing = true;
                        }
                    }
                    if !lan_probing
                        && let (Some(ctx), Some((local_ip, dst))) = (direct_ctx, direct_dst)
                    {
                        record(
                            "install_peers:fresh",
                            path::PathAction::Install(DirectTier::Lan),
                        );
                        // Same-subnet → LAN direct, skip the relay. Forget any
                        // pending relay request so a late grant can't later
                        // clobber the direct carrier.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, local_ip, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, phase_a_dst) {
                        if !lan_probing {
                            record(
                                "install_peers:fresh",
                                path::PathAction::Install(DirectTier::Public),
                            );
                        }
                        // Phase A — peer's NIC is public: dial it directly, skip
                        // the relay. Same forget-the-pending-relay guard.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_public_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let (Some(ctx), Some(dst)) = (direct_ctx, srflx_dst) {
                        if !lan_probing {
                            record(
                                "install_peers:fresh",
                                path::PathAction::Install(DirectTier::Srflx),
                            );
                        }
                        // Phase C — both NAT'd: hole-punch the peer's srflx from
                        // the punch socket, skip the relay.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        // rc.211 — a direct carrier supersedes any relay build
                        // still in flight for this peer; drop it on arrival.
                        relay_bq.invalidate(&np.node_id);
                        self.install_srflx_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let Some(coord) = relay.as_mut() {
                        // P3 PR-A — every sub-case below (mid-build, complete,
                        // request, tracking) is the same decision: the relay
                        // tier carries this peer.
                        if !lan_probing {
                            record("install_peers:fresh", path::PathAction::Relay);
                        }
                        // rc.211 — a carrier for this peer is mid-BUILD off-loop:
                        // post-`try_build` the coordinator no longer tracks it, so
                        // without this guard `!is_tracking` would re-`request` a
                        // DUPLICATE coordination during the 8 s QUIC window.
                        if relay_bq.in_flight.contains_key(&np.node_id) {
                            continue;
                        }
                        if let Some(link) = coord.maybe_complete(np.node_id, &cfg) {
                            let t0 = Instant::now();
                            self.install_ready(wg, by_node, tun, link, relay_bq).await;
                            warn_if_slow("install_ready(maybe_complete)", t0);
                        } else if !coord.is_tracking(&np.node_id) {
                            // Both ends pick the same coturn worker from the
                            // server's symmetric pair_key (in the grant), so no
                            // initiator/responder asymmetry is needed here — see
                            // relay_link.rs. The WG handshake still tie-breaks
                            // the dialer by pubkey in `install_ready`.
                            coord.request(np.node_id, cfg).await;
                        }
                    }
                }
            }
        }
    }

    /// rc.208 make-before-break — start a shadow direct-carrier PROBE for a peer
    /// currently on relay: register the demux + hand the candidate carrier to
    /// [`WgDevice::start_direct_probe`] (its own `Tunn`, NOT in the routing map),
    /// and record the [`UpgradeProbe`] metadata the promote/expire sweep reads.
    /// Does NOT touch `by_node` or the relay allocation — routing stays on relay
    /// until [`Self::sweep_upgrade_probes`] promotes this on a latched handshake.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn start_upgrade_probe(
        &self,
        wg: &mut WgDevice,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        node_id: ObjectId,
        cfg: &PeerConfig,
        sock: Arc<UdpSocket>,
        dst: std::net::SocketAddr,
        tier: DirectTier,
        now: Instant,
    ) {
        // P3 PR-A — mirror the probe start (per-peer serialization + the
        // global cap + the F2 LAN attempt marker live in the monitor).
        self.shadow(|s| s.mon.on_probe_started(&node_id, tier, now));
        wg.ensure_direct_demux(sock.clone());
        // Outbound upgrade: WE dial the peer, so initiate the handshake.
        // rc.276 diagnostics — capture before `sock` moves into the device.
        let probe_local = sock.local_addr().ok();
        wg.start_direct_probe(sock, cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        upgrade_probes.insert(
            node_id,
            UpgradeProbe {
                pubkey: cfg.public_key,
                overlay_ip: cfg.overlay_ip,
                dst,
                tier,
                since: now,
                initiated: true,
                local: probe_local,
            },
        );
        info!(
            peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, ?tier,
            "overlay: make-before-break — probing direct upgrade (relay held; cuts over only if the probe handshakes)"
        );
    }

    /// rc.208 make-before-break — drive in-flight upgrade probes each fallback
    /// tick. For each probe: PROMOTE it (swap the direct carrier in, drop the
    /// relay, retag `by_node` as direct, clear the tier's strikes) the moment its
    /// handshake latches; or, past the tier's [`DirectTier::handshake_deadline`],
    /// DROP it (keep the relay, book a tier failure — CC1, like the health
    /// sweep's fallback). The active carrier never stalls either way. No-op when
    /// no probes are in flight.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn sweep_upgrade_probes(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        upgrade_probes: &mut HashMap<ObjectId, UpgradeProbe>,
        relay_bq: &mut RelayBuildQueue,
    ) {
        if upgrade_probes.is_empty() {
            return;
        }
        let now = Instant::now();
        // P3 PR-A — same env read the legacy escalation table uses.
        let mbb = crate::overlay::direct::make_before_break_enabled();
        let mut settled: Vec<ObjectId> = Vec::new();
        for (nid, p) in upgrade_probes.iter() {
            // P2 — the promote/expire/wait decision is the pure
            // `lifecycle::probe_tick`; this loop executes the disposition.
            let verdict = probe_tick(
                wg.probe_handshake_done(&p.pubkey) == Some(true),
                now.duration_since(p.since),
                p.tier,
            );
            if verdict == ProbeVerdict::Promote {
                // P3 PR-B — read the probe's REAL latch latency (recv-path
                // `handshake_at` stamp minus the probe start) BEFORE the
                // promote moves the probe out of the probes map.
                let latency_ms = wg
                    .probe_handshake_latency_ms(&p.pubkey, p.since)
                    .map(|ms| ms as f64);
                // Bidirectional direct proven → cut over. `promote_direct_probe`
                // drops the old relay carrier; forget its coturn allocation and
                // retag `by_node` as the direct tier.
                if wg.promote_direct_probe(&p.pubkey) {
                    // P3 PR-A — the latch is the tier's proof: Q credit +
                    // strike clear in the monitor (≡ the `*_fails.remove`
                    // below), and any recent monitor refusal of this tier
                    // grades as harmful.
                    self.shadow(|s| {
                        s.mon.on_probe_result(
                            nid,
                            p.tier,
                            path::ProbeOutcome::Latched { latency_ms },
                            mbb,
                            now,
                        );
                        s.establishment(nid, p.tier, now);
                    });
                    if let Some(r) = relay.as_mut() {
                        r.forget(nid);
                    }
                    // P9 — a fresh-install LAN probe can latch while the
                    // fallback relay build is still in flight; drop that build
                    // so its late completion can't clobber the direct carrier
                    // (same rc.211 rule as the destructive installs).
                    relay_bq.invalidate(nid);
                    let off_link = matches!(p.tier, DirectTier::Public | DirectTier::Srflx);
                    by_node.insert(
                        *nid,
                        Installed {
                            initiated: p.initiated,
                            hs_done: true, // promote fires on the latched handshake
                            carrier_local: p.local,
                            carrier_dst: Some(p.dst),
                            public_direct_dst: off_link.then_some(p.dst),
                            ..Installed::base(p.pubkey, p.overlay_ip, p.tier, now)
                        },
                    );
                    tun.add_peer_route(p.overlay_ip).await.ok();
                    // PR-E — the latch already cleared the tier's strikes in
                    // the monitor (`on_probe_result(Latched)`, fed above).
                    info!(
                        peer = %nid, overlay_ip = %p.overlay_ip, tier = ?p.tier,
                        "overlay: make-before-break — direct carrier promoted (relay held throughout; zero stall)"
                    );
                } else {
                    // P3 PR-A — probe slot vanished under us (abnormal): free
                    // the monitor's mirror without booking a verdict.
                    self.shadow(|s| s.mon.on_probe_aborted(nid));
                }
                settled.push(*nid);
            } else if verdict == ProbeVerdict::Expire {
                // Probe never latched within the deadline → direct unreachable.
                // Drop it, KEEP the relay, book the failure on the tier (CC1 —
                // mirrors `sweep_carrier_health`'s direct→relay bookkeeping so a
                // repeatedly-failing tier still escalates to its sticky deny and
                // stops re-probing).
                //
                // P3 PR-A — the expiry books the monitor's strike + penalty (≡
                // the cooldown insert below); F1: NO quality event (the
                // penalty already encodes the failure). Tripwire: the tier
                // must be ineligible immediately after.
                self.shadow(|s| {
                    s.mon
                        .on_probe_result(nid, p.tier, path::ProbeOutcome::Expired, mbb, now);
                    s.assert_ineligible(nid, p.tier, now);
                });
                wg.drop_direct_probe(&p.pubkey).await;
                let tier_name = match p.tier {
                    DirectTier::Srflx => "srflx",
                    DirectTier::Public => "public",
                    _ => "LAN",
                };
                info!(
                    peer = %nid, tier = tier_name,
                    "overlay: make-before-break — direct probe did not handshake within deadline; kept relay (no stall)"
                );
                settled.push(*nid);
            }
        }
        for nid in settled {
            upgrade_probes.remove(&nid);
        }
    }

    /// rc.134 — install a peer over the SHARED direct-LAN socket (demuxed by
    /// source address, so any number of same-subnet peers coexist — no more
    /// "one direct peer" cap). Both ends initiate (bilateral hole-punch,
    /// rc.133). Adds the `/32` route + records it as `direct` in `by_node`.
    #[allow(clippy::too_many_arguments)]
    /// rc.134 — install a peer over the shared, interface-bound direct-LAN
    /// socket (source-demuxed, so any number of same-subnet peers coexist).
    /// The egress socket is chosen by [`lan_egress_socket`] (bind-by-route when
    /// enabled). Both ends initiate (bilateral hole-punch); records the `/32`
    /// route + `by_node` as `direct` / `Lan`.
    pub(super) async fn install_direct(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        ctx: &DirectCtx,
        node_id: ObjectId,
        cfg: &PeerConfig,
        local_ip: Ipv4Addr,
        dst: std::net::SocketAddr,
    ) {
        // Use the socket bound to the interface that shares the peer's subnet
        // (rc.143) so send/receive stay on the right NIC past a full-tunnel VPN.
        // With `OVERLAY_BIND_BY_ROUTE` on, `lan_egress_socket` instead consults
        // the OS route table per-destination and re-pins the socket fresh.
        let Some(sock) = lan_egress_socket(ctx, local_ip, dst).await else {
            warn!(peer = %node_id, %local_ip, "overlay: no socket bound for the matching LAN interface; skipping direct");
            return;
        };
        // P3 PR-A — record the install (switch time + the F2 LAN marker).
        self.shadow(|s| {
            s.mon
                .on_installed(&node_id, DirectTier::Lan, Instant::now())
        });
        wg.ensure_direct_demux(sock.clone());
        wg.add_direct_peer(sock.clone(), cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        by_node.insert(
            node_id,
            Installed {
                initiated: true,
                carrier_local: sock.local_addr().ok(),
                carrier_dst: Some(dst),
                ..Installed::base(
                    cfg.public_key,
                    cfg.overlay_ip,
                    DirectTier::Lan,
                    Instant::now(),
                )
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        // Phase 1 — if this peer is an approved subnet router, route its CIDRs
        // to it (router allowed_ips + OS route).
        self.install_subnets(wg, tun, node_id, cfg.public_key, &cfg.subnets)
            .await;
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: direct LAN carrier (same subnet) — skipping relay");
    }

    /// Phase A — install a peer over the **direct-to-public** carrier: dial its
    /// public NIC endpoint over the shared `public_sock` (a `0.0.0.0` socket, so
    /// the OS picks the egress NIC per-destination), demuxed by source like any
    /// direct peer. Bilateral init (a direct carrier initiates on both ends,
    /// `install_ready` semantics — the peer either dials us back symmetrically
    /// or, if NAT'd, accepts our dial and replies over the mapping our INIT
    /// opened). Records `public_direct_dst` so the health sweep tiers it and the
    /// exit-node exemption pins its IP (never self-wedge).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn install_public_direct(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        ctx: &DirectCtx,
        node_id: ObjectId,
        cfg: &PeerConfig,
        dst: std::net::SocketAddr,
    ) {
        let Some(sock) = ctx.public_sock.clone() else {
            warn!(peer = %node_id, "overlay: public-direct requested but no egress socket; skipping");
            return;
        };
        // P3 PR-A — record the install (switch time).
        self.shadow(|s| {
            s.mon
                .on_installed(&node_id, DirectTier::Public, Instant::now())
        });
        wg.ensure_direct_demux(sock.clone());
        // rc.276 diagnostics — capture before `sock` moves into the device.
        let carrier_local = sock.local_addr().ok();
        wg.add_direct_peer(sock, cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        by_node.insert(
            node_id,
            Installed {
                initiated: true,
                carrier_local,
                carrier_dst: Some(dst),
                public_direct_dst: Some(dst),
                ..Installed::base(
                    cfg.public_key,
                    cfg.overlay_ip,
                    DirectTier::Public,
                    Instant::now(),
                )
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        self.install_subnets(wg, tun, node_id, cfg.public_key, &cfg.subnets)
            .await;
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: direct-to-public carrier (NAT-traversal Phase A) — skipping relay");
    }

    /// Phase C — install a peer over the **srflx hole-punch** carrier: dial its
    /// STUN-learned public mapping from the PUNCH socket (`ctx.punch`, the
    /// interface socket that owns our own first advertised srflx), so our
    /// outbound WG INITs ride the same NAT mapping we advertised — opening our
    /// NAT's filter toward the peer's srflx while the peer's bilateral INITs open
    /// theirs toward ours (the mutual hole-punch). This is the crux difference
    /// from [`install_public_direct`], which dials via the arbitrary-egress
    /// `public_sock`: a punch REQUIRES the mapping-owning socket. Records
    /// `public_direct_dst` (off-link ⇒ exit-node exemption) and `tier = Srflx`
    /// (its own cooldown + the tight handshake deadline). No punch socket (srflx
    /// off / none gathered) ⇒ skip, and the caller falls through to relay.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn install_srflx_direct(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        ctx: &DirectCtx,
        node_id: ObjectId,
        cfg: &PeerConfig,
        dst: std::net::SocketAddr,
    ) {
        let Some((_, sock)) = ctx.punch.clone() else {
            warn!(peer = %node_id, "overlay: srflx punch requested but no punch socket; skipping");
            return;
        };
        // P3 PR-A — record the install (switch time).
        self.shadow(|s| {
            s.mon
                .on_installed(&node_id, DirectTier::Srflx, Instant::now())
        });
        wg.ensure_direct_demux(sock.clone());
        // rc.276 diagnostics — capture before `sock` moves into the device.
        let carrier_local = sock.local_addr().ok();
        wg.add_direct_peer(sock, cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        by_node.insert(
            node_id,
            Installed {
                initiated: true,
                carrier_local,
                carrier_dst: Some(dst),
                public_direct_dst: Some(dst),
                ..Installed::base(
                    cfg.public_key,
                    cfg.overlay_ip,
                    DirectTier::Srflx,
                    Instant::now(),
                )
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        self.install_subnets(wg, tun, node_id, cfg.public_key, &cfg.subnets)
            .await;
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: srflx hole-punch carrier (NAT-traversal Phase C) — skipping relay");
    }
    /// Install a ready carrier as a WG peer, add its `/32` route, and record
    /// it (pubkey + IP) for later removal.
    pub(super) async fn install_ready(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        link: ReadyLink,
        relay_bq: &mut RelayBuildQueue,
    ) {
        // P3 PR-A — a relay install is a switch too (production mode only:
        // the Direct/test carrier mode funnels loopback links through here,
        // which aren't relay-tier decisions).
        if matches!(self.mode, CarrierMode::Relay) {
            self.shadow(|s| {
                s.mon
                    .on_installed(&link.node_id, DirectTier::Relay, Instant::now())
            });
        }
        // Handshake direction. RELAY carriers use a deterministic single
        // initiator (the lexicographically smaller pubkey dials; both ends
        // compute it identically) — fine because the relay forwards both ways.
        //
        // rc.133 — DIRECT carriers need BOTH ends to initiate (bilateral
        // hole-punch). A direct WG init is an UNSOLICITED inbound UDP on the
        // responder's PHYSICAL interface, which default Windows Firewall drops
        // (field: two same-LAN hosts, direct carrier built but
        // HANDSHAKE(REKEY_TIMEOUT) forever). When both ends initiate, each
        // side's outbound init opens a stateful firewall hole for the other's
        // inbound, so the handshake completes. The relay path never hit this
        // because its ciphertext rides the agent's OWN outbound TURN
        // connection (already a stateful hole).
        // Optional QUIC-over-TURN upgrade of a relay carrier (opt-in, default
        // OFF via `overlay_quic_enabled`). QUIC's congestion control smooths the
        // relay's buffer-bloat latency spikes and its keepalive holds the TURN
        // permission fresh. On ANY handshake failure/timeout we fall back to the
        // already-built raw relay carrier, so the upgrade can only improve —
        // never break — the link.
        //
        // rc.199 — mutual coturn permission bootstrap for EVERY relay carrier
        // (raw, QUIC, or QUIC-fallback-to-raw). coturn only relays a peer's
        // datagrams to this allocation once it holds a *permission* for that
        // peer's relayed address, and a permission is opened by SENDING to it
        // (the webrtc-rs `turn` client lazily CreatePermission's the dst on the
        // first `send_to`). The relay carrier uses a single WG initiator (the
        // lexicographically-smaller pubkey), so without this the RESPONDER never
        // sends first → its allocation never opens a permission for the initiator
        // → coturn silently drops the WG handshake INIT → HANDSHAKE(REKEY_TIMEOUT)
        // forever. This is exactly why the cross-NAT relay never completed in the
        // field (P5 exit-node bring-up 2026-07-19: relay LINK ready + peer
        // installed, yet the handshake timed out for every peer); the DIRECT path
        // always worked precisely because it needs no coturn permission. Both
        // ends build a carrier and send this stray `\x00`, so BOTH permissions are
        // open before the handshake. `quic_relay` already does its own `\x00`
        // internally (wg.rs); this covers the raw + QUIC-fallback paths, which
        // previously shipped WITHOUT it (the wg.rs relay tests only passed because
        // they do the bootstrap by hand). The 1-byte datagram is below WG's
        // minimum message size, so boringtun ignores it.
        if let Some((conn, dst)) = &link.relay_parts {
            let _ = conn.send_to(b"\x00", *dst).await;
        }
        // Phase D — a single-relay link FORCES the QUIC carrier, ignoring the
        // `OVERLAY_QUIC` opt-in: a raw `Carrier::Relay` discards the recv source
        // (wg.rs recv), so the anchor would reply to the dialer's ADVERTISED
        // srflx port — wrong under a symmetric NAT (per-destination mapping) —
        // and the handshake dies. Only quinn's server consumes the observed
        // path. Symmetric on both ends: any build that advertises
        // `supports_relay_single` carries this rule, so the pair can't split
        // QUIC/raw (see `ReadyLink::single_relay`).
        //
        // A DERP link (`relay_kind == Derp`) is explicitly EXCLUDED from QUIC:
        // it's raw WG over the pubkey-addressed WS relay, and the pubkey pinning
        // makes the raw recv-source discard correct. QUIC-over-DERP would be
        // QUIC-over-TCP (double-reliable, HOL-on-HOL) and is untested — v1 stays
        // raw (A2). The gate below is belt-and-suspenders: a DERP link already
        // sets `single_relay: None` + `supports_quic: false`, but the explicit
        // `Turn` check keeps a future field-add from silently upgrading it.
        let want_quic = link.relay_parts.is_some()
            && link.relay_kind == RelayKind::Turn
            && (link.single_relay.is_some() || (overlay_quic_enabled() && link.supports_quic));
        if want_quic {
            // rc.211 — the QUIC-over-TURN rendezvous (up to QUIC_BUILD_TIMEOUT
            // = 8 s) runs OFF-LOOP: awaiting it here head-of-line-blocked the
            // `tun.read_packet()` arm for its full duration — the field-proven
            // 1–2 s overlay RTT plateaus (S1 watchdog: five 8.06 s stalls named
            // `install_ready(quic-build)` in one 150 s run on a churny host).
            // The spawned build sends its result to the `built_rx` select! arm,
            // which commits via `install_built` (µs). `quic_relay` sends the
            // `\x00` permission bootstrap itself; on failure the builder sends
            // one for the raw fallback (mirrors the pre-split inline probe).
            //
            // QUIC role. For a SINGLE-RELAY link the ANCHOR must serve — its
            // allocation is the rendezvous, and only the server-on-the-
            // allocation replies to coturn's observed sources. With UDP-aware
            // anchor selection the anchor may hold the LARGER pubkey, so the
            // pubkey rule would invert the roles and deadlock (the anchor
            // would QUIC-connect toward the dialer's srflx, which that
            // socket's NAT filter drops). Both-allocate keeps the pubkey rule
            // (deterministic, both ends agree; either allocation can serve).
            let (conn, dst) = link.relay_parts.clone().unwrap();
            let am_server = match link.single_relay {
                Some(anchor) => anchor,
                None => self.keypair.public.to_bytes() < link.public_key,
            };
            let min_datagram = self.mtu as usize + WG_OVERHEAD;
            let epoch = relay_bq.stamp(link.node_id);
            let tx = relay_bq.tx.clone();
            tokio::spawn(async move {
                let quic = match Carrier::quic_relay(
                    conn.clone(),
                    dst,
                    am_server,
                    min_datagram,
                    QUIC_BUILD_TIMEOUT,
                )
                .await
                {
                    Ok(q) => {
                        info!(peer = %link.node_id, %dst, am_server, "overlay: QUIC-over-TURN carrier up");
                        Some(q)
                    }
                    Err(e) => {
                        // For a single-relay link the raw fallback only carries
                        // for cone-ish dialers (port-preserving mapping); a
                        // symmetric dialer stays dark until the health sweep
                        // re-coordinates.
                        warn!(peer = %link.node_id, %e, single_relay = ?link.single_relay,
                              "overlay: QUIC carrier build failed; using raw relay");
                        // Permission bootstrap for the raw fallback (the QUIC
                        // attempt sent its own, but re-assert — it's 1 byte).
                        let _ = conn.send_to(b"\x00", dst).await;
                        None
                    }
                };
                // Receiver dropped ⇒ runtime exited; the build is moot.
                let _ = tx.send(BuiltRelay { epoch, link, quic }).await;
            });
            return;
        }
        self.install_built(wg, by_node, tun, link, None).await;
    }

    /// rc.211 — commit an already-BUILT relay/test carrier as a WG peer: the
    /// µs-fast install half of the old `install_ready` (`wg.add_peer` + `/32`
    /// route + subnets + bookkeeping). `quic: Some` = the off-loop QUIC build
    /// succeeded; `None` = raw carrier (no-QUIC link, or QUIC fallback).
    pub(super) async fn install_built(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        link: ReadyLink,
        quic: Option<Arc<Carrier>>,
    ) {
        let (relay_local, relay_dst) = match &link.relay_parts {
            Some((conn, dst)) => (conn.local_addr().ok(), Some(*dst)),
            None => (None, None),
        };
        let carrier = quic.unwrap_or_else(|| link.carrier.clone());
        let initiate = carrier.is_direct() || self.keypair.public.to_bytes() < link.public_key;
        let is_direct = carrier.is_direct();
        wg.add_peer(link.public_key, link.overlay_ip, carrier, initiate);
        by_node.insert(
            link.node_id,
            Installed {
                // rc.276 — every install_ready carrier is a flow WE built
                // (our allocation / dialer socket / DERP WS / loopback).
                initiated: true,
                carrier_local: relay_local,
                carrier_dst: relay_dst,
                relay_kind_dbg: (!is_direct).then_some(match link.relay_kind {
                    crate::overlay::relay_link::RelayKind::Turn => "turn",
                    crate::overlay::relay_link::RelayKind::Derp => "derp",
                }),
                relay_local,
                relay_dst,
                // A relay carrier, or the loopback carrier used in Direct/test
                // mode. Test loopback carriers are direct → Lan (no off-link
                // handshake deadline); coturn carriers → Relay.
                ..Installed::base(
                    link.public_key,
                    link.overlay_ip,
                    if is_direct {
                        DirectTier::Lan
                    } else {
                        DirectTier::Relay
                    },
                    Instant::now(),
                )
            },
        );
        // Host `/32` so overlay traffic to this peer beats any colliding
        // less-specific route on the uplink (e.g. a carrier CGNAT /10).
        // Best-effort — clean hosts route fine via the connected /10.
        if let Err(e) = tun.add_peer_route(link.overlay_ip).await {
            debug!(peer = %link.node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        // Phase 1 — subnet-router peer: route its approved CIDRs to it.
        self.install_subnets(wg, tun, link.node_id, link.public_key, &link.subnets)
            .await;
        info!(peer = %link.node_id, overlay_ip = %link.overlay_ip, initiate, "overlay: peer installed");
    }
}
