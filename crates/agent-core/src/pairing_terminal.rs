// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A4 — pairing from a terminal: what `roomlerd enroll --pair` runs
//! on top of the [`crate::pairing`] client, for `install.sh --pair`,
//! `install.ps1 -Pair` and a person at a prompt
//! (`docs/fr/FR-129-guided-cross-platform-installer.md`, Key design A,
//! invariant 10).
//!
//! The flow ([`pair_from_terminal`]):
//!
//! 1. `start` a pairing and print the **user code** and the page to type it
//!    on. The page's address never carries the code (operator decision 1):
//!    the person types it.
//! 2. Poll with RFC 8628's back-off until the browser decides, or the
//!    pairing's clock runs out. A poll that fails (a network blip, a pod
//!    roll, a VPN reconnect) is retried at a longer interval until that
//!    clock runs out, as the wizard retries it — never given up on early.
//! 3. On an approval, show who approved it into which org — the org's name
//!    **and its unique slug**, the approver's name **and their unique
//!    username** — and ask `Join …? [y/N]`. This is the hijack guard (the
//!    spec's Key design A, item 3) in its terminal shape: a display name can
//!    be set to anything, so the two unique handles are what the person
//!    checks. Nothing enrolls without a yes.
//! 4. The caller enrolls in-process with the delivered token, through the
//!    same path a pasted token takes.
//!
//! **The question goes to the terminal, never to stdin.** Under
//! `curl … | sh -s -- --pair` stdin is the pipe the shell is still reading
//! the script from, so a child that read it would eat the rest of the
//! install. The answer is read from `/dev/tty` on Unix and the console
//! (`CONIN$`) on Windows ([`TtyConsole`]). With neither there is nobody to
//! ask, and the only way to enroll is `--pair-org <slug>`: the approval must
//! name exactly that org. A flag that merely said "yes" would skip the guard;
//! a flag that names the org IS the guard, stated in advance — so it also
//! replaces the question on a terminal, and a mismatch refuses there too.
//!
//! **Exactly that org.** The server stores a slug as it was sent — only the
//! admin UI keeps slugs to lowercase letters, digits and dashes — under a
//! unique index that is case- and space-sensitive, so `ACME`, `acme ` and a
//! Cyrillic `аcme` are three OTHER orgs anybody can create through the API.
//! `--pair-org` therefore matches the approved slug byte for byte: a
//! comparison that folded case or trimmed the server's value would enroll
//! the device into a lookalike's org without asking anyone ([`decide`]).
//!
//! **What the person reads is what the server sent, made visible.** The
//! org's name and slug were typed by whoever made the org, the approver's
//! name and username by whoever made the account. A control sequence in any
//! of them could move the cursor and repaint the question with another
//! org's, so every server-sent string reaches the terminal through
//! [`shown`]: controls and invisible characters as escapes, everything else
//! as it is. A slug the admin UI could not have made, and a username with
//! characters a lookalike hides behind, are flagged.
//!
//! **Secrets.** The device code lives in a [`DeviceCode`] and is sent in the
//! poll body only; the delivered token is handed to the caller and is never
//! printed or logged. A test runs the whole flow against a loopback server
//! and scans both the terminal output and the log for either (AC3).
//!
//! [`DeviceCode`]: crate::pairing::DeviceCode

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use roomler_ai_remote_control::models::InstallMethod;

use crate::pairing::{
    self, Approved, PairRole, PollOutcome, StartOutcome, StartRequest, Started,
    interval_after_rate_limit, interval_after_slow_down,
};

/// The flag that names the org the approval must name, spelled once.
pub const EXPECT_ORG_FLAG: &str = "--pair-org";

/// The flag that takes a pasted token instead, for the messages.
pub const TOKEN_FLAG: &str = "--token";

/// How long a delivered enrollment token lives unused — the server mints
/// it with the admin route's 600 s — so a refusal can say what it leaves
/// behind.
pub const TOKEN_LIFE_MINUTES: u64 = 10;

/// Grace past the server's `expires_in` before the loop gives up on its own
/// clock. The server answers `expired_token` first in every normal case;
/// this is for a server that stopped answering.
const EXPIRY_GRACE: Duration = Duration::from_secs(30);

/// The longest a run of failed polls stretches the interval to. RFC 8628
/// §3.5 asks a client to poll less often after a connection failure
/// (doubling is its suggestion); past this the approval would be noticed
/// too late to be worth the saving.
pub const POLL_ERROR_BACKOFF_CAP_SECS: u64 = 30;

/// The interval after a failed poll: doubled, up to
/// [`POLL_ERROR_BACKOFF_CAP_SECS`] — and never shorter than the current one,
/// which a `slow_down` may already have taken past the cap.
pub fn interval_after_poll_error(current: u64) -> u64 {
    current.max(current.saturating_mul(2).min(POLL_ERROR_BACKOFF_CAP_SECS))
}

/// The `client` the start request names, from the install trace's method
/// (its wire spelling, as `InstallInfo` carries it): the served scripts set
/// `ROOMLER_INSTALL_METHOD`, and that is how the `/pair` page says
/// "install.sh" rather than "roomlerd". The server clamps anything else to
/// `other`, so only its own vocabulary is sent.
pub fn client_for(install_method: &str) -> &'static str {
    match InstallMethod::parse(install_method.trim()) {
        Some(InstallMethod::ScriptSh) => "script-sh",
        Some(InstallMethod::ScriptPs1) => "script-ps1",
        _ => "roomlerd",
    }
}

/// What the terminal flow needs to know. Nothing here is secret.
#[derive(Clone, Debug)]
pub struct PairParams<'a> {
    pub server: &'a str,
    /// The device's name, as `--name` gives it: shown on the `/pair` page.
    pub machine_name: &'a str,
    pub role: PairRole,
    /// `1` everywhere today; B2's macOS root half makes it `2`.
    pub halves: u8,
    /// [`client_for`].
    pub client: &'a str,
    pub client_version: &'a str,
    /// `--pair-org`: the slug the approval must name. With it the question
    /// is not asked — the check was stated in advance — and a mismatch
    /// refuses, terminal or not.
    pub expect_org: Option<&'a str>,
}

