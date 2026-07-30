//! S2 config surface — the curated, secret-free slice of [`AgentConfig`]
//! that the LocalAPI `ConfigGet` / `ConfigSet` verbs expose to the
//! desktop app / CLI.
//!
//! One registry drives both verbs: every key carries an editor `kind`
//! (see [`tunnel_core::localapi::ConfigEntry`]), a one-line description,
//! and a per-key parse/validate in [`apply`]. Values travel as strings —
//! bools as `"true"`/`"false"`, lists comma-separated, structured keys
//! (`forward_acl`, `virtual_desktop_apps`) as JSON documents.
//!
//! Deliberately NOT here: secrets (`agent_token`,
//! `overlay_wg_secret_key`), identity (`machine_id`; `machine_name` has
//! its own `SetDeviceName` verb), `[[tunnel_routes]]` (the `Route*`
//! verbs + Routes pane own those), and the crash-bookkeeping fields.
//! Every key is read at daemon startup, so the whole surface is
//! `restart_required = true`.

use crate::config::{AgentConfig, EncoderPreferenceChoice};
use tunnel_core::localapi::ConfigEntry;

/// `(key, kind, description)` for the whole surface, in display order.
/// `kind` is the client-side editor hint contract — see [`ConfigEntry`].
const KEYS: &[(&str, &str, &str)] = &[
    (
        "overlay_enabled",
        "bool",
        "Join the L3 overlay mesh (WireGuard-style private network). Default: off.",
    ),
    (
        "overlay_advertised_routes",
        "list",
        "CIDRs this node offers to route for overlay peers (subnet router); admin approval required. Comma-separated.",
    ),
    (
        "overlay_exit_node_enabled",
        "bool",
        "Offer this node as an overlay exit node (advertises 0.0.0.0/0; admin approval required). Default: off.",
    ),
    (
        "overlay_exit_node",
        "string",
        "Route ALL of this node's internet egress through the named mesh peer (name or node-id hex). Empty = normal routing.",
    ),
    (
        "advertise_routes",
        "list",
        "CIDRs this host advertises for the tunnel/SOCKS mesh; admin approval required. Comma-separated.",
    ),
    (
        "advertise_local_subnets",
        "bool",
        "Auto-detect and advertise directly-connected IPv4 subnets (untrusted until admin-approved). Default: on.",
    ),
    (
        "auto_grant_session",
        "bool",
        "Auto-approve incoming remote-control session requests without an operator prompt. Default: on.",
    ),
    (
        "enable_remote_browse",
        "bool",
        "Answer remote filesystem-browse requests from the controller. Default: on.",
    ),
    (
        "encoder_preference",
        "enum:auto|hardware|software",
        "Video encoder selection: auto (HW probe then fallback), hardware, or software.",
    ),
    (
        "update_check_interval_h",
        "string",
        "Hours between self-update checks (1-8760). Empty = built-in default (24 h).",
    ),
    (
        "overlay_quic",
        "tribool",
        "QUIC-over-TURN overlay carrier. Built-in default: off.",
    ),
    (
        "overlay_direct",
        "tribool",
        "Direct (LAN / hole-punched) overlay carriers. Built-in default: on.",
    ),
    (
        "overlay_derp",
        "tribool",
        "DERP (WebSocket-relay) overlay fallback tier. Built-in default: on.",
    ),
    (
        "overlay_mbb",
        "tribool",
        "Make-before-break overlay carrier upgrades. Built-in default: on.",
    ),
    (
        "overlay_lan_iface_filter",
        "tribool",
        "LAN-gather virtual-interface filter (skip WSL/Hyper-V/other-VPN adapters). Built-in default: on.",
    ),
    (
        "overlay_pathmon",
        "string",
        "Overlay PathMonitor mode: on (authoritative — built-in default) | shadow (compare-only revert rail) | off. Env: ROOMLER_NODE_OVERLAY_PATHMON.",
    ),
    (
        "overlay_route_events",
        "tribool",
        "Event-driven route guard (OS route-table change subscription; the 2 s tick stays as backstop). Built-in default: on.",
    ),
    (
        "local_turn",
        "tribool",
        "Loopback-TURN relay for controllers on the same corporate network. Built-in default: on.",
    ),
    (
        "dns_aaaa",
        "tribool",
        "MagicDNS AAAA (IPv6) answers for overlay names. Built-in default: on.",
    ),
    (
        "auto_update",
        "tribool",
        "Periodic self-update checks (also gates web-pushed updates). Built-in default: on.",
    ),
    (
        "logs_upload_disabled",
        "tribool",
        "Disable centralized diagnostic-log upload. Built-in default: uploads on.",
    ),
    (
        "rate_factor_h264",
        "string",
        "H.264 maxrate ceiling factor, % (50-400). Env: ROOMLER_NODE_RATE_FACTOR_H264. Empty = built-in 150. Restart required.",
    ),
    (
        "rate_factor_hevc",
        "string",
        "HEVC maxrate ceiling factor, % (50-400). Env: ROOMLER_NODE_RATE_FACTOR_HEVC. Empty = built-in 125. Restart required.",
    ),
    (
        "rate_factor_vp9",
        "string",
        "VP9 maxrate ceiling factor, % (50-400). Env: ROOMLER_NODE_RATE_FACTOR_VP9. Empty = built-in 125. Restart required.",
    ),
    (
        "rate_factor_av1",
        "string",
        "AV1 maxrate ceiling factor, % (50-400). Env: ROOMLER_NODE_RATE_FACTOR_AV1. Empty = built-in 100. Restart required.",
    ),
    (
        "ice_follow_renomination",
        "enum:auto|always|never",
        "Media-ICE nomination-follow policy. auto (empty) = upward-only + stale-failover (recommended); always = legacy follow-everything (thrash-prone, diagnostics only); never = pin to first nomination. Env: ROOMLER_ICE_FOLLOW_RENOMINATION.",
    ),
    (
        "ice_warm_standby",
        "tribool",
        "Keepalive pings on validated-but-unselected media ICE pairs (keeps the real-path fallback's NAT mapping alive). Built-in default: on. Env: ROOMLER_ICE_WARM_STANDBY.",
    ),
    (
        "ice_overlay_host_deprioritize",
        "tribool",
        "Rank overlay-TUN host candidates below srflx in media ICE (media prefers the real path). Built-in default: on. Env: ROOMLER_ICE_OVERLAY_HOST_DEPRIORITIZE.",
    ),
    (
        "overlay_tier_detect",
        "tribool",
        "Clamp media bitrate when the overlay carrier under a nominated pair is relay-tier. Built-in default: on. Env: ROOMLER_NODE_OVERLAY_TIER_DETECT.",
    ),
    (
        "forward_acl",
        "json",
        "Agent-side allowlist for tunnel forwards (JSON: {\"enabled\": bool, \"allowlist\": [...]}).",
    ),
    (
        "virtual_desktop_apps",
        "json",
        "Remote app launcher config for virtual-desktop hosts (JSON; browser only ever sends an allowlist key).",
    ),
];

