// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4196 — the file operations Windows runs at the next boot, and which of them
//! would delete a file this install has just put in place: pure, so it is
//! tested on every platform.
//!
//! Windows keeps them in two `REG_MULTI_SZ` values under
//! `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager` ([`VALUES`]), and
//! the session manager runs them at the next boot, before any service starts.
//! An operation is two strings, a source and a destination, and an EMPTY
//! destination means: delete the source. So the value holds empty strings in
//! the middle, which a plain multi-string reader drops
//! ([`crate::carry::split_multi_sz`] does). Here every string is kept as the
//! value holds it, as UTF-16, and a value rebuilt from all of its operations is
//! the value itself, byte for byte ([`encode`]).
//!
//! `MoveFileEx` writes a path as `\??\C:\…`; on Windows 11 build 26200 it puts
//! `*1` in front, and `*1!` in front of a destination it may replace (measured
//! in the #4196 lab). [`normalize`] takes those off before a path is compared.

/// The two values the session manager runs at boot.
pub const VALUES: [&str; 2] = [
    "PendingFileRenameOperations",
    "PendingFileRenameOperations2",
];

/// The key both values live under, below `HKEY_LOCAL_MACHINE`.
pub const SESSION_MANAGER: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager";

/// The immediate action that lists the files this install leaves in place.
pub const LIST_ACTION: &str = "ListInstalledFiles";

/// The deferred action that takes their pending deletes out. Its name is also
/// the property its `CustomActionData` comes from.
///
/// It sorts after `RegisterService` on purpose: WiX v3 numbers the actions
/// scheduled after the same action in the order of their names, depth first,
/// so this one comes after every action scheduled after `InstallFiles` and
/// leaves their numbers as `docs/installation.md` records them.
pub const SPARE_ACTION: &str = "SpareInstalledFiles";

/// What joins the listed paths in `CustomActionData`: no Windows path holds it.
pub const LIST_SEPARATOR: char = '|';

/// Every file of the MSI with its component and that component's directory.
pub const FILES_QUERY: &str = "SELECT `File`.`FileName`, `Component`.`Component`, \
     `Component`.`Directory_` FROM `File`, `Component` \
     WHERE `File`.`Component_` = `Component`.`Component`";

/// `INSTALLSTATE_LOCAL` (msi.h).
pub const LOCAL: i32 = 3;
/// `INSTALLSTATE_UNKNOWN` (msi.h): no action on the component.
pub const UNKNOWN: i32 = -1;

/// Whether a component's files are on this machine once the install ends: it is
/// being installed locally, or it is installed locally and this run leaves it
/// alone (a maintenance run). A component being removed, or one that is not
/// installed and stays so, is not.
pub fn stays_local(installed: i32, action: i32) -> bool {
    action == LOCAL || (action == UNKNOWN && installed == LOCAL)
}

/// The long name in the File table's `FileName`, which holds `short|long` when
/// the two differ.
pub fn long_name(file_name: &str) -> &str {
    file_name.rsplit('|').next().unwrap_or(file_name)
}

/// A file's full path from its directory's target path (which ends with a
/// separator) and its name.
pub fn file_path(directory: &str, name: &str) -> String {
    if directory.ends_with('\\') {
        format!("{directory}{name}")
    } else {
        format!("{directory}\\{name}")
    }
}

/// The listed paths as one `CustomActionData` value.
pub fn join_list(paths: &[String]) -> String {
    paths.join(&LIST_SEPARATOR.to_string())
}

/// The paths of a `CustomActionData` value; empty entries are skipped.
pub fn split_list(value: &str) -> Vec<String> {
    value
        .split(LIST_SEPARATOR)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Why a value is left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// It does not end with the null that closes its last string and the one
    /// that closes the list.
    Unterminated,
    /// Its strings do not pair up into operations.
    Unpaired,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Malformed::Unterminated => "it does not end with two nulls",
            Malformed::Unpaired => "its strings do not pair up into operations",
        })
    }
}

/// One pending operation as the value holds it: a source and a destination,
/// the destination empty for a delete.
pub type Operation<'a> = (&'a [u16], &'a [u16]);

