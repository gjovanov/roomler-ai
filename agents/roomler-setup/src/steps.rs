// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3432 — the steps of an install, and the tracker the pipelines move as each
//! step begins and ends.
//!
//! A failure takes its step from here, never from the error site: the site
//! hands over the error and its diagnostics code, and the tracker knows where
//! the run was ([`Steps::fail`]). The marks are the ones the pipelines already
//! made for the opt-in diagnostics, which the tracker forwards to
//! [`Diagnostics`], so that recorder's records are what they were. Two steps
//! have no diagnostics step of their own: the restart onto a reinstall's
//! enrollment (#3116) and the Linux/macOS steps run as the person after the
//! privileged half.
//!
//! #3590 — the tracker is also the install's checklist: a row per step, each
//! pending, running, done, failed or not reached, and a row of its own for
//! each warning, in the order it came. Every change goes to the page as a
//! [`ProgressEvent::Steps`] snapshot, which the page draws and nothing else,
//! and a failure settles every row as it is built ([`Steps::fail`]), so none
//! is left running.

use std::sync::{Arc, Mutex, OnceLock};

use roomler_ai_remote_control::setup_diagnostics::{ErrorCode, Step};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use wizard_shared::progress::{ProgressEvent, StepRow, replay_log};

use crate::diagnostics::{Diagnostics, cancel_requested};
use crate::failure::{Failure, Said};

/// One step of an install, as the person reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepId {
    /// The checks before anything is changed.
    Preflight,
    /// Asking the server which installer to fetch.
    Resolve,
    Download,
    /// The digest and the publisher of what was downloaded.
    Verify,
    /// The Windows MSI, or the Linux/macOS privileged half.
    Package,
    Extract,
    Integrate,
    /// macOS (FR-129 B2): the root daemon's own enrollment.
    RootHalf,
    Enroll,
    /// #3116: a reinstall's daemon restarted onto the enrollment just written.
    Restart,
    /// Linux/macOS: the steps run as the person after the privileged half.
    UserSteps,
    Companion,
    Anchor,
}

impl StepId {
    /// Its name on the wire, as the page reads it.
    pub fn wire(self) -> &'static str {
        match self {
            StepId::Preflight => "preflight",
            StepId::Resolve => "resolve",
            StepId::Download => "download",
            StepId::Verify => "verify",
            StepId::Package => "package",
            StepId::Extract => "extract",
            StepId::Integrate => "integrate",
            StepId::RootHalf => "root_half",
            StepId::Enroll => "enroll",
            StepId::Restart => "restart",
            StepId::UserSteps => "user_steps",
            StepId::Companion => "companion",
            StepId::Anchor => "anchor",
        }
    }

    /// The opt-in diagnostics' step for it, when it has one.
    pub fn diagnostics(self) -> Option<Step> {
        Some(match self {
            StepId::Preflight => Step::Preflight,
            StepId::Resolve => Step::Resolve,
            StepId::Download => Step::Download,
            StepId::Verify => Step::Verify,
            StepId::Package => Step::Msi,
            StepId::Extract => Step::Extract,
            StepId::Integrate => Step::Integrate,
            StepId::RootHalf | StepId::Enroll => Step::Enroll,
            StepId::Companion => Step::Companion,
            StepId::Anchor => Step::Anchor,
            StepId::Restart | StepId::UserSteps => return None,
        })
    }

    /// What the step does, to finish "Roomler Setup could not …".
    pub fn doing(self) -> &'static str {
        match self {
            StepId::Preflight => "check this computer before the install",
            StepId::Resolve => "find the installer",
            StepId::Download => "download the installer",
            StepId::Verify => "check the installer",
            StepId::Package => "install the package",
            StepId::Extract => "unpack the download",
            StepId::Integrate => "put roomler on this computer's PATH",
            StepId::RootHalf => "enroll the background service",
            StepId::Enroll => "enroll this device",
            StepId::Restart => "restart the service onto the new enrollment",
            StepId::UserSteps => "finish the steps for your account",
            StepId::Companion => "place the desktop app",
            StepId::Anchor => "anchor this device",
        }
    }

    /// Its row's words: what it does, as a line of the checklist.
    pub fn label(self) -> String {
        let mut chars = self.doing().chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => String::new(),
        }
    }
}

/// #3590 — where a step's row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowState {
    Pending,
    Running,
    Done,
    Failed,
    /// The run stopped before it.
    NotReached,
}

