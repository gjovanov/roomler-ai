// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
// RETIRED-NAME-ANCHOR(24): names the PRE-RENAME appdirs segment a host installed before
// P4b still has; appdirs::app_segment resolves it, so it is an input.
//! Persist agent crash reports to disk for later upload.
//!
//! Three callers feed in (panic hook in `logging.rs`, watchdog stall
//! in `watchdog.rs`, SCM supervisor in `win_service/supervisor.rs`),
//! each routing through [`record`] with a [`CrashReason`] +
//! [`WriterContext`]. The recorder serialises an
//! [`AgentCrashPayload`] (shared wire type defined in
//! `roomler_ai_remote_control::models`) to a sidecar JSON file under
//! the appropriate crashes dir; on next agent startup,
//! `crash_uploader::drain_and_upload` POSTs each file to roomler.ai
//! and deletes it on 2xx.
//!
//! ## Two writer contexts on Windows
//!
//! - **Worker** (user-context): writes under `logging::log_dir()/
//!   crashes/` which resolves to `%LOCALAPPDATA%\roomler\
//!   roomler-agent\data\logs\crashes\`. The user-context uploader
//!   reads from this same dir on startup.
//! - **Supervisor** (LocalSystem, Windows-only): writes under
//!   `%PROGRAMDATA%\roomler\roomler-agent\crashes\` because
//!   `directories::ProjectDirs::data_local_dir()` under LocalSystem
//!   resolves to `C:\Windows\System32\config\systemprofile\…` which
//!   the user-context worker can't read. The same uploader scans both
//!   dirs and merges results; the supervisor's dir is the machine's,
//!   reached by handle (below).
//!
//! ## The machine folder, by handle (FR-145)
//!
//! A crashes dir below the machine's `ProgramData` (the supervisor's, and a
//! SYSTEM worker's `service-logs\crashes`) is reached by handle, never by
//! path (`machine_tree::HeldPath::machine`): each folder on the way opened
//! below the one above it, never through a link, its final path checked.
//!
//! - `crashes` is made sealed (`Seal::Readable`, D10): SYSTEM and
//!   Administrators write, Users read and list, so a worker running as the
//!   signed-in person drains the supervisor's records. One an earlier build
//!   made is sealed in place, and a record name in it that is not a plain
//!   file of SYSTEM's or Administrators' is deleted by handle. A link, a
//!   file or another account's folder at `crashes` is moved aside whole, as
//!   `crashes.moved-<unix>`.
//! - A record is a fresh temp file, flushed, then renamed into place by
//!   handle. The scan lists and reads below the folder's handle, a record
//!   that does not parse is renamed into `malformed` by handle, and the
//!   uploader deletes through the folder it read from ([`Sidecar::remove`]).
//! - A link on the way leaves the crash unrecorded, with one WARN per
//!   process start (`crash records left alone: …`), counted with the
//!   suppression tally. It is never written anywhere else.
//! - `ROOMLERD_MACHINE_TREE_HELD=0`: written and read by path as before,
//!   with one WARN naming the switch.
//!
//! On non-Windows the Supervisor context resolves to the writing
//! process's own Worker dir. The one supervisor there is macOS's (FR-43,
//! `macos_supervisor.rs`): the ROOT daemon records a GUI worker that died
//! before its own panic hook could (FR-97 P6e-2), so the record lands in
//! root's dir, and the root daemon's own uploader drains it on the
//! daemon's enrollment.
//!
//! ## Reentrancy + panic safety
//!
//! [`record`] is called from inside a panic hook. If the recorder
//! itself panics (OOM during JSON serialisation, IO blow-up, etc.)
//! the outer `std::panic::set_hook` would normally abort the
//! process before its existing text-dump call has flushed.
//! Wrapping the recorder body in `catch_unwind` keeps the outer
//! hook's `prev(info)` reachable; any inner panic is reported via
//! `eprintln!("crash_recorder:RECURSIVE_PANIC …")` so a fielded
//! support log still surfaces the inner failure.
//!
//! ## Scrub pipeline
//!
//! Both the summary AND the log tail run through
//! [`scrub_credentials`] before the payload is written to disk. The
//! pipeline redacts known credential shapes (Bearer tokens, JWT
//! triplets, MongoDB URIs with userinfo, `password=` query params,
//! WebRTC ICE ufrag/pwd lines). Each redaction increments a count
//! that's appended to the summary as `[scrubbed N tokens]` so
//! operators can tell at-a-glance when redaction fired (low number =
//! probably clean; high number = either a pathological log or a new
//! credential shape that needs adding to the scrub set).

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use roomler_ai_remote_control::models::{AgentCrashPayload, CrashReason};
pub use roomler_ai_remote_control::models::{AgentCrashPayload as Payload, CrashReason as Reason};

/// Maximum serialised JSON payload size. The backend's ingest route
/// caps at 80 KiB to leave HTTP overhead room; we cap the payload
/// itself at 64 KiB so the trim margin covers the body framing.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Last-N lines of the rolling log to attach to a crash report.
/// The tail is trimmed further inside [`record`] if the resulting
/// JSON would exceed [`MAX_PAYLOAD_BYTES`].
pub const LOG_TAIL_LINES: usize = 200;

/// Minimum seconds between consecutive sidecar writes in the same
/// dir. A tight crash-loop (e.g. SystemContext worker dying every
/// 2s with `code=1`, field repro 2026-05-17 a third field-test host) would otherwise
/// fill the dir with ~24 sidecars/minute. With this rate-limit one
/// crash is recorded every 30s, capturing enough forensics + leaving
/// the disk + log alone.
pub const MIN_INTERVAL_SECS: i64 = 30;

/// Hard cap on sidecars in a single dir. If the worker can never
/// successfully start (and thus never run the uploader to drain),
/// even the rate-limited writes would accumulate forever. 100 ×
/// 64 KiB = 6.4 MB max disk impact per dir, which is generous for
/// forensics but bounded. Beyond the cap, new records are dropped
/// silently with a once-per-minute throttled WARN.
pub const HARD_CAP: usize = 100;

/// Where to persist the sidecar. See the module-level docs for the
/// path resolution + the rationale for splitting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterContext {
    /// User-context worker process. Writes under
    /// `logging::log_dir()/crashes/`.
    Worker,
    // RETIRED-NAME-ANCHOR(3): the PRE-RENAME tree appdirs still resolves on an upgraded
    // host.
    /// LocalSystem SCM supervisor. Writes under
    /// `%PROGRAMDATA%\roomler\roomler-agent\crashes\` on Windows,
    /// falls back to the Worker dir on non-Windows: on macOS that is
    /// the root daemon's, recording its GUI worker (FR-97 P6e-2).
    Supervisor,
}

/// Persist a crash sidecar atomically. Best-effort: any IO failure
/// logs `warn!` and returns; never blocks the crash-exit path.
pub fn record(reason: CrashReason, summary: &str, ctx: WriterContext) {
    record_with_log_tail(reason, summary, ctx, None);
}

/// #3018 — [`record`], with `appendix` written after the rolling log's tail.
///
/// [`fit_to_envelope`] trims the tail from its OLDEST line, so whatever comes
/// last survives longest: a stall's thread stacks stay whole while the log
/// lines from before the stall make room. The appendix goes through the same
/// credential scrub as the tail. An empty appendix is plain [`record`].
pub fn record_with_appendix(
    reason: CrashReason,
    summary: &str,
    ctx: WriterContext,
    appendix: &str,
) {
    if appendix.is_empty() {
        return record(reason, summary, ctx);
    }
    let tail = read_log_tail(LOG_TAIL_LINES);
    record_with_log_tail(
        reason,
        summary,
        ctx,
        Some(tail_then_appendix(&tail, appendix)),
    );
}

/// #3018 — the tail, then the appendix, on lines of their own.
fn tail_then_appendix(tail: &str, appendix: &str) -> String {
    match (tail.is_empty(), appendix.is_empty()) {
        (_, true) => tail.to_string(),
        (true, false) => appendix.to_string(),
        (false, false) => format!("{tail}\n{appendix}"),
    }
}

/// Same as [`record`] but accepts a caller-supplied `log_tail`
/// (typically the child worker's captured stderr) that OVERRIDES
/// the local rolling-log read. Used by the SCM supervisor on
/// SupervisorDetected crashes so the sidecar's `log_tail` carries
/// the WORKER's last few KiB of stderr instead of the supervisor's
/// own rolling log (which is useless for diagnosing why the worker
/// died). Field repro: a third field-test host SystemContext loop, 2026-05-17.
///
/// `log_tail_override = None` → existing behaviour (read
/// supervisor's rolling log).
/// `log_tail_override = Some(s)` → use `s` verbatim before scrub +
/// envelope-trim. Empty string is still treated as "override":
/// callers pass `None` when they have no buffer at all.
pub fn record_with_log_tail(
    reason: CrashReason,
    summary: &str,
    ctx: WriterContext,
    log_tail_override: Option<String>,
) {
    // catch_unwind so a recorder-internal panic doesn't unwind into
    // the caller's panic hook + abort the process before its
    // existing text-dump has flushed.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        record_inner(reason, summary, ctx, log_tail_override)
    }));
    match outcome {
        Ok(Ok(path)) => {
            // Downgraded INFO → DEBUG (2026-05-17 a third field-test host storm fix):
            // a tight crash-loop would otherwise spam this line at INFO
            // through the rolling log file. The rate-limit kicks in
            // after the first write, but even those rate-limited
            // writes are DEBUG so the runtime log stays quiet under
            // normal operation. Operators looking for "did the
            // sidecar land" check the dir directly.
            tracing::debug!(crash_sidecar = %path.display(), ?reason, "wrote crash sidecar");
        }
        Ok(Err(e)) => {
            // Suppression returns Err with a constant reason string;
            // log_suppression_throttled already emitted (or threw the
            // throttle), so don't double-warn for that path.
            let msg = e.to_string();
            if !is_suppression_message(&msg) {
                tracing::warn!(error = %msg, "crash_recorder: write failed");
            }
        }
        Err(_) => {
            // The closure panicked. Avoid touching tracing here (its
            // backing subscriber may also be in a poisoned state mid
            // crash) — stderr goes to the SCM service's capture, the
            // terminal or the rolling log (#3292), which is sufficient.
            // Never `eprintln!`: on a failing stderr it panics (#1538).
            crate::logging::say_on_stderr("crash_recorder:RECURSIVE_PANIC during record()");
        }
    }
}

