// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-127 P2 — simulcast: the ladder a camera publishes, and the layer a
//! tile of a given size needs from it.
//!
//! Everything here is pure, and every number the lever knows is in THIS
//! file, so that decision 1 (the ladder) becoming a different decision is a
//! small change: the rungs and their bitrates in [`ladder`], the size → layer
//! thresholds in [`layers_for`], the priority a spotlit tile earns in
//! [`priority_for`]. The room manager asks; it never computes.
//!
//! The ladder is stated RELATIVE to the capture (`scaleResolutionDownBy`),
//! never as absolute sizes: Chrome opens the camera at 640×480 unless asked
//! otherwise, so a "320×180" rung would not exist (the P0 finding). Spatial
//! layers are numbered the way mediasoup numbers them, lowest first.

use mediasoup::prelude::{ConsumerLayers, RtpEncodingParameters};

/// One rung of the ladder: what the client puts in one `encodings[]` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rung {
    /// `scaleResolutionDownBy`: 1 is the capture, 2 half of it each way.
    pub scale: u8,
    /// `maxBitrate`, in bit/s.
    pub max_bitrate: u32,
}

/// Temporal layers in every rung (`L1T3`): 2 is the full rate, 1 half of it,
/// 0 a quarter. mediasoup-client forces one count across the rungs anyway
/// (`Chrome111.js:235`), and Chrome's default is three.
pub const TEMPORAL_LAYERS: u8 = 3;

/// The capture height assumed for a producer that reported none: Chrome's
/// unconstrained 640×480.
pub const DEFAULT_CAPTURE_H: u32 = 480;

/// A consumer's priority: the worker's default, and what a spotlit tile's
/// consumer gets so that, when the viewer's downlink is short, the worker's
/// bandwidth estimator lowers grid tiles first.
pub const DEFAULT_PRIORITY: u8 = 1;
pub const SPOTLIT_PRIORITY: u8 = 2;

/// A rung serves a tile when its height covers this much of the tile's: the
/// half layer (240 px of a 480 capture) up to a 320 px tile.
const COVERAGE_NUM: u64 = 3;
const COVERAGE_DEN: u64 = 4;
/// The temporal layer by the tile's height, in device pixels: the full rate
/// from here, half of it from `HALF_RATE_FROM_PX`, a quarter below that.
const FULL_RATE_FROM_PX: u32 = 240;
const HALF_RATE_FROM_PX: u32 = 120;

/// The ladder for `spatial_layers` rungs, lowest first: 2 is ÷2 at
/// 300 kbit/s and ÷1 at 1.5 Mbit/s; 3 adds ÷4 at 100 kbit/s below them. Any
/// other count is read as 2 — decision 1's default, never a refusal.
pub fn ladder(spatial_layers: u8) -> Vec<Rung> {
    const QUARTER: Rung = Rung {
        scale: 4,
        max_bitrate: 100_000,
    };
    const HALF: Rung = Rung {
        scale: 2,
        max_bitrate: 300_000,
    };
    const FULL: Rung = Rung {
        scale: 1,
        max_bitrate: 1_500_000,
    };
    match spatial_layers {
        3 => vec![QUARTER, HALF, FULL],
        _ => vec![HALF, FULL],
    }
}

/// `meeting.simulcast` as the client reads it: the encodings to publish, in
/// the wire's snake_case (the client maps them onto
/// `RTCRtpEncodingParameters`). The ladder travels with every join, so it is
/// changeable without a client release.
pub fn ladder_json(spatial_layers: u8) -> serde_json::Value {
    let encodings: Vec<serde_json::Value> = ladder(spatial_layers)
        .iter()
        .map(|r| {
            serde_json::json!({
                "scale_resolution_down_by": r.scale,
                "max_bitrate": r.max_bitrate,
                "scalability_mode": format!("L1T{TEMPORAL_LAYERS}"),
            })
        })
        .collect();
    serde_json::json!({ "encodings": encodings })
}

/// How many spatial and temporal layers a stream has — what the worker lets
/// a consumer of it choose from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerShape {
    pub spatial: u8,
    pub temporal: u8,
}

impl LayerShape {
    /// The shape of a set of RTP encodings, or `None` for a plain stream.
    ///
    /// A simulcast producer has one encoding per rung, each carrying the
    /// rung's `scalabilityMode` (`L1T3`); a consumer of it has ONE encoding
    /// whose mode the router mangled into `L{rungs}T{t}` (mediasoup
    /// `ortc.rs:855`); an SVC stream is one encoding with spatial layers in
    /// its mode. One encoding with one spatial layer is a plain stream — a
    /// screen share, or a camera whose client published one encoding — and
    /// there is nothing to choose.
    pub fn of(encodings: &[RtpEncodingParameters]) -> Option<Self> {
        let first = encodings.first()?;
        let temporal = first.scalability_mode.temporal_layers().get();
        let spatial = if encodings.len() > 1 {
            u8::try_from(encodings.len()).unwrap_or(u8::MAX)
        } else {
            first.scalability_mode.spatial_layers().get()
        };
        (spatial > 1).then_some(Self { spatial, temporal })
    }

