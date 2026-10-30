// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-65 P0 — the pump stall watch: its VERDICT as a pure function, and the
//! per-pass bookkeeping that feeds it ([`PassWatch`]), as one type the pump
//! drives and [`cost`] times.
//!
//! # Why this is a module and not four lines in the pump
//!
//! The decision "was this iteration a stall, and where did its time go" is the
//! instrument the whole FR turns on, and it had two properties that made it
//! impossible to check:
//!
//! 1. It lived inline in `media_pump_ffmpeg_dc`, which is behind the
//!    `ffmpeg-encoder` feature — **not** a default feature, so
//!    `cargo test -p roomlerd --lib` compiled none of it. The rule that
//!    decides whether an operator ever sees a stall was, in the lane everyone
//!    runs, unreachable.
//! 2. Its two subtractions are each a judgement that was *paid for in the
//!    field* (see below), and both are easy to "simplify" into something that
//!    looks equivalent and is not.
//!
//! Extracting it costs one struct and buys a test that runs on every push.
//!
//! The same held one level up, and that is where both defects below lived: the
//! bookkeeping that FEEDS the verdict — the pass marks, the window counters,
//! the governor's stall count — stayed inline in the pump, invisible to every
//! test the verdict had. [`PassWatch`] is that bookkeeping, moved here.
//!
//! # The two rules, and what each cost to learn
//!
//! **`work_us = iter − capture`, and the stall decision runs on THAT.** Capture
//! is change-driven, so a quiet screen sleeps *inside* capture and `capture_us`
//! accumulates the wait. At the original 250 ms bar those passes were invisible;
//! at the 100 ms bar they became the MAJORITY of warnings — field 2026-09-03, a
//! direct session logged seven in 45 s, every one shaped
//! `iter_ms=111 capture_ms=101.9`: 9 ms of work and 102 ms of idling. A watch
//! that cries about idling is one people learn to ignore, which costs exactly
//! the signal it exists for. The waiting is genuine: every capture backend hands
//! the device work to a thread that owns the `!Send` capturer and returns a
//! future, so the loop is idling by design.
//!
//! ⚠️ **The cost of that rule, stated so nobody rediscovers it as a bug:** a
//! pathological capture — a wedged DXGI duplication, an EDR filter stalling the
//! desktop, a driver hang — **cannot trip this watch**, and `pump_stalls`
//! under-counts by construction. `capture_us` is still reported, so a reader can
//! see it, but nothing ALERTS on it. If capture needs an alarm it needs its own
//! threshold, not this one widened back.
//!
//! **`other_us = iter − Σphases`, computed rather than left to a reader.** An
//! overrun whose named phases all read ~0 is itself the finding: the time went
//! somewhere still untimed. Field 2026-09-03: four stalls on CORPLAP-1, one on
//! the first pass of every session, `iter_ms` 513–1006 while the named phases
//! summed to 33–52 ms — 0.46–0.96 s in no phase at all. That was the initial
//! encoder open, and it hid for months behind a breakdown that *looked*
//! complete; it was spotted by eye on the fourth occurrence. A breakdown that
//! does not sum to its total is the most useful number in the line, so it must
//! not depend on somebody noticing.
//!
//! # A window reset inside a pass (the 2026-10-30 census)
//!
//! The pump's heartbeat zeroes six of the accumulators every ~2 s — capture,
//! scale, encode, send, apply, open — and it runs at the TAIL of a pass, after
//! the mark that pass took at its top. The close at the next top then computed
//! `0 − mark` for each of them, which saturates to 0: the pass that held the
//! heartbeat lost every microsecond it had spent in those six phases. Its
//! capture wait counted as work, its encoder open counted as nothing, and the
//! whole pass landed in `other`.
//!
//! 🔑 It is not an edge case, because **a pass longer than the heartbeat
//! interval always holds a heartbeat** — the long pass is what makes it due. The
//! rule was blind to exactly the stalls worth finding. Read from the three
//! CORPLAP hosts' logs over 2026-10-17..30: of 339 STALL lines, 84 were emitted
//! within 5 ms of their session's heartbeat with all six reset phases at 0.0
//! and `other ≈ iter`, and they were the longest (1.5–2.6 s mean per host,
//! 4.1 s max). On CORPLAP-1, six of its seven came right after a heartbeat
//! carrying `open_ms` 1951–3529: first-pass encoder opens of 2–3.5 s, blamed on
//! nobody.
//!
//! [`PassWatch::rebase`] closes it where the reset happens: the open pass keeps
//! what it had accrued before the reset, and its mark moves to the values after
//! it, so the close sums both halves.
//!
//! # The governor's stall count never resets
//!
//! FR-79 hands the governor each pass's stall delta. It was `pump_stalls −
//! win_stalls_last`, and the heartbeat zeroed `pump_stalls` but not the mark, so
//! after a window that held a stall the next verdict read `1 − N`, which
//! saturates to 0: the governor's "was the agent's own loop free?" gate never
//! saw it. The first verdict after a reset is the heartbeat pass's own — the
//! long one. [`PassWatch::stalls_total`] is monotonic, so that subtraction
//! cannot lose a stall.
//!
//! # What the kill switch turns off
//!
//! `pump_stall_watch = false` (`ROOMLERD_PUMP_STALL_WATCH=0`): no pass marks, no
//! verdict, no window counters, and none of the timers only the watch reads —
//! [`PassWatch::start`] returns `None` without reading a clock. It leaves
//! capture/scale/encode/send (rc.88's heartbeat averages) and apply/open (on the
//! heartbeat too) running.
//!
//! ⚠️ Since FR-79 it is not a pure instrument switch: with the watch off the
//! governor's `stalled_passes` reads 0 on every window, so its `agent_stalled`
//! gate goes blind. A whole-session A/B through the switch therefore measures a
//! different controller as well as the instrument, which is why FR-65 AC1's A/B
//! is [`cost`] and not a pair of sessions.

use std::time::{Duration, Instant};

/// How many phases a pass is broken into: the length of
/// [`PassTiming::named`].
pub const PHASES: usize = 17;

/// One pump iteration's phase breakdown, in microseconds.
///
/// Every field is a DELTA for this pass (the pump keeps running accumulators
/// and subtracts the previous pass's marks), so the invariant a reader should
/// expect is `Σ phases ≤ iter_us` — and where it does not hold,
/// [`PassTiming::other_us`] is the gap that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PassTiming {
    /// Wall-clock for the whole iteration.
    pub iter_us: u64,
    /// Time inside capture. ⚠️ Mostly *waiting for a frame to change* on a
    /// quiet screen — see the module docs before treating it as work.
    pub capture_us: u64,
    pub scale_us: u64,
    pub encode_us: u64,
    pub send_us: u64,
    /// A rate/dims change applied to an encoder that ALREADY EXISTS.
    ///
    /// ⚠️ `apply_us == 0` on a session's first pass is not a contradiction and
    /// misled this investigation for weeks: the first pass has no encoder to
    /// change, it CONSTRUCTS one, and that is [`Self::open_us`].
    pub apply_us: u64,
    /// Opening (or rebuilding) the encoder.
    pub open_us: u64,
    /// FR-1 P5 cadence pacing — the deliberate sleep to the next slot when the
    /// encoder cannot hold `target_fps`.
    ///
    /// ⚠️ **Idle by design, like [`Self::capture_us`], and excluded from
    /// [`Self::work_us`] for the same reason.** At a paced 2 fps this is ~500 ms
    /// of a single pass. Counting it as work would re-create the warning storm
    /// the `work_us` rule removed for capture, one door along.
    pub pace_us: u64,
    /// Reading the peer connection's ICE/candidate stats — the relay-escape
    /// re-check and FR-33's LAN-capture reason.
    ///
    /// ⚠️ This is **not** free and it is not obviously on the video path: it
    /// walks the stats graph and takes locks the ICE agent and SCTP association
    /// also hold, so it is at its slowest exactly when the session is busiest.
    pub stats_us: u64,
    /// The control DataChannel: taking its lock and sending `rc:video-info`.
    pub ctrl_us: u64,
    /// Adopting a finished background encoder rebuild.
    pub swap_us: u64,
    /// The backpressure gate — the congestion decision plus its short sleep, on
    /// the pass where the pump skips production entirely.
    ///
    /// 🔑 This is the arm that made the gap hard to see: it `continue`s BEFORE
    /// capture, so such a pass reports `capture_us == 0` and `encode_us == 0`
    /// and every millisecond it spent landed in [`Self::other_us`].
    pub gate_us: u64,
    /// Waiting for the video DataChannel's mutex after an encode.
    ///
    /// ⚠️ The send task takes the same mutex in an `if let` scrutinee, and Rust
    /// 2024 keeps a scrutinee's temporaries alive through the body — so it
    /// holds it for a frame's WHOLE chunked, flow-controlled send, and the pump
    /// waits out that send here (checked on the pinned toolchain, 2026-10-30:
    /// inside the body `try_lock` fails, after a `let` statement it succeeds).
    /// Named because the census's untimed passes are shaped like it — capture
    /// tiny, encode normal, the rest in `other`. The fix waits for the number,
    /// per this FR's first rule.
    pub dc_lock_us: u64,
    /// Encoder commands other than the encode, an apply and the open: keyframe
    /// requests and the dims replacement's spec. Since FR-70 M1 each is a round
    /// trip to the encoder's own thread, served in order.
    pub enc_cmd_us: u64,
    /// Dropping an encoder.
    ///
    /// ⚠️ `EncoderThread`'s `Drop` JOINS its thread — synchronously, on the
    /// runtime worker polling the pump. That is blocking work on a shared
    /// runtime, this FR's own subject, on every rebuild and error path; until
    /// 2026-10-30 nothing timed it.
    pub close_us: u64,
    /// Rebuilding the capturer: a relay flip's reopen at the new profile, or
    /// the capture-error arm's backoff and reopen.
    pub recapture_us: u64,
    /// An empty capture poll's pacing: rc.99's 2 ms sleep, or the yield while
    /// the display is held.
    ///
    /// ⚠️ Counted as WORK, unlike [`Self::pace_us`]. At ~2 ms it can never
    /// reach the bar on its own, so a pass this dominates is one where the pump
    /// asked for 2 ms and got the rest back late — the runtime or its timer did
    /// not schedule it. That makes it a canary for P2's question, for free.
    pub poll_us: u64,
    /// The heartbeat itself: it takes the video-DC mutex (the wait
    /// [`Self::dc_lock_us`] names), round-trips the encoder thread for its
    /// stats, and formats a ~120-field line.
    pub heartbeat_us: u64,
}

