//! `roomler-tunnel` — TeamViewer-style native tunnel client.
//!
//! Forwards a local TCP port over a WebRTC P2P data channel to an
//! enrolled `roomler-agent`, which dials the corresponding intranet
//! destination. The Roomler API is signalling-only — payload never
//! touches the server.
//!
//! CLI surface:
//!
//!   roomler-tunnel enroll --server <url> --token <enrollment-jwt> --name <label>
//!   roomler-tunnel forward --agent <agent_id> --local <port> --remote <host:port>
//!   roomler-tunnel run [--config <path>]
//!   roomler-tunnel diagnose [--agent <agent_id>]
//!   roomler-tunnel status [--json]     # local daemon's node state (LocalAPI)
//!   roomler-tunnel peers  [--json]     # peers + connection types (LocalAPI)
//!   roomler-tunnel flows  [--json]     # active forwards / SOCKS5 (LocalAPI)

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use roomler_tunnel::{config, forward, localclient, mesh, update};

/// CLI transport shim — the parsed `--transport` value. The real preference
/// enum (`tunnel_core::driver::TransportPref`) is clap-free (so the driver
/// crate needn't depend on clap); this mirrors its variants for the CLI and
/// converts into it at dispatch. Wire values are lowercase (`auto`/`quic`/
/// `webrtc`), matching the pre-refactor `#[value(rename_all = "lowercase")]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
enum CliTransport {
    #[default]
    Auto,
    Quic,
    Webrtc,
}

