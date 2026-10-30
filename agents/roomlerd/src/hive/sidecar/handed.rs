// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2c-3 — a session's model sidecar, served by its worker.
//!
//! Mirrors `toolbelt::handed`'s own shape (`docs/fr/FR-90-hive-agent-
//! sessions.md` §3's P8-0 point 2): the daemon keeps the listener it bound
//! and hands a copy to the worker, which runs the real HTTP dispatch —
//! [`super::serve`]/[`super::handle`], unchanged — against it. Unlike the
//! toolbelt's Unix-domain socket, the sidecar's listener is a loopback TCP
//! port, so — like `vault_connect`'s own loopback endpoints (P8-2b-3) —
//! both OSes cross it: `SCM_RIGHTS` on Unix, `WSADuplicateSocketW`'s
//! protocol info on Windows.
//!
//! | Piece | As built |
//! |---|---|
//! | what stays the daemon's | the listener it bound (`sidecar::open`'s port); the device's `Supervisor`, reached only by [`admit`]/[`key_forgotten`] |
//! | what the worker is handed | a copy of the listener, this run's `upstream` and `workspace` (none of it secret) — nothing else. The device's model key never crosses in bulk: a worker asks for it fresh, one call's worth at a time ([`Call::SidecarAdmit`]) |
//! | a call | admitted and keyed by one round trip to the daemon ([`WorkerSource::admit`]), exactly as `wait_for_lease_word` + `sidecar_admit` + `model_key` run for a daemon-hosted one, in the same order |
//! | the provider's 401 | [`Call::SidecarForgetKey`], so the daemon fetches the key again next time |
//! | a new worker | the sidecar is handed again, on the same listener: a connection the lost worker held is gone, the port is not |
//! | no worker up | `sidecar::open` serves it in the daemon itself, exactly as before this step |

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use bson::oid::ObjectId;
use hyper::StatusCode;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::debug;
#[cfg(windows)]
use tracing::warn;
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::SOCKET;
use zeroize::Zeroizing;

use super::{Refusal, Source};
use crate::hive::supervisor::Supervisor;
use crate::hive::vault_connect::handed::{Answer, Call, Secret};
use crate::hive::worker::link::Link;

// ─── The daemon's side ──────────────────────────────────────────────────────

/// A sidecar handed to its run's worker, as the daemon keeps it. One a
/// run, unlike `vault_connect`'s many endpoints.
struct Kept {
    endpoint: u64,
    /// The daemon's own copy of the listener, handed again to each
    /// worker that follows: an owned descriptor on Unix (`SCM_RIGHTS`);
    /// on Windows a real listener of the daemon's own, duplicated to
    /// itself at hand-time so it outlives whatever place originally
    /// bound it (mirrors `vault_connect::handed::Kept`'s own split).
    #[cfg(unix)]
    listener: OwnedFd,
    #[cfg(windows)]
    listener: std::net::TcpListener,
    /// Reached only by [`admit`]/[`key_forgotten`], never stored
    /// stronger: a worker's channel outlives neither its run nor the
    /// daemon, so this never keeps the daemon's `Supervisor` alive past
    /// its own life.
    sup: Weak<Supervisor>,
    upstream: String,
    workspace: Option<String>,
}

type Runs = HashMap<(ObjectId, u64), Kept>;

static KEPT: LazyLock<Mutex<Runs>> = LazyLock::new(Mutex::default);

fn kept() -> std::sync::MutexGuard<'static, Runs> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

/// FR-90 P8-2a-2's own counter, shared: no two things a worker serves
/// share one endpoint id.
fn next_endpoint() -> u64 {
    crate::hive::vault_connect::handed::next_endpoint()
}

