// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! What is carried across an upgrade and how it comes back: pure, so it is
//! tested on every platform.
//!
//! An entry is kept as the service's `Environment` holds it, `NAME=VALUE`, so
//! a round trip changes nothing. The entries travel in an MSI property, which
//! cannot hold a null and whose formatting reads `[` and `{`. So they are
//! joined with `|` after a version tag, with `%`, `|`, `[`, `]`, `{`, `}` and
//! control characters percent-escaped.

/// The private property the capture sets. A private name (it has lowercase
/// letters) cannot be set from the msiexec command line.
pub const CARRY_PROPERTY: &str = "RoomlerServiceEnvironment";

/// The deferred restore's action id, which is also the name its
/// `CustomActionData` property is logged under: the MSI hides both.
pub const RESTORE_ACTION: &str = "RestoreServiceEnvironment";

/// The immediate capture's action id.
pub const CAPTURE_ACTION: &str = "CaptureServiceEnvironment";

/// installer-smoke's forced failure: `capture` fails the capture, `restore`
/// the restore, each with 1603, to prove that `Return="ignore"` carries the
/// upgrade through a failing action. Its only effect is that nothing is
/// carried, which is what every upgrade did before #3375.
pub const FAIL_PROPERTY: &str = "ROOMLER_TEST_CA_FAIL";

/// What the capture carries when [`FAIL_PROPERTY`] asks the restore to fail.
/// It is no tagged value, so no real capture can produce it.
pub const FORCED_RESTORE_FAILURE: &str = "fail:restore";

/// The entries the installer writes itself, under every spelling the daemon
/// reads (`tunnel_core::env::PREFIXES`): the SystemContext switch. After an
/// upgrade the SystemContext action that runs after the restore decides it,
/// so a carried value never overrides that decision.
pub const INSTALLER_OWNED: [&str; 2] = [
    "ROOMLERD_ENABLE_SYSTEM_SWAP",
    "ROOMLER_NODE_ENABLE_SYSTEM_SWAP",
];

/// The tag in front of a carried value. A restore refuses any other.
const TAG: &str = "v1:";

/// A larger value is not carried at all. No operator wrote an environment
/// this size by hand, and the property travels through the MSI's script.
pub const MAX_CARRIED: usize = 64 * 1024;

/// Why a carried value could not be read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarryError {
    /// It does not start with the version tag.
    Untagged,
    /// Larger than [`MAX_CARRIED`].
    TooLarge,
    /// A `%` not followed by two hex digits.
    BadEscape,
    /// The unescaped bytes are not UTF-8.
    NotUtf8,
    /// An empty entry, or one with no `=`.
    NotAnEntry,
}

impl std::fmt::Display for CarryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CarryError::Untagged => "it does not start with the version tag",
            CarryError::TooLarge => "it is larger than the carry limit",
            CarryError::BadEscape => "a % is not followed by two hex digits",
            CarryError::NotUtf8 => "an entry is not UTF-8",
            CarryError::NotAnEntry => "an entry is empty or has no '='",
        })
    }
}

/// The name an entry sets: up to its first `=`, past a leading one (Windows
/// keeps per-drive entries as `=C:=C:\`).
pub fn name_of(entry: &str) -> &str {
    let from = usize::from(entry.starts_with('='));
    match entry[from..].find('=') {
        Some(i) => &entry[..from + i],
        None => entry,
    }
}

/// Whether the installer decides `name` itself ([`INSTALLER_OWNED`]). Windows
/// matches environment names without regard to case, and so does this.
pub fn installer_owned(name: &str) -> bool {
    INSTALLER_OWNED.iter().any(|o| o.eq_ignore_ascii_case(name))
}

