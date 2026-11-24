// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-133 P2c — what the daemon says to a person outside a notice calls it
//! `roomlerd` (or "the daemon"), never an agent: the sentences the web viewer
//! shows (a banner, a refusal), what the CLI and LocalAPI print, the reasons a
//! tunnel client is refused with, and every subcommand's `--help` of both
//! binaries (`roomlerd`, and the `roomler` CLI).
//!
//! The sentences are read from the source with P2a's lexer
//! ([`literals`]), so a file that compiles only on Linux or only on a Mac is
//! read on every OS. A wire name, a route, an OS identity, a release tag, a
//! CLI flag and a retired-name input keep their spelling (FR-133's key
//! design): [`KEPT`] takes those out before the check, and nothing else.

use crate::notice_text_tests::{literals, says_agent};
use clap::CommandFactory;

/// The spellings that stay, each replaced by what is left once the word is
/// out. Order matters: a route's `/api/agent` goes before `/agent/`.
const KEPT: [(&str, &str); 11] = [
    // The wire's verbs (`rc:agent.hello`) and the `/ws` role.
    ("rc:agent.", "rc:"),
    ("role=agent", "role="),
    // Routes.
    ("/api/tunnel-client/agents", "/api/tunnel-client/"),
    ("/api/agent", "/api/"),
    ("/agent/", "/"),
    // The release tags every deployed updater polls (D3).
    ("agent-v", "v"),
    // The macOS bundle id and launchd label (an OS identity).
    ("com.roomler.agent", "com.roomler"),
    // RETIRED-NAME-ANCHOR: FR-46's retired names, which the daemon still reads as inputs.
    ("roomler-agent", "roomler"),
    // The CLI's flag, and the placeholder clap derives from its field.
    ("--agent", "--"),
    ("<AGENT>", "<>"),
    // A format argument's name, not a word.
    ("{agent}", "{}"),
];

/// `text` with the kept spellings taken out.
fn prose(text: &str) -> String {
    KEPT.iter()
        .fold(text.to_string(), |text, (from, to)| text.replace(from, to))
}

/// Sentences that say "agent" and reach only a log line, never a person:
/// `AgentDidNotRun`'s text, which the reconnect loop logs and nothing else
/// reads (`signaling.rs`, where it is read back by its type).
const LOG_ONLY: [(&str, &str); 2] = [
    (
        "signaling.rs",
        "{ctx}: {}: this agent did not run for ~{} s before it (frozen or suspended)",
    ),
    (
        "signaling.rs",
        "{why}: this agent did not run for ~{} s of it (frozen or suspended)",
    ),
];

/// The viewer's sentences: each file that writes one, as `include_str!`
/// reads it.
const VIEWER: [(&str, &str); 12] = [
    ("audio_source.rs", include_str!("audio_source.rs")),
    ("capture/mod.rs", include_str!("capture/mod.rs")),
    ("display_match.rs", include_str!("display_match.rs")),
    ("encode/caps.rs", include_str!("encode/caps.rs")),
    ("files.rs", include_str!("files.rs")),
    ("input/reach.rs", include_str!("input/reach.rs")),
    ("keep_busy/engine.rs", include_str!("keep_busy/engine.rs")),
    ("peer.rs", include_str!("peer.rs")),
    ("virtual_desktop.rs", include_str!("virtual_desktop.rs")),
    ("apps/linux.rs", include_str!("apps/linux.rs")),
    ("apps/macos.rs", include_str!("apps/macos.rs")),
    ("apps/mod.rs", include_str!("apps/mod.rs")),
];

/// What the CLI and LocalAPI print, and the refusals a tunnel client prints:
/// `roomlerd`'s files, the `roomler` CLI's, and the node-core ACL whose
/// reasons the daemon's tunnel acceptor sends.
const OUTPUT: [(&str, &str); 11] = [
    ("service.rs", include_str!("service.rs")),
    ("app.rs", include_str!("app.rs")),
    ("probe_target.rs", include_str!("probe_target.rs")),
    ("tunnel/acceptor.rs", include_str!("tunnel/acceptor.rs")),
    ("tunnel/client_mgr.rs", include_str!("tunnel/client_mgr.rs")),
    ("one_row.rs", include_str!("one_row.rs")),
    ("signaling.rs", include_str!("signaling.rs")),
    (
        "roomler-cli cli.rs",
        include_str!("../../roomler-cli/src/cli.rs"),
    ),
    (
        "roomler-cli mesh.rs",
        include_str!("../../roomler-cli/src/mesh.rs"),
    ),
    (
        "roomler-cli sshcmd.rs",
        include_str!("../../roomler-cli/src/sshcmd.rs"),
    ),
    (
        "node-core acl.rs",
        include_str!("../../../crates/agent-core/src/acl.rs"),
    ),
];

