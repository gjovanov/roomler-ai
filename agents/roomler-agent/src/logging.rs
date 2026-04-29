//! Persistent file logging + panic hook.
//!
//! Stdout output is preserved for foreground / interactive runs; a daily-
//! rolling file appender writes everything to
//! `<data-local-dir>/logs/roomler-agent.log[.YYYY-MM-DD]` so a Scheduled-
//! Task / systemd / launchd-supervised agent (where stdout is `/dev/null`)
//! still leaves a forensic trail.
//!
//! On Windows that resolves to `%LOCALAPPDATA%\roomler\roomler-agent\data\logs\`
//! via `directories::ProjectDirs::data_local_dir`. Linux: `~/.local/share/
//! roomler-agent/logs/`. macOS: `~/Library/Application Support/live.roomler.
//! roomler-agent/logs/`.
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
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Holds the non-blocking appender's worker thread alive for the
/// process lifetime. Dropping it stops the writer thread, which would
/// silently drop in-flight log lines.
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Resolved log directory, exposed for diagnostics (the `panic` /
/// `service status` paths surface it to the operator).
static LOG_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

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

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("roomler_agent=info,warn"));

    let stdout = fmt::layer().with_target(false).compact();

    let dir = resolve_log_dir();
    let _ = LOG_DIR.set(dir.clone());

    if let Some(d) = dir.as_deref()
        && std::fs::create_dir_all(d).is_ok()
    {
        prune_old_logs(d, KEEP_DAYS);
        let appender = tracing_appender::rolling::daily(d, "roomler-agent.log");
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
            .try_init();
        install_panic_hook(d.to_path_buf());
    } else {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(stdout)
            .try_init();
        // No log dir → no panic hook. Stdout-only is the right
        // fallback for cargo-test and ad-hoc `cargo run` from a
        // checkout where `directories` couldn't resolve a home.
    }
}

/// Path of the log directory, if persistent file logging is active.
/// Returns `None` when the platform doesn't expose a data dir or
/// `init()` hasn't run yet.
pub fn log_dir() -> Option<PathBuf> {
    LOG_DIR.get().cloned().flatten()
}

fn resolve_log_dir() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("live", "roomler", "roomler-agent")?;
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
/// two prefixes (`roomler-agent.log` rolling files, `panic-` panic
/// dumps) and with mtime older than `cutoff` are unlinked. Anything
/// else in the directory is left alone — the operator may have stashed
/// notes there.
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
        let is_ours = lossy.starts_with("roomler-agent.log") || lossy.starts_with("panic-");
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
        let log = tmp.path().join("roomler-agent.log.2026-04-29");
        let log_root = tmp.path().join("roomler-agent.log");
        let panic = tmp.path().join("panic-1234-100.log");
        let unrelated = tmp.path().join("readme.txt");
        for p in [&log, &log_root, &panic, &unrelated] {
            std::fs::write(p, b"x").unwrap();
        }
        // Cutoff 1 day in the future — every file's mtime is older.
        let future = SystemTime::now() + Duration::from_secs(86_400);
        prune_old_logs_at(tmp.path(), future);
        assert!(!log.exists(), "rolling log should be pruned");
        assert!(!log_root.exists(), "current rolling log should be pruned");
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
