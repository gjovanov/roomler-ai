//! Receives `ServerMsg::TcpForwardForward` from the WS, runs the
//! agent-side ACL, dials dst, and synthesises a
//! `ClientMsg::TcpForwardAccept` or `TcpForwardReject` for the
//! signaling loop to send back.
//!
//! T2.6 ships the request-side roundtrip (ACL → dial → accept/reject
//! wire reply). The accepted `TcpStream` is dropped immediately —
//! the DC pump wiring lands in T2.7-9 once the WebRTC peer + DC pool
//! exist on the agent side.

use bson::oid::ObjectId;
use roomler_ai_remote_control::signaling::{ClientMsg, RejectKind};
use std::time::Duration;
use tracing::{debug, info, warn};

use super::acl::{AclDecision, AgentForwardAcl};
use super::dialer::{DialError, dial_dst};

/// Handle one inbound `ServerMsg::TcpForwardForward`. Returns the
/// `ClientMsg` the signaling loop should send back (Accept or
/// Reject).
pub async fn handle_forward_request(
    session_id: ObjectId,
    flow_id: u32,
    dst_host: &str,
    dst_port: u16,
    acl: &AgentForwardAcl,
    dial_timeout: Duration,
) -> ClientMsg {
    // 1. Local ACL (belt-and-suspenders — server already gated).
    if let AclDecision::Reject { reason } = acl.check(dst_host, dst_port) {
        info!(
            %session_id, %flow_id, %dst_host, %dst_port, %reason,
            "agent ACL rejected forward"
        );
        return ClientMsg::TcpForwardReject {
            session_id,
            flow_id,
            kind: RejectKind::AclDenied,
            reason: format!("agent: {reason}"),
        };
    }

    // 2. Dial dst with timeout.
    match dial_dst(dst_host, dst_port, dial_timeout).await {
        Ok(stream) => {
            debug!(
                %session_id, %flow_id, %dst_host, %dst_port,
                peer = ?stream.peer_addr().ok(),
                "agent dialed dst; replying Accept"
            );
            // T2.7-9: hand the stream to the DC pump. For now we
            // drop it — the operator's integration test asserts the
            // Accept wire roundtrip; data-plane checks come later.
            drop(stream);
            ClientMsg::TcpForwardAccept {
                session_id,
                flow_id,
                // dc_index hardcoded to 0 in T2.6 — the actual DC
                // pool assignment lands when the pool exists.
                dc_index: 0,
            }
        }
        Err(DialError::Timeout(d)) => {
            warn!(%session_id, %flow_id, %dst_host, %dst_port, ?d, "dial timeout");
            ClientMsg::TcpForwardReject {
                session_id,
                flow_id,
                kind: RejectKind::DialFailed,
                reason: format!("dial timed out after {d:?}"),
            }
        }
        Err(DialError::Io(e)) => {
            warn!(%session_id, %flow_id, %dst_host, %dst_port, %e, "dial failed");
            ClientMsg::TcpForwardReject {
                session_id,
                flow_id,
                kind: RejectKind::DialFailed,
                reason: format!("dial: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_core::policy::{DestinationRule, HostPattern, PortRange};

    #[tokio::test]
    async fn agent_disabled_rejects_with_acl_denied() {
        let acl = AgentForwardAcl {
            enabled: false,
            allowlist: vec![],
        };
        let reply = handle_forward_request(
            ObjectId::new(),
            1,
            "127.0.0.1",
            1,
            &acl,
            Duration::from_secs(1),
        )
        .await;
        match reply {
            ClientMsg::TcpForwardReject { kind, .. } => {
                assert_eq!(kind, RejectKind::AclDenied);
            }
            _ => panic!("expected reject, got {reply:?}"),
        }
    }

    #[tokio::test]
    async fn dst_outside_local_allowlist_rejects() {
        let acl = AgentForwardAcl {
            enabled: true,
            allowlist: vec![DestinationRule {
                host_pattern: HostPattern::Exact("db.intranet".into()),
                port_range: PortRange {
                    low: 5432,
                    high: 5432,
                },
            }],
        };
        let reply = handle_forward_request(
            ObjectId::new(),
            1,
            "ssh.intranet",
            22,
            &acl,
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            reply,
            ClientMsg::TcpForwardReject {
                kind: RejectKind::AclDenied,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn dial_failure_returns_dial_failed_kind() {
        // ACL allows, but port 1 on localhost isn't listening →
        // dialer returns Io error → acceptor maps to DialFailed
        // (not AclDenied — the distinction matters for the
        // dashboard's "policy gap" vs "network broken" report).
        let acl = AgentForwardAcl {
            enabled: true,
            allowlist: vec![],
        };
        let reply = handle_forward_request(
            ObjectId::new(),
            1,
            "127.0.0.1",
            1,
            &acl,
            Duration::from_secs(1),
        )
        .await;
        match reply {
            ClientMsg::TcpForwardReject { kind, .. } => {
                assert_eq!(kind, RejectKind::DialFailed);
            }
            _ => panic!("expected DialFailed reject, got {reply:?}"),
        }
    }

    #[tokio::test]
    async fn successful_dial_returns_accept_with_dc_index_zero() {
        // Spawn a one-shot local TCP listener so the acceptor has
        // something to connect to. Bind to ephemeral port so the
        // test never collides.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        // Accept and immediately drop in the background — we only
        // care that the agent's connect succeeded.
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let acl = AgentForwardAcl {
            enabled: true,
            allowlist: vec![],
        };
        let reply = handle_forward_request(
            ObjectId::new(),
            42,
            "127.0.0.1",
            port,
            &acl,
            Duration::from_secs(2),
        )
        .await;
        match reply {
            ClientMsg::TcpForwardAccept {
                flow_id, dc_index, ..
            } => {
                assert_eq!(flow_id, 42);
                assert_eq!(dc_index, 0, "T2.6 stub — real pool assignment in T2.7");
            }
            _ => panic!("expected accept, got {reply:?}"),
        }
    }
}
