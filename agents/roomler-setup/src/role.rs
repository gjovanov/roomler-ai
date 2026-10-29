// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The unified wizard's role picker vocabulary.
//!
//! One `Role` = one thing the operator wants this machine to be. The wire
//! strings are a compatibility surface (persisted in `WizardState`, carried
//! by the SPA's cards), so a variant is reused wherever its meaning matches
//! and each OS gives it its own card copy:
//!
//! - [`Role::DaemonSystem`] — "be accessed, even with nobody signed in".
//!   Windows: perMachine MSI + the SystemContext swap
//!   (`ENABLE_SYSTEM_CONTEXT=1` WiX CA → SCM Environment write + service
//!   restart). Linux (FR-129 B1): the root `roomlerd.service`, the TUN
//!   overlay, config at `/etc/roomler/config.toml` — the terminal path's
//!   `--system`. Not offered on macOS (the root half is B2).
//! - [`Role::DaemonUser`] — "be accessed, this user only". Windows: perUser
//!   MSI (Scheduled-Task autostart under the installing user; no UAC). Linux
//!   (B1): the systemd user unit, the overlay through the userspace netstack.
//!   macOS (B1): the LaunchAgent, screen sharing.
//! - [`Role::DaemonMachine`] — "be accessed, attended": plain perMachine MSI
//!   (SCM service, no SystemContext; UAC once during install). Windows-only.
//! - [`Role::TunnelClient`] — "reach others only": the roomler tunnel CLI,
//!   delivered as an archive + PATH integration. Every OS.
//!
//! Which daemon roles an OS implements is [`daemon_roles`]; whether a Linux
//! or macOS card is *shown* also needs the chosen server's
//! `wizard_unix_daemon` switch ([`crate::features`], key design B.6).
//!
//! Role→action COMPOSITION (P4b): a daemon role ALSO delivers the
//! CLI — not by running a second pipeline, but because the daemon
//! MSIs carry `roomler.exe` (wxs `TunnelExe` component) + a PATH
//! append since P4b, and the Linux packages carry `/usr/bin/roomler`.
//! Each role therefore still runs exactly one orchestrator: on Windows
//! the three daemon roles map onto [`crate::orchestrator_agent`] (which
//! existence-checks the MSI-carried CLI and surfaces it via
//! `DoneReport.cli_included`), on Linux and macOS the two daemon roles
//! onto [`crate::orchestrator_unix`], the tunnel role onto
//! [`crate::orchestrator_tunnel`].

use serde::{Deserialize, Serialize};

/// The OS this wizard runs on, as the role picker sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
    Windows,
    Linux,
    Macos,
    Other,
}

impl HostOs {
    pub fn current() -> HostOs {
        match std::env::consts::OS {
            "windows" => HostOs::Windows,
            "linux" => HostOs::Linux,
            "macos" => HostOs::Macos,
            _ => HostOs::Other,
        }
    }

    /// The SPA's spelling (`DetectResult.agent.os`).
    pub fn wire(self) -> &'static str {
        match self {
            HostOs::Windows => "windows",
            HostOs::Linux => "linux",
            HostOs::Macos => "macos",
            HostOs::Other => "other",
        }
    }
}

/// What a Linux or macOS daemon role installs (FR-129 B1): the scope the
/// embedded `install.sh` halves take, and the enroll's install trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A per-user daemon: the systemd user unit, the LaunchAgent.
    PerUser,
    /// The root daemon: `roomlerd.service`, `/etc/roomler/config.toml`.
    System,
}

impl Scope {
    /// The install trace's spelling and the Done page's "Mode".
    pub fn wire(self) -> &'static str {
        match self {
            Scope::PerUser => "per-user",
            Scope::System => "system",
        }
    }
}

/// The daemon roles this OS implements, in card order. Windows keeps its
/// three; Linux offers the system and the per-user daemon; macOS the
/// per-user one (the root half is B2); anything else none.
pub fn daemon_roles(os: HostOs) -> &'static [Role] {
    match os {
        HostOs::Windows => &[Role::DaemonSystem, Role::DaemonMachine, Role::DaemonUser],
        HostOs::Linux => &[Role::DaemonSystem, Role::DaemonUser],
        HostOs::Macos => &[Role::DaemonUser],
        HostOs::Other => &[],
    }
}

