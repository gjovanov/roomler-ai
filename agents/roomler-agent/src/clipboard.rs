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
//! // Browser -> Agent
//! { "t": "clipboard:write", "text": "hello" }
//! { "t": "clipboard:read" }
//!
//! // Agent -> Browser
//! { "t": "clipboard:content", "text": "hello", "req_id": Option<u64> }
//! { "t": "clipboard:error",   "message": "reason" }
//! ```
//!
//! `req_id` round-trips an optional u64 from the read request so the
//! browser can pair responses to its requests if it interleaves
//! multiple reads. Omitted on unsolicited change notifications (not
//! emitted today — the browser drives all reads explicitly to avoid
//! privacy surprises on the controlled host).
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
    #[serde(rename = "clipboard:read")]
    Read {
        #[serde(default)]
        req_id: Option<u64>,
    },
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
    #[tokio::test]
    async fn write_then_read_round_trip_and_survives_clone_drop() {
        let Ok(cb) = Clipboard::new() else {
            eprintln!("arboard not available in this env — skipping");
            return;
        };
        let payload = "roomler clipboard smoke test";
        cb.write(payload.to_string()).await.unwrap();
        let back = cb.read().await.unwrap();
        assert_eq!(back, payload);

        // Now drop a clone and confirm the original still works.
        {
            let clone = cb.clone();
            clone.write("from clone".to_string()).await.unwrap();
        } // clone drops here; worker MUST stay alive.
        cb.write("from original".to_string()).await.unwrap();
        let back = cb.read().await.unwrap();
        assert_eq!(back, "from original");
    }
}
