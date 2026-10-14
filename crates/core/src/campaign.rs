// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P1b — the campaign key: one taxonomy, checked at both ends.
//!
//! The anonymous plane (purestat) and the account plane (roomler) meet only
//! at the campaign key `(source, medium, campaign, content)` and at counts
//! (spec §3a). A join by key is only a join when both ends spell the key the
//! same way — and the field found `youtub`, `YouTube` and an empty medium in
//! one month of real links (spec §2a). This module is the ONE table:
//!
//! * [`SOURCES`] / [`MEDIUMS`] — the canonical values the link linter checks
//!   against and the reports group by;
//! * [`normalize_source`] / [`normalize_medium`] — what the server applies at
//!   ingress (`attribution::sanitize`): trim, lowercase, then the aliases;
//! * [`source_group`] / [`medium_group`] — the canonical value, or `other`.
//!   ⚠️ An unknown value is **kept verbatim** (lowercased) and only
//!   *reported* under `other`, never dropped: a new channel must show up in
//!   the numbers before it has a name in this table, or the table decides
//!   what can be measured.
//! * [`channel_for_referrer_host`] — the organic channels, from a bare
//!   referrer host (spec §3b): search, AI assistants, social, community,
//!   code, self (`roomler.ai` is navigation, never a source), direct.
//!
//! The private promotion pipeline's link linter checks against a pinned copy
//! of the same table; a change here is a change there.

use serde::Serialize;

/// `utm_source`: the platform, never the account. `instagram` is retired
/// and kept for history.
pub const SOURCES: [&str; 14] = [
    "youtube",
    "tiktok",
    "x",
    "linkedin",
    "facebook",
    "reddit",
    "hn",
    "devto",
    "hashnode",
    "medium",
    "github",
    "newsletter",
    "blog",
    "instagram",
];

/// `utm_medium`: the format.
pub const MEDIUMS: [&str; 8] = [
    "video",
    "post",
    "carousel",
    "comment",
    "community",
    "bio",
    "article",
    "email",
];

/// The group every value outside the table reports under.
pub const OTHER: &str = "other";

/// The spellings seen in the wild, mapped to the canonical value. Applied
/// AFTER lowercasing, so the left side is lowercase.
const SOURCE_ALIASES: &[(&str, &str)] = &[
    ("twitter", "x"),
    ("fb", "facebook"),
    ("yt", "youtube"),
    ("youtub", "youtube"),
    ("li", "linkedin"),
    ("ig", "instagram"),
];

/// Trim, lowercase, then the aliases. Never empties a non-empty value and
/// never invents one: an unknown source comes back as itself, lowercased.
pub fn normalize_source(raw: &str) -> String {
    let value = raw.trim().to_lowercase();
    match SOURCE_ALIASES.iter().find(|(alias, _)| *alias == value) {
        Some((_, canonical)) => (*canonical).to_string(),
        None => value,
    }
}

/// Trim and lowercase. The mediums have no aliases worth encoding yet; an
/// unknown one is kept as itself.
pub fn normalize_medium(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// The canonical source a (normalised) value belongs to, or [`OTHER`].
pub fn source_group(normalised: &str) -> &'static str {
    SOURCES
        .iter()
        .copied()
        .find(|s| *s == normalised)
        .unwrap_or(OTHER)
}

/// The canonical medium a (normalised) value belongs to, or [`OTHER`].
pub fn medium_group(normalised: &str) -> &'static str {
    MEDIUMS
        .iter()
        .copied()
        .find(|m| *m == normalised)
        .unwrap_or(OTHER)
}

/// Where a visit came from, by its referrer host (spec §3b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Search,
    AiAssistant,
    Social,
    Community,
    Code,
    /// `roomler.ai` itself: navigation, never a source.
    #[serde(rename = "self")]
    SelfSite,
    /// No referrer at all.
    Direct,
    Other,
}

impl Channel {
    /// The wire / report spelling (the serde one).
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Search => "search",
            Channel::AiAssistant => "ai_assistant",
            Channel::Social => "social",
            Channel::Community => "community",
            Channel::Code => "code",
            Channel::SelfSite => "self",
            Channel::Direct => "direct",
            Channel::Other => "other",
        }
    }

    /// Every channel, in report order.
    pub const ALL: [Channel; 8] = [
        Channel::Search,
        Channel::AiAssistant,
        Channel::Social,
        Channel::Community,
        Channel::Code,
        Channel::SelfSite,
        Channel::Direct,
        Channel::Other,
    ];
}