/// The value's operations, each string exactly as the value holds it. A value
/// that cannot be read as pairs is an error, and nothing here ever writes one.
pub fn operations(value: &[u16]) -> Result<Vec<Operation<'_>>, Malformed> {
    // Every string closed by a null, and the list by one more.
    let [body @ .., 0, 0] = value else {
        return Err(Malformed::Unterminated);
    };
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let strings: Vec<&[u16]> = body.split(|&unit| unit == 0).collect();
    if !strings.len().is_multiple_of(2) {
        return Err(Malformed::Unpaired);
    }
    Ok(strings
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .collect())
}

/// A value from operations: each string with its null, then the list's.
/// `encode(&operations(v)?) == v` for every value `operations` reads, the
/// empty list (two nulls) included.
pub fn encode(operations: &[Operation<'_>]) -> Vec<u16> {
    if operations.is_empty() {
        return vec![0, 0];
    }
    let mut out = Vec::with_capacity(
        operations
            .iter()
            .map(|(src, dst)| src.len() + dst.len() + 2)
            .sum::<usize>()
            + 1,
    );
    for (src, dst) in operations {
        out.extend_from_slice(src);
        out.push(0);
        out.extend_from_slice(dst);
        out.push(0);
    }
    out.push(0);
    out
}

/// A path made comparable: the `*N` marker, the `!` of a destination that may
/// be replaced and the `\??\` prefix taken off, in lowercase, as Windows
/// compares paths without regard to case.
pub fn normalize(name: &str) -> String {
    let mut rest = name;
    if let Some(after) = rest.strip_prefix('*') {
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            rest = &after[digits..];
        }
    }
    rest = rest.strip_prefix('!').unwrap_or(rest);
    rest = rest.strip_prefix(r"\??\").unwrap_or(rest);
    rest.to_lowercase()
}

fn normalize_units(name: &[u16]) -> String {
    normalize(&String::from_utf16_lossy(name))
}

/// Every path a rename in this value puts a file at, normalized; none for a
/// value that cannot be read.
pub fn renamed_onto(value: &[u16]) -> Vec<String> {
    operations(value)
        .map(|ops| {
            ops.iter()
                .filter(|(_, dst)| !dst.is_empty())
                .map(|(_, dst)| normalize_units(dst))
                .collect()
        })
        .unwrap_or_default()
}

/// What to do with one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The deletes taken out, each source as the value names it.
    pub dropped: Vec<String>,
    /// The deletes of a listed file that stay, because a later operation puts
    /// a file at that path: a replacement whose first step is the delete.
    pub replaced: Vec<String>,
    /// How many operations the value holds once the plan is applied.
    pub left: usize,
    /// The value without the dropped operations, every other string as it
    /// was; `None` when nothing is dropped, and empty when no operation is
    /// left, which deletes the value, as the session manager does once it
    /// has run them.
    pub rewritten: Option<Vec<u16>>,
}

