// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3018 — every thread's stack, for the record a stall leaves behind.
//!
//! A stall's crash record said THAT the agent stopped, never WHERE. On
//! 2026-10-26 a Mac's whole tokio runtime froze for a minute inside an overlay
//! carrier flap, and the watchdog-of-watchdog's exit left one stderr line. On
//! the FR-93 lab's KDE guest the same exit followed four portal dialogs, each
//! holding one runtime worker in a blocking wait (`docs/linux-capture.md`
//! §4.6). In both, the frame each worker was stuck in is the fact that names
//! the cause, and nothing recorded it.
//!
//! The stacks are read from OUTSIDE the code that is stuck, because whatever
//! the workers wait on may be a lock this code would need too:
//!
//! | OS | source | what it gives |
//! |---|---|---|
//! | macOS | `/usr/bin/sample`, a separate process that reads ours for 1 s | every thread's user stack, symbolized. As root it reads the hardened-runtime daemon (measured on 0.4.181: 43 threads in 1.6 s) |
//! | Linux | `/proc/self/task/*` | each thread's name, state and kernel wait channel, and its kernel stack where the kernel shows it (as root) |
//! | Windows | — | nothing yet. The record still names the stall |
//!
//! The crash sidecar gets the condensed form ([`condense_sample`],
//! [`condense_procfs`]): per thread, the leaf end of its stack, with threads
//! that share a stack folded into one entry. The whole report stays in the
//! log dir, the newest [`FULL_REPORTS_KEPT`] of them.
//!
//! ⚠️ The report is written into the agent's own log dir, never a temp dir.
//! The daemon runs as root, and `sample -file <path>` writes as root: a
//! predictable name under a world-writable `/tmp` would let any local account
//! plant a symlink there and have root overwrite the file it points at.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many frames of each stack the condensed form keeps, counted from the
/// leaf: where the thread waits, and the calls that led it there.
pub const FRAMES_PER_STACK: usize = 16;

/// The condensed form's size cap. A crash sidecar holds 64 KiB
/// (`crash_recorder::MAX_PAYLOAD_BYTES`), and the log lines from before the
/// stall want some of it.
pub const CONDENSED_MAX_BYTES: usize = 24 * 1024;

/// How many whole reports the log dir keeps. Each is ~200 KB on a Mac, and a
/// host that stalls on every start must not fill its disk with them.
pub const FULL_REPORTS_KEPT: usize = 3;

/// The prefix and suffix of a whole report's file name in the log dir.
const REPORT_PREFIX: &str = "stall-";
const REPORT_SUFFIX: &str = ".threads.txt";

/// What [`capture`] read.
pub struct ThreadDump {
    /// For the crash sidecar: [`condense_sample`] or [`condense_procfs`].
    pub condensed: String,
    /// The whole report, when it could be written.
    pub full_report: Option<PathBuf>,
}

/// Read every thread's stack, and keep the whole report in `dir`: the
/// agent's log dir (see the module's ⚠️). Gives up after `deadline`. `None`
/// where the OS offers no source, or the source failed.
pub fn capture(dir: &Path, deadline: Duration) -> Option<ThreadDump> {
    let path = dir.join(report_name(unix_now(), std::process::id()));
    let dump = capture_os(&path, deadline);
    prune_reports(dir, FULL_REPORTS_KEPT);
    dump
}

#[cfg(target_os = "macos")]
fn capture_os(path: &Path, deadline: Duration) -> Option<ThreadDump> {
    use std::process::{Command, Stdio};
    let child = Command::new("/usr/bin/sample")
        .arg(std::process::id().to_string())
        .arg("1")
        // Reads the symbols at once, in case the process ends first.
        .arg("-mayDie")
        .arg("-file")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return None;
    };
    if !wait_within(&mut child, deadline) {
        return None;
    }
    let report = std::fs::read_to_string(path).ok()?;
    Some(ThreadDump {
        condensed: condense_sample(&report, FRAMES_PER_STACK),
        full_report: Some(path.to_path_buf()),
    })
}

