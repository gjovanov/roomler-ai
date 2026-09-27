// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #1731 — the stall watchdog.
//!
//! A `roomler2` pod went completely silent twice in 9 h: not one log line,
//! `/health` unanswered, until the liveness probe killed it ~105 s later. A
//! kill takes the only evidence with it, and a runtime that makes no progress
//! cannot log why. So the evidence has to be taken by something that does not
//! need the runtime, and kept somewhere that outlives the container.
//!
//! - A **heartbeat** task on the runtime stamps an atomic every 500 ms.
//! - A plain OS **thread** looks every second. When the stamp is older than
//!   the threshold, it writes a dump, and while the stall lasts it adds a fresh
//!   sample every 20 s, so the dump shows whether the stuck threads move at all.
//! - A dump holds, for every thread of the process: its `/proc` state (the
//!   syscall and its arguments, the kernel wait channel) and its stack,
//!   symbolized. The stack is walked by the thread itself in a signal handler,
//!   because only it can walk its own stack without stopping the world. Plus
//!   the runtime's queue metrics, and how full the stdout/stderr pipes are.
//! - The dump goes to a FILE, never to stdout or through `tracing`. The leading
//!   suspect is a blocked stdout: the fmt layer writes synchronously under the
//!   process-wide stdout lock. Whatever blocked the runtime may well be blocking
//!   those too. The next process logs any unreported dump at boot
//!   ([`report_previous_dumps`]), which puts it in plain `kubectl logs`.
//!
//! Linux only, which is production. Elsewhere [`start`] runs the heartbeat and
//! the watchdog, but a dump says stacks are not captured there.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the runtime proves it is alive.
const BEAT_EVERY: Duration = Duration::from_millis(500);
/// How often the watchdog thread looks.
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// While a stall lasts, a fresh sample this often...
const RESAMPLE_EVERY: Duration = Duration::from_secs(20);
/// ...up to this many per stall. The liveness probe ends a stall ~105 s in.
const SAMPLES_PER_STALL: u32 = 4;
/// Stalls dumped per process. A process that stalls more often than this has
/// already said what it needed to.
const MAX_STALLS_DUMPED: u32 = 3;
/// Dumps kept in the directory. The oldest go at boot.
const KEEP_DUMPS: usize = 20;
const DUMP_PREFIX: &str = "stall-";
const DUMP_SUFFIX: &str = ".txt";
/// Written next to a dump once a later process has logged it.
const REPORTED_SUFFIX: &str = ".reported";

#[derive(Debug, Clone)]
pub struct Config {
    /// How long the heartbeat may go unseen before it counts as a stall.
    pub threshold: Duration,
    /// Where dumps go. It has to outlive a container restart.
    pub dump_dir: PathBuf,
    /// Also write each sample to stderr (production: yes).
    pub echo_stderr: bool,
}

