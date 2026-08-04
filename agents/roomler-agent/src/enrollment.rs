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
        // S2 env-bridged knobs: unset → built-in defaults.
        overlay_quic: None,
        overlay_direct: None,
        overlay_derp: None,
        overlay_mbb: None,
        overlay_lan_iface_filter: None,
        overlay_pathmon: None,
        overlay_route_events: None,
        overlay_route_tick_secs: None,
        overlay_relay_tls: None,
        overlay_tun_stable_guid: None,
        overlay_route_evict: None,
        overlay_tun_persist: None,
        overlay_route_metric0: None,
        local_turn: None,
        dns_aaaa: None,
        auto_update: None,
        logs_upload_disabled: None,
        rate_factor_h264: None,
        rate_factor_hevc: None,
        rate_factor_vp9: None,
        rate_factor_av1: None,
        ice_follow_renomination: None,
        ice_warm_standby: None,
        ice_overlay_host_deprioritize: None,
        overlay_tier_detect: None,
        overlay_rtt_q: None,
        relay_probe: None,
        text_mod_neutralize: None,
        overlay_demote: None,
        overlay_upward_probe: None,
        rc_max_sessions: None,
        overlay_rpf: None,
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
        // P5: not an exit node until the operator opts in.
        overlay_exit_node_enabled: false,
        // P5: not routing egress through a mesh exit node until configured.
        overlay_exit_node: None,
        advertise_routes: Vec::new(),
        advertise_local_subnets: true,
        tunnel_routes: Vec::new(),
        orgs: Vec::new(),
    })
}

/// Multi-org P1 — how [`apply_enrollment`] folded a fresh enrollment into
/// the on-disk config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollOutcome {
    /// No prior config at the path — the fresh config was written as-is.
    FreshPrimary,
    /// The enrollment resolved to the SAME (server, tenant) as the primary:
    /// identity refreshed, operator state preserved (rc.204 semantics).
    RefreshedPrimary,
    /// The enrollment resolved to an existing `[[orgs]]` entry: its token /
    /// agent_id refreshed in place.
    RefreshedOrg { label: String },
    /// A NEW (server, tenant) pair: appended as a secondary org.
    AppendedOrg { label: String },
    /// `--replace` forced the legacy whole-primary rebind.
    ReplacedPrimary,
}

/// Multi-org P1 — fold a fresh enrollment into an existing config (or none).
///
/// Dispatch, in order:
///   1. no existing config → fresh as-is (`FreshPrimary`);
///   2. `force_replace` → legacy primary rebind via
///      [`preserve_operator_config`] (`ReplacedPrimary`); any secondary
///      entry now duplicating the new primary identity is dropped;
///   3. (server, tenant) == the primary's → [`preserve_operator_config`]
///      (`RefreshedPrimary`) — this is exactly the pre-multi-org re-enroll;
///   4. (server, tenant) == a secondary entry's → refresh that entry's
///      token / agent_id in place (`RefreshedOrg`);
///   5. otherwise → APPEND a new secondary entry (`AppendedOrg`) with a
///      freshly minted WireGuard key — NEVER a copy of another org's (a
///      shared pubkey would let two orgs correlate this device).
///
/// On append the top-level `machine_name` is kept as-is even when the
/// operator passed a different `--name` (the new org's SERVER row got the
/// name from the enroll POST; the machine-scoped local name stays until
/// `roomler set-device-name` changes it everywhere).
pub fn apply_enrollment(
    existing: Option<AgentConfig>,
    fresh: AgentConfig,
    requested_label: Option<&str>,
    force_replace: bool,
) -> anyhow::Result<(AgentConfig, EnrollOutcome)> {
    let Some(existing) = existing else {
        return Ok((fresh, EnrollOutcome::FreshPrimary));
    };

    if force_replace {
        let mut merged = preserve_operator_config(fresh, existing);
        let (server, tenant) = (merged.server_url.clone(), merged.tenant_id.clone());
        merged
            .orgs
            .retain(|o| !(o.server_url == server && o.tenant_id == tenant));
        return Ok((merged, EnrollOutcome::ReplacedPrimary));
    }

    if existing.is_primary_identity(&fresh.server_url, &fresh.tenant_id) {
        return Ok((
            preserve_operator_config(fresh, existing),
            EnrollOutcome::RefreshedPrimary,
        ));
    }

    let mut cfg = existing;
    if let Some(org) = cfg.find_org_by_identity_mut(&fresh.server_url, &fresh.tenant_id) {
        org.agent_token = fresh.agent_token;
        org.agent_id = fresh.agent_id;
        org.ws_url = None;
        let label = org.label.clone();
        return Ok((cfg, EnrollOutcome::RefreshedOrg { label }));
    }

    let label = unique_org_label(&cfg, requested_label, &fresh.server_url)?;
    #[cfg_attr(
        not(any(feature = "overlay-l3", feature = "overlay-netstack")),
        allow(unused_mut)
    )]
    let mut entry = crate::config::OrgEntry {
        label: label.clone(),
        server_url: fresh.server_url,
        ws_url: None,
        agent_token: fresh.agent_token,
        agent_id: fresh.agent_id,
        tenant_id: fresh.tenant_id,
        enabled: true,
        overlay_mode: crate::config::OrgOverlayMode::Off,
        overlay_wg_secret_key: None,
        overlay_advertised_routes: Vec::new(),
        overlay_exit_node_enabled: false,
        advertise_routes: Vec::new(),
    };
    // Mint this org's OWN WireGuard identity now (builds without an overlay
    // surface leave it None; P2's first overlay-enabled start mints then,
    // mirroring the primary's lazy path in `run_cmd`).
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    {
        entry.overlay_wg_secret_key =
            Some(tunnel_core::overlay::WgKeypair::generate().secret_base64());
    }
    cfg.orgs.push(entry);
    Ok((cfg, EnrollOutcome::AppendedOrg { label }))
}

