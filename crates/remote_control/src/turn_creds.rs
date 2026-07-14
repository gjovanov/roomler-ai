//! Short-lived TURN credentials following the coturn REST API convention
//! ("draft-uberti-behave-turn-rest").
//!
//! Username format:  `<unix_expiry>:<user_id>`
//! Password:         base64( HMAC-SHA1(shared_secret, username) )
//!
//! The shared secret is the value passed as `--static-auth-secret` to coturn,
//! also set as `RC_TURN_SECRET` in roomler env.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::signaling::IceServer;

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// e.g. ["turn:coturn.roomler.live:3478?transport=udp",
    ///        "turn:coturn.roomler.live:3478?transport=tcp",
    ///        "turns:coturn.roomler.live:5349?transport=tcp"]
    pub urls: Vec<String>,
    /// Same-worker TURN affinity (2026-07-14): per-worker URL variant lists,
    /// one inner `Vec` per coturn worker (each expanded like `urls`). The
    /// generic hostname is 3 DNS A records (one per worker), so each ICE side
    /// resolves independently and relay↔relay sessions routinely straddle two
    /// workers — and cross-worker flows involving the dual-IP worker drop
    /// packets under its SNAT asymmetry (the rc.112 tunnel same-worker-pin
    /// lesson; field 2026-07-14: corp↔corp double-relay stall-bursts). When
    /// ≥2 workers are configured (`ROOMLER__TURN__WORKER_URLS`),
    /// [`Self::issue_for_session`] deterministically picks ONE worker per
    /// session for BOTH sides — worker URLs first, generic-hostname fallback
    /// retained. Empty (default) = exactly the old behaviour.
    pub workers: Vec<Vec<String>>,
    pub shared_secret: String,
    pub ttl_secs: u32,
}

impl TurnConfig {
    pub fn issue(&self, user_id: &str) -> IceServer {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expiry = now_secs + self.ttl_secs as u64;
        let username = format!("{expiry}:{user_id}");

        let mut mac = HmacSha1::new_from_slice(self.shared_secret.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(username.as_bytes());
        let credential = B64.encode(mac.finalize().into_bytes());

        IceServer {
            urls: self.urls.clone(),
            username: Some(username),
            credential: Some(credential),
        }
    }

    /// Same-worker affinity issuance: like [`Self::issue`] but, when ≥2
    /// workers are configured, puts ONE session-selected worker's URLs first
    /// (both ICE stacks weight earlier servers higher in candidate priority,
    /// so both sides converge on the same worker) with the generic-hostname
    /// `urls` kept after as the worker-down fallback. The selection is a
    /// stable hash of `session_key`, so the controller and the agent — issued
    /// creds independently by the Hub — pick the SAME worker. With <2 workers
    /// this is exactly [`Self::issue`].
    pub fn issue_for_session(&self, user_id: &str, session_key: &str) -> IceServer {
        let mut server = self.issue(user_id);
        if self.workers.len() >= 2 {
            let idx = (fnv1a(session_key.as_bytes()) % self.workers.len() as u64) as usize;
            let mut urls = self.workers[idx].clone();
            urls.extend(server.urls);
            server.urls = urls;
        }
        server
    }
}

/// FNV-1a — tiny, dependency-free stable hash for the per-session worker
/// pick. NOT security-sensitive (load spreading only); must simply agree
/// between the two `issue_for_session` calls for one session, which it does
/// trivially by being deterministic.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Convenience: also include public STUN servers so trickle ICE has something
/// to work with even before TURN auth completes.
pub fn ice_servers_for(user_id: &str, turn: Option<&TurnConfig>) -> Vec<IceServer> {
    let mut out = vec![IceServer {
        urls: vec!["stun:stun.l.google.com:19302".into()],
        username: None,
        credential: None,
    }];
    if let Some(t) = turn {
        out.push(t.issue(user_id));
    }
    out
}

/// Session-aware variant of [`ice_servers_for`] — used by the Hub on the two
/// per-session issuance paths (`Ready` → controller, `SdpOffer` → agent) so
/// both sides receive the SAME worker-first TURN ordering for one session.
pub fn ice_servers_for_session(
    user_id: &str,
    session_key: &str,
    turn: Option<&TurnConfig>,
) -> Vec<IceServer> {
    let mut out = vec![IceServer {
        urls: vec!["stun:stun.l.google.com:19302".into()],
        username: None,
        credential: None,
    }];
    if let Some(t) = turn {
        out.push(t.issue_for_session(user_id, session_key));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn affinity_cfg() -> TurnConfig {
        TurnConfig {
            urls: vec!["turn:coturn.example:3478".into()],
            workers: vec![
                vec!["turn:coturn-1.example:3478".into()],
                vec!["turn:coturn-2.example:3478".into()],
                vec!["turn:coturn-3.example:3478".into()],
            ],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        }
    }

    #[test]
    fn issue_for_session_is_deterministic_and_worker_first() {
        let cfg = affinity_cfg();
        // Same session key → same worker, on every call (this is what makes
        // BOTH sides of one session land on the same coturn worker).
        let a = cfg.issue_for_session("controller", "6a54bf440b4fd609a7356f97");
        let b = cfg.issue_for_session("agent", "6a54bf440b4fd609a7356f97");
        assert_eq!(a.urls[0], b.urls[0], "both sides must pick the same worker");
        assert!(a.urls[0].contains("coturn-"), "worker URL must come first");
        // Generic hostname retained as the fallback tail.
        assert_eq!(a.urls.last().unwrap(), "turn:coturn.example:3478");
    }

    #[test]
    fn issue_for_session_spreads_across_workers() {
        let cfg = affinity_cfg();
        // Different sessions should not all land on one worker.
        let mut seen = std::collections::HashSet::new();
        for i in 0..32 {
            let s = cfg.issue_for_session("u", &format!("session-{i}"));
            seen.insert(s.urls[0].clone());
        }
        assert!(seen.len() >= 2, "hash must spread sessions across workers");
    }

    #[test]
    fn issue_for_session_without_workers_matches_issue() {
        let cfg = TurnConfig {
            urls: vec!["turn:coturn.example:3478".into()],
            workers: vec![],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        };
        let s = cfg.issue_for_session("u1", "any-session");
        assert_eq!(s.urls, vec!["turn:coturn.example:3478".to_string()]);
    }

    #[test]
    fn issues_well_formed_creds() {
        let cfg = TurnConfig {
            urls: vec!["turn:coturn.example:3478".into()],
            workers: vec![],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        };
        let cred = cfg.issue("user_42");
        let username = cred.username.unwrap();
        let mut parts = username.splitn(2, ':');
        let expiry: u64 = parts.next().unwrap().parse().unwrap();
        let uid = parts.next().unwrap();
        assert_eq!(uid, "user_42");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(expiry > now && expiry <= now + 600);
        assert!(cred.credential.unwrap().len() >= 20);
    }

    #[test]
    fn ice_list_includes_stun() {
        let list = ice_servers_for("u1", None);
        assert!(list[0].urls[0].starts_with("stun:"));
    }
}
