//! C4 stage 1 — the warm TURN/UDP allocation: loop-side state + pure
//! helpers. **Measurement-only**: nothing routes over the allocation yet;
//! the point of this stage is to make one sentence readable from `roomler
//! status` after a VPN transition — "the allocation survived" (probes keep
//! succeeding over the grandfathered flow) or "it didn't" (and why). The
//! rendezvous use (stage 2) and same-socket cred re-allocation come only
//! after that evidence. Design: `docs/overlay-warm-relay.md`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::transport::relay::RelayConn;

/// Minimum spacing between credential requests — a host whose allocate
/// keeps failing (or whose grants go unanswered) must not hammer the
/// control WS.
pub(crate) const REQUEST_SPACING: Duration = Duration::from_secs(300);

/// Liveness-probe cadence. Doubles as the flow keepalive on the client
/// leg (the turn client's own refreshes also traffic the 5-tuple, but the
/// probe is the one we MEASURE) — comfortably inside the ~20-25 s-per-30 s
/// conntrack-assurance window is unnecessary here because the allocation
/// refresh traffic already keeps the flow assured; the probe only has to
/// notice death promptly.
pub(crate) const PROBE_SPACING: Duration = Duration::from_secs(60);

/// Async outcomes flowing back into the runtime loop from spawned work.
pub(crate) enum WarmMsg {
    Established {
        conn: Arc<dyn RelayConn>,
        /// The allocation's relayed transport address (`worker-ip:port`).
        relayed: SocketAddr,
        /// From the ephemeral username's timestamp prefix, when parseable.
        cred_expiry_epoch_s: Option<u64>,
        /// Where liveness probes send their 1-byte permission assert —
        /// our own srflx at establish time (a real, harmless address that
        /// stays valid as a PERMISSION target even after srflx goes NONE).
        probe_dst: Option<SocketAddr>,
    },
    EstablishFailed(String),
    ProbeOk,
    ProbeFailed(String),
}

/// What `apply` observed — the runtime turns these into log lines (state
/// mutation and narration are separated so the transitions are testable).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WarmTransition {
    Established,
    EstablishFailed,
    ProbeOk,
    Lost,
}

#[derive(Default)]
pub(crate) struct WarmRelay {
    conn: Option<Arc<dyn RelayConn>>,
    relayed: Option<SocketAddr>,
    established: Option<Instant>,
    cred_expiry_epoch_s: Option<u64>,
    probe_dst: Option<SocketAddr>,
    last_probe_ok: Option<Instant>,
    probe_in_flight: bool,
    last_request: Option<Instant>,
    detail: Option<String>,
    lost: bool,
    /// One INFO per grandfather episode (probe OK while srflx is NONE) —
    /// reset when the allocation is re-established.
    pub(crate) grandfather_logged: bool,
}

impl WarmRelay {
    pub(crate) fn is_live(&self) -> bool {
        self.conn.is_some() && !self.lost
    }

    /// Time to (re)request creds? Only when nothing is live and the last
    /// request is old enough — the caller additionally gates on srflx
    /// being non-empty (proof UDP egress works right now).
    pub(crate) fn request_due(&self, now: Instant) -> bool {
        !self.is_live()
            && self
                .last_request
                .is_none_or(|t| now.duration_since(t) >= REQUEST_SPACING)
    }

    pub(crate) fn note_requested(&mut self, now: Instant) {
        self.last_request = Some(now);
    }

    pub(crate) fn probe_due(&self, now: Instant) -> bool {
        self.is_live()
            && !self.probe_in_flight
            && self
                .last_probe_ok
                .is_none_or(|t| now.duration_since(t) >= PROBE_SPACING)
    }

    /// The handles a spawned probe needs; `None` when not live or the
    /// establish-time srflx was unknown (nothing safe to assert toward).
    pub(crate) fn probe_parts(&self) -> Option<(Arc<dyn RelayConn>, SocketAddr)> {
        Some((self.conn.clone()?, self.probe_dst?))
    }

    pub(crate) fn note_probe_started(&mut self) {
        self.probe_in_flight = true;
    }

    pub(crate) fn apply(&mut self, msg: WarmMsg, now: Instant) -> WarmTransition {
        match msg {
            WarmMsg::Established {
                conn,
                relayed,
                cred_expiry_epoch_s,
                probe_dst,
            } => {
                self.conn = Some(conn);
                self.relayed = Some(relayed);
                self.established = Some(now);
                self.cred_expiry_epoch_s = cred_expiry_epoch_s;
                self.probe_dst = probe_dst;
                self.last_probe_ok = None;
                self.probe_in_flight = false;
                self.detail = None;
                self.lost = false;
                self.grandfather_logged = false;
                WarmTransition::Established
            }
            WarmMsg::EstablishFailed(e) => {
                self.detail = Some(e);
                WarmTransition::EstablishFailed
            }
            WarmMsg::ProbeOk => {
                self.probe_in_flight = false;
                self.last_probe_ok = Some(now);
                WarmTransition::ProbeOk
            }
            WarmMsg::ProbeFailed(e) => {
                // A failed permission assert through the allocation means
                // coturn no longer honours it (expiry, worker restart, or
                // the client leg's flow died). Drop the conn — holding it
                // would keep `is_live` true and starve re-establishment.
                self.probe_in_flight = false;
                self.conn = None;
                self.lost = true;
                self.detail = Some(e);
                WarmTransition::Lost
            }
        }
    }

