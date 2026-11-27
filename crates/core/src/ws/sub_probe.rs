// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3952 — every Redis subscriber of the server learns that its own
//! connection went deaf, and subscribes again on a fresh one.
//!
//! A subscribed connection is asked nothing. redis 0.27's `PubSub` has no
//! ping and no timeout, a PING on it would end its stream, and TCP keepalive
//! is blind while the far kernel still ACKs (a frozen Redis, a proxy that
//! lost its backend). So each subscriber proves its own connection end to
//! end. On the SAME connection, after its real channels, it subscribes to a
//! probe channel of its own, `roomler:sub-probe:<subscriber>:<nonce>`, with a
//! fresh nonce each lifetime, so a dead registration is never counted as a
//! receiver. Every interval it PUBLISHes a sequence number there over the
//! bounded command connection (#3840). Redis queues a message to its
//! subscribers before it answers the PUBLISH, so a healthy subscriber hears
//! its probe within network time of the reply.
//!
//! [`Verdict`] judges. A PUBLISH that fails or gets no answer is unknown: it
//! neither counts nor resets. A reply of 0 receivers is a miss, and so is a
//! reply of 1 or more with nothing heard by the next tick. Any probe heard
//! resets the count, and [`DEAF_AFTER`] misses in a row are deaf. A deaf
//! subscription is dropped, which closes its socket and ends its driver
//! (redis 0.27 aborts a `PubSub`'s driver task when its stream is dropped),
//! and the subscriber's own loop subscribes again: break-before-make, so
//! nothing is delivered twice.
//!
//! Every subscribe is bounded too: the connect (TCP and the handshake) by
//! #3840's connection timeout, each SUBSCRIBE by its response timeout. A
//! listener that never answers costs a bounded wait and a retry, never a
//! subscriber parked for as long as the socket lives.
//!
//! The setting: `ROOMLER__REDIS__SUBSCRIBER_PROBE_MS`, how often a subscriber
//! publishes a probe to itself. 0 turns the probe off, and a value from 1 to
//! 999 is used as 1000, with a warn line at boot.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use redis::aio::PubSub;
use redis::{Msg, RedisError, RedisResult};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::redis_conn::{BOUND_FLOOR_MS, RedisBounds, RedisConnection, bound_ms, bound_ms_used};

/// The default of `ROOMLER__REDIS__SUBSCRIBER_PROBE_MS`.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_millis(15_000);

/// Misses in a row that make a subscriber deaf. One late arrival, or one
/// stall of the runtime, costs at most one.
pub const DEAF_AFTER: u32 = 2;

/// How often each subscriber writes its running probe totals.
pub const TOTALS_EVERY: Duration = Duration::from_secs(3600);

/// The front of every probe channel. It is under `roomler:`, so an ACL that
/// admits the server's own channels admits it.
pub const PROBE_CHANNEL_PREFIX: &str = "roomler:sub-probe:";

/// The verdict, one WARN line a deaf lifetime. A field read counts it, and
/// on a Redis that answers its count is 0.
pub const DEAF_LINE: &str = "redis: a subscriber stopped hearing its probe; subscribing again";

/// Redis refused the probe channel (`NOPERM`: an ACL that does not admit
/// it): one WARN line, and the rest of that lifetime runs unprobed.
pub const REFUSED_LINE: &str =
    "redis: a subscriber's probe channel was refused; this subscription runs unprobed";

/// One missed probe, at DEBUG.
pub const MISS_LINE: &str = "redis: a subscriber probe went unheard";

/// Each subscriber's running totals, at INFO, every [`TOTALS_EVERY`].
pub const TOTALS_LINE: &str = "redis: subscriber probe totals";

/// The setting by the name a deployment sets it with.
const PROBE_SETTING: &str = "ROOMLER__REDIS__SUBSCRIBER_PROBE_MS";

/// How a subscriber probes its own connection. Built once at boot and handed
/// to every subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSettings {
    /// A probe every this long; `None` is no probe.
    pub interval: Option<Duration>,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            interval: Some(DEFAULT_PROBE_INTERVAL),
        }
    }
}

impl ProbeSettings {
    /// No probe: a subscriber hears what its connection delivers, as before
    /// #3952.
    pub const OFF: Self = Self { interval: None };

    /// From the settings, in milliseconds: 0 is off, and a value under
    /// [`BOUND_FLOOR_MS`] is used as it, with a warn line.
    pub fn from_settings(settings: &roomler_ai_config::RedisSettings) -> Self {
        let given = settings.subscriber_probe_ms;
        let used = bound_ms_used(given);
        if let Some(used) = used.filter(|&used| used != given) {
            warn!(
                setting = PROBE_SETTING,
                given_ms = given,
                used_ms = used,
                "redis: a subscriber probe interval under {BOUND_FLOOR_MS} ms is used as {BOUND_FLOOR_MS} ms"
            );
        }
        Self {
            interval: used.map(Duration::from_millis),
        }
    }

