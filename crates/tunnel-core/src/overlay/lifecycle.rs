//! P2 — the ONE carrier lifecycle (consolidation invariant I2).
//!
//! Every liveness rule that can kill an overlay carrier lives here as a pure,
//! clock-free transition: *a carrier must HANDSHAKE within its tier deadline,
//! then keep being HEARD within the staleness bound, else it dies with a typed
//! [`DeathReason`]*. Before P2 these rules were four ad-hoc booleans accreted
//! across incidents (`hard_dead` rc.181, `punch_dead` Phase C/rc.204/rc.223,
//! `rx_stale` rc.206, one-way `bad_sweeps` rc.137) scattered through
//! `sweep_carrier_health`; the sweep now gathers inputs, calls
//! [`carrier_tick`], and dispatches the verdict — the rules themselves are
//! testable without tokio, sockets, or time.
//!
//! ## The lifecycle, derived — never stored
//!
//! A carrier's phase is DERIVED from the monotonic handshake latch
//! (`PeerStats::handshake` — boringtun exposes no rekey callback, so "never
//! handshaked" is inferred from `time_since_last_handshake().is_none()` + a
//! deadline, exactly as before):
//!
//! * [`CarrierPhase::Installing`] — handshake not latched. The only death is
//!   [`DeathReason::HandshakeDeadline`] (per-tier: LAN/srflx 12 s, public
//!   30 s, relay 45 s) — plus [`DeathReason::HardDead`] for a relay whose
//!   send hard-errored.
//! * [`CarrierPhase::Established`] — handshake latched (set-once). The deaths
//!   are [`DeathReason::RxStale`] (nothing heard — keepalives included — for
//!   the tier's [`DirectTier::rx_stale_deadline`]: direct 60 s, relay 90 s) and
//!   [`DeathReason::OneWay`] (tx advancing, rx flat, 3 sweeps).
//!
//! `Installing × RxStale` and `Established × HandshakeDeadline` are
//! structurally unreachable — the phase split makes the old comment-level
//! claims ("this can never fire again once the handshake latches") a matter
//! of construction.
//!
//! Storing the phase would duplicate the latch's truth (and break the tests
//! that literal-construct `Installed`); deriving it keeps `Installed`
//! field-for-field identical to pre-P2.
//!
//! ## Counter mechanics (corrected, was mis-stated pre-P2)
//!
//! Pre-handshake, `tx` is NOT strictly flat: boringtun's `encapsulate` with no
//! session queues the data packet and emits the handshake INITIATION as
//! `WriteToNetwork`, which the send path counts (+1 per rekey attempt, ~90 s
//! apart; timer-task retransmits ride `update_timers` and do NOT count). The
//! one-way detector still cannot fire pre-handshake because an isolated
//! single-sweep tx blip resets on the next sweep — locked by
//! `single_sweep_tx_blip_does_not_accumulate_bad_sweeps`.
//!
//! ## Dispositions (the sweep's half, unchanged by P2)
//!
//! | Trigger | Disposition |
//! |---|---|
//! | sweep death (any [`DeathReason`]) | teardown → tier cooldown/strike (direct) or refresh-cooldown (relay) → immediate re-request |
//! | probe expiry ([`ProbeVerdict::Expire`]) | drop probe, KEEP relay, book tier strike |
//! | netmap removal | park (peer left the mesh) |
//! | resume-from-suspend | mass drop + full re-coordinate |
//!
//! ## Boundary: the forced-DERP TTL pin is NOT lifecycle
//!
//! `RelayCoordinator::forced_derp_until` stays in `relay_link.rs`. It is
//! next-establishment *strategy* (which relay sub-tier the next (re)build
//! picks), server-pushed and TTL'd — its whole purpose is to OUTLIVE the
//! carriers it steers (the health sweep tears a carrier down; the pin shapes
//! the rebuild). Attaching it to a per-carrier lifecycle whose instance dies
//! with the carrier would be wrong by construction.

use std::time::Duration;

/// Which carrier tier an installed peer is on. Direct tiers differ in cooldown
/// bookkeeping (CC1 — a failure on one tier must never poison another) and
/// each tier carries a WG-handshake completion deadline (a carrier that never
/// establishes is torn down; direct tiers fall back to relay, the relay tier
/// re-coordinates).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum DirectTier {
    /// Same-subnet LAN direct (rc.131-135) — on-link. rc.204: gets a TIGHT
    /// handshake deadline too — pre-handshake tx/rx stay (near-)flat, so
    /// without a deadline a false LAN match (stale endpoint, AP isolation,
    /// VPN-captured reply path) was a PERMANENT zombie with no relay fallback.
    Lan,
    /// Direct-to-public NIC (Phase A) — off-link; public cooldown + a loose
    /// handshake deadline (the accept side may lag).
    Public,
    /// srflx hole-punch (Phase C) — off-link; srflx cooldown + a tight
    /// handshake deadline (a cross-NAT punch works in a couple of INIT cycles
    /// or won't at all).
    Srflx,
    /// coturn relay carrier — not a direct tier; governed by the hard-dead /
    /// one-way / rx-stale relay signals PLUS (rc.223) its own handshake
    /// deadline.
    Relay,
}

