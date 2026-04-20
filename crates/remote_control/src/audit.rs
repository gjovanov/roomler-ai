//! Audit log writer.
//!
//! Audit writes must never block a session-control path. We fan them through
//! a bounded mpsc channel with a background flusher. Backpressure: if the
//! channel is full, we drop the event and log a warning — better than
//! stalling input forwarding.

use bson::{DateTime, oid::ObjectId};
use mongodb::{Collection, Database};
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::models::{AuditKind, RemoteAuditEvent};

const AUDIT_BUFFER: usize = 4096;

#[derive(Clone)]
pub struct AuditSink {
    tx: mpsc::Sender<RemoteAuditEvent>,
}

impl AuditSink {
    /// Spawns the background flusher. Drop the returned JoinHandle on shutdown.
    pub fn spawn(db: Database) -> (Self, tokio::task::JoinHandle<()>) {
        let coll: Collection<RemoteAuditEvent> = db.collection(RemoteAuditEvent::COLLECTION);
        let (tx, mut rx) = mpsc::channel::<RemoteAuditEvent>(AUDIT_BUFFER);

        let handle = tokio::spawn(async move {
            // Batch up to 64 events or 200ms, whichever first.
            let mut buf = Vec::with_capacity(64);
            loop {
                let timeout = tokio::time::sleep(std::time::Duration::from_millis(200));
                tokio::pin!(timeout);

                tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(ev) => {
                            buf.push(ev);
                            // drain anything else available without awaiting
                            while let Ok(more) = rx.try_recv() {
                                buf.push(more);
                                if buf.len() >= 64 { break; }
                            }
                            if buf.len() >= 64 {
                                flush(&coll, &mut buf).await;
                            }
                        }
                        None => {
                            // channel closed
                            if !buf.is_empty() { flush(&coll, &mut buf).await; }
                            break;
                        }
                    },
                    _ = &mut timeout => {
                        if !buf.is_empty() { flush(&coll, &mut buf).await; }
                    }
                }
            }
        });

        (Self { tx }, handle)
    }

    pub fn record(
        &self,
        session_id: ObjectId,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        event: AuditKind,
    ) {
        let ev = RemoteAuditEvent {
            id: None,
            session_id,
            agent_id,
            tenant_id,
            at: DateTime::now(),
            event,
        };
        if let Err(e) = self.tx.try_send(ev) {
            warn!("audit channel full, dropping event: {e}");
        }
    }
}

async fn flush(coll: &Collection<RemoteAuditEvent>, buf: &mut Vec<RemoteAuditEvent>) {
    if let Err(e) = coll.insert_many(buf.drain(..)).await {
        error!("audit insert_many failed: {e}");
    }
}
