// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-27 — the live remote-control session registry.
//!
//! "Who is watching my screen, and how do I stop them" existed only inside the
//! Windows-native overlay: the daemon knew, but nothing else could ask. There
//! was no LocalAPI verb for a live session, so the desktop companion could not
//! render a banner on any platform and no thin client could offer a Disconnect.
//!
//! This is the shared handle that fixes that. One instance per daemon, created
//! in `run_cmd` and given to BOTH the LocalAPI's `DaemonState` (which reads it)
//! and every signalling loop's [`crate::indicator::ViewerIndicator`] (which
//! writes it, at the same two call sites that already raise and clear the
//! on-screen indicator — so a session cannot appear in one and not the other).
//!
//! ⚠️ Each entry carries its OWN loop's kill channel, not a process-global one.
//! A multi-org daemon runs one signalling loop per enrollment, and a session
//! belongs to exactly one of them; a single shared sender would have to guess.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use bson::oid::ObjectId;
use tunnel_core::localapi::RcSessionInfo;

/// FR-93 P6h — sessions whose capture is parked on the desktop's own dialog,
/// with how many live marks hold each. Process-global rather than a registry
/// field: the capture open that knows it runs far from the registry's owner,
/// and an `ObjectId` names one session daemon-wide.
static ASKING: LazyLock<Mutex<HashMap<ObjectId, usize>>> = LazyLock::new(Mutex::default);

/// `ROOMLERD_BANNER_WAITS_FOR_DIALOG=0` — never mark a session as asking, so
/// the banner shows from the session's start, as before P6h.
fn asking_marks_enabled() -> bool {
    tunnel_core::env::flag("BANNER_WAITS_FOR_DIALOG", true)
}

/// While one lives, `session` lists with `asking` set: the desktop's own
/// dialog is asking the person at the screen to share it, so nothing is
/// shared yet. Dropping it — the open returned, or moved on to another
/// wait — takes the mark away.
pub struct AskingMark(Option<ObjectId>);

impl AskingMark {
    pub fn new(session: ObjectId) -> Self {
        if !asking_marks_enabled() {
            return Self(None);
        }
        *ASKING
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(session)
            .or_default() += 1;
        Self(Some(session))
    }
}

impl Drop for AskingMark {
    fn drop(&mut self) {
        let Some(session) = self.0 else {
            return;
        };
        let mut map = ASKING.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = map.get_mut(&session) {
            *n -= 1;
            if *n == 0 {
                map.remove(&session);
            }
        }
    }
}

fn asking(session: &ObjectId) -> bool {
    ASKING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(session)
}

/// The channel a signalling loop polls for "the person at this device wants
/// this session gone" — the same one the Windows overlay's Disconnect button
/// fires through, so a LocalAPI disconnect and an overlay click take the
/// identical teardown path. Each order names what asked ([`Via`], #4001).
pub type KillSender = tokio::sync::mpsc::Sender<KillOrder>;

#[derive(Clone)]
struct Entry {
    controller_name: String,
    permissions: String,
    org: String,
    started_at_ms: u64,
    kill: KillSender,
    /// FR-85 P3 — this controller is recording the screen.
    recording: bool,
    /// FR-85 P3d — when a client first read `recording`. The flag reaches the
    /// screen only by being read ([`RcSessionRegistry::list_for_client`]).
    recording_read: Option<Instant>,
    /// FR-85 P3b-3 — the session ended while recording, and the entry stays
    /// for the recording's re-attach grace: the banner says "reconnecting".
    detached: bool,
    /// #2531 — when the session was first registered here, which is when a
    /// client's read of the live sessions can first list it. A re-announce
    /// keeps it.
    registered: Instant,
}

/// Cheap to clone; every clone sees the same map.
#[derive(Clone, Default)]
pub struct RcSessionRegistry {
    inner: Arc<Mutex<HashMap<ObjectId, Entry>>>,
    /// #2531 — per person (uid), when a client of theirs last read the live
    /// sessions over the person socket ([`Self::list_for_person`]).
    person_reads: Arc<Mutex<HashMap<u32, Instant>>>,
}

