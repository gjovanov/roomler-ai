// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-127 P1 — receive what is visible, against a real mediasoup worker and
//! real sockets: a client that declares `caps: ["view"]` has its video
//! consumers created paused and resumed by the `media:view` reports that
//! name them, audio always flows, a legacy client receives everything, the
//! kill switch keeps today's shape, an older report is dropped, a report may
//! name a producer before its consumer exists, the server's cap truncates a
//! long report, a report sent through the other pod reaches the owner's
//! consumers, and — AC2 — an owner's FR-30 pause and a viewer's off-screen
//! pause never merge.
//!
//! The consumers' flags are read straight off the owner pod's room manager
//! (`consumer_states` / `producer_states`, plain data). `media:view` has no
//! reply, so a state that must CHANGE is polled for, and a state that must
//! NOT change is read after a `barrier`: a `media:consume` with an invalid
//! producer id, whose `media:error` reply proves every earlier frame on that
//! socket was handled (the WS loop awaits each message in turn). The socket
//! helpers live in `fixtures::media_ws`, shared with P2's simulcast tests.

use bson::oid::ObjectId;
use serde_json::json;

use crate::fixtures::media_ws::*;
use crate::fixtures::test_app::TestApp;
use roomler_ai_api::cluster::directory::{ClaimOutcome, media_key};
use roomler_ai_mod_conference::media_cluster::forward_media_cmd;

/// The core of P1: a view client's video consumer is created paused and
/// follows its reports; its audio consumer always flows; an older report is
/// dropped; the same report again is applied, idempotently; the gauge counts
/// what is held back.
#[tokio::test]
async fn a_view_client_receives_only_the_video_it_shows() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view1").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut b, &room_id, Some(&["view"])).await;
    let announced = levers(&transport);
    assert_eq!(
        announced["view"], true,
        "the server announces its levers: {transport}"
    );
    assert_eq!(announced["audio_dtx"], true);
    assert!(
        announced["simulcast"].is_object(),
        "P2's ladder rides the same object (its shape is P2's test): {announced}"
    );

    let a_video = produce(&mut a, &room_id, "video", 11_001).await;
    let a_audio = produce(&mut a, &room_id, "audio", 11_002).await;

    // Created paused: a view client that has reported nothing shows nothing.
    consume(&mut b, &room_id, &a_video, &caps).await;
    let v = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert_eq!(v.kind, "video");
    assert!(v.paused, "a video consumer of a view client starts paused");
    assert!(!v.producer_paused, "the owner has not paused anything");
    consume(&mut b, &room_id, &a_audio, &caps).await;
    let au = consumer_of(&app, &room_id, &b.conn, &a_audio);
    assert_eq!(au.kind, "audio");
    assert!(!au.paused, "audio always flows");
    assert_eq!(
        conf(&app).media_gauges().video_paused_total,
        1,
        "the gauge counts the held-back video consumer"
    );

    // The report names it: resumed.
    view(&mut b, &room_id, 1, &[&a_video]).await;
    wait_for("the named video consumer resumes", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    assert_eq!(conf(&app).media_gauges().video_paused_total, 0);

    // An empty report: paused again; audio untouched.
    view(&mut b, &room_id, 2, &[]).await;
    wait_for("an empty view pauses the video consumer", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    assert!(!consumer_of(&app, &room_id, &b.conn, &a_audio).paused);

    // A report older than the one held arrives late: dropped.
    view(&mut b, &room_id, 1, &[&a_video]).await;
    barrier(&mut b, &room_id, &caps).await;
    assert!(
        consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "seq 1 after seq 2 is stale and changes nothing"
    );

    // The report held, sent again: applied — the set is idempotent.
    view(&mut b, &room_id, 2, &[&a_video]).await;
    wait_for("a re-sent report with the held seq is applied", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// A client that never declared `caps` is a legacy client: every consumer
/// is resumed at creation, and a `media:view` from it is ignored — never
/// read as "show nothing".
#[tokio::test]
async fn a_legacy_client_receives_everything_and_its_views_are_ignored() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view2").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 12_001).await;

    let mut c = connect(&app, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut c, &room_id, None).await;
    assert_eq!(
        levers(&transport)["view"],
        true,
        "the lever is announced to every client; the legacy one just never asked"
    );
    consume(&mut c, &room_id, &a_video, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &c.conn, &a_video).paused,
        "a legacy client's video consumer is resumed at creation"
    );

    view(&mut c, &room_id, 1, &[]).await;
    barrier(&mut c, &room_id, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &c.conn, &a_video).paused,
        "an empty view from a legacy client is ignored, not read as Some(empty)"
    );

    a.ws.close(None).await.ok();
    c.ws.close(None).await.ok();
}

