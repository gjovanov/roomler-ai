// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-93 P1 — can a controller's input reach this desktop at all?
//!
//! An injector that reaches nothing does not fail. XTest into Xwayland
//! SUCCEEDS and moves Xwayland's own pointer, which the compositor ignores.
//! Where Xwayland sends XTest through EI it reaches no window at all. GNOME 49
//! and later start it with `-enable-ei-portal`. KWin hands it an EI socket in
//! its environment (`LIBEI_SOCKET`) instead, and gates it behind its own prompt.
//! Both measured on the FR-93 lab: GNOME 50, and Plasma 6.7, where xdotool moved
//! nothing and clicked nothing even in an X11 window. Before this, a session on
//! such a host looked fully interactive and silently did nothing.
//!
//! So each session is told what its input will reach (`rc:input-reach` over
//! the control channel, only when that is less than everything), decided from
//! the same facts `input::open_default`'s cascade decides from. The decision is
//! pure ([`decide`]) so its table is testable; [`current`] gathers the facts.

/// How far a controller's input reaches on this desktop. Additive on the
/// wire: an older viewer ignores the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Everything the person at the screen could touch.
    Full,
    /// Only X11 apps running on Xwayland; native Wayland windows ignore it.
    XwaylandOnly,
    /// Nothing at all.
    None,
}

impl Reach {
    /// The wire spelling. A compatibility surface: renaming one changes what
    /// every deployed viewer reads.
    pub fn wire(self) -> &'static str {
        match self {
            Reach::Full => "full",
            Reach::XwaylandOnly => "xwayland_only",
            Reach::None => "none",
        }
    }
}

/// The verdict for one session, with the reason in operator words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputReach {
    pub reach: Reach,
    pub reason: String,
}

/// The X server the enigo injector would talk to.
///
/// Allowed dead: a build without enigo (or off Linux) only ever finds `Absent`.
#[cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum XServer {
    /// No `DISPLAY`, or none that answers on its Unix socket.
    Absent,
    /// A real X server: Xorg, or the daemon's own Xvfb.
    Real,
    /// Xwayland: XTest reaches its own clients only.
    Xwayland,
    /// Xwayland sends XTest through EI: it reaches nothing.
    XwaylandEi(EiRoute),
}

/// How an Xwayland reaches EI, which decides only the words of the reason.
#[cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EiRoute {
    /// `-enable-ei-portal` on its command line: GNOME 49 and later.
    Portal,
    /// A `LIBEI_SOCKET` in its environment, handed over by the compositor:
    /// KWin, measured on Plasma 6.7.
    Compositor,
}

/// Allowed dead: each build constructs only its own platform.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Linux,
    Windows,
    MacOs { accessibility: bool },
}

/// What the injector cascade would find, as data.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Facts {
    pub platform: Platform,
    /// A live portal RemoteDesktop route: it then owns injection.
    pub portal_route: bool,
    /// `uinput` is switched on, and `/dev/uinput` opens.
    pub uinput_on: bool,
    pub uinput_ok: bool,
    /// This build carries the enigo injector at all.
    pub enigo_built: bool,
    pub x: XServer,
}

const WAYLAND_FIX: &str = "On a system install switch on uinput; on a per-user one, \
     portal_capture with portal_input (the person at the screen allows Remote \
     Interaction once).";

/// The reach `input::open_default` would give a session with these facts.
pub(crate) fn decide(f: &Facts) -> InputReach {
    let verdict = |reach, reason: String| InputReach { reach, reason };
    match f.platform {
        Platform::Windows => {
            return if f.enigo_built {
                verdict(Reach::Full, "SendInput".into())
            } else {
                verdict(
                    Reach::None,
                    "this agent was built without an input backend".into(),
                )
            };
        }
        Platform::MacOs { accessibility } => {
            return if !f.enigo_built {
                verdict(
                    Reach::None,
                    "this agent was built without an input backend".into(),
                )
            } else if accessibility {
                verdict(Reach::Full, "CGEvent, with Accessibility granted".into())
            } else {
                verdict(
                    Reach::None,
                    "macOS Accessibility is not granted to this agent: grant it in System \
                     Settings, Privacy & Security, Accessibility, then restart the agent"
                        .into(),
                )
            };
        }
        Platform::Linux => {}
    }
    if f.portal_route {
        return verdict(
            Reach::Full,
            "the desktop portal's RemoteDesktop session".into(),
        );
    }
    if f.uinput_on && f.uinput_ok {
        return verdict(Reach::Full, "uinput, below the compositor".into());
    }
    let uinput_note = if f.uinput_on {
        " (uinput is switched on, but /dev/uinput did not open: it needs root or the \
         input group)"
    } else {
        ""
    };
    if !f.enigo_built {
        return verdict(
            Reach::None,
            format!("this agent was built without an X11 input backend{uinput_note}"),
        );
    }
    match f.x {
        XServer::Real => verdict(Reach::Full, "XTest on a real X server".into()),
        XServer::Xwayland => verdict(
            Reach::XwaylandOnly,
            format!(
                "this is a Wayland desktop, and XTest reaches only apps running on \
                 Xwayland{uinput_note}. {WAYLAND_FIX}"
            ),
        ),
        XServer::XwaylandEi(route) => {
            let via = match route {
                EiRoute::Portal => "the EI portal (GNOME 49 and later)",
                EiRoute::Compositor => "KWin's input-emulation gate (KDE Plasma 6)",
            };
            verdict(
                Reach::None,
                format!(
                    "this is a Wayland desktop whose Xwayland sends XTest through {via}, \
                     so it reaches no window at all{uinput_note}. {WAYLAND_FIX}"
                ),
            )
        }
        XServer::Absent => verdict(
            Reach::None,
            format!(
                "no input path reaches this desktop: no X display, no uinput{uinput_note}. {WAYLAND_FIX}"
            ),
        ),
    }
}

