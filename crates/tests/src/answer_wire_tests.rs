// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4189 — four answers that sent a stored id or time as bson's extended JSON
//! (`{"$oid": …}`, `{"$date": …}`) where the web app reads a 24-hex string or
//! an RFC 3339 time, read here through the real routes:
//!
//! - a device's `access_policy` (`GET …/agent/{aid}`), and the save that sends
//!   the answer back (`PUT …/agent/{aid}`);
//! - a rotated connect code's `rotated_at` (`POST …/agent/{aid}/connect-code`);
//! - the platform orgs rollup's `tenants[].created` (`GET /api/admin/stats/orgs`);
//! - the mesh's `nodes[]` (`GET …/stats/mesh`), whose unread `last_seen_at` is
//!   no longer sent.
//!
//! The other two answers of #4189 are built by pure code and tested where they
//! are built: a log batch's lines (fleet's `agent_log::tests`) and a session's
//! audit trail (remote's `routes::tests`).

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::agent_presence_tests::enroll;
use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::tunnel_policy_permission_tests::call;

fn tenant_url(seeded: &SeededTenant, tail: &str) -> String {
    format!("/api/tenant/{}{tail}", seeded.tenant_id)
}

fn is_hex24(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Every place `answer` still carries bson's extended JSON.
fn raw_bson(what: &str, answer: &Value) -> Vec<String> {
    let text = answer.to_string();
    ["$oid", "$date"]
        .into_iter()
        .filter(|raw| text.contains(raw))
        .map(|raw| format!("{what} carries `{raw}`: {text}"))
        .collect()
}

/// What is wrong with `got` as a list of ids: anything but the 24-hex
/// strings of `want`, in order.
fn ids_problem(what: &str, got: &Value, want: &[&str]) -> Option<String> {
    let Some(got) = got.as_array() else {
        return Some(format!("{what} is not an array: {got}"));
    };
    let strings: Vec<Option<&str>> = got.iter().map(Value::as_str).collect();
    if strings.iter().any(|s| !s.is_some_and(is_hex24)) {
        return Some(format!(
            "{what} holds an id that is not a 24-hex string: {}",
            Value::from(got.clone())
        ));
    }
    let strings: Vec<&str> = strings.into_iter().flatten().collect();
    (strings != want).then(|| format!("{what} is {strings:?}, want {want:?}"))
}

/// What is wrong with `got` as an RFC 3339 time of the instant `want`.
fn time_problem(what: &str, got: &Value, want: DateTime) -> Option<String> {
    let Some(s) = got.as_str() else {
        return Some(format!("{what} is not a string: {got}"));
    };
    match DateTime::parse_rfc3339_str(s) {
        Err(_) => Some(format!("{what} is not RFC 3339: {s}")),
        Ok(back) if back != want => Some(format!("{what} {s} is not the stored {want}")),
        Ok(_) => None,
    }
}

/// The stored id list `key` of a device's access policy, as BSON.
async fn stored_policy_ids(app: &TestApp, agent_id: &str, key: &str) -> Vec<Bson> {
    let row = app
        .db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": ObjectId::parse_str(agent_id).unwrap() })
        .await
        .unwrap()
        .expect("the device's row");
    row.get_document("access_policy")
        .ok()
        .and_then(|p| p.get_array(key).ok())
        .cloned()
        .unwrap_or_default()
}

/// A device's answer names the role and user ids of its access policy as hex,
/// the shape the device settings read (`ui/src/stores/agents.ts`), and a save
/// of that answer as the settings dialog sends it, verbatim, stores the same
/// ObjectIds and changes nothing else: `UpdateAgentRequest` takes the policy
/// model, whose ids read a hex string as well as `{"$oid": …}`.
#[tokio::test]
async fn a_device_answer_names_its_access_policy_ids_as_hex_and_saving_it_back_keeps_them() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("wire-ap").await;
    let owner = &seeded.admin.access_token;
    let (agent_id, _) = enroll(&app, &seeded, "wire-ap-box").await;
    let (s, role) = call(
        &app,
        "POST",
        &tenant_url(&seeded, "/role"),
        owner,
        Some(json!({ "name": "wire-ap viewers", "permissions": 0 })),
    )
    .await;
    assert_eq!(s, 200, "the owner mints a role: {role}");
    let role_id = role["id"].as_str().expect("role id").to_string();
    let member_id = seeded.member.id.clone();
    let device = tenant_url(&seeded, &format!("/agent/{agent_id}"));

    let (s, body) = call(
        &app,
        "PUT",
        &device,
        owner,
        Some(json!({ "access_policy": {
            "allowed_role_ids": [role_id],
            "allowed_user_ids": [member_id],
            "auto_terminate_idle_minutes": 30,
        }})),
    )
    .await;
    assert_eq!(s, 200, "the policy is set: {body}");

    let mut wrong: Vec<String> = Vec::new();
    let (s, first) = call(&app, "GET", &device, owner, None).await;
    assert_eq!(s, 200, "{first}");
    let policy = first["access_policy"].clone();
    wrong.extend(ids_problem(
        "access_policy.allowed_role_ids",
        &policy["allowed_role_ids"],
        &[role_id.as_str()],
    ));
    wrong.extend(ids_problem(
        "access_policy.allowed_user_ids",
        &policy["allowed_user_ids"],
        &[member_id.as_str()],
    ));
    if policy["auto_terminate_idle_minutes"] != 30 {
        wrong.push(format!("the idle limit changed: {policy}"));
    }
    wrong.extend(raw_bson("the device answer", &first));

    // The settings dialog's save: the answer's policy, sent back as it came.
    let (s, body) = call(
        &app,
        "PUT",
        &device,
        owner,
        Some(json!({ "access_policy": policy })),
    )
    .await;
    if s != 200 {
        wrong.push(format!("saving the answer's policy back is a {s}: {body}"));
    }
    let role_oid = ObjectId::parse_str(&role_id).unwrap();
    let member_oid = ObjectId::parse_str(&member_id).unwrap();
    let roles = stored_policy_ids(&app, &agent_id, "allowed_role_ids").await;
    if roles != vec![Bson::ObjectId(role_oid)] {
        wrong.push(format!(
            "the stored role ids are {roles:?}, want [{role_oid}]"
        ));
    }
    let users = stored_policy_ids(&app, &agent_id, "allowed_user_ids").await;
    if users != vec![Bson::ObjectId(member_oid)] {
        wrong.push(format!(
            "the stored user ids are {users:?}, want [{member_oid}]"
        ));
    }
    let (_, again) = call(&app, "GET", &device, owner, None).await;
    if again["access_policy"] != policy {
        wrong.push(format!(
            "the policy read after the save differs: {} vs {policy}",
            again["access_policy"]
        ));
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The rotate answer's `rotated_at` is an RFC 3339 string, as the same field
/// is everywhere else it is read (`GET …/connect-code` has always sent it so).
#[tokio::test]
async fn a_rotated_connect_code_answers_its_time_as_rfc3339() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("wire-code").await;
    let owner = &seeded.admin.access_token;
    let (agent_id, _) = enroll(&app, &seeded, "wire-code-box").await;
    let code = tenant_url(&seeded, &format!("/agent/{agent_id}/connect-code"));

    let before = DateTime::now();
    let (s, body) = call(&app, "POST", &code, owner, None).await;
    let after = DateTime::now();
    assert_eq!(s, 200, "{body}");

    let mut wrong: Vec<String> = Vec::new();
    match body["rotated_at"].as_str() {
        None => wrong.push(format!("rotated_at is not a string: {body}")),
        Some(s) => match DateTime::parse_rfc3339_str(s) {
            Err(_) => wrong.push(format!("rotated_at is not RFC 3339: {s}")),
            Ok(at) if at < before || at > after => {
                wrong.push(format!("rotated_at {s} is not the time of the call"))
            }
            Ok(_) => {}
        },
    }
    if !body["connect_code"].is_string() {
        wrong.push(format!("connect_code is not a string: {body}"));
    }
    wrong.extend(raw_bson("the rotate answer", &body));

    // The control: the read of the same code was RFC 3339 already.
    let (s, read) = call(&app, "GET", &code, owner, None).await;
    assert_eq!(s, 200, "{read}");
    if read["connect_code"] != body["connect_code"] {
        wrong.push(format!("the read names another code: {read} vs {body}"));
    }
    if read["connect_code_rotated_at"]
        .as_str()
        .is_none_or(|s| DateTime::parse_rfc3339_str(s).is_err())
    {
        wrong.push(format!("the read's time is not RFC 3339: {read}"));
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The platform orgs rollup names each org's creation as an RFC 3339 string,
/// the stored instant, and carries no extended JSON anywhere.
#[tokio::test]
async fn the_orgs_rollup_names_each_orgs_creation_as_rfc3339() {
    let admin_id = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
    })
    .await;
    let seeded = app.seed_tenant("wire-orgs").await;
    let tid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let stored = app
        .db
        .collection::<Document>("tenants")
        .find_one(doc! { "_id": tid })
        .await
        .unwrap()
        .expect("the org's row");
    let created_at = *stored.get_datetime("created_at").expect("created_at");
    let tokens = app
        .state
        .auth
        .generate_tokens(admin_id, "padmin@test.io", "padmin")
        .unwrap();

    let (s, body) = call(
        &app,
        "GET",
        "/api/admin/stats/orgs",
        &tokens.access_token,
        None,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["enabled"], json!(true), "{body}");

    let mut wrong: Vec<String> = Vec::new();
    let row = body["tenants"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["id"] == seeded.tenant_id.as_str()))
        .cloned();
    match row {
        None => wrong.push(format!("the seeded org is not in the rollup: {body}")),
        Some(row) => wrong.extend(time_problem(
            "tenants[].created",
            &row["created"],
            created_at,
        )),
    }
    wrong.extend(raw_bson("the orgs rollup", &body));
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The mesh answers each node without its `last_seen_at`: no reader has the
/// field (the dashboard's `MeshPayload`, the device's `MeshNodeLite`), and it
/// went out as `{"$date": …}`. The node is still there, named by its hex id.
#[tokio::test]
async fn the_mesh_names_its_nodes_without_a_raw_time() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("wire-mesh").await;
    let tid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let (agent_id, _) = enroll(&app, &seeded, "wire-mesh-box").await;
    // A live node with every field the mesh reads, `last_seen_at` among them.
    let node_id = ObjectId::new();
    app.db
        .collection::<Document>("overlay_nodes")
        .insert_one(doc! {
            "_id": node_id,
            "tenant_id": tid,
            "node_ref": { "kind": "agent", "id": ObjectId::parse_str(&agent_id).unwrap() },
            "name": "wire-mesh-box",
            "overlay_ip": "100.64.0.9",
            "status": "online",
            "last_seen_at": DateTime::now(),
            "deleted_at": Bson::Null,
        })
        .await
        .unwrap();

    let (s, body) = call(
        &app,
        "GET",
        &tenant_url(&seeded, "/stats/mesh"),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["enabled"], json!(true), "{body}");

    let mut wrong: Vec<String> = Vec::new();
    let node = body["nodes"]
        .as_array()
        .and_then(|n| n.iter().find(|n| n["id"] == node_id.to_hex().as_str()))
        .cloned();
    match node {
        None => wrong.push(format!("the node is not in the mesh: {body}")),
        Some(node) => {
            if node.get("last_seen_at").is_some() {
                wrong.push(format!("the node still carries last_seen_at: {node}"));
            }
            if node["agent_id_hex"] != agent_id.as_str() || node["overlay_ip"] != "100.64.0.9" {
                wrong.push(format!("the node's other fields changed: {node}"));
            }
        }
    }
    wrong.extend(raw_bson("the mesh", &body));
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
