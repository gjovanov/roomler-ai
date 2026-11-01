// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Tauri invoke handlers for the unified setup wizard.
//!
//! 19 handlers — the union of the two legacy wizards' surfaces, the
//! diagnostics pair (FR-102), the pairing four (FR-129 A3) and the two
//! of the Linux/macOS daemon install (FR-129 B1) behind ONE command
//! vocabulary. Each handler returns `Result<T, String>` (or a
//! plain value) so Tauri marshals errors as rejected promises the SPA
//! renders verbatim. The raw enrollment token NEVER flows back from any
//! command — only parsed metadata (H5, inherited from both legacy
//! wizards); a paired token never reaches the webview at all.
//!
//! All payload structs serialise camelCase so the SPA binds
//! `view.expiresAtUnix` / `report.principalId` etc. without a
//! translation layer.

use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use crate::role::Role;

// ─── Probing / status ─────────────────────────────────────────────────────────

/// Output of `cmd_detect_install`: BOTH product probes in one round
/// trip, so the Welcome step renders the daemon and tunnel detection
/// summaries from a single invoke.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectResult {
    pub agent: AgentDetect,
    pub tunnel: TunnelDetect,
}

/// Daemon probe half. Mirrors the legacy agent wizard's `DetectResult`
/// fields — flat string discriminator for JS consumption — plus the
/// `supported` flag the unified SPA uses to hide the daemon role cards, and
/// (FR-129 B1) what this OS implements and what the server allows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDetect {
    /// Whether a daemon card may be shown at all. Windows: always. Linux
    /// and macOS (FR-129 B1): this OS implements a daemon role AND the
    /// server named to `cmd_detect_install` offers it on this OS
    /// ([`Self::daemon_offered`]) — anything else is `false`.
    pub supported: bool,
    /// `"clean" | "peruser" | "permachine" | "ambiguous"`. On Linux and
    /// macOS `permachine` is the system (root) daemon's enrollment, and
    /// `peruser` the person's own.
    pub kind: String,
    /// perUser version when present, else None.
    pub peruser_version: Option<String>,
    /// perMachine version when present, else None.
    pub permachine_version: Option<String>,
    /// Whether the host has BOTH flavours installed — should never
    /// happen post-rc.18 but is surfaced explicitly so the wizard
    /// can refuse to proceed until the operator cleans up.
    pub ambiguous: bool,
    /// `windows` · `linux` · `macos` · `other`: which card copy to show.
    pub os: String,
    /// The daemon roles this OS implements, in card order
    /// ([`crate::role::daemon_roles`]); the SPA hides every other daemon card.
    pub daemon_roles: Vec<Role>,
    /// The server whose daemon switch for this OS `supported` reflects, on
    /// Linux and macOS; `None` on Windows, where no server is asked.
    pub features_server: Option<String>,
    /// That server's daemon switch for this OS (FR-129 B1-s2,
    /// [`crate::features::SetupFeatures::daemon_offered`]):
    /// `wizard_linux_daemon` on Linux, `wizard_macos_daemon` on macOS, each
    /// OR the `wizard_unix_daemon` a server from before the split sends.
    /// `false` for anything but a JSON `true`, and on Windows, where no
    /// server is asked.
    pub daemon_offered: bool,
}

/// Tunnel-CLI probe half. Mirrors the legacy tunnel wizard's
/// `DetectResult` (config.toml probe at the platform-default path).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelDetect {
    pub installed: bool,
    /// When installed, the recorded machine_name from the
    /// config.toml. None for clean / unreadable.
    pub machine_name: Option<String>,
    /// When installed, the config.toml path (verbatim `display()`).
    pub config_path: Option<String>,
}

/// Probe the host: the registry (the daemon MSI flavours) on Windows, the
/// daemon binary and the two config paths on Linux and macOS, plus the
/// tunnel CLI's config path everywhere. `server` is the Server page's URL as
/// the SPA has it when the Welcome page loads (its persisted or default
/// value): on Linux and macOS the daemon cards appear only when THAT server
/// offers the daemon on this OS (FR-129 B1, key design B.6; one switch per OS
/// since B1-s2), and the SPA asks again ([`cmd_setup_features`]) when the URL
/// changes. The orchestrator asks once more, for the chosen server, before
/// installing.
#[tauri::command(rename_all = "camelCase")]
pub async fn cmd_detect_install(server: Option<String>) -> Result<DetectResult, String> {
    let os = crate::role::HostOs::current();
    let agent = match os {
        crate::role::HostOs::Windows => detect_windows(os),
        _ => {
            let server = server
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(cmd_default_server_url);
            let features = crate::features::fetch(&server).await;
            detect_unix(os, &server, features)
        }
    };
    let tunnel = probe_tunnel_install();
    Ok(DetectResult { agent, tunnel })
}

/// Windows: the MSI registry probe (`detect_existing_install`), unchanged.
fn detect_windows(os: crate::role::HostOs) -> AgentDetect {
    use roomlerd::install_detect::{ExistingInstall, detect_existing_install};
    let base = AgentDetect {
        supported: true,
        kind: "clean".to_string(),
        peruser_version: None,
        permachine_version: None,
        ambiguous: false,
        os: os.wire().to_string(),
        daemon_roles: crate::role::daemon_roles(os).to_vec(),
        features_server: None,
        daemon_offered: false,
    };
    match detect_existing_install() {
        ExistingInstall::Clean => base,
        ExistingInstall::PerUser(info) => AgentDetect {
            kind: "peruser".to_string(),
            peruser_version: info.version,
            ..base
        },
        ExistingInstall::PerMachine(info) => AgentDetect {
            kind: "permachine".to_string(),
            permachine_version: info.version,
            ..base
        },
        ExistingInstall::Ambiguous {
            peruser,
            permachine,
        } => AgentDetect {
            kind: "ambiguous".to_string(),
            peruser_version: peruser.version,
            permachine_version: permachine.version,
            ambiguous: true,
            ..base
        },
    }
}

