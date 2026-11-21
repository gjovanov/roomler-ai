// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3682 — the file drawer's reads, to their edges: what resolving a path
//! answers, how long a listing may take and how many may wait on a channel,
//! and who a read is looked up as on Windows.
//!
//! #3669 made a machine-wide Windows install read as the person signed in at
//! the console, one step at a time ([`FsAs::run`]). What a read touched
//! outside such a step, or without a bound, and what its refusal said, is
//! gathered here; the tests are `drawer_edges`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::{DirListing, FsAs};

/// The OS, as a refusal names it.
pub(super) const THIS_OS: &str = if cfg!(target_os = "windows") {
    "Windows"
} else if cfg!(target_os = "macos") {
    "macOS"
} else {
    "Linux"
};

/// Why a read or a write is refused when who it acts as could not be told in
/// time. Linux: the LocalAPI's own words (#3796). #3682 item 2 — Windows:
/// the same, for the person signed in to this computer.
#[cfg(target_os = "linux")]
pub(super) const CANNOT_TELL: &str = tunnel_core::localapi::CANNOT_TELL_WHO_IS_AT_THE_SCREEN;
#[cfg(target_os = "windows")]
pub(super) const CANNOT_TELL: &str = "Could not tell who is signed in to this computer. Try again.";

// ---------------------------------------------------------------------------
// Item 6: what resolving a path answers

/// Resolve `asked` to the path it names, symlinks and `..` followed, as
/// whoever runs the calling step: a download's open, a folder download's
/// begin and walk, a listing. A failure's outermost line is the path and the
/// OS's own reason (the viewer shows no other), and the I/O error stays in
/// the chain, so a refusal is still explained for its reader
/// ([`FsAs::explain_refusal`]).
pub(super) fn resolve(asked: &Path) -> Result<PathBuf> {
    resolve_path(asked).map_err(|e| {
        let line = format!("{}: {e}", asked.display());
        anyhow::Error::new(e).context(line)
    })
}

/// [`resolve`]'s call into the OS; a test stands a refusal in.
pub(super) fn resolve_path(asked: &Path) -> std::io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(refused) = super::drawer_edges::seams::resolving(asked) {
        return Err(refused);
    }
    std::fs::canonicalize(asked)
}

// ---------------------------------------------------------------------------
// Item 3 (D1-B): one bound over a listing, one step at a time per channel

/// The one bound over a listing's step: resolving the folder, opening it and
/// reading its entries. It starts when the step takes the channel's lane, so
/// a listing that waited for another still has all of it; the listing's own
/// deadline, [`LISTING_ANSWERED_WITHIN`], covers that wait.
pub(super) const LISTING_BOUND: Duration = Duration::from_secs(super::DIR_LIST_TIMEOUT_SECS);

/// How long a listing has from reaching its task to its answer, all in: the
/// person lookup (1.5 s at most: [`PERSON_LOOKUP_WAIT`] on Windows, the seat0
/// lookup's on Linux), its wait for the channel's lane, and its step. The
/// viewer waits 5 s from its send, and 5 s after each page of a listing in
/// pages (`ui/src/composables/remoteDirListing.ts`): 0.5 s for the way there
/// and back if the read loop hands it on at once; an upload's end, run inline, can delay it.
pub(super) const LISTING_ANSWERED_WITHIN: Duration = Duration::from_millis(4500);

/// How many listings may wait on a channel's lane behind the step it runs.
/// One more is answered at once.
pub(super) const LISTINGS_WAITING_AT_MOST: usize = 4;

/// One files channel's listings (#3682): its folders take one [`Lane`], and
/// its Drives view another. `GetLogicalDrives` asks no share, so a folder
/// stuck on a share that stopped answering never refuses the viewer's way
/// back to the drives.
#[derive(Clone, Default)]
pub(crate) struct Listings {
    pub(super) folders: Lane,
    pub(super) drives: Lane,
    /// The channel closed, or its session ended: a listing still running
    /// drops its answer.
    closed: tokio_util::sync::CancellationToken,
}

impl Listings {
    /// The channel closed, or its session ended.
    pub(crate) fn close(&self) {
        self.closed.cancel();
    }

    /// Cancelled once [`Self::close`] is called.
    pub(crate) fn closed(&self) -> tokio_util::sync::CancellationToken {
        self.closed.clone()
    }
}

