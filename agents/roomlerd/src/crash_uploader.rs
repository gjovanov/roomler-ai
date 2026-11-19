// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Drain pending crash sidecars to the roomler.ai ingest endpoint.
//!
//! Phase 1C of the agent-crash-upload feature (Task 9). The recorder
//! (Phase 1A) wrote `*.json` sidecars to the Worker + Supervisor
//! crashes dirs at crash time; on next agent startup [`drain_and_
//! upload`] scans both dirs, POSTs each payload to roomler.ai with
//! Bearer auth, and deletes successfully-uploaded sidecars.
//!
//! ## Delete policy
//!
//! - **2xx** — server accepted, delete the sidecar.
//! - **4xx** — server rejected the payload shape (validation error);
//!   re-uploading the same bytes will loop forever. Delete + log.
//! - **5xx / network error** — transient. Keep the sidecar for the
//!   next agent startup retry.
//! - **A final verdict (2xx or 4xx) this process may not delete** — typically a
//!   sidecar another account wrote — is recorded in `<log dir>/crash-verdicts.txt`
//!   and never sent again (#1748). Before, it went out again on every drain:
//!   a 4xx forever, or a duplicate crash. On Windows the service removes one in
//!   the machine's `crashes` once that ledger names it (#3799), so that one is
//!   logged at INFO; one kept anywhere else stays, with a WARN.
//!
//! #4022 — each answered sidecar gets two lines, in this order. The drain's
//! says what the server did: `uploaded`, or `server rejected payload` with the
//! status and the body. Then `settle`'s, after it tried the delete, says what
//! became of the file: `sidecar deleted`, or one of the lines above for a
//! sidecar this process may not delete. Until #4022 the drain said `uploaded +
//! deleted` (and `deleting` for a 4xx) before the delete was tried.
//!
//! ## Concurrency
//!
//! Single sequential POST per sidecar. A fleet of 100 hosts rebooting
//! simultaneously would burst the ingest endpoint with N parallel
//! posts per host otherwise; sequential keeps the host's outbound
//! HTTP behaviour predictable + maps onto the existing `reqwest::
//! Client::new()` (no connection pooling needed).
//!
//! ## Where this runs
//!
//! Spawned as a `tokio::task::spawn` from `app.rs` AFTER the first
//! successful WS connect — proves the agent JWT is valid + the
//! network can reach roomler.ai, so we're not hammering an offline
//! host with retry storms. Gated through `tokio::sync::Notify` from
//! the signaling loop's first `Ok(())` return.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use reqwest::StatusCode;

use crate::config::AgentConfig;
use crate::crash_recorder;

/// HTTP timeout for each individual crash-report POST. Sized
/// generously because the ingest endpoint may go through Cloudflare
/// + the K8s ingress before reaching the API pod.
const POST_TIMEOUT: Duration = Duration::from_secs(30);

/// rc.58: cadence for the periodic mid-session drain in `app.rs`.
/// Before rc.58 the drain ran ONLY at startup; long-running agents
/// that crashed once early (during, say, a flaky-network cold start
/// like the field-test host 2026-05-24 watchdog-loop) never got their
/// sidecars off disk because the next process restart was hours or
/// days away. 5 min keeps the loop cheap (one HTTP HEAD-equivalent
/// per drain when no sidecars are pending) while ensuring the admin
/// UI sees evidence within a single coffee break of the network
/// coming back. Pub so `app.rs` can import without re-defining.
pub const CRASH_DRAIN_INTERVAL_SECS: u64 = 5 * 60;

/// Minimum delay between consecutive sidecar POSTs. The backend's
/// `tower_governor` rate-limit is 60 req/min per IP = 1 req/sec
/// steady-state with a 60-burst budget. Field repro 2026-05-17
/// a third field-test host: agent drained 1317 sidecars at ~50/sec, exhausted the
/// burst budget in ~1.2 sec, then ~1267 of them got 429s. With a
/// 1.1-sec delay we stay safely under the steady-state limit; the
/// drain takes longer (1317 × 1.1s = ~24 min worst case) but every
/// sidecar lands. Backlogs that big are pathological anyway — the
/// recorder's 30-sec rate-limit prevents accumulation under normal
/// operation.
const INTER_REQUEST_DELAY: Duration = Duration::from_millis(1100);

