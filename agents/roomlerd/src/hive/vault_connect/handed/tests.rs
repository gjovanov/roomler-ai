// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::hive::worker::{Msg, recv};
use crate::vault_proxy::testing::{fake_http, made};

const WAIT: Duration = Duration::from_secs(10);

/// This process's own uid: the account a test's endpoint serves.
fn me() -> u32 {
    // SAFETY: geteuid reads this process's credentials; it cannot fail.
    unsafe { libc::geteuid() }
}

/// A bearer endpoint toward the fake upstream at `port`, its CA `ca`.
fn bearer(port: u16, ca: &str) -> Spec {
    Spec {
        engine: "http".into(),
        host: "localhost".into(),
        port,
        auth_source: None,
        auth: Some("bearer".into()),
        front: String::new(),
        secret: "git/bot".into(),
        user: None,
        ca: Some(ca.into()),
        at: At::Loopback { peer: me() },
    }
}

/// The test plays the daemon on its end of a worker's channels, the worker's
/// own loop on the other: each `values` call answered with `token` while
/// `held` holds, and none once it does not.
struct Daemon {
    link: Arc<Link>,
    asked: Arc<std::sync::atomic::AtomicUsize>,
}

async fn daemon(token: String, held: Arc<AtomicBool>) -> Daemon {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let (ours_d, theirs_d) = UnixStream::pair().unwrap();
    tokio::spawn(crate::hive::worker::serve(theirs, Some(theirs_d)));
    let (mut rd, wr) = ours.into_split();
    let link = Arc::new(Link::daemon(wr, ours_d));
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (reading, counting) = (Arc::clone(&link), Arc::clone(&asked));
    tokio::spawn(async move {
        while let Ok(Some(m)) = recv(&mut rd).await {
            match m {
                Msg::Answer { id, answer } => {
                    reading.answered(id, answer);
                }
                Msg::Call {
                    id,
                    call: Call::Values { .. },
                } => {
                    counting.fetch_add(1, Ordering::SeqCst);
                    let values = held
                        .load(Ordering::SeqCst)
                        .then(|| vec![Secret(Zeroizing::new(token.clone()))]);
                    let _ = reading.answer(id, Answer::Values { values }).await;
                }
                _ => {}
            }
        }
    });
    Daemon { link, asked }
}

