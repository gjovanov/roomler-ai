//! Tauri invoke handlers for the tunnel installer wizard.
//!
//! Each handler returns `Result<T, String>` so Tauri marshals errors
//! as rejected promises the SPA renders verbatim. The raw enrollment
//! token NEVER flows back from any command — only parsed metadata
//! (H5, same invariant as the agent wizard).

use serde::Serialize;

// ─── Probing / status ─────────────────────────────────────────────────────────

/// Output of `cmd_detect_install`. Flat shape (string discriminator)
/// is what the SPA renders directly — Tauri's default enum
/// serialisation is tagged, which would force the SPA to dig through
/// `result.type === "Installed"` patterns.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectResult {
    /// `"clean" | "installed"`.
    pub kind: String,
    /// When `kind == "installed"`, the recorded machine_name from the
    /// config.toml. None for clean / unreadable.
    pub machine_name: Option<String>,
    /// When `kind == "installed"`, the config.toml path (verbatim
    /// `display()`). None for clean.
    pub config_path: Option<String>,
}

/// Probe the tunnel CLI's config path. Returns clean/installed +
/// metadata for the Welcome step.
#[tauri::command]
pub fn cmd_detect_install() -> Result<DetectResult, String> {
    let Ok(path) = roomler_tunnel::config::default_config_path() else {
        return Ok(DetectResult {
            kind: "clean".to_string(),
            machine_name: None,
            config_path: None,
        });
    };
    if !path.exists() {
        return Ok(DetectResult {
            kind: "clean".to_string(),
            machine_name: None,
            config_path: None,
        });
    }
    let machine_name = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str::<roomler_tunnel::config::TunnelConfig>(&s).ok())
        .map(|c| c.machine_name);
    Ok(DetectResult {
        kind: "installed".to_string(),
        machine_name,
        config_path: Some(path.display().to_string()),
    })
}

/// Hostname (no domain). The wizard pre-fills the device-name input
/// with this so the operator usually just hits Enter. Falls back to
/// `"roomler-laptop"` if the OS won't yield a name.
#[tauri::command]
pub fn cmd_default_device_name() -> String {
    gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "roomler-laptop".to_string())
}

/// Production roomler.ai URL. Operators usually leave this as-is;
/// staging / on-prem deployments edit it.
#[tauri::command]
pub fn cmd_default_server_url() -> String {
    "https://roomler.ai".to_string()
}

/// Parsed view of an enrollment token. Mirrors the agent installer's
/// shape so the SPA template can be near-identical. NEVER carries
/// the raw token bytes back (H5).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenValidation {
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub subject: Option<String>,
    pub jti: Option<String>,
    pub expires_at_unix: Option<i64>,
    /// `true` when `exp <= now` or `exp` is missing entirely.
    pub appears_expired: bool,
    /// `true` when `aud == "tunnel-enrollment"`. The wizard expects
    /// THIS audience specifically — the agent's `Enrollment` audience
    /// won't enroll a tunnel client. Surfacing the mismatch here
    /// catches "wrong token copied" mistakes before the POST.
    pub audience_matches: bool,
}

/// Introspect a JWT WITHOUT verifying the signature. Used by the
/// Token step to show "Issuer / Audience / Expires in N min".
#[tauri::command]
pub fn cmd_validate_token(token: String) -> Result<TokenValidation, String> {
    use roomler_agent::jwt_introspect::{is_likely_expired, parse_unverified};
    let view = parse_unverified(&token).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let audience_matches = view
        .audience
        .as_deref()
        .map(|a| a == "tunnel-enrollment")
        .unwrap_or(false);
    Ok(TokenValidation {
        appears_expired: is_likely_expired(&view, now),
        audience_matches,
        issuer: view.issuer,
        audience: view.audience,
        subject: view.subject,
        jti: view.jti,
        expires_at_unix: view.expires_at_unix,
    })
}

// ─── Wizard state persistence ────────────────────────────────────────────────

