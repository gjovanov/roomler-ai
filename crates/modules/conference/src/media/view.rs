// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-127 P1 — receive what is visible: what a viewer says it renders, and
//! what that means for each of its consumers. P2 — and which layer each of
//! those consumers should carry.
//!
//! A `media:view` report is the WHOLE set of video producers the client shows
//! right now, not a delta — idempotent, and self-healing after a lost or
//! reordered message. The server keeps the last report per participant as a
//! [`ViewState`] and brings the participant's consumers in line with it: a
//! video consumer whose producer the report names is resumed, every other
//! video consumer is paused, and an audio consumer is never touched.
//!
//! ⚠️ The participant's view is an `Option<ViewState>`, and the two states must
//! never be collapsed: `None` is a legacy client that never declared
//! `caps: ["view"]` — everything flows, exactly as before this phase — while
//! `Some(empty)` is a view client that renders nothing, so every video consumer
//! stays paused. The same shape as `ingress_rules` (`None` = no policy
//! compiled, `Some([])` = deny), for the same reason.
//!
//! ⚠️ This is the VIEWER's pause (`Consumer::pause`, read back as
//! `Consumer::paused`). FR-30's owner pause (`Producer::pause`, mirrored into
//! every consumer's `producer_paused`) is a different flag the worker keeps
//! separately, and nothing here reads or writes it: a viewer hiding a tile
//! changes nothing another viewer can see, and an owner's pause reaches every
//! viewer whatever their view says. [`ConsumerLine`] deliberately has no
//! `producer_paused` field, so the planner cannot be taught to confuse them.
//!
//! P2 adds a second, separate plan ([`tune_plan`]): for the consumers the
//! view SHOWS, the layer ceiling the tile's height maps to and the priority
//! its spotlit flag maps to. It never touches a consumer the view does not
//! name — [`plan`] alone moves those, and only their pause flag.
//!
//! Everything in this module is pure; the room manager clones the consumer
//! handles out of its maps and awaits the worker only after every guard is
//! dropped (`RoomManager::apply_view`).

use std::collections::HashMap;

use mediasoup::prelude::{ConsumerLayers, MediaKind, ProducerId};

use super::simulcast::{self, LayerShape};

/// One tile as the viewer reports it: its size in device pixels and — P2 —
/// whether it is spotlit (the big tile of a spotlight or sidebar layout, or
/// a pinned one), which earns its consumer a higher priority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tile {
    pub w: u32,
    pub h: u32,
    pub spotlit: bool,
}

impl Tile {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            spotlit: false,
        }
    }
}

/// What a view-capable client last reported it renders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewState {
    /// The video producers this viewer shows, each with its tile.
    pub wanted: HashMap<ProducerId, Tile>,
    /// The `seq` of the report held. A report with a smaller `seq` was sent
    /// before the one held and arrived after it: it is dropped.
    pub seq: u64,
}

impl ViewState {
    /// Apply a report. `false` when it is older than the report held, in which
    /// case nothing changes. `max` truncates the report in the order sent — the
    /// server's load guard (`[meetings] view_max`), not a policy.
    pub fn apply<I>(&mut self, seq: u64, tiles: I, max: usize) -> bool
    where
        I: IntoIterator<Item = (ProducerId, Tile)>,
    {
        if seq < self.seq {
            return false;
        }
        self.seq = seq;
        self.wanted = tiles.into_iter().take(max).collect();
        true
    }
}

/// Should `kind` from `producer_id` flow to a viewer whose view is `view`?
///
/// `None` — a legacy client — wants everything. A view client wants every
/// audio producer and exactly the video producers its report names.
pub fn wants(view: Option<&ViewState>, kind: MediaKind, producer_id: &ProducerId) -> bool {
    match view {
        None => true,
        Some(v) => kind == MediaKind::Audio || v.wanted.contains_key(producer_id),
    }
}

/// One consumer as the planner sees it: the producer it consumes, its kind,
/// and the VIEWER's own pause flag (`Consumer::paused`). The owner's flag
/// (`Consumer::producer_paused`) is not an input, by design — see the module
/// docs. P2 adds what the stream offers and what the worker holds for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerLine {
    pub producer_id: ProducerId,
    pub kind: MediaKind,
    pub paused: bool,
    /// P2 — the layers the stream offers; `None` for a plain stream (one
    /// encoding — a screen share, a camera that published one) and for audio.
    pub shape: Option<LayerShape>,
    /// P2 — the ceiling the worker holds now (`Consumer::preferred_layers`).
    pub preferred: Option<ConsumerLayers>,
    /// P2 — the consumer's priority (`Consumer::priority`).
    pub priority: u8,
}

