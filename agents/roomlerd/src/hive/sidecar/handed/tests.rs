// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The sidecar's worker hand-over: its own dispatch, against a `Kept`/
//! `Serving` built directly — no real listener or worker process, mirroring
//! `toolbelt::handed::tests`'s own shape.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::*;

const WAIT: Duration = Duration::from_secs(5);

/// A real listener on a free loopback port, as a worker is handed one: its
/// descriptor, or (Windows) protocol info for this process. The test's own
/// copy is gone, so the port lives only as long as the hand-over keeps it.
#[cfg(unix)]
fn handed_listener() -> (OwnedFd, u16) {
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("a listener binds");
    let port = listener.local_addr().expect("its address").port();
    (OwnedFd::from(listener), port)
}

#[cfg(windows)]
fn handed_listener() -> (Vec<u8>, u16) {
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("a listener binds");
    let port = listener.local_addr().expect("its address").port();
    let info = crate::hive::worker::win::duplicate_for(
        std::process::id(),
        listener.as_raw_socket() as SOCKET,
    )
    .expect("its protocol info");
    (info.to_vec(), port)
}

/// A worker's end of a channel nobody reads: what a call on it asks is never
/// answered, and these tests make none.
fn nobodys_link() -> Arc<Link> {
    Arc::new(Link::worker(tokio::io::sink()))
}

/// Whether nothing takes a connection on `port` any more, within `WAIT`: a
/// stop is seen by its accept loop a moment after it is asked.
async fn port_closes(port: u16) -> bool {
    let deadline = tokio::time::Instant::now() + WAIT;
    while TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .is_ok()
    {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::test]
async fn an_endpoint_nobody_kept_is_refused() {
    let answer = admit(ObjectId::new(), 1, 1, Some("tok".into())).await;
    assert!(matches!(answer, Answer::Refused { .. }));
}

#[tokio::test]
async fn forgetting_a_key_nobody_kept_is_a_quiet_done() {
    let answer = key_forgotten(ObjectId::new(), 1, 1).await;
    assert!(matches!(answer, Answer::Done));
}

#[test]
fn prepare_answers_none_where_the_run_has_no_worker() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let listener = rt
        .block_on(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))
        .expect("a listener binds");
    let session = ObjectId::new();
    let prepared = prepare(
        session,
        1,
        &listener,
        Weak::new(),
        "https://x".into(),
        None,
        false,
    )
    .expect("prepare does not fail just because there is no worker");
    assert!(prepared.is_none());
    assert!(!is_kept(session, 1));
}

/// FR-90 P8-2c-4 — a fresh launch's own case: the device runs workers, and
/// this run's has not started yet, as it never has when its sidecar is
/// opened. Kept for it all the same: asked whether a worker exists yet
/// (`worker::has`), this answered no at every launch, and nothing was
/// ever handed.
#[tokio::test]
async fn prepare_keeps_it_for_a_worker_not_started_yet() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("a listener binds");
    let session = ObjectId::new();
    assert!(!crate::hive::worker::has(session, 1), "no worker yet");
    let prepared = prepare(
        session,
        1,
        &listener,
        Weak::new(),
        "https://x".into(),
        None,
        true,
    )
    .expect("kept");
    assert!(prepared.is_some(), "handed, not served by the daemon");
    assert!(is_kept(session, 1), "kept for the worker to come");
    forget(session, 1);
}

#[tokio::test]
async fn a_kept_sidecar_whose_supervisor_is_gone_is_refused_not_panicked() {
    let session = ObjectId::new();
    // `Weak::new()` never upgrades: stands in for a daemon that has
    // already dropped its `Supervisor` by the time a worker's call
    // arrives (shutdown racing a worker's last request).
    keep_for_test(session, 1, 7, Weak::new());
    assert!(is_kept(session, 1));
    let answer = admit(session, 1, 7, Some("tok".into())).await;
    assert!(matches!(answer, Answer::Refused { .. }));
    forget(session, 1);
    assert!(!is_kept(session, 1));
}

/// A hand-over answered twice (one that crossed a new worker's) is served
/// once: the second answer is the first's, and its listener goes with it.
#[tokio::test]
async fn a_hand_over_answered_twice_serves_it_once() {
    let serving: Arc<Serving> = Arc::default();
    let (first, first_port) = handed_listener();
    let (second, second_port) = handed_listener();
    for listener in [first, second] {
        let answer = serve_handed(
            &serving,
            nobodys_link(),
            ObjectId::new(),
            42,
            "http://127.0.0.1:9".into(),
            None,
            listener,
        );
        assert!(matches!(answer, Answer::Done));
    }
    assert!(
        TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, first_port))
            .await
            .is_ok(),
        "the first is served"
    );
    assert!(
        port_closes(second_port).await,
        "the second's listener went with its answer"
    );
    close_handed(&serving, 42);
    assert!(port_closes(first_port).await, "and the first, at its close");
}

/// #3305 — a hand-over whose close came first is not served. Frames are read
/// in order, but each call is answered on a task of its own on a runtime of
/// many threads, and a close can also cross one a new worker's rehand listed
/// before the sidecar was forgotten. Served, nothing would stop it but the
/// worker's end, its port open all that while.
#[tokio::test]
async fn a_hand_over_that_crosses_its_close_is_not_served() {
    let serving: Arc<Serving> = Arc::default();
    assert!(matches!(close_handed(&serving, 43), Answer::Done));
    let (listener, port) = handed_listener();
    let answer = serve_handed(
        &serving,
        nobodys_link(),
        ObjectId::new(),
        43,
        "http://127.0.0.1:9".into(),
        None,
        listener,
    );
    assert!(matches!(answer, Answer::Done));
    assert!(
        port_closes(port).await,
        "the listener that came after the close went with its answer"
    );
}