/// Hand `listener` (already bound by the caller — `sidecar::open`) to
/// `session` at `fence`'s worker: `None` where the run has none (the
/// caller serves it in the daemon instead), the endpoint handed
/// otherwise.
///
/// Synchronous, like `toolbelt::handed::prepare`: the actual hand-over
/// (which must wait on a possibly-still-starting worker, then make one
/// round trip) is spawned, not awaited, so a connection that arrives
/// before it completes queues in the listener's own backlog rather than
/// blocking the session's launch on a worker that may be slow to start.
pub(crate) fn prepare(
    session: ObjectId,
    fence: u64,
    listener: &TcpListener,
    sup: Weak<Supervisor>,
    upstream: String,
    workspace: Option<String>,
) -> Result<Option<u64>, String> {
    if !crate::hive::worker::has(session, fence) {
        return Ok(None);
    }
    let failed = |e: std::io::Error| format!("the sidecar's listener: {e}");
    #[cfg(unix)]
    let ours = listener.as_fd().try_clone_to_owned().map_err(failed)?;
    // FR-90 P8-2b-3's own pattern — Windows: the daemon's copy is a real
    // listener, duplicated to itself (so its life is the map's alone).
    #[cfg(windows)]
    let ours = {
        let raw = listener.as_raw_socket() as SOCKET;
        crate::hive::worker::win::socket_from_info(
            &crate::hive::worker::win::duplicate_for(std::process::id(), raw).map_err(failed)?,
        )
        .map_err(failed)?
    };
    let endpoint = next_endpoint();
    kept().insert(
        (session, fence),
        Kept {
            endpoint,
            listener: ours,
            sup,
            upstream,
            workspace,
        },
    );
    tokio::spawn(hand_over(session, fence));
    Ok(Some(endpoint))
}

/// The actual hand-over [`prepare`] spawns.
async fn hand_over(session: ObjectId, fence: u64) {
    let Some(link) = crate::hive::worker::up(session, fence).await else {
        debug!(%session, fence, "hive: the sidecar's worker never came up: it serves nothing until the next one does");
        return;
    };
    send_serve(session, fence, &link).await;
}

/// The sidecar handed to the run's worker so far, handed to `link`'s: the
/// worker that follows serves what the lost one did. Mirrors
/// `vault_connect::handed::rehand`.
pub(crate) async fn rehand(session: ObjectId, fence: u64, link: &Link) {
    send_serve(session, fence, link).await;
}

/// [`hand_over`] and [`rehand`] both: send this run's sidecar as
/// [`Call::SidecarServe`], the kept listener riding with it.
async fn send_serve(session: ObjectId, fence: u64, link: &Link) {
    #[cfg(unix)]
    {
        let found = kept().get(&(session, fence)).and_then(|k| {
            k.listener
                .try_clone()
                .ok()
                .map(|fd| (k.endpoint, k.upstream.clone(), k.workspace.clone(), fd))
        });
        let Some((endpoint, upstream, workspace, fd)) = found else {
            return;
        };
        let answer = link
            .call(
                Call::SidecarServe {
                    endpoint,
                    session,
                    upstream,
                    workspace,
                },
                vec![fd],
            )
            .await;
        report(session, fence, answer);
    }
    // FR-90 P8-2b-3's own pattern — Windows: no second channel, so each
    // hand-over duplicates the listener afresh, targeting the new
    // worker's pid (protocol info is single-use and per-target-pid).
    #[cfg(windows)]
    {
        let Some(pid) = crate::hive::worker::pid_of(session, fence) else {
            warn!(%session, fence, "hive: the sidecar's new worker has no pid yet: it waits for the next rehand");
            return;
        };
        let found = kept().get(&(session, fence)).map(|k| {
            (
                k.endpoint,
                k.upstream.clone(),
                k.workspace.clone(),
                k.listener.as_raw_socket() as SOCKET,
            )
        });
        let Some((endpoint, upstream, workspace, raw)) = found else {
            return;
        };
        let answer = match crate::hive::worker::win::duplicate_for(pid, raw) {
            Ok(info) => {
                link.call(Call::SidecarServe {
                    endpoint,
                    session,
                    upstream,
                    workspace,
                    info: info.to_vec(),
                })
                .await
            }
            Err(e) => Err(e.to_string()),
        };
        report(session, fence, answer);
    }
}

