// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-119 P2b-16 — the SCM host's half of the GPU clock pin.
//!
//! Locking an NVIDIA GPU's clocks needs admin. The host is SYSTEM, but the
//! worker that encodes runs as the signed-in person (measured on neo16: the
//! attended worker is the person, session 1; only the host, session 0, is
//! SYSTEM), so its own NVML call is refused. So the host owns one named,
//! auto-reset event for its lifetime and hands its name to every worker it
//! spawns (`--gpu-pin-event <name>`). A worker that wants full clocks (an
//! NVENC session above 60 fps, `crate::gpu_clock`) signals it once a second:
//! a **lease**. This host's thread pins the clocks while signals keep coming
//! and releases them [`LEASE_MS`] after the last one, so a worker that dies,
//! hangs or stops wanting the pin costs at most that long at full clocks.
//!
//! ## Who may signal it
//!
//! [`GPU_PIN_RELAY_SDDL`] grants SYSTEM everything, and interactive users
//! `SYNCHRONIZE | EVENT_MODIFY_STATE` only: enough to signal, nothing more.
//! **A security decision**, unlike the SAS relay (SYSTEM only): the worker
//! that needs this runs as the person. What any process in an interactive
//! session can do with it is keep the GPU at full clocks while it keeps
//! signaling: power and heat, never a privilege, and the host's own
//! `ROOMLERD_GPU_CLOCK_PIN=0` still refuses it.
//!
//! Everything degrades to free clocks: no event or no thread, and the worker
//! is handed no relay; its own NVML call is then refused, and it logs why.

#![cfg(target_os = "windows")]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// SYSTEM gets `EVENT_ALL_ACCESS`; interactive users `SYNCHRONIZE |
/// EVENT_MODIFY_STATE`. **A security decision**: read the module docs before
/// changing it.
pub const GPU_PIN_RELAY_SDDL: &str = "D:P(A;;0x1f0003;;;SY)(A;;0x100002;;;IU)";

/// `Global\`, so a worker in the person's session opens what the host made in
/// session 0. Completed by the host's PID: one relay per host.
const GPU_PIN_RELAY_PREFIX: &str = "Global\\roomler-gpu-pin-";

/// How long the pin outlives the last renewal. The worker renews every second.
pub const LEASE_MS: u32 = 3_000;

/// `SDDL_REVISION_1`, the only defined SDDL revision.
const SDDL_REVISION_1: u32 = 1;

/// The relay's handle, owned by its thread for the host's lifetime.
struct RelayHandle(HANDLE);

// SAFETY: a Win32 `HANDLE` is a process-wide kernel-object reference with no
// thread affinity; the relay thread is its only user once it is moved there.
unsafe impl Send for RelayHandle {}

/// The `--gpu-pin-event <name>` argv for a worker spawn, starting the relay on
/// the first call. Empty when the relay could not be made: that worker's
/// clocks then stay free.
pub fn worker_args() -> Vec<String> {
    static RELAY: OnceLock<Option<String>> = OnceLock::new();
    match RELAY.get_or_init(start) {
        Some(name) => vec!["--gpu-pin-event".to_string(), name.clone()],
        None => Vec::new(),
    }
}

fn start() -> Option<String> {
    // A host that crashed holding a pin left its marker: undo that lock now,
    // before any worker can ask for a new one.
    crate::gpu_clock::reset_stale_pins();
    let name = format!("{GPU_PIN_RELAY_PREFIX}{}", std::process::id());
    let handle = match create(&name) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "gpu-pin relay: no event; the worker's clocks stay free");
            return None;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("roomlerd-gpu-pin-relay".into())
        .spawn(move || serve(handle));
    match spawned {
        Ok(_) => {
            tracing::info!(%name, "gpu-pin relay: ready for the worker's lease");
            Some(name)
        }
        Err(e) => {
            tracing::warn!(error = %e, "gpu-pin relay: no thread; the worker's clocks stay free");
            None
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The auto-reset event, unsignaled, with [`GPU_PIN_RELAY_SDDL`]. Refuses a
/// name that already exists: a stale relay could be someone else's.
fn create(name: &str) -> io::Result<RelayHandle> {
    let sddl = wide(GPU_PIN_RELAY_SDDL);
    let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; `psd` is a valid out-pointer; the size
    // out-pointer is documented optional.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut psd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd,
        bInheritHandle: 0,
    };
    let name_w = wide(name);
    // SAFETY: `sa` and its descriptor outlive the call; `name_w` is
    // NUL-terminated. Auto-reset (0), unsignaled (0).
    let handle = unsafe { CreateEventW(&sa, 0, 0, name_w.as_ptr()) };
    // SAFETY: a thread-local read, before LocalFree can reset it.
    let err = unsafe { GetLastError() };
    // SAFETY: `psd` came from the conversion above; the event copied it.
    let _ = unsafe { LocalFree(psd) };
    if handle.is_null() {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    if err == ERROR_ALREADY_EXISTS {
        // SAFETY: the handle CreateEventW just returned, closed once.
        unsafe { CloseHandle(handle) };
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} already existed"),
        ));
    }
    Ok(RelayHandle(handle))
}

