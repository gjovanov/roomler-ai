// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Operator-consent broker (Plan 3 of the file-DC v2 follow-on
//! cycle).
//!
//! Replaces the agent's session-level auto-grant in `signaling.rs`
//! with a decision flow that — for org-controlled fleets — keeps a
//! human in the loop before a remote-control session starts. Today's
//! self-hosted user typically just controls their own machines, so
//! the default `auto_grant_session = true` preserves the historical
//! behaviour: the broker resolves to `Decision::Granted` immediately
//! and the wire flow is unchanged.
//!
//! When `auto_grant_session = false`, the broker waits for a sentinel
//! file under `<log_dir>/consent/` to appear. The sentinel is dropped
//! by an out-of-process operator running:
//!
//! ```text
//! roomlerd consent --session <hex_id> --approve
//! roomlerd consent --session <hex_id> --deny
//! roomlerd consent --list          # FR-27: what is outstanding right now
//! ```
//!
//! 30 s timeout → auto-deny. Outcome propagates back through the
//! [`ConsentBroker::request`] future the signaling layer awaits. A session
//! that ends while its prompt still stands is withdrawn through
//! [`ConsentBroker::cancel`] (#1632): the future resolves
//! [`Decision::Cancelled`], the marker comes down, and nothing goes on the
//! wire.
//!
//! The tray popup (Phase 3) now renders this prompt on attended
//! sessions: the signaling layer calls [`ConsentBroker::write_pending`]
//! to drop a `<session>.pending` marker in this same dir, the
//! `roomler-desktop` companion watches for it and shows an
//! Approve/Deny modal, and the operator's choice writes the same
//! `.approve`/`.deny` sentinel the poll loop below already consumes.
//! The CLI subcommand remains a headless fallback.
//!
//! Audit lifecycle: hub.rs already emits
//! [`AuditKind::ConsentPrompted`] on `rc:request` send and
//! `ConsentGranted` / `ConsentDenied` on `rc:consent` receive, plus
//! `ConsentTimedOut` on its own server-side timeout
//! (`DEFAULT_CONSENT_TIMEOUT`). The agent simply emits one of two
//! `rc:consent` shapes (granted=true / granted=false) — no new wire
//! types needed.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Default operator-decision timeout when an operator-consent prompt
/// is required. 30 s matches the planner's spec; deliberately on the
/// short side because the controller's own
/// `consent_timeout_secs` (sent via `rc:request`) is the upper bound
/// past which the controller gives up. Both sides converging on the
/// same wall-clock means the operator's ~30 s window aligns with the
/// browser's own progress UI.
pub const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling interval when watching for a sentinel-file decision. Fast
/// enough that the operator perceives "click → resolve" as instant
/// (sub-second) without burning CPU on a permanent poll loop.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Outcome of a consent prompt. The agent sends `rc:consent
/// { granted }` based on this; if Timeout fires, the agent sends
/// `granted: false` (the safe-default closes the Hub's wait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Granted,
    Denied,
    /// Operator did not respond within the prompt timeout. Treat as
    /// denial on the wire (the hub's own audit pipeline records this
    /// distinctly from an explicit deny).
    Timeout,
    /// #1632 — the question was WITHDRAWN before anyone answered it:
    /// [`ConsentBroker::cancel`] ran because the session the prompt was
    /// asking about ended (the controller closed its tab, switched device,
    /// or the server timed the session out first). Not a verdict from the
    /// person at the machine and not a timeout, so the RC path sends
    /// **nothing** on the wire for it — the server has already forgotten
    /// the session, and a `granted:false` would read as "the user denied
    /// you" for a question nobody was asked to the end.
    Cancelled,
}

impl Decision {
    /// Map to the `granted` boolean on `ClientMsg::Consent`. Everything
    /// but `Granted` produces `false` — the hub's server-side audit /
    /// heuristic distinguishes `Denied` from `Timeout` via its own timer,
    /// and `Cancelled` never reaches the wire at all.
    pub fn granted(self) -> bool {
        matches!(self, Decision::Granted)
    }
}

/// Pure helper deciding what to do BEFORE consulting any
/// out-of-process state. Lifted out of [`ConsentBroker::request`] so
/// the contract is easy to lock with unit tests:
///
/// - `auto_grant=true` → immediate grant, no prompt, no sentinel.
/// - `auto_grant=false` → prompt path; broker watches the sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Skip the prompt, immediate `Decision::Granted`.
    AutoGrant,
    /// Wait for an out-of-process decision via the sentinel.
    Prompt { timeout: Duration },
}

impl Mode {
    pub fn from_config(auto_grant: bool) -> Self {
        if auto_grant {
            Mode::AutoGrant
        } else {
            Mode::Prompt {
                timeout: DEFAULT_PROMPT_TIMEOUT,
            }
        }
    }
}

/// FR-27 — resolve the server's directive against this device's own setting,
/// taking the STRICTER of the two.
///
/// The server's directive is a ceiling, not a floor. Before this, the directive
/// simply won (`directive.unwrap_or(local)`), so a device that had set
/// `auto_grant_session = false` — "never start a session without asking someone
/// here" — was silently overridden by a server `Auto`, which is the exact
/// inversion of the gate-4 property the exec and SSH designs are built on: the
/// device's own refusal has to survive a server that has been talked into
/// something. A remote-control session is a strictly more invasive grant than
/// an `exec`, so it cannot be the one subsystem where the local switch is
/// advisory.
///
/// `None` = an older server that sends no directive; the local mode is then the
/// whole answer, exactly as before.
///
/// `host_window` is how long a downgraded prompt should stand — the window the
/// SERVER is waiting on, passed in rather than taken from `local` so the two
/// sides cannot silently disagree. (They happen to coincide today at 30 s, and
/// a coincidence is not a contract.)
///
/// ⚠️ The floor defeats `Auto` and nothing else. `Email`/`Push` do not
/// auto-grant — they ask the device OWNER — so a device that merely refuses to
/// auto-grant has no standing to force a second prompt onto the host. Those
/// modes never reach here as `AutoGrant`; the call site, which knows the mode,
/// keeps them on their own path.
pub fn strictest_of(directive: Option<Mode>, local: Mode, host_window: Duration) -> Mode {
    match (directive, local) {
        // No directive: the device's own setting is the whole answer.
        (None, local) => local,
        // The device refuses to auto-grant. A directive to do so anyway is
        // downgraded to a prompt over the server's own window.
        (Some(Mode::AutoGrant), Mode::Prompt { .. }) => Mode::Prompt {
            timeout: host_window,
        },
        // Otherwise the directive is at least as strict as the local setting.
        (Some(directive), _) => directive,
    }
}

/// FR-27 — which subsystem a pending prompt belongs to. The wire values match
/// [`tunnel_core::localapi::ConsentRequest::kind`].
///
/// The three read very differently to whoever is being asked — a screen share,
/// a single command, an interactive shell — and until FR-27 only the first one
/// ever reached a UI at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    RemoteControl,
    Exec,
    Ssh,
    /// FR-85 P3 — a controller already in a session asks to RECORD the
    /// screen. Its own question, on its own prompt id, because "may they see
    /// my screen" and "may they keep a copy of it" are different answers.
    ///
    /// ⚠️ A companion older than P3 renders a kind it does not know as `rc`
    /// ("is requesting to control this device"), so a record prompt always
    /// carries the whole question in its `detail` line too — the one field
    /// every companion shows as it is.
    Record,
}

impl PromptKind {
    pub fn wire(self) -> &'static str {
        match self {
            PromptKind::RemoteControl => "rc",
            PromptKind::Exec => "exec",
            PromptKind::Ssh => "ssh",
            PromptKind::Record => "record",
        }
    }
}

/// FR-27 — WHO is already showing this prompt.
///
/// The marker is written even when the daemon's own native panel is up,
/// because it is also the machine-readable record that a decision is
/// outstanding — `roomlerd consent --list` must show a natively-prompted
/// session, and the LocalAPI is the only honest place to read that from.
///
/// Which makes this field load-bearing: without it the desktop companion would
/// see the marker and pop a SECOND panel asking the same question, and two
/// Approve buttons for one decision is how someone approves the wrong thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSurface {
    /// The daemon is drawing it (Windows overlay, X11, AppKit). A UI reading
    /// the marker should LIST it and not render a panel.
    Native,
    /// Nobody is drawing it yet — the companion should.
    Companion,
    /// FR-93 P6f — the daemon put it up as a desktop notification, because
    /// neither a panel nor the companion could show it. Written over the
    /// marker once the notification is up, so a companion that starts late
    /// lists the question rather than asking it a second time.
    Notification,
}

