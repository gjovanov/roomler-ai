// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-79 V3b / FR-74 — ONE belief about what the path carries.
//!
//! # Three symptoms, one defect
//!
//! Measured on the fleet across 2026-09-09/10, on three different hosts and
//! two different carriers:
//!
//! 1. **The gate cannot fire on a relay.** CORPLAP-3 ran a whole session with
//!    `goodput_bps=None` and `goodput_samples=(0, 5)` — five windows of blocked
//!    time, every one under `goodput::MIN_WINDOW_BLOCKED`, nothing accepted. So
//!    FR-79 V5's rule ("a stall while sending above the measured pipe is our
//!    fault") had no measurement to compare against and was inert on that host.
//! 2. **The ceiling sawtooths on a direct path.** Regal-Elena-PZ, 1920×1080
//!    HEVC over a direct carrier at 6–11 ms paint age: the ceiling is a
//!    bits-per-pixel CONSTANT (0.625 bpp = 38,880,000), the blocked-send
//!    goodput measured **6,028,814**, and the target oscillated between them
//!    with a two-to-three minute period for the life of the session — 4.5
//!    minutes to first reach the ceiling, then collapse to ~5.3 M, climb,
//!    collapse to ~6.1 M. Every collapse is a visible softening.
//! 3. **The rate memory has almost nothing to remember.** CORPLAP-2 accepted
//!    exactly ONE sample per session in two very different link conditions.
//!
//! Those are one defect: **there is no trustworthy, always-available estimate
//! of what the path carries.** Every loop that needs one either invents a
//! constant (FR-74's bpp ceiling), goes silent (FR-79's gate), or remembers a
//! number nobody measured (the pair memory).
//!
//! # What this type fixes, and it is a distinction the old code did not make
//!
//! Two different things were being averaged together by `pipe_bps`'s
//! `min(goodput, link_rx)`:
//!
//! - **A demonstrated floor** — *"this path carried at least X."* Available
//!   from EVERY window on EVERY carrier, because the viewer reports its
//!   arrival rate whether or not anything is congested. It needs no
//!   congestion, and it is a hard lower bound: those bytes actually arrived.
//! - **A capacity estimate** — *"this path carries about X."* Only available
//!   when the path pushed back: a blocked send, or the viewer's arrival rate
//!   while its queue is growing. It is an estimate of the LIMIT.
//!
//! Taking `min` of the two is wrong in the direction that hurts: an arrival
//! rate measured while the sender was idle is not a capacity, and a
//! blocked-send sample taken during one burst is not a reason to disbelieve a
//! delivery that already happened. Symptom 2 is exactly that error — the
//! collapse target was a 6 Mbps capacity sample on a path that had just
//! delivered far more.
//!
//! So: a floor is never allowed to lower the anchor, and the anchor is never
//! `None` once anything has been delivered.
//!
//! ⚠️ The floor is a MAX over a recent window, not an average. An average of
//! delivered rates measures the CONTENT (an idle desktop sends ~300 kbps
//! through a gigabit path); only the peak says anything about the path.
//!
//! # ⚠️⚠️ The two questions are NOT interchangeable, and the API says so
//!
//! There is deliberately no single `believed_bps()`. The first shadow session
//! showed why, within minutes of shipping — CORPLAP-1 on `relay:derp/tcp`,
//! `0.4.98`, an idle desktop:
//!
//! ```text
//! goodput_bps=None  goodput_samples=(0, 0)
//! pipe_belief=(Some(1802656), Some(1802656), None)   pipe_belief_n=(35, 0)
//! target_bps=3000000   viewer_age_ms=Some(59)
//! ```
//!
//! 35 deliveries, ZERO capacity samples, nothing congested, paint age 59 ms —
//! a perfectly healthy session. A rule that read `1,802,656` as "the pipe"
//! would have computed `3.0 M > 1.2 × 1.8 M` and concluded the session was
//! overdriving. It was not: 1.8 Mbps is what an idle desktop WEIGHS, and the
//! path was never asked for more.
//!
//! So each consumer must name its question:
//!
//! | question | accessor | `None` means |
//! |---|---|---|
//! | *"am I sending more than this path will take?"* (FR-79 V5) | [`Pipe::capacity_bps`] | no push-back ⇒ **no claim to contradict** ⇒ do not cut |
//! | *"what may the ceiling be?"* (FR-74 P5) | [`Pipe::ceiling_anchor_bps`] | nothing measured ⇒ fall back to the bpp bound |
//! | *"what has this path already proven?"* | [`Pipe::floor_bps`] | — |
//!
//! A floor is a LOWER BOUND. It says the path carried at least X. It never
//! says the path would refuse X + 1, and no amount of idle delivery makes it
//! say that.

use std::time::{Duration, Instant};

