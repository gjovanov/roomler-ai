//! Phase B (overlay v3) — netcheck: the measured capability vector.
//!
//! Selection heuristics (srflx-presence-implies-dialer, conviction latches)
//! exist because nothing MEASURED what a host's egress can actually do. This
//! module measures it, Tailscale-netcheck style, and publishes a process-wide
//! [`CapVector`] the selection layers consume instead of folklore:
//!
//! * `relay_band_udp` — THE dialer bit, measured over the EXACT dialer path:
//!   a dedicated TURN allocation (via the warm-grant creds), permissions
//!   bootstrapped for every own public IP, then a fresh egress-pinned raw
//!   socket dials the allocation's own relayed address. The tagged datagram
//!   coming back through the allocation proves the relay band is reachable;
//!   its absence (with STUN working) proves the corp drop CORPLAP-3-class hosts
//!   showed on capture — without burning a peer pair to find out.
//! * `stun_udp` / `nat` — the existing gather/typing results, snapshotted.
//! * `derp_ws_ok` — the central `/derp` WS liveness (the floor's health).
//!
//! Cadence: shortly after start, on a netstate material Major (new network =
//! new egress policy), then every [`NETCHECK_INTERVAL`] ± jitter. The runtime
//! drives it by reusing the C4 warm-grant flow (`OverlayWarmRelayRequest` →
//! `WarmRelayGrant` creds) — no new wire message in this PR; the advert
//! (`OverlayNetcheck`) is PR-B2.
//!
//! Consumers in this PR: none (measure + publish + log only). PR-B3 rewires
//! the dialer-role selection onto `relay_band_udp` and demotes the #508/#511
//! conviction latch to a re-probe trigger.

use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, info};

use crate::transport::relay::RelayConn;

/// Re-measure cadence (± the runtime's tick jitter). 20 min matches the
/// relay-probe REPROBE discipline; a netstate Major re-measures immediately.
pub const NETCHECK_INTERVAL: Duration = Duration::from_secs(20 * 60);

/// Startup delay before the first measurement — let the srflx gather and the
/// control WS settle so the vector's inputs are real.
pub const NETCHECK_STARTUP_DELAY: Duration = Duration::from_secs(45);

/// Per-sample wait for the probe datagram to come back through the
/// allocation, and the number of attempts. Three 2 s samples tolerate a
/// slow corp permission bootstrap (field: 7-15 s TURNS bootstraps — the
/// permission sends run first, and the LAST sample decides).
const PROBE_SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_SAMPLES: u32 = 4;

/// The probe payload tag — random-looking, never a valid WG/QUIC/STUN
/// prefix, so a stray delivery to any real consumer is discarded there and
/// anything ELSE arriving on the probe allocation is ignored here.
const PROBE_TAG: &[u8; 12] = b"rmlr-netchk1";

/// The measured capability vector. `Option` fields are `None` when the
/// measurement could not run (no creds, no srflx to permit) — absence of
/// measurement is NEVER evidence of absence of capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapVector {
    /// The srflx gather found a public mapping (existing Phase B result).
    pub stun_udp: bool,
    /// Raw UDP from this host reaches coturn's relay band — measured over
    /// the exact single-relay dialer path. `Some(false)` = the CORPLAP-3-class
    /// egress drop, proven without a peer pair.
    pub relay_band_udp: Option<bool>,
    /// The central `/derp` WS is up + registered (the floor's health).
    pub derp_ws_ok: bool,
    /// NAT mapping class from the (self-vantage-excluded, W5b) typing.
    pub nat: Option<String>,
}

/// The vector + its measurement stamp, process-wide (a HOST property — one
/// egress policy for every org runtime, like netstate/dialer).
static CURRENT: Mutex<Option<(CapVector, Instant)>> = Mutex::new(None);

/// The latest vector, if any measurement has completed, with its age.
pub fn current() -> Option<(CapVector, Duration)> {
    CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(v, at)| (v.clone(), at.elapsed()))
}

/// Publish a fresh measurement; logs at INFO only when the vector CHANGED
/// (steady-state re-measurements stay quiet).
pub fn publish(v: CapVector) {
    let mut cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let changed = cur.as_ref().map(|(old, _)| old != &v).unwrap_or(true);
    if changed {
        info!(
            stun_udp = v.stun_udp,
            relay_band_udp = ?v.relay_band_udp,
            derp_ws_ok = v.derp_ws_ok,
            nat = ?v.nat,
            "netcheck: capability vector changed"
        );
    } else {
        debug!("netcheck: capability vector re-measured, unchanged");
    }
    *cur = Some((v, Instant::now()));
}

/// Netstate-Major hook — the old vector describes a network that no longer
/// exists; drop it so consumers fall back to presence rules until the
/// re-measurement (which the runtime schedules on the same signal) lands.
pub fn invalidate_on_network_change() {
    if CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .is_some()
    {
        info!("netcheck: network changed — capability vector invalidated pending re-measure");
    }
}

