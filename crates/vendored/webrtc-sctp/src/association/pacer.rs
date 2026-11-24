//! roomler (FR-148 P0, D43): the association's pacer, inert unless a writer
//! turns it on.
//!
//! Slow start frees two chunks for each one a SACK acknowledges. A flight
//! that leaves back to back reaches the bottleneck at the sender's line rate,
//! the bottleneck spaces it to the link, and its SACKs come back at the
//! link's rate: every later flight then arrives at twice the link, and the
//! bottleneck's queue holds half of it, whatever the pipe. A queue shorter
//! than half the pipe overflows before any round has carried the link.
//!
//! While it is on, the pacer times the move of new data from the pending
//! queue into flight. In slow start it lets out the share of the congestion
//! window its writer steers it to over the least round trip seen since it was
//! turned on: a flight leaves spread over its round trip, so the bottleneck
//! queues only once a round carries more than the link, and then a little at
//! a time. Above the slow start threshold, where a SACK frees only what it
//! acknowledges, it lets out the whole window over the least round trip.
//! Before the first round-trip sample nothing is paced, and the first flight
//! leaves back to back as before. Its writer may also cap what is in flight:
//! while in flight is at or above the cap, new data waits for a SACK.
//!
//! It never changes the congestion window, the slow start threshold, the
//! growth rule or the retransmission rules: a retransmission never waits for
//! it, and what it holds stays in the pending queue. Slow start grows the
//! window at every SACK while data waits there, so a paced window ends
//! further above what is in flight than an unpaced one. Off, it keeps no
//! state, gates nothing, and the write loop arms no timer for it: every byte
//! and every timer of the association is as it was.

#[cfg(test)]
mod pacer_test;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use portable_atomic::{AtomicBool, AtomicU32, AtomicU64};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// The whole window, in the thousandths a writer steers the pacer's share of
/// it by.
pub const WHOLE: u32 = 1_000;

/// The most the tokens build while no chunk waits for them: the rate over
/// this span. A pause (an empty queue, a full window) saves up no burst
/// longer than this at the rate.
const IDLE_SPAN: Duration = Duration::from_millis(2);

/// The most the tokens build while a chunk waits for them: the rate over
/// this span. A wake that comes late lets out what came due, up to one tick
/// of Windows' default timer (15.6 ms) and a little more, so a coarse timer
/// costs no rate and a stalled task saves up no longer burst.
const LATE_SPAN: Duration = Duration::from_millis(17);

/// What the switch and the association share: whether the pacer is on, and
/// how its writer steers it.
#[derive(Debug)]
struct Shared {
    on: AtomicBool,
    /// The share of the window let out per least round trip in slow start,
    /// in thousandths of it.
    share: AtomicU32,
    /// The most in flight before new data waits; `u64::MAX` for no cap.
    cap: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            on: AtomicBool::new(false),
            share: AtomicU32::new(WHOLE),
            cap: AtomicU64::new(u64::MAX),
        }
    }
}

/// A handle that turns one association's pacer on and off and steers it,
/// lock-free.
#[derive(Debug, Clone)]
pub struct PaceSwitch {
    shared: Arc<Shared>,
    awake: Arc<mpsc::Sender<()>>,
}

impl PaceSwitch {
    /// Turns the pacer on or off. Either way it forgets how it was steered:
    /// the whole window, no cap. Turned off, it wakes the write loop, so what
    /// it held leaves at once, as the windows allow.
    pub fn set(&self, on: bool) {
        self.shared.share.store(WHOLE, Ordering::SeqCst);
        self.shared.cap.store(u64::MAX, Ordering::SeqCst);
        if self.shared.on.swap(on, Ordering::SeqCst) && !on {
            let _ = self.awake.try_send(());
        }
    }

