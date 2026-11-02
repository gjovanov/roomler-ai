// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2c-2 — a session's toolbelt, served by its worker.
//!
//! Mirrors `vault_connect::handed`'s own shape (`docs/fr/FR-90-hive-agent-
//! sessions.md` §3's P8-0 point 2): the daemon keeps the socket it bound and
//! hands a copy of its listener to the worker, which runs the real MCP
//! dispatch — [`super::serve`] and [`super::connection`], UNCHANGED — against
//! it.
//!
//! | Piece | As built |
//! |---|---|
//! | what stays the daemon's | the listener it bound (`toolbelt::open`'s socket); the session's open approvals, as a daemon-side [`Pending`](super::Pending) that forwards resolutions instead of holding the real wait |
//! | what the worker is handed | a copy of the listener (`SCM_RIGHTS`); nothing else — the worker builds its own [`Ctx`](super::Ctx), with every trait field `None` (P8-2c's remaining scope: `vault`, `knowhow`, `exec`, `net`, `ssh` are not yet forwarded, so none of their tools are offered from a worker-hosted session yet) |
//! | an approval opening | the worker's own [`Pending`](super::Pending) (a real, local wait) plus one fast, bounded call to the daemon ([`Call::ToolbeltOpen`]) so it can record it and tell viewers, exactly as it does for one opened in the daemon itself |
//! | an approval closing | the worker's own word ([`Call::ToolbeltClosed`]), however it ended — the worker alone knows, since it alone holds the wait |
//! | a driver's answer, or a withdrawal | the daemon's [`Pending`] forwards it to the worker ([`Call::ToolbeltAnswer`] / [`Call::ToolbeltWithdraw`] / [`Call::ToolbeltWithdrawAll`]), which resolves its own real wait |
//! | a new worker | the toolbelt is handed again, on the same listener: a connection (and any approval mid-flight on it) the lost worker held is gone, the socket is not |
//! | the first worker | P8-2c-4 — kept as the run launches, before its worker starts, where the device runs workers (`hive_worker`); handed by the worker's own first `rehand` as it comes up, a connection waiting in the listener's backlog till then |
//! | `hive_worker` off | `toolbelt::open` serves it in the daemon itself, exactly as before this step |
//!
//! ⚠️ **Known gap, left open rather than silently patched over**: an
//! approval open when its worker dies has no [`Call::ToolbeltClosed`] to
//! close it — the task that would send it died with the worker. The
//! daemon's `Pending` is left holding a [`Slot::Remote`](super::Slot) that
//! answers and expiries can no longer reach, until the session itself ends
//! (`withdraw_all`). `hive_worker` stays off by default and no release ships
//! the vault, so nothing live is exposed to this yet; closing it is a
//! follow-up, not this step's.
//!
//! ⚠️ Unix only so far, like P8-2a's own first cut: Windows has no
//! equivalent of `SCM_RIGHTS` for a Unix-domain-socket listener (the
//! toolbelt's is one, never a loopback TCP port), so crossing it needs a
//! `DuplicateHandle`-based primitive this build does not have yet — a
//! distinct piece of new machinery, not a rename of `WSADuplicateSocketW`'s
//! own. Left for its own step.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, LazyLock, Mutex};

use bson::oid::ObjectId;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{ApprovalEvent, Ctx, Pending};
use crate::hive::vault_connect::handed::{Answer, Call};
use crate::hive::worker::link::Link;

// ─── The daemon's side ──────────────────────────────────────────────────────

/// A toolbelt handed to its run's worker, as the daemon keeps it. One a
/// run, unlike vault_connect's many endpoints.
struct Kept {
    endpoint: u64,
    peer: u32,
    /// The daemon's own copy of the listener, handed again to each worker
    /// that follows.
    listener: OwnedFd,
    events: mpsc::Sender<ApprovalEvent>,
    pending: Arc<Pending>,
}

type Runs = HashMap<(ObjectId, u64), Kept>;

static KEPT: LazyLock<Mutex<Runs>> = LazyLock::new(Mutex::default);

fn kept() -> std::sync::MutexGuard<'static, Runs> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

/// FR-90 P8-2a-2's own counter, shared: no two things a worker serves share
/// one endpoint id.
fn next_endpoint() -> u64 {
    crate::hive::vault_connect::handed::next_endpoint()
}

