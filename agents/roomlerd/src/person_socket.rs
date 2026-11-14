// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-27 P11 — the person-at-the-device socket of a Linux SYSTEM install;
//! FR-97 P2g — and of a Mac's ROOT half.
//!
//! A root daemon's LocalAPI (`/var/run/roomler`, 0700 / 0600) is root-only,
//! and rightly: it is the trust boundary for every administrative verb. But
//! the person at the screen must still see who is viewing it, end that, and
//! answer a consent prompt, and their companion runs as THEM. Before this, on
//! a system install it read "Service offline" and there was no banner, no
//! Disconnect and — on GNOME/KDE Wayland, which have no native overlay — no
//! consent prompt at all (#1911, measured 2026-10-09).
//!
//! This module keeps ONE [`localapi::serve_person_socket`] listening for
//! whoever is at the seat, and moves it when that changes:
//! - on Linux, "at the seat" is the person signed in at seat0 — logind's
//!   `ActiveSession` there, when it is a person's (`companion::seat0_uid`) —
//!   never a display manager's greeter, never a user switched away in the
//!   background (they must not answer consent for a view of someone else's
//!   desktop), and never a remote login, which sits on no seat (#2871's
//!   review: the first active graphical session anywhere, which this took
//!   before, could be an xrdp user's);
//! - on a Mac, it is the console user, `/dev/console`'s owner, as the FR-43
//!   supervisor reads it;
//! - root at the seat needs nothing: root reaches the daemon's own socket;
//! - a per-user daemon (not root) does nothing: its own socket already IS its
//!   person's.
//!
//! What the socket serves is decided in `localapi` ([`localapi::person_may`]).
//!
//! **Not known is not nobody (#3845).** On Linux who is at the seat is
//! logind's answer, from another process, which may not come: a paused
//! logind, and `loginctl`'s D-Bus call gives up after 25 s. A look that
//! fails or runs out of time ([`Look::NotKnown`]) keeps the socket where it
//! is, with no close, start or move until the seat answers, and the poll says
//! so once a run. Only the seat's own answer is nobody ([`look_of`]).
//! Failing closed stays: every change and every consent answer over the
//! socket asks the seat again first ([`at_the_seat_now`]), and while it is
//! not known the person is told to try again.
//!
//! **A Mac (FR-97 P2g).** A two-half Mac's companion talks to its per-user
//! half, whose enrollment has no overlay node: only root can hold a utun, so
//! the machine is on the private network through its root half alone. The
//! server lists a device with no node by itself (`no_node`), so the companion's
//! Devices page showed the Mac and nothing else. The per-user half now asks
//! the root half, over this socket, for the machine's own view
//! ([`machine_view_wanted`], [`machine_answer`]), and shows that instead.
//!
//! **And the live view (FR-97 P2h).** The same half's `Status` said "(no
//! overlay IP)" and its `Peers` were empty, so the Devices page's Connection
//! column and the Overview's network lines were blank on a Mac. With no overlay
//! address of its own ([`network_view_wanted`]), it now answers those from the
//! root half too: the root half's peers as the person socket serves them,
//! without endpoints, and its own status with the root half's network fields
//! ([`with_machine_network`]).
//!
//! **And one row (FR-43 P3a).** A one-row Mac's GUI worker holds no enrollment
//! at all, so its identity and its link to the server come from the root half
//! the same way ([`with_machine_identity`]), and `Devices` and `Mesh` go there
//! directly ([`machine_view`]): the person's own view is the machine's.
//!
//! Kill switch: `ROOMLERD_PERSON_SOCKET=0` (or `false` / `off` / `no`) — the
//! pre-P11 behaviour, and on a Mac the pre-P2g one, since with no socket the
//! per-user half keeps its own answer.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::sync::watch;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tunnel_core::localapi::{self, LocalApiState};
use tunnel_core::localapi::{NodeStatus, Response};

/// How often the seat is looked at. A consent prompt stays up for 30 s or
/// more, so following a user switch within a few seconds is plenty. A look is
/// a couple of `loginctl` calls, and none at all where nobody but root is
/// logged in ([`any_user_runtime_dir`]); on a Mac it is one `stat`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const POLL: Duration = Duration::from_secs(5);

/// #3845 — how long the poll waits for a look at the seat: the time between
/// looks. A look takes tens of milliseconds, longer on a loaded host, and a
/// look cut short is not known, which leaves the socket where it is. So the
/// poll waits longer than the check a person waits on
/// (`files::SEAT_LOOKUP_WAIT`, 1.5 s), and it still follows a slow logind.
/// The lookup runs on the flight's own thread ([`look_at_the_seat`]), so
/// the wait holds no async worker. A hung logind holds that one thread,
/// however often the poll and the checks ask.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const LOOK_WAIT: Duration = POLL;

/// Is any NON-root user logged in? logind gives every logged-in user a
/// `/run/user/<uid>`, so its absence is a free answer of "nobody to serve".
pub fn any_user_runtime_dir(root: &std::path::Path) -> bool {
    std::fs::read_dir(root)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|uid| uid != 0)
            })
        })
        .unwrap_or(false)
}

/// After a listener could not be served (no runtime dir yet, or a per-user
/// daemon owns the path), the same person is retried this long after, rather
/// than on every tick.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The kill switch. Anything but an explicit off leaves it on.
pub fn enabled_by_env(v: Option<&str>) -> bool {
    !matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// What to do with the listener, given who it serves and who is at the seat.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Keep,
    Stop,
    Start(u32),
    Move(u32),
}

/// #3845 — what a look at the seat found ([`person_at_the_seat`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Look {
    /// The person signed in at the seat, never root.
    Person(u32),
    /// Nobody to serve, as the seat itself answered: nobody is signed in at
    /// it, or root is, who reaches the daemon's own socket. Also where no
    /// person is logged in at all.
    Nobody,
    /// Not known, and why: the lookup failed, or gave no answer in time.
    NotKnown(String),
}

/// Pure: the listener follows the person at the seat. Root at the seat is
/// nobody to serve here. #3845 — a look that could not tell keeps what is
/// served, a person's socket or none: not known is not nobody. The check
/// before each change and each consent answer still refuses while it is not
/// known ([`at_the_seat_now`]).
pub fn step(serving: Option<u32>, look: &Look) -> Step {
    let want = match look {
        Look::Person(uid) => Some(*uid).filter(|&u| u != 0),
        Look::Nobody => None,
        Look::NotKnown(_) => return Step::Keep,
    };
    match (serving, want) {
        (a, b) if a == b => Step::Keep,
        (None, Some(u)) => Step::Start(u),
        (Some(_), None) => Step::Stop,
        (Some(_), Some(u)) => Step::Move(u),
        (None, None) => Step::Keep,
    }
}

/// FR-97 P2g — should a Mac's per-user half ask the machine's root half for
/// this answer? Only when its own answer is a Devices or Mesh view the server
/// cut down to itself because it has no overlay node (`no_node`), and only
/// when it is NOT root: a root daemon is the machine's own view already.
pub fn machine_view_wanted(own: &Response, euid: u32) -> bool {
    euid != 0 && overlay_reason(own) == Some("no_node")
}

