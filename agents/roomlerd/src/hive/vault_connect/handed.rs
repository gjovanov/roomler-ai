// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-2, P8-2b-3 — a session's credential endpoints, served by its
//! worker.
//!
//! Where the device's owner turned workers on (`hive_worker`), what
//! `vault_connect` opens for a session is served in the session's worker:
//! the TLS and the wire protocols it parses from both sides, and the values
//! it presents. A fault in any of them costs the worker, and never the
//! daemon.
//!
//! | Piece | As built |
//! |---|---|
//! | what stays the daemon's | the listener it bound, at the address the session was given; the leases, read at each connection and let go at the end; the endpoint's close |
//! | what the worker is handed | the endpoint's [`Spec`], none of it secret, and a copy of its listener beside the call (`SCM_RIGHTS`) |
//! | a connection | accepted, admitted and served by the worker, which asks the daemon for the endpoint's values as it starts (`values`): read from its leases now, none once any is let go |
//! | the values | zeroized where either side drops them, in the frames that carry them too |
//! | a new worker | handed every endpoint again, on the same listeners: a connection the lost one held is gone, the address is not |
//! | no worker up | the endpoint is refused: where the owner turned workers on, it runs in the worker or not at all |
//! | through another device (P4-6-1b-2; P8-2a-6) | served in the worker too: the daemon keeps the flow toward the upstream, and the worker dials its end, a loopback port the daemon's account alone may use, which on Unix the worker is (P8-0's point 6); an end off this host is refused |
//!
//! ⚠️ On Windows the flow's end admits the daemon's own process alone, so
//! the worker's pid is admitted there with P8-2b.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, SocketAddrV4};
#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use bson::oid::ObjectId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::SOCKET;
use zeroize::Zeroizing;

use super::endpoint::loopback::Peer;
use super::endpoint::{Conf, Live, Place, Session, Sessions, serve};
use super::{Auth, Engine};
use crate::hive::worker::link::Link;
use crate::vault_proxy::{Dial, tls};

// ─── What crosses the channel ───────────────────────────────────────────────

/// A value across the channel: zeroized wherever it is dropped, and never in
/// a log.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(pub(crate) Zeroizing<String>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(|s| Self(Zeroizing::new(s)))
    }
}

/// A call across a worker's channel: an endpoint's, and (P8-2a-4b) the
/// egress proxy's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "c", rename_all = "snake_case")]
pub(crate) enum Call {
    /// The daemon: serve `endpoint` as `spec` says. Unix: the listener comes
    /// on the second channel, just before this frame. Windows (P8-2b-3): no
    /// second channel, so the listener rides along as `info`, its
    /// `WSAPROTOCOL_INFOW`, targeting the worker that receives this call —
    /// recomputed fresh at every hand-over and `rehand`.
    Serve {
        endpoint: u64,
        spec: Box<Spec>,
        #[cfg(windows)]
        info: Vec<u8>,
    },
    /// The daemon: stop serving `endpoint`, its connections cut.
    Close { endpoint: u64 },
    /// The worker: `endpoint`'s values, read from its leases now, for one
    /// connection.
    Values { endpoint: u64 },
    /// P8-2a-4b — the daemon: serve the run's egress fronts as `endpoint`,
    /// on the two listeners sent just before this frame, the HTTP front's
    /// first. Unix only so far (egress's own P8-2b-3, next).
    #[cfg(unix)]
    Egress {
        endpoint: u64,
        spec: Box<crate::hive::egress::handed::Spec>,
    },
    /// P8-2a-4b — the worker: the vault's decision for `host:port`. Unix
    /// only so far.
    #[cfg(unix)]
    Decide { host: String, port: u16 },
    /// P8-2a-4b — the worker: the run's placeholders toward `host:port`.
    /// Unix only so far.
    #[cfg(unix)]
    Bound { host: String, port: u16 },
    /// P8-2a-4b — the worker: the same, each value read from its lease now,
    /// for one request. Unix only so far.
    #[cfg(unix)]
    Valued { host: String, port: u16 },
    /// P8-2a-4b — the worker: the leaf the run's CA makes for `host`. Unix
    /// only so far.
    #[cfg(unix)]
    Leaf { host: String },
}

