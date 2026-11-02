// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Session-scoped NVIDIA GPU clock pin (NVML) — kills the idle-P-state
//! encode-latency ramp on NVENC senders.
//!
//! Field story (DEVBOX, RTX 5090): an idle desktop lets the GPU drop into a
//! low P-state; the first seconds of a remote-desktop session encode at
//! ~20 ms/frame until the clocks ramp, and light-motion sessions can bounce
//! in and out of the slow state indefinitely. A manual `nvidia-smi -lgc`
//! (lock graphics clocks) was field-validated to fix it — this module is
//! that lever, applied automatically for exactly the lifetime of remote
//! sessions. (Parsec's "boost" service does the same thing.)
//!
//! FR-119 P2b-16 (field 2026-10-25, neo16, RTX 5090 laptop): above 60 fps the
//! driver still drops the GPU to P5 under an encoder-only load (NVENC at
//! 15–18 %), and **the memory clock goes with it, to 810 of 14 001 MHz**,
//! whatever the graphics clock is locked to. NVENC's time per frame then
//! doubled (4.5 → 8.5 ms) and a 120 fps session held ~105. With the graphics
//! clock locked alone it was still 6.2–6.6 ms; with the memory clock locked
//! too it held 4.3–4.6 ms. So the pin now locks both, and engages by itself
//! for an NVENC session above 60 fps.
//!
//! Design constraints:
//!
//! * **`ROOMLERD_GPU_CLOCK_PIN`**: unset = **high rate** (pin while a session
//!   runs above 60 fps on NVENC; P2b-16), `0`/`false` = off, `1`/`true` = every
//!   session (graphics [70 % of max, max]), `"<min>,<max>"` = every session at
//!   an explicit graphics band. Garbage reads as off, never a surprise pin.
//! * **The clocks need admin.** A SYSTEM or root daemon pins directly. An SCM
//!   worker runs as the signed-in person (measured: the attended worker on
//!   neo16 is the person, only the host is SYSTEM), so it renews a lease on
//!   the host's relay instead (`--gpu-pin-event`, `win_service::gpu_pin_relay`)
//!   and the host, the one owner of the clocks, pins them while the lease is
//!   alive. Anything else gets `NVML_ERROR_NO_PERMISSION` and a hint.
//! * **NVML is dlopen'd** (`nvml.dll` / `libnvidia-ml.so.1`), never linked:
//!   non-NVIDIA hosts and CI pay nothing, no build-time dependency on the
//!   CUDA stack. The memory-clock entry points are optional: an older driver
//!   without them still gets the graphics pin.
//! * **Never leave clocks pinned.** The pin drops (clocks reset) when nothing
//!   wants it; a lease that stops being renewed lapses in seconds; and a pin
//!   leaves a marker, so the next start of a crashed process undoes exactly
//!   its own lock ([`reset_stale_pins`]) and never someone else's.
//!
//! Pure parse + policy logic is unit-tested on the default build; the FFI
//! paths are field-gated (CI has no NVIDIA device).

use std::ffi::{c_char, c_uint, c_void};
use std::path::PathBuf;
use std::sync::Mutex;

use libloading::Library;
use tunnel_core::env::node_env;

const NVML_SUCCESS: i32 = 0;
const NVML_ERROR_NO_PERMISSION: i32 = 4;
/// `nvmlClockType_t::NVML_CLOCK_GRAPHICS` — the clock domain `-lgc` locks
/// (the video/NVENC clock follows it on shipping SKUs).
const NVML_CLOCK_GRAPHICS: c_uint = 0;
/// `NVML_DEVICE_NAME_V2_BUFFER_SIZE`.
const NAME_BUF: usize = 96;
/// Room for a device's supported memory clocks (a handful on every SKU).
const MEM_CLOCKS_BUF: usize = 64;
/// FR-119 P2b-16 — the rate above which an NVENC session wants the pin.
pub const HIGH_RATE_ABOVE_FPS: u32 = 60;
/// The file a pin leaves while it holds the clocks, next to the logs.
const MARKER: &str = "gpu-clock-pin.active";

