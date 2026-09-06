// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
// RETIRED-NAME-ANCHOR(2): this line EXISTS to explain the retired spelling, so it must
// contain it.
//! `roomler status | peers | flows` — the thin-client read verbs.
//!
//! These connect to the **local** daemon's LocalAPI (the ACL-gated named pipe /
//! unix socket the daemon binds — see `tunnel_core::localapi`) and render its
//! live node / peer / flow state. Purely a *client* of
//! [`tunnel_core::localapi::Client`]: no server, no token, no config — the OS
//! endpoint ACL is the trust boundary. This is the CLI half of the
//! unification's "thin clients over the LocalAPI" story; at the P3d rename
//! `roomler-tunnel <verb>` becomes `roomler <verb>`.
//!
//! Everything below the command handlers is a **pure** formatter (`now_ms` is
//! injected, never read from the clock) so the table rendering is unit-tested
//! with no live daemon.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use tunnel_core::localapi::{
    self, ConnectionType, DaemonMode, FlowInfo, FlowKind, NodeStatus, PeerInfo, RouteInfo,
    RouteState,
};

/// Em-dash for an absent / null field — matches the tray's `devices.js`
/// convention so the two surfaces read the same.
const DASH: &str = "—";

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// `roomler status` — the local node's own state (id, version, mode, overlay
/// IP, server connection). Renders [`NodeStatus`] ONLY: `status --json` is
/// exactly that struct, never a peers fan-out.
pub async fn status(json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let status = client.status().await.map_err(daemon_err)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status(&status);
        print_other_daemon_hint();
    }
    Ok(())
}

/// Say so when this host runs a SECOND daemon we did not answer for.
///
/// On macOS the two halves are separate processes on separate sockets, and the
/// unprivileged CLI always reaches the per-user one — which has no overlay. So
/// `status` prints `overlay ip —` and `peers` prints nothing, both perfectly
/// true of the process reached and both read as "the overlay is broken" when
/// the root half is meshed and healthy. Field-hit on a MacBook, 2026-08-24.
///
/// Only printed in human mode: `--json` output is parsed, and a stray line
/// would be a breaking change for anything consuming it.
fn print_other_daemon_hint() {
    if let Some(other) = localapi::other_daemon_socket() {
        eprintln!();
        eprintln!(
            "note: a second roomler daemon is running on this host ({}).",
            other.display()
        );
        eprintln!("      This is the per-user half; the overlay runs in the privileged one.");
        eprintln!("      Use `sudo roomler …` to ask that one instead.");
    }
}

/// `roomler logs` — surface the S2 `TailLog` verb on the CLI.
///
/// The verb and the daemon side already existed; without a command in front of
/// them the only way to read a remote agent's log was to guess its path through
/// a shell, and that path is genuinely hard: it depends on whether the process
/// carries a service role and, failing that, on which USER it runs as. A
/// Windows host therefore has three plausible `roomlerd.log*` files and two are
/// decoys — the SCM supervisor's (~30 lines/day, no overlay events) and the
/// updater's (a few KB, written only at update time). On 2026-08-07 that cost a
/// wrong "the agent's logging is dead" conclusion.
///
/// The daemon opened the file, so it is the only thing that reliably knows.
/// Composes with Fleet RPC: `roomler exec <host> -- roomler logs --grep ICE`.
/// The resolved path is always printed — remotely, that IS half the answer.
pub async fn logs(
    source: String,
    max_bytes: Option<u64>,
    grep: Option<String>,
    lines: Option<usize>,
    json: bool,
) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let (path, size, content) = client
        .tail_log(&source, max_bytes)
        .await
        .map_err(daemon_err)?;
    // Filter client-side: the daemon's verb is a byte-bounded tail, and
    // narrowing there would silently change what "the last N bytes" means.
    let body: String = match &grep {
        Some(g) => {
            let needle = g.to_lowercase();
            content
                .lines()
                .filter(|l| l.to_lowercase().contains(&needle))
                .collect::<Vec<_>>()
                .join("\n")
        }
        None => content,
    };
    // `-n` trims AFTER the grep, so `--grep X -n 20` reads as "the last 20
    // matching lines" — same order as a shell `grep X | tail -n 20`.
    let body: String = match lines {
        Some(n) => {
            let all: Vec<&str> = body.lines().collect();
            all[all.len().saturating_sub(n)..].join("\n")
        }
        None => body,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path, "size": size, "content": body,
            }))?
        );
        return Ok(());
    }
    println!("{path}  ({size} bytes total)");
    if body.is_empty() {
        println!(
            "(no matching lines{})",
            grep.map(|g| format!(" for {g:?}")).unwrap_or_default()
        );
    } else {
        println!("{body}");
    }
    Ok(())
}

/// `roomler netcheck` — the measured capability vector (overlay v3 B4),
/// read from the daemon's status payload. What selection actually keys on,
/// plus how old the measurement is.
pub async fn netcheck(json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let status = client.status().await.map_err(daemon_err)?;
    match status.netcheck {
        Some(nc) if json => println!("{}", serde_json::to_string_pretty(&nc)?),
        Some(nc) => {
            let band = match nc.relay_band_udp {
                Some(true) => "reachable",
                Some(false) => "BLOCKED",
                None => "unmeasured",
            };
            println!(
                "stun/udp:        {}",
                if nc.stun_udp { "ok" } else { "NO MAPPING" }
            );
            println!("relay band/udp:  {band}");
            println!(
                "derp floor ws:   {}",
                if nc.derp_ws_ok { "up" } else { "DOWN" }
            );
            println!(
                "nat:             {}",
                nc.nat.as_deref().unwrap_or("untyped")
            );
            println!("measured:        {}s ago (fresh < 3600s)", nc.age_s);
        }
        None if json => println!("null"),
        None => println!(
            "no measurement yet — the daemon probes ~45 s after start (older daemons never do)"
        ),
    }
    Ok(())
}

/// `roomler peers` — every peer this node sees, with its live connection type.
///
/// FR-49 — `org` scopes the output to ONE enrollment. Not only ergonomics: on a
/// device in a customer org and a personal one this prints every org's node
/// names together, which makes it unusable on a shared screen or in a bug
/// report. And the orgs are fetched (not just the peers) so that an enrollment
/// with its overlay OFF can be SHOWN as such: it has no peers, so it used to
/// produce no section at all — indistinguishable from an org whose peers merely
/// happen to be offline.
pub async fn peers(json: bool, org: Option<String>) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let peers = client.peers().await.map_err(daemon_err)?;
    let peers: Vec<PeerInfo> = match &org {
        Some(want) => peers.into_iter().filter(|p| &p.org == want).collect(),
        None => peers,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&peers)?);
        return Ok(());
    }
    // A second round trip on a local pipe, for the one thing the peer list
    // cannot say: which enrollments exist but contribute no peers, and why.
    let orgs = client
        .status()
        .await
        .map(|s| s.orgs)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| org.as_deref().is_none_or(|w| o.label == w))
        .collect::<Vec<_>>();
    if let Some(want) = &org
        && orgs.is_empty()
        && peers.is_empty()
    {
        println!("No enrollment labelled {want:?} — see `roomlerd org ls`.");
        return Ok(());
    }
    print_peers(&peers, now_ms());
    print_dark_orgs(&orgs, &peers);
    print_other_daemon_hint();
    Ok(())
}

/// FR-49 — name the enrollments that contribute no peers BECAUSE they are not
/// on a mesh, which `print_peers` cannot do: it only ever sees peers, and an
/// overlay-off org has none.
///
/// ⚠️ Only for an org with **no rows at all**. An org that has peers is already
/// visible, and an org whose overlay is on but whose peers are all offline is a
/// different state that must keep looking different.
///
/// The selection is [`dark_orgs`] so it can be locked by a test: the whole
/// point is which orgs are and are not in it, and getting that wrong rebuilds
/// the ambiguity this exists to remove.
fn print_dark_orgs(orgs: &[tunnel_core::localapi::OrgStatus], peers: &[PeerInfo]) {
    let dark = dark_orgs(orgs, peers);
    if dark.is_empty() {
        return;
    }
    for o in dark {
        println!();
        println!("  ── org: {} ──", o.label);
        println!(
            "  (overlay OFF — this enrollment is not on a mesh, so it has no peers. \
             It is not \"no peers online\".)"
        );
        println!(
            "  join it with: roomlerd org overlay {} netstack   (then restart the daemon)",
            o.label
        );
    }
}

