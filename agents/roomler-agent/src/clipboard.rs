//! Clipboard data-channel handler.
//!
//! Round-trip text clipboard between the browser controller and the
//! agent host over the WebRTC `clipboard` data channel (reliable +
//! ordered). Today text-only — images / HTML / files are out of scope
//! for the first pass; the file-transfer DC has its own MEDIUM Known
//! Issue that's still open.
//!
//! Wire protocol (JSON on the `clipboard` DC):
//!
//! ```text
//! // Browser -> Agent (single-envelope — used for texts ≤12 KB UTF-8)
//! { "t": "clipboard:write", "text": "hello" }
//! { "t": "clipboard:read" }
//!
//! // Browser -> Agent (chunked — rc.44+ — used for texts > 12 KB UTF-8)
//! { "t": "clipboard:write-chunk", "id": "abc123", "seq": 0, "text": "...", "last": false }
//! { "t": "clipboard:write-chunk", "id": "abc123", "seq": 1, "text": "...", "last": true }
//!
//! // Agent -> Browser (single-envelope — texts ≤12 KB UTF-8)
//! { "t": "clipboard:content", "text": "hello", "req_id": Option<u64> }
//! { "t": "clipboard:error",   "message": "reason", "req_id": Option<u64> }
//!
//! // Agent -> Browser (chunked — rc.44+ — texts > 12 KB UTF-8)
//! { "t": "clipboard:content-chunk", "req_id": Option<u64>, "seq": 0, "text": "...", "last": false }
//! { "t": "clipboard:content-chunk", "req_id": Option<u64>, "seq": 1, "text": "...", "last": true }
//! ```
//!
//! `req_id` round-trips an optional u64 from the read request so the
//! browser can pair responses to its requests if it interleaves
//! multiple reads. Omitted on unsolicited change notifications (not
//! emitted today — the browser drives all reads explicitly to avoid
//! privacy surprises on the controlled host).
//!
//! rc.44 — chunked variants. The single-envelope `clipboard:write`
//! shape sent a `text` field unbounded by length, which on payloads
//! >~50 KB hit webrtc-rs's SCTP `max_message_size=65536` default and
//! threw `failed to handle_inbound: ErrChunk`, killing the data
//! channel + session (a third field-test host field repro 2026-05-19, every 1-2 min
//! sessions). The chunked variants cap each envelope at ~14 KB to
//! stay well under the SCTP ceiling; the receiver reassembles by
//! `id` (write) / `req_id` (read response) and applies on `last`.
//! Total payload is capped at [`MAX_CLIPBOARD_BYTES`] (1 MB) to
//! prevent OOM by malicious clients.
//!
//! Thread-pinning: `arboard::Clipboard` on Windows uses Win32's
//! OpenClipboard/SetClipboardData, which are thread-affine and also
//! require a Windows message pump on the owner thread — easiest to
//! satisfy by parking a dedicated OS thread that owns the clipboard
//! handle and services Read/Write via a `std::sync::mpsc` command
//! channel. Same pattern the `input` / `capture` modules use.

#![cfg(feature = "clipboard")]

use anyhow::{Context, Result};
use std::sync::mpsc as std_mpsc;
use std::thread;
use tokio::sync::oneshot;

