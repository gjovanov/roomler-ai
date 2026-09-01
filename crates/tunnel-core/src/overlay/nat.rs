// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Phase 1 subnet-router forwarding + NAT.
//!
//! When a node advertises subnet routes, it must **forward** overlay→LAN traffic
//! and **masquerade** it so LAN hosts reply to the router itself (zero LAN-side
//! config — Tailscale's default). This module enables IP forwarding + NAT scoped
//! to the overlay CIDR at startup and reverts it on `Drop` (mirroring the WFP
//! guard's cleanup pattern).
//!
//! Best-effort: every command failure is logged, never fatal — a node that can't
//! set up NAT simply doesn't route (peers just can't reach its LAN). The agent
//! runs as a privileged service, so it has the rights.
//!
//! - **Linux:** `sysctl net.ipv4.ip_forward=1` + `iptables -t nat -A POSTROUTING
//!   -s <overlay-cidr> -j MASQUERADE`, plus `filter`/`FORWARD` ACCEPT rules for
//!   the overlay interface — container hosts (Docker/containerd, the k8s fleet)
//!   default the `FORWARD` chain policy to DROP, which silently swallows
//!   forwarded packets despite `ip_forward=1` (P5/A4).
//! - **Windows:** `Set-NetIPInterface -Forwarding Enabled` on the overlay NIC +
//!   **WinNAT** `New-NetNat -InternalIPInterfaceAddressPrefix <overlay-cidr>` —
//!   the modern, scriptable, no-reboot NAT engine (Win10 1607+/Server 2016+),
//!   the same one Docker/WSL2 use. Skipped when an overlapping `Get-NetNat`
//!   already exists (WinNAT rejects overlapping internal prefixes).
//! - Other platforms: no-op.
//!
//! Only the *advertising* node needs this; clients of a subnet route need just
//! the route + router-table entry (no NAT).

#[allow(unused_imports)]
use tracing::{info, warn};

/// WinNAT instance name (Windows only).
#[cfg(target_os = "windows")]
const NAT_NAME: &str = "roomler-overlay";

/// RAII guard for the OS forwarding/NAT state. `Drop` reverts whatever `enable`
/// installed. A guard with `active == false` (nothing advertised, or setup
/// failed) is an inert no-op.
pub struct SubnetRouterGuard {
    /// Multi-org v2 — the overlay adapter these rules were scoped to (the
    /// `Drop` revert must name the SAME device the setup did).
    if_name: String,
    /// FR-47 P5e — EVERY block of the org's address space, not just the one
    /// this node is addressed in.
    ///
    /// A subnet router masquerades traffic *sourced from the overlay* out its
    /// uplink. Under multi-block a peer in block 1 reaching a LAN behind a
    /// router in block 0 is still overlay-sourced, and NATing only block 0
    /// would let its packets out un-masqueraded — the far side would reply to
    /// an address it cannot route to, and the flow would black-hole one way.
    ///
    /// One entry for every network that has not grown, which issues exactly
    /// the commands the single-CIDR version did.
    overlay_cidrs: Vec<String>,
    active: bool,
}

