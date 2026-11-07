// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-135 R0 (§6.10, §3.6) — one data-channel pump's window of media numbers,
//! the source of the heartbeat's new fields and of the session's summary.
//!
//! Before this a fleet read of what a session did meant scraping `agent_logs`,
//! which keeps seven days, and every pump answered "how long did a frame wait"
//! differently. A window here is a set of histograms and counts, kept by the
//! pump that owns it and taken whole at its heartbeat:
//!
//! | Field | Instrument | Closes |
//! |---|---|---|
//! | `capture_backend`, `frames_keepalive`, `keepalive_bytes` | N3 | which arm, and what a still screen costs |
//! | `cap_to_handoff_ms` and its p50/p95 | N7 | a frame's age before it reaches the channel, in the process's one clock |
//! | `handoff_gap_ms` | N10, the agent's half | the cadence the channel is fed at |
//! | `kf_ask_to_queued_ms`, `kf_asks`, `kf_asks_gapped` | N12 | how long a viewer's keyframe ask waits, and how many the 500 ms gap drops |
//! | `idr_causes` | cause-tagged IDR | why a session pays for each keyframe |
//! | `dc_lock_ms` | §2.5 | the data-channel mutex wait that `send_wait` does not contain |
//! | `maxrate_bps`, `window_ms` | R12's red, L03 | the ceiling in force, and the window's real length |
//!
//! Histograms, not means, because a p99 over a second at 120 fps is about one
//! sample, and the session summary has to fold them. Each is twelve log-2
//! millisecond buckets: `[0,1) [1,2) [2,4) … [512,1024) [1024,∞)`.
//!
//! The session summary (N16) is folded from the same windows into
//! [`crate::session_telemetry`]: seconds moving and sparse, frames encoded and
//! kept alive, the most followers, and the time to the first frame. FR-135
//! calls a window that captures 5 or more frames a second **moving** and the
//! rest **sparse** (§2.1).
//!
//! ⚠️ Nothing here decides anything: the pump records, the heartbeat prints,
//! the session reports. With `stats` on a session's old path
//! (`rc_media_legacy`, [`crate::media_paths`]) there is no window at all, and
//! the heartbeat is today's.

use std::time::{Duration, Instant};

/// How many log-2 millisecond buckets a histogram has.
pub const BUCKETS: usize = 12;

/// FR-135 §2.1 — the capture rate at or above which a window is moving.
pub const MOVING_FPS: f64 = 5.0;

/// How often a pump folds its window into the session's summary: the 2 s
/// windows §2.1's split was measured on.
pub const FOLD_EVERY: Duration = Duration::from_secs(2);

/// The longest window the summary folds as one. A pump that went quiet for
/// minutes (a gate, a drain, a long capture wait) folds the stretch as one
/// window of at most this, and drops the rest, rather than calling ten minutes
/// of a wait "sparse".
pub const MAX_FOLD: Duration = Duration::from_secs(10);

/// Log-2 millisecond buckets, foldable, with the largest sample kept exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogHist {
    counts: [u32; BUCKETS],
    max_us: u64,
}

impl LogHist {
    fn bucket(us: u64) -> usize {
        let ms = us / 1000;
        if ms == 0 {
            0
        } else {
            ((64 - ms.leading_zeros()) as usize).min(BUCKETS - 1)
        }
    }

    pub fn add_us(&mut self, us: u64) {
        let b = Self::bucket(us);
        self.counts[b] = self.counts[b].saturating_add(1);
        self.max_us = self.max_us.max(us);
    }

    pub fn add(&mut self, d: Duration) {
        self.add_us(d.as_micros().min(u64::MAX as u128) as u64);
    }

    pub fn count(&self) -> u32 {
        self.counts.iter().fold(0u32, |a, c| a.saturating_add(*c))
    }

    /// The bucket counts, for the heartbeat line.
    pub fn counts(&self) -> [u32; BUCKETS] {
        self.counts
    }

    /// The largest sample, in ms; `None` when empty.
    pub fn max_ms(&self) -> Option<f64> {
        (self.count() > 0).then(|| self.max_us as f64 / 1000.0)
    }

    pub fn fold(&mut self, other: &LogHist) {
        for (a, b) in self.counts.iter_mut().zip(other.counts) {
            *a = a.saturating_add(b);
        }
        self.max_us = self.max_us.max(other.max_us);
    }
}

/// Exact quantiles over one window, from a bounded sample buffer. At 144 fps a
/// two-second window is 288 frames; past the cap the window keeps its first
/// [`Self::CAP`] samples and says so in `dropped`.
#[derive(Clone, Debug, Default)]
pub struct Samples {
    us: Vec<u32>,
    dropped: u32,
}