// ─── The console ─────────────────────────────────────────────────────────────

/// Where the yes/no question is asked. One real implementation
/// ([`TtyConsole`]) and whatever a test scripts.
pub trait Console {
    /// Is there a console to ask on? Checked before a pairing starts, so
    /// that nobody approves a pairing whose answer could never be given.
    fn available(&mut self) -> bool;
    /// Ask `question` and read one line. `None` means there is no console to
    /// ask on at all; `Some(false)` is a no, an empty line, or an EOF.
    fn confirm(&mut self, question: &str) -> Option<bool>;
}

/// The process's controlling terminal: `/dev/tty` on Unix, `CONIN$` /
/// `CONOUT$` on Windows — never stdin or stdout, which under a piped install
/// are the script and whatever the person redirected.
#[derive(Debug, Default)]
pub struct TtyConsole;

struct ConsoleHandles {
    input: File,
    output: File,
}

#[cfg(unix)]
fn open_console() -> Option<ConsoleHandles> {
    // Opening fails with ENXIO when the process has no controlling terminal
    // (cron, systemd, `ssh host cmd` without `-t`, a CI runner): exactly the
    // "nobody to ask" case.
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let output = input.try_clone().ok()?;
    Some(ConsoleHandles { input, output })
}

#[cfg(windows)]
fn open_console() -> Option<ConsoleHandles> {
    // The console attached to this process, whether or not stdin/stdout are
    // redirected. A process with no console (a service, a scheduled task, a
    // WinRM shell) cannot open either, which is the "nobody to ask" case.
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
        .ok()?;
    let output = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")
        .ok()?;
    Some(ConsoleHandles { input, output })
}

#[cfg(not(any(unix, windows)))]
fn open_console() -> Option<ConsoleHandles> {
    None
}

/// Write the question to the terminal: bytes on Unix, where the terminal
/// reads UTF-8.
#[cfg(not(windows))]
fn write_to_console(output: &mut File, text: &str) -> std::io::Result<()> {
    output.write_all(text.as_bytes())?;
    output.flush()
}

/// Write the question to the console as UTF-16. A plain write on `CONOUT$`
/// is `WriteFile`, whose bytes the console reads in its own code page —
/// the OEM one (437, 850 …) unless someone changed it — so the question's
/// `«»` and an org's non-ASCII name would arrive as mojibake, in the one
/// line the person is asked to read closely. `std`'s own stdout makes the
/// same choice for a console. If the handle refuses (it is not a console
/// after all), the rest goes out as bytes.
#[cfg(windows)]
fn write_to_console(output: &mut File, text: &str) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::WriteConsoleW;

    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut done = 0usize;
    while done < wide.len() {
        let rest = &wide[done..];
        let count = u32::try_from(rest.len()).unwrap_or(u32::MAX);
        let mut written: u32 = 0;
        // SAFETY: the handle is this process's own `CONOUT$`, open for the
        // whole call (`output` is borrowed); the pointer and the count
        // describe `rest`, which outlives the call; `written` is a valid
        // `u32` to write through; the reserved pointer must be null.
        let ok = unsafe {
            WriteConsoleW(
                output.as_raw_handle(),
                rest.as_ptr(),
                count,
                &mut written,
                std::ptr::null(),
            )
        };
        if ok == 0 || written == 0 {
            output.write_all(String::from_utf16_lossy(rest).as_bytes())?;
            return output.flush();
        }
        done += written as usize;
    }
    Ok(())
}

impl Console for TtyConsole {
    fn available(&mut self) -> bool {
        open_console().is_some()
    }

    fn confirm(&mut self, question: &str) -> Option<bool> {
        let mut tty = open_console()?;
        if write_to_console(&mut tty.output, &format!("{question} ")).is_err() {
            return None;
        }
        let mut line = String::new();
        match BufReader::new(&mut tty.input).read_line(&mut line) {
            // An EOF on the terminal (Ctrl-D) or a read error is not a yes.
            Ok(0) | Err(_) => Some(false),
            Ok(_) => Some(is_yes(&line)),
        }
    }
}

/// `y` / `yes`, case-insensitive, whitespace trimmed. Everything else — an
/// empty line included — is the default, no.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ─── What the person reads ───────────────────────────────────────────────────

/// A server-sent string as the terminal shows it: every control and
/// invisible character — ESC and the rest of C0 and C1, a carriage return,
/// the bidi overrides, zero-width marks, non-breaking spaces, a combining
/// mark — as its escape (`\u{1b}`, `\r`, `\u{202e}`), everything printable
/// as it is. Quotes and backslashes stay literal: they are printable, and
/// a display name may hold them.
pub fn shown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\'' | '"' | '\\' => out.push(c),
            _ => out.extend(c.escape_debug()),
        }
    }
    out
}

/// The shape the admin UI gives a slug (`ui/src/composables/useValidation.ts`):
/// lowercase ASCII letters and digits, joined by single dashes. The server
/// takes any string, so a slug outside this shape was made some other way —
/// which is what a lookalike (`ACME`, `acme `, a Cyrillic `а`) has to do.
pub fn slug_is_ui_shaped(slug: &str) -> bool {
    !slug.is_empty()
        && slug.split('-').all(|run| {
            !run.is_empty() && run.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'))
        })
}

/// A username with nothing a lookalike hides behind: printable ASCII and no
/// spaces. The admin UI allows more (it asks for three characters), so this
/// is a prompt to look closer, not a refusal.
pub fn username_is_plain(username: &str) -> bool {
    !username.is_empty() && username.chars().all(|c| c.is_ascii_graphic())
}

/// What the person reads to pair: the page and the code, and nothing else
/// (the device code is not here, and must never be).
pub fn code_block(started: &Started) -> String {
    let minutes = started.expires_in.div_ceil(60);
    format!(
        "\nTo pair this device, open   {}\nand type the code           {}\n\n(The code expires in {} minutes. Waiting for the approval…)\n",
        shown(&started.verification_uri),
        shown(&started.user_code),
        minutes
    )
}