/// Enable forwarding + NAT for `overlay_cidr` when `advertised_routes` is
/// non-empty. Returns a guard that reverts on `Drop`. A no-op (inert guard) when
/// nothing is advertised or the platform is unsupported.
///
/// Multi-org v2 — `if_name` is the overlay adapter to scope the rules to
/// (formerly a module-level constant): per-org adapters must NAT/forward
/// THEIR device, not the historical singleton. Legacy callers pass the
/// device's own OS name, falling back to
/// [`tun::IF_NAME`](crate::overlay::tun::IF_NAME).
pub async fn enable(
    if_name: &str,
    overlay_cidrs: &[String],
    advertised_routes: &[String],
) -> SubnetRouterGuard {
    if advertised_routes.is_empty() {
        return SubnetRouterGuard {
            if_name: if_name.to_string(),
            overlay_cidrs: overlay_cidrs.to_vec(),
            active: false,
        };
    }
    let fully_ok = setup(if_name, overlay_cidrs, advertised_routes).await;
    // Arm the guard on any platform where `setup` installs rules, so `Drop`
    // reverts even a PARTIALLY-applied ruleset (each `-D` / `Remove-NetNat` is
    // idempotent — reverting an absent rule is a harmless no-op). `fully_ok`
    // only drives the log level. Previously `active = setup()`, which leaked the
    // rules that DID apply whenever one of the (now multiple, P5/A4) commands
    // failed.
    let active = cfg!(any(target_os = "linux", target_os = "windows"));
    if active && fully_ok {
        info!(%if_name, cidrs = ?overlay_cidrs, routes = ?advertised_routes,
            "overlay: subnet-router forwarding + NAT enabled");
    } else if active {
        warn!(%if_name, cidrs = ?overlay_cidrs,
            "overlay: subnet-router forwarding/NAT not fully enabled (see prior errors)");
    }
    SubnetRouterGuard {
        if_name: if_name.to_string(),
        overlay_cidrs: overlay_cidrs.to_vec(),
        active,
    }
}

#[cfg(target_os = "linux")]
async fn setup(if_name: &str, overlay_cidrs: &[String], _advertised_routes: &[String]) -> bool {
    // Global forwarding (leave it on at teardown — another service may rely on
    // it; we only remove our own rules).
    let _ = run(vec![
        "sysctl".into(),
        "-w".into(),
        "net.ipv4.ip_forward=1".into(),
    ])
    .await;
    // NAT: masquerade overlay-sourced traffic out the host's uplink so the far
    // side replies to the router itself (zero peer-side config).
    //
    // FR-47 P5e — one rule per BLOCK. A single-block org issues exactly the one
    // command this used to, so the live subnet-router path is unchanged; a
    // grown org gets a rule for each of its ranges, because a peer in block 1
    // is just as overlay-sourced as one in block 0.
    let mut nat_ok = true;
    for cidr in overlay_cidrs {
        nat_ok &= run(vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-A".into(),
            "POSTROUTING".into(),
            "-s".into(),
            cidr.into(),
            "-j".into(),
            "MASQUERADE".into(),
        ])
        .await;
    }
    // filter/FORWARD ACCEPT (P5/A4): container hosts (Docker/containerd — the
    // k8s fleet buildhost/fleet-host-1/fleet-host-2) default the FORWARD chain policy to DROP, so
    // `ip_forward=1` + NAT alone silently drop forwarded packets. Explicitly
    // accept overlay→uplink and the established return path. Needed by BOTH
    // subnet-routers and exit nodes; the subnet-router path only ever "worked"
    // on LANs whose upstream router had a permissive FORWARD policy.
    let fwd_out_ok = run(vec![
        "iptables".into(),
        "-A".into(),
        "FORWARD".into(),
        "-i".into(),
        if_name.into(),
        "-j".into(),
        "ACCEPT".into(),
    ])
    .await;
    let fwd_ret_ok = run(vec![
        "iptables".into(),
        "-A".into(),
        "FORWARD".into(),
        "-o".into(),
        if_name.into(),
        "-m".into(),
        "conntrack".into(),
        "--ctstate".into(),
        "RELATED,ESTABLISHED".into(),
        "-j".into(),
        "ACCEPT".into(),
    ])
    .await;
    // P5/S3b — IPv6 exit egress (best-effort; independent of the v4 result so a
    // v4-only-uplink exit still reports v4 success). Clients keep v6 fail-closed
    // until this succeeds on the exit.
    setup_v6(if_name).await;
    nat_ok && fwd_out_ok && fwd_ret_ok
}

