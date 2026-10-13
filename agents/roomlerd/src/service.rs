// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
// RETIRED-NAME-ANCHOR(3): the legacy scheduled-task name install/uninstall must still
// find.
//! Cross-platform auto-start-on-boot / login plumbing.
//!
//! `roomlerd service install` registers the agent so it launches
//! automatically on the next interactive login:
//!
//!   - **Windows**: Scheduled Task named `Roomler` (pre-P3D-rename hosts
//!     have `RoomlerAgent`, which `install()` retires and `uninstall()`
//!     deletes), ONLOGON trigger, LIMITED run level (matches the agent's
//!     un-elevated-by-design posture from `packaging/windows/README.txt`).
//!   - **Linux**: systemd user unit. The .deb already drops the unit
//!     file at `/usr/lib/systemd/user/roomler.service`; install
//!     here means `systemctl --user enable --now roomler.service`.
//!   - **macOS**: `launchctl load -w` the LaunchAgent plist. The .pkg
//!     postinstall drops it at `~/Library/LaunchAgents/com.roomler.agent.plist`;
//!     install here only needs to (re-)load it.
//!
//! All paths shell out to the system tool rather than talking to the
//! registries/D-Bus directly. Keeps the implementation small and means
//! we piggy-back on the OS's own permission prompts (e.g. macOS's
//! "Screen Recording" authorization that fires on first run).

use anyhow::{Context, Result, bail};

/// Status of the auto-start hook as reported by the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartStatus {
    /// Registered and will launch on next login.
    Installed,
    /// No trace of the hook — `install` would be a no-op if called
    /// repeatedly.
    NotInstalled,
    /// OS tool returned something unexpected or unreachable. Treat as
    /// inconclusive; operator should investigate manually.
    Unknown,
}

impl std::fmt::Display for AutostartStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutostartStatus::Installed => write!(f, "installed"),
            AutostartStatus::NotInstalled => write!(f, "not installed"),
            AutostartStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// Register the agent to start on next interactive login. Idempotent:
/// re-running after a successful install is harmless on all three
/// platforms (Task Scheduler replaces with `/F`, systemctl enable on
/// an already-enabled unit is a no-op, launchctl load on an already-
/// loaded plist returns exit 0).
pub fn install() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows::install()
    }
    #[cfg(target_os = "linux")]
    {
        linux::install()
    }
    #[cfg(target_os = "macos")]
    {
        macos::install()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        bail!("auto-start is not implemented on this platform")
    }
}

/// Tear down the auto-start hook. Idempotent: no-op when nothing is
/// installed (so `service uninstall` after a clean uninstall of the
/// MSI doesn't error out).
pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows::uninstall()
    }
    #[cfg(target_os = "linux")]
    {
        linux::uninstall()
    }
    #[cfg(target_os = "macos")]
    {
        macos::uninstall()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        bail!("auto-start is not implemented on this platform")
    }
}

/// FR-84 D3 — start the registered auto-start hook NOW: `schtasks /Run` the
/// task, `systemctl --user start` the unit, `launchctl kickstart` the agent.
///
/// The caller half of `Request::RestartDaemon` (`restart_by: "caller"`): the
/// Scheduled Task's own `<RestartOnFailure>` is a minute away at best, so the
/// companion / `roomler restart` runs this once the old daemon is gone. In
/// every flavour the daemon is then spawned by the OS's scheduler / service
/// manager — never as a child of the process calling this — so nothing of
/// the caller's is inherited by the daemon (#1035 is the reverse direction:
/// the daemon spawning the companion).
///
/// Harmless to repeat, which the caller relies on: a start that lands while
/// the old instance is still exiting is dropped by the task's `IgnoreNew`
/// policy, a running unit is a no-op, and a running LaunchAgent is left
/// alone (`kickstart` without `-k`). So the caller simply asks again until a
/// NEW daemon answers.
pub fn start() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows::start()
    }
    #[cfg(target_os = "linux")]
    {
        linux::start()
    }
    #[cfg(target_os = "macos")]
    {
        macos::start()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        bail!("auto-start is not supported on this platform")
    }
}

/// FR-93 P5 (#2144) — at start, a per-user Linux daemon whose unit file wants it
/// by `graphical-session.target` as well, while only `default.target` links to
/// it (it was enabled before that target joined the unit's `[Install]`), enables
/// it again once, so the next login starts it again. Runs on its own thread and
/// never blocks the caller; a no-op as root and off Linux. Kill switch
/// `ROOMLERD_UNIT_REPAIR=0`.
pub fn repair_session_wants_at_start() {
    #[cfg(target_os = "linux")]
    linux::repair_session_wants_at_start();
}

