//! MSI execution with synchronous wait + exit-code decoding.
//!
//! W3 in the rc.28 plan, BLOCKER-3 fix from the architect critique.
//!
//! `roomler_agent::updater::spawn_installer_inner` launches msiexec
//! via `ShellExecuteExW + verb=runas` (perMachine) or
//! `Command::new("msiexec")` (perUser) and returns the PID. That
//! function does NOT wait synchronously — its job is to start the
//! installer for the auto-updater path where the parent must exit so
//! msiexec can overwrite the EXE.
//!
//! The wizard is NOT being replaced by the install — it's a separate
//! binary that wants to observe MSI completion + decode the exit
//! code + surface the right recovery UI per failure mode. That work
//! lives here.
//!
//! ## Why synchronous wait + polling
//!
//! `WaitForSingleObject(handle, INFINITE)` would block the runtime
//! thread until msiexec exits. The wizard's `cmd_install` is an
//! async Tauri command; Tauri cancels its future when the operator
//! closes the window. A blocking wait wouldn't propagate the cancel
//! signal. Polling in 250 ms slices (plus a `Duration` budget) lets
//! the future yield + abort cleanly.
//!
//! ## Why not `tokio::process::Child::wait`
//!
//! The wizard doesn't OWN the msiexec process — it received the PID
//! from `spawn_installer_inner`'s `ShellExecuteExW` call. There's no
//! `Child` handle to wait on. We must `OpenProcess(SYNCHRONIZE)` by
//! PID and use the Win32 wait API directly.
//!
//! Cross-platform stub: on non-Windows builds `MsiRunner` exists but
//! every method returns "not supported" — keeps the wizard crate
//! compiling on Linux CI without `cfg`-gating every caller.

use std::time::Duration;

/// Decoded MSI exit code. Documented under "Windows Installer Error
/// Messages" in MSDN. The wizard's SPA maps each variant to a
/// distinct recovery panel (Retry vs. wait-and-retry vs. switch-
/// flavour vs. surface-log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsiExitDecoded {
    /// `0` — installation succeeded.
    Success,
    /// `1602` ERROR_INSTALL_USEREXIT — user cancelled. Most commonly
    /// "operator clicked No on the UAC prompt".
    UserCancel,
    /// `1603` ERROR_INSTALL_FAILURE — generic fatal error. Surface
    /// the `%TEMP%\MSI*.LOG` path so the operator can investigate.
    FatalError,
    /// `1618` ERROR_INSTALL_ALREADY_RUNNING — another Windows
    /// Installer operation is in progress. Wait-and-retry.
    AnotherInstall,
    /// `3010` — installation succeeded but a reboot is required for
    /// changes to take full effect. The wizard can still proceed to
    /// enrollment; the agent's first start may need the reboot.
    RebootRequired,
    /// Any other non-zero code.
    Other(i32),
}

/// Decode a Windows Installer exit code. Pure function, no IO; the
/// hot path the SPA branches on.
pub fn decode_msi_exit(code: i32) -> MsiExitDecoded {
    match code {
        0 => MsiExitDecoded::Success,
        1602 => MsiExitDecoded::UserCancel,
        1603 => MsiExitDecoded::FatalError,
        1618 => MsiExitDecoded::AnotherInstall,
        3010 => MsiExitDecoded::RebootRequired,
        other => MsiExitDecoded::Other(other),
    }
}

