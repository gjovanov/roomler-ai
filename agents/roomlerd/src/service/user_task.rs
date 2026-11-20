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
//! | `service install` (the MSI's `RegisterAutostart`, the tray, by hand) | registers [`autostart`] with a security descriptor of the account's own (FR-150), then deletes a legacy task that is its own, never before the new one is registered ([`register`]). An elevated run registers again a task its account cannot replace; a replace refused over a task that is there leaves [`start_task`] |
//! | `service start` (the wizard, a restart by the tray or `roomler restart`, the updater's watcher after a failed install, the Start menu entry) and the hard stop's `schtasks /End` | its own task, else a legacy one that is its own ([`own_autostart`]). With neither, and no daemon holding its stop event, the MSI's `StopOldDaemon` has nothing to stop ([`nothing_to_stop`]) |
//! | `service status` | the same, read without acting ([`status`]) |
//! | `service uninstall` (the MSI's `UnregisterAutostart`) | its own tasks, its start task, and the legacy ones that are its own ([`unregister`]) |
//! | the per-user arm of `cleanup-legacy-install` | the tasks of the account signed in to the step's session, each ended and then deleted, and then that account's daemon still running (`install_cleanup.rs`, #3885) |
//!
//! An install from before #3801 keeps its `Roomler` until its own account's
//! update: the old product's `UnregisterAutostart` deletes it, as every update
//! did, and the new `RegisterAutostart` registers `Roomler-<SID>`. Until then it
//! starts that account's daemon, and no other account's code acts on it.
//!
//! FR-150 P1a (#3507) — a task registered from an elevated token was owned by
//! Administrators and only readable by its account, so every self-update's
//! registration, which runs at medium, was refused (`Access is denied`) and the
//! daemon stayed down until the next sign-in. Task Scheduler applies a
//! descriptor named in the definition when it creates a task, and only then
//! (#4128's P0, C3 and C5), so each definition now names the account as the
//! owner with full access: a new task stays its account's to replace,
//! whichever token registers it. A task an earlier elevated registration left
//! keeps its descriptor through a replace, so an elevated run deletes it and
//! registers it again; a medium run, refused, still brings the daemon back
//! through [`start_task`], because a medium process may run a task it cannot
//! replace (C2).
//!
//! The decisions are pure over [`Library`], the task library's seam: the real
//! one runs `schtasks.exe` from System32 (`service.rs`), the tests' is a list.

use super::AutostartStatus;
use anyhow::{Context, Result};

/// The name every per-user install registered before #3801, whichever account
/// it ran as.
pub(crate) const SHARED: &str = "Roomler";

/// The name before the P3d rename, under the same rule as [`SHARED`].
// RETIRED-NAME-ANCHOR: a host may still carry the pre-P3d task; without this the paths lose it and the host runs two daemons. docs/fr/FR-21
pub(crate) const RETIRED: &str = "RoomlerAgent";

/// The names an account's autostart task had before #3801, in the order every
/// path looks for one. Any account's task may hold one.
pub(crate) const LEGACY: [&str; 2] = [SHARED, RETIRED];

/// The launcher check's name before #3801: a throwaway task `service install`
/// registers for seconds where an AppLocker policy is in force (#3292).
pub(crate) const SHARED_CHECK: &str = "Roomler launcher check";

/// The autostart task of the account `sid` (its SID, `S-1-5-21-…`).
pub(crate) fn autostart(sid: &str) -> String {
    format!("{SHARED}-{sid}")
}

/// The launcher check of the account `sid`.
pub(crate) fn launcher_check(sid: &str) -> String {
    format!("{SHARED_CHECK}-{sid}")
}

/// FR-150 P1a — the start task's name before its account's SID.
pub(crate) const START: &str = "Roomler start";

/// FR-150 P1a — the start task of the account `sid`: a task of the account's
/// own that a refused registration leaves, whose one-shots run `service start`
/// and which deletes itself an hour later. [`account_of`] reads no account in
/// it.
pub(crate) fn start_task(sid: &str) -> String {
    format!("{START}-{sid}")
}

/// The account an autostart task's name is for: `Roomler-<SID>`, the whole
/// name. The shared name, the launcher check's and a name that only starts like
/// one are no account's. `Roomler` is a prefix of every account's own name, so
/// this is never a `starts_with`.
pub(crate) fn account_of(name: &str) -> Option<&str> {
    let sid = name.strip_prefix(SHARED)?.strip_prefix('-')?;
    is_sid(sid).then_some(sid)
}

