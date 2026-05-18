//! `roomler-tunnel` — TeamViewer-style native tunnel client.
//!
//! Forwards a local TCP port (or SOCKS5 listener, v1.1) over a
//! WebRTC P2P data channel to an enrolled `roomler-agent`, which
//! dials the corresponding intranet destination. The Roomler API
//! is signalling-only — payload never touches the server.
//!
//! CLI surface:
//!
//!   roomler-tunnel enroll --server <url> --token <enrollment-jwt> --name <label>
//!   roomler-tunnel forward --agent <agent_id> --local <port> --remote <host:port>
//!   roomler-tunnel run [--config <path>]
//!   roomler-tunnel diagnose [--agent <agent_id>]
//!
//! T1 ships compile-clean scaffolding only — every subcommand returns
//! `not_yet_implemented` until T2 wires the data plane.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "roomler-tunnel", version, about, long_about = None)]
struct Cli {
    /// Override config file location. Defaults to the platform config dir
    /// (`~/.config/roomler-tunnel/config.toml` on Linux, equivalent on
    /// Windows / macOS via the `directories` crate).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enroll this laptop against a Roomler server using an admin-issued
    /// tunnel-enrollment token. Writes the long-lived TunnelClient JWT to
    /// the config file. Mirrors `roomler-agent enroll`.
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
    /// foreground; Ctrl-C tears down. T2 implements the data plane.
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
    },
    /// Read a multi-forward config from disk and run all forwards as
    /// persistent listeners. Auto-reconnects on transient failure.
    /// T3 implements the operability layer; T1 stub returns an error.
    Run {},
    /// Probe path-MTU, ICE candidate reachability, and TURN-relay status
    /// against the named agent. Prints a structured diagnostic dump.
    /// T3 implementation.
    Diagnose {
        #[arg(long)]
        agent: Option<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        match cli.command {
            Command::Enroll {
                server,
                token,
                name,
            } => {
                bail!(
                    "T1: enroll wiring lands in task #5 (HTTP enrollment endpoint). \
                     server={server} name={name} token=<redacted, {} bytes>",
                    token.len()
                );
            }
            Command::Forward {
                agent,
                local,
                remote,
            } => {
                bail!("T2: TCP forward not yet wired. agent={agent} local={local} remote={remote}");
            }
            Command::Run {} => {
                bail!("T3: multi-forward `run` not yet wired");
            }
            Command::Diagnose { agent } => {
                bail!("T3: diagnose not yet wired (agent={:?})", agent);
            }
        }
    })
}
