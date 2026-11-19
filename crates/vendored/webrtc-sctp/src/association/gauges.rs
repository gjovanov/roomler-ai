//! roomler (FR-148 P0): the association's congestion state, stored where it
//! changes and read without the association's lock.
//!
//! The acknowledgement clock (`delivery.rs`) says what the peer received.
//! These say why the sender sent what it did: the congestion window and the
//! slow start threshold, the peer's receive window, the bytes in flight and
//! the bytes waiting in the pending queue, the loss events (fast
//! retransmissions and T3-rtx expiries), and how long the peer took to
//! acknowledge the newest chunk a SACK newly acknowledged. Each is one relaxed
//! store, add or min, where the association already holds its lock; the
//! pending bytes where the pending queue counts its own bytes. One more,
//! stored once when the association is made, says what one DATA chunk
//! carries, so a writer can size a message to one packet. Nothing in the
//! association reads them back, so a reader changes nothing it sends: the
//! twin test in `association_internal_test.rs` shows it packet by packet.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use portable_atomic::{AtomicU32, AtomicU64};

/// An acknowledgement delay not taken yet, or none since the last take.
const NONE: u64 = u64::MAX;

/// The gauges, kept by the association under its lock and by the pending
/// queue.
#[derive(Debug)]
pub(crate) struct GaugeCounters {
    cwnd: AtomicU32,
    ssthresh: AtomicU32,
    rwnd: AtomicU32,
    inflight_bytes: AtomicU64,
    pending_bytes: AtomicU64,
    fast_retransmits: AtomicU64,
    t3_expiries: AtomicU64,
    /// Microseconds; [`NONE`] before the first.
    ack_delay_us: AtomicU64,
    /// Microseconds; [`NONE`] when none came since the reader's last take.
    ack_delay_min_us: AtomicU64,
    /// Bytes of user data one DATA chunk carries; stored once.
    max_payload: AtomicU32,
}

impl Default for GaugeCounters {
    fn default() -> Self {
        Self {
            max_payload: AtomicU32::new(0),
            cwnd: AtomicU32::new(0),
            ssthresh: AtomicU32::new(0),
            rwnd: AtomicU32::new(0),
            inflight_bytes: AtomicU64::new(0),
            pending_bytes: AtomicU64::new(0),
            fast_retransmits: AtomicU64::new(0),
            t3_expiries: AtomicU64::new(0),
            ack_delay_us: AtomicU64::new(NONE),
            ack_delay_min_us: AtomicU64::new(NONE),
        }
    }
}

impl GaugeCounters {
    /// The windows and the bytes in flight, as the association holds them
    /// now: after a SACK, a send, the handshake's INIT ACK and a T3-rtx
    /// expiry, the places that change them.
    pub(crate) fn store_windows(&self, cwnd: u32, ssthresh: u32, rwnd: u32, inflight: usize) {
        self.cwnd.store(cwnd, Ordering::Relaxed);
        self.ssthresh.store(ssthresh, Ordering::Relaxed);
        self.rwnd.store(rwnd, Ordering::Relaxed);
        self.inflight_bytes
            .store(inflight as u64, Ordering::Relaxed);
    }

    /// What one DATA chunk carries, which the association fixes when it is
    /// made.
    pub(crate) fn set_max_payload(&self, n: u32) {
        self.max_payload.store(n, Ordering::Relaxed);
    }