/// Wait for `child` until `deadline`, and kill it past that. Whether it
/// exited 0 in time.
#[cfg(target_os = "macos")]
fn wait_within(child: &mut std::process::Child, deadline: Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_os(path: &Path, _deadline: Duration) -> Option<ThreadDump> {
    // procfs reads do not wait on the process they describe.
    let threads = read_proc_threads(Path::new("/proc/self/task"));
    if threads.is_empty() {
        return None;
    }
    let written = std::fs::write(path, full_procfs(&threads)).is_ok();
    Some(ThreadDump {
        condensed: condense_procfs(&threads, FRAMES_PER_STACK),
        full_report: written.then(|| path.to_path_buf()),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn capture_os(_path: &Path, _deadline: Duration) -> Option<ThreadDump> {
    None
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `stall-<unix seconds>-<pid>.threads.txt`. Ten digits of seconds until the
/// year 2286, so the names sort in time order.
fn report_name(unix: u64, pid: u32) -> String {
    format!("{REPORT_PREFIX}{unix:010}-{pid}{REPORT_SUFFIX}")
}

/// Keep the newest `keep` whole reports in `dir`. Best effort.
fn prune_reports(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reports: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(REPORT_PREFIX) && n.ends_with(REPORT_SUFFIX))
        })
        .collect();
    reports.sort();
    let surplus = reports.len().saturating_sub(keep);
    for old in &reports[..surplus] {
        let _ = std::fs::remove_file(old);
    }
}

// ---------------------------------------------------------------------------
// macOS: `sample`'s report
// ---------------------------------------------------------------------------

/// One thread of a `sample` report: its header, and the heaviest path through
/// its call tree, outermost frame first.
struct SampledThread {
    id: String,
    name: String,
    samples: u64,
    path: Vec<String>,
    leaf_samples: u64,
}

/// The condensed form of a `sample` report.
///
/// The call graph lists each thread as a tree whose children are sorted
/// heaviest first, each one level (two columns) deeper than its parent. So the
/// heaviest path is the run of lines right after the header that keep going
/// deeper. A thread blocked for the whole second has exactly one path. The
/// sections after the call graph are ignored.
pub fn condense_sample(report: &str, frames: usize) -> String {
    let threads = parse_sample(report);
    let groups = group(threads.iter().map(|t| Entry {
        stack: last_frames(&t.path, frames),
        name: t.name.clone(),
        id: t.id.clone(),
        note: held(t),
    }));
    render("sample, 1 s", threads.len(), &groups)
}

fn parse_sample(report: &str) -> Vec<SampledThread> {
    let mut threads: Vec<SampledThread> = Vec::new();
    // The column of the last line on the current thread's heaviest path, or
    // `None` once that path has ended.
    let mut deepest: Option<usize> = None;
    for line in report.lines() {
        if line.starts_with("Total number in stack")
            || line.starts_with("Sort by top of stack")
            || line.starts_with("Binary Images:")
        {
            break;
        }
        let Some((col, count, text)) = parse_node(line) else {
            continue;
        };
        if let Some(rest) = text.strip_prefix("Thread_") {
            let (id, name) = split_thread_label(rest);
            threads.push(SampledThread {
                id,
                name,
                samples: count,
                path: Vec::new(),
                leaf_samples: count,
            });
            deepest = Some(col);
            continue;
        }
        let (Some(t), Some(d)) = (threads.last_mut(), deepest) else {
            continue;
        };
        if col > d {
            t.path.push(clean_frame(text));
            t.leaf_samples = count;
            deepest = Some(col);
        } else {
            deepest = None;
        }
    }
    threads
}

/// A call-graph line: its indentation column, its sample count and its text.
/// The indentation is spaces and the tree glyphs `+ ! : |`.
fn parse_node(line: &str) -> Option<(usize, u64, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'+' | b'!' | b':' | b'|') {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start || bytes.get(i) != Some(&b' ') {
        return None;
    }
    let count = line[start..i].parse().ok()?;
    Some((start, count, line[i..].trim()))
}

/// `75251905: tokio-rt-worker` or `75251904   DispatchQueue_1: com.apple.main-thread  (serial)`
/// into its id and its name, whitespace collapsed.
fn split_thread_label(rest: &str) -> (String, String) {
    let id_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (id, tail) = rest.split_at(id_len);
    let name = tail
        .trim_start_matches(':')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let name = if name.is_empty() {
        "(unnamed)".to_string()
    } else {
        name
    };
    (format!("Thread_{id}"), name)
}

/// `tokio::runtime::task::raw::poll::h9755c82ddf6ef493  (in roomlerd) + 440  [0x1067bbb28]`
/// into `tokio::runtime::task::raw::poll`: no image, offset, address or hash,
/// and the legacy mangling's `$…$` escapes read back.
fn clean_frame(raw: &str) -> String {
    let end = raw
        .find("  (in ")
        .or_else(|| raw.find("  [0x"))
        .unwrap_or(raw.len());
    let s = strip_legacy_hash(raw[..end].trim());
    unescape_legacy(s)
}

/// Drop a trailing `::h` + 16 hex digits. Compared as bytes, and those 19
/// are ASCII, so the cut is on a char boundary.
fn strip_legacy_hash(s: &str) -> &str {
    let b = s.as_bytes();
    let n = b.len();
    if n > 19 && &b[n - 19..n - 16] == b"::h" && b[n - 16..].iter().all(u8::is_ascii_hexdigit) {
        &s[..n - 19]
    } else {
        s
    }
}

fn unescape_legacy(s: &str) -> String {
    const ESCAPES: [(&str, &str); 17] = [
        ("$LT$", "<"),
        ("$GT$", ">"),
        ("$LP$", "("),
        ("$RP$", ")"),
        ("$C$", ","),
        ("$RF$", "&"),
        ("$BP$", "*"),
        ("$SP$", "@"),
        ("$u20$", " "),
        ("$u22$", "\""),
        ("$u27$", "'"),
        ("$u2b$", "+"),
        ("$u3b$", ";"),
        ("$u5b$", "["),
        ("$u5d$", "]"),
        ("$u7b$", "{"),
        ("$u7d$", "}"),
    ];
    if !s.contains('$') {
        return s.to_string();
    }
    let mut out = s.to_string();
    for (from, to) in ESCAPES {
        out = out.replace(from, to);
    }
    out
}

/// A note when the heaviest path held fewer samples than the thread: the
/// thread moved during the second, so it was not blocked there.
fn held(t: &SampledThread) -> Option<String> {
    (t.leaf_samples < t.samples)
        .then(|| format!("{} of {} samples here", t.leaf_samples, t.samples))
}

// ---------------------------------------------------------------------------
// Linux: procfs
// ---------------------------------------------------------------------------

/// One thread as `/proc/self/task/<tid>` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcThread {
    pub tid: u32,
    pub name: String,
    /// `R` running, `S` sleeping, `D` uninterruptible (usually I/O), …
    pub state: char,
    /// The kernel function it waits in, if the kernel says.
    pub wchan: Option<String>,
    /// Its kernel stack, innermost frame first, where readable.
    pub kstack: Vec<String>,
}

#[cfg(target_os = "linux")]
fn read_proc_threads(task_dir: &Path) -> Vec<ProcThread> {
    let Ok(entries) = std::fs::read_dir(task_dir) else {
        return Vec::new();
    };
    let mut threads: Vec<ProcThread> = entries
        .flatten()
        .filter_map(|e| {
            let tid: u32 = e.file_name().to_str()?.parse().ok()?;
            let dir = e.path();
            let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
            Some(ProcThread {
                tid,
                name: read("comm")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                state: read("stat").as_deref().and_then(stat_state).unwrap_or('?'),
                wchan: read("wchan").and_then(|w| {
                    let w = w.trim();
                    (!w.is_empty() && w != "0").then(|| w.to_string())
                }),
                kstack: read("stack").map(|s| parse_kstack(&s)).unwrap_or_default(),
            })
        })
        .collect();
    threads.sort_by_key(|t| t.tid);
    threads
}

/// The state field of `/proc/<pid>/task/<tid>/stat`: the first field after the
/// name, which sits in parentheses and may itself contain `)` or spaces.
pub fn stat_state(stat: &str) -> Option<char> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.trim_start().chars().next()
}