    /// The interval as the settings write it: 0 is off.
    pub fn interval_ms(&self) -> u64 {
        bound_ms(self.interval)
    }
}

/// The server's three subscribers, by the name their lines carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subscriber {
    /// The fan-out, `roomler:ws`.
    Fanout,
    /// The pod bus, `roomler:pod:<pod_id>`.
    Bus,
    /// TURN usage, coturn's statistics.
    Turn,
}

impl Subscriber {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fanout => "fanout",
            Self::Bus => "bus",
            Self::Turn => "turn",
        }
    }

    /// This subscriber's totals since boot (`crate::cluster::metrics`).
    pub fn counters(self) -> &'static ProbeCounters {
        use crate::cluster::metrics::{SUB_PROBE_BUS, SUB_PROBE_FANOUT, SUB_PROBE_TURN};
        match self {
            Self::Fanout => &SUB_PROBE_FANOUT,
            Self::Bus => &SUB_PROBE_BUS,
            Self::Turn => &SUB_PROBE_TURN,
        }
    }
}

/// One subscriber's probe totals since boot, across all its lifetimes. The
/// hourly line and `/api/cluster/status` read them.
#[derive(Debug, Default)]
pub struct ProbeCounters {
    sent: AtomicU64,
    heard: AtomicU64,
    missed: AtomicU64,
    unknown: AtomicU64,
    deaf: AtomicU64,
}

/// The totals at one moment. On a Redis that answers, `heard` keeps up with
/// `sent` (one may be in flight), and `missed` and `deaf` stay 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct ProbeTotals {
    /// Probes published.
    pub sent: u64,
    /// Probes heard back on the subscriber's own connection.
    pub heard: u64,
    /// Probes Redis took but the subscriber never heard.
    pub missed: u64,
    /// Probes whose PUBLISH failed, got no answer or was refused.
    pub unknown: u64,
    /// Deaf verdicts, each one a new subscription.
    pub deaf: u64,
}

impl ProbeCounters {
    pub const fn new() -> Self {
        Self {
            sent: AtomicU64::new(0),
            heard: AtomicU64::new(0),
            missed: AtomicU64::new(0),
            unknown: AtomicU64::new(0),
            deaf: AtomicU64::new(0),
        }
    }

    pub fn totals(&self) -> ProbeTotals {
        ProbeTotals {
            sent: self.sent.load(Ordering::Relaxed),
            heard: self.heard.load(Ordering::Relaxed),
            missed: self.missed.load(Ordering::Relaxed),
            unknown: self.unknown.load(Ordering::Relaxed),
            deaf: self.deaf.load(Ordering::Relaxed),
        }
    }

    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// `fut` within `bound`. `None` waits as long as it takes, as before #3840.
/// An elapsed bound fails with an io `TimedOut` error that names the step,
/// so a subscriber's own `subscribe failed` line says what got no answer.
pub async fn within<T>(
    bound: Option<Duration>,
    step: impl FnOnce() -> String,
    fut: impl Future<Output = RedisResult<T>>,
) -> RedisResult<T> {
    let Some(bound) = bound else {
        return fut.await;
    };
    match tokio::time::timeout(bound, fut).await {
        Ok(result) => result,
        Err(_) => Err(RedisError::from(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no answer within {} ms ({})", bound.as_millis(), step()),
        ))),
    }
}

/// Connect and subscribe within `bounds`: the connect (TCP and the
/// handshake) within the connection timeout, each SUBSCRIBE and PSUBSCRIBE
/// within the response timeout. Without a bound that step waits as before.
pub async fn subscribe_bounded(
    client: &redis::Client,
    bounds: RedisBounds,
    channels: &[&str],
    patterns: &[&str],
) -> RedisResult<PubSub> {
    let mut pubsub = within(
        bounds.connection_timeout,
        || "connect".to_string(),
        client.get_async_pubsub(),
    )
    .await?;
    for channel in channels {
        within(
            bounds.response_timeout,
            || format!("SUBSCRIBE {channel}"),
            pubsub.subscribe(*channel),
        )
        .await?;
    }
    for pattern in patterns {
        within(
            bounds.response_timeout,
            || format!("PSUBSCRIBE {pattern}"),
            pubsub.psubscribe(*pattern),
        )
        .await?;
    }
    Ok(pubsub)
}

