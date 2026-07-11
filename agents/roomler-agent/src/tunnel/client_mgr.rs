//! Daemon-originated tunnel-**client** flows (unification P3b-2 PR-C).
//!
//! Lets the roomler daemon act as a tunnel *client* — `roomler forward` /
//! `roomler socks5` over the LocalAPI — by driving
//! [`tunnel_core::driver::run_tunnel_session`] over the daemon's **existing
//! agent WebSocket** (agent JWT, `Principal::Agent` server-side) instead of a
//! second `TunnelClient` identity + second WS. The server half (accepting an
//! agent-WS as a tunnel originator) shipped in PR-B2; this is the consumer.
//!
//! ## Multiplexing N flows over one WS
//!
//! The daemon runs many client sessions over its ONE agent WS, so it needs to
//! demux the server's replies. Two seams make that work:
//!
//! * **egress** — every session's [`DaemonSink`] funnels its `ClientMsg`s onto
//!   the SAME `outbound_tx` the signaling loop drains onto the WS. The sink
//!   stamps this session's **`open_nonce`** onto the `TunnelOpen` (the driver
//!   hardcodes `None` — a single-session CLI matches the reply positionally; we
//!   can't).
//! * **ingress** — [`intercept_server_msg`] (called from the signaling loop's
//!   read arm, mirroring `overlay::intercept`) routes each client-bound
//!   `ServerMsg` into its session's per-session [`ChannelSource`]: pre-`opened`
//!   by `open_nonce`, post-`opened` by `session_id`. Everything else passes
//!   through to the target-side `handle_server_msg`.
//!
//! The demux maps + the flow registry live in [`TunnelClientHub`], created once
//! in `run_cmd` and shared (it's `Clone` over an `Arc`) between the signaling
//! loop (publish the live sink + intercept) and `DaemonState` (the LocalAPI
//! create/kill/flows verbs) — so flows survive WS reconnects.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};
use tunnel_core::driver::{
    SessionOutcome, SessionParams, Target, TransportPref, run_tunnel_session,
};
use tunnel_core::localapi::{FlowInfo, FlowKind};
use tunnel_core::signaling_link::{TunnelSignalingSink, TunnelSignalingSource};
use tunnel_core::transport::TRANSPORT_WEBRTC_DC_V1;

/// Per-session control-channel buffer. Sized to absorb an ICE-trickle burst at
/// session open without blocking the shared WS-read loop — control-plane only
/// (SDP / ICE / per-flow accept / close), never the byte pumps.
const SESSION_SOURCE_DEPTH: usize = 256;

/// Reconnect backoff bounds for a supervised flow, mirroring the CLI's
/// `run_forward`: near-instant re-open after a session that ran then dropped;
/// capped so a persistently-unreachable target isn't hammered.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The hub — shared demux + flow registry
// ---------------------------------------------------------------------------

/// Shared tunnel-client state: the reply demux + the supervised-flow registry +
/// the live agent-WS egress handle. Cheap to clone (an `Arc` inside).
#[derive(Clone)]
pub struct TunnelClientHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    /// The live agent-WS egress (`ClientMsg` ↑), or `None` while the WS is down.
    /// Published by the signaling loop on each (re)connect; flow supervisors
    /// wait for `Some` before opening a session.
    sink_tx: watch::Sender<Option<mpsc::Sender<ClientMsg>>>,
    /// Reply demux, pre-`opened`: `open_nonce` → the session's Source sender.
    pending_opens: Mutex<HashMap<String, mpsc::Sender<ServerMsg>>>,
    /// Reply demux, post-`opened`: `session_id` → the session's Source sender.
    client_sessions: Mutex<HashMap<ObjectId, mpsc::Sender<ServerMsg>>>,
    /// Flow registry: flow id → supervisor handle + display cells.
    flows: Mutex<HashMap<String, FlowHandle>>,
    /// This daemon's version, advertised in `rc:tunnel.hello`.
    client_version: String,
    /// Monotonic source for flow ids + open nonces (unique, deterministic — no
    /// RNG needed: a flow id is unique, and `nonce = <flow-id>.<attempt>`).
    seq: AtomicU64,
}

/// A registered flow: the supervisor's abort handle + the immutable display
/// fields + the shared live cells (`flows()` reads them; the supervisor +
/// Source write them).
struct FlowHandle {
    abort: AbortHandle,
    kind: FlowKind,
    local: u16,
    /// `host:port` for a static forward; `None` for a SOCKS5 listener.
    target: Option<String>,
    /// Target node — the hex agent id being reached.
    node: String,
    /// Requested transport word (`auto` / `quic` / `webrtc`) — shown until a
    /// session negotiates a concrete one.
    requested: String,
    live: Arc<FlowLive>,
}