impl RcSessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A session became visible on this device. Idempotent — a re-announce
    /// replaces the entry rather than duplicating it, matching
    /// `ViewerIndicator::show_session`.
    pub fn insert(
        &self,
        session: ObjectId,
        controller_name: String,
        permissions: String,
        org: String,
        kill: KillSender,
    ) {
        let started_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut map = self.inner.lock().unwrap();
        // ⚠️ A re-announce must not clear a recording in progress: the banner
        // saying "recording" is the host's only notice of it.
        let recording = map.get(&session).is_some_and(|e| e.recording);
        let registered = map
            .get(&session)
            .map_or_else(Instant::now, |e| e.registered);
        map.insert(
            session,
            Entry {
                controller_name,
                permissions,
                org,
                started_at_ms,
                kill,
                recording,
                recording_read: None,
                detached: false,
                registered,
            },
        );
    }

    /// #2531 — when `session` was registered here, if it is.
    pub fn registered_at(&self, session: &ObjectId) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap()
            .get(session)
            .map(|e| e.registered)
    }

    pub fn remove(&self, session: &ObjectId) {
        self.inner.lock().unwrap().remove(session);
    }

    /// #3280 — no session is live on this device, and no recording outlives one.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    /// FR-85 P3b-3 — `session` ended. Its entry goes, UNLESS it is recording:
    /// a remote recording outlives its session for the re-attach grace, and
    /// the banner must keep saying so. `true` = the entry was kept (detached).
    pub fn session_ended(&self, session: &ObjectId) -> bool {
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(session) {
            Some(e) if e.recording => {
                e.detached = true;
                true
            }
            _ => {
                map.remove(session);
                false
            }
        }
    }

    /// FR-85 P3b-3 — the recording of `session` ended, or moved to its
    /// controller's next session. The entry stops saying "recording"; one
    /// that was kept only for the recording (its session gone) is removed.
    /// `true` = removed.
    pub fn recording_ended(&self, session: &ObjectId) -> bool {
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(session) {
            Some(e) if e.detached => {
                map.remove(session);
                true
            }
            Some(e) => {
                e.recording = false;
                false
            }
            None => false,
        }
    }

    /// FR-85 P3 — mark `session` as recording (or not). `false` = no such
    /// live session, which the caller must treat as "nothing on screen says
    /// so" — a recording is never started behind a banner that is not there.
    pub fn set_recording(&self, session: &ObjectId, recording: bool) -> bool {
        match self.inner.lock().unwrap().get_mut(session) {
            Some(e) => {
                e.recording = recording;
                e.recording_read = None;
                true
            }
            None => false,
        }
    }

    /// FR-85 P3d — [`Self::list`], for a client over the LocalAPI, and the
    /// moment a recording's flag leaves the daemon. The companion learns of
    /// it only here, so this is what proves it has been told.
    pub fn list_for_client(&self) -> Vec<RcSessionInfo> {
        let now = Instant::now();
        for e in self.inner.lock().unwrap().values_mut() {
            if e.recording && e.recording_read.is_none() {
                e.recording_read = Some(now);
            }
        }
        self.list()
    }

    /// #2531 — [`Self::list_for_client`], read by `uid`'s own client over the
    /// person socket, and noted as that person's last read. The time is taken
    /// before the list, so a read counts only for sessions it could list, and
    /// the note only moves forward: of two reads at once, the later one's time
    /// stays whichever ends first.
    pub fn list_for_person(&self, uid: u32) -> Vec<RcSessionInfo> {
        let at = Instant::now();
        let list = self.list_for_client();
        self.person_reads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(uid)
            .and_modify(|last| *last = (*last).max(at))
            .or_insert(at);
        list
    }

    /// #2531 — when `uid`'s client last read the live sessions over the
    /// person socket, if it ever did.
    pub fn person_read_at(&self, uid: u32) -> Option<Instant> {
        self.person_reads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&uid)
            .copied()
    }

    /// FR-85 P3d — when a client first read `session` as recording.
    pub fn recording_read_at(&self, session: &ObjectId) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap()
            .get(session)
            .and_then(|e| e.recording_read)
    }

    /// Snapshot for the LocalAPI, oldest first — a banner listing several
    /// viewers should not reorder itself between polls.
    pub fn list(&self) -> Vec<RcSessionInfo> {
        let mut out: Vec<(u64, RcSessionInfo)> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| {
                (
                    e.started_at_ms,
                    RcSessionInfo {
                        session_id: id.to_hex(),
                        controller_name: e.controller_name.clone(),
                        permissions: e.permissions.clone(),
                        org: e.org.clone(),
                        started_at_ms: e.started_at_ms,
                        recording: e.recording,
                        reconnecting: e.detached,
                        asking: asking(id),
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.session_id.cmp(&b.1.session_id))
        });
        out.into_iter().map(|(_, s)| s).collect()
    }

    /// Ask the owning signalling loop to tear `session` down, for `via`.
    /// `false` = no such live session (already gone, or never on this device).
    ///
    /// Deliberately does NOT remove the entry: the signalling loop owns the
    /// lifecycle and clears it through `hide_session` once the peer is
    /// actually closed. Removing it here would make the banner disappear
    /// before the session had, which is the wrong way round for a control the
    /// operator is watching for an effect.
    pub fn disconnect(&self, session: &ObjectId, via: Via) -> bool {
        // FR-85 P3b-3 — a DETACHED entry has no session left to tear down
        // (only its recording remains, which the banner's Stop ends).
        let Some(kill) = self
            .inner
            .lock()
            .unwrap()
            .get(session)
            .filter(|e| !e.detached)
            .map(|e| e.kill.clone())
        else {
            return false;
        };
        // `try_send` on purpose: this runs on the LocalAPI's sync dispatch, and
        // a full 4-slot kill queue means several teardowns are already in
        // flight — reporting that honestly beats blocking the control socket.
        match kill.try_send((*session, via)) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(%session, %e, "rc disconnect could not be queued");
                false
            }
        }
    }
}

