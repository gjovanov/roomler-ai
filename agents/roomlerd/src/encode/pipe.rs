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
//! 2. **The ceiling sawtooths on a direct path.** WINHOST-K, 1920×1080
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
//! ⚠️ The floor is a MAX, not an average. An average of delivered rates
//! measures the CONTENT (an idle desktop sends ~300 kbps through a gigabit
//! path); only the peak says anything about the path.
//!
//! # ⚠️⚠️ What a quiet screen proves: nothing. The overnight shadow, 83 samples
//!
//! The first version expired the floor after 30 s, so that "a path which
//! genuinely degraded is not pinned to its old peak". 300 sweeps across the
//! fleet showed that reasoning is wrong, because it cannot tell a degraded path
//! from a still screen. Direct sessions spanned **11 kbps to 24.7 Mbps**, median
//! 270 kbps — and the extremes are the SAME HOSTS:
//!
//! ```text
//! CORPLAP-3  19:46-20:02  s=6aa307be  belief = 4.8 - 8.4 Mbps    (content moving)
//! CORPLAP-3  13:25-13:54  s=6aa3d669  belief = 12.8 kbps         (static screen)
//!                                     tgt=34,560,000  n=(10696, 0)
//! CORPLAP-2  19:39        s=6aa30771  belief = 24.7 Mbps
//! CORPLAP-1  07:40-07:44  s=6aa3aff5  belief = 10.3 - 10.8 Mbps
//! ```
//!
//! A host that demonstrated 8.4 Mbps read **13 kbps** hours later with nobody
//! typing, and the floor kept expiring to `None` entirely between reports. Had
//! FR-74 P5 anchored a ceiling on that, a still screen would have collapsed the
//! ceiling to the legibility floor and STARVED the next burst — making the
//! symptom this whole arc exists to fix strictly worse.
//!
//! So the rule is evidential, not temporal: **a delivery raises the floor and
//! nothing but contrary evidence lowers it.** A capacity sample below the floor
//! is that contrary evidence — the path got smaller, and the old demonstration
//! stops being true. Time is not evidence. Quiet is not evidence.
//!
//! ⚠️ The cost, accepted knowingly: a path that silently degrades keeps an
//! optimistic floor until something pushes back. That is the right trade,
//! because the floor only ever feeds a CEILING — a bound saying "you may try" —
//! and the capacity path cuts the moment trying fails. An optimistic bound
//! costs one burst; a pessimistic one costs every burst.
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
//!
//! # ⚠️ One consumer since FR-79 V5b: the floor's rise, and only as a veto
//!
//! The floor still never answers *"am I over?"*. What the governor reads is
//! whether a valid window's delivery RAISED it, and it uses that to take out a
//! reading the delivery contradicts ([`Pipe::observe_delivered`]). The order is
//! the rule. Field 2026-10-30 on one laptop, a floor comparison alone gets these
//! backwards:
//!
//! | reading | ÷ the floor when taken | what came after | it was |
//! |---|---|---|---|
//! | 709,460 at 12:04:06 | 0.39 | the floor rose to 4,657,230 two seconds later | false |
//! | 180,484 at 12:19:52 | 0.23 | the floor did not move for the reading's 60 s, 26 more stalls | true |
//!
//! Everything else on this type is still SHADOW: the heartbeat's `pipe_belief`.

use std::time::{Duration, Instant};

/// ⚠️⚠️ There is deliberately NO time window on the floor. The first version of
/// this type expired it after 30 s, and the overnight shadow showed why that is
/// wrong — see the module header's "what a quiet screen proves" section. A
/// demonstrated delivery is a FACT about the path; a quiet screen is not
/// evidence against it. Only contrary evidence retires it.
///
/// Kept as the doc anchor for that decision; nothing reads it.
pub const FLOOR_NEVER_EXPIRES: () = ();

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
    /// FR-148 P0 — the class of the input that last wrote `floor`, and of the
    /// one that wrote `capacity`. Written beside the values; no law reads them,
    /// only the tagged readers ([`Self::floor_number`] and its two siblings).
    floor_basis: Option<Basis>,
    capacity_basis: Option<Basis>,
    /// FR-148 P0 — FR-148 §4.1's laws over the same inputs, in shadow.
    model: PathModel,
    /// FR-148 P1 — the pair memory's side of this session: the same laws from
    /// the entry's floor, and the push-back record (`pair_memory`).
    memory: super::pair_memory::SessionMemory,
    /// FR-148 D46 B2 — windows left that are no reading for the shadow model
    /// or the pair memory: the in-flight budget saw the clock stand still for
    /// the least and S, or read a release ([`Self::note_stall`]). Whether the
    /// last window was one of them, for its push-backs.
    stall_windows: u8,
    window_gated: bool,
    /// The tcp_shape harness's count of such windows; `None` is
    /// [`STALL_WINDOWS`].
    stall_gate: Option<u8>,
    /// FR-148 D48 — the in-flight budget's last step as the governor noted it
    /// ([`Self::note_path_rate`]), and what it read at the last two window
    /// closes, the older first. `None` while the budget is off: then no
    /// window rule reads anything.
    path_rate: Option<PathRate>,
    closes: [Option<PathRate>; 2],
    /// FR-148 D48 — how the last window's readings reach the shadow model and
    /// the pair memory, ruled at its close ([`Self::observe_delivered_in`]).
    window_rule: WindowRule,
    /// FR-148 D48 — the rule that last kept a reading out of the shadow model
    /// and the pair memory, re-read it or bounded it, and when; the `path
    /// model` line names it ([`Self::ruled`]).
    ruled: Option<(Ruled, Instant)>,
    /// The tcp_shape harness's fold of the law before FR-148 D50: no clock
    /// reaches the shadow model ([`Self::set_unclocked`]).
    #[cfg(test)]
    unclocked: bool,
}

/// FR-148 D48 — what the in-flight budget's last step read, as the governor
/// notes it for the shadow model's window rules: the budget's rate, whether
/// this association's figures gave it, and SCTP's loss count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PathRate {
    /// The budget's rate, bit/s: the most of its one-second figures over two
    /// seconds; `None` before one is known.
    pub bps: Option<u32>,
    /// This association's figures gave the rate; else the opening rate stood
    /// in for it.
    pub measured: bool,
    /// SCTP's fast retransmits and T3-rtx expiries over the budget's
    /// associations together: a count that never runs back.
    pub loss: u64,
}

/// FR-148 D48 — the rule that kept a reading out of the shadow model and the
/// pair memory, re-read it or bounded it. The `path model` line names the last
/// one, and how long ago it acted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ruled {
    /// B2's stall gate: a window after a stall the in-flight budget saw
    /// ([`Pipe::note_stall`]).
    Stall,
    /// Rule 1: a P3 sample with a loss in it, re-read at the budget's rate at
    /// its close.
    ReRead,
    /// Rule 4: a P3 sample with no loss in it and a hold SCTP's congestion
    /// window sized, kept out: the association's ramp bound it.
    Ramp,
    /// Rule 2 (`bm0`): a window near a loss that read over the budget's
    /// measured rate, taken at that rate; over a rate the opening stood in
    /// for, taken at nothing.
    Bound,
    /// Rule 3 (`u90`): a window near a loss that read under 0.9 times the
    /// measured rate, its link push-back kept out.
    Hole,
}

impl Ruled {
    /// The word the `path model` line writes.
    pub fn word(self) -> &'static str {
        match self {
            Ruled::Stall => "stall",
            Ruled::ReRead => "reread",
            Ruled::Ramp => "ramp",
            Ruled::Bound => "bound",
            Ruled::Hole => "hole",
        }
    }
}

/// FR-148 D48 — how a window's readings reach the shadow model and the pair
/// memory, near a loss (rules 2 and 3). `Pipe`'s own law takes every window
/// as it is, and B2's stall gate acts before any of these.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum WindowRule {
    /// As read.
    #[default]
    Take,
    /// The delivery and the link push-back at no more than this measured
    /// rate (`bm0`).
    Bound(u32),
    /// None of the window's readings: it read over a rate the opening stood
    /// in for (`bm0`).
    Out,
    /// No link push-back; the delivery and the goodput push-back as read
    /// (`u90`).
    Hole,
}

/// FR-148 D48, rule 3 — a window near a loss that reads under this many
/// tenths of the budget's measured rate read a hole the viewer waited on.
const HOLE_TENTHS: u64 = 9;

/// FR-148 D46 B2 — the windows a stall the in-flight budget saw keeps out of
/// the shadow model and the pair memory, counted from the last step that saw
/// it. The steps inside the stall mark the window that ends in it, and the
/// step that reads the release marks the window its burst comes back in:
/// D24's rule ("that window and the one after it") from the sender's side,
/// where the budget keeps the transit short and so the validity gate sees no
/// stall. A second window kept 15 of a 20 s run's windows out on a `tcp_shape`
/// stall shape (a stall every 3.3 s), and the floor could not come back from
/// a sample that read under the path.
pub const STALL_WINDOWS: u8 = 1;

impl Pipe {
    /// Every window, unconditionally: bytes that ARRIVED. Needs no congestion
    /// and no blocked send, which is the whole point — it is the one
    /// observation a relay-TCP session reliably produces.
    ///
    /// Keeps the MAX for the life of the session: a higher delivery always
    /// wins, and only [`Self::observe_capacity`] with a smaller number takes it
    /// back down.
    ///
    /// Returns whether this delivery raised the floor, which is the fact FR-79
    /// V5b reads. A floor that rose is a delivery LATER than anything that set
    /// it, so it can contradict a reading taken earlier; a floor that stood
    /// cannot.
    ///
    /// FR-148 P0 — `basis` is how the delivery was measured. It is kept beside
    /// the floor it sets and fed to the shadow model; nothing it says changes
    /// what this returns or any number this type gives.
    pub fn observe_delivered(&mut self, bps: u32, basis: Basis, now: Instant) -> bool {
        self.observe_delivered_in(bps, basis, true, now)
    }

    /// FR-148 P1 (D24) — [`Self::observe_delivered`] for a window the validity
    /// gate ruled on. This type's own law takes every window, as it always has;
    /// the shadow model and the pair memory take a delivery only from a `valid`
    /// one, because the opener's burst and the drain after a stall deliver more
    /// in one window than the path carries.
    ///
    /// FR-148 D46 B2 — nor from one of the [`STALL_WINDOWS`] after a stall the
    /// in-flight budget saw ([`Self::note_stall`]).
    ///
    /// FR-148 D48 — the call is the window's close: near a loss the shadow
    /// model and the pair memory take the delivery at no more than the
    /// budget's measured rate there, and nothing of it over a rate the
    /// opening stood in for (rule 2, `bm0`; [`Self::close_window`]). The
    /// stall gate acts first.
    pub fn observe_delivered_in(
        &mut self,
        bps: u32,
        basis: Basis,
        valid: bool,
        now: Instant,
    ) -> bool {
        // A window that delivered nothing is gated while a stall's count
        // stands, and uses none of it.
        self.window_gated = self.stall_windows > 0;
        self.window_rule = self.close_window(bps);
        if bps == 0 {
            return false;
        }
        self.stall_windows = self.stall_windows.saturating_sub(1);
        if valid {
            let taken = if self.window_gated {
                self.note_ruled(Ruled::Stall, now);
                None
            } else {
                match self.window_rule {
                    WindowRule::Take | WindowRule::Hole => Some(bps),
                    WindowRule::Bound(rate) => {
                        self.note_ruled(Ruled::Bound, now);
                        Some(rate)
                    }
                    WindowRule::Out => {
                        self.note_ruled(Ruled::Bound, now);
                        None
                    }
                }
            };
            if let Some(taken) = taken {
                self.model.delivered(taken, basis, now);
                self.memory.delivered(taken, basis, now);
            }
        }
        self.delivered_n = self.delivered_n.saturating_add(1);
        match self.floor {
            Some((best, _)) if bps <= best => false,
            _ => {
                self.floor = Some((bps, now));
                self.floor_basis = Some(basis);
                true
            }
        }
    }

    /// Only when the path PUSHED BACK: a blocked-send goodput sample, or the
    /// viewer's arrival rate while its transit queue was growing.
    ///
    /// ⚠️ This is also the ONLY thing that moves a floor DOWN, and it moves it
    /// by [`rate_memory::damp`] rather than snapping to it — the same
    /// down-fast/up-slow asymmetry FR-79 V4 already uses for the pair memory,
    /// not a second law.
    ///
    /// Snapping was wrong in both directions and the tests caught it:
    /// a single blocked-send sample of 6.03 Mbps must NOT erase a 20 Mbps
    /// delivery (that is the Regal sawtooth's collapse, rebuilt inside the
    /// estimator), but a path that has genuinely shrunk must not keep an
    /// optimistic floor forever either (that is the 38.88 Mbps constant, with
    /// extra steps). Damping says: one sample barely moves it, sustained
    /// evidence converges on it.
    ///
    /// FR-148 P0 — `basis` as for [`Self::observe_delivered`]: kept beside what
    /// it sets, fed to the shadow model, read by no law here. An
    /// application-limited push-back moves this type exactly as a backlogged
    /// one does; only the model tells them apart.
    pub fn observe_capacity(&mut self, bps: u32, basis: Basis, now: Instant) {
        // No window behind it: filed as the sender's own reading.
        let take = super::pair_memory::Take::Sample(super::pair_memory::Read::Overproduced);
        self.observe_capacity_in(bps, basis, take, now);
    }

