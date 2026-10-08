// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The viewer peer, end to end on one host: the supervisor's own rig (a fake
//! harness, an in-memory store), and a webrtc-rs peer standing in for the
//! browser. The signalling the server would relay is relayed here by a pump
//! between the rig's report channel and the "browser".

use std::time::Duration;

use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use super::super::supervisor::tests::{
    Rig, dev, harness_asks, next_state, order, release, rig, until_awaiting,
};
use super::*;

const WAIT: Duration = Duration::from_secs(15);

fn grant(session: ObjectId, user: ObjectId, may_prompt: bool, ttl_secs: u32) -> ViewGrant {
    ViewGrant {
        grant_id: ObjectId::new(),
        session_id: session,
        user_id: user,
        user_name: "Viewer".into(),
        may_prompt,
        ttl_secs,
    }
}

/// Start a session, run one turn, and wait until it is idle again.
async fn session_with_a_turn(r: &mut Rig) -> ObjectId {
    let o = order(r);
    let sid = o.session_id;
    assert!(r.sup.start(o, true).await.refused.is_none());
    assert_eq!(next_state(r, sid).await.0, HiveRunState::Idle);
    r.sup.prompt(sid, dev(r), "first".into()).unwrap();
    assert_eq!(next_state(r, sid).await.0, HiveRunState::Running);
    assert_eq!(next_state(r, sid).await.0, HiveRunState::Idle);
    sid
}

/// The next `rc:hive.view.closed` on the rig's channel.
async fn next_closed(r: &mut Rig) -> (ObjectId, String) {
    loop {
        let msg = tokio::time::timeout(WAIT, r.reports.recv())
            .await
            .expect("a report in time")
            .expect("the reporter is open");
        if let ClientMsg::HiveViewClosed { grant_id, reason } = msg {
            return (grant_id, reason);
        }
    }
}

