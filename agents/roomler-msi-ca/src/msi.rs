// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The entry points msiexec calls, and the registry and MSI calls under them.
//! Every path returns success unless installer-smoke forced a failure
//! ([`carry::FAIL_PROPERTY`]): what went wrong is logged, by name, and the
//! install goes on: with nothing carried, as every upgrade did before #3375,
//! or with nothing spared, as every install did before #4196.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSTALL_FAILURE, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS,
    ERROR_SUCCESS,
};
use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
    INSTALLMESSAGE_INFO, INSTALLSTATE, INSTALLSTATE_LOCAL, INSTALLSTATE_UNKNOWN, MSIHANDLE,
    MsiCloseHandle, MsiCreateRecord, MsiDatabaseOpenViewW, MsiGetActiveDatabase,
    MsiGetComponentStateW, MsiGetPropertyW, MsiGetTargetPathW, MsiProcessMessage,
    MsiRecordGetStringW, MsiRecordSetStringW, MsiSetPropertyW, MsiViewClose, MsiViewExecute,
    MsiViewFetch,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_MULTI_SZ, REG_VALUE_TYPE,
    RRF_RT_REG_MULTI_SZ, RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};

use crate::{carry, pending_ops};

// The pure module's component states are msi.h's.
const _: () = assert!(
    INSTALLSTATE_LOCAL == pending_ops::LOCAL && INSTALLSTATE_UNKNOWN == pending_ops::UNKNOWN
);

/// The service whose `Environment` is carried: the one the new product's
/// `RegisterService` creates under the same name.
const SERVICE_KEY: &str = r"SYSTEM\CurrentControlSet\Services\Roomler";
const ENV_VALUE: &str = "Environment";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// One line in the MSI log. The text goes in field 1 under the template
/// `[1]`, so nothing in it is read as a property reference.
fn log(install: MSIHANDLE, line: &str) {
    let template = wide("[1]");
    let text = wide(&format!("roomler_msi_ca: {line}"));
    // SAFETY: plain MSI handle calls on buffers that outlive them; the record
    // is closed on every path.
    unsafe {
        let record = MsiCreateRecord(1);
        if record == 0 {
            return;
        }
        MsiRecordSetStringW(record, 0, template.as_ptr());
        MsiRecordSetStringW(record, 1, text.as_ptr());
        MsiProcessMessage(install, INSTALLMESSAGE_INFO, record);
        MsiCloseHandle(record);
    }
}

/// A string an MSI getter writes, by the double call each of them documents:
/// the first, with a zero-length buffer, answers ERROR_MORE_DATA and the length
/// without its null; the second fills a buffer one longer. `None` when either
/// call answers anything else.
fn msi_string(mut get: impl FnMut(*mut u16, *mut u32) -> u32) -> Option<String> {
    let mut empty = [0u16; 1];
    let mut len: u32 = 0;
    match get(empty.as_mut_ptr(), &mut len) {
        ERROR_SUCCESS => return Some(String::new()),
        ERROR_MORE_DATA => {}
        _ => return None,
    }
    let mut buf = vec![0u16; len as usize + 1];
    let mut cap = len + 1;
    if get(buf.as_mut_ptr(), &mut cap) != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(cap as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// A property's value; empty when it is unset or cannot be read.
fn property(install: MSIHANDLE, name: &str) -> String {
    let name = wide(name);
    // SAFETY: `name` is null-terminated and outlives the calls; `msi_string`
    // hands each one a buffer and its length in characters.
    msi_string(|buf, len| unsafe { MsiGetPropertyW(install, name.as_ptr(), buf, len) })
        .unwrap_or_default()
}

fn set_property(install: MSIHANDLE, name: &str, value: &str) -> u32 {
    let (name, value) = (wide(name), wide(value));
    // SAFETY: both strings are null-terminated and outlive the call.
    unsafe { MsiSetPropertyW(install, name.as_ptr(), value.as_ptr()) }
}

/// The service's `Environment` entries: `Ok(None)` when the service or the
/// value is not there, `Err` with the Win32 code for anything else (a value of
/// another type among them).
fn read_environment() -> Result<Option<Vec<String>>, u32> {
    let (key, value) = (wide(SERVICE_KEY), wide(ENV_VALUE));
    let flags = RRF_RT_REG_MULTI_SZ;
    let mut bytes: u32 = 0;
    // SAFETY: the sizing call (null buffer), then a buffer of the size it
    // answered; a value that grew in between answers ERROR_MORE_DATA and is
    // asked again.
    unsafe {
        for _ in 0..3 {
            let rc = RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut bytes,
            );
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if rc != ERROR_SUCCESS {
                return Err(rc);
            }
            let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
            let rc = RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            );
            match rc {
                ERROR_SUCCESS => {
                    buf.truncate(bytes as usize / 2);
                    return Ok(Some(carry::split_multi_sz(&buf)));
                }
                ERROR_MORE_DATA => continue,
                ERROR_FILE_NOT_FOUND => return Ok(None),
                other => return Err(other),
            }
        }
    }
    Err(ERROR_MORE_DATA)
}