/// #3305 — `Call::SidecarClose` cuts the connections, as it says: one left
/// open by a process of the session goes with the sidecar, and is not served
/// on past it.
#[tokio::test]
async fn closing_a_handed_sidecar_cuts_its_connections() {
    let serving: Arc<Serving> = Arc::default();
    let (listener, port) = handed_listener();
    let answer = serve_handed(
        &serving,
        nobodys_link(),
        ObjectId::new(),
        44,
        "http://127.0.0.1:9".into(),
        None,
        listener,
    );
    assert!(matches!(answer, Answer::Done));
    let mut sock = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .expect("served");
    // A path it does not forward is answered without asking anyone, and the
    // connection stays open for the next request.
    sock.write_all(b"GET /nope HTTP/1.1\r\nhost: x\r\n\r\n")
        .await
        .unwrap();
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("not_found_error") {
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(WAIT, sock.read(&mut buf))
            .await
            .expect("the answer in time")
            .unwrap();
        assert_ne!(n, 0, "the answer before the end: {seen:?}");
        seen.extend_from_slice(&buf[..n]);
    }

    assert!(matches!(close_handed(&serving, 44), Answer::Done));
    let mut buf = [0u8; 1024];
    let after = tokio::time::timeout(WAIT, sock.read(&mut buf)).await;
    assert!(
        matches!(after, Ok(Ok(0)) | Ok(Err(_))),
        "the connection is cut at the close: {after:?}"
    );
}

/// #3305 — the race itself, on a worker's real loop and a runtime of many
/// threads: a close sent right behind its hand-over, in that order on the
/// wire, each answered on a task of its own. Whichever runs first, nothing
/// is left served.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_racing_its_hand_over_leaves_nothing_served() {
    use crate::hive::worker::{Msg, recv};
    const ROUNDS: u64 = 200;
    const ENOUGH: usize = 3;

    let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
    let (ours_d, theirs_d) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(crate::hive::worker::serve(theirs, Some(theirs_d), None));
    let (mut rd, wr) = ours.into_split();
    let link = Arc::new(Link::daemon(wr, ours_d));
    let reading = Arc::clone(&link);
    tokio::spawn(async move {
        while let Ok(Some(m)) = recv(&mut rd).await {
            if let Msg::Answer { id, answer } = m {
                reading.answered(id, answer);
            }
        }
    });

    let session = ObjectId::new();
    let mut leaked = Vec::new();
    for endpoint in 1..=ROUNDS {
        let (listener, port) = handed_listener();
        let (served, closed) = tokio::join!(
            link.call(
                Call::SidecarServe {
                    endpoint,
                    session,
                    upstream: "http://127.0.0.1:9".into(),
                    workspace: None,
                },
                vec![listener],
            ),
            link.call(Call::SidecarClose { endpoint }, Vec::new()),
        );
        assert_eq!(served.unwrap(), Answer::Done);
        assert_eq!(closed.unwrap(), Answer::Done);
        if !port_closes(port).await {
            leaked.push(endpoint);
            if leaked.len() == ENOUGH {
                break;
            }
        }
    }
    assert!(
        leaked.is_empty(),
        "still served after their close, in {ROUNDS} rounds at most: {leaked:?}"
    );
}

/// #3305 — a refusal's kind crosses in the `snake_case` of every other enum on
/// this wire, and each comes back the kind it went. Safe to have renamed:
/// no release has the vault, so no fielded build speaks this, and a worker of
/// another version never gets this far, since `life` ends at its `ready`
/// before the first call (`a_worker_of_another_build_is_given_up_on_at_once`).
#[test]
fn a_refusal_kind_crosses_in_snake_case() {
    use super::super::AdmitRefusalKind;
    for (kind, wire) in [
        (AdmitRefusalKind::Authentication, "authentication"),
        (AdmitRefusalKind::Overloaded, "overloaded"),
        (AdmitRefusalKind::Api, "api"),
    ] {
        let answer = Answer::SidecarRefused {
            kind,
            message: "why".into(),
        };
        let json = serde_json::to_value(&answer).unwrap();
        assert_eq!(json["kind"], wire);
        assert_eq!(serde_json::from_value::<Answer>(json).unwrap(), answer);
    }
}

/// #3305 — an admit waits past all the daemon may take to answer it: the
/// server's word, then the key helper's whole time limit. Nothing else moves.
/// A call the daemon makes of its worker keeps the session's bound, so a hung
/// worker is found as soon as it ever was.
#[test]
fn only_a_sidecar_admit_waits_past_a_sessions_call_bound() {
    use crate::hive::supervisor::LEASE_WAIT;
    use crate::hive::worker::link::{ADMIT_WAIT, CALL_WAIT, Role, SessionRole};
    assert!(
        ADMIT_WAIT > LEASE_WAIT + super::super::HELPER_TIMEOUT,
        "{ADMIT_WAIT:?}"
    );
    assert_eq!(
        SessionRole::wait_of(&Call::SidecarAdmit {
            endpoint: 1,
            token: None,
        }),
        ADMIT_WAIT
    );
    for call in [
        Call::Close { endpoint: 1 },
        Call::Values { endpoint: 1 },
        Call::SidecarClose { endpoint: 1 },
        Call::SidecarForgetKey { endpoint: 1 },
    ] {
        assert_eq!(SessionRole::wait_of(&call), CALL_WAIT, "{call:?}");
    }
}