/// What to do to one consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Resume,
    Pause,
}

/// The changes that bring `consumers` in line with `view`, as indexes into
/// `consumers`. Audio is never in the plan; a consumer already in the right
/// state is not either, so applying a report twice does nothing the second
/// time.
pub fn plan(view: &ViewState, consumers: &[ConsumerLine]) -> Vec<(usize, Change)> {
    consumers
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            if c.kind != MediaKind::Video {
                return None;
            }
            match (view.wanted.contains_key(&c.producer_id), c.paused) {
                (true, true) => Some((i, Change::Resume)),
                (false, false) => Some((i, Change::Pause)),
                _ => None,
            }
        })
        .collect()
}

/// P2 — what to set on one consumer the view shows: a new layer ceiling, a
/// new priority, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tune {
    pub layers: Option<ConsumerLayers>,
    pub priority: Option<u8>,
}

/// P2 — the tunes that bring the consumers the view SHOWS in line with their
/// tiles, as indexes into `consumers`: the layer ceiling the tile's height
/// maps to (`simulcast::layers_for`, with the producer's capture height from
/// `capture_h`) and the priority its spotlit flag maps to — each only where
/// it differs from what the worker holds, so a report applied twice tunes
/// nothing the second time. A plain stream gets no layers (there is nothing
/// to choose) but still its priority. A consumer the view does not name is
/// never touched: it is paused, or about to be, and [`plan`] is the only
/// thing that moves it. Audio never.
pub fn tune_plan(
    view: &ViewState,
    consumers: &[ConsumerLine],
    capture_h: impl Fn(&ProducerId) -> u32,
) -> Vec<(usize, Tune)> {
    consumers
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            if c.kind != MediaKind::Video {
                return None;
            }
            let tile = view.wanted.get(&c.producer_id)?;
            let layers = c
                .shape
                .map(|shape| simulcast::layers_for(capture_h(&c.producer_id), tile.h, shape))
                .filter(|want| Some(*want) != c.preferred);
            let priority = Some(simulcast::priority_for(tile.spotlit)).filter(|p| *p != c.priority);
            (layers.is_some() || priority.is_some()).then_some((i, Tune { layers, priority }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u8) -> ProducerId {
        format!("00000000-0000-4000-8000-0000000000{n:02x}")
            .parse()
            .expect("a producer id")
    }

    fn line(n: u8, kind: MediaKind, paused: bool) -> ConsumerLine {
        ConsumerLine {
            producer_id: pid(n),
            kind,
            paused,
            shape: None,
            preferred: None,
            priority: simulcast::DEFAULT_PRIORITY,
        }
    }

    fn view_of(ids: &[u8]) -> ViewState {
        let mut v = ViewState::default();
        assert!(v.apply(1, ids.iter().map(|n| (pid(*n), Tile::new(320, 240))), 49));
        v
    }

    /// `None` is a legacy client: everything flows, video included.
    #[test]
    fn a_legacy_client_wants_everything() {
        assert!(wants(None, MediaKind::Video, &pid(1)));
        assert!(wants(None, MediaKind::Audio, &pid(1)));
    }

    /// `Some(empty)` is a view client that renders nothing: only audio flows.
    /// The two must never be collapsed.
    #[test]
    fn a_view_client_showing_nothing_wants_only_audio() {
        let empty = ViewState::default();
        assert!(!wants(Some(&empty), MediaKind::Video, &pid(1)));
        assert!(wants(Some(&empty), MediaKind::Audio, &pid(1)));
        assert_ne!(
            wants(None, MediaKind::Video, &pid(1)),
            wants(Some(&empty), MediaKind::Video, &pid(1)),
            "None and Some(empty) answer differently for video"
        );
    }

    #[test]
    fn a_view_client_wants_the_video_it_names_and_every_audio() {
        let v = view_of(&[1, 2]);
        assert!(wants(Some(&v), MediaKind::Video, &pid(1)));
        assert!(wants(Some(&v), MediaKind::Video, &pid(2)));
        assert!(!wants(Some(&v), MediaKind::Video, &pid(3)));
        assert!(wants(Some(&v), MediaKind::Audio, &pid(3)));
    }

    /// A report is the whole set: the next one replaces, never adds.
    #[test]
    fn a_report_replaces_the_previous_one() {
        let mut v = view_of(&[1, 2]);
        assert!(v.apply(2, [(pid(3), Tile::new(640, 480))], 49));
        assert_eq!(v.wanted.len(), 1);
        assert!(v.wanted.contains_key(&pid(3)));
        assert_eq!(v.wanted[&pid(3)], Tile::new(640, 480));
        assert_eq!(v.seq, 2);
    }

    /// A report sent before the one held, arriving after it, is dropped; a
    /// re-send of the same report (equal `seq`) is applied, idempotently.
    #[test]
    fn an_older_report_is_dropped_and_an_equal_one_reapplied() {
        let mut v = view_of(&[1]);
        assert!(v.apply(5, [(pid(2), Tile::default())], 49));
        assert!(
            !v.apply(4, [(pid(3), Tile::default())], 49),
            "seq 4 after 5 is stale"
        );
        assert!(
            v.wanted.contains_key(&pid(2)),
            "the stale report changed nothing"
        );
        assert!(!v.wanted.contains_key(&pid(3)));
        assert_eq!(v.seq, 5);
        assert!(
            v.apply(5, [(pid(2), Tile::default())], 49),
            "the same report again"
        );
        assert_eq!(v.wanted.len(), 1);
    }

    /// The first report a fresh state sees is applied whatever its `seq`.
    #[test]
    fn the_first_report_is_applied_at_any_seq() {
        let mut v = ViewState::default();
        assert!(v.apply(0, [(pid(1), Tile::default())], 49));
        assert!(v.wanted.contains_key(&pid(1)));
    }

    #[test]
    fn a_long_report_is_truncated_in_the_order_sent() {
        let mut v = ViewState::default();
        assert!(v.apply(
            1,
            [3, 1, 2].into_iter().map(|n| (pid(n), Tile::default())),
            2
        ));
        assert!(v.wanted.contains_key(&pid(3)));
        assert!(v.wanted.contains_key(&pid(1)));
        assert!(
            !v.wanted.contains_key(&pid(2)),
            "the third tile is past the cap"
        );
    }

    /// Named and paused → resume; unnamed and flowing → pause; audio never.
    #[test]
    fn the_plan_resumes_the_named_pauses_the_rest_and_leaves_audio_alone() {
        let v = view_of(&[1]);
        let consumers = [
            line(1, MediaKind::Video, true),  // named, paused: resume
            line(2, MediaKind::Video, false), // not named, flowing: pause
            line(2, MediaKind::Audio, false), // audio: never
            line(3, MediaKind::Audio, true),  // audio, even paused: never
        ];
        assert_eq!(
            plan(&v, &consumers),
            vec![(0, Change::Resume), (1, Change::Pause)]
        );
    }

    /// A consumer already in the state the view asks for is left alone, so a
    /// report applied twice changes nothing the second time.
    #[test]
    fn the_plan_is_empty_when_the_consumers_already_match() {
        let v = view_of(&[1]);
        let consumers = [
            line(1, MediaKind::Video, false),
            line(2, MediaKind::Video, true),
        ];
        assert!(plan(&v, &consumers).is_empty());
    }

    /// An empty view from a view client pauses every flowing video consumer.
    #[test]
    fn an_empty_view_pauses_every_video_consumer() {
        let v = ViewState::default();
        let consumers = [
            line(1, MediaKind::Video, false),
            line(2, MediaKind::Video, false),
            line(1, MediaKind::Audio, false),
        ];
        assert_eq!(
            plan(&v, &consumers),
            vec![(0, Change::Pause), (1, Change::Pause)]
        );
    }

    /// The viewer's flag is the only one the plan reads: a producer its owner
    /// paused (FR-30) and a producer the viewer hid are different states, and
    /// the planner sees only the viewer's. A named video consumer the viewer
    /// paused is resumed whatever its owner is doing — the owner's pause still
    /// holds inside the worker, as `producer_paused`, which this never touches.
    #[test]
    fn the_plan_reads_only_the_viewers_own_flag() {
        let v = view_of(&[1]);
        let consumers = [line(1, MediaKind::Video, true)];
        assert_eq!(plan(&v, &consumers), vec![(0, Change::Resume)]);
    }

    // ---- P2: the tune plan ----

    const TWO: LayerShape = LayerShape {
        spatial: 2,
        temporal: 3,
    };

    fn layers(s: u8, t: u8) -> ConsumerLayers {
        ConsumerLayers {
            spatial_layer: s,
            temporal_layer: Some(t),
        }
    }

    /// A simulcast consumer the worker created at the top (its default).
    fn simulcast_line(n: u8, paused: bool) -> ConsumerLine {
        ConsumerLine {
            shape: Some(TWO),
            preferred: Some(TWO.top()),
            ..line(n, MediaKind::Video, paused)
        }
    }

    fn view_with(tiles: &[(u8, Tile)]) -> ViewState {
        let mut v = ViewState::default();
        assert!(v.apply(1, tiles.iter().map(|(n, t)| (pid(*n), *t)), 49));
        v
    }

    const CAPTURE: fn(&ProducerId) -> u32 = |_| 480;

    /// A small tile lowers the ceiling, a big one keeps the top; a tune is
    /// planned only where the worker holds something else, so the same
    /// report twice tunes nothing the second time.
    #[test]
    fn a_shown_tile_gets_the_layers_its_height_maps_to_once() {
        let v = view_with(&[(1, Tile::new(160, 120)), (2, Tile::new(640, 480))]);
        let consumers = [simulcast_line(1, true), simulcast_line(2, false)];
        assert_eq!(
            tune_plan(&v, &consumers, CAPTURE),
            vec![(
                0,
                Tune {
                    layers: Some(layers(0, 1)),
                    priority: None
                }
            )],
            "the big tile already has the top; the small one is lowered — paused or not"
        );
        // The worker now holds it: nothing to do.
        let settled = [
            ConsumerLine {
                preferred: Some(layers(0, 1)),
                ..simulcast_line(1, false)
            },
            simulcast_line(2, false),
        ];
        assert!(tune_plan(&v, &settled, CAPTURE).is_empty());
    }

    /// A consumer the view does not name is never tuned — it is paused, or
    /// about to be, and only the pause plan moves it. Audio never.
    #[test]
    fn an_unnamed_consumer_and_audio_are_never_tuned() {
        let v = view_with(&[(1, Tile::new(160, 120))]);
        let consumers = [
            simulcast_line(2, false), // not named, holding the top: untouched
            ConsumerLine {
                shape: Some(TWO),
                preferred: Some(TWO.top()),
                priority: simulcast::SPOTLIT_PRIORITY, // keeps it; moot while hidden
                ..line(3, MediaKind::Video, true)
            },
            line(1, MediaKind::Audio, false), // named producer's AUDIO: never
        ];
        assert!(tune_plan(&v, &consumers, CAPTURE).is_empty());
    }

    /// A spotlit tile earns the higher priority, and loses it when it is no
    /// longer spotlit; a plain stream (one encoding) gets the priority but
    /// never layers — there is nothing to choose.
    #[test]
    fn a_spotlit_tile_is_prioritised_and_a_plain_stream_only_prioritised() {
        let spotlit = Tile {
            w: 1280,
            h: 720,
            spotlit: true,
        };
        let v = view_with(&[(1, spotlit), (2, spotlit)]);
        let consumers = [
            simulcast_line(1, false),
            line(2, MediaKind::Video, false), // a screen share: shape None
        ];
        assert_eq!(
            tune_plan(&v, &consumers, CAPTURE),
            vec![
                (
                    0,
                    Tune {
                        layers: None,
                        priority: Some(simulcast::SPOTLIT_PRIORITY)
                    }
                ),
                (
                    1,
                    Tune {
                        layers: None,
                        priority: Some(simulcast::SPOTLIT_PRIORITY)
                    }
                ),
            ]
        );
        // Un-spotlit again: back to the default, in the same tile size.
        let v2 = view_with(&[(1, Tile::new(1280, 720))]);
        let held = [ConsumerLine {
            priority: simulcast::SPOTLIT_PRIORITY,
            ..simulcast_line(1, false)
        }];
        assert_eq!(
            tune_plan(&v2, &held, CAPTURE),
            vec![(
                0,
                Tune {
                    layers: None,
                    priority: Some(simulcast::DEFAULT_PRIORITY)
                }
            )]
        );
    }

    /// The map reads the producer's own capture height: the same tile wants
    /// a different rung from a 720 p camera than from a 480 one.
    #[test]
    fn the_tune_reads_each_producers_capture() {
        let v = view_with(&[(1, Tile::new(640, 400)), (2, Tile::new(640, 400))]);
        let consumers = [simulcast_line(1, false), simulcast_line(2, false)];
        let capture = |p: &ProducerId| if *p == pid(1) { 720 } else { 480 };
        assert_eq!(
            tune_plan(&v, &consumers, capture),
            vec![(
                0,
                Tune {
                    layers: Some(layers(0, 2)),
                    priority: None
                }
            )],
            "400 px fits the 720 camera's half rung (360), not the 480 one's (240)"
        );
    }

    /// A tile reported without a height (an older client, a widget measured
    /// before layout) keeps the top: no worse picture on missing data.
    #[test]
    fn a_tile_without_a_height_keeps_the_top() {
        let v = view_with(&[(1, Tile::new(0, 0))]);
        let consumers = [ConsumerLine {
            preferred: Some(layers(0, 0)),
            ..simulcast_line(1, false)
        }];
        assert_eq!(
            tune_plan(&v, &consumers, CAPTURE),
            vec![(
                0,
                Tune {
                    layers: Some(TWO.top()),
                    priority: None
                }
            )]
        );
    }
}
