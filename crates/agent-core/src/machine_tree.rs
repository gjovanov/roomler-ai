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
//! The service runs that enrollment as SYSTEM, so it takes it only from files
//! that SYSTEM and Administrators alone may change. Three parts:
//!
//! - **[`repair`]** puts the chain back, in-process and by handle, never
//!   through a junction or a symbolic link and with no `icacls` child (#3291).
//!   An entry for another account on the parent is cut to read and execute
//!   ([`parent_cut`]); the machine folder, once it holds a machine config or a
//!   save is writing one, becomes Administrators' with SYSTEM and
//!   Administrators alone; the config, its `.prev`, the keys folder, the key
//!   files and the attempt record get the same, not inheriting, so an entry
//!   added to a folder above later does not reach them. An entry owned by a
//!   member of the computer's Administrators group, read from the computer's
//!   own account database (an account of this computer directly or through a
//!   group; a domain's only when the group names it directly), is given to the
//!   group; one owned by any other account is never taken over, and is left
//!   as it is.
//! - **[`check`]** then judges the same chain: plain folders and files only,
//!   each owned by SYSTEM or Administrators, none that another account may
//!   change ([`verdict`]); `.prev` where it is loaded. The service host and
//!   the worker's ladder ask it
//!   through one function ([`private_machine_config`]), so they cannot
//!   disagree, and `keep-system-context` reads the record through
//!   [`read_record`].
//! - The writers ([`create_fresh`], [`seal_new_file`]) make every file they
//!   write: a file of that name made before them is never written into.
//!
//! Kill switches, read from the process environment only (the service's
//! `Environment` block), never from the config: `ROOMLERD_MACHINE_TREE_REPAIR=0`
//! stops the repair, `ROOMLERD_MACHINE_TREE_CHECK=0` makes the check log only.

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

/// `S-1-3-0`. An inherit-only ACE for it hands what it grants to whoever makes
/// a child, as `%ProgramData%`'s own does; on the parent it is never cut.
const CREATOR_OWNER: &str = "S-1-3-0";
/// `SYNCHRONIZE`: on its own it lets a holder do nothing to the file.
const SYNCHRONIZE: u32 = 0x10_0000;

/// SYSTEM or the Administrators group.
pub fn trusted(sid: &str) -> bool {
    sid == SYSTEM || sid == ADMINISTRATORS
}

/// A mask with its generic bits (`GENERIC_READ`, `_WRITE`, `_EXECUTE`,
/// `_ALL`) replaced by the file rights they stand for.
pub fn map_generic(mask: u32) -> u32 {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_EXECUTE: u32 = 0x2000_0000;
    const GENERIC_ALL: u32 = 0x1000_0000;
    let mut out = mask & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
    if mask & GENERIC_READ != 0 {
        out |= 0x0012_0089;
    }
    if mask & GENERIC_WRITE != 0 {
        out |= 0x0012_0116;
    }
    if mask & GENERIC_EXECUTE != 0 {
        out |= 0x0012_00a0;
    }
    if mask & GENERIC_ALL != 0 {
        out |= FULL;
    }
    out
}

/// B for one entry, over its owner and DACL as read: `Ok` only when it
/// is owned by SYSTEM or Administrators, has a DACL, and no allow-ACE that
/// applies to the entry itself gives anyone else a right in `counts`
/// ([`WRITE_LIKE`] for what SYSTEM loads, [`SWAP`] for the parent, whose
/// `%ProgramData%` entries let Users add files and folders). An inherit-only
/// ACE does not apply to the entry; a deny takes nothing away from SYSTEM or
/// Administrators; an ACE of any other kind is a refusal, because this check
/// fails closed (FR-94's rule). `what` names the entry in the reason.
pub fn verdict(sd: &Sd, counts: u32, what: &str) -> Result<(), String> {
    if !trusted(&sd.owner) {
        return Err(format!(
            "{what} is owned by an account other than SYSTEM or Administrators"
        ));
    }
    let Some(aces) = &sd.dacl else {
        return Err(format!("{what} has no DACL, so everyone may change it"));
    };
    let may = if counts == SWAP {
        "rename or replace what it holds"
    } else {
        "change it"
    };
    for ace in aces {
        if ace.flags & INHERIT_ONLY != 0 {
            continue;
        }
        match ace.kind {
            ACE_DENY => {}
            ACE_ALLOW => {
                if ace.mask & counts != 0 && !trusted(&ace.sid) {
                    return Err(format!(
                        "{what} lets an account other than SYSTEM or Administrators {may}"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "{what} carries an access rule of a kind this check does not read (type {other})"
                ));
            }
        }
    }
    Ok(())
}

/// R's plan for the parent: its explicit ACEs, with each allow-ACE that lets
/// an account other than SYSTEM, Administrators or CREATOR OWNER change
/// anything cut to read and execute (its inheritance flags kept, so what it
/// handed down is cut too), or dropped when nothing is left. Explorer's
/// folder prompt leaves full control, `(OI)(CI)F`, for the account that
/// confirmed it; cut, that account can still read the logs and gets no
/// prompt the next time. Inherited ACEs are not in the plan: the system adds
/// them back from `%ProgramData%`, and the right to add files and folders it
/// hands Users stays, so a person's daemon may still stage below the parent.
/// `None` when there is nothing to cut, or an ACE of a kind the plan cannot
/// write back (the check refuses that one).
pub fn parent_cut(sd: &Sd) -> Option<Vec<Ace>> {
    let aces = sd.dacl.as_ref()?;
    let mut cut = false;
    let mut plan = Vec::new();
    for ace in aces.iter().filter(|a| a.flags & INHERITED == 0) {
        if ace.kind != ACE_ALLOW && ace.kind != ACE_DENY {
            return None;
        }
        let mapped = map_generic(ace.mask);
        if ace.kind == ACE_ALLOW
            && !trusted(&ace.sid)
            && ace.sid != CREATOR_OWNER
            && mapped & WRITE_LIKE != 0
        {
            cut = true;
            let mask = mapped & READ_EXECUTE;
            if mask & !SYNCHRONIZE != 0 {
                plan.push(Ace {
                    mask,
                    ..ace.clone()
                });
            }
            continue;
        }
        plan.push(ace.clone());
    }
    cut.then_some(plan)
}

/// Whether `sid` is an account of this computer: its account domain
/// (`machine_domain`) and one RID more. Only such an account is looked up in
/// the computer's own account database; a domain or Entra account is never
/// looked up, so its membership is never assumed.
pub fn is_local_account(sid: &str, machine_domain: &str) -> bool {
    sid.strip_prefix(machine_domain)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rid| !rid.is_empty() && rid.bytes().all(|b| b.is_ascii_digit()))
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

    /// `ROOMLERD_MACHINE_TREE_REPAIR=0` stops the repair and
    /// `ROOMLERD_MACHINE_TREE_CHECK=0` makes the check log only (`0`, `false`,
    /// `no` or `off`; `ROOMLER_NODE_` is read too, as for every knob). Read
    /// from the process environment and nothing else: the config is what the
    /// check is about, so it can never switch the check off.
    pub fn from_env() -> Self {
        Self {
            repair: !switched_off("MACHINE_TREE_REPAIR"),
            check: !switched_off("MACHINE_TREE_CHECK"),
        }
    }
}

fn switched_off(suffix: &str) -> bool {
    ["ROOMLERD_", "ROOMLER_NODE_"]
        .iter()
        .find_map(|prefix| std::env::var(format!("{prefix}{suffix}")).ok())
        .is_some_and(|v| off_word(&v))
}

/// `0`, `false`, `no` or `off`, in any case, with whitespace around: the
/// words that switch a default-on knob off.
fn off_word(value: &str) -> bool {
    let value = value.trim();
    ["0", "false", "no", "off"]
        .iter()
        .any(|word| value.eq_ignore_ascii_case(word))
}

