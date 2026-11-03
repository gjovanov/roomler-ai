// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 C3 — the wizard's command line, and its elevated relaunch.
//!
//! A limited wizard on a reinstall over the pre-logon service (SystemContext)
//! cannot write that role's config (#3116), so the Welcome page offers
//! "Restart as administrator": the wizard starts its own image again through
//! `runas` (`wizard_shared::elevation::run_elevated`, one UAC prompt) with
//!
//! ```text
//! --role <kebab role> [--server <url>] [--session-data <folder>]
//!     --after-elevation <the limited copy's pid>
//! ```
//!
//! and exits. The elevated copy reads them back here: it waits for that pid to
//! be gone before Tauri's single-instance plugin looks for a running copy
//! (`main.rs`), opens on Welcome with the role preselected (the SPA asks
//! [`crate::commands::cmd_launch_options`]; the role wins over the detected
//! default) and keeps its WebView2 data apart from the limited copy's, in a
//! folder the session's user can open ([`webview_data_directory`]).
//!
//! Nothing secret is on this command line: the relaunch happens on Welcome,
//! before a token is pasted or paired. Every value is validated on both
//! sides, an unknown argument is ignored, and a bad value is dropped, never
//! fatal: the wizard then simply starts as if launched without it.

use crate::role::Role;

/// The role to preselect.
pub const ROLE_FLAG: &str = "--role";
/// The Server page's URL, when it is not the default.
pub const SERVER_FLAG: &str = "--server";
/// The limited copy's local app-data folder ([`elevated_webview_folder`]).
pub const SESSION_DATA_FLAG: &str = "--session-data";
/// The limited copy's process id, to wait for.
pub const AFTER_ELEVATION_FLAG: &str = "--after-elevation";

/// How long the elevated copy waits for the limited one to exit. The limited
/// copy exits as soon as the shell reports the elevated start, which is after
/// the person approved the prompt, so this is a bound for a stuck exit only.
/// A generous one: the wait ends the moment the copy is gone, past the bound
/// the elevated copy hands its arguments to the dying one and quits, and one
/// exit in the field took 12.2 s.
pub const PARENT_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a copy that was ended past [`PARENT_EXIT_WAIT`] gets to be gone.
pub const PARENT_END_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What the wizard was started with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchArgs {
    pub role: Option<Role>,
    pub server: Option<String>,
    pub session_data: Option<String>,
    pub after_elevation: Option<u32>,
}

impl LaunchArgs {
    /// This process's own arguments. A "Run as administrator" by hand has no
    /// limited copy to name the session's folder, so it is looked for (#3229).
    pub fn from_env() -> LaunchArgs {
        let mut args = LaunchArgs::parse(std::env::args().skip(1));
        if args.session_data.is_none() {
            args.session_data = session_data_of_another_account();
        }
        args
    }

    /// Parse `args` (without the program name): `--flag value` or
    /// `--flag=value`; the last of a repeated flag wins; a value that does not
    /// validate is dropped; anything else is ignored. A flag's value is never
    /// another flag, so `--role --server x` drops the role and keeps the
    /// server.
    pub fn parse<I, S>(args: I) -> LaunchArgs
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        let mut out = LaunchArgs::default();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            i += 1;
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
                _ => (arg.as_str(), None),
            };
            if ![
                ROLE_FLAG,
                SERVER_FLAG,
                SESSION_DATA_FLAG,
                AFTER_ELEVATION_FLAG,
            ]
            .contains(&flag)
            {
                continue;
            }
            let value = match inline {
                Some(v) => v,
                None => match args.get(i) {
                    Some(next) if !next.starts_with("--") => {
                        i += 1;
                        next.clone()
                    }
                    _ => continue,
                },
            };
            match flag {
                ROLE_FLAG => out.role = Role::from_state_str(&value),
                SERVER_FLAG => out.server = safe_server(&value).map(str::to_string),
                SESSION_DATA_FLAG => out.session_data = safe_folder(&value).map(str::to_string),
                _ => out.after_elevation = value.parse::<u32>().ok().filter(|p| *p != 0),
            }
        }
        out
    }
}

/// A server URL fit for the command line and the Server page: `http(s)://`
/// and something after it, at most 2048 characters, and no whitespace,
/// control character, quote or backslash (so it never needs quoting, and the
/// C runtime's parser hands it back unchanged).
pub fn safe_server(value: &str) -> Option<&str> {
    let lower = value.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    let fit = !rest.is_empty()
        && value.len() <= 2048
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\');
    fit.then_some(value)
}

