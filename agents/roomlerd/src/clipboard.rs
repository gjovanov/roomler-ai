// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Clipboard data-channel handler.
//!
//! Round-trip clipboard content between the browser controller and
//! the agent host over the WebRTC `clipboard` data channel (a
//! reliable, ordered DC). Protocol v2 (0.3.0-rc.227+) carries text
//! AND PNG images, write-acks, and unsolicited change events for
//! auto-sync; every v2 element is additive so v1 peers interoperate
//! untouched.
//!
//! Wire protocol (JSON control frames + raw binary image frames):
//!
//! ```text
//! // Browser -> Agent
//! { "t": "clipboard:write", "text": "hello", "id": "cb-…"? }        // id v2: requests an ack
//! { "t": "clipboard:write-chunk", "id": "abc123", "seq": 0, "text": "...", "last": false }
//! { "t": "clipboard:read", "req_id": 42?, "accept": ["text","image"]? }
//! { "t": "clipboard:subscribe", "events": ["text","image"] }        // v2 auto-sync
//! { "t": "clipboard:unsubscribe" }                                  // v2
//! { "t": "clipboard:img-begin", "id": "cb-…", "w": 1920, "h": 1080,
//!   "bytes": 123456, "format": "png" }                              // v2, then ≤16 KiB binary frames
//! { "t": "clipboard:img-end", "id": "cb-…" }                        // v2
//!
//! // Agent -> Browser
//! { "t": "clipboard:content", "text": "hello", "req_id": Option<u64> }
//! { "t": "clipboard:content-chunk", "req_id": …, "seq": 0, "text": "...", "last": false }
//! { "t": "clipboard:write-ack", "id": "cb-…", "bytes": 5 }          // v2, only for id-stamped writes
//! { "t": "clipboard:event", "kind": "text", "text": "..." }         // v2, after subscribe
//! { "t": "clipboard:event-chunk", "event_id": "ev-1", "seq": 0, "text": "...", "last": false }
//! { "t": "clipboard:img-begin", "id": "aimg-0", "w": …, "h": …, "bytes": …,
//!   "format": "png", "req_id": Option<u64> }                        // v2, then 64 KiB binary frames
//! { "t": "clipboard:img-end", "id": "aimg-0", "bytes": … }          // v2
//! { "t": "clipboard:error", "message": "reason", "req_id": Option<u64>, "id": Option<String> }
//! ```
//!
//! `req_id` round-trips an optional u64 from the read request so the
//! browser can pair responses to its requests. On an image reply it
//! distinguishes "answer to your read" (present) from "unsolicited
//! change event" (absent). Text is canonical LF on the wire; the
//! worker converts to the host's convention (`CRLF` on Windows) on
//! every write and back on every read — see [`host_to_wire`] /
//! [`wire_to_host`]. Change events are pushed only after an explicit
//! `clipboard:subscribe`, and the CLIPBOARD permission bit gates the
//! whole DC (enforced in `peer.rs::attach_clipboard_handler`).
//!
//! rc.44 — chunked variants. The single-envelope `clipboard:write`
//! shape sent a `text` field unbounded by length, which on payloads
//! over ~50 KB hit webrtc-rs's SCTP `max_message_size=65536` default
//! and threw `failed to handle_inbound: ErrChunk`, killing the data
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
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

use crate::virtual_desktop::XServerNow;

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
    /// v2 — read the host clipboard as a PNG image. `Ok(None)` when
    /// the clipboard holds no image content.
    ReadImage {
        reply: oneshot::Sender<Result<Option<PngImage>>>,
    },
    /// v2 — decode the PNG and place it on the host clipboard.
    WriteImage {
        png: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// v2.1 — read the host clipboard as HTML (+ its plain-text alt).
    /// `Ok(None)` when the clipboard holds no HTML content.
    ReadHtml {
        reply: oneshot::Sender<Result<Option<HtmlPayload>>>,
    },
    /// v2.1 — write HTML + plain-text alternate to the host clipboard
    /// atomically (one clipboard transaction, both formats — a paste
    /// target picks the richest it understands).
    WriteHtml {
        html: String,
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// v2.2 — read the host clipboard's NATIVE formats (RTF + html +
    /// text). `Ok(None)` when no RTF is present (the html/text lanes
    /// cover that). Windows-only in practice — elsewhere always None.
    ReadNative {
        reply: oneshot::Sender<Result<Option<NativePayload>>>,
    },
    /// v2.2 — write RTF + html + text to the host clipboard (html+text
    /// in one arboard transaction, RTF appended via
    /// `set_without_clear`). Windows-only; an error elsewhere.
    WriteNative {
        payload: Box<NativePayload>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// v2 — install a change watcher subscription. The worker switches
    /// from blocking `recv()` to a tick loop while ANY subscription is
    /// live and pushes [`ClipboardChange`]s into each subscription's `tx`
    /// when the HOST clipboard changes. Fire-and-forget: no reply.
    ///
    /// Multi-user P3: keyed by a caller-held token — pre-P3 this was a
    /// single slot, so with two concurrent sessions the second session's
    /// subscribe silently STOLE the first's change feed and either
    /// session's unsubscribe killed both.
    Watch {
        id: u64,
        events: WatchEvents,
        tx: tokio::sync::mpsc::Sender<ClipboardChange>,
    },
    /// v2 — remove ONE watcher subscription by its token (browser
    /// unsubscribed or the DC closed). Fire-and-forget, idempotent.
    Unwatch { id: u64 },
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
    /// ownership is per-thread). On Linux it follows the daemon's own
    /// virtual desktop to its next X server ([`x11_follow`], #3671).
    pub fn new() -> Result<Self> {
        Self::following(crate::virtual_desktop::x_server_now)
    }

    /// [`Self::new`], reading the daemon's own desktop from `desk`: a test
    /// passes a stand-in of its own.
    fn following(desk: fn() -> Option<crate::virtual_desktop::XServerNow>) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        let _ = desk;
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let (tx, rx) = std_mpsc::channel::<ClipboardCmd>();

        thread::Builder::new()
            .name("roomlerd-clipboard".into())
            .spawn(move || {
                // #3671 — the desktop as it was BEFORE the connect: a server
                // that comes back during it is followed at the first look.
                #[cfg(target_os = "linux")]
                let born = desk();
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
                // Content the worker itself last wrote (remote-initiated
                // writes) — the change watcher compares against these so
                // a browser-originated write is never echoed back to the
                // browser as a "host change".
                let mut marks = SelfMarks::default();
                // Multi-user P3 — N concurrent subscriptions (one per
                // session), each with its OWN change baseline so a
                // late-joining session isn't immediately pushed the
                // pre-existing clipboard as a "change".
                let mut watchers: std::collections::HashMap<u64, WatcherState> =
                    std::collections::HashMap::new();
                // FR-97 P3e-3a / P2e-1 — the change gate (an X11 session's
                // owner events, a Mac's change count), opened with the first
                // watcher and kept; `None` where there is none.
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                let mut gate: Option<change_gate::Gate> = None;
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                let mut gate_tried = false;
                // #3671 — the desktop generation this worker's handle is on.
                #[cfg(target_os = "linux")]
                let mut follow = x11_follow::Follow::<arboard::Clipboard>::new(
                    born,
                    std::sync::Arc::new(x11_follow::connect_anew),
                    x11_follow::REBUILD_BOUND,
                );
                loop {
                    // Block indefinitely when nothing is watching (the
                    // pre-v2 behavior — zero idle wakeups); poll at the
                    // tick cadence while any watcher is installed.
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    let tick = if gate.is_some() {
                        GATED_TICK
                    } else {
                        WATCH_TICK
                    };
                    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                    let tick = WATCH_TICK;
                    let cmd = if !watchers.is_empty() {
                        match rx.recv_timeout(tick) {
                            Ok(c) => Some(c),
                            Err(std_mpsc::RecvTimeoutError::Timeout) => None,
                            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv() {
                            Ok(c) => Some(c),
                            Err(_) => break,
                        }
                    };
                    // #3671 — the desktop came back on a new X server, and
                    // this handle and the gate's connection were the old
                    // one's. Before the command, so that it meets the new
                    // server; a watcher's images follow the new gate.
                    #[cfg(target_os = "linux")]
                    if let Some(fresh) = follow.check(desk(), std::time::Instant::now()) {
                        cb = fresh;
                        gate = None;
                        gate_tried = false;
                        if !watchers.is_empty() {
                            gate_tried = true;
                            gate = change_gate::Gate::open();
                            let seq = gate_tick(&mut gate);
                            for w in watchers.values_mut() {
                                w.events.image = w.asked_image && seq.is_some();
                                w.last_seen_seq = seq.unwrap_or(0);
                            }
                        }
                    }
                    match cmd {
                        Some(ClipboardCmd::Read { reply }) => {
                            let _ = reply.send(worker_read_text(&mut cb));
                        }
                        Some(ClipboardCmd::Write { text, reply }) => {
                            let _ = reply.send(worker_write_text(&mut cb, &text, &mut marks));
                        }
                        Some(ClipboardCmd::ReadImage { reply }) => {
                            let _ = reply.send(worker_read_image(&mut cb));
                        }
                        Some(ClipboardCmd::WriteImage { png, reply }) => {
                            let _ = reply.send(worker_write_image(&mut cb, &png, &mut marks));
                        }
                        Some(ClipboardCmd::ReadHtml { reply }) => {
                            let _ = reply.send(worker_read_html(&mut cb));
                        }
                        Some(ClipboardCmd::WriteHtml { html, text, reply }) => {
                            let _ =
                                reply.send(worker_write_html(&mut cb, &html, &text, &mut marks));
                        }
                        Some(ClipboardCmd::ReadNative { reply }) => {
                            let _ = reply.send(worker_read_native(&mut cb));
                        }
                        Some(ClipboardCmd::WriteNative { payload, reply }) => {
                            let _ = reply.send(worker_write_native(&mut cb, &payload, &mut marks));
                        }
                        Some(ClipboardCmd::Watch { id, events, tx }) => {
                            #[cfg(any(target_os = "linux", target_os = "macos"))]
                            let gate_seq = {
                                if gate.is_none() && !gate_tried {
                                    gate_tried = true;
                                    gate = change_gate::Gate::open();
                                }
                                gate_tick(&mut gate)
                            };
                            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                            let gate_seq = None;
                            watchers.insert(id, install_watcher(&mut cb, events, tx, gate_seq));
                        }
                        Some(ClipboardCmd::Unwatch { id }) => {
                            watchers.remove(&id);
                        }
                        Some(ClipboardCmd::Shutdown) => break,
                        None => {
                            // Dead receivers (session died without an
                            // explicit unsubscribe) are dropped so the
                            // worker can fall back to blocking recv.
                            watchers.retain(|_, w| !w.receiver_gone());
                            // One read of the gate per tick, for every watcher.
                            #[cfg(any(target_os = "linux", target_os = "macos"))]
                            let gate_seq = gate_tick(&mut gate);
                            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                            let gate_seq = None;
                            for w in watchers.values_mut() {
                                watch_tick(&mut cb, w, &marks, gate_seq);
                            }
                        }
                    }
                    // FR-97 P3e-3a — no watcher, no gate: an idle worker
                    // blocks on its commands and would read no events, and
                    // an X server buffers what a client does not read. A
                    // Mac's gate holds nothing, but the next one starts
                    // from the count at the next watch, as X11's does.
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    if watchers.is_empty() && gate.is_some() {
                        gate = None;
                        gate_tried = false;
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

    /// v2 — read the host clipboard as a PNG image. `Ok(None)` when
    /// the clipboard holds no image content.
    pub async fn read_image(&self) -> Result<Option<PngImage>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::ReadImage { reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }

    /// v2 — decode `png` and place it on the host clipboard.
    pub async fn write_image(&self, png: Vec<u8>) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::WriteImage {
                png,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }

    /// v2.1 — read the host clipboard as HTML (+ plain-text alt);
    /// `None` when it holds no HTML.
    pub async fn read_html(&self) -> Result<Option<HtmlPayload>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::ReadHtml { reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }

    /// v2.1 — write HTML + plain-text alternate atomically.
    pub async fn write_html(&self, html: String, text: String) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::WriteHtml {
                html,
                text,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }

    /// v2.2 — read the native formats (RTF + alternates); `None` when
    /// the clipboard holds no RTF.
    pub async fn read_native(&self) -> Result<Option<NativePayload>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::ReadNative { reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }

    /// v2.2 — write RTF + html + text (Windows).
    pub async fn write_native(&self, payload: NativePayload) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClipboardCmd::WriteNative {
                payload: Box::new(payload),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("clipboard worker dropped reply"))?
    }

    /// v2.2 — the process-shared clipboard worker. One worker (and one
    /// set of echo self-marks) serves every consumer — the per-session
    /// DC handlers AND the loopback bridge — so a bridge write is
    /// never echoed back by a concurrently-subscribed session watcher.
    /// `None` when the OS clipboard is unavailable (headless).
    ///
    /// #3671 — on a host whose desktop is the daemon's own, a start that
    /// failed while the desktop was down (in a restart, in a back-off gap, or
    /// after a first start that failed or was slow) is made again at the next
    /// ask once the desktop is up, as [`Retry`] says. Elsewhere the first
    /// failure is kept for the daemon's life, as before. The retry has no kill
    /// switch of its own: `ROOMLERD_CLIPBOARD_X11_REBUILD=0` turns off only
    /// the worker's follow of a new X server (`x11_follow`), and without the
    /// retry a daemon whose first start failed has no clipboard for its life.
    pub fn shared() -> Option<Clipboard> {
        static SHARED: Kept<Clipboard> = Kept::new();
        shared_at(
            &SHARED,
            crate::virtual_desktop::x_server_now(),
            Instant::now(),
            Clipboard::new,
        )
    }

    /// v2 — install a change-watcher SUBSCRIPTION. Changes arrive on `tx`;
    /// the worker drops pushes when the channel is full and retries on the
    /// next tick. Returns the subscription token for [`Clipboard::unwatch`].
    ///
    /// Multi-user P3: N subscriptions coexist (one per session); pre-P3 the
    /// single slot meant a 2nd session's subscribe stole the 1st's feed and
    /// either unsubscribe killed both.
    pub(crate) fn watch(
        &self,
        events: WatchEvents,
        tx: tokio::sync::mpsc::Sender<ClipboardChange>,
    ) -> Result<u64> {
        static NEXT_WATCH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_WATCH_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.tx
            .send(ClipboardCmd::Watch { id, events, tx })
            .map_err(|_| anyhow::anyhow!("clipboard worker gone"))?;
        Ok(id)
    }

    /// v2 — remove ONE subscription by token. Idempotent, fire-and-forget.
    pub(crate) fn unwatch(&self, id: u64) {
        let _ = self.tx.send(ClipboardCmd::Unwatch { id });
    }
}

// No Drop impl. `Clipboard` is `Clone` (the Sender is Arc'd internally);
// a Drop-sends-Shutdown would fire on every clone drop, including the
// first, killing the worker prematurely. With no Drop, the worker
// exits naturally when all Sender clones are dropped and `rx.recv()`
// returns `Err(RecvError)` — which ends the worker loop. `ClipboardCmd::
// Shutdown` is still honoured for deterministic shutdowns inside the
// test suite.

// ── Clipboard protocol v2: change watcher + images ──────────────────────────

/// Which change kinds a [`ClipboardCmd::Watch`] subscription wants
/// pushed. Image watching is honoured on Windows only (no cheap
/// change signal elsewhere — see [`watch_tick`]); text + html work
/// everywhere arboard does.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WatchEvents {
    pub text: bool,
    pub image: bool,
    pub html: bool,
    /// v2.2 — RTF-bearing native events (Windows only; needs the
    /// viewer side to run a loopback bridge to consume them).
    pub native: bool,
}

/// A PNG-encoded clipboard image (the wire form on the clipboard DC).
#[derive(Debug, Clone)]
pub struct PngImage {
    pub w: u32,
    pub h: u32,
    pub png: Vec<u8>,
}

/// v2.1 — HTML clipboard content + its plain-text alternate. The pair
/// travels together so the receiving side writes BOTH formats in one
/// clipboard transaction (rich-aware paste targets pick the HTML,
/// plain editors the text).
#[derive(Debug, Clone)]
pub struct HtmlPayload {
    pub html: String,
    pub text: String,
}

/// v2.2 — NATIVE clipboard content: RTF (the only format that carries
/// a document's EMBEDDED images as self-contained bytes) plus the
/// html/text alternates. Only reachable through an agent — no browser
/// API exposes RTF — which is what the loopback clipboard bridge is
/// for. `rtf` is required (else the html/text lanes suffice); `html`
/// and `text` may be empty.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NativePayload {
    /// Raw RTF bytes (Windows "Rich Text Format" registered format).
    #[serde(with = "base64_bytes")]
    pub rtf: Vec<u8>,
    #[serde(default)]
    pub html: String,
    #[serde(default)]
    pub text: String,
}

/// Serde adapter: `Vec<u8>` ⇄ base64 string (the bridge's JSON body).
mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    pub fn serialize<S: serde::Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s: String = serde::Deserialize::deserialize(d)?;
        STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Host-clipboard change pushed to a [`ClipboardCmd::Watch`] subscriber.
#[derive(Debug)]
pub(crate) enum ClipboardChange {
    Text(String),
    Image(PngImage),
    Html(HtmlPayload),
    Native(Box<NativePayload>),
}

/// Watcher tick cadence. Windows gets a fast tick because the
/// unchanged-path is a single `GetClipboardSequenceNumber` syscall
/// (no clipboard open); elsewhere every tick reads + hashes the text,
/// so poll at 1 Hz.
#[cfg(windows)]
const WATCH_TICK: std::time::Duration = std::time::Duration::from_millis(200);
#[cfg(not(windows))]
const WATCH_TICK: std::time::Duration = std::time::Duration::from_millis(1000);

/// FR-97 P3e-3a / P2e-1 — behind a change gate an unchanged tick is one event
/// drain on a local socket (X11) or one change-count read (a Mac), so it runs
/// at Windows' cadence.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const GATED_TICK: std::time::Duration = std::time::Duration::from_millis(200);

// FR-97 P3e-3a / P2e-1 — this OS's clipboard change gate. Windows has its own
// sequence number and needs neither.
#[cfg(target_os = "macos")]
use self::mac_gate as change_gate;
#[cfg(target_os = "linux")]
use self::x11_gate as change_gate;

/// Content the worker itself last wrote to the OS clipboard. The
/// change watcher treats matching state as "our own write" and stays
/// silent — one half of the echo-suppression loop (the browser holds
/// the other half in its `createClipboardEchoGate`).
#[derive(Default)]
struct SelfMarks {
    /// `GetClipboardSequenceNumber` observed right after our own write.
    #[cfg(windows)]
    seq: u32,
    /// FNV-1a of the canonical (LF) text we last wrote.
    text_hash: u64,
    /// FNV-1a of the deterministic re-encode of the image we last
    /// wrote (our own encoder — byte-identical to what a later
    /// [`watch_tick`] re-encode produces, unlike the browser's PNG
    /// bytes which come from a different encoder).
    img_hash: u64,
    /// v2.1 — [`html_event_hash`] of the READ-BACK of the html we
    /// last wrote (the OS re-wraps CF_HTML, so hashing our input
    /// would never match what a later tick reads).
    html_hash: u64,
    /// v2.2 — FNV of the RTF bytes we last wrote. Custom registered
    /// formats round-trip verbatim (no OS re-wrap), so the input hash
    /// matches a later tick's read.
    #[cfg_attr(not(windows), allow(dead_code))]
    native_hash: u64,
}

struct WatcherState {
    events: WatchEvents,
    tx: tokio::sync::mpsc::Sender<ClipboardChange>,
    /// Windows' sequence number, or (FR-97 P3e-3a, P2e-1) the change gate's
    /// counter, as this watcher last saw it.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    last_seen_seq: u32,
    last_text_hash: u64,
    last_img_hash: u64,
    last_html_hash: u64,
    #[cfg_attr(not(windows), allow(dead_code))]
    last_native_hash: u64,
    /// #3671 — the subscriber asked for images. The change gate decides
    /// whether it gets them, and the gate of the desktop's next X server
    /// decides again.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    asked_image: bool,
    /// Set when `try_send` hit a full channel or a read failed
    /// transiently — the next tick bypasses the unchanged-seq early
    /// exit and re-attempts.
    retry: bool,
}

impl WatcherState {
    /// Multi-user P3 — a subscription whose consumer died (session torn
    /// down without an explicit unsubscribe): the worker reaps it on the
    /// next tick so an all-dead registry falls back to blocking recv.
    fn receiver_gone(&self) -> bool {
        self.tx.is_closed()
    }
}

/// `GetClipboardSequenceNumber` via clipboard-win. `None` when the
/// calling thread's window station denies clipboard access.
#[cfg(windows)]
fn win_clipboard_seq() -> Option<u32> {
    clipboard_win::raw::seq_num().map(|n| n.get())
}

/// FR-97 P3e-3a — the gate's counter for this tick; a gate whose connection
/// broke is dropped, and its watchers fall back to the ungated path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn gate_tick(gate: &mut Option<change_gate::Gate>) -> Option<u32> {
    let seq = gate.as_mut().and_then(change_gate::Gate::seq);
    if seq.is_none() {
        *gate = None;
    }
    seq
}

/// #3671 — when an attempt on the X server of the daemon's own virtual desktop
/// that failed is made again. One rule for the clipboard's shared worker
/// ([`Clipboard::shared`]), the change gate's answer for the hello
/// (`x11_gate::available`) and the worker following the desktop to a new
/// server (`x11_follow`): a failure is kept only while its cause can still
/// hold.
///
/// - **No desktop of the daemon's own** ([`crate::virtual_desktop::x_server_now`]
///   reads `None`: a person's display or none, or the desktop's teardown at a
///   stop): kept for the daemon's life, as before #3671. This daemon does not
///   see that server come and go.
/// - **The desktop is down**: nothing is tried; there is no server to reach.
/// - **Up on another server than the one the failure met** (it failed while
///   the desktop was down, before its first server came up, or on an older
///   generation): tried at the next ask.
/// - **Up on the server the failure met**: tried again no sooner than
///   [`Retry::EVERY`] after it, however often it is asked.
///
/// A failure is said once per period of the desktop (an up period is its
/// generation, a down period its count), and a success that ends one once.
#[derive(Debug)]
pub(crate) struct Retry {
    /// The last failure: the desktop as it was read before the attempt, and
    /// when the attempt was made.
    failed: Option<(Option<XServerNow>, Instant)>,
    /// The period of the desktop the last failure was said in.
    said: Option<Option<XServerNow>>,
}

impl Retry {
    /// How often an attempt that failed on the server that still answers is
    /// made again.
    pub(crate) const EVERY: Duration = Duration::from_secs(10);

    pub(crate) const fn new() -> Self {
        Self {
            failed: None,
            said: None,
        }
    }

    /// Whether an attempt may be made at `now`, with the desktop as `desk`
    /// reads.
    pub(crate) fn may_try(&self, desk: Option<XServerNow>, now: Instant) -> bool {
        let Some((met, at)) = self.failed else {
            return true;
        };
        match desk {
            None => false,
            Some(d) if d.down.is_some() => false,
            Some(d) if met == Some(d) => now.saturating_duration_since(at) >= Self::EVERY,
            Some(_) => true,
        }
    }

    /// An attempt made at `now`, with the desktop as `desk` read before it,
    /// failed. `true`: the first failure in this period, to be said.
    pub(crate) fn failed(&mut self, desk: Option<XServerNow>, now: Instant) -> bool {
        self.failed = Some((desk, now));
        let first = self.said != Some(desk);
        self.said = Some(desk);
        first
    }

    /// An attempt succeeded. `true`: it ends a failure, to be said.
    pub(crate) fn succeeded(&mut self) -> bool {
        self.said = None;
        self.failed.take().is_some()
    }
}

/// What [`Kept::get_at`] has to say about the attempt it made, if it made one.
#[derive(Debug)]
pub(crate) enum Said<E> {
    /// Nothing new: the kept value, or a failure kept without an attempt.
    Nothing,
    /// An attempt failed, the first in this period of the desktop: say it.
    Failed(E),
    /// An attempt failed again, in a period that was said.
    Again(E),
    /// An attempt succeeded after a failure: say it, once.
    Started,
}

/// #3671 — a value made from the X server at the first ask and kept for the
/// daemon's life, where a failure is kept only as [`Retry`] says: what
/// [`Clipboard::shared`] and `x11_gate::available` keep. Before #3671 both
/// were a `OnceLock`, so a first ask while the desktop was down (in a restart,
/// in a back-off gap, or after a first start that failed or was slow) left
/// them off for good, though the desktop came back.
pub(crate) struct Kept<T> {
    st: std::sync::Mutex<(Option<T>, Retry)>,
}

impl<T: Clone> Kept<T> {
    pub(crate) const fn new() -> Self {
        Self {
            st: std::sync::Mutex::new((None, Retry::new())),
        }
    }

    /// The kept value, or `make`'s when [`Retry`] allows an attempt at
    /// `now`, with the desktop as `desk` reads. Callers wait for each other's
    /// attempt, as with a `OnceLock`.
    pub(crate) fn get_at<E>(
        &self,
        desk: Option<XServerNow>,
        now: Instant,
        make: impl FnOnce() -> Result<T, E>,
    ) -> (Option<T>, Said<E>) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let (value, retry) = &mut *st;
        if let Some(v) = value {
            return (Some(v.clone()), Said::Nothing);
        }
        if !retry.may_try(desk, now) {
            return (None, Said::Nothing);
        }
        match make() {
            Ok(v) => {
                *value = Some(v.clone());
                let said = if retry.succeeded() {
                    Said::Started
                } else {
                    Said::Nothing
                };
                (Some(v), said)
            }
            Err(e) => {
                let said = if retry.failed(desk, now) {
                    Said::Failed(e)
                } else {
                    Said::Again(e)
                };
                (None, said)
            }
        }
    }
}

/// [`Clipboard::shared`], with its slot, the desktop, the time and the
/// worker's start given: what its tests drive with a stand-in.
fn shared_at<T: Clone>(
    slot: &Kept<T>,
    desk: Option<XServerNow>,
    now: Instant,
    make: impl FnOnce() -> Result<T>,
) -> Option<T> {
    let (value, said) = slot.get_at(desk, now, make);
    match said {
        Said::Nothing => {}
        Said::Failed(e) if desk.is_none() => {
            tracing::warn!(%e, "clipboard: shared worker init failed — clipboard disabled");
        }
        Said::Failed(e) => tracing::warn!(
            %e,
            retry_s = Retry::EVERY.as_secs(),
            "clipboard: shared worker init failed on this device's virtual desktop — tried again at the next ask once its X server answers, at most every retry_s (#3671)"
        ),
        Said::Again(e) => tracing::debug!(%e, "clipboard: shared worker init failed again"),
        Said::Started => tracing::info!(
            "clipboard: the shared worker started on this device's virtual desktop, after a start that failed (#3671)"
        ),
    }
    value
}

/// Drop `handle` with arboard's lines for a dead connection held back on this
/// thread. On a dead connection arboard's `Drop` logs an ERROR for each
/// request it can no longer make ("Could not hand the clipboard data over to
/// the clipboard manager: …", "Failed to destroy the clipboard window. Error:
/// …", "Failed to flush the clipboard window. Error: …"), and they all went
/// with the server; the worker says what the rebuild came to, once (as
/// enigo's `Job::Gone` does, #3918). Exactly those three are held back
/// ([`AllBut`]). Every other line goes where it would: arboard's "The
/// clipboard server thread panicked", the one line in that `Drop` that would
/// name a bug, and anything else said during the drop. arboard's "Worker
/// thread errored with: …" at the server's death is its serving thread's,
/// and stays.
#[cfg(target_os = "linux")]
fn quietly_drop<T>(handle: T) {
    let all_but = tracing::Dispatch::new(AllBut(tracing::dispatcher::get_default(Clone::clone)));
    tracing::dispatcher::with_default(&all_but, move || drop(handle));
}

/// [`quietly_drop`]'s dispatcher: the one in use on the thread, less
/// arboard's lines for the requests a dead connection can no longer make
/// (arboard 3.6.1, `src/platform/linux/x11.rs`, `Drop for Clipboard`).
#[cfg(target_os = "linux")]
struct AllBut(tracing::Dispatch);

#[cfg(target_os = "linux")]
impl AllBut {
    const DEAD_CONNECTION: [&'static str; 3] = [
        "Could not hand the clipboard data over to the clipboard manager",
        "Failed to destroy the clipboard window",
        "Failed to flush the clipboard window",
    ];
}

/// An event's `message`, as `log`'s bridge records it.
#[cfg(target_os = "linux")]
struct Message(String);

#[cfg(target_os = "linux")]
impl tracing::field::Visit for Message {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{value:?}");
        }
    }
}

