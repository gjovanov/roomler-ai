// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3864 — a one-row Mac's consent questions reach the menu-bar app; #3967 —
//! and a two-row Mac's root half's.
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
//! #3967 — and so do a two-row Mac's. There the root half holds a row of its
//! own (`-daemon`) and asks that row's exec and SSH questions (on a supervised
//! Mac, its remote-control questions too) on its own broker, and the socket
//! the app reaches is the per-user half's, whose list was its own broker's.
//!
//! | Half | What it does |
//! |---|---|
//! | the worker (`run --supervised --unenrolled`) | lists its own questions, then the root half's, asked over the root's person socket ([`Relay::listed`]); #4011 — keeps the root half's last list through a list the root half misses, [`ROOT_LIST_KEPT_FOR`] from its answer at most, and takes those questions off at once when the root half refuses or proves gone; sends an answer to a question its own broker is not asking there ([`decided`]); says [`RELAY_CONSENT`] in its caps frame |
//! | the root daemon | counts the app as a question's surface only while that road is open ([`reach`]): an attached worker that said [`RELAY_CONSENT`], and the person socket of that worker's account answering within [`RELAY_WITHIN`] |
//! | #3967 — a two-row Mac's per-user half (a Mac daemon that runs as a person and holds an enrollment) | lists and decides as the worker does; says `consent_relaying` over the root half's person socket for its person and holds that connection (`keep_telling`) |
//! | #3967 — a two-row Mac's root half | takes that announcement from the socket's own person, for as long as its connection stays open ([`Announcements`]), and counts the app only while one is live and that person's socket answers within [`RELAY_WITHIN`] ([`reach_two_rows`]) |
//!
//! Nothing is written anywhere: the question crosses the root's person socket
//! (`/var/run/roomler-person/<uid>.sock`), which serves only the person at the
//! console and root (`localapi::person_peer_ok`) and asks the seat again
//! before it takes an answer, and the worker's (or per-user half's) own
//! socket is that person's alone. The announcement lives in the root
//! daemon's memory, for its connection's life. A road that is closed leaves
//! the floor every host has: the app is not the question's surface, the
//! question stands its window for `roomlerd consent`, and it ends as
//! `no_prompt_surface`.

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

/// Where a one-row Mac's worker asks the root half: a request, and what came
/// of it ([`RootReply`]).
#[async_trait::async_trait]
pub trait RootHalf: Send + Sync {
    async fn ask(&self, req: &Request) -> RootReply;
}

/// #4011 — what an ask of the root half came to. A relaying process keeps
/// the root half's last list through no answer, and through nothing else
/// ([`Relay`]).
#[derive(Debug, Clone, PartialEq)]
pub enum RootReply {
    /// The root half answered, in its own words: a list, a verdict, or a
    /// refusal (an `Error`, as after a seat change).
    Answer(Box<Response>),
    /// Proof that the root half is gone: no socket at its person socket's
    /// path for this account, or nothing listening on one that is there
    /// (`NotFound`, `ConnectionRefused`), as #3998's
    /// `localapi::own_endpoint_gone` reads an endpoint.
    Gone,
    /// No answer, and why: none within the ask's bound, or a connect or a
    /// request that failed any other way.
    NoAnswer(String),
}

impl RootReply {
    /// What a connect to the root half's person socket that failed proves:
    /// gone when nothing is there to connect to, no answer for any other
    /// failure (a socket that someone other than root serves, among them).
    pub fn not_connected(e: &std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => Self::Gone,
            _ => Self::NoAnswer(format!("the connect failed: {e}")),
        }
    }
}

/// The root half as the daemon asks it: the root daemon's person socket for
/// this process's account, kept only when root serves it
/// (`person_socket::machine_reply`). There is none off macOS.
pub struct PersonSocket;

#[async_trait::async_trait]
impl RootHalf for PersonSocket {
    async fn ask(&self, req: &Request) -> RootReply {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: geteuid never fails.
            let euid = unsafe { libc::geteuid() };
            // Each caller bounds its own ask ([`Relay::listed`], [`decided`]).
            crate::person_socket::machine_reply(euid, req, DECIDE_WITHIN).await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = req;
            RootReply::Gone
        }
    }
}

/// #4011 — how long a relaying process keeps the root half's last list
/// through lists the root half misses, from that list's answer: the bound
/// the companion's prompt keeps its questions for through its own reads that
/// fail (`QUESTIONS_KEPT_FOR`, `agents/roomler-desktop/src/main.rs`), so a
/// question nobody can decide lingers no longer here than there.
pub const ROOT_LIST_KEPT_FOR: Duration = Duration::from_secs(10);

/// The questions the person's app lists on a one-row Mac: the worker's own
/// first, then each of the root half's whose id is not there already. Ids are
/// ObjectIds, a server's session or request id or one the asking daemon
/// minted, so two brokers never hold the same one.
pub fn merged(mut own: Vec<ConsentRequest>, theirs: Vec<ConsentRequest>) -> Vec<ConsentRequest> {
    for question in theirs {
        if !own.iter().any(|q| q.session_id == question.session_id) {
            own.push(question);
        }
    }
    own
}

/// #3864 — where a relaying process (a one-row Mac's worker, a two-row Mac's
/// per-user half) asks the root half; #4011 — and the root half's last good
/// list, which a list the root half misses leaves where it was. Until #4011 a
/// missed list was the process's own list alone, which the app took as a good
/// answer: its prompt went down for a poll and came back placed anew, with
/// its click hold armed again, and the app's own hold on a failed read
/// (#3998) could not see it.
///
/// | What came of the ask ([`RootReply`]) | The root half's questions on the list |
/// |---|---|
/// | an answer: a list, empty or not | that list, which is kept in place of the last |
/// | a refusal: any other answer | none, at once |
/// | gone: no socket, or nothing listening on it | none, at once |
/// | no answer: none within [`RELAY_WITHIN`], or any other failure | the kept list, for [`ROOT_LIST_KEPT_FOR`] from its answer at most, less each question whose deadline (`expires_at_ms`) has passed |
/// | #4027 — the root half took an answer the person gave here ([`Relay::decided`]: `ConsentDecided { ok: true }`) | the kept list less that question, at once. An answer it did not take (`ok: false`, a refusal, none) leaves the list as it was. One given elsewhere (`roomlerd consent`) or a question withdrawn is not known here: the hold and the question's deadline bound it |
///
/// The process's own questions are its broker's of the moment, never kept. A
/// run of missed lists that the kept list covers is logged once, at WARN, and
/// the end of its hold once, at INFO; so is a refusal or a proof of gone that
/// takes kept questions off. The app's polls are never logged.
///
/// #4027 — one window is left. The root half lists a question until its
/// prompt's next look takes the answer (`consent::POLL_INTERVAL`, 250 ms), so
/// a list the root half answered before its prompt's look took the answer,
/// read here after the verdict, brings the question back until the next
/// answered list. The app polls every 750 ms on its own clock, so by the
/// cadence about one answer in six meets that window, and the question
/// lingers only when a miss follows: for the hold at most. A tombstone for it
/// would have to be bounded in time: a remote-control session can be asked
/// again under its own id (`PendingGuard::release` in `consent.rs`), and a
/// tombstone kept by id alone would hide that new question.
pub struct Relay {
    root: std::sync::Arc<dyn RootHalf>,
    kept: std::sync::Mutex<Kept>,
}

/// The root half's last good list, as a [`Relay`] keeps it.
#[derive(Debug, Default)]
struct Kept {
    /// The root half's questions in its last answer, less each it took an
    /// answer for since (#4027), and when that answer came. `None` before the
    /// first answer, and once a refusal, a proof that it is gone, or the end
    /// of the hold took them off.
    last: Option<(Vec<ConsentRequest>, tokio::time::Instant)>,
    /// Whether the run of missed lists since that answer has said so.
    said: bool,
}

