// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3609 — who writes a daemon install's config at step 7, and how the
//! background-service role's config reaches the machine.
//!
//! The background-service role's config is the machine's: the service runs it
//! as SYSTEM, and only while SYSTEM and Administrators alone may change it
//! (`roomler_node_core::machine_tree`). So a process running as an
//! administrator writes it, and only such a process:
//!
//! - an elevated wizard writes it itself, as it always has;
//! - a limited wizard has already crossed to the administrator once, for the
//!   step that runs the installer ([`crate::package_step`]). On this role that
//!   step stays after the installer and writes the config: the wizard enrolls
//!   (the network call, which spends the token) and hands the step what the
//!   server issued, over `wizard_shared::step_channel`. One prompt, as before,
//!   and nothing on the checklist moves.
//!
//! ```text
//! wizard (limited)                               step (administrator)
//!   channel made ── runas … --system-context 1 ──> stage, check, msiexec
//!                    <── msi {code}                 (0 or 3010; otherwise it exits)
//!   settle a reinstall's daemon
//!   prepare ──>                                     the parent taken and sealed; the
//!                    <── prepared | failed          config to fold into read; its
//!                                                   path probed
//!   enroll (network) → issued
//!   write {issued, advanced} ──>                    fold, a fresh install's defaults,
//!                    <── written | failed           the boxes, the folder taken, saved
//! ```
//!
//! What crosses is the server's answer and the Advanced options, nothing else.
//! The step builds the config itself (`Issued::into_config`, the one `enroll`
//! writes), folds it into the machine config it read itself, and writes only
//! `machine_global_config_path()`. Every other install (an elevated wizard, the
//! per-user and plain machine-wide roles) runs the same two functions,
//! [`prepare`] and [`write`], in its own process.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(target_os = "windows")]
use std::time::Duration;

use roomler_ai_remote_control::setup_diagnostics::ErrorCode;
use roomlerd::config::AgentConfig;
use roomlerd::enrollment::Issued;
use roomlerd::updater::WindowsInstallFlavour;
use serde::{Deserialize, Serialize};

use crate::orchestrator_agent::AdvancedOptions;

/// The wizard's requests to the step, in this order, once each.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Step 7 up to the enroll ([`prepare`]).
    Prepare,
    /// The rest of it, once the enroll is done ([`write`]).
    Write {
        issued: Box<Issued>,
        advanced: AdvancedOptions,
    },
}

/// The step's words to the wizard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Answer {
    /// The installer's exit code: the step's first line, once it has run.
    Msi {
        code: i32,
    },
    Prepared(Prepared),
    Written(Written),
    Failed(Failure),
}

/// What [`prepare`] found, for the wizard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    /// The machine id of the config the enrollment folds into, so the server
    /// keeps its device row. `None`: derived from the path, as on a first
    /// install.
    pub machine_id: Option<String>,
    /// A config was there to fold into: a reinstall.
    pub had_config: bool,
    /// For the person, in order.
    pub warnings: Vec<String>,
}

/// What [`write`] did, for the wizard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Written {
    /// The device the enrollment landed on (`enrollment::enrolled_identity`).
    pub agent_id: String,
    pub tenant_id: String,
    /// It is another organization's, beside the primary.
    pub added_org: bool,
    /// For the person, in order.
    pub warnings: Vec<String>,
}

/// Why step 7 stopped, with the words for the person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
}

/// Which diagnostics code a [`Failure`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    ConfigWrite,
    AdvancedOptions,
    Enroll,
}

impl Failure {
    fn config(message: String) -> Self {
        Self {
            kind: FailureKind::ConfigWrite,
            message,
        }
    }

    /// Record it in the install's diagnostics, as the failure of the step that
    /// is open (#3432). Its words crossed the administrator step's channel as
    /// text, so they are the step's own: a refusal in those words.
    pub fn fail(self) -> crate::failure::Failure {
        let code = match self.kind {
            FailureKind::ConfigWrite => ErrorCode::ConfigWrite,
            FailureKind::AdvancedOptions => ErrorCode::AdvancedOptions,
            FailureKind::Enroll => ErrorCode::EnrollRejected,
        };
        crate::steps::Steps::global().fail(code, crate::failure::Said(self.message))
    }
}

/// What [`prepare`] read, kept for [`write`].
pub struct Found {
    pub prepared: Prepared,
    existing: Option<AgentConfig>,
}

/// Step 7 up to the enroll, for the config at `config_path`: the config to fold
/// into, read (`enroll_step::read_existing`, which on the machine location
/// repairs and checks it first), and the path probed (`config::probe_writable`).
/// A config this process may not read, or a path it may not write, stops the
/// install here, before the enroll spends the token (#3116).
pub fn prepare(config_path: &Path) -> Result<Found, Failure> {
    use crate::orchestrator_agent::{NOTHING_SPENT, RUN_AS_ADMINISTRATOR, config_unwritable};
    let existing = crate::enroll_step::read_existing(config_path).map_err(|said| {
        Failure::config(format!("{said} {RUN_AS_ADMINISTRATOR} {NOTHING_SPENT}"))
    })?;
    if let Err(e) = roomlerd::config::probe_writable(config_path) {
        return Err(Failure::config(config_unwritable(config_path, &e)));
    }
    Ok(Found {
        prepared: Prepared {
            machine_id: existing.cfg.as_ref().map(|cfg| cfg.machine_id.clone()),
            had_config: existing.cfg.is_some(),
            warnings: existing.warning.into_iter().collect(),
        },
        existing: existing.cfg,
    })
}

