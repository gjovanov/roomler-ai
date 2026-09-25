// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-84 D6 — registering the desktop companion to start at login, on the
//! platforms where Roomler owns that registration.
//!
//! | platform | mechanism | who writes it | a person's opt-out |
//! |---|---|---|---|
//! | Windows perMachine | `HKLM\…\Run\Roomler Desktop` | the elevated `service install --as-service` (MSI CA), self-healed by the SCM host at every start | the companion's per-user state; an `--autostart` launch exits |
//! | Windows perUser | `HKCU\…\Run\Roomler Desktop` | the per-user `service install` (MSI CA) and the companion itself | the companion deletes the value and records the choice |
//! | Linux (.deb) | `/etc/xdg/autostart/roomler-desktop.desktop` | the package | `~/.config/autostart/roomler-desktop.desktop` with `Hidden=true` |
//! | macOS (.pkg) | the `com.roomler.desktop` LaunchAgent (`RunAtLoad`) | the package's postinstall | macOS Login Items — not ours to write |
//!
//! Shared by the daemon and the companion so both spell the value, the
//! argument and the entry the same way. Pure helpers are tested on every
//! platform; the registry calls are Windows-only and take the subkey as a
//! parameter, so their tests run against a scratch key instead of the real
//! `Run` key.

use std::path::{Path, PathBuf};

/// The argument a login start passes: stay in the tray (or exit, for a
/// person who opted out) instead of opening the window.
pub const AUTOSTART_ARG: &str = "--autostart";
/// The argument the daemon's one-shot post-install launch passes: open the
/// Welcome tour.
pub const FIRST_RUN_ARG: &str = "--first-run";

/// `HKLM`/`HKCU` subkey that holds login-start commands.
pub const WINDOWS_RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The value name under [`WINDOWS_RUN_SUBKEY`]. A compatibility surface:
/// renaming it strands every existing registration (two entries, then one
/// that nothing removes).
pub const WINDOWS_RUN_VALUE_NAME: &str = "Roomler Desktop";

/// The companion's desktop-entry file name — the system autostart entry and
/// the per-user override MUST share it, since XDG autostart matches the two
/// by file name.
pub const XDG_ENTRY_FILE: &str = "roomler-desktop.desktop";
/// Where the `.deb` installs the system-wide autostart entry.
pub const XDG_SYSTEM_AUTOSTART: &str = "/etc/xdg/autostart/roomler-desktop.desktop";

/// The data of the Windows Run value: the quoted EXE path, then
/// [`AUTOSTART_ARG`]. Quoted always — `C:\Program Files\…` has a space, and an
/// unquoted Run command with a space is parsed by the shell as
/// `C:\Program` + arguments.
pub fn windows_run_command(exe: &Path) -> String {
    // STUB (RED stage).
    exe.display().to_string()
}

/// What the Run value SHOULD be, or `None` for "no value".
///
/// `device_enabled` is the device's `companion_autostart`; `user_opted_out`
/// is a person's own "Start at login: off" and only applies to the per-user
/// (`HKCU`) value — the machine value serves every account, and a person's
/// opt-out is honoured by the companion at `--autostart` instead.
pub fn desired_run_value(
    _device_enabled: bool,
    exe: &Path,
    _exe_exists: bool,
    _user_opted_out: bool,
) -> Option<String> {
    // STUB (RED stage).
    Some(windows_run_command(exe))
}

/// What to do to the registry to get from `current` to `desired`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunKeyAction {
    Keep,
    Write(String),
    Remove,
}

/// Plan the change. Pure: the registry is read and written by the caller.
pub fn plan_run_key(_current: Option<&str>, _desired: Option<&str>) -> RunKeyAction {
    // STUB (RED stage).
    RunKeyAction::Keep
}

/// Which Windows Run key owns an EXE's login start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    /// Under Program Files: a perMachine install, `HKLM`.
    Machine,
    /// Anywhere else (`%LOCALAPPDATA%\Programs\Roomler`): per-user, `HKCU`.
    User,
}

/// Classify `exe` by its location. `program_files` are the candidate
/// Program Files roots (`%ProgramFiles%`, `%ProgramW6432%`); the comparison
/// is by whole path components and ignores case and slash direction.
pub fn windows_scope_for_exe(_exe: &Path, _program_files: &[PathBuf]) -> InstallScope {
    // STUB (RED stage).
    InstallScope::User
}

/// The per-user XDG autostart directory: `$XDG_CONFIG_HOME/autostart` when
/// that is set to an absolute path (the spec ignores a relative one), else
/// `~/.config/autostart`.
pub fn xdg_user_autostart_dir(
    xdg_config_home: Option<&str>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    // STUB (RED stage).
    let _ = xdg_config_home;
    home.map(|h| h.join("autostart"))
}