/// Write the service's `Environment`. The key must exist: a service that
/// `RegisterService` did not register is not created here as a bare key
/// (`RegSetKeyValueW` would create one), which the SCM would then hold
/// against the next `CreateService`.
fn write_environment(entries: &[String]) -> Result<(), u32> {
    let (key, value) = (wide(SERVICE_KEY), wide(ENV_VALUE));
    let data = carry::join_multi_sz(entries);
    let mut open: HKEY = ptr::null_mut();
    // SAFETY: an open of an existing key for KEY_SET_VALUE, closed on every
    // path; `data` is a complete REG_MULTI_SZ, passed with its byte length.
    unsafe {
        let rc = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut open,
        );
        if rc != ERROR_SUCCESS {
            return Err(rc);
        }
        let rc = RegSetValueExW(
            open,
            value.as_ptr(),
            0,
            REG_MULTI_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        );
        RegCloseKey(open);
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(rc) }
    }
}

fn names(entries: &[String]) -> String {
    entries
        .iter()
        .map(|e| carry::name_of(e))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `body`, and report a panic in it as a logged miss (`missed` says what
/// was not done): no panic may cross into msiexec, and no failure here may
/// fail the install.
fn guarded(install: MSIHANDLE, action: &str, missed: &str, body: impl FnOnce() -> u32) -> u32 {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(rc) => rc,
        Err(_) => {
            log(install, &format!("{action}: stopped by a panic; {missed}"));
            ERROR_SUCCESS
        }
    }
}

fn capture(install: MSIHANDLE) -> u32 {
    let forced = property(install, carry::FAIL_PROPERTY);
    if forced.eq_ignore_ascii_case("capture") {
        log(
            install,
            "capture: failing on purpose (ROOMLER_TEST_CA_FAIL=capture)",
        );
        return ERROR_INSTALL_FAILURE;
    }
    if forced.eq_ignore_ascii_case("restore") {
        set_property(
            install,
            carry::CARRY_PROPERTY,
            carry::FORCED_RESTORE_FAILURE,
        );
        log(
            install,
            "capture: the restore will fail on purpose (ROOMLER_TEST_CA_FAIL=restore)",
        );
        return ERROR_SUCCESS;
    }
    let entries = match read_environment() {
        Ok(Some(entries)) => entries,
        Ok(None) => {
            log(
                install,
                "capture: the old service has no Environment; nothing to carry",
            );
            return ERROR_SUCCESS;
        }
        Err(code) => {
            log(
                install,
                &format!(
                    "capture: reading the old service's Environment failed (error {code}); nothing carried"
                ),
            );
            return ERROR_SUCCESS;
        }
    };
    let (entries, left_out) = carry::carriable(entries);
    if !left_out.is_empty() {
        log(
            install,
            &format!(
                "capture: leaving out {} entries with no '=' after the name: {}",
                left_out.len(),
                left_out.join(" ")
            ),
        );
    }
    if entries.is_empty() {
        log(install, "capture: nothing left to carry");
        return ERROR_SUCCESS;
    }
    let value = carry::encode(&entries);
    if value.len() > carry::MAX_CARRIED {
        log(
            install,
            &format!(
                "capture: {} entries are {} bytes carried, over the {}-byte limit; nothing carried",
                entries.len(),
                value.len(),
                carry::MAX_CARRIED
            ),
        );
        return ERROR_SUCCESS;
    }
    let rc = set_property(install, carry::CARRY_PROPERTY, &value);
    if rc != ERROR_SUCCESS {
        log(
            install,
            &format!("capture: setting the carry property failed (error {rc}); nothing carried"),
        );
        return ERROR_SUCCESS;
    }
    log(
        install,
        &format!(
            "capture: carrying {} entries: {}",
            entries.len(),
            names(&entries)
        ),
    );
    ERROR_SUCCESS
}

fn restore(install: MSIHANDLE) -> u32 {
    let value = property(install, "CustomActionData");
    if value == carry::FORCED_RESTORE_FAILURE {
        log(
            install,
            "restore: failing on purpose (ROOMLER_TEST_CA_FAIL=restore)",
        );
        return ERROR_INSTALL_FAILURE;
    }
    let carried = match carry::decode(&value) {
        Ok(carried) => carried,
        Err(why) => {
            log(
                install,
                &format!("restore: the carried value is unreadable ({why}); nothing restored"),
            );
            return ERROR_SUCCESS;
        }
    };
    let current = match read_environment() {
        Ok(current) => current.unwrap_or_default(),
        Err(code) => {
            log(
                install,
                &format!(
                    "restore: reading the new service's Environment failed (error {code}); nothing restored"
                ),
            );
            return ERROR_SUCCESS;
        }
    };
    let (merged, restored) = carry::merge(current, &carried);
    if restored.is_empty() {
        log(
            install,
            "restore: nothing to put back but the installer's own switch",
        );
        return ERROR_SUCCESS;
    }
    match write_environment(&merged) {
        Ok(()) => log(
            install,
            &format!(
                "restore: put back {} entries: {}",
                restored.len(),
                restored.join(" ")
            ),
        ),
        Err(ERROR_FILE_NOT_FOUND) => log(
            install,
            "restore: the new service is not registered (error 2); nothing restored",
        ),
        Err(code) => log(
            install,
            &format!(
                "restore: writing the new service's Environment failed (error {code}); nothing restored"
            ),
        ),
    }
    ERROR_SUCCESS
}

/// #3375 — immediate, before `InstallInitialize`, on an upgrade: the old
/// service's `Environment` into [`carry::CARRY_PROPERTY`]. msiexec calls it
/// with the install session's handle.
#[unsafe(no_mangle)]
pub extern "system" fn CaptureServiceEnvironment(install: MSIHANDLE) -> u32 {
    guarded(install, "capture", "nothing carried", || capture(install))
}

/// #3375 — deferred, as LocalSystem, after `RegisterService` and before the
/// SystemContext actions: the carried entries back into the new service's
/// `Environment`, all but the installer's own ([`carry::merge`]). msiexec
/// calls it with the install session's handle; the value arrives as
/// `CustomActionData`, the only property a deferred action can read.
#[unsafe(no_mangle)]
pub extern "system" fn RestoreServiceEnvironment(install: MSIHANDLE) -> u32 {
    guarded(install, "restore", "nothing carried", || restore(install))
}

/// An MSI handle, closed when it goes.
struct Handle(MSIHANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: a handle this module opened and nobody else closes.
            unsafe { MsiCloseHandle(self.0) };
        }
    }
}

