// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4042 — a viewer's hang-up of a session that runs through the cross-pod
//! relay, through real servers, real sockets and one MongoDB shared by two
//! pods (`TestApp::spawn_pair`). The device's socket is on pod 1 and the
//! viewer's on pod 2, so the session lives in pod 1's hub and every frame the
//! viewer sends for it crosses the bus (`rc.cmd`).
//!
//! * **the measure**: on one pod, the device is told of a viewer's
//!   `rc:terminate` in `same` ms. Across pods the hang-up takes one relayed
//!   frame more, and one frame takes at most `RPC_DEADLINE` on a live bus, so
//!   the bound is `same + RPC_DEADLINE`, both measured in the same run;
//! * **across pods, at the window's default of 0**: the viewer hangs up and
//!   keeps its socket open. Within the bound the device is told, pod 1 holds
//!   no session, and the row ends `controller_hangup`;
//! * **the next Connect**: the same socket's next request gets a new session;
//! * **with the window on**, the hang-up keeps today's outcome: the device is
//!   told once, pod 1 holds no session, the row ends `controller_hangup`, and
//!   the audit holds as many ends as before;
//! * **another member's hang-up** of a session that is not theirs, from a
//!   socket with its own route to pod 1, is refused as it is on one pod, and
//!   the session stays live.
//!
//! Every cell that needs two pods prints "skipping: no Redis available" and
//! returns when the harness has no Redis: there is no relay without it.

use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_core::cluster::bus::RPC_DEADLINE;
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::rc_resume_claim_tests::{
    QUIET, REMOTE_CONTROL, RESUME, WINDOW, WITHIN, Ws, audit, end_reason, ended, enroll,
    give_member, hello, kinds, next_of, one, open, pair, row, scope_all, wait_audit, wait_row,
};
use crate::rc_resume_rejoin_tests::{controller, live_session, request, send, wait_bus};

/// The viewer's hang-up, as the UI's Disconnect sends it.
fn hangup(sid: ObjectId) -> Value {
    json!({ "t": "rc:terminate", "session_id": sid.to_hex(), "reason": "controller_hangup" })
}

/// The first `rc:terminate` naming `sid` on `ws` within `within`, or `None`.
async fn terminate_for(ws: &mut Ws, sid: ObjectId, within: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + within;
    let hex = sid.to_hex();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let v = next_of(ws, "rc:terminate", left).await?;
        if v["session_id"] == hex.as_str() {
            return Some(v);
        }
    }
}

/// An ICE candidate from the device for `sid`: it reaches the session's
/// viewer, wherever that viewer's socket is, only while the session lives.
async fn device_ice(agent: &mut Ws, sid: ObjectId) {
    send(
        agent,
        json!({
            "t": "rc:ice",
            "session_id": sid.to_hex(),
            "candidate": { "candidate": "candidate:1 1 udp 1 192.0.2.1 9 typ host", "sdpMid": "0" },
        }),
    )
    .await;
}

