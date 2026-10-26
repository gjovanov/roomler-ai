// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// The bounds a test runs at.
fn quick() -> Timing {
    Timing::quick()
}

/// What one fake worker does on its channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    /// The worker's own loop, as `roomlerd hive-worker` runs it.
    Serve,
    /// Ready, then gone.
    DieAfterReady,
    /// Holds its channel and never answers.
    NeverReady,
    /// Ready, and then deaf to every heartbeat, and to `bye`.
    NoBeats,
    /// Gone at once.
    DieAtOnce,
    /// Ready, at another build's version.
    Foreign,
    /// The worker's own loop for this many milliseconds, then gone.
    LiveFor(u64),
}

/// A run nobody ends.
fn nobody() -> GiveUp {
    Arc::new(|_| {})
}

/// A run whose end is recorded: each reason it was ended for.
fn ended_runs() -> (GiveUp, Arc<Mutex<Vec<String>>>) {
    let ended = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&ended);
    let give_up: GiveUp = Arc::new(move |why| record.lock().unwrap().push(why));
    (give_up, ended)
}

/// A worker's `ready`, at this build's version.
fn ready() -> Msg {
    Msg::Ready {
        pid: 1,
        version: VERSION.to_string(),
    }
}

/// Workers as tasks, each playing the next part of `plan` (the last part
/// again once the plan runs out), each life logged.
struct Fake {
    plan: Vec<Part>,
    started: AtomicUsize,
    log: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn new(plan: &[Part]) -> Arc<Self> {
        Arc::new(Self {
            plan: plan.to_vec(),
            started: AtomicUsize::new(0),
            log: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

struct Task(tokio::task::JoinHandle<()>);

#[async_trait::async_trait]
impl Running for Task {
    fn pid(&self) -> Option<u32> {
        None
    }

    async fn kill(&mut self) {
        self.0.abort();
    }
}

impl Spawn for Fake {
    fn spawn(
        &self,
        theirs: std::os::unix::net::UnixStream,
        descriptors: std::os::unix::net::UnixStream,
    ) -> std::io::Result<Box<dyn Running>> {
        let k = self.started.fetch_add(1, Ordering::SeqCst);
        let part = *self.plan.get(k).or(self.plan.last()).unwrap();
        theirs.set_nonblocking(true)?;
        descriptors.set_nonblocking(true)?;
        let mut ch = UnixStream::from_std(theirs)?;
        let descriptors = UnixStream::from_std(descriptors)?;
        let log = Arc::clone(&self.log);
        let note = move |what: String| log.lock().unwrap().push(format!("#{k} {what}"));
        let task = tokio::spawn(async move {
            match part {
                Part::Serve => {
                    let ended = serve(ch, Some(descriptors)).await;
                    note(format!("served: {ended:?}"));
                }
                Part::DieAfterReady => {
                    let _ = recv(&mut ch).await;
                    let _ = send(&mut ch, &ready()).await;
                    note("died after ready".into());
                }
                Part::NeverReady => {
                    note("never ready".into());
                    std::future::pending::<()>().await;
                    drop(ch);
                }
                Part::NoBeats => {
                    let _ = recv(&mut ch).await;
                    let _ = send(&mut ch, &ready()).await;
                    note("ready, deaf".into());
                    while let Ok(Some(_)) = recv(&mut ch).await {}
                    std::future::pending::<()>().await;
                }
                Part::DieAtOnce => {
                    note("died at once".into());
                    drop(ch);
                }
                Part::Foreign => {
                    let _ = recv(&mut ch).await;
                    let other = Msg::Ready {
                        pid: 1,
                        version: "0.0.0-another".into(),
                    };
                    let _ = send(&mut ch, &other).await;
                    note("foreign".into());
                    std::future::pending::<()>().await;
                }
                Part::LiveFor(ms) => {
                    let served = serve(ch, Some(descriptors));
                    let _ = tokio::time::timeout(Duration::from_millis(ms), served).await;
                    note("lived".into());
                }
            }
        });
        Ok(Box::new(Task(task)))
    }
}

/// Until `ok` holds of what `w` has seen, within 5 s.
async fn until(w: &Worker, ok: impl Fn(&Seen) -> bool) -> Seen {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let s = w.seen();
        if ok(&s) || Instant::now() > deadline {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A frame is a length and one JSON message, read back as written; the
/// channel's clean end is `None`; a frame past the bound is refused, on
/// either side, on its length alone: never written, and never read even
/// when it is all there and well-formed.
#[tokio::test]
async fn a_frame_round_trips_and_one_past_its_bound_is_refused() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    let hello = Msg::Hello {
        session: ObjectId::new().to_hex(),
        fence: 7,
    };
    send(&mut a, &hello).await.unwrap();
    send(&mut a, &Msg::Beat { n: 3 }).await.unwrap();
    assert_eq!(recv(&mut b).await.unwrap(), Some(hello));
    assert_eq!(recv(&mut b).await.unwrap(), Some(Msg::Beat { n: 3 }));

    let big = Msg::Hello {
        session: "x".repeat(MAX_FRAME),
        fence: 1,
    };
    let body = serde_json::to_vec(&big).unwrap();
    assert!(body.len() > MAX_FRAME);
    let writer = tokio::spawn(async move {
        let len = u32::try_from(body.len()).unwrap().to_be_bytes();
        let _ = a.write_all(&len).await;
        let _ = a.write_all(&body).await;
        a
    });
    let read = tokio::time::timeout(Duration::from_secs(10), recv(&mut b)).await;
    assert!(
        matches!(read, Ok(Err(_))),
        "a frame past its bound: {:?}",
        read.map(|r| r.map(|m| m.is_some()))
    );
    writer.abort();

    // Bounded: a send that wrongly went ahead would block on a socket no
    // one reads yet, and hang the test rather than fail it.
    let (mut a, mut b) = UnixStream::pair().unwrap();
    let sent = tokio::time::timeout(Duration::from_secs(10), send(&mut a, &big)).await;
    assert!(matches!(sent, Ok(Err(_))), "never written: {sent:?}");
    drop(a);
    assert_eq!(recv(&mut b).await.unwrap(), None, "the clean end");
}

/// A worker is `roomlerd hive-worker` exactly, started by the daemon: typed
/// by hand it has no channel on fd 3, and takes none.
#[test]
fn a_worker_is_the_subcommand_alone_in_a_process_the_daemon_started() {
    let argv = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
    let by_daemon = || Some(std::ffi::OsString::from("1"));
    let worker = argv(&["roomlerd", "hive-worker"]);
    assert!(is_worker(worker.clone().into_iter(), by_daemon()));
    assert!(!is_worker(worker.into_iter(), None), "typed by hand");
    for other in [
        argv(&["roomlerd", "hive-worker", "x"]),
        argv(&["roomlerd", "hive-mcp"]),
        argv(&["roomlerd"]),
    ] {
        assert!(
            !is_worker(other.clone().into_iter(), by_daemon()),
            "{other:?}"
        );
    }
}

/// The worker's own loop answers its run's hello with its pid, each beat
/// with that beat, and ends at `bye`, and at the channel's end too.
#[tokio::test]
async fn a_worker_answers_its_hello_and_each_beat_and_ends_at_bye() {
    let (mut daemon, worker) = UnixStream::pair().unwrap();
    let served = tokio::spawn(serve(worker, None));
    send(
        &mut daemon,
        &Msg::Hello {
            session: ObjectId::new().to_hex(),
            fence: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        recv(&mut daemon).await.unwrap(),
        Some(Msg::Ready {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
        })
    );
    for n in 1..=3 {
        send(&mut daemon, &Msg::Beat { n }).await.unwrap();
        assert_eq!(recv(&mut daemon).await.unwrap(), Some(Msg::Beat { n }));
    }
    send(&mut daemon, &Msg::Bye).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), served).await;
    ended.expect("it ended at bye").unwrap().unwrap();
    assert_eq!(recv(&mut daemon).await.unwrap(), None, "and closed its end");

    let (daemon, worker) = UnixStream::pair().unwrap();
    let served = tokio::spawn(serve(worker, None));
    drop(daemon);
    let ended = tokio::time::timeout(Duration::from_secs(5), served).await;
    ended
        .expect("it ended at its channel's end")
        .unwrap()
        .unwrap();
}

/// A wait for the next frame given up halfway through one — at a heartbeat's
/// deadline, at the stop — loses none of it: the next wait has that frame
/// whole, and the ones after it.
#[tokio::test]
async fn a_wait_given_up_mid_frame_loses_none_of_it() {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let (rd, _wr) = ours.into_split();
    let mut frames = Frames::read(rd);
    let body = serde_json::to_vec(&Msg::Beat { n: 9 }).unwrap();
    let len = u32::try_from(body.len()).unwrap().to_be_bytes();
    theirs.write_all(&len).await.unwrap();
    theirs.write_all(&body[..3]).await.unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(100), frames.next()).await;
    assert!(waited.is_err(), "half a frame is no frame: {waited:?}");
    theirs.write_all(&body[3..]).await.unwrap();
    send(&mut theirs, &Msg::Beat { n: 10 }).await.unwrap();
    assert_eq!(frames.next().await.unwrap(), Some(Msg::Beat { n: 9 }));
    assert_eq!(frames.next().await.unwrap(), Some(Msg::Beat { n: 10 }));
    drop(theirs);
    assert_eq!(frames.next().await.unwrap(), None, "the channel's end");
    assert_eq!(frames.next().await.unwrap(), None, "and after it");
}

/// The wait before a start doubles with each start still inside the window,
/// from the first backoff to the ceiling, and no further however many.
#[test]
fn the_wait_before_a_start_doubles_with_each_recent_one_to_its_ceiling() {
    let t = quick();
    let ms = |recent| wait_before(&t, recent).as_millis();
    assert_eq!(
        [ms(0), ms(1), ms(2), ms(3), ms(4), ms(usize::MAX)],
        [10, 10, 20, 40, 40, 40]
    );
}

/// A worker lost once in a long while is never given up on: its earlier
/// starts age out of the window, and with them the count and the wait.
#[tokio::test]
async fn a_worker_lost_once_in_a_long_while_is_never_given_up_on() {
    let timing = Timing {
        loop_window: Duration::from_millis(100),
        loop_max: 2,
        ..quick()
    };
    let fake = Fake::new(&[Part::LiveFor(150)]);
    let w = Worker::start(ObjectId::new(), 1, fake.clone(), timing, nobody());
    let s = until(&w, |s| s.starts >= 4 || s.gave_up).await;
    assert_eq!((s.starts >= 4, s.gave_up), (true, false), "{s:?}");
    w.stop().await;
}

/// A worker of another build is given up on at once: the next one would be
/// of that build too.
#[tokio::test]
async fn a_worker_of_another_build_is_given_up_on_at_once() {
    let fake = Fake::new(&[Part::Foreign, Part::Serve]);
    let (give_up, ended) = ended_runs();
    let w = Worker::start(ObjectId::new(), 1, fake.clone(), quick(), give_up);
    let s = until(&w, |s| s.gave_up).await;
    assert_eq!((s.starts, s.up, s.gave_up), (1, false, true), "{s:?}");
    let ended = ended.lock().unwrap().clone();
    assert!(
        ended.len() == 1 && ended[0].contains("version 0.0.0-another"),
        "the run ended, with why: {ended:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.started.load(Ordering::SeqCst), 1, "nothing after");
    w.stop().await;
}

/// A worker deaf to `bye` is killed once the grace is over: the stop ends
/// all the same.
#[tokio::test]
async fn a_worker_deaf_to_bye_is_killed_after_the_grace() {
    let timing = Timing {
        beat: Duration::from_secs(60),
        ..quick()
    };
    let fake = Fake::new(&[Part::NoBeats]);
    let w = Worker::start(ObjectId::new(), 1, fake.clone(), timing, nobody());
    let s = until(&w, |s| s.up).await;
    assert!(s.up, "{s:?}");
    let stopped = tokio::time::timeout(Duration::from_secs(5), w.stop()).await;
    assert!(stopped.is_ok(), "the stop ended");
    assert_eq!(fake.log(), ["#0 ready, deaf"]);
}

/// (Linux) A worker runs from the image the daemon runs, which a package
/// upgrade under a running daemon leaves `/proc/self/exe` naming while the
/// path names the next version.
#[cfg(target_os = "linux")]
#[test]
fn a_worker_runs_from_the_daemons_own_image() {
    let p = Program::this_binary().unwrap();
    assert_eq!(p.exe, std::path::Path::new("/proc/self/exe"));
    assert_eq!(p.arg0, std::env::args_os().next(), "what ps shows");
    assert_eq!(p.args, [WORKER_SUBCOMMAND]);
}

/// A worker that is lost after it was ready is started again after its
/// backoff, and the new one is up; one that is never ready is killed at the
/// start bound and started again; one deaf to its heartbeats is lost after
/// the misses allowed.
#[tokio::test]
async fn a_lost_worker_is_started_again() {
    for (first, why) in [
        (Part::DieAfterReady, "died after ready"),
        (Part::NeverReady, "never ready"),
        (Part::NoBeats, "ready, deaf"),
    ] {
        let fake = Fake::new(&[first, Part::Serve]);
        let w = Worker::start(ObjectId::new(), 1, fake.clone(), quick(), nobody());
        let s = until(&w, |s| s.starts == 2 && s.up).await;
        assert_eq!(
            (s.starts, s.up, s.gave_up),
            (2, true, false),
            "{why}: {s:?}"
        );
        assert_eq!(fake.log()[0], format!("#0 {why}"), "{:?}", fake.log());
        w.stop().await;
    }
}

/// Starts past the bound within the window, and the daemon gives up on the
/// run's worker: no start after it, and (P8-2a-2) the run is ended, once,
/// with the reason.
#[tokio::test]
async fn too_many_starts_and_the_daemon_gives_up() {
    let fake = Fake::new(&[Part::DieAtOnce]);
    let (give_up, ended) = ended_runs();
    let w = Worker::start(ObjectId::new(), 1, fake.clone(), quick(), give_up);
    let s = until(&w, |s| s.gave_up).await;
    assert!(s.gave_up, "{s:?}");
    assert_eq!(s.starts, quick().loop_max, "{s:?}");
    let ended = ended.lock().unwrap().clone();
    assert_eq!(ended.len(), 1, "the run ended once: {ended:?}");
    assert!(ended[0].contains("starts within"), "{ended:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fake.started.load(Ordering::SeqCst),
        quick().loop_max as usize,
        "nothing after"
    );
    w.stop().await;
}

/// A stopped worker is told `bye` and ends on it; it is not started again.
#[tokio::test]
async fn a_stopped_worker_is_told_bye() {
    let fake = Fake::new(&[Part::Serve]);
    let w = Worker::start(ObjectId::new(), 3, fake.clone(), quick(), nobody());
    let s = until(&w, |s| s.up).await;
    assert!(s.up, "{s:?}");
    // A few heartbeats answered keep it up.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(w.seen().starts, 1, "still the first");
    w.stop().await;
    assert_eq!(fake.log(), ["#0 served: Ok(())"], "it ended at bye");
    assert_eq!(fake.started.load(Ordering::SeqCst), 1);
}

/// The supervisor's registry: no worker unless the owner turned workers on;
/// a run that went live gets one, which is up; its end stops it, told `bye`,
/// and the registry forgets it.
#[tokio::test]
async fn a_run_gets_a_worker_where_the_owner_said_so_and_loses_it_at_its_end() {
    let (session, fence) = (ObjectId::new(), 4);
    run_started(session, fence, false);
    assert_eq!(seen(session, fence), None, "off: no worker");
    let fake = Fake::new(&[Part::Serve]);
    run_started_with(session, fence, fake.clone(), quick(), nobody());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen(session, fence).is_some_and(|s| s.up) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(seen(session, fence).is_some_and(|s| s.up), "up");
    assert!(drop_run(session, fence), "it had one");
    assert_eq!(seen(session, fence), None, "forgotten");
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.log().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fake.log(), ["#0 served: Ok(())"], "it ended at bye");
    assert!(!drop_run(session, fence), "nothing left");
}

/// (Linux and macOS) A program started as a worker gets its channel on
/// fd 3 and (P8-2a-2) the second one on fd 4, and nothing else of the
/// daemon's: it writes on each, the daemon reads each, and the daemon's ends
/// close as the program exits.
#[tokio::test]
async fn a_program_gets_its_channel_on_fd_3() {
    let program = Program {
        exe: "/bin/sh".into(),
        arg0: None,
        args: vec![
            "-c".into(),
            "printf 'on-fd-3' >&3; printf 'on-fd-4' >&4".into(),
        ],
    };
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let (ours_d, theirs_d) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut running = program.spawn(theirs, theirs_d).unwrap();
    assert!(running.pid().is_some());
    for (ch, want) in [(ours, "on-fd-3"), (ours_d, "on-fd-4")] {
        ch.set_nonblocking(true).unwrap();
        let mut ch = UnixStream::from_std(ch).unwrap();
        let mut got = String::new();
        tokio::time::timeout(Duration::from_secs(10), ch.read_to_string(&mut got))
            .await
            .expect("each channel ends with the program")
            .unwrap();
        assert_eq!(got, want);
    }
    running.kill().await;
}
