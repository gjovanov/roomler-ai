//! Tauri invoke handlers for the installer wizard.
//!
//! W6 in the rc.28 plan: 12 handlers total. This file ships the
//! read-only + pure-parse subset first (W6a) so the SPA can render
//! the welcome / server / token steps before the heavier write-side
//! commands (`cmd_install`, `cmd_force_kill_msi`, etc.) land in W6b.
//!
//! Each handler returns `Result<T, String>` so Tauri marshals errors
//! as rejected promises that the front-end renders verbatim. We
//! never return raw token bytes from any command (H5 — only parsed
//! metadata).

use serde::Serialize;

// ─── Probing / status ─────────────────────────────────────────────────────────

/// Output of `cmd_detect_install`. Mirrors
/// `roomler_agent::install_detect::ExistingInstall` but flat-tagged
/// for JS consumption (Tauri serialises Rust enums with a `type`
/// discriminant by default; this wrapper makes the shape obvious to
/// the SPA).
#[derive(Debug, Clone, Serialize)]
pub struct DetectResult {
    /// `"clean" | "peruser" | "permachine" | "ambiguous"`.
    pub kind: String,
    /// perUser version when present, else None.
    pub peruser_version: Option<String>,
    /// perMachine version when present, else None.
    pub permachine_version: Option<String>,
    /// Whether the host has BOTH flavours installed — should never
    /// happen post-rc.18 but is surfaced explicitly so the wizard
    /// can refuse to proceed until the operator cleans up.
    pub ambiguous: bool,
}

/// Probe the registry for an existing roomler-agent install.
/// On non-Windows platforms always returns `kind: "clean"` because
/// the installer wizard is Windows-only for v1.
#[tauri::command]
pub fn cmd_detect_install() -> Result<DetectResult, String> {
    use roomler_agent::install_detect::{ExistingInstall, detect_existing_install};
    let detected = detect_existing_install();
    Ok(match detected {
        ExistingInstall::Clean => DetectResult {
            kind: "clean".to_string(),
            peruser_version: None,
            permachine_version: None,
            ambiguous: false,
        },
        ExistingInstall::PerUser(info) => DetectResult {
            kind: "peruser".to_string(),
            peruser_version: info.version,
            permachine_version: None,
            ambiguous: false,
        },
        ExistingInstall::PerMachine(info) => DetectResult {
            kind: "permachine".to_string(),
            peruser_version: None,
            permachine_version: info.version,
            ambiguous: false,
        },
        ExistingInstall::Ambiguous {
            peruser,
            permachine,
        } => DetectResult {
            kind: "ambiguous".to_string(),
            peruser_version: peruser.version,
            permachine_version: permachine.version,
            ambiguous: true,
        },
    })
}

/// Hostname (no domain) — the wizard pre-fills the device-name input
/// with this so the operator usually just hits Enter. Falls back to
/// `"roomler-host"` if the OS won't yield a name (rare).
#[tauri::command]
pub fn cmd_default_device_name() -> String {
    gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "roomler-host".to_string())
}

/// Production roomler.ai URL. Operators usually leave this as-is;
/// staging / on-prem deployments edit it. Trailing slash deliberately
/// omitted — `enrollment::enroll` trims it anyway, but consistency
/// with what the operator-facing UI shows + what gets saved into
/// config.toml's `server_url`.
#[tauri::command]
pub fn cmd_default_server_url() -> String {
    "https://roomler.ai".to_string()
}

/// Parsed view of an enrollment token. Mirrors
/// `roomler_agent::jwt_introspect::JwtView`. Crucially: the original
/// token string is NEVER returned (H5). The wizard's "use saved
/// token" path consults this for issuer / expiry only; the raw
/// token bytes stay in the form input until passed to `cmd_install`.
#[derive(Debug, Clone, Serialize)]
pub struct TokenValidation {
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub subject: Option<String>,
    pub jti: Option<String>,
    pub expires_at_unix: Option<i64>,
    /// `true` when `exp <= now` or `exp` is missing entirely.
    pub appears_expired: bool,
}