impl DirectTier {
    /// True for the direct tiers (everything but [`Relay`](Self::Relay)) — the
    /// carriers whose failure bookkeeping is keyed by tier.
    pub(crate) fn is_direct(self) -> bool {
        !matches!(self, DirectTier::Relay)
    }

    /// Phase C — the WG-handshake completion deadline past which a
    /// never-established carrier on this tier is torn down (direct tiers →
    /// relay fallback; relay tier → re-coordinate).
    pub(crate) fn handshake_deadline(self) -> Duration {
        match self {
            DirectTier::Srflx => SRFLX_HANDSHAKE_DEADLINE,
            DirectTier::Public => PUBLIC_HANDSHAKE_DEADLINE,
            // rc.204 — LAN gets a deadline too (see the variant doc): on-link
            // handshakes complete in milliseconds or not at all.
            DirectTier::Lan => LAN_HANDSHAKE_DEADLINE,
            // rc.223 — the RELAY tier gets one as well. A relay carrier that
            // never completes its handshake evaded EVERY detector (field
            // 2026-07-24, neo16 on a UDP-hostile network): `punch_dead` was
            // direct-only, `rx_stale` requires a latched handshake,
            // `hard_dead` needs a send error (sends into a TURNS/TCP
            // allocation "succeed"), and the pre-handshake one-way counter
            // resets between rekey attempts (an init tx-blip is isolated, ~one
            // per 90 s), so `bad_sweeps` never accumulates — an IMMORTAL
            // zombie that also starved the P7 churn counter (no teardown → no
            // re-request → no forced-DERP escalation). Generous deadline: the
            // handshake needs BOTH ends' allocations installed, and the
            // peer's own grant cycle can lag ours by tens of seconds.
            DirectTier::Relay => RELAY_HANDSHAKE_DEADLINE,
        }
    }

    /// rc.206's absolute rx-staleness backstop, per tier.
    ///
    /// Split out of the former single global because the two cases are sized
    /// against different things. Both must clear the ~25 s persistent
    /// keepalive (`wg::KEEPALIVE_SECS`) with room for losses, but:
    ///
    /// * a **direct** carrier dies the instant its path is filtered — a
    ///   full-tunnel VPN connecting drops every off-tunnel datagram at once
    ///   (field 2026-08-08, CORPLAP-1/Check Point: `ping` went from a steady
    ///   75 ms to total loss, and the mesh stayed dark for MINUTES waiting out
    ///   this deadline). 60 s still tolerates two consecutive lost keepalives,
    ///   and a false trip only forces a rebuild that re-establishes if the
    ///   path really recovered;
    /// * a **relay** carrier's liveness depends on a third party (coturn /
    ///   the DERP WS) whose own reconnects can legitimately straddle a
    ///   keepalive or two, so it keeps the original, more forgiving 90 s.
    ///
    /// This is the cheap half of the recovery work — it needs no new state, no
    /// probe, and no platform code. The active liveness poke that takes this
    /// below ~30 s is a separate change.
    pub(crate) fn rx_stale_deadline(self) -> Duration {
        if self.is_direct() {
            DIRECT_RX_STALE_DEADLINE
        } else {
            RELAY_RX_STALE_DEADLINE
        }
    }
}

