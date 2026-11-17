// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-93 P6f — the desktop's own notifications as the last consent surface.
//!
//! Where neither the daemon's panel nor the desktop companion can show a
//! question, it reached nobody. That is a system daemon on a GNOME or KDE
//! Wayland desktop with no companion package: Fedora, openSUSE and Arch, and
//! every aarch64 host. The controller waited, and was told
//! `no_prompt_surface`. Measured on the FR-93 lab (Plasma 6.7, a system install,
//! 0.4.135): *"no desktop companion on this host"*, and nothing on the screen.
//!
//! Every such desktop runs a notification server that takes buttons
//! (`org.freedesktop.Notifications` with the `actions` capability): GNOME
//! Shell's banners, KDE's popups. That is this surface. Its place in the chain
//! is last before the CLI: native panel → companion → notification → none.
//!
//! The notification is posted from INSIDE the person's session, by the session
//! helper (`portal-helper --notify`, spawned by
//! [`crate::capture::portal::helper::spawn_child`], so the session lookup and
//! the privilege drop exist once). The helper prints one marked line when the
//! notification is up and one for what the person did. An answer reaches the
//! broker the way a native panel's does, over the indicator's
//! [`ConsentSender`]: one decision point.
//!
//! - **Approve** and **Deny** are the answer. **Closing** the notification is a
//!   Deny: the person saw the question and did not approve it, and a question
//!   with nothing left on screen would otherwise stand until its window ran
//!   out, with the controller told "nobody answered".
//! - A click on the notification's **body** is not an answer. Servers send it
//!   as the spec's `default` action and then remove the notification (on
//!   GNOME 50 the removal alone arrived as a dismissal, which read as a
//!   refusal), so the helper posts the question again.
//! - **Critical urgency**: GNOME and KDE keep a critical notification on screen
//!   until it is answered, and show it under Do Not Disturb.
//! - **The daemon takes it down** (answered elsewhere, timed out, the session
//!   gone) by closing the helper's stdin. The helper closes the notification
//!   and exits, so no Approve button outlives its question. A press the
//!   server took before that close still reaches the daemon (#3942): after
//!   its close the helper reads the server's signals until the server says
//!   the notification closed, for half a second from the daemon's close at
//!   most, or a moment after a `Notify` that answered later than that.
//! - **A helper the daemon gives up on** (no "up" within 5 s) is let go the
//!   same way, and killed only if it is still there 1 s later (#3886): a
//!   notification it did post, its line late, comes down with it. It posts
//!   nothing once its stdin has closed, and what it posts expires by itself at
//!   the question's end, on a server that keeps the spec's `expire_timeout`.
//!
//! Kill switch `ROOMLERD_NOTIFY_CONSENT=0`.

use serde::{Deserialize, Serialize};

/// One notification, as the parent asks the helper to post it. The parent
/// writes the words; the helper only knows the protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub summary: String,
    pub body: String,
    /// `(key, label)` pairs, in the order shown.
    pub actions: Vec<(String, String)>,
    /// 0 low, 1 normal, 2 critical.
    pub urgency: u8,
    /// How long the helper keeps it up at most, from the request's arrival,
    /// and when the notification expires by itself (#3886). A backstop: the
    /// daemon closes it first.
    pub lifetime_ms: u64,
}

/// Marks the line saying the notification is up (with its id).
const SHOWN: &str = "ROOMLER_NOTIFY_SHOWN:";
/// Marks the line saying it could not be posted, and why.
const FAILED: &str = "ROOMLER_NOTIFY_FAILED:";
/// Marks the key of the button the person pressed.
const ACTION: &str = "ROOMLER_NOTIFY_ACTION:";
/// Marks the server's `NotificationClosed` reason.
const CLOSED: &str = "ROOMLER_NOTIFY_CLOSED:";

/// What the helper said, one marked line at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Said {
    Shown,
    Failed(String),
    Action(String),
    /// `NotificationClosed`'s reason: 1 expired, 2 dismissed by the person,
    /// 3 closed by `CloseNotification`, 4 undefined.
    Closed(u32),
}

/// The helper's stdout carries its own library chatter too, so lines are
/// matched by marker, never by position.
fn parse(line: &str) -> Option<Said> {
    let line = line.trim();
    if line.starts_with(SHOWN) {
        Some(Said::Shown)
    } else if let Some(why) = line.strip_prefix(FAILED) {
        Some(Said::Failed(why.to_string()))
    } else if let Some(key) = line.strip_prefix(ACTION) {
        Some(Said::Action(key.to_string()))
    } else {
        line.strip_prefix(CLOSED)
            .map(|r| Said::Closed(r.parse().unwrap_or(4)))
    }
}

/// The two buttons of a consent question.
const APPROVE: &str = "approve";
const DENY: &str = "deny";
/// The freedesktop spec's click on the body, which no server draws as a
/// button. Without it a click there closed the question as dismissed (GNOME
/// 50), which read as a refusal; with it, the helper posts it again.
const DEFAULT: &str = "default";

/// A consent question's answer, from what the person did: Approve, Deny, or
/// closing it (a Deny). Anything else (the server expired it, it was closed by
/// a call) answers nothing, and the broker's window decides.
fn consent_answer(said: &Said) -> Option<bool> {
    match said {
        Said::Action(k) if k == APPROVE => Some(true),
        Said::Action(k) if k == DENY => Some(false),
        Said::Closed(2) => Some(false),
        _ => None,
    }
}

/// The question in words: who asks and for what, on whose behalf, the
/// permissions, and when an unanswered question is refused.
fn consent_request(view: &super::PromptView, now: std::time::Instant) -> Request {
    let mut lines = vec![view.lead.clone()];
    if !view.org.is_empty() {
        lines.push(format!("On behalf of {}", view.org));
    }
    // The X11 panel's detail box holds two lines; a notification shows them
    // all, the same words.
    if !view.detail.is_empty() {
        lines.push(view.detail.clone());
    }
    if !view.permissions.is_empty() {
        lines.push(format!("Permissions: {}", view.permissions));
    }
    let left = view.expires_at.saturating_duration_since(now);
    lines.push(format!(
        "Unanswered, it is refused in {}.",
        in_words(left.as_secs())
    ));
    Request {
        summary: view.title.clone(),
        body: lines.join("\n"),
        actions: vec![
            (DEFAULT.to_string(), "Show".to_string()),
            (APPROVE.to_string(), "Approve".to_string()),
            (DENY.to_string(), "Deny".to_string()),
        ],
        urgency: 2,
        lifetime_ms: left.as_millis() as u64,
    }
}

/// "5 minutes", "1 minute", "30 seconds". A notification cannot count down, so
/// the window is said once, rounded.
fn in_words(secs: u64) -> String {
    if secs >= 90 {
        format!("{} minutes", (secs + 30) / 60)
    } else if secs >= 60 {
        "1 minute".to_string()
    } else {
        format!("{secs} seconds")
    }
}

/// `body-markup` servers read `&`, `<` and `>` as markup. A controller's name or
/// a command line must arrive as text.
fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// FR-93 P6f kill switch `ROOMLERD_NOTIFY_CONSENT=0`: no notification, as
/// before P6f.
fn enabled() -> bool {
    tunnel_core::env::flag("NOTIFY_CONSENT", true)
}

pub(super) use parent::Prompts;
pub(crate) use parent::{viewing, viewing_ended};

/// FR-93 P6g's button: end the session, as the companion banner's
/// **Disconnect** does.
const DISCONNECT: &str = "disconnect";

/// FR-93 P6g kill switch `ROOMLERD_VIEWING_NOTICE=0`: a session the compositor
/// says nothing about stays unseen at the screen, as before P6g.
fn viewing_enabled() -> bool {
    tunnel_core::env::flag("VIEWING_NOTICE", true)
}

/// Marks the probe's answer: `yes`, or `no` and why.
const CAN: &str = "ROOMLER_NOTIFY_CAN:";

/// How long the probe may take: a session bus round trip, and the start of a
/// notification server that is activated on demand.
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// FR-95 P2 (decision D-b) — can the person in the session on screen (`uid`)
/// be told they are being viewed, and end it? FR-93 P6g's notice is that
/// surface: the companion's banner where one runs for them, else a
/// notification with Disconnect. Asked LIVE, at the capture's open: a
/// companion that is installed but not running for them is not counted on
/// its own, and a session whose notification server shows no buttons is no
/// surface. `false` with the notice switched off (`ROOMLERD_VIEWING_NOTICE=0`).
/// Blocking: a helper is started in the session.
pub(crate) fn can_tell(uid: u32) -> bool {
    if !viewing_enabled() {
        return false;
    }
    if crate::companion::companion_running_for_uid(uid) {
        return true;
    }
    use crate::capture::portal::helper::{SpawnError, spawn_in_session};
    match spawn_in_session(&["--notify-probe"], PROBE_BUDGET) {
        Ok(text) => {
            let said = text.lines().find_map(|l| l.trim().strip_prefix(CAN));
            if said.is_some_and(|s| s.starts_with("yes")) {
                return true;
            }
            tracing::info!(
                said = said.unwrap_or("nothing"),
                "no way to tell the person at the screen they are viewed (FR-95 P2)"
            );
            false
        }
        Err(SpawnError::NoSession) => false,
        Err(SpawnError::Other(why)) => {
            tracing::info!(%why, "the notification probe did not run (FR-95 P2)");
            false
        }
    }
}

