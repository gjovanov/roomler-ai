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
use roomler_agent::{
    config, encode, enrollment, instance_lock, logging, machine, notify, service, signaling,
    updater, watchdog,
};
use std::path::PathBuf;
use std::str::FromStr;

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
    /// Refresh this machine's agent token using a fresh enrollment JWT.
    /// Preserves `server_url` and `machine_name` from the existing
    /// config, so the operator only needs the new token. Used after
    /// an admin revokes the prior token (the `re-enrollment required`
    /// attention sentinel surfaces this case).
    ReEnroll {
        /// Fresh enrollment JWT from the admin UI.
        #[arg(long)]
        token: String,
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
        /// Codec to smoke-test. `h264` (default) or `h265` — HEVC
        /// goes through `open_for_codec` and the MF HEVC cascade.
        /// Accepts `hevc` as an alias.
        #[arg(long, default_value = "h264")]
        codec: String,
    },
    /// Run the capability probe that populates `rc:agent.hello` and
    /// print the result. Useful for verifying what codecs the agent
    /// will actually advertise on this host (the HEVC + AV1 probes
    /// run real MfEncoder activations, so this exits with roughly
    /// the same logs an operator would see in the first session).
    Caps,
    /// Enumerate attached displays and print what the agent will
    /// report in `rc:agent.hello`. Cross-platform via `scrap`.
    Displays,
    /// Manage the auto-start-on-boot hook (Scheduled Task on Windows,
    /// systemd user unit on Linux, LaunchAgent on macOS). Subcommand
    /// is one of `install`, `uninstall`, `status`.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Check GitHub Releases for a newer version and — if found —
    /// download + spawn the installer. The agent exits on successful
    /// spawn so the installer can overwrite the binary; your service
    /// hook re-launches it. Safe to run interactively. Pass
    /// `--check-only` to print the verdict without touching disk.
    SelfUpdate {
        /// Don't download or spawn anything; just report whether an
        /// update is available.
        #[arg(long)]
        check_only: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    /// Register the agent for auto-start on the next login.
    Install,
    /// Remove the auto-start hook. Idempotent.
    Uninstall,
    /// Print the current auto-start status.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();
    if let Some(dir) = logging::log_dir() {
        tracing::debug!(log_dir = %dir.display(), "persistent file logging active");
    }

    let cli = Cli::parse();
    let config_path = match cli.config.clone() {
        Some(p) => p,
        None => config::default_config_path().context("resolving default config path")?,
    };

    match cli.command.unwrap_or(Command::Run { encoder: None }) {
        Command::Enroll {
            server,
            token,
            name,
        } => enroll_cmd(&config_path, &server, &token, &name).await,
        Command::ReEnroll { token } => re_enroll_cmd(&config_path, &token).await,
        Command::Run { encoder } => run_cmd(&config_path, encoder.as_deref()).await,
        Command::EncoderSmoke { encoder, codec } => encoder_smoke_cmd(&encoder, &codec).await,
        Command::Caps => caps_cmd().await,
        Command::Displays => displays_cmd().await,
        Command::Service { action } => service_cmd(action).await,
        Command::SelfUpdate { check_only } => self_update_cmd(check_only).await,
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

async fn re_enroll_cmd(config_path: &PathBuf, enrollment_token: &str) -> Result<()> {
    if !config_path.exists() {
        bail!(
            "no existing config at {}; use `enroll` for first-time setup",
            config_path.display()
        );
    }
    let existing = config::load(config_path).context("loading existing config")?;
    let machine_id = machine::derive_machine_id(config_path);
    tracing::info!(
        %machine_id,
        agent_id = %existing.agent_id,
        machine_name = %existing.machine_name,
        "re-enrolling against existing config"
    );

    let new_cfg = enrollment::enroll(enrollment::EnrollInputs {
        server_url: &existing.server_url,
        enrollment_token,
        machine_id: &machine_id,
        machine_name: &existing.machine_name,
    })
    .await
    .context("re-enrollment failed")?;

    config::save(config_path, &new_cfg).context("saving updated config")?;
    notify::clear_attention();
    println!("Re-enrollment successful. Agent id: {}", new_cfg.agent_id);
    println!("Run `roomler-agent run` (or wait for the supervisor to relaunch) to reconnect.");
    Ok(())
}

async fn run_cmd(config_path: &PathBuf, cli_encoder: Option<&str>) -> Result<()> {
    if !config_path.exists() {
        bail!(
            "no config found at {}. Run `roomler-agent enroll` first.",
            config_path.display()
        );
    }
    // Take the single-instance lock before doing anything else. If
    // another agent is already attached to this config (typically the
    // Scheduled Task / systemd unit launched at logon), exit cleanly
    // instead of fighting it for the WS connection. Only `run` gates
    // on the lock — `enroll`, `service install`, `caps`, `displays`,
    // `encoder-smoke`, `self-update` are intentionally runnable
    // alongside an active agent.
    let _instance_lock = match instance_lock::acquire(config_path)
        .context("acquiring single-instance lock")?
    {
        instance_lock::AcquireOutcome::Acquired(g) => g,
        instance_lock::AcquireOutcome::AlreadyRunning => {
            eprintln!(
                "Another roomler-agent is already running for this config; exiting.\n\
                 (use `roomler-agent service status` to check the auto-start hook,\n\
                 or stop the running instance before starting a new one.)"
            );
            tracing::warn!("single-instance lock held by another process; exiting");
            return Ok(());
        }
    };
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

    // Install the liveness watchdog. Pumps tick after every iteration;
    // the scan loop force-exits via std::process::exit(STALL_EXIT_CODE)
    // when any pump silently stalls past its threshold, relying on
    // the OS supervisor (Win Scheduled Task with RestartOnFailure /
    // systemd Restart=on-failure / launchd KeepAlive) to relaunch.
    // Encoder + capture are registered but gated off until a session
    // attaches — those pumps can legitimately go idle for hours when
    // no controller is connected.
    let wd = watchdog::Watchdog::new();
    wd.register("signaling", std::time::Duration::from_secs(90), true);
    wd.register("encoder", std::time::Duration::from_secs(30), false);
    wd.register("capture", std::time::Duration::from_secs(30), false);
    let _ = watchdog::install(wd.clone());
    watchdog::spawn_thread_watchdog(wd.clone());
    let wd_task = tokio::spawn({
        let wd = wd.clone();
        let rx = shutdown_rx.clone();
        async move { watchdog::run(wd, rx, watchdog::force_exit_on_stall).await }
    });

    let sig_task = tokio::spawn({
        let rx = shutdown_rx.clone();
        async move { signaling::run(cfg, encoder_preference, rx).await }
    });

    // Background auto-updater — checks GitHub Releases on startup and
    // every 6 h. Writes to `shutdown_tx` when a newer version is
    // downloaded and the installer is spawned, so the signalling task
    // tears down cleanly before the running binary gets overwritten.
    // Disable with `ROOMLER_AGENT_AUTO_UPDATE=0` for air-gapped /
    // operator-managed deployments.
    let auto_update_enabled = std::env::var("ROOMLER_AGENT_AUTO_UPDATE")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(true);
    let upd_task = if auto_update_enabled {
        Some(tokio::spawn({
            let rx = shutdown_rx.clone();
            let tx = shutdown_tx.clone();
            async move { updater::run_periodic(rx, tx).await }
        }))
    } else {
        tracing::info!("auto-update disabled via ROOMLER_AGENT_AUTO_UPDATE");
        None
    };

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
    wd_task.abort();
    if let Some(t) = upd_task {
        t.abort();
    }
    Ok(())
}

async fn service_cmd(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => {
            service::install().context("installing auto-start hook")?;
            println!("Auto-start registered. The agent will launch on next login.");
            Ok(())
        }
        ServiceAction::Uninstall => {
            service::uninstall().context("removing auto-start hook")?;
            println!("Auto-start removed.");
            Ok(())
        }
        ServiceAction::Status => {
            let s = service::status().context("querying auto-start status")?;
            println!("Auto-start: {s}");
            Ok(())
        }
    }
}

async fn self_update_cmd(check_only: bool) -> Result<()> {
    let outcome = updater::check_once().await;
    match outcome {
        updater::CheckOutcome::UpToDate { current, latest } => {
            println!("Up to date (current: {current}, latest: {latest})");
            Ok(())
        }
        updater::CheckOutcome::UpdateReady {
            current,
            latest,
            installer_path,
        } => {
            if check_only {
                println!("Update available: {current} -> {latest}");
                println!("(skipping install — --check-only)");
                return Ok(());
            }
            println!(
                "Update available: {current} -> {latest}. Installer at {}. Spawning + exiting.",
                installer_path.display()
            );
            updater::spawn_installer(&installer_path).context("spawning installer")?;
            std::process::exit(0);
        }
        updater::CheckOutcome::Skipped(reason) => {
            println!("Update check skipped: {reason}");
            Ok(())
        }
    }
}

/// Open the preferred encoder, feed it 10 synthetic BGRA frames, and
/// assert at least one keyframe comes out. Used in CI to catch MF init
/// regressions before shipping an MSI. Exits with a non-zero code on
/// any failure so a failed smoke check fails the release build.
async fn encoder_smoke_cmd(pref_raw: &str, codec_raw: &str) -> Result<()> {
    use roomler_agent::encode::{open_default, open_for_codec};
    let pref = encode::EncoderPreference::from_str(pref_raw)
        .map_err(|e| anyhow::anyhow!("bad encoder preference {pref_raw:?}: {e}"))?;
    let w = 640u32;
    let h = 480u32;
    let codec = codec_raw.to_ascii_lowercase();
    tracing::info!(width = w, height = h, ?pref, codec = %codec, "encoder smoke: opening encoder");

    // For H.264 keep the historical `open_default` path (preserves
    // logging + behaviour that CI smoke output is pinned to). For any
    // other codec, go through `open_for_codec` which runs the codec-
    // specific cascade and reports whether a demotion happened.
    let (mut enc, actual_codec) = if codec == "h264" {
        (open_default(w, h, pref), "h264".to_string())
    } else {
        let (e, actual) = open_for_codec(&codec, w, h, pref);
        (e, actual.to_string())
    };
    let backend = enc.name();
    tracing::info!(backend, actual_codec = %actual_codec, "encoder smoke: backend selected");
    if codec != "h264" && actual_codec != codec {
        tracing::warn!(
            requested = %codec,
            actual = %actual_codec,
            "encoder smoke: demoted from requested codec"
        );
    }

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
    println!(
        "encoder smoke PASSED: backend={backend} keyframes={keyframes} total_bytes={total_bytes}"
    );
    Ok(())
}

async fn caps_cmd() -> Result<()> {
    let caps = roomler_agent::encode::caps::detect();
    println!("codecs: {:?}", caps.codecs);
    println!("hw_encoders: {:?}", caps.hw_encoders);
    println!("transports: {:?}", caps.transports);
    println!("has_input_permission: {}", caps.has_input_permission);
    println!("supports_clipboard: {}", caps.supports_clipboard);
    println!("supports_file_transfer: {}", caps.supports_file_transfer);
    println!(
        "max_simultaneous_sessions: {}",
        caps.max_simultaneous_sessions
    );
    Ok(())
}

async fn displays_cmd() -> Result<()> {
    let list = roomler_agent::displays::enumerate();
    println!("displays ({}):", list.len());
    for d in &list {
        println!(
            "  index={} name={:?} {}x{} scale={:.2}{}",
            d.index,
            d.name,
            d.width_px,
            d.height_px,
            d.scale,
            if d.primary { " (primary)" } else { "" }
        );
    }
    Ok(())
}
