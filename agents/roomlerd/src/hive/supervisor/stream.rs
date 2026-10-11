// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2d-2a — the replica stream on the device, on the `direct` rung
//! (spec §3b, "The carrier" and "The stream"): the source's endpoint for a
//! grant, the member's dial, the token, and the opening exchange.
//!
//! | Side | On | Does |
//! |---|---|---|
//! | source | `rc:hive.replica.grant` | checks it holds the session, binds one UDP socket, learns its server-reflexive address over STUN on that very socket, stands a QUIC endpoint up on it, and answers `rc:hive.replica.ready` with its certificate and addresses |
//! | source | the member's connection | serves ONE: the token first, then the member's hello, answered from its chain |
//! | member | `rc:hive.replica.dial` | dials the addresses in order, pinning the certificate, presents the token, and says where its copy ends |
//! | either | the server's `rc:hive.replica.close`, the connection's end, or the grant's | the endpoint and its socket go, and the server is told why |
//!
//! The events, blobs and acks after the opening exchange are P2d-3; the
//! `relay` and `derp` rungs are P2d-2b and P2d-2c, refused `path_unavailable`
//! until then. This build does not advertise `hive-stream` yet: a source that
//! answers the hello but streams no event would hold a member waiting for
//! nothing, so the verb is advertised once events flow (P2d-3).
//!
//! ⚠️ Every endpoint and every connection ends by the grant's `ttl_secs` at
//! the latest, whatever the other side does: each holds a UDP socket, and
//! sockets leaked one grant at a time are how a daemon ran out of ports
//! before (`docs/tunnels.md`).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{HiveStreamPath, HiveStreamRefusal, stream_limits};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};
use roomler_hive_node::stream::{self as wire, MemberMsg, SourceMsg};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, info, warn};
use tunnel_core::transport::quic::{self, QuicConnection, QuicPeer, RecvStream, SendStream};

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
    /// The source's answer to the hello, once the member heard it.
    pub(super) answer: Option<SourceMsg>,
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
    pub ttl_secs: u32,
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

