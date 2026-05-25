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

/// Parsed view of an enrollment token. NEVER carries the raw token
/// bytes back (H5).
///
/// rc.60: The "audience match" gate was previously checking `aud ==
/// "tunnel-enrollment"` but the Roomler server issues JWTs with a
/// custom `token_type` claim (snake-case `tunnel_enrollment`) and NO
/// `aud` claim at all (see crates/services/src/auth/mod.rs::
/// TokenType, `#[serde(rename_all = "snake_case")]`). The check now
/// reads `token_type` instead and the SPA renders that field
/// alongside `audience` (which stays None for these tokens).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenValidation {
    pub issuer: Option<String>,
    pub audience: Option<String>,
    /// Server-emitted `token_type` claim (snake_case enum value,
    /// e.g. `"tunnel_enrollment"`). The SPA renders this in the
    /// info card and the gating logic checks it for
    /// `"tunnel_enrollment"`.
    pub token_type: Option<String>,
    pub subject: Option<String>,
    pub jti: Option<String>,
    pub expires_at_unix: Option<i64>,
    /// `true` when `exp <= now` or `exp` is missing entirely.
    pub appears_expired: bool,
    /// `true` when `token_type == "tunnel_enrollment"`. Catches
    /// "wrong token copied" mistakes (e.g. operator pasted the agent-
    /// enrollment token from Admin → Agents instead of the tunnel-
    /// enrollment one from Admin → Tunnels) before the POST.
    ///
    /// Field name kept as `audience_matches` for SPA backward-
    /// compat — older builds of the SPA bind to `audienceMatches`.
    /// The semantics are now "the token's TYPE matches what the
    /// wizard wants" regardless of which JWT claim carries it.
    pub audience_matches: bool,
}

/// Introspect a JWT WITHOUT verifying the signature. Used by the
/// Token step to show "Issuer / Token type / Expires in N min".
///
/// Two passes:
///   1. `jwt_introspect::parse_unverified` for the standard claims
///      the agent crate's helper exposes.
///   2. An in-line base64+JSON parse of the payload to fish out the
///      custom `token_type` claim (the helper doesn't expose it).
///      Best-effort: a missing/unparseable `token_type` leaves the
///      field None and the audience-matches gate stays false (= SPA
///      shows the warning + Continue disabled).
#[tauri::command]
pub fn cmd_validate_token(token: String) -> Result<TokenValidation, String> {
    use roomler_agent::jwt_introspect::{is_likely_expired, parse_unverified};
    let view = parse_unverified(&token).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let token_type = parse_token_type(&token);
    let audience_matches = token_type.as_deref() == Some("tunnel_enrollment");
    Ok(TokenValidation {
        appears_expired: is_likely_expired(&view, now),
        audience_matches,
        issuer: view.issuer,
        audience: view.audience,
        token_type,
        subject: view.subject,
        jti: view.jti,
        expires_at_unix: view.expires_at_unix,
    })
}

/// Decode the JWT's middle segment and return its `token_type` custom
/// claim. Returns None for any failure path (malformed token, bad
/// base64, non-JSON payload, missing claim, non-string claim) — the
/// caller treats that as "audience-matches = false" which keeps the
/// SPA's gate closed.
fn parse_token_type(token: &str) -> Option<String> {
    use base64::Engine;
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("token_type")
        .and_then(|v| v.as_str())
        .map(str::to_string)
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
            issuer: Some("roomler2".to_string()),
            audience: None,
            token_type: Some("tunnel_enrollment".to_string()),
            subject: Some("507f1f77bcf86cd799439011".to_string()),
            jti: Some("abc".to_string()),
            expires_at_unix: Some(1_700_000_000),
            appears_expired: false,
            audience_matches: true,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("audienceMatches"));
        assert!(json.contains("expiresAtUnix"));
        assert!(json.contains("appearsExpired"));
        // The field name on the wire is `tokenType` (camelCase) so the
        // SPA's `view.tokenType` access lands on a string, not undefined.
        assert!(
            json.contains(r#""tokenType":"tunnel_enrollment""#),
            "{json}"
        );
    }

    #[test]
    fn parse_token_type_extracts_tunnel_enrollment() {
        // Synthetic JWT mirroring the server's emitted shape: snake-case
        // `token_type` claim, NO `aud`. Header + signature are placeholder
        // strings (parse_token_type doesn't validate them).
        //
        // Payload JSON: {"sub":"x","tenant_id":"y","token_type":"tunnel_enrollment"}
        // base64url-encoded (no pad).
        let header = "eyJhbGciOiJIUzI1NiJ9";
        let payload =
            "eyJzdWIiOiJ4IiwidGVuYW50X2lkIjoieSIsInRva2VuX3R5cGUiOiJ0dW5uZWxfZW5yb2xsbWVudCJ9";
        let sig = "sig";
        let token = format!("{header}.{payload}.{sig}");
        assert_eq!(
            parse_token_type(&token).as_deref(),
            Some("tunnel_enrollment")
        );
    }

    #[test]
    fn parse_token_type_returns_none_for_missing_claim() {
        // Payload JSON: {"sub":"x"} — no token_type.
        let header = "eyJhbGciOiJIUzI1NiJ9";
        let payload = "eyJzdWIiOiJ4In0";
        let sig = "sig";
        let token = format!("{header}.{payload}.{sig}");
        assert_eq!(parse_token_type(&token), None);
    }

    #[test]
    fn parse_token_type_returns_none_for_malformed_token() {
        assert_eq!(parse_token_type("not-a-jwt"), None);
        assert_eq!(parse_token_type("only.two"), None);
        assert_eq!(parse_token_type("ey!!.payload.sig"), None);
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
