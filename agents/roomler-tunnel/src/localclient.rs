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
    self, ConnectionType, DaemonMode, FlowInfo, FlowKind, NodeStatus, PeerInfo,
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
    }
    Ok(())
}

/// `roomler peers` — every peer this node sees, with its live connection type.
pub async fn peers(json: bool) -> Result<()> {
    let mut client = localapi::connect().await.map_err(daemon_err)?;
    let peers = client.peers().await.map_err(daemon_err)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&peers)?);
    } else {
        print_peers(&peers, now_ms());
    }
    Ok(())
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
    format!(
        "{} {:<20} {:<16} {:<26} {:<8} {:>7} {}",
        up_glyph(p.online),
        name,
        opt(p.overlay_ip.as_deref()),
        opt(p.overlay_ip6.as_deref()),
        connection_label(p.connection),
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
}

fn print_peers(peers: &[PeerInfo], now_ms: u64) {
    println!(
        "  {:<20} {:<16} {:<26} {:<8} {:>7} LAST SEEN",
        "NAME", "OVERLAY IP", "OVERLAY IP6", "CONN", "RTT"
    );
    if peers.is_empty() {
        println!("(no peers)");
        return;
    }
    for p in peers {
        println!("{}", fmt_peer_row(p, now_ms));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
            name: "pc50045".into(),
            overlay_ip: Some("100.64.0.1".into()),
            overlay_ip6: Some("fd72:6f6f:6d6c::6440:1".into()),
            online: true,
            connection: ConnectionType::Tunnel,
            rtt_ms: Some(52),
            last_seen_ms: Some(now - 3_000),
        };
        let row = fmt_peer_row(&online, now);
        assert!(row.starts_with('●'));
        assert!(row.contains("pc50045"));
        assert!(row.contains("100.64.0.1"));
        assert!(row.contains("fd72:6f6f:6d6c::6440:1"));
        assert!(row.contains("tunnel"));
        assert!(row.contains("52 ms"));
        assert!(row.contains("3s ago"));

        let offline = PeerInfo {
            node_id: "n3".into(),
            name: "home".into(),
            overlay_ip: None,
            overlay_ip6: None,
            online: false,
            connection: ConnectionType::Offline,
            rtt_ms: None,
            last_seen_ms: None,
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
            overlay_ip: Some("100.64.0.7".into()),
            overlay_ip6: None,
            online: true,
            connection: ConnectionType::Direct,
            rtt_ms: None,
            last_seen_ms: None,
        };
        let row = fmt_peer_row(&p, now);
        assert!(row.contains("0123456789ab…"), "row was: {row}");
        assert!(row.contains("100.64.0.7"));
    }

    #[test]
    fn flow_row_target_then_node_fallback() {
        let fwd = FlowInfo {
            id: "0123456789abcdef0123".into(),
            kind: FlowKind::Forward,
            local_addr: "127.0.0.1:5432".into(),
            target: Some("10.0.0.5:5432".into()),
            node: Some("pc50045".into()),
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
            node: Some("pc50045".into()),
            transport: "quic-v1".into(),
            active_flows: 0,
            bytes_in: 0,
            bytes_out: 0,
        };
        let row = fmt_flow_row(&socks);
        assert!(row.contains("socks5"));
        assert!(row.contains("pc50045")); // node fallback when target is None
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
}
