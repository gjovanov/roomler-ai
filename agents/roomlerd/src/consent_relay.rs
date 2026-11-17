// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3864 — a one-row Mac's consent questions reach the menu-bar app.
//!
//! On a one-row Mac (FR-43 P3) the root daemon holds the device's row, so it
//! asks every remote-control, exec and SSH question, on its own consent
//! broker, whose folder is in root's own home. The menu-bar app runs as the
//! person at the console and asks over the LocalAPI, and the socket it reaches
//! first is its own account's (`<TMPDIR>/roomler/roomler.sock`), which the
//! root's GUI worker serves: a process of that person's, with a broker of its
//! own. Until #3864 the worker listed only its own questions, so a question the
//! root's native panel could not show (it shows one at a time) reached nobody,
//! while the root counted the app as the question's surface and the viewer
//! waited out the window.
//!
//! | Half | What it does |
//! |---|---|
//! | the worker (`run --supervised --unenrolled`) | lists its own questions, then the root half's, asked over the root's person socket ([`listed`]); sends an answer to a question its own broker is not asking there ([`decided`]); says [`RELAY_CONSENT`] in its caps frame |
//! | the root daemon | counts the app as a question's surface only while that road is open ([`reach`]): an attached worker that said [`RELAY_CONSENT`], and the person socket of that worker's account answering within [`RELAY_WITHIN`] |
//!
//! Nothing is written anywhere: the question crosses the root's person socket
//! (`/var/run/roomler-person/<uid>.sock`), which serves only the person at the
//! console and root (`localapi::person_peer_ok`) and asks the seat again
//! before it takes an answer, and the worker's own socket is that person's
//! alone. A road that is closed leaves the floor every host has: the app is
//! not the question's surface, the question stands its window for `roomlerd
//! consent`, and it ends as `no_prompt_surface`. A two-row Mac is unchanged:
//! its per-user half holds an enrollment, and its root daemon is not one
//! row's.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use tunnel_core::localapi::{ConsentRequest, Request, Response};

/// The word a one-row Mac's worker says in its caps frame
/// (`delegate::worker_features`) when it lists and decides the root half's
/// questions. Matched by equality, as `keep-delegated` is.
pub const RELAY_CONSENT: &str = "relay-consent";

/// How long the worker's list waits for the root half, and how long the root
/// daemon's look at the road waits for its person socket: the same bound, so
/// a socket too slow for the app's list is no road. The app asks every
/// 750 ms, and a healthy answer is one local socket's round trip.
pub const RELAY_WITHIN: Duration = Duration::from_secs(1);

/// How long an answer the person gave waits for the root half to take it. It
/// asks the seat first (on a Mac, one `stat` of `/dev/console`).
pub const DECIDE_WITHIN: Duration = Duration::from_secs(5);

/// The worker's refusal when the root half did not take an answer in time,
/// or gave none: the person is told to try again, never that the question
/// was unknown.
pub const NO_ANSWER_FROM_THE_ROOT: &str = "The answer was not taken: this Mac's root daemon, \
                                           which asked the question, did not answer in time. \
                                           Try again.";

/// Where a one-row Mac's worker asks the root half: a request, and its
/// answer, or none.
#[async_trait::async_trait]
pub trait RootHalf: Send + Sync {
    async fn ask(&self, req: &Request) -> Option<Response>;
}

/// The root half as the daemon asks it: the root daemon's person socket for
/// this process's account, kept only when root serves it
/// (`person_socket::machine_answer`). There is none off macOS.
pub struct PersonSocket;

#[async_trait::async_trait]
impl RootHalf for PersonSocket {
    async fn ask(&self, req: &Request) -> Option<Response> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: geteuid never fails.
            let euid = unsafe { libc::geteuid() };
            // Each caller bounds its own ask ([`listed`], [`decided`]).
            crate::person_socket::machine_answer(euid, req, DECIDE_WITHIN).await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = req;
            None
        }
    }
}

