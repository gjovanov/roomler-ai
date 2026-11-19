// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-43 P3c — `sudo roomlerd one-row`: a person's one command that makes a
//! two-row Mac one row (`docs/fr/FR-43-macos-single-enrollment.md`, "P3c — a
//! person's one command").
//!
//! A two-row Mac has a root half (`com.roomler.daemon`, on [`ROOT_CONFIG`]) and
//! a per-user half, each with an enrollment of its own. The command takes every
//! enrollment off the people's files. One the root already holds (the same
//! server and tenant) is **retired**: its row reads offline, and a person
//! deletes it. One the root does not hold is **moved** into the root's
//! `[[orgs]]` with its token and WireGuard key unchanged, so it keeps its row.
//! The root then runs both switches (`macos_supervise_gui_worker`,
//! `macos_one_row`), and its worker, holding no enrollment, serves every org's
//! sessions (P3a, P3b).
//!
//! | Step | What it does | If it fails |
//! |---|---|---|
//! | Read | root, macOS, the root half enrolled and connected, a person at the console; every person's file under `/Users` | stops; nothing has changed |
//! | Plan | retired or moved, for each enrollment; `--dry-run` stops here | |
//! | Keep | each file it writes copied to `*.pre-one-row`, mode 600, owned as the original | puts back what it did, and stops |
//! | LaunchAgent | booted out, and its plist moved aside, as `postinstall`'s guard does | the same |
//! | Root's file | both switches on, moved enrollments appended to `[[orgs]]` | the same |
//! | People's files | identity blanked, `[[orgs]]` emptied, every other key kept | the same |
//! | Restart | `launchctl kickstart -k`, which ends live sessions | puts everything back, and restarts |
//! | Verify | within 60 s: connected, the worker attached, `screen-capture` | the same, and says what it saw |
//! | Report | each retired row, and where a person deletes it | |
//!
//! The command cannot delete a row: a device holds no user credential, and
//! `POST /api/agent/self/unenroll` refuses a permanent row on purpose. A row it
//! retires reads offline until a person deletes it, and until then `--revert`
//! puts the two rows back.
//!
//! The planner and both file transforms are pure, and every effect goes through
//! [`Host`], so the sequence and its undo run in tests on every platform. Only
//! the macOS host touches a real Mac.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tunnel_core::localapi::{DaemonInstance, NodeStatus};

use crate::config::{AgentConfig, OrgEntry, OrgOverlayMode, PRIMARY_ORG_LABEL, sanitize_org_label};

/// The root half's file, as `com.roomler.daemon.plist` passes it.
pub const ROOT_CONFIG: &str = "/etc/roomler/config.toml";
/// The extension of every copy the command keeps.
pub const KEPT_EXT: &str = "pre-one-row";
/// The per-user LaunchAgent's plist, in `~/Library/LaunchAgents`.
pub const AGENT_PLIST: &str = "com.roomler.agent.plist";
/// How long the verify waits for the restarted daemon to show one row.
pub const VERIFY_WITHIN: Duration = Duration::from_secs(60);
/// How long the daemon that leaves may take to make way for the next one.
pub const RESTART_WITHIN: Duration = Duration::from_secs(30);
/// Between two reads of the daemon's status while waiting.
const POLL: Duration = Duration::from_millis(500);

/// `path` with `.pre-one-row` after it: `config.toml` → `config.toml.pre-one-row`.
pub fn kept_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(KEPT_EXT);
    PathBuf::from(name)
}

/// FR-147 — root's own tree for the records of a conversion (D4): root:wheel
/// 700, each file 600.
pub const RECORDS: &str = "/etc/roomler/one-row";
/// FR-147 — the command's log, beside the daemon's.
pub const LOG: &str = "/var/log/roomler/one-row.log";
/// FR-147 — the format of root's records. A self-update can land between a
/// conversion and its revert, so a later release reads this one.
pub const RECORD_FORMAT: u32 = 1;
/// FR-147 — the section of the docs a refusal names where it prints no
/// command, in the package's words (`packaging/macos/postinstall:259`).
pub const INSTALL_DOC: &str =
    "docs/installation.md, section \"Which account each step of the package's script writes as\"";

/// FR-147 — a file of root's own that the command reads or writes. No
/// variant leads into a home: [`root_path`] maps each one under
/// `/etc/roomler` or `/var/log/roomler`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RootFile {
    /// Root's file, [`ROOT_CONFIG`].
    Config,
    /// Root's file as it was before the conversion, beside it.
    Kept,
    /// `root.json`: what root's kept copy is ([`RootRecord`]).
    RootRecord,
    /// One of the files root keeps for a person, under their uid.
    Person { uid: u32, part: Part },
    /// The command's own log.
    Log,
}

/// FR-147 — what root keeps of one person: the files of
/// `/etc/roomler/one-row/<uid>/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Part {
    /// The person's config, as their child read it.
    Config,
    /// The LaunchAgent plist's bytes.
    Plist,
    /// `record.json` ([`PersonRecord`]), written last: it commits the uid.
    Record,
    /// `after.json` ([`AfterRecord`]).
    After,
}

impl Part {
    pub const ALL: [Part; 4] = [Part::Config, Part::Plist, Part::Record, Part::After];

    fn name(self) -> &'static str {
        match self {
            Part::Config => "config.toml",
            Part::Plist => AGENT_PLIST,
            Part::Record => "record.json",
            Part::After => "after.json",
        }
    }
}

/// FR-147 — the one function that names a path for a [`RootFile`]: each is
/// under `/etc/roomler` or `/var/log/roomler`, root's own tree.
pub fn root_path(file: RootFile) -> PathBuf {
    match file {
        RootFile::Config => PathBuf::from(ROOT_CONFIG),
        RootFile::Kept => kept_path(Path::new(ROOT_CONFIG)),
        RootFile::RootRecord => Path::new(RECORDS).join("root.json"),
        RootFile::Person { uid, part } => person_records(uid).join(part.name()),
        RootFile::Log => PathBuf::from(LOG),
    }
}

/// FR-147 — the folder of one person's records, `/etc/roomler/one-row/<uid>`.
pub fn person_records(uid: u32) -> PathBuf {
    Path::new(RECORDS).join(uid.to_string())
}

/// `bytes`' sha256, as 64 lowercase hex digits.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// FR-147 — `root.json`: what root's kept copy is, so a revert checks it
/// before it puts it back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootRecord {
    pub v: u32,
    pub kept_sha256: String,
    pub kept_len: u64,
    /// Root's `macos_supervise_gui_worker` before the conversion: whether
    /// `--revert` bootstraps the console person's agent
    /// ([`bootstrap_after_revert`]).
    pub macos_supervise_gui_worker: bool,
    /// The release that wrote it, and when (UTC).
    pub release: String,
    pub at: String,
}

/// FR-147 — one of a person's two files as Read found it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub existed: bool,
    pub sha256: Option<String>,
    pub len: Option<u64>,
    /// Its permission bits, as the child read them.
    pub mode: Option<u32>,
}

impl FileRecord {
    /// A file of `bytes` and `mode`, or none.
    pub fn of(file: Option<(&[u8], u32)>) -> FileRecord {
        match file {
            Some((bytes, mode)) => FileRecord {
                existed: true,
                sha256: Some(sha256_hex(bytes)),
                len: Some(bytes.len() as u64),
                mode: Some(mode),
            },
            None => FileRecord {
                existed: false,
                sha256: None,
                len: None,
                mode: None,
            },
        }
    }

    /// Whether `stored` is the file this records: there exactly when it
    /// existed, with its length and sha256.
    pub fn matches(&self, stored: Option<&[u8]>) -> bool {
        match (self.existed, stored) {
            (false, None) => true,
            (true, Some(bytes)) => {
                self.len == Some(bytes.len() as u64)
                    && self.sha256.as_deref() == Some(sha256_hex(bytes).as_str())
            }
            _ => false,
        }
    }
}

/// FR-147 — `record.json`: one person's conversion. Written last of the
/// uid's files, so a uid folder without one that parses is a Keep that never
/// finished.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonRecord {
    pub v: u32,
    pub uid: u32,
    /// The account's UUID: a revert gives back only to the account the files
    /// were taken from (D7).
    pub uuid: String,
    /// The account's name then, for display.
    pub name: String,
    /// Which name the roomler folder had.
    pub segment: crate::one_row_home::Segment,
    pub config: FileRecord,
    pub plist: FileRecord,
    pub release: String,
    pub at: String,
}

/// FR-147 — `after.json`: what `convert` wrote, so a revert says when the
/// person changed it since.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfterRecord {
    pub v: u32,
    /// The sha256 `convert` reported for the config it wrote.
    pub config_sha256: Option<String>,
}

/// One account's half on this Mac, as Read found it.
#[derive(Debug, Clone)]
pub struct Person {
    /// The account's short name: its folder under `/Users`.
    pub user: String,
    /// The owner of that folder.
    pub uid: u32,
    /// The account's home.
    pub home: PathBuf,
    /// Its roomler folder ([`crate::appdirs::macos_config_dir_in_home`]).
    pub dir: PathBuf,
    /// Its config, when that folder holds one.
    pub config: Option<AgentConfig>,
    /// `~/Library/LaunchAgents/com.roomler.agent.plist` is there.
    pub has_plist: bool,
}

impl Person {
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    pub fn plist(&self) -> PathBuf {
        self.home
            .join("Library")
            .join("LaunchAgents")
            .join(AGENT_PLIST)
    }

    /// Where the plist waits while it is aside: the person's roomler folder.
    /// Never `~/Library/LaunchAgents`, which launchd reads at every login.
    pub fn plist_aside(&self) -> PathBuf {
        self.dir.join(format!("{AGENT_PLIST}.{KEPT_EXT}"))
    }
}

/// What Read found, which is all the rest needs.
#[derive(Debug, Clone)]
pub struct Read {
    pub root_path: PathBuf,
    pub root: AgentConfig,
    pub people: Vec<Person>,
    /// The root daemon's `Status`, before anything changed.
    pub status: NodeStatus,
    /// Whose session the worker serves, and the verify needs.
    pub console_uid: u32,
}

/// FR-147 — an account as root reads it in the account database. Its home is
/// display text and an argument: root opens nothing in it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Account {
    pub uid: u32,
    /// `pw_gid`: the person's child runs with this group and no other.
    pub gid: u32,
    /// `pw_name`: for display, for `dseditgroup` and an administrator's line.
    pub name: String,
    /// `pw_dir`, as text.
    pub home: String,
    /// The account's UUID (`mbr_uid_to_uuid`), which decides whose a record
    /// is (D7).
    pub uuid: String,
}

/// FR-147 — an entry of `/Users` as root's `stat` reads it: links followed,
/// nothing resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsersEntry {
    pub name: String,
    pub uid: u32,
    pub dev: u64,
    pub ino: u64,
}

/// FR-147 — whom Read starts a child for, and whom it leaves alone.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    /// One account per distinct owner uid, in uid order.
    pub accounts: Vec<Account>,
    /// `(uid, line)`: an owner left alone, with why.
    pub left_alone: Vec<(u32, String)>,
}

fn entry_names(entries: &[UsersEntry], uid: u32) -> String {
    let names: Vec<String> = entries
        .iter()
        .filter(|e| e.uid == uid)
        .map(|e| format!("/Users/{}", e.name))
        .collect();
    names.join(" and ")
}

/// FR-147 — the owners of `/Users`' entries that Read starts a `read` child
/// for: one per distinct uid, in uid order. Left alone, and said: uid 0, a
/// uid under 500, a uid with no account (D3: no process of a person's can be
/// started for it), and an account whose primary group is 0. Pure over
/// root's reads.
pub fn accounts_to_ask(entries: &[UsersEntry], account: impl Fn(u32) -> Option<Account>) -> Asked {
    let mut uids: Vec<u32> = entries.iter().map(|e| e.uid).collect();
    uids.sort_unstable();
    uids.dedup();
    let mut asked = Asked::default();
    for uid in uids {
        let why = if uid == 0 {
            "it is root's".to_string()
        } else if uid < 500 {
            format!("uid {uid} is under 500, a system account's")
        } else {
            match account(uid) {
                None => format!(
                    "no account has uid {uid}, so no process of a person's can be started for it"
                ),
                Some(a) if a.gid == 0 => {
                    format!("{}'s primary group is 0 (wheel)", a.name)
                }
                Some(a) => {
                    asked.accounts.push(a);
                    continue;
                }
            }
        };
        asked.left_alone.push((
            uid,
            format!("left alone: {}: {why}", entry_names(entries, uid)),
        ));
    }
    asked
}

/// FR-147 — whether any entry owned by `account` leads to the home its child
/// reported (the device and inode root's `stat` read), and a line for each
/// that does not, which is left alone. A link to a home on another volume
/// leads there; an entry another account's folder sits behind does not.
pub fn entries_of_home(
    entries: &[UsersEntry],
    account: &Account,
    home: &crate::one_row_home::HomeEntry,
) -> (bool, Vec<(u32, String)>) {
    let id = home.dev.zip(home.ino);
    let mut leads = false;
    let mut alone = Vec::new();
    for e in entries.iter().filter(|e| e.uid == account.uid) {
        if id == Some((e.dev, e.ino)) {
            leads = true;
        } else {
            alone.push((
                account.uid,
                format!(
                    "left alone: /Users/{}: owned by {} (uid {}), but it does not lead to that \
                     account's home, {}",
                    e.name, account.name, account.uid, account.home
                ),
            ));
        }
    }
    (leads, alone)
}

/// What happens to one of a person's enrollments.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Fate {
    /// The root half holds this server and tenant: the row goes, by a person's
    /// delete.
    Retired,
    /// Nobody else on this Mac holds it: it moves into the root's `[[orgs]]`
    /// as `label`, and keeps its row.
    Moved { label: String },
}

/// One enrollment in a person's file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Item {
    pub user: String,
    /// `None` for the file's primary, else its `[[orgs]]` label.
    pub org: Option<String>,
    /// The name the row was enrolled with.
    pub name: String,
    pub server_url: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub fate: Fate,
}

/// The migration, before anything changes.
#[derive(Debug, Clone)]
pub struct Plan {
    pub items: Vec<Item>,
    /// The root's file after it.
    pub root_after: AgentConfig,
    /// Each person whose file changes, by index into [`Read::people`], with
    /// the file it becomes.
    pub people_after: Vec<(usize, AgentConfig)>,
    /// What a person should know before saying yes.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn moved_labels(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|i| match &i.fate {
                Fate::Moved { label } => Some(label.clone()),
                Fate::Retired => None,
            })
            .collect()
    }

    pub fn retired(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.fate == Fate::Retired)
    }
}

/// Which of the root's enrollments holds a server and tenant, and what a
/// retirement there leaves behind.
struct Held {
    label: String,
    runs_overlay: bool,
    enabled: bool,
}

