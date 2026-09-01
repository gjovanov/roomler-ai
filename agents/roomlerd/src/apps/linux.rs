// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Linux virtual-desktop backend: `wmctrl` (list/focus) + `tmux` (bash
//! sessions) + `xterm`. Shell-out only — no new crate — consistent with
//! how [`crate::virtual_desktop`] already spawns Xvfb/WM/apps.
//!
//! ## Session model (the flagship)
//! A bash "session" is a **tmux** session shown as an xterm attached to
//! it. This gives three properties a bare xterm can't:
//! * survives the agent restarting / the operator disconnecting (the
//!   tmux server outlives both);
//! * ssh-attachable (`tmux attach -t <name>` from a real login);
//! * one X window per session, so focus works.
//!
//! We launch our own windows with a known title
//! (`roomler:tmux:<session>` / `roomler:app:<key>`) so [`super::classify_title`]
//! can map a window back to its session/app without pid/xprop games.
//!
//! A tmux session with **no live xterm** (after an agent restart, or the
//! operator detached) still appears in the list with a synthetic
//! `tmux:<session>` window id; [`LinuxWm::focus`] special-cases that to
//! spawn a fresh attached xterm — so "attach to an existing bash
//! session" works within the 3-message protocol (no separate verb).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

use super::{
    LaunchOutcome, ResolvedApp, WindowInfo, WindowManager, classify_title, next_tmux_session_name,
    parse_tmux_sessions, parse_wmctrl_list,
};

/// Synthetic window-id prefix for a detached tmux session (no live X
/// window). `focus()` treats it as "attach", not "raise".
const DETACHED_PREFIX: &str = "tmux:";

/// Upper bound on concurrent tmux sessions a browser can spawn — bounds
/// resource use from a misbehaving/compromised controller.
const MAX_TMUX_SESSIONS: usize = 32;

/// How long to wait before re-listing to resolve a freshly-launched
/// window's id (best-effort; `None` on miss and the browser re-lists).
const LAUNCH_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// How to reach the desktop we manage.
///
/// FR-56 P1. Before this there was only a display string, which silently
/// encoded a second assumption: that the DAEMON owns the X server. That is
/// true in virtual-desktop mode and false everywhere else, and it is why
/// Remote Apps never engaged on a Wayland host — see [`discover`].
pub enum Target {
    /// Virtual-desktop mode: the daemon started Xvfb and owns it, so commands
    /// run as the daemon with nothing but `DISPLAY`. Byte-for-byte the
    /// pre-FR-56 behaviour, and the only population using this today.
    Daemon { display: String },
    /// A logged-in user's session — X11, or Wayland whose compositor runs
    /// Xwayland (mutter does, even headless). Commands are **dropped to that
    /// account** and carry the session's X cookie.
    ///
    /// ⚠️ The privilege drop is not politeness. `launch` spawns a terminal and
    /// a tmux server; doing that as root on somebody's own desktop session
    /// would put a root shell on their screen and leave root-owned state in
    /// their runtime dir. The daemon owning Xvfb is the *only* case where
    /// running as the daemon is the right answer.
    Session {
        display: String,
        /// `None` is legal: an X server started without auth accepts anyone.
        /// ⚠️ But on a compositor-started Xwayland it is effectively required —
        /// without it every call dies `Authorization required, but no
        /// authorization protocol specified`, which is what `DISPLAY`-only did.
        xauthority: Option<PathBuf>,
        /// The account to drop to, by NAME (`getpwnam` gives the home dir and
        /// supplementary groups a uid alone cannot).
        user: String,
    },
}

pub struct LinuxWm {
    target: Target,
}

impl LinuxWm {
    pub fn new(target: Target) -> Self {
        Self { target }
    }

    /// Build a command aimed at the target desktop.
    ///
    /// ⚠️ Fallible on purpose. If the privilege drop cannot be installed the
    /// only safe outcome is to run NOTHING — silently falling back would run a
    /// user's terminal as root, which is the exact failure this exists to
    /// prevent. An infallible `cmd()` cannot express that.
    fn cmd(&self, program: &str) -> Result<Command> {
        let mut c = Command::new(program);
        match &self.target {
            Target::Daemon { display } => {
                c.env("DISPLAY", display);
            }
            Target::Session {
                display,
                xauthority,
                user,
            } => {
                c.env("DISPLAY", display);
                if let Some(xa) = xauthority {
                    c.env("XAUTHORITY", xa);
                }
                crate::exec::drop_to_std(&mut c, user)
                    .map_err(|e| anyhow::anyhow!("cannot run `{program}` as {user}: {e}"))?;
            }
        }
        Ok(c)
    }