/// `roomler why <peer>` — F: explain, for ONE pair, why it rides the carrier
/// it rides.
///
/// Everything printed here was already being computed by the selector on every
/// sweep and was readable nowhere. On 2026-08-26 answering "why is this Mac on
/// relay when it is on the same Wi-Fi?" needed a `tcpdump` on one host and log
/// archaeology on the other; worse, the decisive record — the demote-follow —
/// carries only `peer=<node id>`, so an earlier search by overlay IP returned
/// nothing and produced a confident WRONG answer. This command exists so that
/// question costs one command on either end.
pub async fn why(peer: &str, json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let peers = client.peers().await.map_err(daemon_err)?;
    let needle = peer.to_ascii_lowercase();
    // Match on name, overlay IP or node id, and accept a unique prefix of the
    // name — an operator reads the name out of `peers` and should not have to
    // retype `macbook-1-local-daemon` exactly.
    let matches: Vec<_> = peers
        .iter()
        .filter(|p| {
            p.node_id.eq_ignore_ascii_case(&needle)
                || p.overlay_ip.as_deref() == Some(peer)
                || p.overlay_ip6.as_deref() == Some(peer)
                || p.name.to_ascii_lowercase().contains(&needle)
        })
        .collect();
    let p = match matches.as_slice() {
        [one] => *one,
        [] => {
            anyhow::bail!(
                "no peer matches {peer:?} — `roomler peers` lists them by name, overlay IP and node id"
            )
        }
        many => {
            // Name alone is NOT a disambiguator on this fleet: one physical
            // host enrolled in two orgs appears once per org under the SAME
            // name with different overlay IPs, so "matches 2 peers: mars,
            // mars" is true and useless. Print what the caller can retype.
            let rows: Vec<String> = many
                .iter()
                .map(|p| {
                    let ip = p.overlay_ip.as_deref().unwrap_or("no-ip");
                    match p.org.as_str() {
                        "" => format!("{} ({ip})", p.name),
                        org => format!("{} ({ip}, org {org})", p.name),
                    }
                })
                .collect();
            anyhow::bail!(
                "{peer:?} matches {} peers — pass one of: {}",
                many.len(),
                rows.join(" | ")
            )
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(p)?);
        return Ok(());
    }
    print_why(p);
    print_other_daemon_hint();
    Ok(())
}

fn print_why(p: &PeerInfo) {
    let ip = p.overlay_ip.as_deref().unwrap_or("—");
    println!("{}  {}", p.name, ip);
    let carried = match (&p.connection, p.relay_kind.as_deref()) {
        (ConnectionType::Direct, _) => "direct".to_string(),
        (ConnectionType::Relay, Some(k)) => format!("relay:{k}"),
        (c, _) => format!("{c:?}").to_ascii_lowercase(),
    };
    let via = p
        .debug
        .as_ref()
        .and_then(|d| d.dst.as_deref())
        .map(|d| format!(" via {d}"))
        .unwrap_or_default();
    println!("  carrier   {carried}{via}");
    if let Some(d) = &p.debug {
        println!(
            "  tier      {}   handshake {}   tx {} / rx {}   last heard {}s ago",
            d.tier,
            if d.hs_done { "done" } else { "NOT DONE" },
            d.tx,
            d.rx,
            d.last_rx_age_s
        );
        // A one-way carrier is the single most misread state in the field: the
        // CONN column looks healthy while nothing arrives.
        if d.tx > 0 && d.rx == 0 {
            println!("            ^ ONE-WAY: we are sending and receiving nothing");
        }
    }
    // A — what the prober actually measured, printed next to the decision.
    // A clean path the selector will not use is the single most useful thing
    // this command can show, and it is only visible if the two sit together.
    if !p.probes.is_empty() {
        println!();
        println!("  MEASURED PATH          LOSS    RTT     p95     max");
        for pr in &p.probes {
            let pct = |v: Option<f64>| {
                v.map(|v| format!("{:.0}%", v * 100.0))
                    .unwrap_or_else(|| "—".into())
            };
            let ms = |v: Option<f64>| v.map(|v| format!("{v:.0}ms")).unwrap_or_else(|| "—".into());
            println!(
                "  {:<22} {:>5}  {:>6}  {:>6}  {:>6}",
                pr.dst,
                pct(pr.loss),
                ms(pr.rtt_ms),
                ms(pr.rtt_p95_ms),
                ms(pr.rtt_max_ms)
            );
        }
        // An unmeasured path must never read as a bad one.
        if p.probes.iter().all(|pr| pr.loss.is_none()) {
            println!("  (— = not enough rounds yet to judge, NOT a bad path)");
        }
    }
    let Some(w) = &p.why else {
        println!("  (this daemon predates `why` — upgrade it to see the path decision)");
        return;
    };
    println!();
    println!("  TIER      ELIGIBLE  SCORE   = BASE  + Q      - PENALTY   WHY NOT");
    for t in &w.tiers {
        println!(
            "  {:<9} {:<9} {:>6.1} = {:>5.0} {:>+7.1} {:>10.1}   {}",
            t.tier,
            if t.eligible { "yes" } else { "no" },
            t.score,
            t.base,
            t.q,
            t.penalty,
            t.blocked_by.as_deref().unwrap_or("")
        );
    }
    println!();
    // The hold-downs, spelled out. Each of these is a legitimate reason the
    // pair is NOT on the tier its raw scores would pick, and each has a
    // different fix, so naming them is the whole point of the command.
    if let Some(s) = w.relayed_instead_s {
        println!(
            "  HELD DOWN {s}s more: the peer is relaying to us over /derp while we hold\n\
             \x20           another carrier, so we follow it rather than fight it (strike {}).\n\
             \x20           Every direct tier is ineligible regardless of its own health.\n\
             \x20           If the peer can reach us directly, the fix is at the PEER.",
            w.relayed_instead_strikes
        );
    } else if w.relayed_instead_strikes > 0 {
        println!(
            "  {} recent demote-follow(s), window expired — the two ends have been\n\
             \x20           disagreeing about this path.",
            w.relayed_instead_strikes
        );
    }
    if let Some(s) = w.forced_derp_s {
        // Deliberately spelled out. The pin selects which RELAY FLAVOUR this
        // pair uses; it does NOT hold the pair off a direct tier —
        // `PathMonitor` keeps it as an annotation and direct decisions ignore
        // it entirely. Read live on 2026-08-26 against a pair that was
        // DIRECT at the time with 1311 s still on the pin, and the first
        // wording ("forced this pair onto /derp") invited exactly the
        // misreading this whole command exists to prevent: blaming a visible
        // knob for a landing it did not cause.
        println!(
            "  PINNED    {s}s more: the server pins this pair's RELAY flavour to /derp.\n\
             \x20           This does NOT hold the pair off a direct tier — if it is on\n\
             \x20           relay, look above for the reason, not here."
        );
    }
    if w.tiers
        .iter()
        .any(|t| t.blocked_by.as_deref() == Some("lan-captured"))
    {
        // FR-33 P2 — a fact about THIS host, not the path: spelled out so the
        // operator goes to the VPN profile, not to path tuning.
        println!(
            "  CAPTURED  this host's own LAN prefix is routed through another adapter (a\n\
             \x20           VPN client's split-prefix capture), so a LAN dial cannot work and\n\
             \x20           is not attempted — `roomler status` names the adapter. The fix is\n\
             \x20           in the VPN profile (local-LAN access / split-exclude), not here."
        );
    }
    if let Some(t) = &w.probing {
        println!("  PROBING   a direct upgrade on {t} is in flight right now.");
    }
    if w.relayed_instead_s.is_none()
        && w.forced_derp_s.is_none()
        && w.tiers.iter().all(|t| t.eligible)
    {
        println!("  No hold-down is active: every tier is eligible.");
    }
}

/// `roomler flows` — active forwards / SOCKS5 listeners + throughput. Empty on
/// today's agent daemon until the tunnel data plane folds in (P3b).
pub async fn flows(json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let flows = client.flows().await.map_err(daemon_err)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&flows)?);
    } else {
        print_flows(&flows);
    }
    Ok(())
}

/// `roomler rename <name>` — persist a new device name through the running
/// daemon (it writes its OWN config, so this works unelevated even when the
/// daemon is a SYSTEM service). Announced on the next server reconnect. An old
/// daemon that predates the verb reports a clean error.
pub async fn rename(name: &str) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let effective = client.set_device_name(name).await.map_err(daemon_err)?;
    println!("device renamed to \u{201c}{effective}\u{201d} — announced on the next reconnect");
    Ok(())
}

/// `roomler ping <target> [-6] [--timeout-ms N]` — ICMP-ping an overlay peer (by
/// name or IP) over the userspace netstack: the OS-free reachability probe for a
/// locked-down host with no OS route to the mesh. Meaningful on a netstack node;
/// other daemons reply "not supported". The daemon's own error (unknown peer /
/// timeout / not-a-netstack-node) is surfaced verbatim — only a *connect* failure
/// maps through [`daemon_err`].
pub async fn ping(target: &str, timeout_ms: u64, prefer_v6: bool, json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let (overlay_ip, rtt_ms) = client
        .ping(target, timeout_ms, prefer_v6)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "target": target, "overlay_ip": overlay_ip, "rtt_ms": rtt_ms })
        );
    } else {
        println!("{target} ({overlay_ip}): {rtt_ms:.1} ms");
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Fleet RPC — `roomler exec` / `roomler diag`
// ────────────────────────────────────────────────────────────────────────────

/// One remote command's result, in the shape the printers want.
struct RemoteRun {
    node: String,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    duration_ms: u64,
    error: Option<String>,
}

/// Ask the local daemon to run `command` on `node` and wait for the answer.
async fn run_remote(node: &str, shell: &str, command: &str, timeout_ms: u64) -> Result<RemoteRun> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let req = localapi::Request::ExecRemote {
        node: node.to_string(),
        shell: shell.to_string(),
        command: command.to_string(),
        timeout_ms,
    };
    match client.request(&req).await.map_err(|e| anyhow!("{e}"))? {
        localapi::Response::ExecResult {
            node,
            exit_code,
            stdout,
            stderr,
            truncated,
            duration_ms,
            error,
            ..
        } => Ok(RemoteRun {
            node,
            exit_code,
            stdout,
            stderr,
            truncated,
            duration_ms,
            error,
        }),
        localapi::Response::Error { message } => Err(anyhow!("{message}")),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

/// Resolve a roomler device NAME to its overlay address (P6c).
///
/// Case-insensitive, matching the server's `resolve_exec_target`, because
/// device names are display strings and nobody types `CORPLAP-3`.
/// `Ok(None)` = no such peer — the caller must report that rather than dial
/// something arbitrary.
pub async fn resolve_overlay_ip(name: &str) -> Result<Option<String>> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let peers = client.peers().await.map_err(daemon_err)?;
    Ok(peers
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .and_then(|p| p.overlay_ip))
}

/// What the server answered about an SSH session request.
pub struct SshGrant {
    pub address: Option<String>,
    pub port: Option<u16>,
    /// The target's SSH host public key. `None` = the device cannot prove
    /// itself; the caller must refuse rather than connect unverified.
    pub host_pubkey: Option<String>,
    pub error: Option<String>,
}

/// Ask the local daemon for a single-use SSH grant on `node`.
///
/// `public_key` is the caller's ephemeral session key — the daemon relays it
/// and never sees the private half.
pub async fn ssh_session(node: &str, public_key: &str, session_secs: u64) -> Result<SshGrant> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let req = localapi::Request::SshSession {
        node: node.to_string(),
        public_key: public_key.to_string(),
        session_secs,
    };
    match client.request(&req).await.map_err(|e| anyhow!("{e}"))? {
        localapi::Response::SshSession {
            address,
            port,
            host_pubkey,
            error,
            ..
        } => Ok(SshGrant {
            address,
            port,
            host_pubkey,
            error,
        }),
        localapi::Response::Error { message } => Err(anyhow!("{message}")),
        other => Err(anyhow!("unexpected daemon response: {other:?}")),
    }
}

