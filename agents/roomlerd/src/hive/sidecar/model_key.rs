// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3362 — the provider's key, from the device's helper: kept for
//! [`KEY_TTL`], and fetched by one run of the helper at a time, whose result
//! every call waiting on it gets.
//!
//! Until #3362 one async lock held the key, and a call held it across the
//! helper's run. A helper that failed kept nothing, so calls that came
//! together ran it again one after another, and the last of N waited N runs,
//! each up to [`HELPER_TIMEOUT`].
//!
//! | Piece | As built |
//! |---|---|
//! | a call | the key kept, while it is younger than [`KEY_TTL`]; otherwise the run in flight, waited on; otherwise a run of its own, started on a task of its own and waited on |
//! | a run | on a task of its own, and over within [`HELPER_TIMEOUT`], the bound [`run_key_helper`] keeps. Its result, a key or a failure, answers every call waiting on it, each once. Only a key is kept: a call after a failure runs the helper again, and never while another run is in flight |
//! | the lock | a plain mutex, held to read and change what is kept and never across an `.await`: the helper runs with nothing held, so the lock never orders the calls |
//! | a call that gives up | takes nothing with it. The run is no call's own: it goes on for the others, and to its end even when nobody waits on it any more, keeping the key it fetched |
//! | a run's end | however it comes, a panic included ([`RunEnd`]): its key kept, the run out of flight, and every call waiting on it answered, so nothing is left in flight to hold a later call |
//! | a forget | the provider refused the key: the kept one goes at once. A run in flight keeps nothing when it ends, since it began before the refusal, and still answers the calls waiting on it, so none of them waits behind more than one run |
//!
//! [`KEY_TTL`]: super::KEY_TTL
//! [`HELPER_TIMEOUT`]: super::HELPER_TIMEOUT
//! [`run_key_helper`]: super::run_key_helper

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// What one run of the helper came to: the key it printed, or why not.
pub(crate) type Fetched = Result<String, String>;

/// A run that ended with no result: it panicked, or its task went with the
/// runtime.
const NO_ANSWER: &str = "hive_api_key_helper's run ended without an answer";

/// The device's provider key, as the helper last printed it.
pub(crate) struct ModelKey {
    kept: Arc<Mutex<Kept>>,
    ttl: Duration,
}

#[derive(Default)]
struct Kept {
    /// The key, and when the run that fetched it ended.
    key: Option<(String, Instant)>,
    /// The run in flight: a call that finds one waits on it.
    running: Option<watch::Receiver<Option<Fetched>>>,
    /// How many forgets there have been. A run keeps its key only if none
    /// came while it ran.
    forgets: u64,
}

impl Default for ModelKey {
    fn default() -> Self {
        Self::with_ttl(super::KEY_TTL)
    }
}

impl ModelKey {
    /// A key kept for `ttl`: [`KEY_TTL`](super::KEY_TTL), or shorter in a
    /// test.
    pub(crate) fn with_ttl(ttl: Duration) -> Self {
        Self {
            kept: Arc::default(),
            ttl,
        }
    }

    /// The key: the one kept, while it is younger than the TTL; otherwise
    /// what the run in flight fetches; otherwise what `run()` fetches, run on
    /// a task of its own, so that no call owns it. `run()` must end on its
    /// own: every call waiting on it waits until it does.
    pub(crate) async fn get<F, Fut>(&self, run: F) -> Fetched
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Fetched> + Send + 'static,
    {
        let (mut fetched, start) = {
            let mut kept = lock(&self.kept);
            if let Some((key, at)) = &kept.key
                && at.elapsed() < self.ttl
            {
                return Ok(key.clone());
            }
            match &kept.running {
                Some(running) => (running.clone(), None),
                None => {
                    let (tx, rx) = watch::channel(None);
                    kept.running = Some(rx.clone());
                    let end = RunEnd {
                        kept: Arc::clone(&self.kept),
                        forgets: kept.forgets,
                        tx,
                        fetched: None,
                    };
                    (rx, Some(end))
                }
            }
        };
        // Started with nothing held. A panic before the spawn drops `end`,
        // which takes the run out of flight as any end does.
        if let Some(mut end) = start {
            let run = run();
            tokio::spawn(async move {
                end.came_to(run.await);
            });
        }
        let answer = fetched
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|f| (*f).clone());
        answer.unwrap_or_else(|| Err(NO_ANSWER.into()))
    }

    /// The provider refused the key: the kept one goes, and a run in flight
    /// keeps nothing when it ends.
    pub(crate) async fn forget(&self) {
        let mut kept = lock(&self.kept);
        kept.key = None;
        kept.forgets += 1;
    }
}

/// One run's end, however it comes: its key kept, where no forget came while
/// it ran; the run out of flight; and every call waiting on it answered. Out
/// of flight before it answers, so a call that comes after the end never
/// joins this run: it finds the key kept, or runs the helper again.
struct RunEnd {
    kept: Arc<Mutex<Kept>>,
    forgets: u64,
    tx: watch::Sender<Option<Fetched>>,
    fetched: Option<Fetched>,
}

impl RunEnd {
    /// What the run came to: the end keeps it, and answers with it. Through
    /// `&mut self`, since the compiler's liveness check reads a field set
    /// directly before a drop as never read: it does not see `drop`.
    fn came_to(&mut self, fetched: Fetched) {
        self.fetched = Some(fetched);
    }
}

impl Drop for RunEnd {
    fn drop(&mut self) {
        let fetched = self.fetched.take().unwrap_or_else(|| Err(NO_ANSWER.into()));
        {
            let mut kept = lock(&self.kept);
            if let Ok(key) = &fetched
                && kept.forgets == self.forgets
            {
                kept.key = Some((key.clone(), Instant::now()));
            }
            kept.running = None;
        }
        self.tx.send_replace(Some(fetched));
    }
}

fn lock(kept: &Mutex<Kept>) -> MutexGuard<'_, Kept> {
    kept.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
