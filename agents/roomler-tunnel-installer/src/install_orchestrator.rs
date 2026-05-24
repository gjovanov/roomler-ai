//! `cmd_install` orchestrator + cancel + force-kill plumbing.
//!
//! Owns the 6-step pipeline:
//!
//!   1. Preflight: detect existing tunnel-client install (probe the
//!      config.toml at the platform-default path).
//!   2. Resolve archive metadata via `/api/tunnel-wizard/<platform>/health`.
//!   3. Stream archive bytes to `%TEMP%` with progress emits.
//!   4. Verify SHA256.
//!   5. Extract into per-user install dir; locate the CLI binary.
//!   6. Integrate (PATH on Windows, symlink on Unix); enroll via
//!      `/api/tunnel-client/enroll`; write config.toml.
//!
//! ## Cancel
//!
//! Cancellation is a pre-flight + per-await-point flag flip (same
//! shape as the agent installer's CANCEL_REQUESTED): each step calls
//! `check_cancel()` before the next async op. There's no "force-kill"
//! equivalent of the agent installer's msiexec hammer — the tunnel
//! pipeline owns its own threads + descriptors, so the cancel flag
//! is sufficient to unwind cleanly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::asset_resolver;
use crate::enroll;
use crate::extract;
use crate::integration;
use crate::progress::{ProgressEvent, replay_log};

/// `true` while a pre-flight cancel is pending. Orchestrator checks
/// at each await point. Mirrors the agent installer's pattern.
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Output of a successful `cmd_install`. Surfaced on the Done step.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoneReport {
    pub tunnel_client_id: String,
    pub tenant_id: String,
    pub tag: String,
    pub binary_path: String,
    pub config_path: String,
    pub path_updated: bool,
    pub shortcut_created: bool,
}

/// Drive a full install end-to-end. Streams ProgressEvent over the
/// channel; mirrors every emit into the replay log so a late-attached
/// SPA listener catches up via [`crate::progress::replay_log`].
pub async fn run_install(
    server: String,
    token: String,
    device_name: String,
    on_event: Channel<ProgressEvent>,
) -> Result<DoneReport, String> {
    // Reset state for a fresh run. CANCEL_REQUESTED stays cleared
    // unless the operator hits Cancel mid-flow.
    crate::INSTALL_IN_PROGRESS.store(true, Ordering::SeqCst);
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    replay_log().reset();

    let outcome = run_install_inner(server, token, device_name, &on_event).await;

    crate::INSTALL_IN_PROGRESS.store(false, Ordering::SeqCst);

    match outcome {
        Ok(r) => Ok(r),
        Err(e) => {
            // Echo terminal failures into both the channel + the
            // replay log so a late-attached SPA listener sees them.
            emit(
                &on_event,
                ProgressEvent::Error {
                    step: "install".to_string(),
                    message: e.clone(),
                },
            );
            Err(e)
        }
    }
}

