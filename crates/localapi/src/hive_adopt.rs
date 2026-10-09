// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1j — the adopt socket's protocol: what `roomler hive hook`, run by
//! Claude Code as the person at a terminal, tells the daemon about that
//! terminal's session (`docs/fr/FR-90-hive-agent-sessions.md` §3h).
//!
//! ⚠️ This is NOT the LocalAPI socket, which on a root daemon is root-only and
//! whose verbs mostly trust the socket alone. The adopt socket is open to
//! every local account, because every account may adopt its own sessions,
//! and the daemon trusts nothing a hook says about who it is. It reads the
//! peer's uid from the kernel, maps that account through the device's own
//! `hive_accounts`, and refuses root. A hook can only ever speak for its own
//! account's terminals.
//!
//! ⚠️ The hook reads the transcript and SENDS it. The daemon never opens a path
//! a hook names: a root daemon reading `transcript_path` would read any file
//! the requester could point it at.
//!
//! Wire: newline-delimited JSON. Each request is answered by one [`Reply`]
//! before the next is read.
//!
//! ```text
//! hook                                   daemon
//!  ── Hello {harness_session, cwd, event} ─▶   (adopted already? offer it to the server)
//!  ◀─ Reply {ok, offset} ──────────────────
//!  ── Lines {from: offset, data} ─────────▶   whole lines only; repeated to EOF
//!  ◀─ Reply {ok, offset} ──────────────────
//!  ── TurnEnded | End {reason} ───────────▶   the Stop / SessionEnd hook
//!  ◀─ Reply {ok} ──────────────────────────
//! ```

use serde::{Deserialize, Serialize};

/// The socket's file name in the daemon's Hive runtime directory.
pub const SOCKET_NAME: &str = "adopt.sock";

/// The most transcript bytes one [`Request::Lines`] carries. Whole lines only,
/// so a hook stops before the line that would cross this and sends it next.
pub const MAX_CHUNK: usize = 1024 * 1024;

/// The longest single transcript line the daemon takes. A longer one (a huge
/// tool output) is skipped by the hook and noted, never truncated mid-JSON.
pub const MAX_LINE: usize = 4 * 1024 * 1024;

/// The most bytes one hook run sends before it stops: the rest goes with the
/// next turn's hook. Bounds how long a hook can hold the daemon's attention.
pub const MAX_PER_RUN: usize = 64 * 1024 * 1024;

/// Which Claude Code hook ran.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    Stop,
    SessionEnd,
    /// An event this build does not know. The daemon treats it as a mirror
    /// request and nothing more.
    #[serde(other)]
    Other,
}