fn held_by(root: &AgentConfig, server_url: &str, tenant_id: &str) -> Option<Held> {
    if root.is_primary_identity(server_url, tenant_id) {
        return Some(Held {
            label: PRIMARY_ORG_LABEL.to_string(),
            runs_overlay: root.overlay_enabled,
            enabled: true,
        });
    }
    root.orgs
        .iter()
        .find(|o| o.server_url == server_url && o.tenant_id == tenant_id)
        .map(|o| Held {
            label: o.label.clone(),
            runs_overlay: o.overlay_mode != OrgOverlayMode::Off,
            enabled: o.enabled,
        })
}

/// `want` sanitized, or with `-2`, `-3`… when the root already has it. Always
/// a label [`AgentConfig::partition_runnable_orgs`] accepts.
fn free_label(root: &AgentConfig, want: &str) -> String {
    let base = sanitize_org_label(want).unwrap_or_else(|| "org".to_string());
    if root.find_org(&base).is_none() {
        return base;
    }
    // Room for the suffix inside the 32 characters a label may have.
    let stem: String = base.chars().take(28).collect();
    let stem = stem.trim_end_matches('-');
    (2u32..)
        .map(|n| format!("{stem}-{n}"))
        .find(|label| root.find_org(label).is_none())
        .expect("finitely many labels are taken")
}

/// The host part of a server URL, which names a moved primary as `enroll`
/// names a new secondary.
fn host_of(server_url: &str) -> &str {
    server_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(server_url)
}

/// A person's primary enrollment as an entry of the root's `[[orgs]]`: the same
/// token, agent id and WireGuard key, so the row and any address it had stay
/// its own. Its overlay starts off. The root's owner decides a root org's
/// overlay (`roomlerd org overlay`), and a migration that quietly brought up a
/// TUN or a listener would change the root's network under it.
fn moved_primary(person: &AgentConfig, label: &str) -> OrgEntry {
    OrgEntry {
        label: label.to_string(),
        server_url: person.server_url.clone(),
        ws_url: person.ws_url.clone(),
        agent_token: person.agent_token.clone(),
        agent_id: person.agent_id.clone(),
        tenant_id: person.tenant_id.clone(),
        enabled: true,
        overlay_mode: OrgOverlayMode::Off,
        overlay_wg_secret_key: person.overlay_wg_secret_key.clone(),
        overlay_wg_key_epoch: person.overlay_wg_key_epoch,
        overlay_advertised_routes: person.overlay_advertised_routes.clone(),
        overlay_exit_node_enabled: false,
        advertise_routes: person.advertise_routes.clone(),
        netstack_socks_port: None,
    }
}

/// A person's `[[orgs]]` entry as one of the root's, under `label`, its overlay
/// off for the reason [`moved_primary`] gives.
fn moved_org(entry: &OrgEntry, label: &str) -> OrgEntry {
    OrgEntry {
        label: label.to_string(),
        overlay_mode: OrgOverlayMode::Off,
        overlay_exit_node_enabled: false,
        netstack_socks_port: None,
        ..entry.clone()
    }
}

/// A person's file with no enrollment left in it: the identity fields blank,
/// `[[orgs]]` empty, and every other key kept, since those are the person's
/// session knobs the worker reads (P3a, P3b). The WireGuard key goes with the
/// identity: a moved enrollment's key now lives in the root's file, and a key
/// two files hold would make a later re-enrollment of this half a second node
/// with it (`wg_key_taken_by_other`).
pub fn without_enrollments(person: &AgentConfig) -> AgentConfig {
    let mut c = person.clone();
    c.server_url = String::new();
    c.ws_url = None;
    c.agent_token = String::new();
    c.agent_id = String::new();
    c.tenant_id = String::new();
    c.ephemeral = false;
    c.orgs = Vec::new();
    c.overlay_wg_secret_key = None;
    c.overlay_wg_key_epoch = 0;
    c
}

/// The Plan step. Pure: it reads the files Read parsed and writes nothing.
pub fn plan(root: &AgentConfig, people: &[Person]) -> Result<Plan> {
    if !root.has_primary_enrollment() {
        bail!(
            "the root half holds no enrollment ({ROOT_CONFIG}): enroll it first, then run this \
             again"
        );
    }
    let mut root_after = root.clone();
    root_after.macos_supervise_gui_worker = true;
    root_after.macos_one_row = true;
    let mut items = Vec::new();
    let mut notes = Vec::new();
    let mut people_after = Vec::new();
    for (index, person) in people.iter().enumerate() {
        let Some(cfg) = &person.config else {
            continue;
        };
        let mut found = 0;
        if cfg.has_primary_enrollment() {
            found += 1;
            let fate = match held_by(&root_after, &cfg.server_url, &cfg.tenant_id) {
                Some(held) => {
                    retirement_notes(&mut notes, &person.user, cfg.overlay_enabled, &held);
                    Fate::Retired
                }
                None => {
                    if cfg.ephemeral {
                        bail!(
                            "{}'s enrollment ({} tenant {}) is ephemeral, and the root half does \
                             not hold that org: an ephemeral row lives only with the half that \
                             enrolled it. Unenroll it first, or enroll the root half there",
                            person.user,
                            cfg.server_url,
                            cfg.tenant_id
                        );
                    }
                    let label = free_label(&root_after, host_of(&cfg.server_url));
                    if cfg.overlay_enabled {
                        notes.push(format!(
                            "{}'s enrollment moves as `{label}` with its overlay off: the root's \
                             file decides an org's overlay (`roomlerd org overlay`)",
                            person.user
                        ));
                    }
                    root_after.orgs.push(moved_primary(cfg, &label));
                    Fate::Moved { label }
                }
            };
            items.push(Item {
                user: person.user.clone(),
                org: None,
                name: cfg.machine_name.clone(),
                server_url: cfg.server_url.clone(),
                tenant_id: cfg.tenant_id.clone(),
                agent_id: cfg.agent_id.clone(),
                fate,
            });
        }
        for entry in &cfg.orgs {
            found += 1;
            let overlay_on = entry.overlay_mode != OrgOverlayMode::Off;
            let fate = match held_by(&root_after, &entry.server_url, &entry.tenant_id) {
                Some(held) => {
                    retirement_notes(&mut notes, &person.user, overlay_on, &held);
                    Fate::Retired
                }
                None => {
                    let label = free_label(&root_after, &entry.label);
                    if overlay_on {
                        notes.push(format!(
                            "{}'s org `{}` moves as `{label}` with its overlay off: the root's \
                             file decides an org's overlay (`roomlerd org overlay`)",
                            person.user, entry.label
                        ));
                    }
                    root_after.orgs.push(moved_org(entry, &label));
                    Fate::Moved { label }
                }
            };
            items.push(Item {
                user: person.user.clone(),
                org: Some(entry.label.clone()),
                name: cfg.machine_name.clone(),
                server_url: entry.server_url.clone(),
                tenant_id: entry.tenant_id.clone(),
                agent_id: entry.agent_id.clone(),
                fate,
            });
        }
        if found == 0 {
            continue;
        }
        if !cfg.tunnel_routes.is_empty() {
            notes.push(format!(
                "{} declared tunnel route(s) in {}'s file stop running: the worker runs none, and \
                 the root's file declares this Mac's routes (`roomler route add`)",
                cfg.tunnel_routes.len(),
                person.user
            ));
        }
        people_after.push((index, without_enrollments(cfg)));
    }
    Ok(Plan {
        items,
        root_after,
        people_after,
        notes,
    })
}

fn retirement_notes(notes: &mut Vec<String>, user: &str, overlay_on: bool, held: &Held) {
    if overlay_on && !held.runs_overlay {
        notes.push(format!(
            "{user}'s row there ran the overlay and the root's `{}` runs none, so that row may \
             hold this Mac's only overlay address in that org. A deleted row's address is never \
             issued again; turn the root's overlay on there first if the address matters",
            held.label
        ));
    }
    if !held.enabled {
        notes.push(format!(
            "the root holds {user}'s org there as `{}`, disabled: `roomlerd org enable {}` \
             serves its sessions",
            held.label, held.label
        ));
    }
}

/// The plan, in the words the command says before it asks.
pub fn describe(read: &Read, plan: &Plan) -> Vec<String> {
    let mut out = vec!["one row: the plan. Nothing has changed yet.".to_string()];
    out.push(format!(
        "the root half: \"{}\", agent {}, tenant {}, {}{}",
        read.root.machine_name,
        read.root.agent_id,
        read.root.tenant_id,
        read.root.server_url,
        if read.status.connected {
            ", connected"
        } else {
            ", NOT connected"
        }
    ));
    if let Some(ip) = &read.status.overlay_ip {
        out.push(format!(
            "its overlay address: {ip}. Its enrollment is untouched, so it keeps it"
        ));
    }
    let console = read
        .people
        .iter()
        .find(|p| p.uid == read.console_uid)
        .map_or_else(|| format!("uid {}", read.console_uid), |p| p.user.clone());
    out.push(format!("at the console: {console}"));
    if plan.items.is_empty() {
        out.push("no person's file on this Mac holds an enrollment".to_string());
    }
    for item in &plan.items {
        let which = item
            .org
            .as_deref()
            .map_or_else(|| "primary".to_string(), |l| format!("org `{l}`"));
        out.push(format!(
            "{}, {which}: \"{}\", agent {}, tenant {}, {}",
            item.user, item.name, item.agent_id, item.tenant_id, item.server_url
        ));
        out.push(match &item.fate {
            Fate::Retired => "    retired: the root half holds this org. Its row reads offline \
                              until a person deletes it"
                .to_string(),
            Fate::Moved { label } => format!(
                "    moved: into the root's [[orgs]] as `{label}`, its token and WireGuard key \
                 unchanged. It keeps its row"
            ),
        });
    }
    for person in read.people.iter().filter(|p| p.has_plist) {
        out.push(format!(
            "the LaunchAgent: {}'s plist moves aside, and its job is booted out if loaded",
            person.user
        ));
    }
    out.push(format!(
        "the root's file: macos_supervise_gui_worker = true (now {}), macos_one_row = true (now {})",
        read.root.macos_supervise_gui_worker, read.root.macos_one_row
    ));
    for (index, _) in &plan.people_after {
        out.push(format!(
            "{}'s file: no enrollment left in it, every other setting kept",
            read.people[*index].user
        ));
    }
    out.extend(plan.notes.iter().map(|n| format!("note: {n}")));
    out.push(format!(
        "copies first: every file it writes, as *.{KEPT_EXT}, and `--revert` puts them back"
    ));
    out.push(
        "⚠️ the daemon restarts: every live remote-desktop, SSH and tunnel session on this Mac \
         ends"
            .to_string(),
    );
    out
}

/// Copies an earlier migration left, which this one would write over. A
/// refusal, before anything changes: those copies are the only way back from
/// that migration.
pub fn leftover_copies(host: &impl Host, read: &Read, plan: &Plan) -> Vec<PathBuf> {
    let mut paths = vec![kept_path(&read.root_path)];
    paths.extend(
        plan.people_after
            .iter()
            .map(|(i, _)| kept_path(&read.people[*i].config_path())),
    );
    paths.extend(
        read.people
            .iter()
            .filter(|p| p.has_plist)
            .map(Person::plist_aside),
    );
    paths.into_iter().filter(|p| host.exists(p)).collect()
}

/// What the restarted daemon still lacks for one row; `None` when it has it all.
pub fn missing(status: &NodeStatus, moved: &[String]) -> Option<String> {
    if !status.connected {
        return Some("the root half is not connected to its server".to_string());
    }
    for label in moved {
        match status.orgs.iter().find(|o| &o.label == label) {
            Some(org) if org.connected => {}
            Some(org) => {
                return Some(format!(
                    "the moved org `{label}` is not connected{}",
                    org.terminal_error
                        .as_deref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ));
            }
            None => return Some(format!("the moved org `{label}` is not running")),
        }
    }
    let Some(worker) = &status.gui_worker else {
        return Some("the daemon does not report its GUI worker".to_string());
    };
    if !worker.one_row {
        return Some("the daemon does not run one row (macos_one_row)".to_string());
    }
    if !worker.attached {
        return Some("no GUI worker is attached".to_string());
    }
    match &worker.permissions {
        None => Some("the GUI worker has not announced its permissions".to_string()),
        Some(words) if !words.iter().any(|w| w == "screen-capture") => Some(format!(
            "the GUI worker holds no Screen Recording grant (it reports: {})",
            if words.is_empty() {
                "nothing".to_string()
            } else {
                words.join(", ")
            }
        )),
        Some(_) => None,
    }
}

/// FR-147 — the roomler folder's path as the person's own agent names it:
/// from `pw_dir` and the segment the child reported, never from the disk
/// ([`crate::appdirs::macos_config_dir_in_home`] names the segment). Display
/// text for the lines root prints, joined with `/` on every platform.
pub fn folder_display(home: &str, segment: crate::one_row_home::Segment) -> String {
    let at = Path::new(home);
    let current = crate::appdirs::macos_config_dir_in_home(at, |_| false);
    let folder = crate::appdirs::macos_config_dir_in_home(at, |p| {
        segment == crate::one_row_home::Segment::Legacy && p != current
    });
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{home}/Library/Application Support/{name}")
}

/// FR-147 — what root knows of a person beyond their child's report, for the
/// administrator's lines: whether they are the person at the console, and
/// whether `dseditgroup` says that account is an administrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights {
    pub at_console: bool,
    pub administrator: bool,
}

/// FR-147 — what R1 and R3 are about: `~/Library/LaunchAgents`, or the
/// config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    AgentsDir,
    Config,
}

fn kind_words(kind: crate::one_row_home::Kind) -> &'static str {
    use crate::one_row_home::Kind;
    match kind {
        Kind::Dir => "a folder",
        Kind::File => "a file",
        Kind::Link => "a link",
        Kind::Other => "neither a file nor a folder",
        Kind::Absent => "not there",
    }
}

/// The path, kind, owner and mode the child reported for `target`.
fn target_of(
    account: &Account,
    seen: &crate::one_row_home::ReadAnswer,
    target: Target,
) -> (String, crate::one_row_home::Kind, Option<u32>, Option<u32>) {
    use crate::one_row_home::{Held, Kind, ReadError};
    match target {
        Target::AgentsDir => (
            format!("{}/Library/LaunchAgents", account.home),
            seen.agents_dir.kind,
            seen.agents_dir.uid,
            seen.agents_dir.mode,
        ),
        Target::Config => {
            let path = format!(
                "{}/config.toml",
                folder_display(&account.home, seen.segment)
            );
            match &seen.config {
                // `eacces` is read only of what `lstat` said is a regular file.
                Held::Unreadable {
                    error: ReadError::Eacces,
                    uid,
                    mode,
                } => (path, Kind::File, *uid, *mode),
                Held::Present { uid, mode, .. } => (path, Kind::File, Some(*uid), Some(*mode)),
                Held::Unreadable { uid, mode, .. } => (path, Kind::Other, *uid, *mode),
                Held::Absent => (path, Kind::Absent, None, None),
            }
        }
    }
}