impl RowState {
    fn wire(self) -> &'static str {
        match self {
            RowState::Pending => "pending",
            RowState::Running => "running",
            RowState::Done => "done",
            RowState::Failed => "failed",
            RowState::NotReached => "not_reached",
        }
    }
}

/// #3590 — one row of the checklist: a step, or a warning the run raised,
/// which keeps a row of its own.
#[derive(Debug)]
enum Row {
    Step {
        step: StepId,
        state: RowState,
        /// What the step's last progress event said.
        detail: Option<String>,
    },
    Warning(String),
}

impl Row {
    fn of(step: StepId) -> Row {
        Row::Step {
            step,
            state: RowState::Pending,
            detail: None,
        }
    }

    fn is(&self, step: StepId) -> bool {
        matches!(self, Row::Step { step: s, .. } if *s == step)
    }

    fn pending(&self) -> bool {
        matches!(
            self,
            Row::Step {
                state: RowState::Pending,
                ..
            }
        )
    }

    fn wire(&self) -> StepRow {
        match self {
            Row::Step {
                step,
                state,
                detail,
            } => StepRow {
                step: step.wire().to_string(),
                state: state.wire().to_string(),
                label: step.label(),
                detail: detail.clone(),
            },
            Row::Warning(words) => StepRow {
                step: "warning".to_string(),
                state: "warning".to_string(),
                label: words.clone(),
                detail: None,
            },
        }
    }
}

/// Where the rows go: the page and the replay log ([`to_page`]), or a test's
/// recorder.
pub type Sink = Arc<dyn Fn(ProgressEvent) + Send + Sync>;

/// The page's channel as a [`Sink`]: the replay log first, as each
/// pipeline's `emit` does.
pub fn to_page(channel: &Channel<ProgressEvent>) -> Sink {
    let channel = channel.clone();
    Arc::new(move |event| {
        replay_log().push(event.clone());
        let _ = channel.send(event);
    })
}

/// Where a run is: the step that is open, the one before it, and the rows.
#[derive(Default)]
struct State {
    open: Option<StepId>,
    last: Option<StepId>,
    rows: Vec<Row>,
    sink: Option<Sink>,
}

impl State {
    /// Where the run is now: before the first step still pending.
    fn here(&self) -> usize {
        self.rows
            .iter()
            .position(Row::pending)
            .unwrap_or(self.rows.len())
    }

    fn set(&mut self, step: StepId, to: RowState) {
        for row in &mut self.rows {
            if let Row::Step { step: s, state, .. } = row
                && *s == step
            {
                *state = to;
            }
        }
    }

    /// `step` runs. A step the plan did not name gets its row where the run
    /// is, and the plan's steps before it, which the run passed over, are
    /// dropped: they did not apply.
    fn run(&mut self, step: StepId) {
        let at = match self.rows.iter().position(|r| r.is(step)) {
            Some(at) => at,
            None => {
                let at = self.here();
                self.rows.insert(at, Row::of(step));
                at
            }
        };
        let mut i = 0;
        self.rows.retain(|r| {
            let passed_over = i < at && r.pending();
            i += 1;
            !passed_over
        });
        for row in &mut self.rows {
            if let Row::Step {
                step: s,
                state,
                detail,
            } = row
                && *s == step
            {
                *state = RowState::Running;
                *detail = None;
            }
        }
    }

    /// The open step ends as `state`.
    fn close(&mut self, state: RowState) {
        if let Some(open) = self.open.take() {
            self.last = Some(open);
            self.set(open, state);
        }
    }

    /// A warning's row: after the step it came in and that step's earlier
    /// warnings, so the warnings keep the order they came in.
    fn warn(&mut self, words: String) {
        let under = self.open.or(self.last);
        let at = match under.and_then(|step| self.rows.iter().position(|r| r.is(step))) {
            Some(at) => {
                let mut at = at + 1;
                while matches!(self.rows.get(at), Some(Row::Warning(_))) {
                    at += 1;
                }
                at
            }
            None => self.here(),
        };
        self.rows.insert(at, Row::Warning(words));
    }

    /// A progress event's words, on the row of the step it came in.
    fn tell(&mut self, words: String) {
        let Some(step) = self.open.or(self.last) else {
            return;
        };
        for row in &mut self.rows {
            if let Row::Step {
                step: s, detail, ..
            } = row
                && *s == step
            {
                *detail = Some(words.clone());
            }
        }
    }