/// Command sent to the clipboard worker thread. Replies come back
/// over the oneshot carried in each variant.
pub(crate) enum ClipboardCmd {
    Read {
        reply: oneshot::Sender<Result<String>>,
    },
    Write {
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Kept as an affordance for future deterministic shutdowns (e.g.
    /// a test harness that wants to join the worker). Today the
    /// `Clipboard` handle has no Drop impl — dropping the last
    /// `Sender` returns `Err` from `rx.recv()` which ends the worker
    /// loop naturally.
    #[allow(dead_code)]
    Shutdown,
}

/// Handle to a thread-pinned `arboard::Clipboard`. Cheap to clone
/// (`Sender` is Arc'd internally) so multiple data channels in the
/// same session can share one worker.
#[derive(Clone)]
pub struct Clipboard {
    tx: std_mpsc::Sender<ClipboardCmd>,
}

impl Clipboard {
    /// Spin up the worker thread. The `arboard::Clipboard` is
    /// constructed on the worker so the handle never crosses thread
    /// boundaries, which matters on Windows (the OpenClipboard
    /// ownership is per-thread).
    pub fn new() -> Result<Self> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let (tx, rx) = std_mpsc::channel::<ClipboardCmd>();

        thread::Builder::new()
            .name("roomler-agent-clipboard".into())
            .spawn(move || {
                let mut cb = match arboard::Clipboard::new() {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("arboard::Clipboard::new: {e}")));
                        return;
                    }
                };
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        ClipboardCmd::Read { reply } => {
                            let res = cb
                                .get_text()
                                .map_err(|e| anyhow::anyhow!("clipboard get_text: {e}"));
                            let _ = reply.send(res);
                        }
                        ClipboardCmd::Write { text, reply } => {
                            let res = cb
                                .set_text(text)
                                .map_err(|e| anyhow::anyhow!("clipboard set_text: {e}"));
                            let _ = reply.send(res);
                        }
                        ClipboardCmd::Shutdown => break,
                    }
                }
            })
            .context("spawning clipboard worker")?;

        ready_rx
            .recv()
            .context("clipboard worker ack")?
            .context("clipboard worker init")?;

        Ok(Self { tx })
    }

    /// Read the current clipboard text. Empty string on "no text
    /// content" (clipboard holds image/file/nothing). Errors if the
    /// worker has died or the OS clipboard is locked by another
    /// process.
    pub async fn read(&self) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::Read { reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }

    /// Replace the clipboard with the given text.
    pub async fn write(&self, text: String) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::Write {
                text,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }
}

// No Drop impl. `Clipboard` is `Clone` (the Sender is Arc'd internally);
// a Drop-sends-Shutdown would fire on every clone drop, including the
// first, killing the worker prematurely. With no Drop, the worker
// exits naturally when all Sender clones are dropped and `rx.recv()`
// returns `Err(RecvError)` — which ends the `while let Ok(cmd) ...`
// loop. `ClipboardCmd::Shutdown` is still honoured for deterministic
// shutdowns inside the test suite.

/// Incoming clipboard DC message shape. Parsed from the JSON payload
/// the browser sends; the handler in `peer.rs` dispatches on the `t`
/// discriminator.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "t")]
pub(crate) enum ClipboardIncoming {
    #[serde(rename = "clipboard:write")]
    Write { text: String },
    #[serde(rename = "clipboard:write-chunk")]
    WriteChunk {
        id: String,
        seq: u32,
        text: String,
        last: bool,
    },
    #[serde(rename = "clipboard:read")]
    Read {
        #[serde(default)]
        req_id: Option<u64>,
    },
}

/// Hard ceiling on the total reassembled clipboard payload — both
/// for inbound writes (browser → agent) and outbound content
/// (agent → browser when chunking the read response). Anything above
/// this is dropped with an error reply. 1 MB is comfortably above
/// any reasonable clipboard text payload (a 200-page novel manuscript
/// is ~500 KB UTF-8) and well under the 100 MB-ish where SCTP's
/// buffer accounting gets uncomfortable.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Soft byte budget per outbound chunk. Stays well under webrtc-rs's
/// SCTP `max_message_size=65536` ceiling so the JSON envelope
/// overhead + UTF-8 expansion can't push a chunk over the boundary.
/// Used by the agent's `clipboard:content-chunk` emitter; the
/// browser uses the same constant on its `clipboard:write-chunk`
/// emitter (`useRemoteControl.ts::CLIPBOARD_CHUNK_BYTES`). Keep both
/// in lockstep.
pub const CHUNK_BYTES: usize = 14 * 1024;