/// Hand `listener` (already bound, owned and permissioned by the caller —
/// `toolbelt::open`) to `session` at `fence`'s worker: `None` where the run
/// gets none (`in_worker` false: the caller serves it in the daemon
/// instead), a `Pending` that forwards to the worker otherwise — the SAME
/// one this module uses to note what the worker reports open, so
/// [`Pending::ids`] and a double-answer's refusal stay correct.
///
/// FR-90 P8-2c-4 — `in_worker` is the device's own setting (`hive_worker`),
/// the one `launch` hands `worker::run_started`. This runs inside `spawn`,
/// BEFORE that starts the worker, so whether a worker exists yet
/// (`worker::has`) is false at every fresh launch: asked that, it handed
/// nothing, ever.
///
/// Synchronous — `toolbelt::open` is, and its own callers up to `spawn`,
/// `launch` are too — so the actual hand-over (which must wait on a
/// starting worker, then make one round trip) is spawned, not awaited: this
/// records the toolbelt as kept and returns a `Pending` that already
/// forwards correctly (it looks its worker up fresh at every resolution,
/// never a link cached from here) before that spawned call has even run.
/// On a fresh launch the hand-over that counts is the worker's own first
/// [`rehand`], as it comes up; the spawned one finds no worker yet, or the
/// same one, and a hand-over answered twice is served once.
pub(crate) fn prepare(
    session: ObjectId,
    fence: u64,
    peer: u32,
    listener: &UnixListener,
    events: mpsc::Sender<ApprovalEvent>,
    in_worker: bool,
) -> Result<Option<(Arc<Pending>, u64)>, String> {
    if !in_worker {
        return Ok(None);
    }
    let failed = |e: std::io::Error| format!("the toolbelt's listener: {e}");
    let ours = listener.as_fd().try_clone_to_owned().map_err(failed)?;
    let endpoint = next_endpoint();
    let pending = Arc::new(Pending::remote(session, fence, endpoint));
    kept().insert(
        (session, fence),
        Kept {
            endpoint,
            peer,
            listener: ours,
            events,
            pending: Arc::clone(&pending),
        },
    );
    tokio::spawn(hand_over(session, fence));
    Ok(Some((pending, endpoint)))
}

/// The actual hand-over [`prepare`] spawns: waits for a starting worker
/// (one may not have answered its `Hello` yet), then the one round trip.
/// Mirrors `vault_connect::handed::hand`'s own wait-then-call.
async fn hand_over(session: ObjectId, fence: u64) {
    let Some(link) = crate::hive::worker::up(session, fence).await else {
        debug!(%session, fence, "hive: the toolbelt's worker never came up: it serves nothing until the next one does");
        return;
    };
    let found = kept().get(&(session, fence)).and_then(|k| {
        k.listener
            .try_clone()
            .ok()
            .map(|fd| (k.endpoint, k.peer, fd))
    });
    let Some((endpoint, peer, fd)) = found else {
        return; // the toolbelt was forgotten (the session ended) first
    };
    let answer = link
        .call(
            Call::ToolbeltServe {
                endpoint,
                session,
                fence,
                peer,
            },
            vec![fd],
        )
        .await;
    match answer {
        Ok(Answer::Done) => {}
        other => {
            debug!(%session, fence, endpoint, answer = ?other, "hive: the toolbelt's hand-over went unanswered: the next worker (or a later rehand) serves it");
        }
    }
}

/// The toolbelt handed to the run's worker so far, handed to `link`'s: the
/// worker that follows serves what the lost one did. Mirrors
/// `vault_connect::handed::rehand`.
pub(crate) async fn rehand(session: ObjectId, fence: u64, link: &Link) {
    let found = kept().get(&(session, fence)).and_then(|k| {
        k.listener
            .try_clone()
            .ok()
            .map(|fd| (k.endpoint, k.peer, fd))
    });
    let Some((endpoint, peer, fd)) = found else {
        return;
    };
    let answer = link
        .call(
            Call::ToolbeltServe {
                endpoint,
                session,
                fence,
                peer,
            },
            vec![fd],
        )
        .await;
    match answer {
        Ok(Answer::Done) => {}
        other => {
            warn!(%session, fence, endpoint, answer = ?other, "hive: the toolbelt was not handed to the session's new worker");
        }
    }
}

/// A test's: whether this run's toolbelt is kept for hand-over now.
#[cfg(test)]
pub(crate) fn is_kept(session: ObjectId, fence: u64) -> bool {
    kept().contains_key(&(session, fence))
}