#[cfg(target_os = "linux")]
impl tracing::Subscriber for AllBut {
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        self.0.register_callsite(metadata)
    }

    /// The dispatcher's own hint is private to tracing-core (0.1.36), so this
    /// answers the process's current maximum, which already counts it: a hint
    /// is only an upper bound, `enabled` decides, and no hint at all would
    /// raise the maximum to TRACE for every callsite until the next rebuild.
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::current())
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        self.0.enabled(metadata)
    }

    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        self.0.new_span(attrs)
    }

    fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        self.0.record(span, values)
    }

    fn record_follows_from(&self, span: &tracing::span::Id, follows: &tracing::span::Id) {
        self.0.record_follows_from(span, follows)
    }

    fn event(&self, event: &tracing::Event<'_>) {
        let mut said = Message(String::new());
        event.record(&mut said);
        if !Self::DEAD_CONNECTION.iter().any(|l| said.0.starts_with(l)) {
            self.0.event(event);
        }
    }

    fn enter(&self, span: &tracing::span::Id) {
        self.0.enter(span)
    }

    fn exit(&self, span: &tracing::span::Id) {
        self.0.exit(span)
    }

    fn clone_span(&self, span: &tracing::span::Id) -> tracing::span::Id {
        self.0.clone_span(span)
    }

    fn try_close(&self, span: tracing::span::Id) -> bool {
        self.0.try_close(span)
    }
}

/// #3671 — [`quietly_drop`] holds back exactly arboard's three
/// lines for a dead connection, and every other line said during the drop
/// reaches the dispatcher in use: arboard's panic line, which would name a
/// bug, and a line of anyone else's. No X server and no clipboard: a `Drop`
/// that says what arboard's says, under a subscriber that keeps each message.
#[cfg(all(test, target_os = "linux"))]
mod quietly_drop_tests {
    use std::sync::{Arc, Mutex};

