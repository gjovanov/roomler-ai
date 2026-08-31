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
///
/// Serde is here for one reason: the helper child reports across a process
/// boundary (see `helper`), and a status that crosses it must arrive as the
/// same value it left as — including *why* it was unavailable, which is the
/// whole point of the enum.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
                // ⚠️ A MISSING backend is only one cause, and on the one host
                // where this was actually diagnosed it was not the cause: the
                // backend was installed, but its (static, D-Bus-activated)
                // unit was dead — and starting it alone still did not help,
                // because the FRONTEND caches its backend selection at
                // startup and has to be restarted after the backend is up.
                // Advice that only said "install it" sent the reader to a
                // package that was already there.
                "xdg-desktop-portal is running but exposes no ScreenCast. Check the backend for \
                 your compositor is INSTALLED AND RUNNING (systemctl --user status \
                 xdg-desktop-portal-gnome / -kde / -wlr), then restart the frontend (systemctl \
                 --user restart xdg-desktop-portal) — it caches its backend selection at startup"
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

/// Ask the portal *from wherever it can actually be reached* — this process if
/// it has a session bus, otherwise a helper child inside the console user's
/// session.
///
/// ## Why a child at all
///
/// FR-45 P1 measured the thing that shapes this whole phase: `roomlerd` runs
/// as **root**, and the portal is **per-user-session**. Detection from the
/// daemon returns `NoSessionBus` on a host with a perfectly good GNOME Wayland
/// session running three feet away. No amount of care in the D-Bus code fixes
/// that — the daemon is simply not in the session. Something has to run
/// *inside* it.
///
/// ## Why a subcommand and not a separate binary
///
/// The near-miss worth recording: a helper **subcommand does not by itself**
/// solve FR-45's dependency problem. `roomlerd portal-helper` is the same ELF
/// as `roomlerd`, so linking `libpipewire` for the helper would put it in the
/// daemon's `DT_NEEDED` and stop the daemon *starting* on every headless host
/// that will never run a portal. The subcommand buys the session context; the
/// `dlopen` that P3 will use buys the linkage. Both are needed, and it is easy
/// to believe the first has bought the second.
///
/// P2a is only the first half: the helper exists, runs in the session, and
/// reports. Nothing is linked or dlopened yet.
pub fn detect_in_session() -> PortalStatus {
    // Cheapest first, and it is not merely an optimisation: an attended daemon
    // running as the logged-in user IS in the session already, and spawning a
    // child to ask a question we can answer here would be pure cost.
    let direct = detect();
    if !matches!(direct, PortalStatus::NoSessionBus) {
        return direct;
    }
    // We ARE the helper and still found no bus. Report it rather than
    // spawning ourselves again — belt-and-braces against a recursion that
    // would fork-bomb a host.
    if std::env::var_os(helper::CHILD_ENV).is_some() {
        return direct;
    }
    helper::probe()
}

/// The session-resident half: spawn ourselves as the console user, ask there,
/// read the answer back.
pub mod helper {
    use super::PortalStatus;

    /// Marks the line carrying the child's JSON. Scanned for by prefix, never
    /// taken as "the last line" — the child's own logging shares this stream
    /// and would be mistaken for the result the first time it said anything.
    const MARKER: &str = "ROOMLER_PORTAL_JSON:";

    /// Set in the child's environment; `detect_in_session` refuses to spawn
    /// when it sees it.
    pub(super) const CHILD_ENV: &str = "ROOMLERD_PORTAL_CHILD";