/// `[meetings] visible_tiles = false`, `audio_dtx = false` and (P2)
/// `simulcast = false`: the client's `caps` and reports are ignored, every
/// consumer is resumed at creation, and the client is told every lever is
/// off — today's shape, exactly.
#[tokio::test]
async fn the_kill_switch_keeps_todays_shape() {
    let app = TestApp::spawn_with_settings(|s| {
        s.meetings.visible_tiles = false;
        s.meetings.audio_dtx = false;
        s.meetings.simulcast = false;
    })
    .await;
    let (tenant, room_id) = started_call(&app, "view3").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 13_001).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut b, &room_id, Some(&["view"])).await;
    assert_eq!(
        levers(&transport),
        json!({ "view": false, "audio_dtx": false, "simulcast": null })
    );
    consume(&mut b, &room_id, &a_video, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "with the lever off a view client's consumer is resumed at creation"
    );
    view(&mut b, &room_id, 1, &[]).await;
    barrier(&mut b, &room_id, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "with the lever off a report changes nothing"
    );
    assert_eq!(conf(&app).media_gauges().video_paused_total, 0);

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// AC2 — the owner's pause (FR-30, `Producer::pause`, seen by every viewer
/// as `producer_paused`) and the viewer's pause (`Consumer::pause`, the
/// viewer's own `paused`) are two flags: a viewer's report never moves the
/// owner's, the owner's toggle never moves a viewer's, and a viewer's report
/// fans no `media:producer_paused` out to anyone.
#[tokio::test]
async fn an_owners_pause_and_a_viewers_pause_never_merge() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view4").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 14_001).await;

    // B is a view client, C a legacy one (the admin's second browser).
    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps_b, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_video, &caps_b).await;
    let mut c = connect(&app, &tenant.admin.access_token).await;
    let (caps_c, _) = join(&mut c, &room_id, None).await;
    consume(&mut c, &room_id, &a_video, &caps_c).await;
    assert!(consumer_of(&app, &room_id, &b.conn, &a_video).paused);
    assert!(!consumer_of(&app, &room_id, &c.conn, &a_video).paused);

    // The owner pauses: both viewers see producer_paused, and both hear it.
    producer_pause(&mut a, &room_id, &a_video, true).await;
    wait_for("the owner's producer is paused", || {
        producer_paused(&app, &room_id, &a.conn, &a_video)
    })
    .await;
    wait_for("both viewers' consumers mirror the owner's pause", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).producer_paused
            && consumer_of(&app, &room_id, &c.conn, &a_video).producer_paused
    })
    .await;
    for (name, peer) in [("B", &mut b), ("C", &mut c)] {
        let ev = next_of(peer, "media:producer_paused").await;
        assert_eq!(ev["data"]["producer_id"], a_video, "{name}: {ev}");
        assert_eq!(ev["data"]["paused"], true, "{name}: {ev}");
    }

    // B's report names the producer: B's own flag resumes, the owner's pause
    // stays — on the producer, on B's consumer and on C's — and C hears
    // nothing: a viewer's view is nobody else's business.
    view(&mut b, &room_id, 1, &[&a_video]).await;
    wait_for("B's own flag resumes", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    let vb = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert!(
        vb.producer_paused,
        "the owner's pause survives the viewer's view"
    );
    assert!(producer_paused(&app, &room_id, &a.conn, &a_video));
    let vc = consumer_of(&app, &room_id, &c.conn, &a_video);
    assert!(!vc.paused && vc.producer_paused, "C is untouched: {vc:?}");
    let heard = barrier(&mut c, &room_id, &caps_c).await;
    assert!(
        heard.iter().all(|f| f["type"] != "media:producer_paused"),
        "a viewer's report fans nothing out: {heard:?}"
    );

    // B hides the tile again: B's flag only.
    view(&mut b, &room_id, 2, &[]).await;
    wait_for("B's own flag pauses", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    assert!(producer_paused(&app, &room_id, &a.conn, &a_video));
    assert!(!consumer_of(&app, &room_id, &c.conn, &a_video).paused);

    // The owner resumes: the owner's flag clears everywhere, the viewers'
    // own flags stay where their views left them.
    producer_pause(&mut a, &room_id, &a_video, false).await;
    wait_for("the owner's resume reaches both consumers", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).producer_paused
            && !consumer_of(&app, &room_id, &c.conn, &a_video).producer_paused
    })
    .await;
    assert!(
        consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "B still shows nothing, so B's own flag stays paused"
    );
    assert!(!consumer_of(&app, &room_id, &c.conn, &a_video).paused);

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
    c.ws.close(None).await.ok();
}