/// The source's endpoint on the `direct` rung: one UDP socket, STUN asked
/// on it first so the reflexive address is the mapping QUIC then uses, and
/// a QUIC server on that same socket. Returns it with its fingerprint and
/// the addresses to dial.
async fn direct_endpoint(ice: &[IceServer]) -> anyhow::Result<(QuicPeer, String, Vec<String>)> {
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
    Ok((peer, fingerprint, addrs))
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

/// Whether `addr` is one a source could hold: never the unspecified
/// address, port 0, multicast or broadcast. The addresses reach the member
/// through the server from the source, and a member dials as SYSTEM or
/// root: a multicast address relayed to every member of a tenant would have
/// each of them spray its own network.
fn dialable(addr: &SocketAddr) -> bool {
    let ip = addr.ip();
    addr.port() != 0
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !matches!(ip, IpAddr::V4(v4) if v4.is_broadcast())
}

impl Supervisor {
    /// The streams this device serves now, as a source.
    fn serving(&self) -> usize {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|s| s.role == Role::Source)
            .count()
    }

    /// `rc:hive.replica.grant`: stand an endpoint up for the member, and
    /// answer with what it dials, or why not.
    pub(super) async fn serve_grant(self: &Arc<Self>, g: Grant, primary: bool) -> ClientMsg {
        let grant_id = g.grant_id;
        let refuse = |word: HiveStreamRefusal, why: String| {
            info!(grant = %grant_id, refused = word.as_str(), %why, "hive stream: a grant not served");
            ClientMsg::HiveReplicaReady {
                grant_id,
                refused: Some(word),
                detail: Some(why),
                cert_fingerprint: None,
                addrs: Vec::new(),
                derp_pubkey: None,
            }
        };
        if !primary {
            return refuse(
                HiveStreamRefusal::Other,
                "this device serves streams for its primary organization only".into(),
            );
        }
        if g.path != Some(HiveStreamPath::Direct) {
            return refuse(
                HiveStreamRefusal::PathUnavailable,
                "this build serves the direct rung only".into(),
            );
        }
        let Ok(store) = &self.store else {
            return refuse(
                HiveStreamRefusal::NotHeld,
                "the replica store is unavailable".into(),
            );
        };
        match store.chain_tip(&g.session_id.to_hex()).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return refuse(
                    HiveStreamRefusal::NotHeld,
                    "this device holds nothing of the session".into(),
                );
            }
            Err(e) => {
                return refuse(
                    HiveStreamRefusal::NotHeld,
                    format!("the replica store could not be read: {e}"),
                );
            }
        }
        if self.serving() >= stream_limits::MAX_PER_SOURCE {
            return refuse(
                HiveStreamRefusal::Busy,
                format!(
                    "this device serves {} streams already",
                    stream_limits::MAX_PER_SOURCE
                ),
            );
        }
        let (peer, fingerprint, addrs) = match direct_endpoint(&g.ice_servers).await {
            Ok(e) => e,
            Err(e) => {
                return refuse(
                    HiveStreamRefusal::Other,
                    format!("the endpoint could not be bound: {e}"),
                );
            }
        };
        let session_id = g.session_id;
        let member = g.member_id;
        let sup = Arc::clone(self);
        // Held across the spawn, so the task's end finds the entry in.
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let task = tokio::spawn(async move {
            let reason = sup.serve_one(peer, &g).await;
            sup.stream_ended(g.grant_id, &reason);
        });
        streams.insert(
            grant_id,
            Stream {
                session_id,
                role: Role::Source,
                answer: None,
                task: task.abort_handle(),
            },
        );
        drop(streams);
        info!(grant = %grant_id, session = %session_id, member = %member, addrs = ?addrs, "hive stream: serving");
        ClientMsg::HiveReplicaReady {
            grant_id,
            refused: None,
            detail: None,
            cert_fingerprint: Some(fingerprint),
            addrs,
            derp_pubkey: None,
        }
    }

    /// One connection, for the grant's life at most: the member's token,
    /// then its hello answered from this device's chain. The reason it
    /// ended, as the close says it.
    async fn serve_one(&self, peer: QuicPeer, g: &Grant) -> String {
        let ttl = Duration::from_secs(u64::from(g.ttl_secs));
        let served = tokio::time::timeout(ttl, async {
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
        if d.path != Some(HiveStreamPath::Direct) {
            return self.report_close(d.grant_id, "path_unavailable");
        }
        let grant_id = d.grant_id;
        let session_id = d.session_id;
        let sup = Arc::clone(self);
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let task = tokio::spawn(async move {
            let reason = sup.follow_one(&d).await;
            sup.stream_ended(d.grant_id, &reason);
        });
        streams.insert(
            grant_id,
            Stream {
                session_id,
                role: Role::Member,
                answer: None,
                task: task.abort_handle(),
            },
        );
    }

    /// The member's side of one stream, for the grant's life at most.
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
            .filter(dialable)
            .collect();
        if addrs.is_empty() {
            return "no_address".to_string();
        }
        let peer = match QuicPeer::client(
            "0.0.0.0:0".parse().expect("a bind address"),
            &d.cert_fingerprint,
        ) {
            Ok(p) => p,
            Err(e) => return format!("the client could not be bound: {e}"),
        };
        let ttl = Duration::from_secs(u64::from(d.ttl_secs));
        let followed = tokio::time::timeout(ttl, async {
            let Some(conn) = connect_first(&peer, &addrs).await else {
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
    /// and nothing is said back (the server already forgot the grant).
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
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::{Rig, rig};
    use super::*;
    use roomler_hive_node::TranscriptEvent;
    use roomler_hive_node::stream::Mark;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

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

    /// The source serves a grant for a session it holds, and the member is
    /// told to dial it as the server would tell it: the grant's id, the
    /// source's certificate and addresses, and `token`.
    async fn served(src: &Rig, mem: &Rig, sid: ObjectId, token: &str) -> ObjectId {
        let g = grant(sid, HiveStreamPath::Direct);
        let grant_id = g.grant_id;
        let ready = src.sup.serve_grant(g, true).await;
        let ClientMsg::HiveReplicaReady {
            refused: None,
            cert_fingerprint: Some(fp),
            addrs,
            ..
        } = ready
        else {
            panic!("not ready: {ready:?}");
        };
        assert!(!addrs.is_empty(), "somewhere to dial");
        mem.sup.follow_dial(
            Dial {
                grant_id,
                session_id: sid,
                fence: 1,
                source_id: ObjectId::new(),
                path: Some(HiveStreamPath::Direct),
                token: token.into(),
                cert_fingerprint: fp,
                addrs,
                ttl_secs: 30,
            },
            true,
        );
        grant_id
    }

    async fn answer_of(mem: &Rig, grant_id: ObjectId) -> SourceMsg {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(a) = mem.sup.stream_answer(grant_id) {
                return a;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the member heard no answer"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The reason of the next `rc:hive.replica.close` the rig's device
    /// sends the server for `grant_id`, or `None` when none comes within
    /// 15 s, so a test compares it in its own words.
    async fn close_of(r: &mut Rig, grant_id: ObjectId) -> Option<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
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
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        for path in [HiveStreamPath::Relay, HiveStreamPath::Derp] {
            assert_eq!(
                refusal_of(src.sup.serve_grant(grant(sid, path), true).await),
                Some(HiveStreamRefusal::PathUnavailable),
                "{path:?} is not served yet"
            );
        }
        assert_eq!(
            refusal_of(
                src.sup
                    .serve_grant(grant(ObjectId::new(), HiveStreamPath::Direct), true)
                    .await
            ),
            Some(HiveStreamRefusal::NotHeld),
            "a session it holds nothing of"
        );
        assert_eq!(
            refusal_of(
                src.sup
                    .serve_grant(grant(sid, HiveStreamPath::Direct), false)
                    .await
            ),
            Some(HiveStreamRefusal::Other),
            "a secondary org's connection"
        );
        assert_eq!(src.sup.serving(), 0, "nothing stood up");
    }

    /// A source that serves its limit already refuses the next grant
    /// `busy`, and serves again once one of them ends.
    #[tokio::test]
    async fn a_source_at_its_limit_refuses_busy() {
        let src = rig(true, 4);
        let sid = ObjectId::new();
        note(&src, sid, &["one"]);
        let mut held = Vec::new();
        for _ in 0..stream_limits::MAX_PER_SOURCE {
            let g = grant(sid, HiveStreamPath::Direct);
            held.push(g.grant_id);
            assert_eq!(refusal_of(src.sup.serve_grant(g, true).await), None);
        }
        assert_eq!(src.sup.serving(), stream_limits::MAX_PER_SOURCE);
        assert_eq!(
            refusal_of(
                src.sup
                    .serve_grant(grant(sid, HiveStreamPath::Direct), true)
                    .await
            ),
            Some(HiveStreamRefusal::Busy),
            "one past the limit"
        );
        src.sup.close_stream(held[0], "test");
        let g = grant(sid, HiveStreamPath::Direct);
        held.push(g.grant_id);
        assert_eq!(
            refusal_of(src.sup.serve_grant(g, true).await),
            None,
            "a slot freed is served again"
        );
        for grant_id in held {
            src.sup.close_stream(grant_id, "test");
        }
        assert_eq!(src.sup.serving(), 0);
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
        assert_eq!(refusal_of(src.sup.serve_grant(g, true).await), None);
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
    /// and one on a rung this build does not climb yet.
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
            path: Some(path),
            token: TOKEN.into(),
            cert_fingerprint: "00".repeat(32),
            addrs: vec!["127.0.0.1:9".into()],
            ttl_secs: 30,
        };
        for (d, primary, word) in [
            (dial(HiveStreamPath::Direct), false, "secondary_org"),
            (dial(HiveStreamPath::Relay), true, "path_unavailable"),
            (dial(HiveStreamPath::Derp), true, "path_unavailable"),
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

    /// Only an address a source could hold is dialed: never the unspecified
    /// address, port 0, multicast or broadcast.
    #[test]
    fn only_an_address_a_source_could_hold_is_dialed() {
        for a in [
            "192.168.1.20:41000",
            "203.0.113.7:41000",
            "127.0.0.1:41000",
            "[2001:db8::1]:41000",
        ] {
            assert!(dialable(&a.parse().unwrap()), "{a} is dialed");
        }
        for a in [
            "0.0.0.0:41000",
            "192.168.1.20:0",
            "224.0.0.251:5353",
            "239.255.255.250:1900",
            "255.255.255.255:41000",
            "[ff02::1]:41000",
            "[::]:41000",
        ] {
            assert!(!dialable(&a.parse().unwrap()), "{a} is never dialed");
        }
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
                dial_by_hand(src.sup.serve_grant(g, true).await).await;
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
        let (_peer, conn, mut send, _recv) = dial_by_hand(src.sup.serve_grant(g, true).await).await;
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
