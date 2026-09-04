// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Background work a module declares; the host schedules it.
//!
//! Today's server runs its startup maintenance (stale-call reset, thread
//! migration, orphan sweeps) behind a 120 s Mongo lease so exactly one pod
//! does it, and its periodic sweeps on plain `tokio::spawn`s. A module
//! declares the same two shapes here and never touches the lease or the
//! runtime itself — the host owns both.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

/// A boxed run of one job.
pub type JobFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

/// When a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Once, at boot, after indexes are ensured and before routes are served.
    AtStartup,
    /// Repeatedly, on the host's scheduler.
    Every(Duration),
}

/// One declared job.
#[derive(Clone)]
pub struct Job {
    /// Stable name, for logs and the health surface.
    pub name: &'static str,
    /// `true` = only the pod holding the startup lease runs it. Anything that
    /// writes shared state at boot must be gated; a per-pod cache warm need not.
    pub leader_gated: bool,
    pub cadence: Cadence,
    pub run: Arc<dyn Fn() -> JobFuture + Send + Sync>,
}

impl Job {
    pub fn at_startup<F, Fut>(name: &'static str, leader_gated: bool, f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        Self {
            name,
            leader_gated,
            cadence: Cadence::AtStartup,
            run: Arc::new(move || Box::pin(f())),
        }
    }

    pub fn every<F, Fut>(name: &'static str, period: Duration, leader_gated: bool, f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        Self {
            name,
            leader_gated,
            cadence: Cadence::Every(period),
            run: Arc::new(move || Box::pin(f())),
        }
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("name", &self.name)
            .field("leader_gated", &self.leader_gated)
            .field("cadence", &self.cadence)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn a_job_runs_its_closure_each_time_it_is_invoked() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let job = Job::every("sweep", Duration::from_secs(60), true, move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        (job.run)().await.unwrap();
        (job.run)().await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(job.cadence, Cadence::Every(Duration::from_secs(60)));
        assert!(job.leader_gated);
    }
}