    /// Keeps each event's message, as [`super::Message`] reads it.
    #[derive(Clone, Default)]
    struct Heard(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for Heard {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut said = super::Message(String::new());
            event.record(&mut said);
            self.0.lock().unwrap().push(said.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Says what arboard 3.6.1's `Drop for Clipboard` says on a dead
    /// connection, then its panic line, then a line of someone else's.
    struct SaysWhatArboardSays;

    impl Drop for SaysWhatArboardSays {
        fn drop(&mut self) {
            let e = "Broken pipe (os error 32)";
            tracing::error!("Could not hand the clipboard data over to the clipboard manager: {e}");
            tracing::error!("Failed to destroy the clipboard window. Error: {e}");
            tracing::error!("Failed to flush the clipboard window. Error: {e}");
            tracing::error!("The clipboard server thread panicked. Error: {e}");
            tracing::warn!("someone else's line, said during the drop");
        }
    }

    #[test]
    fn quietly_drop_holds_back_only_arboards_three_dead_connection_lines() {
        let heard = Heard::default();
        tracing::subscriber::with_default(heard.clone(), || {
            super::quietly_drop(SaysWhatArboardSays)
        });
        let heard = heard.0.lock().unwrap().clone();
        assert_eq!(
            heard,
            [
                "The clipboard server thread panicked. Error: Broken pipe (os error 32)",
                "someone else's line, said during the drop",
            ],
            "quietly_drop held back a line it must pass, or passed one of arboard's three"
        );
    }
}

/// #3671 — the clipboard worker's X connection follows the daemon's own
/// virtual desktop to its next X server. The worker is one thread for the
/// daemon's life and arboard keeps one X connection per process, so once the
/// desktop's server exited and came back (its supervisor counts the
/// generations: [`crate::virtual_desktop::generation`]), every copy and paste
/// failed on the dead connection until the daemon restarted. Now the worker
/// looks before each command and each watch tick and, once the desktop is up
/// on a generation NEWER than its handle's, takes arboard's handle down and
/// makes one on the new server. A rebuild that failed is tried again as
/// [`Retry`] says. An idle worker does it at its next command.
///
/// Kill switch `ROOMLERD_CLIPBOARD_X11_REBUILD=0`: the worker keeps its first
/// connection, as before #3671.
#[cfg(target_os = "linux")]
mod x11_follow {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::Retry;
    use crate::virtual_desktop::XServerNow;

    /// How long the worker waits for a rebuild before it serves again with
    /// the handle it has: every session's clipboard command waits behind it,
    /// once per new X server. What a rebuild can wait for, read in arboard
    /// 3.6.1's source (`src/platform/linux/x11.rs`; the version is pinned
    /// exactly in the workspace `Cargo.toml`, and an upgrade re-reads this):
    ///
    /// - the second handle's drop: on the dead connection the hand-over to a
    ///   clipboard manager and `destroy_window` fail at their first request,
    ///   and the drop returns before its join, so nothing is waited for. When
    ///   no handle is left on any server (a try after one that failed), the
    ///   second handle is a new connection to the live server, which owns no
    ///   selection: its drop's hand-over returns at once, and the drop then
    ///   joins arboard's serving thread, one round trip for its window's
    ///   `DestroyNotify`;
    /// - the new handle: a connect, and one round trip for its atoms;
    /// - the proof ([`proves_a_live_connection`]): one round trip for the
    ///   selection's owner, a reader's connection, and ONE target asked of the
    ///   SECONDARY selection, which its owner answers within
    ///   `LONG_TIMEOUT_DUR`, 4 s, or arboard gives up (`read_single`); with no
    ///   owner the server answers at once.
    ///
    /// Worst case: 4 s and a few round trips, about 4.1 s, and 6 s leaves
    /// almost 2 s for a loaded host. (A read of the text, as the proof was
    /// before, asks six targets of 4 s each: 24 s.) A rebuild that takes
    /// longer is not given up on: [`Follow::check`].
    pub(super) const REBUILD_BOUND: Duration = Duration::from_secs(6);

    /// #3671 kill switch `ROOMLERD_CLIPBOARD_X11_REBUILD=0`.
    fn enabled() -> bool {
        tunnel_core::env::flag("CLIPBOARD_X11_REBUILD", true)
    }

    /// A test takes the second handle out of [`connect_anew`], to show that
    /// the rebuild fails without it.
    #[cfg(test)]
    pub(super) static SECOND_HANDLE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(true);

    /// What makes a handle on the desktop's server now: [`connect_anew`], or a
    /// test's stand-in.
    pub(super) type Connect<H> = std::sync::Arc<dyn Fn() -> Result<H, String> + Send + Sync>;

    /// The answer of a rebuild that has no thread to say it.
    const NO_ANSWER: &str = "the rebuild's thread ended without an answer";

    /// A rebuild the worker stopped waiting for: the generation it is for, the
    /// desktop as read before it started, and where its answer comes.
    struct Running<H> {
        generation: u64,
        desk: Option<XServerNow>,
        answer: mpsc::Receiver<Result<H, String>>,
    }

    /// Which generation the worker's handle is on, and how it makes the next.
    pub(super) struct Follow<H> {
        on: u64,
        running: Option<Running<H>>,
        retry: Retry,
        connect: Connect<H>,
        bound: Duration,
        off: bool,
    }

    impl<H: Send + 'static> Follow<H> {
        /// A worker whose handle was made with the desktop as `born` read
        /// before its connect.
        pub(super) fn new(born: Option<XServerNow>, connect: Connect<H>, bound: Duration) -> Self {
            Self {
                on: born.map_or(0, |d| d.generation),
                running: None,
                retry: Retry::new(),
                connect,
                bound,
                off: !enabled(),
            }
        }

        /// A handle on the desktop's new X server, once the desktop is up on a
        /// generation newer than the handle's. `desk` is the desktop now
        /// ([`crate::virtual_desktop::x_server_now`]).
        ///
        /// - Only a NEWER generation: after the teardown at a stop the desktop
        ///   reads `None`, which is no new server, and nothing is tried.
        /// - Made on a helper thread the worker waits `bound`
        ///   ([`REBUILD_BOUND`]) for, so a rebuild that hangs cannot take the
        ///   worker, whose answers every session waits for, with it.
        /// - At most ONE helper at a time. One the worker stopped waiting for
        ///   is asked again at each look, and its answer is taken then; until
        ///   it ends no other starts. A helper that hangs holds arboard's
        ///   global lock (its `Clipboard::new` and `Drop` hold it throughout),
        ///   which every other would queue on, one thread more each time. So
        ///   following ends for good only if a helper never ends, which only a
        ///   teardown or a server that hangs inside arboard can do.
        /// - A rebuild that failed is tried again as [`Retry`] says; the worker
        ///   serves with the handle it has meanwhile, dead as before #3671.
        ///   Said once per generation, and once when it follows.
        pub(super) fn check(&mut self, desk: Option<XServerNow>, now: Instant) -> Option<H> {
            if self.off {
                return None;
            }
            if let Some(running) = self.running.take() {
                let answer = match running.answer.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => {
                        self.running = Some(running);
                        return None;
                    }
                    Ok(answer) => answer,
                    Err(mpsc::TryRecvError::Disconnected) => Err(NO_ANSWER.into()),
                };
                if let Some(fresh) =
                    self.answered(running.generation, running.desk, desk, now, None, answer)
                {
                    return Some(fresh);
                }
            }
            let d = desk?;
            if d.down.is_some() || d.generation <= self.on || !self.retry.may_try(desk, now) {
                return None;
            }
            let (tx, rx) = mpsc::channel();
            let connect = self.connect.clone();
            let started = Instant::now();
            let spawned = std::thread::Builder::new()
                .name("roomlerd-clipboard-follow".into())
                .spawn(move || {
                    let _ = tx.send(connect());
                });
            if let Err(e) = spawned {
                let why = Err(format!("no thread for the rebuild: {e}"));
                return self.answered(d.generation, desk, desk, now, Some(started), why);
            }
            match rx.recv_timeout(self.bound) {
                Ok(answer) => self.answered(d.generation, desk, desk, now, Some(started), answer),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        generation = d.generation,
                        bound_ms = self.bound.as_millis() as u64,
                        "clipboard: following the desktop's new X server takes longer than bound_ms — the clipboard keeps its old connection until it ends, and copy and paste fail meanwhile (#3671)"
                    );
                    self.running = Some(Running {
                        generation: d.generation,
                        desk,
                        answer: rx,
                    });
                    None
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let why = Err(NO_ANSWER.into());
                    self.answered(d.generation, desk, desk, now, Some(started), why)
                }
            }
        }

        /// A rebuild's `answer`, for `generation`, started with the desktop
        /// as `was` read and looked at with the desktop as `desk` reads;
        /// `started` when it is the look's own rebuild.
        fn answered(
            &mut self,
            generation: u64,
            was: Option<XServerNow>,
            desk: Option<XServerNow>,
            now: Instant,
            started: Option<Instant>,
            answer: Result<H, String>,
        ) -> Option<H> {
            let current = desk.is_some_and(|d| d.down.is_none() && d.generation == generation);
            match answer {
                Ok(fresh) if current => {
                    self.on = generation;
                    let after_failure = self.retry.succeeded();
                    tracing::info!(
                        generation,
                        took_ms = started.map(|s| s.elapsed().as_millis() as u64),
                        after_failure,
                        "clipboard: the clipboard follows the desktop's new X server (#3671)"
                    );
                    Some(fresh)
                }
                // A late answer whose server went meanwhile: nothing to take.
                Ok(stale) => {
                    super::quietly_drop(stale);
                    None
                }
                Err(why) => {
                    if self.retry.failed(was, now) {
                        tracing::warn!(
                            generation,
                            %why,
                            retry_s = Retry::EVERY.as_secs(),
                            "clipboard: the clipboard could not follow the desktop's new X server — tried again every retry_s while that server answers, and copy and paste fail until then (#3671)"
                        );
                    } else {
                        tracing::debug!(generation, %why, "clipboard: following the desktop's new X server failed again");
                    }
                    None
                }
            }
        }
    }

    /// A handle on `$DISPLAY`'s server now, while the worker's own handle is
    /// still the dead server's.
    ///
    /// ⚠️ The second handle is load-bearing: arboard's rule, as of arboard
    /// 3.6.1 (pinned exactly in the workspace `Cargo.toml`; an upgrade
    /// re-reads it), `src/platform/linux/x11.rs`. A process has one X
    /// connection, held in a global for as long as any handle lives:
    /// `Clipboard::new` (lines 915-933) hands that global back, dead or not.
    /// A handle's drop tears the global down only when the handle is one of
    /// exactly three owners (`impl Drop for Clipboard`, lines 1082-1130,
    /// `MIN_OWNERS = 3`: the global, the thread that serves the selection, and
    /// the handle). Once the server is gone, that thread has ended (arboard
    /// logs "Worker thread errored with: …"), and the worker's handle and the
    /// global are the only two owners: dropping the worker's handle tears
    /// nothing down, and a new one is the dead global again. A second handle
    /// makes three: its drop tears the global down (quietly,
    /// [`super::quietly_drop`]: it returns at `destroy_window`, before its
    /// join), and the next `Clipboard::new` connects to the new server. The
    /// worker's old handle, dropped after, is then the only owner of the dead
    /// connection and tears nothing down. A read on the new handle proves it
    /// before the worker serves with it.
    pub(super) fn connect_anew() -> Result<arboard::Clipboard, String> {
        use arboard::GetExtLinux as _;
        #[cfg(test)]
        let second_handle = SECOND_HANDLE.load(std::sync::atomic::Ordering::SeqCst);
        #[cfg(not(test))]
        let second_handle = true;
        if second_handle {
            let second = arboard::Clipboard::new()
                .map_err(|e| format!("a second handle on the old server: {e}"))?;
            super::quietly_drop(second);
        }
        let mut fresh =
            arboard::Clipboard::new().map_err(|e| format!("a handle on the new server: {e}"))?;
        let proof = fresh
            .get()
            .clipboard(arboard::LinuxClipboardKind::Secondary)
            .html();
        if proves_a_live_connection(&proof) {
            return Ok(fresh);
        }
        let why = match proof {
            Err(e) => format!("the new handle's first read failed: {e}"),
            Ok(_) => String::new(),
        };
        super::quietly_drop(fresh);
        Err(why)
    }

    /// Whether the proof's answer came over a live connection. Any answer at
    /// all does: something, nothing to give, a conversion that failed, an
    /// occupied clipboard. `Unknown` is arboard 3.6.1's word for an X request
    /// or connection that failed (`into_unknown`, `src/platform/linux/mod.rs`),
    /// which is what the dead server's connection gives at its first request,
    /// the question of who owns the selection.
    pub(super) fn proves_a_live_connection(answer: &Result<String, arboard::Error>) -> bool {
        !matches!(answer, Err(arboard::Error::Unknown { .. }))
    }
}

/// FR-97 P3e-3a — the X11 clipboard's change gate, Windows' sequence number
/// for an X11 session: XFixes reports every change of the CLIPBOARD
/// selection's owner, and a copy is exactly that. A tick that saw no event
/// reads nothing, so copied images can be watched (each read is a full
/// transfer and a PNG encode, which a blind 1 Hz poll could not afford) and
/// text no longer costs a read and a hash every second.
///
/// - **Only an X11 session**: an X display and no Wayland one. Under Wayland
///   the clipboard is the compositor's, and Xwayland's copy of it is
///   unmeasured, so images stay unwatched there, as before.
/// - **Our own writes** change the owner too; the content hashes in
///   [`SelfMarks`] still swallow them, as on every OS.
/// - A connection that breaks drops the gate, and the watcher falls back to
///   reading text every tick and leaving images alone.
///
/// Kill switch `ROOMLERD_CLIPBOARD_X11_WATCH=0`: no gate, as before P3e-3a.
#[cfg(target_os = "linux")]
pub(crate) mod x11_gate {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::Event;
    use x11rb::protocol::xfixes::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::rust_connection::RustConnection;

    /// FR-97 P3e-3a — kill switch `ROOMLERD_CLIPBOARD_X11_WATCH=0`.
    pub fn enabled() -> bool {
        tunnel_core::env::flag("CLIPBOARD_X11_WATCH", true)
    }

    fn x11_session() -> bool {
        std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
            && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|w| w.is_empty())
    }

    pub struct Gate {
        conn: RustConnection,
        clipboard: xproto::Atom,
        seq: u32,
    }

    impl Gate {
        /// `None` outside an X11 session, with the switch off, or without
        /// XFixes.
        pub fn open() -> Option<Self> {
            if !enabled() || !x11_session() {
                return None;
            }
            Self::connect()
        }

        /// The gate on `$DISPLAY`'s server, whatever session this is: what
        /// [`Gate::open`] does once it decided to (and what the real-X test
        /// drives, since a WSLg shell names a Wayland display too).
        pub(super) fn connect() -> Option<Self> {
            let (conn, screen) = x11rb::connect(None)
                .map_err(|e| tracing::info!(%e, "clipboard: no X connection for the change gate"))
                .ok()?;
            let root = conn.setup().roots.get(screen)?.root;
            conn.xfixes_query_version(5, 0).ok()?.reply().ok()?;
            let clipboard = conn
                .intern_atom(false, b"CLIPBOARD")
                .ok()?
                .reply()
                .ok()?
                .atom;
            conn.xfixes_select_selection_input(
                root,
                clipboard,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER
                    | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .ok()?
            .check()
            .ok()?;
            Some(Self {
                conn,
                clipboard,
                // Never 0: the watcher reads 0 as "no counter".
                seq: 1,
            })
        }

        /// The change counter, one more for every owner change since the
        /// last call. `None` once the connection broke.
        pub fn seq(&mut self) -> Option<u32> {
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(Event::XfixesSelectionNotify(e))) if e.selection == self.clipboard => {
                        self.seq = self.seq.wrapping_add(1).max(1);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => return Some(self.seq),
                    Err(e) => {
                        tracing::info!(%e, "clipboard: the change gate's X connection broke");
                        return None;
                    }
                }
            }
        }
    }

    /// Whether copied images on this host are watched: an X11 session whose
    /// server takes the gate. Asked for the hello. #3671 — on a host whose
    /// desktop is the daemon's own, a `false` met while the desktop was down
    /// is asked again once it is up, as [`super::Retry`] says, and a host
    /// with no desktop of its own decides once, as before. The hello keeps
    /// the copy its probe took (`encode::caps`).
    pub fn available() -> bool {
        static AVAILABLE: super::Kept<()> = super::Kept::new();
        available_at(
            &AVAILABLE,
            crate::virtual_desktop::x_server_now(),
            std::time::Instant::now(),
            || Gate::open().is_some(),
        )
    }

    /// [`available`], with its slot, the desktop, the time and whether the
    /// gate opens given: what its tests drive.
    pub(super) fn available_at(
        slot: &super::Kept<()>,
        desk: Option<crate::virtual_desktop::XServerNow>,
        now: std::time::Instant,
        opens: impl FnOnce() -> bool,
    ) -> bool {
        slot.get_at(desk, now, || if opens() { Ok(()) } else { Err(()) })
            .0
            .is_some()
    }
}

/// FR-97 P2e-1 — a Mac's clipboard change gate: `NSPasteboard`'s change count,
/// which the pasteboard server moves on every write by any app (Windows'
/// sequence number in another form). An unchanged tick reads the count and
/// nothing else, so copied images can be watched, and text is read when it
/// changed rather than every second.
///
/// - **Fewer reads of the person's copy.** Without the gate a session read the
///   general pasteboard's text and html every second (measured on macOS 27,
///   FR-97 §8). Apple's pasteboard privacy may ask the person before an app
///   reads what another app copied; a read the gate skips cannot ask.
/// - **Our own writes** move the count too; the content hashes in
///   [`SelfMarks`] still swallow them, as on every OS.
/// - **Outside the console session** (a root daemon) the pasteboard server
///   cannot be reached at all, so the count never moves and the gate reads
///   nothing, where every read would have failed anyway.
///
/// Kill switch `ROOMLERD_CLIPBOARD_MAC_WATCH=0`: no gate, as before P2e-1.
#[cfg(target_os = "macos")]
pub(crate) mod mac_gate {
    use objc2_app_kit::NSPasteboard;

    /// FR-97 P2e-1 — kill switch `ROOMLERD_CLIPBOARD_MAC_WATCH=0`.
    pub fn enabled() -> bool {
        tunnel_core::env::flag("CLIPBOARD_MAC_WATCH", true)
    }

    pub struct Gate {
        last: isize,
        seq: u32,
    }

    impl Gate {
        /// `None` with the switch off.
        pub fn open() -> Option<Self> {
            enabled().then(Self::connect)
        }

        /// The gate, whatever the switch says: what [`Gate::open`] does once
        /// it decided to (and what the real-pasteboard test drives).
        pub(super) fn connect() -> Self {
            Self {
                last: change_count(),
                // Never 0: the watcher reads 0 as "no counter".
                seq: 1,
            }
        }

        /// The change counter, one more whenever the change count moved since
        /// the last call. Always `Some`: the count can always be asked.
        pub fn seq(&mut self) -> Option<u32> {
            let now = change_count();
            if now != self.last {
                self.last = now;
                self.seq = self.seq.wrapping_add(1).max(1);
            }
            Some(self.seq)
        }
    }

    fn change_count() -> isize {
        // The clipboard worker is not the main thread, so it drains AppKit's
        // autoreleased temporaries itself, as arboard does around its reads.
        objc2::rc::autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount())
    }

    /// Whether copied images on this host are watched: wherever the switch is
    /// on. Not asked of this process's own pasteboard: on a supervised Mac the
    /// root daemon says the hello, and a delegated session's clipboard runs in
    /// the GUI worker, inside the console session.
    pub fn available() -> bool {
        enabled()
    }
}

/// FNV-1a 64 over raw bytes. Mirrors the browser-side
/// `hashClipboardText` / `hashClipboardBytes` helpers in
/// `useRemoteControl.ts` — both sides hash identical canonical bytes,
/// which is what makes cross-side echo suppression sound.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn worker_read_text(cb: &mut arboard::Clipboard) -> Result<String> {
    match cb.get_text() {
        // Canonicalize to wire form (LF) so browser-side hashing and
        // display are host-convention-free.
        Ok(t) => Ok(host_to_wire(&t)),
        // Clipboard holds no text (image / files / empty). Not an
        // error on the wire: an empty `clipboard:content` lets a rich
        // read fall through to the image path, and old UIs surface
        // "empty clipboard" instead of a scary error toast.
        Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
        Err(e) => Err(anyhow::anyhow!("clipboard get_text: {e}")),
    }
}