    /// The top of both: what a consumer gets when nothing asks for less.
    pub fn top(self) -> ConsumerLayers {
        ConsumerLayers {
            spatial_layer: self.spatial - 1,
            temporal_layer: Some(self.temporal - 1),
        }
    }
}

/// The layers a tile `tile_h` device pixels tall needs from a stream of
/// `shape` captured `capture_h` pixels tall.
///
/// Spatial: the lowest rung whose height covers three quarters of the tile,
/// rung `i` of `n` being the capture halved `n − 1 − i` times — so with a
/// 480 capture the half layer serves a tile up to 320 px and the full one
/// takes over above. Temporal, by the tile's height alone: the full rate
/// from 240 px, half from 120, a quarter below. A low bitrate saves bits and
/// not packets (one packet a frame is the floor), so temporal layers are the
/// packet lever for small tiles (the FR's §3.0). A tile of unknown height
/// (0) takes the top of both: the budget is a load lever, never a reason to
/// withhold a picture.
pub fn layers_for(capture_h: u32, tile_h: u32, shape: LayerShape) -> ConsumerLayers {
    let top = shape.top();
    if tile_h == 0 {
        return top;
    }
    // COVERAGE · tile ≤ rung  ⇔  NUM · tile ≤ DEN · rung, in integers.
    let need = COVERAGE_NUM * u64::from(tile_h);
    let spatial = (0..shape.spatial)
        .find(|i| {
            let halvings = u32::from(shape.spatial - 1 - i);
            let rung_h = u64::from(capture_h.checked_shr(halvings).unwrap_or(0));
            COVERAGE_DEN * rung_h >= need
        })
        .unwrap_or(top.spatial_layer);
    let by_height = if tile_h >= FULL_RATE_FROM_PX {
        2
    } else if tile_h >= HALF_RATE_FROM_PX {
        1
    } else {
        0
    };
    ConsumerLayers {
        spatial_layer: spatial,
        temporal_layer: Some(by_height.min(shape.temporal - 1)),
    }
}

