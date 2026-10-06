// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-86 P1 — the **flow-owned listener**: one loopback port, bound once for
//! the life of a flow, serving its connections through whichever carrier is
//! current.
//!
//! Before this, each tunnel session bound the route's port itself and ran its
//! own accept loop (`driver::run_webrtc_session` / `run_quic_session`), so
//! (a) a second session could never open beside the first — both would bind
//! `127.0.0.1:<port>` — and (b) between one session ending and the next one
//! reaching its bind, the port was unbound and a client got `connection
//! refused` (field 2026-09-28, #1769: 20–30 s per reconnect toward a slow
//! exit, and the whole ladder toward a busy one).
//!
//! The listener now belongs to the flow. A [`Carrier`](crate::driver::Carrier)
//! is installed behind it once a session is established and cleared when the
//! session dies; while no carrier is current an accepted connection is **held**
//! — at most [`HoldPolicy::max_held`] of them, each for at most
//! [`HoldPolicy::max_wait`] — and handed to the next carrier in arrival order.
//! A connection past either bound is closed (the client sees EOF), never
//! refused: the kernel completed its handshake, so it never had to retry.
//!
//! Nothing about the transports changes here: the carrier runs exactly the
//! per-connection code the session's accept loop used to, and the flow's
//! supervisor still decides when a carrier is established and when it is dead.
//! This module only owns the port and the queue in front of it.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::driver::AbortOnDrop;

/// What the listener needs from a carrier: take one accepted connection and
/// run it. [`crate::driver::Carrier`] implements it for both transports; the
/// tests use a fake so the hold and hand-off rules are locked without a peer.
pub trait Carry: Send + Sync + 'static {
    /// Take ownership of `tcp` and forward it through this carrier's session.
    /// The listener's accept task calls it inline, **under the listener's
    /// lock** (#1816): it must not block, and it must not call back into the
    /// listener. Holding the lock is what makes the carry COUNT the connection
    /// on the current carrier before [`install`](FlowListener::install) can
    /// swap it away — see `Slot::offer`.
    fn carry(&self, tcp: TcpStream, peer_addr: SocketAddr);
}

/// Default for [`HoldPolicy::max_held`]: the largest burst a reconnecting
/// route is expected to absorb (a desktop app reopening its pooled
/// connections) with room to spare, and small enough that a client that keeps
/// dialing a route whose exit never comes back cannot pile up sockets.
pub const DEFAULT_MAX_HELD: usize = 64;

/// Default for [`HoldPolicy::max_wait`]: longer than one full establishment
/// (the open 15 s + the pool/QUIC-ready 30 s can both be cut short by the
/// exit answering, and a healthy reconnect takes 1–5 s), shorter than any
/// client's own connect timeout worth waiting out.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(30);

/// How many connections wait for a carrier, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldPolicy {
    /// Connections held at once; one arriving past this is closed at once.
    pub max_held: usize,
    /// How long one connection may wait; past this it is closed.
    pub max_wait: Duration,
}

impl Default for HoldPolicy {
    fn default() -> Self {
        Self {
            max_held: DEFAULT_MAX_HELD,
            max_wait: DEFAULT_MAX_WAIT,
        }
    }
}

/// The flow's listener: bound once, kept until dropped, serving through the
/// current carrier and holding connections while there is none.
///
/// Dropping it aborts the accept task, which closes the listening socket and
/// every held connection. It does **not** end the current carrier — the
/// carrier is `Arc`-shared and ends when its last owner (the flow supervisor)
/// drops it, which for a killed flow is the same moment.
pub struct FlowListener<C: Carry> {
    local_addr: SocketAddr,
    slot: Arc<Slot<C>>,
    _acceptor: AbortOnDrop<()>,
}

/// The listener's shared state: the current carrier and the hold queue, under
/// ONE lock so that "no carrier ⇒ hold" and "install ⇒ drain the hold" can
/// never interleave to strand a connection in the queue — and, since #1816, so
/// that "carrier ⇒ carry" and "install ⇒ swap" can never interleave to hand a
/// connection to a carrier whose drain has already seen it at zero.
struct Slot<C> {
    policy: HoldPolicy,
    state: Mutex<SlotState<C>>,
}

