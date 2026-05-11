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
pub static PARTIAL_REGISTRY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Test-only escape hatch — clears the partial registry between
/// `cargo test` cases that touch the same global. Production code
/// never calls this.
#[cfg(test)]
pub fn reset_partial_registry() {
    if let Ok(mut g) = PARTIAL_REGISTRY.lock() {
        g.clear();
    }
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
    /// Browser → Agent: list a directory. `path` is empty / `~` to
    /// list logical drives (Win) or `/` (Unix). `req_id` echoes back
    /// in the `dir-list` reply so the browser can match concurrent
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
        // drop the operator's data.
        let target_dir = match dest_path.filter(|p| !p.is_empty()) {
            Some(dp) => match resolve_dest_path(dp).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        dest_path = dp,
                        %e,
                        "files: dest_path rejected; falling back to Downloads"
                    );
                    download_dir().context("resolving Downloads folder")?
                }
            },
            None => download_dir().context("resolving Downloads folder")?,
        };
        // Folder upload: resolve `rel_path` to a path under the
        // target dir, sanitising each component. Falls back to flat
        // upload when rel_path is empty / missing / refused by
        // sanitisation.
        let path = match rel_path.filter(|p| !p.is_empty()) {
            Some(rel) => resolve_folder_upload_path(&target_dir, rel).unwrap_or_else(|| {
                tracing::warn!(
                    rel_path = rel,
                    "files: rel_path rejected; falling back to flat upload"
                );
                unique_path(&target_dir, &sanitize_filename(&name))
            }),
            None => unique_path(&target_dir, &sanitize_filename(&name)),
        };
        // Create parent dirs (Downloads itself, plus any rel_path
        // intermediaries). create_dir_all is idempotent so
        // pre-existing dirs are fine. For flat uploads `parent` IS
        // Downloads; for folder uploads it's `Downloads/<root>/sub/`.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
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
async fn resolve_dest_path(input: &str) -> Result<PathBuf> {
    let canonical = validate_outgoing_path(input).context("validating dest_path")?;
    let meta = tokio::fs::metadata(&canonical)
        .await
        .with_context(|| format!("stat {}", canonical.display()))?;
    if !meta.is_dir() {
        return Err(anyhow!(
            "dest_path is not a directory: {}",
            canonical.display()
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
    let mut path = dir.to_path_buf();
    for c in &components {
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

    #[tokio::test]
    async fn begin_with_dest_path_lands_in_dest() {
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
        // Point HOME / USERPROFILE at base so the Downloads fallback
        // doesn't pollute the dev's actual Downloads dir if dest_path
        // resolution somehow fell through.
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", &base);
            std::env::set_var("USERPROFILE", &base);
        }

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
    fn partial_registry_starts_empty_and_resets() {
        // Tests share PARTIAL_REGISTRY (process-global LazyLock). The
        // reset hook clears it; verify the test infrastructure works.
        reset_partial_registry();
        let g = PARTIAL_REGISTRY.lock().unwrap();
        assert!(g.is_empty(), "registry not empty after reset");
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