/// FR-97 P2g — is the root half's answer worth showing instead? Only a view
/// the server shaped for a node it has (`ok`). Anything else — the root half
/// is off the network too, its server has no network module, an error —
/// tells the person nothing better than the per-user half's own answer did.
pub fn machine_view_usable(machine: &Response) -> bool {
    overlay_reason(machine) == Some("ok")
}

/// The envelope's `overlay` reason of a Devices or Mesh answer.
fn overlay_reason(r: &Response) -> Option<&str> {
    match r {
        Response::Devices(p) => Some(p.overlay.as_str()),
        Response::Mesh(m) => Some(m.overlay.as_str()),
        _ => None,
    }
}

/// FR-97 P2g — on a Mac, `own` replaced by the root half's answer to `req`
/// where [`machine_view_wanted`] and [`machine_view_usable`] say so; `own`
/// itself in every other case, on every other OS, and whenever the root half
/// does not answer (an older one, `ROOMLERD_PERSON_SOCKET=0`, no root half).
pub async fn or_machine_view(own: Response, req: tunnel_core::localapi::Request) -> Response {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: geteuid never fails.
        let euid = unsafe { libc::geteuid() };
        if machine_view_wanted(&own, euid)
            && let Some(machine) = machine_answer(euid, &req, SERVER_ANSWER).await
            && machine_view_usable(&machine)
        {
            tracing::debug!("devices: the per-user half shows the machine's view (FR-97 P2g)");
            return machine;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = req;
    own
}

/// FR-43 P3a — the answer a one-row Mac's GUI worker gives to `req`: the root
/// half's, whatever it says, because this process has no server of its own to
/// ask. A root half that does not answer is a server out of reach from here.
pub async fn machine_view(req: tunnel_core::localapi::Request) -> Response {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: geteuid never fails.
        let euid = unsafe { libc::geteuid() };
        if let Some(machine) = machine_answer(euid, &req, SERVER_ANSWER).await {
            return machine;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = req;
    Response::Upstream {
        code: "server_unreachable".into(),
        message: "the root daemon, which holds this Mac's connection to the server, did not \
                  answer"
            .into(),
        status: None,
    }
}

/// How long the root half has to answer a question it asks its server first
/// (Devices, Mesh: one HTTP round trip, cached).
#[cfg(target_os = "macos")]
const SERVER_ANSWER: Duration = Duration::from_secs(5);

/// …and one it answers from memory (Status, Peers), which the companion asks
/// every 2 s: a root half that is slow here is stuck, and the person's own
/// answer is the better one to show meanwhile.
#[cfg(target_os = "macos")]
const LIVE_ANSWER: Duration = Duration::from_millis(1500);

/// FR-97 P2g — ask the root half's person socket for `uid`. `None` when it is
/// not there, is served by anyone but root, or does not answer within `within`:
/// the caller keeps its own answer then.
#[cfg(target_os = "macos")]
pub async fn machine_answer(
    uid: u32,
    req: &tunnel_core::localapi::Request,
    within: Duration,
) -> Option<Response> {
    let path = localapi::person_socket_path(uid);
    if !path.exists() {
        return None;
    }
    let ask = async {
        // Only a socket ROOT serves is the machine's half.
        let mut client = localapi::connect_unix_served_by_root(path).await.ok()?;
        client.request(req).await.ok()
    };
    tokio::time::timeout(within, ask).await.ok().flatten()
}

/// FR-97 P2h — should a Mac's per-user half take the machine's live network
/// view (its `Status` network fields and its `Peers`) from the root half? When
/// it is not root and holds no overlay address of its own: it runs no node
/// then, so its own answer can only say "not on the private network".
pub fn network_view_wanted(own_overlay_ip: Option<&str>, euid: u32) -> bool {
    euid != 0 && own_overlay_ip.is_none()
}

/// FR-97 P2h — `own`, with the root half's NETWORK fields: its overlay
/// addresses, its exit status and its MagicDNS. Everything else stays the
/// per-user half's: its name, version, mode, and its own link to the server.
/// A machine view that holds no overlay address lends nothing.
///
/// #2892 — and the answer SAYS whose network it carries (`network_from`):
/// without that, `roomler status` as the console user printed the root half's
/// address as this half's, right above this half's own "overlay OFF"
/// enrollments, and the companion's Overview showed it as the device's own.
///
/// FR-128 D8 — and the machine's netcheck: this half runs no overlay, so it
/// measures nothing, and an `auto` session it builds reads the far-TURN reach
/// of its own machine from here (`rc_overlay::own_far_turn`).
pub fn with_machine_network(mut own: NodeStatus, machine: &NodeStatus) -> NodeStatus {
    if machine.overlay_ip.is_some() {
        own.overlay_ip = machine.overlay_ip.clone();
        own.overlay_ip6 = machine.overlay_ip6.clone();
        own.exit_node = machine.exit_node.clone();
        own.dns = machine.dns.clone();
        own.netcheck = machine.netcheck.clone();
        own.network_from = Some(tunnel_core::localapi::NetworkOrigin {
            node_id: machine.node_id.clone(),
            name: machine.name.clone(),
        });
    }
    own
}

/// FR-43 P3a — the status a one-row Mac's GUI worker answers: the device's, which
/// the root half holds. Its name, its row, its orgs and its link to the server
/// replace this process's blanks, and its version, mode, account and config path
/// stay this process's own. There is one row, so nothing here is lent:
/// `network_from` goes, and the addresses read as the device's.
///
/// With no answer from the root half, `own` says so: the stand-in always reads
/// connected and is no server, and an `orgs` row naming it must not reach the
/// companion, which opens a row's server in a browser.
pub fn with_machine_identity(mut own: NodeStatus, machine: Option<&NodeStatus>) -> NodeStatus {
    match machine {
        Some(machine) => {
            own.node_id = machine.node_id.clone();
            own.name = machine.name.clone();
            own.tenant_id = machine.tenant_id.clone();
            own.connected = machine.connected;
            own.ephemeral = machine.ephemeral;
            own.orgs = machine.orgs.clone();
            own.network_from = None;
        }
        None => {
            own.connected = false;
            own.orgs = Vec::new();
        }
    }
    own
}

/// FR-97 P2h — the status a Mac's per-user half answers: its own, with the
/// root half's network fields where [`network_view_wanted`] says so.
///
/// FR-43 P3a — and with `no_enrollment`, a flag and not a blank field, its
/// identity and its link too ([`with_machine_identity`]).
#[cfg(target_os = "macos")]
pub async fn or_machine_status(own: NodeStatus, no_enrollment: bool) -> NodeStatus {
    // SAFETY: geteuid never fails.
    let euid = unsafe { libc::geteuid() };
    let network_wanted = network_view_wanted(own.overlay_ip.as_deref(), euid);
    let mut machine = None;
    if network_wanted || no_enrollment {
        machine = match machine_answer(euid, &tunnel_core::localapi::Request::Status, LIVE_ANSWER)
            .await
        {
            Some(Response::Status(machine)) => Some(*machine),
            _ => None,
        };
    }
    let own = match &machine {
        Some(machine) if network_wanted => with_machine_network(own, machine),
        _ => own,
    };
    if no_enrollment {
        with_machine_identity(own, machine.as_ref())
    } else {
        own
    }
}

/// FR-97 P2h — the peers a Mac's per-user half answers: the root half's where
/// [`network_view_wanted`] says so (its own list is empty then). They arrive
/// as the person socket serves them, without endpoints (`localapi::person_view`).
#[cfg(target_os = "macos")]
pub async fn or_machine_peers(
    own: Vec<tunnel_core::localapi::PeerInfo>,
    own_overlay_ip: Option<&str>,
) -> Vec<tunnel_core::localapi::PeerInfo> {
    // SAFETY: geteuid never fails.
    let euid = unsafe { libc::geteuid() };
    if network_view_wanted(own_overlay_ip, euid)
        && let Some(Response::Peers(machine)) =
            machine_answer(euid, &tunnel_core::localapi::Request::Peers, LIVE_ANSWER).await
    {
        return machine;
    }
    own
}

/// Who is at the seat now: on Linux the person signed in at seat0 (logind's
/// `ActiveSession` there, when it is a person's: `companion::seat0_uid`, the
/// rule received files and `console_user` already use), on a Mac the console
/// user. Nobody for nobody, and for root. Blocking: on Linux, logind.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn person_at_the_seat() -> Look {
    #[cfg(target_os = "linux")]
    {
        // This runs on EVERY Linux root daemon, servers and cluster nodes
        // included. Where no non-root user is logged in at all there is
        // nobody to serve, and no `loginctl` to spawn.
        if !any_user_runtime_dir(std::path::Path::new("/run/user")) {
            return Look::Nobody;
        }
        // #2871 (review) — seat0's person, never the first active graphical
        // session anywhere: logind calls every seatless session active, so on
        // a host with remote logins (xrdp, GNOME Remote Login) that was
        // whoever logind listed first, and since design A this socket sets
        // the person's own config keys.
        look_of(crate::companion::seat0_uid())
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        // `/dev/console`'s owner, as the FR-43 supervisor reads it: uid 0 is
        // the login window, which is nobody to serve.
        match std::fs::metadata("/dev/console")
            .ok()
            .map(|m| m.uid())
            .filter(|&u| u != 0)
        {
            Some(uid) => Look::Person(uid),
            None => Look::Nobody,
        }
    }
}

/// #3845 — the pure half of [`person_at_the_seat`] on Linux: what logind
/// said of seat0 (`companion::seat0_uid`). Nobody only when the seat says so
/// (an empty seat, the login screen, a session that is not a person's, a host
/// with no logind and so no seat: `companion::is_nobody_at_the_screen`, the
/// rule a received file follows too), and when root is there. Any other
/// failure (logind or the bus did not answer, `loginctl`'s D-Bus call timed
/// out) cannot tell who is there.
#[cfg(any(target_os = "linux", test))]
fn look_of(found: anyhow::Result<u32>) -> Look {
    match found {
        Ok(0) => Look::Nobody,
        Ok(uid) => Look::Person(uid),
        Err(e) if crate::companion::is_nobody_at_the_screen(&e) => Look::Nobody,
        Err(e) => Look::NotKnown(format!("{e:#}")),
    }
}

/// #2871 (review) — is `uid` the person at the seat RIGHT NOW? The person
/// socket asks before it applies a config change, and since #3845 before it
/// takes a consent answer (`LocalApiState::at_the_seat_now`): its listener
/// follows the seat on a [`POLL`], so a person who switched away still holds
/// an open connection until the next look, and must neither change settings
/// that are the next person's by then nor answer for their screen.
///
/// #3796 — on Linux the lookup is logind's, another process, which may not
/// answer (a stopped logind). It is asked as a received file's is: one
/// lookup in flight ([`PERSON_AT_SEAT0`]), each check waiting
/// `files::SEAT_LOOKUP_WAIT` at most. With no answer in that time, who is
/// at the seat is not known yet, and the change is refused with the
/// try-again text (`AtTheSeat::NotKnownYet`). #3845 — and so is every other
/// look that cannot tell ([`seat_for`]). A Mac looks at `/dev/console`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn at_the_seat_now(uid: u32) -> tunnel_core::localapi::AtTheSeat {
    #[cfg(target_os = "linux")]
    let wait = crate::files::SEAT_LOOKUP_WAIT;
    #[cfg(target_os = "macos")]
    let wait = LOOK_WAIT;
    seat_for(uid, look_at_the_seat(wait).await)
}

