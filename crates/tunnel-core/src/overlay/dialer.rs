//! Dialer honesty — process-wide "can this HOST raw-UDP-dial relay-band
//! ports?" latch (field 2026-08-16, CLK00017265).
//!
//! A srflx candidate only proves UDP reached a WELL-KNOWN port (STUN:3478);
//! a corp egress that whitelists STUN still drops the coturn relay band
//! (~10-13k here), so srflx-presence alone mis-assigns such a host the
//! single-relay DIALER role — its raw dial toward the anchor's relay port
//! never lands, the pair churns, P7 pins it to DERP, and the pair parks
//! "blocked" while an anchor-role pair on the same host runs perfectly
//! (clk↔jupiter, turn/udp 46 ms).
//!
//! The measurement is the failure itself: a dialer-role TURN link that dies
//! at its HANDSHAKE deadline (never completed — the only death class that
//! implicates OUR outbound path; a worked-then-died link is the peer's or
//! the allocation's death) books the peer here. Two DISTINCT peers within
//! [`CONVICTION_TTL`] ⇒ the host declares itself not-dialer-capable. The
//! flag is a HOST property (one egress policy for every org runtime), hence
//! process-wide like [`super::netstate`]'s major-transition stamp; each
//! org's [`RelayCoordinator`](super::relay_link::RelayCoordinator) mirrors
//! it into its own role inputs at sweep time (keeping the role logic
//! static-free and unit-testable), and every `rc:overlay.srflx` advert
//! carries the current value so peers + server converge within one advert
//! cadence (≤60 s; the plane forwarder re-advertises on a timer).
//!
//! ## False-positive guards (field 2026-08-16, the rc.393 latch storm)
//!
//! The first cut latched on ANY dialer-role TURN death — and the rc.393
//! rolling fleet update proved that wrong within minutes: every host's
//! restart killed its peers' dialer links mid-wave, every host saw ≥2
//! distinct-peer deaths, and jupiter/zeus/neo16-class hosts on OPEN
//! networks declared themselves not-dialer-capable (degrading their relay
//! pairs to both-allocate). Three guards make the evidence honest:
//!
//! 1. **Start grace** ([`START_GRACE`]): nothing books until the process
//!    has been up a while — a fleet roll restarts everyone in one wave, so
//!    our own early churn window is exactly the window peers restart in.
//! 2. **Conviction expiry** ([`CONVICTION_TTL`]): two convictions must be
//!    RECENT to latch — two unrelated peer restarts days apart are not
//!    evidence about our egress.
//! 3. **Latch TTL** ([`LATCH_TTL`]): the verdict expires and is re-earned.
//!    A latched host never dials (it anchors), so no positive evidence can
//!    ever clear a false latch from the inside — expiry is the escape
//!    hatch. A genuinely blocked host re-latches after ~2 conviction
//!    cycles (~2 min of churn per 30 min — absorbed by the carrier
//!    cascade), a falsely latched one just recovers.
//!
//! Reset on a MATERIAL network change (netstate Major): a new network is a
//! new egress policy — the host re-earns the verdict either way.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use tracing::info;

/// Distinct dialer-role convictions required to latch. Two, not one: a
/// single conviction can be the PEER's dead allocation or a stale advert —
/// two different peers failing the same way points at OUR egress.
const LATCH_THRESHOLD: usize = 2;

/// No booking until the process has been up this long — a rolling fleet
/// update restarts every peer inside our own early-churn window, and those
/// handshake-deadline deaths say nothing about our egress.
const START_GRACE: Duration = Duration::from_secs(240);

/// A conviction only counts toward the latch while younger than this —
/// the two distinct peers must fail TOGETHER, not days apart.
const CONVICTION_TTL: Duration = Duration::from_secs(600);

/// The latch expires and is re-earned: a latched host anchors everywhere,
/// so it can never gather positive (dial-succeeded) evidence to clear a
/// false verdict — expiry is the only self-heal.
const LATCH_TTL: Duration = Duration::from_secs(1800);

#[derive(Default)]
struct DialerState {
    /// First-touch stamp (from [`touch_start`], i.e. runtime construction).
    started: Option<Instant>,
    /// Recent handshake-deadline convictions: (peer, when).
    convictions: Vec<(ObjectId, Instant)>,
    latched_at: Option<Instant>,
}

