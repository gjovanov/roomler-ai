// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-2 — a session's credential endpoints, served by its worker.
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
//!
//! ⚠️ A through-device dial (P4-6-1b-2) stays the daemon's: the flow's end
//! admits the daemon's own process alone (P8-0's point 6 comes later).

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, SocketAddrV4};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use bson::oid::ObjectId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use zeroize::Zeroizing;

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

/// A call across a worker's channel: an endpoint's, the only calls so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "c", rename_all = "snake_case")]
pub(crate) enum Call {
    /// The daemon: serve `endpoint` as `spec` says, on the listener sent
    /// just before this frame.
    Serve { endpoint: u64, spec: Box<Spec> },
    /// The daemon: stop serving `endpoint`, its connections cut.
    Close { endpoint: u64 },
    /// The worker: `endpoint`'s values, read from its leases now, for one
    /// connection.
    Values { endpoint: u64 },
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
}

/// Where an endpoint's listener serves, and whom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum At {
    /// A socket in the session's directory, for one uid.
    Socket { peer: u32 },
    /// A loopback port, for one uid.
    Loopback { peer: u32 },
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
        }
    }
}

impl Conf {
    /// The worker's: what `spec` names, its roots made again from the
    /// system's and the spec's CA, its upstream dialled directly.
    pub(crate) fn of_spec(spec: &Spec) -> Result<Self, String> {
        let engine = Engine::of(&spec.engine)
            .ok_or_else(|| format!("an engine this build does not know: {}", spec.engine))?;
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
            dial: Dial::Direct,
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
    /// that follows.
    listener: OwnedFd,
    values: Values,
}

type Runs = HashMap<(ObjectId, u64), HashMap<u64, Kept>>;

static KEPT: LazyLock<Mutex<Runs>> = LazyLock::new(Mutex::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

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
    listener: &impl AsFd,
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
    let ours = listener.as_fd().try_clone_to_owned().map_err(failed)?;
    let theirs = ours.try_clone().map_err(failed)?;
    let endpoint = NEXT.fetch_add(1, Ordering::Relaxed);
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
    match link
        .call(
            Call::Serve {
                endpoint,
                spec: Box::new(spec),
            },
            Some(theirs),
        )
        .await
    {
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
            if let Err(why) = link.call(call, None).await {
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
                    Some(fd),
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

/// A test's: how many endpoints the run has handed to its worker now.
#[cfg(test)]
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
    listener: OwnedFd,
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
    match spec.at {
        At::Socket { peer } => {
            let std = std::os::unix::net::UnixListener::from(listener);
            match std
                .set_nonblocking(true)
                .and_then(|()| UnixListener::from_std(std))
            {
                Ok(listener) => claim(serving, endpoint, Socket { listener, peer }, asked),
                Err(e) => refused(format!("the endpoint's socket: {e}")),
            }
        }
        At::Loopback { peer } => {
            let std = std::net::TcpListener::from(listener);
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
    let stop = serving.all.child_token();
    {
        let mut endpoints = serving.endpoints();
        if endpoints.closed.contains(&endpoint) || endpoints.stops.contains_key(&endpoint) {
            return Answer::Done;
        }
        endpoints.stops.insert(endpoint, stop.clone());
    }
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
        match self.link.call(call, None).await {
            Ok(Answer::Values {
                values: Some(values),
            }) => self.conf.session(|i| values.get(i).map(|v| v.0.clone())),
            _ => None,
        }
    }
}

/// A handed socket: the daemon made it, and removes it.
struct Socket {
    listener: UnixListener,
    peer: u32,
}

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
    peer: u32,
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

#[cfg(test)]
mod tests;