/// A folder fit for the command line and for WebView2: an absolute path on a
/// local drive (`X:\...`, so no share and no device path), at most 1024
/// characters, with no `..`, no control character and nothing a Windows file
/// name cannot hold. Spaces are fine: `elevation::command_line` quotes them.
pub fn safe_folder(value: &str) -> Option<&str> {
    let rest = value
        .strip_prefix(|c: char| c.is_ascii_alphabetic())?
        .strip_prefix(":\\")?;
    let fit = !rest.is_empty()
        && value.len() <= 1024
        && !rest
            .chars()
            .any(|c| c.is_control() || "\"<>|?*:/".contains(c))
        && !rest.split('\\').any(|part| part == "..");
    fit.then_some(value)
}

/// #3229 — the local app-data folder of the person at this screen, when this is
/// an elevated wizard running as another account. The limited user can write that
/// folder whichever way it is learned, which is why [`make_elevated_folder`]
/// follows no link in it.
fn session_data_of_another_account() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        use roomlerd::win_identity::{SessionAccount, process_is_elevated, session_account};
        if !process_is_elevated() {
            return None;
        }
        let SessionAccount::Another { profile } = session_account() else {
            return None;
        };
        local_app_data_under(&profile?)
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// A profile's local app-data folder, when it is there and fit to use.
#[cfg(any(target_os = "windows", test))]
fn local_app_data_under(profile: &str) -> Option<String> {
    let folder = std::path::Path::new(profile).join("AppData").join("Local");
    let folder = folder.to_str()?;
    safe_folder(folder)
        .filter(|f| std::path::Path::new(f).is_dir())
        .map(str::to_string)
}

/// #3229 — stop before any webview when nothing it could use was found. WebView2's
/// browser runs as the person at the screen, who cannot open this account's
/// profile, so the only other outcome is a white window that goes away after about
/// a minute. A sentence and a way on are better.
pub fn refuse_if_no_session_folder(args: &LaunchArgs) {
    #[cfg(target_os = "windows")]
    {
        use roomlerd::win_identity::{SessionAccount, process_is_elevated, session_account};
        if args.session_data.is_none()
            && process_is_elevated()
            && matches!(session_account(), SessionAccount::Another { .. })
        {
            wizard_shared::elevation::show_fatal(
                "Roomler Setup cannot open its window this way: it is running as another \
                 account, and cannot find a folder the person signed in here can use. Start it \
                 normally, without Run as administrator, and choose Restart as administrator \
                 inside it.",
            );
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = args;
}

/// The arguments the limited copy hands the elevated one: the role, the
/// server only when it is a fit one that differs from `default_server`, the
/// limited copy's local app-data folder when it is a fit one, and the limited
/// copy's own pid.
pub fn relaunch_args(
    role: Role,
    server: Option<&str>,
    default_server: &str,
    session_data: Option<&str>,
    pid: u32,
) -> Vec<String> {
    let mut args = vec![ROLE_FLAG.to_string(), role.wire().to_string()];
    let same = |a: &str, b: &str| {
        a.trim_end_matches('/')
            .eq_ignore_ascii_case(b.trim_end_matches('/'))
    };
    if let Some(server) = server.and_then(safe_server)
        && !same(server, default_server)
    {
        args.push(SERVER_FLAG.to_string());
        args.push(server.to_string());
    }
    if let Some(folder) = session_data.and_then(safe_folder) {
        args.push(SESSION_DATA_FLAG.to_string());
        args.push(folder.to_string());
    }
    args.push(AFTER_ELEVATION_FLAG.to_string());
    args.push(pid.to_string());
    args
}

/// This process's own `%LOCALAPPDATA%`.
pub fn local_app_data() -> Option<std::path::PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_local_dir().to_path_buf())
}

/// FR-129 C — an override an elevated wizard refuses from its environment. A
/// limited process can write any variable to `HKCU\Environment`, and an elevated
/// process of that user inherits it: `ROOMLER_INSTALLER_PROXY_BASE` there would
/// name where the MSI comes from, and nothing would ask again before it runs.
/// `ROOMLER*` are this product's own switches, `WEBVIEW2_*` and `TAURI_*` steer
/// the webview, `SSL_CERT_*` name what TLS trusts. A proxy stays: a corporate
/// one is how the download gets out, and TLS is still checked through it.
pub fn distrusted_variable(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    name.starts_with("ROOMLER")
        || name.starts_with("WEBVIEW2_")
        || name.starts_with("TAURI_")
        || name == "SSL_CERT_FILE"
        || name == "SSL_CERT_DIR"
}

