// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-149 P1 — a one-row Mac's consent questions on the person's own panel,
//! drawn by the screen worker.
//!
//! The window server refuses a root daemon that launchd started before anyone
//! signed in, and a log-out detaches one it served (#4053), so the root
//! daemon's own panel shows nothing for the rest of its life after a reboot or
//! a log-out. The screen worker (`run --supervised --unenrolled`) is started
//! as the person at each login, in the person's GUI session, where the window
//! server serves it. So the root daemon asks the worker to show its question,
//! and the person's answer comes back over the road the app's answers take
//! (#3864): the root daemon's person socket, which asks the seat again.
//!
//! ## The chain, for one question on a one-row Mac's root daemon
//!
//! | # | Surface | Asked when | Its answer |
//! |---|---|---|---|
//! | 1 | the session panel | the switch is on; an attached worker said [`CONSENT_PANEL`]; its account owns `/dev/console` now; the person socket serves that account; the root daemon's own panel shows no other question | `shown`: `native=true panel=session`. `busy`, `not_up`, or no answer within [`PANEL_WITHIN`]: the app, never the root daemon's own panel |
//! | 2 | the root daemon's own panel (D2) | the session panel was not asked, and shows no other question | as before FR-149: `native=true panel=root` |
//! | 3 | the app | as before FR-149 | `companion` |
//! | 4 | the floor | none of the above | `no_prompt_surface` at the window's end |
//!
//! Every other daemon (a two-row Mac, Linux, Windows) takes master's chain:
//! its own panel, then the app ([`chain`]).
//!
//! ## The invariant
//!
//! A session process's word routes a question and never decides one. `shown`
//! only marks the question `Native`, so the app does not show it a second
//! time; the answer the panel carries passes every gate the app's does
//! (`consent_relay::panel_clicked`). At most one panel is on the screen: once
//! the session panel was asked, the root daemon's own panel is not tried.
//!
//! The readers are macOS's (`TheRoad`, the worker's `answer`); here they are
//! a trait, so these cells run on every lane.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::PromptView;

/// The word a worker says in its caps frame (`delegate::DelegateFrame::WorkerCaps`)
/// when it hosts the panel for the root daemon. Matched by equality, as
/// `relay-consent` is: a near miss is dropped.
pub const CONSENT_PANEL: &str = "consent-panel";

/// How long the root daemon waits for the worker's word on a question. It
/// takes the place of #4063's probe (`PROBE_WAIT`, also 500 ms), never adds to
/// it: a question whose session panel was asked does not go on to the root
/// daemon's own panel.
pub const PANEL_WITHIN: Duration = Duration::from_millis(500);

/// The switch, `ROOMLERD_SESSION_PANEL`. Anything but an explicit off leaves
/// it on, as `ROOMLERD_PERSON_SOCKET` reads.
pub fn enabled_by_env(v: Option<&str>) -> bool {
    !matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// [`enabled_by_env`], for this process. The supervisor forwards the root
/// daemon's value to its worker (`macos_supervisor::WORKER_ENV_FORWARD`), and
/// a worker with the switch off does not say [`CONSENT_PANEL`].
pub fn enabled() -> bool {
    enabled_by_env(std::env::var("ROOMLERD_SESSION_PANEL").ok().as_deref())
}

/// A question as it crosses the delegation channel: the panel's text, and the
/// time left in milliseconds, since an `Instant` does not cross processes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PanelQuestion {
    pub title: String,
    pub lead: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub org: String,
    pub left_ms: u64,
}

impl PanelQuestion {
    /// `view`, with the time left from `now`.
    pub fn of(view: &PromptView, now: Instant) -> Self {
        Self {
            title: view.title.clone(),
            lead: view.lead.clone(),
            detail: view.detail.clone(),
            permissions: view.permissions.clone(),
            org: view.org.clone(),
            left_ms: view.expires_at.saturating_duration_since(now).as_millis() as u64,
        }
    }

    /// The panel's view of question `id`, counting down from `now`.
    pub fn view(&self, id: &str, now: Instant) -> PromptView {
        PromptView {
            session_hex: id.to_string(),
            title: self.title.clone(),
            lead: self.lead.clone(),
            detail: self.detail.clone(),
            permissions: self.permissions.clone(),
            org: self.org.clone(),
            expires_at: now + Duration::from_millis(self.left_ms),
        }
    }
}

/// What the worker says of a question (`PanelTold`): `"shown"`, `"busy"`, or
/// `{"not_up":{"why":…}}`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PanelState {
    /// The panel is up for the question: its window has a window device.
    Shown,
    /// The panel shows another question.
    Busy,
    /// The panel could not take it, and why.
    NotUp { why: String },
}

/// What came of the root daemon's ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    /// The worker said this within the wait.
    Told(PanelState),
    /// No word within the wait, the channel ended under it, or the queue
    /// toward the worker was full or gone.
    NoAnswer,
}

/// Why the root daemon did not ask the session panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotAsked {
    /// `ROOMLERD_SESSION_PANEL` is off.
    SwitchOff,
    /// No worker is attached.
    NoWorker,
    /// The attached worker did not say [`CONSENT_PANEL`].
    NoWord,
    /// The worker's person does not own `/dev/console` now.
    NotAtTheConsole,
    /// The root daemon's person socket does not serve the worker's person
    /// (`ROOMLERD_PERSON_SOCKET=0`, or the seat is moving).
    PersonSocket,
    /// The question's id is not a 24-hex ObjectId, or no time is left.
    NotAQuestion,
    /// The root daemon's own panel shows another question: a second panel
    /// would be on the screen.
    RootPanelBusy,
}