/// The relay-band probe, over the exact dialer path.
///
/// `alloc` is a DEDICATED TURN allocation (never a leg peers dial) with
/// relayed address `relayed`; `own_public_ips` are this host's srflx IPs
/// (coturn permissions are IP-scoped — every observed egress IP must be
/// permitted or a multi-egress host false-negatives, reviewer F2).
///
/// Sequence: bootstrap permissions by sending one tagged datagram from the
/// allocation toward each own IP (port 9, discard — the send is the
/// CreatePermission trigger); then, from a fresh egress-pinned raw socket
/// (the dialer's exact bind recipe), dial `relayed` up to
/// [`PROBE_SAMPLES`] times, reading the allocation for the tag between
/// attempts. Any tagged receipt ⇒ `true`.
pub async fn probe_relay_band(
    alloc: &dyn RelayConn,
    relayed: SocketAddr,
    own_public_ips: &[IpAddr],
) -> bool {
    // Permission bootstrap — IP-scoped; the ports are irrelevant.
    for ip in own_public_ips {
        let _ = alloc.send_to(PROBE_TAG, SocketAddr::new(*ip, 9)).await;
    }
    // The dialer's bind recipe verbatim (relay_link::try_build_dialer):
    // fresh raw socket + VPN-bypass egress pinning.
    let Ok(std_sock) = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)) else {
        return false;
    };
    if std_sock.set_nonblocking(true).is_err() {
        return false;
    }
    let Ok(sock) = tokio::net::UdpSocket::from_std(std_sock) else {
        return false;
    };
    if let Some(ix) = super::direct::vpn_bypass_ifindex() {
        super::direct::force_egress_interface(&sock, ix);
    }

    let mut buf = [0u8; 64];
    for attempt in 0..PROBE_SAMPLES {
        let _ = sock.send_to(PROBE_TAG, relayed).await;
        let deadline = tokio::time::sleep(PROBE_SAMPLE_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                r = alloc.recv_from(&mut buf) => match r {
                    Ok((n, _src)) if buf[..n].starts_with(PROBE_TAG) => {
                        debug!(attempt, "netcheck: relay-band probe datagram returned");
                        return true;
                    }
                    // Stray traffic on the probe allocation — keep reading.
                    Ok(_) => continue,
                    // The allocation died mid-probe: no honest verdict.
                    Err(_) => return false,
                },
                _ = &mut deadline => break,
            }
        }
    }
    false
}

/// One full measurement pass, from the runtime's inputs. `alloc` is the
/// dedicated allocation (or `None` when creds/allocation weren't available
/// — `relay_band_udp` then stays unmeasured rather than guessed).
pub async fn run_measurement(
    stun_udp: bool,
    nat: Option<String>,
    derp_ws_ok: bool,
    alloc: Option<(std::sync::Arc<dyn RelayConn>, SocketAddr)>,
    own_public_ips: Vec<IpAddr>,
) {
    // No srflx ⇒ no UDP out at all ⇒ the relay band is definitionally
    // unreachable; short-circuit rather than burn an allocation
    // (reviewer F2's `!stun_udp` rule).
    let relay_band_udp = if !stun_udp {
        Some(false)
    } else {
        match alloc {
            Some((conn, relayed)) => {
                Some(probe_relay_band(conn.as_ref(), relayed, &own_public_ips).await)
            }
            None => None,
        }
    };
    publish(CapVector {
        stun_udp,
        relay_band_udp,
        derp_ws_ok,
        nat,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot lifecycle: publish/current/invalidate, and change-detection
    /// (one process-wide slot — a single test avoids racing it).
    #[test]
    fn slot_publish_current_and_invalidate() {
        invalidate_on_network_change();
        assert!(current().is_none());
        let v = CapVector {
            stun_udp: true,
            relay_band_udp: Some(false),
            derp_ws_ok: true,
            nat: Some("cone".into()),
        };
        publish(v.clone());
        let (got, age) = current().expect("published");
        assert_eq!(got, v);
        assert!(age < Duration::from_secs(5));
        // Re-publish unchanged keeps it; invalidate clears it.
        publish(v);
        assert!(current().is_some());
        invalidate_on_network_change();
        assert!(current().is_none());
    }

    /// The probe against a loopback echo standing in for coturn: the raw
    /// dial's datagram must come back through the "allocation" and match
    /// the tag; a dead responder times out to `false`.
    #[tokio::test]
    async fn relay_band_probe_round_trips_via_the_allocation() {
        use crate::transport::relay::UdpRelayConn;
        // "Allocation": a socket whose recv the probe reads. "Relayed
        // address": a loopback echo that forwards anything it receives to
        // the allocation socket — the coturn relay in miniature.
        let alloc_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let alloc_addr = alloc_sock.local_addr().unwrap();
        let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relayed = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 64];
            while let Ok((n, _)) = echo.recv_from(&mut b).await {
                let _ = echo.send_to(&b[..n], alloc_addr).await;
            }
        });
        let alloc = UdpRelayConn(alloc_sock);
        assert!(probe_relay_band(&alloc, relayed, &[]).await);

        // A relayed address nothing answers on ⇒ false after the samples.
        let dead = "127.0.0.1:1".parse().unwrap();
        let alloc2 = UdpRelayConn(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        assert!(!probe_relay_band(&alloc2, dead, &[]).await);
    }
}
