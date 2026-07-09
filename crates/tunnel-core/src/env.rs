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

/// Read a Roomler node env var by suffix, preferring `ROOMLER_NODE_<suffix>`
/// and falling back to the legacy `ROOMLER_AGENT_<suffix>`. Returns `None` if
/// neither is set (or the value isn't valid Unicode).
pub fn node_env(suffix: &str) -> Option<String> {
    std::env::var(format!("ROOMLER_NODE_{suffix}"))
        .or_else(|_| std::env::var(format!("ROOMLER_AGENT_{suffix}")))
        .ok()
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
}
