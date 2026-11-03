// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::sync::atomic::{AtomicUsize, Ordering};

use bson::oid::ObjectId;
use roomler_ai_remote_control::knowhow::{ProbeOutcome, probe_limits};
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_node_core::probe_reach;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::super::link::{CALL_WAIT, DEVICE_CALL_WAIT};
use super::super::{GiveUp, WORKER_SUBCOMMAND, Worker, send};
use super::*;

/// How long a test waits for anything that should come in a blink.
const WAIT: Duration = Duration::from_secs(10);
/// A cool-off a test waits out.
const COOL: Duration = Duration::from_millis(300);
/// One no test waits out.
const NEVER: Duration = Duration::from_secs(3600);

/// The bounds a test runs at: a worker's whole life in a blink.
fn quick() -> Timing {
    Timing::quick()
}

/// A worker's end of its channel, as it takes it.
#[cfg(unix)]
type End = tokio::net::UnixStream;
#[cfg(windows)]
type End = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
fn end_of(theirs: Theirs) -> std::io::Result<End> {
    theirs.ch.set_nonblocking(true)?;
    End::from_std(theirs.ch)
}

#[cfg(windows)]
fn end_of(theirs: Theirs) -> std::io::Result<End> {
    use std::os::windows::io::IntoRawHandle;
    // SAFETY: the client end its channel was made with, overlapped, owned
    // nowhere else.
    unsafe { End::from_raw_handle(theirs.pipe.into_raw_handle()) }
}

/// The worker's own loop on its end, as `roomlerd hive-worker` runs it.
async fn own_loop(end: End) -> std::io::Result<()> {
    #[cfg(unix)]
    let served = super::super::serve(end, None, None);
    #[cfg(windows)]
    let served = super::super::serve(end, None);
    served.await
}

/// A worker's `ready`, at this build's version.
fn ready() -> Msg {
    Msg::Ready {
        pid: 1,
        version: VERSION.to_string(),
    }
}

/// The frames a released worker reads and writes, the calls behind its
/// version check left out: what any worker that predates the device's knows.
/// A frame it cannot parse ends it, as its `recv` does.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Released {
    Hello {
        session: String,
        fence: u64,
    },
    Ready {
        pid: u32,
        version: String,
    },
    Beat {
        n: u64,
    },
    Bye,
    Log {
        level: String,
        target: String,
        line: String,
    },
}

const OLDER: &str = "0.4.195";
const NEWER: &str = "999.0.0";

async fn released_frame(end: &mut End) -> std::io::Result<Released> {
    let mut len = [0u8; 4];
    end.read_exact(&mut len).await?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    end.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

async fn say_released(end: &mut End, m: &Released) -> std::io::Result<()> {
    let body = serde_json::to_vec(m).unwrap();
    end.write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
        .await?;
    end.write_all(&body).await?;
    end.flush().await
}

/// What one stand-in worker does on its channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    /// The worker's own loop, as `roomlerd hive-worker` runs it.
    Serve,
    /// The worker's own loop for this many milliseconds, then its channel
    /// gone, as a killed process's goes.
    KilledAfter(u64),
    /// Ready, and then nothing: no beat answered, nothing read. A stopped
    /// process.
    Hung,
    /// Gone before a word: a fault at its start.
    DieAtOnce,
    /// Ready, each beat answered, and gone as it is asked a measure.
    DieOnCall,
    /// Ready, each beat answered, and stopped as it is asked a measure.
    HangOnCall,
    /// Ready, each beat answered, and no call ever answered.
    DeafToCalls,
    /// A worker of a release that predates the device's ([`Released`]).
    Older,
    /// A worker of a later build: it knows the hello, and says its version.
    Newer,
}

