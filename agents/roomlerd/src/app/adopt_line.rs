// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3589 — the last line of `roomlerd adopt-enrollment` says what was at the
//! target.
//!
//! [`apply_enrollment`](roomlerd::enrollment::apply_enrollment) answers
//! `FreshPrimary` for two kinds of target at once: one that was not there, and
//! one that was there and held no enrollment, whose settings it keeps. The
//! outcome cannot tell them apart, so the verb records what its merge-base
//! match found ([`Found`]) and words only the `FreshPrimary` arm from that.
//! Nothing here decides what is written or kept: the save choice, the outcome
//! and the WARNINGs stay where they were.

use super::config;
use std::path::PathBuf;

/// What the merge-base match found at the target, as far as the last line needs.
#[derive(Debug)]
pub(super) enum Found {
    /// Nothing at the target: a first install.
    Absent,
    /// A config that loads.
    Loaded,
    /// A config that does not load. `recovered` says whether its last good copy
    /// (`.prev`) loads, and `kept` where the damaged file went, or why it could
    /// not be kept.
    Unreadable {
        recovered: bool,
        kept: Result<PathBuf, String>,
    },
    /// #3609 — Windows, the machine-global config: there, and not private to
    /// the computer's administrators. Nothing of it is carried over.
    NotPrivate,
}

impl Found {
    /// The record of a config that did not load (`recovered` is what the merge
    /// base handed on, `kept` the result of keeping the damaged file aside).
    pub(super) fn unreadable(
        recovered: &Option<config::AgentConfig>,
        kept: &Result<PathBuf, String>,
    ) -> Self {
        Found::Unreadable {
            recovered: recovered.is_some(),
            kept: kept.clone(),
        }
    }
}

/// The words for the `FreshPrimary` arm of the last line, from what the target
/// held. Every other outcome keeps the words it has in `adopt_enrollment_cmd`.
pub(super) fn fresh_primary(found: &Found) -> String {
    match found {
        Found::Absent => "written fresh (no config was here)".to_string(),
        Found::Loaded => {
            "written into the config here, which held no enrollment (its settings kept)".to_string()
        }
        Found::Unreadable {
            recovered: false,
            kept,
        } => format!(
            "written fresh: the config here could not be read and had no usable last good \
             copy{}",
            kept_clause(kept)
        ),
        Found::Unreadable {
            recovered: true,
            kept,
        } => format!(
            "written into the config's last good copy, which held no enrollment (its settings \
             kept); the config here could not be read{}",
            kept_clause(kept)
        ),
        Found::NotPrivate => "written fresh: the config here was not private to this computer's \
             administrators, so nothing of it was carried over"
            .to_string(),
    }
}

