//! Tunnel-enroll shim — the pure HTTP half lives in
//! `wizard_shared::tunnel_enroll` since P4a; this module keeps the
//! legacy call surface (`crate::enroll::{enroll, write_config,
//! EnrollResult}`) byte-identical for the orchestrator and tests,
//! plus the `roomler_tunnel`-coupled [`write_config`] half (the
//! shared core stays free of the tunnel dep).

use std::path::Path;

use anyhow::{Context, Result};

pub use wizard_shared::tunnel_enroll::EnrollResult;

/// Historical wire-visible User-Agent for this wizard's enroll POST —
/// preserved verbatim through the P4a extraction (the moved core fn
/// takes the UA as a parameter).
const USER_AGENT: &str = concat!("roomler-tunnel-installer/", env!("CARGO_PKG_VERSION"));

/// POST the enrollment exchange — legacy signature preserved; the
/// moved implementation gains a `user_agent` parameter which this
/// shim pins to the historical value.
pub async fn enroll(
    server_url: &str,
    enrollment_token: &str,
    machine_name: &str,
    client_version: &str,
) -> Result<EnrollResult> {
    wizard_shared::tunnel_enroll::enroll(
        server_url,
        enrollment_token,
        machine_name,
        client_version,
        USER_AGENT,
    )
    .await
}

/// Persist the enroll result to the tunnel CLI's config path. Returns
/// the path written so the orchestrator can surface it in the Done
/// step.
///
/// `override_path` lets unit/integration tests redirect the write to
/// a tempdir without touching the operator's real %APPDATA% layout.
pub fn write_config(
    result: &EnrollResult,
    override_path: Option<&Path>,
) -> Result<std::path::PathBuf> {
    let cfg = roomler_tunnel::config::TunnelConfig {
        server_url: result.server_url.clone(),
        tunnel_client_token: result.tunnel_client_token.clone(),
        machine_name: result.machine_name.clone(),
    };
    let path =
        roomler_tunnel::config::save(&cfg, override_path).context("writing tunnel config.toml")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_config_creates_file_at_override_path() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let r = EnrollResult {
            tunnel_client_id: "507f1f77bcf86cd799439011".to_string(),
            tenant_id: "507f191e810c19729de860ea".to_string(),
            tunnel_client_token: "tok".to_string(),
            server_url: "https://roomler.ai".to_string(),
            machine_name: "lap".to_string(),
        };
        let written = write_config(&r, Some(&path)).unwrap();
        assert_eq!(written, path);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("server_url"));
        assert!(contents.contains("tunnel_client_token"));
        assert!(contents.contains("lap"));
        // Token bytes themselves should be present too (we wrote them).
        assert!(contents.contains("tok"));
    }
}
