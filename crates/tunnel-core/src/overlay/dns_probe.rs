// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! #1363 — the OS path, measured: do this host's own lookups reach the overlay
//! resolver?
//!
//! The runtime resolves `_roomler-probe.<magic domain>` through the OS
//! (`getaddrinfo`). Only an overlay resolver answers that name, and this
//! node's resolver answers it with this node's own overlay address
//! ([`super::dns::PROBE_LABEL`]). Two readers share the one lookup:
//!
//! - FR-72 P6's ladder ([`super::hosts::Ladder`]), which picks the resolution
//!   rung from it, exactly as it did when the lookup was its own;
//! - the LocalAPI status, which says `active` only when the OS resolved the
//!   name to this node's own address ([`crate::localapi::DnsStatus::health`]).
//!
//! The status used to read `active` from three attempts (the bind, the written
//! steer, a static read of the Linux resolver files) and said it over a
//! feature that resolved nothing: a policy-managed NRPT discards a written
//! rule, and nothing read the outcome.
//!
//! ⚠️ The lookup runs on a task of its own, never on the runtime loop:
//! `getaddrinfo` can take its whole timeout on a host whose DNS drops rather
//! than refuses, and the loop carries every carrier decision.
//!
//! ⚠️ On Windows, a probe that was not answered reads the NRPT stores to CHOOSE
//! THE WORDS, never to decide the state. A rule the NRPT holds is still not an
//! answer (FR-72: "effective ≠ answers").
//!
//! ⚠️ **What it costs the host's DNS** ([`Cadence`]). Where the steer reaches
//! this node, the lookup is answered by this node's own resolver and leaves
//! nothing on the wire. Where it is bypassed (the corporate laptop whose DNS
//! is enforced), the lookup goes to the host's own DNS servers, and a lookup
//! of one name at one fixed period reads as a beacon to a SOC. So the status's
//! probe waits twice as long after each reading that changed nothing, up to
//! 15 min, each wait jittered, and measures again at once only when something
//! it can see changed. The ladder, opt-in, keeps its one lookup a minute.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{Notify, watch};

use crate::localapi::{
    DNS_PROBE_EVERY_MAX_MS, DNS_PROBE_EVERY_MS, DnsProbeOutcome, DnsProbeStatus, DnsStatus,
    DnsUnreachedReason,
};

/// The first wait between two measurements, and the ladder's only one. The
/// LocalAPI constants are the single source: a client reads a reading as
/// stopped against the time it announced for the next one.
pub const PROBE_EVERY: Duration = Duration::from_millis(DNS_PROBE_EVERY_MS);

/// The longest wait between two measurements that only the status reads.
pub const PROBE_EVERY_MAX: Duration = Duration::from_millis(DNS_PROBE_EVERY_MAX_MS);

/// The longest single sleep of a wait. Between two, the probe compares its
/// clocks, so a host that slept is measured within this long of waking.
const CLOCK_CHECK_EVERY: Duration = Duration::from_secs(30);

/// The wall clock ran this much further than the monotonic one over one
/// sleep: the host slept. Linux's and macOS's monotonic clocks stop in a
/// suspend, so without this check a wait would run long by the time asleep.
const SLEPT_AFTER: Duration = Duration::from_secs(30);

/// Who reads the measurements, which decides how often they are taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cadence {
    /// FR-72 P6's ladder is on (`ROOMLERD_MAGICDNS_HOSTS=1`, opt-in): one
    /// lookup every [`PROBE_EVERY`], with no jitter, exactly as the ladder's own
    /// lookup was before #1363. Its climb back off the hosts file waits on
    /// the next reading.
    Ladder,
    /// Only the status reads them: [`PROBE_EVERY`], doubled after each
    /// reading that changed nothing, up to [`PROBE_EVERY_MAX`], every wait
    /// jittered by up to a tenth. A changed reading, or a kick, starts it over.
    Status,
}

/// The wait after a reading, before its jitter. `every` is the last one, and
/// `unchanged` says the reading was the one before it again.
fn next_every(cadence: Cadence, every: Duration, unchanged: bool) -> Duration {
    match cadence {
        Cadence::Ladder => PROBE_EVERY,
        Cadence::Status if unchanged => (every * 2).min(PROBE_EVERY_MAX),
        Cadence::Status => PROBE_EVERY,
    }
}