    /// FR-148 P1 (R1, R3) — [`Self::observe_capacity`] with the window's
    /// [`Take`](super::pair_memory::Take): read or not, and the sender pushing
    /// against its plan or not. This type's own law and the shadow model take
    /// it either way, exactly as before; the pair memory takes it as a sample
    /// only from a read window in which the sender pushed, and otherwise only
    /// counts it.
    ///
    /// FR-148 D48 — of the window rules, only rule 2's over a stand-in rate
    /// reaches this push-back (the goodput fold's): the window is no reading
    /// at all then. The link loop's goes through
    /// [`Self::observe_link_capacity_in`].
    pub fn observe_capacity_in(
        &mut self,
        bps: u32,
        basis: Basis,
        take: super::pair_memory::Take,
        now: Instant,
    ) {
        let taken = (self.window_rule != WindowRule::Out).then_some(bps);
        self.capacity_in(bps, taken, basis, take, now);
    }

    /// FR-148 D48 — [`Self::observe_capacity_in`] for the link loop's
    /// push-back: the viewer's arrival rate while its queue grew, the window's
    /// own delivery. Near a loss the shadow model and the pair memory take it
    /// at no more than the budget's measured rate (rule 2, `bm0`), and none of
    /// it under 0.9 times that rate (rule 3, `u90`): behind a hole the viewer
    /// reads SCTP's order, not the path. This type's own law takes it as
    /// [`Self::observe_capacity_in`] does.
    pub fn observe_link_capacity_in(
        &mut self,
        bps: u32,
        basis: Basis,
        take: super::pair_memory::Take,
        now: Instant,
    ) {
        let taken = match self.window_rule {
            WindowRule::Take => Some(bps),
            WindowRule::Bound(rate) => Some(bps.min(rate)),
            WindowRule::Out => None,
            WindowRule::Hole => {
                if bps > 0 && !self.window_gated {
                    self.note_ruled(Ruled::Hole, now);
                }
                None
            }
        };
        self.capacity_in(bps, taken, basis, take, now);
    }

    /// A push-back at `bps`, which the shadow model and the pair memory take
    /// at `taken`, or not at all.
    fn capacity_in(
        &mut self,
        bps: u32,
        taken: Option<u32>,
        basis: Basis,
        take: super::pair_memory::Take,
        now: Instant,
    ) {
        if bps == 0 {
            return;
        }
        // Every caller is a valid window: the goodput fold and the link loop
        // both act only on one (`governor`). FR-148 D46 B2: a window after a
        // stall the budget saw is no reading for the model or the memory.
        // FR-148 D48: nor what the window rules keep out. FR-148 D50: the
        // model reads a fall against the clock; the memory is passed none.
        if !self.window_gated
            && let Some(taken) = taken
        {
            let clock = self.clock();
            self.model.pushed_back_clocked(taken, basis, clock, now);
            self.memory.pushed_back_in(taken, basis, take, now);
        }
        self.capacity_n = self.capacity_n.saturating_add(1);
        self.capacity = Some((bps, now));
        self.capacity_basis = Some(basis);
        // ⚠️ A capacity sample is ALSO a delivery. A blocked-send goodput of X
        // means X bits per second actually got through while the sender was
        // blocked — those bytes arrived. So it feeds the floor by the same law
        // as any other delivery: raise outright, lower damped.
        //
        // Missing this cost the anchor a real measurement. Field 2026-09-12,
        // CORPLAP-3 session `6aa53186`: capacity measured 13,163,844 at 11:04,
        // and four minutes later — one `CAPACITY_TTL` — the anchor had fallen
        // back to a content-driven floor of 4,050,945 while the target ran to
        // the 34,560,000 constant. A ceiling anchored there would cap a path
        // just measured at 13 Mbps to about 5.
        match self.floor {
            Some((floor, _)) if bps < floor => {
                self.floor = Some((super::rate_memory::damp(floor, bps), now));
                self.floor_basis = Some(basis);
            }
            Some((floor, _)) if bps <= floor => {}
            _ => {
                self.floor = Some((bps, now));
                self.floor_basis = Some(basis);
            }
        }
    }

    /// FR-148 D46 B2 — a delivery sample's push-back (D46 item 10): P1, a
    /// sample that read a standing queue, or P3, one the in-flight budget read
    /// as backlogged with no still span in it. The shadow model and the pair
    /// memory take it, under the last valid window's `take`; this type's own
    /// law never does, so nothing a session sends or opens moves. Neither
    /// ever raises a floor ([`PathModel::sample_pushed_back`]).
    ///
    /// FR-148 D50 — the shadow model reads it against the clock
    /// ([`Self::clock`]), as the second of a fall or not; the pair memory is
    /// passed none, and keeps the damped law (D30).
    pub fn observe_sample_capacity(
        &mut self,
        bps: u32,
        by: PushedBy,
        take: super::pair_memory::Take,
        now: Instant,
    ) {
        if bps == 0 {
            return;
        }
        let clock = self.clock();
        self.model.sample_pushed_back_clocked(bps, by, clock, now);
        self.memory.sample_pushed_back_in(bps, by, take, now);
    }

    /// FR-148 D46 B2 — the in-flight budget saw the clock stand still for the
    /// least and S with bytes out, or read a release: the next
    /// [`STALL_WINDOWS`] windows are no reading for the shadow model or the
    /// pair memory. This type's own law takes them as before.
    pub fn note_stall(&mut self) {
        self.stall_windows = self.stall_gate.unwrap_or(STALL_WINDOWS);
    }

    /// The tcp_shape harness's rule-D sweep: `n` windows after a stall.
    #[cfg(test)]
    pub(crate) fn set_stall_gate(&mut self, n: u8) {
        self.stall_gate = Some(n);
    }

    /// FR-148 D48, the tcp_shape harness — what the shadow model and the pair
    /// memory take of a reading of `bps` in the last window under the rule its
    /// close ruled, B2's stall gate aside: as its delivery, its goodput
    /// push-back and its link push-back, each at most this or none; and the
    /// rule's word, empty for none.
    #[cfg(test)]
    pub(crate) fn window_takes(&self, bps: u32) -> ([Option<u32>; 3], &'static str) {
        match self.window_rule {
            WindowRule::Take => ([Some(bps); 3], ""),
            WindowRule::Bound(rate) => {
                let bound = Some(bps.min(rate));
                ([bound, Some(bps), bound], "bound")
            }
            WindowRule::Out => ([None; 3], "out"),
            WindowRule::Hole => ([Some(bps), Some(bps), None], "hole"),
        }
    }

    /// FR-148 D48 — the in-flight budget's last step, computing on or in
    /// shadow: its rate, whether this association's figures gave it, and
    /// SCTP's loss count. The next window's close reads it
    /// ([`Self::close_window`]). With the budget off nothing notes it, and
    /// every window reaches the shadow model and the pair memory as before.
    pub fn note_path_rate(&mut self, rate: PathRate) {
        self.path_rate = Some(rate);
    }

    /// FR-148 D50 — the clock a fall is read against: the in-flight budget's
    /// rate as [`Self::note_path_rate`] last noted it, when this association's
    /// figures gave it. `None` with the budget off, while the opening rate
    /// stands in, and at a rate of 0, which the window rules read as none
    /// too ([`Self::close_window`]). Passed to the shadow model at each
    /// push-back and never kept there: the pair memory is passed none.
    fn clock(&self) -> Option<u32> {
        #[cfg(test)]
        if self.unclocked {
            return None;
        }
        self.path_rate
            .filter(|r| r.measured)
            .and_then(|r| r.bps)
            .filter(|&bps| bps > 0)
    }

    /// The tcp_shape harness's fold of the law before FR-148 D50 (`d48`):
    /// D48's window rules read the budget's rate as before, and no clock
    /// reaches the shadow model, so it reads no fall.
    #[cfg(test)]
    pub(crate) fn set_unclocked(&mut self) {
        self.unclocked = true;
    }

    /// FR-148 D48 — `rule` kept a reading out of the shadow model and the
    /// pair memory, re-read it or bounded it, at `now`.
    pub fn note_ruled(&mut self, rule: Ruled, now: Instant) {
        self.ruled = Some((rule, now));
    }

    /// FR-148 D48 — the rule that last kept a reading out, re-read it or
    /// bounded it, and how long ago; `None` while none has.
    pub fn ruled(&self, now: Instant) -> Option<(Ruled, Duration)> {
        self.ruled
            .map(|(rule, at)| (rule, now.saturating_duration_since(at)))
    }

    /// FR-148 D48 — a window's close, `bps` its delivery: how its readings
    /// reach the shadow model and the pair memory. Near a loss (SCTP's count
    /// moved since the close two windows back: over the window or the
    /// window's length before it, since a hole's release reaches the viewer a
    /// round trip or so after the retransmission the count counts), a window
    /// over the budget's rate is taken at that rate when this association's
    /// figures gave it, and not at all over a rate the opening stood in for
    /// (rule 2); one under 0.9 times a measured rate makes no link push-back
    /// (rule 3). Anywhere else, and with the budget off, as read.
    fn close_window(&mut self, bps: u32) -> WindowRule {
        let [back, last] = self.closes;
        self.closes = [last, self.path_rate];
        let Some(now) = self.path_rate else {
            return WindowRule::Take;
        };
        let lost = now.loss > back.map_or(0, |b| b.loss);
        match now.bps.filter(|&rate| rate > 0) {
            Some(rate) if lost && bps > rate => {
                if now.measured {
                    WindowRule::Bound(rate)
                } else {
                    WindowRule::Out
                }
            }
            Some(rate)
                if lost
                    && bps > 0
                    && now.measured
                    && u64::from(bps) * 10 < u64::from(rate) * HOLE_TENTHS =>
            {
                WindowRule::Hole
            }
            _ => WindowRule::Take,
        }
    }

    /// The demonstrated lower bound: the most this path has EVER been shown to
    /// carry this session, minus anything a later push-back contradicted.
    ///
    /// ⚠️ No staleness filter, deliberately. See [`FLOOR_NEVER_EXPIRES`].
    pub fn floor_bps(&self, _now: Instant) -> Option<u32> {
        self.floor.map(|(bps, _)| bps)
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

    /// FR-148 P0 — [`Self::floor_bps`], with the class of the input that set it
    /// and how long ago. The value is always `floor_bps`'s
    /// (`tests::tags_change_no_number`).
    pub fn floor_number(&self, now: Instant) -> Option<PathNumber> {
        let (bps, at) = self.floor?;
        Some(PathNumber::new(
            bps,
            self.floor_basis?,
            now.saturating_duration_since(at),
        ))
    }

    /// FR-148 P0 — [`Self::capacity_bps`], tagged as [`Self::floor_number`] is.
    pub fn capacity_number(&self, now: Instant) -> Option<PathNumber> {
        let (bps, at) = self
            .capacity
            .filter(|(_, at)| now.duration_since(*at) <= CAPACITY_TTL)?;
        Some(PathNumber::new(
            bps,
            self.capacity_basis?,
            now.saturating_duration_since(at),
        ))
    }

    /// FR-148 P0 — [`Self::ceiling_anchor_bps`]: the tagged number it takes, the
    /// capacity on a tie.
    pub fn anchor_number(&self, now: Instant) -> Option<PathNumber> {
        match (self.capacity_number(now), self.floor_number(now)) {
            (Some(c), Some(f)) => Some(if c.bps >= f.bps { c } else { f }),
            (c, f) => c.or(f),
        }
    }

    /// FR-148 P0 — the shadow model over this pipe's inputs.
    pub fn model(&self) -> &PathModel {
        &self.model
    }

    /// FR-148 P0 — the pair memory's seed at the open, for the shadow model
    /// alone ([`PathModel::remembered`]). This type's own numbers do not move:
    /// they never took a seed.
    pub fn remember(&mut self, bps: u32, now: Instant) {
        self.model.remembered(bps, now);
    }

    /// FR-148 P1 — the pair memory's side of this session.
    pub fn memory(&self) -> &super::pair_memory::SessionMemory {
        &self.memory
    }

    #[cfg(test)]
    pub(crate) fn memory_mut(&mut self) -> &mut super::pair_memory::SessionMemory {
        &mut self.memory
    }

    /// FR-148 P1 — open the pair memory's side from the pair's entry. This
    /// type's own numbers and the shadow model do not move.
    pub fn open_memory(
        &mut self,
        entry: Option<&super::pair_memory::PairEntry>,
        now_unix: u64,
        now: Instant,
    ) {
        self.memory.open(entry, now_unix, now);
    }
}

/// FR-148 §4.1 — how a path number was obtained. Three classes and no more: a
/// stalled window is evidence about stalls and is never one of them (FR-71's
/// classifier and FR-79's gate say which windows those are).
///
/// ⚠️ Not `rate_memory::Provenance`, which is what the pair memory stores today
/// (measured or assumed). P1 merges the two; until then they stay apart, and are
/// never glob-imported together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Measured while the sender had more to send than the path took: evidence
    /// about the path's capacity.
    Backlogged,
    /// Measured while the path took everything the sender offered: a lower
    /// bound, which says nothing about where the path would refuse.
    AppLimited,
    /// Nobody measured it: a prior, a stand-in, a default, a computed ramp.
    Assumed,
}

