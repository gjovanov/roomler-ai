// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-4b — a session's egress proxy, served by its worker.
//!
//! Where the device's owner turned workers on (`hive_worker`), the run's two
//! fronts are served in the session's worker: what reads a client's request,
//! terminates TLS for a placeholder's audience, swaps, signs and relays. A
//! fault in any of it costs the worker, never the daemon, which keeps all
//! that holds or decides.
//!
//! | Piece | As built |
//! |---|---|
//! | what stays the daemon's | the two listeners, bound as the run starts at the ports its settings name; the vault's decisions, kept for the run; the run's placeholders and their leases; its CA and the CA's key |
//! | what the worker is handed | the fronts' listeners (`SCM_RIGHTS`, the HTTP front's first) and a [`Spec`]: the run, and the one account its fronts admit. None of it secret |
//! | a connection | accepted, admitted and carried by the worker, which asks the daemon `decide` for a host, `bound` for whether to terminate it, `valued` for a request with an `Authorization`, and `leaf` for a host's certificate |
//! | a leaf | made by the daemon's CA, only for an audience of a placeholder the run holds: the session's tools trust the CA, and it signs for whatever host it is asked |
//! | the dial | the worker's own, from this device, under the same rule as the daemon's ([`super::dialable`]) |
//! | a new worker | handed the fronts again, on the same listeners: a connection the lost one held is gone, the ports are not. A hand-over that crossed a new worker's is served once |
//! | no worker up | nothing serves the fronts: a connection waits in its backlog, and a run whose worker is given up on ends |

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bson::oid::ObjectId;
use roomler_ai_remote_control::vault::canonical_net_host;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tracing::{debug, warn};
use zeroize::Zeroizing;

use super::{
    Bindings, Decide, Decision, Front, Leaves, Peer, Roots, RunKey, RunState, Seams, SystemDialer,
    Valued, accept, ca, runs,
};
use crate::hive::placeholder::{self, Bound};
use crate::hive::vault_connect::handed::{Answer, Call, Secret, Serving, claimed, next_endpoint};
use crate::hive::worker::link::Link;

/// What a worker is handed to serve the run's fronts with: none of it secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Spec {
    /// The run: its session, in hex, and its fence.
    pub(crate) session: String,
    pub(crate) fence: u64,
    /// The one account the fronts admit.
    pub(crate) peer: u32,
}

/// A leaf as it crosses the channel, its key a [`Secret`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Leaf {
    /// Each certificate's DER, in base64: the leaf's, then the CA's.
    chain: Vec<String>,
    /// The leaf's PKCS#8 key, in base64.
    key: Secret,
    /// When it stops being valid, in seconds since the epoch.
    not_after: u64,
}

impl std::fmt::Debug for Leaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leaf")
            .field("chain", &self.chain.len())
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

