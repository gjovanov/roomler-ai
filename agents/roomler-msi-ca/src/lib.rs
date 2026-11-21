// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3375 — the per-Machine MSI's custom actions that carry the `Roomler`
//! service's `Environment` across a major upgrade.
//!
//! ## Why a DLL
//!
//! The MSI's `MajorUpgrade` is `afterInstallInitialize`
//! (`agents/roomlerd/wix-perMachine/main.wxs`), so the old product is removed
//! before the new files land, and the old product's `UnregisterService`
//! deletes the service: its registry key goes, and `Environment` with it. The
//! new `RegisterService` creates a fresh entry, and only the SystemContext
//! switch came back, written by the action after it. Every other entry
//! `roomlerd set-service-env-var` wrote lapsed at the next release, the
//! documented kill switch `ROOMLERD_SFTP_AS_USER=0` among them.
//!
//! The condition that deletes the service lives in the product being
//! removed, so nothing a new release changes there helps the update into it.
//! What does run before that removal is the new package's IMMEDIATE actions,
//! and only three kinds of them can hand a value on:
//!
//! - AppSearch cannot: for a `REG_MULTI_SZ` it "sets the property to a value
//!   beginning with a null" (the RegLocator table's docs), which reads empty;
//! - an EXE action cannot set a property at all, and before `InstallFiles` a
//!   `FileKey` one runs the OLD binary;
//! - a script action would put a script engine inside a SYSTEM msiexec on
//!   every corporate laptop's update.
//!
//! So a DLL, with two entry points:
//!
//! - [`msi::CaptureServiceEnvironment`]: immediate, before `InstallInitialize`
//!   on an upgrade. It reads the old service's `Environment` into the private
//!   property `RoomlerServiceEnvironment`.
//! - [`msi::RestoreServiceEnvironment`]: deferred, as LocalSystem, after
//!   `RegisterService` and before the SystemContext actions, which restart the
//!   service. It reads that value from `CustomActionData` and writes every
//!   carried entry back as it was, except the ones the installer owns
//!   ([`carry::INSTALLER_OWNED`]).
//!
//! Neither one ever fails an upgrade. Each logs what it did, by NAME, never a
//! value, and returns success, and the MSI schedules both with
//! `Return="ignore"`: an action that failed here would stop this device's
//! updates, and a systematic failure every device's. The values never reach a
//! command line, and the MSI hides both properties from its log.
//!
//! ## #4196 — the files an install puts in place are there after the boot
//!
//! When something holds `roomlerd.exe` open without `FILE_SHARE_DELETE` while
//! the old product's removal deletes it, Windows Installer can neither delete
//! nor move it, so it schedules a delete at the next boot
//! (`PendingFileRenameOperations`) and the install ends 3010. The new product
//! then puts its file at the same path, or keeps the one there when it is of an
//! equal version, and neither cancels that delete: the session manager deleted
//! the NEW `roomlerd.exe` at the next boot, and the service could not start
//! (SCM 7000). Measured both ways in the #4196 lab, with a neutral process
//! holding the file. The per-user install does not have it: Windows Installer
//! never schedules a boot-time delete in a folder the person can write
//! ("unsafe path"), so it ends 0 and the file stays.
//!
//! What held the file does not matter, and the fix does not ask: the old
//! service's process, a worker it started in a session, the process of the old
//! product's own `UnregisterService` action, a scanner, a `roomler` call
//! running `roomlerd`. A running image can be renamed, so Windows Installer
//! moves it aside and nothing is left pending at its path; a handle that does
//! not share delete leaves the boot-time delete. The fix acts on that outcome,
//! with two entry points:
//!
//! - [`msi::ListInstalledFiles`]: immediate, after `InstallValidate`, on every
//!   run but an uninstall. It lists the full path of every file whose
//!   component stays installed ([`pending_ops::stays_local`]) and hands the
//!   list on as the next action's `CustomActionData`.
//! - [`msi::SpareInstalledFiles`]: deferred, as LocalSystem, after
//!   `InstallFiles`, so after the old product's removal and after whatever
//!   the new copy had to schedule. It takes every pending delete of a listed
//!   file out of both values ([`pending_ops::plan`]), unless a later operation
//!   puts a file at that path, which makes the delete the first half of a
//!   replacement. Every other operation stays, byte for byte.
//!
//! An uninstall keeps its deletes: they are what it wants. The spare needs no
//! rollback partner: whether the run completes or rolls back, the file at a
//! listed path is one that must stay (the new product's, or the old product's
//! that a rollback puts back), so a delete of it is wrong either way. Both
//! actions are `Return="ignore"` and return success after a logged miss, like
//! the carry's.

pub mod carry;

pub mod pending_ops;

#[cfg(windows)]
pub mod msi;
