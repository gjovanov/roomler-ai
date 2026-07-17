//! Agent on-disk configuration.
//!
//! Stored at `<user config dir>/roomler-agent/config.toml`. On Linux that
//! resolves to `$XDG_CONFIG_HOME/roomler-agent/` or `~/.config/roomler-agent/`.
//!
//! The file holds the enrolled agent's identity, its long-lived agent
//! token, and the server URL. It is the user's responsibility to keep
//! the file at mode 0600; on Linux/macOS we set that permission on write.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::tunnel::acl::AgentForwardAcl;

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

    /// Whether the agent answers `files:dir` (filesystem browse)
    /// requests from the browser controller. Default `true` to
    /// preserve self-controlled-host auto-grant semantics
    /// (`docs/remote-control.md` §11.2). Operators on org-controlled
    /// fleets can disable per-host via `config.toml`. When `false`,
    /// `files:dir` returns `dir-error { message: "remote browse
    /// disabled" }`. Single-file downloads (`files:get`) and uploads
    /// are NOT gated by this flag — they're consent-bound by the
    /// session itself.
    #[serde(default = "default_enable_remote_browse")]
    pub enable_remote_browse: bool,

    /// Whether incoming `rc:session.request` messages are
    /// auto-granted without operator interaction. Default `true` to
    /// match historical self-host behaviour (`docs/remote-control.md`
    /// §11.2 + signaling.rs's pre-Plan-3 auto-grant). Org-controlled
    /// fleets set this to `false` so every session start waits for
    /// an explicit operator decision via the `roomler-agent consent
    /// --session <hex> --approve|--deny` CLI fallback (or, in a
    /// future version, a tray prompt). 30 s timeout → auto-deny.
    /// Has NO effect on the file-DC path — uploads/downloads/dir
    /// browsing remain gated by `enable_remote_browse` + the
    /// agent's denylist.
    #[serde(default = "default_auto_grant_session")]
    pub auto_grant_session: bool,

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

    /// Last config-schema version this file was migrated to. Used by
    /// [`migrate`] to decide which migration steps to apply at startup.
    /// `None` (or missing in TOML) on pre-rc.18 configs — those run
    /// through the rc.18 migration set and the field is then persisted.
    /// Forward-compat: future RCs key migrations off this string
    /// (e.g. `match version { Some("0.3.0-rc.18") => apply_rc18_to_rc19, … }`).
    #[serde(default)]
    pub config_schema_version: Option<String>,

    /// roomler-tunnel agent-side allowlist (T2.6). Default is
    /// `enabled` with an empty allowlist — meaning "trust the
    /// server's tenant policy on every `ServerMsg::TcpForwardForward`".
    /// Operators on org-controlled hosts narrow further by populating
    /// `forward_acl.allowlist` in the TOML or disable forwards
    /// entirely with `forward_acl.enabled = false`. See
    /// `agents/roomler-agent/src/tunnel/acl.rs` for the matching
    /// semantics.
    #[serde(default)]
    pub forward_acl: AgentForwardAcl,

    /// Remote app selection & launch on virtual-desktop hosts. Default:
    /// enabled with a seeded bash/tmux entry so a headless VD host offers
    /// "New bash session" out of the box. Operators add htop/mc/… per host
    /// in the TOML. The browser only ever sends an allowlist KEY, never a
    /// command line. See `agents/roomler-agent/src/apps/`.
    #[serde(default)]
    pub virtual_desktop_apps: crate::apps::VirtualDesktopAppsConfig,

    /// Phase 3b: opt into the overlay L3 mesh. Default off — an
    /// `overlay-l3` build only joins the mesh when this is set.
    #[serde(default)]
    pub overlay_enabled: bool,

    /// Phase 3b: this node's persisted WireGuard Curve25519 secret key
    /// (base64). Generated on the first overlay-enabled startup in `main`;
    /// the public key is what the netmap distributes. `None` until then.
    #[serde(default)]
    pub overlay_wg_secret_key: Option<String>,

    /// Phase 1 subnet router — CIDRs this node offers to route for overlay peers
    /// (e.g. `["192.168.1.0/24"]`). Sent on join as `advertised_routes`; each is
    /// gated behind admin approval server-side before any peer uses it. Empty =
    /// this node is not a subnet router.
    #[serde(default)]
    pub overlay_advertised_routes: Vec<String>,

    /// Tunnel mesh subnet-router — CIDRs this host advertises it can route for
    /// the SOCKS mesh (e.g. `["192.168.1.0/24"]`). Sent on `rc:agent.hello` as
    /// `advertised_routes`; an admin approves a subset (Admin → Agents → Subnet
    /// routes) before the mesh uses them. Separate from
    /// `overlay_advertised_routes` (the L3 overlay's own subnet router).
    #[serde(default)]
    pub advertise_routes: Vec<String>,

    /// Auto-detect this host's directly-connected IPv4 subnets and advertise
    /// them alongside `advertise_routes` (union). Default ON: a subnet router
    /// is zero-config — the admin sees each LAN the host is on as a suggestion
    /// (Admin → Agents → Subnet routes) and approves what should be routed. Set
    /// `false` to advertise only the explicit `advertise_routes`. Detected
    /// routes are UNTRUSTED until approved, so this is safe to leave on.
    #[serde(default = "default_true")]
    pub advertise_local_subnets: bool,

    /// P6: DECLARED, daemon-supervised tunnel routes (`[[tunnel_routes]]`).
    /// Each enabled entry is reconciled into a live daemon flow on every
    /// startup (and on change) by `tunnel::route_reconciler` — the
    /// persistent counterpart of the ephemeral LocalAPI `CreateForward`
    /// flows. The struct is `tunnel_core::localapi::RouteDescriptor`, one
    /// type for wire + disk. Managed via `roomler route add/rm/...` or the
    /// desktop Tunnels pane (the DAEMON writes this field — LocalAPI verbs
    /// persist through the daemon's config-write lock); hand-editing the
    /// TOML also works (picked up at the next daemon start).
    ///
    /// NB: a crash-rollback to a pre-P6 binary rewrites the config without
    /// this field (no unknown-field preservation) — declared routes do not
    /// survive an auto-rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnel_routes: Vec<tunnel_core::localapi::RouteDescriptor>,
}