/// `roomler exec <device> -- <command…>`
///
/// Exit status mirrors the remote command's, so this composes in a script:
/// a refused or failed command is a non-zero exit here too, never a silent 0.
pub async fn exec(
    device: &str,
    shell: &str,
    command: &str,
    timeout_ms: u64,
    json: bool,
) -> Result<()> {
    let run = run_remote(device, shell, command, timeout_ms).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "node": run.node,
                "exit_code": run.exit_code,
                "stdout": run.stdout,
                "stderr": run.stderr,
                "truncated": run.truncated,
                "duration_ms": run.duration_ms,
                "error": run.error,
            })
        );
    } else {
        print!("{}", run.stdout);
        if !run.stderr.is_empty() {
            eprint!("{}", run.stderr);
        }
        if run.truncated {
            eprintln!("[output truncated at the device's limit]");
        }
        if let Some(e) = &run.error {
            eprintln!("{}: {e}", run.node);
        }
    }
    // `error` set = never ran (a gate said no, offline, timed out). 1 is the
    // conventional "didn't work"; a real exit code passes straight through.
    match (run.error.is_some(), run.exit_code) {
        (true, _) => std::process::exit(1),
        (false, Some(0)) | (false, None) => Ok(()),
        (false, Some(code)) => std::process::exit(code),
    }
}

/// A named section of a diagnostic bundle.
struct BundleSection {
    title: &'static str,
    command: &'static str,
}

/// The Windows evidence set for "why is this pair not direct?".
///
/// Everything here is READ-ONLY — a bundle must be safe to run on a
/// production host without a second thought.
const WINDOWS_BUNDLE: &[BundleSection] = &[
    BundleSection {
        title: "adapters",
        command: "Get-NetAdapter | Sort-Object ifIndex | Format-Table -Auto ifIndex,Name,Status,LinkSpeed,InterfaceDescription | Out-String -Width 200",
    },
    BundleSection {
        title: "addresses",
        command: "Get-NetIPAddress -AddressFamily IPv4 | Format-Table -Auto ifIndex,IPAddress,PrefixLength,InterfaceAlias | Out-String -Width 200",
    },
    BundleSection {
        title: "routes (lowest metric first)",
        command: "Get-NetRoute -AddressFamily IPv4 | Sort-Object RouteMetric | Select-Object -First 25 | Format-Table -Auto DestinationPrefix,NextHop,RouteMetric,ifIndex | Out-String -Width 200",
    },
    BundleSection {
        title: "firewall profiles",
        command: "Get-NetFirewallProfile | Format-Table -Auto Name,Enabled,DefaultInboundAction,DefaultOutboundAction | Out-String -Width 200",
    },
    BundleSection {
        title: "firewall rules mentioning roomler",
        command: "Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -like '*oomler*' } | Format-Table -Auto DisplayName,Enabled,Direction,Action,Profile | Out-String -Width 200",
    },
    BundleSection {
        title: "udp dynamic port range",
        command: "netsh int ipv4 show dynamicport udp",
    },
    BundleSection {
        title: "udp excluded port ranges (Hyper-V/WSL reservations)",
        command: "netsh int ipv4 show excludedportrange protocol=udp",
    },
    BundleSection {
        title: "overlay peers",
        command: "& \"$env:ProgramFiles\\Roomler\\roomler.exe\" peers 2>&1 | Out-String -Width 200",
    },
    BundleSection {
        title: "node status",
        command: "& \"$env:ProgramFiles\\Roomler\\roomler.exe\" status 2>&1 | Out-String -Width 200",
    },
];

/// The Linux/macOS evidence set. Same questions, different tools; also
/// strictly read-only.
const UNIX_BUNDLE: &[BundleSection] = &[
    BundleSection {
        title: "addresses",
        command: "ip -br addr 2>/dev/null || ifconfig",
    },
    BundleSection {
        title: "routes",
        command: "ip route 2>/dev/null || netstat -rn",
    },
    BundleSection {
        title: "routes (v6)",
        command: "ip -6 route 2>/dev/null || true",
    },
    BundleSection {
        title: "firewall",
        command: "(nft list ruleset 2>/dev/null | head -40) || (iptables -S 2>/dev/null | head -40) || echo 'no nft/iptables visible'",
    },
    BundleSection {
        title: "overlay peers",
        command: "roomler peers 2>&1 || /usr/bin/roomler peers 2>&1",
    },
    BundleSection {
        title: "node status",
        command: "roomler status 2>&1 || /usr/bin/roomler status 2>&1",
    },
    BundleSection {
        title: "recent overlay log lines",
        command: "journalctl -u roomler -n 400 --no-pager 2>/dev/null | grep -iE 'overlay|carrier|relay' | tail -30 || echo 'no journal access'",
    },
];

/// Marker the bundle prints between sections, so one exec can carry the whole
/// set and the client can split it apart. Chosen to be something no diagnostic
/// output produces on its own.
const SECTION_MARK: &str = "===ROOMLER-DIAG-SECTION===";

/// Detect the target's family with ONE cheap round trip.
///
/// `echo $env:OS` is valid in both shells and disambiguates them: PowerShell
/// expands `$env:OS` to `Windows_NT`, while bash sees an unset `$env` followed
/// by the literal `:OS`. Cheaper and more reliable than trying to run `uname`
/// under PowerShell, and it needs no OS field on the wire.
async fn detect_windows(node: &str) -> Result<bool> {
    let run = run_remote(node, "", "echo $env:OS", 15_000).await?;
    if let Some(e) = run.error {
        return Err(anyhow!("{node}: {e}"));
    }
    Ok(run.stdout.contains("Windows_NT"))
}

/// `roomler diag host|pair` — run the canned bundle on each device and print
/// the sections.
///
/// The bundle lives HERE, in the CLI, not in the agent: a new probe is then a
/// CLI release rather than a fleet-wide agent rollout, which is the whole
/// reason this shipped as free-form exec first.
pub async fn diag_bundle(devices: &[String], json: bool) -> Result<()> {
    let mut all = Vec::new();
    for device in devices {
        let is_windows = detect_windows(device).await?;
        let sections = if is_windows {
            WINDOWS_BUNDLE
        } else {
            UNIX_BUNDLE
        };
        // One exec per host, not one per section: the round trips dominate,
        // and a single command keeps the whole bundle atomic in time — two
        // halves of a route table captured 5 s apart can disagree.
        let script = sections
            .iter()
            .map(|s| {
                if is_windows {
                    format!("Write-Output '{SECTION_MARK}{}'; {}", s.title, s.command)
                } else {
                    format!("echo '{SECTION_MARK}{}'; {}", s.title, s.command)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let run = run_remote(device, "", &script, 120_000).await?;
        all.push((device.clone(), run));
    }

    if json {
        let payload: Vec<serde_json::Value> = all
            .iter()
            .map(|(device, run)| {
                serde_json::json!({
                    "device": device,
                    "error": run.error,
                    "duration_ms": run.duration_ms,
                    "truncated": run.truncated,
                    "sections": split_sections(&run.stdout),
                })
            })
            .collect();
        println!("{}", serde_json::json!(payload));
        return Ok(());
    }

    for (device, run) in &all {
        println!("\n╔══ {device} ══════════════════════════════════════════");
        if let Some(e) = &run.error {
            println!("║ FAILED: {e}");
            continue;
        }
        for (title, body) in split_sections(&run.stdout) {
            println!("║\n║ ── {title} ──");
            for line in body.lines() {
                println!("║ {line}");
            }
        }
        if run.truncated {
            println!("║ [output truncated at the device's limit]");
        }
        if !run.stderr.is_empty() {
            println!("║\n║ ── stderr ──");
            for line in run.stderr.lines() {
                println!("║ {line}");
            }
        }
    }
    println!();
    Ok(())
}

/// Split a bundle's combined output back into `(title, body)` pairs on the
/// section marker. Pure — unit-tested without a daemon.
fn split_sections(out: &str) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let mut title: Option<String> = None;
    let mut body = String::new();
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix(SECTION_MARK) {
            if let Some(t) = title.take() {
                sections.push((t, std::mem::take(&mut body)));
            }
            title = Some(rest.to_string());
            continue;
        }
        if title.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(t) = title {
        sections.push((t, body));
    }
    sections
}

/// `roomler forward --daemon --agent <node> --local L --remote R` — ask the
/// LOCAL daemon to open + supervise a static forward over its OWN agent WS
/// (identity model b: no separate tunnel-client token — the pipe/socket ACL is
/// the trust boundary). Returns as soon as the daemon registers the flow; the
/// flow runs IN the daemon and survives this CLI's exit. `roomler flows` shows
/// it; `roomler kill <id>` stops it. A daemon-side error (bad node/remote, port
/// in use) surfaces verbatim; only a *connect* failure maps through [`daemon_err`].
pub async fn create_forward(node: &str, local: u16, remote: &str, transport: &str) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let id = client
        .create_forward(node, local, remote, transport)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    println!("forward created: {id}");
    println!(
        "  127.0.0.1:{local} → {remote}  via node {}",
        short_id(node)
    );
    println!("  roomler flows          # show it");
    println!("  roomler kill {id}      # stop it");
    Ok(())
}

/// `roomler socks5 --daemon --agent <node> --local L` — ask the LOCAL daemon to
/// open + supervise a SOCKS5 listener (userspace mode; per-connection target)
/// toward `node`. Same lifecycle as [`create_forward`].
pub async fn create_socks5(node: &str, local: u16, transport: &str) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let id = client
        .create_socks5(node, local, transport)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    println!("socks5 listener created: {id}");
    println!(
        "  127.0.0.1:{local} → node {} (per-connection target)",
        short_id(node)
    );
    println!("  roomler flows          # show it");
    println!("  roomler kill {id}      # stop it");
    Ok(())
}