/// The full path of every file whose component is on this machine once the
/// install ends ([`pending_ops::stays_local`]), and how many rows could not be
/// read. Valid from `CostFinalize`, final from `InstallValidate`.
fn installed_files(install: MSIHANDLE) -> Result<(Vec<String>, usize), String> {
    let query = wide(pending_ops::FILES_QUERY);
    // SAFETY: MSI handle calls on null-terminated strings that outlive them;
    // every handle is closed by its guard, the view's after `MsiViewClose`.
    unsafe {
        let db = Handle(MsiGetActiveDatabase(install));
        if db.0 == 0 {
            return Err("the install has no active database".to_string());
        }
        let mut view: MSIHANDLE = 0;
        let rc = MsiDatabaseOpenViewW(db.0, query.as_ptr(), &mut view);
        if rc != ERROR_SUCCESS {
            return Err(format!("opening the File table failed (error {rc})"));
        }
        let view = Handle(view);
        let rc = MsiViewExecute(view.0, 0);
        if rc != ERROR_SUCCESS {
            return Err(format!("reading the File table failed (error {rc})"));
        }
        let (mut files, mut unread) = (Vec::new(), 0usize);
        let outcome = loop {
            let mut row: MSIHANDLE = 0;
            match MsiViewFetch(view.0, &mut row) {
                ERROR_SUCCESS => {}
                ERROR_NO_MORE_ITEMS => break Ok(()),
                rc => break Err(format!("reading the File table failed (error {rc})")),
            }
            let row = Handle(row);
            let field = |i: u32| msi_string(|buf, len| MsiRecordGetStringW(row.0, i, buf, len));
            let (Some(name), Some(component), Some(directory)) = (field(1), field(2), field(3))
            else {
                unread += 1;
                continue;
            };
            let component = wide(&component);
            let (mut installed, mut action): (INSTALLSTATE, INSTALLSTATE) = (0, 0);
            if MsiGetComponentStateW(install, component.as_ptr(), &mut installed, &mut action)
                != ERROR_SUCCESS
            {
                unread += 1;
                continue;
            }
            if !pending_ops::stays_local(installed, action) {
                continue;
            }
            let directory = wide(&directory);
            match msi_string(|buf, len| MsiGetTargetPathW(install, directory.as_ptr(), buf, len)) {
                Some(dir) if !dir.is_empty() => {
                    files.push(pending_ops::file_path(&dir, pending_ops::long_name(&name)))
                }
                _ => unread += 1,
            }
        };
        MsiViewClose(view.0);
        outcome.map(|()| (files, unread))
    }
}