/// Per-session reassembler for `clipboard:write-chunk` envelopes.
/// One instance per [`attach_clipboard_handler`] invocation; lookups
/// keyed by the browser-assigned `id`. Drops entries on the final
/// chunk (`last: true`) and on the next call to [`Self::write_chunk`]
/// that exceeds [`MAX_CLIPBOARD_BYTES`] (sets the entry to an
/// "errored" sentinel via removal — caller emits the error reply).
#[derive(Default)]
pub(crate) struct WriteReassembler {
    in_flight: std::collections::HashMap<String, WriteAccumulator>,
}

pub(crate) struct WriteAccumulator {
    buf: String,
    next_seq: u32,
}

/// Outcome of feeding one chunk through the reassembler. `Pending`
/// means more chunks are expected; `Complete(text)` means the
/// `last` bit fired and the caller should write `text` to the OS
/// clipboard; `Rejected(reason)` means the chunk violated an
/// invariant (size cap exceeded, seq out of order) and the caller
/// should emit a `clipboard:error` reply with the reason.
#[derive(Debug)]
pub(crate) enum WriteChunkOutcome {
    Pending,
    Complete(String),
    Rejected(String),
}

impl WriteReassembler {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound `clipboard:write-chunk` envelope.
    pub(crate) fn feed(
        &mut self,
        id: String,
        seq: u32,
        text: String,
        last: bool,
    ) -> WriteChunkOutcome {
        // Pull the existing accumulator out by value so we don't have
        // to juggle a borrow of `self.in_flight` across the rejection
        // paths (which need to drop the entry). If no accumulator
        // exists yet, this is the first chunk for `id`.
        let mut acc = self.in_flight.remove(&id).unwrap_or(WriteAccumulator {
            buf: String::new(),
            next_seq: 0,
        });
        if seq != acc.next_seq {
            // Drop the partial: the sender's state is unrecoverable;
            // they need to restart with a fresh `id`.
            return WriteChunkOutcome::Rejected(format!(
                "clipboard chunk seq mismatch — expected {}, got {seq}",
                acc.next_seq
            ));
        }
        if acc.buf.len() + text.len() > MAX_CLIPBOARD_BYTES {
            // Drop the partial: caller hit the hard cap.
            return WriteChunkOutcome::Rejected(format!(
                "clipboard payload exceeds {MAX_CLIPBOARD_BYTES}-byte cap"
            ));
        }
        acc.buf.push_str(&text);
        acc.next_seq = acc.next_seq.saturating_add(1);
        if last {
            WriteChunkOutcome::Complete(acc.buf)
        } else {
            self.in_flight.insert(id, acc);
            WriteChunkOutcome::Pending
        }
    }

    /// Number of in-flight write transactions. Test-only helper.
    #[cfg(test)]
    pub(crate) fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }
}

