// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! WebSocket participation — the registration shape, not the socket.
//!
//! Core keeps the one `/ws` upgrade, the role gate, the tenant-affinity check,
//! connection storage, the dispatcher primitives and the Redis fan-out. A
//! module registers handlers **per (role, namespace)** and, if it owns one, an
//! extra upgrade endpoint (`network` owns `/derp`). The socket URL and the
//! front LB's affinity rules never move: agents in the field dial
//! `/ws?role=agent` across every release (FR-69 D7).
//!
//! A namespace is the wire's own grouping: the dotted prefix after `rc:` on the
//! agent socket (`overlay`, `tunnel`, `rpc`, …) and the colon prefix on the
//! user socket (`media`, `message`, `typing`, …). The exhaustive per-variant
//! map on `ClientMsg` — the thing that makes "a new variant without an owner"
//! a compile error — lands with the `fleet` module in P5.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

// FR-69 P1b — the socket layer's primitives, moved from the api crate
// unchanged: the connection registry, the fan-out helpers, the Redis pub/sub
// layer. The upgrade handler and the role gate stay in the api crate until
// the modules exist to register namespaces on them.
pub mod dispatcher;
pub mod redis_pubsub;
pub mod storage;

/// The three roles the socket gate admits, as today's `?role=` query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    User,
    Agent,
    TunnelClient,
}

/// What a handler knows about the connection a message arrived on. Filled by
/// core from the authenticated upgrade — a module never parses a token.
#[derive(Debug, Clone)]
pub struct WsCtx {
    pub connection_id: String,
    pub role: Role,
    /// The user, agent or tunnel-client id the token proved.
    pub principal: ObjectId,
    /// The tenant the socket is pinned to, when the role has one.
    pub tenant_id: Option<ObjectId>,
}

/// A namespace handler. The message arrives already parsed as JSON; the
/// handler decodes the module's own wire type from it.
#[async_trait]
pub trait WsHandler: Send + Sync {
    async fn handle(&self, ctx: &WsCtx, msg: serde_json::Value) -> anyhow::Result<()>;
}

/// One (role, namespace) → handler binding.
#[derive(Clone)]
pub struct WsHandlerSpec {
    pub role: Role,
    pub namespace: &'static str,
    pub handler: Arc<dyn WsHandler>,
}

/// An extra upgrade endpoint at the root (outside `/api`), e.g. `/derp`.
pub struct UpgradeSpec {
    pub path: &'static str,
    pub router: Router,
}

/// Everything a module registers with the socket layer.
#[derive(Default)]
pub struct WsRegistration {
    pub handlers: Vec<WsHandlerSpec>,
    pub upgrades: Vec<UpgradeSpec>,
}

impl std::fmt::Debug for WsHandlerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsHandlerSpec")
            .field("role", &self.role)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for UpgradeSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeSpec")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for WsRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsRegistration")
            .field("handlers", &self.handlers)
            .field("upgrades", &self.upgrades)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_wire_spellings_match_the_query_param() {
        // `/ws?role=agent` and `?role=tunnel-client` are what the field dials.
        assert_eq!(serde_json::to_string(&Role::Agent).unwrap(), "\"agent\"");
        assert_eq!(
            serde_json::to_string(&Role::TunnelClient).unwrap(),
            "\"tunnel-client\""
        );
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
    }
}
