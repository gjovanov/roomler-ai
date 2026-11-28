// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1e — the generation floors the API pushes to the regional DERP
//! relays (`docs/fr/FR-155-one-row-new-credentials.md`, "Every regional relay
//! holds A and B for the device's generation (P1e)").
//!
//! A DERP ticket carries the generation of the control socket it was minted
//! to (`roomler_ai_remote_control::derp_ticket::DerpTicketClaims::generation`).
//! When a token rotation commits G+1, the network module's hook commits a
//! floor at G+1 for each key a ticket minted below it can name
//! ([`RelayFloors::commit`]). The floor is stored where every pod reads it (a
//! Redis hash, merged by the higher generation), kept by this pod too, and
//! pushed at once to every relay that takes floors, before the rotation's
//! route answers. Every poll of the relays' `/stats` pushes every live floor
//! again (`crate::relay_load::spawn_poller`), so a relay the commit's push did
//! not reach has the floor within one poll.
//!
//! A floor lasts until [`floor_until`]: past every ticket minted below it. A
//! ticket verifies at a relay while `now <= exp + DERP_TICKET_LEEWAY_SECS`,
//! and a ticket below G+1 is minted before the commit, or within the ticket
//! arm's bounded read of the row just after it, so its `exp` is at most the
//! commit's time plus that read plus `DERP_TICKET_TTL`. The floor's `until`
//! is the hook's time (after the commit) plus the TTL, the leeway and
//! [`FLOOR_MARGIN_SECS`]. The relay judges both on its own clock; the margin
//! also covers the pods' clocks differing.
//!
//! [`crate::state::Core::relay_floors`] is `Some` only while the token
//! rotation is on (`rc.agent_token_rotation` and `rc.token_generation_check`)
//! and a ticket key is configured. Otherwise no floor is stored and no set is
//! pushed, so a relay holds no live set: it refuses no ticket for its
//! generation, and a re-registration is last-writer-wins.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use roomler_ai_remote_control::derp_ticket::{
    DERP_TICKET_LEEWAY_SECS, DERP_TICKET_TTL, DerpTicketSigner, GenerationFloor,
};
use tracing::{debug, info, warn};

use crate::ws::redis_pubsub::RedisPubSub;

/// How far a floor outlasts the last ticket below it: room for the ticket
/// arm's read of the row, and for the pods' clocks to differ.
pub const FLOOR_MARGIN_SECS: u64 = 3600;
/// The bound on one push to every relay at once.
pub const FLOOR_PUSH_SECS: u64 = 3;

/// When a floor committed at `now` (unix seconds) ends.
pub fn floor_until(now: u64) -> u64 {
    now + DERP_TICKET_TTL.as_secs() + DERP_TICKET_LEEWAY_SECS + FLOOR_MARGIN_SECS
}

/// The Redis hash of the live floors, scoped to the database so two
/// deployments that share a Redis never read each other's. Field
/// `network|key`, value `gen|until`.
pub fn floors_key(db_name: &str) -> String {
    format!("roomler:relay_floors:{db_name}")
}

/// Does this `/stats` body come from a relay that takes floors? Only one that
/// reports the floors it holds (`floors`, an object, since P1e) does.
pub fn takes_floor_push(body: &serde_json::Value) -> bool {
    body.get("floors").is_some_and(serde_json::Value::is_object)
}

