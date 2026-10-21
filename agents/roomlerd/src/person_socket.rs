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
//! - on Linux, "at the seat" is the ACTIVE graphical session's user, by the
//!   same `loginctl` walk the companion launch uses — never a display
//!   manager's greeter, and never a user switched away in the background
//!   (they must not answer consent for a view of someone else's desktop);
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
//! Kill switch: `ROOMLERD_PERSON_SOCKET=0` (or `false` / `off` / `no`) — the
//! pre-P11 behaviour, and on a Mac the pre-P2g one, since with no socket the
//! per-user half keeps its own answer.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::sync::watch;
use tunnel_core::localapi::Response;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tunnel_core::localapi::{self, LocalApiState};

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
            && let Some(machine) = machine_answer(euid, &req).await
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

/// FR-97 P2g — ask the root half's person socket for `uid`. `None` when it is
/// not there, is served by anyone but root, or does not answer within a few
/// seconds: the caller keeps its own answer then.
#[cfg(target_os = "macos")]
pub async fn machine_answer(uid: u32, req: &tunnel_core::localapi::Request) -> Option<Response> {
    let path = localapi::person_socket_path(uid);
    if !path.exists() {
        return None;
    }
    let ask = async {
        // Only a socket ROOT serves is the machine's half.
        let mut client = localapi::connect_unix_served_by_root(path).await.ok()?;
        client.request(req).await.ok()
    };
    tokio::time::timeout(Duration::from_secs(5), ask)
        .await
        .ok()
        .flatten()
}

/// Who is at the seat now: on Linux the active graphical session's user, on
/// a Mac the console user. `None` for nobody, and for root.
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
        crate::companion::graphical_session_matching(None, true)
            .ok()
            .map(|s| s.uid)
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
}
