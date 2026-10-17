// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P7 — the content puller (spec §3k): the blog's posts, read from
//! the site's own Atom feed once a day into `growth_content`, and — with a
//! Search Console credential — each post's index status from URL
//! Inspection.
//!
//! * **The feed is the list.** The blog is built by the docs generator
//!   (`ui/docs/build.ts`, `theme/xml.ts`) and published by the blog lane;
//!   its `/blog/feed.xml` carries every post with its permanent URL, its
//!   title and its `published` time — what the Content table needs and what
//!   no server record holds. A post that leaves the feed keeps its row,
//!   `in_feed: false`.
//! * **URL Inspection** asks Google about each post in the feed, within its
//!   quota of 2,000 a day per property ([`MAX_INSPECTIONS`] here). A post's
//!   failed inspection keeps its last good verdict and records the error; a
//!   fatal one (the key, the token, a 403 — a service account that is only a
//!   *Restricted* user may be refused here while Search Analytics answers it)
//!   stops the run and is recorded on the day's pull row.
//! * Once per cycle each, marked by `growth_pulls` rows `content:<day>` and
//!   `url_inspection:<day>` (the UTC day of the read).

use bson::DateTime;
use roomler_ai_services::dao::{
    base::DaoResult,
    growth::{GrowthDao, PulledDay},
    growth_content::{FeedPost, GrowthContentDao, Inspection},
};
use roomler_core::Core;
use serde::Serialize;
use tracing::warn;

use crate::{
    pull_schedule::{self, day_str},
    search_console::{ScClient, ScError},
};

/// The `growth_pulls` source of a feed read.
pub const CONTENT_SOURCE: &str = "content";
/// The `growth_pulls` source of a day's URL inspections.
pub const INSPECTION_SOURCE: &str = "url_inspection";
pub const CYCLE_HOUR_UTC: u32 = 2;
pub const LOOP_INTERVAL_SECS: u64 = 60 * 60;
pub const FIRST_TICK_DELAY_SECS: u64 = 8 * 60;
/// Posts read from one feed, at most.
pub const MAX_POSTS: usize = 500;
/// Inspections per day, at most — far inside Google's 2,000.
pub const MAX_INSPECTIONS: usize = 200;
/// A stored title is cut to this many characters; a path or URL to
/// [`MAX_URL_LEN`].
pub const MAX_TITLE_LEN: usize = 200;
pub const MAX_URL_LEN: usize = 300;
pub const FEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The text between `<tag>` and `</tag>` in `block`, the first one.
fn between<'a>(block: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = block.find(open)? + open.len();
    let end = block[start..].find(close)? + start;
    Some(&block[start..end])
}

/// XML's five entities and numeric references.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(semi) = tail.find(';').filter(|n| *n <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn bounded(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(max)
        .collect()
}

/// Pure: the posts of an Atom feed as our generator writes it
/// (`ui/docs/theme/xml.ts`): each `<entry>`'s alternate link, title and
/// `published` / `updated` times. An entry without a parseable URL under
/// `/blog/` or without a `published` time is skipped; at most
/// [`MAX_POSTS`].
pub fn parse_feed(xml: &str) -> Vec<FeedPost> {
    let mut out = Vec::new();
    for chunk in xml.split("<entry>").skip(1) {
        if out.len() >= MAX_POSTS {
            break;
        }
        let entry = chunk.split("</entry>").next().unwrap_or("");
        let Some(href) = entry
            .find("<link")
            .and_then(|i| between(&entry[i..], "href=\"", "\""))
            .map(unescape)
        else {
            continue;
        };
        let Ok(url) = url::Url::parse(href.trim()) else {
            continue;
        };
        let path = url.path().to_string();
        if !path.starts_with("/blog/") || path == "/blog/" || path.len() > MAX_URL_LEN {
            continue;
        }
        let Some(published) = between(entry, "<published>", "</published>")
            .and_then(|t| DateTime::parse_rfc3339_str(t.trim()).ok())
        else {
            continue;
        };
        let updated = between(entry, "<updated>", "</updated>")
            .and_then(|t| DateTime::parse_rfc3339_str(t.trim()).ok());
        let title = between(entry, "<title>", "</title>")
            .map(|t| bounded(&unescape(t), MAX_TITLE_LEN))
            .unwrap_or_default();
        out.push(FeedPost {
            path,
            url: bounded(url.as_str(), MAX_URL_LEN),
            title,
            published_at: published,
            updated_at: updated,
        });
    }
    out
}

/// The feed's URL: the setting, else `{frontend_url}/blog/feed.xml`.
pub fn feed_url(core: &Core) -> String {
    let g = core.settings.growth.blog_feed_url.trim();
    if !g.is_empty() {
        return g.to_string();
    }
    format!(
        "{}/blog/feed.xml",
        core.settings.app.frontend_url.trim_end_matches('/')
    )
}

