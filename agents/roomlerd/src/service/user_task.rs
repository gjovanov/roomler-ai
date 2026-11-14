// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3801 — the per-user Scheduled Task: one per account, and the one rule
//! every path follows to find it.
//!
//! Until #3801 every Windows per-user install registered its autostart as
//! `\Roomler`, in the one task library a machine has. Two accounts with a
//! per-user install each shared that name, and only one could hold it: the
//! second registration was refused, so that device never started at sign-in,
//! and the per-user arm of `cleanup-legacy-install`, run for one account,
//! deleted the task by name, whichever account's it was, and that device went
//! offline.
//!
//! Each account now registers a task of its own, `Roomler-<SID>`, named for the
//! SID of the account its process runs as ([`autostart`]): the form Windows'
//! own per-user tasks take. A task under a name any account may hold (the names
//! before #3801, [`LEGACY`], and the launcher check's, [`SHARED_CHECK`]) is an
//! account's own only when its principal says so ([`is_own`]). A definition the
//! process may not read, one with no account it can tell, or none at all, is
//! not its own, and nothing acts on it.
//!
//! | Path | What it acts on |
//! |---|---|
//! | `service install` (the MSI's `RegisterAutostart`, the tray, by hand) | registers [`autostart`], then deletes a legacy task that is its own, never before the new one is registered ([`register`]) |
//! | `service start` (the wizard, a restart by the tray or `roomler restart`, the updater's watcher after a failed install, the Start menu entry) and the hard stop's `schtasks /End` | its own task, else a legacy one that is its own ([`own_autostart`]) |
//! | `service status` | the same, read without acting ([`status`]) |
//! | `service uninstall` (the MSI's `UnregisterAutostart`) | its own tasks, and the legacy ones that are its own ([`unregister`]) |
//! | the per-user arm of `cleanup-legacy-install` | the tasks of the account signed in to the step's session (`install_cleanup.rs`) |
//!
//! An install from before #3801 keeps its `Roomler` until its own account's
//! update: the old product's `UnregisterAutostart` deletes it, as every update
//! did, and the new `RegisterAutostart` registers `Roomler-<SID>`. Until then it
//! starts that account's daemon, and no other account's code acts on it.
//!
//! The decisions are pure over [`Library`], the task library's seam: the real
//! one runs `schtasks.exe` from System32 (`service.rs`), the tests' is a list.

use super::AutostartStatus;
use anyhow::Result;

/// The name every per-user install registered before #3801, whichever account
/// it ran as.
pub(crate) const SHARED: &str = "Roomler";

/// The name before the P3d rename, under the same rule as [`SHARED`].
// RETIRED-NAME-ANCHOR: a host may still carry the pre-P3d task; without this the paths lose it and the host runs two daemons. docs/fr/FR-21
pub(crate) const RETIRED: &str = "RoomlerAgent";

/// The launcher check's name before #3801: a throwaway task `service install`
/// registers for seconds where an AppLocker policy is in force (#3292).
pub(crate) const SHARED_CHECK: &str = "Roomler launcher check";

/// The autostart task of the account `sid` (its SID, `S-1-5-21-…`).
pub(crate) fn autostart(sid: &str) -> String {
    // RED (#3801): today's one name, whichever account registers it.
    let _ = sid;
    SHARED.to_string()
}

/// The launcher check of the account `sid`.
pub(crate) fn launcher_check(sid: &str) -> String {
    // RED (#3801): today's one name, whichever account registers it.
    let _ = sid;
    SHARED_CHECK.to_string()
}

/// The machine's task library as the process asking sees it: the seam between
/// the decisions here and Task Scheduler.
pub(crate) trait Library {
    /// Is a task of this name there, for this process? A standard account sees
    /// its own tasks. `true` when the question could not be asked, so a caller
    /// that takes absence as done is never fooled into skipping work.
    fn exists(&self, name: &str) -> bool;
    /// What `service status` says of one task.
    fn status(&self, name: &str) -> Result<AutostartStatus>;
    /// Register `xml` as `name`, replacing a task of that name.
    fn create(&mut self, name: &str, xml: &str) -> Result<()>;
    /// Delete `name`. A task this process cannot find is gone already.
    fn delete(&mut self, name: &str) -> Result<()>;
}