/// Is `text` a SID, `S-1-` and then numbers between dashes?
pub(crate) fn is_sid(text: &str) -> bool {
    text.strip_prefix("S-1-").is_some_and(|rest| {
        rest.split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Are `a` and `b` one account's SID? A SID's letters compare without case.
fn same_account(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The machine's task library as the process asking sees it: the seam between
/// the decisions here and Task Scheduler.
pub(crate) trait Library {
    /// Is a task of this name there, for this process? A standard account sees
    /// its own tasks. `true` when the question could not be asked, so a caller
    /// that takes absence as done is never fooled into skipping work.
    fn exists(&self, name: &str) -> bool;
    /// The SID of the account the task runs as, from its definition. `None`
    /// when this process may not read it, it is not there, or it names no
    /// account this machine can tell.
    fn principal(&self, name: &str) -> Option<String>;
    /// Every task in the library's top folder that this process sees, by name.
    /// `None` when the listing fails.
    fn names(&self) -> Option<Vec<String>>;
    /// What `service status` says of one task.
    fn status(&self, name: &str) -> Result<AutostartStatus>;
    /// Register `xml` as `name`, replacing a task of that name.
    fn create(&mut self, name: &str, xml: &str) -> Result<()>;
    /// Delete `name`. A task this process cannot find is gone already.
    fn delete(&mut self, name: &str) -> Result<()>;
    /// End the running instance of `name` (`schtasks /End`): the launcher the
    /// task started, and through the launcher's kill-on-close job the daemon
    /// (#3885, `agents/roomlerdw`).
    fn end(&mut self, name: &str) -> Result<()>;
    /// FR-150 P1a — may this process's account, from a medium token, replace
    /// and delete `name`? A self-update registers the task from such a token.
    /// An elevated process is answered for the account behind it. `None`: the
    /// question could not be asked (no such task, or its descriptor could not
    /// be read).
    fn may_replace(&self, _name: &str) -> Option<bool> {
        None
    }
}

/// FR-150 P1a — what `service install` registers for an account.
pub(crate) struct Registration<'a> {
    /// The account's task.
    pub(crate) xml: &'a str,
    /// The definition names a descriptor of the account's own
    /// (`ROOMLERD_TASK_SD`, on unless set to 0). Off, the registration is the
    /// one before FR-150: no repair and no start task.
    pub(crate) own_descriptor: bool,
    /// [`start_task`]'s definition, when the daemon should start after this
    /// registration (a config exists).
    pub(crate) start: Option<&'a str>,
}

/// Is the task `name` the account `sid`'s, by its principal? A task this
/// process cannot see, a definition it may not read and one that names no
/// account it can tell are not its own (#3801).
pub(crate) fn is_own(lib: &impl Library, name: &str, sid: &str) -> bool {
    lib.exists(name) && lib.principal(name).is_some_and(|p| same_account(&p, sid))
}

/// The task that starts the daemon of the account `sid`: its own, else a
/// legacy one whose principal is `sid`. What `service start` runs, and what the
/// hard stop ends.
pub(crate) fn own_autostart(lib: &impl Library, sid: &str) -> Option<String> {
    let own = autostart(sid);
    if lib.exists(&own) {
        return Some(own);
    }
    LEGACY
        .into_iter()
        .find(|name| is_own(lib, name, sid))
        .map(str::to_string)
}

/// FR-150 P1a — does `service stop --for-upgrade`, the per-user MSI's
/// `StopOldDaemon`, find nothing to stop for the account `sid`? Yes when no
/// daemon of its config holds a stop event this process can see
/// (`event_held` false) and the account has no task to end
/// ([`own_autostart`]). On a per-user update the old product's
/// `UnregisterAutostart` has ended and deleted the task before this step
/// (#4128's C10), so the hard stop the step falls back to would fail for want
/// of a task, and the person's log would read every healthy update as a failed
/// step.
pub(crate) fn nothing_to_stop(lib: &impl Library, sid: &str, event_held: bool) -> bool {
    !event_held && own_autostart(lib, sid).is_none()
}

/// `service status` for the account `sid`: its own task, else a legacy one
/// whose principal is `sid`.
pub(crate) fn status(lib: &impl Library, sid: &str) -> Result<AutostartStatus> {
    match lib.status(&autostart(sid))? {
        AutostartStatus::NotInstalled => {}
        other => return Ok(other),
    }
    for name in LEGACY {
        match lib.status(name)? {
            AutostartStatus::Installed if is_own(lib, name, sid) => {
                return Ok(AutostartStatus::Installed);
            }
            AutostartStatus::Unknown => return Ok(AutostartStatus::Unknown),
            _ => {}
        }
    }
    Ok(AutostartStatus::NotInstalled)
}

/// `service install` for the account `sid`: register its task, then delete a
/// legacy task of its own, best effort, once the new one is there. Never
/// another account's, and never before its own task registered: a
/// registration that fails leaves the old task to start the daemon.
///
/// FR-150 P1a, with the account's own descriptor ([`Registration`]):
/// - a replace that went through but left a task its account cannot replace
///   (an elevated run over a task an elevated registration made: the replace
///   keeps the old descriptor, #4128's C5) deletes the task and registers it
///   again, so the descriptor applies (C5c). A daemon running under the
///   deleted task runs on (C5d).
/// - a replace refused over a task that is there registers [`start_task`],
///   when a daemon should start, and still fails, with the repair when the
///   account cannot replace its task.
/// - a registration that succeeds deletes a start task left before it.
pub(crate) fn register(lib: &mut impl Library, sid: &str, reg: &Registration) -> Result<()> {
    let name = autostart(sid);
    let start = start_task(sid);
    if let Err(e) = lib.create(&name, reg.xml) {
        if !reg.own_descriptor {
            return Err(e);
        }
        return Err(refused(lib, &name, &start, reg.start, e));
    }
    if reg.own_descriptor && lib.may_replace(&name) == Some(false) {
        repair(lib, &name, reg.xml)?;
    }
    if reg.own_descriptor
        && lib.exists(&start)
        && let Err(e) = lib.delete(&start)
    {
        tracing::warn!(task = %start, error = %format!("{e:#}"), "the start task could not be removed: it deletes itself within the hour");
    }
    for name in LEGACY {
        if is_own(lib, name, sid)
            && let Err(e) = lib.delete(name)
        {
            tracing::warn!(
                task = name,
                error = %format!("{e:#}"),
                "this account's task from before #3801 could not be removed: it starts the \
                 daemon too, and the second start exits"
            );
        }
    }
    Ok(())
}

/// FR-150 P1a — `name` is registered with its new definition, and its account
/// still cannot replace it: an elevated registration before FR-150 left it, and
/// the replace kept that descriptor (#4128's C5). Delete it and register it
/// again, so the definition's descriptor applies (C5c); a daemon running under
/// it runs on (C5d). A delete that fails leaves the task as the replace left
/// it, which starts the daemon, so it is a warning. A registration that fails
/// after the delete is tried once more, and then fails the step, since then no
/// task starts the daemon.
fn repair(lib: &mut impl Library, name: &str, xml: &str) -> Result<()> {
    if let Err(e) = lib.delete(name) {
        tracing::warn!(
            task = %name,
            error = %format!("{e:#}"),
            "this account's task could not be deleted to be registered again with its own \
             descriptor: its updates still cannot replace it"
        );
        return Ok(());
    }
    if let Err(first) = lib.create(name, xml) {
        lib.create(name, xml).with_context(|| {
            format!(
                "{name} was deleted to be registered again with the account's own descriptor, \
                 and its registration failed twice (first: {first:#}). Run `roomlerd service \
                 install` again: until then no task starts the daemon"
            )
        })?;
    }
    match lib.may_replace(name) {
        Some(true) => tracing::info!(
            task = %name,
            "registered this account's task again with its own descriptor: its updates can \
             replace it now"
        ),
        answer => tracing::warn!(
            task = %name,
            ?answer,
            "registered this account's task again, and its account still cannot replace it"
        ),
    }
    Ok(())
}

/// FR-150 P1a — the account's task is there and its replace was refused. A
/// medium process may still run it (#4128's C2), so when a daemon should start,
/// [`start_task`] does it at the one-shots, and the error says how the task is
/// repaired when its account cannot replace it.
fn refused(
    lib: &mut impl Library,
    name: &str,
    start: &str,
    start_xml: Option<&str>,
    error: anyhow::Error,
) -> anyhow::Error {
    let started = match start_xml {
        Some(xml) if lib.exists(name) => match lib.create(start, xml) {
            Ok(()) => {
                tracing::warn!(
                    task = %name,
                    start_task = %start,
                    "this account's task could not be replaced; the start task starts the daemon \
                     at the one-shots, then deletes itself"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    start_task = %start,
                    error = %format!("{e:#}"),
                    "the start task could not be registered either"
                );
                false
            }
        },
        _ => false,
    };
    if lib.may_replace(name) != Some(false) {
        return error;
    }
    let until = if started {
        format!("; until then {start} starts the daemon after each update")
    } else {
        String::new()
    };
    error.context(format!(
        "this account cannot replace its task {name}: an elevated registration owns it (#3507). \
         Run `roomlerd service install` once from an elevated prompt, or the installer as \
         administrator, to repair it{until}"
    ))
}

/// `service uninstall` for the account `sid`: its task, its start task and
/// launcher check, and the legacy ones whose principal is `sid`.
pub(crate) fn unregister(lib: &mut impl Library, sid: &str) -> Result<()> {
    let _ = lib.delete(&start_task(sid));
    lib.delete(&autostart(sid))?;
    for name in LEGACY {
        if is_own(lib, name, sid) {
            lib.delete(name)?;
        }
    }
    let _ = lib.delete(&launcher_check(sid));
    if is_own(lib, SHARED_CHECK, sid) {
        let _ = lib.delete(SHARED_CHECK);
    }
    Ok(())
}

/// What the per-user arm of `cleanup-legacy-install` does with one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The account it cleans: removed.
    Remove,
    /// Not there.
    Absent,
    /// Another account's: left in place.
    Another,
    /// Its account cannot be read: left in place.
    Unknown,
    /// The arm cannot tell which account it cleans, for this reason: left in
    /// place.
    NoAccount(String),
}

/// One task the per-user arm found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) name: String,
    /// Its name in the report, with an account's SID left out ([`shown`]).
    pub(crate) shown: String,
    pub(crate) verdict: Verdict,
}

/// #3801 — the per-user arm's choice for `cleaned`, the account it cleans
/// (`Err`: why it cannot tell that account). It removes that account's own task
/// and a legacy one whose principal is that account. It leaves every other
/// task: another account's own (from the listing), a legacy one that runs as
/// another account or whose account it cannot read, and all of them when it
/// cleans no account.
pub(crate) fn clean_up_choice(lib: &impl Library, cleaned: Result<&str, &str>) -> Vec<Choice> {
    let mut choice = Vec::new();
    if let Ok(sid) = cleaned {
        let name = autostart(sid);
        let verdict = if lib.exists(&name) {
            Verdict::Remove
        } else {
            Verdict::Absent
        };
        choice.push(Choice {
            shown: shown(&name, cleaned),
            name,
            verdict,
        });
    }
    for name in LEGACY {
        let verdict = match cleaned {
            _ if !lib.exists(name) => Verdict::Absent,
            Err(why) => Verdict::NoAccount(why.to_string()),
            Ok(sid) => match lib.principal(name) {
                Some(p) if same_account(&p, sid) => Verdict::Remove,
                Some(_) => Verdict::Another,
                None => Verdict::Unknown,
            },
        };
        choice.push(Choice {
            name: name.to_string(),
            shown: name.to_string(),
            verdict,
        });
    }
    for name in lib.names().unwrap_or_default() {
        let Some(sid) = account_of(&name) else {
            continue;
        };
        let verdict = match cleaned {
            Ok(own) if same_account(sid, own) => continue,
            Ok(_) => Verdict::Another,
            Err(why) => Verdict::NoAccount(why.to_string()),
        };
        choice.push(Choice {
            shown: shown(&name, cleaned),
            name,
            verdict,
        });
    }
    choice
}

/// How the per-user arm's report names the task `name`: an account's own task
/// with its SID left out, `Roomler-<this account's SID>` for the account it
/// cleans and `Roomler-<another account's SID>` for any other, so no line names
/// an account (#3801).
fn shown(name: &str, cleaned: Result<&str, &str>) -> String {
    match (account_of(name), cleaned) {
        (None, _) => name.to_string(),
        (Some(sid), Ok(own)) if same_account(sid, own) => {
            format!("{SHARED}-<this account's SID>")
        }
        (Some(_), Ok(_)) => format!("{SHARED}-<another account's SID>"),
        (Some(_), Err(_)) => format!("{SHARED}-<an account's SID>"),
    }
}

/// The text of a console program's output: UTF-16 when it starts with the
/// little-endian mark or reads as UTF-16 text, else its bytes as UTF-8, with a
/// replacement character for any byte that is not.
pub(crate) fn console_text(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(&[0xFF, 0xFE][..]);
    let utf16 = body.is_some() || (bytes.len() >= 2 && bytes[0] != 0 && bytes[1] == 0);
    if !utf16 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let units: Vec<u16> = body
        .unwrap_or(bytes)
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// The account in a task's definition (`schtasks /Query /TN <name> /XML`): the
/// `<UserId>` of its principal, never a trigger's, with the document's entities
/// read. `None` for a principal that names a group, or none.
pub(crate) fn principal_user_id(xml: &str) -> Option<String> {
    let principals = xml
        .split_once("<Principals")?
        .1
        .split_once("</Principals>")?
        .0;
    let principal = principals.split_once("<Principal")?.1;
    let principal = principal
        .split_once("</Principal>")
        .map_or(principal, |(inside, _)| inside);
    let user = principal
        .split_once("<UserId>")?
        .1
        .split_once("</UserId>")?
        .0
        .trim();
    (!user.is_empty()).then(|| {
        user.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    })
}

/// The tasks of the library's top folder, by name, from a
/// `schtasks /Query [/TN <name>] /FO CSV /NH` listing: each row's first field,
/// `\<name>`, whole, its leading `\` optional. A task in a folder below the top
/// one is left out.
pub(crate) fn listed_names(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|row| {
            let field = row.trim().strip_prefix('"')?.split('"').next()?;
            let name = field.strip_prefix('\\').unwrap_or(field);
            (!name.is_empty() && !name.contains('\\')).then(|| name.to_string())
        })
        .collect()
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
        /// FR-150 — its descriptor gives its account read only: an elevated
        /// registration whose definition named no descriptor (#3507). Its
        /// account may read, run and end it, and may not replace or delete
        /// it; a replace keeps the descriptor, whoever asks (#4128's C2, C5).
        pub(crate) locked: bool,
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
        /// Every create, end and delete asked of the library, refused ones
        /// too.
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

        /// FR-150 — with a task `name` of `principal`'s that an elevated
        /// registration before FR-150 left: that account cannot replace it.
        pub(crate) fn with_locked(mut self, name: &str, principal: &str) -> Self {
            self = self.with_task(name, Some(principal), false);
            if let Some(task) = self.tasks.last_mut() {
                task.locked = true;
            }
            self
        }

        fn with_task(mut self, name: &str, principal: Option<&str>, seen_by_all: bool) -> Self {
            self.tasks.push(Task {
                name: name.into(),
                principal: principal.map(str::to_string),
                seen_by_all,
                locked: false,
            });
            self
        }

        /// FR-150 — is the task `name` there, and locked?
        pub(crate) fn locked(&self, name: &str) -> Option<bool> {
            self.find(name).map(|i| self.tasks[i].locked)
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

        /// Was a create, an end or a delete of `name` asked?
        pub(crate) fn asked_for(&self, name: &str) -> bool {
            self.asked.iter().any(|(_, n)| n.eq_ignore_ascii_case(name))
        }

        fn find(&self, name: &str) -> Option<usize> {
            self.tasks
                .iter()
                .position(|t| t.name.eq_ignore_ascii_case(name))
        }

        fn may_change(&self, task: &Task) -> bool {
            self.elevated || (task.runs_as(&self.caller) && !task.locked)
        }

        /// Read its definition, run it and end it: what a locked task's own
        /// account keeps (#4128's C2).
        fn may_read(&self, task: &Task) -> bool {
            self.elevated || task.runs_as(&self.caller)
        }

        fn sees(&self, task: &Task) -> bool {
            self.may_read(task) || task.seen_by_all
        }
    }

    impl super::Library for Library {
        fn exists(&self, name: &str) -> bool {
            self.find(name).is_some_and(|i| self.sees(&self.tasks[i]))
        }

        fn principal(&self, name: &str) -> Option<String> {
            let task = &self.tasks[self.find(name)?];
            if self.may_read(task) {
                task.principal.clone()
            } else {
                None
            }
        }

        fn names(&self) -> Option<Vec<String>> {
            Some(
                self.tasks
                    .iter()
                    .filter(|t| self.sees(t))
                    .map(|t| t.name.clone())
                    .collect(),
            )
        }

        fn status(&self, name: &str) -> Result<AutostartStatus> {
            Ok(if super::Library::exists(self, name) {
                AutostartStatus::Installed
            } else {
                AutostartStatus::NotInstalled
            })
        }

        /// A replace keeps the task's descriptor, whoever asks (#4128's C1b,
        /// C5). A new task is locked when an elevated process registers a
        /// definition that names no descriptor of its own (C2), and not when
        /// it names one (C3).
        fn create(&mut self, name: &str, xml: &str) -> Result<()> {
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
                    locked: self.elevated && !xml.contains("<SecurityDescriptor>"),
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

        fn end(&mut self, name: &str) -> Result<()> {
            self.asked.push(("end", name.into()));
            match self.find(name) {
                Some(i) if self.may_read(&self.tasks[i]) => Ok(()),
                Some(i) if self.sees(&self.tasks[i]) => bail!("ERROR: Access is denied."),
                _ => bail!("ERROR: The system cannot find the file specified."),
            }
        }

        /// The account's own task that is not locked, as the medium access
        /// check answers it; nothing for a task this process may not read.
        fn may_replace(&self, name: &str) -> Option<bool> {
            let task = &self.tasks[self.find(name)?];
            self.may_read(task)
                .then(|| task.runs_as(&self.caller) && !task.locked)
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
    /// FR-150 P1a's definition: it names a descriptor of its own.
    const XML: &str = "<Task><SecurityDescriptor>O:S-1-5-21-1D:</SecurityDescriptor></Task>";
    /// The definition before FR-150, and with `ROOMLERD_TASK_SD=0`.
    const MASTER_XML: &str = "<Task/>";
    const START_XML: &str = "<Task><SecurityDescriptor>O:S-1-5-21-1D:</SecurityDescriptor></Task>";

    /// An account's own autostart task, spelled out.
    fn named(sid: &str) -> String {
        format!("Roomler-{sid}")
    }

    /// FR-150 P1a's registration, with no config (no start task).
    fn p1a() -> Registration<'static> {
        Registration {
            xml: XML,
            own_descriptor: true,
            start: None,
        }
    }

    /// FR-150 P1a's registration on an enrolled device.
    fn p1a_enrolled() -> Registration<'static> {
        Registration {
            start: Some(START_XML),
            ..p1a()
        }
    }

    /// The registration before FR-150: what `ROOMLERD_TASK_SD=0` restores.
    fn master() -> Registration<'static> {
        Registration {
            xml: MASTER_XML,
            own_descriptor: false,
            start: None,
        }
    }

    /// FR-150 (#3507), the model of the release before it: a task registered
    /// from an elevated token is its account's to read, run and end, not to
    /// replace. So the account's own update, at medium, is refused, and an
    /// elevated replace keeps the task as it was.
    #[test]
    fn an_elevated_registration_before_p1a_cannot_be_replaced_by_its_update() {
        let mut lib = Fake::new(A, true);
        register(&mut lib, A, &master()).unwrap();
        assert_eq!(lib.locked(&named(A)), Some(true));
        lib.switch(A, false);
        let update = register(&mut lib, A, &master());
        assert!(
            format!("{:#}", update.unwrap_err()).contains("Access is denied"),
            "{:?}",
            lib.asked
        );
        assert_eq!(lib.asked, [("create", named(A))]);
        assert_eq!(own_autostart(&lib, A), Some(named(A)));
        lib.switch(A, true);
        register(&mut lib, A, &master()).unwrap();
        assert_eq!(lib.asked, [("create", named(A))]);
        assert_eq!(lib.locked(&named(A)), Some(true));
    }

    /// FR-150 P1a — a definition that names the account's own descriptor
    /// gives a new task its account can replace, from an elevated token
    /// (#4128's C3) and a medium one (C3b, C3c), so the next update's
    /// registration and an uninstall at medium succeed.
    #[test]
    fn a_task_with_its_own_descriptor_stays_its_accounts_to_replace() {
        for elevated in [true, false] {
            let mut lib = Fake::new(A, elevated);
            register(&mut lib, A, &p1a()).unwrap();
            assert_eq!(lib.locked(&named(A)), Some(false), "elevated {elevated}");
            lib.switch(A, false);
            register(&mut lib, A, &p1a()).unwrap();
            assert_eq!(lib.asked, [("create", named(A))], "elevated {elevated}");
            unregister(&mut lib, A).unwrap();
            assert_eq!(lib.owner(&named(A)), None, "elevated {elevated}");
        }
    }

    /// FR-150 P1a — over a task an elevated registration before P1a left, an
    /// elevated `service install` deletes the task and registers it again, so
    /// its descriptor applies (#4128's C5c), and a start task left before goes
    /// too. At medium the replace is refused and the task is left as it was.
    #[test]
    fn an_elevated_install_registers_a_locked_task_again() {
        let mut lib = Fake::new(A, true)
            .with_locked(&named(A), A)
            .with(&start_task(A), Some(A))
            .with(SHARED, Some(A));
        register(&mut lib, A, &p1a_enrolled()).unwrap();
        assert_eq!(
            lib.asked,
            [
                ("create", named(A)),
                ("delete", named(A)),
                ("create", named(A)),
                ("delete", start_task(A)),
                ("delete", SHARED.to_string()),
            ]
        );
        assert_eq!(lib.locked(&named(A)), Some(false));
        assert_eq!(lib.owner(&start_task(A)), None);
        assert_eq!(lib.owner(SHARED), None);
        lib.switch(A, false);
        register(&mut lib, A, &p1a_enrolled()).unwrap();
        assert_eq!(lib.asked, [("create", named(A))]);

        let mut lib = Fake::new(A, false).with_locked(&named(A), A);
        assert!(register(&mut lib, A, &p1a()).is_err());
        assert!(
            !lib.asked.iter().any(|(verb, _)| *verb == "delete"),
            "{:?}",
            lib.asked
        );
        assert_eq!(lib.locked(&named(A)), Some(true));
    }

    /// FR-150 P1a — a replace refused over the account's task, which is there,
    /// leaves `Roomler start-<SID>` on an enrolled device, and the step still
    /// fails, with the repair when the account cannot replace its task. A
    /// later registration that succeeds deletes it, and so does `unregister`.
    /// No start task without a config, for a task that is not there, or with
    /// `ROOMLERD_TASK_SD=0`.
    #[test]
    fn a_refused_replace_leaves_a_start_of_the_accounts_own() {
        let mut lib = Fake::new(A, false).with_locked(&named(A), A);
        let refused = format!("{:#}", register(&mut lib, A, &p1a_enrolled()).unwrap_err());
        assert!(refused.contains("cannot replace its task"), "{refused}");
        assert!(refused.contains("elevated prompt"), "{refused}");
        assert!(refused.contains(&start_task(A)), "{refused}");
        assert!(refused.contains("Access is denied"), "{refused}");
        assert_eq!(lib.asked, [("create", named(A)), ("create", start_task(A))]);
        assert_eq!(lib.owner(&start_task(A)), Some(Some(A.to_string())));
        assert_eq!(lib.locked(&start_task(A)), Some(false));
        // The start task is no account's autostart, and the start still runs
        // the account's own task.
        assert_eq!(account_of(&start_task(A)), None);
        assert_eq!(own_autostart(&lib, A), Some(named(A)));
        // The next update, refused again, replaces it.
        lib.switch(A, false);
        assert!(register(&mut lib, A, &p1a_enrolled()).is_err());
        assert_eq!(lib.asked, [("create", named(A)), ("create", start_task(A))]);
        // The repair deletes it.
        lib.switch(A, true);
        register(&mut lib, A, &p1a_enrolled()).unwrap();
        assert_eq!(lib.owner(&start_task(A)), None, "{:?}", lib.asked);
        // An uninstall deletes one left behind, even where the task itself
        // cannot be deleted.
        let mut lib = Fake::new(A, false)
            .with_locked(&named(A), A)
            .with(&start_task(A), Some(A));
        assert!(unregister(&mut lib, A).is_err());
        assert_eq!(lib.owner(&start_task(A)), None);

        // A refused registration for another reason: a start task, and no
        // repair named.
        let mut lib = Fake::new(A, false).with(&named(A), Some(A));
        lib.refuse.push(named(A));
        let refused = format!("{:#}", register(&mut lib, A, &p1a_enrolled()).unwrap_err());
        assert!(!refused.contains("elevated prompt"), "{refused}");
        assert_eq!(lib.owner(&start_task(A)), Some(Some(A.to_string())));

        for (case, mut lib, reg) in [
            (
                "no config",
                Fake::new(A, false).with_locked(&named(A), A),
                p1a(),
            ),
            (
                "the switch",
                Fake::new(A, false).with_locked(&named(A), A),
                Registration {
                    start: Some(START_XML),
                    ..master()
                },
            ),
            ("no task", Fake::new(A, false), p1a_enrolled()),
        ] {
            if case == "no task" {
                lib.refuse.push(named(A));
            }
            assert!(register(&mut lib, A, &reg).is_err(), "{case}");
            assert_eq!(lib.asked, [("create", named(A))], "{case}");
            assert_eq!(lib.owner(&start_task(A)), None, "{case}");
        }
    }

    /// FR-150 P1a — `ROOMLERD_TASK_SD=0` registers the task as before FR-150:
    /// an elevated run over a task its account cannot replace neither deletes
    /// it nor registers it again, and a start task left before stays.
    #[test]
    fn the_switch_registers_masters_task() {
        let mut lib = Fake::new(A, true)
            .with_locked(&named(A), A)
            .with(&start_task(A), Some(A));
        register(&mut lib, A, &master()).unwrap();
        assert_eq!(lib.asked, [("create", named(A))]);
        assert_eq!(lib.locked(&named(A)), Some(true));
        assert_eq!(lib.owner(&start_task(A)), Some(Some(A.to_string())));
    }

    /// #3801 — two accounts with a per-user install each register a task each,
    /// named for the account. The second registration neither fails on the
    /// first's task nor replaces it, from a standard or an elevated process,
    /// and each account's start finds its own.
    #[test]
    fn each_account_registers_a_task_of_its_own() {
        for elevated in [false, true] {
            let mut lib = Fake::new(A, false);
            register(&mut lib, A, &p1a()).unwrap();
            lib.switch(B, elevated);
            let registered = register(&mut lib, B, &p1a());
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
            register(&mut lib, A, &p1a()).unwrap();
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
        assert!(register(&mut lib, A, &p1a()).is_err(), "{:?}", lib.asked);
        assert_eq!(lib.owner(SHARED), Some(Some(A.to_string())));
        assert_eq!(own_autostart(&lib, A), Some(SHARED.to_string()));
        assert_eq!(status(&lib, A).unwrap(), Installed);

        lib.refuse.clear();
        lib.switch(A, false);
        register(&mut lib, A, &p1a()).unwrap();
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
            register(&mut lib, A, &p1a()).unwrap();
            assert_eq!(own_autostart(&lib, A), Some(named(A)), "{case}");
            unregister(&mut lib, A).unwrap();
            assert!(!lib.asked_for(SHARED), "{case}: {:?}", lib.asked);
            assert!(lib.owner(SHARED).is_some(), "{case}");
        }
    }

    /// #3801 — a name is an account's own only when it is `Roomler-<SID>`,
    /// whole: `Roomler` is a prefix of every account's own name, and the
    /// launcher check's names start with it too.
    #[test]
    fn a_name_is_an_accounts_only_when_it_is_that_whole_name() {
        assert_eq!(account_of(&autostart(A)), Some(A));
        assert_eq!(account_of(&autostart(B)), Some(B));
        let check = launcher_check(A);
        let start = start_task(A);
        for name in [
            SHARED,
            RETIRED,
            SHARED_CHECK,
            check.as_str(),
            START,
            start.as_str(),
            "Roomler-",
            "Roomler-S-1-",
            "Roomler-S-1-5-x",
            "Roomler-S-1-5--21",
            "Roomler-s-1-5-21-1",
            "RoomlerX-S-1-5-21-1",
            "Roomler S-1-5-21-1",
        ] {
            assert_eq!(account_of(name), None, "{name}");
        }
    }

    /// #3801 — the listing is read by whole names, top folder only: `Roomler`
    /// is listed only when a task of that name is, never because an account's
    /// own task starts with it.
    #[test]
    fn the_listing_is_read_by_whole_names() {
        let listing = format!(
            "\r\n\"\\{}\",\"N/A\",\"Bereit\"\r\n\"\\{SHARED_CHECK}\",\"N/A\",\"Ready\"\r\n\
             \"\\Microsoft\\Windows\\{SHARED}\",\"N/A\",\"Ready\"\r\n",
            named(B)
        );
        let names = listed_names(&listing);
        assert_eq!(names, [named(B), SHARED_CHECK.to_string()]);
        assert!(!names.iter().any(|n| n == SHARED), "{names:?}");
        let names = listed_names(&format!("\"\\{SHARED}\",\"N/A\",\"Ready\"\r\n"));
        assert_eq!(names, [SHARED.to_string()]);
        // One task's query answers with its whole name, however long: the
        // default table's name column is 40 characters, an account's own name
        // about 55. Read with or without its leading `\`.
        assert!(named(A).len() > 40);
        for row in [
            format!("\"\\{}\",\"N/A\",\"Ready\"", named(A)),
            format!("\"{}\",\"N/A\",\"Ready\"", named(A)),
        ] {
            assert_eq!(listed_names(&row), [named(A)], "{row}");
        }
        assert!(listed_names("").is_empty());
    }

    /// #3801 — the account in a task's definition is its principal's
    /// `<UserId>`, never its logon trigger's, as a SID or as a name with its
    /// entities read. A principal that names a group, or nobody, is none.
    #[test]
    fn the_principal_is_the_principals_user_not_a_triggers() {
        let xml = |principal: &str| {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n\
                 <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n\
                 <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>DOMAIN\\other</UserId>\
                 </LogonTrigger></Triggers>\r\n\
                 <Principals>\r\n<Principal id=\"Author\">\r\n{principal}\r\n\
                 <LogonType>InteractiveToken</LogonType>\r\n</Principal>\r\n</Principals>\r\n</Task>\r\n"
            )
        };
        let read = |principal: &str| principal_user_id(&xml(principal));
        assert_eq!(read(&format!("<UserId>{A}</UserId>")), Some(A.to_string()));
        assert_eq!(
            read("<UserId> DOMAIN\\a &amp; b </UserId>"),
            Some("DOMAIN\\a & b".to_string())
        );
        assert_eq!(read("<GroupId>S-1-5-32-545</GroupId>"), None);
        assert_eq!(read("<UserId></UserId>"), None);
        assert_eq!(principal_user_id("ERROR: Access is denied."), None);
        assert_eq!(principal_user_id(""), None);
    }

    /// #3801 — `schtasks` output is read whether it comes as UTF-16, with its
    /// mark or without, or as bytes.
    #[test]
    fn console_output_is_read_as_utf16_or_as_bytes() {
        let text = format!("<UserId>{A}</UserId>");
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let marked: Vec<u8> = [0xFF, 0xFE].into_iter().chain(utf16.clone()).collect();
        assert_eq!(console_text(&marked), text);
        assert_eq!(console_text(&utf16), text);
        assert_eq!(console_text(text.as_bytes()), text);
        assert_eq!(console_text(b""), "");
        assert!(is_sid(A) && !is_sid("DOMAIN\\a") && !is_sid("S-1-") && !is_sid(""));
    }

    /// FR-150 P1a (#4128's C10) — on a per-user update the old product's
    /// `UnregisterAutostart` ends and deletes the task before the new one's
    /// `StopOldDaemon` runs. With no daemon holding its stop event and no task
    /// to end, the step has nothing to stop; a daemon still holding its event,
    /// or a task of the account's own, locked or legacy, is stopped as before.
    #[test]
    fn the_upgrade_stop_after_the_old_uninstall_has_nothing_to_stop() {
        let after_the_uninstall = Fake::new(A, false);
        assert!(nothing_to_stop(&after_the_uninstall, A, false));
        assert!(
            !nothing_to_stop(&after_the_uninstall, A, true),
            "a daemon holds its stop event"
        );
        let cases = [
            ("its task", Fake::new(A, false).with(&named(A), Some(A))),
            (
                "its locked task",
                Fake::new(A, false).with_locked(&named(A), A),
            ),
            ("its legacy task", Fake::new(A, false).with(SHARED, Some(A))),
        ];
        for (case, lib) in cases {
            assert!(!nothing_to_stop(&lib, A, false), "{case} is there to end");
        }
        let others = Fake::new(A, false)
            .with_seen_by_all(SHARED, Some(B))
            .with(&named(B), Some(B));
        assert!(
            nothing_to_stop(&others, A, false),
            "another account's tasks are not this one's to end"
        );
    }
}
