// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3840 — the shared Redis command connection, with a bound on every command.
//!
//! The server sends every Redis command but a subscription through ONE
//! multiplexed connection: the fan-out publisher, the presence registries,
//! the cluster directory, the pod bus and the session slots all hold clones
//! of it. redis 0.27.6's `ConnectionManager`, as built by default, waits for
//! an answer for ever, so a Redis lost without a TCP reset (a dropped path, a
//! frozen machine) left every command in flight hanging for as long as the
//! operating system kept the socket, and every request that awaited one with
//! it. Three things in the crate decide what this module adds:
//!
//! - its response timeout covers the wait for the reply on a connection that
//!   is up, not the wait for the connection while it reconnects, and not the
//!   queue in front of the socket. So every call here runs under one outer
//!   timeout of the same length, and a command answers or fails within
//!   [`RedisBounds::response_timeout`] whatever the connection is doing.
//! - a timed-out command does not make it reconnect (an io `TimedOut` is not
//!   an error it replaces the connection for). A connection whose peer went
//!   silent would time out every command until the kernel gave the socket up:
//!   about 15 minutes with data in flight, never while the peer's kernel
//!   still acknowledges (a frozen process). So a timeout here also starts ONE
//!   background replacement: a fresh connection built like the first, one
//!   attempt at a time under the connection timeout, swapped in once Redis
//!   has answered its handshake. Commands keep using the old one until then.
//! - without a connection timeout one connection attempt to a listener that
//!   accepts and never answers waits for ever, and so does every command that
//!   waits for that reconnect.
//!
//! A timed-out command may still have been applied: the reply was lost, not
//! necessarily the write. What each caller does with the error, and what
//! heals a write applied behind it, is in `docs/multi-pod-scale-out.md`.
//!
//! A Redis that took the bytes and said nothing (a command, or a connection
//! attempt, timed out) is logged at warn in one fixed wording, `redis: no
//! answer within the timeout`; a Redis that refused or closed the connection
//! in another, `redis: no connection could be made`. A Redis that answers
//! produces neither. Each wording writes at most one line a second per pod,
//! and a line says how many were held back since the one before it. Both
//! bounds are settings (`ROOMLER__REDIS__RESPONSE_TIMEOUT_MS`,
//! `ROOMLER__REDIS__CONNECTION_TIMEOUT_MS`): 0 turns a bound off (with both at
//! 0 the connection is the crate's default manager, as before #3840), and a
//! value from 1 to 999 is used as 1000, with a warn line at boot.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use redis::aio::{ConnectionLike, ConnectionManager, ConnectionManagerConfig};
use redis::{AsyncConnectionConfig, Cmd, Pipeline, RedisError, RedisFuture, RedisResult, Value};
use tracing::{info, warn};

/// The default of `ROOMLER__REDIS__RESPONSE_TIMEOUT_MS`.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_millis(3000);
/// The default of `ROOMLER__REDIS__CONNECTION_TIMEOUT_MS`.
pub const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_millis(3000);
/// The least a bound setting is used as, in ms. A value from 1 to 999 (3,
/// meant as seconds) would fail commands on a Redis that answers, so it is
/// used as this, with a warn line at boot.
pub const BOUND_FLOOR_MS: u64 = 1000;
/// A replacement's wait after its first failed attempt; it doubles up to
/// [`REPLACE_WAIT_MAX`].
pub const REPLACE_WAIT_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two replacement attempts.
pub const REPLACE_WAIT_MAX: Duration = Duration::from_secs(5);

/// The one wording of every timeout. A field read counts this line, and on
/// a Redis that answers its count is 0.
pub const TIMEOUT_LINE: &str = "redis: no answer within the timeout";

/// The wording of a Redis that refused or closed the connection, or of a
/// connection attempt that failed for another reason than a timeout: a Redis
/// that is down, as opposed to one that took the bytes and said nothing. On a
/// Redis that answers its count is 0 too.
pub const CONNECT_LINE: &str = "redis: no connection could be made";

