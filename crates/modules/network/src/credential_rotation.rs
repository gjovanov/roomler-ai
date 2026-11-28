// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1d — what a token rotation changes of the device's place on the
//! mesh (`docs/fr/FR-155-one-row-new-credentials.md`, "The hook
//! `agent_credentials_rotated`").
//!
//! Fleet calls the hook once, on the pod where the rotation committed, after
//! it ended the device's control sockets below the new generation (on this
//! pod, and through the control event on every other) and before it answers
//! the new token. [`run`] then, in this order:
//!
//! 1. takes the device's overlay node offline, as a leave does
//!    (`crate::overlay::handle_overlay_leave`): the node is `Offline`, and
//!    its peers' netmaps mark it unreachable. A control socket that the
//!    rotation ended runs no leave of its own, so this one is the node's;
//! 2. ends the device's DERP registrations below the new generation on this
//!    pod ([`crate::derp_rotation`]). Every other pod ends its own when the
//!    control event arrives, and by its own check;
//! 3. orders the device to rotate its overlay key (FR-40, trigger
//!    `credential_rotation`), queued for its next connection, which is at
//!    the new generation. If the per-device limiter refuses the order, it is
//!    placed once more after the limiter's window
//!    ([`ORDER_AGAIN_AFTER_SECS`]).
//!
//! An error is the hook's answer: fleet logs, counts and audits it, and
//! answers the new token all the same.

use std::time::Duration;

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::KEY_ROTATION_TRIGGER_CREDENTIAL_ROTATION;
use tracing::{info, warn};

use crate::NetworkState;
use crate::overlay::{NodeIdentity, current_node, handle_overlay_leave};
use crate::routes::overlay_key::{KeyRotationDenyReason, OrderedBy, place_order};

/// When an order that the limiter refused is placed once more: past the
/// limiter's window, one order per device per minute
/// (`crate::routes::overlay_key`).
pub const ORDER_AGAIN_AFTER_SECS: u64 = 61;

/// What became of the key order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordered {
    /// The order is on the row: pushed, or queued for the next connection.
    Placed,
    /// The per-device limiter refused it.
    RateLimited,
    /// Refused for another reason, which its audit row names.
    Refused,
}

/// The hook's effects, one method each, so a test can stand in for them.
#[async_trait]
pub trait Effects: Send + Sync {
    /// The node goes offline, and its peers hear it.
    async fn take_offline(&self, tenant_id: ObjectId, agent_id: ObjectId);
    /// The agent's DERP registrations below `generation` end on this pod.
    fn end_derp_below(&self, tenant_id: ObjectId, agent_id: ObjectId, generation: u32) -> usize;
    /// The FR-40 order, `credential_rotation`.
    async fn order_key_rotation(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
    ) -> anyhow::Result<Ordered>;
    /// The order once more, after the limiter's window.
    fn order_again_later(&self, tenant_id: ObjectId, agent_id: ObjectId);
}

/// The hook: the effects, in order. An order that cannot be written is the
/// hook's error, after the other two effects ran. An order the limiter
/// refuses is not: it is placed once more later.
pub async fn run(
    effects: &impl Effects,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    generation: u32,
) -> anyhow::Result<()> {
    effects.take_offline(tenant_id, agent_id).await;
    effects.end_derp_below(tenant_id, agent_id, generation);
    match effects.order_key_rotation(tenant_id, agent_id).await? {
        Ordered::Placed | Ordered::Refused => {}
        Ordered::RateLimited => {
            info!(
                %tenant_id, %agent_id, after_secs = ORDER_AGAIN_AFTER_SECS,
                "fr-155: the overlay key order was rate limited; it is placed once more"
            );
            effects.order_again_later(tenant_id, agent_id);
        }
    }
    Ok(())
}

#[async_trait]
impl Effects for NetworkState {
    async fn take_offline(&self, _tenant_id: ObjectId, agent_id: ObjectId) {
        handle_overlay_leave(self, NodeIdentity::Agent(agent_id)).await;
    }

    fn end_derp_below(&self, tenant_id: ObjectId, agent_id: ObjectId, generation: u32) -> usize {
        self.derp_generations
            .end_below_generation(agent_id, tenant_id, generation)
    }