/// Drain every pending crash sidecar to roomler.ai. Best-effort:
/// any IO / network / parse failure logs `warn!` and continues with
/// the next sidecar so a single poisoned file doesn't block the
/// fleet. Returns when the queue is empty.
///
/// FR-103 — `token` is the primary enrollment's LIVE slot, read once per
/// drain: this task runs for the life of the process, and the token it
/// started with may since have been renewed.
pub async fn drain_and_upload(cfg: &AgentConfig, token: &crate::token_slot::TokenSlot) {
    let ledger = verdicts_dir();
    let swept = swept_dir();
    let settled = ledger.as_deref().map(load_settled).unwrap_or_default();
    let pending = without_settled(crash_recorder::pending_all(), &settled);
    if pending.is_empty() {
        tracing::debug!("crash_uploader: no pending sidecars");
        return;
    }
    tracing::info!(
        count = pending.len(),
        "crash_uploader: draining pending crash sidecars"
    );

    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "crash_uploader: client build failed; deferring uploads");
            return;
        }
    };
    let url = format!("{}/api/agent/crash", cfg.server_url.trim_end_matches('/'));
    let agent_token = token.get();

    let mut tally = Tally::default();
    let mut first = true;
    for (sidecar, payload) in pending {
        // Inter-request delay to stay under the backend's 60-req/min
        // rate-limit (tower_governor). Skipped for the first upload
        // so a single-sidecar drain is still instant.
        if !first {
            tokio::time::sleep(INTER_REQUEST_DELAY).await;
        }
        first = false;
        let outcome = upload_one(&client, &url, &agent_token, &payload).await;
        answered(
            &sidecar,
            outcome,
            ledger.as_deref(),
            swept.as_deref(),
            crash_recorder::Sidecar::remove,
            &mut tally,
        );
    }

    tracing::info!(
        uploaded = tally.uploaded,
        kept = tally.kept,
        dropped = tally.dropped,
        "crash_uploader: drain complete"
    );
}

/// What one drain did with the sidecars it sent, for its last line.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    uploaded: u32,
    kept: u32,
    dropped: u32,
}

/// #4022 — one sidecar the server answered. The drain's own line says what the
/// server did. A final verdict (2xx or 4xx) then settles the file ([`settle`]),
/// which says what became of it, once, after it tried: where this process may
/// not delete it (a person's drain in the machine's `crashes`, #3799), nothing
/// says it was deleted. `remove` is [`crash_recorder::Sidecar::remove`]
/// outside the tests.
fn answered(
    sidecar: &crash_recorder::Sidecar,
    outcome: UploadOutcome,
    ledger: Option<&Path>,
    swept: Option<&Path>,
    remove: impl FnOnce(&crash_recorder::Sidecar) -> std::io::Result<()>,
    tally: &mut Tally,
) {
    let file = sidecar.path().display();
    match outcome {
        UploadOutcome::Accepted => {
            tracing::info!(file = %file, "crash_uploader: uploaded");
            settle(sidecar, ledger, swept, "uploaded", remove);
            tally.uploaded += 1;
        }
        UploadOutcome::Rejected { status, body } => {
            tracing::warn!(
                file = %file,
                status = %status,
                body = %body,
                "crash_uploader: server rejected payload (4xx is permanent)"
            );
            settle(sidecar, ledger, swept, "rejected (4xx)", remove);
            tally.dropped += 1;
        }
        UploadOutcome::Transient { reason } => {
            tracing::warn!(
                file = %file,
                reason = %reason,
                "crash_uploader: transient failure; keeping sidecar for next startup"
            );
            tally.kept += 1;
        }
    }
}

/// Build the reqwest client with the same posture as
/// `enrollment.rs::enroll`'s plain `reqwest::Client::new()` —
/// system TLS roots, default timeouts, plus an explicit per-request
/// timeout via [`POST_TIMEOUT`].
fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!(
            "roomlerd-crash-uploader/",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(POST_TIMEOUT)
        .build()?)
}

/// Categorisation of an upload attempt's outcome. Pure data — the
/// caller deletes or retains the sidecar based on the variant.
#[derive(Debug)]
enum UploadOutcome {
    /// 2xx — server accepted. Delete sidecar.
    Accepted,
    /// 4xx — server rejected. Delete sidecar (re-upload is useless).
    Rejected { status: StatusCode, body: String },
    /// 5xx, timeout, network error, etc. Keep sidecar for retry.
    Transient { reason: String },
}

