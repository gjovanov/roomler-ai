//! `Hub` — process-global registry of online agents and live sessions.
//!
//! The Hub is `Clone`-able (it's a thin Arc handle) and is shared across all
//! Axum WS handlers. It owns:
//!
//! - `agents`:   `agent_id` → connected agent's tx + metadata
//! - `sessions`: `session_id` → live session state
//!
//! All concurrent access goes through `DashMap` so we don't take a global
//! lock on every signaling message. The session inner state is wrapped in
//! `parking_lot::Mutex` because it's read+modified together (state machine).

use bson::oid::ObjectId;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audit::AuditSink;
use crate::consent::{ConsentOutcome, DEFAULT_CONSENT_TIMEOUT};
use crate::error::{Error, Result};
use crate::models::{AuditKind, EndReason, OsKind, SessionPhase};
use crate::permissions::Permissions;
use crate::session::{ClientTx, LiveSession};
use crate::signaling::{ClientMsg, Role, ServerMsg};
use crate::turn_creds::{TurnConfig, ice_servers_for};

const SERVER_TX_CAPACITY: usize = 64;

// ────────────────────────────────────────────────────────────────────────────

pub struct ConnectedAgent {
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub owner_user_id: ObjectId,
    pub os: OsKind,
    pub tx: ClientTx,
    pub active_sessions: u8,
    pub max_sessions: u8,
}

pub struct ConnectedController {
    pub user_id: ObjectId,
    pub tx: ClientTx,
}

pub struct HubInner {
    /// Online agents, keyed by agent_id.
    agents: DashMap<ObjectId, ConnectedAgent>,

    /// Live sessions (anything not yet `Closed`).
    sessions: DashMap<ObjectId, Arc<Mutex<LiveSession>>>,

    /// Per-controller-user connections (one user may have multiple browser tabs).
    controllers: DashMap<ObjectId, Vec<ClientTx>>,

    /// TURN issuance.
    turn: Option<TurnConfig>,

    /// Audit sink.
    audit: AuditSink,
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<HubInner>,
}

impl Hub {
    pub fn new(audit: AuditSink, turn: Option<TurnConfig>) -> Self {
        Self {
            inner: Arc::new(HubInner {
                agents: DashMap::new(),
                sessions: DashMap::new(),
                controllers: DashMap::new(),
                turn,
                audit,
            }),
        }
    }

    // ─── connection registration ──────────────────────────────────────