/// What one run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContentReport {
    pub feed_due: bool,
    pub posts: u64,
    pub left_feed: u64,
    pub inspection_due: bool,
    pub inspected: u32,
    pub inspection_failed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

async fn read_feed(url: &str) -> Result<Vec<FeedPost>, String> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("roomler-ai-api/", env!("CARGO_PKG_VERSION")))
        .timeout(FEED_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("feed unreachable: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    if status == 404 {
        // No post, no blog (`ui/blog/README.md`): an empty list, not a failure.
        return Ok(Vec::new());
    }
    if !(200..300).contains(&status) {
        return Err(format!("feed unreachable: HTTP {status}"));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("feed unreadable: {}", e.without_url()))?;
    Ok(parse_feed(&text))
}

fn inspection_of(status: crate::search_console::IndexStatus, now: DateTime) -> Inspection {
    Inspection {
        verdict: status.verdict,
        coverage_state: status.coverage_state,
        indexing_state: status.indexing_state,
        page_fetch_state: status.page_fetch_state,
        robots_txt_state: status.robots_txt_state,
        last_crawl_time: status
            .last_crawl_time
            .as_deref()
            .and_then(|t| DateTime::parse_rfc3339_str(t).ok()),
        google_canonical: status.google_canonical,
        inspected_at: now,
        error: None,
    }
}

/// One run at `now`: the feed once per cycle, then the inspections once
/// per cycle when a Search Console client is given.
pub async fn run_once(
    core: &Core,
    feed: &str,
    sc: Option<&ScClient>,
    now: DateTime,
) -> DaoResult<ContentReport> {
    let dao = GrowthDao::new(&core.db);
    let content = GrowthContentDao::new(&core.db);
    let mut report = ContentReport::default();
    let today = day_str(pull_schedule::date_of(now));
    let due = |pulls: &[roomler_ai_services::dao::growth::GrowthPull]| {
        now.timestamp_millis() >= pull_schedule::cycle_start_ms(now, CYCLE_HOUR_UTC)
            && pulls
                .first()
                .is_none_or(|p| p.status != roomler_ai_services::dao::growth::PullStatus::Ok)
    };

    if due(&dao.pulls_between(CONTENT_SOURCE, &today, &today).await?) {
        report.feed_due = true;
        match read_feed(feed).await {
            Ok(posts) => {
                let (n, left) = content.sync_feed(&posts, now).await?;
                report.posts = n;
                report.left_feed = left;
                dao.record_pull_ok(
                    CONTENT_SOURCE,
                    &today,
                    PulledDay {
                        rows: n as u32,
                        missing_goal: None,
                        truncated: posts.len() >= MAX_POSTS,
                    },
                    now,
                )
                .await?;
            }
            Err(reason) => {
                dao.record_pull_failed(CONTENT_SOURCE, &today, &reason, now)
                    .await?;
                report.error = Some(reason);
            }
        }
    }

    let Some(sc) = sc else {
        return Ok(report);
    };
    if !due(&dao.pulls_between(INSPECTION_SOURCE, &today, &today).await?) {
        return Ok(report);
    }
    report.inspection_due = true;
    let posts: Vec<_> = content
        .list()
        .await?
        .into_iter()
        .filter(|p| p.in_feed)
        .take(MAX_INSPECTIONS)
        .collect();
    let mut fatal: Option<ScError> = None;
    for p in &posts {
        match sc.inspect(&p.url).await {
            Ok(status) => {
                content
                    .set_inspection(&p.path, &inspection_of(status, now))
                    .await?;
                report.inspected += 1;
            }
            Err(e) => {
                report.inspection_failed += 1;
                content
                    .set_inspection(&p.path, &Inspection::failed(now, e.to_string()))
                    .await?;
                if e.is_fatal() {
                    fatal = Some(e);
                    break;
                }
            }
        }
    }
    match fatal {
        Some(e) => {
            let reason = e.to_string();
            warn!(%reason, "url inspection stopped");
            dao.record_pull_failed(INSPECTION_SOURCE, &today, &reason, now)
                .await?;
            report.error.get_or_insert(reason);
        }
        None => {
            dao.record_pull_ok(
                INSPECTION_SOURCE,
                &today,
                PulledDay {
                    rows: report.inspected,
                    missing_goal: None,
                    truncated: posts.len() >= MAX_INSPECTIONS,
                },
                now,
            )
            .await?;
        }
    }
    Ok(report)
}

