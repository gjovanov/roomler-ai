//! roomler (FR-148 P0, D35): the association's congestion window, slow start
//! threshold and `partial_bytes_acked`, taken and given back by a writer that
//! changes them for a while.
//!
//! A train (FR-148 P0 step 4) reads the path by pacing a session's first
//! flight, and in shadow it hands the association back as it found it. At its
//! first write it takes a [`Windows`] snapshot of the three. Once it has
//! stopped and its flight has drained, [`WindowHandle::try_restore`] writes the
//! snapshot's values back, under the association's lock, and only while
//! nothing of any stream is pending or in flight, outside fast recovery, and
//! with the caller's gate agreeing.
//!
//! Until then, [`WindowHandle::try_lower`] lowers the congestion window to the
//! snapshot's, or to what is in flight where that is more, and never raises
//! it. Slow start grows the window by `min(acked, cwnd)` at every SACK while
//! anything waits in the pending queue, not while the window is used, so a
//! paced train ends with a window several times the flight its pacer held. A
//! message written then would leave at once, at the sender's line rate, into
//! all of it, and once it is written the restore, which needs nothing pending
//! or in flight, may never run. A window lowered to what is in flight lets
//! nothing leave at once, and each SACK then lets out at most twice what it
//! acknowledged, slow start's own ack-clocked growth. This is window
//! validation (RFC 7661's idea): a window that grew while its sender was held
//! is cut back to what the path carried.
//!
//! Nothing in the association calls either. A handle holds a `Weak` of the
//! association's state, so it keeps nothing alive, and one that only takes
//! snapshots changes nothing the association sends (the twin in
//! `association_internal_test.rs`): a snapshot takes the lock the read and
//! write loops take, and reads.

use std::sync::Weak;

use tokio::sync::Mutex;

use super::association_internal::AssociationInternal;

/// The three values a snapshot takes and a restore writes back. It has no
/// `Default`, because zeros are not a snapshot: the association never writes a
/// window below one MTU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Windows {
    /// The congestion window, in bytes.
    pub cwnd: u32,
    /// The slow start threshold, in bytes.
    pub ssthresh: u32,
    /// The bytes acknowledged toward congestion avoidance's next step of the
    /// window (RFC 4960 §7.2.2).
    pub partial_bytes_acked: u32,
}

/// A lowering's congestion window, before it and after it. Equal when the
/// window was already at or below the lowering's bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lowered {
    /// The congestion window the lowering found, in bytes.
    pub before: u32,
    /// The congestion window it left, in bytes: never above `before`.
    pub after: u32,
}

/// Why a restore or a lowering wrote nothing. Each is an instant's answer:
/// nothing was written, and the caller may ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreRefusal {
    /// The association's lock was held at that instant: by its read or write
    /// loop handling or gathering a packet, a timer, or its close. Never
    /// waited for.
    Busy,
    /// A chunk of some stream waited in the pending queue.
    Pending,
    /// A chunk was in flight.
    InFlight,
    /// The association was in fast recovery, whose loss response owns the
    /// window.
    FastRecovery,
    /// The caller's gate said no.
    Gate,
    /// The association is gone.
    Gone,
}

impl RestoreRefusal {
    /// One word, for a log line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Pending => "pending",
            Self::InFlight => "inflight",
            Self::FastRecovery => "recovery",
            Self::Gate => "gate",
            Self::Gone => "gone",
        }
    }
}

/// A handle onto one association's windows (`Association::windows`). It holds
/// a `Weak` of the association's state, so it keeps nothing alive: once the
/// association and its loops are gone, every call says so.
#[derive(Debug, Clone)]
pub struct WindowHandle(Weak<Mutex<AssociationInternal>>);

impl WindowHandle {
    pub(crate) fn new(state: Weak<Mutex<AssociationInternal>>) -> Self {
        Self(state)
    }

    /// The three values as they stand, read under the association's lock;
    /// `None` once the association is gone. It waits for the lock, which the
    /// read and write loops hold while they handle or gather a packet and the
    /// association's close holds across its own, so the caller bounds the
    /// wait. While it waits, it holds the association's state alive.
    pub async fn snapshot(&self) -> Option<Windows> {
        let state = self.0.upgrade()?;
        let ai = state.lock().await;
        Some(ai.windows())
    }

