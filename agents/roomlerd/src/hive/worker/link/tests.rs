// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use tokio::net::UnixStream;
use zeroize::Zeroizing;

use super::*;
use crate::hive::vault_connect::handed::{At, Secret, Spec};
use crate::hive::worker::recv;

fn value(s: &str) -> Secret {
    Secret(Zeroizing::new(s.to_string()))
}

/// A bearer endpoint's spec toward `port`, on loopback.
fn spec(port: u16) -> Spec {
    Spec {
        engine: "http".into(),
        host: "localhost".into(),
        port,
        auth_source: None,
        auth: Some("bearer".into()),
        front: String::new(),
        secret: "t/s".into(),
        user: None,
        ca: None,
        at: At::Loopback { peer: 0 },
    }
}

/// The daemon's link over a fresh channel, with a reader that hands each
/// answer to it; and the other side's halves.
fn linked() -> (
    Arc<Link>,
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
    UnixStream,
) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let (ours_d, theirs_d) = UnixStream::pair().unwrap();
    let (rd, wr) = ours.into_split();
    let link = Arc::new(Link::daemon(wr, ours_d));
    let reading = Arc::clone(&link);
    tokio::spawn(async move {
        let mut rd = rd;
        while let Ok(Some(m)) = recv(&mut rd).await {
            if let Msg::Answer { id, answer } = m {
                reading.answered(id, answer);
            }
        }
        reading.end();
    });
    let (trd, twr) = theirs.into_split();
    (link, trd, twr, theirs_d)
}

/// Calls in flight at once are each answered by their own id, whatever the
/// order the answers come back in.
#[tokio::test]
async fn calls_in_flight_are_each_answered_by_their_own_id() {
    let (link, mut rd, mut wr, _d) = linked();
    tokio::spawn(async move {
        let mut asked = Vec::new();
        while asked.len() < 3 {
            match recv(&mut rd).await {
                Ok(Some(Msg::Call {
                    id,
                    call: Call::Values { endpoint },
                })) => asked.push((id, endpoint)),
                other => panic!("not a call: {other:?}"),
            }
        }
        for (id, endpoint) in asked.into_iter().rev() {
            let answer = Answer::Values {
                values: Some(vec![value(&format!("for {endpoint}"))]),
            };
            send(&mut wr, &Msg::Answer { id, answer }).await.unwrap();
        }
    });
    let calls: Vec<_> = (1..=3u64)
        .map(|endpoint| {
            let link = Arc::clone(&link);
            tokio::spawn(async move { link.call(Call::Values { endpoint }, None).await })
        })
        .collect();
    for (endpoint, call) in (1..=3u64).zip(calls) {
        let answer = call.await.unwrap().unwrap();
        let want = Answer::Values {
            values: Some(vec![value(&format!("for {endpoint}"))]),
        };
        assert_eq!(answer, want, "endpoint {endpoint}'s answer");
    }
}

/// A call waiting when the channel goes fails at once, not at its bound; a
/// call made after fails at its start.
#[tokio::test]
async fn a_call_fails_as_the_channel_goes_and_after() {
    let (link, rd, wr, _d) = linked();
    let waiting = {
        let link = Arc::clone(&link);
        tokio::spawn(async move { link.call(Call::Values { endpoint: 1 }, None).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop((rd, wr));
    let failed = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .expect("failed well before its bound")
        .unwrap();
    assert!(failed.is_err(), "{failed:?}");
    let after = link.call(Call::Values { endpoint: 2 }, None).await;
    assert!(after.is_err(), "{after:?}");
}

/// A call's descriptor crosses on the second channel before its frame, so
/// the other side, reading the frame, takes the descriptor that is that
/// call's: in order, call after call.
#[tokio::test]
async fn each_hand_overs_descriptor_is_the_one_beside_its_frame() {
    let (link, mut rd, mut wr, d) = linked();
    let listeners: Vec<std::net::TcpListener> = (0..3)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports: Vec<u16> = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    let other = tokio::spawn(async move {
        let mut got = Vec::new();
        while got.len() < 3 {
            let Ok(Some(Msg::Call {
                id,
                call: Call::Serve { endpoint, spec },
            })) = recv(&mut rd).await
            else {
                panic!("not a serve");
            };
            let fd = super::super::fds::recv(&d).await.unwrap();
            let port = std::net::TcpListener::from(fd).local_addr().unwrap().port();
            got.push((endpoint, spec.port, port));
            send(
                &mut wr,
                &Msg::Answer {
                    id,
                    answer: Answer::Done,
                },
            )
            .await
            .unwrap();
        }
        got
    });
    for (i, l) in listeners.into_iter().enumerate() {
        let spec = spec(ports[i]);
        let fd: OwnedFd = l.into();
        assert!(fd.as_raw_fd() >= 0);
        let answer = link
            .call(
                Call::Serve {
                    endpoint: i as u64,
                    spec: Box::new(spec),
                },
                Some(fd),
            )
            .await
            .expect("each serve answered");
        assert_eq!(answer, Answer::Done);
    }
    let got = other.await.unwrap();
    for (endpoint, named, came) in got {
        assert_eq!(
            named, came,
            "endpoint {endpoint}: its own listener came beside it"
        );
    }
}

/// A call that cannot be sent (the other side's descriptor channel gone,
/// its frames still read) fails, and takes the link for lost, with why:
/// what the daemon's side waits on to end that worker's life.
#[tokio::test]
async fn a_call_that_cannot_be_sent_takes_the_link_for_lost() {
    let (link, _rd, _wr, d) = linked();
    drop(d);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve = Call::Serve {
        endpoint: 1,
        spec: Box::new(spec(port)),
    };
    let failed = link.call(serve, Some(OwnedFd::from(listener))).await;
    assert!(failed.is_err(), "{failed:?}");
    let why = tokio::time::timeout(std::time::Duration::from_secs(5), link.lost())
        .await
        .expect("taken for lost as it could not be sent");
    assert!(why.contains("refused a call"), "{why}");
}

/// A call the other side never answers fails at its bound, and takes the
/// link for lost.
#[tokio::test]
async fn an_unanswered_call_takes_the_link_for_lost() {
    let (link, mut rd, _wr, _d) = linked();
    let calling = {
        let link = Arc::clone(&link);
        tokio::spawn(async move { link.call(Call::Values { endpoint: 1 }, None).await })
    };
    // Its frame read here, the call waits on its bound: only then is the
    // clock paused, so it runs at once to the bound and to nothing before.
    let came = tokio::time::timeout(std::time::Duration::from_secs(5), recv(&mut rd))
        .await
        .expect("the call came");
    assert!(matches!(came, Ok(Some(Msg::Call { .. }))), "{came:?}");
    tokio::time::pause();
    let unanswered = calling.await.unwrap();
    assert!(unanswered.is_err(), "{unanswered:?}");
    let why = tokio::time::timeout(std::time::Duration::from_secs(1), link.lost())
        .await
        .expect("taken for lost at its bound");
    assert!(why.contains("unanswered"), "{why}");
}