    /// A failure at `failed` settles every row: that one failed, the rows the
    /// run reached keep how they ended, and the rest were not reached. Only
    /// the open step runs, and it is the one that failed, so none is left
    /// running or pending.
    fn settle(&mut self, failed: StepId, cancelled: bool) {
        if !self.rows.iter().any(|r| r.is(failed)) {
            let at = self.here();
            self.rows.insert(at, Row::of(failed));
        }
        for row in &mut self.rows {
            if let Row::Step {
                step,
                state,
                detail,
            } = row
            {
                if *step == failed {
                    *state = RowState::Failed;
                    if cancelled {
                        *detail = Some("Cancelled".to_string());
                    }
                } else if *state == RowState::Pending {
                    *state = RowState::NotReached;
                }
            }
        }
    }
}

/// The tracker. One per process ([`Steps::global`]), as one install runs at a
/// time; a test builds its own on a recorder of its own.
pub struct Steps {
    diag: &'static Diagnostics,
    state: Mutex<State>,
}

impl Steps {
    pub fn new(diag: &'static Diagnostics) -> Self {
        Self {
            diag,
            state: Mutex::new(State::default()),
        }
    }

    /// The process-wide tracker, on the process-wide diagnostics recorder.
    pub fn global() -> &'static Steps {
        static GLOBAL: OnceLock<Steps> = OnceLock::new();
        GLOBAL.get_or_init(|| Steps::new(Diagnostics::global()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The install pipeline begins, with the steps it takes on every run in
    /// `plan` (a step it begins that the plan does not name gets its row when
    /// it begins), and the rows go to `sink`.
    pub fn started(&self, plan: &[StepId], sink: Option<Sink>) {
        *self.lock() = State {
            rows: plan.iter().copied().map(Row::of).collect(),
            sink,
            ..State::default()
        };
        self.diag.pipeline_started();
        self.send();
    }

    /// `step` begins. A step still open is closed as passed first: the run
    /// did reach the next one.
    pub fn begin(&self, step: StepId) {
        {
            let mut s = self.lock();
            s.close(RowState::Done);
            s.open = Some(step);
            s.run(step);
        }
        match step.diagnostics() {
            Some(d) => self.diag.begin(d),
            // No diagnostics step of its own: the one before it passed.
            None => self.diag.ok(),
        }
        self.send();
    }

    /// The open step passed.
    pub fn ok(&self) {
        self.lock().close(RowState::Done);
        self.diag.ok();
        self.send();
    }

    /// The open step did not apply after all, and its row goes.
    pub fn skip(&self) {
        {
            let mut s = self.lock();
            if let Some(open) = s.open.take() {
                s.last = Some(open);
                s.rows.retain(|r| !r.is(open));
            }
        }
        self.diag.discard();
        self.send();
    }

    /// A best-effort step's end from its report: `Some(true)` passed,
    /// `Some(false)` failed with `code` and the install goes on, `None` did not
    /// apply.
    pub fn best_effort(&self, report: Option<bool>, code: ErrorCode) {
        match report {
            Some(true) => self.ok(),
            Some(false) => {
                self.lock().close(RowState::Failed);
                self.diag.fail(code, false);
                self.send();
            }
            None => self.skip(),
        }
    }

    /// The pipeline finished. A step of the plan that never began did not
    /// apply, and its row goes.
    pub fn done(&self) {
        {
            let mut s = self.lock();
            s.close(RowState::Done);
            s.rows.retain(|r| !r.pending());
        }
        self.diag.done();
        self.send();
    }

    /// The step that is open, if one is.
    pub fn open(&self) -> Option<StepId> {
        self.lock().open
    }

    /// #3590 — an event the run sent the page. A warning gets a row of its
    /// own, after the step it came in, and a progress event's words go on its
    /// step's row; the rows then go out again.
    pub fn saw(&self, event: &ProgressEvent) {
        let Some(seen) = seen(event) else {
            return;
        };
        {
            let mut s = self.lock();
            match seen {
                Seen::Warning(words) => s.warn(words),
                Seen::Detail(words) => s.tell(words),
            }
        }
        self.send();
    }

    /// The checklist as it now is, as the page gets it.
    pub fn rows(&self) -> Vec<StepRow> {
        self.lock().rows.iter().map(Row::wire).collect()
    }

    /// The warnings the run raised, in the order they came: Done lists them.
    pub fn warnings(&self) -> Vec<String> {
        self.lock()
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Warning(words) => Some(words.clone()),
                Row::Step { .. } => None,
            })
            .collect()
    }

