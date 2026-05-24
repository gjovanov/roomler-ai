//! Deterministic synthetic frame source for headless CI / Linux Pods.
//!
//! Emits a 320×240 BGRA frame at the requested fps cap with a slow
//! horizontal-gradient + frame-counter overlay so an encoder sees real
//! pixel deltas (not a constant black image — which several SW encoders
//! optimise into zero-byte skip frames, defeating the whole point of
//! exercising the encode pipeline in a test).
//!
//! Why a separate backend instead of running Xvfb in the Pod:
//! * Zero system deps — no `xvfb`, `xterm`, `libxcb*`. The agent-e2e
//!   Docker image stays under 100 MiB instead of dragging the X11
//!   stack in.
//! * Deterministic — the same frame counter produces the same pixels.
//!   Test assertions on `framesDecoded ≥ N` (Phase 2) have a fixed
//!   reference instead of whatever Xvfb's `xterm` happened to paint.
//! * No SCM-context surprise: `scrap` on Linux talks to an X server,
//!   which inside a Pod means `Xvfb :99` running on the same Pod.
//!   `synthetic` just produces bytes — no display dependency at all.
//!
//! Selected by setting `ROOMLER_AGENT_SYNTHETIC_FRAMES=1` at runtime
//! AND building with the `synthetic-frame-source` feature. Production
//! agents (`full`, `full-hw` feature sets) never opt in.

use crate::capture::{DownscalePolicy, Frame, PixelFormat, ScreenCapture};
use anyhow::Result;
use std::time::{Duration, Instant};

/// Synthetic frame dimensions. Small enough that openh264 encodes
/// each frame in <5 ms on a 2-vCPU CI runner; large enough that the
/// resulting H.264 bitstream is meaningfully testable.
pub const FRAME_W: u32 = 320;
pub const FRAME_H: u32 = 240;
pub const FRAME_STRIDE: u32 = FRAME_W * 4; // BGRA = 4 bytes/pixel

/// Default frame cadence — 15 fps is plenty for a smoke test and
/// keeps CPU low under CI's vCPU constraints.
pub const DEFAULT_FPS: u32 = 15;

pub struct SyntheticCapture {
    /// Wall-clock frame interval (1 / fps).
    interval: Duration,
    /// Start instant — `next_frame` computes a per-frame deadline as
    /// `start + (counter * interval)` and sleeps until then. This
    /// matches scrap_backend's "rate-limit so we don't burn CPU"
    /// posture.
    start: Instant,
    /// Monotonically increasing frame counter — also painted into
    /// the frame (small pixel-pattern in the top-left) so tests
    /// asserting "we got frame N" have a deterministic reference.
    counter: u64,
}

impl SyntheticCapture {
    pub fn new(target_fps: u32) -> Self {
        let fps = target_fps.clamp(1, 60);
        Self {
            interval: Duration::from_micros(1_000_000 / fps as u64),
            start: Instant::now(),
            counter: 0,
        }
    }

    /// Render a single BGRA frame for the given counter value. Pure
    /// over the input — same counter always produces identical bytes.
    /// Pattern: horizontal gradient that shifts one pixel per frame,
    /// plus a 16-pixel "counter strip" in the top-left where each
    /// pixel column encodes one bit of the counter (so a test can
    /// decode `counter` from the BGRA bytes if it wants).
    pub fn render(counter: u64) -> Vec<u8> {
        let mut data = vec![0u8; (FRAME_W * FRAME_H * 4) as usize];
        let shift = (counter % FRAME_W as u64) as u32;
        for y in 0..FRAME_H {
            for x in 0..FRAME_W {
                let i = ((y * FRAME_W + x) * 4) as usize;
                let gx = (x + shift) % FRAME_W;
                // Diagonal gradient that wraps — the shift gives motion.
                let r = ((gx * 255) / FRAME_W) as u8;
                let g = ((y * 255) / FRAME_H) as u8;
                let b = (((gx + y) * 127) / (FRAME_W + FRAME_H)) as u8;
                // BGRA byte order.
                data[i] = b;
                data[i + 1] = g;
                data[i + 2] = r;
                data[i + 3] = 0xFF;
            }
        }
        // Counter strip: top-left 64×4 pixels, white = 1 bit, black = 0.
        // 64 bits of counter, MSB at x=0. A test that wants the
        // counter back reads the top-left 64 pixels' B channel.
        for bit in 0..64u32 {
            let on = (counter >> (63 - bit)) & 1 == 1;
            let colour = if on { 0xFF } else { 0x00 };
            for dy in 0..4u32 {
                for dx in 0..1u32 {
                    let x = bit + dx;
                    let y = dy;
                    let i = ((y * FRAME_W + x) * 4) as usize;
                    data[i] = colour;
                    data[i + 1] = colour;
                    data[i + 2] = colour;
                    data[i + 3] = 0xFF;
                }
            }
        }
        data
    }
}

