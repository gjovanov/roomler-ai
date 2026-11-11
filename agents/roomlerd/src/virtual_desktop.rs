// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
//! The desktop is SUPERVISED (#3643): [`start`] hands it to a thread that
//! waits for the X server's exit, takes the rest of the tree down, and starts
//! the desktop again on the same display. While its server does not answer,
//! [`down`] says why, and the device stops offering the X11 arm. A desktop
//! that cannot start never stops the daemon, which is also this machine's
//! overlay node and SSH server.
//!
//! The module compiles everywhere, and everything that runs is Linux-only —
//! the call site in `main::run_cmd` is `#[cfg(target_os = "linux")]`.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const X11_UNIX_DIR: &str = "/tmp/.X11-unix";

/// Is this daemon configured to run its OWN virtual desktop
/// (`VIRTUAL_DESKTOP=1` or `true`, through `node_env`: the env var under any
/// of its names, then the config)?
///
/// ⚠️ ONE copy. `app.rs` (whether to start one), the X11 indicator (a
/// virtual desktop is no consent surface) and the recorder (FR-85: a virtual
/// desktop is no login screen) all ask this. Two hand-rolled copies once
/// disagreed: one skipped `node_env`'s config fallback, so a knob set through
/// `roomler config` was seen by one and not the other.
pub fn requested() -> bool {
    tunnel_core::env::node_env("VIRTUAL_DESKTOP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// How long the graceful teardown waits after SIGTERM before it SIGKILLs
/// whatever is left. Small on purpose: the virtual desktop's D-Bus tree —
/// `at-spi-bus-launcher` in particular — ignores SIGTERM, so the SIGKILL is
/// what actually reaps it, and every second here is a second the whole restart
/// (and every `systemctl stop`) is blocked. The point of doing this at all is
/// that systemd's own `TimeoutStopSec` for the same job is 90 s (#1684).
#[cfg(target_os = "linux")]
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// WSLg's PulseAudio server socket. When the agent runs inside WSL2 with
/// WSLg, the host's audio server is reachable here; exporting
/// `PULSE_SERVER` at it lets the virtual-desktop's apps (and thus the
/// `audio` feature's PulseAudio-monitor capture) actually route sound.
/// Without it, cpal's monitor source is silent under WSLg. On a native
/// Linux host this path doesn't exist and we leave `PULSE_SERVER`
/// untouched (the system default applies).
const WSLG_PULSE_SERVER: &str = "/mnt/wslg/PulseServer";

/// Add `PULSE_SERVER` pointing at the WSLg socket to `cmd`, but only when
/// that socket actually exists (i.e. we're under WSLg). No-op on native
/// Linux so the system default PulseAudio server is used.
fn with_wslg_pulse(cmd: &mut Command) {
    if Path::new(WSLG_PULSE_SERVER).exists() {
        cmd.env("PULSE_SERVER", WSLG_PULSE_SERVER);
    }
}

/// What to bring up: `WxH` resolution, a WM binary, and optional startup apps.
#[derive(Debug, Clone)]
pub struct Config {
    pub resolution: String,
    pub wm: String,
    pub startup: Vec<String>,
}

/// FR-108 P2 — the X server behind the desktop. Only Xorg with the dummy
/// driver can change its mode at runtime; Xvfb has one mode, its start size,
/// and `xrandr --newmode` there keeps nothing (spec §8 row 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    Xvfb,
    XorgDummy,
}

/// The real Xorg binary, never the console-only wrapper, which refuses an
/// unprivileged caller that is not on a VT.
const XORG_BINARIES: &[&str] = &["/usr/lib/xorg/Xorg", "/usr/libexec/Xorg", "/usr/lib/Xorg"];

/// `xserver-xorg-video-dummy`, wherever a distribution puts its modules.
const DUMMY_DRIVERS: &[&str] = &[
    "/usr/lib/xorg/modules/drivers/dummy_drv.so",
    "/usr/lib64/xorg/modules/drivers/dummy_drv.so",
    "/usr/lib/x86_64-linux-gnu/xorg/modules/drivers/dummy_drv.so",
    "/usr/lib/aarch64-linux-gnu/xorg/modules/drivers/dummy_drv.so",
];

/// The dummy driver's framebuffer, in KiB: room for an 8K desktop.
pub const XORG_VIDEO_RAM_KIB: u64 = 256_000;

/// FR-108 P2 — the kill switch: `VIRTUAL_DESKTOP_SERVER=xvfb` (through
/// `node_env`: the env var under any of its names, then the config) keeps the
/// desktop on Xvfb, as before P2, wherever the dummy driver is installed.
fn server_forced_xvfb(raw: Option<&str>) -> bool {
    raw.is_some_and(|v| v.trim().eq_ignore_ascii_case("xvfb"))
}

/// FR-108 P2 — Xorg with the dummy driver, when the real binary, the driver
/// and `xrandr` (which switches its modes) are all installed and the kill
/// switch is not set. The first file of each list that exists is used.
fn xorg_dummy() -> Option<std::path::PathBuf> {
    if server_forced_xvfb(tunnel_core::env::node_env("VIRTUAL_DESKTOP_SERVER").as_deref()) {
        return None;
    }
    pick_xorg_dummy(
        |p| Path::new(p).is_file(),
        binary_on_path("xrandr").is_some(),
    )
}

/// [`xorg_dummy`]'s decision, with the filesystem behind `exists` so it is
/// testable.
fn pick_xorg_dummy(exists: impl Fn(&str) -> bool, xrandr: bool) -> Option<std::path::PathBuf> {
    let xorg = XORG_BINARIES.iter().find(|p| exists(p))?;
    if !DUMMY_DRIVERS.iter().any(|p| exists(p)) || !xrandr {
        return None;
    }
    Some(std::path::PathBuf::from(xorg))
}

/// FR-108 P2 — the dummy driver's config. The start mode is the configured
/// resolution exactly, as a `roomler-WxH` modeline: the dummy driver's own
/// default would be 2048x1536. ⚠️ `ReducedBlanking` is not decoration: the
/// modelines carry reduced blanking, and without the option Xorg drops the
/// start mode ("monitor doesn't support reduced blanking") and starts at its
/// default, leaving nothing named to restore to. No input devices: input
/// arrives through XTest, as on Xvfb, and a desktop that adds the host's real
/// devices would take the person's keyboard.
fn xorg_conf(w: u32, h: u32) -> String {
    let (name, timings) = crate::display_match::modeline(w, h);
    format!(
        r#"Section "ServerFlags"
  Option "AutoAddDevices" "false"
  Option "AutoEnableDevices" "false"
  Option "DontVTSwitch" "true"
  Option "BlankTime" "0"
  Option "StandbyTime" "0"
  Option "SuspendTime" "0"
  Option "OffTime" "0"
EndSection
Section "Device"
  Identifier "roomler-dummy"
  Driver "dummy"
  VideoRam {XORG_VIDEO_RAM_KIB}
EndSection
Section "Monitor"
  Identifier "roomler-monitor"
  HorizSync 1.0 - 2000.0
  VertRefresh 1.0 - 200.0
  Option "ReducedBlanking" "true"
  Modeline "{name}" {timings}
EndSection
Section "Screen"
  Identifier "roomler-screen"
  Device "roomler-dummy"
  Monitor "roomler-monitor"
  DefaultDepth 24
  SubSection "Display"
    Depth 24
    Virtual 3840 2160
    Modes "{name}"
  EndSubSection
EndSection
"#
    )
}

/// FR-108 P2 — the argv for Xorg with the dummy driver, unprivileged. Its own
/// `-configdir` (empty), so no `xorg.conf.d` snippet of the host's (a GPU's,
/// an input device's) leaks into the desktop. Like Xvfb's: no TCP, no access
/// control on the loopback socket, and no screen saver. `-dpms` too, since
/// Xorg, unlike Xvfb, carries DPMS and would blank the screen.
fn xorg_args(display: &str, conf: &Path, configdir: &Path, log: &Path) -> Vec<String> {
    vec![
        display.to_string(),
        "-config".into(),
        conf.display().to_string(),
        "-configdir".into(),
        configdir.display().to_string(),
        "-logfile".into(),
        log.display().to_string(),
        "-noreset".into(),
        "-nolisten".into(),
        "tcp".into(),
        "-ac".into(),
        "-s".into(),
        "0".into(),
        "-dpms".into(),
        "-novtswitch".into(),
        "-sharevts".into(),
    ]
}

/// Where the desktop's Xorg keeps its config and log: private to the daemon.
fn xorg_dir(display: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("roomler-xorg-{}", display.trim_start_matches(':')))
}

impl Server {
    /// The server, as the log and a controller read it.
    fn name(self) -> &'static str {
        match self {
            Server::Xvfb => "Xvfb",
            Server::XorgDummy => "Xorg with the dummy driver",
        }
    }
}

/// This daemon's own virtual desktop, from [`start`] until [`teardown`]: its
/// display, what runs on it now, and whether its X server answers. `None` in
/// a process that started no desktop.
#[cfg(target_os = "linux")]
static DESKTOP: std::sync::Mutex<Option<std::sync::Arc<Desktop>>> = std::sync::Mutex::new(None);

#[cfg(target_os = "linux")]
fn desktop() -> Option<std::sync::Arc<Desktop>> {
    DESKTOP.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// FR-108 P2 — the display of this daemon's own virtual desktop when its
/// server can change mode (Xorg with the dummy driver) and answers now;
/// `None` otherwise, including on Xvfb, while the desktop is down, and when
/// no virtual desktop runs.
pub fn switchable_display() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let desk = desktop()?;
        let up = desk.state().up;
        (up == Some(Server::XorgDummy)).then(|| desk.display.clone())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// FR-97 P3e-2 — this daemon runs its own virtual desktop, on any server: its
/// `DISPLAY` is that desktop's, not a person's X11 display. True from
/// [`start`] on, and also while the desktop is down: the display stays its own.
pub fn active() -> bool {
    #[cfg(target_os = "linux")]
    {
        desktop().is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// #3643 — why this daemon's own virtual desktop shows nothing right now.
/// `Some` while its X server does not answer (it exited, or it never came
/// up), with the reason in words a controller can be shown; `None` while it
/// answers. ⚠️ `None` too in a process that started no desktop: a CLI or a
/// probe child run beside the daemon knows nothing of the daemon's desktop
/// and must not call it down.
pub fn down() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let desk = desktop()?;
        let st = desk.state();
        st.up.is_none().then(|| st.why_down.clone())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// [`down`] as the `detail` of a session opened meanwhile: what happened to
/// the desktop, and that the agent is already starting it again, so the
/// person reconnects instead of reporting a fault.
pub fn down_detail() -> Option<String> {
    down().map(|why| down_detail_of(&why))
}

fn down_detail_of(why: &str) -> String {
    format!(
        "this device's virtual desktop is down: {why}. The agent starts it again by itself; \
         reconnect in a moment"
    )
}

/// The handle [`start`] returns. Its `Drop` tears the whole process tree down
/// (see [`teardown`]), so the caller keeps it alive for the agent's lifetime.
#[cfg(target_os = "linux")]
pub struct VirtualDesktop {
    display: String,
}

#[cfg(target_os = "linux")]
impl VirtualDesktop {
    /// The `:N` display the agent should capture (export as `DISPLAY`). It is
    /// the desktop's for the daemon's whole life: a restarted X server comes
    /// back on the same one.
    pub fn display(&self) -> &str {
        &self.display
    }
}

#[cfg(target_os = "linux")]
impl Drop for VirtualDesktop {
    fn drop(&mut self) {
        // The desktop is a whole process tree that outlives its direct
        // children (a setsid'd `at-spi-bus-launcher` grandchild, D-Bus), so
        // killing our own children would leave the rest in the unit's cgroup
        // for systemd's `TimeoutStopSec` (90 s) to reap. `teardown` reaches
        // every descendant and is idempotent, so calling it here (as the RAII
        // backstop) after `run_cmd` already called it explicitly is a no-op
        // the second time.
        teardown();
    }
}

// ---------------------------------------------------------------------------
// Teardown — reach every descendant, not just our direct children (#1684)
// ---------------------------------------------------------------------------
//
// The failure this fixes: on a host running a virtual desktop, `roomler
// restart` took 96 s and the CLI reported failure at 60 s though the restart
// worked. The daemon runs under systemd `KillMode=control-group`,
// `TimeoutStopSec=90s`; once the daemon exits, systemd must empty the unit's
// cgroup, and the desktop's `at-spi-bus-launcher` — a setsid'd grandchild that
// ignores SIGTERM — kept the cgroup non-empty until systemd SIGKILLed it 90 s
// later, then paid `RestartSec` on top.
//
// So the daemon takes its own desktop down before it leaves. Two facts shape
// how:
//
//   * The children are spawned into a dedicated process GROUP ([`start`]), so
//     `killpg` reaches every one that stayed in it — but a grandchild that
//     called `setsid` (at-spi, a D-Bus service) has left the group.
//   * `setsid` changes the session and the group, never the PARENT, so a
//     descendant is still reachable by walking `/proc` parent links down from
//     our direct children. We snapshot `/proc` BEFORE signalling anything, so
//     the links are still intact (nothing has been reparented to init yet).
//
// The union of the two — group members and the parent-link closure of our
// children — is every process the daemon spawned for the desktop, and nothing
// else. SIGTERM, a short grace, then SIGKILL for the SIGTERM-deaf remainder.

/// The desktop's process group and the direct children we spawned, recorded
/// AS each one is spawned ([`Desktop::spawn_server`], [`Desktop::spawn_client`])
/// so that whoever takes the tree down — [`teardown`] on a graceful exit, the
/// supervisor after the X server died — reaches all of it, without the
/// [`VirtualDesktop`] handle.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct TreeHandle {
    /// The process group all our children were placed in (`None` if we could
    /// not create one — then only the parent-link closure is used).
    pgid: Option<i32>,
    /// The pids of the processes the daemon spawned directly (Xvfb, the WM,
    /// the startup apps).
    roots: Vec<i32>,
}

/// One process, as `/proc` reports it, for [`select_targets`] — and, through
/// `starttime`, an IDENTITY. A pid is recycled by the kernel once the process
/// that held it is reaped; a (pid, starttime) pair is not, within one boot. This
/// code SIGKILLs as root, so before it signals any process that is not its own
/// unreaped child it proves the pid still means what the snapshot saw
/// ([`same_process`]).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcInfo {
    pid: i32,
    ppid: i32,
    pgid: i32,
    /// Field 22 of `/proc/<pid>/stat`: when the process started, in clock
    /// ticks since boot. A recycled pid carries a later one.
    starttime: u64,
}

/// The pids to signal: everything in our process group, plus the transitive
/// parent-link closure of `roots` — the desktop's whole tree including
/// grandchildren that `setsid`'d out of the group. Never `self_pid` (the
/// daemon), pid 0, or pid 1, whatever the links say. Pure, so the selection is
/// unit-tested against synthetic `/proc` snapshots.
#[cfg(target_os = "linux")]
fn select_targets(
    snapshot: &[ProcInfo],
    roots: &[i32],
    pgid: Option<i32>,
    self_pid: i32,
) -> std::collections::BTreeSet<i32> {
    use std::collections::BTreeSet;
    let mut targets: BTreeSet<i32> = BTreeSet::new();
    // Group members.
    if let Some(g) = pgid.filter(|g| *g > 1) {
        for p in snapshot.iter().filter(|p| p.pgid == g) {
            targets.insert(p.pid);
        }
    }
    // The direct children we hold, even if `/proc` no longer lists them.
    for &r in roots {
        if r > 1 {
            targets.insert(r);
        }
    }
    // Parent-link closure: repeatedly pull in any process whose parent is
    // already a target. Bounded by the snapshot size — each pass adds at least
    // one pid or we stop.
    loop {
        let mut grew = false;
        for p in snapshot {
            if p.pid > 1 && !targets.contains(&p.pid) && targets.contains(&p.ppid) {
                targets.insert(p.pid);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    // Never the daemon itself, and never pid 0/1 — a stray link to them is a
    // reparenting we must not follow into.
    targets.remove(&self_pid);
    targets.remove(&0);
    targets.remove(&1);
    targets
}

/// Read `/proc` into [`ProcInfo`]s. Linux-only.
#[cfg(target_os = "linux")]
fn read_proc_snapshot() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if let Some(info) = read_proc_stat(pid) {
            out.push(info);
        }
    }
    out
}

/// The fields this module reads from one `/proc/<pid>/stat` line: state (f3),
/// ppid (f4), pgrp (f5) and starttime (f22). The comm field (f2) is
/// parenthesised and may itself contain spaces and `)`, so split on the LAST
/// `)` and count space-separated fields from there — field `k` sits at index
/// `k - 3`. `None` if any of the four is missing or unparseable.
#[cfg(target_os = "linux")]
fn parse_stat(raw: &str) -> Option<(&str, i32, i32, u64)> {
    let rparen = raw.rfind(')')?;
    let fields: Vec<&str> = raw[rparen + 1..].split_whitespace().collect();
    let state = fields.first()?;
    let ppid = fields.get(1)?.parse::<i32>().ok()?;
    let pgid = fields.get(2)?.parse::<i32>().ok()?;
    let starttime = fields.get(19)?.parse::<u64>().ok()?;
    Some((state, ppid, pgid, starttime))
}

/// Read `/proc/<pid>/stat` into a [`ProcInfo`]. Linux-only.
#[cfg(target_os = "linux")]
fn read_proc_stat(pid: i32) -> Option<ProcInfo> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_state, ppid, pgid, starttime) = parse_stat(&raw)?;
    Some(ProcInfo {
        pid,
        ppid,
        pgid,
        starttime,
    })
}

