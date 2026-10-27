// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-127 P2 — simulcast, against a real mediasoup worker and real sockets:
//! a camera published as a ladder makes a `Simulcast` consumer whose layer
//! ceiling follows the height its viewer reports for the tile and whose
//! priority follows the tile's spotlit flag — read off the WORKER's own
//! dump, not this process's copy; a report that names the tile before the
//! consume creates the consumer at that layer; a plain stream (one encoding)
//! is never asked for layers; the switch off keeps today's shape; the pause
//! lever off keeps the layer map running; a legacy client gets the top; a
//! three-rung ladder serves the smallest tiles.
//!
//! No RTP flows here (the transports never DTLS-connect), so the worker's
//! `current` layers stay at none: what a tile actually receives is the field
//! rig's reading (the FR's §8), not this file's.

use serde_json::json;

use crate::fixtures::media_ws::*;
use crate::fixtures::test_app::TestApp;

const TWO_RUNGS: &str = r#"{"encodings":[
  {"scale_resolution_down_by":2,"max_bitrate":300000,"scalability_mode":"L1T3"},
  {"scale_resolution_down_by":1,"max_bitrate":1500000,"scalability_mode":"L1T3"}]}"#;

/// The core of P2: the ladder is announced; a view client's consumer of a
/// simulcast camera starts at the worker's top; a small tile lowers the
/// ceiling, a spotlit one raises the priority, a hidden one is paused with
/// its ceiling untouched, an unknown height takes the top — every claim
/// read off the worker's dump.
#[tokio::test]
async fn a_tile_gets_the_layer_its_height_maps_to_read_off_the_worker() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "sim1").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    let (_, transport) = join(&mut a, &room_id, None).await;
    let announced = levers(&transport);
    assert_eq!(announced["view"], true);
    assert_eq!(
        announced["simulcast"],
        serde_json::from_str::<serde_json::Value>(TWO_RUNGS).unwrap(),
        "the ladder rides the join, lowest rung first: {announced}"
    );
    let a_video = produce_with(
        &mut a,
        &room_id,
        "video",
        "camera",
        simulcast_params(&[21_001, 21_002]),
        Some((640, 480)),
    )
    .await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_video, &caps).await;
    let c = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert!(
        c.simulcast,
        "two encodings make a Simulcast consumer: {c:?}"
    );
    assert!(
        c.paused,
        "a view client that reported nothing shows nothing"
    );
    assert_eq!(
        c.preferred,
        Some((1, Some(2))),
        "created without a tile: the worker's default, the top"
    );
    assert_eq!(c.priority, 1);
    let d = dump_layers(&app, &room_id, &b.conn, &a_video).await;
    assert_eq!(d.kind_type, "simulcast");
    assert_eq!(d.preferred, (1, 2), "the worker agrees: {d:?}");
    assert_eq!(d.priority, 1);
    assert_eq!(
        d.current,
        (-1, -1),
        "nothing flows on a transport that never connects"
    );

    // A 160×120 grid tile: resumed, on the half layer at half the rate.
    view_tiles(&mut b, &room_id, 1, &[(&a_video, 160, 120, false)]).await;
    wait_for("the consumer resumes with the ceiling lowered", || {
        let c = consumer_of(&app, &room_id, &b.conn, &a_video);
        !c.paused && c.preferred == Some((0, Some(1)))
    })
    .await;
    let d = dump_layers(&app, &room_id, &b.conn, &a_video).await;
    assert_eq!(
        d.preferred,
        (0, 1),
        "the worker holds the half layer: {d:?}"
    );
    assert_eq!(d.priority, 1);
    // The gauge counts it as flowing, under "none": no RTP has arrived, so
    // the worker forwards no layer yet.
    assert_eq!(
        conf(&app).media_gauges().video_layers_total,
        [("none".to_string(), 1)].into_iter().collect()
    );

    // The same tile, spotlit and big: the top, and the priority above the rest.
    view_tiles(&mut b, &room_id, 2, &[(&a_video, 640, 480, true)]).await;
    wait_for("the top and the spotlit priority", || {
        let c = consumer_of(&app, &room_id, &b.conn, &a_video);
        c.preferred == Some((1, Some(2))) && c.priority == 2
    })
    .await;
    let d = dump_layers(&app, &room_id, &b.conn, &a_video).await;
    assert_eq!((d.preferred, d.priority), ((1, 2), 2), "{d:?}");

    // No longer spotlit, a 320 px tile: the half layer at the full rate, and
    // the priority back to the default.
    view_tiles(&mut b, &room_id, 3, &[(&a_video, 320, 320, false)]).await;
    wait_for(
        "the half layer at the full rate, the default priority",
        || {
            let c = consumer_of(&app, &room_id, &b.conn, &a_video);
            c.preferred == Some((0, Some(2))) && c.priority == 1
        },
    )
    .await;
    assert_eq!(
        dump_layers(&app, &room_id, &b.conn, &a_video)
            .await
            .preferred,
        (0, 2)
    );

    // Hidden: paused, and the ceiling is left where it was — a consumer the
    // view does not name is never tuned.
    view_tiles(&mut b, &room_id, 4, &[]).await;
    wait_for("hidden: paused", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    assert_eq!(
        consumer_of(&app, &room_id, &b.conn, &a_video).preferred,
        Some((0, Some(2))),
        "a hidden tile's ceiling is untouched"
    );
    assert!(conf(&app).media_gauges().video_layers_total.is_empty());

    // Shown again without a height (a widget measured before layout): the top.
    view_tiles(&mut b, &room_id, 5, &[(&a_video, 0, 0, false)]).await;
    wait_for("an unknown height takes the top", || {
        let c = consumer_of(&app, &room_id, &b.conn, &a_video);
        !c.paused && c.preferred == Some((1, Some(2)))
    })
    .await;
    assert_eq!(
        dump_layers(&app, &room_id, &b.conn, &a_video)
            .await
            .preferred,
        (1, 2)
    );

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// The client learns producer ids before it consumes, so a report may name
/// a tile before its consumer exists: the consumer is then CREATED at that
/// layer (`ConsumerOptions.preferred_layers`), resumed, and — spotlit —
/// prioritised, so its first key frame is the right layer's.
#[tokio::test]
async fn a_report_naming_a_small_tile_before_its_consume_creates_it_at_that_layer() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "sim2").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce_with(
        &mut a,
        &room_id,
        "video",
        "camera",
        simulcast_params(&[22_001, 22_002]),
        Some((640, 480)),
    )
    .await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    view_tiles(&mut b, &room_id, 1, &[(&a_video, 200, 100, true)]).await;
    barrier(&mut b, &room_id, &caps).await;

    consume(&mut b, &room_id, &a_video, &caps).await;
    let c = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert!(!c.paused, "named before it existed: created resumed");
    assert_eq!(
        c.preferred,
        Some((0, Some(0))),
        "created at the layer a 100 px tile maps to: {c:?}"
    );
    assert_eq!(c.priority, 2, "and spotlit from the start");
    let d = dump_layers(&app, &room_id, &b.conn, &a_video).await;
    assert_eq!(
        (d.preferred, d.priority),
        ((0, 0), 2),
        "the worker agrees: {d:?}"
    );

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// A screen share is published as one encoding, so its consumer is `Simple`
/// and has nothing to choose: no layers are ever asked of it (the worker
/// would accept the request and answer with nothing — it is not sent), its
/// priority still follows the spotlit flag, and a report that names it
/// raises no error on the socket. Audio is never touched either way.
#[tokio::test]
async fn a_plain_stream_is_simple_and_only_its_priority_moves() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "sim3").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_screen = produce_with(
        &mut a,
        &room_id,
        "video",
        "screen",
        producer_params("video", 23_001),
        None,
    )
    .await;
    let a_audio = produce(&mut a, &room_id, "audio", 23_002).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_screen, &caps).await;
    consume(&mut b, &room_id, &a_audio, &caps).await;
    let c = consumer_of(&app, &room_id, &b.conn, &a_screen);
    assert!(!c.simulcast, "one encoding: a Simple consumer: {c:?}");
    assert_eq!(c.preferred, None);

    // The share is the big tile: resumed, prioritised, still no layers.
    view_tiles(&mut b, &room_id, 1, &[(&a_screen, 1600, 900, true)]).await;
    wait_for("the share resumes with the spotlit priority", || {
        let c = consumer_of(&app, &room_id, &b.conn, &a_screen);
        !c.paused && c.priority == 2
    })
    .await;
    let heard = barrier(&mut b, &room_id, &caps).await;
    assert!(
        heard
            .iter()
            .all(|f| f["type"] != "media:error" || f["data"]["message"] == "Invalid producer_id"),
        "no error from the plain stream's report: {heard:?}"
    );
    let d = dump_layers(&app, &room_id, &b.conn, &a_screen).await;
    assert_eq!(d.kind_type, "simple");
    assert_eq!(
        d.preferred,
        (-1, -1),
        "a Simple consumer holds no layers: {d:?}"
    );
    assert_eq!(d.priority, 2);
    let au = consumer_of(&app, &room_id, &b.conn, &a_audio);
    assert!(
        !au.paused && !au.simulcast && au.preferred.is_none() && au.priority == 1,
        "{au:?}"
    );

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// `[meetings] simulcast = false`: no ladder is announced, and a camera a
/// client published as a ladder anyway is consumed at the worker's default
/// — the top — whatever its tile's size says; a spotlit tile earns nothing.
/// P1's pausing is untouched: the two levers are separate switches.
#[tokio::test]
async fn the_simulcast_switch_off_keeps_todays_shape() {
    let app = TestApp::spawn_with_settings(|s| s.meetings.simulcast = false).await;
    let (tenant, room_id) = started_call(&app, "sim4").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    let (_, transport) = join(&mut a, &room_id, None).await;
    assert_eq!(
        levers(&transport),
        json!({ "view": true, "audio_dtx": true, "simulcast": null }),
        "{transport}"
    );
    let a_video = produce_with(
        &mut a,
        &room_id,
        "video",
        "camera",
        simulcast_params(&[24_001, 24_002]),
        Some((640, 480)),
    )
    .await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    // Named small and spotlit before the consume: the lever off, the consume
    // asks for nothing.
    view_tiles(&mut b, &room_id, 1, &[(&a_video, 160, 120, true)]).await;
    barrier(&mut b, &room_id, &caps).await;
    consume(&mut b, &room_id, &a_video, &caps).await;
    let c = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert!(c.simulcast, "the worker still sees two encodings");
    assert!(!c.paused, "P1 still runs: named, so resumed");
    assert_eq!(
        c.preferred,
        Some((1, Some(2))),
        "the top, the worker's default: {c:?}"
    );
    assert_eq!(c.priority, 1);

    // A later report changes the pause flag (P1) but never the layers (P2 off).
    view_tiles(&mut b, &room_id, 2, &[]).await;
    wait_for("P1 pauses the hidden tile", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    view_tiles(&mut b, &room_id, 3, &[(&a_video, 100, 80, true)]).await;
    wait_for("P1 resumes it", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    barrier(&mut b, &room_id, &caps).await;
    let d = dump_layers(&app, &room_id, &b.conn, &a_video).await;
    assert_eq!(
        (d.preferred, d.priority),
        ((1, 2), 1),
        "nothing selected: {d:?}"
    );

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// `[meetings] visible_tiles = false` with `simulcast` on: the client is told
/// not to expect pausing but still reports (it is told the ladder), every
/// consumer is created resumed and stays so, and the layer map runs on the
/// reported sizes.
#[tokio::test]
async fn with_pausing_off_the_layer_map_still_runs() {
    let app = TestApp::spawn_with_settings(|s| s.meetings.visible_tiles = false).await;
    let (tenant, room_id) = started_call(&app, "sim5").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce_with(
        &mut a,
        &room_id,
        "video",
        "camera",
        simulcast_params(&[25_001, 25_002]),
        Some((640, 480)),
    )
    .await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut b, &room_id, Some(&["view"])).await;
    assert_eq!(levers(&transport)["view"], false);
    assert!(levers(&transport)["simulcast"].is_object(), "{transport}");
    consume(&mut b, &room_id, &a_video, &caps).await;
    let c = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert!(!c.paused, "pausing off: created resumed, no report needed");
    assert_eq!(c.preferred, Some((1, Some(2))));

    view_tiles(&mut b, &room_id, 1, &[(&a_video, 160, 120, false)]).await;
    wait_for("the ceiling follows the tile", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).preferred == Some((0, Some(1)))
    })
    .await;
    assert!(!consumer_of(&app, &room_id, &b.conn, &a_video).paused);
    // An empty report pauses nothing with the lever off.
    view_tiles(&mut b, &room_id, 2, &[]).await;
    barrier(&mut b, &room_id, &caps).await;
    let c = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert!(!c.paused, "pausing is off: {c:?}");
    assert_eq!(
        c.preferred,
        Some((0, Some(1))),
        "and a hidden tile is not re-tuned"
    );

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// A client that never declared `caps` consumes a simulcast camera at the
/// top, resumed, and a report from it is ignored — never read as a tile.
#[tokio::test]
async fn a_legacy_client_gets_the_top_and_its_reports_are_ignored() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "sim6").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce_with(
        &mut a,
        &room_id,
        "video",
        "camera",
        simulcast_params(&[26_001, 26_002]),
        Some((640, 480)),
    )
    .await;

    let mut c = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut c, &room_id, None).await;
    consume(&mut c, &room_id, &a_video, &caps).await;
    let snap = consumer_of(&app, &room_id, &c.conn, &a_video);
    assert!(snap.simulcast && !snap.paused, "{snap:?}");
    assert_eq!(snap.preferred, Some((1, Some(2))));

    view_tiles(&mut c, &room_id, 1, &[(&a_video, 100, 80, true)]).await;
    barrier(&mut c, &room_id, &caps).await;
    let d = dump_layers(&app, &room_id, &c.conn, &a_video).await;
    assert_eq!(
        (d.preferred, d.priority),
        ((1, 2), 1),
        "a legacy report moves nothing: {d:?}"
    );

    a.ws.close(None).await.ok();
    c.ws.close(None).await.ok();
}