/// FR-129 C — before anything reads the environment or spawns a program, in
/// `main`, while no other thread exists.
///
/// - The variables that name the machine's own folders take their true values
///   on every run. The code the wizard links reads them (`%ProgramFiles%` for
///   the install folder a prompt is then shown for, `%ProgramData%` for where a
///   config with a token goes), and `HKCU\Environment` can override each.
/// - The current directory leaves the person's Downloads, so nothing started
///   from here looks there for a program or a DLL.
/// - An elevated run also drops every [`distrusted_variable`]. A limited run
///   keeps them: it crosses nothing its user cannot already do, and test
///   fixtures point it at a staging server through them.
pub fn start_clean() {
    #[cfg(target_os = "windows")]
    {
        for (name, value) in roomlerd::appdirs::machine_folder_variables() {
            // SAFETY: the first thing `main` does, before any other thread.
            unsafe { std::env::set_var(name, value) };
        }
        if roomlerd::win_identity::process_is_elevated() {
            let names: Vec<_> = std::env::vars_os()
                .map(|(name, _)| name)
                .filter(|name| distrusted_variable(name))
                .collect();
            for name in names {
                // SAFETY: as above.
                unsafe { std::env::remove_var(name) };
            }
        }
        let _ = std::env::set_current_dir(roomlerd::appdirs::system_directory());
    }
}

/// Do these name two different local app-data folders? Case and a trailing
/// separator aside. `false` when either is unknown.
#[cfg(any(target_os = "windows", test))]
fn names_another_profile(session_data: Option<&str>, own: Option<&std::path::Path>) -> bool {
    let clean = |s: &str| s.trim_end_matches(['\\', '/']).to_ascii_lowercase();
    match (session_data, own.and_then(std::path::Path::to_str)) {
        (Some(session), Some(own)) => clean(session) != clean(own),
        _ => false,
    }
}

/// FR-129 C — is this an elevated wizard running as ANOTHER account than the
/// person at the screen? Over-the-shoulder credentials: a standard user started
/// it and an administrator typed their password. The limited copy named its own
/// local app-data folder (`--session-data`), or [`LaunchArgs::from_env`] found it
/// (#3229), so a different one here says so.
/// Only the machine-wide service is right then: the other roles would install
/// into that administrator's profile and report Done
/// ([`crate::role::roles_open_here`]).
pub fn on_another_account(args: &LaunchArgs) -> bool {
    #[cfg(target_os = "windows")]
    {
        roomlerd::win_identity::process_is_elevated()
            && names_another_profile(args.session_data.as_deref(), local_app_data().as_deref())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        false
    }
}

/// FR-129 C — what this run narrows the role cards to.
pub fn role_limit_here() -> crate::role::RoleLimit {
    crate::role::role_limit(
        on_another_account(&LaunchArgs::from_env()),
        wizard_shared::elevation::elevation_refused_by_policy(),
    )
}

/// FR-129 C — open `url` in the session's own browser, from an ELEVATED wizard.
/// A browser started directly would inherit this process's token: the
/// administrator's profile on a standard user's desktop under over-the-shoulder
/// credentials, and elevated (with per-user handler overrides in play) for a
/// same-user elevation. Explorer hands the request to the shell that owns the
/// desktop, which runs as that desktop's user and is not elevated.
#[cfg(target_os = "windows")]
pub fn open_through_the_shell(url: &str) -> std::io::Result<()> {
    std::process::Command::new(roomlerd::appdirs::windows_directory().join("explorer.exe"))
        .arg(url)
        .spawn()
        .map(drop)
}

/// The WebView2 user-data folder of an ELEVATED wizard: beside the limited
/// one's (`%LOCALAPPDATA%\ai.roomler.setup`, where Tauri puts it), never in
/// it. WebView2 shares one browser process among every host that names the
/// same folder ("Manage user data folders", Microsoft Learn), and the limited
/// copy's can still be running for a moment after that copy has exited; an
/// elevated host joining a limited browser process is the cross-integrity
/// failure.
///
/// Under `session_data`, the limited copy's `%LOCALAPPDATA%`, when the relaunch
/// carried it or it was found (#3229) and the folder is there, because WebView2's browser
/// process de-elevates to the user of the SESSION, whoever the host runs as.
/// Under over-the-shoulder credentials the host is the administrator who typed
/// the password and the browser is the standard user, who cannot open the
/// administrator's profile: the browser cannot make its data directory and the
/// wizard is a white window that goes away after about a minute. The limited
/// copy's folder is one that user can write, and for an administrator approving
/// their own prompt it is the same folder as this process's.
///
/// Without it, this process's own: a person elevating themselves.
/// `None` when there is no local app-data folder to put it in.
pub fn elevated_webview_folder(session_data: Option<&str>) -> Option<std::path::PathBuf> {
    session_data
        .map(std::path::PathBuf::from)
        .filter(|folder| folder.is_dir())
        .or_else(local_app_data)
        .map(|folder| folder.join(TAURI_IDENTIFIER).join("elevated"))
}

