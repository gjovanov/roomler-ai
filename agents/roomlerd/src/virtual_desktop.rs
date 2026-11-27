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
use tracing::{error, info, warn};

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

#[cfg(target_os = "linux")]
mod keeper;

/// #3665 — `roomlerd vd-keeper`, the desktop's keeper ([`keeper`]): the daemon
/// runs its own binary with this one hidden argument to start the window
/// manager and the startup apps. The exit code when this process is a keeper,
/// `None` otherwise.
///
/// Answered first in `main`, from raw argv: a keeper builds no runtime, reads
/// no config, opens no log and takes no lock. The argument is matched whole,
/// and nothing may follow it.
pub fn answer_keeper() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        if keeper::is_keeper_argv(std::env::args_os()) {
            return Some(keeper::run());
        }
    }
    None
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

/// #4100 — who starts an X client that Remote Apps (FR-56) asked for on a
/// display of this daemon's ([`start_x_client`]), and how that went.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) enum XStart {
    /// Remote Apps starts it itself, as before #4100: no keeper of this
    /// daemon's runs on that display.
    Direct,
    /// The desktop's keeper started it, as its child.
    Started,
    /// The desktop's keeper could not start it: the error its start
    /// returned, as Remote Apps' own start would have got it.
    Failed(std::io::Error),
    /// Not started, and why: the desktop is stopping or down, or its keeper
    /// refused the start or did not answer in time.
    Refused(String),
}

