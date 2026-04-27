//! Wire protocol for the `rc:*` WebSocket namespace.
//!
//! Both the agent and the controller browser speak the same envelope shape;
//! they're distinguished by which JWT audience their connection authenticated
//! with. See `signaling::Role`.
//!
//! Every message is a JSON object with a `t` discriminator. We use serde's
//! `tag = "t"` adjacent encoding so the wire is small and stable.
//!
//! **ObjectId fields are serialised as raw hex strings, not bson-extended
//! JSON (`{"$oid":"…"}`).** This matches the REST responses and is what
//! the browser / native agent clients actually produce. See
//! [`serde_helpers`] for the pinning shims; a regression test in that
//! module locks the format.

use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::models::{AgentCaps, DisplayInfo, EndReason, OsKind};
use crate::permissions::Permissions;
use crate::serde_helpers::{oid_hex, option_oid_hex};

/// Which side of the connection sent / receives a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Agent,
    Controller,
}

// ────────────────────────────────────────────────────────────────────────────
// Inbound from clients (agent or controller browser)
// ────────────────────────────────────────────────────────────────────────────

/// Messages the server receives.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
pub enum ClientMsg {
    // ─── agent → server ───────────────────────────────────────────────
    /// Agent announces itself after WS auth.
    #[serde(rename = "rc:agent.hello")]
    AgentHello {
        machine_name: String,
        os: OsKind,
        agent_version: String,
        displays: Vec<DisplayInfo>,
        caps: AgentCaps,
    },

    /// Agent periodic stats.
    #[serde(rename = "rc:agent.heartbeat")]
    AgentHeartbeat {
        rss_mb: u32,
        cpu_pct: f32,
        active_sessions: u8,
    },

    /// Agent answers a controller's offer.
    #[serde(rename = "rc:sdp.answer")]
    SdpAnswer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    /// Agent decision on a control request.
    #[serde(rename = "rc:consent")]
    Consent {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        granted: bool,
    },

    // ─── controller → server ─────────────────────────────────────────
    /// Controller initiates a session. Server creates the RemoteSession,
    /// notifies the agent, and waits for consent.
    ///
    /// `browser_caps` is the controller's `RTCRtpReceiver.
    /// getCapabilities('video').codecs` filtered to the codecs the
    /// agent's negotiation logic cares about (h264 / h265 / av1 / vp9).
    /// Phase 2 commit 2B.2 uses the intersection of this list with the
    /// agent's `AgentCaps.codecs` to pick the best codec for the
    /// session. Optional + default-empty so older controllers that
    /// don't include it still get an h264 session.
    ///
    /// `preferred_transport` (Phase Y.3) tells the agent which video
    /// transport the controller wants to use. Recognised values match
    /// `AgentCaps.transports`: today only `data-channel-vp9-444` is
    /// defined. `None` / unset means "use the WebRTC video track" —
    /// the legacy default that all in-flight controllers default to.
    /// The agent only honours the request when its own caps advertise
    /// the same transport (browser × agent intersection); otherwise
    /// it ignores the field and falls back to the WebRTC track.
    #[serde(rename = "rc:session.request")]
    SessionRequest {
        #[serde(with = "oid_hex")]
        agent_id: ObjectId,
        permissions: Permissions,
        #[serde(default)]
        browser_caps: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preferred_transport: Option<String>,
    },

    /// Controller sends an SDP offer (after consent granted).
    #[serde(rename = "rc:sdp.offer")]
    SdpOffer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    // ─── either side → server ────────────────────────────────────────
    /// Trickle ICE candidate. Server forwards to the peer.
    #[serde(rename = "rc:ice")]
    Ice {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        candidate: serde_json::Value, // { candidate, sdpMid, sdpMLineIndex, ... }
    },

    /// Either side hangs up.
    #[serde(rename = "rc:terminate")]
    Terminate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        reason: EndReason,
    },

    /// Liveness ping (cheap; the WS handler also has its own ping/pong).
    #[serde(rename = "rc:ping")]
    Ping { id: u32 },
}

// ────────────────────────────────────────────────────────────────────────────
// Outbound from server
// ────────────────────────────────────────────────────────────────────────────

/// Messages the server sends.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
pub enum ServerMsg {
    /// Sent to the controller right after `SessionRequest` so it knows the id.
    #[serde(rename = "rc:session.created")]
    SessionCreated {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        #[serde(with = "oid_hex")]
        agent_id: ObjectId,
    },

    /// Sent to the agent when a controller asks for control. The agent prompts
    /// the user (or auto-grants per AccessPolicy) and replies with `Consent`.
    ///
    /// `browser_caps` is forwarded verbatim from the controller's
    /// `rc:session.request` (codec short names like `"h264"`,
    /// `"h265"`, etc.). The agent intersects this with its own
    /// `AgentCaps.codecs` to pick the best codec for the session.
    /// Empty on controllers that don't advertise — the agent then
    /// defaults to H.264.
    ///
    /// `preferred_transport` (Phase Y.3) is also forwarded verbatim.
    /// `None` / unset means "use the WebRTC video track" (legacy
    /// default). Recognised values match `AgentCaps.transports` —
    /// today only `data-channel-vp9-444`. The agent honours the
    /// request when its caps advertise the same transport, else
    /// falls back to the WebRTC track silently.
    #[serde(rename = "rc:request")]
    Request {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        #[serde(with = "oid_hex")]
        controller_user_id: ObjectId,
        controller_name: String,
        permissions: Permissions,
        consent_timeout_secs: u32,
        #[serde(default)]
        browser_caps: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preferred_transport: Option<String>,
    },