    /// The rows, to the page.
    fn send(&self) {
        let (sink, rows): (Option<Sink>, Vec<StepRow>) = {
            let s = self.lock();
            (s.sink.clone(), s.rows.iter().map(Row::wire).collect())
        };
        if let Some(sink) = sink {
            sink(ProgressEvent::Steps { rows });
        }
    }

    /// The open step failed with `err`: recorded with `code` (or as cancelled,
    /// when the operator's cancel is what failed it), and the [`Failure`] the
    /// run ends with.
    pub fn fail(&self, code: ErrorCode, err: impl Into<anyhow::Error>) -> Failure {
        self.fail_as(code, cancel_requested(), err.into())
    }

    /// The operator cancelled before the open step ran.
    pub fn cancelled(&self) -> Failure {
        self.fail_as(
            ErrorCode::Other,
            true,
            anyhow::anyhow!("install cancelled by operator"),
        )
    }

    /// The operator cancelled, and the run says what that left in `words`
    /// (a declined administrator prompt, a step that ran to its end before
    /// the cancel could stop it).
    pub fn cancelled_with(&self, words: String) -> Failure {
        self.fail_as(ErrorCode::Other, true, Said(words).into())
    }

    /// [`Steps::fail`] with the cancel flag given, not read. Every row is
    /// settled here, as the failure is built, and the page gets them before
    /// the failure's own event.
    pub(crate) fn fail_as(&self, code: ErrorCode, cancelled: bool, err: anyhow::Error) -> Failure {
        let step = {
            let mut s = self.lock();
            // A failure between two steps is the last one's.
            let step = s.open.take().or(s.last).unwrap_or(StepId::Preflight);
            s.last = Some(step);
            s.settle(step, cancelled);
            step
        };
        self.diag.fail(code, cancelled);
        self.send();
        Failure::build(step, code, cancelled, &err)
    }
}

/// What an event says on the checklist.
enum Seen {
    /// A warning, which keeps a row of its own.
    Warning(String),
    /// A progress event's words, for its step's row.
    Detail(String),
}

/// What `event` says on the checklist, if anything: the run's marks move the
/// rows, and a download's ticks and a script's lines have panels of their own.
/// Every event is named here, so a new one is placed when it is added.
fn seen(event: &ProgressEvent) -> Option<Seen> {
    use ProgressEvent as E;
    let detail = |words: String| Some(Seen::Detail(words));
    match event {
        // What is installed here already and what the install does to it,
        // in a person's words (each pipeline's `label_existing`).
        E::PreflightOk { existing } => detail(existing.clone()),
        E::PreflightWarning { message } => Some(Seen::Warning(message.clone())),
        E::AnchorSkipped { reason } => Some(Seen::Warning(format!("Not anchored: {reason}"))),
        E::DaemonRestartSkipped { reason } => Some(Seen::Warning(format!(
            "The Roomler service was not restarted: {reason}"
        ))),
        E::CompanionAbsent { message } => Some(Seen::Warning(format!(
            "roomler-desktop is not there yet. {message}"
        ))),
        E::ElevationUnavailable { reason, .. } => Some(Seen::Warning(format!(
            "No administrator prompt could be shown: {reason}"
        ))),
        E::AssetResolved {
            tag, size_bytes, ..
        } => detail(format!(
            "{} ({:.1} MB)",
            version_of(tag),
            *size_bytes as f64 / 1_000_000.0
        )),
        E::DownloadVerified { sha256_match: true } => detail("The checksum matches".into()),
        E::DownloadVerified {
            sha256_match: false,
        } => detail("The download does not match its checksum".into()),
        E::PublisherVerified { signer } => detail(format!("Signed by {signer}")),
        E::MsiSpawned { pid } => detail(format!("Windows Installer is running (PID {pid})")),
        E::MsiCompleted { code, decoded } => detail(format!(
            "Windows Installer finished (exit {code}, {decoded})"
        )),
        E::ExtractDone { tunnel_binary } => {
            detail(format!("The roomler CLI is at {tunnel_binary}"))
        }
        E::IntegrationDone {
            path_updated,
            shortcut_created,
        } => detail(format!(
            "PATH {}; shortcut {}",
            if *path_updated {
                "updated"
            } else {
                "unchanged"
            },
            if *shortcut_created {
                "created"
            } else {
                "skipped"
            }
        )),
        E::EnrollOk {
            principal_kind,
            principal_id,
            ..
        } => detail(format!("{} {principal_id}", enrolled_as(principal_kind))),
        E::AnchorOk => detail("Commands can run as SYSTEM here".into()),
        E::CompanionWaiting => {
            detail("Waiting for the Roomler service to add roomler-desktop".into())
        }
        E::CompanionPresent => detail("roomler-desktop is in place".into()),
        E::DaemonRestartOk { pid: Some(pid) } => {
            detail(format!("The service runs the new enrollment (PID {pid})"))
        }
        E::DaemonRestartOk { pid: None } => detail("The service runs the new enrollment".into()),
        E::ScriptHalfStarted { tool, .. } => detail(run_by(tool)),
        E::ScriptHalfDone { code: 0, .. } => detail("Finished".into()),
        E::ScriptHalfDone { code, .. } => detail(format!("Exited with {code}")),
        E::ElevationCancelled { .. } => detail("Cancelled at the administrator prompt".into()),
        // `AssetResolving` names the MSI flavour, so it is no row's words.
        // `SystemContextError` has a panel of its own, and the package step
        // fails right after it.
        E::Started
        | E::PreflightStarted
        | E::AssetResolving { .. }
        | E::DownloadStarted { .. }
        | E::DownloadProgress { .. }
        | E::ExtractStarted { .. }
        | E::IntegrationStarted
        | E::EnrollStarted
        | E::AnchorStarted
        | E::DaemonRestartStarted
        | E::ScriptLine { .. }
        | E::Done
        | E::Error { .. }
        | E::SystemContextError { .. }
        | E::Steps { .. } => None,
    }
}

