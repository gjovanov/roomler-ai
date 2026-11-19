// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3932 — the options every `.deb` install of ours hands dpkg: the daemon's
//! own package, the desktop companion's, and the tunnel CLI's `self-update`.
//! One definition, here, because the CLI shares nothing else with the daemon.
//!
//! When the host's copy of a package's configuration file was edited (or
//! deleted) and the new package carries a different version of it, dpkg asks
//! which one to keep, and reads the answer from stdin. `apt-get -y` does not
//! answer that question, and `DEBIAN_FRONTEND=noninteractive` does not reach
//! it (it quiets debconf, not dpkg). With nothing on stdin dpkg stops at the
//! question: "end of file on stdin at conffile prompt", exit 1 (`apt-get`:
//! 100), and the package is left unpacked, not configured (`iU`), with the new
//! file beside the old one as `.dpkg-new`. Measured on a real dpkg 1.21.23 and
//! apt 2.6.2 (#3926, #3932).

/// dpkg's options for every package install we run.
///
/// `--force-confold`, in dpkg's own words "Always use the old config files,
/// don't prompt": a configuration file the host edited or deleted stays as it
/// is, the package's version lands beside it as `<file>.dpkg-dist`, and dpkg
/// says so (` ==> Using current old file as you requested.`). A file nobody
/// edited still takes the package's version, silently. dpkg asks nothing, so
/// it reads nothing from stdin.
///
/// ⚠️ Not `--force-confdef`: for an edited file it adds nothing to this one,
/// and alone it still asks where there is no default. Not `--force-confnew`:
/// that overwrites what a person chose, such as the companion's login start
/// turned off for every user of a machine.
pub const OPTIONS: [&str; 1] = ["--force-confold"];

/// [`OPTIONS`] for `dpkg` itself, each as it is. They go before the action and
/// the package: dpkg stops reading options at the first argument that is not
/// one.
pub fn dpkg_args() -> impl Iterator<Item = String> {
    OPTIONS.into_iter().map(String::from)
}

/// [`OPTIONS`] as `apt-get` passes them on to dpkg: `-o
/// Dpkg::Options::=<option>`, each.
pub fn apt_get_args() -> impl Iterator<Item = String> {
    OPTIONS
        .into_iter()
        .flat_map(|o| ["-o".to_string(), format!("Dpkg::Options::={o}")])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set the real-dpkg cell proved (#3926, #3932). Changing it means
    /// running that cell again.
    #[test]
    fn the_option_set_is_the_one_measured() {
        assert_eq!(OPTIONS, ["--force-confold"]);
    }

    #[test]
    fn dpkg_takes_each_option_as_it_is() {
        assert_eq!(dpkg_args().collect::<Vec<_>>(), ["--force-confold"]);
    }

    #[test]
    fn apt_get_passes_each_option_on_to_dpkg() {
        assert_eq!(
            apt_get_args().collect::<Vec<_>>(),
            ["-o", "Dpkg::Options::=--force-confold"]
        );
    }
}
