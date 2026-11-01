// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3272 — the pump's side of [`ScreenCapture::next_frame`]'s contract: an
//! empty comes after a bounded wait, and nothing loops on one at full speed.
//!
//! The RTP pump is the first to run on this. It took the SYSTEM-context backend
//! straight, with no sleep of its own, and on a still screen it asked about
//! 90,000 times a second: most of a core, and a log flood that crowded the
//! heartbeats out of the uploader. FR-98 C7 moves the DataChannel pumps here too
//! and retires the opt-in.
//!
//! [`ScreenCapture::next_frame`]: super::ScreenCapture::next_frame

use std::time::{Duration, Instant};

/// The shortest an empty may take. It is what the DXGI arm rests on when it
/// cannot wait in the acquire (a zero-copy host), what a backend that returns
/// at once is held to, and the pump-side sleep the FFmpeg DC pump has had since
/// rc.99: about 450 asks a second, against 90,000.
pub const EMPTY_FLOOR: Duration = Duration::from_millis(2);

/// An empty faster than this did no waiting at all. Under a millisecond is a
/// round trip and a return, not a timer.
const INSTANT_BELOW: Duration = Duration::from_millis(1);

/// How many instant empties in a row, with no frame between them, make a spin.
///
/// One alone is not. A pull backend paces each ask from the last one, so after
/// an encode longer than a frame period its next ask is already due and comes
/// straight back if nothing changed (X11's damage tracker, DRM). The ask after
/// that one sleeps.
const SPINNING_AFTER: u32 = 3;

/// How often a still screen logs that it is still, in time. It used to count
/// empties, which made the line as fast as the capture was: 362 lines a second
/// on a spinning host, 96.7 % of what its uploader sent.
pub const IDLE_LINE_EVERY: Duration = Duration::from_secs(5);

/// `ROOMLERD_RTP_CAPTURE_WAIT`, default on. Off, the RTP pump does not opt in
/// and arms no guard: the capture spins as it did before #3272. Read once per
/// pump (FR-70: no per-frame reads). Env only, by the operator's decision: it
/// is a kill switch for the first release, not a setting (`docs/encoders.md`).
pub fn rtp_capture_wait_enabled() -> bool {
    tunnel_core::env::flag("RTP_CAPTURE_WAIT", true)
}

/// What is left of [`EMPTY_FLOOR`] after an empty that took `took`.
pub fn floor_remaining(took: Duration) -> Duration {
    EMPTY_FLOOR.saturating_sub(took)
}

/// Is the idle line due, `last` being when it was last written (or when the
/// pump started)?
pub fn idle_line_due(last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last) >= IDLE_LINE_EVERY
}

/// Watches a pump's empties, so a backend that breaks the contract costs a
/// warning and 2 ms each rather than a core.
///
/// Every backend the fleet runs passes today, the SYSTEM-context one because the
/// RTP pump opted in. This is here for the next one: the last spin went unseen
/// for months, because an empty loop looks like a healthy idle host.
#[derive(Debug, Default)]
pub struct EmptyGuard {
    armed: bool,
    in_a_row: u32,
    warned: bool,
}

impl EmptyGuard {
    /// `armed` false is the kill switch: every call answers nothing.
    pub fn new(armed: bool) -> Self {
        Self {
            armed,
            ..Self::default()
        }
    }

    /// A frame arrived, so whatever came before it was no spin.
    pub fn on_frame(&mut self) {
        self.in_a_row = 0;
    }

    /// An `Ok(None)` that took `took`. Returns the sleep it owes, zero for an
    /// empty that waited, and warns once a run of them shows a spin.
    pub fn on_empty(&mut self, took: Duration, backend: &str) -> Duration {
        if !self.armed {
            return Duration::ZERO;
        }
        if took >= INSTANT_BELOW {
            self.in_a_row = 0;
            return Duration::ZERO;
        }
        self.in_a_row = self.in_a_row.saturating_add(1);
        if self.in_a_row >= SPINNING_AFTER && !self.warned {
            self.warned = true;
            tracing::warn!(
                backend,
                took_us = took.as_micros() as u64,
                "capture backend answers `no frame` at once, breaking next_frame's contract: flooring each one at 2 ms (#3272)"
            );
        }
        EMPTY_FLOOR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Frame, ScreenCapture};
    use anyhow::Result;