static STATE: Mutex<DialerState> = Mutex::new(DialerState {
    started: None,
    convictions: Vec::new(),
    latched_at: None,
});

fn lock() -> std::sync::MutexGuard<'static, DialerState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stamp process start (idempotent). Called from `RelayCoordinator::new`,
/// so every org runtime's construction arms the grace clock exactly once.
pub fn touch_start() {
    let mut s = lock();
    if s.started.is_none() {
        s.started = Some(Instant::now());
    }
}

/// Current verdict: `true` until proven otherwise (or the proof expired).
pub fn udp_dialer_ok() -> bool {
    udp_dialer_ok_at(&mut lock(), Instant::now())
}

fn udp_dialer_ok_at(s: &mut DialerState, now: Instant) -> bool {
    if let Some(at) = s.latched_at
        && now.duration_since(at) >= LATCH_TTL
    {
        info!(
            "overlay dialer-honesty: not-dialer-capable latch expired — re-earning the verdict on fresh evidence"
        );
        s.latched_at = None;
        s.convictions.clear();
    }
    s.latched_at.is_none()
}

/// Book a dialer-role handshake-deadline conviction against `peer`.
/// Returns `true` when THIS call flipped the host to not-dialer-capable.
/// The CALLER filters on death class — only a never-handshook
/// (`DeathReason::HandshakeDeadline`) dialer-role TURN death is evidence.
pub fn note_dialer_conviction(peer: ObjectId) -> bool {
    note_dialer_conviction_at(&mut lock(), peer, Instant::now())
}

fn note_dialer_conviction_at(s: &mut DialerState, peer: ObjectId, now: Instant) -> bool {
    // Guard 1 — start grace: a fleet-roll wave books nothing.
    match s.started {
        Some(t0) if now.duration_since(t0) >= START_GRACE => {}
        _ => return false,
    }
    // Guard 2 — expiry: only recent convictions count.
    s.convictions
        .retain(|(_, at)| now.duration_since(*at) < CONVICTION_TTL);
    if !s.convictions.iter().any(|(p, _)| *p == peer) {
        s.convictions.push((peer, now));
        // Bounded: the threshold fires long before this matters.
        if s.convictions.len() > 16 {
            s.convictions.remove(0);
        }
    }
    let latch = s.convictions.len() >= LATCH_THRESHOLD && udp_dialer_ok_at(s, now);
    if latch {
        s.latched_at = Some(now);
        info!(
            distinct_peers = s.convictions.len(),
            "overlay dialer-honesty: raw-UDP dials failed at the handshake deadline against multiple peers — this host \
             cannot reach relay-band ports; declaring not-dialer-capable (will ANCHOR henceforth, \
             advertised with the next srflx trickle)"
        );
    }
    latch
}

