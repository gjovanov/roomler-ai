//! netstate — the process-wide network-state subsystem.
//!
//! ONE owner of the OS network-change signal, typed state, and non-blocking
//! fan-out; everything else is a subscriber that reconciles against
//! snapshots. Replaces the per-runtime string-typed `route_events`
//! subscription (which registered the OS callbacks once PER ORG and told its
//! consumer nothing beyond `starts_with("addr")`).
//!
//! Design (docs/… "netstate", 2026-08-16):
//! * **Backend** (per OS) pushes raw `(RawSignal, detail)` pairs from the OS
//!   callbacks into an unbounded channel — never blocks, never allocates
//!   beyond the tiny detail string, exactly the discipline `route_events`
//!   established. Windows = the three IP-Helper registrations
//!   (`NotifyRouteChange2` / `NotifyUnicastIpAddressChange` /
//!   `NotifyIpInterfaceChange`), registered ONCE per process. Non-Windows =
//!   `ip -o monitor route` (route class only; full parity is PR-3).
//! * **Monitor** debounces bursts (a Check Point connect injects dozens of
//!   routes in one gulp), samples a [`NetSnapshot`], diffs it against the
//!   previous one, and publishes a [`NetDelta`] with a severity verdict.
//! * **Fan-out**: `watch<Arc<NetSnapshot>>` (latest state — always readable)
//!   plus `broadcast<NetDelta>` (wake-ups). A lagged subscriber loses only
//!   wake-ups, never state: on `Lagged` it reconciles from the watch. The
//!   monitor never waits on any subscriber.
//!
//! The subsystem is spawned lazily on first [`handle`] call and lives for
//! the process (the OS registration is cancelled at process exit, not per
//! overlay-runtime rebuild — subscribers come and go via handles).
//!
//! Config: `overlay_netmon` (default ON) + `overlay_netmon_debounce_ms`
//! (default 750, clamped 100–5000) via the standard env bridge
//! (`ROOMLER_NODE_OVERLAY_NETMON*`).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

/// Master switch (`ROOMLER_NODE_OVERLAY_NETMON`, default ON).
pub(crate) fn netmon_enabled() -> bool {
    crate::env::flag("OVERLAY_NETMON", true)
}

/// Minimum spacing between event-driven route re-assert waves (consumed by
/// the runtime's net-change arm; formerly `route_events`').
pub(crate) const ROUTE_WAVE_MIN_INTERVAL: Duration = Duration::from_secs(3);

/// `ROOMLER_NODE_OVERLAY_ROUTE_EVENTS` — the legacy per-runtime consumer
/// kill-switch, still honored (default ON; `0`/`false`/`off` = the runtime
/// ignores net deltas and keeps the 2 s tick-only route guard, the pre-P4
/// behaviour). The subsystem-wide switch is [`netmon_enabled`].
pub(crate) fn route_events_enabled() -> bool {
    !matches!(
        crate::env::node_env("OVERLAY_ROUTE_EVENTS")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Debounce window: further raw signals inside this quiet period are
/// absorbed into one delta (`ROOMLER_NODE_OVERLAY_NETMON_DEBOUNCE_MS`).
fn debounce() -> Duration {
    let ms = crate::env::node_env("OVERLAY_NETMON_DEBOUNCE_MS")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(750)
        .clamp(100, 5000);
    Duration::from_millis(ms)
}

/// A continuous signal storm must still emit periodically — cap the absorb
/// window so a delta lands at least this often while churn is ongoing.
const DEBOUNCE_CAP: Duration = Duration::from_secs(3);

/// Raw change classes from the OS backend. Deliberately coarse — the
/// snapshot diff carries the real information; these only (a) label the
/// burst and (b) preserve the legacy `route_events` string classes for the
/// compat shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawSignal {
    Route,
    Addr,
    Iface,
}

/// One interface's addresses, keyed by name in [`NetSnapshot::ifaces`].
/// `vpn_class` is the NAME-based deny-list verdict
/// ([`super::direct::lan_iface_denied`]) — a LABEL, not the Major-severity
/// input: the Check Point adapter is named just "Ethernet" and evades
/// name-only classification, which is exactly why severity keys on
/// default-route movement instead.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IfaceSnap {
    pub v4: Vec<std::net::Ipv4Addr>,
    pub v6_count: usize,
    pub vpn_class: bool,
}

