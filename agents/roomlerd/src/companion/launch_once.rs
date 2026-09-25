// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-84 D6 — open the desktop companion ONCE after a fresh install.
//!
//! Every install path ends with the daemon starting (the MSI's service
//! start, `install.ps1`'s restart, the wizard, `install.sh`'s `systemctl
//! enable --now`), so the daemon is the one place that covers them all. What
//! it must never do is the opposite failure: resurrect a companion a person
//! quit. So the launch is guarded by a MARKER in the daemon's own data dir —
//! written BEFORE the spawn, and never again once written — and by a
//! fresh-install test (`last_known_good_version` is unset until a daemon has
//! run healthily for five minutes, so an UPGRADED install is adopted, not
//! launched into).
//!
//! This module holds the decision (pure, table-tested) and the marker
//! handling; `super::launch_once_after_install` drives it with real facts.

use std::path::Path;

/// The marker's file name, in the launcher's data dir.
pub(crate) const MARKER_FILE: &str = "companion-first-launch.json";

/// What the launcher can see at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LaunchFacts {
    /// The marker exists: this install's launch was already decided.
    pub marker_present: bool,
    /// The device's `companion_autostart` (ON when no config is readable).
    pub autostart_enabled: bool,
    /// A config is readable — the device is enrolled.
    pub enrolled: bool,
    /// No daemon had run healthily on this install before this start
    /// (latched at the first config seen; see [`FreshLatch`]).
    pub fresh_install: bool,
    /// The companion executable is installed.
    pub companion_present: bool,
    /// An interactive user session exists to open it in.
    pub session: bool,
    /// The companion is already running in that session.
    pub companion_running: bool,
}

/// Why the launcher is still waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitReason {
    /// No config yet (the wizard enrolls after its MSI starts the service).
    NotEnrolled,
    /// No companion yet (`install.sh` installs its .deb after the daemon's).
    NoCompanion,
    /// Nobody is logged on; the login autostart covers them later.
    NoSession,
}

/// What to do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchDecision {
    /// The marker exists — nothing, ever again, for this install.
    AlreadyDecided,
    /// `companion_autostart = false` — nothing, and no marker either.
    Disabled,
    /// An install that ran before this daemon: record the decision, launch
    /// nothing (an upgrade must not pop the companion up, and must not bring
    /// back one that was quit).
    Adopt,
    /// Someone already has it open: record the decision, launch nothing.
    RecordRunning,
    /// Not yet; look again shortly.
    Wait(WaitReason),
    /// Write the marker, then open the Welcome.
    Launch,
}

/// The decision, cheapest facts first.
pub(crate) fn decide(_f: &LaunchFacts) -> LaunchDecision {
    // STUB (RED stage).
    LaunchDecision::Launch
}

/// Freshness is decided ONCE, at the first config the launcher sees, and
/// then held: the daemon it is part of promotes `last_known_good_version`
/// after five healthy minutes, and a fresh install that is still waiting for
/// its companion at that point must not flip to "upgrade".
#[derive(Debug, Default)]
pub(crate) struct FreshLatch(Option<bool>);

impl FreshLatch {
    /// Observe a config's `last_known_good_version.is_none()`; the first
    /// observation sticks.
    pub(crate) fn observe(&mut self, _lkgv_unset: bool) -> bool {
        // STUB (RED stage).
        false
    }
}

/// How writing the marker and launching went.
#[derive(Debug)]
pub(crate) enum LaunchOutcome {
    Launched,
    /// The marker was already there — someone else decided first.
    AlreadyDecided,
    /// The marker could not be written: NOTHING was launched.
    MarkerFailed(String),
    /// The marker was written but the spawn failed; the marker is released
    /// again so a later start can try (nobody saw a window, so there is
    /// nothing to "resurrect").
    SpawnFailed(String),
}

/// Record `outcome` in the marker at `path` without launching anything
/// (the `Adopt` / `RecordRunning` decisions). `Ok(false)` = it already
/// existed.
pub(crate) fn record(_path: &Path, _outcome: &str) -> std::io::Result<bool> {
    // STUB (RED stage).
    Ok(true)
}

