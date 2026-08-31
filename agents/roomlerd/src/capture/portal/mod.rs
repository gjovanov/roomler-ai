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

/// FR-45 P3c-ii — the sixth `ScreenCapture` backend.
pub mod backend;
/// FR-45 P4 — wire input events onto RemoteDesktop `Notify*` calls (helper side).
pub mod input;
/// FR-45 P4 — the arbiter→helper input seam (daemon side).
pub mod input_route;
/// FR-45 P3a — reaching `libpipewire` through `dlopen`, never a link.
pub mod pipewire;
/// FR-45 P3b — SPA POD serialisation, so a format can be negotiated.
pub mod pod;
/// FR-45 P2b — the ScreenCast handshake itself, run inside the session.
pub mod screencast;
/// FR-45 P3c-ii — the frame wire format between helper and daemon.
pub mod wire;

use std::fmt;

/// The portal's well-known bus name and object path.
pub(crate) const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
pub(crate) const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub(crate) const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
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

    /// What we ask the compositor for as an upper bound. It is a *range* max,
    /// not a demand — the source picks its own rate within it, and asking for
    /// a single fixed rate is how a negotiation fails on a display that runs
    /// at something else.
    const DEFAULT_MAX_FPS: u32 = 60;

    /// How many frames the helper pulls before reporting. A handful proves
    /// delivery works; this is a proof, not a capture session.
    const WANT_FRAMES: u32 = 3;

    /// P2b's marker. A **separate** marker rather than a variant inside one
    /// payload, so a parent asking for detection can never be handed a session
    /// report (or the reverse) by a helper built from a different revision.
    const SESSION_MARKER: &str = "ROOMLER_PORTAL_SESSION:";

    /// Set in the child's environment; `detect_in_session` refuses to spawn
    /// when it sees it.
    pub(super) const CHILD_ENV: &str = "ROOMLERD_PORTAL_CHILD";

    /// Generous for a handful of D-Bus round-trips, and deliberately larger
    /// than nothing: a wedged session bus blocks `Introspect` for D-Bus's own
    /// 25 s method timeout, and a capture cascade must not stall behind it.
    /// This is a backstop against a hung bus, not a performance budget.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    /// The `portal-helper` child's entire job: do one thing inside the
    /// session, print one marked line.
    ///
    /// The human-readable line goes to **stderr**, which the parent inherits,
    /// so the daemon's log carries the child's own account of what it saw next
    /// to the parent's verdict. Same reasoning as the caps probe.
    pub fn run(screencast: bool, stream: bool, input: bool) {
        if stream {
            run_stream(input);
            return;
        }
        if screencast {
            run_screencast();
            return;
        }
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

    /// P2b — open a ScreenCast session and report it.
    ///
    /// 🔑 **The restore token never appears here.** It is a standing grant the
    /// person at the screen gave to *their* session, and `screencast::open`
    /// both loads and stores it internally, in their own state directory at
    /// 0600. A caller that cannot hold the credential cannot leak it — which
    /// is a stronger guarantee than a caller that holds it and is careful, and
    /// it is what makes "the daemon never sees the token" structurally true
    /// rather than merely intended.
    /// What the child announces before it starts writing pixels.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct StreamStarted {
        pub width: u32,
        pub height: u32,
        pub video_format: u32,
        /// P4 — whether this helper consumes `InputMsg` JSON lines on stdin.
        /// `serde(default)` so a handshake from an older helper reads as
        /// "no input", never as a parse failure.
        #[serde(default)]
        pub input_ok: bool,
    }

    /// Marks the streaming handshake line. A THIRD marker, distinct from the
    /// other two, so a parent expecting a stream can never be handed a
    /// detection result by a helper built from another revision.
    const STREAM_MARKER: &str = "ROOMLER_PORTAL_STREAM:";

    /// P3c-ii, child side — open a session, then write frames on stdout until
    /// the parent goes away.
    ///
    /// ⚠️ **stdout is binary after the handshake line.** Everything
    /// diagnostic goes to stderr, which the parent inherits: a stray
    /// `println!` here does not add a log line, it corrupts a frame.
    fn run_stream(with_input: bool) {
        use super::screencast::SessionKind;
        // The session shape is decided by what the portal OFFERS, not by
        // failing and retrying: a wlr backend has ScreenCast and no
        // RemoteDesktop (measured — the WSL2 field runs), and asking it for
        // input would fail the whole session where capture alone works.
        let kind = if with_input {
            match super::detect() {
                PortalStatus::Available {
                    remote_desktop: true,
                    ..
                } => SessionKind::WithInput,
                st => {
                    eprintln!(
                        "portal-helper: input requested but not available ({st}) — capture only"
                    );
                    SessionKind::CaptureOnly
                }
            }
        } else {
            SessionKind::CaptureOnly
        };
        eprintln!(
            "portal-helper: opening a {} session for streaming",
            match kind {
                SessionKind::WithInput => "RemoteDesktop (capture + input)",
                SessionKind::CaptureOnly => "ScreenCast (capture only)",
            }
        );
        let mut session = match super::screencast::open(kind) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("portal-helper: {e}");
                std::process::exit(1);
            }
        };
        // `take`, not destructure: the session must stay WHOLE — its D-Bus
        // connection keeps the portal session (and so the PipeWire node and
        // the input grant) alive, and the input context still needs it.
        let Some(fd) = session.pipewire_fd.take() else {
            eprintln!("portal-helper: the portal gave no PipeWire fd");
            std::process::exit(1);
        };
        let Some(first) = session.report.streams.first() else {
            eprintln!("portal-helper: the portal gave no stream to attach to");
            std::process::exit(1);
        };
        let node_id = first.node_id;
        let advertised = (first.width, first.height);

        let handle = match super::pipewire::stream(fd, node_id, DEFAULT_MAX_FPS) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("portal-helper: {e}");
                std::process::exit(1);
            }
        };
        eprintln!(
            "portal-helper: streaming {}x{} (spa format {})",
            handle.format.width, handle.format.height, handle.format.video_format
        );

        // P4 — the input pump, spawned before the frame loop takes this
        // thread. The portal's advertised stream size is the LOGICAL space
        // absolute motion is addressed in; the negotiated PIXEL size is only
        // a fallback for a portal that advertised none (correct wherever the
        // scale factor is 1, and the best available answer elsewhere).
        let mut input_ok = false;
        if let Some(sess_path) = session.input_session.clone() {
            let logical = match advertised {
                (Some(w), Some(h)) if w > 0 && h > 0 => (f64::from(w), f64::from(h)),
                _ => (
                    f64::from(handle.format.width),
                    f64::from(handle.format.height),
                ),
            };
            match super::input::InputContext::new(session.connection(), sess_path, node_id, logical)
            {
                Ok(ctx) => {
                    std::thread::spawn(move || super::input::run_pump(ctx, std::io::stdin()));
                    input_ok = true;
                    eprintln!(
                        "portal-helper: input pump ready (logical {}x{})",
                        logical.0, logical.1
                    );
                }
                Err(e) => {
                    eprintln!("portal-helper: input context failed: {e} — capture only");
                }
            }
        }

        let started = StreamStarted {
            width: handle.format.width,
            height: handle.format.height,
            video_format: handle.format.video_format,
            input_ok,
        };
        match serde_json::to_string(&started) {
            Ok(j) => println!("{STREAM_MARKER}{j}"),
            Err(e) => {
                eprintln!("portal-helper: could not encode the handshake: {e}");
                std::process::exit(1);
            }
        }
        use std::io::Write;
        if std::io::stdout().flush().is_err() {
            std::process::exit(1);
        }

        // From here on stdout carries nothing but frames.
        let mut out = std::io::stdout().lock();
        while let Ok((h, bytes)) = handle.frames.recv() {
            // A write failure means the parent closed the pipe — the normal
            // way this process ends. Exit quietly rather than logging a
            // shutdown as an error.
            if out.write_all(&h.encode()).is_err() || out.write_all(&bytes).is_err() {
                break;
            }
            if out.flush().is_err() {
                break;
            }
        }
    }

    /// Parent side — spawn the helper for streaming and hand back the child.
    ///
    /// Unlike [`spawn_in_session`], this does NOT wait: the child lives as
    /// long as the capture does.
    pub fn spawn_streaming(
        _target_fps: u32,
        with_input: bool,
    ) -> anyhow::Result<std::process::Child> {
        let args: &[&str] = if with_input {
            &["--stream", "--input"]
        } else {
            &["--stream"]
        };
        // stdin is the input wire, piped exactly when input is wanted — a
        // helper that will never read it should see EOF, not a pipe nothing
        // writes to.
        spawn_child(args, with_input).map_err(|e| match e {
            SpawnError::NoSession => {
                anyhow::anyhow!("nobody is at this machine's screen — the portal is attended-only")
            }
            SpawnError::Other(why) => anyhow::anyhow!(why),
        })
    }

    /// Read the streaming handshake line, skipping anything the child wrote
    /// before it.
    pub fn read_stream_handshake(r: &mut impl std::io::BufRead) -> anyhow::Result<StreamStarted> {
        let mut line = String::new();
        for _ in 0..64 {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                anyhow::bail!("the portal helper exited before announcing a stream");
            }
            if let Some(json) = line.trim().strip_prefix(STREAM_MARKER) {
                return Ok(serde_json::from_str(json)?);
            }
        }
        anyhow::bail!("the portal helper never announced a stream")
    }

    fn run_screencast() {
        eprintln!("portal-helper: opening a ScreenCast session");
        let outcome =
            super::screencast::open(super::screencast::SessionKind::CaptureOnly).map(|session| {
                let mut report = session.report;
                // P3b-ii — the fd goes to PipeWire, a stream is connected to the
                // node the portal named, and the compositor's chosen format is
                // reported. ⚠️ Still no frames: buffer delivery is P3c.
                report.pipewire = match (session.pipewire_fd, report.streams.first()) {
                    (Some(fd), Some(s)) => super::pipewire::negotiate_status(
                        fd,
                        s.node_id,
                        DEFAULT_MAX_FPS,
                        WANT_FRAMES,
                    ),
                    // A session with no stream cannot be negotiated against, and
                    // saying "not attempted" is the truth rather than a failure.
                    _ => super::pipewire::PipeWireStatus::NotAttempted,
                };
                report
            });
        match &outcome {
            Ok(r) => eprintln!(
                "portal-helper: {} stream(s), node_id={:?}, fd_ok={}, {} ms; pipewire: {}",
                r.streams.len(),
                r.streams.first().map(|s| s.node_id),
                r.pipewire_fd_ok,
                r.elapsed_ms,
                r.pipewire
            ),
            Err(e) => eprintln!("portal-helper: {e}"),
        }
        let wire: Result<super::screencast::SessionReport, super::screencast::OpenError> = outcome;
        match serde_json::to_string(&wire) {
            Ok(json) => println!("{SESSION_MARKER}{json}"),
            Err(e) => eprintln!("portal-helper: could not encode the session report: {e}"),
        }
    }

    /// Parent side. Every failure — no session, no drop, no spawn, timeout,
    /// unparseable output — resolves to a status rather than an error, because
    /// the caller's question is "can the portal serve this host" and every one
    /// of those answers it with *no*.
    pub(super) fn probe() -> PortalStatus {
        match spawn_in_session(&[], TIMEOUT) {
            Ok(text) => parse_marked(&text).unwrap_or_else(|| {
                PortalStatus::Unknown("the portal helper reported nothing usable".into())
            }),
            // Nobody is at this machine's screen. That is not a fault, it IS
            // the answer, and `NoSessionBus`'s advice already says the portal
            // is an attended path by design.
            Err(SpawnError::NoSession) => PortalStatus::NoSessionBus,
            Err(SpawnError::Other(why)) => PortalStatus::Unknown(why),
        }
    }

    /// Why the helper could not be run at all, as distinct from what it said.
    ///
    /// `NoSession` is split out because it is the one arm that is a legitimate
    /// answer rather than a failure, and every caller maps it differently.
    pub(crate) enum SpawnError {
        NoSession,
        Other(String),
    }

    /// Spawn the helper inside the console user's session and return its
    /// stdout. Shared by every helper mode, so the session lookup, the
    /// privilege drop and the deadlock-free drain exist once.
    ///
    /// `timeout` is per-mode on purpose: a detection is a few D-Bus round
    /// trips, while opening a ScreenCast session can legitimately block for as
    /// long as a human takes to read a consent dialog.
    /// Build and start the helper as the console user. Shared by the
    /// collect-and-wait path and the streaming one, so the session lookup, the
    /// session environment and the privilege drop exist exactly once — the
    /// three things that must not drift between two ways of running the same
    /// child.
    fn spawn_child(
        extra_args: &[&str],
        piped_stdin: bool,
    ) -> Result<std::process::Child, SpawnError> {
        let sess = match crate::companion::graphical_session() {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(%e, "portal: no graphical session to run the helper in");
                return Err(SpawnError::NoSession);
            }
        };
        let exe = std::env::current_exe()
            .map_err(|e| SpawnError::Other(format!("resolving our own path: {e}")))?;

        let mut cmd = std::process::Command::new(exe);
        cmd.arg("portal-helper")
            .args(extra_args)
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
            .stdin(if piped_stdin {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            // Inherited on purpose: the child's own account of what it saw
            // belongs in the daemon log, and stdout is reserved for the
            // marked line (or, when streaming, for pixels).
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
            return Err(SpawnError::Other(format!(
                "cannot run the portal helper as the session's owner: {e}"
            )));
        }
        cmd.spawn()
            .map_err(|e| SpawnError::Other(format!("spawning the portal helper: {e}")))
    }

    pub(crate) fn spawn_in_session(
        extra_args: &[&str],
        timeout: std::time::Duration,
    ) -> Result<String, SpawnError> {
        let mut child = spawn_child(extra_args, false)?;

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

        let deadline = std::time::Instant::now() + timeout;
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
                        timeout_s = timeout.as_secs(),
                        "portal: the helper hung — treating the portal as unavailable"
                    );
                    return Err(SpawnError::Other("the portal helper hung".into()));
                }
                Err(e) => {
                    return Err(SpawnError::Other(format!("waiting for the helper: {e}")));
                }
            }
        }

        Ok(reader.join().unwrap_or_default())
    }

    /// Pull the status out of the child's stdout.
    ///
    /// Pure and separately testable on purpose — this is the half that a
    /// chatty child breaks, and the failure looks like an unavailable portal
    /// rather than a parse bug.
    fn parse_marked(out: &str) -> Option<PortalStatus> {
        parse_marked_as(out, MARKER)
    }

    /// The generic form. Finds the marked line — never "the last line", which
    /// breaks the first time anything else writes to stdout — and decodes it.
    fn parse_marked_as<T: serde::de::DeserializeOwned>(out: &str, marker: &str) -> Option<T> {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(marker))
            .and_then(|json| serde_json::from_str(json).ok())
    }

    /// P2b parent side — open a ScreenCast session inside the console user's
    /// session and return what the helper reports.
    ///
    /// ⚠️ The timeout is long because **the first call shows a consent dialog**
    /// and a person has to answer it. That is not a defect to engineer away:
    /// it is the property that makes FR-45 an attended path, and the reason
    /// this can never replace FR-36's greeter and lock-screen capture.
    /// Subsequent calls carry a restore token and return in milliseconds.
    pub fn open_session() -> Result<super::screencast::SessionReport, String> {
        match spawn_in_session(&["--screencast"], SCREENCAST_TIMEOUT) {
            Ok(text) => match parse_marked_as::<
                Result<super::screencast::SessionReport, super::screencast::OpenError>,
            >(&text, SESSION_MARKER)
            {
                Some(Ok(report)) => Ok(report),
                Some(Err(e)) => Err(e.to_string()),
                None => Err("the portal helper reported no session".into()),
            },
            Err(SpawnError::NoSession) => {
                Err("nobody is at this machine's screen — the portal is an attended path".into())
            }
            Err(SpawnError::Other(why)) => Err(why),
        }
    }

    /// Long enough for a person to notice a dialog and decide. Beyond this the
    /// honest reading is that nobody is there, which is a refusal, not a hang.
    const SCREENCAST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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