/// Load the wizard's persisted state. Returns Default (Welcome step,
/// empty fields) when no state file exists or it's corrupt. Token is
/// never in the state — see `wizard_state` docs.
#[tauri::command]
pub fn cmd_load_state() -> Result<crate::wizard_state::WizardState, String> {
    let path = crate::wizard_state::default_state_path().map_err(|e| e.to_string())?;
    Ok(crate::wizard_state::load(&path))
}

/// Persist wizard state. Called on every form-blur from the SPA so a
/// forced kill mid-flow resumes cleanly on the next launch.
#[tauri::command]
pub fn cmd_save_state(state: crate::wizard_state::WizardState) -> Result<(), String> {
    let path = crate::wizard_state::default_state_path().map_err(|e| e.to_string())?;
    crate::wizard_state::save(&path, &state).map_err(|e| e.to_string())
}

// ─── Install execution ───────────────────────────────────────────────────────

/// Drive the full install pipeline end-to-end. Progress streams over
/// the Tauri `ipc::Channel`; every event is also pushed into the
/// replay log so a late-attaching SPA listener catches up via
/// [`cmd_install_progress_replay`].
///
/// `rename_all = "camelCase"` because the JS SPA calls this with
/// `deviceName` / `onEvent` arg keys; Tauri 2's default is snake_case
/// which would silently leave `device_name` empty.
#[tauri::command(rename_all = "camelCase")]
pub async fn cmd_install(
    server: String,
    token: String,
    device_name: String,
    on_event: tauri::ipc::Channel<crate::progress::ProgressEvent>,
) -> Result<crate::install_orchestrator::DoneReport, String> {
    crate::install_orchestrator::run_install(server, token, device_name, on_event).await
}

/// Pre-flight cancel. Flips the orchestrator's `CANCEL_REQUESTED`
/// flag — subsequent `check_cancel()` calls bail. There's no force-
/// kill equivalent (no msiexec to hammer); the tunnel install owns
/// its threads + fds.
#[tauri::command]
pub fn cmd_cancel_in_progress() -> Result<(), String> {
    crate::install_orchestrator::request_cancel();
    Ok(())
}

/// Snapshot of the ProgressEvent replay log. SPA calls this on first
/// listener attach to fast-forward through any events emitted before
/// its `ipc::Channel` listener wired up.
#[tauri::command]
pub fn cmd_install_progress_replay() -> Vec<crate::progress::ProgressEvent> {
    crate::progress::replay_log().snapshot()
}

/// Cleanly exit the wizard process. Wired to the Done page's Finish
/// button. `AppHandle::exit(0)` is the deterministic shutdown — JS
/// `window.close()` in Tauri 2 has corner cases where the webview
/// blanks but the process lingers.
#[tauri::command]
pub fn cmd_exit_wizard(app: tauri::AppHandle) {
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_result_serialises_camel_case() {
        let r = DetectResult {
            kind: "installed".to_string(),
            machine_name: Some("lap".to_string()),
            config_path: Some("/tmp/cfg.toml".to_string()),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("machineName"));
        assert!(json.contains("configPath"));
        assert!(!json.contains("machine_name"));
    }

    #[test]
    fn token_validation_serialises_camel_case_and_audience_match() {
        let v = TokenValidation {
            issuer: Some("roomler-ai".to_string()),
            audience: Some("tunnel-enrollment".to_string()),
            subject: None,
            jti: None,
            expires_at_unix: None,
            appears_expired: false,
            audience_matches: true,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("audienceMatches"));
        assert!(json.contains("expiresAtUnix"));
        assert!(json.contains("appearsExpired"));
    }

    #[test]
    fn default_device_name_returns_non_empty() {
        let n = cmd_default_device_name();
        assert!(!n.is_empty());
    }

    #[test]
    fn default_server_url_is_roomler_ai() {
        assert_eq!(cmd_default_server_url(), "https://roomler.ai");
    }
}
