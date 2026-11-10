// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Telemetry sink for the rc.44 SystemContext automation slice.
//!
//! When the WiX `EnableSystemContext` / `DisableSystemContext` /
//! `RollbackSystemContext` deferred CAs shell out to the agent's
//! `enable-system-context` / `disable-system-context` composite
//! subcommands, the CA runs as LocalSystem with no console, and any
//! stderr disappears into msiexec's verbose log (`%TEMP%\MSI*.LOG`).
//! Operators rarely think to grep that. So the composite subcommand
//! also writes a single-entry JSON file at a known location; the
//! installer wizard reads it after an MSI failure to surface an
//! actionable error to the operator.
//!
//! ## Path
//!
//! `%PROGRAMDATA%\roomler\last-system-context-attempt.json` —
//! `%PROGRAMDATA%` is admin-write / world-read by default, so the
//! LocalSystem-running CA can write it AND the operator's non-
//! elevated wizard process can read it.
//!
//! ## Schema
//!
//! Single entry, overwritten on every invocation (no ring buffer —
//! the wizard only needs to know what the MOST RECENT attempt did).
//! Atomic write via write-tmp-then-rename, so a partial write
//! never leaves corrupt JSON visible to a concurrent reader.
//!
//! ```json
//! {
//!   "version": 1,
//!   "ts": "2026-05-19T10:23:45Z",
//!   "command": "enable-system-context",
//!   "stage": "ok",
//!   "exit_code": 0,
//!   "stderr": "",
//!   "hint": ""
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The `command` of a record `enable-system-context` wrote.
pub const ENABLE_COMMAND: &str = "enable-system-context";
/// The `command` of a record `disable-system-context` wrote.
pub const DISABLE_COMMAND: &str = "disable-system-context";

/// #1727 — was SystemContext ON after the last recorded switch? What
/// `keep-system-context` (the perMachine MSI with no
/// `ENABLE_SYSTEM_CONTEXT` on its command line) restores.
///
/// The record is the evidence because nothing else survives an install:
/// every install deletes and recreates the service (and a major upgrade
/// runs the old product's `service uninstall` first), taking
/// `ROOMLERD_ENABLE_SYSTEM_SWAP` with it, while this file is only ever
/// written by an enable or a disable — every MSI since rc.44 runs one.
///
/// ON only for an enable that got past writing the variable: `ok`, or a
/// restart that failed after the write. A disable, a write that failed,
/// any other command or no record at all is OFF — what an install with
/// no property did before #1727.
pub fn last_switched_on(last: Option<&Attempt>) -> bool {
    matches!(
        last,
        Some(a) if a.command == ENABLE_COMMAND
            && matches!(a.stage, Stage::Ok | Stage::ServiceRestart)
    )
}

/// Where the attempt was when it terminated. `Ok` only on full
/// success; the other variants name which sub-step the composite
/// subcommand was inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Both env-var write AND service restart (when requested)
    /// completed successfully.
    Ok,
    /// `set_service_env_var` / `unset_service_env_var` failed.
    EnvVarWrite,
    /// `restart_service` failed (env-var write had succeeded).
    ServiceRestart,
    /// Catch-all for failures before stage classification (e.g. the
    /// SCM service doesn't exist on this host).
    Unknown,
}

/// Single attempt record. `version=1` is the current schema; bumps
/// require updating the wizard reader to keep parsing the prior
/// schema (forward-compat by treating unknown fields as ignored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    /// Schema version. Always 1 for v1.
    pub version: u32,
    /// RFC 3339 / ISO 8601 UTC timestamp.
    pub ts: String,
    /// Which composite subcommand ran (`enable-system-context` /
    /// `disable-system-context`).
    pub command: String,
    /// Where the attempt ended.
    pub stage: Stage,
    /// `0` on success; non-zero numeric error code when the CLI exited
    /// with one (kept loose — we don't try to map Win32 GetLastError
    /// codes onto a typed enum).
    pub exit_code: i32,
    /// Diagnostic stderr the failing helper produced. Empty on success.
    pub stderr: String,
    /// Operator-actionable hint composed by the composite subcommand
    /// (e.g. "Close services.msc and run `roomlerd restart-service`
    /// again."). Empty on success.
    pub hint: String,
}

