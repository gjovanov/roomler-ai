// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P7 — the blog's posts as the site publishes them, and what Google
//! says about each one's index status (spec §3k): `growth_content`.
//!
//! The `saas` module owns the collection (its index set is the module's,
//! FR-69 rule 3): its content puller reads the blog's own Atom feed once a
//! day and writes one row per post through this DAO, and — with a Search
//! Console credential — each post's URL Inspection verdict. The host's
//! Content table reads it here, below both, so a server that stops
//! mounting `saas` still shows what was read (and says why nothing new
//! arrives), as `billing_events` does for the revenue view.
//!
//! * `_id` is the post's PATH (`/blog/<slug>/`) — the key every reader
//!   joins on: purestat's `path`, Search Console's page URL's path, a
//!   sign-up's `landing_path`.
//! * A post that leaves the feed keeps its row with `in_feed: false` — its
//!   history (visits, search) is still worth reading.
//! * Our own content, no person: no TTL (spec §3p, like `growth_posts`).

use bson::{DateTime, Document, doc};
use futures::TryStreamExt;
use mongodb::Database;
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const GROWTH_CONTENT: &str = "growth_content";
/// The most posts one read loads — a bound, not a page.
pub const MAX_CONTENT: i64 = 2_000;

/// What URL Inspection said about one post, as Google spells it. Every
/// field is optional because Google omits what does not apply (a URL never
/// crawled has no `last_crawl_time`, one not indexed no canonical).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Inspection {
    /// `PASS` · `NEUTRAL` (excluded) · `FAIL` · `PARTIAL` (reserved).
    #[serde(default)]
    pub verdict: Option<String>,
    /// Google's free text: "Submitted and indexed", "Crawled - currently
    /// not indexed", …
    #[serde(default)]
    pub coverage_state: Option<String>,
    #[serde(default)]
    pub indexing_state: Option<String>,
    #[serde(default)]
    pub page_fetch_state: Option<String>,
    #[serde(default)]
    pub robots_txt_state: Option<String>,
    #[serde(default)]
    pub last_crawl_time: Option<DateTime>,
    #[serde(default)]
    pub google_canonical: Option<String>,
    /// When the verdict above was read: the last SUCCESSFUL inspection.
    /// `None` while every attempt so far has failed.
    #[serde(default)]
    pub inspected_at: Option<DateTime>,
    /// The newest attempt, successful or not.
    #[serde(default)]
    pub attempted_at: Option<DateTime>,
    /// Why the newest attempt failed, when it did; the fields above are then
    /// the last good inspection's (none, if there never was one).
    #[serde(default)]
    pub error: Option<String>,
}

impl Inspection {
    /// A failed attempt: the error and its time, nothing else —
    /// [`GrowthContentDao::set_inspection`] keeps the last good verdict and
    /// its `inspected_at`.
    pub fn failed(at: DateTime, error: String) -> Self {
        Self {
            verdict: None,
            coverage_state: None,
            indexing_state: None,
            page_fetch_state: None,
            robots_txt_state: None,
            last_crawl_time: None,
            google_canonical: None,
            inspected_at: None,
            attempted_at: Some(at),
            error: Some(error),
        }
    }
}

/// One post, as stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContentPost {
    /// The post's path, `/blog/<slug>/`.
    #[serde(rename = "_id")]
    pub path: String,
    pub url: String,
    pub title: String,
    pub published_at: DateTime,
    #[serde(default)]
    pub updated_at: Option<DateTime>,
    pub first_seen_at: DateTime,
    pub last_seen_at: DateTime,
    /// In the feed at the newest read.
    pub in_feed: bool,
    #[serde(default)]
    pub inspection: Option<Inspection>,
}

/// A post as the feed gives it.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedPost {
    pub path: String,
    pub url: String,
    pub title: String,
    pub published_at: DateTime,
    pub updated_at: Option<DateTime>,
}

pub struct GrowthContentDao {
    db: Database,
}

impl GrowthContentDao {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    fn coll(&self) -> mongodb::Collection<Document> {
        self.db.collection::<Document>(GROWTH_CONTENT)
    }

    /// One read of the feed: every post in it upserted (`first_seen_at` set
    /// once), and every post NOT in it marked `in_feed: false` — never
    /// deleted. Returns `(in the feed, left it)`.
    pub async fn sync_feed(&self, posts: &[FeedPost], now: DateTime) -> DaoResult<(u64, u64)> {
        let coll = self.coll();
        let mut paths = Vec::with_capacity(posts.len());
        for p in posts {
            paths.push(p.path.clone());
            coll.update_one(
                doc! { "_id": &p.path },
                doc! {
                    "$set": {
                        "url": &p.url,
                        "title": &p.title,
                        "published_at": p.published_at,
                        "updated_at": p.updated_at,
                        "last_seen_at": now,
                        "in_feed": true,
                    },
                    "$setOnInsert": { "first_seen_at": now },
                },
            )
            .upsert(true)
            .await?;
        }
        let left = coll
            .update_many(
                doc! { "_id": { "$nin": &paths[..] }, "in_feed": true },
                doc! { "$set": { "in_feed": false } },
            )
            .await?
            .modified_count;
        Ok((posts.len() as u64, left))
    }

    /// The newest inspection of one post. A failed one keeps the last good
    /// result's fields — its verdict and the `inspected_at` it was read at —
    /// and records only the error and the attempt's time.
    pub async fn set_inspection(&self, path: &str, inspection: &Inspection) -> DaoResult<()> {
        let update = if inspection.error.is_some() {
            doc! { "$set": {
                "inspection.error": inspection.error.clone(),
                "inspection.attempted_at": inspection.attempted_at,
            } }
        } else {
            doc! { "$set": { "inspection": bson::to_bson(inspection)? } }
        };
        self.coll().update_one(doc! { "_id": path }, update).await?;
        Ok(())
    }

    /// Every post, newest first; rows that do not have the shape (a hand
    /// edit) are skipped with a warning.
    pub async fn list(&self) -> DaoResult<Vec<ContentPost>> {
        let docs: Vec<Document> = self
            .coll()
            .find(doc! {})
            .sort(doc! { "published_at": -1 })
            .limit(MAX_CONTENT)
            .await?
            .try_collect()
            .await?;
        let mut out = Vec::with_capacity(docs.len());
        for d in docs {
            match bson::from_document::<ContentPost>(d) {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!(error = %e, "growth_content: skipping an unreadable row"),
            }
        }
        Ok(out)
    }
}