    async fn order_key_rotation(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
    ) -> anyhow::Result<Ordered> {
        let agent = self
            .fleet
            .agents
            .find_live_in_tenant(tenant_id, agent_id)
            .await?;
        // The key the device holds as the order is placed (FR-40 P1c: a join
        // under any other key afterwards is the proof that it rotated): its
        // node's, else the one its overlay identity names.
        let public_key_before = match current_node(self, NodeIdentity::Agent(agent_id)).await {
            Some(node) => Some(node.wg_public_key),
            None => agent
                .overlay_identity
                .as_ref()
                .map(|identity| identity.public_key.clone()),
        };
        let placed = place_order(
            self,
            tenant_id,
            agent_id,
            &agent,
            OrderedBy::Server(KEY_ROTATION_TRIGGER_CREDENTIAL_ROTATION),
            public_key_before,
        )
        .await?;
        Ok(match placed.outcome {
            Ok(dispatch) => {
                info!(
                    %tenant_id, %agent_id, request_id = %placed.request_id,
                    dispatch = dispatch.wire(),
                    "fr-155: the device was ordered to rotate its overlay key"
                );
                Ordered::Placed
            }
            Err(KeyRotationDenyReason::RateLimited) => Ordered::RateLimited,
            Err(reason) => {
                warn!(
                    %tenant_id, %agent_id, request_id = %placed.request_id,
                    reason = reason.wire(),
                    "fr-155: no overlay key order was placed"
                );
                Ordered::Refused
            }
        })
    }

    fn order_again_later(&self, tenant_id: ObjectId, agent_id: ObjectId) {
        let state = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(ORDER_AGAIN_AFTER_SECS)).await;
            match state.order_key_rotation(tenant_id, agent_id).await {
                Ok(Ordered::Placed) => {}
                Ok(Ordered::RateLimited | Ordered::Refused) => warn!(
                    %tenant_id, %agent_id,
                    "fr-155: the overlay key order was refused once more; its audit row says why"
                ),
                Err(e) => warn!(
                    %tenant_id, %agent_id, %e,
                    "fr-155: the overlay key order could not be written"
                ),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        Offline,
        DerpEnd(u32),
        Order,
        OrderAgainLater,
    }

    /// Records each effect, and answers the order with `answer`.
    struct StandIn {
        steps: Mutex<Vec<Step>>,
        answer: fn() -> anyhow::Result<Ordered>,
    }

    impl StandIn {
        fn new(answer: fn() -> anyhow::Result<Ordered>) -> Self {
            Self {
                steps: Mutex::new(Vec::new()),
                answer,
            }
        }

        fn note(&self, step: Step) {
            self.steps.lock().unwrap().push(step);
        }

        fn steps(&self) -> Vec<Step> {
            self.steps.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Effects for StandIn {
        async fn take_offline(&self, _: ObjectId, _: ObjectId) {
            self.note(Step::Offline);
        }

        fn end_derp_below(&self, _: ObjectId, _: ObjectId, generation: u32) -> usize {
            self.note(Step::DerpEnd(generation));
            1
        }

        async fn order_key_rotation(&self, _: ObjectId, _: ObjectId) -> anyhow::Result<Ordered> {
            self.note(Step::Order);
            (self.answer)()
        }

        fn order_again_later(&self, _: ObjectId, _: ObjectId) {
            self.note(Step::OrderAgainLater);
        }
    }

    /// T10 (FR-155 P1d): the hook's effects, in order. The node goes offline,
    /// then the agent's DERP registrations below the new generation end, then
    /// the key order is placed. An order the limiter refuses is placed once
    /// more later, and is not the hook's error. An order that cannot be
    /// written is the hook's error, after the other two effects ran.
    #[tokio::test]
    async fn credential_rotation_hook_runs_leave_then_order() {
        let (tenant, agent) = (ObjectId::new(), ObjectId::new());

        let placed = StandIn::new(|| Ok(Ordered::Placed));
        run(&placed, tenant, agent, 7)
            .await
            .expect("a placed order: the hook succeeds");
        assert_eq!(
            placed.steps(),
            vec![Step::Offline, Step::DerpEnd(7), Step::Order],
            "the node goes offline, then its DERP registrations below the new generation end, \
             then the key order is placed"
        );

        let limited = StandIn::new(|| Ok(Ordered::RateLimited));
        run(&limited, tenant, agent, 7)
            .await
            .expect("an order the limiter refuses is not the hook's error");
        assert_eq!(
            limited.steps(),
            vec![
                Step::Offline,
                Step::DerpEnd(7),
                Step::Order,
                Step::OrderAgainLater
            ],
            "an order the limiter refuses is placed once more later"
        );

        let failing = StandIn::new(|| Err(anyhow::anyhow!("the order could not be written")));
        assert!(
            run(&failing, tenant, agent, 7).await.is_err(),
            "an order that cannot be written is the hook's error"
        );
        assert_eq!(
            failing.steps(),
            vec![Step::Offline, Step::DerpEnd(7), Step::Order],
            "after the other two effects ran"
        );
    }
}
