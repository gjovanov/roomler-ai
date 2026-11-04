// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-112 P1 and P3 — the encoder asks for its clock.
//!
//! A software encoder works a fraction of every frame period: at 30 fps it
//! encodes, then sleeps until the next frame. Linux's energy-aware scheduler
//! sees a thread like that as small. It places it on an efficiency core and
//! lets `schedutil` lower the clock until the work nearly fills its period.
//! Measured on an Apple M2 Pro (Fedora Asahi Remix, FR-112 §2): the same
//! OpenH264 frame takes 6 ms back to back and 17–20 ms at a 30 fps cadence, and
//! the live pump reported 16.1 ms. With the encoder's threads clamped
//! (`uclamp_min` 1024) the same frame took 5.3 ms live, and the VP9-444 pump at
//! a 60 fps target went from 19 to 56 fps with no other change.
//!
//! On Linux the clamp is this module's whole job. A thread asks for its own
//! clock with `sched_setattr(SCHED_FLAG_UTIL_CLAMP_MIN)`, keeping its policy
//! and priority. The scheduler then counts it as a big task: a performance core
//! and a high clock, **but only while it is runnable**, so an encoder waiting
//! for a still screen costs nothing more. Threads it creates afterwards inherit
//! the clamp: libvpx creates its worker threads at the first encode, from the
//! encoder thread, so they carry it too.
//!
//! Who asks: the encoder thread (`encode::thread`) and OpenH264's worker on
//! every OS, the DRM capture worker on Linux, the capture worker on macOS.
//! Nothing else of the daemon does.
//!
//! A kernel without util clamping (`CONFIG_UCLAMP_TASK` off) refuses the
//! request, and the encoder runs as before; the log says so once.
//!
//! macOS and Windows have a request of their own (P3, #2430). Neither is a
//! clamp, and neither is inherited: each covers only the thread that makes it.
//! That is the whole of an H.264 frame, on OpenH264's one worker, and none of
//! libvpx's workers. Neither has been measured yet, so each OS's request is
//! opt-in until its own hardware has answered ([`DEFAULT_ON`]):
//! - **macOS:** the thread's QoS class, `QOS_CLASS_USER_INTERACTIVE`.
//!   `pthread_create` gives a new thread `QOS_CLASS_DEFAULT` whatever its
//!   creator runs at (`sys/qos.h`).
//! - **Windows:** the thread leaves power throttling, Microsoft's "HighQoS"
//!   (`SetThreadInformation`, `ThreadPowerThrottling`). Microsoft documents the
//!   throttling of a background service as "on battery", so a measurement says
//!   little without its power source.
//!
//! The switch: `ROOMLERD_ENCODER_CLOCK=0` never asks, `=1` asks on macOS and
//! Windows too.

use std::sync::Mutex;

/// The clamp asked for: the full scale, "run me as fast as this CPU runs"
/// while the thread is runnable (FR-112 D5).
pub const CLAMP_MIN: u32 = 1024;

/// Whether a thread asks when `ROOMLERD_ENCODER_CLOCK` is unset: on where the
/// clock was measured. macOS and Windows flip this only on their own numbers
/// (#2430): a frame back to back against the same frame at a 30 fps cadence
/// (`encoder-smoke --bench`), with the request and without it.
pub const DEFAULT_ON: bool = cfg!(target_os = "linux");

/// What a thread's request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Linux: the thread now carries `uclamp_min = CLAMP_MIN`.
    Clamped,
    /// macOS: the thread's QoS class is now `QOS_CLASS_USER_INTERACTIVE`.
    QosClass,
    /// Windows: the thread is out of power throttling.
    Unthrottled,
    /// The switch is off; nothing was asked.
    Off,
    /// The OS refused: Linux's `errno` (no util clamping, or a cgroup limit),
    /// the errno macOS returns, or Windows' `GetLastError`.
    Refused(i32),
    /// No such request on this OS.
    Unsupported,
}

/// `ROOMLERD_ENCODER_CLOCK`, [`DEFAULT_ON`] when unset.
pub fn enabled() -> bool {
    tunnel_core::env::flag("ENCODER_CLOCK", DEFAULT_ON)
}

