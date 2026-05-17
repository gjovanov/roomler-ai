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
//! Spawned as a `tokio::task::spawn` from `main.rs` AFTER the first
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

/// Drain every pending crash sidecar to roomler.ai. Best-effort:
/// any IO / network / parse failure logs `warn!` and continues with
/// the next sidecar so a single poisoned file doesn't block the
/// fleet. Returns when the queue is empty.
pub async fn drain_and_upload(cfg: &AgentConfig) {
    let pending = crash_recorder::pending_all();
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

    let mut ok_count = 0u32;
    let mut keep_count = 0u32;
    let mut drop_count = 0u32;
    for (path, payload) in pending {
        match upload_one(&client, &url, &cfg.agent_token, &payload).await {
            UploadOutcome::Accepted => {
                tracing::info!(file = %path.display(), "crash_uploader: uploaded + deleted");
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(
                        file = %path.display(),
                        error = %e,
                        "crash_uploader: post-upload delete failed; will re-upload next run"
                    );
                }
                ok_count += 1;
            }
            UploadOutcome::Rejected { status, body } => {
                tracing::warn!(
                    file = %path.display(),
                    status = %status,
                    body = %body,
                    "crash_uploader: server rejected payload; deleting (4xx is permanent)"
                );
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(
                        file = %path.display(),
                        error = %e,
                        "crash_uploader: post-reject delete failed"
                    );
                }
                drop_count += 1;
            }
            UploadOutcome::Transient { reason } => {
                tracing::warn!(
                    file = %path.display(),
                    reason = %reason,
                    "crash_uploader: transient failure; keeping sidecar for next startup"
                );
                keep_count += 1;
            }
        }
    }

    tracing::info!(
        uploaded = ok_count,
        kept = keep_count,
        dropped = drop_count,
        "crash_uploader: drain complete"
    );
}

/// Build the reqwest client with the same posture as
/// `enrollment.rs::enroll`'s plain `reqwest::Client::new()` —
/// system TLS roots, default timeouts, plus an explicit per-request
/// timeout via [`POST_TIMEOUT`].
fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!(
            "roomler-agent-crash-uploader/",
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

/// Delete a sidecar at `path` if it exists. Idempotent.
#[allow(dead_code)] // used by future Phase 2 + manual smoke tooling
pub fn delete_sidecar(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn delete_sidecar_is_idempotent_when_file_missing() {
        let dir = std::env::temp_dir().join(format!(
            "crash_uploader_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("nonexistent.json");
        // Don't create the dir — call delete on a path that
        // definitely doesn't exist. Must succeed (NotFound is mapped
        // to Ok).
        assert!(delete_sidecar(&path).is_ok());
    }

    #[test]
    fn delete_sidecar_removes_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "crash_uploader_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create_dir_all");
        let path = dir.join("present.json");
        std::fs::write(&path, b"{}").expect("write");
        assert!(path.exists());
        assert!(delete_sidecar(&path).is_ok());
        assert!(!path.exists());
        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
