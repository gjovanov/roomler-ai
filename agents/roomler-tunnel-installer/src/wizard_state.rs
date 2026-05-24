//! Wizard-state persistence.
//!
//! The wizard writes its in-progress state to
//! `<config_dir>/wizard-state.json` on every form-blur from the SPA
//! so a force-killed (or crash-rebooted) wizard resumes mid-flow on
//! the next launch.
//!
//! **The enrollment token is NEVER persisted in plaintext** (same
//! invariant as the agent wizard's H5). If the operator killed the
//! wizard mid-flow with a token already pasted, the resume drops
//! them on the Token step asking to paste again. This protects
//! against the wizard leaving an enrollment JWT readable by other
//! users on a shared machine.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Steps the wizard can be parked at when state is persisted. Numeric
/// order matches the operator's progression.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WizardStep {
    #[default]
    Welcome,
    Server,
    Token,
    Install,
    Done,
}

/// Persisted form data + step pointer. Token is deliberately not in
/// this struct so a corrupted-file replay can't leak it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WizardState {
    /// Schema version. Bumped when the struct changes shape so older
    /// state files trigger a Default-reset rather than a serde panic.
    pub schema_version: u32,
    /// What step the operator was on when the state was last saved.
    pub step: WizardStep,
    /// Server URL the operator entered (or the default).
    pub server_url: String,
    /// Device name the operator entered (or the default = hostname).
    pub device_name: String,
}

/// Current schema. Bump when the struct shape changes so older state
/// files don't fail to deserialise mid-flow.
pub const CURRENT_SCHEMA: u32 = 1;

/// Resolve the path the wizard reads/writes state to.
/// `%LOCALAPPDATA%\roomler\roomler-tunnel-installer\wizard-state.json`
/// on Windows; `~/.config/roomler-tunnel-installer/wizard-state.json`
/// on Linux; `~/Library/Application Support/roomler-tunnel-installer/
/// wizard-state.json` on macOS.
pub fn default_state_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "roomler", "roomler-tunnel-installer")
        .context("resolving wizard state dir via directories::ProjectDirs")?;
    Ok(dirs.data_local_dir().join("wizard-state.json"))
}

/// Load the persisted state. Returns `Default` (= Welcome step,
/// empty fields) when the file doesn't exist, is malformed, or has a
/// schema version we don't recognise. NEVER panics — a missing or
/// corrupted file resets the wizard, it doesn't block it.
pub fn load(path: &std::path::Path) -> WizardState {
    let Ok(bytes) = std::fs::read(path) else {
        return WizardState::default();
    };
    let Ok(parsed) = serde_json::from_slice::<WizardState>(&bytes) else {
        return WizardState::default();
    };
    if parsed.schema_version != CURRENT_SCHEMA {
        return WizardState::default();
    }
    parsed
}

/// Persist the state to `path`. Creates the parent dir if missing.
/// Token is NOT written — caller never passes one in the struct.
pub fn save(path: &std::path::Path, state: &WizardState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating wizard state dir {}", parent.display()))?;
    }
    let mut bumped = state.clone();
    bumped.schema_version = CURRENT_SCHEMA;
    let bytes = serde_json::to_vec_pretty(&bumped).context("serialising wizard state")?;
    std::fs::write(path, bytes)
        .with_context(|| format!("writing wizard state {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nope.json");
        let state = load(&path);
        assert_eq!(state, WizardState::default());
        assert_eq!(state.step, WizardStep::Welcome);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        let state = WizardState {
            schema_version: CURRENT_SCHEMA,
            step: WizardStep::Token,
            server_url: "https://roomler.ai".to_string(),
            device_name: "field-laptop".to_string(),
        };
        save(&path, &state).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, state);
    }

    #[test]
    fn save_bumps_schema_version() {
        // Caller may pass schema_version=0 (default); save() should
        // upgrade it to CURRENT_SCHEMA on write so load() doesn't
        // immediately reject the file on next launch.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        let state = WizardState {
            schema_version: 0,
            step: WizardStep::Server,
            ..Default::default()
        };
        save(&path, &state).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.schema_version, CURRENT_SCHEMA);
        assert_eq!(loaded.step, WizardStep::Server);
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let state = load(&path);
        assert_eq!(state, WizardState::default());
    }

    #[test]
    fn load_mismatched_schema_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        // Future schema we don't know about.
        let bytes = br#"{
            "schemaVersion": 999,
            "step": "token",
            "serverUrl": "https://elsewhere",
            "deviceName": "x"
        }"#;
        std::fs::write(&path, bytes).unwrap();
        let state = load(&path);
        assert_eq!(state, WizardState::default());
    }

    #[test]
    fn wizard_state_has_no_token_field() {
        // Compile-time contract: `WizardState` does not expose a
        // `token` field. Anyone adding one breaks the H5 invariant
        // (the rc.28 plan's token-never-persisted rule). This test
        // exists so a code-review-skipped commit can't slip the field
        // back in.
        let json = serde_json::to_string(&WizardState::default()).unwrap();
        assert!(!json.to_ascii_lowercase().contains("token"), "{json}");
    }

    #[test]
    fn step_serialises_as_kebab_case() {
        let json = serde_json::to_string(&WizardStep::Install).unwrap();
        assert_eq!(json, "\"install\"");
    }
}