async fn upload_one(
    client: &reqwest::Client,
    url: &str,
    agent_token: &str,
    payload: &roomler_ai_remote_control::models::AgentCrashPayload,
) -> UploadOutcome {
    let req = client
        .post(url)
        .bearer_auth(agent_token)
        .json(payload)
        .send()
        .await;

    let resp = match req {
        Ok(r) => r,
        Err(e) => {
            return UploadOutcome::Transient {
                reason: format!("{e}"),
            };
        }
    };
    let status = resp.status();
    classify_status(status, || async { resp.text().await.unwrap_or_default() }).await
}

/// Categorise an HTTP response status into [`UploadOutcome`]. Pure
/// over `status` + body-resolver; tests use a stub body-resolver to
/// drive every branch without a real HTTP server.
async fn classify_status<F, Fut>(status: StatusCode, body_resolver: F) -> UploadOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = String>,
{
    if status.is_success() {
        UploadOutcome::Accepted
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        // 429 is the BACKEND saying "slow down" — NOT permanent. Field
        // repro 2026-05-17 a third field-test host: storm of 1317 sidecars hit the
        // tower_governor 60-req/min IP limit; treating it as 4xx-
        // Rejected (the previous behaviour) deleted ~1267 of them
        // permanently. Now classified as Transient so the next agent
        // startup retries. The inter-request delay (see
        // INTER_REQUEST_DELAY) in `drain_and_upload` keeps the steady
        // state under the limit anyway; this is the safety net for
        // accumulated backlog.
        UploadOutcome::Transient {
            reason: format!("HTTP {status} — rate-limited"),
        }
    } else if status.is_client_error() {
        let body = body_resolver().await;
        UploadOutcome::Rejected { status, body }
    } else {
        // 5xx, 3xx unfollowed redirects, weird statuses → transient.
        UploadOutcome::Transient {
            reason: format!("HTTP {status}"),
        }
    }
}

/// Public helper for tests that want to drive the delete-vs-keep
/// decision without a network. Resolves the body lazily so 2xx
/// branches don't pay for an unused read.
#[cfg(test)]
async fn classify_status_for_test(status: StatusCode, body: &str) -> UploadOutcome {
    let owned = body.to_string();
    classify_status(status, || async move { owned }).await
}

/// The folder that keeps the verdicts on undeletable sidecars
/// ([`crash_recorder::VERDICTS_FILE`]): this process's own log folder, which
/// it can always write. `None` without file logging.
///
/// A sidecar gets a FINAL verdict when it is uploaded, or rejected as
/// permanent. Normally it is then deleted. One this process may not delete is
/// one another account owns — measured: a report a root process wrote into
/// this user's log folder (`sudo` keeps `HOME` on macOS).
/// Without a record, every drain (at start AND every [`CRASH_DRAIN_INTERVAL_SECS`])
/// sent it again: a rejected report as another 4xx forever (field: the
/// supervised MacBook, one 2026-09-08 report re-sent every five minutes), and
/// an accepted one as a duplicate crash.
fn verdicts_dir() -> Option<std::path::PathBuf> {
    crate::logging::log_dir()
}

/// #3799 — the folder whose records the service removes once a drainer's
/// ledger names them: the machine's `crashes`, on Windows
/// (`crash_recorder::sweep_settled`). A worker that runs as the signed-in
/// person reads the supervisor's records there but may not delete them
/// (`Seal::Readable`). A per-user install runs no service: a record an
/// earlier per-Machine install left there stays until a service sweeps again.
/// `None` off Windows, where nothing removes a record this process may not.
fn swept_dir() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        crash_recorder::crashes_dir_for(crash_recorder::WriterContext::Supervisor).ok()
    } else {
        None
    }
}

/// Whose sidecar this process may not delete, outside [`swept_dir`]: measured
/// on macOS, where `sudo` keeps `HOME` ([`verdicts_dir`]). On Windows a
/// drainer reads another account's records only in [`swept_dir`], SYSTEM's;
/// its own folder holds what its own account's processes wrote.
const OWNER: &str = if cfg!(windows) {
    ""
} else {
    " (typically one another account wrote, e.g. a root process run with this user's HOME)"
};

