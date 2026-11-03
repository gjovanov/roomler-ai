// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-93 P5h — a computer going to sleep says so to every viewer first.
//!
//! A sleeping host says nothing, so its viewer saw the picture stop with no
//! reason. Measured on the FR-93 lab (2026-10-15, GNOME 50 per-user, 0.4.136):
//! the viewer dropped 5.6 s into a suspend and showed no reason for the 30 s
//! the computer slept. AC7's rule is "recover within 10 s, or name the reason".
//!
//! So while any remote-control session is live, the daemon holds a logind
//! **delay** inhibitor on `sleep`. logind announces every sleep with
//! `PrepareForSleep(true)` and then waits for its delay inhibitors (up to its
//! `InhibitDelayMaxSec`, 5 s by default). The watcher sends each live
//! session's controller `rc:media-waiting` for `host_sleep` over that
//! session's own control channel, lets go, and the computer sleeps. After the
//! wake the viewer reconnects by itself, and the new session names whatever
//! then holds the picture back (a lock screen: P5b).
//!
//! ⚠️ A *delay* inhibitor, never a *block*: the person at the computer keeps
//! the last word about sleep. Keeping a host awake through a session is
//! FR-55's rule 1, not this.
//! ⚠️ Held only while a session is live, so an idle daemon shows nothing in
//! `systemd-inhibit --list`.
//!
//! The registry is on every platform (every session registers its control
//! channel). The Linux watcher needs D-Bus (`portal-capture`, which every
//! shipped Linux agent carries through `full`).
//!
//! Kill switch `ROOMLERD_SLEEP_NOTICE=0`; P2d-2's `ROOMLERD_MEDIA_WAITING=0`
//! silences it as it does every waiting notice.
//!
//! # FR-55 P4 — and to the server, on every OS
//!
//! The same moment is the device's goodbye. Every live control connection
//! sends `rc:agent.sleeping` with the power posture at that moment
//! ([`crate::power::brief_now`]) and closes itself ([`register_control`] and
//! its arm in `signaling::connect_once`). The server records the notice and
//! tears the connection down at once, so a dashboard reads "asleep", and
//! whether the power policy let it sleep, instead of green until the receive
//! deadline reaps a socket nobody answers and then a bare "offline". The run
//! loop holds every reconnect until the wake ([`hold_while_asleep`]): without
//! it the loop would dial straight back in, inside the very window the OS gave
//! the device to go to sleep, and that hello would erase the notice.
//!
//! The hooks, one per OS, only listen:
//!
//! | OS | the hook | how long the OS waits for us |
//! |---|---|---|
//! | Linux | logind `PrepareForSleep(true)`, the watcher below | while we hold the delay inhibitor (5 s by default), else not at all |
//! | Windows | `PowerRegisterSuspendResumeNotification`, `PBT_APMSUSPEND` | for the callback, about 2 s |
//! | macOS | `IORegisterForSystemPower`, `kIOMessageSystemWillSleep` | until `IOAllowPowerChange`, 30 s at most |
//!
//! ⚠️ None of them takes an assertion, so `pmset -g assertions` and
//! `powercfg /requests` show nothing new and `power_policy = never` stays
//! byte-for-byte (FR-55 AC3). For the same reason the Linux delay inhibitor
//! keeps P5h's scope on a `never` device: live sessions only. It covers live
//! control connections only under `on-ac` and `always`, so a `never` device's
//! goodbye on Linux is best effort, sent in whatever time logind leaves (an
//! open decision in `docs/fr/FR-55-always-reachable.md`).
//! ⚠️ A hook never blocks on the runtime. [`will_sleep`] spawns the work and
//! waits on a std channel with a hard bound, then lets the sleep go on
//! whatever happened. A hook that hung would hold a laptop awake in a bag.
//!
//! Kill switch `ROOMLERD_SLEEP_ANNOUNCE=0`: no goodbye and no reconnect hold,
//! and the Linux inhibitor back to sessions only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_remote_control::models::PowerBrief;
use webrtc::data_channel::RTCDataChannel;

/// A session's control channel, as the session keeps it: empty until the
/// viewer opens it.
pub type ControlDc = Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>;

fn registry() -> &'static Mutex<HashMap<u64, (ObjectId, ControlDc)>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, (ObjectId, ControlDc)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A live session's place in the registry. Dropped with the session.
pub struct Registration(u64);

impl Drop for Registration {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

/// A remote-control session is live: its controller is told when this
/// computer goes to sleep, for as long as the registration lives.
pub fn register(session: ObjectId, control_dc: ControlDc) -> Registration {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id, (session, control_dc));
    Registration(id)
}

