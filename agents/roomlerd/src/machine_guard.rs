// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4280 — one daemon per machine, held before anything host-global.
//!
//! What a `roomlerd run` touches once it is the daemon belongs to the machine,
//! not to its config: the overlay's network interface and its routes, the
//! LocalAPI pipe, the exit, DNS and firewall state, and the server connection
//! under the device's ids (`docs/multi-org.md`). The single-instance lock
//! (`instance_lock`) is the config's, so it guards none of them. This guard is
//! the machine's, keyed by the overlay interface's name ([`KEY`]), and `run`
//! takes it before that lock and before anything host-global
//! (`app::start_gate`). For the Windows service's own worker running as
//! SYSTEM, which holds it, the guard alone decides that the worker runs; that
//! worker still takes the lock, so every other start finds it as before
//! ([`relaxes_config_lock`], #4403).
//!
//! Only a privileged daemon (SYSTEM or an elevated administrator on Windows,
//! root on Linux and macOS) can hold the machine's guard; an unprivileged start
//! can check for it but never creates it ([`action`]).
//!
//! - **Windows**: a mutex in a private namespace whose boundary is named by
//!   the key and holds BUILTIN\Administrators at High integrity ([`win`]). Only
//!   SYSTEM and Administrators, at High integrity, can take the guard or
//!   change it; any other signed-in account can only read whether it is held,
//!   which is what an unprivileged run does. A thread of its holder that never
//!   ends owns it, and the kernel gives it up when that owner ends, a crash
//!   included, so it is held only while its owner lives and a holder that
//!   ends leaves it free. The boundary names no session, so the guard is the
//!   machine's, not a session's.
//! - **Linux and macOS**: `flock` on a 0600 file in the root daemon's own
//!   folder, `/var/run/roomler`, where the root LocalAPI socket lives
//!   ([`unix`]). The kernel drops the lock when the process ends. A non-root
//!   run takes no guard: it cannot touch the interface, its routes, DNS or the
//!   packet filter, and on a Mac the screen worker is a second run on purpose
//!   (`macos_supervisor`).
//!
//! The guard is held until the process exits and is never dropped. `main`
//! purges the exit routes after the daemon returns (`purge_after_daemon`), and
//! a successor must not build its routes while that purge still runs. That is
//! also why a thread of its own owns it on Windows: `main` shuts the runtime
//! that asked for it down before that purge.
//!
//! A supervised start that finds the guard held waits for it a bounded while
//! ([`take_waiting`]): its holder is most likely its own predecessor, still
//! exiting. An unsupervised start leaves at once. Every start that cannot read
//! the guard runs without it and says so at WARN ([`UNAVAILABLE`]): the one
//! wrong answer this daemon cannot afford is a machine left with no daemon.

use crate::supervision::Supervision;
use std::future::Future;
use std::time::{Duration, Instant};

/// The key: the overlay interface's name, `tunnel_core::overlay::tun::IF_NAME`
/// (held to it by a test in the overlay builds). Here as well so a build with
/// no overlay module keys the same guard. On macOS the interface's real name is
/// the kernel's (`utun<n>`), so this is only a stable key there.
#[cfg(windows)]
pub const KEY: &str = "roomler";
#[cfg(target_os = "linux")]
pub const KEY: &str = "roomler0";
#[cfg(not(any(windows, target_os = "linux")))]
pub const KEY: &str = "utun";

/// How long a supervised start waits for a guard that is held. The Windows
/// service host waits 1.5 s for a worker it ended (`TERMINATE_WAIT`), and the
/// worker it starts next may find its predecessor still exiting.
pub const WAIT_BOUND: Duration = Duration::from_secs(30);

/// How often the waiting start tries again.
pub const WAIT_STEP: Duration = Duration::from_millis(250);

/// The words of every start that runs without the guard because it could not
/// take or read it, logged at WARN with the error codes, for a field read to
/// grep.
pub const UNAVAILABLE: &str = "machine guard unavailable; running without it";

/// What a run's token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// euid 0 (Linux, macOS).
    Root,
    /// Windows: BUILTIN\Administrators enabled at System integrity (the service
    /// host's SYSTEM worker).
    System,
    /// Windows: BUILTIN\Administrators enabled at High integrity.
    Elevated,
    /// Everything else: a standard or a UAC-filtered token, LocalService, a
    /// non-root user.
    Unprivileged,
    /// Windows: the token could not be read.
    Unknown,
}

/// The two families the predicate tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Unix,
}

/// This build's family.
pub const PLATFORM: Platform = if cfg!(windows) {
    Platform::Windows
} else {
    Platform::Unix
};

/// What a run does with the machine's guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Take it: create it, or find it held.
    Take,
    /// Only look whether a privileged daemon holds it, and never create it.
    CheckOnly,
    /// Neither: this run cannot touch anything the guard protects.
    Skip,
}

/// The predicate: only a privileged run takes the guard. An unprivileged run
/// checks it on Windows, where it still connects under the device's ids, and
/// skips it off Windows, where a second non-root run is a separate device or,
/// on a Mac, the screen worker the root daemon starts.
pub fn action(platform: Platform, privilege: Privilege) -> Action {
    match platform {
        Platform::Windows => match privilege {
            Privilege::System | Privilege::Elevated | Privilege::Root => Action::Take,
            // A token that could not be read: the boundary decides. The kernel
            // refuses a create from outside it, and the take then only reads
            // the guard, as the check does (`win::take`), so this lets no
            // unprivileged process create anything.
            Privilege::Unknown => Action::Take,
            Privilege::Unprivileged => Action::CheckOnly,
        },
        Platform::Unix => match privilege {
            Privilege::Root => Action::Take,
            Privilege::System
            | Privilege::Elevated
            | Privilege::Unprivileged
            | Privilege::Unknown => Action::Skip,
        },
    }
}

/// What a Windows token says: whether BUILTIN\Administrators is ENABLED in it
/// (`win_token::admin_group_enabled`) and its integrity RID
/// (`win_token::own_integrity_rid`). Pure, so every platform tests it.
pub fn privilege_of(admin_enabled: Option<bool>, integrity_rid: Option<u32>) -> Privilege {
    const HIGH: u32 = 0x3000;
    const SYSTEM: u32 = 0x4000;
    match (admin_enabled, integrity_rid) {
        (Some(true), Some(rid)) if rid >= SYSTEM => Privilege::System,
        (Some(true), Some(rid)) if rid >= HIGH => Privilege::Elevated,
        (Some(_), Some(_)) => Privilege::Unprivileged,
        _ => Privilege::Unknown,
    }
}