/// Grace after install before the fallback can fire — lets the bilateral
/// handshake + first packets flow before we judge the carrier.
pub(crate) const DIRECT_GRACE: Duration = Duration::from_secs(8);
/// Consecutive bad sweeps (sent, received nothing) before falling back. At the
/// 5 s tick that's ~15 s of one-way traffic — long enough to ignore a blip,
/// short enough that a VPN/AP-isolation break doesn't stay dark for long.
pub(crate) const BAD_SWEEPS_TO_FALLBACK: u32 = 3;
/// rc.206 — the "silent zombie" backstop. An *established* carrier that stops
/// RECEIVING is dead even when it also stopped SENDING: a healthy peer emits a
/// WireGuard persistent-keepalive every ~25 s (`wg::KEEPALIVE_SECS`), so no
/// inbound packet for this long means the underlying path died AND boringtun
/// gave up re-handshaking (it stops emitting anything once a rekey attempt
/// expires ~90 s). With no tx either, the `tx>last_tx && rx==last_rx`
/// heuristic reads that as "just idle — no judgment" and never tears the
/// carrier down — observed in the field as an 8-hour "direct" carrier stuck at
/// 100 % loss with a frozen last-seen. This absolute rx-staleness deadline
/// catches it regardless of tx. A false trip only forces a (cheap) rebuild,
/// which re-establishes if the path actually recovered.
///
/// Sized per tier by [`DirectTier::rx_stale_deadline`] — read that for why the
/// two differ.
///
/// 60 s ≈ 2 missed keepalives. Chosen over the original 90 s because a direct
/// carrier's path can be revoked instantly (a full-tunnel VPN connecting), and
/// the old value was the whole reason the mesh stayed dark for minutes after
/// one.
pub(crate) const DIRECT_RX_STALE_DEADLINE: Duration = Duration::from_secs(60);
/// 90 s ≈ 3–4 missed keepalives — the original value, kept for the relay tier
/// (see [`DirectTier::rx_stale_deadline`]).
pub(crate) const RELAY_RX_STALE_DEADLINE: Duration = Duration::from_secs(90);
/// Phase C — the WG-handshake completion deadline for a srflx punch carrier:
/// past it with no session, the punch failed → tear down to relay. Tight —
/// bilateral INIT retransmit is ~5 s, so ~2 cycles + jitter + RTT covers a
/// genuine cross-NAT punch; longer just delays the relay fallback for a pair
/// that can't punch (e.g. one side symmetric).
pub(crate) const SRFLX_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(12);
/// Phase C — the handshake deadline for a public-direct (Phase A) carrier.
/// Looser than srflx: the accept side (a NAT'd client dialling a public exit)
/// can lag, and public-NIC reachability rarely fails outright, so we don't
/// rush it to relay. Still finite so a truly dead public dst can't zombie
/// forever (closes the same latent Phase A gap the srflx work exposed).
pub(crate) const PUBLIC_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);
/// rc.223 — RELAY handshake deadline (see `handshake_deadline`'s Relay arm:
/// the never-handshaked relay was an immortal zombie invisible to every other
/// detector). Generous: the handshake needs both ends' allocations up, and
/// the peer's grant/allocate cycle can lag ours; a teardown just
/// re-coordinates (rate-limited by `RELAY_REFRESH_COOLDOWN`), and each
/// re-request feeds the P7 churn counter toward the forced-DERP escalation —
/// exactly the intended cascade on a network where TURN can't carry data.
pub(crate) const RELAY_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(45);
/// rc.204 — LAN handshake deadline. On-link, so a genuine same-subnet
/// handshake completes in milliseconds; one that hasn't completed by this
/// window is a false LAN match (stale/foreign endpoint, Wi-Fi AP isolation, a
/// VPN-captured reply path). Pre-rc.204 the LAN tier had NO deadline, and a
/// never-handshaken carrier's counters stay (near-)flat, so the rx-flat
/// heuristic never fired either — the pair was a permanent zombie with no
/// relay fallback (field-observed 2026-07-21: every LAN pair wedged in
/// `HANDSHAKE(REKEY_TIMEOUT)` while boringtun gave up after ~90 s). As tight
/// as srflx: it either establishes near-instantly or never will.
pub(crate) const LAN_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(12);

/// Why a carrier died. Precedence when several signals co-trip (mirrors the
/// pre-P2 log selection, which keyed off `(hard_dead, rx_stale)` in this
/// order): `HardDead` > `RxStale` > `HandshakeDeadline` > `OneWay`. The
/// dispatch half maps a reason to the SAME log line the old booleans chose —
/// on the direct side `HandshakeDeadline` and `OneWay` share the
/// "didn't establish" string, on the relay side they share "one-way", exactly
/// as before.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeathReason {
    /// rc.181 — the carrier's send hard-errored (TURNS/TCP reset, lost
    /// QUIC-over-TURN connection). Relay-only in practice (a direct send
    /// failure is a dropped datagram, not a dead session); skips the warm-up
    /// grace.
    HardDead,
    /// rc.206 — established, then nothing heard (keepalives included) within
    /// [`DirectTier::rx_stale_deadline`].
    RxStale,
    /// Phase C / rc.204 / rc.223 — never handshaked within the tier's
    /// [`DirectTier::handshake_deadline`].
    HandshakeDeadline,
    /// rc.137 — tx advancing while rx stayed flat for
    /// [`BAD_SWEEPS_TO_FALLBACK`] consecutive sweeps.
    OneWay,
}