/// Linux and macOS (FR-129 B1): the daemon binary and the two enrollments,
/// and the server's switch for this OS — a card is shown only on its `true`.
fn detect_unix(
    os: crate::role::HostOs,
    server: &str,
    features: crate::features::SetupFeatures,
) -> AgentDetect {
    let d = crate::unix_daemon::detect(os);
    let roles = crate::role::daemon_roles(os);
    let offered = features.daemon_offered(os);
    let kind = match (d.per_user_config, d.system_config) {
        (true, true) => "ambiguous",
        (true, false) => "peruser",
        (false, true) => "permachine",
        (false, false) => "clean",
    };
    AgentDetect {
        supported: !roles.is_empty() && offered,
        kind: kind.to_string(),
        peruser_version: d
            .per_user_config
            .then(|| d.daemon_version.clone())
            .flatten(),
        permachine_version: d.system_config.then(|| d.daemon_version.clone()).flatten(),
        // Two enrollments on a Linux box are two daemons, not an MSI
        // mix-up: said on the Welcome page, never a refusal.
        ambiguous: false,
        os: os.wire().to_string(),
        daemon_roles: roles.to_vec(),
        features_server: Some(server.to_string()),
        daemon_offered: offered,
    }
}

/// FR-129 B1 — the server's switches, for the Server page: when the URL
/// changes, the SPA asks here whether the daemon role it holds is still
/// offered there, on this OS (B1-s2: `daemonOffered`, the per-OS rule applied
/// here, not in the page). Never cached; anything but a JSON `true` is
/// `false`.
#[tauri::command]
pub async fn cmd_setup_features(server: String) -> crate::features::FeaturesView {
    crate::features::fetch(&server)
        .await
        .view(crate::role::HostOs::current())
}

/// FR-129 B1 — the terminal fallback's "I've run it": the privileged half
/// could not be prompted for, the SPA showed the command, and the person
/// says it ran. Wakes the waiting pipeline, which then checks.
#[tauri::command]
pub fn cmd_privileged_half_done() -> Result<(), String> {
    crate::unix_daemon::confirm_fire()
}

/// #3116 — must the pre-logon service role run from an elevated wizard here?
/// The Welcome page asks before a token is pasted or paired: on a reinstall a
/// limited wizard cannot write that role's config. `false` off Windows.
#[tauri::command]
pub fn cmd_system_context_needs_elevation() -> bool {
    crate::orchestrator_agent::system_context_needs_elevation()
}

/// The legacy tunnel wizard's config.toml probe, verbatim: resolve
/// the platform-default path, read + TOML-parse best-effort for the
/// machine_name.
fn probe_tunnel_install() -> TunnelDetect {
    let Ok(path) = roomler_cli::config::default_config_path() else {
        return TunnelDetect {
            installed: false,
            machine_name: None,
            config_path: None,
        };
    };
    if !path.exists() {
        return TunnelDetect {
            installed: false,
            machine_name: None,
            config_path: None,
        };
    }
    let machine_name = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str::<roomler_cli::config::TunnelConfig>(&s).ok())
        .map(|c| c.machine_name);
    TunnelDetect {
        installed: true,
        machine_name,
        config_path: Some(path.display().to_string()),
    }
}

/// Hostname (no domain) — the wizard pre-fills the device-name input
/// with this so the operator usually just hits Enter. Falls back to
/// `"roomler-device"` if the OS won't yield a name (rare).
#[tauri::command]
pub fn cmd_default_device_name() -> String {
    gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "roomler-device".to_string())
}

/// Production roomler.ai URL. Operators usually leave this as-is;
/// staging / on-prem deployments edit it.
#[tauri::command]
pub fn cmd_default_server_url() -> String {
    "https://roomler.ai".to_string()
}

// ─── Token introspection ─────────────────────────────────────────────────────

/// Parsed view of an enrollment token. NEVER carries the raw token
/// bytes back (H5). The Roomler server's enrollment JWTs carry the
/// discriminant in a custom `token_type` claim (snake_case, e.g.
/// `"tunnel_enrollment"`) and NO `aud` claim — `audience` stays None
/// for them; the SPA renders `token_type` in the info card.
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
    /// Server-emitted `token_type` claim via
    /// `wizard_shared::token_peek`.
    pub token_type: Option<String>,
    /// `Some(token_type == "tunnel_enrollment")` ONLY when the
    /// operator picked the tunnel-client role — catches "wrong token
    /// copied" mistakes (agent-enrollment token from Admin → Agents
    /// pasted instead of the tunnel one from Admin → Tunnels) before
    /// the POST. `None` for daemon roles / no role: daemon tokens are
    /// not gated client-side, same as the legacy agent wizard.
    pub audience_matches: Option<bool>,
    /// FR-104 — the token may also anchor the device it enrolls. `Some(true)`
    /// = it will, for this role (a Windows machine-wide install: the wizard
    /// then asks for a second administrator prompt); `Some(false)` = it could,
    /// but this role has no SYSTEM service to anchor; `None` = a plain token.
    pub will_anchor: Option<bool>,
}

/// Introspect a JWT WITHOUT verifying the signature (the wizard has
/// no signing key; the server re-verifies on every enrollment POST).
/// Used by the Token step to show "Issuer / Token type / Expires in
/// N min" before the operator hits Continue.
///
/// `rename_all = "camelCase"` so the JS arg keys bind 1:1 (see
/// `cmd_install` for the field repro that motivates the attribute).
#[tauri::command(rename_all = "camelCase")]
pub fn cmd_validate_token(token: String, role: Option<Role>) -> Result<TokenValidation, String> {
    use roomlerd::jwt_introspect::{is_likely_expired, parse_unverified};
    let view = parse_unverified(&token).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let token_type = wizard_shared::token_peek::parse_token_type(&token);
    let audience_matches = match role {
        Some(Role::TunnelClient) => Some(token_type.as_deref() == Some("tunnel_enrollment")),
        _ => None,
    };
    let will_anchor = wizard_shared::token_peek::parse_anchor_claim(&token)
        .then(|| role.is_some_and(crate::orchestrator_agent::role_anchors));
    Ok(TokenValidation {
        appears_expired: is_likely_expired(&view, now),
        audience_matches,
        will_anchor,
        issuer: view.issuer,
        audience: view.audience,
        token_type,
        subject: view.subject,
        jti: view.jti,
        expires_at_unix: view.expires_at_unix,
    })
}

// ─── Wizard state persistence ────────────────────────────────────────────────

/// Load the wizard's persisted state. Returns `Default` (= Welcome
/// step, empty fields) when no state file exists or it's corrupt;
/// the wizard's UI always has SOMETHING to render. Token is never
/// in the state — see `wizard_shared::wizard_state` docs.
#[tauri::command]
pub fn cmd_load_state() -> Result<wizard_shared::wizard_state::WizardState, String> {
    let path = wizard_shared::wizard_state::default_state_path().map_err(|e| e.to_string())?;
    Ok(wizard_shared::wizard_state::load(&path))
}