impl PromptSurface {
    /// ⚠️ [`PromptSurface::Notification`] goes out as `native`: every
    /// companion renders a marker whose surface is anything but `native`, so
    /// a new word would have each one already installed ask again.
    pub fn wire(self) -> &'static str {
        match self {
            PromptSurface::Native | PromptSurface::Notification => "native",
            PromptSurface::Companion => "companion",
        }
    }
}

/// Everything a prompt surface needs to render one decision.
///
/// Built here rather than at each call site so the three subsystems cannot
/// drift into three subtly different JSON shapes — the marker file is parsed
/// back as `localapi::ConsentRequest`, and a field spelled differently in one
/// producer is simply absent for that kind, silently.
pub struct PendingPrompt<'a> {
    pub kind: PromptKind,
    /// Who is asking (a display name, or the caller identity for exec/ssh).
    pub asked_by: &'a str,
    /// Pipe-separated permission names; empty for exec/ssh.
    pub permissions: String,
    /// The one line that makes the decision answerable — the redacted command,
    /// the SSH principal. Empty for RC.
    ///
    /// ⚠️ Must ALREADY be redacted by the caller. A prompt is rendered by a GUI
    /// and can be screenshotted; exec payloads routinely carry tokens.
    pub detail: String,
    /// Asking organization, for a multi-org device. Empty otherwise.
    pub org: String,
    pub timeout: Duration,
    /// Who is already showing this prompt. See [`PromptSurface`].
    pub surface: PromptSurface,
}

impl PendingPrompt<'_> {
    /// Serialize to the marker body. Deliberately built from the SAME struct
    /// the reader parses, so adding a field cannot leave one producer behind.
    pub fn to_body(&self, session_hex: &str) -> String {
        let expires_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 + self.timeout.as_millis() as u64)
            .unwrap_or(0);
        let req = tunnel_core::localapi::ConsentRequest {
            session_id: session_hex.to_string(),
            controller_name: self.asked_by.to_string(),
            permissions: self.permissions.clone(),
            timeout_secs: self.timeout.as_secs(),
            kind: self.kind.wire().to_string(),
            detail: self.detail.clone(),
            expires_at_ms,
            surface: self.surface.wire().to_string(),
            org: self.org.clone(),
        };
        // Infallible for this shape; a `{}` body would still parse to a
        // renderable (if bare) request rather than dropping the prompt.
        serde_json::to_string(&req).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Cross-platform consent broker. One instance per agent process.
/// Cheap to clone — internal state is `Arc<Mutex<...>>`. Thread-safe.
#[derive(Clone)]
pub struct ConsentBroker {
    inner: Arc<BrokerInner>,
}

// A2 note: this module used to publish a process-wide broker
// (`set_shared`/`shared()`) for Roomler SSH, whose server is constructed
// inside the overlay's TUN factory. That global is GONE — the broker now
// rides `ssh::SessionServices` through `overlay::maybe_start` into the
// server's `Ctx`, so "no broker ⇒ deny" is unrepresentable instead of a doc
// comment. Every consumer receives the broker explicitly.
struct BrokerInner {
    mode: Mode,
    /// Directory where sentinel files are read/written. One file
    /// per pending session: `<sentinel_dir>/<session_hex>.{approve|deny}`.
    /// The CLI subcommand creates these with a tiny "now()" payload
    /// that the broker's poll loop discovers.
    sentinel_dir: PathBuf,
    /// Sessions currently awaiting a decision, each with the handle a
    /// [`ConsentBroker::cancel`] fires. Populated the moment
    /// [`ConsentBroker::request_with_mode`] is CALLED (before its future is
    /// first polled — see there for why). Cleared by [`ConsentBroker::cancel`]
    /// the moment the prompt is withdrawn, and otherwise by the prompt's look
    /// that decides it (an answer, the window running out) or by its
    /// [`PendingGuard`] when the future is dropped.
    ///
    /// ⚠️ Every change to a question happens under this lock (#3433): it is
    /// armed, answered (the sentinel written), withdrawn, decided and released
    /// (with the files that show it) under it. So an answer is on the books
    /// before its question ends, or it is refused: never reported taken by a
    /// question that has already ended.
    pending: Mutex<HashMap<String, Arc<Notify>>>,
}

impl BrokerInner {
    fn sentinel_path(&self, session_hex: &str, kind: SentinelKind) -> PathBuf {
        self.sentinel_dir
            .join(format!("{}.{}", session_hex, kind.suffix()))
    }
}

/// One question's place on the books, and the files that show it: its
/// `.pending` marker and any answer left for it. Released by the look that
/// ends the question, and again on drop, so a prompt whose awaiting future is
/// CANCELLED (the SSH / RC transport died before a decision) does not leak a
/// phantom the CLI and tray keep showing. Straight-line cleanup ran only on a
/// normal decision.
///
/// Owns its broker handle rather than borrowing it, so the prompt future it
/// rides in is `'static` and can be created on one task and awaited on
/// another (the RC path arms the prompt on the WS loop and awaits it in a
/// spawned task).
struct PendingGuard {
    inner: Arc<BrokerInner>,
    session_hex: String,
    /// The cancel handle THIS prompt registered. The entry is removed only
    /// while it still holds this exact handle, so a guard cannot evict a
    /// later prompt that re-used the same session id.
    cancel: Arc<Notify>,
}

impl PendingGuard {
    /// Whether the books still hold THIS question, not a newer one for the
    /// same id.
    fn holds(&self, books: &HashMap<String, Arc<Notify>>) -> bool {
        books
            .get(&self.session_hex)
            .is_some_and(|n| Arc::ptr_eq(n, &self.cancel))
    }

    /// One look at the question, under the books: withdrawn, answered or out
    /// of time, in that order. A question that ends here is released in the
    /// same step, so from its end it takes no answer and leaves none behind.
    ///
    /// ⚠️ The books first (#3433): off them, the question is withdrawn,
    /// whatever has landed since. The poll used to look for an answer first,
    /// so an Allow that passed `record_decision`'s check just before a cancel,
    /// or one the CLI's fallback wrote with no check at all, landed after it
    /// and was taken.
    fn look(&self, approve: &Path, deny: &Path, deadline: Instant) -> Option<Decision> {
        let mut books = self.inner.pending.lock().unwrap();
        let decided = if !self.holds(&books) {
            Decision::Cancelled
        } else if approve.exists() {
            Decision::Granted
        } else if deny.exists() {
            Decision::Denied
        } else if Instant::now() >= deadline {
            Decision::Timeout
        } else {
            return None;
        };
        self.release_in(&mut books, |marker| {
            let _ = std::fs::remove_file(marker);
        });
        Some(decided)
    }

    /// Takes the question off the books, then off the disk: its marker, and
    /// any answer left for it. Only while the books hold this question, or
    /// nothing for its id.
    ///
    /// ⚠️ The entry first, then the marker (#3427). `record_decision` takes an
    /// answer while the in-memory entry stands, and the companion, `roomlerd
    /// consent --list` and the person go by the marker. With the marker
    /// removed first, an answer sent the moment it went was still taken, for a
    /// session that had ended.
    ///
    /// ⚠️ All of it under the books' lock (#3433), which every answer and every
    /// newer question for the same id take too. So no answer lands between the
    /// entry going and its files going, to be reported taken for nothing and
    /// left behind. And a guard whose id a newer question holds (an RC session
    /// asked again under its own id) leaves that question's marker and answers
    /// alone: it used to take the marker off the screen of a question still
    /// asking. `remove_marker` is the unlink, passed in so a test can answer at
    /// exactly that moment.
    fn release(&self, remove_marker: impl FnOnce(&Path)) {
        let mut books = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.release_in(&mut books, remove_marker);
    }