/// P5/S3b — enable IPv6 forwarding + MASQUERADE on an exit node (Linux). Best-
/// effort + logged independently of v4: a v4-only uplink (no v6, no `ip6tables`)
/// simply leaves v6 egress unavailable, and clients then stay v6-fail-closed.
#[cfg(target_os = "linux")]
async fn setup_v6(if_name: &str) {
    // Enable v6 forwarding. `accept_ra=2` so a host that forwards STILL accepts
    // Router Advertisements — otherwise `forwarding=1` downgrades RA acceptance
    // and a SLAAC/RA-configured uplink loses its OWN v6 default on the next RA,
    // killing the egress this NAT depends on (v4's `ip_forward` has no such
    // coupling). A static-v6 uplink is unaffected (no RA to lose). Leave both
    // sysctls on at teardown — another service may rely on them.
    let _ = run(vec![
        "sysctl".into(),
        "-w".into(),
        "net.ipv6.conf.all.forwarding=1".into(),
    ])
    .await;
    let _ = run(vec![
        "sysctl".into(),
        "-w".into(),
        "net.ipv6.conf.all.accept_ra=2".into(),
    ])
    .await;
    // MASQUERADE overlay-sourced v6 out the uplink. Source is the derived-v6
    // `/96` ([`OVERLAY_ULA_V6_CIDR`]) — exactly the on-link prefix, so it can't
    // over-broadly NAT a co-located non-overlay ULA (Docker/WSL2/other VPN).
    let nat6_ok = run(vec![
        "ip6tables".into(),
        "-t".into(),
        "nat".into(),
        "-A".into(),
        "POSTROUTING".into(),
        "-s".into(),
        super::router::OVERLAY_ULA_V6_CIDR.into(),
        "-j".into(),
        "MASQUERADE".into(),
    ])
    .await;
    let fwd6_out_ok = run(vec![
        "ip6tables".into(),
        "-A".into(),
        "FORWARD".into(),
        "-i".into(),
        if_name.into(),
        "-j".into(),
        "ACCEPT".into(),
    ])
    .await;
    let fwd6_ret_ok = run(vec![
        "ip6tables".into(),
        "-A".into(),
        "FORWARD".into(),
        "-o".into(),
        if_name.into(),
        "-m".into(),
        "conntrack".into(),
        "--ctstate".into(),
        "RELATED,ESTABLISHED".into(),
        "-j".into(),
        "ACCEPT".into(),
    ])
    .await;
    if nat6_ok && fwd6_out_ok && fwd6_ret_ok {
        info!("overlay: exit-node IPv6 forwarding + NAT enabled");
    } else {
        info!(
            "overlay: IPv6 exit NAT not fully enabled (v4-only uplink / no ip6tables?) — \
             clients routing through this exit stay v6-fail-closed"
        );
    }
}

