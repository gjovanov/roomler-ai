// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-65 P0 — the pump stall watch's VERDICT, as a pure function.
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
}

impl PassTiming {
    /// Time not spent waiting for input — the quantity the stall decision runs
    /// on. Saturating, so a capture delta that somehow exceeds the iteration
    /// (a clock that moved under us) reads 0 rather than wrapping to ~18
    /// billion milliseconds and reporting a stall on every idle pass.
    pub fn work_us(&self) -> u64 {
        self.iter_us.saturating_sub(self.capture_us)
    }

    /// Everything the pump explicitly timed.
    pub fn phases_us(&self) -> u64 {
        self.capture_us
            .saturating_add(self.scale_us)
            .saturating_add(self.encode_us)
            .saturating_add(self.send_us)
            .saturating_add(self.apply_us)
            .saturating_add(self.open_us)
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
        let named: [(&'static str, u64); 6] = [
            ("capture", self.capture_us),
            ("scale", self.scale_us),
            ("encode", self.encode_us),
            ("send", self.send_us),
            ("apply", self.apply_us),
            ("open", self.open_us),
        ];
        let (name, us) = named.into_iter().max_by_key(|(_, us)| *us)?;
        if us == 0 || us < self.other_us() {
            return None;
        }
        Some((name, us))
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
}