/// The effective default route per family: where a packet to a far
/// destination actually leaves. Sampled via a route LOOKUP (`GetBestRoute2`
/// / `ip route get`), not a `/0`-table scan — a corp capture rarely touches
/// `/0` (Check Point injects `/1`s and supernets; our exits install `/1`s),
/// but the lookup sees through all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRoute {
    /// Interface identity: ifindex (Windows) or device name (Unix) as text.
    pub ifref: String,
    pub gateway: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetSnapshot {
    pub ifaces: BTreeMap<String, IfaceSnap>,
    pub default_v4: Option<DefaultRoute>,
    pub default_v6: Option<DefaultRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The network materially moved: the effective default route changed
    /// identity, or addresses vanished (a socket bound to one is now dead).
    Major,
    /// Something changed but nothing our sockets/strategies key on.
    Minor,
}

/// One published change, derived purely from two snapshots + the burst's
/// raw-signal classes. `summary` is the one-line INFO evidence.
///
/// `material == false` = the burst changed NOTHING the snapshot models (no
/// default-route move, no address delta) — typically our own route re-assert
/// waves or a corp client rewriting specific routes. Still published as a
/// WAKE-UP: the route guard's whole P4 contract is re-asserting within
/// milliseconds of an erase, and an erased peer `/32` is exactly a
/// snapshot-invisible change. Severity-driven subscribers (PR-2) filter on
/// `material`/`severity`; churn-driven ones (route guard) act on every delta.
#[derive(Debug, Clone)]
pub struct NetDelta {
    #[allow(dead_code)] // consumed by the PR-2 severity-driven subscribers
    pub severity: Severity,
    #[allow(dead_code)] // consumed by the PR-2 severity-driven subscribers
    pub material: bool,
    #[allow(dead_code)] // consumed by the PR-2 severity-driven subscribers
    pub default_route_moved: bool,
    #[allow(dead_code)] // consumed by the PR-2 severity-driven subscribers
    pub addrs_added: usize,
    #[allow(dead_code)] // consumed by the PR-2 severity-driven subscribers
    pub addrs_removed: usize,
    pub saw_addr_signal: bool,
    pub saw_iface_signal: bool,
    pub summary: String,
}

/// Pure diff: `None` when nothing material changed (a burst of self-inflicted
/// route churn — our own re-assert waves — lands here).
pub(crate) fn diff(
    prev: &NetSnapshot,
    next: &NetSnapshot,
    saw_addr: bool,
    saw_iface: bool,
) -> Option<NetDelta> {
    let default_route_moved =
        prev.default_v4 != next.default_v4 || prev.default_v6 != next.default_v6;
    let count =
        |s: &NetSnapshot| -> usize { s.ifaces.values().map(|i| i.v4.len() + i.v6_count).sum() };
    let (mut added, mut removed) = (0usize, 0usize);
    for (name, ni) in &next.ifaces {
        match prev.ifaces.get(name) {
            Some(pi) => {
                added += ni.v4.iter().filter(|a| !pi.v4.contains(a)).count();
                removed += pi.v4.iter().filter(|a| !ni.v4.contains(a)).count();
                added += ni.v6_count.saturating_sub(pi.v6_count);
                removed += pi.v6_count.saturating_sub(ni.v6_count);
            }
            None => added += ni.v4.len() + ni.v6_count,
        }
    }
    for (name, pi) in &prev.ifaces {
        if !next.ifaces.contains_key(name) {
            removed += pi.v4.len() + pi.v6_count;
        }
    }
    if !default_route_moved && added == 0 && removed == 0 {
        return None;
    }
    let severity = if default_route_moved || removed > 0 {
        Severity::Major
    } else {
        Severity::Minor
    };
    let material = true;
    let summary = format!(
        "default_moved={default_route_moved} v4_default={} addrs +{added}/-{removed} ifaces={} (was {}, addrs {}→{})",
        next.default_v4
            .as_ref()
            .map(|d| d.ifref.as_str())
            .unwrap_or("-"),
        next.ifaces.len(),
        prev.ifaces.len(),
        count(prev),
        count(next),
    );
    Some(NetDelta {
        severity,
        material,
        default_route_moved,
        addrs_added: added,
        addrs_removed: removed,
        saw_addr_signal: saw_addr,
        saw_iface_signal: saw_iface,
        summary,
    })
}

