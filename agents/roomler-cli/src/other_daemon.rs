// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3898 — the note `status`, `peers`, `why`, `config ls` and `config set` add
//! when this host runs a second daemon: its words, and which of them.
//!
//! On a Mac with two device rows the CLI a person types reaches the per-user
//! half, and the note says so (`localclient::print_other_daemon_hint`). A
//! one-row Mac (FR-43 P3a) has no per-user half: what answers the person there
//! is the screen worker the root daemon starts for whoever is at the console
//! (`roomlerd run --supervised --unenrolled`). It holds no enrollment and is no
//! second device, so there the note names it, and says what a key set through
//! it changes.
//!
//! Pure: the caller asks the daemons and passes in what they said, so every
//! case is a unit test here.

use std::path::Path;

/// The command the note follows, which decides what the note adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hint {
    /// `roomler status`.
    Status,
    /// `roomler peers` and `roomler why`.
    Network,
    /// `roomler config ls` and `roomler config set` (#2871).
    Config,
}

/// The note's lines, a blank one first, or none at all.
///
/// `other` is the root daemon's socket when this host runs a second daemon
/// (`localapi::other_daemon_socket`); with no second daemon there is no note.
/// `one_row` is that daemon's own word on whether it runs one row
/// (`gui_worker.one_row` in its status), `None` when the CLI could not read
/// it. Only `Some(true)` changes the words: a two-row Mac, and every host
/// where that word is missing, keep the ones they had.
pub(crate) fn note(hint: Hint, other: Option<&Path>, one_row: Option<bool>) -> Vec<String> {
    let Some(other) = other else {
        return Vec::new();
    };
    let other = other.display();
    if one_row == Some(true) {
        let mut lines = vec![
            String::new(),
            "note: this answer is from the screen worker of this one-row Mac.".to_string(),
            format!("      Its root daemon ({other}) starts it for the person at the console."),
        ];
        lines.extend(
            one_row_words(hint)
                .iter()
                .map(|line| format!("      {line}")),
        );
        return lines;
    }
    vec![
        String::new(),
        format!("note: a second roomler daemon is running on this host ({other})."),
        format!("      This is the per-user half; {}", per_user_half(hint)),
        "      Use `sudo roomler …` to ask that one instead.".to_string(),
    ]
}

/// What the note adds on a one-row Mac, per command: where the device's own
/// answer is, and for `config` what a key set through the worker changes.
/// The person's file holds their session settings (FR-43); the device's keys
/// are in the root daemon's file.
fn one_row_words(hint: Hint) -> &'static [&'static str] {
    match hint {
        Hint::Status => &["The device's own status is the root daemon's: `sudo roomler status`."],
        Hint::Network => &["The overlay is the root daemon's: `sudo roomler …` asks it directly."],
        Hint::Config => &[
            "What the worker lists and sets is your own file: your session settings.",
            "The device's keys (the overlay, SSH, exec) are the root daemon's:",
            "`sudo roomler config ls` / `sudo roomler config set <key> <value>`.",
        ],
    }
}

/// What the note adds on a Mac with two rows, per command.
fn per_user_half(hint: Hint) -> &'static str {
    match hint {
        Hint::Status | Hint::Network => OVERLAY_IS_THE_ROOT_HALFS,
        Hint::Config => CONFIG_IS_THE_PER_USER_HALFS,
    }
}

/// The note's second line for the network commands.
const OVERLAY_IS_THE_ROOT_HALFS: &str = "the overlay runs in the privileged one.";

