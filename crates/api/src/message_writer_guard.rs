// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4448 — `MessageDao` is the one writer of `messages`.
//!
//! A room's `message_count` moves beside each message `MessageDao` writes or
//! deletes. A write into `messages` that goes around it leaves the count wrong
//! until the repair's next sweep, and each such pass logs at warn: the alarm
//! says a writer was missed, and this test is what keeps one from being
//! added. Two parts:
//!
//! 1. The type system: `MessageDao`'s collection handle is private, so no
//!    code outside the DAO writes through it. [`the_dao_keeps_its_collection_private`]
//!    fails if `base` becomes `pub` again.
//! 2. This scan: it reads every `.rs` file under `crates/` and `agents/`
//!    (not `crates/tests`, a crate's own `tests/` folder, `crates/vendored` or
//!    a build's `target/`) and fails on any file that names the collection
//!    (`Message::COLLECTION`, the literal `"messages"`, `BaseDao<Message>`,
//!    `Collection<Message>`) unless [`ALLOWED`] lists it with its reason. A
//!    listed file that no longer names it fails too, so the list only holds
//!    what is there.
//!
//! A source scan and not a lint: the cheapest check that fails on the next
//! raw write, in every PR's unit-test lane, with no database. It reads text,
//! so it cannot see a collection name built at run time (`format!`) or one a
//! macro writes; a comment line (`//`) is not read.

use std::path::{Path, PathBuf};

/// The files that may name the `messages` collection, and why.
const ALLOWED: &[(&str, &str)] = &[
    (
        "crates/db/src/models/message.rs",
        "defines `Message::COLLECTION`",
    ),
    (
        "crates/services/src/dao/message.rs",
        "`MessageDao`, the one writer: it moves the room's count with each write",
    ),
    (
        "crates/services/src/dao/room.rs",
        "`cascade_delete` removes a room's messages with the room, and the room takes its count",
    ),
    (
        "crates/services/src/dao/room_message_count.rs",
        "the count's repair, which only reads `messages`",
    ),
    (
        "crates/modules/chat/src/lib.rs",
        "chat's `index_plan` declares the collection's indexes",
    ),
    (
        "crates/modules/saas/src/plan_compliance.rs",
        "counts each tenant's live messages for the plan report; reads only",
    ),
];

/// What names the collection.
const NAMES: &[&str] = &[
    "Message::COLLECTION",
    "\"messages\"",
    "BaseDao<Message>",
    "BaseDao::<Message>",
    "Collection<Message>",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                "target" | "tests" | "vendored" | "node_modules"
            ) {
                continue;
            }
            rust_files(&path, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// Each scanned file that names the collection, as `/`-separated paths below
/// the repo, with its first such line.
fn naming_files() -> Vec<(String, String)> {
    let repo = repo();
    let mut files = Vec::new();
    rust_files(&repo.join("crates"), &mut files);
    rust_files(&repo.join("agents"), &mut files);
    let mut out = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(&repo)
            .expect("below the repo")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if rel == "crates/api/src/message_writer_guard.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let hit = text.lines().enumerate().find(|(_, line)| {
            !line.trim_start().starts_with("//") && NAMES.iter().any(|n| line.contains(n))
        });
        if let Some((n, line)) = hit {
            out.push((rel, format!("{}: {}", n + 1, line.trim())));
        }
    }
    out.sort();
    out
}

#[test]
fn only_the_listed_files_name_the_messages_collection() {
    let found = naming_files();
    assert!(
        found
            .iter()
            .any(|(f, _)| f == "crates/services/src/dao/message.rs"),
        "the scan did not find MessageDao itself, so it reads nothing: {found:?}"
    );
    let unlisted: Vec<_> = found
        .iter()
        .filter(|(f, _)| !ALLOWED.iter().any(|(a, _)| a == f))
        .collect();
    assert!(
        unlisted.is_empty(),
        "these files name the `messages` collection outside MessageDao. A write there \
         leaves every room's `message_count` wrong (#4448): write through MessageDao \
         instead. A read may be listed in ALLOWED with its reason: {unlisted:#?}"
    );
    let gone: Vec<_> = ALLOWED
        .iter()
        .filter(|(a, _)| !found.iter().any(|(f, _)| f == a))
        .collect();
    assert!(
        gone.is_empty(),
        "these ALLOWED files no longer name the collection; remove them: {gone:#?}"
    );
}

#[test]
fn the_dao_keeps_its_collection_private() {
    let dao = std::fs::read_to_string(repo().join("crates/services/src/dao/message.rs"))
        .expect("MessageDao's source");
    let start = dao
        .find("pub struct MessageDao {")
        .expect("MessageDao's struct");
    let body = &dao[start..start + dao[start..].find('}').expect("its end")];
    assert!(body.contains("    base: BaseDao<Message>,"), "{body}");
    assert!(
        !body.contains("pub base") && !body.contains("pub(crate) base"),
        "MessageDao's collection handle is public again: a write through it skips the \
         room's message_count (#4448): {body}"
    );
}