/// The pump's running phase accumulators, snapshotted at a pass boundary.
///
/// The pump keeps plain `u64` counters and marks them at the start of each
/// pass; [`Self::delta`] turns "then" and "now" into one [`PassTiming`]. Having
/// the subtraction in ONE place is the point — it used to be eleven inline
/// `saturating_sub`s in the warn macro, where a mismatched pair is invisible.
/// The fields mean what [`PassTiming`]'s of the same name do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhaseAccum {
    pub capture_us: u64,
    pub scale_us: u64,
    pub encode_us: u64,
    pub send_us: u64,
    pub apply_us: u64,
    pub open_us: u64,
    pub pace_us: u64,
    pub stats_us: u64,
    pub ctrl_us: u64,
    pub swap_us: u64,
    pub gate_us: u64,
    pub dc_lock_us: u64,
    pub enc_cmd_us: u64,
    pub close_us: u64,
    pub recapture_us: u64,
    pub poll_us: u64,
    pub heartbeat_us: u64,
}

impl PhaseAccum {
    /// `self − mark`, field by field.
    ///
    /// Saturating throughout: an accumulator can only grow, so a negative delta
    /// means the mark was taken after the reading, which is a bug rather than a
    /// number to propagate. ⚠️ A window reset is the one place it is not a bug,
    /// and saturating there is what lost the long passes — see
    /// [`PassWatch::rebase`].
    pub fn since(self, mark: Self) -> Self {
        Self {
            capture_us: self.capture_us.saturating_sub(mark.capture_us),
            scale_us: self.scale_us.saturating_sub(mark.scale_us),
            encode_us: self.encode_us.saturating_sub(mark.encode_us),
            send_us: self.send_us.saturating_sub(mark.send_us),
            apply_us: self.apply_us.saturating_sub(mark.apply_us),
            open_us: self.open_us.saturating_sub(mark.open_us),
            pace_us: self.pace_us.saturating_sub(mark.pace_us),
            stats_us: self.stats_us.saturating_sub(mark.stats_us),
            ctrl_us: self.ctrl_us.saturating_sub(mark.ctrl_us),
            swap_us: self.swap_us.saturating_sub(mark.swap_us),
            gate_us: self.gate_us.saturating_sub(mark.gate_us),
            dc_lock_us: self.dc_lock_us.saturating_sub(mark.dc_lock_us),
            enc_cmd_us: self.enc_cmd_us.saturating_sub(mark.enc_cmd_us),
            close_us: self.close_us.saturating_sub(mark.close_us),
            recapture_us: self.recapture_us.saturating_sub(mark.recapture_us),
            poll_us: self.poll_us.saturating_sub(mark.poll_us),
            heartbeat_us: self.heartbeat_us.saturating_sub(mark.heartbeat_us),
        }
    }

    /// `self + other`, field by field, saturating.
    pub fn plus(self, other: Self) -> Self {
        Self {
            capture_us: self.capture_us.saturating_add(other.capture_us),
            scale_us: self.scale_us.saturating_add(other.scale_us),
            encode_us: self.encode_us.saturating_add(other.encode_us),
            send_us: self.send_us.saturating_add(other.send_us),
            apply_us: self.apply_us.saturating_add(other.apply_us),
            open_us: self.open_us.saturating_add(other.open_us),
            pace_us: self.pace_us.saturating_add(other.pace_us),
            stats_us: self.stats_us.saturating_add(other.stats_us),
            ctrl_us: self.ctrl_us.saturating_add(other.ctrl_us),
            swap_us: self.swap_us.saturating_add(other.swap_us),
            gate_us: self.gate_us.saturating_add(other.gate_us),
            dc_lock_us: self.dc_lock_us.saturating_add(other.dc_lock_us),
            enc_cmd_us: self.enc_cmd_us.saturating_add(other.enc_cmd_us),
            close_us: self.close_us.saturating_add(other.close_us),
            recapture_us: self.recapture_us.saturating_add(other.recapture_us),
            poll_us: self.poll_us.saturating_add(other.poll_us),
            heartbeat_us: self.heartbeat_us.saturating_add(other.heartbeat_us),
        }
    }

    /// These phases as one pass of `iter_us`.
    pub fn with_iter(self, iter_us: u64) -> PassTiming {
        PassTiming {
            iter_us,
            capture_us: self.capture_us,
            scale_us: self.scale_us,
            encode_us: self.encode_us,
            send_us: self.send_us,
            apply_us: self.apply_us,
            open_us: self.open_us,
            pace_us: self.pace_us,
            stats_us: self.stats_us,
            ctrl_us: self.ctrl_us,
            swap_us: self.swap_us,
            gate_us: self.gate_us,
            dc_lock_us: self.dc_lock_us,
            enc_cmd_us: self.enc_cmd_us,
            close_us: self.close_us,
            recapture_us: self.recapture_us,
            poll_us: self.poll_us,
            heartbeat_us: self.heartbeat_us,
        }
    }

    /// This pass's timing: `self` (now) minus `mark` (the pass start).
    pub fn delta(self, mark: Self, iter_us: u64) -> PassTiming {
        self.since(mark).with_iter(iter_us)
    }
}

impl PassTiming {
    /// Time not spent waiting for input — the quantity the stall decision runs
    /// on. Saturating, so a capture delta that somehow exceeds the iteration
    /// (a clock that moved under us) reads 0 rather than wrapping to ~18
    /// billion milliseconds and reporting a stall on every idle pass.
    ///
    /// ⚠️ **Two deliberate idles are excluded, not one.** `capture_us` is
    /// waiting for the screen to change; `pace_us` is waiting for the next
    /// cadence slot because the encoder cannot hold `target_fps`. Both are the
    /// loop idling on purpose, and at a paced 2 fps the second is ~500 ms — so
    /// counting it would report a "stall" on every paced pass, which is the
    /// same mistake the 100 ms bar exposed for capture. Every phase named
    /// since counts as work, so naming one never changes a verdict: it only
    /// moves time out of [`Self::other_us`].
    pub fn work_us(&self) -> u64 {
        self.iter_us
            .saturating_sub(self.capture_us)
            .saturating_sub(self.pace_us)
    }