impl From<CliTransport> for forward::TransportPref {
    fn from(t: CliTransport) -> Self {
        match t {
            CliTransport::Auto => forward::TransportPref::Auto,
            CliTransport::Quic => forward::TransportPref::Quic,
            CliTransport::Webrtc => forward::TransportPref::Webrtc,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "roomler-tunnel", version, about, long_about = None)]
struct Cli {
    /// Override config file location. Defaults to the platform config dir
    /// (`%APPDATA%\roomler\roomler-tunnel\config.toml` on Windows,
    /// `~/.config/roomler-tunnel/config.toml` on Linux, the equivalent on
    /// macOS via the `directories` crate).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enroll this laptop against a Roomler server using an admin-issued
    /// tunnel-enrollment token. Writes the long-lived `TunnelClient` JWT
    /// to the config file. Mirrors `roomler-agent enroll`.
    Enroll {
        /// Base URL of the Roomler API (e.g. https://roomler.ai).
        #[arg(long)]
        server: String,
        /// Tunnel-enrollment token, as printed by the admin UI.
        #[arg(long)]
        token: String,
        /// Friendly name shown in the admin tunnel-clients list.
        #[arg(long)]
        name: String,
    },
    /// Open one TCP forward: listen on `--local`, accept TCP connections,
    /// dial `--remote` from the named agent's host. Stays in the
    /// foreground; Ctrl-C tears down.
    Forward {
        /// Hex `agent_id` of the target agent (visible in the admin UI).
        #[arg(long)]
        agent: String,
        /// Local TCP port to listen on (bound to 127.0.0.1).
        #[arg(long)]
        local: u16,
        /// Remote destination `host:port` that the agent dials. Subject
        /// to the agent's allowlist + the tenant's tunnel_policies.
        #[arg(long)]
        remote: String,
        /// Data-plane transport. `auto` (default) prefers QUIC and
        /// transparently falls back to WebRTC if QUIC setup fails;
        /// `quic` forces QUIC (no fallback); `webrtc` forces the proven
        /// WebRTC DataChannel path. Server-side QUIC negotiation is
        /// deployed and gates on the agent's reported version, so `auto`
        /// only attempts QUIC against agents that actually support it.
        #[arg(long, value_enum, default_value = "auto")]
        transport: CliTransport,
    },
    /// Run a local SOCKS5 proxy ("userspace mode"): apps point at
    /// `127.0.0.1:<local>` and each connection's SOCKS5 CONNECT target is dialed
    /// by the named agent over the tunnel. Needs NO OS routing, so it works on
    /// strict full-tunnel corporate VPNs (Check Point, etc.) where the L3 overlay
    /// can't win the routing table. Same server policy + agent allowlist as
    /// `forward`. Stays in the foreground; Ctrl-C tears down. TCP CONNECT only.
    Socks5 {
        /// Hex `agent_id` of the target agent. OMIT for **mesh mode**: one proxy
        /// reaches the whole tenant, addressing an agent by its 24-hex id as the
        /// SOCKS hostname (`--socks5-hostname <agent-id>:<port>`).
        #[arg(long)]
        agent: Option<String>,
        /// Local TCP port for the SOCKS5 listener (bound to 127.0.0.1).
        #[arg(long)]
        local: u16,
        /// Data-plane transport — same semantics as `forward` (`auto` prefers
        /// QUIC with WebRTC-DC fallback).
        #[arg(long, value_enum, default_value = "auto")]
        transport: CliTransport,
    },
    /// Read a multi-forward config from disk and run all forwards as
    /// persistent listeners. Auto-reconnects on transient failure.
    /// T3 implements the operability layer; not yet wired.
    Run {},
    /// Probe path-MTU, ICE candidate reachability, and TURN-relay status
    /// against the named agent. Prints a structured diagnostic dump.
    /// T3 implementation.
    Diagnose {
        #[arg(long)]
        agent: Option<String>,
    },
    /// Check for a newer roomler-tunnel release and self-replace the running
    /// binary (Windows). Mirrors `roomler-agent self-update`.
    SelfUpdate {
        /// Only check + report whether an update is available; don't install.
        #[arg(long)]
        check: bool,
    },
    /// Show the local daemon's node state (id, version, mode, overlay IP,
    /// server connection) over the LocalAPI. Read-only; needs no config/token —
    /// the OS pipe/socket ACL is the trust boundary.
    Status {
        #[command(flatten)]
        fmt: OutputFmt,
    },
    /// List the peers the local daemon currently sees, with each peer's live
    /// connection type (direct / relay / tunnel / blocked / offline).
    Peers {
        #[command(flatten)]
        fmt: OutputFmt,
    },
    /// List the local daemon's active forwards / SOCKS5 listeners + throughput.
    /// (Empty until the tunnel data plane folds into the daemon — P3b.)
    Flows {
        #[command(flatten)]
        fmt: OutputFmt,
    },
    /// ICMP-ping an overlay peer (by name or IP) over the userspace netstack —
    /// the OS-free reachability probe. Only meaningful when the local daemon runs
    /// in netstack mode (a locked-down host with no OS route to the mesh).
    Ping {
        /// Overlay peer to ping — a name (e.g. `neo16`) or an overlay IP
        /// (either family; `fd72:6f6f:6d6c::<v4>` is the derived overlay IPv6).
        target: String,
        /// Round-trip timeout in milliseconds.
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
        /// Ping the peer's derived overlay IPv6 instead of its IPv4 (name
        /// targets only; a literal IP already picks its own family).
        #[arg(short = '6', long = "ipv6")]
        prefer_v6: bool,
        #[command(flatten)]
        fmt: OutputFmt,
    },
}

/// Shared output flag for the read-only LocalAPI verbs (`status`/`peers`/
/// `flows`). `--json` emits the raw wire structs for scripting; omitted, a
/// human table is printed. Meaningless on `enroll`/`forward`, so it's flattened
/// only onto the three read verbs rather than made global.
#[derive(Debug, Args)]
struct OutputFmt {
    /// Emit raw JSON instead of a human-readable table.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Enroll {
            server,
            token,
            name,
        } => enroll_cmd(cli.config, server, token, name).await,
        Command::Forward {
            agent,
            local,
            remote,
            transport,
        } => {
            let cfg = config::load(cli.config).context("loading tunnel config")?;
            forward::run(cfg, &agent, local, &remote, transport.into()).await
        }
        Command::Socks5 {
            agent,
            local,
            transport,
        } => {
            let cfg = config::load(cli.config).context("loading tunnel config")?;
            let transport = transport.into();
            match agent {
                Some(agent) => forward::run_socks5(cfg, &agent, local, transport).await,
                None => mesh::run_mesh(cfg, local, transport).await,
            }
        }
        Command::Run {} => bail!("T3: multi-forward `run` not yet wired"),
        Command::Diagnose { agent } => {
            bail!("T3: diagnose not yet wired (agent={:?})", agent);
        }
        Command::SelfUpdate { check } => {
            let cfg = config::load(cli.config).context("loading tunnel config")?;
            update::self_update(&cfg, check).await
        }
        // Read-only LocalAPI verbs: no config/token — talk straight to the
        // local daemon over its ACL-gated pipe/socket.
        Command::Status { fmt } => localclient::status(fmt.json).await,
        Command::Peers { fmt } => localclient::peers(fmt.json).await,
        Command::Flows { fmt } => localclient::flows(fmt.json).await,
        Command::Ping {
            target,
            timeout_ms,
            prefer_v6,
            fmt,
        } => localclient::ping(&target, timeout_ms, prefer_v6, fmt.json).await,
    }
}

