// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The replica store's one writer: a thread that owns the SQLite connection.
//!
//! SQLite writes block, and a session's stdout reader must never stall the
//! runtime on one; one thread owning the connection also gives the chain its
//! one-writer rule for free — every append reads the tip and extends it inside
//! the store's own transaction, in the order the sessions sent them.
//!
//! P0d-2 reads go through the same thread (a viewer's page, a grant's "do I
//! hold this session"), answered on a oneshot so an async caller awaits
//! without blocking the runtime. And every append that lands is PUBLISHED to
//! the live feed: the writer is the only place that knows the `seq` an event
//! got, so it is the only place a viewer can learn it from without a gap.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;

use roomler_hive_node::store::Store;
use roomler_hive_node::{EventEnvelope, TranscriptEvent};
use tokio::sync::{broadcast, oneshot};
use tracing::warn;

/// Appended events buffered per subscriber before a slow one lags. A lagging
/// viewer re-reads the store from its last `seq`, so nothing is lost by
/// dropping here — only re-fetched.
const FEED_CAPACITY: usize = 1024;

enum Cmd {
    Append {
        session: String,
        fence: u64,
        event: TranscriptEvent,
    },
    Page {
        session: String,
        after: u64,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<EventEnvelope>, String>>,
    },
    Tip {
        session: String,
        reply: oneshot::Sender<Option<u64>>,
    },
    #[cfg(test)]
    Events {
        session: String,
        reply: mpsc::Sender<Vec<TranscriptEvent>>,
    },
}

/// The writer's address. Cheap to clone; the thread ends when every handle
/// is gone.
#[derive(Clone)]
pub(crate) struct StoreHandle {
    tx: mpsc::Sender<Cmd>,
    feed: broadcast::Sender<Arc<EventEnvelope>>,
}

impl StoreHandle {
    /// Open the store at `path` (`None` = in memory) and start its writer.
    pub(crate) fn spawn(path: Option<&Path>) -> Result<Self, String> {
        let store = match path {
            Some(p) => Store::open(p),
            None => Store::open_in_memory(),
        }
        .map_err(|e| format!("opening the replica store: {e}"))?;
        let (tx, rx) = mpsc::channel();
        let (feed, _) = broadcast::channel(FEED_CAPACITY);
        let publish = feed.clone();
        std::thread::Builder::new()
            .name("hive-store".into())
            .spawn(move || writer(store, rx, publish))
            .map_err(|e| format!("starting the replica store writer: {e}"))?;
        Ok(Self { tx, feed })
    }

    /// Append `event` to `session`'s chain under `fence`. Never blocks: the
    /// writer orders and applies it.
    pub(crate) fn append(&self, session: &str, fence: u64, event: TranscriptEvent) {
        let _ = self.tx.send(Cmd::Append {
            session: session.to_string(),
            fence,
            event,
        });
    }

    /// Events of `session` after `after`, oldest first, at most `limit`.
    pub(crate) async fn page(
        &self,
        session: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Page {
                session: session.to_string(),
                after,
                limit,
                reply,
            })
            .map_err(|_| "the replica store is closed".to_string())?;
        rx.await
            .map_err(|_| "the replica store did not answer".to_string())?
    }

    /// The newest `seq` this store holds for `session`; `None` = nothing.
    pub(crate) async fn tip(&self, session: &str) -> Option<u64> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Tip {
                session: session.to_string(),
                reply,
            })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Every event appended from now on, of every session — a subscriber
    /// filters for its own. Subscribe BEFORE reading the store, so an event
    /// that lands in between is in one or the other.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<EventEnvelope>> {
        self.feed.subscribe()
    }

    /// Everything recorded for `session`, in order — tests only.
    #[cfg(test)]
    pub(crate) fn events(&self, session: &str) -> Vec<TranscriptEvent> {
        let (reply, rx) = mpsc::channel();
        let _ = self.tx.send(Cmd::Events {
            session: session.to_string(),
            reply,
        });
        rx.recv().unwrap_or_default()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn writer(
    mut store: Store,
    rx: mpsc::Receiver<Cmd>,
    publish: broadcast::Sender<Arc<EventEnvelope>>,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Append {
                session,
                fence,
                event,
            } => {
                let tip = match store.tip(&session) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(%session, %e, "hive: replica store tip unreadable — event dropped");
                        continue;
                    }
                };
                let env = EventEnvelope::next(&session, tip, fence, now_ms(), &event);
                match store.append(&env) {
                    // No subscriber is not an error: nobody is watching.
                    Ok(_) => {
                        let _ = publish.send(Arc::new(env));
                    }
                    Err(e) => warn!(%session, %e, "hive: replica store refused an event"),
                }
            }
            Cmd::Page {
                session,
                after,
                limit,
                reply,
            } => {
                let page = store
                    .page(&session, after, limit)
                    .map_err(|e| format!("reading the replica store: {e}"));
                let _ = reply.send(page);
            }
            Cmd::Tip { session, reply } => {
                let _ = reply.send(store.tip(&session).ok().flatten().map(|t| t.seq));
            }
            #[cfg(test)]
            Cmd::Events { session, reply } => {
                let events = store
                    .page(&session, 0, 100_000)
                    .map(|envs| envs.iter().filter_map(EventEnvelope::event).collect())
                    .unwrap_or_default();
                let _ = reply.send(events);
            }
        }
    }
}