/// The two bounds. `None` is no bound, the crate's own default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedisBounds {
    /// A command, or a pipeline, with no answer within this fails with an io
    /// `TimedOut` error.
    pub response_timeout: Option<Duration>,
    /// One connection attempt (TCP connect and the handshake) gives up after
    /// this.
    pub connection_timeout: Option<Duration>,
}

impl Default for RedisBounds {
    fn default() -> Self {
        Self {
            response_timeout: Some(DEFAULT_RESPONSE_TIMEOUT),
            connection_timeout: Some(DEFAULT_CONNECTION_TIMEOUT),
        }
    }
}

impl RedisBounds {
    /// No bound at all: the connection as it was before #3840.
    pub const NONE: Self = Self {
        response_timeout: None,
        connection_timeout: None,
    };

    /// From the settings, in milliseconds: 0 is no bound, and a value under
    /// [`BOUND_FLOOR_MS`] is used as it, with a warn line.
    pub fn from_settings(settings: &roomler_ai_config::RedisSettings) -> Self {
        Self {
            response_timeout: bound_setting(RESPONSE_SETTING, settings.response_timeout_ms),
            connection_timeout: bound_setting(CONNECTION_SETTING, settings.connection_timeout_ms),
        }
    }

    /// The crate's config with these bounds. Its reconnect schedule (after a
    /// reset or a refused connection) stays the crate's default, as before.
    fn manager_config(&self) -> ConnectionManagerConfig {
        let mut config = ConnectionManagerConfig::new();
        if let Some(t) = self.response_timeout {
            config = config.set_response_timeout(t);
        }
        if let Some(t) = self.connection_timeout {
            config = config.set_connection_timeout(t);
        }
        config
    }

    /// One plain connection with these bounds: a replacement's first step.
    fn probe_config(&self) -> AsyncConnectionConfig {
        let mut config = AsyncConnectionConfig::new();
        if let Some(t) = self.response_timeout {
            config = config.set_response_timeout(t);
        }
        if let Some(t) = self.connection_timeout {
            config = config.set_connection_timeout(t);
        }
        config
    }
}

/// The shared command connection. Cheap to clone; every clone sees the
/// replacement.
#[derive(Clone)]
pub struct RedisConnection {
    shared: Arc<Shared>,
}

struct Shared {
    client: redis::Client,
    bounds: RedisBounds,
    db: i64,
    /// Read and cloned per command, written once per replacement; a std lock,
    /// never held across an await.
    current: RwLock<ConnectionManager>,
    replacing: AtomicBool,
    /// The last word on the connection was a refusal or a close (an io error
    /// that was no timeout): a call that times out now waited for the crate's
    /// reconnect, and is logged as no connection. An answer, a swap, or a
    /// connection attempt that timed out clears it.
    down: AtomicBool,
    timeouts: AtomicU64,
    connect_failures: AtomicU64,
    /// One line a second for each wording.
    timeout_lines: LineBound,
    connect_lines: LineBound,
    replacements: AtomicU64,
}

impl Shared {
    fn set_down(&self, down: bool) {
        // A load first: almost every call finds it already so, and a store
        // from every call would have every core write the same cache line.
        if self.down.load(Ordering::Relaxed) != down {
            self.down.store(down, Ordering::Relaxed);
        }
    }

    /// One call with no answer within `bound`: no connection while the
    /// connection is down, otherwise no answer.
    fn timed_out(&self, what: impl FnOnce() -> String, bound: Duration) {
        if !self.down.load(Ordering::Relaxed) {
            self.no_answer(what, bound);
            return;
        }
        let n = self.connect_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(suppressed) = self.connect_lines.admit() {
            warn!(
                command = what(),
                waited_ms = bound.as_millis() as u64,
                connect_failures = n,
                suppressed,
                "{CONNECT_LINE}"
            );
        }
    }