/// The full editable surface with current values from `cfg`.
pub fn entries(cfg: &AgentConfig) -> Vec<ConfigEntry> {
    KEYS.iter()
        .map(|(key, kind, description)| ConfigEntry {
            key: (*key).to_string(),
            value: current_value(cfg, key),
            kind: (*kind).to_string(),
            restart_required: true,
            description: (*description).to_string(),
        })
        .collect()
}

/// One entry by key (post-apply echo). `None` = unknown key.
pub fn entry_for(cfg: &AgentConfig, key: &str) -> Option<ConfigEntry> {
    KEYS.iter()
        .find(|(k, _, _)| *k == key)
        .map(|(k, kind, description)| ConfigEntry {
            key: (*k).to_string(),
            value: current_value(cfg, k),
            kind: (*kind).to_string(),
            restart_required: true,
            description: (*description).to_string(),
        })
}

fn current_value(cfg: &AgentConfig, key: &str) -> Option<String> {
    match key {
        "overlay_enabled" => Some(fmt_bool(cfg.overlay_enabled)),
        "overlay_advertised_routes" => Some(cfg.overlay_advertised_routes.join(",")),
        "overlay_exit_node_enabled" => Some(fmt_bool(cfg.overlay_exit_node_enabled)),
        "overlay_exit_node" => cfg.overlay_exit_node.clone(),
        "advertise_routes" => Some(cfg.advertise_routes.join(",")),
        "advertise_local_subnets" => Some(fmt_bool(cfg.advertise_local_subnets)),
        "auto_grant_session" => Some(fmt_bool(cfg.auto_grant_session)),
        "enable_remote_browse" => Some(fmt_bool(cfg.enable_remote_browse)),
        "encoder_preference" => Some(
            match cfg.encoder_preference {
                EncoderPreferenceChoice::Auto => "auto",
                EncoderPreferenceChoice::Hardware => "hardware",
                EncoderPreferenceChoice::Software => "software",
            }
            .to_string(),
        ),
        "update_check_interval_h" => cfg.update_check_interval_h.map(|h| h.to_string()),
        "overlay_quic" => cfg.overlay_quic.map(fmt_bool),
        "overlay_direct" => cfg.overlay_direct.map(fmt_bool),
        "overlay_derp" => cfg.overlay_derp.map(fmt_bool),
        "overlay_mbb" => cfg.overlay_mbb.map(fmt_bool),
        "overlay_lan_iface_filter" => cfg.overlay_lan_iface_filter.map(fmt_bool),
        "overlay_pathmon" => cfg.overlay_pathmon.clone(),
        "overlay_route_events" => cfg.overlay_route_events.map(fmt_bool),
        "local_turn" => cfg.local_turn.map(fmt_bool),
        "dns_aaaa" => cfg.dns_aaaa.map(fmt_bool),
        "auto_update" => cfg.auto_update.map(fmt_bool),
        "logs_upload_disabled" => cfg.logs_upload_disabled.map(fmt_bool),
        "rate_factor_h264" => cfg.rate_factor_h264.map(|p| p.to_string()),
        "rate_factor_hevc" => cfg.rate_factor_hevc.map(|p| p.to_string()),
        "rate_factor_vp9" => cfg.rate_factor_vp9.map(|p| p.to_string()),
        "rate_factor_av1" => cfg.rate_factor_av1.map(|p| p.to_string()),
        "ice_follow_renomination" => cfg
            .ice_follow_renomination
            .map(|b| if b { "always" } else { "never" }.to_string()),
        "ice_warm_standby" => cfg.ice_warm_standby.map(fmt_bool),
        "ice_overlay_host_deprioritize" => cfg.ice_overlay_host_deprioritize.map(fmt_bool),
        "overlay_tier_detect" => cfg.overlay_tier_detect.map(fmt_bool),
        "forward_acl" => serde_json::to_string(&cfg.forward_acl).ok(),
        "virtual_desktop_apps" => serde_json::to_string(&cfg.virtual_desktop_apps).ok(),
        _ => None,
    }
}

