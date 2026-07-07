//! SOCKS5 **mesh** mode (`roomler-tunnel socks5` with no `--agent`).
//!
//! One local SOCKS5 proxy that reaches the WHOLE tenant: each CONNECT names an
//! agent, and the mesh routes the flow to that agent. It reuses the proven
//! single-agent data plane VERBATIM — for each agent it lazily spawns a Phase-1
//! `socks5 --agent <id>` proxy on a loopback port and SOCKS-chains into it. No
//! change to the tunnel hot path; each per-agent proxy keeps its own
//! keepalive/auto-reconnect session.
//!
//! v1.5a addresses an agent by its **24-hex agent-id** as the SOCKS hostname
//! (`curl --socks5-hostname <agent-id>:3389`). A friendly-name roster
//! (name → agent-id, from the server) is the v1.5b follow-up.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
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
        "roomler-tunnel SOCKS5 mesh listening (address an agent by its 24-hex id as the SOCKS hostname)"
    );
    let ports: ProxyPorts = Arc::new(Mutex::new(HashMap::new()));

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
        tokio::spawn(async move {
            // SOCKS server handshake → the client's CONNECT target.
            let (host, port) = match socks5::accept_connect(&mut tcp).await {
                Ok(hp) => hp,
                Err(e) => {
                    warn!(%peer_addr, %e, "mesh socks handshake failed");
                    return;
                }
            };
            // v1.5a: the SOCKS hostname IS the agent id.
            let Ok(agent_id) = ObjectId::parse_str(&host) else {
                warn!(
                    %peer_addr, host,
                    "mesh: unknown target (v1.5a expects a 24-hex agent-id as the SOCKS hostname)"
                );
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
