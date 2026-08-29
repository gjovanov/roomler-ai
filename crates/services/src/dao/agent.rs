// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use std::collections::HashMap;

use bson::{DateTime, Document, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::models::{
    AccessPolicy, Agent, AgentCaps, AgentStatus, DesiredConfig, DisplayInfo, ExecPolicy, OsKind,
    PeerRelayPolicy, SshPolicy,
};

use super::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

pub struct AgentDao {
    pub base: BaseDao<Agent>,
}

impl AgentDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Agent::COLLECTION),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        tenant_id: ObjectId,
        owner_user_id: ObjectId,
        name: String,
        machine_id: String,
        os: OsKind,
        agent_version: String,
        agent_token_hash: String,
    ) -> DaoResult<Agent> {
        let now = DateTime::now();
        let agent = Agent {
            id: None,
            tenant_id,
            owner_user_id,
            enrolled_by: Some(owner_user_id),
            name,
            display_name: None,
            tags: Vec::new(),
            name_admin_set: false,
            machine_id,
            os,
            agent_version,
            // FR-27 — unknown until the device's first heartbeat, for the same
            // reason as the host key below: enrolment never reaches the machine.
            companion_version: None,
            // Unknown until the device's first hello — enrolment does not
            // reach the machine, so there is nothing to record yet.
            ssh_host_pubkey: String::new(),
            agent_token_hash,
            status: AgentStatus::Offline,
            last_seen_at: now,
            last_presence: None,
            warm_relay_endpoint: None,
            displays: Vec::new(),
            capabilities: AgentCaps::default(),
            access_policy: AccessPolicy::default(),
            // Fleet RPC off on a new device — enabling it is a deliberate
            // admin act, never a side effect of enrollment.
            exec_policy: ExecPolicy::default(),
            ssh_policy: SshPolicy::default(),
            peer_relay_policy: Default::default(),
            // Nothing requested until an operator asks for something
            // (docs/remote-config.md). A freshly enrolled device must not
            // arrive carrying a config intent nobody wrote.
            desired_config: DesiredConfig::default(),
            // The device has never spoken, so it has said nothing. Distinct
            // from "it said nothing happened" — see `Agent::config_report`.
            config_report: None,
            key_rotation: None,
            key_rotation_report: None,
            overlay_identity: None,
            routes: Vec::new(),
            advertised_routes: Vec::new(),
            relay_home: None,
            relay_rtt: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let id = self.base.insert_one(&agent).await?;
        self.base.find_by_id(id).await
    }

    /// Locate an agent by `(tenant_id, machine_id)` regardless of soft-delete
    /// state. The unique index on this pair is unconditional, so a soft-deleted
    /// row still occupies the slot — the enroll path calls this to detect that
    /// case and rehydrate via [`Self::rehydrate`] rather than failing with E11000.
    pub async fn find_by_tenant_and_machine(
        &self,
        tenant_id: ObjectId,
        machine_id: &str,
    ) -> DaoResult<Option<Agent>> {
        self.base
            .find_one(doc! {
                "tenant_id": tenant_id,
                "machine_id": machine_id,
            })
            .await
    }

    /// Refresh an existing agent row at re-enrollment time: clear `deleted_at`
    /// (in case the row was soft-deleted), update os / agent_version from the
    /// new enrollment payload, bump `updated_at`. Returns the updated row so
    /// the caller can issue a fresh agent token against it.
    ///
    /// `name` is refreshed ONLY while `name_admin_set` is unset: machine-
    /// reported names keep flowing for never-renamed devices, but a re-enroll
    /// must not silently revert an admin rename (it used to — the rename
    /// route existed while every re-enroll clobbered its effect).
    pub async fn rehydrate(
        &self,
        agent_id: ObjectId,
        name: &str,
        os: OsKind,
        agent_version: &str,
    ) -> DaoResult<Agent> {
        let os_bson = bson::to_bson(&os).unwrap_or(bson::Bson::Null);
        self.base
            .update_by_id(
                agent_id,
                doc! {
                    "$set": {
                        "os": os_bson,
                        "agent_version": agent_version,
                        "updated_at": DateTime::now(),
                        "deleted_at": bson::Bson::Null,
                    }
                },
            )
            .await?;
        // Second, FILTERED update for the name half — update_by_id can't
        // carry the extra predicate. Matching zero docs (admin-renamed row)
        // is the intended no-op, not an error.
        self.base
            .update_one(
                doc! { "_id": agent_id, "name_admin_set": { "$ne": true } },
                doc! { "$set": { "name": name } },
            )
            .await?;
        self.base.find_by_id(agent_id).await
    }

    pub async fn list_for_tenant(
        &self,
        tenant_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<Agent>> {
        self.base
            .find_paginated(
                doc! { "tenant_id": tenant_id, "deleted_at": null },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    /// Every non-tombstoned device in the tenant, unpaginated.
    ///
    /// For whole-org operations (archiving revokes them all) rather than UI
    /// lists — those want [`Self::list_for_tenant`] and its pagination.
    pub async fn list_all_active_for_tenant(&self, tenant_id: ObjectId) -> DaoResult<Vec<Agent>> {
        self.base
            .find_many(doc! { "tenant_id": tenant_id, "deleted_at": null }, None)
            .await
    }

    /// S5 — active (non-tombstoned) devices in the tenant, for the plan
    /// device-cap check at enrollment.
    pub async fn count_active_for_tenant(&self, tenant_id: ObjectId) -> DaoResult<u64> {
        self.base
            .count(doc! { "tenant_id": tenant_id, "deleted_at": null })
            .await
    }

    pub async fn find_in_tenant(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
    ) -> DaoResult<Agent> {
        self.base.find_by_id_in_tenant(tenant_id, agent_id).await
    }

    pub async fn update_hello(
        &self,
        agent_id: ObjectId,
        agent_version: &str,
        displays: &[DisplayInfo],
        capabilities: &AgentCaps,
        advertised_routes: &[String],
        ssh_host_pubkey: &str,
    ) -> DaoResult<bool> {
        let displays_bson = bson::to_bson(displays).unwrap_or(bson::Bson::Array(vec![]));
        let caps_bson = bson::to_bson(capabilities).unwrap_or(bson::Bson::Null);
        let advertised_bson = bson::to_bson(advertised_routes).unwrap_or(bson::Bson::Array(vec![]));
        // Written on EVERY hello, including when empty. A device that
        // switched SSH off, or downgraded to a build without it, must stop
        // advertising a key it no longer holds — leaving the old value would
        // have clients verifying against an identity that is gone.
        self.base
            .update_by_id(
                agent_id,
                doc! {
                    "$set": {
                        "agent_version": agent_version,
                        "displays": displays_bson,
                        "capabilities": caps_bson,
                        "advertised_routes": advertised_bson,
                        "ssh_host_pubkey": ssh_host_pubkey,
                        "status": bson::to_bson(&AgentStatus::Online).unwrap(),
                        "last_seen_at": DateTime::now(),
                    }
                },
            )
            .await
    }

    pub async fn mark_status(&self, agent_id: ObjectId, status: AgentStatus) -> DaoResult<bool> {
        self.base
            .update_by_id(
                agent_id,
                doc! {
                    "$set": {
                        "status": bson::to_bson(&status).unwrap(),
                        "last_seen_at": DateTime::now(),
                    }
                },
            )
            .await
    }

    /// Refresh `last_seen_at` from a periodic agent heartbeat. Hello +
    /// mark_status touch the same field, but they only fire at session
    /// boundaries (connect / disconnect); without this method a long-
    /// lived but quiet agent stays at "last_seen = hello time" forever.
    /// 30 s heartbeat cadence on the agent keeps the field fresh enough
    /// that "agent online" can be defined as `last_seen_at > now - 90 s`.
    ///
    /// Phase A-1: also re-asserts `status: Online`. A heartbeat only
    /// arrives over a live registered WS, so Online is definitionally
    /// true — and this bounds any status-clobber race (a displaced
    /// handler's or a late reaper's wrongful `Offline`) to ≤30 s.
    pub async fn touch_heartbeat(
        &self,
        agent_id: ObjectId,
        warm_relay: Option<&str>,
        companion_version: Option<&str>,
    ) -> DaoResult<bool> {
        // C4 stage 2 — the standing warm allocation's relayed address rides
        // the same per-heartbeat write: stored pair-less so a peer can be
        // handed a dial target for this agent without waking its (possibly
        // captured) control WS; `$unset` while no leg is live so a stale
        // address can never be served.
        let mut set = doc! {
            "last_seen_at": DateTime::now(),
            "status": bson::to_bson(&AgentStatus::Online).unwrap(),
        };
        let mut unset = Document::new();
        match warm_relay {
            Some(ep) => {
                set.insert("warm_relay_endpoint", ep);
            }
            None => {
                unset.insert("warm_relay_endpoint", "");
            }
        }
        // FR-27 — the companion version follows the same present/absent rule
        // as the warm leg, and for the same reason: a stale value is worse
        // than none. An agent that stops finding a companion (uninstalled, or
        // a probe that started failing) must clear the field rather than leave
        // the grid asserting a version that is no longer on the host.
        //
        // ⚠️ A PRE-FR-27 agent sends no field at all, which arrives here as
        // `None` and therefore `$unset` — correct, because such an agent also
        // never set it, so there is nothing to erase.
        match companion_version {
            Some(v) => {
                set.insert("companion_version", v);
            }
            None => {
                unset.insert("companion_version", "");
            }
        }
        let update = if unset.is_empty() {
            doc! { "$set": set }
        } else {
            doc! { "$set": set, "$unset": unset }
        };
        self.base.update_by_id(agent_id, update).await
    }

    /// Multi-region relay PoPs: persist the agent's derived `relay_home` and
    /// its full probe table (observability). Called from the WS probe-report
    /// handler, already hysteresis- and rate-limited there.
    pub async fn set_relay_home(
        &self,
        agent_id: ObjectId,
        relay_home: Option<&str>,
        relay_rtt: &[roomler_ai_remote_control::signaling::RelayRegionRtt],
    ) -> DaoResult<bool> {
        let rtt_bson = bson::to_bson(relay_rtt).unwrap_or(bson::Bson::Array(vec![]));
        self.base
            .update_by_id(
                agent_id,
                doc! { "$set": {
                    "relay_home": relay_home,
                    "relay_rtt": rtt_bson,
                    "updated_at": DateTime::now(),
                } },
            )
            .await
    }

    /// Phase A-1 graceful shutdown: bulk-offline the agents whose WSs
    /// this pod held (belt-and-braces behind the per-socket teardowns).
    pub async fn mark_status_many(&self, ids: &[ObjectId], status: AgentStatus) -> DaoResult<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.base
            .update_many(
                doc! { "_id": { "$in": ids.to_vec() } },
                doc! { "$set": {
                    "status": bson::to_bson(&status).unwrap(),
                    "last_seen_at": DateTime::now(),
                } },
            )
            .await
    }

    /// P4 — CAS on the `last_presence` broadcast ledger. Returns `true` iff
    /// THIS caller moved the field (i.e. it should fan the `device:presence`
    /// transition out); a concurrent path on any pod that already recorded
    /// the same value loses the race and stays silent. Tombstoned rows never
    /// match — a deleted device must not resurrect as a badge event.
    pub async fn set_presence_if_changed(
        &self,
        agent_id: ObjectId,
        presence: &str,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": agent_id,
                    "deleted_at": null,
                    "last_presence": { "$ne": presence },
                },
                doc! { "$set": { "last_presence": presence } },
            )
            .await
    }

    /// P4 — the presence sweeper's scan set: every non-tombstoned row whose
    /// Mongo status still claims Online OR whose last BROADCAST presence was
    /// online/stale. The second arm is what catches a pod that died without
    /// teardown (its shutdown belt may have flipped `status` while the
    /// ledger still says "online" — the offline event is still owed).
    pub async fn find_presence_scan_set(&self) -> DaoResult<Vec<Agent>> {
        self.base
            .find_many(
                doc! {
                    "deleted_at": null,
                    "$or": [
                        { "status": bson::to_bson(&AgentStatus::Online).unwrap() },
                        { "last_presence": { "$in": ["online", "stale"] } },
                    ],
                },
                None,
            )
            .await
    }

    /// P9 — batched heartbeat freshness for the overlay netmap's presence
    /// check: which of `ids` heartbeated within `within_ms`? One `$in` query;
    /// ids missing from the result read STALE (an orphaned overlay row must
    /// not look dialable).
    pub async fn last_seen_fresh(
        &self,
        ids: &[ObjectId],
        within_ms: i64,
    ) -> DaoResult<HashMap<ObjectId, bool>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let cutoff = DateTime::from_millis(DateTime::now().timestamp_millis() - within_ms);
        let rows = self
            .base
            .find_many(doc! { "_id": { "$in": ids.to_vec() } }, None)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|a| a.id.map(|id| (id, a.last_seen_at > cutoff)))
            .collect())
    }

    pub async fn update_access_policy(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        policy: &AccessPolicy,
    ) -> DaoResult<bool> {
        let policy_bson = bson::to_bson(policy).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "access_policy": policy_bson } },
            )
            .await
    }

    /// Replace the device's Fleet-RPC policy (gate 3). A `MANAGE_AGENTS`
    /// admin action, deliberately separate from `update_access_policy`:
    /// "may watch your screen" and "may run a root shell" must never be
    /// reachable through the same call.
    pub async fn update_exec_policy(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        policy: &ExecPolicy,
    ) -> DaoResult<bool> {
        let policy_bson = bson::to_bson(policy).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "exec_policy": policy_bson } },
            )
            .await
    }

    /// Replace the device's roomler-SSH policy (gate 3). A `MANAGE_AGENTS`
    /// admin action, exactly like [`Self::update_exec_policy`] — and kept
    /// separate from it so enabling one can never be a side effect of
    /// enabling the other.
    pub async fn update_ssh_policy(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        policy: &SshPolicy,
    ) -> DaoResult<bool> {
        let policy_bson = bson::to_bson(policy).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "ssh_policy": policy_bson } },
            )
            .await
    }

    /// FR-19 gate 3 — replace the device's org-relay approval. A
    /// `MANAGE_AGENTS` + `EXEC_DEVICE` admin action (`routes::peer_relay`),
    /// kept separate from the exec/SSH setters for the reason they are
    /// separate from each other: approving one power must never be a side
    /// effect of approving another.
    pub async fn update_peer_relay_policy(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        policy: &PeerRelayPolicy,
    ) -> DaoResult<bool> {
        let policy_bson = bson::to_bson(policy).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "peer_relay_policy": policy_bson } },
            )
            .await
    }

    /// Every live device the tenant has approved to serve as an org relay.
    /// Gate 3 only: whether it is also SERVING is gate 4, read off its
    /// advertised `relay-server` capability, and whether it is online is the
    /// presence path's answer — neither is this row's to give.
    pub async fn list_relay_approved(&self, tenant_id: ObjectId) -> DaoResult<Vec<Agent>> {
        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "deleted_at": null,
                    "peer_relay_policy.serve": true,
                },
                Some(doc! { "name": 1 }),
            )
            .await
    }

    /// Record the DEVICE's own report on a pushed desired-config
    /// (`docs/remote-config.md`).
    ///
    /// ⚠️ Unlike every other setter on this type, the writer here is the
    /// DEVICE, not an admin — so this must never grow into a general "let the
    /// agent patch its own row" primitive. It writes one field, whose whole
    /// meaning is "this is what the host claims"; `config_audit` holds the
    /// server's own record of what was asked for, and that is the side a
    /// dispute is settled on.
    ///
    /// Last-report-wins: the question this answers is "did the current
    /// revision land?", which is about now, not about history.
    pub async fn record_config_report(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        report: &roomler_ai_remote_control::models::ConfigReport,
    ) -> DaoResult<bool> {
        let report_bson = bson::to_bson(report).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "config_report": report_bson } },
            )
            .await
    }

    /// FR-40 — record an operator's standing rotation order on the row (the
    /// desired state the connect-time reconcile reads). Replaces any earlier
    /// order: a rotation is idempotent in intent, and the report is matched by
    /// `request_id`, so an answer to the superseded order cannot be mistaken
    /// for an answer to this one.
    pub async fn record_key_rotation_request(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        request: &roomler_ai_remote_control::models::KeyRotationRequest,
    ) -> DaoResult<bool> {
        let bson = bson::to_bson(request).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "key_rotation": bson } },
            )
            .await
    }

    /// FR-40 — stamp the moment an order reached a live socket. Guarded on
    /// the `request_id` so a late delivery of a superseded order cannot mark
    /// the current one as delivered.
    pub async fn mark_key_rotation_delivered(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        request_id: &str,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": agent_id,
                    "tenant_id": tenant_id,
                    "key_rotation.request_id": request_id,
                },
                doc! { "$set": { "key_rotation.delivered_at": bson::DateTime::now() } },
            )
            .await
    }

    /// FR-40 — the DEVICE's answer (same discipline as
    /// [`Self::record_config_report`]: one field, written by the device,
    /// meaning "this is what the host claims"; `key_rotation_audit` holds the
    /// server's own record of what was ordered).
    pub async fn record_key_rotation_report(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        report: &roomler_ai_remote_control::models::KeyRotationReport,
    ) -> DaoResult<bool> {
        let bson = bson::to_bson(report).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "key_rotation_report": bson } },
            )
            .await
    }

    /// FR-40 — what the device presented at its overlay join, as `ws::overlay`
    /// verified it. Stamped on EVERY join (not only after a rotation), so the
    /// row always carries the device's current overlay public key.
    pub async fn record_overlay_identity(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        identity: &roomler_ai_remote_control::models::OverlayIdentity,
    ) -> DaoResult<bool> {
        let bson = bson::to_bson(identity).unwrap_or(bson::Bson::Null);
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "overlay_identity": bson } },
            )
            .await
    }

    /// Replace the agent's advertised subnet-router CIDRs (mesh Phase 2). A
    /// `MANAGE_AGENTS` admin action. Callers pass already validated +
    /// canonicalized CIDR strings (see `normalize_routes` in the agent route
    /// handler); the mesh client longest-prefix-matches a LAN target IP
    /// against these to pick the covering agent. Still gated server-side by
    /// the tenant's `tunnel_policies` — routes only steer the dial, they
    /// don't authorize it.
    pub async fn update_routes(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        routes: &[String],
    ) -> DaoResult<bool> {
        let routes_bson = bson::to_bson(routes).unwrap_or(bson::Bson::Array(vec![]));
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "routes": routes_bson } },
            )
            .await
    }

    pub async fn rename(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        name: &str,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                // The flag is what stops the next re-enroll's `rehydrate`
                // from clobbering this rename with the machine-reported name.
                doc! { "$set": { "name": name, "name_admin_set": true } },
            )
            .await
    }

    /// Set/clear the friendly display label. Display-only — never propagates
    /// to the overlay/MagicDNS name.
    pub async fn set_display_name(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        display_name: Option<&str>,
    ) -> DaoResult<bool> {
        let update = match display_name {
            Some(v) => doc! { "$set": { "display_name": v } },
            None => doc! { "$unset": { "display_name": "" } },
        };
        self.base
            .update_one(doc! { "_id": agent_id, "tenant_id": tenant_id }, update)
            .await
    }

    /// Replace the device's whole tag list (the UI edits it as a set).
    pub async fn set_tags(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        tags: &[String],
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "tags": tags.to_vec() } },
            )
            .await
    }

    pub async fn soft_delete(&self, tenant_id: ObjectId, agent_id: ObjectId) -> DaoResult<bool> {
        self.base.soft_delete_in_tenant(tenant_id, agent_id).await
    }

    /// Reassign the device owner (a `MANAGE_AGENTS` admin action). Leaves
    /// `enrolled_by` untouched — that's the audit trail of who first enrolled it.
    pub async fn update_owner(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        owner_user_id: ObjectId,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": agent_id, "tenant_id": tenant_id },
                doc! { "$set": { "owner_user_id": owner_user_id } },
            )
            .await
    }
}