/// How many sessions would be told right now.
pub fn live() -> usize {
    registry().lock().unwrap_or_else(|p| p.into_inner()).len()
}

/// FR-93 P5h kill switch `ROOMLERD_SLEEP_NOTICE=0`: no inhibitor for
/// sessions and no notice to viewers, as before P5h.
fn notice_enabled() -> bool {
    tunnel_core::env::flag("SLEEP_NOTICE", true)
}

/// FR-55 P4 kill switch `ROOMLERD_SLEEP_ANNOUNCE=0`: no goodbye to the server
/// and no reconnect hold, as before P4.
fn announce_enabled() -> bool {
    tunnel_core::env::flag("SLEEP_ANNOUNCE", true)
}

/// Tell every live session's controller that this computer is going to sleep,
/// each within a bound, all at once. How many it reached.
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
async fn tell_all() -> usize {
    let Some(payload) = crate::peer::host_sleep_notice() else {
        return 0;
    };
    let channels: Vec<ControlDc> = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .map(|(_, dc)| dc.clone())
        .collect();
    let sends = channels.iter().map(|dc| {
        crate::peer::send_control_notice(
            dc,
            &payload,
            std::time::Duration::from_millis(100),
            SEND_DEADLINE,
            std::time::Duration::from_secs(1),
        )
    });
    futures::future::join_all(sends)
        .await
        .into_iter()
        .filter(|sent| *sent)
        .count()
}

/// How long one session's notice may take. Well inside logind's 5 s default
/// for a delay inhibitor, so the computer sleeps on time even when a viewer's
/// channel is not open.
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1500);

// ── FR-55 P4 — the goodbye ──────────────────────────────────────────────────

/// What a control connection is handed when the computer is going to sleep:
/// the posture to announce, and where to say it has been sent.
#[derive(Debug)]
pub struct Goodbye {
    pub power: PowerBrief,
    pub sent: tokio::sync::oneshot::Sender<()>,
}

fn controls() -> &'static Mutex<HashMap<u64, tokio::sync::mpsc::Sender<Goodbye>>> {
    static CONTROLS: OnceLock<Mutex<HashMap<u64, tokio::sync::mpsc::Sender<Goodbye>>>> =
        OnceLock::new();
    CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A live control connection's place in the goodbye registry. Dropped with
/// the connection.
pub struct ControlRegistration(u64);

impl Drop for ControlRegistration {
    fn drop(&mut self) {
        controls()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

/// FR-55 P4 — a control connection is up: it is handed a [`Goodbye`] when
/// this computer is going to sleep, for as long as the registration lives.
///
/// One that comes up inside an announced sleep (a dial that was already under
/// way when the OS spoke) is handed its goodbye at once. It is registered
/// FIRST and the flag read after, so a sleep announced in between is seen
/// either here or by [`will_sleep`]'s sweep, never by neither.
pub fn register_control() -> (ControlRegistration, tokio::sync::mpsc::Receiver<Goodbye>) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    controls()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id, tx.clone());
    if announced_at().is_some() {
        let (sent, _) = tokio::sync::oneshot::channel();
        let _ = tx.try_send(Goodbye {
            power: crate::power::brief_now(),
            sent,
        });
    }
    (ControlRegistration(id), rx)
}

/// How many control connections would be told right now.
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
fn controls_live() -> usize {
    controls().lock().unwrap_or_else(|p| p.into_inner()).len()
}

/// Hand each of `senders` its goodbye, and wait until each says it has sent
/// it. How many did. Unbounded: [`will_sleep`] bounds it.
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
async fn say_goodbye(senders: Vec<tokio::sync::mpsc::Sender<Goodbye>>, power: PowerBrief) -> usize {
    let mut sent = Vec::with_capacity(senders.len());
    for tx in senders {
        let (done, rx) = tokio::sync::oneshot::channel();
        if tx
            .try_send(Goodbye {
                power: power.clone(),
                sent: done,
            })
            .is_ok()
        {
            sent.push(rx);
        }
    }
    futures::future::join_all(sent)
        .await
        .into_iter()
        .filter(Result::is_ok)
        .count()
}

/// [`say_goodbye`] on every live control connection.
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
async fn goodbye_all(power: PowerBrief) -> usize {
    let senders: Vec<_> = controls()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect();
    say_goodbye(senders, power).await
}

/// When this computer announced its sleep: `Some` from the OS's warning to
/// the wake or the cancel. Read through [`announced_at`], which also ends it
/// at [`HOLD_MAX`].
fn asleep() -> &'static tokio::sync::watch::Sender<Option<Instant>> {
    static ASLEEP: OnceLock<tokio::sync::watch::Sender<Option<Instant>>> = OnceLock::new();
    ASLEEP.get_or_init(|| tokio::sync::watch::Sender::new(None))
}

