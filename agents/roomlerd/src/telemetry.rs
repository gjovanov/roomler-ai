// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Stats PR-5 — heartbeat telemetry sampler.
//!
//! Fills the [`AgentSysStats`] block on `rc:agent.heartbeat`: agent-process
//! rss/cpu (the historically hardcoded-zero fields), host-total cumulative
//! network counters (the server derives rates from successive samples), and
//! the overlay carrier tallies + median peer RTT read from the runtime's
//! published [`OverlayView`] — the richest telemetry on the host, which
//! previously never left the LocalAPI socket.
//!
//! ## The host counters are read off the signaling loop (#2551)
//!
//! The rss/cpu and interface counters are an OS read: the process table,
//! and on Windows `GetIfTable2`, the whole IP Helper interface table. It
//! used to run inline in the heartbeat arm, holding the overlay view's
//! watch guard, and a stall report named that arm
//! (`signaling=93s@heartbeat`) on a host whose adapters and routes churn.
//! A read that blocks there freezes the control plane until the watchdog
//! kills the worker. So one sampler thread makes the read every
//! [`HOST_SAMPLE_PERIOD`] into a cache, and the heartbeat only reads the
//! cache. A blocked read can then cost stale counters, never the loop, and
//! any read slower than [`SLOW_HOST_SAMPLE`] is logged with its duration.
//! The period matches the heartbeat's 30 s, which satisfies sysinfo's
//! minimum CPU-measurement interval (the first read reports 0%).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use roomler_ai_remote_control::signaling::{AgentSysStats, PeerLink};
use tunnel_core::localapi::{ConnectionType, OverlayView, PeerInfo};

/// How this node currently reaches a peer, as ONE label.
///
/// `ConnectionType` has no `Derp` variant — DERP is a relay flavour only
/// the relay-detail fields distinguish — so the split lives here, used by
/// both the aggregate counters and the per-edge report. The TOP-LEVEL
/// `relay_kind` (carrier consolidation) is preferred; the forensics block
/// is the fallback for a runtime that predates it.
fn carrier_label(p: &PeerInfo) -> &'static str {
    match p.connection {
        ConnectionType::Direct => "direct",
        ConnectionType::Relay => {
            let kind = p
                .relay_kind
                .as_deref()
                .or_else(|| p.debug.as_ref().and_then(|d| d.relay_kind.as_deref()));
            if kind == Some("derp") {
                "derp"
            } else {
                "relay"
            }
        }
        ConnectionType::Tunnel => "tunnel",
        ConnectionType::Blocked => "blocked",
        ConnectionType::Offline => "offline",
    }
}

/// The relay qualifier the CLI shows after `relay:` — `turn/udp`,
/// `derp/tcp`, or just the kind when the transport is unknown (wave 4).
/// `None` for anything that isn't relayed, so a direct edge never carries
/// a stale flavour.
fn relay_qualifier(p: &PeerInfo) -> Option<String> {
    if !matches!(p.connection, ConnectionType::Relay) {
        return None;
    }
    let kind = p
        .relay_kind
        .as_deref()
        .or_else(|| p.debug.as_ref().and_then(|d| d.relay_kind.as_deref()))?;
    Some(match p.relay_transport.as_deref() {
        Some(t) => format!("{kind}/{t}"),
        None => kind.to_string(),
    })
}

/// What the heartbeat reports about the host itself: this process's memory
/// and CPU, and the machine's cumulative interface byte counters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HostCounters {
    pub rss_mb: u32,
    pub cpu_pct: f32,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

/// #2551 — how often the sampler thread reads the host counters: the
/// heartbeat's own period, so a CPU figure averages over the beat that
/// carries it.
const HOST_SAMPLE_PERIOD: Duration = Duration::from_secs(30);

/// #2551 — a read slower than this is logged with its duration: the field
/// evidence for, or against, an OS read being what froze the loop.
const SLOW_HOST_SAMPLE: Duration = Duration::from_secs(5);

/// #2551 — a read still running after this is reported stuck by the
/// heartbeat, once per episode. The heartbeat goes on with the last counters.
const STUCK_HOST_SAMPLE: Duration = Duration::from_secs(60);

/// The sampler thread's cache. Its lock is held only to store or copy, never
/// across an OS read, so a heartbeat never waits on one.
struct HostSlot {
    /// The last completed read.
    last: Option<HostCounters>,
    /// When the read in progress started; `None` between reads.
    reading_since: Option<Instant>,
}

