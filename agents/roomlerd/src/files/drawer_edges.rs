// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3682 — the file drawer's reads, to their edges: what a listing or a
//! download touches, as whom, for how long, and what its refusal says.
//!
//! #3669 made a machine-wide Windows install read as the person signed in at
//! the console, one step at a time ([`FsAs::run`]). These tests pin the reads
//! that still ran outside such a step, or without a bound, or whose refusal
//! did not say what was refused and why.

use super::*;

/// The names a listing holds, in its order.
fn names(listing: &DirListing) -> Vec<String> {
    listing.entries.iter().map(|e| e.name.clone()).collect()
}

/// What the viewer reads of an answer: an error's outermost line, the only
/// one it shows (#3676).
fn outermost<T>(answer: Result<T>) -> String {
    match answer {
        Ok(_) => "answered".to_string(),
        Err(e) => format!("{e}"),
    }
}

/// The stand-ins these tests put where production code reads, each reached
/// from it under `#[cfg(test)]`, as [`stand_in::reader_now`] is.
pub(crate) mod seams {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// The folders resolving answers "access denied" at or under.
    static UNRESOLVABLE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// While held, every path at or under its folder answers resolving
    /// ([`super::super::reads::resolve_path`]) with an access refusal: error 5
    /// on Windows, where a runner with its backup privilege enabled resolves
    /// through any DACL, so no real folder can stand in.
    #[cfg(windows)]
    pub(crate) struct Unresolvable(PathBuf);

    #[cfg(windows)]
    pub(crate) fn unresolvable(folder: &Path) -> Unresolvable {
        let mut held = UNRESOLVABLE.lock().unwrap_or_else(|p| p.into_inner());
        held.push(folder.to_path_buf());
        Unresolvable(folder.to_path_buf())
    }

    #[cfg(windows)]
    impl Drop for Unresolvable {
        fn drop(&mut self) {
            let mut held = UNRESOLVABLE.lock().unwrap_or_else(|p| p.into_inner());
            held.retain(|f| f != &self.0);
        }
    }

    /// What resolving `asked` answers instead, while a stand-in holds it.
    pub(crate) fn resolving(asked: &Path) -> Option<std::io::Error> {
        let held = UNRESOLVABLE.lock().unwrap_or_else(|p| p.into_inner());
        held.iter().any(|f| asked.starts_with(f)).then(|| {
            #[cfg(windows)]
            let refused = std::io::Error::from_raw_os_error(5);
            #[cfg(not(windows))]
            let refused = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
            refused
        })
    }

    /// One held folder's gate: shut until the test opens it.
    #[derive(Default)]
    pub(crate) struct Gate {
        open: Mutex<bool>,
        opened: std::sync::Condvar,
        reached: std::sync::atomic::AtomicUsize,
    }

    /// The folders whose entries, when read, wait at their gate.
    static HELD: Mutex<Vec<(PathBuf, std::sync::Arc<Gate>)>> = Mutex::new(Vec::new());

    /// A folder standing in for one on a share that has stopped answering:
    /// reading its entries ([`super::super::reads::read_entries`]) waits until
    /// [`Held::release`], or the guard's drop.
    pub(crate) struct Held {
        at: PathBuf,
        gate: std::sync::Arc<Gate>,
    }

    pub(crate) fn hold_entries(folder: &Path) -> Held {
        let at = std::fs::canonicalize(folder).expect("a folder to hold");
        let gate = std::sync::Arc::new(Gate::default());
        let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
        held.push((at.clone(), gate.clone()));
        Held { at, gate }
    }

