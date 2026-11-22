// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-141 P2 — the wizard's one-row form for a Mac, and whether it is open.
//!
//! With the server's `wizard_macos_one_row` on, the macOS daemon card with
//! "Join the overlay network" ticked (the macOS default) makes a Mac that is
//! not enrolled yet ONE device row (FR-43): root's daemon, which owns the
//! overlay and supervises the person's screen worker, enrolled with the
//! install's one token ([`crate::role::Layout::OneRow`]). Unticked it is the
//! screen alone (FR-129 B1); with the switch off the box makes FR-129 B2's two
//! rows, as before.
//!
//! **An enrolled Mac keeps its layout.** Migration starts with the person's own
//! `sudo roomlerd one-row` (the operator's 2026-11-02 rule, #971), so the form
//! is closed on any Mac that holds an enrollment. The wizard runs as the
//! person, who cannot read root's config, so it reads what the person can:
//!
//! - **R1**, root's `/etc/roomler/config.toml` exists: the root service is
//!   enrolled (two rows, or P2's supervised worker; a Mac that is one row
//!   already is stopped earlier, by [`crate::reinstall_guard`]). This run's own
//!   one-row attempt, kept for a Retry, is not counted: its config is the one
//!   the Retry finishes, and the install half compares the two `agent_id`s
//!   before it keeps it (`scripts/install.sh`, `install_macos_one_row_half`).
//! - **R3**, the person's own config, in either folder tree, names a device
//!   (the primary's `agent_id` or an org's), or cannot be read: the screen half
//!   is enrolled. A one-row Mac's worker leaves that config without one.
//!
//! **The release (D4-A).** The one-row form needs a release that knows it,
//! [`FIRST_ONE_ROW_RELEASE`] or later, and the version is compared before any
//! token is spent. A release this cannot name or read closes the form.
//!
//! [`decide`] is THE rule, pure, for every place that asks: the Welcome page,
//! pairing start, and the install, which asks again before it spends a token,
//! so the approval and the install make the same layout. [`read_here`] reads
//! its inputs on this host.

use std::path::Path;

use crate::role::HostOs;

/// FR-43 P3d's first release, field-read on 2026-11-05 (FR-43's field log:
/// the served `install.sh --one-row` made a fresh Mac one row from one token).
/// It is also the first whose `roomlerd enroll --help` lists `--one-row`, which
/// is what the install half's belt reads once the package has run: a release
/// below it would fail there only after the token was spent, so the form is
/// closed before (D4-A).
pub const FIRST_ONE_ROW_RELEASE: &str = "agent-v0.4.201";

/// The macOS package's name in a release, as the install half picks it
/// (`install_daemon_macos`: the first asset named `…aarch64-apple-darwin….pkg`).
const MACOS_PACKAGE: &str = "aarch64-apple-darwin";

/// What the person's own config says (R3), over both folder trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonConfig {
    /// No config in either tree, or none that names a device.
    NotEnrolled,
    /// A config names a device: the primary's or an org's.
    Enrolled,
    /// A config is there and cannot be read: never guessed, so read as
    /// enrolled.
    Unreadable,
}

/// The release the install would take (D4-A), as the server's
/// `/api/agent/latest-release` names it for the package the install half
/// downloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Release {
    Tag(String),
    /// The list could not be fetched or read, or holds no macOS package.
    Unknown,
}

/// Why the one-row form is closed on this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Closed {
    /// The server's switch is off, or this is not a Mac: nothing changes.
    NotOffered,
    /// R1: root's config is there, and not this run's own attempt.
    RootEnrolled,
    /// R3: the person's own config names a device, or cannot be read.
    PersonEnrolled,
    /// D4-A: the release predates the one-row form.
    ReleaseTooOld(String),
    /// D4-A: which release the install would take is not known.
    ReleaseUnknown,
}

impl Closed {
    /// What the page says beside the card while the form is closed for this
    /// Mac's own reasons. `None` when nothing changes for the person (the
    /// server's switch is off: the card reads as it always has).
    pub fn note(&self) -> Option<String> {
        let ticked = "with \"Join the overlay network\" ticked";
        match self {
            Closed::NotOffered => None,
            Closed::RootEnrolled => Some(format!(
                "This Mac's root service is enrolled already, so it keeps its layout: {ticked}, \
                 this install keeps its two devices. To make it one device, wait five minutes \
                 after this install, then run sudo roomlerd one-row in Terminal (sooner, it \
                 stops and leaves the Mac as it was). It says its plan and asks first, and \
                 --revert puts the Mac back."
            )),
            Closed::PersonEnrolled => Some(format!(
                "This Mac's screen sharing is enrolled already, so it keeps its layout: {ticked}, \
                 this install adds the private network as a second device. For one device \
                 instead, run the install script with --one-row and a new token, then delete \
                 the old device in Devices."
            )),
            Closed::ReleaseTooOld(tag) => Some(format!(
                "The release this server installs ({tag}) cannot make a Mac one device, so \
                 {ticked} this install makes two."
            )),
            Closed::ReleaseUnknown => Some(format!(
                "This server did not say which release it installs, so {ticked} this install \
                 makes two devices."
            )),
        }
    }
}

