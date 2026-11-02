// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2d-1 — the device's worker (decision 63): one `roomlerd
//! hive-worker` for the device, started and kept up by the daemon, for the
//! parts that serve the device and no one session. The prober's measure is
//! the first in it, as decision 70 orders them; the viewer's peer, the stream
//! and the store follow (the FR-90 spec, §3i "P8-2d-1, as built").
//!
//! | Piece | As built |
//! |---|---|
//! | its start | eagerly, at `hive::init`, where the device's `hive_worker` says so, in a build with the vault (decision 73), once: [`start`] |
//! | its life | the daemon's: it has no run to end, so no session's start or end touches it, and only the daemon's own end stops it ([`wind_down`]) |
//! | its hello | `hello {session: "device", fence: 0}` ([`HELLO`]): the frame every worker ever released reads, never a variant of its own, which one that predates it could not parse |
//! | its fault | found as a session's worker's is, by the one loop ([`Served`], decision 71): a hello within 10 s, a beat every 5 s, three missed; restarts after 1, 2, 4 … 30 s |
//! | giving up | five starts within ten minutes, or a worker of another version at once: it is parked ([`COOL_OFF`]), and never for good. A fault that repeats costs one start a cool-off |
//! | while parked | no process and no channel, one timer in the daemon. A measure is answered `failed` at once, with why ([`DeviceWorker::up`]): never run in the daemon instead |
//! | a measure | a call, bounded by [`DEVICE_CALL_WAIT`](super::link::DEVICE_CALL_WAIT), on a task of its own in the worker, at most [`MAX_MEASURES`] at once, so no probe keeps a beat from being answered. The daemon keeps the prober: what it was asked, its sixteen answers, the connection, the list read under the config's lock |
//! | its memory | its own bound ([`MAX_MEMORY`], decision 72), off swap: `memory.max` where the unit delegates a cgroup, the job's limit on Windows. The worker spawns nothing: every spawn as a session's account stays the daemon's, outside both |
//!
//! ⚠️ **A worker of another version, either way.** Linux runs the daemon's own
//! image (`/proc/self/exe`), so it never meets one. Elsewhere the path can name
//! another build. An older one reads this hello as a session's and answers
//! `ready`; a newer one knows it and answers `ready`. Each says its version,
//! the daemon takes that for [`Ended::Foreign`](super::Ended), and parks: one
//! start a cool-off, until the daemon itself restarts into the next build. No
//! parse error, no crash loop, and nothing it cannot serve is ever handed to it.
//!
//! ⚠️ **`hive_worker` is read once, at `init`.** A local edit waits for the next
//! restart, as a session's worker's does; no server can set the key.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use roomler_ai_remote_control::models::DestinationRule;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::link::{DeviceLink, DeviceRole};
use super::{
    DeviceAnswer, DeviceCall, MAX_FRAME, Msg, Program, Running, Seen, Served, Spawn, Theirs,
    Timing, UP_WAIT, VERSION, log, recv, supervise,
};
use crate::hive::device::{InDaemon, Measure};
use crate::hive::knowhow_probe::{self, Asked, Probed};

/// What a hello says in place of a session: an `ObjectId`'s hex never reads so.
pub(super) const HELLO: &str = "device";

/// How long the device's worker stays parked once the daemon gives up on it
/// (decision 71).
const COOL_OFF: Duration = Duration::from_secs(10 * 60);

/// The most memory the device's worker holds (decision 72): a constant of its
/// own, apart from a session's, until P8-2d-3 measures it.
pub(crate) const MAX_MEMORY: u64 = 1 << 30;

/// The measures the worker runs at once. The server asks a device one probe at
/// a time; past this it is refused at once, never queued: a call that waits
/// here would run into its own bound, which takes the worker for lost.
const MAX_MEASURES: usize = 8;

/// The most the daemon's end waits for the worker to go: `bye`'s grace, and
/// a kill.
const STOP_WAIT: Duration = Duration::from_secs(3);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ─── The worker's side ──────────────────────────────────────────────────────

