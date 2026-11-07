// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Per-session counters for `rc:session.stats` (wave 2).
//!
//! `remote_sessions.stats` has been declared-but-never-written since
//! Phase 4 — every recorded session carries zeros. The transport numbers
//! (bytes, RTT) come from the peer connection's own `get_stats()`, but
//! two of the interesting values are produced deep inside the media pump
//! and the input handler, which are free functions keyed by session id
//! rather than methods on [`crate::peer::AgentPeer`].
//!
//! A tiny process-global registry keyed by session id is the least
//! invasive way to collect them: the producers bump a counter (one
//! relaxed atomic add on paths that already do far more work), and the
//! reporter reads them. Entries are dropped with their `AgentPeer`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_remote_control::models::SessionVideo;
use roomler_ai_remote_control::signaling::{FileNote, Tally, TransferDirection, Transfers};
use sha2::{Digest, Sha256};

/// FR-101 P3 — the most file notes one session holds between two samples.
/// A sample goes out every 30 s and drains them; past this, a note is
/// dropped and logged rather than letting a flood of tiny files grow the
/// daemon's memory without bound.
const MAX_PENDING_FILE_NOTES: usize = 512;

/// How many, and how many bytes, as two counters a producer bumps.
#[derive(Default, Debug)]
pub struct TallyCounter {
    count: AtomicU64,
    bytes: AtomicU64,
}

impl TallyCounter {
    pub fn add(&self, bytes: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn read(&self) -> Tally {
        Tally {
            count: self.count.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn add_tally(&self, t: Tally) {
        self.count.fetch_add(t.count, Ordering::Relaxed);
        self.bytes.fetch_add(t.bytes, Ordering::Relaxed);
    }
}

#[derive(Default, Debug)]
pub struct SessionCounters {
    /// Keyframes the viewer asked for (PLI/FIR + our own resync pulses).
    /// A session with a high count spent it recovering from loss.
    pub keyframe_requests: AtomicU32,
    /// FR-132 P1 (§3b) — decode errors the viewer reported (`rc:decode-error`,
    /// one per error, sent before it falls back to `<video>`). Read through
    /// [`Self::decode_errors`], never directly: a zero means something only
    /// once [`Self::decode_errors_counted`] is set.
    decode_errors: AtomicU32,
    /// FR-132 P1 — the viewer said it counts decode errors (`de` on its
    /// `rc:decodestat`, or a reported error). Without it a session's zero is
    /// "not counted": an older viewer, or one that never decoded in a worker.
    decode_errors_counted: AtomicBool,
    /// Input events accepted for injection (post-suppression).
    pub input_events: AtomicU64,
    /// FR-101 P4b-2 — every input event the controller SENT, suppressed or
    /// not: someone typing at a locked host, or moving the pointer in a
    /// view-only session, is present even though nothing was injected. What
    /// the idle limit reads; `input_events` stays what the stats report.
    pub input_seen: AtomicU64,
    /// P8 Phase 4 — cumulative seconds the owner's shared pipeline
    /// served ≥1 follower (bumped by the pump heartbeats; counted on
    /// the OWNER session, whose pump runs the pipeline).
    pub shared_seconds: AtomicU64,
    /// … of which the viewers' dials (Priority / resolution) were NOT
    /// all equal — the SVC go/no-go dataset (plan Phase 4 gate).
    pub mixed_dial_seconds: AtomicU64,
    /// FR-101 P3 — writes the controller made to this host's clipboard.
    pub clipboard_to_host: TallyCounter,
    /// FR-101 P3 — clipboard content this host sent the controller.
    pub clipboard_from_host: TallyCounter,
    /// FR-101 P3 — files completed since the last sample. A name is hashed
    /// the moment it is noted, so no name is ever held here.
    file_notes: std::sync::Mutex<Vec<FileNote>>,
    /// FR-101 P4b-2 — file-transfer activity: a message on the files channel
    /// from the controller, or a download's progress report. Read only as
    /// "did it move" by the idle limit (`session_idle`), so a transfer that
    /// runs for longer than the limit is never ended as idle.
    pub transfer_activity: AtomicU64,
    /// FR-132 P1d — what the session's video ran on, the pump's newest
    /// reading ([`Self::note_video`]). `None` before its first heartbeat, and
    /// on a pump with no input form to name.
    video: std::sync::Mutex<Option<VideoForm>>,
    /// FR-135 R0 (N16) — the session's media summary, folded from its pump's
    /// windows ([`Self::note_media_fold`]). `None` until the first fold, and
    /// with `stats` on the session's old path.
    media: std::sync::Mutex<Option<MediaTally>>,
    /// FR-135 R0 (N12) — the viewer's `rc:keyframe` asks, and how many of them
    /// the 500 ms gap dropped (R1's slow recoveries pointed at that gap, and
    /// no counter confirmed it).
    kf_asks: AtomicU32,
    kf_asks_gapped: AtomicU32,
    /// When the last ask the gap let through arrived, as `kf_ask_epoch`
    /// micros plus one; 0 = none waiting. The pump takes it when the key frame
    /// that answers it is queued ([`Self::take_keyframe_ask`]).
    kf_ask_at: AtomicU64,
    /// FR-135 R0 (N9-N12, N16) — what the session's viewer said on its
    /// `rc:decodestat`s ([`Self::note_viewer_report`]). Empty until a viewer
    /// that counts reports, and with `stats` on the session's old path, whose
    /// control handler keeps nothing.
    viewer: std::sync::Mutex<ViewerTally>,
}

/// FR-135 R0 — the viewer's reports, kept between two heartbeats.
#[derive(Debug, Default)]
struct ViewerTally {
    /// The viewer's windows since the heartbeat last took them, summed.
    window: crate::media_window::ViewerReport,
    windows: u32,
    /// The viewer's counts as last reported, kept across takes (only the
    /// counts are ever set here).
    latest: crate::media_window::ViewerReport,
    /// A viewer that counts has reported.
    counted: bool,
}

/// FR-135 R0 — the viewer's windows since the heartbeat last took them, with
/// its counts as last reported, for the FFmpeg heartbeat's `viewer_*` fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewerWindow {
    /// How many of the viewer's windows (one a second) the histograms sum: 0
    /// when it sent none since the last heartbeat, and the histograms are then
    /// `None`.
    pub windows: u32,
    pub report: crate::media_window::ViewerReport,
}

/// FR-135 R0 (N16) — what a session's windows add up to, beside P1d's reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaTally {
    /// Windows capturing 5 or more frames a second (FR-135 §2.1), and the rest.
    pub moving_ms: u64,
    pub sparse_ms: u64,
    pub frames_encoded: u64,
    pub frames_keepalive: u64,
    /// The most followers the session's pipeline served at once.
    pub followers: u32,
    /// From the pump's start to the first frame in the channel's queue.
    pub first_frame_ms: Option<u32>,
    /// Windows folded so far. The fold's fields are absent until one has, so a
    /// first frame noted before the first fold adds no zeros.
    pub folds: u32,
}

/// The clock the keyframe ask's arrival is kept in: microseconds since the
/// first call, which only ever compares two readings of itself.
fn kf_ask_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// FR-132 P1d — one pump reading, in the names the heartbeat logs. `'static`
/// on purpose: a heartbeat should not allocate on the media path to say what
/// it said two seconds ago.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoForm {
    pub encoder: &'static str,
    /// The capturer's `backend()`.
    pub capture: &'static str,
    pub input_path: &'static str,
    /// Empty on GPU input, as `InputForm::reason` is.
    pub input_reason: &'static str,
    /// FR-135 R0 — `yuv420` or `yuv444`: with `encoder`, the cell. Empty from a
    /// pump that does not know, and then not sent.
    pub chroma: &'static str,
}