    /// Count one call or connection attempt with no answer within `bound`,
    /// and log it in the one wording, one line a second at most.
    fn no_answer(&self, what: impl FnOnce() -> String, bound: Duration) {
        let n = self.timeouts.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(suppressed) = self.timeout_lines.admit() {
            warn!(
                command = what(),
                timeout_ms = bound.as_millis() as u64,
                timeouts = n,
                suppressed,
                "{TIMEOUT_LINE}"
            );
        }
    }

    /// A connection attempt that failed for another reason than a timeout:
    /// refused, closed, or answered with an error in the handshake.
    fn not_connected(&self, error: &RedisError, retry_in: Duration) {
        self.set_down(true);
        let n = self.connect_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(suppressed) = self.connect_lines.admit() {
            warn!(
                error = %error,
                retry_in_ms = retry_in.as_millis() as u64,
                connect_failures = n,
                suppressed,
                "{CONNECT_LINE}"
            );
        }
    }
}

impl RedisConnection {
    /// Connect with these bounds. Without any bound this is exactly
    /// `ConnectionManager::new`.
    pub async fn connect(client: redis::Client, bounds: RedisBounds) -> RedisResult<Self> {
        let manager =
            ConnectionManager::new_with_config(client.clone(), bounds.manager_config()).await?;
        let db = client.get_connection_info().redis.db;
        Ok(Self {
            shared: Arc::new(Shared {
                client,
                bounds,
                db,
                current: RwLock::new(manager),
                replacing: AtomicBool::new(false),
                down: AtomicBool::new(false),
                timeouts: AtomicU64::new(0),
                connect_failures: AtomicU64::new(0),
                timeout_lines: LineBound::per_second(),
                connect_lines: LineBound::per_second(),
                replacements: AtomicU64::new(0),
            }),
        })
    }

    pub fn bounds(&self) -> RedisBounds {
        self.shared.bounds
    }

    /// Calls and connection attempts with no answer within their bound
    /// ([`TIMEOUT_LINE`]), each counted whether its line was written or held
    /// back.
    pub fn timeouts(&self) -> u64 {
        self.shared.timeouts.load(Ordering::Relaxed)
    }

    /// Connection attempts that failed for another reason than a timeout, and
    /// calls that timed out while the connection was down ([`CONNECT_LINE`]),
    /// each counted whether its line was written or held back.
    pub fn connect_failures(&self) -> u64 {
        self.shared.connect_failures.load(Ordering::Relaxed)
    }

    /// Times the connection was replaced after a timeout.
    pub fn replacements(&self) -> u64 {
        self.shared.replacements.load(Ordering::Relaxed)
    }

