// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-115 P4b — per-org TURN bytes from coturn's statistics DB
//! (`docs/fr/FR-115-pricing-reshape.md`, *P4b, as proposed*). This is the
//! per-tenant TURN attribution FR-20's P3 deferred, by the route FR-20
//! recommended.
//!
//! The cluster's coturn, started with `--redis-statsdb`, publishes each
//! allocation's totals on the cluster Redis when the allocation is deleted
//! (the channels and the format are `remote_control::turn_stats`). A username
//! issued with `turn_creds::charged` (`<expiry>:<principal>:org-<org>`) names
//! the org that pays. This subscribes to both channels and adds what coturn
//! SENT on each leg (`sentb`, the egress it carried) to that org's
//! `turn_bytes`, the way DERP counts what it forwards.
//!
//! Booking is `roomler_core::turn_bytes::record`, shared with the PoP pull
//! (P4b-2): once per allocation leg across pods by a `SET NX`, never guessed
//! for a username that names no org, and never retried.
//!
//! Kill switch: `relay_allowance.turn_attribution`, off by default, because
//! coturn must publish here first.

use std::time::Duration;

use futures::StreamExt;
use roomler_core::turn_bytes::{self, Outcome, Source};
use tracing::{debug, info, warn};

use crate::NetworkState;

pub use roomler_ai_remote_control::turn_stats::{CHANNELS, Reported, parse};
pub use roomler_core::turn_bytes::TURN_UNATTRIBUTED_BYTES;

/// Record one leg against the org its username names, once across pods.
async fn record(state: &NetworkState, r: Reported) {
    // Without the claim every pod would record it ([`spawn`] does not
    // subscribe then; this is the belt to that).
    let Some(redis) = &state.redis_pubsub else {
        return;
    };
    match turn_bytes::record(redis, &state.stats, state.db.name(), Source::Cluster, &r).await {
        Outcome::Unattributed => info!(
            username = %r.username,
            bytes = r.sentb,
            unattributed_total =
                TURN_UNATTRIBUTED_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            "turn usage: a username naming no org (unattributed, never guessed)"
        ),
        Outcome::Failed(e) => warn!(%e, "turn usage: not recorded (under-reports)"),
        Outcome::Booked(_) | Outcome::AlreadyBooked | Outcome::Nothing => {}
    }
}

/// One subscription's life: connect, subscribe, record until the stream ends.
async fn run(state: &NetworkState, url: &str) -> Result<(), redis::RedisError> {
    let client = redis::Client::open(url)?;
    let mut pubsub = client.get_async_pubsub().await?;
    for channel in CHANNELS {
        pubsub.psubscribe(channel).await?;
    }
    info!("turn usage: subscribed to coturn's statistics");
    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        if !state.settings.stats.enabled {
            continue;
        }
        let channel = msg.get_channel_name().to_string();
        let Ok(payload) = msg.get_payload::<String>() else {
            continue;
        };
        match parse(&channel, &payload) {
            Some(r) => record(state, r).await,
            None => debug!(%channel, "turn usage: not a coturn total"),
        }
    }
    Ok(())
}

/// Subscribe on every pod, reconnecting with a backoff; nothing at all with
/// the kill switch off.
pub fn spawn(state: NetworkState) {
    if !state.settings.relay_allowance.turn_attribution {
        info!("turn usage: attribution off (relay_allowance.turn_attribution)");
        return;
    }
    let url = state.settings.redis.url.clone();
    if url.trim().is_empty() || state.redis_pubsub.is_none() {
        // No once-only claim without the cluster's Redis: every pod would
        // record every allocation. Under-reporting is the cheaper error.
        warn!("turn usage: no Redis pub/sub on this pod; TURN bytes are not attributed");
        return;
    }
    tokio::spawn(async move {
        let mut backoff = 1u64;
        loop {
            match run(&state, &url).await {
                Ok(()) => {
                    backoff = 1;
                    warn!("turn usage: the subscription ended; reconnecting");
                }
                Err(e) => warn!(%e, "turn usage: subscribe failed; retrying in {backoff}s"),
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}
