// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-133 — every needs-attention notice the daemon raises calls it `roomlerd`
//! (or "the daemon"), never an agent, and shows a release as its version.
//!
//! The companion's "Attention required" banner, an administrator and a fleet
//! scanner all read these files. Each notice is rendered the way the daemon
//! renders it, written through the one sentinel writer into a temporary folder
//! and read back whole. Never the real sentinel: a test that wrote it once sat
//! in the companion's banner for good (S1b).
//!
//! A goodbye and a duel carry the server's own words inside them. Those are the
//! server's to change (FR-133 P2b, a promote), so the message fed here is
//! neutral.

use crate::notify;
use crate::service::Install;
use roomler_ai_remote_control::signaling::AgentCloseReason;

/// The server's words, which these tests do not judge.
const NEUTRAL: &str = "The server sent this explanation.";

const REASONS: [AgentCloseReason; 3] = [
    AgentCloseReason::AgentDeleted,
    AgentCloseReason::PolicyRejected,
    AgentCloseReason::ReplacedByNewerConnection,
];

const INSTALLS: [Install; 6] = [
    Install::LinuxSystem,
    Install::LinuxUser,
    Install::WindowsService,
    Install::WindowsTask,
    Install::MacOs,
    Install::Other,
];

/// `agent` or `agents`, any case, with no letter, digit or `_` on either side.
/// So `agent_id` (a config key) is not the word, while `agent-v0.4.1` (a tag)
/// and `rc:agent.update` (a wire name) are: either one leaking into prose is
/// caught.
pub(crate) fn says_agent(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    lower.match_indices("agent").any(|(at, _)| {
        let after = &lower[at + 5..];
        let after = after.strip_prefix('s').unwrap_or(after);
        !word(lower[..at].chars().next_back()) && !word(after.chars().next())
    })
}

/// The file a notice becomes once the daemon's one writer has written it.
fn written(reason: &str, notice: &str, failed_version: Option<&str>) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let path = notify::raise_attention_at_full(tmp.path(), reason, notice, failed_version).unwrap();
    std::fs::read_to_string(path).unwrap()
}

/// Every notice the daemon raises under a heading of its own, labelled, with
/// the reason code and the accused build it is written with.
fn every_notice() -> Vec<(String, &'static str, Option<&'static str>, String)> {
    use crate::app::{crash_loop_no_rollback_msg, rollback_attention_msg};
    use crate::signaling::{AUTH_NOTICE, goodbye_notice, replaced_notice};
    let rollback = notify::REASON_ROLLBACK;
    let mut all = Vec::new();
    // A pinned rollback's reasons name the release the way the updater asks
    // for it: by its tag.
    for why in [
        None,
        Some("no platform installer in release agent-v0.4.231"),
        Some("pin fetch agent-v0.4.231: the server did not answer"),
        Some("automatic install failed: the installer did not start"),
    ] {
        let text = rollback_attention_msg("0.4.232", "0.4.231", 3, why);
        all.push((
            format!("rollback, {why:?}"),
            rollback,
            Some("0.4.232"),
            text,
        ));
    }
    let text = crash_loop_no_rollback_msg("0.4.232", "0.4.231", 3);
    all.push(("crash loop".into(), rollback, Some("0.4.232"), text));
    all.push((
        "re-enrol".into(),
        notify::REASON_AUTH,
        None,
        AUTH_NOTICE.into(),
    ));
    for reason in REASONS {
        for install in INSTALLS {
            let text = goodbye_notice(reason, NEUTRAL, install.after_removal());
            let label = format!("goodbye, {reason:?}, {install:?}");
            all.push((label, notify::REASON_GOODBYE, None, text));
        }
    }
    let text = replaced_notice(NEUTRAL, 3);
    all.push(("duel".into(), notify::REASON_DUPLICATE, None, text));
    all
}