/// #4001 — what at this device asked for a session's end, as far as the
/// daemon can tell. A LocalAPI caller is named as such: its request does not
/// say whether the companion's banner or `roomlerd rc --disconnect` sent it,
/// and the daemon does not guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The Disconnect on the Windows overlay's badge.
    OverlayBadge,
    /// A LocalAPI client: the companion's banner, or `roomlerd rc --disconnect`.
    LocalApi,
    /// The Disconnect on the being-viewed notification (Linux, FR-93 P6g).
    Notification,
    /// The desktop's own control over the screen share: Stop on GNOME's
    /// indicator, End on KDE's (FR-93 P6c).
    ShareStop,
    /// A pump with no encoder (#3245). No person asked: the signalling loop
    /// reads the session's own flag and names that end `no_video`.
    NoVideo,
}

impl Via {
    /// The `via` field of [`log_host_ended`]'s line.
    pub fn label(self) -> &'static str {
        match self {
            Self::OverlayBadge => "overlay_badge",
            Self::LocalApi => "localapi",
            Self::Notification => "being_viewed_notification",
            Self::ShareStop => "desktop_share_stop",
            Self::NoVideo => "no_video",
        }
    }
}

/// What a kill channel carries: the session, and what asked for its end.
pub type KillOrder = (ObjectId, Via);

/// #4001 — the line the signalling loop logs when the person at this device
/// ends a session, with what asked. Until #4001 it read `viewee requested
/// disconnect via overlay badge` for every one of them, though only Windows
/// has a badge. Field reads grep this text: change it with its test.
pub fn log_host_ended(session_id: ObjectId, via: Via) {
    tracing::info!(%session_id, via = via.label(), "the person at the device ended the session");
}

/// #2531 — how long the companion of the person at the screen has to show
/// that it lists a viewed session (FR-93 P6g): a read of the live sessions
/// over the person socket ([`RcSessionRegistry::list_for_person`]) after the
/// session was registered, at most this long after the capture opened, or
/// after the registration where that came later (a banner can list a session
/// only once it is registered). And a told session whose companion then reads
/// nothing for longer than this gets the notification too.
///
/// The companion sleeps 750 ms between reads and gives one read 5 s, so a
/// companion that works never goes this long without one. One started for
/// the session that is slower than this to its first read gets the
/// notification as well as its banner: the bound errs toward telling.
pub const TOLD_WITHIN: std::time::Duration = std::time::Duration::from_secs(6);

/// #2531 — what the being-viewed notice knows, at a moment, of whether the
/// companion has shown it lists a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Told {
    /// The person's companion read the live sessions after the session was
    /// registered: its banner lists it.
    Yes,
    /// Not yet, and there is time left.
    NotYet,
    /// The time ran out with no such read: the notification says it.
    No,
}