/// A subscriber's view: the latest snapshot (always readable) + the delta
/// wake-up stream. On `broadcast::error::RecvError::Lagged`, reconcile from
/// [`Self::snapshot`] — state is never lost, only wake-ups.
pub struct NetstateHandle {
    snap_rx: watch::Receiver<Arc<NetSnapshot>>,
    deltas: broadcast::Sender<NetDelta>,
}

impl NetstateHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<NetDelta> {
        self.deltas.subscribe()
    }
    #[allow(dead_code)] // consumed by the PR-2 subscribers
    pub fn snapshot(&self) -> Arc<NetSnapshot> {
        self.snap_rx.borrow().clone()
    }
}

/// The process-wide instance, spawned lazily on first call (needs a tokio
/// context). `None` = disabled by config or the OS backend failed to
/// register — subscribers keep their timer fallbacks.
pub fn handle() -> Option<&'static NetstateHandle> {
    static INSTANCE: OnceLock<Option<NetstateHandle>> = OnceLock::new();
    INSTANCE.get_or_init(spawn).as_ref()
}

fn spawn() -> Option<NetstateHandle> {
    if !netmon_enabled() {
        debug!("netstate: disabled (ROOMLER_NODE_OVERLAY_NETMON=0)");
        return None;
    }
    let (raw_tx, raw_rx) = mpsc::unbounded_channel();
    let _guard = backend::spawn(raw_tx)?;
    let initial = Arc::new(sample_snapshot());
    let (snap_tx, snap_rx) = watch::channel(initial.clone());
    let (delta_tx, _keepalive_rx) = broadcast::channel(16);
    info!(
        ifaces = initial.ifaces.len(),
        v4_default = initial
            .default_v4
            .as_ref()
            .map(|d| d.ifref.as_str())
            .unwrap_or("-"),
        "netstate: network monitor up (one OS registration, process-wide)"
    );
    tokio::spawn(monitor(raw_rx, snap_tx, delta_tx.clone(), _guard, initial));
    Some(NetstateHandle {
        snap_rx,
        deltas: delta_tx,
    })
}

async fn monitor(
    mut raw_rx: mpsc::UnboundedReceiver<(RawSignal, String)>,
    snap_tx: watch::Sender<Arc<NetSnapshot>>,
    delta_tx: broadcast::Sender<NetDelta>,
    _guard: backend::BackendGuard,
    mut prev: Arc<NetSnapshot>,
) {
    let quiet = debounce();
    loop {
        let Some((first, _detail)) = raw_rx.recv().await else {
            warn!(
                "netstate: OS backend channel closed — monitor stopping (subscribers degrade to timers)"
            );
            return;
        };
        let mut saw_addr = first == RawSignal::Addr;
        let mut saw_iface = first == RawSignal::Iface;
        let mut absorbed = 0usize;
        let cap = tokio::time::Instant::now() + DEBOUNCE_CAP;
        loop {
            match tokio::time::timeout(quiet, raw_rx.recv()).await {
                Ok(Some((sig, _))) => {
                    saw_addr |= sig == RawSignal::Addr;
                    saw_iface |= sig == RawSignal::Iface;
                    absorbed += 1;
                    if tokio::time::Instant::now() >= cap {
                        break;
                    }
                }
                Ok(None) => {
                    warn!("netstate: OS backend channel closed mid-burst — monitor stopping");
                    return;
                }
                Err(_) => break, // quiet period elapsed
            }
        }
        let next = Arc::new(sample_snapshot());
        match diff(&prev, &next, saw_addr, saw_iface) {
            Some(delta) => {
                info!(
                    severity = ?delta.severity,
                    absorbed,
                    %delta.summary,
                    "netstate: network changed"
                );
                let _ = snap_tx.send(next.clone());
                let _ = delta_tx.send(delta); // no receivers = fine
            }
            None => {
                // Snapshot-invisible churn (an erased peer /32, our own
                // re-assert wave) — still a wake-up, flagged immaterial.
                debug!(
                    absorbed,
                    "netstate: signal burst with no snapshot-visible change"
                );
                let _ = delta_tx.send(NetDelta {
                    severity: Severity::Minor,
                    material: false,
                    default_route_moved: false,
                    addrs_added: 0,
                    addrs_removed: 0,
                    saw_addr_signal: saw_addr,
                    saw_iface_signal: saw_iface,
                    summary: format!("route churn, no snapshot delta ({} signals)", absorbed + 1),
                });
            }
        }
        prev = next;
    }
}