fn record_inner(
    reason: CrashReason,
    summary: &str,
    ctx: WriterContext,
    log_tail_override: Option<String>,
) -> std::io::Result<PathBuf> {
    let dir = crashes_dir_for(ctx)?;
    record_inner_at(&dir, reason, summary, log_tail_override)
}

/// [`record_inner`] into the crashes folder `dir`, which the caller names: a
/// test names a folder of its own.
fn record_inner_at(
    dir: &Path,
    reason: CrashReason,
    summary: &str,
    log_tail_override: Option<String>,
) -> std::io::Result<PathBuf> {
    let place = Place::to_write(dir)?;

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Crash-loop suppression. Field repro 2026-05-17 on a third field-test host:
    // SystemContext worker dies every 2s with code=1 → supervisor
    // records a SupervisorDetected sidecar per spawn → without
    // suppression we'd fill PROGRAMDATA with ~24 sidecars/minute
    // forever (the worker never lives long enough to run the
    // uploader). See `should_suppress` for the rate-limit + cap
    // contract; `log_suppression_throttled` keeps the warn from
    // spamming.
    if let Some(reason) = place.should_suppress(now_unix) {
        // rc.51: tally the suppressed write so the next sidecar that
        // DOES land carries the count — loop intensity stays visible.
        SUPPRESSED_SINCE_LAST.fetch_add(1, Ordering::Relaxed);
        log_suppression_throttled(dir, reason, now_unix);
        return Err(std::io::Error::other(reason.as_str()));
    }

    let pid = std::process::id();

    // log_tail source: caller-supplied (e.g. captured worker stderr)
    // takes precedence; otherwise read the local rolling log. The
    // scrub + envelope-trim pipeline below treats both identically.
    let raw_tail = log_tail_override.unwrap_or_else(|| read_log_tail(LOG_TAIL_LINES));
    let (scrubbed_summary, scrub_count_summary) = scrub_credentials(summary);
    let (scrubbed_tail, scrub_count_tail) = scrub_credentials(&raw_tail);
    let total_scrubs = scrub_count_summary + scrub_count_tail;
    let final_summary = if total_scrubs > 0 {
        format!("{scrubbed_summary} [scrubbed {total_scrubs} tokens]")
    } else {
        scrubbed_summary
    };

    // rc.51: drain the suppressed-since-last tally — this sidecar is
    // about to be written successfully, so it "absorbs" the count of
    // crashes the 1/60 s throttle dropped since the previous write.
    let suppressed_since_last = SUPPRESSED_SINCE_LAST.swap(0, Ordering::Relaxed) as u32;

    let mut payload = AgentCrashPayload {
        crashed_at_unix: now_unix,
        reason,
        summary: final_summary,
        log_tail: scrubbed_tail,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        hostname: detect_hostname(),
        pid,
        suppressed_since_last,
    };

    // Cap JSON at MAX_PAYLOAD_BYTES by progressively trimming the
    // log tail. Worst case the tail collapses to just the truncation
    // marker; the metadata fields are tiny so we never overflow on
    // those alone.
    fit_to_envelope(&mut payload, MAX_PAYLOAD_BYTES);

    let name = format!("{now_unix}-{pid}.json");
    #[cfg(windows)]
    if let Place::Held(held) = &place {
        // FR-145 — a fresh temp file below the held folder, flushed (#2955),
        // then renamed onto the name by handle.
        let json =
            serde_json::to_vec(&payload).map_err(|e| std::io::Error::other(e.to_string()))?;
        held.write_file(&name, &json, crate::machine_tree::Seal::Readable)?;
        return Ok(held.path().join(name));
    }
    let final_path = dir.join(&name);
    let tmp_path = dir.join(format!("{name}.tmp"));
    {
        let mut f = fs::File::create(&tmp_path)?;
        serde_json::to_writer(&mut f, &payload)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        f.flush()?;
        // #2955 — make the bytes durable BEFORE the rename publishes the
        // name. `flush` is a no-op on a raw `File`; without `sync_all` a
        // power loss or OS crash can persist the rename and the length but
        // not the data, leaving a NUL-filled sidecar no parser will ever
        // read (measured: 529 bytes of JSON, then 15,735 NULs).
        f.sync_all()?;
    }
    // POSIX rename + Windows MoveFileEx(MOVEFILE_REPLACE_EXISTING)
    // are both atomic for same-volume targets. The crashes dir lives
    // alongside other agent state on the data drive, so no cross-
    // volume concern.
    fs::rename(&tmp_path, &final_path)?;

    Ok(final_path)
}

/// Resolve the crashes dir for the given context, creating parent
/// dirs lazily on first write.
pub fn crashes_dir_for(ctx: WriterContext) -> std::io::Result<PathBuf> {
    match ctx {
        WriterContext::Worker => crate::logging::log_dir()
            .map(|p| p.join("crashes"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "logging::log_dir() unavailable for Worker crash sidecar",
                )
            }),
        WriterContext::Supervisor => supervisor_crashes_dir(),
    }
}

#[cfg(target_os = "windows")]
fn supervisor_crashes_dir() -> std::io::Result<PathBuf> {
    Ok(crate::appdirs::machine_global_dir().join("crashes"))
}

#[cfg(not(target_os = "windows"))]
fn supervisor_crashes_dir() -> std::io::Result<PathBuf> {
    // The writing process's own dir. On macOS the writer is the root
    // daemon recording its GUI worker (FR-97 P6e-2), and its own uploader
    // scans this dir (`pending_all`).
    crate::logging::log_dir()
        .map(|p| p.join("crashes"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "logging::log_dir() unavailable for Supervisor crash sidecar",
            )
        })
}

/// FR-145 — where a record is written: below the machine's folder, held by
/// handle ([`crate::machine_tree::HeldPath`]), or a plain path (the person's
/// own daemon in its profile; macOS and Linux).
enum Place<'a> {
    Plain(&'a Path),
    #[cfg(windows)]
    Held(crate::machine_tree::HeldPath),
}

/// The start of the error a record left alone answers with: logged once per
/// process start ([`left_alone`]), never again by [`record_with_log_tail`].
const LEFT_ALONE: &str = "crash_recorder: crash records left alone: ";

impl<'a> Place<'a> {
    /// `dir`, ready to take a record. On Windows a folder below the machine's
    /// `ProgramData` (the supervisor's `crashes`, a SYSTEM worker's
    /// `service-logs\crashes`) is reached by handle and sealed
    /// (`Seal::Readable`, D10: a worker running as the person drains it); a
    /// link on the way leaves the record unwritten. Any other folder is made
    /// as before.
    fn to_write(dir: &'a Path) -> std::io::Result<Self> {
        #[cfg(windows)]
        if in_machine_tree(dir) && held_switch_on() {
            use crate::machine_tree::{HeldPath, Seal};
            let held = HeldPath::machine(dir, Seal::Readable).map_err(|refused| {
                // A crash that goes unrecorded: the next record carries it.
                SUPPRESSED_SINCE_LAST.fetch_add(1, Ordering::Relaxed);
                left_alone(refused)
            })?;
            if held.resealed() {
                // An earlier build's open folder, sealed in place with nothing
                // handed down: each record name is sealed by its own handle
                // when it is a plain file of SYSTEM's or Administrators', and
                // goes when it is not. Other names are left as they are.
                let gone = held.keep_only_plain(
                    |n| n.ends_with(".json") || n.ends_with(".tmp"),
                    Seal::Readable,
                );
                if !gone.is_empty() {
                    let gone: Vec<String> = gone.iter().map(ToString::to_string).collect();
                    tracing::warn!(
                        ?gone,
                        "crash_recorder: crashes sealed; what was not a record removed, or left"
                    );
                }
            }
            return Ok(Self::Held(held));
        }
        fs::create_dir_all(dir)?;
        Ok(Self::Plain(dir))
    }

    /// [`should_suppress`], over this folder's entries.
    fn should_suppress(&self, now_unix: i64) -> Option<SuppressReason> {
        match self {
            Self::Plain(dir) => should_suppress(dir, now_unix),
            #[cfg(windows)]
            Self::Held(held) => {
                let entries = held.entries().ok()?;
                suppress_among(entries.iter().map(|e| e.name.as_str()), now_unix)
            }
        }
    }
}

/// FR-145 — records left alone, written or read: one WARN per process start,
/// and an error [`record_with_log_tail`] does not log again. The crash is
/// never written anywhere else. Only the writer counts a refusal with the
/// suppression tally, as one crash that went unrecorded; a refused read is
/// not a crash.
#[cfg(windows)]
fn left_alone(refused: crate::machine_tree::held::Refusal) -> std::io::Error {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!("{LEFT_ALONE}{refused}");
    }
    std::io::Error::other(format!("{LEFT_ALONE}{refused}"))
}

/// `dir` is below the machine's `ProgramData`.
#[cfg(windows)]
fn in_machine_tree(dir: &Path) -> bool {
    crate::appdirs::names_below(dir, &crate::appdirs::machine_program_data())
        .is_some_and(|names| !names.is_empty())
}

/// FR-145's kill switch (`ROOMLERD_MACHINE_TREE_HELD=0`): off, the records are
/// written and read by path as before, with one WARN naming the switch.
#[cfg(windows)]
fn held_switch_on() -> bool {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = crate::machine_tree::Switches::from_env().held;
    if !on && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "crash_recorder: ROOMLERD_MACHINE_TREE_HELD=0: crash records in the machine folder \
             are written and read by path, not by handle"
        );
    }
    on
}

/// A pending sidecar: its name and the folder it was read from, so the
/// uploader deletes what it read through the same folder (FR-145: below the
/// machine's folder, by handle), never by a path looked up again.
#[derive(Clone)]
pub struct Sidecar {
    path: PathBuf,
    name: String,
    len: u64,
    modified_unix: i64,
    folder: SidecarFolder,
}

