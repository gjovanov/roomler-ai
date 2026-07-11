//! ACL evaluation primitives.
//!
//! Data shapes are canonical in `roomler_ai_remote_control::models`
//! (single source of truth for both the DB rows and the eval logic).
//! This module re-exports them and adds the pure-function evaluator.
//!
//! Evaluation is server-side per plan §7 (defence-in-depth) — the
//! agent runs its own minimal allowlist as a second gate, but the
//! authoritative decision is the server's.
//!
//! Cross-tenant gating happens UPSTREAM of `evaluate` — the caller
//! must filter `policies` to a single tenant before calling here.
//! `evaluate` does NOT receive a `tenant_id` to enforce this
//! contract explicitly: there's no field for it to check, so the
//! responsibility lands cleanly on the caller (the WS handler in
//! `api/ws/tunnel.rs`). The integration test in
//! `crates/tests/src/tunnel_tests.rs` locks the cross-tenant gate.

use bson::oid::ObjectId;

pub use roomler_ai_remote_control::models::{
    Agent, AgentStatus, DestinationRule, HostPattern, PolicySubject, PolicyTarget, PortRange,
    ProtocolKind, TunnelPolicy,
};
pub use roomler_ai_remote_control::signaling::RejectKind;

/// The concrete principal that ORIGINATED a tunnel request. Historically
/// this was always a `TunnelClient`; the node-stack unification (P3b-2) lets
/// an enrolled **agent** originate tunnels over its own WS, so the principal
/// is now a typed union rather than a bare `tunnel_client_id`. `AllUsers` /
/// `UserId{owner}` / `RoleId` policy subjects match EITHER principal (they key
/// on `user_id`/`role_ids`); only the id-specific subjects
/// (`TunnelClientId` / `AgentId`) discriminate on the principal kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    TunnelClient(ObjectId),
    Agent(ObjectId),
}

/// Concrete identity for the requesting tunnel origin. The caller
/// resolves this once per request (from the WS handler's auth
/// context + a Mongo lookup for `role_ids`) and hands it to
/// [`evaluate`].
#[derive(Debug, Clone)]
pub struct ResolvedSubject {
    pub user_id: ObjectId,
    pub role_ids: Vec<ObjectId>,
    pub principal: Principal,
}

/// Outcome of an ACL evaluation. On allow, carries the policy id +
/// the rule that matched + the per-policy ceilings so the caller
/// can plumb them into the per-session counters. On deny, carries
/// a human-readable reason — the caller maps that to the wire-level
/// `RejectKind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow {
        policy_id: ObjectId,
        rule: DestinationRule,
        max_concurrent_flows: Option<u32>,
        max_bytes_per_session: Option<u64>,
    },
    Deny {
        reason: String,
    },
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }
}

/// Run the full ACL check: does any policy in `policies` permit
/// `subject` to reach `(dst_host, dst_port)` on `agent_id`?
///
/// First-match-wins semantics — policies are evaluated in the order
/// the caller supplies them (typically `created_at DESC` for newest-
/// wins). Default-deny if no policy matches.
///
/// Cross-tenant: callers MUST pre-filter `policies` to the
/// agent's tenant. See module docs.
pub fn evaluate(
    policies: &[TunnelPolicy],
    subject: &ResolvedSubject,
    agent_id: ObjectId,
    dst_host: &str,
    dst_port: u16,
    proto: ProtocolKind,
) -> Decision {
    for policy in policies {
        if policy.deleted_at.is_some() {
            continue;
        }
        if !subject_matches(&policy.subjects, subject) {
            continue;
        }
        if !target_matches(&policy.targets, agent_id) {
            continue;
        }
        if let Some(rule) = policy
            .allowlist
            .iter()
            .find(|r| r.proto.permits(proto) && dst_matches(r, dst_host, dst_port))
        {
            return Decision::Allow {
                policy_id: policy.id.unwrap_or_else(ObjectId::new),
                rule: rule.clone(),
                max_concurrent_flows: policy.max_concurrent_flows,
                max_bytes_per_session: policy.max_bytes_per_session,
            };
        }
    }
    Decision::Deny {
        reason: "no policy matches".into(),
    }
}

