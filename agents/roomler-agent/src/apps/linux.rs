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

pub struct LinuxWm {
    display: String,
}

impl LinuxWm {
    pub fn new(display: String) -> Self {
        Self { display }
    }

    fn cmd(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("DISPLAY", &self.display);
        c
    }

    /// Run a helper and capture its output. A `NotFound` spawn error is
    /// rewritten into an actionable "install X" message.
    fn run_capture(&self, program: &str, args: &[&str], apt: &str) -> Result<Output> {
        self.cmd(program)
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
        self.cmd(program)
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
