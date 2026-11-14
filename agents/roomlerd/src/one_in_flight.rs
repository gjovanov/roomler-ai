// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3796 — one blocking lookup of a kind in flight at a time, its answer
//! shared by whoever asks while it runs.
//!
//! A Linux system install asks other processes who is at seat0: logind,
//! then the account database. Either can hang (a stopped logind, an SSSD or
//! LDAP server refreshing a large group list), and a thread blocked on one
//! cannot be called back. Each caller already waited a bounded time for its
//! answer (`files::SEAT_LOOKUP_WAIT`), but each also started its own lookup,
//! so N retries while the lookup hung held N blocking threads.
//!
//! [`OneInFlight`] runs at most one lookup at a time, on a thread of its own,
//! and each caller waits for an answer within its own wait:
//!
//! - **A lookup that began at or after a caller asked answers that caller.**
//!   It is the lookup that caller's question would have started, so its
//!   answer is the one the caller would have had.
//! - **A lookup that began before is waited out, and its answer is not
//!   taken.** That answer may be older than the question. A lookup that hung
//!   until logind's D-Bus call timed out, or one that read the seat before
//!   the account database stalled, answers for a moment that has passed.
//!   Once that lookup ends, the caller starts a fresh lookup, or joins the
//!   fresh one another caller started.
//! - **No second lookup starts while one runs**, so a hang holds one thread,
//!   however many callers ask.
//! - **A late answer reaches no one.** Every caller it could answer has
//!   given up by then. Its end only lets the next lookup start.
//!
//! The cost: with a slow lookup that still works, taking `d`, a caller who
//! asks `e` after a lookup began waits `(d - e) + d`. It is answered only if
//! that fits in its wait. If `d` is at most half the wait, every caller is
//! answered.
//!
//! #3830 — [`Remembered`] keeps the last answer of a kind of lookup for the
//! callers that may not wait at all, over the same one lookup in flight. A
//! status poll or a heartbeat reads the last answer, however old, and starts
//! a lookup when it is older than its window ([`Remembered::now`]). What
//! decides takes an answer from inside the window, or waits for a lookup
//! within its wait ([`Remembered::current`]). A fresh check takes only a
//! lookup that began after it asked ([`Remembered::fresh`]). The lookup's own
//! thread stores what it answers, so an answer that nobody waits for any more
//! still lands, and the lock on the last answer is never held across a
//! lookup.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// What a caller got from [`OneInFlight::ask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Asked<T> {
    /// The answer of a lookup that began at or after the caller asked.
    Answer(T),
    /// That lookup ended with no answer, and why: it panicked, or its thread
    /// could not start.
    Failed(String),
    /// No such lookup answered within the caller's wait.
    OutOfTime,
}

impl<T> From<Result<T, String>> for Asked<T> {
    fn from(ended: Result<T, String>) -> Self {
        match ended {
            Ok(answer) => Self::Answer(answer),
            Err(why) => Self::Failed(why),
        }
    }
}

/// At most one lookup of a kind in flight, its answer shared by the rules of
/// this module.
pub(crate) struct OneInFlight<T> {
    /// What the lookup's thread is called (`/proc/<pid>/task/*/comm` shows
    /// 15 bytes of it).
    thread_name: &'static str,
    /// The last lookup started: running, or ended.
    last: Mutex<Option<Arc<Flight<T>>>>,
}

/// One lookup: when it began, and how it ended once it has.
struct Flight<T> {
    began: Instant,
    ended: Mutex<Option<Result<T, String>>>,
    /// Wakes a caller waiting on a blocking thread.
    ended_for_threads: Condvar,
    /// Wakes a caller waiting in an async task.
    ended_for_tasks: tokio::sync::Notify,
}

impl<T> Flight<T> {
    fn new() -> Self {
        Self {
            began: Instant::now(),
            ended: Mutex::new(None),
            ended_for_threads: Condvar::new(),
            ended_for_tasks: tokio::sync::Notify::new(),
        }
    }

    fn end(&self, how: Result<T, String>) {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = Some(how);
        self.ended_for_threads.notify_all();
        self.ended_for_tasks.notify_waiters();
    }

    fn is_running(&self) -> bool {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }
}

