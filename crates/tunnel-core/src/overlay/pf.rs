// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P1b — a Mac serves as an exit node or subnet router through pf.
//!
//! The macOS twin of the Linux iptables arm in [`super::nat`]: kernel NAT for
//! the overlay, its rules in our OWN anchor under the stock `com.apple/*`
//! wildcard, so `/etc/pf.conf` is never edited.
//!
//! Field-verified before any of this was written (FR-97 §8, 2026-10-16/17):
//! - a fresh macOS boot loads `/etc/pf.conf` and leaves pf disabled, and that
//!   main ruleset carries `nat-anchor "com.apple/*"` and `anchor "com.apple/*"`:
//!   rules in a sub-anchor of ours were evaluated once pf was enabled;
//! - `pfctl -E` enables pf and returns a reference token, and `pfctl -X <token>`
//!   releases it: pf went back to Disabled, as found. Never `pfctl -d`, which
//!   switches pf off under every other user of it (Internet Sharing, a VPN);
//! - `nat on <if> … -> (<if>)` follows the interface's address, but for `inet6`
//!   it round-robins over EVERY address, link-local included, so a v6 rule
//!   names a stable address instead (a global one first);
//! - a Mac whose primary service has no IPv6 has no UNSCOPED v6 default
//!   (every one is ifscoped), and a forwarded packet then has no route: v6 is
//!   served only where an unscoped default exists.
//!
//! The anchor holds one `nat` per egress interface (Linux's MASQUERADE has no
//! `-o` either), never onto another VPN's utun, and a `pass` for the overlay
//! adapter. Forwarding (`net.inet.ip.forwarding`, and the v6 one when v6 is
//! served) is turned on and put back as found, unless Internet Sharing runs:
//! it needs forwarding too, and AC5 says it keeps working.
//!
//! A crash skips every `Drop`, so each serve is recorded in the host ledger
//! (anchor, tokens, what forwarding was) and [`purge`] undoes it at boot and
//! after every exit of the daemon.

#[allow(unused_imports)]
use tracing::{info, warn};

#[cfg(any(target_os = "macos", test))]
use std::net::Ipv6Addr;

/// Our anchors live under the stock wildcard `/etc/pf.conf` already evaluates.
#[cfg(any(target_os = "macos", test))]
pub(crate) const ANCHOR_PREFIX: &str = "com.apple/roomler-";

/// Our anchor for the overlay adapter `if_name`: one per org's adapter.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn anchor_for(if_name: &str) -> String {
    format!("{ANCHOR_PREFIX}{if_name}")
}

/// Strict, because a name read back from the ledger goes into `pfctl -a`:
/// ours and nothing else, `com.apple/roomler-utun<1-3 digits>`.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn is_our_anchor(s: &str) -> bool {
    s.strip_prefix(ANCHOR_PREFIX)
        .and_then(|ifn| ifn.strip_prefix("utun"))
        .is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit()))
}

/// An interface name as pf takes it: lowercase letters, then digits (`en0`,
/// `utun4`, `bridge100`). Anything else never reaches a rule.
#[cfg(any(target_os = "macos", test))]
fn is_ifname(s: &str) -> bool {
    let digits = s.trim_start_matches(|c: char| c.is_ascii_lowercase());
    s.len() <= 15
        && !digits.is_empty()
        && digits.len() < s.len()
        && digits.chars().all(|c| c.is_ascii_digit())
}

/// A tunnel or loopback: never an egress for overlay traffic.
#[cfg(any(target_os = "macos", test))]
fn is_tunnel(s: &str) -> bool {
    ["utun", "ipsec", "ppp", "gif", "stf", "lo"]
        .iter()
        .any(|p| s.starts_with(p))
}