/// `roomler-tunnel enroll` — exchange a tunnel-enrollment JWT for a
/// long-lived `TunnelClient` JWT via the server, then persist server
/// URL + token to the local config file.
async fn enroll_cmd(
    cfg_path: Option<PathBuf>,
    server_url: String,
    enrollment_token: String,
    machine_name: String,
) -> Result<()> {
    let enroll_url = format!(
        "{}/api/tunnel-client/enroll",
        server_url.trim_end_matches('/')
    );
    let body = serde_json::json!({
        "enrollment_token": enrollment_token,
        "machine_name": machine_name,
        "machine_id": derive_machine_id(&machine_name),
        // The server's TunnelEnrollRequest requires `os` (OsKind,
        // snake_case) + `client_version`. `std::env::consts::OS` already
        // yields the exact wire values ("windows" / "linux" / "macos").
        // Without these the enroll fails with HTTP 422 "missing field".
        "os": std::env::consts::OS,
        "client_version": env!("CARGO_PKG_VERSION"),
    });
    let resp = reqwest::Client::new()
        .post(&enroll_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {enroll_url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("enrollment failed (HTTP {status}): {body}");
    }
    #[derive(serde::Deserialize)]
    struct EnrollResponse {
        tunnel_client_token: String,
    }
    let parsed: EnrollResponse = resp.json().await.context("parsing enrollment response")?;

    let cfg = config::TunnelConfig {
        server_url,
        tunnel_client_token: parsed.tunnel_client_token,
        machine_name,
    };
    let path = config::save(&cfg, cfg_path.as_deref()).context("saving tunnel config")?;
    println!("Enrolled. Config written to {}", path.display());
    Ok(())
}

/// Hash `machine_name` + the hostname to a hex-encoded blob the server
/// uses to dedupe re-enrollments from the same host. Mirrors the
/// agent's `derive_machine_id` shape (lowercase 16-hex of the SHA-256).
fn derive_machine_id(machine_name: &str) -> String {
    use sha2::{Digest, Sha256};
    let hostname = hostname_lossy();
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let mut h = Sha256::new();
    h.update(machine_name.as_bytes());
    h.update(b"\0");
    h.update(hostname.as_bytes());
    h.update(b"\0");
    h.update(os.as_bytes());
    h.update(b"\0");
    h.update(arch.as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..16])
}

