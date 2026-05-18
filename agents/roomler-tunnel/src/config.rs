//! On-disk config for `roomler-tunnel`.
//!
//! Mirrors `roomler-agent`'s pattern: TOML file at the platform-specific
//! per-user config dir, populated by `roomler-tunnel enroll` and read
//! by `forward` / `run` / `diagnose`. Env vars `ROOMLER_TUNNEL_SERVER`
//! and `ROOMLER_TUNNEL_TOKEN` override the file when both are set, so
//! CI / smoke tests don't need to drop a real file.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    /// Base URL of the Roomler API (no trailing slash needed,
    /// e.g. `https://roomler.ai`). Used to derive the WS URL by
    /// rewriting the scheme to `wss://` and appending `/ws`.
    pub server_url: String,

    /// Long-lived `TunnelClient` JWT (audience = `tunnel-client`).
    /// Issued by `POST /api/tunnel-client/enroll` in exchange for
    /// the operator's `TunnelEnrollment` token.
    pub tunnel_client_token: String,

    /// Optional friendly name for diagnostics. Mirrors the agent's
    /// `machine_name`; populated by `enroll --name`.
    #[serde(default)]
    pub machine_name: String,
}

/// Resolve the default per-user config path. On Windows this lands at
/// `%APPDATA%\roomler\roomler-tunnel\config.toml`; on Linux/macOS the
/// `directories` crate's `ProjectDirs::config_dir()` is honoured.
pub fn default_config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "roomler", "roomler-tunnel")
        .context("no platform config dir available for roomler-tunnel")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Load config from `path` (or the default). Env-var overrides
/// short-circuit the file read when BOTH `ROOMLER_TUNNEL_SERVER` and
/// `ROOMLER_TUNNEL_TOKEN` are set — useful for CI / smoke tests where
/// dropping a file is more friction than setting two env vars.
pub fn load(path: Option<PathBuf>) -> Result<TunnelConfig> {
    if let (Ok(server), Ok(token)) = (
        std::env::var("ROOMLER_TUNNEL_SERVER"),
        std::env::var("ROOMLER_TUNNEL_TOKEN"),
    ) {
        return Ok(TunnelConfig {
            server_url: server,
            tunnel_client_token: token,
            machine_name: std::env::var("ROOMLER_TUNNEL_NAME").unwrap_or_default(),
        });
    }

    let resolved = match path {
        Some(p) => p,
        None => default_config_path()?,
    };
    if !resolved.exists() {
        bail!(
            "tunnel-client config not found at {}. Run `roomler-tunnel enroll --server <url> --token <jwt> --name <label>` first.",
            resolved.display()
        );
    }
    let s = std::fs::read_to_string(&resolved)
        .with_context(|| format!("reading tunnel config {}", resolved.display()))?;
    let cfg: TunnelConfig = toml::from_str(&s)
        .with_context(|| format!("parsing tunnel config {}", resolved.display()))?;
    if cfg.tunnel_client_token.is_empty() {
        bail!(
            "tunnel-client config at {} has empty tunnel_client_token. Re-run `roomler-tunnel enroll`.",
            resolved.display()
        );
    }
    Ok(cfg)
}

/// Persist `cfg` to `path` (or the default). Creates the parent
/// directory if absent. Idempotent overwrite.
pub fn save(cfg: &TunnelConfig, path: Option<&Path>) -> Result<PathBuf> {
    let target = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path()?,
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating tunnel config dir {}", parent.display()))?;
    }
    let serialised = toml::to_string_pretty(cfg).context("serialising tunnel config")?;
    std::fs::write(&target, serialised)
        .with_context(|| format!("writing tunnel config {}", target.display()))?;
    Ok(target)
}

/// Convert `server_url` into the WS URL the agent dials. Honours
/// `http://` → `ws://` and `https://` → `wss://`. Trims any trailing
/// slash so we don't end up with `//ws`.
pub fn derive_ws_url(server_url: &str) -> Result<String> {
    let trimmed = server_url.trim_end_matches('/');
    let ws = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        bail!("server_url must be http(s):// or ws(s)://, got {server_url}");
    };
    Ok(format!("{ws}/ws"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_ws_url_https_becomes_wss() {
        assert_eq!(
            derive_ws_url("https://roomler.ai").unwrap(),
            "wss://roomler.ai/ws"
        );
    }

    #[test]
    fn derive_ws_url_strips_trailing_slash() {
        assert_eq!(
            derive_ws_url("https://roomler.ai/").unwrap(),
            "wss://roomler.ai/ws"
        );
    }

    #[test]
    fn derive_ws_url_http_becomes_ws() {
        assert_eq!(
            derive_ws_url("http://localhost:3000").unwrap(),
            "ws://localhost:3000/ws"
        );
    }

    #[test]
    fn derive_ws_url_passes_ws_through() {
        assert_eq!(
            derive_ws_url("wss://roomler.ai").unwrap(),
            "wss://roomler.ai/ws"
        );
    }

    #[test]
    fn derive_ws_url_rejects_unknown_scheme() {
        assert!(derive_ws_url("ftp://example.com").is_err());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tunnel.toml");
        let cfg = TunnelConfig {
            server_url: "https://test.example".to_string(),
            tunnel_client_token: "test-jwt".to_string(),
            machine_name: "test-laptop".to_string(),
        };
        save(&cfg, Some(&path)).unwrap();
        let loaded = load(Some(path)).unwrap();
        assert_eq!(loaded.server_url, cfg.server_url);
        assert_eq!(loaded.tunnel_client_token, cfg.tunnel_client_token);
        assert_eq!(loaded.machine_name, cfg.machine_name);
    }

    #[test]
    fn load_rejects_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tunnel.toml");
        let cfg = TunnelConfig {
            server_url: "https://test.example".to_string(),
            tunnel_client_token: String::new(),
            machine_name: String::new(),
        };
        save(&cfg, Some(&path)).unwrap();
        let err = load(Some(path)).unwrap_err();
        assert!(
            err.to_string().contains("empty tunnel_client_token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_missing_file_has_helpful_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.toml");
        let err = load(Some(path)).unwrap_err();
        assert!(err.to_string().contains("roomler-tunnel enroll"));
    }
}
