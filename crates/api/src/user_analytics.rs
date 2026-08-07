//! Platform user analytics (wave 2): who is connected, for how long,
//! from what browser, and from where.
//!
//! **Privacy shape, decided up front.** The raw client IP is used ONCE,
//! at connect time, to resolve a country — and then dropped. Nothing
//! here stores an address, and page paths are normalised (`/tenant/:id`)
//! so no room, message or tenant identifier ends up in an analytics
//! row. What remains is deliberately coarse: user, org, duration,
//! browser family, platform, country, page.
//!
//! Country resolution is OPTIONAL and pluggable — point
//! `ROOMLER__STATS__GEOIP_MMDB` at a GeoLite2/DB-IP country database and
//! it resolves; leave it unset and every session records `unknown`. No
//! licensed dataset is vendored into the repo, and an absent database
//! yields an honest "we don't know" rather than a guess.

use std::net::IpAddr;

use bson::{DateTime, doc, oid::ObjectId};

/// Browser family + OS platform, the only two things we take from a
/// User-Agent. Hand-rolled: pulling a UA-parsing crate (and its
/// regex database) for two coarse labels is not a trade worth making,
/// and the match order below is the whole subtlety — every Chromium
/// browser also says "Chrome", and everything says "Mozilla".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UaInfo {
    pub browser: &'static str,
    pub platform: &'static str,
}

pub fn parse_ua(ua: &str) -> UaInfo {
    let u = ua.to_ascii_lowercase();
    // Order matters: the more specific brand must win over the engine
    // it embeds (Edge/Opera/Brave all carry "chrome").
    let browser = if u.contains("edg/") || u.contains("edga/") || u.contains("edgios/") {
        "Edge"
    } else if u.contains("opr/") || u.contains("opera") {
        "Opera"
    } else if u.contains("brave") {
        "Brave"
    } else if u.contains("vivaldi") {
        "Vivaldi"
    } else if u.contains("firefox") || u.contains("fxios") {
        "Firefox"
    } else if u.contains("chrome") || u.contains("crios") || u.contains("chromium") {
        "Chrome"
    } else if u.contains("safari") {
        // Only after every Chromium brand is excluded: they all claim
        // "safari" for legacy reasons.
        "Safari"
    } else if u.contains("electron") || u.contains("tauri") || u.contains("roomler") {
        "Desktop app"
    } else if u.is_empty() {
        "unknown"
    } else {
        "Other"
    };

    let platform = if u.contains("android") {
        "Android"
    } else if u.contains("iphone") || u.contains("ipad") || u.contains("ios") {
        "iOS"
    } else if u.contains("windows") {
        "Windows"
    } else if u.contains("mac os") || u.contains("macintosh") {
        "macOS"
    } else if u.contains("cros") {
        "ChromeOS"
    } else if u.contains("linux") || u.contains("x11") {
        "Linux"
    } else {
        "unknown"
    };

    UaInfo { browser, platform }
}

/// Country lookup backed by an optional MaxMind-format database.
///
/// Held as `Option`: with no database configured every call answers
/// `None`, which surfaces as `unknown` — never a fabricated country.
pub struct GeoIp {
    reader: Option<maxminddb::Reader<Vec<u8>>>,
}

impl GeoIp {
    /// Open the database named by `stats.geoip_mmdb`. A missing or
    /// unreadable file is logged and degrades to "no geo", because
    /// analytics must never keep the server from starting.
    pub fn open(path: Option<&str>) -> Self {
        let reader = path.and_then(|p| match maxminddb::Reader::open_readfile(p) {
            Ok(r) => {
                tracing::info!(path = %p, "geoip database loaded");
                Some(r)
            }
            Err(e) => {
                tracing::warn!(path = %p, %e, "geoip database unusable — countries will read 'unknown'");
                None
            }
        });
        Self { reader }
    }

    pub fn enabled(&self) -> bool {
        self.reader.is_some()
    }

    /// ISO country code for an address, or `None` when unresolvable —
    /// an address the database doesn't cover is a normal outcome, not an
    /// error worth logging on every connection.
    pub fn country(&self, ip: IpAddr) -> Option<String> {
        let r = self.reader.as_ref()?;
        let looked: maxminddb::geoip2::Country = r.lookup(ip).ok()?;
        looked.country?.iso_code.map(str::to_string)
    }
}

/// Collapse a SPA path to its route shape: every id-looking segment
/// becomes `:id`, so analytics rows can be grouped by page without
/// carrying tenant/room/message identifiers around.
pub fn normalize_path(path: &str) -> String {
    let cleaned = path.split(['?', '#']).next().unwrap_or(path);
    let mut out = String::with_capacity(cleaned.len());
    for seg in cleaned.split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        if is_id_like(seg) {
            out.push_str(":id");
        } else {
            out.push_str(&seg.to_ascii_lowercase());
        }
    }
    if out.is_empty() { "/".to_string() } else { out }
}

fn is_id_like(seg: &str) -> bool {
    // 24-hex ObjectId, a UUID, or any long digit run.
    let hex24 = seg.len() == 24 && seg.chars().all(|c| c.is_ascii_hexdigit());
    let uuid = seg.len() == 36 && seg.matches('-').count() == 4;
    let numeric = seg.len() > 6 && seg.chars().all(|c| c.is_ascii_digit());
    hex24 || uuid || numeric
}