/// The verdict for a session starting now. Call it AFTER capture opened: a
/// portal capture registers its input route as it opens.
pub fn current() -> InputReach {
    decide(&gather())
}

fn gather() -> Facts {
    #[cfg(target_os = "windows")]
    let platform = Platform::Windows;
    #[cfg(target_os = "macos")]
    let platform = Platform::MacOs {
        accessibility: crate::tcc::accessibility_trusted(),
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let platform = Platform::Linux;

    #[cfg(all(target_os = "linux", feature = "portal-capture"))]
    let portal_route = crate::capture::portal::input_route::is_active();
    #[cfg(not(all(target_os = "linux", feature = "portal-capture")))]
    let portal_route = false;

    #[cfg(all(target_os = "linux", feature = "uinput-input"))]
    let (uinput_on, uinput_ok) = {
        let on = super::uinput_backend::env_enabled();
        (on, on && super::uinput_backend::available())
    };
    #[cfg(not(all(target_os = "linux", feature = "uinput-input")))]
    let (uinput_on, uinput_ok) = (false, false);

    Facts {
        platform,
        portal_route,
        uinput_on,
        uinput_ok,
        enigo_built: cfg!(feature = "enigo-input"),
        x: x_server(),
    }
}

#[cfg(all(target_os = "linux", feature = "enigo-input"))]
fn x_server() -> XServer {
    let Some(display) = std::env::var_os("DISPLAY") else {
        return XServer::Absent;
    };
    let display = display.to_string_lossy().into_owned();
    match crate::capture::x_server_xwayland(&display) {
        None => XServer::Absent,
        Some(false) => XServer::Real,
        Some(true) => match xwayland_ei_route(&display) {
            Some(route) => XServer::XwaylandEi(route),
            None => XServer::Xwayland,
        },
    }
}

#[cfg(not(all(target_os = "linux", feature = "enigo-input")))]
fn x_server() -> XServer {
    XServer::Absent
}

/// Does the Xwayland serving `display` send XTest through EI, and how? Read
/// from that process: GNOME starts it as `Xwayland :0 -rootless …
/// -enable-ei-portal`; KWin as `Xwayland :0 -auth … -rootless`, with a
/// `LIBEI_SOCKET=/proc/self/fd/N` in its environment. No process found, or an
/// environment this daemon may not read, means no EI: then the verdict claims
/// XTest reaches X11 apps, the old wording, and not the honest "nothing".
#[cfg(all(target_os = "linux", feature = "enigo-input"))]
fn xwayland_ei_route(display: &str) -> Option<EiRoute> {
    let number = display_number(display)?;
    let procs = std::fs::read_dir("/proc").ok()?;
    procs.flatten().find_map(|p| {
        let path = p.path();
        let is_xwayland = std::fs::read_to_string(path.join("comm"))
            .map(|c| c.trim() == "Xwayland")
            .unwrap_or(false);
        if !is_xwayland {
            return None;
        }
        let cmdline = std::fs::read(path.join("cmdline")).ok()?;
        if !cmdline_serves(&cmdline, number) {
            return None;
        }
        let environ = std::fs::read(path.join("environ")).unwrap_or_default();
        ei_route(&cmdline, &environ)
    })
}

/// `":0"` / `":0.0"` / `"unix:1"` → the display number. `None` for anything
/// else (a TCP display is never Xwayland's).
#[cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]
fn display_number(display: &str) -> Option<&str> {
    let (host, rest) = display.rsplit_once(':')?;
    if !(host.is_empty() || host == "unix") {
        return None;
    }
    let number = rest.split('.').next()?;
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then_some(number)
}

/// Does a NUL-separated command line serve display `:number`?
#[cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]
fn cmdline_serves(cmdline: &[u8], number: &str) -> bool {
    let want = format!(":{number}");
    cmdline.split(|b| *b == 0).any(|a| a == want.as_bytes())
}

/// The pure half of [`xwayland_ei_route`]: from an Xwayland's NUL-separated
/// command line and environment, how it reaches EI, if it does. An empty
/// `LIBEI_SOCKET=` names no socket, so it does not count.
#[cfg_attr(
    not(all(target_os = "linux", feature = "enigo-input")),
    allow(dead_code)
)]
fn ei_route(cmdline: &[u8], environ: &[u8]) -> Option<EiRoute> {
    if cmdline
        .split(|b| *b == 0)
        .any(|a| a == b"-enable-ei-portal")
    {
        return Some(EiRoute::Portal);
    }
    environ
        .split(|b| *b == 0)
        .any(|v| v.len() > b"LIBEI_SOCKET=".len() && v.starts_with(b"LIBEI_SOCKET="))
        .then_some(EiRoute::Compositor)
}