/// Does this desktop entry carry `Hidden=true` in its `[Desktop Entry]`
/// group? That is the XDG way to switch a system autostart entry off for
/// one user.
pub fn xdg_entry_hidden(_content: &str) -> bool {
    // STUB (RED stage).
    false
}

/// A per-user autostart entry for the companion at `exe`, switched off
/// (`Hidden=true`) or on. Marked `X-Roomler-Managed=true` so the companion
/// knows the file is its own to rewrite or delete.
pub fn xdg_user_entry(_exe: &str, _hidden: bool) -> String {
    // STUB (RED stage).
    String::new()
}

/// Registry access for the Run value. Windows-only; every call takes the
/// subkey so tests can point it at a scratch key.
#[cfg(windows)]
pub mod registry {
    use super::RunKeyAction;

    /// Which hive.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Hive {
        LocalMachine,
        CurrentUser,
    }

    /// Read a `REG_SZ` value; `Ok(None)` when the key or the value is absent.
    pub fn read_value(_hive: Hive, _subkey: &str, _name: &str) -> std::io::Result<Option<String>> {
        // STUB (RED stage).
        Ok(None)
    }

    /// Write a `REG_SZ` value, creating the key when needed.
    pub fn write_value(
        _hive: Hive,
        _subkey: &str,
        _name: &str,
        _data: &str,
    ) -> std::io::Result<()> {
        // STUB (RED stage).
        Ok(())
    }

    /// Delete a value; `Ok(false)` when there was nothing to delete.
    pub fn delete_value(_hive: Hive, _subkey: &str, _name: &str) -> std::io::Result<bool> {
        // STUB (RED stage).
        Ok(false)
    }

    /// Bring the value at `subkey\name` to `desired` (`None` = absent) and
    /// say what was done.
    pub fn sync_value(
        hive: Hive,
        subkey: &str,
        name: &str,
        desired: Option<&str>,
    ) -> std::io::Result<RunKeyAction> {
        let current = read_value(hive, subkey, name)?;
        let action = super::plan_run_key(current.as_deref(), desired);
        match &action {
            RunKeyAction::Keep => {}
            RunKeyAction::Write(data) => write_value(hive, subkey, name, data)?,
            RunKeyAction::Remove => {
                delete_value(hive, subkey, name)?;
            }
        }
        Ok(action)
    }

    /// Test-only: remove a scratch key and everything under it.
    #[cfg(test)]
    pub(crate) fn delete_tree(_hive: Hive, _subkey: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_command_quotes_a_path_with_spaces() {
        let exe = Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe");
        assert_eq!(
            windows_run_command(exe),
            r#""C:\Program Files\Roomler\roomler-desktop.exe" --autostart"#
        );
        // Quoted even without a space: one shape, so a comparison with what
        // is in the registry never flips on the install path.
        assert_eq!(
            windows_run_command(Path::new(r"D:\r\roomler-desktop.exe")),
            r#""D:\r\roomler-desktop.exe" --autostart"#
        );
    }

    #[test]
    fn desired_value_table() {
        let exe = Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe");
        let on = Some(windows_run_command(exe));
        // (device_enabled, exe_exists, user_opted_out) → desired
        let table = [
            ((true, true, false), on.clone()),
            ((true, true, true), None), // a person switched it off (HKCU)
            ((true, false, false), None), // daemon-only install / -SkipDesktop
            ((false, true, false), None), // the device kill switch
            ((false, false, true), None),
        ];
        for ((device, exists, opted_out), want) in table {
            assert_eq!(
                desired_run_value(device, exe, exists, opted_out),
                want,
                "device={device} exists={exists} opted_out={opted_out}"
            );
        }
    }

    #[test]
    fn plan_table() {
        use RunKeyAction::*;
        let v = r#""C:\x\roomler-desktop.exe" --autostart"#;
        assert_eq!(plan_run_key(None, None), Keep);
        assert_eq!(plan_run_key(Some(v), Some(v)), Keep);
        assert_eq!(plan_run_key(None, Some(v)), Write(v.to_string()));
        assert_eq!(
            plan_run_key(Some(r#""C:\old\roomler-desktop.exe" --autostart"#), Some(v)),
            Write(v.to_string()),
            "a moved install rewrites the value"
        );
        assert_eq!(plan_run_key(Some(v), None), Remove);
    }

    #[test]
    fn scope_follows_program_files() {
        let pf = [
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files (x86)"),
        ];
        assert_eq!(
            windows_scope_for_exe(
                Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe"),
                &pf
            ),
            InstallScope::Machine
        );
        assert_eq!(
            windows_scope_for_exe(
                Path::new(r"c:/program files/ROOMLER/roomler-desktop.exe"),
                &pf
            ),
            InstallScope::Machine,
            "case and slash direction do not matter"
        );
        assert_eq!(
            windows_scope_for_exe(
                Path::new(r"C:\Users\ana\AppData\Local\Programs\Roomler\roomler-desktop.exe"),
                &pf
            ),
            InstallScope::User
        );
        assert_eq!(
            windows_scope_for_exe(
                Path::new(r"C:\Program Files Extra\roomler-desktop.exe"),
                &pf
            ),
            InstallScope::User,
            "a sibling directory that merely starts with the same letters is not inside it"
        );
        assert_eq!(
            windows_scope_for_exe(
                Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe"),
                &[]
            ),
            InstallScope::User,
            "no Program Files root known ⇒ per-user, never a guess at HKLM"
        );
    }

    #[test]
    fn xdg_dir_honours_an_absolute_config_home_only() {
        let home = Path::new("/home/ana");
        assert_eq!(
            xdg_user_autostart_dir(Some("/cfg"), Some(home)),
            Some(PathBuf::from("/cfg/autostart"))
        );
        assert_eq!(
            xdg_user_autostart_dir(Some("relative/cfg"), Some(home)),
            Some(PathBuf::from("/home/ana/.config/autostart")),
            "the XDG spec says a relative XDG_CONFIG_HOME is ignored"
        );
        assert_eq!(
            xdg_user_autostart_dir(None, Some(home)),
            Some(PathBuf::from("/home/ana/.config/autostart"))
        );
        assert_eq!(xdg_user_autostart_dir(Some(""), None), None);
    }

    #[test]
    fn hidden_is_read_from_the_desktop_entry_group_only() {
        assert!(xdg_entry_hidden(
            "[Desktop Entry]\nName=Roomler\nHidden=true\n"
        ));
        assert!(xdg_entry_hidden("[Desktop Entry]\nHidden = True\n"));
        assert!(!xdg_entry_hidden("[Desktop Entry]\nHidden=false\n"));
        assert!(!xdg_entry_hidden("[Desktop Entry]\nName=Roomler\n"));
        assert!(
            !xdg_entry_hidden("[Desktop Entry]\nName=R\n[Desktop Action x]\nHidden=true\n"),
            "a key in another group does not hide the entry"
        );
        assert!(!xdg_entry_hidden("# Hidden=true\n[Desktop Entry]\n"));
    }

    #[test]
    fn user_entry_round_trips_through_the_parser() {
        let off = xdg_user_entry("/usr/bin/roomler-desktop", true);
        assert!(off.starts_with("[Desktop Entry]\n"), "{off}");
        assert!(
            off.contains("Exec=/usr/bin/roomler-desktop --autostart\n"),
            "{off}"
        );
        assert!(off.contains("X-Roomler-Managed=true\n"), "{off}");
        assert!(xdg_entry_hidden(&off));
        let on = xdg_user_entry("/usr/bin/roomler-desktop", false);
        assert!(!xdg_entry_hidden(&on));
        let spaced = xdg_user_entry("/opt/My Apps/roomler-desktop", false);
        assert!(
            spaced.contains("Exec=\"/opt/My Apps/roomler-desktop\" --autostart\n"),
            "{spaced}"
        );
    }

    /// The registry round trip against a SCRATCH key under HKCU — never the
    /// real Run key — cleaned up before and after.
    #[cfg(windows)]
    #[test]
    fn registry_sync_round_trip_on_a_scratch_key() {
        use registry::{Hive, delete_tree, read_value, sync_value};
        let scratch = format!(
            r"Software\RoomlerTest\companion-autostart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        delete_tree(Hive::CurrentUser, &scratch);
        let v = windows_run_command(Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe"));

        assert_eq!(
            read_value(Hive::CurrentUser, &scratch, WINDOWS_RUN_VALUE_NAME).unwrap(),
            None
        );
        assert_eq!(
            sync_value(
                Hive::CurrentUser,
                &scratch,
                WINDOWS_RUN_VALUE_NAME,
                Some(&v)
            )
            .unwrap(),
            RunKeyAction::Write(v.clone())
        );
        assert_eq!(
            read_value(Hive::CurrentUser, &scratch, WINDOWS_RUN_VALUE_NAME).unwrap(),
            Some(v.clone())
        );
        assert_eq!(
            sync_value(
                Hive::CurrentUser,
                &scratch,
                WINDOWS_RUN_VALUE_NAME,
                Some(&v)
            )
            .unwrap(),
            RunKeyAction::Keep,
            "idempotent"
        );
        assert_eq!(
            sync_value(Hive::CurrentUser, &scratch, WINDOWS_RUN_VALUE_NAME, None).unwrap(),
            RunKeyAction::Remove
        );
        assert_eq!(
            read_value(Hive::CurrentUser, &scratch, WINDOWS_RUN_VALUE_NAME).unwrap(),
            None
        );
        assert_eq!(
            sync_value(Hive::CurrentUser, &scratch, WINDOWS_RUN_VALUE_NAME, None).unwrap(),
            RunKeyAction::Keep,
            "removing an absent value is a no-op, not an error"
        );
        delete_tree(Hive::CurrentUser, &scratch);
    }
}
