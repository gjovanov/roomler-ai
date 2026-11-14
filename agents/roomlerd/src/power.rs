// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-55 — keep the device reachable instead of letting it quietly sleep.
//!
//! A device that sleeps is a device that is offline, and until this module there
//! was nothing in the product with any opinion about power at all:
//! `IOPMAssertion`, `SetThreadExecutionState`, `PowerCreateRequest` and
//! `systemd-inhibit` had zero occurrences across the tree.
//!
//! Measured on the operator's MacBook (2026-09-01): the idle timer is **one
//! minute** (`pmset -g`: `sleep 1`), and it really sleeps on AC —
//! `Entering Sleep state due to 'Idle Sleep'` and `'Clamshell Sleep'`.
//!
//! ## Two rules, and the first is not a policy
//!
//! 1. **An active session always holds the machine awake.** A remote-desktop or
//!    SSH session must not be cut by an idle timer, and that is true whatever
//!    the standing policy says. ⚠️ Today this happens on macOS only BY
//!    ACCIDENT: injecting input tickles `IOHIDSystem`, which registers as user
//!    activity. So a session where someone is *typing* is protected and a
//!    **view-only** one is not — and watching a long build is exactly the case
//!    that most wants to stay up.
//! 2. **Everything else is opt-in, default off.** A remote-access tool that
//!    silently drains a laptop battery earns the reputation it gets, so
//!    [`PowerPolicy`] is device-owned with the same last-word rule as
//!    `exec_enabled` and `ssh_enabled` (`docs/remote-config.md`).
//!
//! ## What this cannot do
//!
//! ⚠️ **macOS clamshell sleep ignores idle-sleep assertions** on a laptop with
//! no external display. Closing the lid puts the machine to sleep no matter
//! what we hold. That is an OS limit, not a bug here, and the UI that offers
//! the policy has to say so — otherwise the feature looks broken to exactly the
//! person who enabled it.
//!
//! ⚠️ Nothing here wakes a machine that is already asleep. That needs a magic
//! packet from a peer on the same L2 (FR-55 P5); `womp` is already enabled on
//! the MacBook and the overlay already knows which peers share a LAN.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How often the keeper re-reads the world. A poll rather than an event
/// subscription: AC transitions and session start/stop are both cheap to
/// observe and neither is latency-critical — being five seconds late to release
/// an assertion costs nothing.
pub const POLL: Duration = Duration::from_secs(5);

/// The device-owned standing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerPolicy {
    /// Never ask the OS to stay awake for the mesh. The default, and
    /// byte-for-byte the behaviour before FR-55.
    #[default]
    Never,
    /// Stay awake only while on mains power. The setting most laptops want:
    /// reachable at a desk, and not a dead battery in a bag.
    OnAc,
    /// Always stay awake.
    Always,
}

impl PowerPolicy {
    /// Parse the config value.
    ///
    /// ⚠️ An unrecognised value is [`PowerPolicy::Never`], never `Always`: the
    /// cost of a wrong "off" is a device that sleeps, and the cost of a wrong
    /// "on" is someone's battery. Same direction as every other gate here.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "always" => PowerPolicy::Always,
            "on-ac" | "on_ac" | "ac" => PowerPolicy::OnAc,
            _ => PowerPolicy::Never,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PowerPolicy::Never => "never",
            PowerPolicy::OnAc => "on-ac",
            PowerPolicy::Always => "always",
        }
    }
}

/// What the keeper knows when it decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    pub policy: PowerPolicy,
    /// `None` when this platform could not tell. Deliberately three-valued:
    /// "on battery" and "we do not know" must not be collapsed, or `on-ac`
    /// silently becomes `always` on a host whose power source we cannot read.
    pub on_ac: Option<bool>,
    /// Any live remote-control or SSH session.
    pub session_active: bool,
}

/// The whole policy, in one testable place.
///
/// Pure and NOT behind a `cfg`, because the platform backends below compile
/// only on their own OS — and logic that only compiles on one platform is logic
/// the other lanes never verify. Same reasoning as
/// [`crate::macos_supervisor::decide`].
pub fn should_stay_awake(i: Inputs) -> bool {
    // Rule 1: a session outranks the policy. Cutting a live session with an
    // idle timer is never what anyone wanted, and this is the case that needs
    // no configuration to be correct.
    if i.session_active {
        return true;
    }
    match i.policy {
        PowerPolicy::Never => false,
        PowerPolicy::Always => true,
        // Unknown power source behaves as "on battery": see `Inputs::on_ac`.
        PowerPolicy::OnAc => i.on_ac.unwrap_or(false),
    }
}

/// Live work that must not be interrupted by an idle timer.
///
/// A COUNT, not a flag: two concurrent SSH sessions must not have the first one
/// to end clear the second one's protection. Shared as an `Arc` because the
/// things that hold it — an SSH handler, an `exec` run — are created all over
/// the daemon and have no other reason to know about each other.
pub type ActivityCounter = Arc<AtomicUsize>;

pub fn new_activity_counter() -> ActivityCounter {
    Arc::new(AtomicUsize::new(0))
}

/// The device-wide counter.
///
/// ⚠️ A process global, unlike the consent broker which this codebase
/// deliberately un-globalised. The difference is what absence MEANS: a missing
/// broker would have to fail one way or the other on a security question, so
/// "no broker" had to be unrepresentable. A missing counter can only mean "the
/// machine may sleep", which is the safe direction — and a machine has exactly
/// ONE power state, so a per-connection instance would be wrong rather than
/// merely inconvenient.
pub fn shared_activity() -> &'static ActivityCounter {
    static ACTIVITY: std::sync::OnceLock<ActivityCounter> = std::sync::OnceLock::new();
    ACTIVITY.get_or_init(new_activity_counter)
}

/// Holds the machine awake for as long as it lives.
///
/// RAII rather than paired calls, for the reason `ssh::Handler`'s own `Drop`
/// gives: a session ends in many ways — clean exit, the client vanishing, a
/// deadline firing, a consent refusal — and only one of them is a path someone
/// would remember to annotate. Dropping the guard is the event they all share,
/// and a missed decrement would pin the machine awake until the daemon
/// restarted.
pub struct ActivityGuard(ActivityCounter);

