// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-93 P3d — an idle desktop's DRM frames, skipped.
//!
//! DRM reports nothing about which pixels changed, so the DRM arm read,
//! converted and encoded the whole scanout buffer at the target frame rate on
//! a desktop nobody touched: **57.9 % of a core** for the FR-93 lab's idle
//! GNOME 50 desktop (1280×800, 0.4.133), where FR-29's X11 damage tracking
//! idles at 3.4 %.
//!
//! A Wayland compositor gives the signal damage would have. It never draws into
//! the buffer on screen: it draws the next frame into another buffer and flips
//! to it, so the primary plane's buffer changes when the picture does. The DRM
//! worker (`drm_backend.rs`) skips a frame, which the pump takes as an idle
//! screen, when all three hold:
//!
//! - the plane shows the same framebuffer over the same buffer as the last
//!   frame delivered. The buffer is named by its dma-buf's inode, because the
//!   kernel reuses a framebuffer id for a new buffer once the old one is gone;
//! - [`SAMPLE_ROWS`] rows spread down the picture hash the same as they did. A
//!   compositor that flips twice between two reads (A → B → A) shows the same
//!   buffer again with new pixels, and a change of any size crosses a sampled
//!   row;
//! - the last frame delivered is younger than [`MAX_SKIP`]. A change that
//!   crosses no sampled row waits at most that long: FR-29's bound for X11.
//!
//! ⚠️ **Wayland only.** An X server and the kernel console draw into the buffer
//! on screen in place, so its identity never changes and only the sampled rows
//! would see a change: a typed character could wait [`MAX_SKIP`]. The caller
//! turns the gate on only when the seat shows a Wayland session.
//!
//! Everything here is pure, so it is pinned by tests with no DRM device.

use std::time::{Duration, Instant};

/// The longest a frame is skipped for: a change no sampled row crosses is
/// delivered this late at worst. FR-29's bound for the X11 damage skip.
pub const MAX_SKIP: Duration = Duration::from_millis(1000);

/// How many rows the change check hashes, spread evenly down the picture.
/// 32 rows of a 1280-pixel scanout is 160 KiB a tick, against the 4 MiB a
/// delivered frame copies.
pub const SAMPLE_ROWS: u32 = 32;

/// What the primary plane showed on one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shown {
    /// The framebuffer id on the plane.
    pub fb: u32,
    /// The buffer under it: its dma-buf's inode, `0` when unknown (an old
    /// kernel, or a failed `fstat`), which leaves the framebuffer id and the
    /// rows to tell buffers apart.
    pub buffer: u64,
    /// [`sample_rows`] of its pixels.
    pub rows: u64,
}

/// Whether a tick may be skipped: the last delivered frame, and when.
#[derive(Debug, Default)]
pub struct IdleGate {
    last: Option<(Shown, Instant)>,
}

impl IdleGate {
    /// Whether a tick that shows `shown` at `now` may be skipped: the same
    /// framebuffer, buffer and rows as the last frame delivered, and that frame
    /// younger than [`MAX_SKIP`].
    pub fn unchanged(&self, shown: Shown, now: Instant) -> bool {
        match self.last {
            Some((last, at)) => last == shown && now.saturating_duration_since(at) < MAX_SKIP,
            None => false,
        }
    }

    /// A frame showing `shown` went out at `now`.
    pub fn delivered(&mut self, shown: Shown, now: Instant) {
        self.last = Some((shown, now));
    }

    /// Nothing is scanned out (a blanked panel, a mode change): whatever shows
    /// next is delivered.
    pub fn forget(&mut self) {
        self.last = None;
    }
}

