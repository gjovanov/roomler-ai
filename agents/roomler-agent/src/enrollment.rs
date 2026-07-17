//! One-shot enrollment exchange.
//!
//! Flow: admin issues an enrollment token in the Roomler UI and hands it to
//! the machine operator. `roomler-agent enroll --token <t>` posts it to
//! `POST /api/agent/enroll` with machine metadata, gets back a long-lived
//! agent token, and persists everything to the config file.

use anyhow::{Context, Result, bail};
use roomler_ai_remote_control::models::OsKind;
use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;

#[derive(Debug, Serialize)]
struct EnrollRequest<'a> {
    enrollment_token: &'a str,
    machine_id: &'a str,
    machine_name: &'a str,
    os: OsKind,
    agent_version: &'a str,
}

#[derive(Debug, Deserialize)]
struct EnrollResponse {
    agent_id: String,
    tenant_id: String,
    agent_token: String,
}

pub struct EnrollInputs<'a> {
    pub server_url: &'a str,
    pub enrollment_token: &'a str,
    pub machine_id: &'a str,
    pub machine_name: &'a str,
}

pub async fn enroll(inputs: EnrollInputs<'_>) -> Result<AgentConfig> {
    // Promote http:// to https://. The production ingress 301-redirects
    // plaintext to TLS; reqwest then downgrades the POST to a GET (RFC
    // 7231 historical behavior for 301/302) so the second hop hits a
    // route that exists for POST but not GET, producing a 405. Doing the
    // upgrade upfront also keeps the enrollment token off the wire in
    // cleartext, and ensures the stored server_url derives wss:// (not
    // ws://) for the long-lived signaling connection.
    let server_url = normalize_server_url(inputs.server_url);
    let url = format!("{server_url}/api/agent/enroll");
    let os = detect_os();
    let agent_version = env!("CARGO_PKG_VERSION");

    tracing::info!(%url, os = ?os, "posting enrollment");

    let resp = reqwest::Client::new()
        .post(&url)
        .json(&EnrollRequest {
            enrollment_token: inputs.enrollment_token,
            machine_id: inputs.machine_id,
            machine_name: inputs.machine_name,
            os,
            agent_version,
        })
        .send()
        .await
        .context("POST /api/agent/enroll")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("enrollment rejected (status {status}): {body}");
    }

    let body: EnrollResponse = resp.json().await.context("parsing enroll response")?;

    Ok(AgentConfig {
        server_url,
        ws_url: None,
        agent_token: body.agent_token,
        agent_id: body.agent_id,
        tenant_id: body.tenant_id,
        machine_id: inputs.machine_id.to_string(),
        machine_name: inputs.machine_name.to_string(),
        encoder_preference: crate::config::EncoderPreferenceChoice::default(),
        update_check_interval_h: None,
        enable_remote_browse: true,
        auto_grant_session: true,
        last_known_good_version: None,
        crash_count: 0,
        last_crash_unix: 0,
        rollback_attempted: false,
        last_run_unhealthy: false,
        // Stamp the current schema version directly on enrollment so
        // a fresh install skips the rc.18 migration on first launch.
        config_schema_version: Some(crate::config::CURRENT_SCHEMA_VERSION.to_string()),
        // T2.8 default = enabled + empty allowlist (trust server).
        forward_acl: crate::tunnel::acl::AgentForwardAcl::default(),
        // Remote app-launch: default = enabled with a seeded bash/tmux entry.
        virtual_desktop_apps: crate::apps::VirtualDesktopAppsConfig::default(),
        // Phase 3b: overlay opt-in, off until the operator enables it.
        overlay_enabled: false,
        overlay_wg_secret_key: None,
        // Phase 1: no advertised subnet routes until the operator configures them.
        overlay_advertised_routes: Vec::new(),
        advertise_routes: Vec::new(),
        advertise_local_subnets: true,
        tunnel_routes: Vec::new(),
    })
}

