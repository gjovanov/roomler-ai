// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-70 M1 — the encoder thread.
//!
//! One dedicated OS thread per session owns the encoder and serves a bounded
//! command channel. The pump keeps every decision it makes today; at each
//! decision site a method call becomes a message, and the reply comes back
//! over a oneshot. Nothing about *what* is decided changes — the same frame,
//! the same decision, the same packet, one thread hop later.
//!
//! # Why a thread, not `spawn_blocking`
//!
//! The encode is `!Send`-shaped in practice even where the type is `Send`:
//! Media Foundation wants per-thread COM / `MFStartup`, and a QSV session is
//! thread-affine. Today the FFmpeg pump encodes with `block_in_place` on
//! whichever runtime worker polls it, which both breaks that affinity and
//! holds a worker for the 5–30 ms of every encode — the send task, the
//! control channel and the heartbeats all share that worker. A thread the
//! encoder lives on for the whole session satisfies the affinity by
//! construction and never touches the runtime.
//!
//! # The contract
//!
//! Every command is answered in order, on the thread, before the next one is
//! read: `encode` cannot overtake a `set_bitrate` sent before it, and a
//! `set_bitrate` awaited by the pump has been applied when the await
//! returns — exactly the sequencing the inline encoder gives. Dropping the
//! handle closes the channel; the thread drains what it already holds, drops
//! the encoder ON that thread (destruction where construction affinity says
//! it belongs), and exits. A thread that died (an encoder panic) surfaces as
//! an error on the next command, which the pump's existing error ladder turns
//! into a rebuild — no new failure mode.
//!
//! Generic over [`EncoderOps`] so it unit-tests on the default build with a
//! fake; the FFmpeg encoder implements the trait behind its feature.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

use anyhow::{Result, anyhow};
use tokio::sync::oneshot;

use crate::capture::Frame;

/// Properties of a built encoder that the pump reads synchronously and often.
/// Stable for the encoder's lifetime (an adoption keeps dims and backend, or
/// is refused), so the handle caches one copy and never asks the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderCaps {
    pub name: &'static str,
    pub supports_dynamic_bitrate: bool,
    pub reconfig_forces_idr: bool,
    pub chroma444: bool,
}

/// What the thread needs from an encoder. Deliberately the pump's surface and
/// nothing more — every method here has a call site in `media_pump_ffmpeg_dc`.
pub trait EncoderOps: Send + 'static {
    /// A replacement built elsewhere (the background rebuild) that this
    /// encoder may adopt in place.
    type Rebuilt: Send + 'static;
    /// A pure description of how to rebuild this encoder at a new rate.
    type RebuildSpec: Send + 'static;
    /// Counters the heartbeat prints.
    type Stats: Send + 'static;

    fn encode_sync(&mut self, frame: &Frame) -> Result<Vec<crate::encode::EncodedPacket>>;
    fn set_bitrate(&mut self, bps: u32);
    fn request_keyframe(&mut self);
    /// `true` = adopted (the previous encoder is gone); `false` = refused
    /// (dims/backend no longer match) and `self` is untouched.
    fn adopt_rebuilt(&mut self, rebuilt: Self::Rebuilt) -> bool;
    fn rebuild_spec(&self, bps: u32) -> Option<Self::RebuildSpec>;
    fn caps(&self) -> EncoderCaps;
    /// The maxrate the encoder is currently configured for.
    fn current_maxrate_bps(&self) -> u32;
    fn rate_stats(&self) -> Self::Stats;
}

enum Cmd<E: EncoderOps> {
    Encode(
        Arc<Frame>,
        oneshot::Sender<Result<Vec<crate::encode::EncodedPacket>>>,
    ),
    /// Replies with the maxrate after the move, so the handle's mirror is
    /// exact the moment the await returns.
    SetBitrate(u32, oneshot::Sender<u32>),
    RequestKeyframe(oneshot::Sender<()>),
    Adopt(E::Rebuilt, oneshot::Sender<(bool, u32)>),
    RebuildSpec(u32, oneshot::Sender<Option<E::RebuildSpec>>),
    RateStats(oneshot::Sender<E::Stats>),
}

