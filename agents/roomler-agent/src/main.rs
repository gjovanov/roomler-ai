//! `roomler-agent` — the native remote-control agent for the Roomler AI
//! platform. Runs on the controlled host, connects out to the Roomler API
//! over WSS, and (eventually) serves a WebRTC peer to a browser controller.
//!
//! This v1 is signaling-only: it enrols against a token from an admin,
//! connects the WS, sends `rc:agent.hello`, auto-grants consent, and cleanly
//! declines media until the screen-capture / encode / WebRTC pieces land.
//!
//! CLI:
//!   roomler-agent enroll --server <url> --token <enrollment-jwt> \
//!                        --name "Goran's Laptop" [--config <path>]
//!   roomler-agent run    [--config <path>]

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use roomler_agent::{config, enrollment, machine, signaling};
use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(name = "roomler-agent", version, about, long_about = None)]
struct Cli {
    /// Override config file location. Defaults to the platform config dir.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enroll this machine against a Roomler server using an admin-issued
    /// enrollment token. Writes the resulting agent token to the config file.
    Enroll {
        /// Base URL of the Roomler API (e.g. https://roomler.live).
        #[arg(long)]
        server: String,
        /// Enrollment token, as printed by the admin UI.
        #[arg(long)]
        token: String,
        /// Friendly name shown in the admin agents list.
        #[arg(long)]
        name: String,
    },
    /// Connect to the server and sit in the signaling loop (default command
    /// if none is given).
    Run,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config_path = match cli.config.clone() {
        Some(p) => p,
        None => config::default_config_path().context("resolving default config path")?,
    };

    match cli.command.unwrap_or(Command::Run) {
        Command::Enroll { server, token, name } => {
            enroll_cmd(&config_path, &server, &token, &name).await
        }
        Command::Run => run_cmd(&config_path).await,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("roomler_agent=info,warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

async fn enroll_cmd(
    config_path: &PathBuf,
    server: &str,
    enrollment_token: &str,
    machine_name: &str,
) -> Result<()> {
    let machine_id = machine::derive_machine_id(config_path);
    tracing::info!(%machine_id, "derived machine fingerprint");

    let cfg = enrollment::enroll(enrollment::EnrollInputs {
        server_url: server,
        enrollment_token,
        machine_id: &machine_id,
        machine_name,
    })
    .await
    .context("enrollment failed")?;

    config::save(config_path, &cfg).context("saving config")?;
    tracing::info!(
        path = %config_path.display(),
        agent_id = %cfg.agent_id,
        "enrollment complete"
    );
    println!("Enrollment successful. Agent id: {}", cfg.agent_id);
    println!("Run `roomler-agent run` to connect.");
    Ok(())
}

async fn run_cmd(config_path: &PathBuf) -> Result<()> {
    if !config_path.exists() {
        bail!(
            "no config found at {}. Run `roomler-agent enroll` first.",
            config_path.display()
        );
    }
    let cfg = config::load(config_path).context("loading config")?;
    tracing::info!(
        path = %config_path.display(),
        server = %cfg.server_url,
        agent_id = %cfg.agent_id,
        "agent starting"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let sig_task = tokio::spawn({
        let rx = shutdown_rx.clone();
        async move { signaling::run(cfg, rx).await }
    });

    // Wait for Ctrl-C / SIGTERM.
    tokio::select! {
        res = sig_task => {
            if let Ok(Err(e)) = res {
                tracing::error!(error = %e, "signaling task exited with error");
                return Err(e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown requested");
            let _ = shutdown_tx.send(true);
            // Give the signaling task a short window to flush.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    Ok(())
}