/// Split a UTF-8 string into JSON-safe chunks of at most
/// [`CHUNK_BYTES`] bytes each, splitting on UTF-8 codepoint
/// boundaries so reassembly via plain string concatenation always
/// yields the original. The agent's `clipboard:content-chunk`
/// emitter uses this; tests lock the boundary handling.
pub(crate) fn split_into_chunks(text: &str) -> Vec<&str> {
    if text.len() <= CHUNK_BYTES {
        // Even a string with all 4-byte UTF-8 codepoints fits in one
        // chunk if `text.len()` is already under the limit — String
        // tracks byte length, so this is correct.
        return vec![text];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let target_end = (start + CHUNK_BYTES).min(text.len());
        // Walk back to the nearest UTF-8 boundary. `is_char_boundary`
        // is O(1) per call. At worst we walk back 3 bytes (max
        // continuation-byte run for valid UTF-8).
        let mut end = target_end;
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        // `end == start` shouldn't happen for a non-empty text with
        // CHUNK_BYTES ≥ 4 (max codepoint width), but guard anyway:
        // emit at least one full codepoint per chunk to avoid an
        // infinite loop. `char_indices()` gives us the next boundary
        // strictly past `start`.
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| start + i)
                .unwrap_or(text.len());
        }
        out.push(&text[start..end]);
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_incoming_write() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:write","text":"hi"}"#).unwrap();
        match m {
            ClipboardIncoming::Write { text } => assert_eq!(text, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_read_with_req_id() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:read","req_id":42}"#).unwrap();
        match m {
            ClipboardIncoming::Read { req_id } => assert_eq!(req_id, Some(42)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_read_without_req_id() {
        let m: ClipboardIncoming = serde_json::from_str(r#"{"t":"clipboard:read"}"#).unwrap();
        match m {
            ClipboardIncoming::Read { req_id } => assert_eq!(req_id, None),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_discriminator_fails_to_parse() {
        let res: serde_json::Result<ClipboardIncoming> =
            serde_json::from_str(r#"{"t":"clipboard:delete"}"#);
        assert!(res.is_err(), "unknown discriminator must not parse");
    }

    #[test]
    fn parse_incoming_write_chunk() {
        let m: ClipboardIncoming = serde_json::from_str(
            r#"{"t":"clipboard:write-chunk","id":"abc","seq":3,"text":"hello","last":true}"#,
        )
        .unwrap();
        match m {
            ClipboardIncoming::WriteChunk {
                id,
                seq,
                text,
                last,
            } => {
                assert_eq!(id, "abc");
                assert_eq!(seq, 3);
                assert_eq!(text, "hello");
                assert!(last);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn write_reassembler_accumulates_then_completes_on_last() {
        let mut r = WriteReassembler::new();
        assert!(matches!(
            r.feed("x".into(), 0, "hel".into(), false),
            WriteChunkOutcome::Pending
        ));
        assert_eq!(r.in_flight_count(), 1);
        assert!(matches!(
            r.feed("x".into(), 1, "lo ".into(), false),
            WriteChunkOutcome::Pending
        ));
        match r.feed("x".into(), 2, "world".into(), true) {
            WriteChunkOutcome::Complete(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Complete, got {other:?}"),
        }
        assert_eq!(r.in_flight_count(), 0, "completed entry must be dropped");
    }

    #[test]
    fn write_reassembler_interleaves_multiple_ids() {
        let mut r = WriteReassembler::new();
        r.feed("a".into(), 0, "AA".into(), false);
        r.feed("b".into(), 0, "BB".into(), false);
        assert_eq!(r.in_flight_count(), 2);
        let a = r.feed("a".into(), 1, "aa".into(), true);
        let b = r.feed("b".into(), 1, "bb".into(), true);
        assert!(
            matches!(a, WriteChunkOutcome::Complete(ref s) if s == "AAaa"),
            "got {a:?}"
        );
        assert!(
            matches!(b, WriteChunkOutcome::Complete(ref s) if s == "BBbb"),
            "got {b:?}"
        );
        assert_eq!(r.in_flight_count(), 0);
    }

    #[test]
    fn write_reassembler_rejects_out_of_order_seq() {
        let mut r = WriteReassembler::new();
        r.feed("x".into(), 0, "first".into(), false);
        match r.feed("x".into(), 5, "wat".into(), false) {
            WriteChunkOutcome::Rejected(reason) => {
                assert!(reason.contains("seq mismatch"), "reason was: {reason}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(
            r.in_flight_count(),
            0,
            "rejected entry must be dropped so a fresh start works"
        );
    }

    #[test]
    fn write_reassembler_rejects_oversized_payload() {
        let mut r = WriteReassembler::new();
        // Build up just over the 1 MB cap across 2 chunks of 600 KB each.
        let chunk = "x".repeat(600 * 1024);
        let first = r.feed("big".into(), 0, chunk.clone(), false);
        assert!(matches!(first, WriteChunkOutcome::Pending), "got {first:?}");
        match r.feed("big".into(), 1, chunk, true) {
            WriteChunkOutcome::Rejected(reason) => {
                assert!(
                    reason.contains("byte cap"),
                    "reason should mention the cap: {reason}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(r.in_flight_count(), 0);
    }

    #[test]
    fn split_into_chunks_passes_through_small_text() {
        let chunks = split_into_chunks("hello");
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_into_chunks_splits_long_ascii_at_chunk_bytes() {
        let text = "a".repeat(CHUNK_BYTES * 3);
        let chunks = split_into_chunks(&text);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.len() <= CHUNK_BYTES));
        // Round-trip via concatenation reproduces the original.
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_into_chunks_respects_utf8_codepoint_boundary() {
        // Build a string slightly over CHUNK_BYTES where a multi-byte
        // codepoint straddles the natural split point. The chunker
        // must walk back to the previous boundary so each chunk parses
        // as valid UTF-8.
        //
        // Crafting: fill to (CHUNK_BYTES - 2) with ASCII, then a 4-byte
        // codepoint (🦀 = 4 bytes UTF-8). The natural split at
        // CHUNK_BYTES lands in the middle of the crab — chunker must
        // either emit the codepoint whole in chunk 0 or push it whole
        // to chunk 1.
        let prefix = "a".repeat(CHUNK_BYTES - 2);
        let text = format!("{prefix}🦀b");
        let chunks = split_into_chunks(&text);
        // Concatenation always reproduces the original — strongest
        // invariant. The number of chunks doesn't matter for this
        // assertion.
        assert_eq!(chunks.concat(), text);
        // Each chunk must be valid UTF-8 (slicing &str at a non-boundary
        // would panic before we got here; we additionally assert each
        // chunk is non-empty + within byte budget).
        for c in &chunks {
            assert!(!c.is_empty(), "no empty chunks");
            assert!(
                c.len() <= CHUNK_BYTES,
                "chunk len {} exceeds budget {}",
                c.len(),
                CHUNK_BYTES
            );
        }
    }

    #[test]
    fn split_into_chunks_handles_empty_string() {
        // Empty text → single empty chunk (caller still emits
        // `clipboard:content-chunk { text: "", last: true }` once).
        let chunks = split_into_chunks("");
        assert_eq!(chunks, vec![""]);
    }

    /// The clipboard worker init may fail on headless CI runners that
    /// have no X server; accept that as a clean skip. If it does
    /// construct, a basic write/read round-trip works AND — locked in
    /// the same test because Windows `OpenClipboard` is process-wide
    /// exclusive and parallel tests would race — dropping a clone must
    /// NOT shut the worker down. The DC handler in `peer.rs` clones
    /// the cb into the per-message closure; if the old Drop impl sent
    /// Shutdown on clone drop, the second clipboard:read on a live
    /// session would fail with "clipboard worker gone" (user-reported
    /// on 0.1.33).
    ///
    /// On Windows, the OS clipboard is inherently racy — apps like
    /// paste-history / password managers may overwrite it between
    /// our `set_text` and `get_text` calls. The *content* assertions
    /// here are best-effort; the invariant this test locks is
    /// "worker survives a clone drop", expressed by the final write
    /// succeeding without "worker gone".
    #[tokio::test]
    async fn write_then_read_round_trip_and_survives_clone_drop() {
        let Ok(cb) = Clipboard::new() else {
            eprintln!("arboard not available in this env — skipping");
            return;
        };
        let payload = "roomler clipboard smoke test";
        cb.write(payload.to_string()).await.unwrap();
        // Soft read — another process may have already clobbered the
        // clipboard. Only enforce content equality when the read
        // actually returned our payload.
        if let Ok(back) = cb.read().await {
            if back == payload {
                // Good — OS let us keep our own write.
            } else {
                eprintln!("clipboard was overwritten externally; content check skipped");
            }
        } else {
            eprintln!("clipboard read hit transient OS error; content check skipped");
        }

        // Now drop a clone. This is the load-bearing assertion: if
        // the old Drop impl's Shutdown still ran on clone-drop, the
        // original's next `send` would return `SendError` and
        // `write()` would surface "clipboard worker gone". Soft-read
        // afterwards — we don't care what's in the OS clipboard,
        // only that our handle's worker is alive.
        {
            let clone = cb.clone();
            let _ = clone.write("from clone".to_string()).await;
        } // clone drops here; worker MUST stay alive.
        cb.write("from original".to_string())
            .await
            .expect("worker must still be alive after a clone was dropped");
        let _ = cb.read().await;
    }
}