/// A release tag as a person reads it: `agent-v0.4.208` is version 0.4.208.
fn version_of(tag: &str) -> String {
    let version = ["agent-v", "tunnel-v", "setup-v"]
        .iter()
        .find_map(|prefix| tag.strip_prefix(prefix))
        .unwrap_or(tag);
    format!("version {version}")
}

/// What an `EnrollOk` enrolled, for a person.
fn enrolled_as(kind: &str) -> &'static str {
    match kind {
        "agent" => "Device",
        "tunnel_client" => "Tunnel client",
        // `orchestrator_unix::ROOT_HALF_PRINCIPAL_KIND`: a Mac's root half.
        "agent_root_half" => "The private network's device",
        _ => "Enrolled",
    }
}

/// What runs a half of the embedded `install.sh`, for a person.
fn run_by(tool: &str) -> String {
    match tool {
        "pkexec" => "Through the system's password prompt".to_string(),
        "osascript" => "Through the macOS administrator prompt".to_string(),
        "terminal" => "In a terminal, where you run the command".to_string(),
        "sh" => "As you".to_string(),
        other => other.to_string(),
    }
}

/// `result.or_fail(code)?`: the error, as the failure of the step that is open.
///
/// It takes an error and never a `String`: a site keeps the cause and adds
/// what it was doing with `anyhow::Context`, and words with no cause under them
/// are a [`Said`].
pub trait OrFail<T> {
    fn or_fail(self, code: ErrorCode) -> Result<T, Failure>;
}