    /// Writes `to`'s three values back, under the association's lock, and
    /// stores the gauges, as every window write of the association does. It
    /// never waits for the lock (`Busy`). Under it, in this order, it writes
    /// nothing while a chunk of any stream is pending (`Pending`) or in flight
    /// (`InFlight`), while in fast recovery (`FastRecovery`), or when `may`,
    /// the caller's gate, says no (`Gate`). `may` runs under the lock, and
    /// only once the three checks have passed.
    ///
    /// A write enters the pending queue without this lock and leaves it only
    /// under it, so a write under the lock never changes the window under a
    /// gathered chunk: one written after the check leaves at the next send,
    /// under the window restored. With nothing pending, the write loop has
    /// nothing to wake for, and the next write wakes it.
    ///
    /// It never leaves the window below one MTU: the window it writes is
    /// `max(to.cwnd, MTU)`, and the other two are written as given. The floor
    /// is the association's own, the one a T3-rtx expiry keeps
    /// (`cwnd <- MTU`), so for any snapshot the association took it changes
    /// nothing. A window of 0 could never grow back: slow start grows the
    /// window by `min(acked, cwnd)`, and the send path would let out one chunk
    /// at a time, and only with nothing in flight.
    pub fn try_restore(
        &self,
        to: &Windows,
        may: impl FnOnce() -> bool,
    ) -> Result<(), RestoreRefusal> {
        let state = self.0.upgrade().ok_or(RestoreRefusal::Gone)?;
        let mut ai = state.try_lock().map_err(|_| RestoreRefusal::Busy)?;
        if ai.chunks_pending() {
            return Err(RestoreRefusal::Pending);
        }
        if ai.chunks_in_flight() {
            return Err(RestoreRefusal::InFlight);
        }
        if ai.in_fast_recovery {
            return Err(RestoreRefusal::FastRecovery);
        }
        if !may() {
            return Err(RestoreRefusal::Gate);
        }
        let cwnd = to.cwnd.max(ai.mtu);
        ai.write_windows(&Windows { cwnd, ..*to });
        Ok(())
    }

    /// Lowers the congestion window to `to`'s, or to the bytes in flight where
    /// that is more, never below one MTU, and never raises it: `cwnd =
    /// min(cwnd, max(to.cwnd, in flight, MTU))`. The slow start threshold and
    /// `partial_bytes_acked` are left alone, and the gauges are stored. It
    /// never waits for the lock (`Busy`), and writes nothing in fast recovery
    /// (`FastRecovery`), whose loss response owns the window, which does not
    /// grow until it ends.
    ///
    /// The floor is the association's own, the one a T3-rtx expiry keeps, as
    /// in `try_restore`: for any snapshot the association took it changes
    /// nothing. As the lowering never raises the window, a window already
    /// under one MTU, which the association never writes, stays where it is.
    ///
    /// It may run with bytes in flight because it never raises the window, as
    /// SCTP's own loss response writes it with bytes out (fast recovery's
    /// entry, a T3-rtx expiry): the send path lets no new chunk out until what
    /// is in flight and the chunk fit the window. So a window lowered to what
    /// is in flight lets nothing leave at once, and it only lowers, so the
    /// write loop has nothing to wake for.
    pub fn try_lower(&self, to: &Windows) -> Result<Lowered, RestoreRefusal> {
        let state = self.0.upgrade().ok_or(RestoreRefusal::Gone)?;
        let mut ai = state.try_lock().map_err(|_| RestoreRefusal::Busy)?;
        if ai.in_fast_recovery {
            return Err(RestoreRefusal::FastRecovery);
        }
        let before = ai.cwnd;
        let in_flight = u32::try_from(ai.inflight_bytes()).unwrap_or(u32::MAX);
        let after = before.min(to.cwnd.max(in_flight).max(ai.mtu));
        ai.write_cwnd(after);
        Ok(Lowered { before, after })
    }
}