static HOST: Mutex<HostSlot> = Mutex::new(HostSlot {
    last: None,
    reading_since: None,
});

static HOST_SAMPLER: OnceLock<()> = OnceLock::new();

fn host_slot() -> std::sync::MutexGuard<'static, HostSlot> {
    HOST.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start the sampler thread, once per process. Detached on purpose: a read
/// blocked in the OS must not hold up the runtime's shutdown either, and
/// the process exit ends the thread.
fn start_host_sampler() {
    HOST_SAMPLER.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("roomlerd-host-sampler".into())
            .spawn(host_sampler_loop);
        if let Err(e) = spawned {
            tracing::warn!(%e, "host sampler: the thread did not start; heartbeats carry no host counters");
        }
    });
}

fn host_sampler_loop() {
    // Building the interface list is a read of the same table, so it is
    // timed as part of the first sample.
    let mut started = Instant::now();
    host_slot().reading_since = Some(started);
    let mut sys = sysinfo::System::new();
    let mut networks = sysinfo::Networks::new_with_refreshed_list();
    let pid = sysinfo::get_current_pid().ok();
    loop {
        let counters = read_host(&mut sys, &mut networks, pid);
        let took = started.elapsed();
        if took >= SLOW_HOST_SAMPLE {
            tracing::warn!(
                took_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                "host sampler: reading the host counters was slow (#2551)"
            );
        }
        {
            let mut slot = host_slot();
            slot.last = Some(counters);
            slot.reading_since = None;
        }
        std::thread::sleep(HOST_SAMPLE_PERIOD);
        started = Instant::now();
        host_slot().reading_since = Some(started);
    }
}

/// The OS read itself. Only the sampler thread calls it.
fn read_host(
    sys: &mut sysinfo::System,
    networks: &mut sysinfo::Networks,
    pid: Option<sysinfo::Pid>,
) -> HostCounters {
    let (rss_mb, cpu_pct) = match pid {
        Some(pid) => {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            match sys.process(pid) {
                Some(p) => (
                    (p.memory() / (1024 * 1024)).min(u64::from(u32::MAX)) as u32,
                    p.cpu_usage(),
                ),
                None => (0, 0.0),
            }
        }
        None => (0, 0.0),
    };

    networks.refresh(true);
    let mut net_rx_bytes = 0u64;
    let mut net_tx_bytes = 0u64;
    for (_name, data) in networks.iter() {
        net_rx_bytes = net_rx_bytes.saturating_add(data.total_received());
        net_tx_bytes = net_tx_bytes.saturating_add(data.total_transmitted());
    }
    HostCounters {
        rss_mb,
        cpu_pct,
        net_rx_bytes,
        net_tx_bytes,
    }
}

/// The cache as a heartbeat at `now` sees it: the last counters, and how long
/// the read in progress has run once that reaches [`STUCK_HOST_SAMPLE`].
fn host_view_at(
    last: Option<HostCounters>,
    reading_since: Option<Instant>,
    now: Instant,
) -> (Option<HostCounters>, Option<Duration>) {
    let stuck = reading_since
        .map(|t| now.saturating_duration_since(t))
        .filter(|d| *d >= STUCK_HOST_SAMPLE);
    (last, stuck)
}

/// Whether a heartbeat should say the sampler is stuck: on the first beat of
/// an episode only, so a stuck read is one line, not one per beat.
fn stuck_news(reported: &mut bool, stuck: Option<Duration>) -> Option<Duration> {
    match stuck {
        Some(d) if !*reported => {
            *reported = true;
            Some(d)
        }
        Some(_) => None,
        None => {
            *reported = false;
            None
        }
    }
}

pub struct SysSampler {
    /// Whether this connection has already reported the current stuck read.
    stuck_reported: bool,
}

impl Default for SysSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl SysSampler {
    pub fn new() -> Self {
        start_host_sampler();
        Self {
            stuck_reported: false,
        }
    }

    /// The heartbeat's block. Never touches the OS: the host counters come
    /// from the sampler thread's cache.
    ///
    /// `None` until that thread's first read completes. The cumulative
    /// counters have no honest stand-in: a zero reads as a counter reset,
    /// which every rollup's `max - min` turns into a since-boot spike.
    ///
    /// `tunnel_bytes` is `(rx, tx)` summed across this daemon's live tunnel
    /// forwards — passed in rather than read here so telemetry stays free of
    /// a dependency on the tunnel client hub.
    pub fn sample(
        &mut self,
        view: &OverlayView,
        tunnel_bytes: (u64, u64),
    ) -> Option<AgentSysStats> {
        let (host, stuck) = {
            let slot = host_slot();
            host_view_at(slot.last, slot.reading_since, Instant::now())
        };
        if let Some(d) = stuck_news(&mut self.stuck_reported, stuck) {
            tracing::warn!(
                stuck_s = d.as_secs(),
                "host sampler: a read of the host counters is stuck (#2551); heartbeats carry the last ones"
            );
        }
        Some(Self::assemble(view, tunnel_bytes, host?))
    }