/// The wait itself: the status's varies by up to a tenth either way (`r` is
/// a random number), so no host asks at one fixed period and a fleet never
/// asks in step. The ladder's does not, as before #1363.
fn jittered(cadence: Cadence, every: Duration, r: u64) -> Duration {
    match cadence {
        Cadence::Ladder => every,
        Cadence::Status => every * (900 + (r % 201) as u32) / 1000,
    }
}

/// Did the host sleep during one sleep of a wait? `mono` and `wall` are what
/// the monotonic and the wall clock counted over it.
fn slept(mono: Duration, wall: Duration) -> bool {
    wall > mono + SLEPT_AFTER
}

/// A `getaddrinfo` that has not answered in this long is a failure. Generous:
/// on an enforced-DNS host the query is refused fast, but a corporate resolver
/// under load can be slow, and a false "DNS is broken" would write the hosts
/// fallback on a host that does not need it.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The first measurement waits this long, so a steer written a moment ago is
/// not judged before the OS has taken it.
const FIRST_PROBE_AFTER: Duration = Duration::from_secs(2);

/// What one lookup found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No measurement yet.
    Pending,
    /// The OS resolved the probe name to this node's own overlay address.
    Answered,
    /// The OS resolved it to another address: another resolver answers the
    /// domain here (WSL in its Windows host's org, through the host's DNS
    /// proxy).
    AnsweredElsewhere(IpAddr),
    /// No address within [`PROBE_TIMEOUT`].
    Unanswered,
    /// The probe is switched off on this node (`ROOMLERD_MAGICDNS_PROBE=0`).
    Off,
}

impl Outcome {
    /// FR-72 P6's question, unchanged: does the OS DNS path resolve the
    /// domain at all? Any answer means the DNS rung serves names and the
    /// hosts file is not needed.
    pub fn dns_resolves(self) -> bool {
        matches!(self, Self::Answered | Self::AnsweredElsewhere(_))
    }
}

/// Sort what `getaddrinfo` returned. `own` is this node's overlay address, the
/// one this node's resolver answers the probe with.
pub fn classify(addrs: impl IntoIterator<Item = IpAddr>, own: Ipv4Addr) -> Outcome {
    let mut first = None;
    for a in addrs {
        if a == IpAddr::V4(own) {
            return Outcome::Answered;
        }
        first.get_or_insert(a);
    }
    match first {
        Some(a) => Outcome::AnsweredElsewhere(a),
        None => Outcome::Unanswered,
    }
}

/// The latest measurement, as the probe task publishes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    /// 0 until the first measurement, then one more per measurement, so a
    /// reader acts on each one once.
    pub seq: u64,
    pub outcome: Outcome,
    pub at: Option<SystemTime>,
    /// Windows, after an [`Outcome::Unanswered`]: what the NRPT stores say.
    pub reason: Option<DnsUnreachedReason>,
    /// When the probe measures next, unless something kicks it sooner.
    pub next_at: Option<SystemTime>,
}

impl Default for Reading {
    /// Not measured yet: what a runtime with no probe running reads.
    fn default() -> Self {
        Self::pending()
    }
}

impl Reading {
    fn pending() -> Self {
        Self {
            seq: 0,
            outcome: Outcome::Pending,
            at: None,
            reason: None,
            next_at: None,
        }
    }

    /// What a runtime reads when the probe is switched off on this node. Its
    /// `seq` stays 0, so the ladder never acts on it.
    pub fn off() -> Self {
        Self {
            outcome: Outcome::Off,
            ..Self::pending()
        }
    }

    /// What a runtime with no probe running reads: not measured yet, or
    /// switched off (`probe_on` is `ROOMLERD_MAGICDNS_PROBE`).
    pub fn idle(probe_on: bool) -> Self {
        if probe_on {
            Self::pending()
        } else {
            Self::off()
        }
    }

    /// The LocalAPI's shape of this reading.
    pub fn to_wire(&self) -> DnsProbeStatus {
        let (outcome, answered_by) = match self.outcome {
            Outcome::Pending => (DnsProbeOutcome::Pending, None),
            Outcome::Answered => (DnsProbeOutcome::Answered, None),
            Outcome::AnsweredElsewhere(a) => {
                (DnsProbeOutcome::AnsweredElsewhere, Some(a.to_string()))
            }
            Outcome::Unanswered => (DnsProbeOutcome::Unanswered, None),
            Outcome::Off => (DnsProbeOutcome::Off, None),
        };
        let ms = |t: SystemTime| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as u64)
        };
        DnsProbeStatus {
            outcome,
            at_ms: self.at.and_then(ms),
            answered_by,
            reason: self.reason,
            next_at_ms: self.next_at.and_then(ms),
        }
    }
}

