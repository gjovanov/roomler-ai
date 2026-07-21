//! Library crate for `roomler-agent`. The binary at `src/main.rs` is a thin
//! CLI shell around these modules; exposing them here lets integration
//! tests drive the agent in-process against a `TestApp` server.

pub mod appdirs;
pub mod apps;
#[cfg(feature = "audio")]
pub mod audio;
pub mod capture;
#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod config;
pub mod consent;
pub mod crash_recorder;
pub mod crash_uploader;
#[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
pub mod derp;
pub mod display_match;
pub mod displays;
#[cfg(target_os = "windows")]
pub mod dpi;
pub mod encode;
pub mod enrollment;
pub mod files;
pub mod indicator;
pub mod input;
pub mod install_cleanup;
pub mod install_detect;
pub mod instance_lock;
pub mod jwt_introspect;
pub mod localapi_state;
pub mod lock_overlay;
pub mod lock_state;
pub mod logging;
pub mod logs_fetch;
pub mod logs_upload;
pub mod machine;
pub mod mdns_resolve;
pub mod notify;
#[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
pub mod overlay;
pub mod peer;
pub mod post_install;
pub mod preflight;
pub mod service;
pub mod signaling;
pub mod subnet_detect;
#[cfg(feature = "system-context")]
pub mod system_context;
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
        tunnel_core::overlay::tun::purge_split_default();
        // S4b — also drop any leftover exit-node DNS steer. On Windows the `.`-root
        // NRPT rule is machine-global and PERSISTS across a crash/reboot, so a stale
        // rule pointing at a dead resolver would blackhole ALL DNS until removed —
        // this boot/pre-exit purge is the load-bearing cleanup for it.
        tunnel_core::overlay::dns::purge_exit_dns();
    }
}
