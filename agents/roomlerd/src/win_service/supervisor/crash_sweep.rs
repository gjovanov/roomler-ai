// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3799 — the service's crash sweep, on a thread of its own.
//!
//! On an attended host the worker runs as the signed-in person. It uploads the
//! supervisor's crash records from the machine's `crashes`, but it may not
//! delete them there (`Seal::Readable`: Users read and list, nobody else
//! writes). So it writes each record's signature in its own ledger (#1748), and
//! before #3799 the record stayed for good: any other account that drained it
//! sent it again, and it counted toward the recorder's cap of 100
//! (`crash_recorder::HARD_CAP`), past which the supervisor records no crash at
//! all.
//!
//! This host runs as SYSTEM, and it removes them
//! (`crash_recorder::sweep_settled`):
//!
//! | When | Ledgers | Waited for |
//! |---|---|---|
//! | before every worker start | every profile's, and `service-logs`' | up to [`BEFORE_START_WAIT`] |
//! | every [`PERSON_SWEEP_EVERY`] while a person's worker runs | that person's, and `service-logs`' | never |
//!
//! So a record one account has sent is gone before the next drainer starts (a
//! SystemContext worker, or another person), and within a minute while the
//! same person's worker runs.
//!
//! The sweep reads ledgers and deletes records; it never sends anything. Its
//! I/O runs on this thread, never on the supervisor's: a sweep that hangs on a
//! profile's disk holds a worker's start for [`BEFORE_START_WAIT`] at most, and
//! the periodic one holds nothing. One sweep runs at a time, and what is asked
//! while one runs is folded into the next. It logs counts and the names of the
//! records it removed, never a ledger's text: a person writes that.
//!
//! `ROOMLERD_CRASH_SWEEP=0` (or `false`, `no`, `off`) turns it off, and records
//! then stay as they did before #3799.

#![cfg(target_os = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The longest a worker's start waits for the sweep before it: the bound for
/// a sweep that hangs on a profile's disk. Part of the supervisor's
/// `SHUTDOWN_BOUND`.
pub(super) const BEFORE_START_WAIT: Duration = Duration::from_secs(3);

/// How often a running person's ledger is swept.
pub(super) const PERSON_SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The supervisor, right before it spawns a worker of either kind: the
/// sweep against every ledger, waited for [`BEFORE_START_WAIT`] at most.
pub(super) fn before_start(sweep: Option<&CrashSweep>) {
    if let Some(sweep) = sweep {
        sweep.before_start();
    }
}

/// The supervisor's loop, on each pass: while the person with the SID
/// `person` runs a worker, a sweep of that person's ledger every
/// [`PERSON_SWEEP_EVERY`], never waited for.
pub(super) fn every_minute(sweep: Option<&CrashSweep>, person: Option<&str>, last: &mut Instant) {
    if let (Some(sweep), Some(sid)) = (sweep, person)
        && last.elapsed() >= PERSON_SWEEP_EVERY
    {
        sweep.person(sid);
        *last = Instant::now();
    }
}

/// What a sweep reads its ledgers from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Job {
    /// Every profile's: before a worker starts.
    Full,
    /// The profile of the account with this SID: while its worker runs.
    Person(String),
}

#[derive(Default)]
struct State {
    /// Full sweeps asked for, and the last one done.
    full_asked: u64,
    full_done: u64,
    /// A person's sweep asked for since the last one began.
    person: Option<String>,
    /// The host is done with the sweep: the thread ends.
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// The sweep's thread, owned by the supervisor. Dropping it ends the thread
/// once the sweep it runs returns; nothing waits for that.
pub(super) struct CrashSweep {
    shared: Arc<Shared>,
    wait: Duration,
}

impl CrashSweep {
    /// The sweep, or `None` when `ROOMLERD_CRASH_SWEEP` turns it off or its
    /// thread cannot start (each logged).
    pub(super) fn start() -> Option<Self> {
        if !enabled(tunnel_core::env::node_env("CRASH_SWEEP").as_deref()) {
            tracing::warn!(
                "crash sweep: off (ROOMLERD_CRASH_SWEEP); a record a person's worker sent \
                 stays in crashes"
            );
            return None;
        }
        Self::with(sweep, BEFORE_START_WAIT)
    }