/// #4100 — now on `CLOCK_MONOTONIC`, in milliseconds: one clock for the daemon
/// and its keeper, who reads the deadline the daemon gave a start.
#[cfg(target_os = "linux")]
fn monotonic_ms() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime(2)` with a live out-pointer; Linux always has
    // `CLOCK_MONOTONIC`.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    }
    (now.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(now.tv_nsec as u64 / 1_000_000)
}

/// #4100 — the starts that wait for their keeper's answer, by id: the daemon
/// puts each one here before it writes it, and the thread that follows the
/// keeper's output ([`follow_keeper`]) hands each answer to its start. That
/// thread's end closes it: every start still waiting hears so at once, and a
/// later one is refused before it is written.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct Waiting {
    starts: std::collections::HashMap<u64, std::sync::mpsc::SyncSender<keeper::Answer>>,
    closed: bool,
}

/// [`Waiting`], shared by the daemon and the thread that follows the keeper.
/// Taken AFTER [`Desktop`]'s state, never before; that thread takes nothing
/// else.
#[cfg(target_os = "linux")]
type Answers = std::sync::Arc<std::sync::Mutex<Waiting>>;

/// #4100 — the slack past a start's deadline that the daemon waits for its
/// answer: the keeper starts nothing read after the deadline, so a start the
/// daemon gave up on never starts later, and one it answered just in time
/// still has its answer heard.
#[cfg(target_os = "linux")]
const X_CLIENT_SLACK: Duration = Duration::from_secs(1);

/// #4100 — what Remote Apps asks before it starts an X client on the daemon's
/// own display: [`start_x_client`] in the daemon, a test's desktop in a test.
/// The display, the program and its arguments.
#[cfg(target_os = "linux")]
pub(crate) type XStarter = std::sync::Arc<dyn Fn(&str, &str, &[&str]) -> XStart + Send + Sync>;

/// #4100 — Remote Apps' door to this daemon's own virtual desktop: who starts
/// `program` with `args` on `display`. [`XStart::Direct`] in a process that
/// started no desktop.
#[cfg(target_os = "linux")]
pub(crate) fn start_x_client(display: &str, program: &str, args: &[&str]) -> XStart {
    desktop().map_or(XStart::Direct, |desk| {
        desk.start_x_client(display, program, args)
    })
}

/// #3671 — how many times this daemon's own virtual desktop came back on a new
/// X server since it started: the X connection a part of the daemon made
/// under one generation is dead in the next, although `DISPLAY` names the
/// same display. The clipboard worker follows it (`clipboard::x11_follow`).
/// `0` in a process that started no desktop, and off Linux.
pub fn generation() -> u64 {
    #[cfg(target_os = "linux")]
    {
        desktop().map_or(0, |d| {
            d.generation.load(std::sync::atomic::Ordering::Acquire)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// #3918 — this daemon's own virtual desktop as an X client of it must see it,
/// in one read: the generation of the X server answering now ([`generation`])
/// and, while it is down, which down period it is in. Read under the lock that
/// publishes the desktop up, where a restart counts its generation
/// (`Desktop::bring_up_again`): a desktop seen up is seen with its new
/// server's generation. `None` in a process that started no desktop, and off
/// Linux.
pub fn x_server_now() -> Option<XServerNow> {
    #[cfg(target_os = "linux")]
    {
        desktop().map(|desk| desk.x_server_now())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// What [`x_server_now`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XServerNow {
    /// [`generation`].
    pub generation: u64,
    /// `Some(n)` while the desktop is down: the n-th time it went down since
    /// the daemon started, `0` before its first server came up. `None` while
    /// its X server answers.
    pub down: Option<u64>,
}

/// [`down`] as the `detail` of a session opened meanwhile: what happened to
/// the desktop, and that the agent is already starting it again, so the
/// person reconnects instead of reporting a fault.
pub fn down_detail() -> Option<String> {
    down().map(|why| down_detail_of(&why))
}

fn down_detail_of(why: &str) -> String {
    format!(
        "this device's virtual desktop is down: {why}. The daemon starts it again by itself; \
         reconnect in a moment"
    )
}

/// #3671 — the sessions told that the desktop went down: every one of
/// `registered` (the live ones) when a session on this device captures the
/// desktop (`plans`, [`crate::capture::plans_the_virtual_desktop`]), and none
/// otherwise, since the viewer keeps the notice up until the person connects
/// again. #3918 — who is MARKED reads no plan: every registered session, told
/// or not ([`sessions_told`]).
#[cfg(target_os = "linux")]
fn sessions_to_tell(
    plans: bool,
    registered: Vec<(bson::oid::ObjectId, crate::sleep_notice::ControlDc)>,
) -> Vec<(bson::oid::ObjectId, crate::sleep_notice::ControlDc)> {
    if plans { registered } else { Vec::new() }
}

/// #3671 kill switch `ROOMLERD_VIRTUAL_DESKTOP_TELL_SESSIONS=0`: an open
/// session is not told that the desktop went down, as before #3671. It then
/// shows its last picture until the person reconnects, and it has no input of
/// the desktop either (#3918: the supervisor marks every session open at the
/// exit before it reads this switch), as on master, where its injector was
/// dead. This switch turns off the notice only; `ROOMLERD_INPUT_X11_FOLLOW=0`
/// turns off what reads the marks (`input::x_follow::enabled`).
#[cfg(target_os = "linux")]
fn tell_sessions_enabled() -> bool {
    tunnel_core::env::flag("VIRTUAL_DESKTOP_TELL_SESSIONS", true)
}

/// #3671 — what the supervisor calls when the desktop goes down: the open
/// sessions that capture it get `rc:media-unavailable` with `backend_error`
/// and the sentence a session opened meanwhile gets ([`down_detail_of`]),
/// each through FR-80's notice. Who is told is read on the supervisor's own
/// thread (cheap on a desktop host: both automatic plans answer at once), and
/// the notices go out on the daemon's runtime, so the supervisor never waits.
///
/// #3918 — the rule: a session that has no picture of this desktop has no
/// input of it either while its input would reach the desktop's X server.
/// Every session registered at the exit was open at the exit, and any of them
/// may have been reading the X server that is gone, so every one is marked
/// (`input::mark_no_picture`) first, on this thread, before the device's plan
/// and the kill switch are read and before anything is spawned: the marks
/// stand before the restart can publish the desktop up. Where the plan is the
/// desktop's, they are also told. Where an explicit `drm_capture` or
/// `portal_capture` arm plans another capture, whose open may have fallen
/// through to the desktop's X server (the tell's known miss,
/// `capture::plans_the_virtual_desktop`), and with the switch, they are
/// marked in silence. What a mark costs is decided where the session's input
/// goes (`input::x_follow::Sessions`: nothing while a route or another
/// injector carries it) and what its shared pipeline reads (`media_share`:
/// a joiner is refused only while its capture may read an X server).
#[cfg(target_os = "linux")]
fn sessions_told(rt: tokio::runtime::Handle) -> DownTell {
    sessions_told_via(
        rt,
        crate::capture::sessions_capture_the_virtual_desktop,
        crate::peer::spawn_capture_unavailable_notice,
    )
}

/// #3671 — how [`sessions_told`] tells one session: FR-80's notice
/// (`peer::spawn_capture_unavailable_notice`), which spawns a task of its own
/// on the runtime it runs in. A test passes a stand-in.
#[cfg(target_os = "linux")]
type Notice =
    fn(bson::oid::ObjectId, crate::sleep_notice::ControlDc, crate::capture::CaptureUnavailable);

/// [`sessions_told`] with the device rule (`plans`) and the notice given, so
/// that a test drives the daemon's own hook from a thread in no runtime, as
/// the supervisor's thread is.
#[cfg(target_os = "linux")]
fn sessions_told_via(rt: tokio::runtime::Handle, plans: fn() -> bool, notice: Notice) -> DownTell {
    Box::new(move |why: &str| {
        let registered = crate::sleep_notice::sessions();
        for (session, _) in &registered {
            crate::input::mark_no_picture(*session);
        }
        if !tell_sessions_enabled() {
            return;
        }
        let sessions = sessions_to_tell(plans(), registered);
        if sessions.is_empty() {
            return;
        }
        let reason = crate::capture::CaptureUnavailable::virtual_desktop_down(down_detail_of(why));
        // The notice runs inside the daemon's runtime: called on the
        // supervisor's thread it would panic there, and that thread would
        // never start the desktop again. A runtime that has shut down drops
        // the task: nobody is told, and the daemon is leaving anyway.
        rt.spawn(async move {
            for (session, control_dc) in sessions {
                notice(session, control_dc, reason.clone());
            }
        });
    })
}

/// #3671 — the shared encoder pipelines (`media_share`) retired with the
/// desktop's X server when a session on this device captures the desktop
/// (`plans`, the rule [`sessions_to_tell`] reads), and none otherwise: a
/// session that does not capture the desktop keeps sharing its pipeline. How
/// many were retired.
#[cfg(target_os = "linux")]
fn retire_if(plans: bool) -> usize {
    if plans {
        crate::media_share::retire_all()
    } else {
        0
    }
}

/// #3671 — the daemon's [`Retire`]. The rule is read on the supervisor's
/// thread as the tell reads it (it asks no bus with the desktop requested),
/// and the retire takes a few short locks of `media_share`'s, never one of
/// the desktop's. It is not the tell, so the tell's kill switch leaves it on:
/// a session opened after the exit, or a reconnect, still gets a pump of its
/// own. With `ROOMLERD_SHARED_ENCODER=0` nothing is registered to retire.
#[cfg(target_os = "linux")]
fn pipelines_retired() -> Retire {
    pipelines_retired_via(crate::capture::sessions_capture_the_virtual_desktop)
}

/// [`pipelines_retired`] with the device rule given, so that a test drives
/// the daemon's own hook from a thread in no runtime.
#[cfg(target_os = "linux")]
fn pipelines_retired_via(plans: fn() -> bool) -> Retire {
    Box::new(move || {
        retire_if(plans());
    })
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
    /// The pids of the processes the daemon spawned directly: the X server,
    /// and the keeper or, without one, the window manager and the startup
    /// apps.
    roots: Vec<i32>,
    /// The desktop's keeper (#3665), one of `roots`: the parent of the window
    /// manager and the apps, and of everything they leave behind.
    keeper: Option<i32>,
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
fn kill_tree(roots: &[i32], pgid: Option<i32>, grace: Duration) -> bool {
    kill_tree_with(&mut RealProcs, roots, pgid, grace)
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
fn kill_tree_with(p: &mut dyn Procs, roots: &[i32], pgid: Option<i32>, grace: Duration) -> bool {
    use std::collections::{BTreeSet, HashMap};
    let self_pid = std::process::id() as i32;
    let is_root = |pid: i32| roots.contains(&pid);
    let group = pgid.filter(|g| *g > 1);

    // Pass 1 — SIGTERM. Snapshot BEFORE any signal, so the parent links are
    // still intact (nothing has been reparented to init yet).
    let snap = p.snapshot();
    let targets = select_targets(&snap, roots, pgid, self_pid);
    if targets.is_empty() {
        return true;
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
    let mut collected = p.reap(roots);
    while !collected && Instant::now() < reap_by {
        p.sleep(Duration::from_millis(20));
        collected = p.reap(roots);
    }
    collected
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

/// #3671 — after an exit, the first part of the restart delay waits for the
/// daemon's own stop before anything is said, told or killed. A service
/// manager's stop (and restart, and a package upgrade's restart) signals every
/// process of the unit at once, so the desktop's X server, or its keeper, can
/// exit before the daemon's teardown reaches [`Desktop::stop`]: about 200 ms
/// after the signal, since the run loop's arm for it sleeps 200 ms and then
/// calls [`teardown`]. A stop inside the grace finds the whole tree recorded
/// and takes it down itself (#1684); the supervisor says one line at INFO and
/// leaves. Carved out of the delay and never longer than it, so the desktop
/// comes back no later than it did before the grace: with a plan whose delay
/// is shorter (the tests' 20 ms), the grace is that delay.
///
/// ⚠️ Not a SIGTERM listener, on purpose. Tokio installs the process's SIGTERM
/// handler for good at the first `signal(SignalKind::terminate())`, and a
/// listener made later hears only the signals after it. One made here, when
/// the desktop starts, would come before the run loop's own (`app.rs`,
/// `terminate_signal`, the agent's only SIGTERM listener): a SIGTERM landing
/// between the two, which ends the daemon by the default action today, would
/// be swallowed, and the daemon would run on.
#[cfg(target_os = "linux")]
const STOP_GRACE: Duration = Duration::from_secs(1);

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
    /// #4099 — the window manager's restart delays, which the desktop's
    /// keeper gets in its job: the X server's numbers in the daemon. Its own,
    /// because the tests' window manager (`true`) exits at once, and the
    /// tests' 20 ms server delays would start it again every few
    /// milliseconds in every keeper test.
    wm_delay_floor: Duration,
    wm_delay_ceil: Duration,
    wm_stable_after: Duration,
    /// Who starts the window manager and the startup apps (#3665).
    clients: Clients,
    /// How long a start waits for the keeper to say it is ready.
    keeper_ready_timeout: Duration,
    /// Where a test reads the lines the keeper says after `ready`, as the
    /// daemon logs them; `None` in the daemon.
    keeper_lines: Option<KeeperLines>,
    /// #4100 — how long a Remote Apps start waits for the keeper's answer.
    x_client_wait: Duration,
}

/// The lines a keeper said, kept for a test.
#[cfg(target_os = "linux")]
type KeeperLines = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

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
            wm_delay_floor: RESTART_DELAY_FLOOR,
            wm_delay_ceil: RESTART_DELAY_CEIL,
            wm_stable_after: STABLE_AFTER,
            clients: if tunnel_core::env::flag("VIRTUAL_DESKTOP_KEEPER", true) {
                Clients::Keeper(KeeperCmd::daemon())
            } else {
                Clients::Direct
            },
            keeper_ready_timeout: KEEPER_READY_TIMEOUT,
            keeper_lines: None,
            x_client_wait: X_CLIENT_WAIT,
        }
    }
}

/// #4100 — how long a Remote Apps start waits for the keeper's answer. A
/// launch already waits 250 ms for its window, and runs on a blocking thread
/// of its own, so a few seconds cost nothing a person would notice.
#[cfg(target_os = "linux")]
const X_CLIENT_WAIT: Duration = Duration::from_secs(3);

/// How long a start waits for the keeper's `ready`. It starts nothing before
/// it, so this is a few milliseconds on a working host; a keeper that says
/// nothing in this time is killed, and the daemon starts the clients itself.
#[cfg(target_os = "linux")]
const KEEPER_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// The keeper's own grace, inside the daemon's: it has ended what it started
/// before the daemon's teardown looks again.
#[cfg(target_os = "linux")]
const KEEPER_GRACE: Duration = Duration::from_millis(1500);

/// The longest job the daemon sends a keeper: one atomic write to an empty
/// pipe (`PIPE_BUF`), so the write never waits for the keeper.
#[cfg(target_os = "linux")]
const KEEPER_JOB_MAX: usize = 4096;

/// Who starts the window manager and the startup apps.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
enum Clients {
    /// The desktop's keeper (#3665, [`keeper`]), run with this command. A
    /// keeper that cannot run falls back to [`Clients::Direct`] for that
    /// start, with a warning that says why.
    Keeper(KeeperCmd),
    /// The daemon itself, as before #3665: the kill switch,
    /// `ROOMLERD_VIRTUAL_DESKTOP_KEEPER=0`.
    Direct,
}

/// The command that runs a keeper.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct KeeperCmd {
    program: std::path::PathBuf,
    args: Vec<std::ffi::OsString>,
    arg0: Option<std::ffi::OsString>,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

#[cfg(target_os = "linux")]
impl KeeperCmd {
    /// The daemon's own binary, through `/proc/self/exe`: the kernel's link to
    /// the image this daemon runs, which still execs after a package upgrade
    /// replaced the file on disk (and keeps keeper and daemon one build).
    ///
    /// Its `argv[0]` is `roomlerd`, so `ps`, an administrator and an EDR
    /// console read `roomlerd vd-keeper`, whose process it is, rather than
    /// `/proc/self/exe vd-keeper`. The price: what matches a name through
    /// `argv[0]` or the executable (`pidof roomlerd`, `pgrep -f`, psmisc's
    /// `killall roomlerd`, which reads `argv[0]` because the 15-byte comm
    /// `roomlerd-keeper` may be cut) finds the keeper beside the daemon.
    /// `pgrep -x roomlerd`, which the docs give, finds the daemon alone.
    fn daemon() -> Self {
        Self {
            program: "/proc/self/exe".into(),
            args: vec![keeper::VERB.into()],
            arg0: Some("roomlerd".into()),
            env: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(arg0) = &self.arg0 {
            cmd.arg0(arg0);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
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
    /// #3918 — how many times the desktop went from up to down: the number of
    /// the down period it is in while `up` is `None` (`0` until its first
    /// server came up), for [`x_server_now`].
    downs: u64,
    /// Children a kill could not collect in its bound (SIGKILLed, and still
    /// stuck in the kernel). Asked for again at the next kill; never
    /// signalled again.
    unreaped: Vec<i32>,
    /// The write end of the keeper's input (#3665). Its end is the keeper's
    /// signal to take the whole desktop down: dropped by every kill of the
    /// tree, and closed by the kernel when the daemon dies however it dies.
    /// Created close-on-exec, so no other program holds it. Non-blocking
    /// once the job is out (#4100): each later write is one start of at most
    /// [`KEEPER_JOB_MAX`] bytes, whole or not at all, never a wait.
    lifeline: Option<std::process::ChildStdin>,
    /// #4100 — the starts written on `lifeline` that wait for the keeper's
    /// answer; kept and dropped with it.
    answers: Option<Answers>,
}

#[cfg(target_os = "linux")]
impl State {
    /// The desktop is down from here on; a desktop that was up starts a new
    /// down period ([`State::downs`]).
    fn went_down(&mut self) {
        if self.up.take().is_some() {
            self.downs += 1;
        }
    }
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
    /// The keeper is not tried again in this daemon's life: the clients are
    /// started directly, as before #3665.
    keeper_off: std::sync::atomic::AtomicBool,
    /// Starts in a row whose keeper did not say `ready` in time.
    keeper_slow: std::sync::atomic::AtomicU32,
    /// #3671 — what the open sessions are told when the desktop goes down,
    /// with why ([`sessions_told`]). Set once: by [`start`] on the daemon's
    /// runtime, by a test with a recorder. Unset, nobody is told.
    tell_down: std::sync::OnceLock<DownTell>,
    /// #3671 — what retires the shared encoder pipelines with the desktop's X
    /// server ([`pipelines_retired`]). Set once: by [`start`], by a test with a
    /// recorder. Unset, nothing is retired.
    retire: std::sync::OnceLock<Retire>,
    /// #3671 — how many times the desktop came back on a new X server
    /// ([`generation`]).
    generation: std::sync::atomic::AtomicU64,
    /// #4100 — the number of the last start of an X client Remote Apps sent
    /// a keeper ([`Desktop::start_x_client`]).
    x_starts: std::sync::atomic::AtomicU64,
}

/// #3671 — what [`supervise`] calls, on its own thread, once an exit's
/// [`STOP_GRACE`] has passed without a stop, with why. It must not block: the
/// daemon's spawns its work on the runtime ([`sessions_told`]).
#[cfg(target_os = "linux")]
type DownTell = Box<dyn Fn(&str) + Send + Sync>;

/// #3671 — what [`supervise`] calls, on its own thread, to retire the shared
/// encoder pipelines: where it tells the open sessions, and again once a
/// restarted desktop is up. It must not block ([`pipelines_retired`]).
#[cfg(target_os = "linux")]
type Retire = Box<dyn Fn() + Send + Sync>;

/// An X server this daemon started, while it answers.
#[cfg(target_os = "linux")]
struct XServer {
    pid: i32,
    kind: Server,
    tail: StderrTail,
    since: Instant,
    /// The keeper that started this server's clients, when one did.
    keeper: Option<i32>,
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
            keeper_off: std::sync::atomic::AtomicBool::new(false),
            keeper_slow: std::sync::atomic::AtomicU32::new(0),
            tell_down: std::sync::OnceLock::new(),
            retire: std::sync::OnceLock::new(),
            generation: std::sync::atomic::AtomicU64::new(0),
            x_starts: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stopping(&self) -> bool {
        self.state().stopping
    }

    /// [`x_server_now`] for this desktop, in one read under `st`: the lock
    /// that publishes the desktop up, and under which a restart counts its
    /// generation ([`Desktop::bring_up_again`]). `st` is held across at most
    /// one `Command::spawn` (`spawn_server`, `spawn_client`, `spawn_keeper`),
    /// never across a kill, a reap or a wait for a server, so a reader waits
    /// no longer than that.
    fn x_server_now(&self) -> XServerNow {
        let st = self.state();
        XServerNow {
            generation: self.generation.load(std::sync::atomic::Ordering::Acquire),
            down: st.up.is_none().then_some(st.downs),
        }
    }

    /// Record that the desktop is down, and why. `false` when the daemon is
    /// stopping instead: then the tree is `stop`'s, and the caller leaves.
    fn set_down(&self, why: String) -> bool {
        let mut st = self.state();
        if st.stopping {
            return false;
        }
        st.went_down();
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
            keeper: None,
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

    /// Spawn the keeper INTO the desktop's group `group` and record it as a
    /// root and as the keeper, under the same one lock as
    /// [`Self::spawn_server`]. `Ok(None)` when the daemon is stopping, or the
    /// tree is gone or is another one's.
    #[allow(clippy::zombie_processes)] // reaped by `kill_tree`, as the server is
    fn spawn_keeper(&self, cmd: &mut Command, group: i32) -> std::io::Result<Option<Child>> {
        let mut st = self.state();
        if st.stopping {
            return Ok(None);
        }
        let Some(tree) = st.tree.as_mut().filter(|t| t.pgid == Some(group)) else {
            return Ok(None);
        };
        join_group(cmd, Some(group));
        let child = cmd.spawn()?;
        let pid = child.id() as i32;
        tree.roots.push(pid);
        tree.keeper = Some(pid);
        Ok(Some(child))
    }

    /// A keeper that did not get its job: SIGKILL it, collect it, and take
    /// it out of the tree, all while no kill of the tree can run (`op`), so
    /// no later teardown signals its number. Every signal over the tree's
    /// roots is sent under `op`, and only a kill of the tree or this reaps a
    /// root, so the pid stays ours until it leaves the roots below. `st` is
    /// held only to read and to write the tree, never across the kill and its
    /// wait: `down()` and `switchable_display()` read it on capture paths.
    /// Nothing happens when the tree no longer lists it: the kill that took
    /// the tree owns it.
    fn discard_keeper(&self, pid: i32) {
        let _op = self.op.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .state()
            .tree
            .as_ref()
            .is_none_or(|t| t.keeper != Some(pid))
        {
            return;
        }
        // Our child, unreaped while the tree lists it, so its pid is ours.
        RealProcs.signal(pid, libc::SIGKILL);
        let by = Instant::now() + Duration::from_secs(1);
        let mut gone = RealProcs.reap(&[pid]);
        while !gone && Instant::now() < by {
            std::thread::sleep(Duration::from_millis(5));
            gone = RealProcs.reap(&[pid]);
        }
        let mut st = self.state();
        if let Some(tree) = st.tree.as_mut() {
            tree.roots.retain(|r| *r != pid);
            tree.keeper = None;
        }
        if !gone {
            st.unreaped.push(pid);
        }
    }

    /// Take the recorded tree and bring it down: SIGTERM, the grace, SIGKILL,
    /// and only then the reap ([`kill_tree_with`]'s two rules). Whoever comes
    /// second waits for the first and finds nothing recorded.
    fn kill_recorded_tree(&self) {
        let _op = self.op.lock().unwrap_or_else(|e| e.into_inner());
        let (handle, lifeline, mut waiting) = {
            let mut st = self.state();
            // #4100 — no start is written once the lifeline is gone; one
            // waiting for its answer hears the keeper's end from the thread
            // that follows it.
            st.answers = None;
            (
                st.tree.take(),
                st.lifeline.take(),
                std::mem::take(&mut st.unreaped),
            )
        };
        // Children an earlier kill could not collect in its bound: ask again.
        waiting.retain(|pid| !RealProcs.reap(&[*pid]));
        // The keeper's input ends here, so it takes down what it started at
        // once, inside the grace below; its own exit then ends that grace
        // early (#3665).
        drop(lifeline);
        if let Some(handle) = handle
            && !kill_tree(&handle.roots, handle.pgid, TEARDOWN_GRACE)
        {
            // `kill_tree` reaps within a bound, and this time the bound
            // passed. A child it left would stay a zombie for the daemon's
            // life, now that the daemon lives on after a kill: nothing else
            // ever waits for it. Keep the ones still there, for the next kill.
            waiting.extend(
                handle
                    .roots
                    .into_iter()
                    .filter(|pid| !RealProcs.reap(&[*pid])),
            );
        }
        self.state().unreaped = waiting;
    }

    /// Clear the lock a server of ours left on the display, once that server
    /// has been collected. One the kill could not collect is still dying, and
    /// its lock is still its own.
    fn clear_lock_of(&self, pid: i32) {
        if !self.state().unreaped.contains(&pid) {
            remove_stale_lock(&x_files(&self.display).1, pid);
        }
    }

    /// The graceful stop: nothing is started from here on, the supervisor is
    /// woken to leave, and the tree comes down.
    fn stop(&self) {
        {
            let mut st = self.state();
            st.stopping = true;
            st.went_down();
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

    /// #3671 — tell the open sessions why the desktop went down, through the
    /// hook [`start`] set (a test's recorder); nobody when none is set.
    fn tell(&self, why: &str) {
        if let Some(tell) = self.tell_down.get() {
            tell(why);
        }
    }

    /// #3671 — retire the shared encoder pipelines through the hook [`start`]
    /// set (a test's recorder); nothing when none is set.
    fn retire_pipelines(&self) {
        if let Some(retire) = self.retire.get() {
            retire();
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
        self.bring_up_as(false)
    }

    /// [`Desktop::bring_up`] after an exit: its server is a new generation
    /// ([`generation`], #3671), counted under the lock that publishes the
    /// desktop up and before it (#3918). So whoever finds the desktop up finds
    /// the new server's generation, and an X client of the old server never
    /// passes for one of the new.
    fn bring_up_again(&self) -> std::result::Result<XServer, NoDesktop> {
        self.bring_up_as(true)
    }

    fn bring_up_as(&self, again: bool) -> std::result::Result<XServer, NoDesktop> {
        let cfg = &self.plan.cfg;
        // An Xorg that hung once is no longer the server that will serve.
        let try_xorg =
            self.plan.xorg && !self.xorg_failed.load(std::sync::atomic::Ordering::Relaxed);
        preflight(cfg, &self.plan.xvfb, try_xorg)
            .map_err(|e| NoDesktop::because(format!("{e:#}")))?;
        let (w, h) = parse_resolution(&cfg.resolution);
        let mut x = self.start_server(try_xorg, w, h)?;
        info!(display = self.display.as_str(), resolution = %cfg.resolution, server = ?x.kind, "virtual-desktop: X server up");
        x.keeper = self.start_clients();
        let pgid = {
            let mut st = self.state();
            if st.stopping {
                return Err(NoDesktop::because("the daemon is stopping"));
            }
            if again {
                self.generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
            st.up = Some(x.kind);
            st.why_down.clear();
            st.tree.as_ref().and_then(|t| t.pgid)
        };
        let clients = if x.keeper.is_some() {
            "vd-keeper"
        } else {
            "daemon"
        };
        info!(display = self.display.as_str(), wm = %cfg.wm, apps = cfg.startup.len(), ?pgid, vd_keeper = ?x.keeper, started_by = clients, "virtual-desktop ready");
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
                keeper: None,
            }),
            Err(not_ready) => {
                self.kill_recorded_tree();
                self.clear_lock_of(pid);
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

    /// The window manager and the startup apps, on a server that answers:
    /// by the desktop's keeper (#3665), or by the daemon itself where the
    /// kill switch is set or no keeper can run here. The keeper's pid when it
    /// started them.
    fn start_clients(&self) -> Option<i32> {
        let cfg = &self.plan.cfg;
        let keeper = match &self.plan.clients {
            Clients::Keeper(cmd) => {
                if self.keeper_off.load(std::sync::atomic::Ordering::Relaxed) {
                    warn!(
                        display = self.display.as_str(),
                        "virtual-desktop: no vd-keeper for the rest of this daemon's life, after two keepers in a row exited by themselves within a minute of their desktop's start, or two were not ready in time with none ready between them — the daemon starts the window manager and the apps, and what they leave behind is out of the teardown's reach (#3665)"
                    );
                    None
                } else {
                    match self.start_keeper(cmd) {
                        Ok(keeper) => keeper,
                        // A stop that came while the keeper started: nothing
                        // starts now, and the keeper it ended is no news
                        // (condition 9, review R2-7).
                        Err(why) if self.state().stopping => {
                            info!(display = self.display.as_str(), %why, "virtual-desktop: the desktop is stopping, and its vd-keeper with it");
                            return None;
                        }
                        Err(why) => {
                            warn!(display = self.display.as_str(), %why, "virtual-desktop: no vd-keeper — the daemon starts the window manager and the apps, and what they leave behind is out of the teardown's reach (#3665)");
                            None
                        }
                    }
                }
            }
            Clients::Direct => None,
        };
        if keeper.is_none() {
            self.start_clients_directly();
        }
        // An app that maps while openbox is still starting can stay unmanaged,
        // and so invisible, until X traffic next wakes it (`WM_NUDGE_AT_MS`).
        if !cfg.startup.is_empty() {
            nudge_wm_after_startup(&self.display);
        }
        keeper
    }

    /// Start the keeper and hand it the clients: `Ok(Some(pid))` once it
    /// said it is ready and has its job, `Ok(None)` when the daemon is
    /// stopping (nothing is started then, either way), `Err` with the reason
    /// it cannot run. A keeper that did not get its job is gone when this
    /// returns, and it started nothing: the job is sent only after `ready`.
    /// Two starts in a row whose keeper was not ready in time turn the keeper
    /// off for this daemon's life, as two early exits do.
    fn start_keeper(&self, kc: &KeeperCmd) -> std::result::Result<Option<i32>, String> {
        use std::io::Write as _;
        let cfg = &self.plan.cfg;
        let group = {
            let st = self.state();
            match st.tree.as_ref() {
                None => return Ok(None),
                Some(t) => t.pgid.filter(|g| *g > 1),
            }
        };
        let Some(group) = group else {
            return Err("the desktop has no process group of its own".into());
        };
        let job = keeper::Job {
            display: self.display.clone(),
            group,
            wm: cfg.wm.clone(),
            apps: cfg.startup.clone(),
            grace_ms: KEEPER_GRACE.as_millis() as u64,
            wm_delay_floor_ms: self.plan.wm_delay_floor.as_millis() as u64,
            wm_delay_ceil_ms: self.plan.wm_delay_ceil.as_millis() as u64,
            wm_stable_after_ms: self.plan.wm_stable_after.as_millis() as u64,
        }
        .line();
        if job.len() > KEEPER_JOB_MAX {
            return Err(format!(
                "its job is {} bytes, more than {KEEPER_JOB_MAX}",
                job.len()
            ));
        }
        let mut cmd = kc.command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = match self.spawn_keeper(&mut cmd, group) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(e) => return Err(format!("it could not be started: {e}")),
        };
        let pid = child.id() as i32;
        let input = child.stdin.take();
        let answers = Answers::default();
        let ready = follow_keeper(
            child.stderr.take(),
            pid,
            self.plan.keeper_lines.clone(),
            answers.clone(),
        );
        // The job goes out only after `ready`, so a keeper given up on here
        // never had one and started nothing; and it is stopped before its
        // input ends, never after.
        let said =
            ready.and_then(
                |ready| match ready.recv_timeout(self.plan.keeper_ready_timeout) {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(why)) => Err(format!("it refused: {why}")),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // As slow twice in a row as two early exits (condition 11):
                        // a host whose keeper never makes it in time would otherwise
                        // pay this wait at every start of its desktop.
                        let slow = self
                            .keeper_slow
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        if slow >= KEEPER_STRIKES {
                            self.keeper_off
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        Err(format!(
                            "it did not say it was ready within {:?}",
                            self.plan.keeper_ready_timeout
                        ))
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        Err("it exited before it said it was ready".to_string())
                    }
                },
            );
        if let Err(why) = said {
            self.discard_keeper(pid);
            drop(input);
            return Err(why);
        }
        self.keeper_slow
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let Some(mut input) = input else {
            self.discard_keeper(pid);
            return Err("it has no input".into());
        };
        if let Err(e) = input.write_all(job.as_bytes()) {
            // Not a whole line, so no job: it started nothing.
            self.discard_keeper(pid);
            drop(input);
            return Err(format!("its job could not be sent: {e}"));
        }
        // #4100 — every later write is a start of Remote Apps' (one line of at
        // most `KEEPER_JOB_MAX`, whole or not at all): a keeper that does not
        // read refuses it, and never holds the daemon.
        keeper::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&input));
        // From here the keeper's exit is the desktop's failure (K1), never a
        // direct start on top of what it started. The lifeline is kept only
        // for the tree it belongs to: a stop that took the tree meanwhile
        // also took the keeper, and the input dropped here ends whatever is
        // left of it.
        let mut st = self.state();
        if !st.stopping && st.tree.as_ref().is_some_and(|t| t.keeper == Some(pid)) {
            st.lifeline = Some(input);
            st.answers = Some(answers);
            Ok(Some(pid))
        } else {
            Ok(None)
        }
    }

    /// The window manager and the startup apps, as the daemon's own children,
    /// in the desktop's group: what the daemon did before #3665. Each gets
    /// `/dev/null` on stdin too, as the keeper's children do.
    fn start_clients_directly(&self) {
        let cfg = &self.plan.cfg;
        let mut wm_cmd = Command::new(&cfg.wm);
        wm_cmd
            .env("DISPLAY", &self.display)
            .stdin(Stdio::null())
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
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            with_wslg_pulse(&mut app_cmd);
            if let Err(e) = self.spawn_client(&mut app_cmd) {
                warn!(%app, %e, "virtual-desktop: startup app failed to launch");
            }
        }
    }

    /// #4100 — who starts the X client `program` with `args` that Remote Apps
    /// asked for on `display` ([`start_x_client`]), and how it went.
    ///
    /// Where this desktop's keeper runs, the keeper starts it, as its child
    /// in the desktop's group, so that its exit is collected and the
    /// desktop's teardown reaches it and whatever it leaves behind. Nothing
    /// falls back to a direct start there: a start the keeper cannot take is
    /// refused. Remote Apps starts the client itself, as before, on another
    /// display, under the kill switch, after a keeper this daemon turned off,
    /// and while the tree has no keeper (it refused, or was not ready in
    /// time: the window manager was started directly too).
    ///
    /// Decided under the state lock, and the start written under it, never a
    /// wait (the lifeline is non-blocking, and a start of at most
    /// [`KEEPER_JOB_MAX`] bytes goes whole or not at all); the answer is
    /// awaited outside it, for the start's wait and [`X_CLIENT_SLACK`].
    fn start_x_client(&self, display: &str, program: &str, args: &[&str]) -> XStart {
        if display != self.display
            || !matches!(self.plan.clients, Clients::Keeper(_))
            || self.keeper_off.load(std::sync::atomic::Ordering::Relaxed)
        {
            return XStart::Direct;
        }
        let wait = self.plan.x_client_wait;
        let id = self
            .x_starts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let line = keeper::Start {
            id,
            argv: std::iter::once(program)
                .chain(args.iter().copied())
                .map(String::from)
                .collect(),
            deadline_ms: monotonic_ms().saturating_add(wait.as_millis() as u64),
        }
        .line();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let answers = {
            let st = self.state();
            if st.stopping {
                return XStart::Refused("the daemon is stopping".into());
            }
            // D1 — no X server to start it onto: a client started now would
            // die at once, or land on the next server, outside any tree.
            if st.up.is_none() {
                return XStart::Refused(format!(
                    "this device's virtual desktop is down: {}. The daemon starts it again by \
                     itself; launch it again in a moment",
                    st.why_down
                ));
            }
            if st.tree.as_ref().and_then(|t| t.keeper).is_none() {
                return XStart::Direct;
            }
            if line.len() > KEEPER_JOB_MAX {
                return XStart::Refused(format!(
                    "its start is {} bytes, more than {KEEPER_JOB_MAX} bytes (one write to the desktop's keeper)",
                    line.len()
                ));
            }
            let (Some(lifeline), Some(answers)) = (st.lifeline.as_ref(), st.answers.as_ref())
            else {
                return XStart::Refused("the desktop's keeper has no input".into());
            };
            {
                let mut waiting = answers.lock().unwrap_or_else(|e| e.into_inner());
                if waiting.closed {
                    return XStart::Refused("the desktop's keeper is gone".into());
                }
                waiting.starts.insert(id, tx);
            }
            let fd = std::os::fd::AsRawFd::as_raw_fd(lifeline);
            // SAFETY: `write(2)` from a live buffer of `line.len()` bytes to
            // the daemon's end of the keeper's input. It is non-blocking, and
            // a write of at most `PIPE_BUF` bytes is whole or fails (EAGAIN
            // on a full pipe, EPIPE once the keeper is gone; SIGPIPE is
            // ignored in the daemon).
            let n = unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
            if n != line.len() as isize {
                let e = std::io::Error::last_os_error();
                answers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .starts
                    .remove(&id);
                return XStart::Refused(format!("the desktop's keeper could not take it: {e}"));
            }
            answers.clone()
        };
        let answer = match rx.recv_timeout(wait + X_CLIENT_SLACK) {
            Ok(answer) => answer,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Given up under the lock its answer is handed over under: an
                // answer that came in between is in the channel already.
                let gave_up = answers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .starts
                    .remove(&id)
                    .is_some();
                match rx.try_recv() {
                    Ok(answer) if !gave_up => answer,
                    _ => {
                        return XStart::Refused(format!(
                            "the desktop's keeper did not answer within {:?}",
                            wait + X_CLIENT_SLACK
                        ));
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return XStart::Refused("the desktop's keeper is gone".into());
            }
        };
        match answer {
            keeper::Answer::Started { .. } => XStart::Started,
            keeper::Answer::Failed {
                os_error,
                kind,
                text,
            } => XStart::Failed(match os_error {
                Some(code) => std::io::Error::from_raw_os_error(code),
                None if kind == "NotFound" => {
                    std::io::Error::new(std::io::ErrorKind::NotFound, text)
                }
                None => std::io::Error::other(text),
            }),
            keeper::Answer::Refused { why } => {
                XStart::Refused(format!("the desktop's keeper refused it: {why}"))
            }
        }
    }
}

/// Follow a keeper's stderr on a thread of its own: its first `ready` or
/// `refused: <why>` line goes to the receiver this returns, its answer to a
/// start of Remote Apps' to that start in `answers` (#4100), and every other
/// line to the log, as a warning where [`keeper_line`] says so (a `warn: `
/// line, or a `refused: ` line after `ready`). The thread ends with the
/// keeper's stderr, and closes `answers` as it ends.
#[cfg(target_os = "linux")]
fn follow_keeper(
    stderr: Option<std::process::ChildStderr>,
    pid: i32,
    copy: Option<KeeperLines>,
    answers: Answers,
) -> std::result::Result<std::sync::mpsc::Receiver<std::result::Result<(), String>>, String> {
    use std::io::BufRead as _;
    let stderr = stderr.ok_or("it has no stderr")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("vd-keeper-log".into())
        .spawn(move || {
            let mut tx = Some(tx);
            for line in std::io::BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.is_some() {
                    let answer = if line == "ready" {
                        Some(Ok(()))
                    } else {
                        line.strip_prefix("refused: ")
                            .map(|why| Err(why.to_string()))
                    };
                    if let Some(answer) = answer {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(answer);
                        }
                        continue;
                    }
                }
                if let Some(copy) = &copy {
                    copy.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(line.clone());
                }
                if let Some((id, answer)) = keeper::Answer::parse(&line) {
                    answered(&answers, pid, id, answer);
                    continue;
                }
                match keeper_line(&line) {
                    (true, said) => warn!(vd_keeper = pid, "virtual-desktop vd-keeper: {said}"),
                    (false, said) => info!(vd_keeper = pid, "virtual-desktop vd-keeper: {said}"),
                }
            }
            // The keeper's output ended, and with it every answer still to
            // come: each start that waits hears so now (its sender dropped),
            // and none is written from here on.
            let mut waiting = answers.lock().unwrap_or_else(|e| e.into_inner());
            waiting.closed = true;
            waiting.starts.clear();
        })
        .map_err(|e| format!("its output could not be followed: {e}"))?;
    Ok(rx)
}

/// #4100 — hand the keeper's answer to start `id` to the start that waits
/// for it, under the lock the daemon gives the start up under (never a
/// wait: one answer a start, into a channel of one). An answer that comes
/// after its start gave up is logged: one that started a client warns, since
/// that client runs although its launch failed.
#[cfg(target_os = "linux")]
fn answered(
    answers: &Answers,
    keeper_pid: i32,
    id: u64,
    answer: std::result::Result<keeper::Answer, String>,
) {
    let answer = answer.unwrap_or_else(|why| keeper::Answer::Refused { why });
    let mut waiting = answers.lock().unwrap_or_else(|e| e.into_inner());
    match waiting.starts.remove(&id) {
        Some(start) => {
            let _ = start.try_send(answer);
        }
        None => {
            drop(waiting);
            match answer {
                keeper::Answer::Started { pid } => warn!(
                    vd_keeper = keeper_pid,
                    "virtual-desktop vd-keeper: Remote Apps' start {id} began client {pid} after the daemon stopped waiting for it, so its launch was reported failed (#4100)"
                ),
                other => info!(
                    vd_keeper = keeper_pid,
                    "virtual-desktop vd-keeper: an answer to Remote Apps' start {id} after the daemon stopped waiting for it: {other:?}"
                ),
            }
        }
    }
}

/// How the daemon logs a keeper's line after its first answer: whether it
/// warns, and what it says. A `warn: ` line warns, and so does a refusal
/// after `ready` (its job): the supervisor's warning that follows names only
/// the exit code (review R2-5). The rest is information, `info: ` dropped.
#[cfg(target_os = "linux")]
fn keeper_line(line: &str) -> (bool, &str) {
    if let Some(said) = line.strip_prefix("warn: ") {
        (true, said)
    } else if line.starts_with("refused: ") {
        (true, line)
    } else {
        (false, line.strip_prefix("info: ").unwrap_or(line))
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
    // #3671 — the shared pipelines are retired with the desktop's X server
    // (no runtime needed: a few locks on the supervisor's thread).
    let _ = desk.retire.set(pipelines_retired());
    // #3671 — the open sessions are told when the desktop goes down. `start`
    // runs on the daemon's runtime; without one, nobody is told.
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            let _ = desk.tell_down.set(sessions_told(rt));
        }
        Err(_) => info!(
            "virtual-desktop: no runtime here — an open session is not told when the desktop goes down"
        ),
    }
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
    let mut strikes = KeeperStrikes::default();
    loop {
        // What is left of the delay after an exit's grace; `None`: a whole one.
        let mut rest = None;
        if let Some(x) = running.take() {
            let gone = wait_desktop_exit(&x);
            let up_for = x.since.elapsed();
            let why = match &gone {
                Gone::Server(exit) => format!("its X server ({}) exited: {exit}", x.kind.name()),
                Gone::Keeper(exit) => format!("its keeper process (vd-keeper) exited: {exit}"),
            };
            if !desk.set_down(why.clone()) {
                // A deliberate stop killed it; the tree is `stop`'s.
                return;
            }
            crate::desktop_brief::refresh_soon();
            if up_for >= desk.plan.stable_after {
                delays.reset();
            }
            let delay = delays.next();
            let grace = STOP_GRACE.min(delay);
            // #3671 — an exit the daemon's own stop caused. Nothing of the tree
            // is touched while this waits, and the keeper's input stays open,
            // so the stop finds everything recorded. The wait is the one
            // `stop` wakes: it never adds to a stop.
            if desk.wait_unless_stopping(grace) {
                let (what, exit) = match &gone {
                    Gone::Server(exit) => ("the X server", exit),
                    Gone::Keeper(exit) => ("the desktop's keeper (vd-keeper)", exit),
                };
                info!(
                    display = desk.display.as_str(),
                    server = x.kind.name(),
                    %exit,
                    "virtual-desktop: {what} exited as the daemon stops"
                );
                return;
            }
            rest = Some(delay.saturating_sub(grace));
            // #3671 — from here on, a session opened while the desktop is down
            // runs a pump of its own, and its open is refused with the sentence
            // below, instead of following a capture of the server that went,
            // untold. Before the tell, so a session opened between the two is
            // refused by its own open.
            desk.retire_pipelines();
            // #3671 — the open sessions that capture this desktop, told why,
            // with the sentence a session opened now gets.
            desk.tell(&why);
            // The window manager and the apps lost their display. Bring them
            // down so the next start is clean and none of them stays a zombie;
            // this is also what reaps the server.
            desk.kill_recorded_tree();
            desk.clear_lock_of(x.pid);
            match gone {
                Gone::Server(exit) => warn!(
                    display = desk.display.as_str(),
                    server = x.kind.name(),
                    %exit,
                    up_for_s = up_for.as_secs(),
                    said = %x.tail.last_lines(desk.plan.tail_wait),
                    "virtual-desktop: the X server exited — the desktop is down until it is started again"
                ),
                Gone::Keeper(exit) => {
                    // #3665 — the keeper is the desktop's as much as its
                    // server is: without it, what the apps leave behind is out
                    // of reach again. So its exit takes the desktop down and
                    // starts it again, as a server's does.
                    warn!(
                        display = desk.display.as_str(),
                        vd_keeper = ?x.keeper,
                        %exit,
                        up_for_s = up_for.as_secs(),
                        "virtual-desktop: the desktop's keeper exited — the desktop is down until it is started again"
                    );
                    if strikes.exited(up_for, desk.plan.stable_after) {
                        desk.keeper_off
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        warn!(
                            display = desk.display.as_str(),
                            "virtual-desktop: the desktop's keeper (vd-keeper) exited by itself twice soon after its desktop started — for the rest of this daemon's life the daemon starts the window manager and the apps itself (#3665)"
                        );
                    }
                }
            }
            if up_for >= desk.plan.stable_after {
                strikes.reset();
            }
        }
        let delay = rest.unwrap_or_else(|| delays.next());
        if desk.wait_unless_stopping(delay) {
            return;
        }
        // #3671 — the new server's generation is counted as it is published up
        // (`bring_up_again`), so whoever reads it after this line finds it.
        match desk.bring_up_again() {
            Ok(x) => {
                // #3671 — whatever is registered now was registered before the
                // new server answered, the refused pumps of the down period
                // included: the next session of each profile opens a capture of
                // its own on the new server.
                desk.retire_pipelines();
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

/// #3665, condition 11 — a keeper that exits by itself soon after its desktop
/// started, twice in a row, is not tried again in this daemon's life: the
/// keeper path may never leave a host with less of a desktop than the daemon
/// gave it before. The next daemon tries it again.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct KeeperStrikes {
    early: u32,
}

/// Early keeper exits in a row that turn the keeper off.
#[cfg(target_os = "linux")]
const KEEPER_STRIKES: u32 = 2;

#[cfg(target_os = "linux")]
impl KeeperStrikes {
    /// A keeper exited after its desktop ran for `up_for`. `true` when that
    /// makes [`KEEPER_STRIKES`] early exits in a row.
    fn exited(&mut self, up_for: Duration, stable_after: Duration) -> bool {
        if up_for >= stable_after {
            self.early = 0;
            return false;
        }
        self.early += 1;
        self.early >= KEEPER_STRIKES
    }

    /// A desktop ran long enough: the next early exit is the first again.
    fn reset(&mut self) {
        self.early = 0;
    }
}

/// How a desktop's run ended.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Gone {
    /// Its X server exited; how.
    Server(String),
    /// Its keeper exited first (#3665); how.
    Keeper(String),
}

/// Block until the desktop's X server, or its keeper when it has one, has
/// exited, and say which and how. Neither is reaped (`WNOWAIT`), for the
/// reason [`wait_exit`] gives.
///
/// With a keeper the wait is on the desktop's process group (`P_PGID`). The
/// daemon's only children in that group are the server and the keeper: the
/// keeper is the parent of everything else on the desktop, and none of the
/// daemon's other children (exec and SSH sessions, the encoder probe) is ever
/// placed in it, so the wait can only answer for those two. Should a child of
/// the daemon's that is neither ever be in the group, it is not this wait's to
/// take: it is left unreaped for its own waiter, one error says so, and from
/// then on the wait is on the server alone. With `WNOWAIT` it would otherwise
/// answer every wait at once, and the supervisor would spin a core, as root.
#[cfg(target_os = "linux")]
fn wait_desktop_exit(x: &XServer) -> Gone {
    let Some(keeper) = x.keeper else {
        return Gone::Server(wait_exit(x.pid));
    };
    // The server leads the desktop's group, so its pid is the group's id, and
    // a keeper is only ever started into that group (`Desktop::spawn_keeper`).
    let group = x.pid;
    loop {
        // SAFETY: `siginfo_t` is plain data that `waitid` fills in; all-zero
        // is a valid value for it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `waitid(2)` on our children in one group, with a live
        // out-pointer. `WNOWAIT` leaves whichever it reports waitable.
        let rc = unsafe {
            libc::waitid(
                libc::P_PGID,
                group as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: `waitid` returned 0 for a child that exited, so this is
            // a SIGCHLD-shaped `siginfo_t`: `si_pid` and `si_status` are set.
            let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
            let how = describe_exit(info.si_code, status);
            if pid == x.pid {
                return Gone::Server(how);
            }
            if pid == keeper {
                return Gone::Keeper(how);
            }
            error!(
                pid,
                group,
                server = x.pid,
                vd_keeper = keeper,
                "virtual-desktop: a child of the daemon that the desktop did not start is in the desktop's process group — it is left to its own waiter, and the supervisor waits for the X server alone from now on"
            );
            break;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => {}
            // Both collected by a teardown that ran meanwhile.
            Some(libc::ECHILD) => return Gone::Server("already collected".into()),
            _ => break,
        }
    }
    Gone::Server(wait_exit(x.pid))
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
/// ⚠️ Call it only for a child that has been COLLECTED
/// ([`Desktop::clear_lock_of`]): one that is still dying keeps its lock. And
/// two guards here, since the display's names are shared with every X server
/// on the host. A lock that names any other pid is not touched. And it is
/// moved aside in one step and read again before it is deleted: between the
/// first read and an unlink, another server could have cleared the stale lock
/// itself and put its own there, and the unlink would have taken that one.
/// What the rename took, if it is not ours after all, is put back.
#[cfg(target_os = "linux")]
fn remove_stale_lock(lock: &Path, dead_pid: i32) {
    if !lock_is(lock, dead_pid) {
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
/// could push a real last word out of it. Only the report's own three shapes
/// are dropped: a line that names `xkbcomp` beside an error is the server's.
fn keymap_chatter(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_start();
    text.starts_with('>')
        || text.starts_with("The XKEYBOARD keymap compiler (xkbcomp) reports:")
        || text.starts_with("Errors from xkbcomp are not fatal to the X server")
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
             (aborted), core dumped. The daemon starts it again by itself; reconnect in a moment"
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
        // A real error that names the compiler is the server's, and is kept.
        let broken = StderrTail::default();
        broken.push(b"(EE) Error compiling keymap (server-99) executing '\"/usr/bin/xkbcomp\"'\n");
        broken.close();
        assert!(
            broken
                .last_lines(Duration::ZERO)
                .contains("Error compiling keymap"),
            "an error line was dropped as chatter"
        );

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
    /// now belongs to somebody else's process. `born_at_end` lists processes
    /// that appear as another one ends: an orphan's child that a subreaper
    /// adopts in the middle of a teardown.
    #[cfg(target_os = "linux")]
    struct MockProcs {
        table: Vec<ProcInfo>,
        running: std::collections::BTreeSet<i32>,
        deaf: std::collections::BTreeSet<i32>,
        recycled: std::collections::BTreeSet<i32>,
        born_at_end: Vec<(i32, ProcInfo)>,
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
                born_at_end: Vec::new(),
                events: Vec::new(),
            }
        }
        fn deliver(&mut self, pid: i32, sig: i32) {
            if sig == libc::SIGKILL || !self.deaf.contains(&pid) {
                self.running.remove(&pid);
                let (born, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.born_at_end)
                    .into_iter()
                    .partition(|(at, _)| *at == pid);
                self.born_at_end = later;
                for (_, p) in born {
                    self.table.push(p);
                    self.running.insert(p.pid);
                }
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

    /// #3665, condition 4 (review R1-4) — the keeper's teardown loops until it
    /// has no child left. Here the window manager (300, the keeper's child)
    /// is deaf to SIGTERM, and as its SIGKILL lands a process of its own is
    /// left behind, deaf too, and adopted by the keeper (400): born after the
    /// first pass took its snapshot. A later pass ends it. Without the loop
    /// the teardown would give up with it running.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_keepers_teardown_ends_what_is_born_during_its_first_pass() {
        // The keeper is 100.
        let me = 100;
        let mut m = MockProcs::new(&[pi(300, me, 200)], &[300, 400], &[]);
        m.born_at_end.push((300, pi(400, me, 400)));
        let mut collect = |p: &mut dyn Procs| {
            if p.snapshot().iter().any(|i| i.ppid == me) {
                keeper::Left::Some
            } else {
                keeper::Left::None
            }
        };
        let ended =
            keeper::teardown_with(&mut m, me, None, Duration::from_millis(30), &mut collect);

        assert_eq!(ended, Ok(()), "{:?}", m.events);
        assert!(
            m.events.contains(&Ev::Signal(400, libc::SIGKILL)),
            "the adopted orphan was never killed: {:?}",
            m.events
        );
        assert!(!m.running.contains(&400));
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

/// #3671 — an X server of a test's own, for the tests of other modules whose
/// X connection must outlive its server: an Xvfb on the first display from
/// `:99` up with neither a socket nor a lock ([`pick_display`]), so never one
/// a server already runs on, started as the daemon starts its desktop
/// (with `true` for a window manager and no keeper), killed as a server dies,
/// started again on the same display, and stopped with the value.
#[cfg(all(test, target_os = "linux"))]
#[cfg_attr(
    not(any(feature = "scrap-capture", feature = "clipboard")),
    allow(dead_code)
)]
pub(crate) struct TestX(std::sync::Arc<Desktop>);

#[cfg(all(test, target_os = "linux"))]
#[cfg_attr(
    not(any(feature = "scrap-capture", feature = "clipboard")),
    allow(dead_code)
)]
impl TestX {
    /// The lane that runs these tests asked for them: `ROOMLERD_TEST_X11=1`,
    /// which CI's X11 step sets, with an Xvfb installed, one test at a time.
    /// Read here, not in the capture's files, whose environment reads the
    /// FR-135 ratchet counts.
    pub(crate) fn opted_in() -> bool {
        std::env::var("ROOMLERD_TEST_X11").as_deref() == Ok("1")
    }

    /// `None` when no Xvfb comes up (none installed).
    pub(crate) fn start() -> Option<Self> {
        let mut plan = Plan::of(&Config {
            resolution: "640x480".into(),
            wm: "true".into(),
            startup: vec![],
        });
        plan.xorg = false;
        plan.clients = Clients::Direct;
        let desk = std::sync::Arc::new(Desktop::new(pick_display(X11_UNIX_DIR), plan));
        let x = Self(desk);
        x.0.bring_up().ok()?;
        Some(x)
    }

    pub(crate) fn display(&self) -> &str {
        &self.0.display
    }

    /// The server's SIGKILL, as a server dies, and its tree taken down and
    /// collected, as the supervisor does after an exit.
    pub(crate) fn kill(&self) {
        let pid = self
            .0
            .state()
            .tree
            .as_ref()
            .and_then(|t| t.roots.first().copied());
        if let Some(pid) = pid {
            // A child of this test that nobody has collected yet.
            RealProcs.signal(pid, libc::SIGKILL);
            self.0.set_down("its X server was killed by a test".into());
            self.0.kill_recorded_tree();
            self.0.clear_lock_of(pid);
        }
    }

    /// A new server on the same display, as the supervisor starts one. Only
    /// the clipboard's and the input's tests restart a server.
    #[cfg_attr(
        not(any(feature = "clipboard", feature = "enigo-input")),
        allow(dead_code)
    )]
    pub(crate) fn restart(&self) -> bool {
        self.0.bring_up().is_ok()
    }

    /// `DISPLAY` names this server until the value is dropped, for what
    /// connects to `$DISPLAY` with no way to be told another display: the
    /// capture's worker, arboard.
    pub(crate) fn point_display(&self) -> TestDisplay {
        let was = std::env::var_os("DISPLAY");
        // SAFETY: only under `ROOMLERD_TEST_X11=1` (`Self::opted_in`), which
        // CI's X11 steps run with `--test-threads=1`: no other test reads
        // the environment while it changes.
        unsafe { std::env::set_var("DISPLAY", self.display()) };
        TestDisplay(was)
    }
}

/// `DISPLAY` as it was before [`TestX::point_display`], put back on drop.
#[cfg(all(test, target_os = "linux"))]
#[cfg_attr(
    not(any(feature = "scrap-capture", feature = "clipboard")),
    allow(dead_code)
)]
pub(crate) struct TestDisplay(Option<std::ffi::OsString>);

#[cfg(all(test, target_os = "linux"))]
impl Drop for TestDisplay {
    fn drop(&mut self) {
        // SAFETY: as in `TestX::point_display`.
        unsafe {
            match self.0.take() {
                Some(v) => std::env::set_var("DISPLAY", v),
                None => std::env::remove_var("DISPLAY"),
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for TestX {
    fn drop(&mut self) {
        self.0.stop();
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

    /// #3918 — what input reads of the desktop, both ways. A
    /// desktop not up yet is down in period 0 under generation 0; up, it is
    /// not down; gone, it is in down period 1; a restart's count and its
    /// publish are one read apart from nothing (the same lock), so it is seen
    /// up with generation 1 or down with 0; a stop is a new down period. And
    /// a process that started no desktop reads `None`, never "down".
    #[test]
    fn input_reads_the_desktops_generation_and_down_period_both_ways() {
        assert_eq!(x_server_now(), None, "no desktop started in this process");
        assert_eq!(down(), None);
        let desk = Desktop::new(":0".into(), plan(String::new()));
        let at = |generation, down| XServerNow { generation, down };
        assert_eq!(desk.x_server_now(), at(0, Some(0)), "not up yet");
        desk.state().up = Some(Server::Xvfb);
        assert_eq!(desk.x_server_now(), at(0, None), "up");
        assert!(desk.set_down("the X server exited".into()));
        assert_eq!(desk.x_server_now(), at(0, Some(1)), "gone");
        assert!(desk.set_down("still down".into()));
        assert_eq!(desk.x_server_now(), at(0, Some(1)), "the same period");
        {
            // What `bring_up_as(true)` does under the lock once the new
            // server answers.
            let mut st = desk.state();
            desk.generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            st.up = Some(Server::Xvfb);
        }
        assert_eq!(desk.x_server_now(), at(1, None), "back, a new generation");
        desk.stop();
        assert_eq!(desk.x_server_now(), at(1, Some(2)), "stopped");
    }

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
            // #4099 — `true` exits at once: an hour, so that no test but
            // those that set these sees the window manager started again.
            wm_delay_floor: Duration::from_secs(3600),
            wm_delay_ceil: Duration::from_secs(3600),
            wm_stable_after: Duration::from_secs(3600),
            // The daemon's default (#3665). The keeper is this test binary,
            // re-run in its role; generous, since a loaded host starts it
            // late.
            clients: Clients::Keeper(KeeperCmd::test()),
            keeper_ready_timeout: Duration::from_secs(20),
            keeper_lines: None,
            // #4100 — short, so that a start the keeper never answers fails
            // its launch within a test's wait.
            x_client_wait: Duration::from_secs(2),
        }
    }

    impl KeeperCmd {
        /// The keeper as a test runs it: this test binary, re-run to run
        /// [`keeper::tests::keeper_role`] alone.
        fn test() -> Self {
            Self {
                program: std::env::current_exe().expect("the test binary"),
                args: [
                    "virtual_desktop::keeper::tests::keeper_role",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ]
                .into_iter()
                .map(Into::into)
                .collect(),
                arg0: None,
                env: vec![(keeper::TEST_ROLE_ENV.into(), "1".into())],
            }
        }

        /// A script of the test's own in the keeper's place.
        fn script(path: &str) -> Self {
            Self {
                program: path.into(),
                args: Vec::new(),
                arg0: None,
                env: Vec::new(),
            }
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

    /// A stale lock is removed only when it is OURS, never one that names
    /// another pid. (That its server has been collected is the caller's to
    /// know: `Desktop::clear_lock_of`.)
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
        let told = tell_recorded(&desk);
        retire_recorded(&desk, &told);

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
        // #3671 — the shared pipelines were retired at the exit, before the
        // sessions were told, and again once the new server was published up.
        wait_for("the shared pipelines to be retired once it is back", || {
            told.lock().unwrap_or_else(|e| e.into_inner()).len() >= 3
        });
        let events: Vec<String> = told
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(e, _)| e.clone())
            .collect();
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(events[0], "retired: down on generation 0", "{events:?}");
        assert!(events[1].starts_with("its X server ("), "{events:?}");
        assert_eq!(events[2], "retired: up on generation 1", "{events:?}");

        desk.stop();
        supervisor.join().expect("the supervisor left cleanly");
        assert!(collected(second_pid), "the stop left the server running");
        assert!(!x_answers(&sock, Duration::from_millis(200)));
        println!(
            "x11-real: a killed X server came back on {display} ({}); the shared pipelines were \
             retired at its exit and once it was back",
            kind.name()
        );
    }

    // -----------------------------------------------------------------------
    // #3665: every process the desktop started stays within reach of its
    // teardown, whatever became of its parent, and every one that exits is
    // collected. Stand-ins only: no X server, and never `:99`.
    // -----------------------------------------------------------------------

    /// An executable shell script `<dir>/<name>`: a window manager, a startup
    /// app or a server stand-in.
    fn script(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    /// A FIFO at `path`: a server stand-in's connection to its clients.
    fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `mkfifo(3)` with a NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo {}", path.display());
    }

    /// The pid a script of this test wrote to `<dir>/<name>`, once it is there.
    fn pid_in(dir: &Path, name: &str) -> i32 {
        let mut pid = None;
        wait_for(name, || {
            pid = std::fs::read_to_string(dir.join(name))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            pid.is_some()
        });
        pid.unwrap()
    }

    /// The live process whose pid a script of this test wrote to
    /// `<dir>/<name>`, as `/proc` describes it: its start time is what every
    /// later look of the test, and its cleanup, check against.
    fn recorded(dir: &Path, name: &str) -> ProcInfo {
        let pid = pid_in(dir, name);
        let mut info = None;
        wait_for(name, || {
            info = read_proc_stat(pid);
            info.is_some()
        });
        info.unwrap()
    }

    /// `info` still runs as the process the test recorded: not gone, not a
    /// zombie, and not a later process under a recycled pid.
    fn still_running(info: &ProcInfo) -> bool {
        is_running(info.pid) && RealProcs.same_process_now(info)
    }

    /// The process `info` recorded has been reaped: `/proc` no longer lists
    /// it, as a zombie or at all (a later process under its pid is another).
    fn reaped(info: &ProcInfo) -> bool {
        !RealProcs.same_process_now(info)
    }

    /// What `at-spi-bus-launcher` is in the field, as one line of shell: a
    /// child that leaves the desktop's process group and session (`setsid`),
    /// ignores SIGTERM, and writes its pid to `<dir>/gc.pid`. `trap '' TERM`
    /// survives the `exec`, so the `sleep` is as deaf as the shell was.
    fn deaf_setsid_child(dir: &Path) -> String {
        format!(
            "setsid sh -c 'trap \"\" TERM; echo $$ > \"{}/gc.pid\"; exec sleep 300' &",
            dir.display()
        )
    }

    /// The escape in the field is a `setsid`. A host without the program
    /// cannot build the case; the pure `select_targets` tests still run there.
    fn have_setsid(test: &str) -> bool {
        if binary_on_path("setsid").is_some() {
            return true;
        }
        eprintln!("SKIP {test}: `setsid` is not on PATH");
        false
    }

    /// Processes this test started that no tree recorded (a grandchild that
    /// left the desktop's group): SIGKILLed when the test leaves, whatever an
    /// assertion did. Each one only while `/proc` still shows the process the
    /// test recorded (its start time), and never pid 0 or 1
    /// (`RealProcs::signal`).
    struct KillAtTheEnd(Vec<ProcInfo>);
    impl Drop for KillAtTheEnd {
        fn drop(&mut self) {
            for info in &self.0 {
                if RealProcs.same_process_now(info) {
                    RealProcs.signal(info.pid, libc::SIGKILL);
                }
            }
        }
    }

    /// #3665, the issue's first path. The X server exits, and a startup app
    /// exits on the X I/O error that follows, before the supervisor reads
    /// `/proc`. It leaves behind a child that had left the desktop's group
    /// (`setsid`) and ignores SIGTERM, as `at-spi-bus-launcher` does. That
    /// child now belongs to whoever adopts orphans here, in a session of its
    /// own, so no parent link from anything the daemon recorded leads to it.
    ///
    /// The supervisor's teardown must still end it, within the teardown's
    /// bound ([`TEARDOWN_GRACE`], a second for the SIGKILL to land, a second
    /// for the reap), and the stop after it must leave nothing the desktop
    /// started.
    ///
    /// The server stand-in closes its clients' connection (a FIFO) first and
    /// exits only when the test says so. So the order the field CAN take, the
    /// clients gone before the supervisor looks, is the order every run takes.
    #[test]
    #[allow(clippy::zombie_processes)] // the server is reaped by the supervisor's teardown
    fn an_orphan_of_an_app_that_died_with_its_server_does_not_outlive_the_teardown() {
        if !have_setsid("an_orphan_of_an_app_that_died_with_its_server…") {
            return;
        }
        let dir = scratch("orphan-x");
        let d = dir.display();
        mkfifo(&dir.join("x.fifo"));
        let server = script(
            &dir,
            "x",
            &format!(
                "exec 3<>\"{d}/x.fifo\"\n\
                 while [ ! -e \"{d}/x.close\" ]; do sleep 0.05; done\n\
                 exec 3>&-\n\
                 while [ ! -e \"{d}/x.exit\" ]; do sleep 0.05; done\n\
                 exit 1"
            ),
        );
        // A client: holds the connection, says its pid, exits on its end.
        let on_the_display = |pid_file: &str| {
            format!(
                "exec 4<\"{d}/x.fifo\"\n\
                 echo $$ > \"{d}/{pid_file}\"\n\
                 read -r line <&4\n\
                 exit 0"
            )
        };
        let wm_script = script(&dir, "wm", &on_the_display("wm.pid"));
        // The deaf child is started BEFORE the app opens the connection, so it
        // holds no end of it.
        let app_script = script(
            &dir,
            "app",
            &format!("{}\n{}", deaf_setsid_child(&dir), on_the_display("app.pid")),
        );
        let desk = Arc::new(Desktop::new(
            ":460".into(),
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: wm_script,
                    startup: vec![app_script],
                },
                // Every start after the exit fails at once: the test is about
                // the teardown before it.
                ..plan(stand_in(&dir, "exit 1"))
            },
        ));
        let _stop = StopAtTheEnd(desk.clone());

        let mut cmd = Command::new(&server);
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = desk
            .spawn_server(&mut cmd)
            .unwrap()
            .expect("the daemon is not stopping");
        let mut x = XServer {
            pid: child.id() as i32,
            kind: Server::Xvfb,
            tail: StderrTail::follow(child.stderr.take()),
            since: Instant::now(),
            keeper: None,
        };
        let x_pid = x.pid;
        x.keeper = desk.start_clients();
        let wm = pid_in(&dir, "wm.pid");
        let app = pid_in(&dir, "app.pid");
        let gc = recorded(&dir, "gc.pid");
        let _gc = KillAtTheEnd(vec![gc]);
        assert_eq!(gc.pgid, gc.pid, "the child leads a group of its own");

        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(x)))
        };
        // The connection drops: the window manager and the app exit, and the
        // app's child goes to whoever adopts orphans here.
        std::fs::write(dir.join("x.close"), "").unwrap();
        wait_for("the app's child to lose its parent", || {
            read_proc_stat(gc.pid).is_some_and(|p| p.ppid != app)
        });
        // Now the server exits, and the supervisor takes the desktop down.
        std::fs::write(dir.join("x.exit"), "").unwrap();
        let exited = Instant::now();
        wait_for("the supervisor's teardown, and the start after it", || {
            !runs(&dir).is_empty()
        });
        let took = exited.elapsed();
        let gc_after_teardown = still_running(&gc);

        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let gc_after_stop = still_running(&gc);
        let left: Vec<i32> = [x_pid, wm, app]
            .into_iter()
            .chain(runs(&dir))
            .filter(|pid| !collected(*pid))
            .collect();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            !gc_after_teardown,
            "the supervisor's teardown ({took:?}) left the app's SIGTERM-deaf orphan {} running",
            gc.pid
        );
        assert!(took < Duration::from_secs(10), "the teardown took {took:?}");
        assert!(
            !gc_after_stop,
            "the stop left the app's orphan {} running",
            gc.pid
        );
        assert!(left.is_empty(), "the stop left these uncollected: {left:?}");
    }

    /// #3665, the issue's second path. A startup app exits on its own while
    /// the desktop runs, and leaves such a child behind. The next stop must
    /// end it, within the same bound.
    #[test]
    fn an_orphan_of_an_app_that_exited_by_itself_is_reached_by_the_next_stop() {
        if !have_setsid("an_orphan_of_an_app_that_exited_by_itself…") {
            return;
        }
        let dir = scratch("orphan-self");
        let d = dir.display();
        let app_script = script(
            &dir,
            "app",
            &format!(
                "{}\n\
                 echo $$ > \"{d}/app.pid\"\n\
                 while [ ! -s \"{d}/gc.pid\" ]; do sleep 0.05; done\n\
                 exit 0",
                deaf_setsid_child(&dir)
            ),
        );
        let desk = Arc::new(Desktop::new(
            ":461".into(),
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: "true".into(),
                    startup: vec![app_script],
                },
                ..plan(stand_in(&dir, "exit 1"))
            },
        ));
        let _stop = StopAtTheEnd(desk.clone());
        let mut server = Command::new("sleep");
        server
            .arg("300")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let x_pid = desk
            .spawn_server(&mut server)
            .unwrap()
            .expect("the daemon is not stopping")
            .id() as i32;
        desk.start_clients();
        let app = pid_in(&dir, "app.pid");
        let gc = recorded(&dir, "gc.pid");
        let _gc = KillAtTheEnd(vec![gc]);
        wait_for("the app's child to lose its parent", || {
            read_proc_stat(gc.pid).is_some_and(|p| p.ppid != app)
        });

        let asked = Instant::now();
        desk.stop();
        let took = asked.elapsed();
        let gc_after_stop = still_running(&gc);
        let server_left = !collected(x_pid);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            !gc_after_stop,
            "the stop ({took:?}) left the orphan {} of an app that had exited running",
            gc.pid
        );
        assert!(took < Duration::from_secs(10), "the stop took {took:?}");
        assert!(!server_left, "the stop left the server {x_pid} uncollected");
    }

    /// #3665, the issue's third path. The window manager and a startup app
    /// exit while the desktop runs. Each is collected at once, so no zombie
    /// of either waits under the daemon for the next teardown, and neither
    /// number is in the roots the teardown signals without a look (the
    /// `is_root` shortcut in [`kill_tree_with`]).
    #[test]
    #[allow(clippy::zombie_processes)] // the server is reaped by the stop's `kill_tree`
    fn a_window_manager_and_an_app_that_exit_are_collected() {
        let dir = scratch("exits");
        let d = dir.display();
        let leaves = |pid_file: &str, code: u8| {
            format!(
                "echo $$ > \"{d}/{pid_file}\"\n\
                 while [ ! -e \"{d}/go\" ]; do sleep 0.05; done\n\
                 exit {code}"
            )
        };
        let wm_script = script(&dir, "wm", &leaves("wm.pid", 0));
        let app_script = script(&dir, "app", &leaves("app.pid", 3));
        let desk = Arc::new(Desktop::new(
            ":462".into(),
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: wm_script,
                    startup: vec![app_script],
                },
                ..plan(stand_in(&dir, "exit 1"))
            },
        ));
        let _stop = StopAtTheEnd(desk.clone());
        let mut server = Command::new("sleep");
        server
            .arg("300")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _server = desk
            .spawn_server(&mut server)
            .unwrap()
            .expect("the daemon is not stopping");
        desk.start_clients();
        let wm = recorded(&dir, "wm.pid");
        let app = recorded(&dir, "app.pid");

        std::fs::write(dir.join("go"), "").unwrap();
        let bound = Instant::now() + Duration::from_secs(2);
        while !(reaped(&wm) && reaped(&app)) && Instant::now() < bound {
            std::thread::sleep(Duration::from_millis(20));
        }
        let (wm_reaped, app_reaped) = (reaped(&wm), reaped(&app));
        let roots = desk
            .state()
            .tree
            .as_ref()
            .map(|t| t.roots.clone())
            .unwrap_or_default();
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            wm_reaped,
            "the window manager {} exited and is still a zombie under the daemon",
            wm.pid
        );
        assert!(
            app_reaped,
            "the startup app {} exited and is still a zombie under the daemon",
            app.pid
        );
        assert!(
            !roots.contains(&wm.pid) && !roots.contains(&app.pid),
            "a collected child is still in the tree's roots: {roots:?}"
        );
    }

    /// A desktop around a `sleep` in the server's place, with `wm_body` as
    /// the window manager's script and `clients` and `ready_timeout` as given.
    /// The desktop, and the server's pid.
    fn desktop_with(
        dir: &Path,
        display: &str,
        wm_body: &str,
        clients: Clients,
        ready_timeout: Duration,
    ) -> (Arc<Desktop>, i32) {
        let wm = script(dir, "wm", wm_body);
        desktop_planned(
            display,
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm,
                    startup: vec![],
                },
                clients,
                keeper_ready_timeout: ready_timeout,
                ..plan(stand_in(dir, "exit 1"))
            },
        )
    }

    /// A desktop on `plan`, with a `sleep` in the server's place spawned and
    /// recorded. The desktop, and the server's pid.
    fn desktop_planned(display: &str, plan: Plan) -> (Arc<Desktop>, i32) {
        let desk = Arc::new(Desktop::new(display.into(), plan));
        let mut server = Command::new("sleep");
        server
            .arg("300")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let x_pid = desk
            .spawn_server(&mut server)
            .unwrap()
            .expect("the daemon is not stopping")
            .id() as i32;
        (desk, x_pid)
    }

    /// The recorded tree's roots now.
    fn roots_of(desk: &Desktop) -> Vec<i32> {
        desk.state()
            .tree
            .as_ref()
            .map(|t| t.roots.clone())
            .unwrap_or_default()
    }

    /// #3665 — the keeper is the parent of the window manager, in the
    /// desktop's group, and the tree's roots are the server and the keeper.
    #[test]
    fn the_keeper_is_the_parent_of_the_clients() {
        let dir = scratch("keeper-parent");
        let wm_body = format!("echo $$ > \"{}/wm.pid\"\nexec sleep 300", dir.display());
        let (desk, x_pid) = desktop_with(
            &dir,
            ":463",
            &wm_body,
            Clients::Keeper(KeeperCmd::test()),
            Duration::from_secs(20),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients();
        let wm = recorded(&dir, "wm.pid");
        let roots = roots_of(&desk);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        let keeper = keeper.expect("the keeper said it was ready");
        assert_eq!(wm.ppid, keeper, "the window manager is the keeper's child");
        assert_eq!(
            wm.pgid, x_pid,
            "the window manager is in the desktop's group"
        );
        assert_eq!(
            roots,
            vec![x_pid, keeper],
            "the roots are the server and the keeper"
        );
        assert!(
            collected(keeper),
            "the stop left the keeper {keeper} uncollected"
        );
    }

    /// #3665 — the kill switch starts the window manager as before: the
    /// daemon's own child, in the desktop's group, one of the roots.
    #[test]
    fn the_kill_switch_starts_the_clients_as_before() {
        let dir = scratch("direct");
        let wm_body = format!("echo $$ > \"{}/wm.pid\"\nexec sleep 300", dir.display());
        let (desk, x_pid) = desktop_with(
            &dir,
            ":464",
            &wm_body,
            Clients::Direct,
            Duration::from_secs(20),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients();
        let wm = recorded(&dir, "wm.pid");
        let roots = roots_of(&desk);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(keeper, None, "the kill switch runs no keeper");
        assert_eq!(wm.ppid, std::process::id() as i32, "the daemon's own child");
        assert_eq!(wm.pgid, x_pid, "in the desktop's group");
        assert_eq!(roots, vec![x_pid, wm.pid]);
    }

    /// #3665, condition 3 — a keeper that never says it is ready is killed and
    /// collected within the wait, leaves the tree in the same step, and the
    /// daemon starts the window manager itself.
    #[test]
    fn a_keeper_that_never_says_ready_is_collected_and_the_clients_start_directly() {
        let dir = scratch("keeper-silent");
        let d = dir.display();
        let silent = script(
            &dir,
            "keeper",
            &format!("echo $$ > \"{d}/keeper.pid\"\nexec sleep 300"),
        );
        let wm_body = format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300");
        let (desk, x_pid) = desktop_with(
            &dir,
            ":465",
            &wm_body,
            Clients::Keeper(KeeperCmd::script(&silent)),
            Duration::from_millis(500),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let asked = Instant::now();
        let keeper = desk.start_clients();
        let took = asked.elapsed();
        let silent_pid = pid_in(&dir, "keeper.pid");
        let wm = recorded(&dir, "wm.pid");
        let roots = roots_of(&desk);
        let still_keeper = desk.state().tree.as_ref().and_then(|t| t.keeper);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(keeper, None);
        assert!(
            took >= Duration::from_millis(500) && took < Duration::from_secs(5),
            "the wait took {took:?}"
        );
        assert!(
            collected(silent_pid),
            "the silent keeper {silent_pid} was not collected"
        );
        assert_eq!(still_keeper, None);
        assert_eq!(
            roots,
            vec![x_pid, wm.pid],
            "the silent keeper left the roots"
        );
        assert_eq!(
            wm.ppid,
            std::process::id() as i32,
            "the window manager was started directly"
        );
    }

    /// Review R1-12 — a keeper that is not ready in time is a strike, as an
    /// early exit is: after two in a row the daemon starts the clients itself
    /// for the rest of its life, and no later start waits for a keeper again.
    ///
    /// Review R3-2: the verdict is what the daemon did, not what the slow
    /// stand-ins had time to write. A third keeper would make the third start
    /// wait out the whole ready timeout, so that start is timed.
    #[test]
    fn two_keepers_not_ready_in_time_turn_the_keeper_off() {
        let dir = scratch("keeper-slow");
        let d = dir.display();
        let slow = script(
            &dir,
            "keeper",
            &format!("echo $$ >> \"{d}/keeper.pids\"\nexec sleep 300"),
        );
        let wm_body = format!("echo $$ >> \"{d}/wm.pids\"\nexec sleep 300");
        let ready_timeout = Duration::from_secs(1);
        let (desk, _x_pid) = desktop_with(
            &dir,
            ":476",
            &wm_body,
            Clients::Keeper(KeeperCmd::script(&slow)),
            ready_timeout,
        );
        let _stop = StopAtTheEnd(desk.clone());
        let off = |desk: &Desktop| desk.keeper_off.load(std::sync::atomic::Ordering::Relaxed);
        let first = desk.start_clients();
        let off_after_one = off(&desk);
        let second = desk.start_clients();
        let off_after_two = off(&desk);
        let asked = Instant::now();
        let third = desk.start_clients();
        let third_took = asked.elapsed();
        let count = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let keepers = count("keeper.pids");
        wait_for("the third window manager", || count("wm.pids") == 3);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!((first, second, third), (None, None, None));
        assert!(!off_after_one, "one slow keeper turned the keeper off");
        assert!(off_after_two, "two slow keepers in a row did not");
        assert!(
            third_took < ready_timeout,
            "the third start waited {third_took:?} for a keeper after the keeper was off"
        );
        assert!(
            keepers <= 2,
            "a keeper was started after the keeper was off: {keepers}"
        );
    }

    /// Review R3-1 — the count of keepers that were not ready in time starts
    /// again at a `ready`: a slow keeper, a ready one and a slow one again do
    /// not turn the keeper off. Without that, any two slow starts in a
    /// daemon's life would. The stand-in is rewritten between the starts, so
    /// which one is ready does not hang on how fast a script runs.
    #[test]
    fn a_ready_keeper_between_two_slow_ones_keeps_the_keeper_on() {
        let dir = scratch("keeper-slow-ready-slow");
        let d = dir.display();
        let slow_body = "exec sleep 300".to_string();
        let ready_body = format!("echo ready >&2\nread -r job && : > \"{d}/job\"\nexec sleep 300");
        let keeper = script(&dir, "keeper", &slow_body);
        let wm_body = format!("echo $$ >> \"{d}/wm.pids\"\nexec sleep 300");
        let (desk, _x_pid) = desktop_with(
            &dir,
            ":479",
            &wm_body,
            Clients::Keeper(KeeperCmd::script(&keeper)),
            Duration::from_secs(2),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let slow = |desk: &Desktop| desk.keeper_slow.load(std::sync::atomic::Ordering::Relaxed);
        let first = desk.start_clients();
        let slow_after_first = slow(&desk);
        script(&dir, "keeper", &ready_body);
        let second = desk.start_clients();
        let slow_after_ready = slow(&desk);
        script(&dir, "keeper", &slow_body);
        let third = desk.start_clients();
        let off_after_third = desk.keeper_off.load(std::sync::atomic::Ordering::Relaxed);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(first, None, "the first, slow keeper was the desktop's");
        assert_eq!(slow_after_first, 1, "the first slow keeper was not counted");
        assert!(
            second.is_some(),
            "the ready keeper was not the desktop's: {second:?}"
        );
        assert_eq!(
            slow_after_ready, 0,
            "a `ready` did not start the count again"
        );
        assert_eq!(third, None, "the third, slow keeper was the desktop's");
        assert!(
            !off_after_third,
            "a slow keeper, a ready one and a slow one turned the keeper off"
        );
    }

    /// Review R2-1 — the daemon's half of R1-2: nothing reaches the keeper's
    /// input before the daemon has read its `ready`. A keeper that takes
    /// 300 ms to get ready looks at its input first and finds it empty; its
    /// job comes after, and it is the desktop's keeper. Were the job written
    /// before the wait, a keeper the daemon gives up on could still start
    /// the clients, and the daemon would start them a second time beside it.
    ///
    /// Review R3-7: the probe proves that it ran. `timeout` answers 124 when
    /// it ended a `dd` that read nothing, 0 for a `dd` that read a byte, and
    /// 127 when either tool is missing, so a runner without them fails the
    /// test instead of passing it.
    #[test]
    fn nothing_reaches_the_keepers_input_before_it_said_ready() {
        let dir = scratch("keeper-late");
        let d = dir.display();
        // `dd` copies a byte that comes within 300 ms, and `timeout` ends it
        // otherwise.
        let late = script(
            &dir,
            "keeper",
            &format!(
                "echo $$ > \"{d}/keeper.pid\"\n\
                 timeout 0.3 dd bs=1 count=1 of=\"{d}/got\" 2>/dev/null\n\
                 echo $? > \"{d}/probe.rc\"\n\
                 echo ready >&2\n\
                 read -r job && : > \"{d}/job\"\n\
                 exec sleep 300"
            ),
        );
        let wm_body = format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300");
        let (desk, _x_pid) = desktop_with(
            &dir,
            ":477",
            &wm_body,
            Clients::Keeper(KeeperCmd::script(&late)),
            Duration::from_secs(20),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients();
        wait_for("the keeper's job", || dir.join("job").exists());
        let probe = std::fs::read_to_string(dir.join("probe.rc")).unwrap_or_default();
        let got = std::fs::metadata(dir.join("got")).map_or(0, |m| m.len());
        let direct_wm = dir.join("wm.pid").exists();
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            (probe.trim(), got),
            ("124", 0),
            "the keeper's look at its input before `ready` (timeout's status, bytes read): \
             (\"0\", 1) is input that came too early, (\"127\", 0) a probe that did not run"
        );
        assert!(
            keeper.is_some(),
            "the late keeper is not the desktop's: {keeper:?}"
        );
        assert!(
            !direct_wm,
            "the daemon started the window manager beside its keeper"
        );
    }

    /// #3665, condition 3 — a keeper that refuses before it is ready (as the
    /// keeper does when it cannot adopt orphans) says why, is collected, and
    /// leaves the tree; the start after it is direct.
    #[test]
    fn a_keeper_that_refuses_falls_back_with_its_reason() {
        let dir = scratch("keeper-refuses");
        let d = dir.display();
        let refuses = script(
            &dir,
            "keeper",
            &format!(
                "echo $$ > \"{d}/keeper.pid\"\n\
                 echo 'refused: no subreaper on this host' >&2\nexit 2"
            ),
        );
        let wm_body = format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300");
        let (desk, x_pid) = desktop_with(
            &dir,
            ":466",
            &wm_body,
            Clients::Keeper(KeeperCmd::script(&refuses)),
            Duration::from_secs(20),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let why = desk.start_keeper(&KeeperCmd::script(&refuses));
        let refused_pid = pid_in(&dir, "keeper.pid");
        let roots_after = roots_of(&desk);
        let keeper = desk.start_clients();
        let wm = recorded(&dir, "wm.pid");
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            matches!(&why, Err(w) if w.contains("no subreaper on this host")),
            "{why:?}"
        );
        assert!(
            collected(refused_pid),
            "the keeper {refused_pid} was not collected"
        );
        assert_eq!(
            roots_after,
            vec![x_pid],
            "the refusing keeper left the roots"
        );
        assert_eq!(keeper, None);
        assert_eq!(
            wm.ppid,
            std::process::id() as i32,
            "the window manager was started directly"
        );
    }

    /// Review R1-2 — a keeper that refuses its job after it said it was
    /// ready (the job names another process group) is the desktop's
    /// failure, as any exit of a keeper that has its job is (K1): the
    /// supervisor takes the desktop down and starts it again, and the daemon
    /// never starts the window manager on top of what the keeper may have
    /// started. Its strike is [`KeeperStrikes`]' (the test above, and on a
    /// real Xvfb `after_two_early_keeper_exits_the_desktop_starts_directly_and_stays_up`).
    #[test]
    fn a_keeper_that_refuses_its_job_is_the_desktops_failure_and_nothing_starts_on_top() {
        let dir = scratch("keeper-refuses-job");
        let d = dir.display();
        let refuses = script(
            &dir,
            "keeper",
            &format!(
                "echo $$ > \"{d}/keeper.pid\"\necho ready >&2\nread -r job\n\
                 echo 'refused: the job is for process group 1, and the keeper is in 2' >&2\nexit 2"
            ),
        );
        let wm_body = format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300");
        let (desk, x_pid) = desktop_with(
            &dir,
            ":475",
            &wm_body,
            Clients::Keeper(KeeperCmd::script(&refuses)),
            Duration::from_secs(20),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients();
        let refused_pid = pid_in(&dir, "keeper.pid");
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(running(&desk, x_pid, keeper))))
        };
        wait_for("the supervisor to start the desktop again", || {
            !runs(&dir).is_empty()
        });
        let down_with_it = collected(x_pid) && collected(refused_pid);
        let wm_started = dir.join("wm.pid").exists();
        let off = desk.keeper_off.load(std::sync::atomic::Ordering::Relaxed);
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            keeper,
            Some(refused_pid),
            "the job went out after ready, so the keeper is the desktop's"
        );
        assert!(
            down_with_it,
            "the restart came before the teardown collected the server {x_pid} and the keeper {refused_pid}"
        );
        assert!(
            !wm_started,
            "the window manager was started on top of a keeper that had its job"
        );
        assert!(!off, "one refusal turned the keeper off");
    }

    /// The server [`desktop_planned`] spawned, as `bring_up` hands it to the
    /// supervisor, with the keeper `start_clients` returned; the desktop is
    /// marked up, as `bring_up` marks it.
    fn running(desk: &Desktop, x_pid: i32, keeper: Option<i32>) -> XServer {
        // What `bring_up` records once the clients are started.
        {
            let mut st = desk.state();
            st.up = Some(Server::Xvfb);
            st.why_down.clear();
        }
        XServer {
            pid: x_pid,
            kind: Server::Xvfb,
            tail: StderrTail::follow(None::<std::process::ChildStderr>),
            since: Instant::now(),
            keeper,
        }
    }

    /// The CPU ticks (user and system) the thread of this process named
    /// `name` has used so far.
    fn thread_ticks(name: &str) -> Option<u64> {
        for entry in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
            let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
                continue;
            };
            if comm.trim() != name {
                continue;
            }
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
            // utime and stime, fields 14 and 15.
            return Some(
                fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?,
            );
        }
        None
    }

    /// #3665, K1 — the keeper's exit is the desktop's failure, as its
    /// server's is: the desktop goes down saying so, the server, the keeper
    /// and what the keeper started are collected, and the desktop is started
    /// again. Before it, the daemon's only children in the desktop's group
    /// are the server and the keeper, which is what lets the supervisor wait
    /// on the group.
    #[test]
    fn a_keeper_that_dies_takes_the_desktop_down_and_it_is_started_again() {
        let dir = scratch("keeper-dies");
        let d = dir.display();
        let (desk, x_pid) = desktop_planned(
            ":467",
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: script(
                        &dir,
                        "wm",
                        &format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300"),
                    ),
                    startup: vec![],
                },
                // Long enough to read the reason before the next start.
                delay_floor: Duration::from_millis(300),
                delay_ceil: Duration::from_millis(300),
                ..plan(stand_in(&dir, "exit 1"))
            },
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let wm = recorded(&dir, "wm.pid");
        let me = std::process::id() as i32;
        let ours: std::collections::BTreeSet<i32> = read_proc_snapshot()
            .into_iter()
            .filter(|p| p.pgid == x_pid && p.ppid == me)
            .map(|p| p.pid)
            .collect();
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(running(&desk, x_pid, Some(keeper)))))
        };
        // Our child, unreaped while the tree lists it.
        RealProcs.signal(keeper, libc::SIGKILL);
        let mut why = String::new();
        wait_for("the desktop to go down for its keeper", || {
            why = desk.state().why_down.clone();
            why.contains("keeper")
        });
        wait_for("the next start", || !runs(&dir).is_empty());
        let (x_gone, keeper_gone, wm_running) =
            (collected(x_pid), collected(keeper), still_running(&wm));
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            ours,
            [x_pid, keeper].into_iter().collect(),
            "the daemon's children in the desktop's group"
        );
        assert_eq!(
            why,
            "its keeper process (vd-keeper) exited: signal 9 (killed)"
        );
        assert!(x_gone, "the server {x_pid} was not collected");
        assert!(keeper_gone, "the keeper {keeper} was not collected");
        assert!(
            !wm_running,
            "the window manager {} outlived its keeper",
            wm.pid
        );
    }

    /// #3665, condition 5 — a child of the daemon's in the desktop's group
    /// that the desktop did not start is not the supervisor's: it is left
    /// unreaped for its own waiter, the desktop stays up, and the supervisor
    /// does not spin on the zombie its `WNOWAIT` wait would otherwise find
    /// again and again.
    #[test]
    #[allow(clippy::zombie_processes)] // the stray is collected by `try_wait`
    fn a_child_the_desktop_did_not_record_does_not_spin_the_supervisor() {
        use std::os::unix::process::CommandExt as _;
        let dir = scratch("stray");
        let wm_body = format!("echo $$ > \"{}/wm.pid\"\nexec sleep 300", dir.display());
        let (desk, x_pid) = desktop_with(
            &dir,
            ":468",
            &wm_body,
            Clients::Keeper(KeeperCmd::test()),
            Duration::from_secs(20),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = {
            let desk = desk.clone();
            std::thread::Builder::new()
                .name("vd-sup-stray".into())
                .spawn(move || supervise(&desk, Some(running(&desk, x_pid, Some(keeper)))))
                .unwrap()
        };
        let mut stray = Command::new("true");
        stray.process_group(x_pid);
        let mut stray = stray.spawn().unwrap();
        let stray_pid = stray.id() as i32;
        wait_for("the stray to exit", || !is_running(stray_pid));
        std::thread::sleep(Duration::from_millis(300));
        let before = thread_ticks("vd-sup-stray");
        std::thread::sleep(Duration::from_secs(1));
        let after = thread_ticks("vd-sup-stray");
        let stray_unreaped = read_proc_stat(stray_pid).is_some();
        let up = desk.state().up;
        let ours_to_collect = matches!(stray.try_wait(), Ok(Some(_)));
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            stray_unreaped,
            "the supervisor reaped a child the desktop did not start"
        );
        assert!(ours_to_collect, "the stray was not left to its own waiter");
        assert_eq!(
            up,
            Some(Server::Xvfb),
            "the desktop went down for a child it did not start"
        );
        let (before, after) = (before.expect("the thread"), after.expect("the thread"));
        assert!(
            after - before < 20,
            "the supervisor spun: {} ticks in one second",
            after - before
        );
    }

    /// A desktop around a `sleep` in the server's place, with `wm_body` as
    /// the window manager's script, started by the keeper, whose lines after
    /// `ready` are kept. The desktop, the server's pid, and those lines.
    fn desktop_keeping_lines(
        dir: &Path,
        display: &str,
        wm_body: &str,
    ) -> (Arc<Desktop>, i32, KeeperLines) {
        let lines = KeeperLines::default();
        let wm = script(dir, "wm", wm_body);
        let (desk, x_pid) = desktop_planned(
            display,
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm,
                    startup: vec![],
                },
                keeper_lines: Some(lines.clone()),
                ..plan(stand_in(dir, "exit 1"))
            },
        );
        (desk, x_pid, lines)
    }

    /// The keeper's lines so far that warn, and whether it said `marker`
    /// (proof that its lines are read at all).
    fn keeper_warnings(lines: &KeeperLines, marker: &str) -> (Vec<String>, bool) {
        let said = lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let read = said.iter().any(|l| l.contains(marker));
        (
            said.into_iter()
                .filter(|l| l.starts_with("warn: "))
                .collect(),
            read,
        )
    }

    /// #3665, condition 9 — a service manager's stop signals every process of
    /// the unit at once, the keeper with the rest. SIGTERM alone makes the
    /// keeper end what it started and stay, so the supervisor sees no exit to
    /// report and starts nothing; the daemon's own stop then ends the keeper.
    ///
    /// Review R1-1: the window manager goes first, and the keeper is
    /// signalled only once it has collected the window manager's exit, before
    /// it knows of the stop: that order made certain, not timed (review
    /// R2-8). The keeper must not warn of the window manager's exit, then
    /// or ever. The server still runs then, so nothing
    /// else could keep the keeper quiet; it gets its SIGTERM with the
    /// daemon's own stop, the rest of the unit's. (The server's own exit in a
    /// stop is the supervisor's to report: #3671.)
    #[test]
    fn a_service_manager_stop_adds_no_keeper_warning() {
        let dir = scratch("unit-stop");
        let wm_body = format!("echo $$ > \"{}/wm.pid\"\nexec sleep 300", dir.display());
        let (desk, x_pid, lines) = desktop_keeping_lines(&dir, ":469", &wm_body);
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let wm = recorded(&dir, "wm.pid");
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(running(&desk, x_pid, Some(keeper)))))
        };
        // The window manager this test's desktop started, while `/proc`
        // still shows it; then, once the keeper has collected it (its entry
        // is gone from `/proc`), the keeper, our child, unreaped while the
        // tree lists it.
        if RealProcs.same_process_now(&wm) {
            RealProcs.signal(wm.pid, libc::SIGTERM);
        }
        wait_for("the keeper to collect the window manager", || {
            read_proc_stat(wm.pid).is_none_or(|p| p.starttime != wm.starttime)
        });
        RealProcs.signal(keeper, libc::SIGTERM);
        // Past the keeper's NOTICE_DELAY: a held exit would be said by now.
        std::thread::sleep(Duration::from_millis(2500));
        let keeper_stayed = is_running(keeper);
        let up = desk.state().up;
        let starts = runs(&dir).len();
        // The server's SIGTERM, our child, unreaped while the tree lists it,
        // and the daemon's own stop. Review R3-6: the desktop is marked
        // stopping first. In a daemon the supervisor that sees the server's
        // exit first waits 1 s before it starts again, and the stop wakes it
        // to leave; here that wait is 20 ms, and a preempted test thread
        // could let a start through, which no daemon would see.
        desk.state().stopping = true;
        RealProcs.signal(x_pid, libc::SIGTERM);
        let asked = Instant::now();
        desk.stop();
        let took = asked.elapsed();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let starts_after = runs(&dir).len();
        let _ = std::fs::remove_dir_all(&dir);
        let (warned, read) = keeper_warnings(&lines, "asked to stop");

        assert!(read, "the keeper's lines were not read: {lines:?}");
        assert!(warned.is_empty(), "the keeper warned in a stop: {warned:?}");
        assert!(keeper_stayed, "the keeper left on SIGTERM alone");
        assert_eq!(
            up,
            Some(Server::Xvfb),
            "the desktop went down on its keeper's SIGTERM"
        );
        assert_eq!(
            (starts, starts_after),
            (0, 0),
            "the desktop was started again"
        );
        assert!(collected(keeper), "the stop left the keeper {keeper}");
        assert!(collected(x_pid), "the stop left the server {x_pid}");
        assert!(took < Duration::from_secs(5), "the stop took {took:?}");
    }

    /// Review R1-1 — an X server's death ends its clients, the window manager
    /// among the first, and the supervisor's teardown then ends the rest.
    /// The keeper collects the window manager's exit before it learns of the
    /// death, and must not warn of it: by the
    /// time the notice is due, the daemon's teardown has cleared it, or the
    /// server (the desktop's group leader) is a zombie, which is not running.
    #[test]
    fn an_x_server_death_adds_no_keeper_warning() {
        let dir = scratch("x-death");
        let wm_body = format!("echo $$ > \"{}/wm.pid\"\nexec sleep 300", dir.display());
        let (desk, x_pid, lines) = desktop_keeping_lines(&dir, ":474", &wm_body);
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let wm = recorded(&dir, "wm.pid");
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(running(&desk, x_pid, Some(keeper)))))
        };
        // The window manager loses its server first, as a real X client does:
        // the one this test's desktop started, while `/proc` still shows it.
        if RealProcs.same_process_now(&wm) {
            RealProcs.signal(wm.pid, libc::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(50));
        // The server's stand-in, our child, unreaped while the tree lists it.
        RealProcs.signal(x_pid, libc::SIGKILL);
        wait_for("the supervisor's teardown to collect the keeper", || {
            collected(keeper)
        });
        // The keeper is gone, and its last lines with it.
        std::thread::sleep(Duration::from_millis(300));
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);
        let (warned, read) = keeper_warnings(&lines, "closed the desktop's input");

        assert!(read, "the keeper's lines were not read: {lines:?}");
        assert!(
            warned.is_empty(),
            "the keeper warned in an X server's death: {warned:?}"
        );
        assert!(!still_running(&wm), "the window manager outlived it all");
    }

    /// Review R2-2 — the notice the two tests above keep quiet is still said
    /// when it is true. A window manager that exits while its X server runs
    /// is collected by the keeper and reported with its pid once the exit is
    /// the keeper's `NOTICE_DELAY` (2 s) old, with the delay after which it
    /// is started again (#4099; [`plan`]'s hour here, so nothing starts in
    /// this test). The line said that the desktop runs on without one until
    /// #4099 decided otherwise (#3665's B2 collected and logged only).
    #[test]
    fn a_window_manager_that_exits_while_its_server_runs_is_reported() {
        let dir = scratch("wm-exits");
        let wm_body = format!("echo $$ > \"{}/wm.pid\"\nexec sleep 300", dir.display());
        let (desk, x_pid, lines) = desktop_keeping_lines(&dir, ":478", &wm_body);
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let wm = recorded(&dir, "wm.pid");
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(running(&desk, x_pid, Some(keeper)))))
        };
        let notice = |lines: &KeeperLines| {
            keeper_warnings(lines, "")
                .0
                .into_iter()
                .find(|l| l.contains("the window manager"))
        };
        // Taken before the signal, so that the exit cannot come before it.
        let signalled = Instant::now();
        // The window manager this test's desktop started, while `/proc`
        // still shows it.
        if RealProcs.same_process_now(&wm) {
            RealProcs.signal(wm.pid, libc::SIGTERM);
        }
        wait_for("the keeper's notice", || notice(&lines).is_some());
        let after = signalled.elapsed();
        let line = notice(&lines).expect("the notice");
        let server_ran = is_running(x_pid);
        let keeper_ran = is_running(keeper);
        let up = desk.state().up;
        let starts = runs(&dir).len();
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            line.contains(&format!("the window manager ({})", wm.pid)),
            "the notice does not name the window manager's pid: {line}"
        );
        assert!(
            line.contains("— it is started again 3600s after its exit"),
            "{line}"
        );
        assert!(
            after >= Duration::from_secs(2),
            "said {after:?} after the signal, before the notice was due: {line}"
        );
        assert!(server_ran, "the X server stopped with its window manager");
        assert!(keeper_ran, "the keeper left with the window manager");
        assert_eq!(up, Some(Server::Xvfb), "the desktop went down");
        assert_eq!(starts, 0, "the desktop was started again");
    }

    // -----------------------------------------------------------------------
    // #4099: a window manager that exits while its X server runs is started
    // again by the keeper, through the function that started it, with the X
    // server's backoff; never in a teardown, and never onto a server that is
    // gone. Stand-ins only: no X server, displays `:490`-`:498`, never `:99`.
    // -----------------------------------------------------------------------

    /// One start of a window manager stand-in, as its first line wrote it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct WmStart {
        pid: i32,
        /// Who started it: the keeper, on the keeper's path.
        parent: i32,
        /// When it ran, in nanoseconds since the epoch (`date +%s%N`).
        at_ns: i128,
    }

    /// A window manager stand-in's first line: each start appends
    /// `$$ $PPID <now>` to `<dir>/wm.pids`, in one short write.
    fn wm_started(dir: &Path) -> String {
        format!(
            "echo \"$$ $PPID $(date +%s%N)\" >> \"{}/wm.pids\"",
            dir.display()
        )
    }

    /// The whole lines of `<dir>/<name>` so far, each split at its blanks: a
    /// line still being written is not read in half.
    fn whole_lines(dir: &Path, name: &str) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        let whole = text.rfind('\n').map_or("", |end| &text[..end]);
        whole
            .lines()
            .map(|l| l.split_whitespace().map(String::from).collect())
            .collect()
    }

    /// The window manager's starts so far, in order.
    fn wm_starts(dir: &Path) -> Vec<WmStart> {
        whole_lines(dir, "wm.pids")
            .iter()
            .filter_map(|f| {
                Some(WmStart {
                    pid: f.first()?.parse().ok()?,
                    parent: f.get(1)?.parse().ok()?,
                    at_ns: f.get(2)?.parse().ok()?,
                })
            })
            .collect()
    }

    /// The window manager's first start, once it has run its first line.
    fn first_start(dir: &Path) -> WmStart {
        let mut first = None;
        wait_for("the window manager's first start", || {
            first = wm_starts(dir).first().copied();
            first.is_some()
        });
        first.unwrap()
    }

    /// Now, on the clock `date +%s%N` reads.
    fn wall_ns() -> i128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos() as i128
    }

    /// From `from` to `to`, both in nanoseconds since the epoch; zero when
    /// `to` came first.
    fn gap(from: i128, to: i128) -> Duration {
        Duration::from_nanos(u64::try_from(to - from).unwrap_or(0))
    }

    /// `look` every 10 ms until it finds something, for at most `within`:
    /// a RED ends here in its own words, never in [`wait_for`]'s 20 s panic.
    fn poll_for<T>(within: Duration, mut look: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(found) = look() {
                return Some(found);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// [`desktop_keeping_lines`] on `base`, with the window manager's restart
    /// numbers set (its delays' floor and ceiling, and how long it must run
    /// for its exit to start them over), and `wm_body` after the window
    /// manager's first line ([`wm_started`]).
    fn desktop_restarting_wm(
        dir: &Path,
        display: &str,
        wm_body: &str,
        [floor, ceil, stable]: [Duration; 3],
        base: Plan,
    ) -> (Arc<Desktop>, i32, KeeperLines) {
        let lines = KeeperLines::default();
        let wm = script(dir, "wm", &format!("{}\n{wm_body}", wm_started(dir)));
        let (desk, x_pid) = desktop_planned(
            display,
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm,
                    startup: vec![],
                },
                wm_delay_floor: floor,
                wm_delay_ceil: ceil,
                wm_stable_after: stable,
                keeper_lines: Some(lines.clone()),
                ..base
            },
        );
        (desk, x_pid, lines)
    }

    /// The supervisor of `desk` on a thread of its own, handed the server and
    /// the keeper as `bring_up` hands them.
    fn supervised(desk: &Arc<Desktop>, x_pid: i32, keeper: i32) -> std::thread::JoinHandle<()> {
        let desk = desk.clone();
        std::thread::spawn(move || supervise(&desk, Some(running(&desk, x_pid, Some(keeper)))))
    }

    /// The daemon's stop, and its supervisor gone.
    fn stopped(desk: &Desktop, supervisor: std::thread::JoinHandle<()>) {
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
    }

    /// The process of start `s` once the `sleep` its script exec'd sleeps,
    /// as `/proc` describes it: what every later look and signal checks
    /// against ([`asleep_in_its_sleep`]).
    fn wm_proc(s: &WmStart) -> ProcInfo {
        asleep_in_its_sleep(s.pid)
    }

    /// `pid` once the `sleep` it exec'd is asleep in its sleep (#4197). Only
    /// then does a look see what it holds for good, which is what it was
    /// handed. From the exec on, its name reads `sleep` while its program
    /// still starts: the loader holds each library open while it maps it, the
    /// C library opens its locale's files, and a `sleep` built in Rust ignores
    /// SIGPIPE until its `main` puts the default back. None of that sleeps
    /// (state `S`); the program's own sleep is the first thing that does. The
    /// name and the state come from one `/proc/<pid>/stat` line, one moment.
    fn asleep_in_its_sleep(pid: i32) -> ProcInfo {
        let mut stat = String::new();
        poll_for(Duration::from_secs(20), || {
            stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let name = stat.get(stat.find('(')? + 1..stat.rfind(')')?)?;
            let (state, ppid, pgid, starttime) = parse_stat(&stat)?;
            (name == "sleep" && state == "S").then_some(ProcInfo {
                pid,
                ppid,
                pgid,
                starttime,
            })
        })
        .unwrap_or_else(|| {
            panic!("the window manager {pid} was not asleep in its sleep in 20 s: {stat:?}")
        })
    }

    /// Signal the process this test recorded, while `/proc` still shows it.
    fn signal_recorded(info: &ProcInfo, sig: i32) {
        if RealProcs.same_process_now(info) {
            RealProcs.signal(info.pid, sig);
        }
    }

    /// The keeper's warnings about the window manager so far.
    fn wm_warnings(lines: &KeeperLines) -> Vec<String> {
        keeper_warnings(lines, "")
            .0
            .into_iter()
            .filter(|l| l.contains("the window manager"))
            .collect()
    }

    /// #4099, box 1 — a window manager that exits while its X server runs is
    /// started again by the keeper, no sooner than its delay's floor after
    /// the exit and within a second of it, with what the first one got from
    /// the keeper: its parent, display, environment, descriptors and signals.
    /// Nothing else moves: the server and the keeper run on, the desktop
    /// stays up and is not started again, and the new one is the only one.
    #[test]
    fn a_window_manager_that_exits_while_its_server_runs_is_started_again() {
        let dir = scratch("wm-again");
        let floor = Duration::from_millis(200);
        let (desk, x_pid, _lines) = desktop_restarting_wm(
            &dir,
            ":490",
            "exec sleep 300",
            [floor, Duration::from_secs(2), Duration::from_secs(60)],
            plan(stand_in(&dir, "exit 1")),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        let first = first_start(&dir);
        let first_wm = wm_proc(&first);
        let first_seen = seen_of(&first_wm, x_pid);
        // Taken before the signal, so that the exit cannot come before it.
        let signalled = wall_ns();
        signal_recorded(&first_wm, libc::SIGTERM);
        let again = poll_for(Duration::from_secs(5), || wm_starts(&dir).get(1).copied());
        let again_seen = again.map(|s| seen_of(&wm_proc(&s), x_pid));
        // Two floors more: a start the keeper wrongly kept due comes by then.
        std::thread::sleep(2 * floor);
        let starts = wm_starts(&dir);
        let server_ran = is_running(x_pid);
        let keeper_ran = is_running(keeper);
        let up = desk.state().up;
        let desktop_starts = runs(&dir).len();
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);

        let again =
            again.expect("the window manager was started once in 5 s after its exit (#4099)");
        let after = gap(signalled, again.at_ns);
        assert!(
            after >= floor,
            "started again {after:?} after the signal, sooner than its floor of {floor:?}"
        );
        assert!(
            after <= floor + Duration::from_secs(1),
            "started again only {after:?} after the signal"
        );
        assert_eq!(
            first.parent, keeper,
            "the first window manager is not the keeper's"
        );
        assert_eq!(
            again.parent, keeper,
            "the window manager was started again by another: {again:?}"
        );
        assert!(
            first_seen.environ.iter().any(|v| v == "DISPLAY=:490"),
            "{first_seen:#?}"
        );
        assert_eq!(
            again_seen.as_ref(),
            Some(&first_seen),
            "the window manager started again did not get what the first one got"
        );
        assert_eq!(starts.len(), 2, "the window manager's starts: {starts:?}");
        assert!(server_ran, "the X server stopped with its window manager");
        assert!(keeper_ran, "the keeper left with the window manager");
        assert_eq!(up, Some(Server::Xvfb), "the desktop went down");
        assert_eq!(desktop_starts, 0, "the desktop was started again");
    }

    /// #4197 — a window manager is read once the `sleep` it exec'd sleeps,
    /// never while that program is still starting. From the exec on, its
    /// name reads `sleep`, and its loader then holds each library open while
    /// it maps it. A stand-in named `sleep` is caught there: stopped with a
    /// library open on fd 3. A slow start, it goes on later, lets the library
    /// go and execs a real `sleep`, and only that one, asleep, may be read.
    #[test]
    fn a_window_manager_still_starting_is_read_once_it_sleeps() {
        let dir = scratch("wm-starting");
        // A library this process maps, so one every host has.
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let lib = maps
            .lines()
            .filter_map(|l| l.split_whitespace().nth(5))
            .find(|p| p.contains(".so"))
            .expect("a library this process maps")
            .to_string();
        // Its name reads `sleep` from this exec on, as a real one's does.
        std::os::unix::fs::symlink("/bin/sh", dir.join("sleep")).unwrap();
        let mut stand_in = Command::new(dir.join("sleep"))
            .arg("-c")
            .arg(format!(
                "exec 3<\"{lib}\"; kill -STOP $$; exec 3<&-; exec sleep 300"
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = stand_in.id() as i32;
        let _end = KillAtTheEnd(read_proc_stat(pid).into_iter().collect());
        wait_for("the stand-in to stop with the library open", || {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| parse_stat(&s).map(|(state, ..)| state == "T"))
                .unwrap_or(false)
        });
        let goes_on = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            RealProcs.signal(pid, libc::SIGCONT);
        });
        let seen = seen_of(
            &wm_proc(&WmStart {
                pid,
                parent: 0,
                at_ns: 0,
            }),
            0,
        );
        goes_on.join().unwrap();
        let _ = stand_in.kill();
        let _ = stand_in.wait();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            seen.fds.get(&3),
            None,
            "the window manager was read while its program was still starting: {:?}",
            seen.fds
        );
    }

    /// #4099, box 2 — a window manager that exits at once, every time, is
    /// started again after each exit, each delay twice the one before, from
    /// the floor up to the ceiling, where it stays: never sooner than its
    /// delay, never much later than the ceiling, and each exit said in one
    /// line, with the window manager's pid and the delay it was given.
    #[test]
    fn repeated_window_manager_exits_back_off_to_the_ceiling() {
        let dir = scratch("wm-backoff");
        let (floor, ceil) = (Duration::from_millis(100), Duration::from_millis(400));
        let (desk, x_pid, lines) = desktop_restarting_wm(
            &dir,
            ":491",
            "exit 0",
            [floor, ceil, Duration::from_secs(60)],
            plan(stand_in(&dir, "exit 1")),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        std::thread::sleep(Duration::from_secs(3));
        let starts = wm_starts(&dir);
        let said = wm_warnings(&lines);
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            starts.len() >= 2,
            "the window manager was started {} time(s) in 3 s, and it exits at once (#4099)",
            starts.len()
        );
        assert!(
            starts.iter().all(|s| s.parent == keeper),
            "a start that is not the keeper's ({keeper}): {starts:?}"
        );
        // 0.1 + 0.2 + 0.4 s: the fourth start, the first at the ceiling, is
        // due 0.7 s after the first.
        assert!(
            (4..=12).contains(&starts.len()),
            "{} starts in 3 s, with delays from {floor:?} doubling to {ceil:?}: {starts:?}",
            starts.len()
        );
        let mut delay = floor;
        for pair in starts.windows(2) {
            let took = gap(pair[0].at_ns, pair[1].at_ns);
            assert!(
                took + Duration::from_millis(20) >= delay,
                "started again {took:?} after the start before, sooner than its delay of {delay:?}: {starts:?}"
            );
            assert!(
                took <= ceil + Duration::from_secs(1),
                "started again only {took:?} after the start before: {starts:?}"
            );
            delay = (delay * 2).min(ceil);
        }
        assert!(
            said.len() <= starts.len(),
            "more lines than attempts: {said:?}"
        );
        let mut delay = floor;
        for (line, start) in said.iter().zip(&starts) {
            assert!(
                line.contains(&format!("the window manager ({}) exited: ", start.pid)),
                "the line does not name the window manager's pid {}: {line}",
                start.pid
            );
            assert!(
                line.contains(&format!(" — it is started again {delay:?} after its exit")),
                "the line does not name its delay of {delay:?}: {line}"
            );
            delay = (delay * 2).min(ceil);
        }
    }

    /// #4099, box 2 — a window manager that ran as long as the X server must
    /// for its exit to start the delays over gets the floor again: after
    /// three exits at once (0.1, 0.2, 0.4 s), a run of 1.5 s, past a stable
    /// time of 1 s, is started again after 0.1 s, not 0.8 s.
    #[test]
    fn a_window_manager_that_stays_up_resets_the_backoff() {
        let dir = scratch("wm-reset");
        let d = dir.display();
        let floor = Duration::from_millis(100);
        // Its own run's number (the first line counted it); the fourth run
        // lives 1.5 s, the fifth stays up, and each exit is stamped.
        let wm_body = format!(
            "n=$(wc -l < \"{d}/wm.pids\")\n\
             if [ \"$n\" -ge 5 ]; then exec sleep 300; fi\n\
             if [ \"$n\" -eq 4 ]; then sleep 1.5; fi\n\
             echo \"$$ $(date +%s%N)\" >> \"{d}/wm.exits\""
        );
        let (desk, x_pid, _lines) = desktop_restarting_wm(
            &dir,
            ":492",
            &wm_body,
            [floor, Duration::from_millis(1600), Duration::from_secs(1)],
            plan(stand_in(&dir, "exit 1")),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        let fifth = poll_for(Duration::from_secs(6), || wm_starts(&dir).get(4).copied());
        let starts = wm_starts(&dir);
        let exits: Vec<(i32, i128)> = whole_lines(&dir, "wm.exits")
            .iter()
            .filter_map(|f| Some((f.first()?.parse().ok()?, f.get(1)?.parse().ok()?)))
            .collect();
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            starts.len() >= 2,
            "the window manager was not started again after its first exit: no second start in 6 s (#4099)"
        );
        let fifth = fifth.unwrap_or_else(|| panic!("{} starts in 6 s: {starts:?}", starts.len()));
        assert!(exits.len() >= 4, "the exits: {exits:?}");
        for ((pid, _), start) in exits.iter().zip(&starts) {
            assert_eq!(
                *pid, start.pid,
                "the exits {exits:?}, the starts {starts:?}"
            );
        }
        let mut delay = floor;
        for ((_, exited), start) in exits.iter().zip(starts.iter().skip(1)).take(3) {
            let took = gap(*exited, start.at_ns);
            assert!(
                took + Duration::from_millis(20) >= delay,
                "started again {took:?} after an exit, sooner than its delay of {delay:?}"
            );
            delay *= 2;
        }
        let ran = gap(starts[3].at_ns, exits[3].1);
        assert!(
            ran >= Duration::from_secs(1),
            "the fourth run lasted {ran:?}, short of its stable time"
        );
        let took = gap(exits[3].1, fifth.at_ns);
        assert!(
            took + Duration::from_millis(20) >= floor,
            "started again {took:?} after its exit, sooner than its floor"
        );
        assert!(
            took < Duration::from_millis(400),
            "started again {took:?} after a run of {ran:?}, past its stable time of 1 s: the delays did not start over (they would be 0.8 s)"
        );
    }

    /// #4099, box 2 — a window manager that cannot be started (its file is
    /// gone) is tried once at each delay, the delays doubling as for an exit,
    /// and each failed attempt said in one line, with the window manager, its
    /// error and the next delay. Put back, it is started by the next attempt.
    #[test]
    fn a_window_manager_that_cannot_be_started_is_tried_with_the_backoff() {
        let dir = scratch("wm-gone");
        let (desk, x_pid, lines) = desktop_restarting_wm(
            &dir,
            ":493",
            "exec sleep 300",
            [
                Duration::from_millis(100),
                Duration::from_millis(800),
                Duration::from_secs(60),
            ],
            plan(stand_in(&dir, "exit 1")),
        );
        let path = dir.join("wm").display().to_string();
        let failures = |lines: &KeeperLines| -> Vec<String> {
            wm_warnings(lines)
                .into_iter()
                .filter(|l| l.contains("failed to start"))
                .collect()
        };
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        let first_wm = wm_proc(&first_start(&dir));
        std::fs::remove_file(dir.join("wm")).unwrap();
        // Taken before the signal, so that the exit cannot come before it.
        let signalled = Instant::now();
        signal_recorded(&first_wm, libc::SIGTERM);
        let tried =
            poll_for(Duration::from_secs(2), || failures(&lines).first().cloned()).is_some();
        // When each of the first three failures was first seen, then the
        // file put back whole: a new file renamed over the name.
        let mut seen = Vec::new();
        let mut back = None;
        if tried {
            while seen.len() < 3 && signalled.elapsed() < Duration::from_secs(4) {
                let n = failures(&lines).len().min(3);
                while seen.len() < n {
                    seen.push(signalled.elapsed());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            script(
                &dir,
                "wm.new",
                &format!("{}\nexec sleep 300", wm_started(&dir)),
            );
            std::fs::rename(dir.join("wm.new"), dir.join("wm")).unwrap();
            back = poll_for(Duration::from_millis(1800), || {
                wm_starts(&dir).get(1).copied()
            });
        }
        let said = failures(&lines);
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            tried,
            "no attempt in 2 s after the window manager's exit, with its file gone (#4099)"
        );
        assert_eq!(seen.len(), 3, "the failed attempts: {said:?}");
        for (k, (at, least)) in seen.iter().zip([100, 300, 700]).enumerate() {
            assert!(
                *at >= Duration::from_millis(least),
                "attempt {} seen {at:?} after the exit, sooner than its delays allow: {said:?}",
                k + 1
            );
        }
        for line in &said {
            assert!(
                line.contains(&format!("the window manager {path} failed to start: ")),
                "the line does not name the window manager: {line}"
            );
        }
        let stated: Vec<&str> = said
            .iter()
            .map(|l| {
                l.rsplit("tried again in ")
                    .next()
                    .unwrap_or_default()
                    .trim()
            })
            .collect();
        assert_eq!(
            stated.get(..3),
            Some(&["200ms", "400ms", "800ms"][..]),
            "the delays the failures named: {said:?}"
        );
        assert!(
            stated[3..].iter().all(|d| *d == "800ms"),
            "past the ceiling: {said:?}"
        );
        let back = back.expect("put back, the window manager was not started within 1.8 s");
        assert_eq!(back.parent, keeper, "started by another: {back:?}");
    }

    /// #4099 — in production the window manager's restart takes the X
    /// server's numbers: a second at first, doubling to five minutes, and a
    /// second again once it ran a minute. From the daemon's plan, and from a
    /// job line without them, as a daemon from before #4099 writes it.
    #[test]
    fn the_window_managers_restart_takes_the_servers_numbers() {
        let p = Plan::of(&Config {
            resolution: "640x480".into(),
            wm: "openbox".into(),
            startup: vec![],
        });
        assert_eq!(
            (p.wm_delay_floor, p.wm_delay_ceil, p.wm_stable_after),
            (
                Duration::from_secs(1),
                Duration::from_secs(300),
                Duration::from_secs(60)
            )
        );
        assert_eq!(
            (p.wm_delay_floor, p.wm_delay_ceil, p.wm_stable_after),
            (p.delay_floor, p.delay_ceil, p.stable_after),
            "the window manager's numbers are not the X server's"
        );
        let job: keeper::Job =
            serde_json::from_str(r#"{"display":":7","group":9,"wm":"openbox","grace_ms":1500}"#)
                .expect("a job line from before #4099");
        assert_eq!(
            (
                job.wm_delay_floor_ms,
                job.wm_delay_ceil_ms,
                job.wm_stable_after_ms
            ),
            (1_000, 300_000, 60_000)
        );
    }

    /// #4099, control — a window manager that stays up is started once.
    #[test]
    fn a_window_manager_that_stays_up_is_not_started_again() {
        let dir = scratch("wm-stays");
        let (desk, x_pid, lines) = desktop_restarting_wm(
            &dir,
            ":494",
            "exec sleep 300",
            [
                Duration::from_millis(100),
                Duration::from_millis(400),
                Duration::from_secs(60),
            ],
            plan(stand_in(&dir, "exit 1")),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        let first = first_start(&dir);
        std::thread::sleep(Duration::from_secs(2));
        let starts = wm_starts(&dir);
        let first_ran = is_running(first.pid);
        let said = wm_warnings(&lines);
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            starts,
            [first],
            "a window manager that stayed up was started again"
        );
        assert_eq!(
            first.parent, keeper,
            "the window manager is not the keeper's"
        );
        assert!(first_ran, "the window manager did not stay up");
        assert!(said.is_empty(), "the keeper warned: {said:?}");
    }

    /// #4099, control (the stop) — a window manager that exits in a stop is
    /// not started again. A service manager's stop signals every process of
    /// the unit at once; here the keeper's SIGTERM comes alone, once it has
    /// collected the window manager's exit, so the server runs on, and only
    /// the stop itself keeps the keeper, lingering for its input's end, from
    /// the start it scheduled.
    #[test]
    fn a_window_manager_that_exits_in_a_stop_is_not_started_again() {
        let dir = scratch("wm-stop");
        let floor = Duration::from_millis(500);
        let (desk, x_pid, lines) = desktop_restarting_wm(
            &dir,
            ":495",
            "exec sleep 300",
            [floor, Duration::from_secs(2), Duration::from_secs(60)],
            plan(stand_in(&dir, "exit 1")),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        let first_wm = wm_proc(&first_start(&dir));
        signal_recorded(&first_wm, libc::SIGTERM);
        wait_for("the keeper to collect the window manager", || {
            reaped(&first_wm)
        });
        // The keeper, our child, unreaped while the tree lists it.
        RealProcs.signal(keeper, libc::SIGTERM);
        // Past the start's due time, by a second.
        std::thread::sleep(floor + Duration::from_secs(1));
        let starts = wm_starts(&dir);
        let keeper_stayed = is_running(keeper);
        let server_ran = is_running(x_pid);
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);
        let (warned, read) = keeper_warnings(&lines, "asked to stop");

        assert!(read, "the keeper's lines were not read: {lines:?}");
        assert!(
            keeper_stayed,
            "the keeper left on SIGTERM alone, before the start was due"
        );
        assert!(
            server_ran,
            "the X server stopped, so its check may have kept the start out"
        );
        assert_eq!(
            starts.len(),
            1,
            "the window manager was started again in a stop: {starts:?}"
        );
        assert!(warned.is_empty(), "the keeper warned in a stop: {warned:?}");
    }

    /// #4099, control (the end of input) — a window manager that exits in the
    /// daemon's teardown is not started again: the teardown ends the keeper's
    /// input before the start is due, and the keeper takes the desktop down.
    #[test]
    fn a_window_manager_that_exits_in_the_daemons_teardown_is_not_started_again() {
        let dir = scratch("wm-teardown");
        let floor = Duration::from_millis(500);
        let (desk, x_pid, _lines) = desktop_restarting_wm(
            &dir,
            ":496",
            "exec sleep 300",
            [floor, Duration::from_secs(2), Duration::from_secs(60)],
            plan(stand_in(&dir, "exit 1")),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        let first_wm = wm_proc(&first_start(&dir));
        signal_recorded(&first_wm, libc::SIGTERM);
        wait_for("the keeper to collect the window manager", || {
            reaped(&first_wm)
        });
        stopped(&desk, supervisor);
        // Past the start's due time.
        std::thread::sleep(floor);
        let starts = wm_starts(&dir);
        let keeper_gone = collected(keeper);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            starts.len(),
            1,
            "the window manager was started again in the daemon's teardown: {starts:?}"
        );
        assert!(keeper_gone, "the stop left the keeper {keeper}");
    }

    /// #4099, control (the server) — a window manager whose X server is gone
    /// is not started again. No supervisor here: nothing takes the desktop
    /// down, so the keeper runs on past the start's due time with its input
    /// open, and only its look at the server, a zombie now, keeps the start
    /// out.
    #[test]
    fn a_window_manager_whose_server_is_gone_is_not_started_again() {
        let dir = scratch("wm-no-server");
        let (desk, x_pid, lines) = desktop_restarting_wm(
            &dir,
            ":497",
            "exec sleep 300",
            [
                Duration::from_millis(200),
                Duration::from_secs(2),
                Duration::from_secs(60),
            ],
            plan(stand_in(&dir, "exit 1")),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let first_wm = wm_proc(&first_start(&dir));
        // The server's stand-in, our child, unreaped: a zombie from now on.
        RealProcs.signal(x_pid, libc::SIGKILL);
        wait_for("the X server to die", || !is_running(x_pid));
        signal_recorded(&first_wm, libc::SIGKILL);
        wait_for("the keeper to collect the window manager", || {
            reaped(&first_wm)
        });
        // Ten floors past the window manager's exit.
        std::thread::sleep(Duration::from_secs(2));
        let starts = wm_starts(&dir);
        let keeper_ran = is_running(keeper);
        let said = wm_warnings(&lines);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            keeper_ran,
            "the keeper left, so its look at the server was not what kept the start out"
        );
        assert_eq!(
            starts.len(),
            1,
            "the window manager was started again onto a server that is gone: {starts:?}"
        );
        assert!(
            said.is_empty(),
            "the keeper warned of a desktop that did not run on: {said:?}"
        );
    }

    /// #4099, control (the order) — an X server's death ends its clients, the
    /// window manager among the first, so its exit reaches the keeper before
    /// the server's (50 ms here, as in [`an_x_server_death_adds_no_keeper_warning`]).
    /// The supervisor's teardown comes only after its stop grace (1 s, as the
    /// daemon's), past the start's due time, so only the keeper's look at the
    /// server keeps the start out.
    #[test]
    fn a_window_manager_that_exits_just_before_its_server_is_not_started_again() {
        let dir = scratch("wm-then-server");
        let floor = Duration::from_millis(500);
        let (desk, x_pid, _lines) = desktop_restarting_wm(
            &dir,
            ":498",
            "exec sleep 300",
            [floor, Duration::from_secs(2), Duration::from_secs(60)],
            plan_delayed(stand_in(&dir, "exit 1"), STOP_GRACE),
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let supervisor = supervised(&desk, x_pid, keeper);
        let first_wm = wm_proc(&first_start(&dir));
        let killed = Instant::now();
        signal_recorded(&first_wm, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(50));
        // The server's stand-in, our child, unreaped while the tree lists it.
        RealProcs.signal(x_pid, libc::SIGKILL);
        wait_for("the supervisor's teardown to collect the keeper", || {
            collected(keeper)
        });
        let keeper_lived = killed.elapsed();
        let starts: Vec<WmStart> = wm_starts(&dir)
            .into_iter()
            .filter(|s| s.parent == keeper)
            .collect();
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            keeper_lived > floor + Duration::from_millis(200),
            "the keeper was gone {keeper_lived:?} after the window manager's exit, before its start was due: this proves nothing"
        );
        assert_eq!(
            starts.len(),
            1,
            "the window manager was started again onto a dying server: {starts:?}"
        );
    }

    // -----------------------------------------------------------------------
    // #4100: the X clients Remote Apps (FR-56) starts on a desktop whose
    // keeper runs are the keeper's: collected when they exit, and ended with
    // the desktop, their orphans with them. A bash session's tmux server
    // stays outside, and without a keeper Remote Apps starts its clients as
    // before. Stand-ins only, never the real tmux, xterm, wmctrl or xprop (a
    // person's tmux is on the default socket); no X server, displays
    // `:440`-`:456`, `:480`, `:482`, `:489` and `:499`, never `:99`.
    // -----------------------------------------------------------------------

    /// Remote Apps' helper programs as stand-ins in `dir`, in the order
    /// [`crate::apps::linux_backend_for_tests`] takes them: tmux, xterm,
    /// wmctrl and xprop.
    ///
    /// tmux's `new-session -d -s <name>` starts a server stand-in in a
    /// session of its own (`setsid`, as a tmux server leaves its client's),
    /// which writes its pid to `<dir>/server.pid`, and adds `<name>` to
    /// `<dir>/sessions`; its `list-sessions` prints that file, and exits 1
    /// without one, as tmux does with no server. xterm appends
    /// `$$ <title>` to `<dir>/xterms` and sleeps. wmctrl and xprop print
    /// nothing.
    fn app_tools(dir: &Path) -> [String; 4] {
        let d = dir.display();
        [
            script(
                dir,
                "tmux",
                &format!(
                    "case \"$1\" in\n\
                     new-session)\n\
                     setsid sh -c 'echo $$ > \"{d}/server.pid\"; exec sleep 300' \
                     </dev/null >/dev/null 2>&1 &\n\
                     echo \"$4\" >> \"{d}/sessions\" ;;\n\
                     list-sessions)\n\
                     [ -s \"{d}/sessions\" ] || exit 1\n\
                     cat \"{d}/sessions\" ;;\n\
                     *) exit 1 ;;\n\
                     esac"
                ),
            ),
            script(
                dir,
                "xterm",
                &format!("echo \"$$ $2\" >> \"{d}/xterms\"\nexec sleep 300"),
            ),
            script(dir, "wmctrl", "exit 0"),
            script(dir, "xprop", "exit 0"),
        ]
    }

    /// Remote Apps' Linux backend on `display`, with the stand-ins of
    /// [`app_tools`] for its helper programs, asking `desk` who starts each X
    /// client, as the daemon asks its own desktop ([`start_x_client`]).
    fn apps_on(
        desk: &Arc<Desktop>,
        display: &str,
        dir: &Path,
    ) -> Box<dyn crate::apps::WindowManager> {
        let desk = desk.clone();
        crate::apps::linux_backend_for_tests(
            display,
            app_tools(dir),
            Arc::new(move |display: &str, program: &str, args: &[&str]| {
                desk.start_x_client(display, program, args)
            }),
        )
    }

    /// A Remote Apps entry, as `resolve_app` hands it to a backend.
    fn entry(key: &str, command: &[&str], terminal: bool, tmux: bool) -> crate::apps::ResolvedApp {
        crate::apps::ResolvedApp {
            key: key.into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            terminal,
            tmux,
        }
    }

    /// A GUI app's stand-in `<dir>/<name>`: it writes its pid to
    /// `<dir>/<name>.pid`, then sleeps.
    fn gui_app(dir: &Path, name: &str) -> String {
        script(
            dir,
            name,
            &format!("echo $$ > \"{}/{name}.pid\"\nexec sleep 300", dir.display()),
        )
    }

    /// A GUI app's stand-in that starts [`deaf_setsid_child`], writes its pid
    /// to `<dir>/app.pid`, and exits once `<dir>/app.exit` is there.
    fn orphaning_app(dir: &Path) -> String {
        let d = dir.display();
        script(
            dir,
            "app",
            &format!(
                "{}\n\
                 echo $$ > \"{d}/app.pid\"\n\
                 while [ ! -e \"{d}/app.exit\" ]; do sleep 0.05; done\n\
                 exit 0",
                deaf_setsid_child(dir)
            ),
        )
    }

    /// `desk`, around the server `x_pid`, with its keeper running and marked
    /// up, as `bring_up` leaves it: a stop for when the test leaves, and the
    /// keeper's pid.
    fn kept_up(desk: &Arc<Desktop>, x_pid: i32) -> (StopAtTheEnd, i32) {
        let stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let _ = running(desk, x_pid, Some(keeper));
        (stop, keeper)
    }

    /// The process whose pid a stand-in wrote to `<dir>/<name>`, as `/proc`
    /// describes it, within `within`: `None` when none ran.
    fn ran(dir: &Path, name: &str, within: Duration) -> Option<ProcInfo> {
        poll_for(within, || {
            let pid = std::fs::read_to_string(dir.join(name))
                .ok()?
                .trim()
                .parse::<i32>()
                .ok()?;
            read_proc_stat(pid)
        })
    }

    /// The `n`th xterm stand-in (from 0) that ran with `title`, within
    /// `within`.
    fn xterm_ran(dir: &Path, title: &str, n: usize, within: Duration) -> Option<ProcInfo> {
        poll_for(within, || {
            let pid = whole_lines(dir, "xterms")
                .into_iter()
                .filter(|f| f.get(1).is_some_and(|t| t == title))
                .nth(n)?
                .first()?
                .parse::<i32>()
                .ok()?;
            read_proc_stat(pid)
        })
    }

    /// [`orphaning_app`]'s run: the app and its child as `/proc` describes
    /// them, once the app was told to exit and its child has another parent
    /// (`None` for what never ran).
    fn orphaned(dir: &Path) -> (Option<ProcInfo>, Option<ProcInfo>) {
        let app = ran(dir, "app.pid", Duration::from_secs(5));
        let gc = ran(dir, "gc.pid", Duration::from_secs(5));
        std::fs::write(dir.join("app.exit"), "").unwrap();
        if let (Some(app), Some(gc)) = (app, gc) {
            let _ = poll_for(Duration::from_secs(5), || {
                read_proc_stat(gc.pid).filter(|p| p.ppid != app.pid)
            });
        }
        (app, gc)
    }

    /// Who `info`'s parent is, in a failure's words.
    fn parent_named(info: &ProcInfo, keeper: i32) -> String {
        if info.ppid == keeper {
            "the desktop's keeper".into()
        } else if info.ppid == std::process::id() as i32 {
            "the daemon".into()
        } else {
            format!("process {}", info.ppid)
        }
    }

    /// How a client that must not have run was started, in a failure's words.
    fn how_started(info: &ProcInfo, keeper: i32) -> String {
        if info.ppid == std::process::id() as i32 {
            "started directly, by the daemon".into()
        } else {
            format!("started, under {}", parent_named(info, keeper))
        }
    }

    /// The processes a test started that no tree may reach (a client Remote
    /// Apps started directly, as before #4100, and the orphan of one):
    /// SIGKILLed when the test leaves, as [`KillAtTheEnd`] does, and each one
    /// that is this process's own child reaped too, so that a RED leaves no
    /// zombie behind it.
    struct EndAtTheEnd(Vec<ProcInfo>);
    impl Drop for EndAtTheEnd {
        fn drop(&mut self) {
            let me = std::process::id() as i32;
            for info in &self.0 {
                if !RealProcs.same_process_now(info) {
                    continue;
                }
                RealProcs.signal(info.pid, libc::SIGKILL);
                if read_proc_stat(info.pid).is_some_and(|now| now.ppid == me) {
                    let _ = poll_for(Duration::from_secs(1), || {
                        let mut status = 0;
                        // SAFETY: `waitpid(2)` on one child of this process,
                        // with a live out-pointer; WNOHANG never blocks.
                        let got = unsafe { libc::waitpid(info.pid, &mut status, libc::WNOHANG) };
                        (got != 0).then_some(())
                    });
                }
            }
        }
    }

    /// SIGCONT for a process a test stopped, when the test leaves, whatever
    /// an assertion did.
    struct ContAtTheEnd(ProcInfo);
    impl Drop for ContAtTheEnd {
        fn drop(&mut self) {
            signal_recorded(&self.0, libc::SIGCONT);
        }
    }

    /// #4100, box 1 — a GUI entry and a terminal entry of Remote Apps,
    /// started on a desktop whose keeper runs, are the keeper's children in
    /// the desktop's group, and each one's exit is collected.
    #[test]
    fn an_app_remote_apps_starts_runs_under_the_keeper_and_its_exit_is_collected() {
        let dir = scratch("apps-kept");
        let probe = gui_app(&dir, "probe");
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":450", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":450", &dir);
        let gui = apps.launch(&entry("probe", &[probe.as_str()], false, false));
        let gui_ran = ran(&dir, "probe.pid", Duration::from_secs(5));
        let term = apps.launch(&entry("top", &["top"], true, false));
        let term_ran = xterm_ran(&dir, "roomler:app:top", 0, Duration::from_secs(5));
        let _ours = EndAtTheEnd(gui_ran.into_iter().chain(term_ran).collect());
        // Each one ends, as a person closes its window: its exit is for its
        // parent to collect.
        let collected_within = |info: &Option<ProcInfo>| {
            info.as_ref().is_some_and(|info| {
                signal_recorded(info, libc::SIGTERM);
                poll_for(Duration::from_secs(2), || reaped(info).then_some(())).is_some()
            })
        };
        let gui_collected = collected_within(&gui_ran);
        let term_collected = collected_within(&term_ran);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        if let Err(e) = &gui {
            panic!("the GUI app did not launch: {e:#}");
        }
        if let Err(e) = &term {
            panic!("the terminal app did not launch: {e:#}");
        }
        let gui_ran = gui_ran.expect("the GUI app never ran");
        let term_ran = term_ran.expect("the terminal app's xterm never ran");
        for (what, info) in [("GUI app", gui_ran), ("terminal app's xterm", term_ran)] {
            assert_eq!(
                info.ppid,
                keeper,
                "the {what}'s parent is {}, not the desktop's keeper {keeper} (#4100)",
                parent_named(&info, keeper)
            );
            assert_eq!(
                info.pgid, x_pid,
                "the {what} is not in the desktop's group (#4100)"
            );
        }
        assert!(
            gui_collected,
            "the GUI app's exit was not collected within 2 s (#4100)"
        );
        assert!(
            term_collected,
            "the terminal app's xterm's exit was not collected within 2 s (#4100)"
        );
    }

    /// #4100, box 1 — a bash session's xterm, and the xterm its re-attach
    /// starts once that one ended, are the keeper's children in the
    /// desktop's group.
    #[test]
    fn a_bash_sessions_xterm_and_its_reattach_run_under_the_keeper() {
        if !have_setsid("a_bash_sessions_xterm_and_its_reattach_run_under_the_keeper") {
            return;
        }
        let dir = scratch("apps-bash");
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":451", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":451", &dir);
        let launched = apps.launch(&entry("bash", &["bash"], true, true));
        let server = ran(&dir, "server.pid", Duration::from_secs(5));
        let first = xterm_ran(&dir, "roomler:tmux:s1", 0, Duration::from_secs(5));
        let mut ours = EndAtTheEnd(server.into_iter().chain(first).collect());
        // The xterm ends, as a person closes its window: the session is
        // detached, and a focus attaches it again in a new xterm.
        if let Some(first) = &first {
            signal_recorded(first, libc::SIGTERM);
            let _ = poll_for(Duration::from_secs(2), || {
                (!still_running(first)).then_some(())
            });
        }
        let focused = apps.focus("tmux:s1");
        let again = xterm_ran(&dir, "roomler:tmux:s1", 1, Duration::from_secs(5));
        ours.0.extend(again);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        if let Err(e) = &launched {
            panic!("the bash entry did not launch: {e:#}");
        }
        if let Err(e) = &focused {
            panic!("the re-attach did not start: {e:#}");
        }
        let first = first.expect("the bash session's xterm never ran");
        let again = again.expect("the re-attach's xterm never ran");
        for (what, info) in [
            ("bash session's xterm", first),
            ("re-attach's xterm", again),
        ] {
            assert_eq!(
                info.ppid,
                keeper,
                "the {what}'s parent is {}, not the desktop's keeper {keeper} (#4100)",
                parent_named(&info, keeper)
            );
            assert_eq!(
                info.pgid, x_pid,
                "the {what} is not in the desktop's group (#4100)"
            );
        }
    }

    /// #4100, box 2 — a Remote Apps client that leaves a child in a session
    /// of its own (`setsid`, deaf to SIGTERM) and exits: the desktop's stop
    /// ends that child, as it ends a startup app's
    /// (`an_orphan_of_an_app_that_exited_by_itself_is_reached_by_the_next_stop`).
    #[test]
    fn the_desktops_stop_reaches_the_setsid_orphan_of_a_remote_app() {
        if !have_setsid("the_desktops_stop_reaches_the_setsid_orphan_of_a_remote_app") {
            return;
        }
        let dir = scratch("apps-orphan");
        let app = orphaning_app(&dir);
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":452", "exec sleep 300");
        let (_stop, _keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":452", &dir);
        let launched = apps.launch(&entry("app", &[app.as_str()], false, false));
        let (app_ran, gc) = orphaned(&dir);
        let _ours = EndAtTheEnd(app_ran.into_iter().chain(gc).collect());
        let asked = Instant::now();
        desk.stop();
        let gone =
            gc.map(|gc| poll_for(Duration::from_secs(5), || reaped(&gc).then_some(())).is_some());
        let took = asked.elapsed();
        let _ = std::fs::remove_dir_all(&dir);

        if let Err(e) = &launched {
            panic!("the app did not launch: {e:#}");
        }
        let gc = gc.expect("the app's setsid child never ran");
        assert_eq!(
            gone,
            Some(true),
            "the setsid orphan {} of a Remote Apps client outlived the desktop's stop ({took:?}) (#4100)",
            gc.pid
        );
    }

    /// #4100, box 2 — the same orphan when its X server dies: the
    /// supervisor's teardown ends it, as it ends a startup app's
    /// (`an_orphan_of_an_app_that_died_with_its_server_does_not_outlive_the_teardown`).
    #[test]
    fn an_x_server_death_takes_the_setsid_orphan_of_a_remote_app() {
        if !have_setsid("an_x_server_death_takes_the_setsid_orphan_of_a_remote_app") {
            return;
        }
        let dir = scratch("apps-orphan-x");
        let app = orphaning_app(&dir);
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":453", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let supervisor = supervised(&desk, x_pid, keeper);
        let apps = apps_on(&desk, ":453", &dir);
        let launched = apps.launch(&entry("app", &[app.as_str()], false, false));
        let (app_ran, gc) = orphaned(&dir);
        let _ours = EndAtTheEnd(app_ran.into_iter().chain(gc).collect());
        let killed = Instant::now();
        // The server's stand-in, our child, unreaped while the tree lists it.
        RealProcs.signal(x_pid, libc::SIGKILL);
        let gone =
            gc.map(|gc| poll_for(Duration::from_secs(10), || reaped(&gc).then_some(())).is_some());
        let took = killed.elapsed();
        stopped(&desk, supervisor);
        let _ = std::fs::remove_dir_all(&dir);

        if let Err(e) = &launched {
            panic!("the app did not launch: {e:#}");
        }
        let gc = gc.expect("the app's setsid child never ran");
        assert_eq!(
            gone,
            Some(true),
            "the setsid orphan {} of a Remote Apps client outlived its X server's death ({took:?}) (#4100)",
            gc.pid
        );
    }

    /// #4100, guard — a start that reaches the keeper in its own stop (its
    /// SIGTERM: what the desktop started is taken down, the server runs on)
    /// is refused, and nothing runs.
    #[test]
    fn no_remote_app_starts_in_the_keepers_stop() {
        let dir = scratch("apps-keeper-stop");
        let probe = gui_app(&dir, "probe");
        let (desk, x_pid, lines) = desktop_keeping_lines(&dir, ":454", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":454", &dir);
        let keeper_info = read_proc_stat(keeper).expect("the keeper runs");
        signal_recorded(&keeper_info, libc::SIGTERM);
        let stopping = poll_for(Duration::from_secs(10), || {
            lines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|l| l.contains("asked to stop"))
                .then_some(())
        });
        let launched = apps.launch(&entry("probe", &[probe.as_str()], false, false));
        let probe_ran = ran(&dir, "probe.pid", Duration::from_secs(1));
        let _ours = EndAtTheEnd(probe_ran.into_iter().collect());
        let server_ran = is_running(x_pid);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(stopping.is_some(), "the keeper did not take its SIGTERM");
        if let Some(info) = probe_ran {
            panic!(
                "a Remote Apps client was {} in the keeper's stop (#4100)",
                how_started(&info, keeper)
            );
        }
        assert!(server_ran, "the server did not run on: this proves nothing");
        assert!(launched.is_err(), "the launch said it started");
    }

    /// #4100, guard — a start onto an X server that is gone (SIGKILLed: a
    /// zombie no supervisor has collected) is refused, nothing runs, and the
    /// keeper runs on.
    #[test]
    fn no_remote_app_starts_onto_a_server_that_is_gone() {
        let dir = scratch("apps-server-gone");
        let probe = gui_app(&dir, "probe");
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":455", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":455", &dir);
        let keeper_info = read_proc_stat(keeper).expect("the keeper runs");
        // The server's stand-in, our child: a zombie until the stop reaps it.
        RealProcs.signal(x_pid, libc::SIGKILL);
        let dead = poll_for(Duration::from_secs(5), || {
            (!is_running(x_pid)).then_some(())
        });
        let launched = apps.launch(&entry("probe", &[probe.as_str()], false, false));
        let probe_ran = ran(&dir, "probe.pid", Duration::from_secs(1));
        let _ours = EndAtTheEnd(probe_ran.into_iter().collect());
        let keeper_ran_on = still_running(&keeper_info);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(dead.is_some(), "the server's stand-in did not die");
        if let Some(info) = probe_ran {
            panic!(
                "a Remote Apps client was {} onto a server that is gone (#4100)",
                how_started(&info, keeper)
            );
        }
        assert!(launched.is_err(), "the launch said it started");
        assert!(keeper_ran_on, "the keeper did not run on");
    }

    /// #4100, guard — a start the daemon writes right behind the job, in the
    /// same write, is answered: the keeper hands on what it read past the
    /// job's line instead of dropping it.
    #[test]
    #[allow(clippy::zombie_processes)] // the keeper is reaped by `try_wait`, or killed and waited for
    fn a_start_written_right_behind_the_job_is_answered() {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::process::CommandExt as _;
        let dir = scratch("apps-behind-the-job");
        let probe = gui_app(&dir, "probe");
        let wm = script(&dir, "wm", "exec sleep 300");
        // The desktop's group, led by a `sleep` in the server's place.
        let mut leader = Command::new("sleep");
        leader
            .arg("300")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut leader = leader.spawn().unwrap();
        let group = leader.id() as i32;
        let mut ours = EndAtTheEnd(vec![read_proc_stat(group).expect("the group's leader")]);
        let mut cmd = KeeperCmd::test().command();
        cmd.process_group(group)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut keeper = cmd.spawn().unwrap();
        let keeper_pid = keeper.id() as i32;
        let job = keeper::Job {
            display: ":445".into(),
            group,
            wm,
            apps: vec![],
            grace_ms: KEEPER_GRACE.as_millis() as u64,
            wm_delay_floor_ms: 3_600_000,
            wm_delay_ceil_ms: 3_600_000,
            wm_stable_after_ms: 3_600_000,
        };
        let start = keeper::Start {
            id: 7,
            argv: vec![probe],
            deadline_ms: u64::MAX,
        };
        let mut input = keeper.stdin.take().unwrap();
        input
            .write_all(format!("{}{}", job.line(), start.line()).as_bytes())
            .unwrap();
        // Its lines on a thread of their own, read to the end: a keeper that
        // never speaks fails this test instead of hanging it.
        let (said, heard) = std::sync::mpsc::channel();
        let stderr = keeper.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                if said.send(line).is_err() {
                    break;
                }
            }
        });
        let first = heard.recv_timeout(Duration::from_secs(20)).ok();
        let answer = first.as_ref().and_then(|_| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match heard.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(line) if line.starts_with("start 7: ") => break Some(line),
                    Ok(_) => continue,
                    Err(_) => break None,
                }
            }
        });
        let probe_ran = ran(&dir, "probe.pid", Duration::from_secs(1));
        ours.0.extend(probe_ran);
        drop(input);
        let asked = Instant::now();
        let mut left = false;
        while asked.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(_)) = keeper.try_wait() {
                left = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !left {
            let _ = keeper.kill();
            let _ = keeper.wait();
        }
        let _ = leader.kill();
        let _ = leader.wait();
        drop(ours);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(first.as_deref(), Some("ready"));
        let answer = answer.unwrap_or_else(|| {
            panic!(
                "the keeper did not answer a start written right behind its job within 2 s (#4100)"
            )
        });
        let probe_ran =
            probe_ran.unwrap_or_else(|| panic!("the keeper answered {answer:?}, and nothing ran"));
        assert_eq!(
            probe_ran.ppid, keeper_pid,
            "the start ran, but not under the keeper"
        );
    }

    /// #4100, guard — a start longer than one atomic write to the keeper's
    /// input (`KEEPER_JOB_MAX`, `PIPE_BUF`) is refused before anything is
    /// written: the refusal names the bound, the keeper says nothing of it,
    /// and nothing runs.
    #[test]
    fn a_start_longer_than_one_atomic_write_is_refused_before_it_is_sent() {
        let dir = scratch("apps-long");
        let probe = gui_app(&dir, "probe");
        let (desk, x_pid, lines) = desktop_keeping_lines(&dir, ":456", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":456", &dir);
        let long = "x".repeat(KEEPER_JOB_MAX);
        let launched = apps.launch(&entry(
            "probe",
            &[probe.as_str(), long.as_str()],
            false,
            false,
        ));
        let probe_ran = ran(&dir, "probe.pid", Duration::from_secs(1));
        let _ours = EndAtTheEnd(probe_ran.into_iter().collect());
        let (warned, _) = keeper_warnings(&lines, "");
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        if let Some(info) = probe_ran {
            panic!(
                "a start of more than {KEEPER_JOB_MAX} bytes was {} (#4100)",
                how_started(&info, keeper)
            );
        }
        let refusal = match &launched {
            Ok(_) => panic!("the launch said it started"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            refusal.contains(&format!("more than {KEEPER_JOB_MAX} bytes")),
            "the refusal does not name the bound: {refusal}"
        );
        assert!(
            warned.is_empty(),
            "something of it reached the keeper: {warned:?}"
        );
    }

    /// #4100, guard — a start the daemon gave up on (its keeper stopped by
    /// SIGSTOP past the wait) fails its launch within the wait, and never
    /// starts later: the keeper, running again, reads it past its deadline.
    #[test]
    fn a_start_the_daemon_gave_up_on_never_starts() {
        let dir = scratch("apps-gave-up");
        let probe = gui_app(&dir, "probe");
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":480", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":480", &dir);
        let keeper_info = read_proc_stat(keeper).expect("the keeper runs");
        signal_recorded(&keeper_info, libc::SIGSTOP);
        let cont = ContAtTheEnd(keeper_info);
        let asked = Instant::now();
        let launched = apps.launch(&entry("probe", &[probe.as_str()], false, false));
        let took = asked.elapsed();
        // The keeper runs again, and reads the start now.
        drop(cont);
        let probe_ran = ran(&dir, "probe.pid", Duration::from_secs(1));
        let _ours = EndAtTheEnd(probe_ran.into_iter().collect());
        let wait = desk.plan.x_client_wait;
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        if let Some(info) = probe_ran {
            panic!(
                "a start the daemon gave up on was {} (#4100)",
                how_started(&info, keeper)
            );
        }
        assert!(launched.is_err(), "the launch said it started");
        assert!(
            took < wait + Duration::from_secs(2),
            "the launch took {took:?}, past its wait of {wait:?}"
        );
    }

    /// #4100, box 3 (control) — a bash session's tmux server is started as
    /// before, never through the keeper: it is neither the keeper's child nor
    /// in the desktop's group, and it outlives the desktop's stop.
    #[test]
    fn a_bash_sessions_tmux_server_stays_outside_the_keeper() {
        if !have_setsid("a_bash_sessions_tmux_server_stays_outside_the_keeper") {
            return;
        }
        let dir = scratch("apps-tmux");
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":482", "exec sleep 300");
        let (_stop, keeper) = kept_up(&desk, x_pid);
        let apps = apps_on(&desk, ":482", &dir);
        let launched = apps.launch(&entry("bash", &["bash"], true, true));
        let server = ran(&dir, "server.pid", Duration::from_secs(5));
        let xterm = xterm_ran(&dir, "roomler:tmux:s1", 0, Duration::from_secs(5));
        let _ours = EndAtTheEnd(server.into_iter().chain(xterm).collect());
        desk.stop();
        let outlived = server.is_some_and(|s| still_running(&s));
        let _ = std::fs::remove_dir_all(&dir);

        if let Err(e) = &launched {
            panic!("the bash entry did not launch: {e:#}");
        }
        let server = server.expect("the tmux server's stand-in never ran");
        assert_ne!(server.ppid, keeper, "the tmux server is the keeper's child");
        assert_ne!(
            server.pgid, x_pid,
            "the tmux server is in the desktop's group"
        );
        assert!(outlived, "the desktop's stop ended the tmux server");
    }

    /// #4100, box 4 (control) — where the keeper does not run, Remote Apps
    /// starts its clients as before: under the kill switch, after a keeper
    /// that refused, and on another display than the desktop's, a client is
    /// the daemon's own child, outside the desktop's group.
    #[test]
    fn where_the_keeper_does_not_run_remote_apps_starts_its_clients_as_before() {
        let dir = scratch("apps-direct");
        let sub = |name: &str| {
            let sub = dir.join(name);
            std::fs::create_dir_all(&sub).unwrap();
            sub
        };
        let (off_dir, refused_dir, kept_dir) = (sub("off"), sub("refused"), sub("kept"));
        // The kill switch.
        let (off, off_x) = desktop_with(
            &off_dir,
            ":489",
            "exec sleep 300",
            Clients::Direct,
            Duration::from_secs(20),
        );
        let _off = StopAtTheEnd(off.clone());
        assert_eq!(off.start_clients(), None, "the kill switch runs no keeper");
        let _ = running(&off, off_x, None);
        // A keeper that refuses: the clients are started directly.
        let refuses = script(
            &refused_dir,
            "keeper",
            "echo 'refused: no subreaper on this host' >&2\nexit 2",
        );
        let (refused, refused_x) = desktop_with(
            &refused_dir,
            ":499",
            "exec sleep 300",
            Clients::Keeper(KeeperCmd::script(&refuses)),
            Duration::from_secs(20),
        );
        let _refused = StopAtTheEnd(refused.clone());
        assert_eq!(refused.start_clients(), None, "the keeper refused");
        let _ = running(&refused, refused_x, None);
        // A keeper that runs, and Remote Apps on another display.
        let (kept, kept_x, _lines) = desktop_keeping_lines(&kept_dir, ":440", "exec sleep 300");
        let (_kept, _keeper) = kept_up(&kept, kept_x);

        let cases = [
            (
                "the kill switch",
                apps_on(&off, ":489", &off_dir),
                off_x,
                &off_dir,
            ),
            (
                "a keeper that refused",
                apps_on(&refused, ":499", &refused_dir),
                refused_x,
                &refused_dir,
            ),
            (
                "another display",
                apps_on(&kept, ":441", &kept_dir),
                kept_x,
                &kept_dir,
            ),
        ];
        let mut started = Vec::new();
        for (case, apps, x_pid, dir) in &cases {
            let probe = gui_app(dir, "probe");
            let launched = apps
                .launch(&entry("probe", &[probe.as_str()], false, false))
                .map_err(|e| format!("{e:#}"));
            let client = ran(dir, "probe.pid", Duration::from_secs(5));
            started.push((*case, launched, client, *x_pid));
        }
        let _ours = EndAtTheEnd(started.iter().filter_map(|s| s.2).collect());
        off.stop();
        refused.stop();
        kept.stop();
        let _ = std::fs::remove_dir_all(&dir);

        let me = std::process::id() as i32;
        for (case, launched, client, x_pid) in started {
            if let Err(e) = launched {
                panic!("{case}: the launch failed: {e}");
            }
            let info = client.unwrap_or_else(|| panic!("{case}: the client never ran"));
            assert_eq!(
                info.ppid, me,
                "{case}: the client's parent is {}, not the daemon",
                info.ppid
            );
            assert_ne!(
                info.pgid, x_pid,
                "{case}: the client is in the desktop's group"
            );
        }
    }

    /// #4100, box 4 (control) — a start that fails says what it said before
    /// #4100, on the keeper's path as on the direct one: a program that is
    /// not there, and a file that is not executable.
    #[test]
    fn a_start_that_fails_says_what_it_said_before() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("apps-fails");
        let not_executable = script(&dir, "not-executable", "exit 0");
        std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
        let missing = dir.join("not-there").display().to_string();
        let (desk, x_pid, _lines) = desktop_keeping_lines(&dir, ":442", "exec sleep 300");
        let (_stop, _keeper) = kept_up(&desk, x_pid);
        let kept = apps_on(&desk, ":442", &dir);
        // Another display than the desktop's: started directly.
        let direct = apps_on(&desk, ":443", &dir);
        let said = |apps: &dyn crate::apps::WindowManager, program: &str| {
            apps.launch(&entry("broken", &[program], false, false))
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        };
        let results: Vec<_> = [&missing, &not_executable]
            .into_iter()
            .map(|program| {
                (
                    program.clone(),
                    said(&*kept, program),
                    said(&*direct, program),
                )
            })
            .collect();
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        for (program, on_the_keepers_path, before) in results {
            assert!(before.is_err(), "{program}: the direct start did not fail");
            assert_eq!(
                on_the_keepers_path, before,
                "{program}: the keeper's path says otherwise"
            );
        }
    }

    /// #4100 (control) — a Remote Apps client's exit is not the window
    /// manager's: with the window manager's restart numbers short, a client
    /// that exits starts no window manager and says nothing of one.
    #[test]
    fn a_remote_apps_client_exit_is_not_the_window_managers() {
        let dir = scratch("apps-not-the-wm");
        let probe = gui_app(&dir, "probe");
        let (desk, x_pid, lines) = desktop_restarting_wm(
            &dir,
            ":444",
            "exec sleep 300",
            [
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_secs(60),
            ],
            plan(stand_in(&dir, "exit 1")),
        );
        let (_stop, _keeper) = kept_up(&desk, x_pid);
        let _first = first_start(&dir);
        let apps = apps_on(&desk, ":444", &dir);
        let launched = apps.launch(&entry("probe", &[probe.as_str()], false, false));
        let probe_ran = ran(&dir, "probe.pid", Duration::from_secs(5));
        let _ours = EndAtTheEnd(probe_ran.into_iter().collect());
        if let Some(probe) = &probe_ran {
            signal_recorded(probe, libc::SIGTERM);
        }
        // Past the restart's ceiling, and the delay of a notice (2 s).
        std::thread::sleep(Duration::from_millis(2500));
        let starts = wm_starts(&dir);
        let said = wm_warnings(&lines);
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        if let Err(e) = &launched {
            panic!("the client did not launch: {e:#}");
        }
        assert!(probe_ran.is_some(), "the client never ran");
        assert_eq!(
            starts.len(),
            1,
            "a client's exit started the window manager again: {starts:?}"
        );
        assert!(
            said.is_empty(),
            "a client's exit was said as the window manager's: {said:?}"
        );
    }

    // -----------------------------------------------------------------------
    // #3671: what an X server's exit tells the open sessions, and the exit a
    // stop of the daemon asked for. Stand-ins only: no X server, never `:99`.
    // -----------------------------------------------------------------------

    /// A log, as the daemon's log file writes it.
    #[derive(Clone, Default)]
    struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LogBuf {
        /// The lines that say `text`, at any level.
        fn lines_with(&self, text: &str) -> Vec<String> {
            let all = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
            String::from_utf8_lossy(&all)
                .lines()
                .filter(|l| l.contains(text))
                .map(|l| l.trim_start().to_string())
                .collect()
        }

        /// The lines logged at WARN or above.
        fn warnings(&self) -> Vec<String> {
            let text = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
            String::from_utf8_lossy(&text)
                .lines()
                .map(str::trim_start)
                .filter(|l| l.starts_with("WARN") || l.starts_with("ERROR"))
                .map(String::from)
                .collect()
        }
    }

    /// [`supervise`] on a thread of its own, whose log is kept: a
    /// subscriber set for one thread hears that thread only.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
    )]
    fn supervise_logged(
        desk: &Arc<Desktop>,
        first: XServer,
    ) -> (std::thread::JoinHandle<()>, LogBuf) {
        let log = LogBuf::default();
        let writer = log.clone();
        let desk = desk.clone();
        let supervisor = std::thread::spawn(move || {
            // Two dispatchers, never one: with one, a callsite's interest is
            // taken once for every thread.
            let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
            let subscriber = tracing_subscriber::fmt()
                .with_target(false)
                .with_ansi(false)
                .without_time()
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || supervise(&desk, Some(first)));
        });
        (supervisor, log)
    }

    /// What [`Desktop::tell_down`] was called with, and when.
    type Told = Arc<std::sync::Mutex<Vec<(String, Instant)>>>;

    fn tell_recorded(desk: &Desktop) -> Told {
        let told = Told::default();
        let seen = told.clone();
        let set = desk.tell_down.set(Box::new(move |why: &str| {
            seen.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((why.to_string(), Instant::now()));
        }));
        assert!(set.is_ok(), "the desktop already had a tell");
        told
    }

    /// #3671 — each retire of the shared pipelines recorded into `told`, in
    /// call order with the tell: whether the desktop was down or up at that
    /// moment, and its generation.
    fn retire_recorded(desk: &Arc<Desktop>, told: &Told) {
        let seen = told.clone();
        let desktop = Arc::downgrade(desk);
        let set = desk.retire.set(Box::new(move || {
            let what = match desktop.upgrade().map(|d| d.x_server_now()) {
                Some(n) => format!(
                    "retired: {} on generation {}",
                    if n.down.is_some() { "down" } else { "up" },
                    n.generation
                ),
                None => "retired: no desktop".to_string(),
            };
            seen.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, Instant::now()));
        }));
        assert!(set.is_ok(), "the desktop already had a retire");
    }

    /// [`plan`] with a restart delay of `d`, so the supervisor's grace is
    /// [`STOP_GRACE`] as the daemon's is, when `d` is at least that.
    fn plan_delayed(xvfb: String, d: Duration) -> Plan {
        Plan {
            delay_floor: d,
            delay_ceil: d,
            ..plan(xvfb)
        }
    }

    /// #3671, part 4 — a service manager's stop (and a restart, and a package
    /// upgrade's restart) signals the daemon and its X server at once, and
    /// the daemon's teardown reaches [`Desktop::stop`] about 200 ms later.
    /// The server's exit comes first, inside the supervisor's
    /// [`STOP_GRACE`]: one line at INFO, with the server's kind and its exit;
    /// no warning, nobody told, nothing started, and nothing of the tree
    /// touched, so the stop finds it all recorded and takes it down within its
    /// usual bound (#1684). Before #3671 the supervisor warned "the X server
    /// exited" and killed the tree itself.
    #[test]
    fn a_stop_the_daemon_asked_for_adds_no_warning() {
        let dir = scratch("asked-stop");
        let (desk, x_pid) =
            desktop_planned(":481", plan_delayed(stand_in(&dir, "exit 1"), STOP_GRACE));
        let _stop = StopAtTheEnd(desk.clone());
        let told = tell_recorded(&desk);
        retire_recorded(&desk, &told);
        let (supervisor, log) = supervise_logged(&desk, running(&desk, x_pid, None));
        // The server's SIGTERM, our child, unreaped while the tree lists it.
        RealProcs.signal(x_pid, libc::SIGTERM);
        // The run loop's 200 ms between the signal and its teardown.
        std::thread::sleep(Duration::from_millis(200));
        let recorded = roots_of(&desk);
        let asked = Instant::now();
        desk.stop();
        let took = asked.elapsed();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let starts = runs(&dir).len();
        let st = desk.state();
        let (left, unreaped) = (st.tree.is_some(), st.unreaped.clone());
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);

        let warned = log.warnings();
        assert!(
            warned.is_empty(),
            "a stop the daemon asked for warned: {warned:?}"
        );
        let said = log.lines_with("exited as the daemon stops");
        assert_eq!(said.len(), 1, "one line for the stop's exit: {said:?}");
        assert!(
            said[0].starts_with("INFO")
                && said[0].contains("the X server exited")
                && said[0].contains("server=\"Xvfb\"")
                && said[0].contains("signal 15 (terminated)"),
            "{said:?}"
        );
        assert_eq!(starts, 0, "the desktop was started again in a stop");
        assert!(
            told.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "in a stop, the sessions were told the desktop went down, or the shared pipelines \
             were retired"
        );
        assert_eq!(recorded, vec![x_pid], "the grace touched the tree");
        assert!(
            !left && unreaped.is_empty(),
            "the stop left {unreaped:?} recorded"
        );
        assert!(collected(x_pid), "the stop left the server {x_pid}");
        assert!(
            took < TEARDOWN_GRACE + Duration::from_secs(1),
            "the stop took {took:?}"
        );
    }

    /// #3671, part 4 — the same grace for the keeper's exit (#3665): the X
    /// server still runs during it, so an open session keeps a live picture
    /// for that second; a stop inside it is one INFO line and no warning, and
    /// takes the server, the keeper and what the keeper started down.
    #[test]
    fn a_keepers_exit_in_a_stop_adds_no_warning() {
        let dir = scratch("keeper-stop");
        let d = dir.display();
        let (desk, x_pid) = desktop_planned(
            ":485",
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: script(
                        &dir,
                        "wm",
                        &format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300"),
                    ),
                    startup: vec![],
                },
                ..plan_delayed(stand_in(&dir, "exit 1"), STOP_GRACE)
            },
        );
        let _stop = StopAtTheEnd(desk.clone());
        let told = tell_recorded(&desk);
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let wm = recorded(&dir, "wm.pid");
        let (supervisor, log) = supervise_logged(&desk, running(&desk, x_pid, Some(keeper)));
        // Our child, unreaped while the tree lists it.
        RealProcs.signal(keeper, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(200));
        let server_ran = is_running(x_pid);
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let starts = runs(&dir).len();
        let (x_gone, keeper_gone, wm_running) =
            (collected(x_pid), collected(keeper), still_running(&wm));
        let _ = std::fs::remove_dir_all(&dir);

        let warned = log.warnings();
        assert!(
            warned.is_empty(),
            "the keeper's exit in a stop warned: {warned:?}"
        );
        let said = log.lines_with("exited as the daemon stops");
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("INFO")
                && said[0].contains("the desktop's keeper (vd-keeper) exited")
                && said[0].contains("signal 9 (killed)"),
            "{said:?}"
        );
        assert!(server_ran, "the grace took the server down");
        assert_eq!(starts, 0, "the desktop was started again in a stop");
        assert!(told.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        assert!(x_gone, "the server {x_pid} was not collected");
        assert!(keeper_gone, "the keeper {keeper} was not collected");
        assert!(
            !wm_running,
            "the window manager {} outlived the stop",
            wm.pid
        );
    }

    /// #3671, part 4 — a SIGTERM that is not the daemon's stop (an operator's
    /// `kill`) is an exit nobody asked for: warned once, after the grace, and
    /// the desktop is started again.
    #[test]
    fn an_x_server_terminated_by_someone_else_warns_once() {
        let dir = scratch("term-alone");
        let (desk, x_pid) = desktop_planned(":486", plan(stand_in(&dir, "exit 1")));
        let _stop = StopAtTheEnd(desk.clone());
        let _told = tell_recorded(&desk);
        let (supervisor, log) = supervise_logged(&desk, running(&desk, x_pid, None));
        RealProcs.signal(x_pid, libc::SIGTERM);
        wait_for("the server to be collected", || collected(x_pid));
        wait_for("the next start", || !runs(&dir).is_empty());
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        let exits: Vec<String> = log
            .warnings()
            .into_iter()
            .filter(|l| l.contains("the X server exited"))
            .collect();
        assert_eq!(exits.len(), 1, "one exit, warned {exits:?}");
        assert!(exits[0].contains("signal 15 (terminated)"), "{exits:?}");
    }

    /// #3671, part 4 — the grace is carved out of the
    /// restart delay: from the exit to the next start takes the delay, as
    /// before, not the delay and the grace.
    #[test]
    fn the_grace_does_not_delay_the_next_start() {
        let dir = scratch("grace-start");
        let at = dir.join("at");
        let body = format!("date +%s%N >> \"{}\"\nexit 1", at.display());
        let (desk, x_pid) =
            desktop_planned(":487", plan_delayed(stand_in(&dir, &body), STOP_GRACE));
        let _stop = StopAtTheEnd(desk.clone());
        let (supervisor, _log) = supervise_logged(&desk, running(&desk, x_pid, None));
        let killed = std::time::SystemTime::now();
        RealProcs.signal(x_pid, libc::SIGKILL);
        // The stand-in records its run before its body writes the time: wait
        // for the time itself.
        let started = || {
            std::fs::read_to_string(&at)
                .unwrap_or_default()
                .lines()
                .next()
                .and_then(|l| l.trim().parse::<u128>().ok())
        };
        wait_for("the next start to write its time", || started().is_some());
        let ns = started().expect("the stand-in wrote when it started");
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        let killed_ns = killed
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let after = Duration::from_nanos(ns.saturating_sub(killed_ns) as u64);
        assert!(
            after >= STOP_GRACE.saturating_sub(Duration::from_millis(50)),
            "started again {after:?} after the exit, within the delay"
        );
        assert!(
            after < STOP_GRACE + Duration::from_millis(700),
            "started again {after:?} after the exit: the grace was added to the delay"
        );
        println!("vd-grace: the next start came {after:?} after the exit (delay {STOP_GRACE:?})");
    }

    /// #3671, part 4 — and an exit nobody asked for still warns, exactly once.
    #[test]
    fn an_x_server_that_dies_by_itself_warns_once() {
        let dir = scratch("dies-alone");
        let (desk, x_pid) = desktop_planned(":483", plan(stand_in(&dir, "exit 1")));
        let _stop = StopAtTheEnd(desk.clone());
        let _told = tell_recorded(&desk);
        let (supervisor, log) = supervise_logged(&desk, running(&desk, x_pid, None));
        RealProcs.signal(x_pid, libc::SIGKILL);
        wait_for("the server to be collected", || collected(x_pid));
        // The stand-in's restarts fail and say so (`did not start`); those
        // are not this exit's.
        std::thread::sleep(Duration::from_millis(300));
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        let exits: Vec<String> = log
            .warnings()
            .into_iter()
            .filter(|l| l.contains("the X server exited"))
            .collect();
        assert_eq!(exits.len(), 1, "one exit, warned {exits:?}");
        assert!(exits[0].contains("signal 9 (killed)"), "{exits:?}");
    }

    /// #3671, part 2 — an X server's exit tells the open sessions once the
    /// stop grace has passed without a stop, with the sentence a session
    /// opened meanwhile gets: `backend_error`, "this device's virtual desktop
    /// is down: … reconnect in a moment" ([`down_detail_of`]). The viewer
    /// receives it as `rc:media-unavailable` (`peer::capture_unavailable_payload`,
    /// locked by FR-80's test) and shows it over the last picture until the
    /// person reconnects. Told once per exit: the failed starts after it tell
    /// nobody again. The bound is the daemon's grace, [`STOP_GRACE`].
    #[test]
    fn an_x_server_exit_tells_the_sessions_after_the_grace() {
        let dir = scratch("tell");
        let (desk, x_pid) =
            desktop_planned(":484", plan_delayed(stand_in(&dir, "exit 1"), STOP_GRACE));
        let _stop = StopAtTheEnd(desk.clone());
        let told = tell_recorded(&desk);
        let (supervisor, _log) = supervise_logged(&desk, running(&desk, x_pid, None));
        let killed = Instant::now();
        RealProcs.signal(x_pid, libc::SIGKILL);
        wait_for("the sessions to be told", || {
            !told.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        });
        // Past a failed start or two of the stand-in.
        std::thread::sleep(Duration::from_millis(2500));
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        let told = told.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(told.len(), 1, "one exit, told {told:?}");
        let (why, at) = &told[0];
        let after = at.duration_since(killed);
        let reason = crate::capture::CaptureUnavailable::new(
            crate::capture::CaptureUnavailableCode::BackendError,
            down_detail_of(why),
        );
        assert_eq!(reason.code.wire(), "backend_error");
        assert_eq!(
            reason.detail,
            "this device's virtual desktop is down: its X server (Xvfb) exited: signal 9 \
             (killed). The daemon starts it again by itself; reconnect in a moment"
        );
        // Not before the grace: a stop that comes within it is the daemon's
        // own, and its sessions are told nothing.
        assert!(
            after >= STOP_GRACE.saturating_sub(Duration::from_millis(50))
                && after < STOP_GRACE + Duration::from_secs(1),
            "told {after:?} after the server's exit (grace {STOP_GRACE:?})"
        );
        println!(
            "vd-tell: the sessions were told {after:?} after the X server's exit (grace {STOP_GRACE:?})"
        );
    }

    /// #3671 — an X server's exit retires the shared encoder pipelines once
    /// the stop grace has passed without a stop, just before the open
    /// sessions are told; the failed starts after it retire nothing more.
    /// From the retire on, a session opened while the desktop is down runs a
    /// pump of its own, and its open is refused with the tell's sentence,
    /// instead of following a capture of the server that went. The retire
    /// once a restarted desktop is up is read against a real X server
    /// (`a_killed_server_comes_back`); a stop retires nothing
    /// (`a_stop_the_daemon_asked_for_adds_no_warning`).
    #[test]
    fn an_x_server_exit_retires_the_shared_pipelines_after_the_grace_before_the_tell() {
        let dir = scratch("retire");
        let (desk, x_pid) =
            desktop_planned(":488", plan_delayed(stand_in(&dir, "exit 1"), STOP_GRACE));
        let _stop = StopAtTheEnd(desk.clone());
        let told = tell_recorded(&desk);
        retire_recorded(&desk, &told);
        let (supervisor, _log) = supervise_logged(&desk, running(&desk, x_pid, None));
        let killed = Instant::now();
        RealProcs.signal(x_pid, libc::SIGKILL);
        wait_for("the sessions to be told", || {
            told.lock().unwrap_or_else(|e| e.into_inner()).len() >= 2
        });
        // Past a failed start or two of the stand-in.
        std::thread::sleep(Duration::from_millis(2500));
        desk.stop();
        wait_for("the supervisor to leave", || supervisor.is_finished());
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        let told = told.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let events: Vec<&str> = told.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(
            events,
            [
                "retired: down on generation 0",
                "its X server (Xvfb) exited: signal 9 (killed)"
            ],
            "retired once, with the desktop down, before the tell"
        );
        let after = told[0].1.duration_since(killed);
        assert!(
            after >= STOP_GRACE.saturating_sub(Duration::from_millis(50))
                && after < STOP_GRACE + Duration::from_secs(1),
            "retired {after:?} after the server's exit (grace {STOP_GRACE:?})"
        );
    }

    /// #3671 — the shared pipelines are retired by the tell's rule: only when
    /// a session on this device captures the desktop. A session that does not
    /// keeps sharing its pipeline.
    #[test]
    fn the_shared_pipelines_are_retired_only_when_the_plan_is_the_virtual_desktop() {
        let _serial = crate::media_share::test_serial();
        let key = crate::media_share::PipelineKey::FfmpegDc("TEST-3671-VD-RULE");
        let owner = crate::media_share::Pipeline::register(key, bson::oid::ObjectId::new());
        assert_eq!(
            retire_if(false),
            0,
            "retired though no session captures the desktop"
        );
        assert!(
            retire_if(true) >= 1,
            "the pipeline of a session that captures the desktop was left joinable"
        );
        drop(owner);
    }

    /// The tell's kill switch, `ROOMLERD_VIRTUAL_DESKTOP_TELL_SESSIONS`, which
    /// the daemon reads at each tell.
    const TELL_SWITCH: &str = "VIRTUAL_DESKTOP_TELL_SESSIONS";

    /// #3671 — the tell's kill switch, held for one test with every spelling
    /// cleared, and put back as it was when the test ends (the fields drop in
    /// order: the variable first, then the lock).
    struct TellSwitch {
        _saved: tunnel_core::env::test_env::Saved,
        _env: std::sync::MutexGuard<'static, ()>,
    }

    impl TellSwitch {
        fn hold() -> Self {
            static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let env = ENV.lock().unwrap_or_else(|e| e.into_inner());
            TellSwitch {
                _saved: tunnel_core::env::test_env::Saved::cleared(TELL_SWITCH),
                _env: env,
            }
        }

        /// The switch at `value`; `None` clears it.
        fn set(&self, value: Option<&str>) {
            // SAFETY: only the tests that hold `ENV` read or write the
            // variable: it is read by the daemon's tell, which they drive.
            match value {
                Some(v) => unsafe { tunnel_core::env::test_env::set(TELL_SWITCH, v) },
                None => unsafe { tunnel_core::env::test_env::clear(TELL_SWITCH) },
            }
        }
    }

    /// What the stand-in for FR-80's notice was handed: the session, the
    /// reason's code and detail, and whether it ran inside a runtime (the
    /// daemon's notice spawns a task, so outside one it panics).
    type Noticed = (bson::oid::ObjectId, &'static str, String, bool);

    static NOTICED: std::sync::Mutex<Vec<Noticed>> = std::sync::Mutex::new(Vec::new());

    fn stand_in_notice(
        session: bson::oid::ObjectId,
        _control_dc: crate::sleep_notice::ControlDc,
        reason: crate::capture::CaptureUnavailable,
    ) {
        let in_runtime = tokio::runtime::Handle::try_current().is_ok();
        NOTICED.lock().unwrap_or_else(|e| e.into_inner()).push((
            session,
            reason.code.wire(),
            reason.detail,
            in_runtime,
        ));
    }

    /// What the stand-in was handed for `ours` (other tests register sessions
    /// of their own).
    fn noticed(ours: &[bson::oid::ObjectId]) -> Vec<Noticed> {
        NOTICED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|n| ours.contains(&n.0))
            .cloned()
            .collect()
    }

    /// A runtime of the test's own, standing for the daemon's.
    fn daemon_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime for the notices")
    }

    /// Run `f` as the supervisor runs its hooks: on a thread in no runtime.
    /// `false` when it panicked.
    fn on_a_thread_in_no_runtime(f: impl FnOnce() + Send + 'static) -> bool {
        std::thread::spawn(move || {
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "the test's thread is in a runtime"
            );
            f();
        })
        .join()
        .is_ok()
    }

    /// #3918 — whether the tell marked each of `ours` as having no picture of
    /// the desktop. Where the desktop's X11 injector is not built nothing is
    /// marked, and there is no input of the desktop to drop.
    fn marked(ours: &[bson::oid::ObjectId]) -> Vec<bool> {
        ours.iter()
            .map(|s| crate::input::has_no_picture(*s))
            .collect()
    }

    /// #3671 — the daemon's own tell, not a recorder: called on a thread in
    /// no runtime, as the supervisor calls it, it does not panic, and every
    /// registered session gets FR-80's notice once, with `backend_error` and
    /// the down sentence, from inside the daemon's runtime. A tell that ran
    /// the notice on the calling thread would panic the supervisor's, which
    /// would then never start the desktop again. #3918 — each is marked as
    /// having no picture of the desktop by the time the tell returns, on the
    /// supervisor's thread: the stand-in notice marks nothing, so the marks
    /// are the tell's own, set before the restart can publish the desktop.
    #[test]
    fn the_daemons_tell_reaches_each_session_from_a_thread_in_no_runtime() {
        let _switch = TellSwitch::hold();
        let rt = daemon_runtime();
        let ours = [bson::oid::ObjectId::new(), bson::oid::ObjectId::new()];
        let _registered: Vec<_> = ours
            .iter()
            .map(|s| crate::sleep_notice::register(*s, Arc::new(tokio::sync::Mutex::new(None))))
            .collect();
        let tell = sessions_told_via(rt.handle().clone(), || true, stand_in_notice);
        let why = "its X server (Xvfb) exited: signal 9 (killed)";
        assert!(
            on_a_thread_in_no_runtime(move || tell(why)),
            "the tell panicked on a thread in no runtime"
        );
        let on_return = marked(&ours);
        let x11_input = cfg!(feature = "enigo-input");
        assert_eq!(
            on_return,
            vec![x11_input; 2],
            "marked as having no picture when the tell returned"
        );
        wait_for("both sessions to be told", || noticed(&ours).len() >= 2);
        // Room for a second notice to a session that was told twice.
        std::thread::sleep(Duration::from_millis(200));
        let got = noticed(&ours);
        assert_eq!(got.len(), 2, "each session told once: {got:?}");
        for (session, code, detail, in_runtime) in &got {
            assert_eq!(*code, "backend_error", "{session}");
            assert_eq!(*detail, down_detail_of(why), "{session}");
            assert!(
                *in_runtime,
                "{session} was told outside the daemon's runtime"
            );
        }
    }

    /// #3671 — `ROOMLERD_VIRTUAL_DESKTOP_TELL_SESSIONS=0` leaves every
    /// session untold, as before #3671. The switch is read at each tell: the
    /// same hook tells the session once the variable is gone. #3918 — and
    /// with the switch the session is marked all the same: it is left behind
    /// in silence, with no input of the desktop, as on master, where its
    /// injector was dead.
    #[test]
    fn the_kill_switch_leaves_every_session_untold() {
        let switch = TellSwitch::hold();
        let rt = daemon_runtime();
        let ours = [bson::oid::ObjectId::new()];
        let _registered =
            crate::sleep_notice::register(ours[0], Arc::new(tokio::sync::Mutex::new(None)));
        let tell = Arc::new(sessions_told_via(
            rt.handle().clone(),
            || true,
            stand_in_notice,
        ));
        let why = "its X server (Xvfb) exited: signal 9 (killed)";
        switch.set(Some("0"));
        let off = tell.clone();
        assert!(
            on_a_thread_in_no_runtime(move || off(why)),
            "the tell panicked"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            noticed(&ours).is_empty(),
            "told with ROOMLERD_{TELL_SWITCH}=0: {:?}",
            noticed(&ours)
        );
        assert_eq!(
            marked(&ours),
            vec![cfg!(feature = "enigo-input")],
            "with ROOMLERD_{TELL_SWITCH}=0 the session was not left behind"
        );
        switch.set(None);
        assert!(
            on_a_thread_in_no_runtime(move || tell(why)),
            "the tell panicked"
        );
        wait_for("the session to be told with the switch gone", || {
            noticed(&ours).len() == 1
        });
    }

    /// #3918 — on a device whose plan is another arm
    /// (an explicit `drm_capture` or `portal_capture`, whose open can fall
    /// through to the desktop's X server), the tell tells nobody and still
    /// marks every registered session by the time it returns: each was open
    /// at the exit, and one whose capture fell through to the X server that
    /// is gone has no picture of the new desktop. Before, the marks followed
    /// the plan, and such a session typed into the new desktop blind.
    #[test]
    fn with_another_arm_every_session_is_marked_and_none_is_told() {
        let _switch = TellSwitch::hold();
        let rt = daemon_runtime();
        let ours = [bson::oid::ObjectId::new(), bson::oid::ObjectId::new()];
        let _registered: Vec<_> = ours
            .iter()
            .map(|s| crate::sleep_notice::register(*s, Arc::new(tokio::sync::Mutex::new(None))))
            .collect();
        let tell = sessions_told_via(rt.handle().clone(), || false, stand_in_notice);
        let why = "its X server (Xvfb) exited: signal 9 (killed)";
        assert!(
            on_a_thread_in_no_runtime(move || tell(why)),
            "the tell panicked on a thread in no runtime"
        );
        let on_return = marked(&ours);
        // Room for a notice that must not come.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            on_return,
            vec![cfg!(feature = "enigo-input"); 2],
            "with another arm planned a session was not marked"
        );
        assert!(
            noticed(&ours).is_empty(),
            "told with another arm planned: {:?}",
            noticed(&ours)
        );
    }

    /// #3918 — on a device whose plan is another arm,
    /// the supervisor's retire retires nothing (it reads the plan), and the
    /// owner's mark is what keeps a new session from following a capture of
    /// the X server that is gone: once the tell has run, the pipeline of a
    /// session that was open at the exit takes no joiner, because nothing was
    /// published of its capture (its arm unknown, which fails closed), not
    /// because of the plan: a pipeline that published an arm off X takes
    /// joiners under a mark (`media_share`'s own tests). Without
    /// `enigo-input` nothing is marked, and it still takes joiners: such a
    /// build has no input of the desktop to protect (`input::mark_no_picture`).
    #[test]
    fn with_another_arm_a_marked_owners_pipeline_takes_no_joiner() {
        let _serial = crate::media_share::test_serial();
        let _switch = TellSwitch::hold();
        let rt = daemon_runtime();
        let owner_session = bson::oid::ObjectId::new();
        let _registered =
            crate::sleep_notice::register(owner_session, Arc::new(tokio::sync::Mutex::new(None)));
        let key = crate::media_share::PipelineKey::FfmpegDc("TEST-3918-ANOTHER-ARM");
        let owner = crate::media_share::Pipeline::register(key, owner_session);
        let before = crate::media_share::takes_joiners(key);
        // The supervisor's two hooks at the exit, in its order, with the
        // plan another arm.
        assert!(
            on_a_thread_in_no_runtime(pipelines_retired_via(|| false)),
            "the retire panicked on a thread in no runtime"
        );
        let after_the_retire = crate::media_share::takes_joiners(key);
        let tell = sessions_told_via(rt.handle().clone(), || false, stand_in_notice);
        assert!(
            on_a_thread_in_no_runtime(move || tell(
                "its X server (Xvfb) exited: signal 9 (killed)"
            )),
            "the tell panicked on a thread in no runtime"
        );
        let after_the_tell = crate::media_share::takes_joiners(key);
        drop(owner);
        assert!(before, "a registered pipeline takes no joiner");
        assert!(after_the_retire, "the retire ran with another arm planned");
        assert_eq!(
            after_the_tell,
            !cfg!(feature = "enigo-input"),
            "a joiner would follow a capture of the X server that is gone"
        );
    }

    /// #3671 — the daemon's own retire, not a recorder: called on a thread
    /// in no runtime, as the supervisor calls it, it does not panic and
    /// leaves no pipeline of the old X server joinable.
    #[test]
    fn the_daemons_retire_runs_on_a_thread_in_no_runtime() {
        let _serial = crate::media_share::test_serial();
        let key = crate::media_share::PipelineKey::FfmpegDc("TEST-3671-VD-HOOK");
        let owner = crate::media_share::Pipeline::register(key, bson::oid::ObjectId::new());
        assert!(
            crate::media_share::takes_joiners(key),
            "a registered pipeline takes no joiner"
        );
        assert!(
            on_a_thread_in_no_runtime(pipelines_retired_via(|| true)),
            "the retire panicked on a thread in no runtime"
        );
        assert!(
            !crate::media_share::takes_joiners(key),
            "a joiner would follow a capture of the old X server"
        );
        drop(owner);
    }

    /// #3671, part 2 — who is told is one decision for the device: every
    /// registered session when its plan is the virtual desktop's X11, and
    /// nobody when an explicit console (`drm_capture`) or portal
    /// (`portal_capture`) key plans another arm, since that session never
    /// captured the desktop. The plan is read with the desktop counted as up:
    /// it is asked as the desktop goes down. #3918 — who is marked reads no
    /// plan (`with_another_arm_every_session_is_marked_and_none_is_told`).
    #[test]
    fn the_sessions_are_told_only_when_the_plan_is_the_virtual_desktop() {
        use crate::capture::{ArmFacts, plans_the_virtual_desktop};
        let ours = [bson::oid::ObjectId::new(), bson::oid::ObjectId::new()];
        let _registered: Vec<_> = ours
            .iter()
            .map(|s| crate::sleep_notice::register(*s, Arc::new(tokio::sync::Mutex::new(None))))
            .collect();
        let told = |facts: ArmFacts| -> Vec<bson::oid::ObjectId> {
            sessions_to_tell(
                plans_the_virtual_desktop(facts),
                crate::sleep_notice::sessions(),
            )
            .into_iter()
            .map(|(s, _)| s)
            .filter(|s| ours.contains(s))
            .collect()
        };
        let vd = ArmFacts {
            x11_built: true,
            // As `arm_facts` reads it while the desktop is down.
            virtual_desktop: false,
            ..ArmFacts::default()
        };
        assert_eq!(
            told(vd).len(),
            2,
            "the plan is the desktop's: every session"
        );
        let drm = ArmFacts { drm: true, ..vd };
        assert!(told(drm).is_empty(), "drm_capture plans the console");
        let portal = ArmFacts { portal: true, ..vd };
        assert!(told(portal).is_empty(), "portal_capture plans the portal");
        let mutter = ArmFacts {
            portal: true,
            portal_via_mutter: true,
            ..vd
        };
        assert!(told(mutter).is_empty(), "mutter_capture plans mutter");
        let no_x11 = ArmFacts {
            x11_built: false,
            ..vd
        };
        assert!(
            told(no_x11).is_empty(),
            "a build without X11 captures nothing"
        );
    }

    /// Review R2-5 — how the daemon logs a keeper's line after its first
    /// answer. A refusal then (a job it could not take) warns with its
    /// reason, because the supervisor's warning that follows names only the
    /// exit code; a `warn: ` line warns; the rest is information.
    #[test]
    fn a_keepers_refusal_after_ready_is_logged_as_a_warning() {
        assert_eq!(
            keeper_line("refused: the job names another group"),
            (true, "refused: the job names another group")
        );
        assert_eq!(
            keeper_line("warn: the window manager (7) exited"),
            (true, "the window manager (7) exited")
        );
        assert_eq!(
            keeper_line("info: startup app xterm (8) exited"),
            (false, "startup app xterm (8) exited")
        );
        assert_eq!(
            keeper_line("a line of its own"),
            (false, "a line of its own")
        );
    }

    /// #3665, condition 11 — two early keeper exits in a row turn the keeper
    /// off; a keeper or a desktop that ran long enough starts the count again.
    #[test]
    fn two_early_keeper_exits_in_a_row_turn_the_keeper_off() {
        let stable = Duration::from_secs(60);
        let early = Duration::from_secs(5);
        let mut s = KeeperStrikes::default();
        assert!(!s.exited(early, stable), "one early exit");
        assert!(s.exited(early, stable), "two in a row");

        let mut s = KeeperStrikes::default();
        assert!(!s.exited(early, stable));
        s.reset();
        assert!(
            !s.exited(early, stable),
            "a desktop ran long enough in between"
        );

        let mut s = KeeperStrikes::default();
        assert!(!s.exited(early, stable));
        assert!(
            !s.exited(stable, stable),
            "a keeper that lasted is no strike"
        );
        assert!(!s.exited(early, stable), "and starts the count again");
    }

    /// #3665, condition 11, against a real Xvfb (the CI Xvfb lane): a keeper
    /// that says it is ready and then leaves, twice, costs the desktop two
    /// restarts and nothing more. The third start is the daemon's own, and
    /// that desktop stays up.
    #[test]
    fn after_two_early_keeper_exits_the_desktop_starts_directly_and_stays_up() {
        if std::env::var("ROOMLERD_TEST_X11").as_deref() != Ok("1") {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return;
        }
        let dir = scratch("keeper-strikes");
        let d = dir.display();
        let brief = script(
            &dir,
            "keeper",
            &format!("echo $$ >> \"{d}/keepers\"\necho ready >&2\nread -r job\nsleep 0.3\nexit 0"),
        );
        let display = pick_display(X11_UNIX_DIR);
        let desk = Arc::new(Desktop::new(
            display.clone(),
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: script(
                        &dir,
                        "wm",
                        &format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300"),
                    ),
                    startup: vec![],
                },
                ready_timeout: READY_TIMEOUT,
                delay_floor: Duration::from_millis(100),
                clients: Clients::Keeper(KeeperCmd::script(&brief)),
                ..plan("Xvfb".into())
            },
        ));
        let _stop = StopAtTheEnd(desk.clone());
        let first = desk.bring_up().expect("the X server comes up");
        assert!(first.keeper.is_some(), "the first start has a keeper");
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(first)))
        };
        let server_now = |desk: &Desktop| {
            let st = desk.state();
            st.up
                .and_then(|_| st.tree.as_ref())
                .filter(|t| t.keeper.is_none())
                .and_then(|t| t.roots.first().copied())
        };
        wait_for("a start without a keeper", || {
            desk.keeper_off.load(std::sync::atomic::Ordering::Relaxed)
                && server_now(&desk).is_some()
        });
        let server = server_now(&desk).unwrap();
        let wm = recorded(&dir, "wm.pid");
        std::thread::sleep(Duration::from_secs(2));
        let still = server_now(&desk);
        let keepers = std::fs::read_to_string(dir.join("keepers")).unwrap_or_default();
        desk.stop();
        supervisor.join().expect("the supervisor left cleanly");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(keepers.lines().count(), 2, "keepers started: {keepers:?}");
        assert_eq!(still, Some(server), "the direct desktop did not stay up");
        assert_eq!(
            wm.ppid,
            std::process::id() as i32,
            "the third start is the daemon's own"
        );
        assert!(collected(server), "the stop left the server running");
        println!("x11-real: after two vd-keeper exits the desktop on {display} ran without one");
    }

    /// The role a test runs this binary in to stand in for an X client: the
    /// variable names the test's scratch directory.
    const X_CLIENT_ROLE_ENV: &str = "ROOMLERD_TEST_VD_X_CLIENT";

    /// An X client, as far as its server can tell: it connects to `DISPLAY`,
    /// completes the connection setup that [`x_answers`] starts, writes its
    /// pid once it is connected, and then waits on the connection. When the
    /// server goes, the connection ends, and it exits as an X client does on
    /// its I/O error. It leaves by itself after a minute, should the test
    /// that started it be gone.
    #[test]
    fn x_client_role() {
        use std::io::{Read as _, Write as _};
        let Some(dir) = std::env::var_os(X_CLIENT_ROLE_ENV).filter(|_| alone_as("x_client_role"))
        else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let display = std::env::var("DISPLAY").unwrap_or_default();
        let sock = Path::new(X11_UNIX_DIR).join(format!("X{}", display.trim_start_matches(':')));
        let Ok(mut conn) = std::os::unix::net::UnixStream::connect(sock) else {
            std::process::exit(3);
        };
        // The setup `x_answers` sends. A reply that starts with 1 is a
        // success; its length in 4-byte units is at bytes 6 and 7.
        let mut head = [0u8; 8];
        if conn
            .write_all(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .is_err()
            || conn.read_exact(&mut head).is_err()
            || head[0] != 1
        {
            std::process::exit(3);
        }
        let mut rest = vec![0u8; usize::from(u16::from_le_bytes([head[6], head[7]])) * 4];
        if conn.read_exact(&mut rest).is_err() {
            std::process::exit(3);
        }
        std::fs::write(dir.join("xclient.pid"), std::process::id().to_string()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let _ = conn.set_read_timeout(Some(Duration::from_secs(60)));
        let mut buf = [0u8; 256];
        loop {
            match conn.read(&mut buf) {
                // The connection ended: the server is gone.
                Ok(0) => std::process::exit(1),
                Ok(_) if Instant::now() < deadline => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    std::process::exit(2)
                }
                Err(_) => std::process::exit(1),
                Ok(_) => std::process::exit(2),
            }
        }
    }

    /// One run of the issue's first path against a real X server, with the
    /// clients started as `clients` says. The server is killed, and the
    /// startup app, an X client ([`x_client_role`]), exits on the end of its
    /// connection. It leaves behind a child that had left the desktop's group
    /// (`setsid`) and ignores SIGTERM. Only then does the supervisor look:
    /// the order the field can take, made the order every run takes. The
    /// display, and whether that child still ran after the supervisor's
    /// teardown and after the stop that follows.
    fn an_x_clients_orphan_after(clients: Clients) -> (String, bool, bool) {
        let dir = scratch("orphan-real-x");
        let d = dir.display();
        let exe = std::env::current_exe().expect("the test binary");
        // The child is started BEFORE the app connects, so it holds no end of
        // the connection.
        let app = script(
            &dir,
            "app",
            &format!(
                "{}\nexport {X_CLIENT_ROLE_ENV}=\"{d}\"\n\
                 exec \"{}\" virtual_desktop::supervise_tests::x_client_role \
                 --exact --nocapture --test-threads=1",
                deaf_setsid_child(&dir),
                exe.display()
            ),
        );
        let display = pick_display(X11_UNIX_DIR);
        let by_keeper = matches!(clients, Clients::Keeper(_));
        let desk = Arc::new(Desktop::new(
            display.clone(),
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: "true".into(),
                    startup: vec![app],
                },
                ready_timeout: READY_TIMEOUT,
                delay_floor: Duration::from_millis(100),
                clients,
                ..plan("Xvfb".into())
            },
        ));
        let _stop = StopAtTheEnd(desk.clone());
        let first = desk.bring_up().expect("the X server comes up");
        let x_pid = first.pid;
        // The app records its child before it starts the X client, and the
        // child is guarded before anything here can fail: an app that has
        // exited leaves it to init, out of the stop's reach (review R2-9).
        let gc = recorded(&dir, "gc.pid");
        let _gc = KillAtTheEnd(vec![gc]);
        assert_eq!(
            first.keeper.is_some(),
            by_keeper,
            "the desktop's keeper started the clients"
        );
        let client = recorded(&dir, "xclient.pid");
        assert_eq!(gc.pgid, gc.pid, "the child leads a group of its own");

        // SAFETY: SIGKILL to a child of this test, which nothing has
        // collected: no supervisor runs yet.
        unsafe {
            libc::kill(x_pid, libc::SIGKILL);
        }
        wait_for("the X client to exit and leave its child", || {
            read_proc_stat(gc.pid).is_some_and(|p| p.ppid != client.pid)
        });
        let supervisor = {
            let desk = desk.clone();
            std::thread::spawn(move || supervise(&desk, Some(first)))
        };
        let server_now = |desk: &Desktop| {
            let st = desk.state();
            st.up
                .and_then(|_| st.tree.as_ref().and_then(|t| t.roots.first().copied()))
        };
        wait_for("the teardown, and the desktop back on a new server", || {
            server_now(&desk).is_some_and(|pid| pid != x_pid)
        });
        let after_teardown = still_running(&gc);
        desk.stop();
        supervisor.join().expect("the supervisor left cleanly");
        let after_stop = still_running(&gc);
        assert!(collected(x_pid), "the killed server was left a zombie");
        let _ = std::fs::remove_dir_all(&dir);
        (display, after_teardown, after_stop)
    }

    /// #3665, the issue's first path, against a real Xvfb (the CI Xvfb lane).
    /// Started directly, as before #3665 and under the kill switch, the
    /// SIGTERM-deaf orphan of an X client outlives the supervisor's teardown
    /// and the stop: the case is real, and this test can fail. Started by the
    /// desktop's keeper, the same orphan is ended by the teardown.
    #[test]
    fn a_killed_x_server_takes_the_orphan_of_its_x_client_with_it() {
        if std::env::var("ROOMLERD_TEST_X11").as_deref() != Ok("1") {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return;
        }
        if !have_setsid("a_killed_x_server_takes_the_orphan_of_its_x_client_with_it") {
            return;
        }
        let (_, teardown, stop) = an_x_clients_orphan_after(Clients::Direct);
        assert!(
            teardown && stop,
            "started directly, the orphan should outlive the teardown and the stop \
             (teardown {teardown}, stop {stop}): the case did not happen, so the \
             run with the desktop's keeper proves nothing"
        );
        let (display, teardown, stop) =
            an_x_clients_orphan_after(Clients::Keeper(KeeperCmd::test()));
        assert!(
            !teardown,
            "with the desktop's keeper, the orphan outlived the supervisor's teardown"
        );
        assert!(
            !stop,
            "with the desktop's keeper, the orphan outlived the stop"
        );
        println!(
            "x11-real: a killed X server on {display} took the orphan of its X client with it \
             (started directly, the orphan outlived it)"
        );
    }

    /// Is this test binary running `test` of this module alone, as a role
    /// started by [`rerun`]? A role's variable that leaked into a plain run
    /// of the suite then runs nothing.
    fn alone_as(test: &str) -> bool {
        keeper::tests::runs_alone_as(&format!("virtual_desktop::supervise_tests::{test}"))
    }

    /// The role a test runs this binary in to stand in for the daemon: the
    /// variable names the test's scratch directory.
    const DAEMON_ROLE_ENV: &str = "ROOMLERD_TEST_VD_DAEMON";

    /// Run this test binary as one test of this module, alone, with `env`
    /// set: a role. The test's name is the module path without the crate.
    fn rerun(test: &str, env: (&str, &std::ffi::OsStr)) -> Command {
        let mut cmd = Command::new(std::env::current_exe().expect("the test binary"));
        cmd.args([
            format!("virtual_desktop::supervise_tests::{test}").as_str(),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(env.0, env.1)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        cmd
    }

    /// A daemon, as far as the desktop can tell: it brings a desktop up with
    /// the test's scripts as the window manager and the app, the keeper
    /// started from this binary, and a `sleep` in the server's place; writes
    /// the server's and the keeper's pids; and waits to be killed. It leaves
    /// by itself after a minute, should the test that started it be gone.
    #[test]
    fn daemon_stand_in_role() {
        let Some(dir) =
            std::env::var_os(DAEMON_ROLE_ENV).filter(|_| alone_as("daemon_stand_in_role"))
        else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let (desk, x_pid) = desktop_planned(
            ":470",
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: dir.join("wm").display().to_string(),
                    startup: vec![dir.join("app").display().to_string()],
                },
                ..plan(stand_in(&dir, "exit 1"))
            },
        );
        let Some(keeper) = desk.start_clients() else {
            // No keeper: the clients started directly are this role's own,
            // and must not outlive it (review R1-5).
            desk.stop();
            std::process::exit(4);
        };
        std::fs::write(dir.join("server.pid"), x_pid.to_string()).unwrap();
        std::fs::write(dir.join("keeper.pid"), keeper.to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(0);
    }

    /// #3665 (a-ii, condition 1) — a daemon killed outright (SIGKILL, the OOM
    /// killer, a stall exit) runs no teardown. Its end closes the keeper's
    /// input, and the keeper takes the whole desktop down by itself: the
    /// server, the window manager, the app, and the app's SIGTERM-deaf
    /// orphan. It logs on the way, into the pipe of a daemon that is gone, so
    /// a keeper whose writes could fail or block would leave the orphan.
    #[test]
    fn a_killed_daemon_leaves_nothing_of_its_desktop() {
        a_killed_daemon_leaves_nothing("a_killed_daemon_leaves_nothing_of_its_desktop", false);
    }

    /// Review R1-9 — the same, while something besides the daemon holds the
    /// write end of the keeper's input (here this test; on a host, a process
    /// that came by it some other way). No end of input comes, and the keeper
    /// learns that the daemon is gone from its parent, at its next poll.
    #[test]
    fn a_killed_daemon_leaves_nothing_of_its_desktop_while_its_input_is_held() {
        a_killed_daemon_leaves_nothing(
            "a_killed_daemon_leaves_nothing_of_its_desktop_while_its_input_is_held",
            true,
        );
    }

    /// A write end of `keeper`'s input of this process's own, opened through
    /// the descriptor of `daemon` that is the same pipe.
    fn hold_keepers_input(daemon: i32, keeper: i32) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt as _;
        let pipe = fd_target(keeper, 0).expect("the keeper's input");
        let fd = fds_of(daemon)
            .into_iter()
            .find(|(_, target)| *target == pipe)
            .map(|(fd, _)| fd)
            .expect("the daemon holds the keeper's input");
        std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(format!("/proc/{daemon}/fd/{fd}"))
            .expect("a write end of the keeper's input")
    }

    fn a_killed_daemon_leaves_nothing(test: &str, hold_input: bool) {
        if !have_setsid(test) {
            return;
        }
        let dir = scratch(if hold_input {
            "daemon-killed-held"
        } else {
            "daemon-killed"
        });
        let d = dir.display();
        script(
            &dir,
            "wm",
            &format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300"),
        );
        script(
            &dir,
            "app",
            &format!(
                "{}\necho $$ > \"{d}/app.pid\"\nexec sleep 300",
                deaf_setsid_child(&dir)
            ),
        );
        let mut daemon = rerun("daemon_stand_in_role", (DAEMON_ROLE_ENV, dir.as_os_str()))
            .spawn()
            .unwrap();
        let daemon_info = read_proc_stat(daemon.id() as i32).expect("the daemon stand-in");
        let mut ours = KillAtTheEnd(vec![daemon_info]);
        let names = ["server.pid", "keeper.pid", "wm.pid", "app.pid", "gc.pid"];
        let desktop: Vec<ProcInfo> = names.iter().map(|n| recorded(&dir, n)).collect();
        ours.0.extend(desktop.iter().copied());
        // Held to the end of the test, so that the daemon's death closes
        // nothing the keeper reads.
        let _held = hold_input.then(|| hold_keepers_input(daemon.id() as i32, desktop[1].pid));

        // SAFETY: SIGKILL to this test's own child, not yet waited for.
        unsafe {
            libc::kill(daemon.id() as i32, libc::SIGKILL);
        }
        let _ = daemon.wait();
        let killed = Instant::now();
        let bound = Duration::from_secs(10);
        while desktop.iter().any(still_running) && killed.elapsed() < bound {
            std::thread::sleep(Duration::from_millis(50));
        }
        let took = killed.elapsed();
        let left: Vec<(&str, i32)> = names
            .iter()
            .zip(&desktop)
            .filter(|(_, p)| still_running(p))
            .map(|(n, p)| (*n, p.pid))
            .collect();
        drop(ours);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            left.is_empty(),
            "{took:?} after the daemon was killed, still running: {left:?}"
        );
    }

    /// The pipe a descriptor of `pid` is, as `/proc` names it (`pipe:[N]`),
    /// or what else it links to.
    fn fd_target(pid: i32, fd: i32) -> Option<String> {
        std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
            .ok()
            .map(|p| p.display().to_string())
    }

    /// Every descriptor `pid` holds, and what each links to.
    fn fds_of(pid: i32) -> std::collections::BTreeMap<i32, String> {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let fd = e.file_name().to_str()?.parse::<i32>().ok()?;
                        Some((fd, fd_target(pid, fd)?))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// #3665 (a-ii, conditions 2 and 8) — only the daemon holds the write end
    /// of the keeper's input, so its end means the daemon is gone or tearing
    /// down: the keeper reads it as fd 0, the daemon's end is write-only and
    /// close-on-exec, and no process of the desktop holds either pipe of the
    /// keeper's. Every client has `/dev/null` on stdin, stdout and stderr.
    #[test]
    fn only_the_daemon_holds_the_keepers_input() {
        use std::os::fd::AsRawFd as _;
        if !have_setsid("only_the_daemon_holds_the_keepers_input") {
            return;
        }
        let dir = scratch("keeper-fds");
        let d = dir.display();
        let app = script(
            &dir,
            "app",
            &format!(
                "{}\necho $$ > \"{d}/app.pid\"\nexec sleep 300",
                deaf_setsid_child(&dir)
            ),
        );
        let (desk, x_pid) = desktop_planned(
            ":471",
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: script(
                        &dir,
                        "wm",
                        &format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300"),
                    ),
                    startup: vec![app],
                },
                ..plan(stand_in(&dir, "exit 1"))
            },
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients().expect("the keeper said it was ready");
        let wm = recorded(&dir, "wm.pid");
        let app = recorded(&dir, "app.pid");
        let gc = recorded(&dir, "gc.pid");
        let _gc = KillAtTheEnd(vec![gc]);
        let me = std::process::id() as i32;
        let (end, flags) = {
            let st = desk.state();
            let fd = st.lifeline.as_ref().expect("the lifeline").as_raw_fd();
            let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap();
            let flags = info
                .lines()
                .find_map(|l| l.strip_prefix("flags:"))
                .and_then(|v| i64::from_str_radix(v.trim(), 8).ok())
                .expect("the lifeline's flags");
            (fd_target(me, fd).expect("the lifeline's pipe"), flags)
        };
        let keeper_fds = fds_of(keeper);
        let input = keeper_fds.get(&0).cloned();
        let output = keeper_fds.get(&2).cloned();
        let daemon_ends = fds_of(me).values().filter(|t| **t == end).count();
        let clients: Vec<(&str, i32, std::collections::BTreeMap<i32, String>)> = [
            ("server", x_pid),
            ("window manager", wm.pid),
            ("app", app.pid),
            ("orphan", gc.pid),
        ]
        .into_iter()
        .map(|(n, pid)| (n, pid, fds_of(pid)))
        .collect();
        desk.stop();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            input.as_deref(),
            Some(end.as_str()),
            "the keeper's fd 0 is the lifeline"
        );
        assert!(end.starts_with("pipe:"), "{end}");
        assert_eq!(daemon_ends, 1, "the daemon holds one end of the lifeline");
        assert_eq!(
            flags & libc::O_ACCMODE as i64,
            libc::O_WRONLY as i64,
            "flags {flags:o}"
        );
        assert_ne!(flags & libc::O_CLOEXEC as i64, 0, "flags {flags:o}");
        let output = output.expect("the keeper's stderr");
        assert!(output.starts_with("pipe:"), "{output}");
        for (name, pid, fds) in &clients {
            for (fd, target) in fds {
                assert!(
                    *target != end && *target != output,
                    "the {name} {pid} holds one of the keeper's pipes on fd {fd}"
                );
            }
        }
        for (name, pid, fds) in clients.iter().filter(|(n, _, _)| *n != "server") {
            for fd in 0..=2 {
                assert_eq!(
                    fds.get(&fd).map(String::as_str),
                    Some("/dev/null"),
                    "the {name} {pid}'s fd {fd}: {fds:?}"
                );
            }
        }
    }

    /// The role [`a_client_sees_the_same_on_both_paths`] runs in.
    const SAME_ROLE_ENV: &str = "ROOMLERD_TEST_VD_SAME";

    /// What a client of the desktop gets from whoever started it.
    #[derive(Debug, PartialEq, Eq)]
    struct Seen {
        environ: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        umask: Option<String>,
        ignored: Option<String>,
        blocked: Option<String>,
        group_is_the_servers: bool,
        session: i32,
        fds: std::collections::BTreeMap<i32, String>,
    }

    /// `/proc/<pid>/status`'s `key:` line.
    fn status_line(pid: i32, key: &str) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
    }

    /// Bring a desktop up on `:472` with `clients`, read what its window
    /// manager got, and stop it. Also what `start_clients` gave, and the
    /// window manager's parent: the one difference the two paths keep.
    fn seen_by_a_client(dir: &Path, clients: Clients) -> (Seen, Option<i32>, i32) {
        let d = dir.display();
        let _ = std::fs::remove_file(dir.join("wm.pid"));
        let (desk, x_pid) = desktop_planned(
            ":472",
            Plan {
                cfg: Config {
                    resolution: "640x480".into(),
                    wm: script(
                        dir,
                        "wm",
                        &format!("echo $$ > \"{d}/wm.pid\"\nexec sleep 300"),
                    ),
                    startup: vec![],
                },
                clients,
                ..plan(stand_in(dir, "exit 1"))
            },
        );
        let _stop = StopAtTheEnd(desk.clone());
        let keeper = desk.start_clients();
        // Its pid is written before its `exec sleep 300`: read it once that
        // `sleep` sleeps, never while its script or its start runs (#4197).
        let wm = asleep_in_its_sleep(pid_in(dir, "wm.pid"));
        let seen = seen_of(&wm, x_pid);
        desk.stop();
        (seen, keeper, wm.ppid)
    }

    /// What `wm`, a client of the desktop whose X server is `x_pid`, got
    /// from whoever started it.
    fn seen_of(wm: &ProcInfo, x_pid: i32) -> Seen {
        let mut environ: Vec<String> = std::fs::read(format!("/proc/{}/environ", wm.pid))
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect();
        environ.sort();
        // SAFETY: `getsid(2)` on a live pid.
        let session = unsafe { libc::getsid(wm.pid) };
        Seen {
            environ,
            cwd: std::fs::read_link(format!("/proc/{}/cwd", wm.pid)).ok(),
            umask: status_line(wm.pid, "Umask:"),
            ignored: status_line(wm.pid, "SigIgn:"),
            blocked: status_line(wm.pid, "SigBlk:"),
            group_is_the_servers: wm.pgid == x_pid,
            session,
            fds: fds_of(wm.pid),
        }
    }

    /// Is SIGHUP in a `/proc/<pid>/status` signal mask?
    fn has_sighup(mask: Option<&str>) -> bool {
        mask.and_then(|m| u64::from_str_radix(m, 16).ok())
            .is_some_and(|m| m & (1 << (libc::SIGHUP - 1)) != 0)
    }

    /// Run in a process of its own ([`a_client_sees_the_same_on_both_paths`]),
    /// where no other test can change the environment or the signals between
    /// the two looks: reads what a client gets on each path, and says whether
    /// each path was taken and what they got matches.
    #[test]
    fn same_on_both_paths_role() {
        let Some(dir) =
            std::env::var_os(SAME_ROLE_ENV).filter(|_| alone_as("same_on_both_paths_role"))
        else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        // Review R1-7: a daemon that ignores SIGHUP (one started under
        // `nohup`) passes that on to the clients it starts.
        // SAFETY: a signal disposition, in this role's process of its own.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
        let me = std::process::id() as i32;
        let (by_keeper, keeper, keepers_wm_parent) =
            seen_by_a_client(&dir, Clients::Keeper(KeeperCmd::test()));
        let (by_daemon, direct, daemons_wm_parent) = seen_by_a_client(&dir, Clients::Direct);
        // Review R1-3: each path was the one taken.
        let verdict = if keeper.is_none() || keeper != Some(keepers_wm_parent) {
            format!(
                "not the keeper's path: the keeper {keeper:?}, the window manager's parent {keepers_wm_parent}"
            )
        } else if direct.is_some() || daemons_wm_parent != me {
            format!(
                "not the direct path: the keeper {direct:?}, the window manager's parent {daemons_wm_parent}, the daemon {me}"
            )
        } else if !has_sighup(by_daemon.ignored.as_deref()) {
            format!("the daemon's own client does not ignore SIGHUP: {by_daemon:#?}")
        } else if by_keeper != by_daemon {
            format!("keeper {by_keeper:#?}\ndaemon {by_daemon:#?}")
        } else {
            "same".to_string()
        };
        std::fs::write(dir.join("verdict"), verdict).unwrap();
    }

    /// #3665, condition 6 — a client started by the keeper gets exactly what
    /// one started by the daemon gets: environment, working directory,
    /// umask, ignored and blocked signals (a SIGHUP the daemon ignores among
    /// them), process group, session and open descriptors. What differs is
    /// its parent, which is the point: the role checks that it is the keeper
    /// on one path and the daemon on the other.
    #[test]
    fn a_client_sees_the_same_on_both_paths() {
        let dir = scratch("same");
        let mut role = rerun("same_on_both_paths_role", (SAME_ROLE_ENV, dir.as_os_str()));
        let status = role.status().expect("re-run this test binary in its role");
        let verdict = std::fs::read_to_string(dir.join("verdict"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.success(), "the role failed: {status}");
        // No verdict: the role's name matched nothing, so it ran nothing.
        assert_eq!(verdict.as_deref().ok(), Some("same"), "{verdict:?}");
    }

    /// Review R1-2 — the keeper says `ready` before it has a job, and starts
    /// nothing until the job comes: a keeper the daemon gives up on there
    /// (it is killed before its input ends) never had anything to start.
    ///
    /// Review R2-3: so an input that ends before the job can only mean that
    /// the daemon died between the two. The keeper then takes the desktop's
    /// group down, its leader with it, as it does once it has its job, and
    /// leaves: a killed daemon leaves nothing of its desktop there either.
    #[test]
    #[allow(clippy::zombie_processes)] // the keeper is reaped by `try_wait`, or killed and waited for
    fn a_keeper_without_a_job_starts_nothing_and_leaves_when_its_input_ends() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        // The desktop's group, led by a `sleep` in the server's place.
        let mut leader = Command::new("sleep");
        leader
            .arg("300")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut leader = leader.spawn().unwrap();
        let group = leader.id() as i32;
        let ours = KillAtTheEnd(vec![read_proc_stat(group).expect("the group's leader")]);
        let mut cmd = KeeperCmd::test().command();
        cmd.process_group(group)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut keeper = cmd.spawn().unwrap();
        let keeper_pid = keeper.id() as i32;
        let input = keeper.stdin.take();
        // Its lines on a thread of their own: a keeper that never speaks
        // must fail this test, not hang it.
        let (said, heard) = std::sync::mpsc::channel();
        let stderr = keeper.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                if said.send(line).is_err() {
                    break;
                }
            }
        });
        let first = heard.recv_timeout(Duration::from_secs(20)).ok();
        // No job, for longer than a keeper takes to start a client.
        std::thread::sleep(Duration::from_millis(500));
        let children: Vec<i32> = RealProcs
            .snapshot()
            .iter()
            .filter(|p| p.ppid == keeper_pid)
            .map(|p| p.pid)
            .collect();
        let asked = Instant::now();
        drop(input);
        let mut status = None;
        while asked.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(s)) = keeper.try_wait() {
                status = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let took = asked.elapsed();
        if status.is_none() {
            let _ = keeper.kill();
            let _ = keeper.wait();
        }
        let leader_running = is_running(group);
        let _ = leader.kill();
        let _ = leader.wait();
        drop(ours);

        assert_eq!(first.as_deref(), Some("ready"));
        assert!(
            children.is_empty(),
            "the keeper started {children:?} before it had a job"
        );
        let status =
            status.unwrap_or_else(|| panic!("the keeper {keeper_pid} had not left after {took:?}"));
        assert!(status.success(), "the keeper left with {status}");
        // The keeper's teardown bound, NO_JOB_GRACE + 3 s.
        assert!(took < Duration::from_secs(6), "it took {took:?} to leave");
        assert!(
            !leader_running,
            "a keeper whose daemon left before its job left the desktop's group leader running"
        );
    }

    /// #3665, condition 4 — the keeper's teardown loops until it has no child
    /// left. Here a client answers SIGTERM by starting a SIGTERM-deaf child in
    /// a session of its own and leaving: an orphan born during the teardown,
    /// after the first snapshot, and adopted by the keeper. The keeper still
    /// ends it, and leaves by itself (exit 0) within its deadline instead of
    /// waiting for a child it would never see go.
    #[test]
    #[allow(clippy::zombie_processes)] // the keeper is reaped by `try_wait`, or killed and waited for
    fn the_keepers_teardown_ends_an_orphan_adopted_during_it() {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::process::CommandExt as _;
        if !have_setsid("the_keepers_teardown_ends_an_orphan_adopted_during_it") {
            return;
        }
        let dir = scratch("keeper-loop");
        let d = dir.display();
        let wm = script(
            &dir,
            "wm",
            &format!(
                "trap 'setsid sh -c \"trap \\\"\\\" TERM; echo \\$\\$ > \\\"{d}/late.pid\\\"; exec sleep 300\" & sleep 0.2; exit 0' TERM\n\
                 echo $$ > \"{d}/wm.pid\"\n\
                 while :; do sleep 0.05; done"
            ),
        );
        // The desktop's group, led by a `sleep` in the server's place.
        let mut leader = Command::new("sleep");
        leader
            .arg("300")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut leader = leader.spawn().unwrap();
        let group = leader.id() as i32;
        // From here on a failed assertion leaves nothing of this test running
        // (review R1-5): the leader at once, the window manager once known.
        let mut ours = KillAtTheEnd(vec![read_proc_stat(group).expect("the group's leader")]);
        let mut cmd = KeeperCmd::test().command();
        cmd.process_group(group)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut keeper = cmd.spawn().unwrap();
        let keeper_pid = keeper.id() as i32;
        let job = keeper::Job {
            display: ":473".into(),
            group,
            wm,
            apps: vec![],
            grace_ms: KEEPER_GRACE.as_millis() as u64,
            wm_delay_floor_ms: 3_600_000,
            wm_delay_ceil_ms: 3_600_000,
            wm_stable_after_ms: 3_600_000,
        };
        let mut input = keeper.stdin.take().unwrap();
        input.write_all(job.line().as_bytes()).unwrap();
        // Its lines on a thread of their own, read to the end: a keeper that
        // never speaks fails this test instead of hanging it.
        let (said, heard) = std::sync::mpsc::channel();
        let stderr = keeper.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                if said.send(line).is_err() {
                    break;
                }
            }
        });
        let first = heard.recv_timeout(Duration::from_secs(20)).ok();
        let wm = recorded(&dir, "wm.pid");
        ours.0.push(wm);

        // The daemon's teardown: the keeper's input ends.
        let asked = Instant::now();
        drop(input);
        let mut status = None;
        while asked.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(s)) = keeper.try_wait() {
                status = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let took = asked.elapsed();
        let late = std::fs::read_to_string(dir.join("late.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok());
        let late_running = late.is_some_and(is_running);
        if let Some(info) = late.and_then(read_proc_stat) {
            ours.0.push(info);
        }
        if status.is_none() {
            let _ = keeper.kill();
            let _ = keeper.wait();
        }
        let leader_running = is_running(group);
        let _ = leader.kill();
        let _ = leader.wait();
        drop(ours);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(first.as_deref(), Some("ready"));
        assert!(late.is_some(), "the client never started its late child");
        assert!(
            !late_running,
            "the orphan born during the teardown is still running"
        );
        assert!(
            !leader_running,
            "the end of input left the desktop's group leader running"
        );
        let status =
            status.unwrap_or_else(|| panic!("the keeper {keeper_pid} had not left after {took:?}"));
        assert!(status.success(), "the keeper left with {status}");
        assert!(took < Duration::from_secs(6), "the teardown took {took:?}");
    }
}
