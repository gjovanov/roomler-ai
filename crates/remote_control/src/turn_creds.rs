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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_well_formed_creds() {
        let cfg = TurnConfig {
            urls: vec!["turn:coturn.example:3478".into()],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        };
        let cred = cfg.issue("user_42");
        let username = cred.username.unwrap();
        let mut parts = username.splitn(2, ':');
        let expiry: u64 = parts.next().unwrap().parse().unwrap();
        let uid = parts.next().unwrap();
        assert_eq!(uid, "user_42");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert!(expiry > now && expiry <= now + 600);
        assert!(cred.credential.unwrap().len() >= 20);
    }

    #[test]
    fn ice_list_includes_stun() {
        let list = ice_servers_for("u1", None);
        assert!(list[0].urls[0].starts_with("stun:"));
    }
}
