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
//! lock, as before.

use super::config::{self, PreviousCopy};
use std::path::Path;

/// What a start does with its config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AtStart {
    /// The file is there. [`config::load`] reads it, and recovers one that does
    /// not load from its copy, as before.
    Load,
    /// The file is missing, and its copy holds an enrollment: [`config::load`]
    /// restores it after the lock.
    Restore,
    /// A worker with no enrollment, and no file: it writes the person's blank
    /// file after the lock, and never asks the copy.
    WriteBlank,
    /// The file is missing, and no copy holds an enrollment: the run ends with
    /// `NoConfig` before the lock, and writes nothing.
    NoConfig,
}

/// The decision. `copy` is asked only when an enrolled run finds no file.
pub(super) fn decide(
    file_there: bool,
    unenrolled: bool,
    copy: impl FnOnce() -> PreviousCopy,
) -> AtStart {
    if file_there {
        return AtStart::Load;
    }
    if unenrolled {
        return AtStart::WriteBlank;
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

/// [`decide`] for the config at `path` and its copy, as they are now.
pub(super) fn of(path: &Path, unenrolled: bool) -> AtStart {
    decide(path.exists(), unenrolled, || config::previous_copy(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const EVERY_COPY: [PreviousCopy; 4] = [
        PreviousCopy::Enrolled,
        PreviousCopy::NoEnrollment,
        PreviousCopy::Missing,
        PreviousCopy::Broken,
    ];

    /// [`decide`], and whether it asked the copy.
    fn decided(file_there: bool, unenrolled: bool, copy: PreviousCopy) -> (AtStart, bool) {
        let asked = Cell::new(false);
        let at = decide(file_there, unenrolled, || {
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
                assert_eq!(
                    decided(true, unenrolled, copy),
                    (AtStart::Load, false),
                    "{copy:?} unenrolled={unenrolled}"
                );
            }
        }
    }

    /// #4157 — an enrolled run whose file is missing restores it from a copy
    /// that holds an enrollment, and from no other.
    #[test]
    fn a_missing_config_is_restored_only_from_a_copy_that_holds_an_enrollment() {
        assert_eq!(
            decided(false, false, PreviousCopy::Enrolled),
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
                decided(false, false, copy),
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
            assert_eq!(
                decided(false, true, copy),
                (AtStart::WriteBlank, false),
                "{copy:?}"
            );
        }
    }

    /// [`of`] on real files: a missing config beside an enrolled copy is
    /// restored by an enrolled run, a worker with no enrollment writes its blank
    /// file, and asking writes nothing.
    #[test]
    fn of_reads_the_files_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let prev = config::prev_path(&path);
        assert_eq!(of(&path, false), AtStart::NoConfig);
        assert_eq!(of(&path, true), AtStart::WriteBlank);
        std::fs::write(&prev, toml::to_string(&config::test_fixture()).unwrap()).unwrap();
        assert_eq!(of(&path, false), AtStart::Restore);
        assert_eq!(of(&path, true), AtStart::WriteBlank);
        assert!(!path.exists(), "no live file was written");
        std::fs::write(&path, toml::to_string(&config::test_fixture()).unwrap()).unwrap();
        assert_eq!(of(&path, false), AtStart::Load);
        assert_eq!(of(&path, true), AtStart::Load);
    }

    /// `run_cmd`, started as the Windows service host's worker (`run
    /// --supervisor scm`), on the config at `path`. Its future is the whole
    /// daemon's, so it runs on a thread with room for it. Only where the start
    /// ends at its first check: a test never starts a daemon.
    fn run_as_the_service_worker(path: &Path) -> anyhow::Result<()> {
        assert_eq!(of(path, false), AtStart::NoConfig, "only a start that ends");
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime")
                    .block_on(Box::pin(super::super::run_cmd(
                        &path,
                        None,
                        false,
                        false,
                        Some("scm"),
                        None,
                        None,
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

            let res = run_as_the_service_worker(&path);
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
}