impl NotAsked {
    /// The log's `why`.
    pub fn why(self) -> &'static str {
        match self {
            Self::SwitchOff => "switch_off",
            Self::NoWorker => "no_worker",
            Self::NoWord => "no_word",
            Self::NotAtTheConsole => "not_at_the_console",
            Self::PersonSocket => "person_socket",
            Self::NotAQuestion => "not_a_question",
            Self::RootPanelBusy => "root_panel_busy",
        }
    }
}

/// What the session panel came to for one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPanel {
    Shown,
    Busy,
    NotUp(String),
    NoAnswer,
    NotAsked(NotAsked),
}

/// The root daemon's reads and its channel to the worker, as [`chain`] asks
/// them. The daemon's are `TheRoad` (macOS); a test stands them in.
#[async_trait::async_trait]
pub trait Road: Send + Sync {
    /// Whether this process is a one-row Mac's root daemon, the one daemon
    /// whose questions the session panel can show.
    fn one_row(&self) -> bool;
    /// [`enabled`].
    fn switch_on(&self) -> bool;
    /// The attached worker: its account, and whether it said
    /// [`CONSENT_PANEL`] on the channel it holds now.
    fn worker(&self) -> Option<(u32, bool)>;
    /// `/dev/console`'s owner now: a person, never root.
    fn console_owner(&self) -> Option<u32>;
    /// The person the root daemon's person socket serves now.
    fn person_socket_serves(&self) -> Option<u32>;
    /// Whether the root daemon's own panel shows another question now.
    fn root_panel_up(&self) -> bool;
    /// Whether the session panel shows another of this daemon's questions
    /// now: one the worker said `shown` for, not yet taken down.
    fn session_panel_up(&self) -> bool;
    /// Ask the worker to show `question` under `id`, waiting `within`.
    async fn ask(&self, id: &str, question: PanelQuestion, within: Duration) -> Asked;
}

/// The daemon's [`Road`]: its one-row host (`consent_relay::one_row_host`,
/// registered only on a one-row Mac's root daemon, never in a test), the
/// switch, `/dev/console`, and the person socket ([`crate::person_socket::serves`]).
/// Off a one-row Mac's root daemon `one_row` is false, and the chain is the
/// daemon's own panel alone, as before FR-149.
pub struct TheRoad;

#[async_trait::async_trait]
impl Road for TheRoad {
    fn one_row(&self) -> bool {
        crate::consent_relay::one_row_host().is_some()
    }
    fn switch_on(&self) -> bool {
        enabled()
    }
    fn worker(&self) -> Option<(u32, bool)> {
        let host = crate::consent_relay::one_row_host()?;
        Some((host.worker_uid()?, host.says_consent_panel()))
    }
    fn console_owner(&self) -> Option<u32> {
        console_owner()
    }
    fn person_socket_serves(&self) -> Option<u32> {
        crate::person_socket::serves()
    }
    fn root_panel_up(&self) -> bool {
        #[cfg(all(target_os = "macos", feature = "viewer-indicator-macos"))]
        {
            super::mac::shows_a_question()
        }
        #[cfg(not(all(target_os = "macos", feature = "viewer-indicator-macos")))]
        {
            false
        }
    }
    fn session_panel_up(&self) -> bool {
        crate::consent_relay::one_row_host().is_some_and(|host| host.panel_shows_one())
    }
    async fn ask(&self, id: &str, question: PanelQuestion, within: Duration) -> Asked {
        match crate::consent_relay::one_row_host() {
            Some(host) => host.ask_panel(id, question, within).await,
            None => Asked::NoAnswer,
        }
    }
}