/// The interfaces overlay traffic leaves through and is NATed on: every
/// Ethernet-class (`en*`) one with an IPv4 address, plus the default route's
/// when it is not a tunnel. Never another VPN's utun: NAT onto it would hand
/// every peer a path into whatever that VPN reaches. Sorted, so the same
/// interfaces always render the same anchor.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn egress(with_v4: &[String], default_if: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = with_v4
        .iter()
        .filter(|n| n.starts_with("en") && is_ifname(n))
        .cloned()
        .collect();
    if let Some(d) = default_if
        && is_ifname(d)
        && !is_tunnel(d)
    {
        out.push(d.to_string());
    }
    out.sort();
    out.dedup();
    out
}

/// The address a v6 `nat` on an interface takes, from `ifconfig <if>`: a
/// stable global one, else a stable ULA; never link-local or temporary. Not
/// `(if)`: for `inet6` pf round-robins over every address, link-local
/// included (field, FR-97 §8, 2026-10-16).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn nat66_source(ifconfig: &str) -> Option<Ipv6Addr> {
    let stable: Vec<Ipv6Addr> = ifconfig
        .lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("inet6 ")?;
            if [
                "temporary",
                "deprecated",
                "detached",
                "tentative",
                "duplicated",
            ]
            .iter()
            .any(|f| rest.contains(f))
            {
                return None;
            }
            let a: Ipv6Addr = rest
                .split_whitespace()
                .next()?
                .split('%')
                .next()?
                .parse()
                .ok()?;
            let link_local = a.segments()[0] & 0xffc0 == 0xfe80;
            (!link_local && !a.is_loopback() && !a.is_unspecified() && !a.is_multicast())
                .then_some(a)
        })
        .collect();
    let ula = |a: &Ipv6Addr| a.segments()[0] & 0xfe00 == 0xfc00;
    // A global address first; a ULA only where the uplink has nothing else
    // (a network that NATs v6 upstream, as the FR-97 v6 cell's does).
    stable
        .iter()
        .find(|a| !ula(a))
        .or_else(|| stable.first())
        .copied()
}

/// Whether `netstat -rn -f inet6` holds an UNSCOPED default. A Mac whose
/// primary service has no v6 has only ifscoped ones (flags with `I`), and a
/// forwarded v6 packet then has no route (field, FR-97 §8, 2026-10-16).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn has_unscoped_v6_default(netstat: &str) -> bool {
    netstat.lines().any(|l| {
        let mut f = l.split_whitespace();
        f.next() == Some("default") && f.nth(1).is_some_and(|flags| !flags.contains('I'))
    })
}

/// The reference token `pfctl -E` prints: `Token : 13889181449326457865`.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_token(output: &str) -> Option<u64> {
    output.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().eq_ignore_ascii_case("token") {
            v.trim().parse().ok()
        } else {
            None
        }
    })
}

/// The anchor's text: per egress interface a `nat` for the overlay's IPv4
/// blocks, and one for its v6 ULA where v6 is served and the interface has a
/// stable address (a global one first, else a ULA); then a `pass` for the
/// overlay adapter. Translation rules first, as pf requires. Inputs that would
/// not parse as what they claim to be never reach a rule.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn render(
    overlay_if: &str,
    egress: &[(String, Option<Ipv6Addr>)],
    overlay_v4: &[String],
    serve_v6: bool,
) -> String {
    let blocks: Vec<String> = overlay_v4
        .iter()
        .filter_map(|c| c.parse::<ipnet::Ipv4Net>().ok())
        .map(|n| n.to_string())
        .collect();
    let mut out = String::new();
    if is_ifname(overlay_if) && !blocks.is_empty() {
        let from = if blocks.len() == 1 {
            blocks[0].clone()
        } else {
            format!("{{ {} }}", blocks.join(", "))
        };
        for (name, v6) in egress.iter().filter(|(n, _)| is_ifname(n)) {
            out.push_str(&format!(
                "nat on {name} inet from {from} to any -> ({name})\n"
            ));
            if serve_v6 && let Some(a) = v6 {
                out.push_str(&format!(
                    "nat on {name} inet6 from {} to any -> {a}\n",
                    super::router::OVERLAY_ULA_V6_CIDR
                ));
            }
        }
        out.push_str(&format!("pass quick on {overlay_if} all keep state\n"));
    }
    out
}