impl<T: Clone> Flight<T> {
    fn how_it_ended(&self) -> Option<Result<T, String>> {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How it ended, waiting until `deadline` at most: `None` while it still
    /// runs then.
    async fn ended_by(&self, deadline: Instant) -> Option<Result<T, String>> {
        let deadline = tokio::time::Instant::from_std(deadline);
        loop {
            let ended = self.ended_for_tasks.notified();
            tokio::pin!(ended);
            // Registered before the look below, so an end between the two
            // still wakes this caller.
            ended.as_mut().enable();
            if let Some(how) = self.how_it_ended() {
                return Some(how);
            }
            if tokio::time::timeout_at(deadline, ended).await.is_err() {
                return None;
            }
        }
    }

    /// [`Self::ended_by`] on a blocking thread.
    #[cfg(any(target_os = "linux", test))]
    fn ended_by_blocking(&self, deadline: Instant) -> Option<Result<T, String>> {
        let mut ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(how) = ended.as_ref() {
                return Some(how.clone());
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())?;
            ended = self
                .ended_for_threads
                .wait_timeout(ended, left)
                .map(|(ended, _)| ended)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

impl<T: Clone + Send + 'static> OneInFlight<T> {
    pub(crate) const fn new(thread_name: &'static str) -> Self {
        Self {
            thread_name,
            last: Mutex::new(None),
        }
    }

    /// The answer of a lookup that began at or after this call, within
    /// `wait`. `lookup` runs only when no lookup is running by the time this
    /// caller's turn comes (the module's rules).
    pub(crate) async fn ask<F>(&self, wait: Duration, lookup: F) -> Asked<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let asked = Instant::now();
        let deadline = asked + wait;
        // A lookup that began before this question is waited out, its answer
        // left to the callers it was asked for.
        while let Some(earlier) = self.running_from_before(asked) {
            if earlier.ended_by(deadline).await.is_none() {
                return Asked::OutOfTime;
            }
        }
        // Then the lookup that began since, or this caller's own.
        match self.join_or_start(lookup).ended_by(deadline).await {
            Some(how) => how.into(),
            None => Asked::OutOfTime,
        }
    }

    /// [`Self::ask`] from a blocking thread.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn ask_blocking<F>(&self, wait: Duration, lookup: F) -> Asked<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let asked = Instant::now();
        let deadline = asked + wait;
        while let Some(earlier) = self.running_from_before(asked) {
            if earlier.ended_by_blocking(deadline).is_none() {
                return Asked::OutOfTime;
            }
        }
        match self.join_or_start(lookup).ended_by_blocking(deadline) {
            Some(how) => how.into(),
            None => Asked::OutOfTime,
        }
    }

    /// #3830 — `lookup`, started unless a lookup is running, and waited for
    /// by nobody: a refresh for the callers that may not wait at all
    /// ([`Remembered::now`]). Whoever asks meanwhile takes its answer by this
    /// module's rules.
    #[cfg(any(all(target_os = "linux", feature = "recording"), test))]
    pub(crate) fn start_unless_running<F>(&self, lookup: F)
    where
        F: FnOnce() -> T + Send + 'static,
    {
        drop(self.join_or_start(lookup));
    }

    /// The lookup running now, when it began before `asked`.
    fn running_from_before(&self, asked: Instant) -> Option<Arc<Flight<T>>> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|flight| flight.is_running() && flight.began < asked)
            .cloned()
    }

    /// The lookup running now, or `lookup`, started now. Every lookup that
    /// started after a caller found none from before its question began
    /// after that question, so either answers it.
    fn join_or_start<F>(&self, lookup: F) -> Arc<Flight<T>>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = last.as_ref().filter(|flight| flight.is_running()) {
            return running.clone();
        }
        let flight = Arc::new(Flight::new());
        *last = Some(flight.clone());
        drop(last);
        self.start(flight.clone(), lookup);
        flight
    }

    /// `lookup` on a thread of its own, which ends `flight` however it goes.
    fn start<F>(&self, flight: Arc<Flight<T>>, lookup: F)
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let ends = flight.clone();
        let started = std::thread::Builder::new()
            .name(self.thread_name.to_string())
            .spawn(move || {
                let how = std::panic::catch_unwind(std::panic::AssertUnwindSafe(lookup))
                    .map_err(|panic| format!("it panicked: {}", panic_text(panic.as_ref())));
                ends.end(how);
            });
        if let Err(e) = started {
            flight.end(Err(format!("its thread did not start: {e}")));
        }
    }
}

/// What a panic said, when it said it in words.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".into())
}

/// #3830 — the last answer of a kind of lookup, for the callers that may not
/// wait at all, over one [`OneInFlight`] (the module's rules). The daemon's
/// are statics, so the methods take `&'static self`: the lookup's own thread
/// stores what it answers.
#[cfg(any(all(target_os = "linux", feature = "recording"), test))]
pub(crate) struct Remembered<T> {
    /// How long an answer counts as current.
    window: Duration,
    /// The last answer, and when the lookup that gave it BEGAN: a lookup
    /// that hung answers for the moment it asked, not for the moment it
    /// ended. Locked only to read it or to replace it, never across a lookup.
    last: Mutex<Option<(Instant, T)>>,
    lookups: OneInFlight<T>,
}

