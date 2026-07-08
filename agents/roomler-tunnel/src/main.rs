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

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use roomler_tunnel::{config, forward, mesh, update};

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
        transport: forward::TransportPref,
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
        transport: forward::TransportPref,
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
            forward::run(cfg, &agent, local, &remote, transport).await
        }
        Command::Socks5 {
            agent,
            local,
            transport,
        } => {
            let cfg = config::load(cli.config).context("loading tunnel config")?;
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
                assert_eq!(transport, forward::TransportPref::Auto);
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
                assert_eq!(transport, forward::TransportPref::Quic);
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
                assert_eq!(transport, forward::TransportPref::Auto);
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
                assert_eq!(transport, forward::TransportPref::Auto);
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
}