/// Hosts matched by suffix (`host == domain` or `host` ends with
/// `.domain`), first match wins — so `gemini.google.com` sits ABOVE the
/// `google` label rule below, and `l.facebook.com` needs no row of its own.
const HOST_SUFFIXES: &[(&str, Channel)] = &[
    // AI assistants
    ("chatgpt.com", Channel::AiAssistant),
    ("chat.openai.com", Channel::AiAssistant),
    ("perplexity.ai", Channel::AiAssistant),
    ("gemini.google.com", Channel::AiAssistant),
    ("copilot.microsoft.com", Channel::AiAssistant),
    ("claude.ai", Channel::AiAssistant),
    // Search (the `google.*` / `yandex.*` families are a label rule below)
    ("bing.com", Channel::Search),
    ("duckduckgo.com", Channel::Search),
    ("ecosia.org", Channel::Search),
    // Social
    ("t.co", Channel::Social),
    ("x.com", Channel::Social),
    ("twitter.com", Channel::Social),
    ("linkedin.com", Channel::Social),
    ("lnkd.in", Channel::Social),
    ("facebook.com", Channel::Social),
    ("youtube.com", Channel::Social),
    ("youtu.be", Channel::Social),
    ("tiktok.com", Channel::Social),
    ("reddit.com", Channel::Social),
    ("instagram.com", Channel::Social),
    // Community
    ("news.ycombinator.com", Channel::Community),
    ("lobste.rs", Channel::Community),
    ("dev.to", Channel::Community),
    ("hashnode.com", Channel::Community),
    ("medium.com", Channel::Community),
    // Code
    ("github.com", Channel::Code),
    // Self
    ("roomler.ai", Channel::SelfSite),
];

/// Search engines whose registrable label is followed by any country TLD
/// (`google.com`, `www.google.co.uk`, `yandex.ru`).
const SEARCH_LABELS: &[&str] = &["google", "yandex"];