    fn release_in(
        &self,
        books: &mut HashMap<String, Arc<Notify>>,
        remove_marker: impl FnOnce(&Path),
    ) {
        if self.holds(books) {
            books.remove(&self.session_hex);
        } else if books.contains_key(&self.session_hex) {
            return;
        }
        remove_marker(
            &self
                .inner
                .sentinel_dir
                .join(format!("{}.pending", self.session_hex)),
        );
        for kind in [SentinelKind::Approve, SentinelKind::Deny] {
            let _ = std::fs::remove_file(self.inner.sentinel_path(&self.session_hex, kind));
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.release(|marker| {
            let _ = std::fs::remove_file(marker);
        });
    }
}

/// A prompt that is REGISTERED (its session is in the pending set, so a
/// decision or a cancel can already reach it) but whose poll loop has not
/// started. Produced synchronously by [`ConsentBroker::request_with_mode`],
/// consumed by the future it returns.
struct ArmedPrompt {
    guard: PendingGuard,
    timeout: Duration,
}

impl ArmedPrompt {
    async fn run(self) -> Decision {
        let ArmedPrompt { guard, timeout } = self;
        let inner = &guard.inner;
        let session_hex = guard.session_hex.as_str();
        let cancel = &guard.cancel;
        tracing::info!(
            session = session_hex,
            timeout_secs = timeout.as_secs(),
            sentinel_dir = %inner.sentinel_dir.display(),
            // RETIRED-NAME-ANCHOR: naming the retired binary is the POINT —
            // this comment records which spelling was wrong and why.
            // FR-21 retired `roomler-agent`; this line is the ONLY guidance a
            // headless operator gets, and naming a binary that no longer
            // exists makes it worse than silence. Field-caught on mars
            // 2026-08-29, still printing the old name at 0.4.16.
            "operator consent required — answer it with `roomlerd consent --session {} --approve|--deny` (`roomlerd consent --list` shows what is outstanding)",
            session_hex
        );

        // The stale-sentinel purge happened in `arm()`, BEFORE this session
        // was registered — a sentinel that exists now is an answer to this
        // prompt. Purging here again would eat one recorded between arming
        // and this first poll.
        let approve = inner.sentinel_path(session_hex, SentinelKind::Approve);
        let deny = inner.sentinel_path(session_hex, SentinelKind::Deny);
        let deadline = Instant::now() + timeout;
        let outcome = loop {
            if let Some(decided) = guard.look(&approve, &deny, deadline) {
                break decided;
            }
            // #1632 — a cancel lands within the poll interval, not at its
            // end: `notify_one` stores a permit when nobody is waiting, so
            // one fired between two iterations is consumed by the very next
            // `notified()`, and one fired while the sleep is pending wakes
            // it at once. Either way the next look finds the question off
            // the books.
            tokio::select! {
                _ = cancel.notified() => {}
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        };

        // The look that decided released the question: off the books, its
        // marker and its sentinels gone, so a re-request of the same id sees
        // nothing stale. The guard releases again on drop, a no-op by then.
        tracing::info!(session = session_hex, ?outcome, "operator consent decision");
        outcome
    }
}

impl ConsentBroker {
    /// Build a new broker. `sentinel_dir` is created if absent. On
    /// Unix the directory is `chmod 700` to match `config.toml`'s
    /// 0600 posture (the sentinel files leak only "yes/no" decisions,
    /// but the convention is "agent state lives at 700"). On Windows the
    /// dir inherits the owning profile's ACL (SYSTEM profile ⇒
    /// SYSTEM+Administrators only); the LocalAPI pipe SDDL is the enforced
    /// decision boundary — an explicit DACL here is future hardening
    /// (P2b review M1).
    pub fn new(mode: Mode, sentinel_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&sentinel_dir)
            .with_context(|| format!("creating sentinel dir {}", sentinel_dir.display()))?;
        // P2b review L3: consent decisions are FILES in this directory —
        // refuse to operate through a symlink, which would let whoever
        // planted it redirect sentinel writes into (or read prompts from)
        // a directory they control. `create_dir_all` above succeeds on a
        // pre-existing symlink-to-dir, so check the link itself.
        let meta = std::fs::symlink_metadata(&sentinel_dir)
            .with_context(|| format!("inspecting sentinel dir {}", sentinel_dir.display()))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "consent sentinel dir {} is a symlink — refusing to use it",
                sentinel_dir.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&sentinel_dir)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&sentinel_dir, perms)?;
        }
        Ok(Self {
            inner: Arc::new(BrokerInner {
                mode,
                sentinel_dir,
                pending: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Convenience: build the default sentinel directory under the
    /// agent's log dir. Mirrors the path layout used by the
    /// `last-install.json`/`needs-attention.txt` sentinels in
    /// `updater.rs` so operators only have to remember one location.
    pub fn default_sentinel_dir() -> Result<PathBuf> {
        let dirs = crate::appdirs::project_dirs()
            .context("could not resolve a platform data directory")?;
        Ok(dirs.data_dir().join("logs").join("consent"))
    }

    /// Path of the sentinel file for `(session_hex, decision)`.
    /// Public so the CLI subcommand can write to it.
    pub fn sentinel_path(&self, session_hex: &str, kind: SentinelKind) -> PathBuf {
        self.inner.sentinel_path(session_hex, kind)
    }

    /// Sentinel directory in use. Public so the CLI can list pending
    /// sessions.
    pub fn sentinel_dir(&self) -> &Path {
        &self.inner.sentinel_dir
    }

    /// Mode the broker was built with. Mostly used by unit tests +
    /// the signaling-layer log line.
    pub fn mode(&self) -> Mode {
        self.inner.mode
    }

    /// Request consent for `session_hex`. Resolves to `Granted`
    /// immediately when configured for auto-grant; otherwise polls
    /// the sentinel directory until an `.approve` / `.deny` appears
    /// or the prompt timeout expires.
    ///
    /// On success the matching sentinel files (both approve+deny
    /// with the same session id) are cleaned up — operators dropping
    /// stale `.approve` files won't accidentally pre-approve a
    /// future session.
    pub fn request(&self, session_hex: &str) -> impl Future<Output = Decision> + Send + use<> {
        self.request_with_mode(session_hex, self.inner.mode)
    }

    /// Like [`request`], but drives the decision with an explicit per-session
    /// `mode` — the server's Phase-2 consent directive
    /// (`ServerMsg::Request { consent_mode }`) — instead of the broker's startup
    /// mode. Server-authoritative consent: the agent obeys the server rather
    /// than its local `auto_grant_session`. The sentinel dir + poll loop are
    /// shared state, so a broker built for `AutoGrant` can still prompt on
    /// demand when the server directs a `Prompt`.
    ///
    /// #1632 — deliberately NOT an `async fn`: the session is registered as
    /// pending HERE, synchronously, and only the poll loop lives in the
    /// returned future. The RC path arms the prompt on the WS read loop and
    /// awaits it in a spawned task, and the `Terminate` for that session can
    /// be the very next frame the loop reads — with lazy registration the
    /// task may not have been polled yet, `cancel()` would find nothing, and
    /// the prompt would then stand for its full window. Eager registration
    /// makes "a cancel issued after this call returns always reaches the
    /// prompt" true without any ordering assumption about the runtime. The
    /// future owns its broker handle (`use<>` captures no borrow), so it is
    /// `'static` and spawnable; dropping it unpolled still runs the guard.
    pub fn request_with_mode(
        &self,
        session_hex: &str,
        mode: Mode,
    ) -> impl Future<Output = Decision> + Send + use<> {
        // Reject anything that doesn't look like a hex session id.
        // Stops a stray empty-string request from scanning the
        // entire sentinel dir.
        let armed = if session_hex.is_empty() || session_hex.len() > 64 {
            tracing::warn!(session = session_hex, "consent request with implausible id");
            Err(Decision::Denied)
        } else {
            match mode {
                Mode::AutoGrant => Err(Decision::Granted),
                Mode::Prompt { timeout } => Ok(self.arm(session_hex, timeout)),
            }
        };
        async move {
            match armed {
                Ok(prompt) => prompt.run().await,
                Err(immediate) => immediate,
            }
        }
    }

    /// Stamp `session_hex` as pending and hand back the prompt to run. The
    /// guard it carries clears the `.pending` marker and the in-memory entry
    /// on EVERY exit, including the future being DROPPED before a decision.
    /// Field 2026-08-29: an SSH dial from mars whose transport reset
    /// mid-prompt left `<id>.pending` behind, so `roomlerd consent --list`
    /// showed a phantom prompt that outlived its session (and a late
    /// `--approve` logged "no active prompt"). The old cleanup was
    /// straight-line code after the poll loop, so a cancelled
    /// `request_with_mode` skipped it.
    fn arm(&self, session_hex: &str, timeout: Duration) -> ArmedPrompt {
        // SECURITY (P2b): purge any pre-existing decision sentinel so ONLY a
        // decision that arrives AFTER this prompt starts can resolve it — a
        // sentinel written before the request (stale from a prior run, or a
        // pre-emptive approve) cannot pre-approve this session.
        //
        // BEFORE the registration below, never at the poll loop's first step
        // (#1632): `record_decision` honours a decision from the moment the
        // session is registered, and the loop's first poll can come later
        // (the RC path arms on the WS loop and polls in a spawned task). A
        // purge there ate an Approve recorded in between — the person who
        // clicked watched the prompt time out.
        let _ = std::fs::remove_file(self.inner.sentinel_path(session_hex, SentinelKind::Approve));
        let _ = std::fs::remove_file(self.inner.sentinel_path(session_hex, SentinelKind::Deny));
        let cancel = Arc::new(Notify::new());
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(session_hex.to_string(), cancel.clone());
        ArmedPrompt {
            guard: PendingGuard {
                inner: self.inner.clone(),
                session_hex: session_hex.to_string(),
                cancel,
            },
            timeout,
        }
    }

    /// #1632 — withdraw the prompt standing for `session_hex`, if one is: its
    /// future resolves [`Decision::Cancelled`] promptly (inside the poll
    /// interval, not at the window's end) and its guard clears the `.pending`
    /// marker the companion and `roomlerd consent --list` read. Returns
    /// whether a prompt was running. A session that is not being prompted —
    /// never was, or already resolved — is a no-op that returns `false`, and
    /// nothing is remembered about it: a later prompt for the same id starts
    /// clean, with its own handle.
    ///
    /// Called from the `ServerMsg::Terminate` path: the session the question
    /// is about no longer exists, so the person at the machine must not be
    /// left deciding on it, and no verdict is owed to the server.
    ///
    /// ⚠️ Off the books first, then woken (#3433): from this call on the
    /// prompt takes no answer, since every look it takes goes to the books
    /// first and finds the question withdrawn, whatever has landed since.
    /// Woken alone, its entry stood until the prompt next ran, which for a
    /// question armed before it is shown can be a companion's start away, and
    /// the poll looked for an answer before the cancel: an answer in between
    /// was taken, and won.
    pub fn cancel(&self, session_hex: &str) -> bool {
        let handle = self.inner.pending.lock().unwrap().remove(session_hex);
        match handle {
            Some(cancel) => {
                tracing::info!(
                    session = session_hex,
                    "consent prompt cancelled — its session ended before an answer arrived"
                );
                cancel.notify_one();
                true
            }
            None => false,
        }
    }

    /// Drop a sentinel file for the given session. Used by the CLI
    /// subcommand. Returns the path written so the caller can surface
    /// it to the operator.
    pub fn write_sentinel(&self, session_hex: &str, kind: SentinelKind) -> Result<PathBuf> {
        let path = self.sentinel_path(session_hex, kind);
        // Body: unix timestamp (string). Just so the file is never
        // empty, easier to grep / debug. The polling loop only checks
        // existence, not content.
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let body = format!("{now_ts}\n");
        // ⚠️ Unix: written under a name of its own, created new and 0600, then
        // renamed into place (#3433). The prompt's poll takes a sentinel the
        // moment it exists and removes it. A chmod after the write then failed,
        // "No such file or directory", and `record_decision` said false for an
        // answer that had counted. The rename is atomic: the poll sees the
        // whole file or nothing, already 0600, and an answer counts only once
        // the rename has landed it. Windows writes no mode, so it has no step
        // after the write to lose.
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let tmp = self.inner.sentinel_dir.join(format!(
                ".{session_hex}.{}.{}-{}.tmp",
                kind.suffix(),
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let landed = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .and_then(|mut f| f.write_all(body.as_bytes()))
                .and_then(|()| std::fs::rename(&tmp, &path));
            if let Err(e) = landed {
                let _ = std::fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("writing sentinel {}", path.display()));
            }
        }
        #[cfg(not(unix))]
        std::fs::write(&path, body)
            .with_context(|| format!("writing sentinel {}", path.display()))?;
        Ok(path)
    }

    /// Record an operator decision for `session_hex` — the LocalAPI's entry point
    /// (P2b) — but ONLY if that session is CURRENTLY being prompted (present in
    /// the live `pending` set). Returns whether it was recorded. Gating on the
    /// live prompt means a caller can't pre-approve, nor approve an unknown or
    /// stale session: a decision only counts as an answer to a question the
    /// broker is actively asking (the confused-deputy guard). `session_hex` is
    /// hex-validated by the caller (`DaemonState::consent_decide`) before it
    /// reaches this.
    pub fn record_decision(&self, session_hex: &str, allow: bool) -> bool {
        // ⚠️ Checked and written under the books' lock (#3433), the one the
        // prompt decides under: an answer lands only while its question still
        // asks, for the prompt's next look to take. Checked under it and
        // written after, an answer that passed the check as its question was
        // decided or withdrawn landed once the question had cleared its
        // sentinels: reported taken for nothing, and left behind. A withdrawal
        // before that next look still wins: the session it was for is over.
        let books = self.inner.pending.lock().unwrap();
        if !books.contains_key(session_hex) {
            drop(books);
            tracing::warn!(
                session = session_hex,
                "consent decision ignored — no active prompt for this session"
            );
            return false;
        }
        let kind = if allow {
            SentinelKind::Approve
        } else {
            SentinelKind::Deny
        };
        let landed = self.write_sentinel(session_hex, kind);
        drop(books);
        match landed {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(session = session_hex, %e, "consent sentinel write failed");
                false
            }
        }
    }

    /// Path of the `.pending` request marker for `session_hex`. Distinct from the
    /// `.approve`/`.deny` decision sentinels: this one is written by the AGENT
    /// (not the operator) when an attended session is awaiting a decision, so the
    /// tray can discover it and pop a prompt. Removed when the decision resolves.
    pub fn pending_path(&self, session_hex: &str) -> PathBuf {
        self.inner
            .sentinel_dir
            .join(format!("{session_hex}.pending"))
    }

    /// Write the `.pending` request marker (`body` = JSON describing the request:
    /// controller, permissions, timeout) for `session_hex`. Called by the
    /// signaling layer, which holds that metadata, right before an attended
    /// prompt begins. The tray watches for these files; the poll loop removes
    /// this one when the decision resolves. Best-effort — a write failure just
    /// means the tray falls back to its CLI-less state; the CLI path still works.
    /// FR-27 — the typed form of [`Self::write_pending`]. Every subsystem
    /// should use this; the raw string form stays for tests.
    pub fn write_prompt(&self, session_hex: &str, prompt: &PendingPrompt<'_>) -> Result<PathBuf> {
        self.write_pending(session_hex, &prompt.to_body(session_hex))
    }

    pub fn write_pending(&self, session_hex: &str, body: &str) -> Result<PathBuf> {
        let path = self.pending_path(session_hex);
        std::fs::write(&path, body)
            .with_context(|| format!("writing pending marker {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        Ok(path)
    }
}

/// Sentinel-file flavour. The two-file layout (separate `.approve` /
/// `.deny`) means an operator-typo'd command won't accidentally flip
/// a previous decision: each is its own touch-create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelKind {
    Approve,
    Deny,
}

impl SentinelKind {
    fn suffix(self) -> &'static str {
        match self {
            SentinelKind::Approve => "approve",
            SentinelKind::Deny => "deny",
        }
    }

    pub fn from_flags(approve: bool, deny: bool) -> Result<Self> {
        match (approve, deny) {
            (true, false) => Ok(SentinelKind::Approve),
            (false, true) => Ok(SentinelKind::Deny),
            (true, true) => Err(anyhow::anyhow!(
                "pass exactly one of --approve / --deny, not both"
            )),
            (false, false) => Err(anyhow::anyhow!("pass exactly one of --approve / --deny")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("roomler-consent-{name}-{nanos}"))
    }

    /// P2b review L3: a symlinked sentinel dir would let whoever planted the
    /// link redirect decision files into a directory they control.
    #[cfg(unix)]
    #[test]
    fn new_refuses_symlinked_sentinel_dir() {
        let base = fixture_dir("symlink");
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = ConsentBroker::new(Mode::AutoGrant, link);
        assert!(err.is_err(), "symlinked sentinel dir must be refused");

        // A regular dir at the same depth keeps working.
        assert!(ConsentBroker::new(Mode::AutoGrant, real).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// FR-93 P6f — a question a notification shows reaches the companion as
    /// one to LIST: every companion renders a marker that is not `native`, so
    /// a new word would put a second Approve on the screen for it.
    #[test]
    fn a_notification_prompt_is_listed_not_asked_again() {
        assert_eq!(PromptSurface::Notification.wire(), "native");
        let body = PendingPrompt {
            kind: PromptKind::RemoteControl,
            asked_by: "Alice",
            permissions: "VIEW".into(),
            detail: String::new(),
            org: String::new(),
            timeout: Duration::from_secs(30),
            surface: PromptSurface::Notification,
        }
        .to_body("6a00000000000000000000aa");
        let req: tunnel_core::localapi::ConsentRequest = serde_json::from_str(&body).unwrap();
        assert_eq!(req.surface, "native");
    }

    #[test]
    fn mode_from_config_default_is_auto_grant() {
        // Default agent config has auto_grant_session = true; this
        // contract is what keeps the auto-grant unchanged for
        // self-hosted users on the upgrade.
        assert_eq!(Mode::from_config(true), Mode::AutoGrant);
    }

    #[test]
    fn mode_from_config_disabled_uses_prompt_timeout() {
        match Mode::from_config(false) {
            Mode::Prompt { timeout } => assert_eq!(timeout, DEFAULT_PROMPT_TIMEOUT),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    /// FR-27 — the local switch is a FLOOR. A device that set
    /// `auto_grant_session = false` must keep prompting even when the server
    /// directs `Auto`; before this, the directive simply won, which inverted
    /// the gate-4 property every other subsystem here relies on.
    #[test]
    fn a_local_refusal_to_auto_grant_beats_a_server_auto() {
        let local = Mode::from_config(false); // auto_grant_session = false
        let host_window = Duration::from_secs(30);
        assert_eq!(
            strictest_of(Some(Mode::AutoGrant), local, host_window),
            Mode::Prompt {
                timeout: host_window
            },
            "a server Auto must not lift a local refusal"
        );
    }

    /// …and the downgraded prompt runs for the window the SERVER is waiting on,
    /// not the local default. A shorter local one would deny a session the
    /// server would still have accepted an answer for.
    #[test]
    fn the_downgraded_prompt_uses_the_server_window() {
        let local = Mode::from_config(false);
        assert_eq!(
            local,
            Mode::Prompt {
                timeout: DEFAULT_PROMPT_TIMEOUT
            }
        );
        let long = Duration::from_secs(300);
        assert_ne!(long, DEFAULT_PROMPT_TIMEOUT);
        assert_eq!(
            strictest_of(Some(Mode::AutoGrant), local, long),
            Mode::Prompt { timeout: long }
        );
    }

    /// The permissive direction is unchanged: a device happy to auto-grant
    /// still obeys a server that asks for a prompt.
    #[test]
    fn a_permissive_device_still_obeys_a_prompt_directive() {
        let local = Mode::from_config(true); // the default
        let w = Duration::from_secs(30);
        let directed = Mode::Prompt { timeout: w };
        assert_eq!(strictest_of(Some(directed), local, w), directed);
        assert_eq!(
            strictest_of(Some(Mode::AutoGrant), local, w),
            Mode::AutoGrant
        );
    }

    /// An older server sends no directive at all — the device's own setting is
    /// then the whole answer, in both directions.
    #[test]
    fn no_directive_falls_back_to_the_local_mode() {
        let w = Duration::from_secs(30);
        assert_eq!(strictest_of(None, Mode::AutoGrant, w), Mode::AutoGrant);
        let p = Mode::Prompt {
            timeout: Duration::from_secs(7),
        };
        assert_eq!(strictest_of(None, p, w), p);
    }

    #[test]
    fn decision_granted_maps_to_true() {
        assert!(Decision::Granted.granted());
        assert!(!Decision::Denied.granted());
        assert!(!Decision::Timeout.granted());
        assert!(!Decision::Cancelled.granted());
    }

    #[test]
    fn sentinel_kind_from_flags_validates_exclusivity() {
        assert_eq!(
            SentinelKind::from_flags(true, false).unwrap(),
            SentinelKind::Approve
        );
        assert_eq!(
            SentinelKind::from_flags(false, true).unwrap(),
            SentinelKind::Deny
        );
        assert!(SentinelKind::from_flags(true, true).is_err());
        assert!(SentinelKind::from_flags(false, false).is_err());
    }

    #[tokio::test]
    async fn auto_grant_resolves_immediately() {
        let dir = fixture_dir("auto");
        let broker = ConsentBroker::new(Mode::AutoGrant, dir.clone()).unwrap();
        let start = Instant::now();
        let decision = broker.request("abc123").await;
        assert_eq!(decision, Decision::Granted);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "auto-grant must not block on any I/O"
        );
        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn record_decision_requires_an_active_prompt() {
        // The LocalAPI decision path (P2b) must IGNORE a decision for a session
        // that isn't currently being prompted (no pre-approval / confused
        // deputy) and HONOR one that is.
        let dir = fixture_dir("record");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(5),
            },
            dir.clone(),
        )
        .unwrap();
        let sess = "0123456789abcdef01234567";

        // Not pending yet ⇒ rejected, and NO sentinel is written.
        assert!(!broker.record_decision(sess, true));
        assert!(!broker.sentinel_path(sess, SentinelKind::Approve).exists());

        // Start a prompt; once it's pending, a decision is honored + resolves it.
        let b2 = broker.clone();
        let s2 = sess.to_string();
        let prompt = tokio::spawn(async move { b2.request(&s2).await });
        let mut recorded = false;
        for _ in 0..100 {
            if broker.record_decision(sess, true) {
                recorded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            recorded,
            "decision should be accepted once the session is pending"
        );
        assert_eq!(prompt.await.unwrap(), Decision::Granted);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn request_with_mode_overrides_startup_mode() {
        // Server-authoritative consent (Phase 2): the per-session directive from
        // `ServerMsg::Request { consent_mode }` wins over the broker's startup
        // (`auto_grant_session`) mode in BOTH directions.
        let dir = fixture_dir("directive");

        // Startup = AutoGrant, but the server directs Prompt → we take the prompt
        // path and (no sentinel) time out, proving auto-grant was NOT used.
        let auto_broker = ConsentBroker::new(Mode::AutoGrant, dir.clone()).unwrap();
        let d = auto_broker
            .request_with_mode(
                "aaaaaaaaaaaaaaaaaaaaaaaa",
                Mode::Prompt {
                    timeout: Duration::from_millis(80),
                },
            )
            .await;
        assert_eq!(
            d,
            Decision::Timeout,
            "Prompt directive must override startup AutoGrant"
        );

        // Startup = Prompt, but the server directs Auto → immediate grant, no
        // sentinel wait.
        let prompt_broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(30),
            },
            dir.clone(),
        )
        .unwrap();
        let start = Instant::now();
        let d = prompt_broker
            .request_with_mode("bbbbbbbbbbbbbbbbbbbbbbbb", Mode::AutoGrant)
            .await;
        assert_eq!(
            d,
            Decision::Granted,
            "Auto directive must override startup Prompt"
        );
        assert!(start.elapsed() < Duration::from_millis(200));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pending_marker_written_and_cleared_on_resolve() {
        // Phase 3 — the signaling layer drops a `.pending` marker so the tray
        // can prompt; the broker's poll loop clears it once the decision lands.
        let dir = fixture_dir("pending");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(5),
            },
            dir.clone(),
        )
        .unwrap();
        let session = "cccccccccccccccccccccccc";

        let pending = broker
            .write_pending(session, r#"{"session_id":"c","controller_name":"x"}"#)
            .unwrap();
        assert!(pending.exists(), ".pending must exist while awaiting");
        assert_eq!(broker.pending_path(session), pending);

        // Operator approves AFTER the prompt starts. P2b: a pre-dropped approve
        // sentinel is purged at prompt start, so the decision must arrive once
        // the session is actively pending (delivered here via record_decision,
        // the LocalAPI path, which gates on the live pending set).
        let b2 = broker.clone();
        let s2 = session.to_string();
        let prompt = tokio::spawn(async move {
            b2.request_with_mode(
                &s2,
                Mode::Prompt {
                    timeout: Duration::from_secs(5),
                },
            )
            .await
        });
        let mut recorded = false;
        for _ in 0..100 {
            if broker.record_decision(session, true) {
                recorded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            recorded,
            "approval should record once the session is pending"
        );
        assert_eq!(prompt.await.unwrap(), Decision::Granted);
        assert!(!pending.exists(), ".pending must be cleared once resolved");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FR-27 field 2026-08-29: an SSH dial whose transport reset mid-prompt
    /// dropped the awaiting future before any decision, and the `.pending`
    /// marker survived — `roomlerd consent --list` then showed a phantom that
    /// outlived its session. Cancelling the prompt future must clear the marker.
    #[tokio::test]
    async fn a_cancelled_prompt_does_not_leak_its_pending_marker() {
        let dir = fixture_dir("cancel");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(30),
            },
            dir.clone(),
        )
        .unwrap();
        let session = "6a00000000000000000000ff";
        broker
            .write_prompt(
                session,
                &PendingPrompt {
                    kind: PromptKind::Ssh,
                    asked_by: "tester",
                    permissions: String::new(),
                    detail: "run a command".into(),
                    org: String::new(),
                    timeout: Duration::from_secs(30),
                    surface: PromptSurface::Companion,
                },
            )
            .unwrap();
        let marker = broker.pending_path(session);
        assert!(marker.exists(), "precondition: the marker was written");

        // Await in a task, let it register as pending and enter the poll loop,
        // then ABORT it — cancelling the task drops the future, which is exactly
        // what a dead transport does: nobody polls the prompt again.
        let b = broker.clone();
        let sess = session.to_string();
        let handle = tokio::spawn(async move {
            b.request_with_mode(
                &sess,
                Mode::Prompt {
                    timeout: Duration::from_secs(30),
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        handle.abort();
        let _ = handle.await; // observe the cancellation + let the drop run
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            !marker.exists(),
            "a cancelled prompt must not leak its .pending marker"
        );
        // In-memory entry cleared too: a decision for it now finds no active
        // prompt (record_decision gates on the live pending set).
        assert!(
            !broker.record_decision(session, true),
            "a cancelled prompt must clear the in-memory pending entry"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window every #1632 test prompts with: long enough that a broker
    /// which only ever resolves at its deadline cannot pass by accident.
    const LONG_WINDOW: Mode = Mode::Prompt {
        timeout: Duration::from_secs(30),
    };

    /// How long a cancelled prompt may take to resolve before the test calls
    /// it "waited for the window". The poll interval is 250 ms; 3 s is an
    /// order of magnitude of slack for a loaded CI box.
    const PROMPT_BOUND: Duration = Duration::from_secs(3);

    /// Approve `session` through the LocalAPI path once its prompt is live.
    async fn approve_once_pending(broker: &ConsentBroker, session: &str) {
        for _ in 0..100 {
            if broker.record_decision(session, true) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{session}: the prompt never became pending");
    }

    /// #1632 — a `Terminate` for a session whose prompt still stands takes
    /// the prompt down with it: `cancel()` resolves the pending future
    /// `Cancelled` inside the poll interval. Before this, nothing reached a
    /// running prompt and the person at the machine was asked to decide on a
    /// session that no longer existed, until the window elapsed.
    #[tokio::test]
    async fn cancel_resolves_a_pending_prompt_promptly() {
        let dir = fixture_dir("cancel-prompt");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "16320000000000000000aaaa";

        let handle = tokio::spawn(broker.request_with_mode(session, LONG_WINDOW));
        // Let it enter the poll loop, so this exercises the sleeping branch
        // of the select (the unpolled case has its own test below).
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            broker.cancel(session),
            "a running prompt reports that it was cancelled"
        );
        let decision = tokio::time::timeout(PROMPT_BOUND, handle)
            .await
            .expect("the prompt must resolve promptly after cancel, not at its 30 s window")
            .unwrap();
        assert_eq!(decision, Decision::Cancelled);
        // The in-memory entry is gone: a late click finds no active prompt.
        assert!(
            !broker.record_decision(session, true),
            "a cancelled prompt must clear the in-memory pending entry"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1632 — the race the RC path actually has: the WS loop arms the
    /// prompt, spawns the task that awaits it, and the very next frame it
    /// reads is the `Terminate`. The spawned task may not have been polled
    /// yet. Registration is therefore eager: a cancel issued after
    /// `request_with_mode` RETURNS reaches the prompt even though its future
    /// has never been polled.
    #[tokio::test]
    async fn a_cancel_before_the_first_poll_still_takes() {
        let dir = fixture_dir("cancel-unpolled");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "16320000000000000000bbbb";

        let prompt = broker.request_with_mode(session, LONG_WINDOW);
        assert!(
            broker.cancel(session),
            "the session must be registered before its future is first polled"
        );
        let decision = tokio::time::timeout(PROMPT_BOUND, prompt)
            .await
            .expect("a prompt cancelled before its first poll must still resolve promptly");
        assert_eq!(decision, Decision::Cancelled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #3433 — a withdrawn question takes no answer from the moment it is
    /// withdrawn. `cancel` used to only wake the prompt: its entry stood until
    /// the prompt next ran, and an answer in between was taken, and won, since
    /// the poll looks for a decision before it looks for the cancel. The
    /// record prompt is armed before it is shown, so "next ran" can be a
    /// companion's start away.
    #[tokio::test]
    async fn a_withdrawn_question_takes_no_answer() {
        let dir = fixture_dir("cancel-takes-no-answer");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "34330000000000000000c0c0";

        let prompt = broker.request_with_mode(session, LONG_WINDOW);
        assert!(broker.cancel(session), "a prompt was standing");
        let taken = broker.record_decision(session, true);
        let decision = tokio::time::timeout(PROMPT_BOUND, prompt)
            .await
            .expect("a withdrawn prompt must still resolve promptly");
        assert_eq!(
            (taken, decision),
            (false, Decision::Cancelled),
            "an answer after the withdrawal was taken, and decided it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #3433 — an answer that counted is never reported as refused. The
    /// prompt's poll takes a sentinel the moment it exists and removes it. The
    /// sentinel used to be chmod'ed after it was written, and that chmod
    /// failed, "No such file or directory", once the poll had taken the file:
    /// `record_decision` said false for an answer that had already decided the
    /// prompt. A thread here takes every sentinel the moment it appears, as the
    /// poll does.
    #[cfg(unix)]
    #[test]
    fn an_answer_the_poll_takes_at_once_still_counts() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = fixture_dir("sentinel-taken-at-once");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let taker = {
            let (dir, done) = (dir.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        for e in rd.flatten() {
                            if e.path().extension().is_some_and(|x| x == "deny") {
                                let _ = std::fs::remove_file(e.path());
                            }
                        }
                    }
                }
            })
        };
        let mut misreported = 0;
        for i in 0..5_000u32 {
            let session = format!("3433{i:020x}");
            let _armed = broker.arm(&session, Duration::from_secs(30));
            if !broker.record_decision(&session, false) {
                misreported += 1;
            }
        }
        done.store(true, Ordering::Relaxed);
        taker.join().unwrap();
        assert_eq!(
            misreported, 0,
            "answers that counted were reported as refused"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1632 — registration is eager, so a decision can be recorded between
    /// `request_with_mode` returning and its future's first poll. That
    /// decision must be honoured: the stale-sentinel purge runs at arm time,
    /// BEFORE the session is registered, never at the first poll (which ate
    /// exactly this Approve while this fix was being written).
    #[tokio::test]
    async fn a_decision_recorded_before_the_first_poll_is_honoured() {
        let dir = fixture_dir("armed-then-approved");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "16320000000000000000a5a5";

        let prompt = broker.request_with_mode(session, LONG_WINDOW);
        assert!(
            broker.record_decision(session, true),
            "the session is pending from the moment request_with_mode returns"
        );
        let decision = tokio::time::timeout(PROMPT_BOUND, prompt)
            .await
            .expect("an approve recorded before the first poll must resolve the prompt");
        assert_eq!(decision, Decision::Granted);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1632 — a cancel for a session nobody is prompting for is a no-op:
    /// `false`, and nothing remembered — a prompt for that very id raised
    /// afterwards runs to its own answer instead of inheriting the cancel.
    #[tokio::test]
    async fn cancel_of_an_unknown_session_is_a_no_op() {
        let dir = fixture_dir("cancel-unknown");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "16320000000000000000cccc";

        assert!(!broker.cancel(session), "never prompted");
        assert!(!broker.cancel(""), "not even an id");

        let handle = tokio::spawn(broker.request_with_mode(session, LONG_WINDOW));
        approve_once_pending(&broker, session).await;
        assert_eq!(
            handle.await.unwrap(),
            Decision::Granted,
            "a cancel that found nothing must not be remembered against a later prompt"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1632 — a cancel that arrives AFTER the prompt resolved is inert: it
    /// reports `false`, and neither a prompt for another session nor a
    /// re-prompt of the same id sees it.
    #[tokio::test]
    async fn a_cancel_after_the_prompt_resolved_does_not_touch_the_next_prompt() {
        let dir = fixture_dir("cancel-late");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let a = "16320000000000000000dddd";
        let b = "16320000000000000000eeee";

        let first = tokio::spawn(broker.request_with_mode(a, LONG_WINDOW));
        approve_once_pending(&broker, a).await;
        assert_eq!(first.await.unwrap(), Decision::Granted);

        assert!(!broker.cancel(a), "the prompt is over; nothing to cancel");

        for session in [b, a] {
            let handle = tokio::spawn(broker.request_with_mode(session, LONG_WINDOW));
            approve_once_pending(&broker, session).await;
            assert_eq!(
                handle.await.unwrap(),
                Decision::Granted,
                "{session}: a late cancel must not bleed into the next prompt"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1632 — the sharper form of the above: a cancel that DID reach a prompt
    /// stores a permit on that prompt's handle. When the prompt then goes away
    /// (here: dropped unpolled, the same path a dead transport takes) and the
    /// SAME session id is prompted again, the new prompt must not consume the
    /// old permit — each prompt registers its own handle.
    #[tokio::test]
    async fn a_stale_cancel_permit_does_not_leak_into_a_re_prompt() {
        let dir = fixture_dir("cancel-stale-permit");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "16320000000000000000ffff";

        let doomed = broker.request_with_mode(session, LONG_WINDOW);
        assert!(broker.cancel(session), "the permit is now stored");
        drop(doomed); // the guard runs: entry + marker gone

        let again = tokio::spawn(broker.request_with_mode(session, LONG_WINDOW));
        approve_once_pending(&broker, session).await;
        assert_eq!(
            again.await.unwrap(),
            Decision::Granted,
            "the re-prompt must answer on its own handle, not the cancelled one's"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #3427 — a question taken off the screen takes no more answers. The
    /// companion, the CLI and the person go by the marker; `record_decision`
    /// goes by the in-memory entry. With the marker removed first, an answer
    /// sent the moment it went was still taken, for a session that had ended
    /// (`control_dc_record`'s six-round cell under load, #1861). The unlink
    /// here is the test's, so it can answer at exactly that moment: from
    /// another thread, as the companion's answer comes, since the marker goes
    /// under the books' lock (#3433) and that answer waits for it.
    #[test]
    fn a_question_off_the_screen_takes_no_answer() {
        let dir = fixture_dir("release-order");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "34270000000000000000abcd";
        broker.write_pending(session, "{}").unwrap();
        let armed = broker.arm(session, Duration::from_secs(30));
        let marker = broker.pending_path(session);
        assert!(
            marker.exists(),
            "precondition: the question is on the screen"
        );

        let (mut under_the_books, mut answer) = (None, None);
        armed.guard.release(|m| {
            let _ = std::fs::remove_file(m);
            under_the_books = Some(broker.inner.pending.try_lock().is_err());
            let broker = broker.clone();
            answer = Some(std::thread::spawn(move || {
                broker.record_decision(session, true)
            }));
        });
        assert_eq!(
            under_the_books,
            Some(true),
            "the marker went outside the books' lock"
        );
        assert_eq!(
            answer.map(|t| t.join().unwrap()),
            Some(false),
            "an answer sent as the marker went was still taken"
        );
        assert!(!marker.exists());
        drop(armed); // its guard releases again: a no-op

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #3433 — a withdrawn question takes no answer that lands after the
    /// withdrawal, whoever writes it. An Allow that passed `record_decision`'s
    /// check just before a session's end lands after the cancel, and the CLI's
    /// fallback writes its sentinel with no check at all. The poll looked for
    /// an answer before it looked at the books, so its first look after the
    /// cancel took it: Granted, for a session that had ended.
    #[tokio::test]
    async fn an_answer_that_lands_after_the_withdrawal_is_not_taken() {
        let dir = fixture_dir("lands-after-cancel");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "34330000000000000000c1c1";

        let prompt = broker.request_with_mode(session, LONG_WINDOW);
        assert!(broker.cancel(session), "a prompt was standing");
        broker
            .write_sentinel(session, SentinelKind::Approve)
            .unwrap();
        let decision = tokio::time::timeout(PROMPT_BOUND, prompt)
            .await
            .expect("a withdrawn prompt must still resolve promptly");
        assert_eq!(
            decision,
            Decision::Cancelled,
            "an answer that landed after the withdrawal decided it"
        );
        assert!(
            !broker
                .sentinel_path(session, SentinelKind::Approve)
                .exists(),
            "the answer that came too late stayed behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #3433 — a decided question takes no more answers. The prompt broke out
    /// of its poll, cleared its sentinels and logged its decision, and only
    /// then did its guard take it off the books: an answer in that gap was
    /// written after the clearing, reported taken for nothing, and stayed
    /// behind. The answer here comes at the decision's own log line, inside
    /// that gap.
    #[tokio::test]
    async fn a_decided_question_takes_no_more_answers() {
        use tracing_subscriber::layer::SubscriberExt;
        let dir = fixture_dir("decided-takes-no-more");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "34330000000000000000d2d2";
        let late = Arc::new(Mutex::new(None));
        // ⚠️ A second dispatcher, alive as long as this one. With only one
        // registered, tracing takes the interest of a callsite first hit on
        // ANOTHER thread from that thread's own default (none) and caches
        // "never" for every thread. The other tests resolve prompts at the
        // same time, and this hook then never heard the decision.
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let _log = tracing::subscriber::set_default(tracing_subscriber::registry().with(
            AnswerAtTheDecision {
                broker: broker.clone(),
                session,
                late: late.clone(),
            },
        ));

        let prompt = broker.request_with_mode(session, LONG_WINDOW);
        assert!(broker.record_decision(session, true));
        let decision = tokio::time::timeout(PROMPT_BOUND, prompt)
            .await
            .expect("an answered prompt must resolve promptly");
        assert_eq!(decision, Decision::Granted);
        assert_eq!(
            *late.lock().unwrap(),
            Some(false),
            "a question already decided took another answer"
        );
        assert!(
            !broker.sentinel_path(session, SentinelKind::Deny).exists(),
            "the answer that came too late stayed behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Answers `session` once, with a Deny, the moment its prompt logs the
    /// decision it made: what a click lands on when it comes just too late.
    struct AnswerAtTheDecision {
        broker: ConsentBroker,
        session: &'static str,
        late: Arc<Mutex<Option<bool>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AnswerAtTheDecision {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Message(String);
            impl tracing::field::Visit for Message {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    if f.name() == "message" {
                        self.0 = format!("{v:?}");
                    }
                }
            }
            let mut m = Message(String::new());
            event.record(&mut m);
            if m.0 == "operator consent decision" {
                let mut late = self.late.lock().unwrap();
                if late.is_none() {
                    *late = Some(self.broker.record_decision(self.session, false));
                }
            }
        }
    }

    /// #3433 — a question's end leaves a newer question for the same id alone.
    /// An RC session can be asked again under its own id while its first
    /// question still stands: arming the second takes the id's entry, and the
    /// first one's guard then took their shared marker off the screen as it
    /// went, leaving the second question asking with nothing to show it.
    #[test]
    fn an_older_question_leaves_a_newer_ones_marker() {
        let dir = fixture_dir("older-leaves-newer");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "34330000000000000000e3e3";
        let older = broker.arm(session, Duration::from_secs(30));
        let newer = broker.arm(session, Duration::from_secs(30));
        let marker = broker.write_pending(session, "{}").unwrap();

        drop(older);
        assert!(
            marker.exists(),
            "an older question took a newer one's marker off the screen"
        );
        assert!(
            broker.record_decision(session, true),
            "the newer question stopped taking answers"
        );
        drop(newer);
        assert!(!marker.exists(), "the newer question's end left its marker");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #3433 review, F2 — a newer question for the same id clears the old
    /// answers and takes the id on the books in one step. Cleared outside the
    /// books and placed under them after, an answer recorded in between, for
    /// the older question still on the books, was left for the newer one,
    /// whose first look took an answer given before it existed.
    ///
    /// The books are held here while the newer question is armed. An arm that
    /// clears outside them clears at once (a leftover going says so), and the
    /// answer is then given as `record_decision` gives it: under the books,
    /// for the question they hold.
    #[tokio::test]
    async fn a_newer_question_takes_no_answer_given_before_it() {
        let dir = fixture_dir("newer-takes-none-before");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "34330000000000000000f4f4";
        let _older = broker.arm(session, Duration::from_secs(30));
        let leftover = broker.write_sentinel(session, SentinelKind::Deny).unwrap();

        let books = broker.inner.pending.lock().unwrap();
        let arming = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.arm(session, Duration::from_millis(300)))
        };
        let waited = Instant::now();
        while leftover.exists() && waited.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(books.contains_key(session), "the older question was asking");
        broker
            .write_sentinel(session, SentinelKind::Approve)
            .unwrap();
        drop(books);

        let newer = arming.join().unwrap();
        let decision = tokio::time::timeout(PROMPT_BOUND, newer.run())
            .await
            .expect("the newer prompt must resolve within its window");
        assert_eq!(
            decision,
            Decision::Timeout,
            "a newer question took an answer given before it existed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1632 — the marker the companion (and `roomlerd consent --list`) reads
    /// is removed on cancel, so the companion's 750 ms poll finds nothing
    /// pending and takes its own panel down (roomler-desktop
    /// `consent_watch_loop`). The native panel is the caller's to hide.
    #[tokio::test]
    async fn cancel_removes_the_pending_marker() {
        let dir = fixture_dir("cancel-marker");
        let broker = ConsentBroker::new(LONG_WINDOW, dir.clone()).unwrap();
        let session = "16320000000000000000abcd";
        broker
            .write_prompt(
                session,
                &PendingPrompt {
                    kind: PromptKind::RemoteControl,
                    asked_by: "tester",
                    permissions: "view|control".into(),
                    detail: String::new(),
                    org: String::new(),
                    timeout: Duration::from_secs(30),
                    surface: PromptSurface::Companion,
                },
            )
            .unwrap();
        let marker = broker.pending_path(session);
        assert!(marker.exists(), "precondition: the marker was written");

        let handle = tokio::spawn(broker.request_with_mode(session, LONG_WINDOW));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(marker.exists(), "the marker stands while the prompt does");
        assert!(broker.cancel(session));
        assert_eq!(
            tokio::time::timeout(PROMPT_BOUND, handle)
                .await
                .expect("resolves promptly")
                .unwrap(),
            Decision::Cancelled
        );
        assert!(
            !marker.exists(),
            "the .pending marker must be gone once the prompt is cancelled"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_resolves_when_approve_sentinel_appears() {
        let dir = fixture_dir("approve");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(5),
            },
            dir.clone(),
        )
        .unwrap();
        let session_hex = "deadbeef".to_string();
        // Schedule a sentinel write 100ms in the future. The
        // 250ms poll interval should pick it up on the second poll.
        let broker_for_writer = broker.clone();
        let session_for_writer = session_hex.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            broker_for_writer
                .write_sentinel(&session_for_writer, SentinelKind::Approve)
                .unwrap();
        });
        let decision = broker.request(&session_hex).await;
        writer.await.unwrap();
        assert_eq!(decision, Decision::Granted);
        // Sentinel was cleaned up.
        assert!(
            !broker
                .sentinel_path(&session_hex, SentinelKind::Approve)
                .exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_resolves_when_deny_sentinel_appears() {
        let dir = fixture_dir("deny");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(5),
            },
            dir.clone(),
        )
        .unwrap();
        let session_hex = "feed".to_string();
        let broker_for_writer = broker.clone();
        let session_for_writer = session_hex.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            broker_for_writer
                .write_sentinel(&session_for_writer, SentinelKind::Deny)
                .unwrap();
        });
        let decision = broker.request(&session_hex).await;
        assert_eq!(decision, Decision::Denied);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_times_out_when_no_sentinel_arrives() {
        let dir = fixture_dir("timeout");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_millis(400),
            },
            dir.clone(),
        )
        .unwrap();
        let start = Instant::now();
        let decision = broker.request("nopromptever").await;
        assert_eq!(decision, Decision::Timeout);
        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "must respect timeout duration"
        );
        assert!(
            start.elapsed() < Duration::from_millis(900),
            "must not run far past timeout (got {:?})",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn request_with_empty_session_id_denies_safely() {
        // Defensive: a malformed signaling message that arrives with
        // an empty session_hex must NOT cause the broker to scan the
        // entire sentinel dir for a stale `.approve`. Default-deny.
        let dir = fixture_dir("empty");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(60),
            },
            dir.clone(),
        )
        .unwrap();
        let start = Instant::now();
        assert_eq!(broker.request("").await, Decision::Denied);
        assert!(start.elapsed() < Duration::from_millis(50));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// RETIRED-NAME-ANCHOR(35): this whole module exists BECAUSE the retired names
// keep coming back into that log line, so it has to spell them out to assert
// they are gone. Naming them is the test.
#[cfg(test)]
mod cli_name_tests {
    /// The guidance in [`ConsentBroker::request`]'s log line is, on a headless
    /// host, the ONLY thing telling an operator how to answer — and it has now
    /// been wrong twice: FR-21 retired `roomler-agent`, and the first repair
    /// swapped it for `roomler consent`, which the `roomler` CLI does not have
    /// (these subcommands live on the DAEMON's clap surface, alongside
    /// `roomlerd enroll` / `roomlerd run`; `roomler` is the tunnel + fleet CLI
    /// and its `Commands` enum has no `consent` arm). Caught by actually
    /// running it: `roomler consent --help` → `unrecognized subcommand`.
    ///
    /// Nothing else fails when this string rots, so assert it.
    #[test]
    fn the_log_line_names_a_command_that_exists() {
        // Scan only what precedes THIS module: the prose above quotes the
        // wrong spellings in order to name them, and a whole-file scan would
        // fail on its own explanation.
        let full = include_str!("consent.rs");
        let src = full.split("mod cli_name_tests").next().unwrap();
        assert!(
            !src.contains("`roomler consent"),
            "the guidance must say `roomlerd consent` — `roomler` has no consent subcommand"
        );
        assert!(
            !src.contains("roomler-agent consent"),
            "FR-21 retired the `roomler-agent` binary name"
        );
        assert!(
            src.contains("roomlerd consent --session"),
            "the guidance must still tell the operator the actual command"
        );
    }
}
