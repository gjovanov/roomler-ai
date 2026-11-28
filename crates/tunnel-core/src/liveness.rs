// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-156 P1 — proven liveness: when an SSH connection's client or a tunnel
//! flow was last heard, and the lists the update gate counts from
//! (`docs/fr/FR-156-update-never-cuts-live-work-by-surprise.md`).
//!
//! - **[`Stamp`]**: the time something was last heard, in milliseconds on its
//!   [`Clock`]. [`Stamp::touch`] is all the byte path pays: one clock read and
//!   one relaxed store, with no lock and no allocation.
//! - **[`LiveList`]**: the open entries, each held by a [`Listed`] guard for
//!   its owner's life, so an entry leaves however its owner ends: a return,
//!   an error, a panic, or a task that was aborted. The list's lock is taken
//!   in synchronous code only, never across an `.await`.
//! - **[`Heard`]**: a stream whose every read that returns bytes touches a
//!   stamp.
//!
//! An entry counts while it was heard within [`WINDOW`]. With the kill switch
//! ([`proof`] off, `ROOMLERD_LIVE_WORK_PROOF=0`) the counters fall back to
//! what they counted before FR-156: every open SSH connection, and no flow.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// D1 — how recently a connection's client, or a flow's bytes, must have
/// been heard for the work to count: three of SSH's 30 s keepalives.
pub const WINDOW: Duration = Duration::from_secs(90);

/// The kill switch's suffix: `ROOMLERD_LIVE_WORK_PROOF` (or
/// `ROOMLER_NODE_LIVE_WORK_PROOF`).
pub const SWITCH: &str = "LIVE_WORK_PROOF";

/// Is proven liveness on? Default on; `ROOMLERD_LIVE_WORK_PROOF=0` turns it
/// off ([`crate::env::flag`]).
pub fn proof() -> bool {
    crate::env::flag(SWITCH, true)
}

/// The clock a [`Stamp`] is read on.
#[derive(Debug, Clone, Copy)]
pub enum Clock {
    /// The host's.
    Host,
    /// A test's: tokio's clock from `origin`, which a paused runtime moves
    /// only when the test, or the runtime's own timers, advance it.
    Tokio { origin: tokio::time::Instant },
}

impl Clock {
    /// A [`Clock::Tokio`] from now.
    pub fn tokio() -> Clock {
        Clock::Tokio {
            origin: tokio::time::Instant::now(),
        }
    }

    /// Now, in milliseconds on this clock.
    pub fn now_ms(&self) -> u64 {
        match self {
            Clock::Host => host_ms(),
            Clock::Tokio { origin } => {
                millis(tokio::time::Instant::now().saturating_duration_since(*origin))
            }
        }
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The host clock, in milliseconds, counting the time the host slept where
/// the OS documents a clock that does (the spec's P1 design, "The clock"):
/// Linux `CLOCK_BOOTTIME`, macOS `CLOCK_MONOTONIC`, and on Windows `Instant`,
/// which is `QueryPerformanceCounter` and counts sleep. `Instant` itself
/// promises nothing across a suspend, and reads Linux's `CLOCK_MONOTONIC` and
/// macOS's `CLOCK_UPTIME_RAW`, both of which stop while the host sleeps.
pub fn host_ms() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // Chosen once, at the first stamp: the clock that counts sleep if the
        // kernel answers for it, else `Instant` for good, so no two readings
        // ever mix clocks. A clock that answered once always does
        // (`clock_gettime` fails only for a clock the kernel lacks).
        static COUNTS_SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *COUNTS_SLEEP.get_or_init(|| clock_ms(SLEEP_CLOCK).is_some())
            && let Some(ms) = clock_ms(SLEEP_CLOCK)
        {
            return ms;
        }
    }
    instant_ms()
}

/// The clock that keeps counting while the host sleeps.
#[cfg(target_os = "linux")]
const SLEEP_CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
#[cfg(target_os = "macos")]
const SLEEP_CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;

/// `clock_gettime(id)` in milliseconds; `None` if the kernel refuses it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn clock_ms(id: libc::clockid_t) -> Option<u64> {
    let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `ts` is valid for the call to write a whole timespec into, and
    // it is read only after the call reported that it did.
    if unsafe { libc::clock_gettime(id, ts.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the call returned 0, so it wrote the timespec.
    let ts = unsafe { ts.assume_init() };
    let secs = u64::try_from(ts.tv_sec).ok()?;
    let nanos = u64::try_from(ts.tv_nsec).ok()?;
    Some(secs.saturating_mul(1000).saturating_add(nanos / 1_000_000))
}

/// `Instant`, from the first reading in this process.
fn instant_ms() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    millis(BASE.get_or_init(std::time::Instant::now).elapsed())
}