#[cfg(target_os = "windows")]
async fn setup(if_name: &str, overlay_cidrs: &[String], advertised_routes: &[String]) -> bool {
    // P5/S3b — WinNAT (`New-NetNat`) has NO IPv6 API, so a Windows exit node
    // cannot NAT v6. Clients routing through a Windows exit stay v6-fail-closed
    // (their global v6 is encapsulated but dropped here — never leaked). v6 exit
    // egress is Linux-only; see docs/remote-control (S5).
    info!(
        "overlay: IPv6 exit NAT unavailable on Windows (WinNAT is v4-only); v6 stays fail-closed"
    );
    // Forwarding on the overlay NIC **and on the EGRESS NIC for each advertised
    // route**.
    //
    // The previous version enabled only `roomler`, on the assumption that "the
    // LAN adapter's forwarding is normally already on". That is FALSE on
    // Windows: every interface ships `Forwarding: Disabled`, and Windows only
    // forwards a packet when forwarding is enabled on BOTH the ingress and the
    // egress interface. So the subnet router accepted overlay packets and then
    // silently dropped them — peers had the OS route installed and every TCP
    // connection still timed out.
    //
    // Field-diagnosed on winhost-a 2026-08-03: `roomler` Enabled, every other NIC
    // Disabled, `Get-NetNat` healthy — the subnet router had been dead for a
    // week while the agent logged "forwarding + NAT enabled".
    //
    // The egress NIC is DERIVED PER ROUTE via `Find-NetRoute` rather than
    // hardcoded to Ethernet/Wi-Fi: a corp route is very often carried by a VPN
    // adapter (Check Point / AnyConnect WAN miniports), which is exactly the
    // deployment a subnet router exists for. Only the interfaces that actually
    // carry an advertised route are touched — still far short of "enable every
    // interface".
    //
    // The script VERIFIES the resulting state and exits non-zero on failure,
    // because `run` can only observe the PowerShell process exit code and
    // `-ErrorAction SilentlyContinue` would otherwise report success for a
    // cmdlet that never applied.
    let targets = advertised_routes
        .iter()
        .map(|c| format!("'{}'", c.trim().replace('\'', "")))
        .collect::<Vec<_>>()
        .join(",");
    // Multi-org v2 — the overlay NIC is the INSTANCE adapter, not the
    // historical `roomler` literal. Quote-sanitized like `targets` (the name
    // comes from our own constants/config, but a `'` must never break out).
    let alias = if_name.trim().replace('\'', "");
    let fwd_ok = run(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        format!(
            "$fail = 0; \
             Set-NetIPInterface -InterfaceAlias '{alias}' -AddressFamily IPv4 \
               -Forwarding Enabled -ErrorAction SilentlyContinue; \
             if ((Get-NetIPInterface -InterfaceAlias '{alias}' -AddressFamily IPv4 \
               -ErrorAction SilentlyContinue | Select-Object -First 1).Forwarding \
               -ne 'Enabled') {{ $fail = 1 }}; \
             foreach ($c in @({targets})) {{ \
               $ip = ($c -split '/')[0]; \
               $r = Find-NetRoute -RemoteIPAddress $ip -ErrorAction SilentlyContinue | \
                 Select-Object -First 1; \
               if (-not $r) {{ continue }}; \
               Set-NetIPInterface -ifIndex $r.InterfaceIndex -AddressFamily IPv4 \
                 -Forwarding Enabled -ErrorAction SilentlyContinue; \
               if ((Get-NetIPInterface -ifIndex $r.InterfaceIndex -AddressFamily IPv4 \
                 -ErrorAction SilentlyContinue | Select-Object -First 1).Forwarding \
                 -ne 'Enabled') {{ $fail = 1 }} \
             }}; \
             exit $fail"
        ),
    ])
    .await;
    if !fwd_ok {
        warn!(
            "overlay: could not enable IPv4 forwarding on the overlay NIC and/or an \
             advertised route's egress NIC — the subnet router will accept packets and \
             drop them. Check `Get-NetIPInterface | ft ifIndex,InterfaceAlias,Forwarding`."
        );
    }
    // Create the NAT only if no existing WinNAT covers this prefix (Docker
    // Desktop / WSL2 also use WinNAT and overlapping prefixes are rejected).
    //
    // FR-47 P5e — one WinNAT per BLOCK, because a WinNAT instance carries
    // exactly one internal prefix. The FIRST block keeps the historical
    // unsuffixed name, so a single-block org creates the identical instance it
    // always did (and an upgrade finds its own NAT rather than orphaning one);
    // later blocks are suffixed by index.
    let mut ok = true;
    for (i, cidr) in overlay_cidrs.iter().enumerate() {
        let name = if i == 0 {
            NAT_NAME.to_string()
        } else {
            format!("{NAT_NAME}-{i}")
        };
        ok &= run(vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "if (-not (Get-NetNat -ErrorAction SilentlyContinue | \
                 Where-Object {{ $_.InternalIPInterfaceAddressPrefix -eq '{cidr}' }})) {{ \
                 New-NetNat -Name {name} \
                 -InternalIPInterfaceAddressPrefix '{cidr}' \
                 -ErrorAction SilentlyContinue }}"
            ),
        ])
        .await;
    }
    ok
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
async fn setup(_if_name: &str, _overlay_cidrs: &[String], _advertised_routes: &[String]) -> bool {
    false
}