/// The facts [`decide`] reads.
#[derive(Clone, Debug)]
pub struct Facts {
    pub os: HostOs,
    /// [`crate::features::SetupFeatures::one_row_offered`].
    pub switch_on: bool,
    /// R1, with this run's own kept one-row attempt already left out.
    pub root_config: bool,
    /// R3.
    pub person: PersonConfig,
    pub release: Release,
}

/// THE rule: open only on a Mac, with the server's switch on, enrolled
/// nowhere (neither R1 nor R3), whose install would take a release that knows
/// the one-row form.
pub fn decide(f: &Facts) -> Result<(), Closed> {
    decide_here(f.os, f.switch_on, f.root_config, f.person)?;
    decide_release(&f.release)
}

/// [`decide`] without the release: what this host says.
fn decide_here(
    os: HostOs,
    switch_on: bool,
    root_config: bool,
    person: PersonConfig,
) -> Result<(), Closed> {
    if os != HostOs::Macos || !switch_on {
        return Err(Closed::NotOffered);
    }
    if root_config {
        return Err(Closed::RootEnrolled);
    }
    if person != PersonConfig::NotEnrolled {
        return Err(Closed::PersonEnrolled);
    }
    Ok(())
}

/// D4-A: the version compare, before anything else of the install.
fn decide_release(release: &Release) -> Result<(), Closed> {
    match release {
        Release::Unknown => Err(Closed::ReleaseUnknown),
        Release::Tag(tag) if release_knows_one_row(tag) => Ok(()),
        Release::Tag(tag) => Err(Closed::ReleaseTooOld(tag.clone())),
    }
}

/// Is `tag` [`FIRST_ONE_ROW_RELEASE`] or later? A tag the updater's parser
/// cannot read is not (`roomlerd::updater::parse_version`).
pub fn release_knows_one_row(tag: &str) -> bool {
    use roomlerd::updater::parse_version;
    match (parse_version(tag), parse_version(FIRST_ONE_ROW_RELEASE)) {
        (Some(tag), Some(first)) => tag >= first,
        _ => false,
    }
}

/// R3 in one config's text: a device named by the primary's `agent_id` or by
/// an org entry's. A text that is not TOML is unreadable, never guessed.
pub fn person_config_text(text: &str) -> PersonConfig {
    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return PersonConfig::Unreadable;
    };
    let names_a_device = |v: &toml::Value| {
        v.get("agent_id")
            .and_then(toml::Value::as_str)
            .is_some_and(|id| !id.trim().is_empty())
    };
    let org_device = value
        .get("orgs")
        .and_then(toml::Value::as_array)
        .is_some_and(|orgs| orgs.iter().any(names_a_device));
    if names_a_device(&value) || org_device {
        PersonConfig::Enrolled
    } else {
        PersonConfig::NotEnrolled
    }
}

