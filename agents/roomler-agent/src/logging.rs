//! Persistent file logging + panic hook.
//!
//! Stdout output is preserved for foreground / interactive runs; a daily-
//! rolling file appender writes everything to
//! `<data-local-dir>/logs/roomlerd.log[.YYYY-MM-DD]` so a Scheduled-
//! Task / systemd / launchd-supervised agent (where stdout is `/dev/null`)
//! still leaves a forensic trail.
//!
//! P3d Slice B renamed the daemon (`roomler-agent` -> `roomlerd`); the log
//! basename moved to match. Old rolled `roomler-agent.log*` files on an
//! upgraded host stay readable — [`active_log_path`] and [`prune_old_logs_at`]
//! (and `logs_fetch`) accept BOTH prefixes.
//!
//! The log DIR is resolved by `appdirs` (new-then-old segment). On a fresh
//! Windows install that's `%LOCALAPPDATA%\roomler\roomler\data\logs\`; a
//! pre-rename host keeps `...\roomler\roomler-agent\data\logs\`. Linux:
//! `~/.local/share/roomler/logs/` (or `.../roomler-agent/logs/` on an upgraded
//! host). macOS: `~/Library/Application Support/live.roomler.roomler/logs/`.
//!
//! A process-wide panic hook captures the message + backtrace and writes
//! it synchronously to `<log_dir>/panic-<pid>-<unix_ts>.log` *before*
//! delegating to the previous hook. The sync write is the belt-and-braces
//! against the non-blocking appender's worker thread not draining the
//! queue before the OS reaps a panicking process.
//!
//! Init is idempotent (a second call is a no-op) so test harness setup
//! that calls `init()` repeatedly doesn't panic on subscriber re-install.

use std::backtrace::Backtrace;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::logs_upload::{LogLine, LogUploadLayer};