impl Relay {
    /// A relay that asks `root`.
    pub fn new(root: std::sync::Arc<dyn RootHalf>) -> Self {
        Self {
            root,
            kept: Default::default(),
        }
    }

    /// The daemon's own: over the root daemon's person socket
    /// ([`PersonSocket`]).
    pub fn to_the_root_half() -> Self {
        Self::new(std::sync::Arc::new(PersonSocket))
    }

    /// [`merged`]: `own`, then the root half's questions, asked for
    /// [`RELAY_WITHIN`] at most, as what came of the ask leaves them
    /// ([`Relay`]'s table).
    pub async fn listed(&self, own: Vec<ConsentRequest>) -> Vec<ConsentRequest> {
        let reply = tokio::time::timeout(RELAY_WITHIN, self.root.ask(&Request::ConsentPending))
            .await
            .unwrap_or_else(|_| RootReply::NoAnswer(format!("none within {RELAY_WITHIN:?}")));
        let theirs = self.theirs_after(reply, tokio::time::Instant::now(), wall_clock_ms());
        merged(own, theirs)
    }

    /// [`decided`], on this relay's road. #4027 — when the root half takes
    /// the answer (`ConsentDecided { ok: true }`), its question comes off the
    /// kept list at once (`Kept::answer_taken`), so a list the root half
    /// misses after it no longer lists it. Any other outcome leaves the kept
    /// list as it was. The app gets [`decided`]'s answer as it is.
    pub async fn decided(&self, session_id: &str, allow: bool) -> Response {
        let answer = decided(self.root.as_ref(), session_id, allow).await;
        if matches!(answer, Response::ConsentDecided { ok: true }) {
            // After the ask, never across it: the app's lists take this lock.
            self.kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .answer_taken(session_id);
        }
        answer
    }

    /// The root half's questions on the list after `reply`, at `now`;
    /// `now_ms` is the wall clock a question's deadline is read on.
    fn theirs_after(
        &self,
        reply: RootReply,
        now: tokio::time::Instant,
        now_ms: u64,
    ) -> Vec<ConsentRequest> {
        let mut kept = self
            .kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match reply {
            RootReply::Answer(answer) => match *answer {
                Response::ConsentPending(theirs) => {
                    kept.last = Some((theirs.clone(), now));
                    kept.said = false;
                    theirs
                }
                other => {
                    let answer = match other {
                        Response::Error { message } => message,
                        _ => "an answer that is not a list".into(),
                    };
                    kept.off(&format!("the root half refused the list ({answer})"));
                    Vec::new()
                }
            },
            RootReply::Gone => {
                kept.off(
                    "the root half is gone (no person socket for this account, or nothing \
                     listening on it)",
                );
                Vec::new()
            }
            RootReply::NoAnswer(why) => kept.through_a_miss(&why, now, now_ms),
        }
    }
}

impl Kept {
    /// A refusal or a proof of gone, `why`: the root half's questions come
    /// off at once, said once when there were any.
    fn off(&mut self, why: &str) {
        if let Some((questions, _)) = self.last.take()
            && !questions.is_empty()
        {
            tracing::info!(
                questions = questions.len(),
                "consent relay: {why}; its questions come off the person's list at once (#4011)"
            );
        }
        self.said = false;
    }

    /// No answer, `why`: the kept list while its answer is less than
    /// [`ROOT_LIST_KEPT_FOR`] old, less each question whose deadline has
    /// passed; none after that.
    fn through_a_miss(
        &mut self,
        why: &str,
        now: tokio::time::Instant,
        now_ms: u64,
    ) -> Vec<ConsentRequest> {
        let Some((questions, answered)) = self.last.as_mut() else {
            return Vec::new();
        };
        // A deadline that has passed is a question over. `0` is one not known
        // (a daemon older than FR-27), which only the hold bounds.
        questions.retain(|q| q.expires_at_ms == 0 || q.expires_at_ms > now_ms);
        if questions.is_empty() {
            return Vec::new();
        }
        let since = now.saturating_duration_since(*answered);
        if since >= ROOT_LIST_KEPT_FOR {
            tracing::info!(
                questions = questions.len(),
                since_its_answer = ?since,
                "consent relay: the root half has not answered a list for {ROOT_LIST_KEPT_FOR:?}; \
                 its questions come off the person's list (#4011)"
            );
            self.last = None;
            self.said = false;
            return Vec::new();
        }
        if !self.said {
            tracing::warn!(
                questions = questions.len(),
                %why,
                "consent relay: the root half did not answer the list; its last one stays on the \
                 person's list, for {ROOT_LIST_KEPT_FOR:?} from its answer at most (#4011)"
            );
            self.said = true;
        }
        questions.clone()
    }

    /// #4027 — the root half took an answer to `session_id`: that question
    /// comes off the kept list at once. The answer's time, the hold, and what
    /// the run of misses has said stay as they were.
    fn answer_taken(&mut self, session_id: &str) {
        if let Some((questions, _)) = self.last.as_mut() {
            questions.retain(|q| q.session_id != session_id);
        }
    }
}