/// Write the marker FIRST, then `spawn` — and never `spawn` when the write
/// fails or the marker already exists.
pub(crate) fn launch_behind_marker(
    _path: &Path,
    spawn: impl FnOnce() -> anyhow::Result<()>,
) -> LaunchOutcome {
    // STUB (RED stage): spawn first, no marker at all.
    match spawn() {
        Ok(()) => LaunchOutcome::Launched,
        Err(e) => LaunchOutcome::SpawnFailed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Everything true for a launch.
    fn ready() -> LaunchFacts {
        LaunchFacts {
            marker_present: false,
            autostart_enabled: true,
            enrolled: true,
            fresh_install: true,
            companion_present: true,
            session: true,
            companion_running: false,
        }
    }

    #[test]
    fn decision_table() {
        use LaunchDecision::*;
        use WaitReason::*;
        let r = ready();
        let table: &[(&str, LaunchFacts, LaunchDecision)] = &[
            ("all set", r, Launch),
            (
                "the marker wins over everything (a quit companion stays quit)",
                LaunchFacts {
                    marker_present: true,
                    ..r
                },
                AlreadyDecided,
            ),
            (
                "marker + kill switch",
                LaunchFacts {
                    marker_present: true,
                    autostart_enabled: false,
                    ..r
                },
                AlreadyDecided,
            ),
            (
                "kill switch",
                LaunchFacts {
                    autostart_enabled: false,
                    ..r
                },
                Disabled,
            ),
            (
                "kill switch beats waiting",
                LaunchFacts {
                    autostart_enabled: false,
                    enrolled: false,
                    ..r
                },
                Disabled,
            ),
            (
                "not enrolled yet",
                LaunchFacts {
                    enrolled: false,
                    ..r
                },
                Wait(NotEnrolled),
            ),
            (
                "an upgrade is adopted, not launched into",
                LaunchFacts {
                    fresh_install: false,
                    ..r
                },
                Adopt,
            ),
            (
                "an upgrade is adopted even with nobody logged on",
                LaunchFacts {
                    fresh_install: false,
                    session: false,
                    companion_present: false,
                    ..r
                },
                Adopt,
            ),
            (
                "the companion is not installed yet",
                LaunchFacts {
                    companion_present: false,
                    ..r
                },
                Wait(NoCompanion),
            ),
            (
                "nobody is logged on: no marker, the login start covers it",
                LaunchFacts {
                    session: false,
                    ..r
                },
                Wait(NoSession),
            ),
            (
                "already open: record, do not launch a second",
                LaunchFacts {
                    companion_running: true,
                    ..r
                },
                RecordRunning,
            ),
        ];
        for (why, facts, want) in table {
            assert_eq!(decide(facts), *want, "{why}: {facts:?}");
        }
    }

    #[test]
    fn freshness_is_latched_at_the_first_observation() {
        let mut fresh = FreshLatch::default();
        assert!(fresh.observe(true), "no healthy run yet ⇒ fresh");
        assert!(
            fresh.observe(false),
            "the daemon's own five-minute promotion must not turn a waiting fresh install into an upgrade"
        );
        let mut upgrade = FreshLatch::default();
        assert!(!upgrade.observe(false));
        assert!(!upgrade.observe(true));
    }

    #[test]
    fn the_marker_is_written_before_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILE);
        let spawned = Cell::new(false);
        let out = launch_behind_marker(&marker, || {
            assert!(
                marker.exists(),
                "the marker must exist before anything is launched"
            );
            spawned.set(true);
            Ok(())
        });
        assert!(matches!(out, LaunchOutcome::Launched), "{out:?}");
        assert!(spawned.get());
        let raw = std::fs::read_to_string(&marker).unwrap();
        assert!(raw.contains("\"launched\""), "{raw}");
    }

    #[test]
    fn a_failed_marker_write_launches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // The marker's parent is a regular FILE, so the write must fail.
        let not_a_dir = dir.path().join("plain-file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let marker = not_a_dir.join(MARKER_FILE);
        let spawned = Cell::new(false);
        let out = launch_behind_marker(&marker, || {
            spawned.set(true);
            Ok(())
        });
        assert!(matches!(out, LaunchOutcome::MarkerFailed(_)), "{out:?}");
        assert!(!spawned.get(), "no marker ⇒ no launch");
    }

    #[test]
    fn an_existing_marker_launches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILE);
        std::fs::write(&marker, b"{}").unwrap();
        let spawned = Cell::new(false);
        let out = launch_behind_marker(&marker, || {
            spawned.set(true);
            Ok(())
        });
        assert!(matches!(out, LaunchOutcome::AlreadyDecided), "{out:?}");
        assert!(!spawned.get());
    }

    #[test]
    fn a_failed_spawn_releases_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILE);
        let marker_seen = Cell::new(false);
        let out = launch_behind_marker(&marker, || {
            marker_seen.set(marker.exists());
            anyhow::bail!("no such file")
        });
        assert!(matches!(out, LaunchOutcome::SpawnFailed(_)), "{out:?}");
        assert!(
            marker_seen.get(),
            "the spawn was attempted behind a written marker"
        );
        assert!(
            !marker.exists(),
            "nobody saw a window: a later start may try again"
        );
    }

    #[test]
    fn record_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("sub").join(MARKER_FILE);
        assert!(record(&marker, "adopted").unwrap(), "first record writes");
        assert!(
            !record(&marker, "already_running").unwrap(),
            "the first decision stands"
        );
        let raw = std::fs::read_to_string(&marker).unwrap();
        assert!(raw.contains("\"adopted\""), "{raw}");
    }
}
