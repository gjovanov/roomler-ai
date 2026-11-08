// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A device's diagnostics: its crash reports (`…/agent/{aid}/crash`) and its
//! uploaded logs (`…/agent/{aid}/logs`). Who may read them, and the server's
//! second scrub of what a device sends.
//!
//! **Who reads them.** A `MANAGE_AGENTS` holder in the path org, for a device
//! of that org ([`require_reader`]). The ordinary device list needs only
//! membership; these are the least-filtered view of a device, so they sit with
//! the connect code behind the bit an org gives the people who manage its
//! devices.
//!
//! **What is stored.** The device scrubs first: the crash recorder before it
//! writes a report (`roomler_node_core::crash_recorder::scrub_credentials`),
//! and a config file's parse error names where the file broke, never what it
//! held (#3086). The server scrubs again at ingest, because a device that
//! predates a device-side fix sends what it had, and a future build can log a
//! secret by mistake. So the pass covers every string a log line carries
//! (`msg`, `target`, each string in `fields` at any depth) and every string of
//! a crash report, and it knows these shapes:
//!
//! | Shape | Recognised as | Stored as |
//! |---|---|---|
//! | the source line a `toml` parse error quotes | the `N \| …` line under a `TOML parse error at line N` header | `N \| [REDACTED_TOML_LINE]` |
//! | a JWT | `eyJ…` with two dots, 64 characters or more | `[REDACTED_JWT]` |
//! | a bearer credential | `Bearer ` (or `bearer `) and 8 or more token characters | `[REDACTED_BEARER]` |
//! | a WireGuard key, public or private | exactly 32 bytes of standard base64 standing alone: 43 characters, the last one a character that can end 32 bytes, then one `=` | `[REDACTED_KEY]` |
//!
//! Each rule is tight on purpose, so a value that only looks like base64 is
//! stored as it came: a hex digest, an ObjectId, a longer or shorter base64
//! value, `==` padding, a run inside a longer one, the word "Bearer" without a
//! credential after it. A public key is redacted with a private one, because
//! the shape cannot tell them apart; the server knows the public keys anyway.

use std::borrow::Cow;