    /// Every phase, by the name the STALL line and [`Self::dominant_phase`]
    /// use.
    pub fn named(&self) -> [(&'static str, u64); PHASES] {
        [
            ("capture", self.capture_us),
            ("scale", self.scale_us),
            ("encode", self.encode_us),
            ("send", self.send_us),
            ("apply", self.apply_us),
            ("open", self.open_us),
            ("pace", self.pace_us),
            ("stats", self.stats_us),
            ("ctrl", self.ctrl_us),
            ("swap", self.swap_us),
            ("gate", self.gate_us),
            ("dc_lock", self.dc_lock_us),
            ("enc_cmd", self.enc_cmd_us),
            ("close", self.close_us),
            ("recapture", self.recapture_us),
            ("poll", self.poll_us),
            ("heartbeat", self.heartbeat_us),
        ]
    }

    /// Everything the pump explicitly timed.
    pub fn phases_us(&self) -> u64 {
        self.named()
            .iter()
            .fold(0u64, |sum, (_, us)| sum.saturating_add(*us))
    }

    /// `iter − Σphases`: time that fell into no named phase. **A non-trivial
    /// value here is a finding, not noise** — it is where the encoder open hid.
    pub fn other_us(&self) -> u64 {
        self.iter_us.saturating_sub(self.phases_us())
    }

    /// Does this pass warrant a stall warning at `warn_us`?
    ///
    /// ⚠️ Compares [`Self::work_us`], never `iter_us`. Swapping them back would
    /// re-create the idle-capture warning storm the 100 ms bar exposed.
    pub fn is_stall(&self, warn_us: u64) -> bool {
        self.work_us() >= warn_us
    }

    /// The single phase that dominated this pass, and its microseconds —
    /// `None` when nothing named accounts for it, which points at
    /// [`Self::other_us`].
    ///
    /// Reporting only; the pump logs the full breakdown. It exists so a test
    /// can assert ATTRIBUTION rather than merely that something was flagged
    /// (FR-65 AC2), because "a stall was caught" and "a stall was caught and
    /// blamed on the right phase" are different claims.
    pub fn dominant_phase(&self) -> Option<(&'static str, u64)> {
        let (name, us) = self.named().into_iter().max_by_key(|(_, us)| *us)?;
        if us == 0 || us < self.other_us() {
            return None;
        }
        Some((name, us))
    }
}

/// FR-65 P0 — the pump's per-pass stall watch: the pass marks, the verdict's
/// rate-limited log decision, the window counters the heartbeat publishes, and
/// the stall total the governor reads.
///
/// The pump calls [`Self::turn_with`] at the TOP of every pass, which closes
/// the previous pass and opens this one, so every `continue` path counts
/// without an RAII guard fighting the borrow checker. [`Self::rebase`] goes
/// around the heartbeat's reset, and [`Self::start`]/[`Self::stop`] time the
/// phases only the watch reads.
#[derive(Debug, Clone)]
pub struct PassWatch {
    enabled: bool,
    warn_us: u64,
    /// The open pass: when it started, and the accumulators at that instant.
    open: Option<(Instant, PhaseAccum)>,
    /// What a window reset took from the open pass ([`Self::rebase`]).
    carried: PhaseAccum,
    window_iter_us_max: u64,
    window_stalls: u32,
    window_passes: u32,
    stalls_total: u64,
    last_log: Option<Instant>,
}

impl PassWatch {
    /// At most one STALL line per this interval. ⚠️ A stall storm that floods
    /// its own log is a second performance problem.
    pub const LOG_EVERY: Duration = Duration::from_secs(2);