/// The rest of step 7, once the enroll is done: `issued` folded into the config
/// [`prepare`] read (#2338: a reinstall keeps the device's settings), a fresh
/// install's defaults and the boxes the person touched (`enroll_step::finish`),
/// and on the background-service role the machine folder taken for
/// Administrators, before the config is saved at `config_path`.
pub fn write(
    config_path: &Path,
    existing: Option<AgentConfig>,
    issued: Issued,
    advanced: &AdvancedOptions,
    is_system_context: bool,
    flavour: WindowsInstallFlavour,
) -> Result<Written, Failure> {
    let crate::enroll_step::Enrolled {
        mut cfg,
        outcome,
        previous_row,
    } = crate::enroll_step::fold(existing, issued.into_config()).map_err(|e| Failure {
        kind: FailureKind::Enroll,
        message: format!("enrollment: {e}"),
    })?;
    let mut warnings = Vec::new();
    // F3: the machine id was read back from the config, so the server reuses
    // the row — unless it no longer can; then the old key is retired and said.
    if let Some(row) = &previous_row {
        warnings.push(roomlerd::enrollment::describe_previous_row(
            row,
            &cfg.agent_id,
        ));
    }
    let note = crate::enroll_step::finish(
        &mut cfg,
        &outcome,
        is_system_context,
        advanced,
        crate::orchestrator_agent::overlay_enabled_default(None, flavour),
    )
    .map_err(|message| Failure {
        kind: FailureKind::AdvancedOptions,
        message,
    })?;
    warnings.extend(note);
    // The device this install enrolled: on an org added beside the primary,
    // that org's row, never the primary's (the config's top-level ids).
    let (agent_id, tenant_id) = roomlerd::enrollment::enrolled_identity(&cfg, &outcome);
    let added_org = roomlerd::enrollment::is_added_org(&outcome);
    // #3609 — the machine folder is taken for Administrators first, the owner
    // and nothing else, so the save's own repair seals it: the repair never
    // takes over a folder another account made (a person's daemon stages
    // received files below it). This runs as an administrator: an elevated
    // wizard, or the step. Best-effort, said if it fails.
    #[cfg(target_os = "windows")]
    if is_system_context && let Err(e) = roomlerd::config::take_machine_folder(config_path) {
        warnings.push(format!(
            "could not take the machine-global config folder for Administrators ({e}); \
             until it belongs to Administrators the service may not run this \
             enrollment as SYSTEM"
        ));
    }
    roomlerd::config::save(&config_path.to_path_buf(), &cfg)
        .map_err(|e| Failure::config(format!("write config.toml: {e}")))?;
    Ok(Written {
        agent_id,
        tenant_id,
        added_org,
        warnings,
    })
}

/// The step's end of the channel, connected to the step and checked
/// (Windows). Nowhere else is there a step.
#[cfg(target_os = "windows")]
pub type StepLink = wizard_shared::step_channel::Link;
/// The step's end of the channel: there is no step off Windows.
#[cfg(not(target_os = "windows"))]
pub enum StepLink {}

/// Who writes step 7's config, from the wizard's side.
pub enum ConfigWriter {
    /// This process: an elevated wizard, or a role whose config is the
    /// person's own.
    Local {
        config_path: PathBuf,
        is_system_context: bool,
        existing: Option<Box<AgentConfig>>,
    },
    /// The administrator step, over its channel: a limited wizard's
    /// background-service install.
    Step(StepLink),
}

/// What a limited wizard says when its background-service install has no
/// step to write the machine enrollment: it never writes it itself.
const NO_STEP: &str = "Roomler was installed, but its administrator step ended before it could \
     write this device's enrollment, which on this role only an administrator's process may write. \
     Run Roomler Setup again: nothing was enrolled, and the enrollment token is still unused.";

/// How long one exchange with the step may take: the parent's seal (two
/// `icacls` runs, each up to 20 s on a host busy with higher-priority work,
/// #3291) and a few local reads and writes.
#[cfg(target_os = "windows")]
const EXCHANGE: Duration = Duration::from_secs(180);

impl ConfigWriter {
    /// The writer for an install whose installer step handed back `link` (a
    /// limited wizard's background-service install), or none. A limited wizard
    /// never writes the background-service role's config itself: without the
    /// step's channel that install stops here.
    pub fn new(
        link: Option<StepLink>,
        config_path: &Path,
        is_system_context: bool,
        wizard_elevated: bool,
    ) -> Result<Self, Failure> {
        if let Some(link) = link {
            return Ok(Self::Step(link));
        }
        if is_system_context && !wizard_elevated {
            return Err(Failure::config(NO_STEP.to_string()));
        }
        Ok(Self::Local {
            config_path: config_path.to_path_buf(),
            is_system_context,
            existing: None,
        })
    }

