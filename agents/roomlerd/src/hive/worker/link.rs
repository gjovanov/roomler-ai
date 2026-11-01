// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-2 — one side's end of a worker's channel: what it writes
//! with and, with the vault (the only calls so far are its endpoints'),
//! calls either way, each a `call {id, …}` answered by the other side's
//! `answer {id, …}`, many in flight at once.
//!
//! | Piece | As built |
//! |---|---|
//! | the writer | one per side, behind a lock, so a frame is never written into another; a descriptor goes on the second channel before the frame that names it, under the same lock, so the Nth descriptor is always the Nth hand-over's |
//! | a call | an id from a counter, a waiter for its answer, and a bound on the wait ([`CALL_WAIT`]) |
//! | the end | every call still waiting fails at once, as the channel goes, and every later one at its start |
//! | lost | a call that cannot be sent, or that goes unanswered within its bound, takes the link for lost, with why ([`Linked::lost`]): the daemon's side then ends that worker's life, as a channel that failed ends it, rather than hand it what it can no longer serve |
//! | its role | P8-2d-0 — whose calls it carries ([`Role`]): a session's worker's ([`Link`]), or the device's ([`DeviceLink`], P8-2d-1). Each takes its own calls alone |

#[cfg(worker_calls)]
use std::collections::HashMap;
use std::marker::PhantomData;
#[cfg(all(worker_calls, unix))]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(worker_calls)]
use std::sync::Mutex;
#[cfg(worker_calls)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(worker_calls)]
use std::time::Duration;

use tokio::io::AsyncWrite;
#[cfg(all(worker_calls, unix))]
use tokio::net::UnixStream;
#[cfg(worker_calls)]
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[cfg(worker_calls)]
use super::{Answer, Call, DeviceAnswer, DeviceCall};
use super::{Msg, send};

/// FR-90 P8-2d-0 — whose calls a link carries: a session's worker's, every
/// call so far, or (P8-2d-1) the device's own worker's. Each role's calls
/// are a type of their own, and a link takes one role's alone, so a device's
/// call is never handed to a session's worker, nor a session's to the
/// device's.
pub(crate) trait Role: Send + Sync + 'static {
    #[cfg(worker_calls)]
    type Call: Send + 'static;
    #[cfg(worker_calls)]
    type Answer: Send + 'static;
    /// The frame a call of this role goes in.
    #[cfg(worker_calls)]
    fn call_frame(id: u64, call: Self::Call) -> Msg;
    /// The frame its answer goes in.
    #[cfg(worker_calls)]
    fn answer_frame(id: u64, answer: Self::Answer) -> Msg;
}

/// A session's worker: its endpoints, its egress fronts, its toolbelt and
/// its sidecar.
pub(crate) enum SessionRole {}

impl Role for SessionRole {
    #[cfg(worker_calls)]
    type Call = Call;
    #[cfg(worker_calls)]
    type Answer = Answer;
    #[cfg(worker_calls)]
    fn call_frame(id: u64, call: Call) -> Msg {
        Msg::Call { id, call }
    }
    #[cfg(worker_calls)]
    fn answer_frame(id: u64, answer: Answer) -> Msg {
        Msg::Answer { id, answer }
    }
}

/// FR-90 P8-2d-0 — the device's own worker: the replica store and stream,
/// the viewer's peer, the prober's measure (decision 63). P8-2d-1 starts it.
#[cfg(worker_calls)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum DeviceRole {}

#[cfg(worker_calls)]
impl Role for DeviceRole {
    type Call = DeviceCall;
    type Answer = DeviceAnswer;
    fn call_frame(id: u64, call: DeviceCall) -> Msg {
        Msg::DeviceCall { id, call }
    }
    fn answer_frame(id: u64, answer: DeviceAnswer) -> Msg {
        Msg::DeviceAnswer { id, answer }
    }
}

/// A session's worker's link: what every call so far goes on.
pub(crate) type Link = Linked<SessionRole>;

