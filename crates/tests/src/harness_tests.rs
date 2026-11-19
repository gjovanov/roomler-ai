// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Tests of the test harness itself.
//!
//! Everything else in this crate tests the product. This module tests
//! `TestApp`, because the fixture silently stopped doing one of its two jobs
//! and nothing noticed for months.
//!
//! `Drop for TestApp` dropped each test's database from a detached
//! `tokio::spawn`. Every test here is a plain `#[tokio::test]` — a
//! current-thread runtime — so the body finished, `Drop` queued the task, and
//! `block_on` returned before it was ever polled; the runtime was then dropped
//! with the task still in the queue. Detached is not "later". Once the runtime
//! goes, it is never.
//!
//! It stayed invisible because a leaked database breaks nothing locally: tests
//! pass, the disk grows a little. It surfaced only at scale — ~246 WiredTiger
//! files per database (one per collection AND per index) crossed mongod's
//! 64 000-descriptor ceiling around the 260th database and aborted the server
//! mid-suite (`EMFILE` → fatal assertion 50853), which then read as dozens of
//! unrelated tests failing on `Connection refused`.
//!
//! CI asserts this too, but a check that lives only in one workflow only ever
//! protects that workflow. This one travels with the suite — build host, WSL,
//! a laptop — wherever it is run.

use crate::fixtures::test_app::{DropDatabase, TestApp};
use mongodb::Client;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A `spawn*` that panics part-way must not leave its database behind.
///
/// The shape of the failure: the database has been written to (so it exists
/// on the server), then setup panics before any `TestApp` exists to own the
/// teardown, e.g. `ensure_indexes(..).expect(..)` on a mongod that stalled.
/// The guard `spawn*` arms before its first write must drop it while the
/// panic unwinds.
#[tokio::test]
async fn a_setup_that_panics_after_its_first_write_leaves_no_database() {
    // Learn the test mongod's address the way every test does.
    let uri = TestApp::spawn().await.settings.database.url.clone();
    let name = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());

    // On a thread of its own, so the panic unwinds there and this test can
    // look at the server afterwards.
    let existed = Arc::new(AtomicBool::new(false));
    let (u, n, seen) = (uri.clone(), name.clone(), existed.clone());
    let setup = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let _drop_db = DropDatabase::new(&u, &n);
            let client = Client::with_uri_str(&u).await.expect("client");
            client
                .database(&n)
                .collection::<bson::Document>("first_write")
                .insert_one(bson::doc! { "x": 1 })
                .await
                .expect("the first write");
            let names = client.list_database_names().await.expect("listDatabases");
            seen.store(names.contains(&n), Ordering::SeqCst);
            panic!("setup failed after its first write (deliberate, from this test)");
        })
    })
    .join();
    assert!(setup.is_err(), "the setup thread was meant to panic");
    // ⚠️ Load-bearing, as in the test below: a database that never existed
    // would satisfy the final assertion and prove nothing.
    assert!(
        existed.load(Ordering::SeqCst),
        "precondition failed: {name} did not exist before the setup panicked"
    );

    let probe = Client::with_uri_str(&uri).await.expect("probe client");
    let after = probe
        .list_database_names()
        .await
        .expect("listDatabases should succeed");
    assert!(
        !after.iter().any(|x| x == &name),
        "{name} survived a setup that panicked after its first write. Every \
         `spawn*` must arm its `DropDatabase` BEFORE the first write, or each \
         failed setup leaves one more database (and its WiredTiger files) behind."
    );
}

/// `TestApp`'s teardown must actually drop the database, before `drop` returns.
#[tokio::test]
async fn testapp_teardown_drops_its_database() {
    let app = TestApp::spawn().await;
    let db_name = app.db.name().to_string();
    let uri = app.settings.database.url.clone();

    // Observe through a SEPARATE client. The point of the test is what is true
    // of the server, not what the about-to-be-torn-down client believes.
    let probe = Client::with_uri_str(&uri)
        .await
        .expect("probe client should reach the test mongod");

    // ⚠️ The precondition is load-bearing, not decoration. Without it, a
    // database that never existed would satisfy the assertion below and this
    // test would pass while proving nothing — the exact vacuous-green shape it
    // exists to rule out.
    let before = probe
        .list_database_names()
        .await
        .expect("listDatabases should succeed");
    assert!(
        before.iter().any(|n| n == &db_name),
        "precondition failed: {db_name} does not exist while its TestApp is \
         alive, so this test cannot say anything about teardown"
    );

    drop(app);

    let after = probe
        .list_database_names()
        .await
        .expect("listDatabases should succeed");
    assert!(
        !after.iter().any(|n| n == &db_name),
        "TestApp's teardown did not drop {db_name}. Cleanup must complete \
         BEFORE `drop` returns — a detached `tokio::spawn` here never runs at \
         all, because this test's current-thread runtime is dropped with the \
         task still queued. Left unfixed, each leaked database holds ~246 \
         WiredTiger files open and a full suite exhausts mongod's descriptors \
         mid-run."
    );
}
