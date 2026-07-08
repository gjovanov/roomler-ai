use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// A remote-control consent request routed to a device OWNER out-of-band
/// (email approve-link / web-push tap) — the async counterpart to the on-host
/// tray prompt, for devices with no one at the console. The `token` is an
/// unguessable capability: the PUBLIC `POST /api/consent/{token}/(approve|deny)`
/// route validates it and resolves the session's in-memory consent slot via
/// `hub.deliver_consent`. TTL-swept on `expires_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsentRequest {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The live remote-control session awaiting a decision. Resolving this
    /// request calls `hub.deliver_consent(session_id, granted)`.
    pub session_id: ObjectId,
    pub agent_id: ObjectId,
    pub controller_user_id: ObjectId,
    /// Display name of the controller, for the email / push body.
    pub controller_name: String,
    /// The device owner the request is routed to (email + push target).
    pub owner_user_id: ObjectId,
    /// Unguessable capability token embedded in the approve/deny link. Unique.
    pub token: String,
    #[serde(default)]
    pub status: ConsentRequestStatus,
    pub expires_at: DateTime,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConsentRequestStatus {
    #[default]
    Pending,
    Approved,
    Denied,
    /// Superseded (session timed out / closed) before the owner acted.
    Expired,
}

impl ConsentRequest {
    pub const COLLECTION: &'static str = "consent_requests";
}