/// A fresh probe channel for one lifetime of `subscriber`.
pub fn probe_channel(subscriber: Subscriber) -> String {
    format!(
        "{PROBE_CHANNEL_PREFIX}{}:{}",
        subscriber.name(),
        uuid::Uuid::new_v4().simple()
    )
}

/// One lifetime's connection, subscribed, with its probe channel when the
/// lifetime is probed.
pub struct Subscription {
    pubsub: PubSub,
    probe: Option<String>,
}

impl Subscription {
    /// Whether this lifetime publishes probes to itself.
    pub fn probed(&self) -> bool {
        self.probe.is_some()
    }
}

/// Subscribe within `bounds` ([`subscribe_bounded`]), then, with the probe
/// on, to a fresh probe channel on the same connection, within the response
/// timeout as well.
///
/// The probe channel's SUBSCRIBE with no answer, or on a connection that
/// failed, fails the whole subscription: the connection did not answer the
/// very next thing it was asked, and a lifetime run unprobed on it could be
/// deaf from birth with nothing to notice. An error reply (Redis answered,
/// refusing) leaves the real subscription standing, unprobed, with one warn
/// line. redis 0.27's `PubSub` reports such a reply as success, so the
/// refusal shows on the probe's PUBLISH instead ([`Reply::Refused`]).
pub async fn subscribe_probed(
    client: &redis::Client,
    bounds: RedisBounds,
    settings: ProbeSettings,
    subscriber: Subscriber,
    channels: &[&str],
    patterns: &[&str],
) -> RedisResult<Subscription> {
    let mut pubsub = subscribe_bounded(client, bounds, channels, patterns).await?;
    if settings.interval.is_none() {
        return Ok(Subscription {
            pubsub,
            probe: None,
        });
    }
    let channel = probe_channel(subscriber);
    let probe = match within(
        bounds.response_timeout,
        || format!("SUBSCRIBE {channel}"),
        pubsub.subscribe(&channel),
    )
    .await
    {
        Ok(()) => Some(channel),
        Err(e) if e.is_io_error() => return Err(e),
        Err(e) => {
            warn!(subscriber = subscriber.name(), error = %e, "{REFUSED_LINE}");
            None
        }
    };
    Ok(Subscription { pubsub, probe })
}

/// How a lifetime ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// The stream ended: Redis or the network closed the connection.
    Ended,
    /// The probe went unheard [`DEAF_AFTER`] times in a row. Its line is
    /// written already.
    Deaf,
}

/// The one PUBLISH of a probe that may be out.
type InFlight = Pin<Box<dyn Future<Output = RedisResult<i64>> + Send>>;

/// One subscriber's probing across its lifetimes. The subscriber's task
/// builds it once, then subscribes and runs each lifetime through it.
pub struct Prober {
    subscriber: Subscriber,
    publisher: RedisConnection,
    settings: ProbeSettings,
    /// The hourly totals line; `None` with the probe off. It runs across
    /// lifetimes, so a new subscription does not restart its hour.
    totals: Option<Interval>,
}

impl Prober {
    /// Build it inside the runtime: it starts the hourly clock.
    pub fn new(
        subscriber: Subscriber,
        publisher: RedisConnection,
        settings: ProbeSettings,
    ) -> Self {
        let totals = settings.interval.map(|_| {
            let mut every = tokio::time::interval_at(Instant::now() + TOTALS_EVERY, TOTALS_EVERY);
            every.set_missed_tick_behavior(MissedTickBehavior::Delay);
            every
        });
        Self {
            subscriber,
            publisher,
            settings,
            totals,
        }
    }

    /// The interval as the settings write it: 0 is off.
    pub fn interval_ms(&self) -> u64 {
        self.settings.interval_ms()
    }

    /// [`subscribe_probed`] within the command connection's bounds.
    pub async fn subscribe(
        &self,
        client: &redis::Client,
        channels: &[&str],
        patterns: &[&str],
    ) -> RedisResult<Subscription> {
        subscribe_probed(
            client,
            self.publisher.bounds(),
            self.settings,
            self.subscriber,
            channels,
            patterns,
        )
        .await
    }