/// One lane of a files channel's listings (#3682, D1-B): its folders', or
/// its Drives view's ([`Listings`]). A listing is one step
/// on a blocking thread, and a step cannot be cancelled: a share that stops
/// answering keeps its thread for as long as it likes. So a channel runs one
/// listing step at a time, and the step keeps the lane until its thread is
/// really done, past its bound if it must. A listing that comes while the
/// step runs within its bound waits for it, inside its own deadline, and its
/// own step then has the whole [`LISTING_BOUND`]: quick navigation is never
/// refused. One that comes, or is still waiting, once the step has passed its
/// bound is answered at once, naming the folder that is not answering. One
/// past its own deadline starts no step. At most
/// [`LISTINGS_WAITING_AT_MOST`] wait. So a dead share strands one thread per
/// channel, however often the viewer asks.
#[derive(Clone, Default)]
pub(crate) struct Lane(Arc<LaneInner>);

#[derive(Default)]
struct LaneInner {
    /// A std lock: taken and let go between awaits, never held across one.
    state: std::sync::Mutex<LaneState>,
    /// Told when a step's thread lets the lane go.
    step_ended: tokio::sync::Notify,
}

#[derive(Default)]
struct LaneState {
    /// The step running now: the folder it was asked for, and when its
    /// bound ends, [`LISTING_BOUND`] after it took the lane.
    running: Option<(String, Instant)>,
    /// The listings waiting for it.
    waiting: usize,
}

/// What a listing does next, decided under the lane's lock.
enum Turn {
    /// Taken, by a step whose bound ends then.
    Taken(Instant),
    Wait(Instant),
    Stuck(String),
    TooMany,
    OutOfTime,
}

/// The lane's step, owned by the blocking thread that runs it: the lane is
/// let go when that thread is done, not when the listing stops waiting.
struct Step(Arc<LaneInner>);

impl Drop for Step {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.running = None;
        drop(state);
        self.0.step_ended.notify_waiters();
    }
}

/// A listing counted as waiting, until it stops: taken, refused, or dropped
/// with its task.
struct Waiting(Arc<LaneInner>);

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.waiting = state.waiting.saturating_sub(1);
    }
}

impl Lane {
    /// Run `step` as `who`, as this lane's one step: `asked` is the folder as
    /// the viewer named it, a listing to be answered `by`. The step's own
    /// bound, [`LISTING_BOUND`], starts when it takes the lane, and the
    /// listing answers by the earlier of that bound's end and `by`.
    pub(super) async fn step<T, F>(
        &self,
        who: &FsAs,
        asked: &str,
        by: Instant,
        step: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let (held, its_end) = self.take(asked, by).await?;
        let ran = who.run(move || {
            let _held = held;
            step()
        });
        match tokio::time::timeout_at(its_end.min(by).into(), ran).await {
            Ok(ran) => ran,
            Err(_) => Err(anyhow!("listing timed out")),
        }
    }

    /// Take the lane for `asked`, a listing to be answered `by`, or say why
    /// not; taken, with when the step's bound ends. A listing already past
    /// `by` takes nothing, so it starts no step: the step would hold the lane
    /// for an answer nobody waits for.
    async fn take(&self, asked: &str, by: Instant) -> Result<(Step, Instant)> {
        let mut waiting: Option<Waiting> = None;
        loop {
            let (turn, ended) = {
                let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                let turn = if now >= by {
                    Turn::OutOfTime
                } else {
                    match state.running.clone() {
                        None => {
                            let its_end = now + LISTING_BOUND;
                            state.running = Some((asked.to_string(), its_end));
                            Turn::Taken(its_end)
                        }
                        Some((stuck, its_end)) if now >= its_end => Turn::Stuck(stuck),
                        Some(_)
                            if waiting.is_none() && state.waiting >= LISTINGS_WAITING_AT_MOST =>
                        {
                            Turn::TooMany
                        }
                        Some((_, its_end)) => {
                            if waiting.is_none() {
                                state.waiting += 1;
                                waiting = Some(Waiting(self.0.clone()));
                            }
                            Turn::Wait(its_end.min(by))
                        }
                    }
                };
                // Made under the lock, so a step that ends after it is
                // heard: `notify_waiters` wakes every `Notified` made before.
                let ended = matches!(turn, Turn::Wait(_)).then(|| self.0.step_ended.notified());
                (turn, ended)
            };
            match turn {
                Turn::Taken(its_end) => return Ok((Step(self.0.clone()), its_end)),
                Turn::Stuck(stuck) => {
                    return Err(anyhow!(
                        "{asked}: not listed, because {stuck} is not answering. Try again in a \
                         moment."
                    ));
                }
                Turn::TooMany => {
                    return Err(anyhow!(
                        "{asked}: not listed, because too many folders were asked at once. Try \
                         again in a moment."
                    ));
                }
                Turn::OutOfTime => return Err(anyhow!("listing timed out")),
                Turn::Wait(wake) => {
                    if let Some(ended) = ended {
                        let _ = tokio::time::timeout_at(wake.into(), ended).await;
                    }
                }
            }
        }
    }
}

