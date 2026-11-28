// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Background work a module declares; the host schedules it.
//!
//! The server runs its startup maintenance (stale-call reset, thread
//! migration, orphan sweeps) behind a 120 s Mongo lease so exactly one pod
//! does it. A module declares its work in two shapes here and never touches
//! the lease or the runtime itself — the host owns both: an `AtStartup` job
//! runs once at boot (under that lease when it is leader-gated), and an
//! `Every` job runs on [`PeriodicJobs`], on every pod, from boot until the
//! host stops it at shutdown (#4448). A leader-gated `Every` job is declared
//! and not run: the startup lease is the host's only one, and it lapses
//! 120 s after boot. The host's own older sweeps still run on plain
//! `tokio::spawn`s.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use tokio::{sync::watch, task::JoinHandle};
use tracing::{error, info, warn};

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
    /// Nor need a write that stays right on BOTH counts: beside a live pod's
    /// own writes (the stale-call reset is not: it ends calls live there) and
    /// beside a second run of the job, e.g. each write a compare on the value
    /// it read. Chat's `room-member-count` is one, and it runs on every pod.
    /// The host runs no leader-gated periodic job: it holds no lease past
    /// boot, so [`PeriodicJobs::spawn`] logs such a job and does not run it.
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

/// The periodic jobs running on this process's runtime (#4448).
///
/// Each runs on a task of its own, on this rhythm:
///
/// - its first run comes after [`first_delay`]: between half a period and one
///   and a half after the start, at a point taken from the clock, so the pods
///   of a deployment do not run a job in the same second;
/// - each next run starts one period after the last run ENDED, so a run never
///   overlaps the job's own next run, and a slow run delays the next instead
///   of stacking;
/// - a run that fails is logged at error, and the next one still comes;
/// - [`PeriodicJobs::stop`] ends them all: a job waiting for its next run
///   never starts it, and a run under way is dropped at its next await. A job
///   whose writes are each a compare on what they read (every periodic job the
///   host runs is ungated, so each must be) loses nothing by that.
///
/// Dropping it stops them as well, so the host keeps it for as long as they
/// should run.
#[must_use = "dropping PeriodicJobs stops every job it started"]
pub struct PeriodicJobs {
    stop: watch::Sender<bool>,
    tasks: Vec<(&'static str, JoinHandle<()>)>,
}

impl PeriodicJobs {
    /// Start each `Every` job of `jobs` that is not leader-gated. `AtStartup`
    /// jobs are not this scheduler's; a leader-gated `Every` job is logged and
    /// not run.
    pub fn spawn(jobs: Vec<Job>) -> Self {
        Self::spawn_with(jobs, first_delay)
    }

    /// [`PeriodicJobs::spawn`] with the first delay of a job of a given period
    /// chosen by `first` (tests pass a fixed one).
    pub fn spawn_with(jobs: Vec<Job>, first: impl Fn(Duration) -> Duration) -> Self {
        let (stop, stopped) = watch::channel(false);
        let mut tasks = Vec::new();
        for job in jobs {
            let Cadence::Every(period) = job.cadence else {
                continue;
            };
            if job.leader_gated {
                warn!(
                    job = job.name,
                    "leader-gated periodic jobs need a lease the host does not hold — declared, not run"
                );
                continue;
            }
            let wait = first(period);
            info!(
                job = job.name,
                period_s = period.as_secs(),
                first_s = wait.as_secs(),
                "periodic job scheduled"
            );
            tasks.push((
                job.name,
                tokio::spawn(run_every(job, period, wait, stopped.clone())),
            ));
        }
        Self { stop, tasks }
    }

    /// The names of the jobs it runs, in the order they were declared.
    pub fn names(&self) -> Vec<&'static str> {
        self.tasks.iter().map(|(name, _)| *name).collect()
    }

    /// End every job, and wait until each task has ended.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        for (name, task) in self.tasks {
            if let Err(e) = task.await {
                error!(job = name, %e, "periodic job task ended badly");
            }
        }
    }
}

/// A job's first run comes this long after the scheduler starts: half a period
/// plus a part of one period taken from the clock's nanoseconds.
pub fn first_delay(period: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    first_delay_at(period, nanos)
}

/// [`first_delay`] at a given nanosecond of the clock, `0..1_000_000_000`.
pub fn first_delay_at(period: Duration, nanos: u32) -> Duration {
    let part = period.as_nanos() * u128::from(nanos.min(999_999_999)) / 1_000_000_000;
    period / 2 + Duration::from_nanos(part as u64)
}

/// Resolves once the scheduler is stopped, or dropped.
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