/// Persist wizard state. Called on every form-blur from the SPA so
/// a forced kill mid-flow resumes cleanly on the next launch.
#[tauri::command]
pub fn cmd_save_state(state: wizard_shared::wizard_state::WizardState) -> Result<(), String> {
    let path = wizard_shared::wizard_state::default_state_path().map_err(|e| e.to_string())?;
    wizard_shared::wizard_state::save(&path, &state).map_err(|e| e.to_string())
}

// ─── Install execution ───────────────────────────────────────────────────────

/// Output of a successful `cmd_install`, whichever orchestrator ran.
/// Surfaced on the Done step. `principal_kind` is `"agent"` or
/// `"tunnel_client"`; the trailing `Option` fields are per-role
/// extras (flavour for daemon roles; binary/PATH details for
/// whichever pipeline delivered a CLI; config path for both).
///
/// P4b (role→action composition): daemon MSIs carry the `roomler`
/// CLI, so daemon roles now also populate `binary_path` /
/// `path_updated` and set `cli_included` from a post-install
/// existence check (`Some(false)` = an old pre-P4b MSI was served —
/// the SPA then doesn't promise a CLI that wasn't delivered).
/// `None` for the tunnel role, where the CLI *is* the install.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoneReport {
    pub principal_kind: String,
    pub principal_id: String,
    pub tenant_id: String,
    pub tag: String,
    pub role: Role,
    pub flavour: Option<String>,
    pub binary_path: Option<String>,
    pub config_path: Option<String>,
    pub path_updated: Option<bool>,
    pub shortcut_created: Option<bool>,
    pub cli_included: Option<bool>,
    /// GAP-A/P6: daemon roles place the `roomler-desktop` GUI
    /// companion beside the daemon (it isn't in the MSI). `Some(true)`
    /// placed, `Some(false)` best-effort failure / no server asset,
    /// `None` for the tunnel role + non-Windows.
    pub desktop_installed: Option<bool>,
    /// FR-104 — the device was anchored for SYSTEM exec in this install:
    /// `Some(true)` anchored, `Some(false)` the step ran and did not (the
    /// administrator prompt was declined, or the anchor failed; the Done
    /// page then shows the manual command), `None` it did not apply (a plain
    /// token, a per-user or tunnel install, non-Windows).
    pub anchored: Option<bool>,
    /// FR-129 B2 — the macOS root half this install enrolled beside the
    /// person's device: its own device row, the one that owns the overlay.
    /// `None` everywhere else.
    pub root_half: Option<RootHalfReport>,
}

/// FR-129 B2 — the device the macOS root half enrolled as, for the Done page.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RootHalfReport {
    pub principal_id: String,
    pub tenant_id: String,
    /// Its device name (`<name>-daemon`).
    pub name: String,
    /// `/etc/roomler/config.toml` — root's, 0600.
    pub config_path: String,
}

/// Drive the full install pipeline end-to-end for the picked role:
/// daemon roles route to [`crate::orchestrator_agent`] (preflight →
/// resolve → download → verify → spawn MSI → wait/decode → enroll),
/// the tunnel-client role to [`crate::orchestrator_tunnel`]
/// (preflight → resolve → download → verify → extract → integrate →
/// enroll). Progress streams over the Tauri `ipc::Channel`; every
/// event is also pushed into the replay log so a late-attaching SPA
/// listener catches up via [`cmd_install_progress_replay`]. The
/// statics + replay log reset on entry inside the orchestrators
/// (legacy semantics preserved: reset on cmd_install entry).
///
/// `rename_all = "camelCase"` because the JS SPA calls this with
/// `deviceName` / `onEvent` arg keys; Tauri 2's default is
/// snake_case which would silently leave `device_name` empty + the
/// channel null. Field repro 2026-05-15 (rc.29): operator's
/// perMachine install ran end-to-end but the Done page rendered
/// blank because the Channel was null + the orchestrator received
/// `device_name = ""`.
///
/// FR-129 A3 — `usePaired: true` installs with the token the pairing
/// delivered ([`crate::pairing::PairStore::tokens_for`]) instead of the
/// pasted one; the SPA then passes `token: ""`. Everything from the
/// orchestrators on is the same call with the same token argument — a
/// paired token is an ordinary single-use enrollment token. The session
/// is cleared only when the install SUCCEEDS, so a Retry after a failure
/// before the enroll reads the same token again. Absent or `false` is the
/// pasted path, byte for byte as before.
///
/// FR-129 B2 — on macOS the daemon role with the private-network box ticked
/// also installs the ROOT HALF, a second device with a token of its own: the
/// pairing's second token, or on the pasted path `rootToken`, the second
/// token the person pasted (required then, and never the first one again).
#[tauri::command(rename_all = "camelCase")]
#[allow(clippy::too_many_arguments)]
pub async fn cmd_install(
    role: Role,
    server: String,
    token: String,
    root_token: Option<String>,
    device_name: String,
    advanced: Option<crate::orchestrator_agent::AdvancedOptions>,
    use_paired: Option<bool>,
    on_event: tauri::ipc::Channel<wizard_shared::progress::ProgressEvent>,
) -> Result<DoneReport, String> {
    let store = crate::pairing::PairStore::global();
    let root_half = crate::role::wants_root_half(
        crate::role::HostOs::current(),
        role,
        advanced.as_ref().and_then(|a| a.overlay_enabled),
    );
    let (tokens, paired) = resolve_install_tokens(
        store, role, &server, token, root_token, use_paired, root_half,
    )?;
    let crate::pairing::InstallTokens {
        main: token,
        root_half: root_token,
    } = tokens;
    let report = match role {
        Role::TunnelClient => {
            // Advanced options are daemon-config keys — the SPA hides the
            // section for this role; ignore whatever arrives.
            crate::orchestrator_tunnel::run_install(role, server, token, device_name, on_event)
                .await
        }
        // FR-129 B1 — the MSI pipeline on Windows; the embedded `install.sh`
        // halves on Linux and macOS (which refuse a role their OS does not
        // implement, and a server that does not allow it).
        Role::DaemonSystem | Role::DaemonUser | Role::DaemonMachine => {
            if crate::role::HostOs::current() == crate::role::HostOs::Windows {
                crate::orchestrator_agent::run_install(
                    role,
                    server,
                    token,
                    device_name,
                    advanced.unwrap_or_default(),
                    on_event,
                )
                .await
            } else {
                crate::orchestrator_unix::run_install(
                    role,
                    server,
                    token,
                    root_token,
                    device_name,
                    advanced.unwrap_or_default(),
                    on_event,
                )
                .await
            }
        }
    };
    if paired && report.is_ok() {
        store.finish();
    }
    report
}