/// #3796 — the one lookup of who is at seat0 in flight for the person
/// socket, on a thread named `seat0-person`. #3845 — the checks
/// ([`at_the_seat_now`]) and the 5 s poll ([`follow_the_seat`]) both ask
/// through it, so a hung logind holds one thread for both.
#[cfg(target_os = "linux")]
static PERSON_AT_SEAT0: crate::one_in_flight::OneInFlight<Look> =
    crate::one_in_flight::OneInFlight::new("seat0-person");

/// #3845 — a look at the seat, `wait` at most, through the one lookup in
/// flight ([`PERSON_AT_SEAT0`]). No answer in time, or a lookup that
/// panicked, is not known.
#[cfg(target_os = "linux")]
async fn look_at_the_seat(wait: Duration) -> Look {
    use crate::one_in_flight::Asked;
    // Asked here, on the thread a test stood the lookup in on.
    #[cfg(test)]
    let asked = match stand_in::lookup_now() {
        Some(stood) => {
            let lookup = stood.lookup;
            stood.flight.ask(wait, move || lookup()).await
        }
        None => PERSON_AT_SEAT0.ask(wait, person_at_the_seat).await,
    };
    #[cfg(not(test))]
    let asked = PERSON_AT_SEAT0.ask(wait, person_at_the_seat).await;
    match asked {
        Asked::Answer(look) => look,
        Asked::Failed(why) => Look::NotKnown(format!("the lookup failed: {why}")),
        Asked::OutOfTime => Look::NotKnown(format!("no answer within {wait:?}")),
    }
}

/// A Mac's look ([`look_at_the_seat`]): `/dev/console`'s owner, one `stat`,
/// on a blocking thread.
#[cfg(target_os = "macos")]
async fn look_at_the_seat(_wait: Duration) -> Look {
    tokio::task::spawn_blocking(person_at_the_seat)
        .await
        .unwrap_or(Look::Nobody)
}

/// #3796 — what a check answers for `uid` from a look at the seat. #3845 —
/// a look that could not tell, however that came about (no answer in time,
/// `loginctl` failed, the lookup panicked), is not known: a retry asks
/// again, never "the seat changed".
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn seat_for(uid: u32, look: Look) -> tunnel_core::localapi::AtTheSeat {
    use tunnel_core::localapi::AtTheSeat;
    match look {
        Look::Person(at_seat) if at_seat == uid => AtTheSeat::Yes,
        Look::Person(_) | Look::Nobody => AtTheSeat::No,
        Look::NotKnown(why) => {
            tracing::warn!(
                uid,
                "person socket: who is at the seat is not known ({}): a change or a consent \
                 answer is refused, try again",
                why.escape_debug()
            );
            AtTheSeat::NotKnownYet
        }
    }
}

