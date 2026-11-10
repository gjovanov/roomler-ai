// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! How a native client's credential travels on a WebSocket upgrade (#3656).
//!
//! The daemon and the tunnel CLI authenticate `/ws` (`role=agent`,
//! `role=tunnel-client`), the control plane's `/derp` and a regional relay's
//! `/derp` with a long-lived credential. Released clients put it in the URL
//! query (`token=`, and `ticket=` at a relay). Newer ones send it as
//! `Authorization: Bearer <credential>`, as their HTTP routes always have: a
//! request line is what access logs and inspecting proxies record, and a
//! request header is not. `role` and `tid` stay in the query, because the
//! front load balancer hashes on `tid`.
//!
//! Both halves of the contract live here, so the server, the relay and both
//! clients read it from one place:
//!
//! - [`presented`]: the credential an upgrade is judged on. A well-formed
//!   Bearer header wins, and the query is then not read at all, so a header
//!   and a query that disagree get exactly what the header alone would.
//! - [`challenge_for`]: the `WWW-Authenticate` a refusal carries (RFC 6750
//!   §3), which says whether a credential arrived.
//! - [`asks_for_query_form`]: how a client reads a refusal. A server or a
//!   relay older than the header answers a header-only dial with no Bearer
//!   challenge, and only then does the client try the query form.
//! - [`QueryFormNotes`]: what a client remembers when that redial upgraded,
//!   so its next dials to that endpoint go straight to the query form for
//!   [`QUERY_FORM_FOR`] instead of paying a refused dial each time.
//! - [`may_log`]: never a TRACE line of the crates that print a client's
//!   whole upgrade request, header and all ([`writes_the_request`]).
//!
//! Nothing here reads a credential's value beyond finding it: every audience,
//! tenant and device check stays with the upgrade that verifies it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Where an upgrade found its credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    /// `Authorization: Bearer <credential>`.
    Header,
    /// The URL query: `token=` on the control plane, `ticket=` at a relay.
    Query,
}

/// The realm every challenge names.
pub const REALM: &str = "roomler";

/// A 401 that found no credential in either carrier. A client whose header
/// was dropped on the way reads this as "try the query form".
pub const CHALLENGE: &str = "Bearer realm=\"roomler\"";

/// A 401 for a credential that arrived, in either carrier, and was refused.
pub const CHALLENGE_REFUSED: &str = "Bearer realm=\"roomler\", error=\"invalid_token\"";

/// A 400 answered after a credential was read.
pub const CHALLENGE_BAD_REQUEST: &str = "Bearer realm=\"roomler\", error=\"invalid_request\"";

/// The value of a well-formed `Authorization: Bearer <credential>`.
///
/// The scheme is case-insensitive and may be followed by any run of spaces
/// or tabs. Anything else (another scheme such as Basic, NTLM or Negotiate, or
/// an empty credential) is `None`, exactly as if no header had been sent: a
/// middlebox that adds its own `Authorization` must not take a released
/// client's query credential away from it.
pub fn bearer(authorization: Option<&str>) -> Option<&str> {
    let value = authorization?.trim();
    let (scheme, rest) = value.split_once([' ', '\t'])?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let credential = rest.trim();
    (!credential.is_empty()).then_some(credential)
}

/// The credential an upgrade is judged on, and its carrier.
///
/// A Bearer header wins, and the query is then neither read nor compared. A
/// bad header is therefore refused even when the query holds a good
/// credential: the query never rescues a header. Our clients never send both
/// (their fallback dial carries the query form alone), so two that disagree
/// are a bug or a hand-built request, and the header's verdict is the
/// predictable one. Without a header, the query's value exactly as before, an
/// empty one included: it fails verification like any other wrong credential.
pub fn presented<'a>(
    authorization: Option<&'a str>,
    query: Option<&'a str>,
) -> Option<(&'a str, Carrier)> {
    match bearer(authorization) {
        Some(credential) => Some((credential, Carrier::Header)),
        None => query.map(|q| (q, Carrier::Query)),
    }
}

