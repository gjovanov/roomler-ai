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
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bson::oid::ObjectId;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

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
            // SOCKS server handshake → the client's CONNECT target.
            let (host, port) = match socks5::accept_connect(&mut tcp).await {
                Ok(hp) => hp,
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

/// Connect to a per-agent loopback proxy and SOCKS-CONNECT to `127.0.0.1:dst_port`
/// (the agent's own host from its vantage). Retries the TCP connect briefly while
/// the freshly-spawned proxy binds its listener.
async fn connect_proxy(proxy_port: u16, dst_port: u16) -> Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + PROXY_READY_TIMEOUT;
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", proxy_port)).await {
            Ok(s) => break s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e).context("connect to agent proxy"),
        }
    };
    let _ = stream.set_nodelay(true);
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
