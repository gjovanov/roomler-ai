// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The replica store's one writer: a thread that owns the SQLite connection.
//!
//! SQLite writes block, and a session's stdout reader must never stall the
//! runtime on one; one thread owning the connection also gives the chain its
//! one-writer rule for free — every append reads the tip and extends it inside
//! the store's own transaction, in the order the sessions sent them.

use std::path::Path;
use std::sync::mpsc;

use roomler_hive_node::store::Store;
use roomler_hive_node::{EventEnvelope, TranscriptEvent};
use tracing::warn;

enum Cmd {
    Append {
        session: String,
        fence: u64,
        event: TranscriptEvent,
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
        std::thread::Builder::new()
            .name("hive-store".into())
            .spawn(move || writer(store, rx))
            .map_err(|e| format!("starting the replica store writer: {e}"))?;
        Ok(Self { tx })
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

    /// Everything recorded for `session`, in order — tests only (the viewer
    /// peer that pages a transcript for a browser is P0d).
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

fn writer(mut store: Store, rx: mpsc::Receiver<Cmd>) {
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
                if let Err(e) = store.append(&env) {
                    warn!(%session, %e, "hive: replica store refused an event");
                }
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