/// The wall clock in Unix milliseconds, which a question's deadline
/// (`expires_at_ms`) is written on: the root half writes it on the same
/// host's clock.
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    match tokio::time::timeout(DECIDE_WITHIN, root.ask(&req)).await {
        Ok(RootReply::Answer(answer))
            if matches!(
                *answer,
                Response::ConsentDecided { .. } | Response::Error { .. }
            ) =>
        {
            *answer
        }
        other => {
            tracing::warn!(
                session = %session_id,
                answered = matches!(other, Ok(RootReply::Answer(_))),
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
    relayed_if_it_answers(uid, answers).await
}

/// The last step of either road: the person socket for `uid` answers a list
/// within [`RELAY_WITHIN`], or the road is closed.
async fn relayed_if_it_answers<F, Fut>(uid: u32, answers: F) -> Reach
where
    F: FnOnce(u32) -> Fut,
    Fut: Future<Output = bool>,
{
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

/// Whether this process is a one-row Mac's root daemon ([`register`]).
pub fn one_row() -> bool {
    ONE_ROW_HOST.get().is_some()
}

/// The road a question this daemon asks takes to the menu-bar app: a one-row
/// Mac's root daemon reads its worker ([`reach_from`]); #3967 — any other
/// Mac's root half reads the announcement the console person's per-user half
/// made over its person socket ([`reach_two_rows`]); every other daemon is
/// asked by its tray directly.
pub async fn reach() -> Reach {
    if let Some(host) = ONE_ROW_HOST.get() {
        let worker = Worker {
            relays: host.relays_consent(),
            uid: host.worker_uid(),
        };
        return reach_from(Some(worker), person_socket_answers).await;
    }
    if a_macs_root_half() {
        return reach_two_rows(announcements().current(), person_socket_answers).await;
    }
    Reach::Direct
}

/// #3967 — whether this process is a Mac's root half: root, on a Mac. Never
/// in a test, which stands its road in.
fn a_macs_root_half() -> bool {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        // SAFETY: geteuid never fails.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(all(target_os = "macos", not(test))))]
    {
        false
    }
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

/// #3967 — the road from a two-row Mac's root half to the console person's
/// app: open while that person's per-user half has said it lists the root
/// half's questions (`announced`, [`Announcements::current`]), and the person
/// socket for that person answers a list within [`RELAY_WITHIN`]. `answers`
/// asks it.
pub async fn reach_two_rows<F, Fut>(announced: Option<u32>, answers: F) -> Reach
where
    F: FnOnce(u32) -> Fut,
    Fut: Future<Output = bool>,
{
    let Some(uid) = announced else {
        return Reach::Closed(NOBODY_ANNOUNCED.into());
    };
    relayed_if_it_answers(uid, answers).await
}

/// #3967 — why a two-row Mac's root half does not count the app when no
/// per-user half has announced.
const NOBODY_ANNOUNCED: &str = "no per-user half of the person at the screen has said it lists \
                                this daemon's consent questions (`consent_relaying`): an older \
                                one, or none running";

/// #3967 — the per-user halves that told this root daemon, over its person
/// socket, that they list its consent questions to their person's app
/// ([`tunnel_core::localapi::Request::ConsentRelaying`]): one entry for each
/// connection that said so, for as long as it stays open.
#[derive(Debug, Default)]
pub struct Announcements {
    live: std::sync::Mutex<Vec<(u64, u32)>>,
    next: std::sync::atomic::AtomicU64,
}

impl Announcements {
    /// `uid`'s per-user half said so, on a connection that holds the answer
    /// until it ends: when the per-user half closes it (it stopped, or
    /// restarted), or when the person socket stops serving it (the person at
    /// the screen changed, or this daemon is stopping). Each start and each
    /// end is logged once, with why it ended.
    pub fn announce(
        self: &std::sync::Arc<Self>,
        uid: u32,
    ) -> tunnel_core::localapi::HeldForTheConnection {
        use std::sync::atomic::Ordering;
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((token, uid));
        tracing::info!(
            uid,
            "consent relay: a two-row Mac's per-user half lists this daemon's consent questions \
             to its person's app, which counts as their surface while it does (#3967)"
        );
        let book = self.clone();
        tunnel_core::localapi::HeldForTheConnection::new(move |how| {
            book.live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|&(t, _)| t != token);
            let why = match how {
                tunnel_core::localapi::ConnectionEnd::ClosedByThePeer => {
                    "its connection closed: the per-user half stopped or restarted"
                }
                tunnel_core::localapi::ConnectionEnd::Dropped => {
                    "the person socket stopped serving it: the person at the screen changed, or \
                     this daemon is stopping"
                }
            };
            tracing::info!(
                uid,
                why,
                "consent relay: the per-user half no longer lists this daemon's consent \
                 questions; the app is not their surface (#3967)"
            );
        })
    }

    /// The person whose per-user half said so last, while its connection is
    /// open. The person socket serves one person at a time, and stops every
    /// connection of the last before it serves the next, so this is the
    /// person at the screen, or was within its look's 5 s.
    pub fn current(&self) -> Option<u32> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .map(|&(_, uid)| uid)
    }
}

/// #3967 — this process's [`Announcements`]: what a two-row Mac's root half
/// takes over its person socket, and reads at every prompt ([`reach`]).
pub fn announcements() -> std::sync::Arc<Announcements> {
    static BOOK: OnceLock<std::sync::Arc<Announcements>> = OnceLock::new();
    BOOK.get_or_init(Default::default).clone()
}

/// #3967 — how soon a per-user half dials its root half again after a dial
/// found no root half to tell: from this, doubling to [`ANNOUNCE_AGAIN_MOST`].
pub const ANNOUNCE_AGAIN_FIRST: Duration = Duration::from_secs(1);

/// #3967 — the longest a per-user half waits between its dials while its
/// root half is out of reach (someone else is at the screen, or the root
/// daemon is down). The person socket follows the seat every 5 s
/// (`person_socket::POLL`), so a person who comes to the screen is counted
/// within about 10 s.
pub const ANNOUNCE_AGAIN_MOST: Duration = Duration::from_secs(5);

/// #3967 — how long a per-user half waits for its root half's answer.
pub const ANNOUNCE_ANSWER_WITHIN: Duration = Duration::from_secs(5);

/// #3967 — how long a per-user half waits before it dials again a root half
/// that refused it twice in a row, each time on a connection that ended
/// within [`HELD_STEADY`]: a root half that refuses and closes at once. The
/// first such refusal goes on the backoff ([`ANNOUNCE_AGAIN_FIRST`]). At an
/// update the installer restarts a two-row Mac's per-user half about 3 s
/// before its root half, so the new per-user half meets the old root half,
/// which refuses and holds the connection until it stops; the root half that
/// replaces it is then told within seconds, not a minute later. An older root
/// half keeps the connection open (0.4.223 and 0.4.224 answer "unknown
/// variant" and read on), and that connection is held, so it is never dialled
/// again until it ends.
pub const ANNOUNCE_AFTER_A_REFUSAL: Duration = Duration::from_secs(60);

/// #3967 — a connection held this long is a steady one: when it ends (the
/// root half restarted, or the seat moved) the next dial waits
/// [`ANNOUNCE_AGAIN_FIRST`] again.
pub const HELD_STEADY: Duration = Duration::from_secs(30);

/// #3967 — a two-row Mac's per-user half tells its root half, over each
/// connection `dial` makes to the root half's person socket for this
/// person, that it lists the root half's questions to its person's app
/// ([`tunnel_core::localapi::Request::ConsentRelaying`]), and holds that
/// connection until it ends or `shutdown` flips.
///
/// Never in a loop. A held connection is not dialled again: the root half
/// sends nothing on it unasked, and its listener holds an idle connection
/// with no deadline (`localapi::serve_person_listener`). One that ended after
/// [`HELD_STEADY`] is dialled again [`ANNOUNCE_AGAIN_FIRST`] later; a dial
/// that finds no root half, or no answer in [`ANNOUNCE_ANSWER_WITHIN`], waits
/// twice as long as the last, up to [`ANNOUNCE_AGAIN_MOST`]. A root half that
/// refused and closed the connection soon after is dialled on that backoff
/// too, and only a second such refusal in a row, with no take between them,
/// waits [`ANNOUNCE_AFTER_A_REFUSAL`]. Each change of what the root half was
/// told is logged once, never a retry.
#[cfg(any(target_os = "macos", test))]
pub(crate) async fn keep_telling<D, F, S>(
    mut dial: D,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    D: FnMut() -> F,
    F: Future<Output = Option<S>>,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut said = Said::Nothing;
    let mut wait = ANNOUNCE_AGAIN_FIRST;
    // A refusal whose connection ended within HELD_STEADY, with no take and
    // no steady hold since. A dial that found no root half, or no answer,
    // leaves it on the books.
    let mut refused_soon = false;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let told = match dial().await {
            Some(stream) => tell_and_hold(stream, &mut shutdown, &mut said).await,
            None => Told::Unreachable,
        };
        let pause = match told {
            Told::Stopping => return,
            Told::Held(held) | Told::Refused(held) if held >= HELD_STEADY => {
                refused_soon = false;
                wait = ANNOUNCE_AGAIN_FIRST;
                doubled(&mut wait)
            }
            // The second in a row: a root half that refuses and closes.
            Told::Refused(_) if refused_soon => ANNOUNCE_AFTER_A_REFUSAL,
            // The first: as at an update, an older root half that stopped
            // soon after it refused. The one that replaces it is dialled on
            // the backoff.
            Told::Refused(_) => {
                refused_soon = true;
                doubled(&mut wait)
            }
            Told::Held(_) => {
                refused_soon = false;
                doubled(&mut wait)
            }
            Told::Unreachable | Told::NoAnswer => doubled(&mut wait),
        };
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            () = stopping(&mut shutdown) => return,
        }
    }
}

/// #3967 — what one dial of [`keep_telling`] came to.
#[cfg(any(target_os = "macos", test))]
enum Told {
    /// No root half to tell: no socket for this person, or no connection.
    Unreachable,
    /// A connection, and no answer in time (or it closed before one).
    NoAnswer,
    /// The root half took it, and the connection stood this long.
    Held(Duration),
    /// The root half refused it (an older one answers "unknown variant"), and
    /// the connection stood this long.
    Refused(Duration),
    /// `shutdown` flipped.
    Stopping,
}