impl Leaf {
    fn of(leaf: &ca::Leaf) -> Self {
        Self {
            chain: leaf
                .chain
                .iter()
                .map(|c| STANDARD.encode(c.as_ref()))
                .collect(),
            key: Secret(Zeroizing::new(STANDARD.encode(leaf.key.secret_pkcs8_der()))),
            not_after: leaf
                .not_after
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    fn leaf(&self) -> Result<ca::Leaf, String> {
        let chain = self
            .chain
            .iter()
            .map(|c| STANDARD.decode(c).map(CertificateDer::from))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("a leaf's chain: {e}"))?;
        let key = Zeroizing::new(
            STANDARD
                .decode(self.key.0.as_bytes())
                .map_err(|e| format!("a leaf's key: {e}"))?,
        );
        Ok(ca::Leaf {
            chain,
            key: PrivatePkcs8KeyDer::from(key.to_vec()),
            not_after: UNIX_EPOCH + Duration::from_secs(self.not_after),
        })
    }
}

// ─── The daemon's side ──────────────────────────────────────────────────────

/// The run's fronts as the daemon keeps them for its worker.
pub(super) struct Kept {
    /// What the worker serves them as.
    endpoint: u64,
    http: OwnedFd,
    socks: OwnedFd,
    peer: Peer,
}

impl Kept {
    pub(super) fn new(http: OwnedFd, socks: OwnedFd, peer: Peer) -> Self {
        Self {
            endpoint: next_endpoint(),
            http,
            socks,
            peer,
        }
    }
}

/// The run's fronts handed to `link`'s worker: each worker the run has
/// serves them, on the same listeners.
pub(crate) async fn rehand(session: ObjectId, fence: u64, link: &Link) {
    let kept = runs().get(&(session, fence)).and_then(|state| {
        let kept = state.handed.as_ref()?;
        Some((
            kept.endpoint,
            kept.peer,
            kept.http.try_clone(),
            kept.socks.try_clone(),
        ))
    });
    let Some((endpoint, peer, http, socks)) = kept else {
        return;
    };
    let (http, socks) = match (http, socks) {
        (Ok(http), Ok(socks)) => (http, socks),
        (Err(e), _) | (_, Err(e)) => {
            warn!(%session, fence, %e, "hive: the session's egress fronts could not be handed to its worker");
            return;
        }
    };
    let spec = Spec {
        session: session.to_hex(),
        fence,
        peer,
    };
    let call = Call::Egress {
        endpoint,
        spec: Box::new(spec),
    };
    match link.call(call, vec![http, socks]).await {
        Ok(Answer::Done) => {
            debug!(%session, fence, "hive: the session's egress fronts handed to its worker");
        }
        other => {
            warn!(%session, fence, answer = ?other, "hive: the session's egress fronts were not handed to its worker");
        }
    }
}

/// The run's fronts gone: the worker serving them, where one is up, is told
/// to stop, as its endpoints are told. Its answer is not waited for: a
/// connection it still takes is decided by nobody (`stopped`).
pub(super) fn closed(session: ObjectId, fence: u64, kept: &Kept) {
    let Some(link) = crate::hive::worker::link_of(session, fence) else {
        return;
    };
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let endpoint = kept.endpoint;
    runtime.spawn(async move {
        if let Err(why) = link.call(Call::Close { endpoint }, Vec::new()).await {
            debug!(%session, fence, %why, "hive: the egress fronts' close went unanswered");
        }
    });
}

/// The run's state and the daemon's seams, where the run's worker serves its
/// fronts.
fn served(run: RunKey) -> Option<(Arc<RunState>, Seams)> {
    let state = runs().get(&run).cloned()?;
    let seams = state.seams.clone()?;
    Some((state, seams))
}

fn stopped() -> Answer {
    Answer::Refused {
        why: "the session's egress proxy has stopped".into(),
    }
}

/// The daemon's answer to its worker's `decide`: the run's decision, kept
/// for the run, the vault asked once however many connections or workers
/// ask.
pub(crate) async fn decide(run: RunKey, host: &str, port: u16) -> Answer {
    let Some((state, seams)) = served(run) else {
        return stopped();
    };
    // A decision is kept by the host's one spelling: one the worker spelled
    // otherwise is asked about nowhere.
    if canonical_net_host(host).as_deref() != Some(host) {
        return Answer::Refused {
            why: format!("{host:?} is not a host in its one spelling"),
        };
    }
    Answer::Decision {
        decision: super::decided(&state, run, host, port, &*seams.decide).await,
    }
}

/// The daemon's answer to its worker's `bound`.
pub(crate) async fn bound(run: RunKey, host: &str, port: u16) -> Answer {
    let Some((_, seams)) = served(run) else {
        return stopped();
    };
    Answer::Bound {
        bound: seams.bindings.bound(run, host, port).await,
    }
}

/// The daemon's answer to its worker's `valued`: each value read from its
/// lease now, in base64, since a value is bytes.
pub(crate) async fn valued(run: RunKey, host: &str, port: u16) -> Answer {
    let Some((_, seams)) = served(run) else {
        return stopped();
    };
    let valued = seams
        .bindings
        .valued(run, host, port)
        .await
        .into_iter()
        .map(|(b, v)| (b, v.map(|v| Secret(Zeroizing::new(STANDARD.encode(&*v))))))
        .collect();
    Answer::Valued { valued }
}

/// The daemon's answer to its worker's `leaf`: made by the run's CA, and
/// only for an audience of a placeholder the run holds. The CA signs a leaf
/// for whatever host it is asked, and the session's tools trust it, so one
/// for any other host would let whoever holds it read that host's traffic.
pub(crate) async fn leaf(run: RunKey, host: &str) -> Answer {
    let Some((_, seams)) = served(run) else {
        return stopped();
    };
    if !placeholder::is_audience(run.0, run.1, host) {
        return Answer::Refused {
            why: format!("{host} is the audience of no placeholder this session holds"),
        };
    }
    match seams.leaves.leaf(run, host).await {
        Ok(leaf) => Answer::Leaf {
            leaf: Leaf::of(&leaf),
        },
        Err(why) => Answer::Refused { why },
    }
}

// ─── The worker's side ──────────────────────────────────────────────────────

/// The daemon's `egress`, the fronts' listeners received just now: serve them
/// until the worker ends. Handed again (a hand-over that crossed a new
/// worker's), they are served once.
pub(crate) fn serve_handed(
    serving: &Arc<Serving>,
    link: Arc<Link>,
    endpoint: u64,
    spec: &Spec,
    http: OwnedFd,
    socks: OwnedFd,
) -> Answer {
    let refused = |why: String| Answer::Refused { why };
    let Ok(session) = ObjectId::parse_str(&spec.session) else {
        return refused(format!("not a session: {:?}", spec.session));
    };
    let run = (session, spec.fence);
    let admit = match crate::hive::vault_connect::session_admit(spec.peer) {
        Ok(admit) => admit,
        Err(why) => return refused(why),
    };
    let tokio_of = |fd: OwnedFd| {
        let std = std::net::TcpListener::from(fd);
        std.set_nonblocking(true)
            .and_then(|()| TcpListener::from_std(std))
    };
    let (http, socks) = match (tokio_of(http), tokio_of(socks)) {
        (Ok(http), Ok(socks)) => (http, socks),
        (Err(e), _) | (_, Err(e)) => return refused(format!("the egress fronts' listeners: {e}")),
    };
    let Some(stop) = claimed(serving, endpoint) else {
        return Answer::Done;
    };
    let state = Arc::new(RunState {
        stop,
        ..RunState::new(None)
    });
    let asked = Arc::new(Asked(link));
    let seams = Seams {
        decide: asked.clone(),
        dial: Arc::new(SystemDialer),
        bindings: asked.clone(),
        leaves: asked,
        roots: Roots::System,
    };
    #[cfg(test)]
    let seams = tests::seams_for(run, seams);
    for (listener, front) in [(http, Front::Http), (socks, Front::Socks)] {
        tokio::spawn(accept(
            listener,
            front,
            run,
            state.clone(),
            admit.clone(),
            seams.clone(),
        ));
    }
    Answer::Done
}

/// The worker's seams: each a call to the daemon.
struct Asked(Arc<Link>);

#[async_trait::async_trait]
impl Decide for Asked {
    async fn decide(&self, _run: RunKey, host: &str, port: u16) -> Decision {
        let call = Call::Decide {
            host: host.to_string(),
            port,
        };
        match self.0.call(call, Vec::new()).await {
            Ok(Answer::Decision { decision }) => decision,
            Ok(other) => Decision::Undecided(format!("the daemon answered {other:?}")),
            Err(why) => Decision::Undecided(why),
        }
    }
}

#[async_trait::async_trait]
impl Bindings for Asked {
    /// None where the daemon does not answer: the host is then carried as it
    /// comes, never decrypted, and nothing is swapped.
    async fn bound(&self, _run: RunKey, host: &str, port: u16) -> Vec<Bound> {
        let call = Call::Bound {
            host: host.to_string(),
            port,
        };
        match self.0.call(call, Vec::new()).await {
            Ok(Answer::Bound { bound }) => bound,
            _ => Vec::new(),
        }
    }

    /// None where the daemon does not answer: nothing is swapped, and a
    /// placeholder goes out as itself.
    async fn valued(&self, _run: RunKey, host: &str, port: u16) -> Vec<Valued> {
        let call = Call::Valued {
            host: host.to_string(),
            port,
        };
        match self.0.call(call, Vec::new()).await {
            Ok(Answer::Valued { valued }) => valued
                .into_iter()
                .map(|(b, v)| {
                    let v =
                        v.and_then(|s| STANDARD.decode(s.0.as_bytes()).ok().map(Zeroizing::new));
                    (b, v)
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl Leaves for Asked {
    async fn leaf(&self, _run: RunKey, host: &str) -> Result<Arc<ca::Leaf>, String> {
        let call = Call::Leaf {
            host: host.to_string(),
        };
        match self.0.call(call, Vec::new()).await {
            Ok(Answer::Leaf { leaf }) => leaf.leaf().map(Arc::new),
            Ok(Answer::Refused { why }) => Err(why),
            Ok(other) => Err(format!("the daemon answered {other:?}")),
            Err(why) => Err(why),
        }
    }
}

#[cfg(test)]
mod tests;