/// A worker's life as the device's, its hello read ([`super::serve_halves`]):
/// `ready`, then each beat answered here and each call on a task of its own,
/// until `bye` or the channel's end. Its log crosses from its `ready` on, as a
/// session's worker's does.
pub(super) async fn serve(
    mut rd: impl AsyncRead + Unpin,
    wr: impl AsyncWrite + Send + Unpin + 'static,
    lines: Option<log::Lines>,
) -> std::io::Result<()> {
    crate::logging::say_on_stderr(format_args!("roomlerd hive-worker: serving the device"));
    let link = Arc::new(DeviceLink::worker(wr));
    let ready = Msg::Ready {
        pid: std::process::id(),
        version: VERSION.to_string(),
    };
    link.send(&ready).await?;
    if let Some(lines) = lines {
        tokio::spawn(log::forward(lines, Arc::clone(&link)));
    }
    info!("hive: this worker serves the device");
    let measures = Arc::new(Semaphore::new(MAX_MEASURES));
    let ended = loop {
        let m = match recv(&mut rd).await {
            Ok(Some(m)) => m,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        let sent = match m {
            Msg::Beat { n } => link.send(&Msg::Beat { n }).await,
            Msg::Bye => break Ok(()),
            // Off this loop: a measure can take its whole deadline, and a
            // beat left unanswered that long reads as a hung worker.
            Msg::DeviceCall { id, call } => {
                let (link, measures) = (Arc::clone(&link), Arc::clone(&measures));
                tokio::spawn(async move {
                    let answer = answer_call(call, measures).await;
                    let _ = link.answer(id, answer).await;
                });
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(e) = sent {
            break Err(e);
        }
    };
    link.end();
    ended
}

/// The worker's answer to one of the daemon's calls.
async fn answer_call(call: DeviceCall, measures: Arc<Semaphore>) -> DeviceAnswer {
    match call {
        DeviceCall::Measure { asked, rules } => {
            let Ok(held) = measures.try_acquire_owned() else {
                return DeviceAnswer::Refused {
                    why: format!("it runs {MAX_MEASURES} measures at once"),
                };
            };
            // On a task of its own, as a measure parses what the host it
            // reached shows it: one that panics costs that probe alone.
            let measured = tokio::spawn(async move {
                let _held = held;
                knowhow_probe::probe(&asked, &rules).await
            })
            .await;
            match measured {
                Ok(probed) => DeviceAnswer::Probed { probed },
                Err(e) => DeviceAnswer::Refused {
                    why: format!("its measure did not finish: {e}"),
                },
            }
        }
    }
}

// ─── The daemon's side ──────────────────────────────────────────────────────

/// What its supervision tells whoever asks a measure of it.
#[derive(Default)]
struct Shared {
    /// The link of the worker that is up now.
    link: Mutex<Option<Arc<DeviceLink>>>,
    /// Why it is parked, while it is.
    parked: Mutex<Option<String>>,
    /// How its last worker was lost: a park's reason says it.
    last: Mutex<Option<String>>,
}

/// The device, as the one supervision loop keeps its worker up.
struct DeviceRun {
    shared: Arc<Shared>,
    cool_off: Duration,
}

impl Served for DeviceRun {
    type Role = DeviceRole;

    fn hello(&self) -> Msg {
        Msg::Hello {
            session: HELLO.to_string(),
            fence: 0,
        }
    }

    fn came_up(&self, pid: u32, link: &Arc<DeviceLink>) {
        info!(pid, "hive: the device's worker is up");
        *lock(&self.shared.link) = Some(Arc::clone(link));
    }

    fn went(&self) {
        lock(&self.shared.link).take();
    }

    fn frame(&self, link: &Arc<DeviceLink>, m: Msg) {
        if let Msg::DeviceAnswer { id, answer } = m {
            link.answered(id, answer);
        }
    }

    fn lost(&self, why: &str, wait: Duration) {
        warn!(%why, wait_ms = wait.as_millis() as u64, "hive: the device's worker was lost: starting another");
        *lock(&self.shared.last) = Some(why.to_string());
    }

    fn gave_up(&self, why: &str) -> Option<Duration> {
        let said = match lock(&self.shared.last).take() {
            Some(last) => format!("{why}: {last}"),
            None => why.to_string(),
        };
        warn!(
            why = %said, cool_off_secs = self.cool_off.as_secs(),
            "hive: the device's worker is parked: another is tried after the cool-off"
        );
        *lock(&self.shared.parked) = Some(said);
        Some(self.cool_off)
    }

    fn cooled_off(&self) {
        info!("hive: the device's worker's cool-off is over: starting another");
        lock(&self.shared.parked).take();
    }
}

/// The device's worker, as the daemon holds it: what its prober measures in.
pub(crate) struct DeviceWorker {
    shared: Arc<Shared>,
    seen: Arc<Mutex<Seen>>,
    stop: CancellationToken,
    cool_off: Duration,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DeviceWorker {
    /// Keep a worker up until stopped, parked for `cool_off` each time the
    /// daemon gives up on it.
    pub(crate) fn start(spawner: Arc<dyn Spawn>, timing: Timing, cool_off: Duration) -> Arc<Self> {
        let shared = Arc::new(Shared::default());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = CancellationToken::new();
        let task = tokio::spawn(supervise(
            DeviceRun {
                shared: Arc::clone(&shared),
                cool_off,
            },
            spawner,
            timing,
            stop.clone(),
            Arc::clone(&seen),
        ));
        Arc::new(Self {
            shared,
            seen,
            stop,
            cool_off,
            task: Mutex::new(Some(task)),
        })
    }

    /// What it has seen so far.
    #[cfg(test)]
    pub(crate) fn seen(&self) -> Seen {
        lock(&self.seen).clone()
    }

    /// End it: `bye` to the worker, then a kill after the grace, and its
    /// supervisor waited for, within [`STOP_WAIT`].
    pub(crate) async fn stop(&self) {
        self.stop.cancel();
        let task = lock(&self.task).take();
        if let Some(task) = task
            && tokio::time::timeout(STOP_WAIT, task).await.is_err()
        {
            warn!(
                pid = ?lock(&self.seen).pid,
                "hive: the device's worker did not go in time: it ends with the daemon"
            );
        }
    }

    /// The link of the worker that is up, waiting at most [`UP_WAIT`] for one
    /// that is starting. Why there is none, at once where it is parked.
    async fn up(&self) -> Result<Arc<DeviceLink>, String> {
        let deadline = Instant::now() + UP_WAIT;
        loop {
            if let Some(link) = lock(&self.shared.link).clone() {
                return Ok(link);
            }
            if let Some(why) = lock(&self.shared.parked).clone() {
                return Err(format!(
                    "the device's worker is parked ({why}); another is tried within {:?}",
                    self.cool_off
                ));
            }
            if self.stop.is_cancelled() {
                return Err("the daemon is stopping".into());
            }
            if Instant::now() >= deadline {
                return Err(format!("the device's worker was not up within {UP_WAIT:?}"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for DeviceWorker {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Whether `call` fits a frame, with room for the frame's own words: one that
/// did not would be refused as it is written, which takes the link for lost,
/// and one oversized word from the server must not cost the worker its life.
fn fits(call: &DeviceCall) -> bool {
    serde_json::to_vec(call).is_ok_and(|body| body.len() + 64 <= MAX_FRAME)
}

/// FR-90 P8-2d-1 — a probe's measure, in the worker. What a caller sees is
/// the probe's own answer or a `failed` that says why: the worker parked, not
/// up in time, or gone before it answered, when the channel's end fails the call
/// at once. It is not run again: the daemon's prober runs a probe id once.
#[async_trait::async_trait]
impl Measure for DeviceWorker {
    async fn measure(&self, asked: Asked, rules: Vec<DestinationRule>) -> Probed {
        let call = DeviceCall::Measure { asked, rules };
        if !fits(&call) {
            return Probed::failed("the probe does not fit the device's worker's channel".into());
        }
        let link = match self.up().await {
            Ok(link) => link,
            Err(why) => return Probed::failed(why),
        };
        #[cfg(unix)]
        let answer = link.call(call, Vec::new());
        #[cfg(windows)]
        let answer = link.call(call);
        match answer.await {
            Ok(DeviceAnswer::Probed { probed }) => probed,
            Ok(DeviceAnswer::Refused { why }) => {
                Probed::failed(format!("the device's worker refused it: {why}"))
            }
            Err(why) => Probed::failed(format!("the device's worker did not answer: {why}")),
        }
    }
}

/// This binary's own worker, held to the device's bound.
pub(super) struct OwnBinary(pub(super) Program);

impl Spawn for OwnBinary {
    fn spawn(&self, theirs: Theirs) -> std::io::Result<Box<dyn Running>> {
        self.0.spawn_held_to(MAX_MEMORY, theirs)
    }

    fn prepare(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.0.prepare()
    }
}

/// No worker can be started: this binary's path is unknown. It parks as any
/// worker that cannot start does, so a probe says why, and the owner's
/// `hive_worker` still means no part of this runs in the daemon.
struct NoBinary(String);

impl Spawn for NoBinary {
    fn spawn(&self, _theirs: Theirs) -> std::io::Result<Box<dyn Running>> {
        Err(std::io::Error::other(self.0.clone()))
    }
}

/// The one a device has.
struct Slot(OnceLock<Arc<DeviceWorker>>);

impl Slot {
    const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// Its worker, made by `make` once, where the owner turned workers on.
    fn start(&self, on: bool, make: impl FnOnce() -> Arc<DeviceWorker>) {
        if on {
            self.0.get_or_init(make);
        }
    }

    /// What a measure runs in: its worker, or with none, this process.
    fn measure(&self) -> Arc<dyn Measure> {
        match self.0.get() {
            Some(worker) => Arc::clone(worker) as Arc<dyn Measure>,
            None => Arc::new(InDaemon),
        }
    }
}

static DEVICE: Slot = Slot::new();

/// `hive::init`: where the device's owner turned workers on (`on`, its
/// `hive_worker`), start its worker, once for the daemon's life.
pub(crate) fn start(on: bool) {
    DEVICE.start(on, || {
        let spawner: Arc<dyn Spawn> = match Program::this_binary() {
            Ok(program) => Arc::new(OwnBinary(program)),
            Err(e) => Arc::new(NoBinary(format!("this binary's own path is unknown: {e}"))),
        };
        DeviceWorker::start(spawner, Timing::default(), COOL_OFF)
    });
}

/// What this daemon's prober measures in ([`crate::hive::device::measure`]).
pub(crate) fn measure() -> Arc<dyn Measure> {
    DEVICE.measure()
}

/// The daemon is leaving: its worker goes with it, told `bye`. One that is
/// hung costs its grace, and it ends at the daemon's end all the same.
pub(crate) async fn wind_down() {
    if let Some(worker) = DEVICE.0.get() {
        worker.stop().await;
    }
}

#[cfg(test)]
pub(crate) mod tests;
