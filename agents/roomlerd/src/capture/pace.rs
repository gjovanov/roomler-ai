// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3272 — the pump's side of [`ScreenCapture::next_frame`]'s contract: an
//! empty comes after a bounded wait, and nothing loops on one at full speed.
//!
//! The RTP pump is the first to run on this. It took the SYSTEM-context backend
//! straight, with no sleep of its own, and on a still screen it asked about
//! 90,000 times a second: most of a core, and a log flood that crowded the
//! heartbeats out of the uploader.
//!
//! FR-135 R4a (FR-98 C7, #3275) moves the two DataChannel pumps here too, behind
//! the `capture_wait` media path ([`dc_capture_wait`]): off by default, on from a
//! device's own `rc_media_canary`. With it on a pump opens its capture paced,
//! runs [`EmptyGuard::rest`] where it slept on an empty, and lets go of its
//! capture before it reopens one. The opt-in goes when that path has been read
//! in the field (#3275's delete).
//!
//! [`ScreenCapture::next_frame`]: super::ScreenCapture::next_frame

use std::time::{Duration, Instant};

/// The shortest an empty may take. It is what a backend that returns at once is
/// held to (the scrap arm, whose acquire takes a fixed 0, and a pointer-only
/// update), and the pump-side sleep the FFmpeg DC pump has had since rc.99: about
/// 450 asks a second, against 90,000.
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

/// How soon after the last encode an owed keyframe re-encodes a still screen:
/// at most 4 times a second, however often a browser on a lossy link asks.
pub const OWED_KEYFRAME_MIN_GAP: Duration = Duration::from_millis(250);

/// `ROOMLERD_RTP_CAPTURE_WAIT`, default on. Off, the RTP pump does not opt in
/// and arms no guard: the capture and its open run as they did before #3272.
/// Read once per pump (FR-70: no per-frame reads). Env only, by the operator's
/// decision: it is a kill switch for the first release, not a setting
/// (`docs/encoders.md`).
pub fn rtp_capture_wait_enabled() -> bool {
    tunnel_core::env::flag("RTP_CAPTURE_WAIT", true)
}

/// FR-135 R4a (#3275) — does this session's DataChannel pump take
/// [`ScreenCapture::next_frame`]'s contract? Read once per pump, where its
/// capture opens (FR-70: no per-frame reads). `true` opens the capture paced,
/// arms the pump's [`EmptyGuard`], runs [`EmptyGuard::rest`] where the pump
/// slept on an empty, and lets go of the capture before each reopen. `false` is
/// the pump as it has always run, its own sleep on every empty included.
///
/// The switch is the `capture_wait` media path (`crate::media_paths`): off by
/// default, on from the device's own `rc_media_canary`, and sent back by a
/// pushed `rc_media_legacy`. Two kinds of session keep their sleeps with it on,
/// and say so once ([`dc_held_back`]).
///
/// [`ScreenCapture::next_frame`]: super::ScreenCapture::next_frame
pub fn dc_capture_wait(paths: &crate::media_paths::MediaPaths) -> bool {
    let held = dc_held_back(cfg!(target_os = "macos"), gpu_frames_at_open());
    dc_capture_wait_on(paths, held)
}

/// [`dc_capture_wait`] for a session that is `held` back, or is not: the switch
/// first, then the hold.
fn dc_capture_wait_on(paths: &crate::media_paths::MediaPaths, held: Option<&'static str>) -> bool {
    let asked = paths.is_new(crate::media_paths::MediaPath::CaptureWait);
    if asked && let Some(why) = held {
        tracing::info!(
            why,
            "FR-135 R4a: capture_wait is on, and this session's pump keeps its own sleeps"
        );
    }
    asked && held.is_none()
}

/// Why a session keeps its pump's sleeps whatever the switch says.
///
/// - `macos`: its capture polls for a frame until R12 gives it an event wake,
///   and nobody has read that arm on this path (FR-135 §7.5).
/// - `gpu_frames`: its capture delivers frames on the GPU. A capture that waits
///   keeps its frames off the GPU, because the wait holds the device's lock and
///   the encoder reads a GPU frame on that device (`dxgi_direct.rs`), so
///   waiting would turn zero-copy off behind the session's back. It waits once
///   the capture has a device of its own (R5).
fn dc_held_back(macos: bool, gpu_frames: bool) -> Option<&'static str> {
    if macos {
        Some("macos")
    } else if gpu_frames {
        Some("gpu_frames")
    } else {
        None
    }
}

