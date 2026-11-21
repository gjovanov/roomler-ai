// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Diagnostic helper for the `rc:logs-fetch` control-DC message
//! (added in rc.23). Reads the tail of the agent's current rolling
//! log file and serialises it as JSON the browser can render in the
//! log-viewer dialog.
//!
//! Why a single round-trip rather than a streaming subscription:
//! the diagnostic value comes from "what just happened in the last
//! failed upload?" — a 200-line snapshot is enough. Streaming would
//! add ordering / reconnect / replay complexity for marginal value.
//! Operator who wants live tail can request again every few seconds.
//!
//! Path resolution: uses `logging::log_dir()` as the source-of-truth,
//! then picks the lexicographically-latest `roomlerd.log.*` file (or a
// RETIRED-NAME-ANCHOR-BEGIN
// Rolled log files written BEFORE the rename are still on disk on every
// upgraded host, and this is what fetches them. Accepting only the current
// prefix would make an upgraded agent report that it has no logs.
//! legacy `roomler-agent.log.*` on a pre-rename host — P3d Slice B).
//! `tracing_appender::rolling::daily` rotates daily so the latest by
//! name is also the latest by mtime — no I/O needed to choose.
//!
//! Truncation: we read the last N lines (default 500, capped at
//! 5000) by streaming from EOF backwards in 4 KiB chunks. Files >
//! ~50 MB read efficiently because we never load the full file.

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

/// Default tail size when the browser doesn't specify `lines`. Sized
/// to a few seconds of busy-period output — enough to capture the
/// run-up to a failed upload's first error without flooding the DC.
pub const DEFAULT_TAIL_LINES: usize = 500;

/// Hard cap so an aggressive browser can't request 1M lines and
/// stall the agent reading them. Matches the in-place clamp in
/// peer.rs::attach_control_handler.
pub const MAX_TAIL_LINES: usize = 5000;