/// The probe's session half: `roomlerd portal-helper --notify-probe`. Posts
/// nothing. On a plain thread, for the reason [`run_helper`] gives.
pub fn run_probe_helper() {
    let answer = match std::thread::Builder::new()
        .name("notify-probe".into())
        .spawn(helper::probe)
    {
        Ok(worker) => worker
            .join()
            .unwrap_or_else(|_| Err("the probe's thread panicked".into())),
        Err(e) => Err(format!("no thread for the probe: {e}")),
    };
    // #2719 — never `println!`: the daemon that reads this answer can be gone
    // by now (a stop, an update), and a panic would be filed as a crash.
    match answer {
        Ok(()) => crate::logging::say_on_stdout(format_args!("{CAN}yes")),
        Err(why) => {
            crate::logging::say_on_stderr(format_args!("portal-helper --notify-probe: {why}"));
            crate::logging::say_on_stdout(format_args!("{CAN}no {why}"));
        }
    }
}

/// FR-93 P6g — who is viewing the screen, whether they can type into it, and
/// how to end it. Critical urgency, so it stays on screen for the session:
/// with the DRM arm nothing else at the screen says so.
fn viewing_request(controller: &str, permissions: &str) -> Request {
    // Equality on the pipe-separated names, never a substring: a name that
    // merely contains another is not that permission.
    let input = permissions.split('|').any(|p| p.trim() == "INPUT");
    let lead = if input {
        format!("{controller} is viewing this screen and can use its keyboard and mouse.")
    } else {
        format!("{controller} is viewing this screen.")
    };
    Request {
        summary: "This screen is being viewed".to_string(),
        body: format!("{lead}\nDisconnect ends the session."),
        actions: vec![
            (DEFAULT.to_string(), "Show".to_string()),
            (DISCONNECT.to_string(), "Disconnect".to_string()),
        ],
        urgency: 2,
        // A backstop: the session's end takes it down first.
        lifetime_ms: 24 * 3600 * 1000,
    }
}

mod parent {
    use super::{
        DISCONNECT, Request, Said, consent_answer, consent_request, enabled, parse,
        viewing_enabled, viewing_request,
    };
    use crate::indicator::{ConsentSender, PromptView};
    use bson::oid::ObjectId;
    use std::collections::HashMap;
    use std::io::{BufRead, Write};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
    use std::time::Duration;

    /// How long posting may take: a session bus round trip or three. A wedged
    /// bus must not hold the prompt's window.
    const SHOW_BUDGET: Duration = Duration::from_secs(5);

    /// #3886 — what a helper the daemon gave up on has, from the close of its
    /// stdin, to take down a notification it did post and exit: once its
    /// `Notify` has answered, its `CloseNotification`, and (#3942) its read of
    /// a press the server took before that close: half of this at most
    /// (`helper::TAKE_DOWN_WAIT`), or a moment after a later answer
    /// (`helper::TAKE_DOWN_LEAST`). Only then is it killed. A press it passes
    /// on meanwhile still answers the question. Counted on a thread of its
    /// own ([`let_go`]), after the verdict: nothing waits for it.
    pub(super) const GIVE_UP_GRACE: Duration = Duration::from_secs(1);

    /// The consent questions this daemon has on screen as notifications.
    #[derive(Clone)]
    pub(crate) struct Prompts {
        consent_tx: ConsentSender,
        /// The notification per question, on its way up or up: dropping an
        /// `Up` entry's stdin is what takes it down. The number tells a
        /// re-asked question from the one before it.
        live: Arc<Mutex<HashMap<String, (u64, Shown)>>>,
    }

    /// #3857 — a consent question's notification.
    enum Shown {
        /// Its show is running (the session walk, the helper's start, the
        /// helper's first line). Taken off the books meanwhile
        /// ([`Prompts::dismiss`]: the question answered elsewhere, its
        /// session ended, its socket closed), the show stops: no helper is
        /// started for it, or the one started takes it down at once.
        Coming,
        /// Up. Dropping the helper's stdin takes it down.
        Up(std::process::ChildStdin),
    }

    /// #3857 — why a notification is not up for a question that ended while
    /// it was on its way.
    const WITHDRAWN: &str = "its question ended while the notification was being put up";

    impl Prompts {
        pub(crate) fn new(consent_tx: ConsentSender) -> Self {
            Self {
                consent_tx,
                live: Arc::default(),
            }
        }

        /// #3857 — `session_hex`'s question on the books as coming, before its
        /// show starts, so a dismiss from here on stops the show. The number
        /// [`Self::show`] takes. Never waits.
        pub(crate) fn coming(&self, session_hex: &str) -> u64 {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let generation = NEXT.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut l) = self.live.lock() {
                l.insert(session_hex.to_string(), (generation, Shown::Coming));
            }
            generation
        }

        /// Put `view` on the person's screen as a notification with Approve
        /// and Deny, for the question [`Self::coming`] numbered `generation`:
        /// `Ok` once it is up, or why it is not. Blocks for the helper's first
        /// line, so callers run it off the runtime.
        pub(crate) fn show(&self, view: &PromptView, generation: u64) -> Result<(), String> {
            let shown = self.post(view, generation);
            if let Err(why) = &shown {
                // This question's entry only: a re-asked question under the
                // same id has its own.
                self.forget(&view.session_hex, generation);
                tracing::info!(session = %view.session_hex, %why, "no notification prompt (FR-93 P6f)");
            }
            shown
        }

        /// [`Self::show`]'s work, its entry left to the caller on a failure.
        fn post(&self, view: &PromptView, generation: u64) -> Result<(), String> {
            if !enabled() {
                return Err("notification prompts are off (ROOMLERD_NOTIFY_CONSENT=0)".into());
            }
            let now = std::time::Instant::now();
            // A question already over is not posted: nothing would take it
            // down but its own lifetime.
            if view.expires_at <= now {
                return Err("its question is already over".into());
            }
            let session = view.session_hex.clone();
            let books = self.live.clone();
            let asked = session.clone();
            let still_wanted = move || {
                books.lock().is_ok_and(|l| {
                    l.get(&asked).is_some_and(|(g, shown)| {
                        *g == generation && matches!(shown, Shown::Coming)
                    })
                })
            };
            let on_said = answer_to(self.consent_tx.clone(), session.clone());
            let live = self.live.clone();
            let end_session = session.clone();
            let on_end = move || {
                // This question's entry only: a re-asked question under the
                // same id has its own.
                if let Ok(mut l) = live.lock()
                    && l.get(&end_session).is_some_and(|(g, _)| *g == generation)
                {
                    l.remove(&end_session);
                }
            };
            let stdin = launch(
                &consent_request(view, now),
                "prompt",
                on_said,
                on_end,
                still_wanted,
            )?;
            let late = {
                let mut l = self.live.lock().unwrap_or_else(|p| p.into_inner());
                match l.get_mut(&session) {
                    Some((g, shown)) if *g == generation && matches!(shown, Shown::Coming) => {
                        *shown = Shown::Up(stdin);
                        None
                    }
                    _ => Some(stdin),
                }
            };
            match late {
                None => Ok(()),
                // #3857 — its question ended while it was on its way up. The
                // stdin goes now, so the helper closes the notification and
                // exits, and this late "up" answers no one.
                Some(stdin) => {
                    drop(stdin);
                    Err(format!("{WITHDRAWN}; it was taken down at once"))
                }
            }
        }

        /// This question's entry, unless a newer question under its id
        /// already took its place.
        fn forget(&self, session_hex: &str, generation: u64) {
            if let Ok(mut l) = self.live.lock()
                && l.get(session_hex).is_some_and(|(g, _)| *g == generation)
            {
                l.remove(session_hex);
            }
        }

        /// #3857 — whether the notification for `session_hex` is up: what a
        /// test reads to know nothing is left on the screen.
        #[cfg(test)]
        pub(crate) fn is_up(&self, session_hex: &str) -> bool {
            self.live
                .lock()
                .is_ok_and(|l| matches!(l.get(session_hex), Some((_, Shown::Up(_)))))
        }