/// `[<0>] futex_wait_queue+0x6c/0xa0` lines into function names, innermost
/// first.
pub fn parse_kstack(stack: &str) -> Vec<String> {
    stack
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let l = l.find("] ").map_or(l, |i| &l[i + 2..]);
            let name = l.split('+').next()?.trim();
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

/// The condensed form of the procfs view. The kernel stack reads outermost
/// first, as the macOS form does, and ends with the thread's state and wait
/// channel. So threads fold only when they wait alike, even where the kernel
/// stack is unreadable (not root).
pub fn condense_procfs(threads: &[ProcThread], frames: usize) -> String {
    let groups = group(threads.iter().map(|t| {
        let mut stack: Vec<String> = t.kstack.iter().rev().cloned().collect();
        stack.push(match &t.wchan {
            Some(w) => format!("state {}, waiting in {w}", t.state),
            None => format!("state {}", t.state),
        });
        Entry {
            stack: last_frames(&stack, frames),
            name: t.name.clone(),
            id: t.tid.to_string(),
            note: None,
        }
    }));
    render(
        "procfs: name, state, kernel wait, kernel stack",
        threads.len(),
        &groups,
    )
}

/// Every thread, unabridged, for the log dir.
#[cfg(target_os = "linux")]
fn full_procfs(threads: &[ProcThread]) -> String {
    let mut out = String::new();
    for t in threads {
        out.push_str(&format!(
            "{} {} {} wchan={}\n",
            t.tid,
            t.name,
            t.state,
            t.wchan.as_deref().unwrap_or("-")
        ));
        for f in &t.kstack {
            out.push_str("    ");
            out.push_str(f);
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shared: grouping and rendering
// ---------------------------------------------------------------------------

/// One thread, ready to group.
struct Entry {
    /// Outermost frame first, leaf last.
    stack: Vec<String>,
    name: String,
    id: String,
    note: Option<String>,
}

/// Threads that share a stack, in the order the first of them appeared.
struct Group {
    stack: Vec<String>,
    names: Vec<String>,
    ids: Vec<String>,
    note: Option<String>,
}

fn group(threads: impl Iterator<Item = Entry>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for e in threads {
        match groups
            .iter_mut()
            .find(|g| g.stack == e.stack && g.note == e.note)
        {
            Some(g) => {
                g.names.push(e.name);
                g.ids.push(e.id);
            }
            None => groups.push(Group {
                stack: e.stack,
                names: vec![e.name],
                ids: vec![e.id],
                note: e.note,
            }),
        }
    }
    groups
}

fn last_frames(path: &[String], frames: usize) -> Vec<String> {
    path[path.len().saturating_sub(frames)..].to_vec()
}

/// `--- thread stacks (#3018): N threads, M stacks (source) ---`, then one
/// entry per group, capped at [`CONDENSED_MAX_BYTES`].
fn render(source: &str, threads: usize, groups: &[Group]) -> String {
    let mut out = format!(
        "--- thread stacks (#3018): {threads} threads, {} stacks ({source}) ---\n",
        groups.len()
    );
    for (i, g) in groups.iter().enumerate() {
        let entry = render_group(g);
        if out.len() + entry.len() > CONDENSED_MAX_BYTES {
            out.push_str(&format!(
                "[… {} more stacks: see the whole report]\n",
                groups.len() - i
            ));
            break;
        }
        out.push_str(&entry);
    }
    out
}

fn render_group(g: &Group) -> String {
    // Each distinct name once, with its count: `tokio-rt-worker ×4`.
    let mut names: Vec<(String, usize)> = Vec::new();
    for n in &g.names {
        match names.iter_mut().find(|(m, _)| m == n) {
            Some((_, c)) => *c += 1,
            None => names.push((n.clone(), 1)),
        }
    }
    let names = names
        .iter()
        .map(|(n, c)| {
            if *c > 1 {
                format!("{n} ×{c}")
            } else {
                n.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    const IDS_SHOWN: usize = 6;
    let mut ids = g
        .ids
        .iter()
        .take(IDS_SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if g.ids.len() > IDS_SHOWN {
        ids.push_str(&format!(", +{}", g.ids.len() - IDS_SHOWN));
    }
    let mut out = format!("[{}] {names} ({ids})", g.ids.len());
    if let Some(note) = &g.note {
        out.push_str(&format!(" — {note}"));
    }
    out.push('\n');
    if g.stack.is_empty() {
        out.push_str("  (no stack)\n");
    }
    for f in &g.stack {
        out.push_str("  ");
        out.push_str(f);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a real `sample` report (macOS 26, 0.4.181), cut down:
    /// two idle runtime workers with the same stack, one worker blocked on a
    /// mutex under an overlay frame, the main thread, and the sections after
    /// the call graph that must be ignored.
    const REPORT: &str = "\
Analysis of sampling roomlerd (pid 14506) every 1 millisecond
Process:         roomlerd [14506]
Path:            /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd

Call graph:
    756 Thread_75251904   DispatchQueue_1: com.apple.main-thread  (serial)
    + 756 start  (in dyld) + 6992  [0x181312b98]
    +   756 roomlerd::app::main::hfcd8123c607f679d  (in roomlerd) + 4228  [0x105622d78]
    +     756 roomlerd::indicator::mac::run_main_loop::h5c8a7afc9db24df5  (in roomlerd) + 3688  [0x10592965c]
    +       756 mach_msg2_trap  (in libsystem_kernel.dylib) + 8  [0x18166cc34]
    756 Thread_75251905: tokio-rt-worker
    + 756 thread_start  (in libsystem_pthread.dylib) + 8  [0x1816accec]
    +   756 core::ops::function::FnOnce::call_once$u7b$$u7b$vtable.shim$u7d$$u7d$::hf720dd4453875532  (in roomlerd) + 76  [0x1067a78ec]
    +     756 tokio::runtime::scheduler::multi_thread::worker::run::h82bb09cb102d2e92  (in roomlerd) + 4036  [0x1067c7fe8]
    +       756 parking_lot::condvar::Condvar::wait_until_internal::hcfa5dd8e5b18a61b  (in roomlerd) + 520  [0x1067d7b34]
    +         756 __psynch_cvwait  (in libsystem_kernel.dylib) + 8  [0x18167050c]
    756 Thread_75251906: tokio-rt-worker
    + 756 thread_start  (in libsystem_pthread.dylib) + 8  [0x1816accec]
    +   756 core::ops::function::FnOnce::call_once$u7b$$u7b$vtable.shim$u7d$$u7d$::hf720dd4453875532  (in roomlerd) + 76  [0x1067a78ec]
    +     756 tokio::runtime::scheduler::multi_thread::worker::run::h82bb09cb102d2e92  (in roomlerd) + 4036  [0x1067c7fe8]
    +       756 parking_lot::condvar::Condvar::wait_until_internal::hcfa5dd8e5b18a61b  (in roomlerd) + 520  [0x1067d7b34]
    +         756 __psynch_cvwait  (in libsystem_kernel.dylib) + 8  [0x18167050c]
    756 Thread_75251907: tokio-rt-worker
    + 756 thread_start  (in libsystem_pthread.dylib) + 8  [0x1816accec]
    +   756 tokio::runtime::task::raw::poll::h9755c82ddf6ef493  (in roomlerd) + 440  [0x1067bbb28]
    +     756 roomler_ai_tunnel_core::overlay::carrier::Plane::promote::h0123456789abcdef  (in roomlerd) + 96  [0x105a00000]
    +     ! 700 std::sys::sync::mutex::pthread::Mutex::lock::h1111111111111111  (in roomlerd) + 32  [0x105a00100]
    +     ! : 700 __psynch_mutexwait  (in libsystem_kernel.dylib) + 8  [0x1816700aa]
    +     ! 56 __psynch_cvwait  (in libsystem_kernel.dylib) + 8  [0x18167050c]
    10 Thread_75251908
    + 10 thread_start  (in libsystem_pthread.dylib) + 8  [0x1816accec]

Total number in stack (recursive counted multiple, when >=5):
        5       Thread_99999999: not a thread of the call graph
Sort by top of stack, same collapsed (when >= 5):
        __psynch_cvwait  (in libsystem_kernel.dylib)        1512
Binary Images:
       0x104cec000 -        0x1070a841b +com.roomler.agent (0.4.181 - 0.4.181)
";

    #[test]
    fn a_sample_report_folds_identical_stacks_and_keeps_the_blocked_one() {
        let c = condense_sample(REPORT, FRAMES_PER_STACK);
        assert!(
            c.starts_with("--- thread stacks (#3018): 5 threads, 4 stacks (sample, 1 s) ---\n"),
            "{c}"
        );
        // The two idle workers fold into one entry.
        assert!(
            c.contains("[2] tokio-rt-worker ×2 (Thread_75251905, Thread_75251906)\n"),
            "{c}"
        );
        // The blocked worker keeps its own entry, leaf last, named by its frames.
        let blocked = "[1] tokio-rt-worker (Thread_75251907) — 700 of 756 samples here\n  thread_start\n  tokio::runtime::task::raw::poll\n  roomler_ai_tunnel_core::overlay::carrier::Plane::promote\n  std::sys::sync::mutex::pthread::Mutex::lock\n  __psynch_mutexwait\n";
        assert!(c.contains(blocked), "{c}");
        // The main thread's label keeps its queue.
        assert!(
            c.contains("[1] DispatchQueue_1: com.apple.main-thread (serial) (Thread_75251904)\n"),
            "{c}"
        );
        // A thread with no name still appears.
        assert!(
            c.contains("[1] (unnamed) (Thread_75251908)\n  thread_start\n"),
            "{c}"
        );
        // Hashes, images and addresses are gone; escapes are read back.
        assert!(!c.contains("::h"), "{c}");
        assert!(!c.contains("(in "), "{c}");
        assert!(!c.contains("[0x"), "{c}");
        assert!(
            c.contains("core::ops::function::FnOnce::call_once{{vtable.shim}}\n"),
            "{c}"
        );
        // The sections after the call graph are not threads.
        assert!(!c.contains("Thread_99999999"), "{c}");
    }

    #[test]
    fn only_the_leaf_end_of_a_deep_stack_is_kept() {
        let c = condense_sample(REPORT, 2);
        assert!(
            c.contains("  std::sys::sync::mutex::pthread::Mutex::lock\n  __psynch_mutexwait\n"),
            "{c}"
        );
        assert!(
            !c.contains("  roomler_ai_tunnel_core::overlay::carrier::Plane::promote\n"),
            "{c}"
        );
    }

    #[test]
    fn a_report_with_no_call_graph_condenses_to_its_header() {
        assert_eq!(
            condense_sample("", FRAMES_PER_STACK),
            "--- thread stacks (#3018): 0 threads, 0 stacks (sample, 1 s) ---\n"
        );
    }

    #[test]
    fn the_condensed_form_stays_under_its_cap() {
        let mut big = String::from("Call graph:\n");
        for t in 0..400 {
            big.push_str(&format!("    756 Thread_{t}: worker-{t}\n"));
            for d in 0..30 {
                let indent = " ".repeat(2 * d);
                big.push_str(&format!(
                    "    {indent} 756 a_long_frame_name_number_{d}_of_thread_{t}::with::a::path\n"
                ));
            }
        }
        let c = condense_sample(&big, FRAMES_PER_STACK);
        assert!(c.len() <= CONDENSED_MAX_BYTES + 100, "{} bytes", c.len());
        assert!(
            c.contains("more stacks: see the whole report]"),
            "the cut is said"
        );
    }

    #[test]
    fn a_frame_loses_its_image_offset_address_and_hash() {
        assert_eq!(
            clean_frame(
                "tokio::runtime::task::raw::poll::h9755c82ddf6ef493  (in roomlerd) + 440  [0x1067bbb28]"
            ),
            "tokio::runtime::task::raw::poll"
        );
        assert_eq!(
            clean_frame("???  (in roomlerd)  load address 0x1 + 0x2  [0x3]"),
            "???"
        );
        // Not a hash: too short, or not hex.
        assert_eq!(strip_legacy_hash("a::hdeadbeef"), "a::hdeadbeef");
        assert_eq!(
            strip_legacy_hash("module::hzzzzzzzzzzzzzzzz"),
            "module::hzzzzzzzzzzzzzzzz"
        );
        assert_eq!(
            unescape_legacy("_$LT$T$u20$as$u20$X$GT$::f"),
            "_<T as X>::f"
        );
    }

    #[test]
    fn report_names_sort_in_time_order_and_only_the_newest_are_kept() {
        assert_eq!(
            report_name(1_793_203_600, 42),
            "stall-1793203600-42.threads.txt"
        );
        assert!(report_name(999, 1) < report_name(1_000_000_000, 1));
        let dir =
            std::env::temp_dir().join(format!("roomlerd-thread-dump-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for unix in [100, 300, 200, 500, 400] {
            std::fs::write(dir.join(report_name(unix, 1)), b"x").unwrap();
        }
        std::fs::write(dir.join("roomlerd.log.2026-10-28"), b"not a report").unwrap();
        prune_reports(&dir, 3);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "roomlerd.log.2026-10-28",
                "stall-0000000300-1.threads.txt",
                "stall-0000000400-1.threads.txt",
                "stall-0000000500-1.threads.txt",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stat_state_survives_a_name_with_parentheses() {
        assert_eq!(stat_state("1234 (tokio-runtime-w) S 1 2 3"), Some('S'));
        assert_eq!(stat_state("1234 (a) (b) D 1 2 3"), Some('D'));
        assert_eq!(stat_state("no parentheses"), None);
    }

    #[test]
    fn a_kernel_stack_reads_as_function_names() {
        assert_eq!(
            parse_kstack("[<0>] futex_wait_queue+0x6c/0xa0\n[<0>] __x64_sys_futex+0x10/0x20\n"),
            ["futex_wait_queue", "__x64_sys_futex"]
        );
        assert!(parse_kstack("").is_empty());
    }

    #[test]
    fn procfs_threads_fold_by_name_state_and_wait() {
        let t = |tid, name: &str, state, wchan: Option<&str>, stack: &[&str]| ProcThread {
            tid,
            name: name.into(),
            state,
            wchan: wchan.map(Into::into),
            kstack: stack.iter().map(|s| s.to_string()).collect(),
        };
        let threads = [
            t(10, "roomlerd", 'S', Some("do_epoll_wait"), &[]),
            t(
                11,
                "tokio-runtime-w",
                'S',
                Some("futex_wait_queue"),
                &["futex_wait_queue", "futex_wait"],
            ),
            t(
                12,
                "tokio-runtime-w",
                'S',
                Some("futex_wait_queue"),
                &["futex_wait_queue", "futex_wait"],
            ),
            t(13, "tokio-runtime-w", 'D', Some("sock_sendmsg"), &[]),
        ];
        let c = condense_procfs(&threads, FRAMES_PER_STACK);
        assert!(
            c.starts_with("--- thread stacks (#3018): 4 threads, 3 stacks (procfs"),
            "{c}"
        );
        assert!(
            c.contains(
                "[2] tokio-runtime-w ×2 (11, 12)\n  futex_wait\n  futex_wait_queue\n  state S, waiting in futex_wait_queue\n"
            ),
            "{c}"
        );
        // No readable kernel stack: they still differ by what they wait on.
        assert!(
            c.contains("[1] roomlerd (10)\n  state S, waiting in do_epoll_wait\n"),
            "{c}"
        );
        assert!(
            c.contains("[1] tokio-runtime-w (13)\n  state D, waiting in sock_sendmsg\n"),
            "{c}"
        );
    }

    /// The real source on this OS, read from this test process: a thread
    /// parked on purpose must show up by name. On macOS this is `sample`
    /// against an unhardened test binary; on Linux, procfs.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn this_process_dumps_its_own_threads() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let parked = std::thread::Builder::new()
            // Linux keeps 15 bytes of a thread's name.
            .name("tdump-parked".into())
            .spawn(move || {
                let _ = ready_tx.send(());
                let _ = rx.recv();
            })
            .unwrap();
        // The new thread sets its own name before it runs this closure, and
        // until then the OS shows its creator's. CI's first run read procfs in
        // that window, under a full parallel test run.
        ready_rx.recv().expect("the parked thread started");
        let dir =
            std::env::temp_dir().join(format!("roomlerd-thread-dump-self-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let dump = capture(&dir, Duration::from_secs(20)).expect("this OS has a source");

        let _ = tx.send(());
        let _ = parked.join();
        assert!(
            dump.condensed.contains("tdump-parked"),
            "{}",
            dump.condensed
        );
        let full = dump.full_report.expect("the whole report was kept");
        assert!(full.starts_with(&dir), "{}", full.display());
        assert!(
            std::fs::read_to_string(&full)
                .unwrap()
                .contains("tdump-parked")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