/// Take out every delete whose source is one of `listed`, the files this
/// install leaves in place, unless a LATER operation of the same value, or
/// any rename in `renamed_elsewhere` (the other value's, as [`renamed_onto`]
/// gives them), puts a file at that same path: then the delete is the first
/// half of a replacement, and the file that replacement brings is the one the
/// install wants. Nothing else changes: a rename is never touched, whatever
/// its paths, and every operation kept stays where it was, byte for byte.
pub fn plan(
    value: &[u16],
    listed: &[String],
    renamed_elsewhere: &[String],
) -> Result<Plan, Malformed> {
    let ops = operations(value)?;
    let listed: Vec<String> = listed.iter().map(|p| normalize(p)).collect();
    let onto: Vec<Option<String>> = ops
        .iter()
        .map(|(_, dst)| (!dst.is_empty()).then(|| normalize_units(dst)))
        .collect();
    let mut kept = Vec::with_capacity(ops.len());
    let (mut dropped, mut replaced) = (Vec::new(), Vec::new());
    for (at, &(src, dst)) in ops.iter().enumerate() {
        if dst.is_empty() && !src.is_empty() {
            let path = normalize_units(src);
            if listed.contains(&path) {
                let put_back = onto[at + 1..].iter().flatten().any(|p| *p == path)
                    || renamed_elsewhere.contains(&path);
                let named = String::from_utf16_lossy(src);
                if !put_back {
                    dropped.push(named);
                    continue;
                }
                replaced.push(named);
            }
        }
        kept.push((src, dst));
    }
    let rewritten = match (dropped.is_empty(), kept.is_empty()) {
        (true, _) => None,
        (false, true) => Some(Vec::new()),
        (false, false) => Some(encode(&kept)),
    };
    Ok(Plan {
        dropped,
        replaced,
        left: kept.len(),
        rewritten,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOMLERD: &str = r"C:\Program Files\Roomler\roomlerd.exe";

    /// A value as the registry holds it, from strings with `|` for each null.
    fn value(text: &str) -> Vec<u16> {
        text.replace('|', "\0").encode_utf16().collect()
    }

    fn text(units: &[u16]) -> String {
        String::from_utf16_lossy(units).replace('\0', "|")
    }

    fn listed(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    /// The shape the #4196 lab read after a held per-Machine upgrade (exit
    /// 3010): three operations of another program (a rename, a delete, a
    /// rename that may replace its destination), then the old product's
    /// boot-time delete of roomlerd.exe, then Windows Installer's delete of
    /// its rollback file. Only the fourth goes, and the value shrinks by
    /// exactly its source and two nulls, as it did in the lab (446 to 356
    /// bytes).
    #[test]
    fn a_held_upgrade_loses_only_its_delete_of_the_installed_file() {
        let before = value(concat!(
            r"*1\??\C:\Temp\ren-src.txt|*1\??\C:\Temp\ren-dst.txt|",
            r"*1\??\C:\Temp\del-me.txt||",
            r"*1\??\C:\Temp\rep-src.txt|*1!\??\C:\Temp\rep-dst.txt|",
            r"*1\??\C:\Program Files\Roomler\roomlerd.exe||",
            r"*1\??\C:\Config.Msi\17ad2.rbf|||",
        ));
        let plan = plan(&before, &listed(&[ROOMLERD]), &[]).unwrap();
        assert_eq!(
            plan.dropped,
            vec![r"*1\??\C:\Program Files\Roomler\roomlerd.exe"]
        );
        assert!(plan.replaced.is_empty());
        assert_eq!(plan.left, 4);
        let after = plan.rewritten.unwrap();
        assert_eq!(
            text(&after),
            concat!(
                r"*1\??\C:\Temp\ren-src.txt|*1\??\C:\Temp\ren-dst.txt|",
                r"*1\??\C:\Temp\del-me.txt||",
                r"*1\??\C:\Temp\rep-src.txt|*1!\??\C:\Temp\rep-dst.txt|",
                r"*1\??\C:\Config.Msi\17ad2.rbf|||",
            )
        );
        let source = r"*1\??\C:\Program Files\Roomler\roomlerd.exe";
        assert_eq!(before.len() - after.len(), source.len() + 2);
    }

    /// A file held through the NEW product's copy too: Windows Installer
    /// schedules the new file's rename onto the path after the old product's
    /// delete. The session manager deletes the old file, then renames the
    /// new one into place, which is what the install wants: both stay.
    #[test]
    fn a_delete_that_a_later_rename_follows_stays() {
        let before = value(concat!(
            r"*1\??\C:\Program Files\Roomler\roomlerd.exe||",
            r"*1\??\C:\Program Files\Roomler\RBa1b2.tmp|*1!\??\C:\Program Files\Roomler\roomlerd.exe||",
        ));
        let plan = plan(&before, &listed(&[ROOMLERD]), &[]).unwrap();
        assert!(plan.dropped.is_empty());
        assert_eq!(
            plan.replaced,
            vec![r"*1\??\C:\Program Files\Roomler\roomlerd.exe"]
        );
        assert_eq!(plan.rewritten, None);
        assert_eq!(plan.left, 2);
    }

    /// Which of the two values the session manager runs first is not ours to
    /// know, so a rename onto the file in the other one keeps the delete.
    #[test]
    fn a_rename_onto_the_file_in_the_other_value_keeps_the_delete() {
        let other = value(
            r"\??\C:\Program Files\Roomler\x.tmp|!\??\C:\Program Files\Roomler\roomlerd.exe||",
        );
        let elsewhere = renamed_onto(&other);
        assert_eq!(elsewhere, vec![ROOMLERD.to_lowercase()]);
        let before = value(r"*1\??\C:\Program Files\Roomler\roomlerd.exe|||");
        let plan = plan(&before, &listed(&[ROOMLERD]), &elsewhere).unwrap();
        assert!(plan.dropped.is_empty());
        assert_eq!(plan.replaced.len(), 1);
        assert_eq!(plan.rewritten, None);
    }

    /// A rename before the delete does not put the file back: the delete runs
    /// after it. And a rename FROM the installed path is never touched.
    #[test]
    fn only_a_later_rename_counts_and_renames_are_never_touched() {
        let before = value(concat!(
            r"\??\C:\Program Files\Roomler\old.tmp|!\??\C:\Program Files\Roomler\roomlerd.exe|",
            r"\??\C:\Program Files\Roomler\roomlerd.exe|\??\C:\Quarantine\roomlerd.exe|",
            r"\??\C:\Program Files\Roomler\roomlerd.exe|||",
        ));
        let plan = plan(&before, &listed(&[ROOMLERD]), &[]).unwrap();
        assert_eq!(
            plan.dropped,
            vec![r"\??\C:\Program Files\Roomler\roomlerd.exe"]
        );
        assert_eq!(
            text(&plan.rewritten.unwrap()),
            concat!(
                r"\??\C:\Program Files\Roomler\old.tmp|!\??\C:\Program Files\Roomler\roomlerd.exe|",
                r"\??\C:\Program Files\Roomler\roomlerd.exe|\??\C:\Quarantine\roomlerd.exe||",
            )
        );
    }

    /// The marker, the replace flag, the prefix and the case are spellings of
    /// one path, on every Windows build: with and without `*N`, with no
    /// `\??\`, and in any case.
    #[test]
    fn every_spelling_of_the_installed_file_is_the_installed_file() {
        for spelled in [
            r"*1\??\C:\Program Files\Roomler\roomlerd.exe",
            r"*12\??\C:\Program Files\Roomler\roomlerd.exe",
            r"\??\C:\Program Files\Roomler\roomlerd.exe",
            r"C:\Program Files\Roomler\roomlerd.exe",
            r"*1\??\c:\PROGRAM FILES\roomler\ROOMLERD.EXE",
        ] {
            let before = value(&format!("{spelled}|||"));
            let plan = plan(&before, &listed(&[ROOMLERD]), &[]).unwrap();
            assert_eq!(plan.dropped, vec![spelled.to_string()], "{spelled}");
            assert_eq!(plan.rewritten, Some(Vec::new()), "{spelled}");
            assert_eq!(plan.left, 0);
        }
        assert_eq!(
            normalize(r"*1!\??\C:\A\B.txt"),
            normalize(r"C:\a\b.TXT"),
            "a replace's destination"
        );
        // `*` with no digits is not the marker.
        assert_eq!(normalize(r"*\??\C:\a"), r"*\??\c:\a");
    }

    /// Equality, never a prefix: a file the install does not list keeps its
    /// delete, whatever it shares with one it does. So does the retired
    /// alias that an upgrade removes on purpose.
    #[test]
    fn a_file_the_install_does_not_list_keeps_its_delete() {
        let before = value(concat!(
            r"*1\??\C:\Program Files\Roomler\roomlerd.exe2||",
            r"*1\??\C:\Program Files\Roomler\roomlerd.ex||",
            r"*1\??\C:\Program Files\Roomler\roomler-agent.exe||",
            r"*1\??\C:\Program Files\Roomler||",
            r"*1\??\D:\Roomler\roomlerd.exe|||",
        ));
        let plan = plan(&before, &listed(&[ROOMLERD]), &[]).unwrap();
        assert!(plan.dropped.is_empty() && plan.replaced.is_empty());
        assert_eq!(plan.rewritten, None);
        assert_eq!(plan.left, 5);
    }

    /// A delete left by an earlier run with no restart since (an upgrade that
    /// ended 3010, an uninstall) goes too: it would take this install's file
    /// all the same. Every listed file, every one of its deletes.
    #[test]
    fn every_delete_of_a_listed_file_goes_stale_ones_too() {
        let before = value(concat!(
            r"*1\??\C:\Program Files\Roomler\roomlerd.exe||",
            r"*1\??\C:\Temp\other.txt||",
            r"*1\??\C:\Program Files\Roomler\roomler.exe||",
            r"*1\??\C:\Program Files\Roomler\roomlerd.exe|||",
        ));
        let files = listed(&[ROOMLERD, r"C:\Program Files\Roomler\roomler.exe"]);
        let plan = plan(&before, &files, &[]).unwrap();
        assert_eq!(plan.dropped.len(), 3);
        assert_eq!(plan.left, 1);
        assert_eq!(
            text(&plan.rewritten.unwrap()),
            r"*1\??\C:\Temp\other.txt|||"
        );
    }

    /// Nothing listed, nothing dropped, even with the file's delete pending.
    #[test]
    fn an_empty_list_changes_nothing() {
        let before = value(r"*1\??\C:\Program Files\Roomler\roomlerd.exe|||");
        let plan = plan(&before, &[], &[]).unwrap();
        assert_eq!(plan.rewritten, None);
        assert_eq!(plan.left, 1);
    }

    /// A value that does not pair up is left as it is: an odd count of
    /// strings, or no closing nulls. An empty list holds no operation.
    #[test]
    fn a_value_that_does_not_pair_up_is_left_alone() {
        let files = listed(&[ROOMLERD]);
        for (raw, why) in [
            (
                r"*1\??\C:\Program Files\Roomler\roomlerd.exe||",
                Malformed::Unpaired,
            ),
            (r"a|b|c||", Malformed::Unpaired),
            (
                r"*1\??\C:\Program Files\Roomler\roomlerd.exe|",
                Malformed::Unterminated,
            ),
            ("", Malformed::Unterminated),
            ("|", Malformed::Unterminated),
        ] {
            assert_eq!(plan(&value(raw), &files, &[]), Err(why), "{raw:?}");
        }
        assert_eq!(operations(&value("||")), Ok(Vec::new()));
        assert!(renamed_onto(&value("a|b|c||")).is_empty());
    }

    /// A kept string is copied as units, never decoded and encoded again: an
    /// unpaired surrogate, which no `String` holds, comes back as it was.
    #[test]
    fn a_kept_string_survives_unit_for_unit() {
        let mut before: Vec<u16> = r"\??\C:\Temp\odd".encode_utf16().collect();
        before.push(0xD800);
        before.extend([0, 0]);
        before.extend(r"\??\C:\Program Files\Roomler\roomlerd.exe".encode_utf16());
        before.extend([0, 0, 0]);
        let plan = plan(&before, &listed(&[ROOMLERD]), &[]).unwrap();
        let mut want: Vec<u16> = r"\??\C:\Temp\odd".encode_utf16().collect();
        want.push(0xD800);
        want.extend([0, 0, 0]);
        assert_eq!(plan.rewritten, Some(want));
    }

    /// Rebuilt from all of its operations, a value is itself, empty strings
    /// and stray trailing nulls included.
    #[test]
    fn a_value_rebuilt_from_its_operations_is_itself() {
        for raw in [
            "||",
            r"a|b||",
            r"a||c|d||",
            r"a|||||",
            r"*1\??\C:\x||*1\??\C:\y|*1!\??\C:\z||",
        ] {
            let v = value(raw);
            assert_eq!(encode(&operations(&v).unwrap()), v, "{raw:?}");
        }
    }

    #[test]
    fn a_file_name_column_gives_its_long_name() {
        assert_eq!(long_name("ROOMLE~1.EXE|roomlerd.exe"), "roomlerd.exe");
        assert_eq!(long_name("wintun.dll"), "wintun.dll");
        assert_eq!(
            file_path(r"C:\Program Files\Roomler\", "roomlerd.exe"),
            ROOMLERD
        );
        assert_eq!(
            file_path(r"C:\Program Files\Roomler", "roomlerd.exe"),
            ROOMLERD
        );
    }

    /// msi.h: ABSENT 2, LOCAL 3, SOURCE 4, UNKNOWN -1.
    #[test]
    fn a_component_stays_local_when_it_is_installed_or_left_installed() {
        assert!(stays_local(2, LOCAL), "an upgrade's new component");
        assert!(stays_local(LOCAL, LOCAL), "a reinstall");
        assert!(
            stays_local(LOCAL, UNKNOWN),
            "a maintenance run that leaves it"
        );
        assert!(!stays_local(LOCAL, 2), "removed");
        assert!(!stays_local(2, UNKNOWN), "absent and left absent");
        assert!(!stays_local(UNKNOWN, UNKNOWN));
        assert!(!stays_local(LOCAL, 4), "run from source");
    }

    #[test]
    fn the_listed_paths_round_trip() {
        let paths = listed(&[ROOMLERD, r"C:\Program Files\Roomler\wintun.dll"]);
        assert_eq!(split_list(&join_list(&paths)), paths);
        assert!(split_list("").is_empty());
        assert_eq!(split_list("|a||b|"), listed(&["a", "b"]));
    }

    /// The MSI's tables name this module's entry points: a typo on either
    /// side would make the action fail to load (error 1723, ignored) and
    /// spare nothing, which shows only on a held upgrade.
    #[test]
    fn the_per_machine_msi_schedules_the_list_and_the_spare() {
        let wxs = include_str!("../../roomlerd/wix-perMachine/main.wxs");
        // The `<CustomAction … Id='id' … />` element, whatever the line ends.
        let declared = |id: &str| -> String {
            wxs.match_indices(&format!("Id='{id}'"))
                .map(|(at, _)| wxs[..at].rfind('<').expect("inside an element"))
                .find(|&open| wxs[open + 1..].starts_with("CustomAction"))
                .map(|open| {
                    let rest = &wxs[open..];
                    rest[..rest.find("/>").expect("closed")].to_string()
                })
                .unwrap_or_else(|| panic!("<CustomAction Id='{id}'> declared"))
        };
        for (id, execute) in [(LIST_ACTION, "immediate"), (SPARE_ACTION, "deferred")] {
            let ca = declared(id);
            assert!(ca.contains("BinaryKey='RoomlerMsiCa'"), "{ca}");
            assert!(ca.contains(&format!("DllEntry='{id}'")), "{ca}");
            assert!(ca.contains(&format!("Execute='{execute}'")), "{ca}");
            // Never fatal: a failing action would stop this device's updates.
            assert!(ca.contains("Return='ignore'"), "{ca}");
        }
        // HKLM's Session Manager key: LocalSystem's to write.
        assert!(declared(SPARE_ACTION).contains("Impersonate='no'"));

        let scheduled = |action: &str, after: &str| -> String {
            let head = format!("<Custom Action='{action}' After='{after}'>");
            let at = wxs.find(&head).unwrap_or_else(|| panic!("{head}"));
            let rest = &wxs[at + head.len()..];
            rest[..rest.find("</Custom>").unwrap()].trim().to_string()
        };
        // The list once the component states are final; the spare once the
        // new files are in place, so after the old product's removal and
        // after Windows Installer scheduled whatever it could not do now.
        // Never on an uninstall, whose deletes are wanted.
        assert_eq!(
            scheduled(LIST_ACTION, "InstallValidate"),
            r#"NOT (REMOVE="ALL")"#
        );
        assert_eq!(
            scheduled(SPARE_ACTION, "InstallFiles"),
            r#"NOT (REMOVE="ALL")"#
        );
        assert!(SPARE_ACTION > "RegisterService" && SPARE_ACTION > "CleanupLegacyPerUser");
        // The per-user MSI has no DLL, and its install runs as the person,
        // who cannot write the Session Manager key; Windows Installer never
        // schedules a boot-time delete in a folder the person can write.
        let per_user = include_str!("../../roomlerd/wix/main.wxs");
        assert!(!per_user.contains(SPARE_ACTION) && !per_user.contains(LIST_ACTION));
    }
}
