// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-127 P1 — receive what is visible: what a viewer says it renders, and
//! what that means for each of its consumers.
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
//! Everything in this module is pure; the room manager clones the consumer
//! handles out of its maps and awaits the worker only after every guard is
//! dropped (`RoomManager::apply_view`).

use std::collections::HashMap;

use mediasoup::prelude::{MediaKind, ProducerId};

/// What a view-capable client last reported it renders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewState {
    /// The video producers this viewer shows, with the tile's size in device
    /// pixels. P1 keeps the size for P2, which maps it to a simulcast layer.
    pub wanted: HashMap<ProducerId, (u32, u32)>,
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
        I: IntoIterator<Item = (ProducerId, (u32, u32))>,
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
/// docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerLine {
    pub producer_id: ProducerId,
    pub kind: MediaKind,
    pub paused: bool,
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
        }
    }

    fn view_of(ids: &[u8]) -> ViewState {
        let mut v = ViewState::default();
        assert!(v.apply(1, ids.iter().map(|n| (pid(*n), (320, 240))), 49));
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
        assert!(v.apply(2, [(pid(3), (640, 480))], 49));
        assert_eq!(v.wanted.len(), 1);
        assert!(v.wanted.contains_key(&pid(3)));
        assert_eq!(v.wanted[&pid(3)], (640, 480));
        assert_eq!(v.seq, 2);
    }

    /// A report sent before the one held, arriving after it, is dropped; a
    /// re-send of the same report (equal `seq`) is applied, idempotently.
    #[test]
    fn an_older_report_is_dropped_and_an_equal_one_reapplied() {
        let mut v = view_of(&[1]);
        assert!(v.apply(5, [(pid(2), (0, 0))], 49));
        assert!(
            !v.apply(4, [(pid(3), (0, 0))], 49),
            "seq 4 after 5 is stale"
        );
        assert!(
            v.wanted.contains_key(&pid(2)),
            "the stale report changed nothing"
        );
        assert!(!v.wanted.contains_key(&pid(3)));
        assert_eq!(v.seq, 5);
        assert!(v.apply(5, [(pid(2), (0, 0))], 49), "the same report again");
        assert_eq!(v.wanted.len(), 1);
    }

    /// The first report a fresh state sees is applied whatever its `seq`.
    #[test]
    fn the_first_report_is_applied_at_any_seq() {
        let mut v = ViewState::default();
        assert!(v.apply(0, [(pid(1), (0, 0))], 49));
        assert!(v.wanted.contains_key(&pid(1)));
    }

    #[test]
    fn a_long_report_is_truncated_in_the_order_sent() {
        let mut v = ViewState::default();
        assert!(v.apply(1, [3, 1, 2].into_iter().map(|n| (pid(n), (0, 0))), 2));
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
}