    /// The sweep with `job` as its work and `wait` as its bound: the tests'
    /// way in.
    fn with(job: impl Fn(&Job) + Send + 'static, wait: Duration) -> Option<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let theirs = shared.clone();
        match std::thread::Builder::new()
            .name("roomlerd-crash-sweep".into())
            .spawn(move || serve(&theirs, job))
        {
            Ok(_) => Some(Self { shared, wait }),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "crash sweep: no thread; a record a person's worker sent stays in crashes"
                );
                None
            }
        }
    }

    /// Before a worker starts: a sweep against every ledger, waited for
    /// [`BEFORE_START_WAIT`] at most. `true` when it ended in that time.
    pub(super) fn before_start(&self) -> bool {
        let mut state = lock(&self.shared);
        state.full_asked += 1;
        let asked = state.full_asked;
        self.shared.changed.notify_all();
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, self.wait, |s| s.full_done < asked)
            .unwrap_or_else(PoisonError::into_inner);
        let done = state.full_done >= asked;
        drop(state);
        if !done {
            tracing::warn!(
                bound_ms = self.wait.as_millis() as u64,
                "crash sweep: still running; the worker starts without waiting for it"
            );
        }
        done
    }

    /// While the person with `sid` runs a worker: a sweep against their
    /// ledger, never waited for.
    pub(super) fn person(&self, sid: &str) {
        let mut state = lock(&self.shared);
        state.person = Some(sid.to_string());
        self.shared.changed.notify_all();
    }
}

impl Drop for CrashSweep {
    fn drop(&mut self) {
        lock(&self.shared).closed = true;
        self.shared.changed.notify_all();
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The thread: one sweep at a time, a full one before a person's.
fn serve(shared: &Shared, job: impl Fn(&Job)) {
    loop {
        let (next, full_upto) = {
            let mut state = lock(shared);
            loop {
                if state.closed {
                    return;
                }
                if state.full_asked > state.full_done {
                    break (Job::Full, Some(state.full_asked));
                }
                if let Some(sid) = state.person.take() {
                    break (Job::Person(sid), None);
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&next)));
        if ran.is_err() {
            tracing::warn!(
                full = full_upto.is_some(),
                "crash sweep: the sweep panicked"
            );
        }
        if let Some(upto) = full_upto {
            let mut state = lock(shared);
            state.full_done = state.full_done.max(upto);
            shared.changed.notify_all();
        }
    }
}

/// The sweep itself: the profiles the job names, swept, and one line when it
/// did anything. Counts and record names only.
fn sweep(job: &Job) {
    let profiles: Vec<PathBuf> = match job {
        Job::Full => profile_sids()
            .iter()
            .filter_map(|sid| crate::appdirs::profile_of_account(sid))
            .collect(),
        Job::Person(sid) => crate::appdirs::profile_of_account(sid)
            .into_iter()
            .collect(),
    };
    let swept = crate::crash_recorder::sweep_settled(&profiles);
    if swept.removed.is_empty() && swept.failed == 0 {
        if swept.ledgers_refused > 0 {
            tracing::debug!(
                ledgers_unread = swept.ledgers_refused,
                "crash sweep: some ledgers were not read"
            );
        }
        return;
    }
    tracing::info!(
        removed = swept.removed.len(),
        records = ?swept.removed,
        kept = swept.kept,
        failed = swept.failed,
        ledgers = swept.ledgers_read,
        ledgers_unread = swept.ledgers_refused,
        before_start = matches!(job, Job::Full),
        "crash sweep: removed the records a drainer had sent"
    );
}

/// `ROOMLERD_CRASH_SWEEP`: on unless it says `0`, `false`, `no` or `off`.
fn enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|v| {
        let v = v.trim();
        ["0", "false", "no", "off"]
            .iter()
            .any(|off| v.eq_ignore_ascii_case(off))
    })
}