impl Samples {
    pub const CAP: usize = 1024;

    pub fn add_us(&mut self, us: u64) {
        if self.us.len() < Self::CAP {
            self.us.push(us.min(u32::MAX as u64) as u32);
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    /// The `q` quantile in ms by nearest rank, or `None` with no samples.
    /// Sorts in place, so call it once per window, at the heartbeat.
    pub fn quantile_ms(&mut self, q: f64) -> Option<f64> {
        if self.us.is_empty() {
            return None;
        }
        self.us.sort_unstable();
        let rank =
            ((q.clamp(0.0, 1.0) * self.us.len() as f64).ceil() as usize).clamp(1, self.us.len());
        Some(self.us[rank - 1] as f64 / 1000.0)
    }

    fn clear(&mut self) {
        self.us.clear();
        self.dropped = 0;
    }
}

/// Why a keyframe was forced. The index is the slot in `idr_causes`, and the
/// order is wire: a new cause is appended, never inserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdrCause {
    /// A force from the shared keyframe flag. The viewer's `rc:keyframe` sets
    /// it (counted apart as `kf_asks`), and so do the pump's settle request
    /// ([`IdrCause::Settle`] counts those requests) and its channel-not-open
    /// request (`dc_unopen_drops` counts those frames). Requests that land
    /// between two passes merge into one force.
    Asked = 0,
    /// A follower of the shared pipeline joined or asked to resync.
    Shared = 1,
    /// The lock screen came or went.
    Lock = 2,
    /// A force went unanswered for 4 s, and was retried.
    Backstop = 3,
    /// The first frame through after a drop burst.
    Resync = 4,
    /// The settle request: the first keepalive after a motion burst.
    Settle = 5,
}

impl IdrCause {
    pub const COUNT: usize = 6;
}

/// One heartbeat's worth of the window, taken whole.
#[derive(Clone, Debug, Default)]
pub struct WindowReport {
    pub window_ms: u64,
    pub frames_keepalive: u32,
    pub keepalive_bytes: u64,
    pub cap_to_handoff: LogHist,
    pub cap_to_handoff_p50_ms: Option<f64>,
    pub cap_to_handoff_p95_ms: Option<f64>,
    pub handoff_gap: LogHist,
    pub kf_ask_to_queued: LogHist,
    pub dc_lock: LogHist,
    pub idr_causes: [u32; IdrCause::COUNT],
}

/// The pump's window. One per pump instance; nothing else touches it.
#[derive(Debug)]
pub struct MediaWindow {
    started: Instant,
    frames_keepalive: u32,
    keepalive_bytes: u64,
    cap_to_handoff: LogHist,
    cap_to_handoff_us: Samples,
    handoff_gap: LogHist,
    last_handoff: Option<Instant>,
    kf_ask_to_queued: LogHist,
    dc_lock: LogHist,
    idr_causes: [u32; IdrCause::COUNT],
    /// The session fold's own clock and count, apart from the heartbeat's.
    fold: Fold,
}

#[derive(Debug)]
struct Fold {
    since: Instant,
    real_frames: u32,
    encoded: u64,
    keepalive: u64,
}

/// What a fold adds to the session's summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldReport {
    /// Milliseconds counted, moving or sparse; at most [`MAX_FOLD`].
    pub ms: u64,
    pub moving: bool,
    pub frames_encoded: u64,
    pub frames_keepalive: u64,
}

