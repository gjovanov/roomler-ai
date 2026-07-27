//! S1a PR-A — keep the `roomler-desktop` companion EXE at the daemon's
//! version.
//!
//! The desktop app is in NEITHER MSI flavour: it's a standalone release
//! asset placed beside the daemon by the setup wizard / `install.ps1`
//! (GAP-A), and the auto-updater only ever downloads `.msi` — so every
//! daemon self-update left `roomler-desktop.exe` stale forever (the
//! reported item 1 of the consolidation roadmap). This module runs at
//! daemon startup: when the sibling desktop EXE's recorded version
//! differs from our own `CARGO_PKG_VERSION`, download the matching
//! `roomler-desktop-*` asset from OUR OWN release tag (not latest — the
//! companion tracks the daemon, not the feed), rename-swap it, and
//! restart it if it was running.
//!
//! Version tracking is a sidecar marker (`roomler-desktop.exe.version`)
//! written on every successful swap — no PE version parsing. A
//! wizard-placed EXE with no marker counts as stale and gets swapped
//! once, after which the marker tracks.
//!
//! Rights model: writing next to the daemon needs whatever rights the
//! daemon's own directory needs. SYSTEM contexts (SCM host,
//! SystemContext worker) can write `%ProgramFiles%\Roomler`; a perUser
//! task daemon owns `%LOCALAPPDATA%\Programs\Roomler`. A user-context
//! worker under a plain-SCM perMachine install CANNOT write
//! `%ProgramFiles%` — it logs and skips; the SCM *host* hook covers
//! that flavour. Failures are always skip-and-retry-next-start, never
//! fatal.

#[cfg(target_os = "windows")]
use anyhow::{Context, Result};

/// Who is calling the refresh — decides how a running desktop gets
/// respawned after the swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespawnContext {
    /// Caller runs as LocalSystem (SCM service host / SystemContext
    /// worker): respawn into the active interactive session via
    /// `CreateProcessAsUserW`.
    SystemService,
    /// Caller runs as the interactive user (perUser task daemon,
    /// attended run): plain spawn.
    UserSession,
}

#[cfg(target_os = "windows")]
pub const DESKTOP_EXE: &str = "roomler-desktop.exe";
#[cfg(target_os = "windows")]
const VERSION_MARKER: &str = "roomler-desktop.exe.version";
#[cfg(target_os = "windows")]
const OLD_SUFFIX: &str = "roomler-desktop.exe.old";

/// Entry point — spawn-and-forget from daemon startup. Never fails the
/// caller; every error path logs and returns (retry on next start).
pub async fn refresh_if_stale(respawn: RespawnContext) {
    #[cfg(target_os = "windows")]
    if let Err(e) = refresh_inner(respawn).await {
        tracing::warn!(error = %format!("{e:#}"), "desktop companion refresh skipped");
    }
    #[cfg(not(target_os = "windows"))]
    {
        // The desktop companion ships for Windows only today.
        let _ = respawn;
    }
}