struct SlotState<C> {
    carrier: Option<Arc<C>>,
    held: VecDeque<Held>,
}

struct Held {
    tcp: TcpStream,
    peer_addr: SocketAddr,
    since: Instant,
}

impl<C: Carry> FlowListener<C> {
    /// Bind `127.0.0.1:local` and start accepting. Connections are held until
    /// the first [`install`](Self::install).
    pub async fn bind(local: u16, policy: HoldPolicy) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", local))
            .await
            .with_context(|| format!("binding 127.0.0.1:{local}"))?;
        let local_addr = listener.local_addr()?;
        let slot = Arc::new(Slot {
            policy,
            state: Mutex::new(SlotState {
                carrier: None,
                held: VecDeque::new(),
            }),
        });
        let acceptor = AbortOnDrop::new(tokio::spawn(accept_loop(listener, Arc::clone(&slot))));
        Ok(Self {
            local_addr,
            slot,
            _acceptor: acceptor,
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Make `carrier` current and hand it every held connection that is still
    /// within its wait bound (the others are closed). Returns how many were
    /// handed over. A carrier already current is replaced; its `Arc` is
    /// returned to the caller through nothing — it drops here unless the
    /// caller kept a clone, which the P1 supervisor always does.
    pub fn install(&self, carrier: Arc<C>) -> usize {
        let held: Vec<Held> = {
            let mut st = self.slot.state.lock().unwrap();
            st.carrier = Some(Arc::clone(&carrier));
            st.held.drain(..).collect()
        };
        let now = Instant::now();
        let mut handed = 0usize;
        for h in held {
            if now.duration_since(h.since) > self.slot.policy.max_wait {
                warn!(
                    peer = %h.peer_addr,
                    waited_ms = now.duration_since(h.since).as_millis(),
                    "held connection outlived its wait bound before a carrier was ready; closing"
                );
                continue;
            }
            carrier.carry(h.tcp, h.peer_addr);
            handed += 1;
        }
        if handed > 0 {
            info!(handed, "held connections handed to the new carrier");
        }
        handed
    }

    /// No carrier from now on: connections are held again. Returns the one
    /// that was current, if any, so the caller can end it.
    pub fn clear(&self) -> Option<Arc<C>> {
        self.slot.state.lock().unwrap().carrier.take()
    }

    /// Connections waiting for a carrier right now.
    pub fn held(&self) -> usize {
        self.slot.state.lock().unwrap().held.len()
    }

    /// Whether a carrier is current.
    pub fn has_carrier(&self) -> bool {
        self.slot.state.lock().unwrap().carrier.is_some()
    }
}

impl<C: Carry> Slot<C> {
    /// One accepted connection: to the current carrier, else into the hold
    /// (or closed when the hold is full). The decision and the enqueue happen
    /// under the one lock — see [`Slot`] — and so does the carry.
    fn offer(&self, tcp: TcpStream, peer_addr: SocketAddr) {
        let mut st = self.state.lock().unwrap();
        if let Some(carrier) = &st.carrier {
            // #1816 — carried, which is to say COUNTED (`carry` runs
            // `InFlight::new` synchronously before it spawns), while the lock is
            // held. `install` swaps under the same lock, so a connection is
            // either counted on the old carrier before a promotion — and the
            // old carrier's drain waits for it — or offered to the new one.
            // Before this the `Arc` was cloned and the lock released first; in
            // that gap `install(new)` + `spawn_drain(old)` saw `active() == 0`
            // and closed the old carrier, and the carry landed on a carrier
            // this call's returning `Arc` then dropped for good — cutting the
            // connection it had just spawned, the one cut P2 promises never
            // happens.
            carrier.carry(tcp, peer_addr);
            return;
        }
        if st.held.len() >= self.policy.max_held {
            warn!(
                peer = %peer_addr,
                held = st.held.len(),
                "no carrier ready and the hold is full; closing the connection"
            );
            return; // `tcp` drops here: closed, not refused
        }
        debug!(peer = %peer_addr, held = st.held.len() + 1, "no carrier ready; holding the connection");
        st.held.push_back(Held {
            tcp,
            peer_addr,
            since: Instant::now(),
        });
    }

    /// When the oldest held connection reaches its wait bound, if any is held.
    fn next_expiry(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap()
            .held
            .front()
            .map(|h| h.since + self.policy.max_wait)
    }

    /// Close every held connection that has waited `max_wait` or longer. The
    /// queue is in arrival order, so the expired ones are at the front.
    fn expire(&self, now: Instant) {
        let expired: Vec<Held> = {
            let mut st = self.state.lock().unwrap();
            let mut out = Vec::new();
            while let Some(front) = st.held.front()
                && now.duration_since(front.since) >= self.policy.max_wait
            {
                if let Some(h) = st.held.pop_front() {
                    out.push(h);
                }
            }
            out
        };
        if !expired.is_empty() {
            warn!(
                closed = expired.len(),
                max_wait_s = self.policy.max_wait.as_secs(),
                "held connections waited out the bound with no carrier ready; closing them"
            );
        }
        // `expired` drops here: each TcpStream closes.
    }
}

/// The accept task. Runs until aborted (the [`FlowListener`] dropped); an
/// accept error is logged and the loop goes on, the shape the sessions'
/// own loops had.
async fn accept_loop<C: Carry>(listener: TcpListener, slot: Arc<Slot<C>>) {
    loop {
        let next_expiry = slot.next_expiry();
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((tcp, peer_addr)) => slot.offer(tcp, peer_addr),
                Err(e) => error!(%e, "accept failed"),
            },
            _ = sleep_until_or_never(next_expiry) => slot.expire(Instant::now()),
        }
    }
}

