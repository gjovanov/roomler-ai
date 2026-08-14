//! Library crate for `roomler-agent`. The binary at `src/main.rs` is a thin
//! CLI shell around these modules; exposing them here lets integration
//! tests drive the agent in-process against a `TestApp` server.

// P3e lever E: the daemon-free building blocks (appdirs, machine, config,
// config_surface, enrollment, logging, logs_upload, crash_recorder, the
// notify primitives, the forward ACL, the apps config shapes) moved to the
// `roomler-agent-core` crate so the desktop companion can link them without
// this crate's data plane. Re-exported here under their old `crate::` paths —
// every internal call site is unchanged. `notify` stays a real module (it
// layers the daemon-only worker-aware wrappers over the core primitives);
// `apps` re-exports the moved config shapes; the ACL is re-exported inside
// `tunnel/mod.rs` so `crate::tunnel::acl::…` still resolves.
pub use roomler_agent_core::{
    appdirs, config, config_surface, crash_recorder, enrollment, logging, logs_upload, machine,
};

pub mod apps;
#[cfg(feature = "audio")]
pub mod audio;
pub mod capture;
#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod companion;
pub mod consent;
pub mod crash_uploader;
#[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
pub mod derp;
pub mod display_match;
pub mod displays;
#[cfg(target_os = "windows")]
pub mod dpi;
pub mod encode;
pub mod exec;
pub mod files;
pub mod fp16;
pub mod gpu_clock;
pub mod indicator;
pub mod input;
pub mod install_cleanup;
pub mod install_detect;
pub mod instance_lock;
pub mod jwt_introspect;
pub mod localapi_state;
pub mod lock_overlay;
pub mod lock_state;
pub mod logs_fetch;
pub mod mdns_resolve;
// P5 — crate-private: its surface leans on `peer::TargetResolution`
// (pub(crate)) and nothing outside the agent consumes it. Compiled on
// every build so its pure logic unit-tests on the default feature set
// (mirrors `encode::viewer_rate`); the dead_code allow covers builds
// without the DC video pumps, which are its only production callers.
#[cfg_attr(
    not(any(feature = "vp9-444", feature = "ffmpeg-encoder")),
    allow(dead_code)
)]
pub(crate) mod media_share;
pub mod notify;
pub mod org_join;
#[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
pub mod overlay;
pub mod peer;
pub mod post_install;
pub mod preflight;
pub mod rc_local_turn;
pub mod relay_probe;
pub mod service;
pub mod session_telemetry;
pub mod signaling;
pub mod subnet_detect;
#[cfg(feature = "system-context")]
pub mod system_context;
pub mod telemetry;
pub mod tunnel;
pub mod updater;
pub mod version_sweep;
pub mod virtual_desktop;
pub mod watchdog;
#[cfg(target_os = "windows")]
pub mod win32_monitors;
#[cfg(target_os = "windows")]
pub mod win_service;
#[cfg(target_os = "windows")]
pub mod win_timer;

/// P5 exit-node crash-safety (A2) — synchronously purge any leftover
/// split-default routes from the overlay NIC. Called at agent startup (the
/// boot-time reconciler: heal a `/1` a crash / kill / unclean reboot left
/// behind) AND immediately before each `std::process::exit` that bypasses the
/// runtime's RAII teardown (watchdog stall, self-update, agent-deleted) — those
/// paths run NO destructors, so without this a Windows host keeps a stale
/// `0.0.0.0/1` pointed at a dead Wintun adapter and blackholes all egress until
/// reboot.
///
/// Lives at the crate root (always compiled) so the exit paths in `watchdog` /
/// `signaling` / `main` can call it WITHOUT an overlay-feature gate — the
/// `overlay` module itself is `cfg`-gated. No-op unless this is an `overlay-l3`
/// build (only the OS-TUN surface installs OS routes; the userspace netstack has
/// none). Best-effort + scoped to the roomler NIC.
pub fn purge_exit_routes() {
    #[cfg(feature = "overlay-l3")]
    {
        // Multi-org v2 — the purges are per-adapter now; this boot/pre-exit
        // path heals the LEGACY/PRIMARY adapter (per-org adapters are Phase
        // 2c, whose reconciler will walk the configured set).
        tunnel_core::overlay::tun::purge_split_default(tunnel_core::overlay::tun::IF_NAME);
        // Drop peer/subnet routes a PREVIOUS generation left on a persisted TUN.
        // Their crypto-router entries died with that runtime, so they black-hole
        // silently: the OS route looks right, the peer is online, and only the
        // traffic vanishes (field case 2026-08-03). Safe here because the router
        // is empty until `install_peers` runs, which re-adds the live set.
        tunnel_core::overlay::tun::purge_stale_peer_routes(tunnel_core::overlay::tun::IF_NAME);
        // S4b — also drop any leftover exit-node DNS steer. On Windows the `.`-root
        // NRPT rule is machine-global and PERSISTS across a crash/reboot, so a stale
        // rule pointing at a dead resolver would blackhole ALL DNS until removed —
        // this boot/pre-exit purge is the load-bearing cleanup for it.
        tunnel_core::overlay::dns::purge_exit_dns();
    }
}
