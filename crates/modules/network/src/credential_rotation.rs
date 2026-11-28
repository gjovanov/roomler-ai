// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1d — what a token rotation changes of the device's place on the
//! mesh (`docs/fr/FR-155-one-row-new-credentials.md`, "The hook
//! `agent_credentials_rotated`").
//!
//! This commit carries the cells only: [`run`] does nothing yet, and no path
//! calls it.

use async_trait::async_trait;
use bson::oid::ObjectId;

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

/// The hook: the effects, in order.
pub async fn run(
    effects: &impl Effects,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    generation: u32,
) -> anyhow::Result<()> {
    let _ = (effects, tenant_id, agent_id, generation);
    Ok(())
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
