// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-67 P5 / #3332 — the two host-health words the caps carry: the last
//! install's verdict (`last_install`) and the crash ladder's state
//! (`crash_health`). The tests come first; the words follow them.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::notify::{AttentionInfo, REASON_GOODBYE, REASON_ROLLBACK};
    use roomler_ai_remote_control::models::{CrashHealth, LastInstallWord};
    use tunnel_core::localapi::LastInstallStatus;

    /// The build that runs in these cells.
    const RUNNING: &str = "0.4.240";
    const NOW: u64 = 1_795_000_000;

    fn sentinel(reason: &str, failed_version: Option<&str>) -> AttentionInfo {
        AttentionInfo {
            path: std::path::PathBuf::from("needs-attention.txt"),
            message: "a sentence a person reads".into(),
            reason: Some(reason.into()),
            failed_version: failed_version.map(str::to_string),
        }
    }

    /// A config with a crash counted at each of `times`, through the start's
    /// own bookkeeping (`record_crash_at`).
    fn crashed(times: &[u64]) -> config::AgentConfig {
        let mut cfg = config::test_fixture();
        for at in times {
            config::record_crash_at(&mut cfg, *at);
        }
        cfg
    }

    /// #3332 — a config with two crashes inside the window stamps two, with
    /// the time of the last one and the window they count in.
    #[test]
    fn two_crashes_inside_the_window_stamp_two() {
        let cfg = crashed(&[NOW - 120, NOW - 60]);
        let word = crash_health_of(&cfg, &[], RUNNING, NOW);
        assert_eq!(word.state, CrashHealth::CRASHING);
        assert_eq!(word.crashes, 2);
        assert_eq!(word.last_crash_unix, Some(NOW - 60));
        assert_eq!(word.window_secs, config::CRASH_WINDOW_SECS);
        assert_eq!(word.accused_version, None);
    }

    /// #3332 — the clean-run promotion clears it, read fresh: after
    /// `record_clean_run_at` the word says nothing, which is a word of its
    /// own (`ok`), never the absence of one.
    #[test]
    fn after_the_clean_run_promotion_the_word_says_nothing() {
        let mut cfg = crashed(&[NOW - 120, NOW - 60]);
        config::record_clean_run_at(&mut cfg, RUNNING);
        assert_eq!(
            crash_health_of(&cfg, &[], RUNNING, NOW),
            CrashHealth {
                state: CrashHealth::OK.into(),
                crashes: 0,
                window_secs: config::CRASH_WINDOW_SECS,
                last_crash_unix: None,
                accused_version: None,
                last_known_good: Some(RUNNING.into()),
            }
        );
    }

    /// #3332 — the ladder tripped and could not roll back: with a
    /// `rollback_failed` sentinel for the running version the word says so
    /// and names the accused version. Another reason's sentinel beside it
    /// changes nothing. FR-53 keeps that sentinel while the accused build
    /// runs, so the word outlives the clean-run promotion that zeroes the
    /// count.
    #[test]
    fn a_ladder_that_could_not_roll_back_names_the_accused_version() {
        let mut cfg = crashed(&[NOW - 200, NOW - 100, NOW - 10]);
        cfg.last_known_good_version = Some("0.4.239".into());
        config::mark_rollback_attempted(&mut cfg);
        let held = [
            sentinel(REASON_GOODBYE, None),
            sentinel(REASON_ROLLBACK, Some(RUNNING)),
        ];
        let word = crash_health_of(&cfg, &held, RUNNING, NOW);
        assert_eq!(word.state, CrashHealth::ROLLBACK_FAILED);
        assert_eq!(word.accused_version.as_deref(), Some(RUNNING));
        assert_eq!(word.crashes, 3);
        assert_eq!(word.last_crash_unix, Some(NOW - 10));
        assert_eq!(word.last_known_good.as_deref(), Some("0.4.239"));

        config::record_clean_run_at(&mut cfg, RUNNING);
        let word = crash_health_of(&cfg, &held, RUNNING, NOW + 400);
        assert_eq!(word.state, CrashHealth::ROLLBACK_FAILED, "{word:?}");
        assert_eq!(word.accused_version.as_deref(), Some(RUNNING));
        assert_eq!(word.crashes, 0, "the count went with the promotion");
    }

    /// The ladder tripped and its rollback ran: the rolled-back build reads
    /// `rolled_back` until its own clean run. A sentinel that accuses a build
    /// no longer running, or one from before FR-53 that names none, accuses
    /// nothing (the healthy connect's own rule, `rollback_is_stale`).
    #[test]
    fn a_rollback_that_ran_reads_rolled_back_and_a_stale_sentinel_accuses_nothing() {
        let mut cfg = crashed(&[NOW - 200, NOW - 100, NOW - 10]);
        cfg.last_known_good_version = Some("0.4.239".into());
        config::mark_rollback_attempted(&mut cfg);
        let held = [
            sentinel(REASON_ROLLBACK, Some(RUNNING)),
            sentinel(REASON_ROLLBACK, None),
        ];
        let word = crash_health_of(&cfg, &held, "0.4.239", NOW);
        assert_eq!(word.state, CrashHealth::ROLLED_BACK);
        assert_eq!(word.accused_version, None);
        assert_eq!(word.crashes, 3);
        assert_eq!(word.last_known_good.as_deref(), Some("0.4.239"));
    }

    /// Crashes whose window has passed are not inside the current window: the
    /// next crash would start a new one at 1 (`record_crash_at`).
    #[test]
    fn crashes_whose_window_has_passed_count_nothing() {
        let cfg = crashed(&[NOW - 3 * config::CRASH_WINDOW_SECS]);
        let word = crash_health_of(&cfg, &[], RUNNING, NOW);
        assert_eq!(word.state, CrashHealth::OK);
        assert_eq!(word.crashes, 0);
        assert_eq!(word.last_crash_unix, None);
    }

    /// Caps are stored on the server and shown to the org's admins, so only a
    /// version reaches the word, never a path or a name the device holds.
    #[test]
    fn only_a_version_reaches_the_crash_word() {
        let mut cfg = config::test_fixture();
        cfg.last_known_good_version = Some(r"C:\Users\someone\roomlerd.exe".into());
        let word = crash_health_of(&cfg, &[], RUNNING, NOW);
        assert_eq!(word.last_known_good, None);
        assert_eq!(word.state, CrashHealth::OK);
    }

    /// What `post_install::last_install_status` reads back, with a note and a
    /// path that name a person, as a Windows profile path does.
    fn read(status: &str) -> LastInstallStatus {
        LastInstallStatus {
            status: status.into(),
            expected_version: Some("agent-v0.4.240".into()),
            new_binary_version: Some("roomlerd 0.4.240".into()),
            started_unix: Some(NOW - 60),
            finished_unix: Some(NOW - 30),
            installer_exit_code: Some(0),
            note: Some(r"the watcher left C:\Users\someone\AppData\Local\roomler".into()),
            path: Some(r"C:\Users\someone\AppData\Local\roomler\last-install.json".into()),
            daemon: None,
        }
    }

    /// FR-67 P5 — each of the eight words the reader says maps to itself, with
    /// the release it was for, the version the probe read and the times. The
    /// note and the path stay on the device.
    #[test]
    fn each_verdict_maps_to_its_word() {
        for status in LastInstallWord::STATUSES {
            let word = last_install_word(&read(status), NOW);
            assert_eq!(word.status, status);
            assert_eq!(word.expected_version.as_deref(), Some("agent-v0.4.240"));
            assert_eq!(word.new_binary_version.as_deref(), Some("0.4.240"));
            assert_eq!(word.started_unix, Some(NOW - 60));
            assert_eq!(word.finished_unix, Some(NOW - 30));
            let json = serde_json::to_string(&word).unwrap();
            assert!(!json.contains("someone"), "{json}");
        }
        // A word no reader of this build says is not passed on as it is.
        let word = last_install_word(&read("SomethingNew"), NOW);
        assert_eq!(word.status, LastInstallWord::UNREADABLE);
    }

    /// FR-67 P5 — absent, in progress and never reported stay three words,
    /// as P3 keeps them: no record, a watcher still at work, and a record
    /// whose verdict never came once no watcher can be at work on it.
    #[test]
    fn absent_in_progress_and_never_reported_stay_apart() {
        let absent = LastInstallStatus {
            status: LastInstallStatus::ABSENT.into(),
            expected_version: None,
            new_binary_version: None,
            started_unix: None,
            finished_unix: None,
            installer_exit_code: None,
            note: None,
            path: Some("last-install.json".into()),
            daemon: None,
        };
        let word = last_install_word(&absent, NOW);
        assert_eq!(word.status, LastInstallWord::ABSENT);
        assert!(!word.never_reported);

        let mut at_work = read(LastInstallStatus::IN_PROGRESS);
        at_work.finished_unix = None;
        let word = last_install_word(&at_work, NOW);
        assert_eq!(word.status, LastInstallWord::IN_PROGRESS);
        assert!(!word.never_reported, "a watcher can still be at work");
        let word = last_install_word(&at_work, NOW - 60 + LastInstallStatus::STALE_AFTER_SECS);
        assert_eq!(word.status, LastInstallWord::IN_PROGRESS);
        assert!(word.never_reported, "none can be any more");

        let done = read(LastInstallStatus::SUCCEEDED_VERIFIED);
        assert!(!last_install_word(&done, NOW + 100_000).never_reported);
    }
}