/// R3 for the person whose home is `home`: the config in each folder tree a
/// Mac has had, the current one and the one from before the rename, named
/// through the agent's own resolver (`appdirs::macos_config_dir_in_home`), as
/// the daemon names them. Enrolled anywhere wins; unreadable anywhere comes
/// next.
pub fn person_config_in(home: &Path) -> PersonConfig {
    use roomlerd::appdirs::macos_config_dir_in_home;
    let current = macos_config_dir_in_home(home, |_| false);
    let before = macos_config_dir_in_home(home, |p| p != current);
    let mut seen = PersonConfig::NotEnrolled;
    for dir in [current, before] {
        let file = dir.join("config.toml");
        let read = match std::fs::read_to_string(&file) {
            Ok(text) => person_config_text(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => PersonConfig::Unreadable,
        };
        match read {
            PersonConfig::Enrolled => return PersonConfig::Enrolled,
            PersonConfig::Unreadable => seen = PersonConfig::Unreadable,
            PersonConfig::NotEnrolled => {}
        }
    }
    seen
}

/// The release in a `/api/agent/latest-release` body, picked as the install
/// half picks its package (`asset_field_for` and `name_release` in
/// `install.sh`): the first release, in the server's order, with an asset
/// whose whole name holds the macOS package's triple and ends in `.pkg`, and
/// which has a download URL.
pub fn release_in(body: &[u8]) -> Release {
    let Ok(serde_json::Value::Array(releases)) = serde_json::from_slice(body) else {
        return Release::Unknown;
    };
    let text = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let is_the_package = |a: &serde_json::Value| {
        text(a, "name").is_some_and(|n| n.contains(MACOS_PACKAGE) && n.ends_with(".pkg"))
            && text(a, "browser_download_url").is_some()
    };
    releases
        .iter()
        .find(|r| {
            r.get("assets")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|assets| assets.iter().any(is_the_package))
        })
        .and_then(|r| text(r, "tag_name"))
        .map_or(Release::Unknown, Release::Tag)
}

/// Ask `server` which release its install half would take. Never fails: a
/// server that cannot be asked is [`Release::Unknown`], which closes the form.
async fn release_for(server: &str) -> Release {
    let url = format!(
        "{}/api/agent/latest-release",
        crate::features::origin(server)
    );
    let client = match wizard_shared::http::builder()
        .user_agent(crate::proxy::USER_AGENT)
        .timeout(crate::features::FETCH_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(_) => return Release::Unknown,
    };
    let release = match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            release_in(&resp.bytes().await.unwrap_or_default())
        }
        _ => Release::Unknown,
    };
    tracing::info!(%url, ?release, "the release a one-row install would take");
    release
}

/// Whether the one-row form is open on this host for `server`, whose
/// switches are `features` (asked by the caller). `ours`: this run's own
/// one-row attempt is kept for a Retry ([`crate::unix_daemon::kept_one_row`]),
/// so root's config may be that attempt's. The release is asked last, only
/// when everything else leaves the form open.
pub async fn read_here(
    server: &str,
    features: crate::features::SetupFeatures,
    ours: bool,
) -> Result<(), Closed> {
    let os = HostOs::current();
    if !features.one_row_offered(os) {
        return Err(Closed::NotOffered);
    }
    let root_config = !ours && root_config_seen(&crate::unix_daemon::system_config_path());
    decide_here(os, true, root_config, person_config_here())?;
    decide_release(&release_for(server).await)
}