/// #1705 — the desktop companion's service probes, recognised from RAW argv:
/// `Some(false)` for `service status`, `Some(true)` for
/// `service status --as-service`, `None` for everything else (any other
/// spelling takes the normal path, full startup included).
///
/// The companion runs both every 10 s (`probe_service_state`). They ask the
/// SERVICE MANAGER one question, so the daemon answers them before any of its
/// own startup — DPI and timer requests, the companion scan, the legacy-tree
/// migration, the persistent log file — which is read here, ahead of clap and
/// of every appdirs consumer. Taking the full startup, they flooded the
/// per-user log with ~17,000 starts a day on every companion host.
pub fn companion_probe(argv: &[String]) -> Option<bool> {
    let rest: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    match rest.as_slice() {
        ["service", "status"] => Some(false),
        ["service", "status", "--as-service"] => Some(true),
        _ => None,
    }
}

/// Query whether the auto-start hook is currently registered.
pub fn status() -> Result<AutostartStatus> {
    #[cfg(target_os = "windows")]
    {
        windows::status()
    }
    #[cfg(target_os = "linux")]
    {
        linux::status()
    }
    #[cfg(target_os = "macos")]
    {
        macos::status()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        Ok(AutostartStatus::Unknown)
    }
}

// ---------------------------------------------------------------------------
// Windows: Scheduled Task via schtasks.exe
//
// We register the task by writing a UTF-16-LE-BOM XML document to a temp
// file and pointing `schtasks /XML` at it, rather than the simpler
// `schtasks /Create /SC ONLOGON ...` line. The XML buys us:
//
//   * `<RestartOnFailure>` — Task Scheduler relaunches the agent up to
//     10 times at 1-minute intervals after a non-zero exit, bringing
//     Windows to parity with systemd `Restart=on-failure` and macOS
//     `KeepAlive`. The plain /SC ONLOGON path has none of this.
//   * `<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>` and
//     `<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>` — schtasks
//     defaults to BOTH true, which silently terminates the agent the
//     moment a laptop unplugs.
//   * `<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>` —
//     belt for the resilience-plan single-instance lock; if Task
//     Scheduler ever fires a second copy due to a logon-event glitch,
//     it's dropped at the scheduler level.
//   * Secondary `<EventTrigger>` on EventID 12 (Microsoft-Windows-
//     Kernel-General "operating system started") — covers the
//     auto-logon-kiosk case where the user is signed in by the OS
//     before the LogonTrigger has a chance to fire.
//
// Schema is pinned to v1.2 (compatible all the way back to Win 7) so
// the same XML works on every Win10 / Win11 build we ship to.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// Every `schtasks` spawn, without a console window: FR-84 D6's wizard
    /// calls [`start`] in-process, and a GUI process has no console for a
    /// child to share — each spawn would flash one.
    fn schtasks() -> Command {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = Command::new("schtasks");
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }

    /// Canonical scheduled-task name after the P3D rename.
    pub const NEW_TASK_NAME: &str = "Roomler";
    /// Pre-rename scheduled-task name, retired best-effort by `install()`
    /// and deleted by `uninstall()`.
    // RETIRED-NAME-ANCHOR: without this the pre-rename scheduled task survives an
    // upgrade forever, so the host runs TWO daemons. See docs/fr/FR-21.
    pub const LEGACY_TASK_NAME: &str = "RoomlerAgent";

    pub fn install() -> Result<()> {
        let exe = std::env::current_exe().context("locating current exe")?;
        let exe_str = exe.to_string_lossy().into_owned();
        let user = current_user_qualified().context("resolving current user")?;

        let xml = render_task_xml(&exe_str, &user);
        let xml_path = write_temp_xml(&xml).context("writing temp XML")?;

        let result = schtasks()
            .args([
                "/Create",
                "/TN",
                NEW_TASK_NAME,
                "/XML",
                xml_path.to_string_lossy().as_ref(),
                "/F",
            ])
            .output()
            .context("running schtasks /Create /XML")?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            // Keep the XML on disk so the operator can inspect it when
            // schtasks rejects the document (rare on supported builds,
            // but the schema is fussy about element order).
            return Err(anyhow::anyhow!(
                "schtasks /Create /XML failed ({}): {}\n(XML kept at {} for inspection)",
                result.status,
                stderr.trim(),
                xml_path.display()
            ));
        }
        let _ = std::fs::remove_file(&xml_path);
        // New task is registered — retire the pre-rename task best-effort.
        // Idempotent: a fresh install (no legacy task) just no-ops here.
        let _ = schtasks()
            .args(["/Delete", "/TN", LEGACY_TASK_NAME, "/F"])
            .output();
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        // Delete BOTH the new and legacy tasks. Each tolerates "not found"
        // so an idempotent uninstall works on a migrated host (only the
        // new task exists), a pre-rename host (only the legacy), or a
        // half-migrated host (both).
        delete_task(NEW_TASK_NAME)?;
        delete_task(LEGACY_TASK_NAME)?;
        Ok(())
    }

    /// FR-84 D3 — `schtasks /Run` the task: the canonical name, or the
    /// legacy one on a host that still runs it. A non-admin user may run a
    /// task registered under their own account, which is how the perUser
    /// MSI registers it. The scheduler spawns the daemon; this process is
    /// not its parent.
    pub fn start() -> Result<()> {
        let name = if task_exists(NEW_TASK_NAME) {
            NEW_TASK_NAME
        } else if task_exists(LEGACY_TASK_NAME) {
            LEGACY_TASK_NAME
        } else {
            bail!(
                "no auto-start task is registered (run `roomlerd service install` first, \
                 or start the service from an elevated prompt)"
            );
        };
        let output = schtasks()
            .args(["/Run", "/TN", name])
            .output()
            .context("running schtasks /Run")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "schtasks /Run /TN {name} failed ({}): {}",
                output.status,
                stderr.trim()
            );
        }
        Ok(())
    }

    fn delete_task(name: &str) -> Result<()> {
        let output = schtasks()
            .args(["/Delete", "/TN", name, "/F"])
            .output()
            .context("running schtasks /Delete")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Task not found → idempotent uninstall, treat as success.
            //
            // Decide that by asking whether the task is STILL THERE, not by
            // reading schtasks' complaint: its messages are localized, and the
            // English-only equivalent of this check is exactly what took
            // winhost-a's overlay down on a German host (#373 — netsh answered
            // "Das Objekt ist bereits vorhanden." to an idempotent add and the
            // runtime aborted). Here the same miss would fail an uninstall that
            // had nothing left to do. The English match is kept as a fast path
            // so the common case costs no extra process spawn.
            if stderr.contains("cannot find")
                || stderr.contains("does not exist")
                || !task_exists(name)
            {
                return Ok(());
            }
            bail!(
                "schtasks /Delete failed ({}): {}",
                output.status,
                stderr.trim()
            );
        }
        Ok(())
    }

    /// Does this scheduled task exist? Locale-proof.
    ///
    /// `schtasks /Query /TN <name>` prints the task's own NAME on success, and
    /// a name we chose is the same in every locale — unlike the error text.
    /// A query that fails to RUN reports `true` ("assume still there"), so a
    /// caller treating absence as success can never be fooled by a broken
    /// probe into skipping real work.
    fn task_exists(name: &str) -> bool {
        let Ok(out) = schtasks().args(["/Query", "/TN", name]).output() else {
            return true;
        };
        if !out.status.success() {
            return false;
        }
        String::from_utf8_lossy(&out.stdout).contains(name)
    }

    /// Is `name` provably ABSENT from the full task list?
    ///
    /// The independent, locale-proof half of [`query_task`]'s error path: a
    /// listing that succeeds and does not mention our name is real evidence of
    /// absence, in any language. `/FO CSV /NH` keeps the output machine-shaped
    /// rather than the localized table.
    ///
    /// Returns `false` — "not proven absent" — whenever the listing itself
    /// fails, so an unavailable or access-denied schtasks yields `Unknown`
    /// rather than a confident `NotInstalled`.
    fn task_absent_from_listing(name: &str) -> bool {
        let Ok(out) = schtasks().args(["/Query", "/FO", "CSV", "/NH"]).output() else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        !String::from_utf8_lossy(&out.stdout).contains(name)
    }

    pub fn status() -> Result<AutostartStatus> {
        // Query the new task first; fall back to the legacy task so a
        // pre-rename host mid-migration still reports Installed.
        match query_task(NEW_TASK_NAME)? {
            AutostartStatus::NotInstalled => query_task(LEGACY_TASK_NAME),
            other => Ok(other),
        }
    }

    fn query_task(name: &str) -> Result<AutostartStatus> {
        let output = schtasks()
            .args(["/Query", "/TN", name])
            .output()
            .context("running schtasks /Query")?;
        if output.status.success() {
            // Confirm it really named OUR task: a success whose output doesn't
            // mention it is not evidence of installation.
            if String::from_utf8_lossy(&output.stdout).contains(name) {
                Ok(AutostartStatus::Installed)
            } else {
                Ok(AutostartStatus::Unknown)
            }
        } else {
            // Localized stderr again (see `task_exists`). English strings stay
            // as a no-extra-spawn fast path; otherwise get an INDEPENDENT
            // locale-proof answer by listing the tasks and looking for our own
            // name — re-running the query that just failed would prove nothing.
            //
            // `Unknown` must stay distinct from `NotInstalled`: a caller that
            // reinstalls on `NotInstalled` would thrash the task every time
            // schtasks itself is unavailable or access-denied. So only a
            // successful listing that lacks our name downgrades to
            // `NotInstalled`.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("cannot find")
                || stderr.contains("does not exist")
                || task_absent_from_listing(name)
            {
                Ok(AutostartStatus::NotInstalled)
            } else {
                Ok(AutostartStatus::Unknown)
            }
        }
    }

    /// Resolve the current user as `DOMAIN\username` (or `COMPUTERNAME\
    /// username` on a non-domain-joined machine). The Task Scheduler
    /// `Principal.UserId` field expects exactly that form, and so does
    /// `LogonTrigger.UserId` if we want the trigger scoped to one
    /// account rather than every interactive logon on the box.
    fn current_user_qualified() -> Result<String> {
        let user = std::env::var("USERNAME").context("reading %USERNAME%")?;
        let domain = std::env::var("USERDOMAIN")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_default();
        Ok(if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        })
    }

    /// Build the Task Scheduler XML that `schtasks /XML` consumes.
    /// Pure function — no I/O — so the test suite can pin the shape
    /// without touching the registry.
    fn render_task_xml(exe_path: &str, user: &str) -> String {
        let exe_xml = xml_escape(exe_path);
        let user_xml = xml_escape(user);
        // Schema 1.2 is the broadest compatible version (Win 7+). The
        // EventTrigger.Subscription is itself an XML query; its angle
        // brackets must be entity-encoded so the outer document parses.
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>roomlerd</Author>
    <Description>Roomler Daemon — keeps this device on your private Roomler mesh network and available for secure remote control (auto-start on logon, restart on failure).</Description>
    <URI>\Roomler</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user_xml}</UserId>
    </LogonTrigger>
    <EventTrigger>
      <Enabled>true</Enabled>
      <Subscription>&lt;QueryList&gt;&lt;Query Id="0" Path="System"&gt;&lt;Select Path="System"&gt;*[System[Provider[@Name='Microsoft-Windows-Kernel-General'] and EventID=12]]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;</Subscription>
    </EventTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user_xml}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>10</Count>
    </RestartOnFailure>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_xml}</Command>
      <Arguments>run --supervisor task</Arguments>
    </Exec>
  </Actions>