/// Whether a capture opening now delivers GPU frames, on a build that has the
/// Windows GPU form (`gpu_frame::gpu_frames_at_open`: the `zero_copy` switch and
/// FR-132's list). `false` on every other build. macOS has a GPU form of its
/// own, and [`dc_held_back`] rules macOS out before this is read.
fn gpu_frames_at_open() -> bool {
    #[cfg(all(
        target_os = "windows",
        any(feature = "mf-encoder", feature = "wgc-capture")
    ))]
    {
        super::gpu_frame::gpu_frames_at_open()
    }
    #[cfg(not(all(
        target_os = "windows",
        any(feature = "mf-encoder", feature = "wgc-capture")
    )))]
    {
        false
    }
}

/// How often a line that would otherwise run at a pump's ask rate is written.
pub const FLOOD_LINE_EVERY: Duration = Duration::from_secs(5);

/// A log line held to one per `every`, on time, across the process (#3275).
///
/// For a line whose arm runs once per ask. A count of asks is no limit: it runs
/// as fast as the capture does (a `% 150` wrote about 3 lines a second at the
/// floor's 450 asks, where its comment said one per 5 s), and a line with no
/// limit at all wrote 450.
#[derive(Debug)]
pub struct LineEvery {
    every: Duration,
    /// When the line was last due, and how many it has stood in for since.
    state: std::sync::Mutex<(Option<Instant>, u64)>,
}

impl LineEvery {
    pub const fn new(every: Duration) -> Self {
        Self {
            every,
            state: std::sync::Mutex::new((None, 0)),
        }
    }

    /// `Some(skipped)` when the line is due at `now`, with how many it stood in
    /// for since the last one. `None` keeps quiet, and counts.
    pub fn due(&self, now: Instant) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = state.0
            && now.saturating_duration_since(last) < self.every
        {
            state.1 += 1;
            return None;
        }
        let skipped = state.1;
        *state = (Some(now), 0);
        Some(skipped)
    }
}

/// What is left of [`EMPTY_FLOOR`] after an empty that took `took`.
pub fn floor_remaining(took: Duration) -> Duration {
    EMPTY_FLOOR.saturating_sub(took)
}