/// W: make `path` a new file, never write into one that is there: a file of
/// that name is removed first (a link of that name is removed, not followed),
/// and `create_new` refuses one that appears in between, so at worst a write
/// fails. Opened for writing, and on Windows for setting its owner and DACL
/// ([`seal_new_file`]); on Unix with mode `0600`.
pub fn create_fresh(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER.
        opts.access_mode(0x0012_0116 | 0x2_0000 | 0x4_0000 | 0x8_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)
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

/// What a [`repair`] did (`changed`) and what it found and left (`left`): an
/// entry another account owns, an entry that is a link, one this process may
/// not change. One line each, naming the entry.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Repairs {
    pub changed: Vec<String>,
    pub left: Vec<String>,
}

/// How R treats an entry.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// `%ProgramData%\roomler`: [`parent_cut`], inheritance as it is.
    Parent,
    /// The machine folder: Administrators', SYSTEM and Administrators alone,
    /// `(OI)(CI)`, not inheriting.
    Folder,
    /// The keys folder: SYSTEM and Administrators alone, `(OI)(CI)`, not
    /// inheriting.
    PrivateDir,
    /// The config, `.prev`, a key file, the record: SYSTEM and Administrators
    /// alone, not inheriting.
    Private,
}

/// SYSTEM and Administrators with full control, `flags` on each.
#[cfg(windows)]
fn sealed_aces(flags: u8) -> Vec<Ace> {
    vec![
        Ace {
            kind: ACE_ALLOW,
            flags,
            mask: FULL,
            sid: SYSTEM.into(),
        },
        Ace {
            kind: ACE_ALLOW,
            flags,
            mask: FULL,
            sid: ADMINISTRATORS.into(),
        },
    ]
}

/// Exactly [`sealed_aces`], in either order, and not inheriting.
#[cfg(windows)]
fn is_sealed(sd: &Sd, flags: u8) -> bool {
    let Some(aces) = &sd.dacl else {
        return false;
    };
    let want = sealed_aces(flags);
    sd.protected && aces.len() == want.len() && want.iter().all(|w| aces.contains(w))
}

/// R: put the chain under `tree` back. As SYSTEM, or as an elevated
/// administrator (Setup, `roomlerd enroll --machine-global`); any other
/// process may change nothing and leaves it to [`check`]. `writing`: a save is
/// about to write the machine config, so the machine folder is repaired even
/// before it holds one. Every entry is opened by handle and changed through
/// that handle, never through a junction or a symbolic link: such an entry,
/// and anything below it, is left for [`check`] to refuse. The order keeps
/// SYSTEM in at every step (#3073): each folder gets its final DACL in one
/// write, which never takes anything from SYSTEM or Administrators, and only
/// then its owner; a file gets both in one call (`win::write_sd`). The files
/// go before the folders that hold them.
#[cfg(windows)]
pub fn repair(tree: &MachineTree, writing: bool) -> Repairs {
    let mut out = Repairs::default();
    let parent = match win::open(tree.parent(), win::Kind::Dir, false, true) {
        Ok(Some(entry)) => entry,
        Ok(None) => return out,
        Err(why) => {
            out.left.push(why);
            return out;
        }
    };
    match win::open(tree.folder(), win::Kind::Dir, false, true) {
        Ok(Some(folder)) => {
            if writing || holds_a_config_it_may_take(tree, &mut out) {
                for path in [tree.config(), tree.prev()] {
                    repair_path(&path, win::Kind::File, Role::Private, &mut out);
                }
                match win::open(&tree.keys(), win::Kind::Dir, false, true) {
                    Ok(Some(keys)) => {
                        for key in key_files(&tree.keys()) {
                            repair_path(&key, win::Kind::File, Role::Private, &mut out);
                        }
                        repair_entry(&tree.keys(), &keys, Role::PrivateDir, &mut out);
                    }
                    Ok(None) => {}
                    Err(why) => out.left.push(why),
                }
                repair_entry(tree.folder(), &folder, Role::Folder, &mut out);
            }
        }
        Ok(None) => {}
        Err(why) => out.left.push(why),
    }
    repair_path(&tree.record(), win::Kind::File, Role::Private, &mut out);
    repair_entry(tree.parent(), &parent, Role::Parent, &mut out);
    out
}

/// Does the machine folder hold a config R may make the machine's: one owned
/// by SYSTEM, Administrators or an administrator. Until it does, the folder is
/// a waiting or attended host's, where the person's own daemon stages files,
/// and R leaves it as `%ProgramData%` hands it down. A config another account
/// put there does not make it the machine's.
#[cfg(windows)]
fn holds_a_config_it_may_take(tree: &MachineTree, out: &mut Repairs) -> bool {
    let config = tree.config();
    let entry = match win::open(&config, win::Kind::File, false, false) {
        Ok(Some(entry)) => entry,
        Ok(None) => return false,
        Err(why) => {
            out.left.push(why);
            return false;
        }
    };
    match win::read_sd(&entry.file) {
        Ok(sd) => win::classify(&sd.owner) != win::Owner::Other,
        Err(e) => {
            out.left
                .push(format!("{}: reading its security: {e}", config.display()));
            false
        }
    }
}

#[cfg(windows)]
fn repair_path(path: &Path, kind: win::Kind, role: Role, out: &mut Repairs) {
    match win::open(path, kind, false, true) {
        Ok(Some(entry)) => repair_entry(path, &entry, role, out),
        Ok(None) => {}
        Err(why) => out.left.push(why),
    }
}

#[cfg(windows)]
fn repair_entry(path: &Path, entry: &win::Entry, role: Role, out: &mut Repairs) {
    let sd = match win::read_sd(&entry.file) {
        Ok(sd) => sd,
        Err(e) => {
            out.left
                .push(format!("{}: reading its security: {e}", path.display()));
            return;
        }
    };
    let owner = win::classify(&sd.owner);
    if owner == win::Owner::Other {
        out.left.push(format!(
            "{} is left as it is: it is owned by an account that is not an administrator \
             of this computer",
            path.display()
        ));
        return;
    }
    // The operator's rule: an administrator's own entry is given to the
    // Administrators group. The machine folder is the group's even when
    // SYSTEM made it, as it always was.
    let take = owner == win::Owner::Admin || (role == Role::Folder && sd.owner == SYSTEM);
    let dacl = match role {
        Role::Parent => parent_cut(&sd).map(|aces| (aces, sd.protected)),
        Role::Folder | Role::PrivateDir => {
            let flags = OBJECT_INHERIT | CONTAINER_INHERIT;
            (!is_sealed(&sd, flags)).then(|| (sealed_aces(flags), true))
        }
        Role::Private => (!is_sealed(&sd, 0)).then(|| (sealed_aces(0), true)),
    };
    if !take && dacl.is_none() {
        return;
    }
    if !entry.writable {
        out.left.push(format!(
            "{} is not repaired: this process may not change its owner or DACL",
            path.display()
        ));
        return;
    }
    let what = match (take, &dacl) {
        (true, Some(_)) => "given to Administrators, and its DACL set",
        (true, None) => "given to Administrators",
        (false, _) if role == Role::Parent => "another account's entry cut to read and execute",
        (false, _) => "its DACL set to SYSTEM and Administrators alone",
    };
    match win::write_sd(
        &entry.file,
        take,
        dacl.as_ref()
            .map(|(aces, protected)| (aces.as_slice(), *protected)),
    ) {
        Ok(()) => out.changed.push(format!("{}: {what}", path.display())),
        Err(e) => out
            .left
            .push(format!("{}: the repair failed: {e}", path.display())),
    }
}

