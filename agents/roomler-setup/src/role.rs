//! The unified wizard's role picker vocabulary.
//!
//! One `Role` = one thing the operator wants this machine to be:
//!
//! - [`Role::DaemonSystem`] — "be accessed, even pre-logon": perMachine
//!   MSI + the SystemContext swap (`ENABLE_SYSTEM_CONTEXT=1` WiX CA →
//!   SCM Environment write + service restart). Windows-only.
//! - [`Role::DaemonUser`] — "be accessed, this user only": perUser MSI
//!   (Scheduled-Task autostart under the installing user; no UAC).
//!   Windows-only.
//! - [`Role::DaemonMachine`] — "be accessed, attended": plain
//!   perMachine MSI (SCM service, no SystemContext; UAC once during
//!   install). Windows-only.
//! - [`Role::TunnelClient`] — "reach others only": the roomler tunnel
//!   CLI, delivered as an archive + PATH integration. The ONLY role
//!   offered on non-Windows hosts.
//!
//! Role→action COMPOSITION (P4b): a daemon role ALSO delivers the
//! CLI — not by running a second pipeline, but because the daemon
//! MSIs carry `roomler.exe` (wxs `TunnelExe` component) + a PATH
//! append since P4b. Each role therefore still runs exactly one
//! orchestrator: the three daemon roles map onto
//! [`crate::orchestrator_agent`] (which existence-checks the
//! MSI-carried CLI and surfaces it via `DoneReport.cli_included`),
//! the tunnel role onto [`crate::orchestrator_tunnel`].

use serde::{Deserialize, Serialize};

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
