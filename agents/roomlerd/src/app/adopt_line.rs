// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3589 — the last line of `roomlerd adopt-enrollment` says what was at the
//! target. Each cell runs the real verb ([`super::adopt_enrollment_cmd`])
//! against a temporary directory and reads the line it returns, which is the
//! line the person reads.

use super::{adopt_enrollment_cmd, config};
use std::path::{Path, PathBuf};

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