/// Live per-flow cells, shared between the supervisor (writes `status` +
/// `nonce`), the per-session Source (writes `transport` + `session_id` when it
/// sees `TunnelOpened`), and `flows()` (reads). `kill_flow` reads `nonce` +
/// `session_id` to reap the demux maps when it aborts the supervisor mid-flight.
#[derive(Default)]
struct FlowLive {
    status: Mutex<FlowStatus>,
    /// Negotiated transport, learned from the pass-through `TunnelOpened`.
    transport: Mutex<Option<String>>,
    /// Current session id, once opened.
    session_id: Mutex<Option<ObjectId>>,
    /// Current in-flight open nonce.
    nonce: Mutex<Option<String>>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum FlowStatus {
    /// Waiting for a live WS / mid-handshake.
    #[default]
    Connecting,
    /// A session is established (the listener is serving).
    Up,
    /// The session dropped; the supervisor is backing off before a retry.
    Down,
}

fn status_word(s: FlowStatus) -> &'static str {
    match s {
        FlowStatus::Connecting => "connecting",
        FlowStatus::Up => "up",
        FlowStatus::Down => "down",
    }
}

impl TunnelClientHub {
    /// Build an idle hub. The sink starts `None`; the signaling loop publishes
    /// it on connect via [`TunnelClientHub::publish_sink`].
    pub fn new(client_version: String) -> Self {
        let (sink_tx, _sink_rx) = watch::channel(None);
        Self {
            inner: Arc::new(HubInner {
                sink_tx,
                pending_opens: Mutex::new(HashMap::new()),
                client_sessions: Mutex::new(HashMap::new()),
                flows: Mutex::new(HashMap::new()),
                client_version,
                seq: AtomicU64::new(1),
            }),
        }
    }

    /// Publish the live agent-WS egress so flow supervisors can open sessions.
    /// Returns a guard that clears it back to `None` on drop — so a supervisor
    /// that holds a clone of the dead `outbound_tx` fails its next send and
    /// re-waits for the next connection's sink (mirrors `ConnectedGuard`).
    ///
    /// Uses `send_replace`, NOT `send`: at publish time there may be no live
    /// receiver (a flow supervisor subscribes only when its flow is created,
    /// which can be AFTER the first WS connect), and `watch::Sender::send`
    /// silently fails + drops the value when there are no receivers — the value
    /// would stay `None` and every later-subscribing supervisor would hang.
    /// `send_replace` always updates the stored value, so a supervisor that
    /// subscribes afterward sees the live sink.
    pub fn publish_sink(&self, tx: mpsc::Sender<ClientMsg>) -> SinkGuard {
        self.inner.sink_tx.send_replace(Some(tx));
        SinkGuard { hub: self.clone() }
    }

    /// Snapshot the registry as LocalAPI [`FlowInfo`]. Live throughput
    /// (`bytes_in`/`bytes_out`/`active_flows`) is `0` for now — surfacing the
    /// driver's per-flow `FlowStats` needs a session-stats handle threaded
    /// through `run_tunnel_session`; deferred to P3b-3 alongside peer
    /// rtt/last_seen. The `transport` column doubles as a liveness signal:
    /// `connecting` / `down` until a session negotiates a concrete transport.
    pub fn flows_snapshot(&self) -> Vec<FlowInfo> {
        let flows = self.inner.flows.lock().unwrap();
        let mut out: Vec<FlowInfo> = flows
            .iter()
            .map(|(id, h)| {
                let status = *h.live.status.lock().unwrap();
                let transport = if status == FlowStatus::Up {
                    h.live
                        .transport
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| h.requested.clone())
                } else {
                    status_word(status).to_string()
                };
                FlowInfo {
                    id: id.clone(),
                    kind: h.kind,
                    local_addr: format!("127.0.0.1:{}", h.local),
                    target: h.target.clone(),
                    node: Some(h.node.clone()),
                    transport,
                    active_flows: 0,
                    bytes_in: 0,
                    bytes_out: 0,
                }
            })
            .collect();
        // Stable order for a readable table + deterministic tests.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Create a supervised static forward. Validates the node id + remote +
    /// that `local` is bindable, then spawns the supervisor and returns the
    /// assigned flow id. `Err` is a user-facing message for the LocalAPI.
    pub async fn create_forward(
        &self,
        node: &str,
        local: u16,
        remote: &str,
        transport: &str,
    ) -> std::result::Result<String, String> {
        let agent_id = parse_node(node)?;
        let (host, port) = parse_host_port(remote).map_err(|e| e.to_string())?;
        let pref = parse_transport(transport);
        probe_local_port(local).await?;
        let id = self.spawn_flow(
            FlowKind::Forward,
            agent_id,
            node.to_string(),
            local,
            Some(remote.to_string()),
            Target::Static { host, port },
            pref,
        );
        info!(flow = %id, %node, local, %remote, ?pref, "created daemon forward");
        Ok(id)
    }