/// The macOS side effects: probe, serve, stop, re-assert, purge.
#[cfg(target_os = "macos")]
mod host {
    use super::{
        ANCHOR_PREFIX, Ipv6Addr, anchor_for, egress, has_unscoped_v6_default, is_ifname,
        is_our_anchor, nat66_source, parse_token, render,
    };
    use crate::overlay::host_ledger::PfRecord;
    use std::sync::{Arc, Mutex};
    use tracing::{info, warn};

    const PFCTL: &str = "/sbin/pfctl";
    const FWD4: &str = "net.inet.ip.forwarding";
    const FWD6: &str = "net.inet6.ip6.forwarding";
    /// How often a serving Mac re-checks its anchor and pf: another program's
    /// `pfctl -F all` or `-d`, or an interface that came or went.
    const REASSERT: std::time::Duration = std::time::Duration::from_secs(30);

    /// Run `pfctl`, `stdin` piped in; `(exit 0, stdout + stderr)`. pfctl
    /// prints its token and most notices on stderr.
    fn pfctl(args: &[&str], stdin: Option<&str>) -> (bool, String) {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let child = Command::new(PFCTL)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            return (false, format!("{PFCTL} could not start"));
        };
        if let (Some(text), Some(mut w)) = (stdin, child.stdin.take()) {
            let _ = w.write_all(text.as_bytes());
        }
        match child.wait_with_output() {
            Ok(o) => {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                (o.status.success(), s)
            }
            Err(e) => (false, e.to_string()),
        }
    }

    fn run(prog: &str, args: &[&str]) -> Option<String> {
        let o = std::process::Command::new(prog).args(args).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn sysctl_on(name: &str) -> Option<bool> {
        run("/usr/sbin/sysctl", &["-n", name]).map(|v| v.trim() != "0")
    }

    fn sysctl_set(name: &str, on: bool) -> bool {
        let kv = format!("{name}={}", if on { 1 } else { 0 });
        run("/usr/sbin/sysctl", &["-w", &kv]).is_some()
    }

    /// Internet Sharing turns forwarding on for itself; switching it off under
    /// it would cut the Macs and phones it serves.
    fn internet_sharing_running() -> bool {
        run("/usr/bin/pgrep", &["-x", "InternetSharing"]).is_some()
    }

    fn pf_enabled() -> bool {
        pfctl(&["-s", "info"], None)
            .1
            .lines()
            .any(|l| l.starts_with("Status: Enabled"))
    }

    /// Interfaces with an IPv4 address that are up and, where `ifconfig`
    /// reports one, `status: active`.
    fn interfaces_with_v4() -> Vec<(String, String)> {
        let Some(list) = run("/sbin/ifconfig", &["-l"]) else {
            return Vec::new();
        };
        list.split_whitespace()
            .filter(|n| is_ifname(n))
            .filter_map(|n| {
                let text = run("/sbin/ifconfig", &[n])?;
                let has_v4 = text.lines().any(|l| l.trim_start().starts_with("inet "));
                let inactive = text.lines().any(|l| l.trim() == "status: inactive");
                (has_v4 && !inactive).then(|| (n.to_string(), text))
            })
            .collect()
    }

    fn default_if() -> Option<String> {
        run("/sbin/route", &["-n", "get", "default"])?
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("interface:")
                    .map(|s| s.trim().to_string())
            })
    }

    fn unscoped_v6_default() -> bool {
        run("/usr/sbin/netstat", &["-rn", "-f", "inet6"])
            .is_some_and(|t| has_unscoped_v6_default(&t))
    }

    /// The egress interfaces with the v6 address each would NAT to.
    fn current_egress() -> Vec<(String, Option<Ipv6Addr>)> {
        let found = interfaces_with_v4();
        let names: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
        egress(&names, default_if().as_deref())
            .into_iter()
            .map(|n| {
                let v6 = found
                    .iter()
                    .find(|(m, _)| *m == n)
                    .and_then(|(_, text)| nat66_source(text));
                (n, v6)
            })
            .collect()
    }

    fn load(anchor: &str, rules: &str) -> Result<(), String> {
        let (ok, out) = pfctl(&["-a", anchor, "-f", "-"], Some(rules));
        if ok { Ok(()) } else { Err(first_line(&out)) }
    }

    fn flush(anchor: &str) {
        let _ = pfctl(&["-a", anchor, "-F", "all"], None);
    }

    fn enable() -> Option<u64> {
        parse_token(&pfctl(&["-E"], None).1)
    }

    fn release(token: u64) {
        let _ = pfctl(&["-X", &token.to_string()], None);
    }

    fn first_line(s: &str) -> String {
        s.lines()
            .map(str::trim)
            .find(|l| {
                !l.is_empty()
                    && !l.contains("ALTQ")
                    && !l.starts_with("Use of -f")
                    && !l.starts_with("present in the main")
                    && !l.starts_with("See /etc")
            })
            .unwrap_or("pfctl failed")
            .chars()
            .take(200)
            .collect()
    }

    /// Can this Mac serve? `Err` says why not, for the offer and `roomler status`.
    pub(crate) fn probe() -> Result<(), String> {
        if !crate::env::flag("PF_NAT", true) {
            return Err(
                "the macOS pf NAT is switched off (ROOMLERD_PF_NAT=0), so this Mac \
                        cannot be an exit node or a subnet router"
                    .into(),
            );
        }
        let (ok, out) = pfctl(&["-s", "nat"], None);
        if !ok {
            return Err(format!(
                "pf is not usable here ({}): serving needs the root daemon",
                first_line(&out)
            ));
        }
        if !out.contains("nat-anchor \"com.apple/*\"") {
            return Err(
                "pf's main ruleset does not evaluate the com.apple/* anchors, and this \
                        Mac does not edit /etc/pf.conf, so it cannot NAT for its peers"
                    .into(),
            );
        }
        Ok(())
    }

    /// FR-97 P1b — this process's hold on forwarding: two orgs' routers share
    /// it, and only the last to stop puts it back. The FIRST holder's reading
    /// decides whether it was ours to turn on.
    struct Fwd {
        v4_holders: usize,
        v4_restore: bool,
        v6_holders: usize,
        v6_restore: bool,
    }
    static FWD: Mutex<Fwd> = Mutex::new(Fwd {
        v4_holders: 0,
        v4_restore: false,
        v6_holders: 0,
        v6_restore: false,
    });

    /// One served adapter. `stop` (from the guard's `Drop`) takes it all back.
    pub(crate) struct Serve {
        anchor: String,
        v6: bool,
        live: Arc<Mutex<Live>>,
        task: Option<tokio::task::JoinHandle<()>>,
    }

    /// What the re-assert task and `stop` share. Every pfctl call of either
    /// happens under this lock, and `stopped` ends the task's work: aborting a
    /// task does not stop a blocking closure already running, which would
    /// otherwise load the anchor again, or take a token, after `stop`.
    struct Live {
        rules: String,
        tokens: Vec<u64>,
        stopped: bool,
    }

    fn record(anchor: &str, live: &Live, v4_restore: bool, v6_restore: bool) {
        crate::overlay::host_ledger::record_pf(PfRecord {
            anchor: anchor.to_string(),
            tokens: live.tokens.clone(),
            v4_was_off: v4_restore,
            v6_was_off: v6_restore,
        });
    }

    /// Serve the overlay adapter `overlay_if` for the IPv4 blocks `v4`.
    /// `(fully ok, the serve to stop later)`; `None` only when nothing at all
    /// could be set up.
    pub(crate) async fn serve(overlay_if: &str, v4: &[String]) -> (bool, Option<Serve>) {
        let overlay_if = overlay_if.to_string();
        let v4 = v4.to_vec();
        let started = tokio::task::spawn_blocking(move || {
            let anchor = anchor_for(&overlay_if);
            let serve_v6 = unscoped_v6_default();
            let (v4_restore, v6_restore) = {
                let mut f = FWD.lock().unwrap_or_else(|e| e.into_inner());
                if f.v4_holders == 0 {
                    f.v4_restore = sysctl_on(FWD4) == Some(false);
                }
                f.v4_holders += 1;
                if serve_v6 {
                    if f.v6_holders == 0 {
                        f.v6_restore = sysctl_on(FWD6) == Some(false);
                    }
                    f.v6_holders += 1;
                }
                (f.v4_restore, serve_v6 && f.v6_restore)
            };
            let mut ok = sysctl_set(FWD4, true);
            if serve_v6 {
                ok &= sysctl_set(FWD6, true);
            } else {
                info!("overlay: pf exit — no unscoped IPv6 default route here, so IPv6 is not served (clients stay v6 fail-closed)");
            }
            let rules = render(&overlay_if, &current_egress(), &v4, serve_v6);
            if let Err(e) = load(&anchor, &rules) {
                warn!(%anchor, err = %e, "overlay: pf anchor load failed");
                ok = false;
            }
            let mut live = Live {
                rules,
                tokens: Vec::new(),
                stopped: false,
            };
            match enable() {
                Some(t) => live.tokens.push(t),
                None => {
                    warn!("overlay: pfctl -E returned no token; pf may not be enabled");
                    ok = false;
                }
            }
            record(&anchor, &live, v4_restore, v6_restore);
            info!(%anchor, rules = %live.rules.trim().replace('\n', " | "), "overlay: pf NAT for the overlay loaded");
            (ok, anchor, serve_v6, live, v4)
        })
        .await;
        let Ok((ok, anchor, serve_v6, live, v4)) = started else {
            return (false, None);
        };
        let overlay_if = anchor.trim_start_matches(ANCHOR_PREFIX).to_string();
        let live = Arc::new(Mutex::new(live));
        let task = tokio::spawn(reassert(
            anchor.clone(),
            overlay_if,
            v4,
            serve_v6,
            live.clone(),
        ));
        (
            ok,
            Some(Serve {
                anchor,
                v6: serve_v6,
                live,
                task: Some(task),
            }),
        )
    }

    /// Keep the anchor and pf as served: another program may flush all rules
    /// or disable pf, and an interface may come or go (a re-render follows
    /// the new set). Logs only what it changes.
    async fn reassert(
        anchor: String,
        overlay_if: String,
        v4: Vec<String>,
        v6: bool,
        live: Arc<Mutex<Live>>,
    ) {
        loop {
            tokio::time::sleep(REASSERT).await;
            let anchor = anchor.clone();
            let overlay_if = overlay_if.clone();
            let v4 = v4.clone();
            let live = live.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut l = live.lock().unwrap_or_else(|e| e.into_inner());
                if l.stopped {
                    return;
                }
                let rules = render(&overlay_if, &current_egress(), &v4, v6);
                let present = pfctl(&["-a", &anchor, "-s", "nat"], None).1.contains("nat on ");
                if rules != l.rules || !present {
                    match load(&anchor, &rules) {
                        Ok(()) => info!(%anchor, flushed = !present, rules = %rules.trim().replace('\n', " | "), "overlay: pf anchor re-asserted"),
                        Err(e) => warn!(%anchor, err = %e, "overlay: pf anchor re-assert failed"),
                    }
                    l.rules = rules;
                }
                if !pf_enabled() && let Some(t) = enable() {
                    warn!("overlay: pf was disabled under a serving exit — enabled it again");
                    l.tokens.push(t);
                    let f = FWD.lock().unwrap_or_else(|e| e.into_inner());
                    record(&anchor, &l, f.v4_restore, v6 && f.v6_restore);
                }
            })
            .await;
        }
    }

    impl Serve {
        /// Flush the anchor, release every token we took (never `pfctl -d`),
        /// and put forwarding back as found once the last holder stops.
        pub(crate) fn stop(mut self) {
            if let Some(t) = self.task.take() {
                t.abort();
            }
            // Under the lock: waits out a re-assert already running, and no
            // later one does anything.
            let mut l = self.live.lock().unwrap_or_else(|e| e.into_inner());
            l.stopped = true;
            flush(&self.anchor);
            for t in std::mem::take(&mut l.tokens) {
                release(t);
            }
            drop(l);
            let sharing = internet_sharing_running();
            let mut f = FWD.lock().unwrap_or_else(|e| e.into_inner());
            f.v4_holders = f.v4_holders.saturating_sub(1);
            if f.v4_holders == 0 && f.v4_restore && !sharing {
                sysctl_set(FWD4, false);
            }
            if self.v6 {
                f.v6_holders = f.v6_holders.saturating_sub(1);
                if f.v6_holders == 0 && f.v6_restore && !sharing {
                    sysctl_set(FWD6, false);
                }
            }
            crate::overlay::host_ledger::forget_pf(&self.anchor);
            info!(anchor = %self.anchor, internet_sharing = sharing, "overlay: pf NAT for the overlay reverted");
        }
    }

    /// The boot / after-exit purge: what a crashed serve left.
    pub(crate) fn purge(records: &[PfRecord]) {
        let ours: Vec<&PfRecord> = records
            .iter()
            .filter(|r| is_our_anchor(&r.anchor))
            .collect();
        if ours.is_empty() {
            return;
        }
        let sharing = internet_sharing_running();
        for r in &ours {
            flush(&r.anchor);
            for t in &r.tokens {
                release(*t);
            }
        }
        if !sharing {
            if ours.iter().any(|r| r.v4_was_off) && sysctl_on(FWD4) == Some(true) {
                sysctl_set(FWD4, false);
            }
            if ours.iter().any(|r| r.v6_was_off) && sysctl_on(FWD6) == Some(true) {
                sysctl_set(FWD6, false);
            }
        }
        warn!(
            anchors = ?ours.iter().map(|r| r.anchor.as_str()).collect::<Vec<_>>(),
            internet_sharing = sharing,
            "overlay: a pf exit left its anchor (an exit that skipped its revert) — flushed it, released its pf reference and put forwarding back"
        );
    }
}

