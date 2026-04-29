//! Agent on-disk configuration.
//!
//! Stored at `<user config dir>/roomler-agent/config.toml`. On Linux that
//! resolves to `$XDG_CONFIG_HOME/roomler-agent/` or `~/.config/roomler-agent/`.
//!
//! The file holds the enrolled agent's identity, its long-lived agent
//! token, and the server URL. It is the user's responsibility to keep
//! the file at mode 0600; on Linux/macOS we set that permission on write.

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

    /// How often (hours) the auto-updater polls GitHub Releases.
    /// `None` keeps the built-in default (24 h, see
    /// `updater::CHECK_INTERVAL`). Override at launch via the
    /// `ROOMLER_AGENT_UPDATE_INTERVAL_H` env var. Setting this to a
    /// large value (e.g. 168 = weekly) is the recommended way to
    /// dampen update load on bandwidth-constrained fleets.
    #[serde(default)]
    pub update_check_interval_h: Option<u32>,

    /// Most recent version that ran for at least
    /// `CLEAN_RUN_THRESHOLD` seconds before exiting cleanly (or
    /// crashing — the threshold is what gates updates here, not exit
    /// reason). Used by [`should_rollback`] to pick a fallback
    /// target when the current version crash-loops on cold start.
    /// `None` on a fresh install (no prior version to roll back to).
    #[serde(default)]
    pub last_known_good_version: Option<String>,

    /// Consecutive cold-start crashes within `CRASH_WINDOW_SECS` of
    /// each other. Bumped at startup by [`record_crash_at`]; reset
    /// to 0 by [`record_clean_run_at`] once a run survives long
    /// enough.
    #[serde(default)]
    pub crash_count: u32,

    /// Unix timestamp (seconds) of the most recent crash. Compared
    /// against the current time to decide whether the next crash
    /// "extends" the current crash window or starts a new one.
    #[serde(default)]
    pub last_crash_unix: u64,

    /// Set by the rollback path when it fires once. Cleared on next
    /// successful clean run (i.e. when the new-old version has
    /// proven itself stable). Prevents an oscillation loop between
    /// two equally-bad versions: we roll back at most once per
    /// install cycle.
    #[serde(default)]
    pub rollback_attempted: bool,

    /// `true` when the previous run started but never reached the
    /// clean-run threshold AND didn't exit gracefully via Ctrl-C.
    /// Read at startup to decide whether the previous run counts
    /// as a crash for [`record_crash_at`]. Set true by
    /// [`mark_run_starting`] at the top of every run; flipped back
    /// to false by [`record_clean_run_at`] (after the threshold)
    /// or by the graceful-shutdown path (Ctrl-C handler).
    ///
    /// Default `false` so a brand-new install isn't treated as a
    /// crash on its first run.
    #[serde(default)]
    pub last_run_unhealthy: bool,
}

/// How long a fresh run must survive before we promote its version
/// to `last_known_good_version` and reset the crash counter. Five
/// minutes is enough to rule out "agent crashed in startup init"
/// while still catching "agent ran fine then deadlocked at session
/// 0" reasonably fast.
pub const CLEAN_RUN_THRESHOLD_SECS: u64 = 5 * 60;

/// How recent a prior crash has to be for the next crash to count
/// against the same window. Ten minutes — chosen so an agent that
/// dies on cold start, gets relaunched in 60 s, and dies again
/// within those ten minutes is recognised as a crash loop and
/// triggers rollback after a few iterations.
pub const CRASH_WINDOW_SECS: u64 = 10 * 60;

/// How many crashes inside `CRASH_WINDOW_SECS` trip the rollback
/// path. Three is the sweet spot — fewer would fire on a single
/// hardware glitch (driver crash, transient OOM); more leaves a
/// genuinely-broken release running longer than necessary.
pub const ROLLBACK_THRESHOLD_CRASHES: u32 = 3;

impl AgentConfig {
    pub fn ws_url(&self) -> String {
        if let Some(url) = &self.ws_url {
            return url.clone();
        }
        derive_ws_url(&self.server_url)
    }
}

/// Mark the start of a fresh run. Sets `last_run_unhealthy=true`
/// optimistically — flipped back to false by either
/// [`record_clean_run_at`] (after the clean-run threshold) or by
/// [`mark_clean_shutdown`] (Ctrl-C handler). Caller saves config.
pub fn mark_run_starting(cfg: &mut AgentConfig) {
    cfg.last_run_unhealthy = true;
}