/// The longest an announced sleep holds the reconnects: for a sleep that
/// never came, or whose wake the OS never reported. On Linux and macOS the
/// sleep itself stops this clock (their monotonic time does not run while
/// the computer sleeps), so it bounds awake time, not the nap.
///
/// ⚠️ It also ends the announcement itself, not just one wait: an
/// announcement that outlived it would hand every new connection its
/// goodbye at once, and the device would drop off again with every dial.
const HOLD_MAX: Duration = Duration::from_secs(60);

/// The moment of the sleep announced and not yet over.
fn announced_at() -> Option<Instant> {
    let at = *asleep().borrow();
    at.filter(|t| t.elapsed() < HOLD_MAX)
}

/// FR-55 P4 — before each connect attempt. While a sleep is announced, wait
/// for the wake or the cancel, and at most until [`HOLD_MAX`] after the
/// announcement. Returns at once in every other case, which is nearly always.
pub async fn hold_while_asleep() {
    hold_for(HOLD_MAX).await;
}

async fn hold_for(max: Duration) {
    // Subscribed BEFORE the read: a wake between the two is still seen by
    // `wait_for`, which checks the value it starts from.
    let mut rx = asleep().subscribe();
    let Some(at) = announced_at() else {
        return;
    };
    let left = max.saturating_sub(at.elapsed());
    tracing::info!(
        hold_s = left.as_secs(),
        "this computer is going to sleep: the reconnect waits for the wake (FR-55 P4)"
    );
    match tokio::time::timeout(left, rx.wait_for(Option::is_none)).await {
        Ok(_) => tracing::info!("awake again: reconnecting (FR-55 P4)"),
        Err(_) => tracing::warn!(
            hold_s = left.as_secs(),
            "no wake reported within the hold: reconnecting anyway (FR-55 P4)"
        ),
    }
}

/// How long each part of a goodbye may take, inside what the OS allows.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
struct Budget {
    /// For the viewers' notices (P5h).
    tell: Duration,
    /// For the server's goodbye (P4), after the viewers'.
    announce: Duration,
}

#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
impl Budget {
    /// The hook's hard bound: both parts, and a little for the hand-over.
    fn total(self) -> Duration {
        self.tell + self.announce + Duration::from_millis(100)
    }
}

/// FR-55 P4 — the OS says this computer is going to sleep. Tell every viewer
/// (P5h) first, because the goodbye closes their sessions, then say goodbye
/// on every control connection, and return within `budget` whatever
/// happened, so the hook that called this lets the sleep go on.
///
/// ⚠️ Called from a hook's own thread, never a runtime thread: the work is
/// spawned on `runtime`, and this waits on a std channel with a hard bound.
/// Never `block_on`.
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
fn will_sleep(runtime: &tokio::runtime::Handle, budget: Budget) {
    let (notice, announce) = (notice_enabled(), announce_enabled());
    if announce {
        // First: from here no connect attempt starts, and one already under
        // way is handed its goodbye as it registers.
        asleep().send_replace(Some(Instant::now()));
    }
    let (sessions, connections) = (live(), controls_live());
    let (tx, rx) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        let told = if notice {
            tokio::time::timeout(budget.tell, tell_all())
                .await
                .unwrap_or(0)
        } else {
            0
        };
        let said = if announce {
            tokio::time::timeout(budget.announce, goodbye_all(crate::power::brief_now()))
                .await
                .unwrap_or(0)
        } else {
            0
        };
        let _ = tx.send((told, said));
    });
    match rx.recv_timeout(budget.total()) {
        Ok((told, said)) => tracing::info!(
            sessions,
            told,
            connections,
            said,
            "the computer is going to sleep: viewers told (FR-93 P5h), server told (FR-55 P4)"
        ),
        Err(_) => tracing::warn!(
            sessions,
            connections,
            bound_ms = u64::try_from(budget.total().as_millis()).unwrap_or(u64::MAX),
            "the computer is going to sleep: the goodbye did not finish within its bound; \
             letting it sleep anyway (FR-55 P4)"
        ),
    }
}

/// FR-55 P4 — the computer woke, or its sleep was cancelled or failed: the
/// reconnects go ahead.
#[cfg_attr(
    not(any(
        windows,
        target_os = "macos",
        all(target_os = "linux", feature = "portal-capture")
    )),
    allow(dead_code)
)]
fn woke() {
    if asleep().send_replace(None).is_some() {
        tracing::debug!("the announced sleep is over (FR-55 P4)");
    }
}