/// Does a still screen re-encode its last frame now? At the pump's `keepalive`,
/// or sooner for a keyframe that is owed: nothing else answers one on a still
/// screen, and a session whose first frame beat its transport has no picture
/// until one is answered.
pub fn re_encode_due(idle_for: Duration, keepalive: Duration, keyframe_owed: bool) -> bool {
    idle_for >= keepalive || (keyframe_owed && idle_for >= OWED_KEYFRAME_MIN_GAP)
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

    /// FR-135 R4a — a pump's whole rest on an `Ok(None)` that took `took`:
    /// nothing after an empty that waited, [`EMPTY_FLOOR`] after one that came
    /// straight back. It is what a DataChannel pump runs in place of its own
    /// sleep on a session that waits in the capture ([`dc_capture_wait`]).
    pub async fn rest(&mut self, took: Duration, backend: &str) {
        let owed = self.on_empty(took, backend);
        if !owed.is_zero() {
            tokio::time::sleep(owed).await;
        }
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
        // A test double: no display.
        fn refresh_hz(&self) -> Option<u16> {
            None
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
            assert!(capturer.next_frame().await.unwrap().is_none());
            // Not read off the clock: a stall of a millisecond on a busy runner
            // would read as a wait, and that empty rightly owes nothing.
            let floor = guard.on_empty(INSTANT, capturer.backend());
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

    /// An owed keyframe does not wait for the keepalive, and however often a
    /// browser asks, a still screen spends at most 4 keyframes a second on it.
    #[test]
    fn an_owed_keyframe_beats_the_keepalive_and_no_more_than_4_times_a_second() {
        const KEEPALIVE: Duration = Duration::from_secs(1);
        const TICK: Duration = Duration::from_millis(16);
        let just_before = |t: Duration| t - Duration::from_millis(1);

        assert!(!re_encode_due(just_before(KEEPALIVE), KEEPALIVE, false));
        assert!(re_encode_due(KEEPALIVE, KEEPALIVE, false));

        assert!(!re_encode_due(
            just_before(OWED_KEYFRAME_MIN_GAP),
            KEEPALIVE,
            true
        ));
        assert!(re_encode_due(OWED_KEYFRAME_MIN_GAP, KEEPALIVE, true));

        // The pump on a still screen, one ask per acquire wait, a keyframe owed
        // on every one of them, for 10 s.
        let (mut idle, mut encodes) = (Duration::ZERO, 0);
        for _ in 0..(10_000 / TICK.as_millis()) {
            idle += TICK;
            if re_encode_due(idle, KEEPALIVE, true) {
                idle = Duration::ZERO;
                encodes += 1;
            }
        }
        assert!((30..=40).contains(&encodes), "{encodes} in 10 s");
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

    // ----- FR-135 R4a (#3275): the DataChannel pumps on `next_frame`'s contract -----
    //
    // The two pumps are 1,500 and 3,850 lines with a live encoder and a peer
    // connection inside, and each opens its own capture, so no test runs one
    // against a fake capture. The bar is therefore in three parts. A MODEL of a
    // pump's empty arm reads what the pump adds between a frame being ready and
    // the pump picking it up, and how often it asks. Its waiting arm is the
    // helper both pumps call (`EmptyGuard::rest`), its other arm copies their
    // sleeps, and the product's own resolver picks the arm from the two keys.
    // A source test then holds `peer.rs` to that model: the same sleeps, behind
    // the same switch. What the capture itself adds (how fast an acquire
    // returns on a real GPU) is the field A/B's to read, not this file's.

    use crate::media_paths::{self, MediaPath, MediaPaths};

    /// A session's paths as the product resolves them from the two keys' text:
    /// `rc_media_legacy`, then `rc_media_canary`.
    fn paths(legacy: Option<&str>, canary: Option<&str>) -> MediaPaths {
        media_paths::resolve(&media_paths::parse(legacy), &media_paths::parse(canary))
    }

    /// Whether a pump waits in the capture on a session nothing holds back: a
    /// Windows service's, on CPU frames. The switch alone, on every test host.
    fn waits(legacy: Option<&str>, canary: Option<&str>) -> bool {
        dc_capture_wait_on(&paths(legacy, canary), None)
    }

    /// The model runs in slow motion, 1000 to 1: the test clock's timers tick in
    /// whole milliseconds, and a frame may land at any microsecond of a 2 ms
    /// sleep. Every reading is divided back.
    const SLOW: u32 = 1_000;

    /// `system_context::capture_pump::ACQUIRE_WAIT_MS`, which only a Windows
    /// build compiles: how long a paced acquire waits for a frame.
    const ACQUIRE_WAIT: Duration = Duration::from_millis(16);

    /// What `media_pump_ffmpeg_dc` sleeps on an empty with the switch off.
    const FFMPEG_SLEEP: Duration = Duration::from_millis(2);

    /// `frame_duration_floor` as `media_pump_vp9_444_dc` computes it. With the
    /// switch off that pump sleeps half of it on an empty, and all of it before
    /// its first frame.
    fn frame_duration_floor(target_fps: u32) -> Duration {
        Duration::from_micros(1_000_000 / u64::from(target_fps))
    }

    /// The SYSTEM-context capture as a pump meets it, in front of a screen that
    /// presents one frame at `ready_at`, or none. Opened unpaced it answers at
    /// once with what is there, as `AcquireNextFrame(0)` does. Opened paced it
    /// waits for a frame for up to the acquire's 16 ms, and a frame ends the
    /// wait the moment it is presented.
    struct ModelCapture {
        paced: bool,
        ready_at: Option<tokio::time::Instant>,
        asks: u64,
    }

    impl ModelCapture {
        async fn next_frame(&mut self) -> Option<()> {
            self.asks += 1;
            let now = tokio::time::Instant::now();
            if !self.paced {
                return self.ready_at.take_if(|at| *at <= now).map(|_| ());
            }
            let timeout = now + ACQUIRE_WAIT * SLOW;
            match self.ready_at {
                Some(at) if at <= timeout => {
                    tokio::time::sleep_until(at).await;
                    self.ready_at = None;
                    Some(())
                }
                _ => {
                    tokio::time::sleep_until(timeout).await;
                    None
                }
            }
        }
    }

    /// A DataChannel pump's capture loop, down to what this phase changes. It
    /// asks, and on an empty it either rests in the helper both pumps call
    /// (`pace_capture`) or sleeps the pump's own sleep. It runs until the frame
    /// is picked up, and says when that was, or until `stop`.
    async fn pump_model(
        pace_capture: bool,
        off_sleep: Duration,
        capture: &mut ModelCapture,
        stop: tokio::time::Instant,
    ) -> Option<tokio::time::Instant> {
        let mut guard = EmptyGuard::new(pace_capture);
        while tokio::time::Instant::now() < stop {
            let started = tokio::time::Instant::now();
            if capture.next_frame().await.is_some() {
                return Some(tokio::time::Instant::now());
            }
            if pace_capture {
                guard.rest(started.elapsed(), "model").await;
            } else {
                tokio::time::sleep(off_sleep * SLOW).await;
            }
        }
        None
    }

    /// Frame ready to picked up, as (mean, max) over 40 arrival instants spread
    /// evenly across one of the pump's sleeps, so the mean is the mean over
    /// where in the sleep a frame lands.
    async fn ready_to_pickup(pace_capture: bool, off_sleep: Duration) -> (Duration, Duration) {
        const PHASES: u32 = 40;
        let (mut sum, mut max) = (Duration::ZERO, Duration::ZERO);
        for i in 0..PHASES {
            let t0 = tokio::time::Instant::now();
            let ready_at = t0 + off_sleep * SLOW * (2 * i + 1) / (2 * PHASES);
            let mut capture = ModelCapture {
                paced: pace_capture,
                ready_at: Some(ready_at),
                asks: 0,
            };
            let stop = t0 + Duration::from_secs(3_600);
            let picked_up = pump_model(pace_capture, off_sleep, &mut capture, stop)
                .await
                .expect("the frame is picked up");
            let late = (picked_up - ready_at) / SLOW;
            sum += late;
            max = max.max(late);
        }
        (sum / PHASES, max)
    }

    /// B1 — what a pump adds between a frame being ready and picking it up.
    /// With the switch off it is the rest of the pump's sleep: half of it on
    /// average. With `rc_media_canary=capture_wait` the capture waits for the
    /// frame, and the pump adds nothing at any phase.
    #[tokio::test(start_paused = true)]
    async fn a_frame_ready_during_an_empty_is_picked_up_without_a_pump_sleep() {
        let off = waits(None, None);
        let on = waits(None, Some("capture_wait"));
        assert!(!off, "with neither key set a pump runs as it always has");

        // The VP9 pump runs at 60 unless a host pins it to 30
        // (`vp9_444_target_fps`).
        let cells = [
            ("FFmpeg DC pump, its 2 ms sleep", FFMPEG_SLEEP),
            (
                "VP9 DC pump at 60 fps, half a frame",
                frame_duration_floor(60) / 2,
            ),
            (
                "VP9 DC pump at 60 fps, a whole frame (no frame yet)",
                frame_duration_floor(60),
            ),
            (
                "VP9 DC pump pinned to 30 fps, half a frame",
                frame_duration_floor(30) / 2,
            ),
        ];
        let mut misses = Vec::new();
        println!("frame ready to picked up, mean / max in µs over 40 phases of one sleep:");
        for (cell, sleep) in cells {
            let (off_mean, off_max) = ready_to_pickup(off, sleep).await;
            let (on_mean, on_max) = ready_to_pickup(on, sleep).await;
            println!(
                "  {cell}: switch off {} / {}, rc_media_canary=capture_wait {} / {}",
                off_mean.as_micros(),
                off_max.as_micros(),
                on_mean.as_micros(),
                on_max.as_micros(),
            );
            // The off arm is master's: half the sleep on average, and never the
            // whole of it. A model that drifts from that is caught here.
            let want = sleep / 2;
            assert!(
                off_mean.abs_diff(want) <= want / 10 && off_max <= sleep,
                "{cell}: the off arm reads {off_mean:?} / {off_max:?}, and a sleep of {sleep:?} is {want:?} on average"
            );
            if on_max >= Duration::from_micros(500) {
                misses.push(format!(
                    "{cell}: a frame waits {on_mean:?} on average and up to {on_max:?} for the pump"
                ));
            }
        }
        assert!(
            misses.is_empty(),
            "with rc_media_canary=capture_wait the bar is under 0.5 ms at every phase:\n{}",
            misses.join("\n")
        );
    }

    /// How often a pump asks its capture while the screen stays still.
    async fn still_screen_asks(pace_capture: bool, off_sleep: Duration, window: Duration) -> u64 {
        let mut capture = ModelCapture {
            paced: pace_capture,
            ready_at: None,
            asks: 0,
        };
        let stop = tokio::time::Instant::now() + window * SLOW;
        let picked_up = pump_model(pace_capture, off_sleep, &mut capture, stop).await;
        assert!(picked_up.is_none(), "a still screen has no frame");
        capture.asks
    }

    /// B3 — asks per 30 s on a still screen. A field read takes it off the
    /// SYSTEM-context capture's own 30 s line (`calls`), or off a pump's
    /// heartbeat (`frames_captured` + `frames_empty`). With the switch off a
    /// pump asks once per sleep: every 2 ms (FFmpeg), every half frame (VP9,
    /// 8.3 ms at its default 60). With it on both ask once per 16 ms acquire
    /// wait. A VP9 pump pinned to 30 already asks about that often, so there
    /// the gain is B1's and not this one.
    #[tokio::test(start_paused = true)]
    async fn a_still_screen_asks_under_2000_times_in_30_s_on_the_waiting_arm() {
        const WINDOW: Duration = Duration::from_secs(30);
        let off = waits(None, None);
        let on = waits(None, Some("capture_wait"));
        // A pump, its sleep on an empty, and its asks in 30 s with the switch off.
        let cells = [
            ("FFmpeg DC pump", FFMPEG_SLEEP, 15_000),
            ("VP9 DC pump at 60 fps", frame_duration_floor(60) / 2, 3_600),
            (
                "VP9 DC pump pinned to 30 fps",
                frame_duration_floor(30) / 2,
                1_800,
            ),
        ];
        println!("asks in 30 s on a still screen, switch off -> rc_media_canary=capture_wait:");
        for (pump, sleep, master) in cells {
            let was = still_screen_asks(off, sleep, WINDOW).await;
            let now = still_screen_asks(on, sleep, WINDOW).await;
            println!("  {pump}: {was} -> {now}");
            assert!(
                was.abs_diff(master) <= master / 100,
                "{pump}: master asks once per sleep of {sleep:?}, {master} in 30 s, and the model reads {was}"
            );
            assert!(
                (1_800..=2_000).contains(&now),
                "{pump} with rc_media_canary=capture_wait: {now} asks in 30 s, and one per 16 ms acquire wait is 1,875"
            );
        }
    }

    /// The same reading as B1 on a real clock, with a real thread standing in
    /// for the capture thread and another for the screen: `late` per frame.
    async fn real_clock_cell(
        pace_capture: bool,
        off_sleep: Duration,
        frames: usize,
    ) -> Vec<Duration> {
        use std::collections::VecDeque;
        use std::sync::mpsc;
        type Ask = tokio::sync::oneshot::Sender<Option<Instant>>;
        let (present_tx, present_rx) = mpsc::channel::<Instant>();
        let (ask_tx, ask_rx) = mpsc::channel::<Ask>();
        // The screen: a frame every 20 to 37 ms, at a phase that walks.
        let screen = std::thread::spawn(move || {
            for i in 0..frames as u64 {
                std::thread::sleep(Duration::from_micros(20_000 + (i * 7_919) % 17_000));
                if present_tx.send(Instant::now()).is_err() {
                    break;
                }
            }
        });
        // The capture thread, as the SYSTEM-context worker answers an ask.
        let capture = std::thread::spawn(move || {
            let mut ready: VecDeque<Instant> = VecDeque::new();
            while let Ok(reply) = ask_rx.recv() {
                ready.extend(present_rx.try_iter());
                if ready.is_empty()
                    && pace_capture
                    && let Ok(at) = present_rx.recv_timeout(ACQUIRE_WAIT)
                {
                    ready.push_back(at);
                }
                let _ = reply.send(ready.pop_front());
            }
        });
        let mut guard = EmptyGuard::new(pace_capture);
        let mut late = Vec::with_capacity(frames);
        while late.len() < frames {
            let started = Instant::now();
            let (tx, rx) = tokio::sync::oneshot::channel();
            ask_tx.send(tx).expect("the capture thread is up");
            match rx.await.expect("the capture thread answers") {
                Some(presented_at) => {
                    late.push(presented_at.elapsed());
                    guard.on_frame();
                }
                None if pace_capture => guard.rest(started.elapsed(), "model").await,
                None => tokio::time::sleep(off_sleep).await,
            }
        }
        drop(ask_tx);
        capture.join().expect("the capture thread ends");
        screen.join().expect("the screen thread ends");
        late
    }

    /// B1's reading on a real clock and real threads, for the numbers a person
    /// reads. It is not a gate, because a wall-clock bound flakes on a loaded
    /// runner: `cargo test -p roomlerd --lib frame_ready_to_pickup_on_a_real_clock
    /// -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "wall-clock numbers for a person to read, not a CI gate"]
    async fn frame_ready_to_pickup_on_a_real_clock() {
        const FRAMES: usize = 200;
        let cells = [
            ("FFmpeg DC pump, its 2 ms sleep", FFMPEG_SLEEP),
            (
                "VP9 DC pump at 60 fps, half a frame",
                frame_duration_floor(60) / 2,
            ),
        ];
        let arms = [
            ("switch off", waits(None, None)),
            (
                "rc_media_canary=capture_wait",
                waits(None, Some("capture_wait")),
            ),
        ];
        for (cell, sleep) in cells {
            for (arm, pace_capture) in arms {
                let mut late = real_clock_cell(pace_capture, sleep, FRAMES).await;
                late.sort();
                let mean = late.iter().sum::<Duration>() / FRAMES as u32;
                println!(
                    "{cell}, {arm}: mean {} µs, p50 {} µs, p95 {} µs, max {} µs over {FRAMES} frames",
                    mean.as_micros(),
                    late[FRAMES / 2].as_micros(),
                    late[FRAMES * 95 / 100].as_micros(),
                    late[FRAMES - 1].as_micros(),
                );
            }
        }
    }

    /// `text` as code: its comment lines dropped and all white space taken out,
    /// so a match holds however rustfmt wraps a line.
    fn code_of(text: &str) -> String {
        text.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .flat_map(str::chars)
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// The body of `async fn <name>(` in `peer.rs` as compiled, to its closing
    /// brace at column 0, with `\n` line ends.
    fn pump_body(name: &str) -> String {
        let src = include_str!("../peer.rs").replace("\r\n", "\n");
        let head = format!("async fn {name}(");
        assert_eq!(src.matches(&head).count(), 1, "one {head:?} in peer.rs");
        let rest = &src[src.find(&head).unwrap()..];
        rest[..rest.find("\n}\n").expect("the fn's end")].to_owned()
    }

    /// The `Ok(None)` arm of a pump's capture match, as code.
    fn empty_arm(body: &str) -> String {
        const ARM: &str = "\n            Ok(None) => {\n";
        assert_eq!(body.matches(ARM).count(), 1, "one empty arm in the pump");
        let rest = &body[body.find(ARM).unwrap()..];
        code_of(
            &rest[..rest
                .find("\n            Err(e) => {\n")
                .expect("the arm after it")],
        )
    }

    /// B2 — the model above is the pumps, read from `peer.rs` as compiled.
    ///
    /// In each DataChannel pump's `Ok(None)` arm every sleep stands behind the
    /// switch's early exit, which rests in [`EmptyGuard::rest`] and loops, so
    /// with the switch on no sleep of the pump's own is reachable. Behind it
    /// each sleep is the one master has. `pace_capture` is read once, from
    /// [`dc_capture_wait`], and it arms the guard, opens every capture and lets
    /// go of the old one before each reopen: a paced open waits for a seat, and
    /// it would be waiting on the seat this pump holds. In the FFmpeg pump that
    /// let-go sits inside the reopen timer, where the assignment dropped the
    /// old capture, so `recapture` times the same work on both arms. Both
    /// heartbeats end with the arm a line came from.
    #[test]
    fn with_the_switch_on_no_dc_pump_sleeps_on_an_empty() {
        const OPEN: &str = "capture::open_async(target_fps, downscale, pace_capture,";
        const LET_GO: &str = "if pace_capture { drop(capturer); }";
        const BACKOFF: &str = "tokio::time::sleep(reopen_backoff.delay()).await;";
        const REST_VP9: &str = "if pace_capture { \
            empty_guard.rest(capture_started.elapsed(), capturer.backend()).await; \
            continue; }";
        const REST_FFMPEG: &str = "if pace_capture { \
            let t = watch.start(); \
            empty_guard.rest(capture_start.elapsed(), capturer.backend()).await; \
            PassWatch::stop(t, &mut poll_us); \
            continue; }";
        const SLEEP_FFMPEG: &str = "let t = watch.start(); \
            tokio::time::sleep(Duration::from_millis(2)).await; \
            PassWatch::stop(t, &mut poll_us); \
            continue;";

        let mut misses = Vec::new();
        let mut want = |pump: &str, hay: &str, n: usize, what: &str| {
            let found = hay.matches(&code_of(what)).count();
            if found != n {
                misses.push(format!("{pump}: {found} of `{what}`, want {n}"));
            }
        };

        let body = pump_body("media_pump_vp9_444_dc");
        let (code, arm) = (code_of(&body), empty_arm(&body));
        let pump = "media_pump_vp9_444_dc";
        want(pump, &arm, 2, "tokio::time::sleep(");
        for sleep in [
            "tokio::time::sleep(frame_duration_floor).await; continue;",
            "tokio::time::sleep(frame_duration_floor / 2).await; continue;",
        ] {
            want(pump, &arm, 1, &format!("{REST_VP9} {sleep}"));
        }
        want(
            pump,
            &code,
            1,
            "let capture_started = std::time::Instant::now(); let next_frame = capturer.next_frame().await;",
        );
        want(pump, &code, 1, &format!("let mut capturer = {OPEN}"));
        want(
            pump,
            &code,
            1,
            &format!("{LET_GO} {BACKOFF} capturer = {OPEN}"),
        );
        want(pump, &code, 2, "capture::open_async(");
        want(pump, &code, 1, "frames_empty += 1;");
        want(
            pump,
            &code,
            1,
            "frames_empty, capture_wait = pace_capture, \"VP9-444 DC pump heartbeat (≈1s window)\"",
        );
        let vp9 = code;

        let body = pump_body("media_pump_ffmpeg_dc");
        let (code, arm) = (code_of(&body), empty_arm(&body));
        let pump = "media_pump_ffmpeg_dc";
        want(pump, &arm, 1, "tokio::time::sleep(");
        want(pump, &arm, 1, &format!("{REST_FFMPEG} {SLEEP_FFMPEG}"));
        want(pump, &code, 1, &format!("let mut capturer = {OPEN}"));
        want(
            pump,
            &code,
            1,
            &format!("let t = watch.start(); {LET_GO} capturer = {OPEN}"),
        );
        want(
            pump,
            &code,
            1,
            &format!("let t = watch.start(); {LET_GO} {BACKOFF} capturer = {OPEN}"),
        );
        want(pump, &code, 3, "capture::open_async(");
        want(
            pump,
            &code,
            1,
            "capture_wait = pace_capture, \"FFmpeg DC pump heartbeat (≈2s window)\"",
        );

        for (pump, code) in [
            ("media_pump_vp9_444_dc", &vp9),
            ("media_pump_ffmpeg_dc", &code),
        ] {
            want(
                pump,
                code,
                1,
                "let pace_capture = capture::pace::dc_capture_wait(&media_paths);",
            );
            want(
                pump,
                code,
                1,
                "let mut empty_guard = capture::pace::EmptyGuard::new(pace_capture);",
            );
            want(pump, code, 1, "empty_guard.on_frame();");
            want(pump, code, 0, "open_default_async(");
        }

        // And the open the off arm runs is the one it ran: `open_default_async`
        // was `open_async` unpaced, which is what `pace_capture` false asks for.
        let capture_rs = code_of(&include_str!("mod.rs").replace("\r\n", "\n"));
        want(
            "capture/mod.rs",
            &capture_rs,
            1,
            "open_async(target_fps, downscale, false, on_wait).await",
        );

        assert!(
            misses.is_empty(),
            "peer.rs is not the model this file measures:\n{}",
            misses.join("\n")
        );
    }

    /// `rest` is `on_empty` and the sleep it owes, in one call: nothing after an
    /// empty that waited, the floor after one that came straight back, and
    /// nothing at all from a guard that is not armed.
    #[tokio::test(start_paused = true)]
    async fn rest_sleeps_the_floor_after_an_instant_empty_and_nothing_after_a_wait() {
        let t0 = tokio::time::Instant::now();
        let mut guard = EmptyGuard::new(true);
        guard.rest(ACQUIRE_WAIT, "x").await;
        assert_eq!(t0.elapsed(), Duration::ZERO, "an empty that waited");
        guard.rest(INSTANT, "x").await;
        assert_eq!(
            t0.elapsed(),
            EMPTY_FLOOR,
            "an empty that came straight back"
        );
        EmptyGuard::new(false).rest(INSTANT, "x").await;
        assert_eq!(t0.elapsed(), EMPTY_FLOOR, "a guard that is not armed");
    }

    /// The switch: off with neither key, on from the device's own canary key,
    /// and a pushed legacy entry sends the session back and wins. No other
    /// path's name turns it on.
    #[test]
    fn the_dc_pumps_wait_only_where_the_device_s_canary_asks() {
        assert!(!waits(None, None));
        assert!(waits(None, Some("capture_wait")));
        assert!(waits(None, Some(" media_plan, capture_wait ")));
        assert!(!waits(Some("capture_wait"), Some("capture_wait")));
        assert!(!waits(None, Some("capture_stage,capture_waits,none")));
        // And on this host as it is, the hold counts too.
        let held = dc_held_back(cfg!(target_os = "macos"), gpu_frames_at_open());
        assert_eq!(
            dc_capture_wait(&paths(None, Some("capture_wait"))),
            held.is_none()
        );
        assert!(!dc_capture_wait(&MediaPaths::default()));
        assert!(!MediaPaths::default().is_new(MediaPath::CaptureWait));
    }

    /// The two sessions that keep their pump's sleeps whatever the switch says:
    /// a Mac's, and one whose capture hands out GPU frames.
    #[test]
    fn a_mac_and_a_gpu_form_session_keep_the_pump_s_sleeps() {
        assert_eq!(dc_held_back(false, false), None);
        assert_eq!(dc_held_back(false, true), Some("gpu_frames"));
        assert_eq!(dc_held_back(true, false), Some("macos"));
        assert_eq!(dc_held_back(true, true), Some("macos"));
        let asked = paths(None, Some("capture_wait"));
        assert!(dc_capture_wait_on(&asked, None));
        assert!(!dc_capture_wait_on(&asked, Some("gpu_frames")));
        assert!(!dc_capture_wait_on(&asked, Some("macos")));
        assert!(!dc_capture_wait_on(&paths(None, None), None));
    }

    /// #3275 — a line whose arm runs once per ask is written once per interval,
    /// on time, and says how many it stood in for.
    #[test]
    fn a_line_at_the_ask_rate_is_written_once_per_interval() {
        let line = LineEvery::new(FLOOD_LINE_EVERY);
        let t0 = Instant::now();
        assert_eq!(line.due(t0), Some(0), "the first one is written");
        // The floor's rate, an ask every 2 ms, for the rest of the interval.
        let asks = (FLOOD_LINE_EVERY.as_millis() / EMPTY_FLOOR.as_millis()) as u32;
        let written = (1..asks)
            .filter(|i| line.due(t0 + EMPTY_FLOOR * *i).is_some())
            .count();
        assert_eq!(written, 0, "{asks} asks in the interval");
        assert_eq!(line.due(t0 + FLOOD_LINE_EVERY), Some(u64::from(asks) - 1));
        assert_eq!(line.due(t0 + FLOOD_LINE_EVERY * 2), Some(0));
        // A clock read before the last line is no line and no panic.
        assert_eq!(line.due(t0), None);
    }
}