/// The scope a role installs through the `install.sh` halves on this OS;
/// `None` where the role is not a daemon role there (every role on Windows,
/// which has the MSI pipeline; `DaemonMachine` and the tunnel client
/// everywhere).
pub fn scope_for(os: HostOs, role: Role) -> Option<Scope> {
    match (os, role) {
        (HostOs::Linux, Role::DaemonSystem) => Some(Scope::System),
        (HostOs::Linux, Role::DaemonUser) | (HostOs::Macos, Role::DaemonUser) => {
            Some(Scope::PerUser)
        }
        _ => None,
    }
}

/// Operator-selected role. Serialises kebab-case (`"daemon-system"` /
/// `"daemon-user"` / `"daemon-machine"` / `"tunnel-client"`) — the
/// same strings the SPA's role cards carry and the persisted
/// `WizardState.role` stores.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    DaemonSystem,
    DaemonUser,
    DaemonMachine,
    TunnelClient,
}

impl Role {
    /// The MSI flavour string the daemon orchestrator feeds to the
    /// installer proxy + `parse`-style mapping. `None` for the tunnel
    /// role (no MSI — archive pipeline instead).
    pub fn msi_flavour(&self) -> Option<&'static str> {
        match self {
            Role::DaemonSystem => Some("permachine-system-context"),
            Role::DaemonUser => Some("peruser"),
            Role::DaemonMachine => Some("permachine"),
            Role::TunnelClient => None,
        }
    }

    /// FR-102 P3 — what the daemon this role installs runs as, for the
    /// enroll's install trace (`install.scope`):
    ///
    /// - `daemon-system` → `system`: the perMachine MSI with SystemContext,
    ///   a LocalSystem service that works pre-logon and on the lock screen;
    /// - `daemon-user` → `per-user`: the perUser MSI, a Scheduled Task under
    ///   the installing user;
    /// - `daemon-machine` → `attended`: the plain perMachine MSI — a service
    ///   too, but one whose capture and input work only beside a signed-in
    ///   session (that is what "Daemon — machine (attended)" means on the
    ///   role card);
    /// - `tunnel-client` → `None`: no daemon is enrolled.
    pub fn install_scope(&self) -> Option<roomlerd::enrollment::InstallScope> {
        use roomlerd::enrollment::InstallScope;
        match self {
            Role::DaemonSystem => Some(InstallScope::System),
            Role::DaemonUser => Some(InstallScope::PerUser),
            Role::DaemonMachine => Some(InstallScope::Attended),
            Role::TunnelClient => None,
        }
    }

    /// FR-129 — which pairing this role asks the browser for, and so which
    /// token kind the approval mints: the tunnel client pairs for a
    /// `TunnelEnrollment` token, every other role for an agent `Enrollment`
    /// token. A role is either the tunnel client or a daemon flavour — the
    /// vocabulary at the top of this file — so the Linux and macOS daemon
    /// roles B1 adds land in the second arm with no change here.
    pub fn pair_role(&self) -> roomlerd::pairing::PairRole {
        use roomlerd::pairing::PairRole;
        match self {
            Role::TunnelClient => PairRole::Tunnel,
            _ => PairRole::Daemon,
        }
    }

    /// Parse the kebab-case string persisted in `WizardState.role`
    /// back into a typed `Role`. Unknown / stale values return `None`
    /// so the SPA re-prompts the picker on resume instead of failing.
    pub fn from_state_str(s: &str) -> Option<Role> {
        match s {
            "daemon-system" => Some(Role::DaemonSystem),
            "daemon-user" => Some(Role::DaemonUser),
            "daemon-machine" => Some(Role::DaemonMachine),
            "tunnel-client" => Some(Role::TunnelClient),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_each_variant() {
        for (role, wire) in [
            (Role::DaemonSystem, "\"daemon-system\""),
            (Role::DaemonUser, "\"daemon-user\""),
            (Role::DaemonMachine, "\"daemon-machine\""),
            (Role::TunnelClient, "\"tunnel-client\""),
        ] {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, wire);
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(back, role);
        }
    }

    #[test]
    fn msi_flavour_mapping() {
        assert_eq!(
            Role::DaemonSystem.msi_flavour(),
            Some("permachine-system-context")
        );
        assert_eq!(Role::DaemonUser.msi_flavour(), Some("peruser"));
        assert_eq!(Role::DaemonMachine.msi_flavour(), Some("permachine"));
        assert_eq!(Role::TunnelClient.msi_flavour(), None);
    }

    /// FR-102 P3 — the documented role → scope mapping, on the wire
    /// spellings the server stores. `daemon-machine` is `attended`, never
    /// `system`: it has no SystemContext and cannot be reached pre-logon.
    #[test]
    fn install_scope_mapping() {
        use roomlerd::enrollment::InstallScope;
        assert_eq!(
            Role::DaemonSystem.install_scope(),
            Some(InstallScope::System)
        );
        assert_eq!(
            Role::DaemonUser.install_scope(),
            Some(InstallScope::PerUser)
        );
        assert_eq!(
            Role::DaemonMachine.install_scope(),
            Some(InstallScope::Attended)
        );
        assert_eq!(Role::TunnelClient.install_scope(), None);
        assert_eq!(
            Role::DaemonSystem.install_scope().map(|s| s.wire()),
            Some("system")
        );
        assert_eq!(
            Role::DaemonUser.install_scope().map(|s| s.wire()),
            Some("per-user")
        );
        assert_eq!(
            Role::DaemonMachine.install_scope().map(|s| s.wire()),
            Some("attended")
        );
    }

    /// FR-129 — the tunnel client pairs as `tunnel`; every daemon flavour as
    /// `daemon`. Listed per variant so a new role is seen here when it lands.
    #[test]
    fn pair_role_mapping() {
        use roomlerd::pairing::PairRole;
        assert_eq!(Role::TunnelClient.pair_role(), PairRole::Tunnel);
        assert_eq!(Role::DaemonSystem.pair_role(), PairRole::Daemon);
        assert_eq!(Role::DaemonUser.pair_role(), PairRole::Daemon);
        assert_eq!(Role::DaemonMachine.pair_role(), PairRole::Daemon);
    }

    /// FR-129 B1 — which daemon roles each OS implements, and what each
    /// installs there. The per-OS lists are what the cards follow (with the
    /// server's switch on top); the scopes are what the halves take.
    #[test]
    fn daemon_roles_and_scopes_per_os() {
        assert_eq!(
            daemon_roles(HostOs::Windows),
            &[Role::DaemonSystem, Role::DaemonMachine, Role::DaemonUser]
        );
        assert_eq!(
            daemon_roles(HostOs::Linux),
            &[Role::DaemonSystem, Role::DaemonUser]
        );
        assert_eq!(daemon_roles(HostOs::Macos), &[Role::DaemonUser]);
        assert!(daemon_roles(HostOs::Other).is_empty());
        // Every listed Linux/macOS role has a scope; nothing else does.
        for os in [HostOs::Linux, HostOs::Macos] {
            for role in daemon_roles(os) {
                assert!(scope_for(os, *role).is_some(), "{os:?} {role:?}");
            }
            assert_eq!(scope_for(os, Role::DaemonMachine), None);
            assert_eq!(scope_for(os, Role::TunnelClient), None);
        }
        assert_eq!(
            scope_for(HostOs::Linux, Role::DaemonSystem),
            Some(Scope::System)
        );
        assert_eq!(
            scope_for(HostOs::Linux, Role::DaemonUser),
            Some(Scope::PerUser)
        );
        assert_eq!(
            scope_for(HostOs::Macos, Role::DaemonUser),
            Some(Scope::PerUser)
        );
        assert_eq!(
            scope_for(HostOs::Macos, Role::DaemonSystem),
            None,
            "the root half is B2"
        );
        for role in [
            Role::DaemonSystem,
            Role::DaemonMachine,
            Role::DaemonUser,
            Role::TunnelClient,
        ] {
            assert_eq!(
                scope_for(HostOs::Windows, role),
                None,
                "Windows takes the MSI pipeline"
            );
        }
        assert_eq!(Scope::PerUser.wire(), "per-user");
        assert_eq!(Scope::System.wire(), "system");
        let expected = match std::env::consts::OS {
            "windows" => "windows",
            "linux" => "linux",
            "macos" => "macos",
            _ => "other",
        };
        assert_eq!(HostOs::current().wire(), expected);
    }

    #[test]
    fn from_state_str_parses_known_and_rejects_unknown() {
        assert_eq!(
            Role::from_state_str("daemon-system"),
            Some(Role::DaemonSystem)
        );
        assert_eq!(Role::from_state_str("daemon-user"), Some(Role::DaemonUser));
        assert_eq!(
            Role::from_state_str("daemon-machine"),
            Some(Role::DaemonMachine)
        );
        assert_eq!(
            Role::from_state_str("tunnel-client"),
            Some(Role::TunnelClient)
        );
        assert_eq!(Role::from_state_str("role-from-the-future"), None);
        assert_eq!(Role::from_state_str(""), None);
        // The serde form is the ONLY accepted spelling — PascalCase /
        // snake_case variants from a hand-edited state file re-prompt.
        assert_eq!(Role::from_state_str("DaemonSystem"), None);
        assert_eq!(Role::from_state_str("daemon_system"), None);
    }
}
