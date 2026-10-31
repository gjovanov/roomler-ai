// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3115 — registering and retiring the `Roomler` SCM entry, so that an MSI
//! run never leaves the machine without it.
//!
//! ## What went wrong
//!
//! Every perMachine MSI run that is not an uninstall runs `RegisterService`
//! (`service install --as-service`, `wix-perMachine/main.wxs`). Up to 0.4.189
//! that made a "clean slate": stop the service, wait at most **5 s** for
//! STOPPED, delete it in whatever state it had reached, and create it again.
//! The host's own shutdown is bounded by far more than 5 s — the worker alone
//! has [`supervisor::WORKER_GRACEFUL_STOP_BUDGET`] (8 s) to leave gracefully —
//! so a stop that was slow but healthy lost by construction:
//!
//! 1. `DeleteService` on a service that is still stopping only MARKS it;
//! 2. `CreateService` then answers 1072 (`ERROR_SERVICE_MARKED_FOR_DELETE`),
//!    which `install` did not expect (it tolerated 1073, `ERROR_SERVICE_EXISTS`);
//! 3. the custom action's failure is ignored, a later one
//!    (`EnableSystemContext`) fails on the missing service, and the MSI rolls
//!    back with 1603 — and nothing in a rollback re-creates the service;
//! 4. when the host finally exits, the SCM removes the marked entry, and the
//!    machine has no service and no daemon until someone installs again.
//!
//! In the field (2026-10-30, a Win11 laptop, a maintenance run of 0.4.188
//! with SystemContext) the worker needed 7.8 s of its 8 s budget, and the
//! machine was offline for about two minutes, until the wizard's Retry. The
//! vmtest cell in #3115 lost one run in five (stops of 3.4–6.2 s).
//!
//! ## The rules now
//!
//! - **An existing entry is reconfigured in place** ([`register`]):
//!   `ChangeServiceConfigW`, the description and the recovery actions, and a
//!   running service keeps running. There is no window without a registered
//!   service, a handle held elsewhere (services.msc, a monitoring agent) cannot
//!   block a re-creation, and the `Environment` block (the SystemContext
//!   switch) survives. The P3d reason for the clean slate, a reused entry
//!   whose `StartServiceW` failed with error 87, was a malformed EMPTY
//!   `Environment` value (`hex(7):00,00`). It was fixed at its writer one
//!   release candidate later (`environment.rs`, `write_service_env`), and a
//!   reconfigure repairs one it finds ([`super::environment::normalize_service_env`]).
//! - **A service is deleted only when a retire wants it gone** ([`retire`]: an
//!   uninstall, or the legacy pre-rename service). It is stopped, and the
//!   retire waits for STOPPED and then for its host PROCESS to exit, within
//!   [`STOP_WAIT_BOUND`]. That bound is derived from the host's own shutdown
//!   constants, so the two cannot drift apart again. Then it deletes, closes
//!   its own handle (the entry cannot go while one is open), and waits for the
//!   SCM to answer 1060.
//! - **A 1072 is waited out** ([`MARKED_WAIT_BOUND`]): an earlier delete is
//!   still completing — a major upgrade's removal of the old product, whose
//!   binary still had the 5 s wait — and the service is created once it has.
//!
//! The waits run against [`Scm`], a seam over the SCM calls, so the tests
//! below drive them with a scripted SCM on a virtual clock.

#![cfg(target_os = "windows")]

use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows_service::service::{Service, ServiceAccess, ServiceInfo};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfigW, QueryServiceStatusEx, SC_HANDLE, SC_STATUS_PROCESS_INFO,
    SERVICE_NO_CHANGE, SERVICE_STATUS_PROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use super::supervisor::{self, OwnedHandle};

/// #3115 — the longest the host takes, by its own construction, from the Stop
/// it is sent to the exit of its process: the supervisor's whole shutdown
/// ([`supervisor::SHUTDOWN_BOUND`]: a poll tick, the worker's graceful budget,
/// the terminate and netd's reap), then the runtime the process lets go after
/// it has reported STOPPED ([`crate::runtime_exit::SHUTDOWN_BUDGET`],
/// `app.rs`'s `shut_down_runtime`).
pub const HOST_STOP_BOUND: Duration =
    supervisor::SHUTDOWN_BOUND.saturating_add(crate::runtime_exit::SHUTDOWN_BUDGET);

/// #3115 — how long a retire waits for the service it stopped to be gone,
/// STOPPED and its process exited: the host's own bound twice over, for a
/// loaded box (Defender scanning a fresh EXE, an EDR hooking every process
/// exit, two vCPUs busy with the MSI). Master's fixed 5 s sat INSIDE the
/// worker's 8 s graceful budget.
pub const STOP_WAIT_BOUND: Duration = HOST_STOP_BOUND.saturating_mul(2);

/// #3115 — after the delete, how long a retire waits for the entry to go
/// (1060). The SCM removes a stopped, deleted entry when its last handle
/// closes, so this outlasts a brief open elsewhere (a status poll); a handle
/// held for good (services.msc on its properties) outlasts any bound, and the
/// retire says so instead.
pub const GONE_WAIT_BOUND: Duration = Duration::from_secs(10);

/// #3115 — how long [`register`] waits for an entry an earlier delete marked
/// (1072) to go. That delete may have been made while the host was still
/// stopping (an old product's uninstall, whose wait was 5 s), so it is a
/// whole stop wait, plus the settle after it.
pub const MARKED_WAIT_BOUND: Duration = STOP_WAIT_BOUND.saturating_add(GONE_WAIT_BOUND);

/// How often a wait asks the SCM again (master's loop used the same step).
const POLL: Duration = Duration::from_millis(100);

/// The bounds the waits run with: the production constants, or a test's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// From the stop to STOPPED and the host's exit ([`STOP_WAIT_BOUND`]).
    pub stop: Duration,
    /// From the delete to 1060 ([`GONE_WAIT_BOUND`]).
    pub gone: Duration,
    /// For a 1072 found by [`register`] ([`MARKED_WAIT_BOUND`]).
    pub marked: Duration,
    /// The step between two questions.
    pub poll: Duration,
}

impl Bounds {
    pub const PRODUCTION: Bounds = Bounds {
        stop: STOP_WAIT_BOUND,
        gone: GONE_WAIT_BOUND,
        marked: MARKED_WAIT_BOUND,
        poll: POLL,
    };
}

