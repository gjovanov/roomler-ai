// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-157 P2 (AC6) — the server's read of a Mac's screen-worker word
//! (`capabilities.gui_worker`): the `gui_workers` counts in the org overview
//! and the platform overview, and the device list's `?gui_worker=` filter,
//! all behind `rc.gui_worker_read`. With the switch off, the default, none of
//! it shows: no `gui_workers` key, no word in a device row, and the filter is
//! refused with the switch's name.
//!
//! A row is written the way a device's hello writes it
//! (`AgentDao::update_hello`), and its presence is the ledger the overviews
//! count (`last_presence`), so an offline row's last word is never read as
//! current.

use bson::{Bson, Document, doc, oid::ObjectId};
use roomler_ai_remote_control::models::{AgentCaps, GuiWorkerBrief};
use roomler_ai_services::dao::agent::AgentDao;
use serde_json::{Value, json};

use crate::fixtures::test_app::TestApp;

fn word(state: &str, reason: Option<&str>, one_row: bool) -> Option<GuiWorkerBrief> {
    Some(GuiWorkerBrief {
        state: state.into(),
        reason: reason.map(Into::into),
        one_row,
    })
}

/// One device row: enrolled, its caps stored by a hello, its presence as the
/// presence ledger has it, and tombstoned when `removed`.
async fn device(
    app: &TestApp,
    tenant: ObjectId,
    name: &str,
    os: &str,
    presence: &str,
    gui_worker: Option<GuiWorkerBrief>,
    removed: bool,
) -> ObjectId {
    let agents = app.state.db.collection::<Document>("agents");
    let id = ObjectId::new();
    let now = bson::DateTime::now();
    agents
        .insert_one(doc! {
            "_id": id, "tenant_id": tenant, "owner_user_id": ObjectId::new(),
            "name": name,
            // The unique {tenant_id, machine_id} index: every row its own.
            "machine_id": format!("machine-{name}"),
            "os": os, "agent_version": "0.4.240", "agent_token_hash": "",
            "status": "offline", "last_seen_at": now,
            "created_at": now, "updated_at": now, "deleted_at": Bson::Null,
        })
        .await
        .unwrap();
    let caps = AgentCaps {
        codecs: vec!["h264".into()],
        gui_worker,
        ..Default::default()
    };
    let wrote = AgentDao::new(&app.db)
        .update_hello(
            id,
            "0.4.240",
            &[],
            &caps,
            &[],
            "",
            "",
            "",
            false,
            &format!("conn-{name}"),
        )
        .await
        .unwrap();
    assert!(wrote, "the hello stored {name}'s caps");
    let status = if presence == "online" {
        "online"
    } else {
        "offline"
    };
    let mut set = doc! { "last_presence": presence, "status": status };
    if removed {
        set.insert("deleted_at", bson::DateTime::now());
    }
    agents
        .update_one(doc! { "_id": id }, doc! { "$set": set })
        .await
        .unwrap();
    id
}

async fn get_json(app: &TestApp, path: &str, token: &str) -> Value {
    let r = app.auth_get(path, token).send().await.unwrap();
    let status = r.status().as_u16();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 200, "{path}: {body}");
    body
}

