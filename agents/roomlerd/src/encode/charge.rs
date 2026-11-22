// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-148 D51 B4f, B4g — the time an encode is charged.
//!
//! libvpx's one-pass CBR reads its frame rate off the gaps between the pts it
//! is handed, and grants each encode its target over that gap. The VP9-444
//! pump hands it the capture's own time (#3472's [`refed_pts`] for a frame fed
//! again), and gates before it captures: a frame that waited H ms for the
//! in-flight budget (`media_inflight`) is granted the target over the capture
//! interval and H. With B4b's gain at 1.0 every frame period then carries
//! exactly the budget's rate, and nothing brings the period back to the
//! capture's own (the slideshow D51's closed loop measured).
//!
//! B4f bounded the charged gap at the interval the pump chose for the frame.
//! B4g: where the budget acts (`on`, a relay session), an encode is charged its
//! gap less the time the pump's capacity gate kept frames back since the last
//! encode ([`GateClock`]: a full send channel, congested followers, the budget's
//! hold, since FR-148 a drain pause), never less than that interval, or the gap
//! where it is shorter: `max(gap - gated, min(gap, interval))` ([`Gate`]). A hold
//! is never charged, so the hold stays the restoring force; a capture slower than
//! the pump's target, jitter and an idle stretch are charged as they come, so the
//! encoder spends its target at the rate the capture runs. Where the budget does
//! not act (`off`, `shadow`, a direct session) there is no gate, and the pts is
//! today's, byte for byte.

use std::time::{Duration, Instant};

/// The last encode's stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// The pts the encoder was handed, µs.
    pub pts: i64,
    /// The pts today's rule gives the same frame ([`refed_pts`]), µs. The
    /// next frame's gap is read from it, so a bound never changes what the
    /// gap is, only how much of it is charged.
    pub today: i64,
    /// When it was handed.
    pub at: Instant,
}

/// #3472 — the pts libvpx is handed for a frame whose own `pts` is not after
/// the last one (`last`: that pts, and when it was handed). The pump's idle
/// keepalive re-encodes its last frame: the same `Arc<Frame>`, so the same
/// `monotonic_us`. libvpx's one-pass rate control reads its frame rate off
/// the gaps between timestamps, so a repeated pts adds no time, yet each
/// encode is still granted `target ÷ that rate`. The keepalive banked a
/// capture's worth of budget and the next capture spent it: 2.0–2.4× the
/// target in the field (2026-11-03, 11-04). Stamped with the last pts plus
/// the time since the last encode, every encode carries its own time and
/// the grants add up to the target. At least 1 µs later, so the pts stays
/// strictly increasing whatever the clock reads.
pub fn refed_pts(last: Option<(i64, Instant)>, pts: i64, now: Instant) -> i64 {
    match last {
        Some((last_pts, at)) if pts <= last_pts => {
            let since =
                i64::try_from(now.saturating_duration_since(at).as_micros()).unwrap_or(i64::MAX);
            last_pts.saturating_add(since.max(1))
        }
        _ => pts,
    }
}

/// FR-148 D51 B4g — what the pump hands an encode where the in-flight budget
/// acts: the time its capacity gate kept frames back since the last encode
/// ([`GateClock::take`]) and the interval it chose for the frame
/// (`RateGovernor::charge_interval`), which is the charge's floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gate {
    pub gated: Duration,
    pub interval: Duration,
}

/// FR-148 D51 B4f, B4g — the stamp of a frame whose own pts is `pts`, fed at
/// `now`, after the encode `last` stamped. Today's pts comes from
/// [`refed_pts`]; the pts handed advances from the last one handed by
/// today's gap, less the time gated, never less than the interval or the gap
/// where that is shorter: `max(gap - gated, min(gap, interval))`, at least
/// 1 µs. With no gate it advances by the whole gap, so a session the budget
/// never acted on is handed today's pts exactly. After a gated stretch the
/// pts handed runs behind today's by what was not charged, and stays there:
/// nothing banks.
pub fn charge(last: Option<Stamp>, pts: i64, now: Instant, gate: Option<Gate>) -> Stamp {
    let today = refed_pts(last.map(|l| (l.today, l.at)), pts, now);
    let handed = match last {
        None => today,
        Some(l) => {
            let gap = today.saturating_sub(l.today);
            let gap = match gate {
                Some(g) => {
                    let floor = gap.min(micros(g.interval).max(1));
                    gap.saturating_sub(micros(g.gated)).max(floor)
                }
                None => gap,
            };
            l.pts.saturating_add(gap)
        }
    };
    Stamp {
        pts: handed,
        today,
        at: now,
    }
}

fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// FR-148 D51 B4g — the time the pump's capacity gate kept frames back since
/// the last encode, for any of its reasons (a full send channel, congested
/// followers, the in-flight budget's hold; since FR-148 a drain pause,
/// [`Self::paused`]). A span runs from the first pass the gate skipped to the
/// pass it let through, and spans add up until the encode takes them ([`Self::take`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct GateClock {
    since: Option<Instant>,
    gated: Duration,
    /// When the gate last let a pass through.
    passed_at: Option<Instant>,
}

impl GateClock {
    /// A pass the gate skipped, at `now`: the first since it last let one
    /// through opens a span.
    pub fn skipped(&mut self, now: Instant) {
        self.since.get_or_insert(now);
    }

    /// A pass the gate let through, at `now`: the open span closes.
    pub fn passed(&mut self, now: Instant) {
        if let Some(since) = self.since.take() {
            self.gated += now.saturating_duration_since(since);
        }
        self.passed_at = Some(now);
    }

    /// FR-148 — the pass the gate let through last was held after the gate
    /// (FR-59 P4's drain pause), its capture included: it counts as a pass
    /// the gate skipped from the instant the gate let it through, and the
    /// next pass the gate lets through closes the span.
    pub fn paused(&mut self) {
        if let Some(at) = self.passed_at {
            self.since.get_or_insert(at);
        }
    }

    /// The time gated since the last encode, which this encode takes: the
    /// next one's starts from nothing.
    pub fn take(&mut self) -> Duration {
        std::mem::take(&mut self.gated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pump's interval at 30 fps, µs: what `charge_interval` gives it.
    const INTERVAL: i64 = 33_333;

    fn us(n: i64) -> Duration {
        Duration::from_micros(n as u64)
    }

    /// Under `on`: `gated` µs of the gap were gated, at the pump's interval.
    fn on(gated: i64) -> Option<Gate> {
        Some(Gate {
            gated: us(gated),
            interval: us(INTERVAL),
        })
    }

    /// Frames fed as the pump feeds them, each `gap` µs of capture after the
    /// last and fed that much later; each answers the gap it was charged.
    struct Feed {
        t0: Instant,
        last: Option<Stamp>,
        capture: i64,
    }

    impl Feed {
        fn new() -> Self {
            Self {
                t0: Instant::now(),
                last: None,
                capture: 1_000,
            }
        }

        fn frame(&mut self, gap: i64, gate: Option<Gate>) -> Option<i64> {
            self.capture += gap;
            let s = charge(self.last, self.capture, self.t0 + us(self.capture), gate);
            let charged = self.last.map(|l| s.pts - l.pts);
            self.last = Some(s);
            charged
        }
    }

    /// #3472 — the stamp rule alone: a pts after the last one passes through;
    /// one that is not (repeated or earlier) takes the last pts plus the time
    /// since the last encode, and is never the last pts again.
    #[test]
    fn a_frame_fed_again_takes_the_time_since_the_last_encode() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(
            refed_pts(None, 5_000, at(0)),
            5_000,
            "a first frame keeps its pts"
        );
        let last = Some((5_000, at(0)));
        assert_eq!(
            refed_pts(last, 105_000, at(100)),
            105_000,
            "a later pts passes"
        );
        assert_eq!(
            refed_pts(last, 5_000, at(60)),
            65_000,
            "a repeat takes the 60 ms since"
        );
        assert_eq!(
            refed_pts(last, 1_000, at(60)),
            65_000,
            "and so does an earlier pts"
        );
        assert_eq!(
            refed_pts(last, 5_000, at(0)),
            5_001,
            "never the last pts again"
        );
    }

    /// FR-148 D51 B4f, B4g (C9) — under `on`, a hold is never charged: an
    /// encode after a hold of H ms, all of it gated, is charged as a frame on
    /// time. Frames on time are charged their 33 ms, a frame after a hold of
    /// 10, 50 or 500 ms one interval, a frame sooner than the interval only
    /// its gap, and N encodes after random holds at most N intervals in all.
    #[test]
    fn under_on_a_hold_is_never_charged() {
        let mut f = Feed::new();
        assert_eq!(f.frame(0, on(0)), None, "the first frame has no gap");
        for (gap, held, want) in [
            (INTERVAL, 0, INTERVAL),
            (INTERVAL + 10_000, 10_000, INTERVAL),
            (INTERVAL, 0, INTERVAL),
            (INTERVAL + 50_000, 50_000, INTERVAL),
            (INTERVAL + 500_000, 500_000, INTERVAL),
            (20_000, 0, 20_000),
            (INTERVAL, 0, INTERVAL),
        ] {
            assert_eq!(f.frame(gap, on(held)), Some(want), "a gap of {gap} µs");
        }
        // Random gaps from on time to a second, all past the interval held.
        let mut state = 0x000b_4fc9_u64;
        let (mut n, mut charged) = (0_i64, 0_i64);
        for _ in 0..10_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let gap = 1 + (state % 1_000_000) as i64;
            let held = (gap - INTERVAL).max(0);
            let c = f.frame(gap, on(held)).expect("a gap");
            assert!((1..=INTERVAL).contains(&c), "a gap of {gap} µs charged {c}");
            n += 1;
            charged += c;
        }
        assert!(charged <= n * INTERVAL, "{charged} µs over {n} encodes");
    }