/// `roomler kill <flow-id>` — stop + deregister a daemon flow. Reports whether
/// the id matched.
pub async fn kill(id: &str) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    if client.kill_flow(id).await.map_err(daemon_err)? {
        println!("killed flow {id}");
    } else {
        println!("no active flow with id {id}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Declared routes (P6)
// ---------------------------------------------------------------------------

/// `roomler route add …` — declare a daemon-supervised route. `remote`
/// present ⇒ static forward; absent ⇒ SOCKS5 listener. The daemon
/// persists it (config `[[tunnel_routes]]`) and reconciles it into a live
/// flow; it comes back on every daemon start until `route rm`.
pub async fn route_add(
    id: String,
    node: &str,
    local: u16,
    remote: Option<String>,
    transport: &str,
    enabled: bool,
) -> Result<()> {
    let kind = if remote.is_some() {
        FlowKind::Forward
    } else {
        FlowKind::Socks5
    };
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let eff = client
        .route_add(localapi::RouteDescriptor {
            id,
            kind,
            node: node.to_string(),
            local,
            remote,
            transport: transport.to_string(),
            enabled,
            org: None,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    println!("route declared: {}", eff.id);
    match &eff.remote {
        Some(r) => println!(
            "  127.0.0.1:{} → {r}  via node {}",
            eff.local,
            short_id(node)
        ),
        None => println!(
            "  socks5 127.0.0.1:{} → node {} (per-connection target)",
            eff.local,
            short_id(node)
        ),
    }
    if !eff.enabled {
        println!("  declared DISABLED — `roomler route enable {}`", eff.id);
    }
    println!("  roomler route ls            # live state");
    println!("  roomler route rm {}   # remove", eff.id);
    Ok(())
}

/// `roomler route rm <id>` — remove a declared route (kills its live flow,
/// deletes it from the daemon config).
pub async fn route_rm(id: &str) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    if client.route_remove(id).await.map_err(|e| anyhow!("{e}"))? {
        println!("removed route {id}");
    } else {
        println!("no declared route with id {id}");
    }
    Ok(())
}

/// `roomler route ls` — declared routes + live state.
pub async fn route_ls(json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let routes = client.route_list().await.map_err(daemon_err)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&routes)?);
    } else {
        print_routes(&routes);
    }
    Ok(())
}

/// `roomler route enable|disable <id>`.
pub async fn route_set_enabled(id: &str, enabled: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    if client
        .route_set_enabled(id, enabled)
        .await
        .map_err(|e| anyhow!("{e}"))?
    {
        println!(
            "route {id} {}",
            if enabled { "enabled" } else { "disabled" }
        );
    } else {
        println!("no declared route with id {id}");
    }
    Ok(())
}

/// `roomler config ls` — the S2 editable config surface: key, current
/// value, type. The daemon reads its own config file, so
/// pending not-yet-restarted edits show.
pub async fn config_ls(json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let entries = client.config_entries().await.map_err(daemon_err)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    let key_w = entries
        .iter()
        .map(|e| e.key.len())
        .max()
        .unwrap_or(3)
        .max(3);
    println!("{:key_w$}  {:9}  VALUE", "KEY", "TYPE");
    for e in &entries {
        let value = match &e.value {
            Some(v) if !v.is_empty() => v.clone(),
            _ => "(default)".to_string(),
        };
        println!("{:key_w$}  {:9}  {value}", e.key, e.kind);
    }
    println!(
        "\nchanges take effect on the next daemon restart — `roomler config set <key> <value>`"
    );
    Ok(())
}

/// `roomler config set <key> <value>` / `config clear <key>` — the
/// daemon validates per key + persists; the echoed entry confirms the
/// stored value. Validation errors come back verbatim.
pub async fn config_set(key: &str, value: Option<&str>) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let entry = client
        .config_set(key, value)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    match &entry.value {
        Some(v) if !v.is_empty() => println!("{} = {v}", entry.key),
        _ => println!("{} cleared (built-in default applies)", entry.key),
    }
    // Two keys are LIVE as of the gate-4 liveness work (docs/remote-config.md
    // §7b): the daemon re-seeds them from the file it just wrote, so telling an
    // operator to restart would be wrong in the direction that matters — they
    // would believe a refusal they just made is not in force yet, and either
    // restart a healthy daemon for nothing or assume they are still exposed.
    if matches!(entry.key.as_str(), "exec_enabled" | "remote_config_enabled") {
        println!("in effect now — no restart needed");
    } else {
        println!("takes effect on the next daemon restart");
    }
    Ok(())
}

/// Map a LocalAPI connect/IO error to a user-facing one. A missing daemon is an
/// *expected* state, so `NotFound` collapses to a single clean line with **no**
/// `.source()` chain (the raw "The system cannot find the file specified" /
/// ENOENT must never surface, and `main` prints just this one line). Everything
/// else keeps its context. Both branches are returned BEFORE any stdout write,
/// so `--json | jq` on a dead daemon fails cleanly with empty stdout.
fn daemon_err(e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::NotFound {
        anyhow!("roomler daemon not running (is the service started?)")
    } else {
        anyhow!("talking to the roomler daemon: {e}")
    }
}

/// Wall-clock ms since epoch, for the (pure) `fmt_last_seen`. Only the command
/// handlers call this; the formatters take `now_ms` as an argument.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Pure formatters (no I/O, no clock — deterministically testable)
// ---------------------------------------------------------------------------

/// ● up / ○ down — used for both the node's `connected` and a peer's `online`.
fn up_glyph(up: bool) -> char {
    if up { '●' } else { '○' }
}

/// The Tailscale-style connection-type word shown per peer.
fn connection_label(c: ConnectionType) -> &'static str {
    match c {
        ConnectionType::Direct => "direct",
        ConnectionType::Relay => "relay",
        ConnectionType::Tunnel => "tunnel",
        ConnectionType::Blocked => "blocked",
        ConnectionType::Offline => "offline",
    }
}

/// `relay` qualified with WHICH relay and HOW it is reached —
/// `relay:turn/udp`, `relay:derp/tcp`, or plain `relay` when the daemon is
/// older than the fields (they are `#[serde(default)]`).
///
/// A bare `relay` could not distinguish a ~50 ms coturn/UDP hop from a
/// ~175 ms DERP/TCP one, nor a healthy PoP from a DEAD one: on 2026-08-12 a
/// coturn worker was down for 90 minutes while agents crash-looped and this
/// column said only "relay". Non-relay rows are unchanged.
fn relay_qualified_label(p: &PeerInfo) -> String {
    let base = connection_label(p.connection);
    if !matches!(p.connection, ConnectionType::Relay) {
        return base.to_string();
    }
    match (p.relay_kind.as_deref(), p.relay_transport.as_deref()) {
        (Some(k), Some(t)) => format!("{base}:{k}/{t}"),
        (Some(k), None) => format!("{base}:{k}"),
        _ => base.to_string(),
    }
}

/// Render an optional `Display` value, falling back to the em-dash.
fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => DASH.to_string(),
    }
}

/// First 12 chars of a node/flow id + ellipsis (full id stays in `--json`).
fn short_id(id: &str) -> String {
    if id.chars().count() > 12 {
        let s: String = id.chars().take(12).collect();
        format!("{s}…")
    } else {
        id.to_string()
    }
}