impl SessionCounters {
    /// FR-132 P1d — the pump's newest reading, for `rc:session.stats`. True
    /// when it differs from the last one, and the pump then asks for a sample
    /// at once: a session can end before the next 30 s tick, and the last
    /// sample of one the server ended arrives too late to reach its row.
    ///
    /// A CPU form always says why, so CPU with no reason is `InputForm`'s
    /// default, from a heartbeat whose `rate_stats` failed. That says nothing
    /// about the session, and must not replace a reading that does.
    pub fn note_video(&self, form: VideoForm) -> bool {
        if form.input_path == "cpu" && form.input_reason.is_empty() {
            return false;
        }
        let mut video = self.video.lock().unwrap_or_else(|p| p.into_inner());
        video.replace(form) != Some(form)
    }

    /// FR-132 P1d — what `rc:session.stats` carries. FR-135 R0 (N16) adds the
    /// session's media summary to the same reading once the pump has folded a
    /// window; without one those fields are absent, never zero. Every count
    /// saturates at the bound the server checks (`SessionVideo::checked`), so
    /// a session past one (an hour's wait for its first frame) costs that
    /// count's precision and never the reading, which the server drops whole.
    pub fn video(&self) -> Option<SessionVideo> {
        let form = (*self.video.lock().unwrap_or_else(|p| p.into_inner()))?;
        let media = *self.media.lock().unwrap_or_else(|p| p.into_inner());
        let folded = media.filter(|m| m.folds > 0);
        let secs = |ms: u64| (ms / 1000).min(u64::from(SessionVideo::MAX_SECS)) as u32;
        let viewer = self.viewer.lock().unwrap_or_else(|p| p.into_inner()).latest;
        Some(SessionVideo {
            encoder: form.encoder.to_string(),
            capture: form.capture.to_string(),
            input_path: form.input_path.to_string(),
            input_reason: (!form.input_reason.is_empty()).then(|| form.input_reason.to_string()),
            chroma: (!form.chroma.is_empty()).then(|| form.chroma.to_string()),
            moving_s: folded.map(|m| secs(m.moving_ms)),
            sparse_s: folded.map(|m| secs(m.sparse_ms)),
            frames_encoded: folded.map(|m| m.frames_encoded.min(SessionVideo::MAX_FRAMES)),
            frames_keepalive: folded.map(|m| m.frames_keepalive.min(SessionVideo::MAX_FRAMES)),
            followers: folded.map(|m| m.followers.min(SessionVideo::MAX_FOLLOWERS)),
            first_frame_ms: media
                .and_then(|m| m.first_frame_ms)
                .map(|ms| ms.min(SessionVideo::MAX_FIRST_FRAME_MS)),
            // FR-135 R0 (N16, N11) — the viewer's own counts, bounded on intake
            // (`ViewerReport::parse`); absent until a viewer that counts says.
            frames_painted: viewer.painted,
            freezes: viewer.freezes,
        })
    }