/// The SCM answers this module acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmError {
    /// 1060 `ERROR_SERVICE_DOES_NOT_EXIST`.
    Absent,
    /// 1072 `ERROR_SERVICE_MARKED_FOR_DELETE`: deleted, and still held (its
    /// host still running, or a handle open somewhere).
    MarkedForDelete,
    /// 1073 `ERROR_SERVICE_EXISTS`.
    Exists,
    /// Any other Win32 error; -1 when the failure was not a Win32 one.
    Other(i32),
}

impl ScmError {
    pub fn from_code(code: i32) -> Self {
        match code {
            1060 => ScmError::Absent,
            1072 => ScmError::MarkedForDelete,
            1073 => ScmError::Exists,
            other => ScmError::Other(other),
        }
    }

    fn of(e: &windows_service::Error) -> Self {
        match e {
            windows_service::Error::Winapi(io) => io
                .raw_os_error()
                .map_or(ScmError::Other(-1), ScmError::from_code),
            _ => ScmError::Other(-1),
        }
    }
}

impl fmt::Display for ScmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScmError::Absent => f.write_str("not registered (1060)"),
            ScmError::MarkedForDelete => f.write_str("marked for deletion (1072)"),
            ScmError::Exists => f.write_str("already registered (1073)"),
            ScmError::Other(-1) => f.write_str("refused (not a Win32 error)"),
            ScmError::Other(code) => write!(f, "{}", std::io::Error::from_raw_os_error(*code)),
        }
    }
}

/// A service's state, as `dwCurrentState` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
}

impl State {
    fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => State::Stopped,
            2 => State::StartPending,
            3 => State::StopPending,
            4 => State::Running,
            5 => State::ContinuePending,
            6 => State::PausePending,
            7 => State::Paused,
            _ => return None,
        })
    }
}

/// What one status query says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// The process hosting it, while there is one. `QueryServiceStatusEx`
    /// reports it in every state but STOPPED; `windows-service`'s own query
    /// reports it only while RUNNING, which is why the live seam reads it raw.
    pub pid: Option<u32>,
}

/// What a service handle is opened for: each asks only for the rights it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Query, stop and delete it.
    Retire,
    /// Query it and rewrite its configuration. `START` too: the restart that a
    /// recovery action names may only be set through a handle that could start
    /// the service.
    Configure,
    /// See whether it is there and whether it is marked for deletion
    /// ([`Scm::is_marked`] needs `CHANGE_CONFIG`).
    Probe,
}

/// #3115 — the SCM calls the service lifecycle makes. The production one is
/// [`LiveScm`]; the tests drive a scripted SCM on a virtual clock.
pub trait Scm {
    /// An open service handle; dropping it closes the handle.
    type Service;
    /// An open process handle that can be waited on; dropping it closes it.
    type Process;

    /// ⚠️ `OpenService` can SUCCEED on an entry an earlier delete marked: on a
    /// Win11 cell, a major upgrade's RegisterService opened the service the old
    /// product had just deleted (#3115). Only a change is refused (1072), so a
    /// handle is not proof that the service is registered ([`Scm::is_marked`]).
    fn open(&self, name: &str, purpose: Purpose) -> Result<Self::Service, ScmError>;
    fn query(&self, service: &Self::Service) -> Result<Status, ScmError>;
    /// Whether an open service is marked for deletion: a change that changes
    /// nothing, which the SCM refuses with 1072 on a marked entry.
    fn is_marked(&self, service: &Self::Service) -> Result<bool, ScmError>;
    fn stop(&self, service: &Self::Service) -> Result<(), ScmError>;
    fn delete(&self, service: &Self::Service) -> Result<(), ScmError>;
    /// Create the service, with its description and recovery actions.
    fn create(&self, info: &ServiceInfo) -> Result<Self::Service, ScmError>;
    /// Rewrite an existing service's configuration, description and recovery
    /// actions, and repair its `Environment` value if it is malformed.
    fn reconfigure(&self, service: &Self::Service, info: &ServiceInfo) -> Result<(), ScmError>;
    /// Open process `pid` for waiting; `None` when it cannot be opened.
    fn open_process(&self, pid: u32) -> Option<Self::Process>;
    /// Wait up to `timeout` for the process to exit; true once it has.
    fn wait_process(&self, process: &Self::Process, timeout: Duration) -> bool;
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

/// How [`stop_and_wait`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopping {
    /// The state it was in when the wait began.
    pub from: Option<State>,
    /// It reached STOPPED within the bound.
    pub stopped: bool,
    /// Its host process was seen to exit. False when there was a process and
    /// it outlived the bound, and when there was none that could be watched.
    pub exited: bool,
    /// From the start of the wait to its end.
    pub took: Duration,
}

/// Stop `service` and wait, within `bounds.stop`, for STOPPED and then for its
/// host process to exit. The process handle is taken BEFORE the stop, so it
/// names this host even if its pid is reused after it exits.
pub fn stop_and_wait<S: Scm>(scm: &S, service: &S::Service, bounds: Bounds) -> Stopping {
    let started = scm.now();
    let deadline = started + bounds.stop;
    let first = scm.query(service).ok();
    let from = first.map(|s| s.state);
    if from == Some(State::Stopped) {
        return Stopping {
            from,
            stopped: true,
            exited: false,
            took: Duration::ZERO,
        };
    }
    let mut process = first
        .and_then(|s| s.pid)
        .and_then(|pid| scm.open_process(pid));
    // A service still starting takes no controls (1061); it is asked again
    // once it runs.
    let mut asked = scm.stop(service).is_ok();
    let mut stopped = false;
    loop {
        match scm.query(service) {
            Ok(s) if s.state == State::Stopped => {
                stopped = true;
                break;
            }
            Ok(s) => {
                if process.is_none() {
                    process = s.pid.and_then(|pid| scm.open_process(pid));
                }
                if !asked && matches!(s.state, State::Running | State::Paused) {
                    asked = scm.stop(service).is_ok();
                }
            }
            Err(_) => {}
        }
        let now = scm.now();
        if now >= deadline {
            break;
        }
        scm.sleep(bounds.poll.min(deadline - now));
    }
    let exited = match &process {
        Some(p) if stopped => scm.wait_process(p, deadline.saturating_duration_since(scm.now())),
        _ => false,
    };
    Stopping {
        from,
        stopped,
        exited,
        took: scm.now().saturating_duration_since(started),
    }
}

