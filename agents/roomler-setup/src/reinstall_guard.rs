// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-141 P3 (D2-A) — a Mac that is one row already gets no install and no
//! pairing from this wizard.
//!
//! The terminal's one-row form (FR-43 P3d, and its default since #971's
//! flip) makes a Mac ONE device row: root's daemon, supervising the person's
//! screen worker, with no enrollment in the person's config. A wizard run over
//! such a Mac used to install its two halves anyway (FR-141 P0, cell M5): the
//! root half enrolled again under the hardware id, where the script had used a
//! derived one, so the script's row went offline, and the person's half made a
//! third row that never came online. This module stops that before any token
//! is asked for: the Welcome page says so and lets no daemon install go on, and
//! pairing start and the install's preflight refuse with the same words.
//!
//! How the wizard tells: it runs as the person, who cannot read root's
//! `/etc/roomler/config.toml`, so it asks the root daemon itself, over the
//! person socket root serves the console user (`/var/run/roomler-person/<uid>
//! .sock`, FR-97 P2g). That socket admits `status`
//! (`roomler_localapi::person_may`) and passes it through whole
//! (`person_view`), and a Mac's root daemon reports `gui_worker.one_row` in it
//! (FR-43 P3c). The read is read-only, bounded by [`QUERY_TIMEOUT`], and only a
//! socket root serves is believed: a socket anyone else serves is refused, not
//! read. Unknown — no root config, no daemon, an older one, a silent one — is
//! never one row: the wizard then does what it did before.
//!
//! A wizard started as root (a person's `sudo`, its only root road on a Mac:
//! it never elevates itself there) has no person socket, so it asks the
//! system socket, the root daemon's own, in the same way ([`socket_to_ask`]).
//! A person with no person socket asks nothing, never the system socket.
//!
//! The stop says how to go on ([`stop_text`]), naming only what works as typed
//! on that Mac: removing Roomler and starting Setup again everywhere, and
//! `sudo roomlerd one-row --revert` only where a migration's copy is there to
//! put back ([`migration_copy`]).
//!
//! The kill switch is the server's `wizard_reinstall_guard` (default on,
//! [`crate::features::SetupFeatures::reinstall_guard_on`]), NOT the wizard's
//! one-row switch: the terminal makes one-row Macs whatever that switch says,
//! so the guard has to hold with it off.

use std::path::{Path, PathBuf};

use crate::role::{HostOs, Role};

/// How long the question may take: the root identity's own bound.
pub use crate::root_identity::QUERY_TIMEOUT;

/// A Mac whose root daemon says it is one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneRowMac {
    /// The device row's name, as the root daemon reports it (may be empty).
    pub name: String,
    /// `sudo roomlerd one-row` made it one row and root's copy of the file
    /// from before is still there ([`migration_copy`]), so `sudo roomlerd
    /// one-row --revert` has rows to put back.
    pub migrated: bool,
}

/// The root daemon's status read as a one-row Mac's, or `None`. One row only
/// when the daemon says `gui_worker.one_row`, and only for the system
/// config's daemon: a root daemon running on another config is not this
/// Mac's system install.
pub fn one_row_from(status: &roomler_localapi::NodeStatus) -> Option<OneRowMac> {
    let one_row = status.gui_worker.as_ref().is_some_and(|g| g.one_row);
    let system = status
        .config_path
        .as_deref()
        .is_none_or(|p| p == crate::unix_daemon::SYSTEM_CONFIG);
    (one_row && system).then(|| OneRowMac {
        name: status.name.clone(),
        migrated: false,
    })
}

/// Which socket answers for this Mac's root daemon (#4133's second note). A
/// person asks the person socket root serves them and nothing else, so a
/// person with none asks nothing, never the system socket. Root has no person
/// socket and asks the system socket, the root daemon's own. Whatever answers
/// is believed only if root serves it, and only through [`one_row_from`].
pub fn socket_to_ask(
    is_root: bool,
    own_person_socket: Option<PathBuf>,
    system_socket: PathBuf,
) -> Option<PathBuf> {
    if is_root {
        Some(system_socket)
    } else {
        own_person_socket
    }
}