/// `[meetings] simulcast_layers = 3`: a three-rung ladder is announced, and
/// a camera published as one serves the smallest tiles from its quarter
/// rung and the big ones from the top.
#[tokio::test]
async fn a_three_rung_ladder_serves_the_smallest_tiles() {
    let app = TestApp::spawn_with_settings(|s| s.meetings.simulcast_layers = 3).await;
    let (tenant, room_id) = started_call(&app, "sim7").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    let (_, transport) = join(&mut a, &room_id, None).await;
    let rungs = levers(&transport)["simulcast"]["encodings"].clone();
    assert_eq!(rungs.as_array().map(Vec::len), Some(3), "{transport}");
    assert_eq!(rungs[0]["scale_resolution_down_by"], 4);
    let a_video = produce_with(
        &mut a,
        &room_id,
        "video",
        "camera",
        simulcast_params(&[27_001, 27_002, 27_003]),
        Some((640, 480)),
    )
    .await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_video, &caps).await;
    assert_eq!(
        consumer_of(&app, &room_id, &b.conn, &a_video).preferred,
        Some((2, Some(2))),
        "three rungs: the top is 2"
    );
    view_tiles(&mut b, &room_id, 1, &[(&a_video, 120, 100, false)]).await;
    wait_for("the quarter rung", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).preferred == Some((0, Some(0)))
    })
    .await;
    assert_eq!(
        dump_layers(&app, &room_id, &b.conn, &a_video)
            .await
            .preferred,
        (0, 0)
    );
    view_tiles(&mut b, &room_id, 2, &[(&a_video, 640, 400, false)]).await;
    wait_for("the top", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).preferred == Some((2, Some(2)))
    })
    .await;
    assert_eq!(
        dump_layers(&app, &room_id, &b.conn, &a_video)
            .await
            .preferred,
        (2, 2)
    );

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}
