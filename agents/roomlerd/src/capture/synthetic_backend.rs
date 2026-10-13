// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Deterministic synthetic frame source for headless CI / Linux Pods.
//!
//! Emits a BGRA frame (320×240 unless told otherwise) at the requested fps
//! cap with a slow horizontal-gradient + frame-counter overlay so an encoder
//! sees real pixel deltas (not a constant black image — which several SW
//! encoders optimise into zero-byte skip frames, defeating the whole point
//! of exercising the encode pipeline in a test).
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
//! Selected by setting `ROOMLERD_SYNTHETIC_FRAMES=1` at runtime
//! AND building with the `synthetic-frame-source` feature. Production
//! agents (`full`, `full-hw` feature sets) never opt in.
//!
//! ## FR-16 L0/L4 — the latency bench
//!
//! Every frame also carries a [`marker`]: its sequence number painted as
//! 16×16 luma blocks that survive 4:2:0, a high QP and a 4× downscale, read
//! back from the DECODED picture. The instant each frame was captured is kept
//! in [`clock`], so a test in the same process measures capture→decode on ONE
//! clock. The knobs, all read per session:
//!
//! | Env | Effect |
//! |---|---|
//! | `ROOMLERD_SYNTHETIC_SIZE=WxH` | frame size (even, 64×64 … 3840×2160); default 320×240 |
//! | `ROOMLERD_BENCH=1` | bench mode: the fps clamp rises from 60 to 240, and the gate below is honoured |
//! | `ROOMLERD_SYNTHETIC_GATE_MS=N` | bench only: hold each frame N ms after it is stamped, a planted capture-stage delay. The L0 test's negative control: a harness that cannot see it is not measuring latency |

use crate::capture::{Damage, DirtyRect, DownscalePolicy, Frame, PixelFormat, ScreenCapture};
use anyhow::Result;
use std::time::{Duration, Instant};
use tunnel_core::env::node_env;

/// Default synthetic frame dimensions. Small enough that openh264 encodes
/// each frame in <5 ms on a 2-vCPU CI runner; large enough that the
/// resulting H.264 bitstream is meaningfully testable.
pub const FRAME_W: u32 = 320;
pub const FRAME_H: u32 = 240;
pub const FRAME_STRIDE: u32 = FRAME_W * 4; // BGRA = 4 bytes/pixel

/// Default frame cadence — 15 fps is plenty for a smoke test and
/// keeps CPU low under CI's vCPU constraints.
pub const DEFAULT_FPS: u32 = 15;

/// The fps clamp outside bench mode, and inside it.
const MAX_FPS: u32 = 60;
const MAX_FPS_BENCH: u32 = 240;

/// The largest synthetic frame (`ROOMLERD_SYNTHETIC_SIZE`).
const MAX_W: u32 = 3840;
const MAX_H: u32 = 2160;
const MIN_SIDE: u32 = 64;

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
    width: u32,
    height: u32,
    /// FR-16 L0 — a planted delay between stamping a frame and handing it
    /// over (bench mode only). Zero in every real run.
    gate: Duration,
}

/// L0's planted gate, set by a test in the same process (µs; bench mode
/// only). An env var cannot carry it: the gate changes between cells of one
/// test binary, and changing the environment while webrtc's threads run is
/// unsound.
static TEST_GATE_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set (or clear, with zero) the planted capture-stage delay every NEW
/// synthetic session uses. Honoured in bench mode only, like
/// `ROOMLERD_SYNTHETIC_GATE_MS`, which it overrides.
pub fn set_test_gate(gate: Duration) {
    TEST_GATE_US.store(
        gate.as_micros().min(5_000_000) as u64,
        std::sync::atomic::Ordering::Release,
    );
}

/// `ROOMLERD_BENCH` — truthy the way every other agent switch is.
fn bench_enabled() -> bool {
    matches!(
        node_env("BENCH").as_deref().map(str::trim),
        Some(v) if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
            || v.eq_ignore_ascii_case("on")
    )
}