/// One request through the endpoint at `port`: the response's head.
async fn ask(port: u16) -> String {
    let mut h = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    h.write_all(b"GET /api/x HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(WAIT, h.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

/// A handed endpoint is served by the worker's side of the channel: a
/// request through its listener reaches the upstream with the value the
/// daemon answered `values` with, asked as the connection started; once the
/// lease is let go, a connection is refused; at `close`, no connection is
/// taken at all.
#[tokio::test]
async fn a_handed_endpoint_is_served_with_the_values_the_daemon_answers() {
    let token = made();
    let (ca, port, mut saw) = fake_http().await;
    let held = Arc::new(AtomicBool::new(true));
    let d = daemon(token.clone(), Arc::clone(&held)).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap().port();
    let served = d
        .link
        .call(
            Call::Serve {
                endpoint: 11,
                spec: Box::new(bearer(port, &ca)),
            },
            vec![OwnedFd::from(listener)],
        )
        .await
        .unwrap();
    assert_eq!(served, Answer::Done);

    let response = ask(at).await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let s = tokio::time::timeout(WAIT, saw.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.target, "/api/x");
    assert_eq!(s.values("authorization"), [format!("Bearer {token}")]);
    assert_eq!(
        d.asked.load(Ordering::SeqCst),
        1,
        "asked once, as it started"
    );

    held.store(false, Ordering::SeqCst);
    let refused = ask(at).await;
    assert!(
        !refused.starts_with("HTTP/1.1 200 OK") && !refused.contains(&token),
        "the lease is gone: {refused}"
    );

    let closed = d
        .link
        .call(Call::Close { endpoint: 11 }, Vec::new())
        .await
        .unwrap();
    assert_eq!(closed, Answer::Done);
    // The worker's copy of the listener is the only one here: at its close
    // it goes, and the port with it.
    let deadline = std::time::Instant::now() + WAIT;
    while TcpStream::connect(("127.0.0.1", at)).await.is_ok() {
        assert!(std::time::Instant::now() < deadline, "the endpoint stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An endpoint handed twice (a hand-over that crossed a new worker's) is
/// served once: the second answer is the first's, and its listener goes.
#[tokio::test]
async fn an_endpoint_handed_twice_is_served_once() {
    let (ca, port, _saw) = fake_http().await;
    let d = daemon(made(), Arc::new(AtomicBool::new(true))).await;
    let mut ports = Vec::new();
    for _ in 0..2 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        ports.push(listener.local_addr().unwrap().port());
        let answer = d
            .link
            .call(
                Call::Serve {
                    endpoint: 12,
                    spec: Box::new(bearer(port, &ca)),
                },
                vec![OwnedFd::from(listener)],
            )
            .await
            .unwrap();
        assert_eq!(answer, Answer::Done);
    }
    // The first is served; the second's listener went with its answer, and
    // nothing holds its port.
    assert!(ask(ports[0]).await.starts_with("HTTP/1.1 200 OK"));
    assert!(
        TcpStream::connect(("127.0.0.1", ports[1])).await.is_err(),
        "the second listener is not served"
    );
    let _ = d.link.call(Call::Close { endpoint: 12 }, Vec::new()).await;
}

/// Two hand-overs of one endpoint answered at once, each on a task of its
/// own on a runtime of many threads, serve it once: on one of the two
/// listeners, never both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hand_overs_of_one_endpoint_at_once_serve_it_once() {
    let (ca, port, _saw) = fake_http().await;
    let d = daemon(made(), Arc::new(AtomicBool::new(true))).await;
    let listeners: Vec<std::net::TcpListener> = (0..2)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports: Vec<u16> = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    let calls: Vec<_> = listeners
        .into_iter()
        .map(|l| {
            let (link, spec) = (Arc::clone(&d.link), bearer(port, &ca));
            tokio::spawn(async move {
                let serve = Call::Serve {
                    endpoint: 14,
                    spec: Box::new(spec),
                };
                link.call(serve, vec![OwnedFd::from(l)]).await
            })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap().unwrap(), Answer::Done);
    }
    let mut served = 0;
    for p in ports {
        if TcpStream::connect(("127.0.0.1", p)).await.is_ok() {
            served += 1;
        }
    }
    assert_eq!(served, 1, "one of the two listeners, never both");
    let _ = d.link.call(Call::Close { endpoint: 14 }, Vec::new()).await;
}

/// A hand-over that crossed its endpoint's close (a new worker's, listed
/// before the close) is not served: the listener that came after the close
/// goes with its answer.
#[tokio::test]
async fn a_hand_over_that_crosses_its_close_is_not_served() {
    let (ca, port, _saw) = fake_http().await;
    let d = daemon(made(), Arc::new(AtomicBool::new(true))).await;
    let closed = d
        .link
        .call(Call::Close { endpoint: 13 }, Vec::new())
        .await
        .unwrap();
    assert_eq!(closed, Answer::Done);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap().port();
    let late = d
        .link
        .call(
            Call::Serve {
                endpoint: 13,
                spec: Box::new(bearer(port, &ca)),
            },
            vec![OwnedFd::from(listener)],
        )
        .await
        .unwrap();
    assert_eq!(late, Answer::Done);
    assert!(
        TcpStream::connect(("127.0.0.1", at)).await.is_err(),
        "a hand-over after its close is not served"
    );
}

/// A worker that follows a lost one is handed every endpoint again, on the
/// same listener: the address the session was given serves again (a request
/// made in the gap waits in its backlog), with the daemon's values.
#[tokio::test]
async fn a_worker_that_follows_is_handed_the_endpoint_again() {
    let token = made();
    let (ca, port, mut saw) = fake_http().await;
    let (session, fence) = (ObjectId::new(), 1);
    crate::hive::worker::run_in_process_lost_after(session, fence, Duration::from_millis(600));
    // The daemon's own copy, as its place holds it until the close.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap().port();
    let t = token.clone();
    let reader: Values = Arc::new(move || Some(vec![Zeroizing::new(t.clone())]));
    let handed = hand(session, fence, bearer(port, &ca), &listener, reader)
        .await
        .expect("handed over")
        .expect("the run has a worker");
    assert!(
        ask(at).await.starts_with("HTTP/1.1 200 OK"),
        "the first serves it"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    let again = ask(at).await;
    assert!(
        again.starts_with("HTTP/1.1 200 OK"),
        "the next serves it: {again}"
    );
    let s = crate::hive::worker::seen(session, fence).unwrap();
    assert!(s.starts >= 2, "a second worker: {s:?}");
    for _ in 0..2 {
        let s = tokio::time::timeout(WAIT, saw.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.values("authorization"), [format!("Bearer {token}")]);
    }
    handed.close().await;
    crate::hive::worker::drop_run(session, fence);
}

/// A worker whose listener channel went while its frames still flow is
/// taken for lost as a hand-over cannot cross it, and another is started:
/// the endpoint is served by the next worker, never left with nobody
/// accepting on it while the one that cannot serve it answers its beats.
#[tokio::test]
async fn a_worker_whose_listener_channel_went_is_replaced_and_its_endpoint_served() {
    let token = made();
    let (ca, port, mut saw) = fake_http().await;
    let (session, fence) = (ObjectId::new(), 1);
    crate::hive::worker::run_in_process_first_without_descriptors(session, fence);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap().port();
    let t = token.clone();
    let reader: Values = Arc::new(move || Some(vec![Zeroizing::new(t.clone())]));
    let handed = hand(session, fence, bearer(port, &ca), &listener, reader)
        .await
        .expect("kept for the next worker")
        .expect("the run has a worker");
    let served = ask(at).await;
    assert!(
        served.starts_with("HTTP/1.1 200 OK"),
        "served by the next worker: {served}"
    );
    let s = crate::hive::worker::seen(session, fence).unwrap();
    assert!(s.starts >= 2, "the first was taken for lost: {s:?}");
    let up = tokio::time::timeout(WAIT, saw.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(up.values("authorization"), [format!("Bearer {token}")]);
    handed.close().await;
    crate::hive::worker::drop_run(session, fence);
}

/// A run whose worker the daemon gave up on has its endpoint refused at
/// once, not after the wait for a worker coming up, and nothing kept.
#[tokio::test]
async fn a_run_whose_worker_was_given_up_on_is_refused_at_once() {
    let (session, fence) = (ObjectId::new(), 1);
    crate::hive::worker::run_never_up(session, fence, Arc::new(|_| {}));
    let deadline = std::time::Instant::now() + WAIT;
    while !crate::hive::worker::seen(session, fence).is_some_and(|s| s.gave_up) {
        assert!(std::time::Instant::now() < deadline, "given up on");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let none: Values = Arc::new(|| None);
    let asked = std::time::Instant::now();
    let refused = hand(session, fence, bearer(443, ""), &listener, none).await;
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "refused at once: {:?}",
        asked.elapsed()
    );
    assert!(matches!(refused, Err(ref why) if why.contains("not up")));
    assert_eq!(kept_for(session, fence), 0, "nothing kept");
    crate::hive::worker::drop_run(session, fence);
}

/// The spec crosses as it was made: every field the worker serves with, and
/// nothing of the values; one of an engine or auth this build does not know
/// is refused, not served with a guess.
#[test]
fn a_spec_crosses_whole_and_an_unknown_one_is_refused() {
    let spec = bearer(
        443,
        "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n",
    );
    let wire = serde_json::to_string(&Call::Serve {
        endpoint: 3,
        spec: Box::new(spec.clone()),
    })
    .unwrap();
    let back: Call = serde_json::from_str(&wire).unwrap();
    assert_eq!(
        back,
        Call::Serve {
            endpoint: 3,
            spec: Box::new(spec)
        }
    );
    let unknown = Spec {
        engine: "smtp".into(),
        ..bearer(25, "")
    };
    assert!(Conf::of_spec(&unknown).is_err(), "an engine not known");
    let unknown_auth = Spec {
        auth: Some("kerberos".into()),
        ..bearer(443, "")
    };
    assert!(Conf::of_spec(&unknown_auth).is_err(), "an auth not known");
}

/// A value never shows in a log line: `Debug` names none.
#[test]
fn a_value_never_shows_in_a_log() {
    let token = made();
    let answer = Answer::Values {
        values: Some(vec![Secret(Zeroizing::new(token.clone()))]),
    };
    assert!(!format!("{answer:?}").contains(&token));
    let wire = serde_json::to_string(&answer).unwrap();
    assert!(wire.contains(&token), "it crosses the channel, as it must");
}

/// The daemon's side: `values` for an endpoint answers what its reader
/// reads, none once its leases are let go, and a refusal for an endpoint it
/// never handed.
#[test]
fn the_daemon_answers_values_from_the_endpoints_reader() {
    let (session, fence) = (ObjectId::new(), 1);
    let token = made();
    let held = Arc::new(AtomicBool::new(true));
    let (t, h) = (token.clone(), Arc::clone(&held));
    let reader: Values = Arc::new(move || {
        h.load(Ordering::SeqCst)
            .then(|| vec![Zeroizing::new(t.clone())])
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    kept().entry((session, fence)).or_default().insert(
        5,
        Kept {
            spec: bearer(443, ""),
            listener: OwnedFd::from(listener),
            values: reader,
        },
    );
    assert_eq!(
        values(session, fence, 5),
        Answer::Values {
            values: Some(vec![Secret(Zeroizing::new(token))])
        }
    );
    held.store(false, Ordering::SeqCst);
    assert_eq!(values(session, fence, 5), Answer::Values { values: None });
    assert!(matches!(values(session, fence, 6), Answer::Refused { .. }));
    Handed {
        session,
        fence,
        endpoint: 5,
    }
    .forget();
    assert!(matches!(values(session, fence, 5), Answer::Refused { .. }));
}
