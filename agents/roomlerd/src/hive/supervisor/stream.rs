// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2d-2 — the replica stream on the device (spec §3b, "The carrier"
//! and "The stream"): the source's endpoint for a grant, the member's dial,
//! the token and the opening exchange, on the `direct` rung (P2d-2a), the
//! `relay` rung (P2d-2b) and the `derp` rung (P2d-2c).
//!
//! | Side | On | Does |
//! |---|---|---|
//! | source | `rc:hive.replica.grant` | registers the grant at once, then stands its endpoint up: on `direct`, one UDP socket, STUN asked on that very socket, a QUIC server on it; on `relay`, a TURN allocation and a QUIC server over it; on `derp`, a QUIC server over its own `/derp` connection, on a route of the stream's own toward the member's key. Answers `rc:hive.replica.ready` with its certificate, and its addresses or its key |
//! | source | `rc:hive.replica.candidate` | the member's relayed address: a TURN permission for it, held until the allocation lands |
//! | source | the member's connection | serves ONE: the token first, then the member's hello, answered from its chain |
//! | member | `rc:hive.replica.dial` | on `relay`, first its own allocation, on the source's TURN worker, and its relayed address sent for the source's permission; on `derp`, a route of the stream's own toward the source's key; then the source dialed, the certificate pinned, the token presented, and where its copy ends said |
//! | either | the server's `rc:hive.replica.close`, the connection's end, or the grant's | the endpoint, its socket and its allocation or route go, and the server is told why |
//!
//! The events, blobs and acks after the opening exchange are P2d-3. This
//! build does not advertise `hive-stream` yet: a source that answers the
//! hello but streams no event would hold a member waiting for nothing, so the
//! verb is advertised once events flow (P2d-3).
//!
//! ⚠️ On `derp`, never the tunnel's route ([`DerpMux::tunnel_conn_for`]):
//! that one is one per peer, last one wins, so a stream on it would take a
//! `quic-derp-v1` tunnel's route between the same two devices, and two
//! streams between one pair would take each other's. Each stream has its own
//! ([`DerpMux::hive_conn_for`], by its grant's id).
//!
//! [`DerpMux::tunnel_conn_for`]: tunnel_core::transport::derp::DerpMux::tunnel_conn_for
//! [`DerpMux::hive_conn_for`]: tunnel_core::transport::derp::DerpMux::hive_conn_for
//!
//! ⚠️ Every endpoint and every connection ends by the grant's `ttl_secs` at
//! the latest, whatever the other side does: each holds a UDP socket (and on
//! `relay` an allocation), and sockets leaked one grant at a time are how a
//! daemon ran out of ports before (`docs/tunnels.md`).
//!
//! ⚠️ A grant is registered BEFORE its endpoint is stood up, so the server's
//! `close` ends a setup still in flight. The server gives up on a source
//! silent for 30 s and ignores a later `ready`, and a TURN allocation was
//! measured at ~20 s on a UDP-hostile network (#1761): an endpoint registered
//! only once built would wait out its whole grant, holding its socket and its
//! allocation, with nobody coming.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{
    HiveStreamPath, HiveStreamRefusal, is_dialable, stream_limits,
};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};
use roomler_ai_remote_control::turn_url;
use roomler_hive_node::stream::{self as wire, MemberMsg, SourceMsg};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};
use tunnel_core::transport::derp::{DerpTunnelHandle, parse_pubkey_hex};
use tunnel_core::transport::quic::{self, QuicConnection, QuicPeer, RecvStream, SendStream};
use tunnel_core::transport::relay::{self, RelayConn, RelayUdpSocket};

use super::Supervisor;

/// How long each address is tried, and how long the source waits for the
/// member's token once it connected.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long STUN waits for an answer, per server.
const STUN_TIMEOUT: Duration = Duration::from_secs(2);
/// How long each side waits for the other's frame in the opening exchange.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest close `reason` this device sends.
const MAX_REASON_CHARS: usize = 64;
/// How long the member waits after its own permissions' datagrams before it
/// dials over its relay, so they are installed on the TURN server first
/// (tunnel-core's `QUIC_PERMIT_SETTLE`).
const PERMIT_SETTLE: Duration = Duration::from_millis(300);
/// The member's addresses a source holds for a grant whose allocation has
/// not landed yet. A member sends one; the bound keeps a replay from growing
/// the entry.
const MAX_HELD_CANDIDATES: usize = 16;
/// The most streams this device follows at once, as a member. Each holds a
/// UDP socket, and on the relay rung an allocation; a dial past it is closed
/// `busy`.
const MAX_FOLLOWING: usize = 32;

/// Which side of a stream this device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Source,
    Member,
}

/// A stream this device is a side of, by grant.
pub(crate) struct Stream {
    pub(super) session_id: ObjectId,
    pub(super) role: Role,
    /// The rung the grant named.
    path: HiveStreamPath,
    /// The source's answer to the hello, once the member heard it.
    pub(super) answer: Option<SourceMsg>,
    /// The source's allocation on the relay rung, once it landed: the
    /// member's permission is installed through it.
    relay: Option<Arc<dyn RelayConn>>,
    /// The member's relayed addresses that arrived before the allocation.
    held: Vec<SocketAddr>,
    task: tokio::task::AbortHandle,
}

/// `rc:hive.replica.grant`, as the source reads it.
pub(crate) struct Grant {
    pub grant_id: ObjectId,
    pub session_id: ObjectId,
    pub fence: u64,
    pub member_id: ObjectId,
    pub path: Option<HiveStreamPath>,
    pub token: String,
    pub ice_servers: Vec<IceServer>,
    /// On `derp`, the member's WireGuard key: whom the stream's route faces.
    pub member_derp_pubkey: Option<String>,
    pub ttl_secs: u32,
}

/// `rc:hive.replica.dial`, as the member reads it.
pub(crate) struct Dial {
    pub grant_id: ObjectId,
    pub session_id: ObjectId,
    pub fence: u64,
    pub source_id: ObjectId,
    pub path: Option<HiveStreamPath>,
    pub token: String,
    pub cert_fingerprint: String,
    pub addrs: Vec<String>,
    pub ice_servers: Vec<IceServer>,
    /// On `derp`, the source's WireGuard key: whom the stream's route faces.
    pub derp_pubkey: Option<String>,
    pub ttl_secs: u32,
}

/// What the source stood up for a grant.
struct Endpoint {
    peer: QuicPeer,
    fingerprint: String,
    addrs: Vec<String>,
    /// On the relay rung, the allocation the endpoint rides.
    relay: Option<Arc<dyn RelayConn>>,
    /// On the derp rung, this device's own key, which the member dials.
    derp_pubkey: Option<String>,
}

/// The STUN servers among `ice`, resolved: an IP literal as it is, a name
/// through the resolver, IPv4 only, as the overlay's STUN is. Best effort: a
/// server that does not resolve is skipped, and host candidates still serve
/// a member on the same network.
async fn stun_servers(ice: &[IceServer]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for url in ice.iter().flat_map(|s| s.urls.iter()) {
        let Some(rest) = url.strip_prefix("stun:") else {
            continue;
        };
        let host_port = rest.split(['?', '#']).next().unwrap_or(rest);
        match tokio::net::lookup_host(host_port).await {
            Ok(addrs) => out.extend(addrs.filter(SocketAddr::is_ipv4).take(1)),
            Err(e) => debug!(%url, %e, "hive stream: a STUN server did not resolve"),
        }
    }
    out
}

/// The TURN server among `ice` the grant minted credentials for, with a
/// UDP url, picked as the tunnel's client picks it (`driver.rs`
/// `pick_turn_creds`): its urls, username and credential.
fn turn_creds(ice: &[IceServer]) -> Option<(Vec<String>, String, String)> {
    ice.iter().find_map(|s| match (&s.username, &s.credential) {
        (Some(u), Some(c)) if relay::turn_udp_server(&s.urls).is_some() => {
            Some((s.urls.clone(), u.clone(), c.clone()))
        }
        _ => None,
    })
}