    impl Held {
        /// How many reads of its entries began.
        pub(crate) fn reached(&self) -> usize {
            self.gate.reached.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Whether a read of its entries began within `wait`.
        pub(crate) async fn reached_within(&self, wait: std::time::Duration) -> bool {
            let until = std::time::Instant::now() + wait;
            while self.reached() == 0 && std::time::Instant::now() < until {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            self.reached() > 0
        }

        /// The share answers: every read of its entries goes on, now and
        /// from now.
        pub(crate) fn release(&self) {
            *self.gate.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.gate.opened.notify_all();
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            self.release();
            let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
            held.retain(|(at, _)| at != &self.at);
        }
    }

    /// Where a listing reads the entries of `at`: waits while a test holds it.
    pub(crate) fn entries_wait(at: &Path) {
        let gate = {
            let held = HELD.lock().unwrap_or_else(|p| p.into_inner());
            held.iter().find(|(f, _)| f == at).map(|(_, g)| g.clone())
        };
        let Some(gate) = gate else { return };
        gate.reached
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut open = gate.open.lock().unwrap_or_else(|p| p.into_inner());
        while !*open {
            open = gate.opened.wait(open).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// #4082 — one held step's gate: shut until the test opens it.
    struct StepGate {
        open: tokio::sync::watch::Sender<bool>,
        reached: std::sync::atomic::AtomicUsize,
    }

    impl StepGate {
        fn shut() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                open: tokio::sync::watch::Sender::new(false),
                reached: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn reached(&self) -> usize {
            self.reached.load(std::sync::atomic::Ordering::SeqCst)
        }

        async fn reached_within(&self, wait: std::time::Duration) -> bool {
            let until = std::time::Instant::now() + wait;
            while self.reached() == 0 && std::time::Instant::now() < until {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            self.reached() > 0
        }

        /// A step reached the gate: it waits there until the gate opens.
        async fn pass(&self) {
            self.reached
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut open = self.open.subscribe();
            let _ = open.wait_for(|open| *open).await;
        }

        fn open(&self) {
            self.open.send_replace(true);
        }
    }

    /// The folders an upload's end waits at, when it places a file into one.
    static PLACING: Mutex<Vec<(PathBuf, std::sync::Arc<StepGate>)>> = Mutex::new(Vec::new());

    /// #4082 — a folder standing in for one whose upload's end takes long (a
    /// large file copied across to the person's folder or to another volume,
    /// a share that stopped answering): an upload's end that places a file
    /// into it ([`super::super::FilesHandler::end`]) waits there until
    /// [`HeldPlacing::release`], or the guard's drop, and then places it as
    /// it would have.
    pub(crate) struct HeldPlacing {
        at: PathBuf,
        gate: std::sync::Arc<StepGate>,
    }

    pub(crate) fn hold_placing(folder: &Path) -> HeldPlacing {
        let at = std::fs::canonicalize(folder).expect("a folder to hold");
        let gate = StepGate::shut();
        let mut held = PLACING.lock().unwrap_or_else(|p| p.into_inner());
        held.push((at.clone(), gate.clone()));
        HeldPlacing { at, gate }
    }

    impl HeldPlacing {
        /// Whether an end began to wait at it within `wait`.
        pub(crate) async fn reached_within(&self, wait: std::time::Duration) -> bool {
            self.gate.reached_within(wait).await
        }

        /// The copy is done: every end waiting at it goes on, now and from
        /// now.
        pub(crate) fn release(&self) {
            self.gate.open();
        }
    }

    impl Drop for HeldPlacing {
        fn drop(&mut self) {
            self.release();
            let mut held = PLACING.lock().unwrap_or_else(|p| p.into_inner());
            held.retain(|(at, gate)| at != &self.at || !std::sync::Arc::ptr_eq(gate, &self.gate));
        }
    }

    /// Where an upload's end places its file into `dest_dir`: waits while a
    /// test holds that folder.
    pub(crate) async fn placing_waits(dest_dir: &Path) {
        let at = std::fs::canonicalize(dest_dir).unwrap_or_else(|_| dest_dir.to_path_buf());
        let gate = {
            let held = PLACING.lock().unwrap_or_else(|p| p.into_inner());
            held.iter().find(|(f, _)| f == &at).map(|(_, g)| g.clone())
        };
        if let Some(gate) = gate {
            gate.pass().await;
        }
    }

    /// The downloads that wait before their first chunk, by id.
    static SENDING: Mutex<Vec<(String, std::sync::Arc<StepGate>)>> = Mutex::new(Vec::new());

    /// #4082 — a download whose bytes are still on their way (a large file
    /// over a slow link): its pump has sent the offer, and waits before its
    /// first chunk until the guard goes. Its stop token ends that wait, as it
    /// ends every wait of the pump. A test never lets one go on into its
    /// chunks: the test viewer reads a message of at most 65535 bytes, and a
    /// chunk is 64 KiB.
    pub(crate) struct HeldSending {
        id: String,
        gate: std::sync::Arc<StepGate>,
    }

    pub(crate) fn hold_sending(id: &str) -> HeldSending {
        let gate = StepGate::shut();
        let mut held = SENDING.lock().unwrap_or_else(|p| p.into_inner());
        held.push((id.to_string(), gate.clone()));
        HeldSending {
            id: id.to_string(),
            gate,
        }
    }

    impl HeldSending {
        /// Whether its pump began to wait within `wait`.
        pub(crate) async fn reached_within(&self, wait: std::time::Duration) -> bool {
            self.gate.reached_within(wait).await
        }
    }

    impl Drop for HeldSending {
        fn drop(&mut self) {
            self.gate.open();
            let mut held = SENDING.lock().unwrap_or_else(|p| p.into_inner());
            held.retain(|(id, gate)| id != &self.id || !std::sync::Arc::ptr_eq(gate, &self.gate));
        }
    }

    /// Where a download's pump sends its first chunk: waits while a test
    /// holds that download.
    pub(crate) async fn sending_waits(id: &str) {
        let gate = {
            let held = SENDING.lock().unwrap_or_else(|p| p.into_inner());
            held.iter()
                .find(|(held, _)| held == id)
                .map(|(_, g)| g.clone())
        };
        if let Some(gate) = gate {
            gate.pass().await;
        }
    }

    /// What a begin runs, once, after it has made (or opened) the staging
    /// folder beside the folder it uploads into and before it makes its own
    /// `<id>` in it: by that folder's path.
    type IdNext = (PathBuf, Box<dyn FnOnce() + Send>);

    static ID_NEXT: Mutex<Vec<IdNext>> = Mutex::new(Vec::new());

    /// #4082 — once a begin into `folder` has made its staging folder, and
    /// before it makes its `<id>` there, `step` runs on the begin's thread:
    /// another upload's end, placing its file beside this begin, removes
    /// that staging folder once empty, between the two.
    pub(crate) fn before_the_id(folder: &Path, step: impl FnOnce() + Send + 'static) {
        let at = std::fs::canonicalize(folder).expect("a folder to upload into");
        let mut next = ID_NEXT.lock().unwrap_or_else(|p| p.into_inner());
        next.push((at, Box::new(step)));
    }

    /// Where a begin into `folder` makes its `<id>`: runs what a test set
    /// with [`before_the_id`], once.
    pub(crate) fn the_id_is_next(folder: &Path) {
        let at = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
        let step = {
            let mut next = ID_NEXT.lock().unwrap_or_else(|p| p.into_inner());
            let found = next.iter().position(|(f, _)| f == &at);
            found.map(|i| next.remove(i).1)
        };
        if let Some(step) = step {
            step();
        }
    }

    /// What a stand-in answers for one thread: a test's own, as
    /// `stand_in::reader` keeps it.
    #[cfg(windows)]
    struct PerThread<T>(Mutex<Vec<(std::thread::ThreadId, T)>>);

    #[cfg(windows)]
    impl<T: Clone> PerThread<T> {
        const fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }

        fn stand(&self, what: T) {
            let mut stood = self.0.lock().unwrap_or_else(|p| p.into_inner());
            stood.push((std::thread::current().id(), what));
        }

        fn now(&self) -> Option<T> {
            let here = std::thread::current().id();
            let stood = self.0.lock().unwrap_or_else(|p| p.into_inner());
            stood
                .iter()
                .find(|(t, _)| *t == here)
                .map(|(_, w)| w.clone())
        }

        fn leave(&self, thread: std::thread::ThreadId) {
            let mut stood = self.0.lock().unwrap_or_else(|p| p.into_inner());
            stood.retain(|(t, _)| *t != thread);
        }
    }

    /// The machine's staging folder, stood in for this thread
    /// ([`super::super::reads::staging_root`]).
    #[cfg(windows)]
    static STAGING_ROOT: PerThread<PathBuf> = PerThread::new();

    #[cfg(windows)]
    pub(crate) struct StagingStoodIn(std::thread::ThreadId);

    #[cfg(windows)]
    pub(crate) fn staging_root(root: &Path) -> StagingStoodIn {
        STAGING_ROOT.stand(root.to_path_buf());
        StagingStoodIn(std::thread::current().id())
    }

    #[cfg(windows)]
    impl Drop for StagingStoodIn {
        fn drop(&mut self) {
            STAGING_ROOT.leave(self.0);
        }
    }

    #[cfg(windows)]
    pub(crate) fn staging_root_now() -> Option<PathBuf> {
        STAGING_ROOT.now()
    }

    /// A Windows person lookup stood in for this thread (#3682 item 2): the
    /// process facts it is decided from, and the lookup itself.
    #[cfg(windows)]
    #[derive(Clone)]
    pub(crate) struct PersonLookup {
        /// Whether this process's token is LocalSystem's (`None`: unread).
        pub(crate) own_token: Option<bool>,
        /// `ROOMLERD_FILES_AS_SEAT_USER`.
        pub(crate) switch_on: bool,
        pub(crate) lookup:
            std::sync::Arc<dyn Fn() -> Option<crate::win_identity::SessionPerson> + Send + Sync>,
        /// The stand-in's own lookup in flight, as `reads::PERSON` is the
        /// process's: a test's lookups never join another test's.
        pub(crate) flight: std::sync::Arc<
            crate::one_in_flight::OneInFlight<Option<crate::win_identity::SessionPerson>>,
        >,
    }

    #[cfg(windows)]
    static PERSON_LOOKUP: PerThread<PersonLookup> = PerThread::new();

    #[cfg(windows)]
    pub(crate) struct PersonStoodIn(std::thread::ThreadId);

    #[cfg(windows)]
    pub(crate) fn person_lookup(
        own_token: Option<bool>,
        switch_on: bool,
        lookup: impl Fn() -> Option<crate::win_identity::SessionPerson> + Send + Sync + 'static,
    ) -> PersonStoodIn {
        PERSON_LOOKUP.stand(PersonLookup {
            own_token,
            switch_on,
            lookup: std::sync::Arc::new(lookup),
            flight: std::sync::Arc::new(crate::one_in_flight::OneInFlight::new(
                "person-lookup-test",
            )),
        });
        PersonStoodIn(std::thread::current().id())
    }

    #[cfg(windows)]
    impl Drop for PersonStoodIn {
        fn drop(&mut self) {
            PERSON_LOOKUP.leave(self.0);
        }
    }

    #[cfg(windows)]
    pub(crate) fn person_lookup_now() -> Option<PersonLookup> {
        PERSON_LOOKUP.now()
    }
}

/// #3682 item 3 — one bound covers a whole listing: resolving the folder,
/// opening it and reading its entries. The entries were read after the
/// bound, so a share that answered the open and then stopped held the
/// listing for as long as it liked. And (D1-B) a listing runs off the files
/// channel's read loop, one step at a time per channel: a step stuck on a
/// share keeps the one thread it stranded, never more.
mod one_bound_over_a_listing {
    use super::*;
    use std::time::{Duration, Instant};

    /// The listing's bound.
    const BOUND: Duration = Duration::from_secs(DIR_LIST_TIMEOUT_SECS);
    /// The room a test gives an answer past it.
    const ROOM: Duration = Duration::from_secs(1);

    /// A folder holding one file, `a.txt`.
    fn folder(temp: &std::path::Path, name: &str) -> PathBuf {
        let folder = temp.join(name);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("a.txt"), b"a").unwrap();
        folder
    }

    /// A listing on `h`'s channel, as this agent's own account, in a task.
    fn listing(
        h: &FilesHandler,
        folder: &std::path::Path,
    ) -> tokio::task::JoinHandle<Result<DirListing>> {
        let (h, folder) = (h.clone(), folder.to_string_lossy().into_owned());
        tokio::spawn(async move { h.list_as(&FsAs::Daemon, &folder).await })
    }

    /// A folder whose entries never come answers `listing timed out` within
    /// the bound, where the read of its entries waited for as long as the
    /// share did.
    #[tokio::test]
    async fn a_folder_whose_entries_do_not_come_answers_within_its_bound() {
        let temp = tempfile::tempdir().unwrap();
        let slow = folder(temp.path(), "slow");
        let held = seams::hold_entries(&slow);
        let started = Instant::now();
        let answered = tokio::time::timeout(
            BOUND + 3 * ROOM,
            list_dir_as(&FsAs::Daemon, &slow.to_string_lossy()),
        )
        .await;
        let waited = started.elapsed();
        let reached = held.reached();
        // The share answers: its thread goes free before anything is asserted.
        held.release();
        assert_eq!(reached, 1, "its entries were asked for once");
        let Ok(answered) = answered else {
            panic!(
                "no answer within {:?}: the entries' read is not bounded",
                BOUND + 3 * ROOM
            );
        };
        assert_eq!(outermost(answered), "listing timed out");
        assert!(waited < BOUND + ROOM, "answered after {waited:?}");
    }

    /// While a listing's step is stuck past its bound, the channel's next
    /// listing is answered at once, naming the folder that is not answering,
    /// and starts no step of its own: a dead share strands one thread per
    /// channel, however often the viewer asks.
    #[tokio::test]
    async fn behind_a_stuck_listing_the_next_is_answered_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let (dead, next) = (folder(temp.path(), "dead"), folder(temp.path(), "next"));
        let (held_dead, held_next) = (seams::hold_entries(&dead), seams::hold_entries(&next));
        let h = FilesHandler::new();
        let first = tokio::time::timeout(BOUND + ROOM, listing(&h, &dead)).await;
        let started = Instant::now();
        let second = tokio::time::timeout(2 * ROOM, listing(&h, &next)).await;
        let waited = started.elapsed();
        let (dead_reached, next_reached) = (held_dead.reached(), held_next.reached());
        held_dead.release();
        held_next.release();
        assert_eq!(
            dead_reached, 1,
            "the first listing's entries were asked for"
        );
        let Ok(Ok(first)) = first else {
            panic!("the first listing gave no answer within its bound");
        };
        assert_eq!(outermost(first), "listing timed out");
        let Ok(Ok(second)) = second else {
            panic!("the next listing waited on the stuck one ({waited:?})");
        };
        assert_eq!(
            next_reached, 0,
            "the next listing began a step behind the stuck one"
        );
        let said = outermost(second);
        let dead = dead.to_string_lossy();
        for words in [&*dead, "is not answering", "Try again"] {
            assert!(said.contains(words), "{words:?} is not in {said:?}");
        }
        assert!(waited < ROOM / 2, "answered after {waited:?}");
    }

    /// Quick navigation is never refused: a listing that arrives while
    /// another is in progress, not stuck, waits for it and gets its own.
    #[tokio::test]
    async fn behind_a_listing_in_progress_the_next_gets_its_listing() {
        let temp = tempfile::tempdir().unwrap();
        let (slow, next) = (folder(temp.path(), "slow"), folder(temp.path(), "next"));
        let held = seams::hold_entries(&slow);
        let h = FilesHandler::new();
        let first = listing(&h, &slow);
        let reached = held.reached_within(BOUND / 2).await;
        let second = listing(&h, &next);
        tokio::time::sleep(Duration::from_millis(300)).await;
        held.release();
        let first = tokio::time::timeout(BOUND + ROOM, first).await;
        let second = tokio::time::timeout(BOUND + ROOM, second).await;
        assert!(reached, "the first listing's entries were asked for");
        both_listed(first, second);
    }

    /// A listing's task, awaited within some time.
    type Awaited =
        Result<Result<Result<DirListing>, tokio::task::JoinError>, tokio::time::error::Elapsed>;

    /// Both listings, each its folder's one file, or which one failed how.
    fn both_listed(first: Awaited, second: Awaited) {
        for (which, answered) in [("first", first), ("second", second)] {
            let listing = match answered {
                Ok(Ok(Ok(listing))) => listing,
                Ok(Ok(Err(e))) => panic!("the {which} listing was refused: {e:#}"),
                Ok(Err(e)) => panic!("the {which} listing's task: {e}"),
                Err(_) => panic!("the {which} listing gave no answer in time"),
            };
            assert_eq!(names(&listing), vec!["a.txt"], "the {which} listing");
        }
    }

    /// A share that answers each listing in 2 s, SMB over a corporate VPN:
    /// a listing asked 0.5 s after another waits for it, and its own step
    /// then has the whole bound. Its bound ran from when it was asked, so it
    /// timed out at 3.5 s, half a second before its step would have ended.
    #[tokio::test]
    async fn behind_a_slow_listing_the_next_has_its_own_steps_whole_bound() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (folder(temp.path(), "a"), folder(temp.path(), "b"));
        let (held_a, held_b) = (seams::hold_entries(&a), seams::hold_entries(&b));
        let h = FilesHandler::new();
        let started = tokio::time::Instant::now();
        let first = listing(&h, &a);
        tokio::time::sleep_until(started + Duration::from_millis(500)).await;
        let second = listing(&h, &b);
        tokio::time::sleep_until(started + Duration::from_secs(2)).await;
        held_a.release();
        let second_began = held_b.reached_within(BOUND).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        held_b.release();
        let first = tokio::time::timeout(ROOM, first).await;
        let second = tokio::time::timeout(2 * ROOM, second).await;
        assert!(second_began, "the second listing's step never began");
        both_listed(first, second);
    }

