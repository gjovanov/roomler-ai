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
//! | lost | a call that cannot be sent, or that goes unanswered within its bound, takes the link for lost, with why ([`Link::lost`]): the daemon's side then ends that worker's life, as a channel that failed ends it, rather than hand it what it can no longer serve |

#[cfg(feature = "vault")]
use std::collections::HashMap;
#[cfg(feature = "vault")]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(feature = "vault")]
use std::sync::Mutex;
#[cfg(feature = "vault")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "vault")]
use std::time::Duration;

#[cfg(feature = "vault")]
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
#[cfg(feature = "vault")]
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "vault")]
use super::{Answer, Call};
use super::{Msg, send};

/// How long a call waits for its answer.
#[cfg(feature = "vault")]
pub(crate) const CALL_WAIT: Duration = Duration::from_secs(10);

/// What one side writes with: its half of the channel and, the daemon's,
/// the second channel, the one descriptors go on.
struct Writer {
    frames: OwnedWriteHalf,
    #[cfg(feature = "vault")]
    descriptors: Option<UnixStream>,
}

/// One side's end of a worker's channel.
pub(crate) struct Link {
    writer: tokio::sync::Mutex<Writer>,
    #[cfg(feature = "vault")]
    waiting: Mutex<Option<HashMap<u64, oneshot::Sender<Answer>>>>,
    #[cfg(feature = "vault")]
    next: AtomicU64,
    /// Cancelled as a call takes the link for lost, `why` set first.
    lost: CancellationToken,
    why: std::sync::Mutex<Option<String>>,
}

impl Link {
    /// The daemon's side: its writing half and (with the vault) the second
    /// channel.
    pub(crate) fn daemon(frames: OwnedWriteHalf, descriptors: tokio::net::UnixStream) -> Self {
        #[cfg(not(feature = "vault"))]
        drop(descriptors);
        Self {
            writer: tokio::sync::Mutex::new(Writer {
                frames,
                #[cfg(feature = "vault")]
                descriptors: Some(descriptors),
            }),
            #[cfg(feature = "vault")]
            waiting: Mutex::new(Some(HashMap::new())),
            #[cfg(feature = "vault")]
            next: AtomicU64::new(1),
            lost: CancellationToken::new(),
            why: std::sync::Mutex::new(None),
        }
    }

    /// A worker's side: it writes frames alone.
    pub(crate) fn worker(frames: OwnedWriteHalf) -> Self {
        Self {
            writer: tokio::sync::Mutex::new(Writer {
                frames,
                #[cfg(feature = "vault")]
                descriptors: None,
            }),
            #[cfg(feature = "vault")]
            waiting: Mutex::new(Some(HashMap::new())),
            #[cfg(feature = "vault")]
            next: AtomicU64::new(1),
            lost: CancellationToken::new(),
            why: std::sync::Mutex::new(None),
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
    #[cfg(feature = "vault")]
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

    /// `call`, and its answer: an error once the channel is gone, and past
    /// [`CALL_WAIT`]. `descriptors` go on the second channel just before the
    /// call's frame, in order (the daemon's side alone has one). A call that
    /// cannot be sent, or goes unanswered within the bound, takes the link
    /// for lost.
    #[cfg(feature = "vault")]
    pub(crate) async fn call(
        &self,
        call: Call,
        descriptors: Vec<OwnedFd>,
    ) -> Result<Answer, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
            let Some(waiting) = waiting.as_mut() else {
                return Err("the worker's channel is gone".into());
            };
            waiting.insert(id, tx);
        }
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
                Ok(()) => send(&mut w.frames, &Msg::Call { id, call }).await,
                Err(e) => Err(e),
            }
        };
        drop(descriptors);
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

    /// The answer to call `id`, as it arrives: `false` when nothing waits
    /// for it (it timed out, or was never made).
    #[cfg(feature = "vault")]
    pub(crate) fn answered(&self, id: u64, answer: Answer) -> bool {
        let tx = self
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|w| w.remove(&id));
        tx.is_some_and(|tx| tx.send(answer).is_ok())
    }

    /// Answer the other side's call `id`.
    #[cfg(feature = "vault")]
    pub(crate) async fn answer(&self, id: u64, answer: Answer) -> std::io::Result<()> {
        self.send(&Msg::Answer { id, answer }).await
    }

    /// The channel is gone: every call still waiting fails now, and every
    /// later one at its start.
    pub(crate) fn end(&self) {
        // Dropping each waiter's sender is what fails it.
        #[cfg(feature = "vault")]
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    #[cfg(feature = "vault")]
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

#[cfg(all(test, feature = "vault"))]
mod tests;
