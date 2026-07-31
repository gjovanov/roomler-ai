//! Roomler node env-var reads with legacy-prefix fallback.
//!
//! The controlled-host daemon is being renamed `roomler-agent` → `roomlerd`
//! (the unified "device / node" model — see the unification plan). Operators
//! set tuning vars in the Windows service Environment block (e.g.
//! `ROOMLER_AGENT_OVERLAY_DIRECT`), and those MUST keep working across the
//! rename: silently dropping a prefix is the MajorUpgrade-drops-env-vars class
//! of bug that already bit the fleet. So every node env read goes through
//! [`node_env`], which prefers the new `ROOMLER_NODE_<SUFFIX>` and falls back
//! to the legacy `ROOMLER_AGENT_<SUFFIX>`. New code + docs use `ROOMLER_NODE_*`;
//! the legacy prefix stays readable indefinitely (cheap, and it's a contract
//! with hosts already in the field).
//!
//! S2 adds a THIRD source: config-backed fallbacks. The daemon registers the
//! operator-grade knobs from its `config.toml` once at startup
//! ([`register_config_fallbacks`]); precedence is env (either prefix) >
//! config > built-in default. Debug/media dials stay env-only.

use std::collections::HashMap;
use std::sync::OnceLock;

/// S2 — config-backed fallback values (suffix → env-equivalent string),
/// registered once by the daemon right after its config loads and BEFORE any
/// runtime that reads these knobs spawns. Consulted by [`node_env`] AFTER
/// both env prefixes, so an operator env var always overrides config, and
/// config overrides the built-in default. One daemon per process makes the
/// process-global honest; re-registration is a no-op (`OnceLock`). Values use
/// the same strings the env parsers accept (`"1"`/`"0"` for the bool knobs).
static CONFIG_FALLBACKS: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Register the config-backed fallback map. Call once, before spawning the
/// overlay/tunnel runtimes; later calls are ignored.
pub fn register_config_fallbacks(map: HashMap<String, String>) {
    let _ = CONFIG_FALLBACKS.set(map);
}

fn config_fallback(suffix: &str) -> Option<String> {
    CONFIG_FALLBACKS.get().and_then(|m| m.get(suffix).cloned())
}

/// Read a Roomler node env var by suffix, preferring `ROOMLER_NODE_<suffix>`,
/// falling back to the legacy `ROOMLER_AGENT_<suffix>`, then to a registered
/// config-backed fallback (S2). Returns `None` if none of the three is set
/// (or the env value isn't valid Unicode).
pub fn node_env(suffix: &str) -> Option<String> {
    std::env::var(format!("ROOMLER_NODE_{suffix}"))
        .or_else(|_| std::env::var(format!("ROOMLER_AGENT_{suffix}")))
        .ok()
        .or_else(|| config_fallback(suffix))
}

/// Parse a boolean gate the way every overlay kill-switch does. With
/// `default = true` (default-ON keys) anything except `0`/`false`/`no`/`off`
/// keeps the gate on; with `default = false` (opt-in keys) only
/// `1`/`true`/`yes`/`on` turns it on. Reads via [`node_env`] (both prefixes +
/// config fallback). Extracted in rc.279 — the fourth hand-rolled copy of
/// this parser was about to land; new gates use this, existing gates keep
/// their in-place parsers until touched.
pub fn flag(suffix: &str, default: bool) -> bool {
    match node_env(suffix) {
        Some(v) => {
            let t = v.trim();
            if default {
                !(t.eq_ignore_ascii_case("0")
                    || t.eq_ignore_ascii_case("false")
                    || t.eq_ignore_ascii_case("no")
                    || t.eq_ignore_ascii_case("off"))
            } else {
                t.eq_ignore_ascii_case("1")
                    || t.eq_ignore_ascii_case("true")
                    || t.eq_ignore_ascii_case("yes")
                    || t.eq_ignore_ascii_case("on")
            }
        }
        None => default,
    }
}

