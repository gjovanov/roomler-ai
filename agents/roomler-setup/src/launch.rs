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

/// What the wizard was started with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchArgs {
    pub role: Option<Role>,
    pub server: Option<String>,
    pub session_data: Option<String>,
    pub after_elevation: Option<u32>,
}

impl LaunchArgs {
    /// This process's own arguments.
    pub fn from_env() -> LaunchArgs {
        LaunchArgs::parse(std::env::args().skip(1))
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

/// The WebView2 user-data folder of an ELEVATED wizard: beside the limited
/// one's (`%LOCALAPPDATA%\ai.roomler.setup`, where Tauri puts it), never in
/// it. WebView2 shares one browser process among every host that names the
/// same folder ("Manage user data folders", Microsoft Learn), and the limited
/// copy's can still be running for a moment after that copy has exited; an
/// elevated host joining a limited browser process is the cross-integrity
/// failure.
///
/// Under `session_data`, the limited copy's `%LOCALAPPDATA%`, when the
/// relaunch carried it and the folder is there, because WebView2's browser
/// process de-elevates to the user of the SESSION, whoever the host runs as.
/// Under over-the-shoulder credentials the host is the administrator who typed
/// the password and the browser is the standard user, who cannot open the
/// administrator's profile: the browser cannot make its data directory and the
/// wizard is a white window that goes away after about a minute. The limited
/// copy's folder is one that user can write, and for an administrator approving
/// their own prompt it is the same folder as this process's.
///
/// Without it, this process's own: "Run as administrator" by hand has no limited
/// copy to ask, so over-the-shoulder credentials still fail that way there.
/// `None` when there is no local app-data folder to put it in.
pub fn elevated_webview_folder(session_data: Option<&str>) -> Option<std::path::PathBuf> {
    session_data
        .map(std::path::PathBuf::from)
        .filter(|folder| folder.is_dir())
        .or_else(local_app_data)
        .map(|folder| folder.join(TAURI_IDENTIFIER).join("elevated"))
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
pub fn webview_data_directory(args: &LaunchArgs) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if !roomlerd::win_identity::process_is_elevated() {
            return None;
        }
        elevated_webview_folder(args.session_data.as_deref())
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
pub fn wait_for_parent(args: &LaunchArgs) {
    let Some(pid) = args.after_elevation else {
        return;
    };
    let image = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let started = std::time::Instant::now();
    let outcome = wizard_shared::elevation::wait_for_exit(pid, &image, PARENT_EXIT_WAIT);
    tracing::info!(
        pid,
        ?outcome,
        waited_ms = started.elapsed().as_millis() as u64,
        "elevated start: waited for the wizard that asked for it"
    );
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
        let main = include_str!("main.rs");
        assert!(main.contains("WebviewWindowBuilder::from_config"));
        assert!(main.contains("webview_data_directory(&launch)"));
    }

    /// No DLL comes from the folder the EXE sits in: the search is restricted
    /// before anything else in `main`, and the linker does the same for the
    /// imports the loader resolves before `main` runs. The plant test in the
    /// field log is what proves the pair; this keeps either half from going.
    #[test]
    fn dlls_come_from_system32_alone() {
        let body = include_str!("main.rs")
            .split("fn main() {")
            .nth(1)
            .expect("main");
        let first = body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("//"))
            .unwrap();
        assert_eq!(first, "wizard_shared::elevation::restrict_dll_search();");
        let build = include_str!("../build.rs");
        assert!(build.contains("rustc-link-arg-bin=roomler-setup=/DEPENDENTLOADFLAG:0x800"));
    }

    /// A relaunch comes to the front. Windows otherwise leaves it behind
    /// whatever took the foreground as the limited copy exited, which is the
    /// browser that offered the download.
    #[test]
    fn a_relaunch_takes_the_foreground() {
        let main = include_str!("main.rs");
        let setup = main.split(".setup(").nth(1).expect("the setup hook");
        let hook = setup.split(".on_window_event(").next().unwrap();
        assert!(hook.contains("launch.after_elevation.is_some()"));
        assert!(hook.contains("set_focus()"));
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