/// The answer to a [`Call`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "a", rename_all = "snake_case")]
pub(crate) enum Answer {
    Done,
    Refused {
        why: String,
    },
    /// The values, in the order the call leased them: none once any is let
    /// go.
    Values {
        values: Option<Vec<Secret>>,
    },
    /// P8-2a-4b — the vault's decision. Unix only so far.
    #[cfg(unix)]
    Decision {
        decision: crate::hive::egress::Decision,
    },
    /// P8-2a-4b — the run's placeholders toward a host. Unix only so far.
    #[cfg(unix)]
    Bound {
        bound: Vec<crate::hive::placeholder::Bound>,
    },
    /// P8-2a-4b — the same, each with its value: none once its lease is let
    /// go. Unix only so far.
    #[cfg(unix)]
    Valued {
        valued: Vec<(crate::hive::placeholder::Bound, Option<Secret>)>,
    },
    /// P8-2a-4b — a leaf, its key among it. Unix only so far.
    #[cfg(unix)]
    Leaf {
        leaf: crate::hive::egress::handed::Leaf,
    },
}

/// What a worker is handed to serve one endpoint with: none of it secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Spec {
    pub(crate) engine: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) auth_source: Option<String>,
    pub(crate) auth: Option<String>,
    pub(crate) front: String,
    pub(crate) secret: String,
    pub(crate) user: Option<String>,
    /// The PEM of the CA the call leased, public: the worker's roots are the
    /// system's and this.
    pub(crate) ca: Option<String>,
    pub(crate) at: At,
    /// P8-2a-6 — through another device (P4-6-1b-2): the end of the flow
    /// the daemon holds toward the upstream, a loopback port its account
    /// alone may use, as the worker is. `None` dials the upstream itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) through: Option<SocketAddr>,
}

/// Where an endpoint's listener serves, and whom. Not [`Copy`]: `Peer` is a
/// `String` on Windows (a SID), not a `u32`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum At {
    /// A socket in the session's directory, for one uid. Unix only: there is
    /// no such place on Windows.
    #[cfg(unix)]
    Socket { peer: u32 },
    /// A loopback port, for one account.
    Loopback { peer: Peer },
}

fn auth_wire(a: Auth) -> &'static str {
    match a {
        Auth::Basic => "basic",
        Auth::Bearer => "bearer",
        Auth::ClientCert => "client_cert",
    }
}

fn auth_of(raw: &str) -> Option<Auth> {
    match raw {
        "basic" => Some(Auth::Basic),
        "bearer" => Some(Auth::Bearer),
        "client_cert" => Some(Auth::ClientCert),
        _ => None,
    }
}

impl Spec {
    /// The spec of an endpoint the daemon serves with `conf`, listening `at`.
    pub(crate) fn of(conf: &Conf, at: At) -> Self {
        Self {
            engine: conf.engine.wire().into(),
            host: conf.host.clone(),
            port: conf.port,
            auth_source: conf.auth_source.clone(),
            auth: conf.auth.map(|a| auth_wire(a).into()),
            front: conf.front.clone(),
            secret: conf.secret.clone(),
            user: conf.user.clone(),
            ca: conf.ca.clone(),
            at,
            through: match conf.dial {
                Dial::Direct => None,
                Dial::Through(end) => Some(end),
            },
        }
    }
}

impl Conf {
    /// The worker's: what `spec` names, its roots made again from the
    /// system's and the spec's CA, its upstream dialled directly or (P8-2a-6)
    /// through the flow's end the spec names.
    pub(crate) fn of_spec(spec: &Spec) -> Result<Self, String> {
        let engine = Engine::of(&spec.engine)
            .ok_or_else(|| format!("an engine this build does not know: {}", spec.engine))?;
        // A flow's end is the daemon's own loopback listener: an end
        // anywhere else is no flow's, and is refused.
        let dial = match spec.through {
            None => Dial::Direct,
            Some(end) if end.ip().is_loopback() => Dial::Through(end),
            Some(end) => return Err(format!("a flow's end off this host: {end}")),
        };
        let auth = match spec.auth.as_deref() {
            Some(raw) => Some(auth_of(raw).ok_or_else(|| format!("an unknown auth: {raw}"))?),
            None => None,
        };
        let roots = if engine == Engine::Ssh {
            Arc::new(tokio_rustls::rustls::RootCertStore::empty())
        } else {
            tls::roots(spec.ca.as_deref()).map_err(|e| format!("the upstream's TLS: {e}"))?
        };
        let tls = tls::connector_for(Arc::clone(&roots), None)
            .map_err(|e| format!("the upstream's TLS: {e}"))?;
        Ok(Self {
            host: spec.host.clone(),
            port: spec.port,
            tls,
            roots,
            ca: spec.ca.clone(),
            engine,
            auth_source: spec.auth_source.clone(),
            auth,
            front: spec.front.clone(),
            secret: spec.secret.clone(),
            user: spec.user.clone(),
            dial,
        })
    }
}