/// OsString twin of [`node_env`] for reads that must tolerate non-Unicode
/// values (`std::env::var_os` semantics). Prefers `ROOMLER_NODE_<suffix>`,
/// falls back to the legacy `ROOMLER_AGENT_<suffix>`, then to a registered
/// config-backed fallback. Returns `None` if none is set.
pub fn node_env_os(suffix: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("ROOMLER_NODE_{suffix}"))
        .or_else(|| std::env::var_os(format!("ROOMLER_AGENT_{suffix}")))
        .or_else(|| config_fallback(suffix).map(std::ffi::OsString::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A unique suffix no other code/test touches, so setting these process-wide
    // env vars can't race a parallel test. All mutations happen inside the one
    // test with no `.await` between them.
    const S: &str = "UNIFY_TEST_DUALREAD";

    fn nk() -> String {
        format!("ROOMLER_NODE_{S}")
    }
    fn ak() -> String {
        format!("ROOMLER_AGENT_{S}")
    }

    // Unique suffix for the S2 config-fallback test (no other code touches it).
    const S_CFG: &str = "UNIFY_TEST_CFGFALLBACK";

    #[test]
    fn config_fallback_loses_to_env_and_beats_unset() {
        // Register once for the whole process — includes a suffix no env var
        // will ever set, plus the one this test also sets via env.
        let mut m = HashMap::new();
        m.insert(S_CFG.to_string(), "from-config".to_string());
        register_config_fallbacks(m);

        // No env set → the registered fallback answers.
        assert_eq!(node_env(S_CFG).as_deref(), Some("from-config"));
        assert_eq!(
            node_env_os(S_CFG).as_deref(),
            Some(std::ffi::OsStr::new("from-config"))
        );

        // Env (legacy prefix) beats the registered fallback.
        // SAFETY (edition 2024): unique suffix, no concurrent access.
        unsafe { std::env::set_var(format!("ROOMLER_AGENT_{S_CFG}"), "from-env") };
        assert_eq!(node_env(S_CFG).as_deref(), Some("from-env"));
        unsafe { std::env::remove_var(format!("ROOMLER_AGENT_{S_CFG}")) };
        assert_eq!(node_env(S_CFG).as_deref(), Some("from-config"));

        // A suffix in neither env nor the map stays None.
        assert_eq!(node_env("UNIFY_TEST_CFGFALLBACK_UNSET"), None);
    }

    #[test]
    fn prefers_node_then_falls_back_to_agent_then_none() {
        // SAFETY (edition 2024): set/remove_var are `unsafe`; safe here because
        // the suffix is unique to this test and there is no concurrent access.
        unsafe {
            std::env::remove_var(nk());
            std::env::remove_var(ak());
        }
        assert_eq!(node_env(S), None, "unset → None");

        unsafe { std::env::set_var(ak(), "legacy") };
        assert_eq!(
            node_env(S).as_deref(),
            Some("legacy"),
            "legacy ROOMLER_AGENT_* is still honoured"
        );

        unsafe { std::env::set_var(nk(), "new") };
        assert_eq!(
            node_env(S).as_deref(),
            Some("new"),
            "ROOMLER_NODE_* wins when both are set"
        );

        unsafe { std::env::remove_var(nk()) };
        assert_eq!(
            node_env(S).as_deref(),
            Some("legacy"),
            "falls back to legacy after the new var is removed"
        );

        unsafe {
            std::env::remove_var(nk());
            std::env::remove_var(ak());
        }
    }

    #[test]
    fn flag_parses_default_on_and_opt_in() {
        // Unique suffixes; all mutations inside this one test (no races).
        const ON: &str = "UNIFY_TEST_FLAG_DEFAULT_ON";
        const OPT: &str = "UNIFY_TEST_FLAG_OPT_IN";

        // default-ON: unset → true; only an explicit falsy turns it off.
        assert!(flag(ON, true));
        // SAFETY (edition 2024): unique suffix, no concurrent access.
        unsafe { std::env::set_var(format!("ROOMLER_NODE_{ON}"), "off") };
        assert!(!flag(ON, true));
        unsafe { std::env::set_var(format!("ROOMLER_NODE_{ON}"), "weird") };
        assert!(
            flag(ON, true),
            "unrecognised value keeps a default-ON gate on"
        );
        unsafe { std::env::remove_var(format!("ROOMLER_NODE_{ON}")) };

        // opt-in: unset → false; only an explicit truthy turns it on (the
        // legacy ROOMLER_AGENT_ prefix must work too).
        assert!(!flag(OPT, false));
        unsafe { std::env::set_var(format!("ROOMLER_AGENT_{OPT}"), "YES") };
        assert!(flag(OPT, false));
        unsafe { std::env::set_var(format!("ROOMLER_AGENT_{OPT}"), "weird") };
        assert!(
            !flag(OPT, false),
            "unrecognised value keeps an opt-in gate off"
        );
        unsafe { std::env::remove_var(format!("ROOMLER_AGENT_{OPT}")) };
    }

    // A distinct unique suffix so this test can't race the String-variant one.
    const S_OS: &str = "UNIFY_TEST_DUALREAD_OS";

    fn nk_os() -> String {
        format!("ROOMLER_NODE_{S_OS}")
    }
    fn ak_os() -> String {
        format!("ROOMLER_AGENT_{S_OS}")
    }

    #[test]
    fn os_prefers_node_then_falls_back_to_agent_then_none() {
        // SAFETY (edition 2024): set/remove_var are `unsafe`; safe here because
        // the suffix is unique to this test and there is no concurrent access.
        unsafe {
            std::env::remove_var(nk_os());
            std::env::remove_var(ak_os());
        }
        assert_eq!(node_env_os(S_OS), None, "unset → None");

        unsafe { std::env::set_var(ak_os(), "legacy") };
        assert_eq!(
            node_env_os(S_OS).as_deref(),
            Some(std::ffi::OsStr::new("legacy")),
            "legacy ROOMLER_AGENT_* is still honoured"
        );

        unsafe { std::env::set_var(nk_os(), "new") };
        assert_eq!(
            node_env_os(S_OS).as_deref(),
            Some(std::ffi::OsStr::new("new")),
            "ROOMLER_NODE_* wins when both are set"
        );

        unsafe { std::env::remove_var(nk_os()) };
        assert_eq!(
            node_env_os(S_OS).as_deref(),
            Some(std::ffi::OsStr::new("legacy")),
            "falls back to legacy after the new var is removed"
        );

        unsafe {
            std::env::remove_var(nk_os());
            std::env::remove_var(ak_os());
        }
    }
}