impl ActivityGuard {
    pub fn new(counter: &ActivityCounter) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter.clone())
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// FR-55 P4 — what the keeper holds, as the rest of the daemon reads it: the
/// heartbeat's power brief, the suspend announcement, `roomler status`.
///
/// One value behind one lock, so a reader never sees a policy from one
/// reconcile beside a hold from another. Never held across an `.await`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posture {
    pub policy: PowerPolicy,
    /// As [`Inputs::on_ac`]: `None` = this platform could not tell.
    pub on_ac: Option<bool>,
    /// A stay-awake request is held right now, for the policy or a session.
    pub held: bool,
}

fn posture_slot() -> &'static std::sync::Mutex<Option<Posture>> {
    static POSTURE: std::sync::OnceLock<std::sync::Mutex<Option<Posture>>> =
        std::sync::OnceLock::new();
    POSTURE.get_or_init(|| std::sync::Mutex::new(None))
}

fn publish(p: Posture) {
    *posture_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
}

/// The keeper's last posture. `None` before the daemon published one, which
/// it does before it spawns the keeper ([`publish_policy`]).
pub fn posture() -> Option<Posture> {
    *posture_slot().lock().unwrap_or_else(|e| e.into_inner())
}

/// Publish the standing policy before the keeper's first reconcile, so a
/// connection that says hello in the first five seconds already reports it.
/// Nothing is held yet.
pub fn publish_policy(policy: PowerPolicy) {
    publish(Posture {
        policy,
        on_ac: imp::on_ac(),
        held: false,
    });
}

/// FR-55 P4 — the heartbeat's power brief: the keeper's last posture.
pub fn brief() -> Option<roomler_ai_remote_control::models::PowerBrief> {
    posture().map(brief_of)
}

/// The brief for the moment of a sleep: the keeper's posture with the power
/// source read again, since the keeper's own read can be five seconds old
/// and a charger that just went out is exactly what the reader wants to see.
/// A process without a keeper (none in the shipped daemon, which publishes
/// before anything connects) reads as the default: `never`, nothing held.
pub fn brief_now() -> roomler_ai_remote_control::models::PowerBrief {
    let p = posture().unwrap_or(Posture {
        policy: PowerPolicy::Never,
        on_ac: None,
        held: false,
    });
    brief_of(Posture {
        on_ac: imp::on_ac(),
        ..p
    })
}

fn brief_of(p: Posture) -> roomler_ai_remote_control::models::PowerBrief {
    roomler_ai_remote_control::models::PowerBrief {
        policy: p.policy.as_str().to_string(),
        on_ac: p.on_ac,
        held: p.held,
    }
}

/// `roomler status`'s power line (the LocalAPI's leaf mirror of the brief).
pub fn local_status() -> Option<tunnel_core::localapi::PowerStatus> {
    posture().map(|p| tunnel_core::localapi::PowerStatus {
        policy: p.policy.as_str().to_string(),
        on_ac: p.on_ac,
        held: p.held,
    })
}

/// Why the OS is being asked to stay awake — shown in `pmset -g assertions` and
/// `powercfg /requests`, so an operator inspecting the machine finds a sentence
/// rather than a process name.
const REASON: &str = "roomler: keeping this device reachable";

/// #3871 — what the keeper asks the OS through: the platform's in the daemon
/// ([`Backend::platform`]), a stand-in in a test.
struct Backend<H> {
    /// Take a stay-awake request, for as long as the OS takes to answer it.
    acquire: Arc<dyn Fn() -> std::io::Result<H> + Send + Sync>,
    /// Let one go.
    release: Arc<dyn Fn(H) + Send + Sync>,
    /// Whether the device runs on mains, `None` when this platform cannot tell.
    on_ac: Arc<dyn Fn() -> Option<bool> + Send + Sync>,
    /// Where each pass's posture goes ([`posture`]).
    publish: Arc<dyn Fn(Posture) + Send + Sync>,
}

impl Backend<imp::Handle> {
    fn platform() -> Self {
        Self {
            acquire: Arc::new(|| imp::acquire(REASON)),
            release: Arc::new(imp::release),
            on_ac: Arc::new(imp::on_ac),
            publish: Arc::new(publish),
        }
    }
}

/// Holds (and releases) the platform's stay-awake request.
///
/// Reconciling rather than toggling: the keeper computes what SHOULD be held
/// and makes reality match, so a missed transition self-heals on the next poll
/// instead of leaving an assertion pinned forever.
struct PowerKeeper<H: Send + 'static> {
    policy: PowerPolicy,
    held: Option<H>,
    /// Logged once, not per poll: a platform that cannot do this would
    /// otherwise print a line every five seconds for the daemon's whole life.
    warned_unsupported: bool,
    /// The OS, as this keeper asks it.
    backend: Backend<H>,
}

impl PowerKeeper<imp::Handle> {
    fn new(policy: PowerPolicy) -> Self {
        Self::with_backend(policy, Backend::platform())
    }
}

impl<H: Send + 'static> PowerKeeper<H> {
    fn with_backend(policy: PowerPolicy, backend: Backend<H>) -> Self {
        Self {
            policy,
            held: None,
            warned_unsupported: false,
            backend,
        }
    }

    /// Make the OS state match the decision. Idempotent.
    fn reconcile(&mut self, session_active: bool) {
        let on_ac = (self.backend.on_ac)();
        let want = should_stay_awake(Inputs {
            policy: self.policy,
            on_ac,
            session_active,
        });
        match (want, self.held.is_some()) {
            (true, false) => match (self.backend.acquire)() {
                Ok(h) => {
                    tracing::info!(
                        policy = self.policy.as_str(),
                        on_ac = ?on_ac,
                        session_active,
                        "power: holding the device awake"
                    );
                    self.held = Some(h);
                }
                Err(e) => {
                    if !self.warned_unsupported {
                        self.warned_unsupported = true;
                        tracing::warn!(
                            error = %e,
                            "power: cannot ask this OS to stay awake — the device may sleep and go offline"
                        );
                    }
                }
            },
            (false, true) => {
                if let Some(h) = self.held.take() {
                    (self.backend.release)(h);
                    tracing::info!(
                        policy = self.policy.as_str(),
                        "power: released the device to sleep normally"
                    );
                }
            }
            _ => {}
        }
        // FR-55 P4 — what is actually held after this pass, not what was
        // wanted: an acquire the OS refused holds nothing, and the reader of
        // a sleep must be able to tell the two apart.
        (self.backend.publish)(Posture {
            policy: self.policy,
            on_ac,
            held: self.held.is_some(),
        });
    }
}

