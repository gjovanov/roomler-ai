// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4157 — what a `run` does with the config it was pointed at, decided before
//! it takes the single-instance lock.
//!
//! An enrolled run whose `config.toml` is missing runs on the file's last good
//! copy when that copy holds an enrollment: [`config::load`] restores it after
//! the lock, with #4089's line, as it does for every other command. With no copy
//! it can run on, the run ends with `NoConfig` before the lock, as it always
//! has. A worker with no enrollment (FR-43 P3a) never restores: its copy can be
//! the person's retired, enrolled config, and it writes its blank file after the
//! lock, as before. Nor does the enrolled worker FR-43's macOS supervisor starts
//! ([`may_restore`]): it ends with `NoConfig` as before, and its copy is not read.
//!
//! #4379 — nor does that worker restore a config that is there and does not
//! load ([`load`]): on a Mac converted to one row the copy can hold an
//! enrollment that moved to the root daemon. It ends with `ConfigDoesNotLoad`,
//! 78 like `NoConfig`, and both files stay as they are. Every other run restores
//! it, as before, the worker with no enrollment among them.
//!
//! #4176 — a Windows SystemContext worker's machine config is restored only from
//! a copy that passes the checks the config itself would
//! (`config::machine_previous_copy`, through [`config::previous_copy`]): the
//! service host starts that worker for such a copy, through the same function.

use super::config::{self, PreviousCopy};
use roomlerd::daemon_error::ConfigDoesNotLoad;
use roomlerd::supervision::Supervision;
use std::path::Path;

/// What a start does with its config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AtStart {
    /// The file is there. [`load`] reads it, and recovers one that does not
    /// load from its copy, as before, but in the supervisor's enrolled worker
    /// (#4379).
    Load,
    /// The file is missing, and its copy holds an enrollment: [`config::load`]
    /// restores it after the lock.
    Restore,
    /// A worker with no enrollment, and no file: it writes the person's blank
    /// file after the lock, and never asks the copy.
    WriteBlank,
    /// The file is missing, and the run may not restore it ([`may_restore`]) or
    /// no copy holds an enrollment: the run ends with `NoConfig` before the
    /// lock, and writes nothing.
    NoConfig,
}

/// The decision. `copy` is asked only when an enrolled run that may restore
/// ([`may_restore`]) finds no file.
pub(super) fn decide(
    file_there: bool,
    unenrolled: bool,
    may_restore: bool,
    copy: impl FnOnce() -> PreviousCopy,
) -> AtStart {
    if file_there {
        return AtStart::Load;
    }
    if unenrolled {
        return AtStart::WriteBlank;
    }
    if !may_restore {
        // Before the copy is asked, so its copy is never read (`may_restore`).
        return AtStart::NoConfig;
    }
    match copy() {
        PreviousCopy::Enrolled => AtStart::Restore,
        // A copy with no enrollment holds nothing an enrolled run could run on:
        // the device is in `NoConfig`'s state, and a restore would write a file
        // and start a daemon with no server.
        PreviousCopy::NoEnrollment | PreviousCopy::Missing | PreviousCopy::Broken => {
            AtStart::NoConfig
        }
    }
}

/// #4157 — whether a run under `supervision` may restore a missing config at
/// its start: every run but the enrolled worker FR-43's macOS supervisor starts
/// (`run --supervised`), which #4157 never claimed.
pub(super) fn may_restore(supervision: Supervision) -> bool {
    match supervision {
        // A one-row Mac's person `.prev` can hold the enrollment that moved to root (#4184), so
        // this worker never restores it at its start: it keeps #4071's 78, as before.
        Supervision::MacosSupervisor => false,
        Supervision::WindowsScm
        | Supervision::WindowsTask
        | Supervision::Systemd(_)
        | Supervision::Launchd
        | Supervision::None => true,
    }
}

/// [`decide`] for the config at `path` and its copy, as they are now, for a
/// run under `supervision`.
pub(super) fn of(path: &Path, unenrolled: bool, supervision: Supervision) -> AtStart {
    decide(path.exists(), unenrolled, may_restore(supervision), || {
        config::previous_copy(path)
    })
}

/// #4379 — whether a run under `supervision` restores a config that is there
/// and does not load from its last good copy: every run but the enrolled
/// worker FR-43's macOS supervisor starts, the one [`may_restore`] names. The
/// worker with no enrollment keeps it: its screen needs a config that loads,
/// and it runs on no enrollment of the file's.
pub(super) fn restores_a_config_that_does_not_load(
    unenrolled: bool,
    supervision: Supervision,
) -> bool {
    unenrolled || may_restore(supervision)
}