/// The organic channel of a referrer host. `None` (no referrer recorded) is
/// [`Channel::Direct`]; a host nothing here recognises is
/// [`Channel::Other`]. The host is read as `attribution::clean_host` stores
/// it — lowercase, optionally `:port` — but a raw `Host`-style value with
/// upper case or a trailing dot is tolerated.
pub fn channel_for_referrer_host(host: Option<&str>) -> Channel {
    let Some(raw) = host else {
        return Channel::Direct;
    };
    let host = raw.trim().to_ascii_lowercase();
    let host = host.split(':').next().unwrap_or("").trim_end_matches('.');
    if host.is_empty() {
        return Channel::Direct;
    }
    for (domain, channel) in HOST_SUFFIXES {
        if host == *domain || host.ends_with(&format!(".{domain}")) {
            return *channel;
        }
    }
    // `google.*`: a `google` label that is not the last one (so `google.com`,
    // `www.google.de`, `google.co.uk`), never a host that merely contains
    // the word (`googleusercontent.com`, `notgoogle.com`).
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() >= 2
        && labels[..labels.len() - 1]
            .iter()
            .any(|l| SEARCH_LABELS.contains(l))
    {
        return Channel::Search;
    }
    Channel::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC5's two halves: the aliases and the case, and an unknown value kept
    /// as itself rather than dropped or guessed at.
    #[test]
    fn sources_normalise_through_case_and_aliases() {
        assert_eq!(normalize_source("YouTub"), "youtube");
        assert_eq!(normalize_source(" yt "), "youtube");
        assert_eq!(normalize_source("YouTube"), "youtube");
        assert_eq!(normalize_source("Twitter"), "x");
        assert_eq!(normalize_source("FB"), "facebook");
        assert_eq!(normalize_source("li"), "linkedin");
        assert_eq!(normalize_source("IG"), "instagram");
        // Unknown: verbatim, lowercased — never dropped.
        assert_eq!(normalize_source("Mastodon"), "mastodon");
        assert_eq!(normalize_source("check"), "check");
        // A canonical value is a fixed point.
        for s in SOURCES {
            assert_eq!(normalize_source(s), s);
            assert_eq!(normalize_source(&s.to_uppercase()), s);
        }
    }

    #[test]
    fn mediums_normalise_through_case() {
        assert_eq!(normalize_medium("Video"), "video");
        assert_eq!(normalize_medium(" POST "), "post");
        assert_eq!(normalize_medium("reel"), "reel");
        for m in MEDIUMS {
            assert_eq!(normalize_medium(m), m);
            assert_eq!(normalize_medium(&m.to_uppercase()), m);
        }
    }

    /// The whole table, both directions: every canonical value groups to
    /// itself, every alias groups to its canonical, and anything else is
    /// `other`.
    #[test]
    fn the_table_is_exhaustive_and_unknowns_group_under_other() {
        for s in SOURCES {
            assert_eq!(source_group(s), s);
        }
        for (alias, canonical) in SOURCE_ALIASES {
            assert!(
                SOURCES.contains(canonical),
                "alias {alias} points outside the table: {canonical}"
            );
            assert_eq!(source_group(&normalize_source(alias)), *canonical);
        }
        for m in MEDIUMS {
            assert_eq!(medium_group(m), m);
        }
        assert_eq!(source_group("mastodon"), OTHER);
        assert_eq!(source_group(""), OTHER);
        assert_eq!(medium_group("reel"), OTHER);
        // The table has no duplicates (a duplicate would make `source_group`
        // depend on order).
        let mut sorted: Vec<&str> = SOURCES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), SOURCES.len());
        let mut sorted: Vec<&str> = MEDIUMS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), MEDIUMS.len());
        // An alias must not shadow a canonical value.
        for (alias, _) in SOURCE_ALIASES {
            assert!(
                !SOURCES.contains(alias),
                "{alias} is both an alias and canonical"
            );
        }
    }

    /// The spec's §3b table, host by host, plus the subdomain and port forms
    /// the referrer actually arrives in.
    #[test]
    fn referrer_hosts_map_to_their_channel() {
        use Channel::*;
        let cases: &[(&str, Channel)] = &[
            // search
            ("google.com", Search),
            ("www.google.com", Search),
            ("www.google.co.uk", Search),
            ("google.de", Search),
            ("bing.com", Search),
            ("www.bing.com", Search),
            ("duckduckgo.com", Search),
            ("ecosia.org", Search),
            ("yandex.ru", Search),
            ("www.yandex.com", Search),
            // AI assistants
            ("chatgpt.com", AiAssistant),
            ("chat.openai.com", AiAssistant),
            ("perplexity.ai", AiAssistant),
            ("www.perplexity.ai", AiAssistant),
            ("gemini.google.com", AiAssistant),
            ("copilot.microsoft.com", AiAssistant),
            ("claude.ai", AiAssistant),
            // social
            ("t.co", Social),
            ("x.com", Social),
            ("twitter.com", Social),
            ("linkedin.com", Social),
            ("www.linkedin.com", Social),
            ("lnkd.in", Social),
            ("facebook.com", Social),
            ("l.facebook.com", Social),
            ("m.facebook.com", Social),
            ("youtube.com", Social),
            ("www.youtube.com", Social),
            ("m.youtube.com", Social),
            ("youtu.be", Social),
            ("tiktok.com", Social),
            ("www.tiktok.com", Social),
            ("reddit.com", Social),
            ("old.reddit.com", Social),
            ("out.reddit.com", Social),
            ("instagram.com", Social),
            // community
            ("news.ycombinator.com", Community),
            ("lobste.rs", Community),
            ("dev.to", Community),
            ("hashnode.com", Community),
            ("medium.com", Community),
            // code
            ("github.com", Code),
            // self
            ("roomler.ai", SelfSite),
            ("www.roomler.ai", SelfSite),
            // other
            ("example.com", Other),
            ("localhost", Other),
            ("localhost:5000", Other),
            ("googleusercontent.com", Other),
            ("notgoogle.com", Other),
            ("google", Other),
            ("roomler.ai.evil.example", Other),
        ];
        for (host, expected) in cases {
            assert_eq!(channel_for_referrer_host(Some(host)), *expected, "{host}");
        }
        // The forms a raw referrer can arrive in.
        assert_eq!(
            channel_for_referrer_host(Some("WWW.YouTube.com:443")),
            Social
        );
        assert_eq!(channel_for_referrer_host(Some("github.com.")), Code);
        // No referrer is direct, not other.
        assert_eq!(channel_for_referrer_host(None), Direct);
        assert_eq!(channel_for_referrer_host(Some("")), Direct);
        assert_eq!(channel_for_referrer_host(Some("   ")), Direct);
    }

    #[test]
    fn channel_spellings_are_the_wire_names() {
        for c in Channel::ALL {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
        }
        assert_eq!(Channel::SelfSite.as_str(), "self");
        assert_eq!(Channel::AiAssistant.as_str(), "ai_assistant");
    }
}