fn worker_write_text(
    cb: &mut arboard::Clipboard,
    wire_text: &str,
    marks: &mut SelfMarks,
) -> Result<()> {
    // Hash the canonical form BEFORE host conversion — the watcher
    // hashes `host_to_wire(get_text())`, which round-trips back to
    // exactly these bytes.
    let canonical_hash = fnv1a64(host_to_wire(wire_text).as_bytes());
    cb.set_text(wire_to_host(wire_text))
        .map_err(|e| anyhow::anyhow!("clipboard set_text: {e}"))?;
    marks.text_hash = canonical_hash;
    #[cfg(windows)]
    {
        marks.seq = win_clipboard_seq().unwrap_or(0);
    }
    Ok(())
}

fn worker_read_image(cb: &mut arboard::Clipboard) -> Result<Option<PngImage>> {
    let img = match cb.get_image() {
        Ok(i) => i,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("clipboard get_image: {e}")),
    };
    let (w, h) = (img.width as u32, img.height as u32);
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        anyhow::bail!("clipboard image {w}x{h} exceeds the {MAX_IMAGE_PIXELS}-pixel cap");
    }
    let png = rgba_to_png(w, h, &img.bytes)?;
    if png.len() > CLIPBOARD_IMAGE_MAX_BYTES {
        anyhow::bail!(
            "clipboard image PNG is {} bytes (cap {CLIPBOARD_IMAGE_MAX_BYTES})",
            png.len()
        );
    }
    Ok(Some(PngImage { w, h, png }))
}

fn worker_write_image(
    cb: &mut arboard::Clipboard,
    png_bytes: &[u8],
    marks: &mut SelfMarks,
) -> Result<()> {
    let (w, h, rgba) = png_to_rgba(png_bytes)?;
    cb.set_image(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Borrowed(&rgba),
    })
    .map_err(|e| anyhow::anyhow!("clipboard set_image: {e}"))?;
    // Echo mark: hash our own deterministic re-encode of the pixels we
    // just wrote — matches what watch_tick would produce when it reads
    // this image back. (The inbound browser PNG hashes differently —
    // different encoder.) Belt-and-suspenders next to the seq mark:
    // catches the coalesced-seq race where another app also touched
    // the clipboard between our write and the next tick.
    if let Ok(reenc) = rgba_to_png(w, h, &rgba) {
        marks.img_hash = fnv1a64(&reenc);
    }
    #[cfg(windows)]
    {
        marks.seq = win_clipboard_seq().unwrap_or(0);
    }
    Ok(())
}

/// v2.1 — combined dedup hash for an html+text clipboard state. Both
/// the self-marks (post-write read-back) and the watcher tick hash the
/// SAME reading of the same clipboard, so echo suppression holds even
/// though the OS re-wraps CF_HTML on every write. The 0x1F separator
/// prevents (html="ab", text="c") colliding with (html="a", text="bc").
pub(crate) fn html_event_hash(html: &str, text: &str) -> u64 {
    let mut bytes = Vec::with_capacity(html.len() + text.len() + 1);
    bytes.extend_from_slice(html.as_bytes());
    bytes.push(0x1f);
    bytes.extend_from_slice(host_to_wire(text).as_bytes());
    fnv1a64(&bytes)
}

fn worker_read_html(cb: &mut arboard::Clipboard) -> Result<Option<HtmlPayload>> {
    let html = match cb.get().html() {
        Ok(h) => h,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("clipboard get html: {e}")),
    };
    if html.is_empty() || html.len() > CLIPBOARD_HTML_MAX_BYTES {
        return Ok(None);
    }
    // The plain-text alternate rides along so the receiving side can
    // write both formats. Missing text (html-only producers) → empty.
    let text = match cb.get_text() {
        Ok(t) => host_to_wire(&t),
        Err(_) => String::new(),
    };
    Ok(Some(HtmlPayload { html, text }))
}

fn worker_write_html(
    cb: &mut arboard::Clipboard,
    html: &str,
    wire_text: &str,
    marks: &mut SelfMarks,
) -> Result<()> {
    let text_host = wire_to_host(wire_text);
    let alt = if text_host.is_empty() {
        None
    } else {
        Some(text_host.as_str())
    };
    // One clipboard transaction, both formats (arboard wraps the
    // CF_HTML header on Windows; text/html target on X11; NSPasteboard
    // html type on macOS).
    cb.set_html(html, alt)
        .map_err(|e| anyhow::anyhow!("clipboard set_html: {e}"))?;
    marks.text_hash = fnv1a64(host_to_wire(wire_text).as_bytes());
    // Echo mark from the READ-BACK — the OS re-wraps CF_HTML, so a
    // later tick reads different bytes than our input; hash what the
    // tick will actually see. Read-back failure → mark from the input
    // (better than nothing; the Windows seq mark still guards).
    marks.html_hash = match cb.get().html() {
        Ok(back) => html_event_hash(&back, wire_text),
        Err(_) => html_event_hash(html, wire_text),
    };
    #[cfg(windows)]
    {
        marks.seq = win_clipboard_seq().unwrap_or(0);
    }
    Ok(())
}

/// v2.2 — Windows RTF access via clipboard-win's raw API (arboard has
/// no custom-format surface). The "Rich Text Format" registered id is
/// process-stable; the open guard retries briefly against other apps
/// holding the clipboard.
#[cfg(windows)]
fn read_rtf_raw() -> Option<Vec<u8>> {
    let fmt = clipboard_win::register_format("Rich Text Format")?;
    let _guard = clipboard_win::Clipboard::new_attempts(10).ok()?;
    let mut out = Vec::new();
    match clipboard_win::raw::get_vec(fmt.get(), &mut out) {
        Ok(_) if !out.is_empty() && out.len() <= CLIPBOARD_NATIVE_MAX_BYTES => Some(out),
        _ => None,
    }
}

/// Append RTF to the CURRENT clipboard contents (no clear — the
/// html/text formats written moments earlier stay). Second transaction
/// after the arboard write; the tiny between-open race is acceptable
/// (worst case: a concurrent copy wins, which is the right outcome).
#[cfg(windows)]
fn append_rtf_raw(rtf: &[u8]) -> Result<()> {
    let fmt = clipboard_win::register_format("Rich Text Format")
        .ok_or_else(|| anyhow::anyhow!("RegisterClipboardFormat failed"))?;
    let _guard = clipboard_win::Clipboard::new_attempts(10)
        .map_err(|e| anyhow::anyhow!("open clipboard for RTF append: {e}"))?;
    clipboard_win::raw::set_without_clear(fmt.get(), rtf)
        .map_err(|e| anyhow::anyhow!("set RTF: {e}"))
}

fn worker_read_native(cb: &mut arboard::Clipboard) -> Result<Option<NativePayload>> {
    #[cfg(windows)]
    {
        let Some(rtf) = read_rtf_raw() else {
            return Ok(None);
        };
        let html = cb.get().html().unwrap_or_default();
        let text = cb.get_text().map(|t| host_to_wire(&t)).unwrap_or_default();
        if rtf.len() + html.len() + text.len() > CLIPBOARD_NATIVE_MAX_BYTES {
            return Ok(None);
        }
        Ok(Some(NativePayload { rtf, html, text }))
    }
    #[cfg(not(windows))]
    {
        let _ = cb;
        Ok(None)
    }
}

fn worker_write_native(
    cb: &mut arboard::Clipboard,
    payload: &NativePayload,
    marks: &mut SelfMarks,
) -> Result<()> {
    #[cfg(windows)]
    {
        if payload.rtf.is_empty() {
            anyhow::bail!("native write without RTF — use the html/text lanes");
        }
        // html+text first (one arboard transaction, clears the board),
        // then append the RTF format.
        if !payload.html.is_empty() {
            worker_write_html(cb, &payload.html, &payload.text, marks)?;
        } else {
            worker_write_text(cb, &payload.text, marks)?;
        }
        append_rtf_raw(&payload.rtf)?;
        marks.native_hash = fnv1a64(&payload.rtf);
        marks.seq = win_clipboard_seq().unwrap_or(0);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (cb, payload, marks);
        anyhow::bail!("native clipboard formats are Windows-only")
    }
}

/// Build the initial watcher state: snapshot the CURRENT clipboard so
/// pre-existing content is never pushed on subscribe — only changes
/// from here on.
fn install_watcher(
    cb: &mut arboard::Clipboard,
    events: WatchEvents,
    tx: tokio::sync::mpsc::Sender<ClipboardChange>,
    gate_seq: Option<u32>,
) -> WatcherState {
    let asked_image = events.image;
    // Image watching needs the Windows sequence-number gate to bound
    // `get_image` calls; without it (a window station that denies
    // clipboard access) fall back to text-only watching. FR-97 P3e-3a —
    // on Linux, the X11 change gate (`gate_seq`, `None` without one); P2e-1,
    // a Mac's change-count gate.
    #[cfg(windows)]
    let events = if events.image && win_clipboard_seq().is_none() {
        tracing::warn!("clipboard: no sequence-number access — image watching disabled");
        WatchEvents {
            image: false,
            ..events
        }
    } else {
        events
    };
    #[cfg(windows)]
    let _ = gate_seq;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let events = WatchEvents {
        image: events.image && gate_seq.is_some(),
        native: false,
        ..events
    };
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let events = {
        let _ = gate_seq;
        WatchEvents {
            image: false,
            native: false,
            ..events
        }
    };
    let text_hash = match cb.get_text() {
        Ok(t) => fnv1a64(host_to_wire(&t).as_bytes()),
        Err(_) => 0,
    };
    // Snapshot current html too — otherwise the first tick after
    // subscribe would push pre-existing rich content.
    let html_hash = match cb.get().html() {
        Ok(h) => {
            let text = cb.get_text().map(|t| host_to_wire(&t)).unwrap_or_default();
            html_event_hash(&h, &text)
        }
        Err(_) => 0,
    };
    // Snapshot current RTF too (Windows) so pre-existing native
    // content isn't pushed on subscribe.
    #[cfg(windows)]
    let native_hash = read_rtf_raw().map(|r| fnv1a64(&r)).unwrap_or(0);
    #[cfg(not(windows))]
    let native_hash = 0;
    WatcherState {
        events,
        tx,
        #[cfg(windows)]
        last_seen_seq: win_clipboard_seq().unwrap_or(0),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        last_seen_seq: gate_seq.unwrap_or(0),
        last_text_hash: text_hash,
        last_img_hash: 0,
        last_html_hash: html_hash,
        last_native_hash: native_hash,
        asked_image,
        retry: false,
    }
}