/// Holds the non-blocking appender's worker thread alive for the
/// process lifetime. Dropping it stops the writer thread, which would
/// silently drop in-flight log lines.
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Resolved log directory, exposed for diagnostics (the `panic` /
/// `service status` paths surface it to the operator).
static LOG_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Holds the consumer end of the log-upload channel after [`init`]
/// runs. [`take_log_upload_receiver`] consumes it (one-shot) so
/// `main.rs` can spawn the uploader task once config is loaded. If
/// the receiver is never claimed, the layer continues to capture
/// events into the channel until it fills up + drops oldest (cap
/// 10 000 lines).
static LOG_UPLOAD_RX: OnceLock<Mutex<Option<mpsc::Receiver<LogLine>>>> = OnceLock::new();

/// Days to retain rolling log + panic files. Anything older than this
/// is pruned on startup (one-shot, not a background task).
const KEEP_DAYS: u64 = 14;

/// Initialise tracing subscribers. Always installs a stdout layer; adds
/// a daily-rolling file layer + panic hook when the platform log dir
/// is writeable. Infallible — file logging failure falls back to
/// stdout-only without erroring out the agent (the agent's signaling
/// loop is the load-bearing path; logging is observability, not
/// correctness).
pub fn init() {
    if GUARD.get().is_some() {
        return;
    }

    // Default filter. `RUST_LOG` (env, or the SCM service Environment
    // block for the SystemContext service) overrides wholesale when
    // set. The fallback keeps `roomler_agent` at info + everything
    // else at warn, with ONE addition: `tunnel_core=info`.
    //
    // rc.74: `tunnel_core=info` surfaces the per-flow throughput
    // logger (`tunnel flow throughput (2s window) …` + the
    // flow-closed totals) added in rc.66. Those live in the
    // `tunnel_core` target, so under the old `roomler_agent=info,warn`
    // default they were filtered out of BOTH the on-disk rolling log
    // AND the centralized upload layer — invisible unless an operator
    // hand-set `RUST_LOG`. The throughput lines are low-volume (one
    // per active flow per 2 s, only when bytes moved) and are exactly
    // the signal we need to diagnose tunnel stalls remotely via the
    // admin-UI log viewer, so they belong on by default. Everything
    // else in `tunnel_core` is debug/trace and stays suppressed; the
    // chatty `webrtc_sctp` / `webrtc_ice` targets stay at warn.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("roomler_agent=info,tunnel_core=info,warn"))
        // rc.181 — silence the `turn` crate's periodic refresh-failure warns
        // (`refresh allocation/permissions failed`, target `turn::client::relay_conn`).
        // Now that the overlay re-allocates a dead relay carrier on the send-error
        // signal (wg.rs `Carrier::dead` → `sweep_carrier_health`), these are
        // transient noise — one every ~5–10 min per corp-VPN host — and they
        // dominate the error/warn-biased agent_logs upload. Appended
        // UNCONDITIONALLY (even when the SCM service sets `RUST_LOG` wholesale) so
        // it lands fleet-wide on the next binary, not just fresh installs. The
        // sweep's own `relay carrier … — re-allocating` warn is the actionable
        // signal that replaces them.
        .add_directive(
            "turn::client::relay_conn=error"
                .parse()
                .expect("static EnvFilter directive is valid"),
        );

    let stdout = fmt::layer().with_target(false).compact();

    // rc.58 — log-upload layer. Captures every tracing event into an
    // mpsc channel; `main.rs` claims the receiver via
    // [`take_log_upload_receiver`] and spawns the uploader task once
    // config is loaded. If the receiver is never claimed (e.g. config
    // load fails), the channel fills + drops oldest with no impact on
    // the on-disk rolling log.
    let (upload_layer, upload_rx) = LogUploadLayer::new();
    let _ = LOG_UPLOAD_RX.set(Mutex::new(Some(upload_rx)));

    let dir = resolve_log_dir();
    let _ = LOG_DIR.set(dir.clone());

    if let Some(d) = dir.as_deref()
        && std::fs::create_dir_all(d).is_ok()
    {
        prune_old_logs(d, KEEP_DAYS);
        // P3d Slice B: write under the new `roomlerd.log` basename.
        let appender = tracing_appender::rolling::daily(d, "roomlerd.log");
        let (nb, guard) = tracing_appender::non_blocking(appender);
        let _ = GUARD.set(guard);
        let file = fmt::layer()
            .with_writer(nb)
            .with_target(false)
            .with_ansi(false);
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(stdout)
            .with(file)
            .with(upload_layer)
            .try_init();
        install_panic_hook(d.to_path_buf());
    } else {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(stdout)
            .with(upload_layer)
            .try_init();
        // No log dir → no panic hook. Stdout-only is the right
        // fallback for cargo-test and ad-hoc `cargo run` from a
        // checkout where `directories` couldn't resolve a home.
    }
}

/// Claim the consumer end of the log-upload channel. Returns `None`
/// if [`init`] hasn't run yet OR the receiver has already been taken
/// — one-shot semantics so we don't have two parallel uploaders
/// fighting for the same channel.
pub fn take_log_upload_receiver() -> Option<mpsc::Receiver<LogLine>> {
    let lock = LOG_UPLOAD_RX.get()?;
    lock.lock().ok()?.take()
}

/// Path of the log directory, if persistent file logging is active.
/// Returns `None` when the platform doesn't expose a data dir or
/// `init()` hasn't run yet.
pub fn log_dir() -> Option<PathBuf> {
    LOG_DIR.get().cloned().flatten()
}

/// Path of TODAY's rolling log file, if file logging is active.
/// Used by `crash_recorder::record` to attach a `log_tail` to crash
/// sidecars. Returns `None` when `init()` hasn't run yet OR no log
/// dir resolved (test harness / no-home environment).
///
/// The path is computed deterministically from the rolling
/// appender's daily-rotation convention: `<log_dir>/roomlerd.log.YYYY-MM-DD`
/// for archived days; `<log_dir>/roomlerd.log` is the symlink-ish "current"
/// name on some platforms but in practice tracing-appender writes to the dated
/// name from the very first line.
///
/// P3d Slice B: probe the new `roomlerd.log[.date]` basenames first, then fall
/// back to the legacy `roomler-agent.log[.date]` so an upgraded host whose most
/// recent rolled file predates the rename is still found.
pub fn active_log_path() -> Option<PathBuf> {
    let dir = log_dir()?;
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Format unix seconds → YYYY-MM-DD without pulling chrono.
    // tracing-appender uses UTC by default; mirror that.
    let date = format_utc_date(today);
    for base in ["roomlerd.log", "roomler-agent.log"] {
        let dated = dir.join(format!("{base}.{date}"));
        if dated.exists() {
            return Some(dated);
        }
        let plain = dir.join(base);
        if plain.exists() {
            return Some(plain);
        }
    }
    None
}

/// Format a unix-seconds value as `YYYY-MM-DD` in UTC. Pure +
/// no-dep so the agent build doesn't pull chrono just for this.
/// Algorithm = days-since-epoch + civil-from-days (Howard Hinnant).
fn format_utc_date(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // From Howard Hinnant's "chrono-Compatible Low-Level Date
    // Algorithms" — converts days-since-1970-01-01 to (year, month,
    // day). Pure integer arithmetic.
    let z = z + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Compute the default log directory PATH — purely, without the `LOG_DIR`
/// `OnceLock` (which is only set by [`init`], so it's `None` in any process
/// that didn't run the agent's logging setup, e.g. the tray). The tray uses
/// this so "Open Logs Folder" resolves a real path instead of failing.
pub fn resolve_log_dir() -> Option<PathBuf> {
    let dirs = crate::appdirs::project_dirs()?;
    Some(dirs.data_local_dir().join("logs"))
}

/// Delete rolling-log + panic files in `dir` older than `keep_days`.
/// Best-effort; any I/O error is swallowed so a permission glitch
/// doesn't block startup.
fn prune_old_logs(dir: &Path, keep_days: u64) {
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(keep_days * 86_400))
    else {
        return;
    };
    prune_old_logs_at(dir, cutoff);
}

/// Same as [`prune_old_logs`] but takes the cutoff as a parameter so
/// tests can drive it deterministically. Files matching one of our
/// prefixes (`roomlerd.log` / legacy `roomler-agent.log` rolling files,
/// `panic-` panic dumps) and with mtime older than `cutoff` are unlinked.
/// Anything else in the directory is left alone — the operator may have
/// stashed notes there.
///
/// P3d Slice B: prunes BOTH the new `roomlerd.log*` files and the legacy
/// `roomler-agent.log*` files so an upgraded host's old rolled logs age out
/// under the same retention policy instead of accreting forever.
fn prune_old_logs_at(dir: &Path, cutoff: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        let is_ours = lossy.starts_with("roomlerd.log")
            || lossy.starts_with("roomler-agent.log")
            || lossy.starts_with("panic-");
        if is_ours && mtime < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn install_panic_hook(log_dir: PathBuf) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = Backtrace::force_capture();
        let pid = std::process::id();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()));
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let content = format_panic(payload, location.as_deref(), &bt.to_string(), pid, ts);
        let path = log_dir.join(format!("panic-{pid}-{ts}.log"));
        // Sync write — we do not trust the non-blocking appender's
        // worker thread to flush before the process is reaped.
        let _ = std::fs::write(&path, &content);
        // Best-effort tracing emission too — usually flushes via the
        // WorkerGuard's Drop, but the sync file above is the source
        // of truth for post-mortem.
        tracing::error!(panic_log = %path.display(), "agent panicked; details written to disk");

        // Phase 1B: emit a JSON crash sidecar for the uploader to
        // ship to roomler.ai on next agent startup. The sidecar is
        // INDEPENDENT of the text dump above — even if the recorder
        // recursively panics inside its catch_unwind, the text dump
        // has already flushed to disk. Worker context: panic hook
        // always runs in the worker process.
        let summary = format!(
            "{} at {}",
            payload.unwrap_or("panic"),
            location.as_deref().unwrap_or("?"),
        );
        crate::crash_recorder::record(
            crate::crash_recorder::Reason::Panic,
            &summary,
            crate::crash_recorder::WriterContext::Worker,
        );

        prev(info);
    }));
}

