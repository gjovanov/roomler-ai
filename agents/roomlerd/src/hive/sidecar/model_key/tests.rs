// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3362 — the device's key against a stand-in helper whose runs each test
//! counts, holds and lets go: no process, and no clock but the TTL's. The
//! runtime is the test's own thread, so every task a test spawned has run as
//! far as it can once the test has yielded a few times ([`settle`]).

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::*;

const WAIT: Duration = Duration::from_secs(5);
const FAILED: &str = "hive_api_key_helper failed (exit status: 1)";

/// What a stand-in run answers.
#[derive(Clone, Copy)]
enum Answer {
    Key(&'static str),
    Fails,
    Panics,
}

/// A stand-in for the device's helper: each run counted as it starts, held
/// until the test lets it go, then answering what [`Helper::answer`] last
/// set.
#[derive(Clone)]
struct Helper {
    started: Arc<AtomicUsize>,
    ended: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
    most: Arc<AtomicUsize>,
    go: Arc<Semaphore>,
    answer: Arc<Mutex<Answer>>,
}

/// A run in flight, counted out however it ends, cut short included.
struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Helper {
    fn answering(answer: Answer) -> Self {
        Self {
            started: Arc::default(),
            ended: Arc::default(),
            live: Arc::default(),
            most: Arc::default(),
            go: Arc::new(Semaphore::new(0)),
            answer: Arc::new(Mutex::new(answer)),
        }
    }

    /// What every run from now on answers.
    fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    /// One run, as `ModelKey::get` is handed it.
    fn run(&self) -> impl Future<Output = Fetched> + Send + 'static {
        let h = self.clone();
        async move {
            h.started.fetch_add(1, Ordering::SeqCst);
            let live = h.live.fetch_add(1, Ordering::SeqCst) + 1;
            h.most.fetch_max(live, Ordering::SeqCst);
            let _live = Live(Arc::clone(&h.live));
            h.go.acquire().await.expect("never closed").forget();
            h.ended.fetch_add(1, Ordering::SeqCst);
            let answer = *h.answer.lock().unwrap();
            match answer {
                Answer::Key(key) => Ok(key.to_string()),
                Answer::Fails => Err(FAILED.to_string()),
                Answer::Panics => panic!("the stand-in helper panics"),
            }
        }
    }

    /// Lets `n` runs end.
    fn release(&self, n: usize) {
        self.go.add_permits(n);
    }

    /// The runs started, the runs that reached their end, and the most ever
    /// in flight at once.
    fn runs(&self) -> (usize, usize, usize) {
        (
            self.started.load(Ordering::SeqCst),
            self.ended.load(Ordering::SeqCst),
            self.most.load(Ordering::SeqCst),
        )
    }
}

/// A call on a task of its own, as each of the sidecar's calls is.
fn call(key: &Arc<ModelKey>, h: &Helper) -> JoinHandle<Fetched> {
    let (key, h) = (Arc::clone(key), h.clone());
    tokio::spawn(async move { key.get(|| h.run()).await })
}

/// Every task the test spawned has run until it waits.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// A call's answer: in time, and an answer rather than a panic.
async fn answered(call: JoinHandle<Fetched>) -> Fetched {
    tokio::time::timeout(WAIT, call)
        .await
        .expect("answered in time")
        .expect("an answer, not a panic")
}

/// Five calls with no key kept share one run of a helper that fails: each
/// gets that failure, once, and the helper ran once. In master's shape each
/// call ran the helper in turn, under the lock.
#[tokio::test]
async fn five_calls_share_one_run_and_its_failure() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Fails);
    let calls: Vec<_> = (0..5).map(|_| call(&key, &h)).collect();
    settle().await;
    assert_eq!(h.runs(), (1, 0, 1), "one run for five calls");
    assert!(
        calls.iter().all(|c| !c.is_finished()),
        "none answered before its run ends"
    );
    // Enough for every run master's shape makes.
    h.release(5);
    for c in calls {
        assert_eq!(answered(c).await, Err(FAILED.to_string()));
    }
    assert_eq!(
        h.runs(),
        (1, 1, 1),
        "one run, to its end, and never two at once"
    );
}