    /// Called by the WS handler when an agent finishes auth+hello.
    /// Returns the rx half the WS layer should pump to the socket.
    pub fn register_agent(
        &self,
        agent_id: ObjectId,
        tenant_id: ObjectId,
        owner_user_id: ObjectId,
        os: OsKind,
        max_sessions: u8,
    ) -> mpsc::Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel(SERVER_TX_CAPACITY);
        let entry = ConnectedAgent {
            agent_id,
            tenant_id,
            owner_user_id,
            os,
            tx,
            active_sessions: 0,
            max_sessions,
        };
        if let Some(prev) = self.inner.agents.insert(agent_id, entry) {
            // Replace older connection (e.g. agent reconnected on a flap).
            // The old tx is dropped → its rx ends → the old WS task exits.
            warn!("agent {} reconnected; dropping previous connection", agent_id);
            drop(prev);
        }
        info!("agent {} online", agent_id);
        rx
    }

    pub fn unregister_agent(&self, agent_id: ObjectId) {
        if self.inner.agents.remove(&agent_id).is_some() {
            info!("agent {} offline", agent_id);
            // Force-close any sessions tied to this agent.
            let dead: Vec<ObjectId> = self
                .inner
                .sessions
                .iter()
                .filter_map(|s| {
                    let live = s.value().lock();
                    (live.agent_id == agent_id).then_some(*s.key())
                })
                .collect();
            for sid in dead {
                let _ = self.terminate(sid, EndReason::AgentDisconnect);
            }
        }
    }

    /// Called by the WS handler when a controller browser tab connects.
    pub fn register_controller(&self, user_id: ObjectId) -> (ClientTx, mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = mpsc::channel(SERVER_TX_CAPACITY);
        self.inner
            .controllers
            .entry(user_id)
            .or_default()
            .push(tx.clone());
        (tx, rx)
    }

    pub fn unregister_controller(&self, user_id: ObjectId, tx: &ClientTx) {
        if let Some(mut list) = self.inner.controllers.get_mut(&user_id) {
            list.retain(|t| !ptr_eq(t, tx));
        }

        // Terminate any sessions this controller still owns. Without this
        // the agent's active_sessions counter never drops when a browser
        // tab closes mid-session, and subsequent Connect attempts fail
        // with AgentBusy until the agent itself disconnects.
        let orphaned: Vec<ObjectId> = self
            .inner
            .sessions
            .iter()
            .filter(|e| e.value().lock().controller_user_id == user_id)
            .map(|e| *e.key())
            .collect();
        for session_id in orphaned {
            let _ = self.terminate(session_id, EndReason::ControllerHangup);
        }
    }

    // ─── session lifecycle ────────────────────────────────────────────

    /// Controller asked to start a session against `agent_id`.
    /// Creates the session, notifies the agent, returns the new session id.
    /// The caller (WS dispatcher) is expected to follow up by awaiting consent
    /// in a spawned task — see [`Self::run_consent_flow`].
    pub fn create_session(
        &self,
        agent_id: ObjectId,
        controller_user_id: ObjectId,
        controller_name: String,
        controller_tx: ClientTx,
        permissions: Permissions,
    ) -> Result<ObjectId> {
        let agent_org = {
            let mut agent = self
                .inner
                .agents
                .get_mut(&agent_id)
                .ok_or_else(|| Error::AgentOffline(agent_id.to_hex()))?;
            if agent.active_sessions >= agent.max_sessions {
                return Err(Error::AgentBusy);
            }
            agent.active_sessions += 1;
            agent.tenant_id
        };

        let session_id = ObjectId::new();
        let (live, waiter) = LiveSession::new(
            session_id,
            agent_id,
            agent_org,
            controller_user_id,
            permissions,
            controller_tx.clone(),
        );
        self.inner
            .sessions
            .insert(session_id, Arc::new(Mutex::new(live)));

        // Tell the controller the session id.
        let _ = controller_tx.try_send(ServerMsg::SessionCreated {
            session_id,
            agent_id,
        });

        // Move to AwaitingConsent and tell the agent.
        self.with_session(session_id, |s| s.transition(SessionPhase::AwaitingConsent))?;
        let agent_tx = self.agent_tx(agent_id)?;
        let _ = agent_tx.try_send(ServerMsg::Request {
            session_id,
            controller_user_id,
            controller_name,
            permissions,
            consent_timeout_secs: DEFAULT_CONSENT_TIMEOUT.as_secs() as u32,
        });

        self.audit(session_id, agent_id, agent_org, AuditKind::SessionRequested);
        self.audit(session_id, agent_id, agent_org, AuditKind::ConsentPrompted);

        // Spawn the consent watcher.
        let hub = self.clone();
        tokio::spawn(async move {
            let outcome = waiter.wait(DEFAULT_CONSENT_TIMEOUT).await;
            hub.handle_consent_outcome(session_id, outcome);
        });

        Ok(session_id)
    }

    fn handle_consent_outcome(&self, session_id: ObjectId, outcome: ConsentOutcome) {
        let (agent_id, tenant_id, controller_tx) = {
            let Some(arc) = self.inner.sessions.get(&session_id) else { return };
            let s = arc.value().lock();
            (s.agent_id, s.tenant_id, s.controller_tx.clone())
        };

        match outcome {
            ConsentOutcome::Granted => {
                self.audit(session_id, agent_id, tenant_id, AuditKind::ConsentGranted);
                if let Err(e) = self.with_session(session_id, |s| {
                    s.transition(SessionPhase::Negotiating)
                }) {
                    warn!("post-consent transition failed: {e}");
                    let _ = self.terminate(session_id, EndReason::Error);
                    return;
                }
                // Tell the controller it can send its offer.
                if let Some(tx) = controller_tx {
                    let user_id = self.controller_for(session_id).unwrap_or_default();
                    let ice = ice_servers_for(&user_id.to_hex(), self.inner.turn.as_ref());
                    let _ = tx.try_send(ServerMsg::Ready {
                        session_id,
                        ice_servers: ice,
                    });
                }
            }
            ConsentOutcome::Denied => {
                self.audit(session_id, agent_id, tenant_id, AuditKind::ConsentDenied);
                let _ = self.terminate(session_id, EndReason::UserDenied);
            }
            ConsentOutcome::Timeout => {
                self.audit(session_id, agent_id, tenant_id, AuditKind::ConsentTimedOut);
                let _ = self.terminate(session_id, EndReason::ConsentTimeout);
            }
        }
    }

    /// Caller: WS dispatcher when it sees `rc:consent` from agent.
    pub fn deliver_consent(&self, session_id: ObjectId, granted: bool) -> Result<()> {
        let arc = self
            .inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
        let slot = {
            let mut s = arc.value().lock();
            s.consent_slot.take()
        };
        slot.ok_or_else(|| Error::BadMessage("consent already delivered"))?
            .resolve(granted)
    }

    // ─── SDP / ICE forwarding ────────────────────────────────────────

    /// Forward controller's SDP offer to the agent.
    pub fn forward_offer(&self, session_id: ObjectId, sdp: String) -> Result<()> {
        let agent_id = self.with_session(session_id, |s| Ok(s.agent_id))?;
        let user_id = self.controller_for(session_id).unwrap_or_default();
        let ice = ice_servers_for(&user_id.to_hex(), self.inner.turn.as_ref());
        let agent_tx = self.agent_tx(agent_id)?;
        agent_tx
            .try_send(ServerMsg::SdpOffer {
                session_id,
                sdp,
                ice_servers: ice,
            })
            .map_err(|_| Error::SendFailed)
    }

    /// Forward agent's SDP answer to the controller.
    pub fn forward_answer(&self, session_id: ObjectId, sdp: String) -> Result<()> {
        let (controller_tx, user_id) = {
            let arc = self
                .inner
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
            let s = arc.value().lock();
            (s.controller_tx.clone(), s.controller_user_id)
        };
        let tx = controller_tx.ok_or(Error::SendFailed)?;
        let ice = ice_servers_for(&user_id.to_hex(), self.inner.turn.as_ref());
        tx.try_send(ServerMsg::SdpAnswer {
            session_id,
            sdp,
            ice_servers: ice,
        })
        .map_err(|_| Error::SendFailed)?;

        // Once the answer is in flight, mark the session active.
        // (The peers may still be doing ICE, but signaling is done from our POV.)
        self.with_session(session_id, |s| s.transition(SessionPhase::Active))?;
        let (sid, aid, oid) = self
            .with_session(session_id, |s| Ok((s.id, s.agent_id, s.tenant_id)))?;
        self.audit(sid, aid, oid, AuditKind::SessionStarted);
        Ok(())
    }

    /// Forward an ICE candidate to whichever side didn't send it.
    pub fn forward_ice(&self, role: Role, session_id: ObjectId, candidate: serde_json::Value) -> Result<()> {
        let (agent_id, controller_tx) = {
            let arc = self
                .inner
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
            let s = arc.value().lock();
            (s.agent_id, s.controller_tx.clone())
        };
        let dest_tx = match role {
            Role::Controller => self.agent_tx(agent_id)?,        // controller → agent
            Role::Agent => controller_tx.ok_or(Error::SendFailed)?,
        };
        dest_tx
            .try_send(ServerMsg::Ice {
                session_id,
                candidate,
            })
            .map_err(|_| Error::SendFailed)
    }

    // ─── termination ─────────────────────────────────────────────────

    pub fn terminate(&self, session_id: ObjectId, reason: EndReason) -> Result<()> {
        let Some((_, arc)) = self.inner.sessions.remove(&session_id) else {
            return Ok(()); // already gone, idempotent
        };

        let (agent_id, tenant_id, controller_tx) = {
            let mut s = arc.lock();
            // Best-effort transition; ignore if already closed.
            let _ = s.transition(SessionPhase::Closed);
            (s.agent_id, s.tenant_id, s.controller_tx.clone())
        };

        // Decrement agent session counter.
        if let Some(mut a) = self.inner.agents.get_mut(&agent_id) {
            a.active_sessions = a.active_sessions.saturating_sub(1);
        }

        // Notify both sides (best-effort; either may be gone).
        let msg = ServerMsg::Terminate { session_id, reason };
        if let Some(tx) = controller_tx {
            let _ = tx.try_send(msg.clone());
        }
        if let Ok(agent_tx) = self.agent_tx(agent_id) {
            let _ = agent_tx.try_send(msg);
        }

        self.audit(
            session_id,
            agent_id,
            tenant_id,
            AuditKind::SessionEnded { reason },
        );
        Ok(())
    }

    // ─── helpers ─────────────────────────────────────────────────────

    fn agent_tx(&self, agent_id: ObjectId) -> Result<ClientTx> {
        self.inner
            .agents
            .get(&agent_id)
            .map(|a| a.tx.clone())
            .ok_or_else(|| Error::AgentOffline(agent_id.to_hex()))
    }

    fn controller_for(&self, session_id: ObjectId) -> Option<ObjectId> {
        self.inner
            .sessions
            .get(&session_id)
            .map(|s| s.value().lock().controller_user_id)
    }

    fn with_session<F, R>(&self, session_id: ObjectId, f: F) -> Result<R>
    where
        F: FnOnce(&mut LiveSession) -> Result<R>,
    {
        let arc = self
            .inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_hex()))?;
        let mut s = arc.value().lock();
        f(&mut s)
    }

    fn audit(&self, sid: ObjectId, aid: ObjectId, oid: ObjectId, k: AuditKind) {
        self.inner.audit.record(sid, aid, oid, k);
    }

    // ─── introspection (for /api/sessions and admin) ─────────────────

    pub fn online_agents(&self) -> Vec<ObjectId> {
        self.inner.agents.iter().map(|e| *e.key()).collect()
    }

    pub fn is_agent_online(&self, agent_id: ObjectId) -> bool {
        self.inner.agents.contains_key(&agent_id)
    }

    pub fn live_sessions(&self) -> usize {
        self.inner.sessions.len()
    }
}