/// Netstate-Major hook: a new network is a new egress policy — reset and
/// re-earn.
pub fn reset_on_network_change() {
    let mut s = lock();
    let was_latched = s.latched_at.take().is_some();
    s.convictions.clear();
    if was_latched {
        info!(
            "overlay dialer-honesty: network changed — resetting the not-dialer-capable latch \
             (re-earned on fresh evidence)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh state with the start stamp `age` in the past (synthetic clock
    /// built by ADDING to a base `Instant` — subtracting can underflow the
    /// monotonic epoch on a fresh runner).
    fn state_started() -> (DialerState, Instant) {
        let t0 = Instant::now();
        (
            DialerState {
                started: Some(t0),
                convictions: Vec::new(),
                latched_at: None,
            },
            t0,
        )
    }

    #[test]
    fn start_grace_absorbs_the_fleet_roll_wave() {
        let (mut s, t0) = state_started();
        let (a, b) = (ObjectId::new(), ObjectId::new());
        // Inside the grace: even many distinct peers book NOTHING — this is
        // the rc.393 latch-storm scenario.
        for peer in [a, b, ObjectId::new()] {
            assert!(!note_dialer_conviction_at(
                &mut s,
                peer,
                t0 + Duration::from_secs(70)
            ));
        }
        assert!(s.convictions.is_empty(), "grace must not even accumulate");
        assert!(udp_dialer_ok_at(&mut s, t0 + Duration::from_secs(100)));

        // Past the grace the same evidence latches on the second peer.
        let t = t0 + START_GRACE + Duration::from_secs(1);
        assert!(!note_dialer_conviction_at(&mut s, a, t));
        assert!(note_dialer_conviction_at(
            &mut s,
            b,
            t + Duration::from_secs(50)
        ));
        assert!(!udp_dialer_ok_at(&mut s, t + Duration::from_secs(51)));
    }

    #[test]
    fn unstamped_start_never_books() {
        let mut s = DialerState::default();
        assert!(!note_dialer_conviction_at(
            &mut s,
            ObjectId::new(),
            Instant::now()
        ));
        assert!(s.convictions.is_empty());
    }

    #[test]
    fn stale_convictions_expire_so_unrelated_restarts_never_accumulate() {
        let (mut s, t0) = state_started();
        let t = t0 + START_GRACE;
        let (a, b) = (ObjectId::new(), ObjectId::new());
        assert!(!note_dialer_conviction_at(&mut s, a, t));
        // Second peer fails AFTER the first conviction expired — two
        // unrelated events, not egress evidence.
        let late = t + CONVICTION_TTL + Duration::from_secs(1);
        assert!(!note_dialer_conviction_at(&mut s, b, late));
        assert!(udp_dialer_ok_at(&mut s, late));
        // A third peer failing promptly after b DOES latch (b + c recent).
        assert!(note_dialer_conviction_at(
            &mut s,
            ObjectId::new(),
            late + Duration::from_secs(30)
        ));
    }

    #[test]
    fn same_peer_repeating_never_latches() {
        let (mut s, t0) = state_started();
        let t = t0 + START_GRACE;
        let a = ObjectId::new();
        for i in 0..5 {
            assert!(!note_dialer_conviction_at(
                &mut s,
                a,
                t + Duration::from_secs(i * 50)
            ));
        }
        assert!(udp_dialer_ok_at(&mut s, t + Duration::from_secs(300)));
    }

    #[test]
    fn latch_expires_and_the_host_re_earns_it() {
        let (mut s, t0) = state_started();
        let t = t0 + START_GRACE;
        let (a, b) = (ObjectId::new(), ObjectId::new());
        assert!(!note_dialer_conviction_at(&mut s, a, t));
        assert!(note_dialer_conviction_at(
            &mut s,
            b,
            t + Duration::from_secs(10)
        ));
        assert!(!udp_dialer_ok_at(&mut s, t + Duration::from_secs(20)));
        // The TTL elapses — a falsely latched host recovers on its own; a
        // genuinely blocked one re-latches within two conviction cycles.
        let after = t + Duration::from_secs(10) + LATCH_TTL;
        assert!(udp_dialer_ok_at(&mut s, after));
        assert!(s.convictions.is_empty(), "expiry clears the evidence too");
        assert!(!note_dialer_conviction_at(
            &mut s,
            a,
            after + Duration::from_secs(5)
        ));
        assert!(note_dialer_conviction_at(
            &mut s,
            b,
            after + Duration::from_secs(60)
        ));
    }

    #[test]
    fn network_change_resets_everything() {
        let (mut s, t0) = state_started();
        let t = t0 + START_GRACE;
        assert!(!note_dialer_conviction_at(&mut s, ObjectId::new(), t));
        assert!(note_dialer_conviction_at(
            &mut s,
            ObjectId::new(),
            t + Duration::from_secs(5)
        ));
        // The public reset (statics) is exercised via the wrappers in the
        // process-wide test below; here mirror its effect on the core.
        s.latched_at = None;
        s.convictions.clear();
        assert!(udp_dialer_ok_at(&mut s, t + Duration::from_secs(10)));
        assert!(!note_dialer_conviction_at(
            &mut s,
            ObjectId::new(),
            t + Duration::from_secs(20)
        ));
    }

    /// One test drives the PROCESS-WIDE wrappers — the statics are shared,
    /// so parallel tests over them would race. Everything time-sensitive is
    /// covered by the `_at` tests above; this locks the wiring only.
    #[test]
    fn process_wide_wrappers_wire_the_core() {
        reset_on_network_change();
        touch_start();
        assert!(udp_dialer_ok());
        // Inside the start grace: booking is a no-op regardless of peers.
        assert!(!note_dialer_conviction(ObjectId::new()));
        assert!(!note_dialer_conviction(ObjectId::new()));
        assert!(udp_dialer_ok());
        reset_on_network_change();
    }
}