/// How long a demonstrated delivery stands as the floor. Long enough that a
/// quiet stretch does not erase what a burst proved, short enough that a path
/// which genuinely degraded is not pinned to its old peak. A carrier change
/// re-keys the session's memory anyway (FR-79 V2), so this only has to survive
/// content, not topology.
pub const FLOOR_WINDOW: Duration = Duration::from_secs(30);

/// A capacity estimate expires on the same clock the goodput estimator uses,
/// so "the path pushed back a minute ago" stops being an argument.
pub const CAPACITY_TTL: Duration = super::goodput::CONFIDENCE_TTL;

/// What this session believes about the path, and how it came to believe it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Pipe {
    /// The largest rate the path has been DEMONSTRATED to carry, and when.
    floor: Option<(u32, Instant)>,
    /// What the path pushed back at, and when.
    capacity: Option<(u32, Instant)>,
    /// Counters for the heartbeat: deliveries folded, capacities folded.
    delivered_n: u32,
    capacity_n: u32,
}

impl Pipe {
    /// Every window, unconditionally: bytes that ARRIVED. Needs no congestion
    /// and no blocked send, which is the whole point — it is the one
    /// observation a relay-TCP session reliably produces.
    ///
    /// Keeps the max over [`FLOOR_WINDOW`]: a higher delivery always wins, and
    /// a lower one only wins once the standing peak has aged out.
    pub fn observe_delivered(&mut self, bps: u32, now: Instant) {
        if bps == 0 {
            return;
        }
        self.delivered_n = self.delivered_n.saturating_add(1);
        match self.floor {
            Some((best, at)) if bps < best && now.duration_since(at) <= FLOOR_WINDOW => {}
            _ => self.floor = Some((bps, now)),
        }
    }

    /// Only when the path PUSHED BACK: a blocked-send goodput sample, or the
    /// viewer's arrival rate while its transit queue was growing.
    pub fn observe_capacity(&mut self, bps: u32, now: Instant) {
        if bps == 0 {
            return;
        }
        self.capacity_n = self.capacity_n.saturating_add(1);
        self.capacity = Some((bps, now));
    }

    /// The demonstrated lower bound: the path carried at least this much, and
    /// recently enough to still mean something.
    pub fn floor_bps(&self, now: Instant) -> Option<u32> {
        self.floor
            .filter(|(_, at)| now.duration_since(*at) <= FLOOR_WINDOW)
            .map(|(bps, _)| bps)
    }

    /// The pushed-back estimate, while it is still fresh.
    pub fn capacity_bps(&self, now: Instant) -> Option<u32> {
        self.capacity
            .filter(|(_, at)| now.duration_since(*at) <= CAPACITY_TTL)
            .map(|(bps, _)| bps)
    }

    /// The anchor a CEILING may be built on: the most this path has been shown
    /// to be worth, from either kind of evidence.
    ///
    /// ⚠️ `max`, never `min`. A capacity estimate below something the path has
    /// just been shown to deliver is the estimate being wrong, not the
    /// delivery: the bytes arrived. Reading it the other way is symptom 2.
    ///
    /// ⚠️⚠️ **This is NOT the number to compare a target against when asking
    /// "am I sending more than this path can take?"** — use
    /// [`Self::capacity_bps`] for that, and accept `None` as "no claim to
    /// contradict". A floor is a LOWER BOUND: it says the path carried at
    /// least X, never that it would refuse X + 1.
    ///
    /// Measured the day this type shipped, CORPLAP-1 on `relay:derp/tcp`,
    /// `0.4.98`: an idle desktop gave `floor = 1,802,656` from 35 deliveries
    /// and ZERO capacity samples, while the target sat at 3,000,000. Nothing
    /// had pushed back and nothing was wrong — but a rule reading this as
    /// "the pipe" would have found 3.0 M > 1.2 × 1.8 M and cut a session that
    /// was never overdriving anything. The floor was the CONTENT's weight, not
    /// the path's limit.
    pub fn ceiling_anchor_bps(&self, now: Instant) -> Option<u32> {
        match (self.capacity_bps(now), self.floor_bps(now)) {
            (Some(c), Some(f)) => Some(c.max(f)),
            (c, f) => c.or(f),
        }
    }