    /// Create a supervised SOCKS5 listener (userspace mode; per-connection
    /// CONNECT target). Same validation as [`create_forward`] minus the remote.
    pub async fn create_socks5(
        &self,
        node: &str,
        local: u16,
        transport: &str,
    ) -> std::result::Result<String, String> {
        let agent_id = parse_node(node)?;
        let pref = parse_transport(transport);
        probe_local_port(local).await?;
        let id = self.spawn_flow(
            FlowKind::Socks5,
            agent_id,
            node.to_string(),
            local,
            None,
            Target::Socks5,
            pref,
        );
        info!(flow = %id, %node, local, ?pref, "created daemon socks5 listener");
        Ok(id)
    }

    /// Abort + deregister a flow by id. Reaps the flow's demux entries (in case
    /// it was aborted mid-open / mid-session, where the supervisor's own
    /// cleanup won't run). Returns whether a flow was found.
    pub fn kill_flow(&self, id: &str) -> bool {
        let Some(handle) = self.inner.flows.lock().unwrap().remove(id) else {
            return false;
        };
        handle.abort.abort();
        if let Some(nonce) = handle.live.nonce.lock().unwrap().take() {
            self.inner.pending_opens.lock().unwrap().remove(&nonce);
        }
        if let Some(sid) = handle.live.session_id.lock().unwrap().take() {
            self.inner.client_sessions.lock().unwrap().remove(&sid);
        }
        info!(flow = %id, "killed daemon flow");
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_flow(
        &self,
        kind: FlowKind,
        agent_id: ObjectId,
        node: String,
        local: u16,
        target_disp: Option<String>,
        target: Target,
        pref: TransportPref,
    ) -> String {
        let id = format!("fl-{}", self.inner.seq.fetch_add(1, Ordering::Relaxed));
        let live = Arc::new(FlowLive::default());
        let handle = tokio::spawn(run_flow_supervisor(
            self.clone(),
            id.clone(),
            agent_id,
            local,
            target,
            pref,
            live.clone(),
        ));
        self.inner.flows.lock().unwrap().insert(
            id.clone(),
            FlowHandle {
                abort: handle.abort_handle(),
                kind,
                local,
                target: target_disp,
                node,
                requested: transport_word(pref).to_string(),
                live,
            },
        );
        id
    }

    // ---- the ingress demux ------------------------------------------------

    /// Route one client-bound `ServerMsg` into its per-session Source. Returns
    /// `None` when consumed (it belonged to a daemon-originated flow), or
    /// `Some(msg)` to pass through to the target-side `handle_server_msg`.
    /// Sync — a bounded `try_send` never blocks the WS-read loop.
    fn intercept(&self, msg: ServerMsg) -> Option<ServerMsg> {
        // 1) Pre-session: `TunnelOpened` (and an open-FAILURE `Error`) carry the
        //    `open_nonce` we stamped. `TunnelOpened` promotes the pending entry
        //    to a session entry; the `Error` fails that flow's open fast.
        match &msg {
            ServerMsg::TunnelOpened {
                open_nonce: Some(nonce),
                session_id,
                ..
            } => {
                let sender = self.inner.pending_opens.lock().unwrap().remove(nonce);
                let Some(tx) = sender else {
                    // Unknown nonce (stale / not ours) — let it fall through.
                    return Some(msg);
                };
                let sid = *session_id;
                // Deliver the `opened` so the driver's open-wait sees it, THEN
                // register the session (the send moves `msg`). If the driver
                // already gave up (receiver dropped), don't register a dead
                // sender.
                match tx.try_send(msg) {
                    Ok(()) => {
                        self.inner.client_sessions.lock().unwrap().insert(sid, tx);
                    }
                    Err(_) => debug!(%sid, "opened arrived after the opener gave up; dropping"),
                }
                return None;
            }
            ServerMsg::Error {
                open_nonce: Some(nonce),
                ..
            } => {
                if let Some(tx) = self.inner.pending_opens.lock().unwrap().remove(nonce) {
                    let _ = tx.try_send(msg); // deliver the failure; the driver bails
                    return None;
                }
                // No matching nonce → fall through to the session_id routing
                // (an `Error` can also carry a live `session_id`).
            }
            _ => {}
        }

        // 2) Post-session: route the session-scoped client-bound variants by
        //    `session_id ∈ client_sessions`. A `session_id` we don't own is a
        //    target-side session — pass it through unchanged.
        let Some(sid) = client_bound_session_id(&msg) else {
            return Some(msg);
        };
        let sender = self
            .inner
            .client_sessions
            .lock()
            .unwrap()
            .get(&sid)
            .cloned();
        let Some(tx) = sender else {
            return Some(msg);
        };
        match tx.try_send(msg) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(m)) => {
                warn!(%sid, kind = server_msg_kind(&m), "client session source full; dropping");
                None
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The session ended between the lookup and the send — reap it.
                self.inner.client_sessions.lock().unwrap().remove(&sid);
                None
            }
        }
    }
}