    /// Run one lifetime: hand every message but a probe to `on_message`
    /// until the stream ends or the probe finds the connection deaf. Nothing
    /// here waits on anything but the stream, the clock and the one probe
    /// out, so `on_message` must not block: it is called inline.
    ///
    /// A probe is matched by its channel before anything else sees it, so
    /// no subscriber's own handling ever meets one. The stream is polled
    /// first (`biased`): a backlog drains before a tick is judged.
    pub async fn run(
        &mut self,
        subscription: Subscription,
        mut on_message: impl FnMut(Msg),
    ) -> Lifetime {
        let Subscription { mut pubsub, probe } = subscription;
        let Self {
            subscriber,
            publisher,
            settings,
            totals,
        } = self;
        let name = subscriber.name();
        let counters = subscriber.counters();
        let probe_ms = settings.interval_ms();
        let mut tick = probe.as_ref().and(settings.interval).map(|every| {
            let mut tick = tokio::time::interval_at(Instant::now() + every, every);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            tick
        });
        let mut verdict = Verdict::default();
        let mut in_flight: Option<InFlight> = None;
        let mut stream = pubsub.on_message();
        loop {
            tokio::select! {
                biased;
                msg = stream.next() => {
                    let Some(msg) = msg else {
                        return Lifetime::Ended;
                    };
                    if probe.as_deref() == Some(msg.get_channel_name()) {
                        verdict.heard();
                        ProbeCounters::bump(&counters.heard);
                    } else {
                        on_message(msg);
                    }
                }
                result = async { in_flight.as_mut().expect("polled only with a probe out").await },
                    if in_flight.is_some() =>
                {
                    in_flight = None;
                    let reply = Reply::of(&result);
                    if !matches!(reply, Reply::Receivers(_)) {
                        ProbeCounters::bump(&counters.unknown);
                    }
                    if let (Reply::Refused, Err(e)) = (reply, &result) {
                        // An ACL refused the probe channel: the rest of this
                        // lifetime runs unprobed, so a refusal that stays is
                        // said once and asked no more.
                        warn!(subscriber = name, error = %e, "{REFUSED_LINE}");
                        tick = None;
                        continue;
                    }
                    if judged(verdict.replied(reply), &verdict, name, counters, probe_ms) {
                        return Lifetime::Deaf;
                    }
                }
                _ = async { tick.as_mut().expect("polled only while probed").tick().await },
                    if tick.is_some() =>
                {
                    let step = verdict.tick();
                    if judged(step, &verdict, name, counters, probe_ms) {
                        return Lifetime::Deaf;
                    }
                    if let (Action::Send(seq), Some(channel)) = (step.action, probe.as_deref()) {
                        ProbeCounters::bump(&counters.sent);
                        in_flight = Some(publish(publisher, channel, seq));
                    }
                }
                _ = async { totals.as_mut().expect("polled only with the probe on").tick().await },
                    if totals.is_some() =>
                {
                    let t = counters.totals();
                    info!(
                        subscriber = name,
                        probe_ms,
                        sent = t.sent,
                        heard = t.heard,
                        missed = t.missed,
                        unknown = t.unknown,
                        deaf = t.deaf,
                        "{TOTALS_LINE}"
                    );
                }
            }
        }
    }
}

/// PUBLISH probe number `seq` over the command connection, which bounds it
/// by the response timeout (#3840).
fn publish(publisher: &RedisConnection, channel: &str, seq: u64) -> InFlight {
    let mut conn = publisher.clone();
    let channel = channel.to_string();
    Box::pin(async move {
        redis::cmd("PUBLISH")
            .arg(&channel)
            .arg(seq)
            .query_async::<i64>(&mut conn)
            .await
    })
}

/// Count and log what a step of the verdict found; true when it is deaf.
fn judged(
    step: Step,
    verdict: &Verdict,
    name: &'static str,
    counters: &ProbeCounters,
    probe_ms: u64,
) -> bool {
    if step.missed {
        ProbeCounters::bump(&counters.missed);
        debug!(
            subscriber = name,
            seq = verdict.seq,
            receivers = verdict.receivers,
            missed = verdict.misses,
            "{MISS_LINE}"
        );
    }
    if step.action != Action::Deaf {
        return false;
    }
    ProbeCounters::bump(&counters.deaf);
    warn!(
        subscriber = name,
        missed = verdict.misses,
        receivers = verdict.receivers,
        probe_ms,
        "{DEAF_LINE}"
    );
    true
}

/// A probe PUBLISH's result, as the verdict reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Redis took it and counted this many receivers.
    Receivers(i64),
    /// No answer within the bound, a connection that failed or was being
    /// replaced, or an error reply that passes (a script holding the server
    /// answers `BUSY`): it says nothing about the subscriber.
    Unknown,
    /// Redis refused the probe channel with `NOPERM`, as an ACL that does not
    /// admit it does. It says nothing about the subscriber either, and as it
    /// does not pass, the rest of the lifetime runs unprobed.
    Refused,
}

impl Reply {
    pub fn of(result: &RedisResult<i64>) -> Self {
        match result {
            Ok(n) => Self::Receivers(*n),
            // Only an ACL's refusal lasts. Any other error turns the probe
            // off for nothing: a lifetime that outlived one `BUSY` would
            // run unprobed for days.
            Err(e) if e.code() == Some("NOPERM") => Self::Refused,
            Err(_) => Self::Unknown,
        }
    }
}