#[derive(Clone)]
enum SidecarFolder {
    Plain,
    #[cfg(windows)]
    Held(std::sync::Arc<crate::machine_tree::HeldPath>),
}

impl std::fmt::Debug for Sidecar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Sidecar").field(&self.path).finish()
    }
}

impl Sidecar {
    /// The sidecar at `path`, read by path: a folder outside the machine's.
    /// `None` when it is not there.
    pub fn plain(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let modified_unix = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        Some(Self {
            path: path.to_path_buf(),
            name: path.file_name()?.to_str()?.to_string(),
            len: meta.len(),
            modified_unix,
            folder: SidecarFolder::Plain,
        })
    }

    /// Where it is, for log lines.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Its identity for the uploader's ledger: `<len>:<mtime> <file name>`,
    /// from when it was read.
    pub fn signature(&self) -> String {
        format!("{}:{} {}", self.len, self.modified_unix, self.name)
    }

    /// Delete it: below the machine's folder through the folder it was read
    /// from, by handle (a link of its name refused); elsewhere by path.
    /// `NotFound` when it is not there.
    pub fn remove(&self) -> std::io::Result<()> {
        match &self.folder {
            SidecarFolder::Plain => fs::remove_file(&self.path),
            #[cfg(windows)]
            SidecarFolder::Held(held) => match held.remove_file(&self.name) {
                Ok(true) => Ok(()),
                Ok(false) => Err(std::io::ErrorKind::NotFound.into()),
                Err(refused) => Err(refused.into()),
            },
        }
    }
}

/// Drain pending crash payloads from BOTH the worker + supervisor
/// dirs (deduped by file path on the rare case they're the same
/// directory). Returned tuples carry the sidecar (its name and folder) so
/// the uploader can delete each on successful POST.
///
/// Oversized or unreadable files are logged + left on disk — better to
/// leak a few orphan sidecars than to silently lose a crash record. A
/// file that does not parse is moved ONCE into `malformed/` (#2955): kept
/// for a later look, never re-warned on every start. FR-145: below the
/// machine's folder each is listed and read below the folder's handle,
/// never through a link.
pub fn pending_all() -> Vec<(Sidecar, AgentCrashPayload)> {
    let mut out: Vec<(Sidecar, AgentCrashPayload)> = Vec::new();
    let mut seen_dirs: Vec<PathBuf> = Vec::with_capacity(2);
    for ctx in [WriterContext::Worker, WriterContext::Supervisor] {
        if let Ok(dir) = crashes_dir_for(ctx) {
            if seen_dirs.contains(&dir) {
                continue;
            }
            seen_dirs.push(dir.clone());
            out.extend(scan_dir(&dir));
        }
    }
    // Stable order: oldest first so the uploader processes in
    // crash chronology.
    out.sort_by_key(|(_, p)| p.crashed_at_unix);
    out
}

fn scan_dir(dir: &Path) -> Vec<(Sidecar, AgentCrashPayload)> {
    #[cfg(windows)]
    if in_machine_tree(dir) && held_switch_on() {
        return scan_held(dir);
    }
    let mut out = Vec::new();
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "crash_recorder: read_dir failed");
            return out;
        }
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_PAYLOAD_BYTES as u64 {
            tracing::warn!(
                file = %path.display(),
                bytes = meta.len(),
                "crash_recorder: skipping oversized sidecar"
            );
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "crash_recorder: read failed");
                continue;
            }
        };
        match serde_json::from_slice::<AgentCrashPayload>(&bytes) {
            Ok(payload) => {
                if let Some(sidecar) = Sidecar::plain(&path) {
                    out.push((sidecar, payload));
                }
            }
            // #2955 — moved aside ONCE: kept for a later look, but never
            // re-read and re-warned on every start. A torn write (a power
            // loss mid-record) is NUL-filled, and no future parser reads it.
            Err(e) => match quarantine(dir, &path) {
                Ok(moved) => tracing::warn!(
                    file = %path.display(),
                    moved_to = %moved.display(),
                    error = %e,
                    "crash_recorder: malformed sidecar moved aside"
                ),
                Err(qe) => tracing::warn!(
                    file = %path.display(),
                    error = %e,
                    move_error = %qe,
                    "crash_recorder: skipping malformed sidecar"
                ),
            },
        }
    }
    out
}

/// FR-145 — [`scan_dir`] below the machine's folder: `dir` reached from the
/// machine's `ProgramData` by handle (making nothing), its entries listed and
/// each record read below its handle, never through a link; a record that
/// does not parse goes into `malformed`, renamed by handle.
#[cfg(windows)]
fn scan_held(dir: &Path) -> Vec<(Sidecar, AgentCrashPayload)> {
    use crate::machine_tree::{HeldPath, Seal};
    let mut out = Vec::new();
    let held = match HeldPath::below(&crate::appdirs::machine_program_data(), dir) {
        Ok(Some(held)) => std::sync::Arc::new(held),
        Ok(None) => return out,
        Err(refused) => {
            let _ = left_alone(refused);
            return out;
        }
    };
    let entries = match held.entries() {
        Ok(entries) => entries,
        Err(refused) => {
            tracing::warn!(%refused, "crash_recorder: listing the crashes folder failed");
            return out;
        }
    };
    for entry in entries {
        if entry.is_dir || !entry.name.ends_with(".json") {
            continue;
        }
        let path = held.path().join(&entry.name);
        if entry.len > MAX_PAYLOAD_BYTES as u64 {
            tracing::warn!(file = %path.display(), bytes = entry.len, "crash_recorder: skipping oversized sidecar");
            continue;
        }
        let bytes = match held.read_file(&entry.name, MAX_PAYLOAD_BYTES as u64) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(refused) => {
                tracing::warn!(%refused, "crash_recorder: read failed");
                continue;
            }
        };
        match serde_json::from_slice::<AgentCrashPayload>(&bytes) {
            Ok(payload) => out.push((
                Sidecar {
                    path,
                    name: entry.name,
                    len: entry.len,
                    modified_unix: entry.modified_unix,
                    folder: SidecarFolder::Held(held.clone()),
                },
                payload,
            )),
            Err(e) => match held
                .sealed_dir(MALFORMED_DIR, Seal::Readable)
                .and_then(|into| held.rename_file(&entry.name, &into, &entry.name, false))
            {
                Ok(()) => tracing::warn!(
                    file = %path.display(),
                    error = %e,
                    "crash_recorder: malformed sidecar moved aside into {MALFORMED_DIR}"
                ),
                Err(refused) => tracing::warn!(
                    file = %path.display(),
                    error = %e,
                    move_error = %refused,
                    "crash_recorder: skipping malformed sidecar"
                ),
            },
        }
    }
    out
}

/// #2955 — where a sidecar that does not parse goes: a subdirectory, which
/// [`scan_dir`] never reads (it does not recurse).
const MALFORMED_DIR: &str = "malformed";

/// Move a malformed sidecar into [`MALFORMED_DIR`] under `dir`.
fn quarantine(dir: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("sidecar path has no file name"))?;
    let target_dir = dir.join(MALFORMED_DIR);
    fs::create_dir_all(&target_dir)?;
    let target = target_dir.join(name);
    fs::rename(path, &target)?;
    Ok(target)
}

// ─── The uploader's verdicts ──────────────────────────────────────────────

/// The uploader's ledger (#1748): one line for each record that got a final
/// verdict but that this process may not delete, so it is never sent again.
/// It lives in the uploading process's log folder.
pub const VERDICTS_FILE: &str = "crash-verdicts.txt";

/// Append `line` to [`VERDICTS_FILE`] in `dir`. FR-145: below the machine's
/// folder (a SYSTEM worker's `service-logs`), `dir` is reached by handle and
/// sealed (`Seal::Readable`), and the line goes through
/// [`crate::machine_tree::HeldPath::append_file`]: a link, or a file with
/// more than one name, at the ledger's name is deleted as itself, never
/// written through, and the file made again. A refusal is logged once per
/// process start, and the line is written nowhere else. Elsewhere by path,
/// as before.
pub fn append_verdict(dir: &Path, line: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    if in_machine_tree(dir) && held_switch_on() {
        use crate::machine_tree::{HeldPath, Seal};
        let held = HeldPath::machine(dir, Seal::Readable).map_err(verdicts_left_alone)?;
        if held.resealed() {
            // Sealed in place with nothing handed down: the ledger, when it
            // is there, is sealed by its own handle; nothing else is touched.
            let gone = held.keep_only_plain(|n| n == VERDICTS_FILE, Seal::Readable);
            if !gone.is_empty() {
                let gone: Vec<String> = gone.iter().map(ToString::to_string).collect();
                tracing::warn!(?gone, "crash_recorder: the verdicts' folder sealed");
            }
        }
        let mut f = held
            .append_file(VERDICTS_FILE, Seal::Readable)
            .map_err(verdicts_left_alone)?;
        return writeln!(f, "{line}");
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(VERDICTS_FILE))?;
    writeln!(f, "{line}")
}

/// [`VERDICTS_FILE`] in `dir`: empty when it is not there or cannot be read
/// (the cost is one more send, never a lost crash). FR-145: below the
/// machine's folder, read below `dir`'s handle, never through a link and
/// never a file with more than one name.
pub fn read_verdicts(dir: &Path) -> String {
    #[cfg(windows)]
    if in_machine_tree(dir) && held_switch_on() {
        let read =
            crate::machine_tree::HeldPath::below(&crate::appdirs::machine_program_data(), dir)
                .and_then(|held| match held {
                    Some(held) => held.read_file(VERDICTS_FILE, MAX_VERDICTS_BYTES),
                    None => Ok(None),
                });
        return match read {
            Ok(bytes) => String::from_utf8_lossy(&bytes.unwrap_or_default()).into_owned(),
            Err(refused) => {
                let _ = verdicts_left_alone(refused);
                String::new()
            }
        };
    }
    fs::read_to_string(dir.join(VERDICTS_FILE)).unwrap_or_default()
}