/// `/dev/console`'s owner now, when that is a person: uid 0 is the login
/// window, which is nobody (the FR-43 supervisor's and the person socket's
/// read). `None` off macOS. One `stat`.
pub fn console_owner() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/dev/console")
            .ok()
            .map(|m| m.uid())
            .filter(|&u| u != 0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Whether the root daemon's answers from the worker have gone silent: one
/// WARN for a run of them, one INFO when answers return.
static SILENT: AtomicBool = AtomicBool::new(false);

/// The session panel for `view`: asked only of the worker whose person owns
/// `/dev/console` now, after the reads that decide whether to ask. A word
/// said, a worker attached or a seat read never makes a question shown: only
/// the worker's `shown` within [`PANEL_WITHIN`] does.
pub async fn ask(road: &dyn Road, view: &PromptView) -> SessionPanel {
    if !road.switch_on() {
        return SessionPanel::NotAsked(NotAsked::SwitchOff);
    }
    let Some((uid, said)) = road.worker() else {
        return SessionPanel::NotAsked(NotAsked::NoWorker);
    };
    if !said {
        return SessionPanel::NotAsked(NotAsked::NoWord);
    }
    if road.console_owner() != Some(uid) {
        return SessionPanel::NotAsked(NotAsked::NotAtTheConsole);
    }
    if road.person_socket_serves() != Some(uid) {
        return SessionPanel::NotAsked(NotAsked::PersonSocket);
    }
    let now = Instant::now();
    let question = PanelQuestion::of(view, now);
    if !crate::localapi_state::is_hex_object_id(&view.session_hex) || question.left_ms == 0 {
        return SessionPanel::NotAsked(NotAsked::NotAQuestion);
    }
    match road.ask(&view.session_hex, question, PANEL_WITHIN).await {
        Asked::Told(state) => {
            if SILENT.swap(false, Ordering::SeqCst) {
                tracing::info!("consent panel: the screen worker answers the root daemon again");
            }
            match state {
                PanelState::Shown => SessionPanel::Shown,
                PanelState::Busy => SessionPanel::Busy,
                PanelState::NotUp { why } => SessionPanel::NotUp(why),
            }
        }
        Asked::NoAnswer => {
            if !SILENT.swap(true, Ordering::SeqCst) {
                tracing::warn!(
                    uid,
                    within = ?PANEL_WITHIN,
                    "consent panel: the screen worker gave no answer in time; this question and \
                     each one it misses go to the person's app until it answers again (FR-149)"
                );
            }
            SessionPanel::NoAnswer
        }
    }
}

/// Where one question went, for its per-prompt line: `native`, then
/// `panel=session|root` when a panel shows it, or `session_panel=<…>
/// why=<…>` when the session panel did not. Every field but `native` is
/// absent off a one-row Mac's root daemon, so those lines read as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompted {
    pub native: bool,
    panel: Option<&'static str>,
    session_panel: Option<&'static str>,
    why: Option<String>,
}

impl Prompted {
    /// A daemon the session panel is not for: its own panel's answer alone.
    pub fn plain(native: bool) -> Self {
        Self {
            native,
            panel: None,
            session_panel: None,
            why: None,
        }
    }

    /// The `panel` field: `session` or `root`, when a panel shows it.
    pub fn panel(&self) -> Option<tracing::field::DisplayValue<&'static str>> {
        self.panel.map(tracing::field::display)
    }

    /// The `session_panel` field: `busy`, `not_up`, `no_answer` or
    /// `not_asked`, when no panel shows it on a one-row Mac's root daemon.
    pub fn session_panel(&self) -> Option<tracing::field::DisplayValue<&'static str>> {
        self.session_panel.map(tracing::field::display)
    }

    /// The `why` field, beside `session_panel`.
    pub fn why(&self) -> Option<tracing::field::DisplayValue<&str>> {
        self.why.as_deref().map(tracing::field::display)
    }

    /// Whether the session panel shows the question.
    pub fn on_the_session_panel(&self) -> bool {
        self.panel == Some("session")
    }
}

/// The chain for one question: the session panel, then the root daemon's own
/// panel (`root_panel`, D2), and `native` from whichever shows it. Off a
/// one-row Mac's root daemon, `root_panel` alone, as before FR-149.
///
/// At most one panel on the screen. Once the session panel was asked,
/// `root_panel` is never called, and a busy or silent session panel leads to
/// the app. Across questions too: while the root daemon's own panel shows
/// another question the session panel is not asked, and while the session
/// panel shows another the root daemon's own is not tried. Each is then the
/// road of a question asked while the panel is busy: the app.
pub async fn chain(
    road: &dyn Road,
    view: &PromptView,
    root_panel: impl FnOnce() -> bool,
) -> Prompted {
    if !road.one_row() {
        return Prompted::plain(root_panel());
    }
    let asked = if road.root_panel_up() {
        SessionPanel::NotAsked(NotAsked::RootPanelBusy)
    } else {
        ask(road, view).await
    };
    let (session_panel, why) = match asked {
        SessionPanel::Shown => {
            return Prompted {
                native: true,
                panel: Some("session"),
                session_panel: None,
                why: None,
            };
        }
        SessionPanel::NotAsked(not) => {
            if !road.session_panel_up() && root_panel() {
                return Prompted {
                    native: true,
                    panel: Some("root"),
                    session_panel: None,
                    why: None,
                };
            }
            ("not_asked", not.why().to_string())
        }
        SessionPanel::Busy => ("busy", "the session panel shows another question".into()),
        SessionPanel::NotUp(why) => ("not_up", why),
        SessionPanel::NoAnswer => ("no_answer", format!("none within {PANEL_WITHIN:?}")),
    };
    Prompted {
        native: false,
        panel: None,
        session_panel: Some(session_panel),
        why: Some(why),
    }
}

/// The worker's gate for a root daemon's question, before its panel goes up:
/// `/dev/console`'s owner is the worker's own account (the root daemon's read,
/// made again: a switch between the two gives `not_up`), the panel shows no
/// other question, the id is a 24-hex ObjectId, and time is left. The window
/// device is read after it, by the panel's own window (`indicator::mac`),
/// the one caller outside the tests.
#[cfg(any(test, all(target_os = "macos", feature = "viewer-indicator-macos")))]
pub fn worker_gate(
    own_uid: u32,
    console_owner: Option<u32>,
    panel_free: bool,
    id: &str,
    left: Duration,
) -> Result<(), PanelState> {
    if console_owner != Some(own_uid) {
        return Err(PanelState::NotUp {
            why: "not_at_the_console".into(),
        });
    }
    if !crate::localapi_state::is_hex_object_id(id) {
        return Err(PanelState::NotUp {
            why: "not_an_id".into(),
        });
    }
    if left.is_zero() {
        return Err(PanelState::NotUp {
            why: "out_of_time".into(),
        });
    }
    if !panel_free {
        return Err(PanelState::Busy);
    }
    Ok(())
}