impl Attempt {
    /// Build a success attempt with the current time stamp.
    pub fn ok(command: &str) -> Self {
        Self {
            version: 1,
            ts: now_iso8601(),
            command: command.to_string(),
            stage: Stage::Ok,
            exit_code: 0,
            stderr: String::new(),
            hint: String::new(),
        }
    }

    /// Build a failure attempt with the current time stamp.
    pub fn failure(command: &str, stage: Stage, stderr: &str, hint: &str) -> Self {
        Self {
            version: 1,
            ts: now_iso8601(),
            command: command.to_string(),
            stage,
            // Non-zero numeric placeholder — the actual process exit
            // code is whatever bash sees when the CLI returns. We
            // record 1 here as a "yes there was a failure" sentinel;
            // the stage field carries the actionable shape.
            exit_code: 1,
            stderr: stderr.to_string(),
            hint: hint.to_string(),
        }
    }
}

/// Location where attempts are persisted. `%PROGRAMDATA%\roomler\` on
/// Windows; the parent directory is created on first write.
pub fn path() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("roomler")
        .join("last-system-context-attempt.json")
}

/// Persist the attempt atomically — write to `<path>.tmp` then rename
/// into place. Best-effort: failure here doesn't fail the caller (we
/// don't want a telemetry-write failure to mask the actual return
/// status of the composite subcommand).
pub fn record(attempt: &Attempt) -> std::io::Result<()> {
    record_at(&path(), attempt)
}

/// Explicit-path variant of [`record`] used by unit tests so they
/// don't have to mutate `PROGRAMDATA` (which races across parallel
/// test threads).
///
/// #3609 — the temp file is one this writer made: a file left under its name
/// is removed first and `create_new` refuses one that appears, because a write
/// into it would keep that file's owner and DACL and the rename would make it
/// the record. Before it takes the name it is given Administrators as its
/// owner and SYSTEM and Administrators alone as its DACL, through its handle;
/// a caller that may not (not SYSTEM, not elevated) leaves the DACL its folder
/// hands down, and the next repair does the rest.
pub fn record_at(target: &std::path::Path, attempt: &Attempt) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(attempt)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut file = roomler_node_core::machine_tree::create_fresh(&tmp)?;
    let written = file.write_all(&json).and_then(|()| file.sync_all());
    if written.is_ok() {
        let _ = roomler_node_core::machine_tree::seal_new_file(&file);
    }
    drop(file);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, target)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Read the last attempt back. Returns `Ok(None)` when no file exists
/// yet (first run). The wizard calls this after observing an MSI
/// failure on a SystemContext install to surface the actionable
/// stage to the operator.
pub fn read_last() -> std::io::Result<Option<Attempt>> {
    read_last_at(&path())
}

/// #3609 — what `keep-system-context` decides by: the record under this
/// computer's `%ProgramData%\roomler`, repaired first and read only while it
/// is private (`machine_tree::read_record`). A record that is not reads as an
/// error, which the command takes as off, as it does an unreadable one.
pub fn read_last_private() -> std::io::Result<Option<Attempt>> {
    use roomler_node_core::machine_tree::{MachineTree, Switches};
    read_last_private_in(&MachineTree::here(), Switches::from_env())
}

/// [`read_last_private`] over `tree`, with the switches given.
pub fn read_last_private_in(
    tree: &roomler_node_core::machine_tree::MachineTree,
    switches: roomler_node_core::machine_tree::Switches,
) -> std::io::Result<Option<Attempt>> {
    match roomler_node_core::machine_tree::read_record(tree, switches) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Ok(None) => Ok(None),
        Err(why) => Err(std::io::Error::other(why)),
    }
}