/// Errors from `MsiRunner` operations.
#[derive(Debug, thiserror::Error)]
pub enum MsiRunnerError {
    #[error("OpenProcess({pid}) failed: error {error:#x}")]
    OpenFailed { pid: u32, error: u32 },
    #[error("GetExitCodeProcess failed: error {0:#x}")]
    ExitCodeFailed(u32),
    #[error("MSI wait timed out after {0:?}")]
    Timeout(Duration),
    #[error("TerminateProcess failed: error {0:#x}")]
    TerminateFailed(u32),
    #[error("operation not supported on this platform")]
    Unsupported,
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{MsiExitDecoded, MsiRunnerError, decode_msi_exit};
    use std::time::{Duration, Instant};
    use tokio::time::sleep;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess, WaitForSingleObject,
    };

    /// Standard access right. windows-sys 0.59 exposes `SYNCHRONIZE`
    /// only as `FILE_ACCESS_RIGHTS` under
    /// `Win32::Storage::FileSystem`, but the same bits apply to any
    /// waitable kernel object. Hardcode the value (`0x00100000`) so
    /// the OpenProcess call below doesn't need typed-constant
    /// gymnastics across submodules.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    /// Win32 process handle that closes on drop. Tracks the
    /// msiexec PID so the wizard can wait + decode + (rarely)
    /// terminate.
    pub struct MsiRunner {
        pub(super) pid: u32,
        handle: HANDLE,
    }

    impl MsiRunner {
        /// Open SYNCHRONIZE | PROCESS_QUERY_INFORMATION |
        /// PROCESS_TERMINATE access to a PID returned by
        /// `roomler_agent::updater::spawn_installer_inner`.
        pub fn attach(pid: u32) -> Result<Self, MsiRunnerError> {
            // SAFETY: standard Win32 OpenProcess call; we check the
            // return value for null and propagate.
            let handle = unsafe {
                OpenProcess(
                    SYNCHRONIZE | PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE,
                    0,
                    pid,
                )
            };
            if handle.is_null() {
                let err = unsafe { GetLastError() };
                return Err(MsiRunnerError::OpenFailed { pid, error: err });
            }
            Ok(Self { pid, handle })
        }

        /// Wait up to `budget` for msiexec to exit, polling in
        /// 250 ms slices so the surrounding async future can be
        /// cancelled cleanly by Tauri.
        pub async fn wait_for_exit(
            &self,
            budget: Duration,
        ) -> Result<MsiExitDecoded, MsiRunnerError> {
            let deadline = Instant::now() + budget;
            loop {
                // SAFETY: handle is valid; SYNCHRONIZE was requested
                // in attach().
                let r = unsafe { WaitForSingleObject(self.handle, 0) };
                if r == WAIT_OBJECT_0 {
                    let mut code: u32 = 0;
                    // SAFETY: handle is valid;
                    // PROCESS_QUERY_INFORMATION was requested in
                    // attach().
                    let ok = unsafe { GetExitCodeProcess(self.handle, &mut code) };
                    if ok == 0 {
                        let err = unsafe { GetLastError() };
                        return Err(MsiRunnerError::ExitCodeFailed(err));
                    }
                    return Ok(decode_msi_exit(code as i32));
                }
                if Instant::now() > deadline {
                    return Err(MsiRunnerError::Timeout(budget));
                }
                sleep(Duration::from_millis(250)).await;
            }
        }

        /// Force-terminate msiexec. Best-effort; caller surfaces
        /// "may leave partial install" UI before invoking this.
        /// No rollback is possible because the wizard holds no
        /// transactional handle.
        pub fn terminate(&self) -> Result<(), MsiRunnerError> {
            // SAFETY: handle is valid; PROCESS_TERMINATE was
            // requested in attach(). Pass exit-code 1602 so a
            // subsequent GetExitCodeProcess maps cleanly to
            // MsiExitDecoded::UserCancel.
            let ok = unsafe { TerminateProcess(self.handle, 1602) };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                return Err(MsiRunnerError::TerminateFailed(err));
            }
            Ok(())
        }

        /// PID of the attached msiexec process. Exposed so the
        /// wizard can log it + emit it via `ProgressEvent` for the
        /// operator's diagnostics.
        pub fn pid(&self) -> u32 {
            self.pid
        }
    }

    impl Drop for MsiRunner {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                // SAFETY: handle is non-null per attach() invariant.
                unsafe { CloseHandle(self.handle) };
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::{MsiExitDecoded, MsiRunnerError};
    use std::time::Duration;

    /// Linux/macOS stub. The wizard EXE is Windows-only for v1; the
    /// stub keeps the crate compiling on Linux CI without `cfg`-
    /// gating every caller.
    pub struct MsiRunner {
        _pid: u32,
    }

    impl MsiRunner {
        pub fn attach(_pid: u32) -> Result<Self, MsiRunnerError> {
            Err(MsiRunnerError::Unsupported)
        }
        pub async fn wait_for_exit(
            &self,
            _budget: Duration,
        ) -> Result<MsiExitDecoded, MsiRunnerError> {
            Err(MsiRunnerError::Unsupported)
        }
        pub fn terminate(&self) -> Result<(), MsiRunnerError> {
            Err(MsiRunnerError::Unsupported)
        }
        pub fn pid(&self) -> u32 {
            self._pid
        }
    }
}

pub use imp::MsiRunner;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_success_to_success() {
        assert_eq!(decode_msi_exit(0), MsiExitDecoded::Success);
    }

    #[test]
    fn decodes_1602_to_user_cancel() {
        assert_eq!(decode_msi_exit(1602), MsiExitDecoded::UserCancel);
    }

    #[test]
    fn decodes_1603_to_fatal_error() {
        assert_eq!(decode_msi_exit(1603), MsiExitDecoded::FatalError);
    }

    #[test]
    fn decodes_1618_to_another_install() {
        assert_eq!(decode_msi_exit(1618), MsiExitDecoded::AnotherInstall);
    }

    #[test]
    fn decodes_3010_to_reboot_required() {
        assert_eq!(decode_msi_exit(3010), MsiExitDecoded::RebootRequired);
    }

    #[test]
    fn decodes_unknown_to_other() {
        assert_eq!(decode_msi_exit(42), MsiExitDecoded::Other(42));
        assert_eq!(decode_msi_exit(1), MsiExitDecoded::Other(1));
        assert_eq!(decode_msi_exit(-1), MsiExitDecoded::Other(-1));
    }

    /// Live smoke: spawn `cmd /c exit 1602`, attach to its PID,
    /// wait for exit, assert MsiExitDecoded::UserCancel. Validates
    /// the full Win32 wait + GetExitCodeProcess path against a real
    /// process. Only runs on Windows.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn smoke_attach_to_real_process_and_decode_exit() {
        use std::process::Command;
        use std::time::Duration;

        let child = Command::new("cmd")
            .args(["/c", "exit 1602"])
            .spawn()
            .expect("spawn cmd /c exit 1602");
        let pid = child.id();
        let runner = MsiRunner::attach(pid).expect("attach to spawned PID");
        let outcome = runner
            .wait_for_exit(Duration::from_secs(5))
            .await
            .expect("wait for exit");
        assert_eq!(outcome, MsiExitDecoded::UserCancel);
    }
}