/// Until `app` reads the device's presence record as another pod's: the
/// record a request on `app` is relayed by.
async fn seen_from(app: &TestApp, agent_id: ObjectId) {
    let redis = app
        .state
        .redis_pubsub
        .as_ref()
        .expect("a pod with a bus has Redis");
    let deadline = Instant::now() + WITHIN;
    loop {
        if let Ok(Some(_)) = redis.agent_presence_foreign(&agent_id.to_hex()).await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pod 2 never read the device's presence record as pod 1's"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The measure. One pod at the window's default of 0, the device and the
/// viewer on it, the session live: the viewer hangs up and keeps its socket
/// open. Asserts that the device is told, naming the session and
/// `controller_hangup`, and returns how long that took from the send.
async fn same_pod_hangup() -> Duration {
    let app = one(0).await;
    let seeded = app.seed_tenant("w17same").await;
    let (agent_id, token) = enroll(&app, &seeded, "w17-same").await;
    let mut agent = hello(&app, &token, &[], &[]).await;
    let (mut ctl, sid, _) =
        live_session(&app, &seeded.admin.access_token, agent_id, &mut agent).await;
    wait_row(&app, sid, "the session goes live", open).await;

    let sent = Instant::now();
    send(&mut ctl, hangup(sid)).await;
    let told = terminate_for(&mut agent, sid, WITHIN)
        .await
        .expect("on one pod the device is told of the viewer's hang-up");
    let took = sent.elapsed();
    println!("hangup same-pod ms={}", took.as_millis());
    assert_eq!(told["reason"], "controller_hangup", "{told}");
    assert!(
        app.state.fleet().rc_hub.session_snapshot(sid).is_none(),
        "the pod holds no session"
    );
    // The viewer's socket stayed open throughout: the end is the hang-up's.
    drop(ctl);
    took
}

/// The bound a hang-up across pods is held to: the measured same-pod time
/// plus the longest one relayed frame may take on a live bus.
fn bound_from(same: Duration) -> Duration {
    let bound = same + RPC_DEADLINE;
    println!(
        "hangup bound ms={} (same-pod {} + RPC_DEADLINE {})",
        bound.as_millis(),
        same.as_millis(),
        RPC_DEADLINE.as_millis()
    );
    bound
}

/// A session that runs through the relay: the device's socket on pod 1, the
/// viewer's on pod 2.
struct Relayed {
    app1: TestApp,
    app2: TestApp,
    seeded: SeededTenant,
    agent_id: ObjectId,
    agent: Ws,
    ctl: Ws,
    sid: ObjectId,
    created: Value,
}

/// Two pods, `app1` and `app2`, a device on pod 1 that advertises `rpc`, and
/// a live session from the org admin's socket on pod 2. With the window on,
/// every device is in the resume scope and the device is first announced the
/// window. `None` (after the skip line) when the harness has no Redis.
async fn relayed(app1: TestApp, app2: TestApp, window: u64, rpc: &[&str]) -> Option<Relayed> {
    if !wait_bus(&app1, &app2).await {
        eprintln!("skipping: no Redis available");
        return None;
    }
    let seeded = app1.seed_tenant("w17relay").await;
    if window > 0 {
        scope_all(&app1).await;
    }
    let (agent_id, token) = enroll(&app1, &seeded, "w17-relay").await;
    let mut agent = hello(&app1, &token, rpc, &[]).await;
    if window > 0 {
        next_of(&mut agent, "rc:agent.resume_window", WITHIN)
            .await
            .expect("pod 1 announces the window");
    }
    seen_from(&app2, agent_id).await;
    let (ctl, sid, created) =
        live_session(&app2, &seeded.admin.access_token, agent_id, &mut agent).await;
    assert!(
        app1.state.fleet().rc_hub.session_snapshot(sid).is_some(),
        "pod 1, the device's, holds the session"
    );
    assert!(
        app2.state.fleet().rc_hub.session_snapshot(sid).is_none(),
        "pod 2, the viewer's, holds none: the session runs through the relay"
    );
    wait_row(&app1, sid, "the session goes live", open).await;
    Some(Relayed {
        app1,
        app2,
        seeded,
        agent_id,
        agent,
        ctl,
        sid,
        created,
    })
}

/// AC1 — the measure on its own: one pod, the viewer hangs up with its socket
/// open, and the device is told, naming the session and `controller_hangup`.
#[tokio::test]
async fn a_viewers_hangup_on_one_pod_reaches_the_device() {
    same_pod_hangup().await;
}

/// AC1 — the RED. At the window's default of 0, a viewer's `rc:terminate` of
/// a session that runs through the relay ends it on pod 1, the pod that holds
/// it, within the bound: the device is told, naming the session and
/// `controller_hangup`; pod 1 holds no session; the row ends
/// `controller_hangup`. The viewer's socket stays open throughout, so neither
/// the socket's close nor the relay's janitor can be what ends it.
#[tokio::test]
async fn a_viewers_hangup_across_pods_reaches_the_device_at_window_zero() {
    let same = same_pod_hangup().await;
    let bound = bound_from(same);
    let (app1, app2) = pair(0).await;
    let Some(mut r) = relayed(app1, app2, 0, &[]).await else {
        return;
    };

    let sent = Instant::now();
    send(&mut r.ctl, hangup(r.sid)).await;
    let told = terminate_for(&mut r.agent, r.sid, bound).await;
    let took = sent.elapsed();
    let still_held = r
        .app1
        .state
        .fleet()
        .rc_hub
        .session_snapshot(r.sid)
        .is_some();
    let Some(told) = told else {
        panic!(
            "W17-RED the device was not told within {} ms (same-pod {} ms + RPC_DEADLINE {} ms); \
             pod 1 still holds the session: {still_held}",
            bound.as_millis(),
            same.as_millis(),
            RPC_DEADLINE.as_millis()
        );
    };
    println!("hangup cross-pod ms={}", took.as_millis());
    assert_eq!(told["reason"], "controller_hangup", "{told}");
    assert!(
        r.app1
            .state
            .fleet()
            .rc_hub
            .session_snapshot(r.sid)
            .is_none(),
        "pod 1 holds no session"
    );
    let row = wait_row(&r.app1, r.sid, "the hang-up ends the row", ended).await;
    assert_eq!(end_reason(&row), Some("controller_hangup"), "{row}");
    // The viewer's socket stayed open until here.
    drop(r.ctl);
}

/// AC1 — the next Connect, in a cell of its own so that the RED above cannot
/// hide what master answers here. After a hang-up across pods at window 0,
/// the same socket's next request gets a NEW session, and the device is asked
/// for it.
#[tokio::test]
async fn the_next_connect_after_a_hangup_across_pods_is_a_new_session() {
    let (app1, app2) = pair(0).await;
    let Some(mut r) = relayed(app1, app2, 0, &[]).await else {
        return;
    };

    send(&mut r.ctl, hangup(r.sid)).await;
    // Whatever the device is told: this cell reads only the next Connect.
    let told = terminate_for(&mut r.agent, r.sid, WITHIN).await.is_some();
    let again = request(&mut r.ctl, r.agent_id, None).await;
    let next = again["session_id"].as_str().map(|s| s.to_string());
    println!(
        "W17-RESULT next connect: answer={} same_id={} device_told_of_the_hangup={told}",
        again["t"],
        next.as_deref() == Some(r.sid.to_hex().as_str())
    );
    assert_eq!(again["t"], "rc:session.created", "{again}");
    assert_ne!(
        next.as_deref(),
        Some(r.sid.to_hex().as_str()),
        "W17-RED the next Connect got the hung-up session's id back: {again}"
    );
    next_of(&mut r.agent, "rc:request", WITHIN)
        .await
        .expect("the device is asked for the new session");
}

/// The ends of `sid` in the audit, once the first is there and the rest have
/// had [`QUIET`] to land.
async fn ends_in_audit(app: &TestApp, sid: ObjectId) -> usize {
    wait_audit(app, sid, "session_ended").await;
    tokio::time::sleep(QUIET).await;
    kinds(&audit(app, sid).await)
        .iter()
        .filter(|k| *k == "session_ended")
        .count()
}

/// AC2 — the control: with the window on, a hang-up across pods keeps
/// today's outcome. The viewer's pod writes the end to the row and publishes
/// it (`controller_terminate`), and pod 1 ends the session. Within the bound
/// the device is told, once; pod 1 holds no session; the row ends
/// `controller_hangup`; and the audit holds the end twice, as before: the
/// trail of the end the viewer's pod wrote to the row, and pod 1's own end.
#[tokio::test]
async fn with_the_window_on_a_hangup_across_pods_keeps_todays_outcome() {
    let same = same_pod_hangup().await;
    let bound = bound_from(same);
    let (app1, app2) = pair(WINDOW).await;
    let Some(mut r) = relayed(app1, app2, WINDOW, &[RESUME]).await else {
        return;
    };
    assert_eq!(
        r.created["resume_secs"], WINDOW,
        "the viewer is told the session may be held: {}",
        r.created
    );

    let sent = Instant::now();
    send(&mut r.ctl, hangup(r.sid)).await;
    let told = terminate_for(&mut r.agent, r.sid, bound)
        .await
        .unwrap_or_else(|| {
            panic!(
                "with the window on the device is told within {} ms",
                bound.as_millis()
            )
        });
    let took = sent.elapsed();
    println!("hangup cross-pod window-on ms={}", took.as_millis());
    assert_eq!(told["reason"], "controller_hangup", "{told}");
    assert!(
        terminate_for(&mut r.agent, r.sid, QUIET).await.is_none(),
        "the device is told once"
    );
    assert!(
        r.app1
            .state
            .fleet()
            .rc_hub
            .session_snapshot(r.sid)
            .is_none(),
        "pod 1 holds no session"
    );
    let row = wait_row(&r.app1, r.sid, "the hang-up ends the row", ended).await;
    assert_eq!(end_reason(&row), Some("controller_hangup"), "{row}");
    let ends = ends_in_audit(&r.app1, r.sid).await;
    println!("W17-RESULT window-on ends in the audit: {ends}");
    assert_eq!(ends, 2, "the row's trail and pod 1's end, as before");
    assert!(
        r.app2
            .state
            .fleet()
            .rc_hub
            .session_snapshot(r.sid)
            .is_none(),
        "and pod 2 holds nothing of it"
    );
    drop(r.ctl);
}

/// AC3 — the party check holds across pods. Another member of the org, with
/// a session of their own to the device through the relay (so their socket
/// has its own route to pod 1), sends `rc:terminate` for the admin's session.
/// On one pod that is refused, and it ends nothing. Across pods it is refused
/// in the same words, and the admin's session stays live: the device is not
/// told, pod 1 still holds it, and a frame from the device still reaches the
/// admin's viewer.
#[tokio::test]
async fn another_members_hangup_across_pods_is_refused_as_on_one_pod() {
    // No org cap: the member's own session and the admin's run at once.
    let (app1, app2) = TestApp::spawn_pair(|s| {
        s.rc.session_resume_secs = 0;
        s.rc.org_session_cap = false;
    })
    .await;
    let Some(mut r) = relayed(app1, app2, 0, &[]).await else {
        return;
    };
    give_member(&r.app1, &r.seeded, "w17 viewer", REMOTE_CONTROL).await;
    let member = r.seeded.member.access_token.clone();

    // One pod: the member's socket on pod 1, the pod that holds the session.
    let mut here = controller(&r.app1, &member).await;
    send(&mut here, hangup(r.sid)).await;
    let same = next_of(&mut here, "rc:error", WITHIN)
        .await
        .expect("on one pod another member's hang-up is refused");
    assert!(
        r.app1
            .state
            .fleet()
            .rc_hub
            .session_snapshot(r.sid)
            .is_some(),
        "and it ends nothing"
    );

    // Across pods: the member's own socket on pod 2, with its own route to
    // pod 1 through its own session there.
    let mut there = controller(&r.app2, &member).await;
    let own = request(&mut there, r.agent_id, None).await;
    assert_eq!(
        own["t"], "rc:session.created",
        "the member's own session, through the relay: {own}"
    );
    send(&mut there, hangup(r.sid)).await;
    let refused = next_of(&mut there, "rc:error", WITHIN).await;

    // The admin's session is live: read before the refusal is judged.
    let held = r
        .app1
        .state
        .fleet()
        .rc_hub
        .session_snapshot(r.sid)
        .is_some();
    let told = terminate_for(&mut r.agent, r.sid, QUIET).await;
    device_ice(&mut r.agent, r.sid).await;
    let ice = next_of(&mut r.ctl, "rc:ice", WITHIN).await;
    println!(
        "W17-RESULT another member's hang-up: same-pod={same} cross-pod={refused:?} held={held} \
         device_told={} viewer_got_ice={}",
        told.is_some(),
        ice.is_some()
    );
    assert!(held, "pod 1 still holds the admin's session");
    assert!(told.is_none(), "the device is not told: {told:?}");
    let ice = ice.expect("a frame from the device still reaches the admin's viewer");
    assert_eq!(ice["session_id"], r.sid.to_hex().as_str(), "{ice}");
    assert!(open(&row(&r.app1, r.sid).await), "the row is still live");

    let refused = refused.expect("W17-RED another member's hang-up across pods is not answered");
    assert_eq!(refused, same, "refused in the same words as on one pod");
}