/// 1024-step human bytes (`B`/`KiB`/`MiB`/`GiB`/`TiB`) for the flows table. The
/// raw `u64` still goes out untouched under `--json`.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// Relative age of `last_seen_ms` (epoch-ms) against `now_ms` — "12s ago" /
/// "3m ago" / "5h ago" / "2d ago". `now_ms` is injected so the formatter stays
/// pure. `None` → em-dash. A clock behind the timestamp clamps to "0s ago"
/// rather than underflowing.
fn fmt_last_seen(last_seen_ms: Option<u64>, now_ms: u64) -> String {
    let Some(ts) = last_seen_ms else {
        return DASH.to_string();
    };
    let secs = now_ms.saturating_sub(ts) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// One `peers` table row: `<glyph> NAME OVERLAY-IP CONN RTT LAST-SEEN`.
fn fmt_peer_row(p: &PeerInfo, now_ms: u64) -> String {
    let rtt = match p.rtt_ms {
        Some(ms) => format!("{ms} ms"),
        None => DASH.to_string(),
    };
    // A peer can arrive without a friendly name (seen in the field); show its
    // short node id rather than a blank cell so the row still identifies it.
    let name = if p.name.is_empty() {
        short_id(&p.node_id)
    } else {
        p.name.clone()
    };
    // rc.275 honesty — a carrier the health sweep judged SILENTLY ONE-WAY
    // renders as `stalled`, not as a healthy-looking `direct`/`relay` (the
    // dishonest label cost a multi-session field hunt on winhost-a: every tier
    // "installed", zero completed handshakes, 100 % ping loss — and the table
    // still said direct). Takes precedence over `upgrading`; the tier itself
    // stays visible in `peers --json` (`connection` + `stalled`).
    // P8-cosmetics — a relay peer with a make-before-break direct probe in
    // flight renders as `upgrading`: a snapshot taken mid-transition reads as
    // what it is instead of contradicting the latency the user just measured.
    let conn =
        if p.stalled && matches!(p.connection, ConnectionType::Direct | ConnectionType::Relay) {
            "stalled".to_string()
        } else if p.upgrading && matches!(p.connection, ConnectionType::Relay) {
            "upgrading".to_string()
        } else {
            relay_qualified_label(p)
        };
    format!(
        "{} {:<20} {:<16} {:<26} {:<15} {:>7} {}",
        up_glyph(p.online),
        name,
        opt(p.overlay_ip.as_deref()),
        opt(p.overlay_ip6.as_deref()),
        conn,
        rtt,
        fmt_last_seen(p.last_seen_ms, now_ms),
    )
}

/// One `flows` table row. `TARGET/NODE` shows the static forward's `target`, or
/// the reachable `node` for a SOCKS5 listener (whose target is per-connection).
fn fmt_flow_row(f: &FlowInfo) -> String {
    let kind = match f.kind {
        FlowKind::Forward => "forward",
        FlowKind::Socks5 => "socks5",
    };
    let target_or_node = f
        .target
        .as_deref()
        .or(f.node.as_deref())
        .unwrap_or(DASH)
        .to_string();
    format!(
        "{:<12} {:<8} {:<21} {:<24} {:<10} {:>6} {:>10} {:>10}",
        short_id(&f.id),
        kind,
        f.local_addr,
        target_or_node,
        f.transport,
        f.active_flows,
        human_bytes(f.bytes_in),
        human_bytes(f.bytes_out),
    )
}

fn print_status(s: &NodeStatus) {
    let mode = match s.mode {
        DaemonMode::Service => "service (SYSTEM)",
        DaemonMode::User => "user",
    };
    println!("{} {}", up_glyph(s.connected), s.name);
    println!("  node id     {}", short_id(&s.node_id));
    println!("  version     {}", s.version);
    println!("  mode        {mode}");
    // FR-51 — only when true: a permanent device's output stays byte-stable.
    if s.ephemeral {
        println!("  ephemeral   yes — removes itself after inactivity, or on clean stop");
    }
    println!("  tenant      {}", opt(s.tenant_id.as_deref()));
    println!("  overlay ip  {}", opt(s.overlay_ip.as_deref()));
    println!("  overlay ip6 {}", opt(s.overlay_ip6.as_deref()));
    println!(
        "  server      {}",
        if s.connected {
            "connected"
        } else {
            "disconnected"
        }
    );
    print_orgs(&s.orgs);
    // P5/S4 — exit-node routing (only when this node is configured as a client).
    // S3b — when active, also report whether global IPv6 rides the exit or is
    // fail-closed (Windows exit / no v6 uplink). S4b — and whether DNS is steered
    // through the exit (NOT steered = a DNS leak the operator should see).
    if let Some(ex) = &s.exit_node {
        let state = if ex.active {
            let v6 = if ex.v6_active {
                "v6 on"
            } else {
                "v6 fail-closed"
            };
            let dns = if ex.dns_steered {
                "DNS steered"
            } else {
                "DNS NOT steered"
            };
            format!("active, {v6}, {dns}")
        } else {
            format!(
                "withheld — {}",
                ex.withheld_reason.as_deref().unwrap_or("not ready")
            )
        };
        println!("  exit node   {} ({state})", ex.selector);
    }
    // S2 — MagicDNS status (only when the tenant has a magic domain and the
    // overlay published it). "resolver down" / "OS steer failed" are the two
    // states worth an operator's eye.
    if let Some(dns) = &s.dns {
        let health = if !dns.resolver_bound {
            "resolver DOWN"
        } else if !dns.os_steer_active {
            "resolver up, OS steer FAILED"
        } else {
            "active"
        };
        let aaaa = if dns.answer_aaaa {
            "AAAA on"
        } else {
            "AAAA off"
        };
        println!(
            "  magicdns    {} ({health}, {aaaa}, upstream {})",
            dns.magic_domain, dns.upstream
        );
    }

    // FR-33 — a captured LAN prefix is the other single most useful answer to
    // "why is this LAN pair on relay?": the peer's handshakes arrive, ours
    // leave through the VPN. Absent field = a daemon without the probe, and
    // then NOTHING is printed (an old daemon must not read as "clear").
    // Probe switched OFF by the operator = say so: there is no verdict, and
    // `why` / the RC pill cannot name a capture until it is back on.
    if s.lan_capture_probe == Some(false) {
        println!(
            "  lan         probe OFF (overlay_lan_capture_probe=false) — no capture verdict; `why` and the RC pill cannot name a VPN capture until it is re-enabled"
        );
    } else if let Some(caps) = &s.lan_captures {
        if caps.is_empty() {
            println!("  lan         clear (own prefixes route on-link)");
        }
        for c in caps {
            let via = match &c.via_name {
                Some(n) => format!("\"{n}\""),
                None => format!("ifref {}", c.via),
            };
            println!(
                "  lan         CAPTURED — {} leaves via {via} (owned by {}); direct on the LAN is impossible while it does",
                c.prefix, c.owner
            );
        }
    }
    // FR-47 — the server refused this node's overlay join. Printed even when
    // the node is connected NOW: "we were refused, and here is why" is the
    // only trace left once the daemon log has rotated, and `connected` above
    // already reports the current state so this cannot be read as it.
    //
    // Absent field = a daemon predating it, and then nothing is printed —
    // silence must never be mistaken for "never refused" on an old build.
    if let Some(r) = &s.join_refusal {
        let ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - r.at_unix)
            .unwrap_or(0)
            .max(0);
        let when = if ago < 90 {
            format!("{ago}s ago")
        } else if ago < 5400 {
            format!("{}m ago", ago / 60)
        } else {
            format!("{}h ago", ago / 3600)
        };
        let hint = if r.retryable {
            "transient — a retry may succeed"
        } else {
            "retrying will not help; an operator has to act"
        };
        println!("  join        REFUSED {when} — {} ({hint})", r.reason);
        if !r.detail.is_empty() {
            println!("              {}", r.detail);
        }
    }
    // NAT-traversal — the srflx line exists because an empty srflx tier is the
    // single most useful answer to "why is every peer on relay?", and it used
    // to be invisible: both failure paths logged at debug! only.
    if let Some(srflx) = &s.srflx {
        if srflx.candidates.is_empty() {
            let why = srflx.error.as_deref().unwrap_or("no public candidate");
            println!("  srflx       NONE — cannot hole-punch ({why})");
        } else {
            let nat = srflx.nat.as_deref().unwrap_or("unknown NAT");
            let via = srflx
                .stun_server
                .as_deref()
                .map(|s| format!(" via {s}"))
                .unwrap_or_default();
            // R2 — say WHICH path owns the mapping: a public-dial rescue means
            // every LAN vantage was dead and punches ride the VPN-tunnel path.
            let vantage = if srflx.via_public_dial {
                ", public-dial vantage"
            } else {
                ""
            };
            println!(
                "  srflx       {} ({nat}{via}{vantage})",
                srflx.candidates.join(", ")
            );
        }
    }

    // #32 — inbound `/derp` frames that reached no consumer. Printed ONLY when
    // non-zero: on a healthy node both are 0 and a permanent zero line is noise,
    // but after a network transition `unrouted` is the first thing to read — it
    // means a peer is relaying to us that we have not followed onto DERP, which
    // is exactly the 2026-08-25 case that could not be diagnosed because these
    // counters existed and were readable nowhere.
    if let Some((unrouted, backpressure)) = s.derp_inbound_drops
        && (unrouted > 0 || backpressure > 0)
    {
        println!(
            "  derp drops  unrouted={unrouted} backpressure={backpressure} \n             (cumulative — DIFF two readings, never judge the absolute)"
        );
    }

    // FR-68 — route-guard evidence. Printed only when something actually
    // happened, like every other counter line here. The pair worth reading is
    // evicted vs spared: on a healthy multi-org host `spared` climbs while
    // `evicted` stays flat. `evicted` climbing steadily is a route war — we
    // delete, a competitor re-adds, neither side holds the FIB.
    if let Some((evicted, spared, waves, revalidations)) = s.route_guard
        && (evicted > 0 || spared > 0 || waves > 0 || revalidations > 0)
    {
        // #1282 — attribute the waves when the daemon reports it. `tick` is the
        // blind 2 s fallback (i.e. no live route-change subscription); `event`
        // at its 3 s floor means something is generating change notifications
        // continuously. `other` is any wave neither call site claimed, which
        // would be a third caller nobody attributed — shown rather than hidden.
        let arms = match s.route_wave_arms {
            Some((tick, event)) => {
                let other = waves.saturating_sub(tick + event);
                let tail = if other > 0 {
                    format!(" other={other}")
                } else {
                    String::new()
                };
                format!(" [tick={tick} event={event}{tail}]")
            }
            None => String::new(),
        };
        // #1328 — the stand-down count. Shown only when non-zero, so a healthy
        // host's line is unchanged and a yielding host names itself. It sits
        // next to `evicted` on purpose: yields up while evicted goes flat is
        // the fight being bounded; BOTH climbing means the cooldown is too
        // short for whatever is competing here.
        let yields = match s.route_yields {
            Some(y) if y > 0 => format!(" yielded={y}"),
            _ => String::new(),
        };
        println!(
            "  route guard evicted={evicted} spared={spared} waves={waves}{arms} revalidations={revalidations}{yields} \n             (cumulative — DIFF two readings, never judge the absolute)"
        );
    }

    // C4 stage 1 — the warm TURN/UDP allocation. The line the Monday-morning
    // VPN check reads: "live" with a fresh probe while srflx is NONE means
    // the relay flow was grandfathered across the VPN connect.
    if let Some(w) = &s.warm_relay {
        match w.state.as_str() {
            "live" => {
                let probe = match w.last_probe_ok_s {
                    Some(a) => format!("probe ok {a}s ago"),
                    None => "probe pending".to_string(),
                };
                let expiry = match w.cred_expiry_in_s {
                    Some(e) => format!(", creds {e}s left"),
                    None => String::new(),
                };
                // C4 stage 2 — say which transport the leg rides: `tls`
                // is the strict-corp flavor that survives a VPN capture.
                let flavor = w
                    .flavor
                    .as_deref()
                    .map(|f| format!("{f}, "))
                    .unwrap_or_default();
                println!(
                    "  warm relay  {} ({flavor}age {}s, {probe}{expiry})",
                    w.relayed.as_deref().unwrap_or("?"),
                    w.age_s.unwrap_or(0)
                );
            }
            other => {
                let why = w.detail.as_deref().unwrap_or("not yet established");
                println!("  warm relay  {other} ({why})");
            }
        }
    }

    // FR-19 — the org-relay probe responder. Absent means this node is not
    // serving; it never means "serving but idle", which is why `answered` is
    // printed even when zero.
    if let Some(r) = &s.org_relay {
        // `bound` is not `reachable`: a DNAT can eat the port upstream of the
        // socket (measured on mars). `answered > 0` is the only line here that
        // is evidence someone actually got through.
        let reach = if r.answered > 0 {
            "reachable (probes answered)"
        } else {
            "bound — NOT yet proven reachable"
        };
        println!("  org relay   {} — {reach}", r.listening);
        println!(
            "              answered={} refused: shape={} not-probe={} rate={}",
            r.answered, r.refused_not_shaped, r.refused_not_probe, r.refused_rate_limited
        );
    }

    // PR-B1 — per-bound-direct-socket receive liveness: a socket with rx=0 /
    // a growing last_rx age while its endpoint is advertised is a dead reader
    // (the 2026-08-10 wedge: bound, advertised, Recv-Q pegged, never read).
    for ds in &s.direct_socks {
        let last = match ds.last_rx_age_s {
            Some(a) => format!("{a}s ago"),
            None => "never".into(),
        };
        println!(
            "  direct sock {}  rx={} last_rx={last}",
            ds.local, ds.rx_pkts
        );
    }
    if let Some(w) = s.direct_bind_walks
        && w > 0
    {
        println!(
            "  bind walks  {w} (stable direct port unavailable — squatter or in-process \
             bind collision)"
        );
    }
    if let Some(r) = s.roam_adoptions
        && r > 0
    {
        println!("  roam adopts {r} (peer endpoints adopted via WG-style roaming)");
    }
    // FR-46 — only printed when NON-EMPTY. An empty list is the common case and
    // says almost nothing (a knob read lazily is absent until it is read), so
    // printing "legacy env: none" every time would read as an all-clear it has
    // not earned. A hit, by contrast, is authoritative: this daemon really is
    // running on a retired variable name.
    if let Some(uses) = s.legacy_env_uses.as_ref()
        && !uses.is_empty()
    {
        println!(
            "  legacy env  {} READ through a retired name: {}",
            uses.len(),
            uses.join(", ")
        );
    }
    // FR-46 P2b — a retired variable is SET but not read. Printed even though
    // it changes nothing, because that is exactly the problem: the daemon runs
    // fine while the host is configured for a spelling nothing honours, so the
    // only way anyone finds out is if something says so.
    //
    // A sibling of the block above, NOT nested inside it: after P2b nothing
    // reads a retired prefix, so `legacy_env_uses` is empty on exactly the
    // hosts this line exists for, and nesting would make it dead code.
    if let Some(present) = s.retired_env_present.as_ref()
        && !present.is_empty()
    {
        println!(
            "  retired env {} SET and IGNORED: {} (rename to ROOMLERD_*)",
            present.len(),
            present.join(", ")
        );
    }
}