/// A hash of [`SAMPLE_ROWS`] rows of a `height`-row picture whose rows start
/// `pitch` bytes apart, hashing the first `row_bytes` of each: the visible
/// pixels, never the pitch's padding, which may hold anything.
///
/// The rows are the centres of [`SAMPLE_ROWS`] equal bands, so the picture's
/// top and bottom are both covered. Each 8-byte word goes through
/// `h = (h ^ word) * FNV_PRIME`, a bijection of `h` for a given word: two
/// inputs that differ in a single word always hash apart.
pub fn sample_rows(src: &[u8], row_bytes: usize, height: u32, pitch: u32) -> u64 {
    let mut h = FNV_OFFSET;
    let n = SAMPLE_ROWS.min(height);
    for i in 0..n {
        let y = (u64::from(2 * i + 1) * u64::from(height) / u64::from(2 * n)) as usize;
        let start = y * pitch as usize;
        let Some(row) = src.get(start..start + row_bytes) else {
            break;
        };
        h = mix(h, row);
    }
    h
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn mix(mut h: u64, bytes: &[u8]) -> u64 {
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        let mut w = [0u8; 8];
        w.copy_from_slice(word);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(FNV_PRIME);
    }
    for &b in words.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(fb: u32, buffer: u64, rows: u64) -> Shown {
        Shown { fb, buffer, rows }
    }

    #[test]
    fn a_tick_is_skipped_only_when_framebuffer_buffer_and_rows_all_match() {
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        let a = shown(31, 1001, 0xabc);
        assert!(!gate.unchanged(a, t0), "nothing delivered yet: deliver");
        gate.delivered(a, t0);
        let soon = t0 + Duration::from_millis(33);
        assert!(gate.unchanged(a, soon));
        // A flip to another framebuffer.
        assert!(!gate.unchanged(shown(32, 1002, 0xabc), soon));
        // The framebuffer id reused for a new buffer (FR-93 §2.3 #6).
        assert!(!gate.unchanged(shown(31, 1003, 0xabc), soon));
        // A → B → A between two reads: the same buffer, new pixels.
        assert!(!gate.unchanged(shown(31, 1001, 0xdef), soon));
    }

    #[test]
    fn no_frame_is_skipped_past_max_skip() {
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        let a = shown(31, 1001, 0xabc);
        gate.delivered(a, t0);
        assert!(gate.unchanged(a, t0 + MAX_SKIP - Duration::from_millis(1)));
        assert!(!gate.unchanged(a, t0 + MAX_SKIP), "the bound delivers");
        gate.delivered(a, t0 + MAX_SKIP);
        assert!(
            gate.unchanged(a, t0 + MAX_SKIP + Duration::from_millis(33)),
            "and the bound restarts from that delivery"
        );
    }

    #[test]
    fn after_nothing_was_scanned_out_the_next_frame_is_delivered() {
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        let a = shown(31, 1001, 0xabc);
        gate.delivered(a, t0);
        gate.forget();
        assert!(!gate.unchanged(a, t0 + Duration::from_millis(33)));
    }

    /// A `height`-row XRGB picture `width` pixels wide, rows `pitch` bytes
    /// apart, every byte set from its position.
    fn picture(width: usize, height: usize, pitch: usize) -> Vec<u8> {
        let mut v = vec![0u8; pitch * height];
        for y in 0..height {
            for x in 0..width * 4 {
                v[y * pitch + x] = (y * 7 + x * 13) as u8;
            }
        }
        v
    }

    /// The rows [`sample_rows`] reads for a `height`-row picture.
    fn sampled(height: u32) -> Vec<usize> {
        let n = SAMPLE_ROWS.min(height);
        (0..n)
            .map(|i| (u64::from(2 * i + 1) * u64::from(height) / u64::from(2 * n)) as usize)
            .collect()
    }

    #[test]
    fn a_change_on_a_sampled_row_changes_the_hash_wherever_it_lands_in_the_row() {
        // 63 pixels: 252 bytes a row, 31 whole words and a 4-byte remainder.
        let (w, h, pitch) = (63usize, 96u32, 63 * 4 + 36);
        let base = picture(w, h as usize, pitch);
        let before = sample_rows(&base, w * 4, h, pitch as u32);
        for &y in &sampled(h) {
            // The first byte, one in the last whole word, and both ends of the
            // trailing partial word all count.
            for x in [0, w * 4 - 5, w * 4 - 4, w * 4 - 1] {
                let mut p = base.clone();
                p[y * pitch + x] ^= 0x40;
                assert_ne!(
                    sample_rows(&p, w * 4, h, pitch as u32),
                    before,
                    "row {y}, byte {x}"
                );
            }
        }
    }

    #[test]
    fn the_pitch_padding_and_unsampled_rows_are_not_read() {
        let (w, h, pitch) = (64usize, 96u32, 64 * 4 + 32);
        let base = picture(w, h as usize, pitch);
        let before = sample_rows(&base, w * 4, h, pitch as u32);
        let rows = sampled(h);
        let mut p = base.clone();
        for y in 0..h as usize {
            // The padding past the visible pixels, on every row.
            for x in w * 4..pitch {
                p[y * pitch + x] = !p[y * pitch + x];
            }
            // A row no sample lands on, in full.
            if !rows.contains(&y) {
                for x in 0..w * 4 {
                    p[y * pitch + x] = !p[y * pitch + x];
                }
            }
        }
        assert_eq!(sample_rows(&p, w * 4, h, pitch as u32), before);
    }

    #[test]
    fn the_samples_span_the_picture_and_never_repeat_a_row() {
        for h in [1u32, 2, 31, 32, 33, 768, 800, 1080, 2160] {
            let rows = sampled(h);
            assert_eq!(rows.len(), SAMPLE_ROWS.min(h) as usize, "height {h}");
            assert!(rows.windows(2).all(|r| r[0] < r[1]), "height {h}: {rows:?}");
            assert!(*rows.last().unwrap() < h as usize, "height {h}");
            // The first and last bands' centres: within one band of each edge.
            let band = (h as usize).div_ceil(SAMPLE_ROWS.min(h) as usize);
            assert!(rows[0] <= band, "height {h}: first row {}", rows[0]);
            assert!(
                h as usize - 1 - rows.last().unwrap() <= band,
                "height {h}: last row {}",
                rows.last().unwrap()
            );
        }
    }

    #[test]
    fn a_short_buffer_hashes_what_it_has_instead_of_panicking() {
        // A pitch × height larger than the mapping: the rows past its end are
        // left out rather than read.
        let (w, h, pitch) = (16usize, 64u32, 16 * 4);
        let full = picture(w, h as usize, pitch);
        let short = &full[..pitch * 10];
        let _ = sample_rows(short, w * 4, h, pitch as u32);
        assert_eq!(sample_rows(&[], w * 4, h, pitch as u32), FNV_OFFSET);
    }
}