/// Whether a name is registered, once a wait is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// 1060: no entry by that name.
    Gone,
    /// 1072 still, at the deadline.
    Marked,
    /// An entry that is not marked: registered again in the meantime.
    Present,
}

/// Wait up to `timeout` for `name` to be gone (1060), or registered again and
/// not marked. Each look opens a fresh handle and closes it before the next
/// sleep: a handle held across the wait would itself keep the entry alive.
pub fn wait_gone<S: Scm>(scm: &S, name: &str, timeout: Duration, poll: Duration) -> Presence {
    let deadline = scm.now() + timeout;
    loop {
        match scm.open(name, Purpose::Probe) {
            Err(ScmError::Absent) => return Presence::Gone,
            // An open can succeed on a marked entry (see `Scm::open`).
            Ok(service) => {
                if scm.is_marked(&service) == Ok(false) {
                    return Presence::Present;
                }
            }
            // Marked, or a transient refusal: ask again until the deadline.
            Err(_) => {}
        }
        let now = scm.now();
        if now >= deadline {
            return Presence::Marked;
        }
        scm.sleep(poll.min(deadline - now));
    }
}

/// How [`retire`] left a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retired {
    /// It was not registered.
    Absent,
    /// Stopped, its host gone, deleted, and the entry gone.
    Removed { stopping: Option<Stopping> },
    /// Deleted once stopped, but a handle elsewhere still holds the entry; the
    /// SCM removes it when that handle closes.
    Held { stopping: Option<Stopping> },
    /// It did not stop within the bound. Deleted anyway, because a retire wants
    /// it gone: the SCM removes it when its host finally exits, or at the next
    /// boot.
    DeletedWhileRunning { stopping: Stopping },
}

/// #3115 — stop `name`, wait for it and its host to be gone, delete it, and
/// wait for the entry to go. For an uninstall and for the legacy service: an
/// install never retires the service it is about to register ([`register`]).
pub fn retire<S: Scm>(scm: &S, name: &str, bounds: Bounds) -> Result<Retired> {
    let service = match scm.open(name, Purpose::Retire) {
        Ok(s) => s,
        Err(ScmError::Absent) => return Ok(Retired::Absent),
        // An earlier delete is completing: wait for it, as a retire would.
        Err(ScmError::MarkedForDelete) => {
            return Ok(
                match wait_gone(scm, name, bounds.stop + bounds.gone, bounds.poll) {
                    Presence::Marked => Retired::Held { stopping: None },
                    Presence::Gone | Presence::Present => Retired::Removed { stopping: None },
                },
            );
        }
        Err(e) => bail!("opening the {name} service to remove it: {e}"),
    };
    let stopping = stop_and_wait(scm, &service, bounds);
    match scm.delete(&service) {
        Ok(()) | Err(ScmError::MarkedForDelete) => {}
        Err(e) => bail!("deleting the {name} service: {e}"),
    }
    // Ours is a handle too: the entry cannot go while it is open.
    drop(service);
    if !stopping.stopped {
        return Ok(Retired::DeletedWhileRunning { stopping });
    }
    Ok(match wait_gone(scm, name, bounds.gone, bounds.poll) {
        Presence::Marked => Retired::Held {
            stopping: Some(stopping),
        },
        // Present: our entry went, and an install registered a new one.
        Presence::Gone | Presence::Present => Retired::Removed {
            stopping: Some(stopping),
        },
    })
}

/// How [`register`] left the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registered {
    /// It was not registered: created.
    Created,
    /// An earlier delete still held the name (1072): created once it had gone.
    CreatedAfterDelete,
    /// It was registered: its configuration rewritten in place. A running
    /// service keeps running; the change applies from its next start.
    Reconfigured,
}

/// #3115 — make the service `info` describes registered, with that
/// configuration, never by deleting it: an existing entry is reconfigured in
/// place, a missing one created, and one an earlier delete marked (1072) is
/// waited out first, within `bounds.marked`.
pub fn register<S: Scm>(scm: &S, info: &ServiceInfo, bounds: Bounds) -> Result<Registered> {
    let name = info.name.to_string_lossy().into_owned();
    let deadline = scm.now() + bounds.marked;
    let mut waited = false;
    // A 1072 or a 1073 sends the loop round again. Four rounds cover every
    // order those can come in; an SCM still changing after them is reported.
    for _ in 0..4 {
        match scm.open(&name, Purpose::Configure) {
            Ok(service) => match scm.reconfigure(&service, info) {
                Ok(()) => return Ok(Registered::Reconfigured),
                // The open reached an entry an earlier delete marked (see
                // `Scm::open`): it cannot be reconfigured, only waited out.
                // Our handle goes first, or the entry can never go.
                Err(ScmError::MarkedForDelete) => drop(service),
                Err(e) => bail!("reconfiguring the {name} service in place: {e}"),
            },
            Err(ScmError::Absent) => match scm.create(info) {
                Ok(_) => {
                    return Ok(if waited {
                        Registered::CreatedAfterDelete
                    } else {
                        Registered::Created
                    });
                }
                // Registered between the open and the create: reconfigure it.
                Err(ScmError::Exists) => continue,
                Err(ScmError::MarkedForDelete) => {}
                Err(e) => bail!("creating the {name} service: {e}"),
            },
            Err(ScmError::MarkedForDelete) => {}
            Err(e) => bail!("opening the {name} service: {e}"),
        }
        // 1072: wait for the marked entry to go, then go round again.
        waited = true;
        let left = deadline.saturating_duration_since(scm.now());
        if wait_gone(scm, &name, left, bounds.poll) == Presence::Marked {
            bail!(
                "the {name} service is still marked for deletion after {} s: an earlier delete \
                 is held by its host still stopping, or by a handle open elsewhere (services.msc, \
                 a monitoring tool). Windows removes it once that lets go; run the install again \
                 then",
                bounds.marked.as_secs()
            );
        }
    }
    bail!("the {name} service kept changing while it was being registered")
}

/// The production [`Scm`]: the local Service Control Manager.
pub struct LiveScm {
    manager: ServiceManager,
}

