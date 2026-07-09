//! Roomler **LocalAPI** — the local control surface (P1: read-only).
//!
//! The unified daemon (`roomlerd`) will expose this over a local-only channel
//! (named pipe on Windows / unix socket elsewhere; ACL-authenticated — wired in
//! P1-cont) so thin clients — the CLI (`roomler`) and the tray — can read live
//! node / peer / flow state without reaching into the daemon's internals. This
//! module is the **transport-agnostic protocol**: the request/response wire
//! types plus a pure [`handle`] dispatch over a [`LocalApiState`] snapshot. The
//! pipe listener + the daemon's `LocalApiState` impl (gathering real overlay /
//! tunnel / forward state) land in P1-cont; keeping the protocol pure here makes
//! it unit-testable with a mock and reusable by both the daemon and clients.
//!
//! Wire shape: newline-delimited JSON, adjacently tagged (`{"t":<verb>}` /
//! `{"t":<verb>,"d":<payload>}`) so a payload may be a struct OR a sequence.

use serde::{Deserialize, Serialize};

/// How this node currently reaches a peer — the Tailscale-style connection
/// type shown per device in the UI. `Tunnel` is the userspace SOCKS/forward
/// path (used when a corp full-tunnel VPN captures the overlay's routes);
/// `Blocked` = a peer with no working carrier; `Offline` = not currently up.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Direct,
    Relay,
    Tunnel,
    Blocked,
    Offline,
}

/// Which privilege mode the daemon is running in.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    /// SYSTEM service — full node (can *be accessed* + *reach others*).
    Service,
    /// Unprivileged user session — *reach others* only, no admin.
    User,
}

/// Snapshot of the local node.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    pub node_id: String,
    pub name: String,
    pub version: String,
    pub mode: DaemonMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    /// Connected to the coordination server.
    pub connected: bool,
}

/// A peer device as this node currently sees it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub node_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    pub online: bool,
    pub connection: ConnectionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
}

/// Whether a forward is a static `--remote` forward or a SOCKS5 listener.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    Forward,
    Socks5,
}

/// One active forward / SOCKS5 listener with cumulative throughput. Sourced
/// from the per-flow `forward::FlowStats` the data plane already records.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FlowInfo {
    pub id: String,
    pub kind: FlowKind,
    pub local_addr: String,
    /// `host:port` for a static forward; `None` for a SOCKS5 listener (its
    /// target is chosen per connection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Peer node this forward reaches (name or id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub transport: String,
    pub active_flows: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// A LocalAPI request. P1 exposes read-only verbs; create/kill/consent verbs
/// (P2+) extend this enum. Adjacently tagged on `t`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Request {
    /// Local node status.
    Status,
    /// Peers with their current connection type.
    Peers,
    /// Active forwards / SOCKS5 listeners + throughput.
    Flows,
}

/// A LocalAPI response. Adjacently tagged so a payload may be a struct
/// (`Status`) or a sequence (`Peers` / `Flows`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Response {
    Status(NodeStatus),
    Peers(Vec<PeerInfo>),
    Flows(Vec<FlowInfo>),
    /// The verb couldn't be served (bad request, state unavailable).
    Error {
        message: String,
    },
}

/// Read-only snapshot the daemon provides to [`handle`]. The daemon's impl
/// gathers this from its live overlay / tunnel / forward state; the trait keeps
/// the protocol unit-testable with a mock and free of daemon internals.
pub trait LocalApiState {
    fn status(&self) -> NodeStatus;
    fn peers(&self) -> Vec<PeerInfo>;
    fn flows(&self) -> Vec<FlowInfo>;
}