#[cfg(target_os = "windows")]
async fn refresh_inner(respawn: RespawnContext) -> Result<()> {
    let own_version = env!("CARGO_PKG_VERSION");
    let exe_dir = std::env::current_exe()
        .context("locating own exe")?
        .parent()
        .context("own exe has no parent dir")?
        .to_path_buf();
    let dest = exe_dir.join(DESKTOP_EXE);
    let marker = exe_dir.join(VERSION_MARKER);

    // Fresh marker + EXE present → nothing to do (the common case on
    // every startup after the first successful swap).
    let recorded = std::fs::read_to_string(&marker)
        .ok()
        .map(|s| s.trim().to_string());
    if dest.exists() && recorded.as_deref() == Some(own_version) {
        return Ok(());
    }
    tracing::info!(
        own = own_version,
        recorded = recorded.as_deref().unwrap_or("<none>"),
        present = dest.exists(),
        "desktop companion is stale/missing — refreshing"
    );

    // Resolve the desktop asset from OUR OWN release tag. `agent-v<ver>`
    // is the tag scheme release-agent.yml pushes; a dev build whose
    // version was never released just logs a clean skip here.
    let tag = format!("agent-v{own_version}");
    let release = crate::updater::fetch_release_by_tag(&tag)
        .await
        .with_context(|| format!("fetching release {tag}"))?;
    let asset = pick_desktop_asset(&release.assets)
        .with_context(|| format!("no roomler-desktop asset in release {tag}"))?;
    let staged = crate::updater::download_asset(asset)
        .await
        .with_context(|| format!("downloading {}", asset.name))?;

    let was_running = desktop_running();

    // Rename-swap: a RUNNING EXE can be renamed (not overwritten) on
    // Windows, so move the live file aside, copy the new one in, and
    // clean the `.old` afterwards. PermissionDenied here = wrong
    // context (user-context worker on a perMachine install) — skip;
    // the SYSTEM-side hook owns that flavour.
    let old = exe_dir.join(OLD_SUFFIX);
    let _ = std::fs::remove_file(&old);
    if dest.exists() {
        std::fs::rename(&dest, &old).context("renaming running desktop EXE aside")?;
    }
    if let Err(e) = std::fs::copy(&staged, &dest) {
        // Best-effort rollback so the host isn't left with NO desktop.
        if old.exists() {
            let _ = std::fs::rename(&old, &dest);
        }
        return Err(anyhow::Error::new(e).context("copying new desktop EXE into place"));
    }
    if let Err(e) = std::fs::write(&marker, format!("{own_version}\n")) {
        tracing::warn!(error = %e, "could not write desktop version marker");
    }

    if was_running {
        kill_desktop();
        respawn_desktop(respawn, &dest);
    }

    // The old EXE may stay locked for a moment while the killed
    // process dies; a few short retries, then leave it for the next
    // cycle's pre-delete.
    for _ in 0..3 {
        if std::fs::remove_file(&old).is_ok() || !old.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    tracing::info!(
        version = own_version,
        respawned = was_running,
        "desktop companion refreshed"
    );
    Ok(())
}

/// Prefer the signed EXE (no `-unsigned` infix) when both are present.
#[cfg(target_os = "windows")]
fn pick_desktop_asset(
    assets: &[crate::updater::GithubAsset],
) -> Option<&crate::updater::GithubAsset> {
    let is_desktop = |name: &str| {
        let lower = name.to_lowercase();
        lower.starts_with("roomler-desktop-") && lower.ends_with(".exe")
    };
    assets
        .iter()
        .find(|a| is_desktop(&a.name) && !a.name.to_lowercase().contains("-unsigned"))
        .or_else(|| assets.iter().find(|a| is_desktop(&a.name)))
}

/// Is `roomler-desktop.exe` currently running? `tasklist` image-name
/// filter — the image name appears verbatim in the table regardless of
/// UI locale. `pub`: the S1b appdirs migration skips while the desktop
/// runs (it may hold open handles inside the tree being moved).
#[cfg(target_os = "windows")]
pub fn desktop_running() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {DESKTOP_EXE}"), "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(DESKTOP_EXE))
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn kill_desktop() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Abrupt kill is safe for the tray: its state is a thin poll over
    // the LocalAPI, and pending consent prompts are daemon-side
    // sentinels the respawned app re-lists within its 1.5 s poll.
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", DESKTOP_EXE])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(target_os = "windows")]
fn respawn_desktop(respawn: RespawnContext, dest: &std::path::Path) {
    match respawn {
        RespawnContext::UserSession => match std::process::Command::new(dest).spawn() {
            Ok(_) => tracing::info!("desktop companion respawned (user session)"),
            Err(e) => tracing::warn!(error = %e, "desktop companion respawn failed"),
        },
        RespawnContext::SystemService => {
            use crate::win_service::supervisor;
            let Some(session_id) = supervisor::active_console_session_id() else {
                tracing::info!(
                    "no active interactive session; desktop starts on next manual launch"
                );
                return;
            };
            match supervisor::query_user_token(session_id) {
                Ok(Some(token)) => {
                    // SAFETY: `token` is a live user token from
                    // WTSQueryUserToken, held for the duration of the call.
                    match unsafe { supervisor::spawn_in_session(token.raw(), dest, &[]) } {
                        Ok(_) => {
                            tracing::info!(session_id, "desktop companion respawned in session")
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "desktop companion session respawn failed")
                        }
                    }
                }
                Ok(None) => tracing::info!(
                    session_id,
                    "no user token for active session; desktop starts on next login"
                ),
                Err(e) => tracing::warn!(error = %e, "query_user_token failed for desktop respawn"),
            }
        }
    }
}
