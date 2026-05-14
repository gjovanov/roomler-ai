//! Detect existing `roomler-agent` installs on a Windows host.
//!
//! Used by the rc.28 installation wizard (and any future CLI tooling)
//! to decide whether the operator is doing a clean install, a
//! same-flavour upgrade (preserves enrollment), or a cross-flavour
//! switch (wipes `%APPDATA%\roomler\roomler-agent\config.toml` —
//! operator needs a fresh enrollment token).
//!
//! Probes the Windows registry directly via the packed-UpgradeCode
//! subkey under `HKLM\SOFTWARE\Classes\Installer\UpgradeCodes\` (per-
//! machine) and the HKCU variant (per-user). Does NOT use
//! [`crate::updater::current_install_flavour`]: that classifies the
//! *running* EXE, which from inside a wizard binary running from
//! `%TEMP%` is always `PerUser` regardless of host install state.
//!
//! Linux / macOS callers get `ExistingInstall::Clean` unconditionally
//! — the installer wizard is Windows-only for v1.

use std::path::PathBuf;

pub mod msi_guid;
pub mod upgrade_codes;

pub use msi_guid::{MsiGuidError, pack_msi_guid, unpack_msi_guid};
pub use upgrade_codes::{PERMACHINE_UPGRADE_CODE, PERUSER_UPGRADE_CODE};

/// Best-effort metadata about a detected install. Fields are
/// `Option` because the registry probe may find the UpgradeCode key
/// (proving an install exists) without resolving every Uninstall
/// value (corrupted install, key ACL'd, etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallInfo {
    /// `DisplayVersion` from the matching Uninstall subkey.
    pub version: Option<String>,
    /// `InstallLocation` from the matching Uninstall subkey.
    pub install_location: Option<PathBuf>,
}

/// What the wizard found when probing the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingInstall {
    /// No prior install detected (neither HKCU perUser nor HKLM
    /// perMachine UpgradeCode keys present).
    Clean,
    /// perUser MSI installed; HKLM has no roomler-agent UpgradeCode.
    PerUser(InstallInfo),
    /// perMachine MSI installed; HKCU has no roomler-agent UpgradeCode.
    PerMachine(InstallInfo),
    /// BOTH flavours appear installed simultaneously. Should never
    /// happen post-rc.18 (cross-flavour cleanup custom actions scrub
    /// the OTHER flavour before InstallFiles), but if a pre-rc.18
    /// install slipped through, surface the ambiguity so the operator
    /// can decide which to keep.
    Ambiguous {
        peruser: InstallInfo,
        permachine: InstallInfo,
    },
}

/// Internal probe result: what each hive yielded independently.
/// Public-in-crate so the pure [`decide_from_probe`] is unit-testable
/// on any platform by constructing this directly. Marked
/// `allow(dead_code)` on non-Windows because the IO path is gated to
/// Windows; tests still exercise it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct InstallProbe {
    pub peruser: Option<InstallInfo>,
    pub permachine: Option<InstallInfo>,
}

/// Combine the two hive-probe results into the public-facing enum.
/// Pure function, no IO, runs on any platform.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn decide_from_probe(probe: InstallProbe) -> ExistingInstall {
    match (probe.peruser, probe.permachine) {
        (None, None) => ExistingInstall::Clean,
        (Some(u), None) => ExistingInstall::PerUser(u),
        (None, Some(m)) => ExistingInstall::PerMachine(m),
        (Some(u), Some(m)) => ExistingInstall::Ambiguous {
            peruser: u,
            permachine: m,
        },
    }
}