/// #4379 — the load a start makes after the lock, `run_cmd`'s only one:
/// [`config::load`], as before, or, for the run that does not restore
/// ([`restores_a_config_that_does_not_load`]), [`config::load_without_restore`].
/// That one's config that does not load ends the run with
/// [`ConfigDoesNotLoad`]: 78, its line naming the file, its copy not read, and
/// nothing written.
pub(super) fn load(
    path: &Path,
    unenrolled: bool,
    supervision: Supervision,
) -> anyhow::Result<config::AgentConfig> {
    use anyhow::Context as _;
    let path = path.to_path_buf();
    if restores_a_config_that_does_not_load(unenrolled, supervision) {
        return config::load(&path).context("loading config");
    }
    config::load_without_restore(&path)
        .map_err(|e| anyhow::Error::new(ConfigDoesNotLoad::at(&path, &e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomlerd::supervision::SystemdUnit;
    use std::cell::Cell;

    const EVERY_COPY: [PreviousCopy; 4] = [
        PreviousCopy::Enrolled,
        PreviousCopy::NoEnrollment,
        PreviousCopy::Missing,
        PreviousCopy::Broken,
    ];

    /// [`decide`], and whether it asked the copy.
    fn decided(
        file_there: bool,
        unenrolled: bool,
        may_restore: bool,
        copy: PreviousCopy,
    ) -> (AtStart, bool) {
        let asked = Cell::new(false);
        let at = decide(file_there, unenrolled, may_restore, || {
            asked.set(true);
            copy
        });
        (at, asked.get())
    }

    /// A file that is there is loaded, by every run, whatever its copy holds:
    /// the copy is not asked.
    #[test]
    fn a_config_that_is_there_is_loaded_and_its_copy_is_not_asked() {
        for copy in EVERY_COPY {
            for unenrolled in [false, true] {
                for may_restore in [false, true] {
                    assert_eq!(
                        decided(true, unenrolled, may_restore, copy),
                        (AtStart::Load, false),
                        "{copy:?} unenrolled={unenrolled} may_restore={may_restore}"
                    );
                }
            }
        }
    }

    /// #4157 — an enrolled run whose file is missing restores it from a copy
    /// that holds an enrollment, and from no other.
    #[test]
    fn a_missing_config_is_restored_only_from_a_copy_that_holds_an_enrollment() {
        assert_eq!(
            decided(false, false, true, PreviousCopy::Enrolled),
            (AtStart::Restore, true)
        );
    }

    /// #4157 — with no copy, a copy that does not load, or a copy that holds no
    /// enrollment, an enrolled run whose file is missing ends with `NoConfig`, as
    /// before.
    #[test]
    fn a_missing_config_with_no_copy_that_holds_an_enrollment_stays_no_config() {
        for copy in [
            PreviousCopy::Missing,
            PreviousCopy::Broken,
            PreviousCopy::NoEnrollment,
        ] {
            assert_eq!(
                decided(false, false, true, copy),
                (AtStart::NoConfig, true),
                "{copy:?}"
            );
        }
    }

    /// #4157 — a worker with no enrollment never restores, not even from an
    /// enrolled copy (a one-row Mac's retired row): it writes its blank file,
    /// and never asks the copy.
    #[test]
    fn the_worker_with_no_enrollment_never_restores() {
        for copy in EVERY_COPY {
            for may_restore in [false, true] {
                assert_eq!(
                    decided(false, true, may_restore, copy),
                    (AtStart::WriteBlank, false),
                    "{copy:?} may_restore={may_restore}"
                );
            }
        }
    }

    /// #4157 — an enrolled run that may not restore (the macOS supervisor's
    /// worker) whose file is missing ends with `NoConfig`, as before, even
    /// beside an enrolled copy, and never asks its copy.
    #[test]
    fn a_run_that_may_not_restore_ends_with_no_config_and_never_asks_its_copy() {
        for copy in EVERY_COPY {
            assert_eq!(
                decided(false, false, false, copy),
                (AtStart::NoConfig, false),
                "{copy:?}"
            );
        }
    }

    /// #4157 — only the worker FR-43's macOS supervisor starts may not restore
    /// at its start. Every other run may: the Linux system service, a Mac's
    /// root daemon (launchd) and the Windows service host's worker among them.
    #[test]
    fn only_the_macos_supervisors_worker_may_not_restore() {
        assert!(!may_restore(Supervision::MacosSupervisor));
        for supervision in [
            Supervision::WindowsScm,
            Supervision::WindowsTask,
            Supervision::Systemd(SystemdUnit {
                name: "roomlerd.service",
                user: false,
            }),
            Supervision::Systemd(SystemdUnit {
                name: "roomler.service",
                user: true,
            }),
            Supervision::Launchd,
            Supervision::None,
        ] {
            assert!(may_restore(supervision), "{supervision:?}");
        }
    }

    /// Every supervision a run can be under.
    fn every_supervision() -> [Supervision; 7] {
        [
            Supervision::WindowsScm,
            Supervision::WindowsTask,
            Supervision::Systemd(SystemdUnit {
                name: "roomlerd.service",
                user: false,
            }),
            Supervision::Systemd(SystemdUnit {
                name: "roomler.service",
                user: true,
            }),
            Supervision::Launchd,
            Supervision::MacosSupervisor,
            Supervision::None,
        ]
    }

    /// #4379, cell 22 — only the enrolled worker FR-43's macOS supervisor
    /// starts never restores a config that is there and does not load: on a
    /// Mac converted to one row its last good copy can hold an enrollment that
    /// moved to root. Every other run restores it, as before, the worker with
    /// no enrollment among them (its screen needs a config that loads), and
    /// its missing config's decision is [`may_restore`]'s, as before.
    #[test]
    fn only_the_supervisors_enrolled_worker_never_restores_a_config_that_does_not_load() {
        for supervision in every_supervision() {
            for unenrolled in [false, true] {
                let enrolled_worker = supervision == Supervision::MacosSupervisor && !unenrolled;
                assert_eq!(
                    restores_a_config_that_does_not_load(unenrolled, supervision),
                    !enrolled_worker,
                    "{supervision:?} unenrolled={unenrolled}"
                );
            }
            assert_eq!(
                may_restore(supervision),
                supervision != Supervision::MacosSupervisor,
                "{supervision:?}"
            );
        }
    }

    /// [`of`] on real files: a missing config beside an enrolled copy is
    /// restored by an enrolled run, but not by the macOS supervisor's worker; a
    /// worker with no enrollment writes its blank file; and asking writes
    /// nothing.
    #[test]
    fn of_reads_the_files_and_writes_nothing() {
        let mac = Supervision::MacosSupervisor;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let prev = config::prev_path(&path);
        assert_eq!(of(&path, false, Supervision::None), AtStart::NoConfig);
        assert_eq!(of(&path, true, Supervision::None), AtStart::WriteBlank);
        std::fs::write(&prev, toml::to_string(&config::test_fixture()).unwrap()).unwrap();
        assert_eq!(of(&path, false, Supervision::None), AtStart::Restore);
        assert_eq!(of(&path, false, mac), AtStart::NoConfig);
        assert_eq!(of(&path, true, Supervision::None), AtStart::WriteBlank);
        assert_eq!(of(&path, true, mac), AtStart::WriteBlank);
        assert!(!path.exists(), "no live file was written");
        std::fs::write(&path, toml::to_string(&config::test_fixture()).unwrap()).unwrap();
        assert_eq!(of(&path, false, Supervision::None), AtStart::Load);
        assert_eq!(of(&path, true, Supervision::None), AtStart::Load);
        assert_eq!(of(&path, false, mac), AtStart::Load);
    }

    /// `run_cmd` on the config at `path`, started as `run --supervisor
    /// <supervisor>`, with FR-43's `--supervised` when `supervised`. Its future
    /// is the whole daemon's, so it runs on a thread with room for it. Only
    /// where the start ends at its first check: a test never starts a daemon.
    fn run_started_as(
        path: &Path,
        supervisor: Option<&'static str>,
        supervised: bool,
    ) -> anyhow::Result<()> {
        let supervision = roomlerd::supervision::detect_here(supervisor, supervised);
        assert_eq!(
            of(path, false, supervision),
            AtStart::NoConfig,
            "only a start that ends"
        );
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime")
                    .block_on(Box::pin(super::super::run_cmd(
                        &path, None, supervised, false, supervisor, None, None,
                    )))
                    .map(|_| ())
            })
            .expect("thread")
            .join()
            .expect("run_cmd panicked")
    }

    /// #4157 — a missing config beside a copy the start cannot run on, one that
    /// does not load or one that holds no enrollment, ends the real `run` as a
    /// missing config with no copy does (#4023, #4028): `NoConfig` by type, its
    /// words, the service worker's own exit code on Windows, and nothing written.
    #[test]
    fn a_missing_config_whose_copy_cannot_be_run_ends_with_no_config_as_before() {
        let service = if cfg!(windows) {
            roomlerd::daemon_error::NO_ENROLLMENT_EXIT_CODE
        } else {
            super::super::EXIT_CODE_OF_AN_ERR
        };
        let blank = config::AgentConfig::for_person_with_no_enrollment("host").unwrap();
        for copy in [
            "agent_token = \"broken\n".to_string(),
            toml::to_string(&blank).unwrap(),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("config.toml");
            let prev = config::prev_path(&path);
            std::fs::write(&prev, &copy).unwrap();

            let res = run_started_as(&path, Some("scm"), false);
            let (words, code) = super::super::step_outcome(&res);
            assert_eq!(code, service, "{copy}");
            assert_eq!(
                words,
                Some(format!(
                    "no config found at {}. Run `roomlerd enroll` first.",
                    path.display()
                )),
                "{copy}"
            );
            let err = res.expect_err("no config");
            assert!(
                err.downcast_ref::<roomlerd::daemon_error::NoConfig>()
                    .is_some(),
                "{err:#}"
            );
            assert_eq!(std::fs::read_to_string(&prev).unwrap(), copy);
            let names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(
                names,
                vec![std::ffi::OsString::from("config.toml.prev")],
                "nothing written: {copy}"
            );
        }
    }

    /// #4157 — the worker FR-43's macOS supervisor starts (`run --supervised`,
    /// no `--unenrolled`) never restores at its start: on a missing config
    /// beside an enrolled copy, the real `run` ends as with no copy (#4071):
    /// `NoConfig` by type, its words, 78, the copy as it was, and nothing
    /// written. A Mac only, the one platform where that flag proves the
    /// supervisor; anywhere else such a `run` may restore.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_supervisors_worker_never_restores_at_its_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let prev = config::prev_path(&path);
        let copy = toml::to_string(&config::test_fixture()).unwrap();
        std::fs::write(&prev, &copy).unwrap();
        assert_eq!(config::previous_copy(&path), PreviousCopy::Enrolled);

        let res = run_started_as(&path, None, true);
        let (words, code) = super::super::step_outcome(&res);
        assert_eq!(code, roomlerd::daemon_error::NO_ENROLLMENT_EXIT_CODE);
        assert_eq!(
            words,
            Some(format!(
                "no config found at {}. Run `roomlerd enroll` first.",
                path.display()
            ))
        );
        let err = res.expect_err("no config");
        assert!(
            err.downcast_ref::<roomlerd::daemon_error::NoConfig>()
                .is_some(),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), copy);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.toml.prev")]);
    }

    /// What the SYSTEM worker's start reads for a missing machine config's
    /// copy: [`config::previous_copy`] at the machine path, which is
    /// `config::machine_previous_copy` there (`roomler-node-core`'s
    /// `config::tests::machine_copy` holds that on the same trees). Asked
    /// directly: this test's trees are not the computer's own, which is all
    /// `previous_copy` reads at the machine path.
    #[cfg(target_os = "windows")]
    fn worker_copy(tree: &roomler_node_core::machine_tree::MachineTree) -> PreviousCopy {
        config::machine_previous_copy(tree)
    }

    /// #4176 — the service host (`want_system_worker` over
    /// `config_opens_in`), the SYSTEM worker's ladder (`pick_config_path` over
    /// `rung_opens`) and its start ([`decide`]) answer alike in every case a
    /// missing machine config can leave, run in that order, as a host's tick
    /// and then its worker's start run them. With no SYSTEM-profile config,
    /// only a copy that passes the machine's checks and holds an enrollment
    /// starts the SYSTEM worker, whose ladder names the machine config and
    /// whose start restores it; in every other case the host waits and the
    /// start would end with `NoConfig`. With a SYSTEM-profile config, that
    /// config runs, ahead of any copy, and nothing is restored. On trees made
    /// by `machine_tree::test_support`, on CI's elevated runner.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_host_the_ladder_and_the_start_agree_on_a_missing_machine_config() {
        use roomler_node_core::machine_tree::Switches;
        use roomler_node_core::machine_tree::test_support::*;
        use roomlerd::win_service::supervisor::{config_opens_in, rung_opens, want_system_worker};
        if !elevated_or_skip("the host, the ladder and the start on a missing machine config") {
            return;
        }
        let mut wrong = Vec::new();
        for with_profile in [false, true] {
            for case in MissingConfig::ALL {
                let root = tempfile::tempdir().expect("tempdir");
                let tree = missing_config(root.path(), case);
                let system_profile = root.path().join("systemprofile");
                let profile = system_profile.join("AppData/Roaming/roomler/config.toml");
                if with_profile {
                    std::fs::create_dir_all(profile.parent().expect("its folder"))
                        .expect("the profile's folder");
                    std::fs::write(&profile, "agent_token = \"profile\"\n")
                        .expect("the profile's config");
                }
                let host = want_system_worker(true, false, || {
                    config_opens_in(&tree, Switches::ON, Some(profile.clone()))
                });
                let ladder = super::super::pick_config_path(
                    None,
                    true,
                    Some(tree.config().as_path()),
                    &profile,
                    &system_profile,
                    |p| rung_opens(&tree, Switches::ON, p),
                );
                let start = decide(
                    ladder.exists(),
                    false,
                    may_restore(Supervision::WindowsScm),
                    || worker_copy(&tree),
                );
                let want = if with_profile {
                    (true, profile.clone(), AtStart::Load)
                } else if case.restorable() {
                    (true, tree.config(), AtStart::Restore)
                } else {
                    (false, tree.config(), AtStart::NoConfig)
                };
                if (host, ladder.clone(), start) != want {
                    wrong.push(format!(
                        "{case:?}, profile {with_profile}: host={host} ladder={} start={start:?}, \
                         want {want:?}",
                        ladder.display()
                    ));
                }
                assert!(!tree.config().exists(), "{case:?}: nothing written");
            }
        }
        assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    }

    /// While the machine config is there, the service host
    /// (`want_system_worker` over `config_opens_in`), the SYSTEM worker's
    /// ladder (`pick_config_path` over `rung_opens`), its start ([`decide`])
    /// and the machine config's check at that start (`rung`: a SYSTEM worker's
    /// `Run` loads no machine config it refuses) answer alike in every case of
    /// `ConfigPresent`, run in that order, as a host's tick and then its
    /// worker's start run them. A config that runs starts the SYSTEM worker
    /// on it, at every tick. A refused one never runs: with a SYSTEM-profile
    /// config that config runs; without one the host waits, and the ladder
    /// names the machine path, which the start refuses. On trees made by
    /// `machine_tree::test_support`, on CI's elevated runner.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_host_the_ladder_and_the_start_agree_on_a_machine_config_that_is_there() {
        use roomler_node_core::machine_tree::Switches;
        use roomler_node_core::machine_tree::test_support::*;
        use roomlerd::win_service::supervisor::{
            config_opens_in, rung, rung_opens, want_system_worker,
        };
        if !elevated_or_skip("the host, the ladder and the start on a machine config") {
            return;
        }
        let mut wrong = Vec::new();
        for with_profile in [false, true] {
            for case in ConfigPresent::ALL {
                let root = tempfile::tempdir().expect("tempdir");
                let tree = config_present(root.path(), case);
                let system_profile = root.path().join("systemprofile");
                let profile = system_profile.join("AppData/Roaming/roomler/config.toml");
                if with_profile {
                    std::fs::create_dir_all(profile.parent().expect("its folder"))
                        .expect("the profile's folder");
                    std::fs::write(&profile, "agent_token = \"profile\"\n")
                        .expect("the profile's config");
                }
                let host = want_system_worker(true, false, || {
                    config_opens_in(&tree, Switches::ON, Some(profile.clone()))
                });
                let ladder = super::super::pick_config_path(
                    None,
                    true,
                    Some(tree.config().as_path()),
                    &profile,
                    &system_profile,
                    |p| rung_opens(&tree, Switches::ON, p),
                );
                let start = decide(
                    ladder.exists(),
                    false,
                    may_restore(Supervision::WindowsScm),
                    || worker_copy(&tree),
                );
                let runs = rung(&tree, Switches::ON, &tree.config()).is_ok();
                let want = if case.runs() {
                    (true, tree.config(), AtStart::Load, true)
                } else if with_profile {
                    (true, profile.clone(), AtStart::Load, false)
                } else {
                    (false, tree.config(), AtStart::Load, false)
                };
                if (host, ladder.clone(), start, runs) != want {
                    wrong.push(format!(
                        "{case:?}, profile {with_profile}: host={host} ladder={} start={start:?} \
                         runs={runs}, want {want:?}",
                        ladder.display()
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    }
}