/// The device names of a list page, sorted.
fn names_of(list: &Value) -> Vec<String> {
    let mut names: Vec<String> = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|row| row["name"].as_str().expect("a name").to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn with_the_read_on_the_overviews_count_the_macs_and_the_filter_names_the_online_ones() {
    let admin_id = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
        s.rc.gui_worker_read = true;
    })
    .await;
    let org = app.seed_tenant("guiworker").await;
    let other = app.seed_tenant("guiworker2").await;
    let tid = ObjectId::parse_str(&org.tenant_id).unwrap();
    let other_tid = ObjectId::parse_str(&other.tenant_id).unwrap();
    let gave_up = || word("not-attached", Some("gave-up"), true);
    let rows = [
        // (org, name, os, presence, word, removed)
        // #3802's state: one row, a person signed in, no worker.
        (tid, "mac-gave-up", "macos", "online", gave_up(), false),
        // Its last word is still stored, and is not current: total only.
        (
            tid,
            "mac-gave-up-earlier",
            "macos",
            "offline",
            gave_up(),
            false,
        ),
        (
            tid,
            "mac-one-row",
            "macos",
            "online",
            word("attached", None, true),
            false,
        ),
        (
            tid,
            "mac-two-row",
            "macos",
            "online",
            word("unsupervised", None, false),
            false,
        ),
        (
            tid,
            "mac-login-window",
            "macos",
            "online",
            word("not-attached", Some("no-console-user"), true),
            false,
        ),
        // An agent older than P1 sends no word: `unknown`.
        (tid, "mac-older-agent", "macos", "online", None, false),
        (tid, "mac-older-agent-off", "macos", "offline", None, false),
        // Not a Mac: never counted, never named.
        (tid, "linux-box", "linux", "online", None, false),
        // Removed: never counted, never named.
        (tid, "mac-removed", "macos", "online", gave_up(), true),
        // Another org: the platform overview's only.
        (
            other_tid,
            "mac-elsewhere",
            "macos",
            "online",
            gave_up(),
            false,
        ),
    ];
    for (tenant, name, os, presence, gui_worker, removed) in rows {
        device(&app, tenant, name, os, presence, gui_worker, removed).await;
    }

    // The org overview: the org's live Macs by state, reason and one_row,
    // online and in total, a Mac with no word as `unknown`.
    let overview = get_json(
        &app,
        &format!("/api/tenant/{}/stats/overview", org.tenant_id),
        &org.member.access_token,
    )
    .await;
    assert_eq!(
        overview["gui_workers"],
        json!([
            { "state": "attached", "reason": null, "one_row": true, "total": 1, "online": 1 },
            { "state": "not-attached", "reason": "gave-up", "one_row": true, "total": 2, "online": 1 },
            { "state": "not-attached", "reason": "no-console-user", "one_row": true, "total": 1, "online": 1 },
            { "state": "unknown", "reason": null, "one_row": null, "total": 2, "online": 1 },
            { "state": "unsupervised", "reason": null, "one_row": false, "total": 1, "online": 1 },
        ]),
        "{overview}"
    );
    let counts = overview["gui_workers"].as_array().unwrap().clone();
    let sum = |key: &str| counts.iter().map(|c| c[key].as_u64().unwrap()).sum::<u64>();
    assert_eq!(
        sum("total"),
        7,
        "every live Mac of the org, and no other row"
    );
    assert_eq!(sum("online"), 5, "every online live Mac of the org");

    // The platform overview: both orgs' Macs, never the removed one.
    let tokens = app
        .state
        .auth
        .generate_tokens(admin_id, "padmin@test.io", "padmin")
        .unwrap();
    let platform = get_json(&app, "/api/admin/stats/orgs", &tokens.access_token).await;
    assert_eq!(
        platform["gui_workers"],
        json!([
            { "state": "attached", "reason": null, "one_row": true, "total": 1, "online": 1 },
            { "state": "not-attached", "reason": "gave-up", "one_row": true, "total": 3, "online": 2 },
            { "state": "not-attached", "reason": "no-console-user", "one_row": true, "total": 1, "online": 1 },
            { "state": "unknown", "reason": null, "one_row": null, "total": 2, "online": 1 },
            { "state": "unsupervised", "reason": null, "one_row": false, "total": 1, "online": 1 },
        ]),
        "{platform}"
    );

    // The filter: exactly the org's ONLINE Macs in the state asked for, as
    // many as the overview counts online in that state.
    let online_in = |state: &str| {
        counts
            .iter()
            .filter(|c| c["state"] == state)
            .map(|c| c["online"].as_u64().unwrap())
            .sum::<u64>()
    };
    for (state, names) in [
        ("not-attached", vec!["mac-gave-up", "mac-login-window"]),
        ("attached", vec!["mac-one-row"]),
        ("unsupervised", vec!["mac-two-row"]),
        ("unknown", vec!["mac-older-agent"]),
        // A word no device sends names no row, never the unfiltered list.
        ("paused", vec![]),
    ] {
        let list = get_json(
            &app,
            &format!("/api/tenant/{}/agent?gui_worker={state}", org.tenant_id),
            &org.member.access_token,
        )
        .await;
        assert_eq!(names_of(&list), names, "{state}: {list}");
        assert_eq!(list["total"], json!(names.len()), "{state}: {list}");
        assert_eq!(
            list["total"].as_u64().unwrap(),
            online_in(state),
            "the filter names what the overview counts online: {state}"
        );
    }

    // The rows carry the word as the device sent it.
    let list = get_json(
        &app,
        &format!(
            "/api/tenant/{}/agent?gui_worker=not-attached",
            org.tenant_id
        ),
        &org.member.access_token,
    )
    .await;
    let row = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "mac-gave-up")
        .expect("the #3802 row")
        .clone();
    assert_eq!(
        row["capabilities"]["gui_worker"],
        json!({ "state": "not-attached", "reason": "gave-up", "one_row": true }),
        "{row}"
    );

    // Another org's filter names its own Macs only.
    let theirs = get_json(
        &app,
        &format!(
            "/api/tenant/{}/agent?gui_worker=not-attached",
            other.tenant_id
        ),
        &other.member.access_token,
    )
    .await;
    assert_eq!(names_of(&theirs), ["mac-elsewhere"], "{theirs}");

    // The filter rides the list's tenant guard: a member of another org is
    // refused before any filter is read.
    let r = app
        .auth_get(
            &format!(
                "/api/tenant/{}/agent?gui_worker=not-attached",
                org.tenant_id
            ),
            &other.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "not_a_member", "{body}");

    // A filter that names no state is refused, never read as no filter.
    let r = app
        .auth_get(
            &format!("/api/tenant/{}/agent?gui_worker=", org.tenant_id),
            &org.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
}