/// Workers as tasks of this process on their real channels, each playing the
/// next part of `plan` (the last part again once the plan runs out), each
/// life noted.
pub(crate) struct Fake {
    plan: Vec<Part>,
    started: AtomicUsize,
    log: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    pub(crate) fn new(plan: &[Part]) -> Arc<Self> {
        Arc::new(Self {
            plan: plan.to_vec(),
            started: AtomicUsize::new(0),
            log: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    fn log(&self) -> Vec<String> {
        lock(&self.log).clone()
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
    fn spawn(&self, theirs: Theirs) -> std::io::Result<Box<dyn Running>> {
        let k = self.started.fetch_add(1, Ordering::SeqCst);
        let part = *self.plan.get(k).or(self.plan.last()).unwrap();
        let mut end = end_of(theirs)?;
        let log = Arc::clone(&self.log);
        let note = move |what: String| lock(&log).push(format!("#{k} {what}"));
        let task = tokio::spawn(async move {
            match part {
                Part::Serve => {
                    let ended = own_loop(end).await;
                    note(format!("served: {ended:?}"));
                }
                Part::KilledAfter(ms) => {
                    let _ = tokio::time::timeout(Duration::from_millis(ms), own_loop(end)).await;
                    note("killed".into());
                }
                Part::Hung => {
                    let _ = recv(&mut end).await;
                    let _ = send(&mut end, &ready()).await;
                    note("hung".into());
                    std::future::pending::<()>().await;
                    drop(end);
                }
                Part::DieAtOnce => {
                    note("died at once".into());
                    drop(end);
                }
                Part::DieOnCall | Part::HangOnCall | Part::DeafToCalls => {
                    let _ = recv(&mut end).await;
                    let _ = send(&mut end, &ready()).await;
                    while let Ok(Some(m)) = recv(&mut end).await {
                        match (m, part) {
                            (Msg::Beat { n }, _) => {
                                let _ = send(&mut end, &Msg::Beat { n }).await;
                            }
                            (Msg::DeviceCall { .. }, Part::DieOnCall) => {
                                note("died on a call".into());
                                return;
                            }
                            (Msg::DeviceCall { .. }, Part::HangOnCall) => {
                                note("hung on a call".into());
                                std::future::pending::<()>().await;
                            }
                            _ => {}
                        }
                    }
                }
                Part::Older => loop {
                    match released_frame(&mut end).await {
                        Ok(Released::Hello { session, .. }) => {
                            note(format!("older: a hello for {session}"));
                            let said = Released::Ready {
                                pid: 1,
                                version: OLDER.into(),
                            };
                            let _ = say_released(&mut end, &said).await;
                        }
                        Ok(Released::Beat { n }) => {
                            let _ = say_released(&mut end, &Released::Beat { n }).await;
                        }
                        Ok(Released::Bye) => return,
                        Ok(_) => {}
                        Err(e) => {
                            note(format!("older: it could not read a frame: {e}"));
                            return;
                        }
                    }
                },
                Part::Newer => {
                    let _ = recv(&mut end).await;
                    let said = Msg::Ready {
                        pid: 1,
                        version: NEWER.into(),
                    };
                    let _ = send(&mut end, &said).await;
                    note("newer".into());
                    std::future::pending::<()>().await;
                    drop(end);
                }
            }
        });
        Ok(Box::new(Task(task)))
    }
}

/// Until `ok` holds of what `w` has seen, within [`WAIT`].
async fn until(w: &DeviceWorker, ok: impl Fn(&Seen) -> bool) -> Seen {
    let deadline = Instant::now() + WAIT;
    loop {
        let s = w.seen();
        if ok(&s) || Instant::now() > deadline {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The device's worker on real channels, its own loop in this process: what
/// a test of what crosses to it measures in.
pub(crate) fn in_process() -> Arc<DeviceWorker> {
    DeviceWorker::start(Fake::new(&[Part::Serve]), quick(), NEVER)
}

fn asked(kind: &str, host: &str, port: u16) -> Asked {
    Asked {
        id: ObjectId::new(),
        kind: kind.into(),
        host: host.into(),
        port,
        sni: None,
    }
}

fn may_reach(port: u16) -> Vec<DestinationRule> {
    probe_reach::rules(&[format!("127.0.0.1:{port}")])
}

/// A listener on 127.0.0.1 that counts what it accepts, and closes each.
async fn counting() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            n.fetch_add(1, Ordering::SeqCst);
            drop(tcp);
        }
    });
    (port, seen)
}

/// A listener on 127.0.0.1 that accepts and never says a word: a `tls` probe
/// of it takes its whole deadline.
async fn silent() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = listener.accept().await {
            held.push(tcp);
        }
    });
    port
}

/// A measure of a port that answers, by a worker up: the probe's own `ok`.
async fn measured_ok(w: &DeviceWorker) {
    let (port, seen) = counting().await;
    let a = asked("tcp", "127.0.0.1", port);
    let probed = w.measure(a, may_reach(port)).await;
    assert_eq!(probed.outcome, ProbeOutcome::Ok, "{probed:?}");
    assert!(probed.rtt_ms.is_some() && probed.address.is_some());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(seen.load(Ordering::SeqCst), 1, "it connected once");
}

/// Decision 71's bounds and decision 72's, as they ship: a test runs at bounds
/// of its own, so this is what says the device's are the ones the operator
/// confirmed. A call outlasts the measure it waits for.
#[test]
fn the_device_s_bounds_are_the_ones_the_operator_confirmed() {
    let t = Timing::default();
    assert_eq!(
        (t.start, t.beat, t.misses),
        (Duration::from_secs(10), Duration::from_secs(5), 3),
        "a 10 s hello, a 5 s heartbeat, three missed"
    );
    assert_eq!(
        (t.backoff, t.backoff_max),
        (Duration::from_secs(1), Duration::from_secs(30))
    );
    assert_eq!(
        (t.loop_max, t.loop_window),
        (5, Duration::from_secs(600)),
        "five starts within ten minutes"
    );
    assert_eq!(COOL_OFF, Duration::from_secs(600), "a 10-minute cool-off");
    assert_eq!(MAX_MEMORY, 1 << 30, "1 GiB until P8-2d-3 measures it");
    assert!(
        DEVICE_CALL_WAIT > Duration::from_secs(probe_limits::TIMEOUT_SECS),
        "a call outlasts the measure it waits for"
    );
    assert_eq!(
        CALL_WAIT,
        Duration::from_secs(10),
        "a session's stays as it was"
    );
}