/// Explicit-path variant of [`read_last`] used by unit tests.
pub fn read_last_at(target: &std::path::Path) -> std::io::Result<Option<Attempt>> {
    match std::fs::read(target) {
        Ok(bytes) => match serde_json::from_slice::<Attempt>(&bytes) {
            Ok(a) => Ok(Some(a)),
            Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn now_iso8601() -> String {
    use chrono::Utc;
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_attempt_has_zero_exit_code_and_empty_stderr() {
        let a = Attempt::ok("enable-system-context");
        assert_eq!(a.stage, Stage::Ok);
        assert_eq!(a.exit_code, 0);
        assert!(a.stderr.is_empty());
        assert!(a.hint.is_empty());
        assert_eq!(a.command, "enable-system-context");
        assert_eq!(a.version, 1);
    }

    #[test]
    fn failure_attempt_carries_stage_and_hint() {
        let a = Attempt::failure(
            "enable-system-context",
            Stage::ServiceRestart,
            "timeout: service state StartPending, expected Running",
            "Close services.msc and run `roomlerd restart-service` again.",
        );
        assert_eq!(a.stage, Stage::ServiceRestart);
        assert_eq!(a.exit_code, 1);
        assert!(a.stderr.contains("timeout"));
        assert!(a.hint.contains("services.msc"));
    }

    #[test]
    fn stage_round_trips_through_json() {
        for s in [
            Stage::Ok,
            Stage::EnvVarWrite,
            Stage::ServiceRestart,
            Stage::Unknown,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let back: Stage = serde_json::from_str(&j).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn stage_json_uses_snake_case() {
        assert_eq!(serde_json::to_string(&Stage::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&Stage::EnvVarWrite).unwrap(),
            "\"env_var_write\""
        );
        assert_eq!(
            serde_json::to_string(&Stage::ServiceRestart).unwrap(),
            "\"service_restart\""
        );
    }

    #[test]
    fn record_then_read_round_trips_via_explicit_path() {
        // Use the explicit-path API so we don't mutate PROGRAMDATA
        // (which would race with parallel test threads).
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("last-system-context-attempt.json");
        let original = Attempt::failure(
            "disable-system-context",
            Stage::EnvVarWrite,
            "RegSetValueExW failed: error 5 (ERROR_ACCESS_DENIED)",
            "Run from an elevated shell.",
        );
        record_at(&target, &original).unwrap();
        let read_back = read_last_at(&target).unwrap().expect("file should exist");
        assert_eq!(read_back.command, original.command);
        assert_eq!(read_back.stage, original.stage);
        assert_eq!(read_back.stderr, original.stderr);
        assert_eq!(read_back.hint, original.hint);
    }

    #[test]
    fn read_last_returns_none_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("does-not-exist.json");
        let result = read_last_at(&target).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn record_atomic_rename_overwrites_prior_attempt() {
        // The composite subcommand records on every invocation — a
        // successful invocation must overwrite a prior failure record
        // so the wizard's read_last() sees the most-recent state.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("last.json");
        let first = Attempt::failure(
            "enable-system-context",
            Stage::EnvVarWrite,
            "first attempt failed",
            "retry",
        );
        record_at(&target, &first).unwrap();
        let second = Attempt::ok("enable-system-context");
        record_at(&target, &second).unwrap();
        let read_back = read_last_at(&target).unwrap().unwrap();
        assert_eq!(read_back.stage, Stage::Ok);
        assert!(read_back.stderr.is_empty());
    }

    #[test]
    fn path_lives_under_programdata_roomler() {
        let p = path();
        let s = p.to_string_lossy().to_lowercase();
        assert!(s.contains("roomler"));
        assert!(s.ends_with("last-system-context-attempt.json"));
    }

    fn rec(command: &str, stage: Stage) -> Attempt {
        match stage {
            Stage::Ok => Attempt::ok(command),
            s => Attempt::failure(command, s, "e", "h"),
        }
    }

    /// #1727 — the whole table `keep-system-context` decides by. ON only
    /// for an enable that wrote the variable; everything else is what an
    /// install with no property did before: off.
    #[test]
    fn only_an_enable_that_wrote_the_switch_keeps_system_context_on() {
        let cells = [
            (Some(rec(ENABLE_COMMAND, Stage::Ok)), true),
            // The restart failed AFTER the variable was written: it is on.
            (Some(rec(ENABLE_COMMAND, Stage::ServiceRestart)), true),
            (Some(rec(ENABLE_COMMAND, Stage::EnvVarWrite)), false),
            (Some(rec(ENABLE_COMMAND, Stage::Unknown)), false),
            (Some(rec(DISABLE_COMMAND, Stage::Ok)), false),
            (Some(rec(DISABLE_COMMAND, Stage::EnvVarWrite)), false),
            (Some(rec("set-service-env-var", Stage::Ok)), false),
            (None, false),
        ];
        for (last, want) in cells {
            assert_eq!(
                last_switched_on(last.as_ref()),
                want,
                "{:?}",
                last.as_ref().map(|a| (&a.command, a.stage))
            );
        }
    }

    /// The record as a field host actually has it (CORPLAP-2 after
    /// `enable-system-context`, 2026-09-27), read back through the same
    /// path the subcommand uses. A reader that ever stopped parsing the
    /// shipped shape would turn every SystemContext host OFF on its next
    /// hand-run install — the bug this exists to prevent.
    #[test]
    fn the_shipped_record_shape_reads_as_on() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("last-system-context-attempt.json");
        std::fs::write(
            &target,
            br#"{ "version": 1, "ts": "2026-09-27T08:14:58Z", "command": "enable-system-context", "stage": "ok", "exit_code": 0, "stderr": "", "hint": "" }"#,
        )
        .unwrap();
        let last = read_last_at(&target).unwrap();
        assert!(last_switched_on(last.as_ref()));
        std::fs::write(
            &target,
            br#"{ "version": 1, "ts": "2026-09-26T23:46:52Z", "command": "disable-system-context", "stage": "ok", "exit_code": 0, "stderr": "", "hint": "" }"#,
        )
        .unwrap();
        let last = read_last_at(&target).unwrap();
        assert!(!last_switched_on(last.as_ref()));
    }

    /// #3609 — the record is written only into a file the writer made. A file
    /// left under the temporary name would keep its own owner and DACL
    /// through a write into it, and the rename would make it the record. A
    /// hard link to the file left there is the witness: it still reads what
    /// it held.
    #[test]
    fn the_record_is_never_written_into_a_file_it_did_not_make() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("last-system-context-attempt.json");
        let tmp = target.with_extension("json.tmp");
        std::fs::write(&tmp, b"made before the record").unwrap();
        let witness = dir.path().join("witness");
        std::fs::hard_link(&tmp, &witness).unwrap();
        record_at(&target, &Attempt::ok(ENABLE_COMMAND)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&witness).unwrap(),
            "made before the record",
            "the record was written into a file it did not make"
        );
        let last = read_last_at(&target).unwrap();
        assert!(last_switched_on(last.as_ref()));
    }

    fn an_enable() -> Vec<u8> {
        serde_json::to_vec(&Attempt::ok(ENABLE_COMMAND)).unwrap()
    }

    /// #3609 — `keep-system-context` takes the record only when SYSTEM and
    /// Administrators alone may change it. The field's usual drift (the record
    /// inherits an account's entry, SYSTEM owns it) is repaired first and the
    /// record still reads as on; a record an account that is not an
    /// administrator owns is left alone and reads as no record, which is off.
    #[test]
    fn keep_system_context_reads_the_record_only_when_it_is_private() {
        use roomler_node_core::machine_tree::Switches;
        use roomler_node_core::machine_tree::test_support::*;
        if !elevated_or_skip("the attempt record") {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let tree = waiting_tree(root.path());
        std::fs::write(tree.record(), an_enable()).unwrap();
        assert!(icacls(&tree.record(), &["/setowner", "*S-1-5-18"]));
        assert!(icacls(&tree.record(), &["/grant", "*S-1-5-32-546:(F)"]));
        let repair_off = Switches {
            repair: false,
            check: true,
        };
        let refused = read_last_private_in(&tree, repair_off);
        assert!(refused.is_err(), "not private: {refused:?}");
        let last = read_last_private_in(&tree, Switches::ON).expect("repaired, then read");
        assert!(last_switched_on(last.as_ref()), "SystemContext stays on");
        assert_eq!(
            owner(&sddl(&tree.record())),
            "SY",
            "SYSTEM keeps its record"
        );

        let other = local_guest().unwrap_or_else(|| USERS.to_string());
        assert!(icacls(&tree.record(), &["/setowner", &format!("*{other}")]));
        let before = sddl(&tree.record());
        let last = read_last_private_in(&tree, Switches::ON);
        assert!(last.is_err(), "another account's record: {last:?}");
        assert_eq!(sddl(&tree.record()), before, "and it is left alone");
    }
}