#[tokio::test]
async fn with_the_read_off_no_response_carries_the_word_and_the_filter_is_refused() {
    let admin_id = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
    })
    .await;
    assert!(
        !app.settings.rc.gui_worker_read,
        "the switch is off by default"
    );
    let org = app.seed_tenant("guiworkeroff").await;
    let tid = ObjectId::parse_str(&org.tenant_id).unwrap();
    let mac = device(
        &app,
        tid,
        "mac-gave-up",
        "macos",
        "online",
        word("not-attached", Some("gave-up"), true),
        false,
    )
    .await;
    device(&app, tid, "mac-older-agent", "macos", "online", None, false).await;

    // The word is stored with the caps all the same: the switch stops reads.
    let stored = app
        .state
        .db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": mac })
        .await
        .unwrap()
        .expect("the row");
    let stored_word = stored
        .get_document("capabilities")
        .and_then(|c| c.get_document("gui_worker"))
        .expect("the stored word");
    assert_eq!(stored_word.get_str("state").unwrap(), "not-attached");

    // Neither overview carries `gui_workers`; the rest of each is as before.
    let overview = get_json(
        &app,
        &format!("/api/tenant/{}/stats/overview", org.tenant_id),
        &org.member.access_token,
    )
    .await;
    assert_eq!(overview["enabled"], true, "{overview}");
    assert!(overview.get("gui_workers").is_none(), "{overview}");
    assert!(overview.get("desktops").is_some(), "{overview}");
    let tokens = app
        .state
        .auth
        .generate_tokens(admin_id, "padmin@test.io", "padmin")
        .unwrap();
    let platform = get_json(&app, "/api/admin/stats/orgs", &tokens.access_token).await;
    assert_eq!(platform["enabled"], true, "{platform}");
    assert!(platform.get("gui_workers").is_none(), "{platform}");
    assert!(platform.get("desktops").is_some(), "{platform}");

    // No row of the device list carries the word, nor the device's own read.
    let list = get_json(
        &app,
        &format!("/api/tenant/{}/agent", org.tenant_id),
        &org.member.access_token,
    )
    .await;
    assert_eq!(
        names_of(&list),
        ["mac-gave-up", "mac-older-agent"],
        "{list}"
    );
    for row in list["items"].as_array().unwrap() {
        assert!(row["capabilities"].is_object(), "{row}");
        assert!(row["capabilities"].get("gui_worker").is_none(), "{row}");
    }
    let one = get_json(
        &app,
        &format!("/api/tenant/{}/agent/{}", org.tenant_id, mac.to_hex()),
        &org.member.access_token,
    )
    .await;
    assert!(one["capabilities"].is_object(), "{one}");
    assert!(one["capabilities"].get("gui_worker").is_none(), "{one}");

    // The filter is refused with the switch's name, never answered with the
    // unfiltered list.
    let r = app
        .auth_get(
            &format!(
                "/api/tenant/{}/agent?gui_worker=not-attached",
                org.tenant_id
            ),
            &org.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "bad_request", "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("ROOMLER__RC__GUI_WORKER_READ")),
        "{body}"
    );
}
