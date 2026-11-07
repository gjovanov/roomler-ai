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

    /// FR-132 P1d — what `rc:session.stats` carries.
    pub fn video(&self) -> Option<SessionVideo> {
        let form = (*self.video.lock().unwrap_or_else(|p| p.into_inner()))?;
        Some(SessionVideo {
            encoder: form.encoder.to_string(),
            capture: form.capture.to_string(),
            input_path: form.input_path.to_string(),
            input_reason: (!form.input_reason.is_empty()).then(|| form.input_reason.to_string()),
        })
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
        });
        assert_eq!(
            counters(s).video(),
            Some(SessionVideo {
                encoder: "h264_qsv".into(),
                capture: "system-context".into(),
                input_path: "cpu".into(),
                input_reason: Some("denied".into()),
            })
        );

        counters(s).note_video(VideoForm {
            encoder: "av1_qsv",
            capture: "wgc",
            input_path: "gpu",
            input_reason: "",
        });
        assert_eq!(
            counters(s).video(),
            Some(SessionVideo {
                encoder: "av1_qsv".into(),
                capture: "wgc".into(),
                input_path: "gpu".into(),
                input_reason: None,
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
