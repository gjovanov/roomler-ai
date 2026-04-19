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
use roomler_agent::{config, encode, enrollment, machine, signaling};
use std::path::PathBuf;
use std::str::FromStr;
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
    Run {
        /// Override the config's `encoder_preference`. One of:
        /// `auto` (default — picks HW on Windows, SW elsewhere),
        /// `hardware` (force MF; falls back to SW only on init failure),
        /// `software` (force openh264). Also honours the
        /// `ROOMLER_AGENT_ENCODER` env var.
        #[arg(long)]
        encoder: Option<String>,
    },
    /// Smoke-test the encoder cascade: open the preferred encoder at
    /// a small resolution, feed 10 synthetic frames, assert at least
    /// one IDR output. Exits non-zero if no encoder could be opened or
    /// no keyframe was produced. Used in the release CI smoke check
    /// to catch regressions in the MF init path before shipping.
    EncoderSmoke {
        /// Encoder preference for the test. Defaults to `hardware` so
        /// the CI exercise actually verifies the MF path.
        #[arg(long, default_value = "hardware")]
        encoder: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config_path = match cli.config.clone() {
        Some(p) => p,
        None => config::default_config_path().context("resolving default config path")?,
    };

    match cli.command.unwrap_or(Command::Run { encoder: None }) {
        Command::Enroll { server, token, name } => {
            enroll_cmd(&config_path, &server, &token, &name).await
        }
        Command::Run { encoder } => run_cmd(&config_path, encoder.as_deref()).await,
        Command::EncoderSmoke { encoder } => encoder_smoke_cmd(&encoder).await,
    }
}

/// Resolution order for `encoder_preference`: CLI flag → env var
/// `ROOMLER_AGENT_ENCODER` → config file field → default (Auto).
/// Invalid values fall through to Auto with a warning, so a typo can't
/// prevent the agent from starting.
fn resolve_encoder_preference(
    cli: Option<&str>,
    cfg_field: config::EncoderPreferenceChoice,
) -> encode::EncoderPreference {
    let from_str = |s: &str, src: &str| match encode::EncoderPreference::from_str(s) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(%e, source = src, "ignoring bad encoder preference");
            None
        }
    };
    if let Some(v) = cli.and_then(|s| from_str(s, "cli")) {
        return v;
    }
    if let Ok(env_val) = std::env::var("ROOMLER_AGENT_ENCODER")
        && let Some(v) = from_str(&env_val, "env")
    {
        return v;
    }
    match cfg_field {
        config::EncoderPreferenceChoice::Auto => encode::EncoderPreference::Auto,
        config::EncoderPreferenceChoice::Hardware => encode::EncoderPreference::Hardware,
        config::EncoderPreferenceChoice::Software => encode::EncoderPreference::Software,
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

async fn run_cmd(config_path: &PathBuf, cli_encoder: Option<&str>) -> Result<()> {
    if !config_path.exists() {
        bail!(
            "no config found at {}. Run `roomler-agent enroll` first.",
            config_path.display()
        );
    }
    let cfg = config::load(config_path).context("loading config")?;
    let encoder_preference = resolve_encoder_preference(cli_encoder, cfg.encoder_preference);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        path = %config_path.display(),
        server = %cfg.server_url,
        agent_id = %cfg.agent_id,
        ?encoder_preference,
        "agent starting"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let sig_task = tokio::spawn({
        let rx = shutdown_rx.clone();
        async move { signaling::run(cfg, encoder_preference, rx).await }
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

/// Open the preferred encoder, feed it 10 synthetic BGRA frames, and
/// assert at least one keyframe comes out. Used in CI to catch MF init
/// regressions before shipping an MSI. Exits with a non-zero code on
/// any failure so a failed smoke check fails the release build.
async fn encoder_smoke_cmd(pref_raw: &str) -> Result<()> {
    use roomler_agent::encode::open_default;
    let pref = encode::EncoderPreference::from_str(pref_raw)
        .map_err(|e| anyhow::anyhow!("bad encoder preference {pref_raw:?}: {e}"))?;
    let w = 640u32;
    let h = 480u32;
    tracing::info!(width = w, height = h, ?pref, "encoder smoke: opening encoder");

    let mut enc = open_default(w, h, pref);
    let backend = enc.name();
    tracing::info!(backend, "encoder smoke: backend selected");

    let mut keyframes = 0usize;
    let mut total_bytes = 0usize;
    for i in 0..10 {
        let mut data = vec![0u8; (w * h * 4) as usize];
        // Alternate solid colours so the encoder has content to encode.
        let (b, g, r) = match i % 3 {
            0 => (255, 0, 0),
            1 => (0, 255, 0),
            _ => (0, 0, 255),
        };
        for px in data.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        let frame = std::sync::Arc::new(roomler_agent::capture::Frame {
            width: w,
            height: h,
            stride: w * 4,
            pixel_format: roomler_agent::capture::PixelFormat::Bgra,
            data,
            monotonic_us: (i as u64) * 33_333,
            monitor: 0,
            dirty_rects: Vec::new(),
        });
        if i == 5 {
            enc.request_keyframe();
        }
        let packets = enc.encode(frame).await?;
        for p in &packets {
            total_bytes += p.data.len();
            if p.is_keyframe {
                keyframes += 1;
            }
        }
    }
    tracing::info!(backend, keyframes, total_bytes, "encoder smoke: done");
    if backend == "noop" {
        bail!("encoder smoke: fell through to NoopEncoder — HW and SW backends both failed");
    }
    if keyframes == 0 {
        bail!("encoder smoke: no keyframes produced (backend={backend})");
    }
    println!("encoder smoke PASSED: backend={backend} keyframes={keyframes} total_bytes={total_bytes}");
    Ok(())
}
