// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::RootCertStore;

use super::super::swap::tests::{
    AllowAll, Audience, H1, HOST, Leases, ToAudience, ask, get, http, through,
};
use super::super::{Dial, RunBindings, RunLeaves, ca_of, drop_run, start_handed};
use super::*;

const WAIT: Duration = Duration::from_secs(10);

/// Each run's dial and roots, in place of the system's, for the worker that
/// serves its fronts: a test's upstream is on this host's loopback, which
/// the system's dial never reaches.
static SEAMS: LazyLock<Mutex<Given>> = LazyLock::new(Mutex::default);

/// Each run's dial and roots, as [`SEAMS`] holds them.
type Given = HashMap<RunKey, (Arc<dyn Dial>, Roots)>;

pub(super) fn seams_for(run: RunKey, seams: Seams) -> Seams {
    let given = SEAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&run)
        .cloned();
    match given {
        Some((dial, roots)) => Seams {
            dial,
            roots,
            ..seams
        },
        None => seams,
    }
}

fn worker_dials(run: RunKey, dial: Arc<dyn Dial>, roots: Roots) {
    SEAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(run, (dial, roots));
}

/// This process's own uid: the account a test's fronts admit.
fn me() -> u32 {
    // SAFETY: geteuid reads this process's credentials; it cannot fail.
    unsafe { libc::geteuid() }
}

/// Allows every host, and counts how often it was asked.
#[derive(Default)]
struct Counting(AtomicUsize);

#[async_trait::async_trait]
impl Decide for Counting {
    async fn decide(&self, _run: RunKey, _host: &str, _port: u16) -> Decision {
        self.0.fetch_add(1, Ordering::SeqCst);
        Decision::Allowed
    }
}

/// The daemon's seams for a run: `decide`, the test's leases, the run's CA.
fn daemon_seams(decide: Arc<dyn Decide>, leases: Arc<Leases>, dial: Arc<dyn Dial>) -> Seams {
    Seams {
        decide,
        dial,
        bindings: Arc::new(RunBindings(leases)),
        leaves: Arc::new(RunLeaves),
        roots: Roots::System,
    }
}

/// A `CONNECT` through `front`, and the first line of its answer within
/// `within`: `None` where none came.
async fn connect_line(front: std::net::SocketAddr, to: &str, within: Duration) -> Option<String> {
    let mut tcp = TcpStream::connect(front).await.ok()?;
    let ask = format!("CONNECT {to} HTTP/1.1\r\nHost: {to}\r\n\r\n");
    tcp.write_all(ask.as_bytes()).await.ok()?;
    let mut got = Vec::new();
    let read = tokio::time::timeout(within, async {
        let mut b = [0u8; 1];
        while !got.ends_with(b"\r\n") {
            if tcp.read(&mut b).await.ok()? == 0 {
                return None;
            }
            got.push(b[0]);
        }
        Some(())
    })
    .await;
    matches!(read, Ok(Some(()))).then(|| String::from_utf8_lossy(&got).trim_end().to_string())
}

/// The calls cross whole, and a leaf's key and a value never show in a log:
/// `Debug` names neither.
#[test]
fn the_egress_calls_cross_whole_and_no_key_shows() {
    let calls = [
        Call::Egress {
            endpoint: 9,
            spec: Box::new(Spec {
                session: ObjectId::new().to_hex(),
                fence: 2,
                peer: 1000,
            }),
        },
        Call::Decide {
            host: HOST.into(),
            port: 443,
        },
        Call::Bound {
            host: HOST.into(),
            port: 443,
        },
        Call::Valued {
            host: HOST.into(),
            port: 443,
        },
        Call::Leaf { host: HOST.into() },
    ];
    for call in calls {
        let wire = serde_json::to_string(&call).unwrap();
        assert_eq!(serde_json::from_str::<Call>(&wire).unwrap(), call, "{wire}");
    }
    let ca = ca::SessionCa::new(ObjectId::new()).unwrap();
    let made = ca.leaf_for(HOST).unwrap();
    let leaf = Leaf::of(&made);
    let answer = Answer::Leaf { leaf: leaf.clone() };
    let wire = serde_json::to_string(&answer).unwrap();
    assert_eq!(serde_json::from_str::<Answer>(&wire).unwrap(), answer);
    let key = STANDARD.encode(made.key.secret_pkcs8_der());
    assert!(wire.contains(&key), "it crosses the channel, as it must");
    assert!(!format!("{answer:?}").contains(&key), "never in a log");
    let back = leaf.leaf().unwrap();
    assert_eq!(back.chain, made.chain);
    assert_eq!(back.key.secret_pkcs8_der(), made.key.secret_pkcs8_der());
    let value = Answer::Valued {
        valued: vec![(
            placeholder::Bound {
                token: "rph_x".into(),
                lease_id: "l".into(),
                host: HOST.into(),
                port: 443,
                secret: "s".into(),
                field: "f".into(),
            },
            Some(Secret(Zeroizing::new("the-value".into()))),
        )],
    };
    assert!(
        !format!("{value:?}").contains("the-value"),
        "never in a log"
    );
}