/// The `WWW-Authenticate` a native upgrade's refusal with this status
/// carries. `presented` is whether a credential arrived in either carrier.
/// `None` for an answer that is not about the credential (a 403 for a `tid`
/// that names another tenant, a 503 for an unmounted module).
pub fn challenge_for(status: u16, presented: bool) -> Option<&'static str> {
    match (status, presented) {
        (401, false) => Some(CHALLENGE),
        (401, true) => Some(CHALLENGE_REFUSED),
        (400, true) => Some(CHALLENGE_BAD_REQUEST),
        _ => None,
    }
}

/// A client's reading of a refused header-only upgrade: should it dial once
/// more with the credential in the query?
///
/// Only a 400 or a 401 can mean so. A server older than the header answers a
/// dial with no query credential `401` and a relay older than it `400`, both
/// with no Bearer challenge. A Bearer challenge with no `error` says that
/// no credential arrived, so the header was dropped on the way. A Bearer
/// challenge that names an `error` is a verdict on the credential itself:
/// dialling again would only put it in a URL to hear the same answer.
/// `www_authenticate` is every value of that header in the answer.
pub fn asks_for_query_form<'a>(
    status: u16,
    www_authenticate: impl IntoIterator<Item = &'a str>,
) -> bool {
    if status != 400 && status != 401 {
        return false;
    }
    !www_authenticate
        .into_iter()
        .any(|value| bearer_verdict(value) == Some(true))
}

/// In one `WWW-Authenticate` value: `None` when it holds no Bearer challenge,
/// else whether that challenge names an `error`. The scheme counts only as a
/// whole token at the start of the value or after a comma, never inside a
/// quoted parameter. Only our own upgrades answer these dials with a Bearer
/// challenge, so an `error` anywhere after the scheme is taken as its own.
fn bearer_verdict(value: &str) -> Option<bool> {
    const SCHEME: &str = "bearer";
    let lower = value.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(SCHEME) {
        let at = from + i;
        let before = lower[..at].trim_end();
        let after = &lower[at + SCHEME.len()..];
        if (before.is_empty() || before.ends_with(','))
            && (after.is_empty() || after.starts_with([' ', '\t', ',']))
        {
            return Some(after.contains("error="));
        }
        from = at + SCHEME.len();
    }
    None
}

/// How long a client keeps dialling an endpoint in the query form after a
/// redial there upgraded where the header was refused. A server or relay
/// older than the header then costs one refused dial per endpoint every six
/// hours, and one updated meanwhile hears the header again the same day.
pub const QUERY_FORM_FOR: Duration = Duration::from_secs(6 * 60 * 60);

/// The endpoint a client's [`QueryFormNotes`] are keyed by: the URL before its
/// query or fragment, so `role` and `tid` never make one server two.
pub fn endpoint_of(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// A client's memory of the endpoints that took its credential only in the
/// query form, each for [`QUERY_FORM_FOR`] from when the redial upgraded.
///
/// A redial that was refused too notes nothing, so a credential refused in
/// both carriers keeps being offered in the header first. The `_at` forms
/// take the time, for tests.
#[derive(Debug, Default)]
pub struct QueryFormNotes {
    noted: Mutex<BTreeMap<String, Instant>>,
}

impl QueryFormNotes {
    pub const fn new() -> Self {
        Self {
            noted: Mutex::new(BTreeMap::new()),
        }
    }

    /// Where the next dial to `endpoint` carries the credential.
    pub fn carrier_for(&self, endpoint: &str) -> Carrier {
        self.carrier_for_at(endpoint, Instant::now())
    }

    pub fn carrier_for_at(&self, endpoint: &str, now: Instant) -> Carrier {
        let mut noted = self.noted.lock().unwrap_or_else(|e| e.into_inner());
        match noted.get(endpoint) {
            Some(at) if now.saturating_duration_since(*at) < QUERY_FORM_FOR => Carrier::Query,
            Some(_) => {
                noted.remove(endpoint);
                Carrier::Header
            }
            None => Carrier::Header,
        }
    }

    /// A redial of `endpoint` in the query form upgraded after its header
    /// was refused.
    pub fn note_query_form(&self, endpoint: &str) {
        self.note_query_form_at(endpoint, Instant::now());
    }

    pub fn note_query_form_at(&self, endpoint: &str, now: Instant) {
        self.noted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(endpoint.to_string(), now);
    }
}

/// This process's [`QueryFormNotes`], which every dial it makes shares.
pub fn query_form_notes() -> &'static QueryFormNotes {
    static NOTES: QueryFormNotes = QueryFormNotes::new();
    &NOTES
}