/// Ask for this thread's clock. Call it first thing on a thread that encodes
/// or captures, before it creates any thread of its own.
pub fn ask_for_this_thread(role: &'static str) -> Outcome {
    let outcome = ask(enabled());
    say_once(role, outcome);
    outcome
}

/// The request itself, with the switch passed in (tests decide it without
/// touching the environment, which other tests read concurrently).
fn ask(enabled: bool) -> Outcome {
    if enabled { imp::ask_os() } else { Outcome::Off }
}

/// One line per role per process: the first thread of a role says what it got,
/// and the rest (one per session) stay quiet.
fn say_once(role: &'static str, outcome: Outcome) {
    static SAID: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&role) {
        return;
    }
    said.push(role);
    match outcome {
        Outcome::Clamped => tracing::info!(
            role,
            uclamp_min = CLAMP_MIN,
            "encoder clock: asked for (FR-112; ROOMLERD_ENCODER_CLOCK=0 turns it off)"
        ),
        Outcome::QosClass => tracing::info!(
            role,
            "encoder clock: asked for, QOS_CLASS_USER_INTERACTIVE (FR-112 P3; ROOMLERD_ENCODER_CLOCK=0 turns it off)"
        ),
        Outcome::Unthrottled => tracing::info!(
            role,
            "encoder clock: asked for, out of power throttling (FR-112 P3; ROOMLERD_ENCODER_CLOCK=0 turns it off)"
        ),
        Outcome::Off => tracing::info!(
            role,
            default_on = DEFAULT_ON,
            "encoder clock: not asked for (ROOMLERD_ENCODER_CLOCK=0, or unset where it is off by default, FR-112)"
        ),
        Outcome::Refused(errno) => tracing::info!(
            role,
            errno,
            "encoder clock: the OS refused (Linux: no CONFIG_UCLAMP_TASK, or a cgroup limit); \
             the encoder runs at the clock the scheduler picks (FR-112)"
        ),
        Outcome::Unsupported => {}
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Outcome;

    /// `struct sched_attr`, `SCHED_ATTR_SIZE_VER1`: the first layout that
    /// carries the util clamps (linux/sched/types.h).
    #[repr(C)]
    #[derive(Debug, Default)]
    pub(super) struct SchedAttr {
        pub size: u32,
        pub sched_policy: u32,
        pub sched_flags: u64,
        pub sched_nice: i32,
        pub sched_priority: u32,
        pub sched_runtime: u64,
        pub sched_deadline: u64,
        pub sched_period: u64,
        pub sched_util_min: u32,
        pub sched_util_max: u32,
    }

    /// Keep the policy and its parameters: the request changes the clamp only.
    pub(super) const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
    pub(super) const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
    pub(super) const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;

    /// The request this module makes, built in one place so a test can pin it.
    pub(super) fn request(min: u32) -> SchedAttr {
        SchedAttr {
            size: std::mem::size_of::<SchedAttr>() as u32,
            sched_flags: SCHED_FLAG_KEEP_POLICY
                | SCHED_FLAG_KEEP_PARAMS
                | SCHED_FLAG_UTIL_CLAMP_MIN,
            sched_util_min: min,
            ..Default::default()
        }
    }

    pub(super) fn ask_os() -> Outcome {
        clamp_min(super::CLAMP_MIN)
    }

    fn clamp_min(min: u32) -> Outcome {
        let attr = request(min);
        // SAFETY: `attr` is a valid `sched_attr` whose `size` is its own; pid 0
        // is the calling thread; flags 0. The kernel only reads it.
        let r = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &attr as *const SchedAttr, 0) };
        if r == 0 {
            Outcome::Clamped
        } else {
            Outcome::Refused(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        }
    }

    /// The calling thread's `sched_util_min` as the kernel reports it.
    #[cfg(test)]
    pub(super) fn read_min() -> std::io::Result<u32> {
        let mut attr = SchedAttr::default();
        // SAFETY: `attr` is writable and `size` bytes long; pid 0 is the calling
        // thread; flags 0.
        let r = unsafe {
            libc::syscall(
                libc::SYS_sched_getattr,
                0,
                &mut attr as *mut SchedAttr,
                std::mem::size_of::<SchedAttr>() as u32,
                0,
            )
        };
        if r == 0 {
            Ok(attr.sched_util_min)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Outcome;

    /// The class asked for (#2430). Compared as an integer: libc's enum has no
    /// `PartialEq`, and it does not list every class the kernel can report.
    pub(super) const CLASS: libc::qos_class_t = libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE;

    pub(super) fn ask_os() -> Outcome {
        // SAFETY: plain values; the call acts on the calling thread only. It
        // returns an errno value itself, and refuses (`EPERM`) only a thread
        // that left the QoS system through `pthread_setschedparam`, which
        // nothing in the daemon calls.
        match unsafe { libc::pthread_set_qos_class_self_np(CLASS, 0) } {
            0 => Outcome::QosClass,
            errno => Outcome::Refused(errno),
        }
    }

    /// The calling thread's requested class, as `qos_class_t`'s integer: the
    /// kernel may write a value libc's enum does not name, which a Rust enum
    /// must never hold.
    #[cfg(test)]
    pub(super) fn read_class() -> std::io::Result<u32> {
        let mut class: u32 = 0;
        // SAFETY: `class` is a writable word of `qos_class_t`'s `repr(u32)`
        // size, read back only as an integer; the priority out-parameter is
        // documented nullable.
        let r = unsafe {
            libc::pthread_get_qos_class_np(
                libc::pthread_self(),
                (&raw mut class).cast(),
                std::ptr::null_mut(),
            )
        };
        if r == 0 {
            Ok(class)
        } else {
            Err(std::io::Error::from_raw_os_error(r))
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::Outcome;
    #[cfg(test)]
    use windows_sys::Win32::System::Threading::GetThreadInformation;
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadInformation, THREAD_POWER_THROTTLING_CURRENT_VERSION,
        THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
        ThreadPowerThrottling,
    };

    /// The request this module makes, built in one place so a test can pin it:
    /// `ControlMask` takes the mechanism out of the system's hands, and a zero
    /// `StateMask` turns it off. Microsoft's sample calls this "HighQoS";
    /// `ControlMask` 0 would hand the choice back to the system's heuristics.
    pub(super) fn request() -> THREAD_POWER_THROTTLING_STATE {
        THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        }
    }

    pub(super) fn ask_os() -> Outcome {
        let state = request();
        // SAFETY: `state` is a valid `THREAD_POWER_THROTTLING_STATE` and the
        // size passed is its own; the pseudo-handle names the calling thread
        // and needs no closing. The call only reads `state`.
        let ok = unsafe {
            SetThreadInformation(
                GetCurrentThread(),
                ThreadPowerThrottling,
                (&raw const state).cast(),
                std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok != 0 {
            Outcome::Unthrottled
        } else {
            Outcome::Refused(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        }
    }

    /// The calling thread's `(ControlMask, StateMask)`, or `None` where Windows
    /// refuses the read: `GetThreadInformation`'s documentation does not list
    /// this class, though Windows 11 answers it.
    #[cfg(test)]
    pub(super) fn read_masks() -> Option<(u32, u32)> {
        let mut state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: 0,
            StateMask: 0,
        };
        // SAFETY: `state` is writable and the size passed is its own; the
        // pseudo-handle names the calling thread and needs no closing.
        let ok = unsafe {
            GetThreadInformation(
                GetCurrentThread(),
                ThreadPowerThrottling,
                (&raw mut state).cast(),
                std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            )
        };
        (ok != 0).then_some((state.ControlMask, state.StateMask))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    pub(super) fn ask_os() -> super::Outcome {
        super::Outcome::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kill switch asks for nothing.
    #[test]
    fn the_kill_switch_asks_for_nothing() {
        assert_eq!(ask(false), Outcome::Off);
    }

    /// The request is a VER1 `sched_attr` (56 bytes, the first size that carries
    /// the clamps) that changes the clamp and nothing else.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_request_changes_the_clamp_and_nothing_else() {
        let attr = imp::request(CLAMP_MIN);
        assert_eq!(attr.size, 56, "SCHED_ATTR_SIZE_VER1");
        assert_eq!(
            attr.sched_flags,
            imp::SCHED_FLAG_KEEP_POLICY
                | imp::SCHED_FLAG_KEEP_PARAMS
                | imp::SCHED_FLAG_UTIL_CLAMP_MIN,
            "policy and priority kept, only the minimum clamp set"
        );
        assert_eq!(attr.sched_util_min, 1024);
        assert_eq!(
            (attr.sched_policy, attr.sched_priority, attr.sched_nice),
            (0, 0, 0)
        );
    }

    /// On a kernel with util clamping the thread reads its clamp back, and a
    /// thread it then creates inherits it (libvpx's workers rely on that). A
    /// kernel without it refuses, which is the documented fallback.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_thread_reads_its_clamp_back_and_its_children_inherit_it() {
        std::thread::spawn(|| match ask(true) {
            Outcome::Clamped => {
                assert_eq!(imp::read_min().expect("sched_getattr"), CLAMP_MIN);
                let child = std::thread::spawn(|| imp::read_min().expect("sched_getattr"))
                    .join()
                    .expect("child thread");
                assert_eq!(child, CLAMP_MIN, "a thread created after the request inherits it");
            }
            Outcome::Refused(errno) => {
                eprintln!("this kernel has no util clamping (errno {errno}); the fallback is to run as before")
            }
            other => panic!("unexpected outcome on Linux: {other:?}"),
        })
        .join()
        .expect("test thread");
    }

    /// macOS: a thread reads its class back, and a thread it then creates does
    /// NOT have it, the opposite of Linux's clamp: `pthread_create` starts a
    /// thread at the default class (`sys/qos.h`). libvpx's workers are outside
    /// this request on a Mac, and this test keeps that claim honest.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_thread_reads_its_class_back_and_its_children_do_not_inherit_it() {
        std::thread::spawn(|| {
            assert_eq!(ask(true), Outcome::QosClass);
            let class = imp::CLASS as u32;
            let own = imp::read_class().expect("pthread_get_qos_class_np");
            assert_eq!(own, class, "the thread reads the class it asked for");
            let child = std::thread::spawn(imp::read_class)
                .join()
                .expect("child thread")
                .expect("pthread_get_qos_class_np");
            assert_ne!(child, class, "a QoS class is not inherited");
        })
        .join()
        .expect("test thread");
    }

    /// Windows: the request is Microsoft's "HighQoS", the mechanism under our
    /// control and turned off. A set bit in `StateMask` would be EcoQoS, the
    /// very thing this leaves, and it compiles and runs just the same.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_request_leaves_power_throttling_and_never_enters_it() {
        let state = imp::request();
        assert_eq!(std::mem::size_of_val(&state), 12);
        assert_eq!(state.Version, 1, "THREAD_POWER_THROTTLING_CURRENT_VERSION");
        assert_eq!(
            state.ControlMask, 1,
            "THREAD_POWER_THROTTLING_EXECUTION_SPEED"
        );
        assert_eq!(state.StateMask, 0, "a set bit here is EcoQoS");
    }

    /// Windows: the kernel takes the request, the thread reads it back, and a
    /// thread it then creates starts without it, so libvpx's workers are outside
    /// this request here too. Microsoft documents neither the read nor the
    /// inheritance; this test is the record of what Windows does.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_thread_leaves_power_throttling_and_its_children_do_not_inherit_it() {
        std::thread::spawn(|| {
            assert_eq!(ask(true), Outcome::Unthrottled);
            let Some(own) = imp::read_masks() else {
                eprintln!("this Windows refuses the read; the request itself was taken");
                return;
            };
            assert_eq!(own, (1, 0), "the mechanism is ours, and it is off");
            let child = std::thread::spawn(imp::read_masks)
                .join()
                .expect("child thread");
            assert_eq!(child, Some((0, 0)), "the system decides for a new thread");
        })
        .join()
        .expect("test thread");
    }
}
