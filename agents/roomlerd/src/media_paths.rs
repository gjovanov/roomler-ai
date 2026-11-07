// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-135 §7.1 — which media path a session runs, its old code or its new one,
//! decided once at the session's open from two temporary keys.
//!
//! - `rc_media_legacy` sends a session BACK to a path's old code. Pushable
//!   through remote config: it can only ever take a new path away, which is why
//!   `MANAGE_AGENTS` is enough (FR-132 decision 1's reasoning).
//! - `rc_media_canary` turns a path's new code ON before its built-in default
//!   does. Local only, and absent from `DesiredConfig`: turning a path on stays
//!   a decision made on the device or by a release, as `zero_copy` is.
//!
//! Precedence: legacy beats canary, which beats the built-in default. Both are
//! comma lists matched by EQUALITY, never by prefix, and `none` names nothing.
//! An entry this build does not know is ignored and named once in the session's
//! log line, so a later phase's name in a push changes nothing on an older
//! agent.
//!
//! Live, with no restart: `RemoteConfigServices` holds the two keys, and its
//! one read site is a session's open. A session never changes path mid-flight,
//! so a push applies from the device's next session.
//!
//! Some new paths stand on others (§7.1's second table). An old-path entry
//! therefore forces the paths that need it onto the old path too, and a canary
//! entry whose requirement is on the old path is refused. The session's
//! `rc:media-plan` names both, so a reader sees what the keys did.
//!
//! ⚠️ The vocabulary is the spec's, all twelve names, though only the paths
//! [`MediaPath::built`] says exist have new code to choose. A name stays on the
//! list until its phase's delete gate retires the old code (§7.1), and is never
//! reused.

use std::fmt;

/// One media path whose new code a phase of FR-135 adds beside the old.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MediaPath {
    /// R0 — the per-window struct and the session summary.
    Stats,
    /// R2 — `rc:media-plan`.
    MediaPlan,
    /// R4a — capture waits for a frame instead of the pump polling.
    CaptureWait,
    /// R4b — capture owns pacing on a stage of its own.
    CaptureStage,
    /// R5 — two devices with a shared surface pool.
    ZeroCopyShared,
    /// R7 — the data-channel sink without the mutex.
    DcSink,
    /// R14 — the SCTP retransmission floor per association.
    SctpRto,
    /// R8 — the one encode loop.
    EncodeStage,
    /// R10 — the readers of `CellTable`.
    CellTable,
    /// R11 — RTP as a sink of the encode loop.
    RtpSink,
    /// R11b — recording through the one encoder door.
    RecordingDoor,
    /// R12 — the high-refresh ladder and gate.
    HighRefresh,
}

use MediaPath::*;

impl MediaPath {
    /// Every path, in §7.1's order. A path comes after every path it stands
    /// on, which [`resolve`] relies on: it decides each one after its
    /// requirements.
    pub const ALL: [MediaPath; 12] = [
        Stats,
        MediaPlan,
        CaptureWait,
        CaptureStage,
        ZeroCopyShared,
        DcSink,
        SctpRto,
        EncodeStage,
        CellTable,
        RtpSink,
        RecordingDoor,
        HighRefresh,
    ];

    /// The entry the two keys take, and the name `rc:media-plan` reports.
    pub const fn wire(self) -> &'static str {
        match self {
            Stats => "stats",
            MediaPlan => "media_plan",
            CaptureWait => "capture_wait",
            CaptureStage => "capture_stage",
            ZeroCopyShared => "zero_copy_shared",
            DcSink => "dc_sink",
            SctpRto => "sctp_rto",
            EncodeStage => "encode_stage",
            CellTable => "cell_table",
            RtpSink => "rtp_sink",
            RecordingDoor => "recording_door",
            HighRefresh => "high_refresh",
        }
    }

    /// The path an entry names, by EQUALITY: `stat` and `stats_v2` are not
    /// `stats`.
    pub fn from_wire(entry: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.wire() == entry)
    }

    /// The paths whose new code stands on this one's (§7.1's second table). An
    /// old-path entry for `self` forces each of them onto the old path.
    const fn needed_by(self) -> &'static [MediaPath] {
        match self {
            CaptureWait => &[CaptureStage, EncodeStage, HighRefresh],
            CaptureStage => &[EncodeStage, HighRefresh],
            DcSink => &[EncodeStage, HighRefresh],
            EncodeStage => &[RtpSink],
            _ => &[],
        }
    }

    /// The paths this one's new code stands on.
    fn requires(self) -> impl Iterator<Item = MediaPath> {
        Self::ALL
            .into_iter()
            .filter(move |p| p.needed_by().contains(&self))
    }

    /// Whether this build has the path's new code. A canary entry for a path
    /// without it is refused, so a phase's name reads the same on an agent from
    /// before that phase as a typo does.
    pub const fn built(self) -> bool {
        matches!(self, MediaPlan)
    }

    /// The built-in default once no key speaks: the new code, for a path whose
    /// phase has flipped it.
    const fn new_by_default(self) -> bool {
        matches!(self, MediaPlan)
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

impl fmt::Display for MediaPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire())
    }
}