/// The approval, with the two unique handles beside the display names, and
/// a warning beside any handle that needs a closer look.
pub fn approved_block(approved: &Approved) -> String {
    let mut s = String::from("\nApproved in the browser.\n");
    s.push_str(&format!(
        "  Organization: {}  (slug: {})\n",
        shown(&approved.org_name),
        approved
            .org_slug
            .as_deref()
            .map(shown)
            .unwrap_or_else(|| "not sent by this server".to_string())
    ));
    s.push_str(&format!(
        "  Approved by:  {}  (username: {})\n",
        shown(&approved.approver_name),
        approved
            .approver_username
            .as_deref()
            .map(shown)
            .unwrap_or_else(|| "not sent by this server".to_string())
    ));
    if approved.org_slug.is_none() || approved.approver_username.is_none() {
        s.push_str(
            "WARNING: this server did not send the org's unique slug and the approver's unique\n         username. A display name can be set to anything — check these carefully.\n",
        );
    }
    if let Some(slug) = approved.org_slug.as_deref()
        && !slug_is_ui_shaped(slug)
    {
        s.push_str(
            "WARNING: this slug is not one the admin UI makes (it allows only a-z, 0-9 and single\n         dashes), so it may be a lookalike of another org's. Compare it character by character.\n",
        );
    }
    if let Some(username) = approved.approver_username.as_deref()
        && !username_is_plain(username)
    {
        s.push_str(
            "WARNING: this username has spaces or characters outside plain ASCII, so it may be a\n         lookalike of another person's. Compare it character by character.\n",
        );
    }
    s
}

/// The question the terminal asks. The two unique handles are in it on
/// purpose: they are what the person checks.
pub fn question(approved: &Approved) -> String {
    format!(
        "Join «{}» ({}) as approved by «{}» ({})? [y/N]",
        shown(&approved.org_name),
        approved
            .org_slug
            .as_deref()
            .map(shown)
            .unwrap_or_else(|| "slug not sent".to_string()),
        shown(&approved.approver_name),
        approved
            .approver_username
            .as_deref()
            .map(|u| format!("@{}", shown(u)))
            .unwrap_or_else(|| "username not sent".to_string()),
    )
}

/// The older-server message: `start` answered 404 (a server from before
/// FR-129, or `[pairing] enabled = false`).
pub fn unavailable_message() -> String {
    format!(
        "this server does not support pairing (an older server, or pairing is switched off there); use {TOKEN_FLAG} with an enrollment token from the admin UI"
    )
}

/// What a refusal leaves behind, said every time: the device stays
/// unenrolled, the pairing stays `delivered`, the token goes unused.
fn leaves_behind() -> String {
    format!(
        "Nothing was enrolled. The pairing stays delivered on the server and its token goes unused; it expires in {TOKEN_LIFE_MINUTES} minutes. Run the command again to pair afresh."
    )
}

// ─── The decision ────────────────────────────────────────────────────────────

/// What the approval led to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JoinDecision {
    /// Enroll with the delivered token.
    Join,
    /// The person answered no (or nothing).
    DeclinedByPerson,
    /// `--pair-org` named one org and the approval another.
    OrgMismatch { expected: String, got: String },
    /// `--pair-org` was given but the server sent no slug to check against.
    OrgNotSent { expected: String },
    /// No terminal to ask on, and no `--pair-org` to decide by.
    NoConsole,
}

/// The no-terminal rule, said once: what to pass instead.
fn no_console_rule() -> String {
    format!(
        "there is no terminal to confirm on, and no {EXPECT_ORG_FLAG} <slug> to decide by. Without a terminal, pass {EXPECT_ORG_FLAG} <slug> (the org's slug, exactly as the admin UI shows it): the device enrolls only if the approval names that org"
    )
}

/// The refusal given BEFORE a pairing starts, when no terminal could answer
/// the question and no `--pair-org` stands in for it: nothing to leave
/// behind, and nobody's approval wasted.
pub fn no_console_before_start() -> String {
    format!("not starting a pairing: {}.", no_console_rule())
}

impl JoinDecision {
    /// The refusal, worded for the terminal. `None` for [`JoinDecision::Join`].
    pub fn refusal(&self) -> Option<String> {
        let why = match self {
            JoinDecision::Join => return None,
            JoinDecision::DeclinedByPerson => "you answered no".to_string(),
            JoinDecision::OrgMismatch { expected, got } => format!(
                "the approval names the org with slug `{}`, not `{}` ({EXPECT_ORG_FLAG} must match it exactly)",
                shown(got),
                shown(expected)
            ),
            JoinDecision::OrgNotSent { expected } => format!(
                "{EXPECT_ORG_FLAG} {} was given, but this server did not send the approved org's slug, so it cannot be checked",
                shown(expected)
            ),
            JoinDecision::NoConsole => no_console_rule(),
        };
        Some(format!("not enrolling: {why}. {}", leaves_behind()))
    }
}

/// The hijack guard. With `expect_org` the approval's slug decides — byte
/// for byte (the module's "Exactly that org") — and the console is never
/// consulted; without it the console is asked, and no console means no.
/// Only the person's own `expect_org` is trimmed; the server's slug is
/// compared as the server sent it.
pub fn decide(
    approved: &Approved,
    expect_org: Option<&str>,
    console: &mut dyn Console,
) -> JoinDecision {
    if let Some(expected) = expect_org {
        let expected = expected.trim();
        return match approved.org_slug.as_deref() {
            Some(got) if !expected.is_empty() && got == expected => JoinDecision::Join,
            Some(got) => JoinDecision::OrgMismatch {
                expected: expected.to_string(),
                got: got.to_string(),
            },
            None => JoinDecision::OrgNotSent {
                expected: expected.to_string(),
            },
        };
    }
    match console.confirm(&question(approved)) {
        Some(true) => JoinDecision::Join,
        Some(false) => JoinDecision::DeclinedByPerson,
        None => JoinDecision::NoConsole,
    }
}