/// One watcher tick. Cheap when nothing changed (Windows: one
/// syscall; elsewhere: one text read + hash). Text wins over image
/// when both are present.
fn watch_tick(
    cb: &mut arboard::Clipboard,
    w: &mut WatcherState,
    marks: &SelfMarks,
    gate_seq: Option<u32>,
) {
    #[cfg(windows)]
    {
        let _ = gate_seq;
        if let Some(seq) = win_clipboard_seq() {
            if !w.retry && seq == w.last_seen_seq {
                return;
            }
            w.last_seen_seq = seq;
            if seq != 0 && seq == marks.seq {
                // The change was our own write — swallow.
                w.retry = false;
                return;
            }
        }
        // Sequence number unavailable → hash polling below still works.
    }
    // FR-97 P3e-3a / P2e-1 — the change gate (X11's owner events, a Mac's
    // change count): no change, nothing read. Our own writes pass it and are
    // swallowed by the hashes below.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match gate_seq {
        Some(seq) => {
            if !w.retry && seq == w.last_seen_seq {
                return;
            }
            w.last_seen_seq = seq;
        }
        // No gate, or it broke: text is read every tick, images never.
        None => w.events.image = false,
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let _ = gate_seq;
    w.retry = false;
    // v2.2 — richest format wins, native (RTF) on top: only RTF
    // carries a document's EMBEDDED images. Below it: html (carries a
    // text alt) beats plain text beats image.
    #[cfg(windows)]
    if w.events.native
        && let Some(rtf) = read_rtf_raw()
    {
        let h = fnv1a64(&rtf);
        if h == w.last_native_hash || h == marks.native_hash {
            w.last_native_hash = h;
            return;
        }
        let html = cb.get().html().unwrap_or_default();
        let text = cb.get_text().map(|t| host_to_wire(&t)).unwrap_or_default();
        if rtf.len() + html.len() + text.len() > CLIPBOARD_NATIVE_MAX_BYTES {
            w.last_native_hash = h;
            return;
        }
        match w
            .tx
            .try_send(ClipboardChange::Native(Box::new(NativePayload {
                rtf,
                html,
                text,
            }))) {
            Ok(()) => w.last_native_hash = h,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => w.retry = true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
        return;
    }
    // v2.1 — html (carries a text alt) beats plain text beats image.
    if w.events.html
        && let Ok(html) = cb.get().html()
        && !html.is_empty()
        && html.len() <= CLIPBOARD_HTML_MAX_BYTES
    {
        let text = cb.get_text().map(|t| host_to_wire(&t)).unwrap_or_default();
        let h = html_event_hash(&html, &text);
        if h == w.last_html_hash || h == marks.html_hash {
            w.last_html_hash = h;
            return;
        }
        match w
            .tx
            .try_send(ClipboardChange::Html(HtmlPayload { html, text }))
        {
            Ok(()) => w.last_html_hash = h,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => w.retry = true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
        return;
    }
    let text = match cb.get_text() {
        Ok(t) => host_to_wire(&t),
        Err(arboard::Error::ContentNotAvailable) => String::new(),
        Err(_) => {
            // Transient (another process holds the clipboard open) —
            // re-attempt next tick even if the seq doesn't move again.
            w.retry = true;
            return;
        }
    };
    if !text.is_empty() {
        if !w.events.text {
            return;
        }
        let h = fnv1a64(text.as_bytes());
        if h == w.last_text_hash || h == marks.text_hash {
            w.last_text_hash = h;
            return;
        }
        if text.len() > MAX_CLIPBOARD_BYTES {
            // Too big to auto-push; remember it so we don't re-read
            // every tick. Explicit reads still serve it (chunked).
            w.last_text_hash = h;
            return;
        }
        match w.tx.try_send(ClipboardChange::Text(text)) {
            Ok(()) => w.last_text_hash = h,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => w.retry = true,
            // Forwarder task gone — an Unwatch is on its way; stay quiet.
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
        // Load-bearing on Windows, Linux and macOS, where the image block
        // below follows; redundant on any other OS, where it is the last
        // statement of the function. Scoped rather than removed — deleting it
        // would fall through into the image path.
        #[cfg_attr(
            not(any(windows, target_os = "linux", target_os = "macos")),
            allow(clippy::needless_return)
        )]
        return;
    }
    // Empty text — maybe an image. Only behind a change gate (Windows'
    // sequence number; FR-97 P3e-3a, the X11 gate; P2e-1, a Mac's change
    // count), which bounds how often we pay for a full image read + PNG encode.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    if w.events.image {
        let img = match cb.get_image() {
            Ok(i) => i,
            Err(_) => return,
        };
        let (iw, ih) = (img.width as u32, img.height as u32);
        if u64::from(iw) * u64::from(ih) > MAX_IMAGE_PIXELS {
            return;
        }
        let Ok(png) = rgba_to_png(iw, ih, &img.bytes) else {
            return;
        };
        if png.len() > CLIPBOARD_IMAGE_MAX_BYTES {
            return;
        }
        let h = fnv1a64(&png);
        if h == w.last_img_hash || h == marks.img_hash {
            w.last_img_hash = h;
            return;
        }
        match w
            .tx
            .try_send(ClipboardChange::Image(PngImage { w: iw, h: ih, png }))
        {
            Ok(()) => w.last_img_hash = h,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => w.retry = true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

// ── Line-ending canonicalization (clipboard protocol v2) ────────────────────
//
// The wire format is canonical LF. The browser normalizes CRLF/CR → LF
// before sending and before hashing (its echo-suppression gate compares
// content hashes); the agent mirrors that on every OS-clipboard
// boundary. Without this, LF-only multiline text written verbatim into
// the Win32 clipboard violates the CF_UNICODETEXT CRLF convention —
// classic edit controls, cmd.exe and several editors mis-render it
// (lines run together / split oddly), which presented in the field as
// "pasted lines in the wrong order". It also makes hash-based echo
// suppression sound: both sides hash the same canonical bytes.

/// Canonicalize host-clipboard text to wire form: `\r\n` → `\n`,
/// lone `\r` → `\n`. Applied to every `get_text` result before it is
/// sent, hashed or compared.
pub(crate) fn host_to_wire(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Expand canonical LF to CRLF for the Windows clipboard. Idempotent —
/// input is canonicalized first, so pre-existing `\r\n` sequences come
/// out as exactly one `\r\n`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn lf_to_crlf(text: &str) -> String {
    host_to_wire(text).replace('\n', "\r\n")
}

/// Convert wire text (canonical LF) to the host's clipboard line-ending
/// convention before `set_text`: CRLF on Windows, canonical LF elsewhere.
pub(crate) fn wire_to_host(text: &str) -> String {
    #[cfg(windows)]
    {
        lf_to_crlf(text)
    }
    #[cfg(not(windows))]
    {
        host_to_wire(text)
    }
}

/// Incoming clipboard DC message shape. Parsed from the JSON payload
/// the browser sends; the handler in `peer.rs` dispatches on the `t`
/// discriminator.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "t")]
pub(crate) enum ClipboardIncoming {
    #[serde(rename = "clipboard:write")]
    Write {
        text: String,
        /// v2 — optional browser-assigned write id. When present the
        /// agent replies `clipboard:write-ack { id, bytes }` after the
        /// OS clipboard write succeeds (the browser gates its deferred
        /// Ctrl+V keystroke on that ack). Old browsers omit it → no
        /// ack, exact v1 behavior.
        #[serde(default)]
        id: Option<String>,
    },
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
        /// v2 — content kinds the browser can handle in the reply.
        /// `["text","image"]` lets the agent answer an image-holding
        /// clipboard with `clipboard:img-begin` + binary frames.
        /// Empty (old browsers) means text-only replies.
        #[serde(default)]
        accept: Vec<String>,
    },
    /// v2 — start pushing unsolicited `clipboard:event` /
    /// `clipboard:img-begin` messages when the HOST clipboard changes.
    /// `events` lists the kinds the browser wants (`"text"`,
    /// `"image"`); image watching is honoured on Windows only.
    #[serde(rename = "clipboard:subscribe")]
    Subscribe {
        #[serde(default)]
        events: Vec<String>,
    },
    /// v2 — stop pushing change events (toggle flipped off). Also
    /// implied by the DC closing.
    #[serde(rename = "clipboard:unsubscribe")]
    Unsubscribe,
    /// v2 — image write header. Binary frames totalling `bytes` bytes
    /// of PNG follow on the same DC, terminated by `clipboard:img-end`
    /// with the matching `id`.
    #[serde(rename = "clipboard:img-begin")]
    ImgBegin {
        id: String,
        w: u32,
        h: u32,
        bytes: u64,
        #[serde(default)]
        format: String,
    },
    /// v2 — image write trailer; the agent decodes the accumulated PNG
    /// and writes it to the OS clipboard, then acks with the `id`.
    #[serde(rename = "clipboard:img-end")]
    ImgEnd { id: String },
    /// v2.1 — HTML write header. Binary frames totalling
    /// `html_bytes + text_bytes` follow (html UTF-8 first, then the
    /// plain-text alt), terminated by `clipboard:html-end`.
    #[serde(rename = "clipboard:html-begin")]
    HtmlBegin {
        id: String,
        html_bytes: u64,
        text_bytes: u64,
    },
    /// v2.1 — HTML write trailer; the agent splits the accumulated
    /// bytes, writes html+text atomically, then acks with the `id`.
    #[serde(rename = "clipboard:html-end")]
    HtmlEnd { id: String },
    /// v2.2 — NATIVE write header. Binary frames totalling
    /// `rtf_bytes + html_bytes + text_bytes` follow (rtf, then html
    /// UTF-8, then text UTF-8), terminated by `clipboard:native-end`.
    #[serde(rename = "clipboard:native-begin")]
    NativeBegin {
        id: String,
        rtf_bytes: u64,
        html_bytes: u64,
        text_bytes: u64,
    },
    /// v2.2 — NATIVE write trailer; the agent splits and writes
    /// RTF + html + text, then acks with the `id`.
    #[serde(rename = "clipboard:native-end")]
    NativeEnd { id: String },
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
/// Hard ceiling on a PNG image payload on the clipboard DC, both
/// directions. Above this the agent refuses inbound transfers and the
/// watcher skips auto-pushing (explicit reads get an error). 8 MiB
/// comfortably covers 4K screenshots (typically 1–4 MiB as PNG) while
/// bounding memory and DC head-of-line blocking.
pub const CLIPBOARD_IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Decompression-bomb guard: reject images whose HEADER declares more
/// pixels than a 4096×4096 canvas before allocating the RGBA buffer
/// (that's already 64 MiB at 4 B/px; a crafted 20000×20000 PNG would
/// otherwise balloon to 1.6 GB from a few KB on the wire).
pub const MAX_IMAGE_PIXELS: u64 = 4096 * 4096;

/// v2.1 — cap on an HTML clipboard payload (html + text alt combined)
/// on the wire, both directions. Chrome's sanitized copies inline
/// styles aggressively (a big table can reach hundreds of KB); 4 MiB
/// gives generous headroom while bounding memory + DC head-of-line.
pub const CLIPBOARD_HTML_MAX_BYTES: usize = 4 * 1024 * 1024;

/// v2.2 — cap on a NATIVE clipboard payload (RTF + html + text
/// combined) on the wire and through the loopback bridge. RTF embeds
/// images as hex (≈2× the binary size), so a Word selection with a
/// few pictures runs to megabytes; 16 MiB bounds memory + DC
/// head-of-line while covering real documents.
pub const CLIPBOARD_NATIVE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Binary frame size for agent → browser image sends. 64 KiB matches
/// the folder-zip download pump (field-proven; Chrome's inbound SCTP
/// message cap is 256 KiB). The browser → agent direction uses 16 KiB
/// frames instead — webrtc-rs's inbound `max_message_size` is 65536
/// and 64 KiB frames sat exactly on that boundary (see the files-DC
/// pump comment in `useRemoteControl.ts`).
pub(crate) const IMG_FRAME_BYTES_TX: usize = 64 * 1024;

/// Encode raw RGBA8 pixels to PNG (the clipboard-DC wire format for
/// images). Runs on the clipboard worker thread — blocking there is
/// fine.
pub(crate) fn rgba_to_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let expected = (w as usize)
        .checked_mul(h as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("image dimensions overflow"))?;
    if rgba.len() != expected {
        anyhow::bail!(
            "rgba buffer is {} bytes, expected {expected} for {w}x{h}",
            rgba.len()
        );
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().context("png write_header")?;
        writer
            .write_image_data(rgba)
            .context("png write_image_data")?;
    }
    Ok(out)
}

/// Decode a PNG into RGBA8. Enforces [`MAX_IMAGE_PIXELS`] from the
/// header BEFORE allocating the pixel buffer (bomb guard). Non-RGBA
/// color types are expanded (palette → RGB, 16-bit → 8-bit) and
/// padded to RGBA with opaque alpha.
pub(crate) fn png_to_rgba(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("png read_info")?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 {
        anyhow::bail!("png has a zero dimension");
    }
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        anyhow::bail!("png {w}x{h} exceeds the {MAX_IMAGE_PIXELS}-pixel cap");
    }
    let out_size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow::anyhow!("png output size overflows"))?;
    let mut buf = vec![0u8; out_size];
    let out = reader.next_frame(&mut buf).context("png next_frame")?;
    buf.truncate(out.buffer_size());
    let rgba = match out.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut v = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                v.extend_from_slice(px);
                v.push(0xff);
            }
            v
        }
        png::ColorType::Grayscale => {
            let mut v = Vec::with_capacity(buf.len() * 4);
            for &g in &buf {
                v.extend_from_slice(&[g, g, g, 0xff]);
            }
            v
        }
        png::ColorType::GrayscaleAlpha => {
            let mut v = Vec::with_capacity(buf.len() * 2);
            for px in buf.chunks_exact(2) {
                v.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            v
        }
        other => anyhow::bail!("unsupported png color type after expand: {other:?}"),
    };
    Ok((w, h, rgba))
}

/// Reassembles an inbound browser → agent RICH transfer (image OR
/// html): a `clipboard:img-begin`/`clipboard:html-begin` header, raw
/// binary frames, then the matching `-end` trailer. One transfer in
/// flight at a time — the browser serializes them, so anonymous
/// binary frames always belong to the last announced header; a new
/// begin replaces (drops) a dangling one. Byte twin of
/// [`WriteReassembler`]; rejections mirror its reason-string contract.
#[derive(Default)]
pub(crate) struct RichRx {
    in_flight: Option<RichRxInFlight>,
}

enum RichRxKind {
    Image { w: u32, h: u32 },
    Html { html_bytes: usize },
    Native { rtf_bytes: usize, html_bytes: usize },
}

struct RichRxInFlight {
    id: String,
    kind: RichRxKind,
    declared: usize,
    buf: Vec<u8>,
}

/// Completed inbound rich payload from [`RichRx::end`].
#[derive(Debug)]
pub(crate) enum RichPayload {
    Image(PngImage),
    Html(HtmlPayload),
    Native(Box<NativePayload>),
}

impl RichRx {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn replace_dangling(&mut self) {
        if let Some(stale) = self.in_flight.take() {
            // The browser serializes transfers; a dangling one means it
            // gave up mid-stream (reload / error). Replace it.
            tracing::debug!(stale = ?stale.id, "clipboard: replacing dangling rich transfer");
        }
    }

    pub(crate) fn begin_image(
        &mut self,
        id: String,
        w: u32,
        h: u32,
        bytes: u64,
        format: &str,
    ) -> Result<(), String> {
        if !format.eq_ignore_ascii_case("png") {
            return Err(format!(
                "unsupported clipboard image format {format:?} (png only)"
            ));
        }
        if bytes == 0 || bytes > CLIPBOARD_IMAGE_MAX_BYTES as u64 {
            return Err(format!(
                "clipboard image {bytes} bytes outside the (0, {CLIPBOARD_IMAGE_MAX_BYTES}] cap"
            ));
        }
        if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
            return Err(format!(
                "clipboard image dims {w}x{h} outside the {MAX_IMAGE_PIXELS}-pixel cap"
            ));
        }
        self.replace_dangling();
        self.in_flight = Some(RichRxInFlight {
            id,
            kind: RichRxKind::Image { w, h },
            declared: bytes as usize,
            buf: Vec::with_capacity(bytes as usize),
        });
        Ok(())
    }

    pub(crate) fn begin_html(
        &mut self,
        id: String,
        html_bytes: u64,
        text_bytes: u64,
    ) -> Result<(), String> {
        let total = html_bytes.saturating_add(text_bytes);
        if html_bytes == 0 || total > CLIPBOARD_HTML_MAX_BYTES as u64 {
            return Err(format!(
                "clipboard html payload {total} bytes outside the (0, {CLIPBOARD_HTML_MAX_BYTES}] cap"
            ));
        }
        self.replace_dangling();
        self.in_flight = Some(RichRxInFlight {
            id,
            kind: RichRxKind::Html {
                html_bytes: html_bytes as usize,
            },
            declared: total as usize,
            buf: Vec::with_capacity(total as usize),
        });
        Ok(())
    }

    pub(crate) fn begin_native(
        &mut self,
        id: String,
        rtf_bytes: u64,
        html_bytes: u64,
        text_bytes: u64,
    ) -> Result<(), String> {
        let total = rtf_bytes
            .saturating_add(html_bytes)
            .saturating_add(text_bytes);
        if rtf_bytes == 0 || total > CLIPBOARD_NATIVE_MAX_BYTES as u64 {
            return Err(format!(
                "clipboard native payload {total} bytes outside the (0, {CLIPBOARD_NATIVE_MAX_BYTES}] cap"
            ));
        }
        self.replace_dangling();
        self.in_flight = Some(RichRxInFlight {
            id,
            kind: RichRxKind::Native {
                rtf_bytes: rtf_bytes as usize,
                html_bytes: html_bytes as usize,
            },
            declared: total as usize,
            buf: Vec::with_capacity(total as usize),
        });
        Ok(())
    }

    pub(crate) fn frame(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(f) = self.in_flight.as_mut() else {
            return Err("binary frame with no rich transfer in flight".into());
        };
        if f.buf.len() + data.len() > f.declared {
            let id = f.id.clone();
            self.in_flight = None;
            return Err(format!("rich transfer {id} overflowed its declared length"));
        }
        f.buf.extend_from_slice(data);
        Ok(())
    }

    pub(crate) fn end(&mut self, id: &str) -> Result<RichPayload, String> {
        let Some(f) = self.in_flight.take() else {
            return Err("end with no rich transfer in flight".into());
        };
        if f.id != id {
            return Err(format!("end id {id:?} does not match in-flight {:?}", f.id));
        }
        if f.buf.len() != f.declared {
            return Err(format!(
                "rich transfer {id} ended at {} of {} declared bytes",
                f.buf.len(),
                f.declared
            ));
        }
        match f.kind {
            RichRxKind::Image { w, h } => Ok(RichPayload::Image(PngImage { w, h, png: f.buf })),
            RichRxKind::Html { html_bytes } => {
                let (html_raw, text_raw) = f.buf.split_at(html_bytes.min(f.buf.len()));
                let html = String::from_utf8(html_raw.to_vec())
                    .map_err(|_| format!("rich transfer {id}: html is not valid UTF-8"))?;
                let text = String::from_utf8(text_raw.to_vec())
                    .map_err(|_| format!("rich transfer {id}: text is not valid UTF-8"))?;
                Ok(RichPayload::Html(HtmlPayload { html, text }))
            }
            RichRxKind::Native {
                rtf_bytes,
                html_bytes,
            } => {
                let (rtf_raw, rest) = f.buf.split_at(rtf_bytes.min(f.buf.len()));
                let (html_raw, text_raw) = rest.split_at(html_bytes.min(rest.len()));
                let html = String::from_utf8(html_raw.to_vec())
                    .map_err(|_| format!("rich transfer {id}: html is not valid UTF-8"))?;
                let text = String::from_utf8(text_raw.to_vec())
                    .map_err(|_| format!("rich transfer {id}: text is not valid UTF-8"))?;
                Ok(RichPayload::Native(Box::new(NativePayload {
                    rtf: rtf_raw.to_vec(),
                    html,
                    text,
                })))
            }
        }
    }

    /// Test-only visibility.
    #[cfg(test)]
    pub(crate) fn is_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
}

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

/// #3813 — the opt-in for every test that reaches the REAL clipboard. Such a
/// test writes it, and a write replaces whatever the person at this machine
/// had copied: a broad native `cargo test` once did that on a developer's own
/// desktop. Only `1` opts in, as with the crate's other `ROOMLERD_TEST_*`
/// variables. CI sets it by name in the steps that have a clipboard nobody
/// uses (`docs/testing.md`).
#[cfg(test)]
pub(crate) const REAL_CLIPBOARD_VAR: &str = "ROOMLERD_TEST_REAL_CLIPBOARD";