impl<T, E: Into<anyhow::Error>> OrFail<T> for Result<T, E> {
    fn or_fail(self, code: ErrorCode) -> Result<T, Failure> {
        self.map_err(|e| Steps::global().fail(code, e))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use roomler_ai_remote_control::setup_diagnostics::SetupDiagnostic;

    use super::*;
    use crate::diagnostics::Sender;

    /// Records every diagnostics record it is handed.
    #[derive(Default)]
    pub(crate) struct Kept(pub Mutex<Vec<SetupDiagnostic>>);

    impl Sender for Kept {
        fn send(&self, _url: String, record: SetupDiagnostic) {
            self.0.lock().unwrap().push(record);
        }
    }

    /// A tracker on a recorder of its own, ticked and pointed at a server.
    pub(crate) fn tracker() -> (Steps, Arc<Kept>) {
        let kept = Arc::new(Kept::default());
        let diag: &'static Diagnostics = Box::leak(Box::new(Diagnostics::new(kept.clone())));
        diag.set_opt_in(true);
        diag.server_known("https://roomler.example");
        (Steps::new(diag), kept)
    }

    fn records(kept: &Kept) -> Vec<(String, String, Option<String>)> {
        kept.0
            .lock()
            .unwrap()
            .iter()
            .map(|d| (d.step.clone(), d.outcome.clone(), d.error_code.clone()))
            .collect()
    }

    #[test]
    fn a_failure_is_the_open_steps_and_the_diagnostics_say_so() {
        let (steps, kept) = tracker();
        steps.started(&[], None);
        steps.begin(StepId::Preflight);
        steps.ok();
        steps.begin(StepId::Resolve);
        let failure = steps.fail_as(
            ErrorCode::Resolve,
            false,
            anyhow::anyhow!("no answer").context("GET https://roomler.example/x"),
        );
        assert_eq!(failure.step, StepId::Resolve);
        assert_eq!(steps.open(), None);
        assert_eq!(
            records(&kept),
            vec![
                ("preflight".into(), "ok".into(), None),
                ("resolve".into(), "failed".into(), Some("resolve".into())),
            ]
        );
    }

    #[test]
    fn a_failure_between_two_steps_is_the_last_ones() {
        let (steps, _) = tracker();
        steps.started(&[], None);
        steps.begin(StepId::Download);
        steps.ok();
        let failure = steps.fail_as(ErrorCode::Other, false, anyhow::anyhow!("x"));
        assert_eq!(failure.step, StepId::Download);
    }

    #[test]
    fn a_step_with_no_diagnostics_step_closes_the_one_before_and_records_nothing() {
        let (steps, kept) = tracker();
        steps.started(&[], None);
        steps.begin(StepId::Enroll);
        steps.begin(StepId::Restart);
        let failure = steps.fail_as(ErrorCode::Other, false, anyhow::anyhow!("x"));
        assert_eq!(failure.step, StepId::Restart);
        assert_eq!(records(&kept), vec![("enroll".into(), "ok".into(), None)]);
    }

    #[test]
    fn a_cancel_is_recorded_as_one_and_reads_as_one() {
        let (steps, kept) = tracker();
        steps.started(&[], None);
        steps.begin(StepId::Download);
        let failure = steps.fail_as(
            ErrorCode::Download,
            true,
            anyhow::anyhow!("download cancelled"),
        );
        assert_eq!(failure.kind, crate::failure::Kind::Cancelled);
        assert_eq!(
            records(&kept),
            vec![("download".into(), "cancelled".into(), None)]
        );
    }

    #[test]
    fn a_best_effort_step_fails_without_ending_the_run() {
        let (steps, kept) = tracker();
        steps.started(&[], None);
        steps.begin(StepId::Companion);
        steps.best_effort(Some(false), ErrorCode::Companion);
        steps.begin(StepId::Anchor);
        steps.best_effort(None, ErrorCode::Anchor);
        steps.done();
        assert_eq!(
            records(&kept),
            vec![
                (
                    "companion".into(),
                    "failed".into(),
                    Some("companion".into())
                ),
                ("done".into(), "ok".into(), None),
            ]
        );
    }

    #[test]
    fn every_step_has_its_own_wire_name() {
        use std::collections::HashSet;
        let all = [
            StepId::Preflight,
            StepId::Resolve,
            StepId::Download,
            StepId::Verify,
            StepId::Package,
            StepId::Extract,
            StepId::Integrate,
            StepId::RootHalf,
            StepId::Enroll,
            StepId::Restart,
            StepId::UserSteps,
            StepId::Companion,
            StepId::Anchor,
        ];
        let names: HashSet<_> = all.iter().map(|s| s.wire()).collect();
        assert_eq!(names.len(), all.len());
        for s in all {
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::Value::String(s.wire().to_string())
            );
        }
    }

    // #3590 — the checklist model.

    /// The Windows pipeline's plan.
    const PLAN: &[StepId] = &[
        StepId::Preflight,
        StepId::Resolve,
        StepId::Download,
        StepId::Verify,
        StepId::Package,
        StepId::Enroll,
    ];

    /// The rows as (step, state), as the page gets them.
    fn states(steps: &Steps) -> Vec<(String, String)> {
        steps
            .rows()
            .into_iter()
            .map(|r| (r.step, r.state))
            .collect()
    }

    fn row(step: &str, state: &str) -> (String, String) {
        (step.to_string(), state.to_string())
    }

    fn resolved(tag: &str, size_bytes: u64) -> ProgressEvent {
        ProgressEvent::AssetResolved {
            tag: tag.into(),
            size_bytes,
            digest: None,
        }
    }

    #[test]
    fn the_plan_is_pending_until_it_runs() {
        let (steps, _) = tracker();
        steps.started(PLAN, None);
        steps.begin(StepId::Preflight);
        let rows = states(&steps);
        assert_eq!(rows.len(), PLAN.len());
        assert_eq!(rows[0], row("preflight", "running"));
        assert!(rows[1..].iter().all(|(_, s)| s == "pending"), "{rows:?}");
    }

