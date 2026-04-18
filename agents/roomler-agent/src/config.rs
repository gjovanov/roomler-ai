//! Agent on-disk configuration.
//!
//! Stored at `<user config dir>/roomler-agent/config.toml`. On Linux that
//! resolves to `$XDG_CONFIG_HOME/roomler-agent/` or `~/.config/roomler-agent/`.
//!
//! The file holds the enrolled agent's identity + its long-lived agent token
//! + the server URL. It is the user's responsibility to keep the file mode
//! 0600 — on Linux/macOS we set that on write.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Base URL of the Roomler API, e.g. `https://roomler.live`. No trailing slash.
    pub server_url: String,
    /// Derived WSS URL; recomputed from `server_url` if absent.
    #[serde(default)]
    pub ws_url: Option<String>,
    /// Opaque agent JWT issued by `/api/agent/enroll`.
    pub agent_token: String,
    /// Server-assigned agent id (hex ObjectId).
    pub agent_id: String,
    /// Server-assigned tenant id (hex ObjectId).
    pub tenant_id: String,
    /// Stable machine fingerprint. Persisted so re-enrollment maps to the
    /// same `agents` row.
    pub machine_id: String,
    /// User-friendly name shown in the admin UI.
    pub machine_name: String,
    /// Encoder preference: `auto` (default), `hardware`, or `software`.
    /// Can be overridden at launch by `ROOMLER_AGENT_ENCODER` env var or
    /// `--encoder` CLI flag.
    #[serde(default)]
    pub encoder_preference: EncoderPreferenceChoice,
}

/// TOML-friendly mirror of `encode::EncoderPreference`. Kept separate so
/// the `encode` module stays CLI-independent and the config file survives
/// feature gating without needing the `mf-encoder` feature enabled to
/// parse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderPreferenceChoice {
    #[default]
    Auto,
    Hardware,
    Software,
}

impl AgentConfig {
    pub fn ws_url(&self) -> String {
        if let Some(url) = &self.ws_url {
            return url.clone();
        }
        derive_ws_url(&self.server_url)
    }
}

/// Resolve the default config path. Can be overridden by `--config` on the CLI.
pub fn default_config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("live", "roomler", "roomler-agent")
        .context("could not resolve a platform config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn load(path: &PathBuf) -> Result<AgentConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    let cfg: AgentConfig = toml::from_str(&raw)
        .with_context(|| format!("parsing config at {}", path.display()))?;
    Ok(cfg)
}

pub fn save(path: &PathBuf, cfg: &AgentConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let serialised = toml::to_string_pretty(cfg).context("serialising config")?;
    std::fs::write(path, serialised)
        .with_context(|| format!("writing config to {}", path.display()))?;

    // Tighten permissions on Unix — the file holds a bearer token.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn derive_ws_url(http_url: &str) -> String {
    let base = http_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws}/ws")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_from_https() {
        assert_eq!(derive_ws_url("https://roomler.live"), "wss://roomler.live/ws");
    }

    #[test]
    fn ws_url_from_http_localhost() {
        assert_eq!(
            derive_ws_url("http://localhost:3000"),
            "ws://localhost:3000/ws"
        );
    }

    #[test]
    fn ws_url_strips_trailing_slash() {
        assert_eq!(
            derive_ws_url("https://roomler.live/"),
            "wss://roomler.live/ws"
        );
    }
}