/// A test's: keep a toolbelt for (`session`, `fence`) directly, without a
/// real listener or a real worker — `opened`/`closed`'s own dispatch is
/// what these tests exercise, not `prepare`'s worker-gate or its listener
/// plumbing.
#[cfg(test)]
pub(crate) fn keep_for_test(
    session: ObjectId,
    fence: u64,
    endpoint: u64,
    events: mpsc::Sender<ApprovalEvent>,
) -> Arc<Pending> {
    let pending = Arc::new(Pending::remote(session, fence, endpoint));
    // A `UnixListener` bound under a fresh, genuinely unique temp directory
    // (RAII-cleaned on drop) purely so `Kept` has a real `OwnedFd` to hold;
    // this test never dials or serves it. `next_endpoint()`'s own counter
    // resets every process, so naming by it alone collided with a stale
    // socket a PRIOR `cargo test` invocation never cleaned up (#AddrInUse);
    // `tempfile::tempdir()` is what this crate's own toolbelt tests already
    // use for exactly this reason.
    let dir = tempfile::tempdir().expect("a temp dir for a test socket");
    let listener = UnixListener::bind(dir.path().join("test.sock")).expect("a test socket binds");
    // The clone keeps the fd valid once `listener` and `dir` drop at this
    // function's end — a kernel fd outlives the name that created it.
    let fd = listener.as_fd().try_clone_to_owned().expect("dup");
    kept().insert(
        (session, fence),
        Kept {
            endpoint,
            peer: 0,
            listener: fd,
            events,
            pending: Arc::clone(&pending),
        },
    );
    pending
}

/// The toolbelt is gone: forgotten, so a later `rehand` finds nothing.
pub(crate) fn forget(session: ObjectId, fence: u64) {
    kept().remove(&(session, fence));
}

/// The daemon's answer to a worker's [`Call::ToolbeltOpen`]: record the
/// approval and tell the session's viewers, exactly as one opened in the
/// daemon itself.
pub(crate) async fn opened(
    session: ObjectId,
    fence: u64,
    endpoint: u64,
    id: String,
    tool_name: String,
    tool_use_id: Option<String>,
    input: serde_json::Value,
) -> Answer {
    let found = {
        let runs = kept();
        runs.get(&(session, fence)).and_then(|k| {
            if k.endpoint != endpoint {
                return None;
            }
            k.pending.note_remote_open(&id);
            Some((k.events.clone(), k.pending.ids().contains(&id)))
        })
    };
    let Some((events, noted)) = found else {
        return Answer::Refused {
            why: "no such toolbelt".into(),
        };
    };
    if !noted {
        return Answer::Refused {
            why: "too many approvals open already".into(),
        };
    }
    if events
        .send(ApprovalEvent::Opened {
            id,
            tool_name,
            tool_use_id,
            input,
        })
        .await
        .is_err()
    {
        return Answer::Refused {
            why: "the session's own task is gone".into(),
        };
    }
    Answer::Done
}

/// The daemon's answer to a worker's [`Call::ToolbeltClosed`].
pub(crate) async fn closed(
    session: ObjectId,
    fence: u64,
    endpoint: u64,
    id: String,
    ended: super::Ended,
) -> Answer {
    let events = kept()
        .get(&(session, fence))
        .filter(|k| k.endpoint == endpoint)
        .map(|k| k.events.clone());
    let Some(events) = events else {
        return Answer::Refused {
            why: "no such toolbelt".into(),
        };
    };
    let _ = events.send(ApprovalEvent::Closed { id, ended }).await;
    Answer::Done
}

// ─── The worker's side ──────────────────────────────────────────────────────

/// The toolbelts one worker serves, each one's stop a child of `all`, and
/// its own `Pending` — the real wait, so the daemon's forwarded
/// [`Call::ToolbeltAnswer`]/[`Call::ToolbeltWithdraw`]/
/// [`Call::ToolbeltWithdrawAll`] can reach it.
#[derive(Default)]
pub(crate) struct Serving {
    endpoints: Mutex<HashMap<u64, (CancellationToken, Arc<Pending>)>>,
    pub(crate) all: CancellationToken,
}

impl Serving {
    /// A test's: claim `endpoint` with `pending` directly, without a real
    /// listener — `answered`/`withdrawn`/`withdrawn_all`/`close_handed`'s
    /// own dispatch is what these tests exercise, not `serve_handed`'s
    /// listener plumbing (already covered by the real-worker tests this
    /// arc's other steps use).
    #[cfg(test)]
    fn claim_for_test(&self, endpoint: u64, pending: Arc<Pending>) -> CancellationToken {
        let stop = self.all.child_token();
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(endpoint, (stop.clone(), pending));
        stop
    }

    fn pending(&self, endpoint: u64) -> Option<Arc<Pending>> {
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&endpoint)
            .map(|(_, p)| Arc::clone(p))
    }
}

