// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P0b-2b — host state this daemon changes that must not outlive it.
//!
//! A subnet router or exit node on Windows turns IPv4 forwarding on, on the
//! overlay adapter and on the egress adapter of every route it serves:
//! Windows ships every interface `Forwarding: Disabled` and forwards a packet
//! only when both ends have it enabled. The revert used to run `Remove-NetNat`
//! alone, so a device that stopped serving kept routing between its adapters
//! for as long as it ran (#2077, defect 2). An unclean exit (a crash, `kill`,
//! a watchdog or self-update `process::exit`) skips every `Drop`, so the
//! revert must also be reachable from the context-free boot purge.
//!
//! Two parts:
//! - [`ForwardingHolds`]: the in-process record, refcounted per interface.
//!   Two orgs' routers can share one egress adapter, and the first to stop
//!   must not switch forwarding off under the other. The FIRST holder's
//!   observation decides whether the interface is ours to restore: a later
//!   holder sees `Enabled` because we enabled it.
//! - [`HostLedger`]: the same facts on disk, at one fixed path per OS, so the
//!   boot purge can restore what a crashed run turned on. Only stable
//!   identities are written (an adapter GUID): an ifIndex can name a
//!   different interface after a reboot.

#[cfg(any(windows, test))]
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One interface whose forwarding a router asked for, as the setup script
/// reported it. `identity` is `guid:{…}` when the adapter has a GUID, else
/// `if:<ifIndex>`; only the first form is persisted.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FwdTarget {
    pub(crate) identity: String,
    pub(crate) if_index: u32,
    /// The interface had forwarding OFF before this setup turned it on.
    pub(crate) was_disabled: bool,
}

/// An adapter GUID exactly as `Get-NetAdapter` prints it: `{8-4-4-4-12}` in
/// hex. Strict, because the value is put back into a PowerShell command, and
/// the ledger it comes from is read back from disk at boot.
pub fn is_guid(s: &str) -> bool {
    let Some(inner) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return false;
    };
    let groups: Vec<&str> = inner.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Parse the setup script's `FWD <guid|-> <ifIndex> <Enabled|Disabled>` lines.
/// Anything else on stdout is ignored, so the script may print diagnostics.
#[cfg(any(windows, test))]
pub(crate) fn parse_forwarding_report(out: &str) -> Vec<FwdTarget> {
    out.lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            if cols.next()? != "FWD" {
                return None;
            }
            let guid = cols.next()?;
            let if_index: u32 = cols.next()?.parse().ok()?;
            let before = cols.next()?;
            let identity = if is_guid(guid) {
                format!("guid:{guid}")
            } else {
                format!("if:{if_index}")
            };
            Some(FwdTarget {
                identity,
                if_index,
                was_disabled: before.eq_ignore_ascii_case("Disabled"),
            })
        })
        .collect()
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hold {
    /// Forwarding was off before the first holder turned it on.
    restore: bool,
    holders: usize,
}

/// FR-97 P0b-2b — who holds forwarding on which interface, this process.
#[cfg(any(windows, test))]
#[derive(Debug, Default)]
pub(crate) struct ForwardingHolds {
    by_identity: BTreeMap<String, Hold>,
}

#[cfg(any(windows, test))]
impl ForwardingHolds {
    pub(crate) const fn new() -> Self {
        Self {
            by_identity: BTreeMap::new(),
        }
    }

    /// A router now relies on forwarding on `t`'s interface.
    pub(crate) fn hold(&mut self, t: &FwdTarget) {
        self.by_identity
            .entry(t.identity.clone())
            .and_modify(|h| h.holders += 1)
            .or_insert(Hold {
                restore: t.was_disabled,
                holders: 1,
            });
    }

    /// A router no longer relies on it. `true` when this was the last holder
    /// and forwarding was off before we turned it on: switch it back off.
    pub(crate) fn release(&mut self, identity: &str) -> bool {
        let Some(h) = self.by_identity.get_mut(identity) else {
            return false;
        };
        h.holders = h.holders.saturating_sub(1);
        if h.holders > 0 {
            return false;
        }
        let restore = h.restore;
        self.by_identity.remove(identity);
        restore
    }

    /// The persisted half: the GUID-identified interfaces we turned on and
    /// still hold. An `if:` identity is not written: after a reboot that
    /// index can belong to a different interface.
    pub(crate) fn ledger(&self) -> HostLedger {
        HostLedger {
            forwarding: self
                .by_identity
                .iter()
                .filter(|(id, h)| h.restore && id.starts_with("guid:"))
                .map(|(id, _)| id.trim_start_matches("guid:").to_string())
                .collect(),
        }
    }
}

/// FR-97 P0b-2b — what the boot purge must undo after an unclean exit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLedger {
    /// Windows adapter GUIDs (`{…}`) whose IPv4 forwarding this daemon turned
    /// on and had not yet turned off.
    #[serde(default)]
    pub forwarding: Vec<String>,
}