/// A running watchdog. Dropping this handle leaves it running, and production
/// never stops it; [`StallWatchdog::stop`] exists for tests.
pub struct StallWatchdog {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StallWatchdog {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Has the heartbeat gone unseen for longer than `threshold`?
pub(crate) fn is_stalled(last_beat_ms: u64, now_ms: u64, threshold: Duration) -> bool {
    now_ms.saturating_sub(last_beat_ms) > threshold.as_millis() as u64
}

/// Start the heartbeat on `handle`'s runtime and the watchdog thread.
pub fn start(handle: &tokio::runtime::Handle, cfg: Config) -> std::io::Result<StallWatchdog> {
    #[cfg(target_os = "linux")]
    linux::install_handler()?;
    let origin = Instant::now();
    let beat = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let beat = beat.clone();
        let stop = stop.clone();
        handle.spawn(async move {
            let mut tick = tokio::time::interval(BEAT_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while !stop.load(Ordering::Acquire) {
                tick.tick().await;
                beat.store(origin.elapsed().as_millis() as u64, Ordering::Release);
            }
        });
    }
    let thread = {
        let handle = handle.clone();
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("stall-watchdog".into())
            .spawn(move || watch(&handle, &cfg, origin, &beat, &stop))?
    };
    Ok(StallWatchdog {
        stop,
        thread: Some(thread),
    })
}

struct Stall {
    /// The last heartbeat before the stall, on the watchdog's clock.
    last_beat_ms: u64,
    dump: Option<PathBuf>,
    samples: u32,
    last_sample: Instant,
}

fn watch(
    handle: &tokio::runtime::Handle,
    cfg: &Config,
    origin: Instant,
    beat: &AtomicU64,
    stop: &AtomicBool,
) {
    // Symbolizing parses the executable once. Do it now, while the process is
    // healthy (and here, off the boot path), not for the first time mid-stall.
    #[cfg(target_os = "linux")]
    linux::warm_symbolizer();
    let mut stall: Option<Stall> = None;
    let mut stalls_dumped = 0u32;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(LOOK_EVERY);
        let now = origin.elapsed().as_millis() as u64;
        let last = beat.load(Ordering::Acquire);
        if is_stalled(last, now, cfg.threshold) {
            match stall.as_mut() {
                None if stalls_dumped < MAX_STALLS_DUMPED => {
                    stalls_dumped += 1;
                    let text = report(handle, now - last, 1);
                    let dump = write_dump(&cfg.dump_dir, &text);
                    if cfg.echo_stderr {
                        echo_to_stderr(text);
                    }
                    stall = Some(Stall {
                        last_beat_ms: last,
                        dump,
                        samples: 1,
                        last_sample: Instant::now(),
                    });
                }
                None => {}
                Some(s)
                    if s.samples < SAMPLES_PER_STALL
                        && s.last_sample.elapsed() >= RESAMPLE_EVERY =>
                {
                    s.samples += 1;
                    s.last_sample = Instant::now();
                    let text = report(handle, now - last, s.samples);
                    if let Some(path) = &s.dump {
                        append(path, &text);
                    }
                    if cfg.echo_stderr {
                        echo_to_stderr(text);
                    }
                }
                Some(_) => {}
            }
        } else if let Some(s) = stall.take() {
            let lasted_ms = now.saturating_sub(s.last_beat_ms);
            if let Some(path) = &s.dump {
                append(
                    path,
                    &format!(
                        "\n=== recovered: the heartbeat resumed ~{lasted_ms} ms after the last beat before the stall ===\n"
                    ),
                );
            }
            // `tracing` writes stdout under its lock, the prime suspect. So
            // log from a throwaway thread: if the pipe blocks again, this
            // watchdog must still be here for the next stall.
            let dump = s.dump;
            let _ = std::thread::Builder::new()
                .name("stall-log".into())
                .spawn(move || {
                    tracing::error!(
                        stalled_ms = lasted_ms,
                        dump = ?dump,
                        "stall watchdog: the runtime stopped making progress and recovered on its own — the dump says what every thread was doing (#1731)"
                    );
                });
        }
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

fn report(handle: &tokio::runtime::Handle, heartbeat_age_ms: u64, sample: u32) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== roomler stall dump (#1731), sample {sample} ===");
    let _ = writeln!(out, "written_at_unix_ms: {}", unix_ms());
    let _ = writeln!(out, "heartbeat_age_ms: {heartbeat_age_ms}");
    let _ = writeln!(out, "server_version: {}", env!("CARGO_PKG_VERSION"));
    let m = handle.metrics();
    let _ = writeln!(
        out,
        "tokio: workers={} alive_tasks={} global_queue_depth={}",
        m.num_workers(),
        m.num_alive_tasks(),
        m.global_queue_depth()
    );
    #[cfg(target_os = "linux")]
    linux::append_process_report(&mut out);
    #[cfg(not(target_os = "linux"))]
    let _ = writeln!(out, "(thread stacks are captured on Linux only)");
    let _ = writeln!(out, "=== end of sample {sample} ===");
    out
}

/// Write a new dump file. If the configured directory refuses, try the temp
/// dir: a dump that dies with the container still beats none if the stall
/// resolves on its own.
fn write_dump(dir: &Path, text: &str) -> Option<PathBuf> {
    let name = format!("{DUMP_PREFIX}{}{DUMP_SUFFIX}", unix_ms());
    for dir in [dir.to_path_buf(), std::env::temp_dir()] {
        let path = dir.join(&name);
        let written = std::fs::create_dir_all(&dir).and_then(|()| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        });
        if written.is_ok() {
            return Some(path);
        }
    }
    None
}

fn append(path: &Path, text: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
        let _ = f.sync_all();
    }
}

/// Also put the dump on stderr, from a throwaway thread: if the log pipe is
/// what blocked, it blocks that thread and nothing else, and the text still
/// lands in the log if the pipe ever drains.
fn echo_to_stderr(text: String) {
    let _ = std::thread::Builder::new()
        .name("stall-echo".into())
        .spawn(move || {
            let _ = std::io::stderr().write_all(text.as_bytes());
        });
}

fn is_dump(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(DUMP_PREFIX) && n.ends_with(DUMP_SUFFIX))
}