/// #3813 — whether a value of [`REAL_CLIPBOARD_VAR`] opts in. Kept apart from
/// the reading, so that its test sets no variable and reaches no clipboard.
#[cfg(test)]
fn real_clipboard_opt_in(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// #3813 — the first thing every test that reaches the real clipboard does.
/// `true`: run. `false`: it prints that `test` skipped, and the test returns,
/// which passes. Once opted in, a clipboard that does not open is a failure,
/// so a step that sets the variable cannot pass by running nothing.
#[cfg(test)]
pub(crate) fn real_clipboard_opted_in(test: &str) -> bool {
    let run = real_clipboard_opt_in(std::env::var_os(REAL_CLIPBOARD_VAR).as_deref());
    if !run {
        eprintln!("{REAL_CLIPBOARD_VAR} unset: skipping {test}, which writes the real clipboard");
    }
    run
}

/// FR-97 P3e-3a — the change gate against a real X server (CI's Xvfb step,
/// `ROOMLERD_TEST_X11=1`): another client takes CLIPBOARD, as a copy does, and
/// the gate counts it and then holds. Taking CLIPBOARD empties the clipboard
/// of whoever is at that display, so the test also waits for the real
/// clipboard's opt-in (#3813): `ROOMLERD_TEST_X11=1` alone says only that a
/// server is there.
#[cfg(all(test, target_os = "linux"))]
mod x11_gate_tests {
    use super::x11_gate::Gate;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
    use x11rb::wrapper::ConnectionExt as _;

    #[test]
    fn x11_real_the_gate_counts_a_copy() {
        if std::env::var("ROOMLERD_TEST_X11").as_deref() != Ok("1") {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return;
        }
        if !super::real_clipboard_opted_in("x11_real_the_gate_counts_a_copy") {
            return;
        }
        let mut gate = Gate::connect().expect("ROOMLERD_TEST_X11=1 but the gate did not open");
        let before = gate.seq().expect("a counter");
        let (conn, screen) = x11rb::connect(None).expect("the same X server");
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id().unwrap();
        conn.create_window(
            0,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap();
        let clipboard = conn
            .intern_atom(false, b"CLIPBOARD")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME)
            .unwrap();
        conn.sync().unwrap();
        let mut after = before;
        for _ in 0..50 {
            after = gate.seq().expect("a counter");
            if after != before {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_ne!(after, before, "the owner change was counted");
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Only an owner change moves it. The lane runs one test at a time; a
        // parallel run may have another test take CLIPBOARD meanwhile, so a
        // later count must come with an owner other than ours.
        let later = gate.seq().expect("a counter");
        if later != after {
            let owner = conn
                .get_selection_owner(clipboard)
                .unwrap()
                .reply()
                .unwrap()
                .owner;
            assert_ne!(owner, win, "the counter moved while CLIPBOARD stayed ours");
        }
        eprintln!("x11-real: the clipboard gate counted an owner change ({before} → {after})");
    }
}

/// #3671 — the rule for a failure on the desktop's X server ([`Retry`]), as the
/// shared worker ([`Clipboard::shared`]) and the change gate's answer
/// (`x11_gate::available`) keep it. Stand-ins only: nothing here reaches a
/// clipboard or an X server, so it runs wherever `clipboard::` does.
#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn up(generation: u64) -> Option<XServerNow> {
        Some(XServerNow {
            generation,
            down: None,
        })
    }

    fn down(generation: u64, period: u64) -> Option<XServerNow> {
        Some(XServerNow {
            generation,
            down: Some(period),
        })
    }

    #[derive(Clone, Default)]
    struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// What `f` returns, and the lines it logs on this thread at INFO and up.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
    )]
    pub(super) fn logged<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        let r = tracing::subscriber::with_default(subscriber, f);
        let all = lines.0.lock().unwrap_or_else(|e| e.into_inner());
        let said = String::from_utf8_lossy(&all)
            .lines()
            .map(String::from)
            .collect();
        (r, said)
    }

    /// The rule, row by row: a failure is kept only while its cause can
    /// still hold, and said once per period of the desktop.
    #[test]
    fn the_retry_rule_keeps_a_failure_only_while_its_cause_can_hold() {
        let t0 = Instant::now();
        let mut r = Retry::new();
        assert!(r.may_try(None, t0), "nothing failed yet");
        assert!(r.failed(down(0, 0), t0), "the first failure is said");
        assert!(
            !r.may_try(down(0, 0), t0 + Retry::EVERY * 3),
            "down: no server to reach"
        );
        assert!(
            r.may_try(up(0), t0),
            "up on a server the failure did not meet: at once"
        );
        assert!(r.failed(up(0), t0), "a new period is said again");
        assert!(!r.may_try(up(0), t0 + Retry::EVERY - Duration::from_millis(1)));
        assert!(r.may_try(up(0), t0 + Retry::EVERY), "after the interval");
        assert!(
            !r.failed(up(0), t0 + Retry::EVERY),
            "the same period is said once"
        );
        assert!(
            r.may_try(up(1), t0 + Retry::EVERY),
            "a newer server: at once"
        );
        assert!(
            !r.may_try(None, t0 + Retry::EVERY * 100),
            "no desktop of its own: kept for life"
        );
        assert!(r.succeeded(), "a success ends the failure");
        assert!(!r.succeeded(), "and is said once");
        assert!(r.may_try(None, t0));
    }

    /// #3671 — the shared worker, first asked while the desktop is down: kept
    /// off while it is down, started at the first ask once it is up, then
    /// kept. Before #3671 the failure was kept for the daemon's life.
    #[test]
    fn a_shared_worker_that_failed_while_the_desktop_was_down_starts_once_it_is_up() {
        let slot = Kept::<u32>::new();
        let tries = AtomicU32::new(0);
        let fails = || -> Result<u32> {
            tries.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("no X server"))
        };
        let t0 = Instant::now();
        let (got, said) = logged(|| shared_at(&slot, down(0, 0), t0, fails));
        assert_eq!(got, None);
        assert_eq!(said.len(), 1, "{said:#?}");
        assert!(
            said[0].contains("WARN") && said[0].contains("tried again at the next ask"),
            "{said:#?}"
        );
        let (got, said) = logged(|| shared_at(&slot, down(0, 0), t0 + Retry::EVERY * 6, fails));
        assert_eq!(got, None);
        assert_eq!(tries.load(Ordering::SeqCst), 1, "nothing tried while down");
        assert!(said.is_empty(), "{said:#?}");
        let (got, said) = logged(|| {
            shared_at(&slot, up(0), t0 + Duration::from_secs(1), || {
                tries.fetch_add(1, Ordering::SeqCst);
                Ok(7)
            })
        });
        assert_eq!(got, Some(7), "started once the desktop is up");
        assert_eq!(tries.load(Ordering::SeqCst), 2);
        assert_eq!(said.len(), 1, "{said:#?}");
        assert!(
            said[0].contains("INFO") && said[0].contains("after a start that failed"),
            "{said:#?}"
        );
        let (got, said) = logged(|| {
            shared_at(&slot, up(1), t0, || -> Result<u32> {
                panic!("a started worker is kept")
            })
        });
        assert_eq!(got, Some(7));
        assert!(said.is_empty(), "{said:#?}");
    }

    /// #3671 — on a live server, a start that failed is tried again no sooner
    /// than the interval, and said once.
    #[test]
    fn a_shared_worker_that_failed_on_a_live_server_is_tried_again_after_the_interval() {
        let slot = Kept::<u32>::new();
        let tries = AtomicU32::new(0);
        let fails = || -> Result<u32> {
            tries.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("refused"))
        };
        let t0 = Instant::now();
        let (_, said) = logged(|| shared_at(&slot, up(3), t0, fails));
        assert_eq!(said.len(), 1, "{said:#?}");
        let (got, said) = logged(|| shared_at(&slot, up(3), t0 + Duration::from_secs(1), fails));
        assert_eq!((got, tries.load(Ordering::SeqCst)), (None, 1), "too soon");
        assert!(said.is_empty(), "{said:#?}");
        let (got, said) = logged(|| shared_at(&slot, up(3), t0 + Retry::EVERY, fails));
        assert_eq!(
            (got, tries.load(Ordering::SeqCst)),
            (None, 2),
            "tried again"
        );
        assert!(said.is_empty(), "said once per period: {said:#?}");
        let (got, said) = logged(|| shared_at(&slot, up(3), t0 + Retry::EVERY * 2, || Ok(9)));
        assert_eq!(got, Some(9));
        assert_eq!(said.len(), 1, "{said:#?}");
    }

    /// #3671 — a host whose desktop is not the daemon's own keeps the first
    /// failure for the daemon's life, as before #3671, in master's words.
    #[test]
    fn without_a_desktop_of_its_own_a_failed_shared_worker_stays_off() {
        let slot = Kept::<u32>::new();
        let tries = AtomicU32::new(0);
        let fails = || -> Result<u32> {
            tries.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("no display"))
        };
        let t0 = Instant::now();
        let (got, said) = logged(|| shared_at(&slot, None, t0, fails));
        assert_eq!(got, None);
        assert_eq!(said.len(), 1, "{said:#?}");
        assert!(
            said[0].contains("clipboard: shared worker init failed — clipboard disabled"),
            "{said:#?}"
        );
        for later in [Duration::ZERO, Retry::EVERY, Retry::EVERY * 1000] {
            let (got, said) = logged(|| shared_at(&slot, None, t0 + later, fails));
            assert_eq!(got, None);
            assert!(said.is_empty(), "{said:#?}");
        }
        assert_eq!(tries.load(Ordering::SeqCst), 1, "tried once, for good");
    }

    /// #3671 — the change gate's answer for the hello, asked while the
    /// desktop is down and again once it is up: `true` the second time, and
    /// tried no more often than the interval while it fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_gate_answer_asked_while_the_desktop_is_down_is_asked_again_once_it_is_up() {
        let slot = Kept::<()>::new();
        let tries = AtomicU32::new(0);
        let opens = |answer: bool| {
            let tries = &tries;
            move || {
                tries.fetch_add(1, Ordering::SeqCst);
                answer
            }
        };
        let t0 = Instant::now();
        assert!(!x11_gate::available_at(&slot, down(0, 0), t0, opens(false)));
        assert!(!x11_gate::available_at(
            &slot,
            down(0, 0),
            t0 + Retry::EVERY,
            opens(true)
        ));
        assert_eq!(tries.load(Ordering::SeqCst), 1, "nothing tried while down");
        assert!(!x11_gate::available_at(&slot, up(0), t0, opens(false)));
        assert!(!x11_gate::available_at(
            &slot,
            up(0),
            t0 + Duration::from_secs(1),
            opens(true)
        ));
        assert_eq!(
            tries.load(Ordering::SeqCst),
            2,
            "no more often than the interval"
        );
        assert!(x11_gate::available_at(
            &slot,
            up(0),
            t0 + Retry::EVERY,
            opens(true)
        ));
        assert!(
            x11_gate::available_at(&slot, up(0), t0, opens(false)),
            "kept"
        );
        assert_eq!(tries.load(Ordering::SeqCst), 3);
    }

    /// #3671 — a host with no desktop of its own decides the gate's answer once,
    /// as before #3671, whichever way it went.
    #[cfg(target_os = "linux")]
    #[test]
    fn without_a_desktop_of_its_own_the_gate_answer_is_decided_once() {
        let t0 = Instant::now();
        let no = Kept::<()>::new();
        assert!(!x11_gate::available_at(&no, None, t0, || false));
        assert!(!x11_gate::available_at(
            &no,
            None,
            t0 + Retry::EVERY * 1000,
            || true
        ));
        let yes = Kept::<()>::new();
        assert!(x11_gate::available_at(&yes, None, t0, || true));
        assert!(x11_gate::available_at(&yes, None, t0, || false));
    }
}

/// #3671 — the worker's follow ([`x11_follow::Follow`]) through its seams: the
/// desktop and the time are given, and the connect is a stand-in, so nothing
/// here reaches a clipboard or an X server.
#[cfg(all(test, target_os = "linux"))]
mod follow_tests {
    use super::retry_tests::logged;
    use super::x11_follow::{Connect, Follow};
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn up(generation: u64) -> Option<XServerNow> {
        Some(XServerNow {
            generation,
            down: None,
        })
    }

    /// A connect that counts its calls and answers from `answers` in turn,
    /// the last one for good.
    fn scripted(answers: Vec<Result<u32, String>>) -> (Connect<u32>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let counted = calls.clone();
        let connect: Connect<u32> = Arc::new(move || {
            let n = counted.fetch_add(1, Ordering::SeqCst) as usize;
            answers[n.min(answers.len() - 1)].clone()
        });
        (connect, calls)
    }

    const BOUND: Duration = Duration::from_secs(5);

    /// #3671 — only a NEWER generation is a new server. After the teardown at a
    /// stop the desktop reads `None` (its generation 0): nothing is tried and
    /// nothing is said, nor for an older or the same generation.
    #[test]
    fn a_follow_acts_only_on_a_newer_generation_and_says_nothing_otherwise() {
        let (connect, calls) = scripted(vec![Ok(7)]);
        let mut follow = Follow::new(up(0), connect, BOUND);
        let t0 = Instant::now();
        let (got, said) = logged(|| follow.check(up(1), t0));
        assert_eq!(got, Some(7), "generation 1 is followed");
        assert_eq!(said.len(), 1, "{said:#?}");
        assert!(
            said[0].contains("follows the desktop's new X server"),
            "{said:#?}"
        );
        let (got, said) = logged(|| {
            [
                follow.check(
                    Some(XServerNow {
                        generation: 0,
                        down: None,
                    }),
                    t0,
                ),
                follow.check(None, t0),
                follow.check(up(1), t0 + Retry::EVERY),
            ]
        });
        assert_eq!(got, [None, None, None]);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no rebuild started");
        assert!(said.is_empty(), "{said:#?}");
    }