fn report(session: ObjectId, fence: u64, answer: Result<Answer, String>) {
    match answer {
        Ok(Answer::Done) => {}
        other => {
            debug!(%session, fence, answer = ?other, "hive: the sidecar's hand-over went unanswered: the next worker (or a later rehand) serves it");
        }
    }
}

/// The sidecar is gone: forgotten, so a later `rehand` finds nothing.
pub(crate) fn forget(session: ObjectId, fence: u64) {
    kept().remove(&(session, fence));
}

/// A test's: whether this run's sidecar is kept for hand-over now.
#[cfg(test)]
pub(crate) fn is_kept(session: ObjectId, fence: u64) -> bool {
    kept().contains_key(&(session, fence))
}

/// A test's: keep a sidecar for (`session`, `fence`) directly, without a
/// real listener — [`admit`]/[`key_forgotten`]'s own dispatch is what
/// these tests exercise, not `prepare`'s worker-gate or its listener
/// plumbing.
#[cfg(test)]
pub(crate) fn keep_for_test(session: ObjectId, fence: u64, endpoint: u64, sup: Weak<Supervisor>) {
    // A real listener purely so `Kept` has something to hold; this test
    // never dials or serves it. Bound on port 0 (RAII-cleaned on drop,
    // and genuinely unique — no stale-file collision like a Unix
    // socket's name could hit, since there is no name to reuse).
    let std_listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("a test listener");
    std_listener.set_nonblocking(true).expect("nonblocking");
    #[cfg(unix)]
    let listener = std_listener.as_fd().try_clone_to_owned().expect("dup");
    #[cfg(windows)]
    let listener = std_listener;
    kept().insert(
        (session, fence),
        Kept {
            endpoint,
            listener,
            sup,
            upstream: super::DEFAULT_UPSTREAM.to_string(),
            workspace: None,
        },
    );
}

/// The daemon's answer to a worker's [`Call::SidecarAdmit`]: the same
/// lease-wait, admission and key fetch a daemon-hosted call runs, in the
/// same order, against the `Supervisor` this endpoint was handed with.
pub(crate) async fn admit(
    session: ObjectId,
    fence: u64,
    endpoint: u64,
    token: Option<String>,
) -> Answer {
    let Some(sup) = sup_of(session, fence, endpoint) else {
        return Answer::Refused {
            why: "no such sidecar".into(),
        };
    };
    match sup.sidecar_admit_and_key(session, token.as_deref()).await {
        Ok(key) => Answer::SidecarKey {
            key: Secret(Zeroizing::new(key)),
        },
        Err((kind, message)) => Answer::SidecarRefused { kind, message },
    }
}

/// The daemon's answer to a worker's [`Call::SidecarForgetKey`].
pub(crate) async fn key_forgotten(session: ObjectId, fence: u64, endpoint: u64) -> Answer {
    if let Some(sup) = sup_of(session, fence, endpoint) {
        sup.forget_model_key().await;
    }
    Answer::Done
}

fn sup_of(session: ObjectId, fence: u64, endpoint: u64) -> Option<Arc<Supervisor>> {
    kept()
        .get(&(session, fence))
        .filter(|k| k.endpoint == endpoint)
        .and_then(|k| k.sup.upgrade())
}

// ─── The worker's side ──────────────────────────────────────────────────────

/// The sidecars one worker serves, each one's stop a child of `all`: the
/// worker's loop holds a guard on `all`, so every sidecar stops as the
/// loop ends however it ends, as the end of its process would stop them.
#[derive(Default)]
pub(crate) struct Serving {
    endpoints: Mutex<HashMap<u64, CancellationToken>>,
    pub(crate) all: CancellationToken,
}