/// #1363 — bring the published status up to what is true now. The runtime
/// calls it every tick, so each fact is at most one tick old: the resolver's
/// own bound signal (`bound`, read fresh from its `watch`), the steer as
/// written, the probe's latest reading, and whether the hosts rung serves.
pub fn refresh_status(
    st: &mut DnsStatus,
    bound: bool,
    steer_written: bool,
    steer_bypassed: Option<&str>,
    reading: &Reading,
    hosts_active: bool,
) {
    st.resolver_bound = bound;
    st.os_steer_active = steer_written;
    st.os_steer_bypassed = steer_bypassed.map(str::to_string);
    st.probe = Some(reading.to_wire());
    st.hosts_fallback = hosts_active;
}

/// The probe task, owned by the runtime that started it.
pub struct OsProbe {
    rx: watch::Receiver<Reading>,
    kick: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl OsProbe {
    /// Start measuring `<PROBE_LABEL>.<magic_domain>` through the OS, as often
    /// as `cadence` says.
    pub fn spawn(magic_domain: String, own: Ipv4Addr, cadence: Cadence) -> Self {
        let name = format!("{}.{magic_domain}:0", super::dns::PROBE_LABEL);
        let ns = format!(".{magic_domain}");
        Self::spawn_with(
            move || lookup(name.clone(), own),
            move || unreached_reason(ns.clone()),
            cadence,
            rand::random::<u64>,
        )
    }

    /// [`Self::spawn`] with the lookup, the reason read and the jitter's
    /// random numbers injected (tests).
    fn spawn_with<L, LF, R, RF, J>(lookup: L, reason: R, cadence: Cadence, jitter: J) -> Self
    where
        L: Fn() -> LF + Send + 'static,
        LF: Future<Output = Outcome> + Send + 'static,
        R: Fn() -> RF + Send + 'static,
        RF: Future<Output = Option<DnsUnreachedReason>> + Send + 'static,
        J: Fn() -> u64 + Send + 'static,
    {
        let (tx, rx) = watch::channel(Reading::pending());
        let kick = Arc::new(Notify::new());
        let task = tokio::spawn(run(lookup, reason, cadence, jitter, tx, kick.clone()));
        Self { rx, kick, task }
    }

    /// The latest reading.
    pub fn reading(&self) -> Reading {
        self.rx.borrow().clone()
    }

    /// Measure again soon rather than at the next interval, and start the
    /// waits over: the OS steer was just installed, the resolver just got its
    /// socket back, or the network moved, and the last reading describes the
    /// state before it. Kicks that come together cost one lookup.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Runtime teardown: stop the task and wait for it (bounded), like the
    /// resolver beside it.
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(1), self.task).await;
    }
}

async fn run<L, LF, R, RF, J>(
    lookup: L,
    reason: R,
    cadence: Cadence,
    jitter: J,
    tx: watch::Sender<Reading>,
    kick: Arc<Notify>,
) where
    L: Fn() -> LF,
    LF: Future<Output = Outcome>,
    R: Fn() -> RF,
    RF: Future<Output = Option<DnsUnreachedReason>>,
    J: Fn() -> u64,
{
    let mut seq = 0u64;
    let mut wait = FIRST_PROBE_AFTER;
    // The un-jittered wait the next one is grown from.
    let mut every = PROBE_EVERY;
    let mut last: Option<(Outcome, Option<DnsUnreachedReason>)> = None;
    loop {
        let woken = pause(wait, &kick).await;
        if woken {
            // Something changed what the OS does: give it a moment to take
            // the change, as the first measurement does. A kick that came in
            // that moment asks for this same measurement.
            tokio::time::sleep(FIRST_PROBE_AFTER).await;
            tokio::select! {
                biased;
                () = kick.notified() => {}
                () = std::future::ready(()) => {}
            }
        }
        let outcome = lookup().await;
        let why = if outcome == Outcome::Unanswered {
            reason().await
        } else {
            None
        };
        let unchanged = !woken && last == Some((outcome, why));
        every = next_every(cadence, every, unchanged);
        wait = jittered(cadence, every, jitter());
        last = Some((outcome, why));
        seq += 1;
        let at = SystemTime::now();
        // `send_replace`: the value is stored even with no receiver alive.
        tx.send_replace(Reading {
            seq,
            outcome,
            at: Some(at),
            reason: why,
            next_at: Some(at + wait),
        });
    }
}