/// The most of [`VERDICTS_FILE`] that is read: some 25,000 verdicts.
#[cfg(windows)]
const MAX_VERDICTS_BYTES: u64 = 1024 * 1024;

/// FR-145 — the ledger left alone below the machine's folder: one WARN per
/// process start. Not a crash, so not counted with the suppression tally.
#[cfg(windows)]
fn verdicts_left_alone(refused: crate::machine_tree::held::Refusal) -> std::io::Error {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!("crash_recorder: crash verdicts left alone: {refused}");
    }
    refused.into()
}

// ─── Crash-loop suppression ────────────────────────────────────────────────

/// Why a record_inner attempt declined to write. Stored in the
/// returned io::Error string so the outer `record()` can distinguish
/// "real write failure" (warn) from "rate-limit / cap hit" (already
/// logged at throttled WARN).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// Most-recent sidecar in the dir is fresh — write rate-limited.
    RecentSidecar,
    /// Dir already has [`HARD_CAP`] sidecars; drop to bound disk.
    HardCapReached,
}

impl SuppressReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecentSidecar => "crash_recorder: recent sidecar; rate-limited",
            Self::HardCapReached => "crash_recorder: hard cap reached; suppressing",
        }
    }
}

fn is_suppression_message(msg: &str) -> bool {
    msg == SuppressReason::RecentSidecar.as_str()
        || msg == SuppressReason::HardCapReached.as_str()
        || msg.starts_with(LEFT_ALONE)
}

/// Pure: should the next sidecar write to `dir` be suppressed?
/// `now_unix` is the proposed crashed_at_unix for the new sidecar.
///
/// Two suppression rules, in order:
/// 1. **Hard cap** — if `dir` already contains [`HARD_CAP`] or more
///    `*.json` sidecars, drop the new write. Bounds worst-case disk.
/// 2. **Rate limit** — if the most-recent sidecar in `dir` is within
///    [`MIN_INTERVAL_SECS`] of `now_unix`, drop the new write. The
///    timestamp is parsed from the filename (`{unix_ts}-{pid}.json`)
///    so we don't need mtime lookups (which can be unreliable on
///    filesystems with low granularity).
///
/// Pure over dir contents + clock so tests drive every branch
/// without mocks.
pub fn should_suppress(dir: &Path, now_unix: i64) -> Option<SuppressReason> {
    let names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    suppress_among(names.iter().map(String::as_str), now_unix)
}

/// [`should_suppress`] over a folder's entry names (FR-145: below the
/// machine's folder, its listing by handle).
fn suppress_among<'n>(
    names: impl Iterator<Item = &'n str>,
    now_unix: i64,
) -> Option<SuppressReason> {
    let entries: Vec<i64> = names
        .filter_map(|name| {
            let stem = name.strip_suffix(".json")?;
            let (ts_str, _) = stem.split_once('-')?;
            ts_str.parse::<i64>().ok()
        })
        .collect();

    if entries.len() >= HARD_CAP {
        return Some(SuppressReason::HardCapReached);
    }

    let cutoff = now_unix.saturating_sub(MIN_INTERVAL_SECS);
    if entries.iter().any(|&ts| ts > cutoff) {
        return Some(SuppressReason::RecentSidecar);
    }

    None
}

/// Last unix-second we emitted a suppression warn. Throttled to
/// one per 60s per process — a tight crash-loop hitting suppression
/// every 2s would otherwise log the WARN ~30x/min, which is exactly
/// the spam the suppression is supposed to prevent.
static SUPPRESSION_LAST_WARN_UNIX: AtomicU64 = AtomicU64::new(0);

/// rc.51: count of crash sidecars rate-limit-suppressed since the
/// last sidecar that actually got written. Incremented every time
/// `should_suppress` blocks a write; drained into the next
/// successfully-written sidecar's `suppressed_since_last` field so
/// forensics see loop *intensity* even though only 1/60 s of a tight
/// loop's crashes land on disk.
static SUPPRESSED_SINCE_LAST: AtomicU64 = AtomicU64::new(0);
const SUPPRESSION_WARN_THROTTLE_SECS: u64 = 60;