/// FR-55 P4 — the two halves of an OS hook, for a process that has no sleep
/// to hook. The integration suite drives the real agent loop through them
/// (`crates/tests/tests/sleep_announce_agent.rs`), from a test binary of its
/// own, because the announcement is process-wide. The daemon never calls
/// them.
#[doc(hidden)]
pub mod simulate {
    use std::time::Duration;

    /// The OS says this computer is going to sleep: what a hook does, within
    /// the Linux hook's bound. Blocks the calling thread for at most that
    /// bound, so never call it on a runtime thread.
    pub fn will_sleep(runtime: &tokio::runtime::Handle) {
        super::will_sleep(
            runtime,
            super::Budget {
                tell: Duration::from_millis(1600),
                announce: Duration::from_millis(1500),
            },
        );
    }

    /// The computer woke, or its sleep was cancelled.
    pub fn woke() {
        super::woke();
    }
}

/// Whether the Linux watcher should hold its delay inhibitor at all: for
/// P5h while a session is live, and for P4's goodbye while a control
/// connection is, but only on a device whose `power_policy` is not `never`.
/// A `never` device keeps P5h's scope, so the default shows nothing new in
/// `systemd-inhibit --list` (FR-55 AC3), and its goodbye is best effort.
#[cfg_attr(
    not(all(target_os = "linux", feature = "portal-capture")),
    allow(dead_code)
)]
fn inhibitor_wanted(
    sessions: usize,
    connections: usize,
    policy_never: bool,
    notice: bool,
    announce: bool,
) -> bool {
    (notice && sessions > 0) || (announce && connections > 0 && !policy_never)
}

/// Whether the watcher should hold its delay inhibitor, and what to do now:
/// take it, let it go, or leave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(all(target_os = "linux", feature = "portal-capture")),
    allow(dead_code)
)]
enum Lock {
    Take,
    Release,
    Keep,
}

/// [`Lock`] as a pure rule: held exactly while it is wanted
/// ([`inhibitor_wanted`]), and never taken while a sleep is under way. logind
/// refuses an inhibitor for a sleep it is already carrying out
/// (`OperationInProgress`, measured on the FR-93 lab), and the next one is
/// taken after the wake.
#[cfg_attr(
    not(all(target_os = "linux", feature = "portal-capture")),
    allow(dead_code)
)]
fn lock_step(wanted: bool, held: bool, sleeping: bool) -> Lock {
    match (wanted && !sleeping, held) {
        (true, false) => Lock::Take,
        (false, true) => Lock::Release,
        _ => Lock::Keep,
    }
}

/// Start the OS hook, once per process. `runtime` sends the notices and the
/// goodbyes; the hooks themselves run on threads of their own (the Linux
/// watcher's, IOKit's run loop, Windows' callback thread), because zbus's
/// blocking API panics on a runtime thread and an OS callback must never wait
/// on one.
pub fn start(runtime: tokio::runtime::Handle) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let (notice, announce) = (notice_enabled(), announce_enabled());
    if !notice {
        tracing::info!("sleep notice off (ROOMLERD_SLEEP_NOTICE=0, FR-93 P5h)");
    }
    if !announce {
        tracing::info!("sleep announcement off (ROOMLERD_SLEEP_ANNOUNCE=0, FR-55 P4)");
    }
    if notice || announce {
        hook::start(runtime);
    }
}

#[cfg(all(target_os = "linux", feature = "portal-capture"))]
use linux as hook;
#[cfg(target_os = "macos")]
use mac as hook;
#[cfg(windows)]
use win as hook;

/// Nothing to watch on a build with no hook (a Linux agent without D-Bus).
#[cfg(not(any(
    windows,
    target_os = "macos",
    all(target_os = "linux", feature = "portal-capture")
)))]
mod hook {
    pub(super) fn start(_runtime: tokio::runtime::Handle) {}
}

#[cfg(all(target_os = "linux", feature = "portal-capture"))]
mod linux {
    use super::{
        Budget, Lock, announce_enabled, controls_live, inhibitor_wanted, live, lock_step,
        notice_enabled, will_sleep, woke,
    };
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    const LOGIND: &str = "org.freedesktop.login1";
    const LOGIND_PATH: &str = "/org/freedesktop/login1";
    const MANAGER: &str = "org.freedesktop.login1.Manager";

    /// How often the inhibitor is matched to the live sessions.
    const TICK: Duration = Duration::from_secs(2);

    /// Inside logind's default `InhibitDelayMaxSec` of 5 s. The viewers keep
    /// the bound P5h gave them (one [`super::SEND_DEADLINE`] each, all at
    /// once).
    const BUDGET: Budget = Budget {
        tell: Duration::from_millis(1600),
        announce: Duration::from_millis(1500),
    };