/// Sleep for `wait`, at most [`CLOCK_CHECK_EVERY`] at a time. `true` when it
/// ended early because something changed: a kick, or a host that slept (the
/// network it measured may be gone, and a monotonic clock that stopped in the
/// suspend would make the wait run long by the time asleep).
async fn pause(wait: Duration, kick: &Notify) -> bool {
    let end = tokio::time::Instant::now() + wait;
    loop {
        let left = end.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        let (mono, wall) = (tokio::time::Instant::now(), SystemTime::now());
        tokio::select! {
            () = tokio::time::sleep(left.min(CLOCK_CHECK_EVERY)) => {}
            // A kick that came while nobody waited is kept, and wakes this.
            () = kick.notified() => return true,
        }
        let wall = SystemTime::now().duration_since(wall).unwrap_or_default();
        if slept(mono.elapsed(), wall) {
            return true;
        }
    }
}

/// One lookup of the probe name through the OS.
async fn lookup(name: String, own: Ipv4Addr) -> Outcome {
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::lookup_host(name)).await {
        Ok(Ok(addrs)) => classify(addrs.map(|a| a.ip()), own),
        _ => Outcome::Unanswered,
    }
}

/// Windows: what the NRPT stores say about the namespace `ns` (`.<domain>`).
/// Elsewhere there is no NRPT: the Linux reason is the steer's bypass, read
/// when the steer was written.
async fn unreached_reason(ns: String) -> Option<DnsUnreachedReason> {
    #[cfg(windows)]
    {
        tokio::task::spawn_blocking(move || nrpt::reason(&ns))
            .await
            .ok()
            .flatten()
    }
    #[cfg(not(windows))]
    {
        let _ = ns;
        None
    }
}

/// Which words the NRPT stores choose for an unanswered probe.
///
/// `policy` is the Group Policy store's rules, `None` when the store does not
/// exist; `local` is the local store's. Each rule is its namespaces (the
/// `Name` value). Windows ignores every local rule while the policy store
/// exists, even an empty one (#1363, comment 4: the corporate laptop's key
/// held no rules, and the effective table was empty).
pub fn reason_from_stores(
    policy: Option<&[Vec<String>]>,
    local: &[Vec<String>],
    ns: &str,
) -> DnsUnreachedReason {
    let names = |rules: &[Vec<String>]| {
        rules
            .iter()
            .flatten()
            .any(|n| n.trim_end_matches('.').eq_ignore_ascii_case(ns))
    };
    match policy {
        Some(rules) if names(rules) => DnsUnreachedReason::NrptRulePresent,
        Some(_) => DnsUnreachedReason::NrptPolicyManaged,
        None if names(local) => DnsUnreachedReason::NrptRulePresent,
        None => DnsUnreachedReason::NrptRuleMissing,
    }
}