/// Many calls at once on many threads, as the daemon's runtime has them: each
/// reaches the one run in flight while it is held, no second run starts
/// beside it, and that run answers them all. In master's shape the lock
/// ordered them instead, and the first to take it ran the helper and kept
/// its key for the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_calls_on_many_threads_share_one_run() {
    const CALLS: usize = 64;
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Key("sk-many"));
    let together = Arc::new(tokio::sync::Barrier::new(CALLS + 1));
    let calls: Vec<_> = (0..CALLS)
        .map(|_| {
            let (key, h, together) = (Arc::clone(&key), h.clone(), Arc::clone(&together));
            tokio::spawn(async move {
                together.wait().await;
                key.get(|| h.run()).await
            })
        })
        .collect();
    together.wait().await;
    // Every call reaches the run meanwhile, held, on four threads. One that
    // came later would find its key kept: no second run either way.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.runs(), (1, 0, 1), "one run for {CALLS} calls");
    // Enough for a run each, were each to start one.
    h.release(CALLS);
    for c in calls {
        assert_eq!(answered(c).await.as_deref(), Ok("sk-many"));
    }
    assert_eq!(h.runs(), (1, 1, 1), "one run, to its end");
}

/// No failure is kept: a call after a run has failed runs the helper again.
#[tokio::test]
async fn a_call_after_a_failed_run_runs_the_helper_again() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Fails);
    let first = call(&key, &h);
    settle().await;
    h.release(1);
    assert_eq!(answered(first).await, Err(FAILED.to_string()));
    h.answer(Answer::Key("sk-new"));
    let again = call(&key, &h);
    settle().await;
    assert_eq!(h.runs().0, 2, "a run of its own");
    h.release(1);
    assert_eq!(answered(again).await.as_deref(), Ok("sk-new"));
}

/// A call that gives up takes nothing from the calls waiting with it, the
/// call whose arrival started the run included: the others get that run's
/// key, and the helper ran once, to its end. In master's shape that call ran
/// the helper itself, so giving up ended the run, and the next call ran the
/// helper again.
#[tokio::test]
async fn a_call_that_gives_up_leaves_its_run_to_the_others() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Key("sk-shared"));
    let calls: Vec<_> = (0..3).map(|_| call(&key, &h)).collect();
    settle().await;
    calls[0].abort();
    settle().await;
    h.release(3);
    for c in calls.into_iter().skip(1) {
        assert_eq!(answered(c).await.as_deref(), Ok("sk-shared"));
    }
    assert_eq!(h.runs(), (1, 1, 1), "one run, to its end");
}

/// A run nobody waits on any more is not cut short: it ends as it would have
/// and keeps its key, and leaves nothing in flight, so a later call gets that
/// key at once. In master's shape giving up ended the run, and nothing was
/// kept.
#[tokio::test]
async fn a_run_nobody_waits_on_any_more_ends_and_keeps_its_key() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Key("sk-fetched"));
    let only = call(&key, &h);
    settle().await;
    only.abort();
    settle().await;
    h.release(1);
    settle().await;
    assert_eq!(h.runs(), (1, 1, 1), "the run went to its end");
    let again = tokio::time::timeout(WAIT, key.get(|| h.run()))
        .await
        .expect("at once");
    assert_eq!(again.as_deref(), Ok("sk-fetched"));
    assert_eq!(h.runs().0, 1, "from what the run kept");
}