// ─── The daemon's side ──────────────────────────────────────────────────────

/// An endpoint's values as one of its connections starts: read from its
/// leases now, `None` once any is let go.
pub(crate) type Values = Arc<dyn Fn() -> Option<Vec<Zeroizing<String>>> + Send + Sync>;

/// An endpoint handed to its run's worker, as the daemon keeps it.
struct Kept {
    spec: Spec,
    /// The daemon's own copy of the listener, handed again to each worker
    /// that follows: an owned descriptor on Unix (`SCM_RIGHTS`); on Windows
    /// (P8-2b-3) a real listener of the daemon's own, duplicated to itself
    /// at `hand`-time so it outlives whatever place originally bound it.
    #[cfg(unix)]
    listener: OwnedFd,
    #[cfg(windows)]
    listener: std::net::TcpListener,
    values: Values,
}

type Runs = HashMap<(ObjectId, u64), HashMap<u64, Kept>>;

static KEPT: LazyLock<Mutex<Runs>> = LazyLock::new(Mutex::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// A new endpoint's id, from the one counter: P8-2a-4b — the egress fronts
/// take theirs from it too, so no two things a worker serves share one.
pub(crate) fn next_endpoint() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn kept() -> std::sync::MutexGuard<'static, Runs> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

/// An endpoint handed over: [`Handed::close`] ends it in the worker.
#[must_use]
pub(crate) struct Handed {
    session: ObjectId,
    fence: u64,
    endpoint: u64,
}

/// Hand an endpoint to the run's worker: `Ok(None)` where the run has none
/// (its owner left `hive_worker` off, and the daemon serves it as before),
/// an error where it has one that is not up.
pub(crate) async fn hand(
    session: ObjectId,
    fence: u64,
    spec: Spec,
    #[cfg(unix)] listener: &impl AsFd,
    #[cfg(windows)] listener: SOCKET,
    values: Values,
) -> Result<Option<Handed>, String> {
    if !crate::hive::worker::has(session, fence) {
        return Ok(None);
    }
    let Some(link) = crate::hive::worker::up(session, fence).await else {
        return Err(
            "the session's worker is not up: where this device runs workers, an endpoint is \
             served in one or not at all"
                .into(),
        );
    };
    let failed = |e: std::io::Error| format!("the endpoint's listener: {e}");
    #[cfg(unix)]
    let ours = listener.as_fd().try_clone_to_owned().map_err(failed)?;
    #[cfg(unix)]
    let theirs = ours.try_clone().map_err(failed)?;
    // FR-90 P8-2b-3 — Windows: the daemon's own copy is a real listener,
    // duplicated to itself (so its life is the map's alone); the worker's is
    // protocol info targeting its pid, fresh for this one call.
    #[cfg(windows)]
    let Some(pid) = crate::hive::worker::pid_of(session, fence) else {
        return Err("the session's worker has no pid yet".into());
    };
    #[cfg(windows)]
    let ours = crate::hive::worker::win::socket_from_info(
        &crate::hive::worker::win::duplicate_for(std::process::id(), listener).map_err(failed)?,
    )
    .map_err(failed)?;
    #[cfg(windows)]
    let info = crate::hive::worker::win::duplicate_for(pid, listener)
        .map_err(failed)?
        .to_vec();
    let endpoint = next_endpoint();
    kept().entry((session, fence)).or_default().insert(
        endpoint,
        Kept {
            spec: spec.clone(),
            listener: ours,
            values,
        },
    );
    let handed = Handed {
        session,
        fence,
        endpoint,
    };
    let call = Call::Serve {
        endpoint,
        spec: Box::new(spec),
        #[cfg(windows)]
        info,
    };
    #[cfg(unix)]
    let answer = link.call(call, vec![theirs]).await;
    #[cfg(windows)]
    let answer = link.call(call).await;
    match answer {
        Ok(Answer::Done) => Ok(Some(handed)),
        Ok(Answer::Refused { why }) => {
            handed.forget();
            Err(format!("the session's worker refused the endpoint: {why}"))
        }
        Ok(other) => {
            handed.forget();
            Err(format!("the session's worker answered {other:?}"))
        }
        // The worker went before it answered, or the call took it for lost
        // (`Link::call`), which ends its life: the one that follows is
        // handed this endpoint with the rest.
        Err(why) => {
            debug!(%session, fence, endpoint, %why, "vault: an endpoint's hand-over went unanswered: the next worker serves it");
            Ok(Some(handed))
        }
    }
}

impl Handed {
    /// The endpoint's end: forgotten, and its worker stops serving it.
    pub(crate) async fn close(self) {
        self.forget();
        if let Some(link) = crate::hive::worker::link_of(self.session, self.fence) {
            let call = Call::Close {
                endpoint: self.endpoint,
            };
            #[cfg(unix)]
            let sent = link.call(call, Vec::new()).await;
            #[cfg(windows)]
            let sent = link.call(call).await;
            if let Err(why) = sent {
                debug!(session = %self.session, fence = self.fence, endpoint = self.endpoint, %why, "vault: an endpoint's close went unanswered");
            }
        }
    }

    fn forget(&self) {
        let mut runs = kept();
        if let Some(run) = runs.get_mut(&(self.session, self.fence)) {
            run.remove(&self.endpoint);
            if run.is_empty() {
                runs.remove(&(self.session, self.fence));
            }
        }
    }
}

/// Every endpoint handed to the run's worker so far, handed to `link`'s:
/// the worker that follows serves what the lost one did.
#[cfg(unix)]
pub(crate) async fn rehand(session: ObjectId, fence: u64, link: &Link) {
    let all: Vec<(u64, Spec, std::io::Result<OwnedFd>)> = kept()
        .get(&(session, fence))
        .map(|run| {
            run.iter()
                .map(|(id, k)| (*id, k.spec.clone(), k.listener.try_clone()))
                .collect()
        })
        .unwrap_or_default();
    for (endpoint, spec, listener) in all {
        let answer = match listener {
            Ok(fd) => {
                link.call(
                    Call::Serve {
                        endpoint,
                        spec: Box::new(spec),
                    },
                    vec![fd],
                )
                .await
            }
            Err(e) => Err(e.to_string()),
        };
        match answer {
            Ok(Answer::Done) => {}
            other => {
                warn!(%session, fence, endpoint, answer = ?other, "vault: an endpoint was not handed to the session's new worker");
            }
        }
    }
}

/// FR-90 P8-2b-3 — [`rehand`] on Windows: no second channel, so each
/// endpoint's listener is duplicated afresh, targeting the new worker's pid
/// (protocol info is single-use and per-target-pid: the worker that follows
/// is a new pid, so nothing from the last hand-over can be reused).
#[cfg(windows)]
pub(crate) async fn rehand(session: ObjectId, fence: u64, link: &Link) {
    let Some(pid) = crate::hive::worker::pid_of(session, fence) else {
        warn!(%session, fence, "vault: the new worker's pid is not known yet: its endpoints wait for the next rehand");
        return;
    };
    let all: Vec<(u64, Spec, SOCKET)> = kept()
        .get(&(session, fence))
        .map(|run| {
            run.iter()
                .map(|(id, k)| (*id, k.spec.clone(), k.listener.as_raw_socket() as SOCKET))
                .collect()
        })
        .unwrap_or_default();
    for (endpoint, spec, listener) in all {
        let answer = match crate::hive::worker::win::duplicate_for(pid, listener) {
            Ok(info) => {
                link.call(Call::Serve {
                    endpoint,
                    spec: Box::new(spec),
                    info: info.to_vec(),
                })
                .await
            }
            Err(e) => Err(e.to_string()),
        };
        match answer {
            Ok(Answer::Done) => {}
            other => {
                warn!(%session, fence, endpoint, answer = ?other, "vault: an endpoint was not handed to the session's new worker");
            }
        }
    }
}

/// A test's: how many endpoints the run has handed to its worker now. Unix
/// only: its only callers drive the Unix hand-over harness.
#[cfg(all(test, unix))]
pub(crate) fn kept_for(session: ObjectId, fence: u64) -> usize {
    kept().get(&(session, fence)).map_or(0, HashMap::len)
}

/// The daemon's answer to a worker's `values {endpoint}`.
pub(crate) fn values(session: ObjectId, fence: u64, endpoint: u64) -> Answer {
    let reader = kept()
        .get(&(session, fence))
        .and_then(|run| run.get(&endpoint))
        .map(|k| Arc::clone(&k.values));
    match reader {
        None => Answer::Refused {
            why: "no such endpoint".into(),
        },
        Some(read) => Answer::Values {
            values: read().map(|v| v.into_iter().map(Secret).collect()),
        },
    }
}

// ─── The worker's side ──────────────────────────────────────────────────────

/// The endpoints one worker serves, each one's stop a child of `all`: the
/// worker's loop holds a guard on `all`, so every endpoint stops as the loop
/// ends however it ends, as the end of its process would stop them.
#[derive(Default)]
pub(crate) struct Serving {
    endpoints: Mutex<Endpoints>,
    pub(crate) all: CancellationToken,
}

/// Each endpoint a worker serves now, by its stop, and each one it closed.
#[derive(Default)]
struct Endpoints {
    stops: HashMap<u64, CancellationToken>,
    closed: HashSet<u64>,
}

impl Serving {
    fn endpoints(&self) -> std::sync::MutexGuard<'_, Endpoints> {
        self.endpoints.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The daemon's `serve`, its listener received just now: serve the endpoint
/// until its `close`. One handed again (a hand-over that crossed a new
/// worker's) is served once, the listener that came with it dropped; one
/// whose close came first (a hand-over that crossed it) is not served.
pub(crate) fn serve_handed(
    serving: &Arc<Serving>,
    link: Arc<Link>,
    endpoint: u64,
    spec: &Spec,
    #[cfg(unix)] listener: OwnedFd,
    #[cfg(windows)] listener: Vec<u8>,
) -> Answer {
    let refused = |why: String| Answer::Refused { why };
    let conf = match Conf::of_spec(spec) {
        Ok(conf) => conf,
        Err(why) => return refused(why),
    };
    let asked = Asked {
        conf,
        link,
        endpoint,
    };
    match &spec.at {
        #[cfg(unix)]
        At::Socket { peer } => {
            let std = std::os::unix::net::UnixListener::from(listener);
            match std
                .set_nonblocking(true)
                .and_then(|()| UnixListener::from_std(std))
            {
                Ok(listener) => claim(
                    serving,
                    endpoint,
                    Socket {
                        listener,
                        peer: *peer,
                    },
                    asked,
                ),
                Err(e) => refused(format!("the endpoint's socket: {e}")),
            }
        }
        At::Loopback { peer } => {
            #[cfg(unix)]
            let std = std::net::TcpListener::from(listener);
            // FR-90 P8-2b-3 — Windows: no fd-4 channel, so the listener
            // rides in `listener`'s own protocol info instead.
            #[cfg(windows)]
            let std = match crate::hive::worker::win::socket_from_info(&listener) {
                Ok(l) => l,
                Err(e) => return refused(format!("the endpoint's listener: {e}")),
            };
            let local = match std.local_addr() {
                Ok(SocketAddr::V4(a)) => a,
                Ok(other) => return refused(format!("a listener not on IPv4 loopback: {other}")),
                Err(e) => return refused(format!("the endpoint's port: {e}")),
            };
            match std
                .set_nonblocking(true)
                .and_then(|()| TcpListener::from_std(std))
            {
                Ok(listener) => {
                    // `Peer` is `u32` (Copy) on Unix, `String` on Windows.
                    #[cfg(unix)]
                    let peer = *peer;
                    #[cfg(windows)]
                    let peer = peer.clone();
                    let place = Loopback {
                        listener,
                        local,
                        peer,
                    };
                    claim(serving, endpoint, place, asked)
                }
                Err(e) => refused(format!("the endpoint's port: {e}")),
            }
        }
    }
}

/// Serve `place` as `endpoint`, unless this worker serves it already or has
/// closed it: decided in one look, since two hand-overs of one endpoint are
/// answered at once, each on a task of its own. A place not served drops
/// here, and its copy of the listener with it.
fn claim<P: Place>(serving: &Arc<Serving>, endpoint: u64, place: P, asked: Asked) -> Answer {
    let Some(stop) = claimed(serving, endpoint) else {
        return Answer::Done;
    };
    let live = Live::new(&stop);
    let end = {
        let serving = Arc::clone(serving);
        move || {
            serving.endpoints().stops.remove(&endpoint);
        }
    };
    tokio::spawn(serve(place, Arc::new(asked), stop, live, end));
    Answer::Done
}

/// `endpoint` claimed for this worker, in one look: its stop, a child of
/// [`Serving::all`]. `None` where this worker serves it already (a
/// hand-over that crossed a new worker's) or has closed it (one that
/// crossed its close). P8-2a-4b — the egress fronts are claimed so too.
pub(crate) fn claimed(serving: &Serving, endpoint: u64) -> Option<CancellationToken> {
    let stop = serving.all.child_token();
    let mut endpoints = serving.endpoints();
    if endpoints.closed.contains(&endpoint) || endpoints.stops.contains_key(&endpoint) {
        return None;
    }
    endpoints.stops.insert(endpoint, stop.clone());
    Some(stop)
}

/// The daemon's `close`: the endpoint stops, its connections cut, and a
/// hand-over of it still on its way is not served.
pub(crate) fn close_handed(serving: &Serving, endpoint: u64) -> Answer {
    let stop = {
        let mut endpoints = serving.endpoints();
        endpoints.closed.insert(endpoint);
        endpoints.stops.remove(&endpoint)
    };
    if let Some(stop) = stop {
        stop.cancel();
    }
    Answer::Done
}

/// A handed endpoint's connections, each one's values asked of the daemon
/// as it starts.
struct Asked {
    conf: Conf,
    link: Arc<Link>,
    endpoint: u64,
}

impl Sessions for Asked {
    fn engine(&self) -> Engine {
        self.conf.engine
    }

    async fn session(&self) -> Option<Session> {
        let call = Call::Values {
            endpoint: self.endpoint,
        };
        #[cfg(unix)]
        let answer = self.link.call(call, Vec::new()).await;
        #[cfg(windows)]
        let answer = self.link.call(call).await;
        match answer {
            Ok(Answer::Values {
                values: Some(values),
            }) => self.conf.session(|i| values.get(i).map(|v| v.0.clone())),
            _ => None,
        }
    }
}

/// A handed socket: the daemon made it, and removes it. Unix only: there is
/// no such place on Windows.
#[cfg(unix)]
struct Socket {
    listener: UnixListener,
    peer: u32,
}

#[cfg(unix)]
impl Place for Socket {
    type Stream = UnixStream;

    async fn accept(&self) -> std::io::Result<UnixStream> {
        self.listener.accept().await.map(|(stream, _)| stream)
    }

    fn admits(&self, stream: &UnixStream) -> Result<(), String> {
        match stream.peer_cred() {
            Ok(c) if c.uid() == self.peer => Ok(()),
            Ok(c) => Err(format!("uid {}", c.uid())),
            Err(e) => Err(format!("a peer that could not be identified: {e}")),
        }
    }

    /// The daemon's copy stays, and so does the socket's directory.
    fn close(self) {}
}

/// A handed loopback port.
struct Loopback {
    listener: TcpListener,
    local: SocketAddrV4,
    peer: Peer,
}

impl Place for Loopback {
    type Stream = TcpStream;

    async fn accept(&self) -> std::io::Result<TcpStream> {
        self.listener.accept().await.map(|(stream, _)| stream)
    }

    fn admits(&self, stream: &TcpStream) -> Result<(), String> {
        let SocketAddr::V4(client) = stream.peer_addr().map_err(|e| e.to_string())? else {
            return Err("a client that is not IPv4 loopback".into());
        };
        super::endpoint::loopback::connection_is(&self.peer, client, self.local)
    }

    fn close(self) {}
}

/// Unix: a real end-to-end harness, descriptors included. Windows's own
/// (P8-2b-3) is a real-worker-process test in `worker::win::tests`, beside
/// its siblings: a different wire (no second channel) wants a different
/// harness, not this one retrofitted.
#[cfg(all(test, unix))]
mod tests;