    /// Steers the pacer while it is on: `share` thousandths of the window per
    /// least round trip in slow start (at least 1, at most [`WHOLE`]), and
    /// `cap`, the most in flight before new data waits. A cap that rises, or
    /// goes, wakes the write loop, so what waited for it leaves.
    pub fn steer(&self, share: u32, cap: Option<u64>) {
        self.shared
            .share
            .store(share.clamp(1, WHOLE), Ordering::SeqCst);
        let cap = cap.unwrap_or(u64::MAX);
        if self.shared.cap.swap(cap, Ordering::SeqCst) < cap {
            let _ = self.awake.try_send(());
        }
    }

    /// Whether the pacer is on.
    pub fn is_on(&self) -> bool {
        self.shared.on.load(Ordering::SeqCst)
    }

    /// The share of the window it is steered to, in thousandths.
    pub fn share(&self) -> u32 {
        self.shared.share.load(Ordering::SeqCst)
    }

    /// The cap on what is in flight it is steered to, if any.
    pub fn cap(&self) -> Option<u64> {
        Some(self.shared.cap.load(Ordering::SeqCst)).filter(|&c| c != u64::MAX)
    }

    /// Whether `self` and `other` switch the same association's pacer.
    pub fn same(&self, other: &PaceSwitch) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

/// A switch of no association's pacer: what it is set and steered to reaches
/// nothing. For a writer's tests, which need a switch without an association.
impl Default for PaceSwitch {
    fn default() -> Self {
        let (awake, _) = mpsc::channel(1);
        PaceSwitch {
            shared: Arc::default(),
            awake: Arc::new(awake),
        }
    }
}

/// The association's side of the pacer: the switch it reads at every send,
/// and, while on, the least round trip and the tokens.
#[derive(Debug, Default)]
pub(crate) struct Pacer {
    shared: Arc<Shared>,
    run: Option<Run>,
}

/// What the pacer keeps while it is on; dropped when it is turned off.
#[derive(Debug, Default, Clone, Copy)]
struct Run {
    /// The least round trip since the pacer was turned on.
    min_rtt: Option<Duration>,
    /// Bytes that may leave now. A chunk leaves while this is above 0, and
    /// spends its bytes, so it is below 0 by less than a chunk at most.
    tokens: i64,
    /// When the tokens were brought up to date, and the rate then, bytes a
    /// second.
    at: Option<Instant>,
    rate: u64,
    /// The cap on what is in flight, as this send read it.
    cap: Option<u64>,
    /// The last send held a chunk for tokens.
    held: bool,
    /// When the chunk held may leave, for the write loop to wake at.
    wake: Option<Instant>,
}

impl Pacer {
    /// The handle that turns this pacer on and off; turning it off wakes the
    /// write loop through `awake`.
    pub(crate) fn switch(&self, awake: Arc<mpsc::Sender<()>>) -> PaceSwitch {
        PaceSwitch {
            shared: Arc::clone(&self.shared),
            awake,
        }
    }

    /// At the start of a send: whether the pacer gates its new data. On, it
    /// brings the tokens to `now` at the rate of `cwnd` and `ssthresh` over
    /// the least round trip ([`pace_rate`]), and reads the cap. Off, it
    /// forgets everything, so the next time it is on it starts afresh.
    pub(crate) fn begin(&mut self, now: Instant, cwnd: u32, ssthresh: u32) -> bool {
        if !self.shared.on.load(Ordering::SeqCst) {
            self.run = None;
            return false;
        }
        let share = self.shared.share.load(Ordering::SeqCst);
        let cap = Some(self.shared.cap.load(Ordering::SeqCst)).filter(|&c| c != u64::MAX);
        let run = self.run.get_or_insert_with(Run::default);
        run.wake = None;
        let Some(min_rtt) = run.min_rtt else {
            // No round trip yet: the first flight leaves as it would.
            run.at = Some(now);
            return false;
        };
        let rate = pace_rate(cwnd, ssthresh, share, min_rtt);
        let most = bytes_in(rate, if run.held { LATE_SPAN } else { IDLE_SPAN }).max(1);
        run.tokens = match run.at {
            Some(at) => run
                .tokens
                .saturating_add(bytes_in(rate, now.saturating_duration_since(at)))
                .min(most),
            None => most,
        };
        run.at = Some(now);
        run.rate = rate;
        run.cap = cap;
        run.held = false;
        true
    }

