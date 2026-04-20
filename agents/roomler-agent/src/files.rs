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
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// 2 GiB. SCTP DCs in webrtc-rs can carry larger payloads in theory
/// but per-transfer >2 GB is outside the "drop a file" use case and
/// would need chunk-resume which this MVP doesn't implement.
const MAX_TRANSFER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Incoming control messages over the `files` DC (string payloads).
/// Binary payloads are handled separately — they're not JSON.
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
}

/// Outgoing control messages sent back to the browser. Flat `t`
/// discriminant mirrors the clipboard DC's pattern for consistency.
#[derive(Debug, Serialize)]
#[serde(tag = "t")]
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
}

/// Per-DC transfer state. A single transfer is "active" at any time —
/// files:begin starts one; files:end or the DC closing finishes it.
pub(crate) struct TransferState {
    pub id: String,
    pub path: PathBuf,
    pub expected: u64,
    pub received: u64,
    pub file: File,
    /// Last byte count reported via files:progress. Progress is sent
    /// every ~256 KiB to keep the browser UI lively without flooding.
    pub last_progress: u64,
}

/// Handle on the file-transfer subsystem for one data channel.
/// Thread-safe — cheap Arc clones are used inside the on_message and
/// on_close callbacks on the DC.
#[derive(Clone)]
pub struct FilesHandler {
    state: Arc<Mutex<Option<TransferState>>>,
}

impl Default for FilesHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesHandler {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
        }
    }

    /// Start a new transfer. Returns the absolute destination path so
    /// the caller can reply `files:accepted { id, path }`.
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

        let mut guard = self.state.lock().await;
        if guard.is_some() {
            // A previous transfer was in-flight and never got files:end
            // (browser closed or error). Drop it silently — the handler
            // doesn't persist partial files across DC restarts.
        }
        *guard = Some(TransferState {
            id,
            path: path.clone(),
            expected,
            received: 0,
            file,
            last_progress: 0,
        });
        Ok(path)
    }

    /// Append binary data to the active transfer. Returns the total
    /// byte count after this append, and whether this append crossed a
    /// progress-report threshold.
    pub async fn chunk(&self, data: &[u8]) -> Result<Option<ChunkProgress>> {
        let mut guard = self.state.lock().await;
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

    /// Finalize the active transfer. Flushes the writer and clears
    /// the state. Returns the final path + total bytes on success.
    pub async fn end(&self, id: &str) -> Result<(PathBuf, u64)> {
        let mut guard = self.state.lock().await;
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

    /// Drop any in-flight transfer (DC closed mid-upload). The partial
    /// file is left on disk; a future version could delete it.
    pub async fn abort(&self) {
        let mut guard = self.state.lock().await;
        *guard = None;
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
    let base = name
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(name);
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
    if let Some(idx) = name.rfind('.') {
        if idx > 0 && idx < name.len() - 1 {
            return (&name[..idx], &name[idx + 1..]);
        }
    }
    (name, "")
}

fn download_dir() -> Result<PathBuf> {
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(dl) = dirs.download_dir() {
            return Ok(dl.to_path_buf());
        }
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