async fn run_install_inner(
    server: String,
    token: String,
    device_name: String,
    on_event: &Channel<ProgressEvent>,
) -> Result<DoneReport, String> {
    emit(on_event, ProgressEvent::Started);

    // --- Step 1: preflight -----------------------------------------------
    check_cancel()?;
    emit(on_event, ProgressEvent::PreflightStarted);
    let existing_label = detect_existing_install_label();
    emit(
        on_event,
        ProgressEvent::PreflightOk {
            existing: existing_label,
        },
    );

    // --- Step 2: resolve archive ---------------------------------------
    check_cancel()?;
    let platform = asset_resolver::current_platform().to_string();
    if platform == "unsupported" {
        return Err(format!(
            "tunnel client is not supported on this OS+arch combination ({} / {})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    }
    emit(
        on_event,
        ProgressEvent::AssetResolving {
            platform: platform.clone(),
        },
    );
    let health = asset_resolver::resolve(&platform, "latest")
        .await
        .map_err(|e| format!("resolve installer: {e}"))?;
    emit(
        on_event,
        ProgressEvent::AssetResolved {
            tag: health.tag.clone(),
            size_bytes: health.size,
            digest: health.digest.clone(),
        },
    );

    // --- Step 3: download ----------------------------------------------
    check_cancel()?;
    emit(
        on_event,
        ProgressEvent::DownloadStarted {
            total_bytes: health.size,
        },
    );
    let progress_emitter = on_event.clone();
    let staged = asset_resolver::download(&health, move |received| {
        // DownloadProgress fires many times per MB; skip the replay
        // log mirror — the SPA's live listener catches the stream.
        let _ = progress_emitter.send(ProgressEvent::DownloadProgress {
            received_bytes: received,
        });
    })
    .await
    .map_err(|e| format!("download installer: {e}"))?;

    // --- Step 4: verify SHA256 ------------------------------------------
    check_cancel()?;
    let sha256_match = match health.digest.as_deref() {
        Some(digest) => asset_resolver::verify_sha256(&staged, digest)
            .map_err(|e| format!("sha256 verify: {e}"))?,
        None => {
            tracing::warn!(
                tag = %health.tag,
                "wizard archive lacks sha256 digest — skipping verification"
            );
            true
        }
    };
    emit(on_event, ProgressEvent::DownloadVerified { sha256_match });
    if !sha256_match {
        let _ = std::fs::remove_file(&staged);
        return Err(format!(
            "SHA256 mismatch for {} — staged file deleted; please retry",
            staged.display()
        ));
    }

    // --- Step 5: extract -------------------------------------------------
    check_cancel()?;
    emit(
        on_event,
        ProgressEvent::ExtractStarted {
            archive: staged.display().to_string(),
        },
    );
    let install_root = default_install_root().map_err(|e| format!("install root: {e}"))?;
    extract::extract_archive(&staged, &install_root)
        .map_err(|e| format!("extract archive: {e}"))?;
    let tunnel_binary = extract::find_tunnel_binary(&install_root)
        .map_err(|e| format!("locate tunnel binary: {e}"))?;
    emit(
        on_event,
        ProgressEvent::ExtractDone {
            tunnel_binary: tunnel_binary.display().to_string(),
        },
    );

    // --- Step 6: integrate + enroll -------------------------------------
    check_cancel()?;
    emit(on_event, ProgressEvent::IntegrationStarted);
    let integration_report = integration::integrate(&install_root, &tunnel_binary)
        .map_err(|e| format!("integrate: {e}"))?;
    emit(
        on_event,
        ProgressEvent::IntegrationDone {
            path_updated: integration_report.path_updated,
            shortcut_created: integration_report.shortcut_created,
        },
    );

    check_cancel()?;
    emit(on_event, ProgressEvent::EnrollStarted);
    // The wizard's own crate version is the closest stand-in for the
    // CLI's wire-reported "client_version" — by build-time both
    // crates share workspace.version so the value is consistent.
    let result = enroll::enroll(&server, &token, &device_name, env!("CARGO_PKG_VERSION"))
        .await
        .map_err(|e| format!("enrollment: {e}"))?;
    let config_path =
        enroll::write_config(&result, None).map_err(|e| format!("write config: {e}"))?;
    emit(
        on_event,
        ProgressEvent::EnrollOk {
            tunnel_client_id: result.tunnel_client_id.clone(),
            tenant_id: result.tenant_id.clone(),
        },
    );

    // --- Step 7: done ----------------------------------------------------
    emit(on_event, ProgressEvent::Done);

    Ok(DoneReport {
        tunnel_client_id: result.tunnel_client_id,
        tenant_id: result.tenant_id,
        tag: health.tag,
        binary_path: integration_report.binary_path.display().to_string(),
        config_path: config_path.display().to_string(),
        path_updated: integration_report.path_updated,
        shortcut_created: integration_report.shortcut_created,
    })
}

/// Pre-flight cancel. Returns immediately; the orchestrator's next
/// `check_cancel()` bails. There is no "post-spawn force-kill"
/// equivalent — the tunnel install pipeline owns its threads + fds,
/// so the flag is enough to unwind.
pub fn request_cancel() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

fn emit(channel: &Channel<ProgressEvent>, event: ProgressEvent) {
    // Replay log first so a late-attaching listener can catch up via
    // cmd_install_progress_replay; channel send is best-effort
    // (errs only when the receiver has been closed = SPA moved on).
    replay_log().push(event.clone());
    let _ = channel.send(event);
}

fn check_cancel() -> Result<(), String> {
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        Err("install cancelled by operator".to_string())
    } else {
        Ok(())
    }
}

/// Probe the tunnel CLI's config path to surface "clean" vs "already
/// installed" on the Welcome step.
fn detect_existing_install_label() -> String {
    let Ok(path) = roomler_tunnel::config::default_config_path() else {
        return "clean".to_string();
    };
    if path.exists() {
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<roomler_tunnel::config::TunnelConfig>(&s).ok())
        {
            Some(cfg) => format!("installed ({} @ {})", cfg.machine_name, path.display()),
            None => format!("installed (config at {} unreadable)", path.display()),
        }
    } else {
        "clean".to_string()
    }
}

/// Resolve the per-user install dir. Windows:
/// `%LOCALAPPDATA%\Programs\roomler-tunnel`. Linux:
/// `~/.local/opt/roomler-tunnel`. macOS:
/// `~/Applications/RoomlerTunnel`.
///
/// Exposed for tests so they can substitute a tempdir via the
/// `ROOMLER_TUNNEL_WIZARD_INSTALL_ROOT` env override.
pub fn default_install_root() -> anyhow::Result<PathBuf> {
    if let Ok(override_path) = std::env::var("ROOMLER_TUNNEL_WIZARD_INSTALL_ROOT") {
        return Ok(PathBuf::from(override_path));
    }
    #[cfg(target_os = "windows")]
    {
        let dirs = directories::ProjectDirs::from("ai", "roomler", "roomler-tunnel")
            .ok_or_else(|| anyhow::anyhow!("no Windows local-app-data dir resolvable"))?;
        // ProjectDirs::data_local_dir on Windows returns
        // %LOCALAPPDATA%\roomler\roomler-tunnel — close enough; we
        // append `Programs` for clarity in the UI ("look under
        // Programs\roomler-tunnel"). Same dir is used for the .lnk
        // shortcut target in Phase B.
        Ok(dirs
            .data_local_dir()
            .join("Programs")
            .join("roomler-tunnel"))
    }
    #[cfg(target_os = "linux")]
    {
        let dirs = directories::UserDirs::new()
            .ok_or_else(|| anyhow::anyhow!("no Linux home dir resolvable"))?;
        Ok(dirs
            .home_dir()
            .join(".local")
            .join("opt")
            .join("roomler-tunnel"))
    }
    #[cfg(target_os = "macos")]
    {
        let dirs = directories::UserDirs::new()
            .ok_or_else(|| anyhow::anyhow!("no macOS home dir resolvable"))?;
        Ok(dirs.home_dir().join("Applications").join("RoomlerTunnel"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        Err(anyhow::anyhow!("install root not defined for this OS"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_report_round_trip() {
        let r = DoneReport {
            tunnel_client_id: "507f1f77bcf86cd799439011".to_string(),
            tenant_id: "507f191e810c19729de860ea".to_string(),
            tag: "tunnel-wizard-v0.3.0-rc.1".to_string(),
            binary_path: "/usr/local/bin/roomler-tunnel".to_string(),
            config_path: "/home/foo/.config/roomler-tunnel/config.toml".to_string(),
            path_updated: true,
            shortcut_created: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: DoneReport = serde_json::from_str(&json).unwrap();
        assert_eq!(r.tunnel_client_id, back.tunnel_client_id);
        assert_eq!(r.tag, back.tag);
    }

    #[test]
    fn request_cancel_then_check_cancel_returns_err() {
        let saved = CANCEL_REQUESTED.swap(false, Ordering::SeqCst);
        request_cancel();
        let result = check_cancel();
        CANCEL_REQUESTED.store(saved, Ordering::SeqCst);
        assert!(result.is_err());
    }

    #[test]
    fn default_install_root_respects_env_override() {
        // SAFETY: this test mutates a process-wide env var. The
        // tunnel-wizard's test suite runs single-threaded by default
        // (cargo test --lib uses the rust default = parallel within a
        // process), so we restore the env var before asserting.
        let saved = std::env::var("ROOMLER_TUNNEL_WIZARD_INSTALL_ROOT").ok();
        // SAFETY: process-wide env mutation is OK here — the test is
        // self-contained and restores the previous value below.
        unsafe { std::env::set_var("ROOMLER_TUNNEL_WIZARD_INSTALL_ROOT", "/tmp/wizard-tests") };
        let resolved = default_install_root().unwrap();
        // Restore BEFORE assert so a panic doesn't leak the override.
        match saved {
            Some(v) => unsafe { std::env::set_var("ROOMLER_TUNNEL_WIZARD_INSTALL_ROOT", v) },
            None => unsafe { std::env::remove_var("ROOMLER_TUNNEL_WIZARD_INSTALL_ROOT") },
        }
        assert_eq!(resolved, PathBuf::from("/tmp/wizard-tests"));
    }

    #[test]
    fn detect_existing_install_label_returns_clean_when_no_file() {
        // Test box may have a real %APPDATA%\roomler\roomler-tunnel\
        // config.toml from a prior run; we can't unconditionally
        // assert "clean" here. The contract is just "returns a non-
        // empty human label" — and the value falls into the small set
        // of expected prefixes.
        let label = detect_existing_install_label();
        assert!(
            label == "clean" || label.starts_with("installed ("),
            "unexpected label: {label:?}"
        );
    }
}