/// #3967 — what a per-user half last said in its log, so each change is
/// said once.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    Nothing,
    Announced,
    Refused,
}

/// The pause before the next dial: `wait`, which then doubles, up to
/// [`ANNOUNCE_AGAIN_MOST`].
#[cfg(any(target_os = "macos", test))]
fn doubled(wait: &mut Duration) -> Duration {
    let pause = *wait;
    *wait = (*wait * 2).min(ANNOUNCE_AGAIN_MOST);
    pause
}

/// Ends when `shutdown` is true, or its sender is gone.
#[cfg(any(target_os = "macos", test))]
async fn stopping(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// One connection of [`keep_telling`]: say it, read the root half's answer,
/// and hold the connection until the root half closes it or `shutdown`
/// flips.
#[cfg(any(target_os = "macos", test))]
async fn tell_and_hold<S>(
    stream: S,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    said: &mut Said,
) -> Told
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut conn = tokio::io::BufReader::new(stream);
    let mut ask = serde_json::to_vec(&Request::ConsentRelaying).expect("a request serialises");
    ask.push(b'\n');
    let mut line = String::new();
    let asked = tokio::time::timeout(ANNOUNCE_ANSWER_WITHIN, async {
        conn.write_all(&ask).await?;
        conn.flush().await?;
        conn.read_line(&mut line).await
    });
    let read = tokio::select! {
        read = asked => read,
        () = stopping(shutdown) => return Told::Stopping,
    };
    let answer = match read {
        Ok(Ok(n)) if n > 0 => serde_json::from_str::<Response>(line.trim_end()).ok(),
        _ => None,
    };
    let refused = match answer {
        Some(Response::ConsentRelayingTaken) => {
            if *said != Said::Announced {
                tracing::info!(
                    "consent relay: told the root half, which counts this person's app as its \
                     questions' surface while this connection stays open (#3967)"
                );
                *said = Said::Announced;
            }
            false
        }
        Some(Response::Error { message }) => {
            if *said != Said::Refused {
                if tunnel_core::localapi::is_unknown_verb(&message) {
                    tracing::info!(
                        answer = %message,
                        "consent relay: the root half is older than the announcement (#3967), so \
                         it does not count this person's app; its questions are still listed here"
                    );
                } else {
                    tracing::info!(
                        answer = %message,
                        "consent relay: the root half did not take the announcement (#3967); its \
                         questions are still listed here"
                    );
                }
                *said = Said::Refused;
            }
            true
        }
        _ => return Told::NoAnswer,
    };
    let since = tokio::time::Instant::now();
    let held = async {
        let mut rest = String::new();
        loop {
            rest.clear();
            match conn.read_line(&mut rest).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    };
    tokio::select! {
        () = held => {}
        () = stopping(shutdown) => return Told::Stopping,
    }
    let stood = since.elapsed();
    if refused {
        return Told::Refused(stood);
    }
    tracing::info!(
        "consent relay: the root half's person socket closed (it restarted, or the person at the \
         screen changed); telling it again once it is back (#3967)"
    );
    *said = Said::Nothing;
    Told::Held(stood)
}

/// #3967 — a two-row Mac's per-user half tells its root half, for as long as
/// it runs ([`keep_telling`], over the root half's person socket for this
/// account). Started by `person_socket::run` on a Mac daemon that runs as a
/// person and holds an enrollment
/// (`LocalApiState::announces_consent_relay`).
#[cfg(target_os = "macos")]
pub async fn tell_the_root_half(shutdown: tokio::sync::watch::Receiver<bool>) {
    keep_telling(dial_the_root_half, shutdown).await
}

/// #3967 — the root half's person socket for this account, kept only when
/// root serves it, as the relay asks it (`person_socket::machine_reply`).
/// `None` while there is none: someone else is at the screen, the root
/// daemon is down, or its person socket is switched off.
#[cfg(target_os = "macos")]
async fn dial_the_root_half() -> Option<tokio::net::UnixStream> {
    // SAFETY: geteuid never fails.
    let euid = unsafe { libc::geteuid() };
    let path = tunnel_core::localapi::person_socket_path(euid);
    if !path.exists() {
        return None;
    }
    tokio::time::timeout(
        RELAY_WITHIN,
        tunnel_core::localapi::connect_unix_stream_served_by_root(path),
    )
    .await
    .ok()?
    .ok()
}

/// A one-row Mac's road, stood in for a test of the root daemon's verdict
/// (`companion::ensure_running`, under its tray stand-in).
#[cfg(test)]
pub(crate) mod stand_in {
    use super::{Reach, Worker, reach_from, reach_two_rows};

    /// How the stood-in person socket answers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Answers {
        Yes,
        No,
        Never,
        /// #3983 — yes, after this long: a person socket slow to answer.
        After(std::time::Duration),
    }

    impl Answers {
        /// The stood-in person socket's answer to a list.
        async fn answer(self) -> bool {
            match self {
                Self::Yes => true,
                Self::No => false,
                Self::Never => std::future::pending().await,
                Self::After(after) => {
                    tokio::time::sleep(after).await;
                    true
                }
            }
        }
    }

    /// The worker as the root daemon knows it, and its person socket.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Road {
        pub(crate) worker: Worker,
        pub(crate) answers: Answers,
    }

    impl Road {
        pub(crate) async fn reach(self) -> Reach {
            reach_from(Some(self.worker), |_| self.answers.answer()).await
        }
    }

    /// #3967 — a two-row Mac's road, as its root half knows it: the person
    /// whose per-user half announced, if any, and how that person's socket
    /// answers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Announced {
        pub(crate) uid: Option<u32>,
        pub(crate) answers: Answers,
    }

    impl Announced {
        pub(crate) async fn reach(self) -> Reach {
            reach_two_rows(self.uid, |_| self.answers.answer()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::{Answers, Road};
    use super::*;
    use std::sync::Arc;
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
        async fn ask(&self, _req: &Request) -> RootReply {
            std::future::pending().await
        }
    }

    /// The root half that answers `0`, counting its asks.
    struct Answering(Response, AtomicUsize);
    #[async_trait::async_trait]
    impl RootHalf for Answering {
        async fn ask(&self, _req: &Request) -> RootReply {
            self.1.fetch_add(1, SeqCst);
            RootReply::Answer(Box::new(self.0.clone()))
        }
    }

    /// The worker's own questions come first, then the root half's, each id
    /// once.
    #[test]
    fn the_list_is_the_workers_own_then_the_root_halfs_each_once() {
        let own = vec![question("a1"), question("b2")];
        let theirs = vec![question("c3"), question("a1")];
        assert_eq!(ids(&merged(own.clone(), theirs)), ["a1", "b2", "c3"]);
        assert_eq!(ids(&merged(own, Vec::new())), ["a1", "b2"]);
    }

    /// #3864, the owner's question 5 — the relay is bounded: a root half that
    /// never answers leaves the worker's own list within [`RELAY_WITHIN`], and
    /// an answer the person gave is refused, in words that say to try again,
    /// within [`DECIDE_WITHIN`].
    #[tokio::test(start_paused = true)]
    async fn a_silent_root_half_leaves_the_own_list_and_refuses_an_answer_in_time() {
        let asked = tokio::time::Instant::now();
        let list = Relay::new(Arc::new(Silent))
            .listed(vec![question("a1")])
            .await;
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

    /// #4011 — the root half, scripted: each ask takes the next reply in
    /// turn, and [`MISSES`] never answers (a root half slow for that list).
    struct Scripted(std::sync::Mutex<std::collections::VecDeque<Option<RootReply>>>);

    impl Scripted {
        fn new(replies: impl IntoIterator<Item = Option<RootReply>>) -> Arc<Self> {
            Arc::new(Self(std::sync::Mutex::new(replies.into_iter().collect())))
        }

        /// The replies to the asks after the ones scripted so far.
        fn then(&self, replies: impl IntoIterator<Item = Option<RootReply>>) {
            self.0.lock().unwrap().extend(replies);
        }
    }

    #[async_trait::async_trait]
    impl RootHalf for Scripted {
        async fn ask(&self, _req: &Request) -> RootReply {
            let next = self.0.lock().unwrap().pop_front();
            match next.expect("the root half was asked more often than scripted") {
                Some(reply) => reply,
                None => std::future::pending().await,
            }
        }
    }

    /// #4011 — a connect to the root half's person socket that failed is
    /// proof that it is gone only when nothing is there to connect to (no
    /// socket, or nothing listening on it); any other failure is no answer.
    /// `person_socket::machine_reply_tests` reads the same on a real path.
    #[test]
    fn a_failed_connect_is_gone_only_when_nothing_is_there() {
        use std::io::{Error, ErrorKind};
        for gone in [ErrorKind::NotFound, ErrorKind::ConnectionRefused] {
            assert_eq!(
                RootReply::not_connected(&Error::from(gone)),
                RootReply::Gone,
                "{gone:?}"
            );
        }
        for other in [
            Error::new(ErrorKind::PermissionDenied, "served by uid 501, not root"),
            Error::from(ErrorKind::WouldBlock),
            Error::from(ErrorKind::TimedOut),
        ] {
            let reply = RootReply::not_connected(&other);
            assert!(
                matches!(&reply, RootReply::NoAnswer(why) if why.contains("the connect failed")),
                "{other:?}: {reply:?}"
            );
        }
    }

    /// The root half lists `questions`.
    fn lists(questions: &[ConsentRequest]) -> Option<RootReply> {
        Some(RootReply::Answer(Box::new(Response::ConsentPending(
            questions.to_vec(),
        ))))
    }

    /// A list the root half misses: it never answers, so the relay's own
    /// bound, [`RELAY_WITHIN`], ends the ask.
    const MISSES: Option<RootReply> = None;

    /// The lines logged at a run of missed lists the kept list covers, and at
    /// the end of its hold, and when a refusal or a proof of gone takes the
    /// root half's questions off.
    const MISS_COVERED: &str = "the root half did not answer the list; its last one stays";
    const HOLD_ENDED: &str = "the root half has not answered a list for";
    const TAKEN_OFF: &str = "its questions come off the person's list at once";

    /// How many times `line` is in `logged`.
    fn times(logged: &crate::peer::files_log_tests::Logged, line: &str) -> usize {
        logged.text().matches(line).count()
    }

    /// #4011 — a root half that misses one list between two answers leaves
    /// its question where it was: on the list the app reads at every step,
    /// after the process's own, so the companion's prompt neither comes down
    /// nor comes back placed anew with its click hold armed again. The miss
    /// takes the relay's own bound, no more, and is said once, at WARN.
    ///
    /// Red before #4011: the missed list held the process's own question
    /// alone.
    #[tokio::test(start_paused = true)]
    async fn a_root_half_that_misses_one_list_leaves_its_question_where_it_was() {
        let logged = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
        let root = Scripted::new([lists(&[question("c3")]), MISSES, lists(&[question("c3")])]);
        let relay = Relay::new(root);
        for step in ["an answer", "a missed list", "an answer again"] {
            let asked = tokio::time::Instant::now();
            let list = relay.listed(vec![question("a1")]).await;
            assert_eq!(ids(&list), ["a1", "c3"], "{step}");
            let waited = asked.elapsed();
            assert!(
                waited < RELAY_WITHIN + Duration::from_millis(50),
                "{step}: the list waited {waited:?}"
            );
        }
        assert_eq!(times(&logged, MISS_COVERED), 1, "{}", logged.text());
        assert_eq!(times(&logged, HOLD_ENDED), 0, "{}", logged.text());
    }

    /// #4011 — a root half that proves gone (no person socket, or nothing
    /// listening on it) or refuses the list takes its questions off at once:
    /// no wait for a bound, and nothing kept for a miss after it. Each is
    /// said once, at INFO.
    #[tokio::test(start_paused = true)]
    async fn a_root_half_that_is_gone_or_refuses_takes_its_questions_off_at_once() {
        let refusal = RootReply::Answer(Box::new(Response::Error {
            message: "only the person signed in at this device's own screen answers".into(),
        }));
        for (what, ends) in [("gone", RootReply::Gone), ("a refusal", refusal)] {
            let logged = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
            let root = Scripted::new([lists(&[question("c3")]), Some(ends), MISSES]);
            let relay = Relay::new(root);
            let own = || vec![question("a1")];
            assert_eq!(ids(&relay.listed(own()).await), ["a1", "c3"], "{what}");
            let asked = tokio::time::Instant::now();
            assert_eq!(ids(&relay.listed(own()).await), ["a1"], "{what}");
            assert_eq!(asked.elapsed(), Duration::ZERO, "{what}: at once");
            assert_eq!(
                ids(&relay.listed(own()).await),
                ["a1"],
                "{what}: a miss after it brings nothing back"
            );
            assert_eq!(times(&logged, TAKEN_OFF), 1, "{what}: {}", logged.text());
            assert_eq!(times(&logged, MISS_COVERED), 0, "{what}: {}", logged.text());
        }
    }

    /// #4011 — the kept list lasts [`ROOT_LIST_KEPT_FOR`] from the root
    /// half's LAST answer, and no longer. With the app asking every 750 ms
    /// and the root half missing every list, its question is on each list
    /// that ends within the bound and off every one after it. An answer
    /// within the bound starts it again. Each run of misses is said once, at
    /// WARN, and the end of the hold once, at INFO: never a line a poll.
    #[tokio::test(start_paused = true)]
    async fn a_root_half_that_misses_past_the_bound_loses_its_questions_there() {
        const POLL: Duration = Duration::from_millis(750);
        let logged = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
        let root = Scripted::new([lists(&[question("c3")])]);
        let relay = Relay::new(root.clone());
        assert_eq!(ids(&relay.listed(Vec::new()).await), ["c3"]);
        let answered = tokio::time::Instant::now();
        let mut misses = 0;
        while answered.elapsed() < ROOT_LIST_KEPT_FOR - Duration::from_secs(2) {
            tokio::time::sleep(POLL).await;
            root.then([MISSES]);
            let list = relay.listed(Vec::new()).await;
            assert_eq!(ids(&list), ["c3"], "at {:?}", answered.elapsed());
            misses += 1;
        }
        assert!(misses >= 4, "{misses} missed lists");

        tokio::time::sleep(POLL).await;
        root.then([lists(&[question("c3")])]);
        assert_eq!(ids(&relay.listed(Vec::new()).await), ["c3"]);
        let again = tokio::time::Instant::now();
        let (mut kept, mut off, mut kept_past_the_first) = (0, 0, false);
        while again.elapsed() < ROOT_LIST_KEPT_FOR + Duration::from_secs(3) {
            tokio::time::sleep(POLL).await;
            root.then([MISSES]);
            let list = relay.listed(Vec::new()).await;
            let at = again.elapsed();
            if at < ROOT_LIST_KEPT_FOR {
                assert_eq!(ids(&list), ["c3"], "{at:?} after the answer");
                kept += 1;
                kept_past_the_first |= answered.elapsed() >= ROOT_LIST_KEPT_FOR;
            } else {
                assert!(list.is_empty(), "{at:?} after the answer: {:?}", ids(&list));
                off += 1;
            }
        }
        assert!(kept >= 4 && off >= 1, "kept {kept}, off {off}");
        assert!(
            kept_past_the_first,
            "the bound counts from the last answer, not the first"
        );
        assert_eq!(times(&logged, MISS_COVERED), 2, "{}", logged.text());
        assert_eq!(times(&logged, HOLD_ENDED), 1, "{}", logged.text());
    }

    /// #4011 — a kept question whose deadline has passed is over: a miss
    /// after its deadline does not keep it, while one whose deadline is still
    /// ahead is kept, and so is one whose deadline is not known (`0`), for
    /// the hold alone. An answer lists what the root half says, as it says
    /// it.
    #[tokio::test(start_paused = true)]
    async fn a_kept_question_past_its_deadline_is_not_kept() {
        let at = |id: &str, expires_at_ms: u64| ConsentRequest {
            expires_at_ms,
            ..question(id)
        };
        // The deadline passes between the answer and the miss.
        let relay = Relay::new(Scripted::new([]));
        let t0 = 1_800_000_000_000;
        let now = tokio::time::Instant::now();
        let answer = RootReply::Answer(Box::new(Response::ConsentPending(vec![
            at("c3", t0 + 5_000),
            at("d4", t0 + 60_000),
            at("e5", 0),
        ])));
        assert_eq!(
            ids(&relay.theirs_after(answer, now, t0)),
            ["c3", "d4", "e5"]
        );
        let missed = RootReply::NoAnswer("none within 1s".into());
        let later = now + Duration::from_secs(6);
        assert_eq!(
            ids(&relay.theirs_after(missed, later, t0 + 6_000)),
            ["d4", "e5"]
        );

        // And on the wall clock, through the relay's own list.
        let root = Scripted::new([
            lists(&[at("c3", 1), at("d4", wall_clock_ms() + 3_600_000)]),
            MISSES,
        ]);
        let relay = Relay::new(root);
        assert_eq!(ids(&relay.listed(Vec::new()).await), ["c3", "d4"]);
        assert_eq!(ids(&relay.listed(Vec::new()).await), ["d4"]);
    }

    /// #4011 — an answer replaces the kept list, an empty one too: a question
    /// the root half no longer lists is not brought back by a miss after it.
    /// And the process's own questions are its broker's of the moment, never
    /// kept: one that has ended is off the next list, miss or not.
    #[tokio::test(start_paused = true)]
    async fn an_empty_answer_empties_the_kept_list_and_the_own_questions_are_never_kept() {
        let root = Scripted::new([lists(&[question("c3")]), lists(&[]), MISSES]);
        let relay = Relay::new(root.clone());
        let own = || vec![question("a1")];
        assert_eq!(ids(&relay.listed(own()).await), ["a1", "c3"]);
        assert_eq!(ids(&relay.listed(own()).await), ["a1"], "an empty answer");
        assert_eq!(
            ids(&relay.listed(own()).await),
            ["a1"],
            "a miss after it brings nothing back"
        );

        root.then([lists(&[question("c3")]), MISSES]);
        assert_eq!(ids(&relay.listed(own()).await), ["a1", "c3"]);
        assert_eq!(
            ids(&relay.listed(Vec::new()).await),
            ["c3"],
            "the own question has ended, and a miss does not keep it"
        );
    }

    /// The root half's verdict on an answer this process sent it.
    fn decides(verdict: Response) -> Option<RootReply> {
        Some(RootReply::Answer(Box::new(verdict)))
    }

    /// #4027 — an answer the root half took (`ConsentDecided { ok: true }`)
    /// takes its question off the kept list at once: the misses after it list
    /// the root half's other question only, whether the answer came before
    /// the run of misses or during it. The app gets the root half's verdict
    /// as it is, and nothing else changes: the other question stays until the
    /// hold's end, counted from the list's answer, and the run is said once
    /// at WARN and its end once at INFO.
    ///
    /// Red before #4027: the answered question stayed on the kept list, and
    /// on the app's, through every miss until the hold's end.
    #[tokio::test(start_paused = true)]
    async fn a_question_the_root_half_took_an_answer_for_leaves_the_kept_list_at_once() {
        for misses_first in [0, 2] {
            let when = format!("{misses_first} misses before the answer");
            let logged = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
            let root = Scripted::new([lists(&[question("c3"), question("d4")])]);
            let relay = Relay::new(root.clone());
            let own = || vec![question("a1")];
            assert_eq!(
                ids(&relay.listed(own()).await),
                ["a1", "c3", "d4"],
                "{when}"
            );
            let answered = tokio::time::Instant::now();
            for _ in 0..misses_first {
                root.then([MISSES]);
                assert_eq!(
                    ids(&relay.listed(own()).await),
                    ["a1", "c3", "d4"],
                    "{when}"
                );
            }

            root.then([decides(Response::ConsentDecided { ok: true })]);
            assert_eq!(
                relay.decided("c3", true).await,
                Response::ConsentDecided { ok: true },
                "{when}: the root half's verdict, as it is"
            );
            root.then([MISSES]);
            assert_eq!(
                ids(&relay.listed(own()).await),
                ["a1", "d4"],
                "{when}: the miss after the answer"
            );

            // The other question's hold is as it was: on every list that ends
            // within the bound of the list's answer, and off after it.
            let (mut kept, mut off) = (0, 0);
            while answered.elapsed() < ROOT_LIST_KEPT_FOR + Duration::from_secs(2) {
                root.then([MISSES]);
                let list = relay.listed(own()).await;
                let at = answered.elapsed();
                if at < ROOT_LIST_KEPT_FOR {
                    assert_eq!(ids(&list), ["a1", "d4"], "{when}: {at:?} after the list");
                    kept += 1;
                } else {
                    assert_eq!(ids(&list), ["a1"], "{when}: {at:?} after the list");
                    off += 1;
                }
            }
            assert!(kept >= 5 && off >= 1, "{when}: kept {kept}, off {off}");
            assert_eq!(times(&logged, MISS_COVERED), 1, "{when}: {}", logged.text());
            assert_eq!(times(&logged, HOLD_ENDED), 1, "{when}: {}", logged.text());
        }
    }

    /// #4027 — an answer the root half did not take leaves the kept list as it
    /// was: its `ConsentDecided { ok: false }`, its refusal (an `Error`, as
    /// after a seat change), and no answer within [`DECIDE_WITHIN`]. The
    /// question may still stand, so both of the root half's questions stay
    /// through the miss after it, and the app gets [`decided`]'s answer as
    /// before.
    #[tokio::test(start_paused = true)]
    async fn an_answer_the_root_half_did_not_take_leaves_the_kept_list_as_it_was() {
        let refusal = Response::Error {
            message: "The answer was not taken: only the person signed in at this device's own \
                      screen answers"
                .into(),
        };
        let no_answer = Response::Error {
            message: NO_ANSWER_FROM_THE_ROOT.into(),
        };
        for (what, verdict, gets) in [
            (
                "ok: false",
                decides(Response::ConsentDecided { ok: false }),
                Response::ConsentDecided { ok: false },
            ),
            ("a refusal", decides(refusal.clone()), refusal),
            ("no answer", MISSES, no_answer),
        ] {
            let logged = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
            let root = Scripted::new([lists(&[question("c3"), question("d4")]), verdict, MISSES]);
            let relay = Relay::new(root);
            let own = || vec![question("a1")];
            assert_eq!(
                ids(&relay.listed(own()).await),
                ["a1", "c3", "d4"],
                "{what}"
            );
            assert_eq!(relay.decided("c3", true).await, gets, "{what}");
            assert_eq!(
                ids(&relay.listed(own()).await),
                ["a1", "c3", "d4"],
                "{what}: the miss after it"
            );
            assert_eq!(times(&logged, TAKEN_OFF), 0, "{what}: {}", logged.text());
        }
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

    /// #3967 — a two-row Mac's root half: the road to the console person's
    /// app is open only while that person's per-user half has announced, and
    /// the person socket for that person answers a list in time. With no
    /// announcement (an older per-user half, or none running for that
    /// person) the socket is not even asked.
    ///
    /// Red before #3967: a two-row Mac's root half looked at no road.
    #[tokio::test(start_paused = true)]
    async fn the_two_row_road_is_open_only_while_its_person_has_announced_and_the_list_answers() {
        use super::stand_in::Announced;
        let asked = AtomicUsize::new(0);
        let answers = |yes: bool| {
            let asked = &asked;
            move |_uid: u32| async move {
                asked.fetch_add(1, SeqCst);
                yes
            }
        };
        let r = reach_two_rows(None, answers(true)).await;
        assert!(
            matches!(&r, Reach::Closed(why) if why.contains("consent_relaying")),
            "nobody announced: {r:?}"
        );
        assert_eq!(
            asked.load(SeqCst),
            0,
            "with nobody announced, nothing is asked"
        );
        assert_eq!(
            reach_two_rows(Some(501), answers(true)).await,
            Reach::Relayed(501)
        );
        let r = reach_two_rows(Some(501), answers(false)).await;
        assert!(
            matches!(&r, Reach::Closed(why) if why.contains("uid 501")),
            "{r:?}"
        );

        let asked = tokio::time::Instant::now();
        let r = Announced {
            uid: Some(501),
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

    /// #3967 — an announcement lives exactly as long as its connection: the
    /// root half reads the person whose per-user half said so last, while
    /// that connection is open, and nobody once every one has ended. A
    /// per-user half that restarted holds two for a moment, and the seat's
    /// old person's are dropped before the new person's socket is served.
    ///
    /// Red before #3967: nothing was kept.
    #[test]
    fn an_announcement_lives_as_long_as_its_connection() {
        let book = std::sync::Arc::new(Announcements::default());
        assert_eq!(book.current(), None);
        let first = book.announce(501);
        assert_eq!(book.current(), Some(501));
        let second = book.announce(501);
        drop(first);
        assert_eq!(book.current(), Some(501), "one is still open");
        drop(second);
        assert_eq!(book.current(), None, "every one has ended");

        let ana = book.announce(501);
        let bob = book.announce(502);
        assert_eq!(book.current(), Some(502), "the last said");
        drop(bob);
        assert_eq!(book.current(), Some(501));
        drop(ana);
        assert_eq!(book.current(), None);
    }

    /// What the stood-in root half's person socket does at a dial.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AtTheDial {
        /// A root half of this release: it takes the announcement.
        Takes,
        /// An older root half (0.4.223, 0.4.224): "unknown variant", and it
        /// reads on.
        Older,
        /// A root half that refuses, and closes the connection at once.
        RefusesAndCloses,
        /// It accepts the connection, and never answers.
        Silent,
        /// No socket to dial: someone else is at the screen, or the root
        /// daemon is down.
        Gone,
    }

    /// The root half's state behind its person socket, as this release's
    /// takes an announcement: into `0`.
    struct Takes(std::sync::Arc<Announcements>);

    #[async_trait::async_trait]
    impl tunnel_core::localapi::LocalApiState for Takes {
        fn status(&self) -> tunnel_core::localapi::NodeStatus {
            serde_json::from_str(
                r#"{"node_id":"d","name":"mac","version":"0.4.0","mode":"service","connected":true}"#,
            )
            .unwrap()
        }
        fn peers(&self) -> Vec<tunnel_core::localapi::PeerInfo> {
            Vec::new()
        }
        fn flows(&self) -> Vec<tunnel_core::localapi::FlowInfo> {
            Vec::new()
        }
        fn consent_relaying(
            &self,
            uid: u32,
        ) -> Option<tunnel_core::localapi::HeldForTheConnection> {
            Some(self.0.announce(uid))
        }
    }

    /// The root half a per-user half of uid 501 dials, stood in: what each
    /// dial reaches (`Gone` once the plan is spent), when it was dialled,
    /// and the connections it serves.
    struct StoodRoot {
        book: std::sync::Arc<Announcements>,
        plan: std::sync::Mutex<std::collections::VecDeque<AtTheDial>>,
        dials: std::sync::Mutex<Vec<tokio::time::Instant>>,
        serving: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    }

    impl StoodRoot {
        fn new(plan: &[AtTheDial]) -> std::sync::Arc<Self> {
            let root = std::sync::Arc::new(Self {
                book: Default::default(),
                plan: Default::default(),
                dials: Default::default(),
                serving: Default::default(),
            });
            root.plan(plan);
            root
        }

        fn plan(&self, plan: &[AtTheDial]) {
            self.plan.lock().unwrap().extend(plan.iter().copied());
        }

        /// A dial, as the per-user half makes it.
        fn dial(&self) -> Option<tokio::io::DuplexStream> {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            self.dials.lock().unwrap().push(tokio::time::Instant::now());
            let next = self.plan.lock().unwrap().pop_front();
            let at = next.unwrap_or(AtTheDial::Gone);
            if at == AtTheDial::Gone {
                return None;
            }
            let (ours, theirs) = tokio::io::duplex(4096);
            let book = self.book.clone();
            let served = tokio::spawn(async move {
                match at {
                    AtTheDial::Takes => {
                        let person = tunnel_core::localapi::ClientPeer {
                            uid: Some(501),
                            gid: Some(20),
                            ..tunnel_core::localapi::ClientPeer::UNKNOWN
                        };
                        let scope = tunnel_core::localapi::Scope::Person { owner_uid: 501 };
                        let _ = tunnel_core::localapi::serve_connection_scoped(
                            theirs,
                            &Takes(book),
                            person,
                            scope,
                        )
                        .await;
                    }
                    AtTheDial::Older | AtTheDial::RefusesAndCloses => {
                        let (rd, mut wr) = tokio::io::split(theirs);
                        let mut lines = tokio::io::BufReader::new(rd).lines();
                        while let Ok(Some(_)) = lines.next_line().await {
                            // What 0.4.223 and 0.4.224 answer a verb they do
                            // not know, from serde's own error.
                            let e = serde_json::from_str::<Request>(r#"{"t":"a_verb_from_later"}"#)
                                .unwrap_err();
                            let answer = Response::Error {
                                message: format!("bad request: {e}"),
                            };
                            let mut line = serde_json::to_vec(&answer).unwrap();
                            line.push(b'\n');
                            if wr.write_all(&line).await.is_err() {
                                return;
                            }
                            if at == AtTheDial::RefusesAndCloses {
                                return;
                            }
                        }
                    }
                    AtTheDial::Silent => {
                        let _held = theirs;
                        std::future::pending::<()>().await;
                    }
                    AtTheDial::Gone => unreachable!("no connection"),
                }
            });
            self.serving.lock().unwrap().push(served);
            Some(ours)
        }

        /// The dialler a per-user half's [`keep_telling`] is given.
        fn dialler(
            self: &std::sync::Arc<Self>,
        ) -> impl FnMut() -> std::future::Ready<Option<tokio::io::DuplexStream>> + use<> {
            let root = self.clone();
            move || std::future::ready(root.dial())
        }

        fn dialled(&self) -> usize {
            self.dials.lock().unwrap().len()
        }

        /// How long each dial since `since` came after the one before it.
        fn gaps_since(&self, since: tokio::time::Instant) -> Vec<Duration> {
            let mut last = since;
            let mut gaps = Vec::new();
            for &at in self.dials.lock().unwrap().iter().filter(|&&at| at >= since) {
                gaps.push(at - last);
                last = at;
            }
            gaps
        }

        /// The root half stops serving every connection it holds: it
        /// restarted, or the seat moved (`serve_person_listener` aborts them).
        async fn stops_serving(&self) {
            let serving: Vec<_> = self.serving.lock().unwrap().drain(..).collect();
            for served in serving {
                served.abort();
                let _ = served.await;
            }
        }
    }

    /// Wait on the paused clock, `within` at most, until `holds`.
    async fn until(what: &str, within: Duration, holds: impl Fn() -> bool) {
        let by = tokio::time::Instant::now() + within;
        while !holds() {
            assert!(
                tokio::time::Instant::now() < by,
                "{what}: not within {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Whether each gap is the one expected, as the timer wheel keeps it.
    fn the_gaps_are(gaps: &[Duration], secs: &[u64]) -> bool {
        gaps.len() == secs.len()
            && gaps
                .iter()
                .zip(secs)
                .all(|(&gap, &s)| waited_its_bound(gap, Duration::from_secs(s)))
    }

    /// #3967 — a two-row Mac's per-user half tells its root half at once and
    /// holds the connection, so a held one is never dialled again. When the
    /// root half stops serving it (it restarted, or the seat moved) the
    /// announcement goes with it, and the per-user half dials again after
    /// 1 s, then 2, 4, 5 and 5 s while the root half is out of reach, and
    /// tells it again once it is back. When the per-user half stops, its
    /// announcement goes with its connection.
    ///
    /// Red before #3967: a per-user half told its root half nothing.
    #[tokio::test(start_paused = true)]
    async fn a_per_user_half_tells_its_root_half_and_tells_it_again_once_it_is_back() {
        use AtTheDial::*;
        let root = StoodRoot::new(&[Takes]);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let telling = tokio::spawn(keep_telling(root.dialler(), shutdown));
        until("told at once", Duration::from_secs(1), || {
            root.book.current() == Some(501)
        })
        .await;
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(root.book.current(), Some(501), "held");
        assert_eq!(root.dialled(), 1, "a held connection is not dialled again");

        root.plan(&[Gone, Gone, Gone, Gone, Takes]);
        let lost = tokio::time::Instant::now();
        root.stops_serving().await;
        assert_eq!(root.book.current(), None, "gone with its connection");
        until("told again", Duration::from_secs(30), || {
            root.book.current() == Some(501)
        })
        .await;
        let gaps = root.gaps_since(lost);
        assert!(the_gaps_are(&gaps, &[1, 2, 4, 5, 5]), "{gaps:?}");

        stop.send(true).unwrap();
        telling.await.unwrap();
        until(
            "gone with the per-user half",
            Duration::from_secs(1),
            || root.book.current().is_none(),
        )
        .await;
    }

    /// #3967 — an older root half (0.4.223, 0.4.224) answers "unknown variant"
    /// and keeps the connection open, so the per-user half holds it and does
    /// not dial again, however long it stands; the older root half logs
    /// nothing for it. Once that root half is replaced (the connection ends
    /// after a long hold), the next dial comes 1 s later and tells it. A root
    /// half that refuses and closes at once is dialled again on the backoff,
    /// 1 s later, and after a second refusal in a row once a minute. One that
    /// never answers is let go after 5 s and dialled again.
    ///
    /// Red before #3967: a per-user half told its root half nothing. And
    /// before the update's order was read: the first refusal that closed
    /// waited a minute too (gaps of 0, 60 and 60 s).
    #[tokio::test(start_paused = true)]
    async fn a_root_half_that_refuses_or_never_answers_is_not_dialled_in_a_loop() {
        use AtTheDial::*;
        let root = StoodRoot::new(&[Older]);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let telling = tokio::spawn(keep_telling(root.dialler(), shutdown));
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(root.dialled(), 1, "an older root half is dialled once");
        assert_eq!(root.book.current(), None, "and takes nothing");
        root.plan(&[Takes]);
        let replaced = tokio::time::Instant::now();
        root.stops_serving().await;
        until(
            "told the root half that replaced it",
            Duration::from_secs(5),
            || root.book.current() == Some(501),
        )
        .await;
        let gaps = root.gaps_since(replaced);
        assert!(the_gaps_are(&gaps, &[1]), "{gaps:?}");
        stop.send(true).unwrap();
        telling.await.unwrap();

        let root = StoodRoot::new(&[RefusesAndCloses, RefusesAndCloses, Takes]);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let started = tokio::time::Instant::now();
        let telling = tokio::spawn(keep_telling(root.dialler(), shutdown));
        until("told after two refusals", Duration::from_secs(180), || {
            root.book.current() == Some(501)
        })
        .await;
        let gaps = root.gaps_since(started);
        assert!(
            the_gaps_are(&gaps, &[0, 1, ANNOUNCE_AFTER_A_REFUSAL.as_secs()]),
            "{gaps:?}"
        );
        stop.send(true).unwrap();
        telling.await.unwrap();

        let root = StoodRoot::new(&[Silent, Takes]);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let started = tokio::time::Instant::now();
        let telling = tokio::spawn(keep_telling(root.dialler(), shutdown));
        until("told after a silent one", Duration::from_secs(30), || {
            root.book.current() == Some(501)
        })
        .await;
        let gaps = root.gaps_since(started);
        assert!(
            the_gaps_are(&gaps, &[0, ANNOUNCE_ANSWER_WITHIN.as_secs() + 1]),
            "{gaps:?}"
        );
        stop.send(true).unwrap();
        telling.await.unwrap();
    }

    /// #3967 — the update's order. The installer restarts a two-row Mac's
    /// per-user half about 3 s before its root half, so the new per-user half
    /// meets the old root half, which refuses ("unknown variant") and holds
    /// the connection until it stops. The next dial comes on the backoff,
    /// 1 s later, and tells the root half that replaced it. Where that root
    /// half is not up yet, the dials go on 1, 2 and 4 s apart until it is:
    /// the refusal is still on the books, but only a second refusal in a row
    /// waits a minute, and a take is no refusal.
    ///
    /// Red before: a refusal whose connection ended within [`HELD_STEADY`]
    /// waited [`ANNOUNCE_AFTER_A_REFUSAL`], a minute, so the root half's
    /// questions in that minute got the floor, once per Mac per update.
    #[tokio::test(start_paused = true)]
    async fn a_root_half_that_refused_and_then_stopped_is_dialled_again_on_the_backoff() {
        use AtTheDial::*;
        // The root half that replaced it is up at the next dial.
        let root = StoodRoot::new(&[Older]);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let telling = tokio::spawn(keep_telling(root.dialler(), shutdown));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(root.dialled(), 1, "the older root half is dialled once");
        assert_eq!(root.book.current(), None, "and takes nothing");
        root.plan(&[Takes]);
        let replaced = tokio::time::Instant::now();
        root.stops_serving().await;
        until(
            "told the root half that replaced it",
            ANNOUNCE_AGAIN_MOST,
            || root.book.current() == Some(501),
        )
        .await;
        let gaps = root.gaps_since(replaced);
        assert!(the_gaps_are(&gaps, &[1]), "{gaps:?}");
        stop.send(true).unwrap();
        telling.await.unwrap();

        // It is not up for the next two dials.
        let root = StoodRoot::new(&[Older]);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let telling = tokio::spawn(keep_telling(root.dialler(), shutdown));
        tokio::time::sleep(Duration::from_secs(3)).await;
        root.plan(&[Gone, Gone, Takes]);
        let replaced = tokio::time::Instant::now();
        root.stops_serving().await;
        until(
            "told the root half once it is up",
            Duration::from_secs(30),
            || root.book.current() == Some(501),
        )
        .await;
        let gaps = root.gaps_since(replaced);
        assert!(the_gaps_are(&gaps, &[1, 2, 4]), "{gaps:?}");
        stop.send(true).unwrap();
        telling.await.unwrap();
    }
}