/// The lifecycle phase, DERIVED from the monotonic handshake latch — never
/// stored (see the module doc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CarrierPhase {
    /// Handshake not latched; must latch within `deadline` or die
    /// [`DeathReason::HandshakeDeadline`].
    Installing { deadline: Duration },
    /// Handshake latched (set-once); governed by rx-staleness and the one-way
    /// counter.
    Established,
}

impl CarrierPhase {
    /// Derive the phase for a carrier on `tier` with the given latch state.
    pub(crate) fn of(handshake_done: bool, tier: DirectTier) -> Self {
        if handshake_done {
            CarrierPhase::Established
        } else {
            CarrierPhase::Installing {
                deadline: tier.handshake_deadline(),
            }
        }
    }
}

/// Everything [`carrier_tick`] needs about one installed carrier, gathered by
/// the sweep from the lock-free `PeerStats` reads + `Installed` bookkeeping.
/// All durations are relative to the sweep's single `now` so the transition
/// is clock-free.
pub(crate) struct HealthInputs {
    pub(crate) tier: DirectTier,
    /// `Installed.is_direct` (drives the strike-clear + relay holdoff rules).
    pub(crate) is_direct: bool,
    /// `WgDevice::peer_carrier_dead` — the rc.181 send-error latch.
    pub(crate) hard_dead: bool,
    /// `WgDevice::peer_handshake_done` — the monotonic latch the phase derives
    /// from.
    pub(crate) handshake_done: bool,
    /// `Installed.since.elapsed()` — age of this carrier.
    pub(crate) since_install: Duration,
    /// `now - Installed.last_rx_at` — how long since we last HEARD the peer
    /// (keepalive-inclusive; the sweep advances `last_rx_at` from `rx_any`
    /// BEFORE building these inputs).
    pub(crate) since_last_rx: Duration,
    /// This sweep's `(tx, rx)` snapshot.
    pub(crate) traffic: (u64, u64),
    /// The previous sweep's `(tx, rx)` snapshot.
    pub(crate) last_traffic: (u64, u64),
    /// The one-way strike counter carried in `Installed.bad_sweeps`.
    pub(crate) bad_sweeps: u32,
    /// A relay refresh happened within `RELAY_REFRESH_COOLDOWN` — the
    /// anti-ping-pong holdoff. The tick applies it only to relay carriers.
    pub(crate) relay_refresh_held: bool,
}

/// [`carrier_tick`]'s result: the updated one-way counter (store back into
/// `Installed.bad_sweeps`), whether the carrier's OWN tier strikes should
/// clear (direct + genuinely receiving — CC1: never cross-tier), and the
/// death verdict, if any.
pub(crate) struct HealthVerdict {
    pub(crate) bad_sweeps: u32,
    pub(crate) clear_tier_strikes: bool,
    pub(crate) death: Option<DeathReason>,
}

/// The ONE carrier-health transition (pre-P2: four ad-hoc booleans inline in
/// `sweep_carrier_health`). Pure and clock-free — parity contract with the
/// pre-P2 order, including the three subtleties:
///
/// * the warm-up grace early-return skips the counter update entirely (a
///   hard-dead relay conclusively skips the grace);
/// * the relay refresh-holdoff defers the death verdict but the updated
///   counter still returns (the pre-P2 `continue` sat after the counter
///   write);
/// * the strike-clear fires only for a direct carrier whose rx genuinely
///   advanced (not merely idle).
pub(crate) fn carrier_tick(i: &HealthInputs) -> HealthVerdict {
    // Phase-split detection: HandshakeDeadline is Installing-only, RxStale is
    // Established-only — structurally disjoint (see the module doc).
    let (punch_dead, rx_stale) = match CarrierPhase::of(i.handshake_done, i.tier) {
        CarrierPhase::Installing { deadline } => (i.since_install > deadline, false),
        CarrierPhase::Established => (
            false,
            i.since_install >= DIRECT_GRACE && i.since_last_rx > i.tier.rx_stale_deadline(),
        ),
    };

    // Warm-up grace: let the handshake + first packets flow — no counter
    // update, no judgment. (A blown handshake deadline is > grace by
    // construction, so it never lands in the grace window; a hard-dead relay
    // conclusively skips it.)
    if !i.hard_dead && i.since_install < DIRECT_GRACE {
        return HealthVerdict {
            bad_sweeps: i.bad_sweeps,
            clear_tier_strikes: false,
            death: None,
        };
    }

    // One-way counter: sent this interval but received nothing back ⇒
    // suspect; anything else resets (idle = no judgment). A direct carrier
    // that's genuinely RECEIVING clears its tier's strikes (CC1).
    let (tx, rx) = i.traffic;
    let (last_tx, last_rx) = i.last_traffic;
    let (bad_sweeps, clear_tier_strikes) = if tx > last_tx && rx == last_rx {
        (i.bad_sweeps + 1, false)
    } else {
        (0, i.is_direct && rx > last_rx)
    };

    let tripped = bad_sweeps >= BAD_SWEEPS_TO_FALLBACK || i.hard_dead || punch_dead || rx_stale;
    let death = if !tripped {
        None
    } else if !i.is_direct && i.relay_refresh_held {
        // Anti-ping-pong: we just refreshed this relay — hold the verdict
        // (the counter above still stores).
        None
    } else if i.hard_dead {
        Some(DeathReason::HardDead)
    } else if rx_stale {
        Some(DeathReason::RxStale)
    } else if punch_dead {
        Some(DeathReason::HandshakeDeadline)
    } else {
        Some(DeathReason::OneWay)
    };
    HealthVerdict {
        bad_sweeps,
        clear_tier_strikes,
        death,
    }
}