async fn sleep_until_or_never(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// A carrier that hands every carried connection to the test.
    struct FakeCarrier {
        name: &'static str,
        tx: mpsc::UnboundedSender<(TcpStream, SocketAddr)>,
    }

    impl Carry for FakeCarrier {
        fn carry(&self, tcp: TcpStream, peer_addr: SocketAddr) {
            let _ = self.tx.send((tcp, peer_addr));
        }
    }

    /// A carrier whose `carry` BLOCKS until the test releases it — or until
    /// the release sender is gone, or 5 s pass, so a red run fails instead of
    /// hanging on a thread parked in it. It signals when it is entered and sets
    /// `returned` as it leaves. The way to hold the listener mid-carry.
    struct BlockingCarrier {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        returned: Arc<AtomicBool>,
    }

    impl Carry for BlockingCarrier {
        fn carry(&self, _tcp: TcpStream, _peer_addr: SocketAddr) {
            let _ = self.entered.send(());
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
            self.returned.store(true, Ordering::SeqCst);
        }
    }

    fn fake(
        name: &'static str,
    ) -> (
        Arc<FakeCarrier>,
        mpsc::UnboundedReceiver<(TcpStream, SocketAddr)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(FakeCarrier { name, tx }), rx)
    }

    async fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    }

    /// A carried stream, or a panic naming the carrier that got nothing.
    async fn carried(
        rx: &mut mpsc::UnboundedReceiver<(TcpStream, SocketAddr)>,
        who: &str,
    ) -> TcpStream {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("carrier {who} was handed no connection within 2 s"))
            .expect("carrier channel open")
            .0
    }

    /// A read on `s` that must see EOF (0 bytes) or a reset within 2 s — the
    /// far side closed it.
    async fn assert_closed(s: &mut TcpStream, what: &str) {
        let mut buf = [0u8; 8];
        match tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(n)) => panic!("{what}: expected a close, read {n} bytes"),
            Err(_) => panic!("{what}: still open after 2 s — it was neither carried nor closed"),
        }
    }

    /// A read on `s` that must NOT complete within `wait` — the connection is
    /// open and quiet (held).
    async fn assert_still_open(s: &mut TcpStream, wait: Duration, what: &str) {
        let mut buf = [0u8; 8];
        match tokio::time::timeout(wait, s.read(&mut buf)).await {
            Err(_) => {}
            Ok(Ok(0)) => panic!("{what}: closed (EOF) while it should be held"),
            Ok(Ok(n)) => panic!("{what}: read {n} bytes while it should be held"),
            Ok(Err(e)) => panic!("{what}: errored while it should be held: {e}"),
        }
    }

    /// FR-86 P1 (AC3), the whole point: the port is bound ONCE across a carrier
    /// replacement. A client that connects while the first carrier is gone and
    /// the second is not yet ready is HELD — accepted by the kernel, neither
    /// refused nor closed — and then carried by the second carrier with its
    /// bytes intact. NC86A (the listener dies with the carrier) and NC86B (no
    /// hold: the connection is closed on arrival) both turn this red.
    #[tokio::test]
    async fn a_client_connecting_between_carriers_is_held_then_carried_never_refused() {
        let port = free_port().await;
        let listener = FlowListener::bind(port, HoldPolicy::default())
            .await
            .expect("bind");
        let addr = listener.local_addr();
        let (a, mut a_rx) = fake("A");
        let (b, mut b_rx) = fake("B");

        // Carrier A is current: a connection goes straight through it.
        listener.install(Arc::clone(&a));
        let mut c1 = TcpStream::connect(addr)
            .await
            .expect("connect while A serves");
        let mut carried1 = carried(&mut a_rx, a.name).await;
        c1.write_all(b"one").await.unwrap();
        let mut buf = [0u8; 3];
        carried1.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"one");

        // A dies (the session ended). The port must STAY bound...
        assert!(listener.clear().is_some(), "A was current");
        assert!(
            TcpListener::bind(addr).await.is_err(),
            "the flow's port must stay bound while no carrier is ready"
        );
        // ...so a client connecting in the gap is accepted and held — not
        // refused, not closed.
        let mut c2 = TcpStream::connect(addr)
            .await
            .expect("a client must not be REFUSED while the flow re-establishes");
        c2.write_all(b"two").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(listener.held(), 1, "the connection is held, not dropped");
        assert_still_open(&mut c2, Duration::from_millis(300), "the held client").await;

        // B comes up: the held connection is handed to it, bytes intact.
        assert_eq!(
            listener.install(Arc::clone(&b)),
            1,
            "one held connection handed over"
        );
        let mut carried2 = carried(&mut b_rx, b.name).await;
        carried2.read_exact(&mut buf).await.unwrap();
        assert_eq!(
            &buf, b"two",
            "the bytes the client wrote while held reach carrier B"
        );
        assert_eq!(listener.held(), 0);
        assert!(
            a_rx.try_recv().is_err(),
            "the dead carrier A must not be handed the gap connection"
        );
        // And the address never changed — one bind for the flow's life.
        assert_eq!(listener.local_addr(), addr);
    }

    /// AC3's time bound: a connection held past `max_wait` with no carrier is
    /// closed, and is NOT handed to a carrier that comes up afterwards.
    /// NC86D (no expiry) turns this red.
    #[tokio::test]
    async fn a_connection_held_past_the_wait_bound_is_closed() {
        let port = free_port().await;
        let policy = HoldPolicy {
            max_held: 8,
            max_wait: Duration::from_millis(300),
        };
        let listener = FlowListener::bind(port, policy).await.expect("bind");
        let addr = listener.local_addr();

        let mut c = TcpStream::connect(addr).await.expect("accepted (held)");
        // Held for a while first — the bound is a bound, not an eager close.
        assert_still_open(&mut c, Duration::from_millis(100), "a freshly held client").await;
        // Then closed once the wait bound passes.
        assert_closed(&mut c, "the held client past max_wait").await;
        assert_eq!(listener.held(), 0, "the expired connection left the hold");

        // A carrier arriving afterwards gets nothing.
        let (b, mut b_rx) = fake("B");
        assert_eq!(listener.install(b), 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            b_rx.try_recv().is_err(),
            "an expired connection is never carried"
        );
    }

    /// AC3's count bound: with `max_held` connections waiting, the next one is
    /// closed at once while the held ones stay; the carrier then gets exactly
    /// the held ones, in arrival order. NC86C (no count bound) turns this red.
    #[tokio::test]
    async fn a_connection_beyond_the_count_bound_is_closed_and_the_held_ones_are_carried() {
        let port = free_port().await;
        let policy = HoldPolicy {
            max_held: 2,
            max_wait: Duration::from_secs(10),
        };
        let listener = FlowListener::bind(port, policy).await.expect("bind");
        let addr = listener.local_addr();

        let mut c1 = TcpStream::connect(addr).await.unwrap();
        c1.write_all(b"1").await.unwrap();
        let mut c2 = TcpStream::connect(addr).await.unwrap();
        c2.write_all(b"2").await.unwrap();
        let mut c3 = TcpStream::connect(addr).await.unwrap();
        c3.write_all(b"3").await.unwrap();
        // The third exceeds the bound: closed, not held.
        assert_closed(&mut c3, "the connection beyond max_held").await;
        assert_eq!(listener.held(), 2, "exactly max_held are held");
        assert_still_open(&mut c1, Duration::from_millis(100), "held #1").await;
        assert_still_open(&mut c2, Duration::from_millis(100), "held #2").await;

        let (b, mut b_rx) = fake("B");
        assert_eq!(listener.install(b), 2);
        let mut first = carried(&mut b_rx, "B").await;
        let mut second = carried(&mut b_rx, "B").await;
        let mut buf = [0u8; 1];
        first.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"1", "handed over in arrival order");
        second.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"2");
        assert!(
            b_rx.try_recv().is_err(),
            "the closed third connection is never carried"
        );
    }

    /// The flow dies (kill_flow aborts its supervisor, which drops the
    /// listener) while connections are held: every held connection is
    /// released (the client sees EOF) and the port is unbound again. NC86E (the
    /// accept task outlives the listener) turns this red.
    #[tokio::test]
    async fn dropping_the_listener_releases_held_connections_and_unbinds_the_port() {
        let port = free_port().await;
        let listener = FlowListener::<FakeCarrier>::bind(port, HoldPolicy::default())
            .await
            .expect("bind");
        let addr = listener.local_addr();
        let mut c = TcpStream::connect(addr).await.expect("accepted (held)");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(listener.held(), 1);

        drop(listener);

        assert_closed(&mut c, "a held client after the flow died").await;
        // The abort lands on the accept task's next poll, so give the runtime
        // a moment before declaring the port stuck.
        let mut rebound = false;
        for _ in 0..40 {
            if TcpListener::bind(addr).await.is_ok() {
                rebound = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            rebound,
            "the port must be free again once the flow's listener is dropped"
        );
    }

    /// A connection arriving while a carrier is current never touches the
    /// hold, and the hold is drained on install even when the policy allows
    /// a long wait — install is the hand-off, not the timer.
    #[tokio::test]
    async fn install_drains_the_hold_and_later_arrivals_bypass_it() {
        let port = free_port().await;
        let listener = FlowListener::bind(port, HoldPolicy::default())
            .await
            .expect("bind");
        let addr = listener.local_addr();
        assert!(!listener.has_carrier());
        let _held = TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(listener.held(), 1);

        let (b, mut b_rx) = fake("B");
        assert_eq!(listener.install(b), 1);
        assert!(listener.has_carrier());
        let _ = carried(&mut b_rx, "B").await;
        assert_eq!(listener.held(), 0);

        let _direct = TcpStream::connect(addr).await.unwrap();
        let _ = carried(&mut b_rx, "B").await;
        assert_eq!(listener.held(), 0, "with a carrier current nothing is held");
    }

    #[test]
    fn the_default_hold_bounds_are_the_documented_ones() {
        let p = HoldPolicy::default();
        assert_eq!(p.max_held, 64);
        assert_eq!(p.max_wait, Duration::from_secs(30));
    }

    /// FR-86 P2 — a re-upgrade PROMOTION is `install(new)`: after it, a NEW
    /// connection goes to the new carrier and never to the old one. (The old
    /// carrier's already-carried connections are untouched — proven end to end
    /// with real transports in `driver::tests::make_before_break_*`.) NC86P2A
    /// (install ignores the new carrier / keeps the old) turns this red.
    #[tokio::test]
    async fn install_promotes_so_new_connections_go_to_the_new_carrier() {
        let port = free_port().await;
        let listener = FlowListener::bind(port, HoldPolicy::default())
            .await
            .expect("bind");
        let addr = listener.local_addr();
        let (a, mut a_rx) = fake("A");
        let (b, mut b_rx) = fake("B");

        // A is current: a connection is carried by A.
        listener.install(Arc::clone(&a));
        let _c1 = TcpStream::connect(addr)
            .await
            .expect("connect while A serves");
        let _ = carried(&mut a_rx, "A").await;

        // Promote to B. From now on new connections ride B, not A.
        listener.install(Arc::clone(&b));
        let _c2 = TcpStream::connect(addr)
            .await
            .expect("connect while B serves");
        let _ = carried(&mut b_rx, "B").await;
        assert!(
            a_rx.try_recv().is_err(),
            "a connection after promotion must not reach the old carrier A"
        );
    }

    /// #1816 — a promotion can never hand a connection to a carrier whose
    /// drain has already seen it at zero: the listener carries — COUNTS — a
    /// connection on the current carrier under the same lock `install` swaps
    /// carriers under, so `install(B)` cannot complete while A's `carry` is in
    /// progress. (Before this `offer` cloned A's `Arc`, released the lock and
    /// then carried; in that gap `install(B)` + A's reaper, seeing
    /// `active() == 0`, closed A under the connection about to be counted on
    /// it.) NC1816-2 (carry after releasing the lock) turns this red.
    ///
    /// A multi-thread runtime so the accept task's worker can block inside A's
    /// `carry`. The whole observation runs OFF the runtime — on the blocking
    /// pool, with `install` on its own OS thread — because a blocking `carry`
    /// occupies a runtime worker and the test body, if it were an ordinary
    /// `async` task, gets starved behind it: measured, it did not resume to
    /// check the invariant until carry returned 5 s later, long after the
    /// window it meant to observe. Sync `std` sockets and sleeps are immune to
    /// that, and the runtime is left to drive only the accept loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_cannot_complete_while_a_carry_on_the_old_carrier_is_in_progress() {
        let port = free_port().await;
        let listener = Arc::new(
            FlowListener::bind(port, HoldPolicy::default())
                .await
                .expect("bind"),
        );
        let addr = listener.local_addr();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let a_returned = Arc::new(AtomicBool::new(false));
        let a = Arc::new(BlockingCarrier {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            returned: Arc::clone(&a_returned),
        });
        // B carries nothing here; built so that even if it did, it would not
        // block (its release sender is already dropped ⇒ recv returns at once).
        let b = Arc::new(BlockingCarrier {
            entered: std::sync::mpsc::channel::<()>().0,
            release: Mutex::new(std::sync::mpsc::channel::<()>().1),
            returned: Arc::new(AtomicBool::new(false)),
        });

        listener.install(Arc::clone(&a));
        let listener2 = Arc::clone(&listener);
        let a_returned2 = Arc::clone(&a_returned);
        tokio::task::spawn_blocking(move || {
            // A sync connect triggers the accept loop (on a runtime worker),
            // which enters A's `carry` and parks there, holding the slot lock.
            let _c = std::net::TcpStream::connect(addr).expect("a client connects while A serves");
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("A's carry is entered for the connection");

            // Promote to B from its own OS thread: `install` must WAIT for the
            // slot lock A's carry holds. It returns whether A's carry had
            // already returned by the time the swap completed — true is the
            // invariant: the connection was counted on A before the carrier
            // changed under it.
            let install = {
                let listener = Arc::clone(&listener2);
                let a_returned = Arc::clone(&a_returned2);
                std::thread::spawn(move || {
                    listener.install(b);
                    a_returned.load(Ordering::SeqCst)
                })
            };
            std::thread::sleep(Duration::from_millis(300));
            assert!(
                !install.is_finished(),
                "install(B) must not complete while A's carry is in progress"
            );
            assert!(!a_returned2.load(Ordering::SeqCst));

            // Release A's carry: install completes — and only after A's carry
            // returned, the connection counted on A.
            release_tx
                .send(())
                .expect("A's carry is waiting on the release");
            let a_had_returned = install.join().expect("the install thread");
            assert!(
                a_had_returned,
                "install(B) completed only after A's carry had returned"
            );
        })
        .await
        .expect("the probe task");
    }
}
