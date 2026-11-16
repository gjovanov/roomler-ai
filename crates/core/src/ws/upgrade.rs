// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! What every WebSocket upgrade shares (FR-69 P7b): the frame ceiling, the
//! query shape the three roles and `/derp` dial with, the affinity check,
//! and the "say why, then close" refusal. The upgrades themselves live with
//! their owners — the user socket in the host, the agent socket in `fleet`,
//! the tunnel-client socket and `/derp` in `network` — and each of them
//! needs exactly these four. The native ones (agent, tunnel client, `/derp`)
//! also share where their credential is found and the challenge a refusal of
//! it carries (#3656): [`native_credential`], [`with_challenge`].

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use roomler_ai_remote_control::upgrade_auth::{self, Carrier};
use serde::Deserialize;
use tracing::{debug, warn};

/// Upper bound on one inbound WebSocket message AND on one frame, applied to
/// every upgrade.
///
/// axum inherits tungstenite's defaults (64 MiB message, 16 MiB frame), and
/// nothing else bounds a post-upgrade frame: `tower_governor` is HTTP
/// middleware and never sees them, so an authenticated peer could make the
/// server buffer multi-MiB messages on every connection it opens.
///
/// Everything that legitimately crosses these sockets is control-plane —
/// signalling JSON, a netmap, an MTU-sized DERP packet — so 8 MiB is orders of
/// magnitude above real traffic while removing that amplification. Deliberately
/// generous rather than tight: a cap that is merely large is safe, whereas one
/// tuned close to the real maximum silently drops a big-but-valid netmap on the
/// day a fleet grows.
pub const MAX_WS_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// The query every socket dial carries.
#[derive(Debug, Deserialize)]
pub struct WsParams {
    /// The credential, when it travels in the URL.
    ///
    /// **Optional since the session-cookie path landed.** A browser on our own
    /// origin no longer needs it: the handshake is same-origin, so the
    /// `access_token` cookie is attached automatically and the host's
    /// `session_cookie` reads it. Native clients (the agent, the tunnel CLI)
    /// have no cookie jar: from 0.4.210 they send `Authorization: Bearer`
    /// instead, and only older ones send this (#3656, [`native_credential`]).
    ///
    /// Why nothing should send it: a query string is written to every access
    /// log it passes through and kept in browser history, and long-lived
    /// sockets reconnect constantly — so a credential that lives for days (a
    /// user's) or a year (a device's) piled up in plaintext on disk.
    /// `files/nginx-pod.conf` currently answers that with `access_log off` on
    /// `/ws`, which is a stopgap to be removed once nothing emits `?token=`
    /// any more.
    #[serde(default)]
    pub token: Option<String>,
    /// Optional connection role. Defaults to `"user"` to preserve existing
    /// browser behaviour. Set to `"agent"` by the native remote-control agent.
    #[serde(default)]
    pub role: Option<String>,
    /// S6 — optional tenant-affinity key. The front load-balancer hashes
    /// on it so one tenant's users, agents, tunnel clients and DERP
    /// sockets co-locate on one pod (the in-memory rc-hub / tunnel-hub /
    /// mediasoup state is pod-local). The SERVER only validates it:
    /// agent/tunnel JWTs carry `tenant_id` → must match; user JWTs carry
    /// no tenant claim → validated via a `tenant_members` lookup.
    /// Absent = legacy client → accepted (the LB pins those to pod 1).
    #[serde(default)]
    pub tid: Option<String>,
}

/// Validate a claimed affinity `tid` against a token-derived tenant id.
/// `None` (param absent) is always fine — validation only rejects a
/// PRESENT-but-wrong claim (mis-routed or forged affinity key).
pub fn tid_matches_claim(tid: &Option<String>, claim_tenant_hex: &str) -> bool {
    match tid {
        None => true,
        Some(t) => t == claim_tenant_hex,
    }
}

/// #3656 — the credential a native upgrade (`/ws` as `role=agent` or
/// `role=tunnel-client`, and `/derp`) is judged on: an `Authorization:
/// Bearer` header, else `query`, the `token` that clients older than 0.4.210
/// send in the URL.
///
/// A Bearer header decides, and the query is then not read, so a refused
/// header is refused even beside a good query. Any other `Authorization`
/// (Basic, NTLM, Negotiate, an empty Bearer) counts as no header: a middlebox
/// that adds its own must not take an older client's credential away. The
/// rules are [`upgrade_auth::presented`]'s, which the regional relay applies
/// to its ticket too. Only the transport is decided here; every audience,
/// `tid` and device check runs on the result unchanged.
///
/// Never for the user role: a browser cannot put a header on a WebSocket
/// handshake, and that role keeps its query token and cookie as they are.
pub fn native_credential(headers: &HeaderMap, query: Option<String>) -> Option<String> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let (credential, carrier) = upgrade_auth::presented(authorization, query.as_deref())?;
    // Which carrier, and whether the query was passed over: never a value.
    debug!(
        ?carrier,
        query_ignored = carrier == Carrier::Header && query.is_some(),
        "native upgrade: credential found"
    );
    Some(credential.to_owned())
}