/// The worker's last-said state of its panel for the root daemon: each change
/// is logged once (up, not up and why), never a line per question. The worker
/// (`delegate_worker`) is unix-only, and so is the worker's half here.
#[cfg(unix)]
#[derive(Default)]
pub struct WorkerSays {
    last: std::sync::Mutex<Option<String>>,
}

#[cfg(unix)]
impl WorkerSays {
    /// `state` was told for a question; a change from the last one is said.
    pub fn told(&self, state: &PanelState) {
        let now = match state {
            PanelState::Shown | PanelState::Busy => "up".to_string(),
            PanelState::NotUp { why } => format!("not_up:{why}"),
        };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_deref() == Some(now.as_str()) {
            return;
        }
        match state {
            PanelState::NotUp { why } => tracing::warn!(
                %why,
                "consent panel: this worker's panel cannot show the root daemon's questions; \
                 they go to the person's app (FR-149)"
            ),
            _ => tracing::info!(
                "consent panel: this worker's panel shows the root daemon's questions (FR-149)"
            ),
        }
        *last = Some(now);
    }
}

/// The worker's answer to a root daemon's question. On a Mac whose build has
/// the panel, [`worker_gate`] and then the panel's own window
/// (`indicator::mac::show_root_question`); elsewhere `not_up`, though such a
/// worker never says [`CONSENT_PANEL`] and is never asked.
#[cfg(unix)]
pub async fn answer(id: String, question: PanelQuestion) -> PanelState {
    #[cfg(all(target_os = "macos", feature = "viewer-indicator-macos"))]
    {
        // `show_root_question` waits for the main thread, so off the runtime.
        tokio::task::spawn_blocking(move || super::mac::show_root_question(&id, &question))
            .await
            .unwrap_or_else(|e| PanelState::NotUp {
                why: format!("its show ended with no answer: {e}"),
            })
    }
    #[cfg(not(all(target_os = "macos", feature = "viewer-indicator-macos")))]
    {
        let _ = (id, question);
        PanelState::NotUp {
            why: "no_panel_in_this_build".into(),
        }
    }
}

/// The worker takes a root daemon's question down: `PanelHide { id }`, or
/// `None` for whichever root question is up, when the channel ends. Its own
/// questions stay. The panel also takes a root question down by itself when
/// its time is up (`indicator::mac`), so a lost `PanelHide` leaves nothing
/// past the window.
#[cfg(unix)]
pub fn take_down(id: Option<&str>) {
    #[cfg(all(target_os = "macos", feature = "viewer-indicator-macos"))]
    super::mac::dismiss_root_question(id);
    #[cfg(not(all(target_os = "macos", feature = "viewer-indicator-macos")))]
    let _ = id;
}

