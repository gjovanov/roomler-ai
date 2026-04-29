//! Cross-platform auto-start-on-boot / login plumbing.
//!
//! `roomler-agent service install` registers the agent so it launches
//! automatically on the next interactive login:
//!
//!   - **Windows**: Scheduled Task named `RoomlerAgent`, ONLOGON trigger,
//!     LIMITED run level (matches the agent's un-elevated-by-design
//!     posture from `packaging/windows/README.txt`).
//!   - **Linux**: systemd user unit. The .deb already drops the unit
//!     file at `/usr/lib/systemd/user/roomler-agent.service`; install
//!     here means `systemctl --user enable --now roomler-agent.service`.
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

    pub const TASK_NAME: &str = "RoomlerAgent";

    pub fn install() -> Result<()> {
        let exe = std::env::current_exe().context("locating current exe")?;
        let exe_str = exe.to_string_lossy().into_owned();
        let user = current_user_qualified().context("resolving current user")?;

        let xml = render_task_xml(&exe_str, &user);
        let xml_path = write_temp_xml(&xml).context("writing temp XML")?;

        let result = Command::new("schtasks")
            .args([
                "/Create",
                "/TN",
                TASK_NAME,
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
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let output = Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output()
            .context("running schtasks /Delete")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Task not found → idempotent uninstall, treat as success.
            // schtasks prints "ERROR: The system cannot find the file specified."
            if stderr.contains("cannot find") || stderr.contains("does not exist") {
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

    pub fn status() -> Result<AutostartStatus> {
        let output = Command::new("schtasks")
            .args(["/Query", "/TN", TASK_NAME])
            .output()
            .context("running schtasks /Query")?;
        if output.status.success() {
            Ok(AutostartStatus::Installed)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("cannot find") || stderr.contains("does not exist") {
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
    <Author>roomler-agent</Author>
    <Description>Roomler AI remote-control agent — auto-start on logon, restart on failure.</Description>
    <URI>\RoomlerAgent</URI>
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
    <DisallowStartOnRemoteAppSession>false</DisallowStartOnRemoteAppSession>
    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>
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
      <Arguments>run</Arguments>
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
        let path = dir.join(format!("roomler-agent-task-{}.xml", std::process::id()));
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

    pub const UNIT: &str = "roomler-agent.service";

    pub fn install() -> Result<()> {
        // Assume the .deb already dropped the unit at
        // /usr/lib/systemd/user/roomler-agent.service. Fall back to a
        // user-local drop-in when running out of cargo.
        if !unit_file_present() {
            install_user_unit_file()?;
        }
        systemctl(&["--user", "daemon-reload"])?;
        systemctl(&["--user", "enable", "--now", UNIT])?;
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        // Ignore non-zero exit — disabling a non-enabled unit is fine.
        let _ = systemctl(&["--user", "disable", "--now", UNIT]);
        Ok(())
    }

    pub fn status() -> Result<AutostartStatus> {
        let output = Command::new("systemctl")
            .args(["--user", "is-enabled", UNIT])
            .output();
        match output {
            Ok(o) if o.status.success() => Ok(AutostartStatus::Installed),
            Ok(_) => Ok(AutostartStatus::NotInstalled),
            Err(_) => Ok(AutostartStatus::Unknown),
        }
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

    fn unit_file_present() -> bool {
        let candidates = [
            "/usr/lib/systemd/user/roomler-agent.service",
            "/usr/local/lib/systemd/user/roomler-agent.service",
            "/etc/systemd/user/roomler-agent.service",
        ];
        candidates.iter().any(|p| std::path::Path::new(p).exists())
            || user_unit_path().map(|p| p.exists()).unwrap_or(false)
    }

    fn user_unit_path() -> Option<std::path::PathBuf> {
        directories::BaseDirs::new()
            .map(|d| d.config_dir().join("systemd/user/roomler-agent.service"))
    }

    fn install_user_unit_file() -> Result<()> {
        let exe = std::env::current_exe().context("locating current exe")?;
        let exe_str = exe.to_string_lossy();
        let unit = format!(
            "[Unit]\n\
             Description=Roomler AI remote-control agent\n\
             After=graphical-session.target\n\
             PartOf=graphical-session.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe_str} run\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             Environment=RUST_LOG=roomler_agent=info,warn\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
        let path = user_unit_path().context("resolving user unit path")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating user unit dir")?;
        }
        std::fs::write(&path, unit).context("writing user unit file")?;
        Ok(())
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
             \t<dict><key>RUST_LOG</key><string>roomler_agent=info,warn</string></dict>\n\
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