/// When something was last heard: milliseconds on [`Stamp::clock`].
#[derive(Debug)]
pub struct Stamp {
    at_ms: AtomicU64,
    clock: Clock,
}

impl Stamp {
    /// A stamp on `clock`, heard now.
    pub fn new(clock: Clock) -> Stamp {
        Stamp {
            at_ms: AtomicU64::new(clock.now_ms()),
            clock,
        }
    }

    /// Heard now: one clock read and one relaxed store.
    #[inline]
    pub fn touch(&self) {
        self.at_ms.store(self.clock.now_ms(), Ordering::Relaxed);
    }

    /// How long ago it was last heard.
    pub fn age(&self) -> Duration {
        let at = self.at_ms.load(Ordering::Relaxed);
        Duration::from_millis(self.clock.now_ms().saturating_sub(at))
    }

    /// Heard no longer ago than `window`.
    pub fn heard_within(&self, window: Duration) -> bool {
        self.age() <= window
    }
}

impl Default for Stamp {
    fn default() -> Stamp {
        Stamp::new(Clock::Host)
    }
}

/// The open entries of one kind of work. Cheap to clone: every clone is the
/// same list.
#[derive(Debug, Clone, Default)]
pub struct LiveList {
    entries: Arc<Mutex<Vec<Arc<Stamp>>>>,
}

impl LiveList {
    /// An empty list.
    pub fn new() -> LiveList {
        LiveList::default()
    }

    // Poison is ignored: no code panics while it holds the lock, and a
    // poisoned list must still count, not take the daemon down.
    fn lock(&self) -> MutexGuard<'_, Vec<Arc<Stamp>>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new entry on `clock`, heard now, listed until the guard drops.
    pub fn join(&self, clock: Clock) -> Listed {
        self.list(Arc::new(Stamp::new(clock)))
    }

    /// `stamp`, touched now and listed until the guard drops.
    pub fn list(&self, stamp: Arc<Stamp>) -> Listed {
        stamp.touch();
        self.lock().push(Arc::clone(&stamp));
        Listed {
            list: self.clone(),
            stamp,
        }
    }

    /// The entries open now.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// No entry is open.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entries heard within `window`.
    pub fn heard_within(&self, window: Duration) -> usize {
        self.lock()
            .iter()
            .filter(|e| e.heard_within(window))
            .count()
    }

    /// Whether `stamp` is listed.
    pub fn contains(&self, stamp: &Arc<Stamp>) -> bool {
        self.lock().iter().any(|e| Arc::ptr_eq(e, stamp))
    }
}

/// One entry of a [`LiveList`], listed until this guard drops.
#[derive(Debug)]
pub struct Listed {
    list: LiveList,
    stamp: Arc<Stamp>,
}

impl Listed {
    /// The entry's stamp.
    pub fn stamp(&self) -> &Arc<Stamp> {
        &self.stamp
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        let mut entries = self.list.lock();
        if let Some(i) = entries.iter().position(|e| Arc::ptr_eq(e, &self.stamp)) {
            entries.swap_remove(i);
        }
    }
}

/// A stream whose every read that returns bytes touches `stamp`. Writes pass
/// through untouched: what this side sends proves nothing of the other's life.
#[derive(Debug)]
pub struct Heard<S> {
    inner: S,
    stamp: Arc<Stamp>,
}