/// Whether this worker hosts the panel for its root daemon, and so says
/// [`CONSENT_PANEL`]: a one-row Mac's worker (no enrollment), in a build with
/// the macOS panel, with the switch on.
pub fn hosts_the_panel(unenrolled: bool) -> bool {
    unenrolled && cfg!(all(target_os = "macos", feature = "viewer-indicator-macos")) && enabled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const ID: &str = "414900000000000000000a01";

    fn view(id: &str, left: Duration) -> PromptView {
        PromptView {
            session_hex: id.into(),
            title: "Remote control request".into(),
            lead: "Alice is requesting to control this device.".into(),
            detail: String::new(),
            permissions: "VIEW|INPUT".into(),
            org: String::new(),
            expires_at: Instant::now() + left,
        }
    }

    /// The root daemon's reads, stood in, and what its asks were.
    struct StoodIn {
        one_row: bool,
        switch: bool,
        worker: Option<(u32, bool)>,
        console: Option<u32>,
        served: Option<u32>,
        root_up: bool,
        session_up: bool,
        told: Asked,
        asked: Mutex<Vec<String>>,
    }

    impl StoodIn {
        /// A one-row Mac whose worker for uid 501 said the word, at the
        /// console, served by the person socket, with no panel up, and says
        /// `told`.
        fn one_row(told: Asked) -> Self {
            Self {
                one_row: true,
                switch: true,
                worker: Some((501, true)),
                console: Some(501),
                served: Some(501),
                root_up: false,
                session_up: false,
                told,
                asked: Mutex::new(Vec::new()),
            }
        }
        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl Road for StoodIn {
        fn one_row(&self) -> bool {
            self.one_row
        }
        fn switch_on(&self) -> bool {
            self.switch
        }
        fn worker(&self) -> Option<(u32, bool)> {
            self.worker
        }
        fn console_owner(&self) -> Option<u32> {
            self.console
        }
        fn person_socket_serves(&self) -> Option<u32> {
            self.served
        }
        fn root_panel_up(&self) -> bool {
            self.root_up
        }
        fn session_panel_up(&self) -> bool {
            self.session_up
        }
        async fn ask(&self, id: &str, _q: PanelQuestion, within: Duration) -> Asked {
            assert_eq!(within, PANEL_WITHIN);
            self.asked.lock().unwrap().push(id.to_string());
            self.told.clone()
        }
    }

    /// FR-149 AC2 — `native` comes only from the worker's `shown`, within
    /// the wait: `busy`, `not_up`, silence, no worker, no word, the switch
    /// off and a person socket not served give no `native` from the session
    /// panel. The root daemon's own panel is out of the way here (`false`).
    #[tokio::test]
    async fn native_comes_only_from_the_session_processs_word() {
        let v = view(ID, Duration::from_secs(30));
        let shown = StoodIn::one_row(Asked::Told(PanelState::Shown));
        let p = chain(&shown, &v, || false).await;
        assert!(p.native && p.on_the_session_panel(), "{p:?}");
        assert_eq!(shown.asked(), 1);

        let not_up = || {
            Asked::Told(PanelState::NotUp {
                why: "no_window_device".into(),
            })
        };
        let mut cases: Vec<(&str, StoodIn, &str)> = vec![
            (
                "busy",
                StoodIn::one_row(Asked::Told(PanelState::Busy)),
                "busy",
            ),
            ("not up", StoodIn::one_row(not_up()), "not_up"),
            ("silent", StoodIn::one_row(Asked::NoAnswer), "no_answer"),
        ];
        let mut no_worker = StoodIn::one_row(Asked::Told(PanelState::Shown));
        no_worker.worker = None;
        cases.push(("no worker", no_worker, "not_asked"));
        let mut no_word = StoodIn::one_row(Asked::Told(PanelState::Shown));
        no_word.worker = Some((501, false));
        cases.push(("no word", no_word, "not_asked"));
        let mut off = StoodIn::one_row(Asked::Told(PanelState::Shown));
        off.switch = false;
        cases.push(("switch off", off, "not_asked"));
        let mut unserved = StoodIn::one_row(Asked::Told(PanelState::Shown));
        unserved.served = None;
        cases.push(("person socket", unserved, "not_asked"));
        for (case, road, said) in &cases {
            let p = chain(road, &v, || false).await;
            assert!(!p.native, "{case}: {p:?}");
            assert_eq!(p.session_panel, Some(*said), "{case}");
            assert!(p.why.is_some(), "{case}: a why");
        }
        // The word alone, an attached worker or a seat read never ask
        // anything that comes back `native`: those four were not even asked.
        for (case, road, said) in &cases {
            assert_eq!(road.asked(), usize::from(*said != "not_asked"), "{case}");
        }
    }

    /// FR-149 — only the person who owns `/dev/console` at the question is
    /// asked, whatever the worker said before.
    #[tokio::test]
    async fn only_the_person_at_the_console_is_asked() {
        let v = view(ID, Duration::from_secs(30));
        for console in [None, Some(502)] {
            let mut road = StoodIn::one_row(Asked::Told(PanelState::Shown));
            road.console = console;
            let p = chain(&road, &v, || false).await;
            assert!(!p.native, "{console:?}");
            assert_eq!(
                (p.session_panel, p.why.as_deref()),
                (Some("not_asked"), Some("not_at_the_console")),
                "{console:?}"
            );
            assert_eq!(road.asked(), 0, "{console:?}: never asked");
        }
    }

    /// FR-149 — at most one panel: the root daemon's own panel is tried only
    /// when the session panel was not asked, never after a busy, refused or
    /// silent one, which leads to the app; and across questions, neither
    /// panel takes one while the other shows another.
    #[tokio::test]
    async fn a_busy_or_silent_session_panel_never_leads_to_a_second_panel() {
        let v = view(ID, Duration::from_secs(30));
        for told in [
            Asked::Told(PanelState::Busy),
            Asked::NoAnswer,
            Asked::Told(PanelState::NotUp { why: "x".into() }),
        ] {
            let road = StoodIn::one_row(told.clone());
            let tried = AtomicBool::new(false);
            let p = chain(&road, &v, || {
                tried.store(true, Ordering::SeqCst);
                true
            })
            .await;
            assert!(!tried.load(Ordering::SeqCst), "{told:?}: a second panel");
            assert!(!p.native, "{told:?}");
        }
        // Not asked: the root daemon's own panel is the second choice (D2).
        let mut road = StoodIn::one_row(Asked::Told(PanelState::Shown));
        road.worker = None;
        let p = chain(&road, &v, || true).await;
        assert!(p.native && p.panel == Some("root"), "{p:?}");
        // Across questions: the root daemon's own panel shows another, so the
        // session panel is not asked, and the question goes on as one asked
        // while that panel is busy (its `false`).
        let mut road = StoodIn::one_row(Asked::Told(PanelState::Shown));
        road.root_up = true;
        let p = chain(&road, &v, || false).await;
        assert_eq!(road.asked(), 0, "a second panel asked for");
        assert!(!p.native, "{p:?}");
        assert_eq!(
            (p.session_panel, p.why.as_deref()),
            (Some("not_asked"), Some("root_panel_busy"))
        );
        // ...and the session panel shows another, so a question it was not
        // asked for does not go to the root daemon's own panel.
        let mut road = StoodIn::one_row(Asked::Told(PanelState::Shown));
        road.console = Some(502);
        road.session_up = true;
        let p = chain(&road, &v, || panic!("a second panel")).await;
        assert!(!p.native, "{p:?}");
        assert_eq!(p.session_panel, Some("not_asked"));
        // Off a one-row Mac's root daemon: its own panel, and no new field.
        let mut road = StoodIn::one_row(Asked::Told(PanelState::Shown));
        road.one_row = false;
        assert_eq!(chain(&road, &v, || true).await, Prompted::plain(true));
        assert_eq!(chain(&road, &v, || false).await, Prompted::plain(false));
        assert_eq!(road.asked(), 0);
    }

    /// The question crosses with its time left in milliseconds, and the
    /// worker counts down to the same end.
    #[test]
    fn a_question_crosses_with_its_time_left() {
        let now = Instant::now();
        let v = view(ID, Duration::from_secs(30));
        let q = PanelQuestion::of(&v, now);
        assert!(q.left_ms > 29_000 && q.left_ms <= 30_000, "{}", q.left_ms);
        let back = q.view(ID, now);
        assert_eq!(
            (back.title.as_str(), back.session_hex.as_str()),
            (v.title.as_str(), ID)
        );
        let wire = serde_json::to_value(&q).unwrap();
        assert!(wire["left_ms"].is_u64());
    }

    /// The worker's own gate: its account at the console, a free panel, an
    /// id, time left.
    #[test]
    fn the_workers_gate() {
        let left = Duration::from_secs(5);
        assert_eq!(worker_gate(501, Some(501), true, ID, left), Ok(()));
        assert!(matches!(
            worker_gate(501, Some(502), true, ID, left),
            Err(PanelState::NotUp { why }) if why == "not_at_the_console"
        ));
        assert!(matches!(
            worker_gate(501, None, true, ID, left),
            Err(PanelState::NotUp { .. })
        ));
        assert_eq!(
            worker_gate(501, Some(501), false, ID, left),
            Err(PanelState::Busy)
        );
        assert!(matches!(
            worker_gate(501, Some(501), true, "../etc", left),
            Err(PanelState::NotUp { why }) if why == "not_an_id"
        ));
        assert!(matches!(
            worker_gate(501, Some(501), true, ID, Duration::ZERO),
            Err(PanelState::NotUp { why }) if why == "out_of_time"
        ));
    }

    #[test]
    fn the_switch_reads_an_explicit_off_only() {
        for off in ["0", "false", "OFF", " no "] {
            assert!(!enabled_by_env(Some(off)), "{off}");
        }
        for on in [None, Some("1"), Some("yes"), Some("")] {
            assert!(enabled_by_env(on), "{on:?}");
        }
    }

    // ---- The delegation channel, a broker, and the app's road ----------

    use crate::consent::{ConsentBroker, Decision, Mode, PendingPrompt, PromptKind, PromptSurface};
    use crate::delegate::tests::{attached_worker, next_frame};
    use crate::delegate::{DelegateFrame, DelegateHost, write_frame};
    use tokio::io::AsyncWriteExt;

    type Lines =
        tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>;
    type Writer = tokio::io::WriteHalf<tokio::io::DuplexStream>;

    /// A one-row Mac's root daemon whose reads all say "ask", and whose ask
    /// is its delegation host's, over the channel a stood-in worker holds.
    struct HostRoad(DelegateHost);

    #[async_trait::async_trait]
    impl Road for HostRoad {
        fn one_row(&self) -> bool {
            true
        }
        fn switch_on(&self) -> bool {
            true
        }
        fn worker(&self) -> Option<(u32, bool)> {
            Some((501, self.0.says_consent_panel()))
        }
        fn console_owner(&self) -> Option<u32> {
            Some(501)
        }
        fn person_socket_serves(&self) -> Option<u32> {
            Some(501)
        }
        fn root_panel_up(&self) -> bool {
            false
        }
        fn session_panel_up(&self) -> bool {
            self.0.panel_shows_one()
        }
        async fn ask(&self, id: &str, question: PanelQuestion, within: Duration) -> Asked {
            self.0.ask_panel(id, question, within).await
        }
    }

    /// The stood-in worker says [`CONSENT_PANEL`] in its caps frame, and the
    /// daemon has read it (a ping answered after it).
    async fn says_the_word(lines: &mut Lines, cwr: &mut Writer) {
        let caps = DelegateFrame::WorkerCaps {
            caps: Default::default(),
            features: vec![CONSENT_PANEL.into()],
        };
        write_frame(cwr, &caps).await.unwrap();
        cwr.write_all(b"{\"t\":\"ping\"}\n").await.unwrap();
        assert!(matches!(next_frame(lines).await, DelegateFrame::Pong));
    }

    /// The daemon read every frame written before this: a ping answered.
    async fn read_so_far(lines: &mut Lines, cwr: &mut Writer) {
        cwr.write_all(b"{\"t\":\"ping\"}\n").await.unwrap();
        assert!(matches!(next_frame(lines).await, DelegateFrame::Pong));
    }

    async fn told(cwr: &mut Writer, id: &str, state: PanelState) {
        let frame = DelegateFrame::PanelTold {
            id: id.into(),
            state,
        };
        write_frame(cwr, &frame).await.unwrap();
    }

    /// The marker a question's caller writes once its surfaces are known.
    fn marker(window: Duration, surface: PromptSurface) -> PendingPrompt<'static> {
        PendingPrompt {
            kind: PromptKind::RemoteControl,
            asked_by: "viewer 2",
            permissions: "VIEW|INPUT".into(),
            detail: String::new(),
            org: String::new(),
            timeout: window,
            surface,
        }
    }

    /// The question's marker as the person's app reads it: on the books with
    /// the file channel off, else its `.pending` file.
    fn marker_of(
        broker: &ConsentBroker,
        id: &str,
    ) -> Option<tunnel_core::localapi::ConsentRequest> {
        match broker.pending_path(id) {
            None => broker
                .pending_in_memory()
                .into_iter()
                .find(|q| q.session_id == id),
            Some(path) => std::fs::read_to_string(path)
                .ok()
                .and_then(|b| serde_json::from_str(&b).ok()),
        }
    }

    /// A broker with no folder: its books hold each marker and answer.
    fn books_broker(window: Duration) -> ConsentBroker {
        ConsentBroker::on_own_folder(
            Mode::Prompt { timeout: window },
            Err(anyhow::anyhow!("a test's broker keeps its books in memory")),
        )
    }

    /// FR-149 — a `PanelTold` saying `shown` after its wait gets a
    /// `PanelHide`: the root daemon counted the question for the app, so a
    /// panel it did not count never stays up beside it.
    #[tokio::test]
    async fn a_late_shown_is_taken_down() {
        let host = DelegateHost::new();
        let (task, mut lines, mut cwr) = attached_worker(&host).await;
        says_the_word(&mut lines, &mut cwr).await;
        let road = HostRoad(host.clone());
        let v = view(ID, Duration::from_secs(30));
        let t0 = Instant::now();
        let p = chain(&road, &v, || panic!("a second panel")).await;
        assert!(t0.elapsed() >= PANEL_WITHIN, "the wait is PANEL_WITHIN");
        assert!(!p.native, "{p:?}");
        assert_eq!(p.session_panel, Some("no_answer"));
        match next_frame(&mut lines).await {
            DelegateFrame::PanelAsk { id, question } => {
                assert_eq!(id, ID);
                assert!(question.left_ms > 0 && question.left_ms <= 30_000);
            }
            other => panic!("expected the question, got {other:?}"),
        }
        told(&mut cwr, ID, PanelState::Shown).await;
        match next_frame(&mut lines).await {
            DelegateFrame::PanelHide { id } => assert_eq!(id, ID),
            other => panic!("expected a take-down, got {other:?}"),
        }
        assert!(!host.on_the_panel(ID), "never counted on the panel");
        drop(cwr);
        drop(lines);
        task.await.unwrap();
    }

    /// FR-149's invariant — a session process's word routes a question and
    /// never decides one: no `PanelTold`, in any state, asked or not, and no
    /// end of its channel, decides or ends a question. Only an answer that
    /// reaches the broker's gate (`record_decision`, where the LocalAPI's
    /// `consent_decide` and the person socket end) decides it. (P2's
    /// `ConsentPanelTold` joins this test when it lands.)
    #[tokio::test]
    async fn a_session_processs_word_never_decides_a_question() {
        let window = Duration::from_secs(30);
        let broker = books_broker(window);
        let asking = tokio::spawn(broker.request_with_mode(ID, Mode::Prompt { timeout: window }));
        broker
            .write_prompt(ID, &marker(window, PromptSurface::Native))
            .unwrap();
        let host = DelegateHost::new();
        host.set_consent_broker(broker.clone());
        let (task, mut lines, mut cwr) = attached_worker(&host).await;
        says_the_word(&mut lines, &mut cwr).await;
        let states = [
            PanelState::Shown,
            PanelState::Busy,
            PanelState::NotUp {
                why: "no_window_device".into(),
            },
        ];
        // Asked: each state is told, and the question still asks.
        for state in states.clone() {
            let h = host.clone();
            let q = PanelQuestion::of(&view(ID, window), Instant::now());
            let asked =
                tokio::spawn(async move { h.ask_panel(ID, q, Duration::from_secs(5)).await });
            assert!(matches!(
                next_frame(&mut lines).await,
                DelegateFrame::PanelAsk { .. }
            ));
            told(&mut cwr, ID, state.clone()).await;
            assert_eq!(asked.await.unwrap(), Asked::Told(state.clone()));
            if state == PanelState::Shown {
                host.hide_panel(ID);
                assert!(matches!(
                    next_frame(&mut lines).await,
                    DelegateFrame::PanelHide { .. }
                ));
            }
            assert!(broker.is_asking(ID), "{state:?} decided the question");
        }
        // Not asked: the same words, and an unasked `shown` comes down.
        for state in states {
            told(&mut cwr, ID, state.clone()).await;
            if state == PanelState::Shown {
                assert!(matches!(
                    next_frame(&mut lines).await,
                    DelegateFrame::PanelHide { .. }
                ));
            }
            read_so_far(&mut lines, &mut cwr).await;
            assert!(
                broker.is_asking(ID),
                "an unasked {state:?} decided the question"
            );
        }
        // Shown, then the channel ends: the question is the app's now (D4),
        // and still asks.
        let h = host.clone();
        let q = PanelQuestion::of(&view(ID, window), Instant::now());
        let asked = tokio::spawn(async move { h.ask_panel(ID, q, Duration::from_secs(5)).await });
        assert!(matches!(
            next_frame(&mut lines).await,
            DelegateFrame::PanelAsk { .. }
        ));
        told(&mut cwr, ID, PanelState::Shown).await;
        assert_eq!(asked.await.unwrap(), Asked::Told(PanelState::Shown));
        drop(cwr);
        drop(lines);
        task.await.unwrap();
        assert!(
            broker.is_asking(ID),
            "the channel's end decided the question"
        );
        assert!(
            !asking.is_finished(),
            "the question ended without an answer"
        );
        // The broker's gate decides it.
        assert!(broker.record_decision(ID, true));
        let decision = tokio::time::timeout(Duration::from_secs(3), asking)
            .await
            .expect("an answered question ends promptly")
            .unwrap();
        assert_eq!(decision, Decision::Granted);
    }

    /// FR-149 D4 — a channel that ends after `shown` marks the question for
    /// the app for the rest of its window: its marker says `companion`, with
    /// the deadline it had, by the rewrite #4000 makes for a notification,
    /// with the file channel and without it. Answers stay bound to its id.
    #[tokio::test]
    async fn a_channel_ended_after_shown_sends_the_question_to_the_app() {
        let window = Duration::from_secs(30);
        let dir = tempfile::tempdir().unwrap();
        let with_a_folder =
            ConsentBroker::new(Mode::Prompt { timeout: window }, dir.path().join("consent"))
                .unwrap();
        for (broker, what) in [
            (books_broker(window), "the books"),
            (with_a_folder, "a folder"),
        ] {
            let asking =
                tokio::spawn(broker.request_with_mode(ID, Mode::Prompt { timeout: window }));
            broker
                .write_prompt(ID, &marker(window, PromptSurface::Native))
                .unwrap();
            let before = marker_of(&broker, ID).expect("the question is marked");
            assert_eq!(before.surface, "native", "{what}");
            let host = DelegateHost::new();
            host.set_consent_broker(broker.clone());
            let (task, mut lines, mut cwr) = attached_worker(&host).await;
            says_the_word(&mut lines, &mut cwr).await;
            let h = host.clone();
            let q = PanelQuestion::of(&view(ID, window), Instant::now());
            let asked =
                tokio::spawn(async move { h.ask_panel(ID, q, Duration::from_secs(5)).await });
            assert!(matches!(
                next_frame(&mut lines).await,
                DelegateFrame::PanelAsk { .. }
            ));
            told(&mut cwr, ID, PanelState::Shown).await;
            assert_eq!(asked.await.unwrap(), Asked::Told(PanelState::Shown));
            assert!(host.on_the_panel(ID));
            drop(cwr);
            drop(lines);
            task.await.unwrap();
            let after = marker_of(&broker, ID).expect("still marked");
            assert_eq!(after.surface, "companion", "{what}: not the app's");
            assert_eq!(
                after.expires_at_ms, before.expires_at_ms,
                "{what}: for its remaining time, never a new window"
            );
            assert!(!host.on_the_panel(ID), "{what}");
            assert!(broker.is_asking(ID), "{what}");
            // Bound to its id: another id's answer counts for nothing.
            assert!(!broker.record_decision("414900000000000000000aff", true));
            assert!(broker.record_decision(ID, false), "{what}");
            let decision = tokio::time::timeout(Duration::from_secs(3), asking)
                .await
                .expect("an answered question ends promptly")
                .unwrap();
            assert_eq!(decision, Decision::Denied, "{what}");
            assert!(
                marker_of(&broker, ID).is_none(),
                "{what}: the marker is gone"
            );
        }
    }

    /// FR-149 D4 — once a question's look has decided it, a later answer for
    /// its id is refused, on the session panel's road as on the app's: on a
    /// one-row Mac, the app's answer and the panel's click each reach the
    /// root half's broker through the root's person socket, and whichever
    /// comes second finds nothing asked.
    #[tokio::test]
    async fn an_answer_for_a_decided_question_is_refused() {
        use tunnel_core::localapi::{LocalApiState, Response};
        let window = Duration::from_secs(30);
        let dirs = tempfile::tempdir().unwrap();
        let (root, worker_broker, worker) =
            crate::localapi_state::tests::one_row_mac(&dirs, window, false);
        let road: &dyn LocalApiState = &*worker;
        async fn decided(asking: impl std::future::Future<Output = Decision>) -> Decision {
            tokio::time::timeout(Duration::from_secs(3), asking)
                .await
                .expect("an answered question ends promptly")
        }

        // The app's answer first, then the panel's click.
        let first = "414900000000000000000c01";
        let asking = crate::localapi_state::tests::ask_on(&root, first, window);
        assert_eq!(
            road.consent_decide_view(first, true).await,
            Response::ConsentDecided { ok: true }
        );
        assert_eq!(decided(asking).await, Decision::Granted);
        assert!(
            !crate::consent_relay::panel_clicked(&worker_broker, Some(road), first, false).await,
            "the panel's click after the app's answer"
        );

        // The panel's click first, then the app's answer.
        let second = "414900000000000000000c02";
        let asking = crate::localapi_state::tests::ask_on(&root, second, window);
        assert!(
            crate::consent_relay::panel_clicked(&worker_broker, Some(road), second, false).await
        );
        assert_eq!(decided(asking).await, Decision::Denied);
        assert_ne!(
            road.consent_decide_view(second, true).await,
            Response::ConsentDecided { ok: true },
            "the app's answer after the panel's click"
        );
    }
}