/// This process's [`Privilege`].
pub fn privilege_here() -> Privilege {
    #[cfg(windows)]
    {
        privilege_of(
            crate::win_token::admin_group_enabled(),
            crate::win_token::own_integrity_rid(),
        )
    }
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` reads this process's credentials and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            Privilege::Root
        } else {
            Privilege::Unprivileged
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        Privilege::Unknown
    }
}

/// Whether a start under `supervision` waits for a held guard ([`take_waiting`]):
/// a supervised one, whose holder is most likely its own predecessor, still
/// exiting. A start nothing supervises leaves at once.
pub fn waits(supervision: Supervision) -> bool {
    match supervision {
        Supervision::WindowsScm
        | Supervision::WindowsTask
        | Supervision::Systemd(_)
        | Supervision::Launchd
        | Supervision::MacosSupervisor => true,
        Supervision::None => false,
    }
}

/// The machine's guard, held. Production parks it until the process exits
/// ([`hold_here`]), and the OS releases it then.
pub struct Guard {
    #[cfg(windows)]
    _owner: win::Owner,
    #[cfg(unix)]
    _file: std::fs::File,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Guard")
    }
}

impl Guard {
    /// Test only: end this guard the way the end of its process does: the file
    /// closes, and the kernel drops the lock with it. (On Windows a dropped
    /// guard's owner thread ends without releasing the mutex, which gives it
    /// up, as the end of the process does.)
    #[cfg(all(test, unix))]
    pub(crate) fn end_as_process_exit(self) {
        drop(self);
    }

    /// Test only: whether the take found the guard given up by a holder that
    /// had ended.
    #[cfg(all(test, windows))]
    pub(crate) fn taken_after_an_end(&self) -> bool {
        self._owner.abandoned
    }

    /// Test only: a guard that holds nothing, for the wait's tests, whose
    /// takes are stand-ins.
    #[cfg(test)]
    pub(crate) fn stand_in() -> Self {
        Guard {
            #[cfg(windows)]
            _owner: win::Owner::none(),
            #[cfg(unix)]
            _file: tempfile::tempfile().expect("a temporary file"),
        }
    }
}

/// What an attempt at the guard found.
#[derive(Debug)]
pub enum Claim {
    /// This process holds it.
    Taken(Guard),
    /// Another privileged daemon of this machine holds it: this run leaves.
    Held,
    /// No privileged daemon holds it, and this run (an unprivileged check)
    /// takes nothing.
    Free,
    /// This run takes no guard ([`Action::Skip`]).
    Skipped,
    /// It could not be taken or read; the reason carries the error codes. The
    /// run goes on without it ([`UNAVAILABLE`]).
    Unavailable(String),
}

impl Claim {
    fn is_held(&self) -> bool {
        matches!(self, Claim::Held)
    }
}

/// A take that waits, when `wait`, for a guard that is held: `try_take` again
/// every [`WAIT_STEP`] for at most [`WAIT_BOUND`], with one line when the wait
/// starts and one when it ends. Any answer but [`Claim::Held`] ends it at once.
/// `sleep` and `now` are the clock, given so a test can drive it.
pub async fn take_waiting<F: Future<Output = ()>>(
    mut try_take: impl FnMut() -> Claim,
    wait: bool,
    mut sleep: impl FnMut(Duration) -> F,
    mut now: impl FnMut() -> Instant,
) -> Claim {
    let first = try_take();
    if !wait || !first.is_held() {
        return first;
    }
    let started = now();
    tracing::info!(
        bound_s = WAIT_BOUND.as_secs(),
        "machine guard: another daemon of this machine holds it; this supervised start waits \
         for it, as for a predecessor that is still exiting"
    );
    loop {
        sleep(WAIT_STEP).await;
        let claim = try_take();
        let waited = now().saturating_duration_since(started);
        if !claim.is_held() || waited >= WAIT_BOUND {
            tracing::info!(
                waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                still_held = claim.is_held(),
                "machine guard: the wait ended"
            );
            return claim;
        }
    }
}

/// What [`hold_here`] did with the machine's guard, for `run`'s gate
/// (`app::start_gate`) to read. A guard that another daemon holds is
/// `hold_here`'s `Err` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// This process holds it, taken under this privilege.
    Taken(Privilege),
    /// An unprivileged check found that no privileged daemon holds it.
    Free,
    /// This run takes no guard ([`Action::Skip`]).
    Skipped,
    /// It could not be taken or read, and this run goes on without it
    /// ([`UNAVAILABLE`]).
    Unavailable,
}

/// #4403 — the predicate: whether the machine's guard, rather than this config's
/// single-instance lock, decides that this run goes on. True only for the
/// Windows service's own worker running as SYSTEM that holds the guard: the
/// guard already makes it the only privileged daemon of the machine. That
/// worker still takes the lock whenever it can and holds it for its life
/// (`instance_lock::acquire_beside_guard`), so every other start finds it as
/// before. Every other run keeps the lock exactly as it was, Linux and macOS
/// included: an elevated administrator, a run whose guard was unavailable, an
/// unprivileged run, and a run under any other supervisor.
pub fn relaxes_config_lock(platform: Platform, supervision: Supervision, hold: Hold) -> bool {
    matches!(
        (platform, supervision, hold),
        (
            Platform::Windows,
            Supervision::WindowsScm,
            Hold::Taken(Privilege::System)
        )
    )
}

/// The guard this process holds, until it exits, and the privilege it was
/// taken under.
static HELD: std::sync::OnceLock<(Guard, Privilege)> = std::sync::OnceLock::new();

/// `run`'s guard, before anything host-global (#4280): take it, check it or
/// skip it, as [`action`] says for this process's token, waiting as
/// [`waits`] says for `supervision`. `Err` = another daemon of this machine
/// holds it, and this run leaves (`daemon_error::MachineHeld`, the
/// single-instance lock's exit code). Every other answer goes on and says what
/// it did ([`Hold`]): a guard taken stays held until the process exits.
pub async fn hold_here(supervision: Supervision) -> Result<Hold, crate::daemon_error::MachineHeld> {
    if let Some((_, privilege)) = HELD.get() {
        return Ok(Hold::Taken(*privilege));
    }
    let privilege = privilege_here();
    let claim = match action(PLATFORM, privilege) {
        Action::Skip => Claim::Skipped,
        Action::CheckOnly => check_here(),
        Action::Take => {
            take_waiting(
                take_here,
                waits(supervision),
                tokio::time::sleep,
                Instant::now,
            )
            .await
        }
    };
    settle(claim, privilege)
}

/// What `run` does with a claim, and its one log line.
fn settle(claim: Claim, privilege: Privilege) -> Result<Hold, crate::daemon_error::MachineHeld> {
    match claim {
        Claim::Taken(guard) => {
            if let Err((guard, _)) = HELD.set((guard, privilege)) {
                // Never: `hold_here` returns first when one is held. Kept, not
                // dropped, so nothing here ever releases a guard.
                std::mem::forget(guard);
            }
            tracing::info!(
                key = KEY,
                ?privilege,
                "machine guard taken: this process is the daemon of this machine until it exits"
            );
            Ok(Hold::Taken(privilege))
        }
        Claim::Held => {
            tracing::warn!(
                key = KEY,
                ?privilege,
                "machine guard held by another daemon of this machine; this run leaves before \
                 anything host-global"
            );
            Err(crate::daemon_error::MachineHeld::here())
        }
        Claim::Free => {
            tracing::info!(
                key = KEY,
                ?privilege,
                "machine guard: no privileged daemon holds this machine; this unprivileged run \
                 checked it and takes nothing"
            );
            Ok(Hold::Free)
        }
        Claim::Skipped => {
            tracing::info!(
                key = KEY,
                ?privilege,
                "machine guard: not taken by a non-root run, which cannot touch the overlay \
                 interface, its routes, DNS or the packet filter"
            );
            Ok(Hold::Skipped)
        }
        Claim::Unavailable(why) => {
            tracing::warn!(key = KEY, ?privilege, %why, "{}", UNAVAILABLE);
            Ok(Hold::Unavailable)
        }
    }
}

/// [`Action::Take`] with the production key.
fn take_here() -> Claim {
    #[cfg(windows)]
    {
        win::take(&win::name_for(KEY))
    }
    #[cfg(unix)]
    {
        unix::take_here()
    }
    #[cfg(not(any(windows, unix)))]
    {
        Claim::Skipped
    }
}

/// [`Action::CheckOnly`] with the production key. Asked on Windows only: off
/// Windows an unprivileged run skips the guard.
fn check_here() -> Claim {
    #[cfg(windows)]
    {
        win::check(&win::name_for(KEY))
    }
    #[cfg(not(windows))]
    {
        Claim::Skipped
    }
}

/// Windows: a mutex in a private namespace (Vista+; the integrity label,
/// Windows 7+).
///
/// - **Keyed.** The namespace's boundary descriptor is named by the key
///   ([`name_for`]) and holds BUILTIN\Administrators plus the High integrity
///   label. A namespace is found by its boundary, so one function builds it
///   for the take and the check, and every test uses a name of its own. A
///   process must be inside the boundary to create the namespace, and creates
///   one only at or below its own integrity level, so only SYSTEM and an
///   elevated administrator can create it. The alias is the same string; an
///   alias belongs to the process that opened it and keys nothing.
/// - **Owned.** The guard is the mutex `<name>\Guard` in that namespace, owned
///   by a thread of its holder that never ends ([`take`]). The kernel gives a
///   mutex up when its owner ends, a crash included, so the guard is held only
///   while its owner lives, and a holder that ends leaves it free. The
///   namespace itself lives until its last handle closes, so its being there
///   says nothing about a holder: only the mutex's owner does.
/// - **Fenced** ([`NAMESPACE_SDDL`], [`MUTEX_SDDL`]). Only SYSTEM and
///   Administrators, at High integrity, can take the guard or change it. Any
///   other signed-in account can only read whether it is held ([`check`]).
/// - The boundary names no session, so the guard is the machine's, not a
///   session's.
#[cfg(windows)]
pub mod win {
    use super::{Claim, Guard};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, WinBuiltinAdministratorsSid, WinHighLabelSid,
    };
    use windows_sys::Win32::System::Threading::{
        AddIntegrityLabelToBoundaryDescriptor, AddSIDToBoundaryDescriptor, ClosePrivateNamespace,
        CreateBoundaryDescriptorW, CreateMutexW, CreatePrivateNamespaceW, DeleteBoundaryDescriptor,
        OpenMutexW, OpenPrivateNamespaceW, WaitForSingleObject,
    };

    /// The namespace's security descriptor. Administrators own it; SYSTEM and
    /// Administrators may do anything in it; any other signed-in account
    /// (Authenticated Users) may only pass through it to the guard
    /// (DIRECTORY_TRAVERSE, 0x2). The High label refuses a create from below
    /// High integrity (no-write-up), a second fence beside the DACL. It sets no
    /// read-up or execute-up, because the directory type maps traverse into
    /// both.
    pub const NAMESPACE_SDDL: &str = "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x2;;;AU)S:(ML;;NW;;;HI)";

    /// The guard's security descriptor: as the namespace's, and any other
    /// signed-in account may only read whether it is held (MUTANT_QUERY_STATE,
    /// 0x1). The label also refuses a wait (SYNCHRONIZE, in the execute
    /// mapping) from below High integrity, the second fence.
    pub const MUTEX_SDDL: &str = "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x1;;;AU)S:(ML;;NWNX;;;HI)";

    /// The one right the check asks for: read whether the mutex is held. Its NT
    /// name; `windows-sys` calls the same bit MUTEX_MODIFY_STATE.
    pub const MUTANT_QUERY_STATE: u32 = 0x0001;

    /// The guard's name inside the namespace.
    const GUARD: &str = "Guard";

    /// The boundary's name, which is the namespace's alias too, for `key`.
    pub fn name_for(key: &str) -> String {
        format!("Roomler-Daemon-{key}")
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn guard_name(name: &str) -> Vec<u16> {
        wide(&format!("{name}\\{GUARD}"))
    }

    fn last_error() -> u32 {
        // SAFETY: a thread-local read.
        unsafe { GetLastError() }
    }

    fn not_found(code: u32) -> bool {
        matches!(code, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
    }

    /// A handle to the namespace, closed on drop, never destroyed: the
    /// namespace ends with its last handle, as at the end of a process.
    struct Namespace(HANDLE);

    impl Drop for Namespace {
        fn drop(&mut self) {
            // SAFETY: the live handle this wrapper owns; closed once.
            unsafe { ClosePrivateNamespace(self.0, 0) };
        }
    }

    /// A handle to the guard, closed on drop. Closing a handle never releases
    /// the guard: only its owner's end gives it up.
    struct Mutex(HANDLE);

    impl Drop for Mutex {
        fn drop(&mut self) {
            // SAFETY: the live handle this wrapper owns; closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A boundary descriptor, deleted on drop.
    struct Boundary(HANDLE);

    impl Drop for Boundary {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the descriptor `boundary` built (the Add calls
            // write back where it moved); deleted once.
            unsafe { DeleteBoundaryDescriptor(self.0) }
        }
    }

    /// The boundary named `name`, the same for the take and the check.
    fn boundary(name: &str) -> Result<Boundary, String> {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated UTF-16 and outlives the call.
        let raw = unsafe { CreateBoundaryDescriptorW(name.as_ptr(), 0) };
        if raw.is_null() {
            return Err(format!("CreateBoundaryDescriptorW error {}", last_error()));
        }
        let mut bd = Boundary(raw);
        let mut admins = crate::win_token::well_known_sid(WinBuiltinAdministratorsSid)?;
        let mut high = crate::win_token::well_known_sid(WinHighLabelSid)?;
        // SAFETY: `bd.0` is a live descriptor, which the call may move and then
        // writes back; `admins` holds a SID and outlives the call.
        if unsafe { AddSIDToBoundaryDescriptor(&mut bd.0, admins.as_mut_ptr().cast()) } == 0 {
            return Err(format!("AddSIDToBoundaryDescriptor error {}", last_error()));
        }
        // SAFETY: as above, with the High integrity label's SID.
        if unsafe { AddIntegrityLabelToBoundaryDescriptor(&mut bd.0, high.as_mut_ptr().cast()) }
            == 0
        {
            return Err(format!(
                "AddIntegrityLabelToBoundaryDescriptor error {}",
                last_error()
            ));
        }
        Ok(bd)
    }

    /// A security descriptor made from SDDL, freed on drop.
    struct LocalSd(PSECURITY_DESCRIPTOR);

    impl Drop for LocalSd {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptor…W; freed once.
            unsafe { LocalFree(self.0) };
        }
    }

    fn descriptor(sddl: &str) -> Result<LocalSd, String> {
        let sddl = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated UTF-16; `sd` is a valid out-pointer;
        // the size out-pointer is optional.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW error {}",
                last_error()
            ));
        }
        Ok(LocalSd(sd))
    }

    fn attributes(sd: &LocalSd) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            // Never inherited: a child this daemon starts must not hold a
            // handle to the guard.
            bInheritHandle: 0,
        }
    }

    fn create_namespace(sd: &LocalSd, bd: &Boundary, alias: &[u16]) -> Result<Namespace, u32> {
        let sa = attributes(sd);
        // SAFETY: `sa`, the descriptors and `alias` (NUL-terminated) outlive the
        // call; a handle it returns is owned by the `Namespace`.
        let handle = unsafe { CreatePrivateNamespaceW(&sa, bd.0.cast_const(), alias.as_ptr()) };
        if handle.is_null() {
            Err(last_error())
        } else {
            Ok(Namespace(handle))
        }
    }

    fn open_namespace(bd: &Boundary, alias: &[u16]) -> Result<Namespace, u32> {
        // SAFETY: the descriptor and `alias` (NUL-terminated) outlive the call.
        let handle = unsafe { OpenPrivateNamespaceW(bd.0.cast_const(), alias.as_ptr()) };
        if handle.is_null() {
            Err(last_error())
        } else {
            Ok(Namespace(handle))
        }
    }

    /// Opens the guard, or creates it with `sd`. It never makes this thread
    /// the owner (`bInitialOwner` FALSE): a mutex that exists ignores that
    /// flag, so only the wait decides who holds it.
    fn create_mutex(sd: &LocalSd, name: &[u16]) -> Result<Mutex, u32> {
        let sa = attributes(sd);
        // SAFETY: `sa`, its descriptor and `name` (NUL-terminated) outlive the
        // call; a handle it returns is owned by the `Mutex`.
        let handle = unsafe { CreateMutexW(&sa, 0, name.as_ptr()) };
        if handle.is_null() {
            Err(last_error())
        } else {
            Ok(Mutex(handle))
        }
    }

    fn open_mutex(access: u32, name: &[u16]) -> Result<Mutex, u32> {
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let handle = unsafe { OpenMutexW(access, 0, name.as_ptr()) };
        if handle.is_null() {
            Err(last_error())
        } else {
            Ok(Mutex(handle))
        }
    }

    /// The layout ntdll writes for class 0 (MutantBasicInformation). Only the
    /// count is read.
    #[repr(C)]
    #[allow(dead_code)]
    struct MutantBasicInformation {
        current_count: i32,
        owned_by_caller: u8,
        abandoned_state: u8,
    }

    // ntdll's read of a mutex's state, which needs MUTANT_QUERY_STATE and
    // nothing else. No feature of `windows-sys` or `windows` declares it.
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtQueryMutant(
            handle: HANDLE,
            class: u32,
            information: *mut core::ffi::c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }

    /// The guard's count: 0 or below while a thread owns it, above 0 while it
    /// is free (given up by a holder that ended included). `Err` = the status.
    fn count(mutex: &Mutex) -> Result<i32, i32> {
        let mut info = MutantBasicInformation {
            current_count: 0,
            owned_by_caller: 0,
            abandoned_state: 0,
        };
        let mut returned = 0u32;
        // SAFETY: a live handle; `info` is class 0's layout and outlives the
        // call, which writes at most `length` bytes into it.
        let status = unsafe {
            NtQueryMutant(
                mutex.0,
                0,
                (&mut info as *mut MutantBasicInformation).cast(),
                std::mem::size_of::<MutantBasicInformation>() as u32,
                &mut returned,
            )
        };
        if status < 0 {
            Err(status)
        } else {
            Ok(info.current_count)
        }
    }

    /// The thread that owns the guard. It took the mutex, holds both handles,
    /// and waits for a word that production never sends (`super::HELD` keeps
    /// this for the life of the process), so the end of the process ends it.
    /// Dropping this ends the thread without releasing the mutex, which is
    /// what the end of the process does.
    pub(super) struct Owner {
        stop: Option<std::sync::mpsc::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
        /// The take found the guard given up by a holder that had ended
        /// (`WAIT_ABANDONED`).
        #[cfg(test)]
        pub(super) abandoned: bool,
    }

    impl Owner {
        fn new(
            stop: std::sync::mpsc::Sender<()>,
            thread: std::thread::JoinHandle<()>,
            abandoned: bool,
        ) -> Self {
            #[cfg(not(test))]
            let _ = abandoned;
            Owner {
                stop: Some(stop),
                thread: Some(thread),
                #[cfg(test)]
                abandoned,
            }
        }

        /// An owner with no thread, for `Guard::stand_in`.
        #[cfg(test)]
        pub(super) fn none() -> Self {
            Owner {
                stop: None,
                thread: None,
                abandoned: false,
            }
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            drop(self.stop.take());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// What the owner thread answers.
    enum Took {
        /// It owns the guard; `abandoned` = its last holder had ended.
        Taken { abandoned: bool },
        /// Anything else, for the caller to return.
        Not(Claim),
    }

    /// [`super::Action::Take`] at `name`, on a thread of its own. A mutex is
    /// owned by a thread, and the guard must stay held until the process ends,
    /// while the thread that asks may end long before: `main` shuts its runtime
    /// down before it purges the exit routes. So a take that takes it leaves
    /// that thread holding it; any other answer ends that thread at once.
    pub fn take(name: &str) -> Claim {
        let (said, answer) = std::sync::mpsc::sync_channel(1);
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let owned = name.to_owned();
        let spawned = std::thread::Builder::new()
            .name("machine-guard".into())
            .spawn(move || own(&owned, &said, &stopped));
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => return Claim::Unavailable(format!("starting the guard's thread: {e}")),
        };
        match answer.recv() {
            Ok(Took::Taken { abandoned }) => Claim::Taken(Guard {
                _owner: Owner::new(stop, thread, abandoned),
            }),
            Ok(Took::Not(claim)) => {
                let _ = thread.join();
                claim
            }
            Err(_) => {
                let _ = thread.join();
                Claim::Unavailable("the guard's thread ended without an answer".to_string())
            }
        }
    }

    /// The owner thread: one try. When it takes the guard it keeps both handles
    /// and waits, and only the end of the process ends it (or, in a test, the
    /// [`Owner`]'s drop).
    fn own(
        name: &str,
        said: &std::sync::mpsc::SyncSender<Took>,
        stopped: &std::sync::mpsc::Receiver<()>,
    ) {
        match attempt(name) {
            Ok((_namespace, _guard, abandoned)) => {
                let _ = said.send(Took::Taken { abandoned });
                let _ = stopped.recv();
                // Never released: the handles close as this returns, and the
                // thread's end gives the mutex up, as the end of the process
                // does.
            }
            Err(claim) => {
                let _ = said.send(Took::Not(claim));
            }
        }
    }

    /// One try at the guard, on the thread that keeps it if it takes it: both
    /// handles and whether the mutex had been given up by a holder that ended,
    /// or the answer. The boundary decides who may create the namespace, and
    /// the wait decides who holds the guard.
    fn attempt(name: &str) -> Result<(Namespace, Mutex, bool), Claim> {
        let (bd, namespace_sd, mutex_sd) = match (
            boundary(name),
            descriptor(NAMESPACE_SDDL),
            descriptor(MUTEX_SDDL),
        ) {
            (Ok(bd), Ok(namespace_sd), Ok(mutex_sd)) => (bd, namespace_sd, mutex_sd),
            (Err(why), _, _) | (_, Err(why), _) | (_, _, Err(why)) => {
                return Err(Claim::Unavailable(why));
            }
        };
        let alias = wide(name);
        let namespace = match create_namespace(&namespace_sd, &bd, &alias) {
            Ok(namespace) => namespace,
            Err(ERROR_ALREADY_EXISTS) => match open_namespace(&bd, &alias) {
                Ok(namespace) => namespace,
                // Its last handle closed between the two calls: one more create.
                Err(open_err) if not_found(open_err) => {
                    create_namespace(&namespace_sd, &bd, &alias).map_err(|again| {
                        Claim::Unavailable(format!(
                            "the namespace was there, then gone (open error {open_err}), and \
                             the create again failed (error {again})"
                        ))
                    })?
                }
                Err(open_err) => {
                    return Err(Claim::Unavailable(format!(
                        "the namespace was there, and its open failed (error {open_err})"
                    )));
                }
            },
            // The boundary refused the create: this run is outside it after all
            // (its token said otherwise, or could not be read). It can only
            // read the guard, as an unprivileged run does, and a refused create
            // alone is never read as held.
            Err(ERROR_ACCESS_DENIED) => {
                return Err(match check(name) {
                    Claim::Free => Claim::Unavailable(
                        "the create was refused (error 5), so this run is outside the boundary \
                         and takes nothing; no daemon of this machine holds the guard"
                            .to_string(),
                    ),
                    other => other,
                });
            }
            Err(err) => {
                return Err(Claim::Unavailable(format!(
                    "the namespace's create failed (error {err})"
                )));
            }
        };
        let mutex = create_mutex(&mutex_sd, &guard_name(name)).map_err(|err| {
            Claim::Unavailable(format!("the guard's create or open failed (error {err})"))
        })?;
        // SAFETY: a live mutex handle; a zero timeout never blocks.
        match unsafe { WaitForSingleObject(mutex.0, 0) } {
            WAIT_OBJECT_0 => Ok((namespace, mutex, false)),
            // No holder releases it, so the end of each holder gives it up: the
            // normal end of a predecessor, not a fault.
            WAIT_ABANDONED => Ok((namespace, mutex, true)),
            // Another daemon's thread owns it. Both handles close as this
            // returns, the mutex's first.
            WAIT_TIMEOUT => Err(Claim::Held),
            other => Err(Claim::Unavailable(format!(
                "the guard's wait returned {other:#x} (error {})",
                last_error()
            ))),
        }
    }

    /// [`super::Action::CheckOnly`] at `name`: it reads whether a thread owns the
    /// guard, with the one right to read that and no right to wait on it, take
    /// it or create anything, and it closes both handles before it answers.
    pub fn check(name: &str) -> Claim {
        let bd = match boundary(name) {
            Ok(bd) => bd,
            Err(why) => return Claim::Unavailable(why),
        };
        let _namespace = match open_namespace(&bd, &wide(name)) {
            Ok(namespace) => namespace,
            Err(err) if not_found(err) => return Claim::Free,
            // A descriptor that refuses the read: read as held, the answer that
            // never makes a second daemon.
            Err(ERROR_ACCESS_DENIED) => return Claim::Held,
            Err(err) => {
                return Claim::Unavailable(format!(
                    "the check's namespace open failed (error {err})"
                ));
            }
        };
        let mutex = match open_mutex(MUTANT_QUERY_STATE, &guard_name(name)) {
            Ok(mutex) => mutex,
            Err(err) if not_found(err) => return Claim::Free,
            Err(ERROR_ACCESS_DENIED) => return Claim::Held,
            Err(err) => {
                return Claim::Unavailable(format!("the check's guard open failed (error {err})"));
            }
        };
        match count(&mutex) {
            Ok(count) if count <= 0 => Claim::Held,
            Ok(_) => Claim::Free,
            Err(status) => {
                Claim::Unavailable(format!("the check's read failed (status {status:#x})"))
            }
        }
    }

    /// Test only: a look that keeps the namespace and, when asked, the guard
    /// open until it drops, as this thread's token opens them.
    #[cfg(test)]
    pub(super) struct Look {
        namespace: Namespace,
        guard: Option<Mutex>,
        name: String,
    }

    #[cfg(test)]
    impl Look {
        /// Opens the namespace at `name`, then the guard with `access` (not at
        /// all for `None`). `Err` = which open failed, and its error code.
        pub(super) fn open(name: &str, access: Option<u32>) -> Result<Look, (&'static str, u32)> {
            let bd = boundary(name).expect("the boundary descriptor");
            let namespace = open_namespace(&bd, &wide(name)).map_err(|e| ("namespace", e))?;
            let guard = match access {
                Some(access) => {
                    Some(open_mutex(access, &guard_name(name)).map_err(|e| ("guard", e))?)
                }
                None => None,
            };
            Ok(Look {
                namespace,
                guard,
                name: name.to_owned(),
            })
        }

        pub(super) fn namespace_handle(&self) -> HANDLE {
            self.namespace.0
        }

        pub(super) fn guard_handle(&self) -> Option<HANDLE> {
            self.guard.as_ref().map(|guard| guard.0)
        }

        /// A create of another object in the namespace, as this thread's token:
        /// `Err` = its error code. What it makes is closed at once.
        pub(super) fn create_beside(&self, other: &str) -> Result<(), u32> {
            let name = wide(&format!("{}\\{other}", self.name));
            // SAFETY: `name` is NUL-terminated; default security; no owner.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle.is_null() {
                return Err(last_error());
            }
            // SAFETY: the handle the create returned; closed once.
            unsafe { CloseHandle(handle) };
            Ok(())
        }
    }
}

/// Linux and macOS: `flock` on a 0600 file in a root-only folder.
#[cfg(unix)]
pub mod unix {
    use super::{Claim, Guard};
    use std::path::{Path, PathBuf};

    /// The root daemon's runtime folder, where its LocalAPI socket lives too.
    pub const DIR: &str = "/var/run/roomler";

    /// The guard's file for `key`.
    pub fn path_for(key: &str) -> PathBuf {
        Path::new(DIR).join(format!("daemon-{key}.lock"))
    }

    /// [`super::Action::Take`] with the production key, in [`DIR`], kept owner
    /// only as the LocalAPI keeps it.
    pub(super) fn take_here() -> Claim {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::create_dir_all(DIR) {
            return Claim::Unavailable(format!("creating {DIR}: {e}"));
        }
        let _ = std::fs::set_permissions(DIR, std::fs::Permissions::from_mode(0o700));
        take_at(&path_for(super::KEY))
    }

    /// [`super::Action::Take`] at `path`. Never a pid file: the lock is the
    /// kernel's, and it goes with the process. The file is 0600, because any
    /// process that can open it can lock it.
    pub fn take_at(path: &Path) -> Claim {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // std opens it close-on-exec, so no program this daemon starts keeps it.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(e) => return Claim::Unavailable(format!("opening {}: {e}", path.display())),
        };
        // 0600 for a file an earlier start left too: `mode` applies to a new one.
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        // SAFETY: the fd is `file`'s for the whole call; LOCK_NB never blocks.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Claim::Taken(Guard { _file: file });
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Claim::Held,
            _ => Claim::Unavailable(format!("flock on {}: {err}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    /// Cell 4 (#4280) — only a privileged run takes the guard: root, SYSTEM and
    /// an elevated administrator. An unprivileged run checks it on Windows and
    /// skips it off Windows. A token that could not be read takes it on Windows,
    /// where the boundary decides.
    #[test]
    fn only_a_privileged_run_takes_the_guard() {
        use Action::{CheckOnly, Skip, Take};
        use Platform::{Unix, Windows};
        use Privilege::{Elevated, Root, System, Unknown, Unprivileged};
        for (platform, privilege, want) in [
            (Windows, System, Take),
            (Windows, Elevated, Take),
            (Windows, Unknown, Take),
            (Windows, Unprivileged, CheckOnly),
            (Unix, Root, Take),
            (Unix, Unprivileged, Skip),
        ] {
            assert_eq!(
                action(platform, privilege),
                want,
                "{platform:?} {privilege:?}"
            );
        }
        // What a Windows token says.
        for (admin, rid, want) in [
            // The service host's worker.
            (Some(true), Some(0x4000), System),
            (Some(true), Some(0x3000), Elevated),
            // A UAC-filtered administrator or a standard user.
            (Some(false), Some(0x2000), Unprivileged),
            // LocalService, NetworkService: System integrity, no Administrators.
            (Some(false), Some(0x4000), Unprivileged),
            (Some(true), Some(0x2000), Unprivileged),
            (None, Some(0x3000), Unknown),
            (Some(true), None, Unknown),
        ] {
            assert_eq!(privilege_of(admin, rid), want, "{admin:?} {rid:?}");
        }
    }

    /// Only a supervised start waits for a held guard.
    #[test]
    fn only_a_supervised_start_waits() {
        use crate::supervision::SystemdUnit;
        let unit = SystemdUnit {
            name: "roomlerd.service",
            user: false,
        };
        for supervision in [
            Supervision::WindowsScm,
            Supervision::WindowsTask,
            Supervision::Systemd(unit),
            Supervision::Launchd,
            Supervision::MacosSupervisor,
        ] {
            assert!(waits(supervision), "{supervision:?}");
        }
        assert!(!waits(Supervision::None));
    }

    /// #4403 — the predicate, as a table: only the Windows service's own worker
    /// running as SYSTEM that holds the guard is decided by the guard alone.
    /// Each input negated in turn (the platform, the privilege the guard was
    /// taken under, what the run did with the guard, the supervisor) leaves the
    /// config's lock deciding.
    #[test]
    fn only_the_service_system_worker_holding_the_guard_relaxes_the_config_lock() {
        use Platform::{Unix, Windows};
        use Privilege::{Elevated, Root, System, Unknown, Unprivileged};
        let unit = crate::supervision::SystemdUnit {
            name: "roomlerd.service",
            user: false,
        };
        let scm = Supervision::WindowsScm;
        for (platform, supervision, hold, want) in [
            (Windows, scm, Hold::Taken(System), true),
            // The platform.
            (Unix, scm, Hold::Taken(System), false),
            // The privilege the guard was taken under.
            (Windows, scm, Hold::Taken(Elevated), false),
            (Windows, scm, Hold::Taken(Root), false),
            (Windows, scm, Hold::Taken(Unprivileged), false),
            (Windows, scm, Hold::Taken(Unknown), false),
            // What the run did with the guard.
            (Windows, scm, Hold::Unavailable, false),
            (Windows, scm, Hold::Free, false),
            (Windows, scm, Hold::Skipped, false),
            // The supervisor.
            (
                Windows,
                Supervision::WindowsTask,
                Hold::Taken(System),
                false,
            ),
            (
                Windows,
                Supervision::Systemd(unit),
                Hold::Taken(System),
                false,
            ),
            (Windows, Supervision::Launchd, Hold::Taken(System), false),
            (
                Windows,
                Supervision::MacosSupervisor,
                Hold::Taken(System),
                false,
            ),
            (Windows, Supervision::None, Hold::Taken(System), false),
        ] {
            assert_eq!(
                relaxes_config_lock(platform, supervision, hold),
                want,
                "{platform:?} {supervision:?} {hold:?}"
            );
        }
    }

    /// #4403 — what `hold_here` says for each claim it settles. A guard another
    /// daemon holds is its `Err`. (A claim that took the guard says `Taken` with
    /// its privilege; settling one stores the guard for the whole process, so
    /// that row is not run here.)
    #[test]
    fn each_claim_settles_to_what_the_run_did() {
        for (claim, privilege, want) in [
            (Claim::Free, Privilege::Unprivileged, Hold::Free),
            (Claim::Skipped, Privilege::Unprivileged, Hold::Skipped),
            (
                Claim::Unavailable("a test".into()),
                Privilege::System,
                Hold::Unavailable,
            ),
            (
                Claim::Unavailable("a test".into()),
                Privilege::Elevated,
                Hold::Unavailable,
            ),
        ] {
            assert_eq!(settle(claim, privilege).ok(), Some(want), "{privilege:?}");
        }
        assert!(settle(Claim::Held, Privilege::System).is_err());
    }

    /// A clock the test moves: each sleep advances it, and every try is counted.
    struct Clock {
        start: Instant,
        at: Cell<Duration>,
        tries: Cell<u32>,
        slept: Cell<u32>,
    }

    impl Clock {
        fn new() -> Self {
            Clock {
                start: Instant::now(),
                at: Cell::new(Duration::ZERO),
                tries: Cell::new(0),
                slept: Cell::new(0),
            }
        }

        /// A take whose holder ends at `ends` (never, for `None`).
        fn wait_for(&self, ends: Option<Duration>, wait: bool) -> Claim {
            block_on(take_waiting(
                || {
                    self.tries.set(self.tries.get() + 1);
                    match ends {
                        Some(ends) if self.at.get() >= ends => Claim::Taken(Guard::stand_in()),
                        _ => Claim::Held,
                    }
                },
                wait,
                |step| {
                    self.slept.set(self.slept.get() + 1);
                    self.at.set(self.at.get() + step);
                    std::future::ready(())
                },
                || self.start + self.at.get(),
            ))
        }
    }

    /// Cell 1 (#4280) — a supervised start whose holder stays past the service
    /// host's 1.5 s terminate wait, but ends inside the bound, takes the guard
    /// once it ends. A holder that never ends is left after the bound, and an
    /// unsupervised start does not wait at all.
    #[test]
    fn a_supervised_start_waits_out_a_predecessor_inside_the_bound() {
        let clock = Clock::new();
        let claim = clock.wait_for(Some(Duration::from_secs(2)), true);
        assert!(matches!(claim, Claim::Taken(_)), "{claim:?}");
        assert_eq!(clock.at.get(), Duration::from_secs(2));
        assert_eq!(
            clock.tries.get(),
            9,
            "one try, then one every 250 ms to 2 s"
        );

        let clock = Clock::new();
        let claim = clock.wait_for(None, true);
        assert!(matches!(claim, Claim::Held), "{claim:?}");
        assert_eq!(
            clock.at.get(),
            WAIT_BOUND,
            "it waits the bound and no longer"
        );
        assert_eq!(clock.tries.get(), 121);

        let clock = Clock::new();
        let claim = clock.wait_for(Some(Duration::from_secs(2)), false);
        assert!(matches!(claim, Claim::Held), "{claim:?}");
        assert_eq!((clock.tries.get(), clock.slept.get()), (1, 0));

        let clock = Clock::new();
        let claim = clock.wait_for(Some(Duration::ZERO), true);
        assert!(matches!(claim, Claim::Taken(_)), "{claim:?}");
        assert_eq!((clock.tries.get(), clock.slept.get()), (1, 0));
    }

    /// The key is the overlay interface's name, in every build that has one.
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    #[test]
    fn the_key_is_the_overlay_interface_name() {
        assert_eq!(KEY, tunnel_core::overlay::tun::IF_NAME);
    }

    /// How every Windows test name starts. The production name never does.
    #[cfg(windows)]
    const TEST_NAME: &str = "Roomler-Test-";

    /// A boundary name of this test's own. The boundary's name keys the
    /// namespace, so a test that shared a name would share a guard; a test
    /// never uses the production name.
    #[cfg(windows)]
    fn test_name(case: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{TEST_NAME}{case}-{}-{nanos}", std::process::id())
    }

    #[cfg(windows)]
    fn elevated_or_skip(case: &str) -> bool {
        roomler_node_core::machine_tree::test_support::elevated_or_skip(case)
    }

    /// The production name is keyed by the interface, and no test name is it.
    #[cfg(windows)]
    #[test]
    fn the_guard_is_keyed_by_the_interface() {
        assert_eq!(win::name_for(KEY), "Roomler-Daemon-roomler");
        assert!(!win::name_for(KEY).starts_with(TEST_NAME));
    }

    /// What a [`machine_guard_helper`] child holds (see its roles).
    #[cfg(windows)]
    const HELPER_ROLE: &str = "ROOMLERD_TEST_GUARD_ROLE";

    /// Where it holds it: a name that starts with [`TEST_NAME`].
    #[cfg(windows)]
    const HELPER_NAME: &str = "ROOMLERD_TEST_GUARD_NAME";

    /// What starts the helper's one line on its stderr.
    #[cfg(windows)]
    const HELPER_SAYS: &str = "[machine guard helper] ";

    /// How long a helper has to say it holds (CI's runners are slow).
    #[cfg(windows)]
    const HELPER_READY_WITHIN: Duration = Duration::from_secs(20);

    /// The second process of the Windows cells. A second daemon is another
    /// process, and these cells need one: a namespace's alias belongs to the
    /// process that opened it, and the guard to the thread that owns it. It
    /// holds its role at its name, says `ready` or `failed <why>`, and holds
    /// until it is killed or its stdin closes (its parent is gone). Ignored,
    /// and with no role it does nothing: only [`Helper::start`] runs it.
    ///
    /// - `take`: the production take.
    /// - `take-thread`: the production take, asked from a thread that ends
    ///   before this process does (cell O1).
    /// - `take-runtime`: the production take, asked from a task of a runtime
    ///   that is shut down before this process ends, as `main` shuts its
    ///   runtime down before the exit purge (cell R).
    /// - `look`: a privileged open of the namespace and of the guard, kept.
    #[cfg(windows)]
    #[test]
    #[ignore = "the second process of the machine guard's Windows cells: Helper::start runs it"]
    fn machine_guard_helper() {
        let (Ok(role), Ok(name)) = (std::env::var(HELPER_ROLE), std::env::var(HELPER_NAME)) else {
            return;
        };
        let say = |word: &str| eprintln!("{HELPER_SAYS}{word}");
        if !name.starts_with(TEST_NAME) {
            say("failed: not a test name");
            panic!("the helper holds a test name only, never {name:?}");
        }
        let taken = |claim: Claim| -> Box<dyn std::any::Any> {
            match claim {
                Claim::Taken(guard) => Box::new(guard),
                other => {
                    say(&format!("failed {other:?}"));
                    panic!("the helper's {role}: {other:?}");
                }
            }
        };
        let held: Box<dyn std::any::Any> = match role.as_str() {
            "take" => taken(win::take(&name)),
            "take-thread" => {
                let asked = name.clone();
                let claim = std::thread::spawn(move || win::take(&asked))
                    .join()
                    .expect("the asking thread");
                taken(claim)
            }
            "take-runtime" => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("a runtime");
                let asked = name.clone();
                let claim = runtime.block_on(async move {
                    tokio::spawn(async move {
                        take_waiting(
                            || win::take(&asked),
                            false,
                            tokio::time::sleep,
                            Instant::now,
                        )
                        .await
                    })
                    .await
                    .expect("the take's task")
                });
                runtime.shutdown_timeout(Duration::from_secs(10));
                taken(claim)
            }
            "look" => match win::Look::open(&name, Some(win::MUTANT_QUERY_STATE)) {
                Ok(look) => Box::new(look),
                Err((which, code)) => {
                    say(&format!("failed: the {which} open, error {code}"));
                    panic!("the helper's look: the {which} open, error {code}");
                }
            },
            other => {
                say(&format!("failed: no role {other:?}"));
                panic!("no helper role {other:?}");
            }
        };
        say("ready");
        // Held until this process is killed, or its stdin closes.
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
        drop(held);
    }

    /// A [`machine_guard_helper`] child. Its drop kills it and waits for its
    /// exit, so a failing assert never leaves a child holding a guard.
    #[cfg(windows)]
    struct Helper(std::process::Child);

    #[cfg(windows)]
    impl Helper {
        /// Starts a helper that holds `role` at `name`, and returns once it says
        /// so. Panics, with what it said, if it fails or says nothing in
        /// [`HELPER_READY_WITHIN`].
        fn start(role: &str, name: &str) -> Helper {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            assert!(name.starts_with(TEST_NAME), "{name:?}");
            let here = module_path!()
                .split_once("::")
                .map_or(module_path!(), |(_, path)| path);
            let test = format!("{here}::machine_guard_helper");
            let child = Command::new(std::env::current_exe().expect("this test binary"))
                .arg(&test)
                .args(["--exact", "--ignored", "--nocapture", "--test-threads=1"])
                .env(HELPER_ROLE, role)
                .env(HELPER_NAME, name)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start a helper");
            let mut helper = Helper(child);
            let stderr = helper.0.stderr.take().expect("the helper's stderr");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                // Read to the end, so the helper never blocks on a full pipe.
                for line in std::io::BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    let _ = tx.send(line);
                }
            });
            let deadline = Instant::now() + HELPER_READY_WITHIN;
            let mut said = Vec::new();
            loop {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(line) => match line.split_once(HELPER_SAYS) {
                        Some((_, "ready")) => return helper,
                        Some((_, word)) => {
                            panic!("the {role} helper at {name}: {word}; it said {said:?}")
                        }
                        None => said.push(line),
                    },
                    Err(err) => {
                        panic!(
                            "the {role} helper at {name} is not ready ({err:?}); it said {said:?}"
                        )
                    }
                }
            }
        }

        /// Ends it as a crash ends a daemon (TerminateProcess), and waits for
        /// its exit.
        fn kill(mut self) {
            self.end();
        }

        fn end(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(windows)]
    impl Drop for Helper {
        fn drop(&mut self) {
            self.end();
        }
    }

    /// Ask an open for everything its token may get.
    #[cfg(windows)]
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;

    /// Read a security descriptor.
    #[cfg(windows)]
    const READ_CONTROL: u32 = 0x0002_0000;

    // ntdll's read of what an open was granted (ObjectBasicInformation).
    #[cfg(windows)]
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtQueryObject(
            handle: windows_sys::Win32::Foundation::HANDLE,
            class: u32,
            information: *mut std::ffi::c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }

    /// What the open that made `handle` was granted. `Err` = the status.
    #[cfg(windows)]
    fn granted(handle: windows_sys::Win32::Foundation::HANDLE) -> Result<u32, i32> {
        #[repr(C)]
        struct Basic {
            attributes: u32,
            granted: u32,
            rest: [u32; 12],
        }
        let mut info = Basic {
            attributes: 0,
            granted: 0,
            rest: [0; 12],
        };
        let mut returned = 0u32;
        // SAFETY: a live handle; `info` is class 0's layout and outlives the
        // call, which writes at most `length` bytes into it.
        let status = unsafe {
            NtQueryObject(
                handle,
                0,
                (&mut info as *mut Basic).cast(),
                std::mem::size_of::<Basic>() as u32,
                &mut returned,
            )
        };
        if status < 0 {
            Err(status)
        } else {
            Ok(info.granted)
        }
    }

    /// The owner, DACL and label stored on the object behind `handle`, as
    /// SDDL. The group is the creator's and differs between machines, so it is
    /// not read.
    #[cfg(windows)]
    fn stored(handle: windows_sys::Win32::Foundation::HANDLE) -> String {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR,
        };
        let what =
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a live handle; the out-pointers are valid or null.
        let rc = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                what,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, 0, "GetSecurityInfo");
        let mut text: *mut u16 = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: `sd` is the descriptor GetSecurityInfo allocated.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                what,
                &mut text,
                &mut len,
            )
        };
        assert_ne!(
            ok, 0,
            "ConvertSecurityDescriptorToStringSecurityDescriptorW"
        );
        // SAFETY: `text` holds `len` UTF-16 units, the trailing NUL included.
        let sddl =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len as usize) })
                .trim_end_matches('\0')
                .to_string();
        // SAFETY: both were allocated by the calls above; freed once.
        unsafe {
            LocalFree(text.cast());
            LocalFree(sd);
        }
        sddl
    }

    /// `stored` names Administrators as the owner, the DACL `dacl` exactly,
    /// and the one label ACE `label`.
    #[cfg(windows)]
    fn assert_stored(stored: &str, dacl: &str, label: &str) {
        let (owner, rest) = stored
            .split_once("D:")
            .unwrap_or_else(|| panic!("no DACL in {stored}"));
        let (dacl_read, sacl) = rest
            .split_once("S:")
            .unwrap_or_else(|| panic!("no label in {stored}"));
        assert_eq!(owner, "O:BA", "{stored}");
        assert_eq!(dacl_read, dacl, "{stored}");
        assert!(
            sacl.ends_with(label) && sacl.matches('(').count() == 1,
            "{stored}"
        );
    }

    /// Runs `f` on a thread that impersonates a restricted, medium-integrity copy
    /// of this process's token: an unprivileged run's view.
    #[cfg(windows)]
    fn as_unprivileged<T: Send>(f: impl FnOnce() -> T + Send) -> T {
        use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
        std::thread::scope(|s| {
            s.spawn(|| {
                let token = crate::win_token::restricted_medium_copy()
                    .expect("a restricted medium copy of this token");
                // SAFETY: a live token this thread holds until it reverts.
                let ok = unsafe { ImpersonateLoggedOnUser(token.raw()) };
                assert_ne!(ok, 0, "impersonating the restricted copy");
                let out = f();
                // SAFETY: ends this thread's impersonation.
                unsafe { RevertToSelf() };
                out
            })
            .join()
            .expect("the impersonating thread")
        })
    }

    /// Cell 3a (#4280) — a crashed holder's guard is free again. Another process
    /// takes it; this one's take reads it held three times running (the
    /// bounded wait's repeated tries from one process), and its check reads it
    /// held. The holder is killed, and the next take takes. Against the real
    /// API, so it needs an elevated token (CI's Windows runners are).
    #[cfg(windows)]
    #[test]
    fn a_crashed_holders_guard_is_free_again() {
        if !elevated_or_skip("the machine guard after its holder ended") {
            return;
        }
        let name = test_name("ended");
        let holder = Helper::start("take", &name);
        for n in 1..=3 {
            let take = win::take(&name);
            assert!(matches!(take, Claim::Held), "take {n}: {take:?}");
        }
        let check = win::check(&name);
        assert!(matches!(check, Claim::Held), "{check:?}");
        holder.kill();
        let again = win::take(&name);
        assert!(matches!(again, Claim::Taken(_)), "{again:?}");
    }

    /// Cell 3b (#4280) — a privileged look open across its holder's crash, and
    /// closed after it, keeps nothing: the next take takes.
    #[cfg(windows)]
    #[test]
    fn a_look_closed_after_its_holders_crash_keeps_nothing() {
        if !elevated_or_skip("the machine guard after a look closed") {
            return;
        }
        let name = test_name("looked");
        let holder = Helper::start("take", &name);
        let look = Helper::start("look", &name);
        holder.kill();
        look.kill();
        let again = win::take(&name);
        assert!(matches!(again, Claim::Taken(_)), "{again:?}");
    }

    /// Cell 3c (#4280) — the fences, read as what each open is granted. Under a
    /// restricted medium-integrity token, beside a holder: the namespace opens
    /// with traverse only (0x2), the guard asked for all it may get opens with
    /// the read only (0x1), and a create of anything else in the namespace is
    /// refused. The holder is killed, and the next take takes.
    #[cfg(windows)]
    #[test]
    fn an_unprivileged_open_is_granted_the_read_only() {
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        if !elevated_or_skip("the machine guard's fences") {
            return;
        }
        let name = test_name("fences");
        let holder = Helper::start("take", &name);
        let (namespace, guard, create) = as_unprivileged(|| {
            let look = win::Look::open(&name, Some(MAXIMUM_ALLOWED)).expect("a restricted look");
            let guard = look.guard_handle().expect("the guard's handle");
            (
                granted(look.namespace_handle()),
                granted(guard),
                look.create_beside("Other"),
            )
        });
        assert_eq!(namespace, Ok(0x2), "the namespace: traverse only");
        assert_eq!(
            guard,
            Ok(win::MUTANT_QUERY_STATE),
            "the guard: the read only"
        );
        assert_eq!(
            create,
            Err(ERROR_ACCESS_DENIED),
            "a create beside the guard"
        );
        holder.kill();
        let again = win::take(&name);
        assert!(matches!(again, Claim::Taken(_)), "{again:?}");
    }

    /// Cell 3d (#4280) — the guard is held only while its owner lives. A look in
    /// another process opens the namespace and the guard before the holder's
    /// crash and keeps both after it; the next take takes, from the guard its
    /// holder gave up (`WAIT_ABANDONED`), while the look is still open. The
    /// namespace lives until its last handle closes, so its being there says
    /// nothing about its holder: that is why the guard is a mutex, which its
    /// owner's end gives up.
    #[cfg(windows)]
    #[test]
    fn a_take_while_a_look_outlives_its_holder() {
        if !elevated_or_skip("the machine guard while a look outlives its holder") {
            return;
        }
        let name = test_name("outlived");
        let holder = Helper::start("take", &name);
        let look = Helper::start("look", &name);
        holder.kill();
        let during = win::take(&name);
        let Claim::Taken(guard) = during else {
            panic!("the take while the look is open: {during:?}");
        };
        assert!(
            guard.taken_after_an_end(),
            "it took the guard its holder gave up"
        );
        drop(guard);
        look.kill();
    }

    /// The guard is keyed: a holder at one name holds nothing at another.
    #[cfg(windows)]
    #[test]
    fn a_holder_at_one_name_holds_no_other() {
        if !elevated_or_skip("the machine guard's key") {
            return;
        }
        let held = test_name("keyed");
        let other = test_name("keyed-other");
        let holder = Helper::start("take", &held);
        let check = win::check(&other);
        assert!(matches!(check, Claim::Free), "{check:?}");
        let take = win::take(&other);
        assert!(matches!(take, Claim::Taken(_)), "{take:?}");
        drop(take);
        holder.kill();
    }

    /// Cell 5 (#4280) — the check, the branch an unprivileged run takes, reads
    /// only whether a thread owns the guard. A fresh name reads free and the
    /// check creates nothing. Under a restricted medium-integrity token, as
    /// under a privileged one: a held guard reads held, and once its holder is
    /// killed it reads free, even while a look in another process keeps the
    /// namespace and the guard open.
    #[cfg(windows)]
    #[test]
    fn an_unprivileged_check_reads_only_the_owner() {
        let name = test_name("check");
        for _ in 0..2 {
            let check = win::check(&name);
            assert!(matches!(check, Claim::Free), "{check:?}");
        }
        if !elevated_or_skip("the machine guard's check beside a holder") {
            return;
        }
        let check = as_unprivileged(|| win::check(&name));
        assert!(matches!(check, Claim::Free), "{check:?}");
        // Nothing was there, so the helper's take takes, or `start` panics.
        let holder = Helper::start("take", &name);
        let check = as_unprivileged(|| win::check(&name));
        assert!(
            matches!(check, Claim::Held),
            "restricted, beside a holder: {check:?}"
        );
        let check = win::check(&name);
        assert!(matches!(check, Claim::Held), "beside a holder: {check:?}");
        let look = Helper::start("look", &name);
        holder.kill();
        let check = as_unprivileged(|| win::check(&name));
        assert!(
            matches!(check, Claim::Free),
            "restricted, after the holder's end, beside the look: {check:?}"
        );
        let check = win::check(&name);
        assert!(
            matches!(check, Claim::Free),
            "after the holder's end, beside the look: {check:?}"
        );
        look.kill();
    }

    /// Cell O1 (#4280) — the take owns the guard on a thread of its own, so the
    /// thread that asked for it may end: a holder that asked from a thread
    /// that has ended still holds it, at once and 15 s later.
    #[cfg(windows)]
    #[test]
    fn a_guard_asked_from_a_thread_that_ended_stays_held() {
        if !elevated_or_skip("the machine guard asked from a thread that ended") {
            return;
        }
        let name = test_name("thread");
        let holder = Helper::start("take-thread", &name);
        let take = win::take(&name);
        assert!(matches!(take, Claim::Held), "{take:?}");
        std::thread::sleep(Duration::from_secs(15));
        let take = win::take(&name);
        assert!(matches!(take, Claim::Held), "15 s later: {take:?}");
        holder.kill();
    }

    /// Cell R (#4280) — the guard stays held after the runtime that asked for it
    /// is shut down: `main` shuts its runtime down before it purges the exit
    /// routes (`purge_after_daemon`), and no successor may take the guard while
    /// that purge runs.
    #[cfg(windows)]
    #[test]
    fn the_guard_outlives_the_runtime_that_asked_for_it() {
        if !elevated_or_skip("the machine guard after its runtime ended") {
            return;
        }
        let name = test_name("runtime");
        let holder = Helper::start("take-runtime", &name);
        let take = win::take(&name);
        assert!(matches!(take, Claim::Held), "{take:?}");
        holder.kill();
    }

    /// The stored security descriptors, read by a privileged look beside a
    /// holder: Administrators own the namespace and the guard, each carries
    /// the High label, and any other signed-in account gets one right on each,
    /// traverse on the namespace and the read on the guard.
    #[cfg(windows)]
    #[test]
    fn the_guard_s_stored_descriptors() {
        if !elevated_or_skip("the machine guard's stored descriptors") {
            return;
        }
        let name = test_name("descriptors");
        let holder = Helper::start("take", &name);
        let look = win::Look::open(&name, Some(READ_CONTROL)).expect("a privileged look");
        let namespace = stored(look.namespace_handle());
        let guard = stored(look.guard_handle().expect("the guard's handle"));
        drop(look);
        holder.kill();
        assert_stored(
            &namespace,
            "P(A;;CCDCLCSWSDRCWDWO;;;SY)(A;;CCDCLCSWSDRCWDWO;;;BA)(A;;DC;;;AU)",
            "(ML;;NW;;;HI)",
        );
        assert_stored(
            &guard,
            "P(A;;0x1f0001;;;SY)(A;;0x1f0001;;;BA)(A;;CC;;;AU)",
            "(ML;;NWNX;;;HI)",
        );
    }

    /// Cell 3 (#4280), off Windows — the kernel drops the lock with the file, as
    /// it does when the process ends, and the guard can be taken again. The file
    /// is 0600.
    #[cfg(unix)]
    #[test]
    fn a_crashed_holders_guard_is_free_again() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon-test.lock");
        let Claim::Taken(first) = unix::take_at(&path) else {
            panic!("the first take must take");
        };
        let second = unix::take_at(&path);
        assert!(matches!(second, Claim::Held), "{second:?}");
        first.end_as_process_exit();
        // A sibling test's fork holds a copy of the fd until its exec (see
        // `instance_lock`'s `lock_releases_on_drop`), so the take may wait a
        // bounded while; a lock that never frees still fails.
        let deadline = Instant::now() + Duration::from_secs(5);
        let again = loop {
            let claim = unix::take_at(&path);
            if matches!(claim, Claim::Taken(_)) || Instant::now() >= deadline {
                break claim;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(again, Claim::Taken(_)), "{again:?}");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The production guard's file is in the root daemon's own folder, keyed by
    /// the interface's name.
    #[cfg(unix)]
    #[test]
    fn the_guard_file_is_keyed_by_the_interface() {
        assert_eq!(
            unix::path_for(KEY),
            std::path::Path::new("/var/run/roomler").join(format!("daemon-{KEY}.lock"))
        );
    }
}