/// What to do after a step of the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Publish probe number `seq`.
    Send(u64),
    /// Nothing now.
    Hold,
    /// Deaf: drop the subscription and subscribe again.
    Deaf,
}

/// One step of the verdict: whether it counted a miss, and what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub missed: bool,
    pub action: Action,
}

/// The verdict of one lifetime, with no I/O: the loop feeds it what it
/// hears, the replies and the ticks.
#[derive(Debug, Default)]
pub struct Verdict {
    /// The last probe sent; 0 before the first.
    seq: u64,
    /// A PUBLISH is out with no reply yet.
    in_flight: bool,
    /// The last reply counted a receiver: its probe is judged at the next tick.
    awaiting: bool,
    /// A probe was heard since the last one was sent.
    heard: bool,
    /// Misses in a row.
    misses: u32,
    /// What the last reply counted.
    receivers: i64,
}

impl Verdict {
    /// A probe was heard: the connection delivers.
    pub fn heard(&mut self) {
        self.heard = true;
        self.misses = 0;
    }

    /// The probe out got its reply.
    pub fn replied(&mut self, reply: Reply) -> Step {
        self.in_flight = false;
        let Reply::Receivers(n) = reply else {
            // Unknown and refused neither count nor reset.
            return Step {
                missed: false,
                action: Action::Hold,
            };
        };
        self.receivers = n;
        if n >= 1 {
            self.awaiting = true;
            return Step {
                missed: false,
                action: Action::Hold,
            };
        }
        // Nobody on the channel: this connection's registration is gone.
        self.miss()
    }

    /// The interval elapsed: judge the last probe, then send the next one.
    /// A probe still out is not judged, and no second one is sent.
    pub fn tick(&mut self) -> Step {
        if self.in_flight {
            return Step {
                missed: false,
                action: Action::Hold,
            };
        }
        let unheard = std::mem::take(&mut self.awaiting) && !self.heard;
        if unheard {
            let step = self.miss();
            if step.action == Action::Deaf {
                return step;
            }
        }
        self.seq += 1;
        self.in_flight = true;
        self.heard = false;
        Step {
            missed: unheard,
            action: Action::Send(self.seq),
        }
    }

