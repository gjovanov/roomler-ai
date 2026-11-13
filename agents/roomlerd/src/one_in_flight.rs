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
}