/// Does any `PolicySubject` in `subjects` match the requesting
/// `ResolvedSubject`? `AllUsers` is the catch-all.
pub fn subject_matches(subjects: &[PolicySubject], req: &ResolvedSubject) -> bool {
    subjects.iter().any(|s| match s {
        PolicySubject::AllUsers => true,
        PolicySubject::UserId { user_id } => *user_id == req.user_id,
        PolicySubject::RoleId { role_id } => req.role_ids.contains(role_id),
        // Id-specific subjects discriminate on the principal KIND: a
        // `TunnelClientId` subject only matches a tunnel-client principal, an
        // `AgentId` subject only an agent principal. Never cross the kinds —
        // an agent_id must not satisfy a tunnel_client_id subject even on a
        // (vanishingly unlikely) ObjectId collision.
        PolicySubject::TunnelClientId { tunnel_client_id } => {
            matches!(req.principal, Principal::TunnelClient(id) if id == *tunnel_client_id)
        }
        PolicySubject::AgentId { agent_id } => {
            matches!(req.principal, Principal::Agent(id) if id == *agent_id)
        }
    })
}

/// Does any `PolicyTarget` in `targets` match `agent_id`? `AllAgents`
/// is the catch-all (within the policy's tenant).
pub fn target_matches(targets: &[PolicyTarget], agent_id: ObjectId) -> bool {
    targets.iter().any(|t| match t {
        PolicyTarget::AllAgents => true,
        PolicyTarget::AgentId { agent_id: id } => *id == agent_id,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Server-side ACL gate (T2.4)
// ────────────────────────────────────────────────────────────────────────────

/// Result of the full server-side gate for a `TcpForwardRequest`.
/// On allow, carries the per-policy ceilings so the caller plumbs
/// them into the per-session counters. On reject, carries the wire-
/// level `RejectKind` + a human-readable reason — the caller maps
/// these straight into a `TcpForwardReject` ServerMsg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResult {
    Allow {
        policy_id: ObjectId,
        rule: DestinationRule,
        max_concurrent_flows: Option<u32>,
        max_bytes_per_session: Option<u64>,
    },
    Reject {
        kind: RejectKind,
        reason: String,
    },
}

impl GateResult {
    pub fn is_allow(&self) -> bool {
        matches!(self, GateResult::Allow { .. })
    }
}

/// The full server-side gate that runs on every `TcpForwardRequest`.
/// Pure function — caller pre-fetches the agent row + the active
/// policies for the agent's tenant.
///
/// Sequence:
/// 1. **Cross-tenant gate** (plan §"Multi-tenancy gotcha") —
///    `client_tenant_id` MUST equal `agent.tenant_id`. Defence-in-
///    depth: the WS upgrade's tenant_id check already covers this,
///    but a cross-tenant `agent_id` snuck in via a forwarded
///    `TcpForwardRequest` must still bounce.
/// 2. **Agent availability** — refuse if the agent is soft-deleted
///    or quarantined. (Status reads as `Offline` are still allowed —
///    the WS may simply not be live; the request will fail at the
///    relay step with a connection-refused.)
/// 3. **ACL eval** — see [`evaluate`].
///
/// Locked by the `cross_tenant_gate_blocks_*` tests below.
pub fn check_forward_request(
    client_tenant_id: ObjectId,
    agent: &Agent,
    policies: &[TunnelPolicy],
    subject: &ResolvedSubject,
    dst_host: &str,
    dst_port: u16,
    proto: ProtocolKind,
) -> GateResult {
    // 1. Cross-tenant gate
    if client_tenant_id != agent.tenant_id {
        return GateResult::Reject {
            kind: RejectKind::CrossTenant,
            reason: format!(
                "client tenant {} ≠ agent tenant {}",
                client_tenant_id, agent.tenant_id
            ),
        };
    }
    // 2. Agent availability
    if agent.deleted_at.is_some() {
        return GateResult::Reject {
            kind: RejectKind::AgentError,
            reason: "agent has been deleted".into(),
        };
    }
    if matches!(agent.status, AgentStatus::Quarantined) {
        return GateResult::Reject {
            kind: RejectKind::AgentError,
            reason: "agent is quarantined".into(),
        };
    }
    // 3. ACL eval
    let agent_id = match agent.id {
        Some(id) => id,
        None => {
            return GateResult::Reject {
                kind: RejectKind::AgentError,
                reason: "agent missing _id".into(),
            };
        }
    };
    match evaluate(policies, subject, agent_id, dst_host, dst_port, proto) {
        Decision::Allow {
            policy_id,
            rule,
            max_concurrent_flows,
            max_bytes_per_session,
        } => GateResult::Allow {
            policy_id,
            rule,
            max_concurrent_flows,
            max_bytes_per_session,
        },
        Decision::Deny { reason } => GateResult::Reject {
            kind: RejectKind::AclDenied,
            reason,
        },
    }
}

/// Match a `(dst_host, dst_port)` tuple against a single destination
/// rule. T2.3 wires this into the full
/// `evaluate(policies, subject, agent, dst)` flow.
pub fn dst_matches(rule: &DestinationRule, dst_host: &str, dst_port: u16) -> bool {
    if dst_port < rule.port_range.low || dst_port > rule.port_range.high {
        return false;
    }
    host_matches(&rule.host_pattern, dst_host)
}

pub fn host_matches(pattern: &HostPattern, host: &str) -> bool {
    match pattern {
        HostPattern::Exact(s) => s.eq_ignore_ascii_case(host),
        HostPattern::Wildcard(s) => match s.strip_prefix("*.") {
            Some(suffix) => {
                host.to_ascii_lowercase()
                    .ends_with(&suffix.to_ascii_lowercase())
                    && host.len() > suffix.len()
                    && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
            }
            // A wildcard without a leading "*." is treated as exact —
            // safer than allow-all.
            None => s.eq_ignore_ascii_case(host),
        },
        HostPattern::Cidr(cidr) => match (
            cidr.parse::<ipnet::IpNet>(),
            host.parse::<std::net::IpAddr>(),
        ) {
            (Ok(net), Ok(ip)) => net.contains(&ip),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(p: HostPattern, low: u16, high: u16) -> DestinationRule {
        DestinationRule {
            host_pattern: p,
            port_range: PortRange { low, high },
            proto: ProtocolKind::Any,
        }
    }

    /// A `proto`-narrowed rule for the UDP/TCP gating tests below.
    fn rule_proto(p: HostPattern, low: u16, high: u16, proto: ProtocolKind) -> DestinationRule {
        DestinationRule {
            host_pattern: p,
            port_range: PortRange { low, high },
            proto,
        }
    }

    /// Test wrapper: `evaluate` with `proto = Tcp` — the pre-UDP
    /// default the bulk of these cases exercise. The proto-specific
    /// gating is locked separately in `proto_*` tests.
    fn eval_tcp(
        policies: &[TunnelPolicy],
        subject: &ResolvedSubject,
        agent_id: ObjectId,
        dst_host: &str,
        dst_port: u16,
    ) -> Decision {
        evaluate(
            policies,
            subject,
            agent_id,
            dst_host,
            dst_port,
            ProtocolKind::Tcp,
        )
    }

    /// Test wrapper: `check_forward_request` with `proto = Tcp`.
    fn check_tcp(
        client_tenant_id: ObjectId,
        agent: &Agent,
        policies: &[TunnelPolicy],
        subject: &ResolvedSubject,
        dst_host: &str,
        dst_port: u16,
    ) -> GateResult {
        check_forward_request(
            client_tenant_id,
            agent,
            policies,
            subject,
            dst_host,
            dst_port,
            ProtocolKind::Tcp,
        )
    }

    #[test]
    fn exact_host_matches_case_insensitive() {
        let r = rule(HostPattern::Exact("db.intranet".into()), 5432, 5432);
        assert!(dst_matches(&r, "db.intranet", 5432));
        assert!(dst_matches(&r, "DB.INTRANET", 5432));
        assert!(!dst_matches(&r, "other.intranet", 5432));
    }

    #[test]
    fn wildcard_requires_subdomain_dot() {
        let r = rule(HostPattern::Wildcard("*.intranet".into()), 1, 65535);
        assert!(dst_matches(&r, "db.intranet", 80));
        assert!(dst_matches(&r, "deeply.nested.intranet", 80));
        // Bare suffix isn't a subdomain match — "intranet" alone fails.
        assert!(!dst_matches(&r, "intranet", 80));
        // Look-alike suffix without the dot fails.
        assert!(!dst_matches(&r, "evilintranet", 80));
    }

    #[test]
    fn cidr_matches_ip() {
        let r = rule(HostPattern::Cidr("10.0.0.0/24".into()), 5432, 5432);
        assert!(dst_matches(&r, "10.0.0.5", 5432));
        assert!(dst_matches(&r, "10.0.0.255", 5432));
        assert!(!dst_matches(&r, "10.0.1.5", 5432));
        // Hostname (non-IP) on a CIDR rule fails — caller must resolve first.
        assert!(!dst_matches(&r, "db.intranet", 5432));
    }

    #[test]
    fn port_range_inclusive() {
        let r = rule(HostPattern::Exact("h".into()), 5000, 5010);
        assert!(dst_matches(&r, "h", 5000));
        assert!(dst_matches(&r, "h", 5005));
        assert!(dst_matches(&r, "h", 5010));
        assert!(!dst_matches(&r, "h", 4999));
        assert!(!dst_matches(&r, "h", 5011));
    }

    #[test]
    fn default_deny_when_pattern_unmatched() {
        let r = rule(HostPattern::Exact("a".into()), 1, 1);
        assert!(!dst_matches(&r, "b", 1));
    }

    // ─── Full eval_tcp() coverage (T2.3) ─────────────────────────────

    use bson::DateTime;

    fn policy(
        subjects: Vec<PolicySubject>,
        targets: Vec<PolicyTarget>,
        allowlist: Vec<DestinationRule>,
    ) -> TunnelPolicy {
        TunnelPolicy {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            name: "test".into(),
            subjects,
            targets,
            allowlist,
            max_concurrent_flows: None,
            max_bytes_per_session: None,
            created_at: DateTime::now(),
            updated_at: DateTime::now(),
            deleted_at: None,
        }
    }

    fn subject(user_id: ObjectId) -> ResolvedSubject {
        ResolvedSubject {
            user_id,
            role_ids: vec![],
            principal: Principal::TunnelClient(ObjectId::new()),
        }
    }

    #[test]
    fn empty_policy_list_denies() {
        let d = eval_tcp(&[], &subject(ObjectId::new()), ObjectId::new(), "h", 1);
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn user_id_exact_match_allows() {
        let uid = ObjectId::new();
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::UserId { user_id: uid }],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let d = eval_tcp(&[p], &subject(uid), aid, "db", 5432);
        assert!(d.is_allow(), "{d:?}");
    }

    #[test]
    fn user_id_mismatch_denies() {
        let uid = ObjectId::new();
        let aid = ObjectId::new();
        let other_user = ObjectId::new();
        let p = policy(
            vec![PolicySubject::UserId { user_id: uid }],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let d = eval_tcp(&[p], &subject(other_user), aid, "db", 5432);
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn role_id_match_allows_when_any_role_matches() {
        let aid = ObjectId::new();
        let role_a = ObjectId::new();
        let role_b = ObjectId::new();
        let p = policy(
            vec![PolicySubject::RoleId { role_id: role_a }],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let req = ResolvedSubject {
            user_id: ObjectId::new(),
            role_ids: vec![role_b, role_a],
            principal: Principal::TunnelClient(ObjectId::new()),
        };
        assert!(eval_tcp(&[p], &req, aid, "db", 5432).is_allow());
    }

    #[test]
    fn tunnel_client_id_match_allows() {
        let aid = ObjectId::new();
        let cid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::TunnelClientId {
                tunnel_client_id: cid,
            }],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let req = ResolvedSubject {
            user_id: ObjectId::new(),
            role_ids: vec![],
            principal: Principal::TunnelClient(cid),
        };
        assert!(eval_tcp(&[p], &req, aid, "db", 5432).is_allow());
    }

    // ─── agent-principal subject matching (P3b-2) ────────────────────

    /// Build a `ResolvedSubject` whose principal is an agent originating a
    /// tunnel (the P3b-2 case). `user_id` is the agent's owner.
    fn agent_subject(owner_user_id: ObjectId, agent_id: ObjectId) -> ResolvedSubject {
        ResolvedSubject {
            user_id: owner_user_id,
            role_ids: vec![],
            principal: Principal::Agent(agent_id),
        }
    }

    #[test]
    fn agent_id_subject_matches_agent_principal() {
        let target = ObjectId::new();
        let origin_agent = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AgentId {
                agent_id: origin_agent,
            }],
            vec![PolicyTarget::AgentId { agent_id: target }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let req = agent_subject(ObjectId::new(), origin_agent);
        assert!(eval_tcp(&[p], &req, target, "db", 5432).is_allow());
    }

    #[test]
    fn agent_principal_does_not_match_tunnel_client_subject() {
        // An `AgentId`-kinded principal must NOT satisfy a `TunnelClientId`
        // subject even if the raw ObjectId happened to be equal — the kinds
        // are disjoint. Use the SAME id for both to prove the kind gate, not
        // an id mismatch, is what denies.
        let aid = ObjectId::new();
        let shared = ObjectId::new();
        let p = policy(
            vec![PolicySubject::TunnelClientId {
                tunnel_client_id: shared,
            }],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let req = agent_subject(ObjectId::new(), shared);
        assert!(matches!(
            eval_tcp(&[p], &req, aid, "db", 5432),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn tunnel_client_principal_does_not_match_agent_subject() {
        // The reverse of the above — a tunnel-client principal must not
        // satisfy an `AgentId` subject.
        let aid = ObjectId::new();
        let shared = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AgentId { agent_id: shared }],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let req = ResolvedSubject {
            user_id: ObjectId::new(),
            role_ids: vec![],
            principal: Principal::TunnelClient(shared),
        };
        assert!(matches!(
            eval_tcp(&[p], &req, aid, "db", 5432),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn all_users_subject_matches_agent_principal() {
        // The de-risking property: an `AllUsers` policy authorizes an
        // agent-originated tunnel with no new subject type needed.
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let req = agent_subject(ObjectId::new(), ObjectId::new());
        assert!(eval_tcp(&[p], &req, aid, "db", 5432).is_allow());
    }

    #[test]
    fn user_id_subject_matches_agent_principal_by_owner() {
        // An agent principal is authorized by a `UserId{owner}` policy —
        // the owner drives the match, independent of principal kind.
        let owner = ObjectId::new();
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::UserId { user_id: owner }],
            vec![PolicyTarget::AllAgents],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let req = agent_subject(owner, ObjectId::new());
        assert!(eval_tcp(&[p], &req, aid, "db", 5432).is_allow());
    }

    #[test]
    fn all_users_subject_matches_any_user() {
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        assert!(eval_tcp(&[p], &subject(ObjectId::new()), aid, "db", 5432).is_allow());
    }

    #[test]
    fn all_agents_target_matches_any_agent() {
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        assert!(eval_tcp(&[p], &subject(ObjectId::new()), ObjectId::new(), "db", 5432).is_allow());
    }

    #[test]
    fn target_mismatch_denies() {
        let policy_agent = ObjectId::new();
        let other_agent = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AgentId {
                agent_id: policy_agent,
            }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let d = eval_tcp(&[p], &subject(ObjectId::new()), other_agent, "db", 5432);
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn first_match_wins_across_policies() {
        // Two policies — first is too restrictive (only port 5432),
        // second allows 22. Evaluator should walk through and return
        // the SECOND policy's allow when asked for port 22.
        let aid = ObjectId::new();
        let p1 = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let p2 = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule(HostPattern::Exact("ssh".into()), 22, 22)],
        );
        assert!(eval_tcp(&[p1, p2], &subject(ObjectId::new()), aid, "ssh", 22).is_allow());
    }

    #[test]
    fn soft_deleted_policy_is_ignored() {
        let aid = ObjectId::new();
        let mut p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        p.deleted_at = Some(DateTime::now());
        // Even though the policy would otherwise match, the soft-
        // deleted flag stops it. Defence in depth — DAO's
        // list_active_for_tenant already filters, but a stale cache
        // shouldn't allow access.
        let d = eval_tcp(&[p], &subject(ObjectId::new()), aid, "db", 5432);
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn multiple_destination_rules_any_match_allows() {
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![
                rule(HostPattern::Exact("db".into()), 5432, 5432),
                rule(HostPattern::Exact("ssh".into()), 22, 22),
            ],
        );
        assert!(eval_tcp(&[p], &subject(ObjectId::new()), aid, "ssh", 22).is_allow());
    }

    #[test]
    fn dst_mismatch_with_subject_target_match_still_denies() {
        // Subject + target match, but dst is not allowlisted —
        // strict default-deny on the destination axis.
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
        );
        let d = eval_tcp(&[p], &subject(ObjectId::new()), aid, "evil-dst", 5432);
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn allow_carries_policy_id_and_rule() {
        let aid = ObjectId::new();
        let uid = ObjectId::new();
        let r = rule(HostPattern::Exact("db".into()), 5432, 5432);
        let mut p = policy(
            vec![PolicySubject::UserId { user_id: uid }],
            vec![PolicyTarget::AgentId { agent_id: aid }],
            vec![r.clone()],
        );
        p.max_concurrent_flows = Some(32);
        p.max_bytes_per_session = Some(1024 * 1024 * 1024);
        let expected_pid = p.id.unwrap();

        match eval_tcp(&[p], &subject(uid), aid, "db", 5432) {
            Decision::Allow {
                policy_id,
                rule: matched,
                max_concurrent_flows,
                max_bytes_per_session,
            } => {
                assert_eq!(policy_id, expected_pid);
                assert_eq!(matched, r);
                assert_eq!(max_concurrent_flows, Some(32));
                assert_eq!(max_bytes_per_session, Some(1024 * 1024 * 1024));
            }
            d => panic!("expected allow, got {d:?}"),
        }
    }

    #[test]
    fn deny_carries_human_reason() {
        let d = eval_tcp(&[], &subject(ObjectId::new()), ObjectId::new(), "h", 1);
        match d {
            Decision::Deny { reason } => assert!(!reason.is_empty()),
            _ => panic!("expected deny"),
        }
    }

    #[test]
    fn cidr_target_works_end_to_end() {
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule(HostPattern::Cidr("10.0.0.0/24".into()), 5432, 5432)],
        );
        assert!(
            eval_tcp(
                std::slice::from_ref(&p),
                &subject(ObjectId::new()),
                aid,
                "10.0.0.5",
                5432
            )
            .is_allow()
        );
        assert!(matches!(
            eval_tcp(&[p], &subject(ObjectId::new()), aid, "10.1.0.5", 5432),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn subject_matches_helper_covers_all_users_alone() {
        let req = subject(ObjectId::new());
        assert!(subject_matches(&[PolicySubject::AllUsers], &req));
        assert!(!subject_matches(&[], &req));
    }

    #[test]
    fn target_matches_helper_covers_all_agents_alone() {
        assert!(target_matches(&[PolicyTarget::AllAgents], ObjectId::new()));
        assert!(!target_matches(&[], ObjectId::new()));
    }

    // ─── Server-side gate (T2.4) ─────────────────────────────────────

    use roomler_ai_remote_control::models::{AgentCaps, OsKind};

    fn agent_for(tenant_id: ObjectId, status: AgentStatus, deleted: bool) -> Agent {
        Agent {
            id: Some(ObjectId::new()),
            tenant_id,
            owner_user_id: ObjectId::new(),
            enrolled_by: None,
            name: "test-agent".into(),
            machine_id: "m".into(),
            os: OsKind::Linux,
            agent_version: "0".into(),
            agent_token_hash: String::new(),
            status,
            last_seen_at: DateTime::now(),
            displays: vec![],
            capabilities: AgentCaps::default(),
            access_policy: Default::default(),
            routes: Vec::new(),
            advertised_routes: Vec::new(),
            created_at: DateTime::now(),
            updated_at: DateTime::now(),
            deleted_at: deleted.then(DateTime::now),
        }
    }

    fn allow_all_policy_for(tenant_id: ObjectId) -> TunnelPolicy {
        TunnelPolicy {
            id: Some(ObjectId::new()),
            tenant_id,
            name: "any".into(),
            subjects: vec![PolicySubject::AllUsers],
            targets: vec![PolicyTarget::AllAgents],
            allowlist: vec![rule(HostPattern::Exact("db".into()), 5432, 5432)],
            max_concurrent_flows: None,
            max_bytes_per_session: None,
            created_at: DateTime::now(),
            updated_at: DateTime::now(),
            deleted_at: None,
        }
    }

    #[test]
    fn cross_tenant_gate_blocks_even_with_allow_all_policy() {
        // The Sev0 case from plan §"Multi-tenancy gotcha". A
        // tenant-A tunnel client sending TcpForwardRequest with a
        // tenant-B agent_id must be rejected even if a permissive
        // allow-all policy exists somewhere.
        let tenant_a = ObjectId::new();
        let tenant_b = ObjectId::new();
        let agent_in_b = agent_for(tenant_b, AgentStatus::Online, false);
        // A policy in tenant B that would otherwise allow this.
        let p = allow_all_policy_for(tenant_b);

        let result = check_tcp(
            tenant_a, // ← client is in tenant A
            &agent_in_b,
            &[p],
            &subject(ObjectId::new()),
            "db",
            5432,
        );
        match result {
            GateResult::Reject { kind, .. } => assert_eq!(kind, RejectKind::CrossTenant),
            r => panic!("expected CrossTenant reject, got {r:?}"),
        }
    }

    #[test]
    fn cross_tenant_gate_runs_before_acl_eval() {
        // Pass an EMPTY policy list — same-tenant would deny via ACL.
        // Cross-tenant must reject with CrossTenant, not AclDenied —
        // the kind drives the audit log; mixing them up makes
        // forensic queries unreliable.
        let tenant_a = ObjectId::new();
        let tenant_b = ObjectId::new();
        let agent_in_b = agent_for(tenant_b, AgentStatus::Online, false);
        let result = check_tcp(
            tenant_a,
            &agent_in_b,
            &[],
            &subject(ObjectId::new()),
            "db",
            5432,
        );
        match result {
            GateResult::Reject { kind, .. } => assert_eq!(kind, RejectKind::CrossTenant),
            _ => panic!("expected CrossTenant"),
        }
    }

    #[test]
    fn deleted_agent_rejects_with_agent_error() {
        let tenant = ObjectId::new();
        let agent = agent_for(tenant, AgentStatus::Online, true);
        let result = check_tcp(
            tenant,
            &agent,
            &[allow_all_policy_for(tenant)],
            &subject(ObjectId::new()),
            "db",
            5432,
        );
        match result {
            GateResult::Reject { kind, .. } => assert_eq!(kind, RejectKind::AgentError),
            r => panic!("expected AgentError, got {r:?}"),
        }
    }

    #[test]
    fn quarantined_agent_rejects_with_agent_error() {
        let tenant = ObjectId::new();
        let agent = agent_for(tenant, AgentStatus::Quarantined, false);
        let result = check_tcp(
            tenant,
            &agent,
            &[allow_all_policy_for(tenant)],
            &subject(ObjectId::new()),
            "db",
            5432,
        );
        assert!(matches!(
            result,
            GateResult::Reject {
                kind: RejectKind::AgentError,
                ..
            }
        ));
    }

    #[test]
    fn happy_path_allows_with_policy_ceilings_plumbed() {
        let tenant = ObjectId::new();
        let agent = agent_for(tenant, AgentStatus::Online, false);
        let mut p = allow_all_policy_for(tenant);
        p.max_concurrent_flows = Some(16);
        p.max_bytes_per_session = Some(500 * 1024 * 1024);
        let expected_pid = p.id.unwrap();

        let result = check_tcp(tenant, &agent, &[p], &subject(ObjectId::new()), "db", 5432);
        match result {
            GateResult::Allow {
                policy_id,
                max_concurrent_flows,
                max_bytes_per_session,
                ..
            } => {
                assert_eq!(policy_id, expected_pid);
                assert_eq!(max_concurrent_flows, Some(16));
                assert_eq!(max_bytes_per_session, Some(500 * 1024 * 1024));
            }
            r => panic!("expected allow, got {r:?}"),
        }
    }

    #[test]
    fn same_tenant_no_policy_rejects_as_acl_denied_not_cross_tenant() {
        // Distinct from CrossTenant — wire form must be AclDenied for
        // the dashboard's "policy gaps" report to be accurate.
        let tenant = ObjectId::new();
        let agent = agent_for(tenant, AgentStatus::Online, false);
        let result = check_tcp(tenant, &agent, &[], &subject(ObjectId::new()), "db", 5432);
        match result {
            GateResult::Reject { kind, .. } => assert_eq!(kind, RejectKind::AclDenied),
            _ => panic!("expected AclDenied"),
        }
    }

    #[test]
    fn offline_agent_status_still_passes_gate() {
        // Offline doesn't block — the WS handler will surface the
        // unreachable-agent failure separately. Gate's job is auth +
        // policy, not liveness.
        let tenant = ObjectId::new();
        let agent = agent_for(tenant, AgentStatus::Offline, false);
        let result = check_tcp(
            tenant,
            &agent,
            &[allow_all_policy_for(tenant)],
            &subject(ObjectId::new()),
            "db",
            5432,
        );
        assert!(result.is_allow());
    }

    // ─── proto gating (UDP ASSOCIATE) ────────────────────────────────

    #[test]
    fn any_proto_rule_permits_both_tcp_and_udp() {
        let aid = ObjectId::new();
        let p = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule_proto(
                HostPattern::Exact("dns".into()),
                53,
                53,
                ProtocolKind::Any,
            )],
        );
        assert!(
            evaluate(
                std::slice::from_ref(&p),
                &subject(ObjectId::new()),
                aid,
                "dns",
                53,
                ProtocolKind::Tcp
            )
            .is_allow()
        );
        assert!(
            evaluate(
                &[p],
                &subject(ObjectId::new()),
                aid,
                "dns",
                53,
                ProtocolKind::Udp
            )
            .is_allow()
        );
    }

    #[test]
    fn udp_rule_denies_tcp_request_and_vice_versa() {
        let aid = ObjectId::new();
        let udp_only = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule_proto(
                HostPattern::Exact("dns".into()),
                53,
                53,
                ProtocolKind::Udp,
            )],
        );
        assert!(
            evaluate(
                std::slice::from_ref(&udp_only),
                &subject(ObjectId::new()),
                aid,
                "dns",
                53,
                ProtocolKind::Udp
            )
            .is_allow()
        );
        assert!(matches!(
            evaluate(
                &[udp_only],
                &subject(ObjectId::new()),
                aid,
                "dns",
                53,
                ProtocolKind::Tcp
            ),
            Decision::Deny { .. }
        ));

        let tcp_only = policy(
            vec![PolicySubject::AllUsers],
            vec![PolicyTarget::AllAgents],
            vec![rule_proto(
                HostPattern::Exact("db".into()),
                5432,
                5432,
                ProtocolKind::Tcp,
            )],
        );
        assert!(
            evaluate(
                std::slice::from_ref(&tcp_only),
                &subject(ObjectId::new()),
                aid,
                "db",
                5432,
                ProtocolKind::Tcp
            )
            .is_allow()
        );
        assert!(matches!(
            evaluate(
                &[tcp_only],
                &subject(ObjectId::new()),
                aid,
                "db",
                5432,
                ProtocolKind::Udp
            ),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn check_forward_request_gates_udp_proto() {
        let tenant = ObjectId::new();
        let agent = agent_for(tenant, AgentStatus::Online, false);
        let mut p = allow_all_policy_for(tenant);
        p.allowlist = vec![rule_proto(
            HostPattern::Exact("db".into()),
            5432,
            5432,
            ProtocolKind::Udp,
        )];
        assert!(
            check_forward_request(
                tenant,
                &agent,
                std::slice::from_ref(&p),
                &subject(ObjectId::new()),
                "db",
                5432,
                ProtocolKind::Udp
            )
            .is_allow()
        );
        match check_forward_request(
            tenant,
            &agent,
            &[p],
            &subject(ObjectId::new()),
            "db",
            5432,
            ProtocolKind::Tcp,
        ) {
            GateResult::Reject { kind, .. } => assert_eq!(kind, RejectKind::AclDenied),
            r => panic!("expected AclDenied, got {r:?}"),
        }
    }
}