/// #2871 — the note's second line for `config ls` / `config set`. On a
/// supervised Mac (FR-43) the per-user half the unprivileged CLI reaches has a
/// configuration of its own, and the device-owned gates that matter to an
/// administrator — the overlay, Roomler SSH, remote commands — are read from
/// the ROOT half's `/etc/roomler/config.toml`. Before this line the command
/// listed, and changed, the per-user half's file with no word that another
/// existed; the Mac in #2871 showed `ssh_enabled = false` here while the root
/// half served SSH.
const CONFIG_IS_THE_PER_USER_HALFS: &str = "this is ITS configuration. The root daemon's keys \
     (the overlay, SSH, exec) live in its own file: `sudo roomler config ls` / `sudo roomler \
     config set <key> <value>`.";

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/var/run/roomler/roomler.sock";
    const EVERY: [Hint; 3] = [Hint::Status, Hint::Network, Hint::Config];

    fn on_this_host(hint: Hint, one_row: Option<bool>) -> Vec<String> {
        note(hint, Some(Path::new(ROOT)), one_row)
    }

    /// The note every Mac with a second daemon printed before #3898, byte for
    /// byte. Written out here rather than read from the constants above, so a
    /// change to those shows as a failure.
    fn the_words_before(hint: Hint) -> Vec<&'static str> {
        let second = match hint {
            Hint::Status | Hint::Network => {
                "      This is the per-user half; the overlay runs in the privileged one."
            }
            Hint::Config => {
                "      This is the per-user half; this is ITS configuration. The root daemon's \
                 keys (the overlay, SSH, exec) live in its own file: `sudo roomler config ls` / \
                 `sudo roomler config set <key> <value>`."
            }
        };
        vec![
            "",
            "note: a second roomler daemon is running on this host \
             (/var/run/roomler/roomler.sock).",
            second,
            "      Use `sudo roomler …` to ask that one instead.",
        ]
    }

    /// #3898 — on a one-row Mac, every command's note names what answered:
    /// the screen worker its root daemon starts for the person at the console.
    /// It never calls it the per-user half, and never a second daemon.
    #[test]
    fn a_one_row_mac_names_the_screen_worker_that_answered() {
        for hint in EVERY {
            let text = on_this_host(hint, Some(true)).join("\n");
            assert!(
                text.contains("from the screen worker of this one-row Mac"),
                "{hint:?}: {text}"
            );
            assert!(
                text.contains("starts it for the person at the console"),
                "{hint:?}: {text}"
            );
            assert!(text.contains(ROOT), "{hint:?}: {text}");
            assert!(!text.contains("per-user half"), "{hint:?}: {text}");
            assert!(!text.contains("second roomler daemon"), "{hint:?}: {text}");
        }
    }

    /// `status` on a one-row Mac says where the device's own status is.
    #[test]
    fn a_one_row_macs_status_note_says_where_the_devices_own_status_is() {
        assert_eq!(
            on_this_host(Hint::Status, Some(true)),
            vec![
                "",
                "note: this answer is from the screen worker of this one-row Mac.",
                "      Its root daemon (/var/run/roomler/roomler.sock) starts it for the person at \
                 the console.",
                "      The device's own status is the root daemon's: `sudo roomler status`.",
            ]
        );
    }

    /// `config ls` / `config set` on a one-row Mac say what a key set there
    /// changes, the person's own file, and that the device's keys are the
    /// root daemon's, behind `sudo`.
    #[test]
    fn a_one_row_macs_config_note_says_what_a_key_set_there_changes() {
        assert_eq!(
            on_this_host(Hint::Config, Some(true)),
            vec![
                "",
                "note: this answer is from the screen worker of this one-row Mac.",
                "      Its root daemon (/var/run/roomler/roomler.sock) starts it for the person at \
                 the console.",
                "      What the worker lists and sets is your own file: your session settings.",
                "      The device's keys (the overlay, SSH, exec) are the root daemon's:",
                "      `sudo roomler config ls` / `sudo roomler config set <key> <value>`.",
            ]
        );
    }

    /// `peers` and `why` on a one-row Mac send the person to the root daemon
    /// for the overlay.
    #[test]
    fn a_one_row_macs_network_note_names_the_root_daemons_overlay() {
        let lines = on_this_host(Hint::Network, Some(true));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("      The overlay is the root daemon's: `sudo roomler …` asks it directly.")
        );
    }

    /// A two-row Mac keeps every word of the note, for every command.
    #[test]
    fn a_two_row_mac_keeps_every_word() {
        for hint in EVERY {
            assert_eq!(
                on_this_host(hint, Some(false)),
                the_words_before(hint),
                "{hint:?}"
            );
        }
    }

    /// No second daemon, no note, whatever the root daemon said.
    #[test]
    fn no_second_daemon_means_no_note() {
        for hint in EVERY {
            for one_row in [Some(true), Some(false), None] {
                assert!(note(hint, None, one_row).is_empty(), "{hint:?} {one_row:?}");
            }
        }
    }

    /// The fallback: when the root daemon's word could not be read (it
    /// refused, did not answer in time, predates the field, or this is not a
    /// Mac), the note keeps the words it had.
    #[test]
    fn an_unread_answer_keeps_the_words_before() {
        for hint in EVERY {
            assert_eq!(on_this_host(hint, None), the_words_before(hint), "{hint:?}");
        }
    }
}
