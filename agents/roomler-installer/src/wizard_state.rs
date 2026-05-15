//! Wizard state persistence.
//!
//! W2 in the rc.28 plan. The wizard's 5-step flow lives across
//! enough operator effort (paste token, type device name, confirm
//! flavour) that a force-killed wizard losing its progress would
//! be a meaningful regression vs. the rc.18 manual ritual it
//! replaces. Persisting form fields + step pointer to
//! `%LOCALAPPDATA%\roomler\roomler-installer\wizard-state.json`
//! lets a relaunch resume where the operator left off.
//!
//! ## What is NOT persisted (H5 from the plan critique)
//!
//! The enrollment token is NEVER written to disk. If the operator
//! had pasted a token and the wizard died, the resume drops them
//! on the Token step asking to paste again. Acceptable UX cost:
//! tokens are short-lived (typically minutes), and the operator
//! generates them in the admin UI seconds before pasting.
//!
//! `device_name` and `server_url` ARE persisted because they're
//! benign + slow to retype + already public (server_url is a
//! domain, device_name is the hostname).
//!
//! ## Failure modes
//!
//! - Missing file → `Default` state (start from Welcome).
//! - Corrupt JSON → log warning, return `Default`. Operator sees
//!   a fresh wizard, no error popup.
//! - Older schema (version mismatch) → log warning, return `Default`.
//!   v2+ migrations will key off the `version` field if we ever
//!   bump beyond 1.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Schema version of the on-disk JSON. Bump when the shape changes
/// in an incompatible way; older files are dropped and the operator
/// restarts from Welcome. v1 was the initial schema.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Wizard step pointer. Tagged on `step` in JSON to keep the file
/// human-readable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Welcome + existing-install detection (Step 1).
    #[default]
    Welcome,
    /// Server URL + device name (Step 2).
    Server,
    /// Enrollment token paste (Step 3). Resuming here always
    /// re-renders the token field empty — we never persist tokens.
    Token,
    /// Install in progress (Step 4). Resuming here on a fresh
    /// wizard means the previous install was interrupted mid-flight;
    /// the SPA returns to Welcome so the operator can re-detect
    /// before retrying.
    Install,
    /// Done (Step 5). A resume from here is the "happy path
    /// confirmation" — wizard re-shows the agent_id + tenant_id.
    Done,
}

/// Persisted wizard state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WizardState {
    /// Schema version (see [`CURRENT_SCHEMA_VERSION`]).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Resume target.
    #[serde(default)]
    pub step: Step,
    /// Selected install flavour. `None` while the operator hasn't
    /// chosen yet. Values: `"peruser"`, `"permachine"`,
    /// `"permachine-system-context"`.
    #[serde(default)]
    pub flavour: Option<String>,
    /// Roomler server URL. Defaults to `https://roomler.ai` via
    /// the SPA's pre-fill; the field stays empty in fresh state.
    #[serde(default)]
    pub server_url: String,
    /// Device name pre-filled from hostname.
    #[serde(default)]
    pub device_name: String,
}

fn default_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// Resolve the on-disk location of `wizard-state.json`.
///
/// Windows: `%LOCALAPPDATA%\roomler\roomler-installer\wizard-state.json`.
/// macOS:   `~/Library/Application Support/ai.roomler.roomler-installer/wizard-state.json`.
/// Linux:   `~/.local/share/roomler-installer/wizard-state.json`.
pub fn default_state_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("ai", "roomler", "roomler-installer")
        .context("could not determine local data dir for roomler-installer")?;
    Ok(dirs.data_local_dir().join("wizard-state.json"))
}

/// Load wizard state from `path`. Returns `Default` (start from
/// Welcome) when the file is missing, malformed, or from an older
/// schema. Never errors — the wizard always has a state to render.
pub fn load(path: &std::path::Path) -> WizardState {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WizardState::default(),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "wizard-state read failed; resetting");
            return WizardState::default();
        }
    };
    let state: WizardState = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "wizard-state JSON parse failed; resetting");
            return WizardState::default();
        }
    };
    if state.version != CURRENT_SCHEMA_VERSION {
        tracing::warn!(
            file_version = state.version,
            expected = CURRENT_SCHEMA_VERSION,
            "wizard-state schema mismatch; resetting"
        );
        return WizardState::default();
    }
    state
}

/// Save wizard state to `path`. Creates parent dirs as needed.
/// Errors propagate — the SPA renders them inline so the operator
/// can investigate (most likely cause: permissions on
/// `%LOCALAPPDATA%`, which would be unusual).
pub fn save(path: &std::path::Path, state: &WizardState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let serialised = serde_json::to_string_pretty(state).context("serialising wizard state")?;
    std::fs::write(path, serialised)
        .with_context(|| format!("writing wizard state to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trips_full_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("wizard-state.json");
        let original = WizardState {
            version: CURRENT_SCHEMA_VERSION,
            step: Step::Token,
            flavour: Some("permachine-system-context".to_string()),
            server_url: "https://roomler.ai".to_string(),
            device_name: "PC50045".to_string(),
        };
        save(&path, &original).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, original);
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        let loaded = load(&path);
        assert_eq!(loaded, WizardState::default());
        assert_eq!(loaded.step, Step::Welcome);
    }

    #[test]
    fn corrupt_json_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("corrupt.json");
        std::fs::write(&path, "not json at all { ].").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, WizardState::default());
    }

    #[test]
    fn schema_version_mismatch_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("v999.json");
        std::fs::write(
            &path,
            r#"{"version":999,"step":"token","server_url":"x","device_name":"y"}"#,
        )
        .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, WizardState::default());
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c/wizard-state.json");
        let state = WizardState {
            version: CURRENT_SCHEMA_VERSION,
            step: Step::Server,
            ..Default::default()
        };
        save(&nested, &state).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn step_serialises_as_snake_case() {
        let state = WizardState {
            version: CURRENT_SCHEMA_VERSION,
            step: Step::Welcome,
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains(r#""step":"welcome""#));
    }

    #[test]
    fn missing_optional_fields_in_json_default_correctly() {
        // Operator could have an old build that wrote a partial JSON
        // shape; missing fields should default rather than fail parse.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("partial.json");
        std::fs::write(&path, r#"{"version":1,"step":"server"}"#).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.step, Step::Server);
        assert_eq!(loaded.server_url, "");
        assert_eq!(loaded.device_name, "");
        assert_eq!(loaded.flavour, None);
    }
}