/// The accounts the machine keeps a profile for: the subkeys of
/// `ProfileList`, each a SID. Each one's folder is then the machine's record
/// of it (`appdirs::profile_of_account`), never a name.
fn profile_sids() -> Vec<String> {
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegCloseKey, RegEnumKeyExW,
        RegOpenKeyExW,
    };
    let path: Vec<u16> = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: a NUL-terminated name that outlives the call, and a valid
    // out-pointer.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if rc != ERROR_SUCCESS || key.is_null() {
        return Vec::new();
    }
    let mut sids = Vec::new();
    for index in 0u32..4096 {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: `name` holds `len` wide chars; the optional out-params are
        // null, as the API allows.
        let rc = unsafe {
            RegEnumKeyExW(
                key,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc == ERROR_SUCCESS {
            sids.push(String::from_utf16_lossy(&name[..len as usize]));
        }
    }
    // SAFETY: opened above, closed once.
    unsafe { RegCloseKey(key) };
    sids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    /// A start waits for the sweep it asked for: the sweep has run when
    /// `before_start` returns.
    #[test]
    fn a_start_waits_for_the_sweep_it_asked_for() {
        let ran = Arc::new(AtomicUsize::new(0));
        let counted = ran.clone();
        let sweep = CrashSweep::with(
            move |job| {
                if *job == Job::Full {
                    std::thread::sleep(Duration::from_millis(100));
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            },
            Duration::from_secs(10),
        )
        .expect("the sweep's thread");
        assert!(sweep.before_start(), "the sweep ended in time");
        assert_eq!(ran.load(Ordering::SeqCst), 1, "it ran before the start");
        assert!(sweep.before_start());
        assert_eq!(ran.load(Ordering::SeqCst), 2, "each start has its own");
    }

    /// A sweep that hangs (a profile's disk that does not answer) holds a
    /// start for the bound and no longer, and a person's sweep asked for
    /// meanwhile returns at once.
    #[test]
    fn a_sweep_that_hangs_holds_a_start_for_the_bound_and_no_longer() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let bound = Duration::from_millis(300);
        let sweep = CrashSweep::with(
            move |_| {
                let _ = released.lock().unwrap().recv();
            },
            bound,
        )
        .expect("the sweep's thread");
        let began = Instant::now();
        assert!(!sweep.before_start(), "the hung sweep did not end");
        let held = began.elapsed();
        assert!(
            held >= bound && held < bound + Duration::from_secs(2),
            "held for {held:?}, bound {bound:?}"
        );
        let began = Instant::now();
        sweep.person("S-1-5-21-1-2-3-1001");
        assert!(began.elapsed() < Duration::from_millis(100));
        // The test's own thread ends: release it, then let the host drop it.
        let _ = release.send(());
        let _ = release.send(());
        drop(sweep);
    }

    /// A person's sweeps asked for while one runs are folded into one, and a
    /// start asked for meanwhile is swept before them.
    #[test]
    fn what_is_asked_while_a_sweep_runs_is_folded_into_the_next() {
        let (began_tx, began) = mpsc::channel::<Job>();
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let sweep = CrashSweep::with(
            move |job| {
                let _ = began_tx.send(job.clone());
                let _ = released.lock().unwrap().recv();
            },
            Duration::from_secs(10),
        )
        .expect("the sweep's thread");
        sweep.person("S-1");
        assert_eq!(
            began.recv_timeout(Duration::from_secs(5)).unwrap(),
            Job::Person("S-1".into())
        );
        sweep.person("S-2");
        sweep.person("S-3");
        let start = std::thread::scope(|s| {
            let waiting = s.spawn(|| sweep.before_start());
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
            assert_eq!(
                began.recv_timeout(Duration::from_secs(5)).unwrap(),
                Job::Full,
                "the start's sweep goes first"
            );
            release.send(()).unwrap();
            waiting.join().unwrap()
        });
        assert!(start, "the start's sweep ended");
        assert_eq!(
            began.recv_timeout(Duration::from_secs(5)).unwrap(),
            Job::Person("S-3".into()),
            "one person's sweep, the last asked for"
        );
        release.send(()).unwrap();
        assert!(began.recv_timeout(Duration::from_millis(300)).is_err());
    }

    /// The kill switch.
    #[test]
    fn only_an_off_word_turns_the_sweep_off() {
        assert!(enabled(None));
        for on in ["1", "true", "on", "", "yes please"] {
            assert!(enabled(Some(on)), "{on:?}");
        }
        for off in ["0", "false", "FALSE", " no ", "off"] {
            assert!(!enabled(Some(off)), "{off:?}");
        }
    }

    /// The bound a start may be held for is short, and the periodic sweep
    /// runs well inside the uploader's own drain interval.
    #[test]
    fn the_start_bound_is_short_and_the_cadence_a_minute() {
        assert!(BEFORE_START_WAIT <= Duration::from_secs(5));
        assert!(PERSON_SWEEP_EVERY <= Duration::from_secs(60));
    }

    /// Every worker start in the supervisor waits for the sweep first: both
    /// arms, the person's and the SystemContext one, ask for it before they
    /// spawn.
    #[test]
    fn every_worker_start_asks_for_the_sweep_first() {
        let source = include_str!("../supervisor.rs");
        let spawns = [
            "unsafe { spawn_in_session(spawn_handle,",
            "winlogon_token::spawn_system_in_session(&token,",
        ];
        for spawn in spawns {
            let at = source
                .find(spawn)
                .unwrap_or_else(|| panic!("the spawn {spawn:?} is in supervisor.rs"));
            let arm = &source[..at];
            let arm = &arm[arm.rfind("SpawnDecision::").unwrap_or(0)..];
            assert!(
                arm.contains("crash_sweep::before_start("),
                "the arm that spawns with {spawn:?} asks for the sweep first"
            );
        }
    }
}