/// The source's endpoint on the `direct` rung: one UDP socket, STUN asked
/// on it first so the reflexive address is the mapping QUIC then uses, and
/// a QUIC server on that same socket.
async fn direct_endpoint(ice: &[IceServer]) -> anyhow::Result<Endpoint> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    let stun = stun_servers(ice).await;
    let mut addrs = quic::gather_candidates(&socket, &stun, STUN_TIMEOUT).await;
    if addrs.is_empty() {
        // No route found: loopback still serves a member on this host, as a
        // test's is. The bound `0.0.0.0` is no address to dial.
        let port = socket.local_addr()?.port();
        addrs.push(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    let (peer, fingerprint) = QuicPeer::server_from_socket(socket.into_std()?)?;
    let addrs = addrs
        .into_iter()
        .take(stream_limits::MAX_ADDRS)
        .map(|a| a.to_string())
        .collect();
    Ok(Endpoint {
        peer,
        fingerprint,
        addrs,
        relay: None,
        derp_pubkey: None,
    })
}

/// The source's endpoint on the `derp` rung: a QUIC server over this
/// device's own `/derp` connection, on the stream's own route toward the
/// member's key (by the grant's id), the MTU clamped under the relay's frame
/// cap. The member dials this device's key, so `ready` names it and no
/// address.
fn derp_endpoint(handle: &DerpTunnelHandle, g: &Grant) -> anyhow::Result<Endpoint> {
    let member = g
        .member_derp_pubkey
        .as_deref()
        .and_then(parse_pubkey_hex)
        .context("the grant names no member key")?;
    let conn = handle.mux.hive_conn_for(member, g.grant_id.bytes());
    let sock = Arc::new(RelayUdpSocket::new(Arc::new(conn))?);
    let (peer, fingerprint) = QuicPeer::server_over_derp(sock)?;
    Ok(Endpoint {
        peer,
        fingerprint,
        addrs: Vec::new(),
        relay: None,
        derp_pubkey: Some(handle.self_pubkey_hex.clone()),
    })
}

/// The source's endpoint on the `relay` rung: a TURN allocation from the
/// grant's credentials, a QUIC server over it, and the relayed address to
/// dial, as the tunnel's exit stands its own up (`tunnel/quic_peer.rs`
/// `setup_over_relay`).
async fn relay_endpoint(ice: &[IceServer]) -> anyhow::Result<Endpoint> {
    let (urls, user, cred) = turn_creds(ice).context("the grant carries no TURN server")?;
    let turn = relay::allocate_relay_from_ice(&urls, &user, &cred).await?;
    let relay: Arc<dyn RelayConn> = Arc::new(turn);
    let relayed = relay.local_addr()?;
    let sock = Arc::new(RelayUdpSocket::new(Arc::clone(&relay))?);
    let (peer, fingerprint) = QuicPeer::server_over_abstract_socket(sock)?;
    Ok(Endpoint {
        peer,
        fingerprint,
        addrs: vec![relayed.to_string()],
        relay: Some(relay),
        derp_pubkey: None,
    })
}

/// A TURN permission for `addr` on `relay`, installed by one datagram to
/// it: the TURN client creates a permission on its first send, and the
/// other side's QUIC endpoint drops a one-byte datagram as no packet.
async fn permit(relay: &Arc<dyn RelayConn>, addr: SocketAddr) {
    if let Err(e) = relay.send_to(b"\x00", addr).await {
        debug!(%addr, %e, "hive stream: a permission's datagram did not go");
    }
}

/// `rc:hive.replica.ready`, refused in `word`.
fn refusal(grant_id: ObjectId, word: HiveStreamRefusal, why: String) -> ClientMsg {
    info!(grant = %grant_id, refused = word.as_str(), %why, "hive stream: a grant not served");
    ClientMsg::HiveReplicaReady {
        grant_id,
        refused: Some(word),
        detail: Some(why),
        cert_fingerprint: None,
        addrs: Vec::new(),
        derp_pubkey: None,
    }
}

/// `msg` sent on `tx` from a task of its own: the signalling loop never
/// waits on its own outbound queue.
fn answer(tx: mpsc::Sender<ClientMsg>, msg: ClientMsg) {
    tokio::spawn(async move {
        let _ = tx.send(msg).await;
    });
}

async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> anyhow::Result<()> {
    send.write_all(&wire::encode(msg)).await?;
    Ok(())
}

async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> anyhow::Result<T> {
    let mut header = [0u8; 4];
    recv.read_exact(&mut header).await?;
    let len = wire::frame_len(header)?;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(wire::decode(&body)?)
}

/// The first address that connects, each tried for [`CONNECT_TIMEOUT`].
async fn connect_first(peer: &QuicPeer, addrs: &[SocketAddr]) -> Option<QuicConnection> {
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, peer.connect(*addr)).await {
            Ok(Ok(conn)) => return Some(conn),
            Ok(Err(e)) => debug!(%addr, %e, "hive stream: an address did not connect"),
            Err(_) => debug!(%addr, "hive stream: an address did not answer in time"),
        }
    }
    None
}

impl Supervisor {
    /// `rc:hive.replica.grant`: what needs only this device's word is
    /// answered at once; otherwise the grant is registered, and its task
    /// spawned, under one lock, so a server `close` finds it from the first
    /// moment, its endpoint still being stood up or not. The task stands the
    /// endpoint up, answers `ready` on `tx`, and serves.
    pub(super) fn take_grant(
        self: &Arc<Self>,
        g: Grant,
        primary: bool,
        tx: mpsc::Sender<ClientMsg>,
    ) {
        let grant_id = g.grant_id;
        if !primary {
            return answer(
                tx,
                refusal(
                    grant_id,
                    HiveStreamRefusal::Other,
                    "this device serves streams for its primary organization only".into(),
                ),
            );
        }
        let path = match g.path {
            Some(p) => p,
            None => {
                return answer(
                    tx,
                    refusal(
                        grant_id,
                        HiveStreamRefusal::PathUnavailable,
                        "a rung this build cannot name".into(),
                    ),
                );
            }
        };
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        if streams.contains_key(&grant_id) {
            // A replay: the stream this grant started answers it.
            return debug!(grant = %grant_id, "hive stream: a grant this device holds already");
        }
        // Counted with the endpoints still being stood up: each holds its
        // socket already.
        if streams.values().filter(|s| s.role == Role::Source).count()
            >= stream_limits::MAX_PER_SOURCE
        {
            drop(streams);
            return answer(
                tx,
                refusal(
                    grant_id,
                    HiveStreamRefusal::Busy,
                    format!(
                        "this device serves {} streams already",
                        stream_limits::MAX_PER_SOURCE
                    ),
                ),
            );
        }
        let session_id = g.session_id;
        let task = tokio::spawn(Arc::clone(self).run_grant(g, tx));
        streams.insert(
            grant_id,
            Stream {
                session_id,
                role: Role::Source,
                path,
                answer: None,
                relay: None,
                held: Vec::new(),
                task: task.abort_handle(),
            },
        );
    }

    /// One registered grant's life, by its `ttl_secs` at most: the endpoint
    /// stood up, `ready` sent, ONE connection served.
    async fn run_grant(self: Arc<Self>, g: Grant, tx: mpsc::Sender<ClientMsg>) {
        let deadline = Instant::now() + Duration::from_secs(u64::from(g.ttl_secs));
        let ep = match tokio::time::timeout_at(deadline, self.stand_up(&g)).await {
            Ok(Ok(ep)) => ep,
            Ok(Err((word, why))) => {
                // Forgotten before the server hears, so the slot is free by
                // then; the ready says why, and the server forgets the
                // grant itself, so no close follows.
                self.forget_stream(g.grant_id);
                let _ = tx.send(refusal(g.grant_id, word, why)).await;
                return;
            }
            Err(_) => return self.stream_ended(g.grant_id, "expired"),
        };
        // The allocation kept in the entry and the addresses held for it
        // permitted BEFORE the ready: the member dials only once told to,
        // and its opening Initials need the permission.
        let Some(held) = self.endpoint_up(g.grant_id, ep.relay.as_ref()) else {
            // Closed meanwhile: nobody to answer.
            return;
        };
        if let Some(relay) = &ep.relay {
            for addr in held {
                permit(relay, addr).await;
            }
        }
        info!(
            grant = %g.grant_id, session = %g.session_id, member = %g.member_id,
            path = ?g.path, addrs = ?ep.addrs, "hive stream: serving"
        );
        let ready = ClientMsg::HiveReplicaReady {
            grant_id: g.grant_id,
            refused: None,
            detail: None,
            cert_fingerprint: Some(ep.fingerprint),
            addrs: ep.addrs,
            derp_pubkey: ep.derp_pubkey,
        };
        if tx.send(ready).await.is_err() {
            return self.stream_ended(g.grant_id, "disconnected");
        }
        drop(tx);
        let reason = self.serve_one(ep.peer, &g, deadline).await;
        self.stream_ended(g.grant_id, &reason);
    }