/// Every signature recorded in the ledger in `dir`. A missing or unreadable
/// ledger is an empty one: the cost is one more send, never a lost crash.
fn load_settled(dir: &Path) -> std::collections::HashSet<String> {
    crash_recorder::read_verdicts(dir)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn record_settled(dir: &Path, sig: &str) -> std::io::Result<()> {
    crash_recorder::append_verdict(dir, sig)
}

/// The queue less every sidecar that already has a final verdict. A
/// sidecar's identity for the ledger is `<len>:<mtime> <file name>`
/// ([`crash_recorder::Sidecar::signature`]): a file nobody can delete does
/// not change, and a new crash never reuses a name (names carry the crash time
/// and pid).
fn without_settled<T>(
    pending: Vec<(crash_recorder::Sidecar, T)>,
    settled: &std::collections::HashSet<String>,
) -> Vec<(crash_recorder::Sidecar, T)> {
    if settled.is_empty() {
        return pending;
    }
    pending
        .into_iter()
        .filter(|(sidecar, _)| !settled.contains(&sidecar.signature()))
        .collect()
}

/// A sidecar has its final verdict: delete it, or, when this process may not,
/// record the verdict so the sidecar is never sent again. `remove` is
/// [`crash_recorder::Sidecar::remove`] outside the tests: below the machine's
/// folder, by handle through the folder it was read from (FR-145). `swept` is
/// [`swept_dir`]: a recorded sidecar there is the service's to remove, so
/// nothing is wrong and the line is INFO; one anywhere else stays, a WARN.
/// #4022 — the one line that says what became of the file, said after the
/// delete was tried: deleted, gone already, or kept with its verdict recorded.
fn settle(
    sidecar: &crash_recorder::Sidecar,
    ledger: Option<&Path>,
    swept: Option<&Path>,
    verdict: &str,
    remove: impl FnOnce(&crash_recorder::Sidecar) -> std::io::Result<()>,
) {
    let path = sidecar.path();
    let err = match remove(sidecar) {
        Ok(()) => {
            tracing::info!(file = %path.display(), "crash_uploader: {verdict}; sidecar deleted");
            return;
        }
        // Another drainer, or the service's sweep, took it since the scan.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                file = %path.display(),
                "crash_uploader: {verdict}; the sidecar was gone already"
            );
            return;
        }
        Err(e) => e,
    };
    match ledger {
        Some(l) if record_settled(l, &sidecar.signature()).is_ok() => {
            let ledger = l.join(crash_recorder::VERDICTS_FILE);
            if swept.is_some_and(|dir| path.parent() == Some(dir)) {
                tracing::info!(
                    file = %path.display(),
                    error = %err,
                    ledger = %ledger.display(),
                    "crash_uploader: {verdict}; this process may not delete the sidecar in the \
                     machine's crashes folder, so its verdict is recorded and it is not sent \
                     again. The service removes it once this ledger names it"
                );
            } else {
                tracing::warn!(
                    file = %path.display(),
                    error = %err,
                    ledger = %ledger.display(),
                    "crash_uploader: {verdict}, but this process may not delete the sidecar\
                     {OWNER}; its verdict is recorded so it is not sent again. Remove it as \
                     the account that owns it"
                );
            }
        }
        _ => tracing::warn!(
            file = %path.display(),
            error = %err,
            "crash_uploader: {verdict}, but the sidecar could not be deleted and the \
             verdict could not be recorded; it will be sent again next drain"
        ),
    }
}