/// Sample the CURRENT network state. Interfaces via `if_addrs` (portable);
/// effective default routes via a route lookup per family.
pub fn sample_snapshot() -> NetSnapshot {
    let mut ifaces: BTreeMap<String, IfaceSnap> = BTreeMap::new();
    if let Ok(addrs) = if_addrs::get_if_addrs() {
        for a in addrs {
            let slot = ifaces.entry(a.name.clone()).or_default();
            slot.vpn_class = super::direct::lan_iface_denied(&a.name, "");
            match a.addr {
                if_addrs::IfAddr::V4(v4) => {
                    if !v4.ip.is_loopback() {
                        slot.v4.push(v4.ip);
                    }
                }
                if_addrs::IfAddr::V6(v6) => {
                    if !v6.ip.is_loopback() {
                        slot.v6_count += 1;
                    }
                }
            }
        }
        for s in ifaces.values_mut() {
            s.v4.sort_unstable();
        }
    }
    NetSnapshot {
        ifaces,
        default_v4: default_route_v4(),
        default_v6: None, // v6 lookup lands with the PR-3 platform pass
    }
}

#[cfg(windows)]
fn default_route_v4() -> Option<DefaultRoute> {
    use windows_sys::Win32::Foundation::NO_ERROR;
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute2, MIB_IPFORWARD_ROW2};
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_INET};
    // SAFETY: zeroed in/out structs + the documented GetBestRoute2 call
    // shape; the row is only read on NO_ERROR. Destination is a fixed public
    // anycast address — the lookup never sends a packet.
    unsafe {
        let mut dest: SOCKADDR_INET = core::mem::zeroed();
        dest.Ipv4.sin_family = AF_INET;
        dest.Ipv4.sin_addr.S_un.S_addr = u32::from(std::net::Ipv4Addr::new(8, 8, 8, 8)).to_be();
        let mut row: MIB_IPFORWARD_ROW2 = core::mem::zeroed();
        let mut src: SOCKADDR_INET = core::mem::zeroed();
        if GetBestRoute2(
            core::ptr::null_mut(),
            0,
            core::ptr::null(),
            &dest,
            0,
            &mut row,
            &mut src,
        ) != NO_ERROR
        {
            return None;
        }
        let gw = if row.NextHop.si_family == AF_INET {
            let o = row.NextHop.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes();
            let ip = std::net::Ipv4Addr::new(o[0], o[1], o[2], o[3]);
            (!ip.is_unspecified()).then_some(IpAddr::V4(ip))
        } else {
            None
        };
        Some(DefaultRoute {
            ifref: row.InterfaceIndex.to_string(),
            gateway: gw,
        })
    }
}

