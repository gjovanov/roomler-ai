//! roomler (FR-148 P0): the association's acknowledgement clock, counted where
//! it happens and read without the association's lock.
//!
//! Two things are counted. The bytes the peer acknowledged it received, in the
//! SACK handler, over every stream: that is the clock a delivery rate is read from,
//! not the write call, which returns once SCTP has queued the bytes. And the
//! moments the queue of written, not yet sent chunks fills and runs empty, in
//! the pending queue: a reader learns from them whether the sender had more
//! to send than the path took for a whole interval, or ran out.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use portable_atomic::AtomicU64;

/// The counters, kept by the pending queue (fills and empties) and by the
/// SACK handler (acknowledged bytes, and the loss mark that decides them).
#[derive(Debug, Default)]
pub(crate) struct DeliveryCounters {
    acked_bytes: AtomicU64,
    queue_fills: AtomicU64,
    queue_empties: AtomicU64,
    /// [`Self::loss_mark`], plus one; 0 is none.
    loss_mark: AtomicU64,
}

impl DeliveryCounters {
    /// `n` bytes of user data that one SACK newly acknowledged the peer
    /// received, by its cumulative point and its gap blocks, over every
    /// stream.
    pub(crate) fn on_acked(&self, n: u64) {
        self.acked_bytes.fetch_add(n, Ordering::SeqCst);
    }

    /// The SACK handler's evidence of loss: every chunk still outstanding
    /// below this TSN was reported missing by a SACK (a gap block above it)
    /// or was outstanding when T3-rtx expired. An abandoned chunk below it
    /// that the cumulative point passes is not counted: a FORWARD TSN may
    /// have skipped it. Kept by the association under its lock; no reader
    /// of the clock sees it.
    pub(crate) fn loss_mark(&self) -> Option<u32> {
        match self.loss_mark.load(Ordering::SeqCst) {
            0 => None,
            m => u32::try_from(m - 1).ok(),
        }
    }

    pub(crate) fn set_loss_mark(&self, mark: Option<u32>) {
        let m = mark.map_or(0, |m| u64::from(m) + 1);
        self.loss_mark.store(m, Ordering::SeqCst);
    }

    /// The pending queue went from empty to holding bytes.
    pub(crate) fn on_queue_filled(&self) {
        self.queue_fills.fetch_add(1, Ordering::SeqCst);
    }

    /// The pending queue went from holding bytes to empty.
    pub(crate) fn on_queue_emptied(&self) {
        self.queue_empties.fetch_add(1, Ordering::SeqCst);
    }
}

/// One reading of an association's delivery counters. Every field counts from
/// the association's start, so two readings give an interval.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    /// Bytes of user data the peer acknowledged it received, each chunk once,
    /// over every stream: by a gap block, or by the cumulative point. A chunk
    /// abandoned under partial reliability that a SACK reported missing, or
    /// that was outstanding when T3-rtx expired, is left out when the
    /// cumulative point passes it, as a FORWARD TSN may have skipped it. One
    /// lost with no such evidence still counts: SCTP does not tell a sender
    /// which skipped chunks arrived.
    pub acked_bytes: u64,
    /// Times the queue of written, not yet sent chunks went from empty to not.
    pub queue_fills: u64,
    /// Times it went from not empty to empty.
    pub queue_empties: u64,
}

impl Delivery {
    /// Whether the queue of written, not yet sent chunks was empty when this
    /// was read. A transition racing the read shows a moment late.
    pub fn queue_empty(&self) -> bool {
        self.queue_fills <= self.queue_empties
    }
}

/// A handle onto one association's [`Delivery`] counters. It keeps the
/// counters alive, and nothing else of the association.
#[derive(Debug, Clone)]
pub struct DeliveryClock(Arc<DeliveryCounters>);

impl DeliveryClock {
    pub(crate) fn new(counters: Arc<DeliveryCounters>) -> Self {
        Self(counters)
    }

    /// The counters now. Lock-free: three loads.
    pub fn read(&self) -> Delivery {
        // Empties before fills: a fill and an empty racing the two loads then
        // read as not empty, which is what the queue was a moment later.
        let queue_empties = self.0.queue_empties.load(Ordering::SeqCst);
        let queue_fills = self.0.queue_fills.load(Ordering::SeqCst);
        let acked_bytes = self.0.acked_bytes.load(Ordering::SeqCst);
        Delivery {
            acked_bytes,
            queue_fills,
            queue_empties,
        }
    }

    /// Whether `self` and `other` read the same association.
    pub fn same(&self, other: &DeliveryClock) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
