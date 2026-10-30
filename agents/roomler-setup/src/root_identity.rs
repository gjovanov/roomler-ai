// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 B1 (F3) — the running root daemon's own word on which device it is,
//! asked before a system-scope enroll.
//!
//! The wizard enrolls a system scope as the person, who cannot read the root
//! daemon's `/etc/roomler/config.toml` and so cannot reuse its machine id:
//! the enroll derives one from the host name, and on a host renamed since —
//! or a VM cloned together with its config — the server makes a NEW device
//! row. The privileged half's `roomlerd adopt-enrollment` catches that from
//! the files, as root, and retires the WireGuard key the previous row still
//! claims (`enrollment::retire_key_of_previous_row`); this module lets the
//! wizard say it on its own page as well, before the half runs.
//!
//! What the daemon tells the console user, over the person socket a Linux
//! root daemon serves in their runtime directory (FR-27 P11, `Scope::Person`,
//! which admits `status`): its agent id (`node_id`), its org and its name —
//! never its machine id, which is why the wizard cannot simply enroll with it
//! and have the server reuse the row. (Exposing it would not be the fix
//! either: on a cloned VM the previous row is the ORIGINAL machine's, in use,
//! and reusing it would make two machines one device.)
//!
//! Best-effort and bounded: no daemon, an older daemon with no person socket,
//! a per-user daemon of the same person on that path (its socket is not
//! root's, and is refused), or a slow answer — each reads as "nothing known",
//! and the privileged half still does the part that matters.

use std::time::Duration;

/// How long the question may take.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// The root daemon's primary enrollment, as it reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootIdentity {
    /// The device row it runs as (hex agent id).
    pub agent_id: String,
    pub tenant_id: Option<String>,
    /// Its device name.
    pub name: String,
}

impl RootIdentity {
    /// What to tell the person when the enrollment just made is ANOTHER
    /// device row of this daemon's org: the server did not recognise the
    /// machine. `None` when it is the same row, or another org.
    pub fn new_row_warning(&self, new_agent_id: &str, new_tenant_id: &str) -> Option<String> {
        if self.agent_id.is_empty()
            || self.agent_id == new_agent_id
            || self.tenant_id.as_deref() != Some(new_tenant_id)
        {
            return None;
        }
        let row = roomlerd::enrollment::PreviousRow {
            agent_id: self.agent_id.clone(),
            org_label: roomlerd::config::PRIMARY_ORG_LABEL.to_string(),
        };
        let said = roomlerd::enrollment::describe_previous_row(&row, new_agent_id);
        Some(format!(
            "The root daemon here runs as \"{}\": {said}",
            self.name
        ))
    }
}

/// Ask the root daemon. `None` whenever it cannot say (see the module doc).
pub async fn query() -> Option<RootIdentity> {
    #[cfg(target_os = "linux")]
    {
        tokio::time::timeout(QUERY_TIMEOUT, query_linux())
            .await
            .ok()
            .flatten()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
async fn query_linux() -> Option<RootIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    // This process's uid: `/proc/self` is owned by it.
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let path = roomler_localapi::person_socket_path(uid);
    // Only a socket ROOT serves: a per-user daemon of the same person binds
    // the same path, and its answer would be about another device.
    let mut client = roomler_localapi::connect_unix_served_by_root(path)
        .await
        .ok()?;
    let status = client.status().await.ok()?;
    // A root daemon on another config is not the system scope's.
    if status
        .config_path
        .as_deref()
        .is_some_and(|p| p != crate::unix_daemon::SYSTEM_CONFIG)
    {
        return None;
    }
    Some(RootIdentity {
        agent_id: status.node_id,
        tenant_id: status.tenant_id,
        name: status.name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> RootIdentity {
        RootIdentity {
            agent_id: "aid-old".into(),
            tenant_id: Some("tid".into()),
            name: "lab-box".into(),
        }
    }

    /// Said only for another row of the same org.
    #[test]
    fn a_new_row_of_the_same_org_is_said() {
        let w = root().new_row_warning("aid-new", "tid").expect("said");
        assert!(w.contains("\"lab-box\""), "{w}");
        assert!(w.contains("aid-old") && w.contains("aid-new"), "{w}");
        assert!(
            root().new_row_warning("aid-old", "tid").is_none(),
            "the same row"
        );
        assert!(
            root().new_row_warning("aid-new", "other").is_none(),
            "another org"
        );
        let unknown = RootIdentity {
            tenant_id: None,
            ..root()
        };
        assert!(unknown.new_row_warning("aid-new", "tid").is_none());
    }

    /// Nothing to ask on a test host (no root daemon serves this uid's
    /// person socket, or this is not Linux): `None`, quickly.
    #[tokio::test]
    async fn no_daemon_is_nothing_known() {
        let begun = std::time::Instant::now();
        let _ = query().await;
        assert!(begun.elapsed() <= QUERY_TIMEOUT + Duration::from_secs(1));
    }
}