impl MediaWindow {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            frames_keepalive: 0,
            keepalive_bytes: 0,
            cap_to_handoff: LogHist::default(),
            cap_to_handoff_us: Samples::default(),
            handoff_gap: LogHist::default(),
            last_handoff: None,
            kf_ask_to_queued: LogHist::default(),
            dc_lock: LogHist::default(),
            idr_causes: [0; IdrCause::COUNT],
            fold: Fold {
                since: now,
                real_frames: 0,
                encoded: 0,
                keepalive: 0,
            },
        }
    }

    /// A real capture: new pixels, not a keepalive re-encode.
    pub fn on_real_frame(&mut self) {
        self.fold.real_frames = self.fold.real_frames.saturating_add(1);
    }

    /// A keepalive re-encode of the last frame was decided.
    pub fn on_keepalive(&mut self) {
        self.frames_keepalive = self.frames_keepalive.saturating_add(1);
        self.fold.keepalive = self.fold.keepalive.saturating_add(1);
    }

    /// The encoder returned a frame's packets.
    pub fn on_encoded(&mut self) {
        self.fold.encoded = self.fold.encoded.saturating_add(1);
    }

    /// One frame's packets reached the channel's queue at `now`. `captured` is
    /// when the pump took the frame from the capturer (the process's one
    /// clock), `None` on a keepalive, which carries no new capture.
    pub fn on_handoff(&mut self, now: Instant, captured: Option<Instant>, bytes: u64) {
        if let Some(prev) = self.last_handoff {
            self.handoff_gap.add(now.saturating_duration_since(prev));
        }
        self.last_handoff = Some(now);
        match captured {
            Some(at) => {
                let waited = now.saturating_duration_since(at);
                self.cap_to_handoff.add(waited);
                self.cap_to_handoff_us
                    .add_us(waited.as_micros().min(u64::MAX as u128) as u64);
            }
            None => self.keepalive_bytes = self.keepalive_bytes.saturating_add(bytes),
        }
    }

    /// The pump waited `waited` for the video channel's mutex.
    pub fn on_dc_lock(&mut self, waited: Duration) {
        self.dc_lock.add(waited);
    }

    /// A keyframe was forced, for `cause`.
    pub fn on_force(&mut self, cause: IdrCause) {
        let slot = &mut self.idr_causes[cause as usize];
        *slot = slot.saturating_add(1);
    }

    /// A key-flagged packet entered the channel's queue, `waited` after the
    /// ask it answers.
    pub fn on_key_queued(&mut self, waited: Duration) {
        self.kf_ask_to_queued.add(waited);
    }

    /// Take the heartbeat's window and start the next one.
    pub fn take(&mut self, now: Instant) -> WindowReport {
        let report = WindowReport {
            window_ms: now.saturating_duration_since(self.started).as_millis() as u64,
            frames_keepalive: self.frames_keepalive,
            keepalive_bytes: self.keepalive_bytes,
            cap_to_handoff: self.cap_to_handoff,
            cap_to_handoff_p50_ms: self.cap_to_handoff_us.quantile_ms(0.5),
            cap_to_handoff_p95_ms: self.cap_to_handoff_us.quantile_ms(0.95),
            handoff_gap: self.handoff_gap,
            kf_ask_to_queued: self.kf_ask_to_queued,
            dc_lock: self.dc_lock,
            idr_causes: self.idr_causes,
        };
        self.started = now;
        self.frames_keepalive = 0;
        self.keepalive_bytes = 0;
        self.cap_to_handoff = LogHist::default();
        self.cap_to_handoff_us.clear();
        self.handoff_gap = LogHist::default();
        self.kf_ask_to_queued = LogHist::default();
        self.dc_lock = LogHist::default();
        self.idr_causes = [0; IdrCause::COUNT];
        report
    }

    /// The session fold, on its own clock: `Some` once `every` has passed since
    /// the last, counted moving or sparse by the window's real capture rate.
    /// A window longer than [`MAX_FOLD`] counts [`MAX_FOLD`].
    pub fn fold_if_due(&mut self, now: Instant, every: Duration) -> Option<FoldReport> {
        let span = now.saturating_duration_since(self.fold.since);
        if span < every {
            return None;
        }
        let secs = span.as_secs_f64();
        let fps = if secs > 0.0 {
            self.fold.real_frames as f64 / secs
        } else {
            0.0
        };
        let report = FoldReport {
            ms: span.min(MAX_FOLD).as_millis() as u64,
            moving: fps >= MOVING_FPS,
            frames_encoded: self.fold.encoded,
            frames_keepalive: self.fold.keepalive,
        };
        self.fold = Fold {
            since: now,
            real_frames: 0,
            encoded: 0,
            keepalive: 0,
        };
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_lands_in_its_log2_millisecond_bucket() {
        let cases = [
            (0, 0),
            (999, 0),
            (1_000, 1),
            (1_999, 1),
            (2_000, 2),
            (3_999, 2),
            (4_000, 3),
            (512_000, 10),
            (1_023_999, 10),
            (1_024_000, 11),
            (60_000_000, 11),
        ];
        for (us, bucket) in cases {
            assert_eq!(LogHist::bucket(us), bucket, "{us} us");
        }
        let mut h = LogHist::default();
        assert_eq!(h.max_ms(), None, "empty is not a zero");
        h.add_us(1_500);
        h.add_us(250_000);
        assert_eq!(h.count(), 2);
        assert_eq!(h.counts()[1], 1);
        assert_eq!(h.counts()[8], 1);
        assert_eq!(h.max_ms(), Some(250.0));
    }

    #[test]
    fn histograms_fold_and_keep_the_largest_sample() {
        let mut a = LogHist::default();
        let mut b = LogHist::default();
        a.add_us(3_000);
        b.add_us(3_500);
        b.add_us(40_000);
        a.fold(&b);
        assert_eq!(a.count(), 3);
        assert_eq!(a.counts()[2], 2);
        assert_eq!(a.max_ms(), Some(40.0));
    }

    #[test]
    fn exact_quantiles_by_nearest_rank_and_a_bounded_buffer() {
        let mut s = Samples::default();
        assert_eq!(s.quantile_ms(0.5), None);
        for ms in 1..=100u64 {
            s.add_us(ms * 1000);
        }
        assert_eq!(s.quantile_ms(0.5), Some(50.0));
        assert_eq!(s.quantile_ms(0.95), Some(95.0));
        assert_eq!(s.quantile_ms(1.0), Some(100.0));
        let mut full = Samples::default();
        for _ in 0..Samples::CAP + 7 {
            full.add_us(1);
        }
        assert_eq!(full.us.len(), Samples::CAP);
        assert_eq!(full.dropped, 7);
    }

    /// A window counts a keepalive's bytes, times real frames from capture to
    /// the channel, gaps between hand-offs, and starts over when taken.
    #[test]
    fn a_window_is_taken_whole_and_starts_over() {
        let t0 = Instant::now();
        let mut w = MediaWindow::new(t0);
        w.on_real_frame();
        w.on_encoded();
        w.on_handoff(t0 + Duration::from_millis(12), Some(t0), 40_000);
        w.on_keepalive();
        w.on_encoded();
        w.on_handoff(t0 + Duration::from_millis(1012), None, 900);
        w.on_dc_lock(Duration::from_micros(300));
        w.on_force(IdrCause::Asked);
        w.on_force(IdrCause::Asked);
        w.on_force(IdrCause::Settle);
        w.on_key_queued(Duration::from_millis(30));
        let r = w.take(t0 + Duration::from_millis(2000));
        assert_eq!(r.window_ms, 2000);
        assert_eq!(r.frames_keepalive, 1);
        assert_eq!(r.keepalive_bytes, 900, "only the keepalive's bytes");
        assert_eq!(
            r.cap_to_handoff.count(),
            1,
            "a keepalive carries no capture"
        );
        assert_eq!(r.cap_to_handoff_p50_ms, Some(12.0));
        assert_eq!(r.handoff_gap.count(), 1);
        assert_eq!(r.handoff_gap.max_ms(), Some(1000.0));
        assert_eq!(r.dc_lock.count(), 1);
        assert_eq!(r.kf_ask_to_queued.max_ms(), Some(30.0));
        assert_eq!(r.idr_causes, [2, 0, 0, 0, 0, 1]);

        let r = w.take(t0 + Duration::from_millis(4000));
        assert_eq!(r.frames_keepalive, 0);
        assert_eq!(r.cap_to_handoff.count(), 0);
        assert_eq!(r.cap_to_handoff_p50_ms, None);
        assert_eq!(r.idr_causes, [0; IdrCause::COUNT]);
        assert_eq!(r.window_ms, 2000);
    }

    /// §2.1's rule: 5 or more real captures a second is moving, keepalives do
    /// not count as motion, and a long quiet stretch folds as at most
    /// MAX_FOLD.
    #[test]
    fn a_fold_is_moving_at_five_real_frames_a_second() {
        let t0 = Instant::now();
        let every = Duration::from_secs(2);
        let mut w = MediaWindow::new(t0);
        assert_eq!(w.fold_if_due(t0 + Duration::from_millis(1999), every), None);
        for _ in 0..10 {
            w.on_real_frame();
            w.on_encoded();
        }
        let f = w.fold_if_due(t0 + every, every).expect("due");
        assert_eq!(
            f,
            FoldReport {
                ms: 2000,
                moving: true,
                frames_encoded: 10,
                frames_keepalive: 0
            }
        );
        for _ in 0..9 {
            w.on_real_frame();
        }
        for _ in 0..3 {
            w.on_keepalive();
        }
        let f = w.fold_if_due(t0 + 2 * every, every).expect("due");
        assert!(!f.moving, "9 frames in 2 s is 4.5 fps: sparse");
        assert_eq!(f.frames_keepalive, 3);
        let f = w
            .fold_if_due(t0 + 2 * every + Duration::from_secs(600), every)
            .expect("due");
        assert_eq!(f.ms, MAX_FOLD.as_millis() as u64);
        assert!(!f.moving);
    }
}