    fn miss(&mut self) -> Step {
        self.misses += 1;
        Step {
            missed: true,
            action: if self.misses >= DEAF_AFTER {
                Action::Deaf
            } else {
                Action::Hold
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn settings(subscriber_probe_ms: u64) -> roomler_ai_config::RedisSettings {
        roomler_ai_config::RedisSettings {
            url: "redis://r:6379".to_string(),
            response_timeout_ms: 3000,
            connection_timeout_ms: 3000,
            subscriber_probe_ms,
        }
    }

    /// 0 is off; a value under 1 s is used as 1 s; 1 s and over as given; the
    /// default is 15 s.
    #[test]
    fn the_probe_interval_is_off_at_0_and_never_under_1_s() {
        let used: Vec<_> = [0, 1, 500, 999, 1000, 15_000]
            .into_iter()
            .map(|given| {
                (
                    given,
                    ProbeSettings::from_settings(&settings(given)).interval_ms(),
                )
            })
            .collect();
        assert_eq!(
            used,
            [
                (0, 0),
                (1, 1000),
                (500, 1000),
                (999, 1000),
                (1000, 1000),
                (15_000, 15_000)
            ]
        );
        assert_eq!(
            ProbeSettings::from_settings(&settings(0)),
            ProbeSettings::OFF
        );
        assert_eq!(
            ProbeSettings::from_settings(&settings(15_000)),
            ProbeSettings::default()
        );
    }

    const HOLD: Step = Step {
        missed: false,
        action: Action::Hold,
    };

    fn send(seq: u64) -> Step {
        Step {
            missed: false,
            action: Action::Send(seq),
        }
    }

    /// Heard every time, whether the probe arrives before its reply or
    /// after: no miss, ever.
    #[test]
    fn a_healthy_subscriber_is_never_missed() {
        let mut v = Verdict::default();
        for seq in 1..=200 {
            assert_eq!(v.tick(), send(seq));
            if seq % 2 == 0 {
                v.heard();
                assert_eq!(v.replied(Reply::Receivers(1)), HOLD);
            } else {
                assert_eq!(v.replied(Reply::Receivers(3)), HOLD);
                v.heard();
            }
        }
        assert_eq!(v.misses, 0);
    }

    /// One probe heard only after the next tick costs one miss, and the
    /// late arrival resets it.
    #[test]
    fn one_late_arrival_costs_one_miss_and_resets() {
        let mut v = Verdict::default();
        assert_eq!(v.tick(), send(1));
        assert_eq!(v.replied(Reply::Receivers(1)), HOLD);
        assert_eq!(
            v.tick(),
            Step {
                missed: true,
                action: Action::Send(2)
            }
        );
        v.heard(); // probe 1, late
        assert_eq!(v.misses, 0);
        assert_eq!(v.replied(Reply::Receivers(1)), HOLD);
        v.heard(); // probe 2
        assert_eq!(v.tick(), send(3));
    }

    /// A reply of 0 receivers is a miss at once; two in a row are deaf.
    #[test]
    fn nobody_on_the_channel_twice_is_deaf() {
        let mut v = Verdict::default();
        assert_eq!(v.tick(), send(1));
        assert_eq!(
            v.replied(Reply::Receivers(0)),
            Step {
                missed: true,
                action: Action::Hold
            }
        );
        assert_eq!(v.tick(), send(2));
        assert_eq!(
            v.replied(Reply::Receivers(0)),
            Step {
                missed: true,
                action: Action::Deaf
            }
        );
    }

    /// Taken by Redis twice and heard neither time: deaf at the second
    /// judging tick.
    #[test]
    fn two_probes_unheard_in_a_row_are_deaf() {
        let mut v = Verdict::default();
        assert_eq!(v.tick(), send(1));
        v.replied(Reply::Receivers(1));
        assert_eq!(
            v.tick(),
            Step {
                missed: true,
                action: Action::Send(2)
            }
        );
        v.replied(Reply::Receivers(1));
        assert_eq!(
            v.tick(),
            Step {
                missed: true,
                action: Action::Deaf
            }
        );
    }

    /// A PUBLISH that fails, gets no answer or is refused says nothing about
    /// the subscriber: never a miss, however many.
    #[test]
    fn unknown_replies_are_never_a_verdict() {
        let mut v = Verdict::default();
        for seq in 1..=100 {
            assert_eq!(v.tick(), send(seq));
            let reply = if seq % 3 == 0 {
                Reply::Refused
            } else {
                Reply::Unknown
            };
            assert_eq!(v.replied(reply), HOLD);
        }
        assert_eq!(v.misses, 0);
    }

    /// An unknown between two misses neither counts nor resets: still deaf.
    #[test]
    fn miss_unknown_miss_is_deaf() {
        let mut v = Verdict::default();
        assert_eq!(v.tick(), send(1));
        v.replied(Reply::Receivers(1));
        assert_eq!(
            v.tick(),
            Step {
                missed: true,
                action: Action::Send(2)
            }
        );
        assert_eq!(v.replied(Reply::Unknown), HOLD);
        assert_eq!(v.tick(), send(3));
        v.replied(Reply::Receivers(1));
        assert_eq!(
            v.tick(),
            Step {
                missed: true,
                action: Action::Deaf
            }
        );
    }

    /// Two misses with a probe heard between them are not in a row.
    #[test]
    fn misses_apart_are_not_deaf() {
        let mut v = Verdict::default();
        let mut deaf = false;
        for seq in 1..=50 {
            let step = v.tick();
            deaf |= step.action == Action::Deaf;
            assert!(matches!(step.action, Action::Send(_)), "{seq}: {step:?}");
            v.replied(Reply::Receivers(1));
            if seq % 2 == 0 {
                v.heard();
            }
        }
        assert!(!deaf);
    }

    /// A probe still out at a tick is not judged, and no second one is sent.
    #[test]
    fn a_probe_in_flight_at_a_tick_is_not_judged() {
        let mut v = Verdict::default();
        assert_eq!(v.tick(), send(1));
        for _ in 0..10 {
            assert_eq!(v.tick(), HOLD);
        }
        assert_eq!(v.replied(Reply::Receivers(1)), HOLD);
        v.heard();
        assert_eq!(v.tick(), send(2));
        for _ in 0..10 {
            assert_eq!(v.tick(), HOLD);
        }
        assert_eq!(v.replied(Reply::Unknown), HOLD);
        assert_eq!(v.tick(), send(3));
        assert_eq!(v.misses, 0);
    }

    /// The reply classes: a count, no answer or a failed connection (io),
    /// and an error Redis answered.
    #[test]
    fn a_reply_is_a_count_unknown_or_refused() {
        assert_eq!(Reply::of(&Ok(2)), Reply::Receivers(2));
        let timed_out = RedisError::from(std::io::Error::from(std::io::ErrorKind::TimedOut));
        assert_eq!(Reply::of(&Err(timed_out)), Reply::Unknown);
        let reset = RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert_eq!(Reply::of(&Err(reset)), Reply::Unknown);
        // Error replies as Redis writes them, parsed the way a reply is.
        let reply = |wire: &[u8]| {
            redis::parse_redis_value(wire)
                .and_then(redis::Value::extract_error)
                .expect_err("an error reply")
        };
        let acl = reply(b"-NOPERM no permissions to access the 'c' channel\r\n");
        assert_eq!(acl.code(), Some("NOPERM"));
        assert_eq!(Reply::of(&Err(acl)), Reply::Refused);
        let busy = reply(b"-BUSY Redis is busy running a script.\r\n");
        assert_eq!(
            Reply::of(&Err(busy)),
            Reply::Unknown,
            "a busy script passes"
        );
        let err = reply(b"-ERR some other trouble\r\n");
        assert_eq!(Reply::of(&Err(err)), Reply::Unknown);
    }

    #[test]
    fn a_probe_channel_is_the_subscribers_own_and_new_each_lifetime() {
        let a = probe_channel(Subscriber::Bus);
        let b = probe_channel(Subscriber::Bus);
        assert!(a.starts_with("roomler:sub-probe:bus:"), "{a}");
        assert_ne!(a, b);
        assert!(probe_channel(Subscriber::Turn).starts_with("roomler:sub-probe:turn:"));
        assert!(probe_channel(Subscriber::Fanout).starts_with("roomler:sub-probe:fanout:"));
    }

    fn bounds_ms(ms: u64) -> RedisBounds {
        RedisBounds {
            response_timeout: Some(Duration::from_millis(ms)),
            connection_timeout: Some(Duration::from_millis(ms)),
        }
    }

    /// A listener that takes the connection and never reads: the kernel
    /// ACKs, nothing answers.
    async fn silent_listener() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        addr
    }

    /// box 2: the connect to a listener that never answers gives up within
    /// the connection timeout, naming the step; with no bound it waits, as
    /// before #3840.
    #[tokio::test]
    async fn a_subscribe_to_a_silent_listener_gives_up_within_the_bound() {
        let addr = silent_listener().await;
        let client = redis::Client::open(format!("redis://{addr}")).expect("url");
        let started = std::time::Instant::now();
        let result = subscribe_bounded(&client, bounds_ms(1000), &["roomler:ws"], &[]).await;
        let waited = started.elapsed();
        let Err(e) = result else {
            panic!("nothing answered, so nothing is subscribed");
        };
        assert!(e.is_timeout(), "{e}");
        assert!(
            e.to_string().contains("no answer within 1000 ms (connect)"),
            "{e}"
        );
        assert!(
            waited >= Duration::from_millis(900) && waited < Duration::from_secs(3),
            "{waited:?}"
        );
        let unbounded = tokio::time::timeout(
            Duration::from_millis(1500),
            subscribe_bounded(&client, RedisBounds::NONE, &["roomler:ws"], &[]),
        )
        .await;
        assert!(unbounded.is_err(), "with no bound it waits as before");
    }

    /// How the stand-in answers a SUBSCRIBE to a probe channel.
    #[derive(Clone, Copy)]
    enum ProbeAnswer {
        Answer,
        Silent,
        Refuse,
    }

    /// A Redis stand-in for one connection: it answers the handshake and
    /// every SUBSCRIBE, except a probe channel's, which it answers as told.
    /// It returns the address and how many probe SUBSCRIBEs it was sent.
    async fn stand_in(probe: ProbeAnswer) -> (std::net::SocketAddr, std::sync::Arc<AtomicU64>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let probes = std::sync::Arc::new(AtomicU64::new(0));
        let seen = probes.clone();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut subscribed = 0;
            loop {
                let n = match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                while let Some((args, used)) = command(&buf) {
                    buf.drain(..used);
                    let reply = match args.as_slice() {
                        [cmd, ..] if cmd.eq_ignore_ascii_case("CLIENT") => "+OK\r\n".to_string(),
                        [cmd, channel] if cmd.eq_ignore_ascii_case("SUBSCRIBE") => {
                            let is_probe = channel.starts_with(PROBE_CHANNEL_PREFIX);
                            if is_probe {
                                seen.fetch_add(1, Ordering::Relaxed);
                            }
                            match (is_probe, probe) {
                                (true, ProbeAnswer::Silent) => continue,
                                (true, ProbeAnswer::Refuse) => {
                                    "-NOPERM this user has no permissions to access one of the channels used as arguments\r\n".to_string()
                                }
                                _ => {
                                    subscribed += 1;
                                    format!(
                                        "*3\r\n$9\r\nsubscribe\r\n${}\r\n{channel}\r\n:{subscribed}\r\n",
                                        channel.len()
                                    )
                                }
                            }
                        }
                        _ => "-ERR unknown command\r\n".to_string(),
                    };
                    if socket.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            }
        });
        (addr, probes)
    }

    /// One RESP array of bulk strings off the front of `buf`: its arguments
    /// and the bytes it took; `None` until it is all there.
    fn command(buf: &[u8]) -> Option<(Vec<String>, usize)> {
        fn line(buf: &[u8], at: &mut usize) -> Option<String> {
            let end = buf[*at..].windows(2).position(|w| w == b"\r\n")? + *at;
            let text = String::from_utf8_lossy(&buf[*at..end]).into_owned();
            *at = end + 2;
            Some(text)
        }
        let mut at = 0;
        let n: usize = line(buf, &mut at)?.strip_prefix('*')?.parse().ok()?;
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            let len: usize = line(buf, &mut at)?.strip_prefix('$')?.parse().ok()?;
            if buf.len() < at + len + 2 {
                return None;
            }
            args.push(String::from_utf8_lossy(&buf[at..at + len]).into_owned());
            at += len + 2;
        }
        Some((args, at))
    }