fn list(install: MSIHANDLE) -> u32 {
    let (files, unread) = match installed_files(install) {
        Ok(found) => found,
        Err(why) => {
            log(install, &format!("list: {why}; nothing will be spared"));
            return ERROR_SUCCESS;
        }
    };
    if unread > 0 {
        log(
            install,
            &format!(
                "list: {unread} rows of the File table could not be read; they are not spared"
            ),
        );
    }
    // The deferred action's CustomActionData: the only property it can read.
    let rc = set_property(
        install,
        pending_ops::SPARE_ACTION,
        &pending_ops::join_list(&files),
    );
    if rc != ERROR_SUCCESS {
        log(
            install,
            &format!("list: handing the files on failed (error {rc}); nothing will be spared"),
        );
        return ERROR_SUCCESS;
    }
    log(
        install,
        &format!("list: {} files stay installed", files.len()),
    );
    ERROR_SUCCESS
}

/// The Session Manager key, open to read and write its values.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key this module opened and nobody else closes.
        unsafe { RegCloseKey(self.0) };
    }
}

/// A value as it is stored, with no repair of its nulls (`RegGetValueW` adds
/// a missing one, so it is not used here).
enum Stored {
    Absent,
    /// Not a `REG_MULTI_SZ`, or an odd number of bytes: never touched.
    Foreign(String),
    Units(Vec<u16>),
}

fn read_stored(key: &Key, name: &[u16]) -> Result<Stored, u32> {
    // SAFETY: the sizing call (null buffer), then a buffer of the size it
    // answered; a value that grew in between answers ERROR_MORE_DATA and is
    // asked again.
    unsafe {
        for _ in 0..3 {
            let (mut kind, mut bytes): (REG_VALUE_TYPE, u32) = (0, 0);
            let rc = RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut bytes,
            );
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(Stored::Absent);
            }
            if rc != ERROR_SUCCESS {
                return Err(rc);
            }
            let mut buf = vec![0u8; bytes as usize];
            let rc = RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                buf.as_mut_ptr(),
                &mut bytes,
            );
            match rc {
                ERROR_SUCCESS => {}
                ERROR_MORE_DATA => continue,
                ERROR_FILE_NOT_FOUND => return Ok(Stored::Absent),
                other => return Err(other),
            }
            buf.truncate(bytes as usize);
            if kind != REG_MULTI_SZ {
                return Ok(Stored::Foreign(format!("it is of type {kind}")));
            }
            if !buf.len().is_multiple_of(2) {
                return Ok(Stored::Foreign(format!(
                    "it holds an odd {} bytes",
                    buf.len()
                )));
            }
            return Ok(Stored::Units(
                buf.chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect(),
            ));
        }
    }
    Err(ERROR_MORE_DATA)
}

/// Write a value back, or delete it when no operation is left.
fn write_stored(key: &Key, name: &[u16], units: &[u16]) -> u32 {
    // SAFETY: `units` is a complete REG_MULTI_SZ, passed with its byte length.
    unsafe {
        if units.is_empty() {
            RegDeleteValueW(key.0, name.as_ptr())
        } else {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_MULTI_SZ,
                units.as_ptr().cast(),
                (units.len() * 2) as u32,
            )
        }
    }
}

