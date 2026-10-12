// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-93 P2d — one portal request per host, handed to whichever session asks
//! for it last.
//!
//! An attended portal open shows the person at the screen a dialog and waits
//! for them, for up to two minutes. Every session used to open its own. The
//! viewer's media watchdog re-creates a session that has shown no frames for
//! 10 s, so an unanswered dialog was joined by another every 10–20 s (four
//! stacked within a minute on the FR-93 lab's KDE guest), and answering a stale
//! one started a capture for a session that was already gone.
//!
//! Now a host has at most one request in flight:
//! - A session that asks while one is pending waits for THAT request instead of
//!   raising another dialog, and an older waiter leaves at once: it is a
//!   session the viewer has already replaced.
//! - When the person answers, the outcome goes to the **newest** waiter, the
//!   session the viewer is showing now.
//! - A late answer for a dead session starts nothing. A waiter is a blocking
//!   thread, not a session: the session's end aborts its pump, which detaches
//!   the open (`capture::open_default_async`), so the capture its waiter takes
//!   is dropped as it arrives and the screencast stops at once. An outcome that
//!   outlives its last waiter (each waits 5 s past the request's own deadline)
//!   is dropped in `Request::land`.
//!
//! The broker is generic over what a request produces, so its handoff unit-tests
//! with no portal at all.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::backend::{HANDSHAKE_DEADLINE, PortalCapture, PortalOpen};

/// A waiter outlasts the request's own deadline a little, so it hears the
/// request's reason ("a consent dialog left unanswered") rather than its own
/// timeout.
const WAIT: Duration = Duration::from_secs(HANDSHAKE_DEADLINE.as_secs() + 5);

/// The portal arm's open. An attended request (a dialog for the person at the
/// screen) goes through the host's one-request broker. mutter
/// (`mutter_capture`) asks nobody and opens in milliseconds, so it opens
/// directly, and concurrent sessions never wait on each other for it.
pub fn open(target_fps: u32) -> anyhow::Result<PortalCapture> {
    let opts = PortalOpen::from_env();
    if opts.mutter {
        return PortalCapture::open_with(target_fps, opts);
    }
    static BROKER: Broker<PortalCapture> = Broker::new();
    BROKER
        .open(WAIT, move || {
            PortalCapture::open_with(target_fps, opts).map_err(|e| format!("{e:#}"))
        })
        .map_err(|miss| anyhow::anyhow!("{miss}"))
}

/// Why a waiter got no capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Miss {
    /// A newer session asked for the same request: this one was replaced.
    Superseded,
    /// The request was still waiting for the person when this waiter's own
    /// deadline passed.
    TimedOut,
    /// The request failed (the dialog was refused, or never answered within its
    /// own deadline, or the helper could not start).
    Failed(String),
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Miss::Superseded => f.write_str(
                "a newer session is waiting for the same portal request (this one was replaced)",
            ),
            Miss::TimedOut => {
                f.write_str("the portal request is still waiting for the person at the screen")
            }
            Miss::Failed(e) => f.write_str(e),
        }
    }
}

/// One request in flight, and who waits for it.
struct Request<T> {
    state: Mutex<State<T>>,
    landed: Condvar,
}

struct State<T> {
    /// The newest waiter's ticket. The outcome goes to it and to no one else.
    newest: u64,
    /// Waiters still waiting.
    waiting: usize,
    /// The outcome, from the moment it lands until the newest waiter takes it.
    outcome: Option<Result<T, String>>,
    /// The request is over: its outcome was taken, or dropped with nobody there.
    done: bool,
}

/// The host's one portal request.
pub(crate) struct Broker<T> {
    slot: Mutex<Option<Arc<Request<T>>>>,
}

impl<T: Send + 'static> Broker<T> {
    pub(crate) const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    /// Wait up to `deadline` for a request's outcome. `start` runs on a thread
    /// of its own, and only when no request is in flight. It returns when the
    /// person answers or the request fails.
    pub(crate) fn open(
        &self,
        deadline: Duration,
        start: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, Miss> {
        let (request, ticket) = {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            let live = slot
                .as_ref()
                .filter(|r| !r.state.lock().unwrap_or_else(|e| e.into_inner()).done)
                .cloned();
            match live {
                Some(r) => {
                    let ticket = r.join();
                    (r, ticket)
                }
                None => {
                    let r = Arc::new(Request {
                        state: Mutex::new(State {
                            newest: 0,
                            waiting: 0,
                            outcome: None,
                            done: false,
                        }),
                        landed: Condvar::new(),
                    });
                    // Join BEFORE the request starts: an answer that lands at
                    // once must find its waiter, not be dropped as unwanted.
                    let ticket = r.join();
                    *slot = Some(r.clone());
                    let worker = r.clone();
                    let spawned = std::thread::Builder::new()
                        .name("portal-request".into())
                        .spawn(move || worker.land(start()));
                    if let Err(e) = spawned {
                        // No thread, no request: say so rather than wait for nothing.
                        *slot = None;
                        return Err(Miss::Failed(format!(
                            "no thread for the portal request: {e}"
                        )));
                    }
                    (r, ticket)
                }
            }
        };
        request.wait(ticket, deadline)
    }
}