/// The NRPT stores, read from the registry: no PowerShell, so a host where
/// the probe keeps failing does not start a process a minute.
#[cfg(windows)]
mod nrpt {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_MULTI_SZ, RegCloseKey, RegEnumKeyExW,
        RegGetValueW, RegOpenKeyExW,
    };

    use crate::localapi::DnsUnreachedReason;

    /// The Group Policy store. Its existence alone makes Windows ignore the
    /// local store.
    const POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows NT\DNSClient\DnsPolicyConfig";
    /// The local store, where `Add-DnsClientNrptRule` writes.
    const LOCAL: &str = r"SYSTEM\CurrentControlSet\Services\Dnscache\Parameters\DnsPolicyConfig";
    /// A store holds a handful of rules; this many is not one worth reading.
    const MAX_RULES: u32 = 4096;
    /// A rule's `Name` is a few namespaces.
    const MAX_NAME_BYTES: u32 = 64 * 1024;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct OpenKey(HKEY);
    impl Drop for OpenKey {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the handle came from RegOpenKeyExW and is closed once.
                unsafe { RegCloseKey(self.0) };
            }
        }
    }

    /// `None` when even the local store cannot be read: no words are
    /// better than wrong ones.
    pub(super) fn reason(ns: &str) -> Option<DnsUnreachedReason> {
        let policy = match read_store(POLICY) {
            Ok(rules) => Some(rules),
            Err(rc) if rc == ERROR_FILE_NOT_FOUND => None,
            Err(_) => return None,
        };
        let local = match read_store(LOCAL) {
            Ok(rules) => rules,
            Err(rc) if rc == ERROR_FILE_NOT_FOUND => Vec::new(),
            Err(_) => return None,
        };
        Some(super::reason_from_stores(policy.as_deref(), &local, ns))
    }

    /// Every rule's namespaces in the store at `path`, or the error that
    /// opening it returned.
    fn read_store(path: &str) -> Result<Vec<Vec<String>>, u32> {
        let wpath = wide(path);
        let mut raw: HKEY = std::ptr::null_mut();
        // SAFETY: `wpath` is NUL-terminated and outlives the call; `raw` is a
        // valid out-pointer.
        let rc =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wpath.as_ptr(), 0, KEY_READ, &mut raw) };
        if rc != ERROR_SUCCESS || raw.is_null() {
            return Err(rc);
        }
        let key = OpenKey(raw);
        let mut rules = Vec::new();
        // A key name is at most 255 characters.
        let mut name = [0u16; 256];
        for index in 0..MAX_RULES {
            let mut len = name.len() as u32;
            // SAFETY: `name` holds `len` characters; the reserved, class and
            // time arguments are null, as the contract allows.
            let rc = unsafe {
                RegEnumKeyExW(
                    key.0,
                    index,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != ERROR_SUCCESS {
                // ERROR_NO_MORE_ITEMS, or a key that changed under us: what
                // was read is what there is.
                break;
            }
            let mut subkey: Vec<u16> = name[..(len as usize).min(name.len())].to_vec();
            subkey.push(0);
            rules.push(rule_names(&key, &subkey));
        }
        Ok(rules)
    }

    /// The `Name` value (REG_MULTI_SZ) of one rule: its namespaces.
    fn rule_names(key: &OpenKey, subkey: &[u16]) -> Vec<String> {
        let value = wide("Name");
        let mut len: u32 = 0;
        // SAFETY: NUL-terminated names that outlive the call; a null buffer
        // asks only for the size, in bytes.
        let rc = unsafe {
            RegGetValueW(
                key.0,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_MULTI_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS || len == 0 || len > MAX_NAME_BYTES {
            return Vec::new();
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `len` bytes; the call fails with ERROR_MORE_DATA
        // rather than overrun it if the value grew.
        let rc = unsafe {
            RegGetValueW(
                key.0,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_MULTI_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS {
            return Vec::new();
        }
        let chars = (len as usize / 2).min(buf.len());
        buf[..chars]
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }

    #[cfg(test)]
    mod tests {
        /// Read-only, on whatever host runs it: a namespace nobody writes is
        /// never read as present. (`None` is allowed: a reader the stores
        /// refuse gives no words rather than wrong ones.)
        #[test]
        fn a_namespace_nobody_wrote_is_never_present() {
            let r = super::reason(".roomler-test-never-written.invalid");
            assert_ne!(
                r,
                Some(crate::localapi::DnsUnreachedReason::NrptRulePresent)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const OWN: Ipv4Addr = Ipv4Addr::new(100, 65, 4, 30);

    #[test]
    fn only_this_nodes_own_address_is_an_answer() {
        assert_eq!(classify([IpAddr::V4(OWN)], OWN), Outcome::Answered);
        // Among others too: the OS may return more than one.
        let other = IpAddr::V4(Ipv4Addr::new(100, 65, 4, 2));
        assert_eq!(classify([other, IpAddr::V4(OWN)], OWN), Outcome::Answered);
        // Another node's resolver (WSL through its Windows host) or a
        // wildcard record upstream: names resolve, not through this node.
        assert_eq!(classify([other], OWN), Outcome::AnsweredElsewhere(other));
        assert_eq!(classify([], OWN), Outcome::Unanswered);
        // The ladder's reading is unchanged: any answer is the DNS rung.
        assert!(Outcome::Answered.dns_resolves());
        assert!(Outcome::AnsweredElsewhere(other).dns_resolves());
        assert!(!Outcome::Unanswered.dns_resolves());
        assert!(!Outcome::Pending.dns_resolves());
    }

    #[test]
    fn the_nrpt_stores_choose_the_words() {
        let ours = vec![".myorg.roomler.net".to_string()];
        let theirs = vec![".corp.example".to_string()];
        let ns = ".myorg.roomler.net";
        // The corporate laptop (#1363, comment 4): the policy store exists
        // with no rules, so the local rule we wrote is ignored.
        assert_eq!(
            reason_from_stores(Some(&[]), std::slice::from_ref(&ours), ns),
            DnsUnreachedReason::NrptPolicyManaged
        );
        assert_eq!(
            reason_from_stores(
                Some(std::slice::from_ref(&theirs)),
                std::slice::from_ref(&ours),
                ns
            ),
            DnsUnreachedReason::NrptPolicyManaged
        );
        // A policy that carries our namespace holds the rule.
        assert_eq!(
            reason_from_stores(Some(&[theirs.clone(), ours.clone()]), &[], ns),
            DnsUnreachedReason::NrptRulePresent
        );
        // No policy store: the local store is the one Windows reads.
        assert_eq!(
            reason_from_stores(None, std::slice::from_ref(&ours), ns),
            DnsUnreachedReason::NrptRulePresent
        );
        assert_eq!(
            reason_from_stores(None, std::slice::from_ref(&theirs), ns),
            DnsUnreachedReason::NrptRuleMissing
        );
        // Windows keeps the namespace as written; match it as DNS does.
        let upper = vec![".MyOrg.Roomler.Net.".to_string()];
        assert_eq!(
            reason_from_stores(None, &[upper], ns),
            DnsUnreachedReason::NrptRulePresent
        );
    }

    #[test]
    fn a_reading_carries_its_time_and_who_answered() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis(1_234);
        let r = Reading {
            seq: 3,
            outcome: Outcome::AnsweredElsewhere(IpAddr::V4(Ipv4Addr::new(100, 65, 4, 2))),
            at: Some(at),
            reason: None,
            next_at: Some(at + PROBE_EVERY),
        };
        let w = r.to_wire();
        assert_eq!(w.outcome, DnsProbeOutcome::AnsweredElsewhere);
        assert_eq!(w.at_ms, Some(1_234));
        assert_eq!(w.next_at_ms, Some(61_234));
        assert_eq!(w.answered_by.as_deref(), Some("100.65.4.2"));
        let p = Reading::pending().to_wire();
        assert_eq!(p.outcome, DnsProbeOutcome::Pending);
        assert_eq!((p.at_ms, p.next_at_ms), (None, None));
        // Switched off: nothing measured, and the ladder never acts on it.
        let off = Reading::off();
        assert_eq!(off.seq, 0);
        assert!(!off.outcome.dns_resolves());
        assert_eq!(off.to_wire().outcome, DnsProbeOutcome::Off);
    }

    /// A jitter source that leaves every wait as it is.
    fn no_jitter() -> u64 {
        100
    }

    /// The wait each reading announced, from its own two times.
    fn announced(r: &Reading) -> Duration {
        r.next_at.unwrap().duration_since(r.at.unwrap()).unwrap()
    }

    #[test]
    fn the_status_waits_double_to_fifteen_minutes_and_vary_by_a_tenth() {
        let s = Cadence::Status;
        let mut every = PROBE_EVERY;
        let mut waits = vec![];
        for _ in 0..6 {
            every = next_every(s, every, true);
            waits.push(every.as_secs());
        }
        assert_eq!(waits, [120, 240, 480, 900, 900, 900]);
        assert_eq!(next_every(s, PROBE_EVERY_MAX, false), PROBE_EVERY);
        // The ladder's never grow, as before #1363.
        assert_eq!(next_every(Cadence::Ladder, PROBE_EVERY, true), PROBE_EVERY);

        let t = Duration::from_secs(600);
        assert_eq!(jittered(s, t, 0), Duration::from_secs(540));
        assert_eq!(jittered(s, t, 100), t);
        assert_eq!(jittered(s, t, 200), Duration::from_secs(660));
        assert_eq!(jittered(s, t, 201), Duration::from_secs(540));
        // Any u64 stays within the tenth (u64::MAX % 201 == 150).
        assert_eq!(jittered(s, t, u64::MAX), Duration::from_secs(630));
        assert_eq!(jittered(Cadence::Ladder, t, 0), t);
    }

    #[test]
    fn a_host_that_slept_is_seen_by_its_clocks() {
        let step = CLOCK_CHECK_EVERY;
        assert!(!slept(step, step), "awake: both clocks agree");
        assert!(!slept(step, step + SLEPT_AFTER), "at the bound");
        assert!(slept(step, step + SLEPT_AFTER + Duration::from_millis(1)));
        // A wall clock set back, or a test's paused clock, is not a sleep.
        assert!(!slept(step, Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn the_status_probe_waits_longer_while_nothing_changes_and_starts_over_on_a_change() {
        let lookups = Arc::new(AtomicU32::new(0));
        let l = lookups.clone();
        let started = tokio::time::Instant::now();
        let p = OsProbe::spawn_with(
            move || {
                // Five readings that are the same, then a changed one.
                let n = l.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 5 {
                        Outcome::Unanswered
                    } else {
                        Outcome::Answered
                    }
                }
            },
            || async { None },
            Cadence::Status,
            no_jitter,
        );
        let mut at = vec![];
        let mut waits = vec![];
        for seq in 1..=7 {
            let r = until_seq(&p, seq).await;
            at.push(started.elapsed().as_secs());
            waits.push(announced(&r).as_secs());
        }
        assert_eq!(at, [2, 62, 182, 422, 902, 1802, 1862]);
        assert_eq!(waits, [60, 120, 240, 480, 900, 60, 120]);
        // 7 lookups in 31 min, where one a minute made 32.
        assert_eq!(lookups.load(Ordering::SeqCst), 7);
        p.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_ladder_probe_keeps_one_lookup_a_minute() {
        let started = tokio::time::Instant::now();
        // A jitter source that would shorten a status wait by a tenth.
        let p = OsProbe::spawn_with(
            || async { Outcome::Unanswered },
            || async { None },
            Cadence::Ladder,
            || 0,
        );
        let mut at = vec![];
        for seq in 1..=4 {
            let r = until_seq(&p, seq).await;
            at.push(started.elapsed().as_secs());
            assert_eq!(announced(&r), PROBE_EVERY);
        }
        assert_eq!(at, [2, 62, 122, 182]);
        p.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_kick_measures_soon_and_starts_the_waits_over() {
        let p = OsProbe::spawn_with(
            || async { Outcome::Unanswered },
            || async { None },
            Cadence::Status,
            no_jitter,
        );
        // Four unchanged readings: the next wait is 8 min.
        let r = until_seq(&p, 4).await;
        assert_eq!(announced(&r), Duration::from_secs(480));
        tokio::time::sleep(Duration::from_secs(10)).await;
        let t = tokio::time::Instant::now();
        // Two kicks together cost one lookup.
        p.kick();
        p.kick();
        let r = until_seq(&p, 5).await;
        assert_eq!(
            t.elapsed(),
            FIRST_PROBE_AFTER,
            "the change is given a moment"
        );
        assert_eq!(announced(&r), PROBE_EVERY, "and the waits start over");
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(p.reading().seq, 5, "the second kick asked for nothing more");
        p.shutdown().await;
    }

    /// Wait (in virtual time) until the probe publishes reading `seq`.
    async fn until_seq(p: &OsProbe, seq: u64) -> Reading {
        let mut rx = p.rx.clone();
        tokio::time::timeout(2 * PROBE_EVERY_MAX, async {
            loop {
                if rx.borrow_and_update().seq >= seq {
                    return rx.borrow().clone();
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("the probe published")
    }

    #[tokio::test(start_paused = true)]
    async fn the_probe_measures_at_its_cadence_and_reads_the_reason_only_when_unanswered() {
        let lookups = Arc::new(AtomicU32::new(0));
        let reasons = Arc::new(AtomicU32::new(0));
        let (l, r) = (lookups.clone(), reasons.clone());
        let started = tokio::time::Instant::now();
        let p = OsProbe::spawn_with(
            move || {
                // Answered, then unanswered, alternately.
                let n = l.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n % 2 == 0 {
                        Outcome::Answered
                    } else {
                        Outcome::Unanswered
                    }
                }
            },
            move || {
                r.fetch_add(1, Ordering::SeqCst);
                async { Some(DnsUnreachedReason::NrptPolicyManaged) }
            },
            Cadence::Status,
            no_jitter,
        );
        assert_eq!(p.reading(), Reading::pending(), "not measured yet");

        let first = until_seq(&p, 1).await;
        assert_eq!(first.outcome, Outcome::Answered);
        assert_eq!(first.reason, None);
        assert!(first.at.is_some());
        assert_eq!(started.elapsed(), FIRST_PROBE_AFTER);
        assert_eq!(
            reasons.load(Ordering::SeqCst),
            0,
            "an answer reads no reason"
        );

        let second = until_seq(&p, 2).await;
        assert_eq!(second.outcome, Outcome::Unanswered);
        assert_eq!(second.reason, Some(DnsUnreachedReason::NrptPolicyManaged));
        assert_eq!(started.elapsed(), FIRST_PROBE_AFTER + PROBE_EVERY);
        assert_eq!(lookups.load(Ordering::SeqCst), 2, "one lookup a reading");
        p.shutdown().await;
    }

    /// #1363's second unit criterion: a resolver that LOSES its socket after a
    /// successful bind reads as not bound in `DnsStatus` within one tick, and
    /// stays so while its rebind fails. Before, only a failed bind reported
    /// `false`, and the runtime read the signal only at bring-up and at a late
    /// bind: a resolver sitting in its rebind loop read as bound, and `active`.
    #[tokio::test(start_paused = true)]
    async fn a_resolver_that_loses_its_socket_reads_not_bound_within_one_tick() {
        use crate::localapi::DnsHealth;
        use crate::overlay::dns::{DnsConfig, run_with};

        let cfg = DnsConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            magic_domain: "myorg.roomler.net".into(),
            upstream: "127.0.0.1:9".parse().unwrap(),
            names: Default::default(),
            answer_aaaa: true,
        };
        let binds = Arc::new(AtomicU32::new(0));
        let lose = Arc::new(Notify::new());
        let (tx, rx) = watch::channel(false);
        let (b, l) = (binds.clone(), lose.clone());
        let resolver = tokio::spawn(run_with(
            cfg,
            tx,
            move || {
                // The first bind works; every rebind finds the port taken.
                let n = b.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        tokio::net::UdpSocket::bind("127.0.0.1:0").await
                    } else {
                        Err(std::io::Error::from(std::io::ErrorKind::AddrInUse))
                    }
                }
            },
            // Serves until the test takes the socket away, as a receive that
            // fails for good does.
            move |_cfg, _sock| {
                let l = l.clone();
                async move { l.notified().await }
            },
        ));

        let mut st = DnsStatus {
            magic_domain: "myorg.roomler.net".into(),
            resolver_bound: false,
            os_steer_active: true,
            upstream: "1.1.1.1:53".into(),
            answer_aaaa: true,
            os_steer_bypassed: None,
            probe: None,
            hosts_fallback: false,
        };
        // The last reading: answered, a moment before the socket went.
        let answered = Reading {
            seq: 1,
            outcome: Outcome::Answered,
            at: Some(SystemTime::now()),
            reason: None,
            next_at: Some(SystemTime::now() + PROBE_EVERY),
        };
        let now_ms = || {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        // One runtime tick: the status read from the live signal.
        let tick =
            |st: &mut DnsStatus| refresh_status(st, *rx.borrow(), true, None, &answered, false);

        let mut up = rx.clone();
        while !*up.borrow_and_update() {
            up.changed().await.unwrap();
        }
        tick(&mut st);
        assert!(st.resolver_bound);
        assert_eq!(st.health(now_ms()), DnsHealth::Active);

        // The socket fails. One tick later (the runtime sweeps every 5 s):
        lose.notify_one();
        tokio::time::sleep(Duration::from_secs(5)).await;
        tick(&mut st);
        assert!(
            !st.resolver_bound,
            "a lost socket reads as not bound within one tick"
        );
        assert_eq!(
            st.health(now_ms()),
            DnsHealth::ResolverBinding,
            "the answer before the loss is not repeated as active"
        );
        assert!(
            binds.load(Ordering::SeqCst) >= 2,
            "the resolver is rebinding"
        );

        // And it stays not bound for as long as the rebind fails.
        tokio::time::sleep(Duration::from_secs(120)).await;
        tick(&mut st);
        assert!(!st.resolver_bound);
        resolver.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_kick_measures_now_rather_than_at_the_interval() {
        let p = OsProbe::spawn_with(
            || async { Outcome::Unanswered },
            || async { None },
            Cadence::Status,
            no_jitter,
        );
        until_seq(&p, 1).await;
        let t = tokio::time::Instant::now();
        p.kick();
        let r = until_seq(&p, 2).await;
        assert_eq!(r.seq, 2);
        assert!(t.elapsed() < PROBE_EVERY, "{:?}", t.elapsed());
        p.shutdown().await;
    }
}