    /// `(deliveries folded, capacities folded)` for the heartbeat — the pair
    /// that says WHY the belief is what it is, and the counter that authorises
    /// the deletions in FR-79 V3b's own acceptance.
    pub fn counts(&self) -> (u32, u32) {
        (self.delivered_n, self.capacity_n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// Symptom 1, CORPLAP-3: a session that never blocks a send still knows
    /// something about its path. Before this type it knew nothing at all, and
    /// FR-79 V5's rule was inert there.
    #[test]
    fn a_session_that_never_pushed_back_still_has_a_belief() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered(900_000, now);
        p.observe_delivered(1_200_000, now + Duration::from_secs(1));
        assert_eq!(p.capacity_bps(now + Duration::from_secs(1)), None);
        assert_eq!(
            p.ceiling_anchor_bps(now + Duration::from_secs(1)),
            Some(1_200_000),
            "the belief falls back to what was demonstrably delivered"
        );
    }

    /// Symptom 2, Regal-Elena-PZ: a capacity sample of 6.03 Mbps arrives on a
    /// path that has just delivered 20 Mbps. The old `min` rule read 6 M and
    /// the target collapsed to it; the delivery is the harder fact.
    #[test]
    fn a_capacity_sample_never_lowers_the_belief_below_a_real_delivery() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered(20_000_000, now);
        p.observe_capacity(6_028_814, now + Duration::from_secs(1));
        assert_eq!(
            p.ceiling_anchor_bps(now + Duration::from_secs(1)),
            Some(20_000_000),
            "the bytes arrived; the estimate is what is wrong"
        );
    }

    /// ...but on a path that really is slow, the capacity estimate stands: the
    /// floor is whatever that path actually delivered, which is also small.
    #[test]
    fn a_genuinely_slow_path_believes_its_measurement() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered(1_050_000, now);
        p.observe_capacity(1_058_145, now + Duration::from_secs(1));
        assert_eq!(
            p.ceiling_anchor_bps(now + Duration::from_secs(1)),
            Some(1_058_145)
        );
    }

    /// The floor is a MAX, not an average — an idle desktop delivering
    /// 300 kbps through a fast path must not talk the belief down.
    #[test]
    fn an_idle_stretch_does_not_talk_the_floor_down() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered(18_000_000, now);
        for i in 1..8 {
            p.observe_delivered(300_000, now + Duration::from_secs(i));
        }
        assert_eq!(p.floor_bps(now + Duration::from_secs(8)), Some(18_000_000));
    }

    /// ...but it does not stand forever: past `FLOOR_WINDOW` a path that has
    /// degraded is believed at its new rate rather than its old peak.
    #[test]
    fn a_stale_peak_gives_way_to_what_the_path_carries_now() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered(18_000_000, now);
        let later = now + FLOOR_WINDOW + Duration::from_secs(1);
        assert_eq!(p.floor_bps(later), None, "the peak aged out");
        p.observe_delivered(2_000_000, later);
        assert_eq!(p.floor_bps(later), Some(2_000_000));
    }

    /// A capacity estimate expires on the goodput estimator's own clock, so a
    /// push-back from a minute ago stops steering the session.
    #[test]
    fn a_capacity_estimate_expires() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_capacity(1_000_000, now);
        assert_eq!(p.capacity_bps(now + CAPACITY_TTL), Some(1_000_000));
        assert_eq!(
            p.capacity_bps(now + CAPACITY_TTL + Duration::from_secs(1)),
            None
        );
    }

    /// The first shadow session, verbatim: CORPLAP-1 on `relay:derp/tcp`,
    /// `0.4.98`, an idle desktop. 35 deliveries, ZERO capacity samples, target
    /// 3,000,000, paint age 59 ms — healthy.
    ///
    /// The anchor has a number (a ceiling may be built on it), but the
    /// over-drive question must still answer `None`: nothing pushed back, so
    /// there is no claim to contradict. Reading the anchor instead would have
    /// cut a session that was never overdriving — the floor is what the
    /// CONTENT weighed.
    #[test]
    fn a_healthy_idle_session_offers_a_ceiling_anchor_but_no_overdrive_verdict() {
        let now = t0();
        let mut p = Pipe::default();
        for i in 0..35 {
            p.observe_delivered(1_802_656, now + Duration::from_millis(i * 500));
        }
        let t = now + Duration::from_millis(35 * 500);
        assert_eq!(p.counts(), (35, 0), "35 deliveries, no push-back");
        assert_eq!(
            p.ceiling_anchor_bps(t),
            Some(1_802_656),
            "a ceiling may be anchored on what was demonstrably delivered"
        );
        assert_eq!(
            p.capacity_bps(t),
            None,
            "but NOTHING pushed back, so there is no capacity claim — and a \
             target of 3,000,000 here is not evidence of overdriving"
        );
    }

    /// Nothing observed at all is still `None` — the honest answer before the
    /// first frame, and the one FR-79 V5 reads as "no claim to contradict".
    #[test]
    fn an_untouched_pipe_believes_nothing() {
        assert_eq!(Pipe::default().ceiling_anchor_bps(t0()), None);
        assert_eq!(Pipe::default().counts(), (0, 0));
    }

    /// Zero is not an observation — a window that delivered nothing says
    /// nothing about the path, and must not become its floor.
    #[test]
    fn a_zero_window_is_not_evidence() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered(0, now);
        p.observe_capacity(0, now);
        assert_eq!(p.ceiling_anchor_bps(now), None);
        assert_eq!(p.counts(), (0, 0));
    }
}
