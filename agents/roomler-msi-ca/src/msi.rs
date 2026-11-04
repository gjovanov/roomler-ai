// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The two entry points msiexec calls, and the registry and MSI calls under
//! them. Every path returns success unless installer-smoke forced a failure
//! ([`carry::FAIL_PROPERTY`]): what went wrong is logged, by name, and the
//! upgrade goes on with nothing carried, as every upgrade did before #3375.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSTALL_FAILURE, ERROR_MORE_DATA, ERROR_SUCCESS,
};
use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
    INSTALLMESSAGE_INFO, MSIHANDLE, MsiCloseHandle, MsiCreateRecord, MsiGetPropertyW,
    MsiProcessMessage, MsiRecordSetStringW, MsiSetPropertyW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_MULTI_SZ, RRF_RT_REG_MULTI_SZ, RegCloseKey,
    RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};

use crate::carry;

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

/// A property's value; empty when it is unset or cannot be read.
fn property(install: MSIHANDLE, name: &str) -> String {
    let name = wide(name);
    let mut empty = [0u16; 1];
    let mut len: u32 = 0;
    // SAFETY: the double call MsiGetPropertyW documents: the first, with a
    // zero-length buffer, answers ERROR_MORE_DATA and the length without its
    // null; the second fills a buffer one longer.
    unsafe {
        let rc = MsiGetPropertyW(install, name.as_ptr(), empty.as_mut_ptr(), &mut len);
        if rc == ERROR_SUCCESS {
            return String::new();
        }
        if rc != ERROR_MORE_DATA {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let mut cap = len + 1;
        if MsiGetPropertyW(install, name.as_ptr(), buf.as_mut_ptr(), &mut cap) != ERROR_SUCCESS {
            return String::new();
        }
        buf.truncate(cap as usize);
        String::from_utf16_lossy(&buf)
    }
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

/// Run `body`, and report a panic in it as a logged miss: no panic may cross
/// into msiexec, and no failure here may fail the upgrade.
fn guarded(install: MSIHANDLE, action: &str, body: impl FnOnce() -> u32) -> u32 {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(rc) => rc,
        Err(_) => {
            log(
                install,
                &format!("{action}: stopped by a panic; nothing carried"),
            );
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
    guarded(install, "capture", || capture(install))
}

/// #3375 — deferred, as LocalSystem, after `RegisterService` and before the
/// SystemContext actions: the carried entries back into the new service's
/// `Environment`, all but the installer's own ([`carry::merge`]). msiexec
/// calls it with the install session's handle; the value arrives as
/// `CustomActionData`, the only property a deferred action can read.
#[unsafe(no_mangle)]
pub extern "system" fn RestoreServiceEnvironment(install: MSIHANDLE) -> u32 {
    guarded(install, "restore", || restore(install))
}