/// Parsed `ROOMLERD_GPU_CLOCK_PIN` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    /// `0` / `false` / unparsable — feature disabled.
    Off,
    /// Unset / empty — pin while an NVENC session runs above 60 fps
    /// (FR-119 P2b-16). The default.
    HighRate,
    /// `1` / `true` — pin for every session, graphics [70 % of max, max].
    Auto,
    /// `"<min>,<max>"` MHz, validated `0 < min <= max`, for every session.
    Explicit { min_mhz: u32, max_mhz: u32 },
}

/// Pure + exported so the accepted grammar is locked by tests. Garbage
/// degrades to `Off` (never a panic, never a surprise pin).
pub fn parse_pin_mode(raw: Option<&str>) -> PinMode {
    let Some(raw) = raw else {
        return PinMode::HighRate;
    };
    let v = raw.trim();
    match v {
        "" => PinMode::HighRate,
        "0" | "false" => PinMode::Off,
        "1" | "true" => PinMode::Auto,
        _ => {
            let Some((min, max)) = v.split_once(',') else {
                return PinMode::Off;
            };
            match (min.trim().parse::<u32>(), max.trim().parse::<u32>()) {
                (Ok(min_mhz), Ok(max_mhz)) if min_mhz > 0 && min_mhz <= max_mhz => {
                    PinMode::Explicit { min_mhz, max_mhz }
                }
                _ => PinMode::Off,
            }
        }
    }
}

fn mode() -> PinMode {
    parse_pin_mode(node_env("GPU_CLOCK_PIN").as_deref())
}

/// The graphics band to pin, given a device's max graphics clock. High rate
/// and auto = [70 %, max] — high enough to hold encode latency flat, below
/// max to leave thermal headroom. Pure for tests.
pub fn pin_band(mode: PinMode, device_max_mhz: u32) -> Option<(u32, u32)> {
    match mode {
        PinMode::Off => None,
        PinMode::HighRate | PinMode::Auto => {
            if device_max_mhz == 0 {
                None
            } else {
                Some((device_max_mhz * 7 / 10, device_max_mhz))
            }
        }
        PinMode::Explicit { min_mhz, max_mhz } => Some((min_mhz, max_mhz)),
    }
}

/// FR-119 P2b-16 — the memory band to pin, from a device's supported memory
/// clocks: up to the highest, from the highest supported clock at or below
/// 70 % of it (the driver accepts only supported clocks as the floor). On
/// neo16 `[405, 810, 9001, 11001, 14001]` gives `(9001, 14001)`, the band that
/// held NVENC at 4.3–4.6 ms. Pure for tests.
pub fn memory_band(supported_mhz: &[u32]) -> Option<(u32, u32)> {
    let max = supported_mhz.iter().copied().max().filter(|&m| m > 0)?;
    let floor = max * 7 / 10;
    let min = supported_mhz
        .iter()
        .copied()
        .filter(|&c| c > 0 && c <= floor)
        .max()
        .unwrap_or(max);
    Some((min, max))
}

/// Whether anything wants the clocks pinned now. Pure for tests.
pub fn wanted(mode: PinMode, sessions: usize, high_rate: usize) -> bool {
    match mode {
        PinMode::Off => false,
        PinMode::HighRate => high_rate > 0,
        PinMode::Auto | PinMode::Explicit { .. } => sessions > 0,
    }
}

type FnInit = unsafe extern "C" fn() -> i32;
type FnShutdown = unsafe extern "C" fn() -> i32;
type FnDeviceCount = unsafe extern "C" fn(*mut c_uint) -> i32;
type FnDeviceByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> i32;
type FnDeviceName = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> i32;
type FnMaxClock = unsafe extern "C" fn(*mut c_void, c_uint, *mut c_uint) -> i32;
type FnSetLocked = unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> i32;
type FnResetLocked = unsafe extern "C" fn(*mut c_void) -> i32;
type FnSupportedMemClocks = unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut c_uint) -> i32;

