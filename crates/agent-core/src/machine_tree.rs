// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3609 — the chain a Windows background service loads its machine
//! enrollment from, kept to SYSTEM and Administrators.
//!
//! ```text
//! %ProgramData%\roomler                     the parent
//! ├─ last-system-context-attempt.json       the attempt record (keep-system-context)
//! └─ roomler\                               the machine folder (appdirs::machine_global_dir)
//!    ├─ config.toml, config.toml.prev
//!    └─ peer-auth\<tenant>.json, <tenant>.controllers.json
//! ```
//!
//! RED: the first commit pins the behaviour with tests; the functions below
//! still answer what master does.

#[cfg(windows)]
use std::path::{Path, PathBuf};

/// `S-1-5-18`, LocalSystem.
pub const SYSTEM: &str = "S-1-5-18";
/// `S-1-5-32-544`, the Administrators group.
pub const ADMINISTRATORS: &str = "S-1-5-32-544";

/// `ACCESS_ALLOWED_ACE_TYPE` and `ACCESS_DENIED_ACE_TYPE`.
pub const ACE_ALLOW: u8 = 0;
pub const ACE_DENY: u8 = 1;
/// `OBJECT_INHERIT_ACE`, `CONTAINER_INHERIT_ACE`, `INHERIT_ONLY_ACE`,
/// `INHERITED_ACE`.
pub const OBJECT_INHERIT: u8 = 0x1;
pub const CONTAINER_INHERIT: u8 = 0x2;
pub const INHERIT_ONLY: u8 = 0x8;
pub const INHERITED: u8 = 0x10;

/// Rights that let a holder change a file's content, its name, its DACL or
/// its owner, or a folder's entries (FR-94's set).
pub const WRITE_LIKE: u32 = 0x2 // FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x4 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x10 // FILE_WRITE_EA
    | 0x40 // FILE_DELETE_CHILD
    | 0x100 // FILE_WRITE_ATTRIBUTES
    | 0x1_0000 // DELETE
    | 0x4_0000 // WRITE_DAC
    | 0x8_0000 // WRITE_OWNER
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE
/// Rights on a folder that let a holder rename or replace what it holds, or
/// take the folder over: `FILE_DELETE_CHILD`, `DELETE`, `WRITE_DAC`,
/// `WRITE_OWNER`, `GENERIC_ALL`.
pub const SWAP: u32 = 0x40 | 0x1_0000 | 0x4_0000 | 0x8_0000 | 0x1000_0000;
/// `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE`, icacls' `RX`.
pub const READ_EXECUTE: u32 = 0x0012_00a9;
/// `FILE_ALL_ACCESS`, icacls' `F`.
pub const FULL: u32 = 0x001F_01FF;

/// One ACE of a DACL, as read: its type, flags, access mask and SID. A kind
/// that is neither an allow nor a deny is read no further: mask 0, SID empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ace {
    pub kind: u8,
    pub flags: u8,
    pub mask: u32,
    pub sid: String,
}

/// An entry's owner and DACL. `dacl: None` is a NULL DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sd {
    pub owner: String,
    pub protected: bool,
    pub dacl: Option<Vec<Ace>>,
}

/// SYSTEM or the Administrators group.
pub fn trusted(sid: &str) -> bool {
    sid == SYSTEM || sid == ADMINISTRATORS
}

/// B for one entry. RED: master checks nothing.
pub fn verdict(_sd: &Sd, _counts: u32, _what: &str) -> Result<(), String> {
    Ok(())
}

/// R's plan for the parent. RED: master never changes it.
pub fn parent_cut(_sd: &Sd) -> Option<Vec<Ace>> {
    None
}

/// Whether `sid` is an account of this computer. RED: master never asks.
pub fn is_local_account(_sid: &str, _machine_domain: &str) -> bool {
    false
}

/// The two kill switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    pub repair: bool,
    pub check: bool,
}

impl Switches {
    /// Both on: what a device runs with when nobody switched either off.
    pub const ON: Self = Self {
        repair: true,
        check: true,
    };
}

/// Why the machine config is not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// There is no machine config to run.
    Absent,
    /// It is there and fails the check, for the reason given.
    NotPrivate(String),
}

/// The chain under one `%ProgramData%\roomler`.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineTree {
    parent: PathBuf,
    folder: PathBuf,
}