/// The ledger's one path per OS. Machine-wide on purpose: one daemon per
/// machine owns this state (`docs/multi-org.md`), and the boot purge runs
/// before any config is read. `None` where nothing is recorded yet.
pub fn ledger_path() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData").map(|d| {
            std::path::PathBuf::from(d)
                .join("roomler")
                .join("overlay-host-ledger.json")
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

impl HostLedger {
    /// Read the ledger; an absent or unreadable file is an empty one.
    pub fn load() -> Self {
        ledger_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write the ledger atomically (temp file, then rename); an empty ledger
    /// removes the file. Best-effort: a daemon that cannot write it keeps
    /// working, and only loses the crash-time restore.
    pub fn store(&self) {
        let Some(path) = ledger_path() else {
            return;
        };
        if self.forwarding.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let Ok(body) = serde_json::to_vec_pretty(self) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUID_A: &str = "{4D36E972-E325-11CE-BFC1-08002BE10318}";
    const GUID_B: &str = "{11111111-2222-3333-4444-555555555555}";

    #[test]
    fn the_report_names_each_interface_and_its_state_before() {
        let out = format!(
            "noise from PowerShell\nFWD {GUID_A} 12 Disabled\nFWD - 31 Enabled\nFWD {GUID_B} x Disabled\nFWD\n"
        );
        let got = parse_forwarding_report(&out);
        assert_eq!(
            got,
            vec![
                FwdTarget {
                    identity: format!("guid:{GUID_A}"),
                    if_index: 12,
                    was_disabled: true,
                },
                FwdTarget {
                    identity: "if:31".into(),
                    if_index: 31,
                    was_disabled: false,
                },
            ],
            "a malformed index or a short line is skipped"
        );
    }

    fn target(guid: &str, before: &str) -> FwdTarget {
        parse_forwarding_report(&format!("FWD {guid} 7 {before}"))
            .pop()
            .unwrap()
    }

    /// The defect: forwarding stayed on after the router stopped. Restored
    /// only by the LAST holder, and only where we had turned it on.
    #[test]
    fn forwarding_we_turned_on_is_restored_by_the_last_holder() {
        let mut holds = ForwardingHolds::default();
        // Org A turns it on; org B, sharing the egress adapter, then sees it
        // Enabled. That must not erase A's observation.
        holds.hold(&target(GUID_A, "Disabled"));
        holds.hold(&target(GUID_A, "Enabled"));
        let id = format!("guid:{GUID_A}");
        assert!(!holds.release(&id), "B still routes: not yet");
        assert!(holds.release(&id), "the last holder restores");
        assert!(!holds.release(&id), "and only once");
    }

    #[test]
    fn forwarding_that_was_already_on_is_left_on() {
        let mut holds = ForwardingHolds::default();
        holds.hold(&target(GUID_A, "Enabled"));
        assert!(
            !holds.release(&format!("guid:{GUID_A}")),
            "someone else's setting, never ours to turn off"
        );
    }

    /// Only stable identities reach the disk, and only those we must restore.
    #[test]
    fn the_ledger_holds_only_guids_we_turned_on() {
        let mut holds = ForwardingHolds::default();
        holds.hold(&target(GUID_A, "Disabled"));
        holds.hold(&target(GUID_B, "Enabled"));
        holds.hold(&parse_forwarding_report("FWD - 9 Disabled").pop().unwrap());
        assert_eq!(holds.ledger().forwarding, vec![GUID_A.to_string()]);
        holds.release(&format!("guid:{GUID_A}"));
        assert!(holds.ledger().forwarding.is_empty());
    }

    #[test]
    fn only_a_well_formed_guid_is_a_guid() {
        assert!(is_guid(GUID_A));
        for bad in [
            "",
            "-",
            "{}",
            "4D36E972-E325-11CE-BFC1-08002BE10318",
            "{4D36E972-E325-11CE-BFC1-08002BE1031}",
            "{4D36E972-E325-11CE-BFC1-08002BE1031Z}",
            "{4D36E972-E325-11CE-BFC1'; Stop-Computer; '}",
        ] {
            assert!(!is_guid(bad), "{bad}");
        }
        // A malformed GUID falls back to the ifIndex, which is never persisted.
        let t = parse_forwarding_report("FWD {not-a-guid} 5 Disabled")
            .pop()
            .unwrap();
        assert_eq!(t.identity, "if:5");
    }

    #[test]
    fn the_ledger_round_trips_and_tolerates_a_missing_field() {
        let l = HostLedger {
            forwarding: vec![GUID_A.into()],
        };
        let back: HostLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        let empty: HostLedger = serde_json::from_str("{}").unwrap();
        assert!(empty.forwarding.is_empty());
    }
}