/// Record that the current run survived long enough to be
/// considered healthy. Resets the crash counter, promotes the
/// running version to `last_known_good_version`, clears the
/// rollback-attempted flag (so future genuine crash loops can
/// trigger another rollback), and clears the unhealthy flag.
pub fn record_clean_run_at(cfg: &mut AgentConfig, current_version: &str) {
    cfg.crash_count = 0;
    cfg.last_crash_unix = 0;
    cfg.rollback_attempted = false;
    cfg.last_run_unhealthy = false;
    cfg.last_known_good_version = Some(current_version.to_string());
}

/// Mark a graceful shutdown. Equivalent to "the run was healthy
/// from the rollback-detector's POV" — clears the unhealthy flag
/// without resetting the crash counter (a brief healthy run after
/// 2 prior crashes shouldn't wipe history that hasn't yet hit the
/// rollback threshold).
pub fn mark_clean_shutdown(cfg: &mut AgentConfig) {
    cfg.last_run_unhealthy = false;
}

/// Record a crash at the given unix timestamp. Increments the
/// counter when the prior crash was within `CRASH_WINDOW_SECS` of
/// `now_unix`; otherwise starts a fresh crash window at 1.
pub fn record_crash_at(cfg: &mut AgentConfig, now_unix: u64) {
    let prior = cfg.last_crash_unix;
    let in_window = prior > 0 && now_unix.saturating_sub(prior) <= CRASH_WINDOW_SECS;
    cfg.crash_count = if in_window {
        cfg.crash_count.saturating_add(1)
    } else {
        1
    };
    cfg.last_crash_unix = now_unix;
}

/// Whether the current state recommends rolling back to
/// `last_known_good_version`. Caller is responsible for actually
/// invoking the rollback (we keep the predicate pure for testing).
pub fn should_rollback(cfg: &AgentConfig, current_version: &str, now_unix: u64) -> bool {
    if cfg.rollback_attempted {
        return false;
    }
    let Some(target) = cfg.last_known_good_version.as_deref() else {
        return false;
    };
    if target == current_version {
        return false;
    }
    if cfg.crash_count < ROLLBACK_THRESHOLD_CRASHES {
        return false;
    }
    cfg.last_crash_unix > 0
        && now_unix.saturating_sub(cfg.last_crash_unix) <= CRASH_WINDOW_SECS
}

