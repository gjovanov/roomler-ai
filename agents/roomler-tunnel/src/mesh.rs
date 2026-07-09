//! SOCKS5 **mesh** mode (`roomler-tunnel socks5` with no `--agent`).
//!
//! One local SOCKS5 proxy that reaches the WHOLE tenant: each CONNECT names an
//! agent, and the mesh routes the flow to that agent. It reuses the proven
//! single-agent data plane VERBATIM — for each agent it lazily spawns a Phase-1
//! `socks5 --agent <id>` proxy on a loopback port and SOCKS-chains into it. No
//! change to the tunnel hot path; each per-agent proxy keeps its own
//! keepalive/auto-reconnect session.
//!
//! Address an agent by its **friendly name** (from the server roster) or its
//! **24-hex agent-id** as the SOCKS hostname
//! (`curl --socks5-hostname neo16:3389` or `<agent-id>:3389`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bson::oid::ObjectId;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::config::TunnelConfig;
use crate::forward::{self, TransportPref};
use crate::socks5;

/// Per-agent loopback proxy registry: agent-id → the `127.0.0.1` port its
/// Phase-1 SOCKS proxy listens on. Lazily populated on first CONNECT to an agent
/// and reused thereafter.
type ProxyPorts = Arc<Mutex<HashMap<ObjectId, u16>>>;

/// Friendly-name → agent-id map (lowercased keys), fetched from the server so a
/// CONNECT can name an agent by its label instead of its raw 24-hex id.
type Roster = Arc<Mutex<HashMap<String, ObjectId>>>;

/// Cap on connecting to a freshly-spawned per-agent proxy (it must bind its
/// listener; its tunnel session establishes lazily on the first forwarded flow).
const PROXY_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run the mesh SOCKS5 proxy on `127.0.0.1:local`.
pub async fn run_mesh(cfg: TunnelConfig, local: u16, transport: TransportPref) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", local))
        .await
        .with_context(|| format!("binding 127.0.0.1:{local}"))?;
    info!(
        local = %listener.local_addr()?,
        ?transport,
        "roomler-tunnel SOCKS5 mesh listening (address an agent by its name or 24-hex id as the SOCKS hostname)"
    );
    let ports: ProxyPorts = Arc::new(Mutex::new(HashMap::new()));

    // Agent roster (friendly-name → agent-id). Best-effort at startup — if it
    // fails (older server, transient), addressing by raw 24-hex agent-id still
    // works, and an unknown name triggers a lazy re-fetch below.
    let roster: Roster = Arc::new(Mutex::new(HashMap::new()));
    match fetch_roster(&cfg).await {
        Ok(r) => {
            info!(
                agents = r.len(),
                "mesh: fetched agent roster (name → agent-id)"
            );
            *roster.lock().await = r;
        }
        Err(e) => warn!(%e, "mesh: roster fetch failed; agent-id addressing still works"),
    }

    loop {
        let (mut tcp, peer_addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                error!(%e, "accept failed");
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let cfg = cfg.clone();
        let ports = Arc::clone(&ports);
        let roster = Arc::clone(&roster);
        tokio::spawn(async move {
            // SOCKS server handshake → CONNECT target or UDP ASSOCIATE.
            let (host, port) = match socks5::accept_request(&mut tcp).await {
                Ok(socks5::Socks5Request::Connect { host, port }) => (host, port),
                Ok(socks5::Socks5Request::UdpAssociate) => {
                    // UDP ASSOCIATE routes per-datagram (each names an agent);
                    // hand off to the mesh UDP relay.
                    if let Err(e) = handle_associate_mesh(tcp, cfg, transport, ports, roster).await
                    {
                        warn!(%peer_addr, %e, "mesh UDP associate ended with error");
                    }
                    return;
                }
                Err(e) => {
                    warn!(%peer_addr, %e, "mesh socks handshake failed");
                    return;
                }
            };
            // Resolve the SOCKS hostname → agent: a raw 24-hex agent-id, or a
            // friendly name from the roster (case-insensitive).
            let Some(agent_id) = resolve_agent(&host, &cfg, &roster).await else {
                warn!(%peer_addr, host, "mesh: unknown target agent (not an id, not a known name)");
                socks5::reply(&mut tcp, socks5::REP_GENERAL_FAILURE).await;
                return;
            };
            let proxy_port = match get_or_spawn_proxy(agent_id, &cfg, transport, &ports).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(%peer_addr, %agent_id, %e, "mesh: agent proxy unavailable");
                    socks5::reply(&mut tcp, socks5::REP_GENERAL_FAILURE).await;
                    return;
                }
            };
            // Chain into the agent's loopback proxy: reach the requested port on
            // the agent's OWN host (`127.0.0.1:port` from the agent's vantage).
            let mut inner = match connect_proxy(proxy_port, port).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer_addr, %agent_id, %e, "mesh: chaining to agent proxy failed");
                    socks5::reply(&mut tcp, socks5::REP_GENERAL_FAILURE).await;
                    return;
                }
            };
            socks5::reply(&mut tcp, socks5::REP_SUCCESS).await;
            if let Err(e) = tokio::io::copy_bidirectional(&mut tcp, &mut inner).await {
                warn!(%peer_addr, %agent_id, %e, "mesh flow ended with error");
            }
        });
    }
}