/// Pick a unique label for a new secondary org: the sanitized requested
/// label if given (hard error when invalid/taken — the operator named it
/// deliberately), else the server host sanitized + `-2`/`-3`… uniquifier.
fn unique_org_label(
    cfg: &AgentConfig,
    requested: Option<&str>,
    server_url: &str,
) -> anyhow::Result<String> {
    use crate::config::sanitize_org_label;
    if let Some(raw) = requested {
        let label = sanitize_org_label(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --label {raw:?}: use lowercase letters/digits/dashes \
                 (and not the reserved {:?})",
                crate::config::PRIMARY_ORG_LABEL
            )
        })?;
        if cfg.find_org(&label).is_some() {
            bail!("--label {label:?} is already in use (see `roomler-agent org ls`)");
        }
        return Ok(label);
    }
    let host = server_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(server_url);
    let base = sanitize_org_label(host).unwrap_or_else(|| "org".to_string());
    if cfg.find_org(&base).is_none() {
        return Ok(base);
    }
    for n in 2..100 {
        let candidate = format!("{base}-{n}");
        if cfg.find_org(&candidate).is_none() {
            return Ok(candidate);
        }
    }
    bail!("could not derive a unique org label from {server_url:?}; pass --label");
}

/// rc.204 — re-enrolling a machine that already has a config must NOT reset
/// operator state. Pre-rc.204, enroll wrote a wholesale-fresh [`AgentConfig`]:
/// a wizard re-install silently flipped `overlay_enabled` back to `false` (the
/// node dropped out of the overlay mesh on its next restart), dropped
/// `overlay_wg_secret_key` (forcing a WG key rotation on the next
/// overlay-enabled start), and wiped `tunnel_routes` / forward ACLs /
/// advertised routes / encoder preference (field-observed on NEO16,
/// 2026-07-21: the P4 wizard field-proofs re-enrolled the box and it fell out
/// of the mesh unnoticed). Keep the EXISTING config as the base — it carries
/// every operator-owned knob, including ones this function has never heard of
/// — and take only the enrollment-owned identity fields from the fresh one.
///
/// `ws_url` intentionally follows the FRESH config (i.e. resets to `None`): a
/// pinned override derived for the OLD server would break the new enrollment's
/// signaling connection, and the default derivation from `server_url` is
/// correct in every ordinary setup.
pub fn preserve_operator_config(fresh: AgentConfig, existing: AgentConfig) -> AgentConfig {
    AgentConfig {
        server_url: fresh.server_url,
        ws_url: fresh.ws_url,
        agent_token: fresh.agent_token,
        agent_id: fresh.agent_id,
        tenant_id: fresh.tenant_id,
        machine_id: fresh.machine_id,
        machine_name: fresh.machine_name,
        config_schema_version: fresh.config_schema_version,
        ..existing
    }
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

    /// rc.204 — a re-enroll over an existing config preserves every
    /// operator-owned knob (overlay opt-in + WG key, routes, ACL posture,
    /// encoder preference, declared tunnel routes) and takes ONLY the
    /// enrollment-owned identity fields from the fresh config.
    #[test]
    fn preserve_operator_config_keeps_operator_state_and_takes_identity() {
        let mut existing = crate::config::test_fixture();
        existing.overlay_enabled = true;
        existing.overlay_wg_secret_key = Some("OLD-WG-KEY".into());
        existing.overlay_advertised_routes = vec!["192.168.1.0/24".into()];
        existing.advertise_routes = vec!["10.9.0.0/16".into()];
        existing.encoder_preference = crate::config::EncoderPreferenceChoice::Software;
        existing.auto_grant_session = false;
        existing.last_known_good_version = Some("0.3.0-rc.199".into());

        let mut fresh = crate::config::test_fixture();
        fresh.server_url = "https://roomler.ai".into();
        fresh.agent_token = "NEW-TOKEN".into();
        fresh.agent_id = "NEW-AGENT-ID".into();
        fresh.tenant_id = "NEW-TENANT".into();
        fresh.machine_id = "NEW-MID".into();
        fresh.machine_name = "renamed-host".into();
        fresh.config_schema_version = Some("9".into());

        let merged = preserve_operator_config(fresh, existing);

        // Identity comes from the fresh enrollment…
        assert_eq!(merged.server_url, "https://roomler.ai");
        assert_eq!(merged.agent_token, "NEW-TOKEN");
        assert_eq!(merged.agent_id, "NEW-AGENT-ID");
        assert_eq!(merged.tenant_id, "NEW-TENANT");
        assert_eq!(merged.machine_id, "NEW-MID");
        assert_eq!(merged.machine_name, "renamed-host");
        assert_eq!(merged.config_schema_version.as_deref(), Some("9"));

        // …and the operator state survives the re-enroll.
        assert!(merged.overlay_enabled, "overlay opt-in must survive");
        assert_eq!(
            merged.overlay_wg_secret_key.as_deref(),
            Some("OLD-WG-KEY"),
            "the WG identity must survive (no forced key rotation)"
        );
        assert_eq!(merged.overlay_advertised_routes, vec!["192.168.1.0/24"]);
        assert_eq!(merged.advertise_routes, vec!["10.9.0.0/16"]);
        assert!(matches!(
            merged.encoder_preference,
            crate::config::EncoderPreferenceChoice::Software
        ));
        assert!(!merged.auto_grant_session);
        assert_eq!(
            merged.last_known_good_version.as_deref(),
            Some("0.3.0-rc.199")
        );
    }

    // ---- Multi-org P1: apply_enrollment dispatch --------------------------

    fn fresh_for(server: &str, tenant: &str, token: &str) -> AgentConfig {
        let mut f = crate::config::test_fixture();
        f.server_url = server.into();
        f.tenant_id = tenant.into();
        f.agent_token = token.into();
        f.agent_id = format!("aid-{tenant}");
        f
    }

    fn org(label: &str, server: &str, tenant: &str) -> crate::config::OrgEntry {
        crate::config::OrgEntry {
            label: label.into(),
            server_url: server.into(),
            ws_url: None,
            agent_token: format!("tok-{label}"),
            agent_id: format!("aid-{label}"),
            tenant_id: tenant.into(),
            enabled: true,
            overlay_mode: crate::config::OrgOverlayMode::Off,
            overlay_wg_secret_key: None,
            overlay_advertised_routes: Vec::new(),
            overlay_exit_node_enabled: false,
            advertise_routes: Vec::new(),
        }
    }

    #[test]
    fn apply_enrollment_fresh_when_no_existing() {
        let fresh = fresh_for("https://a.invalid", "t1", "tok1");
        let (cfg, outcome) = apply_enrollment(None, fresh.clone(), None, false).unwrap();
        assert_eq!(outcome, EnrollOutcome::FreshPrimary);
        assert_eq!(cfg.agent_token, fresh.agent_token);
        assert!(cfg.orgs.is_empty());
    }

    #[test]
    fn apply_enrollment_same_identity_refreshes_primary_and_keeps_orgs() {
        let mut existing = crate::config::test_fixture(); // server example.invalid / tenant tid
        existing.overlay_enabled = true;
        existing.orgs = vec![org("acme", "https://b.invalid", "t-acme")];
        let fresh = fresh_for("https://example.invalid", "tid", "NEW-TOK");
        let (cfg, outcome) = apply_enrollment(Some(existing), fresh, None, false).unwrap();
        assert_eq!(outcome, EnrollOutcome::RefreshedPrimary);
        assert_eq!(cfg.agent_token, "NEW-TOK");
        assert!(cfg.overlay_enabled, "operator state preserved");
        assert_eq!(cfg.orgs.len(), 1, "secondary enrollments must survive");
        assert_eq!(cfg.orgs[0].label, "acme");
    }

    #[test]
    fn apply_enrollment_matching_org_refreshes_in_place() {
        let mut existing = crate::config::test_fixture();
        existing.orgs = vec![org("acme", "https://b.invalid", "t-acme")];
        let fresh = fresh_for("https://b.invalid", "t-acme", "ROTATED");
        let (cfg, outcome) = apply_enrollment(Some(existing), fresh, None, false).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::RefreshedOrg {
                label: "acme".into()
            }
        );
        assert_eq!(cfg.orgs.len(), 1);
        assert_eq!(cfg.orgs[0].agent_token, "ROTATED");
        assert_eq!(cfg.orgs[0].agent_id, "aid-t-acme");
        // The primary identity is untouched.
        assert_eq!(cfg.agent_token, "tok");
    }

    #[test]
    fn apply_enrollment_new_identity_appends_secondary() {
        let existing = crate::config::test_fixture();
        let fresh = fresh_for("https://roomler.ai", "t-new", "tok-new");
        let (cfg, outcome) = apply_enrollment(Some(existing), fresh, None, false).unwrap();
        let EnrollOutcome::AppendedOrg { label } = outcome else {
            panic!("expected append, got {outcome:?}");
        };
        assert_eq!(label, "roomler-ai", "label derives from the server host");
        assert_eq!(cfg.orgs.len(), 1);
        let entry = &cfg.orgs[0];
        assert_eq!(entry.tenant_id, "t-new");
        assert!(entry.enabled);
        assert_eq!(entry.overlay_mode, crate::config::OrgOverlayMode::Off);
        // The primary is untouched — an append must never rebind it.
        assert_eq!(cfg.agent_token, "tok");
        assert_eq!(cfg.tenant_id, "tid");
        // With an overlay surface compiled in, the org gets its OWN key —
        // never a copy of the primary's.
        #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
        {
            assert!(entry.overlay_wg_secret_key.is_some());
            assert_ne!(
                entry.overlay_wg_secret_key, cfg.overlay_wg_secret_key,
                "org WG key must not equal the primary's"
            );
        }
    }

    #[test]
    fn apply_enrollment_append_uses_requested_label_and_rejects_taken() {
        let mut existing = crate::config::test_fixture();
        existing.orgs = vec![org("acme", "https://b.invalid", "t-acme")];
        let fresh = fresh_for("https://c.invalid", "t-c", "tok-c");
        let (cfg, outcome) = apply_enrollment(
            Some(existing.clone()),
            fresh.clone(),
            Some("Beta Corp"),
            false,
        )
        .unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::AppendedOrg {
                label: "beta-corp".into()
            }
        );
        assert_eq!(cfg.orgs.len(), 2);

        // Taken label → hard error (the operator named it deliberately).
        let err = apply_enrollment(Some(existing.clone()), fresh.clone(), Some("acme"), false)
            .unwrap_err();
        assert!(err.to_string().contains("already in use"), "{err}");
        // Reserved label → hard error.
        let err = apply_enrollment(Some(existing), fresh, Some("primary"), false).unwrap_err();
        assert!(err.to_string().contains("invalid --label"), "{err}");
    }

    #[test]
    fn apply_enrollment_replace_rebinds_primary_and_drops_dup_entry() {
        let mut existing = crate::config::test_fixture();
        existing.orgs = vec![
            org("acme", "https://b.invalid", "t-acme"),
            org("keep", "https://c.invalid", "t-keep"),
        ];
        // Replacing the primary with an identity that ALREADY exists as the
        // "acme" secondary: the secondary is dropped (no duplicate identity).
        let fresh = fresh_for("https://b.invalid", "t-acme", "tok-promoted");
        let (cfg, outcome) = apply_enrollment(Some(existing), fresh, None, true).unwrap();
        assert_eq!(outcome, EnrollOutcome::ReplacedPrimary);
        assert_eq!(cfg.server_url, "https://b.invalid");
        assert_eq!(cfg.tenant_id, "t-acme");
        assert_eq!(cfg.agent_token, "tok-promoted");
        assert_eq!(cfg.orgs.len(), 1, "the duplicate entry must be dropped");
        assert_eq!(cfg.orgs[0].label, "keep");
    }

    #[test]
    fn apply_enrollment_appended_label_uniquifies_on_collision() {
        let mut existing = crate::config::test_fixture();
        existing.orgs = vec![org("roomler-ai", "https://other.invalid", "t-x")];
        let fresh = fresh_for("https://roomler.ai", "t-y", "tok-y");
        let (_cfg, outcome) = apply_enrollment(Some(existing), fresh, None, false).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::AppendedOrg {
                label: "roomler-ai-2".into()
            }
        );
    }
}
