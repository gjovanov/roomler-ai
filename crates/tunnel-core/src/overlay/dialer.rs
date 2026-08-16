//! Dialer honesty — process-wide "can this HOST raw-UDP-dial relay-band
//! ports?" latch (field 2026-08-16, CORPLAP-3).
//!
//! A srflx candidate only proves UDP reached a WELL-KNOWN port (STUN:3478);
//! a corp egress that whitelists STUN still drops the coturn relay band
//! (~10-13k here), so srflx-presence alone mis-assigns such a host the
//! single-relay DIALER role — its raw dial toward the anchor's relay port
//! never lands, the pair churns, P7 pins it to DERP, and the pair parks
//! "blocked" while an anchor-role pair on the same host runs perfectly
//! (CORPLAP-3↔jupiter, turn/udp 46 ms).
//!
//! The measurement is the failure itself: every dialer-role TURN conviction
//! ("relay carrier one-way / rekey-unanswered / rx-stale" while WE were the
//! raw dialer) books the peer here. Two DISTINCT peers within one latch
//! lifetime ⇒ the host declares itself not-dialer-capable. The flag is a
//! HOST property (one egress policy for every org runtime), hence
//! process-wide like [`super::netstate`]'s major-transition stamp; each
//! org's [`RelayCoordinator`](super::relay_link::RelayCoordinator) mirrors
//! it into its own role inputs at sweep time (keeping the role logic
//! static-free and unit-testable), and every `rc:overlay.srflx` advert
//! carries the current value so peers + server converge within one advert
//! cadence (≤60 s; the plane forwarder re-advertises on a timer).
//!
//! Reset on a MATERIAL network change (netstate Major): a new network is a
//! new egress policy — the host re-earns the verdict, at the cost of at
//! most one more failed dialer cycle if the policy is unchanged.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bson::oid::ObjectId;
use tracing::info;

/// Distinct dialer-role convictions required to latch. Two, not one: a
/// single conviction can be the PEER's dead allocation or a stale advert —
/// two different peers failing the same way points at OUR egress.
const LATCH_THRESHOLD: usize = 2;

static DIALER_UDP_OK: AtomicBool = AtomicBool::new(true);
static CONVICTED_PEERS: Mutex<Vec<ObjectId>> = Mutex::new(Vec::new());

/// Current verdict: `true` until proven otherwise.
pub fn udp_dialer_ok() -> bool {
    DIALER_UDP_OK.load(Ordering::Relaxed)
}

/// Book a dialer-role TURN conviction against `peer`. Returns `true` when
/// THIS call flipped the host to not-dialer-capable.
pub fn note_dialer_conviction(peer: ObjectId) -> bool {
    let mut set = CONVICTED_PEERS.lock().unwrap_or_else(|e| e.into_inner());
    if !set.contains(&peer) {
        set.push(peer);
        // Bounded: the threshold fires long before this matters.
        if set.len() > 16 {
            set.remove(0);
        }
    }
    let latch = set.len() >= LATCH_THRESHOLD && DIALER_UDP_OK.swap(false, Ordering::Relaxed);
    if latch {
        info!(
            distinct_peers = set.len(),
            "overlay dialer-honesty: raw-UDP dial failed against multiple peers — this host \
             cannot reach relay-band ports; declaring not-dialer-capable (will ANCHOR henceforth, \
             advertised with the next srflx trickle)"
        );
    }
    latch
}

/// Netstate-Major hook: a new network is a new egress policy — reset and
/// re-earn.
pub fn reset_on_network_change() {
    let mut set = CONVICTED_PEERS.lock().unwrap_or_else(|e| e.into_inner());
    let was_latched = !DIALER_UDP_OK.swap(true, Ordering::Relaxed);
    set.clear();
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

    /// One test drives the whole lifecycle — the statics are process-wide,
    /// so splitting into parallel tests would race.
    #[test]
    fn latch_needs_two_distinct_peers_and_resets_on_network_change() {
        reset_on_network_change();
        let (a, b) = (ObjectId::new(), ObjectId::new());
        assert!(!note_dialer_conviction(a), "first peer must not latch");
        assert!(udp_dialer_ok());
        assert!(
            !note_dialer_conviction(a),
            "same peer repeating must not latch"
        );
        assert!(udp_dialer_ok());
        assert!(note_dialer_conviction(b), "second DISTINCT peer latches");
        assert!(!udp_dialer_ok());
        assert!(
            !note_dialer_conviction(ObjectId::new()),
            "already latched — no re-flip"
        );
        reset_on_network_change();
        assert!(udp_dialer_ok(), "network change resets the latch");
        assert!(
            !note_dialer_conviction(ObjectId::new()),
            "post-reset the set is empty — one peer is not enough again"
        );
    }
}