/// Open a WS session row and return its id (for the close update).
#[allow(clippy::too_many_arguments)]
pub async fn open_session(
    state: &crate::state::AppState,
    user_id: ObjectId,
    tenant_id: Option<ObjectId>,
    ua: &str,
    ip: Option<IpAddr>,
) -> Option<ObjectId> {
    if !state.settings.stats.enabled {
        return None;
    }
    let info = parse_ua(ua);
    // The IP is resolved and DROPPED here — it is never written.
    let country = ip
        .and_then(|ip| state.geoip.country(ip))
        .unwrap_or_else(|| "unknown".to_string());
    let id = ObjectId::new();
    let doc = doc! {
        "_id": id,
        "user_id": user_id,
        "tenant_id": tenant_id,
        "started_at": DateTime::now(),
        "ended_at": bson::Bson::Null,
        "browser": info.browser,
        "platform": info.platform,
        "country": country,
        "pod": state.pod.pod_id.clone(),
    };
    match state
        .db
        .collection::<bson::Document>(WS_SESSIONS)
        .insert_one(doc)
        .await
    {
        Ok(_) => Some(id),
        Err(e) => {
            tracing::debug!(%e, "ws session open persist failed");
            None
        }
    }
}

/// Close a WS session row, stamping its duration.
pub async fn close_session(state: &crate::state::AppState, id: ObjectId) {
    let now = DateTime::now();
    let update = vec![doc! { "$set": {
        "ended_at": bson::Bson::DateTime(now),
        "duration_s": { "$divide": [
            { "$subtract": [ bson::Bson::DateTime(now), "$started_at" ] },
            1000,
        ]},
    }}];
    if let Err(e) = state
        .db
        .collection::<bson::Document>(WS_SESSIONS)
        .update_one(doc! { "_id": id, "ended_at": bson::Bson::Null }, update)
        .await
    {
        tracing::debug!(%e, "ws session close persist failed");
    }
}

pub const WS_SESSIONS: &str = "ws_sessions";
pub const PAGE_VIEWS: &str = "page_views";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ua_brand_beats_the_engine_it_embeds() {
        // Every Chromium browser also says "Chrome", and all of them say
        // "Safari" — the specific brand has to win.
        let edge = parse_ua(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0",
        );
        assert_eq!(edge.browser, "Edge");
        assert_eq!(edge.platform, "Windows");

        let chrome = parse_ua(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/126.0.0.0 Safari/537.36",
        );
        assert_eq!(chrome.browser, "Chrome");
        assert_eq!(chrome.platform, "Linux");

        let safari = parse_ua(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
             (KHTML, like Gecko) Version/17.4 Safari/605.1.15",
        );
        assert_eq!(safari.browser, "Safari");
        assert_eq!(safari.platform, "macOS");

        let ff = parse_ua("Mozilla/5.0 (Windows NT 10.0; rv:127.0) Gecko/20100101 Firefox/127.0");
        assert_eq!(ff.browser, "Firefox");

        let ios = parse_ua(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 \
             (KHTML, like Gecko) CriOS/126.0 Mobile/15E148 Safari/604.1",
        );
        assert_eq!(ios.browser, "Chrome");
        assert_eq!(ios.platform, "iOS");
    }

    #[test]
    fn unknown_ua_is_labelled_not_guessed() {
        assert_eq!(parse_ua("").browser, "unknown");
        assert_eq!(parse_ua("").platform, "unknown");
        assert_eq!(parse_ua("curl/8.4.0").browser, "Other");
    }

    #[test]
    fn paths_lose_every_identifier() {
        assert_eq!(
            normalize_path("/tenant/69a1dbbad2000f26adc875ce/room/69a1dbc8d2000f26adc875d5"),
            "/tenant/:id/room/:id"
        );
        assert_eq!(
            normalize_path("/tenant/69a1dbbad2000f26adc875ce"),
            "/tenant/:id"
        );
        assert_eq!(normalize_path("/observability"), "/observability");
        assert_eq!(normalize_path("/"), "/");
        // Query strings and fragments carry ids too — dropped whole.
        assert_eq!(normalize_path("/rooms?tab=all#x"), "/rooms");
        // UUIDs and long digit runs are ids as well.
        assert_eq!(
            normalize_path("/x/123e4567-e89b-12d3-a456-426614174000"),
            "/x/:id"
        );
        assert_eq!(normalize_path("/invite/1234567890"), "/invite/:id");
        // Short numerics are NOT ids — they're usually route params like
        // a page number, and collapsing them would erase real routes.
        assert_eq!(normalize_path("/page/42"), "/page/42");
    }

    #[test]
    fn geoip_without_a_database_answers_unknown_not_a_guess() {
        let g = GeoIp::open(None);
        assert!(!g.enabled());
        assert_eq!(g.country("8.8.8.8".parse().unwrap()), None);
        // A configured-but-missing file degrades the same way.
        let g = GeoIp::open(Some("/nonexistent/GeoLite2-Country.mmdb"));
        assert!(!g.enabled());
    }
}