impl LiveScm {
    /// Connect with `access`: `CREATE_SERVICE` too for [`register`], which
    /// needs an elevated (or SYSTEM) caller; `CONNECT` is enough to retire.
    pub fn connect(access: ServiceManagerAccess) -> Result<Self> {
        let manager = ServiceManager::local_computer(None::<&str>, access)
            .context("opening the Service Control Manager")?;
        Ok(Self { manager })
    }

    /// The description and recovery actions, on a service just created or
    /// reconfigured. Best-effort: a refusal is logged and never sinks the
    /// registration.
    fn describe(service: &Service, name: &str) {
        if let Err(e) = service.set_description(super::SERVICE_DESCRIPTION) {
            tracing::warn!(error = %e, service = name, "could not set the service's description");
        }
        // #2087 — without these a host process that dies stays dead until a
        // reboot.
        if let Err(e) = super::apply_recovery_actions(service) {
            tracing::warn!(
                error = %format!("{e:#}"),
                service = name,
                "could not set the service's recovery actions"
            );
        }
    }
}

impl Scm for LiveScm {
    type Service = Service;
    type Process = OwnedHandle;

    fn open(&self, name: &str, purpose: Purpose) -> Result<Service, ScmError> {
        let access = match purpose {
            Purpose::Retire => {
                ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE
            }
            Purpose::Configure => {
                ServiceAccess::QUERY_STATUS | ServiceAccess::CHANGE_CONFIG | ServiceAccess::START
            }
            Purpose::Probe => ServiceAccess::QUERY_STATUS | ServiceAccess::CHANGE_CONFIG,
        };
        self.manager
            .open_service(name, access)
            .map_err(|e| ScmError::of(&e))
    }

