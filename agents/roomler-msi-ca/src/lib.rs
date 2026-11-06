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

pub mod carry;

#[cfg(windows)]
pub mod msi;