async fn wait_viewers(r: &Rig, n: usize) {
    let deadline = Instant::now() + WAIT;
    while r.sup.viewers().count() != n {
        assert!(Instant::now() < deadline, "viewers never reached {n}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_grant_passes_the_devices_gates_or_names_the_one_that_said_no() {
    let off = rig(false, 4);
    let g = grant(ObjectId::new(), off.user, false, 60);
    assert_eq!(
        off.sup.view_grant(g, true).await.unwrap_err().0,
        HiveViewRefusal::HiveDisabled
    );

    let mut r = rig(true, 4);
    let sid = session_with_a_turn(&mut r).await;
    assert_eq!(
        r.sup
            .view_grant(grant(sid, r.user, false, 60), false)
            .await
            .unwrap_err()
            .0,
        HiveViewRefusal::HiveDisabled,
        "a secondary organization reads nothing here"
    );
    assert_eq!(
        r.sup
            .view_grant(grant(ObjectId::new(), r.user, false, 60), true)
            .await
            .unwrap_err()
            .0,
        HiveViewRefusal::NoSession
    );

    // Idempotent on the grant id; bounded per session.
    let first = grant(sid, r.user, false, 60);
    r.sup.view_grant(first.clone(), true).await.unwrap();
    r.sup.view_grant(first.clone(), true).await.unwrap();
    assert_eq!(
        r.sup.viewers().count(),
        1,
        "a re-sent grant is not a second viewer"
    );
    let mut held = vec![first.grant_id];
    for _ in 1..view_limits::MAX_PER_SESSION {
        let g = grant(sid, r.user, false, 60);
        held.push(g.grant_id);
        r.sup.view_grant(g, true).await.unwrap();
    }
    assert_eq!(
        r.sup
            .view_grant(grant(sid, r.user, false, 60), true)
            .await
            .unwrap_err()
            .0,
        HiveViewRefusal::AtCapacity
    );
    // The server's close ends each one, and says nothing back.
    for g in held {
        r.sup.viewers().command(g, Cmd::Close("test".into()));
    }
    wait_viewers(&r, 0).await;
    r.sup.stop(sid, 1, "owner".into());
}

/// A grant holds a place; one nobody dials frees it, and the server hears
/// why, so the grant leaves its table too.
#[tokio::test]
async fn a_grant_nobody_dials_ends_and_says_so() {
    let mut r = rig(true, 4);
    let sid = session_with_a_turn(&mut r).await;
    let g = grant(sid, r.user, false, 60);
    let gid = g.grant_id;
    r.sup.view_grant(g, true).await.unwrap();
    assert_eq!(next_closed(&mut r).await, (gid, "no_offer".into()));
    wait_viewers(&r, 0).await;
    r.sup.stop(sid, 1, "owner".into());
}

/// The TTL is the device's own clock; a renewal moves it.
#[tokio::test]
async fn an_unrenewed_grant_expires_and_a_renewed_one_does_not() {
    let mut r = rig(true, 4);
    let sid = session_with_a_turn(&mut r).await;
    let short = grant(sid, r.user, false, 1);
    let gid = short.grant_id;
    r.sup.view_grant(short, true).await.unwrap();
    assert_eq!(next_closed(&mut r).await, (gid, "expired".into()));

    let renewed = grant(sid, r.user, false, 1);
    let gid = renewed.grant_id;
    r.sup.view_grant(renewed, true).await.unwrap();
    r.sup.viewers().command(gid, Cmd::Renew(60));
    // Past the first TTL it is still held — and then it ends for the OTHER
    // reason, which proves the renewal moved the deadline.
    assert_eq!(next_closed(&mut r).await, (gid, "no_offer".into()));
    r.sup.stop(sid, 1, "owner".into());
}

/// The browser's side of one viewer peer.
struct Browser {
    pc: Arc<RTCPeerConnection>,
    dc: Arc<RTCDataChannel>,
    inbox: mpsc::Receiver<Value>,
    next_id: u32,
}

impl Browser {
    async fn send(&mut self, v: Value) {
        let bytes = serde_json::to_vec(&v).unwrap();
        for frame in framing::encode(self.next_id, &bytes) {
            self.dc.send(&Bytes::from(frame)).await.unwrap();
        }
        self.next_id += 1;
    }

    /// The next message with `op`, skipping others (state pushes and the
    /// like), up to the test's patience.
    async fn recv_op(&mut self, op: &str) -> Value {
        loop {
            let v = tokio::time::timeout(WAIT, self.inbox.recv())
                .await
                .unwrap_or_else(|_| panic!("no {op:?} in time"))
                .expect("the channel is open");
            if v["op"] == op {
                return v;
            }
        }
    }

    /// Every transcript event that arrives until `until` matches one.
    async fn events_until(&mut self, until: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut got = Vec::new();
        loop {
            let v = self.recv_op("events").await;
            for e in v["events"].as_array().unwrap() {
                got.push(e.clone());
                if until(e) {
                    return got;
                }
            }
        }
    }
}

/// Dial `grant`'s viewer like a browser would: a `hive` channel, an offer,
/// and the device's answer and candidates relayed back from the rig's
/// report channel (everything else on it is passed through).
async fn dial(r: &mut Rig, gid: ObjectId) -> Browser {
    let api = APIBuilder::new().build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let dc = pc.create_data_channel(CHANNEL, None).await.unwrap();
    let (inbox_tx, inbox) = mpsc::channel(256);
    {
        let reasm = Arc::new(std::sync::Mutex::new(Reassembler::new(Bounds {
            max_message_bytes: 8 * 1024 * 1024,
            max_in_flight: 4,
        })));
        dc.on_message(Box::new(move |msg| {
            let inbox = inbox_tx.clone();
            let reasm = Arc::clone(&reasm);
            Box::pin(async move {
                let done = reasm
                    .lock()
                    .unwrap()
                    .push(&msg.data)
                    .expect("a valid frame");
                if let Some(m) = done {
                    let _ = inbox.send(serde_json::from_slice(&m).unwrap()).await;
                }
            })
        }));
    }
    // The browser's candidates wait until the offer has gone: the server
    // relays them in the order the browser sent them.
    let (cand_tx, mut cand_rx) = mpsc::channel::<Value>(64);
    pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let tx = cand_tx.clone();
        Box::pin(async move {
            if let Some(c) = c
                && let Ok(init) = c.to_json()
            {
                let _ = tx.send(serde_json::to_value(init).unwrap()).await;
            }
        })
    }));
    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer.clone()).await.unwrap();
    r.sup.viewers().command(
        gid,
        Cmd::Offer {
            sdp: offer.sdp,
            ice_servers: vec![],
        },
    );
    let sup = Arc::clone(&r.sup);
    tokio::spawn(async move {
        while let Some(c) = cand_rx.recv().await {
            sup.viewers().command(gid, Cmd::Ice(c));
        }
    });

    // The pump: the server's relay, for this grant.
    let (pass_tx, pass_rx) = mpsc::channel(256);
    let mut from_device = std::mem::replace(&mut r.reports, pass_rx);
    let browser_pc = Arc::clone(&pc);
    tokio::spawn(async move {
        while let Some(msg) = from_device.recv().await {
            match msg {
                ClientMsg::HiveViewAnswer { grant_id, sdp } if grant_id == gid => {
                    let answer = RTCSessionDescription::answer(sdp).unwrap();
                    browser_pc.set_remote_description(answer).await.unwrap();
                }
                ClientMsg::HiveViewIce {
                    grant_id,
                    candidate,
                } if grant_id == gid => {
                    let init: RTCIceCandidateInit = serde_json::from_value(candidate).unwrap();
                    let _ = browser_pc.add_ice_candidate(init).await;
                }
                other => {
                    let _ = pass_tx.send(other).await;
                }
            }
        }
    });

    let deadline = Instant::now() + WAIT;
    while dc.ready_state() != RTCDataChannelState::Open {
        assert!(Instant::now() < deadline, "the viewer channel never opened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Browser {
        pc,
        dc,
        inbox,
        next_id: 0,
    }
}