    /// The config is written in this process.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    /// [`prepare`], here or in the step.
    pub async fn prepare(&mut self) -> Result<Prepared, Failure> {
        match self {
            Self::Local {
                config_path,
                existing,
                ..
            } => {
                let found = prepare(config_path)?;
                *existing = found.existing.map(Box::new);
                Ok(found.prepared)
            }
            #[cfg(target_os = "windows")]
            Self::Step(link) => match exchange(link, &Request::Prepare).await {
                Ok(Answer::Prepared(prepared)) => Ok(prepared),
                Ok(Answer::Failed(failure)) => Err(failure),
                Ok(other) => Err(Failure::config(format!(
                    "the administrator step answered {other:?} where it reads the config. {}",
                    crate::orchestrator_agent::NOTHING_SPENT
                ))),
                Err(e) => Err(Failure::config(format!(
                    "the administrator step ended before it read this device's config ({e}). {}",
                    crate::orchestrator_agent::NOTHING_SPENT
                ))),
            },
            #[cfg(not(target_os = "windows"))]
            Self::Step(never) => match *never {},
        }
    }

    /// [`write`], here or in the step. `flavour` is the role's; the step's is
    /// always the machine-wide one.
    pub async fn write(
        &mut self,
        issued: Issued,
        advanced: &AdvancedOptions,
        flavour: WindowsInstallFlavour,
    ) -> Result<Written, Failure> {
        match self {
            Self::Local {
                config_path,
                is_system_context,
                existing,
            } => write(
                config_path,
                existing.take().map(|cfg| *cfg),
                issued,
                advanced,
                *is_system_context,
                flavour,
            ),
            #[cfg(target_os = "windows")]
            Self::Step(link) => {
                let _ = flavour;
                let request = Request::Write {
                    issued: Box::new(issued),
                    advanced: advanced.clone(),
                };
                match exchange(link, &request).await {
                    Ok(Answer::Written(written)) => Ok(written),
                    Ok(Answer::Failed(failure)) => Err(failure),
                    Ok(other) => Err(Failure::config(format!(
                        "the administrator step answered {other:?} where it writes the config"
                    ))),
                    Err(e) => Err(Failure::config(format!(
                        "the administrator step ended before it said it wrote this device's \
                         config ({e})"
                    ))),
                }
            }
            #[cfg(not(target_os = "windows"))]
            Self::Step(never) => match *never {},
        }
    }
}

/// One request to the step and its answer, within [`EXCHANGE`].
#[cfg(target_os = "windows")]
async fn exchange(link: &mut StepLink, request: &Request) -> std::io::Result<Answer> {
    let line = serde_json::to_string(request).map_err(std::io::Error::other)?;
    link.send(&line).await?;
    let answer = tokio::time::timeout(EXCHANGE, link.recv())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer within {} s", EXCHANGE.as_secs()),
            )
        })??;
    serde_json::from_str(&answer)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// rc.52, #3609: `%ProgramData%\roomler`, the parent of the machine config at
/// `config_path`, as the process that writes that config leaves it (an
/// elevated wizard, or the step): taken for Administrators first, by handle
/// (`config::take_machine_parent`: a person's daemon may have made it), then
/// its inheritance replaced by SYSTEM and Administrators
/// (`orchestrator_agent::harden_machine_global_dir`). A parent that is not a
/// plain folder is left for the service's check to refuse, never sealed
/// through. The warnings, for the person.
#[cfg(target_os = "windows")]
pub(crate) fn seal_parent(config_path: &Path) -> Vec<String> {
    if let Err(e) = roomlerd::config::take_machine_parent(config_path) {
        return vec![format!(
            "could not take the machine-global config directory's parent for Administrators ({e})"
        )];
    }
    match crate::orchestrator_agent::harden_machine_global_dir(config_path) {
        Ok(()) => Vec::new(),
        Err(e) => vec![format!(
            "could not restrict permissions on the machine-global config directory ({e})"
        )],
    }
}

/// How long the step waits, once the installer ran, for the wizard to finish
/// the enroll and hand it over: a reinstall's daemon settling (90 s), the
/// enroll's network call, and room to spare.
#[cfg(target_os = "windows")]
const STEP_BUDGET: Duration = Duration::from_secs(20 * 60);

/// The step's side once the installer ran (`code`, 0 or 3010) on the
/// background-service role: reach the wizard's channel for `stage`, say the
/// installer ran, and serve its two requests for the machine config
/// (`machine_global_config_path`, never a path the wizard names). Its exit
/// code: `code` once the config is written, otherwise one of
/// `package_step::exit_code`'s codes for after the install. A watchdog ends the
/// step after [`STEP_BUDGET`], never in the middle of the write.
#[cfg(target_os = "windows")]
pub fn hand_over(stage: &str, code: i32) -> i32 {
    use crate::package_step::exit_code;
    static GATE: Mutex<()> = Mutex::new(());
    let name = wizard_shared::step_channel::pipe_name(stage);
    let Ok(pipe) = wizard_shared::step_channel::connect(&name) else {
        return exit_code::NO_WIZARD;
    };
    let Ok(read_half) = pipe.try_clone() else {
        return exit_code::NO_WIZARD;
    };
    std::thread::spawn(|| {
        std::thread::sleep(STEP_BUDGET);
        let _gate = GATE.lock().unwrap_or_else(|e| e.into_inner());
        std::process::exit(exit_code::TIMED_OUT);
    });
    let mut writer = pipe;
    if send(&mut writer, &Answer::Msi { code }).is_err() {
        return exit_code::NO_WIZARD;
    }
    let mut reader = std::io::BufReader::new(read_half);
    let config_path = roomlerd::config::machine_global_config_path();
    match serve(&mut reader, &mut writer, &config_path, &GATE) {
        Served::Written => code,
        Served::Failed => exit_code::WRITE_FAILED,
        Served::HungUp | Served::Refused(_) => exit_code::NOT_WRITTEN,
    }
}