fn hostname_lossy() -> String {
    #[cfg(unix)]
    {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        "unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_forward() {
        let cli = Cli::try_parse_from([
            "roomler-tunnel",
            "forward",
            "--agent",
            "507f1f77bcf86cd799439011",
            "--local",
            "5432",
            "--remote",
            "10.0.0.5:5432",
        ])
        .unwrap();
        match cli.command {
            Command::Forward {
                agent,
                local,
                remote,
                transport,
            } => {
                assert_eq!(agent, "507f1f77bcf86cd799439011");
                assert_eq!(local, 5432);
                assert_eq!(remote, "10.0.0.5:5432");
                // No --transport given → default is auto (prefer QUIC,
                // fall back to WebRTC on setup failure).
                assert_eq!(transport, CliTransport::Auto);
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn parses_forward_transport_quic() {
        let cli = Cli::try_parse_from([
            "roomler-tunnel",
            "forward",
            "--agent",
            "507f1f77bcf86cd799439011",
            "--local",
            "5432",
            "--remote",
            "10.0.0.5:5432",
            "--transport",
            "quic",
        ])
        .unwrap();
        match cli.command {
            Command::Forward { transport, .. } => {
                assert_eq!(transport, CliTransport::Quic);
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn parses_forward_transport_auto() {
        let cli = Cli::try_parse_from([
            "roomler-tunnel",
            "forward",
            "--agent",
            "507f1f77bcf86cd799439011",
            "--local",
            "5432",
            "--remote",
            "10.0.0.5:5432",
            "--transport",
            "auto",
        ])
        .unwrap();
        match cli.command {
            Command::Forward { transport, .. } => {
                assert_eq!(transport, CliTransport::Auto);
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn parses_socks5() {
        let cli = Cli::try_parse_from([
            "roomler-tunnel",
            "socks5",
            "--agent",
            "507f1f77bcf86cd799439011",
            "--local",
            "1080",
        ])
        .unwrap();
        match cli.command {
            Command::Socks5 {
                agent,
                local,
                transport,
            } => {
                assert_eq!(agent.as_deref(), Some("507f1f77bcf86cd799439011"));
                assert_eq!(local, 1080);
                assert_eq!(transport, CliTransport::Auto);
            }
            other => panic!("expected Socks5, got {other:?}"),
        }
    }

    #[test]
    fn parses_socks5_mesh_without_agent() {
        let cli = Cli::try_parse_from(["roomler-tunnel", "socks5", "--local", "1080"]).unwrap();
        match cli.command {
            Command::Socks5 { agent, local, .. } => {
                assert_eq!(agent, None); // mesh mode
                assert_eq!(local, 1080);
            }
            other => panic!("expected Socks5, got {other:?}"),
        }
    }

    #[test]
    fn parses_enroll() {
        let cli = Cli::try_parse_from([
            "roomler-tunnel",
            "enroll",
            "--server",
            "https://roomler.ai",
            "--token",
            "eyJhbGciOiJIUzI1NiJ9.payload.sig",
            "--name",
            "goran-laptop",
        ])
        .unwrap();
        match cli.command {
            Command::Enroll {
                server,
                token,
                name,
            } => {
                assert_eq!(server, "https://roomler.ai");
                assert_eq!(token, "eyJhbGciOiJIUzI1NiJ9.payload.sig");
                assert_eq!(name, "goran-laptop");
            }
            other => panic!("expected Enroll, got {other:?}"),
        }
    }

    #[test]
    fn derive_machine_id_is_deterministic() {
        let a = derive_machine_id("test-name");
        let b = derive_machine_id("test-name");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32); // 16 bytes hex-encoded
    }

    #[test]
    fn derive_machine_id_changes_with_name() {
        assert_ne!(derive_machine_id("a"), derive_machine_id("b"));
    }

    #[test]
    fn parses_status_peers_flows() {
        for verb in ["status", "peers", "flows"] {
            let cli = Cli::try_parse_from(["roomler-tunnel", verb]).unwrap();
            match (verb, cli.command) {
                ("status", Command::Status { fmt }) => assert!(!fmt.json),
                ("peers", Command::Peers { fmt }) => assert!(!fmt.json),
                ("flows", Command::Flows { fmt }) => assert!(!fmt.json),
                (v, other) => panic!("verb {v} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn parses_status_json_flag() {
        let cli = Cli::try_parse_from(["roomler-tunnel", "status", "--json"]).unwrap();
        match cli.command {
            Command::Status { fmt } => assert!(fmt.json),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn json_flag_is_not_global() {
        // A fully-valid `forward` + `--json` must fail: `--json` belongs only to
        // the read verbs (flattened there), not globally.
        let r = Cli::try_parse_from([
            "roomler-tunnel",
            "forward",
            "--agent",
            "507f1f77bcf86cd799439011",
            "--local",
            "5432",
            "--remote",
            "10.0.0.5:5432",
            "--json",
        ]);
        assert!(r.is_err(), "--json must not be accepted on `forward`");
    }
}