    /// A registered grant's endpoint: the store asked whether this device
    /// holds the session, then the grant's rung stood up. A refusal in the
    /// word `ready` carries.
    async fn stand_up(&self, g: &Grant) -> Result<Endpoint, (HiveStreamRefusal, String)> {
        let Ok(store) = &self.store else {
            return Err((
                HiveStreamRefusal::NotHeld,
                "the replica store is unavailable".into(),
            ));
        };
        match store.chain_tip(&g.session_id.to_hex()).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err((
                    HiveStreamRefusal::NotHeld,
                    "this device holds nothing of the session".into(),
                ));
            }
            Err(e) => {
                return Err((
                    HiveStreamRefusal::NotHeld,
                    format!("the replica store could not be read: {e}"),
                ));
            }
        }
        match g.path {
            // The member climbs down a rung on `path_unavailable`.
            Some(HiveStreamPath::Relay) => relay_endpoint(&g.ice_servers).await.map_err(|e| {
                (
                    HiveStreamRefusal::PathUnavailable,
                    format!("no relay: {e:#}"),
                )
            }),
            Some(HiveStreamPath::Derp) => {
                let Some(handle) = self.derp_handle() else {
                    return Err((
                        HiveStreamRefusal::PathUnavailable,
                        "this device has no live DERP connection".into(),
                    ));
                };
                derp_endpoint(&handle, g).map_err(|e| {
                    (
                        HiveStreamRefusal::PathUnavailable,
                        format!("no DERP endpoint: {e:#}"),
                    )
                })
            }
            _ => direct_endpoint(&g.ice_servers).await.map_err(|e| {
                (
                    HiveStreamRefusal::Other,
                    format!("the endpoint could not be bound: {e}"),
                )
            }),
        }
    }

    /// This device's DERP mux, the primary org's: the one Hive streams ride,
    /// as every Hive frame is the primary org's. `None` without the overlay
    /// features, or while the overlay is down.
    fn derp_handle(&self) -> Option<DerpTunnelHandle> {
        #[cfg(test)]
        if let Some(forced) = self
            .derp_for_tests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return forced;
        }
        crate::tunnel::netwatch::primary_derp_tunnel_handle()
    }

    /// The endpoint is up: on the relay rung its allocation kept for the
    /// member's permission, and the addresses held until now returned.
    /// `None` when the grant was closed meanwhile.
    fn endpoint_up(
        &self,
        grant_id: ObjectId,
        relay: Option<&Arc<dyn RelayConn>>,
    ) -> Option<Vec<SocketAddr>> {
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let s = streams.get_mut(&grant_id)?;
        s.relay = relay.cloned();
        Some(std::mem::take(&mut s.held))
    }

    /// `rc:hive.replica.candidate`: the member's relayed address on the
    /// relay rung, for this source's TURN permission. Without one the TURN
    /// server drops the member's opening Initials, since the source, the
    /// QUIC server, never sends first. Permitted at once when the
    /// allocation is up, held until it lands otherwise.
    pub(super) fn take_candidate(&self, grant_id: ObjectId, addr: &str, primary: bool) {
        if !primary {
            return debug!(grant = %grant_id, "hive stream: a candidate on a secondary org's connection — ignored");
        }
        let Some(addr) = addr.parse::<SocketAddr>().ok().filter(is_dialable) else {
            return debug!(grant = %grant_id, %addr, "hive stream: a candidate no member could hold — dropped");
        };
        let relay = {
            let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
            let Some(s) = streams
                .get_mut(&grant_id)
                .filter(|s| s.role == Role::Source && s.path == HiveStreamPath::Relay)
            else {
                return debug!(grant = %grant_id, "hive stream: a candidate for no relay stream this device serves");
            };
            match &s.relay {
                Some(relay) => Arc::clone(relay),
                None if s.held.len() < MAX_HELD_CANDIDATES => return s.held.push(addr),
                None => {
                    return debug!(grant = %grant_id, "hive stream: candidates past the limit — dropped");
                }
            }
        };
        tokio::spawn(async move { permit(&relay, addr).await });
    }

    /// One connection, until `deadline` at most: the member's token, then
    /// its hello answered from this device's chain. The reason it ended, as
    /// the close says it.
    async fn serve_one(&self, peer: QuicPeer, g: &Grant, deadline: Instant) -> String {
        let served = tokio::time::timeout_at(deadline, async {
            let conn = match peer.accept().await {
                Some(Ok(conn)) => conn,
                Some(Err(e)) => return format!("accept failed: {e}"),
                None => return "endpoint_closed".to_string(),
            };
            // The server is out of the byte path: this token is what keeps
            // the endpoint from serving whoever reaches its address.
            match tokio::time::timeout(CONNECT_TIMEOUT, quic::server_authenticate(&conn, &g.token))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => {
                    conn.close(1u32.into(), b"auth failed");
                    return "auth_failed".to_string();
                }
            }
            let (mut send, mut recv) =
                match tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi()).await {
                    Ok(Ok(halves)) => halves,
                    _ => return "no_hello".to_string(),
                };
            let hello: MemberMsg =
                match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv)).await {
                    Ok(Ok(h)) => h,
                    Ok(Err(e)) if e.is::<wire::FrameError>() => return "bad_frame".to_string(),
                    _ => return "no_hello".to_string(),
                };
            // The token proved the grant; the hello must be for it.
            let MemberMsg::Hello { session, fence, .. } = &hello;
            let answer = if *session != g.session_id.to_hex() {
                SourceMsg::Refused {
                    reason: "wrong_session".into(),
                }
            } else if *fence != g.fence {
                SourceMsg::Refused {
                    reason: "wrong_fence".into(),
                }
            } else {
                match &self.store {
                    Ok(store) => match store.answer_hello(hello).await {
                        Ok(a) => a,
                        Err(e) => SourceMsg::Refused {
                            reason: format!("the store could not answer: {e}"),
                        },
                    },
                    Err(_) => SourceMsg::Refused {
                        reason: "not_held".into(),
                    },
                }
            };
            let refused = matches!(answer, SourceMsg::Refused { .. });
            if write_frame(&mut send, &answer).await.is_err() {
                return "member_gone".to_string();
            }
            if refused {
                // Held until the member lets go, HELLO_TIMEOUT at most: the
                // last to receive is the one to close (quinn's
                // `Connection::close`). Closed under its data, the
                // connection would lose the refusal, and the member would
                // hear no answer where there is one; an acknowledgement is
                // not enough, since a receiver may drop what it acknowledged
                // but had not yet read once the close arrives.
                let _ = send.finish();
                let _ = tokio::time::timeout(HELLO_TIMEOUT, conn.closed()).await;
                return "refused".to_string();
            }
            // P2d-3 streams the events from here. Until then the connection
            // is held until the member closes it or the grant ends.
            conn.closed().await;
            "closed".to_string()
        })
        .await;
        served.unwrap_or_else(|_| "expired".to_string())
    }

    /// `rc:hive.replica.dial`: dial the source and say where this copy
    /// ends. Not this device's to follow: closed at once, in words.
    pub(super) fn follow_dial(self: &Arc<Self>, d: Dial, primary: bool) {
        if !primary {
            return self.report_close(d.grant_id, "secondary_org");
        }
        let path = match d.path {
            Some(p) => p,
            None => return self.report_close(d.grant_id, "path_unavailable"),
        };
        let grant_id = d.grant_id;
        let session_id = d.session_id;
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        if streams.contains_key(&grant_id) {
            // A replay: the stream this dial started follows it.
            return debug!(grant = %grant_id, "hive stream: a dial this device follows already");
        }
        if streams.values().filter(|s| s.role == Role::Member).count() >= MAX_FOLLOWING {
            drop(streams);
            return self.report_close(grant_id, "busy");
        }
        let sup = Arc::clone(self);
        let task = tokio::spawn(async move {
            let reason = sup.follow_one(&d).await;
            sup.stream_ended(d.grant_id, &reason);
        });
        streams.insert(
            grant_id,
            Stream {
                session_id,
                role: Role::Member,
                path,
                answer: None,
                relay: None,
                held: Vec::new(),
                task: task.abort_handle(),
            },
        );
    }

    /// The member's side of one stream, by the grant's life at most.
    async fn follow_one(&self, d: &Dial) -> String {
        let Ok(store) = &self.store else {
            return "store_unavailable".to_string();
        };
        let sid = d.session_id.to_hex();
        // A member of it, by this device's own record of what it joined.
        match store.memberships().await {
            Ok(m) if m.iter().any(|m| m.session == sid) => {}
            Ok(_) => return "not_a_member".to_string(),
            Err(e) => {
                warn!(grant = %d.grant_id, %e, "hive stream: the memberships could not be read");
                return "store_unavailable".to_string();
            }
        }
        let addrs: Vec<SocketAddr> = d
            .addrs
            .iter()
            .take(stream_limits::MAX_ADDRS)
            .filter_map(|a| a.parse().ok())
            .filter(is_dialable)
            .collect();
        // On `derp` the source is dialed by its key, never an address.
        if addrs.is_empty() && d.path != Some(HiveStreamPath::Derp) {
            return "no_address".to_string();
        }
        let deadline = Instant::now() + Duration::from_secs(u64::from(d.ttl_secs));
        let followed = tokio::time::timeout_at(deadline, async {
            let (peer, targets) = match d.path {
                Some(HiveStreamPath::Relay) => match self.member_relay(d, &addrs).await {
                    Ok(peer) => (peer, addrs.clone()),
                    Err(why) => return why,
                },
                Some(HiveStreamPath::Derp) => match self.member_derp(d) {
                    Ok((peer, at)) => (peer, vec![at]),
                    Err(why) => return why,
                },
                _ => match QuicPeer::client(
                    "0.0.0.0:0".parse().expect("a bind address"),
                    &d.cert_fingerprint,
                ) {
                    Ok(peer) => (peer, addrs.clone()),
                    Err(e) => return format!("the client could not be bound: {e}"),
                },
            };
            let Some(conn) = connect_first(&peer, &targets).await else {
                return "unreachable".to_string();
            };
            if quic::client_authenticate(&conn, &d.token).await.is_err() {
                return "auth_failed".to_string();
            }
            let (mut send, mut recv) = match conn.open_bi().await {
                Ok(halves) => halves,
                Err(e) => return format!("no stream: {e}"),
            };
            let hello = match store.member_hello(&sid, d.fence).await {
                Ok(h) => h,
                Err(e) => return format!("no hello: {e}"),
            };
            if write_frame(&mut send, &hello).await.is_err() {
                return "source_gone".to_string();
            }
            let answer: SourceMsg =
                match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv)).await {
                    Ok(Ok(a)) => a,
                    Ok(Err(e)) if e.is::<wire::FrameError>() => return "bad_frame".to_string(),
                    _ => return "no_answer".to_string(),
                };
            info!(grant = %d.grant_id, session = %d.session_id, source = %d.source_id, answer = ?answer, "hive stream: the source answered");
            let refused = matches!(answer, SourceMsg::Refused { .. });
            if let Some(s) = self
                .streams
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&d.grant_id)
            {
                s.answer = Some(answer);
            }
            if refused {
                return "refused".to_string();
            }
            // P2d-3 receives the events from here; until then the
            // connection is held until the source closes it or the grant
            // ends.
            conn.closed().await;
            "closed".to_string()
        })
        .await;
        followed.unwrap_or_else(|_| "expired".to_string())
    }

    /// The member's client on the derp rung: a QUIC client over this device's
    /// own `/derp` connection, on the stream's own route toward the source's
    /// key (by the grant's id), and the address that route answers to. The
    /// reason, in the close's words, when it cannot.
    fn member_derp(&self, d: &Dial) -> Result<(QuicPeer, SocketAddr), String> {
        let Some(handle) = self.derp_handle() else {
            return Err("no_derp".to_string());
        };
        let Some(source) = d.derp_pubkey.as_deref().and_then(parse_pubkey_hex) else {
            return Err("no_derp_key".to_string());
        };
        let conn = handle.mux.hive_conn_for(source, d.grant_id.bytes());
        let at = conn.synth_peer();
        let sock =
            RelayUdpSocket::new(Arc::new(conn)).map_err(|e| format!("no socket over DERP: {e}"))?;
        let peer = QuicPeer::client_over_derp(Arc::new(sock), &d.cert_fingerprint)
            .map_err(|e| format!("the client could not be built: {e}"))?;
        Ok((peer, at))
    }

    /// The member's client on the relay rung: its own allocation, on the
    /// source's TURN worker first, a QUIC client over it, its relayed
    /// address sent through the server for the source's permission, and its
    /// own permission for each of the source's addresses. The reason, in the
    /// close's words, when it cannot.
    async fn member_relay(&self, d: &Dial, source: &[SocketAddr]) -> Result<QuicPeer, String> {
        let Some((granted, user, cred)) = turn_creds(&d.ice_servers) else {
            return Err("no_turn".to_string());
        };
        // The source's relayed address names its TURN worker: two
        // allocations on one worker relay to each other, across workers
        // they may not (`driver.rs` `setup_quic_over_relay`). The granted
        // urls follow, should the pinned one not answer.
        let mut urls = Vec::with_capacity(granted.len() + 1);
        if let Some(ip) = source.first().map(SocketAddr::ip) {
            let host = match ip {
                IpAddr::V6(v6) => format!("[{v6}]"),
                IpAddr::V4(v4) => v4.to_string(),
            };
            let port = turn_url::first_udp_port(granted.iter().map(String::as_str));
            urls.push(format!("turn:{host}:{port}?transport=udp"));
        }
        urls.extend(granted);
        let turn = relay::allocate_relay_from_ice(&urls, &user, &cred)
            .await
            .map_err(|e| {
                debug!(grant = %d.grant_id, error = %format!("{e:#}"), "hive stream: no allocation");
                "allocate_failed".to_string()
            })?;
        let relay: Arc<dyn RelayConn> = Arc::new(turn);
        let ours = relay
            .local_addr()
            .map_err(|e| format!("no relayed address: {e}"))?;
        let sock = RelayUdpSocket::new(Arc::clone(&relay))
            .map_err(|e| format!("no socket over the relay: {e}"))?;
        let peer = QuicPeer::client_over_abstract_socket(Arc::new(sock), &d.cert_fingerprint)
            .map_err(|e| format!("the client could not be built: {e}"))?;
        // Awaited, not dropped on a full queue: without it the source never
        // permits this allocation, and nothing ever connects.
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let candidate = ClientMsg::HiveReplicaCandidate {
            grant_id: d.grant_id,
            addr: ours.to_string(),
        };
        match tx {
            Some(tx) if tx.send(candidate).await.is_ok() => {}
            _ => return Err("disconnected".to_string()),
        }
        // This allocation's own permission for each of the source's
        // addresses, before the first Initial, as the tunnel's client sends
        // it. The TURN client would install it on that first send anyway,
        // so this only spares the first Initial a race.
        for addr in source {
            permit(&relay, *addr).await;
        }
        tokio::time::sleep(PERMIT_SETTLE).await;
        Ok(peer)
    }

    /// A stream's task ended on its own: forget it, and tell the server why.
    fn stream_ended(&self, grant_id: ObjectId, reason: &str) {
        let gone = self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&grant_id);
        if let Some(s) = gone {
            info!(
                grant = %grant_id, session = %s.session_id, role = ?s.role, %reason,
                answer = ?s.answer, "hive stream: ended"
            );
            self.report_close(grant_id, reason);
        }
    }

    /// A stream refused before it began: forgotten without a close, since
    /// the `ready` that refused it ends it on the server.
    fn forget_stream(&self, grant_id: ObjectId) {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&grant_id);
    }

    fn report_close(&self, grant_id: ObjectId, reason: &str) {
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx {
            let reason: String = reason.chars().take(MAX_REASON_CHARS).collect();
            let _ = tx.try_send(ClientMsg::HiveReplicaClose { grant_id, reason });
        }
    }

    /// The server's `rc:hive.replica.close`: the stream ends here at once,
    /// its endpoint still being stood up or not, and nothing is said back
    /// (the server already forgot the grant).
    pub(super) fn close_stream(&self, grant_id: ObjectId, reason: &str) {
        let gone = self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&grant_id);
        if let Some(s) = gone {
            s.task.abort();
            info!(grant = %grant_id, session = %s.session_id, role = ?s.role, %reason, "hive stream: closed by the server");
        }
    }

    /// The source's answer to this member's hello on `grant_id`, once heard.
    #[cfg(all(test, unix))]
    pub(super) fn stream_answer(&self, grant_id: ObjectId) -> Option<SourceMsg> {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&grant_id)
            .and_then(|s| s.answer.clone())
    }

    /// Whether this device is a side of `grant_id` now.
    #[cfg(all(test, unix))]
    pub(super) fn holds_stream(&self, grant_id: ObjectId) -> bool {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&grant_id)
    }

    /// The streams this device serves now, as a source, its endpoints still
    /// being stood up among them.
    #[cfg(all(test, unix))]
    pub(super) fn serving(&self) -> usize {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|s| s.role == Role::Source)
            .count()
    }

    /// The DERP mux this device's streams ride in a test: `Some` a test's
    /// own, `None` none at all.
    #[cfg(all(test, unix))]
    pub(super) fn use_derp_for_tests(&self, handle: Option<DerpTunnelHandle>) {
        *self
            .derp_for_tests
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// The member's addresses held for `grant_id`'s allocation.
    #[cfg(all(test, unix))]
    pub(super) fn held_candidates(&self, grant_id: ObjectId) -> usize {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&grant_id)
            .map_or(0, |s| s.held.len())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::{Rig, rig};
    use super::*;
    use roomler_hive_node::TranscriptEvent;
    use roomler_hive_node::stream::Mark;
    use std::net::Ipv4Addr;
    use webrtc::turn::auth::{AuthHandler, generate_auth_key};
    use webrtc::turn::relay::relay_static::RelayAddressGeneratorStatic;
    use webrtc::turn::server::Server;
    use webrtc::turn::server::config::{ConnConfig, ServerConfig};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const TURN_USER: &str = "hive-tester";
    const TURN_PASS: &str = "turn-secret";
    const TURN_REALM: &str = "roomler.test";

    /// Long-term-credential auth for the one test user.
    struct StaticAuth;
    impl AuthHandler for StaticAuth {
        fn auth_handle(
            &self,
            _username: &str,
            realm: &str,
            _src: SocketAddr,
        ) -> Result<Vec<u8>, webrtc::turn::Error> {
            Ok(generate_auth_key(TURN_USER, realm, TURN_PASS))
        }
    }

    /// An in-process TURN server on loopback, its relayed addresses on
    /// loopback too, and the ICE servers a relay grant would carry for it
    /// (as tunnel-core's `quinn_runs_over_two_turn_allocations` stands it
    /// up). Keep the server for the test's life.
    async fn loopback_turn() -> (Server, Vec<IceServer>) {
        let conn = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = conn.local_addr().unwrap();
        let server = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn,
                relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                    relay_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    address: "127.0.0.1".to_owned(),
                    net: Arc::new(webrtc::util::vnet::net::Net::new(None)),
                }),
            }],
            realm: TURN_REALM.to_owned(),
            auth_handler: Arc::new(StaticAuth),
            channel_bind_timeout: Duration::from_secs(0),
            alloc_close_notify: None,
        })
        .await
        .expect("an in-process TURN server");
        let ice = vec![IceServer {
            urls: vec![format!("turn:{addr}?transport=udp")],
            username: Some(TURN_USER.into()),
            credential: Some(TURN_PASS.into()),
        }];
        (server, ice)
    }

    /// A grant for `session_id` on `path`, as the server would send it.
    fn grant(session_id: ObjectId, path: HiveStreamPath) -> Grant {
        Grant {
            grant_id: ObjectId::new(),
            session_id,
            fence: 1,
            member_id: ObjectId::new(),
            path: Some(path),
            token: TOKEN.into(),
            ice_servers: Vec::new(),
            member_derp_pubkey: None,
            ttl_secs: 30,
        }
    }

    /// Append notes to `sid` in `r`'s store, at fence 1.
    fn note(r: &Rig, sid: ObjectId, texts: &[&str]) {
        for text in texts {
            r.store.append(
                &sid.to_hex(),
                1,
                TranscriptEvent::Note {
                    text: (*text).into(),
                },
            );
        }
    }

    /// The `ready` a grant is answered with, as the server hears it.
    async fn ready_of(src: &Rig, g: Grant, primary: bool) -> ClientMsg {
        let (tx, mut rx) = mpsc::channel(4);
        src.sup.take_grant(g, primary, tx);
        tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("a ready within 15 s")
            .expect("a ready")
    }

    /// The word a `ready` refuses with, or `None` for one that serves; a
    /// refusal never names an endpoint.
    fn refusal_of(ready: ClientMsg) -> Option<HiveStreamRefusal> {
        let ClientMsg::HiveReplicaReady {
            refused,
            cert_fingerprint,
            ..
        } = ready
        else {
            panic!("not a ready: {ready:?}");
        };
        assert!(
            refused.is_none() || cert_fingerprint.is_none(),
            "a refusal names no endpoint"
        );
        refused
    }

    /// The member told to dial what `ready` names, as the server would tell
    /// it, with `ice` the grant carried.
    fn dial(
        mem: &Rig,
        g: (ObjectId, ObjectId, Option<HiveStreamPath>),
        ready: ClientMsg,
        token: &str,
        ice: Vec<IceServer>,
    ) {
        let (grant_id, session_id, path) = g;
        let ClientMsg::HiveReplicaReady {
            refused: None,
            cert_fingerprint: Some(fp),
            addrs,
            derp_pubkey,
            ..
        } = ready
        else {
            panic!("not ready: {ready:?}");
        };
        if path == Some(HiveStreamPath::Derp) {
            assert!(addrs.is_empty(), "no address on derp");
            assert!(derp_pubkey.is_some(), "the source's key to dial");
        } else {
            assert!(!addrs.is_empty(), "somewhere to dial");
        }
        mem.sup.follow_dial(
            Dial {
                grant_id,
                session_id,
                fence: 1,
                source_id: ObjectId::new(),
                path,
                token: token.into(),
                cert_fingerprint: fp,
                addrs,
                ice_servers: ice,
                derp_pubkey,
                ttl_secs: 30,
            },
            true,
        );
    }

    /// The source serves `g`, and the member is told to dial it with
    /// `token`.
    async fn served_on(src: &Rig, mem: &Rig, g: Grant, token: &str) -> ObjectId {
        let ids = (g.grant_id, g.session_id, g.path);
        let ice = g.ice_servers.clone();
        let ready = ready_of(src, g, true).await;
        dial(mem, ids, ready, token, ice);
        ids.0
    }

    /// [`served_on`] a direct grant for `sid`.
    async fn served(src: &Rig, mem: &Rig, sid: ObjectId, token: &str) -> ObjectId {
        served_on(src, mem, grant(sid, HiveStreamPath::Direct), token).await
    }

    async fn answer_of(mem: &Rig, grant_id: ObjectId) -> SourceMsg {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(a) = mem.sup.stream_answer(grant_id) {
                return a;
            }
            assert!(Instant::now() < deadline, "the member heard no answer");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The reason of the next `rc:hive.replica.close` the rig's device
    /// sends the server for `grant_id`, or `None` when none comes within
    /// 15 s, so a test compares it in its own words.
    async fn close_of(r: &mut Rig, grant_id: ObjectId) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let msg = tokio::time::timeout_at(deadline, r.reports.recv())
                .await
                .ok()?
                .expect("the reporter is open");
            if let ClientMsg::HiveReplicaClose {
                grant_id: g,
                reason,
            } = msg
                && g == grant_id
            {
                return Some(reason);
            }
        }
    }

    /// The relayed address the rig's device, a member, sends the server for
    /// `grant_id`'s source to permit, or `None` within 15 s.
    async fn candidate_of(r: &mut Rig, grant_id: ObjectId) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let msg = tokio::time::timeout_at(deadline, r.reports.recv())
                .await
                .ok()?
                .expect("the reporter is open");
            if let ClientMsg::HiveReplicaCandidate { grant_id: g, addr } = msg
                && g == grant_id
            {
                return Some(addr);
            }
        }
    }

    /// What `follow_one` does up to its hello, by hand, so a test can send
    /// what a member of this build never would: the endpoint `ready` names
    /// dialed, the token presented, one stream open. The client is returned
    /// with the rest, since its endpoint ends with it.
    async fn dial_by_hand(ready: ClientMsg) -> (QuicPeer, QuicConnection, SendStream, RecvStream) {
        let ClientMsg::HiveReplicaReady {
            refused: None,
            cert_fingerprint: Some(fp),
            addrs,
            ..
        } = ready
        else {
            panic!("not ready: {ready:?}");
        };
        let peer = QuicPeer::client("0.0.0.0:0".parse().unwrap(), &fp).unwrap();
        let addrs: Vec<SocketAddr> = addrs.iter().map(|a| a.parse().unwrap()).collect();
        let conn = connect_first(&peer, &addrs)
            .await
            .expect("the endpoint answers");
        quic::client_authenticate(&conn, TOKEN).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        (peer, conn, send, recv)
    }

    /// A STUN server that answers nothing: a direct endpoint takes
    /// [`STUN_TIMEOUT`] to stand up against it.
    fn silent_stun(sock: &std::net::UdpSocket) -> Vec<IceServer> {
        vec![IceServer {
            urls: vec![format!("stun:{}", sock.local_addr().unwrap())],
            username: None,
            credential: None,
        }]
    }

    /// The member dials the source the grant stood up, pinning its
    /// certificate and presenting the token, and hears where its copy
    /// stands: an empty copy is a prefix, answered with the source's tip. A
    /// close the server sends ends the member's side at once, and the
    /// source's ends with its connection, and says so.
    #[tokio::test]
    async fn a_member_dials_its_source_and_hears_where_its_copy_stands() {
        let mut src = rig(true, 4);
        let mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one", "two"]);
        let tip = src.store.chain_tip(&sid.to_hex()).await.unwrap().unwrap();
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let grant_id = served(&src, &mem, sid, TOKEN).await;
        assert_eq!(
            answer_of(&mem, grant_id).await,
            SourceMsg::Hello {
                v: wire::STREAM_V,
                tip: Mark {
                    seq: tip.seq,
                    hash: hex::encode(tip.hash),
                },
            },
            "an empty copy is a prefix"
        );
        mem.sup.close_stream(grant_id, "test");
        assert!(!mem.sup.holds_stream(grant_id), "the member's side is gone");
        assert_eq!(
            close_of(&mut src, grant_id).await.as_deref(),
            Some("closed"),
            "the source's ends with the member's connection"
        );
        assert!(!src.sup.holds_stream(grant_id), "and the source's");
    }

    /// The relay rung, end to end, over a TURN server: each side its own
    /// allocation, the member's relayed address relayed to the source for
    /// its permission, and the same opening exchange over both.
    #[tokio::test]
    async fn a_member_follows_its_source_over_two_turn_allocations() {
        let (_turn, ice) = loopback_turn().await;
        let src = rig(true, 4);
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one", "two"]);
        let tip = src.store.chain_tip(&sid.to_hex()).await.unwrap().unwrap();
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let mut g = grant(sid, HiveStreamPath::Relay);
        g.ice_servers = ice;
        let grant_id = served_on(&src, &mem, g, TOKEN).await;
        let addr = candidate_of(&mut mem, grant_id)
            .await
            .expect("the member sends its relayed address");
        src.sup.take_candidate(grant_id, &addr, true);
        assert_eq!(
            answer_of(&mem, grant_id).await,
            SourceMsg::Hello {
                v: wire::STREAM_V,
                tip: Mark {
                    seq: tip.seq,
                    hash: hex::encode(tip.hash),
                },
            },
            "over both allocations"
        );
        mem.sup.close_stream(grant_id, "test");
        src.sup.close_stream(grant_id, "test");
    }

    /// Without its permission on the source's allocation, the member's
    /// opening Initials never arrive: the source, the QUIC server, never
    /// sends first, so the TURN server drops them.
    #[tokio::test]
    async fn without_its_permission_the_members_initials_never_arrive() {
        let (_turn, ice) = loopback_turn().await;
        let src = rig(true, 4);
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let mut g = grant(sid, HiveStreamPath::Relay);
        g.ice_servers = ice;
        let grant_id = served_on(&src, &mem, g, TOKEN).await;
        // The candidate is sent, and not passed on.
        assert!(candidate_of(&mut mem, grant_id).await.is_some());
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("unreachable"),
            "no permission, no connection"
        );
        assert!(mem.sup.stream_answer(grant_id).is_none());
        src.sup.close_stream(grant_id, "test");
    }

    /// A candidate that arrives before the source's allocation landed is
    /// held, and permitted once it lands, before the ready. (A TURN
    /// permission is an IP's alone, so the held address's port need not be
    /// the member's.)
    #[tokio::test]
    async fn a_candidate_before_the_allocation_is_permitted_when_it_lands() {
        let (_turn, ice) = loopback_turn().await;
        let src = rig(true, 4);
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let mut g = grant(sid, HiveStreamPath::Relay);
        g.ice_servers = ice.clone();
        let ids = (g.grant_id, g.session_id, g.path);
        let (tx, mut rx) = mpsc::channel(4);
        src.sup.take_grant(g, true, tx);
        // One thread, nothing awaited yet: the grant's task has not run.
        src.sup.take_candidate(ids.0, "127.0.0.1:9", true);
        assert_eq!(src.sup.held_candidates(ids.0), 1, "held for the allocation");
        let ready = rx.recv().await.expect("a ready");
        dial(&mem, ids, ready, TOKEN, ice);
        // The member's own candidate is not passed on: the held one permits it.
        assert!(candidate_of(&mut mem, ids.0).await.is_some());
        assert!(
            matches!(answer_of(&mem, ids.0).await, SourceMsg::Hello { .. }),
            "permitted by the held address"
        );
        mem.sup.close_stream(ids.0, "test");
        src.sup.close_stream(ids.0, "test");
    }

    /// A grant is registered before its endpoint is stood up, so the
    /// server's close ends a setup still in flight: no endpoint is left to
    /// wait out the grant, and no ready follows.
    #[tokio::test]
    async fn a_close_while_the_endpoint_is_stood_up_ends_it() {
        let src = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut g = grant(sid, HiveStreamPath::Direct);
        g.ice_servers = silent_stun(&silent);
        let grant_id = g.grant_id;
        let (tx, mut rx) = mpsc::channel(4);
        src.sup.take_grant(g, true, tx);
        assert!(
            src.sup.holds_stream(grant_id),
            "registered before its endpoint is up"
        );
        // Into the STUN round, then the server's close.
        tokio::time::sleep(Duration::from_millis(200)).await;
        src.sup.close_stream(grant_id, "test");
        assert!(!src.sup.holds_stream(grant_id));
        if let Ok(Some(m)) = tokio::time::timeout(STUN_TIMEOUT * 2, rx.recv()).await {
            panic!("a ready for a closed grant: {m:?}");
        }
    }

    /// Two DERP muxes, a source's and a member's, wired back to back as the
    /// server's `/derp` relay joins two devices: what one sends toward the
    /// other's key arrives at the other stamped with the sender's.
    fn derp_pair() -> (DerpTunnelHandle, DerpTunnelHandle) {
        use tunnel_core::transport::derp::{DerpMux, DerpPubKey, OutboundFrame, pubkey_hex};
        fn pump(mut out: mpsc::Receiver<OutboundFrame>, from: DerpPubKey, to: Arc<DerpMux>) {
            tokio::spawn(async move {
                while let Some((_, frame)) = out.recv().await {
                    if frame.len() >= 32 && frame[..32] == to.self_pubkey() {
                        let mut stamped = from.to_vec();
                        stamped.extend_from_slice(&frame[32..]);
                        to.deliver(&stamped);
                    }
                }
            });
        }
        let (ka, kb) = ([0xA1u8; 32], [0xB2u8; 32]);
        let (a, a_out) = DerpMux::new(ka);
        let (b, b_out) = DerpMux::new(kb);
        pump(a_out, ka, Arc::clone(&b));
        pump(b_out, kb, Arc::clone(&a));
        (
            DerpTunnelHandle {
                mux: a,
                self_pubkey_hex: pubkey_hex(&ka),
            },
            DerpTunnelHandle {
                mux: b,
                self_pubkey_hex: pubkey_hex(&kb),
            },
        )
    }

    /// The derp rung, end to end: the source serves over its own `/derp`
    /// connection on the stream's own route toward the member's key, `ready`
    /// names its key and no address, and the member dials that key.
    #[tokio::test]
    async fn a_member_follows_its_source_over_derp() {
        let (on_src, on_mem) = derp_pair();
        let src = rig(true, 4);
        let mem = rig(true, 4);
        src.sup.use_derp_for_tests(Some(on_src.clone()));
        mem.sup.use_derp_for_tests(Some(on_mem.clone()));
        let sid = ObjectId::new();
        note(&src, sid, &["one", "two"]);
        let tip = src.store.chain_tip(&sid.to_hex()).await.unwrap().unwrap();
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let mut g = grant(sid, HiveStreamPath::Derp);
        g.member_derp_pubkey = Some(on_mem.self_pubkey_hex.clone());
        let ids = (g.grant_id, g.session_id, g.path);
        let ready = ready_of(&src, g, true).await;
        let ClientMsg::HiveReplicaReady { derp_pubkey, .. } = &ready else {
            panic!("not a ready: {ready:?}");
        };
        assert_eq!(
            derp_pubkey.as_deref(),
            Some(on_src.self_pubkey_hex.as_str()),
            "the source's own key"
        );
        dial(&mem, ids, ready, TOKEN, Vec::new());
        assert_eq!(
            answer_of(&mem, ids.0).await,
            SourceMsg::Hello {
                v: wire::STREAM_V,
                tip: Mark {
                    seq: tip.seq,
                    hash: hex::encode(tip.hash),
                },
            },
            "over DERP"
        );
        mem.sup.close_stream(ids.0, "test");
        src.sup.close_stream(ids.0, "test");
    }

    /// Streams between one pair carry at once on DERP, each on its own
    /// route, and leave the pair's tunnel route alone: the tunnel's route is
    /// one per peer, so a stream on it would have taken a `quic-derp-v1`
    /// tunnel's, and the second stream the first's.
    #[tokio::test]
    async fn streams_between_one_pair_carry_at_once_beside_its_tunnel() {
        let (on_src, on_mem) = derp_pair();
        let src = rig(true, 4);
        let mem = rig(true, 4);
        src.sup.use_derp_for_tests(Some(on_src.clone()));
        mem.sup.use_derp_for_tests(Some(on_mem.clone()));
        // What a `quic-derp-v1` tunnel between the same two devices holds.
        let tunnel_at_src = on_src.mux.tunnel_conn_for(on_mem.mux.self_pubkey());
        let tunnel_at_mem = on_mem.mux.tunnel_conn_for(on_src.mux.self_pubkey());
        let (s1, s2) = (ObjectId::new(), ObjectId::new());
        note(&src, s1, &["one"]);
        note(&src, s2, &["two"]);
        mem.store.join(&s1.to_hex(), "owner", 1).await.unwrap();
        mem.store.join(&s2.to_hex(), "owner", 1).await.unwrap();

        let mut ids = Vec::new();
        for sid in [s1, s2] {
            let mut g = grant(sid, HiveStreamPath::Derp);
            g.member_derp_pubkey = Some(on_mem.self_pubkey_hex.clone());
            ids.push(served_on(&src, &mem, g, TOKEN).await);
        }
        for (n, grant_id) in ids.iter().enumerate() {
            assert!(
                matches!(answer_of(&mem, *grant_id).await, SourceMsg::Hello { .. }),
                "stream {n} answered"
            );
        }
        tunnel_at_src
            .send_to(&[0xC3, 0, 0, 0, 1], tunnel_at_src.synth_peer())
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let (n, _) =
            tokio::time::timeout(Duration::from_secs(2), tunnel_at_mem.recv_from(&mut buf))
                .await
                .expect("the tunnel's packet arrives on its own route")
                .unwrap();
        assert_eq!(&buf[..n], &[0xC3, 0, 0, 0, 1]);
        for grant_id in ids {
            mem.sup.close_stream(grant_id, "test");
            src.sup.close_stream(grant_id, "test");
        }
    }

    /// A derp grant or dial this device cannot carry is refused in words: no
    /// live DERP connection, a grant naming no member key, a dial naming no
    /// source key.
    #[tokio::test]
    async fn a_derp_grant_or_dial_this_device_cannot_carry_is_refused() {
        let src = rig(true, 4);
        let mut mem = rig(true, 4);
        src.sup.use_derp_for_tests(None);
        mem.sup.use_derp_for_tests(None);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let mut g = grant(sid, HiveStreamPath::Derp);
        g.member_derp_pubkey = Some("1f".repeat(32));
        assert_eq!(
            refusal_of(ready_of(&src, g, true).await),
            Some(HiveStreamRefusal::PathUnavailable),
            "no live DERP connection"
        );
        let (on_src, on_mem) = derp_pair();
        src.sup.use_derp_for_tests(Some(on_src));
        assert_eq!(
            refusal_of(ready_of(&src, grant(sid, HiveStreamPath::Derp), true).await),
            Some(HiveStreamRefusal::PathUnavailable),
            "a grant that names no member key"
        );

        let derp_dial = |key: Option<String>| Dial {
            grant_id: ObjectId::new(),
            session_id: sid,
            fence: 1,
            source_id: ObjectId::new(),
            path: Some(HiveStreamPath::Derp),
            token: TOKEN.into(),
            cert_fingerprint: "00".repeat(32),
            addrs: Vec::new(),
            ice_servers: Vec::new(),
            derp_pubkey: key,
            ttl_secs: 30,
        };
        let d = derp_dial(Some("2e".repeat(32)));
        let grant_id = d.grant_id;
        mem.sup.follow_dial(d, true);
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("no_derp"),
            "no live DERP connection"
        );
        mem.sup.use_derp_for_tests(Some(on_mem));
        let d = derp_dial(None);
        let grant_id = d.grant_id;
        mem.sup.follow_dial(d, true);
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("no_derp_key"),
            "a dial that names no source key"
        );
    }

    /// A copy whose tip is off the source's chain hears where the two
    /// diverge: genesis, when they share no checkpoint.
    #[tokio::test]
    async fn a_copy_off_the_sources_chain_hears_where_they_diverge() {
        let src = rig(true, 4);
        let mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one", "two"]);
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();
        note(&mem, sid, &["something else"]);
        assert!(mem.store.chain_tip(&sid.to_hex()).await.unwrap().is_some());

        let grant_id = served(&src, &mem, sid, TOKEN).await;
        assert_eq!(
            answer_of(&mem, grant_id).await,
            SourceMsg::Diverged {
                at: Mark::genesis()
            },
            "off the source's chain, and no checkpoint shared"
        );
        mem.sup.close_stream(grant_id, "test");
        src.sup.close_stream(grant_id, "test");
    }

    /// A member that presents another token is turned away before any
    /// hello: the source says `auth_failed`, and the member hears nothing.
    #[tokio::test]
    async fn a_wrong_token_is_turned_away_before_any_hello() {
        let mut src = rig(true, 4);
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let grant_id = served(&src, &mem, sid, "not-the-token-the-source-holds").await;
        assert_eq!(
            close_of(&mut src, grant_id).await.as_deref(),
            Some("auth_failed"),
            "the source turns the connection away"
        );
        let theirs = close_of(&mut mem, grant_id).await;
        assert!(mem.sup.stream_answer(grant_id).is_none(), "no answer");
        assert!(
            theirs.as_deref().is_some_and(|t| !t.is_empty()),
            "the member says why: {theirs:?}"
        );
    }

    /// A grant the source cannot serve is refused in its own word, and no
    /// endpoint is stood up for it.
    #[tokio::test]
    async fn a_grant_the_source_cannot_serve_is_refused_in_words() {
        let src = rig(true, 4);
        src.sup.use_derp_for_tests(None);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        let mut derp = grant(sid, HiveStreamPath::Derp);
        derp.member_derp_pubkey = Some("1f".repeat(32));
        assert_eq!(
            refusal_of(ready_of(&src, derp, true).await),
            Some(HiveStreamRefusal::PathUnavailable),
            "a derp grant on a device with no DERP connection"
        );
        let unnamed = Grant {
            path: None,
            ..grant(sid, HiveStreamPath::Direct)
        };
        assert_eq!(
            refusal_of(ready_of(&src, unnamed, true).await),
            Some(HiveStreamRefusal::PathUnavailable),
            "a rung this build cannot name"
        );
        assert_eq!(
            refusal_of(ready_of(&src, grant(sid, HiveStreamPath::Relay), true).await),
            Some(HiveStreamRefusal::PathUnavailable),
            "a relay grant that carries no TURN server"
        );
        assert_eq!(
            refusal_of(ready_of(&src, grant(ObjectId::new(), HiveStreamPath::Direct), true).await),
            Some(HiveStreamRefusal::NotHeld),
            "a session it holds nothing of"
        );
        assert_eq!(
            refusal_of(ready_of(&src, grant(sid, HiveStreamPath::Direct), false).await),
            Some(HiveStreamRefusal::Other),
            "a secondary org's connection"
        );
        assert_eq!(src.sup.serving(), 0, "nothing stood up");
    }

    /// A source at its limit refuses the next grant `busy`, its endpoints
    /// still being stood up counted, and serves again once one ends.
    #[tokio::test]
    async fn a_source_at_its_limit_refuses_busy() {
        let src = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut held = Vec::new();
        for _ in 0..stream_limits::MAX_PER_SOURCE {
            let mut g = grant(sid, HiveStreamPath::Direct);
            g.ice_servers = silent_stun(&silent);
            held.push(g.grant_id);
            let (tx, _rx) = mpsc::channel(4);
            src.sup.take_grant(g, true, tx);
        }
        assert_eq!(src.sup.serving(), stream_limits::MAX_PER_SOURCE);
        assert_eq!(
            refusal_of(ready_of(&src, grant(sid, HiveStreamPath::Direct), true).await),
            Some(HiveStreamRefusal::Busy),
            "one past the limit, while all are still being stood up"
        );
        src.sup.close_stream(held[0], "test");
        let g = grant(sid, HiveStreamPath::Direct);
        held.push(g.grant_id);
        assert_eq!(
            refusal_of(ready_of(&src, g, true).await),
            None,
            "a slot freed is served again"
        );
        for grant_id in held {
            src.sup.close_stream(grant_id, "test");
        }
        assert_eq!(src.sup.serving(), 0);
    }

    /// A grant or a dial this device holds already is not taken twice: a
    /// replay answers nothing, and the stream it started is left alone.
    #[tokio::test]
    async fn a_grant_or_dial_held_already_is_not_taken_twice() {
        let mut src = rig(true, 4);
        let mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();
        let g = grant(sid, HiveStreamPath::Direct);
        let replay = Grant {
            grant_id: g.grant_id,
            ..grant(sid, HiveStreamPath::Direct)
        };
        let ids = (g.grant_id, g.session_id, g.path);
        let ready = ready_of(&src, g, true).await;
        let (tx, mut rx) = mpsc::channel(4);
        src.sup.take_grant(replay, true, tx);
        assert!(
            rx.recv().await.is_none(),
            "a replayed grant answers nothing"
        );

        dial(&mem, ids, ready.clone(), TOKEN, Vec::new());
        dial(&mem, ids, ready, TOKEN, Vec::new());
        assert!(
            matches!(answer_of(&mem, ids.0).await, SourceMsg::Hello { .. }),
            "the first is followed through"
        );
        // The follow the server's close ends is the one connected: had a
        // replay taken its place, the first would hold its connection on.
        mem.sup.close_stream(ids.0, "test");
        assert_eq!(
            close_of(&mut src, ids.0).await.as_deref(),
            Some("closed"),
            "the one connection let go"
        );
    }

    /// A member follows at most its limit at once; a dial past it is closed
    /// `busy`, and nothing is dialed.
    #[tokio::test]
    async fn a_member_follows_at_most_its_limit() {
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();
        // A socket that takes the member's packets and answers none.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let to_silence = || Dial {
            grant_id: ObjectId::new(),
            session_id: sid,
            fence: 1,
            source_id: ObjectId::new(),
            path: Some(HiveStreamPath::Direct),
            token: TOKEN.into(),
            cert_fingerprint: "00".repeat(32),
            addrs: vec![silent.local_addr().unwrap().to_string()],
            ice_servers: Vec::new(),
            derp_pubkey: None,
            ttl_secs: 30,
        };
        let mut following = Vec::new();
        for _ in 0..MAX_FOLLOWING {
            let d = to_silence();
            following.push(d.grant_id);
            mem.sup.follow_dial(d, true);
        }
        let past = to_silence();
        let grant_id = past.grant_id;
        mem.sup.follow_dial(past, true);
        assert!(
            !mem.sup.holds_stream(grant_id),
            "one past the limit is not followed"
        );
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("busy"),
            "and is closed in its word"
        );
        for grant_id in following {
            mem.sup.close_stream(grant_id, "test");
        }
    }

    /// Every side ends with its grant at the latest, whatever the other
    /// does: an endpoint nobody dials, and a member whose source never
    /// answers.
    #[tokio::test]
    async fn every_side_ends_with_its_grant() {
        let mut src = rig(true, 4);
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();

        let mut g = grant(sid, HiveStreamPath::Direct);
        g.ttl_secs = 1;
        let grant_id = g.grant_id;
        assert_eq!(refusal_of(ready_of(&src, g, true).await), None);
        assert_eq!(
            close_of(&mut src, grant_id).await.as_deref(),
            Some("expired"),
            "the endpoint nobody dialed"
        );
        assert!(!src.sup.holds_stream(grant_id));

        // A socket that takes the member's packets and answers none.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let grant_id = ObjectId::new();
        mem.sup.follow_dial(
            Dial {
                grant_id,
                session_id: sid,
                fence: 1,
                source_id: ObjectId::new(),
                path: Some(HiveStreamPath::Direct),
                token: TOKEN.into(),
                cert_fingerprint: "00".repeat(32),
                addrs: vec![silent.local_addr().unwrap().to_string()],
                ice_servers: Vec::new(),
                derp_pubkey: None,
                ttl_secs: 1,
            },
            true,
        );
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("expired"),
            "the member its source never answered"
        );
        assert!(!mem.sup.holds_stream(grant_id));
    }

    /// A dial for a session this device never joined is closed at once,
    /// and nothing is dialed.
    #[tokio::test]
    async fn a_dial_for_a_session_not_joined_is_closed_unasked() {
        let src = rig(true, 4);
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        let grant_id = served(&src, &mem, sid, TOKEN).await;
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("not_a_member"),
            "a session never joined"
        );
        src.sup.close_stream(grant_id, "test");
    }

    /// A dial this device does not follow is closed at once, in its own
    /// word, and nothing is dialed: one on a secondary org's connection,
    /// and one on a rung this build cannot name.
    #[tokio::test]
    async fn a_dial_this_device_does_not_follow_is_closed_unasked() {
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();
        let dial = |path| Dial {
            grant_id: ObjectId::new(),
            session_id: sid,
            fence: 1,
            source_id: ObjectId::new(),
            path,
            token: TOKEN.into(),
            cert_fingerprint: "00".repeat(32),
            addrs: vec!["127.0.0.1:9".into()],
            ice_servers: Vec::new(),
            derp_pubkey: None,
            ttl_secs: 30,
        };
        for (d, primary, word) in [
            (dial(Some(HiveStreamPath::Direct)), false, "secondary_org"),
            (dial(None), true, "path_unavailable"),
        ] {
            let grant_id = d.grant_id;
            mem.sup.follow_dial(d, primary);
            assert!(!mem.sup.holds_stream(grant_id), "{word}: nothing followed");
            assert_eq!(
                close_of(&mut mem, grant_id).await.as_deref(),
                Some(word),
                "{word}: closed in its word"
            );
        }
    }

    /// A relay dial that carries no TURN server is closed `no_turn`: the
    /// member has nothing to allocate on.
    #[tokio::test]
    async fn a_relay_dial_without_a_turn_server_is_closed() {
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();
        let grant_id = ObjectId::new();
        mem.sup.follow_dial(
            Dial {
                grant_id,
                session_id: sid,
                fence: 1,
                source_id: ObjectId::new(),
                path: Some(HiveStreamPath::Relay),
                token: TOKEN.into(),
                cert_fingerprint: "00".repeat(32),
                addrs: vec!["198.51.100.4:49152".into()],
                ice_servers: Vec::new(),
                derp_pubkey: None,
                ttl_secs: 30,
            },
            true,
        );
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("no_turn"),
            "nothing to allocate on"
        );
    }

    /// A dial that names no address a source could hold is closed
    /// `no_address`, before anything is dialed. (The two here are ones QUIC
    /// itself refuses, so a member that tried them would end `unreachable`
    /// at once, and send nothing either way.)
    #[tokio::test]
    async fn a_dial_to_nowhere_a_source_could_be_is_closed() {
        let mut mem = rig(true, 4);
        let sid = ObjectId::new();
        mem.store.join(&sid.to_hex(), "owner", 1).await.unwrap();
        let grant_id = ObjectId::new();
        mem.sup.follow_dial(
            Dial {
                grant_id,
                session_id: sid,
                fence: 1,
                source_id: ObjectId::new(),
                path: Some(HiveStreamPath::Direct),
                token: TOKEN.into(),
                cert_fingerprint: "00".repeat(32),
                addrs: vec!["0.0.0.0:41000".into(), "192.168.1.20:0".into()],
                ice_servers: Vec::new(),
                derp_pubkey: None,
                ttl_secs: 30,
            },
            true,
        );
        assert_eq!(
            close_of(&mut mem, grant_id).await.as_deref(),
            Some("no_address"),
            "nowhere a source could be"
        );
    }

    /// A candidate no member could hold is never permitted, and one for no
    /// relay stream of this device's is dropped.
    #[tokio::test]
    async fn only_a_candidate_a_member_could_hold_is_held() {
        let src = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        // A relay grant whose allocation never lands: its TURN server is
        // silent, so what arrives meanwhile is held, or is not.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut g = grant(sid, HiveStreamPath::Relay);
        g.ice_servers = vec![IceServer {
            urls: vec![format!(
                "turn:{}?transport=udp",
                silent.local_addr().unwrap()
            )],
            username: Some(TURN_USER.into()),
            credential: Some(TURN_PASS.into()),
        }];
        let grant_id = g.grant_id;
        let (tx, _rx) = mpsc::channel(4);
        src.sup.take_grant(g, true, tx);
        for nowhere in [
            "224.0.0.251:5353",
            "255.255.255.255:9",
            "0.0.0.0:9",
            "relay.example:3478",
        ] {
            src.sup.take_candidate(grant_id, nowhere, true);
        }
        assert_eq!(
            src.sup.held_candidates(grant_id),
            0,
            "nothing a member could hold"
        );
        src.sup.take_candidate(grant_id, "198.51.100.4:3478", false);
        assert_eq!(
            src.sup.held_candidates(grant_id),
            0,
            "nor on a secondary org's connection"
        );
        src.sup.take_candidate(grant_id, "198.51.100.4:3478", true);
        assert_eq!(src.sup.held_candidates(grant_id), 1);
        src.sup.close_stream(grant_id, "test");
    }

    /// A hello that is not the grant's, for another session or under
    /// another fence, is refused in words although its token was right:
    /// the token proves the grant, and the hello must be for it. The
    /// refusal reaches the member before the source lets the connection go.
    #[tokio::test]
    async fn a_hello_that_is_not_the_grants_is_refused() {
        let mut src = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        for (session, fence, word) in [
            (ObjectId::new().to_hex(), 1, "wrong_session"),
            (sid.to_hex(), 2, "wrong_fence"),
        ] {
            let g = grant(sid, HiveStreamPath::Direct);
            let grant_id = g.grant_id;
            let (_peer, conn, mut send, mut recv) =
                dial_by_hand(ready_of(&src, g, true).await).await;
            let hello = MemberMsg::Hello {
                v: wire::STREAM_V,
                session,
                fence,
                tip: Mark::genesis(),
                floor: 1,
                checkpoints: Vec::new(),
            };
            write_frame(&mut send, &hello).await.unwrap();
            let answer: SourceMsg = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv))
                .await
                .expect("an answer in time")
                .expect("the refusal arrives whole");
            assert_eq!(
                answer,
                SourceMsg::Refused {
                    reason: word.into()
                },
                "{word}"
            );
            // The member, the last to receive, lets go, and the source
            // then ends, saying it refused.
            conn.close(0u32.into(), b"done");
            assert_eq!(
                close_of(&mut src, grant_id).await.as_deref(),
                Some("refused"),
                "{word}: the source says it refused"
            );
        }
    }

    /// A frame that announces more than the limit is refused on its
    /// header: the source does not wait for a body it would not read, and
    /// says so.
    #[tokio::test]
    async fn a_frame_past_the_limit_is_refused_before_it_is_read() {
        let mut src = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        let g = grant(sid, HiveStreamPath::Direct);
        let grant_id = g.grant_id;
        let (_peer, conn, mut send, _recv) = dial_by_hand(ready_of(&src, g, true).await).await;
        let past = u32::try_from(wire::MAX_FRAME + 1).unwrap();
        send.write_all(&past.to_le_bytes()).await.unwrap();
        // The header alone: a reader that took it would wait for the body,
        // and end `no_hello` when the stream finishes short of it.
        send.finish().unwrap();
        assert_eq!(
            close_of(&mut src, grant_id).await.as_deref(),
            Some("bad_frame"),
            "refused on the header"
        );
        conn.close(0u32.into(), b"done");
    }
}