/// FR-147 — R1 and R3 (D2): what the child reported of the folder or file in
/// the way, then `sudo chown -h '<name>' '<path>'` for an administrator where
/// all four conditions hold, else the words with no command. The conditions
/// (the package's `sudo_line_fits` and its owner test,
/// `packaging/macos/postinstall:260-266`, `:301`): the home is the console
/// person's; that person is an administrator; the child reports the home an
/// absolute path to a folder and `~/Library` a folder of the person's; the
/// target is a folder (R1) or a regular file (R3) of uid 0. And the line
/// passes #3846's printing tests: the path survives `sh -c "…"` and displays
/// as it runs, and the name is plain. `owner_name` is the owner's account
/// name, as root reads it.
pub fn chown_words(
    account: &Account,
    seen: &crate::one_row_home::ReadAnswer,
    target: Target,
    rights: Rights,
    owner_name: &str,
) -> String {
    use crate::macos_supervisor::{
        path_displays_as_it_runs, path_survives_double_quotes, plain_account_name, sh_quote,
    };
    use crate::one_row_home::Kind;
    let (path, kind, uid, mode) = target_of(account, seen, target);
    let name = &account.name;
    let first = match (uid, mode) {
        (Some(u), Some(m)) if u == account.uid => format!(
            "{path} ({}) is {name}'s own (uid {u}, mode {m:03o}), but {name}'s account cannot \
             change it.",
            kind_words(kind)
        ),
        (Some(u), Some(m)) => format!(
            "{path} ({}) belongs to {owner_name} (uid {u}, mode {m:03o}), not to {name}, so \
             {name}'s account cannot change it.",
            kind_words(kind)
        ),
        _ => format!("{path} could not be looked at by {name}'s account."),
    };
    let home_reads = seen.home.absolute
        && seen.home.kind == Kind::Dir
        && seen.library.kind == Kind::Dir
        && seen.library.uid == Some(account.uid);
    let roots = uid == Some(0)
        && kind
            == match target {
                Target::AgentsDir => Kind::Dir,
                Target::Config => Kind::File,
            };
    let printable = path_survives_double_quotes(&path)
        && path_displays_as_it_runs(&path)
        && plain_account_name(name);
    if rights.at_console && rights.administrator && home_reads && roots && printable {
        format!(
            "{first}\n  {name} is an administrator of this Mac and can run this, then run `sudo \
             roomlerd one-row` again:\n    sudo chown -h {} {}",
            sh_quote(name),
            sh_quote(&path)
        )
    } else {
        format!(
            "{first}\n  It has to be {name}'s own before `sudo roomlerd one-row` can make this \
             Mac one row;\n  see {INSTALL_DOC}."
        )
    }
}

/// `lstat`'s answer (owner and full mode) for what the child reported: a kind
/// and permission bits. Absent is "not there"; no owner or mode is a read
/// that failed.
fn reported(
    kind: crate::one_row_home::Kind,
    uid: Option<u32>,
    mode: Option<u32>,
) -> std::io::Result<(u32, u32)> {
    use crate::one_row_home::Kind;
    let ty = match kind {
        Kind::Absent => return Err(std::io::ErrorKind::NotFound.into()),
        Kind::Dir => 0o040000,
        Kind::File => 0o100000,
        Kind::Link => 0o120000,
        Kind::Other => 0o010000,
    };
    match (uid, mode) {
        (Some(uid), Some(mode)) => Ok((uid, ty | (mode & 0o7777))),
        _ => Err(std::io::Error::other("it could not be looked at")),
    }
}

/// FR-147 — `lstat` as the person's child reported it: the home, `~/Library`,
/// `~/Library/Application Support`, the roomler folder and the names in it
/// that the report covers. Any other path is an error, so the supervisor's
/// readers ([`crate::macos_supervisor::read_person_folder`],
/// [`crate::macos_supervisor::path_reads_as_the_folder`]) read nothing of a
/// home themselves.
fn reported_lstat<'a>(
    account: &Account,
    seen: &'a crate::one_row_home::ReadAnswer,
) -> impl Fn(&Path) -> std::io::Result<(u32, u32)> + 'a {
    use crate::one_row_home::{Held, Kind, ReadError};
    let home = PathBuf::from(&account.home);
    let library = PathBuf::from(format!("{}/Library", account.home));
    let support = PathBuf::from(format!("{}/Library/Application Support", account.home));
    let folder_text = folder_display(&account.home, seen.segment);
    let folder = PathBuf::from(&folder_text);
    let config = PathBuf::from(format!("{folder_text}/config.toml"));
    let legacy_config = kept_path(&config);
    let legacy_plist = PathBuf::from(format!("{folder_text}/{AGENT_PLIST}.{KEPT_EXT}"));
    move |p: &Path| {
        let entry = |e: &crate::one_row_home::Entry| reported(e.kind, e.uid, e.mode);
        if p == home {
            reported(seen.home.kind, seen.home.uid, seen.home.mode)
        } else if p == library {
            entry(&seen.library)
        } else if p == support {
            entry(&seen.support)
        } else if p == folder {
            entry(&seen.folder)
        } else if p == legacy_config {
            entry(&seen.legacy.config)
        } else if p == legacy_plist {
            entry(&seen.legacy.plist)
        } else if p == config {
            match &seen.config {
                Held::Absent => Err(std::io::ErrorKind::NotFound.into()),
                Held::Present { uid, mode, .. } => reported(Kind::File, Some(*uid), Some(*mode)),
                Held::Unreadable {
                    error: ReadError::Eacces,
                    uid,
                    mode,
                } => reported(Kind::File, *uid, *mode),
                Held::Unreadable { uid, mode, .. } => reported(Kind::Other, *uid, *mode),
            }
        } else {
            Err(std::io::Error::other("not in the child's report"))
        }
    }
}

/// FR-147 — R2 and R4: the person's roomler folder is another account's.
/// What the child reported of it, then #3846's own text
/// ([`crate::macos_supervisor::move_aside`]), built from that report alone:
/// an administrator's command where the package's rule fits (the console
/// person, an administrator, a home whose paths read as the folder), else its
/// words. A folder that holds a config never gets one, so R2 never does.
/// `stamp` names what the command would make, as the supervisor's does.
pub fn folder_words(
    account: &Account,
    seen: &crate::one_row_home::ReadAnswer,
    rights: Rights,
    owner_name: &str,
    stamp: &str,
) -> String {
    use crate::macos_supervisor::{
        NoCommand, move_aside, path_reads_as_the_folder, read_person_folder,
    };
    let folder = folder_display(&account.home, seen.segment);
    let first = match (seen.folder.uid, seen.folder.mode) {
        (Some(u), Some(m)) => format!(
            "{folder} ({}) belongs to {owner_name} (uid {u}, mode {m:03o}), not to {}.",
            kind_words(seen.folder.kind),
            account.name
        ),
        _ => format!(
            "{folder} could not be looked at by {}'s account.",
            account.name
        ),
    };
    let lstat = reported_lstat(account, seen);
    let Some(mut f) = read_person_folder(PathBuf::from(&folder), account.uid, stamp, &lstat) else {
        return format!("{first} No command fits it; see {INSTALL_DOC}.");
    };
    f.administrator = if !rights.at_console {
        Err(NoCommand::NotRead)
    } else if !path_reads_as_the_folder(
        account.uid,
        Path::new(&account.home),
        Path::new(&folder),
        &lstat,
    ) {
        Err(NoCommand::PathsDoNotRead)
    } else if !rights.administrator {
        Err(NoCommand::NotAdministrator(None))
    } else {
        Ok(account.name.clone())
    };
    format!("{first} {}", move_aside(&f))
}

/// FR-147 — R5: the console person's LaunchAgent is loaded, and this run
/// does not convert them. The restarted daemon's worker would stand down for
/// it, so the verify would fail. `why`: why they were left alone.
pub fn r5_words(console_uid: u32, why: Option<&str>) -> String {
    format!(
        "the person at the console (uid {console_uid}) has com.roomler.agent loaded, and this run \
         does not convert them{}: with that LaunchAgent loaded the restarted daemon's worker stands \
         down, and the verify would fail. No command fits this; see {INSTALL_DOC}.",
        why.map(|w| format!(" ({w})")).unwrap_or_default()
    )
}

/// FR-147 D6 — what `launchctl` is given to load a person's LaunchAgent:
/// today's root form, `bootstrap gui/<uid> <P>`, until lab cell C13 shows
/// that the person's own `launchctl` loads it. The one place `<P>` is built:
/// launchd opens it, never a process of ours running as root.
pub fn bootstrap_argv(uid: u32, home: &str) -> Vec<String> {
    vec![
        "bootstrap".to_string(),
        format!("gui/{uid}"),
        format!("{home}/Library/LaunchAgents/{AGENT_PLIST}"),
    ]
}

/// FR-147 D1 — the package's binary (`scripts/install.sh:134`): the program
/// root starts as a person, whatever path the command was typed by.
pub const PERSON_PROGRAM: &str = "/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd";
/// FR-147 — how long a person's child may take.
pub const CHILD_DEADLINE: Duration = Duration::from_secs(30);
/// FR-147 — the most stdout root reads of a child, and the most stdin it
/// sends one.
pub const CHILD_CAP: usize = 1024 * 1024;

/// FR-147 D1 — how root starts a person's child: the package's binary, the
/// child's arguments, no environment, `/` as its folder, and the account's
/// uid and primary group, set by root's own hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub program: &'static str,
    pub args: Vec<String>,
    /// Empty: the child reads no environment.
    pub env: Vec<(String, String)>,
    pub dir: &'static str,
    pub uid: u32,
    pub gid: u32,
}

/// FR-147 D1 — the [`SpawnPlan`] for `verb` as `account`.
pub fn spawn_plan(account: &Account, verb: crate::one_row_home::Verb) -> SpawnPlan {
    SpawnPlan {
        program: PERSON_PROGRAM,
        args: crate::one_row_home::child_args(account.uid, account.gid, verb),
        env: Vec::new(),
        dir: "/",
        uid: account.uid,
        gid: account.gid,
    }
}

/// FR-147 D1 — why root does not start [`PERSON_PROGRAM`], as `lstat` read it
/// (owner and full mode): a link, not a regular file, not uid 0's, or
/// writable by group or others. `None`: it may be started.
pub fn program_refusal(lstat: std::io::Result<(u32, u32)>) -> Option<String> {
    let (uid, mode) = match lstat {
        Ok(stat) => stat,
        Err(e) => return Some(format!("{PERSON_PROGRAM} could not be read: {e}")),
    };
    let why = match mode & 0o170000 {
        0o120000 => "is a link".to_string(),
        0o100000 if uid != 0 => format!("is uid {uid}'s, not root's"),
        0o100000 if mode & 0o022 != 0 => "is writable by group or others".to_string(),
        0o100000 => return None,
        _ => "is not a regular file".to_string(),
    };
    Some(format!(
        "{PERSON_PROGRAM} {why} (mode {:o}), so no process of a person's is started from it",
        mode & 0o7777
    ))
}

/// Every effect the command has, so the sequence runs against a fake in tests.
#[async_trait]
pub trait Host: Send {
    /// One line of the command's account: printed, and kept in its log.
    fn say(&mut self, line: &str);
    fn exists(&self, path: &Path) -> bool;
    /// Copy `live` to `kept`, mode 600 and owned as `live`. Never over a file.
    fn keep(&mut self, live: &Path, kept: &Path) -> Result<()>;
    /// Write `cfg` over `path`, which stays owned as it was.
    fn write_config(&mut self, path: &Path, cfg: &AgentConfig) -> Result<()>;
    /// Move a file, which keeps its owner and mode.
    fn rename(&mut self, from: &Path, to: &Path) -> Result<()>;
    /// Is `gui/<uid>/com.roomler.agent` loaded?
    fn agent_loaded(&mut self, uid: u32) -> bool;
    /// Boot it out, and return once launchd no longer has it.
    fn bootout_agent(&mut self, uid: u32) -> Result<()>;
    fn bootstrap_agent(&mut self, uid: u32, plist: &Path) -> Result<()>;
    /// The root daemon's answer to `Status`.
    async fn status(&mut self) -> Result<NodeStatus>;
    /// `launchctl kickstart -k system/com.roomler.daemon`.
    fn kickstart(&mut self) -> Result<()>;
    async fn sleep(&mut self, d: Duration);
    /// Time since an arbitrary start: the clock every wait is measured on, a
    /// status read that hangs included.
    fn now(&self) -> Duration;
}

/// What the apply has done so far, which is exactly what its undo puts back.
#[derive(Debug, Default)]
struct Done {
    /// `(live, kept)`: a file copied aside. The copy goes back over it.
    kept: Vec<(PathBuf, PathBuf)>,
    /// `(plist, aside)`: a plist moved aside.
    moved: Vec<(PathBuf, PathBuf)>,
    /// `(uid, plist)`: a LaunchAgent booted out.
    booted_out: Vec<(u32, PathBuf)>,
    /// The daemon was asked to restart onto the new files.
    restarted: bool,
}

/// Everything after Read: the plan, said, and its refusals; then, unless only
/// the plan was asked for (`--dry-run`), `confirm` and the apply. `confirm`
/// sees the plan it confirms ([`plan_digest`]).
pub async fn migrate<H: Host>(
    host: &mut H,
    read: &Read,
    dry_run: bool,
    confirm: impl FnOnce(&Plan) -> Result<()>,
) -> Result<()> {
    let plan = plan(&read.root, &read.people)?;
    for line in describe(read, &plan) {
        host.say(&line);
    }
    let leftovers = leftover_copies(&*host, read, &plan);
    if !leftovers.is_empty() {
        let list: Vec<String> = leftovers.iter().map(|p| p.display().to_string()).collect();
        bail!(
            "copies from an earlier migration are still here: {}. `sudo roomlerd one-row \
             --revert` puts them back; remove them to migrate again. Nothing has changed",
            list.join(", ")
        );
    }
    let agents_loaded = read.people.iter().any(|p| host.agent_loaded(p.uid));
    if plan.items.is_empty()
        && !agents_loaded
        && !read.people.iter().any(|p| p.has_plist)
        && read.root.macos_supervise_gui_worker
        && read.root.macos_one_row
    {
        host.say("this Mac is one row already: nothing to do");
        return Ok(());
    }
    if dry_run {
        host.say("--dry-run: nothing has changed");
        return Ok(());
    }
    confirm(&plan)?;
    apply(host, read, &plan).await
}

/// What a person said yes to, as one number. On a Mac the command applies a
/// plan in a detached copy of itself, which goes ahead only if its own plan
/// has the digest of the one that was said. It hashes what the plan does, not
/// what Read saw beside it, so a status that moved in between is not another
/// plan. `DefaultHasher::new` has fixed keys, so one binary agrees with itself.
pub fn plan_digest(plan: &Plan) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    plan.items.hash(&mut h);
    for org in &plan.root_after.orgs {
        (&org.label, &org.server_url, &org.tenant_id, &org.agent_id).hash(&mut h);
    }
    (
        plan.root_after.macos_supervise_gui_worker,
        plan.root_after.macos_one_row,
    )
        .hash(&mut h);
    for (index, _) in &plan.people_after {
        index.hash(&mut h);
    }
    h.finish()
}

/// [`plan_digest`]'s twin for `--revert`.
pub fn revert_digest(r: &Revert) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    r.hash(&mut h);
    h.finish()
}