/// FR-129 C — make `dir` a new, empty folder: run before WebView2 is pointed at
/// it, with the administrator's token, in a place the limited user can write.
/// Nothing a limited process left there may reach the elevated host's browser
/// (a profile, an extension, a preference), and no link there is followed: a
/// junction or symlink at either of the two names this wizard owns is removed,
/// itself and not what it points at, before anything is created through it.
pub fn make_elevated_folder(dir: &std::path::Path) -> std::io::Result<()> {
    let is_link = |p: &std::path::Path| {
        p.symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
    };
    for path in [dir.parent(), Some(dir)].into_iter().flatten() {
        if is_link(path) {
            std::fs::remove_dir(path).or_else(|_| std::fs::remove_file(path))?;
        }
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(dir)
}

/// `tauri.conf.json`'s `identifier`, the folder Tauri keeps the limited
/// copy's WebView2 data in (asserted against the file by a test).
pub const TAURI_IDENTIFIER: &str = "ai.roomler.setup";

/// The label of the one window, created by `main.rs`'s setup hook from its
/// `tauri.conf.json` entry (which says `"create": false`, so Tauri does not
/// create it first without the folder below).
pub const MAIN_WINDOW: &str = "main";

/// The user-data folder `main.rs` hands the main window's builder
/// (`WebviewWindowBuilder::data_directory`, which reaches WebView2 as the
/// `userDataFolder` argument): [`elevated_webview_folder`] when this wizard is
/// elevated — the C3 relaunch and a "Run as administrator" by hand alike —
/// and `None` for a limited wizard, which keeps Tauri's folder exactly as
/// before. Through the API, not the `WEBVIEW2_USER_DATA_FOLDER` variable: an
/// elevated WebView2 host ignores every `WEBVIEW2_*` environment override and
/// honours what the app passes through the API ("Develop secure WebView2
/// apps", Microsoft Learn), which is why an elevated wizard's CDP port also
/// comes only from machine policy, never from the environment.
///
/// The folder is made new each time ([`make_elevated_folder`]). When the one
/// under the limited copy's folder cannot be, this process's own is tried, and
/// a window that then cannot be built says so instead of panicking with none.
pub fn webview_data_directory(args: &LaunchArgs) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if !roomlerd::win_identity::process_is_elevated() {
            return None;
        }
        let dir = elevated_webview_folder(args.session_data.as_deref())?;
        if let Err(e) = make_elevated_folder(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "elevated: its WebView2 folder could not be made");
            let own = elevated_webview_folder(None)?;
            if own != dir {
                if let Err(e) = make_elevated_folder(&own) {
                    tracing::warn!(dir = %own.display(), error = %e, "elevated: nor this one");
                }
                return Some(own);
            }
        }
        Some(dir)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        None
    }
}