/// Parse + validate + write one key into `cfg`. `value: None` clears the
/// key back to its built-in default. Returns a human-readable error (no
/// partial writes — `cfg` is only mutated on the success path of each
/// arm).
pub fn apply(cfg: &mut AgentConfig, key: &str, value: Option<&str>) -> Result<(), String> {
    match key {
        "overlay_enabled" => cfg.overlay_enabled = parse_bool_or(value, false)?,
        "overlay_advertised_routes" => cfg.overlay_advertised_routes = parse_cidr_list(value)?,
        "overlay_exit_node_enabled" => cfg.overlay_exit_node_enabled = parse_bool_or(value, false)?,
        "overlay_exit_node" => {
            cfg.overlay_exit_node = value
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }
        "advertise_routes" => cfg.advertise_routes = parse_cidr_list(value)?,
        "advertise_local_subnets" => cfg.advertise_local_subnets = parse_bool_or(value, true)?,
        "auto_grant_session" => cfg.auto_grant_session = parse_bool_or(value, true)?,
        "enable_remote_browse" => cfg.enable_remote_browse = parse_bool_or(value, true)?,
        "encoder_preference" => {
            cfg.encoder_preference = match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
                None | Some("") | Some("auto") => EncoderPreferenceChoice::Auto,
                Some("hardware") | Some("hw") | Some("mf") => EncoderPreferenceChoice::Hardware,
                Some("software") | Some("sw") | Some("openh264") => {
                    EncoderPreferenceChoice::Software
                }
                Some(other) => {
                    return Err(format!(
                        "encoder_preference must be auto|hardware|software (got {other:?})"
                    ));
                }
            }
        }
        "update_check_interval_h" => {
            cfg.update_check_interval_h = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let h: u32 = v.parse().map_err(|_| {
                        format!("update_check_interval_h must be a number (got {v:?})")
                    })?;
                    if !(1..=8760).contains(&h) {
                        return Err("update_check_interval_h must be between 1 and 8760".into());
                    }
                    Some(h)
                }
            }
        }
        "overlay_quic" => cfg.overlay_quic = parse_tribool(value)?,
        "overlay_direct" => cfg.overlay_direct = parse_tribool(value)?,
        "overlay_derp" => cfg.overlay_derp = parse_tribool(value)?,
        "overlay_mbb" => cfg.overlay_mbb = parse_tribool(value)?,
        "overlay_lan_iface_filter" => cfg.overlay_lan_iface_filter = parse_tribool(value)?,
        "overlay_pathmon" => {
            cfg.overlay_pathmon = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let mode = v.to_ascii_lowercase();
                    if !matches!(mode.as_str(), "on" | "shadow" | "off") {
                        return Err("overlay_pathmon must be on | shadow | off".to_string());
                    }
                    Some(mode)
                }
            }
        }
        "overlay_route_events" => cfg.overlay_route_events = parse_tribool(value)?,
        "local_turn" => cfg.local_turn = parse_tribool(value)?,
        "dns_aaaa" => cfg.dns_aaaa = parse_tribool(value)?,
        "auto_update" => cfg.auto_update = parse_tribool(value)?,
        "logs_upload_disabled" => cfg.logs_upload_disabled = parse_tribool(value)?,
        "rate_factor_h264" => cfg.rate_factor_h264 = parse_rate_factor(key, value)?,
        "rate_factor_hevc" => cfg.rate_factor_hevc = parse_rate_factor(key, value)?,
        "rate_factor_vp9" => cfg.rate_factor_vp9 = parse_rate_factor(key, value)?,
        "rate_factor_av1" => cfg.rate_factor_av1 = parse_rate_factor(key, value)?,
        "ice_follow_renomination" => {
            cfg.ice_follow_renomination =
                match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
                    None | Some("") | Some("auto") => None,
                    Some("always") => Some(true),
                    Some("never") => Some(false),
                    Some(other) => {
                        return Err(format!(
                            "ice_follow_renomination must be auto|always|never (got {other:?})"
                        ));
                    }
                }
        }
        "ice_warm_standby" => cfg.ice_warm_standby = parse_tribool(value)?,
        "ice_overlay_host_deprioritize" => {
            cfg.ice_overlay_host_deprioritize = parse_tribool(value)?
        }
        "overlay_tier_detect" => cfg.overlay_tier_detect = parse_tribool(value)?,
        "forward_acl" => {
            cfg.forward_acl = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => Default::default(),
                Some(v) => serde_json::from_str(v)
                    .map_err(|e| format!("forward_acl: invalid JSON: {e}"))?,
            }
        }
        "virtual_desktop_apps" => {
            cfg.virtual_desktop_apps = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => Default::default(),
                Some(v) => serde_json::from_str(v)
                    .map_err(|e| format!("virtual_desktop_apps: invalid JSON: {e}"))?,
            }
        }
        other => return Err(format!("unknown or non-editable config key {other:?}")),
    }
    Ok(())
}