/// The questions the person's app lists on a one-row Mac: the worker's own
/// first, then each of the root half's whose id is not there already. Ids are
/// ObjectIds, a server's session or request id or one the asking daemon
/// minted, so two brokers never hold the same one. Anything but a list from
/// the root half (no answer, a refusal) leaves the worker's own.
pub fn merged(mut own: Vec<ConsentRequest>, root: Option<Response>) -> Vec<ConsentRequest> {
    if let Some(Response::ConsentPending(theirs)) = root {
        for question in theirs {
            if !own.iter().any(|q| q.session_id == question.session_id) {
                own.push(question);
            }
        }
    }
    own
}

/// [`merged`], with the root half asked for [`RELAY_WITHIN`] at most: a root
/// half that is slow or gone leaves the worker's own list, in time.
pub async fn listed(own: Vec<ConsentRequest>, root: &dyn RootHalf) -> Vec<ConsentRequest> {
    let theirs = tokio::time::timeout(RELAY_WITHIN, root.ask(&Request::ConsentPending))
        .await
        .ok()
        .flatten();
    merged(own, theirs)
}

/// The answer to a question the worker's own broker is not asking: the root
/// half's, asked for [`DECIDE_WITHIN`] at most. Its refusal (the seat changed,
/// the question is over) passes through in its own words. No answer is
/// refused in the worker's ([`NO_ANSWER_FROM_THE_ROOT`]).
pub async fn decided(root: &dyn RootHalf, session_id: &str, allow: bool) -> Response {
    let req = Request::ConsentDecide {
        session_id: session_id.to_string(),
        allow,
    };
    match tokio::time::timeout(DECIDE_WITHIN, root.ask(&req))
        .await
        .ok()
        .flatten()
    {
        Some(answer @ (Response::ConsentDecided { .. } | Response::Error { .. })) => answer,
        other => {
            tracing::warn!(
                session = %session_id,
                answered = other.is_some(),
                "consent: the root half did not take the answer to its question (#3864)"
            );
            Response::Error {
                message: NO_ANSWER_FROM_THE_ROOT.into(),
            }
        }
    }
}

/// What the root daemon knows of its worker: whether it said
/// [`RELAY_CONSENT`] on the channel it holds now, and the account it was
/// started for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Worker {
    pub relays: bool,
    pub uid: Option<u32>,
}

/// Whether a question this daemon asks reaches the menu-bar app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Not a one-row Mac's root daemon: the app asks this daemon itself, over
    /// the socket this daemon serves it.
    Direct,
    /// A one-row Mac's: the worker relays for this account, and the person
    /// socket for it answers.
    Relayed(u32),
    /// A one-row Mac's whose road is closed, and why.
    Closed(String),
}

impl Reach {
    /// Whether the question gets through.
    pub fn open(&self) -> bool {
        !matches!(self, Self::Closed(_))
    }
}

/// The road a question takes, from what the root daemon knows: `worker` is
/// `None` on any daemon but a one-row Mac's root. `answers` asks the person
/// socket of an account, and is given [`RELAY_WITHIN`] at most.
pub async fn reach_from<F, Fut>(worker: Option<Worker>, answers: F) -> Reach
where
    F: FnOnce(u32) -> Fut,
    Fut: Future<Output = bool>,
{
    let Some(worker) = worker else {
        return Reach::Direct;
    };
    if !worker.relays {
        return Reach::Closed(format!(
            "no worker is attached that relays consent questions (`{RELAY_CONSENT}`)"
        ));
    }
    let Some(uid) = worker.uid else {
        return Reach::Closed("the worker's account is not known".into());
    };
    match tokio::time::timeout(RELAY_WITHIN, answers(uid)).await {
        Ok(true) => Reach::Relayed(uid),
        Ok(false) => Reach::Closed(format!("the person socket for uid {uid} does not answer")),
        Err(_) => Reach::Closed(format!(
            "the person socket for uid {uid} gave no answer within {RELAY_WITHIN:?}"
        )),
    }
}

/// A one-row Mac's delegation host, registered by
/// `DelegateHost::set_one_row`: this process is that Mac's root daemon.
static ONE_ROW_HOST: OnceLock<crate::delegate::DelegateHost> = OnceLock::new();