/// Pure formatter for the panic-dump file. Extracted so the test
/// suite can lock the on-disk shape without having to manufacture a
/// real `PanicHookInfo` (the type's fields are private).
fn format_panic(
    payload: Option<&str>,
    location: Option<&str>,
    backtrace: &str,
    pid: u32,
    ts: u64,
) -> String {
    let mut buf = format!("--- panic at {ts} (pid {pid}) ---\n");
    if let Some(loc) = location {
        buf.push_str(&format!("location: {loc}\n"));
    }
    buf.push_str(&format!("payload: {}\n", payload.unwrap_or("<unknown>")));
    buf.push_str("backtrace:\n");
    buf.push_str(backtrace);
    if !backtrace.ends_with('\n') {
        buf.push('\n');
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        // Two calls in the same process must not panic. Subsequent
        // calls fall through the `GUARD.get().is_some()` early return.
        init();
        init();
    }

    #[test]
    fn format_panic_includes_all_fields() {
        let s = format_panic(
            Some("kapow"),
            Some("src/foo.rs:42:1"),
            "frame 0\nframe 1",
            1234,
            9_999,
        );
        assert!(s.starts_with("--- panic at 9999 (pid 1234) ---\n"));
        assert!(s.contains("location: src/foo.rs:42:1\n"));
        assert!(s.contains("payload: kapow\n"));
        assert!(s.contains("backtrace:\nframe 0\nframe 1\n"));
    }

    #[test]
    fn format_panic_uses_unknown_when_payload_missing() {
        let s = format_panic(None, None, "bt", 1, 1);
        assert!(s.contains("payload: <unknown>\n"));
        assert!(!s.contains("location: "));
    }

    #[test]
    fn prune_with_future_cutoff_drops_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        // New (roomlerd) + legacy (roomler-agent) rolling files must BOTH be
        // pruned (P3d Slice B both-name retention).
        let logd = tmp.path().join("roomlerd.log.2026-07-15");
        let logd_root = tmp.path().join("roomlerd.log");
        let log = tmp.path().join("roomler-agent.log.2026-04-29");
        let log_root = tmp.path().join("roomler-agent.log");
        let panic = tmp.path().join("panic-1234-100.log");
        let unrelated = tmp.path().join("readme.txt");
        for p in [&logd, &logd_root, &log, &log_root, &panic, &unrelated] {
            std::fs::write(p, b"x").unwrap();
        }
        // Cutoff 1 day in the future — every file's mtime is older.
        let future = SystemTime::now() + Duration::from_secs(86_400);
        prune_old_logs_at(tmp.path(), future);
        assert!(!logd.exists(), "new rolling log should be pruned");
        assert!(
            !logd_root.exists(),
            "new current rolling log should be pruned"
        );
        assert!(!log.exists(), "legacy rolling log should be pruned");
        assert!(
            !log_root.exists(),
            "legacy current rolling log should be pruned"
        );
        assert!(!panic.exists(), "panic dump should be pruned");
        assert!(unrelated.exists(), "unrelated files must be left alone");
    }

    #[test]
    fn prune_with_past_cutoff_keeps_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("roomler-agent.log.2026-04-29");
        std::fs::write(&log, b"x").unwrap();
        // Cutoff 1 day in the past — every fresh file is newer.
        let past = SystemTime::now() - Duration::from_secs(86_400);
        prune_old_logs_at(tmp.path(), past);
        assert!(log.exists());
    }

    #[test]
    fn prune_handles_missing_directory_gracefully() {
        // No panic when the dir doesn't exist.
        let bogus = std::path::PathBuf::from("definitely/not/a/real/path/12345");
        prune_old_logs_at(&bogus, SystemTime::now());
    }
}