/// The heading names the daemon by its one name, and no line calls it an
/// agent: not the heading, not the body, not a reason, not a release.
#[test]
fn every_notice_names_roomlerd_and_never_an_agent() {
    let mut wrong = Vec::new();
    for (label, reason, failed_version, text) in every_notice() {
        let file = written(reason, &text, failed_version);
        if !file.starts_with("roomlerd: ") {
            wrong.push(format!("{label}: its heading is not `roomlerd:`\n{file}"));
        }
        if says_agent(&file) {
            wrong.push(format!("{label}: it says agent\n{file}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
}

/// A goodbye says in words why the server closed: the enum's own name
/// (`AgentDeleted`) is a wire value, and `\bagent\b` alone would let it by.
#[test]
fn a_goodbye_says_why_in_words_not_the_wire_name() {
    use crate::signaling::goodbye_notice;
    let mut said = Vec::new();
    for reason in REASONS {
        let text = goodbye_notice(reason, NEUTRAL, Install::Other.after_removal());
        let heading = text.lines().next().unwrap_or_default().to_string();
        assert!(!heading.contains(&format!("{reason:?}")), "{heading}");
        said.push(heading);
    }
    said.sort();
    said.dedup();
    assert_eq!(
        said.len(),
        REASONS.len(),
        "each reason reads differently: {said:?}"
    );
}

/// The release a pinned rollback names reaches the file as its version.
#[test]
fn a_rollback_names_the_release_by_its_version() {
    let why = Some("no platform installer in release agent-v0.4.231");
    let text = crate::app::rollback_attention_msg("0.4.232", "0.4.231", 3, why);
    let file = written(notify::REASON_ROLLBACK, &text, Some("0.4.232"));
    assert!(
        file.contains("no platform installer in release 0.4.231"),
        "{file}"
    );
    assert!(!file.contains("agent-v"), "{file}");
}

/// Every string literal in `code` (comments skipped), and whether a log macro
/// (`trace!` … `error!`) holds it: a log line is engineering text, not this
/// FR's (FR-133, out of scope).
pub(crate) fn literals(code: &str) -> Vec<(&str, bool)> {
    const LOG: [&str; 5] = ["trace!", "debug!", "info!", "warn!", "error!"];
    let b = code.as_bytes();
    let (mut out, mut i, mut depth, mut log_at) = (Vec::new(), 0, 0usize, None);
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = code[i..].find('\n').map_or(b.len(), |n| i + n);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = code[i + 2..].find("*/").map_or(b.len(), |n| i + n + 4);
            }
            b'(' | b'[' | b'{' => {
                if log_at.is_none() && LOG.iter().any(|m| code[..i].ends_with(m)) {
                    log_at = Some(depth);
                }
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                if log_at == Some(depth) {
                    log_at = None;
                }
                i += 1;
            }
            // A char literal (`'"'`, `'\''`) or a lifetime (`'a`).
            b'\'' => {
                if b.get(i + 1) == Some(&b'\\') {
                    i = code[i + 3..].find('\'').map_or(b.len(), |n| i + n + 4);
                } else {
                    let c = code[i + 1..].chars().next().map_or(1, char::len_utf8);
                    i += if b.get(i + 1 + c) == Some(&b'\'') {
                        c + 2
                    } else {
                        1
                    };
                }
            }
            // A raw string: `r"…"`, `r#"…"#`.
            b'r' if matches!(b.get(i + 1), Some(b'"' | b'#'))
                && !(i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) =>
            {
                let hashes = code[i + 1..].bytes().take_while(|&c| c == b'#').count();
                let open = i + 1 + hashes;
                if b.get(open) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close = format!("\"{}", "#".repeat(hashes));
                let end = code[open + 1..]
                    .find(&close)
                    .map_or(b.len(), |n| open + 1 + n);
                out.push((&code[open + 1..end], log_at.is_some()));
                i = end + close.len();
            }
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                out.push((&code[start..i.min(b.len())], log_at.is_some()));
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// The reasons the updater hands a notice: a pinned rollback's ("could not
/// run: …"), a failed self-update's (`{e}`). Every prose literal of
/// `updater.rs` outside its log lines; a wire spelling keeps its form (a tag's
/// `agent-v`, the verb `rc:agent.update`, the macOS `com.roomler.agent`).
#[test]
fn no_reason_the_updater_hands_a_notice_says_agent() {
    let src = include_str!("updater.rs").replace("\r\n", "\n");
    let code = src
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .unwrap_or("");
    let said: Vec<&str> = literals(code)
        .into_iter()
        .filter(|(text, log)| !log && text.contains(' '))
        .map(|(text, _)| text)
        .filter(|text| {
            let prose = text
                .replace("rc:agent.", "rc:")
                .replace("agent-v", "v")
                .replace("com.roomler.agent", "com.roomler");
            says_agent(&prose)
        })
        .collect();
    assert!(said.is_empty(), "updater.rs reasons say agent: {said:#?}");
}

/// The lexer itself, on the shapes it has to get right.
#[test]
fn the_literal_lexer_reads_rust() {
    let code = r##"
        let a = '"'; let b = '\''; fn f<'a>(x: &'a str) {}
        // "a comment"
        /* "a block" */
        tracing::warn!(n = 1, "a log line {}", format!("inner"));
        bail!("an error, {e}");
        let r = r#"raw "quoted""#;
    "##;
    let got = literals(code);
    let texts: Vec<&str> = got.iter().map(|(t, _)| *t).collect();
    assert_eq!(
        texts,
        ["a log line {}", "inner", "an error, {e}", r#"raw "quoted""#]
    );
    let logs: Vec<bool> = got.iter().map(|(_, log)| *log).collect();
    assert_eq!(logs, [true, true, false, false]);
}