#[async_trait::async_trait]
impl ScreenCapture for SyntheticCapture {
    async fn next_frame(&mut self) -> Result<Option<Frame>> {
        // Sleep until this frame's nominal emission time. Computed
        // from the start instant + counter * interval so we don't
        // drift on a host where the encoder takes variable time per
        // frame (the deadline stays anchored to wall-clock, not to
        // "previous frame returned at X").
        let deadline = self.start + self.interval * (self.counter as u32);
        let now = Instant::now();
        if deadline > now {
            tokio::time::sleep(deadline - now).await;
        }
        let data = Self::render(self.counter);
        let frame = Frame {
            width: FRAME_W,
            height: FRAME_H,
            stride: FRAME_STRIDE,
            pixel_format: PixelFormat::Bgra,
            data,
            monotonic_us: (now - self.start).as_micros() as u64,
            monitor: 0,
            dirty_rects: Vec::new(), // unknown = full-frame, matches scrap
        };
        self.counter += 1;
        Ok(Some(frame))
    }

    fn monitor_count(&self) -> u8 {
        1
    }
}

/// Construct the synthetic capture honouring the same `target_fps` +
/// `_downscale` plumbing as scrap_backend / wgc_backend. Downscale is
/// ignored — the synthetic frames are already 320×240, well under any
/// downscale threshold.
pub fn primary(target_fps: u32, _downscale: DownscalePolicy) -> SyntheticCapture {
    SyntheticCapture::new(target_fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_320x240_bgra() {
        let data = SyntheticCapture::render(0);
        assert_eq!(data.len(), (FRAME_W * FRAME_H * 4) as usize);
        // Alpha channel must be full.
        for chunk in data.chunks(4) {
            assert_eq!(chunk[3], 0xFF);
        }
    }

    #[test]
    fn render_is_deterministic_for_same_counter() {
        // Pure-over-counter contract: a test asserting "we got frame
        // N" needs the same bytes every time. A regression that
        // accidentally pulled in `rand::random()` for noise would
        // break this.
        let a = SyntheticCapture::render(42);
        let b = SyntheticCapture::render(42);
        assert_eq!(a, b);
    }

    #[test]
    fn consecutive_frames_differ() {
        // The shift-per-frame gives the encoder real pixel delta to
        // chew on. A regression that pinned `shift = 0` would yield
        // identical consecutive frames and an SW encoder would emit
        // zero-byte skip frames after the first IDR — defeating the
        // whole point of synthetic capture in CI.
        let f0 = SyntheticCapture::render(0);
        let f1 = SyntheticCapture::render(1);
        assert_ne!(f0, f1);
    }

    #[test]
    fn counter_strip_round_trips_via_b_channel() {
        // Decode the top-left strip and confirm it represents the
        // counter we passed in. Tests can rely on this to assert
        // frame ordering even after the bitstream has been encoded
        // + decoded round-trip.
        let counter: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let data = SyntheticCapture::render(counter);
        let mut recovered: u64 = 0;
        for bit in 0..64u32 {
            // Top-left row 0, column = bit. B channel is byte 0 of
            // the BGRA quad.
            let i = ((0 * FRAME_W + bit) * 4) as usize;
            if data[i] == 0xFF {
                recovered |= 1 << (63 - bit);
            }
        }
        assert_eq!(recovered, counter);
    }

    #[test]
    fn monitor_count_is_one() {
        let cap = SyntheticCapture::new(15);
        assert_eq!(cap.monitor_count(), 1);
    }

    #[tokio::test]
    async fn next_frame_returns_a_full_size_frame() {
        let mut cap = SyntheticCapture::new(60); // fast for the test
        let f = cap
            .next_frame()
            .await
            .expect("no error")
            .expect("a frame, not None");
        assert_eq!(f.width, FRAME_W);
        assert_eq!(f.height, FRAME_H);
        assert_eq!(f.stride, FRAME_STRIDE);
        assert_eq!(f.pixel_format, PixelFormat::Bgra);
        assert_eq!(f.data.len(), (FRAME_W * FRAME_H * 4) as usize);
        assert_eq!(f.monitor, 0);
    }

    #[tokio::test]
    async fn fps_is_approximately_honoured() {
        // Rate-limit contract: 30 fps means ~33 ms between frames.
        // We don't pin tightly because CI vCPU jitter can shave/pad
        // by a few ms; ≥20 ms between two consecutive frames is the
        // smallest interval that proves we're not just emitting
        // them as fast as possible.
        let mut cap = SyntheticCapture::new(30);
        let _ = cap.next_frame().await.unwrap();
        let t0 = Instant::now();
        let _ = cap.next_frame().await.unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(20),
            "expected ≥20 ms between frames at 30 fps; got {elapsed:?}"
        );
    }
}
