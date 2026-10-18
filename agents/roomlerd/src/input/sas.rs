// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P4a — a real Ctrl+Alt+Del on Windows.
//!
//! The viewer's button typed three ordinary keys, which apps on the desktop
//! receive and the Windows lock and sign-in screens never do: those answer only
//! the secure attention sequence (SAS), which the kernel raises for the physical
//! chord and `SendSAS` raises for software. `SendSAS(FALSE)` works only for a
//! LocalSystem caller, and only where the `SoftwareSASGeneration` policy lets
//! services generate one.
//!
//! So the daemon offers the real one (`input: sas` in the hello, and no
//! `keys-only` entry) only when both hold: this process runs as SYSTEM (the
//! SystemContext worker of an SCM install), and the policy lets services
//! generate a SAS. Anywhere else the button keeps typing keys, and its title
//! says why. The policy is read, never written: it is a security setting, and
//! on a managed laptop it is the organisation's.
//!
//! `sas.dll` is loaded when the sequence is sent, never imported: a binary that
//! imports a DLL some Windows edition lacks does not start at all. It is loaded
//! from System32 only, so a SYSTEM process can never pick up a planted copy.
//!
//! The decision is pure and compiled on every OS, so the lanes that never run
//! Windows tests still check it.

/// What `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`'s
/// `SoftwareSASGeneration` lets generate a SAS. Absent, Windows lets only Ease
/// of Access applications on the secure desktop do it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SasPolicy {
    /// 0: nothing may.
    Nothing,
    /// 1: services may.
    Services,
    /// 2, or the value absent: Ease of Access applications only.
    EaseOfAccess,
    /// 3: services and Ease of Access applications.
    ServicesAndEaseOfAccess,
    /// A value Windows does not define: read as "services may not".
    Unknown(u32),
}

impl SasPolicy {
    /// The policy a registry read gives: `None` when the value is absent.
    pub fn from_value(value: Option<u32>) -> Self {
        match value {
            None | Some(2) => Self::EaseOfAccess,
            Some(0) => Self::Nothing,
            Some(1) => Self::Services,
            Some(3) => Self::ServicesAndEaseOfAccess,
            Some(other) => Self::Unknown(other),
        }
    }

    /// Whether a service (a LocalSystem caller of `SendSAS(FALSE)`) may.
    pub fn lets_services(self) -> bool {
        matches!(self, Self::Services | Self::ServicesAndEaseOfAccess)
    }
}

/// Why this host types keys instead of sending the real sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SasRefusal {
    /// This process is not SYSTEM: a per-user install, or an SCM install whose
    /// worker runs as the signed-in person.
    NotSystem,
    /// The policy lets no service generate a SAS.
    Policy(SasPolicy),
}

impl SasRefusal {
    /// One sentence for the viewer's title, after the button's own words.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotSystem => {
                "Ctrl+Alt+Del is sent as ordinary keys: apps on the desktop get it, but the Windows lock and sign-in screens do not. Only a service running as SYSTEM can send the real one, and this install runs as the signed-in person"
            }
            Self::Policy(_) => {
                "Ctrl+Alt+Del is sent as ordinary keys: apps on the desktop get it, but the Windows lock and sign-in screens do not. Windows' software Secure Attention Sequence policy (SoftwareSASGeneration) lets no service send the real one here"
            }
        }
    }
}

/// The decision: the real sequence needs a SYSTEM caller AND a policy that
/// lets services generate it.
pub fn decide(is_system: bool, policy: SasPolicy) -> Result<(), SasRefusal> {
    if !is_system {
        return Err(SasRefusal::NotSystem);
    }
    if !policy.lets_services() {
        return Err(SasRefusal::Policy(policy));
    }
    Ok(())
}

/// Whether this process can send the real sequence now. Read at every
/// announcement: a policy a GPO refresh changes is picked up at the next one.
#[cfg(target_os = "windows")]
pub fn availability() -> Result<(), SasRefusal> {
    decide(imp::is_system(), SasPolicy::from_value(imp::read_policy()))
}

/// Send the secure attention sequence: what the physical chord raises.
#[cfg(target_os = "windows")]
pub fn send() -> anyhow::Result<()> {
    imp::send()
}