/// dlopen'd NVML with the handful of symbols we use. The raw fn pointers
/// are copies out of the `Library` — the struct keeps `_lib` alive so they
/// stay valid for its lifetime.
struct Nvml {
    _lib: Library,
    init: FnInit,
    shutdown: FnShutdown,
    device_count: FnDeviceCount,
    device_by_index: FnDeviceByIndex,
    device_name: FnDeviceName,
    max_clock: FnMaxClock,
    set_locked: FnSetLocked,
    reset_locked: FnResetLocked,
    /// FR-119 P2b-16 — optional: absent on an older driver, which then gets
    /// the graphics pin alone.
    supported_mem_clocks: Option<FnSupportedMemClocks>,
    set_mem_locked: Option<FnSetLocked>,
    reset_mem_locked: Option<FnResetLocked>,
}

impl Nvml {
    fn load() -> Option<Self> {
        #[cfg(windows)]
        let candidates: &[&str] = &["nvml.dll"];
        #[cfg(not(windows))]
        let candidates: &[&str] = &["libnvidia-ml.so.1", "libnvidia-ml.so"];
        let lib = candidates.iter().find_map(|name| {
            // SAFETY: loading a well-known driver library by name; no
            // initialisation routines beyond the OS loader run here.
            unsafe { Library::new(name).ok() }
        })?;
        // SAFETY: symbol names + signatures match nvml.h for every driver
        // generation that ships these entry points; copied out as plain fn
        // pointers kept valid by `_lib`.
        unsafe {
            let init = *lib.get::<FnInit>(b"nvmlInit_v2\0").ok()?;
            let shutdown = *lib.get::<FnShutdown>(b"nvmlShutdown\0").ok()?;
            let device_count = *lib.get::<FnDeviceCount>(b"nvmlDeviceGetCount_v2\0").ok()?;
            let device_by_index = *lib
                .get::<FnDeviceByIndex>(b"nvmlDeviceGetHandleByIndex_v2\0")
                .ok()?;
            let device_name = *lib.get::<FnDeviceName>(b"nvmlDeviceGetName\0").ok()?;
            let max_clock = *lib.get::<FnMaxClock>(b"nvmlDeviceGetMaxClockInfo\0").ok()?;
            let set_locked = *lib
                .get::<FnSetLocked>(b"nvmlDeviceSetGpuLockedClocks\0")
                .ok()?;
            let reset_locked = *lib
                .get::<FnResetLocked>(b"nvmlDeviceResetGpuLockedClocks\0")
                .ok()?;
            let supported_mem_clocks = lib
                .get::<FnSupportedMemClocks>(b"nvmlDeviceGetSupportedMemoryClocks\0")
                .ok()
                .map(|s| *s);
            let set_mem_locked = lib
                .get::<FnSetLocked>(b"nvmlDeviceSetMemoryLockedClocks\0")
                .ok()
                .map(|s| *s);
            let reset_mem_locked = lib
                .get::<FnResetLocked>(b"nvmlDeviceResetMemoryLockedClocks\0")
                .ok()
                .map(|s| *s);
            Some(Self {
                _lib: lib,
                init,
                shutdown,
                device_count,
                device_by_index,
                device_name,
                max_clock,
                set_locked,
                reset_locked,
                supported_mem_clocks,
                set_mem_locked,
                reset_mem_locked,
            })
        }
    }