/// The scheduler: hourly, one pod (the Redis claim), with `growth.enabled`
/// — the feed is the site's own, so it is read without any credential; the
/// inspections only with a usable Search Console key.
pub fn spawn_puller(core: Core) {
    if !core.settings.growth.enabled {
        return;
    }
    let sc = match ScClient::from_settings(&core.settings.growth) {
        Some(Ok(c)) => Some(c),
        // `search_pull::spawn_puller` says why, once.
        Some(Err(_)) | None => None,
    };
    let feed = feed_url(&core);
    tracing::info!(feed = %feed, inspection = sc.is_some(), "growth content puller starting");
    pull_schedule::spawn_hourly(
        core,
        "growth-content-pull",
        FIRST_TICK_DELAY_SECS,
        LOOP_INTERVAL_SECS,
        move |core| {
            let sc = sc.clone();
            let feed = feed.clone();
            async move {
                match run_once(&core, &feed, sc.as_ref(), DateTime::now()).await {
                    Ok(r) if r.feed_due || r.inspection_due => tracing::info!(
                        posts = r.posts,
                        left_feed = r.left_feed,
                        inspected = r.inspected,
                        inspection_failed = r.inspection_failed,
                        error = r.error.as_deref().unwrap_or(""),
                        "growth content pull"
                    ),
                    Ok(_) => {}
                    Err(e) => warn!(%e, "growth content pull failed"),
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The feed exactly as `ui/docs/theme/xml.ts` writes it.
    const FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xml:lang="en">
  <id>https://roomler.ai/blog/</id>
  <title>Roomler blog</title>
  <link rel="self" type="application/atom+xml" href="https://roomler.ai/blog/feed.xml"/>
  <updated>2026-10-13T14:49:49Z</updated>
  <entry>
    <id>https://roomler.ai/blog/self-hosted-teamviewer-alternative/</id>
    <title>A self-hosted TeamViewer alternative &amp; why &lt;it&gt; matters</title>
    <link rel="alternate" type="text/html" href="https://roomler.ai/blog/self-hosted-teamviewer-alternative/"/>
    <published>2026-09-25T22:45:13Z</published>
    <updated>2026-10-01T08:00:00Z</updated>
    <author><name>Goran</name></author>
    <summary>One &#8212; two</summary>
    <content type="html">&lt;title&gt;not the title&lt;/title&gt;&lt;p&gt;Body&lt;/p&gt;</content>
  </entry>
  <entry>
    <id>https://roomler.ai/blog/ssh-without-port-forwarding/</id>
    <title>SSH without port forwarding</title>
    <link rel="alternate" type="text/html" href="https://roomler.ai/blog/ssh-without-port-forwarding/"/>
    <published>2026-10-19T06:00:00+02:00</published>
    <updated>2026-10-19T06:00:00+02:00</updated>
    <author><name>Goran</name></author>
  </entry>
  <entry>
    <id>https://roomler.ai/docs/x/</id>
    <title>Not a post</title>
    <link rel="alternate" type="text/html" href="https://roomler.ai/docs/x/"/>
    <published>2026-10-01T00:00:00Z</published>
  </entry>
  <entry>
    <title>No date</title>
    <link rel="alternate" type="text/html" href="https://roomler.ai/blog/undated/"/>
  </entry>
</feed>
"#;

    #[test]
    fn the_feed_reads_each_posts_path_title_and_times() {
        let posts = parse_feed(FEED);
        assert_eq!(posts.len(), 2, "{posts:?}");
        let a = &posts[0];
        assert_eq!(a.path, "/blog/self-hosted-teamviewer-alternative/");
        assert_eq!(
            a.url,
            "https://roomler.ai/blog/self-hosted-teamviewer-alternative/"
        );
        assert_eq!(
            a.title,
            "A self-hosted TeamViewer alternative & why <it> matters"
        );
        assert_eq!(
            a.published_at,
            DateTime::parse_rfc3339_str("2026-09-25T22:45:13Z").unwrap()
        );
        assert_eq!(
            a.updated_at,
            Some(DateTime::parse_rfc3339_str("2026-10-01T08:00:00Z").unwrap())
        );
        // An offset is honoured: 06:00+02:00 is 04:00Z.
        assert_eq!(
            posts[1].published_at,
            DateTime::parse_rfc3339_str("2026-10-19T04:00:00Z").unwrap()
        );
        // Not a feed at all: nothing, never a panic.
        assert!(parse_feed("<html>nope</html>").is_empty());
        assert!(parse_feed("<entry><link href=\"").is_empty());
    }

    #[test]
    fn entities_unescape_and_junk_stays_literal() {
        assert_eq!(
            unescape("a &amp; b &lt;c&gt; &quot;d&quot; &apos;e&apos;"),
            "a & b <c> \"d\" 'e'"
        );
        assert_eq!(unescape("&#8212;&#x2014;"), "——");
        assert_eq!(unescape("AT&T & co; &bogus; &"), "AT&T & co; &bogus; &");
    }
}