#[cfg(any(all(target_os = "linux", feature = "recording"), test))]
impl<T: Clone + Send + 'static> Remembered<T> {
    /// Lookups on a thread named `thread_name`, each answer current for
    /// `window`.
    pub(crate) const fn new(thread_name: &'static str, window: Duration) -> Self {
        Self {
            window,
            last: Mutex::new(None),
            lookups: OneInFlight::new(thread_name),
        }
    }

    /// The last answer, however old, and never a wait: `None` before the
    /// first one. When there is none, or it is older than the window,
    /// `lookup` starts unless a lookup is running, and what it answers is
    /// stored when it ends.
    pub(crate) fn now<F>(&'static self, lookup: F) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let last = self.last_answer();
        if last
            .as_ref()
            .is_none_or(|(began, _)| began.elapsed() >= self.window)
        {
            self.lookups.start_unless_running(self.storing(lookup));
        }
        last.map(|(_, answer)| answer)
    }

    /// An answer from inside the window, or else the answer of a lookup that
    /// began at or after this call, within `wait` ([`OneInFlight::ask`]).
    pub(crate) async fn current<F>(&'static self, wait: Duration, lookup: F) -> Asked<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        if let Some((began, answer)) = self.last_answer()
            && began.elapsed() < self.window
        {
            return Asked::Answer(answer);
        }
        self.fresh(wait, lookup).await
    }

    /// Only the answer of a lookup that began at or after this call, within
    /// `wait`: never the stored one, however new.
    pub(crate) async fn fresh<F>(&'static self, wait: Duration, lookup: F) -> Asked<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        self.lookups.ask(wait, self.storing(lookup)).await
    }

    /// The stored answer and when its lookup began.
    fn last_answer(&self) -> Option<(Instant, T)> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `lookup`, storing what it answers as it ends, on its own thread.
    fn storing<F>(&'static self, lookup: F) -> impl FnOnce() -> T + Send + 'static
    where
        F: FnOnce() -> T + Send + 'static,
    {
        move || {
            let began = Instant::now();
            let answer = lookup();
            let mut slot = self.last.lock().unwrap_or_else(PoisonError::into_inner);
            // Lookups run one at a time, so each began after the one before
            // it ended; the check only keeps an older answer from replacing
            // a newer one.
            if slot.as_ref().is_none_or(|(stored, _)| *stored <= began) {
                *slot = Some((began, answer.clone()));
            }
            drop(slot);
            answer
        }
    }
}

/// #3853 — [`OneInFlight`]'s rules for each key on its own: at most one
/// lookup in flight for a key, its answer shared by whoever asks for that key
/// while it runs, and never by a caller who asked before it began. A lookup
/// for one key never waits on, joins or answers another key's.
///
/// A key's entry lives only while its lookup runs. The lookup's own thread
/// removes it as the lookup ends, answered, failed or panicked, so the map
/// holds the keys whose lookups run and nothing else, whatever keys were
/// asked before.
///
/// Exec's and SSH's account lookups are keyed by the account's name
/// (`exec::unix_priv`). While the account database hangs, the asks for one
/// account hold one thread, however many there are, and each other account
/// asked holds one of its own. The daemon's is a static, so the methods take
/// `&'static self`: the lookup's own thread removes its key.
///
/// #3855 — so are the account lookups of the callers on blocking threads
/// (`drop_to_std`, `account_ids`, `account_home`; uids per uid), through
/// [`Self::ask_blocking`]: an ask from a blocking thread and one from a task
/// share the account's lookup.
pub(crate) struct OneInFlightPerKey<K, T> {
    /// What each lookup's thread is called.
    thread_name: &'static str,
    /// The lookup running for each key.
    running: Mutex<std::collections::BTreeMap<K, Arc<Flight<T>>>>,
}