/// The key files the daemon loads from `keys`: `<tenant id>.json` and
/// `<tenant id>.controllers.json` (`peer_auth::key_path`, `pins_path`), a
/// tenant id being 24 hex digits. Anything else there (a backup, a temporary
/// file) is never loaded, and neither repaired nor checked.
#[cfg(windows)]
fn key_files(keys: &Path) -> Vec<PathBuf> {
    fn loaded(name: &str) -> bool {
        let id = name
            .strip_suffix(".controllers.json")
            .or_else(|| name.strip_suffix(".json"));
        id.is_some_and(|id| id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    }
    let Ok(dir) = std::fs::read_dir(keys) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = dir
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(loaded))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// An elevated save of the machine config takes the machine folder for
/// Administrators before it writes. RED: master takes nothing over.
#[cfg(windows)]
pub fn adopt_machine_folder(_tree: &MachineTree) -> Result<bool, String> {
    Ok(false)
}

/// B: may SYSTEM run the machine config under `tree`? `Ok` when the parent, the
/// machine folder, the config, the keys folder and each key file the daemon
/// loads are plain folders and files, owned by SYSTEM or Administrators, and no
/// other account may change any of them (the parent: may rename or replace
/// what it holds). The config is opened for reading, as the worker will open
/// it (#3096). Its `.prev` is checked where it is loaded instead
/// ([`read_checked`], from `config::load`). `Err` names the entry and the
/// reason; with no config at all it says so.
#[cfg(windows)]
pub fn check(tree: &MachineTree) -> Result<(), String> {
    check_chain(tree).map_err(|refusal| match refusal {
        Refusal::Absent => format!("there is no machine config at {}", tree.config().display()),
        Refusal::NotPrivate(why) => why,
    })
}

#[cfg(windows)]
fn check_chain(tree: &MachineTree) -> Result<(), Refusal> {
    // No config, no question: a waiting or attended host, whatever its folder
    // looks like, is quietly one with no machine enrollment.
    match std::fs::symlink_metadata(tree.config()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Refusal::Absent),
        _ => {}
    }
    let open = |path: &Path, kind: win::Kind, read_data: bool| {
        win::open(path, kind, read_data, false).map_err(Refusal::NotPrivate)
    };
    let Some(parent) = open(tree.parent(), win::Kind::Dir, false)? else {
        return Err(Refusal::Absent);
    };
    judge(tree.parent(), &parent, SWAP)?;
    let Some(folder) = open(tree.folder(), win::Kind::Dir, false)? else {
        return Err(Refusal::Absent);
    };
    judge(tree.folder(), &folder, WRITE_LIKE)?;
    let config = tree.config();
    let Some(file) = open(&config, win::Kind::File, true)? else {
        return Err(Refusal::Absent);
    };
    judge(&config, &file, WRITE_LIKE)?;
    // `.prev` is not part of it: it is loaded only to recover a config that
    // does not parse, and checked there (`config::load_prev`). An old copy
    // another account made is replaced by the next save, never a reason the
    // device stops.
    if let Some(keys) = open(&tree.keys(), win::Kind::Dir, false)? {
        judge(&tree.keys(), &keys, WRITE_LIKE)?;
        for key in key_files(&tree.keys()) {
            if let Some(file) = open(&key, win::Kind::File, false)? {
                judge(&key, &file, WRITE_LIKE)?;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn judge(path: &Path, entry: &win::Entry, counts: u32) -> Result<(), Refusal> {
    let sd = win::read_sd(&entry.file).map_err(|e| {
        Refusal::NotPrivate(format!("{}: reading its security: {e}", path.display()))
    })?;
    verdict(&sd, counts, &path.display().to_string()).map_err(Refusal::NotPrivate)
}

/// R (unless switched off), then B: the question the service host asks at its
/// start and at each tick with no SYSTEM worker, and the worker's ladder at
/// its start (`roomlerd`'s `win_service::supervisor::rung_opens`, one function
/// for both). With the check switched off a refusal is logged and the
/// config's read open decides, as before #3609. A refusal is logged once per
/// reason, not once per tick.
#[cfg(windows)]
pub fn private_machine_config(tree: &MachineTree, switches: Switches) -> Result<(), Refusal> {
    if switches.repair {
        log_repairs(&repair(tree, false));
    }
    let verdict = check_chain(tree);
    note_verdict(&verdict, switches.check);
    match verdict {
        Err(Refusal::NotPrivate(_)) if !switches.check => std::fs::File::open(tree.config())
            .map(|_| ())
            .map_err(|_| Refusal::Absent),
        other => other,
    }
}

/// [`private_machine_config`] as a yes or no.
#[cfg(windows)]
pub fn machine_config_opens(tree: &MachineTree, switches: Switches) -> bool {
    private_machine_config(tree, switches).is_ok()
}

/// Whether an elevated writer may fold a new enrollment into the machine
/// config under `tree`. RED: master folds whatever is there.
#[cfg(windows)]
pub fn may_fold(_tree: &MachineTree, _switches: Switches) -> Result<(), String> {
    Ok(())
}

/// Log what a repair changed, once per change, and what it left, once until
/// that changes: the host repairs at every tick while it waits.
#[cfg(windows)]
pub fn log_repairs(repairs: &Repairs) {
    static LEFT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    for line in &repairs.changed {
        tracing::info!(%line, "machine tree: repaired");
    }
    let mut last = LEFT.lock().unwrap_or_else(|e| e.into_inner());
    for line in repairs.left.iter().filter(|l| !last.contains(l)) {
        tracing::warn!(%line, "machine tree: not repaired");
    }
    *last = repairs.left.clone();
}

#[cfg(windows)]
fn note_verdict(verdict: &Result<(), Refusal>, enforced: bool) {
    static LAST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match verdict {
        Err(Refusal::NotPrivate(why)) => {
            if last.as_deref() != Some(why.as_str()) {
                if enforced {
                    tracing::warn!(
                        reason = %why,
                        "machine tree: the machine enrollment is not private, so the service does \
                         not run it as SYSTEM; the device runs as one with no machine enrollment"
                    );
                } else {
                    tracing::warn!(
                        reason = %why,
                        "machine tree: the machine enrollment is not private; \
                         ROOMLERD_MACHINE_TREE_CHECK=0, so it runs anyway"
                    );
                }
                *last = Some(why.clone());
            }
        }
        Ok(()) => {
            if last.take().is_some() {
                tracing::info!("machine tree: the machine enrollment is private again");
            }
        }
        Err(Refusal::Absent) => {
            *last = None;
        }
    }
}

/// `keep-system-context`'s read of the attempt record: R first (unless
/// switched off), then the record only when the parent gives no other account
/// a right to replace what it holds and the record is a plain file owned by
/// SYSTEM or Administrators that no other account may change. Read through the
/// handle it was checked on. `Ok(None)`: there is none. `Err` names the
/// reason, and the caller reads it as no record, which is off. With the check
/// switched off a refusal is logged and the record read as before.
#[cfg(windows)]
pub fn read_record(tree: &MachineTree, switches: Switches) -> Result<Option<Vec<u8>>, String> {
    if switches.repair {
        log_repairs(&repair(tree, false));
    }
    match read_record_checked(tree) {
        Err(why) if !switches.check => {
            tracing::warn!(
                reason = %why,
                "machine tree: the attempt record is not private; \
                 ROOMLERD_MACHINE_TREE_CHECK=0, so it is read anyway"
            );
            match std::fs::read(tree.record()) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(format!("{}: {e}", tree.record().display())),
            }
        }
        other => other,
    }
}

#[cfg(windows)]
fn read_record_checked(tree: &MachineTree) -> Result<Option<Vec<u8>>, String> {
    let Some(parent) = win::open(tree.parent(), win::Kind::Dir, false, false)? else {
        return Ok(None);
    };
    judge_text(tree.parent(), &parent, SWAP)?;
    let record = tree.record();
    let Some(file) = win::open(&record, win::Kind::File, true, false)? else {
        return Ok(None);
    };
    judge_text(&record, &file, WRITE_LIKE)?;
    win::read_to_end(&file.file, &record, 64 * 1024).map(Some)
}

#[cfg(windows)]
fn judge_text(path: &Path, entry: &win::Entry, counts: u32) -> Result<(), String> {
    judge(path, entry, counts).map_err(|refusal| match refusal {
        Refusal::NotPrivate(why) => why,
        Refusal::Absent => format!("{} is not there", path.display()),
    })
}

/// Read `path` whole (at most `max_len` bytes) when it is a plain file owned by
/// SYSTEM or Administrators that no other account may change, through the
/// handle it was checked on: the source of a save's `.prev` rotation.
/// `Ok(None)` when it is not there.
#[cfg(windows)]
pub fn read_checked(path: &Path, max_len: u64) -> Result<Option<Vec<u8>>, String> {
    let Some(file) = win::open(path, win::Kind::File, true, false)? else {
        return Ok(None);
    };
    judge_text(path, &file, WRITE_LIKE)?;
    win::read_to_end(&file.file, path, max_len).map(Some)
}

/// W: give a file this process just made ([`create_fresh`]) Administrators as
/// its owner and a protected DACL of SYSTEM and Administrators alone, through
/// its handle, before it takes its name. One call, so the two land together or
/// not at all (a file has nothing below it to hand a DACL down to). `Err` when
/// this process may not (it is not SYSTEM or an elevated administrator): the
/// file then keeps what its folder hands down, its writer can still rename it
/// into place, and the next [`repair`] does the rest.
#[cfg(windows)]
pub fn seal_new_file(file: &std::fs::File) -> std::io::Result<()> {
    win::set_sd(file, true, Some((&sealed_aces(0), true)))
}

/// FR-94's reader, shared (it moved here from `roomlerd`'s `exec_service`):
/// read `path`, a file in `dir`, if neither is a link and both are owned by
/// SYSTEM or Administrators with no write-like ACE for anyone else. Checked on
/// the handle the content is read from: a name can be swapped between a check
/// and a read, an open file cannot. `Ok(None)` = not there. `what` names the
/// file in errors.
#[cfg(windows)]
pub fn read_private(
    dir: &Path,
    path: &Path,
    what: &str,
    max_len: u64,
) -> Result<Option<String>, String> {
    if let Some(link) = crate::recording_dir::link_component(path) {
        return Err(format!("{} is a link", link.display()));
    }
    let Some(dir_entry) = win::open(dir, win::Kind::Dir, false, false)? else {
        return Ok(None);
    };
    let sd = win::read_sd(&dir_entry.file)
        .map_err(|e| format!("the anchor's directory: reading its security: {e}"))?;
    verdict(&sd, WRITE_LIKE, "the anchor's directory")?;
    let Some(file) = win::open(path, win::Kind::File, true, false)? else {
        return Ok(None);
    };
    let len = file
        .file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if len > max_len {
        return Err(format!("{what} is too large to be one"));
    }
    let sd = win::read_sd(&file.file).map_err(|e| format!("{what}: reading its security: {e}"))?;
    verdict(&sd, WRITE_LIKE, what)?;
    let bytes = win::read_to_end(&file.file, path, max_len)?;
    String::from_utf8(bytes)
        .map_err(|e| format!("reading {what}: {e}"))
        .map(Some)
}

/// The handle-level Windows calls: open an entry itself, read and write its
/// owner and DACL, and look up whether an owner is an administrator.
#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{GENERIC_READ, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authentication::Identity::{
        LSA_OBJECT_ATTRIBUTES, LsaClose, LsaFreeMemory, LsaOpenPolicy, LsaQueryInformationPolicy,
        POLICY_ACCOUNT_DOMAIN_INFO, POLICY_VIEW_LOCAL_INFORMATION, PolicyAccountDomainInformation,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
        GetAce, GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, LookupAccountNameW, LookupAccountSidW,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SE_DACL_PROTECTED, SID_NAME_USE, SidTypeUser, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        READ_CONTROL,
    };
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };

    use super::{ACE_ALLOW, ACE_DENY, Ace, Sd};

    const WRITE_DAC: u32 = 0x4_0000;
    const WRITE_OWNER: u32 = 0x8_0000;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Kind {
        Dir,
        File,
    }

    /// An entry opened by [`open`]. `writable`: the handle may change its
    /// owner and DACL.
    pub(super) struct Entry {
        pub(super) file: std::fs::File,
        pub(super) writable: bool,
    }

    /// Open `path` itself, never what a junction or a symbolic link leads to
    /// (`FILE_FLAG_OPEN_REPARSE_POINT`). `Ok(None)`: it is not there. A link,
    /// or an entry of the other kind, is an `Err` naming it. `read_data` adds
    /// the right to read its content through this handle; `write` asks for
    /// `WRITE_DAC` and `WRITE_OWNER` too, and does without them when this
    /// process may not have them. `FILE_FLAG_BACKUP_SEMANTICS` opens a folder
    /// at all; the daemon never enables the privilege that would let it
    /// override a DACL with it.
    pub(super) fn open(
        path: &Path,
        kind: Kind,
        read_data: bool,
        write: bool,
    ) -> Result<Option<Entry>, String> {
        let base = READ_CONTROL | if read_data { GENERIC_READ } else { 0 };
        let attempt = |access: u32| {
            std::fs::OpenOptions::new()
                .access_mode(access)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
        };
        let (file, writable) = match write.then(|| attempt(base | WRITE_DAC | WRITE_OWNER)) {
            Some(Ok(file)) => (file, true),
            _ => match attempt(base) {
                Ok(file) => (file, false),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(format!("{}: {e}", path.display())),
            },
        };
        let meta = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let noun = match kind {
            Kind::Dir => "folder",
            Kind::File => "file",
        };
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "{} is a link (a junction or a symbolic link), not a plain {noun}",
                path.display()
            ));
        }
        if meta.is_dir() != (kind == Kind::Dir) {
            return Err(format!("{} is not a plain {noun}", path.display()));
        }
        Ok(Some(Entry { file, writable }))
    }

    /// Read at most `max_len` bytes of `file`, opened with `read_data`.
    pub(super) fn read_to_end(
        file: &std::fs::File,
        path: &Path,
        max_len: u64,
    ) -> Result<Vec<u8>, String> {
        use std::io::Read as _;
        let len = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        if len > max_len {
            return Err(format!("{} is too large", path.display()));
        }
        let mut bytes = Vec::new();
        file.take(max_len)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Ok(bytes)
    }

    fn sid_string(sid: PSID) -> std::io::Result<String> {
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: a SID inside a descriptor or buffer that outlives the call;
        // on success `text` is LocalAlloc'd, freed here.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the NUL-terminated string the call above wrote.
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        // SAFETY: `len` units were just read from `text`.
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: allocated by ConvertSidToStringSidW, freed once.
        unsafe { LocalFree(text as _) };
        Ok(s)
    }

    /// The owner and DACL of the entry behind `file`, read through its handle.
    pub(super) fn read_sd(file: &std::fs::File) -> std::io::Result<Sd> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a live handle opened with READ_CONTROL; `owner` and `dacl`
        // point into `sd`, a LocalAlloc'd descriptor freed below.
        let rc = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        let read = (|| {
            let owner = sid_string(owner)?;
            let mut control = 0u16;
            let mut revision = 0u32;
            // SAFETY: the descriptor read above.
            if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let protected = control & SE_DACL_PROTECTED != 0;
            if dacl.is_null() {
                return Ok(Sd {
                    owner,
                    protected,
                    dacl: None,
                });
            }
            let mut info = ACL_SIZE_INFORMATION {
                AceCount: 0,
                AclBytesInUse: 0,
                AclBytesFree: 0,
            };
            // SAFETY: `dacl` is an ACL inside `sd`; `info` is sized for the class.
            let ok = unsafe {
                GetAclInformation(
                    dacl,
                    (&raw mut info).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut aces = Vec::with_capacity(info.AceCount as usize);
            for i in 0..info.AceCount {
                let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                // SAFETY: `i` < AceCount; `ace` points into the ACL.
                if unsafe { GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: every ACE starts with an ACE_HEADER.
                let header = unsafe { std::ptr::read_unaligned(ace as *const ACE_HEADER) };
                // An allow or a deny ACE is the header (4 bytes), the mask (4),
                // then the SID. Any other kind is told apart by its type alone.
                let (mask, sid) = if header.AceType == ACE_ALLOW || header.AceType == ACE_DENY {
                    // SAFETY: the type says this is the layout, and GetAce hands
                    // back an ACE that lies whole inside the ACL.
                    let mask = unsafe {
                        std::ptr::read_unaligned((ace as *const u8).add(4) as *const u32)
                    };
                    // SAFETY: the SID starts 8 bytes in, inside the ACE.
                    (
                        mask,
                        sid_string(unsafe { (ace as *mut u8).add(8) } as PSID)?,
                    )
                } else {
                    (0, String::new())
                };
                aces.push(Ace {
                    kind: header.AceType,
                    flags: header.AceFlags,
                    mask,
                    sid,
                });
            }
            Ok(Sd {
                owner,
                protected,
                dacl: Some(aces),
            })
        })();
        // SAFETY: the descriptor GetSecurityInfo allocated, freed once.
        unsafe { LocalFree(sd as _) };
        read
    }

    /// A security descriptor made from SDDL, freed on drop.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Descriptor {
        fn from_sddl(sddl: &str) -> std::io::Result<Self> {
            let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: a NUL-terminated string; `sd` receives a LocalAlloc'd
            // descriptor, freed in Drop; the size out-param is optional.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(sd))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the one descriptor ConvertStringSecurityDescriptor… made.
            unsafe { LocalFree(self.0 as _) };
        }
    }

    /// One explicit ACE in SDDL. An inherited ACE is never written: the
    /// system adds those back from the parent.
    fn ace_sddl(ace: &Ace) -> std::io::Result<String> {
        let kind = match ace.kind {
            ACE_ALLOW => "A",
            ACE_DENY => "D",
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("an ACE of type {other} cannot be written back"),
                ));
            }
        };
        let mut flags = String::new();
        for (bit, word) in [(0x1, "OI"), (0x2, "CI"), (0x4, "NP"), (0x8, "IO")] {
            if ace.flags & bit != 0 {
                flags.push_str(word);
            }
        }
        Ok(format!("({kind};{flags};0x{:x};;;{})", ace.mask, ace.sid))
    }

    /// Write the entry behind `file`, through its handle: `dacl` (protected
    /// or inheriting, the second half) when given, then Administrators as its
    /// owner when `take`. On a folder the system hands the new inheritable
    /// ACEs down to what it holds, and never follows a link below it.
    ///
    /// ⚠️ On a folder, two calls, the DACL first. With the owner in the same
    /// call, the hand-down stopped at each entry below that another account
    /// owns, when the process held the privileges that override a DACL
    /// disabled, as the service does (measured: such a child kept every ACE it
    /// had inherited before). The DACL alone reaches them. The DACL never
    /// leaves SYSTEM or Administrators out, so taking the owner after it is
    /// still allowed. A file has nothing below it: one call, both or neither.
    pub(super) fn write_sd(
        file: &std::fs::File,
        take: bool,
        dacl: Option<(&[Ace], bool)>,
    ) -> std::io::Result<()> {
        if take && dacl.is_some() && file.metadata()?.is_dir() {
            set_sd(file, false, dacl)?;
            return set_sd(file, true, None);
        }
        set_sd(file, take, dacl)
    }

    /// [`write_sd`]'s one call: the owner (when `take`) and `dacl` together,
    /// both or neither.
    pub(super) fn set_sd(
        file: &std::fs::File,
        take: bool,
        dacl: Option<(&[Ace], bool)>,
    ) -> std::io::Result<()> {
        let mut sddl = String::new();
        if take {
            sddl.push_str("O:BA");
        }
        if let Some((aces, protected)) = dacl {
            sddl.push_str(if protected { "D:P" } else { "D:" });
            for ace in aces {
                sddl.push_str(&ace_sddl(ace)?);
            }
        }
        let sd = Descriptor::from_sddl(&sddl)?;
        let mut info = 0;
        let mut owner: PSID = std::ptr::null_mut();
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        if take {
            // SAFETY: the descriptor made above; `owner` points into it.
            if unsafe { GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            info |= OWNER_SECURITY_INFORMATION;
        }
        if let Some((_, protected)) = dacl {
            // SAFETY: the descriptor made above; `acl` points into it.
            if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) }
                == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            info |= DACL_SECURITY_INFORMATION
                | if protected {
                    PROTECTED_DACL_SECURITY_INFORMATION
                } else {
                    UNPROTECTED_DACL_SECURITY_INFORMATION
                };
        }
        // SAFETY: a live handle opened with WRITE_DAC and WRITE_OWNER; `owner`
        // and `acl` point into `sd`, alive until the end of this function.
        let rc = unsafe {
            SetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                info,
                owner,
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    /// Who owns an entry, as R treats it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Owner {
        /// SYSTEM or the Administrators group.
        Trusted,
        /// A member of the computer's Administrators group, as its own account
        /// database reads (`look_up`).
        Admin,
        /// Anyone else, and any account whose membership cannot be read.
        Other,
    }

    pub(super) fn classify(sid: &str) -> Owner {
        if super::trusted(sid) {
            Owner::Trusted
        } else if is_admin(sid) {
            Owner::Admin
        } else {
            Owner::Other
        }
    }

    /// Whether `sid` is an account of this computer in its Administrators
    /// group. Kept a minute per SID: the host asks at every tick while it
    /// waits, and a membership changes rarely.
    fn is_admin(sid: &str) -> bool {
        static SEEN: OnceLock<Mutex<HashMap<String, (Instant, bool)>>> = OnceLock::new();
        const KEEP: Duration = Duration::from_secs(60);
        let seen = SEEN.get_or_init(Default::default);
        if let Some((at, yes)) = seen.lock().unwrap_or_else(|e| e.into_inner()).get(sid)
            && at.elapsed() < KEEP
        {
            return *yes;
        }
        let yes = look_up(sid).unwrap_or(false);
        seen.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sid.to_string(), (Instant::now(), yes));
        yes
    }

    /// `Some(true)` only when the computer's own account database says so,
    /// read locally and never from a domain controller:
    /// - an account of this computer is a member of the Administrators group,
    ///   directly or through another group (`NetUserGetLocalGroups`,
    ///   `LG_INCLUDE_INDIRECT`);
    /// - any other account (a domain's) only when the group names its SID
    ///   directly (`NetLocalGroupGetMembers`, level 0). Its membership through
    ///   the domain's own groups is never looked up, so such an account reads
    ///   as no administrator here.
    fn look_up(sid: &str) -> Option<bool> {
        if machine_domain().is_some_and(|domain| super::is_local_account(sid, domain))
            && let Some(account) = account_name(sid)
            && let Some(groups) = local_groups(&account)
            && groups
                .iter()
                .any(|group| sid_of(group).as_deref() == Some(super::ADMINISTRATORS))
        {
            return Some(true);
        }
        administrators().map(|members| members.iter().any(|member| member == sid))
    }

    /// The SIDs the Administrators group names directly, from this computer's
    /// account database. Level 0 answers SIDs only, so nothing is translated
    /// through a domain controller.
    pub(super) fn administrators() -> Option<Vec<String>> {
        const MAX_PREFERRED_LENGTH: u32 = u32::MAX;
        let api = net_api()?;
        let group = wide(&group_name(super::ADMINISTRATORS)?);
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut read = 0u32;
        let mut total = 0u32;
        // SAFETY: NULL names this computer; level 0 fills `buf` with an array
        // of LOCALGROUP_MEMBERS_INFO_0 (one SID pointer each), freed below;
        // no resume handle, so one call answers all of them.
        let rc = unsafe {
            (api.members)(
                std::ptr::null(),
                group.as_ptr(),
                0,
                &mut buf,
                MAX_PREFERRED_LENGTH,
                &mut read,
                &mut total,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            if !buf.is_null() {
                // SAFETY: allocated by the call above.
                unsafe { (api.free)(buf.cast()) };
            }
            return None;
        }
        let mut members = Vec::with_capacity(read as usize);
        for i in 0..read as usize {
            // SAFETY: `read` entries of one SID pointer each.
            let sid = unsafe { *(buf as *const PSID).add(i) };
            if let Ok(text) = sid_string(sid) {
                members.push(text);
            }
        }
        if !buf.is_null() {
            // SAFETY: allocated by the call above, freed once.
            unsafe { (api.free)(buf.cast()) };
        }
        Some(members)
    }

    /// The name of a well-known group on this computer (localized: German
    /// Windows calls the Administrators group "Administratoren").
    fn group_name(sid: &str) -> Option<String> {
        let wide_sid = wide(sid);
        let mut psid: PSID = std::ptr::null_mut();
        // SAFETY: a NUL-terminated string; `psid` is LocalAlloc'd, freed below.
        if unsafe { ConvertStringSidToSidW(wide_sid.as_ptr(), &mut psid) } == 0 {
            return None;
        }
        let mut name = vec![0u16; 256];
        let mut domain = vec![0u16; 256];
        let mut name_len = name.len() as u32;
        let mut domain_len = domain.len() as u32;
        let mut kind: SID_NAME_USE = 0;
        // SAFETY: a valid SID; both buffers hold the lengths given.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                psid,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        // SAFETY: allocated by ConvertStringSidToSidW, freed once.
        unsafe { LocalFree(psid as _) };
        (ok != 0).then(|| from_wide(&name))
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// This computer's account domain SID, from the local security policy.
    fn machine_domain() -> Option<&'static str> {
        static DOMAIN: OnceLock<Option<String>> = OnceLock::new();
        DOMAIN
            .get_or_init(|| {
                // SAFETY: a zeroed LSA_OBJECT_ATTRIBUTES is what LsaOpenPolicy
                // documents for the local system.
                let attributes = LSA_OBJECT_ATTRIBUTES {
                    Length: std::mem::size_of::<LSA_OBJECT_ATTRIBUTES>() as u32,
                    ..unsafe { std::mem::zeroed() }
                };
                let mut policy = 0isize;
                // SAFETY: NULL names the local system; the out-param is written
                // on success, closed below.
                let status = unsafe {
                    LsaOpenPolicy(
                        std::ptr::null(),
                        &attributes,
                        POLICY_VIEW_LOCAL_INFORMATION as u32,
                        &mut policy,
                    )
                };
                if status != 0 {
                    return None;
                }
                let mut info: *mut core::ffi::c_void = std::ptr::null_mut();
                // SAFETY: a policy handle opened above; `info` receives an
                // LSA-allocated POLICY_ACCOUNT_DOMAIN_INFO, freed below.
                let status = unsafe {
                    LsaQueryInformationPolicy(policy, PolicyAccountDomainInformation, &mut info)
                };
                let sid = if status == 0 && !info.is_null() {
                    // SAFETY: the class asked for says this is the layout.
                    let domain = unsafe { &*(info as *const POLICY_ACCOUNT_DOMAIN_INFO) };
                    let sid = sid_string(domain.DomainSid).ok();
                    // SAFETY: allocated by LsaQueryInformationPolicy, freed once.
                    unsafe { LsaFreeMemory(info) };
                    sid
                } else {
                    None
                };
                // SAFETY: opened above, closed once.
                unsafe { LsaClose(policy) };
                sid
            })
            .as_deref()
    }

    /// `DOMAIN\name` of a user account, from this computer.
    fn account_name(sid: &str) -> Option<String> {
        let wide_sid = wide(sid);
        let mut psid: PSID = std::ptr::null_mut();
        // SAFETY: a NUL-terminated string; `psid` is LocalAlloc'd, freed below.
        if unsafe { ConvertStringSidToSidW(wide_sid.as_ptr(), &mut psid) } == 0 {
            return None;
        }
        let mut name = vec![0u16; 256];
        let mut domain = vec![0u16; 256];
        let mut name_len = name.len() as u32;
        let mut domain_len = domain.len() as u32;
        let mut kind: SID_NAME_USE = 0;
        // SAFETY: a valid SID; both buffers hold the lengths given.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                psid,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        // SAFETY: allocated by ConvertStringSidToSidW, freed once.
        unsafe { LocalFree(psid as _) };
        if ok == 0 || kind != SidTypeUser {
            return None;
        }
        Some(format!("{}\\{}", from_wide(&domain), from_wide(&name)))
    }

    /// The SID of an account or group name, from this computer.
    fn sid_of(name: &str) -> Option<String> {
        let wide_name = wide(name);
        let mut sid = vec![0u8; 256];
        let mut sid_len = sid.len() as u32;
        let mut domain = vec![0u16; 256];
        let mut domain_len = domain.len() as u32;
        let mut kind: SID_NAME_USE = 0;
        // SAFETY: a NUL-terminated name; both buffers hold the lengths given.
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                wide_name.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        if ok == 0 {
            return None;
        }
        sid_string(sid.as_mut_ptr().cast()).ok()
    }

    /// `NetUserGetLocalGroups`, `NetLocalGroupGetMembers` and
    /// `NetApiBufferFree`, from System32's
    /// netapi32 loaded at run time: the setup wizard links this crate and runs
    /// from a folder anyone may write, so a static import would let a DLL
    /// beside it answer in netapi32's place (`.claude/rules/installers.md`).
    struct NetApi {
        groups: unsafe extern "system" fn(
            *const u16,
            *const u16,
            u32,
            u32,
            *mut *mut u8,
            u32,
            *mut u32,
            *mut u32,
        ) -> u32,
        members: unsafe extern "system" fn(
            *const u16,
            *const u16,
            u32,
            *mut *mut u8,
            u32,
            *mut u32,
            *mut u32,
            *mut usize,
        ) -> u32,
        free: unsafe extern "system" fn(*const core::ffi::c_void) -> u32,
    }

    fn net_api() -> Option<&'static NetApi> {
        static API: OnceLock<Option<NetApi>> = OnceLock::new();
        API.get_or_init(|| {
            let name = wide("netapi32.dll");
            // SAFETY: a NUL-terminated name, loaded from System32 with its
            // own dependencies from there too; never freed.
            let lib = unsafe {
                LoadLibraryExW(
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            if lib.is_null() {
                return None;
            }
            // SAFETY: NUL-terminated export names of the module just loaded.
            let groups = unsafe { GetProcAddress(lib, c"NetUserGetLocalGroups".as_ptr().cast()) }?;
            // SAFETY: as above.
            let members =
                unsafe { GetProcAddress(lib, c"NetLocalGroupGetMembers".as_ptr().cast()) }?;
            // SAFETY: as above.
            let free = unsafe { GetProcAddress(lib, c"NetApiBufferFree".as_ptr().cast()) }?;
            // SAFETY: the documented signatures of these three exports.
            Some(unsafe {
                NetApi {
                    groups: std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        unsafe extern "system" fn(
                            *const u16,
                            *const u16,
                            u32,
                            u32,
                            *mut *mut u8,
                            u32,
                            *mut u32,
                            *mut u32,
                        ) -> u32,
                    >(groups),
                    members: std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        unsafe extern "system" fn(
                            *const u16,
                            *const u16,
                            u32,
                            *mut *mut u8,
                            u32,
                            *mut u32,
                            *mut u32,
                            *mut usize,
                        ) -> u32,
                    >(members),
                    free: std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        unsafe extern "system" fn(*const core::ffi::c_void) -> u32,
                    >(free),
                }
            })
        })
        .as_ref()
    }

    /// The names of the local groups `account` is a member of, directly or
    /// through a group (`LG_INCLUDE_INDIRECT`), from this computer's account
    /// database.
    fn local_groups(account: &str) -> Option<Vec<String>> {
        const LG_INCLUDE_INDIRECT: u32 = 1;
        const MAX_PREFERRED_LENGTH: u32 = u32::MAX;
        let api = net_api()?;
        let wide_account = wide(account);
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut read = 0u32;
        let mut total = 0u32;
        // SAFETY: NULL names this computer; level 0 fills `buf` with an array
        // of LOCALGROUP_USERS_INFO_0 (one name pointer each), freed below.
        let rc = unsafe {
            (api.groups)(
                std::ptr::null(),
                wide_account.as_ptr(),
                0,
                LG_INCLUDE_INDIRECT,
                &mut buf,
                MAX_PREFERRED_LENGTH,
                &mut read,
                &mut total,
            )
        };
        if rc != 0 {
            if !buf.is_null() {
                // SAFETY: allocated by the call above.
                unsafe { (api.free)(buf.cast()) };
            }
            return None;
        }
        let mut names = Vec::with_capacity(read as usize);
        for i in 0..read as usize {
            // SAFETY: `read` entries of one pointer each.
            let name = unsafe { *(buf as *const *const u16).add(i) };
            if name.is_null() {
                continue;
            }
            // SAFETY: a NUL-terminated name inside the buffer.
            let len = (0..).take_while(|&j| unsafe { *name.add(j) } != 0).count();
            // SAFETY: `len` units were just read.
            names.push(String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(name, len)
            }));
        }
        if !buf.is_null() {
            // SAFETY: allocated by the call above, freed once.
            unsafe { (api.free)(buf.cast()) };
        }
        Some(names)
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
        // Set whole, not through `icacls /grant`: on CI's runner a new temp
        // folder comes with a full-control `(OI)(CI)` entry of its own for the
        // runner's account, which `/inheritance:r` keeps, and the parent made
        // below would inherit it.
        set_dacl(
            &pd,
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)(A;CI;0x116;;;BU)",
        );
        pd
    }

    /// Give `path` exactly the DACL `sddl` (`D:…`), by name.
    pub fn set_dacl(path: &Path, sddl: &str) {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACL, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        };
        let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated string; `sd` is LocalAlloc'd, freed below.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "SDDL {sddl}");
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: the descriptor made above; `acl` points into it.
        let ok = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) };
        assert_ne!(ok, 0, "the DACL of {sddl}");
        let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path; `acl` lives in `sd` until it is freed.
        let rc = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            )
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptor…, freed once.
        unsafe { LocalFree(sd as _) };
        assert_eq!(rc, 0, "setting {}'s DACL", path.display());
    }

    /// `%ProgramData%\roomler` and its machine folder, as a host with the
    /// service and no machine enrollment has them: both made below the
    /// stand-in, inheriting all they hold. The parent is given to
    /// Administrators, as Setup or the service's own start leaves it.
    pub fn waiting_tree(root: &Path) -> super::MachineTree {
        let parent = program_data(root).join("roomler");
        std::fs::create_dir(&parent).expect("the parent");
        let folder = parent.join("roomler");
        std::fs::create_dir(&folder).expect("the machine folder");
        // Inherited entries only, whatever entry of its own a new folder may
        // come with on the machine running this (CI's runner adds one).
        for dir in [&parent, &folder] {
            assert!(icacls(dir, &["/reset"]), "{} inherits", dir.display());
        }
        assert!(
            icacls(&parent, &["/setowner", "*S-1-5-32-544"]),
            "the parent to Administrators"
        );
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

    /// The kill switches take the words every default-on knob takes; anything
    /// else, unset included, keeps the repair and the check on.
    #[test]
    fn only_an_off_word_switches_the_machine_tree_off() {
        for off in ["0", "false", "FALSE", "No", "off", " off ", "\tfalse\n"] {
            assert!(off_word(off), "{off:?}");
        }
        for on in ["1", "true", "yes", "on", "", "disable", "00"] {
            assert!(!off_word(on), "{on:?}");
        }
    }

    /// `map_generic` turns each generic bit into the file rights it stands
    /// for, so a cut never leaves a write hidden in one.
    #[test]
    fn generic_rights_are_mapped_before_a_cut() {
        assert_eq!(map_generic(0x1000_0000), FULL);
        assert_eq!(map_generic(0x8000_0000 | 0x2000_0000), READ_EXECUTE);
        assert_ne!(map_generic(0x4000_0000) & WRITE_LIKE, 0);
        assert_eq!(
            map_generic(0x4000_0000) & READ_EXECUTE & !SYNCHRONIZE,
            0x2_0000
        );
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

        /// #3073, kept: the repair runs as the daemon does, with the
        /// privileges that override a DACL held but disabled, over a tree it
        /// owns none of (here SYSTEM's; in the field the signed-in person's).
        /// Each entry is written once with its final owner and DACL, so SYSTEM
        /// keeps full control of the folder and of every child, the ones it
        /// reaches only through what the folder hands down included.
        #[test]
        fn repair_keeps_system_in_a_tree_someone_else_owns() {
            if !elevated_or_skip("#3073's tree") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            let crashes = tree.folder().join("crashes");
            std::fs::create_dir(&crashes).unwrap();
            std::fs::write(crashes.join("a.json"), b"x").unwrap();
            assert!(icacls(tree.folder(), &["/setowner", "*S-1-5-18", "/T"]));
            disable_dacl_override_privileges();
            repair(&tree, false);
            let folder = sddl(tree.folder());
            assert_eq!(owner(&folder), "BA", "{folder}");
            assert!(sealed(&folder, true), "{folder}");
            for (child, inherited) in [(&crashes, "OICIID"), (&crashes.join("a.json"), "ID")] {
                let got = sddl(child);
                for sid in ["SY", "BA"] {
                    let ace = format!("A;{inherited};FA;;;{sid}");
                    assert!(
                        aces(&got).contains(&ace),
                        "{}: {ace} in {got}",
                        child.display()
                    );
                }
                assert!(!names(&got, USERS), "{}: {got}", child.display());
            }
            assert_eq!(
                owner(&sddl(&tree.config())),
                "SY",
                "SYSTEM keeps the files it owns"
            );
            assert_eq!(check(&tree), Ok(()));
        }

        /// A tree already as the repair leaves it is not written again: the
        /// host repairs at every tick while it waits, and a start repairs
        /// before its check.
        #[test]
        fn repair_changes_nothing_on_a_tree_already_as_it_should_be() {
            if !elevated_or_skip("a second repair") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            std::fs::write(tree.record(), b"{}").unwrap();
            let first = repair(&tree, false);
            assert!(!first.changed.is_empty(), "{first:?}");
            let entries = [
                tree.parent().to_path_buf(),
                tree.folder().to_path_buf(),
                tree.config(),
                tree.prev(),
                tree.keys(),
                tree.record(),
            ];
            let before: Vec<String> = entries.iter().map(|e| sddl(e)).collect();
            let second = repair(&tree, false);
            assert_eq!(second, Repairs::default());
            let after: Vec<String> = entries.iter().map(|e| sddl(e)).collect();
            assert_eq!(after, before);
        }

        /// A host with the service and no machine enrollment (waiting, or
        /// attended): the repair leaves the machine folder as `%ProgramData%`
        /// hands it down, so the person's own daemon may still stage files in
        /// it, and the check says nothing at all. The parent's entry for an
        /// account is still cut, and Users keep the right to add files and
        /// folders that the parent inherits.
        #[test]
        fn a_host_with_no_machine_config_keeps_its_folder_open_and_its_check_quiet() {
            if !elevated_or_skip("a waiting host") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = waiting_tree(root.path());
            assert!(icacls(
                tree.parent(),
                &["/grant", "*S-1-5-32-546:(OI)(CI)F"]
            ));
            let folder = sddl(tree.folder());
            repair(&tree, false);
            assert_eq!(
                sddl(tree.folder()),
                folder.replace("(A;OICIID;FA;;;BG)", "(A;OICIID;0x1200a9;;;BG)")
            );
            let parent = sddl(tree.parent());
            assert!(
                aces(&parent).contains(&"A;CIID;DCLCRPCR;;;BU".to_string()),
                "Users may still add files and folders: {parent}"
            );
            assert_eq!(
                private_machine_config(&tree, Switches::ON),
                Err(Refusal::Absent)
            );
        }

        /// W: a file the writer just made is given Administrators as its
        /// owner and a protected DACL of SYSTEM and Administrators alone,
        /// through its handle, before it takes any name; and a file left under
        /// the name it is made at is replaced, never written into.
        #[test]
        fn a_new_file_is_made_fresh_and_sealed_through_its_handle() {
            if !elevated_or_skip("a new file's owner") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("config.toml.tmp.1");
            std::fs::write(&path, b"left there").unwrap();
            let witness = root.path().join("witness");
            std::fs::hard_link(&path, &witness).unwrap();
            {
                use std::io::Write as _;
                let mut file = create_fresh(&path).expect("made fresh");
                file.write_all(b"new").unwrap();
                seal_new_file(&file).expect("sealed");
            }
            assert_eq!(std::fs::read(&witness).unwrap(), b"left there");
            assert_eq!(std::fs::read(&path).unwrap(), b"new");
            let got = sddl(&path);
            assert_eq!(owner(&got), "BA", "{got}");
            assert!(sealed(&got, false), "{got}");
        }

        /// `.prev` is checked where it is loaded, not with the chain: one
        /// another account owns (an old copy an earlier version made) stops
        /// nothing, the repair leaves it, and it is never read back for a
        /// recovery. The next save replaces it with a new file.
        #[test]
        fn a_previous_copy_another_account_owns_stops_nothing_and_is_never_read() {
            if !elevated_or_skip("a previous copy another account owns") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = enrolled_tree(root.path());
            repair(&tree, false);
            let other = local_guest().unwrap_or_else(|| USERS.to_string());
            assert!(icacls(&tree.prev(), &["/setowner", &format!("*{other}")]));
            let prev = sddl(&tree.prev());
            assert_eq!(check(&tree), Ok(()));
            assert_eq!(sddl(&tree.prev()), prev, "left as it is");
            let why = read_checked(&tree.prev(), 1 << 20).expect_err("not read back");
            assert!(why.contains("owned by an account other than"), "{why}");
            assert_eq!(
                read_checked(&tree.config(), 1 << 20).unwrap().as_deref(),
                Some(&b"agent_token = \"t\"\n"[..])
            );
        }

        /// Who owns an entry, as the repair reads it: SYSTEM and the group are
        /// trusted; this runner's own account, an administrator of this
        /// computer, is one; the built-in Guest and an account of another
        /// domain are not. The group's member list is read through
        /// netapi32's `NetLocalGroupGetMembers`, loaded at run time.
        #[test]
        fn the_owner_rule_reads_this_computers_own_account_database() {
            if !elevated_or_skip("the owner rule") {
                return;
            }
            use super::super::win::{Owner, classify};
            assert_eq!(classify(SYSTEM), Owner::Trusted);
            assert_eq!(classify(ADMINISTRATORS), Owner::Trusted);
            assert_eq!(classify(&me()), Owner::Admin, "this elevated runner");
            if let Some(guest) = local_guest() {
                assert_eq!(classify(&guest), Owner::Other);
            }
            assert_eq!(classify("S-1-5-21-1-2-3-1001"), Owner::Other);
            assert_eq!(classify(USERS), Owner::Other);
            let members = super::super::win::administrators().expect("the group's members");
            assert!(
                !members.is_empty(),
                "the Administrators group names someone"
            );
            assert!(members.iter().all(|m| m.starts_with("S-1-")), "{members:?}");
        }

        /// A machine folder another account made (a person's daemon stages
        /// received files below it before the service ever ran) keeps the
        /// device waiting: the repair never takes it over. An elevated save
        /// of the machine config does, the owner and nothing else, before it
        /// writes; then the repair and the check pass, and what that account
        /// made below the folder keeps its owner.
        #[test]
        fn an_elevated_save_takes_the_machine_folder_and_nothing_below_it() {
            if !elevated_or_skip("the machine folder taken by an elevated save") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = waiting_tree(root.path());
            let other = local_guest().unwrap_or_else(|| USERS.to_string());
            let staging = tree.folder().join("staging");
            std::fs::create_dir(&staging).unwrap();
            for dir in [tree.folder(), staging.as_path()] {
                assert!(icacls(dir, &["/setowner", &format!("*{other}")]));
            }
            std::fs::write(tree.config(), "agent_token = \"t\"\n").unwrap();
            repair(&tree, true);
            assert!(
                matches!(
                    private_machine_config(&tree, Switches::ON),
                    Err(Refusal::NotPrivate(_))
                ),
                "without the save's adoption the device waits"
            );
            let before = sddl(tree.folder());
            let staging_owner = owner(&sddl(&staging));
            assert_ne!(staging_owner, "BA", "{staging_owner}");
            assert_eq!(adopt_machine_folder(&tree), Ok(true));
            let adopted = sddl(tree.folder());
            assert_eq!(owner(&adopted), "BA", "{adopted}");
            assert_eq!(aces(&adopted), aces(&before), "the owner only: {adopted}");
            assert_eq!(protected(&adopted), protected(&before), "{adopted}");
            repair(&tree, true);
            assert_eq!(check(&tree), Ok(()));
            let folder = sddl(tree.folder());
            assert_eq!(owner(&folder), "BA", "{folder}");
            assert!(sealed(&folder, true), "{folder}");
            assert_eq!(
                owner(&sddl(&staging)),
                staging_owner,
                "what that account made below keeps its owner"
            );
            assert_eq!(
                adopt_machine_folder(&tree),
                Ok(false),
                "a folder Administrators own is left as it is"
            );
        }

        /// The adoption takes a plain folder only: a machine folder that
        /// leads elsewhere is refused, naming it, and what it leads to keeps
        /// its owner and DACL.
        #[test]
        fn a_machine_folder_that_leads_elsewhere_is_never_taken_over() {
            if !elevated_or_skip("a machine folder that leads elsewhere, taken over") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let tree = waiting_tree(root.path());
            let elsewhere = root.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            let other = local_guest().unwrap_or_else(|| USERS.to_string());
            assert!(icacls(&elsewhere, &["/setowner", &format!("*{other}")]));
            std::fs::remove_dir(tree.folder()).unwrap();
            junction(tree.folder(), &elsewhere);
            let target = sddl(&elsewhere);
            let why = adopt_machine_folder(&tree).expect_err("a junction is never taken over");
            assert!(
                why.contains(&tree.folder().display().to_string()),
                "the reason names the entry: {why}"
            );
            assert_eq!(sddl(&elsewhere), target);
        }

        /// Put this process where the daemon is: SeBackup, SeRestore and
        /// SeTakeOwnership held but DISABLED, as in a service's SYSTEM token.
        /// A shell can hand its children a token with them enabled, and then
        /// an open or a rewrite passes that the daemon's would not (#3073).
        fn disable_dacl_override_privileges() {
            use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
            use windows_sys::Win32::Security::{
                AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
                TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
            };
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: this process's pseudo-handle; `token` is written on success.
            let opened = unsafe {
                OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token)
            };
            assert_ne!(
                opened,
                0,
                "OpenProcessToken: {}",
                std::io::Error::last_os_error()
            );
            for name in [
                "SeBackupPrivilege",
                "SeRestorePrivilege",
                "SeTakeOwnershipPrivilege",
            ] {
                let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
                let mut luid = LUID {
                    LowPart: 0,
                    HighPart: 0,
                };
                // SAFETY: a NUL-terminated name, and a LUID to write.
                let found =
                    unsafe { LookupPrivilegeValueW(std::ptr::null(), wide.as_ptr(), &mut luid) };
                assert_ne!(found, 0, "{name}: {}", std::io::Error::last_os_error());
                // Attributes 0: disabled, still held. A token that does not
                // hold the privilege is already where the test wants it.
                let state = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: 0,
                    }],
                };
                // SAFETY: a token opened for TOKEN_ADJUST_PRIVILEGES, one
                // entry, and no previous state asked for.
                let adjusted = unsafe {
                    AdjustTokenPrivileges(
                        token,
                        0,
                        &state,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_ne!(adjusted, 0, "{name}: {}", std::io::Error::last_os_error());
            }
            // SAFETY: opened above, closed once.
            unsafe { CloseHandle(token) };
        }
    }
}