    #[test]
    fn a_failure_settles_every_row_wherever_the_run_stops() {
        for at in 0..PLAN.len() {
            for cancelled in [false, true] {
                let (steps, _) = tracker();
                steps.started(PLAN, None);
                for &step in &PLAN[..at] {
                    steps.begin(step);
                    steps.ok();
                }
                steps.begin(PLAN[at]);
                let failure = steps.fail_as(ErrorCode::Other, cancelled, anyhow::anyhow!("x"));
                assert_eq!(failure.step, PLAN[at]);
                let want: Vec<_> = PLAN
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let state = match i.cmp(&at) {
                            std::cmp::Ordering::Less => "done",
                            std::cmp::Ordering::Equal => "failed",
                            std::cmp::Ordering::Greater => "not_reached",
                        };
                        row(s.wire(), state)
                    })
                    .collect();
                assert_eq!(
                    states(&steps),
                    want,
                    "stopped at {at}, cancelled {cancelled}"
                );
            }
        }
    }

    #[test]
    fn a_failure_in_a_step_the_plan_did_not_name_settles_it_too() {
        let (steps, _) = tracker();
        steps.started(PLAN, None);
        for &step in PLAN {
            steps.begin(step);
            steps.ok();
        }
        // #3116's restart, which only a reinstall runs.
        steps.begin(StepId::Restart);
        let _ = steps.fail_as(ErrorCode::Other, false, anyhow::anyhow!("x"));
        let rows = states(&steps);
        assert_eq!(rows.last(), Some(&row("restart", "failed")));
        assert!(
            rows[..rows.len() - 1].iter().all(|(_, s)| s == "done"),
            "{rows:?}"
        );
    }

    #[test]
    fn two_warnings_keep_two_rows_in_the_order_they_came() {
        let (steps, _) = tracker();
        steps.started(PLAN, None);
        steps.begin(StepId::Preflight);
        steps.saw(&ProgressEvent::PreflightWarning {
            message: "first".into(),
        });
        steps.saw(&ProgressEvent::PreflightWarning {
            message: "second".into(),
        });
        steps.ok();
        steps.begin(StepId::Resolve);
        let rows = steps.rows();
        let shown: Vec<_> = rows
            .iter()
            .take(4)
            .map(|r| (r.state.as_str(), r.label.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("done", "Check this computer before the install"),
                ("warning", "first"),
                ("warning", "second"),
                ("running", "Find the installer"),
            ]
        );
        assert_eq!(steps.warnings(), ["first", "second"]);
    }

    #[test]
    fn a_progress_event_replaces_its_own_rows_words() {
        let (steps, _) = tracker();
        steps.started(PLAN, None);
        steps.begin(StepId::Preflight);
        steps.ok();
        steps.begin(StepId::Resolve);
        steps.saw(&ProgressEvent::AssetResolving {
            artifact: "permachine".into(),
        });
        steps.saw(&resolved("agent-v0.4.208", 45_100_000));
        let rows = steps.rows();
        assert_eq!(
            rows.len(),
            PLAN.len(),
            "a progress event makes no row: {rows:?}"
        );
        assert_eq!(rows[1].detail.as_deref(), Some("version 0.4.208 (45.1 MB)"));
        steps.ok();
        steps.begin(StepId::Download);
        steps.ok();
        steps.begin(StepId::Verify);
        steps.saw(&ProgressEvent::DownloadVerified { sha256_match: true });
        steps.saw(&ProgressEvent::PublisherVerified {
            signer: "Roomler".into(),
        });
        assert_eq!(steps.rows()[3].detail.as_deref(), Some("Signed by Roomler"));
    }

    #[test]
    fn the_companions_resolve_and_download_are_its_own_step() {
        let (steps, _) = tracker();
        steps.started(PLAN, None);
        for &step in PLAN {
            steps.begin(step);
            if step == StepId::Resolve {
                steps.saw(&resolved("agent-v0.4.208", 45_100_000));
            }
            steps.ok();
        }
        steps.begin(StepId::Companion);
        steps.saw(&ProgressEvent::AssetResolving {
            artifact: "roomler-desktop".into(),
        });
        steps.saw(&resolved("agent-v0.4.209", 9_000_000));
        steps.saw(&ProgressEvent::DownloadStarted {
            total_bytes: 9_000_000,
        });
        steps.ok();
        steps.done();
        let rows = steps.rows();
        assert_eq!(rows.len(), PLAN.len() + 1);
        assert_eq!(
            rows[1].detail.as_deref(),
            Some("version 0.4.208 (45.1 MB)"),
            "the daemon's row keeps its own words"
        );
        let companion = rows.last().unwrap();
        assert_eq!(
            (companion.step.as_str(), companion.state.as_str()),
            ("companion", "done")
        );
        assert_eq!(
            companion.detail.as_deref(),
            Some("version 0.4.209 (9.0 MB)")
        );
    }

    #[test]
    fn no_row_names_an_msi_flavour() {
        use roomlerd::install_detect::{ExistingInstall, InstallInfo};
        let (steps, _) = tracker();
        steps.started(PLAN, None);
        steps.begin(StepId::Preflight);
        // The words the agent pipeline sends for an install found here: the
        // preflight's row says them, so a person knows what is there.
        let existing = crate::orchestrator_agent::label_existing(
            &ExistingInstall::PerUser(InstallInfo {
                version: Some("0.4.100".into()),
                install_location: None,
            }),
            "peruser",
        );
        steps.saw(&ProgressEvent::PreflightOk {
            existing: existing.clone(),
        });
        steps.ok();
        assert_eq!(steps.rows()[0].detail.as_deref(), Some(existing.as_str()));
        steps.begin(StepId::Resolve);
        for artifact in ["permachine", "peruser", "systemcontext"] {
            steps.saw(&ProgressEvent::AssetResolving {
                artifact: artifact.into(),
            });
        }
        let _ = steps.fail_as(ErrorCode::Resolve, false, anyhow::anyhow!("x"));
        for r in steps.rows() {
            let shown = format!("{} {}", r.label, r.detail.unwrap_or_default()).to_lowercase();
            for flavour in ["permachine", "peruser", "systemcontext"] {
                assert!(!shown.contains(flavour), "a row names {flavour}: {shown}");
            }
        }
    }

    #[test]
    fn done_drops_what_did_not_apply_and_keeps_the_warnings() {
        let (steps, _) = tracker();
        // The Linux/macOS plan: a step the plan does not name takes its place
        // where the run is, and a step the run passes over is dropped.
        steps.started(
            &[StepId::Preflight, StepId::Package, StepId::UserSteps],
            None,
        );
        steps.begin(StepId::Preflight);
        steps.ok();
        steps.begin(StepId::Enroll);
        steps.ok();
        steps.begin(StepId::UserSteps);
        steps.saw(&ProgressEvent::DaemonRestartSkipped { reason: "r".into() });
        steps.ok();
        steps.begin(StepId::Anchor);
        steps.skip();
        steps.done();
        assert_eq!(
            states(&steps),
            [
                row("preflight", "done"),
                row("enroll", "done"),
                row("user_steps", "done"),
                row("warning", "warning"),
            ]
        );
        assert_eq!(
            steps.warnings(),
            ["The Roomler service was not restarted: r"]
        );
    }

    #[test]
    fn a_best_effort_failure_is_a_failed_row_and_the_run_goes_on() {
        let (steps, _) = tracker();
        steps.started(PLAN, None);
        steps.begin(StepId::Companion);
        steps.best_effort(Some(false), ErrorCode::Companion);
        steps.done();
        assert_eq!(states(&steps), [row("companion", "failed")]);
    }

    #[test]
    fn every_change_sends_the_rows_and_the_last_has_none_running() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink: Sink = {
            let sent = sent.clone();
            Arc::new(move |event| sent.lock().unwrap().push(event))
        };
        let (steps, _) = tracker();
        steps.started(PLAN, Some(sink));
        steps.begin(StepId::Preflight);
        // A download's tick and a script's line are no row.
        steps.saw(&ProgressEvent::DownloadProgress { received_bytes: 1 });
        steps.saw(&ProgressEvent::ScriptLine {
            half: "user".into(),
            line: "x".into(),
        });
        let _ = steps.fail_as(ErrorCode::Preflight, false, anyhow::anyhow!("x"));
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 3, "{sent:?}");
        let Some(ProgressEvent::Steps { rows }) = sent.last() else {
            panic!("{sent:?}")
        };
        assert_eq!(*rows, steps.rows());
        assert!(
            rows.iter()
                .all(|r| r.state != "running" && r.state != "pending"),
            "{rows:?}"
        );
        let wire = serde_json::to_value(sent.last().unwrap()).unwrap();
        assert_eq!(wire["type"], "Steps");
        assert_eq!(wire["data"]["rows"][0]["step"], "preflight");
        assert_eq!(wire["data"]["rows"][0]["state"], "failed");
    }
}