/// Depth of the command channel. The pump awaits every command it sends, so
/// one in flight is the steady state; the headroom covers a keyframe request
/// racing an encode from another task without ever blocking a sender.
const CMD_DEPTH: usize = 8;

/// The pump's end of the encoder thread.
pub struct EncoderThread<E: EncoderOps> {
    tx: Option<SyncSender<Cmd<E>>>,
    join: Option<std::thread::JoinHandle<()>>,
    caps: EncoderCaps,
    maxrate_bps: u32,
    name: String,
}

impl<E: EncoderOps> EncoderThread<E> {
    /// Move `enc` onto its own thread. `label` names the thread
    /// (`rc-enc-<label>`) so it is recognisable in a debugger or a profiler.
    ///
    /// A spawn failure hands the encoder BACK: the thread is created empty
    /// and receives the encoder only once it exists, so the caller can fall
    /// back to using it inline rather than losing an open it paid for.
    pub fn spawn(enc: E, label: &str) -> std::result::Result<Self, (E, std::io::Error)> {
        let caps = enc.caps();
        let maxrate_bps = enc.current_maxrate_bps();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Cmd<E>>(CMD_DEPTH);
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<E>(1);
        let name = format!("rc-enc-{label}");
        let join = match std::thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                // The encoder arrives right after the spawn succeeded; a sender
                // dropped without sending means the caller kept it, and this
                // thread simply ends.
                if let Ok(enc) = init_rx.recv() {
                    serve(enc, rx);
                }
            }) {
            Ok(join) => join,
            Err(e) => return Err((enc, e)),
        };
        // Cannot fail: the receiver is alive on the thread we just spawned
        // and the channel holds one slot.
        let _ = init_tx.send(enc);
        Ok(Self {
            tx: Some(tx),
            join: Some(join),
            caps,
            maxrate_bps,
            name,
        })
    }

    pub fn caps(&self) -> EncoderCaps {
        self.caps
    }

    pub fn current_maxrate_bps(&self) -> u32 {
        self.maxrate_bps
    }

    /// The thread's name, for logs.
    pub fn thread_name(&self) -> &str {
        &self.name
    }

    fn send(&self, cmd: Cmd<E>) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("encoder thread already closed"))?;
        // `send` blocks only when CMD_DEPTH commands are queued, which the
        // await-every-command pump never reaches; `try_send` first so a
        // dead thread (disconnected) is reported rather than blocked on.
        match tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(anyhow!("encoder thread is gone")),
            Err(TrySendError::Full(cmd)) => {
                tx.send(cmd).map_err(|_| anyhow!("encoder thread is gone"))
            }
        }
    }

    async fn ask<T>(&self, cmd: Cmd<E>, rx: oneshot::Receiver<T>) -> Result<T> {
        self.send(cmd)?;
        rx.await
            .map_err(|_| anyhow!("encoder thread dropped the command (it died mid-command)"))
    }

    pub async fn encode(&self, frame: Arc<Frame>) -> Result<Vec<crate::encode::EncodedPacket>> {
        let (tx, rx) = oneshot::channel();
        self.ask(Cmd::Encode(frame, tx), rx).await?
    }

    pub async fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.maxrate_bps = self.ask(Cmd::SetBitrate(bps, tx), rx).await?;
        Ok(())
    }

    pub async fn request_keyframe(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.ask(Cmd::RequestKeyframe(tx), rx).await
    }

    pub async fn adopt_rebuilt(&mut self, rebuilt: E::Rebuilt) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        let (adopted, maxrate) = self.ask(Cmd::Adopt(rebuilt, tx), rx).await?;
        self.maxrate_bps = maxrate;
        Ok(adopted)
    }

    pub async fn rebuild_spec(&self, bps: u32) -> Result<Option<E::RebuildSpec>> {
        let (tx, rx) = oneshot::channel();
        self.ask(Cmd::RebuildSpec(bps, tx), rx).await
    }

    pub async fn rate_stats(&self) -> Result<E::Stats> {
        let (tx, rx) = oneshot::channel();
        self.ask(Cmd::RateStats(tx), rx).await
    }

    /// Has the thread exited (an encoder panic)? The next command would fail
    /// either way; this lets a caller notice without sending one.
    pub fn is_alive(&self) -> bool {
        self.join.as_ref().is_some_and(|j| !j.is_finished())
    }
}