/// The device's hello is the frame every worker ever released reads, to the
/// byte. A session's is unchanged, and no session's id reads as the device's.
#[test]
fn the_device_s_hello_is_the_frame_every_worker_reads() {
    let run = DeviceRun {
        shared: Arc::default(),
        cool_off: COOL,
    };
    let body = serde_json::to_string(&run.hello()).unwrap();
    assert_eq!(body, r#"{"t":"hello","session":"device","fence":0}"#);
    assert_eq!(
        serde_json::from_str::<Released>(&body).unwrap(),
        Released::Hello {
            session: "device".into(),
            fence: 0
        }
    );
    let session = ObjectId::new();
    assert_ne!(session.to_hex(), HELLO, "a session's id never reads so");
    let (nobody, published): (GiveUp, _) = (Arc::new(|_| {}), Arc::default());
    let run = super::super::Run {
        session,
        fence: 7,
        published,
        end_run: nobody,
    };
    assert_eq!(
        serde_json::to_string(&run.hello()).unwrap(),
        format!(r#"{{"t":"hello","session":"{session}","fence":7}}"#)
    );
}

/// AC1: one worker from its start, and none where the owner's word is off;
/// a second start makes no second. It stays through sessions that come and
/// go, each with a worker of its own: still its first start, and it measures.
#[tokio::test]
async fn one_worker_from_its_start_whatever_sessions_come_and_go() {
    let slot = Slot::new();
    let fake = Fake::new(&[Part::Serve]);
    let made = AtomicUsize::new(0);
    let make = || {
        made.fetch_add(1, Ordering::SeqCst);
        DeviceWorker::start(fake.clone(), quick(), NEVER)
    };
    slot.start(false, make);
    assert_eq!(made.load(Ordering::SeqCst), 0, "off: none");
    assert!(slot.0.get().is_none());
    slot.start(true, make);
    slot.start(true, make);
    assert_eq!(made.load(Ordering::SeqCst), 1, "one, however often asked");
    let w = Arc::clone(slot.0.get().unwrap());
    let s = until(&w, |s| s.up).await;
    assert_eq!((s.up, s.starts), (true, 1), "{s:?}");
    assert!(
        !slot.measure().in_daemon(),
        "what the prober measures in is the worker"
    );

    for fence in 1..=3 {
        let session = Worker::start(
            ObjectId::new(),
            fence,
            Fake::new(&[Part::Serve]),
            quick(),
            Arc::new(|_| {}),
        );
        let deadline = Instant::now() + WAIT;
        while !session.seen().up && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(session.seen().up, "a session's own worker");
        session.stop().await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let s = w.seen();
    assert_eq!((s.up, s.starts), (true, 1), "the same one: {s:?}");
    assert_eq!(fake.started(), 1);
    measured_ok(&w).await;
    w.stop().await;
}

/// AC5: with the owner's word off, nothing starts and a probe measures in this
/// process; and nothing a build without a worker's calls can reach differs.
#[tokio::test]
async fn with_the_owners_word_off_a_probe_measures_in_the_daemon() {
    let slot = Slot::new();
    slot.start(false, || panic!("no worker where the owner's word is off"));
    let measure = slot.measure();
    assert!(measure.in_daemon());
    let (port, seen) = counting().await;
    let a = asked("tcp", "127.0.0.1", port);
    let probed = measure.measure(a, may_reach(port)).await;
    assert_eq!(probed.outcome, ProbeOutcome::Ok, "{probed:?}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

/// AC3, killed: its channel goes, it is started again after its backoff, and
/// the next probe is measured by the new one. A probe that comes while it is
/// starting waits for it, and is measured.
#[tokio::test]
async fn a_killed_worker_is_started_again_and_measures() {
    let fake = Fake::new(&[Part::KilledAfter(150), Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    let s = until(&w, |s| s.up).await;
    assert_eq!((s.up, s.starts), (true, 1), "{s:?}");
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!(
        (s.starts, s.up, s.gave_up),
        (2, true, false),
        "{:?}",
        fake.log()
    );
    assert_eq!(fake.log()[0], "#0 killed");
    measured_ok(&w).await;
    w.stop().await;
}

/// AC3, hung: a stopped heartbeat is found within its bound, the hung one is
/// killed, and another is up and measures.
#[tokio::test]
async fn a_hung_worker_is_found_by_its_beats_and_started_again() {
    let fake = Fake::new(&[Part::Hung, Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!(
        (s.starts, s.up, s.gave_up),
        (2, true, false),
        "{:?}",
        fake.log()
    );
    assert_eq!(fake.log()[0], "#0 hung");
    measured_ok(&w).await;
    w.stop().await;
}

/// AC3, a crash loop: it parks at its starts within the window. While parked
/// no worker runs, and a probe is answered `failed` at once with why, never
/// measured in the daemon. The cool-off is over: ONE start; that one fails too
/// and it parks again, for a fault that repeats costs one start a cool-off
/// (decision 71). A worker that comes up then measures, and it is not parked.
#[tokio::test]
async fn a_crash_loop_parks_it_and_a_probe_says_why() {
    let t = quick();
    let parts: Vec<Part> = std::iter::repeat_n(Part::DieAtOnce, t.loop_max as usize + 1)
        .chain([Part::Serve])
        .collect();
    let fake = Fake::new(&parts);
    let w = DeviceWorker::start(fake.clone(), t.clone(), COOL);
    let s = until(&w, |s| s.gave_up).await;
    assert!(s.gave_up, "{s:?}");
    assert_eq!(s.starts, t.loop_max, "parked at its starts: {s:?}");

    let (port, seen) = counting().await;
    let started = Instant::now();
    let probed = w
        .measure(asked("tcp", "127.0.0.1", port), may_reach(port))
        .await;
    assert!(
        started.elapsed() < COOL / 2,
        "at once: {:?}",
        started.elapsed()
    );
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    assert_eq!(
        (probed.rtt_ms, probed.address, probed.tls),
        (None, None, None)
    );
    let detail = probed.detail.unwrap();
    assert!(detail.contains("parked"), "{detail}");
    assert!(detail.contains("starts within"), "{detail}");
    // How the last one went: gone before its hello was written, or after.
    assert!(
        detail.contains("ended before it was ready") || detail.contains("its channel failed"),
        "{detail}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(seen.load(Ordering::SeqCst), 0, "nothing was measured");

    // One start at the cool-off's end, and parked again at once.
    let s = until(&w, |s| s.starts == t.loop_max + 1 && s.gave_up).await;
    assert_eq!(
        (s.starts, s.gave_up),
        (t.loop_max + 1, true),
        "one start: {:?}",
        fake.log()
    );
    let s = w.seen();
    assert_eq!(s.starts, t.loop_max + 1, "and no second: {s:?}");

    // The next comes up.
    let s = until(&w, |s| s.up).await;
    assert_eq!(
        (s.up, s.gave_up, s.starts),
        (true, false, t.loop_max + 2),
        "{s:?}"
    );
    measured_ok(&w).await;
    w.stop().await;
}

/// Workers that cannot start until `prepare` has been asked `asks` times: a
/// failure that passes (#3349).
struct NotYet {
    asks: usize,
    asked: AtomicUsize,
    then: Arc<Fake>,
}

impl Spawn for NotYet {
    fn spawn(&self, theirs: Theirs) -> std::io::Result<Box<dyn Running>> {
        if self.asked.load(Ordering::SeqCst) < self.asks {
            return Err(std::io::Error::other("not ready yet"));
        }
        self.then.spawn(theirs)
    }

    fn prepare(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// #3349 — a failure that outlasts its starts parks the worker, as any that
/// cannot start does, and that is not for life: the start at the cool-off's
/// end asks again, comes up and measures.
#[tokio::test]
async fn a_failure_that_passes_costs_a_cool_off_and_no_more() {
    let t = quick();
    let spawner = Arc::new(NotYet {
        asks: t.loop_max as usize + 1,
        asked: AtomicUsize::new(0),
        then: Fake::new(&[Part::Serve]),
    });
    let w = DeviceWorker::start(spawner, t.clone(), COOL);
    let s = until(&w, |s| s.gave_up).await;
    assert_eq!((s.gave_up, s.starts), (true, t.loop_max), "parked: {s:?}");
    let s = until(&w, |s| s.up).await;
    assert_eq!(
        (s.up, s.gave_up, s.starts),
        (true, false, t.loop_max + 1),
        "{s:?}"
    );
    measured_ok(&w).await;
    w.stop().await;
}

/// A worker that outlives the window after a park is forgotten: its later
/// loss is an ordinary one, restarted with no park.
#[tokio::test]
async fn a_worker_that_outlives_the_window_is_lost_as_any_other() {
    let t = Timing {
        loop_window: Duration::from_secs(1),
        ..quick()
    };
    let mut parts = vec![Part::DieAtOnce; t.loop_max as usize];
    parts.extend([Part::KilledAfter(1500), Part::Serve]);
    let fake = Fake::new(&parts);
    let w = DeviceWorker::start(fake.clone(), t.clone(), Duration::from_millis(100));
    let s = until(&w, |s| s.gave_up).await;
    assert!(s.gave_up, "the crash loop parked it: {s:?}");
    let s = until(&w, |s| s.up).await;
    assert!(s.up, "up after the cool-off: {s:?}");
    // Through its loss, up to the next: never parked again.
    let deadline = Instant::now() + WAIT;
    loop {
        let s = w.seen();
        assert!(!s.gave_up, "parked again: {:?}", fake.log());
        if s.up && s.starts == t.loop_max + 2 {
            break;
        }
        assert!(Instant::now() < deadline, "{s:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    w.stop().await;
}

/// What a caller sees when its worker dies mid-measure: `failed`, at once as
/// the channel goes, saying so; and the probe is not run again. The next is
/// measured by the worker started after it.
#[tokio::test]
async fn a_worker_that_dies_mid_measure_fails_that_probe_alone() {
    let fake = Fake::new(&[Part::DieOnCall, Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    until(&w, |s| s.up).await;
    let (port, seen) = counting().await;
    let started = Instant::now();
    let probed = w
        .measure(asked("tcp", "127.0.0.1", port), may_reach(port))
        .await;
    assert!(started.elapsed() < WAIT / 2, "{:?}", started.elapsed());
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    assert!(
        probed.detail.as_deref().unwrap().contains("did not answer"),
        "{probed:?}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(seen.load(Ordering::SeqCst), 0, "never run again");
    assert_eq!(fake.log()[0], "#0 died on a call");
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!((s.starts, s.up), (2, true), "{s:?}");
    measured_ok(&w).await;
    w.stop().await;
}

/// A worker that stops mid-measure is found by its beats, well before the
/// call's own bound: `failed`, saying so.
#[tokio::test]
async fn a_worker_hung_mid_measure_fails_that_probe_at_its_beats() {
    let fake = Fake::new(&[Part::HangOnCall, Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    until(&w, |s| s.up).await;
    let (port, _) = counting().await;
    let started = Instant::now();
    let probed = w
        .measure(asked("tcp", "127.0.0.1", port), may_reach(port))
        .await;
    assert!(
        started.elapsed() < DEVICE_CALL_WAIT / 2,
        "{:?}",
        started.elapsed()
    );
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    assert!(
        probed.detail.as_deref().unwrap().contains("did not answer"),
        "{probed:?}"
    );
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!((s.starts, s.up), (2, true), "{s:?}");
    w.stop().await;
}

/// A worker that reads nothing, and a call too big for what its channel holds:
/// the write never ends and holds the writer's lock, so the next beat cannot be
/// written either. It is found all the same, within the beats' own bound; the
/// kill ends the write, and the probe is answered `failed`.
#[tokio::test]
async fn a_worker_hung_with_its_channel_full_is_still_found() {
    let fake = Fake::new(&[Part::Hung, Part::Serve]);
    let t = Timing {
        beat: Duration::from_millis(200),
        ..quick()
    };
    let w = DeviceWorker::start(fake.clone(), t, NEVER);
    until(&w, |s| s.up).await;
    let host = "x".repeat(MAX_FRAME - 1024);
    let started = Instant::now();
    let probed = tokio::time::timeout(WAIT, w.measure(asked("tcp", &host, 80), Vec::new()))
        .await
        .expect("the write ends with the worker");
    assert!(
        started.elapsed() < DEVICE_CALL_WAIT,
        "{:?}",
        started.elapsed()
    );
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    assert!(
        probed.detail.as_deref().unwrap().contains("did not answer"),
        "{probed:?}"
    );
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!((s.starts, s.up), (2, true), "{s:?}");
    w.stop().await;
}

/// A worker whose beats are fine and that answers no call: the call's bound,
/// past which the worker is taken for lost, and another started.
#[tokio::test]
async fn a_call_past_its_bound_takes_the_worker_for_lost() {
    let fake = Fake::new(&[Part::DeafToCalls, Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    until(&w, |s| s.up).await;
    let (port, _) = counting().await;
    let started = Instant::now();
    let probed = w
        .measure(asked("tcp", "127.0.0.1", port), may_reach(port))
        .await;
    let took = started.elapsed();
    assert!(
        took >= DEVICE_CALL_WAIT && took < DEVICE_CALL_WAIT + WAIT,
        "{took:?}"
    );
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    assert!(
        probed.detail.as_deref().unwrap().contains("unanswered"),
        "{probed:?}"
    );
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!((s.starts, s.up), (2, true), "{s:?}");
    w.stop().await;
}

/// The bound on a measure, and the beats while measures run: probes that take
/// their whole deadline, as many as it runs at once and one more. The one past
/// the bound is refused at once, in a word. Each of the others is answered with
/// what the probe saw, past its deadline and inside the call's bound. All the
/// while each beat is answered at a test's quick bounds, so one worker stays
/// up throughout: no slow probe reads as a hung worker.
#[tokio::test]
async fn slow_probes_never_keep_its_beats_from_being_answered() {
    let fake = Fake::new(&[Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    until(&w, |s| s.up).await;
    let port = silent().await;
    let started = Instant::now();
    let mut calls = Vec::new();
    for _ in 0..=MAX_MEASURES {
        let w = Arc::clone(&w);
        calls.push(tokio::spawn(async move {
            let probed = w
                .measure(asked("tls", "127.0.0.1", port), may_reach(port))
                .await;
            (probed, started.elapsed())
        }));
    }
    let mut refused = 0;
    let mut measured = 0;
    for call in calls {
        let (probed, took) = call.await.unwrap();
        assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
        let detail = probed.detail.clone().unwrap();
        if detail.contains("measures at once") {
            refused += 1;
            assert!(took < WAIT / 2, "refused at once: {took:?}");
        } else {
            measured += 1;
            assert!(detail.contains("TLS handshake"), "{detail}");
            assert!(probed.rtt_ms.is_some(), "it connected: {probed:?}");
            assert!(took >= Duration::from_secs(probe_limits::TIMEOUT_SECS));
            assert!(took < DEVICE_CALL_WAIT, "{took:?}");
        }
    }
    assert_eq!((refused, measured), (1, MAX_MEASURES));
    let s = w.seen();
    assert_eq!(
        (s.starts, s.up, s.gave_up),
        (1, true, false),
        "no probe cost it a beat: {:?}",
        fake.log()
    );
    // And the bound let go: another measures.
    measured_ok(&w).await;
    w.stop().await;
}

/// A probe too big for the channel costs that probe alone: a frame past its
/// bound would take the link for lost. The same worker measures after it.
#[tokio::test]
async fn a_probe_too_big_for_the_channel_costs_that_probe_alone() {
    let w = in_process();
    until(&w, |s| s.up).await;
    let host = "x".repeat(MAX_FRAME);
    let probed = w.measure(asked("tcp", &host, 80), Vec::new()).await;
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    assert!(
        probed.detail.as_deref().unwrap().contains("does not fit"),
        "{probed:?}"
    );
    measured_ok(&w).await;
    let s = w.seen();
    assert_eq!((s.starts, s.up), (1, true), "the same one: {s:?}");
    w.stop().await;
}

/// AC6, a worker older than the daemon: one that predates the device's reads
/// its hello as a session's and answers `ready` at its version, and the
/// daemon parks: one start a cool-off, and not one read it could not make, so
/// no parse error and no crash loop. Once one of this build is there it is up.
#[tokio::test]
async fn a_worker_older_than_the_daemon_parks_it_with_no_parse_error() {
    let fake = Fake::new(&[Part::Older, Part::Older, Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), COOL);
    let s = until(&w, |s| s.gave_up).await;
    assert_eq!((s.gave_up, s.starts), (true, 1), "at its first: {s:?}");
    let probed = w
        .measure(asked("tcp", "127.0.0.1", 80), may_reach(80))
        .await;
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    let detail = probed.detail.unwrap();
    assert!(detail.contains("parked"), "{detail}");
    assert!(
        detail.contains(&format!("it is version {OLDER}, the daemon {VERSION}")),
        "{detail}"
    );

    let s = until(&w, |s| s.up).await;
    assert_eq!((s.up, s.starts, s.gave_up), (true, 3, false), "{s:?}");
    assert_eq!(
        fake.log()
            .iter()
            .filter(|l| l.contains("older"))
            .cloned()
            .collect::<Vec<_>>(),
        [
            "#0 older: a hello for device",
            "#1 older: a hello for device"
        ],
        "each read the hello, and nothing it could not"
    );
    measured_ok(&w).await;
    w.stop().await;
}

/// The stand-in above reads as a released worker does: a frame of a shape it
/// does not know is an error to it. A variant of the device's own would have
/// ended every such worker, the loop this hello exists to avoid.
#[test]
fn a_released_worker_cannot_parse_a_frame_it_does_not_know() {
    for frame in [
        r#"{"t":"device_hello"}"#,
        r#"{"t":"device_call","id":1,"call":{"d":"measure"}}"#,
    ] {
        assert!(serde_json::from_str::<Released>(frame).is_err(), "{frame}");
    }
    assert!(serde_json::from_str::<Released>(r#"{"t":"beat","n":1}"#).is_ok());
}

/// AC6, a worker newer than the daemon: it knows the hello and says its
/// version. The daemon parks, and tries one start a cool-off.
#[tokio::test]
async fn a_worker_newer_than_the_daemon_parks_it() {
    let fake = Fake::new(&[Part::Newer, Part::Newer, Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), COOL);
    let s = until(&w, |s| s.gave_up).await;
    assert_eq!((s.gave_up, s.starts), (true, 1), "at its first: {s:?}");
    let probed = w
        .measure(asked("tcp", "127.0.0.1", 80), may_reach(80))
        .await;
    assert!(
        probed
            .detail
            .as_deref()
            .unwrap()
            .contains(&format!("it is version {NEWER}, the daemon {VERSION}")),
        "{probed:?}"
    );
    let s = until(&w, |s| s.up).await;
    assert_eq!((s.up, s.starts), (true, 3), "{s:?}");
    w.stop().await;
}

/// AC6, the daemon older than the worker: one that predates the device's sends
/// a session's hello, byte for byte as it always did, and a worker of this
/// build serves it as a session's: `ready` in the shape that daemon reads, each
/// beat, and its end at `bye`. Its version is then that daemon's to refuse.
#[tokio::test]
async fn a_daemon_older_than_the_worker_is_served_as_it_always_was() {
    let (ours, theirs) = super::super::channels().unwrap();
    let end = end_of(theirs).unwrap();
    let served = tokio::spawn(own_loop(end));
    let (link, mut frames) = super::super::daemon_end::<super::super::link::SessionRole>(ours)
        .await
        .unwrap();
    let session = ObjectId::new();
    let hello: Msg = serde_json::from_str(&format!(
        r#"{{"t":"hello","session":"{session}","fence":3}}"#
    ))
    .unwrap();
    link.send(&hello).await.unwrap();
    let said = tokio::time::timeout(WAIT, frames.next())
        .await
        .expect("it answered its hello")
        .unwrap()
        .unwrap();
    let body = serde_json::to_string(&said).unwrap();
    assert_eq!(
        serde_json::from_str::<Released>(&body).unwrap(),
        Released::Ready {
            pid: std::process::id(),
            version: VERSION.into()
        }
    );
    link.send(&Msg::Beat { n: 1 }).await.unwrap();
    loop {
        let got = tokio::time::timeout(WAIT, frames.next())
            .await
            .expect("it answered its beat");
        match got.unwrap().unwrap() {
            Msg::Beat { n } => {
                assert_eq!(n, 1);
                break;
            }
            Msg::Log { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    link.send(&Msg::Bye).await.unwrap();
    let ended = tokio::time::timeout(WAIT, served).await;
    ended.expect("it ended at bye").unwrap().unwrap();
}

/// Its stop: told `bye`, it ends on it, and nothing starts after.
#[tokio::test]
async fn at_its_stop_it_is_told_bye_and_not_started_again() {
    let fake = Fake::new(&[Part::Serve]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    until(&w, |s| s.up).await;
    tokio::time::timeout(WAIT, w.stop()).await.expect("bounded");
    let log = fake.log();
    assert_eq!(log, ["#0 served: Ok(())"], "it ended at bye");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.started(), 1, "nothing after");
    let probed = w
        .measure(asked("tcp", "127.0.0.1", 80), may_reach(80))
        .await;
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    assert!(
        probed.detail.as_deref().unwrap().contains("stopping"),
        "{probed:?}"
    );
}

/// A worker hung at the stop is killed after its grace, within the bound the
/// daemon's end allows it.
#[tokio::test]
async fn a_hung_worker_is_killed_at_its_stop() {
    let t = Timing {
        beat: Duration::from_secs(60),
        ..quick()
    };
    let fake = Fake::new(&[Part::Hung]);
    let w = DeviceWorker::start(fake.clone(), t, NEVER);
    until(&w, |s| s.up).await;
    let started = Instant::now();
    w.stop().await;
    assert!(started.elapsed() < STOP_WAIT, "{:?}", started.elapsed());
    assert_eq!(fake.started(), 1);
    assert!(!w.seen().up);
}

/// A parked worker's stop ends the cool-off at once: nothing holds the
/// daemon's end for it, and no worker starts after it.
#[tokio::test]
async fn a_parked_worker_stops_at_once() {
    let fake = Fake::new(&[Part::DieAtOnce]);
    let w = DeviceWorker::start(fake.clone(), quick(), NEVER);
    let s = until(&w, |s| s.gave_up).await;
    assert!(s.gave_up, "{s:?}");
    let started = fake.started();
    let at = Instant::now();
    w.stop().await;
    assert!(at.elapsed() < STOP_WAIT / 2, "{:?}", at.elapsed());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.started(), started, "nothing after");
}

/// AC3, the daemon's own traffic: while the device's worker crash-loops and
/// parks, a session's worker beside it stays up on its first start; the prober
/// answers, and answers again as it answered; and this process's runtime never
/// stalls for it.
#[tokio::test]
async fn while_it_fails_the_daemons_own_work_goes_on() {
    let t = quick();
    let session = Worker::start(
        ObjectId::new(),
        1,
        Fake::new(&[Part::Serve]),
        t.clone(),
        Arc::new(|_| {}),
    );
    let device = DeviceWorker::start(Fake::new(&[Part::DieAtOnce]), t.clone(), COOL);
    let beat = Instant::now();
    let longest = Arc::new(Mutex::new(Duration::ZERO));
    let watching = Arc::clone(&longest);
    let ticking = tokio::spawn(async move {
        let mut last = beat;
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let now = Instant::now();
            let mut l = lock(&watching);
            *l = (*l).max(now - last);
            last = now;
        }
    });
    let s = until(&device, |s| s.gave_up).await;
    assert!(s.gave_up, "{s:?}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = crate::config::test_fixture();
    cfg.knowhow_probe_reach = vec!["127.0.0.1:80".into()];
    crate::config::save(&path, &cfg).unwrap();
    let prober = knowhow_probe::Prober::measuring(
        path,
        Default::default(),
        Arc::clone(&device) as Arc<dyn Measure>,
    );
    let probe = asked("tcp", "127.0.0.1", 80);
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    Arc::clone(&prober).request(probe.clone(), tx.clone());
    let first = tokio::time::timeout(WAIT, rx.recv())
        .await
        .unwrap()
        .unwrap();
    let ClientMsg::KnowhowProbed {
        outcome, detail, ..
    } = &first
    else {
        panic!("{first:?}");
    };
    assert_eq!(*outcome, ProbeOutcome::Failed);
    assert!(detail.as_deref().unwrap().contains("parked"), "{first:?}");
    Arc::clone(&prober).request(probe, tx);
    let again = tokio::time::timeout(WAIT, rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        format!("{again:?}"),
        format!("{first:?}"),
        "answered as it was"
    );

    // Through a cool-off and the start after it.
    let before = device.seen().starts;
    let s = until(&device, |s| s.starts > before).await;
    assert!(s.starts > before, "{s:?}");
    let s = session.seen();
    assert_eq!((s.starts, s.up), (1, true), "the session's own: {s:?}");
    ticking.abort();
    assert!(
        *lock(&longest) < Duration::from_secs(5),
        "the daemon's runtime stalled: {:?}",
        *lock(&longest)
    );
    device.stop().await;
    session.stop().await;
}

/// A real worker's process, ended as a fault ends it: nothing it can answer.
#[cfg(unix)]
fn kill_process(pid: u32) {
    // SAFETY: a signal to a process this test's worker started.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: a process this test's worker started; the handle is closed here.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        assert!(!h.is_null(), "its process: {pid}");
        TerminateProcess(h, 9);
        CloseHandle(h);
    }
}

/// The program a real-binary test starts: the one the environment names, as
/// the daemon names its own.
fn named_program(var: &str) -> Program {
    let exe = std::env::var_os(var).unwrap_or_else(|| panic!("{var}"));
    Program {
        exe: exe.into(),
        arg0: None,
        args: vec![WORKER_SUBCOMMAND.to_string()],
    }
}

/// The real `roomlerd hive-worker`, kept up as the device's: a process of its
/// own that answers its hello at this build's version, measures, and is
/// started again when it is killed; at its stop, gone. Needs the binary, which
/// a lib test does not build: `ROOMLERD_WORKER_EXE=… cargo test … -- --ignored`.
#[tokio::test]
#[ignore = "needs a built roomlerd with hive,vault, named in ROOMLERD_WORKER_EXE"]
async fn the_real_worker_serves_the_device_and_is_started_again_when_killed() {
    let spawner = OwnBinary(named_program("ROOMLERD_WORKER_EXE"));
    let w = DeviceWorker::start(Arc::new(spawner), Timing::default(), NEVER);
    let s = until(&w, |s| s.up).await;
    assert!(s.up, "{s:?}");
    let first = s.pid.unwrap();
    assert_ne!(first, std::process::id(), "a process of its own");
    measured_ok(&w).await;

    kill_process(first);
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!((s.starts, s.up, s.gave_up), (2, true, false), "{s:?}");
    assert_ne!(s.pid, Some(first), "another");
    measured_ok(&w).await;

    w.stop().await;
    assert!(!w.seen().up);
}

/// The real worker stopped (`SIGSTOP`, a process hung as a process hangs):
/// found by its beats, killed, and another up. Unix: Windows has no signal for
/// it, and its in-process test above stands for it there.
#[cfg(unix)]
#[tokio::test]
#[ignore = "needs a built roomlerd with hive,vault, named in ROOMLERD_WORKER_EXE"]
async fn the_real_worker_stopped_is_found_by_its_beats() {
    let t = Timing {
        beat: Duration::from_millis(200),
        ..Timing::default()
    };
    let spawner = OwnBinary(named_program("ROOMLERD_WORKER_EXE"));
    let w = DeviceWorker::start(Arc::new(spawner), t, NEVER);
    let s = until(&w, |s| s.up).await;
    assert!(s.up, "{s:?}");
    let first = s.pid.unwrap();
    // SAFETY: a signal to the process this test's worker started.
    unsafe { libc::kill(first as i32, libc::SIGSTOP) };
    let s = until(&w, |s| s.starts == 2 && s.up).await;
    assert_eq!((s.starts, s.up, s.gave_up), (2, true, false), "{s:?}");
    assert_ne!(s.pid, Some(first));
    measured_ok(&w).await;
    w.stop().await;
}

/// AC6 on a real older binary: a released `roomlerd`, started as the device's
/// worker, reads the hello and answers at its own version, and the daemon
/// parks with both versions in its word. The binary is the one the environment
/// names: `ROOMLERD_OLDER_WORKER_EXE=… cargo test … -- --ignored`.
#[tokio::test]
#[ignore = "needs a released roomlerd, named in ROOMLERD_OLDER_WORKER_EXE"]
async fn a_real_older_worker_parks_the_device() {
    let spawner = OwnBinary(named_program("ROOMLERD_OLDER_WORKER_EXE"));
    let w = DeviceWorker::start(Arc::new(spawner), Timing::default(), NEVER);
    let s = until(&w, |s| s.gave_up).await;
    assert_eq!((s.gave_up, s.starts), (true, 1), "at its first: {s:?}");
    let probed = w
        .measure(asked("tcp", "127.0.0.1", 80), may_reach(80))
        .await;
    assert_eq!(probed.outcome, ProbeOutcome::Failed, "{probed:?}");
    let detail = probed.detail.unwrap();
    assert!(detail.contains("parked"), "{detail}");
    assert!(detail.contains("it is version"), "{detail}");
    assert!(
        detail.contains(&format!("the daemon {VERSION}")),
        "{detail}"
    );
    w.stop().await;
}