/// Which tokens `cmd_install` enrolls with: the pasted ones as given, or —
/// `use_paired` — the ones the pairing delivered for this role and server,
/// cloned out of the store. `root_half` (FR-129 B2): the install adds the
/// macOS root half, which needs a second token — the pairing's second, or
/// the second one pasted. Returns the tokens and whether they came from the
/// pairing (so a success can close the session). A pasted call never reads
/// the store, so a pairing left behind cannot leak into a paste.
fn resolve_install_tokens(
    store: &crate::pairing::PairStore,
    role: Role,
    server: &str,
    pasted: String,
    pasted_root: Option<String>,
    use_paired: Option<bool>,
    root_half: bool,
) -> Result<(crate::pairing::InstallTokens, bool), String> {
    if use_paired.unwrap_or(false) {
        return Ok((store.tokens_for(role, server, root_half)?, true));
    }
    let root_half = if root_half {
        let root = pasted_root
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                "the private network on a Mac is a second device with an enrollment token of its \
                 own: paste a second token, or untick \"Join the overlay network\" to install \
                 screen sharing only"
                    .to_string()
            })?;
        if root == pasted.trim() {
            return Err(
                "the two tokens are the same token — each enrolls one device and works once, so \
                 the private network needs a second, different one"
                    .to_string(),
            );
        }
        Some(root)
    } else {
        None
    };
    Ok((
        crate::pairing::InstallTokens {
            main: pasted,
            root_half,
        },
        false,
    ))
}

/// Pre-spawn cancel: flips the process-wide
/// [`crate::CANCEL_REQUESTED`] flag. The active orchestrator bails at
/// its next `check_cancel()` (and the download loop aborts between
/// chunks). No-op once msiexec is spawned — the SPA must surface the
/// "Force-kill" affordance instead.
#[tauri::command]
pub fn cmd_cancel_in_progress() -> Result<(), String> {
    crate::CANCEL_REQUESTED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Force-kill the active msiexec process (daemon roles, Windows).
/// May leave Windows Installer in a partially-rolled-back state; the
/// SPA must show a confirmation dialog before invoking this.
/// Non-Windows returns `Err("not applicable …")`.
#[tauri::command]
pub fn cmd_force_kill_msi() -> Result<(), String> {
    crate::orchestrator_agent::force_kill_msi()
}

/// Snapshot of the ProgressEvent replay log. The SPA calls this on
/// first listener attach to fast-forward through any events emitted
/// before its `ipc::Channel` listener was wired up (H1 mitigation).
#[tauri::command]
pub fn cmd_install_progress_replay() -> Vec<wizard_shared::progress::ProgressEvent> {
    wizard_shared::progress::replay_log().snapshot()
}

// ─── FR-102 P3b: opt-in setup diagnostics ────────────────────────────────────

/// The Welcome page's "Send anonymous setup diagnostics" box, as the
/// operator set it. In memory for this process only — never part of
/// `cmd_save_state`, never read back from an earlier run. Unticking also
/// drops anything buffered while it was ticked
/// ([`crate::diagnostics::Diagnostics::set_opt_in`]).
#[tauri::command]
pub fn cmd_diagnostics_opt_in(enabled: bool) {
    crate::diagnostics::Diagnostics::global().set_opt_in(enabled);
}

/// A page boundary the SPA measured: `welcome` / `server` / `token` with
/// `ok` or `cancelled` and the time spent on the page. Buffered until the
/// server is known; dropped while the box is unticked. A spelling outside
/// the vocabulary is ignored, never an error — diagnostics must not be
/// able to fail the wizard.
#[tauri::command(rename_all = "camelCase")]
pub fn cmd_diagnostics_page(step: String, outcome: String, elapsed_ms: u64) {
    use roomler_ai_remote_control::setup_diagnostics::{Outcome, Step};
    if let (Some(step), Some(outcome)) = (Step::parse(&step), Outcome::parse(&outcome)) {
        crate::diagnostics::Diagnostics::global().page(step, outcome, elapsed_ms);
    }
}

/// Cleanly exit the wizard process. Wired to the Done page's Finish
/// button. JS `window.close()` in Tauri 2 is unreliable: depending on
/// the OS-window vs webview lifecycle, it can blank the webview but
/// leave the wizard process alive (the operator sees a white/gray
/// window with no controls — field repro 2026-05-16 on a Windows
/// field-test host post-SystemContext install). `AppHandle::exit(0)`
/// shuts the runtime down deterministically.
#[tauri::command]
pub fn cmd_exit_wizard(app: tauri::AppHandle) {
    // FR-129 B1 — an enrolled system-scope config kept for a Retry never
    // outlives the wizard: shredded before the process goes.
    crate::unix_daemon::clear_kept();
    app.exit(0);
}

// ─── FR-129 A3: device pairing ───────────────────────────────────────────────

/// What `cmd_pair_start` answers the SPA: the user code, the page to open,
/// and the clock — never the device code, which stays in
/// [`crate::pairing::PairStore`]. `unavailable` is the server's 404 (the
/// `[pairing] enabled = false` switch, or a server from before FR-129): the
/// SPA then hides the pairing action and shows the paste box alone.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PairStartView {
    Started {
        user_code: String,
        verification_uri: String,
        expires_in: u64,
        interval: u64,
    },
    Unavailable,
}