    /// Run a helper and capture its output. A `NotFound` spawn error is
    /// rewritten into an actionable "install X" message.
    fn run_capture(&self, program: &str, args: &[&str], apt: &str) -> Result<Output> {
        self.cmd(program)?
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!(
                        "`{program}` not installed on the agent host (apt install {apt})"
                    )
                } else {
                    anyhow::Error::new(e).context(format!("running {program}"))
                }
            })
    }

    /// Spawn a detached, stdio-null child (an xterm / GUI app). The child
    /// keeps running after the handle drops (std, unlike tokio).
    fn spawn_detached(&self, program: &str, args: &[&str], apt: &str) -> Result<()> {
        self.cmd(program)?
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!(
                        "`{program}` not installed on the agent host (apt install {apt})"
                    )
                } else {
                    anyhow::Error::new(e).context(format!("spawning {program}"))
                }
            })?;
        Ok(())
    }

    /// The active window's id (numeric), via `xprop -root
    /// _NET_ACTIVE_WINDOW`. Best-effort: `None` if xprop is missing or
    /// the property is unset.
    fn active_window(&self) -> Option<u64> {
        let out = self
            .run_capture("xprop", &["-root", "_NET_ACTIVE_WINDOW"], "x11-utils")
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // e.g. "_NET_ACTIVE_WINDOW(WINDOW): window id # 0x3400007"
        let hex = text.rsplit("0x").next()?.trim();
        u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok()
    }

    /// tmux session names (empty when no server is running — tmux exits
    /// non-zero for that, which is NOT an error here).
    fn tmux_sessions(&self) -> Vec<String> {
        match self.run_capture("tmux", &["list-sessions", "-F", "#{session_name}"], "tmux") {
            Ok(out) => parse_tmux_sessions(&String::from_utf8_lossy(&out.stdout)),
            Err(_) => Vec::new(),
        }
    }

    /// Find a freshly-launched window's id by its exact raw title.
    fn window_id_by_title(&self, want_title: &str) -> Option<String> {
        let out = self.run_capture("wmctrl", &["-l"], "wmctrl").ok()?;
        parse_wmctrl_list(&String::from_utf8_lossy(&out.stdout))
            .into_iter()
            .find(|w| w.title == want_title)
            .map(|w| w.window_id)
    }
}