    /// The block from the host counters and the overlay's view. Pure.
    pub(crate) fn assemble(
        view: &OverlayView,
        tunnel_bytes: (u64, u64),
        host: HostCounters,
    ) -> AgentSysStats {
        let HostCounters {
            rss_mb,
            cpu_pct,
            net_rx_bytes,
            net_tx_bytes,
        } = host;

        let (mut direct, mut relay, mut derp) = (0u32, 0u32, 0u32);
        let mut rtts: Vec<u32> = Vec::new();
        // Wave 2: the same walk also emits the per-peer EDGES the org
        // dashboard graphs. Offline/blocked peers are reported too (as
        // their own carrier kinds) — a mesh drawing needs to show the
        // peer we cannot reach, which is exactly the interesting case.
        let mut links: Vec<PeerLink> = Vec::with_capacity(view.peers.len());
        // Wave 3: per-edge IP-data volume, summed into the agent total. The
        // counters live on the installed carrier, so a peer with none
        // (offline/blocked) contributes zero rather than being skipped —
        // the edge still belongs on the graph.
        let mut overlay_rx_bytes = 0u64;
        let mut overlay_tx_bytes = 0u64;
        for p in &view.peers {
            let carrier = carrier_label(p);
            let (tx, rx) = p.debug.as_ref().map(|d| (d.tx, d.rx)).unwrap_or((0, 0));
            overlay_tx_bytes = overlay_tx_bytes.saturating_add(tx);
            overlay_rx_bytes = overlay_rx_bytes.saturating_add(rx);
            links.push(PeerLink {
                node: p.node_id.clone(),
                carrier: carrier.to_string(),
                rtt_ms: p.rtt_ms,
                stalled: p.stalled,
                tx,
                rx,
                relay: relay_qualifier(p),
            });
            if !p.online {
                continue;
            }
            match carrier {
                "direct" => direct += 1,
                "derp" => derp += 1,
                "relay" => relay += 1,
                _ => {}
            }
            if let Some(r) = p.rtt_ms {
                rtts.push(r);
            }
        }
        rtts.sort_unstable();
        let peer_rtt_ms = (!rtts.is_empty()).then(|| rtts[rtts.len() / 2]);

        AgentSysStats {
            rss_mb,
            cpu_pct,
            net_rx_bytes,
            net_tx_bytes,
            direct,
            relay,
            derp,
            peer_rtt_ms,
            overlay_rx_bytes,
            overlay_tx_bytes,
            tunnel_rx_bytes: tunnel_bytes.0,
            tunnel_tx_bytes: tunnel_bytes.1,
            links,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_core::localapi::{PeerCarrierDebug, PeerInfo};

    /// Fixed host counters, so the block's view-derived half is tested without
    /// the sampler thread or the real OS read.
    const HOST: HostCounters = HostCounters {
        rss_mb: 42,
        cpu_pct: 1.5,
        net_rx_bytes: 7_000,
        net_tx_bytes: 9_000,
    };

    /// #2551 — the block carries the sampler's counters as they are.
    #[test]
    fn the_block_carries_the_host_counters() {
        let out = SysSampler::assemble(&OverlayView::default(), (3, 4), HOST);
        assert_eq!(
            (out.rss_mb, out.cpu_pct, out.net_rx_bytes, out.net_tx_bytes),
            (42, 1.5, 7_000, 9_000)
        );
        assert_eq!((out.tunnel_rx_bytes, out.tunnel_tx_bytes), (3, 4));
    }

    /// #2551 — a read is stuck only once it has run [`STUCK_HOST_SAMPLE`], and
    /// the last counters stay readable while it is.
    #[test]
    fn a_read_is_stuck_only_past_its_bound() {
        let t0 = Instant::now();
        assert_eq!(host_view_at(None, None, t0), (None, None));
        let just_under = t0 + STUCK_HOST_SAMPLE - Duration::from_millis(1);
        assert_eq!(
            host_view_at(Some(HOST), Some(t0), just_under),
            (Some(HOST), None)
        );
        let at = t0 + STUCK_HOST_SAMPLE;
        assert_eq!(
            host_view_at(Some(HOST), Some(t0), at),
            (Some(HOST), Some(STUCK_HOST_SAMPLE))
        );
        assert_eq!(
            host_view_at(None, Some(t0), at + Duration::from_secs(30)),
            (None, Some(STUCK_HOST_SAMPLE + Duration::from_secs(30)))
        );
    }

    /// #2551 — one line per stuck episode, not one per beat; a read that
    /// completes ends the episode, so the next stuck read is news again.
    #[test]
    fn a_stuck_read_is_news_once_per_episode() {
        let mut reported = false;
        let d = Some(Duration::from_secs(61));
        assert_eq!(stuck_news(&mut reported, None), None);
        assert_eq!(stuck_news(&mut reported, d), d);
        assert_eq!(
            stuck_news(&mut reported, Some(Duration::from_secs(91))),
            None
        );
        assert_eq!(stuck_news(&mut reported, None), None);
        assert_eq!(stuck_news(&mut reported, d), d);
    }

    fn peer_with_relay(kind: &str, transport: Option<&str>) -> PeerInfo {
        let mut p = peer(true, ConnectionType::Relay, Some(30), None);
        p.relay_kind = Some(kind.into());
        p.relay_transport = transport.map(Into::into);
        p
    }

    fn peer_with_bytes(
        online: bool,
        conn: ConnectionType,
        rtt: Option<u32>,
        kind: Option<&str>,
        tx: u64,
        rx: u64,
    ) -> PeerInfo {
        let mut p = peer(online, conn, rtt, kind);
        if let Some(d) = p.debug.as_mut() {
            d.tx = tx;
            d.rx = rx;
        }
        p
    }

    fn peer(online: bool, conn: ConnectionType, rtt: Option<u32>, kind: Option<&str>) -> PeerInfo {
        PeerInfo {
            node_id: "n".into(),
            name: "p".into(),
            org: String::new(),
            overlay_ip: Some("100.64.0.9".into()),
            overlay_ip6: None,
            online,
            connection: conn,
            upgrading: false,
            stalled: false,
            rtt_ms: rtt,
            last_seen_ms: None,
            agent_id: None,
            relay_local: None,
            relay_dst: None,
            relay_kind: None,
            relay_transport: None,
            relay_server: None,
            why: None,
            probes: Vec::new(),
            debug: kind.map(|k| PeerCarrierDebug {
                tier: "relay".into(),
                initiated: true,
                hs_done: true,
                local: None,
                dst: None,
                tx: 0,
                rx: 0,
                last_rx_age_s: 0,
                relay_kind: Some(k.into()),
                rx_denied: 0,
                rx_denied_noroute: 0,
            }),
        }
    }

    #[test]
    fn carrier_tallies_and_median_rtt() {
        let view = OverlayView {
            self_ip: Some("100.64.0.7".into()),
            self_ip6: None,
            peers: vec![
                peer(true, ConnectionType::Direct, Some(10), None),
                peer(true, ConnectionType::Relay, Some(50), Some("turn")),
                peer(true, ConnectionType::Relay, Some(90), Some("derp")),
                peer(true, ConnectionType::Relay, None, None), // no debug → TURN
                peer(false, ConnectionType::Direct, Some(999), None), // offline: ignored
            ],
            exit_node: None,
            route_offer_withheld: None,
            host_firewall: None,
            dns: None,
            srflx: None,
            warm_relay: None,
            direct_socks: Vec::new(),
            derp_inbound_drops: None,
        };
        let out = SysSampler::assemble(&view, (0, 0), HOST);
        assert_eq!(out.direct, 1);
        assert_eq!(out.relay, 2);
        assert_eq!(out.derp, 1);
        assert_eq!(out.peer_rtt_ms, Some(50)); // median of [10, 50, 90]
        // Wave 2: the same walk emits one EDGE per peer — including the
        // offline one, which a mesh drawing must show rather than omit.
        assert_eq!(out.links.len(), 5);
        let carriers: Vec<&str> = out.links.iter().map(|l| l.carrier.as_str()).collect();
        assert_eq!(
            carriers,
            vec!["direct", "relay", "derp", "relay", "direct"],
            "carrier labels split DERP out of the Relay variant"
        );
        assert_eq!(out.links[0].rtt_ms, Some(10));
    }

    /// A peer we cannot reach is still an edge — with its own carrier
    /// label — but never counts as a live carrier.
    #[test]
    fn unreachable_peers_are_edges_not_carriers() {
        let view = OverlayView {
            self_ip: Some("100.64.0.7".into()),
            self_ip6: None,
            peers: vec![
                peer(true, ConnectionType::Direct, Some(10), None),
                peer(false, ConnectionType::Offline, None, None),
                peer(false, ConnectionType::Blocked, None, None),
            ],
            exit_node: None,
            route_offer_withheld: None,
            host_firewall: None,
            dns: None,
            srflx: None,
            warm_relay: None,
            direct_socks: Vec::new(),
            derp_inbound_drops: None,
        };
        let out = SysSampler::assemble(&view, (0, 0), HOST);
        assert_eq!(out.links.len(), 3);
        assert_eq!(out.links[1].carrier, "offline");
        assert_eq!(out.links[2].carrier, "blocked");
        assert_eq!(out.direct, 1);
        assert_eq!(out.relay + out.derp, 0);
    }

    /// Wave 3 — per-edge volume rides each link, and the agent total is
    /// exactly their sum. A peer with no installed carrier contributes
    /// zero rather than being dropped, so the edge count is unaffected.
    #[test]
    fn overlay_volume_sums_across_edges() {
        let view = OverlayView {
            self_ip: Some("100.64.0.7".into()),
            self_ip6: None,
            peers: vec![
                peer_with_bytes(
                    true,
                    ConnectionType::Relay,
                    Some(10),
                    Some("turn"),
                    100,
                    200,
                ),
                peer_with_bytes(true, ConnectionType::Relay, Some(20), Some("derp"), 30, 7),
                // No debug block at all → no carrier → contributes nothing.
                peer(false, ConnectionType::Offline, None, None),
            ],
            exit_node: None,
            route_offer_withheld: None,
            host_firewall: None,
            dns: None,
            srflx: None,
            warm_relay: None,
            direct_socks: Vec::new(),
            derp_inbound_drops: None,
        };
        let out = SysSampler::assemble(&view, (0, 0), HOST);
        assert_eq!(out.links.len(), 3);
        assert_eq!(out.links[0].tx, 100);
        assert_eq!(out.links[0].rx, 200);
        assert_eq!(out.links[2].tx, 0, "carrier-less peer reports zero volume");
        assert_eq!(out.overlay_tx_bytes, 130);
        assert_eq!(out.overlay_rx_bytes, 207);
    }

    /// Wave 4 — each relay edge carries the CLI's qualifier (`turn/udp`,
    /// `derp/tcp`); the top-level fields are authoritative and the carrier
    /// split follows them, with the forensics block only as fallback.
    #[test]
    fn relay_edges_carry_the_qualified_flavour() {
        let view = OverlayView {
            self_ip: Some("100.64.0.7".into()),
            self_ip6: None,
            peers: vec![
                peer_with_relay("turn", Some("udp")),
                peer_with_relay("derp", Some("tcp")),
                // Kind without transport degrades like the CLI: `turn`,
                // never an invented protocol.
                peer_with_relay("turn", None),
                // Direct peers carry NO qualifier, even if a stale kind
                // lingers from a previous carrier.
                {
                    let mut p = peer(true, ConnectionType::Direct, Some(5), None);
                    p.relay_kind = Some("turn".into());
                    p
                },
                // Pre-consolidation runtime: only the debug block knows.
                peer(true, ConnectionType::Relay, Some(60), Some("derp")),
            ],
            exit_node: None,
            route_offer_withheld: None,
            host_firewall: None,
            dns: None,
            srflx: None,
            warm_relay: None,
            direct_socks: Vec::new(),
            derp_inbound_drops: None,
        };
        let out = SysSampler::assemble(&view, (0, 0), HOST);
        assert_eq!(out.links[0].relay.as_deref(), Some("turn/udp"));
        assert_eq!(out.links[0].carrier, "relay");
        assert_eq!(out.links[1].relay.as_deref(), Some("derp/tcp"));
        assert_eq!(
            out.links[1].carrier, "derp",
            "top-level relay_kind must drive the derp split"
        );
        assert_eq!(out.links[2].relay.as_deref(), Some("turn"));
        assert_eq!(
            out.links[3].relay, None,
            "direct edge never carries a flavour"
        );
        assert_eq!(out.links[4].relay.as_deref(), Some("derp"));
        assert_eq!(
            out.links[4].carrier, "derp",
            "debug fallback still splits derp"
        );
    }
}