#[cfg(target_os = "linux")]
fn default_route_v4() -> Option<DefaultRoute> {
    // One-shot lookup, matching the existing shelled-`ip` style. Cheap and
    // dependency-free; the rtnetlink upgrade is the PR-3 platform pass.
    let out = std::process::Command::new("ip")
        .args(["-o", "route", "get", "8.8.8.8"])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let mut toks = line.split_whitespace().peekable();
    let (mut dev, mut via) = (None, None);
    while let Some(t) = toks.next() {
        match t {
            "dev" => dev = toks.peek().map(|s| s.to_string()),
            "via" => via = toks.peek().and_then(|s| s.parse::<IpAddr>().ok()),
            _ => {}
        }
    }
    dev.map(|d| DefaultRoute {
        ifref: d,
        gateway: via,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn default_route_v4() -> Option<DefaultRoute> {
    None // macOS lands with the PR-3 platform pass (PF_ROUTE)
}

/// The OS raw-signal backends — the `route_events` registrations, moved here
/// verbatim with the sender retyped to `(RawSignal, detail)`.
mod backend {
    use super::RawSignal;

    #[cfg(windows)]
    pub(super) use win::{BackendGuard, spawn};

    #[cfg(not(windows))]
    pub(super) use unix::{BackendGuard, spawn};

    #[cfg(windows)]
    mod win {
        use super::RawSignal;
        use core::ffi::c_void;
        use tokio::sync::mpsc::UnboundedSender;
        use windows_sys::Win32::Foundation::{HANDLE, NO_ERROR};
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            CancelMibChangeNotify2, MIB_IPFORWARD_ROW2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
            MIB_UNICASTIPADDRESS_ROW, NotifyIpInterfaceChange, NotifyRouteChange2,
            NotifyUnicastIpAddressChange,
        };
        use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;

        struct Ctx {
            tx: UnboundedSender<(RawSignal, String)>,
        }

        pub(in super::super) struct BackendGuard {
            handle: HANDLE,
            addr_handle: HANDLE,
            iface_handle: HANDLE,
            ctx: *mut Ctx,
        }
        // Raw pointers are only touched in Drop, after CancelMibChangeNotify2
        // guarantees no callback is running.
        unsafe impl Send for BackendGuard {}

        unsafe extern "system" fn on_route_change(
            caller_context: *const c_void,
            _row: *const MIB_IPFORWARD_ROW2,
            notification_type: MIB_NOTIFICATION_TYPE,
        ) {
            if caller_context.is_null() {
                return;
            }
            let ctx = unsafe { &*(caller_context as *const Ctx) };
            let _ = ctx
                .tx
                .send((RawSignal::Route, format!("type={notification_type}")));
        }

        unsafe extern "system" fn on_addr_change(
            caller_context: *const c_void,
            _row: *const MIB_UNICASTIPADDRESS_ROW,
            notification_type: MIB_NOTIFICATION_TYPE,
        ) {
            if caller_context.is_null() {
                return;
            }
            let ctx = unsafe { &*(caller_context as *const Ctx) };
            let _ = ctx
                .tx
                .send((RawSignal::Addr, format!("type={notification_type}")));
        }

        unsafe extern "system" fn on_iface_change(
            caller_context: *const c_void,
            _row: *const MIB_IPINTERFACE_ROW,
            notification_type: MIB_NOTIFICATION_TYPE,
        ) {
            if caller_context.is_null() {
                return;
            }
            let ctx = unsafe { &*(caller_context as *const Ctx) };
            let _ = ctx
                .tx
                .send((RawSignal::Iface, format!("type={notification_type}")));
        }

        pub(in super::super) fn spawn(
            tx: UnboundedSender<(RawSignal, String)>,
        ) -> Option<BackendGuard> {
            let ctx = Box::into_raw(Box::new(Ctx { tx }));
            let mut handle: HANDLE = core::ptr::null_mut();
            let rc = unsafe {
                NotifyRouteChange2(
                    AF_UNSPEC,
                    Some(on_route_change),
                    ctx as *const c_void,
                    false,
                    &mut handle,
                )
            };
            if rc != NO_ERROR {
                tracing::warn!(rc, "netstate: NotifyRouteChange2 registration failed");
                drop(unsafe { Box::from_raw(ctx) });
                return None;
            }
            let mut addr_handle: HANDLE = core::ptr::null_mut();
            let rc = unsafe {
                NotifyUnicastIpAddressChange(
                    AF_UNSPEC,
                    Some(on_addr_change),
                    ctx as *const c_void,
                    false,
                    &mut addr_handle,
                )
            };
            if rc != NO_ERROR {
                tracing::warn!(
                    rc,
                    "netstate: NotifyUnicastIpAddressChange registration failed"
                );
                addr_handle = core::ptr::null_mut();
            }
            let mut iface_handle: HANDLE = core::ptr::null_mut();
            let rc = unsafe {
                NotifyIpInterfaceChange(
                    AF_UNSPEC,
                    Some(on_iface_change),
                    ctx as *const c_void,
                    false,
                    &mut iface_handle,
                )
            };
            if rc != NO_ERROR {
                tracing::warn!(rc, "netstate: NotifyIpInterfaceChange registration failed");
                iface_handle = core::ptr::null_mut();
            }
            Some(BackendGuard {
                handle,
                addr_handle,
                iface_handle,
                ctx,
            })
        }

        impl Drop for BackendGuard {
            fn drop(&mut self) {
                unsafe {
                    CancelMibChangeNotify2(self.handle);
                    if !self.addr_handle.is_null() {
                        CancelMibChangeNotify2(self.addr_handle);
                    }
                    if !self.iface_handle.is_null() {
                        CancelMibChangeNotify2(self.iface_handle);
                    }
                    drop(Box::from_raw(self.ctx));
                }
            }
        }
    }

    #[cfg(not(windows))]
    mod unix {
        use super::RawSignal;
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::sync::mpsc::UnboundedSender;

        pub(in super::super) struct BackendGuard {
            _child: tokio::process::Child,
            reader: tokio::task::JoinHandle<()>,
        }

        pub(in super::super) fn spawn(
            tx: UnboundedSender<(RawSignal, String)>,
        ) -> Option<BackendGuard> {
            let mut child = tokio::process::Command::new("ip")
                .args(["-o", "monitor", "route"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .ok()?;
            let stdout = child.stdout.take()?;
            let reader = tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send((RawSignal::Route, line)).is_err() {
                        break;
                    }
                }
            });
            Some(BackendGuard {
                _child: child,
                reader,
            })
        }

        impl Drop for BackendGuard {
            fn drop(&mut self) {
                self.reader.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn snap(ifaces: &[(&str, &[&str])], default_if: Option<&str>) -> NetSnapshot {
        let mut m = BTreeMap::new();
        for (name, addrs) in ifaces {
            m.insert(
                name.to_string(),
                IfaceSnap {
                    v4: addrs
                        .iter()
                        .map(|a| a.parse::<Ipv4Addr>().unwrap())
                        .collect(),
                    v6_count: 0,
                    vpn_class: super::super::direct::lan_iface_denied(name, ""),
                },
            );
        }
        NetSnapshot {
            ifaces: m,
            default_v4: default_if.map(|i| DefaultRoute {
                ifref: i.to_string(),
                gateway: Some("192.168.68.1".parse().unwrap()),
            }),
            default_v6: None,
        }
    }

    /// The severity contract: default-route movement or vanished addresses
    /// ⇒ Major (a VPN capture is BOTH); additions alone ⇒ Minor; identical
    /// snapshots (a self-inflicted route-churn burst) ⇒ no delta at all.
    #[test]
    fn diff_severity_contract() {
        let home = snap(&[("Wi-Fi", &["192.168.68.106"])], Some("12"));
        // VPN connect: default route moves to a new interface, address set intact.
        let captured = snap(
            &[
                ("Wi-Fi", &["192.168.68.106"]),
                ("Ethernet 3", &["10.138.80.110"]),
            ],
            Some("47"),
        );
        let d = diff(&home, &captured, true, true).expect("material change");
        assert_eq!(d.severity, Severity::Major);
        assert!(d.default_route_moved);
        assert!(d.saw_addr_signal && d.saw_iface_signal);

        // VPN disconnect: the vpn address vanishes ⇒ Major even if the
        // default-route sample momentarily reads the same.
        let d = diff(&captured, &home, true, false).expect("material change");
        assert_eq!(d.severity, Severity::Major);
        assert_eq!(d.addrs_removed, 1);

        // A new address with nothing else ⇒ Minor.
        let extra = snap(
            &[("Wi-Fi", &["192.168.68.106", "192.168.68.107"])],
            Some("12"),
        );
        let d = diff(&home, &extra, true, false).expect("material change");
        assert_eq!(d.severity, Severity::Minor);
        assert_eq!(d.addrs_added, 1);

        // Identical ⇒ None (the burst was our own route re-asserts).
        assert!(diff(&home, &home.clone(), false, false).is_none());
    }

    /// The sampler runs on the host without privileges and produces a
    /// consistent snapshot (smoke — content is host-specific).
    #[test]
    fn sample_snapshot_smoke() {
        let s = sample_snapshot();
        // At least a loopback-stripped view exists; both calls agree.
        assert_eq!(s.ifaces, sample_snapshot().ifaces);
    }
}