/// #3682 item 1 — the Drives view, read on its own `lane` inside a step as `who`:
/// Windows keeps drive letters per sign-in, so a letter the person mapped is
/// in their sign-in's device map, not the service's. `GetLogicalDrives` asks
/// no drive anything, so the step touches no share. Elsewhere the roots are
/// `/` alone.
pub(super) async fn roots(lane: &Lane, who: &FsAs, by: Instant) -> Result<DirListing> {
    #[cfg(target_os = "windows")]
    let entries = lane
        .step(who, &super::roots_label(), by, || {
            Ok(super::enumerate_roots())
        })
        .await?;
    #[cfg(not(target_os = "windows"))]
    let entries = {
        let _ = (lane, who, by);
        super::enumerate_roots()
    };
    Ok(DirListing {
        path: super::roots_label(),
        parent: None,
        entries,
        capped: false,
    })
}

/// The entries of the folder `read` lists, `at` being its resolved path:
/// where a test stands in a share whose entries do not come
/// (`drawer_edges::seams::hold_entries`). With them, whether the folder held
/// more than a listing takes (#4121).
pub(super) fn read_entries(at: &Path, read: std::fs::ReadDir) -> (Vec<super::DirEntryView>, bool) {
    #[cfg(test)]
    super::drawer_edges::seams::entries_wait(at);
    #[cfg(not(test))]
    let _ = at;
    super::collect_dir_entries(read, super::DIR_ENTRIES_MAX)
}

impl super::FilesHandler {
    /// A folder listing on this channel (#3682, D1-B): who reads it is looked
    /// up first, then the listing takes the channel's lane, all of it within
    /// [`LISTING_ANSWERED_WITHIN`] of its arrival.
    pub(crate) async fn list_dir(&self, path: &str) -> Result<DirListing> {
        let by = Instant::now() + LISTING_ANSWERED_WITHIN;
        let who = FsAs::for_read().await;
        super::list_dir_in(&self.listings, &who, path, by).await
    }

    /// [`Self::list_dir`], read as `who`: a test's, with no lookup.
    #[cfg(test)]
    pub(crate) async fn list_as(&self, who: &FsAs, path: &str) -> Result<DirListing> {
        let by = Instant::now() + LISTING_ANSWERED_WITHIN;
        super::list_dir_in(&self.listings, who, path, by).await
    }

    /// The channel closed: an upload's state goes, as with [`Self::abort`],
    /// and a listing still running drops its answer.
    pub(crate) async fn channel_closed(&self) {
        self.abort().await;
        self.listings.close();
    }

    /// Cancelled once the channel closes or its session ends: a listing
    /// still running then drops its answer.
    pub(crate) fn listings_closed(&self) -> tokio_util::sync::CancellationToken {
        self.listings.closed()
    }
}

// ---------------------------------------------------------------------------
// Item 5 (D3-A): the staging folder says why

/// #3682 item 5 — the Upload staging folder refused to the person says why:
/// on a machine-wide install it is the service's own folder, which only
/// SYSTEM and the Administrators may open, so a standard user, or an
/// administrator while UAC is on, is refused; an administrator reads it with
/// `roomler exec`, which runs as SYSTEM there.
#[cfg(target_os = "windows")]
pub(super) fn explain_staging<T>(who: &FsAs, folder: &Path, listed: Result<T>) -> Result<T> {
    use anyhow::Context as _;
    if matches!(who, FsAs::Person { .. }) && listed.as_ref().is_err_and(super::is_permission_denied)
    {
        return listed.with_context(|| {
            format!(
                "{} is the Roomler service's own folder: a standard user, or an administrator \
                 while UAC is on, may not open it. An administrator can read it with roomler exec.",
                folder.display()
            )
        });
    }
    listed
}

/// The machine's staging folder, as the Upload staging folder button lists
/// it; a test stands one in (`drawer_edges::seams::staging_root`).
#[cfg(target_os = "windows")]
pub(super) fn staging_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = super::drawer_edges::seams::staging_root_now() {
        return root;
    }
    super::staging_root_windows()
}