/// The `rc:input-reach` control-channel message. Built with serde, because a
/// reason carries the operator's words and must arrive as written.
pub fn payload(r: &InputReach) -> String {
    serde_json::json!({
        "t": "rc:input-reach",
        "reach": r.reach.wire(),
        "reason": r.reason,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux(x: XServer) -> Facts {
        Facts {
            platform: Platform::Linux,
            portal_route: false,
            uinput_on: false,
            uinput_ok: false,
            enigo_built: true,
            x,
        }
    }

    /// The FR-93 lab's per-user GNOME 50 guest, stock: XTest is all there is,
    /// and its Xwayland routes XTest through the EI portal.
    #[test]
    fn gnome_50s_xwayland_reaches_nothing() {
        let r = decide(&linux(XServer::XwaylandEi(EiRoute::Portal)));
        assert_eq!(r.reach, Reach::None);
        assert!(r.reason.contains("EI portal"), "{}", r.reason);
        assert!(r.reason.contains("portal_input"), "{}", r.reason);
    }

    /// The FR-93 lab's per-user Plasma 6.7 guest, stock: KWin's Xwayland
    /// carries no `-enable-ei-portal`, yet XTest reached nothing, not even an
    /// X11 window, because KWin handed it an EI socket.
    #[test]
    fn kwins_xwayland_reaches_nothing() {
        let r = decide(&linux(XServer::XwaylandEi(EiRoute::Compositor)));
        assert_eq!(r.reach, Reach::None);
        assert!(r.reason.contains("KWin"), "{}", r.reason);
        assert!(r.reason.contains("portal_input"), "{}", r.reason);
    }

    #[test]
    fn an_older_xwayland_reaches_only_its_own_apps() {
        assert_eq!(decide(&linux(XServer::Xwayland)).reach, Reach::XwaylandOnly);
    }

    #[test]
    fn a_real_x_server_or_the_virtual_desktop_reaches_everything() {
        assert_eq!(decide(&linux(XServer::Real)).reach, Reach::Full);
    }

    /// The lab's system guest: a root unit has no display at all.
    #[test]
    fn no_display_and_no_uinput_reaches_nothing() {
        let r = decide(&linux(XServer::Absent));
        assert_eq!(r.reach, Reach::None);
        assert!(r.reason.contains("uinput"), "{}", r.reason);
    }

    #[test]
    fn uinput_or_a_portal_route_reaches_everything_whatever_the_x_server() {
        for x in [
            XServer::Absent,
            XServer::XwaylandEi(EiRoute::Portal),
            XServer::XwaylandEi(EiRoute::Compositor),
            XServer::Xwayland,
        ] {
            let uinput = Facts {
                uinput_on: true,
                uinput_ok: true,
                ..linux(x)
            };
            assert_eq!(decide(&uinput).reach, Reach::Full, "{x:?}");
            let portal = Facts {
                portal_route: true,
                ..linux(x)
            };
            assert_eq!(decide(&portal).reach, Reach::Full, "{x:?}");
        }
    }

    /// uinput switched on but unusable falls through to XTest, as
    /// `open_default` does, and the reason says why uinput did not count.
    #[test]
    fn uinput_that_did_not_open_says_so() {
        let f = Facts {
            uinput_on: true,
            uinput_ok: false,
            ..linux(XServer::XwaylandEi(EiRoute::Portal))
        };
        let r = decide(&f);
        assert_eq!(r.reach, Reach::None);
        assert!(
            r.reason.contains("/dev/uinput did not open"),
            "{}",
            r.reason
        );
    }

    #[test]
    fn a_build_without_an_injector_reaches_nothing() {
        let f = Facts {
            enigo_built: false,
            ..linux(XServer::Real)
        };
        assert_eq!(decide(&f).reach, Reach::None);
    }

    #[test]
    fn a_mac_without_accessibility_reaches_nothing() {
        let mac = |accessibility| Facts {
            platform: Platform::MacOs { accessibility },
            ..linux(XServer::Absent)
        };
        assert_eq!(decide(&mac(true)).reach, Reach::Full);
        let r = decide(&mac(false));
        assert_eq!(r.reach, Reach::None);
        assert!(r.reason.contains("Accessibility"), "{}", r.reason);
    }

    #[test]
    fn windows_reaches_everything() {
        let f = Facts {
            platform: Platform::Windows,
            ..linux(XServer::Absent)
        };
        assert_eq!(decide(&f).reach, Reach::Full);
    }

    /// GNOME 50's Xwayland command line, as the lab's guest runs it.
    #[test]
    fn gnome_50s_command_line_is_recognised() {
        // NUL-separated, as /proc/<pid>/cmdline holds it.
        let cmdline = |args: &[&str]| args.join("\0").into_bytes();
        let gnome = cmdline(&[
            "Xwayland",
            ":0",
            "-rootless",
            "-noreset",
            "-accessx",
            "-core",
            "-auth",
            "/run/user/1000/.mutter-Xwaylandauth.Q1",
            "-listenfd",
            "4",
            "-displayfd",
            "6",
            "-enable-ei-portal",
            "",
        ]);
        assert!(cmdline_serves(&gnome, "0"));
        assert!(!cmdline_serves(&gnome, "1"), "another display's Xwayland");
        assert_eq!(ei_route(&gnome, b""), Some(EiRoute::Portal));
        let older = cmdline(&["Xwayland", ":0", "-rootless", "-noreset", ""]);
        assert!(cmdline_serves(&older, "0"));
        assert_eq!(ei_route(&older, b"PATH=/usr/bin\0"), None);
    }

    /// Plasma 6.7's Xwayland, as the lab's KDE guest runs it: nothing about EI
    /// on the command line, and the socket in the environment.
    #[test]
    fn plasmas_environment_is_recognised() {
        let nul = |parts: &[&str]| parts.join("\0").into_bytes();
        let kwin = nul(&[
            "/usr/bin/Xwayland",
            ":0",
            "-auth",
            "/run/user/1000/xauth_PPjmwd",
            "-listenfd",
            "8",
            "-listenfd",
            "9",
            "-displayfd",
            "86",
            "-wm",
            "93",
            "-rootless",
            "",
        ]);
        assert!(cmdline_serves(&kwin, "0"));
        let env = nul(&[
            "XDG_SESSION_TYPE=wayland",
            "LIBEI_SOCKET=/proc/self/fd/65",
            "WAYLAND_SOCKET=95",
            "",
        ]);
        assert_eq!(ei_route(&kwin, &env), Some(EiRoute::Compositor));
        // No socket, an empty one, or a look-alike name: no EI.
        assert_eq!(ei_route(&kwin, &nul(&["WAYLAND_SOCKET=95", ""])), None);
        assert_eq!(ei_route(&kwin, &nul(&["LIBEI_SOCKET=", ""])), None);
        assert_eq!(ei_route(&kwin, &nul(&["XLIBEI_SOCKET=/x", ""])), None);
    }

    #[test]
    fn display_numbers_are_parsed_and_tcp_displays_refused() {
        assert_eq!(display_number(":0"), Some("0"));
        assert_eq!(display_number(":12.0"), Some("12"));
        assert_eq!(display_number("unix:1"), Some("1"));
        assert_eq!(display_number("10.0.0.1:0"), None);
        assert_eq!(display_number(":x"), None);
        assert_eq!(display_number(""), None);
    }

    #[test]
    fn the_wire_names_and_payload_are_locked() {
        assert_eq!(Reach::Full.wire(), "full");
        assert_eq!(Reach::XwaylandOnly.wire(), "xwayland_only");
        assert_eq!(Reach::None.wire(), "none");
        let p = payload(&InputReach {
            reach: Reach::None,
            reason: "a \"quoted\" reason".into(),
        });
        let v: serde_json::Value = serde_json::from_str(&p).expect("JSON");
        assert_eq!(v["t"], "rc:input-reach");
        assert_eq!(v["reach"], "none");
        assert_eq!(v["reason"], "a \"quoted\" reason");
    }
}