/// Is the process that `/proc/<pid>/stat` NOW describes (`current_stat`) the
/// same one `snapshot` recorded? Equal starttimes say yes. Pure; `false` for
/// anything unparseable — when in doubt, do not signal.
#[cfg(target_os = "linux")]
fn same_process(snapshot: &ProcInfo, current_stat: &str) -> bool {
    matches!(parse_stat(current_stat), Some((_, _, _, st)) if st == snapshot.starttime)
}

/// Is `pid` a live process (not gone, not a zombie/dead)? A zombie still
/// answers `kill(pid, 0)` until it is reaped, so liveness reads the `/proc`
/// state rather than signalling.
#[cfg(target_os = "linux")]
fn is_running(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(raw) => match raw.rfind(')') {
            Some(rp) => !matches!(
                raw[rp + 1..].split_whitespace().next(),
                Some("Z") | Some("X") | Some("x") | None
            ),
            None => false,
        },
        Err(_) => false,
    }
}

/// SIGTERM the desktop tree, wait a bounded grace, SIGKILL the remainder, and
/// reap our direct children; the supervisor starts nothing after it. Idempotent:
/// takes the desktop, so a second call finds nothing and returns at once. Safe
/// to call from every graceful exit path and from `Drop`.
///
/// No-op on non-Linux (the desktop is Linux-only) and no-op when no desktop was
/// started.
pub fn teardown() {
    #[cfg(target_os = "linux")]
    {
        let desk = DESKTOP.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(desk) = desk {
            desk.stop();
        }
    }
}

/// The process operations [`kill_tree_with`] performs, behind a seam so that
/// the ORDER they happen in — every signal before any reap, every non-root
/// signal behind an identity check — is provable in a unit test without a
/// process tree. [`RealProcs`] is `/proc` + libc; the tests script one.
#[cfg(target_os = "linux")]
trait Procs {
    /// Every process `/proc` lists right now.
    fn snapshot(&mut self) -> Vec<ProcInfo>;
    /// `pid` is a live (not gone, not zombie) process.
    fn is_running(&mut self, pid: i32) -> bool;
    /// `info.pid` is still the process `info` recorded — same starttime
    /// ([`same_process`]). `false` if it is gone, recycled or unreadable.
    fn same_process_now(&mut self, info: &ProcInfo) -> bool;
    fn signal(&mut self, pid: i32, sig: i32);
    fn killpg(&mut self, pgid: i32, sig: i32);
    /// `waitpid(WNOHANG)` each root. `true` once none of them is still an
    /// unreaped child of ours.
    fn reap(&mut self, roots: &[i32]) -> bool;
    fn sleep(&mut self, d: Duration);
}

/// The real thing. Linux-only.
#[cfg(target_os = "linux")]
struct RealProcs;