/// FR-49 — the per-enrollment block in `roomler status`.
///
/// `NodeStatus.orgs` has been on the wire since multi-org P1 and was rendered
/// by NOTHING but `--json`, so on a device in two orgs the human output said
/// nothing about the second at all. Skipped entirely for a single-org daemon,
/// keeping that output byte-identical.
fn print_orgs(orgs: &[tunnel_core::localapi::OrgStatus]) {
    if orgs.len() < 2 {
        return;
    }
    println!("  enrollments");
    for o in orgs {
        let conn = if !o.enabled {
            "disabled"
        } else if o.connected {
            "connected"
        } else {
            "disconnected"
        };
        // ⚠️ Empty is "this daemon does not report it", NOT "off" — those are
        // opposite claims and an older daemon must not be made to assert the
        // one it never made.
        let overlay = match o.overlay_mode.as_str() {
            "" => "overlay ?".to_string(),
            "off" => "overlay OFF".to_string(),
            m => format!("overlay {m}"),
        };
        let primary = if o.primary { " (primary)" } else { "" };
        println!("    {:<14} {conn}, {overlay}{primary}", o.label);
        if let Some(err) = &o.terminal_error {
            println!("                   stopped: {err}");
        }
    }
    // The line that closes the gap this FR is about: enabled + connected +
    // no mesh looks exactly like healthy, and the operator has to be told.
    let dark: Vec<&str> = orgs
        .iter()
        .filter(|o| o.enabled && o.overlay_mode == "off")
        .map(|o| o.label.as_str())
        .collect();
    if !dark.is_empty() {
        println!(
            "                   ⚠️ enrolled but NOT on the mesh: {} \
             (`roomlerd org overlay <label> netstack`)",
            dark.join(", ")
        );
    }
}

/// The orgs `print_dark_orgs` names: enabled, overlay explicitly `off`, and
/// contributing no peer rows.
///
/// ⚠️ Four exclusions, each load-bearing:
/// - an org with peers is already visible, so naming it would be noise;
/// - an org whose overlay is ON but whose peers are all offline is a DIFFERENT
///   state and must keep looking different — that is the distinction the whole
///   FR is about;
/// - a DISABLED org has no signalling loop either, so "not on a mesh" is not
///   the interesting thing about it;
/// - an EMPTY `overlay_mode` means the daemon does not report one, which is not
///   a claim that the overlay is off (absent ≠ off).
fn dark_orgs<'a>(
    orgs: &'a [tunnel_core::localapi::OrgStatus],
    peers: &[PeerInfo],
) -> Vec<&'a tunnel_core::localapi::OrgStatus> {
    orgs.iter()
        .filter(|o| o.enabled && o.overlay_mode == "off")
        .filter(|o| !peers.iter().any(|p| p.org == o.label))
        .collect()
}

fn print_peers(peers: &[PeerInfo], now_ms: u64) {
    println!(
        "  {:<20} {:<16} {:<26} {:<15} {:>7} LAST SEEN",
        "NAME", "OVERLAY IP", "OVERLAY IP6", "CONN", "RTT"
    );
    if peers.is_empty() {
        println!("(no peers)");
        return;
    }
    for (i, (org, rows)) in group_peers_by_org(peers).into_iter().enumerate() {
        if let Some(org) = org {
            if i > 0 {
                println!();
            }
            println!("  ── org: {org} ──");
        }
        for p in rows {
            println!("{}", fmt_peer_row(p, now_ms));
        }
    }
}

/// Multi-org — group peers by org in FIRST-APPEARANCE order (the daemon emits
/// the primary's mesh first, then each secondary). `None` as the group key
/// means "print no header": that is the single-org case, where every row has
/// an empty `org` and the output must stay byte-identical to the flat table
/// older CLIs — and the diag bundle, which shells out to `roomler peers` —
/// already expect. Rows with no org alongside labelled ones (mixed daemon
/// shapes) are kept in a trailing `(unlabelled)` group rather than dropped.
fn group_peers_by_org(peers: &[PeerInfo]) -> Vec<(Option<&str>, Vec<&PeerInfo>)> {
    let mut order: Vec<&str> = Vec::new();
    for p in peers {
        if !p.org.is_empty() && !order.contains(&p.org.as_str()) {
            order.push(p.org.as_str());
        }
    }
    if order.is_empty() {
        return vec![(None, peers.iter().collect())];
    }
    let mut out: Vec<(Option<&str>, Vec<&PeerInfo>)> = order
        .into_iter()
        .map(|org| (Some(org), peers.iter().filter(|p| p.org == org).collect()))
        .collect();
    let orphans: Vec<&PeerInfo> = peers.iter().filter(|p| p.org.is_empty()).collect();
    if !orphans.is_empty() {
        out.push((Some("(unlabelled)"), orphans));
    }
    out
}

