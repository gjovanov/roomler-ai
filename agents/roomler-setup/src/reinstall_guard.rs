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
//! The kill switch is the server's `wizard_reinstall_guard` (default on,
//! [`crate::features::SetupFeatures::reinstall_guard_on`]), NOT the wizard's
//! one-row switch: the terminal makes one-row Macs whatever that switch says,
//! so the guard has to hold with it off.

use crate::role::{HostOs, Role};

/// How long the question may take: the root identity's own bound.
pub use crate::root_identity::QUERY_TIMEOUT;

/// A Mac whose root daemon says it is one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneRowMac {
    /// The device row's name, as the root daemon reports it (may be empty).
    pub name: String,
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
    })
}

/// Ask this Mac's root daemon. `None` off macOS, with no root config, and
/// whenever the daemon cannot say within [`QUERY_TIMEOUT`].
pub async fn one_row_here() -> Option<OneRowMac> {
    #[cfg(target_os = "macos")]
    {
        if !crate::unix_daemon::system_config_path().is_file() {
            return None;
        }
        tokio::time::timeout(QUERY_TIMEOUT, query_macos())
            .await
            .ok()
            .flatten()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
async fn query_macos() -> Option<OneRowMac> {
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    let path = roomler_localapi::person_socket_path(uid);
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
pub fn stop(os: HostOs, guard_on: bool, one_row: Option<&OneRowMac>) -> Option<String> {
    if os != HostOs::Macos || !guard_on {
        return None;
    }
    one_row.map(stop_text)
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
    stop(os, guard_on, Some(&mac))
}

/// What the person reads, on the page and in a refusal.
pub fn stop_text(mac: &OneRowMac) -> String {
    let name = mac.name.trim();
    let named = if name.is_empty() {
        String::new()
    } else {
        format!(" («{name}»)")
    };
    format!(
        "This Mac is one device already{named}: its screen sharing, remote control and \
         private network run there, and it updates itself. Roomler Setup installs nothing \
         over it, because another install would add devices beside it. The tunnel client \
         can still be installed."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac() -> OneRowMac {
        OneRowMac {
            name: "Studio Mac".into(),
        }
    }

    /// FR-141 P0's cell M5 (a wizard run over a Mac the script made one row
    /// left three rows): with the guard on, a one-row Mac gets the stop, which
    /// names the row and says nothing is installed.
    #[test]
    fn a_one_row_mac_gets_no_install_and_no_pairing() {
        let said = stop(HostOs::Macos, true, Some(&mac())).expect("a one-row Mac is stopped");
        assert!(said.contains("«Studio Mac»"), "{said}");
        assert!(said.contains("installs nothing"), "{said}");
    }

    /// Unknown is never one row, the server's `false` turns the guard off,
    /// and it is a Mac's alone.
    #[test]
    fn unknown_is_never_one_row_and_the_switch_turns_it_off() {
        assert_eq!(stop(HostOs::Macos, true, None), None, "unknown");
        assert_eq!(
            stop(HostOs::Macos, false, Some(&mac())),
            None,
            "the operator's off"
        );
        assert_eq!(stop(HostOs::Linux, true, Some(&mac())), None);
        assert_eq!(stop(HostOs::Windows, true, Some(&mac())), None);
        assert_eq!(stop(HostOs::Other, true, Some(&mac())), None);
    }

    /// A nameless row is still named as one device, without empty quotes.
    #[test]
    fn a_nameless_row_reads_without_quotes() {
        let said = stop_text(&OneRowMac { name: " ".into() });
        assert!(
            said.starts_with("This Mac is one device already:"),
            "{said}"
        );
        assert!(!said.contains('«'), "{said}");
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
