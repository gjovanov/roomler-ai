//! Proxy-base resolution for the unified wizard — the identity half
//! the legacy wrappers kept per-crate, re-owned by this app.
//!
//! `wizard_shared::asset_resolver` owns the wire mechanics (health
//! GET, streaming download, SHA256); the CALLER owns everything
//! identity-shaped — proxy bases + their env overrides, the
//! User-Agent, and origin/suffix stripping. This module is that
//! identity for `roomler-setup`.
//!
//! The wizards hit roomler.ai's installer proxies (`/api/agent/
//! installer/{flavour}`, `/api/tunnel/installer/{platform}`) instead
//! of downloading directly from `github.com`: corporate ESET /
//! Defender allow-lists are typically per-domain; `roomler.ai`'s TLS
//! cert is already trusted by IT, `github.com` is often blocked
//! outright (field repro 2026-05-11).

/// Wire-visible User-Agent for every HTTP call this app makes
/// (health GET, artifact download, tunnel-enroll POST).
pub const USER_AGENT: &str = concat!("roomler-setup/", env!("CARGO_PKG_VERSION"));

/// Default proxy base for the daemon MSI endpoint family.
const DEFAULT_AGENT_BASE: &str = "https://roomler.ai/api/agent/installer";

/// Default proxy base for the tunnel-CLI archive endpoint family.
/// NB: `/api/tunnel/installer` serves the `roomler-tunnel` CLI
/// tarball; the (P4c-2-retired) `/api/tunnel-wizard` family served
/// the wizard EXE itself — pointing here at the latter made the
/// wizard install ITSELF (rc.60 bug, fixed rc.61).
const DEFAULT_TUNNEL_BASE: &str = "https://roomler.ai/api/tunnel/installer";

/// Resolve the daemon-MSI proxy base at runtime. Env override
/// `ROOMLER_INSTALLER_PROXY_BASE` (inherited from the legacy agent
/// wizard so staging fixtures keep working) — else the production
/// default. Always returned without a trailing slash.
pub fn agent_base() -> String {
    let raw = std::env::var("ROOMLER_INSTALLER_PROXY_BASE")
        .ok()
        .unwrap_or_else(|| DEFAULT_AGENT_BASE.to_string());
    normalise(&raw)
}

/// Resolve the tunnel-CLI proxy base at runtime. Env override chain
/// `ROOMLER_TUNNEL_CLI_PROXY_BASE` → legacy alias
/// `ROOMLER_TUNNEL_WIZARD_PROXY_BASE` (back-compat with any test
/// fixture that pre-dates the rc.61 rename) → the production default.
/// Always returned without a trailing slash.
pub fn tunnel_base() -> String {
    let raw = std::env::var("ROOMLER_TUNNEL_CLI_PROXY_BASE")
        .ok()
        .or_else(|| std::env::var("ROOMLER_TUNNEL_WIZARD_PROXY_BASE").ok())
        .unwrap_or_else(|| DEFAULT_TUNNEL_BASE.to_string());
    normalise(&raw)
}

/// Pure: strip the trailing slash from `raw`. Extracted so tests can
/// exercise the parsing logic without racing on `std::env`.
fn normalise(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

/// Pure: the origin (scheme://host[:port]) part of a proxy base —
/// used when composing absolute URLs from `health.uri`, which already
/// starts with the `/api/...` path, so concatenation must not double
/// up. Strips the canonical `/api/agent/installer` and `/api/tunnel/
/// installer` suffixes plus the legacy `/api/tunnel-wizard` alias
/// (retired server-side in P4c-2; the strip stays as paste-tolerance
/// for old URLs, unifying the two per-wizard strip fns); anything
/// else passes
/// through unchanged (custom env var without the path segment — e.g.
/// a staging proxy that injects the path internally).
pub fn origin_of(base: &str) -> String {
    for suffix in [
        "/api/agent/installer",
        "/api/tunnel/installer",
        "/api/tunnel-wizard",
    ] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_strips_trailing_slash() {
        assert_eq!(
            normalise("https://staging.local/api/agent/installer/"),
            "https://staging.local/api/agent/installer"
        );
        assert_eq!(
            normalise("https://roomler.ai/api/tunnel/installer"),
            "https://roomler.ai/api/tunnel/installer"
        );
    }

    #[test]
    fn origin_of_strips_agent_suffix() {
        assert_eq!(
            origin_of("https://roomler.ai/api/agent/installer"),
            "https://roomler.ai"
        );
    }

    #[test]
    fn origin_of_strips_tunnel_suffix() {
        assert_eq!(
            origin_of("https://roomler.ai/api/tunnel/installer"),
            "https://roomler.ai"
        );
    }

    #[test]
    fn origin_of_strips_legacy_wizard_suffix() {
        assert_eq!(
            origin_of("https://staging.local/api/tunnel-wizard"),
            "https://staging.local"
        );
    }

    #[test]
    fn origin_of_passthrough_when_suffix_absent() {
        assert_eq!(origin_of("https://staging.local"), "https://staging.local");
    }

    #[test]
    fn user_agent_carries_app_name_and_version() {
        assert!(USER_AGENT.starts_with("roomler-setup/"));
        assert!(USER_AGENT.len() > "roomler-setup/".len());
    }
}