/// The client learns producer ids from `media:new_producer` before it
/// consumes, so a report may name a producer whose consumer does not exist
/// yet: the set is kept and the consumer is created resumed.
#[tokio::test]
async fn a_report_naming_a_producer_before_its_consumer_applies_at_consume() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view5").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 15_001).await;
    let a_audio = produce(&mut a, &room_id, "audio", 15_002).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    view(&mut b, &room_id, 1, &[&a_video]).await;
    barrier(&mut b, &room_id, &caps).await;

    consume(&mut b, &room_id, &a_video, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "named before it existed: created resumed"
    );
    consume(&mut b, &room_id, &a_audio, &caps).await;
    assert!(!consumer_of(&app, &room_id, &b.conn, &a_audio).paused);

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// `[meetings] view_max` truncates a report in the order sent.
#[tokio::test]
async fn a_long_report_is_truncated_at_view_max() {
    let app = TestApp::spawn_with_settings(|s| s.meetings.view_max = 1).await;
    let (tenant, room_id) = started_call(&app, "view6").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 16_001).await;
    let mut d = connect(&app, &tenant.admin.access_token).await;
    join(&mut d, &room_id, None).await;
    let d_video = produce(&mut d, &room_id, "video", 16_002).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_video, &caps).await;
    consume(&mut b, &room_id, &d_video, &caps).await;

    view(&mut b, &room_id, 1, &[&d_video, &a_video]).await;
    wait_for("the first tile of the report resumes", || {
        !consumer_of(&app, &room_id, &b.conn, &d_video).paused
    })
    .await;
    barrier(&mut b, &room_id, &caps).await;
    assert!(
        consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "the second tile is past the cap and stays paused"
    );

    a.ws.close(None).await.ok();
    d.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// A producer its owner closed leaves a dead consumer handle in every other