/// Probe the Windows registry for existing roomler-agent installs.
///
/// On non-Windows platforms returns `ExistingInstall::Clean`
/// unconditionally — the installer wizard is Windows-only for v1.
pub fn detect_existing_install() -> ExistingInstall {
    #[cfg(target_os = "windows")]
    {
        decide_from_probe(windows::probe())
    }
    #[cfg(not(target_os = "windows"))]
    {
        ExistingInstall::Clean
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{InstallInfo, InstallProbe};
    use crate::install_detect::{
        PERMACHINE_UPGRADE_CODE, PERUSER_UPGRADE_CODE, pack_msi_guid, unpack_msi_guid,
    };
    use std::path::PathBuf;
    use std::ptr;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, REG_EXPAND_SZ,
        REG_SZ, RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
    };

    /// Convert a Rust `&str` into a NUL-terminated UTF-16 buffer
    /// suitable for the wide-string Win32 APIs.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// RAII wrapper so we never leak HKEY handles on early-return.
    struct OpenKey(HKEY);
    impl Drop for OpenKey {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: handle was returned by RegOpenKeyExW.
                unsafe { RegCloseKey(self.0) };
            }
        }
    }

    fn open_subkey_read(hkey: HKEY, path: &str) -> Option<OpenKey> {
        let wpath = wide(path);
        let mut out: HKEY = ptr::null_mut();
        // SAFETY: wpath is NUL-terminated and lives through the call.
        let rc = unsafe {
            RegOpenKeyExW(
                hkey,
                wpath.as_ptr(),
                0,
                KEY_READ | KEY_WOW64_64KEY,
                &mut out,
            )
        };
        if rc == ERROR_SUCCESS && !out.is_null() {
            Some(OpenKey(out))
        } else {
            None
        }
    }

    /// Return the FIRST value name under the given open key. The
    /// values under `UpgradeCodes\<packed>` are packed-form ProductCodes;
    /// the first one is the currently-active install.
    fn first_value_name(key: &OpenKey) -> Option<String> {
        let mut name_buf = vec![0u16; 256];
        let mut name_len: u32 = name_buf.len() as u32;
        // SAFETY: the buffer length is in-out per RegEnumValueW docs;
        // we pass valid pointers + correct sizes.
        let rc = unsafe {
            RegEnumValueW(
                key.0,
                0,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        Some(String::from_utf16_lossy(&name_buf[..name_len as usize]))
    }

    /// Read a `REG_SZ` or `REG_EXPAND_SZ` value from an open key.
    fn read_string_value(key: &OpenKey, name: &str) -> Option<String> {
        let wname = wide(name);
        let mut buf_len: u32 = 0;
        let mut value_type: u32 = 0;
        // First call sizes the buffer.
        // SAFETY: standard double-call pattern for RegQueryValueExW.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                wname.as_ptr(),
                ptr::null_mut(),
                &mut value_type,
                ptr::null_mut(),
                &mut buf_len,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
            return None;
        }
        let mut buf = vec![0u8; buf_len as usize];
        // SAFETY: buf has buf_len bytes; we pass the exact size back.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                wname.as_ptr(),
                ptr::null_mut(),
                &mut value_type,
                buf.as_mut_ptr(),
                &mut buf_len,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        // Registry returns UTF-16-LE wide chars; trim trailing NULs.
        let wide_chars: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&w| w != 0)
            .collect();
        Some(String::from_utf16_lossy(&wide_chars))
    }

    /// Look up version + install location for one flavour. Returns
    /// `None` when the UpgradeCode subkey is missing entirely; returns
    /// `Some(InstallInfo::default())` when the subkey exists but the
    /// Uninstall lookup fails (still a valid "is installed" signal).
    fn probe_one(
        hive: HKEY,
        upgrade_codes_subpath: &str,
        packed_upgrade_code: &str,
        uninstall_subpath: &str,
    ) -> Option<InstallInfo> {
        let upgrade_path = format!("{upgrade_codes_subpath}\\{packed_upgrade_code}");
        let upgrade_key = open_subkey_read(hive, &upgrade_path)?;

        let Some(packed_product) = first_value_name(&upgrade_key) else {
            // UpgradeCode key exists but is empty — treat as installed
            // but with no version metadata.
            return Some(InstallInfo::default());
        };
        drop(upgrade_key);

        let Ok(canonical_product) = unpack_msi_guid(&packed_product) else {
            return Some(InstallInfo::default());
        };

        let uninstall_path = format!("{uninstall_subpath}\\{canonical_product}");
        let Some(uninstall_key) = open_subkey_read(hive, &uninstall_path) else {
            return Some(InstallInfo::default());
        };

        Some(InstallInfo {
            version: read_string_value(&uninstall_key, "DisplayVersion"),
            install_location: read_string_value(&uninstall_key, "InstallLocation")
                .map(PathBuf::from),
        })
    }

    pub(super) fn probe() -> InstallProbe {
        // Errors from pack_msi_guid here are impossible because our
        // constants are validated by the WiX parity tests; but treat
        // an error as "not installed" so we never panic on weird input.
        let peruser = pack_msi_guid(PERUSER_UPGRADE_CODE).ok().and_then(|packed| {
            probe_one(
                HKEY_CURRENT_USER,
                r"Software\Microsoft\Installer\UpgradeCodes",
                &packed,
                r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
            )
        });
        let permachine = pack_msi_guid(PERMACHINE_UPGRADE_CODE)
            .ok()
            .and_then(|packed| {
                probe_one(
                    HKEY_LOCAL_MACHINE,
                    r"SOFTWARE\Classes\Installer\UpgradeCodes",
                    &packed,
                    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
                )
            });

        InstallProbe {
            peruser,
            permachine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: &str) -> InstallInfo {
        InstallInfo {
            version: Some(version.to_string()),
            install_location: None,
        }
    }

    #[test]
    fn clean_when_both_hives_empty() {
        let result = decide_from_probe(InstallProbe::default());
        assert_eq!(result, ExistingInstall::Clean);
    }

    #[test]
    fn peruser_when_only_hkcu_hit() {
        let result = decide_from_probe(InstallProbe {
            peruser: Some(info("0.3.0-rc.26")),
            permachine: None,
        });
        assert_eq!(result, ExistingInstall::PerUser(info("0.3.0-rc.26")));
    }

    #[test]
    fn permachine_when_only_hklm_hit() {
        let result = decide_from_probe(InstallProbe {
            peruser: None,
            permachine: Some(info("0.3.0-rc.26")),
        });
        assert_eq!(result, ExistingInstall::PerMachine(info("0.3.0-rc.26")));
    }

    #[test]
    fn ambiguous_when_both_hit() {
        let result = decide_from_probe(InstallProbe {
            peruser: Some(info("0.3.0-rc.18")),
            permachine: Some(info("0.3.0-rc.26")),
        });
        assert_eq!(
            result,
            ExistingInstall::Ambiguous {
                peruser: info("0.3.0-rc.18"),
                permachine: info("0.3.0-rc.26"),
            }
        );
    }

    #[test]
    fn install_info_with_missing_version_still_counts_as_installed() {
        let result = decide_from_probe(InstallProbe {
            peruser: Some(InstallInfo::default()),
            permachine: None,
        });
        // Default has version=None, install_location=None — still
        // counts as "installed" because the UpgradeCode key was found.
        assert_eq!(result, ExistingInstall::PerUser(InstallInfo::default()));
    }
}