/// RAII guard: clears the hub's published sink to `None` on drop (every
/// `connect_once` exit path), so a supervisor holding the dead egress re-waits.
pub struct SinkGuard {
    hub: TunnelClientHub,
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        // `send_replace` (not `send`): clear the value even if no supervisor is
        // currently subscribed, so a supervisor created later doesn't see a
        // stale sink from a dead connection.
        self.hub.inner.sink_tx.send_replace(None);
    }
}

/// The signaling-loop hook (mirrors `overlay::intercept`): consume a
/// client-bound `ServerMsg` (→ `None`) or pass it through (→ `Some`).
pub fn intercept_server_msg(hub: &TunnelClientHub, msg: ServerMsg) -> Option<ServerMsg> {
    hub.intercept(msg)
}

// ---------------------------------------------------------------------------
// Signaling seam impls — the daemon's Sink (nonce-stamping) + Source (channel)
// ---------------------------------------------------------------------------

/// The daemon's [`TunnelSignalingSink`]: funnels a session's `ClientMsg`s onto
/// the shared agent-WS egress, stamping this session's `open_nonce` onto the
/// `TunnelOpen` so the reply demux can match its `TunnelOpened` / `Error`.
struct DaemonSink {
    tx: mpsc::Sender<ClientMsg>,
    nonce: String,
}

#[async_trait]
impl TunnelSignalingSink for DaemonSink {
    async fn send(&self, msg: ClientMsg) -> anyhow::Result<()> {
        let msg = match msg {
            ClientMsg::TunnelOpen {
                agent_id,
                transport,
                open_nonce: _,
            } => ClientMsg::TunnelOpen {
                agent_id,
                transport,
                open_nonce: Some(self.nonce.clone()),
            },
            other => other,
        };
        debug!(nonce = %self.nonce, "DaemonSink: enqueue ClientMsg to agent-WS egress");
        self.tx
            .send(msg)
            .await
            .map_err(|e| anyhow::anyhow!("agent WS egress closed: {e}"))
    }
}

/// The daemon's [`TunnelSignalingSource`]: a per-session mpsc fed by the hub's
/// demux. Sniffs the pass-through `TunnelOpened` to record the negotiated
/// transport + session id into the flow's live cells, then yields it to the
/// driver. `None` = the session's demux entry was removed (WS drop / kill).
struct ChannelSource {
    rx: mpsc::Receiver<ServerMsg>,
    live: Arc<FlowLive>,
}

#[async_trait]
impl TunnelSignalingSource for ChannelSource {
    async fn recv(&mut self) -> Option<ServerMsg> {
        let msg = self.rx.recv().await?;
        if let ServerMsg::TunnelOpened {
            session_id,
            transport,
            ..
        } = &msg
        {
            *self.live.transport.lock().unwrap() = Some(transport.clone());
            *self.live.session_id.lock().unwrap() = Some(*session_id);
            *self.live.status.lock().unwrap() = FlowStatus::Up;
        }
        Some(msg)
    }
}

// ---------------------------------------------------------------------------
// The supervised flow loop
// ---------------------------------------------------------------------------