impl<H: Send + 'static> Drop for PowerKeeper<H> {
    /// Release on the way out. An assertion that outlives the daemon would be a
    /// machine that never sleeps again until reboot — the exact
    /// "worse than the bug it fixes" shape FR-43's orphan taught.
    fn drop(&mut self) {
        if let Some(h) = self.held.take() {
            (self.backend.release)(h);
        }
    }
}

/// What the display assertions are called in `pmset -g assertions`.
#[cfg(target_os = "macos")]
const DISPLAY_REASON: &str = "roomler: a remote viewer is watching this screen";

/// macOS — the screen a remote viewer is watching has to be ON.
///
/// A sleeping display composites nothing, so a `CGDisplayStream` opened on it
/// delivers no frame at all: the session connects, the capture opens, no
/// encoder is ever built, and the viewer gives up and reconnects — in a loop.
/// Field-hit 2026-10-15 on a MacBook whose display had idled off
/// (`system_profiler SPDisplaysDataType`: `Display Asleep: Yes`) while the
/// machine itself stayed up.
///
/// The keeper above cannot fix that and should not: it holds
/// `PreventUserIdleSystemSleep`, which keeps the MACHINE reachable and lets the
/// display go dark on purpose. An SSH session or an `exec` has no business
/// lighting the screen. Only capture needs the pixels, so only capture takes
/// this — held by the capture worker, for exactly as long as it captures.
///
/// Two assertions, for two jobs:
/// * `IOPMAssertionDeclareUserActivity` (`caffeinate -u`) — the user is
///   active, so power the display on NOW if it is off.
/// * `PreventUserIdleDisplaySleep` (`caffeinate -d`) — keep it on while the
///   capture lives. A view-only session injects no input to count as activity,
///   so without this it would go dark again at the display-sleep timeout.
///
/// Both are released on drop, and powerd drops both itself when the process
/// dies, so neither can outlive the capture that took it.
///
/// ⚠️ Waking the display unlocks nothing: a locked Mac wakes to its lock
/// screen, and that is what the viewer sees.
/// ⚠️ A closed lid with no external display cannot be woken by any assertion
/// (the module note above).
#[cfg(target_os = "macos")]
pub struct DisplayAwake {
    activity: Option<imp::Handle>,
    hold: Option<imp::Handle>,
    was_asleep: bool,
}

#[cfg(target_os = "macos")]
impl DisplayAwake {
    pub fn acquire() -> Self {
        let was_asleep = imp::display_asleep();
        let activity = imp::declare_user_activity(DISPLAY_REASON)
            .inspect_err(
                |e| tracing::warn!(error = %e, "power: could not wake the display for a viewer"),
            )
            .ok();
        let hold = imp::acquire_typed(imp::DISPLAY_ASSERTION_TYPE, DISPLAY_REASON)
            .inspect_err(
                |e| tracing::warn!(error = %e, "power: could not keep the display on for a viewer"),
            )
            .ok();
        tracing::info!(
            was_asleep,
            woke = activity.is_some(),
            held = hold.is_some(),
            "power: a viewer is watching — the display stays on while they do"
        );
        Self {
            activity,
            hold,
            was_asleep,
        }
    }

    /// Whether the main display was asleep when the viewer arrived.
    pub fn was_asleep(&self) -> bool {
        self.was_asleep
    }
}

#[cfg(target_os = "macos")]
impl Drop for DisplayAwake {
    fn drop(&mut self) {
        for h in [self.hold.take(), self.activity.take()]
            .into_iter()
            .flatten()
        {
            imp::release(h);
        }
    }
}

/// Run the keeper for the process lifetime.
///
/// `sessions` reports whether any rc session is live; it is polled rather than
/// subscribed to, so a session that ends in any of the several ways a session
/// can end still releases the assertion.
pub async fn run(
    policy: PowerPolicy,
    sessions: Option<crate::rc_sessions::RcSessionRegistry>,
    activity: ActivityCounter,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // NOTE: no early return for `Never`. That policy means "do not keep this
    // machine awake FOR THE MESH" — a live session must still hold it, and
    // returning here would have made `never` silently mean "let sessions die
    // too". The keeper is cheap: one `load` and one platform call every 5 s.
    tracing::info!(policy = policy.as_str(), "power: keeper started (FR-55)");
    keep(PowerKeeper::new(policy), sessions, activity, POLL, shutdown).await
}

