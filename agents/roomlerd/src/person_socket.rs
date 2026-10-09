// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-27 P11 — the person-at-the-device socket of a Linux SYSTEM install.
//!
//! A root daemon's LocalAPI (`/var/run/roomler`, 0700 / 0600) is root-only,
//! and rightly: it is the trust boundary for every administrative verb. But
//! the person at the screen must still see who is viewing it, end that, and
//! answer a consent prompt, and their companion runs as THEM. Before this, on
//! a system install it read "Service offline" and there was no banner, no
//! Disconnect and — on GNOME/KDE Wayland, which have no native overlay — no
//! consent prompt at all (#1911, measured 2026-10-09).
//!
//! This module keeps ONE [`localapi::serve_person_socket`] listening in the
//! runtime dir of whoever is at the seat, and moves it when that changes:
//! - "at the seat" is the ACTIVE graphical session's user, by the same
//!   `loginctl` walk the companion launch uses — never a display manager's
//!   greeter, and never a user switched away in the background (they must not
//!   answer consent for a view of someone else's desktop);
//! - root at the seat needs nothing: root reaches the daemon's own socket;
//! - a per-user daemon (not root) does nothing: its own socket already IS its
//!   person's.
//!
//! What the socket serves is decided in `localapi` ([`localapi::person_may`]).
//!
//! Kill switch: `ROOMLERD_PERSON_SOCKET=0` (or `false` / `off` / `no`) — the
//! pre-P11 behaviour.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tunnel_core::localapi::{self, LocalApiState};

/// How often the seat is looked at. A consent prompt stays up for 30 s or
/// more, so following a user switch within a few seconds is plenty, and each
/// look is a couple of `loginctl` calls.
const POLL: Duration = Duration::from_secs(4);

/// After a listener could not be served (no runtime dir yet, or a per-user
/// daemon owns the path), the same person is retried this long after, rather
/// than on every tick.
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

struct Listener {
    uid: u32,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

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
        let at_seat = tokio::task::spawn_blocking(|| {
            crate::companion::graphical_session_matching(None, true)
                .ok()
                .map(|s| s.uid)
        })
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
                let too_soon = failed.is_some_and(|(u, at)| u == uid && at.elapsed() < RETRY_AFTER);
                if !too_soon {
                    if let Some(l) = current.take() {
                        tracing::info!(
                            from = l.uid,
                            to = uid,
                            "person socket: the person at the seat changed"
                        );
                        l.stop().await;
                    }
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

    #[test]
    fn the_switch_is_on_unless_turned_off() {
        assert!(enabled_by_env(None));
        assert!(enabled_by_env(Some("1")));
        assert!(enabled_by_env(Some("")));
        for off in ["0", "false", "OFF", " no "] {
            assert!(!enabled_by_env(Some(off)), "{off:?} turns it off");
        }
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
}