    /// Generous for a handful of D-Bus round-trips, and deliberately larger
    /// than nothing: a wedged session bus blocks `Introspect` for D-Bus's own
    /// 25 s method timeout, and a capture cascade must not stall behind it.
    /// This is a backstop against a hung bus, not a performance budget.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    /// The `portal-helper` child's entire job, for P2a: detect, print one
    /// marked line. It grows a ScreenCast session in P2b.
    ///
    /// The human-readable line goes to **stderr**, which the parent inherits,
    /// so the daemon's log carries the child's own account of what it saw next
    /// to the parent's verdict. Same reasoning as the caps probe.
    pub fn run() {
        let st = super::detect();
        eprintln!("portal-helper: {st} — {}", st.advice());
        match serde_json::to_string(&st) {
            Ok(json) => println!("{MARKER}{json}"),
            // Unreachable in practice; still not silent. A child that cannot
            // report is worse than one that reports a failure.
            Err(e) => println!(
                "{MARKER}{}",
                serde_json::to_string(&PortalStatus::Unknown(format!("encoding status: {e}")))
                    .unwrap_or_else(|_| r#"{"Unknown":"encoding status"}"#.to_string())
            ),
        }
    }

    /// Parent side. Every failure — no session, no drop, no spawn, timeout,
    /// unparseable output — resolves to a status rather than an error, because
    /// the caller's question is "can the portal serve this host" and every one
    /// of those answers it with *no*.
    pub(super) fn probe() -> PortalStatus {
        let sess = match crate::companion::graphical_session() {
            Ok(s) => s,
            Err(e) => {
                // Nobody is at this machine's screen. That is not a fault, it
                // IS the answer, and `NoSessionBus`'s advice already says the
                // portal is an attended path by design.
                tracing::debug!(%e, "portal: no graphical session to probe from");
                return PortalStatus::NoSessionBus;
            }
        };
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return PortalStatus::Unknown(format!("resolving our own path: {e}")),
        };

        let mut cmd = std::process::Command::new(exe);
        cmd.arg("portal-helper")
            .env(CHILD_ENV, "1")
            // The two variables that put a process in a user's session. The
            // uid drop below is what makes them usable: `XDG_RUNTIME_DIR` is
            // 0700 and the bus checks `SO_PEERCRED`, so pointing root at them
            // gets a permission error, not a session.
            .env("XDG_RUNTIME_DIR", format!("/run/user/{}", sess.uid))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path=/run/user/{}/bus", sess.uid),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        if let Some(d) = &sess.display {
            cmd.env("DISPLAY", d);
        }
        if let Some(w) = &sess.wayland_display {
            cmd.env("WAYLAND_DISPLAY", w);
        }

        // The one privilege story. Not `CommandExt::uid()`: that leaves the
        // child in root's supplementary groups, which is a silent retention
        // bug rather than a visible failure.
        if let Err(e) = crate::exec::drop_to_std(&mut cmd, &sess.name) {
            return PortalStatus::Unknown(format!(
                "cannot run the portal helper as the session's owner: {e}"
            ));
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return PortalStatus::Unknown(format!("spawning the portal helper: {e}")),
        };

        // Drain stdout on another thread while waiting: a child that filled
        // the pipe would otherwise deadlock against our own wait.
        let out = child.stdout.take();
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            if let Some(mut s) = out {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        });

        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    tracing::warn!(
                        timeout_s = TIMEOUT.as_secs(),
                        "portal: the helper hung — treating the portal as unavailable"
                    );
                    return PortalStatus::Unknown("the portal helper hung".into());
                }
                Err(e) => return PortalStatus::Unknown(format!("waiting for the helper: {e}")),
            }
        }

        let text = reader.join().unwrap_or_default();
        parse_marked(&text).unwrap_or_else(|| {
            PortalStatus::Unknown("the portal helper reported nothing usable".into())
        })
    }

    /// Pull the status out of the child's stdout.
    ///
    /// Pure and separately testable on purpose — this is the half that a
    /// chatty child breaks, and the failure looks like an unavailable portal
    /// rather than a parse bug.
    fn parse_marked(out: &str) -> Option<PortalStatus> {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(MARKER))
            .and_then(|json| serde_json::from_str(json).ok())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A status must survive the process boundary unchanged — including
        /// the reason it was unavailable, which is the entire value of the
        /// enum over a bool.
        #[test]
        fn every_status_round_trips_through_the_wire() {
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
                PortalStatus::Unknown("session bus: connection refused".into()),
            ];
            for st in all {
                let line = format!("{MARKER}{}", serde_json::to_string(&st).unwrap());
                assert_eq!(
                    parse_marked(&line),
                    Some(st.clone()),
                    "{st} did not survive"
                );
            }
        }

        /// The child shares stdout with anything else that writes there. The
        /// marked line has to be *found*, not assumed to be the last one —
        /// taking the last line breaks the first time the child says anything
        /// else, and it breaks as "portal unavailable".
        #[test]
        fn the_marked_line_is_found_among_noise() {
            let st = PortalStatus::Available {
                remote_desktop: true,
                version: Some(5),
            };
            let text = format!(
                "some library banner\n{MARKER}{}\ntrailing chatter after the result\n",
                serde_json::to_string(&st).unwrap()
            );
            assert_eq!(parse_marked(&text), Some(st));
        }

        /// No marker, or a marker carrying garbage, must be `None` so the
        /// caller reports an unusable portal rather than inventing a status.
        #[test]
        fn unmarked_or_corrupt_output_yields_nothing() {
            assert_eq!(parse_marked(""), None);
            assert_eq!(parse_marked("portal is fine, honest\n"), None);
            assert_eq!(parse_marked(&format!("{MARKER}not json")), None);
        }
    }
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