impl<K: Ord + Clone + Send + 'static, T: Clone + Send + 'static> OneInFlightPerKey<K, T> {
    pub(crate) const fn new(thread_name: &'static str) -> Self {
        Self {
            thread_name,
            running: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// The answer of a lookup for `key` that began at or after this call,
    /// within `wait`: [`OneInFlight::ask`]'s rules, for `key` alone.
    pub(crate) async fn ask<F>(&'static self, key: K, wait: Duration, lookup: F) -> Asked<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let asked = Instant::now();
        let deadline = asked + wait;
        // A lookup for this key that began before this question is waited
        // out, its answer left to the callers it was asked for.
        while let Some(earlier) = self.running_from_before(&key, asked) {
            if earlier.ended_by(deadline).await.is_none() {
                return Asked::OutOfTime;
            }
        }
        // Then the lookup for this key that began since, or this caller's own.
        match self.join_or_start(key, lookup).ended_by(deadline).await {
            Some(how) => how.into(),
            None => Asked::OutOfTime,
        }
    }

    /// #3855 — [`Self::ask`] from a blocking thread, by the same rules and
    /// through the same lookups: an ask for `key` from a blocking thread and
    /// one from a task share its lookup.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn ask_blocking<F>(&'static self, key: K, wait: Duration, lookup: F) -> Asked<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let asked = Instant::now();
        let deadline = asked + wait;
        while let Some(earlier) = self.running_from_before(&key, asked) {
            if earlier.ended_by_blocking(deadline).is_none() {
                return Asked::OutOfTime;
            }
        }
        match self.join_or_start(key, lookup).ended_by_blocking(deadline) {
            Some(how) => how.into(),
            None => Asked::OutOfTime,
        }
    }

    /// How many keys have a lookup running: everything the map holds.
    #[cfg(test)]
    pub(crate) fn keys_in_flight(&self) -> usize {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The lookup running for `key`, when it began before `asked`.
    fn running_from_before(&self, key: &K, asked: Instant) -> Option<Arc<Flight<T>>> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .filter(|flight| flight.is_running() && flight.began < asked)
            .cloned()
    }

    /// The lookup running for `key`, or `lookup`, started now.
    fn join_or_start<F>(&'static self, key: K, lookup: F) -> Arc<Flight<T>>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(flight) = running.get(&key).filter(|flight| flight.is_running()) {
            return flight.clone();
        }
        let flight = Arc::new(Flight::new());
        running.insert(key.clone(), flight.clone());
        drop(running);
        self.start(key, flight.clone(), lookup);
        flight
    }

    /// `lookup` on a thread of its own, which ends `flight` however it goes
    /// and then removes `key`'s entry.
    fn start<F>(&'static self, key: K, flight: Arc<Flight<T>>, lookup: F)
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let ends = flight.clone();
        let ended = key.clone();
        let started = std::thread::Builder::new()
            .name(self.thread_name.to_string())
            .spawn(move || {
                let how = std::panic::catch_unwind(std::panic::AssertUnwindSafe(lookup))
                    .map_err(|panic| format!("it panicked: {}", panic_text(panic.as_ref())));
                ends.end(how);
                self.forget(&ended, &ends);
            });
        if let Err(e) = started {
            flight.end(Err(format!("its thread did not start: {e}")));
            self.forget(&key, &flight);
        }
    }