impl Basis {
    /// A measured window's class: backlogged when the sender offered more than
    /// arrived, application-limited when it did not.
    pub fn of_window(backlogged: bool) -> Self {
        if backlogged {
            Basis::Backlogged
        } else {
            Basis::AppLimited
        }
    }

    /// The word the `path model` line writes.
    pub fn word(self) -> &'static str {
        match self {
            Basis::Backlogged => "backlogged",
            Basis::AppLimited => "app-limited",
            Basis::Assumed => "assumed",
        }
    }
}

/// FR-148 P0 — a path number as a session reports it: the value, how it was
/// obtained, and how old the evidence behind it is when it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathNumber {
    pub bps: u32,
    pub basis: Basis,
    /// Measured in an earlier session: the pair memory's seed, and what stands at
    /// it. The entry does not say which measured class it was (P1 stores it), so
    /// `basis` is the weaker one, [`Basis::AppLimited`], and the line says
    /// `remembered`.
    pub remembered: bool,
    pub age: Duration,
}

impl PathNumber {
    pub fn new(bps: u32, basis: Basis, age: Duration) -> Self {
        Self {
            bps,
            basis,
            remembered: false,
            age,
        }
    }

    /// A number measured in an earlier session, `age` old.
    pub fn remembered(bps: u32, age: Duration) -> Self {
        Self {
            bps,
            basis: Basis::AppLimited,
            remembered: true,
            age,
        }
    }

    /// `backlogged`, `app-limited`, `assumed` or `remembered`.
    pub fn word(&self) -> &'static str {
        if self.remembered {
            "remembered"
        } else {
            self.basis.word()
        }
    }
}

/// `<bps>/<word>/<age>ms`, as the `path model` line writes each number.
impl std::fmt::Display for PathNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}ms", self.bps, self.word(), self.age.as_millis())
    }
}

/// FR-148 P0 — FR-148 §4.1's laws over the inputs [`Pipe`] takes, in shadow:
/// what the path model would hold if provenance were read. Nothing reads it but
/// the `path model` line (`media_delivery`) and the cells that replay two field
/// chains against it (`governor::tests`).
///
/// | input | [`Pipe`] | here |
/// |---|---|---|
/// | a delivery, of either measured class | raises the floor | raises the floor |
/// | a push-back, backlogged | sets the capacity; lowers the floor damped, or raises it | the same, until a fall (D50) |
/// | a push-back, application-limited | the same as a backlogged one | raises the floor, as a delivery does; the capacity does not move |
/// | the pair memory's seed, at the open | never an input | an application-limited reading, marked remembered ([`Pipe::remember`]) |
/// | anything assumed | never an input | never enters |
///
/// So the floor rises on any measured reading and falls only on a backlogged
/// one, and the capacity moves only on a backlogged one. An assumed value never
/// stands below the floor ([`Self::stand_in`]) and is never offered for storing
/// ([`Self::offer`]).
///
/// FR-148 D46 B2 — a delivery sample's push-back, P1 or P3
/// ([`Self::sample_pushed_back`]), sets the capacity and lowers the floor
/// damped, as a backlogged push-back does, and never raises it: its bytes are
/// SCTP's acknowledgements, which a stall's release brings back at once.
///
/// FR-148 D50 — a fall. A backlogged push-back under the floor, a window's or
/// a delivery sample's, opens a run when none is open or the open one is
/// older than [`CAPACITY_TTL`], and the floor is damped as above. The next
/// one while the run is open, with the clock the caller passes (the in-flight
/// budget's measured rate, [`Pipe::clock`]) under the floor too, is a fall:
/// the floor restarts at the more of the push-back and the clock,
/// backlogged, by the push-back's class, and the run closes
/// ([`Self::fell`]). A delivery that raises the floor, or a push-back at or
/// over it, closes a run; nothing else does. With no clock (the pair memory,
/// the budget off, the opening rate standing in) the law is the damped one,
/// byte for byte.
#[derive(Debug, Default, Clone, Copy)]
pub struct PathModel {
    floor: Option<Held>,
    /// Backlogged by construction: nothing else sets it.
    capacity: Option<(u32, Instant)>,
    /// FR-148 D46 B2 — the delivery sample that set the capacity; `None` when
    /// a window's push-back did.
    capacity_by: Option<PushedBy>,
    /// FR-148 D50 — the first backlogged push-back under the floor since a
    /// reading last met or raised it; whatever the clock read.
    run: Option<Instant>,
    /// FR-148 D50 — the last fall, for the `path model` line.
    fell: Option<Fell>,
}

/// FR-148 D50 — a fall as the model keeps it: when, and the floor before and
/// after.
#[derive(Debug, Clone, Copy)]
struct Fell {
    at: Instant,
    from: u32,
    to: u32,
}

/// FR-148 D50 — the model's last fall as the `path model` line writes it:
/// the floor before and after, and how long ago ([`PathModel::fell`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fall {
    pub from_bps: u32,
    pub to_bps: u32,
    pub age: Duration,
}

/// `<from_bps>-><to_bps>/<age>ms`, as the `path model` line writes it.
impl std::fmt::Display for Fall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}->{}/{}ms",
            self.from_bps,
            self.to_bps,
            self.age.as_millis()
        )
    }
}

/// FR-148 D46 B2 — the delivery sample whose push-back wrote a model number
/// (D46 item 10). The `path model` line and the close line say which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushedBy {
    /// P1: the sample read a standing queue (`QueueReading::queued`).
    P1,
    /// P3: with the in-flight budget computing, the sample counted, was
    /// backlogged, and no still span met it.
    P3,
}

impl PushedBy {
    /// The word the lines write.
    pub fn word(self) -> &'static str {
        match self {
            PushedBy::P1 => "p1",
            PushedBy::P3 => "p3",
        }
    }

    /// `p1`, `p3`, or `-` for a number no delivery sample wrote.
    pub fn word_of(by: Option<Self>) -> &'static str {
        by.map_or("-", Self::word)
    }
}

/// The reading that set the model's floor.
#[derive(Debug, Clone, Copy)]
struct Held {
    bps: u32,
    basis: Basis,
    remembered: bool,
    at: Instant,
    /// FR-148 D46 B2 — the delivery sample whose push-back lowered it.
    by: Option<PushedBy>,
}

impl PathModel {
    /// A delivery: it raises the floor, whichever measured class it is.
    pub fn delivered(&mut self, bps: u32, basis: Basis, now: Instant) {
        if bps > 0 && basis != Basis::Assumed {
            self.raise(bps, basis, false, now);
        }
    }

    /// FR-148 D46 B2 — a delivery sample's push-back, P1 or P3: the capacity
    /// at its rate, and the floor lowered damped toward it when it stands
    /// above. Never a raise, and never a floor where there was none.
    ///
    /// With no clock: the pair memory's call (D30), which reads no fall.
    pub fn sample_pushed_back(&mut self, bps: u32, by: PushedBy, now: Instant) {
        self.sample_pushed_back_clocked(bps, by, None, now);
    }

    /// FR-148 D50 — [`Self::sample_pushed_back`] with the clock a fall is
    /// read against ([`Pipe::clock`]): under the floor, the second of a run
    /// with the clock under the floor too restarts it ([`Self::under_floor`]).
    /// At or over the floor it closes a run, and still never raises.
    pub fn sample_pushed_back_clocked(
        &mut self,
        bps: u32,
        by: PushedBy,
        clock: Option<u32>,
        now: Instant,
    ) {
        if bps == 0 {
            return;
        }
        self.capacity = Some((bps, now));
        self.capacity_by = Some(by);
        let Some(f) = self.floor else {
            return;
        };
        if bps < f.bps {
            self.floor = Some(Held {
                bps: self.under_floor(f.bps, bps, clock, now),
                basis: Basis::Backlogged,
                remembered: false,
                at: now,
                by: Some(by),
            });
        } else {
            self.run = None;
        }
    }

    /// FR-148 D50 — a backlogged push-back of `bps` under the floor `floor`,
    /// a window's or a delivery sample's: the floor it leaves. A fall when a
    /// run is open, no older than [`CAPACITY_TTL`], and the clock reads under
    /// the floor too: the floor restarts at the more of the push-back and the
    /// clock, the run closes, and the fall is kept for the line. Else damped
    /// as built, and the push-back opens a run unless one is open, which
    /// keeps its first instant.
    fn under_floor(&mut self, floor: u32, bps: u32, clock: Option<u32>, now: Instant) -> u32 {
        let open = self
            .run
            .filter(|&first| now.saturating_duration_since(first) <= CAPACITY_TTL);
        match (open, clock.filter(|&clock| clock < floor)) {
            (Some(_), Some(clock)) => {
                let to = bps.max(clock);
                self.run = None;
                self.fell = Some(Fell {
                    at: now,
                    from: floor,
                    to,
                });
                to
            }
            _ => {
                self.run = Some(open.unwrap_or(now));
                super::rate_memory::damp(floor, bps)
            }
        }
    }

    /// FR-148 D50 — the last fall, and how long ago; `None` while none has.
    pub fn fell(&self, now: Instant) -> Option<Fall> {
        self.fell.map(|f| Fall {
            from_bps: f.from,
            to_bps: f.to,
            age: now.saturating_duration_since(f.at),
        })
    }

    /// FR-148 D46 B2 — the delivery sample that lowered the floor last, while
    /// that floor stands; `None` when another reading set it.
    pub fn floor_by(&self) -> Option<PushedBy> {
        self.floor.and_then(|f| f.by)
    }

    /// FR-148 D46 B2 — the delivery sample that set the capacity, while it is
    /// fresh ([`Self::capacity`]).
    pub fn capacity_by(&self, now: Instant) -> Option<PushedBy> {
        self.capacity(now).and(self.capacity_by)
    }

    /// A rate an earlier session measured (the pair memory's seed), taken at the
    /// open as the weaker measured class. Its age here runs from `now`; the
    /// caller, who knows the entry's age, adds it.
    pub fn remembered(&mut self, bps: u32, now: Instant) {
        if bps > 0 {
            self.raise(bps, Basis::AppLimited, true, now);
        }
    }

    /// A push-back. Backlogged, it is [`Pipe::observe_capacity`]'s law;
    /// application-limited, the path took everything offered, so it is a
    /// delivery and nothing more.
    ///
    /// With no clock: the pair memory's call (D30), which reads no fall.
    pub fn pushed_back(&mut self, bps: u32, basis: Basis, now: Instant) {
        self.pushed_back_clocked(bps, basis, None, now);
    }

    /// FR-148 D50 — [`Self::pushed_back`] with the clock a fall is read
    /// against ([`Pipe::clock`]): a backlogged push-back under the floor, the
    /// second of a run with the clock under the floor too, restarts it
    /// ([`Self::under_floor`]); one at or over the floor closes a run.
    pub fn pushed_back_clocked(
        &mut self,
        bps: u32,
        basis: Basis,
        clock: Option<u32>,
        now: Instant,
    ) {
        if bps == 0 {
            return;
        }
        match basis {
            Basis::Assumed => {}
            Basis::AppLimited => self.raise(bps, basis, false, now),
            Basis::Backlogged => {
                self.capacity = Some((bps, now));
                self.capacity_by = None;
                let set = |bps| Held {
                    bps,
                    basis,
                    remembered: false,
                    at: now,
                    by: None,
                };
                match self.floor {
                    Some(f) if bps < f.bps => {
                        let to = self.under_floor(f.bps, bps, clock, now);
                        self.floor = Some(set(to));
                    }
                    Some(f) if bps <= f.bps => self.run = None,
                    _ => {
                        self.run = None;
                        self.floor = Some(set(bps));
                    }
                }
            }
        }
    }

    /// FR-148 D50 — a reading that raises the floor closes a run.
    fn raise(&mut self, bps: u32, basis: Basis, remembered: bool, now: Instant) {
        if self.floor.is_none_or(|f| bps > f.bps) {
            self.run = None;
            self.floor = Some(Held {
                bps,
                basis,
                remembered,
                at: now,
                by: None,
            });
        }
    }

    /// The demonstrated floor, with the class of the reading that set it.
    pub fn floor(&self, now: Instant) -> Option<PathNumber> {
        self.floor.map(|f| PathNumber {
            bps: f.bps,
            basis: f.basis,
            remembered: f.remembered,
            age: now.saturating_duration_since(f.at),
        })
    }