// ---------------------------------------------------------------------------
// Item 2 (D4-A): the Windows person, looked up off the async worker

/// How long a read or a write starting now waits for the Windows person
/// lookup: as long as the Linux seat0 lookup's (`SEAT_LOOKUP_WAIT`), so a
/// listing's [`LISTING_BOUND`] after it still answers inside the viewer's 5 s.
#[cfg(target_os = "windows")]
pub(super) const PERSON_LOOKUP_WAIT: Duration = Duration::from_millis(1500);

/// The one Windows person lookup in flight, as `SEAT0` is Linux's: a lookup
/// that runs past its callers' wait is joined by the next, never doubled.
#[cfg(target_os = "windows")]
static PERSON: crate::one_in_flight::OneInFlight<Option<crate::win_identity::SessionPerson>> =
    crate::one_in_flight::OneInFlight::new("person-lookup");

/// Who a read or a write starting now acts as, asked from an async task. On
/// Windows the person is looked up on a thread of its own, within
/// [`PERSON_LOOKUP_WAIT`]: the lookup calls the session manager and reads
/// the person's registry, and the worker it ran on served the whole files
/// channel. No answer in that time is [`FsAs::CannotTell`], never the
/// service. macOS: the daemon. ([`FsAs::now`], asked on blocking threads,
/// still asks on its own.)
#[cfg(not(target_os = "linux"))]
pub(super) async fn session_or_daemon_within() -> FsAs {
    #[cfg(target_os = "windows")]
    {
        #[cfg(test)]
        if let Some(stood) = super::drawer_edges::seams::person_lookup_now() {
            let lookup = stood.lookup.clone();
            return windows_fs_as_within(
                stood.own_token,
                stood.switch_on,
                &stood.flight,
                PERSON_LOOKUP_WAIT,
                move || lookup(),
            )
            .await;
        }
        windows_fs_as_within(
            own_token_is_local_system(),
            files_as_seat_user(),
            &PERSON,
            PERSON_LOOKUP_WAIT,
            crate::win_identity::session_person,
        )
        .await
    }
    #[cfg(not(target_os = "windows"))]
    FsAs::Daemon
}

/// [`session_or_daemon_within`] from this process's facts, `flight` and
/// `lookup`: the process decides the daemon with no lookup (`fs_as_for`'s
/// rule); otherwise the person, nobody, or [`FsAs::CannotTell`].
#[cfg(target_os = "windows")]
async fn windows_fs_as_within(
    own_token: Option<bool>,
    switch_on: bool,
    flight: &crate::one_in_flight::OneInFlight<Option<crate::win_identity::SessionPerson>>,
    wait: Duration,
    lookup: impl FnOnce() -> Option<crate::win_identity::SessionPerson> + Send + 'static,
) -> FsAs {
    use crate::one_in_flight::Asked;
    if own_token == Some(false) || !switch_on {
        return FsAs::Daemon;
    }
    match flight.ask(wait, lookup).await {
        Asked::Answer(person) => super::fs_as_for(own_token, switch_on, move || person),
        Asked::Failed(_) | Asked::OutOfTime => FsAs::CannotTell,
    }
}

/// Whether this process's own token is LocalSystem's (`None`: it could not be
/// read), as the Windows reader is decided from it (`fs_as_for`); a test
/// stands it in (`drawer_edges::seams::person_lookup`).
#[cfg(target_os = "windows")]
pub(super) fn own_token_is_local_system() -> Option<bool> {
    #[cfg(test)]
    if let Some(stood) = super::drawer_edges::seams::person_lookup_now() {
        return stood.own_token;
    }
    crate::win_identity::own_token_is_local_system()
}

/// `ROOMLERD_FILES_AS_SEAT_USER`, on unless set to `0`; a test stands it in.
#[cfg(target_os = "windows")]
pub(super) fn files_as_seat_user() -> bool {
    #[cfg(test)]
    if let Some(stood) = super::drawer_edges::seams::person_lookup_now() {
        return stood.switch_on;
    }
    tunnel_core::env::flag("FILES_AS_SEAT_USER", true)
}

/// The person signed in to this process's session, as [`FsAs::now`] asks on
/// its own thread; a test stands the lookup in.
#[cfg(target_os = "windows")]
pub(super) fn session_person() -> Option<crate::win_identity::SessionPerson> {
    #[cfg(test)]
    if let Some(stood) = super::drawer_edges::seams::person_lookup_now() {
        return (stood.lookup)();
    }
    crate::win_identity::session_person()
}