// ─── The loop ────────────────────────────────────────────────────────────────

/// How a pairing ended on the server's side.
#[derive(Clone, Debug)]
pub enum PairEnd {
    Approved(Approved),
    Denied,
    Expired,
}

/// Poll until the browser decides or the pairing's clock runs out. `wait`
/// is the sleeper: seconds in, a future out — the real one sleeps, a test's
/// records. The interval grows on `slow_down` and on a 429 and never
/// shrinks ([`crate::pairing`]); a status this build does not know keeps
/// waiting; a failed poll is said on `out` and retried at a longer interval
/// ([`interval_after_poll_error`]) until the clock runs out, which then
/// ends the flow with the last failure rather than a plain expiry.
pub async fn poll_until_decided<S, Fut>(
    server: &str,
    started: &Started,
    out: &mut (dyn Write + Send),
    wait: S,
) -> Result<PairEnd>
where
    S: FnMut(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    let deadline = Instant::now() + Duration::from_secs(started.expires_in) + EXPIRY_GRACE;
    poll_until(server, started, out, wait, deadline).await
}

/// [`poll_until_decided`] with the deadline passed in.
pub async fn poll_until<S, Fut>(
    server: &str,
    started: &Started,
    out: &mut (dyn Write + Send),
    mut wait: S,
    deadline: Instant,
) -> Result<PairEnd>
where
    S: FnMut(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    // A server that said 0 would be hammered; the floor is the protocol's.
    let mut interval = started.interval.max(1);
    let mut last_error: Option<anyhow::Error> = None;
    let mut failures = 0u32;
    loop {
        if Instant::now() >= deadline {
            return match last_error {
                Some(e) => Err(e.context(format!(
                    "the pairing ran out while polling it was failing ({failures} times in a row). Nothing was enrolled; run the command again for a new code"
                ))),
                None => Ok(PairEnd::Expired),
            };
        }
        wait(interval).await;
        let outcome = match pairing::poll(server, &started.device_code).await {
            Ok(outcome) => outcome,
            Err(e) => {
                failures += 1;
                interval = interval_after_poll_error(interval);
                tracing::warn!(
                    error = format!("{e:#}"),
                    failures,
                    next_interval = interval,
                    "pairing: poll failed; trying again"
                );
                let _ = writeln!(
                    out,
                    "(polling the pairing failed: {}; trying again in {interval} s)",
                    shown(&format!("{e:#}"))
                );
                last_error = Some(e);
                continue;
            }
        };
        failures = 0;
        last_error = None;
        match outcome {
            PollOutcome::AuthorizationPending => {}
            PollOutcome::SlowDown { interval: theirs } => {
                interval = interval_after_slow_down(interval, theirs);
            }
            PollOutcome::RateLimited { retry_after } => {
                interval = interval_after_rate_limit(interval, retry_after);
            }
            PollOutcome::AccessDenied => return Ok(PairEnd::Denied),
            PollOutcome::ExpiredToken => return Ok(PairEnd::Expired),
            PollOutcome::Approved(approved) => return Ok(PairEnd::Approved(approved)),
            PollOutcome::Unknown(status) => {
                tracing::info!(
                    status,
                    "pairing: a status this build does not know; still waiting"
                );
            }
        }
    }
}

// ─── The whole flow ──────────────────────────────────────────────────────────

/// Pair on this terminal and hand back the approval — its token(s) go to
/// the caller's in-process enroll and nowhere else. Every refusal is an
/// error that says what it leaves behind; a server without pairing is an
/// error that names `--token`.
pub async fn pair_from_terminal(
    params: PairParams<'_>,
    out: &mut (dyn Write + Send),
    console: &mut (dyn Console + Send),
) -> Result<Approved> {
    pair_from_terminal_with(params, out, console, |secs| {
        tokio::time::sleep(Duration::from_secs(secs))
    })
    .await
}