#[cfg(target_os = "linux")]
impl Procs for RealProcs {
    fn snapshot(&mut self) -> Vec<ProcInfo> {
        read_proc_snapshot()
    }
    fn is_running(&mut self, pid: i32) -> bool {
        is_running(pid)
    }
    fn same_process_now(&mut self, info: &ProcInfo) -> bool {
        std::fs::read_to_string(format!("/proc/{}/stat", info.pid))
            .is_ok_and(|raw| same_process(info, &raw))
    }
    fn signal(&mut self, pid: i32, sig: i32) {
        if pid > 1 {
            // SAFETY: a plain `kill(2)`. `kill_tree_with` calls this only for
            // (a) one of OUR direct children, which stays an unreaped zombie
            // until the final `reap`, so its pid is reserved by the kernel and
            // cannot have been recycled, or (b) a pid whose `/proc` starttime
            // was re-read and matched the snapshot an instant ago
            // (`same_process_now`). A failure (ESRCH: gone in between) is
            // exactly what we want and not actionable.
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }
    fn killpg(&mut self, pgid: i32, sig: i32) {
        if pgid > 1 {
            // SAFETY: `killpg(2)` on the group WE created in `start`. Its id is
            // Xvfb's pid — one of our roots — which stays reserved (an
            // unreaped zombie at worst) until the final `reap`, so the group id
            // cannot have been handed to an unrelated group in the meantime.
            unsafe {
                libc::killpg(pgid, sig);
            }
        }
    }
    fn reap(&mut self, roots: &[i32]) -> bool {
        let mut all_done = true;
        for &pid in roots {
            if pid > 1 {
                let mut status: libc::c_int = 0;
                // SAFETY: `waitpid` on our own child with `WNOHANG`. Returns the
                // pid once reaped, 0 while it still runs, -1/ECHILD if it is not
                // (or no longer) our child — the last two need no action.
                let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if r == 0 {
                    all_done = false;
                }
            }
        }
        all_done
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The teardown itself, separated from the global so a test can drive it
/// against a tree it built. Linux-only.
#[cfg(target_os = "linux")]
fn kill_tree(roots: &[i32], pgid: Option<i32>, grace: Duration) {
    kill_tree_with(&mut RealProcs, roots, pgid, grace);
}

/// SIGTERM the tree, wait `grace`, SIGKILL what is left, wait for the kills to
/// land, and only THEN reap our children. Two rules keep a root-privileged
/// SIGKILL off a recycled pid, and both are load-bearing:
///
/// 1. **Nothing is reaped until every signal has been sent.** An unreaped zombie
///    keeps its pid reserved — and, for the group leader (Xvfb), the desktop's
///    pgid with it — so `kill(root)` and both `killpg(pgid)` calls cannot land
///    on an id the kernel has handed to someone else. Reaping inside the grace
///    loop (the first cut of this code) freed exactly those numbers
///    mid-teardown. [`is_running`] already reads a zombie as not running, so the
///    grace and settle checks need no reaping to terminate.
/// 2. **A pid that is not one of our children is signalled only after its
///    `/proc` starttime is re-read and matches the snapshot that selected it.**
///    Grandchildren are reaped by their parent or init the moment they exit, so
///    their numbers CAN be recycled during the grace; the (pid, starttime) pair
///    cannot. Each pass checks against its own snapshot — never a fresher one,
///    which might already describe the recycled process.
#[cfg(target_os = "linux")]
fn kill_tree_with(p: &mut dyn Procs, roots: &[i32], pgid: Option<i32>, grace: Duration) {
    use std::collections::{BTreeSet, HashMap};
    let self_pid = std::process::id() as i32;
    let is_root = |pid: i32| roots.contains(&pid);
    let group = pgid.filter(|g| *g > 1);

    // Pass 1 — SIGTERM. Snapshot BEFORE any signal, so the parent links are
    // still intact (nothing has been reparented to init yet).
    let snap = p.snapshot();
    let targets = select_targets(&snap, roots, pgid, self_pid);
    if targets.is_empty() {
        return;
    }
    let ident: HashMap<i32, ProcInfo> = snap.iter().map(|i| (i.pid, *i)).collect();
    info!(
        count = targets.len(),
        ?pgid,
        "virtual-desktop: tearing down the desktop process tree (SIGTERM)"
    );
    for &pid in &targets {
        if is_root(pid) {
            p.signal(pid, libc::SIGTERM);
        } else if let Some(info) = ident.get(&pid)
            && p.same_process_now(info)
        {
            p.signal(pid, libc::SIGTERM);
        }
    }
    // The group as a whole, for anything that appeared after the snapshot but
    // stayed in the group.
    if let Some(g) = group {
        p.killpg(g, libc::SIGTERM);
    }

    // Grace: poll until nothing is running, or the deadline hits. No reaping
    // here — see rule 1.
    let deadline = Instant::now() + grace;
    loop {
        if !targets.iter().any(|&t| p.is_running(t)) {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        p.sleep(Duration::from_millis(50));
    }

    // Pass 2 — SIGKILL whatever ignored SIGTERM (at-spi does). A fresh snapshot
    // catches anything that forked during the grace; each pid's identity is
    // checked against the snapshot that SELECTED it (rule 2).
    let snap2 = p.snapshot();
    let survivors = select_targets(&snap2, roots, pgid, self_pid);
    let ident2: HashMap<i32, ProcInfo> = snap2.iter().map(|i| (i.pid, *i)).collect();
    let kill_set: BTreeSet<i32> = targets.union(&survivors).copied().collect();
    let mut killed = 0usize;
    for &pid in &kill_set {
        if !p.is_running(pid) {
            continue;
        }
        let ours = if is_root(pid) {
            true
        } else if survivors.contains(&pid) {
            ident2.get(&pid).is_some_and(|i| p.same_process_now(i))
        } else {
            ident.get(&pid).is_some_and(|i| p.same_process_now(i))
        };
        if ours {
            p.signal(pid, libc::SIGKILL);
            killed += 1;
        }
    }
    if let Some(g) = group {
        p.killpg(g, libc::SIGKILL);
    }

    // Settle: wait for the SIGKILLs to actually take effect. A process the
    // kernel has been told to SIGKILL lingers a few ms in `/proc` as R/S before
    // it is torn down, and the whole point is that when the daemon exits,
    // systemd finds the cgroup EMPTY. Bounded. Still no reaping.
    let settle = Instant::now() + Duration::from_secs(1);
    while Instant::now() < settle {
        if !kill_set.iter().any(|&t| p.is_running(t)) {
            break;
        }
        p.sleep(Duration::from_millis(20));
    }
    if killed > 0 {
        info!(
            killed,
            "virtual-desktop: SIGKILLed processes that ignored SIGTERM"
        );
    }

    // Reap LAST — and only now (rule 1). Bounded, so the unit's cgroup does not
    // keep our zombies: they were SIGKILLed above and exit within milliseconds.
    let reap_by = Instant::now() + Duration::from_secs(1);
    while !p.reap(roots) && Instant::now() < reap_by {
        p.sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// The desktop, supervised (#3643)
// ---------------------------------------------------------------------------
//
// The failure this fixes: on a host with NVIDIA's userspace driver the X
// server aborted ten seconds after its start. The daemon had already logged
// the desktop as up (its socket existed), watched nothing afterwards, and went
// on offering a desktop for as long as it ran: every session connected and
// showed nothing, and the server and the window manager sat under it as
// zombies.
//
// Four things changed, and each one alone leaves the hole open:
//
//   * The server never loads a hardware vendor's EGL ([`egl_vendor_pin`]),
//     which is what aborted it.
//   * "Up" means the server ANSWERED an X11 connection setup
//     ([`wait_display_ready`]). Its socket is made before it initialises its
//     extensions, which is where that one died.
//   * A thread waits for the server's exit ([`supervise`]) WITHOUT reaping it,
//     so the teardown's rule 1 still holds: the dead server keeps its pid, and
//     the desktop's group id with it, until every signal has been sent. Then
//     the rest of the tree comes down and the desktop is started again on the
//     SAME display: `DISPLAY` is exported once, and a process cannot safely
//     change its environment under its own threads.
//   * Nothing here can stop the daemon. A desktop that does not start is
//     logged, reported by [`down`], and tried again. The daemon is also this
//     machine's overlay node and SSH server, and losing those to a broken X
//     server would cost the operator the access they need to repair it.

/// How long a start waits for the X server to answer.
#[cfg(target_os = "linux")]
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one connection waits for the server's answer before the wait
/// looks at the server process again.
#[cfg(target_os = "linux")]
const READY_PROBE: Duration = Duration::from_secs(2);

/// The wait before the desktop is started again: this after the first exit,
/// doubled by every start that fails or does not last…
#[cfg(target_os = "linux")]
const RESTART_DELAY_FLOOR: Duration = Duration::from_secs(1);

/// …up to this. A desktop that cannot start is tried twelve times an hour: a
/// host that keeps failing (a missing package, a server that aborts) costs a
/// log line every five minutes, and heals by itself once the cause is gone.
#[cfg(target_os = "linux")]
const RESTART_DELAY_CEIL: Duration = Duration::from_secs(300);

/// A server that answered for this long had a start that worked, so its exit
/// begins a new run of delays. One that dies sooner keeps the run going: the
/// field failure died within eleven seconds of every start.
#[cfg(target_os = "linux")]
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// How a desktop is started and how patiently it is started again. A struct
/// so the tests can put a stand-in where Xvfb is and count in milliseconds;
/// [`Plan::of`] is what the daemon runs.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct Plan {
    cfg: Config,
    /// The Xvfb program, found on `PATH`.
    xvfb: String,
    /// Try Xorg with the dummy driver first, where it is installed (FR-108 P2).
    xorg: bool,
    ready_timeout: Duration,
    /// How long a log line waits for a dead server's stderr to drain.
    tail_wait: Duration,
    delay_floor: Duration,
    delay_ceil: Duration,
    stable_after: Duration,
}

#[cfg(target_os = "linux")]
impl Plan {
    fn of(cfg: &Config) -> Self {
        Self {
            cfg: cfg.clone(),
            xvfb: "Xvfb".into(),
            xorg: true,
            ready_timeout: READY_TIMEOUT,
            tail_wait: TAIL_WAIT,
            delay_floor: RESTART_DELAY_FLOOR,
            delay_ceil: RESTART_DELAY_CEIL,
            stable_after: STABLE_AFTER,
        }
    }
}

/// Why a start gave no desktop.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct NoDesktop {
    /// What happened, for the log and for [`down`]. Never the server's own
    /// output: a controller's screen is no place for a backtrace, and an Xorg
    /// starts its stderr with a banner.
    why: String,
    /// The server's last lines of stderr, for the log.
    said: String,
    /// The server stayed alive and silent until the timeout.
    hung: bool,
}

#[cfg(target_os = "linux")]
impl NoDesktop {
    fn because(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            said: String::new(),
            hung: false,
        }
    }
}

/// How [`wait_display_ready`] ended without an answer.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotReady {
    /// The server exited.
    Exited,
    /// It is alive, and the timeout passed.
    Silent,
}

/// What a [`Desktop`] is doing now. One lock covers all of it, so "the daemon
/// is stopping" and "this child was spawned and recorded" can never interleave
/// ([`Desktop::spawn_server`]).
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct State {
    /// The daemon is stopping: nothing more is started.
    stopping: bool,
    /// What is spawned right now, for whoever takes it down.
    tree: Option<TreeHandle>,
    /// The server answering on the display now; `None` while the desktop is
    /// down.
    up: Option<Server>,
    /// Why it is down, for [`down`].
    why_down: String,
    /// Children a kill could not collect in its bound (SIGKILLed, and still
    /// stuck in the kernel). Asked for again at the next kill; never
    /// signalled again.
    unreaped: Vec<i32>,
}

/// One virtual desktop: a display that is its own for the daemon's life, and
/// whatever runs on it now.
#[cfg(target_os = "linux")]
struct Desktop {
    display: String,
    plan: Plan,
    st: std::sync::Mutex<State>,
    /// Held across every kill of the tree ([`Desktop::kill_recorded_tree`]).
    /// A graceful exit that arrives while the supervisor is clearing up after
    /// a dead server waits here for it, instead of finding no tree recorded
    /// and letting the daemon leave with half of one still running (#1684).
    /// Taken BEFORE `st`, never after.
    op: std::sync::Mutex<()>,
    /// Wakes the supervisor out of a delay when the daemon stops.
    wake: std::sync::Condvar,
    /// Xorg with the dummy driver did not come up once in this daemon's life,
    /// so later starts go straight to Xvfb: a failing Xorg costs only the
    /// mode switch, and costs its wait only once.
    xorg_failed: std::sync::atomic::AtomicBool,
    /// This desktop made Xorg's private directory, so its stop removes it.
    xorg_dir_made: std::sync::atomic::AtomicBool,
}

/// An X server this daemon started, while it answers.
#[cfg(target_os = "linux")]
struct XServer {
    pid: i32,
    kind: Server,
    tail: StderrTail,
    since: Instant,
}

#[cfg(target_os = "linux")]
impl Desktop {
    fn new(display: String, plan: Plan) -> Self {
        Self {
            display,
            plan,
            st: std::sync::Mutex::new(State {
                why_down: "it has not started yet".into(),
                ..State::default()
            }),
            op: std::sync::Mutex::new(()),
            wake: std::sync::Condvar::new(),
            xorg_failed: std::sync::atomic::AtomicBool::new(false),
            xorg_dir_made: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stopping(&self) -> bool {
        self.state().stopping
    }

    /// Record that the desktop is down, and why. `false` when the daemon is
    /// stopping instead: then the tree is `stop`'s, and the caller leaves.
    fn set_down(&self, why: String) -> bool {
        let mut st = self.state();
        if st.stopping {
            return false;
        }
        st.up = None;
        st.why_down = why;
        true
    }

    /// Spawn the X server at the head of a new process group and record it as
    /// the tree, unless the daemon is stopping (`Ok(None)`).
    ///
    /// ⚠️ The spawn and the record sit under ONE lock, the one `stop` sets
    /// `stopping` under. So a child either exists before the stop looks, and
    /// is in the tree the stop kills, or is never spawned. Recording after the
    /// lock is released would let a stop slip between the two and leave a
    /// server running under a daemon that believes its desktop is gone.
    //
    // The child is reaped by `kill_tree` (libc `waitpid`), which the lint
    // cannot see.
    #[allow(clippy::zombie_processes)]
    fn spawn_server(&self, cmd: &mut Command) -> std::io::Result<Option<Child>> {
        lead_new_group(cmd);
        let mut st = self.state();
        if st.stopping {
            return Ok(None);
        }
        let child = cmd.spawn()?;
        // The desktop's group id is the X server's pid (a group leader's pgid
        // == its pid).
        st.tree = Some(TreeHandle {
            pgid: group_of(&child),
            roots: vec![child.id() as i32],
        });
        Ok(Some(child))
    }

    /// Spawn the window manager or a startup app INTO the desktop's group and
    /// record it, under the same one lock as [`Self::spawn_server`]. `Ok(false)`
    /// when the daemon is stopping, or the tree is already gone.
    #[allow(clippy::zombie_processes)] // reaped by `kill_tree`, as the server is
    fn spawn_client(&self, cmd: &mut Command) -> std::io::Result<bool> {
        let mut st = self.state();
        if st.stopping {
            return Ok(false);
        }
        let Some(tree) = st.tree.as_mut() else {
            return Ok(false);
        };
        join_group(cmd, tree.pgid);
        let child = cmd.spawn()?;
        tree.roots.push(child.id() as i32);
        Ok(true)
    }

    /// Take the recorded tree and bring it down: SIGTERM, the grace, SIGKILL,
    /// and only then the reap ([`kill_tree_with`]'s two rules). Whoever comes
    /// second waits for the first and finds nothing recorded.
    fn kill_recorded_tree(&self) {
        let _op = self.op.lock().unwrap_or_else(|e| e.into_inner());
        let (handle, mut waiting) = {
            let mut st = self.state();
            (st.tree.take(), std::mem::take(&mut st.unreaped))
        };
        if let Some(handle) = handle {
            kill_tree(&handle.roots, handle.pgid, TEARDOWN_GRACE);
            waiting.extend(handle.roots);
        }
        // `kill_tree` reaps within a bound. A child it could not collect in
        // time would stay a zombie for the daemon's life, now that the daemon
        // lives on after a kill: nothing else ever waits for it. Keep it, and
        // ask again at the next kill.
        waiting.retain(|pid| !RealProcs.reap(&[*pid]));
        self.state().unreaped = waiting;
    }

    /// The graceful stop: nothing is started from here on, the supervisor is
    /// woken to leave, and the tree comes down.
    fn stop(&self) {
        {
            let mut st = self.state();
            st.stopping = true;
            st.up = None;
            st.why_down = "the daemon is stopping".into();
        }
        self.wake.notify_all();
        self.kill_recorded_tree();
        // FR-108 P2 — the desktop is gone, so nothing is left to switch.
        if self
            .xorg_dir_made
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let _ = std::fs::remove_dir_all(xorg_dir(&self.display));
        }
    }

    /// Wait `d`, or less when the daemon stops. `true` when it is stopping.
    fn wait_unless_stopping(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut st = self.state();
        loop {
            if st.stopping {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .wake
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Start the X server on this desktop's display and, once it answers, the
    /// window manager and the startup apps. `Err` carries why not, and leaves
    /// nothing running.
    fn bring_up(&self) -> std::result::Result<XServer, NoDesktop> {
        let cfg = &self.plan.cfg;
        // An Xorg that hung once is no longer the server that will serve.
        let try_xorg =
            self.plan.xorg && !self.xorg_failed.load(std::sync::atomic::Ordering::Relaxed);
        preflight(cfg, &self.plan.xvfb, try_xorg)
            .map_err(|e| NoDesktop::because(format!("{e:#}")))?;
        let (w, h) = parse_resolution(&cfg.resolution);
        let x = self.start_server(try_xorg, w, h)?;
        info!(display = self.display.as_str(), resolution = %cfg.resolution, server = ?x.kind, "virtual-desktop: X server up");
        self.start_clients();
        let pgid = {
            let mut st = self.state();
            if st.stopping {
                return Err(NoDesktop::because("the daemon is stopping"));
            }
            st.up = Some(x.kind);
            st.why_down.clear();
            st.tree.as_ref().and_then(|t| t.pgid)
        };
        info!(display = self.display.as_str(), wm = %cfg.wm, apps = cfg.startup.len(), ?pgid, "virtual-desktop ready");
        Ok(x)
    }

    /// The X server: Xorg with the dummy driver where it is installed, so
    /// display match can switch its mode (FR-108 P2); Xvfb otherwise, and also
    /// when Xorg does not come up, so a failing Xorg costs only the switch,
    /// never the desktop. Both on the SAME display: a server of ours that did
    /// not come up is killed, reaped, and its lock cleared before the next.
    fn start_server(
        &self,
        try_xorg: bool,
        w: u32,
        h: u32,
    ) -> std::result::Result<XServer, NoDesktop> {
        use std::sync::atomic::Ordering::Relaxed;
        if try_xorg && let Some(xorg) = xorg_dummy() {
            let started = match xorg_command(&xorg, &self.display, w, h) {
                Ok(cmd) => {
                    self.xorg_dir_made.store(true, Relaxed);
                    self.start_one(Server::XorgDummy, cmd)
                }
                Err(e) => Err(NoDesktop::because(format!("{e:#}"))),
            };
            match started {
                Ok(x) => return Ok(x),
                Err(no) if self.stopping() => {
                    // Made after `stop` looked for it, so `stop` left it.
                    if self.xorg_dir_made.load(Relaxed) {
                        let _ = std::fs::remove_dir_all(xorg_dir(&self.display));
                    }
                    return Err(no);
                }
                Err(no) => {
                    // Only an Xorg that HUNG is given up for the daemon's life:
                    // its wait is what every later start would pay. One that
                    // exits at once costs nothing to try again, and its cause
                    // may pass (another server held the display for a moment).
                    if no.hung {
                        self.xorg_failed.store(true, Relaxed);
                    }
                    warn!(why = %no.why, said = %no.said, log = %xorg_dir(&self.display).join("Xorg.log").display(), "virtual-desktop: Xorg with the dummy driver did not come up — using Xvfb, so display match answers unsupported");
                }
            }
        }
        self.start_one(
            Server::Xvfb,
            xvfb_command(&self.plan.xvfb, &self.display, w, h),
        )
    }

    /// Spawn one X server and wait until it answers. One that does not is
    /// taken down again — it may be alive and stuck — and what it wrote to
    /// stderr comes back with the error, for the log.
    fn start_one(&self, kind: Server, mut cmd: Command) -> std::result::Result<XServer, NoDesktop> {
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        pin_software_egl(&mut cmd);
        let mut child = match self.spawn_server(&mut cmd) {
            Ok(Some(child)) => child,
            Ok(None) => return Err(NoDesktop::because("the daemon is stopping")),
            Err(e) => {
                return Err(NoDesktop::because(format!(
                    "could not start {}: {e}",
                    kind.name()
                )));
            }
        };
        let pid = child.id() as i32;
        let tail = StderrTail::follow(child.stderr.take());
        let (sock, lock) = x_files(&self.display);
        match wait_display_ready(&sock, &lock, pid, self.plan.ready_timeout) {
            Ok(()) => Ok(XServer {
                pid,
                kind,
                tail,
                since: Instant::now(),
            }),
            Err(not_ready) => {
                self.kill_recorded_tree();
                remove_stale_lock(&lock, pid);
                let why = match not_ready {
                    NotReady::Exited => format!(
                        "{} exited while it was starting on display {}",
                        kind.name(),
                        self.display
                    ),
                    NotReady::Silent => format!(
                        "{} did not answer on display {} within {:?}",
                        kind.name(),
                        self.display,
                        self.plan.ready_timeout
                    ),
                };
                Err(NoDesktop {
                    why,
                    // After the kill, so the pipe has closed and the tail is
                    // whole.
                    said: tail.last_lines(self.plan.tail_wait),
                    hung: not_ready == NotReady::Silent,
                })
            }
        }
    }

    /// The window manager and the startup apps, on a server that answers.
    fn start_clients(&self) {
        let cfg = &self.plan.cfg;
        let mut wm_cmd = Command::new(&cfg.wm);
        wm_cmd
            .env("DISPLAY", &self.display)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        with_wslg_pulse(&mut wm_cmd);
        if let Err(e) = self.spawn_client(&mut wm_cmd) {
            warn!(wm = %cfg.wm, %e, "virtual-desktop: window manager failed to start (continuing bare)");
        }
        for app in &cfg.startup {
            let mut app_cmd = Command::new(app);
            app_cmd
                .env("DISPLAY", &self.display)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            with_wslg_pulse(&mut app_cmd);
            if let Err(e) = self.spawn_client(&mut app_cmd) {
                warn!(%app, %e, "virtual-desktop: startup app failed to launch");
            }
        }
        // An app that maps while openbox is still starting can stay unmanaged,
        // and so invisible, until X traffic next wakes it (`WM_NUDGE_AT_MS`).
        if !cfg.startup.is_empty() {
            nudge_wm_after_startup(&self.display);
        }
    }
}

/// Bring the virtual desktop up — the X server, the window manager, the
/// startup apps — and hand it to its supervisor. The first start is made here,
/// before this returns, so the caps probe and the first capture find the
/// display answering.
///
/// ⚠️ It cannot fail, on purpose (#3643). Missing binaries or a server that
/// does not come up leave the daemon running WITHOUT a desktop: the reason is
/// logged and in [`down`], and the supervisor keeps trying, so installing the
/// missing package is enough and needs no restart. Returning the error — as
/// this did until 0.4.209 — ended the whole daemon, and took a headless
/// machine's overlay and SSH down because its X server was broken. What it
/// can still cost the daemon is time: the first start is waited for, for
/// [`READY_TIMEOUT`] a server at most, when a server hangs without exiting.
#[cfg(target_os = "linux")]
pub fn start(cfg: &Config) -> VirtualDesktop {
    let desk = std::sync::Arc::new(Desktop::new(pick_display(X11_UNIX_DIR), Plan::of(cfg)));
    // One desktop per daemon, so a plain overwrite is right.
    *DESKTOP.lock().unwrap_or_else(|e| e.into_inner()) = Some(desk.clone());
    let first = match desk.bring_up() {
        Ok(x) => Some(x),
        Err(no) => {
            warn!(display = desk.display.as_str(), why = %no.why, said = %no.said, "virtual-desktop: the desktop did not start — this device stays online without one, and the daemon keeps trying");
            desk.set_down(no.why);
            None
        }
    };
    let display = desk.display.clone();
    let spawned = std::thread::Builder::new()
        .name("vd-supervisor".into())
        .spawn(move || supervise(&desk, first));
    if let Err(e) = spawned {
        warn!(%e, "virtual-desktop: no supervisor thread — an X server that exits is not started again");
    }
    VirtualDesktop { display }
}

/// Keep the desktop up for the daemon's life: wait for the X server's exit,
/// stop offering the desktop, take the rest of the tree down, and start it
/// again after a delay that grows while the starts keep failing. Returns when
/// the daemon stops.
///
/// The order after an exit is the point. The desktop is marked down FIRST, so
/// nothing is offered that is not there. The tree is killed before the lock is
/// cleared, so the dead server is reaped (it was only waited for, never
/// collected). And the delay comes before the start, so a server that dies at
/// once cannot spin.
#[cfg(target_os = "linux")]
fn supervise(desk: &Desktop, first: Option<XServer>) {
    let mut running = first;
    let mut delays = Backoff::new(desk.plan.delay_floor, desk.plan.delay_ceil);
    loop {
        if let Some(x) = running.take() {
            let exit = wait_exit(x.pid);
            let up_for = x.since.elapsed();
            if !desk.set_down(format!("its X server ({}) exited: {exit}", x.kind.name())) {
                // A deliberate stop killed it; the tree is `stop`'s.
                return;
            }
            crate::desktop_brief::refresh_soon();
            // The window manager and the apps lost their display. Bring them
            // down so the next start is clean and none of them stays a zombie;
            // this is also what reaps the server.
            desk.kill_recorded_tree();
            remove_stale_lock(&x_files(&desk.display).1, x.pid);
            warn!(
                display = desk.display.as_str(),
                server = x.kind.name(),
                %exit,
                up_for_s = up_for.as_secs(),
                said = %x.tail.last_lines(desk.plan.tail_wait),
                "virtual-desktop: the X server exited — the desktop is down until it is started again"
            );
            if up_for >= desk.plan.stable_after {
                delays.reset();
            }
        }
        let delay = delays.next();
        if desk.wait_unless_stopping(delay) {
            return;
        }
        match desk.bring_up() {
            Ok(x) => {
                info!(
                    display = desk.display.as_str(),
                    server = x.kind.name(),
                    "virtual-desktop: the desktop is back"
                );
                crate::desktop_brief::refresh_soon();
                running = Some(x);
            }
            Err(no) => {
                if !desk.set_down(no.why.clone()) {
                    return;
                }
                warn!(
                    display = desk.display.as_str(),
                    why = %no.why,
                    said = %no.said,
                    next_try_s = delays.peek().as_secs(),
                    "virtual-desktop: the desktop did not start"
                );
            }
        }
    }
}

/// The delays between starts: the floor, doubled each time, up to the ceiling.
#[cfg(target_os = "linux")]
struct Backoff {
    floor: Duration,
    ceil: Duration,
    step: u32,
}

#[cfg(target_os = "linux")]
impl Backoff {
    fn new(floor: Duration, ceil: Duration) -> Self {
        Self {
            floor,
            ceil,
            step: 0,
        }
    }

    /// The next delay, without taking it.
    fn peek(&self) -> Duration {
        self.floor
            .saturating_mul(1u32 << self.step.min(20))
            .min(self.ceil)
    }

    fn next(&mut self) -> Duration {
        let delay = self.peek();
        if delay < self.ceil {
            self.step += 1;
        }
        delay
    }

    fn reset(&mut self) {
        self.step = 0;
    }
}

/// Block until our child `pid` has exited, and say how. It is NOT reaped
/// (`WNOWAIT`): it stays a zombie, so its pid — and, for the X server, the
/// desktop's group id — stays reserved until [`kill_tree_with`] has sent its
/// last signal (rule 1 there). `Child::wait` here would free both numbers
/// while the window manager is still to be signalled through them.
#[cfg(target_os = "linux")]
fn wait_exit(pid: i32) -> String {
    loop {
        // SAFETY: `siginfo_t` is plain data that `waitid` fills in; all-zero
        // is a valid value for it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `waitid(2)` on a pid, with a live out-pointer. `WNOWAIT`
        // leaves the child waitable, so this changes nothing a later
        // `waitpid` relies on.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: `waitid` returned 0 for a child that exited, so this is
            // a SIGCHLD-shaped `siginfo_t` and `si_status` is the field set.
            let status = unsafe { info.si_status() };
            return describe_exit(info.si_code, status);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => {}
            // Collected by a teardown that ran meanwhile.
            Some(libc::ECHILD) => return "already collected".into(),
            _ => {
                // Not waitable for a reason we did not foresee: `/proc` still
                // says when it is gone, only later and without the status.
                while is_running(pid) {
                    std::thread::sleep(Duration::from_millis(500));
                }
                return "exit status unknown".into();
            }
        }
    }
}

/// `waitid`'s (`si_code`, `si_status`) as words: `signal 6 (aborted), core
/// dumped` is the whole diagnosis of the field failure, in the log line that
/// reports it.
#[cfg(target_os = "linux")]
fn describe_exit(code: i32, status: i32) -> String {
    match code {
        libc::CLD_EXITED => format!("exit code {status}"),
        libc::CLD_KILLED => format!("signal {status}{}", signal_name(status)),
        libc::CLD_DUMPED => format!("signal {status}{}, core dumped", signal_name(status)),
        _ => format!("status {status} (code {code})"),
    }
}

#[cfg(target_os = "linux")]
fn signal_name(sig: i32) -> &'static str {
    match sig {
        libc::SIGABRT => " (aborted)",
        libc::SIGSEGV => " (segmentation fault)",
        libc::SIGBUS => " (bus error)",
        libc::SIGILL => " (illegal instruction)",
        libc::SIGFPE => " (arithmetic error)",
        libc::SIGKILL => " (killed)",
        libc::SIGTERM => " (terminated)",
        _ => "",
    }
}

/// Does the text of an X lock file (`"%10d\n"`) name `pid`?
fn lock_names(text: &str, pid: i32) -> bool {
    text.trim().parse::<i32>().is_ok_and(|p| p == pid)
}

/// The socket and the lock of `display`: `/tmp/.X11-unix/X<n>`, `/tmp/.X<n>-lock`.
fn x_files(display: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let n = display.trim_start_matches(':');
    (
        Path::new(X11_UNIX_DIR).join(format!("X{n}")),
        Path::new("/tmp").join(format!(".X{n}-lock")),
    )
}

/// Is `lock` a display's lock, held by `pid`? A regular file only: `/tmp` is
/// everyone's, and a link there is not read.
fn lock_is(lock: &Path, pid: i32) -> bool {
    std::fs::symlink_metadata(lock).is_ok_and(|m| m.is_file())
        && std::fs::read_to_string(lock).is_ok_and(|text| lock_names(&text, pid))
}

/// Remove the lock a dead X server of ours left on its display. A server
/// that is killed removes neither its lock nor its socket.
///
/// The next server would clear a stale lock itself, but it decides "stale" by
/// asking whether the pid in it is alive, and a recycled pid answers yes for
/// as long as its new owner lives ("Server is already active for display N",
/// on a display nobody serves). We know better: `dead_pid` was our child. The
/// socket is left where it is: the next server unlinks it before it binds.
///
/// ⚠️ Three guards, since the display's names are shared with every X server
/// on the host. It waits until the child is COLLECTED: one that is still
/// dying keeps its lock. A lock that names any other pid is not touched. And
/// it is moved aside in one step and read again before it is deleted: between
/// the first read and an unlink, another server could have cleared the stale
/// lock itself and put its own there, and the unlink would have taken that
/// one. What the rename took, if it is not ours after all, is put back.
#[cfg(target_os = "linux")]
fn remove_stale_lock(lock: &Path, dead_pid: i32) {
    if Path::new(&format!("/proc/{dead_pid}")).exists() || !lock_is(lock, dead_pid) {
        return;
    }
    let aside =
        std::path::PathBuf::from(format!("{}.roomler-{}", lock.display(), std::process::id()));
    if std::fs::rename(lock, &aside).is_err() {
        return;
    }
    if !std::fs::read_to_string(&aside).is_ok_and(|text| lock_names(&text, dead_pid)) {
        // `link` fails when a newer lock is already there, which then stays.
        let _ = std::fs::hard_link(&aside, lock);
    }
    let _ = std::fs::remove_file(&aside);
}

// ---------------------------------------------------------------------------
// The X server's EGL vendor, and its last words (#3643)
// ---------------------------------------------------------------------------

/// glvnd's list of EGL vendor files. Set, it REPLACES the scan of the vendor
/// directories.
const EGL_VENDOR_FILES: &str = "__EGL_VENDOR_LIBRARY_FILENAMES";
/// glvnd's EGL vendor directories. An operator who sets it has chosen.
const EGL_VENDOR_DIRS: &str = "__EGL_VENDOR_LIBRARY_DIRS";
/// Mesa's EGL vendor file, where a glvnd distribution installs it.
const MESA_EGL_VENDOR: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
/// The directories glvnd reads its EGL vendor files from, in its order.
const EGL_VENDOR_DIRS_DEFAULT: [&str; 2] =
    ["/etc/glvnd/egl_vendor.d", "/usr/share/glvnd/egl_vendor.d"];

/// The EGL vendor list the virtual X server is started with: Mesa's file and
/// nothing else, wherever this host keeps it (`vendors`: the vendor files it
/// has). `None` leaves the server's environment as it is:
///
/// - the daemon's own environment already chose (`files_set`, `dirs_set`),
///   which is also how an operator switches this off;
/// - the host has no vendor file where glvnd looks by default, so there is
///   nothing to keep out, and a host that keeps them elsewhere stays exactly
///   as it was.
///
/// A host with vendor files and none of Mesa's (an NVIDIA image as shipped)
/// gets Mesa's usual path, which is not there: no vendor at all.
///
/// ⚠️ The server is a software framebuffer, and no hardware vendor's EGL
/// belongs inside it. On an AWS g5g (driver 595, Mesa 25.2) the server aborted
/// within seconds of every start (`double free or corruption (out)`, on
/// 0.4.208 and 0.4.209), and the crashed process had NVIDIA's EGL mapped
/// beside Mesa's DRI shim. The likely path, read from those mappings: the
/// server's GLX goes through the shim, the shim lists its configurations
/// through EGL, and glvnd hands that call to the FIRST vendor file it finds,
/// which is NVIDIA's `10_nvidia.json`, ahead of `50_mesa.json`. Measured
/// there: installing Mesa's EGL beside NVIDIA's changes nothing, and neither
/// does `__GLX_VENDOR_LIBRARY_NAME`; naming Mesa's file keeps the server up
/// with no NVIDIA library mapped and GLX still listed. The file need not
/// exist: with no vendor at all the server starts and lists GLX all the same.
///
/// The window manager and the apps are NOT pinned. They are clients, and one
/// of them may well want the GPU.
fn egl_vendor_pin(
    files_set: bool,
    dirs_set: bool,
    vendors: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    if files_set || dirs_set || vendors.is_empty() {
        return None;
    }
    let mesa = vendors.iter().find(|file| {
        file.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().contains("mesa"))
    });
    Some(
        mesa.cloned()
            .unwrap_or_else(|| std::path::PathBuf::from(MESA_EGL_VENDOR)),
    )
}

/// The EGL vendor files this host has, where glvnd looks by default.
fn egl_vendor_files() -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for dir in EGL_VENDOR_DIRS_DEFAULT {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        found.extend(files);
    }
    found
}

/// Apply [`egl_vendor_pin`] to the X server's command.
fn pin_software_egl(cmd: &mut Command) {
    if let Some(file) = egl_vendor_pin(
        std::env::var_os(EGL_VENDOR_FILES).is_some(),
        std::env::var_os(EGL_VENDOR_DIRS).is_some(),
        &egl_vendor_files(),
    ) {
        cmd.env(EGL_VENDOR_FILES, file);
    }
}

/// How much of the X server's stderr is kept: its last words, not its life.
const STDERR_TAIL_BYTES: usize = 4096;

/// How long a log line waits for the tail's reader to see the pipe close.
const TAIL_WAIT: Duration = Duration::from_millis(500);

/// The end of what the X server wrote to stderr. Until #3643 that went to
/// `/dev/null`, and one line of it named the cause.
///
/// A pipe and a reader thread, bounded: the reader keeps the last
/// [`STDERR_TAIL_BYTES`] and drops the rest, so a server that runs for months
/// costs nothing, and it never stops reading, so the server never blocks on a
/// full pipe.
#[derive(Clone, Default)]
struct StderrTail(std::sync::Arc<std::sync::Mutex<Tail>>);

#[derive(Default)]
struct Tail {
    /// The lines kept, the newest last.
    bytes: std::collections::VecDeque<u8>,
    /// The line being received.
    partial: Vec<u8>,
    /// Bytes were dropped from the front, so the first line is a fragment.
    cut: bool,
    /// The writer closed its end.
    closed: bool,
}

impl Tail {
    /// Keep the line just received, unless it is the keymap compiler's.
    fn keep_line(&mut self) {
        let line = std::mem::take(&mut self.partial);
        if line.is_empty() || keymap_chatter(&line) {
            return;
        }
        self.bytes.extend(&line);
        if !line.ends_with(b"\n") {
            self.bytes.push_back(b'\n');
        }
        while self.bytes.len() > STDERR_TAIL_BYTES {
            self.bytes.pop_front();
            self.cut = true;
        }
    }
}

/// Every X server compiles its keymap as it starts, and again whenever a
/// client changes it, and `xkbcomp` reports on the server's stderr each keysym
/// it does not know: dozens of `> Warning:` lines a time, none of them an
/// error. Kept, they would be the whole tail of a server that was killed, and
/// could push a real last word out of it.
fn keymap_chatter(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line);
    text.trim_start().starts_with('>') || text.contains("xkbcomp")
}

impl StderrTail {
    /// Read `from` to its end on a thread of its own.
    fn follow(from: Option<impl std::io::Read + Send + 'static>) -> Self {
        let tail = Self::default();
        let Some(mut from) = from else {
            tail.close();
            return tail;
        };
        let sink = tail.clone();
        let spawned = std::thread::Builder::new()
            .name("vd-x-stderr".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    match from.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => sink.push(&buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                sink.close();
            });
        if spawned.is_err() {
            // The closure was dropped, and the pipe's read end with it: the
            // server's writes fail from here on and it carries on, which is
            // what it did when stderr was `/dev/null`.
            tail.close();
        }
        tail
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tail> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, chunk: &[u8]) {
        let mut tail = self.lock();
        for &byte in chunk {
            tail.partial.push(byte);
            // A line, or as much of an endless one as the tail holds.
            if byte == b'\n' || tail.partial.len() >= STDERR_TAIL_BYTES {
                tail.keep_line();
            }
        }
    }