/// The entries of a `REG_MULTI_SZ`, as UTF-16 code units: one per string,
/// empty strings (the terminators) dropped.
pub fn split_multi_sz(wide: &[u16]) -> Vec<String> {
    wide.split(|&w| w == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// `entries` as a `REG_MULTI_SZ`: each string with its null, then the
/// closing one. The SCM refuses to start a service whose `Environment` is an
/// empty `REG_MULTI_SZ` (error 87, `agents/roomlerd/src/win_service/environment.rs`),
/// so this is never written for an empty list.
pub fn join_multi_sz(entries: &[String]) -> Vec<u16> {
    debug_assert!(
        !entries.is_empty(),
        "an empty Environment is deleted, never written"
    );
    let mut wide = Vec::new();
    for entry in entries {
        wide.extend(entry.encode_utf16());
        wide.push(0);
    }
    wide.push(0);
    wide
}

fn escaped(c: char) -> bool {
    matches!(c, '%' | '|' | '[' | ']' | '{' | '}') || c.is_control()
}

/// The property value that carries `entries`. Empty when there is nothing to
/// carry, so the restore's condition is false.
pub fn encode(entries: &[String]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from(TAG);
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.push('|');
        }
        for c in entry.chars() {
            if escaped(c) {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            } else {
                out.push(c);
            }
        }
    }
    out
}

/// The entries [`encode`] carried. Empty for an empty value.
pub fn decode(value: &str) -> Result<Vec<String>, CarryError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    if value.len() > MAX_CARRIED {
        return Err(CarryError::TooLarge);
    }
    let body = value.strip_prefix(TAG).ok_or(CarryError::Untagged)?;
    body.split('|').map(unescape).collect()
}

fn unescape(escaped: &str) -> Result<String, CarryError> {
    let bytes = escaped.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or(CarryError::BadEscape)?;
            // `from_str_radix` would take a sign: "+F" is 15.
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return Err(CarryError::BadEscape);
            }
            let hex = std::str::from_utf8(hex).map_err(|_| CarryError::BadEscape)?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| CarryError::BadEscape)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let entry = String::from_utf8(out).map_err(|_| CarryError::NotUtf8)?;
    if name_of(&entry).len() == entry.len() || name_of(&entry).is_empty() {
        return Err(CarryError::NotAnEntry);
    }
    Ok(entry)
}