/// [`run`]'s loop for `keeper`: a pass every `every`, until `shutdown`.
async fn keep<H: Send + 'static>(
    mut keeper: PowerKeeper<H>,
    sessions: Option<crate::rc_sessions::RcSessionRegistry>,
    activity: ActivityCounter,
    every: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        // Two independent sources, because they cover different work: the
        // registry knows about rc sessions, and the counter about SSH sessions
        // and `exec` runs, which have no row there.
        let session_active = sessions
            .as_ref()
            .map(|s| !s.list().is_empty())
            .unwrap_or(false)
            || activity.load(Ordering::Relaxed) > 0;
        keeper.reconcile(session_active);
        tokio::select! {
            _ = tokio::time::sleep(every) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // `keeper` drops here, releasing the assertion.
                    tracing::info!("power: keeper stopping");
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// macOS — IOKit power assertions
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    pub struct Handle(u32);

    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    /// `kIOPMAssertionLevelOn`.
    const ASSERTION_LEVEL_ON: u32 = 255;

    /// `kIOPMAssertionTypePreventUserIdleSystemSleep` — what `caffeinate -i`
    /// takes, and the predictable choice.
    ///
    /// ⚠️ The alternative is `NetworkClientActive`, which is semantically
    /// exact for "this machine serves the network" and may interact better
    /// with Power Nap. FR-55 lists picking between them as an open decision to
    /// MEASURE; this one is chosen because its behaviour is well understood,
    /// not because it is known to be better.
    const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";

    /// `kIOPMAssertionTypePreventUserIdleDisplaySleep` — what `caffeinate -d`
    /// takes. It implies the system stays up as well.
    pub const DISPLAY_ASSERTION_TYPE: &str = "PreventUserIdleDisplaySleep";
    /// `kIOPMUserActiveLocal`: the user is at the machine, so the display
    /// powers on. Not `kIOPMUserActiveRemote`, which by its header keeps
    /// components at the power level of a user who is NOT in front of the
    /// screen — and a remote viewer needs exactly the screen.
    const USER_ACTIVE_LOCAL: u32 = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external_representation: u8,
        ) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: u32,
            assertion_name: CFStringRef,
            assertion_id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: u32) -> i32;
        /// What `caffeinate -u` calls: declares the user active, which powers
        /// the display on if it is off. Any process may call it.
        fn IOPMAssertionDeclareUserActivity(
            assertion_name: CFStringRef,
            user_type: u32,
            assertion_id: *mut u32,
        ) -> i32;
        /// Returns `kIOPSTimeRemainingUnlimited` (-2.0) on mains power,
        /// `kIOPSTimeRemainingUnknown` (-1.0) while it is still working it out,
        /// and a positive number of seconds on battery. One call, a plain
        /// `f64` return, and no CoreFoundation objects to own — which is why
        /// it is used here rather than `IOPSCopyPowerSourcesInfo`.
        fn IOPSGetTimeRemainingEstimate() -> f64;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
        /// `boolean_t` — an `int`.
        fn CGDisplayIsAsleep(display: u32) -> i32;
    }

    /// Wrap a Rust `&str` as a CFString. Caller owns the result.
    ///
    /// SAFETY: the bytes outlive the call, and CoreFoundation copies them.
    fn cfstr(s: &str) -> CFStringRef {
        unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                s.as_ptr(),
                s.len() as isize,
                CF_STRING_ENCODING_UTF8,
                0,
            )
        }
    }

    pub fn acquire(reason: &str) -> std::io::Result<Handle> {
        acquire_typed(
            super::ASSERTION_TYPE_OVERRIDE.unwrap_or(ASSERTION_TYPE),
            reason,
        )
    }

    pub fn acquire_typed(ty: &str, reason: &str) -> std::io::Result<Handle> {
        let ty = cfstr(ty);
        let name = cfstr(reason);
        if ty.is_null() || name.is_null() {
            // SAFETY: CFRelease tolerates only non-null; guarded.
            unsafe {
                if !ty.is_null() {
                    CFRelease(ty);
                }
                if !name.is_null() {
                    CFRelease(name);
                }
            }
            return Err(std::io::Error::other("could not build CFStrings"));
        }
        let mut id: u32 = 0;
        // SAFETY: both CFStrings are valid and non-null for the call; `id` is a
        // valid out-pointer. IOKit copies what it needs.
        let rc = unsafe { IOPMAssertionCreateWithName(ty, ASSERTION_LEVEL_ON, name, &mut id) };
        // SAFETY: we own both strings and IOKit does not retain them past the
        // call in a way that requires us to hold them.
        unsafe {
            CFRelease(ty);
            CFRelease(name);
        }
        if rc == 0 {
            Ok(Handle(id))
        } else {
            Err(std::io::Error::other(format!(
                "IOPMAssertionCreateWithName failed: {rc}"
            )))
        }
    }

    pub fn release(h: Handle) {
        // SAFETY: `h.0` is an id IOKit gave us and we have not released yet —
        // `Handle` is not `Copy`, so it cannot be released twice.
        unsafe {
            IOPMAssertionRelease(h.0);
        }
    }

    /// Wake the display, as `caffeinate -u` does. The returned assertion
    /// postpones display sleep until it is released (or powerd times it out
    /// at the display-sleep setting — releasing a timed-out id is harmless).
    pub fn declare_user_activity(reason: &str) -> std::io::Result<Handle> {
        let name = cfstr(reason);
        if name.is_null() {
            return Err(std::io::Error::other("could not build a CFString"));
        }
        let mut id: u32 = 0;
        // SAFETY: `name` is a valid, non-null CFString for the call and `id` a
        // valid out-pointer.
        let rc = unsafe { IOPMAssertionDeclareUserActivity(name, USER_ACTIVE_LOCAL, &mut id) };
        // SAFETY: we own `name`; IOKit copies what it keeps.
        unsafe { CFRelease(name) };
        if rc == 0 {
            Ok(Handle(id))
        } else {
            Err(std::io::Error::other(format!(
                "IOPMAssertionDeclareUserActivity failed: {rc}"
            )))
        }
    }

    pub fn display_asleep() -> bool {
        // SAFETY: neither call takes a pointer; the id is one CoreGraphics
        // just returned.
        unsafe { CGDisplayIsAsleep(CGMainDisplayID()) != 0 }
    }

    pub fn on_ac() -> Option<bool> {
        /// `kIOPSTimeRemainingUnlimited`.
        const UNLIMITED: f64 = -2.0;
        /// `kIOPSTimeRemainingUnknown`.
        const UNKNOWN: f64 = -1.0;
        // SAFETY: takes no arguments and cannot fail.
        let t = unsafe { IOPSGetTimeRemainingEstimate() };
        if t == UNLIMITED {
            Some(true)
        } else if t == UNKNOWN {
            None
        } else {
            Some(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Windows — a power request, not SetThreadExecutionState
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    // ⚠️ These live in TWO modules, not one: the request APIs and the request
    // TYPE are under `System::Power`, while the reason CONTEXT that describes
    // them is under `System::Threading`.
    use windows_sys::Win32::System::Power::{
        GetSystemPowerStatus, PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired,
        PowerSetRequest, SYSTEM_POWER_STATUS,
    };
    use windows_sys::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };

    pub struct Handle(HANDLE);
    // SAFETY: a power-request HANDLE is just a kernel handle; it is not bound
    // to the creating thread, and we only ever touch it from the keeper.
    unsafe impl Send for Handle {}

    pub fn acquire(reason: &str) -> std::io::Result<Handle> {
        // ⚠️ `PowerCreateRequest`, NOT `SetThreadExecutionState`: the latter is
        // per-THREAD, so it is tied to whichever tokio worker happened to run
        // the call and is documented as unreliable from a service — which is
        // exactly how `roomlerd` runs on a fleet host.
        let mut wide: Vec<u16> = reason.encode_utf16().collect();
        wide.push(0);
        let ctx = REASON_CONTEXT {
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: wide.as_mut_ptr(),
            },
        };
        // SAFETY: `ctx` and the string it points at are alive for the call, and
        // Windows copies the reason string into the request object.
        let h = unsafe { PowerCreateRequest(&ctx) };
        if h.is_null() || h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `h` is the handle just created.
        let ok = unsafe { PowerSetRequest(h, PowerRequestSystemRequired) };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: closing the handle we just created.
            unsafe {
                CloseHandle(h);
            }
            return Err(e);
        }
        Ok(Handle(h))
    }

    pub fn release(h: Handle) {
        // SAFETY: `h.0` was created by `acquire` and set once; clearing an
        // already-clear request is harmless, and `Handle` is not `Copy`.
        unsafe {
            PowerClearRequest(h.0, PowerRequestSystemRequired);
            CloseHandle(h.0);
        }
    }

    pub fn on_ac() -> Option<bool> {
        let mut st: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a valid out-pointer for the call.
        let ok = unsafe { GetSystemPowerStatus(&mut st) };
        if ok == 0 {
            return None;
        }
        match st.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            // 255 = "unknown", which must NOT be read as mains.
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Linux with D-Bus — a logind inhibitor (FR-97 P3b)
// ---------------------------------------------------------------------------
#[cfg(all(target_os = "linux", feature = "portal-capture"))]
mod imp {
    /// The inhibitor lives exactly as long as this fd: logind drops it the
    /// moment the fd closes, so a daemon that crashes cannot leave the machine
    /// pinned awake (the shape FR-43's orphan taught).
    pub struct Handle {
        _fd: std::os::fd::OwnedFd,
    }

    /// Ask logind for a `block` inhibitor, from a plain thread: zbus's
    /// blocking API panics inside the tokio runtime the keeper runs on.
    ///
    /// `sleep:idle` first. A per-user daemon runs outside every login session,
    /// where logind's polkit policy grants only `idle`
    /// (`inhibit-block-sleep` is `allow_any=no`), so it falls back to that and
    /// says so. A root daemon gets both.
    pub fn acquire(reason: &str) -> std::io::Result<Handle> {
        let why = reason.to_owned();
        std::thread::Builder::new()
            .name("power-inhibit".into())
            .spawn(move || inhibit(&why))?
            .join()
            .map_err(|_| std::io::Error::other("the logind inhibit thread panicked"))?
    }

    fn inhibit(why: &str) -> std::io::Result<Handle> {
        let conn = zbus::blocking::Connection::system().map_err(std::io::Error::other)?;
        let mut refused = String::from("nothing asked");
        for what in ["sleep:idle", "idle"] {
            match conn.call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "Inhibit",
                &(what, "roomlerd", why, "block"),
            ) {
                Ok(reply) => {
                    let fd = reply
                        .body()
                        .deserialize::<zbus::zvariant::OwnedFd>()
                        .map_err(std::io::Error::other)?;
                    if what != "sleep:idle" {
                        tracing::warn!(
                            %refused,
                            "power: logind refused a sleep inhibitor (a per-user daemon is outside every login session); holding idle only"
                        );
                    }
                    tracing::info!(what, "power: logind inhibitor taken");
                    return Ok(Handle {
                        _fd: std::os::fd::OwnedFd::from(fd),
                    });
                }
                Err(e) => refused = e.to_string(),
            }
        }
        Err(std::io::Error::other(format!(
            "logind refused an inhibitor: {refused}"
        )))
    }

    /// Closing the fd is the release.
    pub fn release(_h: Handle) {}

    pub fn on_ac() -> Option<bool> {
        super::sysfs_on_ac()
    }
}