/// Strip the trailing slash and force the scheme to `https://` if the
/// caller supplied `http://`. Any other scheme (or a bare host) is
/// returned trimmed but otherwise untouched — `https://` URLs stay
/// `https://`, and a malformed input is left to fail at the reqwest
/// layer with a clearer diagnostic than we'd produce here.
///
/// **Loopback is exempt**: `http://127.0.0.1`, `http://localhost`, `http://[::1]`
/// stay `http://`. A loopback address has no off-host network path, so there's
/// no MITM to defend against — and dev / test / CI servers run plaintext on
/// loopback (the integration `TestApp` binds `http://127.0.0.1:<port>`). Forcing
/// TLS there just breaks the enroll POST with a `wrong version number` SSL error.
/// A remote host (the production case) is still upgraded.
fn normalize_server_url(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("http://") {
        if is_loopback_authority(rest) {
            return trimmed.to_string();
        }
        tracing::warn!(
            original = trimmed,
            "upgrading http:// to https:// — enrollment tokens must travel over TLS"
        );
        return format!("https://{rest}");
    }
    trimmed.to_string()
}

/// Is the `host[:port][/path]` authority a loopback host? Handles
/// `127.0.0.1:41003`, `localhost`, `[::1]:8080`, and any `127.0.0.0/8` /
/// IPv6-loopback literal.
fn is_loopback_authority(after_scheme: &str) -> bool {
    // Drop any path, then the port. Bracketed IPv6 keeps its `:`s until the
    // brackets are stripped, so split the path first, then rsplit the port only
    // when the last segment can't be part of an unbracketed host.
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = if let Some(inner) = authority.strip_prefix('[') {
        // `[::1]:8080` → `::1`
        inner.split(']').next().unwrap_or(inner)
    } else if let Some((h, _port)) = authority.rsplit_once(':') {
        // Only treat the tail as a port if the head still looks like a host
        // (an unbracketed IPv6 has multiple `:` — leave it whole for the parse).
        if h.contains(':') { authority } else { h }
    } else {
        authority
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn detect_os() -> OsKind {
    match std::env::consts::OS {
        "linux" => OsKind::Linux,
        "macos" => OsKind::Macos,
        "windows" => OsKind::Windows,
        other => {
            tracing::warn!(%other, "unknown OS, defaulting to Linux");
            OsKind::Linux
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_is_promoted_to_https() {
        assert_eq!(
            normalize_server_url("http://roomler.ai"),
            "https://roomler.ai"
        );
        assert_eq!(
            normalize_server_url("http://roomler.ai/"),
            "https://roomler.ai"
        );
        assert_eq!(
            normalize_server_url("http://10.0.0.5:3000"),
            "https://10.0.0.5:3000"
        );
    }

    #[test]
    fn http_loopback_is_not_promoted() {
        // Loopback has no off-host path to MITM — keep it plaintext so a dev /
        // test / CI server on 127.0.0.1 (the integration `TestApp`) enrolls.
        assert_eq!(
            normalize_server_url("http://127.0.0.1:41003"),
            "http://127.0.0.1:41003"
        );
        assert_eq!(
            normalize_server_url("http://localhost:5001/"),
            "http://localhost:5001"
        );
        assert_eq!(
            normalize_server_url("http://[::1]:8080"),
            "http://[::1]:8080"
        );
        assert_eq!(normalize_server_url("http://127.5.5.5"), "http://127.5.5.5");
        // A non-loopback private IP is still upgraded (only loopback is exempt).
        assert_eq!(
            normalize_server_url("http://192.168.1.10:3000"),
            "https://192.168.1.10:3000"
        );
    }

    #[test]
    fn https_is_left_alone() {
        assert_eq!(
            normalize_server_url("https://roomler.ai"),
            "https://roomler.ai"
        );
        assert_eq!(
            normalize_server_url("https://roomler.ai/"),
            "https://roomler.ai"
        );
    }

    #[test]
    fn does_not_upgrade_unrelated_schemes_or_bare_hosts() {
        // We don't validate — the reqwest call will fail with a clearer
        // error than we could produce here. Just confirm we don't
        // accidentally rewrite these.
        assert_eq!(normalize_server_url("roomler.ai"), "roomler.ai");
        assert_eq!(normalize_server_url("file:///tmp/foo"), "file:///tmp/foo");
    }
}