/// FR-90 P8-2d-0 — the device's worker's link, which P8-2d-1 makes.
#[cfg(worker_calls)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) type DeviceLink = Linked<DeviceRole>;

/// How long a call waits for its answer.
#[cfg(worker_calls)]
pub(crate) const CALL_WAIT: Duration = Duration::from_secs(10);

/// What one side writes with: its half of the channel and, the daemon's, the
/// second channel, the one descriptors go on. Windows has no second channel
/// (P8-2b-3): a listener rides inside the call's own frame there.
struct Writer {
    frames: Box<dyn AsyncWrite + Send + Unpin>,
    #[cfg(all(worker_calls, unix))]
    descriptors: Option<UnixStream>,
}

/// One side's end of a worker's channel, carrying `R`'s calls (P8-2d-0).
pub(crate) struct Linked<R: Role> {
    writer: tokio::sync::Mutex<Writer>,
    #[cfg(worker_calls)]
    waiting: Mutex<Option<HashMap<u64, oneshot::Sender<R::Answer>>>>,
    #[cfg(worker_calls)]
    next: AtomicU64,
    /// Cancelled as a call takes the link for lost, `why` set first.
    lost: CancellationToken,
    why: std::sync::Mutex<Option<String>>,
    role: PhantomData<fn() -> R>,
}

impl<R: Role> Linked<R> {
    /// The daemon's side: its writing half and (with the vault) the second
    /// channel.
    pub(crate) fn daemon(
        frames: impl AsyncWrite + Send + Unpin + 'static,
        #[cfg(unix)] descriptors: tokio::net::UnixStream,
    ) -> Self {
        #[cfg(all(unix, not(all(worker_calls, unix))))]
        drop(descriptors);
        Self {
            writer: tokio::sync::Mutex::new(Writer {
                frames: Box::new(frames),
                #[cfg(all(worker_calls, unix))]
                descriptors: Some(descriptors),
            }),
            #[cfg(worker_calls)]
            waiting: Mutex::new(Some(HashMap::new())),
            #[cfg(worker_calls)]
            next: AtomicU64::new(1),
            lost: CancellationToken::new(),
            why: std::sync::Mutex::new(None),
            role: PhantomData,
        }
    }

