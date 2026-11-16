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
//! Every timeout, of a command or of a replacement's connection attempt, is
//! logged at warn in one fixed wording, `redis: no answer within the timeout`;
//! a Redis that answers never produces it. Both bounds are settings
//! (`ROOMLER__REDIS__RESPONSE_TIMEOUT_MS`,
//! `ROOMLER__REDIS__CONNECTION_TIMEOUT_MS`), and 0 turns a bound off: with
//! both at 0 the connection is the crate's default manager, as before #3840.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use redis::aio::{ConnectionLike, ConnectionManager, ConnectionManagerConfig};
use redis::{Cmd, Pipeline, RedisError, RedisFuture, RedisResult, Value};
use tracing::{info, warn};

/// The default of `ROOMLER__REDIS__RESPONSE_TIMEOUT_MS`.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_millis(3000);
/// The default of `ROOMLER__REDIS__CONNECTION_TIMEOUT_MS`.
pub const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_millis(3000);
/// A replacement's wait after its first failed attempt; it doubles up to
/// [`REPLACE_WAIT_MAX`].
pub const REPLACE_WAIT_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two replacement attempts.
pub const REPLACE_WAIT_MAX: Duration = Duration::from_secs(5);

/// The one wording of every timeout. A field read counts this line, and on
/// a Redis that answers its count is 0.
pub const TIMEOUT_LINE: &str = "redis: no answer within the timeout";

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

    /// From the settings, in milliseconds; 0 is no bound.
    pub fn from_settings(settings: &roomler_ai_config::RedisSettings) -> Self {
        let bound = |ms: u64| (ms > 0).then(|| Duration::from_millis(ms));
        Self {
            response_timeout: bound(settings.response_timeout_ms),
            connection_timeout: bound(settings.connection_timeout_ms),
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
    timeouts: AtomicU64,
    replacements: AtomicU64,
}

impl Shared {
    /// Count and log one timeout, in the one wording.
    fn timed_out(&self, command: &str, bound: Duration) {
        let n = self.timeouts.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(
            command,
            timeout_ms = bound.as_millis() as u64,
            timeouts = n,
            "{TIMEOUT_LINE}"
        );
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
                timeouts: AtomicU64::new(0),
                replacements: AtomicU64::new(0),
            }),
        })
    }

    pub fn bounds(&self) -> RedisBounds {
        self.shared.bounds
    }

    /// Timeouts logged so far, one per line in the one wording.
    pub fn timeouts(&self) -> u64 {
        self.shared.timeouts.load(Ordering::Relaxed)
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
        if let Err(e) = &result
            && e.is_timeout()
        {
            self.shared.timed_out(&what(), bound);
            self.replace();
        }
        result
    }

    /// Start the replacement unless one is already running. It stops once
    /// nothing holds the connection any more.
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
            let mut wait = REPLACE_WAIT_FIRST;
            loop {
                let attempt =
                    ConnectionManager::new_with_config(client.clone(), bounds.manager_config());
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
                        let n = shared.replacements.fetch_add(1, Ordering::Relaxed) + 1;
                        shared.replacing.store(false, Ordering::Release);
                        info!(
                            replacements = n,
                            "redis: the shared connection was replaced after a timeout"
                        );
                        return;
                    }
                    Ok(Err(e)) if !e.is_timeout() => warn!(
                        error = %e,
                        retry_in_ms = wait.as_millis() as u64,
                        "redis: a replacement connection could not be made"
                    ),
                    _ => shared.timed_out("connect", attempt_bound),
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