/// The new registration's entries with the carried ones back in. Each carried
/// entry comes back as it was, in place of one with the same name, except the
/// [`INSTALLER_OWNED`] ones, which keep the installer's value. Returns the
/// merged list and the names put back, in order.
pub fn merge(current: Vec<String>, carried: &[String]) -> (Vec<String>, Vec<String>) {
    let mut merged = current;
    let mut restored = Vec::new();
    for entry in carried {
        let name = name_of(entry);
        if installer_owned(name) {
            continue;
        }
        match merged
            .iter_mut()
            .find(|e| name_of(e).eq_ignore_ascii_case(name))
        {
            Some(slot) => slot.clone_from(entry),
            None => merged.push(entry.clone()),
        }
        restored.push(name.to_string());
    }
    (merged, restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The kill switch and the feed URL the issue lost, and every character
    /// the carry escapes, come back byte for byte.
    #[test]
    fn what_is_carried_comes_back_as_it_was() {
        let block = entries(&[
            "ROOMLERD_ENABLE_SYSTEM_SWAP=1",
            "ROOMLERD_SFTP_AS_USER=0",
            "ROOMLERD_UPDATE_URL=http://127.0.0.1:18080/releases",
            "RUST_LOG=info,roomlerd[span]=debug,{x}=trace",
            "ROOMLERD_ODD=100%|a|b|",
            "ROOMLERD_TAB=a\tb",
            "ROOMLERD_EMPTY=",
            "ROOMLERD_EQ==x=y",
            "=C:=C:\\Windows",
            "ROOMLERD_WIDE=caf\u{e9} \u{2603} \u{1F600} \u{85}",
        ]);
        let carried = encode(&block);
        assert!(carried.starts_with(TAG), "{carried}");
        for c in ['[', ']', '{', '}', '\t', '\u{85}'] {
            assert!(!carried.contains(c), "{c:?} escaped: {carried}");
        }
        assert_eq!(decode(&carried), Ok(block));
    }

    #[test]
    fn nothing_to_carry_is_an_empty_value() {
        assert_eq!(encode(&[]), "");
        assert_eq!(decode(""), Ok(Vec::new()));
    }

    /// A value the capture did not write is refused whole: half a block put
    /// back would be a configuration nobody chose.
    #[test]
    fn a_value_the_capture_did_not_write_is_refused() {
        assert_eq!(decode("ROOMLERD_X=1"), Err(CarryError::Untagged));
        assert_eq!(decode(FORCED_RESTORE_FAILURE), Err(CarryError::Untagged));
        assert_eq!(decode("v1:A=%4"), Err(CarryError::BadEscape));
        assert_eq!(decode("v1:A=%+F"), Err(CarryError::BadEscape));
        assert_eq!(decode("v1:A=%zz"), Err(CarryError::BadEscape));
        assert_eq!(decode("v1:A=%FF"), Err(CarryError::NotUtf8));
        assert_eq!(decode("v1:A=1|"), Err(CarryError::NotAnEntry));
        assert_eq!(decode("v1:NOEQUALS"), Err(CarryError::NotAnEntry));
        assert_eq!(decode("v1:=C:"), Err(CarryError::NotAnEntry));
        let big = format!("v1:A={}", "x".repeat(MAX_CARRIED));
        assert_eq!(decode(&big), Err(CarryError::TooLarge));
    }

    #[test]
    fn a_name_ends_at_its_first_equals_sign() {
        assert_eq!(name_of("A=1"), "A");
        assert_eq!(name_of("A==1"), "A");
        assert_eq!(name_of("A="), "A");
        assert_eq!(name_of("=C:=C:\\"), "=C:");
        assert_eq!(name_of("NONE"), "NONE");
    }

    /// The merge rule (#3375): the installer's switch is its own decision,
    /// in either spelling the daemon reads and in any case; everything else
    /// comes back as it was, in place.
    #[test]
    fn every_entry_comes_back_but_the_installers_own() {
        let carried = entries(&[
            "ROOMLERD_ENABLE_SYSTEM_SWAP=1",
            "roomler_node_enable_system_swap=1",
            "ROOMLERD_SFTP_AS_USER=0",
            "ROOMLERD_UPDATE_URL=http://feed",
        ]);
        let (merged, restored) = merge(Vec::new(), &carried);
        assert_eq!(
            merged,
            entries(&["ROOMLERD_SFTP_AS_USER=0", "ROOMLERD_UPDATE_URL=http://feed"])
        );
        assert_eq!(restored, ["ROOMLERD_SFTP_AS_USER", "ROOMLERD_UPDATE_URL"]);

        // A switch the installer already wrote keeps its value; a carried
        // entry replaces one of the same name in place, case aside.
        let current = entries(&["ROOMLERD_ENABLE_SYSTEM_SWAP=0", "roomlerd_sftp_as_user=1"]);
        let (merged, restored) = merge(current, &carried);
        assert_eq!(
            merged,
            entries(&[
                "ROOMLERD_ENABLE_SYSTEM_SWAP=0",
                "ROOMLERD_SFTP_AS_USER=0",
                "ROOMLERD_UPDATE_URL=http://feed",
            ])
        );
        assert_eq!(restored.len(), 2);
    }

    #[test]
    fn a_block_of_only_the_installers_switch_restores_nothing() {
        let (merged, restored) = merge(Vec::new(), &entries(&["ROOMLERD_ENABLE_SYSTEM_SWAP=1"]));
        assert!(merged.is_empty() && restored.is_empty());
    }

    #[test]
    fn a_multi_sz_round_trips_and_skips_its_terminators() {
        let block = entries(&["A=1", "B=x y"]);
        let wide = join_multi_sz(&block);
        assert_eq!(&wide[wide.len() - 2..], &[0, 0]);
        assert_eq!(split_multi_sz(&wide), block);
        // A value written without its closing null, and a stray empty string.
        let mut ragged: Vec<u16> = "A=1\0\0B=2".encode_utf16().collect();
        assert_eq!(split_multi_sz(&ragged), entries(&["A=1", "B=2"]));
        ragged.clear();
        assert!(split_multi_sz(&ragged).is_empty());
    }

    /// The MSI's tables name this DLL's entry points and properties: a typo
    /// on either side would make the action fail to load (error 1723) or
    /// carry nothing, and neither shows until a real upgrade.
    #[test]
    fn the_per_machine_msi_schedules_what_this_dll_exports() {
        let wxs = include_str!("../../roomlerd/wix-perMachine/main.wxs");
        // The `<kind … Id='id' … />` element: an action and a property can
        // share an id, and the restore's do.
        let declared = |kind: &str, id: &str| -> String {
            wxs.match_indices(&format!("Id='{id}'"))
                .map(|(at, _)| wxs[..at].rfind('<').expect("inside an element"))
                .find(|&open| wxs[open + 1..].starts_with(kind))
                .map(|open| {
                    let rest = &wxs[open..];
                    rest[..rest.find("/>").expect("closed")].to_string()
                })
                .unwrap_or_else(|| panic!("<{kind} Id='{id}'> declared"))
        };
        assert!(wxs.contains("SourceFile='target\\release\\roomler_msi_ca.dll'"));
        for (id, execute) in [(CAPTURE_ACTION, "immediate"), (RESTORE_ACTION, "deferred")] {
            let ca = declared("CustomAction", id);
            assert!(ca.contains(&format!("DllEntry='{id}'")), "{ca}");
            assert!(ca.contains(&format!("Execute='{execute}'")), "{ca}");
            // Never fatal: a failing action would stop this device's updates.
            assert!(ca.contains("Return='ignore'"), "{ca}");
        }
        assert!(declared("CustomAction", RESTORE_ACTION).contains("Impersonate='no'"));
        // The values stay out of the MSI's log: the private property and the
        // name the restore's CustomActionData is logged under.
        for property in [CARRY_PROPERTY, RESTORE_ACTION] {
            let p = declared("Property", property);
            assert!(p.contains("Hidden='yes'"), "{p}");
        }
        let feed = declared("CustomAction", "SetRestoreServiceEnvironment");
        assert!(
            feed.contains(&format!("Property='{RESTORE_ACTION}'")),
            "{feed}"
        );
        assert!(
            feed.contains(&format!("Value='[{CARRY_PROPERTY}]'")),
            "{feed}"
        );
        assert!(declared("Property", FAIL_PROPERTY).contains("Secure='yes'"));
    }

    /// The order the carry depends on: the capture before the old product's
    /// removal (which `afterInstallInitialize` puts right after
    /// InstallInitialize), the restore after RegisterService made the new
    /// entry, and every SystemContext action after the restore, because each
    /// restarts the service and decides the switch itself.
    #[test]
    fn the_restore_lands_between_register_service_and_the_switch() {
        let wxs = include_str!("../../roomlerd/wix-perMachine/main.wxs");
        assert!(wxs.contains("Schedule='afterInstallInitialize'"));
        for scheduled in [
            "<Custom Action='CaptureServiceEnvironment' Before='InstallInitialize'>",
            "<Custom Action='SetRestoreServiceEnvironment' After='RegisterService'>",
            "<Custom Action='RestoreServiceEnvironment' After='SetRestoreServiceEnvironment'>",
            "<Custom Action='RollbackSystemContext' After='RestoreServiceEnvironment'>",
            "<Custom Action='EnableSystemContext' After='RollbackSystemContext'>",
            "<Custom Action='DisableSystemContext' After='RestoreServiceEnvironment'>",
            "<Custom Action='KeepSystemContext' After='RestoreServiceEnvironment'>",
        ] {
            assert!(wxs.contains(scheduled), "{scheduled}");
        }
        let condition = |action: &str| -> String {
            let at = wxs
                .find(&format!("<Custom Action='{action}'"))
                .expect("scheduled");
            let rest = &wxs[at..];
            rest[rest.find('>').unwrap() + 1..rest.find("</Custom>").unwrap()]
                .trim()
                .to_string()
        };
        assert_eq!(
            condition(CAPTURE_ACTION),
            r#"WIX_UPGRADE_DETECTED AND NOT (REMOVE="ALL")"#
        );
        for action in ["SetRestoreServiceEnvironment", RESTORE_ACTION] {
            assert_eq!(
                condition(action),
                format!(r#"{CARRY_PROPERTY} AND NOT (REMOVE="ALL")"#)
            );
        }
    }
}