    /// FR-135 R0 — one `rc:decodestat`'s viewer numbers. A report with none
    /// (an older viewer) leaves everything as it was, so its session's fields
    /// stay absent rather than read zero.
    pub fn note_viewer_report(&self, report: crate::media_window::ViewerReport) {
        if report.is_empty() {
            return;
        }
        let mut v = self.viewer.lock().unwrap_or_else(|p| p.into_inner());
        v.window.fold(&report);
        v.windows = v.windows.saturating_add(1);
        v.latest.fold(&crate::media_window::ViewerReport {
            chunk_gaps: report.chunk_gaps,
            freezes: report.freezes,
            painted: report.painted,
            decoder_hw: report.decoder_hw,
            ..Default::default()
        });
        v.counted = true;
    }

    /// FR-135 R0 — the viewer's windows since the last take, with its counts
    /// as last reported, starting the next window. `None` until a viewer that
    /// counts has reported.
    pub fn take_viewer_window(&self) -> Option<ViewerWindow> {
        let mut v = self.viewer.lock().unwrap_or_else(|p| p.into_inner());
        if !v.counted {
            return None;
        }
        let window = std::mem::take(&mut v.window);
        let windows = std::mem::take(&mut v.windows);
        let latest = v.latest;
        Some(ViewerWindow {
            windows,
            report: crate::media_window::ViewerReport {
                chunk_gaps: latest.chunk_gaps,
                freezes: latest.freezes,
                painted: latest.painted,
                decoder_hw: latest.decoder_hw,
                ..window
            },
        })
    }

    /// FR-135 R0 (N16) — one window of the pump's, folded into the session's
    /// summary, with the followers its pipeline serves now.
    pub fn note_media_fold(&self, fold: crate::media_window::FoldReport, followers: u32) {
        let mut media = self.media.lock().unwrap_or_else(|p| p.into_inner());
        let m = media.get_or_insert_with(MediaTally::default);
        if fold.moving {
            m.moving_ms = m.moving_ms.saturating_add(fold.ms);
        } else {
            m.sparse_ms = m.sparse_ms.saturating_add(fold.ms);
        }
        m.frames_encoded = m.frames_encoded.saturating_add(fold.frames_encoded);
        m.frames_keepalive = m.frames_keepalive.saturating_add(fold.frames_keepalive);
        m.followers = m.followers.max(followers);
        m.folds = m.folds.saturating_add(1);
    }

    /// FR-135 R0 (N16) — the session's first frame reached the channel's queue
    /// `after` the pump started. The first call wins.
    pub fn note_first_frame(&self, after: std::time::Duration) {
        let mut media = self.media.lock().unwrap_or_else(|p| p.into_inner());
        let m = media.get_or_insert_with(MediaTally::default);
        if m.first_frame_ms.is_none() {
            m.first_frame_ms = Some(after.as_millis().min(u32::MAX as u128) as u32);
        }
    }