/// Handle a SOCKS5 UDP ASSOCIATE on the mesh listener. Binds a local
/// relay socket, replies with its address, then relays each app datagram
/// to the agent named in the datagram's SOCKS-UDP header by SOCKS-UDP-
/// chaining into that agent's Phase-1 loopback proxy (which already does
/// UDP ASSOCIATE for a single agent). The target host names an agent
/// (name / 24-hex id); the agent dials `127.0.0.1:port` (its own host),
/// exactly like the mesh TCP path. Association lifetime = the SOCKS
/// control TCP connection (RFC 1928).
async fn handle_associate_mesh(
    mut tcp: TcpStream,
    cfg: TunnelConfig,
    transport: TransportPref,
    ports: ProxyPorts,
    roster: Roster,
) -> Result<()> {
    let relay = Arc::new(
        UdpSocket::bind(("127.0.0.1", 0))
            .await
            .context("bind mesh udp relay")?,
    );
    let relay_addr = relay.local_addr()?;
    socks5::reply_bound(&mut tcp, socks5::REP_SUCCESS, relay_addr).await;
    info!(%relay_addr, "mesh: UDP ASSOCIATE relay bound");

    // agent-id → the mesh-side UDP socket chained to that agent's proxy relay.
    let chains: Arc<Mutex<HashMap<ObjectId, Arc<UdpSocket>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut app_src: Option<SocketAddr> = None;
    let mut buf = vec![0u8; 64 * 1024 + 512];

    loop {
        tokio::select! {
            // App's TCP control conn closing ends the whole association.
            _ = drain_control(&mut tcp) => {
                debug!("mesh: UDP control connection closed; ending association");
                break;
            }
            recvd = relay.recv_from(&mut buf) => {
                let (n, from) = match recvd {
                    Ok(x) => x,
                    Err(e) => { warn!(%e, "mesh: udp relay recv_from failed"); continue; }
                };
                let src = *app_src.get_or_insert(from);
                if from != src {
                    debug!(%from, %src, "mesh: udp datagram from unexpected source — dropping");
                    continue;
                }
                let (name, port, off) = match socks5::parse_udp_datagram(&buf[..n]) {
                    Ok(x) => x,
                    Err(e) => { debug!(%e, "mesh: malformed socks udp datagram — dropping"); continue; }
                };
                let Some(agent_id) = resolve_agent(&name, &cfg, &roster).await else {
                    debug!(host = %name, "mesh: udp datagram for unknown agent — dropping");
                    continue;
                };
                let existing = chains.lock().await.get(&agent_id).cloned();
                let sock = match existing {
                    Some(s) => s,
                    None => match open_udp_chain(
                        agent_id, &cfg, transport, &ports, &relay, src, &name, &chains,
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(%agent_id, %e, "mesh: udp chain open failed");
                            continue;
                        }
                    },
                };
                // Forward to the proxy relay, target rewritten to the agent's
                // own host (`127.0.0.1:port`) — the mesh addressing model.
                let framed = socks5::encode_udp_datagram("127.0.0.1", port, &buf[off..n]);
                if let Err(e) = sock.send(&framed).await {
                    debug!(%agent_id, %e, "mesh: udp send to agent proxy failed");
                }
            }
        }
    }
    // Dropping `chains` drops the mesh-side sockets; each response task sees
    // its socket close and exits, dropping its held control-TCP → the proxy
    // tears the per-agent association down.
    Ok(())
}

/// Open a SOCKS-UDP chain into `agent_id`'s Phase-1 loopback proxy:
/// spawn/reuse the proxy, TCP-connect + UDP ASSOCIATE against it, and bind
/// a mesh-side UDP socket connected to the proxy's relay. Spawns a task
/// pumping the proxy's responses back to the app, re-framed with the agent
/// `name` as source (so the app sees a reply from what it addressed).
#[allow(clippy::too_many_arguments)]
async fn open_udp_chain(
    agent_id: ObjectId,
    cfg: &TunnelConfig,
    transport: TransportPref,
    ports: &ProxyPorts,
    app_relay: &Arc<UdpSocket>,
    app_src: SocketAddr,
    name: &str,
    chains: &Arc<Mutex<HashMap<ObjectId, Arc<UdpSocket>>>>,
) -> Result<Arc<UdpSocket>> {
    let proxy_port = get_or_spawn_proxy(agent_id, cfg, transport, ports).await?;
    let mut control = connect_tcp(proxy_port).await?;
    let proxy_relay = socks5::client_udp_associate(&mut control).await?;
    let sock = Arc::new(
        UdpSocket::bind(("127.0.0.1", 0))
            .await
            .context("bind mesh->proxy udp")?,
    );
    sock.connect(proxy_relay)
        .await
        .context("connect to agent proxy relay")?;
    debug!(%agent_id, %proxy_relay, "mesh: udp chain to agent proxy open");

    let sock_rx = Arc::clone(&sock);
    let app_relay = Arc::clone(app_relay);
    let name = name.to_string();
    let chains_rx = Arc::clone(chains);
    tokio::spawn(async move {
        let _control = control; // hold the association open for its lifetime
        let mut buf = vec![0u8; 64 * 1024 + 512];
        while let Ok(n) = sock_rx.recv(&mut buf).await {
            // Proxy response header names 127.0.0.1:port; rewrite source to
            // the agent name the app used so the app accepts the reply.
            if let Ok((_h, port, off)) = socks5::parse_udp_datagram(&buf[..n]) {
                let reframed = socks5::encode_udp_datagram(&name, port, &buf[off..n]);
                let _ = app_relay.send_to(&reframed, app_src).await;
            }
        }
        chains_rx.lock().await.remove(&agent_id);
    });

    chains.lock().await.insert(agent_id, Arc::clone(&sock));
    Ok(sock)
}