/// A run that panics answers every call waiting on it, in words rather than
/// with a panic of their own, and leaves nothing in flight: a later call runs
/// the helper again.
#[tokio::test]
async fn a_run_that_panics_answers_its_calls_and_leaves_nothing_in_flight() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Panics);
    let calls: Vec<_> = (0..3).map(|_| call(&key, &h)).collect();
    settle().await;
    h.release(1);
    for c in calls {
        let answer = answered(c).await;
        assert!(
            answer
                .as_ref()
                .is_err_and(|e| e.contains("without an answer")),
            "{answer:?}"
        );
    }
    h.answer(Answer::Key("sk-after"));
    let later = call(&key, &h);
    settle().await;
    h.release(1);
    assert_eq!(answered(later).await.as_deref(), Ok("sk-after"));
    assert_eq!(h.runs().0, 2, "a run of its own");
}

/// The provider refuses the key while a run is in flight: the forget is done
/// at once, a call after it waits on that run rather than start a second one,
/// and the run keeps nothing, since it began before the refusal, so the next
/// call runs the helper again. In master's shape the forget waited on the
/// lock, behind the run.
#[tokio::test]
async fn a_forget_while_a_run_is_in_flight() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Key("sk-before"));
    let first = call(&key, &h);
    settle().await;
    tokio::time::timeout(Duration::from_secs(1), key.forget())
        .await
        .expect("a forget does not wait on the run in flight");
    let second = call(&key, &h);
    settle().await;
    assert_eq!(
        h.runs(),
        (1, 0, 1),
        "a call after the forget waits on the run in flight"
    );
    h.release(1);
    for c in [first, second] {
        assert_eq!(answered(c).await.as_deref(), Ok("sk-before"));
    }
    h.answer(Answer::Key("sk-after"));
    let next = call(&key, &h);
    settle().await;
    assert_eq!(
        h.runs().0,
        2,
        "the run began before the refusal: it kept nothing"
    );
    h.release(1);
    assert_eq!(answered(next).await.as_deref(), Ok("sk-after"));
}

/// A key kept and younger than its TTL answers every call at once: no run,
/// and nothing waited on.
#[tokio::test]
async fn a_kept_key_answers_at_once_within_its_ttl() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Key("sk-kept"));
    let first = call(&key, &h);
    settle().await;
    h.release(1);
    assert_eq!(answered(first).await.as_deref(), Ok("sk-kept"));
    // The helper is held from here: a call that ran it would wait.
    for _ in 0..5 {
        let kept = tokio::time::timeout(Duration::from_millis(500), key.get(|| h.run()))
            .await
            .expect("at once");
        assert_eq!(kept.as_deref(), Ok("sk-kept"));
    }
    assert_eq!(h.runs(), (1, 1, 1));
}

/// Past its TTL the key is fetched again.
#[tokio::test]
async fn past_its_ttl_the_key_is_fetched_again() {
    let key = Arc::new(ModelKey::with_ttl(Duration::from_millis(50)));
    let h = Helper::answering(Answer::Key("sk-old"));
    let first = call(&key, &h);
    settle().await;
    h.release(1);
    assert_eq!(answered(first).await.as_deref(), Ok("sk-old"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.answer(Answer::Key("sk-new"));
    let later = call(&key, &h);
    settle().await;
    assert_eq!(h.runs().0, 2, "a run of its own");
    h.release(1);
    assert_eq!(answered(later).await.as_deref(), Ok("sk-new"));
}

/// A forget takes the kept key: the next call runs the helper again.
#[tokio::test]
async fn a_forget_takes_the_kept_key() {
    let key = Arc::new(ModelKey::default());
    let h = Helper::answering(Answer::Key("sk-old"));
    let first = call(&key, &h);
    settle().await;
    h.release(1);
    assert_eq!(answered(first).await.as_deref(), Ok("sk-old"));
    tokio::time::timeout(WAIT, key.forget())
        .await
        .expect("forgotten");
    h.answer(Answer::Key("sk-new"));
    let next = call(&key, &h);
    settle().await;
    assert_eq!(h.runs().0, 2, "a run of its own");
    h.release(1);
    assert_eq!(answered(next).await.as_deref(), Ok("sk-new"));
}