/// Pure dispatch: map a [`Request`] to a [`Response`] over a state snapshot.
/// No I/O — the pipe listener (P1-cont) reads a JSON line, deserialises a
/// [`Request`], calls this, and writes the [`Response`] back.
pub fn handle(req: &Request, state: &dyn LocalApiState) -> Response {
    match req {
        Request::Status => Response::Status(state.status()),
        Request::Peers => Response::Peers(state.peers()),
        Request::Flows => Response::Flows(state.flows()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock;
    impl LocalApiState for Mock {
        fn status(&self) -> NodeStatus {
            NodeStatus {
                node_id: "n1".into(),
                name: "neo16".into(),
                version: "0.3.0-rc.154".into(),
                mode: DaemonMode::Service,
                tenant_id: Some("t1".into()),
                overlay_ip: Some("100.64.0.2".into()),
                connected: true,
            }
        }
        fn peers(&self) -> Vec<PeerInfo> {
            vec![
                PeerInfo {
                    node_id: "n2".into(),
                    name: "pc50045".into(),
                    overlay_ip: Some("100.64.0.1".into()),
                    online: true,
                    connection: ConnectionType::Tunnel,
                    rtt_ms: Some(52),
                    last_seen_ms: Some(1000),
                },
                PeerInfo {
                    node_id: "n3".into(),
                    name: "home".into(),
                    overlay_ip: Some("100.64.0.9".into()),
                    online: true,
                    connection: ConnectionType::Direct,
                    rtt_ms: Some(3),
                    last_seen_ms: Some(1200),
                },
            ]
        }
        fn flows(&self) -> Vec<FlowInfo> {
            vec![FlowInfo {
                id: "f1".into(),
                kind: FlowKind::Socks5,
                local_addr: "127.0.0.1:1080".into(),
                target: None,
                node: Some("pc50045".into()),
                transport: "quic-v1".into(),
                active_flows: 2,
                bytes_in: 4096,
                bytes_out: 8192,
            }]
        }
    }

    #[test]
    fn handle_dispatches_each_verb() {
        let s = Mock;
        match handle(&Request::Status, &s) {
            Response::Status(st) => {
                assert_eq!(st.overlay_ip.as_deref(), Some("100.64.0.2"));
                assert_eq!(st.mode, DaemonMode::Service);
            }
            other => panic!("expected Status, got {other:?}"),
        }
        match handle(&Request::Peers, &s) {
            Response::Peers(p) => {
                assert_eq!(p.len(), 2);
                assert_eq!(p[0].connection, ConnectionType::Tunnel);
                assert_eq!(p[1].connection, ConnectionType::Direct);
            }
            other => panic!("expected Peers, got {other:?}"),
        }
        match handle(&Request::Flows, &s) {
            Response::Flows(f) => {
                assert_eq!(f.len(), 1);
                assert_eq!(f[0].kind, FlowKind::Socks5);
                assert!(f[0].target.is_none());
            }
            other => panic!("expected Flows, got {other:?}"),
        }
    }

    #[test]
    fn request_wire_shape_is_stable() {
        assert_eq!(
            serde_json::to_string(&Request::Status).unwrap(),
            r#"{"t":"status"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"t":"peers"}"#).unwrap(),
            Request::Peers
        );
    }

    #[test]
    fn response_round_trips_struct_and_sequence_payloads() {
        // Adjacently-tagged so a sequence payload (Peers) is legal where an
        // internally-tagged enum would reject it — locks that choice.
        let peers = handle(&Request::Peers, &Mock);
        let s = serde_json::to_string(&peers).unwrap();
        assert!(s.starts_with(r#"{"t":"peers","d":["#), "got {s}");
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), peers);

        let status = handle(&Request::Status, &Mock);
        let s = serde_json::to_string(&status).unwrap();
        assert!(s.contains(r#""t":"status""#));
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), status);

        let err = Response::Error {
            message: "nope".into(),
        };
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"t":"error","d":{"message":"nope"}}"#
        );

        // Connection types serialise snake_case (UI + wire contract).
        assert_eq!(
            serde_json::to_string(&ConnectionType::Tunnel).unwrap(),
            r#""tunnel""#
        );
    }
}
