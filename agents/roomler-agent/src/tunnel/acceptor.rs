//! Agent-side handler for one inbound `ServerMsg::TcpForwardForward`.
//!
//! Splits into two steps so the policy + dial logic stays unit-
//! testable without spinning up a real `AgentTunnelPeer`:
//!
//! 1. [`decide_forward`] applies the agent-local [`AgentForwardAcl`]
//!    then dials dst with a bounded timeout. Returns either the open
//!    `TcpStream` (caller pipes it into the DC) or a typed
//!    `TcpForwardReject` to be shipped over the WS.
//! 2. [`handle_forward_request`] wires the decision into the data
//!    plane: registers the flow on the right `FlowDemux`, replies
//!    `TcpForwardAccept`, spawns `tunnel_core::forward::run_flow`,
//!    and on close cleans the flow + emits `TcpClosed` for audit.

use std::sync::Arc;
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::signaling::{ClientMsg, RejectKind};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::acl::{AclDecision, AgentForwardAcl};
use super::dialer::{DialError, dial_dst};
use super::peer::AgentTunnelPeer;

/// Cap on how long we'll wait for the DC pool to finish opening
/// before bailing on a forward request. The pool usually opens
/// within seconds of `TunnelSdpOffer`; this only fires when a
/// forward arrives suspiciously early or the peer connection
/// stalled.
const POOL_READY_WAIT: Duration = Duration::from_secs(10);

/// Policy + dial layer. Pure decision logic, no peer interaction —
/// keeps the unit tests self-contained.
pub async fn decide_forward(
    session_id: ObjectId,
    flow_id: u32,
    dst_host: &str,
    dst_port: u16,
    acl: &AgentForwardAcl,
    dial_timeout: Duration,
) -> Result<TcpStream, ClientMsg> {
    if let AclDecision::Reject { reason } = acl.check(dst_host, dst_port) {
        info!(
            %session_id, %flow_id, %dst_host, %dst_port, %reason,
            "agent ACL rejected forward"
        );
        return Err(ClientMsg::TcpForwardReject {
            session_id,
            flow_id,
            kind: RejectKind::AclDenied,
            reason: format!("agent: {reason}"),
        });
    }
    match dial_dst(dst_host, dst_port, dial_timeout).await {
        Ok(stream) => {
            debug!(
                %session_id, %flow_id, %dst_host, %dst_port,
                peer = ?stream.peer_addr().ok(),
                "agent dialed dst; preparing flow"
            );
            Ok(stream)
        }
        Err(DialError::Timeout(d)) => {
            warn!(%session_id, %flow_id, %dst_host, %dst_port, ?d, "dial timeout");
            Err(ClientMsg::TcpForwardReject {
                session_id,
                flow_id,
                kind: RejectKind::DialFailed,
                reason: format!("dial timed out after {d:?}"),
            })
        }
        Err(DialError::Io(e)) => {
            warn!(%session_id, %flow_id, %dst_host, %dst_port, %e, "dial failed");
            Err(ClientMsg::TcpForwardReject {
                session_id,
                flow_id,
                kind: RejectKind::DialFailed,
                reason: format!("dial: {e}"),
            })
        }
    }
}