fn print_flows(flows: &[FlowInfo]) {
    if flows.is_empty() {
        println!("No active flows.");
        return;
    }
    println!(
        "{:<12} {:<8} {:<21} {:<24} {:<10} {:>6} {:>10} {:>10}",
        "ID", "KIND", "LOCAL", "TARGET/NODE", "TRANSPORT", "ACTIVE", "IN", "OUT"
    );
    for f in flows {
        println!("{}", fmt_flow_row(f));
    }
}

fn print_routes(routes: &[RouteInfo]) {
    if routes.is_empty() {
        println!("No declared routes. Declare one with `roomler route add`.");
        return;
    }
    println!(
        "{:<14} {:<8} {:<10} {:>6} {:<24} {:<10} STATE",
        "ID", "KIND", "NODE", "LOCAL", "REMOTE", "TRANSPORT"
    );
    for r in routes {
        println!("{}", fmt_route_row(r));
    }
}

/// One `route ls` table row — pure for unit tests.
fn fmt_route_row(r: &RouteInfo) -> String {
    let d = &r.route;
    let kind = match d.kind {
        FlowKind::Forward => "forward",
        FlowKind::Socks5 => "socks5",
    };
    let transport = if d.transport.is_empty() {
        "auto"
    } else {
        &d.transport
    };
    format!(
        "{:<14} {:<8} {:<10} {:>6} {:<24} {:<10} {}",
        d.id,
        kind,
        short_id(&d.node),
        d.local,
        d.remote.as_deref().unwrap_or(DASH),
        transport,
        route_state_word(&r.state),
    )
}

