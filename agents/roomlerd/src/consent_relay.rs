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
//! | the worker (`run --supervised --unenrolled`) | lists its own questions, then the root half's, asked over the root's person socket ([`listed`]); sends an answer to a question its own broker is not asking there ([`decided`]); says [`RELAY_CONSENT`] in its caps frame |
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
/// that refused it and then closed the connection at once. An older root half
/// keeps the connection open (0.4.223 and 0.4.224 answer "unknown variant"
/// and read on), and that connection is held, so it is never dialled again
/// until it ends.
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
/// twice as long as the last, up to [`ANNOUNCE_AGAIN_MOST`]; and a root half
/// that refused and closed the connection soon after waits
/// [`ANNOUNCE_AFTER_A_REFUSAL`]. Each change of what the root half was told is
/// logged once, never a retry.
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
                wait = ANNOUNCE_AGAIN_FIRST;
                doubled(&mut wait)
            }
            Told::Refused(_) => ANNOUNCE_AFTER_A_REFUSAL,
            Told::Held(_) | Told::Unreachable | Told::NoAnswer => doubled(&mut wait),
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
/// root serves it, as the relay asks it (`person_socket::machine_answer`).
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
            reach_two_rows(self.uid, |_| async move {
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
    /// half that refuses and closes at once is dialled once a minute, and
    /// one that never answers is let go after 5 s and dialled again.
    ///
    /// Red before #3967: a per-user half told its root half nothing.
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
        assert!(the_gaps_are(&gaps, &[0, 60, 60]), "{gaps:?}");
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
}