    fn probe_on() -> ProbeSettings {
        ProbeSettings {
            interval: Some(Duration::from_secs(1)),
        }
    }

    /// FYI 7 of the brief, pinned: the probe channel's SUBSCRIBE with no
    /// answer fails the whole subscription within the response timeout,
    /// naming the probe channel, so the subscriber connects again. A lifetime
    /// run unprobed on a connection that stopped answering could be deaf
    /// from birth.
    #[tokio::test]
    async fn a_probe_channel_with_no_answer_fails_the_subscription_within_the_bound() {
        let (addr, probes) = stand_in(ProbeAnswer::Silent).await;
        let client = redis::Client::open(format!("redis://{addr}")).expect("url");
        let started = std::time::Instant::now();
        let result = subscribe_probed(
            &client,
            bounds_ms(1000),
            probe_on(),
            Subscriber::Fanout,
            &["roomler:ws"],
            &[],
        )
        .await;
        let waited = started.elapsed();
        let Err(e) = result else {
            panic!("a probe channel with no answer must fail the subscription");
        };
        assert!(e.is_timeout(), "{e}");
        assert!(
            e.to_string()
                .contains("SUBSCRIBE roomler:sub-probe:fanout:"),
            "{e}"
        );
        assert_eq!(probes.load(Ordering::Relaxed), 1);
        assert!(
            waited >= Duration::from_millis(900) && waited < Duration::from_secs(3),
            "{waited:?}"
        );
    }