/// End-to-end driver for one `TcpForwardForward`. Decides, dials,
/// wires the flow into the DC pool, and runs the bidirectional pump
/// to completion. Sends `TcpForwardAccept` / `Reject` and the final
/// `TcpClosed` audit message itself via `outbound`.
#[allow(clippy::too_many_arguments)]
pub async fn handle_forward_request(
    session_id: ObjectId,
    flow_id: u32,
    dst_host: &str,
    dst_port: u16,
    acl: &AgentForwardAcl,
    dial_timeout: Duration,
    tunnel_peer: &Arc<AgentTunnelPeer>,
    outbound: mpsc::Sender<ClientMsg>,
) {
    let stream =
        match decide_forward(session_id, flow_id, dst_host, dst_port, acl, dial_timeout).await {
            Ok(s) => s,
            Err(reject) => {
                let _ = outbound.send(reject).await;
                return;
            }
        };

    // Make sure the DC pool is fully open before assigning a channel.
    // Under normal operation the SDP/ICE handshake finishes long
    // before the first flow request, so this resolves immediately.
    if !tunnel_peer.wait_pool_ready(POOL_READY_WAIT).await {
        warn!(%session_id, %flow_id, "DC pool not ready within budget — rejecting");
        let _ = outbound
            .send(ClientMsg::TcpForwardReject {
                session_id,
                flow_id,
                kind: RejectKind::AgentError,
                reason: "DC pool not ready on agent".into(),
            })
            .await;
        return;
    }

    let pool_size = tunnel_peer.pool_size().await;
    if pool_size == 0 {
        let _ = outbound
            .send(ClientMsg::TcpForwardReject {
                session_id,
                flow_id,
                kind: RejectKind::AgentError,
                reason: "empty DC pool on agent".into(),
            })
            .await;
        return;
    }
    let dc_index = (flow_id % pool_size as u32) as u8;
    let Some(demux) = tunnel_peer.demux(dc_index).await else {
        let _ = outbound
            .send(ClientMsg::TcpForwardReject {
                session_id,
                flow_id,
                kind: RejectKind::AgentError,
                reason: format!("demux missing for dc_index={dc_index}"),
            })
            .await;
        return;
    };

    let (from_dc, stats) = demux.register(flow_id).await;
    let dc = demux.dc();

    // Accept the flow + start pumping.
    if let Err(e) = outbound
        .send(ClientMsg::TcpForwardAccept {
            session_id,
            flow_id,
            dc_index,
        })
        .await
    {
        debug!(%session_id, %flow_id, %e, "TcpForwardAccept send failed (channel closed)");
        demux.unregister(flow_id).await;
        return;
    }

    let half_close = tunnel_peer.half_close_sink(outbound.clone());
    let close_reason =
        tunnel_core::forward::run_flow(stream, dc, flow_id, from_dc, half_close, stats).await;
    demux.unregister(flow_id).await;
    info!(%session_id, %flow_id, ?close_reason, "agent flow ended");
    let _ = outbound
        .send(ClientMsg::TcpClosed {
            session_id,
            flow_id,
            reason: close_reason,
        })
        .await;
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
        let reply = decide_forward(
            ObjectId::new(),
            1,
            "127.0.0.1",
            1,
            &acl,
            Duration::from_secs(1),
        )
        .await
        .expect_err("expected reject");
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
        let reply = decide_forward(
            ObjectId::new(),
            1,
            "ssh.intranet",
            22,
            &acl,
            Duration::from_secs(1),
        )
        .await
        .expect_err("expected reject");
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
        let reply = decide_forward(
            ObjectId::new(),
            1,
            "127.0.0.1",
            1,
            &acl,
            Duration::from_secs(1),
        )
        .await
        .expect_err("expected reject");
        match reply {
            ClientMsg::TcpForwardReject { kind, .. } => {
                assert_eq!(kind, RejectKind::DialFailed);
            }
            _ => panic!("expected DialFailed reject, got {reply:?}"),
        }
    }

    #[tokio::test]
    async fn successful_dial_returns_open_tcp_stream() {
        // Bind a one-shot local listener so the acceptor has
        // something to connect to.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let acl = AgentForwardAcl {
            enabled: true,
            allowlist: vec![],
        };
        let stream = decide_forward(
            ObjectId::new(),
            42,
            "127.0.0.1",
            port,
            &acl,
            Duration::from_secs(2),
        )
        .await
        .expect("expected open TcpStream");
        // Just verify the stream is bound to a peer — the post-decide
        // data-plane (`run_flow`) is exercised by `peer::tests`.
        assert!(stream.peer_addr().is_ok());
    }
}