/// The elevated copy's first act, before Tauri's single-instance plugin looks
/// for a running copy: wait, bounded, for the limited copy that asked for
/// this start to be gone. While it lives, the plugin would find its lock,
/// hand it this copy's arguments and quit. The limited copy is this same
/// file, so its image name is this process's own — which also tells it apart
/// from a process that reused its id. Under over-the-shoulder credentials the
/// limited copy is another account's process; the wait then reads the
/// process list instead of holding a handle
/// (`wizard_shared::elevation::wait_for_exit`).
///
/// A copy still there past the bound is ended, since it holds the lock and would
/// otherwise swallow this start: the person approved a prompt and would get
/// nothing, with nothing saying why. If it cannot be ended (another account's
/// process usually refuses), the person is told what to do and this copy exits.
pub fn wait_for_parent(args: &LaunchArgs) {
    use wizard_shared::elevation::{WaitOutcome, show_fatal, terminate_if_still, wait_for_exit};
    let Some(pid) = args.after_elevation else {
        return;
    };
    let image = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let started = std::time::Instant::now();
    let mut outcome = wait_for_exit(pid, &image, PARENT_EXIT_WAIT);
    if outcome == WaitOutcome::TimedOut {
        let ended = terminate_if_still(pid, &image);
        tracing::warn!(
            pid,
            ended,
            "elevated start: the wizard that asked for it is still running"
        );
        outcome = wait_for_exit(pid, &image, PARENT_END_WAIT);
    }
    tracing::info!(
        pid,
        ?outcome,
        waited_ms = started.elapsed().as_millis() as u64,
        "elevated start: waited for the wizard that asked for it"
    );
    if outcome == WaitOutcome::TimedOut {
        show_fatal(
            "The Roomler Setup window you started this from has not closed. Close it, \
             then choose Restart as administrator again from a new one.",
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relaunch_arguments_parse_back() {
        let sent = relaunch_args(
            Role::DaemonSystem,
            Some("https://self-hosted.example:8443"),
            "https://roomler.ai",
            Some(r"C:\Users\Two Words\AppData\Local"),
            4242,
        );
        assert_eq!(
            sent,
            [
                "--role",
                "daemon-system",
                "--server",
                "https://self-hosted.example:8443",
                "--session-data",
                r"C:\Users\Two Words\AppData\Local",
                "--after-elevation",
                "4242"
            ]
        );
        assert_eq!(
            LaunchArgs::parse(sent),
            LaunchArgs {
                role: Some(Role::DaemonSystem),
                server: Some("https://self-hosted.example:8443".to_string()),
                session_data: Some(r"C:\Users\Two Words\AppData\Local".to_string()),
                after_elevation: Some(4242),
            }
        );
    }

    /// An unfit folder is not sent, and the relaunch goes on without it.
    #[test]
    fn the_session_folder_is_sent_only_when_it_is_fit() {
        for folder in [None, Some(r"\\share\home\me"), Some("relative")] {
            let sent = relaunch_args(Role::DaemonMachine, None, "https://roomler.ai", folder, 7);
            assert_eq!(
                sent,
                ["--role", "daemon-machine", "--after-elevation", "7"],
                "{folder:?}"
            );
        }
    }

    /// The default server is not sent (the elevated copy has it anyway), in
    /// any spelling of it; an unfit one is not sent either.
    #[test]
    fn the_server_is_sent_only_when_it_is_not_the_default() {
        for server in [
            Some("https://roomler.ai"),
            Some("https://roomler.ai/"),
            Some("HTTPS://ROOMLER.AI"),
            None,
            Some("not a url"),
            Some("https://bad\"quote.example"),
        ] {
            let sent = relaunch_args(Role::DaemonMachine, server, "https://roomler.ai", None, 7);
            assert_eq!(
                sent,
                ["--role", "daemon-machine", "--after-elevation", "7"],
                "{server:?}"
            );
        }
    }

    #[test]
    fn unknown_arguments_are_ignored_and_bad_values_dropped() {
        let parsed = LaunchArgs::parse([
            "--verbose",
            "positional",
            "--role",
            "role-from-the-future",
            "--after-elevation",
            "not-a-pid",
            "--server",
            "ftp://wrong.example",
            "--session-data",
            r"\\share\home\me",
            "--unknown=1",
        ]);
        assert_eq!(parsed, LaunchArgs::default());
        assert_eq!(
            LaunchArgs::parse(Vec::<String>::new()),
            LaunchArgs::default()
        );
        assert_eq!(
            LaunchArgs::parse(["--after-elevation", "0"]).after_elevation,
            None,
            "pid 0 is the idle process, never a wizard"
        );
    }

    /// `--flag=value` works, the last of a repeated flag wins, and a flag is
    /// never taken as another flag's value.
    #[test]
    fn the_forms_of_a_flag() {
        let parsed = LaunchArgs::parse([
            "--role=daemon-user",
            "--role",
            "tunnel-client",
            "--after-elevation=99",
        ]);
        assert_eq!(parsed.role, Some(Role::TunnelClient));
        assert_eq!(parsed.after_elevation, Some(99));
        let parsed = LaunchArgs::parse(["--role", "--server", "https://a.example"]);
        assert_eq!(parsed.role, None);
        assert_eq!(parsed.server.as_deref(), Some("https://a.example"));
        assert_eq!(LaunchArgs::parse(["--role"]).role, None, "no value at all");
    }

    #[test]
    fn a_fit_server() {
        for ok in [
            "https://roomler.ai",
            "http://localhost:5001",
            "https://self-hosted.example:8443/base?x=1#y",
            "HTTPS://UPPER.EXAMPLE",
        ] {
            assert_eq!(safe_server(ok), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "https://",
            "http://",
            "roomler.ai",
            "ftp://roomler.ai",
            "https://two words.example",
            "https://tab\t.example",
            "https://quote\".example",
            "https://back\\slash.example",
            "https://new\nline.example",
        ] {
            assert_eq!(safe_server(bad), None, "{bad:?}");
        }
        let long = format!("https://{}", "a".repeat(2048));
        assert_eq!(safe_server(&long), None);
    }

    #[test]
    fn a_fit_folder() {
        for ok in [
            r"C:\Users\me\AppData\Local",
            r"d:\Profiles\Two Words\AppData\Local",
            r"C:\Users\ćirilica\AppData\Local",
            r"C:\Users\first.last (corp)\AppData\Local",
            r"C:\Users\a=b\AppData\Local",
        ] {
            assert_eq!(safe_folder(ok), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "C:",
            r"C:\",
            "relative",
            r"relative\folder",
            r"\Users\me",
            r"\\server\share\me",
            r"\\?\C:\Users\me",
            r"\\.\pipe\me",
            "C:/Users/me",
            r"C:\Users\..\Windows",
            r"C:\Users\me\..",
            r"C:\Users\me\file:stream",
            r#"C:\Users\quo"te"#,
            "C:\\Users\\new\nline",
            r"C:\Users\star*",
            "--role",
        ] {
            assert_eq!(safe_folder(bad), None, "{bad:?}");
        }
        let long = format!(r"C:\{}", "a".repeat(1024));
        assert_eq!(safe_folder(&long), None);
    }

    /// The elevated folder sits under Tauri's own, and the identifier it is
    /// named from is the one `tauri.conf.json` carries.
    /// #3229 — a folder found for the person on a start by hand has to exist and
    /// be fit for the command line and WebView2, like one handed over.
    #[test]
    fn a_found_folder_must_be_there_and_fit() {
        let profile = tempfile::tempdir().unwrap();
        let profile_path = profile.path().to_str().unwrap();
        assert_eq!(local_app_data_under(profile_path), None, "no AppData yet");
        let local = profile.path().join("AppData").join("Local");
        std::fs::create_dir_all(&local).unwrap();
        #[cfg(target_os = "windows")]
        assert_eq!(
            local_app_data_under(profile_path).as_deref(),
            local.to_str()
        );
        for unfit in [r"\\host\share\Users\a", r"relative\a", r"C:\Users\..\a", ""] {
            assert_eq!(local_app_data_under(unfit), None, "{unfit}");
        }
    }

    /// #3229 — a start that cannot open a window says so before anything tries.
    #[test]
    fn main_refuses_a_start_with_no_folder_before_any_window() {
        let calls = crate::spawn_gate::called_names(include_str!("main.rs"));
        let at = |name: &str| calls.iter().position(|c| c == name).expect(name);
        assert!(at("from_env") < at("refuse_if_no_session_folder"));
        assert!(at("wait_for_parent") < at("refuse_if_no_session_folder"));
        assert!(at("refuse_if_no_session_folder") < at("run"));
    }

    #[test]
    fn the_elevated_webview_folder_is_beside_the_limited_one() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], TAURI_IDENTIFIER);
        let leaf = std::path::Path::new(TAURI_IDENTIFIER).join("elevated");
        assert_eq!(
            elevated_webview_folder(None),
            local_app_data().map(|d| d.join(&leaf))
        );
    }

    /// Over-the-shoulder credentials: the folder the session's user can open,
    /// the one the limited copy named, wins over this process's own.
    #[test]
    fn the_limited_copys_folder_roots_the_elevated_one() {
        let leaf = std::path::Path::new(TAURI_IDENTIFIER).join("elevated");
        let session = std::env::temp_dir();
        assert_eq!(
            elevated_webview_folder(session.to_str()),
            Some(session.join(&leaf))
        );
    }

    /// What an elevated run refuses from its environment, in any spelling, and
    /// what it keeps: a corporate proxy is how the download gets out.
    #[test]
    fn the_overrides_an_elevated_run_refuses() {
        use std::ffi::OsStr;
        for name in [
            "ROOMLER_INSTALLER_PROXY_BASE",
            "roomler_installer_proxy_base",
            "ROOMLER_TUNNEL_CLI_PROXY_BASE",
            "ROOMLER_TUNNEL_WIZARD_PROXY_BASE",
            "ROOMLER_TUNNEL_WIZARD_INSTALL_ROOT",
            "ROOMLER_TUNNEL_ENROLL_BASE_URL",
            "ROOMLERD_ENCODER",
            "WEBVIEW2_USER_DATA_FOLDER",
            "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
            "TAURI_WEBVIEW_AUTOMATION",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ] {
            assert!(distrusted_variable(OsStr::new(name)), "{name}");
        }
        for name in [
            "PATH",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "NO_PROXY",
            "RUST_LOG",
            "TEMP",
            "USERPROFILE",
            "XROOMLER",
        ] {
            assert!(!distrusted_variable(OsStr::new(name)), "{name}");
        }
    }

    /// Every override the wizard's own code reads is on that list, so a new
    /// one cannot slip past it by being spelled differently.
    #[test]
    fn every_override_the_wizard_reads_is_refused_when_elevated() {
        for source in [
            include_str!("proxy.rs"),
            include_str!("orchestrator_tunnel.rs"),
            include_str!("../../../crates/roomler-setup-core/src/tunnel_enroll.rs"),
        ] {
            let mut seen = 0;
            for piece in source.split("env::var(\"").skip(1) {
                let name = piece.split('"').next().unwrap();
                if name == "HOSTNAME" || name == "COMPUTERNAME" {
                    continue;
                }
                seen += 1;
                assert!(distrusted_variable(std::ffi::OsStr::new(name)), "{name}");
            }
            assert!(seen > 0, "this source reads none any more: drop it here");
        }
    }

    /// The same folder in another spelling is the same person; another folder
    /// is another account's; and what cannot be compared is not a difference.
    #[test]
    fn telling_another_account_from_the_session_folder() {
        let own = std::path::Path::new(r"C:\Users\me\AppData\Local");
        for same in [
            r"C:\Users\me\AppData\Local",
            r"c:\users\ME\appdata\local",
            r"C:\Users\me\AppData\Local\",
        ] {
            assert!(!names_another_profile(Some(same), Some(own)), "{same}");
        }
        assert!(names_another_profile(
            Some(r"C:\Users\std\AppData\Local"),
            Some(own)
        ));
        assert!(!names_another_profile(None, Some(own)));
        assert!(!names_another_profile(
            Some(r"C:\Users\std\AppData\Local"),
            None
        ));
    }

    /// The folder comes out new and empty, with nothing left by whoever had it
    /// before; a parent that is missing is made.
    #[test]
    fn the_elevated_folder_is_made_new() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(TAURI_IDENTIFIER).join("elevated");
        make_elevated_folder(&dir).unwrap();
        assert!(dir.is_dir());
        std::fs::create_dir_all(dir.join("EBWebView/Default/Extensions")).unwrap();
        std::fs::write(dir.join("EBWebView/Default/Preferences"), "left").unwrap();
        make_elevated_folder(&dir).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// A link at either of the wizard's two names is removed, never followed:
    /// what it points at is untouched.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_junction_in_the_elevated_folders_path_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        let keep = elsewhere.join("elevated").join("keep.txt");
        std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
        std::fs::write(&keep, "keep").unwrap();
        let junction = |link: &std::path::Path, target: &std::path::Path| {
            let made = std::process::Command::new(roomlerd::appdirs::system32_tool("cmd.exe"))
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(made.status.success(), "mklink: {made:?}");
        };
        // The wizard's folder is a junction to a folder holding an "elevated".
        let owned = root.path().join("first").join(TAURI_IDENTIFIER);
        std::fs::create_dir_all(owned.parent().unwrap()).unwrap();
        junction(&owned, &elsewhere);
        make_elevated_folder(&owned.join("elevated")).unwrap();
        assert_eq!(std::fs::read_to_string(&keep).unwrap(), "keep");
        assert!(!owned.symlink_metadata().unwrap().file_type().is_symlink());
        // "elevated" itself is the junction.
        let parent = root.path().join("second").join(TAURI_IDENTIFIER);
        std::fs::create_dir_all(&parent).unwrap();
        let dir = parent.join("elevated");
        junction(&dir, &elsewhere.join("elevated"));
        make_elevated_folder(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&keep).unwrap(), "keep");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// A named folder that is not there changes nothing.
    #[test]
    fn a_missing_session_folder_falls_back_to_this_process() {
        let missing = std::env::temp_dir().join("roomler-setup-no-such-folder");
        assert!(!missing.exists());
        assert_eq!(
            elevated_webview_folder(missing.to_str()),
            elevated_webview_folder(None)
        );
    }

    /// The main window is ours to create: with `"create": false` Tauri leaves
    /// it to `main.rs`'s setup hook, which is the only place the elevated
    /// folder can be handed to WebView2. Should the flag go, Tauri would make
    /// the window first, from the config, without that folder.
    #[test]
    fn the_main_window_is_created_by_the_setup_hook() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let windows = conf["app"]["windows"].as_array().expect("the windows list");
        assert_eq!(windows.len(), 1, "one window");
        assert_eq!(windows[0]["label"], MAIN_WINDOW);
        assert_eq!(windows[0]["create"], false);
        let calls = crate::spawn_gate::called_names(include_str!("main.rs"));
        for name in [
            "from_config",
            "webview_data_directory",
            "data_directory",
            "build",
        ] {
            assert!(
                calls.iter().any(|c| c == name),
                "main.rs no longer calls {name}"
            );
        }
    }

    /// The window is handed the folder BEFORE it is built: a `data_directory`
    /// after `build` would not be a call the builder has.
    #[test]
    fn the_folder_reaches_the_builder_before_the_window_is_built() {
        let calls = crate::spawn_gate::called_names(include_str!("main.rs"));
        let at = |name: &str| calls.iter().position(|c| c == name).expect(name);
        assert!(at("from_config") < at("data_directory"));
        assert!(at("data_directory") < at("build"));
    }

    /// No DLL comes from the folder the EXE sits in, and no variable of its
    /// user's is taken: the search is restricted first thing in `main`, the
    /// environment made clean second, each before anything can load or read.
    /// `build.rs` holds the linker's half; what proves that one is the flag read
    /// from the built EXE (`scripts/signing/pe-dependent-load-flags.ps1`, run by
    /// CI's Windows job and by the release lane), and what proves the pair is
    /// the plant test in the field log.
    #[test]
    fn main_starts_clean_before_anything_else() {
        let calls = crate::spawn_gate::called_names(include_str!("main.rs"));
        assert_eq!(calls[..2], ["restrict_dll_search", "start_clean"]);
    }

    /// A relaunch comes to the front. Windows otherwise leaves it behind
    /// whatever took the foreground as the limited copy exited, which is the
    /// browser that offered the download.
    #[test]
    fn a_relaunch_takes_the_foreground() {
        let calls = crate::spawn_gate::called_names(include_str!("main.rs"));
        let build = calls.iter().position(|c| c == "build").expect("build");
        // The single-instance callback focuses too, earlier in the file: the
        // last one is the setup hook's, on the window it has just built.
        let focus = calls.iter().rposition(|c| c == "set_focus").expect("focus");
        assert!(build < focus);
    }

    /// FR-129 C, review finding 5: the page gets the two IPC groups it uses and
    /// nothing else. A native file picker or the shell's open, granted to a
    /// script a limited user can reach, is a way into an elevated process.
    #[test]
    fn the_page_has_only_the_grants_it_uses() {
        let caps: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        assert_eq!(
            caps["permissions"],
            serde_json::json!([
                "core:default",
                "core:event:allow-listen",
                "core:event:allow-unlisten"
            ])
        );
        let page = include_str!("front/app.js");
        let plugins: std::collections::BTreeSet<&str> = page
            .split("__TAURI__.")
            .skip(1)
            .map(|rest| rest.split(|c: char| !c.is_alphanumeric()).next().unwrap())
            .collect();
        assert_eq!(
            plugins,
            std::collections::BTreeSet::from(["core", "event"]),
            "a new plugin in the page needs its grant, and this test updating"
        );
    }

    /// A limited process never names a folder of its own, whatever its command
    /// line says: Tauri's default stays exactly what it was. (This test runs
    /// limited unless the shell that ran it was elevated; then it checks the
    /// elevated answer.)
    #[test]
    fn only_an_elevated_wizard_names_its_own_folder() {
        let session = std::env::temp_dir();
        let args = LaunchArgs {
            session_data: session.to_str().map(str::to_string),
            ..LaunchArgs::default()
        };
        #[cfg(target_os = "windows")]
        {
            let elevated = roomlerd::win_identity::process_is_elevated();
            assert_eq!(webview_data_directory(&args).is_some(), elevated);
            if elevated {
                assert_eq!(
                    webview_data_directory(&args),
                    elevated_webview_folder(session.to_str())
                );
            }
        }
        #[cfg(not(target_os = "windows"))]
        assert_eq!(webview_data_directory(&args), None);
    }
}