/// From Keep to Report. Any step that fails puts back everything done before
/// it, so the Mac is as it was.
pub async fn apply<H: Host>(host: &mut H, read: &Read, plan: &Plan) -> Result<()> {
    let mut done = Done::default();
    match steps(host, read, plan, &mut done).await {
        Ok(after) => {
            report(host, read, plan, &after);
            Ok(())
        }
        Err(e) => {
            host.say(&format!("stopped: {e:#}"));
            host.say("putting back everything this run changed");
            let problems = undo(host, &done).await;
            if problems.is_empty() {
                host.say("the Mac is as it was");
                Err(e.context("one row did not take, and the Mac is as it was"))
            } else {
                for p in &problems {
                    host.say(&format!("could not put back: {p}"));
                }
                Err(e.context(format!(
                    "one row did not take, and {} thing(s) could not be put back (above)",
                    problems.len()
                )))
            }
        }
    }
}

async fn steps<H: Host>(
    host: &mut H,
    read: &Read,
    plan: &Plan,
    done: &mut Done,
) -> Result<NodeStatus> {
    // Keep.
    let mut writes: Vec<(PathBuf, &AgentConfig)> = vec![(read.root_path.clone(), &plan.root_after)];
    for (index, cfg) in &plan.people_after {
        writes.push((read.people[*index].config_path(), cfg));
    }
    for (live, _) in &writes {
        let kept = kept_path(live);
        host.keep(live, &kept)
            .with_context(|| format!("keeping a copy of {}", live.display()))?;
        host.say(&format!("kept: {}", kept.display()));
        done.kept.push((live.clone(), kept));
    }
    // LaunchAgent, as `postinstall`'s guard: the job out, then the plist away,
    // so the next login does not bring it back. A loaded LaunchAgent makes the
    // supervisor stand down (`macos_supervisor::decide`).
    for person in &read.people {
        if host.agent_loaded(person.uid) {
            host.bootout_agent(person.uid)
                .with_context(|| format!("booting out {}'s LaunchAgent", person.user))?;
            host.say(&format!("booted out: gui/{}/com.roomler.agent", person.uid));
            done.booted_out.push((person.uid, person.plist()));
        }
        if person.has_plist {
            let (plist, aside) = (person.plist(), person.plist_aside());
            host.rename(&plist, &aside)
                .with_context(|| format!("moving {} aside", plist.display()))?;
            host.say(&format!("moved aside: {}", plist.display()));
            done.moved.push((plist, aside));
        }
    }
    // The root's file, then the people's.
    for (path, cfg) in &writes {
        host.write_config(path, cfg)
            .with_context(|| format!("writing {}", path.display()))?;
        host.say(&format!("written: {}", path.display()));
    }
    // Restart. Marked before the ask: whatever `kickstart` answers, the undo
    // restarts the daemon onto the files it puts back.
    host.say("restarting the daemon: live sessions end now");
    done.restarted = true;
    let leaving = host.status().await.ok().map(|s| DaemonInstance::of(&s));
    host.kickstart().context("restarting the daemon")?;
    wait_for_new_daemon(host, leaving, RESTART_WITHIN).await?;
    // Verify.
    verify(host, &plan.moved_labels(), VERIFY_WITHIN).await
}

/// Puts back what [`steps`] did: the files first, then the LaunchAgents, then a
/// restart onto them. Each failure is said and the rest still run, so one stuck
/// file leaves as little as possible behind.
async fn undo<H: Host>(host: &mut H, done: &Done) -> Vec<String> {
    let mut problems = Vec::new();
    for (live, kept) in done.kept.iter().rev() {
        match host.rename(kept, live) {
            Ok(()) => host.say(&format!("put back: {}", live.display())),
            Err(e) => problems.push(format!(
                "{} (its copy is {}): {e:#}",
                live.display(),
                kept.display()
            )),
        }
    }
    for (plist, aside) in done.moved.iter().rev() {
        match host.rename(aside, plist) {
            Ok(()) => host.say(&format!("put back: {}", plist.display())),
            Err(e) => problems.push(format!(
                "{} (it is at {}): {e:#}",
                plist.display(),
                aside.display()
            )),
        }
    }
    for (uid, plist) in &done.booted_out {
        match host.bootstrap_agent(*uid, plist) {
            Ok(()) => host.say(&format!("bootstrapped: gui/{uid}/com.roomler.agent")),
            Err(e) => problems.push(format!("gui/{uid}/com.roomler.agent: {e:#}")),
        }
    }
    if done.restarted {
        host.say("restarting the daemon onto the files put back");
        let leaving = host.status().await.ok().map(|s| DaemonInstance::of(&s));
        let back = match host.kickstart() {
            Ok(()) => wait_for_new_daemon(host, leaving, RESTART_WITHIN).await,
            Err(e) => Err(e),
        };
        match back {
            Ok(status) => host.say(&format!(
                "the daemon is back{}",
                if status.connected {
                    ", connected"
                } else {
                    ", not connected yet"
                }
            )),
            Err(e) => problems.push(format!(
                "the restart onto the files put back: {e:#}. `sudo launchctl kickstart -k \
                 system/com.roomler.daemon` retries it"
            )),
        }
    }
    problems
}

/// Waits for a daemon other than `leaving` to answer. One that never said who
/// it was counts as gone once it stops answering.
async fn wait_for_new_daemon<H: Host>(
    host: &mut H,
    leaving: Option<DaemonInstance>,
    within: Duration,
) -> Result<NodeStatus> {
    let known = leaving.filter(|l| l.pid.is_some() || l.started_at_ms.is_some());
    let mut gone = false;
    let start = host.now();
    loop {
        match host.status().await {
            Ok(status) => {
                let new = match known {
                    Some(old) => DaemonInstance::of(&status) != old,
                    None => gone,
                };
                if new {
                    return Ok(status);
                }
            }
            Err(_) => gone = true,
        }
        if host.now().saturating_sub(start) >= within {
            bail!("no restarted daemon answered within {} s", within.as_secs());
        }
        host.sleep(POLL).await;
    }
}

/// The Verify step: the restarted daemon has every part of one row within
/// `within`, or the error says the part it lacked last.
async fn verify<H: Host>(host: &mut H, moved: &[String], within: Duration) -> Result<NodeStatus> {
    let start = host.now();
    loop {
        let last = match host.status().await {
            Ok(status) => match missing(&status, moved) {
                None => return Ok(status),
                Some(what) => what,
            },
            Err(e) => format!("the daemon did not answer: {e:#}"),
        };
        if host.now().saturating_sub(start) >= within {
            bail!("the verify failed: after {} s, {last}", within.as_secs());
        }
        host.sleep(POLL).await;
    }
}

/// The Report step.
fn report<H: Host>(host: &mut H, read: &Read, plan: &Plan, after: &NodeStatus) {
    let words = after
        .gui_worker
        .as_ref()
        .and_then(|w| w.permissions.clone())
        .unwrap_or_default();
    host.say(&format!(
        "one row: the root half is connected, and its GUI worker attached ({})",
        words.join(", ")
    ));
    match (&read.status.overlay_ip, &after.overlay_ip) {
        (Some(was), Some(now)) if was == now => {
            host.say(&format!("overlay address: {now}, as before"))
        }
        (Some(was), Some(now)) => host.say(&format!("overlay address: {now} (it was {was})")),
        (Some(was), None) => host.say(&format!(
            "overlay address: not up yet (it was {was}); `roomler status` shows it"
        )),
        (None, _) => {}
    }
    let moved = plan.moved_labels();
    if !moved.is_empty() {
        host.say(&format!(
            "moved into the root's [[orgs]]: {}. The same rows, served by the root half",
            moved.join(", ")
        ));
    }
    let retired: Vec<&Item> = plan.retired().collect();
    if !retired.is_empty() {
        host.say("the rows this Mac no longer uses read offline until a person deletes them:");
        for item in &retired {
            host.say(&format!(
                "  \"{}\", agent {}, tenant {}, {}: DELETE {}/api/tenant/{}/agent/{}",
                item.name,
                item.agent_id,
                item.tenant_id,
                item.server_url,
                item.server_url.trim_end_matches('/'),
                item.tenant_id,
                item.agent_id
            ));
        }
        host.say(
            "delete each from the dashboard's device list, or with that request. Until then, \
             `sudo roomlerd one-row --revert` puts the two rows back",
        );
    }
}

/// What `--revert` puts back: each copy the migration kept, and each plist it
/// moved aside.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Revert {
    /// `(kept, live)`, the root's file first.
    pub configs: Vec<(PathBuf, PathBuf)>,
    /// `(uid, aside, plist)`.
    pub plists: Vec<(u32, PathBuf, PathBuf)>,
}

/// `None` when there is no copy of the root's file, so nothing to revert.
pub fn revert_plan(host: &impl Host, root_path: &Path, people: &[Person]) -> Option<Revert> {
    let root_kept = kept_path(root_path);
    if !host.exists(&root_kept) {
        return None;
    }
    let mut configs = vec![(root_kept, root_path.to_path_buf())];
    let mut plists = Vec::new();
    for person in people {
        let live = person.config_path();
        let kept = kept_path(&live);
        if host.exists(&kept) {
            configs.push((kept, live));
        }
        let aside = person.plist_aside();
        if host.exists(&aside) {
            plists.push((person.uid, aside, person.plist()));
        }
    }
    Some(Revert { configs, plists })
}

/// Whose LaunchAgent `--revert` bootstraps again: the console user's, when the
/// root file it puts back leaves that person's half to its LaunchAgent (no
/// supervision). With supervision on, the installer had removed the plist, and
/// a loaded one would make the supervisor stand down.
pub fn bootstrap_after_revert(kept_root: &AgentConfig, console_uid: Option<u32>) -> Option<u32> {
    console_uid.filter(|_| !kept_root.macos_supervise_gui_worker)
}

/// `--revert`: the copies back, the plists back, the console user's
/// LaunchAgent loaded again where [`bootstrap_after_revert`] says so, and the
/// daemon restarted onto it all.
pub async fn revert<H: Host>(host: &mut H, r: &Revert, bootstrap_for: Option<u32>) -> Result<()> {
    for (kept, live) in &r.configs {
        host.rename(kept, live)
            .with_context(|| format!("putting {} back over {}", kept.display(), live.display()))?;
        host.say(&format!("put back: {}", live.display()));
    }
    for (uid, aside, plist) in &r.plists {
        host.rename(aside, plist)
            .with_context(|| format!("putting {} back", plist.display()))?;
        host.say(&format!("put back: {}", plist.display()));
        if bootstrap_for == Some(*uid) {
            match host.bootstrap_agent(*uid, plist) {
                Ok(()) => host.say(&format!("bootstrapped: gui/{uid}/com.roomler.agent")),
                Err(e) => host.say(&format!(
                    "gui/{uid}/com.roomler.agent did not start ({e:#}); launchd loads it at the \
                     next login"
                )),
            }
        }
    }
    host.say("restarting the daemon onto the files put back: live sessions end now");
    let leaving = host.status().await.ok().map(|s| DaemonInstance::of(&s));
    host.kickstart().context("restarting the daemon")?;
    let status = wait_for_new_daemon(host, leaving, RESTART_WITHIN).await?;
    host.say(&format!(
        "two rows again: the root half is back{}",
        if status.connected {
            ", connected"
        } else {
            ", not connected yet"
        }
    ));
    host.say(
        "a retired row a person already deleted cannot come back: its token is dead, and that \
         half needs `roomlerd enroll` again",
    );
    Ok(())
}

/// `roomlerd one-row`. Refused off macOS, and without root.
pub async fn run(dry_run: bool, revert: bool, yes: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        mac::run(dry_run, revert, yes).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (dry_run, revert, yes);
        bail!("`roomlerd one-row` makes a two-row Mac one row, and this is not a Mac")
    }
}

/// One entry under `/Users` that leads to a folder, as [`scan_homes`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    /// The entry's name: the account's short name, unless the entry is a link.
    pub user: String,
    /// The entry itself, `/Users/<user>`.
    pub path: PathBuf,
    /// The folder it leads to, every link resolved.
    pub canonical: PathBuf,
    /// The owner of that folder.
    pub uid: u32,
    /// The entry is a symbolic link.
    pub via_link: bool,
}

/// One home per folder, sorted by name. A folder reached under two names is
/// one person: the tart image's `/Users/runner` → `/Users/admin` planned the
/// same file twice, and the second Keep stopped the migration with
/// `File exists` (2026-11-05). The folder's own entry wins over a link to it.
/// A folder reached only through links keeps the first of them by name, since
/// its person still has a file to migrate.
pub fn distinct_homes(mut homes: Vec<Home>) -> Vec<Home> {
    // A folder's own entry before any link, then by name: the first of each folder stays.
    homes.sort_by(|a, b| {
        a.via_link
            .cmp(&b.via_link)
            .then_with(|| a.user.cmp(&b.user))
    });
    let mut seen = std::collections::HashSet::new();
    homes.retain(|h| seen.insert(h.canonical.clone()));
    homes.sort_by(|a, b| a.user.cmp(&b.user));
    homes
}