    /// #3671 — nothing is tried while the desktop is down, and a newer
    /// generation is followed once it is up.
    #[test]
    fn a_follow_waits_for_the_desktop_to_be_up() {
        let (connect, calls) = scripted(vec![Ok(7)]);
        let mut follow = Follow::new(up(0), connect, BOUND);
        let t0 = Instant::now();
        let gone = Some(XServerNow {
            generation: 1,
            down: Some(2),
        });
        assert_eq!(follow.check(gone, t0), None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(follow.check(up(1), t0), Some(7));
    }

    /// #3671 — a rebuild that failed is tried again no sooner than the interval
    /// while the same server answers, warned once, and said once when it
    /// follows. Before, it was tried again only at the next X server.
    #[test]
    fn a_failed_follow_is_tried_again_after_the_interval_and_warned_once() {
        let (connect, calls) = scripted(vec![Err("refused".into()), Err("refused".into()), Ok(7)]);
        let mut follow = Follow::new(up(0), connect, BOUND);
        let t0 = Instant::now();
        let (got, said) = logged(|| follow.check(up(1), t0));
        assert_eq!(got, None);
        assert_eq!(said.len(), 1, "{said:#?}");
        assert!(
            said[0].contains("WARN") && said[0].contains("could not follow"),
            "{said:#?}"
        );
        let (got, said) = logged(|| {
            [
                follow.check(up(1), t0 + Duration::from_secs(1)),
                follow.check(up(1), t0 + Retry::EVERY),
            ]
        });
        assert_eq!(got, [None, None]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "once more, after the interval"
        );
        assert!(said.is_empty(), "warned once per generation: {said:#?}");
        let (got, said) = logged(|| follow.check(up(1), t0 + Retry::EVERY * 2));
        assert_eq!(got, Some(7));
        assert_eq!(said.len(), 1, "{said:#?}");
        assert!(said[0].contains("after_failure=true"), "{said:#?}");
    }

    /// #3671 — a failure on one generation does not hold back the next one.
    #[test]
    fn a_follow_that_failed_tries_the_next_generation_at_once() {
        let (connect, calls) = scripted(vec![Err("refused".into()), Ok(7)]);
        let mut follow = Follow::new(up(0), connect, BOUND);
        let t0 = Instant::now();
        assert_eq!(follow.check(up(1), t0), None);
        assert_eq!(follow.check(up(2), t0), Some(7));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// #3671 — a rebuild that outlasts the bound is not given up on: the worker
    /// serves meanwhile, starts no second one while it runs (a hung one holds
    /// arboard's global lock), and takes its answer at a later look.
    #[test]
    fn a_follow_that_outlasts_its_bound_is_taken_late_and_never_doubled() {
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Mutex::new(held);
        let calls = Arc::new(AtomicU32::new(0));
        let counted = calls.clone();
        let connect: Connect<u32> = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = held.lock().unwrap_or_else(|e| e.into_inner()).recv();
            Ok(7)
        });
        let mut follow = Follow::new(up(0), connect, Duration::from_millis(50));
        let t0 = Instant::now();
        let (got, said) = logged(|| follow.check(up(1), t0));
        assert_eq!(got, None);
        assert_eq!(said.len(), 1, "{said:#?}");
        assert!(said[0].contains("takes longer than"), "{said:#?}");
        let (got, said) = logged(|| follow.check(up(1), t0 + Retry::EVERY * 3));
        assert_eq!(got, None, "still running");
        assert!(said.is_empty(), "{said:#?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "never a second rebuild");
        release.send(()).expect("the rebuild waits");
        let deadline = Instant::now() + Duration::from_secs(5);
        let (got, said) = loop {
            let (got, said) = logged(|| follow.check(up(1), t0 + Retry::EVERY * 3));
            if got.is_some() || Instant::now() > deadline {
                break (got, said);
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(got, Some(7), "taken late");
        assert_eq!(said.len(), 1, "{said:#?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// #3671 — the proof passes on any answer from a live connection, and fails
    /// only on arboard's word for a connection that failed.
    #[test]
    fn the_proof_passes_on_any_answer_from_a_live_connection() {
        use super::x11_follow::proves_a_live_connection as live;
        assert!(live(&Ok(String::new())));
        assert!(live(&Err(arboard::Error::ContentNotAvailable)));
        assert!(live(&Err(arboard::Error::ConversionFailure)));
        assert!(live(&Err(arboard::Error::ClipboardOccupied)));
        assert!(!live(&Err(arboard::Error::Unknown {
            description: "Broken pipe (os error 32)".into(),
        })));
    }
}

/// #3671 — the worker follows a new X server on the display it serves:
/// against an Xvfb of the test's own (`ROOMLERD_TEST_X11=1`), killed and
/// started again on the same display as the desktop's supervisor does, with
/// the desktop's generation a counter of the test's. Each test writes the
/// real clipboard of that display, so it waits for the real clipboard's
/// opt-in too (#3813). CI's real-clipboard step runs each test of this module
/// in a process of its own (`--exact`): arboard keeps one connection per
/// process, so a second test in the same process would start on the first
/// one's. `only_one_per_process` refuses that at once. `xclip` is the
/// desktop's other client.
#[cfg(all(test, target_os = "linux"))]
mod x11_follow_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static GENERATION: AtomicU64 = AtomicU64::new(0);

    /// Set by the first real-X clipboard test this process runs.
    static RAN: AtomicBool = AtomicBool::new(false);

    /// Called by each real-X clipboard test once it has opted in: a second one
    /// in the same process would read arboard's connection to the first one's
    /// server, so it fails at once, with the reason, instead of later with a
    /// misleading one.
    fn only_one_per_process() {
        assert!(
            !RAN.swap(true, Ordering::SeqCst),
            "CI runs each real-X clipboard test in a process of its own: arboard keeps one connection per process"
        );
    }

    /// The desktop as the worker reads it: always up, on the test's count.
    fn desk() -> Option<XServerNow> {
        Some(XServerNow {
            generation: GENERATION.load(Ordering::SeqCst),
            down: None,
        })
    }

    /// Everything this process logs from here on, arboard's `log` lines too,
    /// with each line's thread: the worker, its helper and arboard's serving
    /// thread log from threads of their own, which a subscriber set for the
    /// test's thread does not hear. Set once for the process; CI runs each
    /// test of this module in a process of its own.
    #[derive(Clone, Default)]
    struct ProcessLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for ProcessLog {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl ProcessLog {
        /// The process's log, installed at the first call with the bridge
        /// from `log` (`try_init`). `None`: another subscriber was there first.
        #[expect(
            clippy::disallowed_methods,
            reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
        )]
        fn get() -> Option<&'static ProcessLog> {
            use tracing_subscriber::util::SubscriberInitExt as _;
            static LOG: std::sync::OnceLock<Option<ProcessLog>> = std::sync::OnceLock::new();
            LOG.get_or_init(|| {
                let log = ProcessLog::default();
                let writer = log.clone();
                tracing_subscriber::fmt()
                    .with_target(false)
                    .with_ansi(false)
                    .without_time()
                    .with_thread_names(true)
                    .with_max_level(tracing::Level::INFO)
                    .with_writer(move || writer.clone())
                    .finish()
                    .try_init()
                    .ok()
                    .map(|()| log)
            })
            .as_ref()
        }

        fn len(&self) -> usize {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
        }

        /// The lines logged since `from` (a [`Self::len`]).
        fn since(&self, from: usize) -> Vec<String> {
            let all = self.0.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&all[from.min(all.len())..])
                .lines()
                .map(String::from)
                .collect()
        }
    }

    /// The CLIPBOARD selection as another client of `display` reads it.
    fn desktop_reads(display: &str) -> String {
        let out = std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-o", "-display", display])
            .output()
            .expect("xclip, which CI's real-clipboard step installs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Another client of `display` copies `text`, as a copy on the desktop
    /// does, and the copy is readable. xclip forks to serve the selection;
    /// that child ends with the test's server.
    fn desktop_copies(display: &str, text: &str) {
        use std::io::Write as _;
        let mut xclip = std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-i", "-display", display])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("xclip, which CI's real-clipboard step installs");
        xclip
            .stdin
            .take()
            .expect("xclip's input")
            .write_all(text.as_bytes())
            .expect("xclip took the text");
        let _ = xclip.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        while desktop_reads(display) != text {
            assert!(
                Instant::now() < deadline,
                "xclip's copy never became readable"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The test's own Xvfb, `DISPLAY` naming it, a worker on it and a runtime
    /// for the worker's async API. `None`: the lane did not opt in.
    fn worker_on_own_server(
        test: &str,
    ) -> Option<(
        crate::virtual_desktop::TestX,
        crate::virtual_desktop::TestDisplay,
        Clipboard,
        tokio::runtime::Runtime,
    )> {
        if !crate::virtual_desktop::TestX::opted_in() {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return None;
        }
        if !real_clipboard_opted_in(test) {
            return None;
        }
        only_one_per_process();
        let x = crate::virtual_desktop::TestX::start()
            .expect("ROOMLERD_TEST_X11=1 but no Xvfb came up");
        let display = x.point_display();
        let cb =
            Clipboard::following(desk).expect("the clipboard worker opens on the test's own Xvfb");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        Some((x, display, cb, rt))
    }

    /// After the restart a paste from the viewer reaches the desktop, a copy
    /// on the desktop reaches the viewer, and a session that subscribed to
    /// changes before the restart hears the copy: the worker and its change
    /// gate are on the new server, with no daemon restart, and the rebuild
    /// says so in one line: the second handle's drop on the dead connection
    /// logs nothing (#3671). Before #3671 the worker kept the dead
    /// connection: the paste failed.
    #[test]
    fn x11_real_the_clipboard_follows_a_new_x_server() {
        x11_follow::SECOND_HANDLE.store(true, Ordering::SeqCst);
        let Some((x, _display, cb, rt)) =
            worker_on_own_server("x11_real_the_clipboard_follows_a_new_x_server")
        else {
            return;
        };
        let log = ProcessLog::get()
            .expect("the log capture installs: CI runs this test in a process of its own");
        let display = x.display().to_string();
        rt.block_on(cb.write("before the restart".into()))
            .expect("a write on the first server");
        assert_eq!(desktop_reads(&display), "before the restart");
        let (tx, mut changes) = tokio::sync::mpsc::channel(8);
        cb.watch(
            WatchEvents {
                text: true,
                ..WatchEvents::default()
            },
            tx,
        )
        .expect("a watch");

        let mark = log.len();
        x.kill();
        // #3671 — what production always has before the generation moves:
        // arboard's serving thread saw the connection die and ended (its
        // "Worker thread errored with" line is its last act), every request
        // on the old connection fails, and the supervisor waits its stop grace
        // (1 s) before a new server. A rebuild that met that thread still
        // holding its share of arboard's global would find four owners, not
        // three, and tear nothing down.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !log
            .since(mark)
            .iter()
            .any(|l| l.contains("Worker thread errored with"))
        {
            assert!(
                Instant::now() < deadline,
                "arboard's serving thread did not end within 5 s of its server's kill: {:#?}",
                log.since(mark)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            rt.block_on(cb.read()).is_err(),
            "a read on the dead server's connection answered"
        );
        std::thread::sleep(Duration::from_secs(1));
        assert!(x.restart(), "no new server on the same display");
        GENERATION.fetch_add(1, Ordering::SeqCst);

        // The viewer pastes: the first command after the restart meets the
        // new server.
        let asked = Instant::now();
        let pasted = rt.block_on(cb.write("after the restart".into()));
        let took = asked.elapsed();
        assert!(
            pasted.is_ok(),
            "the paste after the restart failed: {pasted:?}"
        );
        assert!(
            took < x11_follow::REBUILD_BOUND,
            "following the new server took {took:?}"
        );
        assert_eq!(
            desktop_reads(&display),
            "after the restart",
            "the desktop did not get the paste"
        );
        // #3671 — one line for the rebuild, and none of arboard's ERRORs
        // from the second handle's drop on the dead connection.
        let lines = log.since(mark);
        let follows = lines
            .iter()
            .filter(|l| l.contains("the clipboard follows the desktop's new X server"))
            .count();
        assert_eq!(follows, 1, "the rebuild's line, once: {lines:#?}");
        for noise in [
            "Could not hand the clipboard data over",
            "Failed to destroy the clipboard window",
            "could not follow the desktop's new X server",
        ] {
            assert!(
                !lines.iter().any(|l| l.contains(noise)),
                "the rebuild logged {noise:?}: {lines:#?}"
            );
        }

        // The person copies on the desktop.
        desktop_copies(&display, "copied on the new desktop");
        assert_eq!(
            rt.block_on(cb.read()).expect("a read on the new server"),
            "copied on the new desktop"
        );
        let heard = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(change) = changes.recv().await {
                    if matches!(&change, ClipboardChange::Text(t) if t == "copied on the new desktop")
                    {
                        return true;
                    }
                }
                false
            })
            .await
        });
        assert_eq!(
            heard,
            Ok(true),
            "the session that subscribed before the restart was not told of the copy"
        );
        println!(
            "x11-real: the clipboard followed a new X server in {took:?}: a paste, a copy and a watched copy on the new server"
        );
    }

    /// The negative control: the same restart with the rebuild's second
    /// handle taken out. The handle arboard then makes is the dead server's,
    /// its first read fails, and the worker stays where it was before #3671:
    /// the paste fails. If this ever passes, arboard's rule has changed (an
    /// upgrade): re-read its `Drop` before trusting `x11_follow`.
    #[test]
    fn x11_real_without_the_second_handle_the_clipboard_stays_on_the_dead_server() {
        let Some((x, _display, cb, rt)) = worker_on_own_server(
            "x11_real_without_the_second_handle_the_clipboard_stays_on_the_dead_server",
        ) else {
            return;
        };
        let display = x.display().to_string();
        rt.block_on(cb.write("before the restart".into()))
            .expect("a write on the first server");
        // The preconditions: the worker serves the first server both ways, and
        // the new one answers another client, so the paste that fails below
        // fails for the worker's connection and for nothing else.
        assert_eq!(desktop_reads(&display), "before the restart");
        desktop_copies(&display, "copied before the restart");
        assert_eq!(
            rt.block_on(cb.read()).expect("a read on the first server"),
            "copied before the restart"
        );
        x.kill();
        assert!(x.restart(), "no new server on the same display");
        desktop_copies(&display, "the new server answers");
        x11_follow::SECOND_HANDLE.store(false, Ordering::SeqCst);
        GENERATION.fetch_add(1, Ordering::SeqCst);
        let pasted = rt.block_on(cb.write("after the restart".into()));
        x11_follow::SECOND_HANDLE.store(true, Ordering::SeqCst);
        let Err(e) = pasted else {
            panic!("without the second handle the paste reached the new server");
        };
        assert_ne!(desktop_reads(&display), "after the restart");
        println!(
            "x11-real: without the second handle the clipboard stayed on the dead X server: {e:#}"
        );
    }

    /// #3671 — a clipboard first asked for while the desktop is down (in a
    /// restart, in a back-off gap, or after a first start that failed) starts
    /// at the first ask once a new X server answers, and a paste and a copy
    /// then reach that server: the failure is not kept for the daemon's life.
    /// The slot is the kind [`Clipboard::shared`] keeps and the start is its
    /// own (`Clipboard::new`); the desktop is the test's, read as the daemon
    /// reads its own. Before #3671 the first failure was kept, and the
    /// clipboard stayed off until the daemon restarted. In a process of its
    /// own: arboard keeps one connection per process.
    #[test]
    fn x11_real_a_clipboard_first_asked_while_the_desktop_is_down_starts_once_it_is_up() {
        if !crate::virtual_desktop::TestX::opted_in() {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return;
        }
        if !real_clipboard_opted_in(
            "x11_real_a_clipboard_first_asked_while_the_desktop_is_down_starts_once_it_is_up",
        ) {
            return;
        }
        only_one_per_process();
        let log = ProcessLog::get()
            .expect("the log capture installs: CI runs this test in a process of its own");
        let x = crate::virtual_desktop::TestX::start()
            .expect("ROOMLERD_TEST_X11=1 but no Xvfb came up");
        let _display = x.point_display();
        let display = x.display().to_string();
        x.kill();
        let mark = log.len();
        let slot: Kept<Clipboard> = Kept::new();
        let down = Some(XServerNow {
            generation: 0,
            down: Some(1),
        });
        assert!(
            shared_at(&slot, down, Instant::now(), Clipboard::new).is_none(),
            "a clipboard started on a display that no server answers"
        );
        assert!(
            shared_at(&slot, down, Instant::now(), Clipboard::new).is_none(),
            "a clipboard started while the desktop was still down"
        );
        assert!(x.restart(), "no new server on the same display");
        let up = Some(XServerNow {
            generation: 1,
            down: None,
        });
        let cb = shared_at(&slot, up, Instant::now(), Clipboard::new)
            .expect("the clipboard did not start at the first ask once the desktop was up");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(cb.write("pasted from the viewer".into()))
            .expect("a paste on the new server");
        assert_eq!(
            desktop_reads(&display),
            "pasted from the viewer",
            "the desktop did not get the paste"
        );
        desktop_copies(&display, "copied on the desktop");
        assert_eq!(
            rt.block_on(cb.read()).expect("a read on the new server"),
            "copied on the desktop"
        );
        let lines = log.since(mark);
        let said = |what: &str| lines.iter().filter(|l| l.contains(what)).count();
        assert_eq!(
            (
                said("shared worker init failed on this device's virtual desktop"),
                said("the shared worker started on this device's virtual desktop")
            ),
            (1, 1),
            "one line for the failure and one for the start: {lines:#?}"
        );
        println!(
            "x11-real: a clipboard first asked for while the desktop was down started once it was up: a paste and a copy on the new server"
        );
    }
}

/// FR-97 P2e-1 — the change gate against the real pasteboard (CI's macOS job):
/// a write moves the change count, as a copy does, and the gate counts it and
/// then holds. Only under the real clipboard's opt-in,
/// `ROOMLERD_TEST_REAL_CLIPBOARD=1` (#3813, which replaced this test's own
/// `ROOMLERD_TEST_MAC_PASTEBOARD`): the write replaces whatever the person at
/// this Mac had copied.
#[cfg(all(test, target_os = "macos"))]
mod mac_gate_tests {
    use super::mac_gate::Gate;

    const PROBE: &str = "FR-97 P2e-1 change-gate probe";

    #[test]
    fn mac_real_the_gate_counts_a_copy() {
        if !super::real_clipboard_opted_in("mac_real_the_gate_counts_a_copy") {
            return;
        }
        let mut gate = Gate::connect();
        let before = gate.seq().expect("a counter");
        assert_ne!(before, 0, "the watcher reads 0 as no counter");
        let mut cb = arboard::Clipboard::new().expect("the general pasteboard");
        cb.set_text(PROBE).expect("a write");
        let mut after = before;
        for _ in 0..50 {
            after = gate.seq().expect("a counter");
            if after != before {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_ne!(after, before, "the write was counted");
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Only a write moves it. A parallel test may write meanwhile, so a later
        // count must come with a copy other than ours.
        let later = gate.seq().expect("a counter");
        if later != after {
            assert_ne!(
                cb.get_text().ok().as_deref(),
                Some(PROBE),
                "the counter moved while the copy stayed ours"
            );
        }
        eprintln!("mac-real: the clipboard gate counted a write ({before} → {after})");
    }
}

/// #3733 — a clipboard for a test of the channel's handler, with nothing on
/// the host behind it: its worker takes every write except one of the text
/// `refused`, which it answers with an error, and holds nothing, so every read
/// finds the clipboard empty.
#[cfg(test)]
impl Clipboard {
    pub(crate) fn for_tests(refused: &'static str) -> Self {
        let (tx, rx) = std_mpsc::channel::<ClipboardCmd>();
        thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    ClipboardCmd::Write { text, reply } => {
                        let _ = reply.send(if text == refused {
                            Err(anyhow::anyhow!("the test's clipboard refused it"))
                        } else {
                            Ok(())
                        });
                    }
                    ClipboardCmd::WriteImage { reply, .. }
                    | ClipboardCmd::WriteHtml { reply, .. }
                    | ClipboardCmd::WriteNative { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    ClipboardCmd::Read { reply } => {
                        let _ = reply.send(Ok(String::new()));
                    }
                    ClipboardCmd::ReadImage { reply } => {
                        let _ = reply.send(Ok(None));
                    }
                    ClipboardCmd::ReadHtml { reply } => {
                        let _ = reply.send(Ok(None));
                    }
                    ClipboardCmd::ReadNative { reply } => {
                        let _ = reply.send(Ok(None));
                    }
                    ClipboardCmd::Watch { .. } | ClipboardCmd::Unwatch { .. } => {}
                    ClipboardCmd::Shutdown => return,
                }
            }
        });
        Self { tx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_incoming_write() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:write","text":"hi"}"#).unwrap();
        match m {
            ClipboardIncoming::Write { text, .. } => assert_eq!(text, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_read_with_req_id() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:read","req_id":42}"#).unwrap();
        match m {
            ClipboardIncoming::Read { req_id, .. } => assert_eq!(req_id, Some(42)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_read_without_req_id() {
        let m: ClipboardIncoming = serde_json::from_str(r#"{"t":"clipboard:read"}"#).unwrap();
        match m {
            ClipboardIncoming::Read { req_id, .. } => assert_eq!(req_id, None),
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

    // ── v2: parse locks for the additive fields + new variants ─────────

    #[test]
    fn parse_incoming_write_with_id() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:write","text":"hi","id":"cb-7"}"#).unwrap();
        match m {
            ClipboardIncoming::Write { text, id } => {
                assert_eq!(text, "hi");
                assert_eq!(id.as_deref(), Some("cb-7"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_write_without_id_stays_v1_compatible() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:write","text":"hi"}"#).unwrap();
        match m {
            ClipboardIncoming::Write { id, .. } => assert!(id.is_none()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_read_with_accept() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:read","req_id":1,"accept":["text","image"]}"#)
                .unwrap();
        match m {
            ClipboardIncoming::Read { req_id, accept } => {
                assert_eq!(req_id, Some(1));
                assert_eq!(accept, vec!["text", "image"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_read_without_accept_stays_v1_compatible() {
        let m: ClipboardIncoming = serde_json::from_str(r#"{"t":"clipboard:read"}"#).unwrap();
        match m {
            ClipboardIncoming::Read { accept, .. } => assert!(accept.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_incoming_subscribe_unsubscribe() {
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:subscribe","events":["text","image"]}"#)
                .unwrap();
        match m {
            ClipboardIncoming::Subscribe { events } => assert_eq!(events, vec!["text", "image"]),
            other => panic!("unexpected: {other:?}"),
        }
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:unsubscribe"}"#).unwrap();
        assert!(matches!(m, ClipboardIncoming::Unsubscribe));
    }

    #[test]
    fn parse_incoming_img_begin_end() {
        let m: ClipboardIncoming = serde_json::from_str(
            r#"{"t":"clipboard:img-begin","id":"cb-1","w":2,"h":3,"bytes":99,"format":"png"}"#,
        )
        .unwrap();
        match m {
            ClipboardIncoming::ImgBegin {
                id,
                w,
                h,
                bytes,
                format,
            } => {
                assert_eq!(
                    (id.as_str(), w, h, bytes, format.as_str()),
                    ("cb-1", 2, 3, 99, "png")
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:img-end","id":"cb-1"}"#).unwrap();
        match m {
            ClipboardIncoming::ImgEnd { id } => assert_eq!(id, "cb-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── v2: line-ending canonicalization ───────────────────────────────

    #[test]
    fn host_to_wire_normalizes_all_conventions() {
        assert_eq!(host_to_wire("a\r\nb\r\nc"), "a\nb\nc");
        assert_eq!(host_to_wire("a\rb"), "a\nb");
        assert_eq!(host_to_wire("a\nb"), "a\nb");
        assert_eq!(
            host_to_wire("mixed\r\nand\rand\nend\r"),
            "mixed\nand\nand\nend\n"
        );
        assert_eq!(host_to_wire(""), "");
        // No CR at all → passthrough (fast path).
        assert_eq!(host_to_wire("plain"), "plain");
    }

    #[test]
    fn lf_to_crlf_is_idempotent() {
        assert_eq!(lf_to_crlf("a\nb"), "a\r\nb");
        assert_eq!(
            lf_to_crlf("a\r\nb"),
            "a\r\nb",
            "existing CRLF must not double"
        );
        assert_eq!(lf_to_crlf(lf_to_crlf("x\ny\nz").as_str()), "x\r\ny\r\nz");
        // Lone CR is normalized too, not passed through.
        assert_eq!(lf_to_crlf("a\rb"), "a\r\nb");
    }

    #[test]
    fn wire_to_host_matches_platform_convention() {
        let out = wire_to_host("l1\nl2");
        #[cfg(windows)]
        assert_eq!(out, "l1\r\nl2");
        #[cfg(not(windows))]
        assert_eq!(out, "l1\nl2");
    }

    // ── v2: FNV-1a 64 (cross-side echo-suppression hash) ───────────────

    #[test]
    fn fnv1a64_matches_published_vectors() {
        // Standard FNV-1a 64 test vectors — the browser-side
        // `hashClipboardText`/`hashClipboardBytes` in useRemoteControl.ts
        // lock the SAME values; if either side changes, echo
        // suppression silently breaks.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    // ── v2: PNG codec ──────────────────────────────────────────────────

    #[test]
    fn png_roundtrip_2x2_rgba() {
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, /* */ 0, 255, 0, 128, //
            0, 0, 255, 255, /* */ 255, 255, 255, 0,
        ];
        let png = rgba_to_png(2, 2, &rgba).unwrap();
        let (w, h, back) = png_to_rgba(&png).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(back, rgba);
    }

    #[test]
    fn rgba_to_png_rejects_wrong_buffer_len() {
        let err = rgba_to_png(2, 2, &[0u8; 3]).unwrap_err();
        assert!(err.to_string().contains("expected"), "got: {err}");
    }

    #[test]
    fn png_to_rgba_rejects_garbage() {
        assert!(png_to_rgba(b"not a png at all").is_err());
    }

    #[test]
    fn png_to_rgba_rejects_decompression_bomb_before_alloc() {
        // Hand-craft a PNG whose IHDR declares 20000x20000 RGBA
        // (400 MPx — a 1.6 GB output buffer) followed by EMPTY
        // IDAT/IEND chunks. png 0.18's `read_info` parses this header
        // happily (its own byte limit only fires later, at frame
        // decode), so without OUR header-stage pixel cap the
        // `vec![0u8; output_buffer_size]` allocation would balloon to
        // 1.6 GB from a 70-byte wire payload. The cap must fire first.
        fn crc32(data: &[u8]) -> u32 {
            let mut crc: u32 = 0xffff_ffff;
            for &b in data {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let mut png_bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr: Vec<u8> = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&20000u32.to_be_bytes()); // width
        ihdr.extend_from_slice(&20000u32.to_be_bytes()); // height
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
        png_bytes.extend_from_slice(&13u32.to_be_bytes());
        png_bytes.extend_from_slice(&ihdr);
        png_bytes.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        for tag in [b"IDAT", b"IEND"] {
            png_bytes.extend_from_slice(&0u32.to_be_bytes());
            png_bytes.extend_from_slice(tag);
            png_bytes.extend_from_slice(&crc32(tag).to_be_bytes());
        }
        let err = png_to_rgba(&png_bytes).unwrap_err();
        assert!(
            err.to_string().contains("pixel cap"),
            "must fail on the header cap, got: {err}"
        );
    }

    #[test]
    fn png_to_rgba_expands_rgb_to_opaque_rgba() {
        // Encode a 1x2 RGB (no alpha) PNG with the png crate directly,
        // then decode through our helper — alpha must come back 255.
        let rgb: Vec<u8> = vec![10, 20, 30, 40, 50, 60];
        let mut buf = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, 1, 2);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&rgb).unwrap();
        }
        let (w, h, rgba) = png_to_rgba(&buf).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(rgba, vec![10, 20, 30, 255, 40, 50, 60, 255]);
    }

    // ── v2/v2.1: inbound rich reassembler (image + html) ───────────────

    fn expect_image(p: RichPayload) -> PngImage {
        match p {
            RichPayload::Image(img) => img,
            other => panic!("expected image payload, got {other:?}"),
        }
    }

    fn expect_html(p: RichPayload) -> HtmlPayload {
        match p {
            RichPayload::Html(h) => h,
            other => panic!("expected html payload, got {other:?}"),
        }
    }

    #[test]
    fn rich_rx_image_happy_path() {
        let mut rx = RichRx::new();
        rx.begin_image("i1".into(), 2, 2, 6, "png").unwrap();
        rx.frame(&[1, 2, 3]).unwrap();
        rx.frame(&[4, 5, 6]).unwrap();
        let img = expect_image(rx.end("i1").unwrap());
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.png, vec![1, 2, 3, 4, 5, 6]);
        assert!(!rx.is_in_flight());
    }

    #[test]
    fn rich_rx_image_rejects_bad_begin() {
        let mut rx = RichRx::new();
        assert!(
            rx.begin_image("i".into(), 2, 2, 6, "jpeg").is_err(),
            "png only"
        );
        assert!(
            rx.begin_image("i".into(), 2, 2, 0, "png").is_err(),
            "zero bytes"
        );
        assert!(
            rx.begin_image(
                "i".into(),
                2,
                2,
                CLIPBOARD_IMAGE_MAX_BYTES as u64 + 1,
                "png"
            )
            .is_err(),
            "over byte cap"
        );
        assert!(
            rx.begin_image("i".into(), 100_000, 100_000, 6, "png")
                .is_err(),
            "over pixel cap"
        );
        assert!(!rx.is_in_flight());
    }

    #[test]
    fn rich_rx_rejects_overflow_and_drops_transfer() {
        let mut rx = RichRx::new();
        rx.begin_image("i1".into(), 2, 2, 4, "png").unwrap();
        assert!(rx.frame(&[1, 2, 3, 4, 5]).is_err(), "over declared length");
        assert!(!rx.is_in_flight(), "overflow must drop the transfer");
        assert!(rx.frame(&[1]).is_err(), "no transfer in flight");
    }

    #[test]
    fn rich_rx_rejects_mismatched_end() {
        let mut rx = RichRx::new();
        rx.begin_image("i1".into(), 2, 2, 2, "png").unwrap();
        rx.frame(&[1, 2]).unwrap();
        assert!(rx.end("other").is_err(), "id mismatch");
        assert!(!rx.is_in_flight());

        rx.begin_image("i2".into(), 2, 2, 4, "png").unwrap();
        rx.frame(&[1, 2]).unwrap();
        let err = rx.end("i2").unwrap_err();
        assert!(err.contains("2 of 4"), "length mismatch surfaces: {err}");
    }

    #[test]
    fn rich_rx_new_begin_replaces_dangling_transfer() {
        let mut rx = RichRx::new();
        rx.begin_image("stale".into(), 2, 2, 100, "png").unwrap();
        rx.frame(&[9; 10]).unwrap();
        rx.begin_html("fresh".into(), 5, 2).unwrap();
        rx.frame(b"<b>xy").unwrap();
        rx.frame(b"hi").unwrap();
        let p = expect_html(rx.end("fresh").unwrap());
        assert_eq!(p.html, "<b>xy");
        assert_eq!(p.text, "hi");
    }

    #[test]
    fn rich_rx_html_happy_path_splits_on_declared_lengths() {
        let mut rx = RichRx::new();
        let html = "<b>bold</b> und ümlaut";
        let text = "bold und ümlaut";
        rx.begin_html("h1".into(), html.len() as u64, text.len() as u64)
            .unwrap();
        // Feed across an arbitrary boundary (mid-multibyte is fine —
        // the split happens at declared lengths on the full buffer).
        let combined = [html.as_bytes(), text.as_bytes()].concat();
        rx.frame(&combined[..10]).unwrap();
        rx.frame(&combined[10..]).unwrap();
        let p = expect_html(rx.end("h1").unwrap());
        assert_eq!(p.html, html);
        assert_eq!(p.text, text);
        assert!(!rx.is_in_flight());
    }

    #[test]
    fn rich_rx_html_rejects_bad_begin_and_bad_utf8() {
        let mut rx = RichRx::new();
        assert!(rx.begin_html("h".into(), 0, 5).is_err(), "zero html bytes");
        assert!(
            rx.begin_html("h".into(), CLIPBOARD_HTML_MAX_BYTES as u64, 1)
                .is_err(),
            "combined size over the cap"
        );
        assert!(!rx.is_in_flight());
        // Invalid UTF-8 in the html half is rejected at end().
        rx.begin_html("h2".into(), 2, 0).unwrap();
        rx.frame(&[0xff, 0xfe]).unwrap();
        let err = rx.end("h2").unwrap_err();
        assert!(err.contains("UTF-8"), "got: {err}");
    }

    // ── v2.2: native (RTF) lane ────────────────────────────────────────

    #[test]
    fn parse_incoming_native_begin_end() {
        let m: ClipboardIncoming = serde_json::from_str(
            r#"{"t":"clipboard:native-begin","id":"cb-n1","rtf_bytes":10,"html_bytes":4,"text_bytes":2}"#,
        )
        .unwrap();
        match m {
            ClipboardIncoming::NativeBegin {
                id,
                rtf_bytes,
                html_bytes,
                text_bytes,
            } => {
                assert_eq!(
                    (id.as_str(), rtf_bytes, html_bytes, text_bytes),
                    ("cb-n1", 10, 4, 2)
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:native-end","id":"cb-n1"}"#).unwrap();
        match m {
            ClipboardIncoming::NativeEnd { id } => assert_eq!(id, "cb-n1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rich_rx_native_splits_three_ways() {
        let mut rx = RichRx::new();
        let rtf = br"{\rtf1 hi}";
        rx.begin_native("n1".into(), rtf.len() as u64, 4, 2)
            .unwrap();
        let mut combined = rtf.to_vec();
        combined.extend_from_slice(b"<b>x");
        combined.extend_from_slice(b"hi");
        rx.frame(&combined[..7]).unwrap();
        rx.frame(&combined[7..]).unwrap();
        match rx.end("n1").unwrap() {
            RichPayload::Native(p) => {
                assert_eq!(p.rtf, rtf.to_vec());
                assert_eq!(p.html, "<b>x");
                assert_eq!(p.text, "hi");
            }
            other => panic!("expected native, got {other:?}"),
        }
        // Zero rtf_bytes and over-cap totals are rejected at begin.
        assert!(rx.begin_native("n2".into(), 0, 4, 2).is_err());
        assert!(
            rx.begin_native("n3".into(), CLIPBOARD_NATIVE_MAX_BYTES as u64, 1, 0)
                .is_err()
        );
    }

    #[test]
    fn native_payload_serde_base64_roundtrip() {
        // The bridge's JSON body: rtf travels base64; html/text default
        // to empty when omitted.
        let p = NativePayload {
            rtf: vec![0x7b, 0x5c, 0x72, 0x74, 0x66, 0xff],
            html: "<i>x</i>".into(),
            text: "x".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"rtf\":\"e1xydGb/\""), "got: {json}");
        let back: NativePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rtf, p.rtf);
        assert_eq!(back.html, p.html);
        assert_eq!(back.text, p.text);
        let sparse: NativePayload = serde_json::from_str(r#"{"rtf":"AQI="}"#).unwrap();
        assert_eq!(sparse.rtf, vec![1, 2]);
        assert_eq!(sparse.html, "");
        assert_eq!(sparse.text, "");
        assert!(serde_json::from_str::<NativePayload>(r#"{"rtf":"not base64!!"}"#).is_err());
    }

    // ── v2.1: html parse locks + event hash ────────────────────────────

    #[test]
    fn parse_incoming_html_begin_end() {
        let m: ClipboardIncoming = serde_json::from_str(
            r#"{"t":"clipboard:html-begin","id":"cb-9","html_bytes":120,"text_bytes":30}"#,
        )
        .unwrap();
        match m {
            ClipboardIncoming::HtmlBegin {
                id,
                html_bytes,
                text_bytes,
            } => {
                assert_eq!((id.as_str(), html_bytes, text_bytes), ("cb-9", 120, 30));
            }
            other => panic!("unexpected: {other:?}"),
        }
        let m: ClipboardIncoming =
            serde_json::from_str(r#"{"t":"clipboard:html-end","id":"cb-9"}"#).unwrap();
        match m {
            ClipboardIncoming::HtmlEnd { id } => assert_eq!(id, "cb-9"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn html_event_hash_separates_html_from_text_and_canonicalizes() {
        // Separator prevents boundary collisions…
        assert_ne!(html_event_hash("ab", "c"), html_event_hash("a", "bc"));
        // …and the text half is canonicalized like every other text
        // hash (CRLF == LF), while the html half is hashed verbatim.
        assert_eq!(
            html_event_hash("<p>x</p>", "l1\r\nl2"),
            html_event_hash("<p>x</p>", "l1\nl2")
        );
        assert_ne!(
            html_event_hash("<p>x</p>", "t"),
            html_event_hash("<p>y</p>", "t")
        );
    }

    /// #3813 — the real clipboard's gate, decided from values alone: only
    /// `1` opts in. Nothing here reads or sets the environment, or reaches a
    /// clipboard.
    #[test]
    fn only_a_one_opts_in_to_the_real_clipboard() {
        use std::ffi::OsStr;
        assert!(real_clipboard_opt_in(Some(OsStr::new("1"))), "1 runs");
        assert!(!real_clipboard_opt_in(None), "unset skips");
        for value in ["", "0", "true", "yes", "on", " 1", "1 ", "01", "11"] {
            assert!(
                !real_clipboard_opt_in(Some(OsStr::new(value))),
                "{value:?} must not opt in"
            );
        }
        // The name CI's steps and docs/testing.md set: a rename shows here.
        assert_eq!(REAL_CLIPBOARD_VAR, "ROOMLERD_TEST_REAL_CLIPBOARD");
    }

    /// A basic write/read round-trip works AND — locked in
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
    ///
    /// #3813 — it writes the REAL clipboard, so it runs only under
    /// `ROOMLERD_TEST_REAL_CLIPBOARD=1`, and a clipboard that then does not
    /// open is a failure. Before, it ran in every native `cargo test` that
    /// built `clipboard`, and replaced what a developer had copied; where no
    /// clipboard opened (CI's headless Linux lane) it skipped, so no CI step
    /// ever ran it against a clipboard.
    #[tokio::test]
    async fn write_then_read_round_trip_and_survives_clone_drop() {
        if !real_clipboard_opted_in("write_then_read_round_trip_and_survives_clone_drop") {
            return;
        }
        let cb = Clipboard::new()
            .unwrap_or_else(|e| panic!("{REAL_CLIPBOARD_VAR}=1, but no clipboard opened: {e:#}"));
        let payload = "roomler clipboard smoke test";
        cb.write(payload.to_string()).await.unwrap();
        // Soft read — another process may have already clobbered the
        // clipboard. Only enforce content equality when the read
        // actually returned our payload.
        let mut read_back = false;
        if let Ok(back) = cb.read().await {
            if back == payload {
                // Good — OS let us keep our own write.
                read_back = true;
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
        // CI's real-clipboard steps grep this line: the test got past its
        // gate, opened a clipboard, and its last write found the worker alive.
        eprintln!(
            "real-clipboard: the round trip ran and outlived a clone's drop (read its own write back: {read_back})"
        );
    }
}