    /// The edge of it: the first listing takes most of its bound, 2.5 s, and
    /// one asked 0.1 s after it needs 1.0 s. Its bound ran from when it was
    /// asked, so it timed out at 3.1 s, 0.4 s before its step would have
    /// ended. Now the first ends 0.5 s inside its bound and the second 1.1 s
    /// inside its 4.5 s: no margin here is under 0.4 s, on a loaded runner too.
    #[tokio::test]
    async fn behind_a_long_listing_one_asked_at_once_gets_its_listing() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (folder(temp.path(), "a"), folder(temp.path(), "b"));
        let (held_a, held_b) = (seams::hold_entries(&a), seams::hold_entries(&b));
        let h = FilesHandler::new();
        let started = tokio::time::Instant::now();
        let first = listing(&h, &a);
        tokio::time::sleep_until(started + Duration::from_millis(100)).await;
        let second = listing(&h, &b);
        tokio::time::sleep_until(started + Duration::from_millis(2500)).await;
        held_a.release();
        let second_began = held_b.reached_within(BOUND).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        held_b.release();
        let first = tokio::time::timeout(ROOM, first).await;
        let second = tokio::time::timeout(2 * ROOM, second).await;
        assert!(second_began, "the second listing's step never began");
        both_listed(first, second);
    }