/// #3796 — who is at the seat, as the person socket's check asks it, stood in
/// on one thread: the lookup itself runs where the real one runs, in a flight
/// of the stand-in's own, so a test sees how long a `ConfigSet` waits for it,
/// and how many lookups began. #3845 — and as its poll asks it.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod stand_in {
    use super::Look;
    use crate::one_in_flight::OneInFlight;
    use std::sync::{Arc, Mutex, PoisonError};

    /// The lookup stood in: what a look at the seat finds.
    type Asked = Arc<dyn Fn() -> Look + Send + Sync>;

    /// The lookup stood in on a thread, with its own lookup in flight.
    pub(super) struct StoodLookup {
        pub(super) lookup: Asked,
        pub(super) flight: Arc<OneInFlight<Look>>,
    }

    type Stands = (std::thread::ThreadId, Asked, Arc<OneInFlight<Look>>);

    static ASKED: Mutex<Option<Stands>> = Mutex::new(None);

    /// The tests that stand the lookup in take turns.
    static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// While this lives, the person socket's check asks the lookup stood in,
    /// on the thread that stood it in.
    pub(crate) struct Stood {
        _turn: tokio::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Stood {
        fn drop(&mut self) {
            *ASKED.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    /// Stand `lookup` in for who is at the seat, on this thread only.
    pub(crate) async fn person_at_the_seat(
        lookup: impl Fn() -> Look + Send + Sync + 'static,
    ) -> Stood {
        let turn = TURN.lock().await;
        *ASKED.lock().unwrap_or_else(PoisonError::into_inner) = Some((
            std::thread::current().id(),
            Arc::new(lookup),
            Arc::new(OneInFlight::new("seat0-person")),
        ));
        Stood { _turn: turn }
    }

    /// The lookup stood in for this thread, if there is one.
    pub(super) fn lookup_now() -> Option<StoodLookup> {
        ASKED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(thread, _, _)| *thread == std::thread::current().id())
            .map(|(_, lookup, flight)| StoodLookup {
                lookup: lookup.clone(),
                flight: flight.clone(),
            })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Listener {
    uid: u32,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Listener {
    fn start(uid: u32, path: std::path::PathBuf, state: Arc<dyn LocalApiState>) -> Self {
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            if let Err(e) = localapi::serve_person_socket(path.clone(), uid, state, rx).await {
                tracing::warn!(
                    path = %path.display(), uid, error = %e,
                    "person socket: not served (FR-27 P11); retrying later"
                );
            }
        });
        Self { uid, stop, task }
    }

    async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

/// Run until `shutdown`. Inert unless this is a root daemon with the switch on.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn run(state: Arc<dyn LocalApiState>, shutdown: watch::Receiver<bool>) {
    if !enabled_by_env(std::env::var("ROOMLERD_PERSON_SOCKET").ok().as_deref()) {
        tracing::info!("person socket: off (ROOMLERD_PERSON_SOCKET) — FR-27 P11");
        return;
    }
    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    follow_the_seat(state, shutdown, Following::the_daemons()).await
}

/// #3845 — how [`follow_the_seat`] follows the seat: the daemon's, or a
/// test's, which looks more often and makes its sockets in a folder of its
/// own.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Following {
    /// The time between looks ([`POLL`]).
    every: Duration,
    /// How long a look waits for its answer ([`LOOK_WAIT`]).
    wait: Duration,
    /// Where the socket for a uid is made (`localapi::person_socket_path`).
    path: Box<dyn Fn(u32) -> std::path::PathBuf + Send + Sync>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Following {
    fn the_daemons() -> Self {
        Self {
            every: POLL,
            wait: LOOK_WAIT,
            path: Box::new(localapi::person_socket_path),
        }
    }
}

/// Serves the person at the seat a socket of their own, and moves it as the
/// seat changes, until `shutdown`.
///
/// #3845 — each look is [`look_at_the_seat`], [`LOOK_WAIT`] at most, through
/// the lookup in flight the checks share. It used to wait on a blocking
/// thread for as long as `loginctl` took, and read a look that failed as
/// nobody, which closed the person's socket while logind hung. A look that
/// cannot tell now keeps what is served ([`step`]). A run of them is said
/// once as it begins and once as it ends, never on every look.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn follow_the_seat(
    state: Arc<dyn LocalApiState>,
    mut shutdown: watch::Receiver<bool>,
    following: Following,
) {
    let mut current: Option<Listener> = None;
    let mut failed: Option<(u32, Instant)> = None;
    // #3845 — when the run of looks that could not tell began, while one runs.
    let mut not_known_since: Option<Instant> = None;
    loop {
        // A listener that ended on its own was not served: note it, and let
        // the step below see "nobody served" so it retries after the pause.
        if let Some(l) = current.take_if(|l| l.task.is_finished()) {
            failed = Some((l.uid, Instant::now()));
        }
        let look = look_at_the_seat(following.wait).await;
        match (&look, not_known_since) {
            (Look::NotKnown(why), None) => {
                tracing::warn!(
                    serving = ?current.as_ref().map(|l| l.uid),
                    "person socket: who is at the seat is not known ({}); serving as before",
                    why.escape_debug()
                );
                not_known_since = Some(Instant::now());
            }
            (Look::NotKnown(_), Some(_)) => {}
            (Look::Person(_) | Look::Nobody, Some(since)) => {
                tracing::info!(
                    after = ?since.elapsed(),
                    "person socket: who is at the seat is known again"
                );
                not_known_since = None;
            }
            (Look::Person(_) | Look::Nobody, None) => {}
        }
        match step(current.as_ref().map(|l| l.uid), &look) {
            Step::Keep => {}
            Step::Stop => {
                if let Some(l) = current.take() {
                    tracing::info!(
                        uid = l.uid,
                        "person socket: nobody at the seat now; closing"
                    );
                    l.stop().await;
                }
            }
            Step::Start(uid) | Step::Move(uid) => {
                // The previous person's socket goes at once, whatever happens
                // to the new one: a person switched away must not keep it
                // while the new one waits out a retry.
                if let Some(l) = current.take() {
                    tracing::info!(
                        from = l.uid,
                        to = uid,
                        "person socket: the person at the seat changed"
                    );
                    l.stop().await;
                }
                let too_soon = failed.is_some_and(|(u, at)| u == uid && at.elapsed() < RETRY_AFTER);
                if !too_soon {
                    current = Some(Listener::start(uid, (following.path)(uid), state.clone()));
                    failed = None;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(following.every) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    if let Some(l) = current.take() {
        l.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_core::localapi::{DevicesPage, MeshView};

    #[test]
    fn the_switch_is_on_unless_turned_off() {
        assert!(enabled_by_env(None));
        assert!(enabled_by_env(Some("1")));
        assert!(enabled_by_env(Some("")));
        for off in ["0", "false", "OFF", " no "] {
            assert!(!enabled_by_env(Some(off)), "{off:?} turns it off");
        }
    }

    /// Only a NON-root uid's runtime dir means someone could be at the seat;
    /// an unreadable root means nobody (and no `loginctl`).
    #[test]
    fn only_a_non_root_runtime_dir_counts_as_someone_logged_in() {
        let base = std::env::temp_dir().join(format!("roomler-p11-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("0")).unwrap();
        std::fs::create_dir_all(base.join("not-a-uid")).unwrap();
        assert!(
            !any_user_runtime_dir(&base),
            "root alone is nobody to serve"
        );
        std::fs::create_dir_all(base.join("1000")).unwrap();
        assert!(any_user_runtime_dir(&base));
        assert!(!any_user_runtime_dir(&base.join("missing")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The listener follows the person at the seat; root and nobody get none.
    #[test]
    fn the_socket_follows_the_person_at_the_seat() {
        use Look::{Nobody, Person};
        assert_eq!(step(None, &Nobody), Step::Keep);
        assert_eq!(step(None, &Person(1000)), Step::Start(1000));
        assert_eq!(step(Some(1000), &Person(1000)), Step::Keep);
        assert_eq!(step(Some(1000), &Person(1001)), Step::Move(1001));
        assert_eq!(step(Some(1000), &Nobody), Step::Stop);
        // Root at the seat reaches the daemon's own socket.
        assert_eq!(step(None, &Person(0)), Step::Keep);
        assert_eq!(step(Some(1000), &Person(0)), Step::Stop);
    }

    /// #3845 — a look that could not tell who is at the seat is not nobody:
    /// the socket stays where it is, served or not, until the seat answers.
    #[test]
    fn a_look_that_cannot_tell_keeps_what_is_served() {
        let not_known = Look::NotKnown(
            "loginctl show-seat seat0 failed: Failed to get properties: Connection timed out"
                .into(),
        );
        assert_eq!(step(Some(1000), &not_known), Step::Keep, "never a Stop");
        assert_eq!(step(None, &not_known), Step::Keep, "never a Start");
    }

    /// #3845 — what a look at seat0 makes of logind's answer: nobody only
    /// when the seat itself says so, or root is there; any other failure
    /// cannot tell who is at the seat.
    #[test]
    fn only_the_seats_own_answer_is_nobody() {
        use crate::companion::{loginctl_did_not_start, loginctl_failed, nobody_at_the_screen};
        use std::io::{Error, ErrorKind};
        let seat0 = ["show-seat", "seat0"];
        assert_eq!(look_of(Ok(1000)), Look::Person(1000));
        assert_eq!(
            look_of(Ok(0)),
            Look::Nobody,
            "root has the daemon's own socket"
        );
        // An empty seat, the login screen: the seat's own answer.
        let empty = nobody_at_the_screen("seat0 has no active session".into());
        assert_eq!(look_of(Err(empty)), Look::Nobody);
        // A host with no logind has no seat.
        let not_installed = loginctl_did_not_start(Error::from(ErrorKind::NotFound));
        assert_eq!(look_of(Err(not_installed)), Look::Nobody);
        let no_systemd = loginctl_failed(
            &seat0,
            "System has not been booted with systemd as init system (PID 1). Can't operate.",
        );
        assert_eq!(look_of(Err(no_systemd)), Look::Nobody);
        // logind or the bus did not answer: not known, and why.
        let timed_out = loginctl_failed(&seat0, "Failed to get properties: Connection timed out\n");
        assert_eq!(
            look_of(Err(timed_out)),
            Look::NotKnown(
                "loginctl show-seat seat0 failed: Failed to get properties: Connection timed out"
                    .into()
            )
        );
        let refused = loginctl_did_not_start(Error::from(ErrorKind::PermissionDenied));
        assert!(
            matches!(look_of(Err(refused)), Look::NotKnown(ref why) if why.contains("spawning loginctl")),
        );
    }

    fn devices(overlay: &str) -> Response {
        Response::Devices(Box::new(DevicesPage {
            overlay: overlay.into(),
            ..Default::default()
        }))
    }

    fn mesh(overlay: &str) -> Response {
        Response::Mesh(Box::new(MeshView {
            overlay: overlay.into(),
            ..Default::default()
        }))
    }

    /// FR-97 P2g — the per-user half asks the root half only for a view the
    /// server cut to itself for want of a node, and never as root.
    #[test]
    fn only_a_no_node_view_off_root_asks_the_machine() {
        assert!(machine_view_wanted(&devices("no_node"), 501));
        assert!(machine_view_wanted(&mesh("no_node"), 501));
        // A root daemon IS the machine's view.
        assert!(!machine_view_wanted(&devices("no_node"), 0));
        // A node, an org with no network, a server without the module: the
        // per-user half's own answer already says the true thing.
        for other in ["ok", "no_network", "unavailable", ""] {
            assert!(!machine_view_wanted(&devices(other), 501), "{other:?}");
        }
        assert!(!machine_view_wanted(
            &Response::Error {
                message: "x".into()
            },
            501
        ));
    }

    /// FR-97 P2g — and shows the root half's answer only when it is a real
    /// network view.
    #[test]
    fn only_a_shaped_machine_view_replaces_the_own_one() {
        assert!(machine_view_usable(&devices("ok")));
        assert!(machine_view_usable(&mesh("ok")));
        for other in ["no_node", "no_network", "unavailable", ""] {
            assert!(!machine_view_usable(&devices(other)), "{other:?}");
        }
        assert!(!machine_view_usable(&Response::Error {
            message: "x".into()
        }));
    }

    fn status(j: &str) -> NodeStatus {
        serde_json::from_str(j).unwrap()
    }

    /// FR-97 P2h — the per-user half takes the root half's network view only
    /// when it holds no overlay address of its own, and never as root.
    #[test]
    fn only_a_node_less_half_off_root_takes_the_machine_network() {
        assert!(network_view_wanted(None, 501));
        assert!(!network_view_wanted(Some("100.64.0.9"), 501));
        assert!(!network_view_wanted(None, 0));
    }

    /// FR-97 P2h — and borrows only the network fields, and only from a
    /// machine view that is on the network.
    #[test]
    fn only_the_network_fields_are_borrowed() {
        let own = status(
            r#"{"node_id":"u","name":"mac-u","version":"0.4.150","mode":"user","connected":false}"#,
        );
        let machine = status(
            r#"{"node_id":"d","name":"mac-d","version":"0.4.149","mode":"service","connected":true,
                "overlay_ip":"100.64.0.9","overlay_ip6":"fd7a:115c:a1e0::9",
                "dns":{"magic_domain":"vmtest.roomler.ai","resolver_bound":true,
                       "os_steer_active":true,"upstream":"192.0.2.53:53","answer_aaaa":true},
                "netcheck":{"stun_udp":false,"relay_band_udp":false,"derp_ws_ok":true,"age_s":90}}"#,
        );
        let merged = with_machine_network(own.clone(), &machine);
        assert_eq!(merged.overlay_ip.as_deref(), Some("100.64.0.9"));
        assert_eq!(merged.overlay_ip6.as_deref(), Some("fd7a:115c:a1e0::9"));
        assert!(merged.dns.is_some());
        assert_eq!(merged.dns, machine.dns);
        // FR-128 D8 — the machine's netcheck, which this half cannot measure.
        assert!(own.netcheck.is_none());
        assert_eq!(merged.netcheck, machine.netcheck);
        assert!(merged.netcheck.as_ref().is_some_and(|n| !n.stun_udp));
        // #2892 — and the answer names the daemon those fields belong to.
        assert_eq!(
            merged.network_from,
            Some(tunnel_core::localapi::NetworkOrigin {
                node_id: "d".into(),
                name: "mac-d".into(),
            })
        );
        // Its own identity and its own link to the server stay its own.
        assert_eq!(
            (
                merged.node_id.as_str(),
                merged.name.as_str(),
                merged.version.as_str()
            ),
            ("u", "mac-u", "0.4.150")
        );
        assert_eq!(merged.mode, own.mode);
        assert!(!merged.connected);
        // A machine view off the network lends nothing — and names nothing
        // (#2892): `own` carries no `network_from`, and neither does this.
        let off = status(
            r#"{"node_id":"d","name":"mac-d","version":"0.4.149","mode":"service","connected":true}"#,
        );
        assert_eq!(with_machine_network(own.clone(), &off), own);
        assert_eq!(own.network_from, None);
    }

    fn worker_status() -> NodeStatus {
        status(
            r#"{"node_id":"","name":"mac","version":"0.4.199","mode":"user","privileged":false,
                "tenant_id":"","connected":true,"config_path":"/Users/p/config.toml",
                "orgs":[{"label":"primary","server_url":"http://127.0.0.1:50123","primary":true,
                         "enabled":true,"connected":true}]}"#,
        )
    }

    fn root_status() -> NodeStatus {
        status(
            r#"{"node_id":"d","name":"MacBook","version":"0.4.198","mode":"service",
                "privileged":true,"tenant_id":"t1","connected":false,"ephemeral":true,
                "overlay_ip":"100.64.0.9",
                "orgs":[{"label":"primary","server_url":"https://roomler.ai","tenant_id":"t1",
                         "agent_id":"d","primary":true,"enabled":true,"connected":false}]}"#,
        )
    }

    /// FR-43 P3a — a worker with no enrollment shows the device: the root half's
    /// identity and link, whatever those say, and this process's own build and
    /// account. Nothing reads as lent, because there is only the one row.
    #[test]
    fn a_worker_with_no_enrollment_shows_the_roots_identity_and_link() {
        let own = worker_status();
        let machine = root_status();
        let merged =
            with_machine_identity(with_machine_network(own.clone(), &machine), Some(&machine));

        assert_eq!(merged.node_id, "d");
        assert_eq!(merged.name, "MacBook");
        assert_eq!(merged.tenant_id.as_deref(), Some("t1"));
        assert!(merged.ephemeral);
        assert_eq!(merged.orgs, machine.orgs);
        assert_eq!(merged.overlay_ip.as_deref(), Some("100.64.0.9"));
        assert_eq!(merged.network_from, None);
        // The stand-in reads connected, and the root half's link is the truth.
        assert!(own.connected);
        assert!(!merged.connected);
        // What this process really is stays its own.
        assert_eq!(merged.version, own.version);
        assert_eq!(merged.mode, own.mode);
        assert_eq!(merged.privileged, own.privileged);
        assert_eq!(merged.config_path, own.config_path);
    }

    /// FR-43 P3a — and with no root half it claims nothing: not the stand-in's
    /// `connected`, and no `orgs` row whose server is the stand-in's port.
    #[test]
    fn a_worker_with_no_root_half_claims_no_link_and_names_no_stand_in() {
        let merged = with_machine_identity(worker_status(), None);
        assert!(!merged.connected);
        assert!(merged.orgs.is_empty());
        let wire = serde_json::to_string(&merged).unwrap();
        assert!(!wire.contains("127.0.0.1"), "{wire}");
    }

    /// FR-43 P3a — off a Mac there is no root half: a server out of reach, by its
    /// named code, which a client can tell from "no devices".
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn the_machine_view_with_no_root_half_is_a_server_out_of_reach() {
        let answer =
            machine_view(tunnel_core::localapi::Request::Mesh { org: String::new() }).await;
        assert!(
            matches!(answer, Response::Upstream { ref code, .. } if code == "server_unreachable"),
            "{answer:?}"
        );
    }
}

/// #3845 — the poll and the person socket of a Linux system install, with
/// who is at the seat stood in: the real poll ([`follow_the_seat`]), the real
/// socket (`localapi::serve_person_socket`, made in a folder of the test's
/// own, for the account the test runs as) and the real check before a change
/// or a consent answer ([`at_the_seat_now`]). Run as an account that is not
/// root, since root is never asked whether it is at the seat: CI's agent lane
/// (`rust-agent`, "Test agent under vp9-444") runs them as the runner.
#[cfg(all(test, target_os = "linux"))]
mod following_the_seat_tests {
    use super::*;
    use crate::peer::files_log_tests::Logged;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{Condvar, Mutex, PoisonError};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tunnel_core::localapi::{AtTheSeat, CANNOT_TELL_WHO_IS_AT_THE_SCREEN, FlowInfo, PeerInfo};

    /// The time between the test poll's looks.
    const EVERY: Duration = Duration::from_millis(50);
    /// How long the test poll waits for a look.
    const WAIT: Duration = Duration::from_millis(150);
    /// Time for several looks, hung ones included.
    const SEVERAL_LOOKS: Duration = Duration::from_millis(1200);
    /// Long enough that only something stuck runs out of it.
    const LONG: Duration = Duration::from_secs(10);
    /// What `loginctl` says when logind did not answer its D-Bus call in time.
    const TIMED_OUT: &str =
        "loginctl show-seat seat0 failed: Failed to get properties: Connection timed out";

    const STATUS: &str = r#"{"t":"status"}"#;
    const SET: &str = r#"{"t":"config_set","d":{"key":"rc_clipboard","value":"on"}}"#;
    const ANSWER: &str =
        r#"{"t":"consent_decide","d":{"session_id":"38450000000000000000aaaa","allow":true}}"#;

    /// The account the test runs as, or `None` (and the test returns) as root.
    fn me() -> Option<u32> {
        // SAFETY: getuid and geteuid never fail.
        let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
        if uid == 0 || euid == 0 {
            eprintln!("skipped: run as an account that is not root, which the seat is asked about");
            return None;
        }
        Some(euid)
    }

    /// Who is at the seat, as a test stands it in: what each look finds, or
    /// nothing until the test says (a logind that does not answer). It counts
    /// the lookups that began, and the most that ran at once.
    #[derive(Clone)]
    struct TheSeat(Arc<SeatState>);

    struct SeatState {
        /// What a look finds; `None` holds it.
        finds: Mutex<Option<Look>>,
        changed: Condvar,
        began: AtomicUsize,
        running: AtomicUsize,
        most_at_once: AtomicUsize,
    }

    impl TheSeat {
        fn finding(look: Look) -> Self {
            Self(Arc::new(SeatState {
                finds: Mutex::new(Some(look)),
                changed: Condvar::new(),
                began: AtomicUsize::new(0),
                running: AtomicUsize::new(0),
                most_at_once: AtomicUsize::new(0),
            }))
        }

        /// From now on every look finds `look`, one that was held included.
        fn finds(&self, look: Look) {
            *self.0.finds.lock().unwrap_or_else(PoisonError::into_inner) = Some(look);
            self.0.changed.notify_all();
        }

        /// From now on every look waits, as on a logind that does not answer.
        fn hangs(&self) {
            *self.0.finds.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }

        /// The lookup the stand-in runs.
        fn lookup(&self) -> impl Fn() -> Look + Send + Sync + 'static {
            let seat = self.0.clone();
            move || {
                seat.began.fetch_add(1, SeqCst);
                let now = seat.running.fetch_add(1, SeqCst) + 1;
                seat.most_at_once.fetch_max(now, SeqCst);
                let finds = seat.finds.lock().unwrap_or_else(PoisonError::into_inner);
                let found = seat
                    .changed
                    .wait_while(finds, |finds| finds.is_none())
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .unwrap_or(Look::Nobody);
                seat.running.fetch_sub(1, SeqCst);
                found
            }
        }

        fn began(&self) -> usize {
            self.0.began.load(SeqCst)
        }

        fn most_at_once(&self) -> usize {
            self.0.most_at_once.load(SeqCst)
        }
    }

    /// Lets every held look go when a test ends, however it ends.
    struct LetGoAtTheEnd(TheSeat);

    impl Drop for LetGoAtTheEnd {
        fn drop(&mut self) {
            self.0.finds(Look::Nobody);
        }
    }

    /// The daemon's side of the socket here: the real check of who is at the
    /// seat, and a configuration and a consent prompt that count what
    /// reaches them.
    #[derive(Default)]
    struct Counted {
        sets: AtomicUsize,
        answers: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LocalApiState for Counted {
        fn status(&self) -> NodeStatus {
            serde_json::from_str(
                r#"{"node_id":"d","name":"seat","version":"0.4.0","mode":"service","connected":true}"#,
            )
            .unwrap()
        }
        fn peers(&self) -> Vec<PeerInfo> {
            Vec::new()
        }
        fn flows(&self) -> Vec<FlowInfo> {
            Vec::new()
        }
        async fn at_the_seat_now(&self, uid: u32) -> AtTheSeat {
            at_the_seat_now(uid).await
        }
        fn consent_decide(&self, _session_id: &str, _allow: bool) -> bool {
            self.answers.fetch_add(1, SeqCst);
            true
        }
        async fn config_set(&self, key: &str, value: Option<&str>) -> Response {
            self.sets.fetch_add(1, SeqCst);
            Response::ConfigUpdated {
                entry: serde_json::from_value(serde_json::json!({
                    "key": key, "value": value, "kind": "tribool",
                    "restart_required": false, "description": "",
                }))
                .unwrap(),
            }
        }
    }

    /// The poll, following the seat for `state` with its sockets in `dir`,
    /// a look each `every`. The sender stops it.
    fn follow(
        state: &Arc<Counted>,
        dir: &Path,
        every: Duration,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let (stop, shutdown) = watch::channel(false);
        let state: Arc<dyn LocalApiState> = state.clone();
        let dir = dir.to_path_buf();
        let following = Following {
            every,
            wait: WAIT,
            path: Box::new(move |uid| dir.join(uid.to_string()).join("roomler.sock")),
        };
        (
            stop,
            tokio::spawn(follow_the_seat(state, shutdown, following)),
        )
    }

    /// The folder `uid`'s socket is made in under `dir`, made for them (it is
    /// theirs: the test runs as them), and the socket's path.
    fn socket_for(dir: &Path, uid: u32) -> PathBuf {
        let folder = dir.join(uid.to_string());
        std::fs::create_dir(&folder).unwrap();
        folder.join("roomler.sock")
    }

    /// One request over the socket at `path`, as this process, answered
    /// within `within`; or why it was not.
    async fn ask(path: &Path, line: &str, within: Duration) -> Result<Response, String> {
        let asked = async {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| format!("connecting: {e}"))?;
            let (read, mut write) = stream.into_split();
            write
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| format!("writing: {e}"))?;
            let answer = tokio::io::BufReader::new(read)
                .lines()
                .next_line()
                .await
                .map_err(|e| format!("reading: {e}"))?
                .ok_or_else(|| "the socket closed with no answer".to_string())?;
            serde_json::from_str::<Response>(&answer).map_err(|e| format!("{e}: {answer}"))
        };
        tokio::time::timeout(within, asked)
            .await
            .map_err(|_| format!("no answer within {within:?}"))?
    }

    /// Whether the socket at `path` answers a status read.
    async fn served(path: &Path) -> bool {
        matches!(
            ask(path, STATUS, Duration::from_secs(2)).await,
            Ok(Response::Status(_))
        )
    }

    /// Until the socket at `path` is served, or with `false` is not, `LONG`
    /// at most.
    async fn until_served(path: &Path, want: bool, what: &str) {
        let deadline = Instant::now() + LONG;
        while served(path).await != want {
            assert!(Instant::now() < deadline, "{what}: not within {LONG:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// `line`, from this account, is refused with the try-again text within
    /// the check's own wait.
    async fn told_to_try_again(path: &Path, line: &str, what: &str) {
        let bound = crate::files::SEAT_LOOKUP_WAIT + Duration::from_secs(1);
        let began = Instant::now();
        let answer = ask(path, line, bound).await;
        let waited = began.elapsed();
        assert!(
            matches!(&answer, Ok(Response::Error { message }) if message == CANNOT_TELL_WHO_IS_AT_THE_SCREEN),
            "{what}: expected the try-again text, got {answer:?}"
        );
        assert!(waited < bound, "{what}: waited {waited:?}");
    }

    /// How many lines of `text` hold `needle`.
    fn lines_with(text: &str, needle: &str) -> usize {
        text.lines().filter(|line| line.contains(needle)).count()
    }

    /// Stops the poll and waits for it, `LONG` at most.
    async fn stop_following(stop: watch::Sender<bool>, poll: tokio::task::JoinHandle<()>) {
        let _ = stop.send(true);
        tokio::time::timeout(LONG, poll)
            .await
            .expect("the poll stops")
            .expect("the poll ran to its end");
    }

    /// #3845 — logind stops answering, and then `loginctl`'s D-Bus call gives
    /// up, as on a paused logind. Neither is nobody at the seat: the person's
    /// socket stays served the whole time, the poll says once that who is at
    /// the seat is not known, and the person's change is told to try again
    /// instead of meeting a closed socket. The poll's looks and the checks
    /// share one lookup in flight. Once logind answers, the same change goes
    /// through.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hung_logind_keeps_the_socket_served_and_the_person_is_told_to_try_again() {
        let Some(me) = me() else { return };
        let log = Logged::start(tracing::Level::INFO);
        let seat = TheSeat::finding(Look::Person(me));
        let _let_go = LetGoAtTheEnd(seat.clone());
        let _stood = stand_in::person_at_the_seat(seat.lookup()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_for(dir.path(), me);
        let state = Arc::new(Counted::default());
        let (stop, poll) = follow(&state, dir.path(), EVERY);
        until_served(&path, true, "the person's socket").await;

        // logind stops answering: every look hangs.
        let before_the_hang = seat.began();
        seat.hangs();
        tokio::time::sleep(SEVERAL_LOOKS).await;
        assert!(served(&path).await, "served while logind hangs");
        told_to_try_again(&path, SET, "a change while logind hangs").await;
        let during_the_hang = seat.began() - before_the_hang;

        // Then `loginctl` gives up, as its D-Bus call does after 25 s, and
        // goes on failing at once while logind stays stopped.
        seat.finds(Look::NotKnown(TIMED_OUT.into()));
        tokio::time::sleep(SEVERAL_LOOKS).await;
        assert!(
            served(&path).await,
            "served after the lookup failed: not known is not nobody"
        );
        told_to_try_again(&path, SET, "a change after the lookup failed").await;
        assert_eq!(state.sets.load(SeqCst), 0, "nothing set while not known");
        let text = log.text();
        assert_eq!(
            lines_with(
                &text,
                "who is at the seat is not known (no answer within 150ms); serving as before"
            ),
            1,
            "one line for the whole run:\n{text}"
        );
        assert_eq!(lines_with(&text, "; serving as before"), 1, "{text}");
        assert_eq!(lines_with(&text, "nobody at the seat now"), 0, "{text}");
        assert!(
            during_the_hang <= 1,
            "the poll's looks and a check while logind hung: one lookup between them, not \
             {during_the_hang}"
        );
        assert_eq!(seat.most_at_once(), 1, "never two lookups at once");

        // logind answers again: the same change goes through.
        seat.finds(Look::Person(me));
        match ask(&path, SET, LONG).await {
            Ok(Response::ConfigUpdated { entry }) => assert_eq!(entry.value.as_deref(), Some("on")),
            other => panic!("the person at the seat, once logind answers: {other:?}"),
        }
        assert_eq!(state.sets.load(SeqCst), 1);
        let deadline = Instant::now() + LONG;
        while lines_with(&log.text(), "who is at the seat is known again") == 0 {
            assert!(
                Instant::now() < deadline,
                "no end of the run:\n{}",
                log.text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(served(&path).await);
        stop_following(stop, poll).await;
    }

    /// #3845's control: when the seat itself answers that nobody is there
    /// (the login screen, an empty seat), the socket closes as before, with
    /// its one line, and nothing says "not known".
    #[tokio::test(flavor = "current_thread")]
    async fn nobody_at_the_seat_still_closes_the_socket() {
        let Some(me) = me() else { return };
        let log = Logged::start(tracing::Level::INFO);
        let seat = TheSeat::finding(Look::Person(me));
        let _let_go = LetGoAtTheEnd(seat.clone());
        let _stood = stand_in::person_at_the_seat(seat.lookup()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_for(dir.path(), me);
        let state = Arc::new(Counted::default());
        let (stop, poll) = follow(&state, dir.path(), EVERY);
        until_served(&path, true, "the person's socket").await;

        seat.finds(Look::Nobody);
        until_served(&path, false, "the socket closes for nobody at the seat").await;
        let text = log.text();
        assert_eq!(
            lines_with(&text, "nobody at the seat now; closing"),
            1,
            "{text}"
        );
        assert_eq!(
            lines_with(&text, "who is at the seat is not known"),
            0,
            "{text}"
        );
        stop_following(stop, poll).await;
    }

    /// #3845 — the person at the seat changes while logind cannot be read:
    /// the socket stays with the person it was made for, and moves only once
    /// logind answers, in one move and never by way of "nobody".
    #[tokio::test(flavor = "current_thread")]
    async fn a_seat_that_changed_while_not_known_moves_only_once_logind_answers() {
        let Some(me) = me() else { return };
        let other = me + 1;
        let log = Logged::start(tracing::Level::INFO);
        let seat = TheSeat::finding(Look::Person(me));
        let _let_go = LetGoAtTheEnd(seat.clone());
        let _stood = stand_in::person_at_the_seat(seat.lookup()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_for(dir.path(), me);
        let state = Arc::new(Counted::default());
        let (stop, poll) = follow(&state, dir.path(), EVERY);
        until_served(&path, true, "the person's socket").await;

        // logind cannot be read, and meanwhile someone else has the seat.
        seat.finds(Look::NotKnown(TIMED_OUT.into()));
        tokio::time::sleep(SEVERAL_LOOKS).await;
        assert!(
            served(&path).await,
            "still the socket of the person it was made for"
        );
        let text = log.text();
        assert_eq!(
            lines_with(&text, "the person at the seat changed"),
            0,
            "{text}"
        );
        assert_eq!(lines_with(&text, "nobody at the seat now"), 0, "{text}");

        // logind answers: the other person is at the seat. Their socket is
        // not made here (their folder is not theirs), which is beside the
        // point: the person who left loses theirs, in one move.
        seat.finds(Look::Person(other));
        until_served(&path, false, "the socket leaves the person who left").await;
        let text = log.text();
        let moved: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("the person at the seat changed"))
            .collect();
        assert_eq!(moved.len(), 1, "{text}");
        assert!(
            moved[0].contains(&format!("from={me}")) && moved[0].contains(&format!("to={other}")),
            "{}",
            moved[0]
        );
        assert_eq!(lines_with(&text, "nobody at the seat now"), 0, "{text}");
        stop_following(stop, poll).await;
    }

    /// #3845 — a consent answer lets someone see and control this screen, so
    /// the person socket asks again, as before a change, whether whoever
    /// answers is the person at the seat NOW. This poll looks once and then
    /// not for an hour, so the socket stays with the person it was made for
    /// while the seat changes under it: from someone who left it is refused
    /// in words, while who is there is not known it is told to try again,
    /// and from the person at the seat it goes through.
    #[tokio::test(flavor = "current_thread")]
    async fn a_consent_answer_comes_from_whoever_is_at_the_seat_now() {
        let Some(me) = me() else { return };
        let seat = TheSeat::finding(Look::Person(me));
        let _let_go = LetGoAtTheEnd(seat.clone());
        let _stood = stand_in::person_at_the_seat(seat.lookup()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_for(dir.path(), me);
        let state = Arc::new(Counted::default());
        let (stop, poll) = follow(&state, dir.path(), Duration::from_secs(3600));
        until_served(&path, true, "the person's socket").await;

        seat.finds(Look::Person(me + 1));
        let answer = ask(&path, ANSWER, LONG).await;
        assert!(
            matches!(&answer, Ok(Response::Error { message }) if message.contains("not this account right now")),
            "an answer from the person who left: {answer:?}"
        );
        seat.hangs();
        told_to_try_again(&path, ANSWER, "an answer while logind hangs").await;
        seat.finds(Look::NotKnown(TIMED_OUT.into()));
        told_to_try_again(&path, ANSWER, "an answer while logind cannot be read").await;
        assert_eq!(
            state.answers.load(SeqCst),
            0,
            "no answer reached the prompt"
        );

        seat.finds(Look::Person(me));
        assert_eq!(
            ask(&path, ANSWER, LONG).await,
            Ok(Response::ConsentDecided { ok: true })
        );
        assert_eq!(state.answers.load(SeqCst), 1);
        stop_following(stop, poll).await;
    }
}