    /// The last backlogged push-back, on [`CAPACITY_TTL`] as [`Pipe`]'s is.
    pub fn capacity(&self, now: Instant) -> Option<PathNumber> {
        self.capacity
            .filter(|(_, at)| now.duration_since(*at) <= CAPACITY_TTL)
            .map(|(bps, at)| {
                PathNumber::new(bps, Basis::Backlogged, now.saturating_duration_since(at))
            })
    }

    /// An assumed value (the governor's stand-in) as the model lets it stand:
    /// never below the demonstrated floor. Where the floor is at or above it,
    /// the floor stands, with its own tag.
    pub fn stand_in(&self, assumed: Option<PathNumber>, now: Instant) -> Option<PathNumber> {
        let assumed = assumed?;
        match self.floor(now) {
            Some(floor) if floor.bps >= assumed.bps => Some(floor),
            _ => Some(assumed),
        }
    }

    /// What it would offer the pair memory at a session's end: its floor, with
    /// the tag of the reading that set it, or nothing when the session measured
    /// nothing. Never an assumed value, since none enters.
    ///
    /// FR-148 P1 (D23) — and nothing while the floor is still the remembered
    /// one: a session whose readings never passed the seed offers nothing, so
    /// a writer that trusts this never writes the seed back as a new reading.
    pub fn offer(&self, now: Instant) -> Option<PathNumber> {
        self.floor(now).filter(|f| !f.remembered)
    }
}