#[tokio::test]
async fn a_viewer_pages_follows_and_prompts_over_the_peer() {
    let mut r = rig(true, 4);
    let sid = session_with_a_turn(&mut r).await;
    let g = grant(sid, r.user, true, 60);
    let gid = g.grant_id;
    r.sup.view_grant(g, true).await.unwrap();
    let mut b = dial(&mut r, gid).await;

    b.send(json!({"op": "hello"})).await;
    let hello = b.recv_op("hello").await;
    assert_eq!(hello["session"], sid.to_hex());
    assert_eq!(hello["may_prompt"], true);
    assert_eq!(hello["live"], true);
    assert_eq!(hello["state"], "idle");
    let tip = hello["tip"].as_u64().unwrap();
    assert!(tip >= 4, "the first turn is in the store: {hello}");

    // A page is bounded and says there is more.
    b.send(json!({"op": "page", "after": 0, "limit": 2})).await;
    let page = b.recv_op("page").await;
    let events = page["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["event"]["kind"], "user_message");
    assert_eq!(page["more"], true);

    // Following from the tip, then prompting over the peer: the turn
    // streams back, attributed to this viewer.
    b.send(json!({"op": "follow", "after": tip})).await;
    b.send(json!({"op": "prompt", "id": "p1", "text": "second"}))
        .await;
    let ack = b.recv_op("prompt").await;
    assert_eq!(ack, json!({"op": "prompt", "id": "p1", "ok": true}));
    let live = b.events_until(|e| e["event"]["kind"] == "turn").await;
    assert_eq!(
        live[0]["seq"],
        tip + 1,
        "live picks up exactly after the tip"
    );
    assert_eq!(live[0]["event"]["kind"], "user_message");
    assert_eq!(live[0]["event"]["author"], "Viewer");
    assert_eq!(live[0]["event"]["text"], "second");
    let seqs: Vec<u64> = live.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "no gap, no repeat: {seqs:?}"
    );

    // The server closes it: the device closes the peer and says nothing
    // back, because the server already knows.
    r.sup
        .viewers()
        .command(gid, Cmd::Close("viewer_left".into()));
    wait_viewers(&r, 0).await;
    let deadline = Instant::now() + WAIT;
    while b.dc.ready_state() == RTCDataChannelState::Open {
        assert!(
            Instant::now() < deadline,
            "the device never closed the peer"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = b.pc.close().await;
    r.sup.stop(sid, 1, "owner".into());
}

/// A grant that may read is not one that may drive: the prompt is refused
/// on the device, and nothing reaches the harness.
#[tokio::test]
async fn a_read_only_viewer_cannot_prompt() {
    let mut r = rig(true, 4);
    let sid = session_with_a_turn(&mut r).await;
    let g = grant(sid, r.user, false, 60);
    let gid = g.grant_id;
    r.sup.view_grant(g, true).await.unwrap();
    let mut b = dial(&mut r, gid).await;

    b.send(json!({"op": "hello"})).await;
    let tip = b.recv_op("hello").await["tip"].as_u64().unwrap();
    b.send(json!({"op": "prompt", "id": "x", "text": "rm -rf"}))
        .await;
    let ack = b.recv_op("prompt").await;
    assert_eq!(ack["ok"], false);
    assert!(
        ack["error"].as_str().unwrap().starts_with("read_only"),
        "{ack}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        r.store.tip(&sid.to_hex()).await,
        Some(tip),
        "nothing reached the harness"
    );

    // The viewer closes its side: the device notices and tells the server.
    b.pc.close().await.unwrap();
    let (closed, reason) = next_closed(&mut r).await;
    assert_eq!(closed, gid);
    assert!(
        reason == "viewer_left" || reason == "peer_failed",
        "{reason}"
    );
    wait_viewers(&r, 0).await;
    r.sup.stop(sid, 1, "owner".into());
}

/// Frames a viewer sends are bounded: a message larger than a prompt may be
/// is refused at its first frame, and garbage is not a request.
#[tokio::test]
async fn a_viewer_cannot_send_unbounded_or_garbage_messages() {
    let mut r = rig(true, 4);
    let sid = session_with_a_turn(&mut r).await;
    let g = grant(sid, r.user, true, 60);
    let gid = g.grant_id;
    r.sup.view_grant(g, true).await.unwrap();
    let mut b = dial(&mut r, gid).await;

    let huge = vec![b'a'; INBOUND.max_message_bytes + framing::MAX_PAYLOAD];
    for frame in framing::encode(99, &huge) {
        b.dc.send(&Bytes::from(frame)).await.unwrap();
    }
    b.dc.send(&Bytes::from_static(b"not a frame"))
        .await
        .unwrap();
    b.send(json!("not an object")).await;
    let err = b.recv_op("error").await;
    assert!(err["error"].as_str().is_some(), "{err}");
    // Still serving after all of it.
    b.send(json!({"op": "hello"})).await;
    assert_eq!(b.recv_op("hello").await["session"], sid.to_hex());

    r.sup.viewers().command(gid, Cmd::Close("test".into()));
    wait_viewers(&r, 0).await;
    let _ = b.pc.close().await;
    r.sup.stop(sid, 1, "owner".into());
}

/// An approval over the peer (P1a): every viewer sees which are open; a
/// reader cannot answer one; a driver can, and the harness gets the answer
/// with the driver's words; then every viewer hears it is closed.
#[tokio::test]
async fn a_driver_answers_an_approval_over_the_peer_and_a_reader_cannot() {
    let mut r = rig(true, 4);
    let sid = session_with_a_turn(&mut r).await;
    r.sup.prompt(sid, dev(&r), "hold".into()).unwrap();
    let mut harness = harness_asks(&r, sid, 2, "whoami").await;
    let approval = until_awaiting(&mut r, sid).await;

    let reader = grant(sid, ObjectId::new(), false, 60);
    let rid = reader.grant_id;
    r.sup.view_grant(reader, true).await.unwrap();
    let mut reader = dial(&mut r, rid).await;
    reader.send(json!({"op": "hello"})).await;
    let hello = reader.recv_op("hello").await;
    assert_eq!(hello["approvals"], json!([approval]), "{hello}");
    assert_eq!(hello["may_answer"], false);
    assert_eq!(hello["state"], "awaiting_approval");
    reader
        .send(json!({"op": "answer", "id": "r1", "approval": approval, "decision": "allow"}))
        .await;
    let ack = reader.recv_op("answer").await;
    assert_eq!(ack["ok"], false);
    assert!(
        ack["error"].as_str().unwrap().starts_with("read_only"),
        "{ack}"
    );
    assert_eq!(
        r.sup.pending_approvals(sid),
        std::slice::from_ref(&approval),
        "a reader's answer changed nothing"
    );

    let driver = grant(sid, r.user, true, 60);
    let did = driver.grant_id;
    r.sup.view_grant(driver, true).await.unwrap();
    let mut driver = dial(&mut r, did).await;
    driver.send(json!({"op": "hello"})).await;
    assert_eq!(driver.recv_op("hello").await["may_answer"], true);
    driver
        .send(json!({"op": "answer", "id": "d0", "approval": approval, "decision": "maybe"}))
        .await;
    assert_eq!(driver.recv_op("answer").await["ok"], false);
    driver
        .send(
            json!({"op": "answer", "id": "d1", "approval": approval, "decision": "deny",
            "message": "  not on this box  "}),
        )
        .await;
    assert_eq!(
        driver.recv_op("answer").await,
        json!({"op": "answer", "id": "d1", "ok": true})
    );
    assert_eq!(
        harness.verdict(2).await,
        json!({"behavior": "deny", "message": "Viewer denied this: not on this box"})
    );
    assert_eq!(driver.recv_op("approvals").await["pending"], json!([]));
    assert_eq!(reader.recv_op("approvals").await["pending"], json!([]));
    driver
        .send(json!({"op": "answer", "id": "d2", "approval": approval, "decision": "allow"}))
        .await;
    let again = driver.recv_op("answer").await;
    assert_eq!(again["ok"], false, "answered once: {again}");

    release(&r);
    for gid in [rid, did] {
        r.sup.viewers().command(gid, Cmd::Close("test".into()));
    }
    wait_viewers(&r, 0).await;
    let _ = reader.pc.close().await;
    let _ = driver.pc.close().await;
    r.sup.stop(sid, 1, "owner".into());
}