    /// `key`'s entry goes once `flight`, its lookup, has ended, unless a
    /// newer lookup for the key has already taken its place.
    fn forget(&self, key: &K, flight: &Arc<Flight<T>>) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(entry, flight))
        {
            running.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// Long enough that only a held lookup runs out of it.
    const LONG: Duration = Duration::from_secs(10);
    /// Short enough that a test waiting it out several times stays quick.
    const SHORT: Duration = Duration::from_millis(100);

    /// Lookups held until the gate opens, counted as they begin.
    #[derive(Clone, Default)]
    struct Gate(Arc<GateState>);

    #[derive(Default)]
    struct GateState {
        open: Mutex<bool>,
        opened: Condvar,
        began: AtomicUsize,
    }

    impl Gate {
        /// A lookup that waits for the gate, then answers `answer`.
        fn lookup(&self, answer: u32) -> impl FnOnce() -> u32 + Send + 'static {
            let gate = self.clone();
            move || {
                gate.0.began.fetch_add(1, SeqCst);
                let open = gate.0.open.lock().unwrap_or_else(PoisonError::into_inner);
                drop(gate.0.opened.wait_while(open, |open| !*open));
                answer
            }
        }

        fn open(&self) {
            *self.0.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.0.opened.notify_all();
        }

        fn began(&self) -> usize {
            self.0.began.load(SeqCst)
        }
    }

    /// Opens its gate when dropped: a failed assertion leaves nothing held.
    struct OpenedAtTheEnd(Gate);

    impl Drop for OpenedAtTheEnd {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    /// Callers who ask while a lookup hangs, in a row or at once, from a
    /// task or a blocking thread, wait on that one lookup within their own
    /// waits, and none starts another. Once it ends, its late answer is
    /// nobody's: the next caller asks afresh.
    #[tokio::test]
    async fn callers_while_a_lookup_hangs_share_it() {
        let lookups = OneInFlight::new("test-lookup");
        let gate = Gate::default();
        let _opened = OpenedAtTheEnd(gate.clone());
        for _ in 0..3 {
            assert_eq!(lookups.ask(SHORT, gate.lookup(1)).await, Asked::OutOfTime);
        }
        let (a, b, c) = tokio::join!(
            lookups.ask(SHORT, gate.lookup(2)),
            lookups.ask(SHORT, gate.lookup(3)),
            lookups.ask(SHORT, gate.lookup(4)),
        );
        assert_eq!(
            [a, b, c],
            [Asked::OutOfTime, Asked::OutOfTime, Asked::OutOfTime]
        );
        assert_eq!(
            lookups.ask_blocking(SHORT, gate.lookup(5)),
            Asked::OutOfTime
        );
        assert_eq!(gate.began(), 1, "one lookup, however many asked");
        gate.open();
        assert_eq!(lookups.ask(LONG, gate.lookup(6)).await, Asked::Answer(6));
        assert_eq!(gate.began(), 2, "the next caller's own lookup");
    }

    /// A caller who asks after a lookup began does not take that lookup's
    /// answer, which may be older than the question: it waits it out and
    /// takes a fresh one's. The caller the first lookup began for takes its
    /// answer.
    #[tokio::test]
    async fn an_answer_from_before_the_question_is_not_taken() {
        let lookups = Arc::new(OneInFlight::new("test-lookup"));
        let gate = Gate::default();
        let _opened = OpenedAtTheEnd(gate.clone());
        let first = tokio::spawn({
            let (lookups, lookup) = (lookups.clone(), gate.lookup(1));
            async move { lookups.ask(LONG, lookup).await }
        });
        while gate.began() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let later = tokio::spawn({
            let (lookups, lookup) = (lookups.clone(), gate.lookup(2));
            async move { lookups.ask(LONG, lookup).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(gate.began(), 1, "the later caller waits, starting nothing");
        gate.open();
        assert_eq!(first.await.unwrap(), Asked::Answer(1));
        assert_eq!(later.await.unwrap(), Asked::Answer(2), "a fresh lookup's");
        assert_eq!(gate.began(), 2);
    }

    /// Callers who wait out the same earlier lookup share the fresh one that
    /// starts when it ends: one more lookup between them, one answer.
    #[tokio::test]
    async fn callers_waiting_out_one_lookup_share_the_next() {
        let lookups = Arc::new(OneInFlight::new("test-lookup"));
        let (earlier, fresh) = (Gate::default(), Gate::default());
        let _opened = (
            OpenedAtTheEnd(earlier.clone()),
            OpenedAtTheEnd(fresh.clone()),
        );
        let first = tokio::spawn({
            let (lookups, lookup) = (lookups.clone(), earlier.lookup(1));
            async move { lookups.ask(LONG, lookup).await }
        });
        while earlier.began() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let waiting: Vec<_> = (2..5)
            .map(|answer| {
                let (lookups, lookup) = (lookups.clone(), fresh.lookup(answer));
                tokio::spawn(async move { lookups.ask(LONG, lookup).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;
        earlier.open();
        assert_eq!(first.await.unwrap(), Asked::Answer(1));
        while fresh.began() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Every one of the three has found the fresh lookup by now.
        tokio::time::sleep(Duration::from_millis(50)).await;
        fresh.open();
        let mut answers = Vec::new();
        for waiting in waiting {
            answers.push(waiting.await.unwrap());
        }
        assert!(
            answers.iter().all(|answer| *answer == answers[0])
                && matches!(answers[0], Asked::Answer(2..=4)),
            "one fresh answer for all three: {answers:?}"
        );
        assert_eq!(
            (earlier.began(), fresh.began()),
            (1, 1),
            "the first lookup, and one fresh one"
        );
    }

    /// #3796's accepted cost, measured. A lookup that takes 500 ms, against
    /// an 800 ms wait: the caller it began for is answered, but a caller who
    /// asks 100 ms after it began waits 400 ms for it to end, and then 500 ms
    /// for its own. That is past its wait, where a lookup of its own would
    /// have answered it. With the lookup at most half the wait, even a caller
    /// who asks just after a lookup began is answered.
    #[tokio::test]
    async fn the_cost_a_caller_just_after_a_slow_lookup_began_waits_for_two() {
        let lookups = Arc::new(OneInFlight::new("test-lookup"));
        let wait = Duration::from_millis(800);
        let taking = |took: u64, answer: u32| {
            move || {
                std::thread::sleep(Duration::from_millis(took));
                answer
            }
        };
        let first = tokio::spawn({
            let lookups = lookups.clone();
            async move { lookups.ask(wait, taking(500, 7)).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let asked = Instant::now();
        let just_after = lookups.ask(wait, taking(500, 8)).await;
        assert_eq!(
            first.await.unwrap(),
            Asked::Answer(7),
            "500 ms inside 800 ms"
        );
        assert_eq!(
            just_after,
            Asked::OutOfTime,
            "400 ms, then 500 ms, is past 800 ms"
        );
        assert!(asked.elapsed() >= wait);
        // At most half the wait: 300 ms.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let first = tokio::spawn({
            let lookups = lookups.clone();
            async move { lookups.ask(wait, taking(300, 9)).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            lookups.ask(wait, taking(300, 10)).await,
            Asked::Answer(10),
            "290 ms, then 300 ms, fits in 800 ms"
        );
        assert_eq!(first.await.unwrap(), Asked::Answer(9));
    }

    /// A lookup that panics tells the callers it answered that it failed,
    /// and holds nothing: the next caller asks afresh and is answered.
    #[tokio::test]
    async fn a_lookup_that_panics_fails_and_holds_nothing() {
        let lookups = OneInFlight::new("test-lookup");
        let failed = lookups
            .ask(LONG, || -> u32 { panic!("a lookup that fails") })
            .await;
        assert!(
            matches!(&failed, Asked::Failed(why) if why.contains("a lookup that fails")),
            "{failed:?}"
        );
        assert_eq!(lookups.ask(LONG, || 9).await, Asked::Answer(9));
        assert_eq!(
            lookups.ask_blocking(LONG, || -> u32 { panic!("again") }),
            Asked::Failed("it panicked: again".into())
        );
        assert_eq!(lookups.ask_blocking(LONG, || 10), Asked::Answer(10));
    }

    /// Each lookup runs on a thread of its own, named for what it looks up,
    /// never on the caller's.
    #[test]
    fn a_lookup_runs_on_its_own_named_thread() {
        let lookups = OneInFlight::new("seat0-lookup");
        let here = std::thread::current().id();
        let Asked::Answer((on, name)) = lookups.ask_blocking(LONG, || {
            let thread = std::thread::current();
            (thread.id(), thread.name().map(str::to_string))
        }) else {
            panic!("the lookup answered");
        };
        assert_ne!(on, here);
        assert_eq!(name.as_deref(), Some("seat0-lookup"));
    }

    /// #3830 — a [`Remembered`] for a test: `'static`, as the daemon's are.
    fn remembered(window: Duration) -> &'static Remembered<u32> {
        Box::leak(Box::new(Remembered::new("test-remembered", window)))
    }

    /// Until `done` holds, `LONG` at most.
    fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + LONG;
        while !done() {
            assert!(Instant::now() < deadline, "{what}: not within {LONG:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// #3830 — `now` never waits. Before the first lookup ends it has no
    /// answer, and it starts no second lookup while that one runs. A lookup
    /// that nobody waits for still stores its answer, and inside the window
    /// that answer is read with no lookup at all.
    #[test]
    fn now_never_waits_and_an_answer_nobody_waited_for_still_lands() {
        let r = remembered(LONG);
        let (first, later) = (Gate::default(), Gate::default());
        let _opened = (OpenedAtTheEnd(first.clone()), OpenedAtTheEnd(later.clone()));
        assert_eq!(r.now(first.lookup(1)), None, "no answer yet");
        eventually("the lookup begins", || first.began() == 1);
        assert_eq!(r.now(first.lookup(2)), None, "none while it hangs");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(first.began(), 1, "one lookup, however many asked");
        first.open();
        eventually("the answer nobody waited for is stored", || {
            r.last_answer().map(|(_, answer)| answer) == Some(1)
        });
        assert_eq!(r.now(later.lookup(3)), Some(1), "inside its window");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(later.began(), 0, "no lookup inside the window");
    }

    /// #3830 — past its window the last answer still stands at once, and the
    /// refresh it starts hangs alone: one lookup in flight.
    #[test]
    fn past_its_window_the_last_answer_stands_while_its_refresh_hangs() {
        let r = remembered(Duration::ZERO);
        let held = Gate::default();
        let _opened = OpenedAtTheEnd(held.clone());
        assert_eq!(r.now(|| 7), None);
        eventually("the first answer lands and starts a refresh", || {
            r.now(held.lookup(8)) == Some(7) && held.began() == 1
        });
        assert_eq!(r.now(held.lookup(9)), Some(7), "the last answer stands");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(held.began(), 1, "one refresh, however many asked");
    }

    /// #3830 — `current` takes an answer from inside its window and starts
    /// nothing. Past it, it waits for a lookup that began after it asked,
    /// within its wait, while what is shown is still the last answer.
    #[tokio::test]
    async fn current_takes_its_window_and_past_it_waits_for_a_lookup() {
        let r = remembered(LONG);
        let held = Gate::default();
        let _opened = OpenedAtTheEnd(held.clone());
        assert_eq!(r.current(LONG, || 1).await, Asked::Answer(1));
        assert_eq!(r.current(SHORT, held.lookup(2)).await, Asked::Answer(1));
        assert_eq!(held.began(), 0, "no lookup inside the window");

        let r = remembered(Duration::ZERO);
        assert_eq!(r.current(LONG, || 3).await, Asked::Answer(3));
        assert_eq!(
            r.current(SHORT, held.lookup(4)).await,
            Asked::OutOfTime,
            "past its window it waits for a lookup, and this one hangs"
        );
        assert_eq!(r.now(|| 5), Some(3), "what is shown is the last answer");
    }

    /// #3830 — `fresh` never takes the stored answer, however new: with no
    /// lookup in time it has none, which its caller reads as not confirmed.
    /// That lookup's late answer is stored for whoever comes next, and the
    /// next fresh check still asks again.
    #[tokio::test]
    async fn fresh_never_takes_the_stored_answer() {
        let r = remembered(LONG);
        let held = Gate::default();
        let _opened = OpenedAtTheEnd(held.clone());
        assert_eq!(r.fresh(LONG, || 1).await, Asked::Answer(1));
        assert_eq!(
            r.fresh(SHORT, held.lookup(2)).await,
            Asked::OutOfTime,
            "never the answer before the question"
        );
        assert_eq!(r.now(|| 3), Some(1));
        held.open();
        eventually("the late answer is stored", || r.now(|| 3) == Some(2));
        assert_eq!(r.fresh(LONG, || 4).await, Asked::Answer(4));
        assert_eq!(r.now(|| 5), Some(4));
    }

    /// #3830 — a lookup that hangs holds no async worker. On a two-worker
    /// runtime, tasks that ask `now`, `current` and `fresh` again and again
    /// leave a 10 ms timer waking on time, and the hang holds one lookup.
    #[test]
    fn a_hung_lookup_holds_no_async_worker() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime");
        let held = Gate::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let r = remembered(Duration::ZERO);
        for _ in 0..3 {
            let held = held.clone();
            rt.spawn(async move {
                for n in 0..1_000 {
                    let _ = r.now(held.lookup(n));
                    let _ = r.current(SHORT, held.lookup(n)).await;
                    let _ = r.fresh(SHORT, held.lookup(n)).await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        }
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let tick = Duration::from_millis(10);
            let mut worst = Duration::ZERO;
            for _ in 0..50 {
                let asked = tokio::time::Instant::now();
                tokio::time::sleep(tick).await;
                worst = worst.max(asked.elapsed().saturating_sub(tick));
            }
            let _ = tx.send(worst);
        });
        let worst = rx.recv_timeout(LONG).expect("the timer kept waking");
        assert!(worst < Duration::from_millis(500), "it woke {worst:?} late");
        assert_eq!(held.began(), 1, "one lookup in flight, however many asked");
    }

    /// #3853 — a [`OneInFlightPerKey`] for a test: `'static`, as the
    /// daemon's is.
    fn per_key() -> &'static OneInFlightPerKey<&'static str, u32> {
        Box::leak(Box::new(OneInFlightPerKey::new("test-per-key")))
    }

    /// #3853 — while lookups hang, the asks for one key share its one
    /// lookup, in a row or at once, and another key gets a lookup of its
    /// own. Once they are let go, no entry is left behind, and the next ask
    /// starts a lookup of its own, whose end leaves none either.
    #[tokio::test]
    async fn asks_for_one_key_share_its_lookup_and_another_key_gets_its_own() {
        let lookups = per_key();
        let (ana, bob) = (Gate::default(), Gate::default());
        let _opened = (OpenedAtTheEnd(ana.clone()), OpenedAtTheEnd(bob.clone()));
        for _ in 0..3 {
            assert_eq!(
                lookups.ask("ana", SHORT, ana.lookup(1)).await,
                Asked::OutOfTime
            );
        }
        let (a, b, c) = tokio::join!(
            lookups.ask("ana", SHORT, ana.lookup(2)),
            lookups.ask("ana", SHORT, ana.lookup(3)),
            lookups.ask("bob", SHORT, bob.lookup(4)),
        );
        assert_eq!(
            [a, b, c],
            [Asked::OutOfTime, Asked::OutOfTime, Asked::OutOfTime]
        );
        assert_eq!(
            (ana.began(), bob.began()),
            (1, 1),
            "one lookup a key, however many asked"
        );
        assert_eq!(lookups.keys_in_flight(), 2, "an entry for each hung key");
        ana.open();
        bob.open();
        eventually("the released lookups leave no entry", || {
            lookups.keys_in_flight() == 0
        });
        assert_eq!(
            lookups.ask("ana", LONG, ana.lookup(5)).await,
            Asked::Answer(5)
        );
        assert_eq!(ana.began(), 2, "the next ask's own lookup");
        eventually("an answered lookup leaves no entry", || {
            lookups.keys_in_flight() == 0
        });
    }

    /// #3853 — a lookup for a key that began before an ask never answers it:
    /// the ask waits it out, starting nothing, and takes a fresh lookup's
    /// answer, while the caller it began for takes its own. Another key's
    /// lookup, hung meanwhile, neither holds the ask up nor answers it.
    #[tokio::test]
    async fn a_lookup_for_the_key_begun_before_the_ask_never_answers_it() {
        let lookups = per_key();
        let (earlier, fresh, other) = (Gate::default(), Gate::default(), Gate::default());
        let _opened = (
            OpenedAtTheEnd(earlier.clone()),
            OpenedAtTheEnd(fresh.clone()),
            OpenedAtTheEnd(other.clone()),
        );
        let first = tokio::spawn(lookups.ask("ana", LONG, earlier.lookup(1)));
        while earlier.began() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let hung_other = tokio::spawn(lookups.ask("bob", LONG, other.lookup(9)));
        let later = tokio::spawn(lookups.ask("ana", LONG, fresh.lookup(2)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            (earlier.began(), fresh.began(), other.began()),
            (1, 0, 1),
            "the later ask waits, starting nothing"
        );
        earlier.open();
        assert_eq!(first.await.unwrap(), Asked::Answer(1));
        while fresh.began() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        fresh.open();
        assert_eq!(
            later.await.unwrap(),
            Asked::Answer(2),
            "a fresh lookup's answer, never the one begun before the ask"
        );
        eventually("only the hung key's entry is left", || {
            lookups.keys_in_flight() == 1
        });
        other.open();
        assert_eq!(hung_other.await.unwrap(), Asked::Answer(9));
        eventually("the ended lookups leave no entry", || {
            lookups.keys_in_flight() == 0
        });
    }

    /// #3855 — asks for a key from blocking threads share its one lookup
    /// with an ask from a task, each within its own wait, and another key's
    /// lookup is its own. Once let go, the next blocking ask starts a lookup
    /// of its own and is answered.
    #[test]
    fn blocking_asks_for_one_key_share_its_lookup() {
        let lookups = per_key();
        let (ana, bob) = (Gate::default(), Gate::default());
        let _opened = (OpenedAtTheEnd(ana.clone()), OpenedAtTheEnd(bob.clone()));
        let asks: Vec<_> = (0..3)
            .map(|answer| {
                let lookup = ana.lookup(answer);
                std::thread::spawn(move || lookups.ask_blocking("ana", SHORT, lookup))
            })
            .collect();
        let other = {
            let lookup = bob.lookup(9);
            std::thread::spawn(move || lookups.ask_blocking("bob", SHORT, lookup))
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        assert_eq!(
            rt.block_on(lookups.ask("ana", SHORT, ana.lookup(4))),
            Asked::OutOfTime
        );
        for ask in asks {
            assert_eq!(ask.join().expect("an ask's thread"), Asked::OutOfTime);
        }
        assert_eq!(other.join().expect("an ask's thread"), Asked::OutOfTime);
        assert_eq!(
            (ana.began(), bob.began()),
            (1, 1),
            "one lookup a key, from threads and a task alike"
        );
        ana.open();
        bob.open();
        eventually("the released lookups leave no entry", || {
            lookups.keys_in_flight() == 0
        });
        assert_eq!(
            lookups.ask_blocking("ana", LONG, ana.lookup(5)),
            Asked::Answer(5)
        );
        assert_eq!(ana.began(), 2, "the next ask's own lookup");
    }

    /// #3853 — a lookup that panics ends as any other: its caller is told it
    /// failed, its key's entry goes, and the next ask is answered afresh.
    #[tokio::test]
    async fn a_lookup_that_panics_leaves_no_entry() {
        let lookups = per_key();
        let failed = lookups
            .ask("ana", LONG, || -> u32 { panic!("a lookup that fails") })
            .await;
        assert!(
            matches!(&failed, Asked::Failed(why) if why.contains("a lookup that fails")),
            "{failed:?}"
        );
        eventually("a lookup that panicked leaves no entry", || {
            lookups.keys_in_flight() == 0
        });
        assert_eq!(lookups.ask("ana", LONG, || 7).await, Asked::Answer(7));
    }
}