/// Root's copy of its file from before `sudo roomlerd one-row`, as that
/// command names it. Only that command writes it, and only a finished
/// `--revert` or the command's own undo removes it, so while it is there the
/// revert has rows to put back. `/etc/roomler` is 0755, so the person's
/// `stat` reaches it; a `stat` that fails reads as no copy, which only leaves
/// the revert out of the stop.
pub fn migration_copy() -> PathBuf {
    roomlerd::one_row::kept_path(Path::new(roomlerd::one_row::ROOT_CONFIG))
}

/// Ask this Mac's root daemon. `None` off macOS, with no root config, and
/// whenever the daemon cannot say within [`QUERY_TIMEOUT`].
pub async fn one_row_here() -> Option<OneRowMac> {
    #[cfg(target_os = "macos")]
    {
        if !crate::unix_daemon::system_config_path().is_file() {
            return None;
        }
        let mac = tokio::time::timeout(QUERY_TIMEOUT, query_macos())
            .await
            .ok()
            .flatten()?;
        Some(OneRowMac {
            migrated: migration_copy().is_file(),
            ..mac
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
async fn query_macos() -> Option<OneRowMac> {
    // The person socket root serves THIS account (the companion's and the
    // CLI's own road to it), or, as root, who has none, the system socket.
    let path = socket_to_ask(
        roomler_localapi::process_is_root(),
        roomler_localapi::own_person_socket_path(),
        roomler_localapi::system_socket_path(),
    )?;
    // Only a socket ROOT serves: a per-user daemon of this person, or anyone
    // else, would be speaking for itself, not for the machine.
    let mut client = roomler_localapi::connect_unix_served_by_root(path)
        .await
        .ok()?;
    let status = client.status().await.ok()?;
    one_row_from(&status)
}

/// Does `role` install a daemon, which is what this guard holds back? The
/// tunnel client enrolls no device row and stays open.
pub fn installs_a_device(role: Role) -> bool {
    match role {
        Role::DaemonSystem | Role::DaemonUser | Role::DaemonMachine => true,
        Role::TunnelClient => false,
    }
}

/// THE decision, for every place that asks it (the Welcome page's detect,
/// pairing start, the install's preflight): the words to stop a daemon
/// install with, or `None` to go on as before. Only a Mac, only with the
/// guard on, and only when the root daemon said one row.
pub fn stop(
    os: HostOs,
    guard_on: bool,
    one_row: Option<&OneRowMac>,
    server: &str,
) -> Option<String> {
    if os != HostOs::Macos || !guard_on {
        return None;
    }
    one_row.map(|mac| stop_text(mac, server))
}

/// [`stop`] for `role` on this host, asked where an install would begin
/// (pairing start): the root daemon first, and the server's switch only on a
/// Mac it says is one row, so every other host asks nothing more. A server
/// that cannot be asked leaves the guard on.
pub async fn stop_for(role: Role, server: &str) -> Option<String> {
    let os = HostOs::current();
    if os != HostOs::Macos || !installs_a_device(role) {
        return None;
    }
    let mac = one_row_here().await?;
    let guard_on = crate::features::ask(server)
        .await
        .features
        .reinstall_guard_on();
    stop(os, guard_on, Some(&mac), server)
}

/// What the person reads, on the page and in a refusal: the stop, then how to
/// go on (#4133's first note), naming only what works as typed on this Mac.
/// Every one-row Mac can start again from nothing: remove Roomler with the
/// script's `--uninstall` (`docs/installation.md`, Uninstall), from `server`,
/// the one the person chose, then start Setup again. That is the road to a new
/// enrollment after the row was deleted (launchd starts the daemon again after
/// its exit 7, #2293, so the stop can still show), and to two rows on a Mac the
/// terminal installed as one row. A Mac a migration made one row is also
/// offered the revert, while its copy is there: anywhere else the revert says
/// only "nothing to revert".
pub fn stop_text(mac: &OneRowMac, server: &str) -> String {
    let name = mac.name.trim();
    let named = if name.is_empty() {
        String::new()
    } else {
        format!(" («{name}»)")
    };
    let revert = if mac.migrated {
        " `sudo roomlerd one-row` made it one device, and `sudo roomlerd one-row --revert` \
         in Terminal puts back the devices it was before, except any deleted since."
    } else {
        ""
    };
    let server = match server.trim().trim_end_matches('/') {
        "" => crate::commands::cmd_default_server_url(),
        chosen => chosen.to_string(),
    };
    format!(
        "This Mac is one device already{named}: its screen sharing, remote control and \
         private network run there, and it updates itself. Roomler Setup installs nothing \
         over it, because another install would add devices beside it. The tunnel client \
         can still be installed.{revert} To start again from nothing, after its device was \
         deleted or to set it up as two devices, remove Roomler in Terminal with \
         `curl -fsSL {server}/api/setup/install.sh | sh -s -- --uninstall`, then start \
         Roomler Setup again."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "https://roomler.example";
    const UNINSTALL: &str =
        "`curl -fsSL https://roomler.example/api/setup/install.sh | sh -s -- --uninstall`";
    const REVERT: &str = "`sudo roomlerd one-row --revert`";

    fn mac() -> OneRowMac {
        OneRowMac {
            name: "Studio Mac".into(),
            migrated: false,
        }
    }

    /// FR-141 P0's cell M5 (a wizard run over a Mac the script made one row
    /// left three rows): with the guard on, a one-row Mac gets the stop, which
    /// names the row and says nothing is installed.
    #[test]
    fn a_one_row_mac_gets_no_install_and_no_pairing() {
        let said =
            stop(HostOs::Macos, true, Some(&mac()), SERVER).expect("a one-row Mac is stopped");
        assert!(said.contains("«Studio Mac»"), "{said}");
        assert!(said.contains("installs nothing"), "{said}");
    }

    /// Unknown is never one row, the server's `false` turns the guard off,
    /// and it is a Mac's alone.
    #[test]
    fn unknown_is_never_one_row_and_the_switch_turns_it_off() {
        assert_eq!(stop(HostOs::Macos, true, None, SERVER), None, "unknown");
        assert_eq!(
            stop(HostOs::Macos, false, Some(&mac()), SERVER),
            None,
            "the operator's off"
        );
        assert_eq!(stop(HostOs::Linux, true, Some(&mac()), SERVER), None);
        assert_eq!(stop(HostOs::Windows, true, Some(&mac()), SERVER), None);
        assert_eq!(stop(HostOs::Other, true, Some(&mac()), SERVER), None);
    }

    /// A nameless row is still named as one device, without empty quotes.
    #[test]
    fn a_nameless_row_reads_without_quotes() {
        let said = stop_text(
            &OneRowMac {
                name: " ".into(),
                migrated: false,
            },
            SERVER,
        );
        assert!(
            said.starts_with("This Mac is one device already:"),
            "{said}"
        );
        assert!(!said.contains('«'), "{said}");
    }

    /// #4133's first note: the stop names a way forward that works as typed
    /// on every Mac it shows on. Removing Roomler and starting Setup again is
    /// the road to a fresh enrollment (the row was deleted) and to two rows
    /// (a Mac the terminal installed as one row, with no copy to put back),
    /// so every stop names it, with the server the person chose. The revert
    /// would only say "nothing to revert" there, so it is not offered.
    #[test]
    fn the_stop_names_a_way_forward() {
        for name in ["Studio Mac", " "] {
            let one_row = OneRowMac {
                name: name.into(),
                migrated: false,
            };
            let said = stop_text(&one_row, SERVER);
            assert!(said.starts_with("This Mac is one device already"), "{said}");
            assert!(said.contains("installs nothing"), "{said}");
            assert!(said.contains(UNINSTALL), "{said}");
            assert!(said.contains("start Roomler Setup again"), "{said}");
            assert!(!said.contains("--revert"), "nothing to revert: {said}");
        }
        assert!(
            stop_text(&mac(), SERVER).contains("already («Studio Mac»): "),
            "the name stays where it was"
        );
        // A trailing slash makes no `//api`, and a blank server names the
        // wizard's default.
        assert!(stop_text(&mac(), "https://roomler.example/").contains(UNINSTALL));
        assert!(stop_text(&mac(), " ").contains(
            "`curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- --uninstall`"
        ));
    }

    /// A Mac `sudo roomlerd one-row` made one row, with root's copy of the
    /// file from before still there, is also offered the way back to two
    /// rows, and told that a deleted row does not come back.
    #[test]
    fn a_migrated_mac_is_offered_the_revert() {
        let migrated = OneRowMac {
            name: "Studio Mac".into(),
            migrated: true,
        };
        let said = stop_text(&migrated, SERVER);
        assert!(
            said.starts_with("This Mac is one device already («Studio Mac»)"),
            "{said}"
        );
        assert!(said.contains("installs nothing"), "{said}");
        assert!(said.contains(REVERT), "{said}");
        assert!(said.contains("deleted since"), "{said}");
        assert!(said.contains(UNINSTALL), "the fresh start stays: {said}");
    }

    /// #4133's second note: the wizard started as root asks the system
    /// socket, the root daemon's own; a person asks only the person socket
    /// root serves them, and a person with none asks nothing, never the
    /// system socket.
    #[test]
    fn root_asks_the_system_socket_and_a_person_only_their_own() {
        use std::path::PathBuf;
        let person = PathBuf::from("/var/run/roomler-person/501.sock");
        let system = PathBuf::from("/var/run/roomler/roomler.sock");
        assert_eq!(
            socket_to_ask(false, Some(person.clone()), system.clone()),
            Some(person.clone()),
            "a person: the person socket"
        );
        assert_eq!(
            socket_to_ask(false, None, system.clone()),
            None,
            "a person with none: never the system socket"
        );
        assert_eq!(
            socket_to_ask(true, None, system.clone()),
            Some(system.clone()),
            "root: the system socket"
        );
        assert_eq!(
            socket_to_ask(true, Some(person), system.clone()),
            Some(system),
            "root never asks a person socket"
        );
    }

    /// The copy that offers the revert is the one `sudo roomlerd one-row`
    /// keeps of root's file, and the one its `--revert` puts back.
    #[test]
    fn the_revert_is_offered_on_the_copy_the_migration_keeps() {
        assert_eq!(
            migration_copy(),
            std::path::PathBuf::from("/etc/roomler/config.toml.pre-one-row")
        );
    }

    fn status(extra: &str) -> roomler_localapi::NodeStatus {
        serde_json::from_str(&format!(
            r#"{{"node_id":"n","name":"Studio Mac","version":"v","mode":"service","connected":true{extra}}}"#
        ))
        .unwrap()
    }

    /// Only the root daemon's own `one_row`, on the system config, is one row.
    #[test]
    fn only_roots_one_row_word_is_one_row() {
        let one_row = r#","gui_worker":{"attached":true,"one_row":true}"#;
        assert_eq!(one_row_from(&status(one_row)), Some(mac()), "no path");
        assert_eq!(
            one_row_from(&status(&format!(
                r#"{one_row},"config_path":"/etc/roomler/config.toml""#
            ))),
            Some(mac()),
            "the system config"
        );
        assert_eq!(
            one_row_from(&status(&format!(
                r#"{one_row},"config_path":"/Users/p/Library/Application Support/live.roomler.roomler/config.toml""#
            ))),
            None,
            "another config's daemon"
        );
        // Two rows, and P2 (the supervisor on, one row off): the root half
        // reports `one_row: false`.
        assert_eq!(
            one_row_from(&status(
                r#","gui_worker":{"attached":false,"one_row":false}"#
            )),
            None
        );
        // An older daemon, or no Mac: no `gui_worker` at all.
        assert_eq!(one_row_from(&status("")), None);
        // A worker attached says nothing by itself.
        assert_eq!(
            one_row_from(&status(r#","gui_worker":{"attached":true}"#)),
            None
        );
    }

    /// The tunnel client is not held back; every daemon role is.
    #[test]
    fn only_a_device_install_is_held_back() {
        assert!(installs_a_device(Role::DaemonUser));
        assert!(installs_a_device(Role::DaemonSystem));
        assert!(installs_a_device(Role::DaemonMachine));
        assert!(!installs_a_device(Role::TunnelClient));
    }

    /// Nothing to ask on a test host (not a Mac, or no root config): `None`,
    /// within the bound.
    #[tokio::test]
    async fn no_root_daemon_is_not_one_row() {
        let begun = std::time::Instant::now();
        let _ = one_row_here().await;
        assert!(begun.elapsed() <= QUERY_TIMEOUT + std::time::Duration::from_secs(1));
    }
}