/// Every sentence of `files` that says agent: a literal outside a log macro
/// and outside the file's `mod tests`, holding a space (so a name or a path
/// alone is not a sentence), once the kept spellings are out. Each with its
/// file and its first line.
fn says_agent_in(files: &[(&str, &str)]) -> Vec<String> {
    let mut said = Vec::new();
    for (name, src) in files {
        let src = src.replace("\r\n", "\n");
        let code = src
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or("");
        for (text, log) in literals(code) {
            if log || !text.contains(' ') {
                continue;
            }
            if LOG_ONLY.contains(&(*name, text)) || !says_agent(&prose(text)) {
                continue;
            }
            let at = text.as_ptr() as usize - code.as_ptr() as usize;
            let line = code[..at].matches('\n').count() + 1;
            said.push(format!("{name}:{line}: {text}"));
        }
    }
    said
}

/// The web viewer shows these as the device wrote them: a banner under a
/// screen it cannot show, a sound it cannot send, a file it refuses, a
/// window it cannot list.
#[test]
fn the_viewer_text_says_roomlerd() {
    let said = says_agent_in(&VIEWER);
    assert!(
        said.is_empty(),
        "the viewer's text says agent:\n{}",
        said.join("\n")
    );
}

/// The CLI's and LocalAPI's output: `roomlerd enroll` and its re-enrollment,
/// `service install`, the one-row move, a tunnel client's refusals, the
/// `roomler` CLI's errors.
#[test]
fn the_cli_and_localapi_output_says_roomlerd() {
    let said = says_agent_in(&OUTPUT);
    assert!(
        said.is_empty(),
        "the output says agent:\n{}",
        said.join("\n")
    );
}

/// Each sentence [`LOG_ONLY`] lets by is still in its file, word for word,
/// so the list never lets by a sentence that has gone.
#[test]
fn every_log_only_sentence_is_still_there() {
    for (name, text) in LOG_ONLY {
        let (_, src) = OUTPUT.iter().find(|(n, _)| *n == name).unwrap();
        let src = src.replace("\r\n", "\n");
        assert!(
            literals(&src).iter().any(|(t, _)| *t == text),
            "{name} no longer holds {text:?}"
        );
    }
}

/// Every subcommand's long help, as `<bin> <subcommand> --help` prints it,
/// hidden subcommands included.
fn every_long_help(mut cmd: clap::Command) -> Vec<(String, String)> {
    fn walk(cmd: &mut clap::Command, path: String, out: &mut Vec<(String, String)>) {
        out.push((path.clone(), cmd.render_long_help().to_string()));
        for sub in cmd.get_subcommands_mut() {
            let path = format!("{path} {}", sub.get_name());
            walk(sub, path, out);
        }
    }
    cmd.build();
    let name = cmd.get_name().to_string();
    let mut out = Vec::new();
    walk(&mut cmd, name, &mut out);
    out
}

/// Each line of a help that says agent once the kept spellings are out.
fn help_says_agent(cmd: clap::Command) -> Vec<String> {
    let mut said = Vec::new();
    for (path, help) in every_long_help(cmd) {
        for line in help.lines() {
            if says_agent(&prose(line)) {
                said.push(format!("{path}: {}", line.trim()));
            }
        }
    }
    said
}

/// `roomlerd --help`, and every subcommand's.
#[test]
fn roomlerd_help_says_roomlerd() {
    let said = help_says_agent(crate::app::Cli::command());
    assert!(
        said.is_empty(),
        "roomlerd's help says agent:\n{}",
        said.join("\n")
    );
}

/// `roomler --help`, and every subcommand's.
#[test]
fn roomler_help_says_roomlerd() {
    let said = help_says_agent(roomler_cli::cli::Cli::command());
    assert!(
        said.is_empty(),
        "roomler's help says agent:\n{}",
        said.join("\n")
    );
}

/// The walk reaches the subcommands, not the top level alone.
#[test]
fn the_help_walk_reaches_every_subcommand() {
    let paths: Vec<String> = every_long_help(crate::app::Cli::command())
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert!(paths.iter().any(|p| p == "roomlerd enroll"), "{paths:?}");
    let paths: Vec<String> = every_long_help(roomler_cli::cli::Cli::command())
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert!(paths.iter().any(|p| p == "roomler forward"), "{paths:?}");
}

/// The kept spellings are taken out whole, and the word around them is not.
#[test]
fn the_kept_spellings_leave_the_word_visible() {
    let kept = "rc:agent.hello role=agent /api/agent/enroll /api/tenant/{}/agent/{} \
                agent-v0.4.1 com.roomler.agent --agent <AGENT> {agent}";
    assert!(!says_agent(&prose(kept)), "{}", prose(kept));
    for (from, _) in KEPT {
        assert!(!says_agent(&prose(from)), "{from}");
    }
    for text in [
        "the agent",
        "rc:agent.hello reaches the agent",
        "--agent names an agent",
        "agents",
    ] {
        assert!(says_agent(&prose(text)), "{text}");
    }
}