/// A make-before-break shadow probe's verdict (pre-P2: the two branch
/// conditions inline in `sweep_upgrade_probes`). A probe has a disjoint input
/// set from an installed carrier (no stats, no hard-dead, no rx-staleness —
/// it never routes) and disjoint dispositions (Promote = cut over; Expire =
/// park + strike), which is why this is a separate tiny fn and NOT a merged
/// phase of [`carrier_tick`] — merging would manufacture unreachable states.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ProbeVerdict {
    /// Handshake latched — the direct path is proven bidirectional; cut over.
    Promote,
    /// Deadline blown with no latch — direct unreachable; drop the probe,
    /// keep the relay, book the tier strike.
    Expire,
    /// Still within the deadline — leave it in flight.
    Wait,
}

/// The probe transition: latch ⇒ promote, past the tier deadline ⇒ expire,
/// else wait.
pub(crate) fn probe_tick(handshake_done: bool, since: Duration, tier: DirectTier) -> ProbeVerdict {
    if handshake_done {
        ProbeVerdict::Promote
    } else if since > tier.handshake_deadline() {
        ProbeVerdict::Expire
    } else {
        ProbeVerdict::Wait
    }
}

/// rc.275 honesty — is this installed carrier SILENTLY ONE-WAY right now?
/// Display verdict only (stored in `Installed.stalled`, surfaced as `stalled`
/// in the LocalAPI peer view) — it kills nothing; every reap decision stays
/// in [`carrier_tick`].
///
/// Two evidence sources, matching the two ways a one-way carrier hides:
/// * **no completed WG handshake ever** — the pre-handshake zombie. Its
///   `tx`/`rx` counters stay flat (handshake packets touch neither), so the
///   rx-flat heuristic can't see it; only the handshake latch can. Field:
///   CORPLAP-1 behind its corp VPN — every tier "installed", `roomler peers`
///   said `direct`/`relay` with fresh LAST SEEN (inbound worked!) while 100 %
///   of its own pings died for hours. This fn is what makes that visible.
/// * **the rc.137 one-way strike counter** (`bad_sweeps`) — an established
///   session that sends but no longer receives.
///
/// The warm-up grace mirrors [`carrier_tick`]'s: no judgment while the
/// handshake + first packets are still expected to be in flight.
pub(crate) fn carrier_stalled(
    since_install: Duration,
    handshake_done: bool,
    bad_sweeps: u32,
) -> bool {
    since_install >= DIRECT_GRACE && (!handshake_done || bad_sweeps >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rc.275 honesty — the stalled display verdict: pre-handshake zombies and
    /// one-way strike accumulation read as stalled once past the grace; a
    /// healthy or still-warming carrier never does.
    #[test]
    fn carrier_stalled_matrix() {
        let grace = DIRECT_GRACE;
        let young = grace / 2;
        let old = grace * 3;
        // Warm-up: never stalled, whatever the evidence.
        assert!(!carrier_stalled(young, false, 0));
        assert!(!carrier_stalled(young, false, 9));
        // The CORPLAP-1 shape: installed for ages, handshake never completed.
        assert!(carrier_stalled(old, false, 0));
        // Established + healthy: not stalled.
        assert!(!carrier_stalled(old, true, 0));
        assert!(!carrier_stalled(old, true, 1), "one strike is a blip");
        // Established but one-way strikes accumulating: stalled.
        assert!(carrier_stalled(old, true, 2));
        // Boundary: judgment starts exactly at the grace edge.
        assert!(carrier_stalled(grace, false, 0));
        assert!(!carrier_stalled(grace - Duration::from_millis(1), false, 0));
    }

    /// Baseline healthy-established inputs; tests override the field under
    /// test.
    fn established() -> HealthInputs {
        HealthInputs {
            tier: DirectTier::Lan,
            is_direct: true,
            hard_dead: false,
            handshake_done: true,
            since_install: Duration::from_secs(60),
            since_last_rx: Duration::from_secs(1),
            traffic: (10, 10),
            last_traffic: (10, 10),
            bad_sweeps: 0,
            relay_refresh_held: false,
        }
    }

    #[test]
    fn grace_suppresses_all_but_hard_dead() {
        // Inside the 8 s grace: a one-way pattern AND a blown-deadline-shaped
        // input yield NO death and an UNTOUCHED counter (the pre-P2 `continue`
        // skipped the counter update).
        let i = HealthInputs {
            handshake_done: false,
            since_install: Duration::from_secs(7),
            traffic: (5, 0),
            last_traffic: (1, 0),
            bad_sweeps: 2,
            ..established()
        };
        let v = carrier_tick(&i);
        assert!(v.death.is_none());
        assert_eq!(v.bad_sweeps, 2, "grace must not touch the counter");
        assert!(!v.clear_tier_strikes);

        // hard_dead skips the grace entirely (rc.181 fast path).
        let i = HealthInputs {
            tier: DirectTier::Relay,
            is_direct: false,
            hard_dead: true,
            since_install: Duration::from_secs(1),
            ..established()
        };
        assert_eq!(carrier_tick(&i).death, Some(DeathReason::HardDead));
    }

    #[test]
    fn handshake_deadline_fires_only_pre_handshake_per_tier() {
        for (tier, deadline_s) in [
            (DirectTier::Lan, 12u64),
            (DirectTier::Srflx, 12),
            (DirectTier::Public, 30),
            (DirectTier::Relay, 45),
        ] {
            // One second short of the deadline: Wait.
            let i = HealthInputs {
                tier,
                is_direct: tier.is_direct(),
                handshake_done: false,
                since_install: Duration::from_secs(deadline_s - 1),
                ..established()
            };
            assert!(
                carrier_tick(&i).death.is_none(),
                "{tier:?} must survive 1 s short of its deadline"
            );
            // One second past: HandshakeDeadline.
            let i = HealthInputs {
                since_install: Duration::from_secs(deadline_s + 1),
                ..i
            };
            assert_eq!(
                carrier_tick(&i).death,
                Some(DeathReason::HandshakeDeadline),
                "{tier:?} must die 1 s past its deadline"
            );
            // Latched handshake: the deadline can NEVER fire (Established
            // phase has no HandshakeDeadline arm — structural).
            let i = HealthInputs {
                handshake_done: true,
                since_last_rx: Duration::from_secs(1),
                ..i
            };
            assert!(carrier_tick(&i).death.is_none());
        }
    }

    #[test]
    fn rx_stale_requires_handshake_grace_and_deadline() {
        // Just under the tier's deadline: survives. Just over: dies RxStale.
        // Expressed RELATIVE to the deadline so the boundary can't silently
        // re-hardcode when a tier's value is retuned.
        for (tier, is_direct) in [
            (DirectTier::Lan, true),
            (DirectTier::Srflx, true),
            (DirectTier::Relay, false),
        ] {
            let d = tier.rx_stale_deadline();
            let at = |since_last_rx: Duration| HealthInputs {
                tier,
                is_direct,
                since_last_rx,
                ..established()
            };
            assert!(
                carrier_tick(&at(d - Duration::from_secs(1)))
                    .death
                    .is_none(),
                "{tier:?} must survive just inside its deadline"
            );
            assert_eq!(
                carrier_tick(&at(d + Duration::from_secs(1))).death,
                Some(DeathReason::RxStale),
                "{tier:?} must die just past it"
            );
            // And the tiers really are on different deadlines: a direct
            // carrier is already dead where the relay one is still alive.
            if is_direct {
                assert!(
                    carrier_tick(&at(RELAY_RX_STALE_DEADLINE - Duration::from_secs(1)))
                        .death
                        .is_some(),
                    "{tier:?} must NOT be waiting out the relay deadline"
                );
            }
        }
        // Heard this sweep: survives regardless of anything else.
        let i = HealthInputs {
            since_last_rx: Duration::ZERO,
            ..established()
        };
        assert!(carrier_tick(&i).death.is_none());
        // Pre-handshake, rx-staleness NEVER fires (Installing phase).
        let i = HealthInputs {
            handshake_done: false,
            since_install: Duration::from_secs(9),
            since_last_rx: Duration::from_secs(120),
            ..established()
        };
        assert!(carrier_tick(&i).death.is_none());
    }

    #[test]
    fn one_way_counts_resets_and_clears_strikes() {
        // tx advanced, rx flat → strike 1, 2, then death on the 3rd.
        let mut i = HealthInputs {
            traffic: (11, 10),
            last_traffic: (10, 10),
            ..established()
        };
        let v = carrier_tick(&i);
        assert_eq!((v.bad_sweeps, v.death), (1, None));
        i.bad_sweeps = 1;
        let v = carrier_tick(&i);
        assert_eq!((v.bad_sweeps, v.death), (2, None));
        i.bad_sweeps = 2;
        let v = carrier_tick(&i);
        assert_eq!(v.bad_sweeps, 3);
        assert_eq!(v.death, Some(DeathReason::OneWay));

        // rx advancing resets the counter AND clears the tier strikes for a
        // direct carrier…
        let i = HealthInputs {
            traffic: (12, 11),
            last_traffic: (11, 10),
            bad_sweeps: 2,
            ..established()
        };
        let v = carrier_tick(&i);
        assert_eq!(v.bad_sweeps, 0);
        assert!(v.clear_tier_strikes);
        // …but NOT for a relay (strike maps are direct-tier bookkeeping)…
        let i = HealthInputs {
            tier: DirectTier::Relay,
            is_direct: false,
            traffic: (12, 11),
            last_traffic: (11, 10),
            ..established()
        };
        assert!(!carrier_tick(&i).clear_tier_strikes);
        // …and idle (nothing sent) resets the counter without claiming the
        // carrier is proven-receiving.
        let i = HealthInputs {
            bad_sweeps: 2,
            ..established()
        };
        let v = carrier_tick(&i);
        assert_eq!(v.bad_sweeps, 0);
        assert!(!v.clear_tier_strikes);
    }

    /// F3 (adversarial-review finding) — pre-handshake `tx` DOES tick once per
    /// rekey attempt (boringtun emits the INIT via `encapsulate`). An isolated
    /// blip books one strike and resets on the next (flat) sweep, so the
    /// one-way detector still cannot fire pre-handshake — the deadline is the
    /// only Installing-phase death.
    #[test]
    fn single_sweep_tx_blip_does_not_accumulate_bad_sweeps() {
        let base = HealthInputs {
            handshake_done: false,
            since_install: Duration::from_secs(9), // past grace, pre-deadline
            ..established()
        };
        // Sweep 1: the init tx-blip → strike 1.
        let v = carrier_tick(&HealthInputs {
            traffic: (1, 0),
            last_traffic: (0, 0),
            ..base
        });
        assert_eq!((v.bad_sweeps, v.death), (1, None));
        // Sweeps 2..: tx flat until the next rekey attempt (~90 s away — the
        // carrier hits its handshake deadline long before) → counter resets.
        let v = carrier_tick(&HealthInputs {
            traffic: (1, 0),
            last_traffic: (1, 0),
            bad_sweeps: 1,
            ..base
        });
        assert_eq!((v.bad_sweeps, v.death), (0, None));
    }

    #[test]
    fn death_reason_precedence() {
        // hard_dead + rx_stale co-trip (relay: send error AND silent) →
        // HardDead wins (the pre-P2 relay log checked hard_dead first).
        let i = HealthInputs {
            tier: DirectTier::Relay,
            is_direct: false,
            hard_dead: true,
            since_last_rx: Duration::from_secs(120),
            ..established()
        };
        assert_eq!(carrier_tick(&i).death, Some(DeathReason::HardDead));
        // rx_stale + one-way co-trip → RxStale wins (distinct "went silent"
        // log, as pre-P2).
        let i = HealthInputs {
            since_last_rx: Duration::from_secs(120),
            traffic: (13, 10),
            last_traffic: (12, 10),
            bad_sweeps: 2,
            ..established()
        };
        assert_eq!(carrier_tick(&i).death, Some(DeathReason::RxStale));
        // rx_stale × handshake-deadline is impossible by phase construction:
        // Installing has no RxStale arm, Established no HandshakeDeadline arm.
        assert!(matches!(
            CarrierPhase::of(false, DirectTier::Lan),
            CarrierPhase::Installing { .. }
        ));
        assert!(matches!(
            CarrierPhase::of(true, DirectTier::Lan),
            CarrierPhase::Established
        ));
    }

    #[test]
    fn relay_refresh_holdoff_defers_death_but_counter_advances() {
        // A relay one-way past the threshold, inside the refresh holdoff:
        // NO death, but the counter still stores (pre-P2 the `continue` sat
        // after the counter update).
        let i = HealthInputs {
            tier: DirectTier::Relay,
            is_direct: false,
            traffic: (13, 10),
            last_traffic: (12, 10),
            bad_sweeps: 2,
            relay_refresh_held: true,
            ..established()
        };
        let v = carrier_tick(&i);
        assert!(v.death.is_none());
        assert_eq!(v.bad_sweeps, 3, "holdoff must not lose the strike");
        // The holdoff is relay-only: a DIRECT carrier with the flag set (the
        // sweep never sets it for direct, but the rule is the tick's) dies.
        let i = HealthInputs {
            relay_refresh_held: true,
            traffic: (13, 10),
            last_traffic: (12, 10),
            bad_sweeps: 2,
            ..established()
        };
        assert_eq!(carrier_tick(&i).death, Some(DeathReason::OneWay));
    }

    #[test]
    fn probe_tick_promote_expire_wait_boundaries() {
        for (tier, deadline_s) in [
            (DirectTier::Lan, 12u64),
            (DirectTier::Srflx, 12),
            (DirectTier::Public, 30),
        ] {
            assert_eq!(
                probe_tick(true, Duration::ZERO, tier),
                ProbeVerdict::Promote,
                "latch promotes immediately"
            );
            assert_eq!(
                probe_tick(false, Duration::from_secs(deadline_s - 1), tier),
                ProbeVerdict::Wait
            );
            assert_eq!(
                probe_tick(false, Duration::from_secs(deadline_s + 1), tier),
                ProbeVerdict::Expire
            );
            // A latch exactly at/past the deadline still promotes — the latch
            // check runs first (pre-P2 branch order).
            assert_eq!(
                probe_tick(true, Duration::from_secs(deadline_s + 5), tier),
                ProbeVerdict::Promote
            );
        }
    }

    #[test]
    fn deadline_ordering_invariants() {
        // The cross-tier deadline relationships the pre-P2 test suite pinned:
        // srflx/LAN tight, public looser, relay loosest; all past the grace.
        assert!(SRFLX_HANDSHAKE_DEADLINE < PUBLIC_HANDSHAKE_DEADLINE);
        assert!(LAN_HANDSHAKE_DEADLINE < PUBLIC_HANDSHAKE_DEADLINE);
        assert!(PUBLIC_HANDSHAKE_DEADLINE < RELAY_HANDSHAKE_DEADLINE);
        assert!(LAN_HANDSHAKE_DEADLINE > DIRECT_GRACE);
        assert!(RELAY_HANDSHAKE_DEADLINE > DIRECT_GRACE);

        // Per-tier rx-staleness: direct is tighter (its path can be revoked
        // instantly by a VPN), relay keeps the forgiving bound because its
        // liveness depends on a third party that may reconnect.
        assert!(DIRECT_RX_STALE_DEADLINE < RELAY_RX_STALE_DEADLINE);
        for t in [DirectTier::Lan, DirectTier::Public, DirectTier::Srflx] {
            assert_eq!(t.rx_stale_deadline(), DIRECT_RX_STALE_DEADLINE);
        }
        assert_eq!(
            DirectTier::Relay.rx_stale_deadline(),
            RELAY_RX_STALE_DEADLINE
        );
    }

    /// The floor under BOTH rx-stale deadlines: a healthy but idle carrier is
    /// legitimately silent for a full persistent-keepalive period
    /// (`wg::KEEPALIVE_SECS` = 25 s), and the two ends are unsynchronised. Any
    /// deadline at or below ~2 keepalives would reap working carriers — the
    /// exact trap that makes a naive "no rx in 10 s ⇒ dead" rule unusable.
    #[test]
    fn rx_stale_deadlines_clear_two_keepalive_periods() {
        const KEEPALIVE: Duration = Duration::from_secs(25); // wg::KEEPALIVE_SECS
        for d in [DIRECT_RX_STALE_DEADLINE, RELAY_RX_STALE_DEADLINE] {
            assert!(
                d > KEEPALIVE * 2,
                "{d:?} must survive two consecutive lost keepalives"
            );
        }
    }

    /// An idle-but-healthy DIRECT carrier must survive right up to the new,
    /// tighter deadline — the regression lock for shortening it.
    #[test]
    fn idle_direct_carrier_survives_until_its_deadline() {
        let quiet = |since_last_rx: Duration| HealthInputs {
            since_last_rx,
            ..established()
        };
        // One missed keepalive: alive.
        assert!(
            carrier_tick(&quiet(Duration::from_secs(30)))
                .death
                .is_none()
        );
        // Just inside the deadline: still alive.
        assert!(
            carrier_tick(&quiet(DIRECT_RX_STALE_DEADLINE - Duration::from_secs(1)))
                .death
                .is_none()
        );
        // Past it: dead, and for the rx-stale reason specifically.
        assert_eq!(
            carrier_tick(&quiet(DIRECT_RX_STALE_DEADLINE + Duration::from_secs(1))).death,
            Some(DeathReason::RxStale)
        );
    }
}