/// R1's read of root's config at `path`: whether it counts as there.
///
/// FR-141 P2's follow-up (F5) — only "no such file" reads as absent. Any
/// other answer (a folder this person may not search, a path through a file,
/// an I/O error) is a Mac R1 cannot tell about, so it counts as enrolled and
/// closes the form, as R3 reads a person's config it cannot read
/// ([`PersonConfig::Unreadable`]). `is_file()` read every error as "not
/// enrolled", and the one token was then spent on a Mac that may be enrolled.
pub fn root_config_seen(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Read the form for one page or command: on this host, for `server`, asked
/// now, with this run's kept one-row attempt counted as its own.
pub async fn read_for(server: &str) -> Result<(), Closed> {
    let os = HostOs::current();
    if os != HostOs::Macos {
        return Err(Closed::NotOffered);
    }
    let features = crate::features::ask(server).await.features;
    read_here(server, features, crate::unix_daemon::kept_one_row(server)).await
}

/// The layout an install of `role`, with the private-network box as
/// `overlay_box`, makes on this host for `server`: what a pairing asks the
/// approval for, and what an install spends its tokens on. Each reads it
/// fresh, before any token exists or is spent. Only a Mac's daemon with the
/// box ticked asks anything.
pub async fn layout_for(
    server: &str,
    role: crate::role::Role,
    overlay_box: Option<bool>,
) -> crate::role::Layout {
    let os = HostOs::current();
    if !crate::role::wants_root_half(os, role, overlay_box) {
        return crate::role::Layout::Device;
    }
    let open = read_for(server).await;
    if let Err(closed) = &open {
        tracing::info!(?closed, "the one-row form is closed: two rows");
    }
    crate::role::layout(os, role, overlay_box, open.is_ok())
}

/// The install's refusal when the form it was started with closed since the
/// page, or the pairing, read it: before the one token is spent.
pub fn closed_since_read(closed: &Closed) -> String {
    let why = closed
        .note()
        .unwrap_or_else(|| "This server no longer offers it.".to_string());
    format!(
        "This Mac can no longer be made one device here. {why} Nothing was installed, and the \
         token is not spent: go back to the first page, which reads this Mac again."
    )
}

/// R3 on this host: the person running the wizard.
fn person_config_here() -> PersonConfig {
    #[cfg(unix)]
    {
        match roomlerd::appdirs::own_home() {
            Some(home) => person_config_in(&home),
            None => PersonConfig::Unreadable,
        }
    }
    #[cfg(not(unix))]
    {
        PersonConfig::Unreadable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mac() -> Facts {
        Facts {
            os: HostOs::Macos,
            switch_on: true,
            root_config: false,
            person: PersonConfig::NotEnrolled,
            release: Release::Tag("agent-v0.4.229".into()),
        }
    }

    /// Open only on a Mac with the switch on, enrolled nowhere, on a release
    /// that knows one row; each closing fact says which it was.
    #[test]
    fn the_form_is_open_only_on_a_mac_enrolled_nowhere() {
        assert_eq!(decide(&open_mac()), Ok(()));
        for os in [HostOs::Linux, HostOs::Windows, HostOs::Other] {
            let f = Facts { os, ..open_mac() };
            assert_eq!(decide(&f), Err(Closed::NotOffered), "{os:?}");
        }
        let off = Facts {
            switch_on: false,
            ..open_mac()
        };
        assert_eq!(decide(&off), Err(Closed::NotOffered));
        assert_eq!(
            Closed::NotOffered.note(),
            None,
            "the switch off changes nothing"
        );
        let root = Facts {
            root_config: true,
            ..open_mac()
        };
        assert_eq!(decide(&root), Err(Closed::RootEnrolled));
        for person in [PersonConfig::Enrolled, PersonConfig::Unreadable] {
            let f = Facts {
                person,
                ..open_mac()
            };
            assert_eq!(decide(&f), Err(Closed::PersonEnrolled), "{person:?}");
        }
    }

    /// D4-A — a pure version compare: the first one-row release and later
    /// open it; an older one, a pre-release of the first, a tag the updater
    /// cannot read, or no answer close it.
    #[test]
    fn the_release_must_know_one_row() {
        for tag in [
            "agent-v0.4.201",
            "agent-v0.4.229",
            "agent-v0.5.0",
            "agent-v1.0.0",
        ] {
            assert!(release_knows_one_row(tag), "{tag}");
        }
        for tag in [
            "agent-v0.4.200",
            "agent-v0.4.201-rc.1",
            "agent-v0.3.99",
            "nonsense",
            "",
        ] {
            assert!(!release_knows_one_row(tag), "{tag}");
        }
        let old = Facts {
            release: Release::Tag("agent-v0.4.200".into()),
            ..open_mac()
        };
        assert_eq!(
            decide(&old),
            Err(Closed::ReleaseTooOld("agent-v0.4.200".into()))
        );
        let unknown = Facts {
            release: Release::Unknown,
            ..open_mac()
        };
        assert_eq!(decide(&unknown), Err(Closed::ReleaseUnknown));
    }

    /// An enrolled Mac is offered the person's own command, never converted
    /// (rule 8, D3-A), and no note of a closed form calls this Mac one device.
    #[test]
    fn an_enrolled_mac_is_told_its_own_road() {
        let root = Closed::RootEnrolled.note().unwrap();
        assert!(root.contains("sudo roomlerd one-row"), "{root}");
        assert!(root.contains("wait five minutes"), "#4097: {root}");
        let person = Closed::PersonEnrolled.note().unwrap();
        assert!(person.contains("--one-row and a new token"), "{person}");
        assert!(
            !person.contains("sudo roomlerd one-row"),
            "needs root's file: {person}"
        );
        for closed in [
            Closed::ReleaseTooOld("agent-v0.4.200".into()),
            Closed::ReleaseUnknown,
        ] {
            let said = closed.note().unwrap();
            assert!(said.contains("two"), "{said}");
        }
        // The install's refusal when the form closed since it was read: the
        // reason, and that nothing was spent.
        let off = closed_since_read(&Closed::NotOffered);
        assert!(off.contains("no longer offers it"), "{off}");
        let root = closed_since_read(&Closed::RootEnrolled);
        assert!(root.contains("sudo roomlerd one-row"), "{root}");
        for said in [off, root] {
            assert!(said.contains("the token is not spent"), "{said}");
        }
    }

    /// R3 reads a device in the primary or in any org, a blank config (a
    /// one-row worker's) as none, and a config that is not TOML as unreadable.
    #[test]
    fn the_persons_config_names_a_device_or_not() {
        assert_eq!(
            person_config_text("agent_id = \"a1\"\nserver_url = \"https://x\"\n"),
            PersonConfig::Enrolled
        );
        assert_eq!(
            person_config_text(
                "agent_id = \"\"\n\n[[orgs]]\nagent_id = \"o1\"\ntenant_id = \"t\"\n"
            ),
            PersonConfig::Enrolled,
            "an org's device"
        );
        assert_eq!(person_config_text(""), PersonConfig::NotEnrolled, "blank");
        assert_eq!(
            person_config_text("agent_id = \"  \"\nmacos_one_row = false\n"),
            PersonConfig::NotEnrolled
        );
        assert_eq!(
            person_config_text("agent_id = \"a1\" garbage ["),
            PersonConfig::Unreadable
        );
    }

    /// R3 over both folder trees: none, the current tree's, the pre-rename
    /// tree's, and an unreadable one.
    #[test]
    fn the_persons_config_is_read_in_both_trees() {
        use roomlerd::appdirs::macos_config_dir_in_home;
        let home = tempfile::tempdir().unwrap();
        let current = macos_config_dir_in_home(home.path(), |_| false);
        let before = macos_config_dir_in_home(home.path(), |p| p != current);
        assert_ne!(current, before);
        assert_eq!(person_config_in(home.path()), PersonConfig::NotEnrolled);

        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("config.toml"), "").unwrap();
        assert_eq!(person_config_in(home.path()), PersonConfig::NotEnrolled);

        std::fs::create_dir_all(&before).unwrap();
        std::fs::write(before.join("config.toml"), "agent_id = \"old\"\n").unwrap();
        assert_eq!(person_config_in(home.path()), PersonConfig::Enrolled);

        std::fs::write(before.join("config.toml"), "not = [toml").unwrap();
        assert_eq!(person_config_in(home.path()), PersonConfig::Unreadable);
    }

    /// FR-141 P2's follow-up (F5) — R1 counts root's config as absent only
    /// when there is no such file. Any other answer is a Mac it cannot tell
    /// about, which closes the form, as R3 reads a person's config it cannot
    /// read: before, a folder this person may not search read as "not
    /// enrolled", and the token was spent on an enrolled Mac. Unix only: the
    /// error below is the one a Mac gives (a path through a file, ENOTDIR,
    /// which root gets too), and Windows gives "not found" for it.
    #[cfg(unix)]
    #[test]
    fn r1_reads_an_error_as_enrolled() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        assert!(!root_config_seen(&config), "no such file: absent");
        std::fs::write(&config, "agent_id = \"root\"\n").unwrap();
        assert!(root_config_seen(&config), "there");
        let through_a_file = config.join("config.toml");
        let err = std::fs::metadata(&through_a_file).expect_err("not a folder");
        assert_ne!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
        assert!(root_config_seen(&through_a_file), "an error: enrolled");
    }

    /// The release is the first in the server's order with the macOS package,
    /// as the install half picks it; anything else is unknown.
    #[test]
    fn the_release_is_the_one_the_install_half_takes() {
        let body = br#"[
            {"tag_name": "agent-v0.4.231", "assets": [
                {"name": "roomlerd-0.4.231-aarch64-apple-darwin.pkg"}]},
            {"tag_name": "agent-v0.4.230", "assets": [
                {"name": "roomlerd-0.4.230-x86_64-unknown-linux-gnu.deb", "browser_download_url": "https://x/a"},
                {"name": "roomlerd-0.4.230-aarch64-apple-darwin.pkg.sha256", "browser_download_url": "https://x/b"}]},
            {"tag_name": "agent-v0.4.229", "assets": [
                {"name": "roomlerd-0.4.229-aarch64-apple-darwin.pkg", "browser_download_url": "https://x/c"}]},
            {"tag_name": "agent-v0.4.228", "assets": [
                {"name": "roomlerd-0.4.228-aarch64-apple-darwin.pkg", "browser_download_url": "https://x/d"}]}
        ]"#;
        assert_eq!(
            release_in(body),
            Release::Tag("agent-v0.4.229".into()),
            "no URL, a .sha256 and a Linux package are not the Mac's package"
        );
        assert_eq!(release_in(b"[]"), Release::Unknown);
        assert_eq!(
            release_in(b"{\"tag_name\": \"agent-v0.4.229\"}"),
            Release::Unknown
        );
        assert_eq!(release_in(b"<html>"), Release::Unknown);
    }
}
