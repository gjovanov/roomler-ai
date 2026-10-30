// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-16 — the synthetic source's pictures, compiled into every build.
//!
//! [`paint`] is the frame the synthetic capture backend has always emitted: a
//! diagonal gradient that shifts one pixel a frame, the 64-bit counter strip
//! top-left, and the L4 [`marker`] bottom-left. It lives here, outside the
//! `synthetic-frame-source` feature, for the same reason the marker does: a
//! release build has to paint it too. The L1 encoder bench (`roomlerd
//! encoder-smoke --bench`) runs on every fleet host through `roomler exec`,
//! and a bench number and an L0/L4 number should come from the same pixels.
//! `synthetic_backend` paints through it, and the bytes are unchanged (pinned
//! by `paint_keeps_the_synthetic_sources_bytes`).
//!
//! [`Scene`] is the motion profile the bench encodes: nothing moving, a window
//! dragged at a known speed, or the whole frame moving. FR-16's starting point
//! was that drag speed is a first-order input to what a session costs, so the
//! speed is a parameter here rather than a human's hand.

use super::bench_marker as marker;

/// The speed `drag` moves at when none is given, in pixels a second: a window
/// dragged briskly across a laptop screen.
pub const DRAG_DEFAULT_PX_PER_S: u32 = 1000;

/// The fastest drag a scene accepts. A window crossing a 4K screen in a fifth
/// of a second; anything faster is a cut, not a drag.
const DRAG_MAX_PX_PER_S: u32 = 20_000;

/// A motion profile: what changes from one frame to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// Frame 0, every time: byte-identical frames, an idle desktop. What a
    /// still picture costs once its keyframe has gone out.
    Static,
    /// A text-like window crossing frame 0's backdrop at `px_per_s`, bouncing
    /// off the edges. The counter strip and the marker still advance.
    Drag { px_per_s: u32 },
    /// The synthetic source's own frame ([`paint`]): every pixel changes every
    /// frame, a full-screen pan.
    Pan,
}

impl Scene {
    /// `static`, `pan`, `drag` (at [`DRAG_DEFAULT_PX_PER_S`]) or `drag:<px/s>`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "static" => Some(Self::Static),
            "pan" => Some(Self::Pan),
            "drag" => Some(Self::Drag {
                px_per_s: DRAG_DEFAULT_PX_PER_S,
            }),
            other => {
                let speed: u32 = other.strip_prefix("drag:")?.parse().ok()?;
                (1..=DRAG_MAX_PX_PER_S)
                    .contains(&speed)
                    .then_some(Self::Drag { px_per_s: speed })
            }
        }
    }

    /// The name [`Self::parse`] reads back, with a drag's speed always spelled
    /// out, so a table row says how fast its window moved.
    pub fn label(self) -> String {
        match self {
            Self::Static => "static".to_string(),
            Self::Pan => "pan".to_string(),
            Self::Drag { px_per_s } => format!("drag:{px_per_s}"),
        }
    }

    /// Frame `k` of the scene, BGRA, for an encoder told it runs at `fps`: a
    /// drag moves `px_per_s / fps` pixels a frame. Pure over its inputs.
    pub fn render(self, k: u64, width: u32, height: u32, fps: u32) -> Vec<u8> {
        match self {
            Self::Static => paint(0, width, height, None),
            Self::Pan => paint(k, width, height, None),
            Self::Drag { px_per_s } => {
                let mut data = vec![0u8; (width * height * 4) as usize];
                paint_gradient(&mut data, width, height, 0);
                paint_window(
                    &mut data,
                    width,
                    height,
                    window_rect(k, width, height, fps, px_per_s),
                );
                paint_counter_strip(&mut data, width, height, k);
                marker::paint_bgra(&mut data, width, height, width * 4, marker::seq_of(k));
                data
            }
        }
    }
}

/// The synthetic source's frame for `counter`, BGRA: a horizontal gradient
/// that shifts one pixel per frame, plus a 64-pixel "counter strip" in the
/// top-left where each pixel column encodes one bit of the counter, plus the
/// L4 [`marker`] in the bottom-left, plus the last bench probe in the
/// bottom-right marker when there is one (FR-16 L4). Pure over its inputs.
pub fn paint(counter: u64, width: u32, height: u32, probe: Option<u16>) -> Vec<u8> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    let shift = (counter % width as u64) as u32;
    paint_gradient(&mut data, width, height, shift);
    paint_counter_strip(&mut data, width, height, counter);
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

fn paint_gradient(data: &mut [u8], width: u32, height: u32, shift: u32) {
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
}

fn paint_counter_strip(data: &mut [u8], width: u32, height: u32, counter: u64) {
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
}

/// Where a drag's window is in frame `k`: three eighths of the frame each
/// way, a quarter of the way down, and `px_per_s · k / fps` pixels along a
/// track that bounces off both edges, so the motion never jumps.
/// `(x, y, w, h)`.
fn window_rect(k: u64, width: u32, height: u32, fps: u32, px_per_s: u32) -> (u32, u32, u32, u32) {
    let w = (width * 3 / 8).max(1);
    let h = (height * 3 / 8).max(1);
    let travel = (width - w) as u64;
    let x = if travel == 0 {
        0
    } else {
        let along = px_per_s as u64 * k / fps.max(1) as u64;
        let p = along % (2 * travel);
        if p <= travel { p } else { 2 * travel - p }
    };
    (x as u32, height / 4, w, h)
}