/// The task that starts the daemon of the account `sid`: its own, else a
/// legacy one whose principal is `sid`. What `service start` runs, and what the
/// hard stop ends.
pub(crate) fn own_autostart(lib: &impl Library, sid: &str) -> Option<String> {
    // RED (#3801): today's two names, by name alone.
    let _ = sid;
    [SHARED, RETIRED]
        .into_iter()
        .find(|name| lib.exists(name))
        .map(str::to_string)
}

/// `service status` for the account `sid`: its own task, else a legacy one
/// whose principal is `sid`.
pub(crate) fn status(lib: &impl Library, sid: &str) -> Result<AutostartStatus> {
    // RED (#3801): today's two names, by name alone.
    let _ = sid;
    match lib.status(SHARED)? {
        AutostartStatus::NotInstalled => lib.status(RETIRED),
        other => Ok(other),
    }
}

/// `service install` for the account `sid`: register its task as `xml`, then
/// delete a legacy task of its own, best effort, once the new one is there.
pub(crate) fn register(lib: &mut impl Library, sid: &str, xml: &str) -> Result<()> {
    // RED (#3801): today's one name, and the retired one deleted by name.
    lib.create(&autostart(sid), xml)?;
    let _ = lib.delete(RETIRED);
    Ok(())
}

/// `service uninstall` for the account `sid`: its task and launcher check, and
/// the legacy ones whose principal is `sid`.
pub(crate) fn unregister(lib: &mut impl Library, sid: &str) -> Result<()> {
    // RED (#3801): today's names, deleted by name alone.
    lib.delete(SHARED)?;
    lib.delete(RETIRED)?;
    let _ = lib.delete(&launcher_check(sid));
    Ok(())
}

/// A task library in memory, for the tests here and the per-user clean-up's
/// (`install_cleanup.rs`).
#[cfg(test)]
pub(crate) mod fake {
    use super::AutostartStatus;
    use anyhow::{Result, bail};

    /// One task of the library.
    #[derive(Debug, Clone)]
    pub(crate) struct Task {
        pub(crate) name: String,
        /// The account it runs as. `None`: its definition names no account
        /// anyone can tell, or does not parse.
        pub(crate) principal: Option<String>,
        /// Every account sees it listed, while only its own account and an
        /// elevated process may read or change it. By default only those two
        /// see it at all, as with Task Scheduler's default security on a task
        /// a person registers.
        pub(crate) seen_by_all: bool,
    }