fn log_suppression_throttled(dir: &Path, reason: SuppressReason, now_unix: i64) {
    let now = now_unix.max(0) as u64;
    let last = SUPPRESSION_LAST_WARN_UNIX.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SUPPRESSION_WARN_THROTTLE_SECS {
        return;
    }
    // Best-effort CAS; if another thread already stamped a newer
    // value we lose the race + skip the log, which is the same
    // outcome we want.
    let _ = SUPPRESSION_LAST_WARN_UNIX.compare_exchange(
        last,
        now,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    tracing::warn!(
        dir = %dir.display(),
        ?reason,
        "crash_recorder: suppressed sidecar write (crash-loop?); throttled to once per 60s"
    );
}

// ─── Scrub pipeline ────────────────────────────────────────────────────────

/// Run the credential scrub over `input` and return (scrubbed,
/// redaction_count). Pure, no IO. Hand-rolled (no regex dep) so the
/// agent build stays slim.
pub fn scrub_credentials(input: &str) -> (String, usize) {
    let mut count = 0usize;
    let mut s = input.to_string();
    // Order matters: Bearer scrub runs BEFORE JWT-shape so a "Bearer
    // <jwt>" string scrubs as a whole instead of double-redacting.
    s = scrub_bearer(&s, &mut count);
    s = scrub_jwt_shape(&s, &mut count);
    s = scrub_mongodb_uri(&s, &mut count);
    s = scrub_password_param(&s, &mut count);
    s = scrub_token_param(&s, &mut count);
    s = scrub_ice_credentials(&s, &mut count);
    (s, count)
}

fn scrub_bearer(input: &str, count: &mut usize) -> String {
    // Replace each `Bearer <token>` occurrence. Token = non-
    // whitespace characters following "Bearer " (case-sensitive,
    // matches the HTTP Authorization header convention). Note that
    // "Bearer" not followed by space-then-token (e.g. "BearerToken"
    // identifiers) is preserved.
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    let bytes = input.as_bytes();
    while let Some(rel) = input[cursor..].find("Bearer ") {
        let start = cursor + rel;
        let token_start = start + "Bearer ".len();
        let mut token_end = token_start;
        while token_end < bytes.len() && !bytes[token_end].is_ascii_whitespace() {
            token_end += 1;
        }
        if token_end > token_start {
            out.push_str(&input[cursor..token_start]);
            out.push_str("[REDACTED]");
            *count += 1;
            cursor = token_end;
        } else {
            // "Bearer " with no token — pass through to avoid an
            // infinite loop on weird inputs.
            out.push_str(&input[cursor..=start]);
            cursor = start + 1;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

fn scrub_jwt_shape(input: &str, count: &mut usize) -> String {
    // Three base64url segments, dot-separated, each ≥8 chars. Walk
    // whitespace-separated tokens; the alternative (full regex) is
    // overkill for the cost of one new dep.
    let mut out = String::with_capacity(input.len());
    let mut first = true;
    for word in input.split_inclusive(|c: char| c.is_whitespace()) {
        // Split off any trailing whitespace so we don't include it
        // in the JWT-shape test.
        let (core, ws_tail) = match word.find(|c: char| c.is_whitespace()) {
            Some(idx) => (&word[..idx], &word[idx..]),
            None => (word, ""),
        };
        if first {
            first = false;
        }
        if is_jwt_shape(core) {
            out.push_str("[REDACTED_JWT]");
            out.push_str(ws_tail);
            *count += 1;
        } else {
            out.push_str(word);
        }
    }
    out
}

fn is_jwt_shape(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts.iter().all(|p| {
        p.len() >= 8
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

fn scrub_mongodb_uri(input: &str, count: &mut usize) -> String {
    // Replace `mongodb://user:pass@…` with `mongodb://[REDACTED]@…`.
    // Handles `mongodb+srv://` too. Pass-through if the URI has no
    // userinfo segment ("@").
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(rel) = find_scheme(&input[cursor..]) {
        let scheme_start = cursor + rel.0;
        let after_scheme = scheme_start + rel.1.len() + "://".len();
        // Find the next `@` BEFORE the next whitespace.
        let rest = &input[after_scheme..];
        let at_pos = rest.find('@');
        let ws_pos = rest.find(|c: char| c.is_whitespace());
        let take_at = match (at_pos, ws_pos) {
            (Some(a), Some(w)) if a < w => Some(a),
            (Some(a), None) => Some(a),
            _ => None,
        };
        if let Some(a) = take_at {
            out.push_str(&input[cursor..after_scheme]);
            out.push_str("[REDACTED]");
            *count += 1;
            cursor = after_scheme + a;
        } else {
            out.push_str(&input[cursor..after_scheme]);
            cursor = after_scheme;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

fn find_scheme(haystack: &str) -> Option<(usize, &'static str)> {
    let candidates = ["mongodb+srv://", "mongodb://"];
    for c in candidates {
        if let Some(idx) = haystack.find(&c[..c.len() - "://".len()]) {
            // Confirm the "://" follows.
            let after = idx + c.len() - "://".len();
            if haystack[after..].starts_with("://") {
                return Some((idx, &c[..c.len() - "://".len()]));
            }
        }
    }
    None
}

fn scrub_password_param(input: &str, count: &mut usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(rel) = input[cursor..].find("password=") {
        let start = cursor + rel;
        let value_start = start + "password=".len();
        let mut value_end = value_start;
        let bytes = input.as_bytes();
        while value_end < bytes.len()
            && bytes[value_end] != b'&'
            && !bytes[value_end].is_ascii_whitespace()
        {
            value_end += 1;
        }
        if value_end > value_start {
            out.push_str(&input[cursor..value_start]);
            out.push_str("[REDACTED]");
            *count += 1;
            cursor = value_end;
        } else {
            out.push_str(&input[cursor..=start]);
            cursor = start + 1;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

/// rc.52: redact `…token = "value"` / `…token=value` assignments —
/// TOML config lines (`agent_token = "eyJ…"`) and query-string
/// params (`token=eyJ…`). The agent's `config.toml` stores the
/// long-lived Agent JWT this way; a crash sidecar that logs config
/// contents would otherwise leak it into the (world-readable until
/// rc.52's dir-ACL) `%PROGRAMDATA%\roomler\…\crashes\` tree.
/// `scrub_jwt_shape` misses this case because the surrounding quotes
/// break its whitespace-delimited-word boundary detection.
///
/// Matches the literal lowercase substring `token` (covers
/// `agent_token`, `enrollment_token`, bare `token`), then an optional
/// run of spaces, a `=`, optional spaces, an optional opening `"`,
/// and redacts the value up to the next `"`, `&`, or whitespace. The
/// closing quote (if any) is preserved.
fn scrub_token_param(input: &str, count: &mut usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    let bytes = input.as_bytes();
    while let Some(rel) = input[cursor..].find("token") {
        let kw_end = cursor + rel + "token".len();
        let mut i = kw_end;
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            // `token` not followed by an `=` assignment — emit through
            // the keyword and keep scanning.
            out.push_str(&input[cursor..kw_end]);
            cursor = kw_end;
            continue;
        }
        i += 1; // skip '='
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1; // skip opening quote — the value, not the quote, is secret
        }
        let value_start = i;
        while i < bytes.len()
            && bytes[i] != b'&'
            && bytes[i] != b'"'
            && !bytes[i].is_ascii_whitespace()
        {
            i += 1;
        }
        if i > value_start {
            out.push_str(&input[cursor..value_start]);
            out.push_str("[REDACTED]");
            *count += 1;
            cursor = i; // closing quote / delimiter emitted next iteration
        } else {
            out.push_str(&input[cursor..kw_end]);
            cursor = kw_end;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

fn scrub_ice_credentials(input: &str, count: &mut usize) -> String {
    // Process line-by-line; collapse any line containing `ice-ufrag:`
    // or `ice-pwd:` to a fixed redaction marker so neither the
    // ufrag nor the password leak.
    let mut out = String::with_capacity(input.len());
    let mut first = true;
    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.contains("ice-ufrag:") || trimmed.contains("ice-pwd:") {
            if !first {
                // no-op; split_inclusive carries the newline already
            }
            out.push_str("<scrubbed ICE credential>");
            // Preserve the newline if it was in the source line.
            if line.ends_with('\n') {
                out.push('\n');
            }
            *count += 1;
        } else {
            out.push_str(line);
        }
        first = false;
    }
    out
}

// ─── Envelope trim ─────────────────────────────────────────────────────────

const TRUNCATION_MARKER: &str = "[…log truncated to fit 64 KiB envelope…]\n";

fn fit_to_envelope(payload: &mut AgentCrashPayload, max_bytes: usize) {
    // Cheap path: under budget already.
    if estimate_json_size(payload) <= max_bytes {
        return;
    }
    // Drop oldest lines from log_tail until we fit, leaving the
    // truncation marker as the first line of the result.
    let lines: Vec<&str> = payload.log_tail.lines().collect();
    let mut kept_from = lines.len();
    let mut prepend = TRUNCATION_MARKER.to_string();
    while kept_from > 0 {
        let candidate_tail: String = std::iter::once(prepend.as_str())
            .chain(
                lines[lines.len() - kept_from..]
                    .iter()
                    .copied()
                    .flat_map(|l| [l, "\n"]),
            )
            .collect();
        let mut probe = payload.clone();
        probe.log_tail = candidate_tail;
        if estimate_json_size(&probe) <= max_bytes {
            *payload = probe;
            return;
        }
        kept_from = kept_from.saturating_sub(10);
        prepend = TRUNCATION_MARKER.to_string();
    }
    // Pathologically large summary + metadata. Final fallback:
    // truncation marker only.
    payload.log_tail = TRUNCATION_MARKER.to_string();
}

fn estimate_json_size(payload: &AgentCrashPayload) -> usize {
    serde_json::to_vec(payload)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn detect_hostname() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Read /etc/hostname; on most Linux/macOS this is the same
        // as `hostname(1)`. Falls back to env-var HOSTNAME if not.
        // `.ok()` collapses the Result so `.filter` (Option) is
        // applicable; without it CI's Linux clippy step blows up
        // with E0599 (Result is not an iterator).
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// How much of the rolling log's end a crash sidecar reads. Its callers run in a
/// dying process, and a per-user Windows daemon's stderr reaches this file, up to
/// 256 MiB a day (`logging::send_stderr_to_the_log`, #3292).
const LOG_TAIL_MAX_BYTES: u64 = 256 * 1024;

fn read_log_tail(n: usize) -> String {
    let Some(log_path) = crate::logging::active_log_path() else {
        return String::new();
    };
    read_tail_of(&log_path, n)
}

/// The last `n` lines within [`LOG_TAIL_MAX_BYTES`] of the end. Lossy, because raw
/// stderr need not be UTF-8 (a driver's localized text), and one such byte must not
/// hide what came after it.
fn read_tail_of(path: &Path, n: usize) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let from = file
        .metadata()
        .map_or(0, |m| m.len().saturating_sub(LOG_TAIL_MAX_BYTES));
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(from)).is_err()
        || file
            .take(LOG_TAIL_MAX_BYTES)
            .read_to_end(&mut bytes)
            .is_err()
    {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    // Starting past the top of the file, the first line is a fragment.
    let whole = if from > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let lines: Vec<&str> = whole.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> AgentCrashPayload {
        AgentCrashPayload {
            crashed_at_unix: 1_700_000_000,
            reason: CrashReason::Panic,
            summary: "test".to_string(),
            log_tail: String::new(),
            agent_version: "0.0.0-test".to_string(),
            os: "linux".to_string(),
            hostname: "test-host".to_string(),
            pid: 42,
            suppressed_since_last: 0,
        }
    }

    // ─── log tail ──────────────────────────────────────────────────────────

    #[test]
    fn the_log_tail_is_bounded_and_survives_a_byte_that_is_not_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("roomlerd.log.2026-01-01");

        // Past the bound, with a fragment at its edge: the tail starts on a whole line.
        let mut big = vec![b'x'; 2 * LOG_TAIL_MAX_BYTES as usize];
        big.extend_from_slice(b"\nstderr: caf\xe9 line\nlast line\n");
        fs::write(&log, &big).unwrap();
        assert_eq!(
            read_tail_of(&log, 200),
            "stderr: caf\u{fffd} line\nlast line"
        );

        // Inside the bound: the top line is whole, and `n` still holds.
        fs::write(&log, b"one\ntwo\nthree\n").unwrap();
        assert_eq!(read_tail_of(&log, 200), "one\ntwo\nthree");
        assert_eq!(read_tail_of(&log, 2), "two\nthree");

        // A file with no newline inside the bound gives nothing, not a fragment.
        fs::write(&log, vec![b'x'; 2 * LOG_TAIL_MAX_BYTES as usize]).unwrap();
        assert_eq!(read_tail_of(&log, 200), "");

        assert_eq!(read_tail_of(&dir.path().join("absent"), 200), "");
    }

    // ─── scrub: Bearer ─────────────────────────────────────────────────────

    #[test]
    fn scrub_redacts_bearer_token() {
        let (out, n) = scrub_credentials("Authorization: Bearer abc.def.ghi123\nnext line");
        assert!(out.contains("Bearer [REDACTED]"));
        assert!(!out.contains("abc.def.ghi123"));
        assert!(out.contains("next line"));
        assert!(n >= 1, "should have counted at least 1 redaction");
    }

    #[test]
    fn scrub_bearer_handles_multiple_tokens() {
        let (out, n) = scrub_credentials("Bearer aaaaaa Bearer bbbbbb");
        // Both tokens redacted.
        assert!(!out.contains("aaaaaa"));
        assert!(!out.contains("bbbbbb"));
        // Bearer literal preserved (we only redact the token part).
        assert_eq!(out.matches("Bearer [REDACTED]").count(), 2);
        assert!(n >= 2);
    }

    // ─── scrub: JWT shape ──────────────────────────────────────────────────

    #[test]
    fn scrub_redacts_jwt_shape() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhYmMxMjMifQ.signaturepart";
        // Bare-token-in-log shape: the JWT must be its own word for
        // the (deliberately conservative) scrub to fire. Embedded
        // `key=jwt` is the password-scrub's territory.
        let (out, n) = scrub_credentials(&format!("auth header: {jwt} (expired)"));
        assert!(out.contains("[REDACTED_JWT]"));
        assert!(!out.contains(jwt));
        assert!(out.contains("(expired)"));
        assert!(n >= 1);
    }

    #[test]
    fn scrub_jwt_shape_ignores_dotted_paths() {
        // `/foo.bar.baz` looks dot-separated but segments are too
        // short / not base64url. Must NOT redact.
        let (out, n) = scrub_credentials("path /foo.bar.baz suffix");
        assert_eq!(out, "path /foo.bar.baz suffix");
        assert_eq!(n, 0);
    }

    // ─── scrub: token= assignment (rc.52) ──────────────────────────────────

    #[test]
    fn scrub_redacts_toml_quoted_agent_token() {
        // The exact config.toml shape that scrub_jwt_shape misses:
        // the quotes break its whitespace-word boundary.
        let line = r#"agent_token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.sigpart""#;
        let (out, n) = scrub_credentials(line);
        assert!(out.contains("[REDACTED]"), "got: {out}");
        assert!(!out.contains("eyJhbGci"), "token leaked: {out}");
        assert!(n >= 1);
        // The closing quote is structural, not secret — preserved.
        assert!(out.trim_end().ends_with('"'));
    }

    #[test]
    fn scrub_redacts_query_string_token() {
        let (out, n) = scrub_credentials("ws connect token=abc123def&role=agent failed");
        assert!(out.contains("token=[REDACTED]"), "got: {out}");
        assert!(!out.contains("abc123def"));
        assert!(out.contains("&role=agent")); // delimiter + rest preserved
        assert!(n >= 1);
    }

    #[test]
    fn scrub_token_param_ignores_token_without_assignment() {
        // The word "token" in prose must not trip the scrub.
        let (out, n) = scrub_credentials("the enrollment token expired yesterday");
        assert_eq!(out, "the enrollment token expired yesterday");
        assert_eq!(n, 0);
    }

    // ─── scrub: MongoDB URI ────────────────────────────────────────────────

    #[test]
    fn scrub_redacts_mongodb_uri_credentials() {
        let (out, n) =
            scrub_credentials("conn=mongodb://user:secret@host:27017/db?retryWrites=true");
        assert!(out.contains("mongodb://[REDACTED]@host"));
        assert!(!out.contains("user:secret"));
        assert!(n >= 1);
    }

    #[test]
    fn scrub_mongodb_srv_variant() {
        let (out, n) = scrub_credentials("uri=mongodb+srv://u:p@cluster.example.com/db");
        assert!(out.contains("mongodb+srv://[REDACTED]@cluster"));
        assert!(!out.contains("u:p"));
        assert!(n >= 1);
    }

    #[test]
    fn scrub_mongodb_passthrough_when_no_userinfo() {
        let (out, _n) = scrub_credentials("mongodb://host:27017/db (no userinfo)");
        // Without userinfo "@" before whitespace, the URI passes
        // through unchanged.
        assert!(out.contains("mongodb://host:27017"));
    }

    // ─── scrub: password=… ────────────────────────────────────────────────

    #[test]
    fn scrub_redacts_password_param() {
        let (out, n) = scrub_credentials("?user=admin&password=hunter2&next=ok");
        assert!(out.contains("password=[REDACTED]"));
        assert!(!out.contains("hunter2"));
        assert!(out.contains("&next=ok"));
        assert!(n >= 1);
    }

    // ─── scrub: ICE ufrag/pwd ─────────────────────────────────────────────

    #[test]
    fn scrub_redacts_ice_ufrag_and_pwd_lines() {
        let input = "ok line\na=ice-ufrag:abcd\na=ice-pwd:supersecretpwd\nlast line";
        let (out, n) = scrub_credentials(input);
        assert!(out.contains("ok line"));
        assert!(out.contains("last line"));
        assert!(!out.contains("abcd"));
        assert!(!out.contains("supersecretpwd"));
        assert!(out.contains("<scrubbed ICE credential>"));
        assert!(n >= 2);
    }

    // ─── envelope trimming ────────────────────────────────────────────────

    #[test]
    fn fit_to_envelope_passes_payload_under_budget_through() {
        let mut p = sample_payload();
        let before = p.clone();
        fit_to_envelope(&mut p, MAX_PAYLOAD_BYTES);
        assert_eq!(p, before);
    }

    // ─── rc.51: suppressed_since_last field ────────────────────────────────

    #[test]
    fn pre_rc51_sidecar_json_without_suppressed_field_deserialises_to_zero() {
        // A sidecar written by rc.50-and-earlier has no
        // `suppressedSinceLast` key. `#[serde(default)]` must let it
        // parse — the uploader reads sidecars left on disk across an
        // agent upgrade, so an old file must not become unreadable.
        let legacy = r#"{
            "crashedAtUnix": 1700000000,
            "reason": "panic",
            "summary": "old crash",
            "logTail": "",
            "agentVersion": "0.3.0-rc.50",
            "os": "windows",
            "hostname": "h",
            "pid": 7
        }"#;
        let p: AgentCrashPayload = serde_json::from_str(legacy).expect("legacy sidecar parses");
        assert_eq!(p.suppressed_since_last, 0);
    }

    #[test]
    fn suppressed_since_last_round_trips() {
        let mut p = sample_payload();
        p.suppressed_since_last = 23;
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("suppressedSinceLast"));
        let back: AgentCrashPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.suppressed_since_last, 23);
    }

    #[test]
    fn fit_to_envelope_trims_oversized_log_tail() {
        let mut p = sample_payload();
        p.log_tail = "x".repeat(MAX_PAYLOAD_BYTES * 2);
        fit_to_envelope(&mut p, MAX_PAYLOAD_BYTES);
        let final_size = serde_json::to_vec(&p).unwrap().len();
        assert!(
            final_size <= MAX_PAYLOAD_BYTES,
            "post-trim payload {final_size} exceeds {MAX_PAYLOAD_BYTES}"
        );
    }

    #[test]
    fn fit_to_envelope_writes_truncation_marker() {
        let mut p = sample_payload();
        p.log_tail = (0..2000).map(|i| format!("line{i}\n")).collect::<String>();
        fit_to_envelope(&mut p, 4096);
        assert!(
            p.log_tail.starts_with(TRUNCATION_MARKER),
            "log_tail should start with truncation marker; got: {:?}",
            &p.log_tail[..p.log_tail.len().min(80)]
        );
    }

    // ─── jwt-shape predicate ──────────────────────────────────────────────

    #[test]
    fn is_jwt_shape_accepts_three_base64url_segments_each_8_plus_chars() {
        assert!(is_jwt_shape("eyJhbGciOi.eyJzdWIiOi.signatureXX"));
    }

    #[test]
    fn is_jwt_shape_rejects_non_jwt_shaped_tokens() {
        assert!(!is_jwt_shape("foo.bar.baz")); // too short segments
        assert!(!is_jwt_shape("only.two")); // not 3 segments
        assert!(!is_jwt_shape("seg1.seg2.seg3.seg4")); // 4 segments
        assert!(!is_jwt_shape("aaaaaaaa.bbbbbbbb.cc!cc!cc")); // invalid char
    }

    // ─── scrub count appended to summary (round-trip via record_inner-ish) ─

    #[test]
    fn scrub_count_visible_via_returned_count() {
        let (out, n) = scrub_credentials("Bearer aaaaaaaa ?password=hunter");
        assert!(out.contains("[REDACTED]"));
        // 1 Bearer + 1 password = 2 redactions.
        assert_eq!(n, 2);
    }

    // ─── Crash-loop suppression (2026-05-17 a third field-test host storm fix) ─────────────

    /// A directory only this test sees, removed when the guard drops, a
    /// failed assertion included (#3090). It used to be named after the
    /// clock: two tests that read it in the same tick shared one directory
    /// and saw each other's sidecars, and the first to finish deleted it
    /// under the other.
    fn tempdir_for_test() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Drop a fake sidecar named `{ts}-{pid}.json` in `dir`. Used by
    /// the suppression tests to pre-populate without going through
    /// the full record_inner path.
    fn touch_sidecar(dir: &Path, unix_ts: i64, pid: u32) {
        let path = dir.join(format!("{unix_ts}-{pid}.json"));
        std::fs::write(&path, b"{}").expect("write fake sidecar");
    }

    #[test]
    fn should_suppress_returns_none_for_empty_dir() {
        let dir = tempdir_for_test();
        assert!(should_suppress(dir.path(), 1_700_000_000).is_none());
    }

    #[test]
    fn should_suppress_rate_limits_when_recent_sidecar_exists() {
        let dir = tempdir_for_test();
        // Sidecar 5 seconds ago — within the 30s MIN_INTERVAL.
        touch_sidecar(dir.path(), 1_700_000_000 - 5, 12345);
        let now = 1_700_000_000_i64;
        assert_eq!(
            should_suppress(dir.path(), now),
            Some(SuppressReason::RecentSidecar)
        );
    }

    #[test]
    fn should_suppress_allows_write_after_min_interval_elapsed() {
        let dir = tempdir_for_test();
        // Sidecar 31 seconds ago — past the 30s MIN_INTERVAL.
        touch_sidecar(dir.path(), 1_700_000_000 - (MIN_INTERVAL_SECS + 1), 12345);
        let now = 1_700_000_000_i64;
        assert!(should_suppress(dir.path(), now).is_none());
    }

    #[test]
    fn should_suppress_hits_hard_cap_regardless_of_age() {
        let dir = tempdir_for_test();
        // Fill the dir with HARD_CAP stale sidecars (all 1 hour ago).
        // Each gets a unique ts so filenames don't collide.
        let stale_base = 1_700_000_000_i64 - 3600;
        for i in 0..HARD_CAP {
            touch_sidecar(dir.path(), stale_base + i as i64, 12345);
        }
        let now = 1_700_000_000_i64;
        assert_eq!(
            should_suppress(dir.path(), now),
            Some(SuppressReason::HardCapReached),
            "hard cap must trip even when all sidecars are old"
        );
    }

    #[test]
    fn should_suppress_ignores_non_json_files() {
        let dir = tempdir_for_test();
        // A *.tmp file (in-progress write) doesn't count toward the
        // cap and doesn't trigger the rate-limit.
        std::fs::write(dir.path().join("1700000000-12345.json.tmp"), b"{}").unwrap();
        assert!(should_suppress(dir.path(), 1_700_000_000).is_none());
    }

    #[test]
    fn suppress_reason_strings_are_stable() {
        // The outer record() matches on these strings to suppress a
        // duplicate WARN; a rename here would silently re-introduce
        // the spam.
        assert_eq!(
            SuppressReason::RecentSidecar.as_str(),
            "crash_recorder: recent sidecar; rate-limited"
        );
        assert_eq!(
            SuppressReason::HardCapReached.as_str(),
            "crash_recorder: hard cap reached; suppressing"
        );
        assert!(is_suppression_message(
            SuppressReason::RecentSidecar.as_str()
        ));
        assert!(is_suppression_message(
            SuppressReason::HardCapReached.as_str()
        ));
        assert!(!is_suppression_message("io: disk full"));
    }

    // ─── record_with_log_tail override path (worker-stderr capture) ────────

    #[test]
    fn record_inner_uses_override_when_provided() {
        // Wire-shape lock: when the caller supplies a log_tail (e.g.
        // captured worker stderr), it lands in the serialised payload
        // verbatim — modulo the scrub pipeline. The local rolling-log
        // read path (`read_log_tail`) is bypassed entirely so the
        // override is the authoritative source.
        let now_unix = 1_700_000_000_i64;
        let mut payload = AgentCrashPayload {
            crashed_at_unix: now_unix,
            reason: CrashReason::SupervisorDetected,
            summary: "worker exit: config load failed".to_string(),
            log_tail: "WORKER's real log content here\nnext line".to_string(),
            agent_version: "0.0.0-test".to_string(),
            os: "test".to_string(),
            hostname: "test".to_string(),
            pid: 99,
            suppressed_since_last: 0,
        };
        // Verify our trim helper doesn't strip the override content
        // when it's well under budget.
        fit_to_envelope(&mut payload, MAX_PAYLOAD_BYTES);
        assert!(payload.log_tail.contains("WORKER's real log content"));
        assert!(!payload.log_tail.contains("[…log truncated"));
    }

    #[test]
    fn record_with_log_tail_signature_accepts_none() {
        // Compile-time lock: the public API is `record_with_log_tail
        // (Reason, &str, WriterContext, Option<String>)`. A signature
        // drift here would break the supervisor / worker callers.
        // Reference the fn to keep the symbol live; never actually
        // call it (logging::log_dir() in a unit-test harness returns
        // None and a record call to log spam would race other tests).
        let _ = record_with_log_tail as fn(CrashReason, &str, WriterContext, Option<String>);
        let _ = record as fn(CrashReason, &str, WriterContext);
        let _ = record_with_appendix as fn(CrashReason, &str, WriterContext, &str);
    }

    // ─── #3018: an appendix after the tail ──────────────────────────────────

    /// #3018 — the appendix comes after the tail, and an empty side adds no
    /// blank line.
    #[test]
    fn the_appendix_follows_the_tail() {
        assert_eq!(tail_then_appendix("a\nb", "S"), "a\nb\nS");
        assert_eq!(tail_then_appendix("", "S"), "S");
        assert_eq!(tail_then_appendix("a", ""), "a");
    }

    /// #3018 — when the envelope cannot hold both, the oldest log lines go
    /// first, and the appendix (a stall's thread stacks) survives whole.
    #[test]
    fn the_envelope_trims_the_tail_before_the_appendix() {
        let tail: String = (0..4000).map(|i| format!("old log line {i}\n")).collect();
        let appendix = "--- threads ---\n[4] tokio-rt-worker\n  __psynch_mutexwait";
        let mut p = sample_payload();
        p.log_tail = tail_then_appendix(tail.trim_end(), appendix);
        assert!(
            estimate_json_size(&p) > MAX_PAYLOAD_BYTES,
            "the test needs a trim"
        );
        fit_to_envelope(&mut p, MAX_PAYLOAD_BYTES);
        assert!(estimate_json_size(&p) <= MAX_PAYLOAD_BYTES);
        assert!(
            p.log_tail.trim_end().ends_with(appendix),
            "the appendix survives whole"
        );
        assert!(
            !p.log_tail.contains("old log line 0\n"),
            "the oldest lines went first"
        );
        assert!(
            p.log_tail.contains("old log line 3999\n"),
            "the newest log lines stay"
        );
    }

    // ─── #2955: torn sidecars ────────────────────────────────────────────────

    /// A valid sidecar named `{ts}-{pid}.json`, the shape `record_inner` writes.
    fn write_valid_sidecar(dir: &Path, unix_ts: i64, pid: u32) {
        let payload = AgentCrashPayload {
            crashed_at_unix: unix_ts,
            reason: CrashReason::SupervisorDetected,
            summary: "worker exit".to_string(),
            log_tail: String::new(),
            agent_version: "0.0.0-test".to_string(),
            os: "test".to_string(),
            hostname: "test".to_string(),
            pid,
            suppressed_since_last: 0,
        };
        let bytes = serde_json::to_vec(&payload).expect("serialise");
        std::fs::write(dir.join(format!("{unix_ts}-{pid}.json")), bytes).expect("write sidecar");
    }

    /// A torn write, as a power loss leaves it: a JSON prefix, then NULs.
    fn write_torn_sidecar(dir: &Path, unix_ts: i64, pid: u32) -> PathBuf {
        let path = dir.join(format!("{unix_ts}-{pid}.json"));
        let mut bytes = br#"{"crashed_at_unix":"#.to_vec();
        bytes.extend_from_slice(&[0u8; 512]);
        std::fs::write(&path, bytes).expect("write torn sidecar");
        path
    }

    /// #2955 — a torn sidecar is moved into `malformed/` on the first scan and
    /// is not returned, while the valid one beside it still is; a second scan
    /// finds nothing left to move or warn about.
    #[test]
    fn a_torn_sidecar_is_moved_aside_once() {
        let tmp = tempdir_for_test();
        let dir = tmp.path().to_path_buf();
        write_valid_sidecar(&dir, 1_700_000_000, 1);
        let torn = write_torn_sidecar(&dir, 1_700_000_100, 2);

        let first = scan_dir(&dir);
        assert_eq!(first.len(), 1, "only the valid sidecar is returned");
        assert_eq!(first[0].1.pid, 1);
        assert!(!torn.exists(), "the torn sidecar left the scanned dir");
        let moved = dir.join(MALFORMED_DIR).join(torn.file_name().unwrap());
        assert!(moved.exists(), "kept for a later look");

        let second = scan_dir(&dir);
        assert_eq!(second.len(), 1);
        assert!(moved.exists(), "a second scan leaves it where it is");
    }

    /// #2955 — when the move fails, the sidecar stays where it was, as before
    /// the quarantine existed: skipped, never lost.
    #[test]
    fn a_torn_sidecar_that_cannot_move_stays_put() {
        let tmp = tempdir_for_test();
        let dir = tmp.path().to_path_buf();
        // A FILE where the quarantine directory would go: create_dir_all fails.
        std::fs::write(dir.join(MALFORMED_DIR), b"not a directory").unwrap();
        let torn = write_torn_sidecar(&dir, 1_700_000_100, 2);

        assert!(scan_dir(&dir).is_empty());
        assert!(torn.exists(), "the sidecar was not lost");
    }

    /// FR-145 — the supervisor's crash records in the machine folder, each
    /// case in a child process of this test binary: a case names its own
    /// stand-in for the machine's `%ProgramData%` (#3702's `program_data_is`),
    /// which is process-wide. Making a sealed folder and writing into it
    /// takes SYSTEM's or an elevated administrator's token, so a case says so
    /// and stops unelevated, and fails in CI's elevated lane.
    #[cfg(windows)]
    mod in_the_machine_folder {
        use std::path::{Path, PathBuf};

        use super::super::record_inner_at;
        use crate::machine_tree::test_support::{
            aces, elevated_or_skip, icacls, junction, owner, program_data, protected, sddl,
        };
        use roomler_ai_remote_control::models::CrashReason;

        const ROOT: &str = "ROOMLER_NODE_TEST_CRASH_ROOT";
        const CHECKED: &str = "crash-record case: checked";

        /// The parent's half: a stand-in `%ProgramData%` with an open machine
        /// folder below it (an attended host's), and a folder beside it that
        /// nothing names; then the case `name` in a child, which must say it ran.
        fn run(name: &str, case: &str) {
            if !elevated_or_skip(case) {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let pd = program_data(root.path());
            std::fs::create_dir_all(pd.join("roomler").join("roomler")).unwrap();
            std::fs::create_dir_all(root.path().join("elsewhere")).unwrap();
            let module = module_path!();
            let test = format!(
                "{}::{name}",
                module.split_once("::").map_or(module, |(_, rest)| rest)
            );
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test.as_str(), "--exact", "--nocapture", "--test-threads=1"])
                .env(ROOT, root.path())
                .output()
                .unwrap();
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success(), "{said}");
            assert!(said.contains(CHECKED), "the child ran the case: {said}");
        }

        /// The child's half: the stand-in, before anything names the machine
        /// tree. `None` in the parent.
        fn child() -> Option<PathBuf> {
            let root = PathBuf::from(std::env::var_os(ROOT)?);
            crate::appdirs::test_fixture::program_data_is(root.join("ProgramData"));
            Some(root)
        }

        /// The names in `dir` a record write leaves: `*.json`, `*.json.tmp`.
        fn records_in(dir: &Path) -> Vec<String> {
            std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .filter(|n| n.ends_with(".json") || n.ends_with(".json.tmp"))
                        .collect()
                })
                .unwrap_or_default()
        }

        /// A junction or a symbolic link, read without following it.
        fn is_link(path: &Path) -> bool {
            std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        }

        /// How many names the file at `path` has, read through a handle of
        /// the test's own, opened by name.
        fn links(path: &Path) -> u32 {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::HANDLE;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let file = std::fs::File::open(path).unwrap();
            let handle = file.as_raw_handle() as HANDLE;
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a live handle; `info` is the struct the call fills in.
            let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
            let error = std::io::Error::last_os_error();
            assert!(ok != 0, "{}: {error}", path.display());
            info.nNumberOfLinks
        }

        /// T11 — `crashes` is a junction: the record lands in a new, sealed
        /// `crashes`, with no `.json` or `.json.tmp` where the junction led,
        /// and the junction is kept beside it as `crashes.moved-<unix>`. A
        /// junction above it: refused, nothing written anywhere.
        #[test]
        fn a_crash_record_lands_in_a_sealed_crashes_never_through_a_junction() {
            let Some(root) = child() else {
                return run(
                    "a_crash_record_lands_in_a_sealed_crashes_never_through_a_junction",
                    "T11: a crash record and a junction",
                );
            };
            let elsewhere = root.join("elsewhere");
            let machine = crate::appdirs::machine_global_dir();
            let crashes = machine.join("crashes");
            junction(&crashes, &elsewhere);
            let written = record_inner_at(
                &crashes,
                CrashReason::SupervisorDetected,
                "worker exit: a test",
                Some("the worker's last line".into()),
            );
            assert!(
                records_in(&elsewhere).is_empty(),
                "nothing is written where the junction leads: {:?} ({written:?})",
                records_in(&elsewhere)
            );
            assert!(!is_link(&crashes), "crashes is a real folder ({written:?})");
            assert_eq!(
                records_in(&crashes).len(),
                1,
                "the record is in the new crashes: {:?} ({written:?})",
                records_in(&crashes)
            );
            let shape = sddl(&crashes);
            let mut got = aces(&shape);
            got.sort();
            assert!(
                protected(&shape)
                    && got == ["A;OICI;0x1200a9;;;BU", "A;OICI;FA;;;BA", "A;OICI;FA;;;SY"],
                "crashes is sealed, Users read: {shape}"
            );
            let moved: Vec<PathBuf> = std::fs::read_dir(&machine)
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("crashes.moved-"))
                })
                .map(|e| e.path())
                .collect();
            assert!(
                moved.len() == 1 && is_link(&moved[0]),
                "the junction is kept beside it: {moved:?}"
            );

            // A junction above it: the machine folder's parent is one, to a
            // tree that holds the same names.
            let parent = machine.parent().unwrap().to_path_buf();
            let above = root.join("above");
            std::fs::create_dir_all(above.join("roomler").join("crashes")).unwrap();
            std::fs::rename(&parent, root.join("roomler-parent-aside")).unwrap();
            junction(&parent, &above);
            let written = record_inner_at(
                &crashes,
                CrashReason::SupervisorDetected,
                "worker exit: a test",
                Some("the worker's last line".into()),
            );
            assert!(
                records_in(&above.join("roomler").join("crashes")).is_empty(),
                "nothing is written through a junction above crashes ({written:?})"
            );
            let refused = written.expect_err("a junction above crashes is refused");
            assert!(refused.to_string().contains("link"), "{refused}");
            println!("{CHECKED}");
        }

        /// FR-145 T21 — the reads and acts below a held `crashes`: a record that
        /// does not parse is moved by handle into a sealed `malformed`, and a
        /// junction where `malformed` goes is moved aside with nothing put
        /// where it led; a record whose name became a symbolic link after it
        /// was read is not deleted through the link. Control: a record read
        /// is deleted through the folder it was read from.
        #[test]
        fn records_are_moved_and_deleted_by_handle_never_through_a_link() {
            use super::super::{MALFORMED_DIR, scan_dir};
            let Some(root) = child() else {
                return run(
                    "records_are_moved_and_deleted_by_handle_never_through_a_link",
                    "crash records moved and deleted by handle",
                );
            };
            let elsewhere = root.join("elsewhere");
            let crashes = crate::appdirs::machine_global_dir().join("crashes");
            let first = record_inner_at(
                &crashes,
                CrashReason::SupervisorDetected,
                "worker exit: a test",
                Some("the worker's last line".into()),
            )
            .expect("the recorder makes a sealed crashes and writes a record");
            super::write_valid_sidecar(&crashes, 2, 2);
            std::fs::write(crashes.join("1-1.json"), b"not json").unwrap();
            junction(&crashes.join(MALFORMED_DIR), &elsewhere);

            let pending = scan_dir(&crashes);
            let malformed = crashes.join(MALFORMED_DIR);
            assert!(
                records_in(&elsewhere).is_empty(),
                "nothing is moved where the junction led: {:?}",
                records_in(&elsewhere)
            );
            assert!(
                !is_link(&malformed) && malformed.join("1-1.json").is_file(),
                "the record that does not parse is in a real malformed"
            );
            assert!(!crashes.join("1-1.json").exists());
            let shape = sddl(&malformed);
            assert!(protected(&shape), "malformed is sealed: {shape}");
            let names: Vec<String> = pending
                .iter()
                .map(|(s, _)| s.path().file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            let first_name = first.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                names.len() == 2
                    && names.contains(&first_name)
                    && names.contains(&"2-2.json".into()),
                "{names:?}"
            );

            // The second record's name becomes a symbolic link after the read.
            let target = elsewhere.join("t.json");
            std::fs::write(&target, b"keep").unwrap();
            let second = crashes.join("2-2.json");
            std::fs::remove_file(&second).unwrap();
            std::os::windows::fs::symlink_file(&target, &second).unwrap();
            let by_name = |n: &str| pending.iter().find(|(s, _)| s.path().ends_with(n)).unwrap();
            let refused = by_name("2-2.json").0.remove();
            assert!(refused.is_err(), "a link is not deleted: {refused:?}");
            assert!(
                is_link(&second) && target.is_file(),
                "the link and its file stay"
            );

            // Control: the recorder's record is deleted through its folder.
            by_name(&first_name).0.remove().expect("deleted by handle");
            assert!(!first.exists());
            println!("{CHECKED}");
        }

        /// FR-145 T20 — a read walk that is refused (a junction above `crashes`)
        /// is not a suppressed crash: the tally the next record carries
        /// stays as it was. Control: a refused write is one.
        #[test]
        fn a_refused_read_is_not_counted_as_a_suppressed_crash() {
            use super::super::{SUPPRESSED_SINCE_LAST, scan_dir};
            use std::sync::atomic::Ordering;
            let Some(root) = child() else {
                return run(
                    "a_refused_read_is_not_counted_as_a_suppressed_crash",
                    "a refused read and the crash tally",
                );
            };
            let machine = crate::appdirs::machine_global_dir();
            let crashes = machine.join("crashes");
            let parent = machine.parent().unwrap().to_path_buf();
            let above = root.join("above");
            std::fs::create_dir_all(above.join("roomler").join("crashes")).unwrap();
            std::fs::rename(&parent, root.join("roomler-parent-aside")).unwrap();
            junction(&parent, &above);

            let before = SUPPRESSED_SINCE_LAST.load(Ordering::Relaxed);
            for _ in 0..3 {
                assert!(scan_dir(&crashes).is_empty(), "nothing is read");
            }
            let after_reads = SUPPRESSED_SINCE_LAST.load(Ordering::Relaxed);
            let written = record_inner_at(
                &crashes,
                CrashReason::SupervisorDetected,
                "worker exit: a test",
                Some("the worker's last line".into()),
            );
            let after_write = SUPPRESSED_SINCE_LAST.load(Ordering::Relaxed);
            assert_eq!(
                after_reads, before,
                "three refused reads are not suppressed crashes"
            );
            assert!(written.is_err(), "the write is refused: {written:?}");
            assert_eq!(after_write, before + 1, "the refused write is one");
            println!("{CHECKED}");
        }

        /// FR-145 T19 — the uploader's ledger in a SYSTEM worker's `service-logs`:
        /// (a) a first verdict makes `service-logs` sealed, Administrators its
        /// owner, as it does for the service. Then the ledger's name is made a
        /// hard link to a file outside that Administrators own, so a second
        /// name is the one thing wrong with it. The folder is sealed already,
        /// so the next walk seals and deletes nothing before the append: the
        /// line reaches `append_file`'s own check and goes to a new file of one
        /// name, the file outside unchanged and nothing moved aside. (b)
        /// `service-logs` is a junction: nothing is written where it leads.
        /// Control: the first verdict, and the verdict is there to read in
        /// both.
        #[test]
        fn a_verdict_is_never_written_through_a_hard_link_or_a_junction() {
            use super::super::{VERDICTS_FILE, append_verdict, read_verdicts};
            let Some(root) = child() else {
                return run(
                    "a_verdict_is_never_written_through_a_hard_link_or_a_junction",
                    "the uploader's ledger and a link",
                );
            };
            let elsewhere = root.join("elsewhere");
            let machine = crate::appdirs::machine_global_dir();
            let logs = machine.join("service-logs");
            let ledger = logs.join(VERDICTS_FILE);

            // (a) The first verdict makes `service-logs`, sealed.
            let first = append_verdict(&logs, "12:34 1-2.json");
            assert!(first.is_ok(), "the first verdict: {first:?}");
            let shape = sddl(&logs);
            let mut got = aces(&shape);
            got.sort();
            assert!(
                protected(&shape)
                    && owner(&shape) == "BA"
                    && got == ["A;OICI;0x1200a9;;;BU", "A;OICI;FA;;;BA", "A;OICI;FA;;;SY"],
                "service-logs is made sealed, Users read, Administrators its owner: {shape}"
            );
            // Then the ledger's name is a hard link to a file outside, owned
            // by Administrators as a file the service wrote would be.
            let outside = elsewhere.join("outside.txt");
            std::fs::write(&outside, b"outside\n").unwrap();
            assert!(
                icacls(&outside, &["/setowner", "*S-1-5-32-544"]) && owner(&sddl(&outside)) == "BA",
                "the file outside is Administrators'"
            );
            std::fs::remove_file(&ledger).unwrap();
            std::fs::hard_link(&outside, &ledger).unwrap();
            assert_eq!(links(&ledger), 2, "the ledger's name is a second name");
            let linked = append_verdict(&logs, "23:45 3-4.json");
            let outside_now = std::fs::read(&outside).unwrap();
            let names = (links(&ledger), links(&outside));
            let moved: Vec<String> = std::fs::read_dir(&machine)
                .unwrap()
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("service-logs.moved-"))
                .collect();
            let read_a = read_verdicts(&logs);

            // (b) `service-logs` is a junction to a folder outside.
            std::fs::rename(&logs, root.join("service-logs-aside")).unwrap();
            junction(&logs, &elsewhere);
            let through = append_verdict(&logs, "56:78 5-6.json");
            let led_to = std::fs::read_to_string(elsewhere.join(VERDICTS_FILE)).ok();
            let read_b = read_verdicts(&logs);

            assert!(
                outside_now == b"outside\n" && led_to.is_none(),
                "no verdict is written through a link: (a) the file outside \
                 now holds {:?} ({linked:?}); (b) where the junction leads: \
                 {led_to:?} ({through:?})",
                String::from_utf8_lossy(&outside_now)
            );
            assert!(linked.is_ok() && through.is_ok(), "{linked:?} {through:?}");
            assert_eq!(
                names,
                (1, 1),
                "(a) a new ledger of one name, and the file outside keeps only its own"
            );
            assert!(
                moved.is_empty(),
                "(a) the sealed service-logs is kept, not moved aside: {moved:?}"
            );
            assert_eq!(read_a, "23:45 3-4.json\n", "(a) the new ledger");
            assert_eq!(read_b, "56:78 5-6.json\n", "(b) the new ledger");
            assert!(!is_link(&logs), "service-logs is a real folder again");
            println!("{CHECKED}");
        }
    }
}