    /// Server forwards SDP offer from controller → agent.
    #[serde(rename = "rc:sdp.offer")]
    SdpOffer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
        ice_servers: Vec<IceServer>,
    },

    /// Server forwards SDP answer from agent → controller.
    #[serde(rename = "rc:sdp.answer")]
    SdpAnswer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
        ice_servers: Vec<IceServer>,
    },

    /// Forward ICE candidate to the peer.
    #[serde(rename = "rc:ice")]
    Ice {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        candidate: serde_json::Value,
    },

    /// Sent to the controller after the agent has consented and is ready for
    /// the SDP offer. Controller now creates its PeerConnection.
    #[serde(rename = "rc:ready")]
    Ready {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        ice_servers: Vec<IceServer>,
    },

    /// Either peer is gone, or admin terminated, or consent denied.
    #[serde(rename = "rc:terminate")]
    Terminate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        reason: EndReason,
    },

    /// Reply to `Ping`.
    #[serde(rename = "rc:pong")]
    Pong { id: u32 },

    /// Generic error pushed to the client.
    #[serde(rename = "rc:error")]
    Error {
        #[serde(with = "option_oid_hex")]
        session_id: Option<ObjectId>,
        code: String,
        message: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_roundtrip() {
        let m = ClientMsg::Ping { id: 42 };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:ping""#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ClientMsg::Ping { id: 42 }));
    }

    #[test]
    fn ice_server_minimal() {
        let s = IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            username: None,
            credential: None,
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(!j.contains("username"));
    }

    #[test]
    fn object_ids_serialise_as_raw_hex_on_wire() {
        // Lock-in: no `$oid` wrapping anywhere in the WS protocol envelope.
        let session_id = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        let agent_id = ObjectId::parse_str("507f1f77bcf86cd799439012").unwrap();

        let created = ServerMsg::SessionCreated {
            session_id,
            agent_id,
        };
        let s = serde_json::to_string(&created).unwrap();
        assert!(
            !s.contains("$oid"),
            "extended JSON leaked into wire format: {s}"
        );
        assert!(s.contains("\"session_id\":\"507f1f77bcf86cd799439011\""));
        assert!(s.contains("\"agent_id\":\"507f1f77bcf86cd799439012\""));

        let req = ClientMsg::SessionRequest {
            agent_id,
            permissions: Permissions::VIEW | Permissions::INPUT,
            browser_caps: vec!["h264".into(), "h265".into()],
            preferred_transport: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("$oid"));
        assert!(s.contains("\"agent_id\":\"507f1f77bcf86cd799439012\""));
        assert!(s.contains("\"browser_caps\":[\"h264\",\"h265\"]"));
        // Default None must NOT serialise — keeps the wire compatible
        // with controllers that don't know about the field at all.
        assert!(
            !s.contains("preferred_transport"),
            "None should be skipped via skip_serializing_if"
        );

        // With a value set, the field appears
        let req_with_t = ClientMsg::SessionRequest {
            agent_id,
            permissions: Permissions::VIEW,
            browser_caps: vec![],
            preferred_transport: Some("data-channel-vp9-444".into()),
        };
        let s = serde_json::to_string(&req_with_t).unwrap();
        assert!(s.contains("\"preferred_transport\":\"data-channel-vp9-444\""));
    }

    #[test]
    fn session_request_browser_caps_default_empty_for_back_compat() {
        // A pre-2B.1 controller that doesn't include browser_caps
        // must still parse — the agent will fall back to h264-only
        // negotiation in that case.
        let json = r#"{"t":"rc:session.request","agent_id":"507f1f77bcf86cd799439012","permissions":"VIEW"}"#;
        let m: ClientMsg = serde_json::from_str(json).unwrap();
        match m {
            ClientMsg::SessionRequest { browser_caps, .. } => {
                assert!(
                    browser_caps.is_empty(),
                    "missing field must default to empty"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn accepts_extended_json_for_backward_compat() {
        // A client still sending extended JSON parses fine — eases rollout.
        let json = r#"{"t":"rc:session.request","agent_id":{"$oid":"507f1f77bcf86cd799439012"},"permissions":"VIEW | INPUT"}"#;
        let m: ClientMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(m, ClientMsg::SessionRequest { .. }));
    }

    #[test]
    fn error_msg_omits_null_session_id_is_ok() {
        let e = ServerMsg::Error {
            session_id: None,
            code: "x".into(),
            message: "y".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        // None → null, not omitted.
        assert!(s.contains("\"session_id\":null"));
    }
}