    pub(super) fn start(runtime: tokio::runtime::Handle) {
        let spawned = std::thread::Builder::new()
            .name("sleep-notice".into())
            .spawn(move || watch(runtime));
        if let Err(e) = spawned {
            tracing::warn!(%e, "sleep notice: no thread to watch logind (FR-93 P5h)");
        }
    }

    /// Forever: a watcher that lost logind (a restarted logind, no system bus
    /// yet at boot) tries again a minute later.
    fn watch(runtime: tokio::runtime::Handle) {
        loop {
            if let Err(e) = watch_once(&runtime) {
                tracing::warn!(%e, "sleep notice: logind unavailable; trying again in 60 s (FR-93 P5h)");
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    fn watch_once(runtime: &tokio::runtime::Handle) -> anyhow::Result<()> {
        let conn = zbus::blocking::Connection::system()?;
        let manager = zbus::blocking::Proxy::new(&conn, LOGIND, LOGIND_PATH, MANAGER)?;
        // zbus's blocking signal iterator has no timed `next`, so the signals
        // are read on their own thread and handed over a channel the tick can
        // wait on with a bound.
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let signals = manager.clone();
        std::thread::Builder::new()
            .name("sleep-notice-signals".into())
            .spawn(move || {
                let Ok(mut stream) = signals.receive_signal("PrepareForSleep") else {
                    return;
                };
                for msg in &mut stream {
                    let Ok(start) = msg.body().deserialize::<bool>() else {
                        continue;
                    };
                    if tx.send(start).is_err() {
                        return;
                    }
                }
            })?;
        tracing::info!("sleep notice: watching logind for sleep (FR-93 P5h, FR-55 P4)");
        let (notice, announce) = (notice_enabled(), announce_enabled());
        let mut lock: Option<zbus::zvariant::OwnedFd> = None;
        // From `PrepareForSleep(true)` to `(false)`: the sleep is under way.
        let mut sleeping = false;
        loop {
            // A wake whose signal never arrived still ends the sleep: logind's
            // own property says whether one is under way.
            if sleeping
                && manager
                    .get_property::<bool>("PreparingForSleep")
                    .is_ok_and(|p| !p)
            {
                sleeping = false;
                woke();
            }
            // FR-55 P4 — the keeper's policy, as it last published it. No
            // posture yet reads as the default, `never`.
            let policy_never = crate::power::posture()
                .is_none_or(|p| p.policy == crate::power::PowerPolicy::Never);
            let wanted = inhibitor_wanted(live(), controls_live(), policy_never, notice, announce);
            match lock_step(wanted, lock.is_some(), sleeping) {
                Lock::Take => match take(&manager) {
                    Ok(fd) => lock = Some(fd),
                    Err(e) => {
                        tracing::warn!(%e, "sleep notice: logind refused a delay inhibitor (FR-93 P5h)")
                    }
                },
                Lock::Release => lock = None,
                Lock::Keep => {}
            }
            match rx.recv_timeout(TICK) {
                Ok(true) => {
                    will_sleep(runtime, BUDGET);
                    // Let it sleep. The next tick after the wake takes the
                    // inhibitor again if it is still wanted.
                    lock = None;
                    sleeping = true;
                }
                Ok(false) => {
                    sleeping = false;
                    woke();
                    tracing::info!("the computer woke up (FR-93 P5h)");
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("logind's PrepareForSleep stream ended")
                }
            }
        }
    }

    /// A logind delay inhibitor on sleep, held for as long as its fd is.
    fn take(manager: &zbus::blocking::Proxy<'_>) -> zbus::Result<zbus::zvariant::OwnedFd> {
        manager
            .call_method(
                "Inhibit",
                &(
                    "sleep",
                    "Roomler",
                    "tell remote viewers that this computer is going to sleep",
                    "delay",
                ),
            )?
            .body()
            .deserialize::<zbus::zvariant::OwnedFd>()
    }
}

/// Windows: `PowerRegisterSuspendResumeNotification` with a callback, which
/// works for a service and a user process alike (no window, no message
/// loop). The registration lives as long as the process.
#[cfg(windows)]
mod win {
    use super::{Budget, will_sleep, woke};
    use std::ffi::c_void;
    use std::time::Duration;
    use windows_sys::Win32::System::Power::{
        DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
    };

    /// Windows waits about 2 s for a suspend callback; this stays inside it.
    const BUDGET: Budget = Budget {
        tell: Duration::from_millis(600),
        announce: Duration::from_millis(800),
    };

    pub(super) fn start(runtime: tokio::runtime::Handle) {
        // Both live as long as the registration, which is the process's life:
        // leaked on purpose.
        let context: &'static tokio::runtime::Handle = Box::leak(Box::new(runtime));
        let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
            Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(on_power),
                Context: std::ptr::from_ref(context).cast_mut().cast::<c_void>(),
            }));
        let mut registration: *mut c_void = std::ptr::null_mut();
        // SAFETY: `params` points at a valid DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS
        // that outlives the registration, and `registration` is a valid out
        // pointer for the call.
        let rc = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                std::ptr::from_mut(params).cast::<c_void>(),
                &mut registration,
            )
        };
        if rc == 0 {
            tracing::info!("sleep notice: watching Windows for suspend (FR-55 P4)");
        } else {
            tracing::warn!(
                rc,
                "sleep notice: Windows refused the suspend notification; this device \
                 will not announce its sleeps (FR-55 P4)"
            );
        }
    }

    /// Windows calls this on a thread of its own and waits for it to return
    /// before it suspends, so it returns within [`BUDGET`] whatever happens.
    /// ⚠️ It never unwinds into Windows: a panic across `extern "system"`
    /// aborts the daemon.
    unsafe extern "system" fn on_power(
        context: *const c_void,
        kind: u32,
        _setting: *const c_void,
    ) -> u32 {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match kind {
            PBT_APMSUSPEND => {
                // SAFETY: `context` is the runtime handle `start` leaked for
                // this registration; it is never freed.
                let runtime = unsafe { &*context.cast::<tokio::runtime::Handle>() };
                will_sleep(runtime, BUDGET);
            }
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => woke(),
            _ => {}
        }));
        0 // ERROR_SUCCESS
    }
}

