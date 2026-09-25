// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-84 D6 — how the companion starts: what its own command line asks for,
//! whether this person has seen the Welcome tour, and whether a login start
//! should stay in the tray, open the tour, or leave again at once.
//!
//! Before D6 only the SECOND instance read argv (the single-instance
//! callback in `main.rs`): the first process ignored `--view=` entirely, and
//! nothing could tell a login start from a person opening the app.

/// What the command line asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchArgs {
    /// `--view=<name>`: open on this view. Whitelisted to ASCII alphanumerics
    /// (it is routed into the page), else ignored.
    pub view: Option<String>,
    /// `--autostart`: a login start.
    pub autostart: bool,
    /// `--first-run`: the daemon's one-shot post-install launch.
    pub first_run: bool,
}

/// Parse the companion's own arguments (`args[0]` is the program).
pub fn parse_launch_args<S: AsRef<str>>(_args: &[S]) -> LaunchArgs {
    // STUB (RED stage).
    LaunchArgs::default()
}

/// What the FIRST process does at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupAction {
    /// Leave at once: a login start for a person who switched it off.
    Exit,
    /// Stay in the tray, window hidden (today's behaviour).
    Tray,
    /// Show the window on this view.
    Show(String),
}

/// Decide the first process's start. `first_run_done` / `opted_out` come
/// from the per-user desktop state.
pub fn decide_startup(
    _args: &LaunchArgs,
    _first_run_done: bool,
    _opted_out: bool,
) -> StartupAction {
    // STUB (RED stage).
    StartupAction::Tray
}

/// What a SECOND launch, forwarded to the running instance, does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondLaunch {
    /// Nothing at all: a login start must not pop the window of a companion
    /// that is already up.
    Ignore,
    /// Show and focus the window where it is (a double-click).
    Show,
    /// Show the window on this view.
    ShowView(String),
}

/// Decide a forwarded launch.
pub fn decide_second_instance(_args: &LaunchArgs, _first_run_done: bool) -> SecondLaunch {
    // STUB (RED stage).
    SecondLaunch::Show
}

/// The first port in `start..start+span` that is not in `exclude` and that
/// `can_bind` accepts. Userspace mode's SOCKS front needs a loopback port
/// that is free NOW and not claimed by a declared route that is merely down
/// at the moment — 1080 is exactly the port such a route tends to hold.
pub fn pick_free_port(
    _start: u16,
    _span: u16,
    _exclude: &[u16],
    _can_bind: impl Fn(u16) -> bool,
) -> Option<u16> {
    // STUB (RED stage).
    Some(1080)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        std::iter::once("roomler-desktop")
            .chain(v.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn argv_is_parsed_for_the_first_process_too() {
        assert_eq!(parse_launch_args(&args(&[])), LaunchArgs::default());
        assert_eq!(
            parse_launch_args(&args(&["--autostart"])),
            LaunchArgs {
                autostart: true,
                ..LaunchArgs::default()
            }
        );
        assert_eq!(
            parse_launch_args(&args(&["--first-run"])),
            LaunchArgs {
                first_run: true,
                ..LaunchArgs::default()
            }
        );
        assert_eq!(
            parse_launch_args(&args(&["--view=devices", "--autostart"])),
            LaunchArgs {
                view: Some("devices".into()),
                autostart: true,
                ..LaunchArgs::default()
            }
        );
        // The view goes into the page: anything but ASCII alphanumerics is
        // dropped, not escaped.
        for bad in [
            "--view=",
            "--view=a'b",
            "--view=../x",
            "--view=welcome;alert(1)",
            "--view=wélcome",
        ] {
            assert_eq!(parse_launch_args(&args(&[bad])).view, None, "{bad}");
        }
        // The program name is never an argument, whatever it is.
        assert_eq!(
            parse_launch_args(&["--first-run".to_string()]),
            LaunchArgs::default()
        );
    }

    #[test]
    fn startup_table() {
        use StartupAction::*;
        let a = |v: &[&str]| parse_launch_args(&args(v));
        let welcome = || Show("welcome".to_string());
        // (args, first_run_done, opted_out) → action
        let table: Vec<(Vec<&str>, bool, bool, StartupAction)> = vec![
            // A person who never finished the tour sees it, however launched.
            (vec![], false, false, welcome()),
            (vec!["--autostart"], false, false, welcome()),
            (vec!["--autostart"], false, true, welcome()),
            (vec!["--first-run"], false, false, welcome()),
            // After the tour: login starts stay in the tray…
            (vec!["--autostart"], true, false, Tray),
            // …or leave at once for a person who switched login start off.
            (vec!["--autostart"], true, true, Exit),
            // A plain start (a daemon respawn, a double-click) keeps today's
            // tray-only behaviour.
            (vec![], true, false, Tray),
            (vec![], true, true, Tray),
            // The installer's launch on an account that already did the tour
            // still opens the window — something was just installed.
            (
                vec!["--first-run"],
                true,
                false,
                Show("overview".to_string()),
            ),
            // An explicit view always wins, even over the tour.
            (
                vec!["--view=devices"],
                false,
                false,
                Show("devices".to_string()),
            ),
            (
                vec!["--view=settings", "--autostart"],
                true,
                true,
                Show("settings".to_string()),
            ),
        ];
        for (v, done, opted_out, want) in table {
            assert_eq!(
                decide_startup(&a(&v), done, opted_out),
                want,
                "{v:?} done={done} opted_out={opted_out}"
            );
        }
    }

    #[test]
    fn a_forwarded_launch_never_pops_a_login_start() {
        use SecondLaunch::*;
        let a = |v: &[&str]| parse_launch_args(&args(v));
        assert_eq!(decide_second_instance(&a(&["--autostart"]), true), Ignore);
        assert_eq!(decide_second_instance(&a(&["--autostart"]), false), Ignore);
        assert_eq!(
            decide_second_instance(&a(&["--view=devices"]), true),
            ShowView("devices".into())
        );
        assert_eq!(
            decide_second_instance(&a(&["--first-run"]), false),
            ShowView("welcome".into())
        );
        assert_eq!(
            decide_second_instance(&a(&["--first-run"]), true),
            ShowView("overview".into())
        );
        // A plain second launch (a double-click) shows the window where it is.
        assert_eq!(decide_second_instance(&a(&[]), true), Show);
    }

    #[test]
    fn free_port_skips_excluded_and_taken_ports() {
        // Fake binder: 41080 and 41081 are taken by someone.
        let taken = [41080u16, 41081];
        let can_bind = |p: u16| !taken.contains(&p);
        assert_eq!(pick_free_port(41080, 10, &[], can_bind), Some(41082));
        // 41082 is a declared route's port (down right now, so bindable) —
        // still not ours to take.
        assert_eq!(pick_free_port(41080, 10, &[41082], can_bind), Some(41083));
        assert_eq!(
            pick_free_port(41080, 2, &[], can_bind),
            None,
            "span exhausted"
        );
        // Never wraps past 65535.
        assert_eq!(pick_free_port(65535, 10, &[], |_| false), None);
        assert_eq!(pick_free_port(65535, 10, &[], |_| true), Some(65535));
    }

    #[test]
    fn free_port_really_skips_a_bound_port() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let got = pick_free_port(port, 50, &[], |p| {
            std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
        })
        .expect("a free port in the span");
        assert_ne!(got, port, "the held port must be skipped");
        assert!(
            got > port && u32::from(got) < u32::from(port) + 50,
            "{got} is not inside the requested span starting at {port}"
        );
    }
}