impl WindowManager for LinuxWm {
    fn list(&self) -> Result<Vec<WindowInfo>> {
        // Live X windows.
        let out = self.run_capture("wmctrl", &["-l"], "wmctrl")?;
        // ⚠️ The status check is load-bearing, and it was missing. `wmctrl -l`
        // against a display it cannot open writes NOTHING to stdout and exits
        // non-zero, so parsing stdout regardless turned "I could not reach the
        // desktop" into `Ok(vec![])` — *no windows*, which is a different
        // claim and a reassuring one. Measured under FR-56 P1: pointing the
        // daemon at `:99` (no X server there) reported `windows: 0` rather
        // than an error. `focus` and `tmux new-session` already check; only
        // this one did not, and discovery makes it matter — the display is now
        // found rather than owned, so it CAN go stale (a compositor restart
        // invalidates the cookie) where an Xvfb the daemon started could not.
        if !out.status.success() {
            bail!(
                "wmctrl could not read the window list from {}: {}",
                match &self.target {
                    Target::Daemon { display } => display.clone(),
                    Target::Session { display, .. } => display.clone(),
                },
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let raw = parse_wmctrl_list(&String::from_utf8_lossy(&out.stdout));
        let active = self.active_window();

        let mut windows = Vec::new();
        let mut attached_sessions: BTreeSet<String> = BTreeSet::new();
        for w in raw {
            let c = classify_title(&w.title);
            if let Some(s) = &c.session {
                attached_sessions.insert(s.clone());
            }
            let focused = active
                .zip(parse_hex(&w.window_id))
                .is_some_and(|(a, id)| a == id);
            windows.push(WindowInfo {
                window_id: w.window_id,
                title: c.title,
                app_key: c.app_key,
                session: c.session,
                focused,
            });
        }

        // Detached tmux sessions (no live xterm): show them so the
        // operator can re-attach. Synthetic id → focus() spawns an xterm.
        for s in self.tmux_sessions() {
            if attached_sessions.contains(&s) {
                continue;
            }
            windows.push(WindowInfo {
                window_id: format!("{DETACHED_PREFIX}{s}"),
                title: format!("Terminal ({s}) — detached"),
                app_key: None,
                session: Some(s),
                focused: false,
            });
        }

        Ok(windows)
    }

    fn focus(&self, window_id: &str) -> Result<()> {
        // Detached tmux session → attach (spawn a fresh xterm).
        if let Some(session) = window_id.strip_prefix(DETACHED_PREFIX) {
            if !is_safe_session(session) {
                bail!("invalid session name");
            }
            let title = format!("roomler:tmux:{session}");
            self.spawn_detached(
                "xterm",
                &["-T", title.as_str(), "-e", "tmux", "attach", "-t", session],
                "xterm",
            )
            .with_context(|| format!("attaching to tmux session {session}"))?;
            return Ok(());
        }

        // Live window → raise. Guard the id shape so a malformed arg
        // can't be interpreted as a wmctrl flag.
        if parse_hex(window_id).is_none() {
            bail!("invalid window id");
        }
        let out = self.run_capture("wmctrl", &["-i", "-a", window_id], "wmctrl")?;
        if !out.status.success() {
            bail!(
                "wmctrl could not focus {window_id}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn launch(&self, app: &ResolvedApp) -> Result<LaunchOutcome> {
        if app.command.is_empty() {
            bail!("empty command");
        }

        if app.tmux {
            let existing = self.tmux_sessions();
            if existing.len() >= MAX_TMUX_SESSIONS {
                bail!("too many sessions ({MAX_TMUX_SESSIONS} max) — close some first");
            }
            let session = next_tmux_session_name(&existing);

            // Create the detached session running the configured shell.
            let mut new_args: Vec<&str> = vec!["new-session", "-d", "-s", session.as_str()];
            new_args.extend(app.command.iter().map(String::as_str));
            let created = self.run_capture("tmux", &new_args, "tmux")?;
            if !created.status.success() {
                bail!(
                    "tmux new-session failed: {}",
                    String::from_utf8_lossy(&created.stderr).trim()
                );
            }

            // Attach it in an xterm titled by our convention.
            let title = format!("roomler:tmux:{session}");
            self.spawn_detached(
                "xterm",
                &[
                    "-T",
                    title.as_str(),
                    "-e",
                    "tmux",
                    "attach",
                    "-t",
                    session.as_str(),
                ],
                "xterm",
            )?;

            std::thread::sleep(LAUNCH_SETTLE);
            return Ok(LaunchOutcome {
                window_id: self.window_id_by_title(&title),
                session: Some(session),
            });
        }

        if app.terminal {
            // TUI app in an xterm titled by our convention.
            let title = format!("roomler:app:{}", app.key);
            let mut args: Vec<&str> = vec!["-T", title.as_str(), "-e"];
            args.extend(app.command.iter().map(String::as_str));
            self.spawn_detached("xterm", &args, "xterm")?;
            std::thread::sleep(LAUNCH_SETTLE);
            return Ok(LaunchOutcome {
                window_id: self.window_id_by_title(&title),
                session: None,
            });
        }

        // GUI app: run the command directly; it sets its own window title.
        let (program, rest) = app.command.split_first().expect("non-empty checked above");
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        self.spawn_detached(program, &rest, program)?;
        Ok(LaunchOutcome::default())
    }
}

/// Parse an X11 window id (`0x03400007`) to a number for comparison.
fn parse_hex(id: &str) -> Option<u64> {
    let h = id.strip_prefix("0x").or_else(|| id.strip_prefix("0X"))?;
    if h.is_empty() || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(h, 16).ok()
}

/// tmux session names we generate are `s<N>`; a re-attach target must
/// look like a plain session token (defence-in-depth against a crafted
/// `window_id` reaching a shell-free `tmux attach -t`).
fn is_safe_session(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Work out how to reach a desktop on this host, or say why we cannot.
///
/// FR-56 P1. The old gate was `env::var_os("DISPLAY").is_some()` **on the
/// daemon**, which is only ever true in virtual-desktop mode — so on a Wayland
/// host Remote Apps did not fail, it never engaged.
///
/// Order is deliberate:
///
/// 1. **The daemon's own `DISPLAY`** wins. That is Xvfb mode, it is the only
///    population using this feature today, and it must stay byte-for-byte
///    unchanged — a "smarter" probe that reordered this would change behaviour
///    for the one set of hosts already relying on it.
/// 2. Otherwise, whoever is at the screen ([`crate::companion::graphical_session`],
///    already built for FR-27's consent prompt and FR-45's capture helper).
///    A Wayland session counts: mutter runs **Xwayland** even headless, so X11
///    tooling reaches its windows.
///
/// ⚠️ Candidates are **verified, not guessed**: each `DISPLAY`+cookie pair is
/// tried against `wmctrl -m` and the first that answers wins. `loginctl` does
/// not report a `Display=` for every Wayland session, so a guess of `:0` is
/// unavoidable — but an unverified guess would surface later as a confusing
/// "no windows" instead of an honest "no desktop".
pub fn discover() -> Option<Target> {
    if let Some(display) = std::env::var_os("DISPLAY").and_then(|d| d.into_string().ok()) {
        // Virtual-desktop mode. Unchanged, including running as the daemon:
        // the daemon started that Xvfb and owns it.
        return Some(Target::Daemon { display });
    }

    let sess = crate::companion::graphical_session().ok()?;
    let xauthority = find_xauthority(sess.uid);
    // loginctl reports `Display=` for X11 sessions and often not for Wayland
    // ones; `:0` is what a compositor-started Xwayland almost always takes.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(d) = sess.display.clone() {
        candidates.push(d);
    }
    for d in [":0", ":1"] {
        if !candidates.iter().any(|c| c == d) {
            candidates.push(d.to_string());
        }
    }

    for candidate in candidates {
        let target = Target::Session {
            display: candidate.clone(),
            xauthority: xauthority.clone(),
            user: sess.name.clone(),
        };
        match probe(&target) {
            Probe::Answered => {
                // ⚠️ NOT `%display`: tracing's `%x` shorthand expands through
                // its own `field::display` helper, so a local named `display`
                // makes the macro resolve the FUNCTION instead — and rustc
                // 1.95 ICEs while rendering that error rather than printing
                // it (`--message-format=short` shows it).
                tracing::info!(
                    display = %candidate,
                    user = %sess.name,
                    xauthority = ?xauthority,
                    "apps: found a usable desktop in the user's session"
                );
                return Some(target);
            }
            // wmctrl missing is not "no desktop" — it is a dependency the
            // existing error message already names actionably ("apt install
            // wmctrl"). Returning the target lets that message reach the
            // operator instead of a silent `supported:false`.
            Probe::ToolMissing => return Some(target),
            Probe::NoDisplay => continue,
        }
    }
    tracing::debug!(
        user = %sess.name,
        "apps: a graphical session exists but no X display answered — a Wayland \
         compositor with no Xwayland cannot be managed by the X11 backend"
    );
    None
}

/// Outcome of poking a candidate desktop.
enum Probe {
    /// An X server answered — this target is usable.
    Answered,
    /// `wmctrl` is not installed. Says nothing about the display.
    ToolMissing,
    /// `wmctrl` ran and could not open that display.
    NoDisplay,
}

/// Try `wmctrl -m` against a candidate. Doubles as the dependency check,
/// because `wmctrl` is what the whole backend runs on.
fn probe(target: &Target) -> Probe {
    let wm = LinuxWm::new(match target {
        Target::Daemon { display } => Target::Daemon {
            display: display.clone(),
        },
        Target::Session {
            display,
            xauthority,
            user,
        } => Target::Session {
            display: display.clone(),
            xauthority: xauthority.clone(),
            user: user.clone(),
        },
    });
    let Ok(mut cmd) = wm.cmd("wmctrl") else {
        return Probe::NoDisplay;
    };
    match cmd
        .arg("-m")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(st) if st.success() => Probe::Answered,
        Ok(_) => Probe::NoDisplay,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probe::ToolMissing,
        Err(_) => Probe::NoDisplay,
    }
}

/// Where a session's X cookie lives.
///
/// ⚠️ Measured, not assumed: with `DISPLAY` alone and no cookie every call
/// dies `Authorization required, but no authorization protocol specified` —
/// which is precisely what the pre-FR-56 code did, since `linux.rs` set
/// `DISPLAY` and nothing else. The compositor-generated name is a glob
/// (`.mutter-Xwaylandauth.XXXXXX`), so the directory is scanned rather than a
/// path guessed.
fn find_xauthority(uid: u32) -> Option<PathBuf> {
    let run = PathBuf::from(format!("/run/user/{uid}"));
    // Newest first: a compositor restart leaves the old cookie behind, and the
    // stale one authorises nothing.
    let mut mutter: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&run)
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".mutter-Xwaylandauth")
        })
        .filter_map(|e| {
            let m = e.metadata().ok()?.modified().ok()?;
            Some((m, e.path()))
        })
        .collect();
    mutter.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    if let Some((_, path)) = mutter.into_iter().next() {
        return Some(path);
    }
    // GDM's X11 sessions, then the classic home-directory cookie.
    [run.join("gdm/Xauthority"), run.join("Xauthority")]
        .into_iter()
        .find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_x11_ids() {
        assert_eq!(parse_hex("0x03400007"), Some(0x03400007));
        assert_eq!(parse_hex("0x3400007"), Some(0x3400007));
        assert!(parse_hex("tmux:main").is_none());
        assert!(parse_hex("0x").is_none());
        assert!(parse_hex("0xZZ").is_none());
        assert!(parse_hex("garbage").is_none());
    }

    #[test]
    fn safe_session_guard() {
        assert!(is_safe_session("s1"));
        assert!(is_safe_session("main"));
        assert!(is_safe_session("deploy-2"));
        assert!(!is_safe_session(""));
        assert!(!is_safe_session("a b"));
        assert!(!is_safe_session("a;rm -rf"));
        assert!(!is_safe_session(&"x".repeat(65)));
    }
}