/// `WxH`, both even and in range, or `None` (the caller keeps the default).
fn parse_size(raw: &str) -> Option<(u32, u32)> {
    let (w, h) = raw.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if !(MIN_SIDE..=MAX_W).contains(&w) || !(MIN_SIDE..=MAX_H).contains(&h) {
        return None;
    }
    // Every encoder this source feeds wants 4:2:0-friendly even sides.
    Some((w & !1, h & !1))
}

impl SyntheticCapture {
    pub fn new(target_fps: u32) -> Self {
        let bench = bench_enabled();
        let fps = target_fps.clamp(1, if bench { MAX_FPS_BENCH } else { MAX_FPS });
        let (width, height) = node_env("SYNTHETIC_SIZE")
            .as_deref()
            .and_then(parse_size)
            .unwrap_or((FRAME_W, FRAME_H));
        let gate = if bench {
            match TEST_GATE_US.load(std::sync::atomic::Ordering::Acquire) {
                0 => node_env("SYNTHETIC_GATE_MS")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(|ms| Duration::from_millis(ms.min(5_000)))
                    .unwrap_or(Duration::ZERO),
                us => Duration::from_micros(us),
            }
        } else {
            Duration::ZERO
        };
        Self {
            interval: Duration::from_micros(1_000_000 / fps as u64),
            start: Instant::now(),
            counter: 0,
            width,
            height,
            gate,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Render a single default-size BGRA frame for the given counter value.
    /// Pure over the input — same counter always produces identical bytes.
    pub fn render(counter: u64) -> Vec<u8> {
        Self::render_sized(counter, FRAME_W, FRAME_H)
    }

    /// [`Self::render`] at any size. Pattern: horizontal gradient that shifts
    /// one pixel per frame, plus a 64-pixel "counter strip" in the top-left
    /// where each pixel column encodes one bit of the counter, plus the L4
    /// [`marker`] in the bottom-left.
    pub fn render_sized(counter: u64, width: u32, height: u32) -> Vec<u8> {
        Self::render_frame(counter, width, height, None)
    }

    /// [`Self::render_sized`], plus the last bench probe in the bottom-right
    /// marker when there is one (FR-16 L4). Pure over its inputs.
    pub fn render_frame(counter: u64, width: u32, height: u32, probe: Option<u16>) -> Vec<u8> {
        let mut data = vec![0u8; (width * height * 4) as usize];
        let shift = (counter % width as u64) as u32;
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let gx = (x + shift) % width;
                // Diagonal gradient that wraps — the shift gives motion.
                let r = ((gx * 255) / width) as u8;
                let g = ((y * 255) / height) as u8;
                let b = (((gx + y) * 127) / (width + height)) as u8;
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
        for bit in 0..64u32.min(width) {
            let on = (counter >> (63 - bit)) & 1 == 1;
            let colour = if on { 0xFF } else { 0x00 };
            for y in 0..4u32.min(height) {
                let i = ((y * width + bit) * 4) as usize;
                data[i] = colour;
                data[i + 1] = colour;
                data[i + 2] = colour;
                data[i + 3] = 0xFF;
            }
        }
        marker::paint_bgra(&mut data, width, height, width * 4, marker::seq_of(counter));
        if let Some(seq) = probe {
            marker::paint_bgra_at(
                marker::Corner::BottomRight,
                &mut data,
                width,
                height,
                width * 4,
                seq,
            );
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
        let data = Self::render_frame(self.counter, self.width, self.height, probe::latest());
        // L0 — the capture instant, for a same-process test to read back by
        // the sequence number the marker carries.
        let captured = Instant::now();
        clock::record(marker::seq_of(self.counter), captured);
        if !self.gate.is_zero() {
            tokio::time::sleep(self.gate).await;
        }
        let frame = Frame {
            width: self.width,
            height: self.height,
            stride: self.width * 4,
            pixel_format: PixelFormat::Bgra,
            data,
            monotonic_us: (captured - self.start).as_micros() as u64,
            monitor: 0,
            // P8a — the shifting gradient repaints EVERY pixel each
            // frame, so the honest tracked damage is the full frame.
            // Also serves as the e2e oracle for the tracked path.
            damage: Damage::Tracked(vec![DirtyRect {
                x: 0,
                y: 0,
                w: self.width,
                h: self.height,
            }]),
            source: None,
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
/// ignored — the synthetic frames are sized by `ROOMLERD_SYNTHETIC_SIZE`.
pub fn primary(target_fps: u32, _downscale: DownscalePolicy) -> SyntheticCapture {
    SyntheticCapture::new(target_fps)
}

/// FR-16 L4 — the frame marker: a 12-bit sequence number and a CRC-4,
/// painted as a 2×8 grid of 16×16 blocks in the bottom-left corner, inside
/// an 8 px quiet margin.
///
/// Why this shape: the old 1-px counter strip does not survive a lossy
/// encode. A 16×16 block of pure black or white (luma 16 / 235 after the
/// BT.601 conversion) does, through 4:2:0, a high QP and a 4× downscale,
/// and its centre still averages far from the 125 threshold. The CRC turns
/// a misread into a miss instead of a wrong number. The browser's decoder
/// reads the same layout (FR-16 L4); this module is the reference.
pub mod marker {
    pub const BLOCK: u32 = 16;
    pub const COLS: u32 = 8;
    pub const ROWS: u32 = 2;
    pub const MARGIN: u32 = 8;
    /// The grid, without its margin.
    pub const W: u32 = BLOCK * COLS;
    pub const H: u32 = BLOCK * ROWS;
    /// Luma threshold between a 0 block and a 1 block.
    const THRESHOLD: u32 = 125;
    /// A block whose centre averages inside this band around the threshold
    /// is unreadable, not a guess.
    const DEAD_BAND: u32 = 40;
    /// The downscale factors a decoder tries, smallest first.
    const SCALES: [u32; 3] = [1, 2, 4];

    /// The sequence number a frame counter carries (12 bits, wrapping).
    pub fn seq_of(counter: u64) -> u16 {
        (counter & 0x0FFF) as u16
    }

    /// CRC-4/ITU (x⁴ + x + 1, init 0) over the 12 bits, MSB first.
    pub fn crc4(seq: u16) -> u8 {
        let mut crc: u8 = 0;
        for i in (0..12).rev() {
            let bit = ((seq >> i) & 1) as u8;
            let top = (crc >> 3) & 1;
            crc = (crc << 1) & 0x0F;
            if top ^ bit == 1 {
                crc ^= 0x03;
            }
        }
        crc
    }

    /// The 16-bit word painted: sequence (12 bits) then CRC (4 bits).
    pub fn word(seq: u16) -> u16 {
        let s = seq & 0x0FFF;
        (s << 4) | crc4(s) as u16
    }

    /// Which corner a marker sits in. The frame's own sequence number is
    /// bottom-left; the last bench probe the source saw is bottom-right
    /// (FR-16 L4, [`super::probe`]).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Corner {
        BottomLeft,
        BottomRight,
    }

    /// The bottom-left grid's top-left corner in a `w × h` frame, or `None`
    /// when the frame is too small to hold it with its margin.
    pub fn origin(w: u32, h: u32) -> Option<(u32, u32)> {
        origin_at(Corner::BottomLeft, w, h)
    }

    /// [`origin`] for either corner. The bottom-right grid needs room for
    /// both, so the two can never overlap.
    pub fn origin_at(corner: Corner, w: u32, h: u32) -> Option<(u32, u32)> {
        let need_w = match corner {
            Corner::BottomLeft => W + 2 * MARGIN,
            Corner::BottomRight => 2 * (W + 2 * MARGIN),
        };
        if w < need_w || h < H + 2 * MARGIN {
            return None;
        }
        let x = match corner {
            Corner::BottomLeft => MARGIN,
            Corner::BottomRight => w - MARGIN - W,
        };
        Some((x, h - MARGIN - H))
    }

    /// Paint `seq` into a BGRA buffer's bottom-left corner, the quiet margin
    /// as mid-grey.
    pub fn paint_bgra(data: &mut [u8], w: u32, h: u32, stride: u32, seq: u16) {
        paint_bgra_at(Corner::BottomLeft, data, w, h, stride, seq);
    }

    /// [`paint_bgra`] into either corner.
    pub fn paint_bgra_at(corner: Corner, data: &mut [u8], w: u32, h: u32, stride: u32, seq: u16) {
        let Some((ox, oy)) = origin_at(corner, w, h) else {
            return;
        };
        let word = word(seq);
        for y in (oy - MARGIN)..(oy + H + MARGIN) {
            for x in (ox - MARGIN)..(ox + W + MARGIN) {
                let inside = x >= ox && x < ox + W && y >= oy && y < oy + H;
                let v = if inside {
                    let bit = ((y - oy) / BLOCK) * COLS + (x - ox) / BLOCK;
                    if (word >> (15 - bit)) & 1 == 1 {
                        0xFF
                    } else {
                        0x00
                    }
                } else {
                    0x80
                };
                let i = (y * stride + x * 4) as usize;
                data[i] = v;
                data[i + 1] = v;
                data[i + 2] = v;
                data[i + 3] = 0xFF;
            }
        }
    }

    /// Read the sequence number back from a decoded luma plane, trying each
    /// downscale factor in [`SCALES`]. `None` when no factor yields a grid
    /// whose every block is readable and whose CRC matches.
    pub fn decode_luma(y: &[u8], w: u32, h: u32, stride: usize) -> Option<u16> {
        decode_luma_at(Corner::BottomLeft, y, w, h, stride)
    }

    /// [`decode_luma`] for either corner.
    pub fn decode_luma_at(corner: Corner, y: &[u8], w: u32, h: u32, stride: usize) -> Option<u16> {
        SCALES
            .iter()
            .find_map(|&k| decode_at_scale(corner, y, w, h, stride, k))
    }

    fn decode_at_scale(
        corner: Corner,
        y: &[u8],
        w: u32,
        h: u32,
        stride: usize,
        k: u32,
    ) -> Option<u16> {
        // The marker sits MARGIN in from its side and the bottom of the
        // SOURCE frame; a k× downscale divides every coordinate by k.
        let block = BLOCK / k;
        let ox = match corner {
            Corner::BottomLeft => MARGIN / k,
            Corner::BottomRight => w.checked_sub((MARGIN + W) / k)?,
        };
        let oy = h.checked_sub((MARGIN + H) / k)?;
        if block < 4 || ox + W / k > w || oy + H / k > h {
            return None;
        }
        // The quiet margin anchors the scale: all four strips around the
        // grid must read mid-grey. At a wrong scale at least one lands on the
        // gradient or on the grid itself, and the dead band and the CRC are
        // then not the only guard against a lucky misread.
        if !margin_is_grey(y, w, h, stride, ox, oy, k) {
            return None;
        }
        // Sample each block's centre half, away from the edges where the
        // encoder's blocking and chroma bleed live.
        let inset = block / 4;
        let mut word: u16 = 0;
        for bit in 0..(ROWS * COLS) {
            let bx = ox + (bit % COLS) * block;
            let by = oy + (bit / COLS) * block;
            let (mut sum, mut n) = (0u32, 0u32);
            for yy in (by + inset)..(by + block - inset) {
                let row = yy as usize * stride;
                for xx in (bx + inset)..(bx + block - inset) {
                    sum += *y.get(row + xx as usize)? as u32;
                    n += 1;
                }
            }
            let mean = sum / n.max(1);
            if mean.abs_diff(THRESHOLD) < DEAD_BAND {
                return None;
            }
            word = (word << 1) | u16::from(mean > THRESHOLD);
        }
        let seq = word >> 4;
        (crc4(seq) == (word & 0x0F) as u8).then_some(seq)
    }

    /// Mid-grey (0x80 → luma ≈ 126 after BT.601), within this band.
    const GREY: std::ops::RangeInclusive<u32> = 100..=152;

    /// Are the centre lines of the four margin strips around the grid grey?
    fn margin_is_grey(y: &[u8], w: u32, h: u32, stride: usize, ox: u32, oy: u32, k: u32) -> bool {
        let (gw, gh) = (W / k, H / k);
        let d = (MARGIN / k / 2).max(1);
        let (Some(left), right, Some(top), bottom) = (
            ox.checked_sub(d),
            ox + gw + d - 1,
            oy.checked_sub(d),
            oy + gh + d - 1,
        ) else {
            return false;
        };
        if right >= w || bottom >= h {
            return false;
        }
        let mean = |pts: &mut dyn Iterator<Item = (u32, u32)>| -> Option<u32> {
            let (mut sum, mut n) = (0u32, 0u32);
            for (xx, yy) in pts {
                sum += *y.get(yy as usize * stride + xx as usize)? as u32;
                n += 1;
            }
            (n > 0).then(|| sum / n)
        };
        [
            mean(&mut (oy..oy + gh).map(|yy| (left, yy))),
            mean(&mut (oy..oy + gh).map(|yy| (right, yy))),
            mean(&mut (ox..ox + gw).map(|xx| (xx, top))),
            mean(&mut (ox..ox + gw).map(|xx| (xx, bottom))),
        ]
        .into_iter()
        .all(|m| m.is_some_and(|m| GREY.contains(&m)))
    }
}

/// Is this process a bench target: the synthetic source selected AND bench
/// mode on? What the hello advertises as `bench` in `AgentCaps.video`, so a
/// viewer turns its probes on only where they mean something.
pub fn bench_active() -> bool {
    bench_enabled() && super::synthetic_env_enabled()
}

/// FR-16 L4 — the bench probe. A viewer in bench mode sends
/// `{"t":"probe","seq":N}` on the input DataChannel; the synthetic source
/// paints the last `N` it saw into the bottom-right [`marker`] of every frame
/// that follows, and the viewer times send → first decoded frame carrying
/// `N`. That is input to photon through the real pipeline, minus only the
/// host's own input stack, which `roomlerd probe-target` covers.
///
/// A probe never reaches an injector. Outside bench mode it is consumed and
/// dropped.
pub mod probe {
    use std::sync::atomic::{AtomicU32, Ordering};

    const NONE: u32 = u32::MAX;
    static LATEST: AtomicU32 = AtomicU32::new(NONE);

    /// Is `text` a probe? If it is, it is consumed here, recorded when bench
    /// mode is on, and the caller must not treat it as input.
    pub fn accept_json(text: &str) -> bool {
        // Every input message passes through here: a substring check first,
        // so pointer moves never pay for a second JSON parse.
        if !text.contains("\"probe\"") {
            return false;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return false;
        };
        if v.get("t").and_then(|t| t.as_str()) != Some("probe") {
            return false;
        }
        if super::bench_enabled()
            && let Some(seq) = v.get("seq").and_then(|s| s.as_u64())
        {
            LATEST.store((seq & 0x0FFF) as u32, Ordering::Release);
        }
        true
    }

    /// The last probe recorded, if any.
    pub fn latest() -> Option<u16> {
        match LATEST.load(Ordering::Acquire) {
            NONE => None,
            v => Some(v as u16),
        }
    }
}

/// FR-16 L0 — when each sequence number was captured, so a test in the same
/// process reads capture→decode latency on one clock. A ring of 4096 slots
/// indexed by the 12-bit sequence: at 30 fps that is 136 s before a slot is
/// reused, far longer than any frame stays in flight.
pub mod clock {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::{Duration, Instant};

    const SLOTS: usize = 4096;
    const EMPTY: i64 = i64::MIN;
    /// Signed microseconds from [`epoch`]. Signed, because the epoch is set
    /// on first use, which can come after the first instant handed in.
    static STAMPS: [AtomicI64; SLOTS] = [const { AtomicI64::new(EMPTY) }; SLOTS];

    fn epoch() -> Instant {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        *EPOCH.get_or_init(Instant::now)
    }

    /// Record that `seq` was captured at `at`.
    pub fn record(seq: u16, at: Instant) {
        let e = epoch();
        let us = if at >= e {
            (at - e).as_micros() as i64
        } else {
            -((e - at).as_micros() as i64)
        };
        STAMPS[seq as usize % SLOTS].store(us, Ordering::Release);
    }

    /// When `seq` was last captured, if ever.
    pub fn captured_at(seq: u16) -> Option<Instant> {
        let us = STAMPS[seq as usize % SLOTS].load(Ordering::Acquire);
        if us == EMPTY {
            return None;
        }
        let e = epoch();
        Some(if us >= 0 {
            e + Duration::from_micros(us as u64)
        } else {
            e - Duration::from_micros(us.unsigned_abs())
        })
    }
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
            let i = (bit * 4) as usize; // row 0 — offset is just column×4
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

    #[test]
    fn parse_size_accepts_even_in_range_and_refuses_the_rest() {
        assert_eq!(parse_size("640x360"), Some((640, 360)));
        assert_eq!(parse_size(" 1280X720 "), Some((1280, 720)));
        // Odd sides round down to even.
        assert_eq!(parse_size("641x361"), Some((640, 360)));
        assert_eq!(parse_size("32x32"), None);
        assert_eq!(parse_size("7680x4320"), None);
        assert_eq!(parse_size("wide"), None);
        assert_eq!(parse_size("640"), None);
    }

    #[test]
    fn crc4_matches_the_itu_polynomial() {
        // Hand-computed reference points for x⁴ + x + 1, init 0.
        assert_eq!(marker::crc4(0x000), 0x0);
        assert_eq!(marker::crc4(0x001), 0x3);
        // x¹¹·x⁴ = x¹⁵ ≡ 1: x has order 15 modulo this primitive polynomial.
        assert_eq!(marker::crc4(0x800), 0x1);
        // Every 1-bit error in the sequence changes the CRC.
        for s in [0x000u16, 0x123, 0xABC, 0xFFF] {
            for b in 0..12 {
                assert_ne!(
                    marker::crc4(s),
                    marker::crc4(s ^ (1 << b)),
                    "seq {s:#x} bit {b}"
                );
            }
        }
    }

    #[test]
    fn the_words_the_browser_decoder_pins_too() {
        // `ui/src/__tests__/workers/rcBenchMarker.spec.ts` pins the same
        // table against the browser's decoder: if either side drifts, one
        // of the two suites fails.
        for (seq, word) in [
            (0x000u16, 0x0000u16),
            (0x001, 0x0013),
            (0x800, 0x8001),
            (0x123, 0x1230),
            (0x5A5, 0x5A5D),
            (0xABC, 0xABCA),
            (0xFFF, 0xFFFE),
        ] {
            assert_eq!(marker::word(seq), word, "seq {seq:#05x}");
        }
        assert_eq!(
            (marker::BLOCK, marker::COLS, marker::ROWS, marker::MARGIN),
            (16, 8, 2, 8)
        );
    }

    /// The BT.601 limited-range luma of a BGRA pixel, as an encoder computes it.
    fn luma_of(bgra: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut y = vec![0u8; (w * h) as usize];
        for (i, px) in bgra.chunks_exact(4).enumerate() {
            let (b, g, r) = (px[0] as u32, px[1] as u32, px[2] as u32);
            y[i] = (16 + (66 * r + 129 * g + 25 * b + 128) / 256) as u8;
        }
        y
    }

    #[test]
    fn marker_round_trips_through_luma_at_every_sequence_boundary() {
        for counter in [0u64, 1, 2, 0x7FF, 0x800, 0xFFF, 0x1000, 0x1234_5678] {
            let data = SyntheticCapture::render_sized(counter, 320, 240);
            let y = luma_of(&data, 320, 240);
            assert_eq!(
                marker::decode_luma(&y, 320, 240, 320),
                Some(marker::seq_of(counter)),
                "counter {counter:#x}"
            );
        }
    }

    #[test]
    fn marker_survives_a_box_downscale() {
        // The viewer may decode a picture the agent downscaled. A 2× and a
        // 4× box filter keep each block's centre on its side of the threshold.
        let (w, h) = (640u32, 480u32);
        let data = SyntheticCapture::render_sized(0x5A5, w, h);
        let y = luma_of(&data, w, h);
        for k in [2u32, 4] {
            let (sw, sh) = (w / k, h / k);
            let mut small = vec![0u8; (sw * sh) as usize];
            for sy in 0..sh {
                for sx in 0..sw {
                    let mut sum = 0u32;
                    for dy in 0..k {
                        for dx in 0..k {
                            sum += y[((sy * k + dy) * w + sx * k + dx) as usize] as u32;
                        }
                    }
                    small[(sy * sw + sx) as usize] = (sum / (k * k)) as u8;
                }
            }
            assert_eq!(
                marker::decode_luma(&small, sw, sh, sw as usize),
                Some(0x5A5),
                "{k}×"
            );
        }
    }

    #[test]
    fn a_flipped_block_is_a_miss_never_a_wrong_number() {
        let data = SyntheticCapture::render_sized(0x321, 320, 240);
        let mut y = luma_of(&data, 320, 240);
        // Invert one block of the grid (bit 5: row 0, column 5).
        let (ox, oy) = marker::origin(320, 240).unwrap();
        for yy in oy..oy + marker::BLOCK {
            for xx in (ox + 5 * marker::BLOCK)..(ox + 6 * marker::BLOCK) {
                let i = (yy * 320 + xx) as usize;
                y[i] = 251 - y[i];
            }
        }
        let read = marker::decode_luma(&y, 320, 240, 320);
        assert_ne!(
            read,
            Some(0x321 ^ (1 << (11 - 5))),
            "a single misread block must not pass the CRC"
        );
        assert!(read.is_none() || read == Some(0x321), "got {read:?}");
    }

    #[test]
    fn the_probe_marker_reads_back_from_the_bottom_right_and_only_when_painted() {
        let y_of = |probe| {
            let data = SyntheticCapture::render_frame(0x123, 320, 240, probe);
            luma_of(&data, 320, 240)
        };
        let painted = y_of(Some(0x5A5));
        assert_eq!(
            marker::decode_luma_at(marker::Corner::BottomRight, &painted, 320, 240, 320),
            Some(0x5A5)
        );
        // The frame's own number is untouched beside it.
        assert_eq!(marker::decode_luma(&painted, 320, 240, 320), Some(0x123));
        // No probe yet: nothing to read, never a stale or invented number.
        let unpainted = y_of(None);
        assert_eq!(
            marker::decode_luma_at(marker::Corner::BottomRight, &unpainted, 320, 240, 320),
            None
        );
    }

    #[test]
    fn the_two_markers_never_overlap() {
        // Too narrow for both: the probe marker is refused, not squeezed in.
        assert!(marker::origin_at(marker::Corner::BottomLeft, 200, 240).is_some());
        assert_eq!(
            marker::origin_at(marker::Corner::BottomRight, 200, 240),
            None
        );
        let (lx, _) = marker::origin_at(marker::Corner::BottomLeft, 320, 240).unwrap();
        let (rx, _) = marker::origin_at(marker::Corner::BottomRight, 320, 240).unwrap();
        assert!(lx + marker::W + marker::MARGIN <= rx - marker::MARGIN);
    }

    #[test]
    fn only_a_probe_message_is_consumed_and_typing_the_word_still_reaches_input() {
        assert!(probe::accept_json(r#"{"t":"probe","seq":7}"#));
        assert!(!probe::accept_json(
            r#"{"t":"mouse_move","x":0.1,"y":0.2,"mon":0}"#
        ));
        // The substring pre-check must not swallow a person typing "probe".
        assert!(!probe::accept_json(r#"{"t":"key_text","text":"probe"}"#));
        assert!(!probe::accept_json("not json at all \"probe\""));
    }

    #[test]
    fn a_frame_too_small_for_the_marker_is_left_alone() {
        assert_eq!(marker::origin(100, 40), None);
        let mut data = vec![7u8; 100 * 40 * 4];
        marker::paint_bgra(&mut data, 100, 40, 400, 9);
        assert!(data.iter().all(|&b| b == 7));
    }

    #[test]
    fn the_clock_returns_what_was_recorded_and_nothing_else() {
        // An instant taken BEFORE the clock's epoch exists (the first call
        // sets it) must come back too: that is the first frame of every run.
        let t = Instant::now();
        clock::record(4095, t);
        let back = clock::captured_at(4095).expect("recorded");
        assert!(
            back.max(t) - back.min(t) < Duration::from_micros(2),
            "{t:?} came back as {back:?}"
        );
        // A later one, after the epoch.
        let t2 = Instant::now() + Duration::from_millis(5);
        clock::record(4094, t2);
        let back2 = clock::captured_at(4094).expect("recorded");
        assert!(back2.max(t2) - back2.min(t2) < Duration::from_micros(2));
        assert_eq!(
            clock::captured_at(4093),
            None,
            "an unwritten slot reads as never captured"
        );
    }
}