/// Hold the pin while the lease is renewed; release it when it lapses.
fn serve(handle: RelayHandle) {
    let mut pin: Option<crate::gpu_clock::GpuClockPin> = None;
    loop {
        // SAFETY: a live event handle this thread owns; a bounded wait on the
        // relay thread, which does nothing else.
        match unsafe { WaitForSingleObject(handle.0, LEASE_MS) } {
            WAIT_OBJECT_0 => {
                if pin.is_none() {
                    pin = crate::gpu_clock::host_engage();
                    if pin.is_some() {
                        tracing::info!("gpu-pin relay: the worker's lease holds the clocks");
                    }
                }
            }
            WAIT_TIMEOUT => {
                if pin.take().is_some() {
                    tracing::info!("gpu-pin relay: the lease lapsed; clocks released");
                }
            }
            other => {
                tracing::warn!(
                    result = other,
                    "gpu-pin relay: the wait failed; no more clock pins from this host"
                );
                // Dropping the pin resets the clocks.
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SYSTEM gets all; interactive users only enough to signal, and nobody
    /// else anything. Changing this string is changing that decision.
    #[test]
    fn interactive_users_may_signal_and_nothing_more() {
        assert_eq!(
            GPU_PIN_RELAY_SDDL,
            "D:P(A;;0x1f0003;;;SY)(A;;0x100002;;;IU)"
        );
        assert!(!GPU_PIN_RELAY_SDDL.contains(";WD)"), "not everyone");
        assert!(
            !GPU_PIN_RELAY_SDDL.contains("0x1f0003;;;IU"),
            "not all access"
        );
    }

    /// The descriptor builds into a real event; a second relay under the same
    /// name is refused; and this test, which runs as a person and not as
    /// SYSTEM (as the SAS relay's test assumes too), cannot open it with full
    /// access. Where the test's token is interactive (a desktop, as a worker's
    /// always is) it can open it to signal; a CI runner's service logon carries
    /// no interactive SID, so there that half is not asserted.
    #[test]
    fn a_person_can_signal_the_relay_and_no_more() {
        use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent};
        let name = format!("{GPU_PIN_RELAY_PREFIX}test-{}", std::process::id());
        let first = create(&name).expect("the SDDL must build an event");
        assert!(matches!(
            create(&name).err().map(|e| e.kind()),
            Some(io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied)
        ));
        let name_w = wide(&name);
        // SAFETY: `name_w` is NUL-terminated; EVENT_MODIFY_STATE (0x2) is what
        // a worker asks for.
        let signal = unsafe { OpenEventW(0x0002, 0, name_w.as_ptr()) };
        if !signal.is_null() {
            // SAFETY: the handle just opened.
            assert_ne!(unsafe { SetEvent(signal) }, 0, "an opened relay signals");
            // SAFETY: the handle just opened, closed once.
            unsafe { CloseHandle(signal) };
        }
        // SAFETY: as above; EVENT_ALL_ACCESS (0x1f0003).
        let all = unsafe { OpenEventW(0x1f0003, 0, name_w.as_ptr()) };
        assert!(all.is_null(), "never with full access");
        // SAFETY: the relay's own handle, closed once.
        unsafe { CloseHandle(first.0) };
    }
}