/// Where the damaged file went, or why it did not.
fn kept_clause(kept: &Result<PathBuf, String>) -> String {
    match kept {
        Ok(copy) => format!(" (kept as {})", copy.display()),
        Err(e) => format!(" (and could not be kept: {e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::adopt_enrollment_cmd;
    use super::*;
    use std::path::Path;

    /// R1's target: it does not parse.
    const BROKEN: &[u8] = b"overlay_enabled = ture\n";
    const OLD_LINE: &str = "no config was here";

    /// One row of the table: a temporary directory with the target set up, and
    /// the enrollment the wizard hands over beside it.
    struct Row {
        dir: tempfile::TempDir,
        target: PathBuf,
        from: PathBuf,
        /// The bytes of the config that was at the target, when it loaded.
        before: Option<Vec<u8>>,
    }

    impl Row {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("config.toml");
            let from = dir.path().join("fresh.toml");
            let mut fresh = config::test_fixture();
            fresh.agent_token = "tok-new".into();
            fresh.machine_name = "renamed".into();
            config::save(&from, &fresh).unwrap();
            Row {
                dir,
                target,
                from,
                before: None,
            }
        }

        /// R3 — nothing at the target.
        fn absent() -> Self {
            Self::new()
        }

        /// R1 — a target that does not parse, and no `.prev`.
        fn unreadable() -> Self {
            let row = Self::new();
            std::fs::write(&row.target, BROKEN).unwrap();
            row
        }

        /// R2 — a target that loads and holds no enrollment.
        fn no_enrollment() -> Self {
            let mut row = Self::new();
            config::save(&row.target, &settings_only()).unwrap();
            row.before = Some(std::fs::read(&row.target).unwrap());
            row
        }

        /// R4 — a target that does not parse, whose `.prev` loads and holds no
        /// enrollment.
        fn unreadable_over_no_enrollment() -> Self {
            let mut row = Self::new();
            config::save(&row.target, &settings_only()).unwrap();
            config::save(&row.target, &settings_only()).unwrap(); // rotates the first to .prev
            row.before = Some(std::fs::read(config::prev_path(&row.target)).unwrap());
            std::fs::write(&row.target, BROKEN).unwrap();
            row
        }

        /// R5 — an enrolled target of the same org.
        fn enrolled() -> Self {
            let mut row = Self::new();
            let mut existing = config::test_fixture();
            existing.overlay_enabled = true;
            config::save(&row.target, &existing).unwrap();
            row.before = Some(std::fs::read(&row.target).unwrap());
            row
        }

        fn adopt(&self) -> String {
            adopt_enrollment_cmd(&self.target, &self.from, &[]).unwrap()
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn kept(&self) -> PathBuf {
            config::unreadable_copy_path(&self.target)
        }
    }

    /// A config with settings and no enrollment: what a person's file holds on a
    /// one-row Mac, and what a worker with nothing enrolled writes (FR-43 P3a).
    /// Two settings that differ from a fresh config's, so a carry-over shows.
    fn settings_only() -> config::AgentConfig {
        let mut cfg = config::AgentConfig::for_person_with_no_enrollment("host").unwrap();
        cfg.overlay_enabled = true;
        cfg.keep_busy_enabled = false;
        cfg
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// R3 — nothing at the target: the line is what it was.
    #[test]
    fn absent_target_keeps_its_line() {
        let row = Row::absent();
        let line = row.adopt();
        assert_eq!(
            line,
            format!(
                "Enrollment adopted: written fresh (no config was here). Config written to: {}",
                row.target.display()
            )
        );
    }

    /// R1 — a target that does not load, with no usable `.prev`: the line says it
    /// was there and where it is kept.
    #[test]
    fn unreadable_target_with_no_last_good_copy_says_it_was_there() {
        let row = Row::unreadable();
        let line = row.adopt();
        assert!(!line.contains(OLD_LINE), "{line}");
        assert!(
            line.contains(&row.kept().display().to_string()),
            "names the kept copy: {line}"
        );
        assert!(line.contains("could not be read"), "{line}");
    }

    /// R2 — a target that loads and holds no enrollment: its settings are kept,
    /// and the line says so rather than that no config was here.
    #[test]
    fn target_with_no_enrollment_says_its_settings_were_kept() {
        let row = Row::no_enrollment();
        let line = row.adopt();
        assert!(!line.contains(OLD_LINE), "{line}");
        assert!(line.contains("no enrollment"), "{line}");
        assert!(line.contains("settings kept"), "{line}");
    }

    /// R4 — a target that does not load, whose last good copy loads and holds no
    /// enrollment: the enrollment goes into that copy, and the line says both.
    #[test]
    fn unreadable_target_over_a_last_good_copy_with_no_enrollment() {
        let row = Row::unreadable_over_no_enrollment();
        let line = row.adopt();
        assert!(!line.contains(OLD_LINE), "{line}");
        assert!(
            line.contains(&row.kept().display().to_string()),
            "names the kept copy: {line}"
        );
        assert!(line.contains("settings kept"), "{line}");
    }

    /// R5 — an enrolled target: the lines of the other arms are what they were.
    #[test]
    fn enrolled_target_keeps_the_lines_of_the_other_arms() {
        // The same org: refreshed in place.
        let row = Row::enrolled();
        let line = row.adopt();
        assert_eq!(
            line,
            format!(
                "Enrollment adopted: folded into the existing config (same server + org; operator \
                 settings kept). Config written to: {}",
                row.target.display()
            )
        );

        // Another org: appended beside the primary, and the `--take` keys dropped.
        let row = Row::new();
        config::save(&row.target, &config::test_fixture()).unwrap();
        let mut other = config::test_fixture();
        other.server_url = "https://other.invalid".into();
        other.tenant_id = "tid-other".into();
        other.agent_id = "aid-other".into();
        other.agent_token = "tok-other".into();
        config::save(&row.from, &other).unwrap();
        let line =
            adopt_enrollment_cmd(&row.target, &row.from, &["overlay_enabled".to_string()]).unwrap();
        let head = "Enrollment adopted: appended as an ADDITIONAL org ";
        let tail = format!(
            " (the primary enrollment stays). Config written to: {}",
            row.target.display()
        );
        assert!(line.starts_with(head), "{line}");
        assert!(line.ends_with(&tail), "{line}");
    }

    /// W — what each row writes and keeps. Green before the wording changes and
    /// after it: the line is the only thing this fix touches.
    #[test]
    fn what_is_written_and_kept_is_what_it_was() {
        // R3: a fresh file, nothing kept.
        let row = Row::absent();
        row.adopt();
        assert_eq!(names(row.path()), ["config.toml", "fresh.toml"]);
        assert_eq!(config::load(&row.target).unwrap().agent_token, "tok-new");

        // R1: written fresh, the damaged file kept, no `.prev`.
        let row = Row::unreadable();
        row.adopt();
        assert_eq!(
            names(row.path()),
            ["config.toml", "config.toml.unreadable", "fresh.toml"]
        );
        assert_eq!(std::fs::read(row.kept()).unwrap(), BROKEN);
        let written = config::load(&row.target).unwrap();
        assert_eq!(written.agent_token, "tok-new");
        assert!(!written.overlay_enabled, "nothing to carry over");

        // R2: the settings kept, the config that was here its own `.prev`.
        let row = Row::no_enrollment();
        row.adopt();
        assert_eq!(
            names(row.path()),
            ["config.toml", "config.toml.prev", "fresh.toml"]
        );
        assert_eq!(
            std::fs::read(config::prev_path(&row.target)).unwrap(),
            row.before.clone().unwrap()
        );
        let written = config::load(&row.target).unwrap();
        assert_eq!(written.agent_token, "tok-new");
        assert!(written.overlay_enabled, "kept");
        assert!(!written.keep_busy_enabled, "kept");

        // R4: the settings recovered from `.prev`, the damaged file kept, and
        // `.prev` as it was: a file that did not load never overwrites it.
        let row = Row::unreadable_over_no_enrollment();
        row.adopt();
        assert_eq!(
            names(row.path()),
            [
                "config.toml",
                "config.toml.prev",
                "config.toml.unreadable",
                "fresh.toml"
            ]
        );
        assert_eq!(std::fs::read(row.kept()).unwrap(), BROKEN);
        assert_eq!(
            std::fs::read(config::prev_path(&row.target)).unwrap(),
            row.before.clone().unwrap()
        );
        let written = config::load(&row.target).unwrap();
        assert_eq!(written.agent_token, "tok-new");
        assert!(written.overlay_enabled, "recovered from .prev");
        assert!(!written.keep_busy_enabled, "recovered from .prev");

        // R5: folded in place, the operator's settings kept.
        let row = Row::enrolled();
        row.adopt();
        assert_eq!(
            names(row.path()),
            ["config.toml", "config.toml.prev", "fresh.toml"]
        );
        assert_eq!(
            std::fs::read(config::prev_path(&row.target)).unwrap(),
            row.before.clone().unwrap()
        );
        let written = config::load(&row.target).unwrap();
        assert_eq!(written.agent_token, "tok-new");
        assert!(written.overlay_enabled, "kept");
    }

    /// P — the wording over every row that ends in `FreshPrimary`, with no file
    /// involved: R6 and a damaged file that could not be kept included.
    #[test]
    fn the_wording_of_every_fresh_primary_row() {
        let copy = PathBuf::from("dir").join("config.toml.unreadable");
        let shown = copy.display().to_string();
        let kept = || Ok(copy.clone());
        let lost = || Err("disk full".to_string());
        let rows: Vec<(&str, Found, Vec<&str>)> = vec![
            (
                "R3",
                Found::Absent,
                vec!["written fresh (no config was here)"],
            ),
            (
                "R2",
                Found::Loaded,
                vec!["held no enrollment", "settings kept"],
            ),
            (
                "R1",
                Found::Unreadable {
                    recovered: false,
                    kept: kept(),
                },
                vec!["could not be read", "no usable last good copy", &shown],
            ),
            (
                "R1 not kept",
                Found::Unreadable {
                    recovered: false,
                    kept: lost(),
                },
                vec!["could not be read", "could not be kept: disk full"],
            ),
            (
                "R4",
                Found::Unreadable {
                    recovered: true,
                    kept: kept(),
                },
                vec![
                    "last good copy",
                    "held no enrollment",
                    "settings kept",
                    &shown,
                ],
            ),
            (
                "R4 not kept",
                Found::Unreadable {
                    recovered: true,
                    kept: lost(),
                },
                vec![
                    "last good copy",
                    "settings kept",
                    "could not be kept: disk full",
                ],
            ),
            (
                "R6",
                Found::NotPrivate,
                vec!["not private", "nothing of it was carried over"],
            ),
        ];
        for (row, found, says) in rows {
            let line = fresh_primary(&found);
            for part in says {
                assert!(line.contains(part), "{row}: {part:?} not in {line:?}");
            }
            // Only a target that was not there says so.
            assert_eq!(
                line.contains("no config was here"),
                matches!(found, Found::Absent),
                "{row}: {line:?}"
            );
            // A config that was there and carries nothing over never says its
            // settings were kept; one that was folded into always does.
            let folded = matches!(
                found,
                Found::Loaded
                    | Found::Unreadable {
                        recovered: true,
                        ..
                    }
            );
            assert_eq!(line.contains("settings kept"), folded, "{row}: {line:?}");
        }
    }
}