    const INSTANT: Duration = Duration::from_micros(20);

    /// A backend that breaks the contract: it answers at once, always.
    struct Spinner;

    #[async_trait::async_trait]
    impl ScreenCapture for Spinner {
        async fn next_frame(&mut self) -> Result<Option<Frame>> {
            Ok(None)
        }
        fn monitor_count(&self) -> u8 {
            1
        }
        fn backend(&self) -> &'static str {
            "spinner"
        }
    }

    /// The RTP pump's `Ok(None)` arm against a backend that spins: however fast
    /// the backend answers, the loop asks at most once per floor.
    #[tokio::test]
    async fn a_backend_that_answers_at_once_is_floored() {
        let mut capturer = Spinner;
        let mut guard = EmptyGuard::new(true);
        let started = Instant::now();
        const ASKS: u32 = 25;
        for _ in 0..ASKS {
            let asked = Instant::now();
            assert!(capturer.next_frame().await.unwrap().is_none());
            let floor = guard.on_empty(asked.elapsed(), capturer.backend());
            assert_eq!(floor, EMPTY_FLOOR);
            tokio::time::sleep(floor).await;
        }
        assert!(guard.warned, "a spin is said out loud");
        assert!(
            started.elapsed() >= EMPTY_FLOOR * ASKS,
            "{ASKS} asks in {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn one_instant_empty_is_not_a_spin_and_a_frame_or_a_wait_clears_the_run() {
        // After a slow encode a pull backend's next ask comes straight back.
        // It still owes its floor, but says nothing: this is a healthy X11 host.
        let mut guard = EmptyGuard::new(true);
        for _ in 0..(SPINNING_AFTER - 1) {
            assert_eq!(guard.on_empty(INSTANT, "x"), EMPTY_FLOOR);
        }
        assert!(!guard.warned);

        guard.on_frame();
        for _ in 0..(SPINNING_AFTER - 1) {
            guard.on_empty(INSTANT, "x");
        }
        assert!(!guard.warned, "a frame between them ends the run");

        assert_eq!(
            guard.on_empty(Duration::from_millis(16), "x"),
            Duration::ZERO
        );
        for _ in 0..(SPINNING_AFTER - 1) {
            guard.on_empty(INSTANT, "x");
        }
        assert!(!guard.warned, "an empty that waited ends the run");

        guard.on_empty(INSTANT, "x");
        assert!(guard.warned, "the third in a row is a spin");
    }

    /// The contract's other side: an empty that waited owes nothing, down to
    /// the floor itself, so a backend on its 2 ms floor is not floored twice.
    #[test]
    fn an_empty_that_waited_owes_nothing() {
        let mut guard = EmptyGuard::new(true);
        for took in [
            INSTANT_BELOW,
            EMPTY_FLOOR,
            Duration::from_millis(16),
            Duration::from_millis(100),
        ] {
            assert_eq!(guard.on_empty(took, "x"), Duration::ZERO, "{took:?}");
        }
        assert!(!guard.warned);
    }

    #[test]
    fn the_kill_switch_disarms_the_guard() {
        let mut guard = EmptyGuard::new(false);
        for _ in 0..100 {
            assert_eq!(guard.on_empty(INSTANT, "x"), Duration::ZERO);
        }
        assert!(!guard.warned);
    }

    #[test]
    fn the_floor_is_what_the_empty_has_not_already_spent() {
        assert_eq!(floor_remaining(Duration::ZERO), EMPTY_FLOOR);
        assert_eq!(
            floor_remaining(Duration::from_micros(500)),
            Duration::from_micros(1_500)
        );
        assert_eq!(floor_remaining(EMPTY_FLOOR), Duration::ZERO);
        assert_eq!(floor_remaining(Duration::from_millis(16)), Duration::ZERO);
    }

    #[test]
    fn the_idle_line_runs_on_time_not_on_empties() {
        let t0 = Instant::now();
        assert!(!idle_line_due(t0, t0));
        assert!(!idle_line_due(
            t0,
            t0 + IDLE_LINE_EVERY - Duration::from_millis(1)
        ));
        assert!(idle_line_due(t0, t0 + IDLE_LINE_EVERY));
        // A clock read before `last` is no line and no panic.
        assert!(!idle_line_due(t0 + IDLE_LINE_EVERY, t0));
    }
}
