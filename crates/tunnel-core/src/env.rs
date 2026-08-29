// RETIRED-NAME-ANCHOR-BEGIN
// This whole module IS the env-prefix fallback chain. Every occurrence of a
// retired name in it is a prefix the chain must keep honouring
// (`ROOMLER_AGENT_`, `ROOMLER_NODE_`), its documentation, or a test that sets one
// to prove the field keeps working.
// INVARIANT: if you add a retired name here that is NOT part of that chain, that
// is a bug, not a new exemption. docs/fr/FR-21
//! Roomler node env-var reads with legacy-prefix fallback.
//!
//! The controlled-host daemon is being renamed `roomler-agent` → `roomlerd`
//! (the unified "device / node" model — see the unification plan). Operators
//! set tuning vars in the Windows service Environment block (e.g.
//! `ROOMLER_AGENT_OVERLAY_DIRECT`), and those MUST keep working across the
//! rename: silently dropping a prefix is the MajorUpgrade-drops-env-vars class
//! of bug that already bit the fleet. So every node env read goes through
//! [`node_env`], which tries `ROOMLERD_<SUFFIX>` first, then the interim
//! `ROOMLER_NODE_<SUFFIX>`, then the original `ROOMLER_AGENT_<SUFFIX>`.
//!
//! FR-21 P3 (decision D1) makes `ROOMLERD_*` the spelling new code and docs
//! use — it matches the binary. Both older prefixes stay readable
//! INDEFINITELY: they are a contract with hosts already in the field, and the
//! cost of honouring them is one `or_else` arm. Adding a preferred prefix is a
//! change to this one function, never to the 166 call sites, because every
//! caller passes a bare SUFFIX.
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

/// Read a Roomler node env var by suffix. Precedence, highest first:
///
///   1. `ROOMLERD_<suffix>`      — the current spelling (FR-21 P3, decision D1)
///   2. `ROOMLER_NODE_<suffix>`  — the previous "new" spelling, still honoured
///   3. `ROOMLER_AGENT_<suffix>` — the original, still honoured
///   4. a registered config-backed fallback (S2)
///
/// Returns `None` if none is set (or the env value isn't valid Unicode).
///
/// Every knob in the daemon reads through here and passes a SUFFIX, never a
/// full name, so adding a preferred prefix is one arm in this chain rather than
/// an edit at 166 call sites.
///
/// RETIRED-NAME-ANCHOR(4): arms 2 and 3 are the reason a rename here costs
/// nothing in the field. Both spellings are set on real hosts today — mars,
/// jupiter and zeus each carry four `ROOMLER_AGENT_*` entries in an
/// operator-authored `/etc/systemd/system/roomlerd.service.d/` drop-in, which a
/// package upgrade never rewrites. Dropping either arm silently un-configures
/// those hosts: the daemon starts fine and simply ignores what it was told.
/// See docs/fr/FR-21.
pub fn node_env(suffix: &str) -> Option<String> {
    std::env::var(format!("ROOMLERD_{suffix}"))
        .or_else(|_| std::env::var(format!("ROOMLER_NODE_{suffix}")))
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
/// values (`std::env::var_os` semantics). Same precedence, and it MUST stay the
/// same: two readers of one knob that disagree about which prefix wins is a
/// bug nobody would think to look for.
///
/// RETIRED-NAME-ANCHOR(5): the legacy arms, as in [`node_env`]. See docs/fr/FR-21.
pub fn node_env_os(suffix: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("ROOMLERD_{suffix}"))
        .or_else(|| std::env::var_os(format!("ROOMLER_NODE_{suffix}")))
        .or_else(|| std::env::var_os(format!("ROOMLER_AGENT_{suffix}")))
        .or_else(|| config_fallback(suffix).map(std::ffi::OsString::from))
}

#[cfg(test)]
mod tests {
    // ── FR-21 P3 (D1): ROOMLERD_* wins, and NOTHING in the field stops working ──

    const S3: &str = "UNIFY_TEST_P3_PRECEDENCE";

    fn dk() -> String {
        format!("ROOMLERD_{S3}")
    }
    fn nk3() -> String {
        format!("ROOMLER_NODE_{S3}")
    }
    fn ak3() -> String {
        format!("ROOMLER_AGENT_{S3}")
    }

    #[test]
    fn roomlerd_prefix_wins_but_both_legacy_spellings_still_work() {
        // SAFETY (edition 2024): suffix is unique to this test, no concurrency.
        unsafe {
            std::env::remove_var(dk());
            std::env::remove_var(nk3());
            std::env::remove_var(ak3());
        }
        assert_eq!(node_env(S3), None, "none set -> None");

        // The ORIGINAL spelling alone must still be honoured. This is the case
        // that is live on mars/jupiter/zeus right now, in a drop-in no package
        // upgrade rewrites.
        unsafe { std::env::set_var(ak3(), "from-agent") };
        assert_eq!(node_env(S3).as_deref(), Some("from-agent"));

        // The interim spelling outranks it.
        unsafe { std::env::set_var(nk3(), "from-node") };
        assert_eq!(node_env(S3).as_deref(), Some("from-node"));

        // And the current spelling outranks both.
        unsafe { std::env::set_var(dk(), "from-roomlerd") };
        assert_eq!(node_env(S3).as_deref(), Some("from-roomlerd"));

        // Removing the winner falls back down the chain rather than to None.
        unsafe { std::env::remove_var(dk()) };
        assert_eq!(node_env(S3).as_deref(), Some("from-node"));
        unsafe { std::env::remove_var(nk3()) };
        assert_eq!(node_env(S3).as_deref(), Some("from-agent"));

        unsafe { std::env::remove_var(ak3()) };
        assert_eq!(node_env(S3), None);
    }

    #[test]
    fn the_os_twin_agrees_with_node_env_on_precedence() {
        // Two readers of one knob that disagree about which prefix wins is a
        // bug nobody would think to look for, so it is asserted rather than
        // assumed.
        const S4: &str = "UNIFY_TEST_P3_OS_TWIN";
        unsafe {
            std::env::set_var(format!("ROOMLER_AGENT_{S4}"), "agent");
            std::env::set_var(format!("ROOMLER_NODE_{S4}"), "node");
            std::env::set_var(format!("ROOMLERD_{S4}"), "roomlerd");
        }
        assert_eq!(node_env(S4).as_deref(), Some("roomlerd"));
        assert_eq!(
            node_env_os(S4).as_deref(),
            Some(std::ffi::OsStr::new("roomlerd"))
        );
        unsafe {
            std::env::remove_var(format!("ROOMLERD_{S4}"));
        }
        assert_eq!(node_env(S4).as_deref(), Some("node"));
        assert_eq!(
            node_env_os(S4).as_deref(),
            Some(std::ffi::OsStr::new("node"))
        );
        unsafe {
            std::env::remove_var(format!("ROOMLER_NODE_{S4}"));
            std::env::remove_var(format!("ROOMLER_AGENT_{S4}"));
        }
    }

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

// RETIRED-NAME-ANCHOR-END