/// The daemon answers its worker's calls for the run: a decision asked of
/// the vault once and kept; the run's placeholders and their values; a leaf
/// for an audience of one of them, and none for any other host. Gone with
/// the run.
#[tokio::test]
async fn the_daemon_answers_its_workers_calls_from_the_runs_own() {
    let run = (ObjectId::new(), 1);
    let vault = Arc::new(Counting::default());
    let leases = Arc::new(Leases::default());
    let seams = daemon_seams(vault.clone(), leases.clone(), Arc::new(SystemDialer));
    start_handed(run, me(), seams).unwrap();
    for _ in 0..2 {
        assert_eq!(
            decide(run, HOST, 443).await,
            Answer::Decision {
                decision: Decision::Allowed
            }
        );
    }
    assert_eq!(vault.0.load(Ordering::SeqCst), 1, "the vault asked once");
    assert!(
        matches!(decide(run, "API.test", 443).await, Answer::Refused { .. }),
        "a host not in its one spelling"
    );
    let token = placeholder::bind_for_test(run.0, run.1, HOST, 443);
    lock(&leases.0).insert(format!("test-lease-{token}"), b"the-value".to_vec());
    let Answer::Bound { bound: got } = bound(run, HOST, 443).await else {
        panic!("bound");
    };
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].token, token);
    let Answer::Valued { valued: got } = valued(run, HOST, 443).await else {
        panic!("valued");
    };
    let value = got[0].1.as_ref().expect("its value");
    assert_eq!(STANDARD.decode(value.0.as_bytes()).unwrap(), b"the-value");
    let Answer::Leaf { leaf: made } = leaf(run, HOST).await else {
        panic!("a leaf for the audience");
    };
    assert_eq!(made.leaf().unwrap().chain.len(), 2, "the leaf, then the CA");
    assert!(
        matches!(leaf(run, "other.test").await, Answer::Refused { .. }),
        "no leaf for a host no placeholder names"
    );
    drop_run(run.0, run.1);
    placeholder::drop_run(run.0, run.1);
    assert_eq!(decide(run, HOST, 443).await, stopped(), "gone with the run");
}

/// The device's setting decides where a run's fronts are served: kept for
/// the run's worker where it runs workers, by the daemon where it does not.
#[tokio::test]
async fn where_the_device_runs_workers_the_fronts_are_kept_for_the_worker() {
    for in_worker in [true, false] {
        let run = (ObjectId::new(), 1);
        super::super::start_with(
            run,
            me(),
            Arc::new(AllowAll),
            Arc::new(SystemDialer),
            in_worker,
        )
        .unwrap();
        let kept = runs().get(&run).is_some_and(|s| s.handed.is_some());
        assert_eq!(kept, in_worker, "kept for the worker: {in_worker}");
        drop_run(run.0, run.1);
    }
}

/// Where the run has a worker, the daemon serves none of its fronts'
/// connections: with no worker up, a `CONNECT` is answered by nobody.
#[tokio::test]
async fn where_the_run_has_a_worker_the_daemon_serves_no_front() {
    let run = (ObjectId::new(), 1);
    let seams = daemon_seams(
        Arc::new(AllowAll),
        Arc::new(Leases::default()),
        Arc::new(SystemDialer),
    );
    let fronts = start_handed(run, me(), seams).unwrap();
    let answered = connect_line(fronts.http, "example.test:443", Duration::from_millis(500)).await;
    assert_eq!(answered, None, "nobody answered");
    drop_run(run.0, run.1);
}