// ---------------------------------------------------------------------------
// Everything else — a Linux build without D-Bus, and the rest
// ---------------------------------------------------------------------------
#[cfg(not(any(
    target_os = "macos",
    windows,
    all(target_os = "linux", feature = "portal-capture")
)))]
mod imp {
    pub struct Handle;

    /// ⚠️ Fails LOUDLY once rather than pretending. logind's `Inhibit()` is
    /// D-Bus-only (no file or sysctl takes a sleep lock), and this build has no
    /// D-Bus stack: `zbus` comes with `portal-capture`, which every shipped
    /// Linux build carries (`full`), so only a hand-built agent lands here.
    pub fn acquire(_reason: &str) -> std::io::Result<Handle> {
        Err(std::io::Error::other(
            "no sleep-inhibit backend in this build (logind needs D-Bus: the portal-capture feature)",
        ))
    }

    pub fn release(_h: Handle) {}

    pub fn on_ac() -> Option<bool> {
        super::sysfs_on_ac()
    }
}

/// Readable without D-Bus, so the policy can at least be REPORTED correctly
/// even where it cannot be enforced.
#[cfg(not(any(target_os = "macos", windows)))]
fn sysfs_on_ac() -> Option<bool> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let mut saw_mains = false;
    for e in dir.flatten() {
        let p = e.path();
        let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
        if kind.trim() != "Mains" {
            continue;
        }
        saw_mains = true;
        if std::fs::read_to_string(p.join("online"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
        {
            return Some(true);
        }
    }
    // A machine with a mains supply that reports offline is on battery; a
    // machine with NO mains supply at all (a server, a VM) tells us nothing
    // about a battery, so say so rather than guess.
    saw_mains.then_some(false)
}

/// Escape hatch for the macOS assertion type, so the open decision in FR-55 can
/// be measured on a real host without a rebuild. `None` everywhere else.
#[cfg(target_os = "macos")]
static ASSERTION_TYPE_OVERRIDE: Option<&str> = None;

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Inputs {
        Inputs {
            policy: PowerPolicy::Never,
            on_ac: Some(true),
            session_active: false,
        }
    }

    /// The rule that needs no configuration to be correct: a live session is
    /// never cut by an idle timer.
    ///
    /// ⚠️ It must hold under `Never` too — that policy means "do not keep this
    /// machine awake FOR THE MESH", not "let a session you are watching die".
    #[test]
    fn an_active_session_outranks_every_policy() {
        for policy in [PowerPolicy::Never, PowerPolicy::OnAc, PowerPolicy::Always] {
            for on_ac in [Some(true), Some(false), None] {
                assert!(
                    should_stay_awake(Inputs {
                        policy,
                        on_ac,
                        session_active: true,
                    }),
                    "a live session must win over {policy:?} / on_ac={on_ac:?}"
                );
            }
        }
    }

    /// The default must be byte-for-byte the pre-FR-55 world.
    #[test]
    fn never_is_the_default_and_holds_nothing() {
        assert_eq!(PowerPolicy::default(), PowerPolicy::Never);
        assert!(!should_stay_awake(base()));
        assert!(!should_stay_awake(Inputs {
            on_ac: Some(false),
            ..base()
        }));
    }

    /// `on-ac` is the setting a laptop wants, so the battery case is the one
    /// that must not regress.
    #[test]
    fn on_ac_follows_the_power_source() {
        assert!(should_stay_awake(Inputs {
            policy: PowerPolicy::OnAc,
            on_ac: Some(true),
            ..base()
        }));
        assert!(!should_stay_awake(Inputs {
            policy: PowerPolicy::OnAc,
            on_ac: Some(false),
            ..base()
        }));
    }

    /// ⚠️ "On battery" and "we could not tell" must not be collapsed: reading
    /// an unknown power source as mains would turn `on-ac` into `always` on
    /// every host whose supply we cannot see, which is the failure mode that
    /// drains a battery in a bag.
    #[test]
    fn an_unknown_power_source_is_treated_as_battery() {
        assert!(!should_stay_awake(Inputs {
            policy: PowerPolicy::OnAc,
            on_ac: None,
            ..base()
        }));
        // …but `always` still means always: the operator said so explicitly.
        assert!(should_stay_awake(Inputs {
            policy: PowerPolicy::Always,
            on_ac: None,
            ..base()
        }));
    }

    /// An unrecognised config value must fail SAFE. A typo'd `power_policy`
    /// should cost reachability, never someone's battery.
    #[test]
    fn an_unparseable_policy_is_never() {
        assert_eq!(PowerPolicy::parse("always"), PowerPolicy::Always);
        assert_eq!(PowerPolicy::parse("on-ac"), PowerPolicy::OnAc);
        assert_eq!(PowerPolicy::parse("on_ac"), PowerPolicy::OnAc);
        assert_eq!(PowerPolicy::parse("  ALWAYS  "), PowerPolicy::Always);
        assert_eq!(PowerPolicy::parse("never"), PowerPolicy::Never);
        assert_eq!(PowerPolicy::parse(""), PowerPolicy::Never);
        assert_eq!(PowerPolicy::parse("alwyas"), PowerPolicy::Never);
        assert_eq!(PowerPolicy::parse("yes"), PowerPolicy::Never);
    }

    /// The config value round-trips, so `roomler config get` shows back what
    /// was set rather than a normalised surprise.
    #[test]
    fn the_policy_round_trips_through_its_config_string() {
        for p in [PowerPolicy::Never, PowerPolicy::OnAc, PowerPolicy::Always] {
            assert_eq!(PowerPolicy::parse(p.as_str()), p);
        }
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;

    /// A count, not a flag. Two overlapping sessions, the first ending, and the
    /// machine must still be held — the bug a `bool` would have.
    #[test]
    fn overlapping_work_keeps_the_machine_held() {
        let c = new_activity_counter();
        assert_eq!(c.load(Ordering::Relaxed), 0);
        let a = ActivityGuard::new(&c);
        let b = ActivityGuard::new(&c);
        assert_eq!(c.load(Ordering::Relaxed), 2);
        drop(a);
        assert_eq!(
            c.load(Ordering::Relaxed),
            1,
            "one session ending must not release another's hold"
        );
        drop(b);
        assert_eq!(c.load(Ordering::Relaxed), 0);
    }

    /// The guard must survive being moved into a struct that is dropped on an
    /// unusual path — which is every path an SSH session actually takes.
    #[test]
    fn a_guard_released_by_unwinding_still_decrements() {
        let c = new_activity_counter();
        let res = std::panic::catch_unwind({
            let c = c.clone();
            move || {
                let _g = ActivityGuard::new(&c);
                panic!("session died the ugly way");
            }
        });
        assert!(res.is_err());
        assert_eq!(
            c.load(Ordering::Relaxed),
            0,
            "a guard dropped by unwinding must still release"
        );
    }
}

/// #3871 — the keeper on a runtime like the daemon's, with the OS stood in: a
/// stay-awake request that hangs as logind's `Inhibit` does while logind does
/// not answer, for a session of either kind rule 1 counts.
#[cfg(test)]
mod keeper_tests {
    use super::*;
    use crate::rc_sessions::RcSessionRegistry;
    use bson::oid::ObjectId;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::{Condvar, Mutex, PoisonError};
    use std::time::Instant;

    /// The keeper's pass in these tests; the daemon's is [`POLL`], 5 s.
    const EVERY: Duration = Duration::from_millis(50);
    /// A request nobody answers gives up after this, as a D-Bus call to a
    /// logind that never answers ends at last. It lets a keeper that waited
    /// for its request end its run too.
    const GIVES_UP: Duration = Duration::from_secs(4);
    /// A pass, a held request or a release comes within this of when it is
    /// due.
    const BOUND: Duration = Duration::from_millis(750);
    /// A 10 ms timer later than this means the runtime stood still. The
    /// daemon's canary warns at 100 ms; this leaves room for a loaded runner.
    const LATE: Duration = Duration::from_millis(200);
    /// How long a request is left hanging while the keeper is watched.
    const HUNG_FOR: Duration = Duration::from_millis(1500);

    /// What the stood-in OS does with a request.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Answer {
        /// Nothing, until the test changes its answer (logind paused).
        Hangs,
        Grants,
        Refuses,
    }

    /// The OS stood in. Each request it grants is an id, released once.
    #[derive(Clone)]
    struct Os(Arc<OsState>);

    struct OsState {
        answer: Mutex<Answer>,
        answered: Condvar,
        /// Requests that began.
        began: AtomicUsize,
        /// Granted requests let go.
        released: AtomicUsize,
        /// Each pass's posture, and when it was published.
        passes: Mutex<Vec<(Instant, Posture)>>,
    }

    impl Os {
        fn answering(answer: Answer) -> Self {
            Self(Arc::new(OsState {
                answer: Mutex::new(answer),
                answered: Condvar::new(),
                began: AtomicUsize::new(0),
                released: AtomicUsize::new(0),
                passes: Mutex::new(Vec::new()),
            }))
        }

        fn answer(&self, answer: Answer) {
            *self.0.answer.lock().unwrap_or_else(PoisonError::into_inner) = answer;
            self.0.answered.notify_all();
        }

        fn began(&self) -> usize {
            self.0.began.load(SeqCst)
        }

        fn released(&self) -> usize {
            self.0.released.load(SeqCst)
        }

        /// The postures published at or after `since`.
        fn passes_since(&self, since: Instant) -> Vec<(Instant, Posture)> {
            self.0
                .passes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .filter(|(at, _)| *at >= since)
                .copied()
                .collect()
        }

        /// One request, answered as the OS answers it now, or as it comes to
        /// answer it while the request waits.
        fn request(&self) -> std::io::Result<u32> {
            let id = self.0.began.fetch_add(1, SeqCst) as u32;
            let gives_up = Instant::now() + GIVES_UP;
            let mut answer = self.0.answer.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                match *answer {
                    Answer::Grants => return Ok(id),
                    Answer::Refuses => return Err(std::io::Error::other("refused")),
                    Answer::Hangs => {}
                }
                let Some(left) = gives_up
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                else {
                    return Err(std::io::Error::other("no answer: the call timed out"));
                };
                answer = self
                    .0
                    .answered
                    .wait_timeout(answer, left)
                    .map(|(answer, _)| answer)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
        }

        fn backend(&self) -> Backend<u32> {
            let (asks, lets_go, records) = (self.clone(), self.clone(), self.clone());
            Backend {
                acquire: Arc::new(move || asks.request()),
                release: Arc::new(move |_| {
                    lets_go.0.released.fetch_add(1, SeqCst);
                }),
                on_ac: Arc::new(|| Some(true)),
                publish: Arc::new(move |p| {
                    records
                        .0
                        .passes
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((Instant::now(), p));
                }),
            }
        }
    }

    /// The two kinds of session rule 1 counts, each as the keeper reads it.
    #[derive(Clone, Copy, Debug)]
    enum Kind {
        /// A remote-control session: its row in the registry.
        Rc,
        /// An SSH session (or an exec run): its hold on the activity counter.
        Ssh,
    }

    /// The device's live sessions, as [`run`] is handed them.
    struct Device {
        registry: RcSessionRegistry,
        activity: ActivityCounter,
        kill: crate::rc_sessions::KillSender,
        _killed: tokio::sync::mpsc::Receiver<ObjectId>,
    }

    /// A session that is live while this is.
    enum Live {
        Rc(RcSessionRegistry, ObjectId),
        Ssh(#[allow(dead_code)] ActivityGuard),
    }

    impl Drop for Live {
        fn drop(&mut self) {
            if let Live::Rc(registry, id) = self {
                registry.remove(id);
            }
        }
    }

    impl Device {
        fn new() -> Self {
            let (kill, killed) = tokio::sync::mpsc::channel(4);
            Self {
                registry: RcSessionRegistry::new(),
                activity: new_activity_counter(),
                kill,
                _killed: killed,
            }
        }

        fn start(&self, kind: Kind) -> Live {
            match kind {
                Kind::Rc => {
                    let id = ObjectId::new();
                    self.registry.insert(
                        id,
                        "a viewer".into(),
                        "view".into(),
                        "an org".into(),
                        self.kill.clone(),
                    );
                    Live::Rc(self.registry.clone(), id)
                }
                Kind::Ssh => Live::Ssh(ActivityGuard::new(&self.activity)),
            }
        }

        /// The keeper of `os` running for this device, a pass every [`EVERY`].
        fn keeper(
            &self,
            os: &Os,
        ) -> (
            tokio::sync::watch::Sender<bool>,
            tokio::task::JoinHandle<()>,
        ) {
            let (stop, stopped) = tokio::sync::watch::channel(false);
            let keeper = PowerKeeper::with_backend(PowerPolicy::Never, os.backend());
            let task = tokio::spawn(keep(
                keeper,
                Some(self.registry.clone()),
                self.activity.clone(),
                EVERY,
                stopped,
            ));
            (stop, task)
        }
    }

    /// A 10 ms timer beside the keeper, as the daemon's runtime canary is:
    /// how late its latest wake came, at most.
    fn canary() -> (Arc<Mutex<Duration>>, tokio::task::JoinHandle<()>) {
        let worst = Arc::new(Mutex::new(Duration::ZERO));
        let record = worst.clone();
        let task = tokio::spawn(async move {
            loop {
                let due = Instant::now() + Duration::from_millis(10);
                tokio::time::sleep(Duration::from_millis(10)).await;
                let late = Instant::now().saturating_duration_since(due);
                let mut worst = record.lock().unwrap_or_else(PoisonError::into_inner);
                *worst = (*worst).max(late);
            }
        });
        (worst, task)
    }

    /// Wait until `done`, `within` at most, or fail saying `what`.
    async fn until(within: Duration, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + within;
        while !done() {
            assert!(Instant::now() < deadline, "{what}: not within {within:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The keeper's first pass, with nothing live: nothing wanted.
    async fn first_pass(os: &Os) {
        until(BOUND, "the keeper's first pass", || {
            !os.0
                .passes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        })
        .await;
    }

    /// #3871 — logind paused while a session of `kind` is live: the request to
    /// stay awake hangs, and neither the keeper nor the runtime waits for it.
    /// The passes go on at their pace and publish nothing held, a 10 ms timer
    /// beside them stays on time, and one request is in flight however many
    /// passes want one. Once the OS answers, the request is held; once the
    /// session ends, it is let go. As in the field, the person socket's poll
    /// has a lookup in flight on a blocking thread when the session starts.
    async fn a_request_the_os_does_not_answer_holds_neither_the_keeper_nor_the_runtime(kind: Kind) {
        let os = Os::answering(Answer::Hangs);
        let device = Device::new();
        let (worst, canary) = canary();
        let (stop, keeper) = device.keeper(&os);
        first_pass(&os).await;
        let poll = tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(2500)));

        // The session starts between two passes.
        let session = device.start(kind);
        let started = Instant::now();
        tokio::time::sleep(HUNG_FOR).await;
        let passes = os.passes_since(started);
        let first = passes.first().map(|(at, _)| at.duration_since(started));
        let late = *worst.lock().unwrap_or_else(PoisonError::into_inner);
        eprintln!(
            "{kind:?}: watched {:?} while the request hung: first pass after {first:?}, {} passes, \
             {} held, {} requests began, the 10 ms timer at most {late:?} late",
            started.elapsed(),
            passes.len(),
            passes.iter().filter(|(_, p)| p.held).count(),
            os.began(),
        );
        assert!(
            first.is_some_and(|first| first <= EVERY + BOUND),
            "{kind:?}: the keeper's pass after the session started came after {first:?}, not \
             within {:?}: it waited for the request",
            EVERY + BOUND
        );
        assert!(
            passes.len() >= 10,
            "{kind:?}: {} passes in {HUNG_FOR:?}, one every {EVERY:?} wanted",
            passes.len()
        );
        assert!(
            passes.iter().all(|(_, p)| !p.held),
            "{kind:?}: nothing is published as held while the request hangs"
        );
        assert_eq!(os.began(), 1, "{kind:?}: one request in flight");
        assert!(
            late <= LATE,
            "{kind:?}: the 10 ms timer woke {late:?} late: the runtime stood still"
        );

        // The OS answers: held at once, not a pass later.
        let answered = Instant::now();
        os.answer(Answer::Grants);
        until(BOUND, "held once the OS answers", || {
            os.passes_since(answered).iter().any(|(_, p)| p.held)
        })
        .await;

        // The session ends: let go at the next pass.
        drop(session);
        let ended = Instant::now();
        until(EVERY + BOUND, "let go once the session ended", || {
            os.released() == 1
        })
        .await;
        until(EVERY + BOUND, "published as not held", || {
            os.passes_since(ended).last().is_some_and(|(_, p)| !p.held)
        })
        .await;

        let _ = stop.send(true);
        keeper.await.expect("the keeper ends");
        canary.abort();
        poll.await.expect("the stood-in poll lookup ends");
        assert_eq!(os.began(), 1, "{kind:?}: one request in all");
        assert_eq!(os.released(), 1, "{kind:?}: the one held, let go once");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_the_os_does_not_answer_holds_neither_the_keeper_nor_the_runtime_rc() {
        a_request_the_os_does_not_answer_holds_neither_the_keeper_nor_the_runtime(Kind::Rc).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_the_os_does_not_answer_holds_neither_the_keeper_nor_the_runtime_ssh() {
        a_request_the_os_does_not_answer_holds_neither_the_keeper_nor_the_runtime(Kind::Ssh).await;
    }

    /// #3871 — a request the OS answers only after the session has ended is
    /// let go as soon as it lands, and never published as held.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_answered_after_the_session_ended_is_let_go_at_once() {
        let os = Os::answering(Answer::Hangs);
        let device = Device::new();
        let (stop, keeper) = device.keeper(&os);
        first_pass(&os).await;
        let session = device.start(Kind::Ssh);
        until(EVERY + BOUND, "the request began", || os.began() == 1).await;
        drop(session);
        tokio::time::sleep(EVERY * 3).await;

        let answered = Instant::now();
        os.answer(Answer::Grants);
        until(BOUND, "the late request let go", || os.released() == 1).await;
        tokio::time::sleep(EVERY * 3).await;
        let passes = os.passes_since(answered);
        assert!(
            passes.iter().all(|(_, p)| !p.held),
            "a request that landed after the session ended is never published as held"
        );
        let _ = stop.send(true);
        keeper.await.expect("the keeper ends");
        assert_eq!(os.began(), 1, "one request in all");
    }

    /// #3871 — a keeper that stops while its request still hangs stops at
    /// once, and the request is let go when it lands: nothing it took
    /// outlives it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_still_hanging_when_the_keeper_stops_is_let_go_when_it_lands() {
        let os = Os::answering(Answer::Hangs);
        let device = Device::new();
        let (stop, keeper) = device.keeper(&os);
        first_pass(&os).await;
        let _session = device.start(Kind::Rc);
        until(EVERY + BOUND, "the request began", || os.began() == 1).await;

        let stopping = Instant::now();
        let _ = stop.send(true);
        let stopped = tokio::time::timeout(BOUND, keeper).await;
        let took = stopping.elapsed();
        assert!(
            stopped.is_ok() && took <= BOUND,
            "the keeper stopped {took:?} after it was told to, not within {BOUND:?}: it waited \
             for its request"
        );
        os.answer(Answer::Grants);
        until(BOUND, "the request let go once it landed", || {
            os.released() == 1
        })
        .await;
    }

    /// The control: an OS that grants at once is held on the first pass that
    /// wants it, as before, and let go when the session ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_the_os_grants_at_once_is_held_at_once() {
        for kind in [Kind::Rc, Kind::Ssh] {
            let os = Os::answering(Answer::Grants);
            let device = Device::new();
            let (stop, keeper) = device.keeper(&os);
            first_pass(&os).await;
            let session = device.start(kind);
            let started = Instant::now();
            until(EVERY + BOUND, "held while the session is live", || {
                os.passes_since(started).iter().any(|(_, p)| p.held)
            })
            .await;
            drop(session);
            until(EVERY + BOUND, "let go once the session ended", || {
                os.released() == 1
            })
            .await;
            let _ = stop.send(true);
            keeper.await.expect("the keeper ends");
            assert_eq!(os.began(), 1, "{kind:?}: one request");
        }
    }

    /// An OS that refuses is asked again once a pass, never in a loop: a
    /// refusal that comes back at once must not spin the keeper.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_request_is_asked_again_once_a_pass() {
        let os = Os::answering(Answer::Refuses);
        let device = Device::new();
        let (stop, keeper) = device.keeper(&os);
        first_pass(&os).await;
        let _session = device.start(Kind::Ssh);
        let started = Instant::now();
        tokio::time::sleep(EVERY * 10).await;
        let (began, watched) = (os.began(), started.elapsed());
        let _ = stop.send(true);
        keeper.await.expect("the keeper ends");
        let most = (watched.as_millis() / EVERY.as_millis()) as usize + 2;
        assert!(
            (2..=most).contains(&began),
            "{began} requests in {watched:?}: one a pass ({EVERY:?}) wanted, at most {most}"
        );
        assert!(
            os.passes_since(started).iter().all(|(_, p)| !p.held),
            "a refused request is never held"
        );
    }
}