    fn query(&self, service: &Service) -> Result<Status, ScmError> {
        // SAFETY: zeroed is a valid SERVICE_STATUS_PROCESS (all integers).
        let mut raw: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
        let mut needed = 0u32;
        // `windows-service` 0.7 builds on windows-sys 0.52, whose SC_HANDLE is
        // an isize; ours (0.59) is a pointer. Same handle, two spellings.
        let handle = service.raw_handle() as SC_HANDLE;
        // SAFETY: a live service handle opened with QUERY_STATUS, and a
        // buffer of exactly the size passed.
        let ok = unsafe {
            QueryServiceStatusEx(
                handle,
                SC_STATUS_PROCESS_INFO,
                (&mut raw as *mut SERVICE_STATUS_PROCESS).cast::<u8>(),
                std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            return Err(ScmError::from_code(code));
        }
        let state = State::from_raw(raw.dwCurrentState).ok_or(ScmError::Other(-1))?;
        Ok(Status {
            state,
            pid: (raw.dwProcessId != 0).then_some(raw.dwProcessId),
        })
    }

    fn is_marked(&self, service: &Service) -> Result<bool, ScmError> {
        // SAFETY: a live service handle opened with CHANGE_CONFIG. Every field
        // is SERVICE_NO_CHANGE or NULL, so nothing is changed.
        let ok = unsafe {
            ChangeServiceConfigW(
                service.raw_handle() as SC_HANDLE,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if ok != 0 {
            return Ok(false);
        }
        match std::io::Error::last_os_error()
            .raw_os_error()
            .map(ScmError::from_code)
        {
            Some(ScmError::MarkedForDelete) => Ok(true),
            Some(e) => Err(e),
            None => Err(ScmError::Other(-1)),
        }
    }

    fn stop(&self, service: &Service) -> Result<(), ScmError> {
        service.stop().map(|_| ()).map_err(|e| ScmError::of(&e))
    }

    fn delete(&self, service: &Service) -> Result<(), ScmError> {
        service.delete().map_err(|e| ScmError::of(&e))
    }

    fn create(&self, info: &ServiceInfo) -> Result<Service, ScmError> {
        let service = self
            .manager
            .create_service(
                info,
                ServiceAccess::QUERY_STATUS | ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
            )
            .map_err(|e| ScmError::of(&e))?;
        Self::describe(&service, &info.name.to_string_lossy());
        Ok(service)
    }

    fn reconfigure(&self, service: &Service, info: &ServiceInfo) -> Result<(), ScmError> {
        let name = info.name.to_string_lossy();
        service.change_config(info).map_err(|e| ScmError::of(&e))?;
        Self::describe(service, &name);
        match super::environment::normalize_service_env(&name) {
            Ok(super::environment::EnvRepair::Keep) => {}
            Ok(repair) => tracing::warn!(
                ?repair,
                service = %name,
                "the service's Environment value was malformed; repaired (P3d: an empty one fails \
                 StartServiceW with error 87)"
            ),
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                service = %name,
                "could not check the service's Environment value"
            ),
        }
        Ok(())
    }

    fn open_process(&self, pid: u32) -> Option<OwnedHandle> {
        // SAFETY: a plain open for SYNCHRONIZE; the handle is owned below.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        OwnedHandle::new(raw)
    }

    fn wait_process(&self, process: &OwnedHandle, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: a live process handle opened with SYNCHRONIZE.
        unsafe { WaitForSingleObject(process.raw(), ms) == WAIT_OBJECT_0 }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::rc::Rc;
    use windows_service::service::{ServiceErrorControl, ServiceStartType, ServiceType};

    const NAME: &str = "Roomler";

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    /// The field case (#3115): the worker needed 7.8 s of its 8 s budget, and
    /// the host's process went a moment after it reported STOPPED.
    const FIELD_STOP: Duration = Duration::from_millis(7_800);
    const FIELD_LINGER: Duration = Duration::from_millis(300);

    /// What the scripted SCM saw, in order, on its virtual clock.
    #[derive(Debug, Clone, PartialEq)]
    enum Event {
        Stop {
            at: Duration,
        },
        Delete {
            at: Duration,
            state: State,
            host_alive: bool,
        },
        Removed {
            at: Duration,
        },
        Create {
            at: Duration,
            answer: Result<(), ScmError>,
        },
        Reconfigure {
            at: Duration,
            state: State,
        },
    }

    /// How the one service the tests care about stops.
    #[derive(Debug, Clone, Copy)]
    struct StopModel {
        /// From the stop to STOPPED; `None`: never.
        takes: Option<Duration>,
        /// From STOPPED to its host's exit; `None`: never.
        lingers: Option<Duration>,
    }

    #[derive(Debug)]
    struct Entry {
        state: State,
        /// What a status query reports: the host's pid, 0 (`None`) once STOPPED.
        pid: Option<u32>,
        /// The process that hosts it, which can outlive its STOPPED report.
        host: Option<u32>,
        marked: bool,
        /// Handles this process holds on it.
        handles: u32,
        /// A handle another process holds, until then (`None`: none).
        held_elsewhere_until: Option<Duration>,
        start_pending_until: Option<Duration>,
        stop: StopModel,
        stopped_at: Option<Duration>,
    }

    #[derive(Debug, Default)]
    struct Inner {
        now: Duration,
        entry: Option<Entry>,
        /// pid → when that process exits (`None`: never).
        exits: HashMap<u32, Option<Duration>>,
        events: Vec<Event>,
        /// Answers `create` gives before it simulates (front first).
        create_script: VecDeque<ScmError>,
        /// Whether `open` succeeds on a marked entry. The field saw both: the
        /// laptop's `OpenService` refused one with 1072, a Win11 cell's
        /// upgrade opened one (#3115).
        open_marked_ok: bool,
    }

    impl Inner {
        /// Bring the service up to the virtual clock.
        fn tick(&mut self) {
            let now = self.now;
            let mut removed = false;
            if let Some(e) = self.entry.as_mut() {
                if e.state == State::StartPending && e.start_pending_until.is_some_and(|t| now >= t)
                {
                    e.state = State::Running;
                }
                if e.state == State::StopPending && e.stopped_at.is_some_and(|t| now >= t) {
                    e.state = State::Stopped;
                    // dwProcessId is 0 once STOPPED.
                    e.pid = None;
                }
                let held = e.held_elsewhere_until.is_some_and(|t| now < t);
                removed = e.marked && e.state == State::Stopped && e.handles == 0 && !held;
            }
            if removed {
                self.entry = None;
                self.events.push(Event::Removed { at: now });
            }
        }

        fn host_alive(&self, host: Option<u32>) -> bool {
            host.and_then(|p| self.exits.get(&p).copied())
                .is_some_and(|exit| exit.is_none_or(|t| self.now < t))
        }
    }

    #[derive(Clone)]
    struct FakeScm {
        inner: Rc<RefCell<Inner>>,
        base: Instant,
    }

    struct FakeHandle {
        inner: Rc<RefCell<Inner>>,
    }

    impl Drop for FakeHandle {
        fn drop(&mut self) {
            let mut i = self.inner.borrow_mut();
            if let Some(e) = i.entry.as_mut() {
                e.handles -= 1;
            }
            i.tick();
        }
    }

    struct FakeProcess {
        pid: u32,
    }

    const HOST_PID: u32 = 4242;

    impl FakeScm {
        fn with(entry: Option<Entry>) -> Self {
            let mut inner = Inner {
                entry,
                ..Inner::default()
            };
            if inner.entry.as_ref().is_some_and(|e| e.host.is_some()) {
                inner.exits.insert(HOST_PID, None);
            }
            Self {
                inner: Rc::new(RefCell::new(inner)),
                base: Instant::now(),
            }
        }

        /// Registered and running, stopping as `stop` says.
        fn running(stop: StopModel) -> Self {
            Self::with(Some(Entry {
                state: State::Running,
                pid: Some(HOST_PID),
                host: Some(HOST_PID),
                marked: false,
                handles: 0,
                held_elsewhere_until: None,
                start_pending_until: None,
                stop,
                stopped_at: None,
            }))
        }

        fn absent() -> Self {
            Self::with(None)
        }

        /// The upgrade path: an old product's uninstall (master's 5 s wait)
        /// deleted the service while its host was still stopping, `stops_in`
        /// before it reports STOPPED.
        fn marked_while_stopping(stops_in: Duration) -> Self {
            let fake = Self::with(Some(Entry {
                state: State::StopPending,
                pid: Some(HOST_PID),
                host: Some(HOST_PID),
                marked: true,
                handles: 0,
                held_elsewhere_until: None,
                start_pending_until: None,
                stop: StopModel {
                    takes: Some(stops_in),
                    lingers: Some(FIELD_LINGER),
                },
                stopped_at: Some(stops_in),
            }));
            fake.inner
                .borrow_mut()
                .exits
                .insert(HOST_PID, Some(stops_in + FIELD_LINGER));
            fake
        }

        fn events(&self) -> Vec<Event> {
            self.inner.borrow().events.clone()
        }

        fn elapsed(&self) -> Duration {
            self.inner.borrow().now
        }

        fn registered_state(&self) -> Option<(State, bool)> {
            let mut i = self.inner.borrow_mut();
            i.tick();
            i.entry.as_ref().map(|e| (e.state, e.marked))
        }

        fn handle(&self) -> FakeHandle {
            FakeHandle {
                inner: Rc::clone(&self.inner),
            }
        }
    }

    impl Scm for FakeScm {
        type Service = FakeHandle;
        type Process = FakeProcess;

        fn open(&self, name: &str, _purpose: Purpose) -> Result<FakeHandle, ScmError> {
            assert_eq!(name, NAME);
            let mut i = self.inner.borrow_mut();
            i.tick();
            let open_marked_ok = i.open_marked_ok;
            match i.entry.as_mut() {
                None => Err(ScmError::Absent),
                Some(e) if e.marked && !open_marked_ok => Err(ScmError::MarkedForDelete),
                Some(e) => {
                    e.handles += 1;
                    drop(i);
                    Ok(self.handle())
                }
            }
        }

        fn query(&self, _service: &FakeHandle) -> Result<Status, ScmError> {
            let mut i = self.inner.borrow_mut();
            i.tick();
            let e = i.entry.as_ref().expect("a handle keeps the entry");
            Ok(Status {
                state: e.state,
                pid: e.pid,
            })
        }

        fn stop(&self, _service: &FakeHandle) -> Result<(), ScmError> {
            let mut i = self.inner.borrow_mut();
            i.tick();
            let now = i.now;
            i.events.push(Event::Stop { at: now });
            let e = i.entry.as_mut().expect("a handle keeps the entry");
            match e.state {
                // ERROR_SERVICE_CANNOT_ACCEPT_CTRL / ERROR_SERVICE_NOT_ACTIVE.
                State::StartPending | State::StopPending => Err(ScmError::Other(1061)),
                State::Stopped => Err(ScmError::Other(1062)),
                _ => {
                    e.state = State::StopPending;
                    let stopped_at = e.stop.takes.map(|t| now + t);
                    e.stopped_at = stopped_at;
                    let exit = stopped_at.and_then(|s| e.stop.lingers.map(|l| s + l));
                    let host = e.host;
                    if let Some(pid) = host {
                        i.exits.insert(pid, exit);
                    }
                    Ok(())
                }
            }
        }

        fn delete(&self, _service: &FakeHandle) -> Result<(), ScmError> {
            let mut i = self.inner.borrow_mut();
            i.tick();
            let now = i.now;
            let (state, host, marked) = {
                let e = i.entry.as_ref().expect("a handle keeps the entry");
                (e.state, e.host, e.marked)
            };
            let host_alive = i.host_alive(host);
            i.events.push(Event::Delete {
                at: now,
                state,
                host_alive,
            });
            if marked {
                return Err(ScmError::MarkedForDelete);
            }
            i.entry.as_mut().expect("present").marked = true;
            Ok(())
        }

        fn create(&self, _info: &ServiceInfo) -> Result<FakeHandle, ScmError> {
            let mut i = self.inner.borrow_mut();
            i.tick();
            let now = i.now;
            let answer = if let Some(scripted) = i.create_script.pop_front() {
                if scripted == ScmError::Exists {
                    // A racing install registered it between our open and
                    // our create.
                    i.entry = Some(Entry {
                        state: State::Stopped,
                        pid: None,
                        host: None,
                        marked: false,
                        handles: 0,
                        held_elsewhere_until: None,
                        start_pending_until: None,
                        stop: StopModel {
                            takes: Some(Duration::ZERO),
                            lingers: Some(Duration::ZERO),
                        },
                        stopped_at: None,
                    });
                }
                Err(scripted)
            } else {
                match i.entry.as_ref() {
                    Some(e) if e.marked => Err(ScmError::MarkedForDelete),
                    Some(_) => Err(ScmError::Exists),
                    None => Ok(()),
                }
            };
            i.events.push(Event::Create { at: now, answer });
            answer?;
            i.entry = Some(Entry {
                state: State::Stopped,
                pid: None,
                host: None,
                marked: false,
                handles: 1,
                held_elsewhere_until: None,
                start_pending_until: None,
                stop: StopModel {
                    takes: Some(Duration::ZERO),
                    lingers: Some(Duration::ZERO),
                },
                stopped_at: None,
            });
            drop(i);
            Ok(self.handle())
        }

        fn reconfigure(&self, _service: &FakeHandle, _info: &ServiceInfo) -> Result<(), ScmError> {
            let mut i = self.inner.borrow_mut();
            i.tick();
            let now = i.now;
            let (state, marked) = {
                let e = i.entry.as_ref().expect("a handle keeps the entry");
                (e.state, e.marked)
            };
            if marked {
                return Err(ScmError::MarkedForDelete);
            }
            i.events.push(Event::Reconfigure { at: now, state });
            Ok(())
        }

        fn is_marked(&self, _service: &FakeHandle) -> Result<bool, ScmError> {
            let mut i = self.inner.borrow_mut();
            i.tick();
            Ok(i.entry.as_ref().expect("a handle keeps the entry").marked)
        }

        fn open_process(&self, pid: u32) -> Option<FakeProcess> {
            let i = self.inner.borrow();
            i.host_alive(Some(pid)).then_some(FakeProcess { pid })
        }

        fn wait_process(&self, process: &FakeProcess, timeout: Duration) -> bool {
            let mut i = self.inner.borrow_mut();
            let exit = i.exits.get(&process.pid).copied().flatten();
            let now = i.now;
            match exit {
                Some(t) if t <= now + timeout => {
                    i.now = now.max(t);
                    i.tick();
                    true
                }
                _ => {
                    i.now = now + timeout;
                    i.tick();
                    false
                }
            }
        }

        fn now(&self) -> Instant {
            self.base + self.inner.borrow().now
        }

        fn sleep(&self, d: Duration) {
            let mut i = self.inner.borrow_mut();
            i.now += d;
            i.tick();
        }
    }

    fn info() -> ServiceInfo {
        ServiceInfo {
            name: OsString::from(NAME),
            display_name: OsString::from(NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: PathBuf::from(r"C:\Program Files\Roomler\roomlerd.exe"),
            launch_arguments: vec![OsString::from("service-run")],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        }
    }

    /// Master's `install`, step for step over the same seam: stop, 50 × 100 ms
    /// for STOPPED, delete whatever the state, create. Kept as the control that
    /// shows the scripted SCM reproduces the field failure.
    fn masters_install(scm: &FakeScm) -> Result<(), ScmError> {
        if let Ok(s) = scm.open(NAME, Purpose::Retire) {
            if scm.query(&s).map(|st| st.state) != Ok(State::Stopped) {
                let _ = scm.stop(&s);
                for _ in 0..50 {
                    scm.sleep(Duration::from_millis(100));
                    if scm.query(&s).map(|st| st.state) == Ok(State::Stopped) {
                        break;
                    }
                }
            }
            let _ = scm.delete(&s);
        }
        scm.create(&info()).map(|_| ())
    }

    fn deletes(events: &[Event]) -> Vec<(State, bool)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Delete {
                    state, host_alive, ..
                } => Some((*state, *host_alive)),
                _ => None,
            })
            .collect()
    }

    /// #3115 — the control: master's 5 s wait loses to the field's 7.8 s stop.
    /// It deletes a service that is still stopping, the create answers 1072,
    /// and once the host exits there is no service at all.
    #[test]
    fn masters_install_deleted_the_field_service_while_it_was_still_stopping() {
        let scm = FakeScm::running(StopModel {
            takes: Some(FIELD_STOP),
            lingers: Some(FIELD_LINGER),
        });
        assert_eq!(masters_install(&scm), Err(ScmError::MarkedForDelete));
        assert_eq!(deletes(&scm.events()), [(State::StopPending, true)]);
        // The host finishes its stop; the SCM removes the marked entry.
        scm.sleep(secs(10.0));
        assert_eq!(scm.registered_state(), None, "the machine has no service");
    }

    /// #3115 — the field's 7.8 s stop is waited out: the delete comes only once
    /// the service is STOPPED and its host has exited, and the entry is gone.
    #[test]
    fn a_retire_waits_out_the_fields_seven_point_eight_second_stop() {
        let scm = FakeScm::running(StopModel {
            takes: Some(FIELD_STOP),
            lingers: Some(FIELD_LINGER),
        });
        let retired = retire(&scm, NAME, Bounds::PRODUCTION).expect("retire");
        let Retired::Removed {
            stopping: Some(stopping),
        } = retired
        else {
            panic!("{retired:?}");
        };
        assert!(stopping.stopped && stopping.exited, "{stopping:?}");
        assert_eq!(stopping.from, Some(State::Running));
        assert_eq!(deletes(&scm.events()), [(State::Stopped, false)]);
        assert!(
            scm.elapsed() >= FIELD_STOP + FIELD_LINGER,
            "{:?}",
            scm.elapsed()
        );
        assert_eq!(scm.registered_state(), None);
    }

    /// The same retire with master's 5 s for the stop wait deletes the service
    /// while it is still stopping: the bound, not the algorithm, is what loses.
    #[test]
    fn the_same_retire_with_masters_five_seconds_deletes_it_while_running() {
        let scm = FakeScm::running(StopModel {
            takes: Some(FIELD_STOP),
            lingers: Some(FIELD_LINGER),
        });
        let five = Bounds {
            stop: Duration::from_secs(5),
            ..Bounds::PRODUCTION
        };
        let retired = retire(&scm, NAME, five).expect("retire");
        assert!(
            matches!(retired, Retired::DeletedWhileRunning { .. }),
            "{retired:?}"
        );
        assert_eq!(deletes(&scm.events()), [(State::StopPending, true)]);
    }

    /// #3115 — the maintenance run the field lost: an install over a running
    /// service reconfigures it in place. Nothing stops it, nothing deletes it,
    /// and it is still running after.
    #[test]
    fn an_install_reconfigures_a_running_service_in_place() {
        let scm = FakeScm::running(StopModel {
            takes: Some(FIELD_STOP),
            lingers: Some(FIELD_LINGER),
        });
        let registered = register(&scm, &info(), Bounds::PRODUCTION).expect("register");
        assert_eq!(registered, Registered::Reconfigured);
        assert_eq!(
            scm.events(),
            [Event::Reconfigure {
                at: Duration::ZERO,
                state: State::Running
            }]
        );
        assert_eq!(scm.registered_state(), Some((State::Running, false)));
    }

    /// #3115 — the upgrade path: the old product's uninstall (master's 5 s)
    /// deleted the service while its host was still stopping. The new install
    /// waits for the marked entry to go, then creates the service.
    ///
    /// Both answers the field gave to an open of the marked entry: refused with
    /// 1072 (the laptop), or a handle on which only a change is refused (the
    /// cell's major upgrade). The second used to read as "registered", and a
    /// reconfigure in place would have failed the install.
    #[test]
    fn an_install_waits_out_an_entry_an_old_uninstall_marked() {
        for open_marked_ok in [false, true] {
            // Deleted at 5 s of a 7.8 s stop: 2.8 s still to go.
            let scm = FakeScm::marked_while_stopping(secs(2.8));
            scm.inner.borrow_mut().open_marked_ok = open_marked_ok;
            let registered = register(&scm, &info(), Bounds::PRODUCTION).expect("register");
            assert_eq!(
                registered,
                Registered::CreatedAfterDelete,
                "{open_marked_ok}"
            );
            let events = scm.events();
            let removed = events
                .iter()
                .position(|e| matches!(e, Event::Removed { .. }))
                .expect("the marked entry went");
            let created = events
                .iter()
                .position(|e| matches!(e, Event::Create { answer: Ok(()), .. }))
                .expect("created");
            assert!(removed < created, "{open_marked_ok}: {events:?}");
            assert_eq!(scm.registered_state(), Some((State::Stopped, false)));
        }
    }

    /// A retire whose open reaches an entry an earlier delete marked waits for
    /// its stop like any other, and the entry then goes.
    #[test]
    fn a_retire_that_opens_a_marked_entry_waits_it_out() {
        let scm = FakeScm::marked_while_stopping(secs(2.8));
        scm.inner.borrow_mut().open_marked_ok = true;
        let retired = retire(&scm, NAME, Bounds::PRODUCTION).expect("retire");
        assert!(matches!(retired, Retired::Removed { .. }), "{retired:?}");
        assert_eq!(deletes(&scm.events()), [(State::Stopped, false)]);
        assert_eq!(scm.registered_state(), None);
    }

    /// #3115 — a 1072 from the create itself (the entry was deleted between
    /// our open and our create) is waited out, and the create then succeeds.
    #[test]
    fn a_1072_on_create_is_waited_out_and_the_create_succeeds() {
        let scm = FakeScm::absent();
        scm.inner
            .borrow_mut()
            .create_script
            .push_back(ScmError::MarkedForDelete);
        let registered = register(&scm, &info(), Bounds::PRODUCTION).expect("register");
        assert_eq!(registered, Registered::CreatedAfterDelete);
        let creates: Vec<_> = scm
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Create { answer, .. } => Some(answer),
                _ => None,
            })
            .collect();
        assert_eq!(creates, [Err(ScmError::MarkedForDelete), Ok(())]);
    }

    /// A 1073 from the create (a racing install registered it between our open
    /// and our create) is reconfigured in place, never deleted.
    #[test]
    fn a_1073_on_create_is_reconfigured_in_place() {
        let scm = FakeScm::absent();
        scm.inner
            .borrow_mut()
            .create_script
            .push_back(ScmError::Exists);
        let registered = register(&scm, &info(), Bounds::PRODUCTION).expect("register");
        assert_eq!(registered, Registered::Reconfigured);
        assert!(deletes(&scm.events()).is_empty());
        assert_eq!(scm.registered_state(), Some((State::Stopped, false)));
    }

    /// #3115 — a name that is not registered (1060) is nothing to retire, any
    /// number of times, and an install creates it.
    #[test]
    fn a_service_that_is_not_registered_is_nothing_to_retire() {
        let scm = FakeScm::absent();
        assert_eq!(
            retire(&scm, NAME, Bounds::PRODUCTION).unwrap(),
            Retired::Absent
        );
        assert_eq!(
            retire(&scm, NAME, Bounds::PRODUCTION).unwrap(),
            Retired::Absent
        );
        assert!(scm.events().is_empty());
        assert_eq!(scm.elapsed(), Duration::ZERO, "no wait for a 1060");
        assert_eq!(
            register(&scm, &info(), Bounds::PRODUCTION).unwrap(),
            Registered::Created
        );
    }

    /// #3115 — a host that never stops runs the retire into its bound, which
    /// says so: the service is deleted (a retire wants it gone) and marked
    /// until its host exits. The wait ends at the bound, not after it.
    #[test]
    fn a_host_that_never_exits_hits_the_bound_and_says_so() {
        let scm = FakeScm::running(StopModel {
            takes: None,
            lingers: None,
        });
        let retired = retire(&scm, NAME, Bounds::PRODUCTION).expect("retire");
        let Retired::DeletedWhileRunning { stopping } = retired else {
            panic!("{retired:?}");
        };
        assert!(!stopping.stopped && !stopping.exited, "{stopping:?}");
        assert_eq!(stopping.took, STOP_WAIT_BOUND);
        assert_eq!(scm.elapsed(), STOP_WAIT_BOUND);
        assert_eq!(scm.registered_state(), Some((State::StopPending, true)));
    }

    /// A host that reports STOPPED but whose process never exits holds the
    /// retire to the same bound; the service is then deleted cleanly (it is
    /// stopped), and the outcome says the process outlived the wait.
    #[test]
    fn a_process_that_outlives_its_stopped_report_is_waited_to_the_bound() {
        let scm = FakeScm::running(StopModel {
            takes: Some(secs(2.0)),
            lingers: None,
        });
        let retired = retire(&scm, NAME, Bounds::PRODUCTION).expect("retire");
        let Retired::Removed {
            stopping: Some(stopping),
        } = retired
        else {
            panic!("{retired:?}");
        };
        assert!(stopping.stopped && !stopping.exited, "{stopping:?}");
        assert_eq!(stopping.took, STOP_WAIT_BOUND);
        assert_eq!(deletes(&scm.events()), [(State::Stopped, true)]);
    }

    /// An install that finds the name still marked at its bound fails, and the
    /// error says why and what to do; nothing is created half-way.
    #[test]
    fn an_install_that_finds_the_name_marked_for_good_says_so() {
        for open_marked_ok in [false, true] {
            let scm = FakeScm::marked_while_stopping(Duration::from_secs(3600));
            scm.inner.borrow_mut().open_marked_ok = open_marked_ok;
            let err = register(&scm, &info(), Bounds::PRODUCTION).expect_err("still marked");
            let msg = format!("{err:#}");
            assert!(msg.contains("still marked for deletion"), "{msg}");
            assert!(msg.contains("run the install again"), "{msg}");
            assert_eq!(scm.elapsed(), MARKED_WAIT_BOUND, "{open_marked_ok}");
            assert!(
                !scm.events()
                    .iter()
                    .any(|e| matches!(e, Event::Create { answer: Ok(()), .. }))
            );
        }
    }

    /// A deleted entry that a handle elsewhere still holds is reported, after
    /// the settle bound, instead of waited on for good.
    #[test]
    fn a_handle_held_elsewhere_is_reported_after_the_settle_bound() {
        let scm = FakeScm::running(StopModel {
            takes: Some(secs(1.0)),
            lingers: Some(secs(0.1)),
        });
        scm.inner
            .borrow_mut()
            .entry
            .as_mut()
            .unwrap()
            .held_elsewhere_until = Some(Duration::from_secs(3600));
        let retired = retire(&scm, NAME, Bounds::PRODUCTION).expect("retire");
        assert!(matches!(retired, Retired::Held { .. }), "{retired:?}");
        assert!(
            scm.elapsed() <= secs(1.1) + GONE_WAIT_BOUND + POLL,
            "{:?}",
            scm.elapsed()
        );
    }

    /// A service still starting refuses the stop (1061); it is asked again once
    /// it runs, and then retired as usual.
    #[test]
    fn a_starting_service_is_asked_to_stop_again_once_it_runs() {
        let scm = FakeScm::running(StopModel {
            takes: Some(secs(1.0)),
            lingers: Some(secs(0.1)),
        });
        {
            let mut i = scm.inner.borrow_mut();
            let e = i.entry.as_mut().unwrap();
            e.state = State::StartPending;
            e.start_pending_until = Some(secs(0.5));
        }
        let retired = retire(&scm, NAME, Bounds::PRODUCTION).expect("retire");
        assert!(matches!(retired, Retired::Removed { .. }), "{retired:?}");
        let stops = scm
            .events()
            .iter()
            .filter(|e| matches!(e, Event::Stop { .. }))
            .count();
        assert_eq!(stops, 2, "asked once while starting, once running");
    }

    /// #3115 — the bounds are derived from the host's own shutdown constants,
    /// and cover it: the stop wait outlasts the supervisor's whole bound (the
    /// worker's graceful budget inside it), the runtime the host lets go after,
    /// the STOP_PENDING hint the host gives the SCM, and the field's stop.
    #[test]
    fn the_stop_wait_covers_the_hosts_whole_shutdown() {
        assert!(supervisor::SHUTDOWN_BOUND > supervisor::WORKER_GRACEFUL_STOP_BUDGET);
        assert_eq!(
            HOST_STOP_BOUND,
            supervisor::SHUTDOWN_BOUND + crate::runtime_exit::SHUTDOWN_BUDGET
        );
        assert!(STOP_WAIT_BOUND > HOST_STOP_BOUND);
        assert!(STOP_WAIT_BOUND >= super::super::STOP_PENDING_WAIT_HINT);
        assert!(STOP_WAIT_BOUND > FIELD_STOP + FIELD_LINGER);
        assert!(MARKED_WAIT_BOUND > STOP_WAIT_BOUND);
        // Master waited 5 s: inside the worker's own budget, so it lost by
        // construction to any stop that used most of that budget.
        assert!(Duration::from_secs(5) < supervisor::WORKER_GRACEFUL_STOP_BUDGET);
    }

    #[test]
    fn scm_errors_name_what_the_scm_said() {
        assert_eq!(ScmError::from_code(1060), ScmError::Absent);
        assert_eq!(ScmError::from_code(1072), ScmError::MarkedForDelete);
        assert_eq!(ScmError::from_code(1073), ScmError::Exists);
        assert_eq!(ScmError::from_code(5), ScmError::Other(5));
        assert!(ScmError::MarkedForDelete.to_string().contains("1072"));
        assert!(ScmError::Other(5).to_string().contains("error 5"));
    }
}
