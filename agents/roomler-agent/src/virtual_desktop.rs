//! `--virtual-desktop` mode (Linux): bring up a headless X display (Xvfb)
//! plus a minimal window manager that the agent captures, turning a
//! Linux/WSL node into a browser-remotable desktop.
//!
//! WSLg's native display can't be screen-grabbed (rootless XWayland → the
//! X root has no readable framebuffer, `XGetImage` fails), so a dedicated
//! Xvfb is the capturable path — the same one `scripts/dev-xvfb.sh`
//! exercises. The agent's existing scrap (X11) capture + enigo input +
//! openh264/vp9 encode all work against it unchanged; this module is just
//! the orchestration.
//!
//! The code is cross-platform-compilable (`std::process` only) but only
//! wired in on Linux — the call site in `main::run_cmd` is
//! `#[cfg(target_os = "linux")]`.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const X11_UNIX_DIR: &str = "/tmp/.X11-unix";

/// What to bring up: `WxH` resolution, a WM binary, and optional startup apps.
#[derive(Debug, Clone)]
pub struct Config {
    pub resolution: String,
    pub wm: String,
    pub startup: Vec<String>,
}

/// A running virtual desktop. Its `Drop` kills Xvfb + the WM + apps, so the
/// caller keeps the handle alive for the agent's lifetime.
pub struct VirtualDesktop {
    display: String,
    children: Vec<Child>,
}

impl VirtualDesktop {
    /// The `:N` display the agent should capture (export as `DISPLAY`).
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for VirtualDesktop {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
        }
    }
}

/// Bring up Xvfb + WM + startup apps. Errors (with an actionable message)
/// if the required binaries are missing or Xvfb doesn't come up.
pub fn start(cfg: &Config) -> Result<VirtualDesktop> {
    preflight(cfg)?;
    let dpy = pick_display(X11_UNIX_DIR);
    let (w, h) = parse_resolution(&cfg.resolution);
    let mut children = Vec::new();

    let xvfb = Command::new("Xvfb")
        .args([
            dpy.as_str(),
            "-screen",
            "0",
            &format!("{w}x{h}x24"),
            "-ac",
            "-nolisten",
            "tcp",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn Xvfb")?;
    children.push(xvfb);

    if let Err(e) = wait_display_ready(&dpy, Duration::from_secs(15)) {
        // We haven't built `VirtualDesktop` yet, so Drop won't fire — kill
        // the Xvfb we just spawned before bailing.
        for c in &mut children {
            let _ = c.kill();
        }
        return Err(e);
    }
    info!(display = dpy.as_str(), resolution = %cfg.resolution, "virtual-desktop: Xvfb up");

    match Command::new(&cfg.wm)
        .env("DISPLAY", &dpy)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => children.push(c),
        Err(e) => {
            warn!(wm = %cfg.wm, %e, "virtual-desktop: window manager failed to start (continuing bare)")
        }
    }

    for app in &cfg.startup {
        match Command::new(app)
            .env("DISPLAY", &dpy)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => children.push(c),
            Err(e) => warn!(%app, %e, "virtual-desktop: startup app failed to launch"),
        }
    }
    info!(display = dpy.as_str(), wm = %cfg.wm, apps = cfg.startup.len(), "virtual-desktop ready");
    Ok(VirtualDesktop {
        display: dpy,
        children,
    })
}

/// Confirm the required binaries exist, else bail with the apt line.
fn preflight(cfg: &Config) -> Result<()> {
    let mut missing = Vec::new();
    if binary_on_path("Xvfb").is_none() {
        missing.push("xvfb".to_string());
    }
    if binary_on_path(&cfg.wm).is_none() {
        missing.push(cfg.wm.clone());
    }
    if !missing.is_empty() {
        bail!(
            "virtual-desktop: missing required binaries. Install them, e.g.:\n    sudo apt install {}",
            missing.join(" ")
        );
    }
    Ok(())
}

/// Is `bin` an executable on `$PATH`? Absolute/relative paths are checked
/// directly. Cross-platform (uses `PATH` + the platform separator).
fn binary_on_path(bin: &str) -> Option<std::path::PathBuf> {
    if bin.contains('/') || bin.contains('\\') {
        let p = std::path::PathBuf::from(bin);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let cand = dir.join(bin);
        cand.is_file().then_some(cand)
    })
}

/// Pick a free `:N` (`:99..=:120`) by requiring BOTH a missing X socket
/// (`<x11_dir>/X<N>`) AND a missing lock (`/tmp/.X<N>-lock`). Xvfb refuses a
/// display whose lock file exists even when the socket is gone — a crash-loop
/// leaves stale `/tmp/.X<N>-lock` behind, which is exactly the WSL failure
/// mode — so skip any display that still has one. Falls back to `:99`.
fn pick_display(x11_dir: &str) -> String {
    for n in 99..=120 {
        let socket = Path::new(x11_dir).join(format!("X{n}"));
        let lock = Path::new("/tmp").join(format!(".X{n}-lock"));
        if !socket.exists() && !lock.exists() {
            return format!(":{n}");
        }
    }
    ":99".to_string()
}

/// `"1920x1080"` → `(1920, 1080)`; anything unparseable → `(1920, 1080)`.
fn parse_resolution(s: &str) -> (u32, u32) {
    let mut it = s.split(['x', 'X']);
    match (
        it.next().and_then(|v| v.trim().parse::<u32>().ok()),
        it.next().and_then(|v| v.trim().parse::<u32>().ok()),
    ) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => {
            warn!(resolution = %s, "virtual-desktop: unparseable resolution — using 1920x1080");
            (1920, 1080)
        }
    }
}

/// Poll for the Xvfb X socket to appear, then a short settle.
fn wait_display_ready(display: &str, timeout: Duration) -> Result<()> {
    let n = display.trim_start_matches(':');
    let sock = Path::new(X11_UNIX_DIR).join(format!("X{n}"));
    let start = Instant::now();
    while start.elapsed() < timeout {
        if sock.exists() {
            std::thread::sleep(Duration::from_millis(300));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("virtual-desktop: Xvfb display {display} did not become ready within {timeout:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resolution_parses_and_falls_back() {
        assert_eq!(parse_resolution("1920x1080"), (1920, 1080));
        assert_eq!(parse_resolution("1280X720"), (1280, 720));
        assert_eq!(parse_resolution("garbage"), (1920, 1080));
        assert_eq!(parse_resolution("0x0"), (1920, 1080));
        assert_eq!(parse_resolution(""), (1920, 1080));
    }

    #[test]
    fn pick_display_returns_first_free_when_dir_absent() {
        // A dir that doesn't exist → no sockets → first candidate `:99`.
        assert_eq!(pick_display("/nonexistent/x11/dir/zzz"), ":99");
    }

    #[test]
    fn preflight_missing_binaries_names_apt_packages() {
        let cfg = Config {
            resolution: "1920x1080".into(),
            wm: "definitely-not-a-real-wm-binary-xyz".into(),
            startup: vec![],
        };
        let err = preflight(&cfg).unwrap_err().to_string();
        assert!(err.contains("apt install"), "err: {err}");
        assert!(
            err.contains("definitely-not-a-real-wm-binary-xyz"),
            "err: {err}"
        );
    }
}