    /// FR-135 R0 (N12) — a viewer's `rc:keyframe`, let through by the gap or
    /// dropped by it. One the gap let through starts the wait the pump reads
    /// when the key frame that answers it is queued.
    pub fn note_keyframe_ask(&self, allowed: bool) {
        self.kf_asks.fetch_add(1, Ordering::Relaxed);
        if allowed {
            let at = kf_ask_epoch()
                .elapsed()
                .as_micros()
                .min(u64::MAX as u128 - 1) as u64;
            // Keep the OLDEST unanswered ask: the wait is the viewer's.
            let _ =
                self.kf_ask_at
                    .compare_exchange(0, at + 1, Ordering::Relaxed, Ordering::Relaxed);
        } else {
            self.kf_asks_gapped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// FR-135 R0 (N12) — how long the oldest unanswered ask has waited, taken
    /// so the next key frame does not answer it twice. `None` when none waits.
    pub fn take_keyframe_ask(&self) -> Option<std::time::Duration> {
        match self.kf_ask_at.swap(0, Ordering::Relaxed) {
            0 => None,
            at => {
                let now = kf_ask_epoch().elapsed().as_micros() as u64;
                Some(std::time::Duration::from_micros(now.saturating_sub(at - 1)))
            }
        }
    }

    /// FR-135 R0 (N12) — the session's asks so far, and how many the gap
    /// dropped.
    pub fn keyframe_asks(&self) -> (u32, u32) {
        (
            self.kf_asks.load(Ordering::Relaxed),
            self.kf_asks_gapped.load(Ordering::Relaxed),
        )
    }

    /// FR-101 P3 — one completed file transfer. Only a SHA-256 of `name`
    /// is kept.
    pub fn note_file(&self, dir: TransferDirection, name: &str, bytes: u64) {
        let note = FileNote {
            dir,
            name_sha256: hex::encode(Sha256::digest(name.as_bytes())),
            bytes,
        };
        let mut notes = self.file_notes.lock().unwrap_or_else(|p| p.into_inner());
        if notes.len() >= MAX_PENDING_FILE_NOTES {
            tracing::warn!(
                held = notes.len(),
                "file transfer audit: too many notes waiting for the next sample — dropping one"
            );
            return;
        }
        notes.push(note);
    }

    /// What `rc:session.stats` carries for the audit: the cumulative
    /// clipboard tallies, and the file notes waiting, which this TAKES.
    /// Hand them back with [`Self::restore_file_notes`] if the sample could
    /// not be sent.
    pub fn take_transfers(&self) -> Transfers {
        let files = std::mem::take(&mut *self.file_notes.lock().unwrap_or_else(|p| p.into_inner()));
        Transfers {
            clipboard_to_host: self.clipboard_to_host.read(),
            clipboard_from_host: self.clipboard_from_host.read(),
            files,
        }
    }

    /// Put back the file notes of a sample that was never sent, ahead of
    /// any noted since.
    pub fn restore_file_notes(&self, mut files: Vec<FileNote>) {
        if files.is_empty() {
            return;
        }
        let mut notes = self.file_notes.lock().unwrap_or_else(|p| p.into_inner());
        files.append(&mut notes);
        files.truncate(MAX_PENDING_FILE_NOTES);
        *notes = files;
    }

    /// Carry a session's previous peer's readings into this entry. A session
    /// renegotiated under a new peer (a second offer for one session id)
    /// keeps its cumulative clipboard tallies. The old peer's drop forgets its
    /// counters, and without this the tallies would restart below the totals
    /// the server stored, which would then count nothing until they caught up.
    pub fn carry_transfers(&self, t: Transfers) {
        self.clipboard_to_host.add_tally(t.clipboard_to_host);
        self.clipboard_from_host.add_tally(t.clipboard_from_host);
        self.restore_file_notes(t.files);
    }

    pub fn note_keyframe(&self) {
        self.keyframe_requests.fetch_add(1, Ordering::Relaxed);
    }
    /// FR-132 P1 — one decode error the viewer reported. Returns the count
    /// including this one, so the caller can tell the first few (logged, and
    /// sent at once) from a flood. Saturating: a viewer that sends billions
    /// pins the count rather than wrapping it back to a clean-looking zero.
    pub fn note_decode_error(&self) -> u32 {
        self.decode_errors_counted.store(true, Ordering::Relaxed);
        let prev = self
            .decode_errors
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            })
            .unwrap_or(u32::MAX);
        prev.saturating_add(1)
    }
    /// FR-132 P1 — the viewer said it counts decode errors (see
    /// [`Self::decode_errors_counted`]). `true` the first time, so the caller
    /// can send one sample at once: a short session with no errors then
    /// records its measured zero, rather than nothing, before it ends.
    pub fn note_decode_errors_counted(&self) -> bool {
        !self.decode_errors_counted.swap(true, Ordering::Relaxed)
    }
    /// FR-132 P1 — what `rc:session.stats` carries: the count once the
    /// viewer counts (so `Some(0)` is a measured zero), `None` before.
    pub fn decode_errors(&self) -> Option<u32> {
        self.decode_errors_counted
            .load(Ordering::Relaxed)
            .then(|| self.decode_errors.load(Ordering::Relaxed))
    }
    /// FR-132 P1 — carry a session's previous peer's decode-error reading
    /// (its [`Self::decode_errors`]) into this entry, as
    /// [`Self::carry_transfers`] carries the tallies: the old peer's drop
    /// forgets its counters, and the next sample's `$set` would put the
    /// server's live count back below the errors it already recorded.
    pub fn carry_decode_errors(&self, reading: Option<u32>) {
        let Some(n) = reading else { return };
        self.decode_errors_counted.store(true, Ordering::Relaxed);
        let _ = self
            .decode_errors
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                Some(c.saturating_add(n))
            });
    }
    pub fn note_input(&self) {
        self.input_events.fetch_add(1, Ordering::Relaxed);
    }
    /// FR-101 P4b-2 — see [`Self::input_seen`].
    pub fn note_input_seen(&self) {
        self.input_seen.fetch_add(1, Ordering::Relaxed);
    }
    /// FR-101 P4b-2 — see [`Self::transfer_activity`].
    pub fn note_transfer(&self) {
        self.transfer_activity.fetch_add(1, Ordering::Relaxed);
    }
    /// FR-101 P4b-2 — what the controller has done in this session, as one
    /// comparable value: input sent, clipboard both ways, file transfer.
    /// The idle limit only asks whether it CHANGED since its last look; the
    /// numbers mean nothing on their own.
    pub fn activity(&self) -> ControllerActivity {
        ControllerActivity {
            input: self.input_seen.load(Ordering::Relaxed),
            clipboard: self
                .clipboard_to_host
                .read()
                .count
                .wrapping_add(self.clipboard_from_host.read().count),
            transfers: self.transfer_activity.load(Ordering::Relaxed),
        }
    }
    /// One shared heartbeat window: `secs` of shared pipeline time,
    /// `mixed` when the viewers' dials differed during it.
    pub fn note_shared_window(&self, secs: u64, mixed: bool) {
        self.shared_seconds.fetch_add(secs, Ordering::Relaxed);
        if mixed {
            self.mixed_dial_seconds.fetch_add(secs, Ordering::Relaxed);
        }
    }
}

/// FR-101 P4b-2 — [`SessionCounters::activity`]: a snapshot the idle limit
/// compares with the previous one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ControllerActivity {
    pub input: u64,
    pub clipboard: u64,
    pub transfers: u64,
}