    impl Task {
        fn runs_as(&self, sid: &str) -> bool {
            self.principal
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(sid))
        }
    }

    /// The library as the process of `caller` sees it.
    #[derive(Debug, Default)]
    pub(crate) struct Library {
        pub(crate) caller: String,
        pub(crate) elevated: bool,
        pub(crate) tasks: Vec<Task>,
        /// Every create and delete asked of the library, refused ones too.
        pub(crate) asked: Vec<(&'static str, String)>,
        /// Names whose registration the library refuses.
        pub(crate) refuse: Vec<String>,
    }

    impl Library {
        pub(crate) fn new(caller: &str, elevated: bool) -> Self {
            Self {
                caller: caller.into(),
                elevated,
                ..Self::default()
            }
        }

        /// With a task `name` that runs as `principal`.
        pub(crate) fn with(self, name: &str, principal: Option<&str>) -> Self {
            self.with_task(name, principal, false)
        }

        /// With a task every account sees listed, whose definition only its
        /// own account and an elevated process may read.
        pub(crate) fn with_seen_by_all(self, name: &str, principal: Option<&str>) -> Self {
            self.with_task(name, principal, true)
        }

        fn with_task(mut self, name: &str, principal: Option<&str>, seen_by_all: bool) -> Self {
            self.tasks.push(Task {
                name: name.into(),
                principal: principal.map(str::to_string),
                seen_by_all,
            });
            self
        }

        /// Now the process of `caller`; what the last one asked is forgotten.
        pub(crate) fn switch(&mut self, caller: &str, elevated: bool) {
            self.caller = caller.into();
            self.elevated = elevated;
            self.asked.clear();
        }

        /// The account the task `name` runs as, `None` when there is no such
        /// task: what the machine holds, whoever looks.
        pub(crate) fn owner(&self, name: &str) -> Option<Option<String>> {
            self.find(name).map(|i| self.tasks[i].principal.clone())
        }

        /// Was a create or a delete of `name` asked?
        pub(crate) fn asked_for(&self, name: &str) -> bool {
            self.asked.iter().any(|(_, n)| n.eq_ignore_ascii_case(name))
        }

        fn find(&self, name: &str) -> Option<usize> {
            self.tasks
                .iter()
                .position(|t| t.name.eq_ignore_ascii_case(name))
        }

        fn may_change(&self, task: &Task) -> bool {
            self.elevated || task.runs_as(&self.caller)
        }

        fn sees(&self, task: &Task) -> bool {
            self.may_change(task) || task.seen_by_all
        }
    }

    impl super::Library for Library {
        fn exists(&self, name: &str) -> bool {
            self.find(name).is_some_and(|i| self.sees(&self.tasks[i]))
        }

        fn status(&self, name: &str) -> Result<AutostartStatus> {
            Ok(if super::Library::exists(self, name) {
                AutostartStatus::Installed
            } else {
                AutostartStatus::NotInstalled
            })
        }

        fn create(&mut self, name: &str, _xml: &str) -> Result<()> {
            self.asked.push(("create", name.into()));
            if self.refuse.iter().any(|r| r.eq_ignore_ascii_case(name)) {
                bail!("ERROR: the task is refused");
            }
            match self.find(name) {
                Some(i) if !self.may_change(&self.tasks[i]) => bail!("ERROR: Access is denied."),
                Some(i) => self.tasks[i].principal = Some(self.caller.clone()),
                None => self.tasks.push(Task {
                    name: name.into(),
                    principal: Some(self.caller.clone()),
                    seen_by_all: false,
                }),
            }
            Ok(())
        }

        fn delete(&mut self, name: &str) -> Result<()> {
            self.asked.push(("delete", name.into()));
            match self.find(name) {
                Some(i) if self.may_change(&self.tasks[i]) => {
                    self.tasks.remove(i);
                    Ok(())
                }
                Some(i) if self.sees(&self.tasks[i]) => bail!("ERROR: Access is denied."),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Library as Fake;
    use super::*;
    use AutostartStatus::{Installed, NotInstalled};

    /// Two made-up accounts of one machine.
    const A: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    const B: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";
    const XML: &str = "<Task/>";

    /// An account's own autostart task, spelled out.
    fn named(sid: &str) -> String {
        format!("Roomler-{sid}")
    }

    /// #3801 — two accounts with a per-user install each register a task each,
    /// named for the account. The second registration neither fails on the
    /// first's task nor replaces it, from a standard or an elevated process,
    /// and each account's start finds its own.
    #[test]
    fn each_account_registers_a_task_of_its_own() {
        for elevated in [false, true] {
            let mut lib = Fake::new(A, false);
            register(&mut lib, A, XML).unwrap();
            lib.switch(B, elevated);
            let registered = register(&mut lib, B, XML);
            assert!(
                registered.is_ok(),
                "elevated {elevated}: {registered:?}, {:?}",
                lib.asked
            );
            for sid in [A, B] {
                assert_eq!(autostart(sid), named(sid));
                assert_eq!(
                    lib.owner(&named(sid)),
                    Some(Some(sid.to_string())),
                    "elevated {elevated}: {:?}",
                    lib.tasks
                );
            }
            assert!(
                !lib.asked_for(&named(A)),
                "B's registration acted on A's task: {:?}",
                lib.asked
            );
            assert_eq!(own_autostart(&lib, B), Some(named(B)));
            lib.switch(A, false);
            assert_eq!(own_autostart(&lib, A), Some(named(A)));
        }
    }

    /// #3801 — the launcher check is each account's own too: an uninstall
    /// deletes its own account's, under its name or the shared one, and never
    /// another account's left under the shared name.
    #[test]
    fn the_launcher_check_is_each_accounts_own() {
        assert_ne!(launcher_check(A), launcher_check(B));
        for sid in [A, B] {
            let check = launcher_check(sid);
            assert!(
                check.starts_with(SHARED_CHECK) && check.ends_with(sid),
                "{check}"
            );
        }
        // B's check, left under the shared name by a registration that was killed.
        let mut lib = Fake::new(A, true).with(SHARED_CHECK, Some(B));
        unregister(&mut lib, A).unwrap();
        assert_eq!(lib.owner(SHARED_CHECK), Some(Some(B.to_string())));
        assert!(!lib.asked_for(SHARED_CHECK), "{:?}", lib.asked);
        // A's own under the shared name goes with A's uninstall.
        let mut lib = Fake::new(A, false).with(SHARED_CHECK, Some(A));
        unregister(&mut lib, A).unwrap();
        assert_eq!(lib.owner(SHARED_CHECK), None, "{:?}", lib.asked);
    }

    /// #3801, the mixed state — A's install is on its own task and B's still on
    /// the shared `Roomler` an older release registered. A's code never acts on
    /// B's task, at a registration, a start, a status or an uninstall, from a
    /// standard or an elevated process; and B's start still finds it.
    #[test]
    fn an_account_on_its_own_task_never_acts_on_anothers_shared_one() {
        for elevated in [false, true] {
            let mut lib = Fake::new(A, elevated).with(SHARED, Some(B));
            register(&mut lib, A, XML).unwrap();
            assert_eq!(own_autostart(&lib, A), Some(named(A)), "{elevated}");
            assert_eq!(status(&lib, A).unwrap(), Installed, "{elevated}");
            unregister(&mut lib, A).unwrap();
            assert_eq!(own_autostart(&lib, A), None, "elevated {elevated}");
            assert_eq!(status(&lib, A).unwrap(), NotInstalled, "{elevated}");
            assert!(
                !lib.asked_for(SHARED),
                "elevated {elevated}: {:?}",
                lib.asked
            );
            assert_eq!(lib.owner(SHARED), Some(Some(B.to_string())));
            lib.switch(B, false);
            assert_eq!(own_autostart(&lib, B), Some(SHARED.to_string()));
            assert_eq!(status(&lib, B).unwrap(), Installed);
        }
    }

    /// #3801 — an install from before #3801 holds `Roomler`, its own. Its own
    /// update registers the account's task first and only then retires the
    /// shared one, and the one from before the P3d rename with it. A
    /// registration that fails keeps the old task, which still starts the
    /// daemon.
    #[test]
    fn an_existing_roomler_task_moves_to_its_accounts_name_on_its_own_update() {
        let mut lib = Fake::new(A, false)
            .with(SHARED, Some(A))
            .with(RETIRED, Some(A));
        lib.refuse.push(named(A));
        assert!(register(&mut lib, A, XML).is_err(), "{:?}", lib.asked);
        assert_eq!(lib.owner(SHARED), Some(Some(A.to_string())));
        assert_eq!(own_autostart(&lib, A), Some(SHARED.to_string()));
        assert_eq!(status(&lib, A).unwrap(), Installed);

        lib.refuse.clear();
        lib.switch(A, false);
        register(&mut lib, A, XML).unwrap();
        assert_eq!(
            lib.asked,
            [
                ("create", named(A)),
                ("delete", SHARED.to_string()),
                ("delete", RETIRED.to_string()),
            ]
        );
        assert_eq!(lib.owner(SHARED), None);
        assert_eq!(lib.owner(RETIRED), None);
        assert_eq!(own_autostart(&lib, A), Some(named(A)));
    }

    /// #3801 — a task under a shared name whose account this process cannot
    /// read is not its own: another account's, listed to every account with
    /// its definition refused, or one whose definition names no account. A
    /// registration, a start, a status and an uninstall leave it alone.
    #[test]
    fn a_shared_task_whose_account_cannot_be_read_is_not_its_own() {
        let cases = [
            (
                "refused",
                Fake::new(A, false).with_seen_by_all(SHARED, Some(B)),
            ),
            (
                "unparsable",
                Fake::new(A, false).with_seen_by_all(SHARED, None),
            ),
            (
                "unparsable, elevated",
                Fake::new(A, true).with(SHARED, None),
            ),
        ];
        for (case, mut lib) in cases {
            assert_eq!(own_autostart(&lib, A), None, "{case}");
            assert_eq!(status(&lib, A).unwrap(), NotInstalled, "{case}");
            register(&mut lib, A, XML).unwrap();
            assert_eq!(own_autostart(&lib, A), Some(named(A)), "{case}");
            unregister(&mut lib, A).unwrap();
            assert!(!lib.asked_for(SHARED), "{case}: {:?}", lib.asked);
            assert!(lib.owner(SHARED).is_some(), "{case}");
        }
    }
}