    /// The probe channel answered: the lifetime is probed.
    #[tokio::test]
    async fn an_answered_probe_channel_makes_a_probed_subscription() {
        let (addr, probes) = stand_in(ProbeAnswer::Answer).await;
        let client = redis::Client::open(format!("redis://{addr}")).expect("url");
        let sub = subscribe_probed(
            &client,
            bounds_ms(1000),
            probe_on(),
            Subscriber::Bus,
            &["roomler:pod:p"],
            &[],
        )
        .await
        .map_err(|e| e.to_string())
        .expect("subscribed");
        assert!(sub.probed());
        assert_eq!(probes.load(Ordering::Relaxed), 1);
    }

    /// An error reply to the probe channel never fails the real
    /// subscription. (redis 0.27 reports the reply as success, so the
    /// refusal shows on the probe's PUBLISH, as [`Reply::Refused`].)
    #[tokio::test]
    async fn a_refused_probe_channel_leaves_the_subscription_standing() {
        let (addr, probes) = stand_in(ProbeAnswer::Refuse).await;
        let client = redis::Client::open(format!("redis://{addr}")).expect("url");
        let result = subscribe_probed(
            &client,
            bounds_ms(1000),
            probe_on(),
            Subscriber::Turn,
            &[],
            &[],
        )
        .await;
        assert!(result.is_ok(), "{:?}", result.err());
        assert_eq!(probes.load(Ordering::Relaxed), 1);
    }

    /// The probe off: no probe channel is asked for (the stand-in would
    /// never answer one), and the subscription is unprobed.
    #[tokio::test]
    async fn with_the_probe_off_no_probe_channel_is_asked_for() {
        let (addr, probes) = stand_in(ProbeAnswer::Silent).await;
        let client = redis::Client::open(format!("redis://{addr}")).expect("url");
        let sub = subscribe_probed(
            &client,
            bounds_ms(1000),
            ProbeSettings::OFF,
            Subscriber::Fanout,
            &["roomler:ws"],
            &[],
        )
        .await
        .map_err(|e| e.to_string())
        .expect("subscribed");
        assert!(!sub.probed());
        assert_eq!(probes.load(Ordering::Relaxed), 0);
    }
}