/// Delete a sidecar if it is still there. Idempotent.
#[allow(dead_code)] // used by future Phase 2 + manual smoke tooling
pub fn delete_sidecar(sidecar: &crash_recorder::Sidecar) -> std::io::Result<()> {
    match sidecar.remove() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(_: &crash_recorder::Sidecar) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }

    /// The sidecar at `path`, as the drain reads one outside the machine's
    /// folder.
    fn plain(path: &Path) -> crash_recorder::Sidecar {
        crash_recorder::Sidecar::plain(path).expect("a sidecar the test wrote")
    }

    /// The field bug: a rejected sidecar this process may not delete was sent
    /// again on every drain. Its verdict must be recorded, and the next drain
    /// must skip it.
    #[test]
    fn an_undeletable_verdict_is_recorded_and_never_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1788894634-71069.json");
        std::fs::write(&path, b"{}").unwrap();
        let sidecar = plain(&path);
        let ledger = dir.path();

        settle(&sidecar, Some(ledger), None, "rejected (4xx)", denied);

        assert!(path.exists(), "the test's remove refused, so it stays");
        let left = without_settled(vec![(plain(&path), ())], &load_settled(ledger));
        assert!(
            left.is_empty(),
            "a settled sidecar must not be queued again: {left:?}"
        );
    }

    /// The control: a sidecar that WAS deleted leaves no record at all, so
    /// the ledger only ever holds what could not be cleaned up.
    #[test]
    fn a_deletable_sidecar_leaves_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.json");
        std::fs::write(&path, b"{}").unwrap();
        let ledger = dir.path();

        settle(
            &plain(&path),
            Some(ledger),
            None,
            "uploaded",
            crash_recorder::Sidecar::remove,
        );

        assert!(!path.exists());
        assert!(
            !ledger.join(crash_recorder::VERDICTS_FILE).exists(),
            "nothing to record for a deleted sidecar"
        );
    }

    /// A new crash is never mistaken for a settled one: only the recorded
    /// sidecar is skipped, and a missing ledger skips nothing.
    #[test]
    fn only_the_settled_sidecar_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("100-1.json");
        let new = dir.path().join("200-2.json");
        std::fs::write(&old, b"{}").unwrap();
        std::fs::write(&new, b"{}").unwrap();
        let ledger = dir.path();

        let none = without_settled(
            vec![(plain(&old), ()), (plain(&new), ())],
            &load_settled(ledger),
        );
        assert_eq!(none.len(), 2, "no ledger yet: nothing is skipped");

        settle(&plain(&old), Some(ledger), None, "rejected (4xx)", denied);
        let left = without_settled(
            vec![(plain(&old), ()), (plain(&new), ())],
            &load_settled(ledger),
        );
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0.path(), new);
    }

    /// What `f` logs, as the daemon's log file writes it.
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        // Two dispatchers, never one: with one, a callsite's interest is
        // taken once for every thread.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    /// #3799 — a recorded sidecar in the folder the service sweeps is the
    /// service's to remove: nothing is wrong, so the line is INFO and says the
    /// service removes it. One anywhere else stays: a WARN that says to remove
    /// it. Both are recorded, and the next drain skips both.
    #[test]
    fn a_record_the_service_removes_is_info_and_any_other_a_warn() {
        let dir = tempfile::tempdir().unwrap();
        let swept = dir.path().join("crashes");
        std::fs::create_dir(&swept).unwrap();
        let theirs = swept.join("100-1.json");
        let elsewhere = dir.path().join("200-2.json");
        std::fs::write(&theirs, b"{}").unwrap();
        std::fs::write(&elsewhere, b"{}").unwrap();
        let ledger = dir.path();

        let line = logged(|| {
            settle(
                &plain(&theirs),
                Some(ledger),
                Some(swept.as_path()),
                "uploaded",
                denied,
            )
        });
        assert!(line.trim_start().starts_with("INFO "), "{line}");
        assert!(
            line.contains("The service removes it once this ledger names it"),
            "{line}"
        );
        assert!(!line.contains("Remove it as the account"), "{line}");

        let line = logged(|| {
            settle(
                &plain(&elsewhere),
                Some(ledger),
                Some(swept.as_path()),
                "uploaded",
                denied,
            )
        });
        assert!(line.trim_start().starts_with("WARN "), "{line}");
        assert!(
            line.contains("Remove it as the account that owns it"),
            "{line}"
        );
        assert!(!line.contains("The service removes it"), "{line}");

        let left = without_settled(
            vec![(plain(&theirs), ()), (plain(&elsewhere), ())],
            &load_settled(ledger),
        );
        assert!(left.is_empty(), "both are recorded: {left:?}");
    }

    /// The two answers that settle a sidecar: accepted, and rejected for good.
    fn final_answers() -> [(UploadOutcome, &'static str); 2] {
        [
            (UploadOutcome::Accepted, "crash_uploader: uploaded "),
            (
                UploadOutcome::Rejected {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    body: "log_tail too big".into(),
                },
                "crash_uploader: server rejected payload (4xx is permanent) ",
            ),
        ]
    }

    /// #4022 — the field bug: a person's drain logged `uploaded + deleted` for
    /// a record in the machine's `crashes`, then, 0.6 ms later, that it may not
    /// delete it. An accepted or rejected sidecar this process may not delete
    /// is never said deleted: the server's line comes first, then `settle`'s
    /// words for a sidecar it may not delete, in the service's folder or
    /// anywhere else, and nothing more.
    #[test]
    fn a_sidecar_this_process_may_not_delete_is_never_said_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let swept = dir.path().join("crashes");
        std::fs::create_dir(&swept).unwrap();
        let ledger = dir.path();
        let folders = [
            (
                swept.clone(),
                "The service removes it once this ledger names it",
            ),
            (
                dir.path().to_path_buf(),
                "Remove it as the account that owns it",
            ),
        ];
        let mut tally = Tally::default();
        for (f, (folder, settled)) in folders.into_iter().enumerate() {
            for (o, (outcome, server)) in final_answers().into_iter().enumerate() {
                let path = folder.join(format!("{f}{o}-1.json"));
                std::fs::write(&path, b"{}").unwrap();
                let log = logged(|| {
                    answered(
                        &plain(&path),
                        outcome,
                        Some(ledger),
                        Some(swept.as_path()),
                        denied,
                        &mut tally,
                    )
                });
                assert!(
                    !log.contains("deleted") && !log.contains("deleting"),
                    "{log}"
                );
                let lines: Vec<&str> = log.lines().collect();
                assert_eq!(lines.len(), 2, "the server's line, then settle's: {log}");
                assert!(lines[0].contains(server), "{log}");
                assert!(lines[1].contains(settled), "{log}");
                assert!(path.exists(), "the test's remove refused, so it stays");
            }
        }
        let expected = Tally {
            uploaded: 2,
            kept: 0,
            dropped: 2,
        };
        assert_eq!(tally, expected);
    }

    /// #4022 — the control: a sidecar this process deletes is said deleted
    /// once, by `settle`, after the server's line, and the file is gone. A
    /// transient answer deletes nothing and keeps the sidecar for the next
    /// drain.
    #[test]
    fn a_sidecar_is_said_deleted_once_after_the_servers_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut tally = Tally::default();
        for (n, (outcome, server)) in final_answers().into_iter().enumerate() {
            let path = dir.path().join(format!("{n}-1.json"));
            std::fs::write(&path, b"{}").unwrap();
            let log = logged(|| {
                answered(
                    &plain(&path),
                    outcome,
                    Some(dir.path()),
                    None,
                    crash_recorder::Sidecar::remove,
                    &mut tally,
                )
            });
            let lines: Vec<&str> = log.lines().collect();
            assert_eq!(lines.len(), 2, "{log}");
            assert!(lines[0].contains(server), "{log}");
            assert!(lines[1].contains("; sidecar deleted"), "{log}");
            assert_eq!(log.matches("deleted").count(), 1, "said once: {log}");
            assert!(!path.exists());
        }

        let path = dir.path().join("9-1.json");
        std::fs::write(&path, b"{}").unwrap();
        let log = logged(|| {
            answered(
                &plain(&path),
                UploadOutcome::Transient {
                    reason: "HTTP 503".into(),
                },
                Some(dir.path()),
                None,
                |_| panic!("a transient answer deletes nothing"),
                &mut tally,
            )
        });
        assert!(log.contains("keeping sidecar"), "{log}");
        assert!(path.exists());
        let expected = Tally {
            uploaded: 1,
            kept: 1,
            dropped: 1,
        };
        assert_eq!(tally, expected);
        assert!(
            !dir.path().join(crash_recorder::VERDICTS_FILE).exists(),
            "nothing to record for a deleted sidecar"
        );
    }

    /// #3799 — the folder the uploader calls the service's is the one the
    /// service sweeps, the machine's `crashes` (`sweep_settled`), and only on
    /// Windows: nothing removes a record anywhere else.
    #[test]
    fn only_windows_has_a_folder_the_service_sweeps() {
        #[cfg(windows)]
        {
            assert_eq!(
                swept_dir(),
                Some(crate::appdirs::machine_global_dir().join("crashes"))
            );
            assert_eq!(OWNER, "", "no root running with this user's HOME there");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(swept_dir(), None);
            assert!(OWNER.contains("this user's HOME"), "{OWNER}");
        }
    }

    #[tokio::test]
    async fn classify_2xx_returns_accepted_delete() {
        let out = classify_status_for_test(StatusCode::CREATED, "ignored").await;
        assert!(matches!(out, UploadOutcome::Accepted));
        let out = classify_status_for_test(StatusCode::OK, "").await;
        assert!(matches!(out, UploadOutcome::Accepted));
    }

    #[tokio::test]
    async fn classify_4xx_returns_rejected_delete_with_body() {
        let out =
            classify_status_for_test(StatusCode::UNPROCESSABLE_ENTITY, "log_tail too big").await;
        match out {
            UploadOutcome::Rejected { status, body } => {
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(body, "log_tail too big");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_401_returns_rejected_delete() {
        // 401 is a 4xx → delete. A persistently-invalid agent token
        // means re-uploading won't help; the sentinel system will
        // eventually surface the re-enrollment requirement.
        let out = classify_status_for_test(StatusCode::UNAUTHORIZED, "bad token").await;
        assert!(matches!(out, UploadOutcome::Rejected { .. }));
    }

    #[tokio::test]
    async fn classify_5xx_returns_transient_keep() {
        let out = classify_status_for_test(StatusCode::INTERNAL_SERVER_ERROR, "ignored").await;
        match out {
            UploadOutcome::Transient { reason } => {
                assert!(
                    reason.contains("500"),
                    "reason should include status code; got {reason:?}"
                );
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_503_returns_transient_keep() {
        let out = classify_status_for_test(StatusCode::SERVICE_UNAVAILABLE, "ignored").await;
        assert!(matches!(out, UploadOutcome::Transient { .. }));
    }

    #[tokio::test]
    async fn classify_429_returns_transient_not_rejected() {
        // Field bug 2026-05-17 a third field-test host: 429 was being classified as
        // 4xx-Rejected → sidecar deleted permanently → ~1267 of 1317
        // crash reports lost when the backend rate-limited a drain
        // burst. 429 MUST be Transient so the next agent startup
        // retries.
        let out = classify_status_for_test(
            StatusCode::TOO_MANY_REQUESTS,
            "Too Many Requests! Wait for 0s",
        )
        .await;
        match out {
            UploadOutcome::Transient { reason } => {
                assert!(
                    reason.contains("429"),
                    "reason should mention 429; got {reason:?}"
                );
                assert!(
                    reason.contains("rate-limited"),
                    "reason should label as rate-limit; got {reason:?}"
                );
            }
            other => panic!("expected Transient (rate-limit retry), got {other:?}"),
        }
    }

    #[test]
    fn inter_request_delay_under_steady_state_rate_limit() {
        // Backend rate-limit is 60 req/min = 1 req/sec steady-state.
        // INTER_REQUEST_DELAY of 1100ms keeps us comfortably under
        // the limit even with clock drift / network jitter; a value
        // below 1000ms would silently trip the limiter once the
        // burst budget is exhausted.
        assert!(
            INTER_REQUEST_DELAY >= Duration::from_millis(1000),
            "inter-request delay must be >= 1s to stay under the 60 req/min limit"
        );
    }

    #[test]
    // Compile-time contract lock — the constant-ness IS the point.
    #[allow(clippy::assertions_on_constants)]
    fn crash_drain_interval_is_reasonable() {
        // Phase 4 pin: too short → wasted CPU + needless HTTP load on
        // long-running healthy agents. Too long → sidecars sit on
        // disk for hours after network repair, defeating the rc.58
        // periodic-drain motivation. 5 min hits the field-tuned
        // sweet spot: cheap when no sidecars are pending (empty
        // pending_all() returns immediately), responsive enough that
        // an operator opening the admin UI within a coffee break of
        // a network repair sees the evidence land.
        assert!(
            CRASH_DRAIN_INTERVAL_SECS >= 60,
            "CRASH_DRAIN_INTERVAL_SECS must be >= 60s to keep periodic \
             drain off the hot path; got {CRASH_DRAIN_INTERVAL_SECS}"
        );
        assert!(
            CRASH_DRAIN_INTERVAL_SECS <= 30 * 60,
            "CRASH_DRAIN_INTERVAL_SECS must be <= 30min so post-repair \
             sidecar surfacing stays operator-visible; got {CRASH_DRAIN_INTERVAL_SECS}"
        );
    }

    #[test]
    fn delete_sidecar_is_idempotent_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.json");
        std::fs::write(&path, b"{}").expect("write");
        let sidecar = plain(&path);
        std::fs::remove_file(&path).unwrap();
        // The sidecar is no longer there: must succeed (NotFound is
        // mapped to Ok).
        assert!(delete_sidecar(&sidecar).is_ok());
    }

    #[test]
    fn delete_sidecar_removes_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("present.json");
        std::fs::write(&path, b"{}").expect("write");
        assert!(path.exists());
        assert!(delete_sidecar(&plain(&path)).is_ok());
        assert!(!path.exists());
    }
}