        /// Take the notification for `session_hex` down, if one is up: its
        /// helper reads the close of its stdin, closes the notification and
        /// exits. Idempotent.
        pub(crate) fn dismiss(&self, session_hex: &str) {
            if let Ok(mut l) = self.live.lock() {
                l.remove(session_hex);
            }
        }
    }

    /// What the person did with a consent question's notification, as its
    /// helper's reader passes it on: the answer goes to `consent_tx` under
    /// `session`, once, the way a native panel's does. The broker takes it
    /// only while the question is still asked.
    fn answer_to(consent_tx: ConsentSender, session: String) -> impl FnMut(&Said) + Send + 'static {
        let mut answered = false;
        move |said: &Said| {
            let answer = consent_answer(said);
            tracing::info!(
                %session, ?said, ?answer,
                "the person at the screen acted on the notification prompt (FR-93 P6f)"
            );
            if let Some(allow) = answer
                && !answered
            {
                answered = true;
                // The reader's own thread, never the runtime's:
                // `blocking_send` is safe here.
                let _ = consent_tx.blocking_send((session.clone(), allow));
            }
        }
    }

    /// Post `req` from inside the person's session and wait, at most
    /// [`SHOW_BUDGET`], for it to be up. What the person then does goes to
    /// `on_said`, on the reader's own thread, and `on_end` runs once the helper
    /// has gone. The returned stdin is the notification's life: dropping it
    /// takes the notification down. Blocks, so callers run it off the runtime.
    ///
    /// #3857 — the session walk (1.5 s at most, one walk in flight), then the
    /// helper, one at a time: a question that ended while logind answered
    /// (`still_wanted` false) starts no helper.
    fn launch(
        req: &Request,
        what: &str,
        on_said: impl FnMut(&Said) + Send + 'static,
        on_end: impl FnOnce() + Send + 'static,
        still_wanted: impl Fn() -> bool,
    ) -> Result<std::process::ChildStdin, String> {
        use crate::capture::portal::helper::{SpawnError, helper_session, spawn_child_in};
        if !still_wanted() {
            return Err(WITHDRAWN.into());
        }
        // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
        let euid = unsafe { libc::geteuid() };
        let spawned = helper_session(euid).and_then(|session| {
            if !still_wanted() {
                return Err(SpawnError::Other(WITHDRAWN.into()));
            }
            spawn_child_in(&session, euid, &["--notify"], true)
        });
        let child = match spawned {
            Ok(c) => c,
            Err(SpawnError::NoSession) => return Err("no graphical session".into()),
            Err(SpawnError::Other(why)) => return Err(why),
        };
        put_up(child, req, what, on_said, on_end, SHOW_BUDGET)
    }

    /// #3886 — [`launch`]'s helper, once started: `req` written to its stdin,
    /// its lines read on a thread of their own, and its first line waited for
    /// `budget` at most ([`SHOW_BUDGET`]). A test hands it a stand-in.
    ///
    /// A helper given up on is let go the way the daemon takes any
    /// notification down: its stdin closes, so a notification it did post,
    /// its line still on the way, comes down too. It is killed only if it is
    /// still there [`GIVE_UP_GRACE`] later ([`let_go`]). The verdict, "not
    /// up", is returned at the give-up, as before. Its reader runs until the
    /// helper exits, so a press the helper passes on in that grace (#3942)
    /// still reaches `on_said`.
    fn put_up(
        mut child: std::process::Child,
        req: &Request,
        what: &str,
        mut on_said: impl FnMut(&Said) + Send + 'static,
        on_end: impl FnOnce() + Send + 'static,
        budget: Duration,
    ) -> Result<std::process::ChildStdin, String> {
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the session helper has no pipes".into());
        };
        // The request, and then nothing: stdin stays open, and its close is
        // the daemon's "take it down".
        let written = serde_json::to_string(req)
            .map_err(|e| e.to_string())
            .and_then(|l| {
                writeln!(stdin, "{l}")
                    .and_then(|_| stdin.flush())
                    .map_err(|e| e.to_string())
            });
        if let Err(e) = written {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("writing the request: {e}"));
        }
        let (shown_tx, shown_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let pid = child.id();
        let ended = Arc::new(Ended::default());
        let reader_ended = ended.clone();
        let reader = std::thread::Builder::new()
            .name(format!("notify-{what}"))
            .spawn(move || {
                let mut shown = Some(shown_tx);
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    match parse(&line) {
                        Some(Said::Shown) => {
                            if let Some(s) = shown.take() {
                                let _ = s.send(Ok(()));
                            }
                        }
                        Some(Said::Failed(why)) => {
                            if let Some(s) = shown.take() {
                                let _ = s.send(Err(why));
                            }
                        }
                        Some(said) => on_said(&said),
                        None => {}
                    }
                }
                // Before the reap: from here on no kill is sent to its pid.
                reader_ended.mark();
                if let Some(s) = shown.take() {
                    let _ = s.send(Err("the helper ended without posting it".into()));
                }
                on_end();
                let _ = child.wait();
            });
        if reader.is_err() {
            // The child was moved into the closure that never ran; its stdin
            // closing ends it.
            return Err("no thread to read the session helper".into());
        }
        match shown_rx.recv_timeout(budget) {
            Ok(Ok(())) => Ok(stdin),
            Ok(Err(why)) => Err(why),
            Err(_) => {
                // #3886 — the daemon's "take it down": a helper whose
                // notification is up, its line late, closes it and exits,
                // passing on a press the server took before the close
                // (#3942).
                drop(stdin);
                let_go(pid, ended, GIVE_UP_GRACE);
                Err(format!(
                    "the session helper did not post it within {} s",
                    budget.as_secs()
                ))
            }
        }
    }

    /// #3886 — whether a helper's stdout has closed: it has exited, or is
    /// exiting, and its reader reaps it next. Marked under the lock BEFORE the
    /// reap, so a kill sent under the lock while it is unmarked reaches our
    /// own unreaped child (a zombie at worst), never a process that took its
    /// pid.
    #[derive(Default)]
    struct Ended {
        ended: Mutex<bool>,
        marked: Condvar,
    }

    impl Ended {
        /// Its stdout closed: no kill is sent to its pid from here on.
        fn mark(&self) {
            *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.marked.notify_all();
        }

        /// `SIGKILL` to `pid`, unless its stdout closes within `grace`.
        fn kill_unless_ended_within(&self, pid: u32, grace: Duration) {
            let ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
            let (ended, _) = self
                .marked
                .wait_timeout_while(ended, grace, |ended| !*ended)
                .unwrap_or_else(PoisonError::into_inner);
            if !*ended {
                // Unmarked, and the lock held: not yet reaped, so the pid is
                // still this child's.
                // SAFETY: plain `kill(2)` on our own unreaped child.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }

    /// #3886 — a helper given up on, its stdin already closed: it takes down a
    /// notification it did post, and exits. Killed if it is still there
    /// `grace` later, on a thread of its own, so nothing waits for it.
    fn let_go(pid: u32, ended: Arc<Ended>, grace: Duration) {
        let waiting = ended.clone();
        let spawned = std::thread::Builder::new()
            .name("notify-let-go".into())
            .spawn(move || waiting.kill_unless_ended_within(pid, grace));
        if spawned.is_err() {
            // No thread to wait on: killed at once, as before #3886.
            ended.kill_unless_ended_within(pid, Duration::ZERO);
        }
    }

    /// FR-93 P6g — what the daemon has put up to say a session is viewed.
    enum Viewing {
        /// Being put up: the companion asked first, then the notification.
        Pending,
        /// The companion runs, and its banner lists every live session.
        Companion,
        /// The notification is up; dropping this takes it down.
        Live(std::process::ChildStdin),
        /// The person closed it: not posted again for this session.
        Closed,
    }

    /// How long a being-viewed notice waits for its session to be registered:
    /// 60 × 500 ms. A session still unregistered after that never connected.
    const REGISTERED_POLLS: u32 = 60;
    const REGISTERED_POLL: Duration = Duration::from_millis(500);

    fn viewings() -> &'static Mutex<HashMap<ObjectId, Viewing>> {
        static V: OnceLock<Mutex<HashMap<ObjectId, Viewing>>> = OnceLock::new();
        V.get_or_init(Default::default)
    }

    fn with_viewings<T>(f: impl FnOnce(&mut HashMap<ObjectId, Viewing>) -> T) -> T {
        f(&mut viewings().lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// FR-93 P6g — `session` is served by an arm the compositor shows nothing
    /// for: DRM reads the scanout below it, so neither GNOME's pill nor KDE's
    /// tray icon appears, and the person at the screen could be watched with
    /// nothing saying so. Say it: the companion's banner where a companion can
    /// run (it lists every live session), else a notification naming the
    /// controller, with **Disconnect**. Once per session; called on the runtime.
    pub(crate) fn viewing(session: ObjectId, registry: crate::rc_sessions::RcSessionRegistry) {
        if !viewing_enabled() {
            return;
        }
        let first = with_viewings(|v| {
            if v.contains_key(&session) {
                return false;
            }
            v.insert(session, Viewing::Pending);
            true
        });
        if !first {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            with_viewings(|v| v.remove(&session));
            return;
        };
        rt.spawn(async move {
            if crate::companion::ensure_running().await {
                tracing::info!(%session, "being viewed: the companion's banner says so (FR-93 P6g)");
                with_viewings(|v| {
                    if matches!(v.get(&session), Some(Viewing::Pending)) {
                        v.insert(session, Viewing::Companion);
                    }
                });
                return;
            }
            // The pump opens its capture as the peer is built, a moment before
            // the session is registered (once its answer is ready): wait for
            // that, and never for a session that does not get there. Measured
            // in the lab: the first build looked once, too early, and showed
            // nothing.
            let hex = session.to_hex();
            let mut found = None;
            for _ in 0..REGISTERED_POLLS {
                found = registry.list().into_iter().find(|i| i.session_id == hex);
                if found.is_some() || !with_viewings(|v| v.contains_key(&session)) {
                    break;
                }
                tokio::time::sleep(REGISTERED_POLL).await;
            }
            let Some(info) = found else {
                tracing::info!(%session, "no being-viewed notification: the session never got to be registered (FR-93 P6g)");
                with_viewings(|v| v.remove(&session));
                return;
            };
            let req = viewing_request(&info.controller_name, &info.permissions);
            let closed = Arc::new(AtomicBool::new(false));
            let (closed_said, closed_end) = (closed.clone(), closed);
            let on_said = move |said: &Said| match said {
                Said::Action(k) if k == DISCONNECT => {
                    let ended = registry.disconnect(&session, crate::rc_sessions::Via::Notification);
                    tracing::info!(%session, ended, "the person at the screen pressed Disconnect on the being-viewed notification (FR-93 P6g)");
                }
                Said::Closed(2) => closed_said.store(true, Ordering::Relaxed),
                _ => {}
            };
            let on_end = move || {
                // Ended by the person's close: remembered, so a reopen of the
                // capture does not put it back. Any other end leaves the entry
                // to `viewing_ended`.
                if closed_end.load(Ordering::Relaxed) {
                    with_viewings(|v| {
                        if v.contains_key(&session) {
                            v.insert(session, Viewing::Closed);
                        }
                    });
                }
            };
            // Its session's end is read after the show (`Viewing::Pending`
            // below), so the show itself is always wanted.
            let posted = tokio::task::spawn_blocking(move || {
                launch(&req, "viewing", on_said, on_end, || true)
            })
            .await;
            match posted {
                Ok(Ok(stdin)) => {
                    tracing::info!(%session, "being viewed: a notification says so, with Disconnect (FR-93 P6g)");
                    // A session that ended while it was being posted drops it
                    // here, which takes the notification down at once.
                    with_viewings(|v| {
                        if matches!(v.get(&session), Some(Viewing::Pending)) {
                            v.insert(session, Viewing::Live(stdin));
                        }
                    });
                }
                Ok(Err(why)) => {
                    tracing::info!(%session, %why, "no being-viewed notification (FR-93 P6g)");
                    with_viewings(|v| v.remove(&session));
                }
                Err(e) => {
                    tracing::warn!(%session, %e, "no being-viewed notification (FR-93 P6g)");
                    with_viewings(|v| v.remove(&session));
                }
            }
        });
    }

    /// FR-93 P6g — `session` ended: its notification, if one is up, comes
    /// down. Idempotent.
    pub(crate) fn viewing_ended(session: &ObjectId) {
        with_viewings(|v| v.remove(session));
    }

    /// #3886 — a session helper the daemon gives up on, stood in by `sh` and
    /// handed to [`put_up`] as `launch` hands it the real one. The stand-in's
    /// notification is a file: it posts it by creating it, and takes it down
    /// by removing it once its stdin closes, as the helper's
    /// `CloseNotification` does. The give-up waits `BUDGET`, not
    /// `SHOW_BUDGET`, to keep the suite quick.
    #[cfg(test)]
    mod given_up_tests {
        use super::{GIVE_UP_GRACE, Request, answer_to, put_up};
        use std::path::Path;
        use std::process::{Child, Command, Stdio};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        /// The tests' give-up: how long [`put_up`] waits for the first line.
        const BUDGET: Duration = Duration::from_secs(1);
        /// How late the verdict may come past the give-up on a busy runner:
        /// well under the helper's 1 s grace, which it never waits for.
        const AT_ONCE: Duration = Duration::from_millis(600);
        /// How soon after the give-up a helper is gone at the latest: its
        /// grace (1 s), then the kill, and a busy runner's slack.
        const GONE_WITHIN: Duration = Duration::from_secs(3);

        /// A consent question, as the daemon writes it to the helper.
        fn request() -> Request {
            Request {
                summary: "Remote control request".into(),
                body: "Alice is requesting to control this device.".into(),
                actions: vec![
                    ("approve".into(), "Approve".into()),
                    ("deny".into(), "Deny".into()),
                ],
                urgency: 2,
                lifetime_ms: 30_000,
            }
        }

        /// The stand-in: `script` run by `sh`, its notification the file
        /// `$UP`, its pipes those `launch` gives the helper.
        fn stand_in(script: &str, up: &Path) -> Child {
            Command::new("sh")
                .args(["-c", script])
                .env("UP", up)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("sh starts")
        }

        /// Whether `done` holds within `wait`.
        fn within(wait: Duration, done: impl Fn() -> bool) -> bool {
            let until = Instant::now() + wait;
            loop {
                if done() {
                    return true;
                }
                if Instant::now() >= until {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Whether `pid` is still in the process table: only the reap takes
        /// a process out, so a killed helper nobody waited on is still in it.
        fn in_process_table(pid: u32) -> bool {
            Path::new(&format!("/proc/{pid}")).exists()
        }

        /// #3886 — a helper whose notification is up, and whose line saying
        /// so comes only after the daemon gave up on it (the slow session
        /// bus the budget exists for). The verdict is "not shown", at the
        /// give-up. Then the notification comes down, and the helper is gone.
        ///
        /// Red before #3886: the helper was killed at the give-up, and its
        /// notification stayed on the screen, with buttons that reached no
        /// one.
        #[test]
        fn a_helper_given_up_on_takes_its_notification_down_and_is_gone() {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let up = dir.path().join("up");
            // Posts at once; its `Notify` answers only after the daemon has
            // given up on it. Then, as the helper does, it reads that its
            // stdin closed, takes its notification down and exits.
            let child = stand_in(
                r#"IFS= read -r request || exit 1
: > "$UP"
while IFS= read -r line; do :; done
sleep 0.2 </dev/null >/dev/null
echo "ROOMLER_NOTIFY_SHOWN:7"
rm -f "$UP"
"#,
                &up,
            );
            let pid = child.id();
            let asked = Instant::now();
            let shown = put_up(child, &request(), "test", |_| {}, || {}, BUDGET);
            let answered = asked.elapsed();
            assert!(
                up.exists(),
                "the stand-in's notification was up when the daemon gave up on it"
            );
            let why = shown.expect_err("a helper given up on is never reported up");
            assert!(why.contains("did not post it"), "the reason: {why}");
            assert!(
                answered < BUDGET + AT_ONCE,
                "the verdict came {answered:?} after the request, past the give-up"
            );
            assert!(
                within(GONE_WITHIN, || !up.exists()),
                "the notification of a helper given up on is still on the screen"
            );
            assert!(
                within(GONE_WITHIN, || !in_process_table(pid)),
                "the helper given up on is still in the process table"
            );
        }

        /// #3886's control — a helper that never answers and ignores its
        /// stdin's close is still killed, and is gone within the bound. The
        /// verdict is "not shown", at the give-up.
        #[test]
        fn a_helper_that_ignores_its_stdin_is_still_killed_within_the_bound() {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let up = dir.path().join("up");
            // Posts, never answers, and never reads its stdin again; its
            // stdout stays open with it.
            let child = stand_in(
                r#"IFS= read -r request || exit 1
: > "$UP"
exec sleep 30
"#,
                &up,
            );
            let pid = child.id();
            let asked = Instant::now();
            let shown = put_up(child, &request(), "test", |_| {}, || {}, BUDGET);
            let answered = asked.elapsed();
            let why = shown.expect_err("a helper given up on is never reported up");
            assert!(why.contains("did not post it"), "the reason: {why}");
            assert!(
                answered < BUDGET + AT_ONCE,
                "the verdict came {answered:?} after the request, past the give-up"
            );
            assert!(
                within(GONE_WITHIN, || !in_process_table(pid)),
                "a helper that ignores its stdin was not killed within {GONE_WITHIN:?} of the \
                 give-up"
            );
        }

        /// The control — a helper that posts in time is reported up, and is
        /// left alone past the give-up and its grace: the stdin it returns is
        /// the notification's life, and dropping it takes the notification
        /// down and ends the helper.
        #[test]
        fn a_helper_that_answers_in_time_is_up_until_its_stdin_closes() {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let up = dir.path().join("up");
            let child = stand_in(
                r#"IFS= read -r request || exit 1
: > "$UP"
echo "ROOMLER_NOTIFY_SHOWN:7"
while IFS= read -r line; do :; done
rm -f "$UP"
"#,
                &up,
            );
            let pid = child.id();
            let ended = Arc::new(AtomicBool::new(false));
            let on_end = {
                let ended = ended.clone();
                move || ended.store(true, Ordering::SeqCst)
            };
            let stdin = put_up(child, &request(), "test", |_| {}, on_end, BUDGET)
                .expect("a helper that answers in time is up");
            // Past the give-up and the grace a helper given up on would have.
            std::thread::sleep(BUDGET + GONE_WITHIN);
            assert!(up.exists(), "a notification that came up in time stays up");
            assert!(in_process_table(pid), "its helper still runs");
            assert!(!ended.load(Ordering::SeqCst), "its helper has not ended");
            drop(stdin);
            assert!(
                within(GONE_WITHIN, || !up.exists()),
                "dropping its stdin takes the notification down"
            );
            assert!(
                within(GONE_WITHIN, || !in_process_table(pid)),
                "and its helper exits"
            );
            assert!(ended.load(Ordering::SeqCst), "and its end is told");
        }

        /// #3942 — a press the helper passes on after the daemon gave up on
        /// it, within the grace, reaches the consent sender. Its question is
        /// still asked then (its window runs on after the verdict "not up"),
        /// so the broker takes the press as the answer.
        #[test]
        fn a_press_passed_on_after_the_give_up_reaches_the_consent_sender() {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let up = dir.path().join("up");
            // Posts at once; its `Notify` answers only after the daemon has
            // given up on it, and the person's press comes behind that
            // answer. As the helper does, it passes the press on and takes
            // its notification down.
            let child = stand_in(
                r#"IFS= read -r request || exit 1
: > "$UP"
while IFS= read -r line; do :; done
echo "ROOMLER_NOTIFY_SHOWN:7"
echo "ROOMLER_NOTIFY_ACTION:approve"
rm -f "$UP"
"#,
                &up,
            );
            let (consent_tx, mut consent_rx) = tokio::sync::mpsc::channel(4);
            let shown = put_up(
                child,
                &request(),
                "test",
                answer_to(consent_tx, "abc".into()),
                || {},
                BUDGET,
            );
            let gave_up = Instant::now();
            let why = shown.expect_err("a helper given up on is never reported up");
            assert!(why.contains("did not post it"), "the reason: {why}");
            let until = gave_up + GIVE_UP_GRACE;
            let answer = loop {
                use tokio::sync::mpsc::error::TryRecvError;
                match consent_rx.try_recv() {
                    Ok(answer) => break Some(answer),
                    // The reader is gone, and passed nothing on.
                    Err(TryRecvError::Disconnected) => break None,
                    Err(TryRecvError::Empty) if Instant::now() < until => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(TryRecvError::Empty) => break None,
                }
            };
            assert_eq!(
                answer,
                Some(("abc".to_string(), true)),
                "a press passed on after the give-up did not reach the consent sender within \
                 the grace"
            );
            assert!(
                within(GONE_WITHIN, || !up.exists()),
                "the notification of a helper given up on is still on the screen"
            );
        }
    }
}

/// The session helper's half: `roomlerd portal-helper --notify`. Reads one
/// [`Request`] line on stdin, posts it, says what happened on stdout, and
/// closes the notification when stdin closes or its lifetime ends. A stdin
/// that closed before the post (the daemon gave up on it, #3886) posts
/// nothing.
///
/// ⚠️ zbus's blocking API panics inside a tokio runtime ("Cannot start a
/// runtime from within a runtime"), and the subcommand is reached from the
/// daemon's runtime, so the D-Bus work runs on a plain thread: the rule keep
/// busy's helper and `capture::portal::mutter` follow. Measured on the FR-93
/// lab: called on the runtime's thread, the helper panicked before it posted
/// anything.
pub fn run_helper() {
    let outcome = match std::thread::Builder::new()
        .name("notify-helper".into())
        .spawn(helper::run)
    {
        Ok(worker) => worker
            .join()
            .unwrap_or_else(|_| Err("the helper's thread panicked".into())),
        Err(e) => Err(format!("no thread for the helper: {e}")),
    };
    // #2719 — as the probe's answer above: the reader may be gone.
    if let Err(why) = outcome {
        crate::logging::say_on_stderr(format_args!("portal-helper --notify: {why}"));
        crate::logging::say_on_stdout(format_args!("{FAILED}{why}"));
    }
}

mod helper {
    use super::{ACTION, CLOSED, DEFAULT, Request, SHOWN, escape_markup};
    use std::collections::HashMap;
    use std::io::{BufRead, Write};
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};
    use zbus::zvariant::Value;

    const BUS: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";
    const IFACE: &str = "org.freedesktop.Notifications";

    /// #3942 — how long the helper keeps reading its signals after the
    /// daemon's close, at most, counted from when it heard the close: the
    /// time a press the server took before the close has to reach it. Half of
    /// the daemon's grace for a helper it gave up on (`GIVE_UP_GRACE`, 1 s,
    /// then `SIGKILL`). The other half is margin for the helper's exit and
    /// for the daemon's read of it on a loaded host, so the helper exits
    /// before the kill. A server that keeps the spec says the notification
    /// closed within milliseconds of the close, and the wait ends there.
    const TAKE_DOWN_WAIT: Duration = Duration::from_millis(500);

    /// #3942 — the least the helper reads after its own close, for a `Notify`
    /// that answered more than [`TAKE_DOWN_WAIT`] after the daemon's close. A
    /// bus that stalled delivers the answer and a press behind it at once,
    /// and the press comes off the bus on the signals' own thread: without
    /// this, the helper could look before that thread had passed it on.
    const TAKE_DOWN_LEAST: Duration = Duration::from_millis(50);

    /// #3942 — `NotificationClosed`'s reason for a notification closed by a
    /// call to `CloseNotification`: the helper's own close, never the
    /// person's answer.
    const CLOSED_BY_CALL: u32 = 3;

    enum Event {
        Action(u32, String),
        Closed(u32, u32),
        /// The daemon closed our stdin: take it down. When the helper heard
        /// it.
        Parent(Instant),
    }

    /// #3942 — the notification server, as the helper uses it: its `Notify`
    /// and its `CloseNotification`. Its signals come on the helper's one
    /// stream of [`Event`]s ([`run`]). zbus's proxy is the only server the
    /// helper has; a test stands in for it.
    trait Server {
        /// `Notify` ([`post`]): the id the server gave the notification.
        fn notify(&self, req: &Request, body: &str, deadline: Instant) -> Result<u32, String>;
        /// `CloseNotification` for notification `id`.
        fn close(&self, id: u32);
    }

    impl Server for zbus::blocking::Proxy<'_> {
        fn notify(&self, req: &Request, body: &str, deadline: Instant) -> Result<u32, String> {
            post(self, req, body, deadline)
        }

        /// #3942 — sent without waiting for its reply: the server's
        /// `NotificationClosed` comes on the stream, and a slow session bus
        /// cannot hold the helper past the daemon's grace on the reply.
        /// `call_noreply` returns once the whole message is written to the
        /// bus's socket (zbus 5: `Connection::send` writes until every byte
        /// is out), so the close is on its way before the helper exits, on
        /// every path that closes.
        fn close(&self, id: u32) {
            let _ = self.call_noreply("CloseNotification", &(id,));
        }
    }

    fn say(line: String) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    /// FR-95 P2 — the session has a notification server that shows buttons:
    /// the one a being-viewed notice's Disconnect needs.
    pub(super) fn probe() -> Result<(), String> {
        let conn =
            zbus::blocking::Connection::session().map_err(|e| format!("no session bus: {e}"))?;
        let proxy = zbus::blocking::Proxy::new(&conn, BUS, PATH, IFACE)
            .map_err(|e| format!("no notification server: {e}"))?;
        let caps: Vec<String> = proxy
            .call_method("GetCapabilities", &())
            .and_then(|r| r.body().deserialize())
            .map_err(|e| format!("no notification server: {e}"))?;
        if caps.iter().any(|c| c == "actions") {
            Ok(())
        } else {
            Err(format!(
                "the notification server shows no buttons (capabilities: {})",
                caps.join(" ")
            ))
        }
    }

    pub(super) fn run() -> Result<(), String> {
        let mut first = String::new();
        let stdin = std::io::stdin();
        stdin
            .lock()
            .read_line(&mut first)
            .map_err(|e| format!("reading the request: {e}"))?;
        let req: Request =
            serde_json::from_str(first.trim()).map_err(|e| format!("the request: {e}"))?;
        // #3886 — the question's end, counted from its arrival: this helper's
        // own backstop, and the notification's expiry.
        let deadline = Instant::now() + Duration::from_millis(req.lifetime_ms);

        // ONE stream for the daemon's close and the server's signals (below).
        let (tx, rx) = std::sync::mpsc::channel::<Event>();
        // #3886 — the daemon's close is heard from the start: one that comes
        // while the session bus is still being reached (the daemon gave up on
        // this helper) is seen before anything is posted.
        let parent = tx.clone();
        std::thread::spawn(move || {
            // Everything after the request is ignored; the close is the
            // message.
            let mut sink = String::new();
            let mut lock = std::io::stdin().lock();
            while matches!(lock.read_line(&mut sink), Ok(n) if n > 0) {
                sink.clear();
            }
            let _ = parent.send(Event::Parent(Instant::now()));
        });

        let conn =
            zbus::blocking::Connection::session().map_err(|e| format!("no session bus: {e}"))?;
        let proxy = zbus::blocking::Proxy::new(&conn, BUS, PATH, IFACE)
            .map_err(|e| format!("no notification server: {e}"))?;
        let caps: Vec<String> = proxy
            .call_method("GetCapabilities", &())
            .and_then(|r| r.body().deserialize())
            .map_err(|e| format!("no notification server: {e}"))?;
        if !req.actions.is_empty() && !caps.iter().any(|c| c == "actions") {
            return Err(format!(
                "the notification server shows no buttons (capabilities: {})",
                caps.join(" ")
            ));
        }
        let markup = caps.iter().any(|c| c == "body-markup");
        let body = if markup {
            escape_markup(&req.body)
        } else {
            req.body.clone()
        };

        // Subscribed BEFORE Notify, so a press can never come before the
        // subscription that would hear it. ONE stream for both signals, so
        // they arrive in the order the server sent them: a body click is
        // `ActionInvoked(default)` then `NotificationClosed`, and read the
        // other way round it would be a refusal.
        let signals = proxy
            .receive_all_signals()
            .map_err(|e| format!("subscribing to the notification signals: {e}"))?;
        std::thread::spawn(move || {
            for msg in signals {
                let header = msg.header();
                let event = match header.member().map(|m| m.as_str()) {
                    Some("ActionInvoked") => msg
                        .body()
                        .deserialize::<(u32, String)>()
                        .ok()
                        .map(|(id, key)| Event::Action(id, key)),
                    Some("NotificationClosed") => msg
                        .body()
                        .deserialize::<(u32, u32)>()
                        .ok()
                        .map(|(id, reason)| Event::Closed(id, reason)),
                    _ => None,
                };
                if let Some(e) = event
                    && tx.send(e).is_err()
                {
                    return;
                }
            }
        });

        serve(&proxy, &rx, &req, &body, deadline, TAKE_DOWN_WAIT, &mut say)
    }

    /// The helper from its subscription on, over `server`: it posts the
    /// request, says what the person did with it, and takes it down on the
    /// daemon's close or at `deadline`, the question's end. `signals` is the
    /// ONE stream of the server's signals and the daemon's close ([`run`]).
    /// `take_down_wait` is [`TAKE_DOWN_WAIT`]; a test passes its own. What
    /// the daemon reads goes to `say`, a line each.
    fn serve(
        server: &impl Server,
        signals: &Receiver<Event>,
        req: &Request,
        body: &str,
        deadline: Instant,
        take_down_wait: Duration,
        say: &mut impl FnMut(String),
    ) -> Result<(), String> {
        // #3886 — the daemon gave up on this helper before it could post:
        // nothing goes up. Nothing is posted yet, so any other event is
        // another notification's.
        if signals.try_iter().any(|e| matches!(e, Event::Parent(_))) {
            crate::logging::say_on_stderr(format_args!(
                "portal-helper --notify: the daemon took it down before it was posted; nothing \
                 posted"
            ));
            return Ok(());
        }

        let mut id = server.notify(req, body, deadline)?;
        say(format!("{SHOWN}{id}"));
        crate::logging::say_on_stderr(format_args!(
            "portal-helper --notify: posted notification {id}"
        ));

        let close = |id: u32, why: &str| {
            server.close(id);
            crate::logging::say_on_stderr(format_args!(
                "portal-helper --notify: closed notification {id} ({why})"
            ));
        };
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match signals.recv_timeout(left) {
                // A click on the body, not a button: the server invokes
                // `default` and takes the notification away (a dismissal,
                // measured on GNOME 50). The person was looking, not
                // answering, so the question comes back. The old one's
                // `NotificationClosed` then names an id no longer ours.
                Ok(Event::Action(i, key)) if i == id && key == DEFAULT => {
                    close(i, "its body was clicked");
                    id = server.notify(req, body, deadline)?;
                    crate::logging::say_on_stderr(format_args!(
                        "portal-helper --notify: posted it again as notification {id}"
                    ));
                }
                Ok(Event::Action(i, key)) if i == id => {
                    say(format!("{ACTION}{key}"));
                    // A question is answered once; the server removes a
                    // non-resident notification on a press, and the close
                    // makes sure.
                    close(id, "answered");
                    return Ok(());
                }
                Ok(Event::Closed(i, reason)) if i == id => {
                    say(format!("{CLOSED}{reason}"));
                    return Ok(());
                }
                // #3942 — the daemon's close: the notification comes down at
                // once, as before, and a press the server took before the
                // close still reaches the daemon ([`take_down`]).
                Ok(Event::Parent(heard)) => {
                    close(id, "the daemon took it down");
                    let until = (heard + take_down_wait).max(Instant::now() + TAKE_DOWN_LEAST);
                    take_down(signals, id, until, say);
                    return Ok(());
                }
                Ok(_) => {}
                Err(_) => {
                    close(id, "its lifetime ended");
                    return Ok(());
                }
            }
        }
    }

    /// #3942 — after the daemon's close, its `CloseNotification` for `id`
    /// already sent: what the server sends about the notification is read
    /// until the server says it closed, or until `until`. On the ONE stream,
    /// a press the server took while the notification was up comes before
    /// the `NotificationClosed` that the close brings. So a person who
    /// pressed while the question was on the screen is heard, even when the
    /// press reaches the helper after the close: on a slow session bus the
    /// daemon's close comes before the server's reply to `Notify`, and the
    /// press after that reply. The daemon reads the helper until it exits.
    /// After a give-up its question is still asked, so the press answers it.
    /// After an ordinary take-down the question is over, and the broker
    /// refuses the press.
    ///
    /// - A press (any key but `default`) is passed on, as in the loop.
    /// - A click on its body (`default`) is no answer, and nothing is posted
    ///   again; nor is the dismissal a server sends after it (GNOME 50).
    /// - `NotificationClosed` ends it. Reason 3 is the helper's own close,
    ///   no answer. Any other reason is passed on as the loop passes it on:
    ///   2, the person's own dismissal before the close, is a Deny.
    fn take_down(signals: &Receiver<Event>, id: u32, until: Instant, say: &mut impl FnMut(String)) {
        let mut clicked = false;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match signals.recv_timeout(left) {
                Ok(Event::Action(i, key)) if i == id && key == DEFAULT => clicked = true,
                Ok(Event::Action(i, key)) if i == id => {
                    say(format!("{ACTION}{key}"));
                    crate::logging::say_on_stderr(format_args!(
                        "portal-helper --notify: passed on a press of notification {id} that came \
                         after the daemon's close"
                    ));
                    return;
                }
                Ok(Event::Closed(i, reason)) if i == id => {
                    if reason != CLOSED_BY_CALL && !clicked {
                        say(format!("{CLOSED}{reason}"));
                        crate::logging::say_on_stderr(format_args!(
                            "portal-helper --notify: passed on the close of notification {id} \
                             (reason {reason}) that came after the daemon's close"
                        ));
                    }
                    return;
                }
                Ok(_) => {}
                // The wait is over, or nothing is left to send on the stream.
                Err(_) => {
                    crate::logging::say_on_stderr(format_args!(
                        "portal-helper --notify: the server did not say notification {id} \
                         closed in time; exiting"
                    ));
                    return;
                }
            }
        }
    }

    /// One `Notify`: the request's words and buttons, at its urgency. The
    /// daemon or the lifetime closes it. #3886 — it also expires by itself at
    /// `deadline`, the question's end, on a server that keeps the spec's
    /// `expire_timeout`: a notification this helper could not close (killed
    /// while its `Notify` was being answered) goes then.
    fn post(
        proxy: &zbus::blocking::Proxy<'_>,
        req: &Request,
        body: &str,
        deadline: Instant,
    ) -> Result<u32, String> {
        let flat: Vec<&str> = req
            .actions
            .iter()
            .flat_map(|(k, l)| [k.as_str(), l.as_str()])
            .collect();
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("urgency", Value::from(req.urgency));
        proxy
            .call_method(
                "Notify",
                &(
                    "Roomler",
                    0u32,
                    "preferences-desktop-remote-desktop",
                    req.summary.as_str(),
                    body,
                    flat,
                    hints,
                    expire_timeout(deadline.saturating_duration_since(Instant::now())),
                ),
            )
            .and_then(|r| r.body().deserialize())
            .map_err(|e| format!("Notify: {e}"))
    }

    /// #3886 — `Notify`'s `expire_timeout` for what is `left` of the
    /// question: milliseconds, at least 1 and at most what an `i32` holds.
    /// ⚠️ Never 0, which is "never expires", nor -1, the server's default.
    fn expire_timeout(left: Duration) -> i32 {
        i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1)
    }

    #[cfg(test)]
    mod tests {
        use super::super::parent::GIVE_UP_GRACE;
        use super::{
            Event, Request, Server, TAKE_DOWN_LEAST, TAKE_DOWN_WAIT, expire_timeout, serve,
        };
        use std::cell::RefCell;
        use std::sync::mpsc::{Receiver, Sender, channel};
        use std::time::{Duration, Instant};

        /// The longest a test waits for the helper: one that works is far
        /// quicker, and one that hangs fails its test, not the run.
        const TEST_WAIT: Duration = Duration::from_secs(10);
        /// A take-down wait no test reaches: each ends on what the server
        /// sends, so a busy runner cannot cut it short.
        const UNREACHED: Duration = Duration::from_secs(30);

        /// A consent question, as the daemon writes it to the helper.
        fn request() -> Request {
            Request {
                summary: "Remote control request".into(),
                body: "Alice is requesting to control this device.".into(),
                actions: vec![
                    ("default".into(), "Show".into()),
                    ("approve".into(), "Approve".into()),
                    ("deny".into(), "Deny".into()),
                ],
                urgency: 2,
                lifetime_ms: 60_000,
            }
        }

        /// The notification server, stood in. Its `Notify` names the
        /// notification 7, at once or once the test lets it answer
        /// (`answer`), and then sends `behind_reply`, as a server sends a
        /// press it took after its reply. Each `Notify` and each
        /// `CloseNotification` is told to the test. A close is followed on
        /// the stream by `NotificationClosed(id, 3)`, as the spec has it,
        /// unless the stand-in is `mute`.
        struct StandIn {
            /// The helper's one stream: the server's signals go on it.
            stream: Sender<Event>,
            answer: Option<Receiver<()>>,
            behind_reply: RefCell<Vec<Event>>,
            notifies: Sender<()>,
            /// Each close's id, and when it was sent.
            closes: Sender<(u32, Instant)>,
            mute: bool,
        }

        impl Server for StandIn {
            fn notify(&self, _: &Request, _: &str, _: Instant) -> Result<u32, String> {
                let _ = self.notifies.send(());
                if let Some(answer) = &self.answer {
                    answer
                        .recv_timeout(TEST_WAIT)
                        .map_err(|_| "the test never let Notify answer".to_string())?;
                }
                for event in self.behind_reply.borrow_mut().drain(..) {
                    let _ = self.stream.send(event);
                }
                Ok(7)
            }

            fn close(&self, id: u32) {
                let _ = self.closes.send((id, Instant::now()));
                if !self.mute {
                    let _ = self.stream.send(Event::Closed(id, 3));
                }
            }
        }

        /// The helper's [`serve`] against a [`StandIn`], on a thread of its
        /// own as in the helper: the test's ends of its channels, and what
        /// the test has read off them so far.
        struct Run {
            /// The stream, the test's end: the daemon's close and the
            /// server's signals.
            stream: Sender<Event>,
            said_rx: Receiver<String>,
            notify_rx: Receiver<()>,
            close_rx: Receiver<(u32, Instant)>,
            ended: Receiver<(Result<(), String>, Instant)>,
            said: Vec<String>,
            closed: Vec<(u32, Instant)>,
            posted: usize,
        }

        /// What a run came to.
        struct End {
            /// Every line the helper said, in order.
            said: Vec<String>,
            /// Each notification it closed, in order.
            closed: Vec<u32>,
            /// When it sent its last close.
            last_close: Option<Instant>,
            /// How many times it posted.
            posted: usize,
            /// When it returned.
            at: Instant,
        }

        /// [`serve`] started against a stand-in that is `mute` or not, whose
        /// `Notify` answers once `answer` lets it (or at once), with
        /// `behind_reply` behind its answer.
        fn start(
            mute: bool,
            answer: Option<Receiver<()>>,
            behind_reply: Vec<Event>,
            take_down_wait: Duration,
        ) -> Run {
            let (stream, signals) = channel();
            let (notifies, notify_rx) = channel();
            let (closes, close_rx) = channel();
            let (said_tx, said_rx) = channel();
            let (ended_tx, ended) = channel();
            let server = StandIn {
                stream: stream.clone(),
                answer,
                behind_reply: RefCell::new(behind_reply),
                notifies,
                closes,
                mute,
            };
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(60);
                let mut say = |line: String| {
                    let _ = said_tx.send(line);
                };
                let outcome = serve(
                    &server,
                    &signals,
                    &request(),
                    "body",
                    deadline,
                    take_down_wait,
                    &mut say,
                );
                let _ = ended_tx.send((outcome, Instant::now()));
            });
            Run {
                stream,
                said_rx,
                notify_rx,
                close_rx,
                ended,
                said: Vec::new(),
                closed: Vec::new(),
                posted: 0,
            }
        }

        impl Run {
            /// Waits for the helper's `Notify`.
            fn posting(&mut self) {
                self.notify_rx
                    .recv_timeout(TEST_WAIT)
                    .expect("the helper posts its notification");
                self.posted += 1;
            }

            /// Waits for the helper to say that notification 7 is up.
            fn up(&mut self) {
                self.posting();
                let line = self
                    .said_rx
                    .recv_timeout(TEST_WAIT)
                    .expect("the helper says its notification is up");
                assert_eq!(line, "ROOMLER_NOTIFY_SHOWN:7");
                self.said.push(line);
            }

            /// The daemon's close, as the helper's stdin listener hears it:
            /// when.
            fn daemon_closes(&self) -> Instant {
                self.daemon_closed(Duration::ZERO)
            }

            /// The daemon's close, heard `ago`: when.
            fn daemon_closed(&self, ago: Duration) -> Instant {
                let heard = Instant::now()
                    .checked_sub(ago)
                    .expect("the clock reaches back that far");
                let _ = self.stream.send(Event::Parent(heard));
                heard
            }

            /// Waits for the helper's `CloseNotification`: its id.
            fn closing(&mut self) -> u32 {
                let (id, at) = self
                    .close_rx
                    .recv_timeout(TEST_WAIT)
                    .expect("the helper closes its notification");
                self.closed.push((id, at));
                id
            }

            /// What the server sends, in order.
            fn server_sends(&self, events: Vec<Event>) {
                for event in events {
                    let _ = self.stream.send(event);
                }
            }

            /// Waits for the helper to return: all it did.
            fn end(mut self) -> End {
                let (outcome, at) = self
                    .ended
                    .recv_timeout(TEST_WAIT)
                    .expect("the helper returns");
                outcome.expect("the helper ends with no error");
                self.said.extend(self.said_rx.try_iter());
                self.closed.extend(self.close_rx.try_iter());
                self.posted += self.notify_rx.try_iter().count();
                End {
                    said: self.said,
                    closed: self.closed.iter().map(|(id, _)| *id).collect(),
                    last_close: self.closed.last().map(|(_, at)| *at),
                    posted: self.posted,
                    at,
                }
            }
        }

        /// #3942 — the slow session bus: the server answers `Notify` only
        /// after the daemon's close was heard, and the person's press comes
        /// behind that answer. The helper passes the press on, closes its
        /// notification, and returns.
        ///
        /// Red before #3942: the close came first on the stream, so the
        /// helper closed the notification and returned, and the press, made
        /// while the question was on the screen, reached no one.
        #[test]
        fn a_press_behind_a_late_notify_answer_is_passed_on() {
            for (when, ago) in [
                ("just after the daemon's close", Duration::ZERO),
                (
                    "later than the take-down wait",
                    TAKE_DOWN_WAIT + Duration::from_millis(100),
                ),
            ] {
                let (let_answer, answer) = channel();
                let mut run = start(
                    false,
                    Some(answer),
                    vec![Event::Action(7, "approve".into())],
                    TAKE_DOWN_WAIT,
                );
                run.posting();
                // The daemon gives up while the `Notify` is on its way.
                run.daemon_closed(ago);
                let _ = let_answer.send(());
                let end = run.end();
                assert_eq!(
                    end.said,
                    ["ROOMLER_NOTIFY_SHOWN:7", "ROOMLER_NOTIFY_ACTION:approve"],
                    "an answer {when}: the press behind it is passed on"
                );
                assert_eq!(end.closed, [7], "an answer {when}: closed once");
                assert_eq!(end.posted, 1, "an answer {when}: posted once");
            }
        }

        /// #3942 — a `Notify` that answers later than `TAKE_DOWN_WAIT` after
        /// the daemon's close: the helper still reads for `TAKE_DOWN_LEAST`
        /// after its own close, for a press that came in with the answer, and
        /// not much longer, so it exits inside the daemon's grace.
        #[test]
        fn after_a_late_notify_answer_the_helper_reads_a_moment_after_its_close() {
            let (let_answer, answer) = channel();
            let mut run = start(true, Some(answer), Vec::new(), TAKE_DOWN_WAIT);
            run.posting();
            run.daemon_closed(TAKE_DOWN_WAIT + Duration::from_millis(100));
            let _ = let_answer.send(());
            assert_eq!(run.closing(), 7, "the close goes at once");
            let end = run.end();
            let read = end
                .at
                .duration_since(end.last_close.expect("the helper closed it"));
            assert_eq!(end.said, ["ROOMLER_NOTIFY_SHOWN:7"]);
            assert!(
                read >= TAKE_DOWN_LEAST,
                "the helper read for {read:?} after its close: a press that came in with the \
                 late answer could be missed"
            );
            assert!(
                read < TAKE_DOWN_WAIT,
                "the helper read for {read:?} after its close, as if its wait began there"
            );
        }

        /// #3942 — a press the server took before the daemon's close reached
        /// it comes after the helper's `CloseNotification`, which then finds
        /// nothing to close. The helper passes the press on.
        ///
        /// Red before #3942: the helper returned at its close.
        #[test]
        fn a_press_that_comes_after_the_close_is_passed_on() {
            let mut run = start(true, None, Vec::new(), UNREACHED);
            run.up();
            run.daemon_closes();
            assert_eq!(
                run.closing(),
                7,
                "the helper closes its notification at once"
            );
            // The press, then the removal it brought, as GNOME and KDE send
            // them.
            run.server_sends(vec![Event::Action(7, "deny".into()), Event::Closed(7, 2)]);
            let end = run.end();
            assert_eq!(
                end.said,
                ["ROOMLER_NOTIFY_SHOWN:7", "ROOMLER_NOTIFY_ACTION:deny"],
                "the press that came after the close is passed on"
            );
            assert_eq!(end.closed, [7]);
        }

        /// #3942 — after its close, what the server sends about its
        /// notification decides what the helper says, until the server says
        /// the notification closed. The person's own dismissal is passed on,
        /// as the loop passes it on; the close itself (reason 3) is no
        /// answer; a click on its body is no answer, and nothing is posted
        /// again.
        #[test]
        fn after_the_close_the_helper_says_what_the_person_did() {
            let cases: Vec<(&str, Vec<Event>, &[&str])> = vec![
                (
                    "its own close, reason 3, is no answer",
                    vec![Event::Closed(7, 3)],
                    &[],
                ),
                (
                    "a dismissal before the close is passed on",
                    vec![Event::Closed(7, 2)],
                    &["ROOMLER_NOTIFY_CLOSED:2"],
                ),
                (
                    "an expiry before the close is passed on",
                    vec![Event::Closed(7, 1)],
                    &["ROOMLER_NOTIFY_CLOSED:1"],
                ),
                (
                    "a click on its body is no answer, nor the removal after it",
                    vec![Event::Action(7, "default".into()), Event::Closed(7, 2)],
                    &[],
                ),
                (
                    "a press after a click on its body is passed on",
                    vec![
                        Event::Action(7, "default".into()),
                        Event::Action(7, "approve".into()),
                    ],
                    &["ROOMLER_NOTIFY_ACTION:approve"],
                ),
                (
                    "another notification's press and close are not its own",
                    vec![
                        Event::Action(8, "approve".into()),
                        Event::Closed(8, 2),
                        Event::Closed(7, 3),
                    ],
                    &[],
                ),
            ];
            for (what, after_close, passed_on) in cases {
                let mut run = start(true, None, Vec::new(), UNREACHED);
                run.up();
                run.daemon_closes();
                assert_eq!(run.closing(), 7, "{what}: the close goes at once");
                run.server_sends(after_close);
                let end = run.end();
                let mut said = vec!["ROOMLER_NOTIFY_SHOWN:7"];
                said.extend_from_slice(passed_on);
                assert_eq!(end.said, said, "{what}");
                assert_eq!(end.closed, [7], "{what}: closed once");
                assert_eq!(end.posted, 1, "{what}: nothing is posted again");
            }
        }

        /// #3942 — on the daemon's close the helper sends `CloseNotification`
        /// for its notification at once, and returns once the server says it
        /// closed: inside the daemon's grace, with nothing passed on.
        #[test]
        fn the_close_goes_at_once_and_the_helper_returns_when_the_server_says_so() {
            let mut run = start(false, None, Vec::new(), UNREACHED);
            run.up();
            let heard = run.daemon_closes();
            assert_eq!(run.closing(), 7, "the close goes at once");
            let end = run.end();
            let took = end.at.duration_since(heard);
            assert_eq!(
                end.said,
                ["ROOMLER_NOTIFY_SHOWN:7"],
                "its own close is no answer"
            );
            assert!(
                took < GIVE_UP_GRACE,
                "the helper returned {took:?} after the daemon's close, not at the server's word"
            );
        }

        /// #3942 — a server that never says the notification closed holds
        /// the helper no longer than `TAKE_DOWN_WAIT` from the daemon's
        /// close. It waits that long for a press the server may still send,
        /// and returns inside the daemon's grace.
        #[test]
        fn a_server_that_never_says_it_closed_holds_the_helper_no_longer_than_the_bound() {
            assert!(
                TAKE_DOWN_WAIT * 2 <= GIVE_UP_GRACE,
                "the wait leaves half of the daemon's grace for the helper's exit"
            );
            let mut run = start(true, None, Vec::new(), TAKE_DOWN_WAIT);
            run.up();
            let heard = run.daemon_closes();
            assert_eq!(run.closing(), 7, "the close goes at once");
            let end = run.end();
            let took = end.at.duration_since(heard);
            assert_eq!(end.said, ["ROOMLER_NOTIFY_SHOWN:7"]);
            assert!(
                took >= TAKE_DOWN_WAIT,
                "the helper returned {took:?} after the daemon's close: a press still on its way \
                 would be lost"
            );
            assert!(
                took < GIVE_UP_GRACE,
                "the helper returned {took:?} after the daemon's close, past the daemon's grace"
            );
        }

        /// #3886 — the expiry is what is left of the question, in
        /// milliseconds; a question with nothing left still expires (1 ms),
        /// and a lifetime past what an `i32` holds is capped, never wrapped.
        #[test]
        fn the_notification_expires_with_its_question() {
            assert_eq!(expire_timeout(Duration::from_secs(30)), 30_000);
            assert_eq!(expire_timeout(Duration::from_millis(1500)), 1500);
            assert_eq!(expire_timeout(Duration::ZERO), 1);
            assert_eq!(expire_timeout(Duration::from_micros(400)), 1);
            assert_eq!(
                expire_timeout(Duration::from_secs(24 * 3600)),
                86_400_000,
                "the being-viewed notice's day"
            );
            assert_eq!(
                expire_timeout(Duration::from_secs(40 * 24 * 3600)),
                i32::MAX
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn view(
        now: Instant,
        org: &str,
        detail: &str,
        permissions: &str,
        left: Duration,
    ) -> super::super::PromptView {
        super::super::PromptView {
            session_hex: "abc".into(),
            title: "Remote control request".into(),
            lead: "Alice is requesting to control this device.".into(),
            detail: detail.into(),
            permissions: permissions.into(),
            org: org.into(),
            expires_at: now + left,
        }
    }

    /// FR-93 P6f — the notification says what the native panel says, the
    /// window included, with Approve and Deny, at critical urgency.
    #[test]
    fn the_question_reads_as_the_panel_does() {
        let now = Instant::now();
        let v = view(now, "Acme", "", "VIEW|INPUT", Duration::from_secs(300));
        let r = consent_request(&v, now);
        assert_eq!(r.summary, "Remote control request");
        assert_eq!(
            r.body,
            "Alice is requesting to control this device.\nOn behalf of Acme\n\
             Permissions: VIEW|INPUT\nUnanswered, it is refused in 5 minutes."
        );
        // `default` is the click on the body, which no server draws as a
        // button: the helper posts the question again on it.
        assert_eq!(
            r.actions,
            vec![
                ("default".to_string(), "Show".to_string()),
                ("approve".to_string(), "Approve".to_string()),
                ("deny".to_string(), "Deny".to_string())
            ]
        );
        assert_eq!(r.urgency, 2);
        assert_eq!(r.lifetime_ms, 300_000);
        let exec = view(now, "", "uname -a", "", Duration::from_secs(30));
        assert_eq!(
            consent_request(&exec, now).body,
            "Alice is requesting to control this device.\nuname -a\n\
             Unanswered, it is refused in 30 seconds."
        );
    }

    /// FR-93 P6g — the being-viewed notice names the controller and whether
    /// they can type, by permission name, and has Disconnect.
    #[test]
    fn the_viewing_notice_names_who_and_what() {
        let r = viewing_request("Alice", "VIEW|INPUT|CLIPBOARD");
        assert_eq!(r.summary, "This screen is being viewed");
        assert_eq!(
            r.body,
            "Alice is viewing this screen and can use its keyboard and mouse.\n\
             Disconnect ends the session."
        );
        assert_eq!(
            r.actions,
            vec![
                ("default".to_string(), "Show".to_string()),
                ("disconnect".to_string(), "Disconnect".to_string())
            ]
        );
        assert_eq!(r.urgency, 2);
        assert_eq!(
            viewing_request("Bob", "VIEW").body,
            "Bob is viewing this screen.\nDisconnect ends the session."
        );
        // ⚠️ equality, never a substring: a name that merely contains INPUT
        // grants no input.
        assert_eq!(
            viewing_request("Eve", "VIEW|INPUT_LOG").body,
            "Eve is viewing this screen.\nDisconnect ends the session."
        );
    }

    #[test]
    fn the_window_in_words() {
        assert_eq!(in_words(300), "5 minutes");
        assert_eq!(in_words(287), "5 minutes");
        assert_eq!(in_words(90), "2 minutes");
        assert_eq!(in_words(75), "1 minute");
        assert_eq!(in_words(30), "30 seconds");
        assert_eq!(in_words(0), "0 seconds");
    }

    /// P6f — Approve grants, Deny refuses, closing it refuses; a notification
    /// the server expired, or one the daemon closed, answers nothing.
    #[test]
    fn what_the_person_did_is_the_answer() {
        assert_eq!(consent_answer(&Said::Action("approve".into())), Some(true));
        assert_eq!(consent_answer(&Said::Action("deny".into())), Some(false));
        assert_eq!(consent_answer(&Said::Closed(2)), Some(false));
        assert_eq!(consent_answer(&Said::Closed(1)), None);
        assert_eq!(consent_answer(&Said::Closed(3)), None);
        assert_eq!(consent_answer(&Said::Action("default".into())), None);
        // ⚠️ equality, never a prefix: "approve-all" is not "approve".
        assert_eq!(consent_answer(&Said::Action("approved".into())), None);
    }

    #[test]
    fn marked_lines_are_found_among_chatter() {
        assert_eq!(parse("ROOMLER_NOTIFY_SHOWN:17"), Some(Said::Shown));
        assert_eq!(
            parse("  ROOMLER_NOTIFY_ACTION:deny\n"),
            Some(Said::Action("deny".into()))
        );
        assert_eq!(parse("ROOMLER_NOTIFY_CLOSED:2"), Some(Said::Closed(2)));
        assert_eq!(parse("ROOMLER_NOTIFY_CLOSED:x"), Some(Said::Closed(4)));
        assert_eq!(
            parse("ROOMLER_NOTIFY_FAILED:no buttons"),
            Some(Said::Failed("no buttons".into()))
        );
        assert_eq!(parse("Gtk-WARNING: something"), None);
    }

    /// P6f — a command line or a name with `<`, `>` or `&` arrives as text on
    /// a server that reads markup.
    #[test]
    fn markup_is_escaped() {
        assert_eq!(
            escape_markup("cat </etc/hosts && echo <b>x</b>"),
            "cat &lt;/etc/hosts &amp;&amp; echo &lt;b&gt;x&lt;/b&gt;"
        );
    }

    #[test]
    fn the_request_survives_the_pipe() {
        let now = Instant::now();
        let r = consent_request(&view(now, "", "", "VIEW", Duration::from_secs(60)), now);
        let back: Request = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }
}