impl<T> Request<T> {
    /// A new waiter: it becomes the newest, so every older waiter leaves.
    fn join(&self) -> u64 {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.newest += 1;
        st.waiting += 1;
        let ticket = st.newest;
        drop(st);
        // Wake the older waiters so they see they were replaced.
        self.landed.notify_all();
        ticket
    }

    /// The request's outcome. With nobody waiting it is dropped here, which for
    /// a capture means its helper is killed and the screencast stops. This is
    /// the backstop for a request that outlives its last waiter: a waiter whose
    /// SESSION ended is still waiting here, and its capture is dropped by the
    /// detached open instead (see the module doc).
    fn land(&self, outcome: Result<T, String>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.waiting == 0 {
            st.done = true;
            drop(st);
            drop(outcome);
            return;
        }
        st.outcome = Some(outcome);
        drop(st);
        self.landed.notify_all();
    }

    fn wait(&self, ticket: u64, deadline: Duration) -> Result<T, Miss> {
        let until = Instant::now() + deadline;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if st.newest != ticket {
                st.waiting -= 1;
                return Err(Miss::Superseded);
            }
            if let Some(outcome) = st.outcome.take() {
                st.waiting -= 1;
                st.done = true;
                return outcome.map_err(Miss::Failed);
            }
            if st.done {
                // Over without an outcome for this waiter.
                st.waiting -= 1;
                return Err(Miss::Failed("the portal request ended".into()));
            }
            let now = Instant::now();
            if now >= until {
                st.waiting -= 1;
                return Err(Miss::TimedOut);
            }
            st = self
                .landed
                .wait_timeout(st, until - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::mpsc;

    /// A request the test answers when it chooses.
    fn gated() -> (
        mpsc::Sender<Result<u32, String>>,
        impl FnOnce() -> Result<u32, String> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        (tx, move || rx.recv().unwrap_or(Err("gone".into())))
    }

    #[test]
    fn one_session_gets_the_answer() {
        let b = Broker::<u32>::new();
        let (answer, start) = gated();
        answer.send(Ok(7)).unwrap();
        assert_eq!(b.open(Duration::from_secs(5), start), Ok(7));
    }

    #[test]
    fn a_second_session_adopts_the_request_and_the_first_is_superseded() {
        let b = Arc::new(Broker::<u32>::new());
        let started = Arc::new(AtomicUsize::new(0));
        let (answer, start) = gated();
        let counted = {
            let started = started.clone();
            move || {
                started.fetch_add(1, SeqCst);
                start()
            }
        };
        let first = {
            let b = b.clone();
            std::thread::spawn(move || b.open(Duration::from_secs(10), counted))
        };
        // Let the first waiter join before the second asks.
        std::thread::sleep(Duration::from_millis(100));
        let second = {
            let b = b.clone();
            std::thread::spawn(move || {
                b.open(Duration::from_secs(10), || -> Result<u32, String> {
                    panic!("a second request must never start while one is in flight")
                })
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(first.join().unwrap(), Err(Miss::Superseded));
        answer.send(Ok(42)).unwrap();
        assert_eq!(
            second.join().unwrap(),
            Ok(42),
            "the newest waiter gets the answer"
        );
        assert_eq!(started.load(SeqCst), 1, "one dialog, not two");
    }

    /// An answer that lands with nobody waiting is dropped, and the next session
    /// starts a fresh request.
    #[test]
    fn a_late_answer_for_a_dead_session_is_dropped() {
        struct Capture(Arc<AtomicUsize>);
        impl Drop for Capture {
            fn drop(&mut self) {
                self.0.fetch_add(1, SeqCst);
            }
        }
        let b = Broker::<Capture>::new();
        let dropped = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel::<()>();
        let d = dropped.clone();
        let start = move || {
            rx.recv().ok();
            Ok(Capture(d))
        };
        // The only waiter gives up before the person answers.
        assert!(matches!(
            b.open(Duration::from_millis(50), start),
            Err(Miss::TimedOut)
        ));
        tx.send(()).unwrap();
        // The answer lands with nobody waiting: dropped, not handed out.
        let deadline = Instant::now() + Duration::from_secs(5);
        while dropped.load(SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(dropped.load(SeqCst), 1, "the late capture was dropped");
        // And the next session raises a new request rather than waiting on the old.
        let fresh = b.open(Duration::from_secs(5), || {
            Ok(Capture(Arc::new(AtomicUsize::new(0))))
        });
        assert!(fresh.is_ok());
    }

    #[test]
    fn a_failed_request_reaches_the_waiter_with_its_reason() {
        let b = Broker::<u32>::new();
        assert_eq!(
            b.open(Duration::from_secs(5), || Err(
                "the dialog was refused".into()
            )),
            Err(Miss::Failed("the dialog was refused".into()))
        );
        // Over: the next open starts again.
        assert_eq!(b.open(Duration::from_secs(5), || Ok(1)), Ok(1));
    }
}