async fn run_every(job: Job, period: Duration, first: Duration, mut rx: watch::Receiver<bool>) {
    let mut wait = first;
    loop {
        tokio::select! {
            biased;
            _ = stopped(&mut rx) => return,
            _ = tokio::time::sleep(wait) => {}
        }
        tokio::select! {
            biased;
            _ = stopped(&mut rx) => return,
            done = (job.run)() => {
                if let Err(e) = done {
                    error!(job = job.name, %e, "periodic job failed");
                }
            }
        }
        wait = period;
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

    use std::sync::Mutex;
    use tokio::time::Instant;

    const MS: Duration = Duration::from_millis(1);

    /// A job of period `period` whose every run takes `takes`, and records
    /// when it started and ended; `fail` makes each run an error.
    fn timed(
        name: &'static str,
        period: Duration,
        takes: Duration,
        fail: bool,
        log: Arc<Mutex<Vec<(Instant, Instant)>>>,
    ) -> Job {
        Job::every(name, period, false, move || {
            let log = log.clone();
            async move {
                let start = Instant::now();
                tokio::time::sleep(takes).await;
                log.lock().unwrap().push((start, Instant::now()));
                if fail {
                    anyhow::bail!("this run fails");
                }
                Ok(())
            }
        })
    }

    #[test]
    fn the_first_run_comes_between_half_and_one_and_a_half_periods() {
        let p = Duration::from_secs(600);
        assert_eq!(first_delay_at(p, 0), Duration::from_secs(300));
        assert_eq!(first_delay_at(p, 500_000_000), Duration::from_secs(600));
        assert!(first_delay_at(p, 999_999_999) < Duration::from_secs(900));
        assert!(first_delay_at(p, u32::MAX) < Duration::from_secs(900));
        let any = first_delay(p);
        assert!(any >= Duration::from_secs(300) && any < Duration::from_secs(900));
    }

    #[tokio::test]
    async fn only_ungated_periodic_jobs_are_scheduled() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let jobs = vec![
            Job::at_startup("boot", false, || async { Ok(()) }),
            timed("every", 60_000 * MS, MS, false, log.clone()),
            Job::every("gated", 60_000 * MS, true, || async { Ok(()) }),
        ];
        let periodic = PeriodicJobs::spawn_with(jobs, |_| Duration::from_secs(3600));
        assert_eq!(periodic.names(), vec!["every"]);
        periodic.stop().await;
    }

    #[tokio::test]
    async fn a_run_starts_one_period_after_the_last_one_ended_and_never_overlaps() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let period = 20 * MS;
        let takes = 30 * MS;
        let periodic = PeriodicJobs::spawn_with(
            vec![timed("slow", period, takes, false, log.clone())],
            |_| Duration::ZERO,
        );
        tokio::time::sleep(300 * MS).await;
        periodic.stop().await;
        let runs = log.lock().unwrap().clone();
        assert!(runs.len() >= 3, "runs: {}", runs.len());
        for pair in runs.windows(2) {
            let (_, ended) = pair[0];
            let (next, _) = pair[1];
            assert!(
                next >= ended + period,
                "a run began {:?} after the last ended",
                next - ended
            );
        }
    }

    #[tokio::test]
    async fn a_failed_run_is_logged_and_the_next_still_comes() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let periodic = PeriodicJobs::spawn_with(
            vec![timed("failing", 10 * MS, MS, true, log.clone())],
            |_| Duration::ZERO,
        );
        tokio::time::sleep(200 * MS).await;
        periodic.stop().await;
        assert!(log.lock().unwrap().len() >= 3);
    }

    #[tokio::test]
    async fn the_first_run_waits_for_its_first_delay() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let began = Instant::now();
        let periodic = PeriodicJobs::spawn_with(
            vec![timed("later", 10 * MS, MS, false, log.clone())],
            |_| 150 * MS,
        );
        tokio::time::sleep(100 * MS).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "it ran before its first delay"
        );
        tokio::time::sleep(200 * MS).await;
        periodic.stop().await;
        let runs = log.lock().unwrap().clone();
        assert!(!runs.is_empty());
        assert!(runs[0].0 >= began + 150 * MS);
    }

    #[tokio::test]
    async fn stop_ends_a_run_under_way_and_starts_no_other() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let periodic = PeriodicJobs::spawn_with(
            vec![timed(
                "long",
                10 * MS,
                Duration::from_secs(30),
                false,
                log.clone(),
            )],
            |_| Duration::ZERO,
        );
        tokio::time::sleep(50 * MS).await;
        let asked = Instant::now();
        tokio::time::timeout(Duration::from_secs(2), periodic.stop())
            .await
            .expect("stop waited for a 30 s run");
        assert!(asked.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(50 * MS).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "the dropped run recorded an end"
        );
    }

    #[tokio::test]
    async fn a_stopped_scheduler_waiting_for_a_run_never_starts_it() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let periodic = PeriodicJobs::spawn_with(
            vec![timed("waiting", 10 * MS, MS, false, log.clone())],
            |_| 100 * MS,
        );
        periodic.stop().await;
        tokio::time::sleep(200 * MS).await;
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dropping_the_scheduler_stops_its_jobs() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let periodic = PeriodicJobs::spawn_with(
            vec![timed("dropped", 10 * MS, MS, false, log.clone())],
            |_| 100 * MS,
        );
        drop(periodic);
        tokio::time::sleep(200 * MS).await;
        assert!(log.lock().unwrap().is_empty());
    }
}