/// The priority a tile's consumer gets: a spotlit tile — the big tile of a
/// spotlight or sidebar layout, or a pinned one — above the rest.
pub fn priority_for(spotlit: bool) -> u8 {
    if spotlit {
        SPOTLIT_PRIORITY
    } else {
        DEFAULT_PRIORITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mediasoup::types::scalability_modes::ScalabilityMode;

    const TWO: LayerShape = LayerShape {
        spatial: 2,
        temporal: 3,
    };
    const THREE: LayerShape = LayerShape {
        spatial: 3,
        temporal: 3,
    };

    fn layers(s: u8, t: u8) -> ConsumerLayers {
        ConsumerLayers {
            spatial_layer: s,
            temporal_layer: Some(t),
        }
    }

    fn encoding(mode: &str) -> RtpEncodingParameters {
        RtpEncodingParameters {
            scalability_mode: mode.parse::<ScalabilityMode>().expect("a mode"),
            ..Default::default()
        }
    }

    /// Decision 1: two rungs relative to the capture; three on request;
    /// anything else is two.
    #[test]
    fn the_ladder_is_two_rungs_by_default_and_three_on_request() {
        let two = ladder(2);
        assert_eq!(two.len(), 2);
        assert_eq!(
            two[0],
            Rung {
                scale: 2,
                max_bitrate: 300_000
            }
        );
        assert_eq!(
            two[1],
            Rung {
                scale: 1,
                max_bitrate: 1_500_000
            }
        );
        let three = ladder(3);
        assert_eq!(three.len(), 3);
        assert_eq!(three[0].scale, 4);
        assert_eq!(three[0].max_bitrate, 100_000);
        assert_eq!(&three[1..], &two[..], "the top two rungs are the same");
        assert_eq!(ladder(0), two);
        assert_eq!(ladder(7), two);
        for l in [ladder(2), ladder(3)] {
            assert!(
                l.windows(2).all(|w| w[0].scale > w[1].scale),
                "lowest rung first, the order mediasoup numbers spatial layers in"
            );
        }
    }

    /// What the client is handed, in the wire's snake_case.
    #[test]
    fn the_ladder_json_is_what_the_client_publishes() {
        let v = ladder_json(2);
        let enc = v["encodings"].as_array().expect("encodings");
        assert_eq!(enc.len(), 2);
        assert_eq!(enc[0]["scale_resolution_down_by"], 2);
        assert_eq!(enc[0]["max_bitrate"], 300_000);
        assert_eq!(enc[0]["scalability_mode"], "L1T3");
        assert_eq!(enc[1]["scale_resolution_down_by"], 1);
        assert_eq!(enc[1]["max_bitrate"], 1_500_000);
        assert_eq!(ladder_json(3)["encodings"].as_array().unwrap().len(), 3);
    }

    /// A simulcast producer (one encoding per rung), a consumer of it (one
    /// mangled encoding), an SVC stream, and a plain one.
    #[test]
    fn the_shape_of_a_stream() {
        assert_eq!(
            LayerShape::of(&[encoding("L1T3"), encoding("L1T3")]),
            Some(TWO)
        );
        assert_eq!(
            LayerShape::of(&[encoding("L2T3")]),
            Some(TWO),
            "a consumer's mangled mode"
        );
        assert_eq!(
            LayerShape::of(&[encoding("L3T3")]),
            Some(THREE),
            "an SVC stream"
        );
        assert_eq!(
            LayerShape::of(&[encoding("L1T3")]),
            None,
            "one encoding, one spatial layer: a plain stream"
        );
        assert_eq!(LayerShape::of(&[RtpEncodingParameters::default()]), None);
        assert_eq!(LayerShape::of(&[]), None);
        assert_eq!(
            LayerShape::of(&[
                RtpEncodingParameters::default(),
                RtpEncodingParameters::default()
            ]),
            Some(LayerShape {
                spatial: 2,
                temporal: 1
            }),
            "two rungs without temporal layers"
        );
        assert_eq!(TWO.top(), layers(1, 2));
    }

    /// The map, with a 480 capture and two rungs: the half layer up to a
    /// 320 px tile, the full one above; the temporal layer by height.
    #[test]
    fn a_tile_gets_the_lowest_rung_that_covers_three_quarters_of_it() {
        assert_eq!(layers_for(480, 100, TWO), layers(0, 0), "a mini-view tile");
        assert_eq!(layers_for(480, 119, TWO), layers(0, 0));
        assert_eq!(layers_for(480, 120, TWO), layers(0, 1), "a filmstrip tile");
        assert_eq!(layers_for(480, 180, TWO), layers(0, 1));
        assert_eq!(layers_for(480, 239, TWO), layers(0, 1));
        assert_eq!(layers_for(480, 240, TWO), layers(0, 2), "a grid tile");
        assert_eq!(
            layers_for(480, 320, TWO),
            layers(0, 2),
            "0.75 · 320 = 240 = the half rung"
        );
        assert_eq!(
            layers_for(480, 321, TWO),
            layers(1, 2),
            "one pixel more: the full rung"
        );
        assert_eq!(layers_for(480, 600, TWO), layers(1, 2), "a spotlit tile");
        assert_eq!(
            layers_for(480, 4000, TWO),
            layers(1, 2),
            "nothing above the top"
        );
    }

    /// A tile of unknown height takes the top of both: never a worse picture
    /// on missing data.
    #[test]
    fn an_unknown_height_takes_the_top() {
        assert_eq!(layers_for(480, 0, TWO), layers(1, 2));
        assert_eq!(layers_for(480, 0, THREE), layers(2, 2));
    }

    /// Three rungs: the quarter layer (120 px of a 480 capture) serves a tile
    /// up to 160 px.
    #[test]
    fn a_third_rung_serves_the_smallest_tiles() {
        assert_eq!(layers_for(480, 100, THREE), layers(0, 0));
        assert_eq!(layers_for(480, 160, THREE), layers(0, 1));
        assert_eq!(layers_for(480, 161, THREE), layers(1, 1));
        assert_eq!(layers_for(480, 320, THREE), layers(1, 2));
        assert_eq!(layers_for(480, 400, THREE), layers(2, 2));
    }

    /// The map is relative to the capture: a 720 p camera's half rung is
    /// 360 px, so it serves a tile up to 480.
    #[test]
    fn the_map_is_relative_to_the_capture() {
        assert_eq!(layers_for(720, 480, TWO), layers(0, 2));
        assert_eq!(layers_for(720, 481, TWO), layers(1, 2));
        assert_eq!(layers_for(DEFAULT_CAPTURE_H, 320, TWO), layers(0, 2));
    }

    /// A stream with fewer temporal layers than the bands: the band is
    /// clamped to what exists.
    #[test]
    fn the_temporal_layer_is_clamped_to_what_the_stream_has() {
        let one_t = LayerShape {
            spatial: 2,
            temporal: 1,
        };
        assert_eq!(layers_for(480, 600, one_t), layers(1, 0));
        assert_eq!(layers_for(480, 100, one_t), layers(0, 0));
        let two_t = LayerShape {
            spatial: 2,
            temporal: 2,
        };
        assert_eq!(layers_for(480, 600, two_t), layers(1, 1));
    }

    #[test]
    fn a_spotlit_tile_outranks_the_rest() {
        assert_eq!(priority_for(true), SPOTLIT_PRIORITY);
        assert_eq!(priority_for(false), DEFAULT_PRIORITY);
        assert!(SPOTLIT_PRIORITY > DEFAULT_PRIORITY);
    }
}