/// serde default for `advertise_local_subnets` — auto-detect is ON by default.
fn default_true() -> bool {
    true
}

/// Current schema version. Bumped whenever [`migrate`] gains a new
/// step. Persisted into the config file by the migration so subsequent
/// runs short-circuit the migration check.
pub const CURRENT_SCHEMA_VERSION: &str = "0.3.0-rc.198";

/// Apply schema migrations to `cfg` in place. Returns `true` when the
/// caller should persist the mutated config via [`save`]. Safe to call
/// on a freshly-loaded config of any age — same-version configs return
/// `false` after a no-op pass.
///
/// Migrations applied (rc.18 set):
/// 1. Trim trailing slash on `server_url` (older enrollment flows
///    occasionally left one in; harmless until something concatenates
///    a path).
/// 2. If `last_known_good_version` is a pre-rc.18 string (any 0.1.x or
///    0.2.x), reset `crash_count = 0` so the historical counter
///    doesn't trip the rc.18 rollback path.
/// 3. Stamp `config_schema_version = Some(CURRENT_SCHEMA_VERSION)` if
///    not already current.
///
/// Note: defaults for new fields (`enable_remote_browse`,
/// `auto_grant_session`, etc.) are applied at deserialize time by
/// `#[serde(default = "fn")]`. The migration's job is to ensure the
/// on-disk file matches the in-memory shape — so a future operator
/// reading `config.toml` sees ALL fields the running agent actually
/// uses, not just the ones they explicitly set when first enrolling.
pub fn migrate(cfg: &mut AgentConfig) -> bool {
    if cfg
        .config_schema_version
        .as_deref()
        .is_some_and(|v| v == CURRENT_SCHEMA_VERSION)
    {
        // Already on this schema; no-op.
        return false;
    }

    let mut changed = false;

    // 1. Trim trailing slash on server_url.
    let trimmed = cfg.server_url.trim_end_matches('/').to_string();
    if trimmed != cfg.server_url {
        cfg.server_url = trimmed;
        changed = true;
    }

    // 2. Reset crash_count when last_known_good_version is from a
    //    pre-rc.18 branch. The rollback heuristic is keyed off this
    //    counter; carrying it across branches could trip rollback
    //    against a healthy rc.18 install.
    if let Some(ref v) = cfg.last_known_good_version
        && (v.starts_with("0.1.") || v.starts_with("0.2."))
        && cfg.crash_count > 0
    {
        cfg.crash_count = 0;
        cfg.last_crash_unix = 0;
        changed = true;
    }

    // 3. Stamp the new schema version. ALWAYS persist after any
    //    migration ran, even if no other field changed — that's how
    //    we mark the file as having been processed.
    if cfg.config_schema_version.as_deref() != Some(CURRENT_SCHEMA_VERSION) {
        cfg.config_schema_version = Some(CURRENT_SCHEMA_VERSION.to_string());
        changed = true;
    }

    changed
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
    cfg.last_crash_unix > 0 && now_unix.saturating_sub(cfg.last_crash_unix) <= CRASH_WINDOW_SECS
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
/// Default for `enable_remote_browse` — `true` so a 0.3.0 install
/// preserves self-controlled-host auto-grant semantics. Operators on
/// org-controlled fleets explicitly set `enable_remote_browse = false`
/// in `config.toml`. Hard-coded helper instead of a `bool::default()`
/// because that defaults to `false`.
fn default_enable_remote_browse() -> bool {
    true
}

/// Default for `auto_grant_session` — `true` for back-compat with
/// every pre-0.3.x agent which auto-granted unconditionally
/// (signaling.rs:365 TODO). Org-controlled fleets opt out via
/// `config.toml`. See [`AgentConfig::auto_grant_session`] for the
/// security model.
fn default_auto_grant_session() -> bool {
    true
}

/// Daemon-wide config-WRITE lock (P6). The daemon has several runtime
/// writers of `config.toml` — the clean-run promotion task, the graceful
/// shutdown path, and the route reconciler's LocalAPI verbs. Each does a
/// reload-modify-save; interleaved unlocked, one writer's full-struct save
/// silently drops another's just-written field. Every daemon-side runtime
/// writer must hold this lock across its load→mutate→save. (Cross-PROCESS
/// writers — tray, CLI, wizard — remain last-writer-wins on the file;
/// [`save`]'s atomic rename keeps a torn file impossible either way.)
pub type WriteLock = std::sync::Arc<tokio::sync::Mutex<()>>;

/// A fully-populated config for unit tests in other modules (the route
/// reconciler persists through real [`save`]/[`load`] round-trips).
#[cfg(test)]
pub fn test_fixture() -> AgentConfig {
    tests::fixture()
}

pub fn default_config_path() -> Result<PathBuf> {
    let dirs =
        crate::appdirs::project_dirs().context("could not resolve a platform config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// rc.52: machine-global config path —
/// `%PROGRAMDATA%\roomler\roomler-agent\config.toml`.
///
/// `default_config_path()` resolves to a per-USER profile
/// (`%APPDATA%` via `ProjectDirs`). A SystemContext worker runs as
/// LocalSystem and, crucially, must be able to load its config
/// BEFORE any interactive user logs in (the whole point of M3 A1
/// pre-logon control). A user-profile path is unreachable pre-logon;
/// `%PROGRAMDATA%` is machine-global and LocalSystem-readable with no
/// logged-in user. The perMachine + SystemContext installer writes
/// the enrolled config here; the worker's resolution ladder consults
/// it ahead of the (never-populated) SYSTEM-profile default.
///
/// Windows-only — there is no machine-global config concept on
/// Linux/macOS (the agent there is perUser). Returns `None` if
/// `%PROGRAMDATA%` can't be resolved (it always can on a sane
/// Windows install; the `C:\ProgramData` literal is the documented
/// fallback used elsewhere in the crate).
#[cfg(target_os = "windows")]
pub fn machine_global_config_path() -> PathBuf {
    crate::appdirs::machine_global_dir().join("config.toml")
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

    // ATOMIC (P6 hardening): write a sibling temp file, then rename over
    // the target. The file holds `agent_token` — a torn `fs::write`
    // (power loss / crash mid-write) used to brick enrollment. `rename`
    // within one directory is atomic on Unix and uses
    // MOVEFILE_REPLACE_EXISTING semantics on Windows. The temp name is
    // pid-suffixed so two PROCESSES (daemon + tray/CLI) can't collide on
    // the temp file itself; last-writer-wins on the rename is the
    // documented cross-process limitation.
    let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serialised)
        .with_context(|| format!("writing config temp file {}", tmp.display()))?;

    // Tighten permissions on Unix BEFORE the rename — the file holds a
    // bearer token, and the temp file must never be world-readable even
    // for an instant at the final path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }

    std::fs::rename(&tmp, path).with_context(|| {
        // Best-effort cleanup so failed saves don't accrete temp files.
        let _ = std::fs::remove_file(&tmp);
        format!("renaming config into place at {}", path.display())
    })?;
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

    #[cfg(target_os = "windows")]
    #[test]
    fn machine_global_config_path_under_programdata_roomler() {
        let p = machine_global_config_path();
        let s = p.to_string_lossy().to_lowercase();
        assert!(s.contains("roomler"), "path missing roomler: {s}");
        assert!(
            s.ends_with(r"roomler\roomler-agent\config.toml"),
            "unexpected tail: {s}"
        );
        // Distinct from the perUser default (which is under %APPDATA%).
        assert_ne!(p, default_config_path().unwrap());
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

    pub(super) fn fixture() -> AgentConfig {
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
            enable_remote_browse: true,
            auto_grant_session: true,
            last_known_good_version: None,
            crash_count: 0,
            last_crash_unix: 0,
            rollback_attempted: false,
            last_run_unhealthy: false,
            config_schema_version: None,
            forward_acl: AgentForwardAcl::default(),
            virtual_desktop_apps: crate::apps::VirtualDesktopAppsConfig::default(),
            overlay_enabled: false,
            overlay_wg_secret_key: None,
            overlay_advertised_routes: Vec::new(),
            advertise_routes: Vec::new(),
            advertise_local_subnets: true,
            tunnel_routes: Vec::new(),
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

    // ---- Migration tests (rc.18 P4) ---------------------------------

    #[test]
    fn migrate_pre_rc18_stamps_schema_version() {
        // Old config has no version field. Migration runs the rc.18
        // step set and stamps CURRENT_SCHEMA_VERSION so subsequent
        // launches no-op.
        let mut cfg = fixture();
        assert!(cfg.config_schema_version.is_none());
        let changed = migrate(&mut cfg);
        assert!(changed, "first migration must rewrite the config");
        assert_eq!(
            cfg.config_schema_version.as_deref(),
            Some(CURRENT_SCHEMA_VERSION)
        );
    }

    #[test]
    fn migrate_same_schema_is_noop() {
        // Second launch on the same version: migrate returns false,
        // caller skips the save.
        let mut cfg = fixture();
        cfg.config_schema_version = Some(CURRENT_SCHEMA_VERSION.to_string());
        let changed = migrate(&mut cfg);
        assert!(!changed, "same-version migration must be a no-op");
    }

    #[test]
    fn migrate_trims_trailing_slash_on_server_url() {
        let mut cfg = fixture();
        cfg.server_url = "https://example.invalid/".into();
        let changed = migrate(&mut cfg);
        assert!(changed);
        assert_eq!(cfg.server_url, "https://example.invalid");
    }

    #[test]
    fn migrate_no_trailing_slash_to_trim() {
        // server_url already clean; ONLY the schema version stamp
        // counts as a change.
        let mut cfg = fixture();
        cfg.server_url = "https://example.invalid".into();
        let changed = migrate(&mut cfg);
        assert!(changed); // schema version stamp
        assert_eq!(cfg.server_url, "https://example.invalid");
    }

    #[test]
    fn migrate_resets_crash_count_from_pre_rc18_branch() {
        // last_known_good_version from 0.2.x with a live crash counter:
        // those crashes happened on a different branch, the counter
        // must not trip rollback against rc.18.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.2.7".into());
        cfg.crash_count = 2;
        cfg.last_crash_unix = 1_700_000_000;
        let changed = migrate(&mut cfg);
        assert!(changed);
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
    }

    #[test]
    fn migrate_preserves_crash_count_for_same_branch() {
        // 0.3.x crash counter is still relevant when running 0.3.x —
        // don't wipe it.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.3.0-rc.16".into());
        cfg.crash_count = 1;
        cfg.last_crash_unix = 1_700_000_000;
        migrate(&mut cfg);
        assert_eq!(cfg.crash_count, 1, "0.3.x history must be preserved");
        assert_eq!(cfg.last_crash_unix, 1_700_000_000);
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