/// Shared parse/validate for the four `rate_factor_*` keys: empty clears
/// (built-in applies), numeric must be 50–400 %.
fn parse_rate_factor(key: &str, value: Option<&str>) -> Result<Option<u32>, String> {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => {
            let pct: u32 = v
                .parse()
                .map_err(|_| format!("{key} must be a number (got {v:?})"))?;
            if !(50..=400).contains(&pct) {
                return Err(format!("{key} must be between 50 and 400 (percent)"));
            }
            Ok(Some(pct))
        }
    }
}

fn fmt_bool(b: bool) -> String {
    if b { "true" } else { "false" }.to_string()
}

/// `"true"/"1"/"yes"/"on"` → true; `"false"/"0"/"no"/"off"` → false
/// (case-insensitive). Anything else is an error.
fn parse_bool(v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!("expected true/false (got {other:?})")),
    }
}

/// Plain-bool keys: `None`/empty clears back to the serde default.
fn parse_bool_or(v: Option<&str>, default: bool) -> Result<bool, String> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(default),
        Some(v) => parse_bool(v),
    }
}

/// Tri-state keys: `None`/empty = unset (built-in default applies).
fn parse_tribool(v: Option<&str>) -> Result<Option<bool>, String> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => parse_bool(v).map(Some),
    }
}