/// Read + discard the SOCKS control connection until EOF/error — its close
/// is the UDP association's teardown signal (RFC 1928).
async fn drain_control(tcp: &mut TcpStream) -> std::io::Result<()> {
    let mut b = [0u8; 256];
    loop {
        match tcp.read(&mut b).await {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Get the loopback port of `agent_id`'s Phase-1 SOCKS proxy, spawning it (a
/// persistent `run_socks5` task on a free loopback port) on first use.
async fn get_or_spawn_proxy(
    agent_id: ObjectId,
    cfg: &TunnelConfig,
    transport: TransportPref,
    ports: &ProxyPorts,
) -> Result<u16> {
    if let Some(p) = ports.lock().await.get(&agent_id).copied() {
        return Ok(p);
    }
    // Reserve a free loopback port, then hand it to a persistent per-agent proxy.
    // (Tiny bind→spawn race on loopback is acceptable; the connect below retries.)
    let probe = TcpListener::bind("127.0.0.1:0")
        .await
        .context("probe free loopback port")?;
    let port = probe.local_addr()?.port();
    drop(probe);

    let cfg = cfg.clone();
    let agent_hex = agent_id.to_hex();
    tokio::spawn(async move {
        // `run_socks5` reconnects internally (v1.1); it only returns on a fatal
        // setup error (e.g. a bad config), in which case this agent's proxy is
        // gone until re-requested.
        if let Err(e) = forward::run_socks5(cfg, &agent_hex, port, transport).await {
            warn!(%agent_hex, %e, "per-agent proxy exited");
        }
    });
    ports.lock().await.insert(agent_id, port);
    Ok(port)
}

/// TCP-connect to a per-agent loopback proxy, retrying briefly while the
/// freshly-spawned proxy binds its listener.
async fn connect_tcp(proxy_port: u16) -> Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + PROXY_READY_TIMEOUT;
    loop {
        match TcpStream::connect(("127.0.0.1", proxy_port)).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e).context("connect to agent proxy"),
        }
    }
}

/// Connect to a per-agent loopback proxy and SOCKS-CONNECT to `127.0.0.1:dst_port`
/// (the agent's own host from its vantage).
async fn connect_proxy(proxy_port: u16, dst_port: u16) -> Result<TcpStream> {
    let mut stream = connect_tcp(proxy_port).await?;
    socks5::client_connect(&mut stream, "127.0.0.1", dst_port).await?;
    Ok(stream)
}

/// One agent in the server's roster response.
#[derive(serde::Deserialize)]
struct AgentInfo {
    agent_id: String,
    name: String,
}

/// Resolve a SOCKS-CONNECT hostname to an agent-id: a raw 24-hex id, or a
/// friendly name from the roster (case-insensitive). On a name miss, re-fetch
/// the roster once (picks up a newly-added / renamed agent) and retry.
async fn resolve_agent(host: &str, cfg: &TunnelConfig, roster: &Roster) -> Option<ObjectId> {
    if let Ok(id) = ObjectId::parse_str(host) {
        return Some(id);
    }
    let key = host.to_ascii_lowercase();
    if let Some(id) = roster.lock().await.get(&key).copied() {
        return Some(id);
    }
    // Unknown name — refresh once and retry.
    match fetch_roster(cfg).await {
        Ok(fresh) => {
            let id = fresh.get(&key).copied();
            *roster.lock().await = fresh;
            id
        }
        Err(e) => {
            warn!(%e, "mesh: roster refresh failed");
            None
        }
    }
}

/// GET the tenant's agent roster (`/api/tunnel-client/agents`, TunnelClient
/// bearer auth) and build a case-insensitive `name → agent-id` map.
async fn fetch_roster(cfg: &TunnelConfig) -> Result<HashMap<String, ObjectId>> {
    let url = format!(
        "{}/api/tunnel-client/agents",
        cfg.server_url.trim_end_matches('/')
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&cfg.tunnel_client_token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("roster fetch: HTTP {}", resp.status());
    }
    let agents: Vec<AgentInfo> = resp.json().await.context("parse roster json")?;
    let mut map = HashMap::new();
    for a in agents {
        if !a.name.is_empty()
            && let Ok(id) = ObjectId::parse_str(&a.agent_id)
        {
            map.insert(a.name.to_ascii_lowercase(), id);
        }
    }
    Ok(map)
}