/// A window of dark "text" on a light page under a title bar, drawn in its
/// own coordinates, so it moves as one rigid block the way a dragged window
/// does. Lines of 5 px glyphs and 2 px gaps, with a deterministic scatter of
/// spaces, give an encoder the high-frequency edges real text has.
fn paint_window(data: &mut [u8], width: u32, height: u32, (x0, y0, w, h): (u32, u32, u32, u32)) {
    let title = (h / 8).max(2);
    for y in y0..(y0 + h).min(height) {
        let ly = y - y0;
        for x in x0..(x0 + w).min(width) {
            let lx = x - x0;
            let (b, g, r) = if ly < title {
                (0x70, 0x50, 0x30)
            } else {
                let row = (ly - title) % 14;
                let line = (ly - title) / 14;
                let glyph = lx / 7;
                let ink = (2..10).contains(&row)
                    && lx % 7 < 5
                    && (line * 31 + glyph * 17) % 11 != 0
                    && (glyph + line * 3) % 9 != 8;
                if ink {
                    (0x20, 0x20, 0x20)
                } else {
                    (0xF0, 0xF0, 0xF0)
                }
            };
            let i = ((y * width + x) * 4) as usize;
            data[i] = b;
            data[i + 1] = g;
            data[i + 2] = r;
            data[i + 3] = 0xFF;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// The painter moved here from `synthetic_backend` so release builds can
    /// paint it (FR-16 L1). These hashes were taken from the synthetic source
    /// BEFORE the move (master `3fdccc44f`): L0 and L4 read these exact
    /// pixels, so a change here is a change to their instrument.
    #[test]
    fn paint_keeps_the_synthetic_sources_bytes() {
        for (counter, w, h, probe, want) in GOLDEN {
            assert_eq!(
                fnv1a(&paint(counter, w, h, probe)),
                want,
                "paint({counter}, {w}, {h}, {probe:?})"
            );
        }
    }

    const GOLDEN: [(u64, u32, u32, Option<u16>, u64); 5] = [
        (0, 320, 240, None, 0x9268_fd23_cc77_f04f),
        (7, 320, 240, None, 0xbc9f_c9f6_f2e9_a88d),
        (4097, 640, 360, Some(1234), 0x427b_1947_2c1c_d141),
        (123_456_789, 1280, 720, None, 0x90ad_ead3_46b3_01af),
        (31, 66, 64, Some(5), 0x88f5_f0bc_83f0_485e),
    ];

    #[test]
    fn static_repeats_frame_zero_exactly() {
        let first = Scene::Static.render(0, 320, 240, 30);
        assert_eq!(first, paint(0, 320, 240, None));
        for k in [1, 2, 29, 30, 1000] {
            assert_eq!(Scene::Static.render(k, 320, 240, 30), first, "frame {k}");
        }
    }

    #[test]
    fn pan_is_the_synthetic_sources_own_frame() {
        for k in [0, 1, 7, 321] {
            assert_eq!(Scene::Pan.render(k, 320, 240, 30), paint(k, 320, 240, None));
        }
        assert_ne!(
            Scene::Pan.render(1, 320, 240, 30),
            Scene::Pan.render(2, 320, 240, 30)
        );
    }

    /// 300 px/s at 30 fps is 10 px a frame. A 320 px frame leaves a 120 px
    /// window 200 px of track, so it turns at frame 20 and is home at 40.
    #[test]
    fn a_drag_moves_at_its_speed_and_bounces_off_the_edges() {
        let x = |k| window_rect(k, 320, 240, 30, 300).0;
        assert_eq!(window_rect(0, 320, 240, 30, 300), (0, 60, 120, 90));
        assert_eq!(x(5), 50);
        assert_eq!(x(20), 200);
        assert_eq!(x(25), 150);
        assert_eq!(x(40), 0);
        // Twice the speed, twice the distance.
        assert_eq!(window_rect(5, 320, 240, 30, 600).0, 100);
        // The same speed at twice the rate is half the step.
        assert_eq!(window_rect(5, 320, 240, 60, 300).0, 25);
    }

    /// Only the window, the counter strip and the marker change; the backdrop
    /// stays frame 0's gradient.
    #[test]
    fn a_drag_changes_the_window_and_leaves_the_backdrop() {
        let scene = Scene::Drag { px_per_s: 300 };
        let (a, b) = (scene.render(3, 320, 240, 30), scene.render(4, 320, 240, 30));
        assert_ne!(a, b);
        let backdrop = paint(0, 320, 240, None);
        // (300, 20): right of the counter strip, above the window (y 60..150),
        // clear of the bottom-left marker.
        let i = ((20 * 320 + 300) * 4) as usize;
        assert_eq!(a[i..i + 4], backdrop[i..i + 4]);
        assert_eq!(b[i..i + 4], backdrop[i..i + 4]);
        // Frame 3's window spans x 30..150; frame 4's starts at 40.
        let j = ((100 * 320 + 35) * 4) as usize;
        assert_ne!(a[j..j + 4], backdrop[j..j + 4], "inside frame 3's window");
        assert_eq!(b[j..j + 4], backdrop[j..j + 4], "left of frame 4's window");
    }

    #[test]
    fn scene_names_round_trip_and_nonsense_is_refused() {
        for s in [
            Scene::Static,
            Scene::Pan,
            Scene::Drag { px_per_s: 1 },
            Scene::Drag { px_per_s: 2400 },
        ] {
            assert_eq!(Scene::parse(&s.label()), Some(s));
        }
        assert_eq!(
            Scene::parse("drag"),
            Some(Scene::Drag {
                px_per_s: DRAG_DEFAULT_PX_PER_S
            })
        );
        assert_eq!(Scene::parse(" Pan "), Some(Scene::Pan));
        for bad in [
            "",
            "walk",
            "drag:",
            "drag:0",
            "drag:-5",
            "drag:fast",
            "drag:20001",
        ] {
            assert_eq!(Scene::parse(bad), None, "{bad:?}");
        }
    }
}