/// macOS: `IORegisterForSystemPower` on a thread with its own run loop. The
/// sleep messages IOKit asks about are always allowed (`IOAllowPowerChange`),
/// and only after the goodbye or its bound.
#[cfg(target_os = "macos")]
mod mac {
    use super::{Budget, will_sleep, woke};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    type IoNotificationPortRef = *mut c_void;
    type CfRunLoopRef = *mut c_void;
    type CfRunLoopSourceRef = *mut c_void;
    type CfStringRef = *const c_void;
    /// `IOServiceInterestCallback`.
    type InterestCallback = extern "C" fn(*mut c_void, u32, u32, *mut c_void);

    /// `kIOMessageCanSystemSleep`: may the system sleep (an idle sleep)?
    const CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
    /// `kIOMessageSystemWillSleep`: it will, now.
    const SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
    /// `kIOMessageSystemWillNotSleep`: someone vetoed an idle sleep.
    const SYSTEM_WILL_NOT_SLEEP: u32 = 0xE000_0290;
    /// `kIOMessageSystemHasPoweredOn`: the wake is complete.
    const SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IORegisterForSystemPower(
            refcon: *mut c_void,
            the_port_ref: *mut IoNotificationPortRef,
            callback: InterestCallback,
            notifier: *mut u32,
        ) -> u32;
        fn IOAllowPowerChange(kernel_port: u32, notification_id: isize) -> i32;
        fn IONotificationPortGetRunLoopSource(notify: IoNotificationPortRef) -> CfRunLoopSourceRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRunLoopGetCurrent() -> CfRunLoopRef;
        fn CFRunLoopAddSource(rl: CfRunLoopRef, source: CfRunLoopSourceRef, mode: CfStringRef);
        fn CFRunLoopRun();
        static kCFRunLoopDefaultMode: CfStringRef;
    }

    /// The root power domain's connection, which `IOAllowPowerChange`
    /// answers on. Set once, before the run loop delivers anything.
    static ROOT_PORT: AtomicU32 = AtomicU32::new(0);

    /// macOS waits up to 30 s for the answer; a lid closing should not.
    const BUDGET: Budget = Budget {
        tell: Duration::from_millis(600),
        announce: Duration::from_millis(800),
    };

    pub(super) fn start(runtime: tokio::runtime::Handle) {
        let spawned = std::thread::Builder::new()
            .name("sleep-notice".into())
            .spawn(move || watch(runtime));
        if let Err(e) = spawned {
            tracing::warn!(%e, "sleep notice: no thread to watch for sleep (FR-55 P4)");
        }
    }

    fn watch(runtime: tokio::runtime::Handle) {
        // Lives as long as the registration, which is the process's life.
        let refcon: *mut c_void = Box::into_raw(Box::new(runtime)).cast();
        let mut port: IoNotificationPortRef = std::ptr::null_mut();
        let mut notifier: u32 = 0;
        // SAFETY: valid out pointers for the call; `refcon` is never freed.
        let root = unsafe { IORegisterForSystemPower(refcon, &mut port, on_power, &mut notifier) };
        if root == 0 || port.is_null() {
            tracing::warn!(
                "sleep notice: IORegisterForSystemPower failed; this Mac will not \
                 announce its sleeps (FR-55 P4)"
            );
            return;
        }
        ROOT_PORT.store(root, Ordering::Release);
        // SAFETY: `port` is the notification port IOKit just made, and the run
        // loop is this thread's own.
        unsafe {
            CFRunLoopAddSource(
                CFRunLoopGetCurrent(),
                IONotificationPortGetRunLoopSource(port),
                kCFRunLoopDefaultMode,
            );
        }
        tracing::info!("sleep notice: watching macOS for sleep (FR-55 P4)");
        // SAFETY: runs this thread's run loop, which returns only when every
        // source is gone; nothing removes ours.
        unsafe { CFRunLoopRun() };
        tracing::warn!("sleep notice: the macOS power run loop ended (FR-55 P4)");
    }

    /// IOKit's callback, on the run loop's thread.
    ///
    /// ⚠️ Every sleep IOKit asks about is allowed, and a `WillSleep` only
    /// after at most [`BUDGET`]: an unanswered one delays the sleep by 30 s,
    /// with the lid closed. And it never unwinds into IOKit: a panic across
    /// `extern "C"` aborts the daemon.
    extern "C" fn on_power(
        refcon: *mut c_void,
        _service: u32,
        message: u32,
        argument: *mut c_void,
    ) {
        match message {
            CAN_SYSTEM_SLEEP => allow(argument),
            SYSTEM_WILL_SLEEP => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // SAFETY: `refcon` is the runtime handle `watch` leaked for
                    // this registration; it is never freed.
                    let runtime = unsafe { &*refcon.cast::<tokio::runtime::Handle>() };
                    will_sleep(runtime, BUDGET);
                }));
                allow(argument);
            }
            SYSTEM_WILL_NOT_SLEEP | SYSTEM_HAS_POWERED_ON => {
                let _ = std::panic::catch_unwind(woke);
            }
            _ => {}
        }
    }

    fn allow(argument: *mut c_void) {
        // SAFETY: `argument` is the notification id IOKit sent with the
        // message (an `intptr_t`), answered on the port it came from.
        unsafe {
            IOAllowPowerChange(ROOT_PORT.load(Ordering::Acquire), argument.addr() as isize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test at a time touches the process-wide sleep announcement. An
    /// async lock, because it is held across the tests' awaits.
    static ANNOUNCEMENT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// FR-93 P5h — the inhibitor is held exactly while it is wanted: taken
    /// with the first reason, let go with the last, and nothing in between.
    /// Never taken while a sleep is under way: logind refuses it then.
    #[test]
    fn the_inhibitor_is_held_exactly_while_it_is_wanted() {
        assert_eq!(lock_step(true, false, false), Lock::Take);
        assert_eq!(lock_step(true, true, false), Lock::Keep);
        assert_eq!(lock_step(false, true, false), Lock::Release);
        assert_eq!(lock_step(false, false, false), Lock::Keep);
        assert_eq!(lock_step(true, false, true), Lock::Keep);
        assert_eq!(lock_step(true, true, true), Lock::Release);
    }

    /// FR-55 P4 — a live session wants the inhibitor whatever the policy
    /// (P5h, unchanged). A live control connection wants it only off `never`,
    /// so the default shows nothing new in `systemd-inhibit --list` (AC3).
    /// Each kill switch takes its own reason away, and only its own.
    #[test]
    fn the_inhibitor_covers_sessions_always_and_connections_only_off_never() {
        // (sessions, connections, policy_never, notice, announce)
        assert!(
            inhibitor_wanted(1, 0, true, true, true),
            "a session, on never"
        );
        assert!(inhibitor_wanted(1, 3, true, true, true));
        assert!(
            !inhibitor_wanted(0, 1, true, true, true),
            "never: connections add nothing"
        );
        assert!(
            inhibitor_wanted(0, 1, false, true, true),
            "on-ac / always: a connection"
        );
        assert!(!inhibitor_wanted(0, 0, false, true, true), "nothing live");
        assert!(
            !inhibitor_wanted(0, 1, false, true, false),
            "ROOMLERD_SLEEP_ANNOUNCE=0"
        );
        assert!(
            inhibitor_wanted(1, 0, true, true, false),
            "the notice keeps its own"
        );
        assert!(
            !inhibitor_wanted(1, 0, true, false, true),
            "ROOMLERD_SLEEP_NOTICE=0"
        );
        assert!(
            inhibitor_wanted(0, 1, false, false, true),
            "the goodbye keeps its own"
        );
    }

    /// P5h — a session is told for exactly as long as it is registered. By
    /// its own entries: the registry is process-wide, so another test's
    /// sessions may be in it too.
    #[test]
    fn a_session_is_registered_until_it_ends() {
        let listed = |id: u64| registry().lock().unwrap().contains_key(&id);
        let dc: ControlDc = Arc::new(tokio::sync::Mutex::new(None));
        let a = register(ObjectId::new(), dc.clone());
        let b = register(ObjectId::new(), dc);
        let (ida, idb) = (a.0, b.0);
        assert!(listed(ida) && listed(idb));
        assert!(live() >= 2);
        drop(a);
        assert!(!listed(ida) && listed(idb));
        drop(b);
        assert!(!listed(idb));
    }

    /// P5h — the notice is a `host_sleep` wait, with its hint, and it lasts as
    /// long as any host wait (no sleep has a known end).
    #[test]
    fn the_notice_names_the_sleep() {
        let notice = crate::peer::host_sleep_notice().expect("on by default");
        let v: serde_json::Value = serde_json::from_str(&notice).expect("JSON");
        assert_eq!(v["t"], "rc:media-waiting");
        assert_eq!(v["for"], "host_sleep");
        assert_eq!(v["waits_ms"], 3_600_000);
        assert!(
            v["hint"].as_str().unwrap().contains("going to sleep"),
            "{notice}"
        );
    }

    fn brief() -> PowerBrief {
        PowerBrief {
            policy: "on-ac".into(),
            on_ac: Some(false),
            held: false,
        }
    }

    /// FR-55 P4 — every connection is handed the posture, and the sweep
    /// counts only the ones that say they sent it: one whose socket died
    /// drops its `sent` unanswered and is not counted.
    #[tokio::test]
    async fn every_connection_is_handed_its_goodbye_and_counted_once_sent() {
        let (a, mut ra) = tokio::sync::mpsc::channel(1);
        let (b, mut rb) = tokio::sync::mpsc::channel(1);
        let sweep = tokio::spawn(say_goodbye(vec![a, b], brief()));
        let ga = ra.recv().await.expect("a is told");
        let gb = rb.recv().await.expect("b is told");
        assert_eq!(ga.power, brief());
        ga.sent.send(()).unwrap();
        drop(gb); // b's socket died before it could send
        assert_eq!(sweep.await.unwrap(), 1);
    }

    /// FR-55 P4 — a connection is registered for exactly as long as it
    /// lives, and is handed a goodbye at once only when it comes up inside an
    /// announced sleep: a dial already under way when the OS spoke.
    #[tokio::test]
    async fn a_connection_that_comes_up_inside_the_sleep_is_told_at_once() {
        let _one = ANNOUNCEMENT.lock().await;
        woke();
        let listed = |id: u64| controls().lock().unwrap().contains_key(&id);
        let (awake, mut rx) = register_control();
        assert!(listed(awake.0));
        assert!(rx.try_recv().is_err(), "awake: nothing to say");
        let id = awake.0;
        drop(awake);
        assert!(!listed(id), "gone with its connection");

        asleep().send_replace(Some(Instant::now()));
        let (_late, mut rx) = register_control();
        let g = rx.try_recv().expect("inside the window: told at once");
        assert_eq!(g.power.policy, crate::power::brief_now().policy);

        woke();
        let (_after, mut rx) = register_control();
        assert!(rx.try_recv().is_err(), "after the wake: nothing to say");
    }

    /// FR-55 P4 — the reconnect waits for the wake, and never past its
    /// bound. ⚠️ An announcement older than the bound is over: were it not,
    /// every new connection would be told at once, and the device would drop
    /// off again with every dial.
    #[tokio::test]
    async fn the_reconnect_waits_for_the_wake_and_never_past_its_bound() {
        let _one = ANNOUNCEMENT.lock().await;
        woke();
        // Awake: no wait at all.
        tokio::time::timeout(Duration::from_millis(50), hold_for(Duration::from_secs(5)))
            .await
            .expect("awake: no hold");

        // Announced: held until the wake.
        asleep().send_replace(Some(Instant::now()));
        let held = tokio::spawn(hold_for(Duration::from_secs(30)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!held.is_finished(), "held while asleep");
        woke();
        tokio::time::timeout(Duration::from_secs(2), held)
            .await
            .expect("the wake ends the hold")
            .unwrap();

        // Announced and never woken: held only up to the bound.
        asleep().send_replace(Some(Instant::now()));
        tokio::time::timeout(Duration::from_secs(2), hold_for(Duration::from_millis(100)))
            .await
            .expect("the bound ends the hold");

        // An announcement past HOLD_MAX is over.
        asleep().send_replace(Instant::now().checked_sub(HOLD_MAX + Duration::from_secs(1)));
        assert!(announced_at().is_none());
        let (_conn, mut rx) = register_control();
        assert!(rx.try_recv().is_err(), "a stale announcement tells nobody");
        woke();
    }
}