fn reported_marker(dump: &Path) -> PathBuf {
    let mut name = dump.as_os_str().to_owned();
    name.push(REPORTED_SUFFIX);
    PathBuf::from(name)
}

/// At boot: log every dump a previous process left and no process has logged
/// yet, then mark it. A stall normally ends in a liveness kill, so this is
/// where its dump first becomes visible, in the log of the container that
/// replaced it. Keeps the newest [`KEEP_DUMPS`] files.
pub fn report_previous_dumps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dumps: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_dump(p))
        .collect();
    // The names embed the unix ms they were written at, so this is age order.
    dumps.sort();
    while dumps.len() > KEEP_DUMPS {
        let old = dumps.remove(0);
        let _ = std::fs::remove_file(reported_marker(&old));
        let _ = std::fs::remove_file(&old);
    }
    for dump in &dumps {
        let marker = reported_marker(dump);
        if marker.exists() {
            continue;
        }
        match std::fs::read_to_string(dump) {
            Ok(text) => {
                tracing::error!(
                    "stall watchdog: a previous process on this pod stalled (#1731); its dump {}:\n{text}",
                    dump.display()
                );
                let _ = std::fs::write(&marker, b"");
            }
            Err(e) => {
                tracing::warn!(dump = %dump.display(), error = %e, "stall watchdog: could not read a previous stall dump");
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fmt::Write as _;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const MAX_FRAMES: usize = 48;
    /// How long a signalled thread gets to walk its own stack.
    pub(super) const ANSWER_WITHIN: Duration = Duration::from_millis(250);

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    /// The thread asked to walk its stack. 0 = nobody.
    static TARGET: AtomicI32 = AtomicI32::new(0);
    /// Bumped per request, echoed by the handler when it is done. A handler
    /// that answers after its request timed out echoes a stale number.
    static REQUEST: AtomicU64 = AtomicU64::new(0);
    static ANSWERED: AtomicU64 = AtomicU64::new(0);
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    static FRAMES: [AtomicUsize; MAX_FRAMES] = [const { AtomicUsize::new(0) }; MAX_FRAMES];
    /// One capture at a time: the slots above are shared.
    static CAPTURE: Mutex<()> = Mutex::new(());

    /// A real-time signal no library in this server uses. Its default action
    /// terminates the process, which is why [`stack_of`] refuses to send it
    /// until the handler is installed.
    fn stack_signal() -> libc::c_int {
        libc::SIGRTMIN() + 5
    }

    pub(super) fn install_handler() -> std::io::Result<()> {
        if INSTALLED.load(Ordering::Acquire) {
            return Ok(());
        }
        // SAFETY: a zeroed `sigaction` is a valid starting value. The handler
        // only touches atomics and walks its own stack. SA_RESTART resumes the
        // syscall the signal interrupted (a futex wait, a write), so a thread
        // that answers carries on as it was.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_stack_signal as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(stack_signal(), &sa, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        INSTALLED.store(true, Ordering::Release);
        Ok(())
    }

    extern "C" fn on_stack_signal(
        _sig: libc::c_int,
        _info: *mut libc::siginfo_t,
        _ctx: *mut libc::c_void,
    ) {
        let request = REQUEST.load(Ordering::Acquire);
        // SAFETY: gettid is async-signal-safe.
        let me = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        if me != TARGET.load(Ordering::Acquire) {
            return;
        }
        let mut n = 0usize;
        // SAFETY: the closure never allocates and only stores into fixed
        // atomics. The unwinder is the one `std` panics with. Signal-safe
        // enough for a process that has already stopped. It could deadlock
        // only if this very thread was interrupted inside the loader's lock,
        // and then this thread never answers, which `stack_of` survives.
        unsafe {
            backtrace::trace_unsynchronized(|frame| {
                if n == MAX_FRAMES {
                    return false;
                }
                FRAMES[n].store(frame.ip() as usize, Ordering::Relaxed);
                n += 1;
                true
            });
        }
        COUNT.store(n, Ordering::Relaxed);
        ANSWERED.store(request, Ordering::Release);
    }

    /// Ask thread `tid` to walk its own stack. `None` if it did not answer
    /// within [`ANSWER_WITHIN`], or the handler is not installed.
    pub(super) fn stack_of(tid: i32) -> Option<Vec<usize>> {
        if !INSTALLED.load(Ordering::Acquire) {
            return None;
        }
        let _one_at_a_time = CAPTURE.lock().unwrap_or_else(|p| p.into_inner());
        let request = REQUEST.fetch_add(1, Ordering::AcqRel) + 1;
        COUNT.store(0, Ordering::Relaxed);
        TARGET.store(tid, Ordering::Release);
        // SAFETY: tgkill to a thread of this process, with a signal whose
        // handler is installed (checked above).
        let sent = unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                tid,
                stack_signal() as libc::c_long,
            )
        } == 0;
        let answered = sent && {
            let deadline = Instant::now() + ANSWER_WITHIN;
            loop {
                if ANSWERED.load(Ordering::Acquire) == request {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        TARGET.store(0, Ordering::Release);
        answered.then(|| {
            (0..COUNT.load(Ordering::Relaxed).min(MAX_FRAMES))
                .map(|i| FRAMES[i].load(Ordering::Relaxed))
                .collect()
        })
    }

    /// Name the function `ip` is in. Every frame but the innermost holds a
    /// return address, which points past its call, so look one byte back.
    pub(super) fn symbolize(ip: usize, return_address: bool) -> String {
        let at = if return_address {
            ip.wrapping_sub(1)
        } else {
            ip
        };
        let mut found: Option<String> = None;
        backtrace::resolve(at as *mut libc::c_void, |sym| {
            if found.is_none() {
                let name = sym
                    .name()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".to_string());
                found = Some(match sym.addr() {
                    Some(start) => format!("{name} +{:#x}", at.wrapping_sub(start as usize)),
                    None => name,
                });
            }
        });
        found.unwrap_or_else(|| "?".to_string())
    }

    pub(super) fn warm_symbolizer() {
        let _ = symbolize(warm_symbolizer as *const () as usize, false);
    }

    pub(super) fn gettid() -> i32 {
        // SAFETY: plain syscall, no arguments.
        unsafe { libc::syscall(libc::SYS_gettid) as i32 }
    }

    fn read_trim(path: &str) -> String {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|e| format!("? ({e})"))
    }

    pub(super) fn thread_ids() -> Vec<i32> {
        let mut tids: Vec<i32> = std::fs::read_dir("/proc/self/task")
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        tids.sort_unstable();
        tids
    }

    /// `/proc/self/task/<tid>`: name, run state, kernel wait channel, and the
    /// syscall it is in. A thread blocked writing stdout shows `write` with
    /// fd `0x1` as its first argument.
    pub(super) fn thread_info(tid: i32) -> String {
        let base = format!("/proc/self/task/{tid}");
        let comm = read_trim(&format!("{base}/comm"));
        let stat = read_trim(&format!("{base}/stat"));
        // The comm in `stat` is parenthesized and may hold spaces, so the
        // state is the first field after the LAST ')'.
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("?")
            .to_string();
        let wchan = read_trim(&format!("{base}/wchan"));
        let syscall = describe_syscall(&read_trim(&format!("{base}/syscall")));
        format!("\"{comm}\" state={state} wchan={wchan} syscall=[{syscall}]")
    }

    /// `/proc/<tid>/syscall` reads `nr arg1..arg6 sp pc` while the thread is in
    /// a syscall. Name the few this bug is about; print the rest as numbers.
    pub(super) fn describe_syscall(raw: &str) -> String {
        let mut fields = raw.split_whitespace();
        let Some(nr) = fields.next().and_then(|n| n.parse::<libc::c_long>().ok()) else {
            return raw.to_string();
        };
        let name = match nr {
            n if n == libc::SYS_write => "write",
            n if n == libc::SYS_writev => "writev",
            n if n == libc::SYS_read => "read",
            n if n == libc::SYS_futex => "futex",
            n if n == libc::SYS_epoll_pwait => "epoll_pwait",
            #[cfg(target_arch = "x86_64")]
            n if n == libc::SYS_epoll_wait => "epoll_wait",
            _ => "syscall",
        };
        let rest: Vec<&str> = fields.collect();
        format!("{name}#{nr} {}", rest.join(" "))
    }

    /// What fd 1 or 2 is, and for a pipe how much of it the reader has not
    /// taken yet. `unread_bytes == pipe_capacity` means the reader stopped.
    pub(super) fn describe_fd(fd: libc::c_int) -> String {
        let target = std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("? ({e})"));
        let mut unread: libc::c_int = 0;
        // SAFETY: FIONREAD writes one int through the pointer we pass.
        let unread = if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut unread) } == 0 {
            unread.to_string()
        } else {
            "n/a".to_string()
        };
        // SAFETY: F_GETPIPE_SZ takes no argument and fails on a non-pipe.
        let capacity = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
        let capacity = if capacity >= 0 {
            capacity.to_string()
        } else {
            "n/a".to_string()
        };
        format!("fd {fd}: {target}  unread_bytes={unread}  pipe_capacity={capacity}")
    }

    /// Where the executable is mapped, so a raw address in the dump can also
    /// be symbolized offline against the same image's binary.
    fn exe_mapping() -> String {
        let exe = std::fs::read_link("/proc/self/exe")
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        maps.lines()
            .find(|l| !exe.is_empty() && l.ends_with(&exe))
            .map(|l| format!("{exe} first mapping: {l}"))
            .unwrap_or_else(|| format!("{exe} (mapping not found)"))
    }

    /// Memory, pressure and throttling, from the process and its cgroup (v2
    /// paths inside the container). A stall in reclaim near `memory.max`, or
    /// CPU throttling, looks from outside exactly like a hang. These say
    /// whether either was happening. Missing files are skipped.
    fn append_resource_report(out: &mut String) {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in status.lines().filter(|l| {
            ["VmRSS:", "VmHWM:", "Threads:"]
                .iter()
                .any(|k| l.starts_with(k))
        }) {
            let _ = writeln!(
                out,
                "proc {}",
                line.split_whitespace().collect::<Vec<_>>().join(" ")
            );
        }
        for file in [
            "memory.current",
            "memory.max",
            "memory.events",
            "memory.pressure",
            "cpu.pressure",
            "io.pressure",
            "cpu.stat",
        ] {
            if let Ok(text) = std::fs::read_to_string(format!("/sys/fs/cgroup/{file}")) {
                let _ = writeln!(out, "cgroup {file}: {}", text.trim().replace('\n', " | "));
            }
        }
    }

    pub(super) fn append_process_report(out: &mut String) {
        let me = gettid();
        let _ = writeln!(out, "pid: {}  watchdog_tid: {me}", std::process::id());
        for fd in [1, 2] {
            let _ = writeln!(out, "{}", describe_fd(fd));
        }
        append_resource_report(out);
        let _ = writeln!(out, "exe: {}", exe_mapping());
        let tids = thread_ids();
        let _ = writeln!(out, "threads: {}", tids.len());
        for tid in tids {
            let _ = writeln!(out, "\n--- tid {tid} {}", thread_info(tid));
            if tid == me {
                let _ = writeln!(out, "    (the watchdog itself)");
                continue;
            }
            match stack_of(tid) {
                Some(ips) => {
                    for (i, ip) in ips.iter().enumerate() {
                        let _ = writeln!(out, "    #{i:<2} {ip:#014x} {}", symbolize(*ip, i > 0));
                    }
                }
                None => {
                    let _ = writeln!(
                        out,
                        "    (no stack: the thread did not answer within {ANSWER_WITHIN:?})"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heartbeat_older_than_the_threshold_is_a_stall() {
        let t = Duration::from_secs(10);
        assert!(!is_stalled(1_000, 1_000, t));
        assert!(
            !is_stalled(1_000, 11_000, t),
            "exactly the threshold is not yet a stall"
        );
        assert!(is_stalled(1_000, 11_001, t));
        // A beat stamped after `now` was read is fresh, never an underflow.
        assert!(!is_stalled(5_000, 4_000, t));
    }

    #[test]
    fn a_previous_dump_is_reported_once_and_the_oldest_are_pruned() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..(KEEP_DUMPS + 2) {
            let name = format!("{DUMP_PREFIX}{:013}{DUMP_SUFFIX}", 1_000 + i);
            std::fs::write(dir.path().join(name), format!("dump {i}")).expect("write");
        }
        std::fs::write(dir.path().join("unrelated.txt"), "keep me").expect("write");
        report_previous_dumps(dir.path());
        let dumps: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .expect("read")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| is_dump(p))
            .collect();
        assert_eq!(dumps.len(), KEEP_DUMPS, "the two oldest are pruned");
        assert!(
            dumps.iter().all(|d| reported_marker(d).exists()),
            "each is marked reported"
        );
        assert!(
            dir.path().join("unrelated.txt").exists(),
            "other files are left alone"
        );
        assert!(
            !dir.path()
                .join(format!("{DUMP_PREFIX}{:013}{DUMP_SUFFIX}", 1_000))
                .exists()
        );
    }

    #[cfg(target_os = "linux")]
    mod linux_capture {
        use super::super::linux;
        use super::super::*;
        use std::sync::mpsc;

        #[inline(never)]
        fn parked_for_the_stall_watchdog_test(release: mpsc::Receiver<()>) {
            let _ = release.recv();
            std::hint::black_box(());
        }

        /// The signalled thread walks its own stack, and the walk names the
        /// function it is parked in.
        #[test]
        fn a_signalled_thread_names_the_function_it_is_parked_in() {
            linux::install_handler().expect("handler");
            let (release_tx, release_rx) = mpsc::channel();
            let (tid_tx, tid_rx) = mpsc::channel();
            let parked = std::thread::spawn(move || {
                tid_tx.send(linux::gettid()).expect("tid");
                parked_for_the_stall_watchdog_test(release_rx);
            });
            let tid = tid_rx.recv().expect("tid");
            std::thread::sleep(Duration::from_millis(100));
            let ips = linux::stack_of(tid).expect("the parked thread answers");
            let names: Vec<String> = ips
                .iter()
                .enumerate()
                .map(|(i, ip)| linux::symbolize(*ip, i > 0))
                .collect();
            assert!(
                names
                    .iter()
                    .any(|n| n.contains("parked_for_the_stall_watchdog_test")),
                "the walk names where the thread waits: {names:#?}"
            );
            release_tx.send(()).expect("release");
            parked.join().expect("join");
        }

        /// The stdout suspect: a writer blocked on a pipe nobody reads shows
        /// `write` on that fd, and the pipe reads full.
        #[test]
        fn a_writer_blocked_on_a_full_pipe_shows_write_and_a_full_pipe() {
            let mut fds: [libc::c_int; 2] = [0; 2];
            // SAFETY: pipe fills the two-int array we pass.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let (read_fd, write_fd) = (fds[0], fds[1]);
            let (tid_tx, tid_rx) = mpsc::channel();
            let writer = std::thread::spawn(move || {
                tid_tx.send(linux::gettid()).expect("tid");
                let chunk = [b'x'; 4096];
                let mut written = 0usize;
                // Blocks for good once the pipe is full, until the test reads.
                while written < 1 << 20 {
                    // SAFETY: writing from a live stack buffer to our pipe.
                    let n = unsafe { libc::write(write_fd, chunk.as_ptr().cast(), chunk.len()) };
                    if n <= 0 {
                        break;
                    }
                    written += n as usize;
                }
            });
            let tid = tid_rx.recv().expect("tid");
            std::thread::sleep(Duration::from_millis(300));
            let info = linux::thread_info(tid);
            let pipe = linux::describe_fd(write_fd);
            assert!(
                info.contains(&format!("write#{} {:#x} ", libc::SYS_write, write_fd)),
                "the blocked writer shows write on fd {write_fd}: {info}"
            );
            let unread: usize = pipe
                .split("unread_bytes=")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .expect("unread bytes");
            let capacity: usize = pipe
                .split("pipe_capacity=")
                .nth(1)
                .and_then(|s| s.parse().ok())
                .expect("capacity");
            assert_eq!(unread, capacity, "the pipe reads full: {pipe}");
            // SAFETY: closing the read end fails the writer's blocked write
            // with EPIPE; SIGPIPE is ignored in Rust programs by default.
            unsafe { libc::close(read_fd) };
            writer.join().expect("join");
            // SAFETY: our own fd.
            unsafe { libc::close(write_fd) };
        }

        #[inline(never)]
        fn frozen_for_the_stall_watchdog_test(release: &mpsc::Receiver<()>) {
            // A blocking wait on a runtime worker: the runtime stops.
            let _ = release.recv_timeout(Duration::from_secs(30));
            std::hint::black_box(());
        }

        fn dumps_in(dir: &Path) -> Vec<PathBuf> {
            std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| is_dump(p))
                        .collect()
                })
                .unwrap_or_default()
        }

        /// End to end: a healthy runtime writes nothing. A runtime whose only
        /// worker is stuck gets a dump naming where, and the dump records the
        /// recovery once the worker is released.
        #[test]
        fn a_frozen_runtime_leaves_a_dump_naming_where_its_worker_is_stuck() {
            let dir = tempfile::tempdir().expect("tempdir");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            let watchdog = start(
                rt.handle(),
                Config {
                    threshold: Duration::from_secs(1),
                    dump_dir: dir.path().to_path_buf(),
                    echo_stderr: false,
                },
            )
            .expect("start");

            std::thread::sleep(Duration::from_millis(3_000));
            assert!(
                dumps_in(dir.path()).is_empty(),
                "a healthy runtime is never dumped"
            );

            let (release_tx, release_rx) = mpsc::channel::<()>();
            rt.spawn(async move { frozen_for_the_stall_watchdog_test(&release_rx) });
            let deadline = Instant::now() + Duration::from_secs(15);
            let dump = loop {
                if let Some(d) = dumps_in(dir.path()).into_iter().next() {
                    break d;
                }
                assert!(
                    Instant::now() < deadline,
                    "no dump within 15 s of the freeze"
                );
                std::thread::sleep(Duration::from_millis(200));
            };
            // The first sample is written in one go, so it is complete now.
            let text = std::fs::read_to_string(&dump).expect("read dump");
            assert!(text.contains("heartbeat_age_ms"), "{text}");
            assert!(
                text.contains("frozen_for_the_stall_watchdog_test"),
                "the dump names where the worker is stuck:\n{text}"
            );

            let _ = release_tx.send(());
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let text = std::fs::read_to_string(&dump).expect("read dump");
                if text.contains("=== recovered") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the recovery was never recorded:\n{text}"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            watchdog.stop();
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}