    /// Whether one more new chunk of `len` bytes may leave now, with
    /// `inflight` bytes in flight, in a send `begin` gates; if it may, its
    /// bytes are spent. A chunk held for the cap waits for a SACK, which
    /// wakes the write loop; one held for tokens waits for
    /// [`Pacer::take_wake`].
    pub(crate) fn admit(&mut self, len: usize, inflight: usize) -> bool {
        let Some(run) = self.run.as_mut() else {
            return true;
        };
        if run.cap.is_some_and(|cap| inflight as u64 >= cap) {
            return false;
        }
        if run.tokens > 0 {
            run.tokens = run.tokens.saturating_sub(len as i64);
            return true;
        }
        run.held = true;
        run.wake = run
            .at
            .map(|at| at + time_of(run.tokens.unsigned_abs() + 1, run.rate));
        false
    }

    /// A new chunk of `len` bytes that left without the gate, in a send
    /// `begin` gates: the zero-window probe. Its bytes are spent.
    pub(crate) fn spend(&mut self, len: usize) {
        if let Some(run) = self.run.as_mut() {
            run.tokens = run.tokens.saturating_sub(len as i64);
        }
    }

    /// When the chunk the last send held may leave; `None` when it held
    /// none, and always `None` while the pacer is off. Taking it clears it.
    pub(crate) fn take_wake(&mut self) -> Option<Instant> {
        self.run.as_mut().and_then(|run| run.wake.take())
    }

    /// One round-trip sample: the acknowledgement delay of the newest chunk
    /// a SACK newly acknowledged, of those sent once. Kept only while on.
    pub(crate) fn on_rtt(&mut self, rtt: Duration) {
        if !self.shared.on.load(Ordering::SeqCst) {
            return;
        }
        let run = self.run.get_or_insert_with(Run::default);
        run.min_rtt = Some(run.min_rtt.map_or(rtt, |m| m.min(rtt)));
    }
}

/// The write loop's wait for the pacer: until `at`, or never when the pacer
/// holds nothing. Never arms a timer for `None`.
pub(crate) async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The pacing rate, bytes a second: `share` thousandths of the congestion
/// window over the least round trip in slow start (`cwnd <= ssthresh`, the
/// growth rule's own test), the whole window above the threshold, where a
/// SACK frees only what it acknowledges and the window grows by a chunk a
/// round only once a whole window was acknowledged in it.
fn pace_rate(cwnd: u32, ssthresh: u32, share: u32, min_rtt: Duration) -> u64 {
    let window = if cwnd <= ssthresh {
        u64::from(cwnd) * u64::from(share.clamp(1, WHOLE)) / u64::from(WHOLE)
    } else {
        u64::from(cwnd)
    };
    rate(window, min_rtt)
}

/// Bytes a second of `window` bytes over `rtt`.
fn rate(window: u64, rtt: Duration) -> u64 {
    let us = rtt.as_micros().max(1);
    u64::try_from(u128::from(window) * 1_000_000 / us).unwrap_or(u64::MAX)
}

/// Bytes, of `rate` bytes a second over `d`.
fn bytes_in(rate: u64, d: Duration) -> i64 {
    i64::try_from(u128::from(rate) * d.as_micros() / 1_000_000).unwrap_or(i64::MAX)
}

/// How long `bytes` take at `rate` bytes a second, rounded up: at that
/// time, the tokens have them.
fn time_of(bytes: u64, rate: u64) -> Duration {
    let us = (u128::from(bytes) * 1_000_000).div_ceil(u128::from(rate.max(1)));
    Duration::from_micros(u64::try_from(us).unwrap_or(u64::MAX))
}
