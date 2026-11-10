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

use std::sync::{Mutex, OnceLock};

use roomler_ai_remote_control::setup_diagnostics::{ErrorCode, Step};
use serde::{Deserialize, Serialize};

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
}

/// Where a run is: the step that is open, and the one before it.
#[derive(Debug, Default)]
struct State {
    open: Option<StepId>,
    last: Option<StepId>,
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

    /// The install pipeline begins.
    pub fn started(&self) {
        *self.lock() = State::default();
        self.diag.pipeline_started();
    }

    /// `step` begins. A step still open is closed as passed first: the run
    /// did reach the next one.
    pub fn begin(&self, step: StepId) {
        {
            let mut s = self.lock();
            if let Some(open) = s.open.replace(step) {
                s.last = Some(open);
            }
        }
        match step.diagnostics() {
            Some(d) => self.diag.begin(d),
            // No diagnostics step of its own: the one before it passed.
            None => self.diag.ok(),
        }
    }

    /// The open step passed.
    pub fn ok(&self) {
        self.close();
        self.diag.ok();
    }

    /// The open step did not apply after all.
    pub fn skip(&self) {
        self.close();
        self.diag.discard();
    }

    /// A best-effort step's end from its report: `Some(true)` passed,
    /// `Some(false)` failed with `code` and the install goes on, `None` did not
    /// apply.
    pub fn best_effort(&self, report: Option<bool>, code: ErrorCode) {
        match report {
            Some(true) => self.ok(),
            Some(false) => {
                self.close();
                self.diag.fail(code, false);
            }
            None => self.skip(),
        }
    }

    /// The pipeline finished.
    pub fn done(&self) {
        self.close();
        self.diag.done();
    }

    /// The step that is open, if one is.
    pub fn open(&self) -> Option<StepId> {
        self.lock().open
    }

    fn close(&self) {
        let mut s = self.lock();
        if let Some(open) = s.open.take() {
            s.last = Some(open);
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

    /// [`Steps::fail`] with the cancel flag given, not read.
    pub(crate) fn fail_as(&self, code: ErrorCode, cancelled: bool, err: anyhow::Error) -> Failure {
        let step = {
            let mut s = self.lock();
            // A failure between two steps is the last one's.
            let step = s.open.take().or(s.last).unwrap_or(StepId::Preflight);
            s.last = Some(step);
            step
        };
        self.diag.fail(code, cancelled);
        Failure::build(step, code, cancelled, &err)
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
        steps.started();
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
        steps.started();
        steps.begin(StepId::Download);
        steps.ok();
        let failure = steps.fail_as(ErrorCode::Other, false, anyhow::anyhow!("x"));
        assert_eq!(failure.step, StepId::Download);
    }

    #[test]
    fn a_step_with_no_diagnostics_step_closes_the_one_before_and_records_nothing() {
        let (steps, kept) = tracker();
        steps.started();
        steps.begin(StepId::Enroll);
        steps.begin(StepId::Restart);
        let failure = steps.fail_as(ErrorCode::Other, false, anyhow::anyhow!("x"));
        assert_eq!(failure.step, StepId::Restart);
        assert_eq!(records(&kept), vec![("enroll".into(), "ok".into(), None)]);
    }

    #[test]
    fn a_cancel_is_recorded_as_one_and_reads_as_one() {
        let (steps, kept) = tracker();
        steps.started();
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
        steps.started();
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
}