#[cfg(target_os = "macos")]
pub(crate) use host::{Serve, probe, serve};

/// The pf half of [`super::host_ledger::purge`]. No-op off macOS.
pub(crate) fn purge(records: &[super::host_ledger::PfRecord]) {
    #[cfg(target_os = "macos")]
    host::purge(records);
    #[cfg(not(target_os = "macos"))]
    let _ = records;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_anchor_is_one_level_under_the_stock_wildcard_and_nothing_else() {
        assert_eq!(anchor_for("utun4"), "com.apple/roomler-utun4");
        assert!(is_our_anchor("com.apple/roomler-utun4"));
        assert!(is_our_anchor("com.apple/roomler-utun123"));
        for bad in [
            "com.apple/roomler-utun",
            "com.apple/roomler-utun1234",
            "com.apple/roomler-en0",
            "com.apple/internet-sharing",
            "com.apple/roomler-utun4; pfctl -d",
            "com.apple/*",
            "roomler-utun4",
        ] {
            assert!(!is_our_anchor(bad), "{bad}");
        }
    }

    #[test]
    fn egress_is_every_ethernet_class_interface_plus_a_non_tunnel_default() {
        let up = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            egress(
                &up(&["en0", "en7", "lo0", "utun4", "bridge100"]),
                Some("en0")
            ),
            vec!["en0", "en7"]
        );
        // a default that is not `en*` but is real joins
        assert_eq!(
            egress(&up(&["en0"]), Some("bridge0")),
            vec!["bridge0", "en0"]
        );
        // never another VPN's tunnel, even as the default
        assert_eq!(egress(&up(&["en0"]), Some("utun3")), vec!["en0"]);
        assert_eq!(egress(&up(&["en0"]), Some("ppp0")), vec!["en0"]);
        // junk never reaches a rule
        assert_eq!(
            egress(&up(&["en0;x", "EN1", "en"]), Some("en0 ")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_v6_nat_target_is_stable_global_first_never_link_local_or_temporary() {
        let ifc = "en7: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
                   \tinet6 fe80::1448:e811:8eee:e167%en7 prefixlen 64 secured scopeid 0x20\n\
                   \tinet6 fd00:ac4::2 prefixlen 64\n\
                   \tinet6 2a01:5a8:51b:fb6b:1c2a:3b4c:5d6e:7f80 prefixlen 64 autoconf temporary\n\
                   \tinet6 2a01:5a8:51b:fb6b:aaaa:bbbb:cccc:dddd prefixlen 64 autoconf secured\n\
                   \tinet 172.20.10.2 netmask 0xfffffff0 broadcast 172.20.10.15\n";
        // the global wins over a ULA listed before it; the temporary never counts
        assert_eq!(
            nat66_source(ifc),
            Some("2a01:5a8:51b:fb6b:aaaa:bbbb:cccc:dddd".parse().unwrap())
        );
        // a ULA where the uplink has nothing else (v6 NATed upstream)
        assert_eq!(
            nat66_source("\tinet6 fe80::1%en0 prefixlen 64\n\tinet6 fd00:ac4::2 prefixlen 64\n"),
            Some("fd00:ac4::2".parse().unwrap())
        );
        // link-local alone, or no v6 at all: nothing to NAT to
        assert_eq!(
            nat66_source("\tinet6 fe80::1%en0 prefixlen 64 secured scopeid 0x4\n"),
            None
        );
        assert_eq!(
            nat66_source("\tinet 192.168.8.106 netmask 0xffffff00\n"),
            None
        );
    }

    #[test]
    fn only_an_unscoped_v6_default_serves_v6() {
        // the field Mac: Wi-Fi primary without v6, the iPhone's v6 default ifscoped
        let scoped = "Routing tables\n\nInternet6:\nDestination        Gateway                        Flags         Netif Expire\n\
                      default            fe80::%utun1                   UGcIg         utun1\n\
                      default            fe80::30e0:4fff:fe04:ac64%en7  UGcIg           en7\n";
        assert!(!has_unscoped_v6_default(scoped));
        let unscoped = "Internet6:\nDestination        Gateway                        Flags         Netif Expire\n\
                        default            fe80::1%en0                    UGc             en0\n";
        assert!(has_unscoped_v6_default(unscoped));
        assert!(!has_unscoped_v6_default(""));
    }

    #[test]
    fn the_token_pfctl_e_prints_is_read_back() {
        assert_eq!(
            parse_token("pf enabled\nToken : 13889181449326457865\n"),
            Some(13889181449326457865)
        );
        assert_eq!(
            parse_token("No ALTQ support in kernel\nToken : 7\n"),
            Some(7)
        );
        assert_eq!(parse_token("pfctl: /dev/pf: Permission denied\n"), None);
    }

    #[test]
    fn the_anchor_nats_every_egress_and_passes_the_overlay_adapter() {
        let v6: Ipv6Addr = "2a01:5a8:51b:fb6b:aaaa:bbbb:cccc:dddd".parse().unwrap();
        let eg = vec![("en0".to_string(), None), ("en7".to_string(), Some(v6))];
        let one = render("utun4", &eg, &["100.65.20.0/24".into()], true);
        assert_eq!(
            one,
            "nat on en0 inet from 100.65.20.0/24 to any -> (en0)\n\
             nat on en7 inet from 100.65.20.0/24 to any -> (en7)\n\
             nat on en7 inet6 from fd72:6f6f:6d6c::/96 to any -> 2a01:5a8:51b:fb6b:aaaa:bbbb:cccc:dddd\n\
             pass quick on utun4 all keep state\n"
        );
        // v6 not served: no inet6 rule even where an address exists
        assert!(!render("utun4", &eg, &["100.65.20.0/24".into()], false).contains("inet6"));
        // a grown org: every block (FR-47 P5e)
        assert!(
            render(
                "utun4",
                &eg[..1],
                &["100.65.20.0/24".into(), "100.65.21.0/24".into()],
                false
            )
            .starts_with(
                "nat on en0 inet from { 100.65.20.0/24, 100.65.21.0/24 } to any -> (en0)\n"
            )
        );
    }

    #[test]
    fn nothing_that_does_not_parse_reaches_a_rule() {
        let eg = vec![("en0".to_string(), None)];
        assert_eq!(
            render(
                "utun4",
                &eg,
                &["100.65.20.0/24 to any -> (en0)\npass all".into()],
                false
            ),
            ""
        );
        assert_eq!(
            render("utun4;x", &eg, &["100.65.20.0/24".into()], false),
            ""
        );
        assert_eq!(
            render(
                "utun4",
                &[("en0 inet".to_string(), None)],
                &["100.65.20.0/24".into()],
                false
            ),
            "pass quick on utun4 all keep state\n"
        );
    }
}

/// FR-97 P1b — on a real Mac, as root: CI's `macos_kernel_tests` step runs
/// these under `sudo` with `ROOMLER_TUN_KERNEL_TEST=1`.
#[cfg(all(test, target_os = "macos"))]
mod macos_kernel_tests {
    /// `Enabled` or `Disabled`: the word alone. The line around it changes
    /// with a toggle (`Disabled for 0 days 00:00:00` once pf has been on),
    /// which CI's first run compared and failed on with pf correctly off.
    fn pf_status() -> String {
        let o = std::process::Command::new("/sbin/pfctl")
            .args(["-s", "info"])
            .output()
            .expect("pfctl");
        let s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.lines()
            .find(|l| l.starts_with("Status:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string()
    }

    fn anchor_nat(anchor: &str) -> String {
        let o = std::process::Command::new("/sbin/pfctl")
            .args(["-a", anchor, "-s", "nat"])
            .output()
            .expect("pfctl");
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn fwd4() -> String {
        let o = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "net.inet.ip.forwarding"])
            .output()
            .expect("sysctl");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn a_served_anchor_nats_and_pf_and_forwarding_are_left_as_found() {
        if std::env::var("ROOMLER_TUN_KERNEL_TEST").is_err() {
            return;
        }
        super::probe().expect("this runner's pf evaluates com.apple/* (the stock pf.conf)");
        let (status0, fwd0) = (pf_status(), fwd4());
        let anchor = super::anchor_for("utun199");
        let (ok, serve) = super::serve("utun199", &["100.64.0.0/10".to_string()]).await;
        let serve = serve.expect("a serve to stop");
        assert!(ok, "the serve set up fully");
        assert_eq!(pf_status(), "Enabled", "pf enabled by our token");
        assert!(
            anchor_nat(&anchor).contains("nat on "),
            "our anchor holds its nat: {}",
            anchor_nat(&anchor)
        );
        assert_eq!(fwd4(), "1", "forwarding on while serving");
        serve.stop();
        assert_eq!(anchor_nat(&anchor).trim(), "", "our anchor is empty again");
        assert_eq!(pf_status(), status0, "pf back as found");
        assert_eq!(fwd4(), fwd0, "forwarding back as found");
    }
}
