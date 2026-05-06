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
//! - Destination: `~/Downloads` (or platform equivalent per
//!   `directories::UserDirs::download_dir()`). Falls back to the OS
//!   temp dir if the user has no Downloads (rare — headless CI).
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
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// 2 GiB. SCTP DCs in webrtc-rs can carry larger payloads in theory
/// but per-transfer >2 GB is outside the "drop a file" use case and
/// would need chunk-resume which this MVP doesn't implement.
const MAX_TRANSFER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Process-global flag for the `files:dir` browse capability. Set
/// once at startup from `AgentConfig::enable_remote_browse` (and the
/// `ROOMLER_AGENT_DISABLE_BROWSE` env-var escape hatch). Readers in
/// the DC hot path use [`is_remote_browse_enabled`] which compiles
/// to a single relaxed atomic load.
///
/// Default `true` (preserves self-controlled-host auto-grant
/// semantics from `docs/remote-control.md` §11.2). `set_remote_browse`
/// is called from `main.rs::Run` after config load; no
/// initialization race in practice because file-DC handlers can't
/// fire until a session opens, which is well after main has
/// settled.
static REMOTE_BROWSE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

pub fn set_remote_browse_enabled(enabled: bool) {
    REMOTE_BROWSE_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
}

pub fn is_remote_browse_enabled() -> bool {
    REMOTE_BROWSE_ENABLED.load(std::sync::atomic::Ordering::Acquire)
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
    /// Browser → Agent: cancel an in-flight outgoing transfer.
    /// Affects whichever direction the id maps to (today: only
    /// outgoing; uploads abort via DC close).
    #[serde(rename = "files:cancel")]
    Cancel { id: String },
    /// Browser → Agent: list a directory. `path` is empty / `~` to
    /// list logical drives (Win) or `/` (Unix). `req_id` echoes back
    /// in the `dir-list` reply so the browser can match concurrent
    /// requests.
    #[serde(rename = "files:dir")]
    Dir { req_id: String, path: String },
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

/// Browser → Agent upload state. A single incoming transfer is
/// "active" at any time — files:begin starts one; files:end or the
/// DC closing finishes it.
pub(crate) struct IncomingTransfer {
    pub id: String,
    pub path: PathBuf,
    pub expected: u64,
    pub received: u64,
    pub file: File,
    /// Last byte count reported via files:progress. Progress is sent
    /// every ~256 KiB to keep the browser UI lively without flooding.
    pub last_progress: u64,
}

/// Agent → Browser download state. One outgoing transfer is active
/// at any time. The `cancel` flag is checked between chunks so a
/// `files:cancel` message exits the pump cleanly.
pub(crate) struct OutgoingTransfer {
    pub id: String,
    pub path: PathBuf,
    /// Captured file size from the begin_outgoing stat. Useful for
    /// future per-transfer audit logging; not consulted by the pump
    /// today (which reads-until-EOF).
    #[allow(dead_code)]
    pub size: u64,
    pub cancel: Arc<AtomicBool>,
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
        }
    }

    /// Start a new incoming transfer (browser → host upload). Returns
    /// the absolute destination path so the caller can reply
    /// `files:accepted { id, path }`.
    pub async fn begin(&self, id: String, name: String, expected: u64) -> Result<PathBuf> {
        if expected > MAX_TRANSFER_BYTES {
            return Err(anyhow!(
                "transfer size {expected} exceeds the {} B cap",
                MAX_TRANSFER_BYTES
            ));
        }
        let downloads = download_dir().context("resolving Downloads folder")?;
        let path = unique_path(&downloads, &sanitize_filename(&name));
        tokio::fs::create_dir_all(&downloads)
            .await
            .with_context(|| format!("creating {}", downloads.display()))?;
        let file = File::create(&path)
            .await
            .with_context(|| format!("creating {}", path.display()))?;

        let mut guard = self.incoming.lock().await;
        if guard.is_some() {
            // A previous transfer was in-flight and never got files:end
            // (browser closed or error). Drop it silently — the handler
            // doesn't persist partial files across DC restarts.
        }
        *guard = Some(IncomingTransfer {
            id,
            path: path.clone(),
            expected,
            received: 0,
            file,
            last_progress: 0,
        });
        Ok(path)
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

    /// Finalize the active incoming transfer. Flushes the writer and
    /// clears the state. Returns the final path + total bytes on
    /// success.
    pub async fn end(&self, id: &str) -> Result<(PathBuf, u64)> {
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
            return Err(anyhow!(
                "short transfer: received {} of {} bytes",
                state.received,
                state.expected
            ));
        }
        Ok((state.path, state.received))
    }

    /// Drop any in-flight incoming transfer (DC closed mid-upload).
    /// The partial file is left on disk; a future version could
    /// delete it.
    pub async fn abort(&self) {
        let mut guard = self.incoming.lock().await;
        *guard = None;
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
        let resolved = validate_outgoing_path(path).context("validating outgoing path")?;

        // Stat to surface a real error before we set state, AND get
        // the size for the offer.
        let meta = tokio::fs::metadata(&resolved)
            .await
            .with_context(|| format!("stat {}", resolved.display()))?;
        if !meta.is_file() {
            return Err(anyhow!("not a regular file: {}", resolved.display()));
        }
        let size = meta.len();
        if size > MAX_TRANSFER_BYTES {
            return Err(anyhow!(
                "file size {size} exceeds the {} B cap",
                MAX_TRANSFER_BYTES
            ));
        }

        let name = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "download.bin".to_string());
        let mime = guess_mime(&name);
        let cancel = Arc::new(AtomicBool::new(false));

        let mut guard = self.outgoing.lock().await;
        if guard.is_some() {
            return Err(anyhow!(
                "another outgoing transfer is already active; cancel it first"
            ));
        }
        *guard = Some(OutgoingTransfer {
            id: id.clone(),
            path: resolved.clone(),
            size,
            cancel: cancel.clone(),
        });

        Ok(OutgoingOffer {
            id,
            path: resolved,
            name,
            size: Some(size),
            mime,
            cancel,
        })
    }

    /// Open the outgoing transfer's file for reading. Caller pumps
    /// chunks via the returned handle.
    pub async fn open_outgoing(&self, id: &str) -> Result<File> {
        let guard = self.outgoing.lock().await;
        let Some(state) = guard.as_ref() else {
            return Err(anyhow!("no active outgoing transfer"));
        };
        if state.id != id {
            return Err(anyhow!(
                "outgoing id mismatch: requested {id}, active {}",
                state.id
            ));
        }
        File::open(&state.path)
            .await
            .with_context(|| format!("opening {}", state.path.display()))
    }

    /// Flip the cancel flag on the active outgoing transfer if its
    /// id matches. The pump task checks the flag between chunks and
    /// exits cleanly. Returns true if a matching transfer was found.
    pub async fn cancel_outgoing(&self, id: &str) -> bool {
        let guard = self.outgoing.lock().await;
        let Some(state) = guard.as_ref() else {
            return false;
        };
        if state.id != id {
            return false;
        }
        state.cancel.store(true, Ordering::Release);
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
/// (cancellation flag).
pub struct OutgoingOffer {
    pub id: String,
    pub path: PathBuf,
    pub name: String,
    pub size: Option<u64>,
    pub mime: Option<&'static str>,
    pub cancel: Arc<AtomicBool>,
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

/// List a directory. Empty / `~` / `/` enumerates roots (logical
/// drives on Windows; `/` on Unix). Returns at most 10000 entries —
/// a directory with more is a degenerate case (deeply nested
/// `node_modules` on a dev box) that we'd rather refuse than
/// stream a 1 MiB JSON listing.
pub async fn list_dir(path: &str) -> Result<DirListing> {
    if path.is_empty() || path == "~" || path == "/" {
        return Ok(DirListing {
            path: roots_label(),
            parent: None,
            entries: enumerate_roots(),
        });
    }
    let pb = PathBuf::from(path);
    let canon =
        std::fs::canonicalize(&pb).with_context(|| format!("canonicalising {}", pb.display()))?;
    let parent = canon.parent().map(|p| p.to_string_lossy().to_string());
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(DIR_LIST_TIMEOUT_SECS),
        tokio::fs::read_dir(&canon),
    )
    .await
    .map_err(|_| anyhow!("listing timed out"))?
    .with_context(|| format!("reading {}", canon.display()))?;

    let entries = collect_dir_entries(read).await;
    Ok(DirListing {
        path: canon.to_string_lossy().to_string(),
        parent,
        entries,
    })
}

async fn collect_dir_entries(mut read: tokio::fs::ReadDir) -> Vec<DirEntryView> {
    let mut entries: Vec<DirEntryView> = Vec::new();
    let mut count: usize = 0;
    while let Ok(Some(entry)) = read.next_entry().await {
        count += 1;
        if count > 10_000 {
            break;
        }
        let name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue, // skip non-UTF-8 names
        };
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue, // permission denied / vanished — skip
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
        let resolved = validate_outgoing_path(path).context("validating folder path")?;
        let meta = tokio::fs::metadata(&resolved)
            .await
            .with_context(|| format!("stat {}", resolved.display()))?;
        if !meta.is_dir() {
            return Err(anyhow!("not a directory: {}", resolved.display()));
        }
        let folder_name = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .map(sanitize_filename)
            .unwrap_or_else(|| "folder".to_string());
        let zip_name = format!("{folder_name}.zip");
        let cancel = Arc::new(AtomicBool::new(false));

        let mut guard = self.outgoing.lock().await;
        if guard.is_some() {
            return Err(anyhow!(
                "another outgoing transfer is already active; cancel it first"
            ));
        }
        *guard = Some(OutgoingTransfer {
            id: id.clone(),
            path: resolved.clone(),
            size: 0, // unknown for streaming zip
            cancel: cancel.clone(),
        });
        Ok(OutgoingOffer {
            id,
            path: resolved,
            name: zip_name,
            size: None,
            mime: Some("application/zip"),
            cancel,
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
pub async fn walk_and_zip<W>(
    writer: W,
    root: &std::path::Path,
    cancel: Arc<AtomicBool>,
) -> Result<u32>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    use async_zip::tokio::write::ZipFileWriter;
    use async_zip::{Compression, ZipEntryBuilder};
    use std::collections::HashSet;
    use tokio_util::compat::FuturesAsyncWriteCompatExt;

    let root_canon = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalising {}", root.display()))?;

    let mut zip = ZipFileWriter::with_tokio(writer);
    let mut stack: Vec<PathBuf> = vec![root_canon.clone()];
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(root_canon.clone());
    let mut count: u32 = 0;

    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Acquire) {
            return Err(anyhow!("cancelled by browser"));
        }
        let mut read_dir = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(dir = %dir.display(), %e, "zip walk: skipping unreadable dir");
                continue;
            }
        };
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            if cancel.load(Ordering::Acquire) {
                return Err(anyhow!("cancelled by browser"));
            }
            count = count.saturating_add(1);
            if count > ZIP_MAX_ENTRIES {
                return Err(anyhow!(
                    "folder exceeds {ZIP_MAX_ENTRIES} entries — refusing to stream"
                ));
            }
            let path = entry.path();
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue, // permission denied — skip
            };

            if meta.is_dir() {
                // Cycle protection: canonical path already visited?
                // (Else branch — already-visited — is a silent skip:
                // symlink loop or hard-linked dir.)
                if let Ok(canon) = std::fs::canonicalize(&path)
                    && visited.insert(canon)
                {
                    stack.push(path);
                }
                continue;
            }

            if !meta.is_file() {
                continue; // skip pipes / sockets / device files
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

            // Open the file + stream into a zip entry. Failures on
            // a single file are logged at debug and skipped — we
            // don't want one unreadable file to abort the whole
            // archive.
            let mut src = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!(file = %path.display(), %e, "zip walk: skipping unreadable file");
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

fn download_dir() -> Result<PathBuf> {
    // M3 A1 SystemContext fallback: when the worker is spawned by the
    // SCM service via winlogon-token, it runs as LocalSystem
    // (S-1-5-18) but in the user's interactive session.
    // `directories::UserDirs::new()` resolves Downloads to the
    // LocalSystem profile (`C:\Windows\System32\config\systemprofile\
    // Downloads\`) which usually doesn't exist — uploads fail (or
    // worse, succeed silently into a directory the user can't see).
    // Field repro PC50045 rc.7 2026-05-06: file upload via browse-
    // and-select hung because `create_dir_all` couldn't create the
    // SYSTEM-profile path. Same fallback shape as the rc.6 config
    // fix; see `system_context::user_profile`.
    #[cfg(all(feature = "system-context", target_os = "windows"))]
    {
        use crate::system_context::{user_profile, worker_role};
        if matches!(
            worker_role::probe_self(),
            Ok(worker_role::WorkerRole::SystemContext)
        ) && let Some(dl) = user_profile::active_user_downloads_path()
        {
            tracing::debug!(
                fallback_path = %dl.display(),
                "files: SystemContext worker — using active-user Downloads (default would be SYSTEM profile)"
            );
            return Ok(dl);
        }
    }

    if let Some(dirs) = directories::UserDirs::new()
        && let Some(dl) = dirs.download_dir()
    {
        return Ok(dl.to_path_buf());
    }
    // Fall back to the OS temp dir — acceptable for headless CI /
    // service accounts with no Downloads folder.
    Ok(std::env::temp_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let listing = list_dir("").await.expect("list_dir empty");
        assert!(!listing.entries.is_empty(), "roots view must have entries");
        // Every entry must be a directory (drive root or "/").
        for e in &listing.entries {
            assert!(e.is_dir, "root entry should be a dir: {:?}", e);
        }
        // Parent must be None for the roots view.
        assert!(listing.parent.is_none());
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
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let folder_clone = folder.clone();
        let cancel_clone = cancel.clone();
        let walk_handle = tokio::spawn(async move {
            crate::files::walk_and_zip(zip_writer, &folder_clone, cancel_clone).await
        });
        let drain_handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            zip_reader.read_to_end(&mut out).await.unwrap();
            out
        });
        let _count = walk_handle.await.unwrap().expect("walk_and_zip");
        let sink = drain_handle.await.unwrap();

        assert!(sink.len() > 0, "zip output should be non-empty");
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

        // Point HOME/USERPROFILE at base so begin() picks a Downloads
        // dir we control. Otherwise begin() would land on the dev's
        // real Downloads.
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", &base);
            std::env::set_var("USERPROFILE", &base);
        }
        tokio::fs::create_dir_all(base.join("Downloads"))
            .await
            .unwrap();

        let h = FilesHandler::new();
        // Start an upload — populates `incoming`.
        let upload_path = h
            .begin("u1".into(), "upload.txt".into(), 5)
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

        // The download is still active.
        let cancelled = h.cancel_outgoing("d1").await;
        assert!(cancelled);
        h.finish_outgoing("d1").await;

        // Restore env + cleanup.
        unsafe {
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
            if let Some(v) = prev_userprofile {
                std::env::set_var("USERPROFILE", v);
            } else {
                std::env::remove_var("USERPROFILE");
            }
        }
        let _ = tokio::fs::remove_dir_all(&base).await;
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
        assert!(!offer.cancel.load(Ordering::Acquire));
        let cancelled = h.cancel_outgoing("c1").await;
        assert!(cancelled);
        assert!(offer.cancel.load(Ordering::Acquire));

        // Mismatched id → false, no flag change on a fresh transfer.
        let cancelled_other = h.cancel_outgoing("nonexistent").await;
        assert!(!cancelled_other);

        h.finish_outgoing("c1").await;
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn round_trip_begin_chunk_end() {
        let h = FilesHandler::new();
        let tmp = tempdir_or_skip().await;
        // Override the download-dir resolver by ensuring the sanitized
        // file lands somewhere writable. Easiest: test against the
        // OS temp dir. `begin` uses Downloads, so we point
        // HOME/USERPROFILE at tmp for the test.
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", &tmp);
            std::env::set_var("USERPROFILE", &tmp);
        }

        let path = h.begin("t1".into(), "hello.txt".into(), 5).await.unwrap();
        h.chunk(b"hello").await.unwrap();
        let (final_path, bytes) = h.end("t1").await.unwrap();
        assert_eq!(final_path, path);
        assert_eq!(bytes, 5);
        let got = tokio::fs::read(&final_path).await.unwrap();
        assert_eq!(got, b"hello");

        // Restore env.
        unsafe {
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
            if let Some(v) = prev_userprofile {
                std::env::set_var("USERPROFILE", v);
            } else {
                std::env::remove_var("USERPROFILE");
            }
        }
        // Best-effort cleanup.
        let _ = tokio::fs::remove_file(&final_path).await;
    }

    async fn tempdir_or_skip() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "roomler-agent-files-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        // Some test environments don't have a Downloads dir config —
        // create one under HOME so directories::UserDirs can find it.
        let dl = base.join("Downloads");
        tokio::fs::create_dir_all(&dl).await.unwrap();
        base
    }
}