/// [`pair_from_terminal`] with the sleeper passed in.
pub async fn pair_from_terminal_with<S, Fut>(
    params: PairParams<'_>,
    out: &mut (dyn Write + Send),
    console: &mut (dyn Console + Send),
    wait: S,
) -> Result<Approved>
where
    S: FnMut(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    // Before anything reaches the server: a pairing nobody could confirm
    // must not be started, or someone approves it in a browser for nothing.
    if params.expect_org.is_none() && !console.available() {
        bail!("{}", no_console_before_start());
    }
    let request = StartRequest {
        machine_name: params.machine_name.to_string(),
        os: pairing::host_os(),
        arch: pairing::host_arch(),
        role: params.role,
        halves: params.halves,
        client: params.client.to_string(),
        client_version: params.client_version.to_string(),
    };
    let started = match pairing::start(params.server, &request)
        .await
        .context("starting the pairing")?
    {
        StartOutcome::Unavailable => bail!("{}", unavailable_message()),
        StartOutcome::Started(s) => s,
    };
    out.write_all(code_block(&started).as_bytes())
        .and_then(|_| out.flush())
        .context("writing the pairing code")?;

    let approved = match poll_until_decided(params.server, &started, out, wait).await? {
        PairEnd::Approved(a) => a,
        PairEnd::Denied => {
            bail!("the pairing was denied in the browser. Nothing was enrolled.")
        }
        PairEnd::Expired => bail!(
            "the pairing expired before anyone approved it. Nothing was enrolled; run the command again for a new code."
        ),
    };
    out.write_all(approved_block(&approved).as_bytes())
        .and_then(|_| out.flush())
        .context("writing the approval")?;

    match decide(&approved, params.expect_org, console) {
        JoinDecision::Join => Ok(approved),
        refused => bail!("{}", refused.refusal().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::test_http::scripted_server;
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    /// A console that answers what it was told to, and remembers the
    /// questions it was asked.
    struct Scripted {
        answer: Option<bool>,
        asked: Vec<String>,
    }

    impl Scripted {
        fn new(answer: Option<bool>) -> Self {
            Self {
                answer,
                asked: Vec::new(),
            }
        }
    }

    impl Console for Scripted {
        fn available(&mut self) -> bool {
            self.answer.is_some()
        }

        fn confirm(&mut self, question: &str) -> Option<bool> {
            self.asked.push(question.to_string());
            self.answer
        }
    }

    fn approved() -> Approved {
        Approved {
            tokens: vec!["eyJ.TOKEN.sig".into()],
            org_name: "Acme Corp".into(),
            org_slug: Some("acme".into()),
            approver_name: "Dana Smith".into(),
            approver_username: Some("dana".into()),
        }
    }

    fn params<'a>(server: &'a str, expect_org: Option<&'a str>) -> PairParams<'a> {
        PairParams {
            server,
            machine_name: "reception-pc",
            role: PairRole::Daemon,
            halves: 1,
            client: "script-sh",
            client_version: "0.4.182",
            expect_org,
        }
    }

    const STARTED: &str = r#"{"device_code":"dc-SECRET-0123456789","user_code":"BCDF-GHJK","verification_uri":"https://example.invalid/pair","expires_in":900,"interval":5}"#;
    const PENDING: &str = r#"{"status":"authorization_pending"}"#;
    const APPROVED: &str = r#"{"status":"approved","tokens":["eyJ.TOKEN.sig"],"org_name":"Acme Corp","org_slug":"acme","approver_name":"Dana Smith","approver_username":"dana"}"#;

    /// A sleeper that records what it was asked to wait and waits nothing.
    fn recorder() -> (
        Arc<Mutex<Vec<u64>>>,
        impl FnMut(u64) -> std::future::Ready<()>,
    ) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let w = waits.clone();
        (waits, move |secs| {
            w.lock().unwrap().push(secs);
            std::future::ready(())
        })
    }

    // ── the decision ────────────────────────────────────────────────────────

    #[test]
    fn a_yes_joins_and_a_no_declines() {
        let mut yes = Scripted::new(Some(true));
        assert_eq!(decide(&approved(), None, &mut yes), JoinDecision::Join);
        assert_eq!(yes.asked.len(), 1);
        assert_eq!(
            yes.asked[0],
            "Join «Acme Corp» (acme) as approved by «Dana Smith» (@dana)? [y/N]"
        );

        let mut no = Scripted::new(Some(false));
        assert_eq!(
            decide(&approved(), None, &mut no),
            JoinDecision::DeclinedByPerson
        );
        let msg = JoinDecision::DeclinedByPerson.refusal().unwrap();
        assert!(msg.contains("not enrolling"), "{msg}");
        assert!(msg.contains("stays delivered"), "{msg}");
        assert!(msg.contains("10 minutes"), "{msg}");
    }

    /// No terminal and no flag: refused, and the refusal names the flag.
    #[test]
    fn no_console_without_the_flag_refuses_and_names_the_flag() {
        let mut none = Scripted::new(None);
        let d = decide(&approved(), None, &mut none);
        assert_eq!(d, JoinDecision::NoConsole);
        let msg = d.refusal().unwrap();
        assert!(msg.contains("--pair-org"), "{msg}");
        assert!(msg.contains("no terminal"), "{msg}");
        assert!(msg.contains("Nothing was enrolled"), "{msg}");
    }

    /// The flag with a matching slug joins — without a terminal, and on a
    /// terminal without asking: the check was stated in advance. The
    /// person's own value is trimmed; that is all the leeway there is.
    #[test]
    fn a_matching_pair_org_joins_without_asking() {
        let mut none = Scripted::new(None);
        assert_eq!(
            decide(&approved(), Some("acme"), &mut none),
            JoinDecision::Join
        );
        let mut would_say_no = Scripted::new(Some(false));
        assert_eq!(
            decide(&approved(), Some(" acme "), &mut would_say_no),
            JoinDecision::Join,
            "the person's own value, trimmed"
        );
        assert!(
            would_say_no.asked.is_empty(),
            "the console is never consulted when the flag decides"
        );
    }

    /// The server keeps a slug as it was sent, under a case- and
    /// space-sensitive unique index, so every one of these is ANOTHER org
    /// that anybody can create through the API. A comparison that folded
    /// case, trimmed the server's value or normalised Unicode would enroll
    /// the device into it without asking anyone.
    #[test]
    fn a_pair_org_matches_the_server_slug_byte_for_byte_only() {
        for lookalike in [
            "ACME",
            "Acme",
            "acme ",
            " acme",
            "acme\t",
            "\u{430}cme",   // Cyrillic а
            "acme\u{200b}", // a zero-width space
            "acm\u{435}",   // Cyrillic е
        ] {
            let mut a = approved();
            a.org_slug = Some(lookalike.into());
            let mut yes = Scripted::new(Some(true));
            let d = decide(&a, Some("acme"), &mut yes);
            assert_eq!(
                d,
                JoinDecision::OrgMismatch {
                    expected: "acme".into(),
                    got: lookalike.into()
                },
                "{lookalike:?} must not pass for acme"
            );
            assert!(
                yes.asked.is_empty(),
                "the flag decides, the console is not asked"
            );
        }
        // And the other way round: the person's flag in another case does
        // not reach a lowercase slug either.
        let mut yes = Scripted::new(Some(true));
        assert!(matches!(
            decide(&approved(), Some("ACME"), &mut yes),
            JoinDecision::OrgMismatch { .. }
        ));
    }

    /// A mismatched slug refuses even when a terminal would have said yes:
    /// the flag is the guard, not a shortcut past it. The refusal shows the
    /// server's slug escaped, so a lookalike is visible in it.
    #[test]
    fn a_mismatched_pair_org_refuses_even_with_a_willing_console() {
        let mut yes = Scripted::new(Some(true));
        let d = decide(&approved(), Some("other-org"), &mut yes);
        assert_eq!(
            d,
            JoinDecision::OrgMismatch {
                expected: "other-org".into(),
                got: "acme".into()
            }
        );
        assert!(yes.asked.is_empty());
        let msg = d.refusal().unwrap();
        assert!(
            msg.contains("`acme`") && msg.contains("`other-org`"),
            "{msg}"
        );
        assert!(msg.contains("--pair-org"), "{msg}");
        assert!(msg.contains("exactly"), "{msg}");

        let d = JoinDecision::OrgMismatch {
            expected: "acme".into(),
            got: "acme\u{200b}".into(),
        };
        let msg = d.refusal().unwrap();
        assert!(msg.contains("`acme\\u{200b}`"), "{msg}");
    }

    /// An older server sends no slug: the flag has nothing to check against
    /// and refuses rather than trusting a display name.
    #[test]
    fn a_pair_org_against_a_server_without_slugs_refuses() {
        let mut a = approved();
        a.org_slug = None;
        let mut yes = Scripted::new(Some(true));
        let d = decide(&a, Some("acme"), &mut yes);
        assert_eq!(
            d,
            JoinDecision::OrgNotSent {
                expected: "acme".into()
            }
        );
        assert!(d.refusal().unwrap().contains("did not send"));
        // A blank slug matches nothing, and a blank flag nothing either.
        a.org_slug = Some("  ".into());
        assert!(matches!(
            decide(&a, Some("acme"), &mut yes),
            JoinDecision::OrgMismatch { .. }
        ));
        a.org_slug = Some(String::new());
        assert!(matches!(
            decide(&a, Some("  "), &mut yes),
            JoinDecision::OrgMismatch { .. }
        ));
    }

    /// The question and the approval text carry both unique handles, and say
    /// so when a server omits them.
    #[test]
    fn the_texts_show_the_unique_handles_or_say_they_are_missing() {
        let text = approved_block(&approved());
        assert!(
            text.contains("Acme Corp") && text.contains("slug: acme"),
            "{text}"
        );
        assert!(
            text.contains("Dana Smith") && text.contains("username: dana"),
            "{text}"
        );
        assert!(!text.contains("WARNING"), "{text}");

        let mut bare = approved();
        bare.org_slug = None;
        bare.approver_username = None;
        let text = approved_block(&bare);
        assert!(text.contains("not sent by this server"), "{text}");
        assert!(text.contains("WARNING"), "{text}");
        let q = question(&bare);
        assert!(
            q.contains("slug not sent") && q.contains("username not sent"),
            "{q}"
        );
    }

    /// Every server-sent string was typed by somebody. One that carries
    /// terminal controls must not be able to move the cursor, clear a line
    /// or reorder the text the person reads: the controls are shown as
    /// escapes, and nothing else changes.
    #[test]
    fn server_text_reaches_the_terminal_with_its_controls_escaped() {
        let mut evil = approved();
        // Cursor up, clear the line, carriage return, then a forged question.
        evil.org_name =
            "Evil\u{1b}[1A\u{1b}[2K\rJoin «Acme Corp» (acme) as approved by «Dana Smith» (@dana)? [y/N]".into();
        evil.org_slug = Some("evil\u{202e}emca".into()); // a right-to-left override
        evil.approver_name = "Dana\u{7}\u{9b}31m Smith".into(); // a bell, a C1 CSI
        evil.approver_username = Some("dana\u{200b}".into());
        for text in [question(&evil), approved_block(&evil)] {
            for c in text.chars() {
                assert!(
                    c == '\n' || !(c.is_control() || matches!(c, '\u{200b}' | '\u{202e}')),
                    "a raw {c:?} reached the terminal: {text:?}"
                );
            }
            assert!(text.contains("\\u{1b}[1A"), "{text}");
            assert!(text.contains("\\r"), "{text}");
            assert!(text.contains("\\u{202e}"), "{text}");
            assert!(
                text.contains("\\u{7}") && text.contains("\\u{9b}"),
                "{text}"
            );
            assert!(text.contains("\\u{200b}"), "{text}");
        }
        // The legitimate characters a name may hold are shown as they are.
        assert_eq!(
            shown("O'Brien \"Ops\" Müller\\Team"),
            "O'Brien \"Ops\" Müller\\Team"
        );
        assert_eq!(shown("Société Générale"), "Société Générale");
        // The code block goes through the same door.
        let mut s = started();
        s.verification_uri =
            "https://example.invalid/pair\u{1b}]8;;https://evil.invalid\u{7}".into();
        let text = code_block(&s);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
    }

    /// A slug the admin UI could not have made, and a username with spaces
    /// or non-ASCII characters, are flagged on the terminal.
    #[test]
    fn a_handle_a_lookalike_could_hide_behind_is_flagged() {
        for good in ["acme", "beta-labs", "a1", "my-home-lab-2"] {
            assert!(slug_is_ui_shaped(good), "{good}");
        }
        for odd in [
            "ACME",
            "acme ",
            "\u{430}cme",
            "acme-",
            "-acme",
            "ac--me",
            "",
            "a_b",
            "a.b",
        ] {
            assert!(!slug_is_ui_shaped(odd), "{odd:?}");
        }
        assert!(username_is_plain("dana") && username_is_plain("Dana.Smith_2"));
        for odd in ["d\u{430}na", "dana ", "da na", "", "dana\u{200b}"] {
            assert!(!username_is_plain(odd), "{odd:?}");
        }

        let mut a = approved();
        a.org_slug = Some("ACME".into());
        let text = approved_block(&a);
        assert!(
            text.contains("this slug is not one the admin UI makes"),
            "{text}"
        );
        assert!(!text.contains("this username"), "{text}");

        let mut a = approved();
        a.approver_username = Some("d\u{430}na".into());
        let text = approved_block(&a);
        assert!(
            text.contains("this username has spaces or characters outside plain ASCII"),
            "{text}"
        );
        assert!(!text.contains("this slug"), "{text}");
    }

    #[test]
    fn yes_is_y_or_yes_and_nothing_else() {
        for yes in ["y", "Y", "yes", "YES", " yes \n", "y\r\n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "n", "no", "\n", "yes please", "ok", "true", "1"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }

    #[test]
    fn the_client_follows_the_install_method() {
        assert_eq!(client_for(InstallMethod::ScriptSh.wire()), "script-sh");
        assert_eq!(client_for(InstallMethod::ScriptPs1.wire()), "script-ps1");
        assert_eq!(client_for(" script-sh "), "script-sh");
        assert_eq!(client_for(InstallMethod::Manual.wire()), "roomlerd");
        assert_eq!(client_for(InstallMethod::Wizard.wire()), "roomlerd");
        assert_eq!(client_for("container"), "roomlerd");
        assert_eq!(client_for(""), "roomlerd");
        assert_eq!(client_for("something-new"), "roomlerd");
    }

    // ── the loop ────────────────────────────────────────────────────────────

    fn started() -> Started {
        Started {
            device_code: pairing::DeviceCode::new("dc-SECRET-0123456789"),
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://example.invalid/pair".into(),
            expires_in: 900,
            interval: 5,
        }
    }

    /// The back-off: `slow_down` takes the server's interval, a 429 adds the
    /// step or takes `Retry-After` when longer, and the interval never
    /// shrinks.
    #[tokio::test]
    async fn the_loop_backs_off_and_ends_on_the_approval() {
        let (base, seen) = scripted_server(vec![
            ("200 OK", PENDING),
            ("200 OK", r#"{"status":"slow_down","interval":10}"#),
            ("429 Too Many Requests\r\nRetry-After: 12", ""),
            ("200 OK", r#"{"status":"needs_a_second_factor"}"#),
            ("200 OK", APPROVED),
        ])
        .await;
        let (waits, wait) = recorder();
        let mut out: Vec<u8> = Vec::new();
        let end = poll_until_decided(&base, &started(), &mut out, wait)
            .await
            .unwrap();
        assert!(matches!(end, PairEnd::Approved(a) if a.org_slug.as_deref() == Some("acme")));
        assert_eq!(*waits.lock().unwrap(), vec![5, 5, 10, 15, 15]);
        assert_eq!(seen.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn a_denial_and_an_expiry_end_the_loop() {
        let (base, _) = scripted_server(vec![("200 OK", r#"{"status":"access_denied"}"#)]).await;
        let mut out: Vec<u8> = Vec::new();
        let (_, wait) = recorder();
        assert!(matches!(
            poll_until_decided(&base, &started(), &mut out, wait)
                .await
                .unwrap(),
            PairEnd::Denied
        ));
        let (base, _) = scripted_server(vec![("200 OK", r#"{"status":"expired_token"}"#)]).await;
        let (_, wait) = recorder();
        assert!(matches!(
            poll_until_decided(&base, &started(), &mut out, wait)
                .await
                .unwrap(),
            PairEnd::Expired
        ));
    }

    /// The local clock ends the loop too, without a poll, for a server that
    /// stopped answering.
    #[tokio::test]
    async fn a_passed_deadline_expires_without_polling() {
        let (base, seen) = scripted_server(vec![("200 OK", PENDING)]).await;
        let mut out: Vec<u8> = Vec::new();
        let (waits, wait) = recorder();
        let end = poll_until(&base, &started(), &mut out, wait, Instant::now())
            .await
            .unwrap();
        assert!(matches!(end, PairEnd::Expired));
        assert!(waits.lock().unwrap().is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    /// RFC 8628 §3.5: a failed poll lengthens the interval (doubling, up to
    /// the cap, never shrinking) and the loop goes on — a pod roll or a VPN
    /// reconnect in the middle of a pairing costs a late notice, not the
    /// pairing. Each failure is said on the terminal, with no secret in it.
    #[tokio::test]
    async fn poll_errors_back_off_and_the_loop_goes_on_to_the_approval() {
        let mut script = vec![("500 Internal Server Error", ""); 7];
        script.push(("200 OK", APPROVED));
        let (base, seen) = scripted_server(script).await;
        let mut out: Vec<u8> = Vec::new();
        let (waits, wait) = recorder();
        let end = poll_until_decided(&base, &started(), &mut out, wait)
            .await
            .unwrap();
        assert!(matches!(end, PairEnd::Approved(_)));
        assert_eq!(seen.lock().unwrap().len(), 8);
        assert_eq!(*waits.lock().unwrap(), vec![5, 10, 20, 30, 30, 30, 30, 30]);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("trying again").count(), 7, "{text}");
        assert!(!text.contains("SECRET"), "{text}");

        assert_eq!(interval_after_poll_error(5), 10);
        assert_eq!(interval_after_poll_error(20), 30);
        assert_eq!(interval_after_poll_error(30), 30);
        assert_eq!(
            interval_after_poll_error(45),
            45,
            "a slow_down's longer interval stays"
        );
        assert_eq!(interval_after_poll_error(u64::MAX), u64::MAX);
    }

    /// A run of failures that lasts to the end of the pairing's clock ends
    /// the flow with that failure — not with "nobody approved it", which
    /// would send the person to look for an approval that may have happened.
    #[tokio::test]
    async fn failures_until_the_deadline_end_with_the_failure() {
        let (base, seen) = scripted_server(vec![("503 Service Unavailable", "")]).await;
        let mut out: Vec<u8> = Vec::new();
        // A few real milliseconds a wait, so the clock does run out.
        let wait = |_| tokio::time::sleep(Duration::from_millis(20));
        let deadline = Instant::now() + Duration::from_millis(300);
        let e = poll_until(&base, &started(), &mut out, wait, deadline)
            .await
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("ran out while polling it was failing"), "{e}");
        assert!(e.contains("503"), "{e}");
        assert!(!seen.lock().unwrap().is_empty());
    }

    // ── the whole flow ──────────────────────────────────────────────────────

    /// A log sink for the secret scan: every line the flow logs lands here.
    #[derive(Clone, Default)]
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
        type Writer = LogSink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// AC3 and the token: the whole flow — start, a failed poll, a pending
    /// one, the approval, a yes — against a loopback server, with every line
    /// it prints and every line it logs scanned. The user code and the page
    /// are shown; the device code and the token appear nowhere.
    #[tokio::test]
    async fn the_flow_prints_the_code_and_never_a_secret() {
        // A second live dispatcher, held for the whole test. With exactly
        // one registered, tracing-core rebuilds a NEW callsite's interest
        // from the registering thread's own default (`Rebuilder::JustOne`):
        // when another test thread — no subscriber of its own — reaches
        // `pairing::start` first, `never` is cached for that callsite and the
        // subscriber below misses the event, so the scan would pass on a log
        // that never saw the flow. With two, every registered one is asked.
        // The positive controls at the end keep that loud.
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let sink = LogSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(sink.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let (base, seen) = scripted_server(vec![
            ("200 OK", STARTED),
            ("502 Bad Gateway", ""),
            ("200 OK", PENDING),
            ("200 OK", APPROVED),
        ])
        .await;
        let mut out: Vec<u8> = Vec::new();
        let mut console = Scripted::new(Some(true));
        let (_, wait) = recorder();
        let approved = pair_from_terminal_with(params(&base, None), &mut out, &mut console, wait)
            .await
            .unwrap();
        assert_eq!(approved.tokens, vec!["eyJ.TOKEN.sig".to_string()]);

        // The start body names the terminal client, and the polls carry the
        // device code and nothing else.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        let start: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(start["client"], "script-sh");
        assert_eq!(start["role"], "daemon");
        assert_eq!(start["halves"], 1);
        assert_eq!(start["machine_name"], "reception-pc");
        for poll in &seen[1..] {
            let body: serde_json::Value = serde_json::from_str(&poll.body).unwrap();
            assert_eq!(
                body,
                serde_json::json!({ "device_code": "dc-SECRET-0123456789" })
            );
        }

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("BCDF-GHJK"), "{text}");
        assert!(text.contains("https://example.invalid/pair"), "{text}");
        assert!(
            text.contains("trying again"),
            "the failed poll is said: {text}"
        );
        assert!(
            text.contains("Acme Corp") && text.contains("slug: acme"),
            "{text}"
        );
        assert!(
            text.contains("Dana Smith") && text.contains("username: dana"),
            "{text}"
        );
        assert!(
            !text.contains("SECRET"),
            "the device code was printed: {text}"
        );
        assert!(!text.contains("TOKEN"), "the token was printed: {text}");
        assert_eq!(console.asked.len(), 1);
        assert!(!console.asked[0].contains("SECRET") && !console.asked[0].contains("TOKEN"));

        let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("device pairing started") && log.contains("poll failed"),
            "the flow did log: {log}"
        );
        assert!(
            !log.contains("SECRET"),
            "the device code reached the log: {log}"
        );
        assert!(!log.contains("TOKEN"), "the token reached the log: {log}");
    }

    /// An older server (404 on start): an error that names `--token`, no
    /// poll, nothing printed as a code.
    #[tokio::test]
    async fn an_older_server_is_an_error_that_names_the_token_flag() {
        let (base, seen) = scripted_server(vec![("404 Not Found", "")]).await;
        let mut out: Vec<u8> = Vec::new();
        let mut console = Scripted::new(Some(true));
        let (_, wait) = recorder();
        let e = pair_from_terminal_with(params(&base, None), &mut out, &mut console, wait)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not support pairing"), "{e}");
        assert!(e.contains("--token"), "{e}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(out.is_empty());
        assert!(console.asked.is_empty());
    }

    /// A no, a denial, a no-console run and a lookalike org each end
    /// without a token and say what they leave behind.
    #[tokio::test]
    async fn refusals_end_the_flow_without_a_token() {
        let (base, _) = scripted_server(vec![("200 OK", STARTED), ("200 OK", APPROVED)]).await;
        let mut out: Vec<u8> = Vec::new();
        let mut no = Scripted::new(Some(false));
        let (_, wait) = recorder();
        let e = pair_from_terminal_with(params(&base, None), &mut out, &mut no, wait)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("you answered no") && e.contains("stays delivered"),
            "{e}"
        );

        // No terminal and no flag: refused BEFORE the server hears anything,
        // so nobody approves a pairing that could never be confirmed.
        let (base, seen) = scripted_server(vec![("200 OK", STARTED), ("200 OK", APPROVED)]).await;
        let mut none = Scripted::new(None);
        let (_, wait) = recorder();
        let e = pair_from_terminal_with(params(&base, None), &mut out, &mut none, wait)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not starting a pairing"), "{e}");
        assert!(e.contains("no terminal") && e.contains("--pair-org"), "{e}");
        assert!(seen.lock().unwrap().is_empty(), "nothing was sent");

        let (base, _) = scripted_server(vec![("200 OK", STARTED), ("200 OK", APPROVED)]).await;
        let mut none = Scripted::new(None);
        let (_, wait) = recorder();
        let approved =
            pair_from_terminal_with(params(&base, Some("acme")), &mut out, &mut none, wait)
                .await
                .unwrap();
        assert_eq!(approved.org_slug.as_deref(), Some("acme"));

        // The same approval into a lookalike org: refused with no terminal,
        // and no terminal is asked.
        let (base, _) = scripted_server(vec![
            ("200 OK", STARTED),
            (
                "200 OK",
                r#"{"status":"approved","tokens":["eyJ.TOKEN.sig"],"org_name":"Acme Corp","org_slug":"ACME","approver_name":"Dana Smith","approver_username":"dana"}"#,
            ),
        ])
        .await;
        let mut none = Scripted::new(None);
        let (_, wait) = recorder();
        let e = pair_from_terminal_with(params(&base, Some("acme")), &mut out, &mut none, wait)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("`ACME`, not `acme`"), "{e}");
        assert!(e.contains("Nothing was enrolled"), "{e}");
        assert!(none.asked.is_empty());

        let (base, _) = scripted_server(vec![
            ("200 OK", STARTED),
            ("200 OK", r#"{"status":"access_denied"}"#),
        ])
        .await;
        let mut yes = Scripted::new(Some(true));
        let (_, wait) = recorder();
        let e = pair_from_terminal_with(params(&base, None), &mut out, &mut yes, wait)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("denied"), "{e}");
        assert!(yes.asked.is_empty());
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("SECRET") && !text.contains("TOKEN"),
            "{text}"
        );
    }
}
