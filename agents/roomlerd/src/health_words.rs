// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-67 P5 / #3332 — the two host-health words the caps carry: the last
//! install's verdict (`last_install`) and the crash ladder's state
//! (`crash_health`).
//!
//! Both come from records on disk: the post-install watcher's
//! `last-install.json` (`post_install::last_install_status`), the daemon's
//! config (the crash ladder, `config::read_crash_ladder`) and its attention
//! sentinels (`notify::read_all_attention`). None of them is read on the
//! signalling loop (#3439's rule): a thread of its own reads them every
//! [`REFRESH`], and at once on [`refresh_soon`], into a cache, and the hello
//! and the heartbeat only ever read the cache ([`current`]). The heartbeat
//! compares what it read with what it last announced and re-announces the caps
//! when a word changed (`signaling::Announced`).
//!
//! Read fresh every time, never from the config the start loaded: the
//! clean-run promotion clears the crash word five minutes into a run, and on
//! Windows the install's verdict lands after the new service started.
//!
//! Only words, versions and unix times leave this module, never a record's
//! note or path nor a sentinel's message: caps are stored on the server and
//! shown to the org's admins, and a device-local path can name a person.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use roomler_ai_remote_control::models::{CrashHealth, LastInstallWord};
use tunnel_core::localapi::LastInstallStatus;

use crate::config::{self, CrashLadder};
use crate::notify::{self, AttentionInfo};

/// How often the records are read again: once a heartbeat, so a change
/// reaches the device's row within two beats.
const REFRESH: Duration = Duration::from_secs(30);

/// How long [`start`] waits for the first read, so that the hello carries
/// both words. The read is a few small local files; a disk that hangs costs
/// the hello its words, never the start, and the beat after the read lands
/// carries them.
const FIRST_READ_WAIT: Duration = Duration::from_secs(2);

/// The words as last read. Both `None` until the first read lands, and in a
/// process that never starts the refresher (a view, FR-43 P3a): the caps then
/// carry neither, as an agent older than the words does.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Words {
    pub last_install: Option<LastInstallWord>,
    pub crash_health: Option<CrashHealth>,
}

struct Cache {
    words: Words,
    /// A read has landed ([`wait_for_the_first_read`]).
    landed: bool,
}

static LATEST: Mutex<Cache> = Mutex::new(Cache {
    words: Words {
        last_install: None,
        crash_health: None,
    },
    landed: false,
});

/// Notified each time a read lands.
static LANDED: Condvar = Condvar::new();

/// Set by [`refresh_soon`]; the refresher waits on it between two reads.
static SOON: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// The words as last read. Never touches the disk.
pub fn current() -> Words {
    LATEST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .words
        .clone()
}

/// Read the records again now, not at the next [`REFRESH`]: the clean-run
/// promotion saved, or the install record was settled. Never blocks, and a
/// call made while a read is running is not lost: the refresher reads once
/// more as soon as that one ends.
pub fn refresh_soon() {
    *SOON.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    SOON.1.notify_all();
}

/// Start the refresher for the daemon whose config is `config_path` (once per
/// process; a second call does nothing), and wait up to [`FIRST_READ_WAIT`]
/// for its first read.
pub fn start(config_path: PathBuf) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("health-words".into())
            .spawn(move || {
                // The last ladder read, kept through a read that fails.
                let mut ladder = None;
                loop {
                    let words = read_now(&config_path, &mut ladder);
                    store(words);
                    wait_for_the_next_refresh(REFRESH);
                }
            });
        match spawned {
            Ok(_) => wait_for_the_first_read(FIRST_READ_WAIT),
            Err(e) => {
                tracing::warn!(%e, "health words: no refresher thread; the caps carry neither word")
            }
        }
    });
}

/// One read of the three records. A ladder that does not read keeps the last
/// one: a save renames a new file into place, and a read that meets it can
/// fail once. Before any ladder was read the crash word is `None`, a device
/// that cannot say.
fn read_now(config_path: &Path, ladder: &mut Option<CrashLadder>) -> Words {
    let now = unix_now();
    let running = env!("CARGO_PKG_VERSION");
    match config::read_crash_ladder(config_path) {
        Ok(read) => *ladder = Some(read),
        Err(e) => tracing::debug!(
            error = %format!("{e:#}"),
            "health words: the crash ladder was not read; its last word stands"
        ),
    }
    let held = notify::read_all_attention();
    Words {
        last_install: Some(last_install_word(
            &crate::post_install::last_install_status(),
            now,
        )),
        crash_health: ladder
            .as_ref()
            .map(|ladder| crash_health_from(ladder, &held, running, now)),
    }
}