    fn manager(&self) -> ConnectionManager {
        self.shared
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Run one call under the response timeout. `what` names it in the log,
    /// and is read only when the bound fires.
    async fn bounded<T>(
        &self,
        what: impl FnOnce() -> String,
        call: impl Future<Output = RedisResult<T>>,
    ) -> RedisResult<T> {
        let Some(bound) = self.shared.bounds.response_timeout else {
            return call.await;
        };
        let result = match tokio::time::timeout(bound, call).await {
            Ok(result) => result,
            Err(_) => Err(RedisError::from(std::io::Error::from(
                std::io::ErrorKind::TimedOut,
            ))),
        };
        match &result {
            Err(e) if e.is_timeout() => {
                self.shared.timed_out(what, bound);
                self.replace();
            }
            // Refused or closed: the crate reconnects, and a call that waits
            // for that reconnect has no connection, not no answer.
            Err(e) if e.is_io_error() => self.shared.set_down(true),
            // A reply, an error reply included: Redis answered.
            _ => self.shared.set_down(false),
        }
        result
    }

    /// Start the replacement unless one is already running. It stops once
    /// nothing holds the connection any more. Each attempt first makes one
    /// plain connection, so a Redis that refuses fails the attempt at once:
    /// the crate's own retries inside an attempt would wait 1 to 2 s, then 60
    /// to 120 s, and the attempt would end at its bound as a timeout.
    fn replace(&self) {
        if self.shared.replacing.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(&self.shared);
        let client = self.shared.client.clone();
        let bounds = self.shared.bounds;
        tokio::spawn(async move {
            // One attempt is bounded even with no connection timeout set.
            let attempt_bound = bounds
                .connection_timeout
                .or(bounds.response_timeout)
                .unwrap_or(DEFAULT_CONNECTION_TIMEOUT);
            let probe = bounds.probe_config();
            let mut wait = REPLACE_WAIT_FIRST;
            loop {
                let attempt = async {
                    client
                        .get_multiplexed_async_connection_with_config(&probe)
                        .await?;
                    ConnectionManager::new_with_config(client.clone(), bounds.manager_config())
                        .await
                };
                let outcome = tokio::time::timeout(attempt_bound, attempt).await;
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                match outcome {
                    Ok(Ok(manager)) => {
                        *shared
                            .current
                            .write()
                            .unwrap_or_else(PoisonError::into_inner) = manager;
                        shared.set_down(false);
                        let n = shared.replacements.fetch_add(1, Ordering::Relaxed) + 1;
                        shared.replacing.store(false, Ordering::Release);
                        info!(
                            replacements = n,
                            "redis: the shared connection was replaced after a timeout"
                        );
                        return;
                    }
                    Ok(Err(e)) if !e.is_timeout() => shared.not_connected(&e, wait),
                    _ => {
                        // Taken and not answered: a silent Redis, not a down one.
                        shared.set_down(false);
                        shared.no_answer(|| "connect".to_string(), attempt_bound);
                    }
                }
                drop(shared);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(REPLACE_WAIT_MAX);
            }
        });
    }
}

/// The command's name for the log: its first argument.
fn command_name(cmd: &Cmd) -> String {
    match cmd.args_iter().next() {
        Some(redis::Arg::Simple(name)) => String::from_utf8_lossy(name).into_owned(),
        _ => "?".to_string(),
    }
}

impl ConnectionLike for RedisConnection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        Box::pin(async move {
            let mut manager = self.manager();
            self.bounded(|| command_name(cmd), manager.send_packed_command(cmd))
                .await
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let mut manager = self.manager();
            self.bounded(
                || format!("pipeline of {count}"),
                manager.send_packed_commands(cmd, offset, count),
            )
            .await
        })
    }

    fn get_db(&self) -> i64 {
        self.shared.db
    }
}

/// At most one log line a period for one wording, each with how many lines
/// were held back since the one before it. A field read counts lines, so an
/// outage of an hour writes one line a second per pod at most, and
/// `suppressed` says how many each stands for.
#[derive(Debug)]
pub struct LineBound {
    /// When the last line was let through, in [`now_ms`]; 0 is never.
    last: AtomicU64,
    held: AtomicU64,
    every_ms: u64,
}

impl LineBound {
    /// One line a second.
    pub const fn per_second() -> Self {
        Self {
            last: AtomicU64::new(0),
            held: AtomicU64::new(0),
            every_ms: 1000,
        }
    }

    /// `Some(n)` when a line may be written now, `n` the lines held back
    /// since the last one written; `None` holds this one back.
    pub fn admit(&self) -> Option<u64> {
        self.admit_at(now_ms())
    }