    /// A worker's side: it writes frames alone.
    pub(crate) fn worker(frames: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            writer: tokio::sync::Mutex::new(Writer {
                frames: Box::new(frames),
                #[cfg(all(worker_calls, unix))]
                descriptors: None,
            }),
            #[cfg(worker_calls)]
            waiting: Mutex::new(Some(HashMap::new())),
            #[cfg(worker_calls)]
            next: AtomicU64::new(1),
            lost: CancellationToken::new(),
            why: std::sync::Mutex::new(None),
            role: PhantomData,
        }
    }

    /// Why a call took the link for lost, once one has: never, while every
    /// call is sent and answered in time.
    pub(crate) async fn lost(&self) -> String {
        self.lost.cancelled().await;
        self.why
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
    }

    /// Take the link for lost: the first reason stays.
    #[cfg(worker_calls)]
    fn lose(&self, why: &str) {
        self.why
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(|| why.to_string());
        self.lost.cancel();
    }

    /// Write one frame.
    pub(crate) async fn send(&self, m: &Msg) -> std::io::Result<()> {
        send(&mut self.writer.lock().await.frames, m).await
    }

    /// A call's id, registered among those awaiting an answer: `None` where
    /// the channel is already gone.
    #[cfg(worker_calls)]
    fn register(&self) -> Option<(u64, oneshot::Receiver<R::Answer>)> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        waiting.as_mut()?.insert(id, tx);
        Some((id, rx))
    }

    /// Once a call's frame is sent (or failed): its answer, within
    /// [`CALL_WAIT`]. A failed send, or one unanswered in time, takes the
    /// link for lost.
    #[cfg(worker_calls)]
    async fn finish_call(
        &self,
        id: u64,
        rx: oneshot::Receiver<R::Answer>,
        sent: std::io::Result<()>,
    ) -> Result<R::Answer, String> {
        if let Err(e) = sent {
            self.forget(id);
            let why = format!("the worker's channel refused a call: {e}");
            self.lose(&why);
            return Err(why);
        }
        match tokio::time::timeout(CALL_WAIT, rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err("the worker's channel went before it answered".into()),
            Err(_) => {
                self.forget(id);
                let why = format!("a call went unanswered within {CALL_WAIT:?}");
                self.lose(&why);
                Err(why)
            }
        }
    }

    /// `call`, and its answer: an error once the channel is gone, and past
    /// [`CALL_WAIT`]. `descriptors` go on the second channel just before the
    /// call's frame, in order (the daemon's side alone has one). A call that
    /// cannot be sent, or goes unanswered within the bound, takes the link
    /// for lost.
    #[cfg(all(worker_calls, unix))]
    pub(crate) async fn call(
        &self,
        call: R::Call,
        descriptors: Vec<OwnedFd>,
    ) -> Result<R::Answer, String> {
        let Some((id, rx)) = self.register() else {
            return Err("the worker's channel is gone".into());
        };
        // A descriptor sent and its frame not would leave the next call's
        // frame naming it: the channel is gone then, and so is this link.
        let sent = {
            let mut w = self.writer.lock().await;
            let mut descriptors_sent = Ok(());
            for fd in &descriptors {
                descriptors_sent = match &w.descriptors {
                    Some(ch) => super::fds::send(ch, fd.as_raw_fd()).await,
                    None => Err(std::io::Error::other("no channel for descriptors here")),
                };
                if descriptors_sent.is_err() {
                    break;
                }
            }
            match descriptors_sent {
                Ok(()) => send(&mut w.frames, &R::call_frame(id, call)).await,
                Err(e) => Err(e),
            }
        };
        drop(descriptors);
        self.finish_call(id, rx, sent).await
    }

    /// FR-90 P8-2b-3 — [`call`](Self::call) on Windows: no second channel, so
    /// what a Unix call hands across as a descriptor rides inside `call`
    /// itself (a listener's `WSAPROTOCOL_INFOW`, in its `Spec`).
    #[cfg(all(worker_calls, windows))]
    pub(crate) async fn call(&self, call: R::Call) -> Result<R::Answer, String> {
        let Some((id, rx)) = self.register() else {
            return Err("the worker's channel is gone".into());
        };
        let sent = {
            let mut w = self.writer.lock().await;
            send(&mut w.frames, &R::call_frame(id, call)).await
        };
        self.finish_call(id, rx, sent).await
    }

    /// The answer to call `id`, as it arrives: `false` when nothing waits
    /// for it (it timed out, or was never made).
    #[cfg(worker_calls)]
    pub(crate) fn answered(&self, id: u64, answer: R::Answer) -> bool {
        let tx = self
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|w| w.remove(&id));
        tx.is_some_and(|tx| tx.send(answer).is_ok())
    }

    /// Answer the other side's call `id`.
    #[cfg(worker_calls)]
    pub(crate) async fn answer(&self, id: u64, answer: R::Answer) -> std::io::Result<()> {
        self.send(&R::answer_frame(id, answer)).await
    }

    /// The channel is gone: every call still waiting fails now, and every
    /// later one at its start.
    pub(crate) fn end(&self) {
        // Dropping each waiter's sender is what fails it.
        #[cfg(worker_calls)]
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    #[cfg(worker_calls)]
    fn forget(&self, id: u64) {
        if let Some(w) = self
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            w.remove(&id);
        }
    }
}

/// Unix: a real end-to-end harness over `UnixStream`, descriptors included.
/// Windows has no in-process stand-in yet (P8-2b-3, as `worker.rs`'s
/// `run_in_process` does not either); it leans on the real-binary tests.
#[cfg(all(test, worker_calls, unix))]
mod tests;
