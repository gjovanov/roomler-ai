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

/// Pure: the listener follows the person at the seat. Root at the seat is
/// nobody to serve here.
pub fn step(serving: Option<u32>, at_seat: Option<u32>) -> Step {
    let want = at_seat.filter(|&u| u != 0);
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
/// user. `None` for nobody, and for root.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn person_at_the_seat() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        // This runs on EVERY Linux root daemon, servers and cluster nodes
        // included. Where no non-root user is logged in at all there is
        // nobody to serve, and no `loginctl` to spawn.
        if !any_user_runtime_dir(std::path::Path::new("/run/user")) {
            return None;
        }
        // #2871 (review) — seat0's person, never the first active graphical
        // session anywhere: logind calls every seatless session active, so on
        // a host with remote logins (xrdp, GNOME Remote Login) that was
        // whoever logind listed first, and since design A this socket sets
        // the person's own config keys.
        crate::companion::seat0_uid().ok().filter(|&u| u != 0)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        // `/dev/console`'s owner, as the FR-43 supervisor reads it: uid 0 is
        // the login window, which is nobody to serve.
        std::fs::metadata("/dev/console")
            .ok()
            .map(|m| m.uid())
            .filter(|&u| u != 0)
    }
}

/// #2871 (review) — is `uid` the person at the seat RIGHT NOW? The person
/// socket asks before it applies a config change
/// (`LocalApiState::at_the_seat_now`): its listener follows the seat on a
/// [`POLL`], so a person who switched away still holds an open connection
/// until the next look, and must not change settings that are the next
/// person's by then.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn at_the_seat_now(uid: u32) -> bool {
    tokio::task::spawn_blocking(person_at_the_seat)
        .await
        .ok()
        .flatten()
        == Some(uid)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Listener {
    uid: u32,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Listener {
    fn start(uid: u32, state: Arc<dyn LocalApiState>) -> Self {
        let path = localapi::person_socket_path(uid);
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
pub async fn run(state: Arc<dyn LocalApiState>, mut shutdown: watch::Receiver<bool>) {
    if !enabled_by_env(std::env::var("ROOMLERD_PERSON_SOCKET").ok().as_deref()) {
        tracing::info!("person socket: off (ROOMLERD_PERSON_SOCKET) — FR-27 P11");
        return;
    }
    // SAFETY: geteuid never fails.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let mut current: Option<Listener> = None;
    let mut failed: Option<(u32, Instant)> = None;
    loop {
        // A listener that ended on its own was not served: note it, and let
        // the step below see "nobody served" so it retries after the pause.
        if let Some(l) = current.take_if(|l| l.task.is_finished()) {
            failed = Some((l.uid, Instant::now()));
        }
        let at_seat = tokio::task::spawn_blocking(person_at_the_seat)
            .await
            .ok()
            .flatten();
        match step(current.as_ref().map(|l| l.uid), at_seat) {
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
                    current = Some(Listener::start(uid, state.clone()));
                    failed = None;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
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
        assert_eq!(step(None, None), Step::Keep);
        assert_eq!(step(None, Some(1000)), Step::Start(1000));
        assert_eq!(step(Some(1000), Some(1000)), Step::Keep);
        assert_eq!(step(Some(1000), Some(1001)), Step::Move(1001));
        assert_eq!(step(Some(1000), None), Step::Stop);
        // Root at the seat reaches the daemon's own socket.
        assert_eq!(step(None, Some(0)), Step::Keep);
        assert_eq!(step(Some(1000), Some(0)), Step::Stop);
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