impl Drop for SubnetRouterGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // `Drop` can't await — revert synchronously via blocking `Command`.
        #[cfg(target_os = "linux")]
        {
            // FR-47 P5e — one `-D` per block, mirroring `setup`'s one `-A` per
            // block. Deleting a rule that is not there is a harmless no-op, so
            // a partially-applied ruleset still reverts cleanly.
            for cidr in &self.overlay_cidrs {
                let _ = std::process::Command::new("iptables")
                    .args([
                        "-t",
                        "nat",
                        "-D",
                        "POSTROUTING",
                        "-s",
                        cidr,
                        "-j",
                        "MASQUERADE",
                    ])
                    .output();
            }
            // Mirror the P5/A4 FORWARD ACCEPT rules from `setup`.
            let _ = std::process::Command::new("iptables")
                .args(["-D", "FORWARD", "-i", &self.if_name, "-j", "ACCEPT"])
                .output();
            let _ = std::process::Command::new("iptables")
                .args([
                    "-D",
                    "FORWARD",
                    "-o",
                    &self.if_name,
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "RELATED,ESTABLISHED",
                    "-j",
                    "ACCEPT",
                ])
                .output();
            // P5/S3b — mirror the v6 rules from `setup_v6` (idempotent `-D`;
            // reverting an absent rule on a v4-only exit is a harmless no-op). The
            // forwarding/accept_ra sysctls are left on, like v4's `ip_forward`.
            let _ = std::process::Command::new("ip6tables")
                .args([
                    "-t",
                    "nat",
                    "-D",
                    "POSTROUTING",
                    "-s",
                    super::router::OVERLAY_ULA_V6_CIDR,
                    "-j",
                    "MASQUERADE",
                ])
                .output();
            let _ = std::process::Command::new("ip6tables")
                .args(["-D", "FORWARD", "-i", &self.if_name, "-j", "ACCEPT"])
                .output();
            let _ = std::process::Command::new("ip6tables")
                .args([
                    "-D",
                    "FORWARD",
                    "-o",
                    &self.if_name,
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "RELATED,ESTABLISHED",
                    "-j",
                    "ACCEPT",
                ])
                .output();
        }
        #[cfg(target_os = "windows")]
        {
            // FR-47 P5e — remove one instance per block, matching `setup`'s
            // naming (first unsuffixed, later ones `-1`, `-2`, …). Removing an
            // absent NAT is a no-op, so a partial setup still reverts.
            for i in 0..self.overlay_cidrs.len().max(1) {
                let name = if i == 0 {
                    NAT_NAME.to_string()
                } else {
                    format!("{NAT_NAME}-{i}")
                };
                let _ = std::process::Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        &format!(
                            "Remove-NetNat -Name {name} -Confirm:$false \
                             -ErrorAction SilentlyContinue"
                        ),
                    ])
                    .output();
            }
        }
        info!(
            if_name = %self.if_name,
            overlay_cidrs = ?self.overlay_cidrs,
            "overlay: subnet-router forwarding/NAT reverted"
        );
    }
}

/// Run an OS command off the async reactor (`std::process` in a blocking task —
/// avoids tokio's `process` feature). `true` on exit 0, else logs stderr.
#[cfg(any(target_os = "linux", target_os = "windows"))]
async fn run(args: Vec<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        let prog = args[0].clone();
        match std::process::Command::new(&prog).args(&args[1..]).output() {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                warn!(%prog, stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "overlay: subnet-router command failed");
                false
            }
            Err(e) => {
                warn!(%prog, %e, "overlay: subnet-router command spawn failed");
                false
            }
        }
    })
    .await
    .unwrap_or(false)
}
