//! FR-148 D51 B4f — the time an encode is charged.
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
//! B4f: where the budget acts (`on`, a relay session), the pump charges an
//! encode at most the interval it chose for that frame, its capture interval
//! times the skip divisor (`RateGovernor::charge_most`). A gap beyond it is not
//! charged: a budget hold, a channel-full skip, a drain pause or an idle
//! stretch. A frame that waited is granted as a frame on time, so after a hold
//! the encoder sends less than its target until the queue drains, and the hold
//! is the restoring force. Where the budget does not act (`off`, `shadow`, a
//! direct session) there is no bound, and the pts is today's, byte for byte.

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

/// FR-148 D51 B4f — the stamp of a frame whose own pts is `pts`, fed at
/// `now`, after the encode `last` stamped. Today's pts comes from
/// [`refed_pts`]; the pts handed advances from the last one handed by
/// today's gap, at most `most` (at least 1 µs). With no bound it advances
/// by the whole gap, so a session that was never bounded is handed today's
/// pts exactly. After a bounded stretch the pts handed runs behind today's
/// by what was not charged, and stays there: nothing banks.
pub fn charge(last: Option<Stamp>, pts: i64, now: Instant, most: Option<Duration>) -> Stamp {
    let today = refed_pts(last.map(|l| (l.today, l.at)), pts, now);
    let handed = match last {
        None => today,
        Some(l) => {
            let gap = today.saturating_sub(l.today);
            let gap = match most {
                Some(m) => gap.min(i64::try_from(m.as_micros()).unwrap_or(i64::MAX).max(1)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The pump's interval at 30 fps, µs: what `charge_most` gives it.
    const INTERVAL: i64 = 33_333;

    fn us(n: i64) -> Duration {
        Duration::from_micros(n as u64)
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

    /// FR-148 D51 B4f (C9) — under `on`, an encode after a hold of H ms is
    /// charged the smaller of its gap and the interval: frames on time are
    /// charged their 33 ms, a frame after a hold of 10, 50 or 500 ms is
    /// charged one interval, a frame sooner than the interval only its gap,
    /// and N encodes are charged at most N intervals in all.
    #[test]
    fn under_on_an_encode_after_a_hold_is_charged_at_most_the_interval() {
        let t0 = Instant::now();
        let most = Some(us(INTERVAL));
        let mut last: Option<Stamp> = None;
        let mut capture = 1_000_i64;
        let mut feed = |gap: i64| {
            capture += gap;
            let now = t0 + us(capture);
            let s = charge(last, capture, now, most);
            let charged = last.map(|l| s.pts - l.pts);
            last = Some(s);
            charged
        };
        assert_eq!(feed(0), None, "the first frame has no gap");
        for (gap, want) in [
            (INTERVAL, INTERVAL),
            (INTERVAL + 10_000, INTERVAL),
            (INTERVAL, INTERVAL),
            (INTERVAL + 50_000, INTERVAL),
            (INTERVAL + 500_000, INTERVAL),
            (20_000, 20_000),
            (INTERVAL, INTERVAL),
        ] {
            assert_eq!(feed(gap), Some(want), "a gap of {gap} µs");
        }
        // Random gaps from on time to a second: N encodes, at most N intervals.
        let mut state = 0x000b_4fc9_u64;
        let (mut n, mut charged) = (0_i64, 0_i64);
        for _ in 0..10_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let gap = 1 + (state % 1_000_000) as i64;
            let c = feed(gap).expect("a gap");
            assert!((1..=INTERVAL).contains(&c), "a gap of {gap} µs charged {c}");
            n += 1;
            charged += c;
        }
        assert!(charged <= n * INTERVAL, "{charged} µs over {n} encodes");
    }

    /// FR-148 D51 B4f (C9) — where the budget does not act (`off`, `shadow`,
    /// a direct session) the charged pts is today's (`refed_pts`), byte for
    /// byte: 100,000 frames of random gaps, repeats (the keepalive), earlier
    /// pts and clock steps, against today's rule kept as the encoder kept it.
    #[test]
    fn with_no_bound_the_charged_pts_is_todays() {
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

    /// FR-148 D51 B4f (C10) — the keepalive. A frame fed again 60 ms after
    /// its capture (`IDLE_KEEPALIVE`) is charged, under `on`, at most the
    /// interval; with no bound, exactly #3472's 60 ms since the last encode.
    #[test]
    fn under_on_a_refed_frame_is_charged_at_most_the_interval() {
        let t0 = Instant::now();
        for (most, want) in [(Some(us(INTERVAL)), INTERVAL), (None, 60_000)] {
            let first = charge(None, 5_000, t0, most);
            let again = charge(Some(first), 5_000, t0 + Duration::from_millis(60), most);
            assert_eq!(again.pts - first.pts, want, "{most:?}");
            assert_eq!(again.today, 65_000, "today's pts, {most:?}");
        }
    }
}
