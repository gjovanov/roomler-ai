// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Knowhow's half of the agent socket. The socket keeps one handler and one
//! lifecycle a module (`roomler_core::AgentSocketRegistry::register`), so
//! these two answer for all of knowhow's conversations with a device: its
//! scans (P4-3a, [`crate::scan`]), its probes (P4-5, [`crate::probe`]) and
//! its sessions' questions (P4-4-1, [`crate::session`]).

use async_trait::async_trait;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_core::{AgentCtx, AgentMsgHandler, AgentSocketLifecycle};

use crate::KnowhowState;
use crate::probe::ProbeAnswers;
use crate::scan::{ScanAnswers, SocketHooks};

/// The frames knowhow owns, dispatched here by `ClientMsg::namespace()`: a
/// scan's answer, a probe's, else a session's question. Anything else goes
/// back.
pub(crate) struct Frames {
    pub scans: ScanAnswers,
    pub probes: ProbeAnswers,
    pub state: KnowhowState,
}

#[async_trait]
impl AgentMsgHandler for Frames {
    async fn handle(&self, ctx: &AgentCtx, msg: ClientMsg) -> Option<ClientMsg> {
        let rest = self.scans.handle(ctx, msg).await?;
        let rest = self.probes.handle(ctx, rest).await?;
        self.state.questions.take(ctx, rest)
    }
}

/// A connection's life: its hello mirrors the device and links it for scans
/// and probes (P4-3a, P4-5), and opens its questions' queue (P4-4-1); its
/// closing undoes both.
pub(crate) struct Lifecycle {
    pub scans: SocketHooks,
    pub state: KnowhowState,
}

#[async_trait]
impl AgentSocketLifecycle for Lifecycle {
    async fn hello(&self, ctx: &AgentCtx) {
        self.scans.hello(ctx).await;
        crate::session::hello(&self.state, ctx);
    }

    async fn heartbeat(&self, ctx: &AgentCtx, warm_relay: Option<&str>) {
        self.scans.heartbeat(ctx, warm_relay).await;
    }

    async fn closing(&self, ctx: &AgentCtx) {
        self.scans.closing(ctx).await;
        self.state.questions.close(&ctx.conn_id);
    }

    async fn closed(&self, ctx: &AgentCtx, removal_was_ours: bool) {
        self.scans.closed(ctx, removal_was_ours).await;
    }
}
