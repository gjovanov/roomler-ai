// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! P3b-1a — a session's egress allowlist: the vault's answer to
//! `rc:vault.egress`, whether a session's commands may reach one host
//! through the session's egress proxy (decision 27;
//! `docs/fr/FR-90-hive-agent-sessions.md` §3e, "P3b, `placeholder` egress,
//! as designed").
//!
//! The session's device asks over its own control connection, in the queue
//! its leases share, once per host and run, and the vault answers on that
//! connection: `rc:vault.egress.allowed`, naming the host as it was decided,
//! or one word (`rc:vault.refused`, [`LeaseRefusal`]).
//!
//! | Check, in order | Refused as | Recorded |
//! |---|---|---|
//! | the request within its bounds: a host [`canonical_net_host`] spells, and a port | `bad_request` | no |
//! | the vault serves the organization | `not_served` | no |
//! | the device takes leases: its hello advertised `vault` and carried a key | `unsupported` | no |
//! | a session of the organization whose record places it on THIS device | `not_located` | no |
//! | at the record's fence | `stale_fence` | yes |
//! | live | `ended` | yes |
//! | within the session's per-minute ceiling, which its leases share | `busy` | no |
//! | holding a vault role | `no_role` | yes |
//! | Cedar's `net:egress`, the device's facts read from its ROW | `denied` | yes, with the request |
//! | the allow recorded | `undecided` | — |
//!
//! ⚠️ The allow is recorded BEFORE it is answered: an allow the vault could
//! not record is never given.
//!
//! ⚠️ A host is decided in its one spelling ([`canonical_net_host`]), and
//! answered in exactly that: `PyPI.org.` is decided, and answered, as
//! `pypi.org`, so no spelling steps around a `forbid`.
//!
//! ⚠️ The vault seeds no permit, and no guardrail permits: an organization's
//! sessions reach no host until one of its own policies names it.
//!
//! ⚠️ An allow is no lease and no grant: nothing revokes it, and nothing but
//! the daemon that asked acts on it. The device keeps it, and a denial, for
//! the session's run there (decision 27).

use bson::oid::ObjectId;
use roomler_ai_remote_control::vault::{LeaseRefusal, canonical_net_host, lease_limits};
use tracing::warn;

use crate::VaultState;
use crate::lease::{Conn, identify, os_name, reason_of};
use crate::policy::{self, Ask, DeviceFacts, SessionFacts};
use crate::policy_routes::decide_for_org;

/// One `rc:vault.egress`, as it arrived.
#[derive(Debug, Clone)]
pub struct EgressRequest {
    pub req_id: String,
    pub session_id: ObjectId,
    pub fence: u64,
    pub host: String,
    pub port: u16,
}

/// The host in its one spelling, and the port, once the request is within
/// its bounds; `None` past a bound.
fn within_bounds(req: &EgressRequest) -> Option<(String, u16)> {
    if req.req_id.is_empty()
        || req.req_id.len() > lease_limits::MAX_REQ_ID_LEN
        || req.req_id.chars().any(char::is_control)
        || req.port == 0
    {
        return None;
    }
    Some((canonical_net_host(&req.host)?, req.port))
}

/// What a row names as asked: `host:port`, an IPv6 address in brackets so
/// its port stays its own.
fn asked(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Decide one request: the host as it was decided, and the port, when it is
/// allowed.
pub async fn decide(
    state: &VaultState,
    conn: Conn,
    req: &EgressRequest,
) -> Result<(String, u16), LeaseRefusal> {
    let Some((host, port)) = within_bounds(req) else {
        return Err(LeaseRefusal::BadRequest);
    };
    let target = asked(&host, port);
    let (agent, session, rec) = identify(
        state,
        conn,
        req.session_id,
        req.fence,
        &target,
        policy::ACTION_NET_EGRESS,
        "reach",
    )
    .await?;
    if session.vault_role.is_none() {
        let why = LeaseRefusal::NoRole;
        return Err(rec.refuse(why, why.wire()).await);
    }
    let device = match DeviceFacts::new(os_name(agent.os), &agent.tags, agent.owner_user_id) {
        Ok(d) => d,
        Err(why) => {
            return Err(rec
                .refuse(LeaseRefusal::Denied, &format!("device: {why}"))
                .await);
        }
    };
    let ask = Ask::NetEgress {
        session: SessionFacts {
            id: req.session_id.to_hex(),
            owner: session.owner_id,
            role: session.vault_role.clone(),
            playbook: None,
        },
        host: host.clone(),
        port,
        device,
    };
    let verdict = decide_for_org(state, conn.tenant_id, &ask).await;
    let reason = reason_of(&verdict);
    if !verdict.allow {
        if let Err(e) = rec.decided(false, &reason, &ask, None).await {
            warn!(session = %req.session_id, %e, "vault: an egress deny was not recorded");
        }
        return Err(LeaseRefusal::Denied);
    }
    if let Err(e) = rec.decided(true, &reason, &ask, None).await {
        warn!(session = %req.session_id, %e, "vault: an egress allow was not recorded — not given");
        return Err(LeaseRefusal::Undecided);
    }
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(host: &str, port: u16) -> EgressRequest {
        EgressRequest {
            req_id: "e1".into(),
            session_id: ObjectId::new(),
            fence: 1,
            host: host.into(),
            port,
        }
    }

    /// A request names a host in a spelling the vault can decide, and a
    /// port; the host is taken in its one spelling, and anything else is
    /// refused before the session is read.
    #[test]
    fn a_request_names_a_host_and_a_port() {
        for (host, one) in [
            ("pypi.org", "pypi.org"),
            ("PyPI.org.", "pypi.org"),
            ("::ffff:10.0.0.5", "10.0.0.5"),
            ("2001:DB8::1", "2001:db8::1"),
        ] {
            assert_eq!(
                within_bounds(&req(host, 443)),
                Some((one.to_string(), 443)),
                "{host}"
            );
        }
        for bad in [
            req("pypi.org", 0),
            req("", 443),
            req("pypi.org:443", 443),
            req("010.0.0.5", 443),
            req("a b", 443),
            req(&"a".repeat(lease_limits::MAX_NET_HOST_LEN + 1), 443),
        ] {
            assert_eq!(within_bounds(&bad), None, "{bad:?}");
        }
        for req_id in [
            String::new(),
            "a\nb".to_string(),
            "r".repeat(lease_limits::MAX_REQ_ID_LEN + 1),
        ] {
            let r = EgressRequest {
                req_id,
                ..req("pypi.org", 443)
            };
            assert_eq!(within_bounds(&r), None, "{r:?}");
        }
    }

    /// A row names what was asked so that an address keeps its port apart.
    #[test]
    fn a_row_names_the_host_and_its_port() {
        assert_eq!(asked("pypi.org", 443), "pypi.org:443");
        assert_eq!(asked("10.0.0.5", 22), "10.0.0.5:22");
        assert_eq!(asked("2001:db8::1", 443), "[2001:db8::1]:443");
    }
}