/// Keep `words` as the latest, said once per change, so the journal shows
/// what the hello and the heartbeat carry.
fn store(words: Words) {
    let mut cache = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    if !cache.landed || cache.words != words {
        tracing::info!(
            last_install = words
                .last_install
                .as_ref()
                .map_or("-", |w| w.status.as_str()),
            never_reported = words
                .last_install
                .as_ref()
                .is_some_and(|w| w.never_reported),
            crash_health = words
                .crash_health
                .as_ref()
                .map_or("-", |w| w.state.as_str()),
            crashes = words.crash_health.as_ref().map_or(0, |w| w.crashes),
            "health words (FR-67 P5 / #3332)"
        );
    }
    cache.words = words;
    cache.landed = true;
    drop(cache);
    LANDED.notify_all();
}

/// Wait until a read has landed, or `within` has passed.
fn wait_for_the_first_read(within: Duration) {
    let due = Instant::now() + within;
    let mut cache = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    while !cache.landed {
        let left = due.saturating_duration_since(Instant::now());
        if left.is_zero() {
            tracing::warn!(
                ?within,
                "health words: the first read has not landed; the hello carries neither word"
            );
            return;
        }
        cache = LANDED
            .wait_timeout(cache, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// Sleep until the next read is due: `every` from now, or at once when
/// [`refresh_soon`] has asked.
fn wait_for_the_next_refresh(every: Duration) {
    let due = Instant::now() + every;
    let mut soon = SOON.0.lock().unwrap_or_else(|e| e.into_inner());
    while !*soon {
        let left = due.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        soon = SOON
            .1
            .wait_timeout(soon, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    *soon = false;
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// #3332 — the crash word for `ladder`, with `held` the attention sentinels
/// on this host and `running` this binary's own version, at `now_unix`.
///
/// - `crashes` counts only inside the window (`config::CRASH_WINDOW_SECS` from
///   the last crash, `record_crash_at`'s own rule), so the word drops to 0
///   once, when the window passes.
/// - `rollback_failed` when a held sentinel accuses the running build (FR-53's
///   rule, `notify::rollback_accuses`): the ladder tripped and could not roll
///   back. FR-53 keeps that sentinel while the accused build runs, so this
///   outlives the clean-run promotion that zeroes the count.
/// - `rolled_back` when the ladder marked a rollback AND the build running is
///   the one it went back to (`last_known_good_version`, compared as stored and
///   trimmed, a difference and not an order, as `rollback_is_stale` compares).
///   The mark alone is not a rollback that ran: on Linux and macOS a rollback
///   installer that fails after it started brings the accused build back with
///   the mark set and no sentinel. Until the rolled-back build's own clean run.
/// - `crashing` with crashes inside the window, else `ok`.
fn crash_health_from(
    ladder: &CrashLadder,
    held: &[AttentionInfo],
    running: &str,
    now_unix: u64,
) -> CrashHealth {
    let in_window = ladder.last_crash_unix > 0
        && now_unix.saturating_sub(ladder.last_crash_unix) <= config::CRASH_WINDOW_SECS;
    let crashes = if in_window { ladder.crash_count } else { 0 };
    let accused = held
        .iter()
        .any(|info| notify::rollback_accuses(info, running));
    let last_known_good = ladder.last_known_good_version.as_deref().map(str::trim);
    let state = if accused {
        CrashHealth::ROLLBACK_FAILED
    } else if ladder.rollback_attempted && last_known_good == Some(running.trim()) {
        CrashHealth::ROLLED_BACK
    } else if crashes > 0 {
        CrashHealth::CRASHING
    } else {
        CrashHealth::OK
    };
    CrashHealth {
        state: state.into(),
        crashes,
        window_secs: config::CRASH_WINDOW_SECS,
        last_crash_unix: (crashes > 0).then_some(ladder.last_crash_unix),
        accused_version: if accused { version_word(running) } else { None },
        last_known_good: last_known_good.and_then(version_word),
    }
}

/// [`crash_health_from`] for the ladder `cfg` holds.
#[cfg(test)]
fn crash_health_of(
    cfg: &config::AgentConfig,
    held: &[AttentionInfo],
    running: &str,
    now_unix: u64,
) -> CrashHealth {
    crash_health_from(&CrashLadder::of(cfg), held, running, now_unix)
}

/// FR-67 P5 — the install word for what `post_install::last_install_status`
/// read, at `now_unix`. The status passes as it is when it is one of the
/// words ([`LastInstallWord::STATUSES`]), and any other is `Unreadable`, so
/// only an enumerated word reaches the server. `never_reported` is P3's own
/// rule (`LastInstallStatus::never_reported`), and it flips once. The record's
/// note, path, exit code and daemon stay on the device.
fn last_install_word(read: &LastInstallStatus, now_unix: u64) -> LastInstallWord {
    let status = if LastInstallWord::STATUSES.contains(&read.status.as_str()) {
        read.status.clone()
    } else {
        LastInstallWord::UNREADABLE.to_string()
    };
    LastInstallWord {
        status,
        never_reported: read.never_reported(now_unix),
        expected_version: read.expected_version.as_deref().and_then(version_word),
        // The watcher writes what `--version` printed (`roomlerd 0.4.240`):
        // its last word is the version.
        new_binary_version: read
            .new_binary_version
            .as_deref()
            .and_then(|printed| printed.split_whitespace().last())
            .and_then(version_word),
        started_unix: read.started_unix,
        finished_unix: read.finished_unix,
    }
}

/// A version as a word may carry it, kept as given: `0.4.240`, or a tag
/// (`agent-v0.4.240`, `v0.4.240`), at most 64 bytes, a digit after the tag's
/// prefix and nothing but letters, digits and `.+_-`. Anything else (a path, a
/// name, a sentence) is `None`, so nothing the device holds about a person
/// reaches the server through a version field.
fn version_word(raw: &str) -> Option<String> {
    let v = raw.trim();
    let bare = v
        .strip_prefix("agent-v")
        .or_else(|| v.strip_prefix('v'))
        .unwrap_or(v);
    let plain = v.len() <= 64
        && bare.starts_with(|c: char| c.is_ascii_digit())
        && bare
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '_' | '-'));
    plain.then(|| v.to_string())
}

/// Put `words` in the cache, as a read would.
#[cfg(test)]
pub(crate) fn set_for_test(words: Words) {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).words = words;
}

/// Puts the cache back as it was when [`restore_after_test`] made it.
#[cfg(test)]
pub(crate) struct RestoreAfterTest(Words);

#[cfg(test)]
impl Drop for RestoreAfterTest {
    fn drop(&mut self) {
        set_for_test(std::mem::take(&mut self.0));
    }
}

/// The cache as it is now, put back when the guard drops. Callers hold
/// `encode::caps::loop_fixture()`, as every announcer test does, so no other
/// test reads the cache meanwhile.
#[cfg(test)]
pub(crate) fn restore_after_test() -> RestoreAfterTest {
    RestoreAfterTest(current())
}

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

    /// The rollback's mark alone is not a rollback that ran: on Linux and
    /// macOS a rollback installer that fails after it started brings the
    /// accused build back with the mark set and no sentinel. Only the build
    /// the ladder went back to, running, reads `rolled_back`; the accused one
    /// reads what its count says.
    #[test]
    fn rolled_back_needs_the_last_known_good_running_not_the_mark_alone() {
        let mut cfg = crashed(&[NOW - 200, NOW - 100, NOW - 10]);
        cfg.last_known_good_version = Some("0.4.239".into());
        config::mark_rollback_attempted(&mut cfg);
        let back = crash_health_of(&cfg, &[], "0.4.239", NOW);
        assert_eq!(back.state, CrashHealth::ROLLED_BACK, "{back:?}");
        let still = crash_health_of(&cfg, &[], RUNNING, NOW);
        assert_eq!(still.state, CrashHealth::CRASHING, "{still:?}");
        assert_eq!(still.accused_version, None);
        assert_eq!(still.crashes, 3);
        assert_eq!(still.last_known_good.as_deref(), Some("0.4.239"));
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

    /// The validator behind every version field of both words: a version or
    /// a tag passes as given, and nothing else does.
    #[test]
    fn a_version_word_is_a_version_or_nothing() {
        for kept in [
            "0.4.240",
            "agent-v0.4.240",
            "v0.4.240",
            "0.4.240-rc.1",
            "0.4.240+build_7",
        ] {
            assert_eq!(version_word(kept).as_deref(), Some(kept));
        }
        assert_eq!(version_word(" 0.4.240 ").as_deref(), Some("0.4.240"));
        for dropped in [
            r"C:\Users\someone\roomlerd.exe",
            "/home/someone/.local/bin/roomlerd",
            "someone",
            "",
            "agent-v",
            "0.4.240 and more",
        ] {
            assert_eq!(version_word(dropped), None, "{dropped}");
        }
        let long = format!("0.{}", "9".repeat(63));
        assert_eq!(version_word(&long), None, "longer than 64 bytes");
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

    /// The wire's words are the reader's, spelled the same: the server reads
    /// `last_install` with no copy of the reader's crate.
    #[test]
    fn the_wire_words_are_the_readers_words() {
        assert_eq!(
            LastInstallWord::STATUSES,
            [
                LastInstallStatus::ABSENT,
                LastInstallStatus::IN_PROGRESS,
                LastInstallStatus::SUCCEEDED_VERIFIED,
                LastInstallStatus::SUCCEEDED_UNVERIFIED,
                LastInstallStatus::INSTALLER_FAILED,
                LastInstallStatus::TIMEOUT,
                LastInstallStatus::WATCHER_STOPPED,
                LastInstallStatus::UNREADABLE,
            ]
        );
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