/// Why a canary entry did not turn its path on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `rc_media_legacy` names the same path, and legacy beats canary.
    Legacy,
    /// This build has no new code for the path.
    NotBuilt,
    /// A path it stands on is on the old path.
    Requires(MediaPath),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Legacy => f.write_str("legacy"),
            Refusal::NotBuilt => f.write_str("not_built"),
            Refusal::Requires(p) => write!(f, "requires:{p}"),
        }
    }
}

/// One key's value, read as entries. `none` and blank read as no entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entries {
    pub paths: Vec<MediaPath>,
    /// Entries no path of this build is named: ignored, and logged.
    pub unknown: Vec<String>,
}

/// Read a key's comma list. Entries are trimmed and matched whole, by equality
/// and case-sensitively, as `fps_deny`'s are; a repeated entry counts once.
pub fn parse(value: Option<&str>) -> Entries {
    let mut out = Entries::default();
    for entry in value.unwrap_or("").split(',').map(str::trim) {
        if entry.is_empty() || entry == "none" {
            continue;
        }
        match MediaPath::from_wire(entry) {
            Some(p) if !out.paths.contains(&p) => out.paths.push(p),
            Some(_) => {}
            None if !out.unknown.iter().any(|u| u == entry) => out.unknown.push(entry.to_string()),
            None => {}
        }
    }
    out
}

/// What one session runs, decided at its open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPaths {
    /// The paths on their old code, one bit each.
    old: u16,
    /// `rc_media_legacy`'s entries this build knows, as listed.
    legacy: Vec<MediaPath>,
    /// Paths on their old code because a path they stand on is, and that
    /// `rc_media_legacy` does not list itself.
    implied: Vec<MediaPath>,
    /// `rc_media_canary`'s entries that turned their path on.
    canary: Vec<MediaPath>,
    /// `rc_media_canary`'s entries that did not, and why.
    refused: Vec<(MediaPath, Refusal)>,
    /// Entries of either key that name no path in this build.
    unknown: Vec<String>,
}

impl Default for MediaPaths {
    /// What a device runs with neither key set: every built-in default.
    fn default() -> Self {
        resolve(&Entries::default(), &Entries::default())
    }
}

impl MediaPaths {
    /// Whether this session runs `path`'s new code.
    pub fn is_new(&self, path: MediaPath) -> bool {
        self.old & path.bit() == 0
    }

    /// Whether either key said anything this session should be told about.
    pub fn any_key(&self) -> bool {
        !(self.legacy.is_empty()
            && self.implied.is_empty()
            && self.canary.is_empty()
            && self.refused.is_empty()
            && self.unknown.is_empty())
    }

    /// The paths `rc_media_legacy` sent back, listed or implied, for
    /// `rc:media-plan`: what this session runs on its old code because a key
    /// said so, which a default alone never puts here.
    pub fn legacy_wire(&self) -> Vec<&'static str> {
        self.legacy
            .iter()
            .chain(&self.implied)
            .map(|p| p.wire())
            .collect()
    }

    /// `rc_media_canary`'s refused entries, `path:why`, for `rc:media-plan`.
    pub fn refused_wire(&self) -> Vec<String> {
        self.refused
            .iter()
            .map(|(p, why)| format!("{p}:{why}"))
            .collect()
    }

    /// One line in the session's log when either key spoke, and nothing when
    /// neither did, so the common session logs nothing new.
    pub fn log(&self, session_id: bson::oid::ObjectId) {
        if !self.any_key() {
            return;
        }
        let names = |v: &[MediaPath]| v.iter().map(|p| p.wire()).collect::<Vec<_>>();
        tracing::info!(
            %session_id,
            legacy = ?names(&self.legacy),
            implied = ?names(&self.implied),
            canary = ?names(&self.canary),
            refused = ?self.refused_wire(),
            unknown = ?self.unknown,
            "FR-135 media paths: rc_media_legacy and rc_media_canary decided this session"
        );
    }
}