/// The log targets that write a client's whole upgrade request at TRACE.
/// tungstenite prints the request it sends, `Authorization` header included,
/// and marking the header's value sensitive does not hide it there. Every
/// native client's log filter drops their TRACE lines, whatever it was asked.
pub const WRITES_THE_REQUEST: [&str; 2] = ["tungstenite", "tokio_tungstenite"];

/// Whether an event's `target` belongs to [`WRITES_THE_REQUEST`]: one of those
/// crates or a module under it.
pub fn writes_the_request(target: &str) -> bool {
    WRITES_THE_REQUEST.iter().any(|krate| {
        target
            .strip_prefix(krate)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// A native client's log keeps a line only if this says so: never a TRACE line
/// of [`WRITES_THE_REQUEST`]. Each client's subscriber applies it as a global
/// filter (`tracing_subscriber::filter::filter_fn`) beside its `EnvFilter`, and
/// not as a directive inside it: a directive would either raise the level the
/// spec gives these crates or lose to a span-scoped TRACE directive, which
/// `EnvFilter` lets win over every target directive.
pub fn may_log(meta: &tracing::Metadata<'_>) -> bool {
    *meta.level() != tracing::Level::TRACE || !writes_the_request(meta.target())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_header_is_read_in_any_case_and_spacing() {
        assert_eq!(bearer(Some("Bearer abc.def.ghi")), Some("abc.def.ghi"));
        assert_eq!(bearer(Some("bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("BEARER   abc  ")), Some("abc"));
        assert_eq!(bearer(Some("Bearer\tabc")), Some("abc"));
        assert_eq!(bearer(Some("  Bearer abc")), Some("abc"));
    }

    #[test]
    fn anything_but_a_bearer_credential_is_as_if_no_header_came() {
        assert_eq!(bearer(None), None);
        assert_eq!(bearer(Some("")), None);
        assert_eq!(bearer(Some("Bearer")), None, "a scheme with no credential");
        assert_eq!(bearer(Some("Bearer    ")), None);
        assert_eq!(bearer(Some("Basic dXNlcjpwYXNz")), None);
        assert_eq!(bearer(Some("NTLM TlRMTVNTUAAB")), None);
        assert_eq!(bearer(Some("Negotiate YIIG")), None);
        assert_eq!(bearer(Some("Bearerabc")), None, "no space after the scheme");
    }

    #[test]
    fn the_header_wins_and_the_query_is_then_not_read() {
        assert_eq!(
            presented(Some("Bearer from-header"), Some("from-query")),
            Some(("from-header", Carrier::Header))
        );
        assert_eq!(
            presented(Some("Bearer from-header"), None),
            Some(("from-header", Carrier::Header))
        );
        // A refused header is not rescued by a query that would pass: the
        // query is not even returned.
        assert_eq!(
            presented(Some("Bearer garbage"), Some("good")),
            Some(("garbage", Carrier::Header))
        );
    }

    #[test]
    fn without_a_bearer_header_the_query_is_read_as_before() {
        assert_eq!(
            presented(None, Some("from-query")),
            Some(("from-query", Carrier::Query))
        );
        assert_eq!(
            presented(Some("Basic dXNlcjpwYXNz"), Some("from-query")),
            Some(("from-query", Carrier::Query)),
            "another scheme leaves the query form alone"
        );
        assert_eq!(
            presented(None, Some("")),
            Some(("", Carrier::Query)),
            "an empty query credential is still presented, and fails verification"
        );
        assert_eq!(presented(None, None), None);
        assert_eq!(presented(Some("Bearer "), None), None);
    }

    #[test]
    fn a_refusal_says_whether_a_credential_arrived() {
        assert_eq!(challenge_for(401, false), Some(CHALLENGE));
        assert_eq!(challenge_for(401, true), Some(CHALLENGE_REFUSED));
        assert_eq!(challenge_for(400, true), Some(CHALLENGE_BAD_REQUEST));
        assert_eq!(challenge_for(400, false), None);
        assert_eq!(challenge_for(403, true), None, "a tid for another tenant");
        assert_eq!(challenge_for(503, true), None, "an unmounted module");
        assert_eq!(challenge_for(101, true), None);
    }

    /// The client's half, against what each server generation answers.
    #[test]
    fn only_a_server_that_never_looked_for_the_header_gets_the_query_form() {
        // A server or relay older than the header: a bare 401 or 400.
        assert!(asks_for_query_form(401, []));
        assert!(asks_for_query_form(400, []));
        // A middlebox's own challenge is not ours.
        assert!(asks_for_query_form(401, ["NTLM"]));
        assert!(asks_for_query_form(401, ["Basic realm=\"bearer\""]));
        // A new server that saw no credential: the header was dropped.
        assert!(asks_for_query_form(401, [CHALLENGE]));
        // A new server's verdict on the credential: final.
        assert!(!asks_for_query_form(401, [CHALLENGE_REFUSED]));
        assert!(!asks_for_query_form(400, [CHALLENGE_BAD_REQUEST]));
        assert!(!asks_for_query_form(401, ["Negotiate", CHALLENGE_REFUSED]));
        assert!(!asks_for_query_form(
            401,
            ["Negotiate, Bearer realm=\"roomler\", error=\"invalid_token\""]
        ));
        assert!(!asks_for_query_form(
            401,
            ["bearer REALM=\"roomler\", ERROR=\"invalid_token\""]
        ));
        // Answers about something other than the credential never are.
        for status in [403, 404, 429, 500, 502, 503] {
            assert!(!asks_for_query_form(status, []), "{status}");
        }
    }

    #[test]
    fn every_challenge_we_send_reads_back_as_meant() {
        for (status, presented, falls_back) in
            [(401, false, true), (401, true, false), (400, true, false)]
        {
            let c = challenge_for(status, presented).unwrap();
            assert!(c.starts_with("Bearer realm=\"roomler\""), "{c}");
            assert_eq!(asks_for_query_form(status, [c]), falls_back, "{c}");
        }
        assert!(CHALLENGE.contains(REALM));
    }

    #[test]
    fn an_endpoint_is_the_url_before_its_query() {
        assert_eq!(
            endpoint_of("wss://roomler.ai/ws?role=agent&tid=t1"),
            "wss://roomler.ai/ws"
        );
        assert_eq!(
            endpoint_of("wss://relay.example/derp#x"),
            "wss://relay.example/derp"
        );
        assert_eq!(
            endpoint_of("wss://roomler.ai/derp"),
            "wss://roomler.ai/derp"
        );
    }

    /// The header first, the query form for six hours after a redial needed
    /// it, then the header again, and one endpoint's note is its own.
    #[test]
    fn a_noted_endpoint_takes_the_query_form_for_six_hours() {
        let notes = QueryFormNotes::new();
        let t0 = Instant::now();
        assert_eq!(notes.carrier_for_at("wss://a/ws", t0), Carrier::Header);
        notes.note_query_form_at("wss://a/ws", t0);
        assert_eq!(notes.carrier_for_at("wss://a/ws", t0), Carrier::Query);
        let almost = t0 + QUERY_FORM_FOR - Duration::from_secs(1);
        assert_eq!(notes.carrier_for_at("wss://a/ws", almost), Carrier::Query);
        assert_eq!(
            notes.carrier_for_at("wss://a/derp", almost),
            Carrier::Header
        );
        assert_eq!(notes.carrier_for_at("wss://b/ws", almost), Carrier::Header);
        let over = t0 + QUERY_FORM_FOR;
        assert_eq!(notes.carrier_for_at("wss://a/ws", over), Carrier::Header);
        assert_eq!(
            notes.carrier_for_at("wss://a/ws", almost),
            Carrier::Header,
            "an expired note is gone, not hidden"
        );
        notes.note_query_form_at("wss://a/ws", over);
        assert_eq!(notes.carrier_for_at("wss://a/ws", over), Carrier::Query);
    }

    #[test]
    fn only_the_websocket_crates_count_as_writing_the_request() {
        for target in [
            "tungstenite",
            "tungstenite::handshake::client",
            "tokio_tungstenite",
            "tokio_tungstenite::compat",
        ] {
            assert!(writes_the_request(target), "{target}");
        }
        for target in [
            "tungstenite_x",
            "tokio",
            "roomlerd::ws_connect",
            "",
            "tunnel_core",
        ] {
            assert!(!writes_the_request(target), "{target}");
        }
    }
}
