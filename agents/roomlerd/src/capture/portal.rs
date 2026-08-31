// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-45 P1 — is the desktop portal actually usable on this host?
//!
//! [FR-36](../../../docs/fr/FR-36-wayland-capture.md) captures *below* the
//! compositor with DRM/KMS, which needs a real CRTC. Where there is none —
//! WSL2, containers, nested compositors — `xdg-desktop-portal`'s ScreenCast
//! interface is the only route to the pixels. This module answers the question
//! that has to come first: **is it there?**
//!
//! ## Why this is a real question and not a formality
//!
//! FR-36 measured a host where `xdg-desktop-portal` was running and reachable —
//! four portal names on the session bus — and yet exposed **neither
//! `ScreenCast` nor `RemoteDesktop`**, because those come from a
//! compositor-matching backend (`-gnome`, `-kde`, `-wlr`) that was not
//! installed for the running session. "The portal is running" therefore proves
//! nothing at all. The interface has to be **looked for by name**.
//!
//! ⚠️ This module deliberately reports *why* it is unavailable rather than a
//! bare bool. "Portal unavailable" sends an operator hunting; "the portal is
//! running but exposes no ScreenCast interface — install the backend matching
//! your compositor" is a next step.
//!
//! ## Dependency note
//!
//! `zbus` is **pure Rust** — it speaks the D-Bus wire protocol itself and does
//! not link `libdbus`. That matters here: FR-45's central risk is that linking
//! `libpipewire` would put a system `.so` in `roomlerd`'s `DT_NEEDED` and stop
//! the daemon *starting* on headless hosts that will never run a portal. zbus
//! carries none of that hazard, which is why detection can land ahead of the
//! decision about how to reach PipeWire.

use std::fmt;

/// The portal's well-known bus name and object path.
const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REMOTE_DESKTOP_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";

/// What we found, in enough detail to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalStatus {
    /// ScreenCast is present. `remote_desktop` says whether input is available
    /// too — FR-45 needs BOTH on a host with no evdev consumer, so a session
    /// that can see but not touch is worth naming separately.
    Available {
        remote_desktop: bool,
        version: Option<u32>,
    },
    /// The portal answered, but does not expose ScreenCast. This is the case
    /// FR-36 hit: the service runs, the compositor-matching backend is missing.
    NoScreenCast,
    /// Nothing is listening on the portal's bus name.
    PortalAbsent,
    /// There is no session bus to ask (headless daemon, no user session).
    NoSessionBus,
    /// We could not determine it. Carries the reason rather than swallowing it.
    Unknown(String),
}

impl PortalStatus {
    pub fn usable_for_capture(&self) -> bool {
        matches!(self, PortalStatus::Available { .. })
    }

    /// One line an operator can act on. The whole point of the enum.
    pub fn advice(&self) -> &'static str {
        match self {
            PortalStatus::Available {
                remote_desktop: true,
                ..
            } => "portal ScreenCast + RemoteDesktop available",
            PortalStatus::Available {
                remote_desktop: false,
                ..
            } => "portal ScreenCast available but NO RemoteDesktop — capture would be read-only",
            PortalStatus::NoScreenCast => {
                "xdg-desktop-portal is running but exposes no ScreenCast — install the backend \
                 matching your compositor (xdg-desktop-portal-gnome / -kde / -wlr)"
            }
            PortalStatus::PortalAbsent => {
                "no xdg-desktop-portal on the session bus — install xdg-desktop-portal plus a \
                 backend for your compositor"
            }
            PortalStatus::NoSessionBus => {
                "no D-Bus session bus reachable — the portal needs a logged-in user session \
                 (it is an ATTENDED path by design)"
            }
            PortalStatus::Unknown(_) => "portal availability could not be determined",
        }
    }
}

impl fmt::Display for PortalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortalStatus::Available {
                remote_desktop,
                version,
            } => write!(
                f,
                "available (screencast v{}, remote_desktop={remote_desktop})",
                version.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
            ),
            PortalStatus::NoScreenCast => write!(f, "no-screencast"),
            PortalStatus::PortalAbsent => write!(f, "portal-absent"),
            PortalStatus::NoSessionBus => write!(f, "no-session-bus"),
            PortalStatus::Unknown(why) => write!(f, "unknown ({why})"),
        }
    }
}

/// Ask the session bus what the portal actually exposes.
///
/// Introspects rather than trusting the bus name: a portal with no
/// compositor-matching backend owns the name and answers, while offering
/// neither interface we need.
///
/// ⚠️ Runs the D-Bus work on its own thread. `zbus`'s blocking API panics when
/// called from inside a tokio runtime (it would block the reactor it is
/// standing on), and this is reachable from both the async CLI path and the
/// synchronous `open_default` cascade. A thread makes it safe from either
/// without forcing every caller to be async.
pub fn detect() -> PortalStatus {
    std::thread::spawn(detect_blocking)
        .join()
        .unwrap_or_else(|_| PortalStatus::Unknown("detection thread panicked".into()))
}