    fn admit_at(&self, now: u64) -> Option<u64> {
        let last = self.last.load(Ordering::Acquire);
        let recent = last != 0 && now < last.saturating_add(self.every_ms);
        if recent
            || self
                .last
                .compare_exchange(last, now.max(1), Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            self.held.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(self.held.swap(0, Ordering::AcqRel))
    }
}

/// Milliseconds since the first call in this process, from 1.
fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// The two settings by the names a deployment sets them with.
const RESPONSE_SETTING: &str = "ROOMLER__REDIS__RESPONSE_TIMEOUT_MS";
const CONNECTION_SETTING: &str = "ROOMLER__REDIS__CONNECTION_TIMEOUT_MS";

/// A bound setting in milliseconds as it is used: 0 is no bound, a value
/// under [`BOUND_FLOOR_MS`] is used as it, any other as given.
pub fn bound_ms_used(given: u64) -> Option<u64> {
    (given > 0).then_some(given.max(BOUND_FLOOR_MS))
}

/// A bound in milliseconds as the settings write it: 0 is no bound.
pub fn bound_ms(bound: Option<Duration>) -> u64 {
    bound.map_or(0, |b| b.as_millis() as u64)
}

/// One bound setting as it is used, with one warn line when it was raised.
fn bound_setting(setting: &str, given: u64) -> Option<Duration> {
    let used = bound_ms_used(given);
    if let Some(used) = used.filter(|&used| used != given) {
        warn!(
            setting,
            given_ms = given,
            used_ms = used,
            "redis: a timeout setting under {BOUND_FLOOR_MS} ms is used as {BOUND_FLOOR_MS} ms"
        );
    }
    used.map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value under 1 s (3, meant as seconds) would put every Redis user
    /// into "Redis down" on a Redis that answers: it is used as 1 s. 0 stays
    /// no bound, and 1 s and over are used as given.
    #[test]
    fn a_bound_under_1_s_is_used_as_1_s_and_0_stays_no_bound() {
        let used: Vec<_> = [0, 1, 3, 999, 1000, 1001, 3000, 60_000]
            .into_iter()
            .map(|given| (given, bound_ms_used(given)))
            .collect();
        assert_eq!(
            used,
            [
                (0, None),
                (1, Some(1000)),
                (3, Some(1000)),
                (999, Some(1000)),
                (1000, Some(1000)),
                (1001, Some(1001)),
                (3000, Some(3000)),
                (60_000, Some(60_000)),
            ],
            "W14-RED a bound under 1 s is used as given"
        );
        let settings = |response_timeout_ms, connection_timeout_ms| {
            RedisBounds::from_settings(&roomler_ai_config::RedisSettings {
                url: "redis://r:6379".to_string(),
                response_timeout_ms,
                connection_timeout_ms,
            })
        };
        assert_eq!(
            settings(3, 0),
            RedisBounds {
                response_timeout: Some(Duration::from_secs(1)),
                connection_timeout: None,
            }
        );
        assert_eq!(settings(3000, 3000), RedisBounds::default());
        assert_eq!(settings(0, 0), RedisBounds::NONE);
    }

    #[test]
    fn one_line_a_second_each_with_the_count_held_back() {
        let bound = LineBound::per_second();
        assert_eq!(bound.admit_at(5), Some(0), "the first line is written");
        assert_eq!(bound.admit_at(6), None);
        assert_eq!(bound.admit_at(1004), None, "still within the second");
        assert_eq!(
            bound.admit_at(1005),
            Some(2),
            "a second on: written, 2 held"
        );
        assert_eq!(bound.admit_at(1006), None);
        assert_eq!(bound.admit_at(60_000), Some(1), "a quiet minute later");
        assert_eq!(bound.admit_at(61_000), Some(0), "nothing held since");
    }

    /// 8 threads ask 1000 times each within one second: one line is written,
    /// and every one of the 8000 is either written or counted as held back.
    #[test]
    fn lines_on_many_threads_are_all_counted() {
        let bound = Arc::new(LineBound::per_second());
        let (lines, counted) = (0..8)
            .map(|_| {
                let bound = bound.clone();
                std::thread::spawn(move || {
                    (0..1000)
                        .filter_map(|_| bound.admit_at(7))
                        .fold((0u64, 0u64), |(n, c), held| (n + 1, c + held + 1))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .fold((0, 0), |(n, c), (tn, tc)| (n + tn, c + tc));
        assert_eq!(lines, 1, "one line within the second");
        let rest = bound
            .admit_at(2000)
            .expect("a second on, a line is written");
        assert_eq!(counted + rest, 8000, "every line is written or counted");
    }
}