/// The step's first word, from the wizard's side: wait for process `pid`, the
/// step, to connect, and read the installer's exit code.
#[cfg(target_os = "windows")]
pub async fn accept_install(
    listener: wizard_shared::step_channel::Listener,
    pid: u32,
) -> std::io::Result<(i32, StepLink)> {
    let mut link = listener.accept(pid).await?;
    let line = link.recv().await?;
    match serde_json::from_str::<Answer>(&line) {
        Ok(Answer::Msi { code }) => Ok((code, link)),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the step's first word was not the installer's exit code",
        )),
    }
}

/// How the step's side of the conversation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// The config is written.
    Written,
    /// Step 7 stopped, and the wizard was told why.
    Failed,
    /// The wizard hung up before it asked for the write.
    HungUp,
    /// The wizard asked for something out of turn, or said something that is
    /// not a request: nothing is written.
    Refused(String),
}

/// The step's side, once the installer has run and `Answer::Msi` is sent: the
/// wizard's two requests, in order, for the config at `config_path`, which is
/// the machine's (`hand_over`) and never one the wizard names. The write runs
/// under `gate`, which the step's watchdog takes before it ends the process,
/// so the process never ends in the middle of one.
pub fn serve<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    config_path: &Path,
    gate: &Mutex<()>,
) -> Served {
    match next_request(reader) {
        Ok(Request::Prepare) => {}
        Ok(Request::Write { .. }) => {
            return Served::Refused("a write before the config was read".into());
        }
        Err(end) => return end,
    }
    let found = match prepare(config_path) {
        Ok(found) => found,
        Err(failure) => {
            let _ = send(writer, &Answer::Failed(failure));
            return Served::Failed;
        }
    };
    #[allow(unused_mut)]
    let mut prepared = found.prepared;
    // The parent taken and sealed, as an elevated wizard does, here and only
    // for the machine's own config. #3666: only once the config was read as
    // found (`prepare`): sealed first, a parent another account made would
    // read as the machine's, and a sealed folder moved into it would be folded.
    #[cfg(target_os = "windows")]
    if config_path == roomlerd::config::machine_global_config_path().as_path() {
        prepared.warnings.splice(0..0, seal_parent(config_path));
    }
    if send(writer, &Answer::Prepared(prepared)).is_err() {
        return Served::HungUp;
    }
    let (issued, advanced) = match next_request(reader) {
        Ok(Request::Write { issued, advanced }) => (*issued, advanced),
        Ok(Request::Prepare) => return Served::Refused("the config was read already".into()),
        Err(end) => return end,
    };
    let issued = match fit_issued(issued).and_then(|issued| {
        advanced
            .validate()
            .map(|()| issued)
            .map_err(|e| format!("the step refused the options: {e}"))
    }) {
        Ok(issued) => issued,
        Err(message) => {
            let _ = send(writer, &Answer::Failed(Failure::config(message)));
            return Served::Failed;
        }
    };
    let written = {
        let _gate = gate.lock().unwrap_or_else(|e| e.into_inner());
        write(
            config_path,
            found.existing,
            issued,
            &advanced,
            true,
            WindowsInstallFlavour::PerMachine,
        )
    };
    match written {
        Ok(written) => {
            let _ = send(writer, &Answer::Written(written));
            Served::Written
        }
        Err(failure) => {
            let _ = send(writer, &Answer::Failed(failure));
            Served::Failed
        }
    }
}

/// The wizard's next request, or how the conversation ended instead.
fn next_request<R: BufRead>(reader: &mut R) -> Result<Request, Served> {
    match wizard_shared::step_channel::read_line(reader) {
        Ok(line) => serde_json::from_str(&line)
            .map_err(|e| Served::Refused(format!("not a request the step reads ({e})"))),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Served::HungUp),
        Err(e) => Err(Served::Refused(e.to_string())),
    }
}

fn send<W: Write>(writer: &mut W, answer: &Answer) -> std::io::Result<()> {
    let line = serde_json::to_string(answer).map_err(std::io::Error::other)?;
    wizard_shared::step_channel::write_line(writer, &line)
}