/// FR-148 P0 — every path number a session's heartbeat carries, each tagged,
/// and the shadow model's view beside them: what the `path model` line writes.
/// Built by `RateGovernor::path_view` at a heartbeat.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PathView {
    /// `goodput_bps`: the blocked-send estimate.
    pub goodput: Option<PathNumber>,
    /// `pipe_belief`: [`Pipe`]'s (anchor, floor, capacity).
    pub belief: (Option<PathNumber>, Option<PathNumber>, Option<PathNumber>),
    /// `pipe_measured`: the reading a stall is judged against.
    pub measured: Option<PathNumber>,
    /// `prior_bps`: the stand-in.
    pub prior: Option<PathNumber>,
    /// `learned_ceiling_bps`: `0` is the nominal start, assumed.
    pub learned_ceiling: Option<PathNumber>,
    /// `slow_link_floor_bps`: the relieved floor, tagged as what it came from.
    pub slow_link_floor: Option<PathNumber>,
    /// The last arrival rate the viewer reported.
    pub viewer_rx: Option<PathNumber>,
    /// The pair memory's seed at the open.
    pub seed: Option<PathNumber>,
    /// The model's floor, which is also what it would offer for storing.
    pub model_floor: Option<PathNumber>,
    pub model_capacity: Option<PathNumber>,
    /// The stand-in as the model lets it stand ([`PathModel::stand_in`]).
    pub model_stand_in: Option<PathNumber>,
    /// FR-148 D46 B2 — the delivery sample, P1 or P3, whose push-back lowered
    /// the model's floor last and set its capacity ([`PathModel::floor_by`],
    /// [`PathModel::capacity_by`]); `None` where a window's reading did.
    pub model_floor_by: Option<PushedBy>,
    pub model_capacity_by: Option<PushedBy>,
    /// FR-148 D48 — the rule that last kept a reading out of the model and
    /// the memory, re-read it or bounded it, and how long ago
    /// ([`Pipe::ruled`]); `None` while none has.
    pub model_ruled: Option<(Ruled, Duration)>,
    /// FR-148 D50 — the model's last fall: the floor before and after, and
    /// how long ago ([`PathModel::fell`]); `None` while none has.
    pub model_fell: Option<Fall>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// FR-148 P0 — the law as it stood before the tags, verbatim (`Pipe` at
    /// `a0738e3b6`). Every cell in this module is checked against it.
    #[derive(Default)]
    struct Before {
        floor: Option<(u32, Instant)>,
        capacity: Option<(u32, Instant)>,
        delivered_n: u32,
        capacity_n: u32,
    }

    impl Before {
        fn observe_delivered(&mut self, bps: u32, now: Instant) -> bool {
            if bps == 0 {
                return false;
            }
            self.delivered_n = self.delivered_n.saturating_add(1);
            match self.floor {
                Some((best, _)) if bps <= best => false,
                _ => {
                    self.floor = Some((bps, now));
                    true
                }
            }
        }

        fn observe_capacity(&mut self, bps: u32, now: Instant) {
            if bps == 0 {
                return;
            }
            self.capacity_n = self.capacity_n.saturating_add(1);
            self.capacity = Some((bps, now));
            match self.floor {
                Some((floor, _)) if bps < floor => {
                    self.floor = Some((super::super::rate_memory::damp(floor, bps), now));
                }
                Some((floor, _)) if bps <= floor => {}
                _ => self.floor = Some((bps, now)),
            }
        }

        fn floor_bps(&self, _now: Instant) -> Option<u32> {
            self.floor.map(|(bps, _)| bps)
        }

        fn capacity_bps(&self, now: Instant) -> Option<u32> {
            self.capacity
                .filter(|(_, at)| now.duration_since(*at) <= CAPACITY_TTL)
                .map(|(bps, _)| bps)
        }

        fn ceiling_anchor_bps(&self, now: Instant) -> Option<u32> {
            match (self.capacity_bps(now), self.floor_bps(now)) {
                (Some(c), Some(f)) => Some(c.max(f)),
                (c, f) => c.or(f),
            }
        }

        fn counts(&self) -> (u32, u32) {
            (self.delivered_n, self.capacity_n)
        }
    }

    const BASES: [Basis; 3] = [Basis::Backlogged, Basis::AppLimited, Basis::Assumed];

    /// FR-148 P0, decision 2: a tag changes no number. Every cell in this module
    /// runs through this: the tagged [`Pipe`], fed a class that cycles through all
    /// three, beside [`Before`]. After each input and at each read, every reader
    /// gives `Before`'s number and each tagged reader its untagged reader's value.
    #[derive(Default)]
    struct Twin {
        p: Pipe,
        before: Before,
        n: usize,
    }

    impl Twin {
        fn basis(&mut self) -> Basis {
            self.n = self.n.wrapping_add(1);
            BASES[self.n % 3]
        }

        fn observe_delivered(&mut self, bps: u32, now: Instant) -> bool {
            let basis = self.basis();
            let rose = self.p.observe_delivered(bps, basis, now);
            assert_eq!(rose, self.before.observe_delivered(bps, now), "rise");
            self.check(now);
            rose
        }

        fn observe_capacity(&mut self, bps: u32, now: Instant) {
            let basis = self.basis();
            self.p.observe_capacity(bps, basis, now);
            self.before.observe_capacity(bps, now);
            self.check(now);
        }

        fn check(&self, now: Instant) {
            let p = &self.p;
            assert_eq!(p.floor_bps(now), self.before.floor_bps(now), "floor");
            assert_eq!(
                p.capacity_bps(now),
                self.before.capacity_bps(now),
                "capacity"
            );
            assert_eq!(
                p.ceiling_anchor_bps(now),
                self.before.ceiling_anchor_bps(now),
                "anchor"
            );
            assert_eq!(p.counts(), self.before.counts(), "counts");
            assert_eq!(p.floor_number(now).map(|n| n.bps), p.floor_bps(now));
            assert_eq!(p.capacity_number(now).map(|n| n.bps), p.capacity_bps(now));
            assert_eq!(
                p.anchor_number(now).map(|n| n.bps),
                p.ceiling_anchor_bps(now)
            );
        }

        fn floor_bps(&self, now: Instant) -> Option<u32> {
            self.check(now);
            self.p.floor_bps(now)
        }

        fn capacity_bps(&self, now: Instant) -> Option<u32> {
            self.check(now);
            self.p.capacity_bps(now)
        }

        fn ceiling_anchor_bps(&self, now: Instant) -> Option<u32> {
            self.check(now);
            self.p.ceiling_anchor_bps(now)
        }

        fn counts(&self) -> (u32, u32) {
            assert_eq!(self.p.counts(), self.before.counts());
            self.p.counts()
        }
    }

    /// xorshift64: the same sequence on every run and every platform.
    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut x = seed;
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        }
    }

    /// A rate over six decades, a zero, or the one before it again.
    fn draw_bps(next: &mut impl FnMut() -> u64, last: u32) -> u32 {
        match next() % 10 {
            0 => 0,
            1 => last,
            _ => {
                let base = 10u64.pow(2 + (next() % 6) as u32);
                (base + next() % (9 * base)) as u32
            }
        }
    }

    /// FR-148 P0, decision 2, over a long random sequence: 100,000 deliveries and
    /// push-backs, values over six decades with zeros and repeats, steps from none
    /// to past [`CAPACITY_TTL`], each with a class drawn at random, and a read at a
    /// random later moment after each. Every number is `Before`'s.
    #[test]
    fn tags_change_no_number() {
        let mut next = rng(0x9E37_79B9_7F4A_7C15);
        let start = t0();
        let mut t = start;
        let mut twin = Twin::default();
        let mut last = 0;
        let (mut rises, mut pushes) = (0u32, 0u32);
        for _ in 0..100_000 {
            let bps = draw_bps(&mut next, last);
            last = bps;
            t += match next() % 16 {
                0 => CAPACITY_TTL + Duration::from_millis(next() % 5_000),
                1 => Duration::ZERO,
                _ => Duration::from_millis(next() % 3_000),
            };
            twin.n = (next() % 3) as usize;
            if next().is_multiple_of(3) {
                twin.observe_capacity(bps, t);
                pushes += 1;
            } else if twin.observe_delivered(bps, t) {
                rises += 1;
            }
            twin.check(t + Duration::from_millis(next() % 70_000));
        }
        // The sequence reached every branch of both laws.
        assert!(
            rises > 100 && pushes > 30_000,
            "{rises} rises, {pushes} push-backs"
        );
    }

    /// FR-148 P0 — with every input backlogged, the model is [`Pipe`]'s law
    /// exactly; with every one application-limited, its capacity never moves and
    /// its floor is the highest reading.
    #[test]
    fn the_model_is_pipes_law_when_everything_is_backlogged() {
        for basis in [Basis::Backlogged, Basis::AppLimited] {
            let mut next = rng(0xD1B5_4A32_D192_ED03);
            let mut t = t0();
            let mut p = Pipe::default();
            let mut highest = 0;
            let mut last = 0;
            for _ in 0..20_000 {
                let bps = draw_bps(&mut next, last);
                last = bps;
                t += Duration::from_millis(next() % 4_000);
                if next().is_multiple_of(3) {
                    p.observe_capacity(bps, basis, t);
                } else {
                    p.observe_delivered(bps, basis, t);
                }
                highest = highest.max(bps);
                let m = p.model();
                if basis == Basis::Backlogged {
                    assert_eq!(m.floor(t).map(|n| n.bps), p.floor_bps(t));
                    assert_eq!(m.capacity(t).map(|n| n.bps), p.capacity_bps(t));
                } else {
                    assert_eq!(m.capacity(t), None);
                    assert_eq!(m.floor(t).map(|n| n.bps), Some(highest).filter(|h| *h > 0));
                }
            }
        }
    }

    /// FR-148 P0 — the jupiter shape at this type's level: a minute's delivery,
    /// then a push-back read on idle traffic. [`Pipe`] takes the trickle as the
    /// capacity and halves its floor toward it; the model, which reads that the
    /// sender was application-limited, moves neither, and offers the floor.
    #[test]
    fn an_application_limited_push_back_lowers_nothing_in_the_model() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered(15_915_240, Basis::AppLimited, now);
        let t = now + Duration::from_secs(7);
        p.observe_capacity(9_489, Basis::AppLimited, t);
        assert_eq!(
            (p.floor_bps(t), p.capacity_bps(t)),
            (Some(7_962_364), Some(9_489)),
            "the law of 2026-11-16"
        );
        let trickle = p.capacity_number(t).expect("a capacity");
        assert_eq!((trickle.bps, trickle.word()), (9_489, "app-limited"));
        let m = p.model();
        assert_eq!(m.capacity(t), None, "no capacity from a trickle");
        let floor = PathNumber::new(15_915_240, Basis::AppLimited, Duration::from_secs(7));
        assert_eq!(m.floor(t), Some(floor), "the floor the minute set");
        assert_eq!(m.offer(t), Some(floor));
        // A backlogged push-back is the evidence that may lower it.
        p.observe_capacity(2_000_000, Basis::Backlogged, t);
        let floor = m_floor(&p, t);
        assert!(floor.bps < 15_915_240 && floor.basis == Basis::Backlogged);
        assert_eq!(p.model().capacity(t).map(|c| c.bps), Some(2_000_000));
    }

    fn m_floor(p: &Pipe, t: Instant) -> PathNumber {
        p.model().floor(t).expect("a floor")
    }

    /// FR-148 P0 — an assumed value is never offered and never stands below a
    /// demonstrated floor; above the floor it stands as what it is.
    #[test]
    fn an_assumed_value_never_stands_below_the_floor_and_is_never_offered() {
        let now = t0();
        let mut m = PathModel::default();
        let stand_in = PathNumber::new(179_980, Basis::Assumed, Duration::from_secs(1));
        assert_eq!(
            m.offer(now),
            None,
            "a session that measured nothing offers nothing"
        );
        assert_eq!(
            m.stand_in(Some(stand_in), now),
            Some(stand_in),
            "no floor to stand on"
        );
        m.delivered(179_980, Basis::Assumed, now);
        m.pushed_back(179_980, Basis::Assumed, now);
        assert_eq!(m.offer(now), None, "nothing assumed enters");
        m.delivered(1_399_272, Basis::AppLimited, now);
        let floor = PathNumber::new(1_399_272, Basis::AppLimited, Duration::ZERO);
        assert_eq!(m.stand_in(Some(stand_in), now), Some(floor));
        assert_eq!(m.offer(now), Some(floor));
        let high = PathNumber::new(2_000_000, Basis::Assumed, Duration::ZERO);
        assert_eq!(m.stand_in(Some(high), now), Some(high));
        assert_eq!(m.stand_in(None, now), None);
    }

    /// FR-148 P0 — the pair memory's seed is a reading an earlier session took:
    /// the model opens on it, marked remembered. This session's readings raise
    /// it, and only a backlogged one lowers it. The pipe never sees it.
    #[test]
    fn a_remembered_seed_is_an_application_limited_floor() {
        let now = t0();
        let mut p = Pipe::default();
        p.remember(3_322, now);
        assert_eq!((p.floor_bps(now), p.capacity_bps(now)), (None, None));
        let seed = PathNumber::remembered(3_322, Duration::ZERO);
        assert_eq!(p.model().floor(now), Some(seed), "the seed is the floor");
        // FR-148 P1 (D23): until 2026-11-17 this offered the seed itself.
        assert_eq!(p.model().offer(now), None, "measured nothing: no offer");
        let t = now + Duration::from_secs(2);
        p.observe_capacity(2_000, Basis::AppLimited, t);
        assert_eq!(
            m_floor(&p, t).bps,
            3_322,
            "an application-limited push-back"
        );
        p.observe_delivered(9_120, Basis::AppLimited, t);
        let delivered = PathNumber::new(9_120, Basis::AppLimited, Duration::ZERO);
        assert_eq!(p.model().offer(t), Some(delivered));
        p.observe_capacity(4_000, Basis::Backlogged, t);
        let floor = m_floor(&p, t);
        assert!(
            floor.bps < 9_120 && floor.basis == Basis::Backlogged,
            "{floor:?}"
        );
    }

    /// FR-148 P0 — how the line writes a number: value, word, age in ms.
    #[test]
    fn a_number_is_written_as_value_word_and_age() {
        let age = Duration::from_millis(2_330_400);
        let cases = [
            (
                PathNumber::new(1_399_272, Basis::AppLimited, age),
                "1399272/app-limited/2330400ms",
            ),
            (
                PathNumber::new(564_865, Basis::Backlogged, Duration::ZERO),
                "564865/backlogged/0ms",
            ),
            (
                PathNumber::new(179_980, Basis::Assumed, age),
                "179980/assumed/2330400ms",
            ),
            (
                PathNumber::remembered(3_322, age),
                "3322/remembered/2330400ms",
            ),
        ];
        for (n, want) in cases {
            assert_eq!(n.to_string(), want);
        }
        assert_eq!(PathNumber::remembered(3_322, age).basis, Basis::AppLimited);
    }

    /// Symptom 1, CORPLAP-3: a session that never blocks a send still knows
    /// something about its path. Before this type it knew nothing at all, and
    /// FR-79 V5's rule was inert there.
    #[test]
    fn a_session_that_never_pushed_back_still_has_a_belief() {
        let now = t0();
        let mut p = Twin::default();
        p.observe_delivered(900_000, now);
        p.observe_delivered(1_200_000, now + Duration::from_secs(1));
        assert_eq!(p.capacity_bps(now + Duration::from_secs(1)), None);
        assert_eq!(
            p.ceiling_anchor_bps(now + Duration::from_secs(1)),
            Some(1_200_000),
            "the belief falls back to what was demonstrably delivered"
        );
    }

    /// Symptom 2, WINHOST-K: a capacity sample of 6.03 Mbps arrives on a
    /// path that has just delivered 20 Mbps. The old `min` rule read 6 M and
    /// the target collapsed to it; the delivery is the harder fact.
    #[test]
    fn one_capacity_sample_does_not_collapse_the_belief_onto_itself() {
        let now = t0();
        let mut p = Twin::default();
        p.observe_delivered(20_000_000, now);
        let t = now + Duration::from_secs(1);
        p.observe_capacity(6_028_814, t);
        let anchor = p.ceiling_anchor_bps(t).expect("an anchor");
        // ALPHA_DOWN halves the gap: 20 M -> ~13 M, not 6.03 M. Collapsing onto
        // the sample IS the Regal sawtooth, rebuilt inside the estimator.
        assert_eq!(anchor, 13_014_407);
        assert!(
            anchor > 6_028_814 * 2,
            "a single blocked-send sample must not erase a 20 Mbps delivery"
        );
    }

    /// ...but SUSTAINED push-back converges on it, so a path that really did
    /// shrink is believed at its new size rather than its old peak. Without
    /// this the floor would be the 38.88 Mbps constant with extra steps.
    #[test]
    fn sustained_push_back_converges_the_floor_onto_the_capacity() {
        let now = t0();
        let mut p = Twin::default();
        p.observe_delivered(20_000_000, now);
        let mut t = now;
        for _ in 0..8 {
            t += Duration::from_secs(1);
            p.observe_capacity(6_028_814, t);
        }
        let anchor = p.ceiling_anchor_bps(t).expect("an anchor");
        assert!(
            anchor < 6_500_000,
            "eight consistent samples should converge on the measurement, got {anchor}"
        );
    }

    /// ...but on a path that really is slow, the capacity estimate stands: the
    /// floor is whatever that path actually delivered, which is also small.
    #[test]
    fn a_genuinely_slow_path_believes_its_measurement() {
        let now = t0();
        let mut p = Twin::default();
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
        let mut p = Twin::default();
        p.observe_delivered(18_000_000, now);
        for i in 1..8 {
            p.observe_delivered(300_000, now + Duration::from_secs(i));
        }
        assert_eq!(p.floor_bps(now + Duration::from_secs(8)), Some(18_000_000));
    }

    /// The overnight shadow, verbatim: CORPLAP-3 demonstrated 8.4 Mbps at
    /// 20:02 and read 12,818 bps at 13:25 the next day with a static screen and
    /// `n=(10696, 0)` — ten thousand deliveries, not one push-back.
    ///
    /// A still screen is NOT evidence the path shrank. The floor must survive
    /// it, or FR-74 P5's ceiling would collapse on an idle desktop and starve
    /// the next burst — the very symptom this arc exists to remove.
    #[test]
    fn hours_of_a_still_screen_do_not_retire_what_the_path_demonstrated() {
        let now = t0();
        let mut p = Twin::default();
        p.observe_delivered(8_409_584, now);
        // Seventeen hours of an idle desktop, one report a second.
        let mut t = now;
        for _ in 0..2_000 {
            t += Duration::from_secs(1);
            p.observe_delivered(12_818, t);
        }
        assert_eq!(
            p.floor_bps(t),
            Some(8_409_584),
            "the demonstration stands; quiet is not contrary evidence"
        );
        assert_eq!(p.counts(), (2_001, 0));
    }

    /// A push-back is the ONLY thing that moves the floor down, and it damps
    /// rather than snaps — see `one_capacity_sample_does_not_collapse_the_
    /// belief_onto_itself` and `sustained_push_back_converges_the_floor_onto_
    /// the_capacity` for the two halves of that law.
    #[test]
    fn nothing_but_a_push_back_moves_the_floor_down() {
        let now = t0();
        let mut p = Twin::default();
        p.observe_delivered(18_000_000, now);
        let mut t = now;
        // Deliveries below the floor, however many, never lower it.
        for _ in 0..50 {
            t += Duration::from_secs(1);
            p.observe_delivered(20_000, t);
        }
        assert_eq!(p.floor_bps(t), Some(18_000_000));
        // One push-back does.
        p.observe_capacity(2_000_000, t);
        assert!(p.floor_bps(t).expect("a floor") < 18_000_000);
    }

    /// Field 2026-09-12, CORPLAP-3 session `6aa53186`, verbatim: a capacity of
    /// 13,163,844 measured at 11:04 against a content-driven floor of
    /// 4,050,945. One `CAPACITY_TTL` later the capacity had expired and the
    /// anchor fell back to the floor, while the target ran to the 34,560,000
    /// constant.
    ///
    /// A blocked-send goodput IS a delivery — those bytes got through at that
    /// rate — so it must raise the floor, and the anchor must survive the
    /// capacity expiring.
    #[test]
    fn a_capacity_sample_survives_its_own_expiry_by_raising_the_floor() {
        let now = t0();
        let mut p = Twin::default();
        p.observe_delivered(4_050_945, now);
        p.observe_capacity(13_163_844, now + Duration::from_secs(1));
        let after_ttl = now + Duration::from_secs(1) + CAPACITY_TTL + Duration::from_secs(1);
        assert_eq!(p.capacity_bps(after_ttl), None, "the estimate aged out");
        assert_eq!(
            p.ceiling_anchor_bps(after_ttl),
            Some(13_163_844),
            "but the path demonstrably carried 13.16 Mbps, and that does not \
             stop being true when the estimate expires"
        );
    }

    /// A capacity estimate expires on the goodput estimator's own clock, so a
    /// push-back from a minute ago stops steering the session.
    #[test]
    fn a_capacity_estimate_expires() {
        let now = t0();
        let mut p = Twin::default();
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
        let mut p = Twin::default();
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

    /// FR-79 V5b reads the rise, so the return value has to mean exactly that:
    /// a first delivery and a higher one rise, an equal or lower one does not,
    /// and a zero window is no delivery at all.
    #[test]
    fn a_delivery_says_whether_it_raised_the_floor() {
        let now = t0();
        let mut p = Twin::default();
        assert!(!p.observe_delivered(0, now), "a zero is not a delivery");
        assert!(p.observe_delivered(1_199_442, now), "the first one rises");
        assert!(
            !p.observe_delivered(1_199_442, now),
            "an equal one does not"
        );
        assert!(!p.observe_delivered(300_000, now), "a lower one does not");
        assert!(p.observe_delivered(2_423_632, now), "a higher one does");
    }

    /// Nothing observed at all is still `None` — the honest answer before the
    /// first frame, and the one FR-79 V5 reads as "no claim to contradict".
    #[test]
    fn an_untouched_pipe_believes_nothing() {
        assert_eq!(Twin::default().ceiling_anchor_bps(t0()), None);
        assert_eq!(Twin::default().counts(), (0, 0));
    }

    /// Zero is not an observation — a window that delivered nothing says
    /// nothing about the path, and must not become its floor.
    #[test]
    fn a_zero_window_is_not_evidence() {
        let now = t0();
        let mut p = Twin::default();
        p.observe_delivered(0, now);
        p.observe_capacity(0, now);
        assert_eq!(p.ceiling_anchor_bps(now), None);
        assert_eq!(p.counts(), (0, 0));
    }

    /// FR-148 D46 B2 — a delivery sample's push-back moves the shadow model
    /// and the pair memory, and nothing of this type's own: the capacity at
    /// its rate, the floor lowered damped toward it (sustained evidence
    /// converges on it), never raised, and never set where there was none.
    /// Counted in the memory under the window's take, in the split as well.
    #[test]
    fn a_sample_push_back_moves_only_the_model_and_never_raises_its_floor() {
        use super::super::pair_memory::{Read, Take};
        let now = t0();
        let (clear, over) = (Take::Sample(Read::Clear), Take::Sample(Read::Overproduced));
        let mut p = Pipe::default();
        p.observe_delivered_in(2_400_000, Basis::AppLimited, true, now);
        p.observe_sample_capacity(1_500_000, PushedBy::P1, over, now);
        let m = p.model();
        assert_eq!(m.capacity(now).map(|c| c.bps), Some(1_500_000));
        assert_eq!(m.capacity_by(now), Some(PushedBy::P1));
        let damped = super::super::rate_memory::damp(2_400_000, 1_500_000);
        assert_eq!(
            m.floor(now).map(|f| (f.bps, f.basis)),
            Some((damped, Basis::Backlogged))
        );
        assert_eq!(m.floor_by(), Some(PushedBy::P1));
        // This type's own numbers: the delivery's, and no capacity.
        assert_eq!(p.floor_bps(now), Some(2_400_000));
        assert_eq!(p.capacity_bps(now), None);
        assert_eq!(p.counts(), (1, 0));
        for _ in 0..20 {
            p.observe_sample_capacity(1_500_000, PushedBy::P3, clear, now);
        }
        let floor = p.model().floor(now).unwrap().bps;
        assert!(floor.abs_diff(1_500_000) <= 15_000, "{floor}");
        assert_eq!(p.model().floor_by(), Some(PushedBy::P3));
        // A burst no rule saw is still no raise.
        p.observe_sample_capacity(3_000_000, PushedBy::P1, clear, now);
        assert_eq!(p.model().floor(now).unwrap().bps, floor);
        assert_eq!(p.model().capacity(now).map(|c| c.bps), Some(3_000_000));
        let c = p.memory().closing(now, 1_795_000_000);
        assert_eq!((c.counted, c.counted_overproduced), (22, 1));
        assert_eq!((c.counted_p1, c.counted_p3), ((2, 1), (20, 20)));
        // A window's push-back after them is today's again.
        p.observe_capacity(1_400_000, Basis::Backlogged, now);
        assert_eq!(p.model().capacity_by(now), None);
        let mut fresh = Pipe::default();
        fresh.observe_sample_capacity(1_000_000, PushedBy::P3, clear, now);
        assert_eq!(fresh.model().floor(now), None);
        assert_eq!(fresh.model().capacity(now).map(|c| c.bps), Some(1_000_000));
        assert_eq!(fresh.floor_bps(now), None);
    }

    /// FR-148 D46 B2 — a stall the in-flight budget saw keeps the next
    /// [`STALL_WINDOWS`] window out of the shadow model and the pair memory,
    /// its delivery and its push-backs alike, counted from the last mark; a
    /// window that delivered nothing counts for none. This type's own law
    /// takes every one, as before.
    #[test]
    fn a_stall_the_budget_saw_keeps_the_next_window_out_of_the_model() {
        let now = t0();
        let mut p = Pipe::default();
        p.observe_delivered_in(1_000_000, Basis::AppLimited, true, now);
        p.note_stall();
        p.note_stall();
        assert!(!p.observe_delivered_in(0, Basis::AppLimited, true, now));
        p.observe_delivered_in(3_000_000, Basis::Backlogged, true, now);
        p.observe_capacity(3_000_000, Basis::Backlogged, now);
        assert_eq!(p.model().floor(now).map(|f| f.bps), Some(1_000_000));
        assert_eq!(p.model().capacity(now), None);
        assert_eq!(p.memory().closing(now, 1_795_000_000).counted, 0);
        assert_eq!(p.capacity_bps(now), Some(3_000_000), "its own law took it");
        assert_eq!(p.counts(), (2, 1));
        p.observe_delivered_in(1_200_000, Basis::Backlogged, true, now);
        assert_eq!(p.model().floor(now).map(|f| f.bps), Some(1_200_000));
        p.observe_capacity(1_100_000, Basis::Backlogged, now);
        assert_eq!(p.model().capacity(now).map(|c| c.bps), Some(1_100_000));
        // A rejected window is no reading either way, and it is the one the
        // stall keeps out.
        p.note_stall();
        p.observe_delivered_in(1_300_000, Basis::Backlogged, false, now);
        p.observe_delivered_in(1_400_000, Basis::Backlogged, true, now);
        assert_eq!(p.model().floor(now).map(|f| f.bps), Some(1_400_000));
    }

    /// FR-148 D48 — what the in-flight budget's last step read: its rate,
    /// given by this association's figures or not, and SCTP's loss count.
    fn path(bps: u32, measured: bool, loss: u64) -> PathRate {
        PathRate {
            bps: Some(bps),
            measured,
            loss,
        }
    }

    /// The take every D48 cell files its push-backs under.
    fn pushed_take() -> super::super::pair_memory::Take {
        use super::super::pair_memory::{Read, Take};
        Take::Sample(Read::Overproduced)
    }

    /// FR-148 D48 — a window's close at `now`, the budget's step `noted`
    /// before it: its delivery, then the link loop's push-back at it.
    fn link_window(p: &mut Pipe, noted: PathRate, bps: u32, now: Instant) {
        p.note_path_rate(noted);
        p.observe_delivered_in(bps, Basis::Backlogged, true, now);
        p.observe_link_capacity_in(bps, Basis::Backlogged, pushed_take(), now);
    }

    /// The window as the model took it before D48: its delivery and its
    /// push-back at `bps`.
    fn plain_window(p: &mut Pipe, bps: u32, now: Instant) {
        p.observe_delivered_in(bps, Basis::Backlogged, true, now);
        p.observe_capacity_in(bps, Basis::Backlogged, pushed_take(), now);
    }

    /// The shadow model and the pair memory read exactly what `want`'s did.
    fn assert_same_reading(p: &Pipe, want: &Pipe) {
        assert_eq!(format!("{:?}", p.model()), format!("{:?}", want.model()));
        assert_eq!(format!("{:?}", p.memory()), format!("{:?}", want.memory()));
    }

    /// FR-148 D48, rule 2 (`bm0`) — near a loss, a window that read over the
    /// budget's measured rate reaches the shadow model and the pair memory at
    /// that rate, its delivery and its link push-back alike: a hole's release
    /// comes back at once. This type's own law takes it as read, and the
    /// `path model` line names the rule.
    #[test]
    fn near_a_loss_a_window_over_the_measured_rate_is_taken_at_that_rate() {
        let now = t0();
        let later = now + Duration::from_secs(1);
        let mut p = Pipe::default();
        link_window(&mut p, path(1_000_000, true, 0), 900_000, now);
        assert_eq!(p.ruled(now), None);
        link_window(&mut p, path(1_000_000, true, 1), 2_500_000, later);
        let mut want = Pipe::default();
        plain_window(&mut want, 900_000, now);
        plain_window(&mut want, 1_000_000, later);
        assert_same_reading(&p, &want);
        assert_eq!(p.model().floor(later).map(|f| f.bps), Some(1_000_000));
        assert_eq!(p.model().capacity(later).map(|c| c.bps), Some(1_000_000));
        assert_eq!(p.floor_bps(later), Some(2_500_000), "its own law: as read");
        assert_eq!(p.capacity_bps(later), Some(2_500_000));
        assert_eq!(p.ruled(later), Some((Ruled::Bound, Duration::ZERO)));
        assert_eq!(
            p.ruled(later + Duration::from_millis(1_200)),
            Some((Ruled::Bound, Duration::from_millis(1_200)))
        );
    }

    /// FR-148 D48, rule 2 — near a loss, a window that read over a rate the
    /// opening stood in for is no reading for the shadow model or the pair
    /// memory, its delivery and both push-backs, as a stall's window is not:
    /// a stand-in is no measurement to bound it by, nor to call a hole by.
    /// This type's own law takes it as read.
    #[test]
    fn near_a_loss_a_window_over_a_stand_in_rate_is_no_reading() {
        let now = t0();
        let (later, next) = (now + Duration::from_secs(1), now + Duration::from_secs(2));
        let mut p = Pipe::default();
        link_window(&mut p, path(1_500_000, false, 0), 900_000, now);
        p.note_path_rate(path(1_500_000, false, 2));
        p.observe_delivered_in(2_500_000, Basis::Backlogged, true, later);
        p.observe_capacity_in(2_400_000, Basis::Backlogged, pushed_take(), later);
        p.observe_link_capacity_in(2_500_000, Basis::Backlogged, pushed_take(), later);
        let mut want = Pipe::default();
        plain_window(&mut want, 900_000, now);
        assert_same_reading(&p, &want);
        assert_eq!(p.floor_bps(later), Some(2_500_000), "its own law took it");
        assert_eq!(p.capacity_bps(later), Some(2_500_000));
        assert_eq!(p.ruled(later), Some((Ruled::Bound, Duration::ZERO)));
        // Under the stand-in, as read: no hole is called by one.
        link_window(&mut p, path(1_500_000, false, 2), 600_000, next);
        plain_window(&mut want, 600_000, next);
        assert_same_reading(&p, &want);
    }

    /// FR-148 D48, rule 3 (`u90`) — near a loss, a window that read under 0.9
    /// times the budget's measured rate makes no link push-back on the shadow
    /// model or the pair memory: the viewer read SCTP's order behind a hole,
    /// not the path. Its delivery and the goodput fold's push-back count, and
    /// this type's own law takes all of it. At 0.9 times the rate it is a
    /// reading.
    #[test]
    fn near_a_loss_a_window_under_nine_tenths_of_the_rate_makes_no_link_push_back() {
        let now = t0();
        let (later, next) = (now + Duration::from_secs(1), now + Duration::from_secs(2));
        let mut p = Pipe::default();
        link_window(&mut p, path(2_000_000, true, 0), 1_900_000, now);
        p.note_path_rate(path(2_000_000, true, 1));
        p.observe_delivered_in(1_000_000, Basis::Backlogged, true, later);
        p.observe_capacity_in(1_200_000, Basis::Backlogged, pushed_take(), later);
        p.observe_link_capacity_in(1_000_000, Basis::Backlogged, pushed_take(), later);
        // The delivery and the goodput fold's push-back; no link push-back.
        let mut want = Pipe::default();
        plain_window(&mut want, 1_900_000, now);
        want.observe_delivered_in(1_000_000, Basis::Backlogged, true, later);
        want.observe_capacity_in(1_200_000, Basis::Backlogged, pushed_take(), later);
        assert_same_reading(&p, &want);
        assert_eq!(
            p.capacity_bps(later),
            Some(1_000_000),
            "its own law took it"
        );
        assert_eq!(p.ruled(later), Some((Ruled::Hole, Duration::ZERO)));
        // The equal case: 1.8 Mbit/s is 0.9 times the rate, a reading.
        link_window(&mut p, path(2_000_000, true, 1), 1_800_000, next);
        plain_window(&mut want, 1_800_000, next);
        assert_same_reading(&p, &want);
        assert_eq!(p.ruled(next), Some((Ruled::Hole, Duration::from_secs(1))));
    }

    /// FR-148 D48 — near a loss is the window SCTP's count moved over, or
    /// the window's length before it: a hole's release reaches the viewer a
    /// round trip or so after the retransmission the count counts. Two
    /// windows on, the rules read nothing, and a window over the rate is
    /// taken as read.
    #[test]
    fn a_loss_is_near_for_the_window_it_moved_in_and_the_next_only() {
        let now = t0();
        let s = |n: u64| now + Duration::from_secs(n);
        let floor = |p: &Pipe, n: u64| p.model().floor(s(n)).map(|f| f.bps);
        let mut p = Pipe::default();
        link_window(&mut p, path(1_000_000, true, 0), 1_000_000, s(0));
        link_window(&mut p, path(1_000_000, true, 1), 1_500_000, s(1));
        assert_eq!(floor(&p, 1), Some(1_000_000), "the window it moved in");
        link_window(&mut p, path(1_000_000, true, 1), 1_600_000, s(2));
        assert_eq!(floor(&p, 2), Some(1_000_000), "the window after it");
        link_window(&mut p, path(1_000_000, true, 1), 1_700_000, s(3));
        assert_eq!(floor(&p, 3), Some(1_700_000), "two windows on: as read");
    }

    /// FR-148 D48 — B2's stall gate acts before the window rules: a window
    /// after a stall the budget saw is no reading, whatever it read near a
    /// loss, and the `path model` line names the gate. A window that
    /// delivered nothing is no reading either way.
    #[test]
    fn the_stall_gate_acts_before_the_window_rules() {
        let now = t0();
        let later = now + Duration::from_secs(1);
        let mut p = Pipe::default();
        link_window(&mut p, path(2_000_000, true, 0), 1_900_000, now);
        p.note_stall();
        link_window(&mut p, path(2_000_000, true, 1), 3_000_000, later);
        let mut want = Pipe::default();
        plain_window(&mut want, 1_900_000, now);
        assert_same_reading(&p, &want);
        assert_eq!(p.ruled(later), Some((Ruled::Stall, Duration::ZERO)));
        // Under 0.9 times the rate, after a stall: the gate's, not a hole's.
        p.note_stall();
        link_window(&mut p, path(2_000_000, true, 2), 1_000_000, later);
        assert_same_reading(&p, &want);
        assert_eq!(p.ruled(later), Some((Ruled::Stall, Duration::ZERO)));
        let next = later + Duration::from_secs(1);
        link_window(&mut p, path(2_000_000, true, 3), 0, next);
        assert_same_reading(&p, &want);
    }

    /// FR-148 D48 — with the in-flight budget off nothing notes a rate, and
    /// with it on a path that loses nothing the rules read nothing: every
    /// window reaches the shadow model and the pair memory byte for byte as
    /// before, over and under the rate, the link loop's push-back as the
    /// goodput fold's, and a stall's gate as B2 left it. No rule is named.
    #[test]
    fn with_no_loss_or_the_budget_off_the_model_reads_every_window_as_before() {
        let now = t0();
        let s = |n: u64| now + Duration::from_millis(n * 1_000);
        let windows: [(u32, Option<u32>, bool); 6] = [
            (900_000, Some(850_000), false),
            (2_500_000, None, false),
            (600_000, Some(700_000), true),
            (3_000_000, Some(2_900_000), false),
            (0, None, false),
            (1_100_000, None, false),
        ];
        let (mut before, mut off, mut lossless) =
            (Pipe::default(), Pipe::default(), Pipe::default());
        for (n, &(rx, goodput, stall)) in (0..).zip(windows.iter()) {
            let now = s(n);
            lossless.note_path_rate(path(1_000_000, n > 1, 0));
            for p in [&mut before, &mut off, &mut lossless] {
                if stall {
                    p.note_stall();
                }
                p.observe_delivered_in(rx, Basis::Backlogged, true, now);
                if let Some(g) = goodput {
                    p.observe_capacity_in(g, Basis::Backlogged, pushed_take(), now);
                }
            }
            before.observe_capacity_in(rx, Basis::Backlogged, pushed_take(), now);
            off.observe_link_capacity_in(rx, Basis::Backlogged, pushed_take(), now);
            lossless.observe_link_capacity_in(rx, Basis::Backlogged, pushed_take(), now);
        }
        let end = s(6);
        for p in [&off, &lossless] {
            assert_same_reading(p, &before);
            assert_eq!(p.floor_bps(end), before.floor_bps(end));
            assert_eq!(p.capacity_bps(end), before.capacity_bps(end));
            assert_eq!(p.counts(), before.counts());
            assert_eq!(p.ruled(end), Some((Ruled::Stall, Duration::from_secs(4))));
        }
    }

    /// FR-148 D50 — the in-flight budget's measured rate, with no loss: the
    /// clock the cells below note before a push-back.
    fn clock(bps: u32) -> PathRate {
        path(bps, true, 0)
    }

    /// FR-148 D50 — a pipe whose shadow model's floor a valid delivery of
    /// `bps` set at `now`, with no rate noted yet.
    fn floored(bps: u32, now: Instant) -> Pipe {
        let mut p = Pipe::default();
        p.observe_delivered_in(bps, Basis::AppLimited, true, now);
        p
    }

    /// FR-148 D50 — a P3 sample's push-back of `bps` at `now`, the budget's
    /// measured rate noted at `clock_bps` before it.
    fn p3_at(p: &mut Pipe, bps: u32, clock_bps: u32, now: Instant) {
        p.note_path_rate(clock(clock_bps));
        p.observe_sample_capacity(bps, PushedBy::P3, pushed_take(), now);
    }

    fn model_floor(p: &Pipe, now: Instant) -> u32 {
        m_floor(p, now).bps
    }

    /// FR-148 D50, cell 1 — a lone backlogged push-back under the floor is
    /// damped as built, the clock under the floor or not: it sets the
    /// capacity, opens a run, and the model reads no fall.
    #[test]
    fn a_lone_push_back_under_the_floor_is_damped_as_built() {
        use super::super::rate_memory::damp;
        let now = t0();
        let mut p = floored(2_000_000, now);
        let t = now + Duration::from_secs(2);
        p3_at(&mut p, 1_000_000, 1_000_000, t);
        let m = p.model();
        assert_eq!(
            m.floor(t).map(|f| (f.bps, f.basis)),
            Some((damp(2_000_000, 1_000_000), Basis::Backlogged))
        );
        assert_eq!(m.floor_by(), Some(PushedBy::P3));
        assert_eq!(m.capacity(t).map(|c| c.bps), Some(1_000_000));
        assert_eq!(m.fell(t), None);
    }

    /// FR-148 D50, cell 2 — the second backlogged push-back under the floor,
    /// with the budget's measured rate under it too, is a fall: the floor
    /// restarts at the more of the push-back and the clock, backlogged, by
    /// the push-back's class, under the floor it replaced. The capacity is
    /// the push-back's, as always. The model keeps the fall for the line:
    /// the floor before and after, and its age.
    #[test]
    fn a_second_push_back_under_the_floor_with_the_clock_under_it_is_a_fall() {
        use super::super::rate_memory::damp;
        let now = t0();
        let mut p = floored(2_000_000, now);
        let (t1, t2) = (now + Duration::from_secs(2), now + Duration::from_secs(4));
        p3_at(&mut p, 1_000_000, 1_000_000, t1);
        let damped = model_floor(&p, t1);
        assert_eq!(damped, damp(2_000_000, 1_000_000));
        p3_at(&mut p, 1_000_000, 1_020_000, t2);
        let m = p.model();
        let floor = m.floor(t2).expect("a floor");
        assert_eq!(
            (floor.bps, floor.basis, floor.age),
            (1_020_000, Basis::Backlogged, Duration::ZERO)
        );
        assert!(floor.bps < damped, "never over the floor it replaced");
        assert_eq!(m.floor_by(), Some(PushedBy::P3));
        assert_eq!(m.capacity(t2).map(|c| c.bps), Some(1_000_000));
        let fall = Fall {
            from_bps: damped,
            to_bps: 1_020_000,
            age: Duration::ZERO,
        };
        assert_eq!(m.fell(t2), Some(fall));
        assert_eq!(fall.to_string(), "1500000->1020000/0ms");
        let later = t2 + Duration::from_millis(3_400);
        assert_eq!(
            m.fell(later).map(|f| f.to_string()),
            Some("1500000->1020000/3400ms".to_owned())
        );
    }

    /// FR-148 D50, cell 3 — a valid delivery that raises the floor between
    /// the two closes the run: the second push-back under the floor is
    /// damped and opens a new run, and the third is the fall. A delivery
    /// under the floor is no such reading: the run stays open.
    #[test]
    fn a_delivery_that_raises_the_floor_between_them_is_no_fall() {
        use super::super::rate_memory::damp;
        let now = t0();
        let s = |n: u64| now + Duration::from_secs(n);
        let mut p = floored(2_000_000, s(0));
        p3_at(&mut p, 1_000_000, 1_000_000, s(2));
        p.observe_delivered_in(1_600_000, Basis::AppLimited, true, s(3));
        assert_eq!(model_floor(&p, s(3)), 1_600_000, "raised");
        p3_at(&mut p, 1_000_000, 1_000_000, s(4));
        assert_eq!(model_floor(&p, s(4)), damp(1_600_000, 1_000_000));
        assert_eq!(p.model().fell(s(4)), None);
        p3_at(&mut p, 1_000_000, 1_000_000, s(6));
        assert_eq!(model_floor(&p, s(6)), 1_000_000, "the new run's fall");
        // A delivery under the floor between them neither raises nor closes.
        let mut q = floored(2_000_000, s(0));
        p3_at(&mut q, 1_000_000, 1_000_000, s(2));
        q.observe_delivered_in(1_400_000, Basis::Backlogged, true, s(3));
        assert_eq!(model_floor(&q, s(3)), 1_500_000, "no raise");
        p3_at(&mut q, 1_000_000, 1_000_000, s(4));
        assert_eq!(model_floor(&q, s(4)), 1_000_000, "the fall");
    }

    /// FR-148 D50, cell 4 — a push-back at or over the floor between the two
    /// closes the run: a window's at it, a window's over it (which raises it,
    /// `Pipe`'s law), a sample's at it and a sample's over it (which never
    /// raises it). The next push-back under the floor is damped and opens a
    /// new run, and the one after it is the fall.
    #[test]
    fn a_push_back_at_or_over_the_floor_between_them_is_no_fall() {
        use super::super::rate_memory::damp;
        let now = t0();
        let s = |n: u64| now + Duration::from_secs(n);
        let case = |what: &str, between: &dyn Fn(&mut Pipe, Instant), floor: u32| {
            let mut p = floored(2_000_000, s(0));
            p3_at(&mut p, 1_000_000, 1_000_000, s(2));
            between(&mut p, s(3));
            assert_eq!(model_floor(&p, s(3)), floor, "{what}");
            p3_at(&mut p, 1_000_000, 1_000_000, s(4));
            assert_eq!(model_floor(&p, s(4)), damp(floor, 1_000_000), "{what}");
            assert_eq!(p.model().fell(s(4)), None, "{what}");
            p3_at(&mut p, 1_000_000, 1_000_000, s(6));
            assert_eq!(
                model_floor(&p, s(6)),
                1_000_000,
                "{what}: the new run's fall"
            );
        };
        let window = |bps: u32| {
            move |p: &mut Pipe, t: Instant| {
                p.observe_capacity_in(bps, Basis::Backlogged, pushed_take(), t)
            }
        };
        let sample = |bps: u32| {
            move |p: &mut Pipe, t: Instant| {
                p.observe_sample_capacity(bps, PushedBy::P1, pushed_take(), t)
            }
        };
        case("a window's at the floor", &window(1_500_000), 1_500_000);
        case("a window's over it", &window(1_700_000), 1_700_000);
        case("a sample's at it", &sample(1_500_000), 1_500_000);
        case("a sample's over it", &sample(1_800_000), 1_500_000);
    }

    /// FR-148 D50, cell 4a — the level is the present, not the run. In the
    /// deep halving's order: the old path's last sample opens the run just
    /// under the floor, and the first sample after the fall, with the clock
    /// under the floor, restarts it at the more of that push-back and the
    /// clock, not at the run's first reading.
    #[test]
    fn the_level_is_the_present_not_the_run() {
        let now = t0();
        let mut p = floored(2_000_000, now);
        let (t1, t2) = (now + Duration::from_secs(2), now + Duration::from_secs(6));
        p3_at(&mut p, 1_960_000, 2_015_000, t1);
        assert_eq!(model_floor(&p, t1), 1_980_000, "damped: it opens the run");
        assert_eq!(p.model().fell(t1), None);
        p3_at(&mut p, 1_000_000, 1_010_000, t2);
        assert_eq!(model_floor(&p, t2), 1_010_000);
        let fall = Fall {
            from_bps: 1_980_000,
            to_bps: 1_010_000,
            age: Duration::ZERO,
        };
        assert_eq!(p.model().fell(t2), Some(fall));
    }

    /// FR-148 D50, cell 4b — the level is the more of the confirming
    /// push-back and the clock, both under the floor it replaces.
    #[test]
    fn the_level_is_the_more_of_the_push_back_and_the_clock() {
        let now = t0();
        let (t1, t2) = (now + Duration::from_secs(2), now + Duration::from_secs(4));
        for (bps, clock_bps) in [(1_000_000, 900_000), (900_000, 1_000_000)] {
            let mut p = floored(2_000_000, now);
            p3_at(&mut p, 1_000_000, 1_000_000, t1);
            p3_at(&mut p, bps, clock_bps, t2);
            assert_eq!(
                model_floor(&p, t2),
                1_000_000,
                "{bps} with the clock at {clock_bps}"
            );
            let fall = p.model().fell(t2).expect("a fall");
            assert!(fall.to_bps < fall.from_bps, "{fall:?}");
        }
    }

    /// FR-148 D50, cell 5 — the clock at or over the floor is no fall yet:
    /// the push-back is damped and the run stays open; the next one with
    /// the clock under the floor is the fall.
    #[test]
    fn the_clock_at_or_over_the_floor_is_no_fall_yet() {
        let now = t0();
        let s = |n: u64| now + Duration::from_secs(n);
        let mut p = floored(2_000_000, s(0));
        p3_at(&mut p, 1_000_000, 1_000_000, s(2));
        assert_eq!(model_floor(&p, s(2)), 1_500_000);
        p3_at(&mut p, 1_000_000, 1_600_000, s(4));
        assert_eq!(model_floor(&p, s(4)), 1_250_000, "the clock over the floor");
        p3_at(&mut p, 1_000_000, 1_250_000, s(6));
        assert_eq!(model_floor(&p, s(6)), 1_125_000, "the clock at the floor");
        assert_eq!(p.model().fell(s(6)), None);
        p3_at(&mut p, 1_000_000, 1_050_000, s(8));
        assert_eq!(
            model_floor(&p, s(8)),
            1_050_000,
            "the clock under it: the fall"
        );
    }

    /// FR-148 D50, cell 6 — a rate the opening stood in for is no clock, nor
    /// is a measured rate of 0, nor none: every push-back under the floor is
    /// damped, as built, and the model reads no fall.
    #[test]
    fn an_opening_rate_is_no_clock() {
        use super::super::rate_memory::damp;
        let now = t0();
        let s = |n: u64| now + Duration::from_secs(n);
        let none = PathRate {
            bps: None,
            measured: true,
            loss: 0,
        };
        for rate in [path(1_000_000, false, 0), path(0, true, 0), none] {
            let mut p = floored(2_000_000, s(0));
            let mut want = 2_000_000;
            for k in 1..=4 {
                p.note_path_rate(rate);
                p.observe_sample_capacity(1_000_000, PushedBy::P3, pushed_take(), s(2 * k));
                want = damp(want, 1_000_000);
                assert_eq!(model_floor(&p, s(2 * k)), want, "{rate:?}");
            }
            assert_eq!(want, 1_062_500);
            assert_eq!(p.model().fell(s(8)), None, "{rate:?}");
        }
    }

    /// FR-148 D50 — the shadow model's law before D50 (`8c7bb5f6d`),
    /// verbatim: the floor and the capacity under a delivery, a window's
    /// push-back and a delivery sample's. The kill switch's cell checks the
    /// model against it.
    #[derive(Default)]
    struct DampedLaw {
        floor: Option<(u32, Basis, bool, Instant, Option<PushedBy>)>,
        capacity: Option<(u32, Instant)>,
        capacity_by: Option<PushedBy>,
    }

    impl DampedLaw {
        fn delivered(&mut self, bps: u32, basis: Basis, now: Instant) {
            if bps > 0 && basis != Basis::Assumed {
                self.raise(bps, basis, false, now);
            }
        }

        fn remembered(&mut self, bps: u32, now: Instant) {
            if bps > 0 {
                self.raise(bps, Basis::AppLimited, true, now);
            }
        }

        fn sample_pushed_back(&mut self, bps: u32, by: PushedBy, now: Instant) {
            if bps == 0 {
                return;
            }
            self.capacity = Some((bps, now));
            self.capacity_by = Some(by);
            if let Some(f) = self.floor
                && bps < f.0
            {
                let damped = super::super::rate_memory::damp(f.0, bps);
                self.floor = Some((damped, Basis::Backlogged, false, now, Some(by)));
            }
        }

        fn pushed_back(&mut self, bps: u32, basis: Basis, now: Instant) {
            if bps == 0 {
                return;
            }
            match basis {
                Basis::Assumed => {}
                Basis::AppLimited => self.raise(bps, basis, false, now),
                Basis::Backlogged => {
                    self.capacity = Some((bps, now));
                    self.capacity_by = None;
                    let set = |bps| Some((bps, basis, false, now, None));
                    match self.floor {
                        Some(f) if bps < f.0 => {
                            self.floor = set(super::super::rate_memory::damp(f.0, bps));
                        }
                        Some(f) if bps <= f.0 => {}
                        _ => self.floor = set(bps),
                    }
                }
            }
        }

        fn raise(&mut self, bps: u32, basis: Basis, remembered: bool, now: Instant) {
            if self.floor.is_none_or(|f| bps > f.0) {
                self.floor = Some((bps, basis, remembered, now, None));
            }
        }

        fn floor(&self, now: Instant) -> Option<PathNumber> {
            self.floor
                .map(|(bps, basis, remembered, at, _)| PathNumber {
                    bps,
                    basis,
                    remembered,
                    age: now.saturating_duration_since(at),
                })
        }

        fn capacity(&self, now: Instant) -> Option<PathNumber> {
            self.capacity
                .filter(|(_, at)| now.duration_since(*at) <= CAPACITY_TTL)
                .map(|(bps, at)| {
                    PathNumber::new(bps, Basis::Backlogged, now.saturating_duration_since(at))
                })
        }

        /// The model's numbers, read at `now`, are this law's.
        fn check(&self, m: &PathModel, now: Instant) {
            assert_eq!(m.floor(now), self.floor(now), "floor");
            assert_eq!(m.floor_by(), self.floor.and_then(|f| f.4), "floor_by");
            assert_eq!(m.capacity(now), self.capacity(now), "capacity");
            let by = self.capacity(now).and(self.capacity_by);
            assert_eq!(m.capacity_by(now), by, "capacity_by");
            assert_eq!(m.fell(now), None, "no fall");
        }
    }

    /// FR-148 D50, cell 7 — the kill switch: with the in-flight budget off
    /// nothing notes a rate, so no clock reaches the shadow model, and its
    /// numbers are the law before D50 byte for byte ([`DampedLaw`]), over
    /// 100,000 deliveries, window push-backs of each class and delivery
    /// samples' push-backs, valid or not, over six decades with zeros and
    /// repeats, at steps from none to past [`CAPACITY_TTL`], read after each
    /// and at a random later moment. A seed opens it.
    #[test]
    fn with_the_budget_off_the_law_is_the_law_as_built() {
        let mut next = rng(0x6A09_E667_F3BC_C908);
        let mut t = t0();
        let (mut p, mut law) = (Pipe::default(), DampedLaw::default());
        p.remember(1_250_000, t);
        law.remembered(1_250_000, t);
        let mut last = 0;
        let mut under = 0u32;
        for _ in 0..100_000 {
            let bps = draw_bps(&mut next, last);
            last = bps;
            t += match next() % 16 {
                0 => CAPACITY_TTL + Duration::from_millis(next() % 5_000),
                1 => Duration::ZERO,
                _ => Duration::from_millis(next() % 3_000),
            };
            let basis = BASES[(next() % 3) as usize];
            if p.model().floor(t).is_some_and(|f| bps < f.bps) {
                under += 1;
            }
            match next() % 4 {
                0 => {
                    let valid = !next().is_multiple_of(4);
                    p.observe_delivered_in(bps, basis, valid, t);
                    if valid {
                        law.delivered(bps, basis, t);
                    }
                }
                1 => {
                    p.observe_capacity_in(bps, basis, pushed_take(), t);
                    law.pushed_back(bps, basis, t);
                }
                2 => {
                    p.observe_link_capacity_in(bps, basis, pushed_take(), t);
                    law.pushed_back(bps, basis, t);
                }
                _ => {
                    let by = if next().is_multiple_of(2) {
                        PushedBy::P1
                    } else {
                        PushedBy::P3
                    };
                    p.observe_sample_capacity(bps, by, pushed_take(), t);
                    law.sample_pushed_back(bps, by, t);
                }
            }
            law.check(p.model(), t);
            law.check(p.model(), t + Duration::from_millis(next() % 70_000));
        }
        assert!(under > 20_000, "{under} readings under the floor");
    }

    /// FR-148 D50, cell 8 — the pair memory keeps the damped law (D30):
    /// through `Pipe`, cell 2's readings restart the shadow model's floor
    /// and damp the memory's twice, for the memory is passed no clock.
    #[test]
    fn the_pair_memory_keeps_the_damped_law() {
        use super::super::rate_memory::damp;
        let now = t0();
        let mut p = floored(2_000_000, now);
        let (t1, t2) = (now + Duration::from_secs(2), now + Duration::from_secs(4));
        p3_at(&mut p, 1_000_000, 1_000_000, t1);
        p3_at(&mut p, 1_000_000, 1_020_000, t2);
        assert_eq!(model_floor(&p, t2), 1_020_000, "the model restarted");
        let memory = p.memory().closing(t2, 0).floor.map(|f| f.bps);
        let twice = damp(damp(2_000_000, 1_000_000), 1_000_000);
        assert_eq!(memory, Some(twice), "the memory damped twice");
        assert_eq!(twice, 1_250_000);
    }

    /// FR-148 D50, cell 9 — a run older than [`CAPACITY_TTL`], from its
    /// first push-back, starts again: the push-back is damped and opens a
    /// new run, and the next one is the fall. A push-back the clock does not
    /// confirm keeps the run's first instant; a run exactly
    /// [`CAPACITY_TTL`] old still reads the fall.
    #[test]
    fn a_run_older_than_the_capacity_ttl_starts_again() {
        use super::super::rate_memory::damp;
        let now = t0();
        let s = |n: u64| now + Duration::from_secs(n);
        let mut p = floored(2_000_000, s(0));
        p3_at(&mut p, 1_900_000, 1_950_000, s(2));
        p3_at(&mut p, 1_900_000, 1_950_000, s(30));
        assert_eq!(model_floor(&p, s(30)), 1_925_000, "the clock at the floor");
        let late = s(2) + CAPACITY_TTL + Duration::from_secs(1);
        p3_at(&mut p, 1_000_000, 1_000_000, late);
        assert_eq!(model_floor(&p, late), damp(1_925_000, 1_000_000));
        assert_eq!(p.model().fell(late), None, "the run began 61 s back");
        let next = late + Duration::from_secs(2);
        p3_at(&mut p, 1_000_000, 1_000_000, next);
        assert_eq!(model_floor(&p, next), 1_000_000, "the new run's fall");
        let mut q = floored(2_000_000, s(0));
        p3_at(&mut q, 1_000_000, 1_000_000, s(2));
        let edge = s(2) + CAPACITY_TTL;
        p3_at(&mut q, 1_000_000, 1_000_000, edge);
        assert_eq!(model_floor(&q, edge), 1_000_000, "a run 60 s old");
    }

    /// FR-148 D50, cell 10 — a window's push-back and a delivery sample's
    /// make one run, in either order: the link loop's push-back under the
    /// floor opens it and a P3 is the fall (`floor_by` p3); a P3 opens it
    /// and the link loop's push-back is the fall, backlogged, by a window
    /// (`floor_by` none).
    #[test]
    fn a_window_and_a_sample_make_one_run() {
        let now = t0();
        let s = |n: u64| now + Duration::from_secs(n);
        let mut p = floored(2_000_000, s(0));
        p.note_path_rate(clock(1_000_000));
        p.observe_link_capacity_in(1_200_000, Basis::Backlogged, pushed_take(), s(1));
        assert_eq!(model_floor(&p, s(1)), 1_600_000);
        p3_at(&mut p, 1_000_000, 1_010_000, s(2));
        assert_eq!(model_floor(&p, s(2)), 1_010_000);
        assert_eq!(p.model().floor_by(), Some(PushedBy::P3));
        let mut q = floored(2_000_000, s(0));
        p3_at(&mut q, 1_200_000, 1_000_000, s(1));
        assert_eq!(model_floor(&q, s(1)), 1_600_000);
        q.observe_link_capacity_in(1_000_000, Basis::Backlogged, pushed_take(), s(2));
        let floor = m_floor(&q, s(2));
        assert_eq!((floor.bps, floor.basis), (1_000_000, Basis::Backlogged));
        assert_eq!(q.model().floor_by(), None);
        assert!(q.model().fell(s(2)).is_some());
    }

    /// FR-148 D50, cell 11 — a fall never raises a floor nor makes one (D46
    /// item 10), over 100,000 random readings: deliveries, window push-backs,
    /// delivery samples' push-backs, and the budget's rate noted measured or
    /// not, over six decades with zeros and repeats, at steps from none to
    /// past [`CAPACITY_TTL`]. After a sample's push-back the floor is at or
    /// under the one before, and none where there was none. After a
    /// backlogged push-back under the floor it lies at or over the push-back
    /// and under the floor before: damped, or a fall that restarts it at
    /// the more of the push-back and a clock that read under the floor.
    #[test]
    fn a_fall_never_raises_a_floor_nor_makes_one() {
        use super::super::rate_memory::damp;
        let mut next = rng(0x3C6E_F372_FE94_F82B);
        let mut t = t0();
        let mut p = Pipe::default();
        let (mut last, mut clock_bps) = (0, None);
        let (mut falls, mut damped) = (0u32, 0u32);
        for _ in 0..100_000 {
            let bps = draw_bps(&mut next, last);
            last = bps;
            t += match next() % 16 {
                0 => CAPACITY_TTL + Duration::from_millis(next() % 5_000),
                1 => Duration::ZERO,
                _ => Duration::from_millis(next() % 3_000),
            };
            if next().is_multiple_of(3) {
                let rate = draw_bps(&mut next, bps);
                let measured = !next().is_multiple_of(4);
                p.note_path_rate(path(rate, measured, 0));
                clock_bps = (measured && rate > 0).then_some(rate);
            }
            let before = p.model().floor(t).map(|f| f.bps);
            let was = p.model().fell(t);
            let sample = match next() % 4 {
                0 => {
                    let valid = !next().is_multiple_of(4);
                    p.observe_delivered_in(bps, BASES[(next() % 3) as usize], valid, t);
                    continue;
                }
                1 => {
                    p.observe_capacity_in(bps, Basis::Backlogged, pushed_take(), t);
                    false
                }
                2 => {
                    p.observe_link_capacity_in(bps, Basis::Backlogged, pushed_take(), t);
                    false
                }
                _ => {
                    p.observe_sample_capacity(bps, PushedBy::P3, pushed_take(), t);
                    true
                }
            };
            let after = p.model().floor(t).map(|f| f.bps);
            let fell = p.model().fell(t) != was;
            let Some(f) = before else {
                let made = (!sample && bps > 0).then_some(bps);
                assert_eq!(after, made, "{bps} on no floor");
                continue;
            };
            if bps == 0 || bps >= f {
                let want = if sample { f } else { f.max(bps) };
                assert_eq!(after, Some(want), "{bps} at or over {f}");
                assert!(!fell);
                continue;
            }
            let a = after.expect("a floor");
            assert!(bps <= a && a < f, "{bps} under {f}: {a}");
            if fell {
                let clock = clock_bps.expect("a fall reads a clock");
                assert!(clock < f, "the clock {clock} under {f}");
                assert_eq!(a, bps.max(clock), "{bps} with the clock at {clock}");
                let fall = p.model().fell(t).expect("the fall");
                assert_eq!(
                    (fall.from_bps, fall.to_bps, fall.age),
                    (f, a, Duration::ZERO)
                );
                falls += 1;
            } else {
                assert_eq!(a, damp(f, bps));
                damped += 1;
            }
        }
        assert!(
            falls > 1_000 && damped > 1_000,
            "{falls} falls, {damped} damped steps"
        );
    }
}