    /// `enabled` is the kill switch (`pump_stall_watch`); `warn` is the bar a
    /// pass's [`PassTiming::work_us`] must reach to count as a stall.
    pub fn new(enabled: bool, warn: Duration) -> Self {
        Self {
            enabled,
            warn_us: u64::try_from(warn.as_micros()).unwrap_or(u64::MAX),
            open: None,
            carried: PhaseAccum::default(),
            window_iter_us_max: 0,
            window_stalls: 0,
            window_passes: 0,
            stalls_total: 0,
            last_log: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Start timing a phase only the watch reads: `None`, and no clock read,
    /// when the watch is off.
    #[inline]
    pub fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    /// Add the time since `start` to `acc`; a `None` start adds nothing.
    #[inline]
    pub fn stop(start: Option<Instant>, acc: &mut u64) {
        if let Some(t) = start {
            *acc = acc.saturating_add(t.elapsed().as_micros() as u64);
        }
    }

    /// The pass boundary: close the previous pass and open the next, reading
    /// the clock once. `snapshot` builds the accumulators and is never called
    /// when the watch is off. Returns the closed pass when it was a stall AND
    /// a STALL line is due ([`Self::LOG_EVERY`]); every stall is counted either
    /// way.
    #[inline]
    pub fn turn_with(&mut self, snapshot: impl FnOnce() -> PhaseAccum) -> Option<PassTiming> {
        if !self.enabled {
            return None;
        }
        let acc = snapshot();
        self.turn_at(acc, Instant::now())
    }

    /// [`Self::turn_with`] at a given instant — the deterministic core.
    ///
    /// ⚠️ One clock read serves both the close and the open, so consecutive
    /// passes tile time exactly: a STALL line's own formatting lands in the
    /// next pass (as `other`) instead of in no pass at all.
    pub fn turn_at(&mut self, acc: PhaseAccum, now: Instant) -> Option<PassTiming> {
        let due = match self.open.take() {
            Some((started, mark)) => {
                let iter_us = u64::try_from(now.saturating_duration_since(started).as_micros())
                    .unwrap_or(u64::MAX);
                let pass = acc
                    .since(mark)
                    .plus(std::mem::take(&mut self.carried))
                    .with_iter(iter_us);
                self.window_passes = self.window_passes.saturating_add(1);
                self.window_iter_us_max = self.window_iter_us_max.max(iter_us);
                if pass.is_stall(self.warn_us) {
                    self.window_stalls = self.window_stalls.saturating_add(1);
                    self.stalls_total = self.stalls_total.saturating_add(1);
                    let log_due = self
                        .last_log
                        .is_none_or(|t| now.saturating_duration_since(t) >= Self::LOG_EVERY);
                    if log_due {
                        self.last_log = Some(now);
                        Some(pass)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        self.open = Some((now, acc));
        due
    }

    /// The heartbeat is about to zero (some of) the accumulators the open pass
    /// was marked against: `before` is them before the reset, `after` after it.
    ///
    /// 🔑 Without this the close computes `0 − mark` for every reset field,
    /// which saturates to 0, so the pass holding the heartbeat — and every pass
    /// longer than the heartbeat interval holds one — loses its capture,
    /// encode and open time to `other` (see the module docs: 84 of 339 field
    /// STALL lines). With it the pass keeps what it had accrued and its mark
    /// moves past the reset. Works for any subset of fields, so the heartbeat
    /// can reset what it likes as long as both snapshots are taken from the
    /// same counters.
    pub fn rebase(&mut self, before: PhaseAccum, after: PhaseAccum) {
        if let Some((_, mark)) = self.open.as_mut() {
            self.carried = self.carried.plus(before.since(*mark));
            *mark = after;
        }
    }

    /// The worst pass closed in this window (the heartbeat's `iter_ms_max`).
    pub fn window_iter_us_max(&self) -> u64 {
        self.window_iter_us_max
    }

    /// Stalls closed in this window (the heartbeat's `pump_stalls`).
    pub fn window_stalls(&self) -> u32 {
        self.window_stalls
    }

    /// Passes closed in this window (the heartbeat's `passes`). The watch's
    /// cost per second is this rate times its cost per pass — FR-65 AC1 reads
    /// the rate from here rather than inferring it from the frame counters.
    pub fn window_passes(&self) -> u32 {
        self.window_passes
    }

    /// The heartbeat's window reset: its maxes and counts describe that window
    /// alone. [`Self::stalls_total`] is NOT reset.
    pub fn reset_window(&mut self) {
        self.window_iter_us_max = 0;
        self.window_stalls = 0;
        self.window_passes = 0;
    }

    /// Every stall since the pump started. Monotonic — the governor's per-pass
    /// delta reads this, never the window count (see the module docs).
    pub fn stalls_total(&self) -> u64 {
        self.stalls_total
    }
}

/// FR-65 AC1 — what the watch costs, measured on the host that runs it:
/// `roomlerd stall-watch-cost`.
///
/// An interleaved A/B of the pump's per-pass watch work through the REAL
/// [`PassWatch`], once enabled and once disabled. Rounds alternate which arm
/// goes first, so a drift in clock speed or temperature lands on both arms;
/// everything else in a pass is identical in both, so the per-pass difference
/// is the watch.
///
/// Four cells. `steady` is what every pass enters while frames flow: the loop
/// top's close/open, the swap block, the gate and the wait for the video
/// DataChannel (`media_pump_ffmpeg_dc`). `worst` enters EVERY phase only the
/// watch reads, on every pass. Each runs `warm`, with nothing between the
/// watch's operations (the precise figure), and `cold`, with a walk over a
/// buffer larger than an L1 between every one of them — the pump runs capture
/// and an encode between them, and a tokio worker may move it to another core,
/// so its state is rarely hot.
///
/// The bar is 1 % of a frame at the pump's HIGHEST frame rate
/// (`frame_rate::CHOICES`, 144 fps today: 69.4 µs), not the 60 fps the spec
/// first assumed — a 144 fps session has the smallest frame to spend 1 % of.
///
/// ⚠️ **The positive control is what makes a pass mean anything.** A harness
/// that cannot resolve the quantity reports a small number for every cost. So
/// the run repeats `steady-warm` with a calibrated spin INJECTED into the ON
/// arm at three times the bar, and at least half of it must be measured back —
/// which puts it over the bar. If it is not, the verdict is INVALID and nothing
/// else in the report is evidence. The spin is calibrated immediately before
/// that cell, so a clock that ramped during the run does not shrink it.
pub mod cost {
    use super::{PassWatch, PhaseAccum};
    use std::hint::black_box;
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// The verb on the command line.
    pub const VERB: &str = "stall-watch-cost";

    /// The pump's highest frame rate.
    pub fn max_fps() -> u32 {
        crate::encode::frame_rate::CHOICES
            .iter()
            .copied()
            .max()
            .unwrap_or(60)
    }

    /// One frame at `fps`, in nanoseconds.
    pub fn budget_ns(fps: u32) -> f64 {
        1e9 / f64::from(fps.max(1))
    }

    /// AC1's bar: 1 % of a frame at [`max_fps`].
    pub fn bar_ns() -> f64 {
        budget_ns(max_fps()) * 0.01
    }

    /// Which phase timers a pass enters.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Profile {
        /// The loop top plus swap, gate and dc_lock: every pass while frames
        /// flow.
        Steady,
        /// The loop top plus every phase only the watch reads.
        Worst,
    }

    impl Profile {
        /// Clock reads the ON arm makes per pass: one at the loop top, two per
        /// phase timer.
        pub fn clock_reads(self) -> u32 {
            match self {
                Self::Steady => 1 + 2 * 3,
                Self::Worst => 1 + 2 * 11,
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Opts {
        pub rounds: u32,
        pub warm_passes: u32,
        pub cold_passes: u32,
        /// Bytes walked between consecutive watch operations in a cold cell.
        pub cold_segment_bytes: usize,
        /// The positive control's injected cost; `None` is three times the
        /// bar.
        pub inject_ns: Option<u64>,
        pub json: bool,
    }

    impl Default for Opts {
        fn default() -> Self {
            Self {
                rounds: 31,
                warm_passes: 20_000,
                cold_passes: 200,
                cold_segment_bytes: 1 << 20,
                inject_ns: None,
                json: false,
            }
        }
    }

    impl Opts {
        /// A short run (~1 s) for a smoke check; the default is ~15 s.
        pub fn quick() -> Self {
            Self {
                rounds: 9,
                warm_passes: 2_000,
                cold_passes: 30,
                ..Self::default()
            }
        }
    }

    /// One cell's result, per pass, in nanoseconds.
    #[derive(Debug, Clone)]
    pub struct CellResult {
        pub name: String,
        pub passes: u32,
        pub rounds: u32,
        pub clock_reads_per_pass: u32,
        /// Median per-pass time with the watch OFF (the body alone).
        pub off_ns: f64,
        /// Median per-pass time with the watch ON.
        pub on_ns: f64,
        /// The per-round ON − OFF difference: the watch.
        pub diff_p10_ns: f64,
        pub diff_median_ns: f64,
        pub diff_p90_ns: f64,
    }

    #[derive(Debug, Clone)]
    pub struct Report {
        /// One `Instant::now()` on this host, for context.
        pub clock_read_ns: f64,
        pub max_fps: u32,
        pub bar_ns: f64,
        pub cells: Vec<CellResult>,
        pub inject_ns: u64,
        /// `steady-warm` with [`Self::inject_ns`] added to the ON arm.
        pub control: CellResult,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Verdict {
        /// The control resolved the injected cost and every cell's p90 is
        /// under the bar.
        Pass,
        /// A cell's p90 is at or over the bar.
        Fail,
        /// Less than half the injected cost came back: the harness could not
        /// see a cost of that size, so its small numbers prove nothing.
        Invalid,
    }

    impl Report {
        /// Did the control come back? At least half the injected cost must be
        /// measured; at the default (3x the bar) that is over the bar.
        pub fn control_resolved(&self) -> bool {
            self.control.diff_median_ns >= 0.5 * self.inject_ns as f64
        }

        pub fn verdict(&self) -> Verdict {
            if !self.control_resolved() {
                return Verdict::Invalid;
            }
            if self.cells.iter().all(|c| c.diff_p90_ns < self.bar_ns) {
                Verdict::Pass
            } else {
                Verdict::Fail
            }
        }

        /// The worst cell's p90 — the number the verdict is read from.
        pub fn worst_p90_ns(&self) -> f64 {
            self.cells.iter().map(|c| c.diff_p90_ns).fold(0.0, f64::max)
        }
    }

    /// Bytes walked between watch operations in a cold cell. The buffer is
    /// written first, so its pages are real (a zeroed `Vec` can be backed by a
    /// single shared page and pollute nothing).
    struct Walker {
        buf: Vec<u8>,
        pos: usize,
        seg: usize,
    }

    impl Walker {
        fn new(seg: usize) -> Self {
            let len = seg.saturating_mul(8).max(64);
            let buf = (0..len)
                .map(|i| (i.wrapping_mul(31) & 0xff) as u8)
                .collect();
            Self { buf, pos: 0, seg }
        }

        #[inline(never)]
        fn walk(&mut self) {
            if self.seg == 0 {
                return;
            }
            let len = self.buf.len();
            let mut sum = 0u64;
            let mut i = self.pos;
            let end = self.pos + self.seg;
            while i < end {
                sum = sum.wrapping_add(u64::from(self.buf[i % len]));
                i += 64;
            }
            black_box(sum);
            self.pos = end % len;
        }
    }

    /// One pass of the pump's watch work, in the pump's order. Never inlined,
    /// and the watch arrives through `black_box`, so the ON and OFF arms run
    /// the same machine code with the switch read at run time, as in the pump.
    #[inline(never)]
    fn pass(w: &mut PassWatch, a: &mut PhaseAccum, profile: Profile, walker: &mut Walker) {
        black_box(w.turn_with(|| *a));
        walker.walk();
        macro_rules! timed {
            ($($field:ident),*) => {$(
                let t = w.start();
                PassWatch::stop(t, &mut a.$field);
                walker.walk();
            )*};
        }
        timed!(swap_us, gate_us, dc_lock_us);
        if profile == Profile::Worst {
            timed!(
                pace_us,
                stats_us,
                ctrl_us,
                enc_cmd_us,
                close_us,
                recapture_us,
                poll_us,
                heartbeat_us
            );
        }
    }

    /// The same pass for a COLD cell: the walk evicts the watch's state and
    /// code before each operation, and each operation is bracketed by its own
    /// clock pair; the return is the nanoseconds inside the brackets.
    ///
    /// ⚠️ Why not the whole pass, as the warm cells do: a cold pass is ~100 us
    /// of walking whose time varies by microseconds, so a difference of two
    /// whole-pass blocks bounds the watch only at that noise (tens of us on
    /// NEO16, 2026-10-30) — useless once multiplied by a pass rate. The
    /// brackets leave the walk outside the measurement and keep the eviction
    /// inside it. The bracket's own clock reads are in both arms and cancel;
    /// the first one does warm the clock's path for the watch's own read, so a
    /// cold miss on THAT is not in the figure.
    #[inline(never)]
    fn pass_timed(
        w: &mut PassWatch,
        a: &mut PhaseAccum,
        profile: Profile,
        walker: &mut Walker,
    ) -> u64 {
        let mut ns = 0u64;
        walker.walk();
        let t = Instant::now();
        black_box(w.turn_with(|| *a));
        ns += t.elapsed().as_nanos() as u64;
        macro_rules! timed {
            ($($field:ident),*) => {$(
                walker.walk();
                let t = Instant::now();
                let s = w.start();
                PassWatch::stop(s, &mut a.$field);
                ns += t.elapsed().as_nanos() as u64;
            )*};
        }
        timed!(swap_us, gate_us, dc_lock_us);
        if profile == Profile::Worst {
            timed!(
                pace_us,
                stats_us,
                ctrl_us,
                enc_cmd_us,
                close_us,
                recapture_us,
                poll_us,
                heartbeat_us
            );
        }
        ns
    }

    /// A busy loop of `iters` steps — the positive control's injected cost.
    #[inline(never)]
    fn spin(iters: u64) {
        let mut x = 0u64;
        for i in 0..iters {
            x = black_box(x.wrapping_add(i));
        }
        black_box(x);
    }

    fn spin_ns_per_iter() -> f64 {
        const N: u64 = 20_000_000;
        spin(N / 10);
        let t = Instant::now();
        spin(N);
        t.elapsed().as_nanos() as f64 / N as f64
    }

    fn clock_read_ns() -> f64 {
        const N: u32 = 1_000_000;
        let t = Instant::now();
        for _ in 0..N {
            black_box(Instant::now());
        }
        t.elapsed().as_nanos() as f64 / f64::from(N)
    }

    struct Arm {
        watch: PassWatch,
        acc: PhaseAccum,
        spin_iters: u64,
    }

    impl Arm {
        fn new(enabled: bool, spin_iters: u64) -> Self {
            Self {
                watch: black_box(PassWatch::new(enabled, Duration::from_millis(100))),
                acc: PhaseAccum::default(),
                spin_iters,
            }
        }

        /// `passes` passes; the mean nanoseconds per pass. A cold walker
        /// (`seg > 0`) switches to [`pass_timed`]: the mean nanoseconds inside
        /// the watch's bracketed operations per pass.
        fn block(&mut self, profile: Profile, walker: &mut Walker, passes: u32) -> f64 {
            if walker.seg > 0 {
                let mut ns = 0u64;
                for _ in 0..passes {
                    ns += pass_timed(&mut self.watch, &mut self.acc, profile, walker);
                }
                return ns as f64 / f64::from(passes.max(1));
            }
            let t = Instant::now();
            for _ in 0..passes {
                pass(&mut self.watch, &mut self.acc, profile, walker);
                if self.spin_iters > 0 {
                    spin(self.spin_iters);
                }
            }
            t.elapsed().as_nanos() as f64 / f64::from(passes.max(1))
        }
    }

    /// Nearest-rank percentile of `v` (sorted in place).
    fn pct(v: &mut [f64], p: f64) -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(f64::total_cmp);
        let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
        v[rank.clamp(1, v.len()) - 1]
    }

    fn cell(
        name: &str,
        profile: Profile,
        passes: u32,
        rounds: u32,
        segment: usize,
        inject_iters: u64,
    ) -> CellResult {
        let mut on = Arm::new(true, inject_iters);
        let mut off = Arm::new(false, 0);
        let mut walker = Walker::new(segment);
        // One discarded block each: page faults, caches, frequency.
        on.block(profile, &mut walker, passes);
        off.block(profile, &mut walker, passes);
        let (mut ons, mut offs, mut diffs) = (Vec::new(), Vec::new(), Vec::new());
        for r in 0..rounds.max(1) {
            let (a, b) = if r % 2 == 0 {
                let a = on.block(profile, &mut walker, passes);
                (a, off.block(profile, &mut walker, passes))
            } else {
                let b = off.block(profile, &mut walker, passes);
                (on.block(profile, &mut walker, passes), b)
            };
            ons.push(a);
            offs.push(b);
            diffs.push(a - b);
        }
        CellResult {
            name: name.to_string(),
            passes,
            rounds: rounds.max(1),
            clock_reads_per_pass: profile.clock_reads(),
            off_ns: pct(&mut offs, 50.0),
            on_ns: pct(&mut ons, 50.0),
            diff_p10_ns: pct(&mut diffs, 10.0),
            diff_median_ns: pct(&mut diffs, 50.0),
            diff_p90_ns: pct(&mut diffs, 90.0),
        }
    }

    /// The whole A/B. Single-threaded, on the calling thread.
    pub fn run(opts: &Opts) -> Report {
        let bar = bar_ns();
        let inject_ns = opts.inject_ns.unwrap_or((3.0 * bar).round() as u64);
        let seg = opts.cold_segment_bytes;
        let cells = vec![
            cell(
                "steady-warm",
                Profile::Steady,
                opts.warm_passes,
                opts.rounds,
                0,
                0,
            ),
            cell(
                "steady-cold",
                Profile::Steady,
                opts.cold_passes,
                opts.rounds,
                seg,
                0,
            ),
            cell(
                "worst-warm",
                Profile::Worst,
                opts.warm_passes,
                opts.rounds,
                0,
                0,
            ),
            cell(
                "worst-cold",
                Profile::Worst,
                opts.cold_passes,
                opts.rounds,
                seg,
                0,
            ),
        ];
        // Calibrated HERE, after the cells: a core that ramped its clock during
        // them would otherwise run the spin faster than it was calibrated at.
        let inject_iters = (inject_ns as f64 / spin_ns_per_iter().max(0.01)).round() as u64;
        // The control injects ~208 us a pass, so it runs fewer passes than the
        // warm cells it copies or it would take minutes.
        let control_passes = (opts.warm_passes / 100).max(50);
        let control = cell(
            "control",
            Profile::Steady,
            control_passes,
            opts.rounds,
            0,
            inject_iters,
        );
        Report {
            clock_read_ns: clock_read_ns(),
            max_fps: max_fps(),
            bar_ns: bar,
            cells,
            inject_ns,
            control,
        }
    }

    fn pct_of_frame(ns: f64, fps: u32) -> f64 {
        100.0 * ns / budget_ns(fps)
    }

    /// Plain ASCII on purpose: this is read through `roomler exec`, whose
    /// output reaches a Windows console code page or a lossy UTF-8 pipe.
    pub fn render_text(r: &Report) -> String {
        let mut s = String::new();
        let fps = r.max_fps;
        s.push_str(&format!(
            "roomlerd {VERB} - FR-65 AC1: the pump stall watch's own cost, A/B'd on this host\n\
             one clock read (Instant::now): {:.1} ns\n\
             bar: 1 % of a {fps} fps frame ({:.1} us) = {:.2} us per pass\n\n",
            r.clock_read_ns,
            budget_ns(fps) / 1000.0,
            r.bar_ns / 1000.0
        ));
        s.push_str(
            "(warm cells: whole passes, ON vs OFF. cold cells: only the watch's operations, each\n \
             bracketed after the eviction walk, ON vs OFF - the walk is outside the brackets.)\n\n",
        );
        s.push_str(
            "cell          passes x rounds  reads/pass  OFF ns/pass  ON ns/pass  watch ns/pass p10 / median / p90  p90 % of frame\n",
        );
        for c in r.cells.iter().chain(std::iter::once(&r.control)) {
            s.push_str(&format!(
                "{:<12}  {:>6} x {:<6}  {:>10}  {:>11.1}  {:>10.1}  {:>9.1} / {:>9.1} / {:>9.1}  {:>13.4}\n",
                c.name,
                c.passes,
                c.rounds,
                c.clock_reads_per_pass,
                c.off_ns,
                c.on_ns,
                c.diff_p10_ns,
                c.diff_median_ns,
                c.diff_p90_ns,
                pct_of_frame(c.diff_p90_ns, fps),
            ));
        }
        s.push_str(&format!(
            "\ncontrol: {:.1} us injected into the ON arm, {:.1} us measured back ({:.2}x) - {}\n",
            r.inject_ns as f64 / 1000.0,
            r.control.diff_median_ns / 1000.0,
            r.control.diff_median_ns / (r.inject_ns as f64).max(1.0),
            if r.control_resolved() {
                "resolved: the harness sees a cost this size"
            } else {
                "NOT resolved: the harness cannot see a cost this size"
            },
        ));
        let worst = r.worst_p90_ns();
        // The precise figure is a warm cell's median; a cold cell's p90 is a
        // BOUND, mostly its own noise (a ~100 us body whose time varies by
        // microseconds), so the headroom is read from the former.
        let precise = r
            .cells
            .iter()
            .find(|c| c.name == "steady-warm")
            .map_or(worst, |c| c.diff_median_ns);
        let verdict = match r.verdict() {
            Verdict::Pass => format!(
                "PASS - every cell's p90 is under the bar; the worst ({:.1} ns, {:.4} % of a \
                 {fps} fps frame) is an upper bound set by that cell's noise. Per pass the watch \
                 costs {:.1} ns (steady-warm median), {:.4} % of a frame: the pump could make \
                 {:.0} passes per frame before it reached 1 %",
                worst,
                pct_of_frame(worst, fps),
                precise,
                pct_of_frame(precise, fps),
                r.bar_ns / precise.max(1e-9)
            ),
            Verdict::Fail => format!(
                "FAIL - the worst cell's p90 is {:.1} ns, over the {:.1} ns bar",
                worst, r.bar_ns
            ),
            Verdict::Invalid => {
                "INVALID - the control was not resolved, so no cell here is evidence".to_string()
            }
        };
        s.push_str(&format!("verdict: {verdict}\n"));
        // The pump makes several passes per frame when it polls an idle
        // screen, so the per-second view is the one AC1's "1 % of the frame
        // budget" really means. A reference rate, not a field rate: the
        // heartbeat's `passes` is this host's.
        const REF_PASSES_PER_SEC: f64 = 1000.0;
        s.push_str(&format!(
            "per second at {REF_PASSES_PER_SEC:.0} passes/s (a reference rate - read the heartbeat's \
             `passes` for this host's): {:.1} us of watch ({:.4} % of each second), {:.1} us at \
             the worst cell's p90 ({:.4} %)\n",
            precise * REF_PASSES_PER_SEC / 1000.0,
            precise * REF_PASSES_PER_SEC / 1e7,
            worst * REF_PASSES_PER_SEC / 1000.0,
            worst * REF_PASSES_PER_SEC / 1e7,
        ));
        s
    }

    pub fn render_json(r: &Report) -> serde_json::Value {
        let cell = |c: &CellResult| {
            serde_json::json!({
                "name": c.name,
                "passes": c.passes,
                "rounds": c.rounds,
                "clock_reads_per_pass": c.clock_reads_per_pass,
                "off_ns": c.off_ns,
                "on_ns": c.on_ns,
                "diff_p10_ns": c.diff_p10_ns,
                "diff_median_ns": c.diff_median_ns,
                "diff_p90_ns": c.diff_p90_ns,
            })
        };
        serde_json::json!({
            "verdict": format!("{:?}", r.verdict()),
            "control_resolved": r.control_resolved(),
            "clock_read_ns": r.clock_read_ns,
            "max_fps": r.max_fps,
            "bar_ns": r.bar_ns,
            "worst_p90_ns": r.worst_p90_ns(),
            "steady_warm_median_ns": r
                .cells
                .iter()
                .find(|c| c.name == "steady-warm")
                .map(|c| c.diff_median_ns),
            "inject_ns": r.inject_ns,
            "cells": r.cells.iter().map(cell).collect::<Vec<_>>(),
            "control": cell(&r.control),
        })
    }

    pub const USAGE: &str =
        "usage: roomlerd stall-watch-cost [--json] [--quick] [--rounds N] [--inject-ns N]";

    /// `Some` when this command line is `roomlerd stall-watch-cost …` (read off
    /// argv before clap, like `update-helper --capabilities`); `Err` names a
    /// bad argument.
    pub fn cli_opts<I, S>(args: I) -> Option<Result<Opts, String>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut it = args.into_iter().skip(1);
        if it.next()?.as_ref() != VERB {
            return None;
        }
        let mut opts = Opts::default();
        let mut quick = false;
        let mut rounds = None;
        while let Some(arg) = it.next() {
            let arg = arg.as_ref().to_string_lossy().into_owned();
            let mut number = |flag: &str| -> Result<u64, String> {
                it.next()
                    .and_then(|v| v.as_ref().to_string_lossy().trim().parse::<u64>().ok())
                    .ok_or_else(|| format!("{flag} needs a number"))
            };
            match arg.as_str() {
                "--json" => opts.json = true,
                "--quick" => quick = true,
                "--rounds" => match number("--rounds") {
                    Ok(n) => rounds = Some(u32::try_from(n.max(1)).unwrap_or(u32::MAX)),
                    Err(e) => return Some(Err(e)),
                },
                "--inject-ns" => match number("--inject-ns") {
                    Ok(n) => opts.inject_ns = Some(n),
                    Err(e) => return Some(Err(e)),
                },
                other => return Some(Err(format!("unknown argument {other:?}"))),
            }
        }
        if quick {
            opts = Opts {
                json: opts.json,
                inject_ns: opts.inject_ns,
                ..Opts::quick()
            };
        }
        if let Some(n) = rounds {
            opts.rounds = n;
        }
        Some(Ok(opts))
    }

    /// Run, print, and give the exit code: 0 PASS, 3 FAIL, 4 INVALID. Output
    /// write errors are ignored — a closed stdout must not turn a measurement
    /// into a panic.
    pub fn cli_main(opts: &Opts) -> i32 {
        let report = run(opts);
        let text = if opts.json {
            render_json(&report).to_string()
        } else {
            render_text(&report)
        };
        let _ = writeln!(std::io::stdout().lock(), "{text}");
        match report.verdict() {
            Verdict::Pass => 0,
            Verdict::Fail => 3,
            Verdict::Invalid => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact field shape from 2026-09-03: 9 ms of work, 102 ms of idling.
    /// Seven of these in 45 s on one direct session were the majority of the
    /// warnings at the 100 ms bar.
    #[test]
    fn an_idle_capture_wait_is_not_a_stall() {
        let pass = PassTiming {
            iter_us: 111_000,
            capture_us: 101_900,
            encode_us: 9_000,
            ..Default::default()
        };
        assert_eq!(pass.work_us(), 9_100);
        assert!(
            !pass.is_stall(100_000),
            "a pass that idled 102 ms inside capture must not warn"
        );
    }

    /// The same pass under the pre-0.4.60 rule, which judged on `iter_us`.
    /// Kept as a test so the regression has a name: this is what the storm was.
    #[test]
    fn the_same_pass_would_have_warned_under_the_old_rule() {
        let pass = PassTiming {
            iter_us: 111_000,
            capture_us: 101_900,
            encode_us: 9_000,
            ..Default::default()
        };
        assert!(
            pass.iter_us >= 100_000,
            "the old iter-based rule fired here — that is the storm this replaced"
        );
    }

    /// CORPLAP-1, 0.4.60, session 6a9a0602: the first pass of a session, whose
    /// 815 ms is the encoder open.
    #[test]
    fn an_encoder_open_is_a_stall_and_is_attributed_to_open() {
        let pass = PassTiming {
            iter_us: 815_677,
            capture_us: 9_973,
            encode_us: 16_104,
            open_us: 789_024,
            apply_us: 0,
            ..Default::default()
        };
        assert!(pass.is_stall(100_000));
        assert_eq!(pass.work_us(), 805_704);
        assert_eq!(pass.dominant_phase(), Some(("open", 789_024)));
        // The breakdown SUMS: 815 677 − (9 973 + 16 104 + 789 024) = 576 µs.
        assert!(
            pass.other_us() < 1_000,
            "a fully-attributed pass left {} µs unexplained",
            pass.other_us()
        );
    }

    /// The pre-#1279 shape, before `open_us` existed: the same pass with the
    /// open untimed. The remainder must surface as `other`, and `dominant_phase`
    /// must REFUSE to blame a named phase for it.
    #[test]
    fn an_untimed_phase_surfaces_as_other_and_blames_nobody() {
        let pass = PassTiming {
            iter_us: 773_062,
            capture_us: 8_837,
            encode_us: 15_821,
            ..Default::default()
        };
        assert!(pass.is_stall(100_000));
        assert_eq!(pass.other_us(), 773_062 - 8_837 - 15_821);
        assert!(
            pass.other_us() > 700_000,
            "the untimed remainder is the finding and must be visible"
        );
        assert_eq!(
            pass.dominant_phase(),
            None,
            "no named phase may be blamed when the remainder dwarfs them all"
        );
    }

    /// ⚠️ Guards the saturating subtraction. `capture_us > iter_us` should be
    /// impossible, but a wrapping subtraction here would compute ~1.8e19 µs and
    /// report a stall on EVERY pass — a log flood from an arithmetic edge.
    #[test]
    fn a_capture_delta_larger_than_the_iteration_cannot_wrap() {
        let pass = PassTiming {
            iter_us: 5_000,
            capture_us: 9_000,
            ..Default::default()
        };
        assert_eq!(pass.work_us(), 0);
        assert!(!pass.is_stall(100_000));
        assert_eq!(pass.other_us(), 0);
    }

    /// CORPLAP-2, 2026-09-04: `iter 662 / capture 0.0 / encode 0.0`, the whole
    /// pass in `other`. Zero capture AND zero encode is the signature of the
    /// backpressure gate, which `continue`s BEFORE capture — so every
    /// millisecond it spent was invisible.
    ///
    /// ⚠️ 2026-10-30: it is ALSO the signature of a pass that held the
    /// heartbeat, whose reset zeroed capture and encode under it (see
    /// `a_window_reset_mid_pass_keeps_the_pass_whole`). Both readings fit
    /// these two lines; the census says the second one was common.
    #[test]
    fn a_gate_skip_is_attributed_to_gate_not_to_other() {
        let pass = PassTiming {
            iter_us: 662_022,
            gate_us: 661_500,
            ..Default::default()
        };
        assert_eq!(pass.dominant_phase(), Some(("gate", 661_500)));
        assert!(
            pass.other_us() < 1_000,
            "the gate skip left {} us unexplained",
            pass.other_us()
        );
        assert!(pass.is_stall(100_000), "a 662 ms gate pass is real work");
    }

    /// A paced pass is the loop idling on purpose, exactly like a capture wait.
    /// At 2 fps the sleep is ~500 ms and it must NOT warn.
    #[test]
    fn a_cadence_sleep_is_idle_and_does_not_warn() {
        let pass = PassTiming {
            iter_us: 505_000,
            capture_us: 4_000,
            encode_us: 11_000,
            pace_us: 490_000,
            ..Default::default()
        };
        assert_eq!(pass.work_us(), 11_000, "pace is excluded from work");
        assert!(
            !pass.is_stall(100_000),
            "a 490 ms deliberate cadence sleep must not report a stall"
        );
        assert!(pass.other_us() < 1_000);
    }

    /// The stats read is the opposite case: not idle, and slowest exactly when
    /// the session is busiest, so it must both count as work and be blamed.
    #[test]
    fn an_ice_stats_read_counts_as_work_and_is_named() {
        let pass = PassTiming {
            iter_us: 171_500,
            capture_us: 300,
            encode_us: 13_600,
            stats_us: 157_000,
            ..Default::default()
        };
        assert!(pass.is_stall(100_000));
        assert_eq!(pass.dominant_phase(), Some(("stats", 157_000)));
        assert!(pass.other_us() < 1_000);
    }

    /// Every field of a full literal, each with its own value — a test that
    /// stops compiling when a phase is added and not listed here, which is what
    /// makes the next three tests cover every phase.
    fn every_phase() -> PhaseAccum {
        PhaseAccum {
            capture_us: 1,
            scale_us: 2,
            encode_us: 4,
            send_us: 8,
            apply_us: 16,
            open_us: 32,
            pace_us: 64,
            stats_us: 128,
            ctrl_us: 256,
            swap_us: 512,
            gate_us: 1_024,
            dc_lock_us: 2_048,
            enc_cmd_us: 4_096,
            close_us: 8_192,
            recapture_us: 16_384,
            poll_us: 32_768,
            heartbeat_us: 65_536,
        }
    }

    /// `PhaseAccum::delta` is the one place the subtractions live, and every
    /// phase is named exactly once: each value is a distinct power of two, so
    /// a phase left out of `named`, or listed twice, changes the sum.
    #[test]
    fn every_phase_is_subtracted_added_and_named_once() {
        let mark = every_phase();
        let now = every_phase().plus(every_phase());
        let d = now.delta(mark, 1_000_000);
        assert_eq!(d, every_phase().with_iter(1_000_000));
        assert_eq!(d.phases_us(), (1u64 << PHASES) - 1);
        let names: std::collections::BTreeSet<&str> = d.named().iter().map(|(n, _)| *n).collect();
        assert_eq!(names.len(), PHASES, "a phase name is used twice");
        // ⚠️ A counter that went BACKWARDS is a bug, not a number to propagate.
        let back = PhaseAccum::default().delta(mark, 500);
        assert_eq!(back.phases_us(), 0);
    }

    #[test]
    fn the_threshold_is_inclusive() {
        let pass = PassTiming {
            iter_us: 100_000,
            ..Default::default()
        };
        assert!(pass.is_stall(100_000), "a pass exactly at the bar warns");
        let just_under = PassTiming {
            iter_us: 99_999,
            ..Default::default()
        };
        assert!(!just_under.is_stall(100_000));
    }

    /// A slow ENCODE must still be caught and blamed on encode — the watch is
    /// not only about the open, and a rule tuned until nothing fires would be
    /// worse than no rule.
    #[test]
    fn a_slow_encode_is_caught_and_attributed_to_encode() {
        let pass = PassTiming {
            iter_us: 260_000,
            capture_us: 4_000,
            encode_us: 250_000,
            ..Default::default()
        };
        assert!(pass.is_stall(100_000));
        assert_eq!(pass.dominant_phase(), Some(("encode", 250_000)));
    }

    /// Naming a phase never changes a verdict: every named phase but capture
    /// and pace counts as work, exactly as `other` did.
    #[test]
    fn naming_a_phase_moves_time_out_of_other_and_changes_no_verdict() {
        let unnamed = PassTiming {
            iter_us: 511_070,
            capture_us: 60,
            ..Default::default()
        };
        let named = PassTiming {
            poll_us: 510_950,
            ..unnamed
        };
        assert_eq!(unnamed.work_us(), named.work_us());
        assert_eq!(unnamed.is_stall(100_000), named.is_stall(100_000));
        assert_eq!(unnamed.dominant_phase(), None);
        assert_eq!(named.dominant_phase(), Some(("poll", 510_950)));
    }

    // ---- PassWatch ------------------------------------------------------

    const WARN: Duration = Duration::from_millis(100);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The pump's accumulators, as the pump keeps them: running counters the
    /// heartbeat zeroes six of.
    #[derive(Default, Clone, Copy)]
    struct Pump {
        acc: PhaseAccum,
    }

    impl Pump {
        /// The heartbeat's reset, exactly the fields `media_pump_ffmpeg_dc`
        /// zeroes.
        fn heartbeat_reset(&mut self) {
            self.acc.capture_us = 0;
            self.acc.scale_us = 0;
            self.acc.encode_us = 0;
            self.acc.send_us = 0;
            self.acc.apply_us = 0;
            self.acc.open_us = 0;
        }
    }

    /// The first turn has nothing to close: it only opens a pass.
    #[test]
    fn the_first_turn_only_opens_a_pass() {
        let mut w = PassWatch::new(true, WARN);
        assert_eq!(w.turn_at(PhaseAccum::default(), Instant::now()), None);
        assert_eq!(w.window_iter_us_max(), 0);
        assert_eq!(w.window_stalls(), 0);
    }

    /// CORPLAP-1, 2026-10-29 14:20:48: the first pass of a session, which held
    /// a 3529 ms `hevc_qsv` open, a 19.8 ms capture and a 35.2 ms encode — and,
    /// being 3.6 s long, the session's first heartbeat. The field line read
    /// `capture 0.0 / encode 0.0 / open 0.0 / other 3586 / dominant None`.
    #[test]
    fn a_window_reset_mid_pass_keeps_the_pass_whole() {
        let t0 = Instant::now();
        let mut w = PassWatch::new(true, WARN);
        let mut p = Pump::default();
        w.turn_at(p.acc, t0);
        p.acc.capture_us += 19_825;
        p.acc.open_us += 3_528_944;
        p.acc.encode_us += 35_157;
        // The heartbeat, at the tail of this same pass.
        let before = p.acc;
        p.heartbeat_reset();
        w.rebase(before, p.acc);
        let pass = w
            .turn_at(p.acc, t0 + Duration::from_micros(3_586_045))
            .expect("a 3.6 s pass is a stall, and the first one logs");
        assert_eq!(pass.open_us, 3_528_944);
        assert_eq!(pass.capture_us, 19_825);
        assert_eq!(pass.encode_us, 35_157);
        assert_eq!(pass.dominant_phase(), Some(("open", 3_528_944)));
        assert!(pass.other_us() < 3_000, "other = {}", pass.other_us());
        assert_eq!(pass.work_us(), 3_586_045 - 19_825);
    }

    /// The negative control for the test above: the same pass WITHOUT the
    /// rebase reproduces the field line exactly. A test that would pass with
    /// the fix reverted proves nothing; this one shows what the fix changed.
    #[test]
    fn without_the_rebase_the_reset_reproduces_the_field_line() {
        let t0 = Instant::now();
        let mut w = PassWatch::new(true, WARN);
        let mut p = Pump::default();
        w.turn_at(p.acc, t0);
        p.acc.capture_us += 19_825;
        p.acc.open_us += 3_528_944;
        p.acc.encode_us += 35_157;
        p.heartbeat_reset(); // no rebase: the pump before 2026-10-30
        let pass = w
            .turn_at(p.acc, t0 + Duration::from_micros(3_586_045))
            .expect("still a stall");
        assert_eq!(
            (pass.capture_us, pass.encode_us, pass.open_us),
            (0, 0, 0),
            "the reset saturated every reset phase to 0"
        );
        assert_eq!(pass.dominant_phase(), None);
        assert_eq!(pass.other_us(), 3_586_045);
    }

    /// The same loss on a pass whose mark holds a running window (not the
    /// first pass): `0 − mark` saturates the same way.
    #[test]
    fn without_the_rebase_a_mid_session_pass_loses_its_phases_too() {
        let t0 = Instant::now();
        let mut w = PassWatch::new(true, WARN);
        let mut p = Pump::default();
        p.acc.capture_us = 40_000;
        p.acc.encode_us = 90_000;
        w.turn_at(p.acc, t0);
        p.acc.encode_us += 1_200_000;
        p.heartbeat_reset();
        let pass = w.turn_at(p.acc, t0 + ms(1_210)).expect("a stall");
        assert_eq!(pass.encode_us, 0, "1.2 s of encode, lost to `other`");
        assert_eq!(pass.dominant_phase(), None);
    }

    /// The reset also turned a heartbeat pass's capture WAIT into work: a quiet
    /// screen's ~105 ms wait in the pass that held the heartbeat became a
    /// stall. With the rebase it is an idle pass again.
    #[test]
    fn a_window_reset_does_not_turn_a_capture_wait_into_a_stall() {
        let t0 = Instant::now();
        for rebase in [true, false] {
            let mut w = PassWatch::new(true, WARN);
            let mut p = Pump::default();
            p.acc.capture_us = 500_000; // the window so far
            w.turn_at(p.acc, t0);
            p.acc.capture_us += 105_000;
            p.acc.encode_us += 9_000;
            let before = p.acc;
            p.heartbeat_reset();
            if rebase {
                w.rebase(before, p.acc);
            }
            let verdict = w.turn_at(p.acc, t0 + ms(116));
            if rebase {
                assert_eq!(verdict, None, "a capture wait is not a stall");
                assert_eq!(w.stalls_total(), 0);
            } else {
                assert!(verdict.is_some(), "the old pump called the wait work");
                assert_eq!(w.stalls_total(), 1);
            }
        }
    }

    /// The governor's per-pass feed, as the pump runs it: a counter's delta
    /// against its value at the last feed.
    #[derive(Default)]
    struct GovernorFeed {
        fed_total: u64,
        mark_total: u64,
        fed_window: u64,
        mark_window: u32,
    }

    impl GovernorFeed {
        fn feed(&mut self, w: &PassWatch) {
            self.fed_total += w.stalls_total().saturating_sub(self.mark_total);
            self.mark_total = w.stalls_total();
            // What the pump fed before 2026-10-30.
            self.fed_window += u64::from(w.window_stalls().saturating_sub(self.mark_window));
            self.mark_window = w.window_stalls();
        }
    }

    /// Over a window reset the monotonic total delivers every stall to the
    /// governor; the window count hides the first one after the reset — which
    /// is the heartbeat pass's own verdict, the long one.
    #[test]
    fn the_governor_sees_every_stall_across_a_window_reset() {
        let t0 = Instant::now();
        let mut w = PassWatch::new(true, WARN);
        let acc = PhaseAccum::default();
        let mut now = t0;
        w.turn_at(acc, now);
        let mut g = GovernorFeed::default();
        // Window 1: two stalls, each fed to the governor in its own pass.
        for _ in 0..2 {
            now += ms(150);
            w.turn_at(acc, now);
            g.feed(&w);
        }
        // The heartbeat resets the window; the next pass's verdict is a stall.
        w.reset_window();
        now += ms(150);
        w.turn_at(acc, now);
        g.feed(&w);
        assert_eq!(w.stalls_total(), 3);
        assert_eq!(g.fed_total, 3, "the monotonic total delivers every stall");
        assert_eq!(
            g.fed_window, 2,
            "the window count read 1 − 2 → 0 after the reset: one stall hidden"
        );
    }

    /// Every stall is counted; the LINE is rate-limited to one per
    /// `LOG_EVERY`.
    #[test]
    fn every_stall_counts_but_only_one_line_per_two_seconds() {
        let t0 = Instant::now();
        let mut w = PassWatch::new(true, WARN);
        let acc = PhaseAccum::default();
        w.turn_at(acc, t0);
        let mut lines = 0;
        let mut now = t0;
        for _ in 0..10 {
            now += ms(150); // ten 150 ms stalls in 1.5 s
            if w.turn_at(acc, now).is_some() {
                lines += 1;
            }
        }
        assert_eq!(w.window_stalls(), 10);
        assert_eq!(w.stalls_total(), 10);
        assert_eq!(lines, 1, "one line per two seconds");
        assert_eq!(w.window_iter_us_max(), 150_000);
        now += ms(2_100);
        assert!(w.turn_at(acc, now).is_some(), "due again after 2 s");
    }

    /// The window's max and count reset with the heartbeat; the total does not.
    #[test]
    fn the_window_resets_and_the_total_does_not() {
        let t0 = Instant::now();
        let mut w = PassWatch::new(true, WARN);
        let acc = PhaseAccum::default();
        w.turn_at(acc, t0);
        w.turn_at(acc, t0 + ms(300));
        w.turn_at(acc, t0 + ms(310));
        assert_eq!(w.window_iter_us_max(), 300_000);
        assert_eq!(w.window_passes(), 2, "the opening turn closes nothing");
        w.reset_window();
        assert_eq!(
            (w.window_iter_us_max(), w.window_stalls(), w.window_passes()),
            (0, 0, 0)
        );
        assert_eq!(w.stalls_total(), 1);
        w.turn_at(acc, t0 + ms(320));
        assert_eq!(w.window_iter_us_max(), 10_000);
        assert_eq!(w.window_passes(), 1);
    }

    /// The kill switch: no clock, no snapshot, no counters.
    #[test]
    fn a_disabled_watch_reads_no_clock_and_counts_nothing() {
        let mut w = PassWatch::new(false, WARN);
        assert!(w.start().is_none(), "a phase timer reads no clock when off");
        let mut acc = 7u64;
        PassWatch::stop(w.start(), &mut acc);
        assert_eq!(acc, 7);
        let mut snapshots = 0;
        for _ in 0..3 {
            let r = w.turn_with(|| {
                snapshots += 1;
                PhaseAccum::default()
            });
            assert_eq!(r, None);
        }
        assert_eq!(snapshots, 0, "the snapshot is never built when off");
        w.rebase(every_phase(), PhaseAccum::default());
        assert_eq!(
            (w.window_iter_us_max(), w.window_stalls(), w.stalls_total()),
            (0, 0, 0)
        );
    }

    /// A rebase with no open pass (before the first turn) is a no-op, and two
    /// resets in one pass still sum.
    #[test]
    fn rebase_is_safe_before_the_first_turn_and_composes() {
        let t0 = Instant::now();
        let mut w = PassWatch::new(true, WARN);
        w.rebase(every_phase(), PhaseAccum::default());
        let mut p = Pump::default();
        w.turn_at(p.acc, t0);
        for _ in 0..2 {
            p.acc.encode_us += 60_000;
            let before = p.acc;
            p.heartbeat_reset();
            w.rebase(before, p.acc);
        }
        p.acc.encode_us += 1_000;
        let pass = w.turn_at(p.acc, t0 + ms(122)).expect("122 ms of encode");
        assert_eq!(pass.encode_us, 121_000);
    }

    // ---- the cost harness ---------------------------------------------------

    /// ⚠️ Pins the number the docs quote (69.4 us at 144 fps). If
    /// `frame_rate::CHOICES` grows, the bar tightens with it — update
    /// `docs/fr/FR-65-…` and this test together.
    #[test]
    fn the_bar_is_one_percent_of_the_fastest_frame() {
        assert_eq!(cost::max_fps(), 144);
        assert!((cost::bar_ns() - 69_444.44).abs() < 0.01);
    }

    #[test]
    fn the_verdict_needs_the_control_to_resolve() {
        let cell = |p90: f64, median: f64| cost::CellResult {
            name: "x".into(),
            passes: 1,
            rounds: 1,
            clock_reads_per_pass: 7,
            off_ns: 0.0,
            on_ns: 0.0,
            diff_p10_ns: 0.0,
            diff_median_ns: median,
            diff_p90_ns: p90,
        };
        let report = |cells: Vec<cost::CellResult>, control: cost::CellResult| cost::Report {
            clock_read_ns: 20.0,
            max_fps: 144,
            bar_ns: 69_444.0,
            cells,
            inject_ns: 208_333,
            control,
        };
        let pass = report(vec![cell(300.0, 200.0)], cell(210_000.0, 208_000.0));
        assert_eq!(pass.verdict(), cost::Verdict::Pass);
        let fail = report(vec![cell(70_000.0, 200.0)], cell(210_000.0, 208_000.0));
        assert_eq!(fail.verdict(), cost::Verdict::Fail);
        let blind = report(vec![cell(300.0, 200.0)], cell(1_000.0, 900.0));
        assert_eq!(
            blind.verdict(),
            cost::Verdict::Invalid,
            "a harness that cannot see an injected cost proves nothing"
        );
        // Half the injected cost is the line: 104 166 of 208 333 ns.
        let half = report(vec![cell(300.0, 200.0)], cell(110_000.0, 104_200.0));
        assert_eq!(half.verdict(), cost::Verdict::Pass);
        let under_half = report(vec![cell(300.0, 200.0)], cell(110_000.0, 104_100.0));
        assert_eq!(under_half.verdict(), cost::Verdict::Invalid);
    }

    #[test]
    fn the_command_line_is_ours_only_for_its_verb() {
        assert!(cost::cli_opts(["roomlerd", "run"]).is_none());
        assert!(cost::cli_opts(["roomlerd"]).is_none());
        let o = cost::cli_opts(["roomlerd", "stall-watch-cost", "--json", "--rounds", "5"])
            .expect("our verb")
            .expect("valid");
        assert!(o.json);
        assert_eq!(o.rounds, 5);
        let q = cost::cli_opts([
            "roomlerd",
            "stall-watch-cost",
            "--quick",
            "--inject-ns",
            "1000",
        ])
        .expect("our verb")
        .expect("valid");
        assert_eq!((q.rounds, q.inject_ns), (9, Some(1_000)));
        assert!(
            cost::cli_opts(["roomlerd", "stall-watch-cost", "--rounds"])
                .expect("our verb")
                .is_err()
        );
        assert!(
            cost::cli_opts(["roomlerd", "stall-watch-cost", "--bogus"])
                .expect("our verb")
                .is_err()
        );
    }

    /// The harness end to end, small. Asserts only what holds on any machine:
    /// the injected control comes out over the bar (it is ~2× the bar, so a
    /// harness that reports it under the bar cannot see costs this size), and
    /// the report renders. The real reading is `roomlerd stall-watch-cost` on
    /// the host in question — FR-65 AC1 — never a CI runner.
    #[test]
    fn the_harness_resolves_its_control() {
        let opts = cost::Opts {
            rounds: 5,
            warm_passes: 2_000,
            cold_passes: 4,
            cold_segment_bytes: 64 << 10,
            inject_ns: None,
            json: false,
        };
        let report = cost::run(&opts);
        assert!(
            report.control_resolved(),
            "control: {} ns injected, {} ns measured",
            report.inject_ns,
            report.control.diff_median_ns
        );
        assert!(report.control.diff_median_ns > report.bar_ns);
        assert_ne!(report.verdict(), cost::Verdict::Invalid);
        assert!(cost::render_text(&report).contains("verdict:"));
        assert!(cost::render_json(&report)["cells"].is_array());
    }
}
