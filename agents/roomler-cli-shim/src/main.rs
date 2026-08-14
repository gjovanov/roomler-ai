//! `roomler.exe` on daemon hosts — a launcher that re-execs `roomlerd cli`.
//!
//! # Why
//!
//! Both MSI flavours used to bundle the full standalone tunnel CLI next to the
//! daemon (P4b). Measured on rc.361 that was 22.1 MiB of which only ~1.2 MiB
//! was CLI-specific code: `cargo bloat` puts `roomler_tunnel` + its bin at
//! 1.16 MiB of a 15.0 MiB `.text`, and everything else (std, the webrtc
//! family, tunnel_core, tokio, rustls, reqwest, quinn, clap) is a second copy
//! of crates `roomlerd.exe` already links. It also cost the Windows MSI job a
//! serial 143 s `cargo build -p roomler-tunnel` with a different feature set.
//!
//! So the daemon now owns that command surface (`roomlerd cli <args>` →
//! `roomler_tunnel::cli::run_from`), and this ~150 KB launcher keeps the
//! user-facing `roomler` command, its PATH entry, and the installer-smoke
//! payload assertions exactly where they were. Tunnel-ONLY hosts are
//! unaffected — they still install the real standalone binary from
//! `release-tunnel.yml`.
//!
//! # Behaviour notes
//!
//! * stdio is INHERITED, so streaming verbs (`roomler exec`, `roomler run`)
//!   behave identically to the standalone binary.
//! * Ctrl-C is deliberately ignored HERE so the child owns it. Windows
//!   delivers CTRL_C_EVENT to every process in the console group; if this
//!   launcher took the default action it would exit first and leave the
//!   long-running child (`roomler run`, `roomler forward`) orphaned.
//! * The child is located next to this executable rather than via PATH, so a
//!   different Roomler install elsewhere on PATH can never be dispatched to.

use std::process::Command;

/// Exit code for "we could not even start the daemon binary". 127 is the
/// shell convention for command-not-found, which is exactly this case.
const EXIT_NO_DAEMON: i32 = 127;

#[cfg(target_os = "windows")]
fn ignore_ctrl_c() {
    // Passing a NULL handler with `add = TRUE` tells Windows to IGNORE
    // Ctrl-C in this process; the child (which installs its own handler)
    // still receives the event.
    unsafe {
        let _ = windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 1);
    }
}

#[cfg(not(target_os = "windows"))]
fn ignore_ctrl_c() {
    // No-op off Windows: this launcher only ships in the Windows MSIs, and
    // POSIX signal disposition is inherited by the child anyway.
}

// Exits via `std::process::exit` rather than returning `ExitCode`, because
// `ExitCode: From<u8>` would TRUNCATE the child's code — a script checking for
// a specific nonzero exit must see exactly what the standalone CLI would have
// returned.
fn main() -> ! {
    let daemon = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(DAEMON_FILE_NAME)))
    {
        Some(p) if p.is_file() => p,
        Some(p) => {
            eprintln!(
                "roomler: the daemon binary is missing at {} — reinstall the Roomler MSI, \
                 or use the standalone tunnel CLI on hosts without a daemon.",
                p.display()
            );
            std::process::exit(EXIT_NO_DAEMON);
        }
        None => {
            eprintln!("roomler: could not resolve this executable's own path");
            std::process::exit(EXIT_NO_DAEMON);
        }
    };

    ignore_ctrl_c();

    // argv[0] is dropped: the daemon re-labels it as `roomler` before handing
    // the vector to clap, so usage/help text still reads `roomler ...`.
    let status = Command::new(&daemon)
        .arg("cli")
        .args(std::env::args_os().skip(1))
        .status();

    match status {
        // Pass the child's exit code through UNTRUNCATED.
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("roomler: failed to launch {}: {e}", daemon.display());
            std::process::exit(EXIT_NO_DAEMON);
        }
    }
}

#[cfg(target_os = "windows")]
const DAEMON_FILE_NAME: &str = "roomlerd.exe";
#[cfg(not(target_os = "windows"))]
const DAEMON_FILE_NAME: &str = "roomlerd";
