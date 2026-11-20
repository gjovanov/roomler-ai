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

/// After a listener could not be served (a per-user daemon's live socket
/// holds the path, a file that is not a socket is there, the directory is not
/// the person's), the same person is retried this long after, rather than on
/// every tick. #2531 — not for a runtime directory that is not there yet,
/// which a log-in makes a moment later: that is tried again at the next look.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// #2531 — how often a listener checks that its path still names its socket
/// (`localapi::serve_person_socket`): one `lstat` in the person's runtime
/// directory. A listener whose path no longer names it ends, and is served
/// again at once ([`Ended::Lost`]), so a removed or replaced socket file comes
/// back within this and one look.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PATH_CHECK: Duration = Duration::from_secs(1);

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
///
/// #4011 — the same ask as [`machine_reply`], which tells those apart for the
/// consent relay: a root half that is gone takes its questions off the
/// person's list at once, and one that did not answer leaves its last list
/// there for a while. Every other caller keeps its own answer for all three.
#[cfg(target_os = "macos")]
pub async fn machine_answer(
    uid: u32,
    req: &tunnel_core::localapi::Request,
    within: Duration,
) -> Option<Response> {
    use crate::consent_relay::RootReply;
    match machine_reply(uid, req, within).await {
        RootReply::Answer(answer) => Some(*answer),
        RootReply::Gone | RootReply::NoAnswer(_) => None,
    }
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

/// #4011 — [`machine_answer`]'s ask, with what came of it told apart
/// ([`crate::consent_relay::RootReply`]): the root half's answer, whatever it
/// says; proof that it is gone, when no socket is at its path for `uid` or
/// nothing listens on one there; and no answer for anything else, a socket
/// that someone other than root serves and an ask past `within` among them.
/// The consent relay keeps the root half's last list through no answer alone.
#[cfg(target_os = "macos")]
pub async fn machine_reply(
    uid: u32,
    req: &tunnel_core::localapi::Request,
    within: Duration,
) -> crate::consent_relay::RootReply {
    machine_reply_at(localapi::person_socket_path(uid), req, within).await
}

/// [`machine_reply`] at `path`, which a test makes in a folder of its own.
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
async fn machine_reply_at(
    path: std::path::PathBuf,
    req: &tunnel_core::localapi::Request,
    within: Duration,
) -> crate::consent_relay::RootReply {
    use crate::consent_relay::RootReply;
    let ask = async {
        // Only a socket ROOT serves is the machine's half.
        let mut client = match localapi::connect_unix_served_by_root(path).await {
            Ok(client) => client,
            Err(e) => return RootReply::not_connected(&e),
        };
        match client.request(req).await {
            Ok(answer) => RootReply::Answer(Box::new(answer)),
            Err(e) => RootReply::NoAnswer(format!("the request failed: {e}")),
        }
    };
    tokio::time::timeout(within, ask)
        .await
        .unwrap_or_else(|_| RootReply::NoAnswer(format!("none within {within:?}")))
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

/// #2531 — the person at the seat now, by the rule this socket follows (a
/// person, never root), through the same one lookup in flight,
/// `files::SEAT_LOOKUP_WAIT` at most. `None` for nobody, and for a look that
/// could not tell. The being-viewed notice (`indicator::notify::viewing`)
/// asks it whose reads of the live sessions show that the companion of the
/// person at the screen is listening: this socket serves that person, so
/// their companion reads over it.
#[cfg(target_os = "linux")]
pub async fn person_at_the_seat_now() -> Option<u32> {
    match look_at_the_seat(crate::files::SEAT_LOOKUP_WAIT).await {
        Look::Person(uid) if uid != 0 => Some(uid),
        Look::Person(_) | Look::Nobody | Look::NotKnown(_) => None,
    }
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

/// #2531 — how a listener's task ended on its own, which decides when its
/// person is served again.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Its path no longer named its socket (removed, or replaced): a log-out
    /// and log-in, or the file taken away. Served again at once.
    Lost,
    /// The person's runtime directory was not there: a log-in makes it a
    /// moment after the seat shows the person. Tried again at the next look.
    NoDir,
    /// Not served for any other reason: tried again after [`RETRY_AFTER`].
    Failed,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Listener {
    uid: u32,
    stop: watch::Sender<bool>,
    /// #2531 — how its task ended, once it has; `None` while it serves.
    ended: watch::Receiver<Option<Ended>>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Listener {
    fn start(
        uid: u32,
        path: std::path::PathBuf,
        state: Arc<dyn LocalApiState>,
        check: Duration,
    ) -> Self {
        let (stop, rx) = watch::channel(false);
        let (said, ended) = watch::channel(None);
        let task = tokio::spawn(async move {
            let how = match localapi::serve_person_socket(path.clone(), uid, state, rx, check).await
            {
                Ok(()) => Ended::Lost,
                // Linux only: a Mac's bind makes its own root-owned
                // directory, and a failure there keeps the pause, as before.
                Err(e) if cfg!(target_os = "linux") && e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        path = %path.display(), uid, error = %e,
                        "person socket: the person's runtime directory is not there yet"
                    );
                    Ended::NoDir
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(), uid, error = %e,
                        "person socket: not served (FR-27 P11); retrying later"
                    );
                    Ended::Failed
                }
            };
            let _ = said.send(Some(how));
        });
        Self {
            uid,
            stop,
            ended,
            task,
        }
    }

    /// #2531 — whether its task has ended (or is ending) on its own.
    fn has_ended(&self) -> bool {
        self.task.is_finished()
            || self.ended.borrow().is_some()
            || self.ended.has_changed().is_err()
    }

    /// #2531 — until its task ends because its path no longer names its
    /// socket. Any other end waits for the next look, where its retry rule
    /// applies: a runtime directory that is not there is not tried again in
    /// a loop.
    async fn until_lost(&mut self) {
        if self
            .ended
            .wait_for(|how| *how == Some(Ended::Lost))
            .await
            .is_err()
        {
            std::future::pending::<()>().await;
        }
    }

    async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

/// Run until `shutdown`. Inert unless this is a root daemon with the switch on.
///
/// #3967 — on a Mac, the other side of the same socket: a two-row Mac's
/// per-user half (`LocalApiState::announces_consent_relay`) tells its root
/// half here, for as long as it runs, that it lists the root half's consent
/// questions to its person's app (`consent_relay::tell_the_root_half`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn run(state: Arc<dyn LocalApiState>, shutdown: watch::Receiver<bool>) {
    if !enabled_by_env(std::env::var("ROOMLERD_PERSON_SOCKET").ok().as_deref()) {
        tracing::info!("person socket: off (ROOMLERD_PERSON_SOCKET) — FR-27 P11");
        return;
    }
    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } != 0 {
        #[cfg(target_os = "macos")]
        if state.announces_consent_relay() {
            crate::consent_relay::tell_the_root_half(shutdown).await;
        }
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
    /// #2531 — how often a listener checks its path ([`PATH_CHECK`]).
    check: Duration,
    /// #2531 — the pause after a listener that could not be served
    /// ([`RETRY_AFTER`]).
    retry_after: Duration,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Following {
    fn the_daemons() -> Self {
        Self {
            every: POLL,
            wait: LOOK_WAIT,
            path: Box::new(localapi::person_socket_path),
            check: PATH_CHECK,
            retry_after: RETRY_AFTER,
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
///
/// #2531 — a listener lives only while its path names its socket
/// (`localapi::serve_person_socket`). One that ended because it no longer
/// did is looked after at once, and served again at that look with no pause:
/// before, a log-out and log-in that no look fell between left it serving a
/// socket no path reached, until the daemon restarted.
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
    // #2531 — whose runtime directory was missing at the last try, said once.
    let mut no_dir: Option<u32> = None;
    loop {
        // A listener that ended on its own: the step below sees "nobody
        // served", and how it ended decides whether it is served again at
        // this look or after the pause (#2531).
        if let Some(l) = current.take_if(|l| l.has_ended()) {
            let how = *l.ended.borrow();
            let uid = l.uid;
            let _ = l.task.await;
            match how.unwrap_or(Ended::Failed) {
                Ended::Lost => no_dir = None,
                Ended::NoDir => {
                    if no_dir != Some(uid) {
                        tracing::info!(
                            uid,
                            "person socket: the person's runtime directory is not there yet; \
                             trying again at each look (#2531)"
                        );
                        no_dir = Some(uid);
                    }
                }
                Ended::Failed => {
                    no_dir = None;
                    failed = Some((uid, Instant::now()));
                }
            }
        }
        let look = look_at_the_seat(following.wait).await;
        // #2531 — nobody at the seat ends a run of tries for a missing
        // runtime directory, so the next log-in's is said again.
        if matches!(look, Look::Nobody) {
            no_dir = None;
        }
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
                let too_soon =
                    failed.is_some_and(|(u, at)| u == uid && at.elapsed() < following.retry_after);
                if !too_soon {
                    current = Some(Listener::start(
                        uid,
                        (following.path)(uid),
                        state.clone(),
                        following.check,
                    ));
                    failed = None;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(following.every) => {}
            // #2531 — a listener whose path no longer names it is looked
            // after at once.
            () = async {
                match current.as_mut() {
                    Some(l) => l.until_lost().await,
                    None => std::future::pending().await,
                }
            } => {}
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
        follow_pausing(state, dir, every, RETRY_AFTER)
    }

    /// #2531 — how often a test's listener checks its path.
    const CHECK: Duration = Duration::from_millis(20);

    /// [`follow`], pausing `retry_after` after a listener that could not be
    /// served.
    fn follow_pausing(
        state: &Arc<Counted>,
        dir: &Path,
        every: Duration,
        retry_after: Duration,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let (stop, shutdown) = watch::channel(false);
        let state: Arc<dyn LocalApiState> = state.clone();
        let dir = dir.to_path_buf();
        let following = Following {
            every,
            wait: WAIT,
            path: Box::new(move |uid| dir.join(uid.to_string()).join("roomler.sock")),
            check: CHECK,
            retry_after,
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

    /// The socket file at `path` as (device, inode), if one is there.
    ///
    /// ⚠️ An identity only while something holds that file open: once the
    /// last holder of a removed file is gone, the filesystem may give its
    /// inode number to the next file (ext4 did, here). So the tests below
    /// count the binds in the log (`UP`) and ask the path, and compare inodes
    /// only of a file a live socket holds.
    fn file_at(path: &Path) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .ok()
            .map(|m| (m.dev(), m.ino()))
    }

    /// The line each bind of a person socket logs.
    const UP: &str = "person-at-the-device socket up";

    /// #2531 — the person socket's file is removed, as a log-out and log-in
    /// that no look falls between does to it (logind removes the runtime
    /// directory and makes a new one), and as the lab's RED did by hand. The
    /// listener ends within one look, the poll binds a new socket at the same
    /// path for the same person with no pause (the pause here is `LONG`), and
    /// the old listener's way out takes nothing from the new one.
    #[tokio::test(flavor = "current_thread")]
    async fn a_removed_socket_file_is_served_again_within_a_look() {
        let Some(me) = me() else { return };
        let log = Logged::start(tracing::Level::INFO);
        let seat = TheSeat::finding(Look::Person(me));
        let _let_go = LetGoAtTheEnd(seat.clone());
        let _stood = stand_in::person_at_the_seat(seat.lookup()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_for(dir.path(), me);
        let state = Arc::new(Counted::default());
        let (stop, poll) = follow_pausing(&state, dir.path(), EVERY, LONG);
        until_served(&path, true, "the person's socket").await;

        // With no file at the path nothing answers there, so an answer is
        // the new socket's.
        std::fs::remove_file(&path).unwrap();
        let removed = Instant::now();
        until_served(&path, true, "a new socket at the path").await;
        assert!(
            removed.elapsed() < SEVERAL_LOOKS,
            "served again {:?} after the removal: not within a look",
            removed.elapsed()
        );
        let second = file_at(&path);
        // Several looks and checks later the new socket is still the one at
        // the path, and still served: nothing took it away, and nothing was
        // bound again (its own listener holds its file, so the pair is its).
        tokio::time::sleep(SEVERAL_LOOKS).await;
        assert_eq!(file_at(&path), second, "the new socket stays");
        assert!(served(&path).await, "and is served");
        let text = log.text();
        assert_eq!(lines_with(&text, UP), 2, "{text}");
        assert_eq!(lines_with(&text, "path no longer names it"), 1, "{text}");
        assert_eq!(lines_with(&text, "retrying later"), 0, "{text}");
        stop_following(stop, poll).await;
    }

    /// #2531 — the socket's file is replaced by another socket. One nobody
    /// serves (left behind) is cleared and the person served again within a
    /// look. One that another process serves (a per-user daemon of the same
    /// person would) ends the listener all the same, and is never taken over:
    /// the old listener's way out leaves it, and the poll keeps the pause for
    /// it. Once it is gone, the person is served again after the pause.
    #[tokio::test(flavor = "current_thread")]
    async fn a_replaced_socket_file_ends_the_listener_and_a_live_one_is_left_alone() {
        let Some(me) = me() else { return };
        let log = Logged::start(tracing::Level::INFO);
        let seat = TheSeat::finding(Look::Person(me));
        let _let_go = LetGoAtTheEnd(seat.clone());
        let _stood = stand_in::person_at_the_seat(seat.lookup()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_for(dir.path(), me);
        let state = Arc::new(Counted::default());
        let pause = Duration::from_millis(600);
        let (stop, poll) = follow_pausing(&state, dir.path(), EVERY, pause);
        until_served(&path, true, "the person's socket").await;

        // Replaced by a socket nobody serves: cleared, and served again. The
        // one left behind answers nothing, so an answer is the new socket's.
        std::fs::remove_file(&path).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let replaced = Instant::now();
        until_served(&path, true, "a new socket over the one left behind").await;
        assert!(
            replaced.elapsed() < SEVERAL_LOOKS,
            "served again {:?} after the replacement: not within a look",
            replaced.elapsed()
        );
        let text = log.text();
        assert_eq!(lines_with(&text, UP), 2, "{text}");

        // Replaced by a socket another process serves: left alone. Its file
        // is held open by `other`, so its pair names it alone.
        std::fs::remove_file(&path).unwrap();
        let other = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let others = file_at(&path);
        let deadline = Instant::now() + LONG;
        while lines_with(&log.text(), "path no longer names it") < 2 {
            assert!(Instant::now() < deadline, "the listener did not end");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(pause / 2).await;
        assert_eq!(file_at(&path), others, "the other process's socket stays");
        assert!(
            std::os::unix::net::UnixStream::connect(&path).is_ok(),
            "and still answers"
        );
        let text = log.text();
        assert!(lines_with(&text, "retrying later") >= 1, "{text}");
        assert_eq!(
            lines_with(&text, UP),
            2,
            "nothing was bound over it: {text}"
        );

        // It goes, leaving its file: after the pause, served again.
        drop(other);
        until_served(&path, true, "the person's socket again").await;
        let text = log.text();
        assert_eq!(lines_with(&text, UP), 3, "{text}");
        stop_following(stop, poll).await;
    }

    /// #2531 — the seat shows the person before logind has made their
    /// runtime directory, as a log-in can. The bind is tried again at the
    /// next look, not after the pause (the pause here is `LONG`), it is said
    /// once, and the socket is up as soon as the directory exists. After a
    /// log-out (nobody at the seat, the directory removed) the next log-in's
    /// missing directory is said once again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_runtime_directory_not_there_yet_is_tried_again_at_the_next_look() {
        let Some(me) = me() else { return };
        let log = Logged::start(tracing::Level::INFO);
        let seat = TheSeat::finding(Look::Person(me));
        let _let_go = LetGoAtTheEnd(seat.clone());
        let _stood = stand_in::person_at_the_seat(seat.lookup()).await;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Counted::default());
        let (stop, poll) = follow_pausing(&state, dir.path(), EVERY, LONG);
        // Several looks with no directory to bind in.
        tokio::time::sleep(SEVERAL_LOOKS / 2).await;
        let path = socket_for(dir.path(), me);
        let made = Instant::now();
        until_served(&path, true, "the socket, once the directory is there").await;
        assert!(
            made.elapsed() < SEVERAL_LOOKS,
            "served {:?} after the directory was made: not at the next look",
            made.elapsed()
        );
        let text = log.text();
        assert_eq!(
            lines_with(&text, "runtime directory is not there yet"),
            1,
            "{text}"
        );
        assert_eq!(lines_with(&text, "retrying later"), 0, "{text}");

        // A log-out: nobody at the seat, and logind removes the directory.
        seat.finds(Look::Nobody);
        let deadline = Instant::now() + LONG;
        while file_at(&path).is_some() {
            assert!(
                Instant::now() < deadline,
                "the socket stayed after the log-out"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        std::fs::remove_dir(path.parent().unwrap()).unwrap();
        // The next log-in shows the person before the directory again.
        seat.finds(Look::Person(me));
        let deadline = Instant::now() + LONG;
        while lines_with(&log.text(), "runtime directory is not there yet") < 2 {
            assert!(
                Instant::now() < deadline,
                "the next log-in's missing directory was not said: {}",
                log.text()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let path = socket_for(dir.path(), me);
        until_served(&path, true, "the socket at the next log-in").await;
        let text = log.text();
        assert_eq!(
            lines_with(&text, "runtime directory is not there yet"),
            2,
            "{text}"
        );
        assert_eq!(lines_with(&text, "retrying later"), 0, "{text}");
        stop_following(stop, poll).await;
    }
}

/// #4011 — what an ask of a real socket path comes to, as the consent relay
/// reads it ([`machine_reply_at`]). No socket at the path, or one that nothing
/// listens on (a root daemon that stopped and left it behind), is proof that
/// the root half is gone. A live socket that root does not serve is no
/// answer, never gone. CI runs it as the runner: rust-agent's "Test agent
/// under vp9-444" and the macOS job's "cargo check + test the agent's Mac-only
/// units". Run as root (`ROOMLERD_TEST_REQUIRE_ROOT` turns the skip into a
/// failure), a socket root serves that never answers is no answer within the
/// bound, and one that answers is its answer.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod machine_reply_tests {
    use super::*;
    use crate::consent_relay::RootReply;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tunnel_core::localapi::Request;

    /// Long enough that only something stuck runs out of it.
    const LONG: Duration = Duration::from_secs(10);

    /// Whether this test runs as root. Under `ROOMLERD_TEST_REQUIRE_ROOT` a
    /// run that is not root fails instead, so a sudo run cannot pass by
    /// skipping the root half's answer.
    fn as_root() -> bool {
        // SAFETY: geteuid never fails.
        let root = unsafe { libc::geteuid() } == 0;
        if !root && std::env::var_os("ROOMLERD_TEST_REQUIRE_ROOT").is_some() {
            panic!("ROOMLERD_TEST_REQUIRE_ROOT is set and this test is not running as root");
        }
        root
    }

    #[tokio::test]
    async fn a_gone_root_half_is_told_from_one_that_does_not_answer() {
        let list = Request::ConsentPending;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("501.sock");

        // No socket at the path: the seat moved, or the root daemon stopped.
        assert_eq!(
            machine_reply_at(path.clone(), &list, LONG).await,
            RootReply::Gone
        );

        // A socket nothing listens on: a root daemon that stopped and left it.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "the socket is left behind");
        assert_eq!(
            machine_reply_at(path.clone(), &list, LONG).await,
            RootReply::Gone
        );
        std::fs::remove_file(&path).unwrap();

        // A live socket, which this test's own account serves.
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        if !as_root() {
            let reply = machine_reply_at(path.clone(), &list, LONG).await;
            assert!(
                matches!(&reply, RootReply::NoAnswer(why) if why.contains("not root")),
                "a socket that someone other than root serves is no answer: {reply:?}"
            );
            eprintln!("machine-reply: not root, so a socket root serves is not asked here");
            return;
        }

        // As root, it is served by root, and it never answers.
        let within = Duration::from_millis(300);
        let asked = std::time::Instant::now();
        let reply = machine_reply_at(path.clone(), &list, within).await;
        assert!(
            matches!(&reply, RootReply::NoAnswer(why) if why.contains("none within")),
            "{reply:?}"
        );
        assert!(asked.elapsed() < LONG, "it waited {:?}", asked.elapsed());

        // And it answers each connection's request: the first it takes is the
        // silent ask's, whose asker gave up on it.
        let answering = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                let read = tokio::io::BufReader::new(read).read_line(&mut line).await;
                if !matches!(read, Ok(n) if n > 0) {
                    continue;
                }
                let mut answer = serde_json::to_vec(&Response::ConsentPending(Vec::new())).unwrap();
                answer.push(b'\n');
                let _ = write.write_all(&answer).await;
            }
        });
        assert_eq!(
            machine_reply_at(path.clone(), &list, LONG).await,
            RootReply::Answer(Box::new(Response::ConsentPending(Vec::new())))
        );
        answering.abort();
        eprintln!("machine-reply: as root, a silent root half is no answer and an answer is read");
    }
}