/// #2531 — [`Told`] at `now`, for a session registered at `registered`
/// whose companion has until `deadline`, given the person's `last_read`. A
/// read taken before the registration, or at the same instant, does not
/// count: it could not list the session.
pub fn told(
    now: Instant,
    registered: Instant,
    deadline: Instant,
    last_read: Option<Instant>,
) -> Told {
    if last_read.is_some_and(|read| read > registered) {
        Told::Yes
    } else if now >= deadline {
        Told::No
    } else {
        Told::NotYet
    }
}

/// #2531 — a told session's companion has gone quiet: no read of the live
/// sessions for longer than `bound` at `now`.
pub fn gone_quiet(now: Instant, last_read: Option<Instant>, bound: std::time::Duration) -> bool {
    last_read.is_none_or(|read| now.saturating_duration_since(read) > bound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// #2531 — with a stubbed clock and record: a session is told only by a
    /// read of the person's that came after its registration. None inside
    /// the bound, and the notification goes up; one before the registration,
    /// or at the same instant, is no read of it.
    #[test]
    fn a_session_is_told_only_by_a_read_after_its_registration() {
        let opened = Instant::now();
        let registered = opened + Duration::from_millis(40);
        let deadline = registered + TOLD_WITHIN;
        let at = |ms: u64| opened + Duration::from_millis(ms);
        // No read at all: waiting, then the notification.
        assert_eq!(told(at(100), registered, deadline, None), Told::NotYet);
        assert_eq!(told(deadline, registered, deadline, None), Told::No);
        // A read before the registration, and one at its instant, list it not.
        for read in [at(10), registered] {
            assert_eq!(
                told(at(500), registered, deadline, Some(read)),
                Told::NotYet
            );
            assert_eq!(told(deadline, registered, deadline, Some(read)), Told::No);
        }
        // A read after it: told, the moment it comes.
        assert_eq!(
            told(at(900), registered, deadline, Some(at(800))),
            Told::Yes
        );
        // Inside the bound, however late in it.
        let last_moment = deadline - Duration::from_millis(1);
        assert_eq!(
            told(last_moment, registered, deadline, Some(last_moment)),
            Told::Yes
        );
    }

    /// #2531 — with a stubbed clock and record: a told session's companion
    /// that reads nothing for longer than the bound has gone quiet, and one
    /// that reads within it has not. Never read at all is quiet.
    #[test]
    fn a_companion_that_stops_reading_for_longer_than_the_bound_has_gone_quiet() {
        let read = Instant::now();
        assert!(!gone_quiet(read, Some(read), TOLD_WITHIN));
        assert!(!gone_quiet(read + TOLD_WITHIN, Some(read), TOLD_WITHIN));
        assert!(gone_quiet(
            read + TOLD_WITHIN + Duration::from_millis(1),
            Some(read),
            TOLD_WITHIN
        ));
        assert!(gone_quiet(read, None, TOLD_WITHIN));
        // A clock that reads earlier than the read (a read noted just now on
        // another thread) is no gap.
        assert!(!gone_quiet(
            read,
            Some(read + Duration::from_millis(5)),
            TOLD_WITHIN
        ));
    }

    /// #2531 — the record: a person's read of the live sessions is noted as
    /// theirs, at a time taken before the list, and lists what a client read
    /// lists. Another person's read, or any client's other read, is not
    /// theirs. A re-announce keeps the session's registration.
    #[test]
    fn a_persons_read_is_noted_for_them_and_a_reannounce_keeps_the_registration() {
        let (reg, ids, _rx) = reg_with(1);
        let registered = reg.registered_at(&ids[0]).expect("registered");
        assert_eq!(reg.person_read_at(1000), None);
        let before = Instant::now();
        let listed = reg.list_for_person(1000);
        let after = Instant::now();
        assert_eq!(listed.len(), 1);
        let read = reg.person_read_at(1000).expect("noted for 1000");
        assert!(before <= read && read <= after, "taken around the list");
        assert!(read > registered);
        assert_eq!(reg.person_read_at(1001), None, "nobody else's");
        let _ = reg.list_for_client();
        let _ = reg.list();
        assert_eq!(
            reg.person_read_at(1000),
            Some(read),
            "other reads are not theirs"
        );

        let (tx, _rx2) = tokio::sync::mpsc::channel(1);
        reg.insert(ids[0], "viewer0".into(), "VIEW".into(), String::new(), tx);
        assert_eq!(reg.registered_at(&ids[0]), Some(registered));
        assert_eq!(reg.registered_at(&ObjectId::new()), None);
    }

    fn reg_with(
        n: usize,
    ) -> (
        RcSessionRegistry,
        Vec<ObjectId>,
        tokio::sync::mpsc::Receiver<KillOrder>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let reg = RcSessionRegistry::new();
        let ids: Vec<ObjectId> = (0..n).map(|_| ObjectId::new()).collect();
        for (i, id) in ids.iter().enumerate() {
            reg.insert(
                *id,
                format!("viewer{i}"),
                "VIEW | INPUT".into(),
                String::new(),
                tx.clone(),
            );
        }
        (reg, ids, rx)
    }

    #[tokio::test]
    async fn disconnect_routes_to_the_owning_loop() {
        let (reg, ids, mut rx) = reg_with(2);
        assert!(reg.disconnect(&ids[1], Via::LocalApi));
        // #4001 — the order names what asked, for the loop's line.
        assert_eq!(rx.recv().await, Some((ids[1], Via::LocalApi)));
        // The entry SURVIVES the request — the signalling loop clears it once
        // the peer is really closed, so the banner does not vanish early.
        assert_eq!(reg.list().len(), 2);
    }

    #[tokio::test]
    async fn disconnecting_an_unknown_session_is_a_clean_false() {
        let (reg, _ids, _rx) = reg_with(1);
        assert!(!reg.disconnect(&ObjectId::new(), Via::LocalApi));
    }

    /// What `f` logs, as the daemon's log file writes it.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
    )]
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        // Two dispatchers, never one: with one, a callsite's interest is
        // taken once for every thread.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    /// #4001 — a session the person at the device ended says so, and names
    /// what asked as a field. Until #4001 every one of them, the banner's and
    /// `roomlerd rc --disconnect`'s too, read "via overlay badge", and only
    /// Windows has a badge. Field reads grep this text.
    #[test]
    fn the_host_ended_line_says_the_person_ended_it_and_names_what_asked() {
        let session = ObjectId::new();
        let id = format!("session_id={session}");
        for (via, field) in [
            (Via::OverlayBadge, r#"via="overlay_badge""#),
            (Via::LocalApi, r#"via="localapi""#),
            (Via::Notification, r#"via="being_viewed_notification""#),
            (Via::ShareStop, r#"via="desktop_share_stop""#),
        ] {
            let line = logged(|| log_host_ended(session, via));
            for want in [
                "INFO",
                "the person at the device ended the session",
                id.as_str(),
                field,
            ] {
                assert!(line.contains(want), "{want:?} not in {line:?}");
            }
            assert!(!line.contains("overlay badge"), "{line:?}");
            assert_eq!(line.lines().count(), 1, "{line:?}");
        }
    }

    /// A dead loop (its receiver dropped) must report failure rather than
    /// silently swallowing the click — the operator is watching for an effect.
    #[tokio::test]
    async fn a_closed_kill_channel_reports_failure() {
        let (reg, ids, rx) = reg_with(1);
        drop(rx);
        assert!(!reg.disconnect(&ids[0], Via::OverlayBadge));
    }

    #[test]
    fn list_is_stable_and_carries_the_grant() {
        let (reg, ids, _rx) = reg_with(3);
        let a = reg.list();
        let b = reg.list();
        assert_eq!(a, b, "two polls must not reorder the banner");
        assert_eq!(a.len(), 3);
        assert!(a.iter().all(|s| s.permissions == "VIEW | INPUT"));
        let listed: std::collections::HashSet<String> =
            a.iter().map(|s| s.session_id.clone()).collect();
        for id in &ids {
            assert!(listed.contains(&id.to_hex()));
        }
    }

    #[test]
    fn remove_drops_the_session() {
        let (reg, ids, _rx) = reg_with(2);
        reg.remove(&ids[0]);
        let left = reg.list();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].session_id, ids[1].to_hex());
    }

    /// FR-85 P3 — the banner's "recording" follows the session, survives a
    /// re-announce (which would otherwise hide a recording in progress), and
    /// cannot be set on a session the banner does not show.
    #[test]
    fn recording_is_marked_per_session_and_survives_a_re_announce() {
        let (reg, ids, _rx) = reg_with(2);
        assert!(reg.list().iter().all(|s| !s.recording));
        assert!(reg.set_recording(&ids[1], true));
        let listed = reg.list();
        assert!(!listed[0].recording && listed[1].recording);

        let (tx, _rx2) = tokio::sync::mpsc::channel(1);
        reg.insert(ids[1], "viewer1".into(), "VIEW".into(), String::new(), tx);
        assert!(
            reg.list()[1].recording,
            "a re-announce cleared a recording the host must still see"
        );

        assert!(reg.set_recording(&ids[1], false));
        assert!(reg.list().iter().all(|s| !s.recording));
        assert!(
            !reg.set_recording(&ObjectId::new(), true),
            "no banner, no recording"
        );
    }

    /// FR-85 P3b-3 — a session that ends while RECORDING keeps its banner
    /// entry, marked reconnecting, for as long as the recording lasts; one
    /// that was only watching goes at once. When the recording ends (or moves
    /// to the controller's next session), the kept entry goes too.
    #[test]
    fn a_recording_keeps_its_banner_after_its_session_ends() {
        let (reg, ids, _rx) = reg_with(2);
        assert!(reg.set_recording(&ids[1], true));
        assert!(!reg.session_ended(&ids[0]), "a watcher's entry is not kept");
        assert!(reg.session_ended(&ids[1]), "a recording's entry is kept");
        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, ids[1].to_hex());
        assert!(listed[0].recording && listed[0].reconnecting, "{listed:?}");
        // Nothing to disconnect: the session is gone, only the recording is
        // left, and the banner's Stop ends that.
        assert!(!reg.disconnect(&ids[1], Via::LocalApi));

        assert!(
            reg.recording_ended(&ids[1]),
            "the kept entry goes with its recording"
        );
        assert!(reg.list().is_empty());
    }

    /// The same ending on a LIVE session only clears "recording": the session
    /// itself is still there to be shown.
    #[test]
    fn a_recording_that_ends_on_a_live_session_leaves_the_session() {
        let (reg, ids, _rx) = reg_with(1);
        assert!(reg.set_recording(&ids[0], true));
        assert!(!reg.recording_ended(&ids[0]));
        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].recording && !listed[0].reconnecting);
        // An unknown session is nothing to end.
        assert!(!reg.recording_ended(&ObjectId::new()));
    }

    #[test]
    fn a_reconnecting_entry_is_said_on_the_wire_and_absent_otherwise() {
        let (reg, ids, _rx) = reg_with(1);
        let plain = serde_json::to_value(&reg.list()[0]).unwrap();
        assert!(plain.get("reconnecting").is_none(), "{plain}");
        reg.set_recording(&ids[0], true);
        reg.session_ended(&ids[0]);
        let kept = serde_json::to_value(&reg.list()[0]).unwrap();
        assert_eq!(kept["reconnecting"], true, "{kept}");
    }

    /// FR-93 P6h — a session lists as asking while a mark for it lives, and
    /// stops the moment the last one goes: a second wait replacing the first
    /// (the open moving on) must not make it flicker off in between.
    #[test]
    fn a_session_is_asking_while_its_mark_lives() {
        let (reg, ids, _rx) = reg_with(2);
        let asking = |reg: &RcSessionRegistry| -> Vec<bool> {
            reg.list().iter().map(|s| s.asking).collect()
        };
        assert_eq!(asking(&reg), [false, false]);
        let first = AskingMark::new(ids[0]);
        assert_eq!(asking(&reg), [true, false]);
        let second = AskingMark::new(ids[0]);
        drop(first);
        assert_eq!(
            asking(&reg),
            [true, false],
            "a replaced mark still holds it"
        );
        drop(second);
        assert_eq!(asking(&reg), [false, false]);
        // Additive on the wire: a session nobody asks about carries no field.
        let row = serde_json::to_value(&reg.list()[0]).unwrap();
        assert!(row.get("asking").is_none(), "{row}");
    }
}