#[cfg(target_os = "windows")]
mod imp {
    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, FreeLibrary};
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `SoftwareSASGeneration`, or `None` when it is absent or unreadable.
    pub fn read_policy() -> Option<u32> {
        let key = wide(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
        let name = wide("SoftwareSASGeneration");
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: `key` and `name` are NUL-terminated UTF-16 buffers; `value`
        // and `size` are valid out-pointers sized for the DWORD asked for.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        };
        (rc == ERROR_SUCCESS).then_some(value)
    }

    /// Whether this process runs as LocalSystem (the SystemContext worker).
    pub fn is_system() -> bool {
        #[cfg(feature = "system-context")]
        {
            crate::system_context::worker_role::probe_self()
                .map(|role| role.is_system_context())
                .unwrap_or(false)
        }
        #[cfg(not(feature = "system-context"))]
        {
            false
        }
    }

    pub fn send() -> Result<()> {
        let dll = wide("sas.dll");
        // SAFETY: `dll` is a NUL-terminated UTF-16 buffer; System32 only, so a
        // copy planted beside the binary is never the one loaded.
        let lib = unsafe {
            LoadLibraryExW(
                dll.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if lib.is_null() {
            bail!("sas.dll: {}", std::io::Error::last_os_error());
        }
        // SAFETY: `lib` is the module just loaded; the name is NUL-terminated.
        let proc = unsafe { GetProcAddress(lib, c"SendSAS".as_ptr().cast()) };
        let Some(proc) = proc else {
            // SAFETY: `lib` came from LoadLibraryExW and is freed once.
            unsafe { FreeLibrary(lib) };
            bail!("sas.dll has no SendSAS");
        };
        // SAFETY: `SendSAS` is `VOID SendSAS(BOOL AsUser)`, stdcall/system; the
        // pointer came from GetProcAddress for exactly that export.
        let send_sas: unsafe extern "system" fn(i32) = unsafe { std::mem::transmute(proc) };
        // FALSE: as a service. TRUE would be an Ease of Access application's
        // call, which needs uiAccess and the policy's other half.
        // SAFETY: a plain call into the export with a BOOL argument.
        unsafe { send_sas(0) };
        // SAFETY: as above, freed once; SendSAS has returned.
        unsafe { FreeLibrary(lib) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_value_reads_as_windows_documents_it() {
        assert_eq!(SasPolicy::from_value(None), SasPolicy::EaseOfAccess);
        assert_eq!(SasPolicy::from_value(Some(0)), SasPolicy::Nothing);
        assert_eq!(SasPolicy::from_value(Some(1)), SasPolicy::Services);
        assert_eq!(SasPolicy::from_value(Some(2)), SasPolicy::EaseOfAccess);
        assert_eq!(
            SasPolicy::from_value(Some(3)),
            SasPolicy::ServicesAndEaseOfAccess
        );
        assert_eq!(SasPolicy::from_value(Some(7)), SasPolicy::Unknown(7));
    }

    /// Only 1 and 3 let a service send it. An absent value is NOT "services
    /// may": Windows' default lets only Ease of Access applications.
    #[test]
    fn only_a_policy_that_names_services_lets_the_daemon_send_it() {
        let lets: Vec<bool> = [None, Some(0), Some(1), Some(2), Some(3), Some(9)]
            .into_iter()
            .map(|v| SasPolicy::from_value(v).lets_services())
            .collect();
        assert_eq!(lets, [false, false, true, false, true, false]);
    }

    #[test]
    fn the_real_sequence_needs_system_and_the_policy() {
        assert_eq!(decide(true, SasPolicy::Services), Ok(()));
        assert_eq!(decide(true, SasPolicy::ServicesAndEaseOfAccess), Ok(()));
        assert_eq!(
            decide(false, SasPolicy::Services),
            Err(SasRefusal::NotSystem),
            "a per-user install, however open the policy"
        );
        assert_eq!(
            decide(true, SasPolicy::EaseOfAccess),
            Err(SasRefusal::Policy(SasPolicy::EaseOfAccess)),
            "SYSTEM, but Windows' default policy"
        );
        assert_eq!(
            decide(false, SasPolicy::Nothing),
            Err(SasRefusal::NotSystem),
            "not being SYSTEM is said first: without it the policy does not matter"
        );
    }

    /// The reasons extend the button's own words; each says what reaches the
    /// lock screen and what would change it.
    #[test]
    fn each_refusal_says_why() {
        assert!(SasRefusal::NotSystem.reason().contains("signed-in person"));
        assert!(
            SasRefusal::Policy(SasPolicy::Nothing)
                .reason()
                .contains("SoftwareSASGeneration")
        );
    }
}