</Task>
"#
        )
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    /// Persist the XML as UTF-16-LE with BOM. schtasks /XML on every
    /// supported Windows build accepts that encoding; UTF-8 sometimes
    /// works but is documented as unsupported and has been seen to
    /// fail with "The argument is incorrect" on Win10 22H2.
    fn write_temp_xml(xml_utf8: &str) -> Result<PathBuf> {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("roomlerd-task-{}.xml", std::process::id()));
        let mut buf = Vec::with_capacity(xml_utf8.len() * 2 + 2);
        buf.extend_from_slice(&[0xFF, 0xFE]); // UTF-16-LE BOM
        for c in xml_utf8.encode_utf16() {
            buf.extend_from_slice(&c.to_le_bytes());
        }
        std::fs::write(&path, &buf).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn xml_template_pins_resilience_settings() {
            let xml = render_task_xml("C:\\path\\agent.exe", "DOMAIN\\user");
            // The five settings the resilience plan was specifically
            // about — losing any of these is the regression we're
            // guarding against.
            assert!(xml.contains("<RestartOnFailure>"));
            assert!(xml.contains("<Interval>PT1M</Interval>"));
            assert!(xml.contains("<Count>10</Count>"));
            assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
            assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
            assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
            assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
            // Belt-and-braces triggers and the un-elevated-by-design
            // run level are also load-bearing.
            assert!(xml.contains("<LogonTrigger>"));
            assert!(xml.contains("<EventTrigger>"));
            assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
            // Pinned schema and references.
            assert!(xml.contains(r#"version="1.2""#));
            assert!(xml.contains("C:\\path\\agent.exe"));
            assert!(xml.contains("DOMAIN\\user"));
        }

        #[test]
        fn xml_template_excludes_schema_1_3_only_elements() {
            // 0.1.52 shipped these in a Schema 1.2 document and
            // schtasks /XML rejected the whole task with
            // `(39,7):DisallowStartOnRemoteAppSession: ERROR: The
            // task XML contains an unexpected node`. Both elements
            // are Schema 1.3+ only. Document stays at 1.2 (broadest
            // Win-version compat), so these are out — the resilience-
            // critical settings (RestartOnFailure, battery, etc.)
            // are all Schema 1.2 native and survive.
            let xml = render_task_xml("C:\\path\\agent.exe", "DOMAIN\\user");
            assert!(
                !xml.contains("DisallowStartOnRemoteAppSession"),
                "DisallowStartOnRemoteAppSession is Schema 1.3+; must not appear in a 1.2 document"
            );
            assert!(
                !xml.contains("UseUnifiedSchedulingEngine"),
                "UseUnifiedSchedulingEngine is Schema 1.3+; must not appear in a 1.2 document"
            );
        }

        #[test]
        fn xml_template_escapes_special_chars_in_paths_and_user() {
            let xml = render_task_xml("C:\\Users\\Joe & Sons\\app.exe", "DOM\\joe<sons");
            assert!(
                xml.contains("Joe &amp; Sons"),
                "ampersand should be entity-encoded"
            );
            assert!(
                xml.contains("joe&lt;sons"),
                "less-than should be entity-encoded"
            );
            // The bare ampersand in a path must NOT survive — that
            // would break XML parsing when schtasks reads the file.
            assert!(!xml.contains("Joe & Sons"));
        }

        /// FR-84 D3 — the worker learns it is task-supervised from its own
        /// argv, so `RestartDaemon` knows the relaunch is the CALLER's job
        /// (the task's `<RestartOnFailure>` is a minute away and `IgnoreNew`
        /// would drop a second instance anyway). Written against the literal
        /// so it compiles on the pre-D3 tree, where it is RED: the action was
        /// a bare `run`, which `supervision::detect` reads as an orphan and
        /// refuses to restart.
        #[test]
        fn xml_template_marks_the_worker_as_task_supervised() {
            let xml = render_task_xml("C:\\path\\agent.exe", "DOMAIN\\user");
            assert!(
                xml.contains("<Arguments>run --supervisor task</Arguments>"),
                "the task action must carry the supervisor flag: {xml}"
            );
        }

        #[test]
        fn write_temp_xml_uses_utf16_le_bom() {
            let path = write_temp_xml("<?xml?>").unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(&bytes[..2], &[0xFF, 0xFE], "UTF-16-LE BOM missing");
            assert_eq!(
                &bytes[2..4],
                &[0x3C, 0x00],
                "first ASCII char `<` should be 0x3C 0x00 in UTF-16-LE"
            );
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: systemd user unit
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::process::Command;

    /// S1b — the canonical unit name, matching what the .deb ships
    /// (`usr/lib/systemd/user/roomler.service`, ExecStart=/usr/bin/roomlerd).
    /// Pre-S1b `roomlerd service install` only knew the legacy name below and
    /// couldn't SEE the packaged unit — so a .deb host that also ran
    /// `service install` ended up with TWO enabled units (double-start).
    pub const UNIT: &str = "roomler.service";
    /// Legacy unit name written by pre-S1b `service install` runs.
    // RETIRED-NAME-ANCHOR(2): the unit name on hosts installed before P3d
    // Slice B. Install/uninstall must still find it or an upgraded host ends
    // up with two enabled units and a silent restart storm.
    const LEGACY_UNIT: &str = "roomler-agent.service";

    /// Which unit name is in force on this host: the canonical name when its
    /// unit file exists (or on a fresh host), the legacy name only when ONLY
    /// the legacy unit file exists.
    fn resolved_unit() -> &'static str {
        if unit_file_present_for(UNIT) {
            UNIT
        } else if unit_file_present_for(LEGACY_UNIT) {
            LEGACY_UNIT
        } else {
            UNIT
        }
    }

    pub fn install() -> Result<()> {
        // Prefer the packaged unit; fall back to a user-local drop-in
        // (under the CANONICAL name) when running out of cargo.
        let unit = resolved_unit();
        if unit == UNIT && !unit_file_present_for(UNIT) {
            install_user_unit_file()?;
        }
        systemctl(&["--user", "daemon-reload"])?;
        systemctl(&["--user", "enable", "--now", unit])?;
        // Kill the double-start hazard: when enabling the canonical unit,
        // best-effort disable a stray legacy one.
        if unit == UNIT {
            let _ = systemctl(&["--user", "disable", "--now", LEGACY_UNIT]);
        }
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        // Ignore non-zero exit — disabling a non-enabled unit is fine.
        // Both names, so an upgraded host is fully cleaned.
        let _ = systemctl(&["--user", "disable", "--now", UNIT]);
        let _ = systemctl(&["--user", "disable", "--now", LEGACY_UNIT]);
        Ok(())
    }

    /// FR-84 D3 — start the per-user unit now (the resolved name, so a host
    /// still on the legacy unit starts that one). systemd spawns the daemon.
    pub fn start() -> Result<()> {
        systemctl(&["--user", "start", resolved_unit()])
    }

    /// FR-93 P5 (#2144) — the target that has to START the unit after a
    /// logout, because `PartOf=` stops it with the session.
    const SESSION_TARGET: &str = "graphical-session.target";

    /// The per-user unit's enablement: what `systemctl --user show` says, and
    /// what its unit file asks for.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct UnitEnablement {
        /// `UnitFileState`: `enabled`, `disabled`, `static`, …
        pub state: String,
        /// The targets that want the unit now: its `WantedBy=` property, from
        /// the `.wants` links the manager has loaded.
        pub wanted_by: Vec<String>,
        /// The targets its unit file's `[Install]` section names.
        pub install_wanted_by: Vec<String>,
    }

    /// Whether an enabled unit lacks a link its unit file asks for, the
    /// session target's: `enable` creates the links, so a unit enabled before
    /// its file named the target never got that one.
    pub(super) fn needs_session_wants(e: &UnitEnablement) -> bool {
        e.state == "enabled"
            && e.install_wanted_by.iter().any(|t| t == SESSION_TARGET)
            && !e.wanted_by.iter().any(|t| t == SESSION_TARGET)
    }

    /// The targets a unit file's `[Install]` section names in `WantedBy=`:
    /// every such line, space-separated, an empty assignment clearing the list
    /// (systemd's rule). Other sections and comments are not read.
    pub(super) fn install_wanted_by(unit_file: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut in_install = false;
        for raw in unit_file.lines() {
            let line = raw.trim();
            if line.starts_with('[') {
                in_install = line == "[Install]";
                continue;
            }
            if !in_install || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() != "WantedBy" {
                continue;
            }
            let value = value.trim();
            if value.is_empty() {
                out.clear();
            } else {
                out.extend(value.split_whitespace().map(str::to_string));
            }
        }
        out
    }

    /// What `systemctl --user show` says of `unit`, with its unit file read
    /// from disk: after a package upgrade the manager may still hold the old
    /// file, which nobody reloaded for this user. `None` without systemd, a
    /// user manager, or a unit file.
    fn enablement(unit: &str) -> Option<UnitEnablement> {
        let out = Command::new("systemctl")
            .args([
                "--user",
                "show",
                unit,
                "-p",
                "UnitFileState",
                "-p",
                "WantedBy",
                "-p",
                "FragmentPath",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (mut state, mut wanted_by, mut fragment) = (String::new(), Vec::new(), String::new());
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("UnitFileState=") {
                state = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("WantedBy=") {
                wanted_by = v.split_whitespace().map(str::to_string).collect();
            } else if let Some(v) = line.strip_prefix("FragmentPath=") {
                fragment = v.trim().to_string();
            }
        }
        let file = std::fs::read_to_string(&fragment).ok()?;
        Some(UnitEnablement {
            state,
            wanted_by,
            install_wanted_by: install_wanted_by(&file),
        })
    }

    /// See the module-level [`super::repair_session_wants_at_start`].
    ///
    /// ⚠️ Through `systemctl`, never by writing the link: the unit's sandbox
    /// (`ProtectHome=read-only`) leaves `~/.config/systemd/user` read-only to
    /// the daemon, and the user manager, which makes the link, is outside it.
    pub(super) fn repair_session_wants_at_start() {
        if !tunnel_core::env::flag("UNIT_REPAIR", true) {
            return;
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("unit-repair".into())
            .spawn(|| {
                let unit = resolved_unit();
                let Some(e) = enablement(unit) else {
                    return;
                };
                if !needs_session_wants(&e) {
                    return;
                }
                match systemctl(&["--user", "enable", unit]) {
                    Ok(()) => tracing::info!(
                        unit,
                        wanted_by = ?e.wanted_by,
                        "per-user unit: enabled for graphical-session.target too, so the next login \
                         starts the daemon again (#2144)"
                    ),
                    Err(err) => tracing::warn!(
                        unit,
                        error = %format!("{err:#}"),
                        "per-user unit: could not enable it for graphical-session.target; after a \
                         logout the daemon stays down until the next boot (#2144)"
                    ),
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(%e, "per-user unit: no thread to check its enablement (#2144)");
        }
    }

    pub fn status() -> Result<AutostartStatus> {
        // Either name counts as installed (an upgraded host may still run
        // the legacy-named unit until its next `service install`).
        for unit in [UNIT, LEGACY_UNIT] {
            let output = Command::new("systemctl")
                .args(["--user", "is-enabled", unit])
                .output();
            match output {
                Ok(o) if o.status.success() => return Ok(AutostartStatus::Installed),
                Ok(_) => {}
                Err(_) => return Ok(AutostartStatus::Unknown),
            }
        }
        Ok(AutostartStatus::NotInstalled)
    }

    fn systemctl(args: &[&str]) -> Result<()> {
        let output = Command::new("systemctl")
            .args(args)
            .output()
            .context("running systemctl")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "systemctl {:?} failed ({}): {}",
                args,
                output.status,
                stderr.trim()
            );
        }
        Ok(())
    }

    fn unit_file_present_for(name: &str) -> bool {
        let candidates = [
            format!("/usr/lib/systemd/user/{name}"),
            format!("/usr/local/lib/systemd/user/{name}"),
            format!("/etc/systemd/user/{name}"),
        ];
        candidates.iter().any(|p| std::path::Path::new(p).exists())
            || user_unit_path_for(name)
                .map(|p| p.exists())
                .unwrap_or(false)
    }

    fn user_unit_path() -> Option<std::path::PathBuf> {
        user_unit_path_for(UNIT)
    }

    fn user_unit_path_for(name: &str) -> Option<std::path::PathBuf> {
        directories::BaseDirs::new().map(|d| d.config_dir().join("systemd/user").join(name))
    }

    /// The unit `service install` writes where no packaged one exists. Wanted
    /// by `graphical-session.target` too, like the packaged unit (#2144):
    /// `PartOf=` stops it with the session, so the session has to start it.
    pub(super) fn user_unit_text(exe: &str) -> String {
        format!(
            "[Unit]\n\
             Description=Roomler Daemon — keeps this device on your private Roomler mesh network and available for secure remote control\n\
             After=graphical-session.target\n\
             PartOf=graphical-session.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe} run\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             Environment=RUST_LOG=roomlerd=info,tunnel_core=info,warn\n\
             \n\
             [Install]\n\
             WantedBy=default.target graphical-session.target\n"
        )
    }

    fn install_user_unit_file() -> Result<()> {
        let exe = std::env::current_exe().context("locating current exe")?;
        let unit = user_unit_text(&exe.to_string_lossy());
        let path = user_unit_path().context("resolving user unit path")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating user unit dir")?;
        }
        std::fs::write(&path, unit).context("writing user unit file")?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn ue(state: &str, wanted_by: &[&str], file_wants: &[&str]) -> UnitEnablement {
            UnitEnablement {
                state: state.to_string(),
                wanted_by: wanted_by.iter().map(|s| s.to_string()).collect(),
                install_wanted_by: file_wants.iter().map(|s| s.to_string()).collect(),
            }
        }

        /// #2144: enabled before the unit file named the session target, so
        /// only `default.target` links to it, and a user manager that outlives
        /// a logout never starts it again.
        #[test]
        fn an_enabled_unit_without_the_session_link_its_file_names_is_repaired() {
            let both = ["default.target", "graphical-session.target"];
            assert!(needs_session_wants(&ue(
                "enabled",
                &["default.target"],
                &both
            )));
        }

        #[test]
        fn nothing_to_repair_when_linked_disabled_or_the_file_is_older() {
            let both = ["default.target", "graphical-session.target"];
            assert!(
                !needs_session_wants(&ue("enabled", &both, &both)),
                "already linked"
            );
            assert!(
                !needs_session_wants(&ue("disabled", &[], &both)),
                "a unit the user switched off stays off"
            );
            assert!(!needs_session_wants(&ue("static", &[], &both)));
            assert!(
                !needs_session_wants(&ue("enabled", &["default.target"], &["default.target"])),
                "an older unit file: enabling again would add nothing"
            );
        }

        #[test]
        fn install_wanted_by_reads_only_the_install_section() {
            let file = "[Unit]\n\
                        WantedBy=not-this.target\n\
                        PartOf=graphical-session.target\n\
                        \n\
                        [Install]\n\
                        # WantedBy=commented.target\n\
                        WantedBy = default.target  graphical-session.target\n\
                        WantedBy=extra.target\n";
            assert_eq!(
                install_wanted_by(file),
                ["default.target", "graphical-session.target", "extra.target"]
            );
            assert_eq!(
                install_wanted_by("[Install]\nWantedBy=a.target\nWantedBy=\nWantedBy=b.target\n"),
                ["b.target"],
                "an empty assignment clears the list"
            );
            assert!(install_wanted_by("[Service]\nWantedBy=x.target\n").is_empty());
        }

        /// The two units a per-user install can run, the packaged one and the
        /// one `service install` writes, both stop with the session and are
        /// both started again by it.
        #[test]
        fn both_per_user_units_are_wanted_by_the_session_they_stop_with() {
            let packaged = include_str!("../packaging/linux/roomler.service");
            let written = user_unit_text("/usr/bin/roomlerd");
            for (name, unit) in [("packaged", packaged), ("written", written.as_str())] {
                assert!(unit.contains("PartOf=graphical-session.target"), "{name}");
                let wants = install_wanted_by(unit);
                assert!(
                    wants.iter().any(|t| t == "default.target"),
                    "{name}: {wants:?}"
                );
                assert!(
                    wants.iter().any(|t| t == SESSION_TARGET),
                    "{name}: {wants:?} (#2144)"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// macOS: LaunchAgent plist
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::process::Command;

    pub const PLIST: &str = "com.roomler.agent";

    pub fn install() -> Result<()> {
        let plist = plist_path()?;
        if !plist.exists() {
            write_plist(&plist)?;
        }
        let output = Command::new("launchctl")
            .args(["load", "-w", plist.to_string_lossy().as_ref()])
            .output()
            .context("running launchctl load")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // launchctl returns non-zero when the plist is already
            // loaded — treat that as success.
            if stderr.contains("already loaded") || stderr.contains("service already loaded") {
                return Ok(());
            }
            bail!(
                "launchctl load failed ({}): {}",
                output.status,
                stderr.trim()
            );
        }
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let plist = plist_path()?;
        if !plist.exists() {
            return Ok(());
        }
        let _ = Command::new("launchctl")
            .args(["unload", "-w", plist.to_string_lossy().as_ref()])
            .output();
        Ok(())
    }

    /// FR-84 D3 — `launchctl kickstart gui/<uid>/com.roomler.agent`: start
    /// the loaded LaunchAgent now (no `-k`: a running one is left alone).
    /// launchd spawns the daemon. Fails when the agent is not bootstrapped —
    /// `service install` is the remedy, and the error says so.
    pub fn start() -> Result<()> {
        // SAFETY: `getuid` takes no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        let target = format!("gui/{uid}/{PLIST}");
        let output = Command::new("launchctl")
            .args(["kickstart", &target])
            .output()
            .context("running launchctl kickstart")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "launchctl kickstart {target} failed ({}): {} (is the LaunchAgent loaded? \
                 `roomlerd service install` loads it)",
                output.status,
                stderr.trim()
            );
        }
        Ok(())
    }

    pub fn status() -> Result<AutostartStatus> {
        let output = Command::new("launchctl").args(["list", PLIST]).output();
        match output {
            Ok(o) if o.status.success() => Ok(AutostartStatus::Installed),
            Ok(_) => Ok(AutostartStatus::NotInstalled),
            Err(_) => Ok(AutostartStatus::Unknown),
        }
    }

    fn plist_path() -> Result<std::path::PathBuf> {
        let home = directories::BaseDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .context("resolving home dir")?;
        Ok(home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{PLIST}.plist")))
    }

    fn write_plist(path: &std::path::Path) -> Result<()> {
        let exe = std::env::current_exe().context("locating current exe")?;
        let exe_str = exe.to_string_lossy();
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key><string>{PLIST}</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array><string>{exe_str}</string><string>run</string></array>\n\
             \t<key>RunAtLoad</key><true/>\n\
             \t<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
             \t<key>ProcessType</key><string>Interactive</string>\n\
             \t<key>EnvironmentVariables</key>\n\
             \t<dict><key>RUST_LOG</key><string>roomlerd=info,tunnel_core=info,warn</string></dict>\n\
             </dict>\n\
             </plist>\n"
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating LaunchAgents dir")?;
        }
        std::fs::write(path, xml).context("writing LaunchAgent plist")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_companions_two_probes_skip_the_daemon_startup() {
        let argv = |a: &[&str]| -> Vec<String> {
            std::iter::once("roomlerd")
                .chain(a.iter().copied())
                .map(String::from)
                .collect()
        };
        assert_eq!(companion_probe(&argv(&["service", "status"])), Some(false));
        assert_eq!(
            companion_probe(&argv(&["service", "status", "--as-service"])),
            Some(true)
        );
        // Everything else keeps the full startup: other service verbs, other
        // commands, a global flag in front, trailing extras, no command.
        for other in [
            &["service", "install"][..],
            &["service", "status", "--as-service", "--verbose"],
            &["--config", "x.toml", "service", "status"],
            &["run"],
            &["status"],
            &[],
        ] {
            assert_eq!(companion_probe(&argv(other)), None, "{other:?}");
        }
    }

    #[test]
    fn autostart_status_renders_human_readable() {
        assert_eq!(AutostartStatus::Installed.to_string(), "installed");
        assert_eq!(AutostartStatus::NotInstalled.to_string(), "not installed");
        assert_eq!(AutostartStatus::Unknown.to_string(), "unknown");
    }

    #[test]
    fn status_returns_a_value_on_this_platform() {
        // The exact status depends on the test host. We only assert
        // that the call path doesn't panic and returns one of the
        // three known variants.
        match status() {
            Ok(
                AutostartStatus::Installed
                | AutostartStatus::NotInstalled
                | AutostartStatus::Unknown,
            ) => {}
            Err(_) => panic!("status() errored on this platform"),
        }
    }
}