    /// FR-148 D51 B4f, B4g — the kill switch. Where the budget does not act
    /// (`off`, `shadow`, a direct session) there is no gate, and the charged
    /// pts is today's (`refed_pts`), byte for byte: 100,000 frames of random
    /// gaps, repeats (the keepalive), earlier pts and clock steps, against
    /// today's rule kept as the encoder kept it.
    #[test]
    fn with_no_gate_the_charged_pts_is_todays() {
        let t0 = Instant::now();
        let mut state = 0x00b4_f0ff_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut stamp, mut today): (Option<Stamp>, Option<(i64, Instant)>) = (None, None);
        let (mut capture, mut clock) = (1_i64, 0_u64);
        for i in 0..100_000u32 {
            let r = next();
            clock += r % 200_000;
            let now = t0 + Duration::from_micros(clock);
            let pts = match (r >> 20) % 8 {
                // The keepalive: the same frame again.
                0 => capture,
                // An earlier pts.
                1 => capture - ((r >> 32) % 50_000) as i64,
                // A fresh capture, any gap.
                _ => {
                    capture += 1 + ((r >> 32) % 2_000_000) as i64;
                    capture
                }
            };
            let want = refed_pts(today, pts, now);
            let s = charge(stamp, pts, now, None);
            assert_eq!(s.pts, want, "frame {i}");
            stamp = Some(s);
            today = Some((want, now));
        }
    }

    /// FR-148 D51 B4f, B4g (C10) — the keepalive. A frame fed again 60 ms
    /// after its capture (`IDLE_KEEPALIVE`, #3472) is charged, under `on`, its
    /// 60 ms less the time gated: after 40 ms of a hold, the interval; after
    /// 10 ms, 50 ms; with none gated, its 60 ms, as with no gate.
    #[test]
    fn under_on_a_refed_frame_is_charged_its_time_less_the_gated_time() {
        let t0 = Instant::now();
        for (gate, want) in [
            (on(40_000), INTERVAL),
            (on(10_000), 50_000),
            (on(0), 60_000),
            (None, 60_000),
        ] {
            let first = charge(None, 5_000, t0, gate);
            let again = charge(Some(first), 5_000, t0 + Duration::from_millis(60), gate);
            assert_eq!(again.pts - first.pts, want, "{gate:?}");
            assert_eq!(again.today, 65_000, "today's pts, {gate:?}");
        }
    }

    /// FR-148 D51 B4g (C12) — under `on`, a capture slower than the pump's
    /// target (20 fps on its 30, no FR-112 pacing) with no pass gated is
    /// charged its real gaps, 50 ms each, so N encodes are charged their wall
    /// time. B4f charged each the 33.3 ms interval: libvpx believed 30 fps and
    /// spent two thirds of its target.
    #[test]
    fn under_on_a_slow_capture_is_charged_its_real_gaps() {
        let mut f = Feed::new();
        assert_eq!(f.frame(0, on(0)), None, "the first frame has no gap");
        let (mut n, mut charged) = (0_i64, 0_i64);
        for i in 0..600 {
            let c = f.frame(50_000, on(0)).expect("a gap");
            assert_eq!(c, 50_000, "frame {i}");
            n += 1;
            charged += c;
        }
        assert_eq!(charged, n * 50_000, "{n} encodes, their wall time");
    }

    /// FR-148 D51 B4g (C13) — under `on`, capture jitter with no pass gated is
    /// charged as it comes: gaps alternating 30 and 37 ms are charged 30 and
    /// 37 ms, never clipped at the 33.3 ms interval, so they add up to the wall
    /// time.
    #[test]
    fn under_on_capture_jitter_is_charged_as_it_comes() {
        let mut f = Feed::new();
        assert_eq!(f.frame(0, on(0)), None, "the first frame has no gap");
        let (mut wall, mut charged) = (0_i64, 0_i64);
        for i in 0..600 {
            let gap = if i % 2 == 0 { 30_000 } else { 37_000 };
            let c = f.frame(gap, on(0)).expect("a gap");
            assert_eq!(c, gap, "frame {i}");
            wall += gap;
            charged += c;
        }
        assert_eq!(charged, wall, "the wall time");
    }

    /// FR-148 D51 B4g (C14) — the floor. Under `on` the gated time comes off
    /// the gap down to the interval and no further: a 40 ms gap of which 20 ms
    /// were gated is charged the interval (33.3 ms), of which 5 ms were gated
    /// 35 ms. A gap shorter than the interval is charged as it is, gated or
    /// not, and so is a gap shorter than the time gated.
    #[test]
    fn under_on_the_gated_time_comes_off_down_to_the_interval() {
        let mut f = Feed::new();
        assert_eq!(f.frame(0, on(0)), None, "the first frame has no gap");
        for (gap, gated, want) in [
            (40_000, 20_000, INTERVAL),
            (40_000, 5_000, 35_000),
            (100_000, 100_000, INTERVAL),
            (20_000, 0, 20_000),
            (20_000, 15_000, 20_000),
            (30_000, 50_000, 30_000),
        ] {
            assert_eq!(
                f.frame(gap, on(gated)),
                Some(want),
                "a gap of {gap} µs, {gated} µs gated"
            );
        }
    }

    /// FR-148 D51 B4g — the pump's gate clock: a span runs from the first pass
    /// the gate skipped to the pass it let through, spans add up, and the
    /// encode takes them all; a pass let through with no skip before it adds
    /// nothing.
    #[test]
    fn the_gate_clock_adds_its_spans_until_the_encode_takes_them() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut c = GateClock::default();
        c.passed(at(1));
        assert_eq!(c.take(), Duration::ZERO, "no skip, nothing gated");
        for ms in [10, 12, 14] {
            c.skipped(at(ms));
        }
        c.passed(at(16));
        c.passed(at(17));
        c.skipped(at(30));
        c.passed(at(33));
        assert_eq!(c.take(), Duration::from_millis(9), "6 ms and 3 ms");
        assert_eq!(c.take(), Duration::ZERO, "taken");
        c.skipped(at(40));
        assert_eq!(c.take(), Duration::ZERO, "an open span is not taken");
        c.passed(at(45));
        assert_eq!(c.take(), Duration::from_millis(5), "it closes later");
    }

    /// FR-148 (C16) — under `on`, a drain pause is gated. FR-59 P4's pause
    /// holds a pass after the VP9-444 pump's gate has let it through, its
    /// capture included, so the pass counts as one the gate skipped from the
    /// instant the gate let it through ([`GateClock::paused`]), and the pass
    /// the gate lets through after the pause closes the span. Passes as the
    /// pump runs them on a 30 fps capture: the gate lets a pass through 10 ms
    /// after the last one's capture (the encode, or the pause's sleep), and the
    /// capture waits for its frame. An encode after a 300 ms pause, with no
    /// pass the gate skipped, is charged the interval, where today it is
    /// charged its gap; a span opened only where the pause sleeps would gate
    /// 90 ms of the 300. With no gate (`off`, `shadow`, a direct session) it is
    /// charged its gap, as today.
    #[test]
    fn under_on_a_drain_pause_is_gated() {
        let t0 = Instant::now();
        let at = |us_: i64| t0 + us(us_);
        // Pass k's frame is captured at 100 ms + k intervals.
        let captured = |k: i64| 100_000 + k * INTERVAL;
        for (gate, want) in [(on(0), INTERVAL), (None, 10 * INTERVAL)] {
            let mut clock = GateClock::default();
            clock.passed(at(captured(0) - 20_000));
            let first = charge(None, captured(0), at(captured(0)), gate);
            let _ = clock.take();
            // Passes 1 to 9: the pause holds each once its frame is captured.
            for k in 1..=9 {
                clock.passed(at(captured(k - 1) + 10_000));
                clock.paused();
            }
            // Pass 10: the pause is over, and its frame is encoded.
            clock.passed(at(captured(9) + 10_000));
            let gated = clock.take();
            let after = charge(
                Some(first),
                captured(10),
                at(captured(10)),
                gate.map(|g| Gate { gated, ..g }),
            );
            assert_eq!(
                after.pts - first.pts,
                want,
                "{gate:?}: a gap of {} µs after the pause",
                after.today - first.today
            );
            assert_eq!(gated, us(9 * INTERVAL), "the pause, from its first pass");
        }
    }
}