/// Register `host`: this process is a one-row Mac's root daemon. Once.
pub fn register(host: &crate::delegate::DelegateHost) {
    let _ = ONE_ROW_HOST.set(host.clone());
}

/// [`reach_from`], for this daemon.
pub async fn reach() -> Reach {
    let worker = ONE_ROW_HOST.get().map(|host| Worker {
        relays: host.relays_consent(),
        uid: host.worker_uid(),
    });
    reach_from(worker, person_socket_answers).await
}

/// Whether this daemon's person socket for `uid` answers a list, asked the
/// way the worker asks it: served by root, a `ConsentPending` and its list.
async fn person_socket_answers(uid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        matches!(
            crate::person_socket::machine_answer(uid, &Request::ConsentPending, RELAY_WITHIN).await,
            Some(Response::ConsentPending(_))
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = uid;
        false
    }
}

/// A one-row Mac's road, stood in for a test of the root daemon's verdict
/// (`companion::ensure_running`, under its tray stand-in).
#[cfg(test)]
pub(crate) mod stand_in {
    use super::{Reach, Worker, reach_from};

    /// How the stood-in person socket answers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Answers {
        Yes,
        No,
        Never,
    }

    /// The worker as the root daemon knows it, and its person socket.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Road {
        pub(crate) worker: Worker,
        pub(crate) answers: Answers,
    }

    impl Road {
        pub(crate) async fn reach(self) -> Reach {
            reach_from(Some(self.worker), |_| async move {
                match self.answers {
                    Answers::Yes => true,
                    Answers::No => false,
                    Answers::Never => std::future::pending().await,
                }
            })
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::{Answers, Road};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    fn question(id: &str) -> ConsentRequest {
        ConsentRequest {
            session_id: id.into(),
            controller_name: "Alice".into(),
            permissions: "VIEW".into(),
            timeout_secs: 30,
            kind: "rc".into(),
            detail: String::new(),
            expires_at_ms: 0,
            surface: "companion".into(),
            org: String::new(),
        }
    }

    fn ids(list: &[ConsentRequest]) -> Vec<&str> {
        list.iter().map(|q| q.session_id.as_str()).collect()
    }

    /// Whether a wait on the paused clock took its bound, no more: the timer
    /// wheel rounds a deadline up to its next millisecond.
    fn waited_its_bound(waited: Duration, bound: Duration) -> bool {
        waited >= bound && waited < bound + Duration::from_millis(50)
    }

    /// The root half that never answers: a stalled or wedged root daemon.
    struct Silent;
    #[async_trait::async_trait]
    impl RootHalf for Silent {
        async fn ask(&self, _req: &Request) -> Option<Response> {
            std::future::pending().await
        }
    }

    /// The root half that answers `0`, counting its asks.
    struct Answering(Response, AtomicUsize);
    #[async_trait::async_trait]
    impl RootHalf for Answering {
        async fn ask(&self, _req: &Request) -> Option<Response> {
            self.1.fetch_add(1, SeqCst);
            Some(self.0.clone())
        }
    }

    /// The worker's own questions come first, then the root half's, each id
    /// once. No list from the root half (none, a refusal) leaves the own.
    #[test]
    fn the_list_is_the_workers_own_then_the_root_halfs_each_once() {
        let own = vec![question("a1"), question("b2")];
        let theirs = Response::ConsentPending(vec![question("c3"), question("a1")]);
        assert_eq!(ids(&merged(own.clone(), Some(theirs))), ["a1", "b2", "c3"]);
        assert_eq!(ids(&merged(own.clone(), None)), ["a1", "b2"]);
        let refused = Response::Error {
            message: "not on the person socket".into(),
        };
        assert_eq!(ids(&merged(own, Some(refused))), ["a1", "b2"]);
    }

    /// #3864, the owner's question 5 — the relay is bounded: a root half that
    /// never answers leaves the worker's own list within [`RELAY_WITHIN`], and
    /// an answer the person gave is refused, in words that say to try again,
    /// within [`DECIDE_WITHIN`].
    #[tokio::test(start_paused = true)]
    async fn a_silent_root_half_leaves_the_own_list_and_refuses_an_answer_in_time() {
        let asked = tokio::time::Instant::now();
        let list = listed(vec![question("a1")], &Silent).await;
        assert_eq!(ids(&list), ["a1"]);
        let waited = asked.elapsed();
        assert!(
            waited_its_bound(waited, RELAY_WITHIN),
            "the list waited {waited:?}, its bound is {RELAY_WITHIN:?}"
        );

        let asked = tokio::time::Instant::now();
        let answer = decided(&Silent, "c3", true).await;
        assert_eq!(
            answer,
            Response::Error {
                message: NO_ANSWER_FROM_THE_ROOT.into()
            }
        );
        let waited = asked.elapsed();
        assert!(
            waited_its_bound(waited, DECIDE_WITHIN),
            "the answer waited {waited:?}, its bound is {DECIDE_WITHIN:?}"
        );
    }

    /// The root half's refusal passes through in its own words, and its
    /// verdict as it is. Anything else is no answer.
    #[tokio::test]
    async fn the_root_halfs_verdict_and_refusal_pass_through() {
        let refusal = Response::Error {
            message: "The answer was not taken: only the person signed in at this device's own \
                      screen answers"
                .into(),
        };
        for verdict in [
            Response::ConsentDecided { ok: true },
            Response::ConsentDecided { ok: false },
            refusal,
        ] {
            let root = Answering(verdict.clone(), AtomicUsize::new(0));
            assert_eq!(decided(&root, "c3", false).await, verdict);
            assert_eq!(root.1.load(SeqCst), 1);
        }
        let odd = Answering(Response::ConsentPending(Vec::new()), AtomicUsize::new(0));
        assert_eq!(
            decided(&odd, "c3", true).await,
            Response::Error {
                message: NO_ANSWER_FROM_THE_ROOT.into()
            }
        );
    }

    /// The root daemon's verdict on the road. Off a one-row Mac's root there
    /// is nothing to look at, and nothing is asked. On one row, the road is
    /// open only for a worker that said [`RELAY_CONSENT`], for a known account,
    /// whose person socket answers in time; the socket is not asked for a
    /// worker that did not say the word (main's ask: such a worker is no
    /// road, whatever its socket does).
    #[tokio::test(start_paused = true)]
    async fn the_road_is_open_only_for_a_relaying_worker_whose_person_socket_answers() {
        let asked = AtomicUsize::new(0);
        let answers = |yes: bool| {
            let asked = &asked;
            move |_uid: u32| async move {
                asked.fetch_add(1, SeqCst);
                yes
            }
        };
        assert_eq!(reach_from(None, answers(false)).await, Reach::Direct);
        assert_eq!(asked.load(SeqCst), 0, "off one row, nothing is asked");

        let silent_word = Worker {
            relays: false,
            uid: Some(501),
        };
        let r = reach_from(Some(silent_word), answers(true)).await;
        assert!(!r.open(), "a worker without the word is no road: {r:?}");
        assert!(
            matches!(&r, Reach::Closed(why) if why.contains(RELAY_CONSENT)),
            "{r:?}"
        );
        assert_eq!(asked.load(SeqCst), 0, "its socket is not even asked");

        let relaying = Worker {
            relays: true,
            uid: Some(501),
        };
        let r = reach_from(
            Some(Worker {
                uid: None,
                ..relaying
            }),
            answers(true),
        )
        .await;
        assert!(!r.open(), "{r:?}");
        assert_eq!(
            reach_from(Some(relaying), answers(true)).await,
            Reach::Relayed(501)
        );
        let r = reach_from(Some(relaying), answers(false)).await;
        assert!(
            matches!(&r, Reach::Closed(why) if why.contains("uid 501")),
            "{r:?}"
        );

        let asked = tokio::time::Instant::now();
        let r = Road {
            worker: relaying,
            answers: Answers::Never,
        }
        .reach()
        .await;
        assert!(!r.open(), "{r:?}");
        let waited = asked.elapsed();
        assert!(
            waited_its_bound(waited, RELAY_WITHIN),
            "a socket that never answers was waited for {waited:?}, its bound is {RELAY_WITHIN:?}"
        );
    }
}