fn detect_blocking() -> PortalStatus {
    let conn = match zbus::blocking::Connection::session() {
        Ok(c) => c,
        // No session bus at all — the ordinary case on a headless daemon, and
        // not an error worth shouting about. It IS the answer.
        Err(e) => {
            return if is_missing_bus(&e) {
                PortalStatus::NoSessionBus
            } else {
                PortalStatus::Unknown(format!("session bus: {e}"))
            };
        }
    };

    let proxy = match zbus::blocking::fdo::IntrospectableProxy::builder(&conn)
        .destination(PORTAL_BUS)
        .and_then(|b| b.path(PORTAL_PATH))
        .and_then(|b| b.build())
    {
        Ok(p) => p,
        Err(e) => return PortalStatus::Unknown(format!("portal proxy: {e}")),
    };

    let xml = match proxy.introspect() {
        Ok(x) => x,
        // Name not owned / no such service: the portal simply is not there.
        Err(_) => return PortalStatus::PortalAbsent,
    };

    if !xml.contains(SCREENCAST_IFACE) {
        return PortalStatus::NoScreenCast;
    }
    PortalStatus::Available {
        remote_desktop: xml.contains(REMOTE_DESKTOP_IFACE),
        version: screencast_version(&conn),
    }
}

/// The ScreenCast interface's `version` property, when it can be read.
/// Advisory only — absence must never turn an available portal into an
/// unavailable one, which is why this returns `Option` and is not in the
/// availability decision.
fn screencast_version(conn: &zbus::blocking::Connection) -> Option<u32> {
    let p = zbus::blocking::Proxy::new(conn, PORTAL_BUS, PORTAL_PATH, SCREENCAST_IFACE).ok()?;
    p.get_property::<u32>("version").ok()
}

/// Distinguish "there is no session bus" from "the bus rejected us". The first
/// is a normal state on a headless host; the second is a fault worth naming.
fn is_missing_bus(e: &zbus::Error) -> bool {
    let s = e.to_string();
    s.contains("DBUS_SESSION_BUS_ADDRESS")
        || s.contains("No such file or directory")
        || s.contains("Failed to connect")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status must offer an operator a next step. A bare "unavailable"
    /// is what this enum exists to prevent — FR-36 lost time to exactly that.
    #[test]
    fn every_status_gives_actionable_advice() {
        let all = [
            PortalStatus::Available {
                remote_desktop: true,
                version: Some(5),
            },
            PortalStatus::Available {
                remote_desktop: false,
                version: None,
            },
            PortalStatus::NoScreenCast,
            PortalStatus::PortalAbsent,
            PortalStatus::NoSessionBus,
            PortalStatus::Unknown("x".into()),
        ];
        for s in all {
            assert!(!s.advice().is_empty(), "{s} has no advice");
        }
    }

    /// Only `Available` may be used for capture. In particular a portal that
    /// is RUNNING but exposes no ScreenCast must not count — that is the exact
    /// host FR-36 measured, and treating it as usable would open a session
    /// that then produces nothing.
    #[test]
    fn only_available_is_usable() {
        assert!(
            PortalStatus::Available {
                remote_desktop: true,
                version: Some(4)
            }
            .usable_for_capture()
        );
        // Read-only is still usable for CAPTURE; input is a separate gate.
        assert!(
            PortalStatus::Available {
                remote_desktop: false,
                version: None
            }
            .usable_for_capture()
        );
        assert!(!PortalStatus::NoScreenCast.usable_for_capture());
        assert!(!PortalStatus::PortalAbsent.usable_for_capture());
        assert!(!PortalStatus::NoSessionBus.usable_for_capture());
        assert!(!PortalStatus::Unknown("e".into()).usable_for_capture());
    }

    /// A missing ScreenCast and a missing RemoteDesktop are different
    /// problems: the first means no picture, the second means a session you
    /// can watch but not drive. FR-45 needs both on a host with no evdev
    /// consumer, so the difference has to survive into the report.
    #[test]
    fn read_only_availability_is_distinguishable() {
        let ro = PortalStatus::Available {
            remote_desktop: false,
            version: Some(5),
        };
        assert!(ro.advice().contains("read-only"));
        let rw = PortalStatus::Available {
            remote_desktop: true,
            version: Some(5),
        };
        assert!(!rw.advice().contains("read-only"));
        assert_ne!(ro, rw);
    }

    #[test]
    fn display_is_greppable() {
        assert_eq!(PortalStatus::NoScreenCast.to_string(), "no-screencast");
        assert_eq!(PortalStatus::PortalAbsent.to_string(), "portal-absent");
        assert!(
            PortalStatus::Available {
                remote_desktop: true,
                version: Some(5)
            }
            .to_string()
            .contains("v5")
        );
    }
}