fn ptr_eq(a: &ClientTx, b: &ClientTx) -> bool {
    // mpsc::Sender doesn't expose pointer identity directly; use same_channel.
    a.same_channel(b)
}

// ────────────────────────────────────────────────────────────────────────────
// High-level dispatch — the WS handler funnels every parsed ClientMsg here.
// ────────────────────────────────────────────────────────────────────────────

pub struct DispatchCtx {
    pub role: Role,
    pub user_id: Option<ObjectId>,    // Some for Controller
    pub agent_id: Option<ObjectId>,   // Some for Agent
    pub controller_name: Option<String>,
    pub controller_tx: Option<ClientTx>,
}

impl Hub {
    pub fn dispatch(&self, ctx: &DispatchCtx, msg: ClientMsg) -> Result<()> {
        match (ctx.role, msg) {
            (
                Role::Controller,
                ClientMsg::SessionRequest {
                    agent_id,
                    permissions,
                    browser_caps: _,
                },
            ) => {
                // browser_caps reserved for codec negotiation (2B.2);
                // ignored at the Hub today. Once SDP munging lands the
                // Hub can forward the list to the agent in the
                // Request-to-agent envelope.
                let user_id = ctx.user_id.ok_or(Error::PermissionDenied("no user"))?;
                let name = ctx.controller_name.clone().unwrap_or_default();
                let tx = ctx.controller_tx.clone().ok_or(Error::SendFailed)?;
                self.create_session(agent_id, user_id, name, tx, permissions)?;
                Ok(())
            }
            (Role::Controller, ClientMsg::SdpOffer { session_id, sdp }) => {
                self.forward_offer(session_id, sdp)
            }
            (Role::Agent, ClientMsg::SdpAnswer { session_id, sdp }) => {
                self.forward_answer(session_id, sdp)
            }
            (Role::Agent, ClientMsg::Consent { session_id, granted }) => {
                self.deliver_consent(session_id, granted)
            }
            (role, ClientMsg::Ice { session_id, candidate }) => {
                self.forward_ice(role, session_id, candidate)
            }
            (_, ClientMsg::Terminate { session_id, reason }) => {
                self.terminate(session_id, reason)
            }
            (_, ClientMsg::Ping { id: _ }) => Ok(()), // pong handled by WS layer
            (_, ClientMsg::AgentHello { .. } | ClientMsg::AgentHeartbeat { .. }) => {
                // Hello is handled at registration time; heartbeat is logged by WS layer.
                Ok(())
            }
            (role, msg) => {
                warn!("unexpected msg for role {role:?}: {:?}", msg);
                Err(Error::BadMessage("wrong role for message"))
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditSink;
    use mongodb::Client;
    use std::time::Duration;

    async fn test_hub() -> Hub {
        // Use an in-memory-ish setup: Mongo isn't actually contacted unless we
        // record audit; the channel buffers it and we never let the task tick.
        // For a real CI test that exercises Mongo, see the integration tests.
        let client = Client::with_uri_str("mongodb://localhost:27017")
            .await
            .expect("mongo for tests");
        let db = client.database("rc_test");
        let (audit, _h) = AuditSink::spawn(db);
        Hub::new(audit, None)
    }

    #[tokio::test]
    async fn rejects_session_for_offline_agent() {
        let hub = test_hub().await;
        let (tx, _rx) = mpsc::channel(8);
        let res = hub.create_session(
            ObjectId::new(),
            ObjectId::new(),
            "Goran".into(),
            tx,
            Permissions::default(),
        );
        assert!(matches!(res, Err(Error::AgentOffline(_))));
    }

    #[tokio::test]
    async fn end_to_end_consent_grant() {
        let hub = test_hub().await;
        let agent_id = ObjectId::new();
        let _agent_rx = hub.register_agent(
            agent_id,
            ObjectId::new(),
            ObjectId::new(),
            OsKind::Linux,
            3,
        );
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let sid = hub
            .create_session(
                agent_id,
                ObjectId::new(),
                "Goran".into(),
                ctl_tx,
                Permissions::default(),
            )
            .unwrap();

        // Controller should immediately receive SessionCreated.
        let m = ctl_rx.try_recv().unwrap();
        assert!(matches!(m, ServerMsg::SessionCreated { .. }));

        // Deliver consent.
        hub.deliver_consent(sid, true).unwrap();

        // Give the consent task a tick to fire.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Controller should now have a Ready message.
        let m = ctl_rx.try_recv().unwrap();
        assert!(matches!(m, ServerMsg::Ready { .. }));
    }
}