/// The worker's answer to the daemon's [`Call::ToolbeltServe`]: build this
/// run's [`Ctx`] (every trait field `None` — P8-2c's remaining scope) and
/// serve the handed listener with the toolbelt's own, unchanged
/// [`super::serve`]/[`super::connection`].
pub(crate) fn serve_handed(
    serving: &Arc<Serving>,
    link: Arc<Link>,
    session: ObjectId,
    fence: u64,
    peer: u32,
    endpoint: u64,
    listener: OwnedFd,
) -> Answer {
    if serving
        .endpoints
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&endpoint)
    {
        // A hand-over answered twice (the first already claimed it).
        return Answer::Done;
    }
    let std = std::os::unix::net::UnixListener::from(listener);
    let listener = match std
        .set_nonblocking(true)
        .and_then(|()| UnixListener::from_std(std))
    {
        Ok(l) => l,
        Err(e) => {
            return Answer::Refused {
                why: format!("the toolbelt's listener: {e}"),
            };
        }
    };
    let (events_tx, mut events_rx) = mpsc::channel::<ApprovalEvent>(8);
    let pending = Arc::new(Pending::default());
    let stop = {
        let mut endpoints = serving.endpoints.lock().unwrap_or_else(|e| e.into_inner());
        if endpoints.contains_key(&endpoint) {
            // Lost the race with another answer of the same hand-over.
            return Answer::Done;
        }
        let stop = serving.all.child_token();
        endpoints.insert(endpoint, (stop.clone(), Arc::clone(&pending)));
        stop
    };
    let ctx = Arc::new(Ctx::worker_hosted(
        session,
        fence,
        peer,
        Arc::clone(&pending),
        events_tx,
    ));
    // Forwards this run's approval events to the daemon, one fast, bounded
    // call at a time: the real wait never crosses, only its opening and its
    // outcome.
    {
        let link = Arc::clone(&link);
        tokio::spawn(async move {
            while let Some(ev) = events_rx.recv().await {
                let call = match ev {
                    ApprovalEvent::Opened {
                        id,
                        tool_name,
                        tool_use_id,
                        input,
                    } => Call::ToolbeltOpen {
                        endpoint,
                        id,
                        tool_name,
                        tool_use_id,
                        input,
                    },
                    ApprovalEvent::Closed { id, ended } => Call::ToolbeltClosed {
                        endpoint,
                        id,
                        ended,
                    },
                };
                if let Err(why) = link.call(call, Vec::new()).await {
                    debug!(%session, fence, %why, "hive: a toolbelt approval event's forward to the daemon went unanswered");
                }
            }
        });
    }
    {
        let serving = Arc::clone(serving);
        let stop_child = stop.clone();
        tokio::spawn(async move {
            super::serve(listener, ctx, stop_child).await;
            serving
                .endpoints
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&endpoint);
        });
    }
    Answer::Done
}

/// The daemon's [`Call::ToolbeltClose`]: the toolbelt stops, its connections
/// cut.
pub(crate) fn close_handed(serving: &Serving, endpoint: u64) -> Answer {
    let stop = serving
        .endpoints
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&endpoint)
        .map(|(stop, _)| stop);
    if let Some(stop) = stop {
        stop.cancel();
    }
    Answer::Done
}

/// The daemon's [`Call::ToolbeltAnswer`]: the toolbelt's own `Pending`
/// resolves the real wait.
pub(crate) fn answered(
    serving: &Serving,
    endpoint: u64,
    id: &str,
    answer: super::Answer,
) -> Answer {
    match serving.pending(endpoint) {
        Some(pending) if pending.answer(id, answer) => Answer::Done,
        Some(_) => Answer::Refused {
            why: "that approval is not waiting (answered, expired or withdrawn)".into(),
        },
        None => Answer::Refused {
            why: "no such toolbelt here".into(),
        },
    }
}

/// The daemon's [`Call::ToolbeltWithdraw`].
pub(crate) fn withdrawn(serving: &Serving, endpoint: u64, id: &str) -> Answer {
    match serving.pending(endpoint) {
        Some(pending) => {
            pending.withdraw(id);
            Answer::Done
        }
        None => Answer::Refused {
            why: "no such toolbelt here".into(),
        },
    }
}

/// The daemon's [`Call::ToolbeltWithdrawAll`].
pub(crate) fn withdrawn_all(serving: &Serving, endpoint: u64) -> Answer {
    match serving.pending(endpoint) {
        Some(pending) => {
            pending.withdraw_all();
            Answer::Done
        }
        None => Answer::Refused {
            why: "no such toolbelt here".into(),
        },
    }
}

#[cfg(test)]
mod tests;
