// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3437 — a run of the device's key helper: the line of its config, run
//! through a shell as the daemon (root or SYSTEM).

use std::process::{Output, Stdio};
use std::time::Duration;

/// A run of the helper's line, started.
pub(super) struct Run {
    child: tokio::process::Child,
}

/// Start `line` as the key helper runs: `/bin/sh -c "<line>"` on Unix.
///
/// P1i-2 — on Windows through `cmd.exe /d /s /c "<line>"`, by its full path:
/// `/s` makes cmd strip the outer quotes and run the helper as written, as
/// `/bin/sh -c` does, and the line is passed raw, so no quoting of Rust's own
/// can change it. `/d`: no AutoRun from the registry runs first.
pub(super) fn start(line: &str) -> std::io::Result<Run> {
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(line);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(crate::hive_win::system_cmd());
        cmd.raw_arg(format!("/d /s /c \"{line}\""))
            // CREATE_NO_WINDOW: no console flashes on the user's desktop.
            .creation_flags(0x0800_0000);
        cmd
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(Run {
        child: cmd.spawn()?,
    })
}

impl Run {
    /// The shell's pid: a test's.
    #[cfg(all(test, any(target_os = "linux", windows)))]
    pub(super) fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// What the helper printed and how it ended: `None` where it had not
    /// ended `within` its start.
    pub(super) async fn output_within(self, within: Duration) -> Option<std::io::Result<Output>> {
        tokio::time::timeout(within, self.child.wait_with_output())
            .await
            .ok()
    }
}

#[cfg(test)]
mod tests;
