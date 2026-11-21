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
//!
//! #3952 — the subscription probes its own connection
//! (`roomler_core::ws::sub_probe`), so a connection that went deaf is found
//! and subscribed again. The subscriber's loop waits on nothing but its
//! stream: each total goes to one recorder task over a bounded queue, so a
//! slow database never delays a probe into a false verdict.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use roomler_core::turn_bytes::{self, Outcome, Source};
use roomler_core::ws::redis_conn::LineBound;
use roomler_core::ws::sub_probe::{Lifetime, ProbeSettings, Prober, Subscriber};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::NetworkState;

/// Totals waiting for the recorder. Past this (a database slower than
/// coturn for that long) a total goes unrecorded, with a warn line:
/// under-reporting is the cheaper error, and the subscriber never waits.
const RECORDER_QUEUE: usize = 4096;

/// Totals not recorded because the recorder was behind.
static RECORDER_DROPPED: AtomicU64 = AtomicU64::new(0);
/// One line a second for them.
static RECORDER_DROP_LINES: LineBound = LineBound::per_second();

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

/// Hand one leg to the recorder without waiting. When the recorder is
/// behind, the leg goes unrecorded (under-reports) and the subscriber's loop
/// is never held up.
fn hand_over(recorder: &mpsc::Sender<Reported>, r: Reported) {
    if recorder.try_send(r).is_ok() {
        return;
    }
    let dropped = RECORDER_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some(suppressed) = RECORDER_DROP_LINES.admit() {
        warn!(
            dropped,
            suppressed,
            "turn usage: a total was not recorded: the recorder is behind (under-reports)"
        );
    }
}

/// One subscription's life: connect, subscribe, hand each total to the
/// recorder until the stream ends or the probe finds the connection deaf.
/// A probe is matched before the kill switch and the parser see it.
async fn run(
    state: &NetworkState,
    url: &str,
    prober: &mut Prober,
    recorder: &mpsc::Sender<Reported>,
) -> Result<Lifetime, redis::RedisError> {
    let client = redis::Client::open(url)?;
    let subscription = prober.subscribe(&client, &[], &CHANNELS).await?;
    info!(
        probe_ms = prober.interval_ms(),
        "turn usage: subscribed to coturn's statistics"
    );
    Ok(prober
        .run(subscription, |msg| {
            if !state.settings.stats.enabled {
                return;
            }
            let channel = msg.get_channel_name();
            let Ok(payload) = msg.get_payload::<String>() else {
                return;
            };
            match parse(channel, &payload) {
                Some(r) => hand_over(recorder, r),
                None => debug!(%channel, "turn usage: not a coturn total"),
            }
        })
        .await)
}

/// Subscribe on every pod, reconnecting with a backoff; nothing at all with
/// the kill switch off.
pub fn spawn(state: NetworkState) {
    if !state.settings.relay_allowance.turn_attribution {
        info!("turn usage: attribution off (relay_allowance.turn_attribution)");
        return;
    }
    let url = state.settings.redis.url.clone();
    let publisher = state.redis_pubsub.as_ref().map(|redis| redis.connection());
    let Some(publisher) = publisher.filter(|_| !url.trim().is_empty()) else {
        // No once-only claim without the cluster's Redis: every pod would
        // record every allocation. Under-reporting is the cheaper error.
        warn!("turn usage: no Redis pub/sub on this pod; TURN bytes are not attributed");
        return;
    };
    let probe = ProbeSettings::from_settings(&state.settings.redis);
    // One recorder for the subscriber's whole life, so a new subscription
    // loses nothing queued. Booking stays once per leg across pods.
    let (recorder, mut queue) = mpsc::channel::<Reported>(RECORDER_QUEUE);
    {
        let state = state.clone();
        tokio::spawn(async move {
            while let Some(r) = queue.recv().await {
                record(&state, r).await;
            }
        });
    }
    tokio::spawn(async move {
        let mut prober = Prober::new(Subscriber::Turn, publisher, probe);
        let mut backoff = 1u64;
        loop {
            match run(&state, &url, &mut prober, &recorder).await {
                Ok(Lifetime::Ended) => {
                    backoff = 1;
                    warn!("turn usage: the subscription ended; reconnecting");
                }
                // The verdict wrote its own line.
                Ok(Lifetime::Deaf) => backoff = 1,
                Err(e) => warn!(%e, "turn usage: subscribe failed; retrying in {backoff}s"),
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}