/// S2 wizard — validate a comma-separated CIDR list WITHOUT a config
/// (the setup wizard checks route inputs before the MSI runs and the
/// single-use enrollment token is burned). Same parser [`apply`] uses,
/// so a value that validates here can't fail there.
pub fn validate_cidr_list(v: &str) -> Result<(), String> {
    parse_cidr_list(Some(v)).map(|_| ())
}

/// Comma-separated CIDR list; each token must be `addr/prefix` with a
/// prefix that fits the address family. `None`/empty clears the list.
fn parse_cidr_list(v: Option<&str>) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for token in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (addr, prefix) = token
            .split_once('/')
            .ok_or_else(|| format!("{token:?} is not CIDR notation (addr/prefix)"))?;
        let ip: std::net::IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("{token:?}: bad IP address"))?;
        let bits: u8 = prefix
            .trim()
            .parse()
            .map_err(|_| format!("{token:?}: bad prefix length"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if bits > max {
            return Err(format!("{token:?}: prefix /{bits} exceeds /{max}"));
        }
        out.push(token.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_gets_and_applies() {
        let mut cfg = crate::config::test_fixture();
        let all = entries(&cfg);
        assert_eq!(all.len(), KEYS.len());
        // Every listed key roundtrips through entry_for + a no-op apply
        // of its own current value (or a clear for unset optionals).
        for e in &all {
            assert!(e.restart_required);
            assert!(!e.description.is_empty());
            apply(&mut cfg, &e.key, e.value.as_deref()).expect("self-value apply");
            let echoed = entry_for(&cfg, &e.key).expect("known key");
            assert_eq!(echoed.value, e.value, "roundtrip for {}", e.key);
        }
    }

    #[test]
    fn tribool_set_and_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_quic", Some("on")).unwrap();
        assert_eq!(cfg.overlay_quic, Some(true));
        assert_eq!(
            entry_for(&cfg, "overlay_quic").unwrap().value.as_deref(),
            Some("true")
        );
        apply(&mut cfg, "overlay_quic", Some("0")).unwrap();
        assert_eq!(cfg.overlay_quic, Some(false));
        apply(&mut cfg, "overlay_quic", None).unwrap();
        assert_eq!(cfg.overlay_quic, None);
        assert!(apply(&mut cfg, "overlay_quic", Some("maybe")).is_err());
    }

    /// rc.275 — the LAN-gather virtual-interface filter key set/echo/clear
    /// (per the every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_lan_iface_filter_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_lan_iface_filter", Some("off")).unwrap();
        assert_eq!(cfg.overlay_lan_iface_filter, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_lan_iface_filter")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_lan_iface_filter", Some("1")).unwrap();
        assert_eq!(cfg.overlay_lan_iface_filter, Some(true));
        apply(&mut cfg, "overlay_lan_iface_filter", None).unwrap();
        assert_eq!(cfg.overlay_lan_iface_filter, None);
        assert!(apply(&mut cfg, "overlay_lan_iface_filter", Some("maybe")).is_err());
    }

    /// PR-D — the PathMonitor mode key: multi-state (on|shadow|off), so a
    /// validated string, not a tribool (per the every-new-env rule's
    /// enum-not-tribool clause). Case-normalized; empty/None clears.
    #[test]
    fn overlay_pathmon_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_pathmon", Some("shadow")).unwrap();
        assert_eq!(cfg.overlay_pathmon.as_deref(), Some("shadow"));
        assert_eq!(
            entry_for(&cfg, "overlay_pathmon").unwrap().value.as_deref(),
            Some("shadow")
        );
        apply(&mut cfg, "overlay_pathmon", Some("ON")).unwrap();
        assert_eq!(cfg.overlay_pathmon.as_deref(), Some("on"));
        apply(&mut cfg, "overlay_pathmon", None).unwrap();
        assert_eq!(cfg.overlay_pathmon, None);
        assert!(apply(&mut cfg, "overlay_pathmon", Some("sideways")).is_err());
    }

    /// P4/PR-D — the event-driven route-guard key set/echo/clear.
    #[test]
    fn overlay_route_events_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_route_events", Some("off")).unwrap();
        assert_eq!(cfg.overlay_route_events, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_route_events")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_route_events", None).unwrap();
        assert_eq!(cfg.overlay_route_events, None);
        assert!(apply(&mut cfg, "overlay_route_events", Some("maybe")).is_err());
    }

    #[test]
    fn rate_factor_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        for key in [
            "rate_factor_h264",
            "rate_factor_hevc",
            "rate_factor_vp9",
            "rate_factor_av1",
        ] {
            apply(&mut cfg, key, Some("120")).unwrap();
            assert_eq!(
                entry_for(&cfg, key).unwrap().value.as_deref(),
                Some("120"),
                "set/echo for {key}"
            );
            assert!(apply(&mut cfg, key, Some("49")).is_err(), "{key} below 50");
            assert!(
                apply(&mut cfg, key, Some("401")).is_err(),
                "{key} above 400"
            );
            assert!(apply(&mut cfg, key, Some("fast")).is_err(), "{key} garbage");
            // Failed applies never partially wrote:
            assert_eq!(entry_for(&cfg, key).unwrap().value.as_deref(), Some("120"));
            apply(&mut cfg, key, None).unwrap();
            assert_eq!(entry_for(&cfg, key).unwrap().value, None, "clear {key}");
        }
    }

    #[test]
    fn ice_follow_renomination_enum_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "ice_follow_renomination", Some("always")).unwrap();
        assert_eq!(cfg.ice_follow_renomination, Some(true));
        assert_eq!(
            entry_for(&cfg, "ice_follow_renomination")
                .unwrap()
                .value
                .as_deref(),
            Some("always")
        );
        apply(&mut cfg, "ice_follow_renomination", Some("never")).unwrap();
        assert_eq!(cfg.ice_follow_renomination, Some(false));
        apply(&mut cfg, "ice_follow_renomination", Some("auto")).unwrap();
        assert_eq!(cfg.ice_follow_renomination, None);
        assert_eq!(
            entry_for(&cfg, "ice_follow_renomination").unwrap().value,
            None
        );
        assert!(apply(&mut cfg, "ice_follow_renomination", Some("on")).is_err());
    }

    #[test]
    fn ice_and_overlay_hatch_tribools_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        for key in [
            "ice_warm_standby",
            "ice_overlay_host_deprioritize",
            "overlay_tier_detect",
        ] {
            apply(&mut cfg, key, Some("off")).unwrap();
            assert_eq!(
                entry_for(&cfg, key).unwrap().value.as_deref(),
                Some("false"),
                "set/echo for {key}"
            );
            apply(&mut cfg, key, None).unwrap();
            assert_eq!(entry_for(&cfg, key).unwrap().value, None, "clear {key}");
        }
    }

    #[test]
    fn bool_clear_restores_serde_default() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "auto_grant_session", Some("false")).unwrap();
        assert!(!cfg.auto_grant_session);
        apply(&mut cfg, "auto_grant_session", None).unwrap();
        assert!(cfg.auto_grant_session, "default_auto_grant_session is true");
        apply(&mut cfg, "overlay_enabled", None).unwrap();
        assert!(!cfg.overlay_enabled, "overlay default is off");
    }

    #[test]
    fn cidr_list_validates() {
        let mut cfg = crate::config::test_fixture();
        apply(
            &mut cfg,
            "advertise_routes",
            Some("192.168.1.0/24, 10.0.0.0/8"),
        )
        .unwrap();
        assert_eq!(cfg.advertise_routes, vec!["192.168.1.0/24", "10.0.0.0/8"]);
        assert!(apply(&mut cfg, "advertise_routes", Some("not-a-cidr")).is_err());
        assert!(apply(&mut cfg, "advertise_routes", Some("10.0.0.0/40")).is_err());
        // Bad input never partially wrote:
        assert_eq!(cfg.advertise_routes, vec!["192.168.1.0/24", "10.0.0.0/8"]);
        apply(&mut cfg, "advertise_routes", Some("")).unwrap();
        assert!(cfg.advertise_routes.is_empty());
    }

    #[test]
    fn json_keys_validate() {
        let mut cfg = crate::config::test_fixture();
        let acl = r#"{"enabled": false, "allowlist": []}"#;
        apply(&mut cfg, "forward_acl", Some(acl)).unwrap();
        assert!(!cfg.forward_acl.enabled);
        assert!(apply(&mut cfg, "forward_acl", Some("{nope")).is_err());
        apply(&mut cfg, "forward_acl", None).unwrap();
        assert!(cfg.forward_acl.enabled, "default ACL is enabled");
    }

    #[test]
    fn secrets_and_identity_are_not_exposed() {
        let cfg = crate::config::test_fixture();
        for hidden in [
            "agent_token",
            "overlay_wg_secret_key",
            "machine_id",
            "machine_name",
            "tunnel_routes",
        ] {
            assert!(entry_for(&cfg, hidden).is_none(), "{hidden} must be hidden");
            let mut c = cfg.clone();
            assert!(apply(&mut c, hidden, Some("x")).is_err());
        }
    }

    #[test]
    fn enum_and_interval_parse() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "encoder_preference", Some("hw")).unwrap();
        assert!(matches!(
            cfg.encoder_preference,
            EncoderPreferenceChoice::Hardware
        ));
        assert!(apply(&mut cfg, "encoder_preference", Some("fast")).is_err());
        apply(&mut cfg, "update_check_interval_h", Some("168")).unwrap();
        assert_eq!(cfg.update_check_interval_h, Some(168));
        assert!(apply(&mut cfg, "update_check_interval_h", Some("0")).is_err());
        assert!(apply(&mut cfg, "update_check_interval_h", Some("nope")).is_err());
        apply(&mut cfg, "update_check_interval_h", None).unwrap();
        assert_eq!(cfg.update_check_interval_h, None);
    }
}