/// participant (`close_producer` drops only the owner's entry). A report must
/// not plan a pause or resume on it — every report would then ask the worker
/// to touch a closed consumer, for the participant's lifetime — and the gauge
/// must not count it.
#[tokio::test]
async fn a_closed_producers_consumer_is_dropped_from_the_plan_and_the_gauge() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view8").await;
    let rid = oid(&room_id);

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 18_001).await;
    let a_audio = produce(&mut a, &room_id, "audio", 18_002).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_video, &caps).await;
    consume(&mut b, &room_id, &a_audio, &caps).await;
    // B shows nothing: its video consumer is paused, and the gauge counts it.
    assert!(consumer_of(&app, &room_id, &b.conn, &a_video).paused);
    assert_eq!(conf(&app).media_gauges().video_paused_total, 1);
    assert_eq!(conf(&app).room_manager.consumer_handles(&rid, &b.conn), 2);

    // A closes its camera: the worker closes B's consumer of it.
    send(
        &mut a,
        "media:producer_close",
        json!({ "room_id": room_id, "producer_id": a_video }),
    )
    .await;
    let closed = next_of(&mut b, "media:producer_closed").await;
    assert_eq!(closed["data"]["producer_id"], a_video);
    wait_for("the worker marks B's consumer closed", || {
        !conf(&app)
            .room_manager
            .consumer_states(&rid, &b.conn)
            .iter()
            .any(|c| c.producer_id == a_video)
    })
    .await;
    // The handle lingers until a report, and the gauge no longer counts it.
    assert_eq!(conf(&app).room_manager.consumer_handles(&rid, &b.conn), 2);
    assert_eq!(conf(&app).media_gauges().video_paused_total, 0);

    // A report drops the dead handle instead of planning on it; the live
    // audio consumer is untouched.
    view(&mut b, &room_id, 1, &[]).await;
    barrier(&mut b, &room_id, &caps).await;
    assert_eq!(
        conf(&app).room_manager.consumer_handles(&rid, &b.conn),
        1,
        "only the audio consumer's handle remains"
    );
    assert!(!consumer_of(&app, &room_id, &b.conn, &a_audio).paused);
    assert_eq!(conf(&app).media_gauges().video_paused_total, 0);

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// A forwarded `media:view` the bus cannot deliver (a deadline, a Redis error)
/// is best-effort: the next report heals it, so it neither tells the viewer
/// the conference broke nor prunes the owner's claim — which every other
/// command's failure does (the presumed-dead-owner rule), and which a viewer
/// scrolling on the non-owner pod would otherwise trip on every report.
#[tokio::test]
async fn a_failed_forwarded_view_does_not_prune_the_owners_claim() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let Some(dir) = app2.state.cluster_directory.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let _ = &app1;
    let uid = ObjectId::new();
    let rid = ObjectId::new();
    let key = media_key(&rid.to_hex());
    // An owner that will never answer: a record in the directory's own
    // `pod/epoch/extra/since` shape, naming a pod that does not exist. The
    // forward then runs into the bus deadline, today's "presumed dead" case.
    let ghost = || {
        format!(
            "ghost-pod/0/media/{}",
            bson::DateTime::now().timestamp_millis()
        )
    };
    let planted = |outcome: ClaimOutcome| assert!(matches!(outcome, ClaimOutcome::Won));

    // Positive control — the rule for every other command: a deadline toward
    // the presumed owner prunes its record.
    planted(dir.claim_nx(&key, &ghost(), 30).await.unwrap());
    let consume = json!({ "room_id": rid.to_hex(), "producer_id": "x", "rtp_capabilities": {} });
    assert!(
        !forward_media_cmd(
            conf(&app2),
            "ghost-pod",
            &uid,
            "conn-x",
            "media:consume",
            Some(&consume)
        )
        .await
    );
    assert_eq!(
        dir.get(&key).await.unwrap(),
        None,
        "a failed consume prunes the presumed-dead owner's record"
    );

    // The view: the same failure, the record stays.
    planted(dir.claim_nx(&key, &ghost(), 30).await.unwrap());
    let report = json!({ "room_id": rid.to_hex(), "seq": 1, "video": [] });
    assert!(
        !forward_media_cmd(
            conf(&app2),
            "ghost-pod",
            &uid,
            "conn-x",
            "media:view",
            Some(&report)
        )
        .await
    );
    let kept = dir.get(&key).await.unwrap();
    assert!(
        kept.as_deref().is_some_and(|r| r.starts_with("ghost-pod/")),
        "a failed view prunes nothing: {kept:?}"
    );
    let _ = dir.release(&key, kept.as_deref().unwrap_or("")).await;
}

/// Multi-pod (C-4): a viewer whose socket is on the non-owner pod sends
/// `media:view` like any other command — it is forwarded over `media.cmd`
/// and applied to the owner pod's `ParticipantMedia`.
#[tokio::test]
async fn a_view_sent_through_the_other_pod_reaches_the_owners_consumers() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (tenant, room_id) = started_call(&app1, "view7").await;
    let rid = oid(&room_id);
    assert!(conf(&app1).room_manager.has_room(&rid));
    assert!(!conf(&app2).room_manager.has_room(&rid));

    // The producer on the owner pod.
    let mut a = connect(&app1, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 17_001).await;

    // The viewer through the other pod: join, consume and view all forwarded.
    let mut b = connect(&app2, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut b, &room_id, Some(&["view"])).await;
    assert_eq!(levers(&transport)["view"], true, "{transport}");
    assert!(
        !conf(&app2).room_manager.has_room(&rid),
        "no second island: the owner served the join"
    );
    consume(&mut b, &room_id, &a_video, &caps).await;
    assert!(
        consumer_of(&app1, &room_id, &b.conn, &a_video).paused,
        "the owner pod holds B's consumer, created paused"
    );

    view(&mut b, &room_id, 1, &[&a_video]).await;
    wait_for(
        "the forwarded view resumes the consumer on the owner pod",
        || !consumer_of(&app1, &room_id, &b.conn, &a_video).paused,
    )
    .await;
    view(&mut b, &room_id, 2, &[]).await;
    wait_for("the forwarded empty view pauses it again", || {
        consumer_of(&app1, &room_id, &b.conn, &a_video).paused
    })
    .await;

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}