    fn close(&self) {
        let mut tail = self.lock();
        tail.keep_line();
        tail.closed = true;
    }

    /// The kept lines on one line, once the writer has closed its end or
    /// `wait` has passed. A fragment at the front is dropped.
    fn last_lines(&self, wait: Duration) -> String {
        let deadline = Instant::now() + wait;
        loop {
            {
                let tail = self.lock();
                if tail.closed || Instant::now() >= deadline {
                    let bytes: Vec<u8> = tail.bytes.iter().copied().collect();
                    let text = String::from_utf8_lossy(&bytes);
                    let mut lines: Vec<&str> = text
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect();
                    if tail.cut && lines.len() > 1 {
                        lines.remove(0);
                    }
                    return lines.join(" | ");
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Has the X server behind `sock` answered a connection setup, within
/// `within`? The whole exchange is bounded: the connect, the write and the
/// read each give up after `within`.
///
/// Written out here, not through x11rb, because it has to work in a build
/// with no capture feature and has to give up: an X client library waits for
/// the setup reply for as long as the server holds the connection open, and a
/// server stuck in its start holds it open for ever.
#[cfg(target_os = "linux")]
fn x_answers(sock: &Path, within: Duration) -> bool {
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::io::{Read as _, Write as _};
    // Never zero: to the socket layer a zero timeout means "wait for ever".
    let within = within.max(Duration::from_millis(10));
    let Ok(addr) = SockAddr::unix(sock) else {
        return false;
    };
    let Ok(mut conn) = Socket::new(Domain::UNIX, Type::STREAM, None) else {
        return false;
    };
    if conn.connect_timeout(&addr, within).is_err()
        || conn.set_read_timeout(Some(within)).is_err()
        || conn.set_write_timeout(Some(within)).is_err()
    {
        return false;
    }
    // The connection setup of a little-endian client: protocol 11.0, and no
    // authorization, which the desktop's servers do not ask for (`-ac`).
    const SETUP: [u8; 12] = [b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    // The fixed head of the reply. Any status is an answer: a server that
    // refuses a client is running.
    let mut head = [0u8; 8];
    conn.write_all(&SETUP).is_ok() && conn.read_exact(&mut head).is_ok()
}

/// The Xvfb command for `dpy`.
fn xvfb_command(program: &str, dpy: &str, w: u32, h: u32) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(xvfb_args(dpy, w, h));
    cmd
}

/// Make `dir` a directory of this user's own that nobody else can enter:
/// created with mode 0700 in one step, and reused only when it already is
/// exactly that. Anything else found at the path is removed first, and
/// whatever then stands in the way is an error. Nothing here follows a link:
/// the path is under a temp directory everyone can write to, and what goes
/// into it is a config an X server will read.
#[cfg(unix)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    let make = || std::fs::DirBuilder::new().mode(0o700).create(dir);
    match make() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        made => return made,
    }
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    let euid = unsafe { libc::geteuid() };
    let ours = std::fs::symlink_metadata(dir)
        .is_ok_and(|m| m.is_dir() && m.uid() == euid && m.mode() & 0o077 == 0);
    if ours {
        return Ok(());
    }
    // `remove_dir_all` removes a link itself, never what it points at.
    std::fs::remove_dir_all(dir).or_else(|_| std::fs::remove_file(dir))?;
    make()
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// FR-108 P2 — the command for Xorg with the dummy driver on `dpy`, its config
/// and log in a directory of its own ([`private_dir`]). The directory is kept
/// from one start to the next, so the log of a server that crashed is still
/// there afterwards, as Xorg's own `Xorg.log.old`.
fn xorg_command(xorg: &Path, dpy: &str, w: u32, h: u32) -> Result<Command> {
    let dir = xorg_dir(dpy);
    private_dir(&dir).context("creating the Xorg directory")?;
    // Empty, and ours: inside a directory only this user can enter.
    match std::fs::create_dir(dir.join("conf.d")) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(e).context("creating the Xorg config directory");
        }
        _ => {}
    }
    let conf = dir.join("xorg.conf");
    std::fs::write(&conf, xorg_conf(w, h)).context("writing xorg.conf")?;
    let mut cmd = Command::new(xorg);
    cmd.args(xorg_args(
        dpy,
        &conf,
        &dir.join("conf.d"),
        &dir.join("Xorg.log"),
    ));
    Ok(cmd)
}

/// Place a child at the head of a NEW process group (`setpgid(0, 0)` before
/// exec). Linux-only; a no-op elsewhere (the desktop is never started there).
/// Like every child of the desktop, it keeps its stdio and nothing else of
/// the daemon's.
#[cfg(target_os = "linux")]
fn lead_new_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0);
    tunnel_core::child::inherit_only_stdio(cmd);
}
#[cfg(not(target_os = "linux"))]
fn lead_new_group(_cmd: &mut Command) {}

/// Place a child INTO the desktop's process group, if we have one. Linux-only.
/// It keeps its stdio and nothing else of the daemon's.
#[cfg(target_os = "linux")]
fn join_group(cmd: &mut Command, pgid: Option<i32>) {
    use std::os::unix::process::CommandExt as _;
    if let Some(g) = pgid.filter(|g| *g > 1) {
        cmd.process_group(g);
    }
    tunnel_core::child::inherit_only_stdio(cmd);
}
#[cfg(not(target_os = "linux"))]
fn join_group(_cmd: &mut Command, _pgid: Option<i32>) {}

/// The process group Xvfb leads — its pid, since we made it a group leader.
/// `None` on non-Linux. Reading it back from `/proc` would race a just-exec'd
/// child, so we take the leader's pid, which IS the group id by construction.
#[cfg(target_os = "linux")]
fn group_of(leader: &Child) -> Option<i32> {
    Some(leader.id() as i32)
}
#[cfg(not(target_os = "linux"))]
fn group_of(_leader: &Child) -> Option<i32> {
    None
}

/// The Xvfb command line for one virtual desktop.
///
/// Extracted from `start` so the flags are assertable — the screen-saver
/// disable below is invisible at runtime until it has already cost someone a
/// black screen, which is not a thing to leave un-locked.
fn xvfb_args(display: &str, w: u32, h: u32) -> Vec<String> {
    vec![
        display.to_string(),
        "-screen".into(),
        "0".into(),
        format!("{w}x{h}x24"),
        // No access control: the agent may run as root while the desktop is
        // owned by another user, and this is a loopback-only server.
        "-ac".into(),
        "-nolisten".into(),
        "tcp".into(),
        // A remote-desktop host must never blank itself. X's BUILT-IN screen
        // saver defaults to ~10 minutes and there is no local keyboard here to
        // wake it, so without this the operator eventually connects to a black
        // screen — and capture reports it faithfully, which reads as a broken
        // stream rather than a blanked one. `-s 0` disables that timeout.
        //
        // Deliberately NOT `-dpms`: Xvfb has no DPMS extension at all
        // (`Xlib: extension "DPMS" missing` on the display it serves), so the
        // flag would be inert noise. And this reaches the X SERVER only — a
        // full DE started through VIRTUAL_DESKTOP_WM/_STARTUP brings its own
        // screensaver (xfce4-screensaver blanked and then LOCKED a field host
        // on 2026-08-29, costing a measurement round), which is that DE's
        // setting to turn off and not something a server flag can reach.
        "-s".into(),
        "0".into(),
        // One generation for the server's whole life (#3643), as Xorg's argv
        // has it. Without this an X server RESETS when its last client
        // disconnects, and the wait for "it answers" is exactly that: the
        // first client, gone again before the window manager connects. A
        // reset there re-runs the server's whole start under the desktop
        // that is about to use it.
        "-noreset".into(),
    ]
}

/// Confirm the required binaries exist, else bail with the apt line: the
/// window manager, and `xvfb` (the Xvfb program) unless Xorg with the dummy
/// driver will serve (`try_xorg`, FR-108 P2).
fn preflight(cfg: &Config, xvfb: &str, try_xorg: bool) -> Result<()> {
    let mut missing = Vec::new();
    if !(try_xorg && xorg_dummy().is_some()) && binary_on_path(xvfb).is_none() {
        missing.push("xvfb".to_string());
    }
    if binary_on_path(&cfg.wm).is_none() {
        missing.push(cfg.wm.clone());
    }
    if !missing.is_empty() {
        // One line: it is logged, and shown to a controller as the reason.
        bail!(
            "missing required binaries: install them, e.g. `sudo apt install {}`",
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

/// Wait until the X server `x_pid` ANSWERS on its display's socket
/// ([`x_answers`]), and fail as soon as it has exited or `timeout` has passed.
///
/// ⚠️ Not "its socket exists" (#3643). An X server makes its socket first and
/// initialises its extensions afterwards, and the field failure died there:
/// the old wait saw the socket, called the desktop up, and the server was
/// gone ten seconds later. A server answers a connection setup only once it
/// has reached its dispatch loop.
///
/// ⚠️ And not "something answers". The display keeps its number for the
/// daemon's life, so between two of our servers another one can take it (an
/// `xvfb-run -a` starts looking at `:99` too). It would answer for us, our own
/// server would exit a moment later ("already active"), and by then the
/// window manager and the apps would be on a stranger's display, with capture
/// to follow. A server writes its pid into the display's lock before it makes
/// its socket, so the lock has to name OUR server before an answer counts.
#[cfg(target_os = "linux")]
fn wait_display_ready(
    sock: &Path,
    lock: &Path,
    x_pid: i32,
    timeout: Duration,
) -> std::result::Result<(), NotReady> {
    let deadline = Instant::now() + timeout;
    loop {
        // A zombie reads as not running, and it stays one: nothing is reaped
        // here (rule 1 of `kill_tree_with`).
        if !is_running(x_pid) {
            return Err(NotReady::Exited);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(NotReady::Silent);
        }
        if lock_is(lock, x_pid) && sock.exists() && x_answers(sock, left.min(READY_PROBE)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100).min(left));
    }
}

/// When `start` nudges the window manager, counted from the startup apps'
/// launch.
///
/// ⚠️ A window that maps while openbox is still starting is left UNMAPPED,
/// with no `WM_STATE`, until X traffic next wakes openbox: its startup reads
/// the MapRequest, and its main loop then sleeps in `poll()` on a socket with
/// nothing new on it. The desktop is a black root and a session shows nothing,
/// which is how the first session on a rented DGX Spark came up (FR-122 §8).
/// Measured with Xvfb, openbox, xclock and xterm in the order `start` uses:
/// apps launched 50 ms after the WM stayed unmapped in 30 runs of 30, and one
/// root-property change mapped every one of them. Starting the apps once the
/// WM sets `_NET_SUPPORTING_WM_CHECK` made it worse (22 of 25), because openbox
/// sets it as its startup begins, which is exactly that window. Nudges at
/// 0.25, 0.5, 1 and 2 s left none unmapped, 0 of 75 at 0, 20 and 50 ms; the
/// 4 and 8 s ones cover a WM that starts slowly on a loaded host. A WM that is awake
/// ignores the property.
#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
const WM_NUDGE_AT_MS: [u64; 6] = [250, 500, 1_000, 2_000, 4_000, 8_000];

/// The root-window property a nudge sets, and the last one deletes. Nothing
/// reads it: a window manager selects `PropertyChangeMask` on the root, so the
/// `PropertyNotify` is what wakes it.
#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
const WM_NUDGE_ATOM: &[u8] = b"_ROOMLER_WM_NUDGE";

/// Run [`WM_NUDGE_AT_MS`] on a thread of its own, so `start` does not wait for
/// it. A display that no Unix socket reaches any more (the desktop was torn
/// down) ends the schedule.
#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
fn nudge_wm_after_startup(display: &str) {
    let display = display.to_string();
    let spawned = std::thread::Builder::new()
        .name("vd-wm-nudge".into())
        .spawn(move || {
            let launched = Instant::now();
            let last = WM_NUDGE_AT_MS.len() - 1;
            for (i, at) in WM_NUDGE_AT_MS.iter().enumerate() {
                if let Some(wait) = Duration::from_millis(*at).checked_sub(launched.elapsed()) {
                    std::thread::sleep(wait);
                }
                if nudge_wm(&display, i as u32, i == last).is_none() {
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        warn!(%e, "virtual-desktop: the window-manager nudges did not start");
    }
}

/// A build with no X11 client cannot nudge, and captures nothing from the
/// desktop either.
#[cfg(not(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
)))]
fn nudge_wm_after_startup(_display: &str) {}

/// Set [`WM_NUDGE_ATOM`] on `display`'s root window to `n`, or with `remove`
/// delete it; either one is a `PropertyNotify` to every client listening on the
/// root. `Some(true)` once the server has taken the request, `None` when no
/// Unix socket reaches the display.
#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
fn nudge_wm(display: &str, n: u32, remove: bool) -> Option<bool> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
    use x11rb::wrapper::ConnectionExt as _;
    crate::capture::x_server_query(display, |conn, root| {
        let Some(atom) = conn
            .intern_atom(false, WM_NUDGE_ATOM)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
        else {
            return false;
        };
        let sent = if remove {
            conn.delete_property(root, atom).map(|c| c.check())
        } else {
            conn.change_property32(PropMode::REPLACE, root, atom, AtomEnum::CARDINAL, &[n])
                .map(|c| c.check())
        };
        matches!(sent, Ok(Ok(())))
    })
}

/// The nudge against a real X server: the CI Xvfb lane runs these with
/// `ROOMLERD_TEST_X11=1` and its `DISPLAY` (`.github/workflows/ci.yml`).
#[cfg(all(
    test,
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
mod wm_nudge_tests {
    use super::*;