/// Introspect a JWT WITHOUT verifying the signature (the wizard has
/// no signing key; server-side `/api/agent/enroll` re-verifies on
/// receipt). Used by the Token step to show "Issuer: roomler-ai,
/// expires in 8 min" before the operator hits Continue.
///
/// Error path returns a `String` describing what failed without
/// quoting the token (potentially sensitive bytes).
#[tauri::command]
pub fn cmd_validate_token(token: String) -> Result<TokenValidation, String> {
    use roomler_agent::jwt_introspect::{is_likely_expired, parse_unverified};
    let view = parse_unverified(&token).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(TokenValidation {
        appears_expired: is_likely_expired(&view, now),
        issuer: view.issuer,
        audience: view.audience,
        subject: view.subject,
        jti: view.jti,
        expires_at_unix: view.expires_at_unix,
    })
}

// ─── Wizard state persistence ────────────────────────────────────────────────

/// Load the wizard's persisted state. Returns `Default` (= Welcome
/// step, empty fields) when no state file exists or it's corrupt;
/// the wizard's UI always has SOMETHING to render. Token is never
/// in the state — see `wizard_state` module docs.
#[tauri::command]
pub fn cmd_load_state() -> Result<crate::wizard_state::WizardState, String> {
    let path = crate::wizard_state::default_state_path().map_err(|e| e.to_string())?;
    Ok(crate::wizard_state::load(&path))
}

/// Persist wizard state. Called on every form-blur from the SPA so
/// a forced kill mid-flow resumes cleanly on the next launch.
#[tauri::command]
pub fn cmd_save_state(state: crate::wizard_state::WizardState) -> Result<(), String> {
    let path = crate::wizard_state::default_state_path().map_err(|e| e.to_string())?;
    crate::wizard_state::save(&path, &state).map_err(|e| e.to_string())
}

// ─── Install execution (W6b) ─────────────────────────────────────────────────

/// Drive the full install pipeline end-to-end: preflight → resolve →
/// download → verify → spawn MSI → wait/decode → enroll. Progress
/// streams over the Tauri `ipc::Channel`; every event is also pushed
/// into the replay log so a late-attaching SPA listener catches up
/// via [`cmd_install_progress_replay`].
///
/// `rename_all = "camelCase"` because the JS SPA calls this with
/// `deviceName` / `onEvent` arg keys; Tauri 2's default is
/// snake_case which would silently leave `device_name` empty + the
/// channel null. Field repro 2026-05-15 (rc.29): operator's
/// perMachine install ran end-to-end but the Done page rendered
/// blank because the Channel was null + the orchestrator received
/// `device_name = ""`.
#[tauri::command(rename_all = "camelCase")]
pub async fn cmd_install(
    flavour: String,
    server: String,
    token: String,
    device_name: String,
    on_event: tauri::ipc::Channel<crate::progress::ProgressEvent>,
) -> Result<crate::install_orchestrator::DoneReport, String> {
    crate::install_orchestrator::run_install(flavour, server, token, device_name, on_event).await
}

/// Pre-spawn cancel: flips the orchestrator's `CANCEL_REQUESTED`
/// flag. Subsequent `check_cancel()` calls bail with
/// "cancelled by operator". No-op once msiexec is spawned —
/// SPA must surface the "Force-kill" affordance instead.
#[tauri::command]
pub fn cmd_cancel_in_progress() -> Result<(), String> {
    crate::install_orchestrator::request_cancel();
    Ok(())
}

/// Force-kill the active msiexec process. May leave Windows
/// Installer in a partially-rolled-back state; the SPA must show
/// a confirmation dialog before invoking this.
#[tauri::command]
pub fn cmd_force_kill_msi() -> Result<(), String> {
    crate::install_orchestrator::force_kill_msi()
}

/// Snapshot of the ProgressEvent replay log. The SPA calls this on
/// first listener attach to fast-forward through any events emitted
/// before its `ipc::Channel` listener was wired up (H1 mitigation).
#[tauri::command]
pub fn cmd_install_progress_replay() -> Vec<crate::progress::ProgressEvent> {
    crate::progress::replay_log().snapshot()
}