/// The run's worker serves its fronts: a `CONNECT` is decided by the daemon
/// and carried by the worker; and a placeholder of an audience is swapped
/// by the worker, under a leaf the daemon's CA made, with the value the
/// daemon read from its lease, as the daemon's own proxy swaps it.
#[tokio::test]
async fn the_runs_worker_serves_its_fronts_with_the_daemons_answers() {
    let audience = Audience::start(false).await;
    let run = (ObjectId::new(), 1);
    let vault = Arc::new(Counting::default());
    let leases = Arc::new(Leases::default());
    let seams = daemon_seams(vault.clone(), leases.clone(), Arc::new(SystemDialer));
    let fronts = start_handed(run, me(), seams).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(audience.ca.clone()).unwrap();
    worker_dials(
        run,
        Arc::new(ToAudience(audience.addr)),
        Roots::Given(Arc::new(roots)),
    );
    crate::hive::worker::run_in_process(run.0, run.1, Arc::new(|_| {}));
    let token = placeholder::bind_for_test(run.0, run.1, HOST, 443);
    lock(&leases.0).insert(format!("test-lease-{token}"), b"the-value".to_vec());
    let ca = ca_of(run.0, run.1).unwrap().der();
    let tls = tokio::time::timeout(WAIT, through(fronts.http, "api.test:443", HOST, &ca, H1))
        .await
        .expect("an answer in time")
        .expect("the run's CA, through the worker");
    let mut send = http(tls).await;
    let (status, _) = ask(
        &mut send,
        get("/user", HOST, Some(&format!("token {token}"))),
    )
    .await;
    assert_eq!(status, hyper::StatusCode::OK);
    assert_eq!(
        audience.seen()[0].authorization,
        ["token the-value"],
        "the daemon's value, swapped in by the worker"
    );
    assert_eq!(vault.0.load(Ordering::SeqCst), 1, "decided by the daemon");
    crate::hive::worker::drop_run(run.0, run.1);
    drop_run(run.0, run.1);
    placeholder::drop_run(run.0, run.1);
}

/// A worker already up as the run's fronts start is handed them then, not
/// at its next start: a `CONNECT` is carried. And as the fronts end, the
/// worker lets go of them: nothing listens on their ports any more.
#[tokio::test]
async fn a_worker_up_before_the_fronts_is_handed_them_and_lets_go_at_their_end() {
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_at = echo.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = echo.accept().await {
            tokio::spawn(async move {
                let (mut rd, mut wr) = tcp.split();
                let _ = tokio::io::copy(&mut rd, &mut wr).await;
            });
        }
    });
    let run = (ObjectId::new(), 1);
    worker_dials(run, Arc::new(ToAudience(echo_at)), Roots::System);
    crate::hive::worker::run_in_process(run.0, run.1, Arc::new(|_| {}));
    let deadline = std::time::Instant::now() + WAIT;
    while crate::hive::worker::link_of(run.0, run.1).is_none() {
        assert!(std::time::Instant::now() < deadline, "the worker came up");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let seams = daemon_seams(
        Arc::new(AllowAll),
        Arc::new(Leases::default()),
        Arc::new(SystemDialer),
    );
    let fronts = start_handed(run, me(), seams).unwrap();
    let mut tcp = TcpStream::connect(fronts.http).await.unwrap();
    tcp.write_all(b"CONNECT up.test:80 HTTP/1.1\r\nHost: up.test:80\r\n\r\n")
        .await
        .unwrap();
    let mut head = Vec::new();
    tokio::time::timeout(WAIT, async {
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(tcp.read(&mut b).await.unwrap(), 1, "an answer");
            head.push(b[0]);
        }
    })
    .await
    .expect("answered by the worker");
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "carried: {}",
        String::from_utf8_lossy(&head)
    );
    tcp.write_all(b"ping").await.unwrap();
    let mut back = [0u8; 4];
    tokio::time::timeout(WAIT, tcp.read_exact(&mut back))
        .await
        .expect("relayed in time")
        .unwrap();
    assert_eq!(&back, b"ping");
    drop_run(run.0, run.1);
    let deadline = std::time::Instant::now() + WAIT;
    while TcpStream::connect(fronts.http).await.is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker let go of the front"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    crate::hive::worker::drop_run(run.0, run.1);
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