impl<S> Heard<S> {
    pub fn new(inner: S, stamp: Arc<Stamp>) -> Heard<S> {
        Heard { inner, stamp }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Heard<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.stamp.touch();
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Heard<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The clock that stops while the host sleeps (Linux `CLOCK_MONOTONIC`,
    /// macOS `CLOCK_UPTIME_RAW`, `Instant`'s), in milliseconds, with how far
    /// the host clock may read behind it on a host that never slept. `None`
    /// where `Instant` already counts sleep.
    #[cfg(target_os = "linux")]
    fn stopping_clock_ms() -> Option<(u64, u64)> {
        // `CLOCK_BOOTTIME` is `CLOCK_MONOTONIC` plus the time suspended, on
        // the same base: never behind it.
        clock_ms(libc::CLOCK_MONOTONIC).map(|ms| (ms, 0))
    }
    #[cfg(target_os = "macos")]
    fn stopping_clock_ms() -> Option<(u64, u64)> {
        // `CLOCK_MONOTONIC` follows the clock's frequency adjustments, which
        // the RAW clocks do not (clock_gettime(3)), so on a Mac that never
        // slept it may read a slew's worth behind `CLOCK_UPTIME_RAW`. 1 % of
        // the uptime is far above any slew, and far below the gap to a clock
        // that starts with the process.
        clock_ms(libc::CLOCK_UPTIME_RAW).map(|ms| (ms, ms / 100))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn stopping_clock_ms() -> Option<(u64, u64)> {
        None
    }

    /// FR-156 C1 — the host clock keeps real time in milliseconds, never runs
    /// back, and where `Instant` stops while the host sleeps it is the clock
    /// that does not: since boot, and not behind the one that stops (their
    /// gap is the time the host has slept, give or take a Mac's slew).
    #[test]
    fn the_host_clock_keeps_real_time_and_counts_the_hosts_sleep() {
        let a = host_ms();
        std::thread::sleep(Duration::from_millis(250));
        let b = host_ms();
        assert!(
            (240..10_000).contains(&b.saturating_sub(a)),
            "250 ms of real time read as {} ms",
            b.saturating_sub(a)
        );
        let mut last = host_ms();
        for _ in 0..10_000 {
            let now = host_ms();
            assert!(now >= last, "the host clock ran back: {last} -> {now}");
            last = now;
        }
        if let Some((stopping, slack)) = stopping_clock_ms() {
            let host = host_ms();
            assert!(
                host.saturating_add(slack) >= stopping,
                "the host clock ({host} ms) is behind the clock that stops in sleep \
                 ({stopping} ms, {slack} ms allowed): it is not the one that counts \
                 the host's sleep"
            );
        }
    }

    /// FR-156 C2 — the seam: a stamp on a test's clock ages only as a paused
    /// runtime's time moves, and a touch makes it new again.
    #[tokio::test(start_paused = true)]
    async fn a_stamp_on_a_test_clock_ages_with_the_paused_runtime() {
        let stamp = Stamp::new(Clock::tokio());
        assert_eq!(stamp.age(), Duration::ZERO);
        tokio::time::advance(Duration::from_secs(91)).await;
        assert_eq!(stamp.age(), Duration::from_secs(91));
        assert!(!stamp.heard_within(WINDOW));
        stamp.touch();
        assert_eq!(stamp.age(), Duration::ZERO);
        assert!(stamp.heard_within(WINDOW));
    }

    /// An entry leaves its list however its guard ends: dropped, or with a
    /// task that was aborted mid-await.
    #[tokio::test]
    async fn an_entry_leaves_with_its_guard_however_it_ends() {
        let list = LiveList::new();
        let listed = list.join(Clock::Host);
        assert!(list.contains(listed.stamp()));
        drop(listed);
        assert!(list.is_empty());

        let held = list.clone();
        let task = tokio::spawn(async move {
            let _listed = held.join(Clock::Host);
            std::future::pending::<()>().await;
        });
        while list.is_empty() {
            tokio::task::yield_now().await;
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(list.is_empty(), "an aborted task left its entry");
    }

    /// A read that returns bytes touches the stamp; a write, or a read at EOF,
    /// does not.
    #[tokio::test(start_paused = true)]
    async fn only_bytes_read_are_heard() {
        let stamp = Arc::new(Stamp::new(Clock::tokio()));
        let (near, mut far) = tokio::io::duplex(1024);
        let mut heard = Heard::new(near, Arc::clone(&stamp));
        tokio::time::advance(Duration::from_secs(30)).await;
        heard.write_all(b"out").await.unwrap();
        let mut out = [0u8; 3];
        far.read_exact(&mut out).await.unwrap();
        assert_eq!(stamp.age(), Duration::from_secs(30), "a write was heard");
        far.write_all(b"in").await.unwrap();
        let mut got = [0u8; 2];
        heard.read_exact(&mut got).await.unwrap();
        assert_eq!(stamp.age(), Duration::ZERO, "the bytes read were not heard");
        tokio::time::advance(Duration::from_secs(5)).await;
        drop(far);
        assert_eq!(heard.read(&mut got).await.unwrap(), 0);
        assert_eq!(stamp.age(), Duration::from_secs(5), "EOF was heard");
    }
}