/// What the wizard handed over, as the step takes it: each value of the shape
/// an enrollment gives it, and the server reached over TLS, as `enroll` posted
/// to it (a loopback one excepted). Anything else is refused, naming the value.
fn fit_issued(mut issued: Issued) -> Result<Issued, String> {
    fn plain(value: &str, max: usize) -> bool {
        !value.is_empty()
            && value.len() <= max
            && !value.chars().any(|c| c.is_control() || c.is_whitespace())
    }
    let refused = |what: &str| Err(format!("the step refused the enrollment's {what}"));
    if crate::launch::safe_server(&issued.server_url).is_none() {
        return refused("server");
    }
    issued.server_url = roomler_ai_remote_control::server_url::secure(&issued.server_url, false)
        .url
        .trim_end_matches('/')
        .to_string();
    for (what, value) in [
        ("device id", &issued.agent_id),
        ("organization id", &issued.tenant_id),
    ] {
        if !plain(value, 64) || !value.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return refused(what);
        }
    }
    if !plain(&issued.agent_token, 16 * 1024) {
        return refused("token");
    }
    if !plain(&issued.machine_id, 256) {
        return refused("machine id");
    }
    let name = &issued.machine_name;
    if name.trim().is_empty() || name.len() > 1024 || name.chars().any(char::is_control) {
        return refused("device name");
    }
    Ok(issued)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::models::LockOnEnd;

    pub(super) fn issued() -> Issued {
        Issued {
            server_url: "https://roomler.ai".into(),
            agent_id: "6af285450d1b34b5d32ea7c1".into(),
            tenant_id: "6a975e17971e2d55ddcdedb2".into(),
            agent_token: "header.payload.signature".into(),
            machine_id: "f00dfeed".into(),
            machine_name: "Front desk".into(),
            ephemeral: false,
        }
    }

    pub(super) fn advanced() -> AdvancedOptions {
        AdvancedOptions {
            overlay_enabled: None,
            advertise_local_subnets: Some(true),
            auto_grant_session: Some(false),
            overlay_advertised_routes: None,
            advertise_routes: None,
            touched: vec!["auto_grant_session".into()],
        }
    }

    fn line(request: &Request) -> String {
        format!("{}\n", serde_json::to_string(request).unwrap())
    }

    fn answers(sent: &[u8]) -> Vec<Answer> {
        String::from_utf8_lossy(sent)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// A config path in a folder of its own, never the machine's.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("roomler")
            .join("roomler")
            .join("config.toml");
        (dir, path)
    }

    /// The wire: a request and an answer read back as sent, and a request
    /// carrying anything more than its own fields is not one.
    #[test]
    fn the_wire_reads_back_and_takes_nothing_more() {
        let sent = serde_json::to_string(&Request::Prepare).unwrap();
        assert_eq!(sent, r#""prepare""#);
        let write = serde_json::to_string(&Request::Write {
            issued: Box::new(issued()),
            advanced: advanced(),
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_str::<Request>(&write).unwrap(),
            Request::Write { .. }
        ));
        let extra = write.replacen(
            r#""ephemeral":false"#,
            r#""ephemeral":false,"exec_enabled":true"#,
            1,
        );
        assert_ne!(extra, write, "the fixture holds the key");
        assert!(serde_json::from_str::<Request>(&extra).is_err());
        let path = write.replacen(r#"{"write":{"#, r#"{"write":{"config_path":"C:\\x","#, 1);
        assert!(
            serde_json::from_str::<Request>(&path).is_err(),
            "no path on the wire"
        );
        for answer in [
            Answer::Msi { code: 3010 },
            Answer::Prepared(Prepared {
                machine_id: Some("m".into()),
                had_config: true,
                warnings: vec!["w".into()],
            }),
            Answer::Written(Written::default()),
            Answer::Failed(Failure::config("no".into())),
        ] {
            let text = serde_json::to_string(&answer).unwrap();
            assert_eq!(serde_json::from_str::<Answer>(&text).unwrap(), answer);
        }
    }

    /// The step writes nothing unless it is asked to read first and to write
    /// second, and a wizard that hangs up in between leaves nothing written.
    #[test]
    fn the_step_writes_only_when_asked_in_turn() {
        let gate = Mutex::new(());
        let (_dir, path) = scratch();
        let write = line(&Request::Write {
            issued: Box::new(issued()),
            advanced: advanced(),
        });

        let mut out = Vec::new();
        let served = serve(&mut write.as_bytes(), &mut out, &path, &gate);
        assert!(matches!(served, Served::Refused(_)), "{served:?}");
        assert!(out.is_empty() && !path.exists());

        let mut out = Vec::new();
        let asked = line(&Request::Prepare);
        let served = serve(&mut asked.as_bytes(), &mut out, &path, &gate);
        assert_eq!(served, Served::HungUp);
        assert!(matches!(answers(&out)[..], [Answer::Prepared(_)]));
        assert!(!path.exists(), "nothing written");

        let twice = format!("{asked}{asked}");
        let served = serve(&mut twice.as_bytes(), &mut Vec::new(), &path, &gate);
        assert!(matches!(served, Served::Refused(_)), "{served:?}");
        assert!(!path.exists());

        let junk = format!("{asked}{{\"write\":42}}\n");
        let served = serve(&mut junk.as_bytes(), &mut Vec::new(), &path, &gate);
        assert!(matches!(served, Served::Refused(_)), "{served:?}");
        assert!(!path.exists());
    }

    /// Asked in turn, the step folds the enrollment into a fresh config with
    /// the background service's defaults and the boxes, and says which device
    /// it wrote.
    #[test]
    fn the_step_writes_what_the_server_issued_with_the_services_defaults() {
        let gate = Mutex::new(());
        let (_dir, path) = scratch();
        let talk = format!(
            "{}{}",
            line(&Request::Prepare),
            line(&Request::Write {
                issued: Box::new(issued()),
                advanced: advanced(),
            })
        );
        let mut out = Vec::new();
        assert_eq!(
            serve(&mut talk.as_bytes(), &mut out, &path, &gate),
            Served::Written
        );
        let said = answers(&out);
        assert!(
            matches!(&said[0], Answer::Prepared(p) if !p.had_config && p.machine_id.is_none()),
            "{said:?}"
        );
        let Answer::Written(written) = &said[1] else {
            panic!("{said:?}");
        };
        assert_eq!(written.agent_id, "6af285450d1b34b5d32ea7c1");
        assert_eq!(written.tenant_id, "6a975e17971e2d55ddcdedb2");
        assert!(!written.added_org);
        let cfg = roomlerd::config::load(&path).unwrap();
        assert_eq!(cfg.agent_token, "header.payload.signature");
        assert_eq!(cfg.machine_name, "Front desk");
        assert_eq!(cfg.lock_on_end, LockOnEnd::Auto, "the service's default");
        assert!(cfg.overlay_enabled, "the machine roles' default");
        assert!(!cfg.auto_grant_session, "the box as the person set it");
        assert!(!cfg.exec_enabled, "an enrollment's own default");
    }

    /// What the step takes from the wizard is the shape of an enrollment, and
    /// an option the Server page could not have sent is refused before
    /// anything is written.
    #[test]
    fn the_step_refuses_what_no_enrollment_gives() {
        let gate = Mutex::new(());
        let refused = |issued: Issued, advanced: AdvancedOptions| {
            let (_dir, path) = scratch();
            let talk = format!(
                "{}{}",
                line(&Request::Prepare),
                line(&Request::Write {
                    issued: Box::new(issued),
                    advanced,
                })
            );
            let mut out = Vec::new();
            let served = serve(&mut talk.as_bytes(), &mut out, &path, &gate);
            let failed = matches!(answers(&out).last(), Some(Answer::Failed(_)));
            served == Served::Failed && failed && !path.exists()
        };
        let with = |edit: fn(&mut Issued)| {
            let mut i = issued();
            edit(&mut i);
            i
        };
        assert!(refused(
            with(|i| i.server_url = "ftp://x".into()),
            advanced()
        ));
        assert!(refused(
            with(|i| i.server_url = "https://a b".into()),
            advanced()
        ));
        assert!(refused(with(|i| i.agent_id = "../x".into()), advanced()));
        assert!(refused(with(|i| i.tenant_id = String::new()), advanced()));
        assert!(refused(with(|i| i.agent_token = "a\nb".into()), advanced()));
        assert!(refused(with(|i| i.machine_id = "a b".into()), advanced()));
        assert!(refused(
            with(|i| i.machine_name = "a\u{7}".into()),
            advanced()
        ));
        let mut bad_route = advanced();
        bad_route.advertise_routes = Some("10.0.0.0/33".into());
        assert!(refused(issued(), bad_route));

        // Plaintext to a remote server is not what `enroll` posted to.
        let gate = Mutex::new(());
        let (_dir, path) = scratch();
        let talk = format!(
            "{}{}",
            line(&Request::Prepare),
            line(&Request::Write {
                issued: Box::new(with(|i| i.server_url = "http://roomler.ai/".into())),
                advanced: advanced(),
            })
        );
        assert_eq!(
            serve(&mut talk.as_bytes(), &mut Vec::new(), &path, &gate),
            Served::Written
        );
        let cfg = roomlerd::config::load(&path).unwrap();
        assert_eq!(cfg.server_url, "https://roomler.ai");
    }

    /// Who writes: a limited wizard's background-service install goes through
    /// its step and never writes the config itself; everything else writes in
    /// this process.
    #[test]
    fn a_limited_wizard_never_writes_the_machine_config_itself() {
        let (_dir, path) = scratch();
        assert!(
            ConfigWriter::new(None, &path, true, false).is_err(),
            "no step, no write"
        );
        assert!(
            ConfigWriter::new(None, &path, true, true)
                .unwrap()
                .is_local()
        );
        assert!(
            ConfigWriter::new(None, &path, false, false)
                .unwrap()
                .is_local()
        );
        assert!(
            ConfigWriter::new(None, &path, false, true)
                .unwrap()
                .is_local()
        );
    }

    #[cfg(target_os = "windows")]
    mod on_ntfs {
        use super::*;
        use roomler_node_core::machine_tree::test_support::{
            USERS, elevated_or_skip, icacls, local_guest, names, owner, sddl, sealed, set_dacl,
        };
        use roomler_node_core::machine_tree::{self, MachineTree};

        const ROOT: &str = "ROOMLER_TEST_MACHINE_WRITE_ROOT";
        const THIS_TEST: &str = "machine_write::tests::on_ntfs::a_limited_setups_machine_enrollment_is_owned_by_administrators_and_sealed";
        const PERSON_TEST: &str = "machine_write::tests::on_ntfs::a_limited_setup_on_a_tree_a_person_made_comes_out_the_same";
        const PLANTED_TEST: &str = "machine_write::tests::on_ntfs::a_limited_setup_never_folds_into_a_folder_moved_into_a_parent_a_person_made";
        const CHECKED: &str = "MACHINE WRITE CHECKED";

        /// `%ProgramData%`'s own DACL: SYSTEM and Administrators full control,
        /// full control for whoever creates an entry below it (CREATOR OWNER),
        /// Users read and execute, and Users' right to add files and folders.
        const PROGRAM_DATA: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICIIO;FA;;;CO)(A;OICI;0x1200a9;;;BU)(A;CI;0x116;;;BU)";

        /// The field's case (a fresh install of the background-service role
        /// from a Setup a person started, an administrator at its one prompt):
        /// the machine config comes out Administrators', SYSTEM's and
        /// Administrators' alone, in a sealed machine folder, and the service's
        /// check takes it.
        ///
        /// A child process runs it, with `%ProgramData%` a stand-in under a
        /// temporary folder: the machine's folders are read once per process.
        /// In it the wizard's side runs on a thread with a limited copy of this
        /// test's token (Administrators deny-only, Medium integrity), as a
        /// Setup a person started has; the step's side runs with the test's own,
        /// elevated one.
        #[test]
        fn a_limited_setups_machine_enrollment_is_owned_by_administrators_and_sealed() {
            if let Some(root) = std::env::var_os(ROOT) {
                return the_wizard_and_its_step(Path::new(&root));
            }
            if !elevated_or_skip("Setup's machine write") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            // What the service leaves on a fresh install, before Setup writes:
            // both folders Administrators', inheriting all they hold.
            let program_data = stand_in(root.path());
            for dir in [
                program_data.join("roomler"),
                program_data.join("roomler").join("roomler"),
            ] {
                assert!(
                    icacls(&dir, &["/setowner", "*S-1-5-32-544"]),
                    "{}",
                    dir.display()
                );
                assert!(icacls(&dir, &["/reset"]), "{}", dir.display());
            }
            run_the_child(THIS_TEST, root.path(), &program_data);
        }

        /// G7, the same install on a host where a person's daemon staged a
        /// received file before the service ever ran: the parent, the machine
        /// folder and the staging folder are that person's. The step takes the
        /// parent and the machine folder for Administrators, and the service's
        /// check takes the result; what the person made below keeps its owner.
        #[test]
        fn a_limited_setup_on_a_tree_a_person_made_comes_out_the_same() {
            if let Some(root) = std::env::var_os(ROOT) {
                return the_wizard_and_its_step(Path::new(&root));
            }
            if !elevated_or_skip("Setup's machine write over a person's tree") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let program_data = stand_in(root.path());
            let person = local_guest().unwrap_or_else(|| USERS.to_string());
            let staging = program_data.join("roomler").join("roomler").join("staging");
            std::fs::create_dir(&staging).unwrap();
            for dir in [
                program_data.join("roomler"),
                program_data.join("roomler").join("roomler"),
                staging.clone(),
            ] {
                assert!(
                    icacls(&dir, &["/setowner", &format!("*{person}")]),
                    "{}",
                    dir.display()
                );
            }
            let staging_owner = owner(&sddl(&staging));
            run_the_child(PERSON_TEST, root.path(), &program_data);
            assert_eq!(
                owner(&sddl(&staging)),
                staging_owner,
                "the person's own folder"
            );
        }

        /// #3666 — the same install where a sealed machine folder holding an
        /// administrator's config and `.prev` was moved into a parent a person
        /// made. Setup reads the config as found, before the step takes the
        /// parent: nothing is folded (the child sees no config to keep), the
        /// config is written fresh, and no `.prev` is kept.
        #[test]
        fn a_limited_setup_never_folds_into_a_folder_moved_into_a_parent_a_person_made() {
            if let Some(root) = std::env::var_os(ROOT) {
                return the_wizard_and_its_step(Path::new(&root));
            }
            if !elevated_or_skip("Setup's machine write over a folder moved in") {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let program_data = stand_in(root.path());
            let person = local_guest().unwrap_or_else(|| USERS.to_string());
            let parent = program_data.join("roomler");
            let folder = parent.join("roomler");
            assert!(icacls(&parent, &["/setowner", &format!("*{person}")]));
            set_dacl(&folder, "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
            let planted = "server_url = \"https://roomler.ai\"\nagent_token = \"tok\"\n\
                           agent_id = \"aid\"\ntenant_id = \"tid\"\nmachine_id = \"mid\"\n\
                           machine_name = \"planted\"\nplanted_marker = \"planted\"\n";
            for name in ["config.toml", "config.toml.prev"] {
                let file = folder.join(name);
                std::fs::write(&file, planted).unwrap();
                set_dacl(&file, "D:P(A;;FA;;;SY)(A;;FA;;;BA)");
            }
            run_the_child(PLANTED_TEST, root.path(), &program_data);
            assert!(
                !folder.join("config.toml.prev").exists(),
                "no .prev is kept"
            );
            let written = std::fs::read_to_string(folder.join("config.toml")).unwrap();
            assert!(!written.contains("planted"), "{written}");
        }

        /// `%ProgramData%`'s stand-in under `root`, with the parent and the
        /// machine folder made below it, inheriting.
        fn stand_in(root: &Path) -> PathBuf {
            let program_data = root.join("ProgramData");
            std::fs::create_dir(&program_data).unwrap();
            set_dacl(&program_data, PROGRAM_DATA);
            std::fs::create_dir_all(program_data.join("roomler").join("roomler")).unwrap();
            program_data
        }

        /// Run `test` in a child process whose `%ProgramData%` is the stand-in.
        fn run_the_child(test: &str, root: &Path, program_data: &Path) {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test, "--exact", "--nocapture", "--test-threads=1"])
                .env(ROOT, root)
                .env("PROGRAMDATA", program_data)
                .output()
                .unwrap();
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success(), "{said}");
            assert!(said.contains(CHECKED), "the child ran the case: {said}");
        }

        fn the_wizard_and_its_step(root: &Path) {
            let config_path = roomlerd::config::machine_global_config_path();
            assert!(
                config_path.starts_with(root),
                "the machine config here is the stand-in's, never this computer's: {}",
                config_path.display()
            );
            let tree = MachineTree::here();
            let name =
                wizard_shared::step_channel::pipe_name(&crate::package_step::new_stage_name());
            let wizard_path = config_path.clone();
            let (written, served) = as_limited(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let listener =
                        wizard_shared::step_channel::Listener::create(&name).expect("the channel");
                    let step = {
                        let path = wizard_path.clone();
                        std::thread::spawn(move || the_step(&name, &path))
                    };
                    let (code, link) = accept_install(listener, std::process::id())
                        .await
                        .expect("the step's first word");
                    assert_eq!(code, 0);
                    let mut writer =
                        ConfigWriter::new(Some(link), &wizard_path, true, false).expect("a writer");
                    let prepared = writer.prepare().await.expect("prepared");
                    assert!(!prepared.had_config, "{prepared:?}");
                    let written = writer
                        .write(issued(), &advanced(), WindowsInstallFlavour::PerMachine)
                        .await
                        .expect("written");
                    drop(writer);
                    // Joined off the runtime's thread: the channel closes only
                    // once the runtime has seen its last read cancelled.
                    let served = tokio::task::spawn_blocking(move || step.join().unwrap())
                        .await
                        .unwrap();
                    (written, served)
                })
            });
            assert_eq!(written.agent_id, issued().agent_id);
            let config = sddl(&tree.config());
            assert_eq!(
                owner(&config),
                "BA",
                "the machine config belongs to Administrators: {config}"
            );
            assert!(
                sealed(&config, false),
                "SYSTEM and Administrators alone: {config}"
            );
            assert!(!names(&config, USERS), "{config}");
            let folder = sddl(tree.folder());
            assert_eq!(owner(&folder), "BA", "{folder}");
            assert!(
                sealed(&folder, true),
                "the machine folder is sealed: {folder}"
            );
            assert_eq!(machine_tree::check(&tree), Ok(()), "the service takes it");
            assert_eq!(served, Served::Written, "the step wrote it");
            let cfg = roomlerd::config::load(&config_path).unwrap();
            assert_eq!(cfg.agent_id, issued().agent_id);
            assert_eq!(cfg.lock_on_end, LockOnEnd::Auto);
            println!("{CHECKED}");
        }

        /// The step's side, with this process's own token: what `hand_over`
        /// does after the installer, for the config at `path`.
        fn the_step(name: &str, path: &Path) -> Served {
            let pipe = wizard_shared::step_channel::connect(name).expect("the step connects");
            let mut writer = pipe.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(pipe);
            send(&mut writer, &Answer::Msi { code: 0 }).unwrap();
            serve(&mut reader, &mut writer, path, &Mutex::new(()))
        }

        /// Run `f` on a new thread with a limited copy of this process's
        /// token: Administrators deny-only, Medium integrity.
        fn as_limited<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
            use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
            use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
            use windows_sys::Win32::Security::{
                CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, GetLengthSid,
                ImpersonateLoggedOnUser, PSID, RevertToSelf, SID_AND_ATTRIBUTES,
                SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
                TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
            };
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
            /// `SE_GROUP_INTEGRITY`.
            const INTEGRITY: u32 = 0x20;

            fn sid(text: &str) -> PSID {
                let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
                let mut sid: PSID = std::ptr::null_mut();
                // SAFETY: a NUL-terminated string; `sid` is LocalAlloc'd,
                // freed by the caller.
                let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
                assert_ne!(ok, 0, "{text}: {}", std::io::Error::last_os_error());
                sid
            }

            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: this process's pseudo-handle; `token` is written on
            // success and closed below.
            let opened = unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
                    &mut token,
                )
            };
            assert_ne!(opened, 0, "{}", std::io::Error::last_os_error());
            let admins = sid("S-1-5-32-544");
            let medium = sid("S-1-16-8192");
            let deny_only = [SID_AND_ATTRIBUTES {
                Sid: admins,
                Attributes: 0,
            }];
            let mut limited: HANDLE = std::ptr::null_mut();
            // SAFETY: a token opened for TOKEN_DUPLICATE; one SID to make
            // deny-only, DISABLE_MAX_PRIVILEGE, nothing named to delete
            // and no restricting SIDs.
            let made = unsafe {
                CreateRestrictedToken(
                    token,
                    DISABLE_MAX_PRIVILEGE,
                    1,
                    deny_only.as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    &mut limited,
                )
            };
            assert_ne!(made, 0, "{}", std::io::Error::last_os_error());
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: medium,
                    Attributes: INTEGRITY,
                },
            };
            // SAFETY: the new token, and a label whose SID lives until after
            // the call.
            let labelled = unsafe {
                SetTokenInformation(
                    limited,
                    TokenIntegrityLevel,
                    (&raw const label).cast(),
                    std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(medium),
                )
            };
            assert_ne!(labelled, 0, "{}", std::io::Error::last_os_error());
            // SAFETY: both SIDs were LocalAlloc'd above, and the token opened.
            unsafe {
                LocalFree(admins as _);
                LocalFree(medium as _);
                CloseHandle(token);
            }
            let limited = limited as usize;
            let joined = std::thread::spawn(move || {
                let limited = limited as HANDLE;
                // SAFETY: a token this process made; the thread reverts and
                // closes it below.
                let on = unsafe { ImpersonateLoggedOnUser(limited) };
                assert_ne!(on, 0, "{}", std::io::Error::last_os_error());
                let out = f();
                // SAFETY: this thread's own token, and the handle.
                unsafe {
                    RevertToSelf();
                    CloseHandle(limited);
                }
                out
            })
            .join();
            match joined {
                Ok(out) => out,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    }
}