/// Mark that we just spawned a rollback installer. Sets
/// `rollback_attempted=true` so a same-cycle re-trigger is
/// suppressed.
pub fn mark_rollback_attempted(cfg: &mut AgentConfig) {
    cfg.rollback_attempted = true;
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

/// Resolve the default config path. Can be overridden by `--config` on the CLI.
pub fn default_config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("live", "roomler", "roomler-agent")
        .context("could not resolve a platform config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn load(path: &PathBuf) -> Result<AgentConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    let cfg: AgentConfig =
        toml::from_str(&raw).with_context(|| format!("parsing config at {}", path.display()))?;
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
        assert_eq!(
            derive_ws_url("https://roomler.live"),
            "wss://roomler.live/ws"
        );
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

    fn fixture() -> AgentConfig {
        AgentConfig {
            server_url: "https://example.invalid".into(),
            ws_url: None,
            agent_token: "tok".into(),
            agent_id: "aid".into(),
            tenant_id: "tid".into(),
            machine_id: "mid".into(),
            machine_name: "host".into(),
            encoder_preference: EncoderPreferenceChoice::Auto,
            update_check_interval_h: None,
            last_known_good_version: None,
            crash_count: 0,
            last_crash_unix: 0,
            rollback_attempted: false,
            last_run_unhealthy: false,
        }
    }

    #[test]
    fn record_clean_run_resets_counter_and_promotes_version() {
        let mut cfg = fixture();
        cfg.crash_count = 4;
        cfg.last_crash_unix = 1_000;
        cfg.rollback_attempted = true;
        record_clean_run_at(&mut cfg, "0.1.50");
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
        assert!(!cfg.rollback_attempted);
        assert_eq!(cfg.last_known_good_version.as_deref(), Some("0.1.50"));
    }

    #[test]
    fn record_crash_starts_window_at_one() {
        let mut cfg = fixture();
        record_crash_at(&mut cfg, 1_000_000);
        assert_eq!(cfg.crash_count, 1);
        assert_eq!(cfg.last_crash_unix, 1_000_000);
    }

    #[test]
    fn record_crash_increments_when_within_window() {
        let mut cfg = fixture();
        record_crash_at(&mut cfg, 1_000_000);
        record_crash_at(&mut cfg, 1_000_060); // +60s, in window
        record_crash_at(&mut cfg, 1_000_300); // +300s, still in window (10 min)
        assert_eq!(cfg.crash_count, 3);
        assert_eq!(cfg.last_crash_unix, 1_000_300);
    }

    #[test]
    fn record_crash_resets_when_outside_window() {
        let mut cfg = fixture();
        record_crash_at(&mut cfg, 1_000_000);
        record_crash_at(&mut cfg, 1_000_060);
        // +700s = 11 min 40s — outside the 10-min window.
        record_crash_at(&mut cfg, 1_000_760);
        assert_eq!(cfg.crash_count, 1, "counter resets on a fresh window");
        assert_eq!(cfg.last_crash_unix, 1_000_760);
    }

    #[test]
    fn should_rollback_false_when_no_known_good() {
        let mut cfg = fixture();
        cfg.crash_count = 5;
        cfg.last_crash_unix = 1_000_000;
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_001));
    }

    #[test]
    fn should_rollback_false_when_under_threshold() {
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 2; // threshold is 3
        cfg.last_crash_unix = 1_000_000;
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_001));
    }

    #[test]
    fn should_rollback_false_when_target_equals_current() {
        // Refusing this case prevents a same-version-rollback loop.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.51".into());
        cfg.crash_count = 5;
        cfg.last_crash_unix = 1_000_000;
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_001));
    }

    #[test]
    fn should_rollback_false_when_window_expired() {
        // A flaky day that adds 3 unrelated crashes over a week
        // shouldn't trigger rollback.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 3;
        cfg.last_crash_unix = 1_000_000;
        // +700s — outside CRASH_WINDOW_SECS.
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_700));
    }

    #[test]
    fn should_rollback_true_in_active_window_above_threshold() {
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 3;
        cfg.last_crash_unix = 1_000_000;
        assert!(should_rollback(&cfg, "0.1.51", 1_000_030));
    }

    #[test]
    fn should_rollback_false_when_already_attempted() {
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 5;
        cfg.last_crash_unix = 1_000_000;
        cfg.rollback_attempted = true;
        assert!(
            !should_rollback(&cfg, "0.1.51", 1_000_001),
            "must not oscillate between bad versions"
        );
    }

    #[test]
    fn mark_run_starting_sets_unhealthy_flag() {
        let mut cfg = fixture();
        assert!(!cfg.last_run_unhealthy);
        mark_run_starting(&mut cfg);
        assert!(cfg.last_run_unhealthy);
    }

    #[test]
    fn record_clean_run_clears_unhealthy_flag() {
        let mut cfg = fixture();
        mark_run_starting(&mut cfg);
        record_clean_run_at(&mut cfg, "0.1.50");
        assert!(!cfg.last_run_unhealthy);
        assert_eq!(cfg.last_known_good_version.as_deref(), Some("0.1.50"));
    }

    #[test]
    fn mark_clean_shutdown_clears_only_unhealthy() {
        // Clean shutdown after 2 prior crashes shouldn't wipe the
        // counter — those still represent a crash window that the
        // 3rd crash should escalate.
        let mut cfg = fixture();
        cfg.crash_count = 2;
        cfg.last_crash_unix = 1_000_000;
        mark_run_starting(&mut cfg);
        mark_clean_shutdown(&mut cfg);
        assert!(!cfg.last_run_unhealthy);
        assert_eq!(cfg.crash_count, 2, "clean shutdown preserves crash history");
        assert_eq!(cfg.last_crash_unix, 1_000_000);
    }

    #[test]
    fn old_config_without_new_fields_loads_with_defaults() {
        // Backwards-compat: a config.toml written by a pre-0.1.51
        // agent must continue to load.
        let raw = r#"
            server_url = "https://example.invalid"
            agent_token = "tok"
            agent_id = "aid"
            tenant_id = "tid"
            machine_id = "mid"
            machine_name = "host"
        "#;
        let cfg: AgentConfig = toml::from_str(raw).expect("legacy config must parse");
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
        assert!(!cfg.rollback_attempted);
        assert!(cfg.last_known_good_version.is_none());
    }
}