/// Every entry under `root` (`/Users` on a Mac) that leads to a folder: not
/// hidden, not `Shared`, and resolvable, a link followed and marked. Outside
/// `mod mac`, so the Linux lane runs its tests.
#[cfg(unix)]
pub fn scan_homes(root: &Path) -> Vec<Home> {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let user = entry.file_name().to_string_lossy().into_owned();
        if user.starts_with('.') || user == "Shared" {
            continue;
        }
        let path = entry.path();
        let (Ok(meta), Ok(canonical)) = (std::fs::metadata(&path), std::fs::canonicalize(&path))
        else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let via_link = entry.file_type().is_ok_and(|t| t.is_symlink());
        out.push(Home {
            user,
            path,
            canonical,
            uid: meta.uid(),
            via_link,
        });
    }
    out
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use tunnel_core::localapi::{Request, Response};

    const DAEMON: &str = "system/com.roomler.daemon";
    const AGENT_LABEL: &str = "com.roomler.agent";
    const LOG_DIR: &str = "/var/log/roomler";
    const LOG: &str = "/var/log/roomler/one-row.log";

    pub(super) async fn run(dry_run: bool, revert_asked: bool, yes: bool) -> Result<()> {
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            bail!("one row changes the root half: run it as `sudo roomlerd one-row`");
        }
        let mut host = MacHost::new();
        let root_path = PathBuf::from(ROOT_CONFIG);
        let people = people()?;
        if revert_asked {
            return run_revert(&mut host, &root_path, &people, dry_run, yes).await;
        }
        if !root_path.exists() {
            bail!("{ROOT_CONFIG} is not here: this Mac has no root half to make one row of");
        }
        let root =
            crate::config::load(&root_path).with_context(|| format!("reading {ROOT_CONFIG}"))?;
        let status = host
            .status()
            .await
            .context("the root daemon does not answer on its LocalAPI socket")?;
        if !status.connected {
            bail!("the root half is not connected to its server: one row needs it connected");
        }
        let console_uid = console_uid().context(
            "nobody is logged in at the console, and the verify needs a person's session: log in \
             and run it again",
        )?;
        let read = Read {
            root_path,
            root,
            people,
            status,
            console_uid,
        };
        migrate(&mut host, &read, dry_run, |plan| {
            go_ahead(yes, plan_digest(plan), &["one-row", "--yes"])
        })
        .await
    }

    async fn run_revert(
        host: &mut MacHost,
        root_path: &Path,
        people: &[Person],
        dry_run: bool,
        yes: bool,
    ) -> Result<()> {
        let Some(r) = revert_plan(&*host, root_path, people) else {
            bail!(
                "nothing to revert: {} is not here",
                kept_path(root_path).display()
            );
        };
        let kept_root = crate::config::load(&r.configs[0].0)
            .with_context(|| format!("reading {}", r.configs[0].0.display()))?;
        let bootstrap_for = bootstrap_after_revert(&kept_root, console_uid());
        host.say("one row --revert: the plan. Nothing has changed yet.");
        for (kept, live) in &r.configs {
            host.say(&format!(
                "{} goes back over {}",
                kept.display(),
                live.display()
            ));
        }
        for (uid, aside, plist) in &r.plists {
            host.say(&format!(
                "{} goes back to {}{}",
                aside.display(),
                plist.display(),
                if bootstrap_for == Some(*uid) {
                    ", and is bootstrapped"
                } else {
                    ""
                }
            ));
        }
        host.say("⚠️ the daemon restarts: every live remote-desktop, SSH and tunnel session ends");
        if dry_run {
            host.say("--dry-run: nothing has changed");
            return Ok(());
        }
        go_ahead(yes, revert_digest(&r), &["one-row", "--revert", "--yes"])?;
        revert(host, &r, bootstrap_for).await
    }

    /// Set in the detached copy that applies what a person said yes to: the
    /// digest of that plan.
    const DETACHED: &str = "ROOMLERD_ONE_ROW_DETACHED";

    /// The plan has been said. Go ahead on a person's yes, and never in this
    /// process: under `roomler exec` it is the daemon's own child (`sh -c`
    /// hands a lone command its pid), which the daemon kills by pid when the
    /// restart stops it, before the verify and the undo. So a copy in a
    /// session of its own applies the plan, and this one waits for it and
    /// leaves with its code. The copy goes ahead only on the same plan.
    fn go_ahead(yes: bool, digest: u64, args: &[&str]) -> Result<()> {
        if let Ok(want) = std::env::var(DETACHED) {
            if want != digest.to_string() {
                bail!(
                    "this Mac changed after the plan was said, so nothing has changed: run it \
                     again"
                );
            }
            // SAFETY: signal has no memory-safety preconditions.
            unsafe {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
            return Ok(());
        }
        ask(yes)?;
        hand_over(digest, args)
    }

    /// A person's yes: `--yes`, or typed at a terminal. With no terminal
    /// (`roomler exec`) only `--yes` is one.
    fn ask(yes: bool) -> Result<()> {
        use std::io::IsTerminal;
        if yes {
            return Ok(());
        }
        if !std::io::stdin().is_terminal() {
            bail!("nothing has changed: run it again with --yes to go ahead");
        }
        print!("type yes to go ahead: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "yes" {
            bail!("nothing has changed");
        }
        Ok(())
    }

    /// Runs the detached copy to its end and leaves with its exit code. Once
    /// it has started, nothing here stops it: a Ctrl-C or a killed session
    /// ends this process, never a migration halfway.
    fn hand_over(digest: u64, args: &[&str]) -> ! {
        use std::os::unix::process::CommandExt;
        let run = || -> Result<i32> {
            let exe = std::env::current_exe().context("finding this command's own binary")?;
            let mut cmd = std::process::Command::new(exe);
            cmd.args(args)
                .env(DETACHED, digest.to_string())
                .stdin(std::process::Stdio::null());
            // SAFETY: setsid is async-signal-safe, which is all a pre_exec
            // hook may call.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            let mut child = cmd.spawn().context("starting the detached copy")?;
            let _ = writeln!(
                std::io::stdout(),
                "going ahead in a detached copy (pid {}): it outlives this session, and its \
                 account is also in {LOG}",
                child.id()
            );
            Ok(child.wait()?.code().unwrap_or(1))
        };
        match run() {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "{e:#}");
                std::process::exit(1)
            }
        }
    }

    /// Every account under `/Users` with a roomler folder or a LaunchAgent
    /// plist, one person per folder ([`distinct_homes`]). A file that does not
    /// parse stops Read: the plan would be a guess.
    fn people() -> Result<Vec<Person>> {
        let mut out = Vec::new();
        for found in distinct_homes(scan_homes(Path::new("/Users"))) {
            let Home {
                user,
                path: home,
                uid,
                ..
            } = found;
            if uid == 0 {
                continue;
            }
            let dir = crate::appdirs::macos_config_dir_in_home(&home, Path::exists);
            let config_path = dir.join("config.toml");
            let config = if config_path.exists() {
                Some(
                    crate::config::load(&config_path)
                        .with_context(|| format!("reading {}", config_path.display()))?,
                )
            } else {
                None
            };
            let person = Person {
                user,
                uid,
                home,
                dir,
                config,
                has_plist: false,
            };
            let has_plist = person.plist().exists();
            if person.config.is_some() || has_plist {
                out.push(Person {
                    has_plist,
                    ..person
                });
            }
        }
        out.sort_by(|a, b| a.user.cmp(&b.user));
        Ok(out)
    }

    /// The console user's uid; `None` at the login window.
    fn console_uid() -> Option<u32> {
        let uid = std::fs::metadata("/dev/console").ok()?.uid();
        (uid != 0).then_some(uid)
    }

    fn launchctl(args: &[&str]) -> std::result::Result<(), String> {
        let out = std::process::Command::new("/bin/launchctl")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn owner(path: &Path) -> Result<(u32, u32)> {
        let meta =
            std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        Ok((meta.uid(), meta.gid()))
    }

    struct MacHost {
        log: Option<std::fs::File>,
        started: std::time::Instant,
    }

    impl MacHost {
        fn new() -> Self {
            let _ = std::fs::create_dir_all(LOG_DIR);
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(LOG)
                .ok();
            Self {
                log,
                started: std::time::Instant::now(),
            }
        }
    }

    #[async_trait]
    impl Host for MacHost {
        fn say(&mut self, line: &str) {
            // Never `println!`: under `roomler exec` the pipe behind stdout
            // belongs to the daemon being restarted, and `println!` panics on a
            // closed pipe, which would stop the verify and its undo midway.
            let _ = writeln!(std::io::stdout(), "{line}");
            if let Some(log) = self.log.as_mut() {
                let _ = writeln!(log, "{} {line}", chrono::Utc::now().to_rfc3339());
            }
        }

        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }

        fn keep(&mut self, live: &Path, kept: &Path) -> Result<()> {
            let bytes = std::fs::read(live)?;
            let (uid, gid) = owner(live)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(kept)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::os::unix::fs::chown(kept, Some(uid), Some(gid))?;
            Ok(())
        }

        fn write_config(&mut self, path: &Path, cfg: &AgentConfig) -> Result<()> {
            // `save` writes a new file and renames it into place, so as root it
            // would leave a person's file owned by root, and their worker could
            // no longer read it. The owner goes back, `.prev` included.
            let (uid, gid) = owner(path)?;
            crate::config::save(&path.to_path_buf(), cfg)?;
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
            let prev = path.with_extension("toml.prev");
            if prev.exists() {
                std::os::unix::fs::chown(&prev, Some(uid), Some(gid))?;
            }
            Ok(())
        }

        fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
            // A person with a LaunchAgent plist and no roomler folder yet: the
            // plist goes into one made as theirs, which their first worker
            // then uses as its own.
            if let Some(dir) = to.parent()
                && !dir.exists()
            {
                let (uid, gid) = owner(from)?;
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                std::os::unix::fs::chown(dir, Some(uid), Some(gid))?;
            }
            std::fs::rename(from, to)?;
            Ok(())
        }

        fn agent_loaded(&mut self, uid: u32) -> bool {
            launchctl(&["print", &format!("gui/{uid}/{AGENT_LABEL}")]).is_ok()
        }

        fn bootout_agent(&mut self, uid: u32) -> Result<()> {
            let target = format!("gui/{uid}/{AGENT_LABEL}");
            let _ = launchctl(&["bootout", &target]);
            // `bootout` can return while the job still drains; `postinstall`
            // waits for it the same way, bounded.
            for _ in 0..15 {
                if !self.agent_loaded(uid) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            bail!("{target} is still loaded 15 s after `launchctl bootout`")
        }

        fn bootstrap_agent(&mut self, uid: u32, plist: &Path) -> Result<()> {
            let domain = format!("gui/{uid}");
            let plist_arg = plist.to_string_lossy();
            let mut last = String::new();
            for _ in 0..5 {
                match launchctl(&["bootstrap", &domain, &plist_arg]) {
                    Ok(()) => return Ok(()),
                    Err(e) => last = e,
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            bail!("launchctl bootstrap {domain} {plist_arg}: {last}")
        }

        async fn status(&mut self) -> Result<NodeStatus> {
            // The system socket, by name: a client's default order asks a
            // per-user daemon first, and on a two-row Mac that is the half
            // this command is retiring.
            let ask = async {
                let mut client = tunnel_core::localapi::connect_unix_served_by_root(
                    tunnel_core::localapi::system_socket_path(),
                )
                .await?;
                client.request(&Request::Status).await
            };
            match tokio::time::timeout(Duration::from_secs(5), ask).await {
                Ok(Ok(Response::Status(status))) => Ok(*status),
                Ok(Ok(other)) => bail!("the daemon answered Status with {other:?}"),
                Ok(Err(e)) => Err(e.into()),
                Err(_) => bail!("the daemon did not answer within 5 s"),
            }
        }

        fn kickstart(&mut self) -> Result<()> {
            launchctl(&["kickstart", "-k", DAEMON]).map_err(anyhow::Error::msg)
        }

        async fn sleep(&mut self, d: Duration) {
            tokio::time::sleep(d).await;
        }

        fn now(&self) -> Duration {
            self.started.elapsed()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use tunnel_core::localapi::{GuiWorkerStatus, OrgStatus};

    const SERVER: &str = "https://roomler.example";

    fn cfg(tenant: &str, agent: &str, token: &str) -> AgentConfig {
        toml::from_str(&format!(
            "server_url = \"{SERVER}\"\nagent_token = \"{token}\"\nagent_id = \"{agent}\"\n\
             tenant_id = \"{tenant}\"\nmachine_id = \"m-1\"\nmachine_name = \"Mac\"\n"
        ))
        .unwrap()
    }

    fn org(label: &str, tenant: &str, agent: &str, token: &str) -> OrgEntry {
        OrgEntry {
            label: label.to_string(),
            server_url: SERVER.to_string(),
            ws_url: None,
            agent_token: token.to_string(),
            agent_id: agent.to_string(),
            tenant_id: tenant.to_string(),
            enabled: true,
            overlay_mode: OrgOverlayMode::Off,
            overlay_wg_secret_key: Some(format!("wg-{label}")),
            overlay_wg_key_epoch: 3,
            overlay_advertised_routes: Vec::new(),
            overlay_exit_node_enabled: false,
            advertise_routes: vec!["192.168.7.0/24".to_string()],
            netstack_socks_port: None,
        }
    }

    fn person(user: &str, uid: u32, home: &Path, config: Option<AgentConfig>) -> Person {
        Person {
            user: user.to_string(),
            uid,
            home: home.to_path_buf(),
            dir: crate::appdirs::macos_config_dir_in_home(home, |_| false),
            config,
            has_plist: false,
        }
    }

    /// The root half: tenant T1 primary, and T3 as a secondary.
    fn root() -> AgentConfig {
        let mut r = cfg("t1", "root-1", "root-token");
        r.orgs.push(org("third", "t3", "root-3", "root-token-3"));
        r
    }

    /// A person's half: T1 primary (the root holds it), T2 (it does not), T3
    /// (it does).
    fn goran() -> AgentConfig {
        let mut p = cfg("t1", "user-1", "user-token");
        p.overlay_wg_secret_key = Some("wg-user".to_string());
        p.overlay_wg_key_epoch = 2;
        p.auto_grant_session = false;
        p.orgs.push(org("jovanov", "t2", "user-2", "user-token-2"));
        p.orgs.push(org("third", "t3", "user-3", "user-token-3"));
        p
    }

    #[test]
    fn retired_where_the_root_holds_the_org_and_moved_where_it_does_not() {
        let home = Path::new("/Users/goran");
        let people = vec![person("goran", 501, home, Some(goran()))];
        let plan = plan(&root(), &people).unwrap();
        let fates: Vec<(Option<&str>, &Fate)> = plan
            .items
            .iter()
            .map(|i| (i.org.as_deref(), &i.fate))
            .collect();
        assert_eq!(
            fates,
            vec![
                (None, &Fate::Retired),
                (
                    Some("jovanov"),
                    &Fate::Moved {
                        label: "jovanov".to_string()
                    }
                ),
                (Some("third"), &Fate::Retired),
            ]
        );
        let after = &plan.root_after;
        assert!(after.macos_supervise_gui_worker && after.macos_one_row);
        // The root's own enrollments are untouched, so its row keeps its node.
        assert_eq!(after.agent_id, "root-1");
        assert_eq!(after.agent_token, "root-token");
        let moved = after.find_org("jovanov").unwrap();
        // Token, agent id and WireGuard key unchanged: the same row.
        assert_eq!(moved.agent_token, "user-token-2");
        assert_eq!(moved.agent_id, "user-2");
        assert_eq!(moved.tenant_id, "t2");
        assert_eq!(moved.overlay_wg_secret_key.as_deref(), Some("wg-jovanov"));
        assert_eq!(moved.overlay_wg_key_epoch, 3);
        assert_eq!(moved.advertise_routes, vec!["192.168.7.0/24".to_string()]);
        assert_eq!(moved.overlay_mode, OrgOverlayMode::Off);
        assert_eq!(after.orgs.len(), 2, "only the moved org is appended");
        assert!(
            after
                .partition_runnable_orgs()
                .iter()
                .all(|(_, problem)| problem.is_none()),
            "the root's file runs every org it holds"
        );
        // The person's file: no enrollment, the person's knobs kept.
        assert_eq!(plan.people_after.len(), 1);
        let (index, stripped) = &plan.people_after[0];
        assert_eq!(*index, 0);
        assert!(!stripped.has_primary_enrollment());
        assert!(stripped.orgs.is_empty());
        assert!(stripped.overlay_wg_secret_key.is_none());
        assert!(!stripped.auto_grant_session, "a person's floor is kept");
        assert_eq!(stripped.machine_name, "Mac");
    }

    #[test]
    fn a_moved_primary_is_named_after_its_server_and_never_takes_a_label() {
        let mut root = root();
        root.orgs.push(org("roomler-example", "t9", "root-9", "x"));
        let mut theirs = cfg("t2", "user-2", "user-token-2");
        theirs.overlay_wg_secret_key = Some("wg-user".to_string());
        theirs.overlay_wg_key_epoch = 4;
        theirs.ws_url = Some("wss://roomler.example/ws".to_string());
        let people = vec![person(
            "goran",
            501,
            Path::new("/Users/goran"),
            Some(theirs),
        )];
        let plan = plan(&root, &people).unwrap();
        assert_eq!(
            plan.items[0].fate,
            Fate::Moved {
                label: "roomler-example-2".to_string()
            }
        );
        let moved = plan.root_after.find_org("roomler-example-2").unwrap();
        assert_eq!(moved.agent_token, "user-token-2");
        assert_eq!(moved.overlay_wg_secret_key.as_deref(), Some("wg-user"));
        assert_eq!(moved.overlay_wg_key_epoch, 4);
        assert_eq!(moved.ws_url.as_deref(), Some("wss://roomler.example/ws"));
        assert!(
            plan.root_after
                .partition_runnable_orgs()
                .iter()
                .all(|(_, problem)| problem.is_none())
        );
    }

    #[test]
    fn a_long_label_still_fits_with_its_suffix() {
        let mut root = root();
        let long = "a".repeat(32);
        root.orgs.push(org(&long, "t9", "root-9", "x"));
        let label = free_label(&root, &long);
        assert_ne!(label, long);
        assert_eq!(sanitize_org_label(&label).as_deref(), Some(label.as_str()));
    }

    #[test]
    fn a_second_person_in_an_org_the_first_one_moved_is_retired() {
        let mut a = cfg("t2", "user-a", "token-a");
        a.machine_name = "Mac (a)".to_string();
        let mut b = cfg("t2", "user-b", "token-b");
        b.machine_name = "Mac (b)".to_string();
        let people = vec![
            person("a", 501, Path::new("/Users/a"), Some(a)),
            person("b", 502, Path::new("/Users/b"), Some(b)),
        ];
        let plan = plan(&root(), &people).unwrap();
        assert!(matches!(plan.items[0].fate, Fate::Moved { .. }));
        assert_eq!(plan.items[1].fate, Fate::Retired);
        assert_eq!(plan.root_after.orgs.len(), 2);
        assert_eq!(plan.people_after.len(), 2);
    }

    #[test]
    fn an_ephemeral_half_the_root_does_not_hold_stops_the_plan() {
        let mut theirs = cfg("t2", "user-2", "token");
        theirs.ephemeral = true;
        let people = vec![person(
            "goran",
            501,
            Path::new("/Users/goran"),
            Some(theirs),
        )];
        let e = plan(&root(), &people).unwrap_err().to_string();
        assert!(e.contains("ephemeral"), "{e}");
        // One the root holds is retired, ephemeral or not.
        let mut held = cfg("t1", "user-1", "token");
        held.ephemeral = true;
        let people = vec![person("goran", 501, Path::new("/Users/goran"), Some(held))];
        assert_eq!(plan(&root(), &people).unwrap().items[0].fate, Fate::Retired);
    }

    #[test]
    fn a_root_half_with_no_enrollment_stops_the_plan() {
        let empty = AgentConfig::for_person_with_no_enrollment("Mac").unwrap();
        assert!(plan(&empty, &[]).is_err());
    }

    #[test]
    fn a_person_with_nothing_enrolled_is_left_alone() {
        let blank = AgentConfig::for_person_with_no_enrollment("Mac").unwrap();
        let people = vec![person("goran", 501, Path::new("/Users/goran"), Some(blank))];
        let plan = plan(&root(), &people).unwrap();
        assert!(plan.items.is_empty());
        assert!(plan.people_after.is_empty());
    }

    #[test]
    fn the_notes_name_what_a_person_should_know_first() {
        let mut root = root();
        root.orgs[0].enabled = false;
        let mut theirs = cfg("t2", "user-2", "token");
        theirs.overlay_enabled = true;
        let mut third = org("third", "t3", "user-3", "token-3");
        third.overlay_mode = OrgOverlayMode::Netstack;
        theirs.orgs.push(third);
        theirs.tunnel_routes = vec![tunnel_core::localapi::RouteDescriptor {
            id: String::new(),
            kind: tunnel_core::localapi::FlowKind::Forward,
            node: "n".to_string(),
            local: 8080,
            remote: Some("127.0.0.1:80".to_string()),
            transport: String::new(),
            enabled: true,
            org: None,
        }];
        let people = vec![person(
            "goran",
            501,
            Path::new("/Users/goran"),
            Some(theirs),
        )];
        let notes = plan(&root, &people).unwrap().notes.join("\n");
        assert!(notes.contains("with its overlay off"), "{notes}");
        assert!(notes.contains("only overlay address"), "{notes}");
        assert!(notes.contains("roomlerd org enable third"), "{notes}");
        assert!(notes.contains("tunnel route"), "{notes}");
    }

    #[test]
    fn without_enrollments_keeps_every_other_key() {
        let mut theirs = goran();
        theirs.rc_max_sessions = Some(2);
        theirs.privacy_mode_allowed = true;
        let stripped = without_enrollments(&theirs);
        assert!(!stripped.has_primary_enrollment());
        let identity_blank = toml::to_string(&without_enrollments(&stripped)).unwrap();
        assert_eq!(
            identity_blank,
            toml::to_string(&stripped).unwrap(),
            "idempotent"
        );
        let mut put_back = stripped.clone();
        put_back.server_url = theirs.server_url.clone();
        put_back.ws_url = theirs.ws_url.clone();
        put_back.agent_token = theirs.agent_token.clone();
        put_back.agent_id = theirs.agent_id.clone();
        put_back.tenant_id = theirs.tenant_id.clone();
        put_back.ephemeral = theirs.ephemeral;
        put_back.orgs = theirs.orgs.clone();
        put_back.overlay_wg_secret_key = theirs.overlay_wg_secret_key.clone();
        put_back.overlay_wg_key_epoch = theirs.overlay_wg_key_epoch;
        assert_eq!(
            toml::to_string(&put_back).unwrap(),
            toml::to_string(&theirs).unwrap(),
            "nothing but the identity changed"
        );
    }

    fn worker(one_row: bool, attached: bool, words: Option<&[&str]>) -> GuiWorkerStatus {
        GuiWorkerStatus {
            attached,
            permissions: words.map(|w| w.iter().map(|s| s.to_string()).collect()),
            one_row,
        }
    }

    fn status(connected: bool, gui: Option<GuiWorkerStatus>, orgs: &[(&str, bool)]) -> NodeStatus {
        let mut s: NodeStatus = serde_json::from_str(&format!(
            r#"{{"node_id":"n","name":"Mac","version":"v","mode":"service","connected":{connected}}}"#
        ))
        .unwrap();
        s.gui_worker = gui;
        s.orgs = orgs
            .iter()
            .map(|(label, connected)| OrgStatus {
                label: label.to_string(),
                server_url: SERVER.to_string(),
                tenant_id: None,
                agent_id: None,
                primary: false,
                enabled: true,
                connected: *connected,
                terminal_error: None,
                updates_ignored: 0,
                overlay_mode: "off".to_string(),
                token_expires_at: None,
            })
            .collect();
        s
    }

    #[test]
    fn one_row_is_connected_every_moved_org_and_a_worker_that_can_see_the_screen() {
        let full = worker(true, true, Some(&["screen-capture", "input"]));
        let moved = vec!["jovanov".to_string()];
        let ok = status(true, Some(full.clone()), &[("jovanov", true)]);
        assert_eq!(missing(&ok, &moved), None);
        let cases = [
            (
                status(false, Some(full.clone()), &[("jovanov", true)]),
                "not connected",
            ),
            (
                status(true, Some(full.clone()), &[("jovanov", false)]),
                "`jovanov` is not connected",
            ),
            (
                status(true, Some(full.clone()), &[]),
                "`jovanov` is not running",
            ),
            (status(true, None, &[("jovanov", true)]), "does not report"),
            (
                status(
                    true,
                    Some(worker(false, true, Some(&["screen-capture"]))),
                    &[("jovanov", true)],
                ),
                "does not run one row",
            ),
            (
                status(true, Some(worker(true, false, None)), &[("jovanov", true)]),
                "no GUI worker",
            ),
            (
                status(true, Some(worker(true, true, None)), &[("jovanov", true)]),
                "not announced",
            ),
            (
                status(
                    true,
                    Some(worker(true, true, Some(&["input"]))),
                    &[("jovanov", true)],
                ),
                "no Screen Recording grant (it reports: input)",
            ),
        ];
        for (s, want) in cases {
            let got = missing(&s, &moved).unwrap_or_default();
            assert!(got.contains(want), "{got:?} should say {want:?}");
        }
    }

    /// A Mac in a temp dir: the root's file, one person's file and plist, and
    /// a daemon whose answers depend on how often it was restarted.
    struct Fake {
        tmp: tempfile::TempDir,
        read: Read,
        said: Vec<String>,
        calls: Vec<String>,
        counts: HashMap<&'static str, usize>,
        /// Fail the n-th call of this effect.
        fail: Option<(&'static str, usize)>,
        loaded: HashSet<u32>,
        kicks: u32,
        /// After how many polls a restarted daemon answers as the new one.
        lag: u32,
        polls_since_kick: u32,
        /// What the daemon reports once restarted `n` times.
        daemon: fn(u32) -> NodeStatus,
        /// The virtual clock: only a sleep moves it.
        clock: Duration,
    }

    fn one_row_daemon(kicks: u32) -> NodeStatus {
        match kicks {
            0 => status(true, Some(worker(false, false, None)), &[("third", true)]),
            1 => status(
                true,
                Some(worker(true, true, Some(&["screen-capture", "input"]))),
                &[("third", true), ("jovanov", true)],
            ),
            _ => status(true, Some(worker(false, false, None)), &[("third", true)]),
        }
    }

    fn no_grant_daemon(kicks: u32) -> NodeStatus {
        match kicks {
            1 => status(
                true,
                Some(worker(true, true, Some(&["input"]))),
                &[("third", true), ("jovanov", true)],
            ),
            _ => one_row_daemon(kicks),
        }
    }

    impl Fake {
        fn new(daemon: fn(u32) -> NodeStatus) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root_path = dir.path().join("etc").join("config.toml");
            std::fs::create_dir_all(root_path.parent().unwrap()).unwrap();
            crate::config::save(&root_path, &root()).unwrap();
            let home = dir.path().join("Users").join("goran");
            let mut goran_half = person("goran", 501, &home, Some(goran()));
            std::fs::create_dir_all(&goran_half.dir).unwrap();
            crate::config::save(&goran_half.config_path(), &goran()).unwrap();
            std::fs::create_dir_all(goran_half.plist().parent().unwrap()).unwrap();
            std::fs::write(goran_half.plist(), "<plist/>").unwrap();
            goran_half.has_plist = true;
            let read = Read {
                root_path,
                root: root(),
                people: vec![goran_half],
                status: daemon(0),
                console_uid: 501,
            };
            Self {
                tmp: dir,
                read,
                said: Vec::new(),
                calls: Vec::new(),
                counts: HashMap::new(),
                fail: None,
                loaded: HashSet::from([501]),
                kicks: 0,
                lag: 2,
                polls_since_kick: 0,
                daemon,
                clock: Duration::ZERO,
            }
        }

        fn hit(&mut self, effect: &'static str) -> Result<()> {
            let n = self.counts.entry(effect).or_default();
            *n += 1;
            if self.fail == Some((effect, *n)) {
                bail!("{effect} #{n} failed (the test said so)");
            }
            Ok(())
        }

        /// Every file on the fake Mac, by path, with its bytes.
        fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
            let mut out = Vec::new();
            let mut stack = vec![self.tmp.path().to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).unwrap().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if !p.to_string_lossy().ends_with(".prev") {
                        // `.prev` is `save`'s own rotation, not the Mac's state.
                        out.push((p.clone(), std::fs::read(&p).unwrap()));
                    }
                }
            }
            out.sort();
            out
        }

        fn said(&self) -> String {
            self.said.join("\n")
        }
    }

    #[async_trait]
    impl Host for Fake {
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }
        fn keep(&mut self, live: &Path, kept: &Path) -> Result<()> {
            self.hit("keep")?;
            assert!(!kept.exists(), "a copy is never written over");
            std::fs::copy(live, kept)?;
            self.calls.push(format!("keep {}", live.display()));
            Ok(())
        }
        fn write_config(&mut self, path: &Path, cfg: &AgentConfig) -> Result<()> {
            self.hit("write")?;
            crate::config::save(&path.to_path_buf(), cfg)?;
            self.calls.push(format!("write {}", path.display()));
            Ok(())
        }
        fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
            self.hit("rename")?;
            std::fs::rename(from, to)?;
            Ok(())
        }
        fn agent_loaded(&mut self, uid: u32) -> bool {
            self.loaded.contains(&uid)
        }
        fn bootout_agent(&mut self, uid: u32) -> Result<()> {
            self.hit("bootout")?;
            self.loaded.remove(&uid);
            self.calls.push(format!("bootout {uid}"));
            Ok(())
        }
        fn bootstrap_agent(&mut self, uid: u32, plist: &Path) -> Result<()> {
            self.hit("bootstrap")?;
            assert!(
                plist.exists(),
                "a LaunchAgent is bootstrapped from its plist"
            );
            self.loaded.insert(uid);
            self.calls.push(format!("bootstrap {uid}"));
            Ok(())
        }
        async fn status(&mut self) -> Result<NodeStatus> {
            self.polls_since_kick += 1;
            // The leaving daemon still answers for a moment after the ask.
            let kicks = if self.kicks > 0 && self.polls_since_kick <= self.lag {
                self.kicks - 1
            } else {
                self.kicks
            };
            let mut s = (self.daemon)(kicks);
            s.pid = Some(1000 + kicks);
            s.started_at_ms = Some(u64::from(kicks));
            Ok(s)
        }
        fn kickstart(&mut self) -> Result<()> {
            self.hit("kickstart")?;
            self.kicks += 1;
            self.polls_since_kick = 0;
            self.calls.push("kickstart".to_string());
            Ok(())
        }
        async fn sleep(&mut self, d: Duration) {
            self.clock += d;
        }
        fn now(&self) -> Duration {
            self.clock
        }
    }

    #[tokio::test]
    async fn a_migration_that_verifies_reports_the_retired_rows_and_keeps_its_copies() {
        let mut mac = Fake::new(one_row_daemon);
        let read = mac.read.clone();
        let root_before = std::fs::read(&read.root_path).unwrap();
        let person_before = std::fs::read(read.people[0].config_path()).unwrap();
        let said = plan_digest(&plan(&read.root, &read.people).unwrap());
        migrate(&mut mac, &read, false, |asked| {
            // What is confirmed is the plan that was said.
            assert_eq!(plan_digest(asked), said);
            Ok(())
        })
        .await
        .unwrap();

        let root_now = crate::config::load(&read.root_path).unwrap();
        assert!(root_now.macos_supervise_gui_worker && root_now.macos_one_row);
        assert_eq!(root_now.agent_token, "root-token");
        assert_eq!(
            root_now.find_org("jovanov").map(|o| o.agent_token.as_str()),
            Some("user-token-2")
        );
        let person_now = crate::config::load(&read.people[0].config_path()).unwrap();
        assert!(!person_now.has_primary_enrollment() && person_now.orgs.is_empty());
        // The copies are the files as they were.
        assert_eq!(
            std::fs::read(kept_path(&read.root_path)).unwrap(),
            root_before
        );
        assert_eq!(
            std::fs::read(kept_path(&read.people[0].config_path())).unwrap(),
            person_before
        );
        assert!(!read.people[0].plist().exists());
        assert!(read.people[0].plist_aside().exists());
        assert!(!mac.loaded.contains(&501), "the LaunchAgent is booted out");
        assert_eq!(mac.counts.get("kickstart"), Some(&1));
        let said = mac.said();
        assert!(
            said.contains("one row: the root half is connected"),
            "{said}"
        );
        assert!(
            said.contains(&format!("DELETE {SERVER}/api/tenant/t1/agent/user-1")),
            "{said}"
        );
        assert!(
            said.contains(&format!("DELETE {SERVER}/api/tenant/t3/agent/user-3")),
            "{said}"
        );
        assert!(
            !said.contains("agent/user-2"),
            "a moved row is not deleted: {said}"
        );
        assert!(said.contains("live sessions end"), "{said}");
    }

    #[tokio::test]
    async fn a_dry_run_writes_nothing() {
        let mut mac = Fake::new(one_row_daemon);
        let read = mac.read.clone();
        let before = mac.files();
        migrate(&mut mac, &read, true, |_| panic!("a dry run asks nothing"))
            .await
            .unwrap();
        assert_eq!(mac.files(), before);
        assert!(mac.calls.is_empty(), "{:?}", mac.calls);
        assert!(mac.said().contains("the plan"));
        assert!(mac.said().contains("--dry-run: nothing has changed"));
    }

    #[tokio::test]
    async fn no_yes_changes_nothing() {
        let mut mac = Fake::new(one_row_daemon);
        let read = mac.read.clone();
        let before = mac.files();
        let e = migrate(&mut mac, &read, false, |_| bail!("nothing has changed"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("nothing has changed"));
        assert_eq!(mac.files(), before);
        assert!(mac.calls.is_empty());
    }

    #[tokio::test]
    async fn a_migration_whose_verify_fails_leaves_the_mac_as_it_was() {
        let mut mac = Fake::new(no_grant_daemon);
        let read = mac.read.clone();
        let before = mac.files();
        let e = migrate(&mut mac, &read, false, |_| Ok(()))
            .await
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("no Screen Recording grant"), "{e}");
        assert!(e.contains("the Mac is as it was"), "{e}");
        // Both files byte for byte, the plist back, no copy left behind.
        assert_eq!(mac.files(), before);
        assert!(mac.loaded.contains(&501), "the LaunchAgent is loaded again");
        // Restarted onto the new files, then onto the old ones.
        assert_eq!(mac.counts.get("kickstart"), Some(&2));
        let order: Vec<&str> = mac
            .calls
            .iter()
            .map(String::as_str)
            .filter(|c| *c == "kickstart" || c.starts_with("bootstrap"))
            .collect();
        assert_eq!(order, vec!["kickstart", "bootstrap 501", "kickstart"]);
    }

    #[tokio::test]
    async fn a_step_that_fails_puts_back_everything_before_it() {
        // The person's file is the second write: the root's is already new.
        let mut mac = Fake::new(one_row_daemon);
        mac.fail = Some(("write", 2));
        let read = mac.read.clone();
        let before = mac.files();
        let e = migrate(&mut mac, &read, false, |_| Ok(()))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("the Mac is as it was"), "{e:#}");
        assert_eq!(mac.files(), before);
        assert!(mac.loaded.contains(&501));
        assert_eq!(mac.counts.get("kickstart"), None, "never restarted");
    }

    #[tokio::test]
    async fn a_keep_that_fails_changes_nothing() {
        let mut mac = Fake::new(one_row_daemon);
        mac.fail = Some(("keep", 2));
        let read = mac.read.clone();
        let before = mac.files();
        assert!(migrate(&mut mac, &read, false, |_| Ok(())).await.is_err());
        assert_eq!(mac.files(), before);
        assert_eq!(mac.counts.get("bootout"), None);
        assert_eq!(mac.counts.get("write"), None);
    }

    #[tokio::test]
    async fn a_restart_that_never_comes_back_puts_everything_back() {
        let mut mac = Fake::new(one_row_daemon);
        // The leaving daemon keeps answering past the restart's window.
        mac.lag = u32::MAX;
        let read = mac.read.clone();
        let before = mac.files();
        let e = migrate(&mut mac, &read, false, |_| Ok(()))
            .await
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("no restarted daemon answered"),
            "{e:#}"
        );
        assert_eq!(mac.files(), before);
        assert!(mac.loaded.contains(&501));
    }

    #[tokio::test]
    async fn copies_from_an_earlier_migration_stop_it_before_anything_changes() {
        let mut mac = Fake::new(one_row_daemon);
        let read = mac.read.clone();
        std::fs::write(kept_path(&read.root_path), "earlier").unwrap();
        let before = mac.files();
        let e = migrate(&mut mac, &read, false, |_| Ok(()))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("earlier migration"), "{e}");
        assert_eq!(mac.files(), before);
        assert!(mac.calls.is_empty());
    }

    #[tokio::test]
    async fn a_mac_that_is_one_row_already_is_left_alone() {
        let mut mac = Fake::new(one_row_daemon);
        let mut read = mac.read.clone();
        read.root.macos_supervise_gui_worker = true;
        read.root.macos_one_row = true;
        read.people[0].config = Some(AgentConfig::for_person_with_no_enrollment("Mac").unwrap());
        read.people[0].has_plist = false;
        mac.loaded.clear();
        let before = mac.files();
        migrate(&mut mac, &read, false, |_| panic!("nothing to ask"))
            .await
            .unwrap();
        assert_eq!(mac.files(), before);
        assert!(mac.said().contains("one row already"));
    }

    #[tokio::test]
    async fn revert_gives_back_the_two_row_mac() {
        let mut mac = Fake::new(one_row_daemon);
        let read = mac.read.clone();
        let before = mac.files();
        migrate(&mut mac, &read, false, |_| Ok(())).await.unwrap();
        assert_ne!(mac.files(), before);

        let r = revert_plan(&mac, &read.root_path, &read.people).unwrap();
        assert_eq!(r.configs.len(), 2);
        assert_eq!(r.plists.len(), 1);
        let kept_root = crate::config::load(&r.configs[0].0).unwrap();
        let bootstrap_for = bootstrap_after_revert(&kept_root, Some(read.console_uid));
        assert_eq!(
            bootstrap_for,
            Some(501),
            "the root left that half to its LaunchAgent"
        );
        revert(&mut mac, &r, bootstrap_for).await.unwrap();

        assert_eq!(
            mac.files(),
            before,
            "the files as they were, the copies consumed"
        );
        assert!(mac.loaded.contains(&501));
        assert_eq!(mac.counts.get("kickstart"), Some(&2));
        assert!(revert_plan(&mac, &read.root_path, &read.people).is_none());
    }

    #[test]
    fn revert_bootstraps_only_what_the_root_left_to_launchd() {
        let mut supervised = root();
        assert_eq!(bootstrap_after_revert(&supervised, Some(501)), Some(501));
        supervised.macos_supervise_gui_worker = true;
        assert_eq!(bootstrap_after_revert(&supervised, Some(501)), None);
        assert_eq!(bootstrap_after_revert(&root(), None), None);
    }

    /// The people `--revert` gives their old copies back to
    /// (`put-back-legacy`).
    fn legacy_uids(r: &Revert) -> Vec<u32> {
        let mut uids: Vec<u32> = r.plists.iter().map(|(uid, _, _)| *uid).collect();
        uids.sort_unstable();
        uids.dedup();
        uids
    }

    /// `--revert` as the command drives it, without the yes: the plan, the
    /// bit [`bootstrap_after_revert`] reads, the revert.
    async fn revert_now(mac: &mut Fake) -> Result<()> {
        let read = mac.read.clone();
        let Some(r) = revert_plan(&*mac, &read.root_path, &read.people) else {
            bail!("nothing to revert");
        };
        let kept_root = crate::config::load(&r.configs[0].0)?;
        let bootstrap_for = bootstrap_after_revert(&kept_root, Some(read.console_uid));
        revert(mac, &r, bootstrap_for).await
    }

    impl Fake {
        /// The person's files as every release before root's side left a
        /// conversion: the config's copy and the plist's in `<F>`, a blank
        /// config in its place, no plist, the agent booted out. Root's own
        /// kept copy is gone: an older revert put it back and stopped.
        fn converted_by_an_older_release(&mut self) {
            let p = self.read.people[0].clone();
            let live = p.config_path();
            std::fs::rename(&live, kept_path(&live)).unwrap();
            crate::config::save(&live, &without_enrollments(&goran())).unwrap();
            std::fs::rename(p.plist(), p.plist_aside()).unwrap();
            self.loaded.clear();
        }

        fn root_bytes(&self) -> Vec<u8> {
            std::fs::read(&self.read.root_path).unwrap()
        }
    }

    /// AC1: a Mac converted by a release before root's side, whose revert has
    /// only the old copies a person's child reports to go on.
    #[test]
    fn revert_plan_counts_legacy_names_alone() {
        let mut mac = Fake::new(one_row_daemon);
        mac.converted_by_an_older_release();
        let r = revert_plan(&mac, &mac.read.root_path, &mac.read.people)
            .expect("the old copies alone are a revert");
        assert_eq!(legacy_uids(&r), vec![501]);
    }

    /// AC1: an older revert put root's file back and stopped. The switch
    /// `bootstrap_after_revert` reads comes from root's live file, which holds
    /// the kept bytes, and root's file is not written again.
    #[tokio::test]
    async fn resumed_revert_reads_the_bit_without_a_kept_copy() {
        let mut mac = Fake::new(one_row_daemon);
        mac.converted_by_an_older_release();
        assert!(!mac.read.root.macos_supervise_gui_worker);
        let root_before = mac.root_bytes();
        revert_now(&mut mac)
            .await
            .expect("the old copies alone are a revert");
        assert!(
            mac.calls.contains(&"bootstrap 501".to_string()),
            "the console person's agent is bootstrapped: {:?}",
            mac.calls
        );
        assert_eq!(mac.root_bytes(), root_before, "root's file is not written");
    }

    /// The detached copy goes ahead only on the plan a person said yes to.
    #[test]
    fn the_digest_is_what_the_plan_does() {
        let people = vec![person(
            "goran",
            501,
            Path::new("/Users/goran"),
            Some(goran()),
        )];
        let said = plan(&root(), &people).unwrap();
        assert_eq!(
            plan_digest(&said),
            plan_digest(&plan(&root(), &people).unwrap()),
            "the same files make the same plan"
        );
        // Another enrollment in the person's file is another plan.
        let mut more = goran();
        more.orgs
            .push(org("fourth", "t4", "user-4", "user-token-4"));
        let other = vec![person("goran", 501, Path::new("/Users/goran"), Some(more))];
        assert_ne!(
            plan_digest(&said),
            plan_digest(&plan(&root(), &other).unwrap())
        );
        // So is a root that now holds what was to move.
        let mut holds = root();
        holds
            .orgs
            .push(org("jovanov", "t2", "root-2", "root-token-2"));
        assert_ne!(
            plan_digest(&said),
            plan_digest(&plan(&holds, &people).unwrap())
        );

        let r = Revert {
            configs: vec![(PathBuf::from("a.pre-one-row"), PathBuf::from("a"))],
            plists: Vec::new(),
        };
        let mut r2 = r.clone();
        assert_eq!(revert_digest(&r), revert_digest(&r2));
        r2.plists
            .push((501, PathBuf::from("p.pre-one-row"), PathBuf::from("p")));
        assert_ne!(revert_digest(&r), revert_digest(&r2));
    }

    #[test]
    fn a_kept_copy_sits_beside_its_file() {
        assert_eq!(
            kept_path(Path::new("/etc/roomler/config.toml")),
            PathBuf::from("/etc/roomler/config.toml.pre-one-row")
        );
        let p = person("goran", 501, Path::new("/Users/goran"), None);
        assert!(p.plist_aside().starts_with(&p.dir));
        assert!(
            !p.plist_aside()
                .starts_with(p.home.join("Library").join("LaunchAgents"))
        );
    }

    fn home(user: &str, canonical: &str, via_link: bool) -> Home {
        Home {
            user: user.to_string(),
            path: Path::new("/Users").join(user),
            canonical: PathBuf::from(canonical),
            uid: 501,
            via_link,
        }
    }

    #[test]
    fn a_folder_reached_under_two_names_is_one_person_named_after_the_folder() {
        // The tart image's `/Users/runner` → `/Users/admin` (2026-11-05).
        let kept = distinct_homes(vec![
            home("runner", "/Users/admin", true),
            home("admin", "/Users/admin", false),
        ]);
        assert_eq!(kept, vec![home("admin", "/Users/admin", false)]);
    }

    #[test]
    fn a_folder_reached_only_through_links_keeps_the_first_link() {
        let kept = distinct_homes(vec![
            home("zed", "/Volumes/Data/bob", true),
            home("carol", "/Users/carol", false),
            home("bob", "/Volumes/Data/bob", true),
        ]);
        assert_eq!(
            kept,
            vec![
                home("bob", "/Volumes/Data/bob", true),
                home("carol", "/Users/carol", false),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_homes_follows_a_link_and_marks_it() {
        let users = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let u = users.path();
        std::fs::create_dir(u.join("admin")).unwrap();
        std::os::unix::fs::symlink(u.join("admin"), u.join("runner")).unwrap();
        std::fs::create_dir(elsewhere.path().join("bob")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("bob"), u.join("bob")).unwrap();
        // None of these is an account.
        std::fs::create_dir(u.join("Shared")).unwrap();
        std::fs::create_dir(u.join(".hidden")).unwrap();
        std::fs::write(u.join("notes.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(u.join("gone"), u.join("dangling")).unwrap();

        let mut found = scan_homes(u);
        found.sort_by(|a, b| a.user.cmp(&b.user));
        let seen: Vec<(&str, bool)> = found
            .iter()
            .map(|h| (h.user.as_str(), h.via_link))
            .collect();
        assert_eq!(seen, [("admin", false), ("bob", true), ("runner", true)]);
        assert_eq!(
            found[0].canonical, found[2].canonical,
            "the link leads to admin's folder"
        );
        assert_eq!(found[2].path, u.join("runner"));

        let kept: Vec<String> = distinct_homes(found).into_iter().map(|h| h.user).collect();
        assert_eq!(kept, ["admin", "bob"]);
    }

    // ---- FR-147 P1: root's side, the pure core ----

    use crate::one_row_home::{
        Entry, Held, HomeEntry, Kind, LegacyEntries, ReadAnswer, ReadError, Segment, Verb,
    };

    fn account(uid: u32, name: &str) -> Account {
        Account {
            uid,
            gid: 20,
            name: name.to_string(),
            home: format!("/Users/{name}"),
            uuid: format!("UUID-{uid}"),
        }
    }

    fn dir_of(uid: u32, mode: u32) -> Entry {
        Entry {
            kind: Kind::Dir,
            uid: Some(uid),
            mode: Some(mode),
            writable: true,
        }
    }

    const ABSENT: Entry = Entry {
        kind: Kind::Absent,
        uid: None,
        mode: None,
        writable: false,
    };

    /// A home as the package leaves it, every folder the person's, as the
    /// child of `uid` reports it.
    fn seen(uid: u32) -> ReadAnswer {
        ReadAnswer {
            home: HomeEntry {
                kind: Kind::Dir,
                uid: Some(uid),
                mode: Some(0o750),
                absolute: true,
                dev: Some(1),
                ino: Some(u64::from(uid)),
            },
            library: dir_of(uid, 0o700),
            support: dir_of(uid, 0o755),
            agents_dir: dir_of(uid, 0o755),
            folder: dir_of(uid, 0o755),
            segment: Segment::Current,
            config: Held::Present {
                bytes: b"x = 1\n".to_vec(),
                uid,
                mode: 0o600,
            },
            plist: Held::Present {
                bytes: b"<plist/>".to_vec(),
                uid,
                mode: 0o644,
            },
            legacy: LegacyEntries {
                config: ABSENT,
                plist: ABSENT,
            },
        }
    }

    const ADMIN_AT_CONSOLE: Rights = Rights {
        at_console: true,
        administrator: true,
    };

    #[test]
    fn root_path_maps_into_root_trees_only() {
        let mut every = vec![
            RootFile::Config,
            RootFile::Kept,
            RootFile::RootRecord,
            RootFile::Log,
        ];
        for uid in [501, 0, u32::MAX] {
            every.extend(Part::ALL.map(|part| RootFile::Person { uid, part }));
        }
        for file in every {
            let path = root_path(file);
            assert!(
                path.starts_with("/etc/roomler") || path.starts_with("/var/log/roomler"),
                "{file:?} -> {}",
                path.display()
            );
            assert!(
                path.components()
                    .all(|c| !matches!(c, std::path::Component::ParentDir)),
                "{file:?} -> {}",
                path.display()
            );
        }
        assert_eq!(
            root_path(RootFile::Person {
                uid: 501,
                part: Part::Record
            }),
            Path::new(RECORDS).join("501").join("record.json")
        );
        assert_eq!(
            root_path(RootFile::Kept),
            Path::new("/etc/roomler/config.toml.pre-one-row")
        );
    }

    #[test]
    fn an_entry_that_is_not_its_owners_home_is_left_alone() {
        // Another account's folder behind an entry owned by 501.
        let entries = [UsersEntry {
            name: "shadow".into(),
            uid: 501,
            dev: 1,
            ino: 9999,
        }];
        let asked = accounts_to_ask(&entries, |uid| Some(account(uid, "a")));
        assert_eq!(asked.accounts.len(), 1, "its owner's child reads the home");
        let (leads, alone) = entries_of_home(&entries, &asked.accounts[0], &seen(501).home);
        assert!(
            !leads,
            "no entry of 501 leads to 501's home: no person, no convert"
        );
        assert_eq!(alone.len(), 1);
        assert!(alone[0].1.contains("/Users/shadow"), "{}", alone[0].1);
        assert!(alone[0].1.contains("does not lead"), "{}", alone[0].1);
    }

    #[test]
    fn two_entries_of_one_uid_are_one_person() {
        // The tart image: `/Users/runner`, a link to `/Users/admin`, and an
        // entry of the same uid that leads elsewhere.
        let entries = [
            UsersEntry {
                name: "runner".into(),
                uid: 501,
                dev: 1,
                ino: 501,
            },
            UsersEntry {
                name: "admin".into(),
                uid: 501,
                dev: 1,
                ino: 501,
            },
            UsersEntry {
                name: "old".into(),
                uid: 501,
                dev: 1,
                ino: 7,
            },
        ];
        let asked = accounts_to_ask(&entries, |uid| Some(account(uid, "admin")));
        assert_eq!(asked.accounts, vec![account(501, "admin")], "one child");
        assert!(asked.left_alone.is_empty());
        let (leads, alone) = entries_of_home(&entries, &asked.accounts[0], &seen(501).home);
        assert!(leads);
        let lines: Vec<&str> = alone.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("/Users/old"), "{lines:?}");
    }

    #[test]
    fn a_home_with_no_account_is_left_alone() {
        let entry = |name: &str, uid: u32| UsersEntry {
            name: name.into(),
            uid,
            dev: 1,
            ino: u64::from(uid),
        };
        let entries = [
            entry("ghost", 777),
            entry("daemon", 300),
            entry("wheelie", 502),
            entry("root", 0),
        ];
        let asked = accounts_to_ask(&entries, |uid| match uid {
            502 => Some(Account {
                gid: 0,
                ..account(502, "wheelie")
            }),
            300 => Some(account(300, "daemon")),
            _ => None,
        });
        assert!(asked.accounts.is_empty(), "no child is started");
        let said: Vec<(u32, &str)> = asked
            .left_alone
            .iter()
            .map(|(uid, l)| (*uid, l.as_str()))
            .collect();
        assert_eq!(said.len(), 4, "{said:?}");
        let line = |uid: u32| said.iter().find(|(u, _)| *u == uid).unwrap().1;
        assert!(line(0).contains("root's"), "{said:?}");
        assert!(line(300).contains("under 500"), "{said:?}");
        assert!(line(502).contains("primary group is 0"), "{said:?}");
        assert!(line(777).contains("no account has uid 777"), "{said:?}");
        assert!(line(777).contains("/Users/ghost"), "{said:?}");
    }

    /// D2: an administrator's line only where all four conditions hold, and
    /// #3846's printing tests pass; each failing alone gives the words.
    #[test]
    fn sudo_line_only_where_it_fits() {
        let a = account(501, "goran");
        let mut l_roots = seen(501);
        l_roots.agents_dir = Entry {
            kind: Kind::Dir,
            uid: Some(0),
            mode: Some(0o755),
            writable: false,
        };
        let mut config_roots = seen(501);
        config_roots.config = Held::Unreadable {
            error: ReadError::Eacces,
            uid: Some(0),
            mode: Some(0o600),
        };
        let line_for = |path: &str| {
            format!(
                "goran is an administrator of this Mac and can run this, then run `sudo roomlerd \
                 one-row` again:\n    sudo chown -h 'goran' '{path}'"
            )
        };
        let config_path = format!(
            "{}/config.toml",
            folder_display("/Users/goran", Segment::Current)
        );
        for (target, base, path) in [
            (
                Target::AgentsDir,
                &l_roots,
                "/Users/goran/Library/LaunchAgents".to_string(),
            ),
            (Target::Config, &config_roots, config_path),
        ] {
            let words = chown_words(&a, base, target, ADMIN_AT_CONSOLE, "root");
            assert!(words.contains(&line_for(&path)), "{words}");
            assert!(
                words.starts_with(&format!("{path} (")) && words.contains("uid 0, mode"),
                "{words}"
            );
            let no_command = |words: &str, case: &str| {
                assert!(!words.contains("sudo chown"), "{case}: {words}");
                assert!(words.contains(INSTALL_DOC), "{case}: {words}");
                assert!(words.contains("belongs to root (uid 0"), "{case}: {words}");
            };
            // 1. Not the console person's home.
            let rights = Rights {
                at_console: false,
                ..ADMIN_AT_CONSOLE
            };
            no_command(
                &chown_words(&a, base, target, rights, "root"),
                "not at the console",
            );
            // 2. Not an administrator.
            let rights = Rights {
                administrator: false,
                ..ADMIN_AT_CONSOLE
            };
            no_command(&chown_words(&a, base, target, rights, "root"), "not admin");
            // 3. The home is no plain absolute folder, or its Library is not
            // a folder of the person's.
            let mut s = base.clone();
            s.home.absolute = false;
            no_command(
                &chown_words(&a, &s, target, ADMIN_AT_CONSOLE, "root"),
                "home not absolute",
            );
            let mut s = base.clone();
            s.home.kind = Kind::Link;
            no_command(
                &chown_words(&a, &s, target, ADMIN_AT_CONSOLE, "root"),
                "home a link",
            );
            let mut s = base.clone();
            s.library.uid = Some(0);
            no_command(
                &chown_words(&a, &s, target, ADMIN_AT_CONSOLE, "root"),
                "Library root's",
            );
            let mut s = base.clone();
            s.library.kind = Kind::Link;
            no_command(
                &chown_words(&a, &s, target, ADMIN_AT_CONSOLE, "root"),
                "Library a link",
            );
            // #3846's printing tests: the name, and a path that would not
            // survive `sh -c "…"` or not display as it runs.
            let odd = Account {
                name: "go ran".into(),
                ..a.clone()
            };
            let words = chown_words(&odd, base, target, ADMIN_AT_CONSOLE, "root");
            assert!(!words.contains("sudo chown"), "{words}");
            for home in ["/Users/go$ran", "/Users/go\u{202E}ran"] {
                let odd = Account {
                    home: home.into(),
                    ..a.clone()
                };
                let words = chown_words(&odd, base, target, ADMIN_AT_CONSOLE, "root");
                assert!(!words.contains("sudo chown"), "{home}: {words}");
            }
        }
        // 4. The target is not root's, or not a folder (R1) or a file (R3).
        let mut s = l_roots.clone();
        s.agents_dir.uid = Some(502);
        let words = chown_words(&a, &s, Target::AgentsDir, ADMIN_AT_CONSOLE, "bob");
        assert!(!words.contains("sudo chown"), "{words}");
        assert!(words.contains("belongs to bob (uid 502"), "{words}");
        let mut s = l_roots.clone();
        s.agents_dir.kind = Kind::Link;
        let words = chown_words(&a, &s, Target::AgentsDir, ADMIN_AT_CONSOLE, "root");
        assert!(!words.contains("sudo chown"), "{words}");
        assert!(words.contains("(a link)"), "{words}");
        let mut s = config_roots.clone();
        s.config = Held::Unreadable {
            error: ReadError::Eacces,
            uid: Some(502),
            mode: Some(0o600),
        };
        let words = chown_words(&a, &s, Target::Config, ADMIN_AT_CONSOLE, "bob");
        assert!(!words.contains("sudo chown"), "{words}");
        // The person's own folder, but not writable: no command, and never
        // "belongs to".
        let mut s = l_roots.clone();
        s.agents_dir.uid = Some(501);
        s.agents_dir.mode = Some(0o555);
        let words = chown_words(&a, &s, Target::AgentsDir, ADMIN_AT_CONSOLE, "goran");
        assert!(!words.contains("sudo chown"), "{words}");
        assert!(
            words.contains("is goran's own (uid 501, mode 555)"),
            "{words}"
        );
    }

    /// R2 and R4 (#3802): #3846's text, from the child's report alone. A
    /// folder that holds a config (R2) never gets a command; one that does
    /// not (R4) gets the administrator's `sudo mv` where the package's rule
    /// fits.
    #[test]
    fn the_folder_refusals_print_move_asides_own_text() {
        let a = account(501, "goran");
        let mut r2 = seen(501);
        r2.folder = Entry {
            kind: Kind::Dir,
            uid: Some(0),
            mode: Some(0o755),
            writable: false,
        };
        let folder = folder_display("/Users/goran", Segment::Current);
        assert_eq!(
            folder,
            "/Users/goran/Library/Application Support/live.roomler.roomler"
        );
        let words = folder_words(&a, &r2, ADMIN_AT_CONSOLE, "root", "20261119T060000Z");
        assert!(
            words.starts_with(&format!(
                "{folder} (a folder) belongs to root (uid 0, mode 755), not to goran."
            )),
            "{words}"
        );
        assert!(words.contains("an entry named config.toml"), "{words}");
        assert!(
            !words.contains("sudo mv"),
            "R2 never gets a command: {words}"
        );

        let mut r4 = r2.clone();
        r4.config = Held::Absent;
        let words = folder_words(&a, &r4, ADMIN_AT_CONSOLE, "root", "20261119T060000Z");
        assert!(words.contains("sudo mv"), "{words}");
        assert!(
            words.contains(&format!("{folder}.made-by-root-20261119T060000Z")),
            "{words}"
        );
        // Not at the console, not an administrator, or Application Support
        // not the person's own: the words, no command.
        for (case, rights, support_uid) in [
            (
                "not at the console",
                Rights {
                    at_console: false,
                    ..ADMIN_AT_CONSOLE
                },
                501,
            ),
            (
                "not admin",
                Rights {
                    administrator: false,
                    ..ADMIN_AT_CONSOLE
                },
                501,
            ),
            ("support root's", ADMIN_AT_CONSOLE, 0),
        ] {
            let mut s = r4.clone();
            s.support.uid = Some(support_uid);
            let words = folder_words(&a, &s, rights, "root", "20261119T060000Z");
            assert!(!words.contains("sudo mv"), "{case}: {words}");
            assert!(
                words.contains("Only an administrator of this Mac can move it aside"),
                "{case}: {words}"
            );
        }
    }

    #[test]
    fn r5_says_why_the_console_person_was_left_alone() {
        let words = r5_words(501, Some("no account has uid 501"));
        assert!(words.contains("uid 501"), "{words}");
        assert!(words.contains("no account has uid 501"), "{words}");
        assert!(words.contains("the verify would fail"), "{words}");
        assert!(!words.contains("sudo "), "no command: {words}");
    }

    #[test]
    fn bootstrap_argv_is_todays_root_form() {
        assert_eq!(
            bootstrap_argv(501, "/Users/a"),
            [
                "bootstrap",
                "gui/501",
                "/Users/a/Library/LaunchAgents/com.roomler.agent.plist"
            ]
        );
    }

    #[test]
    fn the_spawn_plan_is_the_packages_binary_as_the_person() {
        let a = account(501, "a");
        for verb in [Verb::Read, Verb::PutBack, Verb::PutBackLegacy] {
            let plan = spawn_plan(&a, verb);
            assert_eq!(
                plan.program,
                "/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd"
            );
            assert_eq!(
                plan.args,
                crate::one_row_home::child_args(501, 20, verb),
                "child_args(uid, pw_gid, verb)"
            );
            assert!(plan.env.is_empty(), "an empty environment");
            assert_eq!(plan.dir, "/");
            assert_eq!((plan.uid, plan.gid), (501, 20));
        }
        // Started only when `lstat` reads a regular file of uid 0 that group
        // and others cannot write.
        assert_eq!(program_refusal(Ok((0, 0o100755))), None);
        for (case, stat) in [
            ("a link", Ok((0, 0o120755))),
            ("not root's", Ok((501, 0o100755))),
            ("group-writable", Ok((0, 0o100775))),
            ("other-writable", Ok((0, 0o100757))),
            ("a folder", Ok((0, 0o040755))),
            (
                "unread",
                Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            ),
        ] {
            let why = program_refusal(stat);
            assert!(why.is_some(), "{case} is refused");
        }
    }
}