use bson::{Bson, Document, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_core::{ApiError, guards::require_permission};

use crate::FleetState;

/// Who may read a device's crash reports and uploaded logs: a `MANAGE_AGENTS`
/// holder in the path org, for a device of that org.
///
/// 1. The bit FIRST, before the device is looked up, as the connect code does.
///    A caller without it gets one 403 whatever the id names, so the refusal
///    says nothing about which devices exist. A caller outside the org is told
///    `not_a_member` by the same call (`get_member_permissions`), as before.
/// 2. Then the device, WITHIN the tenant and ANY rather than live (#1821):
///    another org's id, or one that names nothing, is a 404, and a removed
///    device still reads. These are audit views, and removing a device must
///    not be the way to hide what it reported. Nothing of the row is returned.
pub(crate) async fn require_reader(
    state: &FleetState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    user_id: ObjectId,
) -> Result<(), ApiError> {
    require_permission(
        state,
        tenant_id,
        user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let _ = state.agents.find_any_in_tenant(tenant_id, agent_id).await?;
    Ok(())
}

/// Scrub one string in place (see the module docs for what it recognises).
pub(crate) fn scrub_string(s: &mut String) {
    let scrubbed = match scrub_text(s) {
        Cow::Borrowed(_) => return,
        Cow::Owned(t) => t,
    };
    *s = scrubbed;
}

/// Scrub every string in a document, at any depth: nested documents and
/// arrays included. Keys and non-string values are left as they are.
pub(crate) fn scrub_document(doc: &mut Document) {
    for (_, value) in doc.iter_mut() {
        scrub_bson(value);
    }
}

fn scrub_bson(value: &mut Bson) {
    match value {
        Bson::String(s) => scrub_string(s),
        Bson::Document(d) => scrub_document(d),
        Bson::Array(items) => items.iter_mut().for_each(scrub_bson),
        _ => {}
    }
}

/// The scrub itself. Borrowed when nothing matched, which is nearly always.
pub(crate) fn scrub_text(s: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(s);
    for pass in [toml_source_lines, jwts, bearer_credentials, wireguard_keys] {
        if let Some(scrubbed) = pass(&out) {
            out = Cow::Owned(scrubbed);
        }
    }
    out
}

// ── The source line a `toml` parse error quotes ────────────────────────────

const TOML_HEADER: &str = "TOML parse error at line ";
const TOML_LINE: &str = "[REDACTED_TOML_LINE]";
/// How far after its header the quoted line may start. `toml` prints the rest
/// of the header (`, column M`) and a gutter line (`  |`) in between, a few
/// dozen bytes; anything further away is not the line this header quotes.
const TOML_QUOTE_WINDOW: usize = 128;

/// `toml`'s error quotes the line it stopped at, whatever that line held:
///
/// ```text
/// TOML parse error at line 7, column 71
///   |
/// 7 | overlay_wg_secret_key = "…"x
///   |                                                                       ^
/// expected newline, `#`
/// ```
///
/// The location and the reason stay; the quoted line goes. Found by the line
/// number its own header names, as the start of a line, so nothing else in the
/// string is touched. A field logged with `?` holds the same text with its
/// newlines escaped (`\n`), and that form is read too.
fn toml_source_lines(s: &str) -> Option<String> {
    if !s.contains(TOML_HEADER) {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut copied = 0; // `s[..copied]` is in `out`
    let mut search = 0;
    while let Some(rel) = s[search..].find(TOML_HEADER) {
        let num_start = search + rel + TOML_HEADER.len();
        let num_len = s[num_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        search = num_start + num_len;
        if num_len == 0 {
            continue;
        }
        let num = &s[num_start..num_start + num_len];
        let plain = format!("\n{num} | ");
        let escaped = format!("\\n{num} | ");
        let quoted = [(plain.as_str(), false), (escaped.as_str(), true)]
            .into_iter()
            .filter_map(|(needle, is_escaped)| {
                s[search..]
                    .find(needle)
                    .filter(|at| *at <= TOML_QUOTE_WINDOW)
                    .map(|at| (at + needle.len(), is_escaped))
            })
            .min_by_key(|(at, _)| *at);
        let Some((at, is_escaped)) = quoted else {
            continue;
        };
        let line_start = search + at;
        let line_end = line_start + quoted_line_len(&s[line_start..], is_escaped);
        out.push_str(&s[copied..line_start]);
        out.push_str(TOML_LINE);
        copied = line_end;
        search = line_end;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&s[copied..]);
    Some(out)
}

/// The length of the quoted line: up to its newline, or, in the escaped form,
/// up to a `\n` escape. An escaped backslash (`\\`) is skipped as a pair, so a
/// `\\n` in the source does not end the line early.
fn quoted_line_len(rest: &str, escaped: bool) -> usize {
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => return i,
            b'\\' if escaped => {
                if b.get(i + 1) == Some(&b'n') {
                    return i;
                }
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

// ── JWTs ───────────────────────────────────────────────────────────────────

const JWT: &str = "[REDACTED_JWT]";

/// Three base64url segments starting `eyJ` (a JSON header), two dots, 64
/// characters or more: a real JWT is 200 or more, and a short `eyJ…` is often
/// not one.
fn jwts(s: &str) -> Option<String> {
    if !s.contains("eyJ") {
        return None;
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i + 3 <= b.len() {
        if &b[i..i + 3] != b"eyJ" {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        let mut dots = 0;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'.' | b'_' | b'-')) {
            dots += usize::from(b[j] == b'.');
            j += 1;
        }
        if dots >= 2 && j - i >= 64 {
            out.push_str(&s[copied..i]);
            out.push_str(JWT);
            copied = j;
        }
        i = j;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&s[copied..]);
    Some(out)
}

// ── Bearer credentials ─────────────────────────────────────────────────────

const BEARER: &str = "[REDACTED_BEARER]";
/// Shorter than this after `Bearer ` is a word, not a credential ("missing
/// Bearer token").
const BEARER_MIN: usize = 8;

/// `Bearer <credential>`: the RFC 6750 token characters (letters, digits,
/// `-._~+/`, trailing `=`), at least [`BEARER_MIN`] of them. A shorter run is
/// left exactly as it is.
fn bearer_credentials(s: &str) -> Option<String> {
    if !s.contains("earer ") {
        return None;
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut search = 0;
    while let Some(at) = next_bearer(s, search) {
        let token_start = at + "Bearer ".len();
        let token_len = b[token_start..]
            .iter()
            .take_while(|&&c| is_token_char(c))
            .count();
        if token_len >= BEARER_MIN {
            out.push_str(&s[copied..at]);
            out.push_str(BEARER);
            copied = token_start + token_len;
        }
        search = token_start + token_len;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&s[copied..]);
    Some(out)
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
}

/// The next `Bearer ` or `bearer ` at or after `from`.
fn next_bearer(s: &str, from: usize) -> Option<usize> {
    let rest = &s[from..];
    [rest.find("Bearer "), rest.find("bearer ")]
        .into_iter()
        .flatten()
        .min()
        .map(|at| from + at)
}

// ── WireGuard keys ─────────────────────────────────────────────────────────

const KEY: &str = "[REDACTED_KEY]";

fn is_base64(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'+' || c == b'/'
}

/// 32 bytes are 256 bits; the 43rd base64 character carries the last 4 of them
/// and 2 zero bits, so only a character whose index is a multiple of 4 can end
/// a canonical encoding (`A E I M Q U Y c g k o s w 0 4 8`).
fn ends_32_bytes(c: u8) -> bool {
    let index = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return false,
    };
    index % 4 == 0
}

/// A WireGuard key is 32 bytes in standard base64: exactly 43 alphabet
/// characters, the last one able to end 32 bytes, then a single `=`, with no
/// alphabet character on either side. A run directly after an escape (`\n`,
/// `\t`, `\r`, in a field logged with `?`) starts after the escape.
fn wireguard_keys(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 44 || !s.contains('=') {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        if !is_base64(b[i]) {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < b.len() && is_base64(b[i]) {
            i += 1;
        }
        // `i` is the first byte after the run. An escape letter glued to the
        // front belongs to the escape, not to the key.
        let start = if i - run_start == 44
            && run_start > 0
            && b[run_start - 1] == b'\\'
            && matches!(b[run_start], b'n' | b't' | b'r')
        {
            run_start + 1
        } else {
            run_start
        };
        let is_key = i - start == 43
            && ends_32_bytes(b[i - 1])
            && b.get(i) == Some(&b'=')
            && !b.get(i + 1).is_some_and(|c| is_base64(*c) || *c == b'=');
        if is_key {
            out.push_str(&s[copied..start]);
            out.push_str(KEY);
            copied = i + 1;
            i += 1;
        }
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&s[copied..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes 0x00..=0x1f in standard base64.
    const WG_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    const JWT_VALUE: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMiLCJpYXQiOjE1MTYyMzkwMjJ9.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";

    fn scrubbed(s: &str) -> String {
        scrub_text(s).into_owned()
    }

    #[test]
    fn a_wireguard_key_is_redacted_wherever_it_stands_alone() {
        for (input, want) in [
            (WG_KEY.to_string(), KEY.to_string()),
            (format!("key={WG_KEY}"), format!("key={KEY}")),
            (format!("\"{WG_KEY}\""), format!("\"{KEY}\"")),
            (
                format!("peer {WG_KEY}, rotated"),
                format!("peer {KEY}, rotated"),
            ),
            (format!("a\n{WG_KEY}\nb"), format!("a\n{KEY}\nb")),
            // Logged with `?`: escaped quotes and newlines around it.
            (format!("\\\"{WG_KEY}\\\""), format!("\\\"{KEY}\\\"")),
            (format!("x\\n{WG_KEY}\\ny"), format!("x\\n{KEY}\\ny")),
            (format!("{WG_KEY} {WG_KEY}"), format!("{KEY} {KEY}")),
        ] {
            assert_eq!(scrubbed(&input), want, "{input}");
        }
    }

    #[test]
    fn what_only_looks_like_base64_is_left_alone() {
        let non_canonical = WG_KEY.replace("8=", "9="); // 9 cannot end 32 bytes
        let thirty_one_bytes = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHg==";
        let longer = format!("Q{WG_KEY}");
        let shorter = &WG_KEY[1..];
        let unpadded = &WG_KEY[..43];
        let followed = format!("{WG_KEY}x");
        let double = format!("{WG_KEY}=");
        for kept in [
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "6a1f0c2e9b3d4a5f6e7d8c9b",
            "203.0.113.7:41641",
            "c2VjcmV0IHNhdWNl", // a short base64 value
            non_canonical.as_str(),
            thirty_one_bytes,
            longer.as_str(),
            shorter,
            unpadded,
            followed.as_str(),
            double.as_str(),
            "missing Bearer token",
            "saw eyJabc not a real jwt",
            "plain words, no secrets = none here",
        ] {
            assert!(
                matches!(scrub_text(kept), Cow::Borrowed(_)),
                "changed: {kept}"
            );
        }
    }

    #[test]
    fn jwts_and_bearer_credentials_are_redacted() {
        assert_eq!(
            scrubbed(&format!("token {JWT_VALUE} ok")),
            format!("token {JWT} ok")
        );
        assert_eq!(
            scrubbed("header was: Bearer somelongtokenvaluehere1234567890"),
            format!("header was: {BEARER}")
        );
        assert_eq!(scrubbed("bearer abc+def/ghi=="), BEARER);
        // A JWT behind `Bearer ` goes whichever rule takes it first.
        let both = scrubbed(&format!("Authorization: Bearer {JWT_VALUE}"));
        assert!(!both.contains("eyJ0eXAi"), "{both}");
        // A short run after the word is a word: kept, never mangled.
        assert_eq!(scrubbed("Bearer abc"), "Bearer abc");
    }

    fn toml_error(source_line: &str) -> String {
        format!(
            "parse /etc/roomler/config.toml: TOML parse error at line 7, column 71\n  |\n7 | {source_line}\n  |{}^\nexpected newline, `#`\n",
            " ".repeat(71)
        )
    }

    #[test]
    fn the_line_a_toml_error_quotes_is_redacted_and_the_rest_kept() {
        let input = toml_error("agent_token = \"not-a-jwt-but-still-secret\"x");
        let out = scrubbed(&input);
        assert!(!out.contains("not-a-jwt"), "{out}");
        assert!(!out.contains("agent_token"), "{out}");
        assert!(
            out.contains(
                "TOML parse error at line 7, column 71\n  |\n7 | [REDACTED_TOML_LINE]\n  |"
            ),
            "{out}"
        );
        assert!(out.ends_with("^\nexpected newline, `#`\n"), "{out}");
        // A multi-line string's body line is no shape at all: the line rule is
        // what takes it.
        let pem = scrubbed(&toml_error(
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW",
        ));
        assert!(!pem.contains("b3BlbnNzaC1rZXkt"), "{pem}");
    }

    #[test]
    fn the_escaped_form_of_a_toml_error_is_read_too() {
        // What `?err` prints: one line, `\n` and `\"` escaped.
        let input = format!(
            "TOML parse error at line 3, column 9\\n  |\\n3 | ssh_host_key = \\\"a\\\\nb{WG_KEY}\\\"\\n  |         ^\\ninvalid escape\\n"
        );
        let out = scrubbed(&input);
        assert!(!out.contains("ssh_host_key"), "{out}");
        assert!(!out.contains(WG_KEY), "{out}");
        assert!(
            out.starts_with(
                "TOML parse error at line 3, column 9\\n  |\\n3 | [REDACTED_TOML_LINE]\\n  |"
            ),
            "{out}"
        );
        assert!(out.ends_with("invalid escape\\n"), "{out}");
    }

    #[test]
    fn a_toml_error_only_loses_the_line_its_header_names() {
        // A crash tail: an unrelated `N | ` line elsewhere, two errors.
        let input = format!(
            "12 | an ordinary log line\n{}between\n{}",
            toml_error("first = \"secret-one\"x"),
            toml_error("second = \"secret-two\"x"),
        );
        let out = scrubbed(&input);
        assert!(out.starts_with("12 | an ordinary log line\n"), "{out}");
        assert!(
            !out.contains("secret-one") && !out.contains("secret-two"),
            "{out}"
        );
        assert_eq!(out.matches(TOML_LINE).count(), 2, "{out}");
        // A header with no quoted line after it changes nothing.
        let bare = "TOML parse error at line 4, column 1\nunexpected end of input";
        assert!(matches!(scrub_text(bare), Cow::Borrowed(_)));
    }

    #[test]
    fn a_document_is_scrubbed_at_every_depth_and_only_its_strings() {
        let mut doc = bson::doc! {
            "error": format!("key {WG_KEY}"),
            "nested": { "auth": format!("Bearer {JWT_VALUE}"), "list": [WG_KEY, "kept", 7_i32] },
            "n": 3_i64,
            "ok": true,
        };
        scrub_document(&mut doc);
        assert_eq!(doc.get_str("error"), Ok("key [REDACTED_KEY]"));
        let nested = doc.get_document("nested").unwrap();
        assert!(!nested.get_str("auth").unwrap().contains("eyJ0eXAi"));
        let list = nested.get_array("list").unwrap();
        assert_eq!(list[0].as_str(), Some(KEY));
        assert_eq!(list[1].as_str(), Some("kept"));
        assert_eq!(list[2], Bson::Int32(7));
        assert_eq!(doc.get_i64("n"), Ok(3));
        assert_eq!(doc.get_bool("ok"), Ok(true));
    }

    #[test]
    fn a_multibyte_text_survives_the_byte_scans() {
        let input = format!("config unreadable — RECOVERED: ключ {WG_KEY} — ok");
        assert_eq!(
            scrubbed(&input),
            format!("config unreadable — RECOVERED: ключ {KEY} — ok")
        );
    }
}