/// Where a relay takes floors: under the `/stats` path its nginx proxies.
pub fn floors_url(stats_url: &str) -> String {
    format!("{}/floors", stats_url.trim_end_matches('/'))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// (network hex, base64 key) → (generation, until).
type FloorMap = BTreeMap<(String, String), (u32, u64)>;

/// Merge one floor into `map` by the higher generation and the later until.
fn raise(map: &mut FloorMap, net: String, pk: String, generation: u32, until: u64) {
    let floor = map.entry((net, pk)).or_insert((generation, until));
    floor.0 = floor.0.max(generation);
    floor.1 = floor.1.max(until);
}

/// The generation floors this pod commits and pushes to the regional relays.
pub struct RelayFloors {
    signer: DerpTicketSigner,
    redis: Option<Arc<RedisPubSub>>,
    key: String,
    /// What this pod committed: pushed without Redis, and beside the store.
    local: Mutex<FloorMap>,
    /// The floors URL of every relay whose last answered `GET /stats` took
    /// floors ([`takes_floor_push`]); the poller sets it.
    takers: RwLock<Vec<String>>,
    client: reqwest::Client,
    /// How long a relay counts a pushed set as live: three polls.
    ttl: u64,
}

impl RelayFloors {
    /// `None` unless the token rotation is on and the ticket key is usable.
    pub fn new(
        settings: &roomler_ai_config::Settings,
        redis: Option<Arc<RedisPubSub>>,
        db_name: &str,
    ) -> Option<Self> {
        if !(settings.rc.agent_token_rotation && settings.rc.token_generation_check) {
            return None;
        }
        let private_b64 = settings.relay.derp_ticket_private_key.as_deref()?;
        let signer = match DerpTicketSigner::from_pkcs8_b64(private_b64) {
            Ok(signer) => signer,
            Err(e) => {
                warn!(%e, "relay floors: no usable ticket key; no relay is told a floor");
                return None;
            }
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(FLOOR_PUSH_SECS))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                warn!(%e, "relay floors: client build failed; no relay is told a floor");
                return None;
            }
        };
        Some(Self {
            signer,
            redis,
            key: floors_key(db_name),
            local: Mutex::default(),
            takers: RwLock::default(),
            client,
            ttl: settings.relay.stats_poll_secs.saturating_mul(3),
        })
    }

    /// A rotation's commit: store `floors` where every pod reads them, then
    /// push the live set to every relay that takes floors, at once and within
    /// [`FLOOR_PUSH_SECS`]. The error is the store's. A relay the push does
    /// not reach has the floors on the next poll, from the store or, on this
    /// pod, from its own.
    pub async fn commit(&self, floors: Vec<GenerationFloor>) -> Result<(), String> {
        if floors.is_empty() {
            return Ok(());
        }
        let now = unix_now();
        {
            let mut local = self.local.lock().unwrap_or_else(|p| p.into_inner());
            for f in &floors {
                raise(
                    &mut local,
                    f.net.clone(),
                    f.pk.clone(),
                    f.generation,
                    f.until,
                );
            }
        }
        let mut stored = Ok(());
        if let Some(redis) = &self.redis {
            for f in &floors {
                let field = format!("{}|{}", f.net, f.pk);
                if let Err(e) = redis
                    .floor_raise(&self.key, &field, f.generation, f.until, now)
                    .await
                {
                    stored = Err(format!("the shared store refused a floor: {e}"));
                    break;
                }
            }
        }
        let (reached, of) = self.push().await;
        info!(
            floors = floors.len(),
            reached, of, "fr-155: the generation floors were pushed to the regional relays"
        );
        stored
    }

    /// The poller's half: the floors URLs of the relays that take floors.
    pub fn set_takers(&self, urls: Vec<String>) {
        *self.takers.write().unwrap_or_else(|p| p.into_inner()) = urls;
    }

    /// Sign the live set and `POST` it to every relay that takes floors, at
    /// once (one dead relay must not stretch the push by its timeout per
    /// relay), within [`FLOOR_PUSH_SECS`]. How many took it, of how many.
    pub async fn push(&self) -> (usize, usize) {
        let takers = self
            .takers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if takers.is_empty() {
            return (0, 0);
        }
        let token = match self.signer.sign_floor_set(self.live_set().await, self.ttl) {
            Ok(token) => token,
            Err(e) => {
                warn!(%e, "relay floors: signing failed");
                return (0, takers.len());
            }
        };
        let token = &token;
        let pushes = takers.iter().map(|url| {
            let req = self
                .client
                .post(url)
                .header("content-type", "application/jwt")
                .body(token.clone());
            async move { (url, req.send().await) }
        });
        let answers = match tokio::time::timeout(
            Duration::from_secs(FLOOR_PUSH_SECS + 1),
            futures::future::join_all(pushes),
        )
        .await
        {
            Ok(answers) => answers,
            Err(_) => {
                debug!("relay floors: the push outlasted its bound");
                return (0, takers.len());
            }
        };
        let mut reached = 0;
        for (url, res) in answers {
            match res {
                Ok(r) if r.status().is_success() => reached += 1,
                Ok(r) => debug!(%url, status = %r.status(), "relay floors: refused"),
                Err(e) => debug!(%url, %e, "relay floors: failed"),
            }
        }
        (reached, takers.len())
    }

    /// The live floors: the store's and this pod's own, merged by the higher
    /// generation and the later until. A store that cannot be read leaves
    /// this pod's own, which lowers nothing at a relay.
    async fn live_set(&self) -> Vec<GenerationFloor> {
        let now = unix_now();
        let mut merged = {
            let mut local = self.local.lock().unwrap_or_else(|p| p.into_inner());
            local.retain(|_, (_, until)| *until >= now);
            local.clone()
        };
        if let Some(redis) = &self.redis {
            match redis.hash_all(&self.key).await {
                Ok(all) => {
                    for (field, value) in all {
                        let (Some((net, pk)), Some((generation, until))) = (
                            field.split_once('|'),
                            value.split_once('|').and_then(|(g, u)| {
                                Some((g.parse::<u32>().ok()?, u.parse::<u64>().ok()?))
                            }),
                        ) else {
                            continue;
                        };
                        raise(
                            &mut merged,
                            net.to_string(),
                            pk.to_string(),
                            generation,
                            until,
                        );
                    }
                }
                Err(e) => {
                    debug!(%e, "relay floors: the shared store is unreadable; pushing this pod's own")
                }
            }
        }
        merged
            .into_iter()
            .filter(|(_, (_, until))| *until >= now)
            .map(|((net, pk), (generation, until))| GenerationFloor {
                net,
                pk,
                generation,
                until,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A floor outlasts every ticket minted below it: a ticket minted at the
    /// commit, or up to the ticket arm's 10 s read after it, verifies until
    /// its `exp` plus the leeway, and the floor holds at least an hour past
    /// that, at `now == until` included.
    #[test]
    fn a_floor_outlasts_every_ticket_minted_below_it() {
        let commit = 1_800_000_000;
        let latest_exp = commit + 10 + DERP_TICKET_TTL.as_secs();
        assert!(floor_until(commit) >= latest_exp + DERP_TICKET_LEEWAY_SECS + 3590);
    }

    /// Floors merge by the higher generation and the later until.
    #[test]
    fn floors_merge_by_the_higher_generation_and_the_later_until() {
        let mut map = FloorMap::new();
        raise(&mut map, "n".into(), "k".into(), 5, 100);
        raise(&mut map, "n".into(), "k".into(), 3, 200);
        raise(&mut map, "n".into(), "k".into(), 7, 50);
        assert_eq!(map[&("n".to_string(), "k".to_string())], (7, 200));
    }

    #[test]
    fn a_relay_takes_floors_when_it_reports_them() {
        assert!(takes_floor_push(
            &serde_json::json!({ "floors": { "held": 0 } })
        ));
        assert!(!takes_floor_push(&serde_json::json!({ "cpus": 1 })));
        assert_eq!(floors_url("http://pop/stats/"), "http://pop/stats/floors");
    }
}
