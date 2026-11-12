// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! File-transfer data-channel handler.
//!
//! Accepts uploads from the controller browser and writes them into
//! the controlled host's Downloads folder. Closes the final open
//! MEDIUM Known Issue on `docs/remote-control.md` (file-transfer DC
//! was accepted but log-only).
//!
//! Wire protocol on the `files` data channel:
//!
//! ```text
//! // Browser → Agent (control: string payloads)
//! { "t": "files:begin", "id": "<client-chosen-id>",
//!   "name": "report.pdf", "size": 1048576, "mime": "application/pdf" }
//! // Browser → Agent (data: binary payloads, one or many per transfer)
//! <raw ArrayBuffer bytes; appended in arrival order to the current
//!  transfer identified by the most recent files:begin>
//! { "t": "files:end", "id": "<same id>" }
//!
//! // Agent → Browser (all control: string payloads)
//! { "t": "files:accepted", "id": "<id>", "path": "C:\\...\\report.pdf" }
//! { "t": "files:progress", "id": "<id>", "bytes": 524288 }
//! { "t": "files:complete", "id": "<id>", "path": "...", "bytes": 1048576 }
//! { "t": "files:error",    "id": "<id>", "message": "<reason>" }
//! ```
//!
//! Design notes
//!
//! - One active transfer per DC. Concurrent transfers would require
//!   multiplexing binary chunks by id, which SCTP on a DC doesn't do
//!   for us — browsers would need to open one DC per transfer. Ship
//!   the simple path first; queue client-side.
//! - Destination: the Downloads of whoever the file is written as. Never
//!   the temp dir on Linux (#3688: the daemon's own `~/Downloads`, made
//!   0700, else a refusal); Windows falls back to the machine's folder.
//! - Filename safety: the browser-provided `name` is stripped to its
//!   basename and any character outside `[A-Za-z0-9._-]` is replaced
//!   with `_` so the agent never writes outside Downloads. Collisions
//!   append ` (N)` before the extension.
//! - Size cap: 2 GiB per transfer (below SCTP's 2^31-1 limit; well
//!   above any sane "drop a file onto a screen-share" use case).
//!   Configurable later.
//! - The writer is an owned `tokio::fs::File` behind a Mutex so a
//!   burst of binary chunks serializes on the filesystem without the
//!   handler blocking the DC read loop.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tunnel_core::env::node_env_os;

use crate::files_dir;

/// #3688 — a received file's staging on Linux, held by handles.
#[cfg(target_os = "linux")]
mod staging;

/// #3688 — a transfer's staging, held by handles from its begin (or resume)
/// to its end: on Linux, its folders ([`staging::Held`]; `None` only in the
/// tests' own staging-root mode); elsewhere nothing, and the steps go by path.
#[cfg(target_os = "linux")]
type HeldStaging = Option<staging::Held>;
#[cfg(not(target_os = "linux"))]
type HeldStaging = ();

/// 2 GiB. SCTP DCs in webrtc-rs can carry larger payloads in theory
/// but per-transfer >2 GB is outside the "drop a file" use case and
/// would need chunk-resume which this MVP doesn't implement.
const MAX_TRANSFER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Process-global flag for the `files:dir` browse capability. Set
/// once at startup from `AgentConfig::enable_remote_browse` (and the
/// `ROOMLERD_DISABLE_BROWSE` env-var escape hatch). Readers in
/// the DC hot path use [`is_remote_browse_enabled`] which compiles
/// to a single relaxed atomic load.
///
/// Default `true` (preserves self-controlled-host auto-grant
/// semantics from `docs/remote-control.md` §11.2). `set_remote_browse`
/// is called from `app.rs::Run` after config load; no
/// initialization race in practice because file-DC handlers can't
/// fire until a session opens, which is well after main has
/// settled.
///
/// ⚠️ The default is what a process that never loads the config sees —
/// the `caps-probe` child, above all. That is why the hello's `files`
/// list is assigned by `encode::caps::detect()` in the DAEMON (its second
/// reader, at the first hello, after `run_cmd` has stored the flag) and
/// never computed inside the child's `compute_caps` (#1672).
static REMOTE_BROWSE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

pub fn set_remote_browse_enabled(enabled: bool) {
    REMOTE_BROWSE_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
}

pub fn is_remote_browse_enabled() -> bool {
    REMOTE_BROWSE_ENABLED.load(std::sync::atomic::Ordering::Acquire)
}

/// FR-84 D4 — the configured drop folder (`files_dir`), LIVE: seeded from
/// the config at startup (`app.rs::run_cmd`) and re-seeded by the daemon's
/// `ConfigSet` (`localapi_state::config_set`); read by every transfer. The
/// raw config VALUE is kept, not a resolved path: `~` expands against the
/// ACTIVE user at the moment of the drop, and the placement rule is judged
/// against the identity doing the writing then — one machine-global config
/// stays right for whoever is logged in.
///
/// A process-global like `REMOTE_BROWSE_ENABLED` above rather than a
/// per-transfer config-file read: the daemon already holds the only write
/// path that matters (the LocalAPI verb — remote configuration cannot carry
/// this key, `DesiredConfig` has no such field), the value is one string,
/// and a file read per drop would add a TOML parse plus a torn-read race
/// with the atomic save for no gain — a hand edit of the file is picked up
/// at the next start, exactly as every other key.
static FILES_DIR: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Bumped — under `FILES_DIR`'s write lock — every time the configured value
/// CHANGES, so a status answer computed for an older value can never
/// overwrite one computed for the current value.
static FILES_DIR_GEN: AtomicU64 = AtomicU64::new(0);

/// Seed / re-seed the configured drop folder. Blank = unset (the ladder).
/// A change also starts a background refresh of the status answer, so the
/// Overview has one long before anyone asks.
pub fn set_files_dir(value: Option<String>) {
    let value = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let changed = {
        let mut g = FILES_DIR.write().unwrap_or_else(|e| e.into_inner());
        if *g == value {
            false
        } else {
            *g = value;
            FILES_DIR_GEN.fetch_add(1, Ordering::AcqRel);
            true
        }
    };
    if changed {
        spawn_effective_refresh();
    }
}

/// The configured drop folder, as spelled in the config (unexpanded).
pub fn configured_files_dir() -> Option<String> {
    FILES_DIR.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The value and its generation, read together.
fn configured_files_dir_and_gen() -> (Option<String>, u64) {
    let g = FILES_DIR.read().unwrap_or_else(|e| e.into_inner());
    (g.clone(), FILES_DIR_GEN.load(Ordering::Acquire))
}

/// Serialises the tests that set the process-global drop folder. A tokio
/// mutex because one of them (`localapi_state`) holds it across `.await`;
/// the sync tests take it with `blocking_lock`.
#[cfg(test)]
pub(crate) static FILES_DIR_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Who this process writes files as, for `files_dir`'s placement rule
/// (`roomler_node_core::files_dir`).
///
/// * Windows, `LocalSystem` — the winlogon-token worker in a user's session
///   (`system_context`), or any `roomlerd run` started under a SYSTEM token,
///   feature or no feature. Privileged; the active user is the one signed in
///   to THIS process's session, and their profile comes from their token
///   (`win_identity::session_user_profile_dir`), never from their name. No
///   user in the session (session 0, pre-logon) = no profile = every folder
///   refused, by design.
/// * Unix, root — privileged; the "active" profile is the daemon's own home.
///   A root daemon serves the console through KMS capture / uinput with no
///   per-user identity to resolve, its default ladder already lands in its
///   own folders, and the LocalAPI socket is owner-only (0600) — the pipe's
///   non-admin caller does not exist on Unix, so the rule is belt and braces.
///   On Linux that home is the one root's account names (`account_home`),
///   never the `HOME` its environment names (`sudo -E` keeps the caller's;
///   a drop-in may set one), so another account's home there never becomes
///   where a configured or a chosen folder may sit (#3688).
/// * Everyone else — unprivileged: this process can only write where its own
///   user can, so the folder rules apply and the profile rule does not.
pub fn writer_context() -> files_dir::Writer {
    #[cfg(target_os = "windows")]
    {
        if crate::win_identity::process_is_local_system() {
            return files_dir::Writer {
                privileged: true,
                active_home: crate::win_identity::session_user_profile_dir(),
            };
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            #[cfg(target_os = "linux")]
            let active_home = account_home().map(|home| home.to_string_lossy().into_owned());
            #[cfg(not(target_os = "linux"))]
            let active_home = process_home();
            return files_dir::Writer {
                privileged: true,
                active_home,
            };
        }
    }
    files_dir::Writer::unprivileged(process_home())
}

/// This process's own home. Windows asks the token (a registry read — never
/// the known-folder lookups `UserDirs` makes, which verify every folder and
/// so can touch a share a folder is redirected to).
fn process_home() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        crate::win_identity::own_profile_dir()
    }
    #[cfg(not(target_os = "windows"))]
    {
        // #3688 — a test's stand-in for the daemon's own home, so the
        // placement rule's home is never a real one either: the `HOME` its
        // environment names (`stand_in::env_home`), which `BaseDirs` reads,
        // else its account's own.
        #[cfg(all(test, target_os = "linux"))]
        if let Some(home) = stand_in::env_home_now().or_else(stand_in::own_home_now) {
            return home.map(|home| home.to_string_lossy().into_owned());
        }
        directories::BaseDirs::new().map(|d| d.home_dir().to_string_lossy().into_owned())
    }
}

/// FR-97 P3d — who a received file is written as.
///
/// A Linux system install is root, and its sessions show the person at the
/// screen; a file sent to them used to land in root's own Downloads, where
/// they could not see it. It now lands in THEIR folder, and every path step
/// there (the staging folder, the data file, the final rename, a resume, the
/// orphan sweep) runs on a thread that took on their filesystem identity
/// (`crate::fs_identity`), never as root followed by a `chown`: in a folder
/// the person owns, a link they planted would otherwise be followed with
/// root's rights, and the staging folder's `meta.json`, which a resume reads,
/// is theirs to edit. "The person" is the one at seat0
/// (`companion::seat0_user`, FR-97 P5a's definition), never whoever asked.
/// `ROOMLERD_FILES_AS_SEAT_USER=0` keeps the daemon's own identity.
///
/// #3048 — the Windows twin. A machine-wide (SystemContext) worker is
/// LocalSystem in the session it serves, and every path step on the person's
/// side (their folder, the file, a resume's placement) runs on a blocking
/// thread impersonating the person signed in to that session
/// (`win_identity::as_person`), never as LocalSystem. The person is read from
/// their token (`win_identity::session_person`), never from a name. Staging
/// stays the service's own folder, so the file is copied at the end into a new
/// file the person creates: it is born theirs, with their folder's
/// permissions. With nobody signed in, a received file is refused
/// ([`NOBODY_SIGNED_IN`]), never written as LocalSystem. The same switch keeps
/// the daemon's own identity.
///
/// #3669 — a viewer's reads there take the same person ([`FsAs::for_read`]):
/// a download, a folder download and the drawer's listing open everything
/// they touch as them, and with nobody signed in they are refused.
///
/// #3688 — so do a viewer's reads on a Linux system install: the person at
/// seat0, each step with the thread's effective capabilities emptied
/// (`fs_identity::FsIdentity::begin_reading`), and each handle held to what
/// was asked, off the kernel's own file systems ([`held_to_the_person`]).
/// With nobody there a read is refused, and a received file is the daemon's
/// but in the default folder: never in a folder a controller chose, nor in
/// the configured one ([`FsAs::DaemonAlone`]).
/// `ROOMLERD_FILES_AS_SEAT_USER=0` gives back the daemon's identity both
/// ways.
///
/// #3711 — a received file asks who is at seat0 as a read does, off the
/// async worker and [`SEAT_LOOKUP_WAIT`] at most ([`FsAs::for_write`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FsAs {
    /// This process: every user install, a Mac's GUI worker (its system
    /// daemon declines the channel, #3757: `peer::files_channel`), a Windows
    /// worker that is not LocalSystem, a Linux root daemon with root itself
    /// signed in at seat0, and any install with `ROOMLERD_FILES_AS_SEAT_USER=0`.
    Daemon,
    /// (Linux root, a received file) nobody at seat0 (#3688): written as the
    /// daemon, as [`Self::Daemon`] is, but never in a folder a controller
    /// chose: FR-84 D4's rule, "with nobody signed in, nowhere a controller
    /// chose" ([`check_dest_placement`]). Nor in the configured folder: its
    /// writer has no active home ([`Self::writer`]), so the drop lands in
    /// the default folder. Only a write takes it; a read with nobody there
    /// is [`Self::Nobody`].
    #[cfg(target_os = "linux")]
    DaemonAlone,
    /// (Linux root) the person signed in at seat0.
    #[cfg(target_os = "linux")]
    Person {
        account: crate::fs_identity::Account,
        home: PathBuf,
        /// #3688 — what a step does with the thread's capabilities.
        caps: Caps,
    },
    /// (Windows, a LocalSystem worker) the person signed in to this process's
    /// session, fixed when the transfer began.
    #[cfg(target_os = "windows")]
    Person {
        /// Their SID: every step checks the token it runs with is theirs.
        sid: String,
        /// Their profile folder, the placement rule's active home.
        profile: PathBuf,
        /// Their Downloads known folder, read without being verified.
        downloads: Option<PathBuf>,
        /// Where their token comes from at each step.
        token: crate::win_identity::PersonToken,
    },
    /// (Windows, a LocalSystem worker) nobody is signed in to this process's
    /// session: every step is refused with [`NOBODY_SIGNED_IN`]. (Linux root,
    /// a read) nobody at seat0 (#3688): every read is refused, saying so.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    Nobody,
    /// (Linux root) who is at seat0 could not be told (#3688): the lookup
    /// failed or ran out of its time. Every read is refused with
    /// [`CANNOT_TELL`], never as if nobody were there. #3711 — so is a
    /// received file whose lookup ran out of its time ([`Seat::OutOfTime`]);
    /// one whose lookup failed outright takes [`Self::DaemonAlone`], as with
    /// nobody.
    #[cfg(target_os = "linux")]
    CannotTell,
}

/// #3688 — what a step as the person at seat0 does with its thread's
/// capabilities (`fs_identity::FsIdentity`).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caps {
    /// Kept: a received file's steps, which the placement rule holds to the
    /// person's own folders (FR-97 P3d, unchanged).
    Kept,
    /// Emptied for the step and given back after it: a read's, which may
    /// name any path. The thread is still root by its euid, so it reads
    /// with the person's rights and no capability on top of them.
    Empty,
}

/// #3688 — a step taken for nobody on a Linux system install. A read says so
/// in its own words before any step ([`NOBODY_SIGNED_IN_TO_SEND`],
/// [`NOBODY_SIGNED_IN_TO_BROWSE`]), and a received file never takes nobody
/// ([`FsAs::DaemonAlone`]), so this answers only a state nothing produces.
#[cfg(target_os = "linux")]
const NOBODY_AT_THE_SCREEN: &str = "Nobody is signed in at this computer's screen";

/// #3688 — why a read is refused on a Linux system install when who is at
/// seat0 could not be told: the seat0 lookup failed, or did not answer in
/// time ([`SEAT_LOOKUP_WAIT`]). Not nobody: a retry may well find them.
/// #3711 — and why a received file is refused when the lookup did not
/// answer in time.
#[cfg(target_os = "linux")]
const CANNOT_TELL: &str = "Could not tell who is signed in at this computer's screen. Try again.";

/// #3048 — said when a step would be taken for someone other than the person
/// a transfer, or a listing (#3669), began for: they signed out, or another
/// person signed in.
pub(crate) const NOT_THE_PERSON: &str = "the person this began for is no longer signed in";

/// #3048 — why a received file is refused on a machine-wide Windows install
/// with nobody signed in. The viewer shows it in its `files:error`.
#[cfg(target_os = "windows")]
pub(crate) const NOBODY_SIGNED_IN: &str =
    "Nobody is signed in to this computer to receive the file";

/// #3669 — why a download (a file, or a folder as one zip) is refused on a
/// machine-wide Windows install with nobody signed in. The viewer shows it in
/// its `files:error`.
#[cfg(not(target_os = "linux"))]
pub(crate) const NOBODY_SIGNED_IN_TO_SEND: &str =
    "Nobody is signed in to this computer to send the file";

/// #3688 — the same on a Linux system install with nobody at seat0. The rule
/// there is the screen: someone signed in over SSH is not at it.
#[cfg(target_os = "linux")]
pub(crate) const NOBODY_SIGNED_IN_TO_SEND: &str =
    "Nobody is signed in at this computer's screen to send the file";

/// #3669 — why the drawer lists nothing on a machine-wide Windows install
/// with nobody signed in. The viewer shows it in its `files:dir-error`.
#[cfg(not(target_os = "linux"))]
pub(crate) const NOBODY_SIGNED_IN_TO_BROWSE: &str =
    "Nobody is signed in to this computer to browse its files";

/// #3688 — the same on a Linux system install with nobody at seat0.
#[cfg(target_os = "linux")]
pub(crate) const NOBODY_SIGNED_IN_TO_BROWSE: &str =
    "Nobody is signed in at this computer's screen to browse its files";

/// #3669 — whether `e` is, anywhere in its chain, an access refusal.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn is_permission_denied(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/// #3048 — who a transfer starting now acts as on Windows, from what this
/// process's own token said (`win_identity::own_token_is_local_system`): the
/// person signed in to this process's session when it is LocalSystem; nobody
/// when no one is; the daemon itself when it is another account, or with
/// `ROOMLERD_FILES_AS_SEAT_USER=0`. #3669 — a token that could not be read
/// counts as LocalSystem, so the answer is then the person or nobody, never
/// the worker itself. `person` is asked only when it is needed. Pure, so the
/// decision is a test.
#[cfg(target_os = "windows")]
fn fs_as_for(
    own_token: Option<bool>,
    switch_on: bool,
    person: impl FnOnce() -> Option<crate::win_identity::SessionPerson>,
) -> FsAs {
    if own_token == Some(false) || !switch_on {
        return FsAs::Daemon;
    }
    match person() {
        Some(p) => FsAs::Person {
            sid: p.sid,
            profile: p.profile,
            downloads: p.downloads,
            token: crate::win_identity::PersonToken::Session,
        },
        None => FsAs::Nobody,
    }
}

impl FsAs {
    /// Who a transfer starting now writes as, asked on a blocking thread:
    /// `ConfigSet`'s check of `files_dir` and the status refresh's own
    /// thread. On a Linux system install it asks seat0 inline, with no time
    /// limit. #3711 — an async task asks [`Self::for_write`] instead.
    pub(crate) fn now() -> Self {
        #[cfg(target_os = "linux")]
        {
            let (root, switch_on) = linux_process();
            linux_fs_as(root, switch_on, false, seat_now)
        }
        #[cfg(not(target_os = "linux"))]
        Self::session_or_daemon()
    }

    /// #3669 — who a read starting now (a download, a folder download, a
    /// listing) runs as: on a machine-wide Windows install, the person a
    /// write takes, by the same function, so a read and a write cannot take
    /// different people. #3688 — on a Linux system install, the person at
    /// seat0 a write takes, by the same rule ([`linux_fs_as`]), each step
    /// with no capability; nobody there, or a seat0 lookup that does not
    /// answer within [`SEAT_LOOKUP_WAIT`], refuses the read. macOS and user
    /// installs read as the daemon, as before. On Windows a test stands a
    /// reader in for this answer (`stand_in::reader`), so the entry points
    /// are tested with the person, not only the daemon. On Linux it stands
    /// in who is at seat0 (`stand_in::seat`), so they are tested through
    /// this very rule.
    pub(crate) async fn for_read() -> Self {
        #[cfg(all(test, target_os = "windows"))]
        if let Some(who) = stand_in::reader_now() {
            return who;
        }
        #[cfg(target_os = "linux")]
        {
            Self::at_seat0(true).await
        }
        #[cfg(not(target_os = "linux"))]
        Self::session_or_daemon()
    }

    /// #3711 — who a transfer starting now writes as, asked from an async
    /// task (`files:begin`, `files:resume`, the startup sweep). On a Linux
    /// system install the seat0 lookup runs as a read's does, on a blocking
    /// thread and [`SEAT_LOOKUP_WAIT`] at most. One that gives no answer in
    /// that time refuses the write as a read is refused
    /// ([`Self::CannotTell`]); one that failed outright leaves the file the
    /// daemon's alone, as with nobody at seat0 ([`linux_fs_as`]). Windows
    /// and macOS answer as [`Self::now`] does.
    pub(crate) async fn for_write() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::at_seat0(false).await
        }
        #[cfg(not(target_os = "linux"))]
        Self::session_or_daemon()
    }

    /// #3688, #3711 — who a read, or a write, starting now acts as on Linux,
    /// from this process and who is at seat0 ([`linux_fs_as_within`]). A
    /// test stands in the lookup on its thread (`stand_in::seat`,
    /// `stand_in::seat_asked`).
    #[cfg(target_os = "linux")]
    async fn at_seat0(reading: bool) -> Self {
        let (root, switch_on) = linux_process();
        // Asked here, on the thread a test stood it in on.
        #[cfg(test)]
        let stood_in = stand_in::lookup_now();
        linux_fs_as_within(root, switch_on, reading, SEAT_LOOKUP_WAIT, move || {
            #[cfg(test)]
            if let Some(lookup) = stood_in {
                return lookup();
            }
            seat_now()
        })
        .await
    }

    /// #3048 — on Windows, the person signed in to this process's session
    /// when it is LocalSystem (or cannot tell, #3669), nobody when no one is;
    /// the daemon itself otherwise, or with `ROOMLERD_FILES_AS_SEAT_USER=0`.
    /// macOS: the daemon. Linux asks [`linux_fs_as`] instead.
    #[cfg(not(target_os = "linux"))]
    fn session_or_daemon() -> Self {
        #[cfg(target_os = "windows")]
        {
            fs_as_for(
                crate::win_identity::own_token_is_local_system(),
                tunnel_core::env::flag("FILES_AS_SEAT_USER", true),
                crate::win_identity::session_person,
            )
        }
        #[cfg(not(target_os = "windows"))]
        {
            Self::Daemon
        }
    }

    /// #3669 — a read is refused before anything is touched when nobody is
    /// signed in on a machine-wide Windows install; `why` is what the viewer
    /// is told. #3688 — and on a Linux system install with nobody at seat0,
    /// a received file's answer for that ([`Self::DaemonAlone`]) included:
    /// it is the daemon's identity, which no read there takes.
    fn refuse_read(&self, why: &'static str) -> Result<()> {
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        if matches!(self, Self::Nobody) {
            return Err(anyhow!(why));
        }
        #[cfg(target_os = "linux")]
        if matches!(self, Self::DaemonAlone) {
            return Err(anyhow!(why));
        }
        #[cfg(target_os = "linux")]
        if matches!(self, Self::CannotTell) {
            return Err(anyhow!(CANNOT_TELL));
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let _ = why;
        Ok(())
    }

    /// #3669 — a read refused to the person says so, naming what was asked
    /// for: the viewer shows only an error's outermost line, which would
    /// otherwise be a bare "reading …". It says Windows refused, not that
    /// the person may not: their rights are one cause, and an EDR or DLP
    /// filter answers with the same code. The daemon's errors stay as they
    /// were. #3688 — Linux says the same of the person at seat0: their
    /// rights are one cause, and an LSM (AppArmor, SELinux) answers with the
    /// same code.
    fn explain_refusal<T>(&self, asked: &str, read: Result<T>) -> Result<T> {
        #[cfg(target_os = "windows")]
        if matches!(self, Self::Person { .. }) && read.as_ref().is_err_and(is_permission_denied) {
            return read.with_context(|| {
                format!(
                    "{asked}: Windows refused to open it for the person signed in to this computer"
                )
            });
        }
        #[cfg(target_os = "linux")]
        if matches!(self, Self::Person { .. }) && read.as_ref().is_err_and(is_permission_denied) {
            return read.with_context(|| {
                format!(
                    "{asked}: Linux refused to open it for the person signed in at this \
                     computer's screen"
                )
            });
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        let _ = asked;
        read
    }

    /// #3048 — the reason a transfer is refused before anything is touched:
    /// nobody signed in on a machine-wide Windows install.
    pub(crate) fn refusal(&self) -> Option<&'static str> {
        #[cfg(target_os = "windows")]
        if matches!(self, Self::Nobody) {
            return Some(NOBODY_SIGNED_IN);
        }
        #[cfg(target_os = "linux")]
        if matches!(self, Self::Nobody) {
            return Some(NOBODY_AT_THE_SCREEN);
        }
        #[cfg(target_os = "linux")]
        if matches!(self, Self::CannotTell) {
            return Some(CANNOT_TELL);
        }
        None
    }

    /// #3048 — the person a Windows transfer is written as, by SID.
    pub(crate) fn person_sid(&self) -> Option<&str> {
        #[cfg(target_os = "windows")]
        if let Self::Person { sid, .. } = self {
            return Some(sid);
        }
        None
    }

    /// #3688 — whether steps as `self` are the daemon's own: a received file
    /// with nobody at seat0 is written as the daemon too
    /// ([`Self::DaemonAlone`]).
    fn is_the_daemon(&self) -> bool {
        #[cfg(target_os = "linux")]
        if matches!(self, Self::DaemonAlone) {
            return true;
        }
        matches!(self, Self::Daemon)
    }

    /// #3688 — whether a received file is written by the daemon alone: a
    /// root daemon with nobody at seat0 ([`Self::DaemonAlone`]). Its files
    /// are born 0600, root's only, wherever they land: in root's own
    /// Downloads ([`default_download_dir`]).
    fn writes_alone(&self) -> bool {
        #[cfg(target_os = "linux")]
        if matches!(self, Self::DaemonAlone) {
            return true;
        }
        false
    }

    /// #3688 — whether a received file is written by root itself: the
    /// daemon alone (nobody at seat0), or a daemon that is root (root at the
    /// screen, or the switch off). Such a writer's resume and end place only
    /// in a folder the rule gives it now ([`root_may_place_in`]), and its
    /// folder upload follows no link on the way.
    #[cfg(target_os = "linux")]
    fn writes_as_root(&self) -> bool {
        match self {
            Self::DaemonAlone => true,
            Self::Daemon => own_euid() == 0,
            _ => false,
        }
    }

    /// #3688 — whether a read's steps hold each handle they open to what
    /// was asked ([`held_to_the_person`], [`folder_held_to_the_person`]):
    /// on Linux, as the person at seat0.
    fn checks_handles(&self) -> bool {
        #[cfg(target_os = "linux")]
        if matches!(self, Self::Person { .. }) {
            return true;
        }
        false
    }

    /// #3688 — before a folder download lists its next folder, or opens its
    /// next file: whether the person at seat0 it began for is still there, by
    /// seat0's active uid ([`seat0_active_uid`]). They signed out, the login
    /// screen or another person took seat0, or the seat does not read: the
    /// walk ends, saying so ([`NOT_THE_PERSON`], a Windows walk's words), never
    /// with a zip that looks complete. Every other reader goes on.
    async fn still_at_the_screen(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        if let Self::Person { account, .. } = self {
            // Asked here, on the thread a test stood a seat in on.
            #[cfg(test)]
            let stood_in = stand_in::seat0_uid_now();
            let uid = account.uid;
            let there = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(at_seat0) = stood_in {
                    return at_seat0 == Some(uid);
                }
                seat0_active_uid() == Some(uid)
            })
            .await
            .unwrap_or(false);
            if !there {
                tracing::info!(
                    uid,
                    "files: the person a folder download began for has left seat0 — it ends"
                );
                return Err(anyhow!(NOT_THE_PERSON));
            }
        }
        Ok(())
    }

    /// Run blocking filesystem work as this identity, on a blocking thread:
    /// an identity belongs to the thread that takes it on, never to an async
    /// worker.
    pub(crate) async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let who = self.clone();
        // #3688 — what a test stood in on its thread (the daemon's own home,
        // the environment's, whether file systems keep modes) goes with the
        // step to its thread.
        #[cfg(all(test, target_os = "linux"))]
        let carried = stand_in::carried();
        tokio::task::spawn_blocking(move || {
            #[cfg(all(test, target_os = "linux"))]
            let _carried = carried.stand();
            who.run_here(f)
        })
        .await
        .context("a file-transfer step's thread")?
    }

    /// [`Self::run`] on THIS thread, which must be a blocking one.
    fn run_here<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        match self {
            Self::Daemon => f(),
            #[cfg(target_os = "linux")]
            Self::DaemonAlone => f(),
            #[cfg(target_os = "linux")]
            Self::Nobody => Err(anyhow!(NOBODY_AT_THE_SCREEN)),
            #[cfg(target_os = "linux")]
            Self::CannotTell => Err(anyhow!(CANNOT_TELL)),
            #[cfg(target_os = "linux")]
            Self::Person { account, caps, .. } => {
                // #3688 — a read's step also empties the thread's
                // capabilities; a received file's is FR-97 P3d's, unchanged.
                let _as_them = match caps {
                    Caps::Kept => crate::fs_identity::FsIdentity::begin(account),
                    Caps::Empty => crate::fs_identity::FsIdentity::begin_reading(account),
                }
                .map_err(|e| anyhow!("acting as {}: {e}", account.name))?;
                f()
            }
            #[cfg(target_os = "windows")]
            Self::Person { sid, token, .. } => {
                crate::win_identity::as_person(sid, token, f).map_err(|why| anyhow!(why))?
            }
            #[cfg(target_os = "windows")]
            Self::Nobody => Err(anyhow!(NOBODY_SIGNED_IN)),
        }
    }

    /// The writer FR-84 D4's placement rules check: for the person, the
    /// daemon is still privileged, and the person's home is the active one.
    fn writer(&self) -> files_dir::Writer {
        match self {
            Self::Daemon => writer_context(),
            // #3688 — nobody at seat0: the daemon's writer with no active
            // home, as a SYSTEM worker's with nobody signed in. `~` has no
            // home to expand against, and a root daemon's folders none to
            // sit in, so no configured folder passes its gates (set, use,
            // the status card, the sweep) and a drop lands in the default
            // folder. A controller's chosen folder is refused before it is
            // touched ([`resolve_dest_path_as`]).
            #[cfg(target_os = "linux")]
            Self::DaemonAlone => files_dir::Writer {
                active_home: None,
                ..writer_context()
            },
            #[cfg(target_os = "linux")]
            Self::Person { home, .. } => files_dir::Writer {
                privileged: true,
                active_home: Some(home.to_string_lossy().into_owned()),
            },
            #[cfg(target_os = "windows")]
            Self::Person { profile, .. } => files_dir::Writer {
                privileged: true,
                active_home: Some(profile.to_string_lossy().into_owned()),
            },
            // No profile: every folder refused, as for any privileged writer
            // that cannot name one.
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            Self::Nobody => files_dir::Writer {
                privileged: true,
                active_home: None,
            },
            #[cfg(target_os = "linux")]
            Self::CannotTell => files_dir::Writer {
                privileged: true,
                active_home: None,
            },
        }
    }
}

/// #3048 — the person's Downloads on Windows: their known folder when `check`
/// finds it a folder (read as them, so a share it is redirected to is reached
/// with their logon), else their profile. Unchecked (the status card), the
/// known folder as it is: no probe, so an offline share cannot hold it up.
/// Compiled for tests on every OS, so its test runs where the caller does not.
#[cfg(any(target_os = "windows", test))]
fn person_downloads_windows(
    downloads: Option<&std::path::Path>,
    profile: &std::path::Path,
    check: bool,
) -> PathBuf {
    match downloads {
        Some(d) if !check || d.is_dir() => d.to_path_buf(),
        _ => profile.to_path_buf(),
    }
}

/// #3688 — who is at seat0, as a root daemon's file steps see it.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seat {
    /// A person, as an account (uid 0 refused) and a home.
    Person {
        account: crate::fs_identity::Account,
        home: PathBuf,
    },
    /// root itself is signed in at the screen: the daemon is that person.
    Root,
    /// Nobody, and why: an empty seat, the login screen, a lock that
    /// switches seat0 to a greeter, a session that is not a person's, a name
    /// that resolves to another uid than logind's, a host with no logind.
    Nobody(String),
    /// Not known, and why: logind did not answer, the account database
    /// did not answer, or the lookup failed outright.
    CannotTell(String),
    /// #3711 — not known yet: the lookup gave no answer within its wait
    /// ([`seat_within`]) and goes on, on its own thread, so the account
    /// database's own caches usually hold its answer for the next one.
    OutOfTime(std::time::Duration),
}

/// Who is at seat0 now, by FR-97 P5a's rule (`companion::seat0_account`).
/// Blocking: logind, then the account database.
#[cfg(target_os = "linux")]
fn seat_now() -> Seat {
    seat_of(crate::companion::seat0_account(), |name| {
        crate::fs_identity::Account::by_name(name)
            .and_then(|account| crate::exec::account_home(name).map(|home| (account, home)))
    })
}

/// The pure half of [`seat_now`]: what logind `found` at seat0, and the
/// account and home its name `resolve`s to. Nobody only when seat0 says so
/// (`companion::is_nobody_at_the_screen`: an empty seat, the login screen, a
/// session that is not a person's, a host with no logind and so no seat), or
/// when the name resolves to another uid than logind recorded for the session
/// (a lookup order that puts another source first, an account deleted and
/// made again): its rights are not the person's. Any other failed lookup,
/// logind's or the account database's, cannot tell.
#[cfg(target_os = "linux")]
fn seat_of(
    found: Result<(String, Option<u32>)>,
    resolve: impl FnOnce(&str) -> std::result::Result<(crate::fs_identity::Account, PathBuf), String>,
) -> Seat {
    let (name, uid) = match found {
        Ok(found) => found,
        Err(e) if crate::companion::is_nobody_at_the_screen(&e) => {
            return Seat::Nobody(format!("{e:#}"));
        }
        Err(e) => return Seat::CannotTell(format!("{e:#}")),
    };
    if uid == Some(0) {
        return Seat::Root;
    }
    match resolve(&name) {
        Ok((account, _)) if uid.is_some_and(|at_seat0| at_seat0 != account.uid) => {
            Seat::Nobody(format!(
                "seat0's session is uid {}, but its name {name} resolves to uid {}",
                uid.unwrap_or_default(),
                account.uid
            ))
        }
        Ok((account, home)) => Seat::Person { account, home },
        Err(e) => Seat::CannotTell(e),
    }
}

/// #3688 — the uid of seat0's active session, from logind's own record of
/// the seat (`ACTIVE_UID=` in `/run/systemd/seats/seat0`, which `sd-login`
/// reads): one small file on a tmpfs, cheap enough to read before every
/// folder of a walk. `None` when it does not read, or names no uid.
#[cfg(target_os = "linux")]
fn seat0_active_uid() -> Option<u32> {
    seat0_active_uid_in(&std::fs::read_to_string("/run/systemd/seats/seat0").ok()?)
}

/// The pure half of [`seat0_active_uid`].
#[cfg(target_os = "linux")]
fn seat0_active_uid_in(seat: &str) -> Option<u32> {
    seat.lines()
        .find_map(|line| line.strip_prefix("ACTIVE_UID="))?
        .trim()
        .parse()
        .ok()
}

/// Whether this process is root, and whether `ROOMLERD_FILES_AS_SEAT_USER`
/// is on (the default).
#[cfg(target_os = "linux")]
fn linux_process() -> (bool, bool) {
    // SAFETY: `geteuid` reads this process's own credentials.
    let root = unsafe { libc::geteuid() } == 0;
    (root, tunnel_core::env::flag("FILES_AS_SEAT_USER", true))
}

/// #3711 — whether file transfer here moves files only as the person signed
/// in at the screen: a Linux system install (root) with
/// `ROOMLERD_FILES_AS_SEAT_USER` on, its default ([`FsAs`]). With nobody
/// signed in there it browses and sends nothing, and a received file is
/// root's alone. A hint that says what still works with no picture says so
/// (`capture::CaptureUnavailableCode::hint`, `apps::Unavailable::reason`).
pub(crate) fn follows_the_screen() -> bool {
    #[cfg(target_os = "linux")]
    {
        let (root, switch_on) = linux_process();
        root && switch_on
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// #3688 — who a step starting now acts as on Linux, from whether this
/// process is root, the switch, and who is at seat0, which is asked only
/// when it is needed. Pure, so the decision is a test.
#[cfg(target_os = "linux")]
fn linux_fs_as(root: bool, switch_on: bool, reading: bool, seat: impl FnOnce() -> Seat) -> FsAs {
    if !root || !switch_on {
        return FsAs::Daemon;
    }
    match seat() {
        // A read may name any path, so its steps hold no capability; a
        // received file's are held to the person's folders (FR-97 P3d).
        Seat::Person { account, home } => FsAs::Person {
            account,
            home,
            caps: if reading { Caps::Empty } else { Caps::Kept },
        },
        Seat::Root => FsAs::Daemon,
        Seat::Nobody(why) if reading => {
            tracing::debug!(%why, "files: nobody at seat0 — a read is refused");
            FsAs::Nobody
        }
        // #3711 — no answer in time: refused both ways, and a retry asks
        // again. The lookup goes on and usually finds the person, so a
        // received file is not written for anyone else meanwhile.
        Seat::OutOfTime(wait) => {
            tracing::debug!(
                ?wait,
                reading,
                "files: who is at seat0 is not known yet — refused"
            );
            FsAs::CannotTell
        }
        Seat::CannotTell(why) if reading => {
            tracing::debug!(%why, "files: who is at seat0 is not known — a read is refused");
            FsAs::CannotTell
        }
        Seat::Nobody(why) => {
            tracing::debug!(%why, "files: nobody at seat0 — a write is the daemon's");
            FsAs::DaemonAlone
        }
        // A lookup that failed outright: a write has a safe answer, the
        // daemon alone (root's own default folder, 0600, never a folder a
        // controller chose), so a host whose lookup always fails keeps its
        // uploads. A read has none: as root it would read what root may.
        Seat::CannotTell(why) => {
            tracing::debug!(%why, "files: who is at seat0 is not known — a write is the daemon's");
            FsAs::DaemonAlone
        }
    }
}

/// #3688, #3711 — [`linux_fs_as`] from an async task, a read's or a write's:
/// `lookup` is asked only when it is needed (a root daemon, the switch on),
/// on a blocking thread and `wait` at most ([`seat_within`]). logind and the
/// account database are other processes, and either may hang.
#[cfg(target_os = "linux")]
async fn linux_fs_as_within(
    root: bool,
    switch_on: bool,
    reading: bool,
    wait: std::time::Duration,
    lookup: impl FnOnce() -> Seat + Send + 'static,
) -> FsAs {
    if !(root && switch_on) {
        return FsAs::Daemon;
    }
    let seat = seat_within(wait, lookup).await;
    linux_fs_as(root, switch_on, reading, move || seat)
}

/// #3688 — how long a read waits for the seat0 lookup. A listing waits for
/// it and then for its own timeout ([`DIR_LIST_TIMEOUT_SECS`], 3 s), and the
/// viewer's drawer gives up on `files:dir` after 5 s, so the two together
/// must leave room inside that. `files:get` and `files:get-folder` have no
/// timer of their own in the viewer: their wait only delays the offer.
/// #3711 — a received file's lookup waits as long, at `files:begin`,
/// `files:resume` (which the viewer gives 10 s) and the startup sweep (which
/// signalling waits for).
#[cfg(target_os = "linux")]
const SEAT_LOOKUP_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// #3688 — `lookup` on a blocking thread, given `wait` at most: a wedged
/// logind or account lookup never stalls an async worker. One that fails
/// cannot tell who is at the screen ([`Seat::CannotTell`]); #3711 — one past
/// its time does not know yet ([`Seat::OutOfTime`]), and finishes on its own
/// thread, so the account database's own caches (SSSD's, nscd's) hold its
/// answer for the next lookup.
#[cfg(target_os = "linux")]
async fn seat_within(
    wait: std::time::Duration,
    lookup: impl FnOnce() -> Seat + Send + 'static,
) -> Seat {
    match tokio::time::timeout(wait, tokio::task::spawn_blocking(lookup)).await {
        Ok(Ok(seat)) => seat,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "files: the seat0 lookup failed — who is at seat0 is not known");
            Seat::CannotTell(format!("the seat0 lookup failed: {e}"))
        }
        Err(_) => {
            tracing::warn!(
                ?wait,
                "files: the seat0 lookup did not answer in time — who is at seat0 is not known yet"
            );
            Seat::OutOfTime(wait)
        }
    }
}

/// The person's Downloads: `XDG_DOWNLOAD_DIR` from their own
/// `user-dirs.dirs` when it names a folder inside their home that exists,
/// else `~/Downloads` when it exists, else their home. Read as the person
/// (the caller runs it under their identity).
#[cfg(target_os = "linux")]
fn person_downloads(home: &std::path::Path) -> PathBuf {
    let from_xdg = std::fs::read_to_string(home.join(".config/user-dirs.dirs"))
        .ok()
        .and_then(|text| xdg_download_dir(&text, home));
    if let Some(dir) = from_xdg.filter(|d| d.is_dir()) {
        return dir;
    }
    let plain = home.join("Downloads");
    if plain.is_dir() {
        return plain;
    }
    home.to_path_buf()
}

/// The pure half of [`person_downloads`]: `XDG_DOWNLOAD_DIR` out of a
/// `user-dirs.dirs`, with `$HOME` expanded, and only when it stays inside
/// `home` (the file is the person's to write, so it may name anything).
#[cfg(any(target_os = "linux", test))]
fn xdg_download_dir(text: &str, home: &std::path::Path) -> Option<PathBuf> {
    let value = text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=")?;
        Some(rest.trim().trim_matches('"').to_string())
    })?;
    let path = match value.strip_prefix("$HOME") {
        Some(rel) => home.join(rel.trim_start_matches('/')),
        None => PathBuf::from(&value),
    };
    let inside = path
        .components()
        .all(|c| !matches!(c, std::path::Component::ParentDir))
        && path.starts_with(home)
        && path != home;
    inside.then_some(path)
}

/// FR-84 D4 — the SET-time gate: everything a drop will demand of the value
/// at use time (shape, placement for THIS writer, creatable and writable),
/// run now so a `ConfigSet` the daemon would not honour is refused with the
/// reason instead of saved and silently ignored. The refusal text is what the
/// desktop shows verbatim.
pub fn validate_files_dir_setting(raw: &str) -> Result<PathBuf, String> {
    validate_files_dir_setting_as(&FsAs::now(), raw)
}

/// [`validate_files_dir_setting`] for `who`, on this thread, which must be
/// a blocking one.
fn validate_files_dir_setting_as(who: &FsAs, raw: &str) -> Result<PathBuf, String> {
    // FR-97 P3d — for whoever a drop would be written as now, and as them:
    // the gate creates the folder and probes a write.
    let writer = who.writer();
    who.run_here(|| {
        Ok(files_dir::validate(
            raw,
            &writer,
            &files_dir::Rules::from_env(),
        ))
    })
    .unwrap_or_else(|e| Err(format!("{e:#}")))
}

/// `raw` as it resolves for `writer` right now — shape, placement, the
/// on-disk link check and "a folder could be here" — with nothing created
/// or written: what a drop would use, short of the write probe. Run under
/// the identity the writer describes.
fn resolve_without_creating(raw: &str, writer: &files_dir::Writer) -> Result<PathBuf, String> {
    let rules = files_dir::Rules::from_env();
    let p = files_dir::resolve(raw, writer, &rules)?;
    files_dir::check_placement_on_disk(&p, writer, &rules)?;
    files_dir::check_creatable(&p)?;
    Ok(p)
}

/// The configured folder via [`resolve_without_creating`], for `who` and
/// under their identity. `Ok(None)` = no folder configured.
fn configured_folder_without_creating(who: &FsAs) -> Result<Option<PathBuf>, String> {
    match configured_files_dir() {
        None => Ok(None),
        Some(raw) => resolve_without_creating(&raw, &who.writer()).map(Some),
    }
}

// ── NodeStatus.files_dir: answered from a cache, never from the disk ──────

/// The status answer and what it was computed for.
struct EffectiveFilesDir {
    /// The `FILES_DIR_GEN` it was computed for.
    generation: u64,
    computed_at: Option<std::time::Instant>,
    value: Option<String>,
}

static EFFECTIVE_FILES_DIR: std::sync::Mutex<EffectiveFilesDir> =
    std::sync::Mutex::new(EffectiveFilesDir {
        generation: 0,
        computed_at: None,
        value: None,
    });
static EFFECTIVE_REFRESHING: AtomicBool = AtomicBool::new(false);
/// How old a status answer may be before a read starts a refresh — long
/// enough that a 2 s poller does not re-resolve on every tick, short enough
/// that a folder deleted or a user switched shows up within one glance.
const EFFECTIVE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// FR-84 D4 — where a drop would land RIGHT NOW, for `NodeStatus.files_dir`:
/// the configured folder when it passes for this writer (it need not exist
/// yet — a drop creates it), else the default ladder.
///
/// NEVER touches the disk: the status verb is a synchronous read on the
/// LocalAPI's async dispatch, polled every 2 s by the companion, and the
/// honest answer needs the disk (a link out of the profile) and, through the
/// default ladder's known-folder lookups, possibly a Downloads redirected to
/// a share that is offline right now. So this answers from a cache and, when
/// the cached answer is older than [`EFFECTIVE_TTL`] or was computed for a
/// previous value, refreshes it on a background thread. `None` only before
/// the first refresh has landed (`set_files_dir` at startup starts one).
pub fn effective_files_dir() -> Option<String> {
    let current = FILES_DIR_GEN.load(Ordering::Acquire);
    let (value, fresh) = {
        let c = EFFECTIVE_FILES_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fresh =
            c.generation == current && c.computed_at.is_some_and(|at| at.elapsed() < EFFECTIVE_TTL);
        (c.value.clone(), fresh)
    };
    if !fresh {
        spawn_effective_refresh();
    }
    value
}

/// Compute the status answer now and cache it. BLOCKING (token, disk,
/// known-folder lookups): the daemon's `ConfigSet` runs it on its blocking
/// thread, so the status read right after a change is already the new
/// answer; everything else reaches it through [`spawn_effective_refresh`].
pub fn refresh_effective_files_dir() -> Option<String> {
    refresh_effective_files_dir_as(&FsAs::now())
}

/// [`refresh_effective_files_dir`] for `who`, on this thread, which must be
/// a blocking one.
fn refresh_effective_files_dir_as(who: &FsAs) -> Option<String> {
    let (raw, generation) = configured_files_dir_and_gen();
    let value = compute_effective_files_dir(who, raw.as_deref());
    let mut c = EFFECTIVE_FILES_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // An answer for an OLDER value never replaces one for a newer value.
    if generation >= c.generation {
        *c = EffectiveFilesDir {
            generation,
            computed_at: Some(std::time::Instant::now()),
            value: value.clone(),
        };
    }
    value
}

/// One background refresh at a time; a read that finds one running simply
/// returns the cached answer.
fn spawn_effective_refresh() {
    if EFFECTIVE_REFRESHING.swap(true, Ordering::AcqRel) {
        return;
    }
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            EFFECTIVE_REFRESHING.store(false, Ordering::Release);
        }
    }
    let spawned = std::thread::Builder::new()
        .name("files-dir-status".into())
        .spawn(|| {
            let _done = Done;
            refresh_effective_files_dir();
        });
    if spawned.is_err() {
        EFFECTIVE_REFRESHING.store(false, Ordering::Release);
    }
}

fn compute_effective_files_dir(who: &FsAs, raw: Option<&str>) -> Option<String> {
    // FR-97 P3d — for whoever a drop would be written as now, and as them
    // (a blocking thread: the status refresh's own, or `ConfigSet`'s).
    who.run_here(|| Ok(compute_effective_files_dir_as(who, raw)))
        .unwrap_or_else(|e| {
            tracing::debug!(%e, "files: the effective folder could not be computed as its writer");
            None
        })
}

fn compute_effective_files_dir_as(who: &FsAs, raw: Option<&str>) -> Option<String> {
    if let Some(raw) = raw {
        match resolve_without_creating(raw, &who.writer()) {
            Ok(p) => return Some(p.to_string_lossy().into_owned()),
            Err(e) => tracing::debug!(
                files_dir = %raw,
                %e,
                "files: the configured files_dir would be refused right now — reporting the default folder"
            ),
        }
    }
    default_download_dir_as(who, false)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// #3769 — one `ConfigSet` of `files_dir` (`localapi_state::config_set`):
/// whoever a drop would be written as now (FR-97 P3d), asked once, and the
/// two checks that need them, FR-84 D4's set-time gate ([`Self::check`]) and
/// the status answer after the save ([`Self::refresh_status`]).
pub(crate) struct FilesDirSet {
    who: FsAs,
}

impl FilesDirSet {
    /// Who the folder is checked for, then, when `value` names a folder, the
    /// set-time gate for and as them. `Err` is the refusal, verbatim.
    pub(crate) async fn check(value: Option<&str>) -> Result<Self, String> {
        // Asked here, on the thread a test stood the lookup in on, and run
        // on a blocking thread with no time limit.
        #[cfg(all(test, target_os = "linux"))]
        let stood_in = stand_in::lookup_now();
        let who = tokio::task::spawn_blocking(move || {
            #[cfg(all(test, target_os = "linux"))]
            if let Some(lookup) = stood_in {
                let (root, switch_on) = linux_process();
                return linux_fs_as(root, switch_on, false, lookup);
            }
            FsAs::now()
        })
        .await
        .map_err(|e| format!("config set task join: {e}"))?;
        Self::check_as(who, value).await
    }

    /// [`Self::check`] for `who`.
    async fn check_as(who: FsAs, value: Option<&str>) -> Result<Self, String> {
        if let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) {
            let (gate, raw) = (who.clone(), raw.to_string());
            on_a_blocking_thread(move || validate_files_dir_setting_as(&gate, &raw))
                .await
                .map_err(|e| format!("config set task join: {e}"))??;
        }
        Ok(Self { who })
    }

    /// After the save: the status answer for the same person, so the read
    /// right after the set shows it.
    pub(crate) async fn refresh_status(self) {
        let who = self.who;
        let _ = on_a_blocking_thread(move || refresh_effective_files_dir_as(&who)).await;
    }
}

/// `f` on a blocking thread, with what a test stood in on this one carried
/// to it, as [`FsAs::run`] carries it.
async fn on_a_blocking_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    #[cfg(all(test, target_os = "linux"))]
    let carried = stand_in::carried();
    tokio::task::spawn_blocking(move || {
        #[cfg(all(test, target_os = "linux"))]
        let _carried = carried.stand();
        f()
    })
    .await
}

/// Number of in-flight file transfers (incoming uploads + outgoing
/// downloads) summed across all DCs. The auto-updater reads this
/// before deciding whether to fire a scheduled install — see
/// `updater::run_periodic`. Incremented on `FilesHandler::begin` /
/// `resume_incoming` / `begin_outgoing` success; decremented via the
/// `Drop` impl on `IncomingTransfer` / `OutgoingTransfer` so every
/// exit path (success, error, panic, DC drop) releases the counter.
///
/// rc.19 added this static so the auto-update timer doesn't kill the
/// agent mid-upload (the field bug that motivated resumable
/// transfers in the first place — see plan
/// `~/.claude/plans/floating-splashing-nebula.md`).
pub static ACTIVE_TRANSFERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn active_transfer_count() -> usize {
    ACTIVE_TRANSFERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-global registry of partial-upload staging dirs keyed by
/// transfer `id`. Populated by three writers:
///
/// 1. [`sweep_orphans`] at startup — walks known dest roots, registers
///    surviving `.roomler-partial/<id>/` dirs (after deleting any with
///    `created_at_unix` > 24h).
/// 2. [`FilesHandler::begin`] when a resumable upload starts — writes
///    `id → meta.json_path` so a same-process resume request on a
///    different DC can find the partial.
/// 3. [`FilesHandler::commit_partial`] (rename success) and
///    [`FilesHandler::cancel_incoming`] / error paths — remove the
///    registry entry alongside the on-disk staging cleanup.
///
/// rc.19 uses `LazyLock` over `OnceLock + get_or_init` because the
/// initial value is a closure-free `Mutex::new(HashMap::new())`. The
/// inner `std::sync::Mutex` is fine for HashMap lookups (sub-µs
/// critical section); no tokio async needed.
///
/// **Test isolation**: `cargo test` runs in parallel by default and
/// the registry is process-global, so use [`reset_partial_registry`]
/// at the start of every test that touches partial state.
pub(crate) static PARTIAL_REGISTRY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PartialEntry>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// One registered partial: its `meta.json`, and who staged it. FR-97 P3d —
/// a resume, a cancel and the sweep act on the partial AS whoever staged
/// it: in the person's own folder, `meta.json` is theirs to edit, and a
/// resume renames to the folder it names.
#[derive(Debug, Clone)]
pub(crate) struct PartialEntry {
    pub(crate) meta_path: PathBuf,
    pub(crate) who: FsAs,
}

/// Test-only escape hatch — clears the partial registry between
/// `cargo test` cases that touch the same global. Production code
/// never calls this.
///
/// Recovers from poison: cargo runs tests in parallel and a panic
/// in one test holding the lock would poison it for all subsequent
/// readers. The HashMap has no invariants a panic could break, so
/// recovering the inner value is sound.
#[cfg(test)]
pub fn reset_partial_registry() {
    let mut g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    g.clear();
}

/// Per-partial metadata persisted next to the staging `data` file as
/// `<dest_dir>/.roomler-partial/<id>/meta.json`. Survives agent
/// restart so [`sweep_orphans`] can rebuild [`PARTIAL_REGISTRY`] from
/// disk at startup. `protocol_version` is bumped if the on-disk
/// format ever changes (v2 will add per-chunk SHA roots).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PartialMeta {
    pub protocol_version: u32,
    pub filename: String,
    pub expected_size: u64,
    pub dest_dir: PathBuf,
    /// The begin-time reserved final path. Advisory only — the real
    /// rename target is recomputed via `unique_path()` at end time
    /// per M4 fix (operator may have created a file at this path
    /// while the upload was in flight).
    pub reserved_final_path: PathBuf,
    pub created_at_unix: i64,
    /// Folder-upload context. `Some` when the begin envelope carried
    /// `rel_path`; needed at end time to recreate the directory tree
    /// for collision-resolution.
    #[serde(default)]
    pub rel_path: Option<String>,
    /// #3048 — the person a Windows transfer began for (their SID): a resume
    /// places the file only for them. `None` from a transfer written as the
    /// daemon, and from a daemon before #3048; additive both ways.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_sid: Option<String>,
}

/// Age threshold for [`sweep_orphans`] — partials older than this
/// get their dir + sidecar deleted at agent startup.
pub(crate) const PARTIAL_ORPHAN_TTL_SECS: i64 = 24 * 3600;

/// Granularity at which `chunk()` calls `sync_data()` so that the
/// page cache is flushed to disk at sub-1-MiB intervals.
///
/// rc.24 — was 1 MiB. the field-test host field repro showed sub-1-MiB files
/// never had a sync point mid-upload; if the agent process was
/// killed before `end()` flushed (e.g. msiexec auto-update,
/// SCM service restart), the file stayed 0 bytes on disk and
/// resume couldn't make progress because `bytesAcked` reset to 0
/// each cycle. Drop to 256 KiB so even small files persist
/// incrementally. Overhead on Windows: 4× more `FlushFileBuffers`
/// per MB ≈ 4-120 ms per MB vs the prior 1-30 ms; acceptable
/// because resume durability is the harder problem to solve.
pub(crate) const FSYNC_THRESHOLD_BYTES: u64 = 256 * 1024;

/// rc.22 — staging-strategy flag. When `true`, all upload staging on
/// Windows happens under `%PROGRAMDATA%\roomler\<segment>\staging\`
/// regardless of the upload's `dest_dir`. When `false`, staging lives
/// under `<dest_dir>/.roomler-partial/` (rc.19–rc.21 legacy layout).
///
/// **Why**: the field-test host field repro 2026-05-11 — ESET Security real-time
/// scanner intercepts per-chunk writes under `C:\Users\<user>\Downloads\`
/// aggressively enough to push the file-DC's SCTP buffer into overflow
/// during large uploads, ending in "reconnect budget exhausted after
/// 6 attempts" with no recoverable state. ESET scans `C:\ProgramData\`
/// writes less aggressively (service-writable, typical legitimate
/// staging location), so moving the per-chunk write loop to PROGRAMDATA
/// and only crossing into the user's Downloads at end-time rename keeps
/// ESET's hot path narrow.
///
/// Hypothesis-driven, not yet confirmed on the field; the env-var
/// escape hatch `ROOMLERD_STAGING_LEGACY_PER_DEST=1` reverts the
/// behavior without a rebuild if Option B turns out wrong. Cycles
/// back to `Option D` (escalate corporate-AV exclusion request) if
/// rc.22 still fails.
///
/// Defaults: Windows → `true`, other → `false`. Tests force `false`
/// via `cfg(test)` so the existing per-dest test assertions hold.
///
/// `dead_code` allow on non-Windows: the static + accessor are only
/// READ from inside `#[cfg(target_os = "windows")]` blocks elsewhere
/// in this file; on Linux/macOS the compiler can't see those reads
/// and would warn. We keep the symbols cross-platform so the
/// strategy-flag tests stay portable.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) static STAGE_IN_PROGRAMDATA: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| {
        if cfg!(test) {
            return false;
        }
        if node_env_os("STAGING_LEGACY_PER_DEST").is_some() {
            tracing::info!("files: STAGING_LEGACY_PER_DEST set; reverting to per-dest staging");
            return false;
        }
        cfg!(target_os = "windows")
    });

/// True when the rc.22 always-PROGRAMDATA staging strategy is active.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn stage_in_programdata() -> bool {
    *STAGE_IN_PROGRAMDATA
}

// RETIRED-NAME-ANCHOR(3): names the pre-rename segment because explaining the
// conditional resolution requires naming both halves. docs/fr/FR-21
/// Windows-only staging root: `%PROGRAMDATA%\roomler\<segment>\staging\`,
/// where `<segment>` is resolved per host by `appdirs::machine_global_dir()`
/// (the `roomler` segment on a fresh install; the pre-rename `roomler-agent`
/// one only where that tree already exists). Do NOT restate it as a literal —
/// FR-21 P4 fixed a viewer that did exactly that.
/// SYSTEM-writable, persistent across reboots, present on every Windows
/// install. Rooted in the machine's `ProgramData` folder as the system
/// names it (`appdirs::machine_program_data`), never in a variable.
#[cfg(target_os = "windows")]
pub(crate) fn staging_root_windows() -> PathBuf {
    crate::appdirs::machine_global_dir().join("staging")
}

/// #3688 — the most bytes an upload's id may have ([`transfer_id`]).
pub(crate) const TRANSFER_ID_MAX_BYTES: usize = 128;

/// #3688 — what an id that is not one plain name is told ([`transfer_id`]).
/// The viewer shows it in its `files:error`.
pub(crate) const NOT_A_TRANSFER_ID: &str = "the transfer id is not valid";

/// #3688 — an upload's id names its staging folder, so it is one plain name:
/// letters, digits, `.`, `_` and `-` only, not starting with `.`, and at most
/// [`TRANSFER_ID_MAX_BYTES`]. The viewer's ids (`up-<base36>-<base36>`) are
/// such names. Every entry that takes an id from the viewer checks it first
/// (`begin_as`, `resume_incoming_as`, `cancel_incoming`, `end`), and
/// [`partial_dir_for`] builds a path from no other id.
pub(crate) fn transfer_id(id: &str) -> Result<&str> {
    let plain = !id.is_empty()
        && id.len() <= TRANSFER_ID_MAX_BYTES
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if plain {
        Ok(id)
    } else {
        Err(anyhow!(NOT_A_TRANSFER_ID))
    }
}

/// Compute the canonical staging dir path for an upload `id`. On
/// Windows in production this is `%PROGRAMDATA%\roomler\<segment>\
/// staging\<id>` (rc.22, ESET fix; segment per `machine_global_dir()`). On other platforms or when the
/// legacy-per-dest escape hatch is set, falls back to the rc.19 layout
/// at `<dest_dir>/.roomler-partial/<id>`. #3688 — an id that is not one plain
/// name ([`transfer_id`]) is refused here too.
pub(crate) fn partial_dir_for(dest_dir: &std::path::Path, id: &str) -> Result<PathBuf> {
    let id = transfer_id(id)?;
    #[cfg(target_os = "windows")]
    {
        if stage_in_programdata() {
            return Ok(staging_root_windows().join(id));
        }
    }
    Ok(dest_dir.join(".roomler-partial").join(id))
}

/// #3688 — how a received file's own files (its partial, the partial's
/// notes) are created: new, so a link planted in their place refuses the
/// open, and with `private` (the daemon alone) 0600 from the first byte. The
/// umask can only take bits away, and nothing changes the mode after.
fn new_file(private: bool) -> std::fs::OpenOptions {
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    open
}

/// rc.22 — detect a "rename crosses devices" error from `tokio::fs::rename`.
/// `io::ErrorKind::CrossesDevices` (stable since Rust 1.85) catches the
/// portable case. For older toolchains or platforms without the
/// canonical mapping, fall back to checking the raw OS error code:
/// `17` on Windows (`ERROR_NOT_SAME_DEVICE`) and `18` on Linux/macOS
/// (`EXDEV`).
pub(crate) fn is_cross_volume_error(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::CrossesDevices {
        return true;
    }
    match err.raw_os_error() {
        Some(17) if cfg!(target_os = "windows") => true,
        Some(18) if cfg!(not(target_os = "windows")) => true,
        _ => false,
    }
}

/// Apply the Windows hidden attribute to `path`. Logs + continues on
/// failure (the staging path still works without the hidden bit;
/// operators on Linux/macOS get the dot-prefix hide instead).
#[cfg(target_os = "windows")]
fn set_hidden(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: wide is a valid null-terminated UTF-16 string from a
    // PathBuf. FILE_ATTRIBUTE_HIDDEN = 0x2.
    let ok =
        unsafe { windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(wide.as_ptr(), 0x2) };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!(path = %path.display(), %err, "files: SetFileAttributesW failed; staging dir visible");
    }
}

#[cfg(not(target_os = "windows"))]
fn set_hidden(_path: &std::path::Path) {
    // Unix relies on the dot-prefix; nothing to do.
}

/// Walk the Downloads folder's `.roomler-partial/` dir at agent
/// startup, deleting any per-id subdir whose `meta.json` is older
/// than [`PARTIAL_ORPHAN_TTL_SECS`] and registering the survivors in
/// [`PARTIAL_REGISTRY`]. Returns the count of (kept, swept) pairs.
///
/// Called synchronously from `app.rs::run_cmd` BEFORE the signaling
/// task spawns, so no DC can carry a `files:resume` message until
/// the registry is populated. This closes the B1 race documented in
/// the plan.
///
/// **Coverage limitation**: this v1 sweep only walks Downloads. For
/// uploads with explicit `dest_path` to arbitrary locations, the
/// in-memory [`PARTIAL_REGISTRY`] populated at begin time covers
/// same-process resume. After agent restart those partials are
/// undiscoverable and the browser falls back to a fresh `files:begin`
/// with a new id; the orphan dirs are left on disk until the operator
/// cleans them (no automated cleanup for non-Downloads in v1).
pub async fn sweep_orphans() -> (usize, usize) {
    // rc.22 — when always-PROGRAMDATA staging is active, the sweep
    // scans `staging_root_windows()` DIRECTLY (no `.roomler-partial/`
    // parent — the staging root IS that parent). Otherwise fall back
    // to the legacy `<Downloads>/.roomler-partial/` layout.
    #[cfg(target_os = "windows")]
    {
        if stage_in_programdata() {
            let root = staging_root_windows();
            return sweep_orphans_root(&root).await;
        }
    }
    // FR-84 D4 — a partial lives next to the folder its transfer targeted:
    // walk the default folder AND the configured one (when set, usable and
    // different), so a `files_dir` change strands no resumable upload.
    // FR-97 P3d — for, and as, whoever a drop would be written as now: on a
    // Linux system install, the person at seat0, in their own folder. #3711 —
    // signalling waits for this sweep, so that is asked off the async worker
    // and waited for 1.5 s at most; with no answer in time, nothing is swept
    // this start.
    let who = FsAs::for_write().await;
    let w = who.clone();
    let (roots, private) = who
        .run(move || {
            let mut roots: Vec<PathBuf> = Vec::new();
            if let Ok(d) = default_download_dir_as(&w, false) {
                roots.push(d);
            }
            // Only a folder that still passes for this writer — the sweep
            // deletes, so it must never walk a configured value the
            // placement rule refuses.
            if let Ok(Some(c)) = configured_folder_without_creating(&w)
                && !roots.contains(&c)
            {
                roots.push(c);
            }
            // R4 — and the writer's own private staging folder, whose
            // children are partials.
            #[cfg(target_os = "linux")]
            let private = private_staging_root(&w);
            #[cfg(not(target_os = "linux"))]
            let private: Option<PathBuf> = None;
            Ok((roots, private))
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(%e, "sweep_orphans: could not resolve the drop folders as their writer");
            (Vec::new(), None)
        });
    if roots.is_empty() && private.is_none() {
        tracing::debug!("sweep_orphans: Downloads dir not resolvable; skipping");
        return (0, 0);
    }
    let (mut kept, mut swept) = (0, 0);
    let walked = roots
        .iter()
        .map(|root| root.join(".roomler-partial"))
        .chain(private);
    for dir in walked {
        let (k, s) = sweep_orphans_dir_as(&who, dir).await;
        kept += k;
        swept += s;
    }
    // #3688 — and the empty folders `staging::keeps_modes`'s probe left in a
    // drop folder (a crash between their make and their removal): nothing a
    // probe makes stays.
    #[cfg(target_os = "linux")]
    {
        let probes = who
            .run(move || {
                Ok(roots
                    .iter()
                    .map(|root| staging::sweep_probes(root))
                    .sum::<usize>())
            })
            .await;
        if let Err(e) = probes {
            tracing::debug!(%e, "sweep_orphans: probe folders not swept");
        }
    }
    (kept, swept)
}

/// rc.22 — sweep a directory whose CHILDREN are per-id staging dirs.
/// Used when the staging root is `%PROGRAMDATA%\roomler\<segment>\
/// staging\` (no `.roomler-partial` parent). Mirror of [`sweep_orphans_in`]
/// minus the parent-join — the contents-of-directory loop is identical.
pub async fn sweep_orphans_root(dir: &std::path::Path) -> (usize, usize) {
    sweep_orphans_dir_as(&FsAs::Daemon, dir.to_path_buf()).await
}

/// Test-friendly variant taking an explicit root. Production code
/// calls [`sweep_orphans`] which forwards to this with `download_dir()`.
/// Tests that exercise the sweep logic pass a `tempfile::TempDir`
/// root to avoid polluting the user's real Downloads folder on
/// Windows (where `download_dir()` ignores HOME and resolves via
/// `KNOWNFOLDERID`).
pub async fn sweep_orphans_in(root: &std::path::Path) -> (usize, usize) {
    sweep_orphans_dir_as(&FsAs::Daemon, root.join(".roomler-partial")).await
}

/// Shared sweep body — reads `dir` as a directory whose children are
/// `<id>/{meta.json,data}` triples, removes those older than
/// [`PARTIAL_ORPHAN_TTL_SECS`] and registers survivors. Used by both
/// the legacy `<Downloads>/.roomler-partial/` layout and the rc.22
/// `%PROGRAMDATA%\roomler\<segment>\staging\` layout. FR-97 P3d — the
/// walk and every removal run as `who`, and a survivor is registered as
/// staged by `who`, so a later resume acts as them too.
async fn sweep_orphans_dir_as(who: &FsAs, dir: PathBuf) -> (usize, usize) {
    let now = chrono::Utc::now().timestamp();
    let walked = dir.clone();
    let (survivors, swept) = match who
        .run(move || Ok(sweep_orphans_dir_here(&walked, now)))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), %e, "sweep_orphans: could not act as the folder's writer");
            return (0, 0);
        }
    };
    let kept = survivors.len();
    if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
        for (id, meta_path) in survivors {
            g.insert(
                id,
                PartialEntry {
                    meta_path,
                    who: who.clone(),
                },
            );
        }
    }
    if kept + swept > 0 {
        tracing::info!(kept, swept, "sweep_orphans: complete");
    }
    (kept, swept)
}

/// The sweep's filesystem half, on THIS (blocking) thread under the
/// caller's identity: the survivors as `(id, meta.json)`, and how many
/// partials were removed. #3688 — on Linux a partial is a survivor only
/// when it is its stager's own and nobody else may write it, every step
/// opened without following a link (`staging::sweep`).
#[cfg(target_os = "linux")]
fn sweep_orphans_dir_here(dir: &std::path::Path, now: i64) -> (Vec<(String, PathBuf)>, usize) {
    staging::sweep(dir, now, PARTIAL_ORPHAN_TTL_SECS)
}

/// [`sweep_orphans_dir_here`] elsewhere, by path.
#[cfg(not(target_os = "linux"))]
fn sweep_orphans_dir_here(dir: &std::path::Path, now: i64) -> (Vec<(String, PathBuf)>, usize) {
    let mut survivors = Vec::new();
    let mut swept = 0usize;
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(dir = %dir.display(), "sweep_orphans: dir absent; nothing to do");
            return (survivors, 0);
        }
        Err(e) => {
            tracing::warn!(dir = %dir.display(), %e, "sweep_orphans: read_dir failed");
            return (survivors, 0);
        }
    };
    for entry in rd.flatten() {
        let path = entry.path();
        // `DirEntry::file_type` does not follow a link: a link to a folder
        // is skipped, never walked.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let id = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let meta_path = path.join("meta.json");
        let meta_bytes = match std::fs::read(&meta_path) {
            Ok(b) => b,
            Err(_) => {
                // No meta.json — partial state corrupt, prune.
                let _ = std::fs::remove_dir_all(&path);
                swept += 1;
                continue;
            }
        };
        let meta: PartialMeta = match serde_json::from_slice(&meta_bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(meta = %meta_path.display(), %e, "sweep_orphans: corrupt meta.json; removing");
                let _ = std::fs::remove_dir_all(&path);
                swept += 1;
                continue;
            }
        };
        if now - meta.created_at_unix > PARTIAL_ORPHAN_TTL_SECS {
            tracing::info!(
                id = %id,
                age_h = (now - meta.created_at_unix) / 3600,
                "sweep_orphans: removing orphaned partial"
            );
            let _ = std::fs::remove_dir_all(&path);
            swept += 1;
            continue;
        }
        survivors.push((id, meta_path));
    }
    (survivors, swept)
}

/// Incoming control messages over the `files` DC (string payloads).
/// Binary payloads are handled separately — they're not JSON.
///
/// File-DC v2 (0.3.0) adds reverse-direction transfers (`Get`,
/// `Cancel`) and directory listing (`Dir`) on top of the original
/// upload pair. Old browsers continue to send only `Begin` / `End`
/// — backwards-compat is preserved by serde's `#[serde(tag = "t")]`
/// rejecting unknown variants on the agent side and the agent
/// emitting only the original variants when the operator never
/// triggers a download.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
pub(crate) enum FilesIncoming {
    #[serde(rename = "files:begin")]
    Begin {
        id: String,
        name: String,
        size: u64,
        #[serde(default)]
        mime: Option<String>,
        /// Folder-upload extension (file-DC v2.1). When `Some`, the
        /// browser is dropping a folder and this is the relative path
        /// of the file inside that folder, e.g.
        /// `MyFolder/sub/file.txt`. The agent recreates the directory
        /// structure under Downloads/<root> with per-component
        /// sanitisation (reuses the zip walker's safety rules).
        /// `None` for individual-file uploads (the original behavior;
        /// `name` is the basename, file lands in Downloads/).
        #[serde(default)]
        rel_path: Option<String>,
        /// Path-targeted upload extension (file-DC v2.2). When
        /// `Some(<host_dir>)`, the file lands under `<host_dir>/`
        /// instead of the default Downloads/. The browser sets this
        /// when the operator drops a file onto the browse drawer's
        /// current directory — natural completion of browse +
        /// upload. Validated via the same denylist as `files:get`
        /// downloads (rejects `\\?\GLOBALROOT…`, registry hives,
        /// non-directory paths). When `None` the file lands in
        /// Downloads/ as before. Stacks with `rel_path` for folder-
        /// drop-into-arbitrary-dir.
        #[serde(default)]
        dest_path: Option<String>,
    },
    #[serde(rename = "files:end")]
    End { id: String },
    /// Browser → Agent: download a single file from the host.
    /// Path is the absolute host path. Subject to denylist + read
    /// permission of the agent process.
    #[serde(rename = "files:get")]
    Get { id: String, path: String },
    /// Browser → Agent: download a folder as a streaming zip
    /// (Phase 4). `format` is currently always `"zip"`.
    #[serde(rename = "files:get-folder")]
    GetFolder {
        id: String,
        path: String,
        #[serde(default)]
        format: Option<String>,
    },
    /// Browser → Agent: cancel an in-flight transfer. file-DC v3
    /// (rc.19) extends this to also cancel incoming uploads — the
    /// agent removes the in-flight `IncomingTransfer` state AND the
    /// `.roomler-partial/<id>/` staging dir + PARTIAL_REGISTRY entry.
    /// Sent by the browser when the resume reconnect budget is
    /// exhausted (6 attempts) so the partial doesn't sit until the
    /// 24h orphan sweep.
    #[serde(rename = "files:cancel")]
    Cancel { id: String },
    /// Browser → Agent: list a directory. An EMPTY `path` (or the legacy
    /// `~`) lists the roots — the logical drives on Windows, a single `/`
    /// on Unix. Every other path, `/` included, is a directory to list:
    /// `/` used to answer with the roots again, so the roots view's one
    /// entry on a Mac or Linux host led nowhere (#2870). `req_id` echoes
    /// back in the `dir-list` reply so the browser can match concurrent
    /// requests.
    #[serde(rename = "files:dir")]
    Dir { req_id: String, path: String },
    /// Browser → Agent: resume an upload that lost its DC mid-flight
    /// (file-DC v3, rc.19). The browser claims `bytesAcked` from the
    /// last `files:progress` envelope; the agent replies with
    /// `files:resumed { id, accepted_offset }` where `accepted_offset`
    /// may be < requested if the agent's on-disk size is smaller
    /// (truncated to a 256 KiB boundary matching `files:progress`
    /// cadence). On any failure the agent replies `files:error` and
    /// the browser falls back to a fresh `files:begin` with a new id.
    /// `sha256_prefix` is reserved for v2 (per-chunk integrity); v1
    /// agents ignore the field if present.
    #[serde(rename = "files:resume")]
    Resume {
        id: String,
        offset: u64,
        #[serde(default)]
        sha256_prefix: Option<String>,
    },
}

/// Outgoing control messages sent back to the browser. Flat `t`
/// discriminant mirrors the clipboard DC's pattern for consistency.
#[derive(Debug, Serialize)]
#[serde(tag = "t")]
#[allow(dead_code)] // DirList is wired in Phase 3 (browse drawer)
pub(crate) enum FilesOutgoing<'a> {
    #[serde(rename = "files:accepted")]
    Accepted { id: &'a str, path: &'a str },
    #[serde(rename = "files:progress")]
    Progress { id: &'a str, bytes: u64 },
    #[serde(rename = "files:complete")]
    Complete {
        id: &'a str,
        path: &'a str,
        bytes: u64,
    },
    #[serde(rename = "files:error")]
    Error { id: &'a str, message: &'a str },
    /// Agent → Browser: announce an outgoing transfer. `size` is
    /// `Some` for single files (known up-front) and `None` for
    /// streaming folder zips (Phase 4 — size unknown until end).
    #[serde(rename = "files:offer")]
    Offer {
        id: &'a str,
        name: &'a str,
        size: Option<u64>,
        mime: Option<&'a str>,
    },
    /// Agent → Browser: terminal frame for an outgoing transfer.
    /// Sent after the last binary chunk; carries the final byte
    /// count so the browser can verify a complete stream.
    #[serde(rename = "files:eof")]
    Eof { id: &'a str, bytes: u64 },
    /// Agent → Browser: directory listing reply. `parent` is the
    /// canonical parent path (or `None` at the root / drives view).
    /// Permission-denied per-entry results are dropped from the
    /// list silently — best-effort so a single inaccessible file
    /// doesn't sink the listing.
    #[serde(rename = "files:dir-list")]
    DirList {
        req_id: &'a str,
        path: &'a str,
        parent: Option<&'a str>,
        entries: &'a [DirEntryView],
    },
    /// Agent → Browser: directory listing failed (path doesn't
    /// exist, permission denied, browse disabled by config).
    #[serde(rename = "files:dir-error")]
    DirError { req_id: &'a str, message: &'a str },
    /// Agent → Browser: reply to `files:resume` confirming the byte
    /// offset from which the agent will accept appended chunks
    /// (file-DC v3, rc.19). `accepted_offset` is the largest 256 KiB-
    /// aligned offset ≤ the browser's requested offset AND ≤ the
    /// agent's on-disk size for the partial file. The browser MUST
    /// re-pump from `accepted_offset`, which may be 0 (full re-send)
    /// if the agent's partial was truncated to nothing or the staging
    /// file is gone. After this reply the browser sends a normal
    /// `files:end` once it has pumped the remaining bytes.
    #[serde(rename = "files:resumed")]
    Resumed { id: &'a str, accepted_offset: u64 },
}

/// Listing entry surfaced over `files:dir-list`. Sorted dirs-first
/// then files, both alphabetical case-insensitive. Permission-denied
/// entries are dropped from the result rather than failing the
/// whole list.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct DirEntryView {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime_unix: Option<i64>,
}

/// RAII guard that increments [`ACTIVE_TRANSFERS`] on creation and
/// decrements on drop. Embedded in `IncomingTransfer` /
/// `OutgoingTransfer` so every exit path (success, error, panic,
/// DC drop, abort) releases the counter automatically.
///
/// Pair with `sync_data` per [`FSYNC_THRESHOLD_BYTES`] (B2 fix) so
/// the counter only decrements after bytes are durable on disk;
/// otherwise a kill-between-write-and-sync could lose data while
/// the updater observed `active == 0` and fired an install.
pub(crate) struct ActiveTransferGuard;

impl ActiveTransferGuard {
    pub fn new() -> Self {
        ACTIVE_TRANSFERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for ActiveTransferGuard {
    fn drop(&mut self) {
        ACTIVE_TRANSFERS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Browser → Agent upload state. A single incoming transfer is
/// "active" at any time — files:begin starts one; files:end or the
/// DC closing finishes it.
pub(crate) struct IncomingTransfer {
    pub id: String,
    pub expected: u64,
    pub received: u64,
    pub file: File,
    /// Last byte count reported via files:progress. Progress is sent
    /// every ~256 KiB to keep the browser UI lively without flooding.
    pub last_progress: u64,
    /// rc.19: staging dir `<dest_dir>/.roomler-partial/<id>/`. The
    /// `data` file is inside; `meta.json` is alongside. `end()`
    /// renames `<dir>/data` to a fresh unique_path under `dest_dir`
    /// and removes this dir + the registry entry.
    pub partial_dir: PathBuf,
    /// rc.19: target directory captured at begin time so `end()` can
    /// re-run `unique_path()` against it (M4 fix).
    pub dest_dir: PathBuf,
    /// rc.19: sanitized basename captured at begin time for the same
    /// reason as `dest_dir`.
    pub filename: String,
    /// rc.19: byte count at the most recent `sync_data()` call. The
    /// `chunk()` path advances this every [`FSYNC_THRESHOLD_BYTES`]
    /// so kernel writeback is durable at the moment we ack progress
    /// over the wire (B2 fix — survives msiexec TerminateProcess).
    pub last_synced: u64,
    /// rc.19: folder-upload context preserved for end-time rename
    /// (used by `commit_partial` to recreate the directory tree).
    pub rel_path: Option<String>,
    /// FR-97 P3d: who this transfer writes as, fixed when it began (or
    /// resumed): the end-time rename and its cleanup run as them too, even
    /// after someone else has signed in at the screen.
    pub who: FsAs,
    /// #3048: who staged it. The daemon, when uploads stage in the machine's
    /// own folder and `who` is the person: the end then copies across, from
    /// a source the daemon opens into a file the person creates. Otherwise
    /// `who`.
    pub stager: FsAs,
    /// #3688: its staging, held from its begin (or resume) to its end, so
    /// the end places from the folders it began in, never from whatever has
    /// those names by then (Linux; nothing elsewhere).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub held: HeldStaging,
    /// rc.19: increments [`ACTIVE_TRANSFERS`] on creation, decrements
    /// on drop. Last field so it drops AFTER `file` — guarantees the
    /// kernel page cache has reached the disk-flush queue before the
    /// updater sees `active == 0`.
    pub _active_guard: ActiveTransferGuard,
}

/// Agent → Browser download state. One outgoing transfer is active
/// at any time. `cancel` stops its pump: a `files:cancel` from the
/// browser, or the session ending ([`FilesHandler::end_session`]).
pub(crate) struct OutgoingTransfer {
    pub id: String,
    pub path: PathBuf,
    /// Captured file size from the begin_outgoing stat. Useful for
    /// future per-transfer audit logging; not consulted by the pump
    /// today (which reads-until-EOF).
    #[allow(dead_code)]
    pub size: u64,
    /// #3669 — a download's file, opened as its reader when it began; the
    /// pump takes it ([`FilesHandler::open_outgoing`]). None for a folder,
    /// whose walk opens each file itself.
    pub file: Option<std::fs::File>,
    /// #1730 — a TOKEN, not a flag: the pumps race every await against it.
    /// A flag read between awaits was never read by a pump parked inside
    /// one (a send on a channel whose peer vanished never returns), so a
    /// download cut by a dropped session held its file, its channel and
    /// this transfer's updater guard for good.
    pub cancel: tokio_util::sync::CancellationToken,
    /// rc.19: ACTIVE_TRANSFERS counter guard (see IncomingTransfer).
    pub _active_guard: ActiveTransferGuard,
}

/// Handle on the file-transfer subsystem for one data channel.
/// Thread-safe — cheap Arc clones are used inside the on_message and
/// on_close callbacks on the DC.
///
/// Incoming and outgoing transfers each get their own mutex so a
/// busy upload doesn't lock the download path (and vice versa).
/// One outgoing AND one incoming transfer can run concurrently;
/// queueing within a direction is enforced client-side.
#[derive(Clone)]
pub struct FilesHandler {
    incoming: Arc<Mutex<Option<IncomingTransfer>>>,
    outgoing: Arc<Mutex<Option<OutgoingTransfer>>>,
    /// Set by [`Self::end_session`]: nothing new begins on a session that
    /// is over. A message already in flight when the session ended would
    /// otherwise start a download after the teardown, whose pump outlives
    /// it (found in review; the clipboard's `stopped` is the same guard).
    ended: Arc<AtomicBool>,
    /// #3048 — a test's own staging root, standing in for the machine one
    /// ([`staging_root_windows`]), so the split between staging and placing
    /// runs on any machine. `None` = [`partial_dir_for`]'s rule.
    #[cfg(test)]
    staging_root: Option<PathBuf>,
}

impl Default for FilesHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesHandler {
    pub fn new() -> Self {
        Self {
            incoming: Arc::new(Mutex::new(None)),
            outgoing: Arc::new(Mutex::new(None)),
            ended: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            staging_root: None,
        }
    }

    /// #3048 — a handler that stages in `root`, as a Windows install stages in
    /// the machine's own folder.
    #[cfg(all(test, target_os = "windows"))]
    pub(crate) fn with_staging_root(root: PathBuf) -> Self {
        Self {
            staging_root: Some(root),
            ..Self::new()
        }
    }

    /// The staging folder of upload `id` that targets `dest_dir`; #3688 —
    /// refused for an id that is not one plain name, as [`partial_dir_for`].
    fn partial_dir(&self, dest_dir: &std::path::Path, id: &str) -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(root) = &self.staging_root {
            return Ok(root.join(transfer_id(id)?));
        }
        partial_dir_for(dest_dir, id)
    }

    /// Uploads stage in a folder of the daemon's own (the machine's on
    /// Windows), not beside the target.
    fn stages_in_a_root(&self) -> bool {
        #[cfg(test)]
        if self.staging_root.is_some() {
            return true;
        }
        cfg!(target_os = "windows") && stage_in_programdata()
    }

    /// #3048 — who stages a transfer written as `who`: the daemon, in its own
    /// folder, which the person may not write; `who` when staging sits beside
    /// the target (every step then is theirs, as FR-97 P3d's).
    fn stager_for(&self, who: &FsAs) -> FsAs {
        if self.stages_in_a_root() {
            FsAs::Daemon
        } else {
            who.clone()
        }
    }

    /// Start a new incoming transfer (browser → host upload). Returns
    /// the absolute destination path so the caller can reply
    /// `files:accepted { id, path }`.
    ///
    /// `rel_path` is the file-DC v2.1 folder-upload extension. When
    /// `Some(<rel>)`, the browser is uploading a single file from a
    /// dropped folder and `rel` is its path relative to the folder
    /// root (e.g. `MyFolder/sub/file.txt`). The agent recreates the
    /// directory structure under Downloads/ with per-component
    /// sanitisation. When `None`, behaviour is the file-DC v1 default:
    /// `name` is the basename and the file lands in Downloads/
    /// directly (with collision-safe rename).
    ///
    /// `dest_path` is the file-DC v2.2 path-targeted-upload extension.
    /// When `Some(<host_dir>)`, the file lands under `<host_dir>/`
    /// instead of Downloads/. Subject to the existing
    /// [`validate_outgoing_path`] denylist (kernel-namespace prefixes,
    /// registry-hive container) plus a directory-existence check.
    /// On validation failure the file falls back to Downloads/ with
    /// a warning log (operator's intent was clear, refusing entirely
    /// would be confusing). Stacks with `rel_path` for
    /// folder-drop-into-arbitrary-dir.
    pub async fn begin(
        &self,
        id: String,
        name: String,
        expected: u64,
        rel_path: Option<&str>,
        dest_path: Option<&str>,
    ) -> Result<PathBuf> {
        // FR-97 P3d — who this transfer writes as, for its whole life: on a
        // Linux system install the person at seat0, in their own folder.
        self.begin_as(
            FsAs::for_write().await,
            id,
            name,
            expected,
            rel_path,
            dest_path,
        )
        .await
    }

    /// [`Self::begin`] as an explicit identity: the seam the root-only tests
    /// use, since a test runner has nobody at seat0.
    pub(crate) async fn begin_as(
        &self,
        who: FsAs,
        id: String,
        name: String,
        expected: u64,
        rel_path: Option<&str>,
        dest_path: Option<&str>,
    ) -> Result<PathBuf> {
        // #3688 — the id names this upload's staging folder: one plain name,
        // checked before anything else.
        transfer_id(&id)?;
        // Before anything lands on disk (a message straddling the session's
        // end); an upload that got past this is dropped by `end_session`.
        self.refuse_if_ended()?;
        // #3048 — nobody signed in to a Windows worker's session: refused, in
        // the words the viewer shows, before anything is touched.
        if let Some(why) = who.refusal() {
            return Err(anyhow!(why));
        }
        if expected > MAX_TRANSFER_BYTES {
            return Err(anyhow!(
                "transfer size {expected} exceeds the {} B cap",
                MAX_TRANSFER_BYTES
            ));
        }
        // Resolve the target directory: dest_path (operator-chosen,
        // validated) or Downloads (default). When dest_path is set
        // but invalid (denylist hit / not a directory), fall back to
        // Downloads with a warning so the upload doesn't silently
        // drop the operator's data. #3688 — a default folder that cannot
        // receive files says why in its own words, which the viewer shows
        // (it shows an error's outermost line only).
        // #3688 — and whether it is the daemon's own default folder, whose
        // rule its staging then holds the handle it opens to.
        let (target_dir, the_default) = match dest_path.filter(|p| !p.is_empty()) {
            Some(dp) => match resolve_dest_path_as(&who, dp).await {
                Ok(d) => (d, false),
                Err(e) => {
                    tracing::warn!(
                        dest_path = dp,
                        %e,
                        "files: dest_path rejected; falling back to Downloads"
                    );
                    drop_folder_for(&who).await?
                }
            },
            None => drop_folder_for(&who).await?,
        };
        #[cfg(not(target_os = "linux"))]
        let _ = the_default;
        let filename = sanitize_filename(&name);

        // rc.19: stage at <dest_dir>/.roomler-partial/<id>/data so a
        // mid-upload DC drop leaves the partial discoverable (B1 +
        // B2 fixes). `end()` renames to a freshly-resolved final path.
        let partial_dir = self.partial_dir(&target_dir, &id)?;
        // #3048 — staging in the daemon's own folder is the daemon's to write,
        // and the target folder the person's: the two halves below run as
        // each. Staged beside the target, both are `who` (FR-97 P3d).
        let stager = self.stager_for(&who);

        // The target folder's half, as `who`: folder upload resolves
        // `rel_path` to a path under the target dir, sanitising each
        // component, and falls back to flat upload when rel_path is empty /
        // missing / refused by sanitisation. This is the *reserved* final
        // path; the real target is recomputed at `end()` time per M4 fix.
        let reserved_final = {
            let (target_dir, filename) = (target_dir.clone(), filename.clone());
            let rel_for_path = rel_path.filter(|p| !p.is_empty()).map(str::to_string);
            who.run(move || {
                Ok(match rel_for_path.as_deref() {
                    Some(rel) => {
                        resolve_folder_upload_path(&target_dir, rel).unwrap_or_else(|| {
                            tracing::warn!(
                                rel_path = rel,
                                "files: rel_path rejected; falling back to flat upload"
                            );
                            unique_path(&target_dir, &filename)
                        })
                    }
                    None => unique_path(&target_dir, &filename),
                })
            })
            .await?
        };

        // The staging half, as the stager.
        let (partial_dir, file, held) = {
            let (target_dir, partial_dir, filename, id, reserved_final) = (
                target_dir.clone(),
                partial_dir.clone(),
                filename.clone(),
                id.clone(),
                reserved_final.clone(),
            );
            let rel_for_meta = rel_path.map(str::to_string);
            let person_sid = who.person_sid().map(str::to_string);
            // #3688 — the daemon alone (nobody at seat0): its files are
            // born readable by root only, in whatever folder they land.
            let private = stager.writes_alone();
            // #3688 — on Linux, staged and held by handles (`staging`);
            // the tests' own staging-root mode keeps the steps below.
            #[cfg(target_os = "linux")]
            let (by_handles, as_stager) = (!self.stages_in_a_root(), stager.clone());
            stager
                .run(move || {
                    #[cfg(target_os = "linux")]
                    if by_handles {
                        let meta = PartialMeta {
                            protocol_version: 1,
                            filename: filename.clone(),
                            expected_size: expected,
                            dest_dir: target_dir.clone(),
                            reserved_final_path: reserved_final.clone(),
                            created_at_unix: chrono::Utc::now().timestamp(),
                            rel_path: rel_for_meta,
                            person_sid,
                        };
                        let meta_bytes = serde_json::to_vec_pretty(&meta)
                            .context("serialising partial meta.json")?;
                        let (held, file) = stage_held(
                            &as_stager,
                            &target_dir,
                            &id,
                            &meta_bytes,
                            private,
                            the_default,
                        )?;
                        return Ok((held.dir_path().to_path_buf(), file, Some(held)));
                    }
                    // B1 fix — reject if an existing partial dir is on disk for
                    // this id. The browser MUST send `files:resume` instead, or
                    // generate a new UUID. Without this check the data file
                    // below would truncate the existing partial. The
                    // `PARTIAL_REGISTRY` lookup is an optimisation — the
                    // authoritative test is the on-disk dir. `symlink_metadata`:
                    // a link planted there counts as "on disk" too.
                    if std::fs::symlink_metadata(&partial_dir).is_ok() {
                        return Err(anyhow!(
                            "id {id} already has partial state; send files:resume"
                        ));
                    }

                    let parent_partial = partial_dir
                        .parent()
                        .expect("partial_dir has a parent by construction");
                    std::fs::create_dir_all(parent_partial)
                        .with_context(|| format!("creating {}", parent_partial.display()))?;
                    set_hidden(parent_partial);
                    // `create_dir`, never `_all`: this id's folder is made now,
                    // or the begin fails — an existing one is never adopted.
                    std::fs::create_dir(&partial_dir)
                        .with_context(|| format!("creating {}", partial_dir.display()))?;

                    // Write meta.json BEFORE opening the data file. If meta
                    // write fails the staging dir is left empty (cheap to clean).
                    let meta = PartialMeta {
                        protocol_version: 1,
                        filename: filename.clone(),
                        expected_size: expected,
                        dest_dir: target_dir.clone(),
                        reserved_final_path: reserved_final.clone(),
                        created_at_unix: chrono::Utc::now().timestamp(),
                        rel_path: rel_for_meta,
                        person_sid,
                    };
                    let meta_path = partial_dir.join("meta.json");
                    let meta_bytes = serde_json::to_vec_pretty(&meta)
                        .context("serialising partial meta.json")?;
                    if private {
                        use std::io::Write;
                        new_file(true)
                            .open(&meta_path)
                            .and_then(|mut meta| meta.write_all(&meta_bytes))
                    } else {
                        std::fs::write(&meta_path, &meta_bytes)
                    }
                    .with_context(|| format!("writing {}", meta_path.display()))?;

                    // `create_new`: the folder is new, so the file must be too;
                    // a link planted in its place refuses the open.
                    let data_path = partial_dir.join("data");
                    let file = new_file(private)
                        .open(&data_path)
                        .with_context(|| format!("creating {}", data_path.display()))?;
                    Ok((partial_dir, file, HeldStaging::default()))
                })
                .await?
        };
        let meta_path = partial_dir.join("meta.json");
        // The open file carries the identity's rights from here on; the
        // writes through it need no identity of their own.
        let file = File::from_std(file);

        // Populate the in-memory registry so a same-process resume
        // request (DC drop without agent restart) can find this
        // partial without re-running the orphan sweep. It is registered as
        // staged by the stager: a resume reads it and a cancel removes it as
        // them.
        if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
            g.insert(
                id.clone(),
                PartialEntry {
                    meta_path,
                    who: stager.clone(),
                },
            );
        }

        let mut guard = self.incoming.lock().await;
        if guard.is_some() {
            // A previous transfer on THIS DC was in-flight and never
            // got files:end (browser closed or error). Drop it; the
            // partial dir + meta.json remain on disk for a future
            // resume attempt (or the orphan sweep after 24h).
        }
        *guard = Some(IncomingTransfer {
            id,
            expected,
            received: 0,
            file,
            last_progress: 0,
            partial_dir,
            dest_dir: target_dir,
            filename,
            last_synced: 0,
            rel_path: rel_path.map(|s| s.to_string()),
            who,
            stager,
            held,
            _active_guard: ActiveTransferGuard::new(),
        });
        Ok(reserved_final)
    }

    /// Append binary data to the active incoming transfer. Returns
    /// the total byte count after this append, and whether this
    /// append crossed a progress-report threshold.
    pub async fn chunk(&self, data: &[u8]) -> Result<Option<ChunkProgress>> {
        let mut guard = self.incoming.lock().await;
        let Some(state) = guard.as_mut() else {
            // Chunk arrived without an active transfer. Browser sent
            // bytes before files:begin or after files:end — we choose
            // to drop rather than guess.
            return Err(anyhow!("no active transfer"));
        };
        state.received = state.received.saturating_add(data.len() as u64);
        if state.received > state.expected {
            return Err(anyhow!(
                "received {} bytes, expected {}",
                state.received,
                state.expected
            ));
        }
        state.file.write_all(data).await?;
        // rc.19 B2 fix — flush the kernel page cache to disk every
        // FSYNC_THRESHOLD_BYTES so a hard kill (msiexec
        // TerminateProcess, BSOD, power loss) loses at most one
        // window of progress, not the entire transfer. Tuned to
        // 1 MiB; see FSYNC_THRESHOLD_BYTES doc.
        if state.received - state.last_synced >= FSYNC_THRESHOLD_BYTES {
            // sync_data flushes file content + metadata required to
            // read it back, but skips timestamp metadata — faster
            // than sync_all and sufficient for our durability needs.
            // Failure is logged but not fatal; the next chunk will
            // try again. (Disk full / quota errors will surface on
            // the next write_all anyway.)
            if let Err(e) = state.file.sync_data().await {
                tracing::debug!(%e, "files: sync_data failed; will retry next window");
            }
            state.last_synced = state.received;
        }
        let progress = if state.received - state.last_progress >= 256 * 1024 {
            state.last_progress = state.received;
            Some(ChunkProgress {
                id: state.id.clone(),
                bytes: state.received,
            })
        } else {
            None
        };
        Ok(progress)
    }

    /// Finalize the active incoming transfer. Flushes + fsyncs the
    /// staging `data` file, renames it to a freshly-resolved unique
    /// final path under `dest_dir`, removes the `.roomler-partial/
    /// <id>/` dir + registry entry. Returns the *actual* final path
    /// (may differ from the begin-time reservation per M4 fix).
    pub async fn end(&self, id: &str) -> Result<(PathBuf, u64)> {
        // #3688 — one plain name, as every entry checks first; the upload in
        // flight is not touched.
        transfer_id(id)?;
        let mut guard = self.incoming.lock().await;
        let Some(mut state) = guard.take() else {
            return Err(anyhow!("no active transfer to end"));
        };
        if state.id != id {
            // Put the state back so we don't drop someone else's
            // transfer on an id mismatch.
            let wrong_id = state.id.clone();
            *guard = Some(state);
            return Err(anyhow!(
                "files:end id={id} but active transfer is {wrong_id}"
            ));
        }
        state.file.flush().await?;
        state.file.sync_all().await.ok();
        if state.received != state.expected {
            // Don't commit the partial — leave the staging dir on disk
            // so a future files:resume can complete it. Re-insert the
            // state slot is wrong (we already moved file out of state),
            // but the partial dir IS the durable state. Browser will
            // get files:error and may retry via resume.
            return Err(anyhow!(
                "short transfer: received {} of {} bytes",
                state.received,
                state.expected
            ));
        }
        // Drop the file handle BEFORE the rename — Windows refuses
        // to rename an open file. tokio::fs::File::drop just queues
        // close, so we explicitly await... actually drop is fine
        // because end() returns shortly after; if it ever races, the
        // commit_partial below would error and the caller surfaces
        // it. We could `drop(state.file)` explicitly for clarity:
        drop(state.file);

        // M4 fix: re-run unique_path at rename time so an operator
        // who created a colliding file mid-upload doesn't make us
        // overwrite their work. FR-97 P3d: every step in the target
        // folder runs as whoever began the transfer.
        let (dest_dir, filename, rel, partial_dir) = (
            state.dest_dir.clone(),
            state.filename.clone(),
            state.rel_path.clone(),
            state.partial_dir.clone(),
        );
        // #3688 — on Linux, placed from the held staging into the held
        // target, never by path: a folder moved aside since and another put
        // in its place changes nothing. A root writer places only in a
        // folder the rule gives it now, and its folder upload follows no
        // link on the way.
        #[cfg(target_os = "linux")]
        if let Some(held) = state.held.take() {
            let who = state.who.clone();
            let placed = state
                .who
                .run(move || {
                    let root_writer = who.writes_as_root();
                    if root_writer {
                        let target = held
                            .target()
                            .ok_or_else(|| anyhow!("no folder is held to place the file in"))?;
                        root_may_place_in(&who, &dest_dir, target).map_err(|why| {
                            anyhow!(
                                "{} is not a folder this computer receives files into now \
                                 ({why}), so the file is not placed there",
                                dest_dir.display()
                            )
                        })?;
                    }
                    let (folders, name) = rel
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .and_then(folder_upload_parts)
                        .unwrap_or((Vec::new(), filename));
                    staging::place(held, &dest_dir, &folders, &name, root_writer)
                })
                .await?;
            if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
                g.remove(&state.id);
            }
            return Ok((placed, state.received));
        }
        // #3048 — staged by the daemon, placed for the person: copied across.
        if state.stager.is_the_daemon() && !state.who.is_the_daemon() {
            let placed = place_across(
                &state.stager,
                &state.who,
                partial_dir,
                dest_dir,
                filename,
                rel,
            )
            .await?;
            if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
                g.remove(&state.id);
            }
            return Ok((placed, state.received));
        }
        let actual_final_path = state
            .who
            .run(move || {
                let actual_final_path = match rel.as_deref().filter(|p| !p.is_empty()) {
                    Some(rel) => resolve_folder_upload_path(&dest_dir, rel)
                        .unwrap_or_else(|| unique_path(&dest_dir, &filename)),
                    None => unique_path(&dest_dir, &filename),
                };
                if let Some(parent) = actual_final_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating final parent {}", parent.display()))?;
                }

                let data_path = partial_dir.join("data");
                // rc.22 — the staging dir may now be on a different volume than
                // the final destination (PROGRAMDATA = C:, dest_path could be
                // D:\, a network mapped drive, or operator-redirected
                // Downloads). `rename` returns `ErrorKind::CrossesDevices` in
                // that case (Windows `ERROR_NOT_SAME_DEVICE` / Linux `EXDEV`);
                // fall back to copy + remove. Cross-volume costs an extra pass
                // through the file (35 MB ≈ 200 ms on NVMe, single-digit
                // seconds on HDD or 1 GbE SMB) which is acceptable for a
                // one-shot at end-time — the per-chunk write loop is what we
                // want to keep on PROGRAMDATA.
                match std::fs::rename(&data_path, &actual_final_path) {
                    Ok(()) => {}
                    Err(e) if is_cross_volume_error(&e) => {
                        tracing::info!(
                            from = %data_path.display(),
                            to = %actual_final_path.display(),
                            "files: cross-volume staging → final; copy+remove fallback"
                        );
                        std::fs::copy(&data_path, &actual_final_path).with_context(|| {
                            format!(
                                "cross-volume copy {} -> {}",
                                data_path.display(),
                                actual_final_path.display()
                            )
                        })?;
                        std::fs::remove_file(&data_path).ok();
                    }
                    Err(e) => {
                        return Err(anyhow::Error::from(e).context(format!(
                            "renaming {} -> {}",
                            data_path.display(),
                            actual_final_path.display()
                        )));
                    }
                }

                // Cleanup: remove the per-id staging dir, then the parent
                // `.roomler-partial/` dir if it's now empty so the staging
                // convention stays invisible (best-effort).
                let _ = std::fs::remove_dir_all(&partial_dir);
                if let Some(parent) = partial_dir.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
                Ok(actual_final_path)
            })
            .await?;
        if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
            g.remove(&state.id);
        }

        Ok((actual_final_path, state.received))
    }
}

/// #3048 — the end of a transfer the daemon staged (`stager`, in its own
/// folder, which the person may not write) and the person receives (`who`).
/// No rename crosses that boundary: a rename would carry the staging file's
/// descriptor into the person's folder. Instead:
///
/// 1. the staging `data` is opened as the daemon;
/// 2. as the person: the final path is resolved (`unique_path`, the M4 fix),
///    its folder made, a NEW file created there (`create_new`: never one
///    already there; it is born theirs, with their folder's permissions) and
///    the data copied in through the daemon's handle;
/// 3. the staging goes, as the daemon.
///
/// A copy that fails removes its half-written file and keeps the staging, so a
/// resume can finish it.
async fn place_across(
    stager: &FsAs,
    who: &FsAs,
    partial_dir: PathBuf,
    dest_dir: PathBuf,
    filename: String,
    rel: Option<String>,
) -> Result<PathBuf> {
    let data_path = partial_dir.join("data");
    let source = stager
        .run(move || {
            std::fs::File::open(&data_path)
                .with_context(|| format!("opening {}", data_path.display()))
        })
        .await?;
    let placed = who
        .run(move || {
            let final_path = match rel.as_deref().filter(|p| !p.is_empty()) {
                Some(rel) => resolve_folder_upload_path(&dest_dir, rel)
                    .unwrap_or_else(|| unique_path(&dest_dir, &filename)),
                None => unique_path(&dest_dir, &filename),
            };
            if let Some(parent) = final_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating final parent {}", parent.display()))?;
            }
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&final_path)
                .with_context(|| format!("creating {}", final_path.display()))?;
            if let Err(e) = copy_into(source, &mut out) {
                drop(out);
                let _ = std::fs::remove_file(&final_path);
                return Err(anyhow::Error::from(e)
                    .context(format!("copying into {}", final_path.display())));
            }
            Ok(final_path)
        })
        .await?;
    // Best-effort, as the rename path's cleanup.
    let _ = stager
        .run(move || {
            let _ = std::fs::remove_dir_all(&partial_dir);
            if let Some(parent) = partial_dir.parent() {
                let _ = std::fs::remove_dir(parent);
            }
            Ok(())
        })
        .await;
    Ok(placed)
}

/// Copy `source` into `out` through a 1 MiB buffer, then flush it to the
/// disk. Plain reads and writes through handles already open: neither end is
/// looked up again by path.
fn copy_into(mut source: std::fs::File, out: &mut std::fs::File) -> std::io::Result<()> {
    use std::io::{Read as _, Write as _};
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    out.sync_all()
}

impl FilesHandler {
    /// Drop any in-flight incoming transfer (DC closed mid-upload).
    /// The staging dir is left on disk so a future `files:resume`
    /// can pick up where it left off. The 24h orphan sweep at the
    /// next agent startup cleans abandoned partials. Browser-driven
    /// `files:cancel` (rc.19 P2) deletes the staging dir immediately.
    pub async fn abort(&self) {
        let mut guard = self.incoming.lock().await;
        *guard = None;
    }

    /// The session is over: drop an upload's state (its file closes; the
    /// partial stays for a resume, as [`Self::abort`]) and stop a download
    /// at its pump's next check. A download over a link slower than disk
    /// waits on the channel's send buffer, and after a network drop that
    /// buffer never drains: only its cancel flag (or the channel's state)
    /// ends the pump, which otherwise ran on holding the file, the channel
    /// and the transfer guard the updater defers for (found in review).
    pub async fn end_session(&self) {
        // First, so a begin that takes its lock after this refuses, and one
        // that took it before is caught below.
        self.ended.store(true, Ordering::Release);
        self.abort().await;
        if let Some(state) = self.outgoing.lock().await.as_ref() {
            state.cancel.cancel();
        }
    }

    fn refuse_if_ended(&self) -> Result<()> {
        if self.ended.load(Ordering::Acquire) {
            return Err(anyhow!("the session is over"));
        }
        Ok(())
    }

    /// rc.19: cancel an in-flight incoming upload. Removes the per-id
    /// staging dir + registry entry. Called from the
    /// `FilesIncoming::Cancel` arm in peer.rs (P2 wiring) when the
    /// browser's reconnect budget is exhausted. Returns true when an
    /// upload matched the id (caller logs this for audit clarity), and an
    /// error the viewer is sent for an id that is not one plain name (#3688).
    pub async fn cancel_incoming(&self, id: &str) -> Result<bool> {
        // #3688 — one plain name, checked before anything is cleared or
        // removed.
        transfer_id(id)?;
        // First clear in-flight state if it matches.
        let mut cleared_state = false;
        let in_flight = {
            let mut guard = self.incoming.lock().await;
            if guard.as_ref().is_some_and(|state| state.id == id) {
                cleared_state = true;
                guard.take()
            } else {
                None
            }
        };
        // #3688 — on Linux the in-flight upload's staging goes through the
        // handles it holds, as whoever staged it.
        #[cfg(target_os = "linux")]
        if let Some(mut state) = in_flight
            && let Some(held) = state.held.take()
        {
            drop(state.file);
            let _ = state
                .stager
                .run(move || {
                    staging::discard(held);
                    Ok(())
                })
                .await;
            if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
                g.remove(id);
            }
            return Ok(true);
        }
        #[cfg(not(target_os = "linux"))]
        drop(in_flight);
        // Remove the staging dir if known via the registry. Even if
        // in-flight state didn't match, a partial from a previous DC
        // might still be on disk.
        let registered = {
            let g = match PARTIAL_REGISTRY.lock() {
                Ok(g) => g,
                Err(_) => return Ok(cleared_state),
            };
            // meta_path is <dest_dir>/.roomler-partial/<id>/meta.json;
            // parent is the per-id staging dir.
            g.get(id).and_then(|e| {
                e.meta_path
                    .parent()
                    .map(|p| (p.to_path_buf(), e.who.clone()))
            })
        };
        if let Some((dir, who)) = registered {
            // #3688 — on Linux walked again with rule 3's checks, and
            // removed through handles only when it is its stager's own;
            // anything else stays for the sweep.
            #[cfg(target_os = "linux")]
            if !self.stages_in_a_root() {
                let (notes, id_s, w) = (dir.join("meta.json"), id.to_string(), who.clone());
                let _ = who
                    .run(move || {
                        match staging::reopen(&notes, &id_s, private_staging_root(&w).as_deref()) {
                            Ok((held, _)) => staging::discard(held),
                            Err(e) => tracing::warn!(
                                id = %id_s,
                                error = %format!("{e:#}"),
                                "files: a cancelled partial that is not its stager's own is left"
                            ),
                        }
                        Ok(())
                    })
                    .await;
                if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
                    g.remove(id);
                }
                return Ok(true);
            }
            // FR-97 P3d — removed as whoever staged it.
            let _ = who
                .run(move || {
                    let _ = std::fs::remove_dir_all(&dir);
                    if let Some(parent) = dir.parent() {
                        let _ = std::fs::remove_dir(parent);
                    }
                    Ok(())
                })
                .await;
            if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
                g.remove(id);
            }
            Ok(true)
        } else {
            Ok(cleared_state)
        }
    }

    /// rc.19: resume a previously-started upload after a DC drop.
    /// Looks up the staging dir for `id`, truncates the partial
    /// `data` file to the largest 256 KiB-aligned offset ≤ both the
    /// browser's requested offset and the file's on-disk size, opens
    /// it for append at that offset, and reinstalls the
    /// `IncomingTransfer` state on THIS DC's `incoming` Mutex.
    /// Returns the `accepted_offset` the browser should pump from.
    ///
    /// Errors when:
    /// - the registry has no entry for this id AND no canonical
    ///   `Downloads/.roomler-partial/<id>/meta.json` exists on disk;
    /// - `meta.json` is corrupt;
    /// - the `incoming` Mutex on this DC already has another upload
    ///   in-flight (browser must not interleave begins/resumes).
    ///
    /// Truncating to a 256 KiB boundary matches the `files:progress`
    /// cadence (the browser's `bytesAcked` source) so a same-id
    /// resume after a transient blip almost always returns the
    /// browser's requested offset verbatim. The mask only kicks in
    /// when the agent's on-disk size is smaller than what the
    /// browser thinks it acked (B3 fix in the rc.19 plan).
    pub async fn resume_incoming(&self, id: &str, requested_offset: u64) -> Result<u64> {
        self.resume_incoming_as(FsAs::for_write().await, id, requested_offset)
            .await
    }

    /// [`Self::resume_incoming`] with who a transfer starting now would be
    /// written as: the seam the tests use.
    pub(crate) async fn resume_incoming_as(
        &self,
        now: FsAs,
        id: &str,
        requested_offset: u64,
    ) -> Result<u64> {
        // #3688 — one plain name, checked before any partial is looked for.
        transfer_id(id)?;
        // Locate meta_path: registry first, then on-demand probe at
        // the canonical Downloads-rooted location for cross-restart
        // discovery survivors. Poison-tolerant — the HashMap has no
        // invariants a panicking test/peer could break.
        let entry = {
            let g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            g.get(id).cloned()
        };
        let (meta_path, stager) = match entry {
            Some(e) => (e.meta_path, e.who),
            None => {
                // On-demand fallback (sweep may have raced or the
                // partial lives under the staging root but registry
                // was cleared). rc.22 — the staging root depends on
                // the strategy: PROGRAMDATA on Windows (always-on by
                // default), or `<Downloads>/.roomler-partial/` for the
                // legacy escape-hatch path / non-Windows. FR-97 P3d —
                // beside the target, probed as whoever a drop would be
                // written as now; #3048 — in the daemon's own folder, as
                // the daemon, whose folder it is.
                let stager = self.stager_for(&now);
                let canonical = if self.stages_in_a_root() {
                    self.partial_dir(std::path::Path::new(""), id)?
                        .join("meta.json")
                } else {
                    // R4 — on Linux, first in the stager's own private folder,
                    // where an upload into a folder that keeps no modes stages.
                    #[cfg(target_os = "linux")]
                    let private = {
                        let (w, id_s) = (stager.clone(), id.to_string());
                        stager
                            .run(move || {
                                Ok(private_staging_root(&w)
                                    .map(|root| root.join(&id_s).join("meta.json"))
                                    .filter(|notes| std::fs::symlink_metadata(notes).is_ok()))
                            })
                            .await?
                    };
                    #[cfg(not(target_os = "linux"))]
                    let private: Option<PathBuf> = None;
                    match private {
                        Some(notes) => notes,
                        None => {
                            // #3688 — on Linux the lookup creates nothing: the
                            // drop folder as it would resolve now, unmade.
                            #[cfg(target_os = "linux")]
                            let downloads = {
                                let w = now.clone();
                                now.run(move || drop_folder_without_creating(&w))
                                    .await
                                    .context("resolving Downloads for on-demand resume probe")?
                            };
                            #[cfg(not(target_os = "linux"))]
                            let downloads = download_dir_for(&now)
                                .await
                                .context("resolving Downloads for on-demand resume probe")?;
                            self.partial_dir(&downloads, id)?.join("meta.json")
                        }
                    }
                };
                let probe = canonical.clone();
                if !stager
                    .run(move || Ok(std::fs::metadata(&probe).is_ok()))
                    .await?
                {
                    return Err(anyhow!("no partial state for id {id}"));
                }
                (canonical, stager)
            }
        };
        // #3048 — who places the file: the person a drop would be written as
        // now, when the daemon staged it in its own folder; whoever staged it,
        // when it sits beside the target (FR-97 P3d).
        let who = if self.stages_in_a_root() && stager.is_the_daemon() {
            now
        } else {
            stager.clone()
        };
        if let Some(why) = who.refusal() {
            return Err(anyhow!(why));
        }
        // #3688 — on Linux, walked again with rule 3's checks and held.
        #[cfg(target_os = "linux")]
        if !self.stages_in_a_root() {
            return self
                .resume_held(id, requested_offset, meta_path, stager, who)
                .await;
        }

        // FR-97 P3d — read, truncate and reopen as whoever staged the
        // partial: in the person's own folder, `meta.json` is theirs to
        // edit, and the end-time rename goes where it says.
        let placer = who.person_sid().map(str::to_string);
        let (meta, partial_dir, accepted, file) = stager
            .run(move || {
                let meta_bytes = std::fs::read(&meta_path)
                    .with_context(|| format!("reading {}", meta_path.display()))?;
                let meta: PartialMeta = serde_json::from_slice(&meta_bytes)
                    .with_context(|| format!("parsing {}", meta_path.display()))?;
                // #3048 — placed only for the person the transfer began for,
                // checked before anything is touched. A partial from a daemon
                // that recorded no one goes to whoever is signed in now.
                if let Some(began_for) = meta.person_sid.as_deref()
                    && !placer
                        .as_deref()
                        .is_some_and(|sid| sid.eq_ignore_ascii_case(began_for))
                {
                    return Err(anyhow!(NOT_THE_PERSON));
                }
                let partial_dir = meta_path
                    .parent()
                    .ok_or_else(|| anyhow!("meta_path has no parent"))?
                    .to_path_buf();
                let data_path = partial_dir.join("data");

                // disk_size — the source of truth for how much survived. May
                // be smaller than the browser's requested offset (kill
                // happened between sync_data calls).
                let disk_size = std::fs::metadata(&data_path)
                    .with_context(|| format!("stat {}", data_path.display()))?
                    .len();

                // B3 truncation: align to 256 KiB so resumes match the
                // existing files:progress cadence the browser uses for
                // bytesAcked.
                const ALIGN_MASK: u64 = !(256 * 1024 - 1);
                let accepted = requested_offset.min(disk_size) & ALIGN_MASK;

                // Truncate the file in place to accepted, then open for
                // append.
                {
                    let truncate_file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&data_path)
                        .with_context(|| format!("opening {} for truncate", data_path.display()))?;
                    truncate_file.set_len(accepted).with_context(|| {
                        format!("truncating {} to {accepted}", data_path.display())
                    })?;
                }
                let file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&data_path)
                    .with_context(|| format!("re-opening {} for append", data_path.display()))?;
                Ok((meta, partial_dir, accepted, file))
            })
            .await?;
        let file = File::from_std(file);

        // Reinstall state on THIS DC. Refuse if the slot is already
        // populated — browser is misbehaving if it sends resume
        // while another transfer is in-flight on the same DC.
        let mut guard = self.incoming.lock().await;
        if guard.is_some() {
            return Err(anyhow!(
                "files:resume {id} but another transfer is in-flight on this DC"
            ));
        }
        *guard = Some(IncomingTransfer {
            id: id.to_string(),
            expected: meta.expected_size,
            received: accepted,
            file,
            last_progress: accepted,
            partial_dir,
            dest_dir: meta.dest_dir.clone(),
            filename: meta.filename.clone(),
            last_synced: accepted,
            rel_path: meta.rel_path.clone(),
            who,
            stager,
            held: HeldStaging::default(),
            _active_guard: ActiveTransferGuard::new(),
        });
        tracing::info!(
            id,
            requested_offset,
            accepted_offset = accepted,
            "files: resume accepted"
        );
        Ok(accepted)
    }

    /// #3688 — [`Self::resume_incoming_as`] on Linux, as the stager: the
    /// partial walked again from its notes' path and held (`staging::reopen`),
    /// trusted only when its folder, notes and `data` are the stager's own
    /// and nobody else may write them, each opened without following a link
    /// (rule 3). A root writer's partial must also sit beside the folder its
    /// notes record, under a bare name, and that folder must be one the rule
    /// gives it now (rule 4): its file is placed there, never where the notes
    /// alone say. The person's, or a per-user daemon's, is placed where its
    /// notes say, as them (FR-97 P3d). Anything else is "no partial state",
    /// and the viewer starts that upload over.
    #[cfg(target_os = "linux")]
    async fn resume_held(
        &self,
        id: &str,
        requested_offset: u64,
        meta_path: PathBuf,
        stager: FsAs,
        who: FsAs,
    ) -> Result<u64> {
        let placer = who.person_sid().map(str::to_string);
        let (id_s, w, st, partial_dir) = (
            id.to_string(),
            who.clone(),
            stager.clone(),
            meta_path.parent().map(std::path::Path::to_path_buf),
        );
        let (meta, held, accepted, file) = stager
            .run(move || {
                let refuse = |why: String| {
                    tracing::warn!(
                        id = %id_s,
                        %why,
                        "files: a resume found no partial it may trust — refused"
                    );
                    anyhow!("no partial state for id {id_s}")
                };
                let private_root = private_staging_root(&st);
                let (mut held, beside) =
                    staging::reopen(&meta_path, &id_s, private_root.as_deref())
                        .map_err(|e| refuse(format!("{e:#}")))?;
                let notes = staging::read_notes(&held).map_err(|e| refuse(format!("{e:#}")))?;
                let meta: PartialMeta = serde_json::from_slice(&notes)
                    .with_context(|| format!("parsing {}", meta_path.display()))?;
                // #3048 — placed only for the person the transfer began for.
                if let Some(began_for) = meta.person_sid.as_deref()
                    && !placer
                        .as_deref()
                        .is_some_and(|sid| sid.eq_ignore_ascii_case(began_for))
                {
                    return Err(anyhow!(NOT_THE_PERSON));
                }
                match (w.writes_as_root(), beside) {
                    (true, Some(base)) => {
                        if meta.dest_dir != base {
                            return Err(refuse(format!(
                                "its notes record {}, but it sits beside {}",
                                meta.dest_dir.display(),
                                base.display()
                            )));
                        }
                        if sanitize_filename(&meta.filename) != meta.filename {
                            return Err(refuse(
                                "its notes record a name that is not a bare one".into(),
                            ));
                        }
                        let target = held
                            .target()
                            .ok_or_else(|| refuse("no folder is held beside it".into()))?;
                        root_may_place_in(&w, &base, target).map_err(refuse)?;
                    }
                    // R4 — staged in root's private folder, for a folder
                    // that keeps no modes: that folder must be one the rule
                    // gives now, under a bare name, and one no other
                    // account may write.
                    (true, None) => {
                        if sanitize_filename(&meta.filename) != meta.filename {
                            return Err(refuse(
                                "its notes record a name that is not a bare one".into(),
                            ));
                        }
                        let folder = staging::open_folder(&meta.dest_dir).map_err(|e| {
                            refuse(format!("opening {}: {e}", meta.dest_dir.display()))
                        })?;
                        root_may_place_in(&w, &meta.dest_dir, &folder).map_err(refuse)?;
                        staging::may_receive(&folder, own_euid(), true)
                            .map_err(|why| refuse(format!("{}: {why}", meta.dest_dir.display())))?;
                        held.place_in(folder);
                    }
                    (false, _) => {
                        let folder = staging::open_folder(&meta.dest_dir)
                            .with_context(|| format!("opening {}", meta.dest_dir.display()))?;
                        held.place_in(folder);
                    }
                }
                let (data, disk_size) =
                    staging::reopen_data(&held).map_err(|e| refuse(format!("{e:#}")))?;
                // B3 truncation, on the handle just checked: aligned to the
                // 256 KiB `files:progress` cadence.
                const ALIGN_MASK: u64 = !(256 * 1024 - 1);
                let accepted = requested_offset.min(disk_size) & ALIGN_MASK;
                data.set_len(accepted)
                    .with_context(|| format!("truncating data to {accepted}"))?;
                Ok((meta, held, accepted, data))
            })
            .await?;
        let file = File::from_std(file);
        let mut guard = self.incoming.lock().await;
        if guard.is_some() {
            return Err(anyhow!(
                "files:resume {id} but another transfer is in-flight on this DC"
            ));
        }
        *guard = Some(IncomingTransfer {
            id: id.to_string(),
            expected: meta.expected_size,
            received: accepted,
            file,
            last_progress: accepted,
            partial_dir: partial_dir.unwrap_or_default(),
            dest_dir: meta.dest_dir.clone(),
            filename: meta.filename.clone(),
            last_synced: accepted,
            rel_path: meta.rel_path.clone(),
            who,
            stager,
            held: Some(held),
            _active_guard: ActiveTransferGuard::new(),
        });
        tracing::info!(
            id,
            requested_offset,
            accepted_offset = accepted,
            "files: resume accepted"
        );
        Ok(accepted)
    }

    /// The id of the currently-active incoming transfer, if any.
    /// Used by the peer-layer error path: when `chunk()` fails we
    /// need to send a `files:error` with the matching id so the
    /// browser's per-upload promise listener fires its reject.
    /// Without this the browser silently discards the error (its
    /// listener filters by id) and the upload spinner spins forever.
    pub async fn current_id(&self) -> Option<String> {
        let guard = self.incoming.lock().await;
        guard.as_ref().map(|s| s.id.clone())
    }

    /// Begin a new outgoing transfer (host → browser download).
    /// Validates the path, opens the file for reading, and stashes
    /// state. Returns metadata for the `files:offer` reply.
    ///
    /// The cancellation flag is returned so the pump can check it
    /// between chunks; calling [`Self::cancel_outgoing`] flips the
    /// flag and the next loop iteration will exit.
    pub async fn begin_outgoing(&self, id: String, path: &str) -> Result<OutgoingOffer> {
        self.begin_outgoing_as(FsAs::for_read().await, id, path)
            .await
    }

    /// [`Self::begin_outgoing`], read as `who` (#3669): the file is opened in
    /// one step as them, before the offer, and the pump reads that handle.
    pub(crate) async fn begin_outgoing_as(
        &self,
        who: FsAs,
        id: String,
        path: &str,
    ) -> Result<OutgoingOffer> {
        who.refuse_read(NOBODY_SIGNED_IN_TO_SEND)?;
        let (asked, checks) = (path.to_string(), who.checks_handles());
        let opened = who.run(move || open_to_send(&asked, checks)).await;
        let (resolved, size, file) = who.explain_refusal(path, opened)?;

        let name = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "download.bin".to_string());
        let mime = guess_mime(&name);
        let cancel = tokio_util::sync::CancellationToken::new();

        let mut guard = self.outgoing.lock().await;
        // Under the lock: `end_session` flags first, then takes it.
        self.refuse_if_ended()?;
        if guard.is_some() {
            return Err(anyhow!(
                "another outgoing transfer is already active; cancel it first"
            ));
        }
        *guard = Some(OutgoingTransfer {
            id: id.clone(),
            path: resolved.clone(),
            size,
            file: Some(file),
            cancel: cancel.clone(),
            _active_guard: ActiveTransferGuard::new(),
        });

        Ok(OutgoingOffer {
            id,
            path: resolved,
            name,
            size: Some(size),
            mime,
            cancel,
            who,
        })
    }

    /// The outgoing transfer's file, opened when the transfer began (as its
    /// reader, #3669), handed to the pump once. Caller pumps chunks via the
    /// returned handle.
    pub async fn open_outgoing(&self, id: &str) -> Result<File> {
        let mut guard = self.outgoing.lock().await;
        let Some(state) = guard.as_mut() else {
            return Err(anyhow!("no active outgoing transfer"));
        };
        if state.id != id {
            return Err(anyhow!(
                "outgoing id mismatch: requested {id}, active {}",
                state.id
            ));
        }
        let file = state
            .file
            .take()
            .ok_or_else(|| anyhow!("{} is already being sent", state.path.display()))?;
        Ok(File::from_std(file))
    }

    /// Stop the active outgoing transfer if its id matches: its pump
    /// ends wherever it is waiting (#1730). Returns true if a matching
    /// transfer was found.
    pub async fn cancel_outgoing(&self, id: &str) -> bool {
        let guard = self.outgoing.lock().await;
        let Some(state) = guard.as_ref() else {
            return false;
        };
        if state.id != id {
            return false;
        }
        state.cancel.cancel();
        true
    }

    /// Clear the active outgoing transfer state. Called by the pump
    /// task when the transfer terminates (success, cancel, or error).
    pub async fn finish_outgoing(&self, id: &str) {
        let mut guard = self.outgoing.lock().await;
        if let Some(state) = guard.as_ref()
            && state.id == id
        {
            *guard = None;
        }
    }
}

/// Metadata returned from [`FilesHandler::begin_outgoing`]. The peer
/// layer uses these to format `files:offer` and to drive the pump
/// (its stop token, [`OutgoingTransfer::cancel`]).
pub struct OutgoingOffer {
    pub id: String,
    pub path: PathBuf,
    pub name: String,
    pub size: Option<u64>,
    pub mime: Option<&'static str>,
    pub cancel: tokio_util::sync::CancellationToken,
    /// #3669 — who the transfer reads as, fixed when it began: a folder's
    /// walk opens every entry as them.
    pub(crate) who: FsAs,
}

/// #3669 — the one step a download takes on the host before its offer, run
/// as the reader: the path resolved and held to the denylists, the agent's
/// own secrets refused, the file opened, and the handle stated and capped.
/// Windows checks access when a handle is opened, so the pump that reads
/// through this one needs no identity. #3688 — so does Linux, for the files
/// it is left to read: with `checks` (the person at seat0), the handle is
/// held to the file asked for before it leaves the step
/// ([`held_to_the_person`]).
fn open_to_send(asked: &str, checks: bool) -> Result<(PathBuf, u64, std::fs::File)> {
    let resolved = validate_outgoing_path(asked).context("validating outgoing path")?;
    if is_agent_secret(&resolved) {
        return Err(anyhow!(
            "{} holds the agent's own credentials — file transfer never sends it",
            resolved.display()
        ));
    }
    // The path is stated first only to keep what is not a file from being
    // opened at all: a folder's open fails on Windows with the code a refused
    // open has, and a FIFO's open on Unix waits for a writer.
    let not_a_file = || anyhow!("not a regular file: {}", resolved.display());
    let meta =
        std::fs::metadata(&resolved).with_context(|| format!("stat {}", resolved.display()))?;
    if !meta.is_file() {
        return Err(not_a_file());
    }
    let file = std::fs::File::open(&resolved)
        .with_context(|| format!("opening {}", resolved.display()))?;
    #[cfg(target_os = "linux")]
    if checks {
        held_to_the_person(&file, &resolved)?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = checks;
    // The checks that count are on the handle that is sent, not the path:
    // the path may have been swapped between that stat and the open.
    let meta = file
        .metadata()
        .with_context(|| format!("stat {}", resolved.display()))?;
    if !meta.is_file() {
        return Err(not_a_file());
    }
    let size = meta.len();
    if size > MAX_TRANSFER_BYTES {
        return Err(anyhow!(
            "file size {size} exceeds the {} B cap",
            MAX_TRANSFER_BYTES
        ));
    }
    Ok((resolved, size, file))
}

// ---------------------------------------------------------------------------
// Outgoing path validation

/// Per-OS denylist for paths the agent will refuse to download. These
/// are NOT a sandbox — the operator already has full remote-control
/// rights — but they stop a malicious browser from path-encoding its
/// way into registry hives or kernel-namespace prefixes that bypass
/// the normal Win32 ACL surface.
fn validate_outgoing_path(input: &str) -> Result<PathBuf> {
    if input.is_empty() {
        return Err(anyhow!("path is empty"));
    }
    if input.len() > 4096 {
        return Err(anyhow!("path exceeds 4096 bytes"));
    }

    // Platform-aware denylist.
    let lower = input.to_ascii_lowercase();
    if lower.contains("\\\\?\\globalroot") || lower.contains("//?/globalroot") {
        return Err(anyhow!("path uses kernel-namespace prefix (denied)"));
    }
    // Registry-hive container under Windows. Reading SAM / SECURITY
    // directly is meaningless without the registry API anyway, but
    // also explicitly closing the door helps the audit trail.
    if lower.contains("\\windows\\system32\\config\\")
        || lower.contains("/windows/system32/config/")
    {
        return Err(anyhow!("path is under registry-hive container (denied)"));
    }

    let path = PathBuf::from(input);
    // Canonicalise to dereference symlinks + resolve `..` segments.
    // If canonicalize fails (path doesn't exist, permission denied),
    // surface a friendly error.
    let canonical = std::fs::canonicalize(&path)
        .with_context(|| format!("canonicalising {}", path.display()))?;

    // Re-check the denylist on the canonical form so a symlink trick
    // can't bypass us.
    let canon_lower = canonical.to_string_lossy().to_ascii_lowercase();
    if canon_lower.contains("\\\\?\\globalroot") || canon_lower.contains("//?/globalroot") {
        return Err(anyhow!(
            "canonical path uses kernel-namespace prefix (denied)"
        ));
    }
    if canon_lower.contains("\\windows\\system32\\config\\")
        || canon_lower.contains("/windows/system32/config/")
    {
        return Err(anyhow!(
            "canonical path is under registry-hive container (denied)"
        ));
    }

    Ok(canonical)
}

/// Resolve a path-targeted upload's destination directory. Reuses
/// the same denylist as [`validate_outgoing_path`] (kernel-namespace
/// prefixes, registry-hive container) but additionally requires the
/// path to exist AND be a directory (uploads land at `<dir>/<name>`,
/// so the dir must be writeable on the host).
///
/// Used by [`FilesHandler::begin`] when the browser sends a
/// `dest_path` field on `files:begin`. On error, the caller falls
/// back to the default Downloads/ target with a warning log so the
/// upload doesn't silently drop the operator's data.
#[cfg(test)]
async fn resolve_dest_path(input: &str) -> Result<PathBuf> {
    resolve_dest_path_for(input, &writer_context(), &files_dir::Rules::from_env()).await
}

/// FR-97 P3d — [`resolve_dest_path`] for `who`, AS `who`: the path is
/// canonicalised (links followed) with their rights, and for the person at
/// seat0 the placement rule's active home is theirs.
///
/// #3688 — with nobody at seat0 on a Linux system install, FR-84 D4's rule
/// for a privileged writer with no one signed in holds: nowhere a controller
/// chose. The folder is refused before it is touched, so the upload lands in
/// the default folder ([`FilesHandler::begin`]'s fallback).
async fn resolve_dest_path_as(who: &FsAs, input: &str) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if matches!(who, FsAs::DaemonAlone) {
        return Err(anyhow!(
            "nobody is signed in at this computer's screen, so a received file goes to the \
             default folder, never to one a controller chose"
        ));
    }
    let (input, writer) = (input.to_string(), who.writer());
    who.run(move || resolve_dest_path_here(&input, &writer, &files_dir::Rules::from_env()))
        .await
}

/// [`resolve_dest_path`] for an explicit writer — the seam the tests use,
/// since a test process is never SYSTEM.
#[cfg(test)]
async fn resolve_dest_path_for(
    input: &str,
    writer: &files_dir::Writer,
    rules: &files_dir::Rules,
) -> Result<PathBuf> {
    let (input, writer, rules) = (input.to_string(), writer.clone(), rules.clone());
    FsAs::Daemon
        .run(move || resolve_dest_path_here(&input, &writer, &rules))
        .await
}

/// The body of the dest_path checks, on THIS (blocking) thread under the
/// identity `writer` describes.
fn resolve_dest_path_here(
    input: &str,
    writer: &files_dir::Writer,
    rules: &files_dir::Rules,
) -> Result<PathBuf> {
    let canonical = validate_outgoing_path(input).context("validating dest_path")?;
    if let Some(run) = agent_state_run(&canonical) {
        return Err(anyhow!(
            "dest_path {} is inside the agent's own configuration and state (…{run}…) — \
             file transfer never writes there",
            canonical.display()
        ));
    }
    let meta =
        std::fs::metadata(&canonical).with_context(|| format!("stat {}", canonical.display()))?;
    if !meta.is_dir() {
        return Err(anyhow!(
            "dest_path is not a directory: {}",
            canonical.display()
        ));
    }
    check_dest_placement(&canonical, writer, rules)
        .map_err(|e| anyhow!("dest_path refused: {e}"))?;
    Ok(canonical)
}

/// FR-84 D4's placement rule, applied to a per-upload destination: a
/// PRIVILEGED writer (SYSTEM / root) writes only inside the active user's
/// profile — the same place the drop folder itself must be — and, with
/// nobody signed in, nowhere a controller chose. A controller with FILES
/// acts on the person's files; writing where only the machine may is what
/// `exec` and SSH are gated for. An unprivileged writer is bounded by the
/// OS's own rights on the write.
fn check_dest_placement(
    canonical: &std::path::Path,
    writer: &files_dir::Writer,
    rules: &files_dir::Rules,
) -> Result<(), String> {
    if !writer.privileged {
        return Ok(());
    }
    let plain = files_dir::strip_verbatim(canonical.to_path_buf());
    let placed = files_dir::resolve(&plain.to_string_lossy(), writer, rules)?;
    files_dir::check_placement_on_disk(&placed, writer, rules)
}

/// Component runs that name the agent's OWN configuration and state — its
/// config (enrollment token, SSH host key, WireGuard keys), logs, caches —
/// matched anywhere in a canonical path, case-folded where the file system
/// is. Every profile's copy counts, not only this process's: a SYSTEM or
/// root worker can reach every user's.
#[cfg(target_os = "windows")]
const AGENT_STATE_RUNS: &[&[&str]] = &[
    &["programdata", "roomler"],
    &["appdata", "roaming", "roomler"],
    &["appdata", "local", "roomler"],
];
#[cfg(target_os = "macos")]
const AGENT_STATE_RUNS: &[&[&str]] = &[
    &["etc", "roomler"],
    &["library", "application support", "live.roomler.roomler"],
    &["library", "caches", "live.roomler.roomler"],
    // RETIRED-NAME-ANCHOR(4): a pre-rename Mac keeps its state (its config included) in the old segment's folder.
    &[
        "library",
        "application support",
        "live.roomler.roomler-agent",
    ],
    &["var", "log", "roomler"],
];
#[cfg(all(unix, not(target_os = "macos")))]
const AGENT_STATE_RUNS: &[&[&str]] = &[
    &["etc", "roomler"],
    &[".config", "roomler"],
    &[".local", "share", "roomler"],
    &[".local", "state", "roomler"],
    // RETIRED-NAME-ANCHOR(2): a pre-rename host keeps its config and data in the old segment's trees.
    &[".config", "roomler-agent"],
    &[".local", "share", "roomler-agent"],
    &[".cache", "roomler"],
    &["var", "log", "roomler"],
];

/// File names inside the agent's state that carry secrets: the config and
/// every sibling the writer leaves (`.prev`, `.tmp.<pid>`, archived copies),
/// and the Wayland portal's restore tokens.
const AGENT_SECRET_PREFIXES: &[&str] = &["config.toml", "portal-restore-token"];

fn fold_case() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}

/// The run of [`AGENT_STATE_RUNS`] `path` passes through, if any.
fn agent_state_run(path: &std::path::Path) -> Option<String> {
    let comps: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => {
                let s = s.to_string_lossy();
                Some(if fold_case() {
                    s.to_lowercase()
                } else {
                    s.into_owned()
                })
            }
            _ => None,
        })
        .collect();
    AGENT_STATE_RUNS.iter().find_map(|run| {
        comps
            .windows(run.len())
            .any(|w| w.iter().zip(run.iter()).all(|(a, b)| a == b))
            .then(|| run.join("/"))
    })
}

/// A secret-bearing file of the agent's own: never leaves the host by file
/// transfer, whatever the daemon's privilege. This is a second line, not the
/// boundary. The rest of its state (logs, crash dumps, uploads) is not
/// refused here, so the drawer reaches it wherever the reader may. A Windows
/// background-service install reads as the person signed in (#3669), and its
/// machine-wide folder is SYSTEM's and the Administrators' alone, so the
/// person is refused there and an administrator fetches those files with
/// `roomler exec`.
fn is_agent_secret(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else {
        return false;
    };
    let name = if fold_case() {
        name.to_lowercase()
    } else {
        name.into_owned()
    };
    AGENT_SECRET_PREFIXES.iter().any(|p| name.starts_with(p)) && agent_state_run(path).is_some()
}

/// Best-effort MIME guess from a filename's extension. Used in the
/// `files:offer` so the browser's `showSaveFilePicker` shows the
/// right filter / so a Blob fallback can set the right type for a
/// later anchor download.
fn guess_mime(name: &str) -> Option<&'static str> {
    let dot = name.rfind('.')?;
    let ext = name[dot + 1..].to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "exe" | "msi" => "application/octet-stream",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Directory listing

/// 3 second cap on a single `read_dir` enumeration. A disconnected
/// network drive can hang a Win32 `FindFirstFile` for tens of
/// seconds; with this timeout the listing fails fast as a
/// `dir-error` rather than blocking the entire pump.
const DIR_LIST_TIMEOUT_SECS: u64 = 3;

/// Sentinel meaning "wherever THIS host stages in-flight uploads". The caller
/// sends the token; the agent resolves it.
///
/// FR-21 P4. The viewer used to hardcode
// RETIRED-NAME-ANCHOR-BEGIN
// appdirs::machine_global_dir() keeps resolving the pre-rename `roomler-agent`
// segment while that tree exists and the new one does not, so a host
// installed before the rename keeps finding its own staging dir and uploads.
// Every occurrence below is that fallback or a test pinning it — the retired
// name is the INPUT.
/// `C:\ProgramData\roomler\roomler-agent\staging`, but `machine_global_dir()`
/// resolves the `roomler` segment on a fresh install and only falls back to the
/// pre-rename `roomler-agent` segment when that tree already exists — so the
/// literal was correct on exactly one class of host and silently wrong on the
/// rest. Confirmed on a real SYSTEM-mode Windows host, where
/// `%PROGRAMDATA%\roomler\roomler-agent` does not exist at all.
///
/// A hardcoded copy of a path that a resolver made conditional is a defect
/// waiting for the resolver to take its other branch. The fix is not a better
/// literal — it is removing the caller's ability to hold one.
pub const STAGING_SENTINEL: &str = "<staging>";

/// List a directory. Empty / `~` enumerates the roots (logical drives on
/// Windows; a single `/` on Unix); [`STAGING_SENTINEL`] resolves to this
/// host's staging root; anything else is canonicalised and read, `/`
/// included. Returns at most 10000 entries — a directory with more is a
/// degenerate case (deeply nested `node_modules` on a dev box) that we'd
/// rather refuse than stream a 1 MiB JSON listing.
///
/// ⚠️ `/` is a DIRECTORY, not a roots request (#2870). Until this change the
/// roots arm also matched `/`, so on a Unix host the roots view listed one
/// entry, `/`, and opening it asked for `/` — which came back as the roots
/// view again. The viewer's `isRootsRequest` mirrors this arm: the two must
/// agree on which spellings mean "the roots", or the drawer labels a real
/// listing a roots view and drives into bare entry names.
pub async fn list_dir(path: &str) -> Result<DirListing> {
    list_dir_as(&FsAs::for_read().await, path).await
}

/// [`list_dir`], read as `who` (#3669). With nobody signed in it is refused
/// outright, the roots view included, so the drawer says so as it opens.
pub(crate) async fn list_dir_as(who: &FsAs, path: &str) -> Result<DirListing> {
    who.refuse_read(NOBODY_SIGNED_IN_TO_BROWSE)?;
    if path.is_empty() || path == "~" {
        return Ok(DirListing {
            path: roots_label(),
            parent: None,
            entries: enumerate_roots(),
        });
    }
    if path == STAGING_SENTINEL {
        return list_staging(who).await;
    }
    list_canonical(who, &PathBuf::from(path)).await
}

/// Canonicalise, enumerate and shape one directory, as `who`. Shared by the
/// plain path arm and the staging sentinel so the two can never drift in
/// timeout, entry cap or error wording.
async fn list_canonical(who: &FsAs, pb: &std::path::Path) -> Result<DirListing> {
    let listed = async {
        // #3669 — resolving and opening the folder is one step as `who`,
        // under the timeout that always bounded the open. Resolving moved
        // off the async worker with it, so a share that hangs cannot stall
        // one. The entries are read after, untimed as before, through the
        // handle that step opened.
        let (asked, checks) = (pb.to_path_buf(), who.checks_handles());
        let (canon, read) = tokio::time::timeout(
            std::time::Duration::from_secs(DIR_LIST_TIMEOUT_SECS),
            who.run(move || {
                let canon = std::fs::canonicalize(&asked)
                    .with_context(|| format!("canonicalising {}", asked.display()))?;
                let read = folder_listing(&canon, checks)?;
                Ok((canon, read))
            }),
        )
        .await
        .map_err(|_| anyhow!("listing timed out"))??;
        let entries = who.run(move || Ok(collect_dir_entries(read))).await?;
        Ok::<_, anyhow::Error>(DirListing {
            parent: canon.parent().map(|p| p.to_string_lossy().to_string()),
            path: canon.to_string_lossy().to_string(),
            entries,
        })
    }
    .await;
    who.explain_refusal(&pb.to_string_lossy(), listed)
}

/// Resolve [`STAGING_SENTINEL`] against THIS host's layout.
///
/// The staging dir is created per update and removed after, so it is absent
/// most of the time. Erroring then would make the quick-access button useless
/// except during the few seconds an update is in flight, so an absent staging
/// dir lists its parent — the machine-global root, which holds `crashes/`,
/// `service-logs/` and `uploads/` besides. That root is made by whichever of
/// those is written first, never by this resolver (a listing creates nothing),
/// so on a host where nothing has written yet — a fresh CI runner, a box whose
/// ProgramData was cleaned under a running daemon — it is absent too: the one
/// failure of the first full `--lib` run on a Windows runner (#2886),
/// `canonicalising C:\ProgramData\roomler\roomler: cannot find the path`. The
/// contract is "always lands somewhere real", so then the nearest ancestor
/// that exists is listed, `%PROGRAMDATA%` at the furthest. No wider than the
/// plain path arm, which lists any directory the caller names.
///
/// #3669 — the folder is found as the service, and listed as `who`: on a
/// machine-wide install the service's folder is SYSTEM's and the
/// Administrators', so a person without an elevated administrator's rights
/// is refused, and told so.
#[cfg(target_os = "windows")]
async fn list_staging(who: &FsAs) -> Result<DirListing> {
    let staging = staging_root_windows();
    let target = if staging.is_dir() {
        staging
    } else {
        let root = crate::appdirs::machine_global_dir();
        nearest_existing_dir(&root).unwrap_or(root)
    };
    list_canonical(who, &target).await
}

/// The deepest directory that exists on `path`'s way up: `path` itself when
/// it is one, else the nearest ancestor that is. `None` only when nothing on
/// the way up exists, which no path under a drive or `/` can manage. Compiled
/// for tests on every OS so its test runs where the Windows caller does not.
#[cfg(any(target_os = "windows", test))]
fn nearest_existing_dir(path: &std::path::Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.is_dir())
        .map(std::path::Path::to_path_buf)
}

/// Non-Windows hosts stage per-destination (`<dest>/.roomler-partial/<id>`),
/// so there is no single staging root to open. Say so rather than inventing a
/// path that would then be wrong in a new way.
#[cfg(not(target_os = "windows"))]
async fn list_staging(_who: &FsAs) -> Result<DirListing> {
    Err(anyhow!(
        "this host has no single staging root — non-Windows agents stage \
         alongside each destination"
    ))
}

/// The entries of a folder [`list_canonical`] opened: at most 10 000, the
/// listing ending at its first error. #3688 — a listing through a folder's
/// handle (`/proc/self/fd/N`, [`folder_listing`]) names its entries after that
/// handle, so read only `file_name()` and `metadata()` here, never `path()`.
fn collect_dir_entries(read: std::fs::ReadDir) -> Vec<DirEntryView> {
    let mut entries: Vec<DirEntryView> = Vec::new();
    for entry in read.take(10_000) {
        let Ok(entry) = entry else {
            break;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue; // skip non-UTF-8 names
        };
        let Ok(meta) = entry.metadata() else {
            continue; // permission denied / vanished — skip
        };
        let is_dir = meta.is_dir();
        let size = if is_dir { None } else { Some(meta.len()) };
        let mtime_unix = meta.modified().ok().and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });
        entries.push(DirEntryView {
            name,
            is_dir,
            size,
            mtime_unix,
        });
    }
    // Sort: dirs first then files; both alphabetical case-insensitive.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    entries
}

#[cfg(target_os = "windows")]
fn enumerate_roots() -> Vec<DirEntryView> {
    let mut out = Vec::new();
    // GetLogicalDrives returns a bitmask; bit i set = drive (i + 'A')
    // is present. SAFETY: no preconditions; thread-safe.
    let mask: u32 = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    if mask == 0 {
        return out;
    }
    for i in 0u32..26 {
        if mask & (1 << i) != 0 {
            let letter = (b'A' + i as u8) as char;
            out.push(DirEntryView {
                name: format!("{letter}:\\"),
                is_dir: true,
                size: None,
                mtime_unix: None,
            });
        }
    }
    out
}

#[cfg(not(target_os = "windows"))]
fn enumerate_roots() -> Vec<DirEntryView> {
    vec![DirEntryView {
        name: "/".to_string(),
        is_dir: true,
        size: None,
        mtime_unix: None,
    }]
}

#[cfg(target_os = "windows")]
fn roots_label() -> String {
    "Drives".to_string()
}

#[cfg(not(target_os = "windows"))]
fn roots_label() -> String {
    "/".to_string()
}

/// Result of [`list_dir`].
pub struct DirListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<DirEntryView>,
}

// ---------------------------------------------------------------------------
// Folder zip streaming (Phase 4 of file-DC v2)
//
// We expose async_zip 0.0.17 behind two helpers: `begin_outgoing_zip`
// validates the requested folder and stashes outgoing state (parallel
// to `begin_outgoing` for single-file downloads); `walk_and_zip` is
// the writer task that the peer layer calls inside a tokio::spawn,
// driven by a `tokio::io::duplex` pipe that gives async_zip natural
// backpressure when the DC reader can't keep up.

/// Maximum entries (files + dirs) we'll walk into a single zip
/// before bailing. A degenerate `node_modules` can have hundreds of
/// thousands of files; we'd rather refuse cleanly than stream a 50
/// MiB zip the operator didn't expect.
const ZIP_MAX_ENTRIES: u32 = 10_000;

/// Maximum entry-name length inside the zip (UTF-8 bytes). 4096 is
/// well above any realistic Windows MAX_PATH (260) or POSIX (4096)
/// path; rejection is a sanity check against pathological inputs.
const ZIP_MAX_ENTRY_PATH_LEN: usize = 4096;

impl FilesHandler {
    /// Begin a folder-download outgoing transfer. Validates the path
    /// (denylist + canonicalisation), confirms it's a directory, and
    /// stashes outgoing state. Returns an offer with `size = None`
    /// (folder zips have unknown size up front — it's a streaming
    /// stream).
    pub async fn begin_outgoing_zip(&self, id: String, path: &str) -> Result<OutgoingOffer> {
        self.begin_outgoing_zip_as(FsAs::for_read().await, id, path)
            .await
    }

    /// [`Self::begin_outgoing_zip`], read as `who` (#3669): the folder is
    /// resolved and opened for listing in one step as them, and the offer
    /// carries them to the walk.
    pub(crate) async fn begin_outgoing_zip_as(
        &self,
        who: FsAs,
        id: String,
        path: &str,
    ) -> Result<OutgoingOffer> {
        who.refuse_read(NOBODY_SIGNED_IN_TO_SEND)?;
        let (asked, checks) = (path.to_string(), who.checks_handles());
        let checked = who
            .run(move || {
                let resolved = validate_outgoing_path(&asked).context("validating folder path")?;
                let meta = std::fs::metadata(&resolved)
                    .with_context(|| format!("stat {}", resolved.display()))?;
                if !meta.is_dir() {
                    return Err(anyhow!("not a directory: {}", resolved.display()));
                }
                // A folder the reader may not list is refused here, saying
                // why, rather than sent as an empty zip that looks complete.
                folder_listing(&resolved, checks)?;
                Ok(resolved)
            })
            .await;
        let resolved = who.explain_refusal(path, checked)?;
        let folder_name = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .map(sanitize_filename)
            .unwrap_or_else(|| "folder".to_string());
        let zip_name = format!("{folder_name}.zip");
        let cancel = tokio_util::sync::CancellationToken::new();

        let mut guard = self.outgoing.lock().await;
        // Under the lock: `end_session` flags first, then takes it.
        self.refuse_if_ended()?;
        if guard.is_some() {
            return Err(anyhow!(
                "another outgoing transfer is already active; cancel it first"
            ));
        }
        *guard = Some(OutgoingTransfer {
            id: id.clone(),
            path: resolved.clone(),
            size: 0, // unknown for streaming zip
            file: None,
            cancel: cancel.clone(),
            _active_guard: ActiveTransferGuard::new(),
        });
        Ok(OutgoingOffer {
            id,
            path: resolved,
            name: zip_name,
            size: None,
            mime: Some("application/zip"),
            cancel,
            who,
        })
    }
}

/// Walk `root` recursively, writing every file as a Stored zip
/// entry (no compression). Cycle-safe via a canonical-path HashSet;
/// capped at [`ZIP_MAX_ENTRIES`] to avoid runaway zips.
///
/// `writer` should be the write end of a `tokio::io::duplex` pipe
/// whose read end is being pumped to the DC by the caller — that
/// duplex is what gives us backpressure (writes block when the
/// pipe is full, async_zip awaits, the whole producer chain
/// stalls until the DC reader drains).
///
/// Stored (no-compression) is the right default: most user content
/// is already compressed (jpeg/mp4/zip/exe), so deflate wastes CPU
/// for ~1% gain. Operators who specifically want a smaller zip
/// for log/source folders can use a normal compress-then-download
/// workflow — that's not the use case for the live remote-control
/// drawer.
///
/// #3669 — every step that touches the folder runs as `who`, the reader the
/// download began for: the root resolved, each folder listed, each file
/// opened. What they may not open is left out, as an unreadable entry always
/// was; a step whose identity fails (they signed out) ends the walk with an
/// error, so a zip is never cut short to look complete.
pub(crate) async fn walk_and_zip<W>(
    writer: W,
    root: &std::path::Path,
    who: &FsAs,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<u32>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    use async_zip::tokio::write::ZipFileWriter;
    use async_zip::{Compression, ZipEntryBuilder};
    use std::collections::HashSet;
    use tokio_util::compat::FuturesAsyncWriteCompatExt;

    who.refuse_read(NOBODY_SIGNED_IN_TO_SEND)?;
    let checks = who.checks_handles();
    let root_canon = {
        let root = root.to_path_buf();
        who.run(move || {
            std::fs::canonicalize(&root)
                .with_context(|| format!("canonicalising {}", root.display()))
        })
        .await?
    };

    let mut zip = ZipFileWriter::with_tokio(writer);
    let mut stack: Vec<PathBuf> = vec![root_canon.clone()];
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(root_canon.clone());
    let mut count: u32 = 0;

    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            return Err(anyhow!("cancelled by browser"));
        }
        // #3688 — a person who has left seat0 ends the walk.
        who.still_at_the_screen().await?;
        // The folder, listed in one step as `who`: no more entries than the
        // cap leaves room for, plus one to trip it.
        let room = ZIP_MAX_ENTRIES.saturating_sub(count).saturating_add(1);
        let listed = {
            let dir = dir.clone();
            who.run(move || Ok(zip_folder_entries(&dir, room, checks)))
                .await?
        };
        let entries = match listed {
            Ok(entries) => entries,
            Err(e) => {
                tracing::debug!(
                    dir = ?dir,
                    error = ?format!("{e:#}"),
                    "zip walk: skipping unreadable dir"
                );
                continue;
            }
        };
        for (path, kind) in entries {
            if cancel.is_cancelled() {
                return Err(anyhow!("cancelled by browser"));
            }
            count = count.saturating_add(1);
            if count > ZIP_MAX_ENTRIES {
                return Err(anyhow!(
                    "folder exceeds {ZIP_MAX_ENTRIES} entries — refusing to stream"
                ));
            }
            match kind {
                // Cycle protection: canonical path already visited? (Else —
                // already visited, or no canonical path — a silent skip: a
                // symlink loop or a hard-linked dir.)
                ZipEntry::Folder(canon) => {
                    if let Some(canon) = canon
                        && visited.insert(canon)
                    {
                        stack.push(path);
                    }
                    continue;
                }
                ZipEntry::Skip => continue,
                ZipEntry::File => {}
            }
            if is_agent_secret(&path) {
                // A folder download of e.g. the machine-global root carries
                // its logs and crash dumps — never the agent's credentials.
                tracing::debug!(file = ?path, "zip walk: skipping the agent's own secret");
                continue;
            }

            // Per-component-sanitised relative path inside the zip.
            // Forward slashes per zip spec.
            let rel = match path.strip_prefix(&root_canon) {
                Ok(r) => r,
                Err(_) => continue, // weirdness — skip
            };
            let rel_safe = rel
                .components()
                .map(|c| sanitize_filename(&c.as_os_str().to_string_lossy()))
                .collect::<Vec<_>>()
                .join("/");
            if rel_safe.is_empty() || rel_safe.len() > ZIP_MAX_ENTRY_PATH_LEN {
                continue;
            }

            // #3688 — a person who has left seat0 ends the walk before its next
            // file too. Each file opens in a step of its own as `who`, and
            // streams into a zip entry through that handle. One that will not
            // open is skipped, logged at debug; an identity that fails ends the walk.
            who.still_at_the_screen().await?;
            let opened = {
                let path = path.clone();
                who.run(move || Ok(open_zip_entry(&path, checks))).await?
            };
            let mut src = match opened {
                Ok(f) => tokio::fs::File::from_std(f),
                Err(e) => {
                    tracing::debug!(file = ?path, e = ?e.to_string(), "zip walk: skipping unreadable file");
                    continue;
                }
            };
            let builder = ZipEntryBuilder::new(rel_safe.into(), Compression::Stored);
            let entry_writer = match zip.write_entry_stream(builder).await {
                Ok(w) => w,
                Err(e) => {
                    return Err(anyhow!("zip write_entry_stream failed: {e}"));
                }
            };
            // async_zip 0.0.17 returns a `futures::AsyncWrite`-shaped
            // EntryStreamWriter; wrap with compat_write so we can use
            // `tokio::io::copy` against our tokio::fs::File source.
            let mut entry_tokio = entry_writer.compat_write();
            if let Err(e) = tokio::io::copy(&mut src, &mut entry_tokio).await {
                return Err(anyhow!("zip copy failed for {}: {}", path.display(), e));
            }
            // Recover the inner futures-AsyncWrite to call .close().
            let entry_writer = entry_tokio.into_inner();
            if let Err(e) = entry_writer.close().await {
                return Err(anyhow!("zip entry close failed: {e}"));
            }
        }
    }

    if let Err(e) = zip.close().await {
        return Err(anyhow!("zip finalise failed: {e}"));
    }
    Ok(count)
}

/// #3688 — a folder a read lists, opened as the step's reader. With `checks`
/// (the person at seat0) it is opened first and its handle held to what was
/// asked, as a file's is ([`folder_held_to_the_person`]), and it is listed
/// through that handle: `/proc/self/fd/N` names the open handle, not the
/// folder's path, so the kernel lists the very folder the handle holds, and a
/// component swapped for a link since the path resolved is never followed.
/// Without, the folder is read by its path, as it always was. An open that
/// fails says so as `reading <dir>`; a handle that is not the folder asked
/// says why, in the words the viewer shows.
fn folder_listing(dir: &std::path::Path, checks: bool) -> Result<std::fs::ReadDir> {
    #[cfg(all(test, target_os = "linux"))]
    stand_in::before_a_folder_opens(dir);
    #[cfg(target_os = "linux")]
    if checks {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let held = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(dir)
            .with_context(|| format!("reading {}", dir.display()))?;
        folder_held_to_the_person(&held, dir)?;
        return std::fs::read_dir(format!("/proc/self/fd/{}", held.as_raw_fd()))
            .with_context(|| format!("reading {}", dir.display()));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = checks;
    std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))
}

/// #3669 — one entry of a folder a zip walk lists, as its listing step saw it.
enum ZipEntry {
    /// A folder, with its canonical path for the cycle check when it has one.
    Folder(Option<PathBuf>),
    /// A regular file.
    File,
    /// Anything else, or an entry whose metadata would not read: skipped, and
    /// counted. `DirEntry::metadata` does not follow a link, so a link is
    /// one too.
    Skip,
}

/// #3669 — one folder of a zip walk, listed in one step as the reader: each
/// entry's path and kind, at most `room` of them, the listing ending at its
/// first error as it always did. `Err` when the reader may not list it, and
/// (#3688, `checks`) when its handle is not the folder asked or is on a
/// kernel file system ([`folder_listing`]).
fn zip_folder_entries(
    dir: &std::path::Path,
    room: u32,
    checks: bool,
) -> Result<Vec<(PathBuf, ZipEntry)>> {
    let mut out = Vec::new();
    for entry in folder_listing(dir, checks)?.take(room as usize) {
        let Ok(entry) = entry else {
            break;
        };
        // Named from the folder asked: a listing read through the folder's
        // handle names its entries after that handle.
        let path = dir.join(entry.file_name());
        let kind = match entry.metadata() {
            Ok(meta) if meta.is_dir() => ZipEntry::Folder(std::fs::canonicalize(&path).ok()),
            Ok(meta) if meta.is_file() => ZipEntry::File,
            _ => ZipEntry::Skip,
        };
        out.push((path, kind));
    }
    Ok(out)
}

/// #3688 — one file of a zip walk, opened in a step as the reader and, with
/// `checks` (the person at seat0), held to the entry its folder listed
/// ([`held_to_the_person`]): a link swapped in since the listing, which the
/// open follows, is left out as an unreadable file is.
fn open_zip_entry(path: &std::path::Path, checks: bool) -> std::io::Result<std::fs::File> {
    let file = std::fs::File::open(path)?;
    #[cfg(target_os = "linux")]
    if checks {
        held_to_the_person(&file, path)?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = checks;
    Ok(file)
}

/// #3688 — the kernel's own file systems, by `statfs` magic: a read as the
/// person at seat0 never opens a file on one, nor lists a folder of one.
/// Some of their files answer by who reads at `read()`, which the pump does
/// as the daemon, and some by the thread's euid, which stays root; neither
/// is the person's own right to the file.
#[cfg(target_os = "linux")]
const KERNEL_FILE_SYSTEMS: &[(u32, &str)] = &[
    (0x0000_9fa0, "procfs"),
    (0x6265_6572, "sysfs"),
    (0x6462_6720, "debugfs"),
    (0x7472_6163, "tracefs"),
    (0x7363_6673, "securityfs"),
    (0xde5e_81e4, "efivarfs"),
    (0xcafe_4a11, "bpf"),
    (0x6165_676c, "pstore"),
    (0x6265_6570, "configfs"),
    (0x0027_e0eb, "cgroup"),
    (0x6367_7270, "cgroup2"),
    (0xf97c_ff8c, "selinuxfs"),
    (0x4341_5d53, "smackfs"),
    (0x4249_4e4d, "binfmt_misc"),
    (0x6573_5543, "fusectl"),
];

/// #3688 — a file a read opened as the person at seat0, held to what they
/// asked for before its handle leaves the step: never on a kernel file
/// system ([`KERNEL_FILE_SYSTEMS`]), and the very file asked for, by the
/// kernel's own path for the handle. A link swapped in after the path was
/// resolved is followed by the open; through a `/proc/<pid>/fd` link it
/// lands on a file the daemon holds open, past the folders that would have
/// refused the person. `Err` says why, in the words the viewer shows.
#[cfg(target_os = "linux")]
fn held_to_the_person(file: &std::fs::File, asked: &std::path::Path) -> std::io::Result<()> {
    held_to(file, asked, "file", "send")
}

/// #3688 — a folder a read lists as the person at seat0, held to what they
/// asked for by its handle before anything is listed, as a file is
/// ([`held_to_the_person`]): never on a kernel file system, and the very
/// folder asked for, by the kernel's own path for the handle. A link swapped
/// in after the path was resolved is followed by the open; through a
/// `/proc/<pid>/fd` link it lands on a folder the daemon holds open, past
/// the folders that would have refused the person.
#[cfg(target_os = "linux")]
fn folder_held_to_the_person(
    folder: &std::fs::File,
    asked: &std::path::Path,
) -> std::io::Result<()> {
    held_to(folder, asked, "folder", "list")
}

/// #3688 — `handle` held to `asked`, the `what` ("file", "folder") a read
/// opened to `does` ("send", "list") it. `Err` says why, in the words the
/// viewer shows.
#[cfg(target_os = "linux")]
fn held_to(
    handle: &std::fs::File,
    asked: &std::path::Path,
    what: &str,
    does: &str,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = handle.as_raw_fd();
    // SAFETY: `fstatfs` fills the zeroed struct it is handed, for a handle
    // this function borrows.
    let held_on = statfs_with(|st| unsafe { libc::fstatfs(fd, st) })?;
    off_the_kernel_file_systems(held_on, asked)?;
    // The kernel's own path for the handle: what the open reached, wherever
    // the path led by then.
    let held = std::fs::read_link(format!("/proc/self/fd/{fd}"))?;
    if held != asked {
        return Err(std::io::Error::other(format!(
            "{}: what opened is not the {what} asked for, so file transfer does not {does} it",
            asked.display()
        )));
    }
    Ok(())
}

/// #3688 — the file system type `call` (an `fstatfs` on a handle) reports.
#[cfg(target_os = "linux")]
fn statfs_with(call: impl FnOnce(*mut libc::statfs) -> libc::c_int) -> std::io::Result<u32> {
    let mut st = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if call(st.as_mut_ptr()) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so the kernel filled the struct (zeroed
    // before it, so every field is initialised either way).
    let st = unsafe { st.assume_init() };
    // `f_type` is a signed word on most targets and a u32 on a few; every
    // magic fits 32 bits.
    #[allow(clippy::unnecessary_cast)]
    let magic = st.f_type as u32;
    Ok(magic)
}

/// #3688 — `Err`, in the words the viewer shows, when `magic` is one of
/// [`KERNEL_FILE_SYSTEMS`]. Not an access refusal: the viewer is told this,
/// not that Linux refused the person.
#[cfg(target_os = "linux")]
fn off_the_kernel_file_systems(magic: u32, path: &std::path::Path) -> std::io::Result<()> {
    match KERNEL_FILE_SYSTEMS.iter().find(|(m, _)| *m == magic) {
        Some((_, fs)) => Err(std::io::Error::other(format!(
            "{}: {fs} is a kernel file system, which file transfer does not read for the person \
             signed in at this computer's screen",
            path.display()
        ))),
        None => Ok(()),
    }
}

/// Byte-count snapshot emitted after a chunk that crossed a progress
/// threshold. Owned so the caller can serialize it outside the state
/// lock.
pub struct ChunkProgress {
    pub id: String,
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// Filename + path helpers

/// Sanitize a browser-provided filename to a safe basename. Strips any
/// directory components and replaces characters outside
/// `[A-Za-z0-9._ -]` with `_`. Falls back to `download.bin` for empty
/// input.
pub fn sanitize_filename(name: &str) -> String {
    // Take the last path component. Browsers normally send just a
    // basename but some send full paths on some platforms (drag-and-
    // drop from Finder, etc.).
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "download.bin".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolve a folder-upload's relative path under Downloads/. Splits on
/// `/` (the canonical separator the browser sends — Chrome /
/// Firefox / Safari all return forward-slash relative paths from
/// `webkitGetAsEntry()`); sanitises each component with the existing
/// [`sanitize_filename`] rules so a malicious browser can't smuggle
/// `..` or absolute paths; rejects empty / single-component inputs
/// (those should use the flat-upload path); and applies
/// [`unique_path`] to the FILE component so a re-upload gets a
/// `(2)` rename suffix instead of overwriting.
///
/// Returns `None` for inputs that produce no usable path (all
/// components sanitised to empty, deeper than 32 levels — a sane
/// nesting cap that catches degenerate inputs while passing every
/// realistic project tree).
///
/// `dir` is the Downloads directory; the returned path lives under
/// `dir/<root>/<sub...>/<file>` with each segment safe.
fn resolve_folder_upload_path(dir: &std::path::Path, rel: &str) -> Option<PathBuf> {
    let (folders, file_name) = folder_upload_parts(rel)?;
    let mut path = dir.to_path_buf();
    for c in &folders {
        path.push(c);
    }
    // Apply collision-safe rename to the leaf file. The directory
    // prefix is shared across all files in a folder upload so we
    // intentionally do NOT version-suffix it — a re-upload of the
    // same folder merges files into the same destination directory
    // (each colliding file picks up its own `(2)` suffix). This
    // matches the behaviour operators expect from a desktop
    // file-manager paste-on-existing-folder.
    Some(unique_path(&path, &file_name))
}

/// A folder upload's relative path as its folders and its file name, each
/// component sanitised ([`resolve_folder_upload_path`]'s rules); `None` for
/// one with fewer than two usable components or deeper than 32. #3688 — the
/// end of a held transfer makes those folders from the folder it holds.
fn folder_upload_parts(rel: &str) -> Option<(Vec<String>, String)> {
    // Normalise separators: Windows-style backslashes can sneak in
    // if a buggy browser converts paths.
    let normalised = rel.replace('\\', "/");
    let mut components: Vec<String> = normalised
        .split('/')
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(sanitize_filename)
        .filter(|c| !c.is_empty() && c != "_")
        .collect();
    if components.len() < 2 {
        // Need at least <root>/<file>; otherwise the rel_path is
        // empty or a single basename — caller should fall back to
        // flat upload.
        return None;
    }
    if components.len() > 32 {
        return None; // pathological depth
    }
    // Last component is the file; everything before is dir hierarchy.
    let file_name = components.pop()?;
    Some((components, file_name))
}

/// Given a base directory and a desired filename, return a path that
/// doesn't collide with an existing file — appends `(2)`, `(3)` etc.
/// before the extension when needed.
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_stem_ext(name);
    for n in 2..1000u32 {
        let suffixed = if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        let p = dir.join(&suffixed);
        if !p.exists() {
            return p;
        }
    }
    // Exceedingly unlikely — hand back the original and let create()
    // overwrite.
    candidate
}

fn split_stem_ext(name: &str) -> (&str, &str) {
    if let Some(idx) = name.rfind('.')
        && idx > 0
        && idx < name.len() - 1
    {
        return (&name[..idx], &name[idx + 1..]);
    }
    (name, "")
}

/// Where an incoming transfer lands: the configured `files_dir` when it
/// passes for THIS writer at THIS moment, else the default ladder below.
///
/// FR-84 D4 — this is the USE-time gate, and the load-bearing one: it holds
/// even when the value reached the config file behind the daemon's back (a
/// hand edit, the desktop's direct-file fallback while the daemon was down,
/// a user switch since it was set). The full check runs per transfer —
/// shape, placement for the identity doing the writing and the user signed
/// in now, links followed, created if missing, a write probe — and a
/// refusal is logged and falls back rather than failing the drop.
/// FR-97 P3d — the drop folder for `who`, resolved as them (the configured
/// folder's gate creates it and probes a write).
#[cfg(any(test, not(target_os = "linux")))]
async fn download_dir_for(who: &FsAs) -> Result<PathBuf> {
    drop_folder_for(who).await.map(|(folder, _)| folder)
}

/// [`download_dir_for`], and whether the folder is the daemon's own default
/// one ([`default_download_dir`]), which a begin then holds to its rule on
/// the handle its staging opens (#3688).
async fn drop_folder_for(who: &FsAs) -> Result<(PathBuf, bool)> {
    let w = who.clone();
    who.run(move || drop_folder_as(&w)).await
}

/// The drop folder for `who` ([`drop_folder_as`]), on a thread under their
/// identity.
#[cfg(all(test, target_os = "windows"))]
fn download_dir_as(who: &FsAs) -> Result<PathBuf> {
    drop_folder_as(who).map(|(folder, _)| folder)
}

/// [`download_dir_as`], and whether the folder is the daemon's own default
/// one. For the person at seat0: the configured folder when it passes for
/// them (inside THEIR home, FR-84 D4's rule), else their own Downloads,
/// neither the daemon's.
fn drop_folder_as(who: &FsAs) -> Result<(PathBuf, bool)> {
    match who {
        FsAs::Daemon => drop_folder_with(&writer_context()),
        // #3688 — nobody at seat0: the configured folder is checked with no
        // active home ([`FsAs::writer`]), so it never resolves in the
        // daemon's own home, and the drop lands in the default folder.
        #[cfg(target_os = "linux")]
        FsAs::DaemonAlone => drop_folder_with(&who.writer()),
        #[cfg(target_os = "linux")]
        FsAs::Person { home, .. } => {
            if let Some(raw) = configured_files_dir() {
                let writer = who.writer();
                match files_dir::validate(&raw, &writer, &files_dir::Rules::from_env()) {
                    Ok(dir) => return Ok((dir, false)),
                    Err(e) => tracing::warn!(
                        files_dir = %raw,
                        active_home = ?writer.active_home,
                        %e,
                        "files: configured files_dir refused for the person at seat0 — this \
                         transfer lands in their Downloads"
                    ),
                }
            }
            Ok((person_downloads(home), false))
        }
        // #3048 — the same for the person signed in to a Windows worker's
        // session: their `files_dir` when it passes for them (inside THEIR
        // profile), else their own Downloads, else their profile.
        #[cfg(target_os = "windows")]
        FsAs::Person {
            downloads, profile, ..
        } => {
            if let Some(raw) = configured_files_dir() {
                let writer = who.writer();
                match files_dir::validate(&raw, &writer, &files_dir::Rules::from_env()) {
                    Ok(dir) => return Ok((dir, false)),
                    Err(e) => tracing::warn!(
                        files_dir = %raw,
                        active_home = ?writer.active_home,
                        %e,
                        "files: configured files_dir refused for the person signed in — this \
                         transfer lands in their Downloads"
                    ),
                }
            }
            Ok((
                person_downloads_windows(downloads.as_deref(), profile, true),
                false,
            ))
        }
        #[cfg(target_os = "windows")]
        FsAs::Nobody => Err(anyhow!(NOBODY_SIGNED_IN)),
        #[cfg(target_os = "linux")]
        FsAs::Nobody => Err(anyhow!(NOBODY_AT_THE_SCREEN)),
        #[cfg(target_os = "linux")]
        FsAs::CannotTell => Err(anyhow!(CANNOT_TELL)),
    }
}

/// [`default_download_dir`] for `who`, on a thread under their identity.
fn default_download_dir_as(who: &FsAs, log: bool) -> Result<PathBuf> {
    match who {
        FsAs::Daemon => default_download_dir(log),
        #[cfg(target_os = "linux")]
        FsAs::DaemonAlone => default_download_dir(log),
        #[cfg(target_os = "linux")]
        FsAs::Person { home, .. } => Ok(person_downloads(home)),
        // `log` is false for the status card and the sweep: no probe there.
        #[cfg(target_os = "windows")]
        FsAs::Person {
            downloads, profile, ..
        } => Ok(person_downloads_windows(downloads.as_deref(), profile, log)),
        #[cfg(target_os = "windows")]
        FsAs::Nobody => Err(anyhow!(NOBODY_SIGNED_IN)),
        #[cfg(target_os = "linux")]
        FsAs::Nobody => Err(anyhow!(NOBODY_AT_THE_SCREEN)),
        #[cfg(target_os = "linux")]
        FsAs::CannotTell => Err(anyhow!(CANNOT_TELL)),
    }
}

/// Where the daemon's own drop lands: [`drop_folder_with`] for this process.
#[cfg(test)]
fn download_dir() -> Result<PathBuf> {
    drop_folder_with(&writer_context()).map(|(folder, _)| folder)
}

/// The drop folder for `writer`: the configured folder when it passes for
/// it, else the default ladder, and whether it is that default folder.
fn drop_folder_with(writer: &files_dir::Writer) -> Result<(PathBuf, bool)> {
    if let Some(raw) = configured_files_dir() {
        match files_dir::validate(&raw, writer, &files_dir::Rules::from_env()) {
            Ok(dir) => return Ok((dir, false)),
            Err(e) => tracing::warn!(
                files_dir = %raw,
                privileged = writer.privileged,
                active_home = ?writer.active_home,
                %e,
                "files: configured files_dir refused — this transfer lands in the default folder"
            ),
        }
    }
    Ok((default_download_dir(true)?, true))
}

/// The default ladder — what `files_dir` unset has always meant. `log` =
/// false for the status answer, which is recomputed every few seconds while
/// the companion is open and must not repeat the fallback's warning each time.
///
/// #3688 — on Linux it is the daemon's own folder, never one another account
/// can write: the XDG Downloads folder when it exists and passes, else
/// `~/Downloads`, made 0700 when a file arrives (`log`; looking makes
/// nothing), else a refusal saying why, which the viewer shows. Each is
/// opened following links, and the folder it leads to must be the writer's
/// own; for root, one no other account may write; for a per-user daemon,
/// one not every account may (Ubuntu's private-group umask makes
/// `~/Downloads` 0775), unless its file system keeps no owners or modes,
/// where it need only be one the daemon can write into
/// (`staging::may_receive`). Root's home is its account's, and must pass
/// root's rule too ([`own_home_and_downloads`]). It was the temp dir: on a
/// server, root's was `/tmp`.
#[cfg(target_os = "linux")]
fn default_download_dir(log: bool) -> Result<PathBuf> {
    let uid = own_euid();
    let strict = uid == 0;
    let cannot = |folder: &std::path::Path, why: &str| {
        anyhow!("{} cannot receive files: {why}", folder.display())
    };
    let (home, xdg) = own_home_and_downloads();
    if let Some(xdg) = xdg {
        match staging::open_folder(&xdg) {
            Ok(held) => match staging::may_receive(&held, uid, strict) {
                Ok(()) => return Ok(xdg),
                Err(why) if log => tracing::warn!(
                    folder = %xdg.display(),
                    %why,
                    "files: the XDG Downloads folder cannot receive files — trying ~/Downloads"
                ),
                Err(_) => {}
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if log => tracing::warn!(
                folder = %xdg.display(),
                %e,
                "files: the XDG Downloads folder does not open — trying ~/Downloads"
            ),
            Err(_) => {}
        }
    }
    let home = home.map_err(|why| anyhow!("{why}"))?;
    let downloads = home.join("Downloads");
    let held = match staging::open_folder(&downloads) {
        Ok(held) => held,
        // Looking makes nothing: where the first file would make it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !log => return Ok(downloads),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt;
            match std::fs::DirBuilder::new().mode(0o700).create(&downloads) {
                Ok(()) => tracing::info!(
                    folder = %downloads.display(),
                    "files: made the default folder, 0700"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(cannot(&downloads, &format!("it could not be made ({e})"))),
            }
            staging::open_folder(&downloads)
                .map_err(|e| cannot(&downloads, &format!("it does not open ({e})")))?
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
            return Err(cannot(&downloads, "it is not a folder"));
        }
        Err(e) => return Err(cannot(&downloads, &format!("it does not open ({e})"))),
    };
    staging::may_receive(&held, uid, strict).map_err(|why| cannot(&downloads, &why))?;
    Ok(downloads)
}

/// #3688 — this process's effective uid: root's 0 for a system install.
#[cfg(target_os = "linux")]
fn own_euid() -> u32 {
    // SAFETY: `geteuid` reads this process's own credentials.
    unsafe { libc::geteuid() }
}

/// #3688 — the folder a received file for `who` would land in now, with
/// nothing made: the configured folder when it passes for them, else the
/// default one. Where a resume's lookup looks for a partial. As `who`.
#[cfg(target_os = "linux")]
fn drop_folder_without_creating(who: &FsAs) -> Result<PathBuf> {
    if let Ok(Some(configured)) = configured_folder_without_creating(who) {
        return Ok(configured);
    }
    default_download_dir_as(who, false)
}

/// #3688 — begin's staging on Linux, on the stager's thread: the target
/// folder opened (its links followed) and held; when begin resolved it as
/// the daemon's own default folder (`the_default`), that very handle
/// checked by its rule ([`default_download_dir`]); then the partial made
/// beside it and held (`staging::stage`): `data` 0600 for the daemon alone
/// (#3697), 0644 otherwise. R4 — a folder whose file system keeps no modes
/// (`staging::keeps_modes`) stages in the stager's private folder instead
/// ([`private_staging_root`], `staging::stage_privately`), and a root
/// writer stays strict there: never into a folder another account can
/// write.
#[cfg(target_os = "linux")]
fn stage_held(
    stager: &FsAs,
    target_dir: &std::path::Path,
    id: &str,
    meta: &[u8],
    private: bool,
    the_default: bool,
) -> Result<(staging::Held, std::fs::File)> {
    #[cfg(test)]
    stand_in::before_a_folder_opens(target_dir);
    let base = staging::open_folder(target_dir)
        .with_context(|| format!("opening {}", target_dir.display()))?;
    let cannot = |why: String| anyhow!("{} cannot receive files: {why}", target_dir.display());
    let uid = own_euid();
    if the_default {
        staging::may_receive(&base, uid, uid == 0).map_err(cannot)?;
    }
    let data_mode = if private { 0o600 } else { 0o644 };
    if staging::keeps_modes(&base) {
        return staging::stage(base, target_dir, id, meta, data_mode);
    }
    if stager.writes_as_root() {
        staging::may_receive(&base, uid, true).map_err(cannot)?;
    }
    let root = private_staging_root(stager).ok_or_else(|| {
        anyhow!(
            "{} keeps no file modes, and file transfer has no folder of its own to stage in",
            target_dir.display()
        )
    })?;
    staging::stage_privately(base, &root, id, meta, data_mode)
}

/// R4 — the stager's own private staging folder, on a file system that
/// keeps modes, for an upload into a folder whose file system keeps none
/// (`staging::keeps_modes`): the person's `~/.local/state/roomler/staging`
/// (their own `XDG_STATE_HOME` is not the daemon's to know), a per-user
/// daemon's `$XDG_STATE_HOME/roomler/staging` (else under
/// `~/.local/state`), root's `/var/lib/roomler/staging`. A test's stood-in
/// home holds the daemon's.
#[cfg(target_os = "linux")]
fn private_staging_root(stager: &FsAs) -> Option<PathBuf> {
    const OWN: &str = "roomler/staging";
    let state = |home: &std::path::Path| home.join(".local/state");
    match stager {
        FsAs::Person { home, .. } => Some(state(home).join(OWN)),
        FsAs::Daemon | FsAs::DaemonAlone => {
            #[cfg(test)]
            if let Some(home) = stand_in::own_home_now() {
                let home = home?;
                return Some(if own_euid() == 0 {
                    home.join("var/lib").join(OWN)
                } else {
                    state(&home).join(OWN)
                });
            }
            if own_euid() == 0 {
                return Some(PathBuf::from("/var/lib").join(OWN));
            }
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| directories::BaseDirs::new().map(|d| state(d.home_dir())))
                .map(|base| base.join(OWN))
        }
        FsAs::Nobody | FsAs::CannotTell => None,
    }
}

/// #3688, R4 — the home this process's account names in the account
/// database (`getpwuid_r` of its effective uid), never the `HOME` its
/// environment names: root's default folder sits in it, and FR-84 D4's
/// placement rule takes it as root's active home ([`writer_context`]). A
/// test stands it in (`stand_in::own_home`).
#[cfg(target_os = "linux")]
fn account_home() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(home) = stand_in::own_home_now() {
        return home;
    }
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0 as libc::c_char; 1024];
    // SAFETY: an all-zero `passwd` is a valid value (its pointers null),
    // read only after a successful call filled it.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    loop {
        // SAFETY: every pointer is to memory this frame owns, sized as
        // passed; the answer points into `buf`, read before it moves.
        let rc = unsafe {
            libc::getpwuid_r(
                own_euid(),
                &mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && buf.len() < 64 * 1024 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        break;
    }
    // SAFETY: a NUL-terminated string in `buf`, which the call filled.
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    let dir = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
    dir.is_absolute().then_some(dir)
}

/// #3688 rule 4 — whether `folder`, held as `held`, is one a root writer
/// (`FsAs::writes_as_root`) may place a received file in now: the default
/// folder, with that handle passing its rule; and for a daemon that is root
/// (not the daemon alone), also the configured folder as it resolves now, or
/// a folder a controller may choose now ([`resolve_dest_path_here`], its
/// canonical path the same). `Err` says why not. Run as `who`.
#[cfg(target_os = "linux")]
fn root_may_place_in(
    who: &FsAs,
    folder: &std::path::Path,
    held: &std::fs::File,
) -> Result<(), String> {
    match default_download_dir_as(who, false) {
        Ok(default) if default == folder => {
            let uid = own_euid();
            return staging::may_receive(held, uid, uid == 0);
        }
        // The default folder itself fails its rule now: say that.
        Err(e) if !matches!(who, FsAs::Daemon) => return Err(format!("{e:#}")),
        _ => {}
    }
    if !matches!(who, FsAs::Daemon) {
        return Err(format!(
            "with nobody at the screen only the default folder receives files, and {} is not it",
            folder.display()
        ));
    }
    if let Ok(Some(configured)) = configured_folder_without_creating(who)
        && configured == folder
    {
        return Ok(());
    }
    match resolve_dest_path_here(
        &folder.to_string_lossy(),
        &who.writer(),
        &files_dir::Rules::from_env(),
    ) {
        Ok(canonical) if canonical == folder => Ok(()),
        Ok(canonical) => Err(format!(
            "{} resolves to {}",
            folder.display(),
            canonical.display()
        )),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The default ladder elsewhere: the Downloads `directories` names (always
/// `~/Downloads` on macOS), else Windows' machine folder.
#[cfg(not(target_os = "linux"))]
fn default_download_dir(log: bool) -> Result<PathBuf> {
    // #3048 — a SystemContext worker (LocalSystem in the person's session)
    // writes as the person signed in, into THEIR Downloads, read from their
    // token (`FsAs::Person`, `person_downloads_windows`). This ladder is the
    // daemon's own: a worker that is not LocalSystem, or one with
    // `ROOMLERD_FILES_AS_SEAT_USER=0`.
    if let Some(dirs) = directories::UserDirs::new()
        && let Some(dl) = dirs.download_dir()
    {
        return Ok(dl.to_path_buf());
    }
    // Windows-specific final fallback (the field-test host 2026-05-11 rc.20 field
    // repro): when the worker is LocalSystem and the user's `Downloads`
    // is Folder-Redirected to a network share (e.g.
    // `\\fileserver\UserData$\<user>\Downloads`) that SYSTEM can't
    // access, the session user's Downloads was out of reach AND
    // `UserDirs::new().download_dir()` returns None. Pre-rc.21 the
    // code fell back to `std::env::temp_dir()` = `C:\Windows\SystemTemp\`
    // — which Windows Defender / SmartScreen scan AGGRESSIVELY for
    // any `.exe`-named write. Defender held the staging `data` file
    // open during write, `write_all` never landed bytes, browser SCTP
    // buffer overflowed, retry budget exhausted. Switch the final
    // fallback to `%PROGRAMDATA%\roomler\roomler-agent\uploads\` —
    // SYSTEM-writable, persistent across reboots, NOT under
    // Defender's SystemTemp-scan policy. Operator can find their
    // landed files there via the `files:complete { path }` reply.
    #[cfg(target_os = "windows")]
    {
        let staging = crate::appdirs::machine_global_dir().join("uploads");
        if log {
            tracing::warn!(
                fallback_path = %staging.display(),
                "files: no user-accessible Downloads dir (Folder Redirection?); staging in PROGRAMDATA"
            );
        }
        Ok(staging)
    }
    // macOS: `directories` always names `~/Downloads` there, so this temp-dir
    // fallback is not reached. (Linux has its own ladder, above: #3688.)
    #[cfg(not(target_os = "windows"))]
    {
        let _ = log;
        Ok(std::env::temp_dir())
    }
}

/// #3688 — the daemon's own home (`Err`: why there is none to take), and
/// the Downloads folder its `user-dirs.dirs` names (`XDG_DOWNLOAD_DIR`). A
/// per-user daemon's: `$HOME`, and the folder as `directories` reads it.
/// R4 — root's: the home its account names ([`account_home`]), never the
/// `HOME` its environment names, and only when that home passes root's own
/// rule (`staging::may_receive`); its `user-dirs.dirs` read only when it is
/// root's own plain file nobody else may write, and taken only for a folder
/// inside that home ([`xdg_download_dir`]). A test stands homes in for its
/// thread (`stand_in::own_home`, `stand_in::env_home`).
#[cfg(target_os = "linux")]
fn own_home_and_downloads() -> (Result<PathBuf, String>, Option<PathBuf>) {
    const NO_HOME: &str =
        "file transfer has no folder to receive files into: this account has no home folder";
    if own_euid() == 0 {
        let Some(home) = account_home() else {
            return (Err(NO_HOME.into()), None);
        };
        let cannot = |why: &str| Err(format!("{} cannot receive files: {why}", home.display()));
        let held = match staging::open_folder(&home) {
            Ok(held) => held,
            Err(e) => return (cannot(&format!("it does not open ({e})")), None),
        };
        if let Err(why) = staging::may_receive(&held, own_euid(), true) {
            return (cannot(&why), None);
        }
        let xdg = staging::read_own_file(&held, ".config/user-dirs.dirs")
            .and_then(|text| xdg_download_dir(&text, &home));
        return (Ok(home), xdg);
    }
    // The environment's home as `BaseDirs` reads it, and its
    // `user-dirs.dirs` as `UserDirs` does.
    #[cfg(test)]
    if let Some(home) = stand_in::env_home_now().or_else(stand_in::own_home_now) {
        let xdg = home.as_deref().and_then(|home| {
            std::fs::read_to_string(home.join(".config/user-dirs.dirs"))
                .ok()
                .and_then(|text| stand_in::xdg_as_directories_reads_it(&text, home))
        });
        return (home.ok_or_else(|| NO_HOME.into()), xdg);
    }
    (
        directories::BaseDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .ok_or_else(|| NO_HOME.into()),
        directories::UserDirs::new()
            .and_then(|d| d.download_dir().map(std::path::Path::to_path_buf)),
    )
}

/// #3669 — who the Windows tests stand in, shared with the peer's tests,
/// which drive a folder download through its pump.
///
/// A test runner is not SYSTEM and has no session user to ask for. So
/// [`stand_in::person`] is the runner's own SID through a low-integrity copy
/// of its token, the runner itself (at medium or high) plays the daemon, and
/// [`stand_in::label_no_read_up`] makes a file or a folder one only the
/// daemon may read. [`stand_in::reader`] stands a reader in for what
/// [`FsAs::for_read`] answers: the public entry points (`begin_outgoing`,
/// `begin_outgoing_zip`, `list_dir`) would otherwise always read as the
/// daemon here, and could not be told from ones that never ask.
///
/// #3688 — on Linux [`stand_in::seat`] stands in who is at seat0 instead, so
/// `FsAs::for_read` itself still decides: a real account (`nobody`, in the
/// tests run as root) at the screen is read as through the very rule a
/// daemon takes. [`stand_in::swap_before_open`] swaps a folder for a link
/// just before a read opens it.
#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
pub(crate) mod stand_in {
    #[cfg(target_os = "windows")]
    use super::FsAs;
    #[cfg(target_os = "linux")]
    use super::Seat;
    #[cfg(target_os = "windows")]
    use crate::win_identity::PersonToken;
    use std::path::Path;
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;
    #[cfg(target_os = "windows")]
    use std::sync::Arc;
    use std::sync::{Mutex, PoisonError};

    /// The reader stood in, with the thread it answers on: one at a time.
    #[cfg(target_os = "windows")]
    static READER: Mutex<Option<(std::thread::ThreadId, FsAs)>> = Mutex::new(None);

    /// The tests that stand a reader (or a seat) in take turns, each for as
    /// long as its stand-in lives.
    static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// While this lives, `FsAs::for_read()` answers its reader on the thread
    /// that stood it in.
    #[cfg(target_os = "windows")]
    pub(crate) struct Reader {
        _turn: tokio::sync::MutexGuard<'static, ()>,
    }

    #[cfg(target_os = "windows")]
    impl Drop for Reader {
        fn drop(&mut self) {
            *READER.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    /// Stand `who` in for what `FsAs::for_read()` answers, on this thread
    /// only. A `#[tokio::test]` polls its whole body on its own thread, and
    /// the tests that run beside it, which read through `for_read()` on
    /// threads of their own, keep the daemon.
    #[cfg(target_os = "windows")]
    pub(crate) async fn reader(who: FsAs) -> Reader {
        let turn = TURN.lock().await;
        *READER.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((std::thread::current().id(), who));
        Reader { _turn: turn }
    }

    /// The reader stood in for this thread, if there is one.
    #[cfg(target_os = "windows")]
    pub(super) fn reader_now() -> Option<FsAs> {
        let held = READER.lock().unwrap_or_else(PoisonError::into_inner);
        held.as_ref()
            .filter(|(thread, _)| *thread == std::thread::current().id())
            .map(|(_, who)| who.clone())
    }

    /// #3688 — who is at seat0, stood in on one thread: the seat the
    /// lookup answers, and how many more times a folder download finds them
    /// still there (`None`: always). #3711 — or the lookup itself
    /// ([`seat_asked`]), run where the real one runs.
    #[cfg(target_os = "linux")]
    struct StoodSeat {
        thread: std::thread::ThreadId,
        seat: Seat,
        stays_for: Option<u32>,
        asked: Option<Asked>,
    }

    /// #3711 — a seat0 lookup stood in: it runs on whatever thread the real
    /// one would, so a test sees which thread that is, and how long its
    /// caller waits for it.
    #[cfg(target_os = "linux")]
    type Asked = std::sync::Arc<dyn Fn() -> Seat + Send + Sync>;

    #[cfg(target_os = "linux")]
    static SEAT: Mutex<Option<StoodSeat>> = Mutex::new(None);

    /// While this lives, the seat0 lookup answers its seat on the thread
    /// that stood it in.
    #[cfg(target_os = "linux")]
    pub(super) struct SeatStoodIn {
        _turn: tokio::sync::MutexGuard<'static, ()>,
    }

    #[cfg(target_os = "linux")]
    impl Drop for SeatStoodIn {
        fn drop(&mut self) {
            *SEAT.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    /// Stand `seat` in for who is at seat0, on this thread only, as
    /// [`reader`] does on Windows. `FsAs::for_read()` still decides who
    /// reads, from the process (root, the switch) and this seat.
    #[cfg(target_os = "linux")]
    pub(super) async fn seat(seat: Seat) -> SeatStoodIn {
        stand(seat, None).await
    }

    /// [`seat`], whose person a folder download finds still at the screen
    /// `checks` times, and gone after.
    #[cfg(target_os = "linux")]
    pub(super) async fn seat_leaving_after(seat: Seat, checks: u32) -> SeatStoodIn {
        stand(seat, Some(checks)).await
    }

    /// #3711 — stand `lookup` in for the seat0 lookup itself, on this thread
    /// only: `FsAs::for_read()` and `FsAs::for_write()` run it where they run
    /// the real one, and decide from what it answers, if it answers in time.
    #[cfg(target_os = "linux")]
    pub(super) async fn seat_asked(
        lookup: impl Fn() -> Seat + Send + Sync + 'static,
    ) -> SeatStoodIn {
        let turn = TURN.lock().await;
        *SEAT.lock().unwrap_or_else(PoisonError::into_inner) = Some(StoodSeat {
            thread: std::thread::current().id(),
            seat: Seat::CannotTell("a stood-in lookup".into()),
            stays_for: None,
            asked: Some(std::sync::Arc::new(lookup)),
        });
        SeatStoodIn { _turn: turn }
    }

    #[cfg(target_os = "linux")]
    async fn stand(seat: Seat, stays_for: Option<u32>) -> SeatStoodIn {
        let turn = TURN.lock().await;
        *SEAT.lock().unwrap_or_else(PoisonError::into_inner) = Some(StoodSeat {
            thread: std::thread::current().id(),
            seat,
            stays_for,
            asked: None,
        });
        SeatStoodIn { _turn: turn }
    }

    /// The seat0 lookup stood in for this thread, if there is one: a seat
    /// stood in answers at once; a lookup stood in ([`seat_asked`]) runs.
    #[cfg(target_os = "linux")]
    pub(super) fn lookup_now() -> Option<Box<dyn FnOnce() -> Seat + Send>> {
        let held = SEAT.lock().unwrap_or_else(PoisonError::into_inner);
        let stood = held
            .as_ref()
            .filter(|stood| stood.thread == std::thread::current().id())?;
        Some(match &stood.asked {
            Some(asked) => {
                let asked = asked.clone();
                Box::new(move || asked())
            }
            None => {
                let seat = stood.seat.clone();
                Box::new(move || seat)
            }
        })
    }

    /// #3688 — seat0's active uid as a folder download finds it, stood in
    /// for this thread: the seat's, while its person stays, and none after.
    /// `None` when no seat is stood in here.
    #[cfg(target_os = "linux")]
    pub(super) fn seat0_uid_now() -> Option<Option<u32>> {
        let mut held = SEAT.lock().unwrap_or_else(PoisonError::into_inner);
        let stood = held
            .as_mut()
            .filter(|stood| stood.thread == std::thread::current().id())?;
        if let Some(left) = stood.stays_for.as_mut() {
            if *left == 0 {
                return Some(None);
            }
            *left -= 1;
        }
        Some(match &stood.seat {
            Seat::Person { account, .. } => Some(account.uid),
            Seat::Root => Some(0),
            Seat::Nobody(_) | Seat::CannotTell(_) | Seat::OutOfTime(_) => None,
        })
    }

    /// What a read does just before it opens a folder, once: by the
    /// folder's path.
    #[cfg(target_os = "linux")]
    type BeforeOpen = (PathBuf, Box<dyn FnOnce() + Send>);

    #[cfg(target_os = "linux")]
    static BEFORE_OPEN: Mutex<Vec<BeforeOpen>> = Mutex::new(Vec::new());

    /// #3688 — once a read has resolved `dir` and is about to open it, `dir`
    /// is moved aside and a link to `to` takes its place: a component
    /// swapped after it was resolved. The swap runs on the step's own
    /// thread, with its rights, so the folder `dir` is in must be theirs.
    #[cfg(target_os = "linux")]
    pub(super) fn swap_before_open(dir: &Path, to: &Path) {
        let (dir, to) = (dir.to_path_buf(), to.to_path_buf());
        let aside = dir.with_extension("aside");
        let swap = {
            let dir = dir.clone();
            move || {
                std::fs::rename(&dir, &aside).expect("moving the folder aside");
                std::os::unix::fs::symlink(&to, &dir).expect("a link in its place");
            }
        };
        BEFORE_OPEN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((dir, Box::new(swap)));
    }

    // #3688 — the daemon's own home, stood in on one thread: `Some(home)`,
    // or `None` for an account with no home. The default folder's ladder
    // reads it (`own_home_and_downloads`), and `FsAs::run` carries it to the
    // blocking thread a step runs on, so no test touches a real home,
    // `/root/Downloads` or the temp dir.
    #[cfg(target_os = "linux")]
    thread_local! {
        static OWN_HOME: std::cell::RefCell<Option<Option<PathBuf>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// While this lives, the daemon's own home is the one stood in, on the
    /// thread that stood it in; what was stood in before comes back after.
    #[cfg(target_os = "linux")]
    pub(crate) struct OwnHomeStoodIn {
        before: Option<Option<PathBuf>>,
    }

    #[cfg(target_os = "linux")]
    impl Drop for OwnHomeStoodIn {
        fn drop(&mut self) {
            let before = self.before.take();
            OWN_HOME.with(|home| *home.borrow_mut() = before);
        }
    }

    /// #3688 — stand `home` in for the daemon's own home on this thread
    /// (`None`: it has none).
    #[cfg(target_os = "linux")]
    pub(crate) fn own_home(home: Option<&Path>) -> OwnHomeStoodIn {
        let before = OWN_HOME.with(|h| h.borrow_mut().replace(home.map(Path::to_path_buf)));
        OwnHomeStoodIn { before }
    }

    /// The home stood in on this thread, if one is.
    #[cfg(target_os = "linux")]
    pub(crate) fn own_home_now() -> Option<Option<PathBuf>> {
        OWN_HOME.with(|home| home.borrow().clone())
    }

    // #3688 — the home the daemon's environment names (`$HOME`, which
    // `directories` reads), stood in on one thread apart from its account's
    // own ([`own_home`]): a daemon started with another account's `HOME`.
    // With none stood in, the environment names the account's own.
    #[cfg(target_os = "linux")]
    thread_local! {
        static ENV_HOME: std::cell::RefCell<Option<Option<PathBuf>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// While this lives, the environment's home is the one stood in, on the
    /// thread that stood it in.
    #[cfg(target_os = "linux")]
    pub(crate) struct EnvHomeStoodIn {
        before: Option<Option<PathBuf>>,
    }

    #[cfg(target_os = "linux")]
    impl Drop for EnvHomeStoodIn {
        fn drop(&mut self) {
            let before = self.before.take();
            ENV_HOME.with(|home| *home.borrow_mut() = before);
        }
    }

    /// #3688 — stand `home` in for the home the daemon's environment names,
    /// on this thread (`None`: it names none).
    #[cfg(target_os = "linux")]
    pub(crate) fn env_home(home: Option<&Path>) -> EnvHomeStoodIn {
        let before = ENV_HOME.with(|h| h.borrow_mut().replace(home.map(Path::to_path_buf)));
        EnvHomeStoodIn { before }
    }

    /// The environment's home stood in on this thread, if one is.
    #[cfg(target_os = "linux")]
    pub(crate) fn env_home_now() -> Option<Option<PathBuf>> {
        ENV_HOME.with(|home| home.borrow().clone())
    }

    /// `XDG_DOWNLOAD_DIR` out of a `user-dirs.dirs` the way `directories`
    /// reads it (dirs-sys): a quoted `$HOME/…` under `home`, or any absolute
    /// path; `$HOME/` alone, or anything else, names none.
    #[cfg(target_os = "linux")]
    pub(crate) fn xdg_as_directories_reads_it(text: &str, home: &Path) -> Option<PathBuf> {
        let value = text.lines().find_map(|line| {
            let rest = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=")?;
            rest.trim()
                .strip_prefix('"')?
                .strip_suffix('"')
                .map(str::to_string)
        })?;
        if value == "$HOME/" {
            return None;
        }
        match value.strip_prefix("$HOME/") {
            Some(rel) => Some(home.join(rel)),
            None => value.starts_with('/').then(|| PathBuf::from(value)),
        }
    }

    // #3688, R4 — whether the file systems a step stages on keep Unix owners
    // and modes, stood in on one thread: `Some(false)` stands in a FAT disk,
    // WSL's drive mounts without metadata, a share mounted with fixed modes.
    #[cfg(target_os = "linux")]
    thread_local! {
        static MODES_KEPT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }

    /// While this lives, every file system keeps modes as stood in, on the
    /// thread that stood it in.
    #[cfg(target_os = "linux")]
    pub(crate) struct ModesKeptStoodIn {
        before: Option<bool>,
    }

    #[cfg(target_os = "linux")]
    impl Drop for ModesKeptStoodIn {
        fn drop(&mut self) {
            MODES_KEPT.with(|kept| kept.set(self.before));
        }
    }

    /// #3688, R4 — stand in whether file systems keep modes, on this thread
    /// (`None`: as each really does).
    #[cfg(target_os = "linux")]
    pub(crate) fn modes_kept(kept: Option<bool>) -> ModesKeptStoodIn {
        ModesKeptStoodIn {
            before: MODES_KEPT.with(|k| k.replace(kept)),
        }
    }

    /// Whether file systems keep modes, as stood in on this thread.
    #[cfg(target_os = "linux")]
    pub(crate) fn modes_kept_now() -> Option<bool> {
        MODES_KEPT.with(std::cell::Cell::get)
    }

    /// What a test stood in on its thread, for the blocking thread a step
    /// runs on (`FsAs::run`).
    #[cfg(target_os = "linux")]
    pub(crate) struct Carried {
        own_home: Option<Option<PathBuf>>,
        env_home: Option<Option<PathBuf>>,
        modes_kept: Option<bool>,
    }

    /// What this thread stood in, to carry.
    #[cfg(target_os = "linux")]
    pub(crate) fn carried() -> Carried {
        Carried {
            own_home: own_home_now(),
            env_home: env_home_now(),
            modes_kept: modes_kept_now(),
        }
    }

    /// The carried stand-ins, stood in on this thread while it lives.
    #[cfg(target_os = "linux")]
    pub(crate) struct CarriedStoodIn {
        _own_home: Option<OwnHomeStoodIn>,
        _env_home: Option<EnvHomeStoodIn>,
        _modes_kept: ModesKeptStoodIn,
    }

    #[cfg(target_os = "linux")]
    impl Carried {
        pub(crate) fn stand(self) -> CarriedStoodIn {
            CarriedStoodIn {
                _own_home: self.own_home.map(|home| own_home(home.as_deref())),
                _env_home: self.env_home.map(|home| env_home(home.as_deref())),
                _modes_kept: modes_kept(self.modes_kept),
            }
        }
    }

    /// What a begin does once it has opened an upload's `.roomler-partial`
    /// and before it makes the upload's own folder in it: by the path of the
    /// folder that staging sits in.
    #[cfg(target_os = "linux")]
    type BeforeTheId = (PathBuf, Box<dyn FnOnce() + Send>);

    #[cfg(target_os = "linux")]
    static BEFORE_THE_ID: Mutex<Vec<BeforeTheId>> = Mutex::new(Vec::new());

    /// #3688 — once a begin has opened `base`'s `.roomler-partial`, and
    /// before it makes the upload's folder in it, that `.roomler-partial` is
    /// removed: another upload's end removing it once empty, between the two.
    #[cfg(target_os = "linux")]
    pub(crate) fn staging_goes_before_the_id_is_made(base: &Path) {
        let staging = base.join(".roomler-partial");
        BEFORE_THE_ID
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                base.to_path_buf(),
                Box::new(move || {
                    std::fs::remove_dir(&staging).expect("removing the empty staging folder");
                }),
            ));
    }

    // #3688 — what `staging::keeps_modes`'s probe meets between making its
    // folder and opening it, once, on the thread that stood it in.
    #[cfg(target_os = "linux")]
    type ProbeStep = Box<dyn FnOnce(&Path)>;

    #[cfg(target_os = "linux")]
    thread_local! {
        static BEFORE_THE_PROBE_OPENS: std::cell::RefCell<Option<ProbeStep>> =
            const { std::cell::RefCell::new(None) };
    }

    /// #3688 — once `staging::keeps_modes`'s probe has made its folder, and
    /// before it opens it, `step` runs with that folder's path (through the
    /// folder it was made in): another account removing it, or putting a
    /// folder of its own in its place.
    #[cfg(target_os = "linux")]
    pub(crate) fn before_the_probe_opens(step: impl FnOnce(&Path) + 'static) {
        BEFORE_THE_PROBE_OPENS.with(|s| *s.borrow_mut() = Some(Box::new(step)));
    }

    /// Runs what this thread set with [`before_the_probe_opens`], once.
    #[cfg(target_os = "linux")]
    pub(crate) fn the_probe_is_made(probe: &Path) {
        if let Some(step) = BEFORE_THE_PROBE_OPENS.with(|s| s.borrow_mut().take()) {
            step(probe);
        }
    }

    /// Runs what a test set for `base` with
    /// [`staging_goes_before_the_id_is_made`], once.
    #[cfg(target_os = "linux")]
    pub(crate) fn before_the_id_is_made(base: &Path) {
        let step = {
            let mut held = BEFORE_THE_ID.lock().unwrap_or_else(PoisonError::into_inner);
            let at = held.iter().position(|(b, _)| b == base);
            at.map(|i| held.remove(i).1)
        };
        if let Some(step) = step {
            step();
        }
    }

    /// Runs what a test set for `dir` with [`swap_before_open`], once.
    #[cfg(target_os = "linux")]
    pub(super) fn before_a_folder_opens(dir: &Path) {
        let swap = {
            let mut held = BEFORE_OPEN.lock().unwrap_or_else(PoisonError::into_inner);
            let at = held.iter().position(|(d, _)| d == dir);
            at.map(|i| held.remove(i).1)
        };
        if let Some(swap) = swap {
            swap();
        }
    }

    /// The person: the runner's own SID, through a low-integrity copy of
    /// its token, with `downloads` as their Downloads.
    #[cfg(target_os = "windows")]
    pub(crate) fn person(profile: &Path, downloads: &Path) -> FsAs {
        with_token(
            profile,
            downloads,
            PersonToken::Fixed(Arc::new(low_integrity_token())),
        )
    }

    /// [`person`], who signs out after `steps` steps: every step after
    /// those is refused, as it is once a person has gone.
    #[cfg(target_os = "windows")]
    pub(crate) fn person_signing_out_after(steps: u32, profile: &Path) -> FsAs {
        with_token(
            profile,
            profile,
            PersonToken::SignsOutAfter {
                token: Arc::new(low_integrity_token()),
                steps_left: Arc::new(std::sync::atomic::AtomicU32::new(steps)),
            },
        )
    }

    #[cfg(target_os = "windows")]
    fn low_integrity_token() -> crate::win_token::Owned {
        crate::win_token::low_integrity_copy().expect("a low-integrity token")
    }

    #[cfg(target_os = "windows")]
    fn with_token(profile: &Path, downloads: &Path, token: PersonToken) -> FsAs {
        FsAs::Person {
            sid: crate::win_token::own_user_sid().expect("our own SID"),
            profile: profile.to_path_buf(),
            downloads: Some(downloads.to_path_buf()),
            token,
        }
    }

    /// `path` made one that only the daemon may read: a medium label that
    /// refuses reading up (`S:(ML;;NRNW;;;ME)`). The low-integrity person
    /// can neither read it nor list it; the runner can. An unlabelled file
    /// is the person's own.
    #[cfg(target_os = "windows")]
    pub(crate) fn label_no_read_up(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SetFileSecurityW,
        };
        let sddl: Vec<u16> = "S:(ML;;NRNW;;;ME)".encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string and an out-param; the
        // descriptor is freed below.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        assert!(ok != 0, "the no-read-up label as a descriptor");
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path and the descriptor made above.
        let set = unsafe { SetFileSecurityW(wide.as_ptr(), LABEL_SECURITY_INFORMATION, sd) };
        let why = std::io::Error::last_os_error();
        // SAFETY: allocated by the conversion, freed once.
        unsafe { LocalFree(sd as _) };
        assert!(set != 0, "labelling {}: {why}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-21 P4 — the sentinel must resolve on the AGENT, never in the viewer.
    ///
    /// The viewer used to hardcode `C:\ProgramData\roomler\roomler-agent\staging`,
    /// which `machine_global_dir()` only resolves to on a host that already
    /// carries the pre-rename tree. Verified broken on a real SYSTEM-mode
    /// Windows host, where that parent does not exist at all.
    #[tokio::test]
    async fn the_staging_sentinel_resolves_on_the_agent_and_never_errors() {
        let listing = list_dir(STAGING_SENTINEL).await;

        #[cfg(target_os = "windows")]
        {
            // Staging is transient, so the contract is "always lands somewhere
            // real": the staging dir when an update is in flight, otherwise the
            // machine-global root. Never an error, or the button is useless
            // except during the seconds an update is running.
            let listing = listing.expect("the sentinel must always resolve on Windows");
            let root = crate::appdirs::machine_global_dir();
            let root_s = root.to_string_lossy().to_lowercase();
            let got = listing.path.to_lowercase();
            let got = got.trim_start_matches(r"\\?\");
            if root.is_dir() {
                // A host something has written under (an enrolled box): the
                // staging dir while an update is in flight, else the root.
                assert!(
                    got.contains(root_s.trim_start_matches(r"\\?\")),
                    "resolved {got} is outside the machine-global root {root_s}"
                );
            } else {
                // A host nothing has written under yet — a fresh CI runner
                // (#2886): the nearest existing ancestor of the root, never an
                // error. The root's spelling continues the resolved path at a
                // separator, so `c:\programdata\room` could not pass.
                assert!(
                    root_s.starts_with(&format!("{got}\\")),
                    "resolved {got} is not an existing ancestor of the absent root {root_s}"
                );
            }
            // And it must NOT be the literal the viewer used to hold.
            assert!(
                !got.contains("roomler-agent") || root_s.contains("roomler-agent"),
                "resolved {got} used the pre-rename segment on a host whose root is {root_s}"
            );
        }

        #[cfg(not(target_os = "windows"))]
        {
            // No single staging root off Windows — saying so beats inventing a
            // path that would be wrong in a new way.
            //
            // Matched rather than `expect_err`: that would require
            // `DirListing: Debug`, which it does not implement. The arm compiles
            // only on Linux/macOS, so the mistake was invisible locally on
            // Windows and CI is what caught it.
            let msg = match listing {
                Ok(l) => panic!("non-Windows hosts have no staging root, got {}", l.path),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains("no single staging root"),
                "unhelpful error: {msg}"
            );
        }
    }

    /// The fallback the sentinel takes on a host where nothing has written
    /// under the machine-global root yet (#2886): the deepest directory that
    /// exists on the way up — the path itself when it is one, and a FILE on
    /// the way is not a directory to list, its parent is. On every OS, so the
    /// Linux lanes prove the choice the Windows caller makes.
    #[test]
    fn the_nearest_existing_dir_is_the_path_or_its_deepest_existing_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let here = tmp.path().to_path_buf();
        assert_eq!(
            nearest_existing_dir(&here).as_deref(),
            Some(here.as_path()),
            "a directory that exists is its own answer"
        );
        let absent = here.join("roomler").join("roomler");
        assert_eq!(
            nearest_existing_dir(&absent).as_deref(),
            Some(here.as_path()),
            "two absent segments resolve to the deepest existing ancestor"
        );
        let file = here.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            nearest_existing_dir(&file.join("below")).as_deref(),
            Some(here.as_path()),
            "a file on the way is skipped for its parent"
        );
    }

    #[test]
    fn the_sentinel_is_not_a_path_anything_could_canonicalise() {
        // If this ever became a real path the viewer could send, the resolver
        // arm would be dead and the defect would come back silently.
        assert!(STAGING_SENTINEL.starts_with('<') && STAGING_SENTINEL.ends_with('>'));
        assert!(!STAGING_SENTINEL.contains(char::from(92)));
        assert!(!STAGING_SENTINEL.contains('/'));
    }

    /// Serializes every test that mutates the process-global HOME /
    /// USERPROFILE (`begin()` resolves the Downloads dir through them, and
    /// tests run in parallel threads). Without this the mutations race:
    /// `round_trip_begin_chunk_end` resolved its destination inside
    /// `concurrent_upload_and_download…`'s hijacked HOME and failed on the
    /// final rename (flake observed 2026-08-14 under full-suite
    /// parallelism). tokio Mutex, not std: the guard is held across the
    /// tests' awaits by design.
    static HOME_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A folder THIS writer may use: the temp dir for an ordinary test
    /// runner; inside the active home for a privileged one (a root CI
    /// container), where the profile rule would refuse the temp dir.
    fn scratch_drop_dir(tag: &str) -> PathBuf {
        let w = writer_context();
        let base = if w.privileged {
            PathBuf::from(w.active_home.expect("a privileged test runner has a home"))
        } else {
            std::env::temp_dir()
        };
        base.join(format!("roomler-files-dir-{tag}-{}", std::process::id()))
    }

    /// Where an upload with no `dest_path` lands for the length of a test:
    /// a scratch drop folder, set as the process-wide `files_dir` (the
    /// production override `download_dir()` answers first), and put back to
    /// unset when the guard drops, a failed assertion included. Hold
    /// `FILES_DIR_TEST_LOCK` while it lives.
    ///
    /// ⚠️ Never a HOME/USERPROFILE redirect: on Windows the default ladder
    /// resolves the Downloads known folder and ignores both, so those tests
    /// wrote into the developer's REAL Downloads (452 `upload (N).txt` had
    /// piled up there by 2026-09-26), and rewriting USERPROFILE mid-run
    /// raced `win_identity`'s test, which reads it.
    struct ScratchDrops(PathBuf);

    impl ScratchDrops {
        fn new(tag: &str) -> Self {
            let dir = scratch_drop_dir(tag);
            let _ = std::fs::remove_dir_all(&dir);
            set_files_dir(Some(dir.to_string_lossy().into_owned()));
            Self(dir)
        }
    }

    impl Drop for ScratchDrops {
        fn drop(&mut self) {
            set_files_dir(None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// FR-84 D4 — the USE-time gate. A configured folder that passes is where
    /// the drop lands (created on first use); one the validator refuses —
    /// here a system root, the case a hand-edited config or the desktop's
    /// direct-file fallback can produce with no daemon to say no — falls back
    /// to the default ladder, per transfer, and the status answer names the
    /// folder a drop would really use. Unset = today's ladder.
    #[test]
    fn configured_files_dir_is_used_when_valid_and_ignored_when_not() {
        let _lock = FILES_DIR_TEST_LOCK.blocking_lock();
        let _home = HOME_ENV_LOCK.blocking_lock();
        // #3688 — the default folder's home, stood in: never a real one.
        #[cfg(target_os = "linux")]
        let own = tempfile::tempdir().unwrap();
        #[cfg(target_os = "linux")]
        let _own = stand_in::own_home(Some(own.path()));
        set_files_dir(None);
        let default = default_download_dir(false).unwrap();
        let default_s = default.to_string_lossy().into_owned();
        assert_eq!(download_dir().unwrap(), default, "unset = the ladder");
        assert_eq!(
            refresh_effective_files_dir().as_deref(),
            Some(default_s.as_str())
        );

        let dir = scratch_drop_dir("use");
        let _ = std::fs::remove_dir_all(&dir);
        let spelled = dir.to_string_lossy().into_owned();
        set_files_dir(Some(format!("  {spelled}  ")));
        assert_eq!(
            configured_files_dir().as_deref(),
            Some(spelled.as_str()),
            "trimmed"
        );
        // Before the first drop the folder does not exist, and the status
        // still names it: the drop creates it. Looking created nothing.
        assert_eq!(
            refresh_effective_files_dir().as_deref(),
            Some(spelled.as_str())
        );
        assert!(!dir.exists(), "the status answer creates nothing");
        assert_eq!(download_dir().unwrap(), dir);
        assert!(dir.is_dir(), "created on first use");

        let bad = if cfg!(windows) {
            r"C:\Windows\Temp\roomler-drops"
        } else {
            "/usr/roomler-drops"
        };
        set_files_dir(Some(bad.to_string()));
        assert_eq!(
            download_dir().unwrap(),
            default,
            "a refused value never wins over the ladder"
        );
        assert_eq!(
            refresh_effective_files_dir().as_deref(),
            Some(default_s.as_str()),
            "status reports where a drop would really land"
        );
        assert!(
            !std::path::Path::new(bad).exists(),
            "a refused folder is never created"
        );

        set_files_dir(Some("   ".into()));
        assert_eq!(configured_files_dir(), None, "blank = unset");
        assert_eq!(download_dir().unwrap(), default);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FR-84 D4 — a value that passed when it was set and fails now (here the
    /// folder was replaced by a file; a user switch or a new link are the
    /// field versions). The drop does not fail and does not follow the stale
    /// value: it lands in the default folder, and the status says so.
    #[test]
    fn a_files_dir_that_became_invalid_falls_back_at_use_time() {
        let _lock = FILES_DIR_TEST_LOCK.blocking_lock();
        let _home = HOME_ENV_LOCK.blocking_lock();
        // #3688 — the default folder's home, stood in: never a real one.
        #[cfg(target_os = "linux")]
        let own = tempfile::tempdir().unwrap();
        #[cfg(target_os = "linux")]
        let _own = stand_in::own_home(Some(own.path()));
        let dir = scratch_drop_dir("stale");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        let spelled = dir.to_string_lossy().into_owned();
        validate_files_dir_setting(&spelled).expect("accepted at set time");
        set_files_dir(Some(spelled.clone()));
        assert_eq!(download_dir().unwrap(), dir);

        // The folder becomes a file, retried should another test make the folder again.
        turn_the_folder_into_a_file(&dir, b"not a folder any more");
        let default = default_download_dir(false).unwrap();
        assert_eq!(
            download_dir().unwrap(),
            default,
            "the drop falls back instead of failing"
        );
        assert_eq!(
            refresh_effective_files_dir(),
            Some(default.to_string_lossy().into_owned()),
            "and the status answer follows"
        );
        assert!(
            validate_files_dir_setting(&spelled).is_err(),
            "the set-time gate would refuse it now too"
        );
        set_files_dir(None);
        let _ = std::fs::remove_file(&dir);
    }

    /// FR-84 D4 — the status read never computes: it answers from the cache
    /// (a disk or share lookup on the LocalAPI's async dispatch would stall
    /// every client), and an answer for an OLD value never overwrites the
    /// answer for the current one.
    #[test]
    fn effective_files_dir_answers_from_the_cache() {
        let _lock = FILES_DIR_TEST_LOCK.blocking_lock();
        let _home = HOME_ENV_LOCK.blocking_lock();
        let dir = scratch_drop_dir("cache");
        let spelled = dir.to_string_lossy().into_owned();
        set_files_dir(Some(spelled.clone()));
        assert_eq!(
            refresh_effective_files_dir().as_deref(),
            Some(spelled.as_str())
        );
        // Fresh ⇒ served from the cache, the same answer.
        assert_eq!(effective_files_dir().as_deref(), Some(spelled.as_str()));
        // A stale generation's late result is dropped on the floor.
        {
            let mut c = EFFECTIVE_FILES_DIR
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            c.generation += 1; // as if a newer value's answer had landed
        }
        refresh_effective_files_dir(); // computed for the (now older) current generation
        let c = EFFECTIVE_FILES_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            c.generation,
            FILES_DIR_GEN.load(Ordering::Acquire) + 1,
            "an older generation must not replace a newer one"
        );
        drop(c);
        // Put the cache back in step for whoever runs next.
        {
            let mut c = EFFECTIVE_FILES_DIR
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            c.generation = FILES_DIR_GEN.load(Ordering::Acquire);
            c.computed_at = None;
        }
        set_files_dir(None);
    }

    #[test]
    fn writer_context_describes_this_process() {
        let w = writer_context();
        assert!(w.active_home.is_some(), "a test runner has a home: {w:?}");
        #[cfg(target_os = "windows")]
        assert_eq!(w.privileged, crate::win_identity::process_is_local_system());
        #[cfg(unix)]
        assert_eq!(w.privileged, unsafe { libc::geteuid() } == 0);
    }

    #[test]
    fn sanitize_strips_path_components() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\Windows\\System32\\a.txt"), "a.txt");
        assert_eq!(sanitize_filename("normal.pdf"), "normal.pdf");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(
            sanitize_filename("my:weird*file?.txt"),
            "my_weird_file_.txt"
        );
    }

    #[test]
    fn sanitize_empty_input_falls_back() {
        assert_eq!(sanitize_filename(""), "download.bin");
        assert_eq!(sanitize_filename("/"), "download.bin");
        assert_eq!(sanitize_filename("///"), "download.bin");
    }

    #[test]
    fn split_stem_ext_handles_edges() {
        assert_eq!(split_stem_ext("report.pdf"), ("report", "pdf"));
        assert_eq!(split_stem_ext(".hidden"), (".hidden", ""));
        assert_eq!(split_stem_ext("trailing."), ("trailing.", ""));
        assert_eq!(split_stem_ext("noext"), ("noext", ""));
    }

    #[test]
    fn parse_files_begin() {
        let m: FilesIncoming =
            serde_json::from_str(r#"{"t":"files:begin","id":"abc","name":"x.bin","size":100}"#)
                .unwrap();
        match m {
            FilesIncoming::Begin {
                id,
                name,
                size,
                mime,
                ..
            } => {
                assert_eq!(id, "abc");
                assert_eq!(name, "x.bin");
                assert_eq!(size, 100);
                assert_eq!(mime, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_end() {
        let m: FilesIncoming = serde_json::from_str(r#"{"t":"files:end","id":"abc"}"#).unwrap();
        match m {
            FilesIncoming::End { id } => assert_eq!(id, "abc"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_begin_with_dest_path() {
        // Path-targeted upload extension (file-DC v2.2). The browser
        // sends `dest_path` when the operator drops onto the drawer's
        // current dir; the agent uses it as the upload target.
        let m: FilesIncoming = serde_json::from_str(
            r#"{"t":"files:begin","id":"f1","name":"x.bin","size":42,"dest_path":"C:\\Users\\me\\Documents"}"#,
        )
        .unwrap();
        match m {
            FilesIncoming::Begin {
                id,
                name,
                size,
                dest_path,
                rel_path,
                ..
            } => {
                assert_eq!(id, "f1");
                assert_eq!(name, "x.bin");
                assert_eq!(size, 42);
                assert_eq!(dest_path.as_deref(), Some("C:\\Users\\me\\Documents"));
                assert!(rel_path.is_none(), "dest_path doesn't imply rel_path");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_begin_with_dest_path_and_rel_path() {
        // The two extensions stack — folder-drop into an arbitrary
        // host directory.
        let m: FilesIncoming = serde_json::from_str(
            r#"{"t":"files:begin","id":"f1","name":"file.txt","size":42,"rel_path":"MyFolder/sub/file.txt","dest_path":"C:\\Projects"}"#,
        )
        .unwrap();
        match m {
            FilesIncoming::Begin {
                rel_path,
                dest_path,
                ..
            } => {
                assert_eq!(rel_path.as_deref(), Some("MyFolder/sub/file.txt"));
                assert_eq!(dest_path.as_deref(), Some("C:\\Projects"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_dest_path_accepts_existing_dir() {
        let base = std::env::temp_dir().join(format!(
            "roomler-dest-ok-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let resolved = resolve_dest_path(&base.to_string_lossy())
            .await
            .expect("dir should be accepted");
        // Canonicalisation may add a `\\?\` prefix on Windows; just
        // check the resolved path equals (or canonicalises to) the
        // input.
        assert!(resolved.exists());
        assert!(resolved.is_dir());
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn resolve_dest_path_rejects_file() {
        let base = std::env::temp_dir().join(format!(
            "roomler-dest-file-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let f = base.join("not-a-dir.txt");
        tokio::fs::write(&f, b"hi").await.unwrap();
        let res = resolve_dest_path(&f.to_string_lossy()).await;
        assert!(res.is_err(), "regular file should be rejected");
        assert!(
            res.unwrap_err().to_string().contains("not a directory"),
            "error mentions not-a-directory"
        );
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn resolve_dest_path_rejects_globalroot() {
        // Same denylist as validate_outgoing_path. We never get to
        // the metadata check.
        let res = resolve_dest_path(r"\\?\GLOBALROOT\Device\HarddiskVolume2\foo").await;
        assert!(res.is_err());
    }

    /// A temp tree laid out like the agent's own per-user state on this
    /// platform, with a stand-in config holding a marker "secret".
    fn agent_state_fixture(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "roomler-statefixture-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = if cfg!(target_os = "windows") {
            base.join("AppData")
                .join("Roaming")
                .join("roomler")
                .join("roomler")
        } else if cfg!(target_os = "macos") {
            base.join("Library")
                .join("Application Support")
                .join("live.roomler.roomler")
        } else {
            base.join(".config").join("roomler")
        };
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("config.toml"), AGENT_SECRET_MARKER).unwrap();
        (base, state)
    }

    const AGENT_SECRET_MARKER: &[u8] = b"agent_token = \"marker-7f3a9c\"\n";

    // The agent's own configuration and state — its enrollment token, its
    // SSH host key, its logs — is never a file-transfer source or target,
    // whatever the daemon's privilege: a controller with FILES acts on the
    // person's files, not the daemon's.
    #[test]
    fn the_agents_own_state_is_recognised_in_every_spelling() {
        use std::path::Path;
        #[cfg(target_os = "windows")]
        {
            for p in [
                r"C:\ProgramData\roomler\roomler\config.toml",
                r"\\?\C:\PROGRAMDATA\Roomler\ROOMLER\service-logs\x.log",
                r"C:\Users\u\AppData\Roaming\roomler\roomler\config\config.toml",
                r"C:\Users\u\AppData\Local\roomler\roomler\data\desktop\desktop.log",
            ] {
                assert!(agent_state_run(Path::new(p)).is_some(), "{p}");
            }
            for p in [
                r"C:\Users\u\Documents\roomler\notes.txt",
                r"C:\ProgramData\Microsoft\x",
                r"C:\Users\u\AppData\Local\Temp\roomler-x\y",
            ] {
                assert!(agent_state_run(Path::new(p)).is_none(), "{p}");
            }
        }
        #[cfg(unix)]
        {
            for p in [
                "/etc/roomler/config.toml",
                "/private/etc/roomler/config.toml",
                "/home/u/.config/roomler/config.toml",
                "/root/.local/share/roomler/logs/roomlerd.log",
            ] {
                assert!(agent_state_run(Path::new(p)).is_some(), "{p}");
            }
            for p in ["/home/u/Documents/roomler/x", "/tmp/roomler-x/y"] {
                assert!(agent_state_run(Path::new(p)).is_none(), "{p}");
            }
        }
    }

    #[tokio::test]
    async fn an_upload_never_lands_in_the_agents_own_state() {
        let (base, state) = agent_state_fixture("up");
        let res = resolve_dest_path_for(
            &state.to_string_lossy(),
            &files_dir::Writer::unprivileged(None),
            &files_dir::Rules::from_env(),
        )
        .await;
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            res.is_err(),
            "agent state is never an upload target: {res:?}"
        );
    }

    #[tokio::test]
    async fn the_agents_own_config_is_never_a_download() {
        let (base, state) = agent_state_fixture("down");
        std::fs::write(state.join("config.toml.prev"), AGENT_SECRET_MARKER).unwrap();
        std::fs::create_dir_all(state.join("logs")).unwrap();
        std::fs::write(state.join("logs").join("roomlerd.log"), b"a log line\n").unwrap();
        let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let config = FilesHandler::new()
            .begin_outgoing("o1".into(), &path(&state.join("config.toml")))
            .await
            .is_err();
        let prev = FilesHandler::new()
            .begin_outgoing("o2".into(), &path(&state.join("config.toml.prev")))
            .await
            .is_err();
        // Support still works: the drawer lists the agent's folders and a
        // log downloads — only the secret-bearing files are refused.
        let log_ok = FilesHandler::new()
            .begin_outgoing("o3".into(), &path(&state.join("logs").join("roomlerd.log")))
            .await
            .is_ok();
        let listing_ok = list_dir(&path(&state)).await.is_ok();
        let _ = std::fs::remove_dir_all(&base);
        assert!(config, "the agent's config is never a download");
        assert!(prev, "nor its .prev sibling");
        assert!(log_ok, "a log is still a download");
        assert!(listing_ok, "the agent's folders are still listed");
    }

    #[tokio::test]
    async fn a_folder_download_leaves_the_agents_secrets_out() {
        let (base, state) = agent_state_fixture("zip");
        std::fs::write(base.join("keep.txt"), b"a person's file").unwrap();
        std::fs::write(state.join("service.log"), b"a diagnostic line").unwrap();
        let (zip_writer, mut zip_reader) = tokio::io::duplex(64 * 1024);
        let cancel = tokio_util::sync::CancellationToken::new();
        let root = base.clone();
        let walk =
            tokio::spawn(
                async move { walk_and_zip(zip_writer, &root, &FsAs::Daemon, cancel).await },
            );
        let drain = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            zip_reader.read_to_end(&mut out).await.unwrap();
            out
        });
        walk.await.unwrap().expect("walk_and_zip");
        let zip = drain.await.unwrap();
        let _ = std::fs::remove_dir_all(&base);
        let has = |needle: &[u8]| zip.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"a person's file"), "the person's file is in the zip");
        assert!(has(b"a diagnostic line"), "the agent's logs still are");
        assert!(!has(AGENT_SECRET_MARKER), "the agent's config never is");
    }

    // FR-84 D4's placement rule binds a per-upload destination too: a
    // PRIVILEGED writer (SYSTEM / root) writes only inside the active
    // user's profile, where the drop folder itself must be.
    #[tokio::test]
    async fn a_privileged_upload_stays_inside_the_active_profile() {
        let base = std::env::temp_dir().join(format!(
            "roomler-destpriv-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let inside = home.join("Documents");
        let outside = base.join("elsewhere");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let home_s = std::fs::canonicalize(&home).unwrap();
        let home_s = files_dir::strip_verbatim(home_s)
            .to_string_lossy()
            .into_owned();
        let rules = files_dir::Rules::from_env();
        let system = files_dir::Writer {
            privileged: true,
            active_home: Some(home_s.clone()),
        };
        let nobody = files_dir::Writer {
            privileged: true,
            active_home: None,
        };
        let user = files_dir::Writer::unprivileged(Some(home_s));
        let ok_inside = resolve_dest_path_for(&inside.to_string_lossy(), &system, &rules).await;
        let outside_sys = resolve_dest_path_for(&outside.to_string_lossy(), &system, &rules).await;
        let no_one = resolve_dest_path_for(&inside.to_string_lossy(), &nobody, &rules).await;
        let outside_user = resolve_dest_path_for(&outside.to_string_lossy(), &user, &rules).await;
        let _ = std::fs::remove_dir_all(&base);
        assert!(ok_inside.is_ok(), "inside the profile: {ok_inside:?}");
        let e = outside_sys.expect_err("SYSTEM outside the profile is refused");
        assert!(format!("{e:#}").contains("profile"), "{e:#}");
        assert!(
            no_one.is_err(),
            "SYSTEM with nobody signed in writes nowhere it chose"
        );
        // An unprivileged writer is bounded by the OS's own rights, not here.
        assert!(outside_user.is_ok(), "{outside_user:?}");
    }

    #[tokio::test]
    async fn begin_with_dest_path_lands_in_dest() {
        let _lock = FILES_DIR_TEST_LOCK.lock().await;
        let _env = HOME_ENV_LOCK.lock().await;
        // End-to-end: a `begin` call with dest_path should produce a
        // path under the dest dir, not under Downloads.
        let base = std::env::temp_dir().join(format!(
            "roomler-dest-e2e-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dest = base.join("dest");
        tokio::fs::create_dir_all(&dest).await.unwrap();
        // Should dest_path resolution ever fall through, the default folder
        // is a scratch one, never the developer's real Downloads.
        let _drops = ScratchDrops::new("dest-e2e");

        let h = FilesHandler::new();
        let path = h
            .begin(
                "d1".into(),
                "out.bin".into(),
                4,
                None,
                Some(&dest.to_string_lossy()),
            )
            .await
            .expect("begin");
        // The destination path's parent should canonicalise-match
        // dest. (canonicalize() of dest may add long-path prefixes on
        // Windows.)
        let dest_canon = std::fs::canonicalize(&dest).unwrap();
        let parent = path.parent().unwrap();
        assert_eq!(parent, dest_canon, "lands under dest_path: {path:?}");

        // Cleanup.
        h.abort().await;
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// #3688 — FR-84 D4 on a Linux system install with nobody at seat0: a
    /// received file is the daemon's, but never in a folder a controller
    /// chose. That folder is refused before it is touched, and the upload
    /// lands in the default folder, as one with no folder chosen does. The
    /// daemon itself (root at the screen, or the switch off) still lands
    /// where it was asked (the control). The chosen folder is one THIS
    /// writer may use, so the control holds as root too.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nobody_at_the_screen_takes_no_folder_a_controller_chose() {
        let _lock = FILES_DIR_TEST_LOCK.lock().await;
        let _env = HOME_ENV_LOCK.lock().await;
        let drops = ScratchDrops::new("alone-default");
        std::fs::create_dir_all(&drops.0).unwrap();
        let default = std::fs::canonicalize(&drops.0).unwrap();
        let chosen = scratch_drop_dir("alone-chosen");
        let _ = std::fs::remove_dir_all(&chosen);
        std::fs::create_dir_all(&chosen).unwrap();
        let chosen = std::fs::canonicalize(&chosen).unwrap();
        let at = chosen.to_string_lossy().into_owned();
        let lands = |path: PathBuf| path.parent().map(std::path::Path::to_path_buf);
        let h = FilesHandler::new();

        let alone = h
            .begin_as(
                FsAs::DaemonAlone,
                "a3688-1".into(),
                "out.bin".into(),
                4,
                None,
                Some(&at),
            )
            .await
            .expect("the default folder takes it");
        h.abort().await;
        assert_eq!(
            lands(alone),
            Some(default.clone()),
            "a folder a controller chose, with nobody at the screen: the default folder"
        );
        let unasked = h
            .begin_as(
                FsAs::DaemonAlone,
                "a3688-2".into(),
                "out.bin".into(),
                4,
                None,
                None,
            )
            .await
            .expect("no folder chosen");
        h.abort().await;
        assert_eq!(
            lands(unasked),
            Some(default),
            "no folder chosen: the default folder"
        );
        let daemon = h
            .begin_as(
                FsAs::Daemon,
                "a3688-3".into(),
                "out.bin".into(),
                4,
                None,
                Some(&at),
            )
            .await
            .expect("the daemon's own");
        h.abort().await;
        assert_eq!(
            lands(daemon),
            Some(chosen.clone()),
            "the daemon lands where it was asked"
        );
        let _ = std::fs::remove_dir_all(&chosen);
    }

    #[test]
    fn parse_files_begin_with_rel_path() {
        // Folder-upload extension (file-DC v2.1). When the browser
        // sends a `rel_path`, the agent recreates the directory
        // structure under Downloads/.
        let m: FilesIncoming = serde_json::from_str(
            r#"{"t":"files:begin","id":"f1","name":"file.txt","size":42,"rel_path":"MyFolder/sub/file.txt"}"#,
        )
        .unwrap();
        match m {
            FilesIncoming::Begin {
                id,
                name,
                size,
                rel_path,
                ..
            } => {
                assert_eq!(id, "f1");
                assert_eq!(name, "file.txt");
                assert_eq!(size, 42);
                assert_eq!(rel_path.as_deref(), Some("MyFolder/sub/file.txt"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_begin_without_rel_path_back_compat() {
        // Old browsers (file-DC v1) don't send rel_path; deserialise
        // as None via #[serde(default)].
        let m: FilesIncoming =
            serde_json::from_str(r#"{"t":"files:begin","id":"f1","name":"x.bin","size":100}"#)
                .unwrap();
        match m {
            FilesIncoming::Begin { rel_path, .. } => {
                assert!(rel_path.is_none(), "old browsers omit rel_path");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn resolve_folder_upload_simple_two_component() {
        let dir = std::env::temp_dir().join("roomler-folder-resolve-test");
        std::fs::create_dir_all(&dir).ok();
        let p = resolve_folder_upload_path(&dir, "MyFolder/file.txt").expect("simple");
        let s = p.to_string_lossy();
        assert!(s.contains("MyFolder"), "kept root: {s}");
        assert!(s.ends_with("file.txt"), "kept leaf: {s}");
    }

    #[test]
    fn resolve_folder_upload_deep_nesting() {
        let dir = std::env::temp_dir().join("roomler-folder-deep-test");
        std::fs::create_dir_all(&dir).ok();
        let p = resolve_folder_upload_path(&dir, "a/b/c/d/file.txt").expect("deep");
        let s = p.to_string_lossy();
        assert!(s.contains("a"));
        assert!(s.contains("b"));
        assert!(s.ends_with("file.txt"));
    }

    #[test]
    fn resolve_folder_upload_rejects_traversal_components() {
        // Sanitisation strips `..` and `.` components. Result is
        // safe even on a malicious browser.
        let dir = std::env::temp_dir().join("roomler-folder-traversal-test");
        std::fs::create_dir_all(&dir).ok();
        let p = resolve_folder_upload_path(&dir, "MyFolder/../../etc/passwd").expect("traversal");
        let s = p.to_string_lossy();
        // The `..` segments are filtered; only `MyFolder` + `etc` +
        // `passwd` survive, and the result lives under `dir`.
        assert!(
            s.starts_with(&*dir.to_string_lossy()),
            "stays under dir: {s}"
        );
        assert!(!s.contains(".."), "no traversal sequences: {s}");
    }

    #[test]
    fn resolve_folder_upload_rejects_single_component() {
        let dir = std::env::temp_dir().join("roomler-folder-single-test");
        std::fs::create_dir_all(&dir).ok();
        // No "/" → can't be a folder upload; caller should fall back
        // to flat upload.
        assert!(resolve_folder_upload_path(&dir, "justafile.txt").is_none());
    }

    #[test]
    fn resolve_folder_upload_rejects_empty() {
        let dir = std::env::temp_dir().join("roomler-folder-empty-test");
        std::fs::create_dir_all(&dir).ok();
        assert!(resolve_folder_upload_path(&dir, "").is_none());
    }

    #[test]
    fn resolve_folder_upload_normalises_backslash() {
        // A buggy browser that sends Windows-style separators
        // shouldn't break; we normalise to forward slashes before
        // splitting.
        let dir = std::env::temp_dir().join("roomler-folder-backslash-test");
        std::fs::create_dir_all(&dir).ok();
        let p = resolve_folder_upload_path(&dir, "MyFolder\\sub\\file.txt").expect("backslash");
        let s = p.to_string_lossy();
        assert!(s.contains("MyFolder"));
        assert!(s.contains("sub"));
        assert!(s.ends_with("file.txt"));
    }

    #[test]
    fn resolve_folder_upload_caps_extreme_depth() {
        let dir = std::env::temp_dir().join("roomler-folder-deep-cap-test");
        std::fs::create_dir_all(&dir).ok();
        let mut deep = String::from("a");
        for i in 1..50 {
            deep.push_str(&format!("/b{i}"));
        }
        deep.push_str("/file.txt");
        // 50 levels exceeds the 32-component cap.
        assert!(resolve_folder_upload_path(&dir, &deep).is_none());
    }

    #[test]
    fn parse_files_get() {
        let m: FilesIncoming =
            serde_json::from_str(r#"{"t":"files:get","id":"d1","path":"C:\\report.pdf"}"#).unwrap();
        match m {
            FilesIncoming::Get { id, path } => {
                assert_eq!(id, "d1");
                assert_eq!(path, "C:\\report.pdf");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_get_folder() {
        let m: FilesIncoming = serde_json::from_str(
            r#"{"t":"files:get-folder","id":"f1","path":"C:\\Logs","format":"zip"}"#,
        )
        .unwrap();
        match m {
            FilesIncoming::GetFolder { id, path, format } => {
                assert_eq!(id, "f1");
                assert_eq!(path, "C:\\Logs");
                assert_eq!(format.as_deref(), Some("zip"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_cancel() {
        let m: FilesIncoming = serde_json::from_str(r#"{"t":"files:cancel","id":"d1"}"#).unwrap();
        match m {
            FilesIncoming::Cancel { id } => assert_eq!(id, "d1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_dir() {
        let m: FilesIncoming =
            serde_json::from_str(r#"{"t":"files:dir","req_id":"r1","path":""}"#).unwrap();
        match m {
            FilesIncoming::Dir { req_id, path } => {
                assert_eq!(req_id, "r1");
                assert_eq!(path, "");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- rc.19 file-DC v3 resume wire-format locks ----

    #[test]
    fn parse_files_resume_without_sha_prefix() {
        let m: FilesIncoming =
            serde_json::from_str(r#"{"t":"files:resume","id":"u9","offset":4194304}"#).unwrap();
        match m {
            FilesIncoming::Resume {
                id,
                offset,
                sha256_prefix,
            } => {
                assert_eq!(id, "u9");
                assert_eq!(offset, 4_194_304);
                assert_eq!(sha256_prefix, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_files_resume_with_sha_prefix_is_accepted() {
        // v1 agents accept-and-ignore sha256_prefix; v2 agents will
        // verify. Locking the schema now so v2 doesn't break compat.
        let m: FilesIncoming = serde_json::from_str(
            r#"{"t":"files:resume","id":"u9","offset":0,"sha256_prefix":"deadbeef"}"#,
        )
        .unwrap();
        match m {
            FilesIncoming::Resume {
                sha256_prefix: Some(s),
                ..
            } => assert_eq!(s, "deadbeef"),
            other => panic!("expected Resume with sha_prefix, got {other:?}"),
        }
    }

    #[test]
    fn serialize_files_resumed() {
        let s = serde_json::to_string(&FilesOutgoing::Resumed {
            id: "u9",
            accepted_offset: 4_194_304,
        })
        .unwrap();
        assert!(s.contains("\"t\":\"files:resumed\""), "got: {s}");
        assert!(s.contains("\"id\":\"u9\""), "got: {s}");
        assert!(s.contains("\"accepted_offset\":4194304"), "got: {s}");
    }

    #[test]
    fn serialize_files_resumed_with_zero_offset() {
        // accepted_offset == 0 is a normal response — agent has no
        // partial state for this id, browser re-pumps from byte 0.
        // Field must serialise explicitly, not be elided.
        let s = serde_json::to_string(&FilesOutgoing::Resumed {
            id: "u9",
            accepted_offset: 0,
        })
        .unwrap();
        assert!(s.contains("\"accepted_offset\":0"), "got: {s}");
    }

    #[test]
    fn active_transfers_counter_starts_at_zero() {
        // Sanity check: in a fresh test run no transfer guards are
        // alive. Other tests in this file may temporarily increment
        // this counter but must always release the guard before
        // returning — verify the baseline.
        let n = active_transfer_count();
        // We can't assert == 0 because tests run in parallel and a
        // sibling test may be mid-flight. We CAN assert < 1000 (a
        // sensible upper bound that would catch a runaway leak).
        assert!(n < 1000, "active_transfer_count={n} suspicious");
    }

    #[test]
    fn serialize_files_offer() {
        let s = serde_json::to_string(&FilesOutgoing::Offer {
            id: "d1",
            name: "report.pdf",
            size: Some(1024),
            mime: Some("application/pdf"),
        })
        .unwrap();
        assert!(s.contains("\"t\":\"files:offer\""), "got: {s}");
        assert!(s.contains("\"id\":\"d1\""));
        assert!(s.contains("\"size\":1024"));
        // size: None must serialize as null (browser checks size === null
        // for streaming offers like folder zips).
        let s2 = serde_json::to_string(&FilesOutgoing::Offer {
            id: "f1",
            name: "Logs.zip",
            size: None,
            mime: None,
        })
        .unwrap();
        assert!(s2.contains("\"size\":null"), "got: {s2}");
    }

    #[test]
    fn serialize_files_eof() {
        let s = serde_json::to_string(&FilesOutgoing::Eof {
            id: "d1",
            bytes: 1024,
        })
        .unwrap();
        assert_eq!(s, r#"{"t":"files:eof","id":"d1","bytes":1024}"#);
    }

    #[test]
    fn validate_outgoing_path_rejects_globalroot() {
        let res = validate_outgoing_path(r"\\?\GLOBALROOT\Device\HarddiskVolume2\foo");
        assert!(res.is_err(), "should reject globalroot prefix");
        assert!(res.unwrap_err().to_string().contains("kernel-namespace"));
    }

    #[test]
    fn validate_outgoing_path_rejects_registry_hive_dir() {
        let res = validate_outgoing_path(r"C:\Windows\System32\config\SAM");
        assert!(res.is_err(), "should reject registry hive container");
        assert!(res.unwrap_err().to_string().contains("registry-hive"));
    }

    #[test]
    fn validate_outgoing_path_rejects_empty() {
        assert!(validate_outgoing_path("").is_err());
    }

    #[test]
    fn validate_outgoing_path_rejects_oversized() {
        let long = "a".repeat(5000);
        assert!(validate_outgoing_path(&long).is_err());
    }

    #[tokio::test]
    async fn list_dir_against_tempdir() {
        // Build a temp dir with one file + one subdir; verify the
        // listing is sorted dirs-first and each entry has the right
        // is_dir flag.
        let base = std::env::temp_dir().join(format!(
            "roomler-list-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        tokio::fs::create_dir(base.join("subdir")).await.unwrap();
        tokio::fs::write(base.join("file.txt"), b"hello")
            .await
            .unwrap();

        let listing = list_dir(&base.to_string_lossy()).await.expect("list_dir");
        assert_eq!(listing.entries.len(), 2);
        assert!(listing.entries[0].is_dir, "subdir should sort first");
        assert_eq!(listing.entries[0].name, "subdir");
        assert!(!listing.entries[1].is_dir);
        assert_eq!(listing.entries[1].name, "file.txt");
        assert_eq!(listing.entries[1].size, Some(5));

        // Cleanup best-effort.
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn list_dir_roots_returns_drives_or_root() {
        // The empty path and the legacy `~` are the two spellings of "the
        // roots"; the viewer's `isRootsRequest` names the same two.
        for request in ["", "~"] {
            let listing = list_dir(request).await.expect("list_dir roots");
            assert!(
                !listing.entries.is_empty(),
                "roots view for {request:?} must have entries"
            );
            // Every entry must be a directory (drive root or "/").
            for e in &listing.entries {
                assert!(e.is_dir, "root entry should be a dir: {:?}", e);
            }
            // Parent must be None for the roots view.
            assert!(listing.parent.is_none());
            assert_eq!(listing.path, roots_label());
        }
    }

    /// #2870 — `/` is a directory, not the roots view. On a Unix host the
    /// roots view holds one entry, `/`; opening it must list the real root
    /// (`Users`, `usr`, ...), not the roots view again. Locks the arm the
    /// viewer's `isRootsRequest` mirrors.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn list_dir_slash_lists_the_real_root_not_the_roots_view() {
        let listing = list_dir("/").await.expect("list_dir /");
        assert_eq!(listing.path, "/");
        // `Path::parent()` of `/` is None — the drawer must not take that
        // for a roots view (it keys on the REQUEST, see `isRootsRequest`).
        assert!(listing.parent.is_none());
        assert!(!listing.entries.is_empty(), "/ is never empty");
        // Real children carry bare names; the roots view's one entry was `/`
        // itself — a directory never lists itself.
        for e in &listing.entries {
            assert_ne!(e.name, "/", "the roots view leaked into a listing of /");
            assert!(
                !e.name.contains('/'),
                "a child of / must be a bare name, got {:?}",
                e.name
            );
        }
        // The roots view is still reachable by its own spelling.
        let roots = list_dir("").await.expect("list_dir roots");
        assert_eq!(roots.entries.len(), 1);
        assert_eq!(roots.entries[0].name, "/");
    }

    /// #2870 on Windows — `/` is a path there too (the current drive's root,
    /// `\\?\C:\` once canonicalised), never the drives view: that is the empty
    /// path's, and stays so.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn list_dir_slash_is_a_directory_not_the_drives_view() {
        let listing = list_dir("/").await.expect("list_dir /");
        assert_ne!(listing.path, roots_label());
        for e in &listing.entries {
            assert!(
                !e.name.ends_with(":\\"),
                "a drive root in a listing of /: {:?}",
                e.name
            );
        }
        let roots = list_dir("").await.expect("list_dir roots");
        assert_eq!(roots.path, roots_label());
        assert!(roots.entries.iter().all(|e| e.name.ends_with(":\\")));
    }

    #[tokio::test]
    async fn begin_outgoing_stats_a_real_file() {
        let base = std::env::temp_dir().join(format!(
            "roomler-out-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let file_path = base.join("data.bin");
        tokio::fs::write(&file_path, vec![0u8; 4096]).await.unwrap();

        let h = FilesHandler::new();
        let offer = h
            .begin_outgoing("d1".into(), &file_path.to_string_lossy())
            .await
            .expect("begin_outgoing");
        assert_eq!(offer.id, "d1");
        assert_eq!(offer.size, Some(4096));
        assert_eq!(offer.name, "data.bin");

        // Cleanup
        h.finish_outgoing("d1").await;
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// #3669 — while it lives, `path` (a file or a folder) cannot be opened
    /// to be read or listed: on Unix its mode is 000; on Windows a handle
    /// shares it with no one. (A DACL would not do there: a shell that
    /// enabled its backup privilege reads through any.)
    struct Unopenable {
        #[cfg(unix)]
        path: PathBuf,
        #[cfg(unix)]
        mode: u32,
        #[cfg(windows)]
        _held: std::fs::File,
    }

    impl Unopenable {
        fn new(path: &std::path::Path) -> Self {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path).unwrap().permissions().mode();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
                Self {
                    path: path.to_path_buf(),
                    mode,
                }
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // FILE_FLAG_BACKUP_SEMANTICS: the flag that opens a folder,
                // and a file the same as without it.
                const OPEN_A_FOLDER_TOO: u32 = 0x0200_0000;
                let held = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .custom_flags(OPEN_A_FOLDER_TOO)
                    .open(path)
                    .unwrap();
                Self { _held: held }
            }
        }
    }

    #[cfg(unix)]
    impl Drop for Unopenable {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode));
        }
    }

    /// #3669 — on every install, a folder that cannot be listed is refused
    /// when its download begins, naming it, where it used to be offered and
    /// then sent as an empty zip that looked complete. A runner that lists it
    /// anyway (root through mode 000) skips.
    #[tokio::test]
    async fn an_unlistable_folder_is_refused_when_its_download_begins() {
        let temp = tempfile::tempdir().unwrap();
        let shut = temp.path().join("shut");
        std::fs::create_dir(&shut).unwrap();
        std::fs::write(shut.join("inside.txt"), b"x").unwrap();
        let held = Unopenable::new(&shut);
        if std::fs::read_dir(&shut).is_ok() {
            eprintln!(
                "SKIPPED an_unlistable_folder_is_refused_when_its_download_begins: this runner \
                 lists a folder it was refused (root reads through mode 000), so this test \
                 proved nothing here"
            );
            return;
        }
        let begun = FilesHandler::new()
            .begin_outgoing_zip("z3669-0".into(), &shut.to_string_lossy())
            .await;
        drop(held);
        let Err(refused) = begun else {
            panic!("a folder that cannot be listed was offered");
        };
        assert!(
            format!("{refused}").starts_with("reading "),
            "the reason names the folder: {refused:#}"
        );
        let io = refused
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .unwrap_or_else(|| panic!("an I/O refusal: {refused:#}"));
        #[cfg(unix)]
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied, "{io}");
        #[cfg(windows)]
        assert_eq!(io.raw_os_error(), Some(32), "a sharing violation: {io}");
    }

    /// #3669 — on every install, a download opens its file when it begins,
    /// before its offer: a file that will not open is refused at once,
    /// naming it, where it used to be offered and then fail in the pump. A
    /// runner that opens it anyway (root through mode 000) skips.
    #[tokio::test]
    async fn a_file_that_will_not_open_is_refused_before_its_offer() {
        let temp = tempfile::tempdir().unwrap();
        let shut = temp.path().join("shut.txt");
        std::fs::write(&shut, b"x").unwrap();
        let held = Unopenable::new(&shut);
        if std::fs::File::open(&shut).is_ok() {
            eprintln!(
                "SKIPPED a_file_that_will_not_open_is_refused_before_its_offer: this runner \
                 opens a file it was refused (root reads through mode 000), so this test \
                 proved nothing here"
            );
            return;
        }
        let begun = FilesHandler::new()
            .begin_outgoing("o3669-0".into(), &shut.to_string_lossy())
            .await;
        drop(held);
        let Err(refused) = begun else {
            panic!("a file that will not open was offered");
        };
        assert!(
            format!("{refused}").starts_with("opening "),
            "the reason names the file: {refused:#}"
        );
    }

    /// #3669 — on every install, a listing resolves its path inside its timed
    /// step, on a blocking thread, never on the async worker: with the one
    /// blocking thread busy, a path that does not exist answers "listing
    /// timed out", where it used to be resolved, and refused, on the worker
    /// itself. So a share that hangs while it resolves holds a blocking
    /// thread, not a worker.
    #[test]
    fn a_listing_resolves_its_path_off_the_async_worker() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("not-there").to_string_lossy().into_owned();
        runtime.block_on(async {
            let (release, held) = std::sync::mpsc::channel::<()>();
            let (busy, is_busy) = tokio::sync::oneshot::channel::<()>();
            let holder = tokio::task::spawn_blocking(move || {
                let _ = busy.send(());
                let _ = held.recv();
            });
            is_busy.await.unwrap();
            let listed = list_dir_as(&FsAs::Daemon, &missing).await;
            // Free the thread before any assertion, or a failure would hang
            // the runtime's drop instead of failing.
            release.send(()).unwrap();
            holder.await.unwrap();
            let Err(why) = listed else {
                panic!("a path that does not exist was listed");
            };
            assert_eq!(format!("{why}"), "listing timed out", "{why:#}");
        });
    }

    #[tokio::test]
    async fn walk_and_zip_round_trip() {
        // Build a 3-file folder, zip it, parse the zip back and
        // verify entry names + contents are preserved. Locks the
        // backbone of Phase 4 — sanitisation, recursion, and the
        // compat_write pipeline through async_zip.
        let base = std::env::temp_dir().join(format!(
            "roomler-zip-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let folder = base.join("payload");
        let subdir = folder.join("nested");
        tokio::fs::create_dir_all(&subdir).await.unwrap();
        tokio::fs::write(folder.join("a.txt"), b"first")
            .await
            .unwrap();
        tokio::fs::write(folder.join("b.bin"), vec![0u8; 8])
            .await
            .unwrap();
        tokio::fs::write(subdir.join("c.txt"), b"third")
            .await
            .unwrap();

        // Capture zip bytes via a duplex pipe + drain task — mirrors
        // the production topology (walk_and_zip writes to a duplex,
        // a separate task pumps the reader half to the DC).
        let (zip_writer, mut zip_reader) = tokio::io::duplex(64 * 1024);
        let cancel = tokio_util::sync::CancellationToken::new();
        let folder_clone = folder.clone();
        let cancel_clone = cancel.clone();
        let walk_handle = tokio::spawn(async move {
            crate::files::walk_and_zip(zip_writer, &folder_clone, &FsAs::Daemon, cancel_clone).await
        });
        let drain_handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            zip_reader.read_to_end(&mut out).await.unwrap();
            out
        });
        let _count = walk_handle.await.unwrap().expect("walk_and_zip");
        let sink = drain_handle.await.unwrap();

        assert!(!sink.is_empty(), "zip output should be non-empty");
        // The zip MUST end with the End-of-Central-Directory record
        // (PKzip signature 0x06054b50). If walk_and_zip exited
        // without `.close()` we'd see truncated output.
        let last4 = &sink[sink.len() - 22..sink.len() - 22 + 4];
        // EOCD signature is 0x06054b50 little-endian = 50 4B 05 06
        assert_eq!(
            last4,
            &[0x50, 0x4B, 0x05, 0x06],
            "zip should end with EOCD record"
        );

        // Cleanup.
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn concurrent_upload_and_download_do_not_contend() {
        let _lock = FILES_DIR_TEST_LOCK.lock().await;
        let _env = HOME_ENV_LOCK.lock().await;
        // Critique #2 in the plan said an in-flight upload should
        // not block a concurrent download (and vice versa). Locks
        // the invariant: incoming + outgoing each have their own
        // mutex; one can be active while the other progresses.
        let base = std::env::temp_dir().join(format!(
            "roomler-concurrent-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let download_src = base.join("source.bin");
        tokio::fs::write(&download_src, vec![0u8; 1024])
            .await
            .unwrap();

        // The upload lands in a scratch drop folder, never the dev's
        // real Downloads (see `ScratchDrops`).
        let _drops = ScratchDrops::new("concurrent");

        let h = FilesHandler::new();
        // Start an upload — populates `incoming`.
        let upload_path = h
            .begin("u1".into(), "upload.txt".into(), 5, None, None)
            .await
            .expect("begin upload");

        // Concurrently start a download — populates `outgoing`. If
        // the two states shared a mutex this would deadlock or block.
        let offer = h
            .begin_outgoing("d1".into(), &download_src.to_string_lossy())
            .await
            .expect("begin_outgoing");
        assert_eq!(offer.size, Some(1024));

        // Both should be reachable simultaneously.
        let active_id = h.current_id().await;
        assert_eq!(active_id.as_deref(), Some("u1"));
        // We can write a chunk to the upload while the download
        // state is still pinned.
        h.chunk(b"hello").await.expect("chunk");
        let (final_path, bytes) = h.end("u1").await.expect("end");
        assert_eq!(final_path, upload_path);
        assert_eq!(bytes, 5);
        // The whole point of the scratch folder: nothing a test uploads
        // ever lands in the real Downloads (red with a HOME/USERPROFILE
        // redirect on Windows, which the known folder ignores).
        if let Some(real) =
            directories::UserDirs::new().and_then(|u| u.download_dir().map(PathBuf::from))
        {
            assert!(
                !final_path.starts_with(&real),
                "a test upload landed in the real Downloads: {}",
                final_path.display()
            );
        }

        // The download is still active.
        let cancelled = h.cancel_outgoing("d1").await;
        assert!(cancelled);
        h.finish_outgoing("d1").await;

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// The session is over: a download's pump is told to stop (and an
    /// upload's state goes, as [`FilesHandler::abort`]). Red without the
    /// download half: after a drop its pump waits on a send buffer that
    /// never drains, holding the file, the channel and the transfer guard
    /// the updater defers for.
    #[tokio::test]
    async fn a_session_end_stops_the_download() {
        let base = std::env::temp_dir().join(format!(
            "roomler-session-end-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let file_path = base.join("x.bin");
        tokio::fs::write(&file_path, b"xx").await.unwrap();

        let h = FilesHandler::new();
        let offer = h
            .begin_outgoing("session-end-d1".into(), &file_path.to_string_lossy())
            .await
            .expect("begin_outgoing");
        assert!(!offer.cancel.is_cancelled());
        h.end_session().await;
        let told = offer.cancel.is_cancelled();
        // Twice is fine (the channel's own close may come after), and with
        // no upload in flight there is none left.
        h.end_session().await;
        let upload_left = h.current_id().await;
        // A message still in flight when the session ended begins nothing.
        // (The upload names this test's own folder, never the real
        // Downloads, should the refusal ever go.)
        h.finish_outgoing("session-end-d1").await;
        let late_download = h
            .begin_outgoing("session-end-d2".into(), &file_path.to_string_lossy())
            .await;
        let late_upload = h
            .begin(
                "session-end-u2".into(),
                "late.txt".into(),
                1,
                None,
                Some(&base.to_string_lossy()),
            )
            .await;

        let _ = tokio::fs::remove_dir_all(&base).await;
        assert!(told, "the download's pump was not told to stop");
        assert_eq!(upload_left, None);
        assert!(
            late_download.is_err(),
            "a download began after the session ended"
        );
        assert!(
            late_upload.is_err(),
            "an upload began after the session ended"
        );
    }

    #[tokio::test]
    async fn cancel_outgoing_flips_flag() {
        let base = std::env::temp_dir().join(format!(
            "roomler-cancel-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let file_path = base.join("x.bin");
        tokio::fs::write(&file_path, b"xx").await.unwrap();

        let h = FilesHandler::new();
        let offer = h
            .begin_outgoing("c1".into(), &file_path.to_string_lossy())
            .await
            .expect("begin_outgoing");
        assert!(!offer.cancel.is_cancelled());
        let cancelled = h.cancel_outgoing("c1").await;
        assert!(cancelled);
        assert!(offer.cancel.is_cancelled());

        // Mismatched id → false, no flag change on a fresh transfer.
        let cancelled_other = h.cancel_outgoing("nonexistent").await;
        assert!(!cancelled_other);

        h.finish_outgoing("c1").await;
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn round_trip_begin_chunk_end() {
        let _lock = FILES_DIR_TEST_LOCK.lock().await;
        let _env = HOME_ENV_LOCK.lock().await;
        // `begin` with no dest_path uses the drop folder: a scratch one
        // here, never the dev's real Downloads (see `ScratchDrops`).
        let _drops = ScratchDrops::new("round-trip");
        let h = FilesHandler::new();

        let path = h
            .begin("t1".into(), "hello.txt".into(), 5, None, None)
            .await
            .unwrap();
        h.chunk(b"hello").await.unwrap();
        let (final_path, bytes) = h.end("t1").await.unwrap();
        assert_eq!(final_path, path);
        assert_eq!(bytes, 5);
        let got = tokio::fs::read(&final_path).await.unwrap();
        assert_eq!(got, b"hello");
    }

    // ---- rc.19 P1 — staging dir + meta.json + sweep + sync_data ----
    //
    // These tests use `dest_path` to override the upload's target
    // directory because Windows `download_dir()` resolves via
    // `KNOWNFOLDERID_Downloads`, NOT via HOME/USERPROFILE (a test with no
    // `dest_path` uses `ScratchDrops` instead). `dest_path` is the same
    // operator-chosen-target path the browser sends when dropping into
    // the drawer's current dir.

    /// Create an isolated `dest_path`-style tempdir for staging tests, so
    /// the staging dir is fully under our control on every platform.
    async fn stage_tmpdir() -> PathBuf {
        let base = stage_dir_at(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
        // `create_dir`, never `_all`: a folder that exists already fails the
        // test here, instead of being shared with another one.
        tokio::fs::create_dir(&base).await.unwrap();
        base
    }

    /// #2956 — the folder [`stage_tmpdir`] makes at the clock reading
    /// `nanos`: unique by construction, because the clock alone is not. On a
    /// busy 4-core Windows runner, two tests spawned back to back read the
    /// same 100 ns tick (6 of 3000 such pairs, measured with the process
    /// pinned to 4 loaded CPUs). Two of these tests then shared one folder,
    /// and the first to finish removed the other's upload along with its own.
    fn stage_dir_at(nanos: i64) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "roomler-rc19-stage-{nanos}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn two_staging_folders_made_at_one_clock_reading_are_two_folders() {
        assert_ne!(stage_dir_at(42), stage_dir_at(42));
    }

    // rc.19 counter tests: ActiveTransferGuard wraps a process-
    // global AtomicUsize. Parallel tests share that global, so
    // before/after comparisons race fatally. The Drop contract is
    // structurally enforced by the type system — every transfer
    // struct owns a `_active_guard` field, every Drop on that field
    // decrements. No runtime test can cleanly assert "counter went
    // back down" without serialising every test that touches a
    // transfer. The updater-side `decide_defer` test in updater.rs
    // covers the gating logic; the counter mechanics fall out of
    // the type system.

    #[tokio::test]
    async fn begin_creates_staging_dir_with_meta_json() {
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-stage-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );

        h.begin(
            id.clone(),
            "report.pdf".into(),
            1024,
            None,
            Some(dest.to_str().unwrap()),
        )
        .await
        .expect("begin");

        // .roomler-partial/<id>/data + meta.json must exist before
        // any chunk arrives. `dest_path` is canonicalised by
        // `validate_outgoing_path` (Win adds `\\?\` UNC prefix), so
        // compare against the canonicalised root.
        let canonical_dest = tokio::fs::canonicalize(&dest).await.unwrap();
        let staging = canonical_dest.join(".roomler-partial").join(&id);
        assert!(
            staging.exists(),
            "staging dir missing: {}",
            staging.display()
        );
        let meta_path = staging.join("meta.json");
        let data_path = staging.join("data");
        assert!(meta_path.exists(), "meta.json missing");
        assert!(data_path.exists(), "data file missing");

        // meta.json round-trips.
        let bytes = tokio::fs::read(&meta_path).await.unwrap();
        let meta: PartialMeta = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(meta.protocol_version, 1);
        assert_eq!(meta.filename, "report.pdf");
        assert_eq!(meta.expected_size, 1024);
        assert_eq!(meta.dest_dir, canonical_dest);

        // Registry was populated. Lookup canonicalises to the same
        // path as what `begin()` stored. (Block-scoped, not drop():
        // clippy's await_holding_lock drop-tracking doesn't credit an
        // explicit drop and flags the guard against the await below.)
        {
            let g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                g.get(&id).map(|e| e.meta_path.as_path()),
                Some(meta_path.as_path())
            );
        }

        h.abort().await;
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn end_renames_staging_to_final_and_cleans_dir() {
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-end-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let reserved = h
            .begin(
                id.clone(),
                "hello.txt".into(),
                5,
                None,
                Some(dest.to_str().unwrap()),
            )
            .await
            .expect("begin");
        h.chunk(b"hello").await.expect("chunk");
        let (final_path, bytes) = h.end(&id).await.expect("end");
        assert_eq!(bytes, 5);
        // M4 fix: final path matches the begin-time reservation when
        // there's no operator-induced collision.
        assert_eq!(final_path, reserved);
        // File on disk has the payload.
        let got = tokio::fs::read(&final_path).await.unwrap();
        assert_eq!(got, b"hello");
        // Staging dir + per-id dir + parent .roomler-partial/ are
        // all gone.
        let staging_parent = dest.join(".roomler-partial");
        assert!(!staging_parent.join(&id).exists(), "per-id dir leaked");
        // Registry entry removed. (Block-scoped for await_holding_lock.)
        {
            let g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            assert!(g.get(&id).is_none(), "registry entry leaked");
        }
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn begin_rejects_existing_partial_dir_for_same_id() {
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-collide-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );

        // First begin → success, staging dir exists.
        h.begin(
            id.clone(),
            "a.txt".into(),
            4,
            None,
            Some(dest.to_str().unwrap()),
        )
        .await
        .expect("first begin");
        // Drop the in-flight state so the second begin doesn't
        // collide on the Mutex — only the on-disk dir should block.
        h.abort().await;

        // Second begin with the SAME id MUST fail — without this
        // guard, File::create(data) would truncate the partial and
        // destroy progress (B1 fix).
        let second = h
            .begin(
                id.clone(),
                "a.txt".into(),
                4,
                None,
                Some(dest.to_str().unwrap()),
            )
            .await;
        assert!(second.is_err(), "second begin should reject");
        let msg = second.unwrap_err().to_string();
        assert!(
            msg.contains("already has partial state") || msg.contains("send files:resume"),
            "unexpected error message: {msg}"
        );
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn chunk_syncs_data_at_fsync_threshold_boundary() {
        // rc.24 — was named "..._at_one_mib_boundary" when
        // FSYNC_THRESHOLD_BYTES was 1 MiB. After lowering to 256 KiB
        // (so sub-1-MiB uploads can survive a process kill), the
        // sync cadence changed but the contract is the same:
        // last_synced advances by exactly one threshold per chunk
        // that crosses the boundary.
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-sync-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        // Send 1.5× threshold total — one sync at threshold, none past.
        let size = (FSYNC_THRESHOLD_BYTES + FSYNC_THRESHOLD_BYTES / 2) as usize;
        h.begin(
            id.clone(),
            "big.bin".into(),
            size as u64,
            None,
            Some(dest.to_str().unwrap()),
        )
        .await
        .expect("begin");

        // 6 chunks × (threshold / 4) so we cross the threshold
        // exactly once on chunk 4, and stay short of the second
        // sync after chunk 6 = 1.5 × threshold.
        let chunk_size = (FSYNC_THRESHOLD_BYTES / 4) as usize;
        let chunk = vec![0xABu8; chunk_size];
        for _ in 0..6 {
            h.chunk(&chunk).await.expect("chunk");
        }

        // Verify the durable-at-sync-boundary contract that B2
        // actually guarantees: last_synced advanced past the
        // threshold exactly once. Chunks 1..4 = threshold → triggers
        // first sync (received == FSYNC_THRESHOLD_BYTES, delta ==
        // threshold). Chunks 5..6 = 1.5×threshold total → delta =
        // 0.5×threshold < threshold, no second sync.
        //
        // We do NOT assert on `tokio::fs::metadata(...).len()` here:
        // `tokio::fs::File` has an internal write buffer that
        // `write_all().await` populates without forcing an OS-level
        // write, and Linux + Windows tokio runtimes drain that
        // buffer at different cadences. The B2 production invariant
        // is "bytes 0..last_synced are durable", which IS testable
        // via the in-memory state; "bytes 0..received are visible
        // to stat()" depends on tokio's buffer state and isn't what
        // the resume protocol relies on. (Resume's B3 fix protects
        // the wire against any acked-bytes > disk-size mismatch.)
        let guard = h.incoming.lock().await;
        let state = guard.as_ref().expect("state");
        assert_eq!(state.last_synced, FSYNC_THRESHOLD_BYTES);
        assert_eq!(state.received, (6 * chunk_size) as u64);
        drop(guard);

        h.abort().await;
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn resume_round_trips_after_partial_upload() {
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-resume-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        // Total expected = 2 MiB. Send 1 MiB then abort to simulate
        // a DC drop. last_synced will be 1 MiB (one sync at the
        // boundary), disk_size == 1 MiB.
        h.begin(
            id.clone(),
            "big.bin".into(),
            2 * 1024 * 1024,
            None,
            Some(dest.to_str().unwrap()),
        )
        .await
        .expect("begin");
        let chunk = vec![0xCDu8; 256 * 1024];
        for _ in 0..4 {
            h.chunk(&chunk).await.expect("chunk");
        }
        // Simulated DC drop — state cleared, partial dir survives.
        h.abort().await;

        // Browser reconnects, sends files:resume claiming 1 MiB.
        let accepted = h
            .resume_incoming(&id, 1024 * 1024)
            .await
            .expect("resume_incoming");
        assert_eq!(accepted, 1024 * 1024);

        // Pump the remaining 1 MiB and finalise.
        for _ in 0..4 {
            h.chunk(&chunk).await.expect("post-resume chunk");
        }
        let (final_path, bytes) = h.end(&id).await.expect("end");
        assert_eq!(bytes, 2 * 1024 * 1024);
        let got_meta = tokio::fs::metadata(&final_path).await.unwrap();
        assert_eq!(got_meta.len(), 2 * 1024 * 1024);
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn resume_truncates_when_disk_size_below_requested() {
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-resume-truncate-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        // Begin a 2 MiB upload, send 512 KiB only (no sync because
        // 512 KiB < FSYNC_THRESHOLD_BYTES — that's irrelevant, we
        // care about disk size). Abort.
        h.begin(
            id.clone(),
            "small.bin".into(),
            2 * 1024 * 1024,
            None,
            Some(dest.to_str().unwrap()),
        )
        .await
        .expect("begin");
        let chunk = vec![0xEFu8; 256 * 1024];
        for _ in 0..2 {
            h.chunk(&chunk).await.expect("chunk");
        }
        h.abort().await;

        // Browser THINKS it sent 1 MiB but agent only has 512 KiB
        // on disk. Resume should accept 512 KiB (already aligned to
        // 256 KiB boundary; B3 mask is a no-op here).
        let accepted = h.resume_incoming(&id, 1024 * 1024).await.expect("resume");
        assert_eq!(accepted, 512 * 1024);

        // Browser pumps from 512 KiB.
        for _ in 0..6 {
            h.chunk(&chunk).await.expect("post-resume chunk");
        }
        let (_, bytes) = h.end(&id).await.expect("end");
        assert_eq!(bytes, 2 * 1024 * 1024);
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn resume_unknown_id_errors() {
        // A per-test unique id; the registry is never reset (that would race sibling tests).
        // An unknown id resolves the drop folder: hold its lock (`turn_the_folder_into_a_file`).
        let _lock = FILES_DIR_TEST_LOCK.lock().await;
        let id = format!(
            "rc19-resume-unknown-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let h = FilesHandler::new();
        let res = h.resume_incoming(&id, 0).await;
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("no partial state") || msg.contains("Downloads"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn end_recomputes_unique_path_when_collision_appears_mid_upload() {
        // M4 fix: re-run unique_path at rename time. If the operator
        // creates a file at the begin-time reservation BETWEEN begin
        // and end, the rename target shifts to the next unique slot.
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-m4-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let reserved = h
            .begin(
                id.clone(),
                "doc.txt".into(),
                4,
                None,
                Some(dest.to_str().unwrap()),
            )
            .await
            .expect("begin");
        // Operator creates a file at the begin-time reservation.
        tokio::fs::write(&reserved, b"existing").await.unwrap();
        h.chunk(b"abcd").await.expect("chunk");
        let (final_path, bytes) = h.end(&id).await.expect("end");
        assert_eq!(bytes, 4);
        // The actual final path must NOT be the begin reservation —
        // unique_path appended a `(1)` suffix.
        assert_ne!(
            final_path, reserved,
            "M4 fix: end() should pick a new unique path"
        );
        // Original file content preserved (operator's `existing`
        // bytes weren't overwritten).
        let original = tokio::fs::read(&reserved).await.unwrap();
        assert_eq!(original, b"existing");
        // New upload landed at the colliding-renamed path.
        let uploaded = tokio::fs::read(&final_path).await.unwrap();
        assert_eq!(uploaded, b"abcd");
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn cancel_incoming_removes_staging_dir_and_registry() {
        let dest = stage_tmpdir().await;
        let h = FilesHandler::new();
        let id = format!(
            "rc19-cancel-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        h.begin(
            id.clone(),
            "doomed.bin".into(),
            1024,
            None,
            Some(dest.to_str().unwrap()),
        )
        .await
        .expect("begin");
        let canonical_dest = tokio::fs::canonicalize(&dest).await.unwrap();
        let staging = canonical_dest.join(".roomler-partial").join(&id);
        assert!(staging.exists(), "staging dir absent pre-cancel");
        // Confirm registry has the id pre-cancel.
        {
            let g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            assert!(g.contains_key(&id));
        }
        let cleared = h.cancel_incoming(&id).await.expect("a plain id");
        assert!(cleared, "cancel_incoming should return true");
        assert!(!staging.exists(), "staging dir leaked after cancel");
        {
            let g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            assert!(!g.contains_key(&id), "registry leaked entry after cancel");
        }
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    #[tokio::test]
    async fn sweep_orphans_deletes_stale_and_keeps_fresh() {
        let dest = stage_tmpdir().await;
        // Build two fake partial dirs by hand: one 25h old, one 1h old.
        let root = dest.join(".roomler-partial");
        let now = chrono::Utc::now().timestamp();
        let mk = |id: &str, age_secs: i64| {
            let dir = root.join(id);
            let meta = PartialMeta {
                protocol_version: 1,
                filename: format!("{id}.bin"),
                expected_size: 100,
                dest_dir: dest.clone(),
                reserved_final_path: dest.join(format!("{id}.bin")),
                created_at_unix: now - age_secs,
                rel_path: None,
                person_sid: None,
            };
            (dir, meta)
        };
        let (stale_dir, stale_meta) = mk("rc19-sweep-stale", 25 * 3600);
        let (fresh_dir, fresh_meta) = mk("rc19-sweep-fresh", 3600);
        for (dir, meta) in [
            (stale_dir.clone(), stale_meta),
            (fresh_dir.clone(), fresh_meta),
        ] {
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(
                dir.join("meta.json"),
                serde_json::to_vec_pretty(&meta).unwrap(),
            )
            .await
            .unwrap();
            tokio::fs::write(dir.join("data"), b"").await.unwrap();
            // #3688 — its stager's own whatever the umask, as a begin makes
            // it: the sweep trusts no partial another account may write.
            #[cfg(unix)]
            for (p, mode) in [
                (dir.clone(), 0o700),
                (dir.join("meta.json"), 0o600),
                (dir.join("data"), 0o600),
            ] {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }

        let (kept, swept) = sweep_orphans_in(&dest).await;
        assert_eq!(swept, 1, "should sweep the 25h-old dir");
        assert_eq!(kept, 1, "should keep the 1h-old dir");
        assert!(!stale_dir.exists(), "stale dir should be removed");
        assert!(fresh_dir.exists(), "fresh dir should survive");
        // Fresh entry is in the registry. (Block-scoped for
        // await_holding_lock.)
        {
            let g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                g.contains_key("rc19-sweep-fresh"),
                "fresh id missing from registry: {:?}",
                g.keys().collect::<Vec<_>>()
            );
        }
        let _ = tokio::fs::remove_dir_all(&dest).await;
    }

    // ---- rc.22 — always-PROGRAMDATA staging strategy ----

    #[test]
    fn rc22_stage_in_programdata_is_false_under_cfg_test() {
        // cfg(test) forces the legacy per-dest layout so the existing
        // in-crate assertions on `<dest>/.roomler-partial/<id>` still
        // hold. Production on Windows flips this true; tests stay
        // false to keep isolation cheap.
        assert!(
            !stage_in_programdata(),
            "tests must use the legacy per-dest staging strategy"
        );
    }

    #[test]
    fn rc22_partial_dir_for_is_legacy_under_cfg_test() {
        // Pin the contract: in tests, partial_dir_for hands back the
        // rc.19 per-dest layout regardless of platform. The actual
        // staging-strategy-flag lock is `STAGE_IN_PROGRAMDATA`.
        let dest = std::path::Path::new("C:\\some\\dest");
        let got = partial_dir_for(dest, "abc-123").expect("a plain id");
        let want = dest.join(".roomler-partial").join("abc-123");
        assert_eq!(got, want);
    }

    #[test]
    fn rc22_is_cross_volume_error_recognises_kind() {
        // The portable case — Rust's ErrorKind::CrossesDevices (stable
        // 1.85+) directly maps to ERROR_NOT_SAME_DEVICE / EXDEV.
        let e = std::io::Error::from(std::io::ErrorKind::CrossesDevices);
        assert!(
            is_cross_volume_error(&e),
            "ErrorKind::CrossesDevices must be cross-volume"
        );
    }

    #[test]
    fn rc22_is_cross_volume_error_recognises_raw_os_error() {
        // Belt-and-suspenders for cases where ErrorKind didn't get
        // mapped (older runtimes / unusual error sources). Platform-
        // specific raw codes catch the gap.
        #[cfg(target_os = "windows")]
        {
            let e = std::io::Error::from_raw_os_error(17);
            assert!(
                is_cross_volume_error(&e),
                "Windows raw os error 17 must be cross-volume"
            );
        }
        #[cfg(not(target_os = "windows"))]
        {
            let e = std::io::Error::from_raw_os_error(18);
            assert!(
                is_cross_volume_error(&e),
                "Unix raw errno 18 must be cross-volume"
            );
        }
    }

    #[test]
    fn rc22_is_cross_volume_error_rejects_unrelated() {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(
            !is_cross_volume_error(&e),
            "NotFound must not be misclassified as cross-volume"
        );
        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(
            !is_cross_volume_error(&e),
            "PermissionDenied must not be misclassified as cross-volume"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rc22_staging_root_windows_under_programdata() {
        // Pin the path shape so an accidental edit (joining the wrong
        // component, e.g. uploads/ instead of staging/) fails this
        // test. Both halves matter — the prefix is the PROGRAMDATA
        // root that ESET is hypothesised to treat differently from
        // user Downloads; the suffix names what's there.
        let root = staging_root_windows();
        let s = root.to_string_lossy().to_lowercase();
        assert!(
            s.contains("programdata") || s.contains("c:\\"),
            "staging root must live under ProgramData: {}",
            root.display()
        );
        // S1b: either segment is valid — `roomler` on fresh/migrated
        // machines, legacy `roomler-agent` pre-migration.
        assert!(
            s.ends_with("roomler\\roomler-agent\\staging")
                || s.ends_with("roomler/roomler-agent/staging")
                || s.ends_with("roomler\\roomler\\staging")
                || s.ends_with("roomler/roomler/staging"),
            "staging root must end in roomler\\<segment>\\staging: {}",
            root.display()
        );
    }

    #[tokio::test]
    async fn rc22_sweep_orphans_dir_handles_flat_layout() {
        // The new sweep_orphans_dir helper scans a directory whose
        // CHILDREN are per-id staging dirs — used when PROGRAMDATA
        // staging is active and there's no `.roomler-partial` parent.
        // This test mirrors `sweep_orphans_deletes_stale_and_keeps_fresh`
        // but writes the partials directly under the root instead of
        // under root/.roomler-partial/.
        let root = std::env::temp_dir().join(format!(
            "roomler-rc22-flat-{}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            std::process::id()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let now = chrono::Utc::now().timestamp();
        let mk = |id: &str, age_secs: i64| {
            let dir = root.join(id);
            let meta = PartialMeta {
                protocol_version: 1,
                filename: format!("{id}.bin"),
                expected_size: 100,
                dest_dir: root.clone(),
                reserved_final_path: root.join(format!("{id}.bin")),
                created_at_unix: now - age_secs,
                rel_path: None,
                person_sid: None,
            };
            (dir, meta)
        };
        let (stale_dir, stale_meta) = mk("rc22-stale", 25 * 3600);
        let (fresh_dir, fresh_meta) = mk("rc22-fresh", 3600);
        for (dir, meta) in [
            (stale_dir.clone(), stale_meta),
            (fresh_dir.clone(), fresh_meta),
        ] {
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(
                dir.join("meta.json"),
                serde_json::to_vec_pretty(&meta).unwrap(),
            )
            .await
            .unwrap();
            tokio::fs::write(dir.join("data"), b"").await.unwrap();
            // #3688 — its stager's own whatever the umask, as a begin makes
            // it: the sweep trusts no partial another account may write.
            #[cfg(unix)]
            for (p, mode) in [
                (dir.clone(), 0o700),
                (dir.join("meta.json"), 0o600),
                (dir.join("data"), 0o600),
            ] {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        let (kept, swept) = sweep_orphans_root(&root).await;
        assert_eq!(swept, 1, "should sweep the 25h-old dir");
        assert_eq!(kept, 1, "should keep the 1h-old dir");
        assert!(!stale_dir.exists());
        assert!(fresh_dir.exists());
        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}
// RETIRED-NAME-ANCHOR-END

/// FR-97 P3d — a received file on a Linux system install, written as the
/// person at seat0. The identity half needs root: CI runs this module under
/// sudo with `ROOMLERD_TEST_REQUIRE_ROOT`, so it cannot pass by skipping.
/// `xdg_download_dir` is pure and runs everywhere, as does #3048's Windows
/// folder choice (`person_downloads_windows`).
#[cfg(test)]
mod seat_user_tests {
    use super::*;

    #[test]
    fn the_xdg_download_dir_is_taken_only_inside_home() {
        let home = std::path::Path::new("/home/ana");
        let pick = |text: &str| xdg_download_dir(text, home);
        assert_eq!(
            pick("XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n"),
            Some(home.join("Downloads"))
        );
        assert_eq!(
            pick(
                "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n"
            ),
            Some(home.join("Téléchargements"))
        );
        assert_eq!(
            pick("XDG_DOWNLOAD_DIR=\"/home/ana/dl\""),
            Some(home.join("dl"))
        );
        // The file is the person's to write, so it may name anything: only a
        // folder inside their home is taken.
        assert_eq!(pick("XDG_DOWNLOAD_DIR=\"/etc\""), None);
        assert_eq!(pick("XDG_DOWNLOAD_DIR=\"$HOME/../bob\""), None);
        assert_eq!(
            pick("XDG_DOWNLOAD_DIR=\"$HOME/\""),
            None,
            "the home itself means unset"
        );
        assert_eq!(pick(""), None);
    }

    /// #3048 — the Windows folder choice, on every OS: temp folders stand in
    /// for a local Downloads and for the share Folder Redirection moved it
    /// to. The person's Downloads known folder when it is a folder, else their
    /// profile; unchecked (the status card), the known folder as it is.
    #[test]
    fn person_downloads_windows_is_the_known_folder_else_the_profile() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        let local = profile.join("Downloads");
        // Redirected: outside the profile, while the local `Downloads` above
        // outlived the redirection.
        let redirected = temp.path().join("share").join("Downloads");
        let offline = temp.path().join("offline").join("Downloads");
        let a_file = temp.path().join("not-a-folder");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&redirected).unwrap();
        std::fs::write(&a_file, b"x").unwrap();
        let checked =
            |known: Option<&std::path::Path>| person_downloads_windows(known, &profile, true);

        assert_eq!(checked(Some(&local)), local, "local");
        assert_eq!(
            checked(Some(&redirected)),
            redirected,
            "redirected: the known folder, not the local one left behind"
        );
        assert_eq!(checked(None), profile, "none: the profile");
        assert_eq!(checked(Some(&offline)), profile, "unreachable: the profile");
        assert_eq!(checked(Some(&a_file)), profile, "not a folder: the profile");
        // Unchecked: the known folder as it is, even one that does not exist.
        assert_eq!(
            person_downloads_windows(Some(&offline), &profile, false),
            offline
        );
        assert_eq!(person_downloads_windows(None, &profile, false), profile);
    }

    #[cfg(target_os = "linux")]
    mod as_root {
        use super::*;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        fn root() -> bool {
            // SAFETY: reads our own credentials.
            let root = unsafe { libc::geteuid() } == 0;
            if !root && std::env::var_os("ROOMLERD_TEST_REQUIRE_ROOT").is_some() {
                panic!("ROOMLERD_TEST_REQUIRE_ROOT is set and this test is not running as root");
            }
            root
        }

        /// A home `nobody` owns, as a person's is theirs.
        pub(super) fn nobodys_home() -> (tempfile::TempDir, crate::fs_identity::Account) {
            let person =
                crate::fs_identity::Account::by_name("nobody").expect("the `nobody` account");
            let home = tempfile::tempdir().unwrap();
            std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::chown(home.path(), Some(person.uid), Some(person.gid)).unwrap();
            (home, person)
        }

        pub(super) fn as_person(
            person: &crate::fs_identity::Account,
            home: &std::path::Path,
        ) -> FsAs {
            FsAs::Person {
                account: person.clone(),
                home: home.to_path_buf(),
                caps: Caps::Kept,
            }
        }

        /// #3688 — the person as a read takes them: their capabilities
        /// emptied for each step.
        fn reader(person: &crate::fs_identity::Account, home: &std::path::Path) -> FsAs {
            FsAs::Person {
                account: person.clone(),
                home: home.to_path_buf(),
                caps: Caps::Empty,
            }
        }

        /// #3688 — the person at seat0, as the seat0 lookup answers them: a
        /// read through `FsAs::for_read()` then takes them by its own rule.
        fn at_the_screen(person: &crate::fs_identity::Account, home: &std::path::Path) -> Seat {
            Seat::Person {
                account: person.clone(),
                home: home.to_path_buf(),
            }
        }

        /// What the viewer is told when what a read opened as the person is
        /// not the folder asked for.
        fn not_the_folder_asked(path: &std::path::Path) -> String {
            format!(
                "{}: what opened is not the folder asked for, so file transfer does not list it",
                path.display()
            )
        }

        /// A file or folder only root may open, as root's own are.
        fn roots_only(path: &std::path::Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        /// Made the person's own.
        fn theirs(path: &std::path::Path, person: &crate::fs_identity::Account) {
            std::os::unix::fs::chown(path, Some(person.uid), Some(person.gid)).unwrap();
        }

        /// What the viewer is told when Linux refuses a read to the person.
        fn not_allowed(path: &std::path::Path) -> String {
            format!(
                "{}: Linux refused to open it for the person signed in at this computer's \
                 screen",
                path.display()
            )
        }

        /// What the viewer is told when a read as the person meets a kernel
        /// file system.
        fn on_a_kernel_fs(path: &std::path::Path, fs: &str) -> String {
            format!(
                "{}: {fs} is a kernel file system, which file transfer does not read for the \
                 person signed in at this computer's screen",
                path.display()
            )
        }

        /// #3688 — a download is opened as the person a read takes: through
        /// `begin_outgoing`, the entry point `files:get` calls, with the
        /// person stood in at seat0, so `FsAs::for_read()` decides. Their own
        /// file is offered as theirs and its bytes are the ones sent; a file
        /// only root may read is refused, saying why; the daemon itself is
        /// offered that file (the control). An entry point that read as the
        /// daemon would offer it to the person.
        #[tokio::test]
        async fn a_download_opens_the_file_as_the_person() {
            use tokio::io::AsyncReadExt;
            if !root() {
                return;
            }
            let (home, person) = nobodys_home();
            let mine = home.path().join("theirs.txt");
            let not_mine = home.path().join("roots-only.txt");
            std::fs::write(&mine, b"the person's file").unwrap();
            theirs(&mine, &person);
            std::fs::write(&not_mine, b"only root reads this file").unwrap();
            roots_only(&not_mine, 0o600);
            let at = |p: &std::path::Path| p.to_string_lossy().into_owned();
            let me = reader(&person, home.path());
            let h = FilesHandler::new();
            {
                let _as = stand_in::seat(at_the_screen(&person, home.path())).await;
                let offer = h
                    .begin_outgoing("r3688-1".into(), &at(&mine))
                    .await
                    .expect("the person's own file");
                assert_eq!(offer.who, me, "offered as theirs");
                assert_eq!(offer.size, Some(17));
                let mut sent = Vec::new();
                h.open_outgoing("r3688-1")
                    .await
                    .expect("the open file")
                    .read_to_end(&mut sent)
                    .await
                    .unwrap();
                assert_eq!(sent, b"the person's file");
                h.finish_outgoing("r3688-1").await;

                let Err(refused) = h.begin_outgoing("r3688-2".into(), &at(&not_mine)).await else {
                    panic!("files:get offered the person a file only root may read");
                };
                assert_eq!(format!("{refused}"), not_allowed(&not_mine));
            }
            h.begin_outgoing_as(FsAs::Daemon, "r3688-3".into(), &at(&not_mine))
                .await
                .expect("the daemon reads it");
            h.finish_outgoing("r3688-3").await;
        }

        /// #3688 — a folder download, from the task `files:get-folder` runs
        /// to its last message (`peer::outgoing_zip`), opens every entry as
        /// the person: their file is in the zip; a file only root may read,
        /// and one inside a folder only root may list, are left out; the zip
        /// completes. The daemon itself zips all three (the control).
        #[tokio::test]
        async fn a_folder_download_opens_every_entry_as_the_person() {
            use crate::peer::folder_download_as_the_person_tests::download;
            if !root() {
                return;
            }
            let (home, person) = nobodys_home();
            let root_dir = home.path().join("folder");
            let shut = root_dir.join("roots-only");
            std::fs::create_dir_all(&shut).unwrap();
            std::fs::write(root_dir.join("theirs.txt"), b"the person's file").unwrap();
            theirs(&root_dir.join("theirs.txt"), &person);
            std::fs::write(root_dir.join("not-theirs.txt"), b"only root reads this").unwrap();
            roots_only(&root_dir.join("not-theirs.txt"), 0o600);
            std::fs::write(shut.join("inside.txt"), b"only root lists this").unwrap();
            roots_only(&shut, 0o700);

            let mine = {
                let _as = stand_in::seat(at_the_screen(&person, home.path())).await;
                download(&root_dir).await
            };
            let said = mine.said();
            assert!(
                said.last()
                    .is_some_and(|m| m.contains(r#""t":"files:eof""#)),
                "the zip completes: {said:?}"
            );
            assert!(mine.has(b"the person's file"), "their file is in");
            assert!(
                !mine.has(b"only root reads this"),
                "a file they may not read is not: the walk read as the daemon"
            );
            assert!(
                !mine.has(b"only root lists this"),
                "nor one inside a folder they may not list"
            );
            let all = {
                let _as = stand_in::seat(Seat::Root).await;
                download(&root_dir).await
            };
            for needle in [
                &b"the person's file"[..],
                b"only root reads this",
                b"only root lists this",
            ] {
                assert!(all.has(needle), "the daemon zips {needle:?}");
            }
        }

        /// #3688 — the drawer lists a folder as the person, through
        /// `list_dir`, which `files:dir` calls: theirs lists; one only root
        /// may list is refused, saying why; the daemon itself lists it (the
        /// control).
        #[tokio::test]
        async fn a_listing_runs_as_the_person() {
            if !root() {
                return;
            }
            let (home, person) = nobodys_home();
            let mine = home.path().join("theirs");
            let shut = home.path().join("roots-only");
            std::fs::create_dir(&mine).unwrap();
            std::fs::write(mine.join("a.txt"), b"a").unwrap();
            theirs(&mine, &person);
            std::fs::create_dir(&shut).unwrap();
            std::fs::write(shut.join("b.txt"), b"b").unwrap();
            roots_only(&shut, 0o700);
            let at = |p: &std::path::Path| p.to_string_lossy().into_owned();
            {
                let _as = stand_in::seat(at_the_screen(&person, home.path())).await;
                let listed = list_dir(&at(&mine)).await.expect("their folder lists");
                let names: Vec<_> = listed.entries.iter().map(|e| e.name.as_str()).collect();
                assert_eq!(names, ["a.txt"]);
                let Err(refused) = list_dir(&at(&shut)).await else {
                    panic!("files:dir listed the person a folder only root may list");
                };
                assert_eq!(format!("{refused}"), not_allowed(&shut));
            }
            let control = list_dir_as(&FsAs::Daemon, &at(&shut))
                .await
                .expect("the daemon lists it");
            assert_eq!(control.entries.len(), 1);
        }

        /// This thread's credentials, as the kernel reports them.
        #[derive(Debug, PartialEq, Eq)]
        struct Creds {
            thread: std::thread::ThreadId,
            cap_eff: String,
            uids: String,
            gids: String,
            groups: String,
        }

        fn own_creds() -> Creds {
            let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
            let field = |key: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_default()
            };
            Creds {
                thread: std::thread::current().id(),
                cap_eff: field("CapEff:"),
                uids: field("Uid:"),
                gids: field("Gid:"),
                groups: field("Groups:"),
            }
        }

        /// #3688 — a read's step holds no capability while it runs as the
        /// person, and leaves the thread as it found it: capabilities,
        /// fsuid, fsgid and groups, after a step that returns and after one
        /// that panics. One blocking thread, so every step lands on it.
        #[test]
        fn a_read_step_empties_the_capabilities_and_leaves_nothing_behind() {
            if !root() {
                return;
            }
            let rt = tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (home, person) = nobodys_home();
                let me = reader(&person, home.path());
                let before = FsAs::Daemon.run(|| Ok(own_creds())).await.unwrap();
                assert_ne!(
                    before.cap_eff, "0000000000000000",
                    "root holds capabilities"
                );

                let inside = me.run(|| Ok(own_creds())).await.unwrap();
                assert_eq!(inside.thread, before.thread, "the same blocking thread");
                assert_eq!(
                    inside.cap_eff, "0000000000000000",
                    "no capability while the person reads"
                );
                let fsuid = inside.uids.split(' ').nth(3).map(str::to_string);
                assert_eq!(fsuid, Some(person.uid.to_string()), "the person's fsuid");
                assert_eq!(
                    FsAs::Daemon.run(|| Ok(own_creds())).await.unwrap(),
                    before,
                    "everything given back"
                );

                let panicked = me
                    .run(|| -> Result<()> { panic!("a read step that panics as the person") })
                    .await;
                assert!(panicked.is_err(), "the panic is the step's error");
                assert_eq!(
                    FsAs::Daemon.run(|| Ok(own_creds())).await.unwrap(),
                    before,
                    "everything given back after a panic"
                );
            });
        }

        /// #3688 — a read as the person never opens a file on a kernel file
        /// system, nor lists one, nor zips one: their files answer by the
        /// reader at `read()`, which the pump does as the daemon, or by the
        /// thread's euid, which stays root. The daemon itself reads them, as
        /// it always has (the control).
        #[tokio::test]
        async fn kernel_filesystems_are_not_read_as_the_person() {
            if !root() {
                return;
            }
            let (home, person) = nobodys_home();
            let me = reader(&person, home.path());
            let h = FilesHandler::new();
            let status = std::fs::canonicalize("/proc/self/status").unwrap();
            let Err(refused) = h
                .begin_outgoing_as(me.clone(), "k3688-1".into(), "/proc/self/status")
                .await
            else {
                panic!("a procfs file was offered to the person");
            };
            assert_eq!(format!("{refused}"), on_a_kernel_fs(&status, "procfs"));
            let online = std::path::Path::new("/sys/devices/system/cpu/online");
            if online.exists() {
                let online = std::fs::canonicalize(online).unwrap();
                let Err(refused) = h
                    .begin_outgoing_as(me.clone(), "k3688-2".into(), &online.to_string_lossy())
                    .await
                else {
                    panic!("a sysfs file was offered to the person");
                };
                assert_eq!(format!("{refused}"), on_a_kernel_fs(&online, "sysfs"));
            }
            for (dir, fs) in [("/proc", "procfs"), ("/sys", "sysfs")] {
                let Err(refused) = list_dir_as(&me, dir).await else {
                    panic!("{dir} was listed for the person");
                };
                assert_eq!(
                    format!("{refused}"),
                    on_a_kernel_fs(std::path::Path::new(dir), fs)
                );
            }
            let Err(refused) = h
                .begin_outgoing_zip_as(me.clone(), "k3688-3".into(), "/proc/sys/kernel")
                .await
            else {
                panic!("a procfs folder was offered to the person as a zip");
            };
            assert_eq!(
                format!("{refused}"),
                on_a_kernel_fs(std::path::Path::new("/proc/sys/kernel"), "procfs")
            );

            h.begin_outgoing_as(FsAs::Daemon, "k3688-4".into(), "/proc/self/status")
                .await
                .expect("the daemon reads it");
            h.finish_outgoing("k3688-4").await;
            let listed = list_dir_as(&FsAs::Daemon, "/proc")
                .await
                .expect("the daemon lists it");
            assert!(!listed.entries.is_empty());
        }

        /// The three folders a read lists, each one of the person's swapped
        /// for a link to `to` after it resolved and before it opens
        /// (`stand_in::swap_before_open`): the drawer's listing, a folder
        /// download's root, and a folder of its walk. Each is refused,
        /// saying `why` of the folder asked.
        async fn swapped_folders_are_refused(
            to: &std::path::Path,
            why: impl Fn(&std::path::Path) -> String,
        ) {
            let (home, person) = nobodys_home();
            let base = std::fs::canonicalize(home.path()).unwrap();
            let me = reader(&person, home.path());
            let swapped = |tag: &str| {
                let dir = base.join(tag);
                std::fs::create_dir(&dir).unwrap();
                theirs(&dir, &person);
                stand_in::swap_before_open(&dir, to);
                dir
            };

            let dir = swapped("listed");
            {
                let _as = stand_in::seat(at_the_screen(&person, home.path())).await;
                match list_dir(&dir.to_string_lossy()).await {
                    Ok(listed) => panic!(
                        "the drawer listed a link to {}: {:?}",
                        to.display(),
                        listed.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
                    ),
                    Err(refused) => assert_eq!(format!("{refused}"), why(&dir), "the drawer"),
                }
            }
            let dir = swapped("zipped");
            match FilesHandler::new()
                .begin_outgoing_zip_as(me.clone(), "s3688-1".into(), &dir.to_string_lossy())
                .await
            {
                Ok(_) => panic!("a folder download began on a link to {}", to.display()),
                Err(refused) => {
                    assert_eq!(format!("{refused}"), why(&dir), "a folder download's root")
                }
            }
            let dir = swapped("walked");
            let walked = {
                let dir = dir.clone();
                me.run(move || Ok(zip_folder_entries(&dir, 100, true)))
                    .await
                    .unwrap()
            };
            match walked {
                Ok(entries) => panic!(
                    "a walk listed a link to {}: {} entries",
                    to.display(),
                    entries.len()
                ),
                Err(e) => assert_eq!(format!("{e}"), why(&dir), "a folder of a walk"),
            }
        }

        /// #3688 — a folder a read lists is held to the folder asked, as a
        /// file is: here one swapped for a link to the daemon's own handle
        /// (`/proc/self/fd/N`) on a folder inside one only root may pass. A
        /// thread of the daemon's follows that link whatever the folders on
        /// its path say, so a listing by path would name what is in it. The
        /// same link met while the path resolves is refused by the folder
        /// only root may pass: the path is resolved as the person.
        #[tokio::test]
        async fn a_folder_swapped_for_a_link_to_the_daemons_own_handle_is_not_listed() {
            use std::os::fd::AsRawFd;
            if !root() {
                return;
            }
            let shut = tempfile::tempdir().unwrap();
            roots_only(shut.path(), 0o700);
            let inner = shut.path().join("inner");
            std::fs::create_dir(&inner).unwrap();
            std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(inner.join("a-name-only-root-may-see.txt"), b"x").unwrap();
            let held = std::fs::File::open(&inner).unwrap();
            let magic = PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd()));

            let (home, person) = nobodys_home();
            let link = std::fs::canonicalize(home.path()).unwrap().join("link");
            std::os::unix::fs::symlink(&magic, &link).unwrap();
            {
                let _as = stand_in::seat(at_the_screen(&person, home.path())).await;
                let Err(refused) = list_dir(&link.to_string_lossy()).await else {
                    panic!("a link to the daemon's handle was listed as it resolved");
                };
                assert_eq!(format!("{refused}"), not_allowed(&link));
            }

            swapped_folders_are_refused(&magic, not_the_folder_asked).await;
            drop(held);
        }

        /// #3688 — a folder swapped for a link to a kernel file system after
        /// it resolved is refused by its handle, as a file is: what opened is
        /// on procfs, whatever the path said before.
        #[tokio::test]
        async fn a_folder_swapped_for_a_link_to_a_kernel_file_system_is_not_listed() {
            if !root() {
                return;
            }
            swapped_folders_are_refused(std::path::Path::new("/proc/sys/kernel"), |dir| {
                on_a_kernel_fs(dir, "procfs")
            })
            .await;
        }

        /// #3688 — a folder download ends with `files:error`, never
        /// `files:eof`, once the person it began for has left seat0 (signed
        /// out, or the login screen or another person took it): before it
        /// lists each folder, and before it opens each file, it asks who is
        /// there. Here they are there for the first folder (its listing and
        /// its file) and gone by the second, so the first folder's file is
        /// in, the second's is not, and the viewer is told why, in the words
        /// a Windows walk uses.
        #[tokio::test]
        async fn a_person_who_leaves_the_screen_mid_folder_ends_it_with_an_error_not_eof() {
            use crate::peer::folder_download_as_the_person_tests::download;
            if !root() {
                return;
            }
            let (home, person) = nobodys_home();
            let root_dir = home.path().join("folder");
            let later = root_dir.join("later");
            std::fs::create_dir_all(&later).unwrap();
            std::fs::write(root_dir.join("first.txt"), b"the first folder's file").unwrap();
            std::fs::write(later.join("second.txt"), b"the second folder's file").unwrap();
            let got = {
                let seat = at_the_screen(&person, home.path());
                let _as = stand_in::seat_leaving_after(seat, 2).await;
                download(&root_dir).await
            };
            let said = got.said();
            assert!(
                got.has(b"the first folder's file"),
                "the file sent before they left"
            );
            assert!(!got.has(b"the second folder's file"), "none after");
            assert!(
                !said.iter().any(|m| m.contains("files:eof")),
                "never files:eof: {said:?}"
            );
            let last = said.last().expect("a last message");
            assert!(
                last.contains(r#""t":"files:error""#) && last.contains(NOT_THE_PERSON),
                "the walk's error reaches the viewer: {last}"
            );
        }

        /// #3688 — a person who leaves seat0 partway through a folder ends its
        /// download at the next file, not only at the next folder, as a Windows
        /// walk ends at its next step
        /// (`a_walk_whose_person_signs_out_ends_in_an_error_not_a_short_zip`):
        /// before it opens each file too, the walk asks who is there. Here one
        /// folder holds two files, and the person is there for its listing and
        /// its first file, and gone by its second. The file under way when
        /// they left still goes, none after it, and the viewer is told why
        /// with `files:error`, never `files:eof`.
        #[tokio::test]
        async fn a_person_who_leaves_the_screen_between_two_files_ends_it_at_the_next_file() {
            use crate::peer::folder_download_as_the_person_tests::download;
            if !root() {
                return;
            }
            let (home, person) = nobodys_home();
            let root_dir = home.path().join("flat");
            std::fs::create_dir(&root_dir).unwrap();
            let files: [&[u8]; 2] = [b"one of the folder's two files", b"the other of the two"];
            std::fs::write(root_dir.join("one.txt"), files[0]).unwrap();
            std::fs::write(root_dir.join("two.txt"), files[1]).unwrap();
            let got = {
                let seat = at_the_screen(&person, home.path());
                let _as = stand_in::seat_leaving_after(seat, 2).await;
                download(&root_dir).await
            };
            let said = got.said();
            assert!(
                !said.iter().any(|m| m.contains("files:eof")),
                "never files:eof: {said:?}"
            );
            let last = said.last().expect("a last message");
            assert!(
                last.contains(r#""t":"files:error""#) && last.contains(NOT_THE_PERSON),
                "the walk's error reaches the viewer: {last}"
            );
            // A folder lists in no set order, so either file may be first.
            let sent = files.iter().filter(|file| got.has(file)).count();
            assert_eq!(sent, 1, "the file under way went, and none after it");
        }

        /// #3688 — with nobody at seat0 a received file is the daemon's, in
        /// the default folder, root's own `~/Downloads` (a stand-in here): it is
        /// born root's and 0600, its partial and the partial's notes
        /// included, so no other account reads any of it at any point.
        #[tokio::test]
        async fn nobody_at_the_screen_receives_a_file_only_root_may_read() {
            if !root() {
                return;
            }
            // No folder chosen: the drop folder, which is process-wide and
            // which a test beside this one sets.
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            // Root's own home, stood in: never the real `/root/Downloads`.
            let _home = super::own_staging::OwnHome::new(false, true);
            let lands = download_dir_for(&FsAs::DaemonAlone).await.unwrap();
            let only_root = |path: &std::path::Path| {
                let md = std::fs::metadata(path).unwrap();
                (md.mode() & 0o777, md.uid(), md.gid())
            };
            let h = FilesHandler::new();
            let id = format!("alone-0600-{}", std::process::id());
            h.begin_as(
                FsAs::DaemonAlone,
                id.clone(),
                format!("{id}.bin"),
                5,
                None,
                None,
            )
            .await
            .expect("begin with nobody at the screen");
            let partial = lands.join(".roomler-partial").join(&id);
            assert_eq!(
                only_root(&partial.join("data")),
                (0o600, 0, 0),
                "the partial, from its first byte"
            );
            assert_eq!(
                only_root(&partial.join("meta.json")),
                (0o600, 0, 0),
                "the partial's notes"
            );
            h.chunk(b"hello").await.expect("chunk");
            let (placed, bytes) = h.end(&id).await.expect("end");
            let mode = only_root(&placed);
            let _ = std::fs::remove_file(&placed);
            assert_eq!(bytes, 5);
            assert_eq!(placed.parent(), Some(lands.as_path()), "the default folder");
            assert_eq!(mode, (0o600, 0, 0), "the file");
        }

        /// #3711 — at each entry point that asks who a received file is
        /// written as (`files:begin`, `files:resume`, the startup sweep), the
        /// seat0 lookup runs off the async worker and is waited for
        /// [`SEAT_LOOKUP_WAIT`] at most. One that gives no answer in that time
        /// refuses the upload and the resume in the read's words, and the
        /// sweep sweeps nothing: nothing is touched, root's own default folder
        /// included. A current-thread runtime has one worker, this test's own
        /// thread.
        #[tokio::test(flavor = "current_thread")]
        async fn a_lookup_past_its_time_refuses_a_received_file_at_every_entry_point() {
            const CANNOT_TELL: &str =
                "Could not tell who is signed in at this computer's screen. Try again.";
            if !root() {
                return;
            }
            // The drop folder, which is process-wide and which a test beside
            // this one sets.
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            // Root's own home, stood in: never the real `/root/Downloads`.
            let home = super::own_staging::OwnHome::new(false, true);
            let worker = std::thread::current().id();
            let asked_on = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let _asked = stand_in::seat_asked({
                let asked_on = asked_on.clone();
                move || {
                    asked_on.lock().unwrap().push(std::thread::current().id());
                    std::thread::sleep(SEAT_LOOKUP_WAIT * 2);
                    Seat::Nobody("seat0 has no active session".into())
                }
            })
            .await;
            let off_the_worker = |what: &str| {
                let asked = asked_on.lock().unwrap().clone();
                assert!(
                    !asked.is_empty() && asked.iter().all(|on| *on != worker),
                    "{what}: a lookup ran on the async worker ({asked:?}, worker {worker:?})"
                );
            };
            let bound = SEAT_LOOKUP_WAIT + std::time::Duration::from_secs(1);
            let h = FilesHandler::new();
            let pid = std::process::id();

            let began = std::time::Instant::now();
            let begun = h
                .begin(format!("late-b-{pid}"), "late.txt".into(), 1, None, None)
                .await;
            let waited = began.elapsed();
            off_the_worker("files:begin");
            assert!(waited < bound, "files:begin waited {waited:?}");
            assert_eq!(
                format!("{}", begun.expect_err("files:begin with no answer in time")),
                CANNOT_TELL
            );

            let began = std::time::Instant::now();
            let resumed = h.resume_incoming(&format!("late-r-{pid}"), 0).await;
            let waited = began.elapsed();
            off_the_worker("files:resume");
            assert!(waited < bound, "files:resume waited {waited:?}");
            assert_eq!(
                format!(
                    "{}",
                    resumed.expect_err("files:resume with no answer in time")
                ),
                CANNOT_TELL
            );

            let began = std::time::Instant::now();
            let swept = sweep_orphans().await;
            let waited = began.elapsed();
            off_the_worker("the startup sweep");
            assert!(waited < bound, "the startup sweep waited {waited:?}");
            assert_eq!(swept, (0, 0), "nothing swept");

            assert_eq!(asked_on.lock().unwrap().len(), 3, "each asked once");
            assert!(
                !home.downloads().exists(),
                "nothing was made in root's own default folder"
            );
        }

        /// #3711 — the control: a lookup that answers in time still gives a
        /// received file to the person at seat0 through `files:begin` itself
        /// (FR-97 P3d), and it too runs off the async worker.
        #[tokio::test(flavor = "current_thread")]
        async fn a_lookup_in_time_still_receives_the_file_as_the_person() {
            if !root() {
                return;
            }
            // No folder chosen: the drop folder, which is process-wide and
            // which a test beside this one sets.
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let (home, person) = nobodys_home();
            let worker = std::thread::current().id();
            let asked_on = std::sync::Arc::new(std::sync::Mutex::new(None));
            let seat = at_the_screen(&person, home.path());
            let _asked = stand_in::seat_asked({
                let asked_on = asked_on.clone();
                move || {
                    *asked_on.lock().unwrap() = Some(std::thread::current().id());
                    seat.clone()
                }
            })
            .await;
            let h = FilesHandler::new();
            let id = format!("in-time-{}", std::process::id());
            h.begin(id.clone(), "in-time.txt".into(), 5, None, None)
                .await
                .expect("files:begin as the person");
            h.chunk(b"hello").await.expect("chunk");
            let (path, bytes) = h.end(&id).await.expect("end");
            let md = std::fs::metadata(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            assert_eq!(bytes, 5);
            assert!(path.starts_with(home.path()), "in their home: {path:?}");
            assert_eq!(
                (md.uid(), md.gid()),
                (person.uid, person.gid),
                "born the person's"
            );
            let asked_on = asked_on.lock().unwrap().expect("the lookup was asked");
            assert_ne!(asked_on, worker, "the lookup ran on the async worker");
        }

        /// #3769 — a daemon's LocalAPI state over a config file of its own,
        /// as a `ConfigSet` and a `ConfigGet` reach it, and that file.
        fn a_daemon_with_its_config(
            dir: &std::path::Path,
        ) -> (
            crate::localapi_state::DaemonState,
            std::path::PathBuf,
            tokio::sync::watch::Sender<tunnel_core::localapi::OverlayView>,
        ) {
            let path = dir.join("config.toml");
            crate::config::save(&path, &crate::config::test_fixture()).unwrap();
            let (overlay, view) = tokio::sync::watch::channel(Default::default());
            let st = crate::localapi_state::DaemonState::new(
                "aid".into(),
                "host".into(),
                tunnel_core::localapi::DaemonMode::Service,
                None,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                view,
                crate::consent::ConsentBroker::new(
                    crate::consent::Mode::AutoGrant,
                    dir.join("consent"),
                )
                .unwrap(),
                None,
                crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            )
            .with_config_persist(
                path.clone(),
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
            );
            (st, path, overlay)
        }

        /// #3769 — the drop folder unset again, whatever a test left.
        struct NoFilesDirAfter;

        impl Drop for NoFilesDirAfter {
            fn drop(&mut self) {
                set_files_dir(None);
            }
        }

        /// #3769 — a `ConfigSet` of the files folder asks who is at seat0 as
        /// a received file does, and never under the config write lock. A
        /// lookup that gives no answer in its wait refuses the set in the
        /// read's words within that wait, with nothing saved and no folder
        /// made, and a `ConfigGet` asked meanwhile answers at once, not after
        /// the lookup. A current-thread runtime has one worker, this test's
        /// own thread.
        #[tokio::test(flavor = "current_thread")]
        async fn a_files_folder_set_whose_lookup_is_late_holds_no_config_read() {
            use tunnel_core::localapi::{LocalApiState, Response};
            const CANNOT_TELL: &str =
                "Could not tell who is signed in at this computer's screen. Try again.";
            if !root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            set_files_dir(None);
            let _after = NoFilesDirAfter;
            let dir = tempfile::tempdir().unwrap();
            let (st, path, _overlay) = a_daemon_with_its_config(dir.path());
            let (home, person) = nobodys_home();
            let folder = home.path().join("drops");
            let spelled = folder.to_string_lossy().into_owned();
            let seat = at_the_screen(&person, home.path());
            let _asked = stand_in::seat_asked(move || {
                std::thread::sleep(SEAT_LOOKUP_WAIT * 2);
                seat.clone()
            })
            .await;
            let bound = SEAT_LOOKUP_WAIT + std::time::Duration::from_secs(1);
            let began = std::time::Instant::now();
            let (set, (read, read_waited)) =
                tokio::join!(st.config_set("files_dir", Some(&spelled)), async {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let asked = std::time::Instant::now();
                    let read = st.config_entries().await;
                    (read, asked.elapsed())
                });
            let set_waited = began.elapsed();
            assert!(
                read_waited < std::time::Duration::from_secs(1),
                "the ConfigGet waited {read_waited:?}, behind the lookup"
            );
            assert!(matches!(read, Response::ConfigEntries(_)), "{read:?}");
            assert!(set_waited < bound, "the ConfigSet waited {set_waited:?}");
            match set {
                Response::Error { message } => assert_eq!(message, CANNOT_TELL),
                other => panic!("expected the refusal, got {other:?}"),
            }
            assert_eq!(
                crate::config::load(&path).unwrap().files_dir,
                None,
                "nothing saved"
            );
            assert_eq!(configured_files_dir(), None, "nothing re-seeded");
            assert!(!folder.exists(), "no folder made");
        }

        /// #3769 — the control: a lookup that answers in time leaves the set
        /// checked for and as the person at seat0, as before (FR-97 P3d). The
        /// folder is saved, live, and made as them, and the `ConfigGet` asked
        /// meanwhile answers at once too.
        #[tokio::test(flavor = "current_thread")]
        async fn a_files_folder_set_whose_lookup_answers_in_time_is_checked_as_the_person() {
            use tunnel_core::localapi::{LocalApiState, Response};
            if !root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            set_files_dir(None);
            let _after = NoFilesDirAfter;
            let dir = tempfile::tempdir().unwrap();
            let (st, path, _overlay) = a_daemon_with_its_config(dir.path());
            let (home, person) = nobodys_home();
            let folder = home.path().join("drops");
            let spelled = folder.to_string_lossy().into_owned();
            let seat = at_the_screen(&person, home.path());
            let _asked = stand_in::seat_asked(move || seat.clone()).await;
            let (set, (read, read_waited)) =
                tokio::join!(st.config_set("files_dir", Some(&spelled)), async {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let asked = std::time::Instant::now();
                    let read = st.config_entries().await;
                    (read, asked.elapsed())
                });
            assert!(
                read_waited < std::time::Duration::from_secs(1),
                "the ConfigGet waited {read_waited:?}"
            );
            assert!(matches!(read, Response::ConfigEntries(_)), "{read:?}");
            match set {
                Response::ConfigUpdated { entry } => {
                    assert_eq!(entry.value.as_deref(), Some(spelled.as_str()))
                }
                other => panic!("expected the folder saved, got {other:?}"),
            }
            assert_eq!(
                crate::config::load(&path).unwrap().files_dir.as_deref(),
                Some(spelled.as_str()),
                "saved"
            );
            assert_eq!(
                configured_files_dir().as_deref(),
                Some(spelled.as_str()),
                "live for the next drop"
            );
            let md = std::fs::metadata(&folder).expect("made at set time");
            assert_eq!(
                (md.uid(), md.gid()),
                (person.uid, person.gid),
                "made as the person"
            );
        }

        /// #3769 — a lookup that fails outright keeps today's answer, at once:
        /// the daemon writes alone (`FsAs::DaemonAlone`), with no one's
        /// profile to place the folder in, so the placement rule refuses it in
        /// its own words, never "Try again".
        #[tokio::test(flavor = "current_thread")]
        async fn a_files_folder_set_whose_lookup_failed_keeps_todays_refusal() {
            use tunnel_core::localapi::{LocalApiState, Response};
            if !root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            set_files_dir(None);
            let _after = NoFilesDirAfter;
            let dir = tempfile::tempdir().unwrap();
            let (st, path, _overlay) = a_daemon_with_its_config(dir.path());
            let (home, _person) = nobodys_home();
            let spelled = home.path().join("drops").to_string_lossy().into_owned();
            let _asked = stand_in::seat_asked(|| {
                Seat::CannotTell(
                    "loginctl show-seat seat0 failed: Unit dbus-org.freedesktop.login1.service \
                     is masked."
                        .into(),
                )
            })
            .await;
            let began = std::time::Instant::now();
            match st.config_set("files_dir", Some(&spelled)).await {
                Response::Error { message } => assert!(
                    message.contains("no active user profile is known right now"),
                    "{message}"
                ),
                other => panic!("expected the placement rule's refusal, got {other:?}"),
            }
            let waited = began.elapsed();
            assert!(waited < SEAT_LOOKUP_WAIT, "waited {waited:?}");
            assert_eq!(crate::config::load(&path).unwrap().files_dir, None);
        }

        /// The file is born the person's, in their folder, and nothing of the
        /// staging is left. 0.4.143 wrote it `root:root` into `/tmp` (FR-97
        /// §8, P3d's red).
        #[tokio::test]
        async fn a_received_file_is_born_the_persons() {
            if !root() {
                return;
            }
            // No folder chosen: the drop folder, which is process-wide and
            // which a test beside this one sets.
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let (home, person) = nobodys_home();
            let h = FilesHandler::new();
            let id = format!("p3d-born-{}", std::process::id());
            h.begin_as(
                as_person(&person, home.path()),
                id.clone(),
                "probe.txt".into(),
                5,
                None,
                None,
            )
            .await
            .expect("begin as the person");
            h.chunk(b"hello").await.expect("chunk");
            let (path, bytes) = h.end(&id).await.expect("end");
            assert_eq!(bytes, 5);
            assert_eq!(
                path,
                home.path().join("probe.txt"),
                "no Downloads: their home"
            );
            let md = std::fs::metadata(&path).unwrap();
            assert_eq!(
                (md.uid(), md.gid()),
                (person.uid, person.gid),
                "born the person's"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"hello");
            assert!(
                !home.path().join(".roomler-partial").exists(),
                "the staging is gone"
            );
        }

        /// A link the person planted where their file goes is followed with
        /// THEIR rights: into a folder only root may write, the begin is
        /// refused and nothing lands there. The control: root's own rights go
        /// straight through the same link. #3688 — the link is their
        /// Downloads itself; one planted as the staging folder is not
        /// followed at all (rule 2).
        #[tokio::test]
        async fn a_link_in_the_persons_folder_gets_only_the_persons_rights() {
            if !root() {
                return;
            }
            // No folder chosen: the drop folder, which is process-wide and
            // which a test beside this one sets.
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let (home, person) = nobodys_home();
            let roots_only = tempfile::tempdir().unwrap();
            std::fs::set_permissions(roots_only.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let plant = |link: &std::path::Path| {
                std::os::unix::fs::symlink(roots_only.path(), link).unwrap();
                std::os::unix::fs::lchown(link, Some(person.uid), Some(person.gid)).unwrap();
            };
            let h = FilesHandler::new();

            // #3688 — planted as the staging folder: refused, never followed.
            let staging = home.path().join(".roomler-partial");
            plant(&staging);
            let refused = h
                .begin_as(
                    as_person(&person, home.path()),
                    format!("p3d-staging-link-{}", std::process::id()),
                    "probe.txt".into(),
                    5,
                    None,
                    None,
                )
                .await
                .expect_err("a staging folder that is a link is refused");
            assert_eq!(
                refused.to_string(),
                format!(
                    "{} is not a folder, so file transfer does not stage there",
                    staging.display()
                )
            );
            std::fs::remove_file(&staging).unwrap();

            let link = home.path().join("Downloads");
            plant(&link);
            // The control: as root, the planted link leads straight in.
            let probe = link.join("root-probe");
            std::fs::create_dir(&probe).expect("root creates through the planted link");
            std::fs::remove_dir(&probe).unwrap();

            let id = format!("p3d-link-{}", std::process::id());
            let err = h
                .begin_as(
                    as_person(&person, home.path()),
                    id,
                    "probe.txt".into(),
                    5,
                    None,
                    None,
                )
                .await
                .expect_err("the person cannot write where the link leads");
            assert!(format!("{err:#}").contains("Permission denied"), "{err:#}");
            assert_eq!(
                std::fs::read_dir(roots_only.path()).unwrap().count(),
                0,
                "nothing landed in root's folder"
            );
        }

        /// Their Downloads when they have one, and the folder their own
        /// `user-dirs.dirs` names when it names one.
        #[tokio::test]
        async fn the_persons_downloads_is_theirs_to_name() {
            if !root() {
                return;
            }
            // The drop folder is process-wide, and a test beside this one
            // sets it.
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let (home, person) = nobodys_home();
            let theirs = |p: &std::path::Path| {
                std::fs::create_dir_all(p).unwrap();
                std::os::unix::fs::chown(p, Some(person.uid), Some(person.gid)).unwrap();
            };
            let who = as_person(&person, home.path());
            assert_eq!(
                download_dir_for(&who).await.unwrap(),
                home.path().to_path_buf()
            );
            theirs(&home.path().join("Downloads"));
            assert_eq!(
                download_dir_for(&who).await.unwrap(),
                home.path().join("Downloads")
            );
            theirs(&home.path().join(".config"));
            theirs(&home.path().join("Fichiers"));
            std::fs::write(
                home.path().join(".config/user-dirs.dirs"),
                "XDG_DOWNLOAD_DIR=\"$HOME/Fichiers\"\n",
            )
            .unwrap();
            assert_eq!(
                download_dir_for(&who).await.unwrap(),
                home.path().join("Fichiers")
            );
        }
    }

    /// #3669, #3688 — a read takes whom a received file takes. On a Linux
    /// system install that is the person at seat0, whose read steps hold no
    /// capability; with nobody there a read is refused and a received file
    /// is the daemon's, never where a controller chose; root itself at the
    /// screen, a user install, and the switch off are the daemon, both
    /// ways. On Windows a test process is never LocalSystem, so a read is the
    /// daemon's, as a write is; macOS reads as the daemon.
    #[tokio::test]
    async fn reads_take_whom_writes_take() {
        #[cfg(target_os = "linux")]
        {
            let ana = crate::fs_identity::Account {
                uid: 1000,
                gid: 1000,
                groups: vec![1000, 27],
                name: "ana".into(),
            };
            let home = PathBuf::from("/home/ana");
            let person = || Seat::Person {
                account: ana.clone(),
                home: home.clone(),
            };
            let not_asked =
                || -> Seat { panic!("the seat is asked only by a root daemon, switch on") };
            for reading in [true, false] {
                assert_eq!(
                    linux_fs_as(false, true, reading, not_asked),
                    FsAs::Daemon,
                    "a user install"
                );
                assert_eq!(
                    linux_fs_as(true, false, reading, not_asked),
                    FsAs::Daemon,
                    "the switch off"
                );
                assert_eq!(
                    linux_fs_as(true, true, reading, || Seat::Root),
                    FsAs::Daemon,
                    "root at the screen"
                );
            }
            let caps_of = |who: FsAs| match who {
                FsAs::Person {
                    account,
                    home: h,
                    caps,
                } if account == ana && h == home => Some(caps),
                _ => None,
            };
            assert_eq!(
                caps_of(linux_fs_as(true, true, true, person)),
                Some(Caps::Empty),
                "a read: the person, emptied"
            );
            assert_eq!(
                caps_of(linux_fs_as(true, true, false, person)),
                Some(Caps::Kept),
                "a write: the person, as P3d"
            );
            let nobody = || Seat::Nobody("seat0 has no active session".into());
            assert_eq!(
                linux_fs_as(true, true, true, nobody),
                FsAs::Nobody,
                "a read: refused"
            );
            assert_eq!(
                linux_fs_as(true, true, false, nobody),
                FsAs::DaemonAlone,
                "a write: the daemon, alone"
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(FsAs::for_read().await, FsAs::Daemon);
            #[cfg(target_os = "windows")]
            assert_eq!(
                FsAs::for_read().await,
                FsAs::now(),
                "a read and a write take the same person"
            );
        }
    }

    /// #3688 — with nobody at seat0 on a Linux system install, every read is
    /// refused before anything is touched, with a reason the viewer shows:
    /// a download, a folder download and its walk say there is no one to
    /// send it as; the drawer (a folder, the roots, the staging button) says
    /// there is no one to browse as. A received file's answer for nobody
    /// ([`FsAs::DaemonAlone`]) is refused the same way, were it ever to reach
    /// a read.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nobody_at_the_screen_refuses_every_read_with_a_reason() {
        const SEND: &str = "Nobody is signed in at this computer's screen to send the file";
        const BROWSE: &str = "Nobody is signed in at this computer's screen to browse its files";
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("x.txt");
        std::fs::write(&file, b"x").unwrap();
        let folder = temp.path().to_string_lossy().into_owned();
        let h = FilesHandler::new();

        for who in [FsAs::Nobody, FsAs::DaemonAlone] {
            let Err(get) = h
                .begin_outgoing_as(who.clone(), "n3688-1".into(), &file.to_string_lossy())
                .await
            else {
                panic!("a download began as {who:?}");
            };
            assert_eq!(format!("{get}"), SEND, "{who:?}");
            let Err(zip) = h
                .begin_outgoing_zip_as(who.clone(), "n3688-2".into(), &folder)
                .await
            else {
                panic!("a folder download began as {who:?}");
            };
            assert_eq!(format!("{zip}"), SEND, "{who:?}");
            let (zip_writer, _zip_reader) = tokio::io::duplex(64 * 1024);
            let cancel = tokio_util::sync::CancellationToken::new();
            let walked = walk_and_zip(zip_writer, temp.path(), &who, cancel).await;
            assert_eq!(format!("{}", walked.expect_err("a walk")), SEND, "{who:?}");
            for path in [folder.as_str(), "", "~", STAGING_SENTINEL] {
                let Err(dir) = list_dir_as(&who, path).await else {
                    panic!("{path:?} was listed as {who:?}");
                };
                assert_eq!(format!("{dir}"), BROWSE, "{path:?} as {who:?}");
            }
        }
    }

    /// #3688 — a seat0 lookup that fails or runs out of time is not nobody at
    /// the screen: who is there is not known, and a read says so in its own
    /// words, never that nobody is signed in. #3711 — a received file whose
    /// lookup failed takes the daemon alone, as with nobody there; one whose
    /// lookup gave no answer in time is refused in the read's words. The
    /// states that are nobody (an empty seat, the login screen) keep their
    /// texts.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_seat_that_cannot_be_told_is_not_nobody() {
        const CANNOT_TELL: &str =
            "Could not tell who is signed in at this computer's screen. Try again.";
        let wait = std::time::Duration::from_millis(20);
        let slow = seat_within(wait, || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            Seat::Root
        })
        .await;
        assert_eq!(slow, Seat::OutOfTime(wait), "a lookup past its time");
        let broke = seat_within(std::time::Duration::from_secs(5), || -> Seat {
            panic!("a seat0 lookup that fails")
        })
        .await;
        assert!(
            matches!(broke, Seat::CannotTell(_)),
            "a lookup that failed: {broke:?}"
        );
        let quick = seat_within(std::time::Duration::from_secs(5), || Seat::Root).await;
        assert_eq!(quick, Seat::Root);

        let not_asked =
            |_: &str| -> std::result::Result<(crate::fs_identity::Account, PathBuf), String> {
                panic!("no account is resolved for nobody")
            };
        let unanswered = seat_of(
            Err(anyhow!(
                "loginctl show-seat seat0 failed: Failed to connect to bus"
            )),
            not_asked,
        );
        assert!(
            matches!(unanswered, Seat::CannotTell(_)),
            "logind did not answer: {unanswered:?}"
        );
        let empty = seat_of(
            Err(crate::companion::nobody_at_the_screen(
                "seat0 has no active session".into(),
            )),
            not_asked,
        );
        assert!(matches!(empty, Seat::Nobody(_)), "an empty seat: {empty:?}");
        let unresolved = seat_of(Ok(("ana".into(), Some(1000))), |_| {
            Err("getpwnam_r(ana): the account database did not answer".into())
        });
        assert!(
            matches!(unresolved, Seat::CannotTell(_)),
            "an account the database did not answer: {unresolved:?}"
        );
        let failed = || {
            Seat::CannotTell(
                "loginctl show-seat seat0 failed: Unit dbus-org.freedesktop.login1.service is \
                 masked."
                    .into(),
            )
        };
        assert_eq!(
            linux_fs_as(true, true, true, failed),
            FsAs::CannotTell,
            "a failed lookup: a read is refused"
        );
        assert_eq!(
            linux_fs_as(true, true, false, failed),
            FsAs::DaemonAlone,
            "a failed lookup: a write is the daemon's, alone"
        );
        let late = || Seat::OutOfTime(SEAT_LOOKUP_WAIT);
        assert_eq!(
            linux_fs_as(true, true, true, late),
            FsAs::CannotTell,
            "no answer in time: a read is refused"
        );
        assert_eq!(
            linux_fs_as(true, true, false, late),
            FsAs::CannotTell,
            "no answer in time: a write is refused too"
        );

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("x.txt");
        std::fs::write(&file, b"x").unwrap();
        let folder = temp.path().to_string_lossy().into_owned();
        let h = FilesHandler::new();
        let who = FsAs::CannotTell;
        let Err(begun) = h
            .begin_as(who.clone(), "c3711-1".into(), "x.txt".into(), 1, None, None)
            .await
        else {
            panic!("a received file began with no one known at the screen");
        };
        assert_eq!(format!("{begun}"), CANNOT_TELL, "files:begin");
        let Err(get) = h
            .begin_outgoing_as(who.clone(), "c3688-1".into(), &file.to_string_lossy())
            .await
        else {
            panic!("a download began with no one known at the screen");
        };
        assert_eq!(format!("{get}"), CANNOT_TELL, "files:get");
        let Err(zip) = h
            .begin_outgoing_zip_as(who.clone(), "c3688-2".into(), &folder)
            .await
        else {
            panic!("a folder download began with no one known at the screen");
        };
        assert_eq!(format!("{zip}"), CANNOT_TELL, "files:get-folder");
        let (zip_writer, _zip_reader) = tokio::io::duplex(64 * 1024);
        let cancel = tokio_util::sync::CancellationToken::new();
        let walked = walk_and_zip(zip_writer, temp.path(), &who, cancel).await;
        assert_eq!(format!("{}", walked.expect_err("a walk")), CANNOT_TELL);
        for path in [folder.as_str(), "", "~", STAGING_SENTINEL] {
            let Err(dir) = list_dir_as(&who, path).await else {
                panic!("{path:?} was listed with no one known at the screen");
            };
            assert_eq!(format!("{dir}"), CANNOT_TELL, "files:dir {path:?}");
        }
    }

    /// #3711 — a received file's seat0 lookup is asked as a read's is: on a
    /// blocking thread, never the async worker, and given its wait at most.
    /// One that gives no answer in that time refuses the write in the read's
    /// words, before anything is touched. A current-thread runtime has one
    /// worker, this test's own thread: a lookup run inline would run there,
    /// and hold the whole runtime for as long as it took.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_seat0_lookup_past_its_time_refuses_a_write_as_it_refuses_a_read() {
        const CANNOT_TELL: &str =
            "Could not tell who is signed in at this computer's screen. Try again.";
        let worker = std::thread::current().id();
        let wait = std::time::Duration::from_millis(100);
        let lookup_takes = std::time::Duration::from_secs(3);
        for reading in [false, true] {
            let asked_on = std::sync::Arc::new(std::sync::Mutex::new(None));
            let slow = {
                let asked_on = asked_on.clone();
                move || {
                    *asked_on.lock().unwrap() = Some(std::thread::current().id());
                    std::thread::sleep(lookup_takes);
                    Seat::Root
                }
            };
            let began = std::time::Instant::now();
            let who = linux_fs_as_within(true, true, reading, wait, slow).await;
            let waited = began.elapsed();
            // The lookup's thread names itself as it starts.
            let asked_on = (0..500)
                .find_map(|_| {
                    let on = *asked_on.lock().unwrap();
                    if on.is_none() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    on
                })
                .expect("the lookup was asked");
            assert_ne!(
                asked_on, worker,
                "reading {reading}: the lookup ran on the async worker"
            );
            assert!(
                waited < wait + std::time::Duration::from_secs(1),
                "reading {reading}: waited {waited:?} for a lookup given {wait:?}"
            );
            assert_eq!(who, FsAs::CannotTell, "reading {reading}");
            assert_eq!(who.refusal(), Some(CANNOT_TELL), "reading {reading}");
        }
        // The controls: inside its wait the lookup's answer stands, and a
        // lookup that failed outright leaves a write the daemon's, alone.
        let in_time = linux_fs_as_within(true, true, false, lookup_takes, || Seat::Root).await;
        assert_eq!(in_time, FsAs::Daemon, "root at the screen, in time");
        let failed = linux_fs_as_within(true, true, false, lookup_takes, || {
            Seat::CannotTell("getpwnam_r(ana): the account database did not answer".into())
        })
        .await;
        assert_eq!(failed, FsAs::DaemonAlone, "a failed lookup, a write");
    }

    /// #3688 — seat0's active uid is read from logind's own record of the
    /// seat, as `sd-login` reads it: `ACTIVE_UID=`, which logind writes only
    /// while a session is active there.
    #[cfg(target_os = "linux")]
    #[test]
    fn seat0s_active_uid_is_logind_s_own_record() {
        let seat = "# This is private data. Do not parse.
IS_SEAT0=1
CAN_MULTI_SESSION=1

                    CAN_TTY=1
CAN_GRAPHICAL=1
ACTIVE=2
ACTIVE_UID=1000
SESSIONS=2 c1

                    UIDS=1000 120
";
        assert_eq!(seat0_active_uid_in(seat), Some(1000));
        let empty = "# This is private data. Do not parse.
IS_SEAT0=1
CAN_MULTI_SESSION=1

                     CAN_TTY=1
CAN_GRAPHICAL=0
";
        assert_eq!(seat0_active_uid_in(empty), None, "no active session");
        assert_eq!(
            seat0_active_uid_in(
                "ACTIVE_UID=
"
            ),
            None
        );
        assert_eq!(
            seat0_active_uid_in(
                "ACTIVE_UID=gdm
"
            ),
            None
        );
    }

    /// #3688 — a listing answers inside the viewer's own timer for it: the
    /// drawer gives up on `files:dir` after 5 s (`listDir`,
    /// `ui/src/composables/useRemoteControl.ts`). The seat0 lookup and the
    /// listing's own timeout run one after the other, so together they stay
    /// inside it; a lookup that took the whole 5 s could never be answered.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_listing_waits_for_the_seat_inside_the_drawers_timer() {
        let drawer = std::time::Duration::from_secs(5);
        let listing = std::time::Duration::from_secs(DIR_LIST_TIMEOUT_SECS);
        assert!(
            SEAT_LOOKUP_WAIT + listing < drawer,
            "{SEAT_LOOKUP_WAIT:?} + {listing:?} leaves no room inside {drawer:?}"
        );
    }

    /// #3688 — the uid logind recorded for seat0's session must be the uid
    /// its name resolves to: a name the account database answers with
    /// another uid (a lookup order that puts another source first, an
    /// account deleted and made again) is nobody, saying why, never another
    /// account's rights. With no uid recorded the name decides, as before.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_seat0_name_that_resolves_to_another_uid_is_nobody() {
        fn resolves_to(
            uid: u32,
        ) -> impl FnOnce(&str) -> std::result::Result<(crate::fs_identity::Account, PathBuf), String>
        {
            move |name| Ok((ana(uid, name), PathBuf::from("/home/ana")))
        }
        fn ana(uid: u32, name: &str) -> crate::fs_identity::Account {
            crate::fs_identity::Account {
                uid,
                gid: uid,
                groups: vec![uid],
                name: name.into(),
            }
        }
        let found = |uid: Option<u32>| Ok(("ana".to_string(), uid));
        let person = Seat::Person {
            account: ana(1000, "ana"),
            home: PathBuf::from("/home/ana"),
        };
        assert_eq!(
            seat_of(found(Some(1000)), resolves_to(1000)),
            person,
            "the same uid"
        );
        assert_eq!(
            seat_of(found(None), resolves_to(1000)),
            person,
            "no uid recorded: the name's account"
        );
        assert_eq!(seat_of(found(Some(0)), resolves_to(1000)), Seat::Root);
        match seat_of(found(Some(1001)), resolves_to(1000)) {
            Seat::Nobody(why) => assert!(
                why.contains("1001") && why.contains("1000"),
                "the reason names both: {why}"
            ),
            other => panic!("another uid's account was taken: {other:?}"),
        }
    }

    /// #3688 — a host with no logind at all (`loginctl` not installed, or a
    /// host systemd did not boot: a container, WSL without systemd, a minimal
    /// VM) has no seat0, so nobody is at its screen. A read there gets the
    /// nobody text ([`FsAs::Nobody`]), which the drop-in answers, never "Try
    /// again" on every read; a received file is the daemon's alone, as
    /// before. Every other failure of the lookup still cannot tell.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_host_with_no_logind_has_nobody_at_the_screen() {
        use crate::companion::{loginctl_did_not_start, loginctl_failed};
        let not_asked =
            |_: &str| -> std::result::Result<(crate::fs_identity::Account, PathBuf), String> {
                panic!("no account is resolved for nobody")
            };
        let seat = ["show-seat", "seat0"];
        let no_seat = [
            (
                "loginctl is not installed",
                loginctl_did_not_start(std::io::ErrorKind::NotFound.into()),
            ),
            (
                "systemd did not boot this host",
                loginctl_failed(
                    &seat,
                    "System has not been booted with systemd as init system (PID 1). Can't \
                     operate.\nFailed to connect to bus: Host is down\n",
                ),
            ),
        ];
        for (host, found) in no_seat {
            let at_seat0 = seat_of(Err(found), not_asked);
            assert!(matches!(at_seat0, Seat::Nobody(_)), "{host}: {at_seat0:?}");
            let seated = at_seat0.clone();
            assert_eq!(
                linux_fs_as(true, true, true, move || seated),
                FsAs::Nobody,
                "{host}: a read"
            );
            assert_eq!(
                linux_fs_as(true, true, false, move || at_seat0),
                FsAs::DaemonAlone,
                "{host}: a received file"
            );
        }
        let not_known = [
            (
                "the bus does not answer",
                loginctl_failed(&seat, "Failed to connect to bus: Connection refused"),
            ),
            (
                "logind does not answer in time",
                loginctl_failed(&seat, "Failed to get properties: Connection timed out"),
            ),
            (
                "loginctl would not start",
                loginctl_did_not_start(std::io::ErrorKind::PermissionDenied.into()),
            ),
        ];
        for (failure, found) in not_known {
            let at_seat0 = seat_of(Err(found), not_asked);
            assert!(
                matches!(at_seat0, Seat::CannotTell(_)),
                "{failure}: {at_seat0:?}"
            );
        }
    }

    /// #3688 — a received file's log line says which of the two it is,
    /// though both write as the daemon alone: nobody at seat0, or who is
    /// there not known (the lookup failed or ran out of its time).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_received_files_log_line_tells_nobody_from_not_known() {
        let logged = |seat: Seat| {
            let log = crate::peer::files_log_tests::Logged::start(tracing::Level::DEBUG);
            assert_eq!(
                linux_fs_as(true, true, false, move || seat),
                FsAs::DaemonAlone
            );
            log.text()
        };
        let nobody = logged(Seat::Nobody("seat0 has no active session".into()));
        assert!(nobody.contains("nobody at seat0"), "nobody: {nobody}");
        let unknown = logged(Seat::CannotTell(
            "the seat0 lookup did not answer within 1.5s".into(),
        ));
        assert!(
            !unknown.contains("nobody"),
            "who is there is not known, not nobody: {unknown}"
        );
        assert!(unknown.contains("not known"), "not known: {unknown}");
    }

    /// The process-wide drop folder a test set, unset again when this drops
    /// (a failed assertion included), with the folder it names removed.
    /// Hold `FILES_DIR_TEST_LOCK` while it lives.
    #[cfg(target_os = "linux")]
    struct DropFolderSet(PathBuf);

    #[cfg(target_os = "linux")]
    impl Drop for DropFolderSet {
        fn drop(&mut self) {
            set_files_dir(None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// #3688 — with nobody at seat0 on a Linux system install, a configured
    /// drop folder is not taken either: `~` has no person's home to expand
    /// against, so `~/…` never resolves in the daemon's own home, which on a
    /// root daemon is root's. The drop, the status card and the orphan sweep
    /// take the default folder, and nothing is made where the configured
    /// folder would be. The daemon itself (root at the screen, or the switch
    /// off) still takes it there (the control).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nobody_at_the_screen_takes_no_configured_folder_either() {
        let _lock = FILES_DIR_TEST_LOCK.lock().await;
        // The default folder's home, stood in: never a real one.
        let _own = own_staging::OwnHome::new(false, true);
        let home = writer_context()
            .active_home
            .map(PathBuf::from)
            .expect("the daemon's own home");
        let sub = format!("roomler-files-dir-alone-{}", std::process::id());
        let in_its_home = home.join(&sub);
        let _ = std::fs::remove_dir_all(&in_its_home);
        let raw = format!("~/{sub}");
        set_files_dir(Some(raw.clone()));
        let _set = DropFolderSet(in_its_home.clone());
        let default = default_download_dir(false).unwrap();

        assert_eq!(
            download_dir_for(&FsAs::DaemonAlone).await.unwrap(),
            default,
            "a drop with nobody at the screen: the default folder"
        );
        assert!(
            !in_its_home.exists(),
            "nothing is made in the daemon's own home"
        );
        assert_eq!(
            compute_effective_files_dir_as(&FsAs::DaemonAlone, Some(raw.as_str())),
            Some(default.to_string_lossy().into_owned()),
            "the status card names the default folder"
        );
        let walked = configured_folder_without_creating(&FsAs::DaemonAlone);
        assert!(walked.is_err(), "the sweep never walks it: {walked:?}");

        assert_eq!(
            download_dir_for(&FsAs::Daemon).await.unwrap(),
            in_its_home,
            "the daemon itself takes `~` in its own home"
        );
    }

    /// #3688 — a handle a read as the person opened leaves its step only
    /// when it is the file asked for, off the kernel's own file systems: the
    /// same file reached through a `/proc/self/fd` link is not the one asked
    /// (the link's own path is), and a procfs file is on one; a folder on
    /// sysfs is refused for listing. A plain file and folder pass.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_handle_that_is_not_the_file_asked_is_refused() {
        use std::os::fd::AsRawFd;
        let temp = tempfile::tempdir().unwrap();
        let asked = std::fs::canonicalize(temp.path())
            .unwrap()
            .join("asked.txt");
        std::fs::write(&asked, b"x").unwrap();
        let file = std::fs::File::open(&asked).unwrap();
        held_to_the_person(&file, &asked).expect("the file asked for passes");
        let folder = std::fs::canonicalize(temp.path()).unwrap();
        let held_dir = std::fs::File::open(&folder).unwrap();
        folder_held_to_the_person(&held_dir, &folder).expect("an ordinary folder passes");
        let magic_dir = PathBuf::from(format!("/proc/self/fd/{}", held_dir.as_raw_fd()));
        let through_dir = std::fs::File::open(&magic_dir).unwrap();
        let Err(swapped_dir) = folder_held_to_the_person(&through_dir, &magic_dir) else {
            panic!("a folder reached through a /proc link passed as the folder asked");
        };
        assert_eq!(
            swapped_dir.to_string(),
            format!(
                "{}: what opened is not the folder asked for, so file transfer does not list it",
                magic_dir.display()
            )
        );

        let magic = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let through = std::fs::File::open(&magic).unwrap();
        let Err(swapped) = held_to_the_person(&through, &magic) else {
            panic!("a handle reached through a /proc link passed as the file asked");
        };
        assert_eq!(
            swapped.to_string(),
            format!(
                "{}: what opened is not the file asked for, so file transfer does not send it",
                magic.display()
            )
        );

        let status = std::fs::canonicalize("/proc/self/status").unwrap();
        let kernel = std::fs::File::open(&status).unwrap();
        let Err(on_proc) = held_to_the_person(&kernel, &status) else {
            panic!("a procfs handle passed");
        };
        assert_eq!(
            on_proc.to_string(),
            format!(
                "{}: procfs is a kernel file system, which file transfer does not read for the \
                 person signed in at this computer's screen",
                status.display()
            )
        );
        let sys = std::path::Path::new("/sys");
        let Err(on_sys) = folder_held_to_the_person(&std::fs::File::open(sys).unwrap(), sys) else {
            panic!("a sysfs folder passed");
        };
        assert!(
            on_sys
                .to_string()
                .starts_with("/sys: sysfs is a kernel file system"),
            "{on_sys}"
        );
    }

    /// #3048 — the Windows twin of `as_root`: a machine-wide (SystemContext)
    /// worker writes a received file AS the person signed in to its session.
    ///
    /// A test runner is not SYSTEM and has no session user to ask for, so the
    /// person is STOOD IN for: the runner's own token, duplicated at LOW
    /// integrity (`win_token::low_integrity_copy`), handed to `FsAs::Person`
    /// as a fixed token. A folder labelled low is one the person may write; an
    /// unlabelled one reads as medium, which a low token may not write. The
    /// runner itself, at medium or high, plays the daemon. This proves that
    /// the person's identity is applied to every step on their side. It proves
    /// nothing about `WTSQueryUserToken`, which needs SYSTEM: that half is the
    /// lab's.
    #[cfg(target_os = "windows")]
    mod as_the_session_person {
        use super::*;
        use crate::files::stand_in::{self, label_no_read_up, person};
        use crate::win_identity::{PersonToken, SessionPerson};
        use std::path::Path;

        fn system32(tool: &str, args: &[&std::ffi::OsStr]) {
            let out = std::process::Command::new(crate::appdirs::system32_tool(tool))
                .args(args)
                .output()
                .unwrap_or_else(|e| panic!("{tool} runs: {e}"));
            assert!(out.status.success(), "{tool} {args:?}: {out:?}");
        }

        /// A folder the low-integrity person may write: its own low label, not
        /// handed down, so a file made inside it carries a label only when a
        /// low token made it.
        fn label_low(dir: &Path) {
            system32(
                "icacls.exe",
                &[dir.as_os_str(), "/setintegritylevel".as_ref(), "L".as_ref()],
            );
        }

        /// `from`, made to lead to the folder `to` (`mklink /J`).
        fn lead_to(from: &Path, to: &Path) {
            system32(
                "cmd.exe",
                &[
                    "/c".as_ref(),
                    "mklink".as_ref(),
                    "/J".as_ref(),
                    from.as_os_str(),
                    to.as_os_str(),
                ],
            );
        }

        /// `path`'s mandatory label as SDDL (`S:(ML;;NW;;;LW)` for low), empty
        /// when it has none (which reads as medium).
        fn label_of(path: &Path) -> String {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Foundation::LocalFree;
            use windows_sys::Win32::Security::Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
                SDDL_REVISION_1, SE_FILE_OBJECT,
            };
            use windows_sys::Win32::Security::{LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: a NUL-terminated path and out-params; the descriptor is
            // freed below.
            let rc = unsafe {
                GetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    LABEL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            assert_eq!(rc, 0, "reading the label of {}", path.display());
            let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
            // SAFETY: the descriptor read above; `text` is freed below.
            let ok = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    sd,
                    SDDL_REVISION_1,
                    LABEL_SECURITY_INFORMATION,
                    &mut text,
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: allocated by GetNamedSecurityInfoW, freed once.
            unsafe { LocalFree(sd as _) };
            assert!(ok != 0, "the label of {} as SDDL", path.display());
            // SAFETY: a NUL-terminated string the call allocated.
            let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
            let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
            // SAFETY: allocated by the conversion, freed once.
            unsafe { LocalFree(text as _) };
            sddl
        }

        /// A partial the daemon staged under `staging` for a transfer that
        /// targets `dest`, begun for `person_sid`, and registered as staged by
        /// the daemon (as the startup sweep registers one).
        fn staged_partial(staging: &Path, id: &str, dest: &Path, person_sid: Option<&str>) {
            let dir = staging.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let meta = PartialMeta {
                protocol_version: 1,
                filename: "probe.txt".into(),
                expected_size: 5,
                dest_dir: dest.to_path_buf(),
                reserved_final_path: dest.join("probe.txt"),
                created_at_unix: chrono::Utc::now().timestamp(),
                rel_path: None,
                person_sid: person_sid.map(str::to_string),
            };
            std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
            std::fs::write(dir.join("data"), b"").unwrap();
            PARTIAL_REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id.to_string(),
                    PartialEntry {
                        meta_path: dir.join("meta.json"),
                        who: FsAs::Daemon,
                    },
                );
        }

        /// The person's Downloads leads to a folder they may not write: the
        /// received file is refused, and nothing lands there, as for anything
        /// they would write themselves.
        #[tokio::test]
        async fn a_link_in_the_persons_folder_is_followed_only_with_their_rights() {
            let temp = tempfile::tempdir().unwrap();
            let profile = temp.path().join("profile");
            std::fs::create_dir(&profile).unwrap();
            let theirs_not = temp.path().join("admin-only");
            std::fs::create_dir(&theirs_not).unwrap();
            let downloads = profile.join("Downloads");
            lead_to(&downloads, &theirs_not);
            let h = FilesHandler::new();
            let id = format!("w3048-link-{}", std::process::id());
            let begun = h
                .begin_as(
                    person(&profile, &downloads),
                    id.clone(),
                    "probe.txt".into(),
                    5,
                    None,
                    None,
                )
                .await;
            if begun.is_ok() {
                // Finished, so the file lands wherever the begin pointed it.
                let _ = h.chunk(b"hello").await;
                let _ = h.end(&id).await;
            }
            let landed: Vec<_> = std::fs::read_dir(&theirs_not)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert!(
                landed.is_empty(),
                "nothing lands where the person may not write: {landed:?}"
            );
            assert!(begun.is_err(), "the begin is refused");
        }

        /// Staging stays the daemon's (a folder the person may not write),
        /// and the received file is copied into a new file the person makes:
        /// it carries their low label, where a file the daemon renamed in
        /// carries none.
        #[tokio::test]
        async fn the_programdata_split_stages_as_the_daemon_and_places_as_the_person() {
            let temp = tempfile::tempdir().unwrap();
            let profile = temp.path().join("profile");
            let downloads = profile.join("Downloads");
            std::fs::create_dir_all(&downloads).unwrap();
            label_low(&downloads);
            let staging = temp.path().join("staging");
            std::fs::create_dir(&staging).unwrap();
            let h = FilesHandler::with_staging_root(staging.clone());
            let id = format!("w3048-split-{}", std::process::id());
            h.begin_as(
                person(&profile, &downloads),
                id.clone(),
                "probe.txt".into(),
                5,
                None,
                None,
            )
            .await
            .expect("begin");
            h.chunk(b"hello").await.expect("chunk");
            let (placed, bytes) = h.end(&id).await.expect("end");
            assert_eq!(bytes, 5);
            assert_eq!(placed, downloads.join("probe.txt"));
            assert_eq!(std::fs::read(&placed).unwrap(), b"hello");
            let label = label_of(&placed);
            assert!(
                label.contains(";;;LW)"),
                "born the person's, made by their token: {label:?}"
            );
            assert!(!staging.join(&id).exists(), "the staging is gone");
        }

        /// A resume places the file only for the person the transfer began
        /// for; one from a daemon that recorded no person goes to whoever is
        /// signed in now.
        #[tokio::test]
        async fn a_resume_places_only_for_the_person_it_began_for() {
            let temp = tempfile::tempdir().unwrap();
            let profile = temp.path().join("profile");
            let downloads = profile.join("Downloads");
            std::fs::create_dir_all(&downloads).unwrap();
            label_low(&downloads);
            let staging = temp.path().join("staging");
            let me = person(&profile, &downloads);

            let other = format!("w3048-other-{}", std::process::id());
            staged_partial(&staging, &other, &downloads, Some("S-1-5-21-1-2-3-1001"));
            let refused = FilesHandler::with_staging_root(staging.clone())
                .resume_incoming_as(me.clone(), &other, 0)
                .await
                .expect_err("another person's transfer");
            assert!(
                format!("{refused:#}").contains(NOT_THE_PERSON),
                "{refused:#}"
            );

            let older = format!("w3048-older-{}", std::process::id());
            staged_partial(&staging, &older, &downloads, None);
            let accepted = FilesHandler::with_staging_root(staging.clone())
                .resume_incoming_as(me, &older, 0)
                .await
                .expect("a partial with no person recorded");
            assert_eq!(accepted, 0);
            PARTIAL_REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|id, _| id != &other && id != &older);
        }

        /// With nobody signed in, a received file is refused with a reason the
        /// viewer shows, and nothing is written.
        #[tokio::test]
        async fn nobody_signed_in_is_refused_with_a_reason() {
            let temp = tempfile::tempdir().unwrap();
            let dest = temp.path().to_string_lossy().into_owned();
            let h = FilesHandler::new();
            let id = format!("w3048-nobody-{}", std::process::id());
            let err = h
                .begin_as(
                    FsAs::Nobody,
                    id.clone(),
                    "probe.txt".into(),
                    5,
                    None,
                    Some(&dest),
                )
                .await
                .expect_err("nobody to write as");
            // What the viewer's `files:error` carries (`peer.rs` sends `{e}`).
            assert_eq!(
                format!("{err}"),
                "Nobody is signed in to this computer to receive the file"
            );
            assert!(
                std::fs::read_dir(temp.path()).unwrap().next().is_none(),
                "nothing written"
            );
        }

        /// Who a transfer acts as: the person signed in, on a LocalSystem
        /// worker; nobody there refuses; another account, or the switch off,
        /// is the daemon itself, which never asks for the person. #3669 — a
        /// worker that could not read its own token counts as LocalSystem:
        /// the person, or nobody, never itself (fail closed).
        #[test]
        fn fs_as_for_takes_the_person_only_as_local_system() {
            let p = SessionPerson {
                sid: "S-1-5-21-1-2-3-1001".into(),
                profile: r"C:\Users\p".into(),
                downloads: Some(r"C:\Users\p\Downloads".into()),
            };
            let them = FsAs::Person {
                sid: p.sid.clone(),
                profile: p.profile.clone(),
                downloads: p.downloads.clone(),
                token: PersonToken::Session,
            };
            assert_eq!(fs_as_for(Some(true), true, || Some(p.clone())), them);
            assert_eq!(fs_as_for(Some(true), true, || None), FsAs::Nobody);
            let not_asked = || -> Option<SessionPerson> { panic!("the person was asked for") };
            assert_eq!(fs_as_for(Some(false), true, not_asked), FsAs::Daemon);
            assert_eq!(
                fs_as_for(Some(true), false, not_asked),
                FsAs::Daemon,
                "the switch keeps the daemon's own identity"
            );
            assert_eq!(
                fs_as_for(None, true, || Some(p.clone())),
                them,
                "an unreadable token takes the person"
            );
            assert_eq!(
                fs_as_for(None, true, || None),
                FsAs::Nobody,
                "an unreadable token with no one signed in refuses"
            );
        }

        /// #3048's criterion, for a SystemContext worker (`fs_as_for` as
        /// LocalSystem, with a person signed in): with no folder chosen, a file
        /// lands in the person's Downloads known folder, local or redirected,
        /// else in their profile, and the status card shows the known folder
        /// unchecked. Never the machine-global folder the daemon's own ladder
        /// ends in.
        #[test]
        fn a_system_context_worker_takes_the_person_downloads_never_the_machine_folder() {
            let _lock = FILES_DIR_TEST_LOCK.blocking_lock();
            set_files_dir(None);
            let temp = tempfile::tempdir().unwrap();
            let profile = temp.path().join("p");
            let local = profile.join("Downloads");
            let redirected = temp.path().join("share").join("p").join("Downloads");
            let offline = temp.path().join("offline").join("Downloads");
            std::fs::create_dir_all(&local).unwrap();
            std::fs::create_dir_all(&redirected).unwrap();
            let machine = crate::appdirs::machine_global_dir().join("uploads");
            let worker = |known: Option<&Path>| {
                fs_as_for(Some(true), true, || {
                    Some(SessionPerson {
                        sid: "S-1-5-21-1-2-3-1001".into(),
                        profile: profile.clone(),
                        downloads: known.map(Path::to_path_buf),
                    })
                })
            };
            // (their known folder, where a file lands, what the card shows)
            let cases = [
                (Some(local.as_path()), &local, &local),
                (Some(redirected.as_path()), &redirected, &redirected),
                (None, &profile, &profile),
                (Some(offline.as_path()), &profile, &offline),
            ];
            for (known, lands, card) in cases {
                let who = worker(known);
                let got = download_dir_as(&who).unwrap();
                assert_eq!(&got, lands, "a file, with the known folder {known:?}");
                assert_ne!(got, machine, "never the machine-global folder");
                assert_eq!(
                    &default_download_dir_as(&who, false).unwrap(),
                    card,
                    "the card, with the known folder {known:?}"
                );
            }
        }

        /// What the viewer is told when Windows refuses a read to the person.
        fn not_allowed(path: &Path) -> String {
            format!(
                "{}: Windows refused to open it for the person signed in to this computer",
                path.display()
            )
        }

        /// The zip of `root`, walked as `who`.
        async fn zip_as(who: &FsAs, root: &Path) -> Vec<u8> {
            use tokio::io::AsyncReadExt;
            let (zip_writer, mut zip_reader) = tokio::io::duplex(64 * 1024);
            let (who, root) = (who.clone(), root.to_path_buf());
            let walk = tokio::spawn(async move {
                let cancel = tokio_util::sync::CancellationToken::new();
                walk_and_zip(zip_writer, &root, &who, cancel).await
            });
            let mut zip = Vec::new();
            zip_reader.read_to_end(&mut zip).await.unwrap();
            walk.await.unwrap().expect("walk_and_zip");
            zip
        }

        /// #3669 — a download is opened as the person: their own file is
        /// offered and its bytes are the ones sent; a file only the daemon may
        /// read is refused, saying why; the daemon itself is offered that file
        /// (the control).
        #[tokio::test]
        async fn a_download_opens_the_file_as_the_person() {
            use tokio::io::AsyncReadExt;
            let temp = tempfile::tempdir().unwrap();
            let theirs = temp.path().join("theirs.txt");
            let not_theirs = temp.path().join("daemon-only.txt");
            std::fs::write(&theirs, b"the person's file").unwrap();
            std::fs::write(&not_theirs, b"only the daemon reads this file").unwrap();
            label_no_read_up(&not_theirs);
            let me = person(temp.path(), temp.path());
            let at = |p: &Path| p.to_string_lossy().into_owned();
            let h = FilesHandler::new();

            let offer = h
                .begin_outgoing_as(me.clone(), "r3669-1".into(), &at(&theirs))
                .await
                .expect("the person's own file");
            assert_eq!(offer.size, Some(17));
            let mut sent = Vec::new();
            h.open_outgoing("r3669-1")
                .await
                .expect("the open file")
                .read_to_end(&mut sent)
                .await
                .unwrap();
            assert_eq!(sent, b"the person's file");
            h.finish_outgoing("r3669-1").await;

            let Err(refused) = h
                .begin_outgoing_as(me, "r3669-2".into(), &at(&not_theirs))
                .await
            else {
                panic!("a file only the daemon may read was offered to the person");
            };
            assert_eq!(format!("{refused}"), not_allowed(&not_theirs));

            h.begin_outgoing_as(FsAs::Daemon, "r3669-3".into(), &at(&not_theirs))
                .await
                .expect("the daemon reads it");
            h.finish_outgoing("r3669-3").await;
        }

        /// #3669 — a folder download opens every entry as the person, not only
        /// its root: their file is in the zip; a file only the daemon may
        /// read, and one inside a folder only the daemon may list, are left
        /// out, as an unreadable file always was; the daemon itself zips all
        /// three (the control). A folder the person may not list is refused
        /// when the download begins, saying why, never sent as an empty zip.
        #[tokio::test]
        async fn a_folder_download_opens_every_entry_as_the_person() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("folder");
            let shut = root.join("daemon-only");
            std::fs::create_dir_all(&shut).unwrap();
            std::fs::write(root.join("theirs.txt"), b"the person's file").unwrap();
            std::fs::write(root.join("not-theirs.txt"), b"only the daemon reads this").unwrap();
            std::fs::write(shut.join("inside.txt"), b"only the daemon lists this").unwrap();
            label_no_read_up(&root.join("not-theirs.txt"));
            label_no_read_up(&shut);
            let me = person(temp.path(), temp.path());
            let has = |zip: &[u8], needle: &[u8]| zip.windows(needle.len()).any(|w| w == needle);

            let theirs = zip_as(&me, &root).await;
            assert!(has(&theirs, b"the person's file"), "their file is in");
            assert!(
                !has(&theirs, b"only the daemon reads this"),
                "a file they may not read is not"
            );
            assert!(
                !has(&theirs, b"only the daemon lists this"),
                "nor one inside a folder they may not list"
            );
            let all = zip_as(&FsAs::Daemon, &root).await;
            for needle in [
                &b"the person's file"[..],
                b"only the daemon reads this",
                b"only the daemon lists this",
            ] {
                assert!(has(&all, needle), "the daemon zips {needle:?}");
            }

            let Err(refused) = FilesHandler::new()
                .begin_outgoing_zip_as(me, "z3669-1".into(), &shut.to_string_lossy())
                .await
            else {
                panic!("a folder the person may not list was offered to them");
            };
            assert_eq!(format!("{refused}"), not_allowed(&shut));
        }

        /// #3669 — the drawer lists a folder as the person: theirs lists; one
        /// only the daemon may list is refused, saying why; the daemon itself
        /// lists it (the control).
        #[tokio::test]
        async fn a_listing_runs_as_the_person() {
            let temp = tempfile::tempdir().unwrap();
            let theirs = temp.path().join("theirs");
            let shut = temp.path().join("daemon-only");
            std::fs::create_dir(&theirs).unwrap();
            std::fs::create_dir(&shut).unwrap();
            std::fs::write(theirs.join("a.txt"), b"a").unwrap();
            std::fs::write(shut.join("b.txt"), b"b").unwrap();
            label_no_read_up(&shut);
            let me = person(temp.path(), temp.path());
            let at = |p: &Path| p.to_string_lossy().into_owned();

            let listed = list_dir_as(&me, &at(&theirs))
                .await
                .expect("their folder lists");
            let names: Vec<_> = listed.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["a.txt"]);
            let Err(refused) = list_dir_as(&me, &at(&shut)).await else {
                panic!("a folder only the daemon may list was listed for the person");
            };
            assert_eq!(format!("{refused}"), not_allowed(&shut));
            let control = list_dir_as(&FsAs::Daemon, &at(&shut))
                .await
                .expect("the daemon lists it");
            assert_eq!(control.entries.len(), 1);
        }

        /// #3669 — the wiring, not only the step: `begin_outgoing`, which
        /// `files:get` calls, reads as whoever `FsAs::for_read()` answers.
        /// With the person stood in, their own file is offered as theirs, and
        /// a file only the daemon may read is refused, saying why. An entry
        /// point that read as the daemon would offer it.
        #[tokio::test]
        async fn files_get_reads_as_the_reader_for_read_answers() {
            let temp = tempfile::tempdir().unwrap();
            let theirs = temp.path().join("theirs.txt");
            let not_theirs = temp.path().join("daemon-only.txt");
            std::fs::write(&theirs, b"the person's file").unwrap();
            std::fs::write(&not_theirs, b"only the daemon reads this file").unwrap();
            label_no_read_up(&not_theirs);
            let _as = stand_in::reader(person(temp.path(), temp.path())).await;
            let h = FilesHandler::new();

            let offer = h
                .begin_outgoing("e3669-1".into(), &theirs.to_string_lossy())
                .await
                .expect("the person's own file");
            let me = crate::win_token::own_user_sid();
            assert_eq!(offer.who.person_sid(), me.as_deref(), "offered as theirs");
            h.finish_outgoing("e3669-1").await;
            let Err(refused) = h
                .begin_outgoing("e3669-2".into(), &not_theirs.to_string_lossy())
                .await
            else {
                panic!("files:get offered a file only the daemon may read: it read as the daemon");
            };
            assert_eq!(format!("{refused}"), not_allowed(&not_theirs));
        }

        /// #3669 — `list_dir`, which `files:dir` calls, lists as whoever
        /// `FsAs::for_read()` answers: with the person stood in, their folder
        /// lists, and one only the daemon may list is refused, saying why.
        #[tokio::test]
        async fn files_dir_lists_as_the_reader_for_read_answers() {
            let temp = tempfile::tempdir().unwrap();
            let theirs = temp.path().join("theirs");
            let shut = temp.path().join("daemon-only");
            std::fs::create_dir(&theirs).unwrap();
            std::fs::create_dir(&shut).unwrap();
            std::fs::write(theirs.join("a.txt"), b"a").unwrap();
            std::fs::write(shut.join("b.txt"), b"b").unwrap();
            label_no_read_up(&shut);
            let _as = stand_in::reader(person(temp.path(), temp.path())).await;

            let listed = list_dir(&theirs.to_string_lossy())
                .await
                .expect("their folder lists");
            let names: Vec<_> = listed.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["a.txt"]);
            let Err(refused) = list_dir(&shut.to_string_lossy()).await else {
                panic!(
                    "files:dir listed a folder only the daemon may list: it listed as the daemon"
                );
            };
            assert_eq!(format!("{refused}"), not_allowed(&shut));
        }

        /// #3669 — a person who signs out partway through a folder download
        /// ends its walk in an error, after the entries already written:
        /// `walk_and_zip` returns `Err`, so the pump sends `files:error`,
        /// never the `files:eof` that would pass a short zip off as whole.
        /// The steps: the root resolved (1) and listed (2), the first file
        /// opened (3); by the second file they have gone.
        #[tokio::test]
        async fn a_walk_whose_person_signs_out_ends_in_an_error_not_a_short_zip() {
            use tokio::io::AsyncReadExt;
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("folder");
            std::fs::create_dir(&root).unwrap();
            // NTFS lists a folder in name order.
            std::fs::write(root.join("a.txt"), b"the first file").unwrap();
            std::fs::write(root.join("b.txt"), b"the second file").unwrap();
            let who = stand_in::person_signing_out_after(3, temp.path());
            let (zip_writer, mut zip_reader) = tokio::io::duplex(64 * 1024);
            let walk = tokio::spawn(async move {
                let cancel = tokio_util::sync::CancellationToken::new();
                walk_and_zip(zip_writer, &root, &who, cancel).await
            });
            let mut zip = Vec::new();
            zip_reader.read_to_end(&mut zip).await.unwrap();
            let walked = walk.await.unwrap();
            let has = |needle: &[u8]| zip.windows(needle.len()).any(|w| w == needle);
            assert!(has(b"the first file"), "the entry written before they went");
            assert!(!has(b"the second file"), "none after");
            assert!(
                !has(b"PK\x05\x06"),
                "no end record: never passed off as whole"
            );
            let why = walked.expect_err("the walk ends in an error");
            assert_eq!(format!("{why}"), NOT_THE_PERSON);
        }

        /// #3669 — with nobody signed in, every read is refused before
        /// anything is touched, with a reason the viewer shows: a download, a
        /// folder download and its walk say there is no one to send it as;
        /// the drawer (a folder, the roots, the staging button) says there is
        /// no one to browse as.
        #[tokio::test]
        async fn nobody_signed_in_refuses_every_read_with_a_reason() {
            const SEND: &str = "Nobody is signed in to this computer to send the file";
            const BROWSE: &str = "Nobody is signed in to this computer to browse its files";
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join("x.txt");
            std::fs::write(&file, b"x").unwrap();
            let folder = temp.path().to_string_lossy().into_owned();
            let h = FilesHandler::new();

            let Err(get) = h
                .begin_outgoing_as(FsAs::Nobody, "n3669-1".into(), &file.to_string_lossy())
                .await
            else {
                panic!("a download began with nobody signed in");
            };
            assert_eq!(format!("{get}"), SEND);
            let Err(zip) = h
                .begin_outgoing_zip_as(FsAs::Nobody, "n3669-2".into(), &folder)
                .await
            else {
                panic!("a folder download began with nobody signed in");
            };
            assert_eq!(format!("{zip}"), SEND);
            let (zip_writer, _zip_reader) = tokio::io::duplex(64 * 1024);
            let cancel = tokio_util::sync::CancellationToken::new();
            let walked = walk_and_zip(zip_writer, temp.path(), &FsAs::Nobody, cancel).await;
            assert_eq!(format!("{}", walked.expect_err("a walk as nobody")), SEND);
            for path in [folder.as_str(), "", "~", STAGING_SENTINEL] {
                let Err(dir) = list_dir_as(&FsAs::Nobody, path).await else {
                    panic!("{path:?} was listed with nobody signed in");
                };
                assert_eq!(format!("{dir}"), BROWSE, "{path:?}");
            }
        }

        /// Whether this thread is impersonating now: it has a token of its own.
        fn impersonating() -> bool {
            use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_TOKEN};
            use windows_sys::Win32::Security::TOKEN_QUERY;
            use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
            let mut token = std::ptr::null_mut();
            // SAFETY: this thread's pseudo-handle and an out-param, closed below.
            if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } != 0 {
                // SAFETY: the token just opened, closed once.
                unsafe { CloseHandle(token) };
                return true;
            }
            let why = std::io::Error::last_os_error();
            assert_eq!(
                why.raw_os_error(),
                Some(ERROR_NO_TOKEN as i32),
                "no token of its own: {why}"
            );
            false
        }

        /// #3669 — the lifetime #3048's steps have, pinned for the reads that
        /// now take them: a step as the person impersonates on its blocking
        /// thread, and the next job on that same thread does not, after a
        /// step that panicked too. No other thread ever carries the token.
        #[test]
        fn a_step_as_the_person_never_outlives_itself() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(1)
                .build()
                .unwrap();
            let temp = tempfile::tempdir().unwrap();
            let me = person(temp.path(), temp.path());
            let probe = || Ok((impersonating(), std::thread::current().id()));
            runtime.block_on(async {
                let (inside, thread) = me.run(probe).await.unwrap();
                assert!(inside, "a step as the person impersonates");
                let (after, next) = FsAs::Daemon.run(probe).await.unwrap();
                assert_eq!(next, thread, "the one blocking thread");
                assert!(!after, "the next job on it does not");
                let panicked = me
                    .run(|| -> Result<()> { panic!("a step that panics") })
                    .await;
                assert!(panicked.is_err(), "a panicking step fails");
                let (after_panic, next) = FsAs::Daemon.run(probe).await.unwrap();
                assert_eq!(next, thread, "the same thread, kept after the panic");
                assert!(!after_panic, "a panicking step ends its impersonation");
                assert!(!impersonating(), "the runtime's own thread never did");
            });
        }
    }

    /// #3688 — a received file on Linux stays off folders another account
    /// can write: the daemon's own default folder (rule 1), the staging
    /// folders (rule 2), the partial state a resume or the sweep trusts (rule
    /// 3), where a root writer's resume places (rule 4), and the handles a
    /// transfer holds from begin to end (rule 6). CI runs this module as the
    /// runner and again as root (its sudo step), so each test holds for both;
    /// what only root can set up (another account's folder) says so. Every
    /// test stands in the daemon's own home, so none touches a real home,
    /// `/root/Downloads` or the temp dir.
    #[cfg(target_os = "linux")]
    mod own_staging {
        use super::*;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::path::Path;

        pub(super) fn euid() -> u32 {
            // SAFETY: reads our own credentials.
            unsafe { libc::geteuid() }
        }

        /// Root, or a skip; under `ROOMLERD_TEST_REQUIRE_ROOT` (CI's sudo
        /// step) a test that is not root fails instead.
        fn as_root() -> bool {
            let root = euid() == 0;
            if !root && std::env::var_os("ROOMLERD_TEST_REQUIRE_ROOT").is_some() {
                panic!("ROOMLERD_TEST_REQUIRE_ROOT is set and this test is not running as root");
            }
            root
        }

        /// The daemon's own home for one test, stood in on its thread (and on
        /// every step a transfer takes for it): `downloads` makes
        /// `~/Downloads` (0700), `xdg` names it in `~/.config/user-dirs.dirs`.
        pub(super) struct OwnHome {
            _stood: stand_in::OwnHomeStoodIn,
            dir: tempfile::TempDir,
        }

        impl OwnHome {
            pub(super) fn new(downloads: bool, xdg: bool) -> Self {
                let dir = tempfile::tempdir().unwrap();
                // Its owner's own, as a home is, whatever the umask (root's
                // must pass root's rule).
                chmod(dir.path(), 0o700);
                if downloads {
                    std::fs::create_dir(dir.path().join("Downloads")).unwrap();
                    chmod(&dir.path().join("Downloads"), 0o700);
                }
                if xdg {
                    std::fs::create_dir_all(dir.path().join(".config")).unwrap();
                    std::fs::write(
                        dir.path().join(".config/user-dirs.dirs"),
                        "XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n",
                    )
                    .unwrap();
                }
                let stood = stand_in::own_home(Some(dir.path()));
                Self { _stood: stood, dir }
            }

            pub(super) fn path(&self) -> &Path {
                self.dir.path()
            }

            pub(super) fn downloads(&self) -> PathBuf {
                self.dir.path().join("Downloads")
            }
        }

        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        /// The mode bits and the owner of `path` itself, a link not followed.
        fn mode_and_owner(path: &Path) -> (u32, u32) {
            let md = std::fs::symlink_metadata(path).unwrap();
            (md.mode() & 0o7777, md.uid())
        }

        /// What the viewer is told when the default folder is refused.
        fn cannot_receive(folder: &Path, why: &str) -> String {
            format!("{} cannot receive files: {why}", folder.display())
        }

        /// Why a folder others may write is refused: for root, any other
        /// account; for a per-user daemon, every account.
        fn writable_by_others() -> &'static str {
            if euid() == 0 {
                "another account can write to it"
            } else {
                "every account can write to it"
            }
        }

        /// An id of this test run's own.
        fn tid(tag: &str) -> String {
            format!("own-{tag}-{}", std::process::id())
        }

        fn registered(id: &str) -> bool {
            PARTIAL_REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(id)
        }

        fn register(id: &str, meta_path: PathBuf, who: FsAs) {
            PARTIAL_REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.to_string(), PartialEntry { meta_path, who });
        }

        fn forget(ids: &[&str]) {
            let mut g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            for id in ids {
                g.remove(*id);
            }
        }

        /// A partial as a begin leaves one, in `staging`: `<id>` made
        /// `dir_mode`, its `meta.json` and `data` made `file_mode`, its notes
        /// naming `dest`, begun `age` seconds ago.
        fn partial(
            staging: &Path,
            id: &str,
            dest: &Path,
            age: i64,
            dir_mode: u32,
            file_mode: u32,
        ) -> PathBuf {
            let dir = staging.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let meta = PartialMeta {
                protocol_version: 1,
                filename: format!("{id}.bin"),
                expected_size: 8,
                dest_dir: dest.to_path_buf(),
                reserved_final_path: dest.join(format!("{id}.bin")),
                created_at_unix: chrono::Utc::now().timestamp() - age,
                rel_path: None,
                person_sid: None,
            };
            std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
            std::fs::write(dir.join("data"), b"0123").unwrap();
            chmod(&dir.join("meta.json"), file_mode);
            chmod(&dir.join("data"), file_mode);
            chmod(&dir, dir_mode);
            dir
        }

        /// Everything under `root`, each with its length, mode and owner, a
        /// link not followed: a change anywhere under it shows.
        fn tree(root: &Path) -> std::collections::BTreeSet<(PathBuf, u64, u32, u32)> {
            let mut out = std::collections::BTreeSet::new();
            let mut dirs = vec![root.to_path_buf()];
            while let Some(dir) = dirs.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(md) = std::fs::symlink_metadata(&path) else {
                        continue;
                    };
                    if md.is_dir() {
                        dirs.push(path.clone());
                    }
                    out.insert((path, md.len(), md.mode() & 0o7777, md.uid()));
                }
            }
            out
        }

        /// Rule 1 — the daemon's own default folder is never the temp dir.
        /// With no `~/Downloads`, a received file lands there (made when one
        /// arrives); with no home, or a `~/Downloads` every account can
        /// write, it is refused, saying why. Before, both fell back to the
        /// temp dir: on a server, root's was `/tmp`.
        #[tokio::test]
        async fn the_temp_dir_is_never_a_writers_folder() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let temp = std::env::temp_dir();
            {
                let home = OwnHome::new(false, false);
                for who in [FsAs::Daemon, FsAs::DaemonAlone] {
                    assert_eq!(
                        default_download_dir_as(&who, false).ok(),
                        Some(home.downloads()),
                        "{who:?}: the status card names ~/Downloads, never {}",
                        temp.display()
                    );
                }
                assert!(!home.downloads().exists(), "looking made nothing");
            }
            {
                let _no_home = stand_in::own_home(None);
                for who in [FsAs::Daemon, FsAs::DaemonAlone] {
                    let got = download_dir_for(&who).await;
                    assert_eq!(
                        got.as_ref().map_err(|e| e.to_string()).err().as_deref(),
                        Some(
                            "file transfer has no folder to receive files into: this account \
                             has no home folder"
                        ),
                        "{who:?} with no home: {got:?}"
                    );
                }
            }
            let open = OwnHome::new(true, false);
            chmod(&open.downloads(), 0o777);
            for who in [FsAs::Daemon, FsAs::DaemonAlone] {
                let got = download_dir_for(&who).await;
                assert_eq!(
                    got.as_ref().map_err(|e| e.to_string()).err(),
                    Some(cannot_receive(&open.downloads(), writable_by_others())),
                    "{who:?}, a Downloads every account can write: {got:?}"
                );
            }
        }

        /// Rule 1 — with no Downloads, the first received file makes
        /// `~/Downloads`: 0700 and its writer's (root's on a system install).
        #[tokio::test]
        async fn the_fallback_folder_is_made_0700_and_owned_by_its_writer() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            for (n, who) in [FsAs::Daemon, FsAs::DaemonAlone].into_iter().enumerate() {
                let home = OwnHome::new(false, false);
                let lands = download_dir_for(&who).await;
                assert_eq!(
                    lands.as_ref().ok(),
                    Some(&home.downloads()),
                    "{who:?}: ~/Downloads, never the temp dir"
                );
                assert_eq!(
                    mode_and_owner(&home.downloads()),
                    (0o700, euid()),
                    "{who:?}: made 0700, its writer's"
                );
                let h = FilesHandler::new();
                let id = tid(&format!("fallback-{n}"));
                h.begin_as(who.clone(), id.clone(), "a.txt".into(), 2, None, None)
                    .await
                    .expect("begin");
                h.chunk(b"hi").await.expect("chunk");
                let (placed, _) = h.end(&id).await.expect("end");
                assert_eq!(placed, home.downloads().join("a.txt"), "{who:?}: in it");
            }
        }

        /// Rule 1, as R3 rules it — the folder a rung resolves to is checked
        /// for its writer. Every account may write it: refused. Another
        /// account may (group-writable): refused for root, taken for a
        /// per-user daemon, whose `~/Downloads` Ubuntu's private-group umask
        /// makes 0775. Another account's: refused (only root can set that
        /// up). A link is followed, and the folder it leads to is checked.
        #[tokio::test]
        async fn a_downloads_folder_is_checked_by_its_writer() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, false);
            let dl = home.downloads();
            let asks = || default_download_dir_as(&FsAs::Daemon, false).map_err(|e| e.to_string());
            chmod(&dl, 0o770);
            if euid() == 0 {
                assert_eq!(
                    asks(),
                    Err(cannot_receive(&dl, "another account can write to it")),
                    "root, a group-writable Downloads"
                );
            } else {
                assert_eq!(asks(), Ok(dl.clone()), "a per-user daemon, group-writable");
            }
            chmod(&dl, 0o777);
            assert_eq!(
                asks(),
                Err(cannot_receive(&dl, writable_by_others())),
                "world-writable"
            );
            chmod(&dl, 0o700);
            if euid() == 0 {
                let nobody =
                    crate::fs_identity::Account::by_name("nobody").expect("the `nobody` account");
                std::os::unix::fs::chown(&dl, Some(nobody.uid), Some(nobody.gid)).unwrap();
                assert_eq!(
                    asks(),
                    Err(cannot_receive(&dl, "it belongs to another account")),
                    "another account's"
                );
                std::os::unix::fs::chown(&dl, Some(0), Some(0)).unwrap();
            }
            let real = home.path().join("real");
            std::fs::create_dir(&real).unwrap();
            chmod(&real, 0o700);
            std::fs::remove_dir(&dl).unwrap();
            std::os::unix::fs::symlink(&real, &dl).unwrap();
            assert_eq!(asks(), Ok(dl.clone()), "a link to the writer's own folder");
            assert_eq!(
                download_dir_for(&FsAs::Daemon)
                    .await
                    .map_err(|e| e.to_string()),
                Ok(dl.clone()),
                "and a received file lands through it"
            );
            chmod(&real, 0o777);
            assert_eq!(
                asks(),
                Err(cannot_receive(&dl, writable_by_others())),
                "a link to a folder every account can write"
            );
        }

        /// R2 — the daemon's staging is made with explicit modes, so it passes
        /// its own checks under any umask (a per-user daemon on Ubuntu runs
        /// under 0002): `.roomler-partial` and `<id>` 0700, `meta.json` 0600,
        /// `data` 0600 for the daemon alone (#3697) and no wider than 0644
        /// otherwise, every one its stager's. The umask is the process's, so
        /// the test runs again in a child process of its own under 0002, and
        /// no other test runs under it.
        #[tokio::test]
        async fn a_partial_is_made_its_stagers_own_whatever_the_umask() {
            const UNDER_0002: &str = "ROOMLERD_TEST_UNDER_UMASK_0002";
            if std::env::var_os(UNDER_0002).is_none() {
                let this = "files::seat_user_tests::own_staging::\
                            a_partial_is_made_its_stagers_own_whatever_the_umask";
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([this, "--exact", "--test-threads=1"])
                    .env(UNDER_0002, "1")
                    .output()
                    .expect("the test run again in a child");
                let said = String::from_utf8_lossy(&child.stdout);
                assert!(
                    child.status.success() && said.contains("1 passed"),
                    "under umask 0002:\n{said}\n{}",
                    String::from_utf8_lossy(&child.stderr)
                );
                return;
            }
            // SAFETY: sets this child process's own umask; it runs this test
            // alone.
            unsafe { libc::umask(0o002) };
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let staging = home.downloads().join(".roomler-partial");
            for (n, (who, widest)) in [(FsAs::Daemon, 0o644), (FsAs::DaemonAlone, 0o600)]
                .into_iter()
                .enumerate()
            {
                let h = FilesHandler::new();
                let id = tid(&format!("modes-{n}"));
                h.begin_as(who.clone(), id.clone(), "m.bin".into(), 4, None, None)
                    .await
                    .expect("begin");
                let partial = staging.join(&id);
                assert_eq!(
                    mode_and_owner(&staging),
                    (0o700, euid()),
                    "{who:?}: .roomler-partial"
                );
                assert_eq!(mode_and_owner(&partial), (0o700, euid()), "{who:?}: <id>");
                assert_eq!(
                    mode_and_owner(&partial.join("meta.json")),
                    (0o600, euid()),
                    "{who:?}: meta.json"
                );
                let (data, owner) = mode_and_owner(&partial.join("data"));
                assert!(
                    owner == euid() && data & !widest == 0,
                    "{who:?}: data {data:o}, no wider than {widest:o}"
                );
                assert!(h.cancel_incoming(&id).await.expect("a plain id"));
            }
        }

        /// R1 — a `.roomler-partial` that is the writer's own but open to
        /// others (one a per-user daemon made under umask 0002 before this
        /// change) is tightened to 0700 through its handle and used, at begin
        /// and by the sweep.
        #[tokio::test]
        async fn the_writers_own_open_staging_folder_is_tightened_not_refused() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let staging = home.downloads().join(".roomler-partial");
            std::fs::create_dir(&staging).unwrap();
            chmod(&staging, 0o777);
            let h = FilesHandler::new();
            let id = tid("tighten");
            h.begin_as(FsAs::Daemon, id.clone(), "t.bin".into(), 2, None, None)
                .await
                .expect("begin, in the writer's own folder");
            assert_eq!(
                mode_and_owner(&staging),
                (0o700, euid()),
                "tightened at begin"
            );
            h.chunk(b"ok").await.expect("chunk");
            let (placed, _) = h.end(&id).await.expect("end");
            assert_eq!(std::fs::read(&placed).unwrap(), b"ok");
            std::fs::create_dir_all(&staging).unwrap();
            chmod(&staging, 0o777);
            let kept = tid("tighten-kept");
            partial(&staging, &kept, &home.downloads(), 60, 0o700, 0o600);
            sweep_orphans_in(&home.downloads()).await;
            let took = registered(&kept);
            forget(&[&kept]);
            assert_eq!(
                mode_and_owner(&staging),
                (0o700, euid()),
                "tightened by the sweep"
            );
            assert!(took, "and its own partial registered");
        }

        /// Rule 2 — a `.roomler-partial` that is a link is never adopted:
        /// begin refuses, saying why, and makes nothing where it leads; the
        /// sweep neither registers nor removes what is there.
        #[tokio::test]
        async fn a_staging_folder_that_is_a_link_is_refused_at_begin_and_by_the_sweep() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let elsewhere = home.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            chmod(&elsewhere, 0o700);
            let there = tid("link-there");
            partial(&elsewhere, &there, &home.downloads(), 60, 0o700, 0o600);
            let staging = home.downloads().join(".roomler-partial");
            std::os::unix::fs::symlink(&elsewhere, &staging).unwrap();
            let before = tree(&elsewhere);
            let h = FilesHandler::new();
            let id = tid("link-begin");
            let begun = h
                .begin_as(FsAs::Daemon, id.clone(), "l.bin".into(), 2, None, None)
                .await;
            h.abort().await;
            assert_eq!(
                begun.map_err(|e| e.to_string()),
                Err(format!(
                    "{} is not a folder, so file transfer does not stage there",
                    staging.display()
                ))
            );
            assert_eq!(
                tree(&elsewhere),
                before,
                "nothing made where the link leads"
            );
            let swept = sweep_orphans_in(&home.downloads()).await;
            let took = registered(&there);
            forget(&[&there, &id]);
            assert_eq!(swept, (0, 0), "the sweep takes nothing there");
            assert!(!took, "nor registers it");
            assert_eq!(tree(&elsewhere), before, "nor removes it");
        }

        /// Rule 2 — a `.roomler-partial` another account owns is never
        /// adopted: begin refuses, saying why, and changes nothing in it; the
        /// sweep neither registers nor removes the partial in it. Only root
        /// can give a folder to another account.
        #[tokio::test]
        async fn a_staging_folder_another_account_owns_is_refused_at_begin_and_by_the_sweep() {
            if !as_root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let nobody =
                crate::fs_identity::Account::by_name("nobody").expect("the `nobody` account");
            let staging = home.downloads().join(".roomler-partial");
            std::fs::create_dir(&staging).unwrap();
            chmod(&staging, 0o700);
            let theirs = tid("theirs");
            let in_theirs = partial(&staging, &theirs, &home.downloads(), 60, 0o700, 0o600);
            for p in [
                staging.clone(),
                in_theirs.clone(),
                in_theirs.join("meta.json"),
                in_theirs.join("data"),
            ] {
                std::os::unix::fs::chown(&p, Some(nobody.uid), Some(nobody.gid)).unwrap();
            }
            let before = tree(&staging);
            let h = FilesHandler::new();
            let id = tid("theirs-begin");
            let begun = h
                .begin_as(FsAs::DaemonAlone, id.clone(), "o.bin".into(), 2, None, None)
                .await;
            h.abort().await;
            assert_eq!(
                begun.map_err(|e| e.to_string()),
                Err(format!(
                    "{} belongs to another account, so file transfer does not stage there",
                    staging.display()
                ))
            );
            assert_eq!(tree(&staging), before, "nothing changed in it");
            assert_eq!(
                mode_and_owner(&staging),
                (0o700, nobody.uid),
                "nor its mode or owner"
            );
            let swept = sweep_orphans_dir_as(&FsAs::DaemonAlone, staging.clone()).await;
            let took = registered(&theirs);
            forget(&[&theirs, &id]);
            assert_eq!(swept, (0, 0), "the sweep takes nothing there");
            assert!(!took, "nor registers it");
            assert_eq!(tree(&staging), before, "nor removes anything");
        }

        /// Rule 3 — a resume opens `data` and `meta.json` without following
        /// links: a link at either is refused as no partial state (the viewer
        /// starts over), and what it leads to is untouched.
        #[tokio::test]
        async fn a_link_at_data_or_meta_json_is_refused_on_resume() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let staging = home.downloads().join(".roomler-partial");
            let elsewhere = home.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            let outside = home.path().join("elsewhere.txt");
            std::fs::write(&outside, b"a file of its own").unwrap();
            let h = FilesHandler::new();
            let at_data = tid("link-data");
            h.begin_as(FsAs::Daemon, at_data.clone(), "d.bin".into(), 8, None, None)
                .await
                .expect("begin");
            h.chunk(b"1234").await.expect("chunk");
            h.abort().await;
            let data = staging.join(&at_data).join("data");
            std::fs::remove_file(&data).unwrap();
            std::os::unix::fs::symlink(&outside, &data).unwrap();
            let resumed = h.resume_incoming_as(FsAs::Daemon, &at_data, 0).await;
            h.abort().await;
            let elsewhere_after = std::fs::read(&outside).unwrap();
            let at_meta = tid("link-meta");
            h.begin_as(FsAs::Daemon, at_meta.clone(), "m.bin".into(), 8, None, None)
                .await
                .expect("begin");
            h.abort().await;
            let meta = staging.join(&at_meta).join("meta.json");
            let mut notes: PartialMeta =
                serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
            notes.dest_dir = elsewhere.clone();
            let other_notes = elsewhere.join("notes.json");
            std::fs::write(&other_notes, serde_json::to_vec(&notes).unwrap()).unwrap();
            std::fs::remove_file(&meta).unwrap();
            std::os::unix::fs::symlink(&other_notes, &meta).unwrap();
            let resumed_meta = h.resume_incoming_as(FsAs::Daemon, &at_meta, 0).await;
            h.abort().await;
            let _ = h.cancel_incoming(&at_data).await;
            let _ = h.cancel_incoming(&at_meta).await;
            forget(&[&at_data, &at_meta]);
            assert_eq!(
                resumed.map_err(|e| e.to_string()),
                Err(format!("no partial state for id {at_data}")),
                "a link at data"
            );
            assert_eq!(
                elsewhere_after, b"a file of its own",
                "what it leads to, untouched"
            );
            assert_eq!(
                resumed_meta.map_err(|e| e.to_string()),
                Err(format!("no partial state for id {at_meta}")),
                "a link at meta.json"
            );
            assert_eq!(
                std::fs::read_dir(&elsewhere).unwrap().count(),
                1,
                "nothing placed where those notes say"
            );
        }

        /// Rule 3, R1 — a resume trusts a partial only when it is its
        /// stager's own and nobody else may write it. One another account can
        /// write (made under umask 0002 before this change, say) is refused
        /// once and stays as it was; the viewer starts that upload over.
        #[tokio::test]
        async fn a_resume_of_a_partial_another_account_can_write_is_refused() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let h = FilesHandler::new();
            let id = tid("open-resume");
            h.begin_as(FsAs::Daemon, id.clone(), "o.bin".into(), 8, None, None)
                .await
                .expect("begin");
            h.chunk(b"1234").await.expect("chunk");
            h.abort().await;
            let partial = home.downloads().join(".roomler-partial").join(&id);
            chmod(&partial, 0o770);
            let resumed = h.resume_incoming_as(FsAs::Daemon, &id, 0).await;
            h.abort().await;
            let left = (mode_and_owner(&partial).0, partial.join("data").exists());
            let _ = h.cancel_incoming(&id).await;
            forget(&[&id]);
            assert_eq!(
                resumed.map_err(|e| e.to_string()),
                Err(format!("no partial state for id {id}"))
            );
            assert_eq!(left, (0o770, true), "left as it was");
        }

        /// Rule 3, R1 — the startup sweep registers a partial only when its
        /// folder and both files are its stager's own and nobody else may
        /// write them, opened without following links. The stager's own
        /// folder that fails that is never registered, and goes once it is
        /// over a day old; a link is left as it is, and so is what it leads to.
        #[tokio::test]
        async fn the_sweep_trusts_only_partials_that_are_their_stagers_own() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            let staging = dl.join(".roomler-partial");
            std::fs::create_dir(&staging).unwrap();
            chmod(&staging, 0o700);
            let elsewhere = home.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            let hour = 3600;
            let own = tid("sweep-own");
            partial(&staging, &own, &dl, hour, 0o700, 0o600);
            let open = tid("sweep-open");
            partial(&staging, &open, &dl, hour, 0o770, 0o600);
            let open_stale = tid("sweep-open-stale");
            let stale = partial(&staging, &open_stale, &dl, 25 * hour, 0o770, 0o600);
            std::fs::File::open(&stale)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600),
                )
                .unwrap();
            let notes_link = tid("sweep-notes-link");
            let with_link = partial(&staging, &notes_link, &dl, hour, 0o700, 0o600);
            let notes = elsewhere.join("notes.json");
            std::fs::rename(with_link.join("meta.json"), &notes).unwrap();
            std::os::unix::fs::symlink(&notes, with_link.join("meta.json")).unwrap();
            let id_link = tid("sweep-id-link");
            let real = tid("sweep-real");
            partial(&elsewhere, &real, &dl, hour, 0o700, 0o600);
            std::os::unix::fs::symlink(elsewhere.join(&real), staging.join(&id_link)).unwrap();
            let outside = tree(&elsewhere);

            let counts = sweep_orphans_in(&dl).await;
            let mine = [
                own.as_str(),
                open.as_str(),
                open_stale.as_str(),
                notes_link.as_str(),
                id_link.as_str(),
            ];
            let took: Vec<&str> = mine.iter().copied().filter(|id| registered(id)).collect();
            forget(&mine);
            assert_eq!(took, vec![own.as_str()], "only the stager's own partial");
            assert_eq!(counts, (1, 1), "(kept, swept)");
            assert!(
                staging.join(&open).exists(),
                "a fresh one that fails is left"
            );
            assert!(
                !stale.exists(),
                "a stale one of its own goes, whatever its mode"
            );
            assert!(
                with_link.join("meta.json").is_symlink(),
                "a link at meta.json is left"
            );
            assert!(
                staging.join(&id_link).is_symlink(),
                "a link for a partial is left"
            );
            assert_eq!(tree(&elsewhere), outside, "and nothing where a link leads");
        }

        /// Rule 4 — a root writer's resume never takes its folder from disk.
        /// With nobody at the screen, the folder a partial's notes record must
        /// be the one it sits beside, and that must be the default folder the
        /// rule gives now. Notes naming another folder, or a partial beside
        /// another folder, are refused as no partial state, and nothing lands
        /// there.
        #[tokio::test]
        async fn a_resume_whose_recorded_folder_is_not_the_rules_folder_is_refused() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let elsewhere = home.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            chmod(&elsewhere, 0o700);
            let h = FilesHandler::new();
            let renamed = tid("rule4-notes");
            h.begin_as(
                FsAs::DaemonAlone,
                renamed.clone(),
                "r.bin".into(),
                8,
                None,
                None,
            )
            .await
            .expect("begin");
            h.chunk(b"1234").await.expect("chunk");
            h.abort().await;
            let meta = home
                .downloads()
                .join(".roomler-partial")
                .join(&renamed)
                .join("meta.json");
            let mut notes: PartialMeta =
                serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
            notes.dest_dir = elsewhere.clone();
            // The same file, so the same owner and mode: only what it says changed.
            std::fs::write(&meta, serde_json::to_vec(&notes).unwrap()).unwrap();
            let named_elsewhere = h.resume_incoming_as(FsAs::DaemonAlone, &renamed, 0).await;
            h.abort().await;
            let beside = tid("rule4-beside");
            let estaging = elsewhere.join(".roomler-partial");
            std::fs::create_dir(&estaging).unwrap();
            chmod(&estaging, 0o700);
            let p = partial(&estaging, &beside, &elsewhere, 60, 0o700, 0o600);
            register(&beside, p.join("meta.json"), FsAs::DaemonAlone);
            let beside_elsewhere = h.resume_incoming_as(FsAs::DaemonAlone, &beside, 0).await;
            h.abort().await;
            let landed = std::fs::read_dir(&elsewhere).unwrap().count();
            let _ = h.cancel_incoming(&renamed).await;
            forget(&[&renamed, &beside]);
            assert_eq!(
                named_elsewhere.map_err(|e| e.to_string()),
                Err(format!("no partial state for id {renamed}")),
                "notes naming another folder"
            );
            assert_eq!(
                beside_elsewhere.map_err(|e| e.to_string()),
                Err(format!("no partial state for id {beside}")),
                "a partial beside another folder"
            );
            assert_eq!(landed, 1, "nothing landed there but its staging");
        }

        /// Rule 6 — a transfer holds its staging by handles from begin to end.
        /// Its `.roomler-partial`, or its `<id>`, moved aside mid-upload with
        /// another put in its place: the file placed is still the one
        /// received, never what was put there.
        #[tokio::test]
        async fn a_staging_folder_swapped_mid_upload_is_not_followed() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            let staging = dl.join(".roomler-partial");
            for (n, whole) in [true, false].into_iter().enumerate() {
                let h = FilesHandler::new();
                let id = tid(&format!("swap-{n}"));
                let name = format!("swap-{n}.txt");
                h.begin_as(FsAs::Daemon, id.clone(), name.clone(), 14, None, None)
                    .await
                    .expect("begin");
                h.chunk(b"the real bytes").await.expect("chunk");
                let (moved, aside, notes) = if whole {
                    let aside = dl.join(format!(".aside-{n}"));
                    (
                        staging.clone(),
                        aside.clone(),
                        aside.join(&id).join("meta.json"),
                    )
                } else {
                    let aside = staging.join(format!("{id}.aside"));
                    (staging.join(&id), aside.clone(), aside.join("meta.json"))
                };
                std::fs::rename(&moved, &aside).unwrap();
                let other = staging.join(&id);
                std::fs::create_dir_all(&other).unwrap();
                std::fs::write(other.join("data"), b"another file!!").unwrap();
                std::fs::copy(&notes, other.join("meta.json")).unwrap();
                let ended = h.end(&id).await;
                let _ = std::fs::remove_dir_all(&aside);
                let _ = std::fs::remove_dir_all(&staging);
                forget(&[&id]);
                let (placed, bytes) = ended.expect("end");
                assert_eq!(
                    (bytes, std::fs::read(&placed).unwrap()),
                    (14, b"the real bytes".to_vec()),
                    "{} swapped: the file received",
                    if whole { ".roomler-partial" } else { "<id>" }
                );
                assert_eq!(placed, dl.join(&name));
            }
        }

        /// Rule 6 — a root writer's folder upload makes and opens each
        /// subfolder from the folder it holds, never through a link: a
        /// subfolder that is a link refuses, and nothing lands where it leads.
        /// A per-user daemon still follows its own links with its own rights
        /// (the control; as root, the daemon is a root writer too).
        #[tokio::test]
        async fn a_folder_upload_follows_no_link_for_root() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let elsewhere = home.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            let link = home.downloads().join("Shared");
            std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
            let h = FilesHandler::new();
            let id = tid("rel-link");
            h.begin_as(
                FsAs::DaemonAlone,
                id.clone(),
                "x.txt".into(),
                2,
                Some("Shared/x.txt"),
                None,
            )
            .await
            .expect("begin");
            h.chunk(b"hi").await.expect("chunk");
            let ended = h.end(&id).await;
            let landed = std::fs::read_dir(&elsewhere).unwrap().count();
            let _ = h.cancel_incoming(&id).await;
            forget(&[&id]);
            assert_eq!(
                ended.map_err(|e| e.to_string()),
                Err(format!(
                    "{} is not a folder, so file transfer does not place the file there",
                    link.display()
                ))
            );
            assert_eq!(landed, 0, "nothing landed where the link leads");
            if euid() != 0 {
                let id = tid("rel-link-own");
                h.begin_as(
                    FsAs::Daemon,
                    id.clone(),
                    "y.txt".into(),
                    2,
                    Some("Shared/y.txt"),
                    None,
                )
                .await
                .expect("begin");
                h.chunk(b"hi").await.expect("chunk");
                let (placed, _) = h
                    .end(&id)
                    .await
                    .expect("a per-user daemon follows its own link");
                assert_eq!(placed, link.join("y.txt"));
                assert!(elsewhere.join("y.txt").exists());
            }
        }

        /// A partial of `who`'s own, as [`partial`] makes it, given to `who`.
        fn partial_of(
            who: &crate::fs_identity::Account,
            staging: &Path,
            id: &str,
            dest: &Path,
            age: i64,
        ) -> PathBuf {
            let dir = partial(staging, id, dest, age, 0o700, 0o600);
            for p in [dir.clone(), dir.join("meta.json"), dir.join("data")] {
                std::os::unix::fs::chown(&p, Some(who.uid), Some(who.gid)).unwrap();
            }
            dir
        }

        /// The configured folder, put back to unset when it drops.
        struct Configured;

        impl Configured {
            fn set(folder: &Path) -> Self {
                set_files_dir(Some(folder.to_string_lossy().into_owned()));
                Self
            }
        }

        impl Drop for Configured {
            fn drop(&mut self) {
                set_files_dir(None);
            }
        }

        /// R4 — a per-user daemon whose Downloads sits on a file system that
        /// ignores Unix owners and modes, where every folder reads 0777: the
        /// folder is not refused for reading world-writable, the upload
        /// stages in the daemon's own private folder
        /// (`~/.local/state/roomler/staging`), not beside it, and the end
        /// copies the file across. A resume after a restart and the startup
        /// sweep find it there. The temp dir is still refused. As root, the
        /// daemon is root, which stays strict
        /// (`root_stays_strict_where_modes_are_not_kept`).
        #[tokio::test]
        async fn a_per_user_daemons_folder_that_keeps_no_modes_receives_by_copy() {
            if euid() == 0 {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            chmod(&dl, 0o777);
            let _fat = stand_in::modes_kept(Some(false));
            assert_eq!(
                default_download_dir_as(&FsAs::Daemon, false).map_err(|e| e.to_string()),
                Ok(dl.clone()),
                "not refused for reading world-writable"
            );
            let private = home.path().join(".local/state/roomler/staging");
            let h = FilesHandler::new();
            let id = tid("fat-user");
            h.begin_as(FsAs::Daemon, id.clone(), "f.txt".into(), 4, None, None)
                .await
                .expect("begin");
            let partial_dir = private.join(&id);
            let staged = (
                private.is_dir().then(|| mode_and_owner(&private)),
                partial_dir.is_dir().then(|| mode_and_owner(&partial_dir)),
                partial_dir.join("data").is_file(),
                dl.join(".roomler-partial").exists(),
            );
            h.chunk(b"fat!").await.expect("chunk");
            let ended = h.end(&id).await;
            forget(&[&id]);
            assert_eq!(
                staged,
                (Some((0o700, euid())), Some((0o700, euid())), true, false),
                "staged in its own private folder, 0700, nothing beside the target"
            );
            let (placed, _) = ended.expect("end");
            assert_eq!(placed, dl.join("f.txt"));
            assert_eq!(std::fs::read(&placed).unwrap(), b"fat!", "copied across");
            assert!(!partial_dir.exists(), "its staging goes");

            let again = tid("fat-user-resume");
            h.begin_as(FsAs::Daemon, again.clone(), "g.txt".into(), 8, None, None)
                .await
                .expect("begin");
            h.chunk(b"1234").await.expect("chunk");
            h.abort().await;
            forget(&[&again]);
            let resumed = h.resume_incoming_as(FsAs::Daemon, &again, 0).await;
            assert_eq!(
                resumed.as_ref().map_err(|e| e.to_string()),
                Ok(&0),
                "a resume after a restart finds it in the private folder"
            );
            h.chunk(b"12345678").await.expect("chunk");
            let (placed, _) = h.end(&again).await.expect("end");
            assert_eq!(std::fs::read(&placed).unwrap(), b"12345678");

            let fresh = tid("fat-user-fresh");
            partial(&private, &fresh, &dl, 3600, 0o700, 0o600);
            let stale = tid("fat-user-stale");
            partial(&private, &stale, &dl, 25 * 3600, 0o700, 0o600);
            sweep_orphans().await;
            let took = registered(&fresh);
            forget(&[&fresh, &stale]);
            assert!(took, "the sweep registers a fresh one there");
            assert!(!private.join(&stale).exists(), "and removes a stale one");

            let real = home.path().join("Downloads.real");
            std::fs::rename(&dl, &real).unwrap();
            std::os::unix::fs::symlink(std::env::temp_dir(), &dl).unwrap();
            assert_eq!(
                default_download_dir_as(&FsAs::Daemon, false).map_err(|e| e.to_string()),
                Err(cannot_receive(&dl, "it is the temp folder")),
                "the temp dir stays refused"
            );
        }

        /// R4 — root stays strict where modes are not kept: a default folder
        /// others can write is still refused. One that passes its rule stages
        /// in root's own private folder (`/var/lib/roomler/staging`, stood in
        /// under the test's home), 0700 and root's, `data` 0600 for the daemon
        /// alone; the end copies the file across, and a resume after a
        /// restart and the sweep find it there.
        #[tokio::test]
        async fn root_stays_strict_where_modes_are_not_kept() {
            if !as_root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            let _fat = stand_in::modes_kept(Some(false));
            chmod(&dl, 0o777);
            for who in [FsAs::Daemon, FsAs::DaemonAlone] {
                assert_eq!(
                    download_dir_for(&who).await.map_err(|e| e.to_string()),
                    Err(cannot_receive(&dl, "another account can write to it")),
                    "{who:?}: refused"
                );
            }
            chmod(&dl, 0o700);
            let private = home.path().join("var/lib/roomler/staging");
            let h = FilesHandler::new();
            for (n, (who, mode)) in [(FsAs::Daemon, 0o644), (FsAs::DaemonAlone, 0o600)]
                .into_iter()
                .enumerate()
            {
                let id = tid(&format!("fat-root-{n}"));
                let name = format!("r{n}.txt");
                h.begin_as(who.clone(), id.clone(), name.clone(), 4, None, None)
                    .await
                    .expect("begin");
                let partial_dir = private.join(&id);
                let data = partial_dir.join("data");
                let staged = (
                    private.is_dir().then(|| mode_and_owner(&private)),
                    partial_dir.is_dir().then(|| mode_and_owner(&partial_dir)),
                    data.is_file().then(|| mode_and_owner(&data)),
                    dl.join(".roomler-partial").exists(),
                );
                h.chunk(b"root").await.expect("chunk");
                let ended = h.end(&id).await;
                forget(&[&id]);
                assert_eq!(
                    staged,
                    (Some((0o700, 0)), Some((0o700, 0)), Some((mode, 0)), false),
                    "{who:?}: staged in root's own private folder"
                );
                let (placed, _) = ended.expect("end");
                assert_eq!(placed, dl.join(&name));
                assert_eq!(
                    (std::fs::read(&placed).unwrap(), mode_and_owner(&placed)),
                    (b"root".to_vec(), (mode, 0)),
                    "{who:?}: copied across"
                );
                assert!(!partial_dir.exists(), "{who:?}: its staging goes");
            }
            let again = tid("fat-root-resume");
            h.begin_as(
                FsAs::DaemonAlone,
                again.clone(),
                "s.txt".into(),
                8,
                None,
                None,
            )
            .await
            .expect("begin");
            h.chunk(b"1234").await.expect("chunk");
            h.abort().await;
            forget(&[&again]);
            let resumed = h.resume_incoming_as(FsAs::DaemonAlone, &again, 0).await;
            assert_eq!(
                resumed.as_ref().map_err(|e| e.to_string()),
                Ok(&0),
                "a resume after a restart finds it in root's private folder"
            );
            h.chunk(b"12345678").await.expect("chunk");
            let (placed, _) = h.end(&again).await.expect("end");
            assert_eq!(std::fs::read(&placed).unwrap(), b"12345678");
            let fresh = tid("fat-root-fresh");
            partial(&private, &fresh, &dl, 3600, 0o700, 0o600);
            let stale = tid("fat-root-stale");
            partial(&private, &stale, &dl, 25 * 3600, 0o700, 0o600);
            let counts = sweep_orphans_dir_as(&FsAs::DaemonAlone, private.clone()).await;
            let took = registered(&fresh);
            forget(&[&fresh, &stale]);
            assert_eq!((counts, took), ((1, 1), true), "the sweep covers it");
        }

        /// R4 — the person at the screen, whose Downloads sits on a file
        /// system that ignores Unix owners and modes (a disk root mounted:
        /// every folder there reads root's, 0777). The upload stages in THEIR
        /// own private folder (`~/.local/state/roomler/staging`), made as
        /// them, and the end copies the file across as them. A resume after a
        /// restart and the sweep find it there.
        #[tokio::test]
        async fn a_persons_folder_that_keeps_no_modes_receives_by_copy() {
            if !as_root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let (home, person) = super::as_root::nobodys_home();
            let dl = home.path().join("Downloads");
            std::fs::create_dir(&dl).unwrap();
            chmod(&dl, 0o777);
            let who = super::as_root::as_person(&person, home.path());
            let _fat = stand_in::modes_kept(Some(false));
            let private = home.path().join(".local/state/roomler/staging");
            let h = FilesHandler::new();
            let id = tid("fat-person");
            h.begin_as(who.clone(), id.clone(), "p.txt".into(), 4, None, None)
                .await
                .expect("begin");
            let partial_dir = private.join(&id);
            let data = partial_dir.join("data");
            let staged = (
                private.is_dir().then(|| mode_and_owner(&private)),
                partial_dir.is_dir().then(|| mode_and_owner(&partial_dir)),
                data.is_file().then(|| mode_and_owner(&data)),
                dl.join(".roomler-partial").exists(),
            );
            h.chunk(b"mine").await.expect("chunk");
            let ended = h.end(&id).await;
            forget(&[&id]);
            assert_eq!(
                staged,
                (
                    Some((0o700, person.uid)),
                    Some((0o700, person.uid)),
                    Some((0o644, person.uid)),
                    false
                ),
                "staged in their own private folder, as them"
            );
            let (placed, _) = ended.expect("end");
            assert_eq!(placed, dl.join("p.txt"));
            assert_eq!(
                (std::fs::read(&placed).unwrap(), mode_and_owner(&placed).1),
                (b"mine".to_vec(), person.uid),
                "copied across, theirs"
            );
            assert!(!partial_dir.exists(), "its staging goes");
            let again = tid("fat-person-resume");
            h.begin_as(who.clone(), again.clone(), "q.txt".into(), 8, None, None)
                .await
                .expect("begin");
            h.chunk(b"1234").await.expect("chunk");
            h.abort().await;
            forget(&[&again]);
            let resumed = h.resume_incoming_as(who.clone(), &again, 0).await;
            assert_eq!(
                resumed.as_ref().map_err(|e| e.to_string()),
                Ok(&0),
                "a resume after a restart finds it in their private folder"
            );
            h.chunk(b"12345678").await.expect("chunk");
            let (placed, _) = h.end(&again).await.expect("end");
            assert_eq!(std::fs::read(&placed).unwrap(), b"12345678");
            let fresh = tid("fat-person-fresh");
            partial_of(&person, &private, &fresh, &dl, 3600);
            let stale = tid("fat-person-stale");
            partial_of(&person, &private, &stale, &dl, 25 * 3600);
            let counts = sweep_orphans_dir_as(&who, private.clone()).await;
            let took = registered(&fresh);
            forget(&[&fresh, &stale]);
            assert_eq!((counts, took), ((1, 1), true), "the sweep covers it");
        }

        /// R4 finding 2 — a root daemon takes its home from its account
        /// (`getpwuid`), never from the `HOME` its environment names (`sudo
        /// -E`, or a drop-in that sets it): another account's home there, with
        /// a `user-dirs.dirs` naming a folder of root's outside it, chooses
        /// nothing. Its own home must pass root's rule, and its
        /// `user-dirs.dirs` counts only when it is root's own plain file
        /// nobody else may write, and only for a folder inside that home.
        #[tokio::test]
        async fn a_root_daemon_takes_its_home_from_its_account_not_its_environment() {
            if !as_root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let roots = OwnHome::new(true, false);
            let (foreign, person) = super::as_root::nobodys_home();
            let picked = tempfile::tempdir().unwrap();
            chmod(picked.path(), 0o700);
            let names = |home: &Path, value: &str| {
                std::fs::create_dir_all(home.join(".config")).unwrap();
                let file = home.join(".config/user-dirs.dirs");
                let _ = std::fs::remove_file(&file);
                std::fs::write(&file, format!("XDG_DOWNLOAD_DIR=\"{value}\"\n")).unwrap();
                file
            };
            let file = names(foreign.path(), &picked.path().to_string_lossy());
            for p in [foreign.path().join(".config"), file] {
                std::os::unix::fs::chown(&p, Some(person.uid), Some(person.gid)).unwrap();
            }
            let _env = stand_in::env_home(Some(foreign.path()));
            let asks =
                || default_download_dir_as(&FsAs::DaemonAlone, false).map_err(|e| e.to_string());
            assert_eq!(
                asks(),
                Ok(roots.downloads()),
                "the environment's home and what it names choose nothing"
            );
            let inside = roots.path().join("Inside");
            std::fs::create_dir(&inside).unwrap();
            chmod(&inside, 0o700);
            names(roots.path(), "$HOME/Inside");
            assert_eq!(asks(), Ok(inside.clone()), "its own user-dirs.dirs, inside");
            names(roots.path(), &picked.path().to_string_lossy());
            assert_eq!(asks(), Ok(roots.downloads()), "outside its home: not taken");
            let elsewhere = roots.path().join("dirs-elsewhere");
            std::fs::write(&elsewhere, "XDG_DOWNLOAD_DIR=\"$HOME/Inside\"\n").unwrap();
            let file = roots.path().join(".config/user-dirs.dirs");
            std::fs::remove_file(&file).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
            assert_eq!(asks(), Ok(roots.downloads()), "a link there: not read");
            let file = names(roots.path(), "$HOME/Inside");
            std::os::unix::fs::chown(&file, Some(person.uid), Some(person.gid)).unwrap();
            assert_eq!(
                asks(),
                Ok(roots.downloads()),
                "another account's file there: not read"
            );
            chmod(roots.path(), 0o777);
            assert_eq!(
                asks(),
                Err(cannot_receive(
                    roots.path(),
                    "another account can write to it"
                )),
                "a home others can write: refused"
            );
            chmod(roots.path(), 0o700);
        }

        /// R4 finding 3 — another upload's end removes an empty
        /// `.roomler-partial`. Landing after a begin opened it and before it
        /// made its upload's folder there, it costs nothing: the begin makes
        /// and opens it again, and goes on.
        #[tokio::test]
        async fn a_begin_whose_staging_folder_goes_before_its_id_is_made_makes_it_again() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            stand_in::staging_goes_before_the_id_is_made(&dl);
            let h = FilesHandler::new();
            let id = tid("staging-goes");
            let begun = h
                .begin_as(FsAs::Daemon, id.clone(), "s.txt".into(), 2, None, None)
                .await;
            assert_eq!(
                begun.as_ref().map(drop).map_err(|e| format!("{e:#}")),
                Ok(()),
                "begin"
            );
            h.chunk(b"ok").await.expect("chunk");
            let (placed, _) = h.end(&id).await.expect("end");
            assert_eq!(std::fs::read(&placed).unwrap(), b"ok");
            assert!(
                !dl.join(".roomler-partial").exists(),
                "an empty one still goes at the end"
            );
        }

        /// R4 finding 4 — begin passes down that its target is the daemon's
        /// own default folder, and the very handle its staging opens is held
        /// to that folder's rule: a `~/Downloads` swapped, after it was
        /// resolved, for a link to a folder others can write is refused there,
        /// saying why, and nothing is staged in it. (The handle was checked
        /// only when resolving the default folder again gave the same path,
        /// which the swap made fail.)
        #[tokio::test]
        async fn a_default_folder_swapped_before_its_staging_is_held_to_its_rule() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            let open = home.path().join("open");
            std::fs::create_dir(&open).unwrap();
            chmod(&open, 0o777);
            stand_in::swap_before_open(&dl, &open);
            let h = FilesHandler::new();
            let id = tid("swapped-default");
            let begun = h
                .begin_as(FsAs::Daemon, id.clone(), "w.txt".into(), 2, None, None)
                .await;
            h.abort().await;
            forget(&[&id]);
            assert_eq!(
                begun.map_err(|e| e.to_string()),
                Err(cannot_receive(&dl, writable_by_others()))
            );
            assert_eq!(
                std::fs::read_dir(&open).unwrap().count(),
                0,
                "nothing staged in it"
            );
        }

        /// Rule 4, which R4 found untested for a daemon that is root — by
        /// the drop-in (the switch off), or with root itself at the screen:
        /// it places a received file in the configured folder, or in a folder
        /// a controller chose, as each resolves now, at the end and on a
        /// resume.
        #[tokio::test]
        async fn a_root_daemon_places_in_the_configured_or_a_chosen_folder() {
            if !as_root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let drop_in = linux_fs_as(true, false, false, || unreachable!("no seat asked"));
            let at_the_screen = linux_fs_as(true, true, false, || Seat::Root);
            assert_eq!((&drop_in, &at_the_screen), (&FsAs::Daemon, &FsAs::Daemon));
            let configured = home.path().join("configured");
            let chosen = home.path().join("chosen");
            for d in [&configured, &chosen] {
                std::fs::create_dir(d).unwrap();
                chmod(d, 0o700);
            }
            let _configured = Configured::set(&configured);
            let chosen_s = chosen.to_string_lossy().into_owned();
            let h = FilesHandler::new();
            for (n, who) in [drop_in, at_the_screen].into_iter().enumerate() {
                for (dest, lands) in [(None, &configured), (Some(chosen_s.as_str()), &chosen)] {
                    let id = tid(&format!("root-places-{n}-{}", dest.is_some()));
                    let name = format!("{id}.txt");
                    h.begin_as(who.clone(), id.clone(), name.clone(), 4, None, dest)
                        .await
                        .expect("begin");
                    h.chunk(b"1234").await.expect("chunk");
                    let ended = h.end(&id).await;
                    forget(&[&id]);
                    assert_eq!(
                        ended.map(|(p, _)| p).map_err(|e| e.to_string()),
                        Ok(lands.join(&name)),
                        "{n}: at the end"
                    );
                    let again = format!("{id}-resumed");
                    let name = format!("{again}.txt");
                    h.begin_as(who.clone(), again.clone(), name.clone(), 8, None, dest)
                        .await
                        .expect("begin");
                    h.chunk(b"1234").await.expect("chunk");
                    h.abort().await;
                    let resumed = h.resume_incoming_as(who.clone(), &again, 0).await;
                    assert_eq!(
                        resumed.as_ref().map_err(|e| e.to_string()),
                        Ok(&0),
                        "{n}: resumed"
                    );
                    h.chunk(b"12345678").await.expect("chunk");
                    let ended = h.end(&again).await;
                    forget(&[&again]);
                    assert_eq!(
                        ended.map(|(p, _)| p).map_err(|e| e.to_string()),
                        Ok(lands.join(&name)),
                        "{n}: after a resume"
                    );
                }
            }
        }

        /// Rule 4, which R4 found untested — an end places only in a folder
        /// the rule gives a root writer now: its default folder opened to
        /// other accounts mid-upload refuses the end, saying why, and the
        /// partial survives it, so a resume once the folder is the daemon's
        /// own again completes the upload.
        #[tokio::test]
        async fn an_end_the_rule_refuses_keeps_its_partial_for_a_resume() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            let h = FilesHandler::new();
            let id = tid("end-refused");
            h.begin_as(FsAs::DaemonAlone, id.clone(), "e.txt".into(), 4, None, None)
                .await
                .expect("begin");
            h.chunk(b"1234").await.expect("chunk");
            chmod(&dl, 0o777);
            let ended = h.end(&id).await;
            chmod(&dl, 0o700);
            let partial_dir = dl.join(".roomler-partial").join(&id);
            let survived = (
                std::fs::read(partial_dir.join("data")).ok(),
                partial_dir.join("meta.json").is_file(),
                registered(&id),
                dl.join("e.txt").exists(),
            );
            let resumed = h.resume_incoming_as(FsAs::DaemonAlone, &id, 4).await;
            let placed = match &resumed {
                Ok(_) => {
                    h.chunk(b"1234").await.expect("chunk");
                    h.end(&id).await.map(|(p, _)| p).map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            forget(&[&id]);
            assert_eq!(
                ended.map(|(p, _)| p).map_err(|e| e.to_string()),
                Err(format!(
                    "{} is not a folder this computer receives files into now ({}), so the \
                     file is not placed there",
                    dl.display(),
                    cannot_receive(&dl, writable_by_others())
                ))
            );
            assert_eq!(
                survived,
                (Some(b"1234".to_vec()), true, true, false),
                "the partial survives it"
            );
            assert_eq!(
                resumed.map_err(|e| e.to_string()),
                Ok(0),
                "and resumes, from 0"
            );
            assert_eq!(placed, Ok(dl.join("e.txt")));
        }

        /// #3688 — a per-user daemon whose Downloads sits on a file system that
        /// keeps no Unix owners either: a VirtualBox shared folder reads
        /// `root:vboxsf`, a share mounted with a fixed owner reads that one.
        /// A folder's owner says nothing there, so the daemon's default folder
        /// is not refused for reading another account's when the daemon can
        /// write into it (here a folder root owns that every account may
        /// write, reached through `~/Downloads`): the upload stages in the
        /// daemon's own private folder, and the end copies the file across.
        /// Another account's folder it cannot write into (`XDG_DOWNLOAD_DIR`
        /// naming `/usr`) is passed over. As root, the daemon is root, which
        /// stays strict: another account's folder is refused.
        #[tokio::test]
        async fn a_per_user_daemons_folder_of_another_owner_keeping_no_modes_receives_by_copy() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(false, false);
            let dl = home.downloads();
            let _fat = stand_in::modes_kept(Some(false));
            if as_root() {
                let nobody =
                    crate::fs_identity::Account::by_name("nobody").expect("the `nobody` account");
                std::fs::create_dir(&dl).unwrap();
                chmod(&dl, 0o700);
                std::os::unix::fs::chown(&dl, Some(nobody.uid), Some(nobody.gid)).unwrap();
                for who in [FsAs::Daemon, FsAs::DaemonAlone] {
                    assert_eq!(
                        download_dir_for(&who).await.map_err(|e| e.to_string()),
                        Err(cannot_receive(&dl, "it belongs to another account")),
                        "{who:?}: root stays strict"
                    );
                }
                return;
            }
            // Root's, and every account may write it, as FHS keeps
            // `/var/tmp` (or `/dev/shm`); never the temp dir.
            let temp = std::fs::canonicalize(std::env::temp_dir()).ok();
            let theirs = ["/var/tmp", "/dev/shm"]
                .into_iter()
                .map(PathBuf::from)
                .find(|p| {
                    std::fs::metadata(p)
                        .is_ok_and(|md| md.is_dir() && md.uid() != euid() && md.mode() & 0o002 != 0)
                        && std::fs::canonicalize(p).ok() != temp
                })
                .expect("a folder another account owns that every account may write");
            std::os::unix::fs::symlink(&theirs, &dl).unwrap();
            std::fs::create_dir_all(home.path().join(".config")).unwrap();
            std::fs::write(
                home.path().join(".config/user-dirs.dirs"),
                "XDG_DOWNLOAD_DIR=\"/usr\"\n",
            )
            .unwrap();
            assert_eq!(
                default_download_dir_as(&FsAs::Daemon, false).map_err(|e| e.to_string()),
                Ok(dl.clone()),
                "another account's folder it can write into: taken; one it cannot: passed over"
            );
            /// What the end places in that shared folder, removed with the test.
            struct Placed(PathBuf);
            impl Drop for Placed {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let private = home.path().join(".local/state/roomler/staging");
            let h = FilesHandler::new();
            let id = tid("fat-theirs");
            let name = format!("{id}.txt");
            let landed = Placed(theirs.join(&name));
            h.begin_as(FsAs::Daemon, id.clone(), name.clone(), 4, None, None)
                .await
                .expect("begin");
            let partial_dir = private.join(&id);
            let staged = (
                partial_dir.is_dir().then(|| mode_and_owner(&partial_dir)),
                theirs.join(".roomler-partial").join(&id).exists(),
            );
            h.chunk(b"vbox").await.expect("chunk");
            let ended = h.end(&id).await;
            forget(&[&id]);
            assert_eq!(
                staged,
                (Some((0o700, euid())), false),
                "staged in its own private folder, nothing beside the target"
            );
            let (placed, _) = ended.expect("end");
            assert_eq!(placed, dl.join(&name));
            assert_eq!(
                (std::fs::read(&landed.0).ok(), mode_and_owner(&landed.0).1),
                (Some(b"vbox".to_vec()), euid()),
                "copied across, the daemon's"
            );
            assert!(!partial_dir.exists(), "its staging goes");
        }

        /// #3688 — a daemon that is root (root at the screen, or the switch off)
        /// judges a configured folder, or one a controller chose, by FR-84
        /// D4's rule against the home its account names (`getpwuid`), never
        /// the `HOME` its environment names (`sudo -E` keeps the caller's, a
        /// drop-in may set one): a folder inside another account's home named
        /// there is not taken, and the upload lands in the default folder.
        /// `~` expands against its account's home.
        #[tokio::test]
        async fn a_root_daemons_configured_or_chosen_folder_is_judged_against_its_accounts_home() {
            if !as_root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, false);
            let (foreign, _person) = super::as_root::nobodys_home();
            let _env = stand_in::env_home(Some(foreign.path()));
            let theirs = foreign.path().join("Drops");
            std::fs::create_dir(&theirs).unwrap();
            chmod(&theirs, 0o700);
            let _configured = Configured::set(&theirs);
            let seen = (
                writer_context().active_home.map(PathBuf::from),
                download_dir_for(&FsAs::Daemon)
                    .await
                    .map_err(|e| e.to_string()),
                resolve_dest_path_as(&FsAs::Daemon, &theirs.to_string_lossy())
                    .await
                    .is_ok(),
            );
            assert_eq!(
                seen,
                (Some(home.path().to_path_buf()), Ok(home.downloads()), false),
                "the environment's home decides nothing: neither the configured folder nor a \
                 chosen one inside it is taken"
            );
            let ours = home.path().join("Drops");
            std::fs::create_dir(&ours).unwrap();
            chmod(&ours, 0o700);
            set_files_dir(Some("~/Drops".into()));
            assert_eq!(
                download_dir_for(&FsAs::Daemon)
                    .await
                    .map_err(|e| e.to_string()),
                Ok(ours),
                "`~` expands against its account's home"
            );
        }

        /// #3688 — `keeps_modes`'s probe, met by another account between
        /// making its folder and opening it, answers nothing: a probe removed
        /// before it opens, or (as root) replaced by another account's folder
        /// that every account may write, leaves the answer "keeps modes", so
        /// the folder's owner and modes still count; and that other folder's
        /// mode is left as it is.
        #[test]
        fn a_probe_that_does_not_open_as_it_was_made_answers_nothing() {
            let target = tempfile::tempdir().unwrap();
            chmod(target.path(), 0o777);
            let held = staging::open_folder(target.path()).unwrap();
            staging::forget_what_keeps_modes();
            stand_in::before_the_probe_opens(|probe| std::fs::remove_dir(probe).unwrap());
            let removed = staging::keeps_modes(&held);
            staging::forget_what_keeps_modes();
            let mut swapped = None;
            if as_root() {
                let nobody =
                    crate::fs_identity::Account::by_name("nobody").expect("the `nobody` account");
                let theirs = target.path().join("theirs");
                std::fs::create_dir(&theirs).unwrap();
                std::fs::write(theirs.join("kept"), b"x").unwrap();
                chmod(&theirs, 0o777);
                std::os::unix::fs::chown(&theirs, Some(nobody.uid), Some(nobody.gid)).unwrap();
                stand_in::before_the_probe_opens(move |probe| {
                    std::fs::remove_dir(probe).unwrap();
                    std::fs::rename(&theirs, probe).unwrap();
                });
                let answer = staging::keeps_modes(&held);
                staging::forget_what_keeps_modes();
                let left = std::fs::read_dir(target.path())
                    .unwrap()
                    .flatten()
                    .find(|e| e.path().join("kept").exists())
                    .map(|e| mode_and_owner(&e.path()).0);
                swapped = Some((answer, left));
            }
            assert_eq!(
                (removed, swapped),
                (true, as_root().then_some((true, Some(0o777)))),
                "no answer from a probe that did not open as made; another account's folder untouched"
            );
        }

        /// #3688 — root's private staging folder sits in root's own
        /// `/var/lib/roomler` (stood in under the test's home): opened from its
        /// parent without following a link, root's, and no other account may
        /// write it; made 0755 when absent. Another account's, one others may
        /// write, or a link there refuses the upload, saying why.
        #[tokio::test]
        async fn roots_private_staging_sits_only_in_roots_own_var_lib_roomler() {
            if !as_root() {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let _fat = stand_in::modes_kept(Some(false));
            let roomler = home.path().join("var/lib/roomler");
            std::fs::create_dir_all(roomler.parent().unwrap()).unwrap();
            let nobody =
                crate::fs_identity::Account::by_name("nobody").expect("the `nobody` account");
            let elsewhere = tempfile::tempdir().unwrap();
            let h = FilesHandler::new();
            let mut refused = Vec::new();
            for n in 0..3 {
                let _ = std::fs::remove_dir_all(&roomler);
                let _ = std::fs::remove_file(&roomler);
                if n == 2 {
                    std::os::unix::fs::symlink(elsewhere.path(), &roomler).unwrap();
                } else {
                    std::fs::create_dir(&roomler).unwrap();
                    chmod(&roomler, if n == 0 { 0o755 } else { 0o777 });
                }
                if n == 0 {
                    std::os::unix::fs::chown(&roomler, Some(nobody.uid), Some(nobody.gid)).unwrap();
                }
                let id = tid(&format!("var-lib-{n}"));
                let begun = h
                    .begin_as(FsAs::DaemonAlone, id.clone(), "v.txt".into(), 1, None, None)
                    .await;
                if begun.is_ok() {
                    h.abort().await;
                }
                forget(&[&id]);
                refused.push(begun.err().map(|e| format!("{e:#}")));
            }
            let shown = roomler.display();
            let why = |why: &str| {
                Some(format!(
                    "{shown} {why}, so file transfer does not stage there"
                ))
            };
            let refused: Vec<_> = refused
                .into_iter()
                .zip([
                    why("belongs to another account"),
                    why("is open to other accounts"),
                    why("is not a folder"),
                ])
                .map(|(got, want)| got.zip(want).is_some_and(|(got, want)| got.contains(&want)))
                .collect();
            let _ = std::fs::remove_file(&roomler);
            let _ = std::fs::remove_dir_all(&roomler);
            let id = tid("var-lib-made");
            h.begin_as(FsAs::DaemonAlone, id.clone(), "m.txt".into(), 1, None, None)
                .await
                .expect("begin");
            let made = mode_and_owner(&roomler);
            h.chunk(b"m").await.expect("chunk");
            let ended = h.end(&id).await.map(|(p, _)| p).map_err(|e| e.to_string());
            forget(&[&id]);
            assert_eq!(
                (refused, made, ended),
                (
                    vec![true; 3],
                    (0o755, 0),
                    Ok(home.downloads().join("m.txt"))
                ),
                "refused, saying why, unless root's own; made 0755 root:root when absent"
            );
        }

        /// #3688 — a probe's folder left behind in a drop folder (a crash
        /// between its make and its removal) goes at the startup sweep when it
        /// is its stager's own and empty. One with something in it, a link
        /// named so and (as root) another account's are left as they are.
        #[tokio::test]
        async fn the_sweep_removes_a_probe_folder_left_behind() {
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            let probe = |n: u8| dl.join(format!(".roomler-probe-{n:016x}"));
            let (left, full, link, theirs) = (probe(1), probe(2), probe(3), probe(4));
            std::fs::create_dir(&left).unwrap();
            std::fs::create_dir(&full).unwrap();
            std::fs::write(full.join("kept"), b"x").unwrap();
            std::os::unix::fs::symlink(home.path(), &link).unwrap();
            if as_root() {
                let nobody =
                    crate::fs_identity::Account::by_name("nobody").expect("the `nobody` account");
                std::fs::create_dir(&theirs).unwrap();
                std::os::unix::fs::chown(&theirs, Some(nobody.uid), Some(nobody.gid)).unwrap();
                staging::sweep_probes(&dl);
            } else {
                sweep_orphans().await;
            }
            assert_eq!(
                (
                    left.exists(),
                    full.join("kept").exists(),
                    link.symlink_metadata().is_ok(),
                    theirs.exists() == as_root()
                ),
                (false, true, true, true),
                "only its stager's own empty probe folder goes"
            );
        }

        /// #3688 — an upload staged privately whose copy across fails (its
        /// folder could not be written at the end) keeps its partial, and a
        /// resume completes it once the folder can be written again.
        #[tokio::test]
        async fn a_copy_across_that_fails_keeps_its_partial() {
            if euid() == 0 {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let dl = home.downloads();
            let _fat = stand_in::modes_kept(Some(false));
            let private = home.path().join(".local/state/roomler/staging");
            let h = FilesHandler::new();
            let id = tid("copy-fails");
            h.begin_as(FsAs::Daemon, id.clone(), "c.txt".into(), 4, None, None)
                .await
                .expect("begin");
            h.chunk(b"copy").await.expect("chunk");
            chmod(&dl, 0o500);
            let ended = h.end(&id).await;
            chmod(&dl, 0o700);
            let kept = (
                std::fs::read(private.join(&id).join("data")).ok(),
                private.join(&id).join("meta.json").is_file(),
                registered(&id),
                dl.join("c.txt").exists(),
            );
            let resumed = h.resume_incoming_as(FsAs::Daemon, &id, 4).await;
            let placed = match &resumed {
                Ok(at) => {
                    h.chunk(&b"copy"[*at as usize..]).await.expect("chunk");
                    h.end(&id).await.map(|(p, _)| p).map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            forget(&[&id]);
            assert!(ended.is_err(), "the end fails");
            assert_eq!(
                (kept, placed),
                (
                    (Some(b"copy".to_vec()), true, true, false),
                    Ok(dl.join("c.txt"))
                ),
                "the partial stays, and a resume completes it"
            );
        }

        /// #3688 — a private partial resumes at an offset past the 256 KiB
        /// mark, from that mark, and the file placed is the one sent.
        #[tokio::test]
        async fn a_private_partial_resumes_past_256_kib() {
            if euid() == 0 {
                return;
            }
            let _lock = FILES_DIR_TEST_LOCK.lock().await;
            let home = OwnHome::new(true, true);
            let _fat = stand_in::modes_kept(Some(false));
            let sent: Vec<u8> = (0..600 * 1024).map(|i| (i % 251) as u8).collect();
            let h = FilesHandler::new();
            let id = tid("resume-past");
            h.begin_as(
                FsAs::Daemon,
                id.clone(),
                "big.bin".into(),
                sent.len() as u64,
                None,
                None,
            )
            .await
            .expect("begin");
            h.chunk(&sent[..300 * 1024]).await.expect("chunk");
            h.abort().await;
            forget(&[&id]);
            let resumed = h
                .resume_incoming_as(FsAs::Daemon, &id, 300 * 1024)
                .await
                .map_err(|e| e.to_string());
            let at = resumed.clone();
            let placed = match resumed {
                Ok(at) => {
                    h.chunk(&sent[at as usize..]).await.expect("chunk");
                    h.end(&id).await.map(|(p, _)| p).map_err(|e| e.to_string())
                }
                Err(e) => Err(e),
            };
            forget(&[&id]);
            assert_eq!(at, Ok(256 * 1024), "resumed from the 256 KiB mark");
            let placed = placed.expect("end");
            assert_eq!(placed, home.downloads().join("big.bin"));
            assert!(
                std::fs::read(&placed).unwrap() == sent,
                "the file placed is the one sent"
            );
        }
    }
}

/// #3688 — a test's step that turns its configured drop folder into a file.
///
/// The configured folder (`FILES_DIR`) is process-wide, and resolving the drop
/// folder runs `files_dir::validate` on it, which creates a missing folder,
/// probes a write and removes the probe. Every test that resolves the drop
/// folder holds `FILES_DIR_TEST_LOCK` for that: an upload with no folder
/// chosen, and a resume of an unknown id, which looks for its notes beside the
/// drop folder (`resume_unknown_id_errors`). A test that did not could create
/// this folder again between the remove and the write, so the step is retried,
/// bounded, while the path is still a folder. Once the file stands, a
/// concurrent `validate` refuses it and creates nothing.
#[cfg(test)]
fn turn_the_folder_into_a_file(dir: &std::path::Path, bytes: &[u8]) {
    let mut tries = 1;
    loop {
        let _ = std::fs::remove_dir_all(dir);
        match std::fs::write(dir, bytes) {
            Ok(()) => return,
            Err(e) => {
                assert!(
                    tries < 50 && dir.is_dir(),
                    "{} did not become a file in {tries} tries: {e}",
                    dir.display()
                );
                tries += 1;
            }
        }
    }
}

/// #3688 — an upload's id is one plain name ([`transfer_id`]), checked first
/// at every entry that takes one from the viewer, before anything on disk is
/// touched, on every OS. Each test collects what went wrong for every kind
/// of id before it fails, so one run shows them all.
#[cfg(test)]
mod transfer_id_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// The shared registry is keyed by id, and `..`, `.` and the empty id
    /// are the same in every test: the tests that register under them, or
    /// read the registry for them, take turns.
    static TURNS: Mutex<()> = Mutex::const_new(());

    fn base36(mut n: u64) -> String {
        const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut out = Vec::new();
        loop {
            out.push(DIGITS[(n % 36) as usize]);
            n /= 36;
            if n == 0 {
                break;
            }
        }
        out.reverse();
        String::from_utf8(out).expect("base36 digits are ASCII")
    }

    /// An id shaped as the viewer mints one (`uploadOne`,
    /// `ui/src/composables/useRemoteControl.ts`):
    /// `up-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`.
    fn viewer_shaped() -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let millis = chrono::Utc::now().timestamp_millis().unsigned_abs();
        let random = u64::from(std::process::id())
            .wrapping_mul(1_000_003)
            .wrapping_add(SEQ.fetch_add(1, Ordering::Relaxed))
            % 36u64.pow(6);
        format!("up-{}-{}", base36(millis), base36(random))
    }

    /// Every kind of id that is not one plain name, with what it is. `tag`
    /// keeps a test's ids its own where the kind allows; the absolute one
    /// names a path inside `abs_in`.
    fn not_one_plain_name(tag: &str, abs_in: &Path) -> Vec<(String, &'static str)> {
        let over = format!("{tag}-{}", "x".repeat(TRANSFER_ID_MAX_BYTES - tag.len()));
        assert_eq!(over.len(), TRANSFER_ID_MAX_BYTES + 1);
        vec![
            ("..".to_string(), "`..`"),
            (format!("../{tag}"), "a name after `..`"),
            (
                abs_in
                    .join(format!("{tag}-abs"))
                    .to_string_lossy()
                    .into_owned(),
                "an absolute path",
            ),
            (format!("{tag}/{tag}"), "a name holding `/`"),
            (format!("{tag}\\{tag}"), "a name holding `\\`"),
            (format!(".{tag}"), "a leading dot"),
            (".".to_string(), "`.`"),
            (String::new(), "an empty id"),
            (over, "one byte over the most"),
        ]
    }

    /// `Ok` when `res` refused the id with the words the viewer's
    /// `files:error` shows; else what happened instead.
    fn refused<T: std::fmt::Debug>(res: Result<T>) -> std::result::Result<(), String> {
        match res {
            Err(e) if e.to_string() == NOT_A_TRANSFER_ID => Ok(()),
            Err(e) => Err(format!("refused, but saying {e:#}")),
            Ok(v) => Err(format!("taken: {v:?}")),
        }
    }

    /// Everything under `root`, each path with its length, so a change
    /// anywhere under it shows.
    fn tree(root: &Path) -> BTreeSet<(PathBuf, u64)> {
        let mut out = BTreeSet::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let len = std::fs::symlink_metadata(&path).map_or(0, |m| m.len());
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    dirs.push(path.clone());
                }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.insert((rel, len));
            }
        }
        out
    }

    /// A partial as a begin leaves one in `dir`: its notes, and ten bytes.
    fn plant(dir: &Path, dest: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let meta = PartialMeta {
            protocol_version: 1,
            filename: "x.bin".into(),
            expected_size: 10,
            dest_dir: dest.to_path_buf(),
            reserved_final_path: dest.join("x.bin"),
            created_at_unix: chrono::Utc::now().timestamp(),
            rel_path: None,
            person_sid: None,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        std::fs::write(dir.join("data"), b"0123456789").unwrap();
    }

    fn registered(id: &str) -> Option<PathBuf> {
        let g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        g.get(id).map(|e| e.meta_path.clone())
    }

    fn register(id: &str, meta_path: PathBuf) {
        let mut g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(
            id.to_string(),
            PartialEntry {
                meta_path,
                who: FsAs::Daemon,
            },
        );
    }

    fn forget(id: &str) {
        let mut g = PARTIAL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        g.remove(id);
    }

    /// A handler that stages in `root`, as a Windows install stages in the
    /// machine's own folder: a resume that finds no registered partial looks
    /// for one there, by its id.
    fn staging_in(root: &Path) -> FilesHandler {
        FilesHandler {
            staging_root: Some(root.to_path_buf()),
            ..FilesHandler::new()
        }
    }

    /// The rule itself. The viewer's ids, and ids shaped as the tests', pass
    /// up to 128 bytes; every kind of id that is not one plain name is
    /// refused, as is any byte but letters, digits, `.`, `_` and `-`.
    #[test]
    fn a_transfer_id_is_one_plain_name() {
        let most = "x".repeat(TRANSFER_ID_MAX_BYTES);
        let viewer = viewer_shaped();
        let mut wrong = Vec::new();
        for id in [
            viewer.as_str(),
            "up-mhv3k2x1-4fzyo8",
            "up-mhv3k2x1-",
            "rc19-stage-1762876543210987654",
            "a3688-1",
            "w3048-split-4242",
            "abc-123",
            "a.b_c-D9.",
            most.as_str(),
        ] {
            if transfer_id(id).ok() != Some(id) {
                wrong.push(format!("refused one plain name {id:?}"));
            }
        }
        let mut not: Vec<(String, &str)> = not_one_plain_name("rule", &std::env::temp_dir());
        for (id, what) in [
            ("a b", "a space"),
            ("a:b", "a colon"),
            ("a\0b", "a NUL"),
            ("caf\u{e9}", "a byte past ASCII"),
            ("a%2fb", "a percent sign"),
            ("a*b?", "a wildcard"),
            ("~a", "a tilde"),
        ] {
            not.push((id.to_string(), what));
        }
        for (id, what) in not {
            if let Err(why) = refused(transfer_id(&id)) {
                wrong.push(format!("{what} {id:?}: {why}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// A staging path is built from one plain name only: `partial_dir_for`
    /// and a handler's own staging root refuse every other id themselves, so
    /// a caller that skipped the check reaches no path. One plain name stays
    /// one folder inside the staging folder.
    #[test]
    fn a_staging_path_is_built_from_one_plain_name_only() {
        let dest = Path::new("dest");
        let root = Path::new("staging");
        let mut wrong = Vec::new();
        for (id, what) in not_one_plain_name("path", &std::env::temp_dir()) {
            if let Err(why) = refused(partial_dir_for(dest, &id)) {
                wrong.push(format!("partial_dir_for, {what} {id:?}: {why}"));
            }
            if let Err(why) = refused(FilesHandler::new().partial_dir(dest, &id)) {
                wrong.push(format!("the handler, {what} {id:?}: {why}"));
            }
            if let Err(why) = refused(staging_in(root).partial_dir(dest, &id)) {
                wrong.push(format!("a staging root, {what} {id:?}: {why}"));
            }
        }
        let id = viewer_shaped();
        assert_eq!(
            partial_dir_for(dest, &id).expect("a viewer-shaped id"),
            dest.join(".roomler-partial").join(&id)
        );
        assert_eq!(
            staging_in(root)
                .partial_dir(dest, &id)
                .expect("a viewer-shaped id"),
            root.join(&id)
        );
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// `files:begin` refuses an id that is not one plain name, in the words
    /// the viewer shows, before anything on disk is touched: the folder it
    /// targets, the folders around it and the path an absolute id names stay
    /// as they were, and nothing is registered. A viewer-shaped id begins as
    /// before, staged in its own folder.
    #[tokio::test]
    async fn begin_refuses_an_id_that_is_not_one_plain_name_before_touching_the_disk() {
        let _turn = TURNS.lock().await;
        let sandbox = tempfile::tempdir().unwrap();
        let dest = sandbox.path().join("to").join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let dest = dest.to_string_lossy().into_owned();
        let mut wrong = Vec::new();
        for (id, what) in not_one_plain_name("begin", sandbox.path()) {
            let before = tree(sandbox.path());
            let begun = FilesHandler::new()
                .begin_as(
                    FsAs::Daemon,
                    id.clone(),
                    "x.bin".into(),
                    10,
                    None,
                    Some(&dest),
                )
                .await;
            if let Err(why) = refused(begun) {
                wrong.push(format!("{what} {id:?}: {why}"));
            }
            if tree(sandbox.path()) != before {
                wrong.push(format!("{what} {id:?}: the disk changed"));
            }
            if registered(&id).is_some() {
                wrong.push(format!("{what} {id:?}: registered"));
                forget(&id);
            }
        }
        let id = viewer_shaped();
        let h = FilesHandler::new();
        h.begin_as(
            FsAs::Daemon,
            id.clone(),
            "x.bin".into(),
            10,
            None,
            Some(&dest),
        )
        .await
        .expect("a viewer-shaped id begins");
        let staged = registered(&id).expect("and is registered");
        assert!(
            staged.ends_with(Path::new(".roomler-partial").join(&id).join("meta.json")),
            "staged in its own folder: {}",
            staged.display()
        );
        assert!(h.cancel_incoming(&id).await.expect("and cancels"));
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// `files:resume` refuses an id that is not one plain name before it
    /// looks for a partial. A partial planted where each id names, for a
    /// handler that stages in a root of its own (a Windows install's layout,
    /// where a resume with nothing registered looks there by the id), keeps
    /// every byte. A viewer-shaped id resumes as before.
    #[tokio::test]
    async fn resume_refuses_an_id_that_is_not_one_plain_name_before_touching_the_disk() {
        let _turn = TURNS.lock().await;
        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("staging");
        let dest = sandbox.path().join("dest");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let mut wrong = Vec::new();
        for (id, what) in not_one_plain_name("resume", sandbox.path()) {
            plant(&root.join(&id), &dest);
            let before = tree(sandbox.path());
            let resumed = staging_in(&root)
                .resume_incoming_as(FsAs::Daemon, &id, 0)
                .await;
            if let Err(why) = refused(resumed) {
                wrong.push(format!("{what} {id:?}: {why}"));
            }
            if tree(sandbox.path()) != before {
                wrong.push(format!("{what} {id:?}: the disk changed"));
            }
        }
        let id = viewer_shaped();
        plant(&root.join(&id), &dest);
        let h = staging_in(&root);
        let accepted = h
            .resume_incoming_as(FsAs::Daemon, &id, 0)
            .await
            .expect("a viewer-shaped id resumes");
        assert_eq!(accepted, 0);
        h.abort().await;
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// `files:cancel` refuses an id that is not one plain name before it
    /// acts: a partial registered under the id, as a begin that took it would
    /// have left one, stays on disk and registered. A viewer-shaped id
    /// cancels as before, and its partial goes.
    #[tokio::test]
    async fn cancel_refuses_an_id_that_is_not_one_plain_name_before_touching_the_disk() {
        let _turn = TURNS.lock().await;
        let sandbox = tempfile::tempdir().unwrap();
        let dest = sandbox.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let mut wrong = Vec::new();
        for (n, (id, what)) in not_one_plain_name("cancel", sandbox.path())
            .into_iter()
            .enumerate()
        {
            let partial = sandbox.path().join("partials").join(n.to_string());
            plant(&partial, &dest);
            register(&id, partial.join("meta.json"));
            let before = tree(sandbox.path());
            if let Err(why) = refused(FilesHandler::new().cancel_incoming(&id).await) {
                wrong.push(format!("{what} {id:?}: {why}"));
            }
            if tree(sandbox.path()) != before {
                wrong.push(format!("{what} {id:?}: the disk changed"));
            }
            if registered(&id) != Some(partial.join("meta.json")) {
                wrong.push(format!("{what} {id:?}: the registry changed"));
            }
            forget(&id);
        }
        let id = viewer_shaped();
        let h = FilesHandler::new();
        h.begin_as(
            FsAs::Daemon,
            id.clone(),
            "x.bin".into(),
            10,
            None,
            Some(&dest.to_string_lossy()),
        )
        .await
        .expect("a viewer-shaped id begins");
        let staging = registered(&id)
            .and_then(|m| m.parent().map(Path::to_path_buf))
            .expect("its partial, registered");
        assert!(h.cancel_incoming(&id).await.expect("and cancels"));
        assert!(!staging.exists(), "its partial goes");
        assert!(registered(&id).is_none(), "and its registration");
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// `files:end` refuses an id that is not one plain name, and the upload
    /// in flight stays as it was: its own end still completes it.
    #[tokio::test]
    async fn end_refuses_an_id_that_is_not_one_plain_name_and_keeps_the_upload() {
        let sandbox = tempfile::tempdir().unwrap();
        let dest = sandbox.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let id = viewer_shaped();
        let h = FilesHandler::new();
        h.begin_as(
            FsAs::Daemon,
            id.clone(),
            "x.bin".into(),
            5,
            None,
            Some(&dest.to_string_lossy()),
        )
        .await
        .expect("a viewer-shaped id begins");
        h.chunk(b"hello").await.expect("its bytes");
        let mut wrong = Vec::new();
        for (bad, what) in not_one_plain_name("end", sandbox.path()) {
            if let Err(why) = refused(h.end(&bad).await) {
                wrong.push(format!("{what} {bad:?}: {why}"));
            }
        }
        let (path, bytes) = h.end(&id).await.expect("its own end completes it");
        assert_eq!(
            (bytes, std::fs::read(&path).unwrap()),
            (5, b"hello".to_vec())
        );
        let _ = std::fs::remove_file(&path);
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