/// The worker's answer to the daemon's [`Call::SidecarServe`]: serve the
/// handed listener with the sidecar's own, unchanged
/// [`super::serve`]/[`super::handle`], asking the daemon for admission
/// and the key one call at a time ([`WorkerSource`]).
pub(crate) fn serve_handed(
    serving: &Arc<Serving>,
    link: Arc<Link>,
    session: ObjectId,
    endpoint: u64,
    upstream: String,
    workspace: Option<String>,
    #[cfg(unix)] listener: OwnedFd,
    #[cfg(windows)] listener: Vec<u8>,
) -> Answer {
    let mut endpoints = serving.endpoints.lock().unwrap_or_else(|e| e.into_inner());
    if endpoints.contains_key(&endpoint) {
        // A hand-over answered twice (the first already claimed it).
        return Answer::Done;
    }
    #[cfg(unix)]
    let std_listener = std::net::TcpListener::from(listener);
    #[cfg(windows)]
    let std_listener = match crate::hive::worker::win::socket_from_info(&listener) {
        Ok(l) => l,
        Err(e) => {
            return Answer::Refused {
                why: format!("the sidecar's listener: {e}"),
            };
        }
    };
    let listener = match std_listener
        .set_nonblocking(true)
        .and_then(|()| TcpListener::from_std(std_listener))
    {
        Ok(l) => l,
        Err(e) => {
            return Answer::Refused {
                why: format!("the sidecar's listener: {e}"),
            };
        }
    };
    let stop = serving.all.child_token();
    endpoints.insert(endpoint, stop.clone());
    drop(endpoints);
    let source = WorkerSource {
        link,
        endpoint,
        upstream,
        workspace,
        session,
    };
    super::serve(listener, source, stop);
    Answer::Done
}

/// The daemon's [`Call::SidecarClose`]: the sidecar stops, its
/// connections cut.
pub(crate) fn close_handed(serving: &Serving, endpoint: u64) -> Answer {
    let stop = serving
        .endpoints
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&endpoint);
    if let Some(stop) = stop {
        stop.cancel();
    }
    Answer::Done
}

/// A sidecar's [`Source`] where a worker serves its session: the
/// daemon's channel to it, one round trip a call. Never retains the key
/// past the call that asked for it.
#[derive(Clone)]
struct WorkerSource {
    link: Arc<Link>,
    endpoint: u64,
    upstream: String,
    workspace: Option<String>,
    session: ObjectId,
}

impl Source for WorkerSource {
    async fn admit(&self, token: Option<&str>) -> Result<String, Refusal> {
        let call = Call::SidecarAdmit {
            endpoint: self.endpoint,
            token: token.map(str::to_string),
        };
        #[cfg(unix)]
        let answer = self.link.call(call, Vec::new()).await;
        #[cfg(windows)]
        let answer = self.link.call(call).await;
        match answer {
            // The zeroizing wrapper's job ends here, at the channel it
            // crossed to escape: `Source::admit` is a plain `String`
            // (it must compile with no `vault` feature at all), the same
            // shape `model_key` was already, unzeroized, before a worker
            // existed to ask for it over a channel.
            Ok(Answer::SidecarKey { key }) => Ok(key.0.to_string()),
            Ok(Answer::SidecarRefused { kind, message }) => Err(kind.into_refusal(message)),
            Ok(other) => Err(Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                format!("the device's daemon answered {other:?}"),
            )),
            Err(why) => Err(Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                format!("the device's daemon did not answer: {why}"),
            )),
        }
    }

    async fn forget_key(&self) {
        let call = Call::SidecarForgetKey {
            endpoint: self.endpoint,
        };
        #[cfg(unix)]
        let _ = self.link.call(call, Vec::new()).await;
        #[cfg(windows)]
        let _ = self.link.call(call).await;
    }

    fn upstream(&self) -> &str {
        &self.upstream
    }

    fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    fn session(&self) -> ObjectId {
        self.session
    }
}

#[cfg(test)]
mod tests;
