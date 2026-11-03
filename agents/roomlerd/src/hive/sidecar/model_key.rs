// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3362 — the provider's key, from the device's helper, kept for
//! [`KEY_TTL`](super::KEY_TTL).
//!
//! A scaffold: master's behaviour in the fix's shape, so that the tests
//! compile against it. One async lock holds the key, and a call holds it
//! across the helper's run.

use std::future::Future;
use std::time::{Duration, Instant};

/// What one run of the helper came to: the key it printed, or why not.
pub(crate) type Fetched = Result<String, String>;

/// The device's provider key, as the helper last printed it.
pub(crate) struct ModelKey {
    kept: tokio::sync::Mutex<Option<(String, Instant)>>,
    ttl: Duration,
}

impl Default for ModelKey {
    fn default() -> Self {
        Self::with_ttl(super::KEY_TTL)
    }
}

impl ModelKey {
    /// A key kept for `ttl`: [`KEY_TTL`](super::KEY_TTL), or shorter in a
    /// test.
    pub(crate) fn with_ttl(ttl: Duration) -> Self {
        Self {
            kept: tokio::sync::Mutex::new(None),
            ttl,
        }
    }

    /// The key: the one kept, while it is younger than the TTL; otherwise
    /// what `run()` fetches.
    pub(crate) async fn get<F, Fut>(&self, run: F) -> Fetched
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Fetched> + Send + 'static,
    {
        let mut cached = self.kept.lock().await;
        if let Some((key, at)) = cached.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(key.clone());
        }
        let key = run().await?;
        *cached = Some((key.clone(), Instant::now()));
        Ok(key)
    }

    /// The provider refused the key: the next call runs the helper again.
    pub(crate) async fn forget(&self) {
        *self.kept.lock().await = None;
    }
}

#[cfg(test)]
mod tests;