fn registry() -> &'static DashMap<ObjectId, Arc<SessionCounters>> {
    static REG: std::sync::OnceLock<DashMap<ObjectId, Arc<SessionCounters>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(DashMap::new)
}

/// Counters for a session, created on first touch. Cheap enough to call
/// from a producer path; the clone is an `Arc` bump.
pub fn counters(session_id: ObjectId) -> Arc<SessionCounters> {
    registry()
        .entry(session_id)
        .or_insert_with(|| Arc::new(SessionCounters::default()))
        .clone()
}

/// Drop a finished session's counters (called from `AgentPeer::drop`) so
/// a long-lived agent doesn't accumulate one entry per session forever.
pub fn forget(session_id: ObjectId) {
    registry().remove(&session_id);
}

/// FR-101 P4b-2 — a session's counters if it has any, WITHOUT creating them:
/// the idle limit reads on its own timer, and a read that created an entry
/// after the peer's drop forgot it would leak one per session.
pub fn peek(session_id: ObjectId) -> Option<Arc<SessionCounters>> {
    registry().get(&session_id).map(|e| e.value().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // P8 Phase 4 — shared windows accumulate seconds; mixed only when
    // the dials differed during the window.
    #[test]
    fn shared_windows_accumulate_and_mixed_is_a_subset() {
        let s = ObjectId::new();
        counters(s).note_shared_window(2, false);
        counters(s).note_shared_window(2, true);
        counters(s).note_shared_window(1, true);
        assert_eq!(counters(s).shared_seconds.load(Ordering::Relaxed), 5);
        assert_eq!(counters(s).mixed_dial_seconds.load(Ordering::Relaxed), 3);
        forget(s);
    }

    #[test]
    fn counters_are_per_session_and_forgettable() {
        let a = ObjectId::new();
        let b = ObjectId::new();
        counters(a).note_keyframe();
        counters(a).note_keyframe();
        counters(a).note_input();
        counters(b).note_input();

        assert_eq!(counters(a).keyframe_requests.load(Ordering::Relaxed), 2);
        assert_eq!(counters(a).input_events.load(Ordering::Relaxed), 1);
        // Sessions never bleed into each other.
        assert_eq!(counters(b).keyframe_requests.load(Ordering::Relaxed), 0);

        forget(a);
        // A forgotten session starts clean rather than resurrecting stale
        // totals if the id is ever touched again.
        assert_eq!(counters(a).keyframe_requests.load(Ordering::Relaxed), 0);
        forget(a);
        forget(b);
    }

    /// FR-132 P1 — a session's decode errors read as "not counted" until the
    /// viewer says it counts, then as a count that a zero can be; each error
    /// adds one and marks the session counted; a flood pins rather than wraps;
    /// and a renegotiated session's new entry carries the old reading.
    #[test]
    fn decode_errors_are_not_counted_until_the_viewer_counts() {
        let s = ObjectId::new();
        let c = counters(s);
        assert_eq!(
            c.decode_errors(),
            None,
            "an older viewer: no count, not zero"
        );
        assert!(c.note_decode_errors_counted(), "the first time says so");
        assert!(!c.note_decode_errors_counted(), "once only");
        assert_eq!(c.decode_errors(), Some(0), "a measured zero");
        assert_eq!(c.note_decode_error(), 1);
        assert_eq!(c.note_decode_error(), 2);
        assert_eq!(c.decode_errors(), Some(2));
        forget(s);

        // An error alone marks the session counted (a configure failure can
        // come before the first `rc:decodestat`).
        let t = ObjectId::new();
        assert_eq!(counters(t).note_decode_error(), 1);
        assert_eq!(counters(t).decode_errors(), Some(1));

        // The carry across a re-offer: the old peer's drop forgets, the new
        // entry gets the reading back; an uncounted reading carries nothing.
        let reading = counters(t).decode_errors();
        forget(t);
        assert_eq!(counters(t).decode_errors(), None);
        counters(t).carry_decode_errors(reading);
        assert_eq!(counters(t).decode_errors(), Some(1));
        counters(t).carry_decode_errors(None);
        assert_eq!(counters(t).decode_errors(), Some(1));
        forget(t);
        let u = ObjectId::new();
        counters(u).carry_decode_errors(None);
        assert_eq!(counters(u).decode_errors(), None, "nothing to carry");

        // Saturating.
        counters(u).carry_decode_errors(Some(u32::MAX));
        assert_eq!(counters(u).note_decode_error(), u32::MAX);
        assert_eq!(counters(u).decode_errors(), Some(u32::MAX));
        forget(u);
    }

    /// FR-132 P1d — a session's video reads as not reported until the pump
    /// says, then as its newest reading. GPU input has no reason to give, so
    /// none is sent.
    #[test]
    fn a_sessions_video_is_the_pumps_newest_reading() {
        let s = ObjectId::new();
        assert_eq!(counters(s).video(), None, "not reported, never CPU");

        counters(s).note_video(VideoForm {
            encoder: "h264_qsv",
            capture: "system-context",
            input_path: "cpu",
            input_reason: "denied",
            chroma: "yuv420",
        });
        assert_eq!(
            counters(s).video(),
            Some(SessionVideo {
                encoder: "h264_qsv".into(),
                capture: "system-context".into(),
                input_path: "cpu".into(),
                input_reason: Some("denied".into()),
                chroma: Some("yuv420".into()),
                ..Default::default()
            })
        );

        counters(s).note_video(VideoForm {
            encoder: "av1_qsv",
            capture: "wgc",
            input_path: "gpu",
            input_reason: "",
            chroma: "yuv444",
        });
        assert_eq!(
            counters(s).video(),
            Some(SessionVideo {
                encoder: "av1_qsv".into(),
                capture: "wgc".into(),
                input_path: "gpu".into(),
                input_reason: None,
                chroma: Some("yuv444".into()),
                ..Default::default()
            }),
            "the newest reading"
        );
        forget(s);
    }

    /// FR-132 P1d — the pump asks for a sample only when the reading is new,
    /// or every session would send one each heartbeat.
    #[test]
    fn only_a_new_reading_asks_for_a_sample() {
        let s = ObjectId::new();
        let cpu = VideoForm {
            encoder: "h264_qsv",
            capture: "wgc",
            input_path: "cpu",
            input_reason: "cpu_frames",
            chroma: "yuv420",
        };
        let gpu = VideoForm {
            input_path: "gpu",
            input_reason: "",
            ..cpu
        };
        assert!(counters(s).note_video(cpu), "the first reading");
        assert!(!counters(s).note_video(cpu), "the same one again");
        assert!(counters(s).note_video(gpu), "the form moved");
        assert!(!counters(s).note_video(gpu));
        forget(s);
    }

    /// FR-135 R0 (N16) — the summary is absent until a window folds, then
    /// rides the reading: moving and sparse by the fold's verdict, frames
    /// summed, the most followers kept, and the first frame's time once.
    #[test]
    fn the_media_summary_folds_into_the_reading() {
        use crate::media_window::FoldReport;
        let s = ObjectId::new();
        let form = VideoForm {
            encoder: "hevc_qsv",
            capture: "system-context",
            input_path: "cpu",
            input_reason: "switch_off",
            chroma: "yuv420",
        };
        counters(s).note_video(form);
        let v = counters(s).video().unwrap();
        assert_eq!(v.chroma.as_deref(), Some("yuv420"));
        assert_eq!(
            (v.moving_s, v.sparse_s, v.frames_encoded, v.first_frame_ms),
            (None, None, None, None),
            "not counted before a fold, never zero"
        );

        counters(s).note_media_fold(
            FoldReport {
                ms: 2_000,
                moving: true,
                frames_encoded: 120,
                frames_keepalive: 0,
            },
            0,
        );
        counters(s).note_media_fold(
            FoldReport {
                ms: 10_000,
                moving: false,
                frames_encoded: 10,
                frames_keepalive: 10,
            },
            2,
        );
        counters(s).note_media_fold(
            FoldReport {
                ms: 2_500,
                moving: false,
                frames_encoded: 2,
                frames_keepalive: 2,
            },
            1,
        );
        counters(s).note_first_frame(std::time::Duration::from_millis(640));
        counters(s).note_first_frame(std::time::Duration::from_millis(9_999));
        let v = counters(s).video().unwrap();
        assert_eq!(v.moving_s, Some(2));
        assert_eq!(
            v.sparse_s,
            Some(12),
            "12.5 s of sparse windows, whole seconds"
        );
        assert_eq!(v.frames_encoded, Some(132));
        assert_eq!(v.frames_keepalive, Some(12));
        assert_eq!(v.followers, Some(2), "the most at once");
        assert_eq!(v.first_frame_ms, Some(640), "the first call wins");
        assert!(v.clone().checked().is_some(), "{v:?}");
        forget(s);
    }

    /// FR-135 R0b's review — a first frame noted before the first fold (a slow
    /// inline open straddling the 2 s mark) adds no zeros: the fold's fields stay
    /// absent, not counted, until a window has folded.
    #[test]
    fn a_first_frame_before_any_fold_adds_no_zeros() {
        let s = ObjectId::new();
        counters(s).note_video(VideoForm {
            encoder: "h264_nvenc",
            capture: "wgc",
            input_path: "gpu",
            input_reason: "",
            chroma: "yuv420",
        });
        counters(s).note_first_frame(std::time::Duration::from_millis(2_100));
        let v = counters(s).video().unwrap();
        assert_eq!(v.first_frame_ms, Some(2_100));
        assert_eq!(
            (
                v.moving_s,
                v.sparse_s,
                v.frames_encoded,
                v.frames_keepalive,
                v.followers
            ),
            (None, None, None, None, None),
            "no fold yet: not counted, never zero"
        );
        forget(s);
    }

    /// FR-135 R0b's review — the server drops a reading with a count past its
    /// bound whole, so the agent saturates every count at that bound: a session
    /// that waited over an hour for its first frame still names its cell.
    #[test]
    fn the_reading_never_carries_a_count_the_server_drops() {
        use crate::media_window::FoldReport;
        let s = ObjectId::new();
        counters(s).note_video(VideoForm {
            encoder: "hevc_qsv",
            capture: "system-context",
            input_path: "cpu",
            input_reason: "switch_off",
            chroma: "yuv420",
        });
        counters(s).note_first_frame(std::time::Duration::from_secs(2 * 3600));
        counters(s).note_media_fold(
            FoldReport {
                ms: u64::from(SessionVideo::MAX_SECS) * 1000 + 5_000,
                moving: true,
                frames_encoded: SessionVideo::MAX_FRAMES + 1,
                frames_keepalive: 0,
            },
            SessionVideo::MAX_FOLLOWERS + 1,
        );
        let v = counters(s).video().unwrap();
        assert_eq!(v.first_frame_ms, Some(SessionVideo::MAX_FIRST_FRAME_MS));
        assert_eq!(v.moving_s, Some(SessionVideo::MAX_SECS));
        assert_eq!(v.frames_encoded, Some(SessionVideo::MAX_FRAMES));
        assert_eq!(v.followers, Some(SessionVideo::MAX_FOLLOWERS));
        assert!(v.clone().checked().is_some(), "the server keeps it: {v:?}");
        forget(s);
    }

    /// FR-135 R0 (N12) — every ask counts, the gap's drops count apart, and the
    /// oldest unanswered ask's wait is taken once, by the key frame that
    /// answers it.
    #[test]
    fn a_keyframe_ask_waits_until_a_key_frame_takes_it() {
        let s = ObjectId::new();
        assert_eq!(counters(s).take_keyframe_ask(), None);
        counters(s).note_keyframe_ask(true);
        std::thread::sleep(std::time::Duration::from_millis(5));
        counters(s).note_keyframe_ask(false);
        counters(s).note_keyframe_ask(true);
        assert_eq!(counters(s).keyframe_asks(), (3, 1));
        let waited = counters(s).take_keyframe_ask().expect("an ask waits");
        assert!(
            waited >= std::time::Duration::from_millis(5),
            "the OLDEST ask's wait: {waited:?}"
        );
        assert_eq!(counters(s).take_keyframe_ask(), None, "answered once");
        assert_eq!(
            counters(s).keyframe_asks(),
            (3, 1),
            "taking leaves the counts"
        );
        forget(s);
    }

    /// FR-135 R0 (N11, N16) — the viewer's painted and freeze counts ride the
    /// reading once a viewer that counts reports them, and an older viewer's
    /// decodestat leaves them absent, never zero.
    #[test]
    fn the_viewers_counts_ride_the_reading() {
        use crate::media_window::ViewerReport;
        let s = ObjectId::new();
        counters(s).note_video(VideoForm {
            encoder: "hevc_nvenc",
            capture: "wgc",
            input_path: "gpu",
            input_reason: "",
            chroma: "yuv420",
        });
        counters(s).note_viewer_report(ViewerReport::default());
        let v = counters(s).video().unwrap();
        assert_eq!((v.frames_painted, v.freezes), (None, None), "not counted");
        counters(s).note_viewer_report(ViewerReport {
            painted: Some(600),
            freezes: Some(0),
            ..Default::default()
        });
        let v = counters(s).video().unwrap();
        assert_eq!(
            (v.frames_painted, v.freezes),
            (Some(600), Some(0)),
            "a measured zero"
        );
        // A later window without the counts keeps the last ones.
        counters(s).note_viewer_report(ViewerReport {
            dec: Some([1; crate::media_window::BUCKETS]),
            ..Default::default()
        });
        counters(s).note_viewer_report(ViewerReport {
            painted: Some(1_200),
            freezes: Some(3),
            ..Default::default()
        });
        let v = counters(s).video().unwrap();
        assert_eq!((v.frames_painted, v.freezes), (Some(1_200), Some(3)));
        assert!(v.checked().is_some());
        forget(s);
    }

    /// FR-135 R0 — the heartbeat takes the viewer's windows once each, summed,
    /// with the counts as last reported; nothing before a viewer that counts.
    #[test]
    fn the_heartbeat_takes_the_viewers_windows_once() {
        use crate::media_window::{BUCKETS, ViewerReport};
        let s = ObjectId::new();
        assert_eq!(counters(s).take_viewer_window(), None, "an older viewer");
        let mut one = [0; BUCKETS];
        one[4] = 30;
        for painted in [60, 120] {
            counters(s).note_viewer_report(ViewerReport {
                dec: Some(one),
                gate: Some([0; BUCKETS]),
                painted: Some(painted),
                freezes: Some(1),
                chunk_gaps: Some(0),
                decoder_hw: Some(true),
                ..Default::default()
            });
        }
        let w = counters(s)
            .take_viewer_window()
            .expect("a viewer that counts");
        assert_eq!(w.windows, 2);
        let mut two = [0; BUCKETS];
        two[4] = 60;
        assert_eq!(w.report.dec, Some(two), "the two windows summed");
        assert_eq!(w.report.gate, Some([0; BUCKETS]), "measured, and none");
        assert_eq!(w.report.paint, None, "never sent");
        assert_eq!(
            (w.report.painted, w.report.freezes, w.report.decoder_hw),
            (Some(120), Some(1), Some(true))
        );
        let w = counters(s)
            .take_viewer_window()
            .expect("still a viewer that counts");
        assert_eq!(w.windows, 0, "nothing came since");
        assert_eq!(w.report.dec, None);
        assert_eq!(w.report.painted, Some(120), "the counts carry on");
        forget(s);
    }

    /// FR-132 P1d, the reviewer's nit on #3284 — the rule below rests on what a
    /// failed read produces: `EncoderDriver::rate_stats` answers
    /// `RateStats::default()`, whose input form must stay CPU with no reason.
    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn a_failed_read_is_cpu_with_no_reason() {
        let failed = crate::encode::ffmpeg::encoder::RateStats::default();
        assert!(!failed.input.gpu);
        assert_eq!(failed.input.reason, "");
        assert_eq!(failed.maxrate_bps, 0);
    }

    /// FR-132 P1d — a heartbeat whose `rate_stats` failed reads `InputForm`'s
    /// default, CPU with no reason. It says nothing about the session, so a
    /// reading stands through it, and no sample goes for it.
    #[test]
    fn a_failed_stats_read_replaces_no_reading() {
        let s = ObjectId::new();
        let gpu = VideoForm {
            encoder: "h264_qsv",
            capture: "wgc",
            input_path: "gpu",
            input_reason: "",
            chroma: "yuv420",
        };
        let failed = VideoForm {
            input_path: "cpu",
            ..gpu
        };
        assert!(!counters(s).note_video(failed));
        assert_eq!(counters(s).video(), None, "not reported, never CPU");

        assert!(counters(s).note_video(gpu));
        assert!(!counters(s).note_video(failed));
        assert_eq!(
            counters(s).video().map(|v| v.input_path).as_deref(),
            Some("gpu")
        );
        forget(s);
    }

    // FR-101 P3 — the clipboard tallies are cumulative (a sample never
    // resets them), and the file notes are taken exactly once.
    #[test]
    fn clipboard_tallies_accumulate_and_file_notes_are_taken_once() {
        let s = ObjectId::new();
        let c = counters(s);
        c.clipboard_to_host.add(10);
        c.clipboard_to_host.add(5);
        c.clipboard_from_host.add(7);
        c.note_file(TransferDirection::ToHost, "report.pdf", 1234);

        let first = c.take_transfers();
        assert_eq!(
            first.clipboard_to_host,
            Tally {
                count: 2,
                bytes: 15
            }
        );
        assert_eq!(first.clipboard_from_host, Tally { count: 1, bytes: 7 });
        assert_eq!(first.files.len(), 1);

        c.clipboard_to_host.add(1);
        let second = c.take_transfers();
        assert_eq!(
            second.clipboard_to_host,
            Tally {
                count: 3,
                bytes: 16
            },
            "cumulative: the server takes the difference"
        );
        assert!(second.files.is_empty(), "a note goes out once");
        forget(s);
    }

    // FR-101 P3 — a name never leaves the device: only its SHA-256 is held,
    // and equal names hash equally.
    #[test]
    fn a_file_name_is_held_only_as_its_sha256() {
        let s = ObjectId::new();
        let c = counters(s);
        c.note_file(TransferDirection::FromHost, "salaries 2026.xlsx", 99);
        c.note_file(TransferDirection::ToHost, "salaries 2026.xlsx", 99);
        c.note_file(TransferDirection::ToHost, "abc", 3);
        let t = c.take_transfers();
        let held = format!("{t:?}");
        assert!(!held.contains("salaries"), "{held}");
        assert_eq!(t.files[0].name_sha256, t.files[1].name_sha256);
        assert_eq!(t.files[0].dir, TransferDirection::FromHost);
        // FIPS 180-2's test vector, so this checks the hash, not itself.
        assert_eq!(
            t.files[2].name_sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        forget(s);
    }

    // FR-101 P3 — a session renegotiated under a new peer keeps its
    // readings: the old peer's drop forgets the entry, and the carry seeds the
    // fresh one, so the cumulative tallies continue instead of restarting.
    #[test]
    fn a_replaced_peer_carries_its_readings_into_the_fresh_entry() {
        let s = ObjectId::new();
        counters(s).clipboard_to_host.add(40);
        counters(s).note_file(TransferDirection::ToHost, "a", 1);
        let carried = counters(s).take_transfers();
        forget(s); // the old peer's drop
        counters(s).carry_transfers(carried);
        counters(s).clipboard_to_host.add(2);
        let t = counters(s).take_transfers();
        assert_eq!(
            t.clipboard_to_host,
            Tally {
                count: 2,
                bytes: 42
            }
        );
        assert_eq!(t.files.len(), 1, "a note waiting on the old peer survives");
        forget(s);
    }

    // FR-101 P3 — the notes of a sample that was never sent go back ahead
    // of newer ones, and the queue stays bounded.
    #[test]
    fn unsent_notes_go_back_first_and_the_queue_is_bounded() {
        let s = ObjectId::new();
        let c = counters(s);
        c.note_file(TransferDirection::ToHost, "a", 1);
        let unsent = c.take_transfers().files;
        c.note_file(TransferDirection::ToHost, "b", 2);
        c.restore_file_notes(unsent);
        let t = c.take_transfers();
        assert_eq!(t.files.iter().map(|f| f.bytes).collect::<Vec<_>>(), [1, 2]);

        for i in 0..(MAX_PENDING_FILE_NOTES + 10) {
            c.note_file(TransferDirection::ToHost, "x", i as u64);
        }
        assert_eq!(c.take_transfers().files.len(), MAX_PENDING_FILE_NOTES);
        forget(s);
    }
}