    fn devices(&self) -> Vec<*mut c_void> {
        let mut count: c_uint = 0;
        // SAFETY: out-pointer to a local; NVML initialised by the caller.
        if unsafe { (self.device_count)(&mut count) } != NVML_SUCCESS {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0..count {
            let mut handle: *mut c_void = std::ptr::null_mut();
            // SAFETY: out-pointer to a local; index bounded by the count.
            if unsafe { (self.device_by_index)(i, &mut handle) } == NVML_SUCCESS
                && !handle.is_null()
            {
                out.push(handle);
            }
        }
        out
    }

    fn name(&self, device: *mut c_void) -> String {
        let mut buf = [0i8; NAME_BUF];
        // SAFETY: buffer sized to NVML_DEVICE_NAME_V2_BUFFER_SIZE.
        if unsafe {
            (self.device_name)(device, buf.as_mut_ptr() as *mut c_char, NAME_BUF as c_uint)
        } != NVML_SUCCESS
        {
            return "unknown".into();
        }
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// FR-119 P2b-16 — the device's supported memory clocks; empty when the
    /// driver cannot say.
    fn memory_clocks(&self, device: *mut c_void) -> Vec<u32> {
        let Some(f) = self.supported_mem_clocks else {
            return Vec::new();
        };
        let mut buf = [0 as c_uint; MEM_CLOCKS_BUF];
        let mut count = MEM_CLOCKS_BUF as c_uint;
        // SAFETY: valid device handle; `count` holds the buffer's capacity in,
        // the number written out.
        if unsafe { f(device, &mut count, buf.as_mut_ptr()) } != NVML_SUCCESS {
            return Vec::new();
        }
        buf[..(count as usize).min(MEM_CLOCKS_BUF)].to_vec()
    }
}

/// RAII pin: holds NVML loaded and the devices whose clocks this process
/// locked; `Drop` resets them, removes the marker and shuts NVML down.
pub struct GpuClockPin {
    nvml: Nvml,
    pinned: Vec<*mut c_void>,
    /// FR-119 P2b-16 — the devices whose memory clock was locked too.
    mem_pinned: Vec<*mut c_void>,
}

// SAFETY: NVML is documented thread-safe and its device handles are
// process-global opaque identifiers (not memory this struct owns); the pin
// is only ever used behind the module Mutex or by the host relay's thread.
unsafe impl Send for GpuClockPin {}

impl GpuClockPin {
    /// Attempt to engage the pin in `mode`. `None` when the mode is off, NVML
    /// is absent (non-NVIDIA host), or no device accepted the lock (e.g. a
    /// process without admin rights — logged as a hint).
    fn engage(mode: PinMode) -> Option<Self> {
        if mode == PinMode::Off {
            return None;
        }
        let Some(nvml) = Nvml::load() else {
            tracing::debug!("gpu-clock pin wanted but NVML is not loadable (non-NVIDIA host?)");
            return None;
        };
        // SAFETY: entry point of the loaded library; balanced by shutdown.
        let rc = unsafe { (nvml.init)() };
        if rc != NVML_SUCCESS {
            tracing::info!(rc, "gpu-clock pin: nvmlInit failed — skipped");
            return None;
        }
        let mut pinned = Vec::new();
        let mut mem_pinned = Vec::new();
        for device in nvml.devices() {
            let mut max: c_uint = 0;
            // SAFETY: valid device handle from devices(); out-ptr to local.
            let band = if unsafe { (nvml.max_clock)(device, NVML_CLOCK_GRAPHICS, &mut max) }
                == NVML_SUCCESS
            {
                pin_band(mode, max)
            } else {
                pin_band(mode, 0)
            };
            let Some((min_mhz, max_mhz)) = band else {
                continue;
            };
            // SAFETY: valid device handle; plain u32 args.
            let rc = unsafe { (nvml.set_locked)(device, min_mhz, max_mhz) };
            match rc {
                NVML_SUCCESS => {
                    pinned.push(device);
                    // FR-119 P2b-16 — the memory clock too: it is what an
                    // encoder-only load lets the driver drop.
                    let mem = memory_band(&nvml.memory_clocks(device));
                    let mem_locked = match (nvml.set_mem_locked, mem) {
                        // SAFETY: valid device handle; plain u32 args.
                        (Some(f), Some((lo, hi))) => match unsafe { f(device, lo, hi) } {
                            NVML_SUCCESS => {
                                mem_pinned.push(device);
                                Some((lo, hi))
                            }
                            rc => {
                                tracing::debug!(rc, "gpu-clock pin: memory lock refused");
                                None
                            }
                        },
                        _ => None,
                    };
                    tracing::info!(
                        device = %nvml.name(device),
                        min_mhz,
                        max_mhz,
                        memory_mhz = ?mem_locked,
                        ?mode,
                        "gpu-clock pin engaged"
                    );
                }
                NVML_ERROR_NO_PERMISSION => {
                    tracing::info!(
                        device = %nvml.name(device),
                        "gpu-clock pin: no permission — needs a SYSTEM or root daemon, or the \
                         service host's relay; skipped"
                    );
                }
                _ => {
                    tracing::debug!(device = %nvml.name(device), rc, "gpu-clock pin: set failed");
                }
            }
        }
        if pinned.is_empty() {
            // SAFETY: balances the successful init above.
            unsafe { (nvml.shutdown)() };
            return None;
        }
        write_marker();
        Some(Self {
            nvml,
            pinned,
            mem_pinned,
        })
    }
}

impl Drop for GpuClockPin {
    fn drop(&mut self) {
        for device in &self.pinned {
            // SAFETY: handles were valid at engage; reset is idempotent.
            let _ = unsafe { (self.nvml.reset_locked)(*device) };
        }
        if let Some(reset_mem) = self.nvml.reset_mem_locked {
            for device in &self.mem_pinned {
                // SAFETY: as above.
                let _ = unsafe { reset_mem(*device) };
            }
        }
        // SAFETY: balances the successful init in engage().
        unsafe { (self.nvml.shutdown)() };
        remove_marker();
        tracing::info!(
            devices = self.pinned.len(),
            memory = self.mem_pinned.len(),
            "gpu-clock pin released"
        );
    }
}

fn marker_path() -> Option<PathBuf> {
    crate::logging::log_dir()
        .or_else(crate::logging::resolve_log_dir)
        .map(|d| d.join(MARKER))
}

fn write_marker() {
    if let Some(p) = marker_path() {
        let _ = std::fs::write(p, b"pinned\n");
    }
}

fn remove_marker() {
    if let Some(p) = marker_path() {
        let _ = std::fs::remove_file(p);
    }
}

struct State {
    sessions: usize,
    high_rate: usize,
    pin: Option<GpuClockPin>,
}

static STATE: Mutex<State> = Mutex::new(State {
    sessions: 0,
    high_rate: 0,
    pin: None,
});

/// Engage or release to match what is wanted now. A worker with a host relay
/// renews the host's lease instead of touching the clocks itself.
fn reevaluate(st: &mut State) {
    let mode = mode();
    let want = wanted(mode, st.sessions, st.high_rate);
    #[cfg(windows)]
    if relay::active() {
        relay::set_want(want);
        return;
    }
    if want {
        if st.pin.is_none() {
            st.pin = GpuClockPin::engage(mode);
        }
    } else {
        // Drop resets the clocks.
        st.pin = None;
    }
}

/// Session-count hook for the signaling loop: call after every mutation of
/// the remote-desktop peers map. In the every-session modes it engages the pin
/// on 0→N and releases it at 0; in the default high-rate mode it only records
/// the count. Cheap when nothing changes.
pub fn on_sessions_changed(active_sessions: usize) {
    let Ok(mut st) = STATE.lock() else { return };
    st.sessions = active_sessions;
    reevaluate(&mut st);
}

/// FR-119 P2b-16 — held by a pump while it runs an NVENC encoder above
/// [`HIGH_RATE_ABOVE_FPS`]; the pin stays wanted while any is held.
pub struct HighRateGuard(());

impl HighRateGuard {
    fn new() -> Self {
        if let Ok(mut st) = STATE.lock() {
            st.high_rate += 1;
            reevaluate(&mut st);
        }
        Self(())
    }
}

impl Drop for HighRateGuard {
    fn drop(&mut self) {
        if let Ok(mut st) = STATE.lock() {
            st.high_rate = st.high_rate.saturating_sub(1);
            reevaluate(&mut st);
        }
    }
}

/// The pump's one call: hold a guard while `want`, drop it when not.
pub fn hold_high_rate(slot: &mut Option<HighRateGuard>, want: bool) {
    match (want, slot.is_some()) {
        (true, false) => *slot = Some(HighRateGuard::new()),
        (false, true) => *slot = None,
        _ => {}
    }
}

/// FR-119 P2b-16 — the SCM host's half: pin the clocks for a worker's lease,
/// unless this host's own setting is off. The caller holds the returned pin
/// while the lease lives; dropping it resets the clocks.
#[cfg(windows)]
pub fn host_engage() -> Option<GpuClockPin> {
    let host_mode = mode();
    if host_mode == PinMode::Off {
        return None;
    }
    GpuClockPin::engage(match host_mode {
        PinMode::Explicit { .. } => host_mode,
        _ => PinMode::HighRate,
    })
}

/// The worker's half: remember the host's relay, once, at start.
#[cfg(windows)]
pub fn set_host_relay(name: String) {
    relay::set_name(name);
}

/// Boot-time sweep: a crashed or killed predecessor never ran `Drop`, so its
/// locked clocks survive it. If the predecessor left its marker (it held a
/// pin), or an every-session mode is set (the pre-P2b-16 opt-in), reset the
/// locks on every device once. Never touches a lock this daemon did not set.
pub fn reset_stale_pins() {
    let marker = marker_path().is_some_and(|p| p.exists());
    let every_session = matches!(mode(), PinMode::Auto | PinMode::Explicit { .. });
    if !marker && !every_session {
        return;
    }
    let Some(nvml) = Nvml::load() else { return };
    // SAFETY: entry point of the loaded library; balanced below.
    if unsafe { (nvml.init)() } != NVML_SUCCESS {
        return;
    }
    let mut reset = 0usize;
    for device in nvml.devices() {
        // SAFETY: valid handle; reset is idempotent (no-op if not locked).
        if unsafe { (nvml.reset_locked)(device) } == NVML_SUCCESS {
            reset += 1;
        }
        if let Some(reset_mem) = nvml.reset_mem_locked {
            // SAFETY: as above.
            let _ = unsafe { reset_mem(device) };
        }
    }
    // SAFETY: balances the init above.
    unsafe { (nvml.shutdown)() };
    remove_marker();
    if reset > 0 {
        tracing::info!(
            devices = reset,
            "gpu-clock boot sweep: cleared stale clock locks"
        );
    }
}

/// FR-119 P2b-16 — a worker's lease on the SCM host's relay: while anything
/// wants the pin, signal the host's event once a second; the host pins while
/// signals keep coming and releases when they stop.
#[cfg(windows)]
mod relay {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent};

    /// How often the lease is renewed; the host lets it lapse after 3 s.
    const RENEW_MS: u64 = 1_000;
    /// `EVENT_MODIFY_STATE`: enough to signal, all the relay's DACL grants a
    /// person.
    const EVENT_MODIFY_STATE: u32 = 0x0002;

    static NAME: OnceLock<String> = OnceLock::new();
    static WANT: AtomicBool = AtomicBool::new(false);
    static STARTED: std::sync::Once = std::sync::Once::new();

    pub(super) fn set_name(name: String) {
        let _ = NAME.set(name);
    }

    pub(super) fn active() -> bool {
        NAME.get().is_some()
    }

    pub(super) fn set_want(want: bool) {
        WANT.store(want, Ordering::Relaxed);
        if want {
            STARTED.call_once(|| {
                let Some(name) = NAME.get().cloned() else {
                    return;
                };
                let spawned = std::thread::Builder::new()
                    .name("roomlerd-gpu-pin-lease".into())
                    .spawn(move || renew(&name));
                if let Err(e) = spawned {
                    tracing::warn!(error = %e, "gpu-clock pin: no lease thread; clocks stay free");
                }
            });
        }
    }

    /// A handle that is only ever signaled, owned by the lease thread.
    struct Handle(HANDLE);

    fn open(name: &str) -> Option<Handle> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated; modify access only.
        let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, wide.as_ptr()) };
        (!h.is_null()).then_some(Handle(h))
    }

    fn renew(name: &str) {
        let Some(handle) = open(name) else {
            tracing::warn!(
                %name,
                "gpu-clock pin: the host's relay cannot be opened; clocks stay free"
            );
            return;
        };
        tracing::info!(%name, "gpu-clock pin: leasing from the service host");
        loop {
            if WANT.load(Ordering::Relaxed) {
                // SAFETY: a live event handle owned by this thread.
                unsafe { SetEvent(handle.0) };
            }
            std::thread::sleep(std::time::Duration::from_millis(RENEW_MS));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_forms_and_rejects_garbage() {
        // P2b-16 — unset or empty is the high-rate default.
        assert_eq!(parse_pin_mode(None), PinMode::HighRate);
        assert_eq!(parse_pin_mode(Some("")), PinMode::HighRate);
        assert_eq!(parse_pin_mode(Some("0")), PinMode::Off);
        assert_eq!(parse_pin_mode(Some("false")), PinMode::Off);
        assert_eq!(parse_pin_mode(Some("1")), PinMode::Auto);
        assert_eq!(parse_pin_mode(Some("true")), PinMode::Auto);
        assert_eq!(
            parse_pin_mode(Some("1500,3000")),
            PinMode::Explicit {
                min_mhz: 1500,
                max_mhz: 3000
            }
        );
        assert_eq!(
            parse_pin_mode(Some(" 1500 , 3000 ")),
            PinMode::Explicit {
                min_mhz: 1500,
                max_mhz: 3000
            }
        );
        // Inverted band, zero min, non-numeric → Off, never a surprise pin.
        assert_eq!(parse_pin_mode(Some("3000,1500")), PinMode::Off);
        assert_eq!(parse_pin_mode(Some("0,3000")), PinMode::Off);
        assert_eq!(parse_pin_mode(Some("banana")), PinMode::Off);
        assert_eq!(parse_pin_mode(Some("1500")), PinMode::Off);
    }

    #[test]
    fn band_is_seventy_percent_to_max_in_auto() {
        assert_eq!(pin_band(PinMode::Auto, 3000), Some((2100, 3000)));
        assert_eq!(pin_band(PinMode::HighRate, 3090), Some((2163, 3090)));
        assert_eq!(pin_band(PinMode::Auto, 0), None, "no max info → no pin");
        assert_eq!(
            pin_band(
                PinMode::Explicit {
                    min_mhz: 1500,
                    max_mhz: 2800
                },
                3000
            ),
            Some((1500, 2800)),
            "explicit band ignores the queried max"
        );
        assert_eq!(pin_band(PinMode::Off, 3000), None);
    }

    /// The memory floor is a supported clock: on neo16 the band that held
    /// NVENC at 4.3–4.6 ms.
    #[test]
    fn the_memory_band_is_the_supported_clocks_from_seventy_percent_up() {
        assert_eq!(
            memory_band(&[405, 810, 9001, 11001, 14001]),
            Some((9001, 14001))
        );
        assert_eq!(memory_band(&[14001, 405, 9001]), Some((9001, 14001)));
        // nothing at or under 70 % of the max: the max alone
        assert_eq!(memory_band(&[5000]), Some((5000, 5000)));
        assert_eq!(memory_band(&[]), None);
        assert_eq!(memory_band(&[0]), None);
    }

    /// The default wants the pin only for a high-rate NVENC session; the
    /// every-session modes as before; off never.
    #[test]
    fn what_wants_the_pin_in_each_mode() {
        assert!(!wanted(PinMode::HighRate, 3, 0), "sessions alone do not");
        assert!(wanted(PinMode::HighRate, 1, 1));
        assert!(wanted(PinMode::HighRate, 0, 1), "a guard is the claim");
        assert!(wanted(PinMode::Auto, 1, 0));
        assert!(!wanted(PinMode::Auto, 0, 0));
        assert!(wanted(
            PinMode::Explicit {
                min_mhz: 1,
                max_mhz: 2
            },
            1,
            0
        ));
        assert!(!wanted(PinMode::Off, 5, 5));
    }

    #[test]
    fn session_hook_is_inert_in_the_default_mode() {
        // Env is unset in CI → the high-rate default, where sessions alone
        // want nothing: no FFI runs, and the hook is safely callable from
        // the signaling loop regardless of hardware.
        on_sessions_changed(1);
        on_sessions_changed(2);
        on_sessions_changed(0);
        assert!(STATE.lock().unwrap().pin.is_none());
    }
}