    #[test]
    fn the_schedule_runs_forward_for_seconds() {
        assert!(WM_NUDGE_AT_MS.windows(2).all(|w| w[0] < w[1]));
        // Early enough for an app that maps at once, long enough for a WM
        // that starts slowly on a loaded host.
        assert!(WM_NUDGE_AT_MS[0] <= 500);
        assert!(WM_NUDGE_AT_MS[WM_NUDGE_AT_MS.len() - 1] >= 4_000);
    }

    /// A window manager selects `PropertyChangeMask` on the root. Listen the
    /// same way and require both nudges' events, the set and the delete: an
    /// event that never arrives wakes nobody.
    #[test]
    fn a_nudge_wakes_a_root_listener() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::Event;
        use x11rb::protocol::xproto::{
            ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Property,
        };
        if std::env::var("ROOMLERD_TEST_X11").as_deref() != Ok("1") {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return;
        }
        let display = std::env::var("DISPLAY").expect("ROOMLERD_TEST_X11=1 but DISPLAY is unset");
        let (conn, screen) = x11rb::connect(Some(&display)).expect("the Xvfb");
        let root = conn.setup().roots[screen].root;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
        let atom = conn
            .intern_atom(false, WM_NUDGE_ATOM)
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        assert_eq!(
            nudge_wm(&display, 7, false),
            Some(true),
            "the nudge did not reach {display}"
        );
        assert_eq!(
            nudge_wm(&display, 8, true),
            Some(true),
            "the last nudge did not reach {display}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while seen.len() < 2 && Instant::now() < deadline {
            match conn.poll_for_event().unwrap() {
                Some(Event::PropertyNotify(e)) if e.window == root && e.atom == atom => {
                    seen.push(e.state)
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert_eq!(
            seen,
            vec![Property::NEW_VALUE, Property::DELETE],
            "a root listener was not woken by each nudge"
        );
        println!("x11-real: a nudge woke a root listener");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen-saver disable is the whole point of this argv and is
    /// invisible at runtime until it has already blanked someone's session,
    /// so assert it as a pair rather than trusting the flag survived an edit.
    #[test]
    fn xvfb_args_disable_the_builtin_screen_saver() {
        let a = xvfb_args(":99", 1920, 1080);
        let i = a
            .iter()
            .position(|s| s == "-s")
            .expect("Xvfb must be told to disable the screen saver");
        assert_eq!(
            a.get(i + 1).map(String::as_str),
            Some("0"),
            "-s must be 0 (disabled); any other value is a TIMEOUT, not a disable"
        );
        // -dpms would be inert here: Xvfb ships no DPMS extension.
        assert!(!a.iter().any(|s| s == "-dpms"));
    }

    #[test]
    fn xvfb_args_carry_the_display_and_geometry() {
        let a = xvfb_args(":7", 1280, 720);
        assert_eq!(a.first().map(String::as_str), Some(":7"));
        assert!(a.iter().any(|s| s == "1280x720x24"), "got {a:?}");
        assert!(a.iter().any(|s| s == "-ac"));
    }

    /// FR-108 P2 — the config starts at the configured size exactly, declares
    /// the reduced blanking its modelines carry (or Xorg drops the start mode
    /// and starts at the dummy's 2048x1536, with nothing named to restore to),
    /// and adds no input device.
    #[test]
    fn xorg_conf_starts_at_the_configured_size() {
        let c = xorg_conf(1920, 1080);
        assert!(c.contains(r#"Driver "dummy""#), "{c}");
        assert!(c.contains(&format!("VideoRam {XORG_VIDEO_RAM_KIB}")), "{c}");
        assert!(c.contains(r#"Option "ReducedBlanking" "true""#), "{c}");
        assert!(
            c.contains(r#"Modeline "roomler-1920x1080" 138.52 1920 1968 2000 2080 1080 1083 1088 1110 +hsync +vsync"#),
            "{c}"
        );
        assert!(c.contains(r#"Modes "roomler-1920x1080""#), "{c}");
        assert!(c.contains(r#"Option "AutoAddDevices" "false""#), "{c}");
        assert!(c.contains(r#"Option "BlankTime" "0""#), "{c}");
    }

    /// FR-108 P2 — Xorg's argv: its own config and an EMPTY config dir (no
    /// host `xorg.conf.d` snippet), loopback only, and nothing that blanks.
    #[test]
    fn xorg_args_isolate_and_never_blank() {
        let a = xorg_args(
            ":101",
            Path::new("/tmp/x/xorg.conf"),
            Path::new("/tmp/x/conf.d"),
            Path::new("/tmp/x/Xorg.log"),
        );
        assert_eq!(a.first().map(String::as_str), Some(":101"));
        let after = |flag: &str| {
            a.iter()
                .position(|s| s == flag)
                .and_then(|i| a.get(i + 1))
                .cloned()
        };
        assert_eq!(after("-config").as_deref(), Some("/tmp/x/xorg.conf"));
        assert_eq!(after("-configdir").as_deref(), Some("/tmp/x/conf.d"));
        assert_eq!(after("-nolisten").as_deref(), Some("tcp"));
        assert_eq!(
            after("-s").as_deref(),
            Some("0"),
            "-s 0 disables the screen saver"
        );
        for flag in ["-dpms", "-ac", "-noreset", "-novtswitch"] {
            assert!(a.iter().any(|s| s == flag), "{flag} missing from {a:?}");
        }
    }

    /// FR-108 P2 — Xorg with the dummy driver only when the real binary, the
    /// driver and `xrandr` are all there; the first binary found wins.
    #[test]
    fn xorg_dummy_needs_the_binary_the_driver_and_xrandr() {
        let have = |set: &'static [&'static str]| move |p: &str| set.contains(&p);
        let all: &'static [&'static str] = &[
            "/usr/lib/xorg/Xorg",
            "/usr/lib/xorg/modules/drivers/dummy_drv.so",
        ];
        assert_eq!(
            pick_xorg_dummy(have(all), true),
            Some(std::path::PathBuf::from("/usr/lib/xorg/Xorg"))
        );
        assert_eq!(
            pick_xorg_dummy(have(all), false),
            None,
            "no xrandr: no way to switch"
        );
        let fedora: &'static [&'static str] = &[
            "/usr/libexec/Xorg",
            "/usr/lib64/xorg/modules/drivers/dummy_drv.so",
        ];
        assert_eq!(
            pick_xorg_dummy(have(fedora), true),
            Some(std::path::PathBuf::from("/usr/libexec/Xorg"))
        );
        let no_driver: &'static [&'static str] = &["/usr/lib/xorg/Xorg"];
        assert_eq!(pick_xorg_dummy(have(no_driver), true), None);
        let wrapper_only: &'static [&'static str] = &[
            "/usr/bin/Xorg",
            "/usr/lib/xorg/modules/drivers/dummy_drv.so",
        ];
        assert_eq!(
            pick_xorg_dummy(have(wrapper_only), true),
            None,
            "the console-only wrapper is not the binary"
        );
    }

    /// FR-108 P2 — the kill switch is the one word, any case.
    #[test]
    fn the_server_kill_switch() {
        assert!(server_forced_xvfb(Some("xvfb")));
        assert!(server_forced_xvfb(Some(" XVFB ")));
        assert!(!server_forced_xvfb(Some("xorg")));
        assert!(!server_forced_xvfb(Some("")));
        assert!(!server_forced_xvfb(None));
    }

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
        // A dir that doesn't exist → no sockets → the first candidate with no
        // lock: `:99`, unless this host runs an X server there (a dev box
        // with a virtual desktop of its own does, and so does `xvfb-run`).
        let unlocked = (99..=120)
            .find(|n| !Path::new("/tmp").join(format!(".X{n}-lock")).exists())
            .unwrap_or(99);
        assert_eq!(
            pick_display("/nonexistent/x11/dir/zzz"),
            format!(":{unlocked}")
        );
    }

    #[test]
    fn preflight_missing_binaries_names_apt_packages() {
        let cfg = Config {
            resolution: "1920x1080".into(),
            wm: "definitely-not-a-real-wm-binary-xyz".into(),
            startup: vec![],
        };
        let err = preflight(&cfg, "definitely-not-a-real-xvfb-xyz", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("apt install"), "err: {err}");
        assert!(err.contains("xvfb"), "err: {err}");
        assert!(
            err.contains("definitely-not-a-real-wm-binary-xyz"),
            "err: {err}"
        );
        // One line: it is the reason a controller is shown, too.
        assert!(!err.contains('\n'), "err: {err}");
    }

    // ---- the desktop, supervised (#3643) -----------------------------------

    /// An X server that resets when its last client leaves would reset under
    /// the wait for "it answers", which is its first client.
    #[test]
    fn xvfb_args_keep_one_server_generation() {
        assert!(xvfb_args(":99", 1920, 1080).iter().any(|s| s == "-noreset"));
    }

    /// The X server gets Mesa's EGL vendor file and nothing else, and an
    /// operator who set either glvnd variable keeps their choice.
    #[test]
    fn the_x_server_is_pinned_to_mesas_egl_unless_the_operator_chose() {
        use std::path::PathBuf;
        let files = |names: &[&str]| names.iter().map(PathBuf::from).collect::<Vec<_>>();
        let nvidia = "/usr/share/glvnd/egl_vendor.d/10_nvidia.json";
        let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";

        // The field host as shipped: NVIDIA's file only. Mesa's usual path is
        // named, which is not there, so the server gets no vendor at all.
        assert_eq!(
            egl_vendor_pin(false, false, &files(&[nvidia])),
            Some(PathBuf::from(MESA_EGL_VENDOR))
        );
        // Both installed: NVIDIA's sorts first, and Mesa's is the one named.
        assert_eq!(
            egl_vendor_pin(false, false, &files(&[nvidia, mesa])),
            Some(PathBuf::from(mesa))
        );
        // Mesa's wherever the host keeps it, by its name.
        let etc = "/etc/glvnd/egl_vendor.d/50_mesa.json";
        assert_eq!(
            egl_vendor_pin(false, false, &files(&[etc, nvidia])),
            Some(PathBuf::from(etc))
        );
        // Mesa alone: named too, which changes nothing glvnd would not do.
        assert_eq!(
            egl_vendor_pin(false, false, &files(&[mesa])),
            Some(PathBuf::from(mesa))
        );
        // No vendor file where glvnd looks: nothing to keep out, nothing set.
        assert_eq!(egl_vendor_pin(false, false, &[]), None);
        // The operator chose: either variable, whatever is installed.
        assert_eq!(egl_vendor_pin(true, false, &files(&[nvidia, mesa])), None);
        assert_eq!(egl_vendor_pin(false, true, &files(&[nvidia])), None);
        // The variable that REPLACES glvnd's directory scan. The GLX one was
        // measured and changes nothing.
        assert_eq!(EGL_VENDOR_FILES, "__EGL_VENDOR_LIBRARY_FILENAMES");
        assert!(MESA_EGL_VENDOR.ends_with("/egl_vendor.d/50_mesa.json"));
    }

    /// The reason reads as a sentence where a session shows it.
    #[test]
    fn a_down_desktop_is_explained_in_a_sentence() {
        assert_eq!(
            down_detail_of("its X server (Xvfb) exited: signal 6 (aborted), core dumped"),
            "this device's virtual desktop is down: its X server (Xvfb) exited: signal 6 \
             (aborted), core dumped. The agent starts it again by itself; reconnect in a moment"
        );
    }

    /// An X lock is the pid, right-aligned in ten columns, and a newline.
    #[test]
    fn a_lock_is_ours_only_when_it_names_our_pid() {
        assert!(lock_names("      1234\n", 1234));
        assert!(lock_names("1234", 1234));
        assert!(!lock_names("      1235\n", 1234));
        assert!(!lock_names("", 1234));
        assert!(!lock_names("not a pid\n", 1234));
        assert!(!lock_names("12 34\n", 1234));
    }

    /// The tail keeps the END, whole lines only, on one line.
    #[test]
    fn the_stderr_tail_keeps_the_last_whole_lines() {
        let tail = StderrTail::default();
        tail.push(b"double free or corruption (out)\n(EE) Backtrace:\n\n");
        tail.push(b"Fatal server error:\n(EE) Caught signal 6 (Aborted). Server aborting\n");
        tail.close();
        assert_eq!(
            tail.last_lines(Duration::ZERO),
            "double free or corruption (out) | (EE) Backtrace: | Fatal server error: | \
             (EE) Caught signal 6 (Aborted). Server aborting"
        );

        // The keymap compiler's report is not the server's last words: a
        // killed server's tail is empty, not forty unknown keysyms.
        let killed = StderrTail::default();
        killed.push(b"The XKEYBOARD keymap compiler (xkbcomp) reports:\n");
        killed.push(b"> Warning:          Could not resolve keysym XF86NavInfo\n");
        killed.push(b"Errors from xkbcomp are not fatal to the X server\n");
        killed.close();
        assert_eq!(killed.last_lines(Duration::ZERO), "");
        // And a last line with no newline is still a line.
        let abrupt = StderrTail::default();
        abrupt.push(b"> Warning:          Could not resolve keysym XF86NavInfo\n(EE) no screens");
        abrupt.push(b" found");
        abrupt.close();
        assert_eq!(abrupt.last_lines(Duration::ZERO), "(EE) no screens found");

        // More than the cap: the front goes, and the fragment it leaves.
        let long = StderrTail::default();
        for i in 0..400 {
            long.push(format!("line number {i:04}\n").as_bytes());
        }
        long.close();
        let kept = long.last_lines(Duration::ZERO);
        // Seventeen bytes a line, so the cap holds 240 whole ones.
        let lines = kept.matches("line number").count();
        assert!((200..=STDERR_TAIL_BYTES / 17).contains(&lines), "{lines}");
        assert!(kept.ends_with("line number 0399"), "{kept}");
        assert!(kept.starts_with("line number 0"), "a fragment led: {kept}");
        assert!(!kept.contains("line number 0000"), "{kept}");
    }

    /// The reader thread follows a stream to its end, and `last_lines` waits
    /// for that end instead of reporting half of it.
    #[test]
    fn the_stderr_tail_follows_a_stream_to_its_end() {
        let tail = StderrTail::follow(Some(std::io::Cursor::new(
            b"(EE) Server is already active for display 99\n".to_vec(),
        )));
        assert_eq!(
            tail.last_lines(Duration::from_secs(5)),
            "(EE) Server is already active for display 99"
        );
        // No stream at all (a child with no stderr pipe) is an empty tail,
        // closed, so nobody waits for it.
        let none = StderrTail::follow(None::<std::io::Empty>);
        assert_eq!(none.last_lines(Duration::from_secs(5)), "");
    }

    // ---- teardown (#1684) --------------------------------------------------

    /// A synthetic `/proc` entry. `starttime` plays no part in SELECTION, so
    /// a constant; the identity tests set it explicitly.
    #[cfg(target_os = "linux")]
    fn pi(pid: i32, ppid: i32, pgid: i32) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            pgid,
            starttime: 1,
        }
    }

    /// The core of the fix: the SIGTERM-deaf `at-spi-bus-launcher` is a
    /// grandchild that `setsid`'d into its OWN process group, so the group net
    /// cannot see it — only the parent-link closure can. And the closure must
    /// walk DOWN from the desktop roots, never up into the daemon, so a sibling
    /// the daemon spawned for something else (an exec session) is never swept.
    #[cfg(target_os = "linux")]
    #[test]
    fn select_targets_reaches_a_setsid_grandchild_and_nothing_else() {
        let snap = [
            // The daemon itself, in its own group.
            pi(100, 1, 100),
            // Xvfb — the desktop group leader (pgid == pid).
            pi(200, 100, 200),
            // The WM / a startup app — still in the desktop group.
            pi(300, 200, 200),
            // at-spi-bus-launcher: a grandchild that setsid'd into its OWN
            // group (400), so pgid == 200 does NOT match it.
            pi(400, 300, 400),
            // An unrelated process.
            pi(500, 1, 500),
            // ANOTHER daemon child — an exec/PTY session, say — that the
            // desktop teardown must never touch.
            pi(600, 100, 600),
        ];
        let targets = select_targets(&snap, &[200], Some(200), 100);
        assert!(targets.contains(&200), "Xvfb");
        assert!(targets.contains(&300), "the WM/app");
        assert!(
            targets.contains(&400),
            "the setsid'd grandchild must be reached via the parent-link closure"
        );
        assert!(!targets.contains(&100), "never the daemon");
        assert!(!targets.contains(&500), "never an unrelated process");
        assert!(
            !targets.contains(&600),
            "never a non-desktop child of the daemon"
        );
        assert_eq!(targets.len(), 3, "{targets:?}");
    }

    /// A root that already exited (absent from the snapshot) is still targeted:
    /// signalling it is harmless (ESRCH) and it lets `reap` collect a zombie.
    #[cfg(target_os = "linux")]
    #[test]
    fn select_targets_keeps_roots_absent_from_the_snapshot() {
        let targets = select_targets(&[], &[7, 8], None, 999);
        assert!(targets.contains(&7) && targets.contains(&8));
    }

    /// Defensive: pid 0/1 and the daemon are removed even if a link points at
    /// them — a reparent to init must not turn into "signal init".
    #[cfg(target_os = "linux")]
    #[test]
    fn select_targets_never_signals_pid1_or_the_daemon() {
        let snap = [
            pi(1, 0, 1),
            pi(100, 1, 200), // daemon, sharing the group
            pi(200, 1, 200), // a root, ppid reparented to init
            pi(300, 200, 200),
        ];
        let targets = select_targets(&snap, &[200], Some(200), 100);
        assert!(!targets.contains(&1), "never pid 1");
        assert!(!targets.contains(&0), "never pid 0");
        assert!(!targets.contains(&100), "never the daemon");
        assert!(targets.contains(&200) && targets.contains(&300));
    }

    /// A `/proc/<pid>/stat` line for pid 1234 (`Xvfb`), started at tick
    /// 987654 — field 22, the 20th token after the comm's closing `)`.
    #[cfg(target_os = "linux")]
    const STAT_1234: &str = "1234 (Xvfb) S 1 1234 1234 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 1 0 987654 12345 678 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0";

    /// Identity is the starttime: equal ⇒ the same process; different ⇒ the pid
    /// was recycled; unreadable ⇒ never "the same" (when in doubt, don't signal).
    #[cfg(target_os = "linux")]
    #[test]
    fn same_process_compares_starttime_and_fails_safe() {
        assert_eq!(parse_stat(STAT_1234), Some(("S", 1, 1234, 987_654)));
        let info = ProcInfo {
            pid: 1234,
            ppid: 1,
            pgid: 1234,
            starttime: 987_654,
        };
        // The same stat.
        assert!(same_process(&info, STAT_1234));
        // A recycled pid: everything equal but a later starttime.
        let recycled = STAT_1234.replace(" 987654 ", " 987655 ");
        assert_ne!(recycled, STAT_1234, "the fixture must contain the field");
        assert!(!same_process(&info, &recycled));
        // Unparseable, in every way a read can go wrong.
        assert!(!same_process(&info, ""));
        assert!(!same_process(&info, "garbage"));
        assert!(!same_process(&info, "1234 (Xvfb) S 1 1234 1234 0")); // too short
        assert!(!same_process(
            &info,
            "1234 (Xvfb) S 1 1234 1234 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 1 0 notanumber"
        ));
        // A comm with spaces and `)` in it is split on the LAST `)`.
        let evil = STAT_1234.replace("(Xvfb)", "(a) b) c)");
        assert!(same_process(&info, &evil));
        assert_eq!(parse_stat(&evil).map(|t| t.1), Some(1), "ppid still f4");
    }

    /// What `kill_tree_with` did, in order.
    #[cfg(target_os = "linux")]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ev {
        Signal(i32, i32),
        Killpg(i32, i32),
        /// `same_process_now` was asked about this pid.
        Identity(i32),
        Reap,
    }

    /// A scripted process table. `deaf` pids ignore SIGTERM (at-spi);
    /// `recycled` pids LOOK running but fail the identity check — the number
    /// now belongs to somebody else's process.
    #[cfg(target_os = "linux")]
    struct MockProcs {
        table: Vec<ProcInfo>,
        running: std::collections::BTreeSet<i32>,
        deaf: std::collections::BTreeSet<i32>,
        recycled: std::collections::BTreeSet<i32>,
        events: Vec<Ev>,
    }

    #[cfg(target_os = "linux")]
    impl MockProcs {
        fn new(table: &[ProcInfo], deaf: &[i32], recycled: &[i32]) -> Self {
            Self {
                table: table.to_vec(),
                running: table.iter().map(|p| p.pid).collect(),
                deaf: deaf.iter().copied().collect(),
                recycled: recycled.iter().copied().collect(),
                events: Vec::new(),
            }
        }
        fn deliver(&mut self, pid: i32, sig: i32) {
            if sig == libc::SIGKILL || !self.deaf.contains(&pid) {
                self.running.remove(&pid);
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Procs for MockProcs {
        fn snapshot(&mut self) -> Vec<ProcInfo> {
            // A dead process leaves `/proc` once its parent (or init) reaps it.
            let running = &self.running;
            self.table
                .iter()
                .filter(|p| running.contains(&p.pid))
                .copied()
                .collect()
        }
        fn is_running(&mut self, pid: i32) -> bool {
            self.running.contains(&pid)
        }
        fn same_process_now(&mut self, info: &ProcInfo) -> bool {
            self.events.push(Ev::Identity(info.pid));
            self.running.contains(&info.pid) && !self.recycled.contains(&info.pid)
        }
        fn signal(&mut self, pid: i32, sig: i32) {
            self.events.push(Ev::Signal(pid, sig));
            self.deliver(pid, sig);
        }
        fn killpg(&mut self, pgid: i32, sig: i32) {
            self.events.push(Ev::Killpg(pgid, sig));
            let members: Vec<i32> = self
                .table
                .iter()
                .filter(|p| p.pgid == pgid)
                .map(|p| p.pid)
                .collect();
            for m in members {
                self.deliver(m, sig);
            }
        }
        fn reap(&mut self, _roots: &[i32]) -> bool {
            self.events.push(Ev::Reap);
            true
        }
        fn sleep(&mut self, _d: Duration) {}
    }

    /// The field tree: Xvfb (root, group leader) → the WM → a SIGTERM-deaf
    /// grandchild that `setsid`'d into its own group.
    #[cfg(target_os = "linux")]
    fn desktop_tree() -> [ProcInfo; 3] {
        [pi(200, 7, 200), pi(300, 200, 200), pi(400, 300, 400)]
    }

    /// Rule 1 of `kill_tree_with`: every signal — both passes, both `killpg`s
    /// — is sent before ANY root is reaped, because reaping frees a pid (and,
    /// for Xvfb, the desktop's pgid) that a later signal would then hit
    /// recycled. Reaping inside the grace loop is exactly what the first cut
    /// of this code did. And rule 2: the SIGKILL of the non-root grandchild is
    /// preceded, immediately, by its identity check.
    #[cfg(target_os = "linux")]
    #[test]
    fn kill_tree_signals_everything_before_it_reaps_anything() {
        let mut m = MockProcs::new(&desktop_tree(), &[400], &[]);
        kill_tree_with(&mut m, &[200], Some(200), Duration::from_millis(30));
        let ev = &m.events;
        let first_reap = ev
            .iter()
            .position(|e| *e == Ev::Reap)
            .expect("the roots are reaped at the end");
        let last_signal = ev
            .iter()
            .rposition(|e| matches!(e, Ev::Signal(..) | Ev::Killpg(..)))
            .expect("something was signalled");
        assert!(
            last_signal < first_reap,
            "a reap ran before the last signal: {ev:?}"
        );
        assert_eq!(
            ev.iter().filter(|e| **e == Ev::Reap).count(),
            1,
            "reaped once, at the end: {ev:?}"
        );
        // The deaf grandchild needed SIGKILL; the others died on SIGTERM.
        let kill_400 = ev
            .iter()
            .position(|e| *e == Ev::Signal(400, libc::SIGKILL))
            .expect("the SIGTERM-deaf grandchild is SIGKILLed");
        assert!(!ev.contains(&Ev::Signal(200, libc::SIGKILL)), "{ev:?}");
        assert!(!ev.contains(&Ev::Signal(300, libc::SIGKILL)), "{ev:?}");
        // …and its identity was re-read RIGHT before that SIGKILL.
        assert_eq!(
            ev[kill_400 - 1],
            Ev::Identity(400),
            "identity check must immediately precede the kill: {ev:?}"
        );
        // Both group kills happened, each after the per-pid signals of its pass.
        assert!(ev.contains(&Ev::Killpg(200, libc::SIGTERM)), "{ev:?}");
        assert!(ev.contains(&Ev::Killpg(200, libc::SIGKILL)), "{ev:?}");
        assert!(m.running.is_empty(), "left running: {:?}", m.running);
    }

    /// Rule 2: a pid that is not our own child is signalled only while it is
    /// still the process the snapshot saw. Here 400's number has been recycled
    /// — it LOOKS running but fails the identity check — so it must not receive
    /// a single signal, TERM or KILL. Our own children (pids we hold unreaped,
    /// hence unrecyclable) are still torn down. systemd's own cgroup kill is the
    /// backstop for what we refuse to touch.
    #[cfg(target_os = "linux")]
    #[test]
    fn kill_tree_never_signals_a_recycled_pid() {
        let mut m = MockProcs::new(&desktop_tree(), &[400], &[400]);
        kill_tree_with(&mut m, &[200], Some(200), Duration::from_millis(30));
        let ev = &m.events;
        assert!(
            ev.contains(&Ev::Identity(400)),
            "identity was checked: {ev:?}"
        );
        assert!(
            !ev.iter().any(|e| matches!(e, Ev::Signal(400, _))),
            "a recycled pid was signalled: {ev:?}"
        );
        assert!(ev.contains(&Ev::Signal(200, libc::SIGTERM)), "{ev:?}");
        assert!(ev.contains(&Ev::Signal(300, libc::SIGTERM)), "{ev:?}");
        // Whoever holds 400 now is untouched.
        assert!(m.running.contains(&400), "{:?}", m.running);
    }

    /// End to end, with real syscalls: build the field failure — a daemon
    /// child, its child, and a SIGTERM-deaf grandchild that `setsid`s into its
    /// own session (as at-spi-bus-launcher does) — then run the teardown and
    /// prove the whole tree is gone within the bound. Before the fix, the
    /// grandchild survives every signal the daemon sent.
    #[cfg(target_os = "linux")]
    #[test]
    // The spawned `root` is reaped by `kill_tree`/`reap` (libc `waitpid`) rather
    // than `Child::wait`, and on the `setsid`-missing skip path it is killed via
    // its group — so the lint's "call `.wait()`" advice does not apply, and it
    // cannot see the `waitpid` we do call.
    #[allow(clippy::zombie_processes)]
    fn kill_tree_reaps_a_sigterm_deaf_setsid_grandchild() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!(
            "roomler-vdesk-teardown-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display();

        // root → child subshell → grandchild. The grandchild `setsid`s (its
        // own session/group), traps SIGTERM, and loops — so only SIGKILL ends
        // it, exactly like the field host's at-spi-bus-launcher.
        let script = format!(
            "echo $$ > \"{d}/root.pid\"\n\
             (\n\
             echo $$ > \"{d}/child.pid\"\n\
             setsid sh -c 'echo $$ > \"{d}/gc.pid\"; trap \"\" TERM; while true; do sleep 1; done' &\n\
             sleep 300\n\
             ) &\n\
             sleep 300\n"
        );
        let script_path = dir.join("tree.sh");
        std::fs::File::create(&script_path)
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();

        let mut cmd = Command::new("sh");
        cmd.arg(&script_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        lead_new_group(&mut cmd);
        let root = cmd.spawn().expect("spawn the desktop-tree root");
        let root_pid = root.id() as i32;
        let pgid = group_of(&root);

        let read_pid = |name: &str| -> Option<i32> {
            std::fs::read_to_string(dir.join(name))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
        };
        let (mut child, mut gc) = (None, None);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            child = read_pid("child.pid");
            gc = read_pid("gc.pid");
            if child.is_some() && gc.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let Some(gc) = gc else {
            // No `setsid` binary (a minimal host) ⇒ the grandchild never
            // formed. Tear down what we did spawn and skip — the pure
            // `select_targets` tests carry the closure proof without it.
            if let Some(g) = pgid {
                unsafe {
                    libc::killpg(g, libc::SIGKILL);
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("SKIP kill_tree_reaps_…: `setsid` unavailable; grandchild never formed");
            return;
        };
        let child = child.expect("child pid recorded once the grandchild is");

        assert!(
            is_running(gc),
            "the grandchild must be alive before teardown"
        );

        // The fix under test.
        kill_tree(&[root_pid], pgid, Duration::from_secs(3));

        let (root_alive, child_alive, gc_alive) =
            (is_running(root_pid), is_running(child), is_running(gc));

        // Belt-and-braces cleanup so a failing assertion never leaks the tree.
        if let Some(g) = pgid {
            unsafe {
                libc::killpg(g, libc::SIGKILL);
            }
        }
        RealProcs.signal(gc, libc::SIGKILL);
        RealProcs.reap(&[root_pid]);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!root_alive, "the root must be gone");
        assert!(!child_alive, "the child must be gone");
        assert!(
            !gc_alive,
            "the SIGTERM-deaf, setsid'd grandchild must be SIGKILLed within the bound"
        );
    }
}

/// #3643 — the supervised desktop against real processes and real sockets.
/// No X server is needed for these: the stand-ins are shell scripts, and the
/// "server that answers" is a listener in the test.
#[cfg(all(test, target_os = "linux"))]
mod supervise_tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    /// A directory of this test's own.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("roomler-vd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An executable stand-in for Xvfb. Each run appends its pid to
    /// `<dir>/runs`, then does `body`.
    fn stand_in(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-xvfb");
        let script = format!("#!/bin/sh\necho $$ >> \"{}/runs\"\n{body}\n", dir.display());
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    /// The pids of the stand-in's runs so far.
    fn runs(dir: &Path) -> Vec<i32> {
        std::fs::read_to_string(dir.join("runs"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// A plan around a stand-in: no Xorg, `true` for a window manager, and
    /// delays in milliseconds.
    fn plan(xvfb: String) -> Plan {
        Plan {
            cfg: Config {
                resolution: "640x480".into(),
                wm: "true".into(),
                startup: vec![],
            },
            xvfb,
            xorg: false,
            ready_timeout: Duration::from_millis(400),
            // Generous: on a loaded host the reader thread may run late, and
            // a test that asserts on the tail must not race it.
            tail_wait: Duration::from_secs(10),
            delay_floor: Duration::from_millis(20),
            delay_ceil: Duration::from_millis(80),
            stable_after: Duration::from_secs(60),
        }
    }

    /// `pid` is neither running nor a zombie: it was reaped.
    fn collected(pid: i32) -> bool {
        !Path::new(&format!("/proc/{pid}")).exists()
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_delays_double_to_the_ceiling_and_start_over() {
        let mut d = Backoff::new(Duration::from_secs(1), Duration::from_secs(300));
        let took: Vec<u64> = (0..11).map(|_| d.next().as_secs()).collect();
        assert_eq!(took, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(d.peek().as_secs(), 300);
        d.reset();
        assert_eq!(d.next().as_secs(), 1);
        // The daemon's own numbers: a second at first, five minutes at most.
        assert_eq!(RESTART_DELAY_FLOOR.as_secs(), 1);
        assert_eq!(RESTART_DELAY_CEIL.as_secs(), 300);
    }

    #[test]
    fn an_exit_is_described_by_its_signal() {
        assert_eq!(describe_exit(libc::CLD_EXITED, 1), "exit code 1");
        assert_eq!(
            describe_exit(libc::CLD_DUMPED, libc::SIGABRT),
            "signal 6 (aborted), core dumped"
        );
        assert_eq!(
            describe_exit(libc::CLD_KILLED, libc::SIGKILL),
            "signal 9 (killed)"
        );
        assert_eq!(describe_exit(libc::CLD_KILLED, 64), "signal 64");
    }

    /// "It answers" is an X11 connection setup answered, and every way of not
    /// answering is a `false` within the bound: no socket, a listener nobody
    /// serves, a server that hangs up, and one that accepts and says nothing —
    /// the field failure, a socket in front of a server still starting.
    #[test]
    fn up_means_the_server_answered_a_connection_setup() {
        let dir = scratch("answers");
        let quick = Duration::from_millis(300);

        assert!(!x_answers(&dir.join("no-such-socket"), quick));

        // Accepts, reads the setup, answers with a reply's fixed head.
        let good = dir.join("good");
        let listener = UnixListener::bind(&good).unwrap();
        let served = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut setup = [0u8; 12];
            conn.read_exact(&mut setup).unwrap();
            conn.write_all(&[1, 0, 11, 0, 0, 0, 0, 0]).unwrap();
            setup
        });
        assert!(x_answers(&good, Duration::from_secs(5)));
        let setup = served.join().unwrap();
        assert_eq!(setup[0], b'l', "a little-endian client");
        assert_eq!(u16::from_le_bytes([setup[2], setup[3]]), 11, "protocol 11");
        assert_eq!(&setup[6..10], &[0, 0, 0, 0], "no authorization");

        // Listening, and nobody accepts: the socket of a server still
        // starting. The answer is `false`, and it arrives.
        let silent = dir.join("silent");
        let _held = UnixListener::bind(&silent).unwrap();
        let asked = Instant::now();
        assert!(!x_answers(&silent, quick));
        assert!(
            asked.elapsed() < Duration::from_secs(3),
            "{:?}",
            asked.elapsed()
        );

        // Accepts and hangs up without a word.
        let rude = dir.join("rude");
        let listener = UnixListener::bind(&rude).unwrap();
        let hung_up = std::thread::spawn(move || drop(listener.accept().unwrap()));
        assert!(!x_answers(&rude, quick));
        hung_up.join().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server that dies while it starts, as the field one did: the start
    /// fails at once (not at the timeout), says what the server said, leaves
    /// the desktop down with that reason, and leaves no zombie.
    #[test]
    fn a_server_that_dies_starting_is_reported_with_its_last_words() {
        let dir = scratch("dies");
        let desk = Arc::new(Desktop::new(
            ":457".into(),
            Plan {
                ready_timeout: Duration::from_secs(30),
                ..plan(stand_in(
                    &dir,
                    "echo 'double free or corruption (out)' >&2\nexit 134",
                ))
            },
        ));
        let _stop = StopAtTheEnd(desk.clone());
        let asked = Instant::now();
        let no = desk.bring_up().err().expect("a dead server is no desktop");
        assert!(
            asked.elapsed() < Duration::from_secs(10),
            "waited out the timeout for a server that had exited: {:?}",
            asked.elapsed()
        );
        assert_eq!(no.why, "Xvfb exited while it was starting on display :457");
        // The server's own words go to the log, never into the reason.
        assert_eq!(no.said, "double free or corruption (out)");
        assert!(!no.hung);

        let st = desk.state();
        assert!(st.up.is_none() && st.tree.is_none(), "{st:?}");
        drop(st);
        let pids = runs(&dir);
        assert_eq!(pids.len(), 1, "{pids:?}");
        assert!(collected(pids[0]), "the dead server was left a zombie");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server that hangs in its start is given the timeout and no longer,
    /// then killed and collected: "not answering" must not leave it running
    /// beside the next one.
    #[test]
    fn a_server_that_never_answers_is_killed_at_the_timeout() {
        let dir = scratch("hangs");
        let desk = Arc::new(Desktop::new(
            ":458".into(),
            plan(stand_in(&dir, "exec sleep 300")),
        ));
        let _stop = StopAtTheEnd(desk.clone());
        let no = desk
            .bring_up()
            .err()
            .expect("a silent server is no desktop");
        assert_eq!(no.why, "Xvfb did not answer on display :458 within 400ms");
        assert!(no.hung, "a server that stays silent is one that hung");
        let pids = runs(&dir);
        assert_eq!(pids.len(), 1, "{pids:?}");
        assert!(collected(pids[0]), "the stuck server was left running");
        assert!(desk.state().tree.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The supervisor keeps trying a desktop that does not start, with every
    /// failed server collected and the reason kept for `down`, and the
    /// daemon's stop ends it: the supervisor leaves, and promptly.
    #[test]
    fn the_supervisor_keeps_trying_and_a_stop_ends_it() {
        let dir = scratch("retries");
        let desk = Arc::new(Desktop::new(
            ":459".into(),
            plan(stand_in(&dir, "echo 'no screens found' >&2\nexit 1")),
        ));
        let _stop = StopAtTheEnd(desk.clone());
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, None))
        };
        wait_for("three starts", || runs(&dir).len() >= 3);
        {
            let st = desk.state();
            assert!(st.up.is_none(), "{st:?}");
            assert_eq!(
                st.why_down,
                "Xvfb exited while it was starting on display :459"
            );
        }

        desk.stop();
        // A supervisor that missed the stop would start servers for ever, and
        // this would time out.
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let started = runs(&dir);
        for pid in &started {
            assert!(collected(*pid), "server {pid} was left a zombie");
        }
        let st = desk.state();
        assert!(
            st.stopping && st.tree.is_none() && st.unreaped.is_empty(),
            "{st:?}"
        );
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The display keeps its number between two of our servers, so another
    /// server can be on it when ours starts. An answer counts only when the
    /// display's lock names OUR server: here something answers throughout,
    /// and the wait still says no until the lock is ours.
    #[test]
    fn an_answer_from_someone_elses_server_is_not_ours() {
        let dir = scratch("foreign");
        let (sock, lock) = (dir.join("X9"), dir.join(".X9-lock"));
        // "Someone else's server": answers every connection setup.
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let answering = {
            let done = done.clone();
            std::thread::spawn(move || {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok((mut conn, _)) = listener.accept() {
                        let _ = conn.set_nonblocking(false);
                        let mut setup = [0u8; 12];
                        if conn.read_exact(&mut setup).is_ok() {
                            let _ = conn.write_all(&[1, 0, 11, 0, 0, 0, 0, 0]);
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            })
        };
        // "Our server": alive, and not the one holding the display.
        let mut ours = Command::new("sleep").arg("300").spawn().unwrap();
        let pid = ours.id() as i32;
        let quick = Duration::from_millis(300);

        assert!(
            x_answers(&sock, Duration::from_secs(5)),
            "the stand-in answers"
        );
        assert_eq!(
            wait_display_ready(&sock, &lock, pid, quick),
            Err(NotReady::Silent),
            "no lock at all"
        );
        std::fs::write(&lock, format!("{:>10}\n", pid + 1)).unwrap();
        assert_eq!(
            wait_display_ready(&sock, &lock, pid, quick),
            Err(NotReady::Silent),
            "the lock names another server"
        );
        std::fs::write(&lock, format!("{pid:>10}\n")).unwrap();
        assert_eq!(wait_display_ready(&sock, &lock, pid, quick), Ok(()));

        let _ = ours.kill();
        let _ = ours.wait();
        assert_eq!(
            wait_display_ready(&sock, &lock, pid, quick),
            Err(NotReady::Exited),
            "a server that is gone is not waited for"
        );
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        answering.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stale lock is removed only when it is OURS and its server is gone:
    /// never a live server's, and never one that names another pid.
    #[test]
    fn a_stale_lock_is_removed_only_when_it_is_ours() {
        let dir = scratch("lock");
        let lock = dir.join(".X9-lock");
        // A pid that is gone: a child, collected.
        let mut child = Command::new("true").spawn().unwrap();
        let gone = child.id() as i32;
        child.wait().unwrap();
        wait_for("the pid to leave /proc", || collected(gone));

        std::fs::write(&lock, format!("{:>10}\n", gone + 1)).unwrap();
        remove_stale_lock(&lock, gone);
        assert!(lock.exists(), "another server's lock was removed");

        // This test's own pid is alive, so its "server" is not gone yet.
        let alive = std::process::id() as i32;
        std::fs::write(&lock, format!("{alive:>10}\n")).unwrap();
        remove_stale_lock(&lock, alive);
        assert!(lock.exists(), "a live server's lock was removed");

        std::fs::write(&lock, format!("{gone:>10}\n")).unwrap();
        remove_stale_lock(&lock, gone);
        assert!(!lock.exists(), "our dead server's lock was left");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert!(left.is_empty(), "something was left beside it: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Xorg's directory is made, never adopted: private from its first
    /// moment, reused only when it is this user's and closed, and a link
    /// planted at its path is replaced without touching what it points at.
    #[test]
    fn the_xorg_directory_is_made_never_adopted() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("private");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let fresh = dir.join("fresh");
        private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);

        // Ours and closed: kept, with what is in it (the last server's log).
        std::fs::write(fresh.join("Xorg.log"), "kept").unwrap();
        private_dir(&fresh).unwrap();
        assert_eq!(
            std::fs::read_to_string(fresh.join("Xorg.log")).unwrap(),
            "kept"
        );

        // Ours and open to others: not trusted, made again.
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        assert!(!fresh.join("Xorg.log").exists());

        // A link where the directory should be: replaced by a directory, and
        // what it pointed at is neither entered nor changed.
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(elsewhere.join("xorg.conf"), "theirs").unwrap();
        let planted = dir.join("planted");
        std::os::unix::fs::symlink(&elsewhere, &planted).unwrap();
        private_dir(&planted).unwrap();
        assert!(std::fs::symlink_metadata(&planted).unwrap().is_dir());
        assert_eq!(mode(&planted), 0o700);
        assert_eq!(mode(&elsewhere), 0o755);
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("xorg.conf")).unwrap(),
            "theirs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole thing against a real Xvfb, which the CI Xvfb lane has
    /// (`ROOMLERD_TEST_X11=1`): the desktop comes up answering; its server is
    /// SIGKILLed, which leaves its lock and its socket behind; and the
    /// supervisor has the desktop back on the SAME display, on a new server,
    /// with the old one collected. Then the stop takes it all down.
    #[test]
    fn a_killed_x_server_comes_back_on_the_same_display() {
        if std::env::var("ROOMLERD_TEST_X11").as_deref() != Ok("1") {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return;
        }
        a_killed_server_comes_back(Server::Xvfb);
    }

    /// The same for Xorg with the dummy driver (FR-108 P2), which keeps its
    /// config and log in a directory the restart makes again. Run by hand,
    /// as root, where the driver is installed (`ROOMLERD_TEST_XORG=1`): CI's
    /// runner has no console for Xorg to open, and there the desktop falls
    /// back to Xvfb, which is the test above.
    #[test]
    fn a_killed_xorg_comes_back_as_xorg() {
        if std::env::var("ROOMLERD_TEST_XORG").as_deref() != Ok("1") {
            eprintln!(
                "ROOMLERD_TEST_XORG unset — skipping (a test for a host with the dummy driver)"
            );
            return;
        }
        a_killed_server_comes_back(Server::XorgDummy);
    }

    /// Stops the desktop when the test leaves, so a failed assertion does not
    /// leave an X server running on the host.
    struct StopAtTheEnd(Arc<Desktop>);
    impl Drop for StopAtTheEnd {
        fn drop(&mut self) {
            self.0.stop();
        }
    }

    fn a_killed_server_comes_back(kind: Server) {
        let display = pick_display(X11_UNIX_DIR);
        let n = display.trim_start_matches(':').to_string();
        let sock = Path::new(X11_UNIX_DIR).join(format!("X{n}"));
        let lock = Path::new("/tmp").join(format!(".X{n}-lock"));
        let desk = Arc::new(Desktop::new(
            display.clone(),
            Plan {
                xorg: kind == Server::XorgDummy,
                ready_timeout: READY_TIMEOUT,
                delay_floor: Duration::from_millis(100),
                ..plan("Xvfb".into())
            },
        ));
        let _stop = StopAtTheEnd(desk.clone());

        let first = desk.bring_up().expect("the X server comes up");
        let first_pid = first.pid;
        assert_eq!(desk.state().up, Some(kind));
        assert!(x_answers(&sock, Duration::from_secs(2)));
        assert!(
            std::fs::read_to_string(&lock).is_ok_and(|t| lock_names(&t, first_pid)),
            "the lock names the server we started"
        );

        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(first)))
        };
        // SAFETY: SIGKILL to a child of this test, which the supervisor has
        // not collected (it only waits for it).
        unsafe {
            libc::kill(first_pid, libc::SIGKILL);
        }

        let server_pid = |desk: &Desktop| {
            let st = desk.state();
            st.up
                .and_then(|_| st.tree.as_ref().and_then(|t| t.roots.first().copied()))
        };
        wait_for("the desktop to be back", || {
            server_pid(&desk).is_some_and(|pid| pid != first_pid)
        });
        let second_pid = server_pid(&desk).unwrap();
        assert_eq!(
            desk.state().up,
            Some(kind),
            "it came back on another server"
        );
        assert!(
            x_answers(&sock, Duration::from_secs(2)),
            "nothing answers on {display} after the restart"
        );
        assert!(
            std::fs::read_to_string(&lock).is_ok_and(|t| lock_names(&t, second_pid)),
            "the lock on {display} does not name the new server"
        );
        assert!(collected(first_pid), "the killed server was left a zombie");

        desk.stop();
        supervisor.join().expect("the supervisor left cleanly");
        assert!(collected(second_pid), "the stop left the server running");
        assert!(!x_answers(&sock, Duration::from_millis(200)));
        println!(
            "x11-real: a killed X server came back on {display} ({})",
            kind.name()
        );
    }
}