/// Supervise one flow: (re)establish a tunnel session over the live agent WS
/// and serve `local` until the session drops, then back off + retry. Owns the
/// local-port intent across WS reconnects (the CLI's `run_forward` shape,
/// relocated + sharing the daemon's ONE WS instead of dialing its own).
async fn run_flow_supervisor(
    hub: TunnelClientHub,
    flow_id: String,
    agent_id: ObjectId,
    local: u16,
    target: Target,
    pref: TransportPref,
    live: Arc<FlowLive>,
) {
    info!(flow = %flow_id, "flow supervisor started");
    let mut sink_rx = hub.inner.sink_tx.subscribe();
    let mut backoff = RECONNECT_BACKOFF_MIN;
    let mut attempt: u64 = 0;
    loop {
        *live.status.lock().unwrap() = FlowStatus::Connecting;
        // Wait for a live agent WS.
        let sink_tx = match wait_for_sink(&mut sink_rx).await {
            Some(tx) => {
                info!(flow = %flow_id, "flow supervisor: got live agent-WS sink");
                tx
            }
            None => return, // hub dropped — the daemon is shutting down
        };

        let result = run_session_with_fallback(
            &hub,
            &flow_id,
            &mut attempt,
            &sink_tx,
            local,
            agent_id,
            &target,
            pref,
            &live,
        )
        .await;

        *live.status.lock().unwrap() = FlowStatus::Down;
        match result {
            Ok(()) => {
                info!(flow = %flow_id, "tunnel session ended; reconnecting");
                backoff = RECONNECT_BACKOFF_MIN;
            }
            Err(e) => {
                warn!(flow = %flow_id, %e, backoff_s = backoff.as_secs(), "tunnel session failed; retrying");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

/// Return the current live sink, or wait for one. `None` once the hub's sink
/// sender is dropped (daemon shutdown).
async fn wait_for_sink(
    sink_rx: &mut watch::Receiver<Option<mpsc::Sender<ClientMsg>>>,
) -> Option<mpsc::Sender<ClientMsg>> {
    loop {
        if let Some(tx) = sink_rx.borrow_and_update().clone() {
            return Some(tx);
        }
        if sink_rx.changed().await.is_err() {
            return None;
        }
    }
}

/// One session with the Auto→WebRTC transport fallback. Mirrors the CLI's
/// `run_one_session`, but each attempt gets a fresh nonce + Source (the daemon
/// re-opens over the shared WS rather than dialing a new one).
#[allow(clippy::too_many_arguments)]
async fn run_session_with_fallback(
    hub: &TunnelClientHub,
    flow_id: &str,
    attempt: &mut u64,
    sink_tx: &mpsc::Sender<ClientMsg>,
    local: u16,
    agent_id: ObjectId,
    target: &Target,
    pref: TransportPref,
    live: &Arc<FlowLive>,
) -> Result<()> {
    let outcome = drive_one(
        hub,
        flow_id,
        attempt,
        sink_tx,
        local,
        agent_id,
        target,
        pref.supported_transports(),
        pref.request_transport(),
        live,
    )
    .await?;
    if matches!(outcome, SessionOutcome::QuicSetupFailed) {
        if pref == TransportPref::Auto {
            warn!(flow = %flow_id, "QUIC setup failed; re-opening over webrtc-dc-v1");
            let fallback = drive_one(
                hub,
                flow_id,
                attempt,
                sink_tx,
                local,
                agent_id,
                target,
                vec![TRANSPORT_WEBRTC_DC_V1.to_string()],
                TRANSPORT_WEBRTC_DC_V1,
                live,
            )
            .await?;
            if matches!(fallback, SessionOutcome::QuicSetupFailed) {
                bail!("webrtc-dc-v1 fallback unexpectedly reported QUIC-setup-failed");
            }
        } else {
            bail!("QUIC setup failed and transport={pref:?} forbids fallback");
        }
    }
    Ok(())
}

/// Build this attempt's nonce + demux registration + seam, drive one
/// `run_tunnel_session`, then reap the attempt's demux entries.
#[allow(clippy::too_many_arguments)]
async fn drive_one(
    hub: &TunnelClientHub,
    flow_id: &str,
    attempt: &mut u64,
    sink_tx: &mpsc::Sender<ClientMsg>,
    local: u16,
    agent_id: ObjectId,
    target: &Target,
    supported: Vec<String>,
    request: &str,
    live: &Arc<FlowLive>,
) -> Result<SessionOutcome> {
    *attempt += 1;
    let nonce = format!("{flow_id}.{attempt}");
    let (src_tx, src_rx) = mpsc::channel::<ServerMsg>(SESSION_SOURCE_DEPTH);

    // Register the pending open BEFORE the driver sends `TunnelOpen`, so a fast
    // `TunnelOpened` can't race the insert (critique U3).
    hub.inner
        .pending_opens
        .lock()
        .unwrap()
        .insert(nonce.clone(), src_tx);
    *live.nonce.lock().unwrap() = Some(nonce.clone());

    let sink: Arc<dyn TunnelSignalingSink> = Arc::new(DaemonSink {
        tx: sink_tx.clone(),
        nonce: nonce.clone(),
    });
    let source: Box<dyn TunnelSignalingSource> = Box::new(ChannelSource {
        rx: src_rx,
        live: live.clone(),
    });

    info!(flow = %flow_id, %nonce, request, "flow: driving run_tunnel_session (hello+open)");
    let result = run_tunnel_session(
        sink,
        source,
        local,
        SessionParams {
            agent_id,
            target: target.clone(),
            client_version: hub.inner.client_version.clone(),
        },
        supported,
        request,
    )
    .await;

    // Reap this attempt's demux entries (the pending nonce if the open never
    // completed, the session if it did) so nothing leaks across attempts.
    hub.inner.pending_opens.lock().unwrap().remove(&nonce);
    *live.nonce.lock().unwrap() = None;
    if let Some(sid) = live.session_id.lock().unwrap().take() {
        hub.inner.client_sessions.lock().unwrap().remove(&sid);
    }
    *live.transport.lock().unwrap() = None;

    result.with_context(|| format!("tunnel session (flow {flow_id})"))
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The `session_id` of a **client-bound** session-scoped `ServerMsg` — the set a
/// tunnel-client session consumes (driver `dispatch_loop` + QUIC path). `None`
/// for target-only / non-session variants (they pass through). `TunnelOpened` is
/// routed by nonce upstream, so it's `None` here.
fn client_bound_session_id(msg: &ServerMsg) -> Option<ObjectId> {
    match msg {
        ServerMsg::TunnelSdpAnswer { session_id, .. }
        | ServerMsg::TunnelIce { session_id, .. }
        | ServerMsg::TcpForwardAccept { session_id, .. }
        | ServerMsg::TcpForwardReject { session_id, .. }
        | ServerMsg::TcpHalfClose { session_id, .. }
        | ServerMsg::TcpClosed { session_id, .. }
        | ServerMsg::UdpForwardAccept { session_id, .. }
        | ServerMsg::UdpForwardReject { session_id, .. }
        | ServerMsg::UdpClosed { session_id, .. }
        | ServerMsg::TunnelTerminate { session_id, .. }
        | ServerMsg::TunnelQuicReady { session_id, .. } => Some(*session_id),
        // An `Error` may carry a live session id (post-open failures).
        ServerMsg::Error {
            session_id: Some(sid),
            ..
        } => Some(*sid),
        _ => None,
    }
}

/// Short kind label for diagnostics on a dropped/overflowed frame.
fn server_msg_kind(msg: &ServerMsg) -> &'static str {
    match msg {
        ServerMsg::TunnelSdpAnswer { .. } => "tunnel.sdp.answer",
        ServerMsg::TunnelIce { .. } => "tunnel.ice",
        ServerMsg::TcpForwardAccept { .. } => "tunnel.tcp.accept",
        ServerMsg::TcpForwardReject { .. } => "tunnel.tcp.reject",
        ServerMsg::TcpHalfClose { .. } => "tunnel.tcp.half_close",
        ServerMsg::TcpClosed { .. } => "tunnel.tcp.closed",
        ServerMsg::UdpForwardAccept { .. } => "tunnel.udp.accept",
        ServerMsg::UdpForwardReject { .. } => "tunnel.udp.reject",
        ServerMsg::UdpClosed { .. } => "tunnel.udp.closed",
        ServerMsg::TunnelTerminate { .. } => "tunnel.terminate",
        ServerMsg::TunnelQuicReady { .. } => "tunnel.quic.ready",
        _ => "other",
    }
}

/// Parse + validate a target node (a 24-hex agent id).
fn parse_node(node: &str) -> std::result::Result<ObjectId, String> {
    ObjectId::parse_str(node).map_err(|_| format!("node must be a 24-hex agent id, got '{node}'"))
}

/// `auto` (default / empty / unknown) | `quic` | `webrtc`.
fn parse_transport(s: &str) -> TransportPref {
    match s.trim().to_ascii_lowercase().as_str() {
        "quic" | "quic-v1" => TransportPref::Quic,
        "webrtc" | "webrtc-dc-v1" => TransportPref::Webrtc,
        _ => TransportPref::Auto,
    }
}

/// The display word for a preference (inverse of [`parse_transport`]).
fn transport_word(p: TransportPref) -> &'static str {
    match p {
        TransportPref::Auto => "auto",
        TransportPref::Quic => "quic",
        TransportPref::Webrtc => "webrtc",
    }
}

/// Parse a `host:port` (robust to bracketed IPv6 `[::1]:80`). Mirrors the CLI's
/// `forward::parse_remote` — kept local so the daemon doesn't depend on the CLI
/// crate.
fn parse_host_port(s: &str) -> Result<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let close = rest
            .find(']')
            .with_context(|| format!("remote with `[` must close with `]:port`: {s}"))?;
        let host = &rest[..close];
        let port_str = rest[close + 1..]
            .strip_prefix(':')
            .with_context(|| format!("missing `:port` after `]`: {s}"))?;
        let port = port_str
            .parse()
            .with_context(|| format!("invalid port {port_str}"))?;
        return Ok((host.to_string(), port));
    }
    let (host, port_str) = s
        .rsplit_once(':')
        .with_context(|| format!("remote must be host:port, got {s}"))?;
    if host.is_empty() {
        bail!("remote host must not be empty");
    }
    let port = port_str
        .parse()
        .with_context(|| format!("invalid port {port_str}"))?;
    Ok((host.to_string(), port))
}

/// Fail fast at create time if `local` can't be bound (the common "port already
/// in use" misconfig), with a clean message. The listener is dropped
/// immediately; the driver re-binds it per session attempt (it late-binds to
/// preserve the QUIC→WebRTC fallback), so this is only a validation probe — the
/// tiny TOCTOU window before the supervisor's first bind is a non-issue for an
/// operator-paced create.
async fn probe_local_port(local: u16) -> std::result::Result<(), String> {
    if local == 0 {
        return Err("local port must not be 0".into());
    }
    match tokio::net::TcpListener::bind(("127.0.0.1", local)).await {
        Ok(l) => {
            drop(l);
            Ok(())
        }
        Err(e) => Err(format!("local port {local} is not available: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::signaling::CloseReason;
    use serde_json::json;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes([byte; 12])
    }

    #[test]
    fn parse_transport_maps_words_and_defaults() {
        assert_eq!(parse_transport("quic"), TransportPref::Quic);
        assert_eq!(parse_transport("QUIC-V1"), TransportPref::Quic);
        assert_eq!(parse_transport("webrtc"), TransportPref::Webrtc);
        assert_eq!(parse_transport("auto"), TransportPref::Auto);
        assert_eq!(parse_transport(""), TransportPref::Auto);
        assert_eq!(parse_transport("garbage"), TransportPref::Auto);
        assert_eq!(transport_word(TransportPref::Auto), "auto");
        assert_eq!(transport_word(TransportPref::Quic), "quic");
        assert_eq!(transport_word(TransportPref::Webrtc), "webrtc");
    }

    #[test]
    fn parse_host_port_variants() {
        assert_eq!(parse_host_port("db:5432").unwrap(), ("db".into(), 5432));
        assert_eq!(parse_host_port("[::1]:80").unwrap(), ("::1".into(), 80));
        assert!(parse_host_port("noport").is_err());
        assert!(parse_host_port(":5432").is_err());
        assert!(parse_host_port("h:99999").is_err());
    }

    #[test]
    fn parse_node_rejects_non_object_id() {
        assert!(parse_node("nope").is_err());
        assert!(parse_node("0123456789abcdef01234567").is_ok());
    }

    /// The demux is the crux (Risk 1). Lock each routing decision: pre-session
    /// nonce, post-session id, bidirectional pass-through, target-only
    /// pass-through, and open-failure.
    #[tokio::test]
    async fn demux_routes_by_nonce_then_session_id() {
        let hub = TunnelClientHub::new("test".into());
        let sid = oid(7);
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(16);
        hub.inner
            .pending_opens
            .lock()
            .unwrap()
            .insert("n1".into(), tx);

        // (a) TunnelOpened with our nonce is consumed, promoted, and delivered.
        let opened = ServerMsg::TunnelOpened {
            session_id: sid,
            transport: "quic-v1".into(),
            dc_pool_size: 4,
            sctp_rwnd_bytes: 0,
            ice_servers: vec![],
            quic_auth_token: None,
            open_nonce: Some("n1".into()),
        };
        assert!(hub.intercept(opened).is_none(), "opened consumed by nonce");
        assert!(matches!(rx.try_recv(), Ok(ServerMsg::TunnelOpened { .. })));
        assert!(hub.inner.pending_opens.lock().unwrap().is_empty());
        assert!(hub.inner.client_sessions.lock().unwrap().contains_key(&sid));

        // (b) A post-open client-bound variant for that session is consumed.
        let accept = ServerMsg::TcpForwardAccept {
            session_id: sid,
            flow_id: 1,
            dc_index: 0,
        };
        assert!(hub.intercept(accept).is_none(), "accept routed by session");
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMsg::TcpForwardAccept { .. })
        ));

        // (c) The SAME variant for an UNKNOWN session passes through (target).
        let other = ServerMsg::TcpForwardAccept {
            session_id: oid(9),
            flow_id: 2,
            dc_index: 0,
        };
        assert!(
            hub.intercept(other).is_some(),
            "unknown session passes through"
        );

        // (d) A bidirectional variant (TunnelIce) for our session is consumed…
        let ice_ours = ServerMsg::TunnelIce {
            session_id: sid,
            candidate: json!({}),
        };
        assert!(hub.intercept(ice_ours).is_none());
        // …but for an unknown session passes through to the target side.
        let ice_target = ServerMsg::TunnelIce {
            session_id: oid(9),
            candidate: json!({}),
        };
        assert!(hub.intercept(ice_target).is_some());

        // (e) A target-only variant (TunnelSdpOffer) always passes through.
        let offer = ServerMsg::TunnelSdpOffer {
            session_id: sid,
            sdp: "x".into(),
        };
        assert!(hub.intercept(offer).is_some(), "target-only passes through");
    }

    #[tokio::test]
    async fn demux_open_failure_error_resolves_pending_nonce() {
        let hub = TunnelClientHub::new("test".into());
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(16);
        hub.inner
            .pending_opens
            .lock()
            .unwrap()
            .insert("n2".into(), tx);

        let err = ServerMsg::Error {
            session_id: None,
            code: "cross_tenant".into(),
            message: "nope".into(),
            open_nonce: Some("n2".into()),
        };
        assert!(
            hub.intercept(err).is_none(),
            "open-failure Error consumed by nonce"
        );
        assert!(matches!(rx.try_recv(), Ok(ServerMsg::Error { .. })));
        assert!(hub.inner.pending_opens.lock().unwrap().is_empty());

        // A nonceless Error (old server / target-side) passes through.
        let bare = ServerMsg::Error {
            session_id: None,
            code: "x".into(),
            message: "y".into(),
            open_nonce: None,
        };
        assert!(hub.intercept(bare).is_some());
    }

    #[tokio::test]
    async fn demux_terminate_is_bidirectional() {
        let hub = TunnelClientHub::new("test".into());
        let sid = oid(3);
        let (tx, _rx) = mpsc::channel::<ServerMsg>(16);
        hub.inner.client_sessions.lock().unwrap().insert(sid, tx);

        // Our session → consumed.
        let ours = ServerMsg::TunnelTerminate {
            session_id: sid,
            reason: CloseReason::ServerTerminated,
        };
        assert!(hub.intercept(ours).is_none());
        // Someone else's → passes through to the target-side handler.
        let target = ServerMsg::TunnelTerminate {
            session_id: oid(4),
            reason: CloseReason::ServerTerminated,
        };
        assert!(hub.intercept(target).is_some());
    }

    #[test]
    fn flows_snapshot_reflects_registered_flows() {
        let hub = TunnelClientHub::new("test".into());
        // No live sink, so the supervisor just parks in Connecting — but the
        // registry + snapshot are exercised without a running session.
        let live = Arc::new(FlowLive::default());
        let handle = tokio::runtime::Runtime::new().unwrap();
        let abort = handle.spawn(async {}).abort_handle();
        hub.inner.flows.lock().unwrap().insert(
            "fl-1".into(),
            FlowHandle {
                abort,
                kind: FlowKind::Forward,
                local: 5432,
                target: Some("db:5432".into()),
                node: "0123456789abcdef01234567".into(),
                requested: "auto".into(),
                live: live.clone(),
            },
        );
        let snap = hub.flows_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "fl-1");
        assert_eq!(snap[0].local_addr, "127.0.0.1:5432");
        assert_eq!(snap[0].target.as_deref(), Some("db:5432"));
        // Not yet up → the transport column shows the liveness word.
        assert_eq!(snap[0].transport, "connecting");

        // Once a session negotiates, the concrete transport shows.
        *live.status.lock().unwrap() = FlowStatus::Up;
        *live.transport.lock().unwrap() = Some("quic-v1".into());
        assert_eq!(hub.flows_snapshot()[0].transport, "quic-v1");

        // kill removes it from the registry.
        assert!(hub.kill_flow("fl-1"));
        assert!(hub.flows_snapshot().is_empty());
        assert!(!hub.kill_flow("fl-1"), "second kill is a no-op false");
    }

    #[tokio::test]
    async fn publish_sink_is_visible_to_a_later_subscriber() {
        // Regression (found via the E2E test): a flow supervisor subscribes only
        // when its flow is created, which can be AFTER the first WS connect
        // published the sink. `watch::Sender::send` silently drops the value when
        // there are no receivers yet, so a plain `send` left the value `None` and
        // the supervisor hung at wait_for_sink forever. `publish_sink` must use
        // `send_replace` so a LATER subscriber still sees the live sink.
        let hub = TunnelClientHub::new("t".into());
        let (tx, _rx) = mpsc::channel::<ClientMsg>(1);
        let _guard = hub.publish_sink(tx); // published with NO subscriber yet
        let mut sink_rx = hub.inner.sink_tx.subscribe();
        assert!(
            sink_rx.borrow_and_update().is_some(),
            "a subscriber created after publish_sink must see the live egress"
        );
        // Dropping the guard clears it (also visible to the existing subscriber).
        drop(_guard);
        assert!(
            sink_rx.borrow_and_update().is_none(),
            "the guard clears the sink on drop"
        );
    }
}