/// Decide each path, in [`MediaPath::ALL`]'s order: legacy, then canary, then
/// the built-in default. Pure, so every combination is testable.
pub fn resolve(legacy: &Entries, canary: &Entries) -> MediaPaths {
    // Old because `rc_media_legacy` says so, directly or through a path it
    // stands on. ALL's order puts every requirement first, so one pass closes
    // the set.
    let mut forced = 0u16;
    for p in MediaPath::ALL {
        if legacy.paths.contains(&p) || p.requires().any(|r| forced & r.bit() != 0) {
            forced |= p.bit();
        }
    }
    let mut out = MediaPaths {
        old: 0,
        legacy: MediaPath::ALL
            .into_iter()
            .filter(|p| legacy.paths.contains(p))
            .collect(),
        implied: MediaPath::ALL
            .into_iter()
            .filter(|p| forced & p.bit() != 0 && !legacy.paths.contains(p))
            .collect(),
        canary: Vec::new(),
        refused: Vec::new(),
        unknown: legacy.unknown.iter().chain(&canary.unknown).fold(
            Vec::new(),
            |mut acc: Vec<String>, u| {
                if !acc.contains(u) {
                    acc.push(u.clone());
                }
                acc
            },
        ),
    };
    for p in MediaPath::ALL {
        let asked = canary.paths.contains(&p);
        let new = if forced & p.bit() != 0 {
            if asked {
                out.refused.push((p, Refusal::Legacy));
            }
            false
        } else if asked {
            // A requirement decided earlier in this pass, by any of the three.
            match (p.built(), p.requires().find(|r| out.old & r.bit() != 0)) {
                (false, _) => {
                    out.refused.push((p, Refusal::NotBuilt));
                    false
                }
                (true, Some(r)) => {
                    out.refused.push((p, Refusal::Requires(r)));
                    false
                }
                (true, None) => {
                    out.canary.push(p);
                    true
                }
            }
        } else {
            // A default stands on its requirements too, so a phase that flips
            // one path's default before the path under it cannot put new code
            // over old.
            p.new_by_default() && p.requires().all(|r| out.old & r.bit() == 0)
        };
        if !new {
            out.old |= p.bit();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[MediaPath]) -> Entries {
        Entries {
            paths: paths.to_vec(),
            unknown: Vec::new(),
        }
    }

    fn subset(mask: u16) -> Vec<MediaPath> {
        MediaPath::ALL
            .into_iter()
            .filter(|p| mask & p.bit() != 0)
            .collect()
    }

    /// The vocabulary is twelve whole names, each one way round, and none is a
    /// prefix of another: the match is equality, so a prefix would still not
    /// match, but a reader of a list must not have to know that.
    #[test]
    fn every_name_reads_back_as_its_path_and_none_prefixes_another() {
        for p in MediaPath::ALL {
            assert_eq!(MediaPath::from_wire(p.wire()), Some(p), "{p}");
            for q in MediaPath::ALL {
                if p != q {
                    assert!(!q.wire().starts_with(p.wire()), "{p} prefixes {q}");
                }
            }
        }
        assert_eq!(MediaPath::from_wire("stat"), None);
        assert_eq!(MediaPath::from_wire("stats_v2"), None);
        assert_eq!(MediaPath::from_wire("STATS"), None);
        assert_eq!(
            MediaPath::from_wire(" stats"),
            None,
            "parse trims; this does not"
        );
    }

    /// ALL lists a path after every path it stands on, which is what lets
    /// `resolve` decide them in one pass.
    #[test]
    fn every_requirement_comes_first() {
        for (i, p) in MediaPath::ALL.into_iter().enumerate() {
            for r in p.requires() {
                let j = MediaPath::ALL.iter().position(|q| *q == r).unwrap();
                assert!(j < i, "{p} stands on {r}, listed after it");
            }
        }
    }

    /// §7.1's second table, read back from the requirements.
    #[test]
    fn the_dependency_table_is_the_specs() {
        let need = |p: MediaPath| p.requires().collect::<Vec<_>>();
        assert_eq!(need(CaptureStage), vec![CaptureWait]);
        assert_eq!(need(EncodeStage), vec![CaptureWait, CaptureStage, DcSink]);
        assert_eq!(need(HighRefresh), vec![CaptureWait, CaptureStage, DcSink]);
        assert_eq!(need(RtpSink), vec![EncodeStage]);
        for p in [
            Stats,
            MediaPlan,
            CaptureWait,
            ZeroCopyShared,
            DcSink,
            SctpRto,
            CellTable,
            RecordingDoor,
        ] {
            assert!(need(p).is_empty(), "{p}");
        }
    }

    #[test]
    fn a_list_reads_whole_entries_and_skips_none_blanks_and_repeats() {
        let e = parse(Some(
            " media_plan, ,stats,none, media_plan ,later_phase,later_phase",
        ));
        assert_eq!(e.paths, vec![MediaPlan, Stats]);
        assert_eq!(e.unknown, vec!["later_phase".to_string()]);
        assert_eq!(parse(Some("none")), Entries::default());
        assert_eq!(parse(Some("")), Entries::default());
        assert_eq!(parse(None), Entries::default());
    }

    /// With neither key set a session runs the built-in defaults, and says
    /// nothing about keys.
    #[test]
    fn no_key_is_the_defaults() {
        let d = MediaPaths::default();
        for p in MediaPath::ALL {
            assert_eq!(d.is_new(p), p.new_by_default(), "{p}");
        }
        assert!(!d.any_key());
        assert!(d.legacy_wire().is_empty() && d.refused_wire().is_empty());
    }

    /// The push this R0 build exists to carry: `media_plan` off for the next
    /// session, named on nothing because the plan itself is what it stops.
    #[test]
    fn legacy_media_plan_sends_the_plan_back() {
        let s = resolve(&entries(&[MediaPlan]), &Entries::default());
        assert!(!s.is_new(MediaPlan));
        assert_eq!(s.legacy_wire(), vec!["media_plan"]);
        assert!(s.any_key());
    }

    /// Legacy beats canary, and the canary entry says why it lost.
    #[test]
    fn legacy_beats_canary() {
        let s = resolve(&entries(&[MediaPlan]), &entries(&[MediaPlan]));
        assert!(!s.is_new(MediaPlan));
        assert_eq!(s.refused, vec![(MediaPlan, Refusal::Legacy)]);
        assert_eq!(s.refused_wire(), vec!["media_plan:legacy"]);
    }

    /// An old-path entry forces the paths that stand on it, and names them.
    #[test]
    fn an_old_path_forces_what_stands_on_it() {
        let s = resolve(&entries(&[CaptureWait]), &Entries::default());
        assert_eq!(s.legacy, vec![CaptureWait]);
        assert_eq!(
            s.implied,
            vec![CaptureStage, EncodeStage, RtpSink, HighRefresh]
        );
        for p in [CaptureWait, CaptureStage, EncodeStage, RtpSink, HighRefresh] {
            assert!(!s.is_new(p), "{p}");
        }
        let s = resolve(&entries(&[DcSink]), &Entries::default());
        assert_eq!(s.implied, vec![EncodeStage, RtpSink, HighRefresh]);
    }

    /// A canary entry for a path this build has no new code for is refused,
    /// and so is one whose requirement is on the old path.
    #[test]
    fn a_canary_entry_is_refused_without_its_code_or_its_requirement() {
        let s = resolve(&Entries::default(), &entries(&[EncodeStage]));
        assert!(!s.is_new(EncodeStage));
        assert_eq!(s.refused, vec![(EncodeStage, Refusal::NotBuilt)]);
        assert_eq!(s.refused_wire(), vec!["encode_stage:not_built"]);
    }

    /// Unknown names are reported once, from either key, and change nothing.
    #[test]
    fn an_unknown_entry_changes_nothing_and_is_named_once() {
        let s = resolve(&parse(Some("later,stats_v2")), &parse(Some("later")));
        assert_eq!(s.unknown, vec!["later".to_string(), "stats_v2".to_string()]);
        for p in MediaPath::ALL {
            assert_eq!(s.is_new(p), p.new_by_default(), "{p}");
        }
        assert!(s.any_key(), "it is still worth a log line");
    }

    /// §7.1: "A unit test walks every combination." Every legacy subset of the
    /// twelve, against no canary, each single canary entry, and every canary
    /// subset against no legacy: about 57,000 sessions, each held to the rules.
    #[test]
    fn every_combination_keeps_the_rules() {
        let check = |legacy: &[MediaPath], canary: &[MediaPath]| {
            let s = resolve(&entries(legacy), &entries(canary));
            for p in MediaPath::ALL {
                // 1. A listed legacy entry is always on the old path.
                if legacy.contains(&p) {
                    assert!(!s.is_new(p), "{legacy:?} {canary:?}: {p} listed");
                }
                // 2. Nothing runs new code over an old requirement.
                if s.is_new(p) {
                    for r in p.requires() {
                        assert!(s.is_new(r), "{legacy:?} {canary:?}: {p} over old {r}");
                    }
                }
                // 3. New code needs either the default or a canary that took.
                if s.is_new(p) {
                    assert!(
                        p.new_by_default() || s.canary.contains(&p),
                        "{legacy:?} {canary:?}: {p} new for no reason"
                    );
                }
                // 4. A canary entry either took or says why not, never both.
                if canary.contains(&p) {
                    let took = s.canary.contains(&p);
                    let refused = s.refused.iter().any(|(q, _)| *q == p);
                    assert!(took != refused, "{legacy:?} {canary:?}: {p}");
                    assert_eq!(took, s.is_new(p), "{legacy:?} {canary:?}: {p}");
                }
                // 5. Implied means forced and unlisted.
                if s.implied.contains(&p) {
                    assert!(!legacy.contains(&p) && !s.is_new(p));
                }
            }
        };
        for mask in 0u16..(1 << 12) {
            let legacy = subset(mask);
            check(&legacy, &[]);
            for c in MediaPath::ALL {
                check(&legacy, &[c]);
            }
            check(&[], &legacy);
        }
    }
}