/// Take this install's files out of one value's pending deletes, and say so.
fn spare_in(install: MSIHANDLE, key: &Key, value: &str, listed: &[String], elsewhere: &[String]) {
    let name = wide(value);
    // Another program may add an operation at any time: the value is written
    // only over what was read, and otherwise read and planned again.
    for _ in 0..3 {
        let stored = match read_stored(key, &name) {
            Ok(Stored::Units(units)) => units,
            Ok(Stored::Absent) => {
                log(install, &format!("spare: {value} is not set"));
                return;
            }
            Ok(Stored::Foreign(why)) => {
                log(install, &format!("spare: {value} is left as it is: {why}"));
                return;
            }
            Err(rc) => {
                log(
                    install,
                    &format!("spare: reading {value} failed (error {rc}); it is left as it is"),
                );
                return;
            }
        };
        let plan = match pending_ops::plan(&stored, listed, elsewhere) {
            Ok(plan) => plan,
            Err(why) => {
                log(install, &format!("spare: {value} is left as it is: {why}"));
                return;
            }
        };
        let Some(rewritten) = plan.rewritten else {
            for kept in &plan.replaced {
                log(
                    install,
                    &format!(
                        "spare: kept the delete of {kept} in {value}: a later operation puts a file there"
                    ),
                );
            }
            log(
                install,
                &format!(
                    "spare: {value} holds no delete of a file that stays ({} operations)",
                    plan.left
                ),
            );
            return;
        };
        match read_stored(key, &name) {
            Ok(Stored::Units(now)) if now == stored => {}
            _ => continue,
        }
        let rc = write_stored(key, &name, &rewritten);
        if rc != ERROR_SUCCESS {
            log(
                install,
                &format!("spare: writing {value} failed (error {rc}); its deletes stay"),
            );
            return;
        }
        for kept in &plan.replaced {
            log(
                install,
                &format!(
                    "spare: kept the delete of {kept} in {value}: a later operation puts a file there"
                ),
            );
        }
        for dropped in &plan.dropped {
            log(
                install,
                &format!("spare: dropped the delete of {dropped} from {value}"),
            );
        }
        if rewritten.is_empty() {
            log(
                install,
                &format!("spare: {value} deleted: no operation was left"),
            );
        } else {
            log(
                install,
                &format!(
                    "spare: {value} keeps its other {} operations as they were",
                    plan.left
                ),
            );
        }
        return;
    }
    log(
        install,
        &format!("spare: {value} changed while it was read, three times; its deletes stay"),
    );
}

fn spare(install: MSIHANDLE) -> u32 {
    let listed = pending_ops::split_list(&property(install, "CustomActionData"));
    if listed.is_empty() {
        log(install, "spare: no file was listed; nothing to check");
        return ERROR_SUCCESS;
    }
    let path = wide(pending_ops::SESSION_MANAGER);
    let mut open: HKEY = ptr::null_mut();
    // SAFETY: an open of an existing key, closed by its guard.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut open,
        )
    };
    if rc != ERROR_SUCCESS {
        log(
            install,
            &format!("spare: opening the Session Manager key failed (error {rc}); nothing spared"),
        );
        return ERROR_SUCCESS;
    }
    let key = Key(open);
    log(
        install,
        &format!(
            "spare: checking the boot-time deletes for {} files that stay",
            listed.len()
        ),
    );
    // Which of the two values the session manager runs first is not ours to
    // know, so a rename onto a file in either one keeps a delete in the other.
    let renamed = pending_ops::VALUES.map(|value| match read_stored(&key, &wide(value)) {
        Ok(Stored::Units(units)) => pending_ops::renamed_onto(&units),
        _ => Vec::new(),
    });
    for (at, value) in pending_ops::VALUES.iter().enumerate() {
        spare_in(install, &key, value, &listed, &renamed[1 - at]);
    }
    ERROR_SUCCESS
}

/// #4196 — immediate, after `InstallValidate`, on every run but an uninstall:
/// the full path of every file that stays installed when this run ends, handed
/// to [`SpareInstalledFiles`] as its `CustomActionData`. msiexec calls it with
/// the install session's handle.
#[unsafe(no_mangle)]
pub extern "system" fn ListInstalledFiles(install: MSIHANDLE) -> u32 {
    guarded(install, "list", "nothing will be spared", || list(install))
}

/// #4196 — deferred, as LocalSystem, after `InstallFiles`, on every run but an
/// uninstall: takes the boot-time deletes of the listed files out of the
/// session manager's pending operations ([`pending_ops::plan`]), whoever held
/// the file when Windows Installer scheduled them. msiexec calls it with the
/// install session's handle.
#[unsafe(no_mangle)]
pub extern "system" fn SpareInstalledFiles(install: MSIHANDLE) -> u32 {
    guarded(install, "spare", "nothing spared", || spare(install))
}