/// One request from a hook.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// The first word of every hook run: which terminal session, where, and
    /// which hook. The account is the kernel's answer, never the hook's.
    Hello {
        /// Claude Code's own session id (a UUID).
        harness_session: String,
        /// The terminal's working directory, as Claude Code reports it.
        cwd: String,
        event: HookEvent,
    },
    /// The transcript's next whole lines, starting at byte `from`. A `from`
    /// that is not where the daemon's copy ends is answered with where it does
    /// end, and the hook sends from there.
    ///
    /// `skipped`: bytes AFTER `data` the hook did not send: one line longer
    /// than [`MAX_LINE`] (a huge tool output), whose place the daemon's copy
    /// moves past and marks. Never part of a line: whole lines only.
    Lines {
        from: u64,
        data: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        skipped: u64,
    },
    /// The turn the person just finished (the `Stop` hook).
    TurnEnded,
    /// The terminal session ended (`SessionEnd`), with Claude Code's reason.
    End {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Where a daemon on this system listens: its Hive runtime directory, which
/// it owns and clears at boot. `None` where no daemon adopts (Windows, until
/// its sessions land).
pub fn socket_path() -> Option<std::path::PathBuf> {
    let dir = if cfg!(target_os = "linux") {
        RUNTIME_DIR_LINUX
    } else if cfg!(target_os = "macos") {
        RUNTIME_DIR_MACOS
    } else {
        return None;
    };
    Some(std::path::Path::new(dir).join(SOCKET_NAME))
}

/// The daemon's Hive runtime directory on Linux (`/run` is cleared at boot).
pub const RUNTIME_DIR_LINUX: &str = "/run/roomler-hive";
/// … and on macOS, which clears `/var/run` at boot.
pub const RUNTIME_DIR_MACOS: &str = "/var/run/roomler-hive";

/// The daemon's answer to one [`Request`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Reply {
    pub ok: bool,
    /// Where the daemon's copy of the transcript ends: the hook sends from
    /// here next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Why not, for `ok: false`: a word the hook may log, never shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

impl Reply {
    pub fn ok(offset: Option<u64>) -> Self {
        Self {
            ok: true,
            offset,
            refused: None,
        }
    }

    pub fn refused(word: impl Into<String>, offset: Option<u64>) -> Self {
        Self {
            ok: false,
            offset,
            refused: Some(word.into()),
        }
    }
}

/// Every word [`Reply::refused`] carries. Locked by test: the hook's log and
/// `roomler hive adopt`'s status read them.
pub mod refusal {
    /// The device's owner has not allowed adopting (`hive_adopt` off).
    pub const ADOPT_DISABLED: &str = "adopt_disabled";
    /// The account this terminal runs as maps to nobody in `hive_accounts`.
    pub const NO_ACCOUNT: &str = "no_account";
    /// Root's own sessions are never adopted.
    pub const ROOT: &str = "root";
    /// The server is not reachable now; the next hook offers it again.
    pub const OFFLINE: &str = "offline";
    /// The server refused it; its word follows after a colon.
    pub const SERVER: &str = "server";
    /// Mirroring was stopped for this terminal session (from Roomler, or
    /// because it ended); nothing more is taken for it.
    pub const STOPPED: &str = "stopped";
    /// Another account's terminal session.
    pub const NOT_YOURS: &str = "not_yours";
    /// The request was malformed or out of order.
    pub const BAD_REQUEST: &str = "bad_request";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_is_locked() {
        let hello = Request::Hello {
            harness_session: "6f1c8a2e-3b7d-4e5f-9a10-2b3c4d5e6f70".into(),
            cwd: "/home/alice/x".into(),
            event: HookEvent::SessionStart,
        };
        assert_eq!(
            serde_json::to_value(&hello).unwrap(),
            serde_json::json!({
                "op": "hello",
                "harness_session": "6f1c8a2e-3b7d-4e5f-9a10-2b3c4d5e6f70",
                "cwd": "/home/alice/x",
                "event": "session_start",
            })
        );
        assert_eq!(
            serde_json::to_value(Request::Lines {
                from: 7,
                data: "{}\n".into(),
                skipped: 0,
            })
            .unwrap(),
            serde_json::json!({"op": "lines", "from": 7, "data": "{}\n"})
        );
        assert_eq!(
            serde_json::to_value(Request::Lines {
                from: 7,
                data: String::new(),
                skipped: 9,
            })
            .unwrap(),
            serde_json::json!({"op": "lines", "from": 7, "data": "", "skipped": 9})
        );
        assert_eq!(
            serde_json::to_value(Request::TurnEnded).unwrap(),
            serde_json::json!({"op": "turn_ended"})
        );
        assert_eq!(
            serde_json::to_value(Request::End { reason: None }).unwrap(),
            serde_json::json!({"op": "end"})
        );
        assert_eq!(
            serde_json::to_value(Reply::ok(Some(3))).unwrap(),
            serde_json::json!({"ok": true, "offset": 3})
        );
        assert_eq!(
            serde_json::to_value(Reply::refused(refusal::STOPPED, None)).unwrap(),
            serde_json::json!({"ok": false, "refused": "stopped"})
        );
    }

    #[test]
    fn an_unknown_hook_event_still_decodes() {
        let r: Request = serde_json::from_value(serde_json::json!({
            "op": "hello", "harness_session": "u", "cwd": "/", "event": "pre_compact"
        }))
        .unwrap();
        assert!(matches!(
            r,
            Request::Hello {
                event: HookEvent::Other,
                ..
            }
        ));
    }
}