    /// A listing already past its own bound when it reaches the lane answers
    /// `listing timed out` and starts no step. A step it started would hold
    /// the lane, for an answer nobody waits for.
    #[tokio::test]
    async fn a_listing_past_its_own_bound_starts_no_step() {
        let lane = reads::Lane::default();
        let ran = Arc::new(AtomicBool::new(false));
        let marks = ran.clone();
        let by = Instant::now();
        let answered = lane
            .step(&FsAs::Daemon, "past", by, move || {
                marks.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(!ran.load(Ordering::SeqCst), "the listing started a step");
        assert_eq!(outermost(answered), "listing timed out");
    }

    /// While a folder's step is stuck on a share that stopped answering, the
    /// Drives view still answers on that channel. `GetLogicalDrives` asks no
    /// share, so the Drives view needs no turn behind a folder, and it is the
    /// viewer's way back from a dead share. Elsewhere the roots take no lane.
    #[tokio::test]
    async fn behind_a_stuck_listing_the_drives_view_still_answers() {
        let temp = tempfile::tempdir().unwrap();
        let dead = folder(temp.path(), "dead");
        let held = seams::hold_entries(&dead);
        let h = FilesHandler::new();
        let first = tokio::time::timeout(BOUND + ROOM, listing(&h, &dead)).await;
        let started = Instant::now();
        let drives = tokio::time::timeout(2 * ROOM, h.list_as(&FsAs::Daemon, "")).await;
        let waited = started.elapsed();
        held.release();
        let Ok(Ok(first)) = first else {
            panic!("the first listing gave no answer within its bound");
        };
        assert_eq!(outermost(first), "listing timed out");
        let drives = match drives {
            Ok(Ok(drives)) => drives,
            Ok(Err(e)) => panic!("the Drives view was refused: {e:#}"),
            Err(_) => panic!("the Drives view gave no answer within {:?}", 2 * ROOM),
        };
        assert!(!drives.entries.is_empty(), "the Drives view lists nothing");
        assert!(waited < ROOM, "answered after {waited:?}");
    }
}

/// #3682 item 6 — a refusal names what was refused and why, for every
/// reader, as its outermost line. A path the reader may not resolve used to
/// answer a bare label (`validating folder path`, `validating outgoing
/// path`, `canonicalising <path>`) to every reader but the person, whose
/// refusal alone was explained. The daemon reads as its own account, which on
/// a per-user install need not be the person at the screen, so its words say
/// that account.
mod a_refusal_names_what_and_why {
    use super::*;

    /// The OS, as a refusal names it.
    const OS: &str = if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    };

    /// The outermost lines of a folder download of `folder`, a download of
    /// `file` and a listing of `folder`, read as this agent's own account.
    async fn three_reads(folder: &std::path::Path, file: &std::path::Path) -> Vec<String> {
        let (folder, file) = (folder.to_string_lossy(), file.to_string_lossy());
        let zip = FilesHandler::new()
            .begin_outgoing_zip_as(FsAs::Daemon, "z3682-6".into(), &folder)
            .await;
        let get = FilesHandler::new()
            .begin_outgoing_as(FsAs::Daemon, "f3682-6".into(), &file)
            .await;
        let dir = list_dir_as(&FsAs::Daemon, &folder).await;
        vec![outermost(zip), outermost(get), outermost(dir)]
    }

    /// A folder and a file below a folder this agent may not search: on Unix
    /// its mode is 000, and a runner that resolves through it anyway (root)
    /// skips; on Windows a stand-in refuses resolving it.
    #[tokio::test]
    async fn a_path_this_agent_may_not_resolve_is_refused_naming_it_and_why() {
        let temp = tempfile::tempdir().unwrap();
        let shut = temp.path().join("shut");
        let (folder, file) = (shut.join("inner"), shut.join("inner.txt"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(&file, b"x").unwrap();
        #[cfg(unix)]
        let held = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
            struct Reopen(std::path::PathBuf);
            impl Drop for Reopen {
                fn drop(&mut self) {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
                }
            }
            Reopen(shut.clone())
        };
        #[cfg(windows)]
        let held = seams::unresolvable(&shut);
        if reads::resolve_path(&folder).is_ok() {
            eprintln!(
                "SKIPPED a_path_this_agent_may_not_resolve_is_refused_naming_it_and_why: this \
                 runner resolves below a folder it may not search (root reads through mode \
                 000), so this test proved nothing here"
            );
            return;
        }
        let read = three_reads(&folder, &file).await;
        drop(held);
        let why = |asked: &std::path::Path| {
            format!(
                "{}: {OS} refused to open it for the account this agent runs as",
                asked.display()
            )
        };
        assert_eq!(read, vec![why(&folder), why(&file), why(&folder)]);
    }

    /// A folder and a file that are not there: each read names the path and
    /// the OS's own reason, where it used to name a step of the agent's.
    #[tokio::test]
    async fn a_path_that_is_not_there_is_refused_naming_it_and_why() {
        let temp = tempfile::tempdir().unwrap();
        let (folder, file) = (temp.path().join("gone"), temp.path().join("gone.txt"));
        let read = three_reads(&folder, &file).await;
        let why = |asked: &std::path::Path| {
            let gone = std::fs::canonicalize(asked).expect_err("not there");
            format!("{}: {gone}", asked.display())
        };
        assert_eq!(read, vec![why(&folder), why(&file), why(&folder)]);
    }
}

/// The Windows person's cells: a low-integrity copy of the runner's own token
/// stands in for the person ([`stand_in::person`]), as in #3669's tests.
#[cfg(target_os = "windows")]
mod as_the_person {
    use super::*;

    /// One drive of the Drives view: `X:\`.
    fn is_a_drive(name: &str) -> bool {
        let b = name.as_bytes();
        b.len() == 3 && b[0].is_ascii_uppercase() && &b[1..] == b":\\"
    }

    /// #3682 item 1 — the Drives view is read inside a step as the reader:
    /// Windows keeps drive letters per sign-in, so a letter the person mapped
    /// is theirs, not the service's. A person whose token has gone is told so,
    /// where the service's own letters used to answer; the person still signed
    /// in lists the drives of their own sign-in, which is the runner's here;
    /// the daemon lists its own.
    #[tokio::test]
    async fn the_drives_view_is_read_as_the_reader_not_the_service() {
        let temp = tempfile::tempdir().unwrap();
        for path in ["", "~"] {
            let gone = stand_in::person_signing_out_after(0, temp.path());
            let Err(why) = list_dir_as(&gone, path).await else {
                panic!("{path:?}: the Drives view answered for a person who has gone");
            };
            assert_eq!(format!("{why}"), NOT_THE_PERSON, "{path:?}");
        }
        let daemon = list_dir_as(&FsAs::Daemon, "")
            .await
            .expect("the daemon's drives");
        assert_eq!(daemon.path, "Drives");
        assert!(
            !daemon.entries.is_empty() && daemon.entries.iter().all(|e| is_a_drive(&e.name)),
            "{:?}",
            names(&daemon)
        );
        let me = stand_in::person(temp.path(), temp.path());
        let theirs = list_dir_as(&me, "~").await.expect("the person's drives");
        assert_eq!(theirs.path, "Drives");
        assert_eq!(
            names(&theirs),
            names(&daemon),
            "one sign-in, one set of letters"
        );
    }

    /// What a read or a write starting now is told when the person could not
    /// be looked up in time.
    const TRY_AGAIN: &str = "Could not tell who is signed in to this computer. Try again.";

    /// Who a read (`false`) or a write (`true`) starting now acts as.
    async fn who_for(writing: bool) -> FsAs {
        if writing {
            FsAs::for_write().await
        } else {
            FsAs::for_read().await
        }
    }

    /// #3682 item 2 — on a machine-wide install, the person a read or a write
    /// starting now acts as is looked up off the async worker, within its
    /// wait (1.5 s, as the Linux seat0 lookup's): the lookup calls the
    /// session manager and reads the person's registry, and the worker it ran
    /// on serves the whole files channel. A lookup that does not answer in
    /// time refuses in try-again words, never as the service.
    #[tokio::test(flavor = "current_thread")]
    async fn a_person_lookup_past_its_time_refuses_in_try_again_words() {
        let wait = std::time::Duration::from_millis(1500);
        let worker = std::thread::current().id();
        let asked_on = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = asked_on.clone();
        let _stood = seams::person_lookup(Some(true), true, move || {
            recorded.lock().unwrap().push(std::thread::current().id());
            std::thread::sleep(std::time::Duration::from_secs(3));
            None
        });
        for writing in [false, true] {
            let started = std::time::Instant::now();
            let who = who_for(writing).await;
            let waited = started.elapsed();
            let ran_on = asked_on.lock().unwrap().clone();
            assert!(
                !ran_on.is_empty() && !ran_on.contains(&worker),
                "writing: {writing}: the lookup ran on the async worker"
            );
            assert!(
                waited < wait + std::time::Duration::from_secs(1),
                "writing: {writing}: waited {waited:?} for a lookup given {wait:?}"
            );
            assert_eq!(
                who.refusal(),
                Some(TRY_AGAIN),
                "writing: {writing}: {who:?}"
            );
            assert!(
                !who.is_the_daemon(),
                "writing: {writing}: never the service"
            );
        }
    }

    /// #3682 item 2 — a lookup that answers in time decides as before: the
    /// person, or nobody; and the process's own facts (another account, or
    /// `ROOMLERD_FILES_AS_SEAT_USER=0`) decide the daemon with no lookup.
    #[tokio::test(flavor = "current_thread")]
    async fn a_person_lookup_in_time_decides_as_before() {
        let temp = tempfile::tempdir().unwrap();
        let person = crate::win_identity::SessionPerson {
            sid: "S-1-5-21-1-2-3-1001".into(),
            profile: temp.path().to_path_buf(),
            downloads: None,
        };
        for writing in [false, true] {
            let found = person.clone();
            let stood = seams::person_lookup(None, true, move || Some(found.clone()));
            let who = who_for(writing).await;
            drop(stood);
            assert_eq!(
                who.person_sid(),
                Some("S-1-5-21-1-2-3-1001"),
                "writing: {writing}"
            );
            let stood = seams::person_lookup(Some(true), true, || None);
            let who = who_for(writing).await;
            drop(stood);
            assert_eq!(who.refusal(), Some(NOBODY_SIGNED_IN), "writing: {writing}");
            for (own_token, switch_on) in [(Some(false), true), (Some(true), false)] {
                let stood = seams::person_lookup(own_token, switch_on, || {
                    panic!("looked up where the process decides")
                });
                let who = who_for(writing).await;
                drop(stood);
                assert!(
                    who.is_the_daemon(),
                    "writing: {writing}: {own_token:?} {switch_on}"
                );
            }
        }
    }

    /// #3682 item 5 — the Upload staging folder is the service's own: on a
    /// machine-wide install only SYSTEM and the Administrators may open it,
    /// so a standard user, or an administrator while UAC is on, is refused.
    /// The refusal says that, and how an administrator reads it, where it
    /// said only that Windows refused. The daemon still lists it.
    #[tokio::test]
    async fn the_staging_folder_refused_to_the_person_says_why() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("a.part"), b"a").unwrap();
        stand_in::label_no_read_up(&staging);
        let _root = seams::staging_root(&staging);
        let me = stand_in::person(temp.path(), temp.path());
        let said = outermost(list_dir_as(&me, STAGING_SENTINEL).await);
        let folder = staging.to_string_lossy();
        for words in [
            &*folder,
            "the Roomler service's own folder",
            "a standard user",
            "while UAC is on",
            "roomler exec",
        ] {
            assert!(said.contains(words), "{words:?} is not in {said:?}");
        }
        let listed = list_dir_as(&FsAs::Daemon, STAGING_SENTINEL)
            .await
            .expect("the daemon lists its own folder");
        assert_eq!(names(&listed), vec!["a.part"]);
    }
}