/// #3656 — a native upgrade's answer, with the challenge a refusal carries
/// (RFC 6750 §3): a 401 says `WWW-Authenticate: Bearer realm="roomler"`, plus
/// `error="invalid_token"` when a credential arrived in either carrier, and a
/// 400 after one arrived says `error="invalid_request"`. Anything else (the
/// 101, a 403 for a `tid` that names another tenant, a 500, a 503 for an
/// unmounted module) passes unchanged, as does every status and body.
///
/// `presented` is whether [`native_credential`] found one. A client reads the
/// challenge to tell a server older than the header, which answers a
/// header-only dial with no challenge at all, from a verdict on its
/// credential ([`upgrade_auth::asks_for_query_form`]).
pub fn with_challenge(mut resp: Response, presented: bool) -> Response {
    if let Some(challenge) = upgrade_auth::challenge_for(resp.status().as_u16(), presented) {
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(challenge),
        );
    }
    resp
}

/// rc.53: push a server-initiated close frame WITH a structured
/// `ServerMsg` text frame in front of it, so the agent / tunnel-client
/// learns WHY the connection is being closed before the socket
/// drops. Used at the WS-handler refusal sites (the agent and
/// tunnel-client upgrades) when the row is deleted or quarantined.
///
/// Tungstenite serialises both frames into the same outbound TCP
/// buffer; the OS delivers them in order. No `sleep` guard is needed
/// — the `.await` on `send(Close)` completes the underlying flush.
/// On send-error we still attempt the close so the agent at least
/// gets the TCP FIN; on close-send-error we just drop (the socket
/// is already dead, nothing further to do).
pub async fn send_goodbye_and_close<M: serde::Serialize>(
    socket: WebSocket,
    msg: &M,
    close_code: u16,
    close_reason: &str,
) {
    let mut socket = socket;
    let json = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            warn!(%e, "failed to serialise Goodbye payload; closing without it");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    if let Err(e) = socket.send(Message::Text(json.into())).await {
        warn!(%e, "Goodbye text-send failed; attempting close anyway");
    }
    if let Err(e) = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code,
            reason: close_reason.to_string().into(),
        })))
        .await
    {
        debug!(%e, "Goodbye close-send failed (socket may already be dropped)");
    }
    // socket drops here; tungstenite has flushed both frames into
    // the outbound TCP buffer before the close-send `.await`
    // returned.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorization(value: &[u8]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_bytes(value).unwrap(),
        );
        headers
    }

    /// #3656 — a Bearer header decides; another scheme, an empty Bearer or a
    /// value that is not text leaves an older client's query credential as
    /// it was; with neither, nothing is presented.
    #[test]
    fn a_bearer_header_decides_and_anything_else_leaves_the_query() {
        let query = || Some("from-query".to_string());
        assert_eq!(
            native_credential(&authorization(b"Bearer from-header"), query()).as_deref(),
            Some("from-header")
        );
        assert_eq!(
            native_credential(&authorization(b"bearer \t from-header "), None).as_deref(),
            Some("from-header")
        );
        for other in [
            &b"Basic dXNlcjpwYXNz"[..],
            b"Negotiate YIIGhg",
            b"Bearer ",
            b"Bearer \xff\xfe",
        ] {
            assert_eq!(
                native_credential(&authorization(other), query()).as_deref(),
                Some("from-query"),
                "{}",
                String::from_utf8_lossy(other)
            );
        }
        assert_eq!(
            native_credential(&HeaderMap::new(), Some(String::new())).as_deref(),
            Some(""),
            "an empty query credential is presented, and fails verification"
        );
        assert_eq!(native_credential(&HeaderMap::new(), None), None);
    }

    /// #3656 — the challenge rides a 401 and a 400 only; every status and
    /// body stays what the upgrade answered.
    #[test]
    fn a_refusal_carries_the_challenge_and_nothing_else_changes() {
        let answer = |status: u16| {
            Response::builder()
                .status(status)
                .body("as answered".into())
                .unwrap()
        };
        let challenge = |resp: &Response| {
            resp.headers()
                .get(header::WWW_AUTHENTICATE)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        assert_eq!(
            challenge(&with_challenge(answer(401), false)).as_deref(),
            Some(upgrade_auth::CHALLENGE)
        );
        assert_eq!(
            challenge(&with_challenge(answer(401), true)).as_deref(),
            Some(upgrade_auth::CHALLENGE_REFUSED)
        );
        assert_eq!(
            challenge(&with_challenge(answer(400), true)).as_deref(),
            Some(upgrade_auth::CHALLENGE_BAD_REQUEST)
        );
        for (status, presented) in [
            (400, false),
            (101, true),
            (403, true),
            (500, true),
            (503, false),
        ] {
            let resp = with_challenge(answer(status), presented);
            assert_eq!(resp.status().as_u16(), status);
            assert_eq!(challenge(&resp), None, "{status}");
        }
    }
}