#[cfg(windows)]
impl MachineTree {
    /// This computer's.
    pub fn here() -> Self {
        let folder = crate::appdirs::machine_global_dir();
        let parent = folder
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| folder.clone());
        Self { parent, folder }
    }

    /// The chain under `parent`, its machine folder resolved new-then-old.
    pub fn under(parent: &Path) -> Self {
        Self {
            parent: parent.to_path_buf(),
            folder: crate::appdirs::resolve_machine_global(parent),
        }
    }

    pub fn parent(&self) -> &Path {
        &self.parent
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    pub fn config(&self) -> PathBuf {
        self.folder.join("config.toml")
    }

    pub fn prev(&self) -> PathBuf {
        self.folder.join("config.toml.prev")
    }

    pub fn keys(&self) -> PathBuf {
        self.folder.join("peer-auth")
    }

    pub fn record(&self) -> PathBuf {
        self.parent.join("last-system-context-attempt.json")
    }
}

/// R. RED: master repairs nothing.
#[cfg(windows)]
pub fn repair(_tree: &MachineTree, _writing: bool) -> Vec<String> {
    Vec::new()
}

/// B. RED: master takes any config that opens.
#[cfg(windows)]
pub fn check(tree: &MachineTree) -> Result<(), String> {
    std::fs::File::open(tree.config())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// R then B. RED: master's read open.
#[cfg(windows)]
pub fn private_machine_config(tree: &MachineTree, _switches: Switches) -> Result<(), Refusal> {
    std::fs::File::open(tree.config())
        .map(|_| ())
        .map_err(|_| Refusal::Absent)
}

/// [`private_machine_config`] as a yes or no.
#[cfg(windows)]
pub fn machine_config_opens(tree: &MachineTree, switches: Switches) -> bool {
    private_machine_config(tree, switches).is_ok()
}

/// keep-system-context's read of the attempt record. RED: master reads it.
#[cfg(windows)]
pub fn read_record(tree: &MachineTree, _switches: Switches) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(tree.record()) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Helpers for the tests here and in `roomlerd`: an independent read of what
/// is on disk (SDDL through `GetNamedSecurityInfoW`), `icacls`, and a tree
/// shaped like the field's.
#[cfg(all(windows, any(test, feature = "test-fixtures")))]
#[doc(hidden)]
pub mod test_support {
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    /// BUILTIN\Guests: an account that is neither SYSTEM nor an
    /// administrator, which `icacls` can name on every Windows.
    pub const GUESTS: &str = "S-1-5-32-546";
    /// BUILTIN\Users.
    pub const USERS: &str = "S-1-5-32-545";

    /// `path`'s owner and DACL in SDDL (`O:…D:…`), read by name.
    pub fn sddl(path: &Path) -> String {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated path; `sd` receives a LocalAlloc'd
        // descriptor, freed below; the other out-params are optional.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, 0, "reading {}'s security", path.display());
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: the descriptor read above; `text` is LocalAlloc'd, freed
        // below.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut text,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "SDDL of {}", path.display());
        // SAFETY: the NUL-terminated string the call above wrote.
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        // SAFETY: `len` units were just read from `text`.
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: both were LocalAlloc'd by the calls above, freed once.
        unsafe {
            LocalFree(text as _);
            LocalFree(sd as _);
        }
        s
    }

    /// The owner of an SDDL string, `BA`, `SY` or a SID.
    pub fn owner(sddl: &str) -> String {
        sddl.strip_prefix("O:")
            .and_then(|rest| rest.split("D:").next())
            .unwrap_or("")
            .to_string()
    }

    /// Whether an SDDL string's DACL is protected (`D:P…`).
    pub fn protected(sddl: &str) -> bool {
        sddl.split("D:")
            .nth(1)
            .is_some_and(|dacl| dacl.split('(').next().unwrap_or("").contains('P'))
    }

    /// The ACE strings of an SDDL string's DACL, `A;OICI;FA;;;SY` each.
    pub fn aces(sddl: &str) -> Vec<String> {
        let dacl = sddl.split("D:").nth(1).unwrap_or("");
        dacl.split('(')
            .skip(1)
            .map(|a| a.trim_end_matches(')').to_string())
            .collect()
    }

    /// Whether any ACE of `sddl` names `sid` (a SID string, or SDDL's
    /// two-letter alias for it).
    pub fn names(sddl: &str, sid: &str) -> bool {
        let alias = match sid {
            GUESTS => "BG",
            USERS => "BU",
            super::SYSTEM => "SY",
            super::ADMINISTRATORS => "BA",
            other => other,
        };
        aces(sddl)
            .iter()
            .any(|a| a.rsplit(';').next() == Some(alias) || a.ends_with(&format!(";{sid}")))
    }

    /// Run `icacls <path> <args>`; whether it succeeded.
    pub fn icacls(path: &Path, args: &[&str]) -> bool {
        std::process::Command::new(crate::appdirs::system32_tool("icacls.exe"))
            .arg(path)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Make `link` a junction to `target` (`mklink /J`, no privilege needed).
    pub fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new(crate::appdirs::system32_tool("cmd.exe"))
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd runs");
        assert!(made.status.success(), "mklink /J: {made:?}");
    }

    /// This process's account SID, from `whoami`.
    pub fn me() -> String {
        let out = std::process::Command::new(crate::appdirs::system32_tool("whoami.exe"))
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .expect("whoami runs");
        let text = String::from_utf8_lossy(&out.stdout);
        text.trim()
            .rsplit(',')
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .to_string()
    }

    /// The built-in Guest account of this computer (RID 501): a local
    /// account that is not an administrator. `None` when this process does
    /// not run as an account of this computer.
    pub fn local_guest() -> Option<String> {
        let me = me();
        let (domain, _rid) = me.rsplit_once('-')?;
        domain
            .starts_with("S-1-5-21-")
            .then(|| format!("{domain}-501"))
    }

    /// Whether this token may set an owner it is not (SeRestorePrivilege,
    /// which `icacls /setowner` enables for itself): an elevated one. When it
    /// may not, a test that needs it says so and stops, except in the CI lane
    /// that runs elevated on purpose, where stopping would prove nothing.
    pub fn elevated_or_skip(case: &str) -> bool {
        let probe = tempfile_dir();
        let file = probe.join("probe");
        std::fs::write(&file, b"x").expect("a probe file");
        let ok = icacls(&file, &["/setowner", "*S-1-5-18"]);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&probe);
        if !ok {
            assert!(
                std::env::var_os("ROOMLERD_TEST_REQUIRE_ELEVATED").is_none(),
                "this lane must run elevated for {case}"
            );
            eprintln!("skipped {case}: this token cannot set an owner; run elevated");
        }
        ok
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "roomler-machine-tree-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("a probe folder");
        dir
    }

    /// A stand-in for `%ProgramData%` under `root`: not inheriting, with the
    /// entries `%ProgramData%` has (SYSTEM and Administrators full control,
    /// Users read and execute, and Users' right to add files and folders),
    /// so a folder made below it inherits as one below `%ProgramData%` does.
    pub fn program_data(root: &Path) -> PathBuf {
        let pd = root.join("ProgramData");
        std::fs::create_dir(&pd).expect("the ProgramData stand-in");
        assert!(
            icacls(
                &pd,
                &[
                    "/inheritance:r",
                    "/grant:r",
                    "*S-1-5-18:(OI)(CI)F",
                    "*S-1-5-32-544:(OI)(CI)F",
                    "*S-1-5-32-545:(OI)(CI)RX",
                ],
            ),
            "the ProgramData stand-in's DACL"
        );
        // A second call: one `/grant:r` naming Users twice keeps one entry.
        assert!(
            icacls(&pd, &["/grant", "*S-1-5-32-545:(CI)(WD,AD,WEA,WA)"]),
            "Users' right to add files and folders"
        );
        pd
    }

    /// `%ProgramData%\roomler` and its machine folder, as a host with the
    /// service and no machine enrollment has them: both made below the
    /// stand-in, inheriting all they hold. The parent is given to
    /// Administrators, as Setup or the service's own start leaves it.
    pub fn waiting_tree(root: &Path) -> super::MachineTree {
        let parent = program_data(root).join("roomler");
        std::fs::create_dir(&parent).expect("the parent");
        assert!(
            icacls(&parent, &["/setowner", "*S-1-5-32-544"]),
            "the parent to Administrators"
        );
        std::fs::create_dir(parent.join("roomler")).expect("the machine folder");
        super::MachineTree::under(&parent)
    }

    /// [`waiting_tree`] with a machine config, a previous copy, and a key
    /// file, made by this process.
    pub fn enrolled_tree(root: &Path) -> super::MachineTree {
        let tree = waiting_tree(root);
        std::fs::write(tree.config(), "agent_token = \"t\"\n").expect("the config");
        std::fs::write(tree.prev(), "agent_token = \"p\"\n").expect("its previous copy");
        std::fs::create_dir(tree.keys()).expect("the keys folder");
        std::fs::write(
            tree.keys().join("66f0c0ffee00000000000a01.json"),
            br#"{"version":1}"#,
        )
        .expect("a key file");
        tree
    }

    /// A protected DACL of SYSTEM and Administrators alone, `(OI)(CI)` on a
    /// folder: what the repair leaves on the machine folder.
    pub fn sealed(sddl: &str, folder: bool) -> bool {
        let mut want: Vec<String> = if folder {
            vec!["A;OICI;FA;;;SY".into(), "A;OICI;FA;;;BA".into()]
        } else {
            vec!["A;;FA;;;SY".into(), "A;;FA;;;BA".into()]
        };
        want.sort();
        let mut got = aces(sddl);
        got.sort();
        protected(sddl) && got == want
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ace(kind: u8, flags: u8, mask: u32, sid: &str) -> Ace {
        Ace {
            kind,
            flags,
            mask,
            sid: sid.into(),
        }
    }

    fn sd(owner: &str, protected: bool, aces: Vec<Ace>) -> Sd {
        Sd {
            owner: owner.into(),
            protected,
            dacl: Some(aces),
        }
    }

    const ACCOUNT: &str = "S-1-5-21-1-2-3-1001";
    const USERS: &str = "S-1-5-32-545";
    const CREATOR_OWNER: &str = "S-1-3-0";

    /// SYSTEM and Administrators with full control, as Setup and the repair
    /// leave a folder.
    fn sealed_aces(flags: u8) -> Vec<Ace> {
        vec![
            ace(ACE_ALLOW, flags, FULL, SYSTEM),
            ace(ACE_ALLOW, flags, FULL, ADMINISTRATORS),
        ]
    }

    /// B, the entries SYSTEM loads from: an account's write, inherited or
    /// not, an owner other than SYSTEM or Administrators, a NULL DACL and an
    /// ACE of a kind the check does not read are refusals.
    #[test]
    fn machine_chain_verdict_refuses_what_another_account_may_change() {
        let config = |extra: Ace| {
            let mut aces = sealed_aces(INHERITED);
            aces.push(extra);
            sd(ADMINISTRATORS, false, aces)
        };
        let refused = [
            (
                "an account's inherited full control",
                config(ace(ACE_ALLOW, INHERITED, FULL, ACCOUNT)),
            ),
            (
                "an account's own write",
                config(ace(ACE_ALLOW, 0, 0x2, ACCOUNT)),
            ),
            (
                "Users may append",
                config(ace(ACE_ALLOW, INHERITED, 0x4, USERS)),
            ),
            (
                "GENERIC_WRITE for an account",
                config(ace(ACE_ALLOW, 0, 0x4000_0000, ACCOUNT)),
            ),
            ("an object ACE", config(ace(5, 0, 0, ""))),
            ("owned by an account", sd(ACCOUNT, true, sealed_aces(0))),
            (
                "a NULL DACL",
                Sd {
                    owner: SYSTEM.into(),
                    protected: true,
                    dacl: None,
                },
            ),
        ];
        for (case, entry) in refused {
            assert!(
                verdict(&entry, WRITE_LIKE, "the machine config").is_err(),
                "{case} must refuse: {entry:?}"
            );
        }
    }

    /// B, the parent: only a right to rename or replace what it holds
    /// refuses. The right to add files and folders that `%ProgramData%`
    /// hands Users passes, so a person's daemon may still stage below it.
    #[test]
    fn machine_chain_verdict_on_the_parent_refuses_only_a_right_to_replace() {
        let parent = |extra: Ace| {
            let mut aces = sealed_aces(OBJECT_INHERIT | CONTAINER_INHERIT | INHERITED);
            aces.push(extra);
            sd(SYSTEM, false, aces)
        };
        let delete_child = parent(ace(ACE_ALLOW, 0, 0x40, ACCOUNT));
        assert!(
            verdict(&delete_child, SWAP, "the parent").is_err(),
            "an account's explicit FILE_DELETE_CHILD"
        );
        let full = parent(ace(
            ACE_ALLOW,
            OBJECT_INHERIT | CONTAINER_INHERIT,
            FULL,
            ACCOUNT,
        ));
        assert!(
            verdict(&full, SWAP, "the parent").is_err(),
            "Explorer's full control"
        );
        let owned = Sd {
            owner: ACCOUNT.into(),
            ..parent(ace(ACE_ALLOW, INHERITED, READ_EXECUTE, USERS))
        };
        assert!(
            verdict(&owned, SWAP, "the parent").is_err(),
            "an owner other than SYSTEM or Administrators"
        );
    }

    /// The other side of the two tests above: what passes.
    #[test]
    fn machine_chain_verdict_passes_what_only_system_and_administrators_may_change() {
        let users_create = ace(ACE_ALLOW, CONTAINER_INHERIT | INHERITED, 0x116, USERS);
        let creator_owner = ace(
            ACE_ALLOW,
            OBJECT_INHERIT | CONTAINER_INHERIT | INHERIT_ONLY | INHERITED,
            FULL,
            CREATOR_OWNER,
        );
        let mut aces = sealed_aces(OBJECT_INHERIT | CONTAINER_INHERIT | INHERITED);
        aces.extend([
            users_create,
            creator_owner,
            ace(ACE_ALLOW, INHERITED, READ_EXECUTE, USERS),
            ace(ACE_DENY, 0, FULL, ACCOUNT),
        ]);
        assert_eq!(
            verdict(&sd(SYSTEM, false, aces), SWAP, "the parent"),
            Ok(())
        );
        assert_eq!(
            verdict(
                &sd(ADMINISTRATORS, true, sealed_aces(0)),
                WRITE_LIKE,
                "a file"
            ),
            Ok(())
        );
    }

    /// R, the parent: an account's own entry that lets it change anything
    /// is cut to read and execute, keeping its inheritance flags; one that
    /// only writes loses the ACE. SYSTEM, Administrators, CREATOR OWNER and
    /// every inherited ACE are left out of the plan (the system adds the
    /// inherited ones back), and a parent with nothing to cut has no plan.
    #[test]
    fn parent_cut_takes_an_accounts_entry_down_to_read_and_execute() {
        let oici = OBJECT_INHERIT | CONTAINER_INHERIT;
        let mut aces = sealed_aces(oici);
        aces.extend([
            ace(ACE_ALLOW, oici, FULL, ACCOUNT),
            ace(ACE_ALLOW, CONTAINER_INHERIT, 0x116, USERS),
            ace(ACE_ALLOW, oici | INHERIT_ONLY, FULL, CREATOR_OWNER),
            ace(ACE_ALLOW, oici | INHERITED, FULL, "S-1-5-21-9-9-9-500"),
        ]);
        let plan = parent_cut(&sd(SYSTEM, true, aces)).expect("an entry to cut");
        assert_eq!(
            plan,
            vec![
                ace(ACE_ALLOW, oici, FULL, SYSTEM),
                ace(ACE_ALLOW, oici, FULL, ADMINISTRATORS),
                ace(ACE_ALLOW, oici, READ_EXECUTE, ACCOUNT),
                ace(ACE_ALLOW, oici | INHERIT_ONLY, FULL, CREATOR_OWNER),
            ]
        );
        assert_eq!(parent_cut(&sd(SYSTEM, true, sealed_aces(oici))), None);
    }

    /// Only an account of this computer is looked up in its account
    /// database: its SID is the computer's domain and one RID more.
    #[test]
    fn only_an_account_of_this_computer_is_looked_up() {
        let machine = "S-1-5-21-11-22-33";
        assert!(is_local_account("S-1-5-21-11-22-33-1001", machine));
        assert!(is_local_account("S-1-5-21-11-22-33-500", machine));
        for other in [
            "S-1-5-21-11-22-33",
            "S-1-5-21-11-22-334-1001",
            "S-1-5-21-11-22-33-1001-7",
            "S-1-5-21-44-55-66-1001",
            "S-1-12-1-1-2-3-4",
            "S-1-5-32-544",
            "S-1-5-18",
        ] {
            assert!(!is_local_account(other, machine), "{other}");
        }
    }

    #[cfg(windows)]
    mod on_ntfs {
        use super::super::test_support::*;
        use super::super::*;

        /// An account's entry on the machine folder (Explorer's prompt leaves
        /// `(OI)(CI)F`) makes the check refuse, and the repair takes it off:
        /// the folder is Administrators', protected, SYSTEM and
        /// Administrators alone, and the check passes again.
        #[test]
        fn an_entry_for_another_account_on_the_machine_folder_is_refused_until_repaired() {
            if !elevated_or_skip("the machine folder's repair") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            repair(&tree, false);
            assert_eq!(check(&tree), Ok(()), "a repaired tree passes");
            assert!(icacls(
                tree.folder(),
                &["/grant", "*S-1-5-32-546:(OI)(CI)F"]
            ));
            let why = check(&tree).expect_err("another account may change the machine folder");
            assert!(
                why.contains(&tree.folder().display().to_string()),
                "the reason names the entry: {why}"
            );
            repair(&tree, false);
            let folder = sddl(tree.folder());
            assert_eq!(owner(&folder), "BA", "{folder}");
            assert!(sealed(&folder, true), "{folder}");
            assert_eq!(check(&tree), Ok(()));
        }

        /// The design's R cell, with the owner rule the operator chose: on the
        /// parent the account's entry is cut to read and execute, below it the
        /// account is gone, an entry an administrator's own account owns is
        /// given to Administrators, and SYSTEM keeps the record it owns.
        #[test]
        fn repair_cuts_an_accounts_entry_and_keeps_every_other_owner() {
            if !elevated_or_skip("the parent's repair") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            std::fs::write(tree.record(), b"{}").unwrap();
            assert!(icacls(&tree.record(), &["/setowner", "*S-1-5-18"]));
            assert!(icacls(
                tree.parent(),
                &["/grant", "*S-1-5-32-546:(OI)(CI)F"]
            ));
            assert!(icacls(
                tree.folder(),
                &["/grant", "*S-1-5-32-546:(OI)(CI)F"]
            ));
            repair(&tree, false);
            let parent = sddl(tree.parent());
            assert!(
                aces(&parent).contains(&"A;OICI;0x1200a9;;;BG".to_string()),
                "cut to read and execute on the parent: {parent}"
            );
            for entry in [
                tree.folder().to_path_buf(),
                tree.config(),
                tree.prev(),
                tree.keys(),
                tree.record(),
            ] {
                let got = sddl(&entry);
                assert!(!names(&got, GUESTS), "{}: {got}", entry.display());
                assert!(protected(&got), "{}: {got}", entry.display());
            }
            assert_eq!(
                owner(&sddl(&tree.record())),
                "SY",
                "SYSTEM keeps its record"
            );
            assert_eq!(
                owner(&sddl(&tree.config())),
                "BA",
                "an administrator's file"
            );
            assert_eq!(check(&tree), Ok(()));
        }

        /// The design's cell: once repaired, an entry added to the machine
        /// folder again (the prompt, a second time) does not reach the config.
        #[test]
        fn an_entry_on_the_folder_never_reaches_the_config_after_repair() {
            if !elevated_or_skip("the config's own protection") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            repair(&tree, false);
            let before = sddl(&tree.config());
            assert!(icacls(
                tree.folder(),
                &["/grant", "*S-1-5-32-546:(OI)(CI)F"]
            ));
            assert_eq!(sddl(&tree.config()), before);
            assert!(sealed(&before, false), "{before}");
        }

        /// The brief's cell: a config another account owns is refused, the
        /// repair leaves it and its folder as they are, and the host runs the
        /// device as one with no machine enrollment.
        #[test]
        fn a_config_another_account_owns_is_refused_and_left_alone() {
            if !elevated_or_skip("a config another account owns") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            let other = local_guest().unwrap_or_else(|| USERS.to_string());
            assert!(icacls(&tree.config(), &["/setowner", &format!("*{other}")]));
            let config = sddl(&tree.config());
            let folder = sddl(tree.folder());
            repair(&tree, false);
            assert_eq!(sddl(&tree.config()), config, "never adopted");
            assert_eq!(sddl(tree.folder()), folder, "a waiting host's folder");
            assert!(matches!(
                private_machine_config(&tree, Switches::ON),
                Err(Refusal::NotPrivate(_))
            ));
        }

        /// The brief's cell: a machine folder that is not a plain folder is
        /// refused, and the repair never touches what it leads to.
        #[test]
        fn a_folder_that_leads_elsewhere_is_refused_and_never_repaired() {
            if !elevated_or_skip("a machine folder that leads elsewhere") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = waiting_tree(root.path());
            let elsewhere = root.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            std::fs::write(elsewhere.join("config.toml"), "agent_token = \"t\"\n").unwrap();
            std::fs::remove_dir(tree.folder()).unwrap();
            junction(tree.folder(), &elsewhere);
            let target = sddl(&elsewhere);
            let config = sddl(&elsewhere.join("config.toml"));
            repair(&tree, true);
            assert_eq!(sddl(&elsewhere), target);
            assert_eq!(sddl(&elsewhere.join("config.toml")), config);
            let why = check(&tree).expect_err("a junction for a machine folder");
            assert!(why.contains("link"), "{why}");
            assert!(!machine_config_opens(&tree, Switches::ON));
        }

        /// The same one level down: a keys folder that is a junction is
        /// refused after the rest of the tree was repaired, and what it
        /// leads to is never touched.
        #[test]
        fn a_keys_folder_that_leads_elsewhere_is_refused_and_never_repaired() {
            if !elevated_or_skip("a keys folder that leads elsewhere") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            repair(&tree, false);
            assert_eq!(check(&tree), Ok(()), "a repaired tree passes");
            let elsewhere = root.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            let key = elsewhere.join("66f0c0ffee00000000000a01.json");
            std::fs::write(&key, br#"{"version":1}"#).unwrap();
            std::fs::remove_dir_all(tree.keys()).unwrap();
            junction(&tree.keys(), &elsewhere);
            let target = sddl(&elsewhere);
            let key_sd = sddl(&key);
            repair(&tree, false);
            assert_eq!(sddl(&elsewhere), target);
            assert_eq!(sddl(&key), key_sd);
            let why = check(&tree).expect_err("a junction for a keys folder");
            assert!(why.contains("link"), "{why}");
        }

        /// The brief's cell and the operator's rule: a keys folder that an
        /// administrator's own account owns (this box's case) is given to
        /// Administrators and kept, with its key; a stray file beside the
        /// keys that the daemon never loads is left alone.
        #[test]
        fn an_administrators_keys_folder_is_taken_for_administrators_and_kept() {
            if !elevated_or_skip("the keys folder's owner") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            let me = me();
            assert!(icacls(&tree.keys(), &["/setowner", &format!("*{me}")]));
            let key = tree.keys().join("66f0c0ffee00000000000a01.json");
            let stray = tree.keys().join("66f0c0ffee00000000000a01.json.minted.bak");
            std::fs::write(&stray, b"x").unwrap();
            assert!(icacls(&stray, &["/setowner", "*S-1-5-32-546"]));
            let stray_before = sddl(&stray);
            repair(&tree, false);
            let keys = sddl(&tree.keys());
            assert_eq!(owner(&keys), "BA", "{keys}");
            assert!(sealed(&keys, true), "{keys}");
            assert!(sealed(&sddl(&key), false));
            assert_eq!(std::fs::read(&key).unwrap(), br#"{"version":1}"#, "kept");
            assert_eq!(
                owner(&sddl(&stray)),
                owner(&stray_before),
                "not loaded, not touched"
            );
            assert_eq!(check(&tree), Ok(()));
        }

        /// The pre-rename segment wins only when it passes too. Such a folder
        /// that another account made, with no `roomler` beside it, is refused
        /// instead of run for existing first.
        #[test]
        fn an_old_segment_that_is_not_private_never_wins() {
            if !elevated_or_skip("the pre-rename segment") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = waiting_tree(root.path());
            std::fs::remove_dir(tree.folder()).unwrap();
            let old = tree.parent().join(crate::appdirs::OLD_APP);
            std::fs::create_dir(&old).unwrap();
            std::fs::write(old.join("config.toml"), "agent_token = \"t\"\n").unwrap();
            let other = local_guest().unwrap_or_else(|| USERS.to_string());
            assert!(icacls(&old, &["/setowner", &format!("*{other}")]));
            assert!(icacls(
                &old.join("config.toml"),
                &["/setowner", &format!("*{other}")]
            ));
            let tree = MachineTree::under(tree.parent());
            assert_eq!(tree.folder(), old, "resolved new-then-old");
            assert!(!machine_config_opens(&tree, Switches::ON));
        }
    }
}