    pub(crate) fn status(&self, now: Instant) -> crate::localapi::WarmRelayStatus {
        let epoch_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        crate::localapi::WarmRelayStatus {
            state: if self.is_live() {
                "live"
            } else if self.lost {
                "lost"
            } else {
                "none"
            }
            .to_string(),
            relayed: self.relayed.map(|r| r.to_string()),
            age_s: self
                .established
                .filter(|_| self.is_live())
                .map(|t| now.duration_since(t).as_secs()),
            cred_expiry_in_s: self
                .cred_expiry_epoch_s
                .filter(|_| self.is_live())
                .map(|e| e as i64 - epoch_now as i64),
            last_probe_ok_s: self
                .last_probe_ok
                .filter(|_| self.is_live())
                .map(|t| now.duration_since(t).as_secs()),
            detail: self.detail.clone(),
        }
    }
}

/// The expiry baked into a coturn ephemeral username (`"<epoch>:<label>"`).
pub(crate) fn ephemeral_cred_expiry_s(username: &str) -> Option<u64> {
    username.split(':').next()?.parse().ok()
}

/// The UDP-capable TURN urls out of a grant — the warm allocation is
/// UDP-or-nothing (a TCP "warm" allocation would just be today's TURNS
/// fallback with extra steps).
pub(crate) fn udp_turn_urls(urls: &[String]) -> Vec<String> {
    urls.iter()
        .filter(|u| {
            let l = u.to_ascii_lowercase();
            l.starts_with("turn:") && !l.contains("transport=tcp")
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct NopConn;
    #[async_trait::async_trait]
    impl RelayConn for NopConn {
        async fn send_to(&self, buf: &[u8], _dst: SocketAddr) -> io::Result<usize> {
            Ok(buf.len())
        }
        async fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
            std::future::pending().await
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("5.9.157.221:12795".parse().unwrap())
        }
    }

    fn established(now: Instant) -> WarmRelay {
        let mut w = WarmRelay::default();
        let t = w.apply(
            WarmMsg::Established {
                conn: Arc::new(NopConn),
                relayed: "5.9.157.221:12795".parse().unwrap(),
                cred_expiry_epoch_s: Some(u64::MAX / 2),
                probe_dst: Some("37.63.112.129:43648".parse().unwrap()),
            },
            now,
        );
        assert_eq!(t, WarmTransition::Established);
        w
    }

    /// The lifecycle the Monday VPN event must be readable through:
    /// establish → probes OK (live) → probe fails (LOST, conn dropped so
    /// re-establishment isn't starved) → status says why.
    #[test]
    fn lifecycle_establish_probe_lose() {
        let now = Instant::now();
        let mut w = established(now);
        assert!(w.is_live());
        assert!(w.probe_due(now), "first probe fires immediately");
        assert!(w.probe_parts().is_some());
        w.note_probe_started();
        assert!(!w.probe_due(now), "no overlapping probes");
        assert_eq!(w.apply(WarmMsg::ProbeOk, now), WarmTransition::ProbeOk);
        assert!(!w.probe_due(now), "just probed — not due again yet");
        assert!(w.probe_due(now + PROBE_SPACING));

        w.note_probe_started();
        assert_eq!(
            w.apply(WarmMsg::ProbeFailed("437 allocation gone".into()), now),
            WarmTransition::Lost
        );
        assert!(!w.is_live());
        let s = w.status(now);
        assert_eq!(s.state, "lost");
        assert_eq!(s.detail.as_deref(), Some("437 allocation gone"));
        assert_eq!(s.age_s, None, "a lost allocation has no age");
        // Lost ⇒ requesting again becomes due (spacing permitting).
        assert!(w.request_due(now + REQUEST_SPACING));
    }

    /// Request throttling: never hammer the control WS.
    #[test]
    fn requests_are_spaced() {
        let now = Instant::now();
        let mut w = WarmRelay::default();
        assert!(w.request_due(now));
        w.note_requested(now);
        assert!(!w.request_due(now + Duration::from_secs(1)));
        assert!(w.request_due(now + REQUEST_SPACING));
        // A live allocation never requests.
        let w = established(now);
        assert!(!w.request_due(now + REQUEST_SPACING * 10));
    }

    #[test]
    fn cred_expiry_parses_the_ephemeral_username_prefix() {
        assert_eq!(
            ephemeral_cred_expiry_s("1786831244:coturntest"),
            Some(1786831244)
        );
        assert_eq!(ephemeral_cred_expiry_s("not-a-number:x"), None);
        assert_eq!(ephemeral_cred_expiry_s(""), None);
    }

    #[test]
    fn udp_url_filter_drops_tcp_and_turns() {
        let urls = vec![
            "turn:coturn.roomler.ai:3478?transport=udp".to_string(),
            "turn:coturn.roomler.ai:3478".to_string(),
            "turn:coturn.roomler.ai:443?transport=tcp".to_string(),
            "turns:coturn.roomler.ai:443".to_string(),
        ];
        assert_eq!(
            udp_turn_urls(&urls),
            vec![
                "turn:coturn.roomler.ai:3478?transport=udp".to_string(),
                "turn:coturn.roomler.ai:3478".to_string(),
            ]
        );
    }
}