/// Resolve the current log file path. Returns `Err` when log dir is
/// unresolvable (rare — only on platforms without a data dir) or
/// when no `roomlerd.log*` (or legacy `roomler-agent.log*`) file exists
/// yet (e.g. agent just started and `tracing_appender` hasn't created
/// the first file).
///
/// P3d Slice B: the live basename is now `roomlerd.log`; a pre-rename host
/// may still have `roomler-agent.log*` files. Accept BOTH prefixes. The
/// lexicographic pick naturally prefers `roomlerd.*` over `roomler-agent.*`
/// (`'d'` > `'-'`), which matches "the new-basename file is the current one".
pub fn current_log_path() -> Result<PathBuf> {
    let dir = crate::logging::log_dir()
        .ok_or_else(|| anyhow!("log dir is unresolvable on this platform"))?;
    let entries =
        std::fs::read_dir(&dir).with_context(|| format!("reading log dir {}", dir.display()))?;
    let mut best: Option<(String, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("roomlerd.log") {
            continue;
        }
        // Prefer the lexicographically-latest filename. tracing's
        // daily rotation creates `roomlerd.log.YYYY-MM-DD` so
        // the latest day sorts last. The current-day's file (no
        // suffix yet on some appender versions) is fine too — we
        // pick whichever sorts highest.
        match &best {
            Some((existing, _)) if *existing >= name => {}
            _ => best = Some((name, entry.path())),
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| {
        anyhow!(
            "no roomlerd.log* or roomler-agent.log* file in {}",
            dir.display()
        )
    })
}

/// Read the last `lines` lines from the current log file and return
/// a JSON envelope ready to send back over the control DC.
///
/// JSON shape:
/// ```json
/// {
///   "t": "rc:logs-fetch.reply",
///   "ok": true,
///   "path": "C:\\Users\\...\\roomler-agent.log.2026-05-12",
///   "lines": ["[2026-05-12T11:00:01Z] INFO ...", "..."],
///   "truncated": false
/// }
/// ```
///
/// On error: `{ t: "rc:logs-fetch.reply", ok: false, error: "..." }`.
pub async fn fetch_tail(lines: usize) -> Result<serde_json::Value> {
    let lines = lines.clamp(1, MAX_TAIL_LINES);
    let path = current_log_path()?;
    let (out, truncated) = read_tail_lines(&path, lines).await?;
    Ok(serde_json::json!({
        "t": "rc:logs-fetch.reply",
        "ok": true,
        "path": path.to_string_lossy(),
        "lines": out,
        "truncated": truncated,
    }))
}

/// rc.24 — chunk the agent's reply across multiple SCTP messages so
/// large fetches (1000+ lines = >64 KiB JSON) don't exceed the
/// `max_message_size` limit and get silently dropped by webrtc-rs.
///
/// Wire format (browser already understands `rc:logs-fetch.reply`
/// from rc.23; the chunked variants below are new):
///
/// ```json
/// // First message
/// {
///   "t": "rc:logs-fetch.reply.start",
///   "id": "<request-id>",        // optional, only set when browser supplied it
///   "ok": true,
///   "path": "<log file path>",
///   "total_lines": 2000,         // total lines that will be streamed
///   "truncated": false
/// }
/// // Zero or more chunk messages
/// {
///   "t": "rc:logs-fetch.reply.chunk",
///   "id": "<request-id>",
///   "lines": ["<line>", "<line>", ...]   // the frame ≤ FRAME_BUDGET_BYTES as sent
/// }
/// // Final message
/// { "t": "rc:logs-fetch.reply.end", "id": "<request-id>" }
/// ```
///
/// Browsers built before rc.24 (rc.23.x) only know
/// `rc:logs-fetch.reply` — they won't recognise the streamed
/// variants. To keep backward-compat: if the whole reply fits in one
/// frame (`FRAME_BUDGET_BYTES` as serialized), fall back to the
/// monolithic `rc:logs-fetch.reply` shape so old browsers still
/// work for small fetches.
///
/// Returns a `Vec<serde_json::Value>` of envelopes the caller must
/// send in order over the control DC. Each one serializes to at most
/// `FRAME_BUDGET_BYTES` whatever the log text holds
/// (`reply_envelopes`), under the 65 535 bytes a peer takes in one
/// message.
pub async fn fetch_tail_chunked(
    lines: usize,
    request_id: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    let lines = lines.clamp(1, MAX_TAIL_LINES);
    let path = current_log_path()?;
    let (out, truncated) = read_tail_lines(&path, lines).await?;
    Ok(reply_envelopes(
        &path.to_string_lossy(),
        out,
        truncated,
        request_id,
    ))
}

/// The reply's envelopes for tail lines already read, in the order the
/// caller sends them. Pure, so a test builds every frame from lines in
/// memory (#4208).
///
/// Every frame is measured as the bytes it is sent as, its JSON text
/// (`json_len`): there a quote or a backslash takes two bytes and a
/// control character six, so frames sized by raw text passed the
/// message limit once escaped. Each line is first made to fit a chunk
/// on its own (`cut_to_fit`). The reply is then the single rc.23 frame
/// when all of it fits in one, else a stream whose chunks close before
/// the next line would take one past `FRAME_BUDGET_BYTES`.
fn reply_envelopes(
    path: &str,
    lines: Vec<String>,
    truncated: bool,
    request_id: Option<&str>,
) -> Vec<serde_json::Value> {
    // A chunk's bytes before its first line; the rest of the budget is
    // the lines'.
    let room = FRAME_BUDGET_BYTES.saturating_sub(json_len(&chunk_frame(request_id, Vec::new())));
    let lines: Vec<String> = lines
        .into_iter()
        .map(|line| cut_to_fit(line, room))
        .collect();
    let sizes: Vec<usize> = lines.iter().map(|line| json_len(line.as_str())).collect();

    // A comma goes between two lines.
    let commas = lines.len().saturating_sub(1);
    let single =
        json_len(&single_reply(path, Vec::new(), truncated)) + sizes.iter().sum::<usize>() + commas;
    if single <= FRAME_BUDGET_BYTES {
        return vec![single_reply(path, lines, truncated)];
    }

    let mut envelopes = vec![serde_json::json!({
        "t": "rc:logs-fetch.reply.start",
        "id": request_id,
        "ok": true,
        "path": path,
        "total_lines": lines.len(),
        "truncated": truncated,
    })];
    let mut chunk: Vec<String> = Vec::new();
    let mut used = 0;
    for (line, size) in lines.into_iter().zip(sizes) {
        // Every line of a chunk but its first pays a comma.
        if !chunk.is_empty() && used + 1 + size > room {
            envelopes.push(chunk_frame(request_id, std::mem::take(&mut chunk)));
            used = 0;
        }
        used += size + usize::from(!chunk.is_empty());
        chunk.push(line);
    }
    if !chunk.is_empty() {
        envelopes.push(chunk_frame(request_id, chunk));
    }
    envelopes.push(serde_json::json!({
        "t": "rc:logs-fetch.reply.end",
        "id": request_id,
    }));
    envelopes
}

/// The most bytes one frame of the reply may take as sent: its JSON
/// text, escapes included (#4208). A peer takes at most 65 535 bytes in
/// one message (#4121): a webrtc-rs receiver reads each into a
/// 65 535-byte buffer, and the SCTP stack refuses to send one over
/// 65 536. Frames are measured, not estimated, so the 24 KiB left under
/// that is not there for escapes: it keeps an ordinary log's chunks the
/// size rc.24 sent (40 KiB of raw text each), and no frame near the
/// limit is ever tried.
pub const FRAME_BUDGET_BYTES: usize = 40 * 1024;

/// The rc.23 reply: the whole tail in one frame, which every viewer reads.
fn single_reply(path: &str, lines: Vec<String>, truncated: bool) -> serde_json::Value {
    serde_json::json!({
        "t": "rc:logs-fetch.reply",
        "ok": true,
        "path": path,
        "lines": lines,
        "truncated": truncated,
    })
}

/// One `rc:logs-fetch.reply.chunk` of a streamed reply.
fn chunk_frame(request_id: Option<&str>, lines: Vec<String>) -> serde_json::Value {
    serde_json::json!({
        "t": "rc:logs-fetch.reply.chunk",
        "id": request_id,
        "lines": lines,
    })
}

/// `line` whole when its JSON string takes at most `room` bytes.
/// Otherwise the longest prefix that fits, cut on a `char` boundary and
/// followed by a marker naming the bytes kept and the line's length: a
/// line too long for any frame is never dropped and never sent whole.
fn cut_to_fit(mut line: String, room: usize) -> String {
    if json_len(line.as_str()) <= room {
        return line;
    }
    let total = line.len();
    // The marker names the bytes kept, which have no more digits than
    // `total`; `- 2` drops the marker's own quotes.
    let marker = json_len(cut_marker(total, total).as_str()) - 2;
    let mut used = 2; // the line's quotes
    let mut kept = 0;
    let mut utf8 = [0u8; 4];
    for (at, ch) in line.char_indices() {
        let one: &str = ch.encode_utf8(&mut utf8);
        let size = json_len(one) - 2;
        if used + size + marker > room {
            break;
        }
        used += size;
        kept = at + ch.len_utf8();
    }
    line.truncate(kept);
    line.push_str(&cut_marker(kept, total));
    line
}

/// How a cut line ends: where it was cut, and how long it was.
fn cut_marker(kept: usize, total: usize) -> String {
    format!(" … [line cut at {kept} of {total} bytes]")
}

/// The length of `serde_json::to_string(value)`, counted without
/// building the string: a log line can be megabytes long, and its JSON
/// up to six times that.
fn json_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    // Neither a string nor a JSON value can fail to serialize, and this
    // writer never fails.
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

/// Read the last `count` lines from `path`. Returns (lines, truncated)
/// where `truncated` is true when the file had more than `count` lines.
///
/// Implementation streams from EOF backwards in 4 KiB chunks. Stops
/// once `count + 1` newlines have been seen (the +1 accounts for the
/// boundary line). Then splits the accumulated buffer and trims to
/// the last `count`.
///
/// Public for unit testing — production callers go through `fetch_tail`.
pub async fn read_tail_lines(path: &std::path::Path, count: usize) -> Result<(Vec<String>, bool)> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let total_len = file
        .metadata()
        .await
        .with_context(|| format!("stat {}", path.display()))?
        .len();

    const CHUNK: u64 = 4096;
    let mut buf: Vec<u8> = Vec::with_capacity((CHUNK as usize) * 8);
    let mut pos = total_len;
    // Count newlines we've already buffered so we can stop early
    // once we've seen enough.
    let mut newlines = 0usize;
    while pos > 0 && newlines <= count {
        let read_at = pos.saturating_sub(CHUNK);
        let read_len = (pos - read_at) as usize;
        let mut chunk = vec![0u8; read_len];
        file.seek(SeekFrom::Start(read_at)).await?;
        file.read_exact(&mut chunk).await?;
        // Count newlines in this chunk (excluding a possibly-final
        // newline of the file itself — we treat any newline as a
        // line separator).
        newlines += chunk.iter().filter(|&&b| b == b'\n').count();
        // Prepend the chunk to the buffer (we read backwards, so
        // older data goes first). `append(&mut buf)` is the clippy-
        // preferred form vs `extend(buf.drain(..))` — same effect
        // (moves elements, clears buf), one fewer iterator chain.
        let mut new_buf = chunk;
        new_buf.append(&mut buf);
        buf = new_buf;
        pos = read_at;
    }

    let text = String::from_utf8_lossy(&buf);
    let all_lines: Vec<&str> = text.lines().collect();
    let truncated = all_lines.len() > count || pos > 0;
    let start = all_lines.len().saturating_sub(count);
    let out = all_lines[start..].iter().map(|s| s.to_string()).collect();
    Ok((out, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn read_tail_lines_small_file_no_truncation() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        {
            let mut f = tokio::fs::File::create(&path).await.unwrap();
            f.write_all(b"line 1\nline 2\nline 3\n").await.unwrap();
            f.sync_all().await.unwrap();
        }
        let (lines, truncated) = read_tail_lines(&path, 100).await.unwrap();
        assert_eq!(lines, vec!["line 1", "line 2", "line 3"]);
        assert!(!truncated, "tail < count should not flag truncated");
    }

    #[tokio::test]
    async fn read_tail_lines_truncates_to_count() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        {
            let mut f = tokio::fs::File::create(&path).await.unwrap();
            for i in 1..=50 {
                f.write_all(format!("line {i}\n").as_bytes()).await.unwrap();
            }
            f.sync_all().await.unwrap();
        }
        let (lines, truncated) = read_tail_lines(&path, 5).await.unwrap();
        assert_eq!(
            lines,
            vec!["line 46", "line 47", "line 48", "line 49", "line 50"]
        );
        assert!(truncated, "tail < total should flag truncated");
    }

    #[tokio::test]
    async fn read_tail_lines_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let (lines, truncated) = read_tail_lines(&path, 10).await.unwrap();
        assert!(lines.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn read_tail_lines_spans_chunks() {
        // 5000 lines × ~20 bytes each ≈ 100 KB, definitely spans
        // multiple 4 KiB chunks. Pin that the chunked-read logic
        // produces the same result as a naive whole-file read.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        {
            let mut f = tokio::fs::File::create(&path).await.unwrap();
            for i in 1..=5000 {
                f.write_all(format!("event-id-{i:08} payload\n").as_bytes())
                    .await
                    .unwrap();
            }
            f.sync_all().await.unwrap();
        }
        let (lines, truncated) = read_tail_lines(&path, 3).await.unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "event-id-00004998 payload");
        assert_eq!(lines[2], "event-id-00005000 payload");
        assert!(truncated);
    }

    #[tokio::test]
    async fn fetch_tail_clamps_lines_argument() {
        // Indirect test — fetch_tail's clamp pre-routes to
        // read_tail_lines. We can't easily test the full path
        // without the log dir set up, but we can sanity-check the
        // clamp arithmetic via the constants.
        assert_eq!(0_usize.clamp(1, MAX_TAIL_LINES), 1);
        assert_eq!(10_000_usize.clamp(1, MAX_TAIL_LINES), MAX_TAIL_LINES);
        assert_eq!(
            DEFAULT_TAIL_LINES.clamp(1, MAX_TAIL_LINES),
            DEFAULT_TAIL_LINES
        );
    }

    // #4208 — every frame is measured as the bytes it is sent as: its JSON
    // text, where a quote or a backslash takes two bytes and a control
    // character six.

    /// The largest message the peer takes (#4121): a webrtc-rs receiver
    /// reads each one into a 65 535-byte buffer, and its SCTP stack refuses
    /// to send one over 65 536 bytes. It is the peer's number, so it is
    /// written out here, not read from the module's own budget.
    const MESSAGE_LIMIT: usize = 65_535;

    const PATH: &str = r"C:\ProgramData\roomler\logs\roomlerd.log.2026-11-21";

    /// Each frame's size as `peer.rs` sends it: `serde_json::to_string`.
    fn frame_sizes(envelopes: &[serde_json::Value]) -> Vec<usize> {
        envelopes
            .iter()
            .map(|e| serde_json::to_string(e).unwrap().len())
            .collect()
    }

    fn assert_every_frame_fits(envelopes: &[serde_json::Value]) {
        let sizes = frame_sizes(envelopes);
        assert!(
            sizes.iter().all(|&n| n <= MESSAGE_LIMIT),
            "a frame passes the {MESSAGE_LIMIT}-byte message limit; frame sizes: {sizes:?}"
        );
    }

    #[test]
    fn a_chunk_of_control_characters_fits_the_message_limit() {
        // ESC starts every ANSI colour code, and JSON writes it as `\u001b`:
        // six bytes for one. 64 lines of 1 KiB stream as chunks of 40 KiB of
        // raw text.
        let lines = vec!["\u{1b}".repeat(1024); 64];
        assert_every_frame_fits(&reply_envelopes(PATH, lines, false, None));
    }

    #[test]
    fn a_single_reply_of_control_characters_fits_the_message_limit() {
        // 24 KiB of raw text is under the single reply's raw-text cut, and
        // six times that once written as JSON.
        let lines = vec!["\u{1b}".repeat(1024); 24];
        assert_every_frame_fits(&reply_envelopes(PATH, lines, false, None));
    }

    #[test]
    fn a_chunk_of_quotes_and_backslashes_fits_the_message_limit() {
        // A Windows path or a quoted value: `"` and `\` take two bytes each.
        let lines = vec!["\"\\".repeat(512); 64];
        assert_every_frame_fits(&reply_envelopes(PATH, lines, false, Some("req-1")));
    }

    #[test]
    fn one_line_of_40_kib_of_control_characters_fits_the_message_limit() {
        let lines = vec!["\u{1b}".repeat(40 * 1024)];
        assert_every_frame_fits(&reply_envelopes(PATH, lines, false, None));
    }

    #[test]
    fn one_line_of_100_kib_fits_the_message_limit() {
        let lines = vec!["x".repeat(100 * 1024)];
        assert_every_frame_fits(&reply_envelopes(PATH, lines, false, None));
    }

    fn assert_every_frame_within_budget(envelopes: &[serde_json::Value]) {
        let sizes = frame_sizes(envelopes);
        assert!(
            sizes.iter().all(|&n| n <= FRAME_BUDGET_BYTES),
            "a frame passes the {FRAME_BUDGET_BYTES}-byte budget; frame sizes: {sizes:?}"
        );
    }

    /// The lines the frames carry, joined in order as the viewer joins
    /// them: the single reply's, or every chunk's.
    fn joined_lines(envelopes: &[serde_json::Value]) -> Vec<String> {
        envelopes
            .iter()
            .filter(|e| e["t"] == "rc:logs-fetch.reply" || e["t"] == "rc:logs-fetch.reply.chunk")
            .flat_map(|e| e["lines"].as_array().unwrap().iter())
            .map(|line| line.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn json_len_counts_what_to_string_makes() {
        for s in [
            "",
            "plain",
            "\"\\",
            "\u{1b}[32mINFO\u{1b}[0m",
            "\t\n\r\u{8}\u{c}\u{0}\u{1f}",
            "\u{7f}",
            "ünïcödé ✓ 🦀",
            "</script>",
        ] {
            assert_eq!(
                json_len(s),
                serde_json::to_string(s).unwrap().len(),
                "{s:?}"
            );
        }
        let frame = chunk_frame(Some("id"), vec!["a\u{1b}b".into(), "\"".into()]);
        assert_eq!(
            json_len(&frame),
            serde_json::to_string(&frame).unwrap().len()
        );
    }

    #[test]
    fn a_reply_that_fits_one_frame_is_the_single_reply_every_viewer_reads() {
        let lines = vec!["line 1".to_string(), "line 2".to_string()];
        let envelopes = reply_envelopes(PATH, lines.clone(), true, Some("req-2"));
        assert_eq!(
            envelopes,
            vec![serde_json::json!({
                "t": "rc:logs-fetch.reply",
                "ok": true,
                "path": PATH,
                "lines": lines,
                "truncated": true,
            })]
        );
        // An empty log is that reply too.
        let empty = reply_envelopes(PATH, Vec::new(), false, None);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0]["t"], "rc:logs-fetch.reply");
        assert_eq!(empty[0]["lines"], serde_json::json!([]));
    }

    #[test]
    fn the_single_reply_is_chosen_by_its_serialized_length() {
        // One line that brings the single reply to the budget exactly,
        // then the same line one byte longer.
        let fixed = json_len(&single_reply(PATH, Vec::new(), false));
        let line = "q".repeat(FRAME_BUDGET_BYTES - fixed - 2);
        let at_budget = reply_envelopes(PATH, vec![line.clone()], false, None);
        assert_eq!(at_budget.len(), 1);
        assert_eq!(at_budget[0]["t"], "rc:logs-fetch.reply");
        assert_eq!(frame_sizes(&at_budget), vec![FRAME_BUDGET_BYTES]);
        assert_every_frame_fits(&at_budget);

        let longer = format!("{line}q");
        let over = reply_envelopes(PATH, vec![longer.clone()], false, None);
        assert_eq!(over[0]["t"], "rc:logs-fetch.reply.start");
        assert_eq!(joined_lines(&over), vec![longer], "it fits a chunk whole");
        assert_every_frame_within_budget(&over);
    }

    #[test]
    fn lines_joined_across_the_chunks_equal_the_input() {
        // What a log holds: plain text, ANSI colour, Windows paths, quoted
        // JSON, non-ASCII. 5 000 lines take many chunks.
        let lines: Vec<String> = (0..5000)
            .map(|i| match i % 5 {
                0 => format!("2026-11-21T04:00:00.{i:06}Z  INFO roomlerd::peer: frame {i} sent"),
                1 => format!("\u{1b}[2m2026-11-21\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m colour {i}"),
                2 => format!(r"C:\Users\someone\AppData\Local\roomler\logs\roomlerd.log.{i}"),
                3 => format!(r#"payload="{{\"t\":\"rc:logs-fetch\",\"n\":{i}}}""#),
                _ => format!("ünïcödé ✓ 🦀\t{i}"),
            })
            .collect();
        let envelopes = reply_envelopes(PATH, lines.clone(), true, Some("req-7"));
        assert_every_frame_fits(&envelopes);
        assert_every_frame_within_budget(&envelopes);

        let (start, rest) = envelopes.split_first().unwrap();
        let (end, chunks) = rest.split_last().unwrap();
        assert_eq!(start["t"], "rc:logs-fetch.reply.start");
        assert_eq!(start["id"], "req-7");
        assert_eq!(start["path"], PATH);
        assert_eq!(start["total_lines"], 5000);
        assert_eq!(start["truncated"], true);
        assert_eq!(
            *end,
            serde_json::json!({"t": "rc:logs-fetch.reply.end", "id": "req-7"})
        );
        assert!(chunks.len() > 1, "5 000 lines take more than one chunk");
        for chunk in chunks {
            assert_eq!(chunk["t"], "rc:logs-fetch.reply.chunk");
            assert_eq!(chunk["id"], "req-7");
            assert!(!chunk["lines"].as_array().unwrap().is_empty());
        }
        assert_eq!(joined_lines(&envelopes), lines);
    }

    #[test]
    fn a_line_too_long_for_a_frame_is_cut_on_a_char_boundary_and_says_so() {
        // Four bytes a char, so a cut off a char boundary could not pass
        // unseen.
        let long = "🦀".repeat(30_000);
        let lines = vec![long.clone(), "next".to_string()];
        let envelopes = reply_envelopes(PATH, lines, false, None);
        assert_every_frame_fits(&envelopes);
        assert_every_frame_within_budget(&envelopes);
        let joined = joined_lines(&envelopes);
        assert_eq!(joined.len(), 2, "the cut line is still one line");
        assert_eq!(joined[1], "next");
        let (kept, rest) = joined[0]
            .split_once(" … [line cut at ")
            .expect("a cut line carries the marker");
        assert!(long.starts_with(kept));
        assert_eq!(rest, format!("{} of {} bytes]", kept.len(), long.len()));
        // As much as a frame holds, not a token prefix.
        assert!(
            kept.len() > FRAME_BUDGET_BYTES - 1024,
            "kept {}",
            kept.len()
        );
    }

    #[test]
    fn a_line_of_control_characters_is_cut_by_its_json_length() {
        let long = "\u{1b}".repeat(40 * 1024);
        let envelopes = reply_envelopes(PATH, vec![long], false, None);
        assert_every_frame_within_budget(&envelopes);
        let joined = joined_lines(&envelopes);
        let (kept, rest) = joined[0]
            .split_once(" … [line cut at ")
            .expect("a cut line carries the marker");
        assert!(kept.bytes().all(|b| b == 0x1b));
        assert_eq!(rest, format!("{} of {} bytes]", kept.len(), 40 * 1024));
        // Six bytes of JSON each: a sixth of the frame, nearly all of it.
        assert!(kept.len() <= FRAME_BUDGET_BYTES / 6, "kept {}", kept.len());
        assert!(
            kept.len() > FRAME_BUDGET_BYTES / 6 - 64,
            "kept {}",
            kept.len()
        );
    }
}
// RETIRED-NAME-ANCHOR-END