    /// `n` bytes of user data queued to send.
    pub(crate) fn on_pending(&self, n: usize) {
        self.pending_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// `n` bytes of user data popped from the queue to be sent.
    pub(crate) fn on_popped(&self, n: usize) {
        self.pending_bytes.fetch_sub(n as u64, Ordering::Relaxed);
    }

    /// One chunk fast-retransmitted, where the association's stats count it.
    pub(crate) fn on_fast_retransmit(&self) {
        self.fast_retransmits.fetch_add(1, Ordering::Relaxed);
    }

    /// One T3-rtx expiry, where the association's stats count it.
    pub(crate) fn on_t3_expiry(&self) {
        self.t3_expiries.fetch_add(1, Ordering::Relaxed);
    }

    /// How long the newest chunk one SACK newly acknowledged, of those sent
    /// once, took from its send to that SACK.
    pub(crate) fn on_ack_delay(&self, delay: Duration) {
        let us = u64::try_from(delay.as_micros()).map_or(NONE - 1, |us| us.min(NONE - 1));
        self.ack_delay_us.store(us, Ordering::Relaxed);
        self.ack_delay_min_us.fetch_min(us, Ordering::Relaxed);
    }

    /// The gauges now; `take` resets the least acknowledgement delay.
    pub(crate) fn read(&self, take: bool) -> Gauges {
        let min = if take {
            self.ack_delay_min_us.swap(NONE, Ordering::Relaxed)
        } else {
            self.ack_delay_min_us.load(Ordering::Relaxed)
        };
        let us = |v: u64| (v != NONE).then_some(Duration::from_micros(v));
        Gauges {
            max_payload: self.max_payload.load(Ordering::Relaxed),
            cwnd: self.cwnd.load(Ordering::Relaxed),
            ssthresh: self.ssthresh.load(Ordering::Relaxed),
            rwnd: self.rwnd.load(Ordering::Relaxed),
            inflight_bytes: self.inflight_bytes.load(Ordering::Relaxed),
            pending_bytes: self.pending_bytes.load(Ordering::Relaxed),
            fast_retransmits: self.fast_retransmits.load(Ordering::Relaxed),
            t3_expiries: self.t3_expiries.load(Ordering::Relaxed),
            ack_delay: us(self.ack_delay_us.load(Ordering::Relaxed)),
            ack_delay_min: us(min),
        }
    }
}

/// One reading of an association's gauges. Each field is what the association
/// last stored; the fields are not one consistent snapshot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Gauges {
    /// Bytes of user data one DATA chunk carries: the path MTU less the
    /// common and DATA chunk headers, fixed when the association is made.
    pub max_payload: u32,
    /// The congestion window, bytes.
    pub cwnd: u32,
    /// The slow start threshold, bytes. 0 until the peer's INIT ACK sets it to
    /// the peer's receive window, so 0 on an association its peer's COOKIE
    /// ECHO established, until a loss sets it.
    pub ssthresh: u32,
    /// The peer's receive window, less what is in flight, bytes. 0 until the
    /// peer's INIT ACK or first SACK says it.
    pub rwnd: u32,
    /// Bytes of user data sent and not yet acknowledged.
    pub inflight_bytes: u64,
    /// Bytes of user data written and not yet sent, over every stream.
    pub pending_bytes: u64,
    /// Chunks fast-retransmitted since the association began.
    pub fast_retransmits: u64,
    /// T3-rtx expiries since the association began.
    pub t3_expiries: u64,
    /// The latest acknowledgement delay: from the send of the newest chunk a
    /// SACK newly acknowledged, among the chunks it acknowledged that were
    /// sent once (Karn), to that SACK. A browser's delayed acknowledgement is
    /// in it. `None` before the first.
    pub ack_delay: Option<Duration>,
    /// The least of those delays since this reader last took the gauges;
    /// `None` when no SACK gave one since.
    pub ack_delay_min: Option<Duration>,
}

/// A handle onto one association's [`Gauges`]. It keeps the gauges alive, and
/// nothing else of the association.
#[derive(Debug, Clone)]
pub struct GaugeReader(Arc<GaugeCounters>);

impl GaugeReader {
    pub(crate) fn new(counters: Arc<GaugeCounters>) -> Self {
        Self(counters)
    }

    /// The gauges now, lock-free. The least acknowledgement delay is the
    /// reader's to take: this take resets it, so it covers the SACKs since the
    /// previous take, and a second reader of the same association would take
    /// from the first.
    pub fn take(&self) -> Gauges {
        self.0.read(true)
    }

    /// Whether `self` and `other` read the same association.
    pub fn same(&self, other: &GaugeReader) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