/// Compact human word for a route's live state. `backoff`/`failed` carry
/// their detail — that's what the operator acts on.
fn route_state_word(s: &RouteState) -> String {
    match s {
        RouteState::Disabled => "disabled".into(),
        RouteState::Pending => "pending".into(),
        RouteState::Active { flow_id } => format!("active ({flow_id})"),
        RouteState::Backoff {
            next_retry_secs,
            last_error,
        } => format!("backoff {next_retry_secs}s: {last_error}"),
        RouteState::Failed { reason } => format!("FAILED: {reason} (route enable to retry)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, org: &str) -> PeerInfo {
        PeerInfo {
            node_id: name.into(),
            name: name.into(),
            org: org.into(),
            overlay_ip: None,
            overlay_ip6: None,
            online: true,
            connection: ConnectionType::Direct,
            upgrading: false,
            stalled: false,
            rtt_ms: None,
            last_seen_ms: None,
            agent_id: None,
            relay_local: None,
            relay_dst: None,
            relay_kind: None,
            relay_transport: None,
            relay_server: None,
            why: None,
            probes: Vec::new(),
            debug: None,
        }
    }

    /// A relayed peer must say WHICH relay and HOW — a bare `relay` hid a dead
    /// coturn PoP for 90 minutes on 2026-08-12, and makes a ~50 ms UDP hop
    /// indistinguishable from a ~175 ms DERP/TCP one.
    #[test]
    fn relay_rows_name_the_relay_kind_and_transport() {
        let mut p = peer("fleet-host-2", "");
        p.connection = ConnectionType::Relay;

        p.relay_kind = Some("turn".into());
        p.relay_transport = Some("udp".into());
        assert_eq!(relay_qualified_label(&p), "relay:turn/udp");

        p.relay_kind = Some("derp".into());
        p.relay_transport = Some("tcp".into());
        assert_eq!(relay_qualified_label(&p), "relay:derp/tcp");

        // Older daemon: the fields are `#[serde(default)]`, so the row must
        // degrade to the historical word rather than render "relay:".
        p.relay_kind = None;
        p.relay_transport = None;
        assert_eq!(relay_qualified_label(&p), "relay");

        // A DIRECT peer never carries a relay qualifier, even if stale fields
        // linger in the payload.
        let mut d = peer("buildhost", "");
        d.relay_kind = Some("turn".into());
        d.relay_transport = Some("udp".into());
        assert_eq!(relay_qualified_label(&d), "direct");
    }

    fn org(label: &str, overlay_mode: &str, enabled: bool) -> tunnel_core::localapi::OrgStatus {
        tunnel_core::localapi::OrgStatus {
            label: label.into(),
            server_url: "https://example.invalid".into(),
            tenant_id: None,
            agent_id: None,
            primary: false,
            enabled,
            connected: true,
            terminal_error: None,
            updates_ignored: 0,
            overlay_mode: overlay_mode.into(),
        }
    }

    /// FR-49 — an org with no mesh and an org with no peers must not render
    /// identically. Before this, an overlay-off org produced no section at all
    /// in `roomler peers`, which is exactly what an org whose peers happen to
    /// be offline produces.
    #[test]
    fn only_an_enabled_overlay_off_org_with_no_peers_is_dark() {
        let orgs = vec![
            org("dark", "off", true),        // ← the case this exists for
            org("meshed-idle", "tun", true), // overlay ON, no peers yet: NOT dark
            org("disabled", "off", false),   // no signalling loop either
            org("unreported", "", true),     // older daemon: absent is not "off"
            org("has-peers", "off", true),   // already visible in the table
        ];
        let peers = vec![peer("node-a", "has-peers")];
        let dark: Vec<&str> = dark_orgs(&orgs, &peers)
            .into_iter()
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(dark, vec!["dark"]);
    }

    /// The single-org case must stay byte-identical: nothing to say, so
    /// nothing is said.
    #[test]
    fn a_single_org_daemon_has_no_dark_orgs_and_no_enrollment_block() {
        assert!(dark_orgs(&[], &[]).is_empty());
        assert!(dark_orgs(&[org("solo", "tun", true)], &[]).is_empty());
    }

    /// Single-org (every row unlabelled) must render as ONE unheaded group —
    /// byte-identical to the pre-multi-org flat table that older CLIs and the
    /// diag bundle parse. Multi-org groups in first-appearance order.
    #[test]
    fn peers_group_by_org_first_appearance_and_single_org_is_flat() {
        // Single-org: one group, no header.
        let flat = vec![peer("a", ""), peer("b", "")];
        let g = group_peers_by_org(&flat);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, None, "single-org must print no org header");
        assert_eq!(g[0].1.len(), 2);

        // Multi-org: primary first (as the daemon emits), then secondaries,
        // each group holding only its own rows.
        let multi = vec![
            peer("p1", "primary"),
            peer("j1", "jovanov"),
            peer("p2", "primary"),
        ];
        let g = group_peers_by_org(&multi);
        assert_eq!(
            g.iter().map(|(o, _)| o.unwrap()).collect::<Vec<_>>(),
            vec!["primary", "jovanov"]
        );
        assert_eq!(g[0].1.len(), 2);
        assert_eq!(g[1].1.len(), 1);
        assert_eq!(g[1].1[0].name, "j1");

        // Mixed shapes: unlabelled rows are kept, not dropped.
        let mixed = vec![peer("x", "primary"), peer("y", "")];
        let g = group_peers_by_org(&mixed);
        assert_eq!(g.len(), 2);
        assert_eq!(g[1].0, Some("(unlabelled)"));
        assert_eq!(g[1].1[0].name, "y");
    }

    #[test]
    fn route_rows_render_each_state() {
        fn info(state: RouteState) -> RouteInfo {
            RouteInfo {
                route: tunnel_core::localapi::RouteDescriptor {
                    id: "pg-buildhost".into(),
                    kind: FlowKind::Forward,
                    node: "aabbccddeeff001122334455".into(),
                    local: 15432,
                    remote: Some("db:5432".into()),
                    transport: String::new(),
                    enabled: true,
                    org: None,
                },
                state,
            }
        }
        let active = fmt_route_row(&info(RouteState::Active {
            flow_id: "fl-3".into(),
        }));
        assert!(active.contains("pg-buildhost"), "got {active}");
        assert!(active.contains("forward"));
        assert!(active.contains("db:5432"));
        assert!(active.contains("auto"), "empty transport renders auto");
        assert!(active.contains("active (fl-3)"));

        let backoff = fmt_route_row(&info(RouteState::Backoff {
            next_retry_secs: 12,
            last_error: "port 15432 in use".into(),
        }));
        assert!(backoff.contains("backoff 12s: port 15432 in use"));

        let failed = fmt_route_row(&info(RouteState::Failed {
            reason: "revoked".into(),
        }));
        assert!(failed.contains("FAILED: revoked"));

        // A socks5 route has no remote — the column shows the dash.
        let mut socks = info(RouteState::Pending);
        socks.route.kind = FlowKind::Socks5;
        socks.route.remote = None;
        let row = fmt_route_row(&socks);
        assert!(row.contains("socks5"));
        assert!(row.contains(DASH));
        assert!(row.contains("pending"));
    }

    #[test]
    fn human_bytes_steps() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn last_seen_relative_and_dash() {
        let now = 1_700_000_000_000u64; // realistic epoch-ms (well past 2 days)
        assert_eq!(fmt_last_seen(None, now), "—");
        assert_eq!(fmt_last_seen(Some(now), now), "0s ago");
        assert_eq!(fmt_last_seen(Some(now - 5_000), now), "5s ago");
        assert_eq!(fmt_last_seen(Some(now - 120_000), now), "2m ago");
        assert_eq!(fmt_last_seen(Some(now - 7_200_000), now), "2h ago");
        assert_eq!(fmt_last_seen(Some(now - 172_800_000), now), "2d ago");
        // Clock behind the reported timestamp clamps to 0, not an underflow panic.
        assert_eq!(fmt_last_seen(Some(now + 5_000), now), "0s ago");
    }

    #[test]
    fn peer_row_fields_and_dash_for_nulls() {
        let now = 1_000_000u64;
        let online = PeerInfo {
            node_id: "n2".into(),
            name: "winhost-a".into(),
            org: String::new(),
            overlay_ip: Some("100.64.0.1".into()),
            overlay_ip6: Some("fd72:6f6f:6d6c::6440:1".into()),
            online: true,
            connection: ConnectionType::Tunnel,
            upgrading: false,
            stalled: false,
            rtt_ms: Some(52),
            last_seen_ms: Some(now - 3_000),
            agent_id: None,
            relay_local: None,
            relay_dst: None,
            relay_kind: None,
            relay_transport: None,
            relay_server: None,
            why: None,
            probes: Vec::new(),
            debug: None,
        };
        let row = fmt_peer_row(&online, now);
        assert!(row.starts_with('●'));
        assert!(row.contains("winhost-a"));
        assert!(row.contains("100.64.0.1"));
        assert!(row.contains("fd72:6f6f:6d6c::6440:1"));
        assert!(row.contains("tunnel"));
        assert!(row.contains("52 ms"));
        assert!(row.contains("3s ago"));

        let offline = PeerInfo {
            node_id: "n3".into(),
            name: "home".into(),
            org: String::new(),
            overlay_ip: None,
            overlay_ip6: None,
            online: false,
            connection: ConnectionType::Offline,
            upgrading: false,
            stalled: false,
            rtt_ms: None,
            last_seen_ms: None,
            agent_id: None,
            relay_local: None,
            relay_dst: None,
            relay_kind: None,
            relay_transport: None,
            relay_server: None,
            why: None,
            probes: Vec::new(),
            debug: None,
        };
        let row = fmt_peer_row(&offline, now);
        assert!(row.starts_with('○'));
        assert!(row.contains('—')); // null overlay_ip + rtt + last_seen
        assert!(row.contains("offline"));
    }

    #[test]
    fn peer_row_empty_name_shows_short_id() {
        let now = 1_000_000u64;
        let p = PeerInfo {
            node_id: "0123456789abcdef0123".into(),
            name: String::new(),
            org: String::new(),
            overlay_ip: Some("100.64.0.7".into()),
            overlay_ip6: None,
            online: true,
            connection: ConnectionType::Direct,
            upgrading: false,
            stalled: false,
            rtt_ms: None,
            last_seen_ms: None,
            agent_id: None,
            relay_local: None,
            relay_dst: None,
            relay_kind: None,
            relay_transport: None,
            relay_server: None,
            why: None,
            probes: Vec::new(),
            debug: None,
        };
        let row = fmt_peer_row(&p, now);
        assert!(row.contains("0123456789ab…"), "row was: {row}");
        assert!(row.contains("100.64.0.7"));
    }

    /// rc.275 honesty — a silently-one-way carrier renders `stalled`, never a
    /// healthy-looking tier label; it also beats `upgrading`. A stalled
    /// TUNNEL/OFFLINE peer keeps its own label (the flag describes overlay
    /// carriers only).
    #[test]
    fn peer_row_stalled_beats_tier_and_upgrading() {
        let now = 1_000_000u64;
        let mut p = PeerInfo {
            node_id: "n4".into(),
            name: "winhost-a".into(),
            org: String::new(),
            overlay_ip: Some("100.64.0.1".into()),
            overlay_ip6: None,
            online: true,
            connection: ConnectionType::Direct,
            upgrading: false,
            stalled: true,
            rtt_ms: None,
            last_seen_ms: Some(now - 4_000),
            agent_id: None,
            relay_local: None,
            relay_dst: None,
            relay_kind: None,
            relay_transport: None,
            relay_server: None,
            why: None,
            probes: Vec::new(),
            debug: None,
        };
        assert!(fmt_peer_row(&p, now).contains("stalled"));
        p.connection = ConnectionType::Relay;
        p.upgrading = true;
        let row = fmt_peer_row(&p, now);
        assert!(row.contains("stalled"), "stalled beats upgrading: {row}");
        assert!(!row.contains("upgrading"));
        // Not an overlay carrier ⇒ the flag is ignored.
        p.connection = ConnectionType::Tunnel;
        assert!(fmt_peer_row(&p, now).contains("tunnel"));
        p.stalled = false;
        p.connection = ConnectionType::Relay;
        assert!(fmt_peer_row(&p, now).contains("upgrading"));
    }

    #[test]
    fn flow_row_target_then_node_fallback() {
        let fwd = FlowInfo {
            id: "0123456789abcdef0123".into(),
            kind: FlowKind::Forward,
            local_addr: "127.0.0.1:5432".into(),
            target: Some("10.0.0.5:5432".into()),
            node: Some("winhost-a".into()),
            transport: "quic-v1".into(),
            active_flows: 2,
            bytes_in: 4096,
            bytes_out: 1024 * 1024,
        };
        let row = fmt_flow_row(&fwd);
        assert!(row.contains("0123456789ab…")); // short id
        assert!(row.contains("forward"));
        assert!(row.contains("127.0.0.1:5432"));
        assert!(row.contains("10.0.0.5:5432")); // target wins over node
        assert!(row.contains("4.0 KiB"));
        assert!(row.contains("1.0 MiB"));

        let socks = FlowInfo {
            id: "f2".into(),
            kind: FlowKind::Socks5,
            local_addr: "127.0.0.1:1080".into(),
            target: None,
            node: Some("winhost-a".into()),
            transport: "quic-v1".into(),
            active_flows: 0,
            bytes_in: 0,
            bytes_out: 0,
        };
        let row = fmt_flow_row(&socks);
        assert!(row.contains("socks5"));
        assert!(row.contains("winhost-a")); // node fallback when target is None
    }

    #[test]
    fn labels_glyphs_and_short_id() {
        assert_eq!(connection_label(ConnectionType::Direct), "direct");
        assert_eq!(connection_label(ConnectionType::Relay), "relay");
        assert_eq!(connection_label(ConnectionType::Blocked), "blocked");
        assert_eq!(up_glyph(true), '●');
        assert_eq!(up_glyph(false), '○');
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("0123456789abcdef0123"), "0123456789ab…");
        assert_eq!(opt::<&str>(None), "—");
        assert_eq!(opt(Some("x")), "x");
    }

    // ─── Fleet-RPC diag bundle ───────────────────────────────────────

    #[test]
    fn splits_a_bundle_into_sections() {
        let out = format!(
            "{SECTION_MARK}adapters\neth0 UP\nwlan0 DOWN\n{SECTION_MARK}routes\ndefault via 1.1.1.1\n"
        );
        let sections = split_sections(&out);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "adapters");
        assert_eq!(sections[0].1, "eth0 UP\nwlan0 DOWN\n");
        assert_eq!(sections[1].0, "routes");
        assert_eq!(sections[1].1, "default via 1.1.1.1\n");
    }

    #[test]
    fn preamble_before_the_first_marker_is_dropped() {
        // PowerShell profiles and shell rc files love printing banners. They
        // belong to no section, and attributing them to the first one would
        // quietly corrupt the evidence.
        let out = format!("Windows PowerShell\nCopyright (c)\n{SECTION_MARK}routes\n0.0.0.0/0\n");
        let sections = split_sections(&out);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "routes");
        assert_eq!(sections[0].1, "0.0.0.0/0\n");
    }

    #[test]
    fn an_empty_section_still_appears() {
        // "the firewall has no roomler rules" is a FINDING, not a missing
        // section — dropping it would read as "we didn't check".
        let out =
            format!("{SECTION_MARK}firewall rules mentioning roomler\n{SECTION_MARK}routes\nx\n");
        let sections = split_sections(&out);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "firewall rules mentioning roomler");
        assert_eq!(sections[0].1, "");
    }

    #[test]
    fn output_with_no_markers_yields_nothing() {
        assert!(split_sections("just some output\n").is_empty());
        assert!(split_sections("").is_empty());
    }

    #[test]
    fn bundles_are_read_only() {
        // A bundle must be safe to run unthinkingly on a production node —
        // three of the fleet's Linux hosts are live k8s cluster members.
        // This is a coarse guard, not a sandbox, but it catches the obvious
        // slip of pasting a mutating command into the evidence set.
        const FORBIDDEN: &[&str] = &[
            "Stop-Service",
            "Restart-Service",
            "Set-Net",
            "New-Net",
            "Remove-",
            "systemctl stop",
            "systemctl restart",
            "pkill",
            "kill -",
            "reboot",
            "shutdown",
            " rm ",
            "iptables -A",
            "iptables -I",
            "nft add",
        ];
        for section in WINDOWS_BUNDLE.iter().chain(UNIX_BUNDLE.iter()) {
            for bad in FORBIDDEN {
                assert!(
                    !section.command.contains(bad),
                    "bundle section {:?} contains a mutating command {bad:?}",
                    section.title
                );
            }
        }
    }
}