impl<E: EncoderOps> Drop for EncoderThread<E> {
    fn drop(&mut self) {
        // Close the channel first so the thread's `recv` returns, then join:
        // the encoder is destroyed on the thread that owned it. A join on a
        // thread that panicked yields `Err`, which there is nothing to do
        // about here — the panic was already reported when it happened.
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn serve<E: EncoderOps>(mut enc: E, rx: Receiver<Cmd<E>>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Encode(frame, reply) => {
                let _ = reply.send(enc.encode_sync(&frame));
            }
            Cmd::SetBitrate(bps, reply) => {
                enc.set_bitrate(bps);
                let _ = reply.send(enc.current_maxrate_bps());
            }
            Cmd::RequestKeyframe(reply) => {
                enc.request_keyframe();
                let _ = reply.send(());
            }
            Cmd::Adopt(rebuilt, reply) => {
                let adopted = enc.adopt_rebuilt(rebuilt);
                let _ = reply.send((adopted, enc.current_maxrate_bps()));
            }
            Cmd::RebuildSpec(bps, reply) => {
                let _ = reply.send(enc.rebuild_spec(bps));
            }
            Cmd::RateStats(reply) => {
                let _ = reply.send(enc.rate_stats());
            }
        }
    }
    // `enc` drops here, on this thread.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Damage, Frame, PixelFormat};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn frame(tag: u8) -> Arc<Frame> {
        Arc::new(Frame {
            width: 2,
            height: 2,
            stride: 8,
            pixel_format: PixelFormat::Bgra,
            data: vec![tag; 16],
            monotonic_us: u64::from(tag) * 33_333,
            monitor: 0,
            damage: Damage::Unknown,
            source: None,
        })
    }

    /// Records every call in order, on whatever thread it ran.
    struct Fake {
        log: Arc<Mutex<Vec<String>>>,
        maxrate: u32,
        dropped_on: Arc<Mutex<Option<String>>>,
        fail_encode: bool,
        panic_on_encode: bool,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let name = std::thread::current().name().unwrap_or("?").to_string();
            *self.dropped_on.lock().unwrap() = Some(name);
        }
    }

    impl EncoderOps for Fake {
        type Rebuilt = u32;
        type RebuildSpec = (u32, u32);
        type Stats = usize;

        fn encode_sync(&mut self, frame: &Frame) -> Result<Vec<crate::encode::EncodedPacket>> {
            if self.panic_on_encode {
                panic!("encoder exploded");
            }
            let thread = std::thread::current().name().unwrap_or("?").to_string();
            self.log
                .lock()
                .unwrap()
                .push(format!("encode:{}@{thread}", frame.data[0]));
            if self.fail_encode {
                return Err(anyhow!("hw says no"));
            }
            Ok(vec![crate::encode::EncodedPacket {
                data: vec![frame.data[0]],
                is_keyframe: false,
                duration_us: 33_333,
                qp: None,
            }])
        }
        fn set_bitrate(&mut self, bps: u32) {
            self.maxrate = bps;
            self.log.lock().unwrap().push(format!("rate:{bps}"));
        }
        fn request_keyframe(&mut self) {
            self.log.lock().unwrap().push("idr".into());
        }
        fn adopt_rebuilt(&mut self, rebuilt: u32) -> bool {
            self.log.lock().unwrap().push(format!("adopt:{rebuilt}"));
            if rebuilt == 0 {
                return false;
            }
            self.maxrate = rebuilt;
            true
        }
        fn rebuild_spec(&self, bps: u32) -> Option<(u32, u32)> {
            (bps > 0).then_some((self.maxrate, bps))
        }
        fn caps(&self) -> EncoderCaps {
            EncoderCaps {
                name: "fake",
                supports_dynamic_bitrate: true,
                reconfig_forces_idr: false,
                chroma444: false,
            }
        }
        fn current_maxrate_bps(&self) -> u32 {
            self.maxrate
        }
        fn rate_stats(&self) -> usize {
            self.log.lock().unwrap().len()
        }
    }

    fn fake(log: &Arc<Mutex<Vec<String>>>, dropped_on: &Arc<Mutex<Option<String>>>) -> Fake {
        Fake {
            log: log.clone(),
            maxrate: 1_000_000,
            dropped_on: dropped_on.clone(),
            fail_encode: false,
            panic_on_encode: false,
        }
    }

    #[tokio::test]
    async fn commands_run_in_order_on_the_named_thread() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let dropped_on = Arc::new(Mutex::new(None));
        let mut t = EncoderThread::spawn(fake(&log, &dropped_on), "test")
            .ok()
            .expect("spawn");
        assert_eq!(t.caps().name, "fake");
        assert_eq!(t.current_maxrate_bps(), 1_000_000);

        let p = t.encode(frame(7)).await.unwrap();
        assert_eq!(p[0].data, vec![7]);
        t.set_bitrate(500_000).await.unwrap();
        assert_eq!(
            t.current_maxrate_bps(),
            500_000,
            "the mirror is exact after the await"
        );
        t.request_keyframe().await.unwrap();
        let p = t.encode(frame(9)).await.unwrap();
        assert_eq!(p[0].data, vec![9]);
        assert_eq!(
            t.rebuild_spec(250_000).await.unwrap(),
            Some((500_000, 250_000))
        );
        assert_eq!(t.rebuild_spec(0).await.unwrap(), None);
        assert!(t.adopt_rebuilt(750_000).await.unwrap());
        assert_eq!(t.current_maxrate_bps(), 750_000);
        assert!(!t.adopt_rebuilt(0).await.unwrap(), "a refused adoption");
        assert_eq!(t.current_maxrate_bps(), 750_000, "…leaves the mirror alone");
        let n = t.rate_stats().await.unwrap();

        let got = log.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                "encode:7@rc-enc-test",
                "rate:500000",
                "idr",
                "encode:9@rc-enc-test",
                "adopt:750000",
                "adopt:0",
            ]
        );
        assert_eq!(n, got.len());
    }

    #[tokio::test]
    async fn dropping_the_handle_destroys_the_encoder_on_its_own_thread() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let dropped_on = Arc::new(Mutex::new(None));
        let t = EncoderThread::spawn(fake(&log, &dropped_on), "drop")
            .ok()
            .expect("spawn");
        assert!(t.is_alive());
        drop(t);
        assert_eq!(dropped_on.lock().unwrap().as_deref(), Some("rc-enc-drop"));
    }

    #[tokio::test]
    async fn an_encode_error_is_the_encoders_error_not_the_threads() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let dropped_on = Arc::new(Mutex::new(None));
        let mut f = fake(&log, &dropped_on);
        f.fail_encode = true;
        let t = EncoderThread::spawn(f, "err").ok().expect("spawn");
        let e = t.encode(frame(1)).await.unwrap_err();
        assert!(e.to_string().contains("hw says no"), "{e}");
        assert!(t.is_alive(), "an error does not kill the thread");
        // And it still answers.
        assert!(t.request_keyframe().await.is_ok());
    }

    #[tokio::test]
    async fn a_dead_thread_surfaces_as_an_error_on_the_next_command() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let dropped_on = Arc::new(Mutex::new(None));
        let mut f = fake(&log, &dropped_on);
        f.panic_on_encode = true;
        let t = EncoderThread::spawn(f, "panic").ok().expect("spawn");
        // Silence the panic's default hook output for this test only.
        let quiet = Arc::new(AtomicBool::new(true));
        let prev = std::panic::take_hook();
        let q = quiet.clone();
        std::panic::set_hook(Box::new(move |info| {
            if !q.load(Ordering::Relaxed) {
                eprintln!("{info}");
            }
        }));
        let first = t.encode(frame(1)).await;
        std::panic::set_hook(prev);
        assert!(
            first.is_err(),
            "the command in flight when the thread died fails"
        );
        // The thread has exited; the handle knows and every later command fails
        // at the send, not by blocking.
        for _ in 0..50 {
            if !t.is_alive() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(!t.is_alive());
        assert!(t.request_keyframe().await.is_err());
        assert!(t.encode(frame(2)).await.is_err());
    }
}