/// Open a pairing against `server` for `role`, named `device_name`. The
/// device code the server answers is held in the store; the SPA gets the
/// rest. A pairing already held is replaced (its token dropped) — a Start
/// again is this call.
///
/// `rootHalf` (FR-129 B2) is the private-network box as the SPA shows it:
/// on macOS the daemon role with it ticked adds the root half, so the
/// pairing asks for `halves: 2` — one approval, two tokens. Anywhere else it
/// changes nothing ([`crate::role::wants_root_half`]).
///
/// The verification URI must be a web address: the wizard opens it with
/// the shell plugin, and a server that answered anything else is refused
/// here rather than handed to the OS.
#[tauri::command(rename_all = "camelCase")]
pub async fn cmd_pair_start(
    server: String,
    role: Role,
    device_name: String,
    root_half: Option<bool>,
) -> Result<PairStartView, String> {
    use roomlerd::pairing::{StartOutcome, StartRequest, host_arch, host_os};
    let halves = crate::role::pair_halves(crate::role::wants_root_half(
        crate::role::HostOs::current(),
        role,
        root_half,
    ));
    let request = StartRequest {
        machine_name: device_name,
        os: host_os(),
        arch: host_arch(),
        role: role.pair_role(),
        halves,
        client: "wizard".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let outcome = roomlerd::pairing::start(&server, &request)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let started = match outcome {
        StartOutcome::Unavailable => return Ok(PairStartView::Unavailable),
        StartOutcome::Started(s) => s,
    };
    if !is_web_address(&started.verification_uri) {
        return Err(
            "the server's pairing page is not a web address; paste a token instead".to_string(),
        );
    }
    let view = PairStartView::Started {
        user_code: started.user_code.clone(),
        verification_uri: started.verification_uri.clone(),
        expires_in: started.expires_in,
        interval: started.interval,
    };
    crate::pairing::PairStore::global().begin(server, role, halves, started);
    Ok(view)
}

/// One poll of the held pairing. The device code is copied out of the
/// store before the request and the answer applied after it — the lock is
/// never held across the await ([`crate::pairing`]). With no pairing held
/// the call refuses, which ends a loop that outlived its session.
///
/// `interval_secs` is what the SPA polled with; a `slow_down` or a 429
/// answers the next interval from it (never shorter), so the back-off
/// arithmetic lives — and is tested — on this side.
#[tauri::command(rename_all = "camelCase")]
pub async fn cmd_pair_poll(interval_secs: u64) -> Result<crate::pairing::PairPollView, String> {
    let store = crate::pairing::PairStore::global();
    let snapshot = store
        .snapshot()
        .ok_or_else(|| "no pairing in progress".to_string())?;
    let outcome = roomlerd::pairing::poll(&snapshot.server, &snapshot.device_code)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(store.apply_poll(snapshot.generation, outcome, interval_secs))
}

/// Open the held pairing's page in the default browser. The URL comes from
/// the store, never from the SPA, and carries no code (operator decision
/// 1): the person types the code the wizard shows.
#[tauri::command]
pub fn cmd_pair_open(app: tauri::AppHandle) -> Result<(), String> {
    let uri = crate::pairing::PairStore::global()
        .verification_uri()
        .ok_or_else(|| "no pairing in progress".to_string())?;
    open_in_browser(&app, &uri)
}

/// Forget the held pairing: its device code and any delivered token. The
/// Cancel on the approval card, Back, and a paste-path install call this.
/// There is nothing to tell the server — the row expires on its own.
#[tauri::command]
pub fn cmd_pair_cancel() {
    crate::pairing::PairStore::global().cancel();
}

/// `http://` or `https://` — the only schemes the wizard hands to the OS.
fn is_web_address(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && lower.len() > "https://".len()
}

/// The default browser through the shell plugin, as the desktop companion
/// opens its links (`shell::open` is deprecated for tauri-plugin-opener,
/// but the shell plugin is already shipped and initialised here).
fn open_in_browser(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    #[allow(deprecated)]
    {
        use tauri_plugin_shell::ShellExt;
        app.shell()
            .open(url, None)
            .map_err(|e| format!("opening the browser: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic JWTs mirroring the server's emitted shape (snake_case
    // `token_type` claim, NO `aud`). Header + signature are
    // placeholder segments (introspection doesn't validate them);
    // payloads are pre-encoded base64url so the test module doesn't
    // need a base64 dev-dep — same convention as the legacy tunnel
    // wizard's command tests.
    const FAKE_HEADER: &str = "eyJhbGciOiJub25lIn0"; // {"alg":"none"}

    /// {"iss":"roomler-ai","token_type":"tunnel_enrollment","exp":9999999999}
    fn tunnel_enrollment_jwt() -> String {
        format!(
            "{FAKE_HEADER}.eyJpc3MiOiJyb29tbGVyLWFpIiwidG9rZW5fdHlwZSI6InR1bm5lbF9lbnJvbGxtZW50IiwiZXhwIjo5OTk5OTk5OTk5fQ.sig"
        )
    }

    /// {"iss":"roomler-ai","token_type":"enrollment","exp":9999999999}
    fn agent_enrollment_jwt() -> String {
        format!(
            "{FAKE_HEADER}.eyJpc3MiOiJyb29tbGVyLWFpIiwidG9rZW5fdHlwZSI6ImVucm9sbG1lbnQiLCJleHAiOjk5OTk5OTk5OTl9.sig"
        )
    }

    /// {"iss":"roomler-ai","exp":1000000}
    fn expired_jwt() -> String {
        format!("{FAKE_HEADER}.eyJpc3MiOiJyb29tbGVyLWFpIiwiZXhwIjoxMDAwMDAwfQ.sig")
    }

    #[test]
    fn detect_result_serialises_camel_case() {
        let r = DetectResult {
            agent: AgentDetect {
                supported: true,
                kind: "peruser".to_string(),
                peruser_version: Some("0.3.0-rc.190".to_string()),
                permachine_version: None,
                ambiguous: false,
                os: "linux".to_string(),
                daemon_roles: vec![Role::DaemonSystem, Role::DaemonUser],
                features_server: Some("https://example.invalid".to_string()),
                daemon_offered: true,
            },
            tunnel: TunnelDetect {
                installed: true,
                machine_name: Some("lap".to_string()),
                config_path: Some("/tmp/cfg.toml".to_string()),
            },
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""agent""#), "{json}");
        assert!(json.contains(r#""tunnel""#), "{json}");
        assert!(json.contains("peruserVersion"), "{json}");
        assert!(json.contains("permachineVersion"), "{json}");
        assert!(json.contains("machineName"), "{json}");
        assert!(json.contains("configPath"), "{json}");
        assert!(json.contains(r#""supported":true"#), "{json}");
        assert!(!json.contains("peruser_version"), "{json}");
        assert!(!json.contains("machine_name"), "{json}");
        // FR-129 B1 — the per-OS fields, camelCase, the roles in kebab-case.
        assert!(json.contains(r#""os":"linux""#), "{json}");
        assert!(
            json.contains(r#""daemonRoles":["daemon-system","daemon-user"]"#),
            "{json}"
        );
        assert!(
            json.contains(r#""featuresServer":"https://example.invalid""#),
            "{json}"
        );
        assert!(json.contains(r#""daemonOffered":true"#), "{json}");
        assert!(!json.contains("wizardUnixDaemon"), "{json}");
    }

    /// FR-129 B1 — on Linux and macOS a daemon card needs the server's
    /// `true` for that OS; the kind reads the two enrollments; Windows never
    /// asks.
    #[test]
    fn unix_detection_follows_the_server_switch() {
        use crate::features::SetupFeatures;
        use crate::role::HostOs;
        let off = SetupFeatures::default();
        let on = SetupFeatures {
            pairing: true,
            pairing_off: false,
            wizard_linux_daemon: true,
            wizard_macos_daemon: true,
            wizard_unix_daemon: true,
        };
        let d = detect_unix(HostOs::Linux, "https://a.invalid", off);
        assert!(!d.supported, "the switch is off");
        assert_eq!(d.os, "linux");
        assert_eq!(d.daemon_roles, vec![Role::DaemonSystem, Role::DaemonUser]);
        assert_eq!(d.features_server.as_deref(), Some("https://a.invalid"));
        assert!(!d.daemon_offered);
        let d = detect_unix(HostOs::Linux, "https://a.invalid", on);
        assert!(d.supported);
        assert!(d.daemon_offered);
        let d = detect_unix(HostOs::Macos, "https://a.invalid", on);
        assert!(d.supported);
        assert!(d.daemon_offered);
        assert_eq!(d.daemon_roles, vec![Role::DaemonUser]);
        // B1-s2 — the field the page binds is this OS's switch alone.
        let linux_only = SetupFeatures {
            pairing: true,
            wizard_linux_daemon: true,
            ..SetupFeatures::default()
        };
        let d = detect_unix(HostOs::Macos, "https://a.invalid", linux_only);
        assert!(!d.supported, "Linux's switch is not the Mac's");
        assert!(!d.daemon_offered);
        let d = detect_unix(HostOs::Other, "https://a.invalid", on);
        assert!(!d.supported, "no daemon role on this OS");
        assert!(d.daemon_roles.is_empty());
        // The kind is one of the SPA's four, whatever this box has.
        assert!(matches!(
            d.kind.as_str(),
            "clean" | "peruser" | "permachine" | "ambiguous"
        ));
    }

    /// FR-129 B1-s2 — each OS's cards follow that OS's switch, read from the
    /// server's answer as the wizard parses it. A split server with Linux on
    /// alone sends `wizard_unix_daemon: false`, the key every wizard released
    /// before the split reads for both OSes: this one shows the Linux cards
    /// and still hides the macOS ones. A server from before the split sends
    /// `wizard_unix_daemon` alone, and it still turns both on.
    #[test]
    fn each_os_follows_its_own_switch_and_an_older_server_still_works() {
        use crate::features::parse;
        use crate::role::HostOs;
        let server = "https://a.invalid";
        let supported = |os, body: &[u8]| detect_unix(os, server, parse(200, body)).supported;

        let linux_only = br#"{"pairing": true, "wizard_linux_daemon": true, "wizard_macos_daemon": false, "wizard_unix_daemon": false, "wizard_pairing": false}"#;
        assert!(supported(HostOs::Linux, linux_only), "Linux is on");
        assert!(!supported(HostOs::Macos, linux_only), "macOS stays off");

        let macos_only = br#"{"pairing": true, "wizard_linux_daemon": false, "wizard_macos_daemon": true, "wizard_unix_daemon": false, "wizard_pairing": false}"#;
        assert!(!supported(HostOs::Linux, macos_only), "Linux stays off");
        assert!(supported(HostOs::Macos, macos_only), "macOS is on");

        // A split server with both on: all three keys.
        let both = br#"{"pairing": true, "wizard_linux_daemon": true, "wizard_macos_daemon": true, "wizard_unix_daemon": true, "wizard_pairing": false}"#;
        assert!(supported(HostOs::Linux, both));
        assert!(supported(HostOs::Macos, both));

        // A server from before the split: one key, both OSes.
        let older_on = br#"{"pairing": true, "wizard_unix_daemon": true, "wizard_pairing": false}"#;
        assert!(supported(HostOs::Linux, older_on));
        assert!(supported(HostOs::Macos, older_on));
        let older_off =
            br#"{"pairing": true, "wizard_unix_daemon": false, "wizard_pairing": false}"#;
        assert!(!supported(HostOs::Linux, older_off));
        assert!(!supported(HostOs::Macos, older_off));

        // Off by default, and no OS without a daemon role is offered one.
        let off = br#"{"pairing": true, "wizard_linux_daemon": false, "wizard_macos_daemon": false, "wizard_unix_daemon": false, "wizard_pairing": false}"#;
        assert!(!supported(HostOs::Linux, off));
        assert!(!supported(HostOs::Macos, off));
        assert!(!supported(HostOs::Other, both));
    }

    #[test]
    fn done_report_serialises_camel_case_with_kebab_role() {
        let r = DoneReport {
            principal_kind: "tunnel_client".to_string(),
            principal_id: "507f1f77bcf86cd799439011".to_string(),
            tenant_id: "507f191e810c19729de860ea".to_string(),
            tag: "tunnel-v0.3.0-rc.194".to_string(),
            role: Role::TunnelClient,
            flavour: None,
            // RETIRED-NAME-ANCHOR(4): fixtures of the INSTALLED layout on hosts that
            // predate the rename.
            binary_path: Some("/usr/local/bin/roomler-tunnel".to_string()),
            config_path: Some("/home/foo/.config/roomler-tunnel/config.toml".to_string()),
            path_updated: Some(true),
            shortcut_created: Some(false),
            cli_included: None,
            desktop_installed: None,
            anchored: None,
            root_half: Some(RootHalfReport {
                principal_id: "507f1f77bcf86cd799439012".to_string(),
                tenant_id: "507f191e810c19729de860ea".to_string(),
                name: "lab-daemon".to_string(),
                config_path: "/etc/roomler/config.toml".to_string(),
            }),
        };
        let json = serde_json::to_string(&r).unwrap();
        // FR-129 B2: the root half's report binds camelCase too.
        assert!(
            json.contains(r#""rootHalf":{"principalId":"507f1f77bcf86cd799439012""#),
            "{json}"
        );
        assert!(!json.contains("root_half"), "{json}");
        assert!(
            json.contains(r#""principalKind":"tunnel_client""#),
            "{json}"
        );
        assert!(json.contains("principalId"), "{json}");
        assert!(json.contains("tenantId"), "{json}");
        assert!(json.contains(r#""role":"tunnel-client""#), "{json}");
        assert!(json.contains("binaryPath"), "{json}");
        assert!(json.contains("configPath"), "{json}");
        assert!(json.contains("pathUpdated"), "{json}");
        assert!(json.contains("shortcutCreated"), "{json}");
        // P4b: camelCase lock for the composition flag.
        assert!(json.contains("cliIncluded"), "{json}");
        assert!(!json.contains("cli_included"), "{json}");
        // GAP-A: camelCase lock for the desktop-companion flag.
        assert!(json.contains("desktopInstalled"), "{json}");
        assert!(!json.contains("desktop_installed"), "{json}");
        assert!(!json.contains("principal_kind"), "{json}");
        // Round-trips (Deserialize derive is part of the contract).
        let back: DoneReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.principal_id, r.principal_id);
        assert_eq!(back.role, Role::TunnelClient);
    }

    #[test]
    fn token_validation_serialises_camel_case() {
        let v = TokenValidation {
            issuer: Some("roomler-ai".to_string()),
            audience: None,
            subject: Some("507f1f77bcf86cd799439011".to_string()),
            jti: Some("abc".to_string()),
            expires_at_unix: Some(1_700_000_000),
            appears_expired: false,
            token_type: Some("tunnel_enrollment".to_string()),
            audience_matches: Some(true),
            will_anchor: Some(true),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("audienceMatches"), "{json}");
        // FR-104 — the SPA reads `willAnchor`.
        assert!(json.contains(r#""willAnchor":true"#), "{json}");
        assert!(json.contains("expiresAtUnix"), "{json}");
        assert!(json.contains("appearsExpired"), "{json}");
        assert!(
            json.contains(r#""tokenType":"tunnel_enrollment""#),
            "{json}"
        );
        assert!(!json.contains("expires_at_unix"), "{json}");
    }

    #[test]
    fn validate_token_gates_audience_only_for_tunnel_role() {
        // Tunnel role: gate evaluates.
        let v = cmd_validate_token(tunnel_enrollment_jwt(), Some(Role::TunnelClient)).unwrap();
        assert_eq!(v.audience_matches, Some(true));
        assert_eq!(v.token_type.as_deref(), Some("tunnel_enrollment"));
        assert_eq!(v.issuer.as_deref(), Some("roomler-ai"));
        // Daemon role: no client-side gate (None), token_type still
        // surfaced for the info card.
        let v = cmd_validate_token(tunnel_enrollment_jwt(), Some(Role::DaemonSystem)).unwrap();
        assert_eq!(v.audience_matches, None);
        // No role: also ungated.
        let v = cmd_validate_token(tunnel_enrollment_jwt(), None).unwrap();
        assert_eq!(v.audience_matches, None);
    }

    #[test]
    fn validate_token_flags_wrong_type_for_tunnel_role() {
        // Operator pasted the AGENT-enrollment token (token_type
        // "enrollment") while on the tunnel-client role.
        let v = cmd_validate_token(agent_enrollment_jwt(), Some(Role::TunnelClient)).unwrap();
        assert_eq!(v.audience_matches, Some(false));
        assert_eq!(v.token_type.as_deref(), Some("enrollment"));
    }

    #[test]
    fn validate_token_reports_expiry() {
        let v = cmd_validate_token(expired_jwt(), None).unwrap();
        assert!(v.appears_expired);
        assert_eq!(v.expires_at_unix, Some(1_000_000));
    }

    #[test]
    fn validate_token_rejects_malformed() {
        assert!(cmd_validate_token("not-a-jwt".to_string(), None).is_err());
    }

    #[test]
    fn default_device_name_returns_non_empty() {
        let n = cmd_default_device_name();
        assert!(!n.is_empty());
    }

    // ---- FR-129 A3: pairing ------------------------------------------------

    /// The SPA binds `kind` plus camelCase fields; `unavailable` is a bare
    /// tag. Neither variant has a place for the device code.
    #[test]
    fn pair_start_view_serialises_camel_case_with_a_kind_tag() {
        let v = PairStartView::Started {
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://roomler.ai/pair".into(),
            expires_in: 900,
            interval: 5,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains(r#""kind":"started""#), "{json}");
        assert!(json.contains(r#""userCode":"BCDF-GHJK""#), "{json}");
        assert!(
            json.contains(r#""verificationUri":"https://roomler.ai/pair""#),
            "{json}"
        );
        assert!(json.contains(r#""expiresIn":900"#), "{json}");
        assert!(json.contains(r#""interval":5"#), "{json}");
        assert!(!json.contains("user_code"), "{json}");
        assert!(!json.contains("device"), "{json}");

        let json = serde_json::to_string(&PairStartView::Unavailable).unwrap();
        assert_eq!(json, r#"{"kind":"unavailable"}"#);
    }

    /// Only a web address is handed to the OS to open.
    #[test]
    fn pair_page_must_be_a_web_address() {
        assert!(is_web_address("https://roomler.ai/pair"));
        assert!(is_web_address("http://127.0.0.1:3000/pair"));
        assert!(is_web_address(" HTTPS://Example.invalid/pair "));
        assert!(!is_web_address("file:///etc/passwd"));
        assert!(!is_web_address("javascript:alert(1)"));
        assert!(!is_web_address("roomler.ai/pair"));
        assert!(!is_web_address("https://"));
        assert!(!is_web_address(""));
    }

    /// One token, no root half: what `cmd_install` enrolls with.
    fn resolve_install_token(
        store: &crate::pairing::PairStore,
        role: Role,
        server: &str,
        pasted: String,
        use_paired: Option<bool>,
    ) -> Result<(String, bool), String> {
        resolve_install_tokens(store, role, server, pasted, None, use_paired, false).map(
            |(t, paired)| {
                assert!(t.root_half.is_none());
                (t.main, paired)
            },
        )
    }

    fn started_pairing() -> roomlerd::pairing::Started {
        roomlerd::pairing::Started {
            device_code: roomlerd::pairing::DeviceCode::new("dc"),
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://example.invalid/pair".into(),
            expires_in: 900,
            interval: 5,
        }
    }

    fn approved_with(tokens: &[&str]) -> roomlerd::pairing::PollOutcome {
        roomlerd::pairing::PollOutcome::Approved(roomlerd::pairing::Approved {
            tokens: tokens.iter().map(|t| t.to_string()).collect(),
            org_name: "Acme".into(),
            org_slug: Some("acme".into()),
            approver_name: "Dana".into(),
            approver_username: Some("dana".into()),
        })
    }

    /// `cmd_install`'s token choice: a pasted call never touches the store
    /// (a pairing left behind cannot leak into a paste), and a paired call
    /// reads the delivered token for its role and server — or refuses.
    #[test]
    fn install_token_resolution_keeps_the_paths_apart() {
        let store = crate::pairing::PairStore::new();
        let server = "https://example.invalid";

        // Pasted, with no pairing at all: the pasted token, untouched.
        let (t, paired) =
            resolve_install_token(&store, Role::DaemonSystem, server, "pasted".into(), None)
                .unwrap();
        assert_eq!((t.as_str(), paired), ("pasted", false));
        let (t, paired) = resolve_install_token(
            &store,
            Role::DaemonSystem,
            server,
            "pasted".into(),
            Some(false),
        )
        .unwrap();
        assert_eq!((t.as_str(), paired), ("pasted", false));

        // Paired, with nothing delivered: refused, and the pasted argument
        // is not used as a fallback.
        let e = resolve_install_token(&store, Role::DaemonSystem, server, "".into(), Some(true))
            .unwrap_err();
        assert!(e.contains("no pairing in progress"), "{e}");

        let generation = store.begin(server.to_string(), Role::DaemonSystem, 1, started_pairing());
        let e = resolve_install_token(&store, Role::DaemonSystem, server, "".into(), Some(true))
            .unwrap_err();
        assert!(e.contains("not been approved"), "{e}");

        store.apply_poll(generation, approved_with(&["eyJ.paired.sig"]), 5);
        let (t, paired) =
            resolve_install_token(&store, Role::DaemonSystem, server, "".into(), Some(true))
                .unwrap();
        assert_eq!((t.as_str(), paired), ("eyJ.paired.sig", true));

        // A pasted call beside a delivered pairing still takes the paste.
        let (t, paired) = resolve_install_token(
            &store,
            Role::DaemonSystem,
            server,
            "pasted".into(),
            Some(false),
        )
        .unwrap();
        assert_eq!((t.as_str(), paired), ("pasted", false));

        // The role and the server must be the pairing's.
        assert!(
            resolve_install_token(&store, Role::TunnelClient, server, "".into(), Some(true))
                .is_err()
        );
        assert!(
            resolve_install_token(
                &store,
                Role::DaemonSystem,
                "https://other.invalid",
                "".into(),
                Some(true)
            )
            .is_err()
        );
    }

    /// FR-129 B2 — the root half's token. Paired: the approval's second
    /// token, from a pairing started for two halves. Pasted: the second
    /// token the person pasted — required, and never the first one again (a
    /// token works once; the second enroll would be refused after the first
    /// had made its device). Without the root half a second paste is ignored.
    #[test]
    fn the_root_half_takes_a_token_of_its_own() {
        let store = crate::pairing::PairStore::new();
        let server = "https://example.invalid";

        let (t, paired) = resolve_install_tokens(
            &store,
            Role::DaemonUser,
            server,
            "eyJ.user.sig".into(),
            Some(" eyJ.root.sig\n".into()),
            None,
            true,
        )
        .unwrap();
        assert!(!paired);
        assert_eq!(t.main, "eyJ.user.sig");
        assert_eq!(t.root_half.as_deref(), Some("eyJ.root.sig"));

        for missing in [None, Some(String::new()), Some("  \n".into())] {
            let e = resolve_install_tokens(
                &store,
                Role::DaemonUser,
                server,
                "eyJ.user.sig".into(),
                missing,
                None,
                true,
            )
            .unwrap_err();
            assert!(
                e.contains("paste a second token") && e.contains("Join the overlay network"),
                "{e}"
            );
        }
        let e = resolve_install_tokens(
            &store,
            Role::DaemonUser,
            server,
            "eyJ.same.sig".into(),
            Some("eyJ.same.sig ".into()),
            None,
            true,
        )
        .unwrap_err();
        assert!(e.contains("same token"), "{e}");

        // No root half: a second paste goes nowhere.
        let (t, _) = resolve_install_tokens(
            &store,
            Role::DaemonUser,
            server,
            "eyJ.user.sig".into(),
            Some("eyJ.root.sig".into()),
            None,
            false,
        )
        .unwrap();
        assert!(t.root_half.is_none());

        // Paired: the two the approval delivered, in order.
        let generation = store.begin(server.to_string(), Role::DaemonUser, 2, started_pairing());
        store.apply_poll(generation, approved_with(&["eyJ.p1.sig", "eyJ.p2.sig"]), 5);
        let (t, paired) = resolve_install_tokens(
            &store,
            Role::DaemonUser,
            server,
            String::new(),
            None,
            Some(true),
            true,
        )
        .unwrap();
        assert!(paired);
        assert_eq!(t.main, "eyJ.p1.sig");
        assert_eq!(t.root_half.as_deref(), Some("eyJ.p2.sig"));
    }

    #[test]
    fn default_server_url_is_roomler_ai() {
        assert_eq!(cmd_default_server_url(), "https://roomler.ai");
    }

    #[tokio::test]
    async fn detect_install_returns_consistent_shape() {
        // Real probe on whatever host runs the tests: contract is a
        // valid discriminator + `supported` matching the compile
        // target — NOT a specific install state (dev boxes may have
        // real installs). The server named is a closed loopback port, so
        // off Windows the features fetch fails at once and the switch reads
        // off, as it must (FR-129 B1): no test reaches the network.
        let r = cmd_detect_install(Some("http://127.0.0.1:9".to_string()))
            .await
            .unwrap();
        assert!(
            matches!(
                r.agent.kind.as_str(),
                "clean" | "peruser" | "permachine" | "ambiguous"
            ),
            "unexpected agent kind {:?}",
            r.agent.kind
        );
        assert_eq!(r.agent.supported, cfg!(target_os = "windows"));
        assert_eq!(r.agent.os, crate::role::HostOs::current().wire());
        if !r.tunnel.installed {
            assert!(r.tunnel.config_path.is_none());
        }
    }
}
