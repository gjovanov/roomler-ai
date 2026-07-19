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

/// Overlay TUN interface name on Linux — matches `tun.rs` / `dns.rs`. Used to
/// scope the `filter`/`FORWARD` ACCEPT rules (P5/A4) to overlay-forwarded
/// traffic only.
#[cfg(target_os = "linux")]
const IF_NAME: &str = "roomler0";

/// RAII guard for the OS forwarding/NAT state. `Drop` reverts whatever `enable`
/// installed. A guard with `active == false` (nothing advertised, or setup
/// failed) is an inert no-op.
pub struct SubnetRouterGuard {
    overlay_cidr: String,
    active: bool,
}

/// Enable forwarding + NAT for `overlay_cidr` when `advertised_routes` is
/// non-empty. Returns a guard that reverts on `Drop`. A no-op (inert guard) when
/// nothing is advertised or the platform is unsupported.
pub async fn enable(overlay_cidr: &str, advertised_routes: &[String]) -> SubnetRouterGuard {
    if advertised_routes.is_empty() {
        return SubnetRouterGuard {
            overlay_cidr: overlay_cidr.to_string(),
            active: false,
        };
    }
    let fully_ok = setup(overlay_cidr).await;
    // Arm the guard on any platform where `setup` installs rules, so `Drop`
    // reverts even a PARTIALLY-applied ruleset (each `-D` / `Remove-NetNat` is
    // idempotent — reverting an absent rule is a harmless no-op). `fully_ok`
    // only drives the log level. Previously `active = setup()`, which leaked the
    // rules that DID apply whenever one of the (now multiple, P5/A4) commands
    // failed.
    let active = cfg!(any(target_os = "linux", target_os = "windows"));
    if active && fully_ok {
        info!(%overlay_cidr, routes = ?advertised_routes,
            "overlay: subnet-router forwarding + NAT enabled");
    } else if active {
        warn!(%overlay_cidr,
            "overlay: subnet-router forwarding/NAT not fully enabled (see prior errors)");
    }
    SubnetRouterGuard {
        overlay_cidr: overlay_cidr.to_string(),
        active,
    }
}

#[cfg(target_os = "linux")]
async fn setup(overlay_cidr: &str) -> bool {
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
    let nat_ok = run(vec![
        "iptables".into(),
        "-t".into(),
        "nat".into(),
        "-A".into(),
        "POSTROUTING".into(),
        "-s".into(),
        overlay_cidr.into(),
        "-j".into(),
        "MASQUERADE".into(),
    ])
    .await;
    // filter/FORWARD ACCEPT (P5/A4): container hosts (Docker/containerd — the
    // k8s fleet mars/jupiter/zeus) default the FORWARD chain policy to DROP, so
    // `ip_forward=1` + NAT alone silently drop forwarded packets. Explicitly
    // accept overlay→uplink and the established return path. Needed by BOTH
    // subnet-routers and exit nodes; the subnet-router path only ever "worked"
    // on LANs whose upstream router had a permissive FORWARD policy.
    let fwd_out_ok = run(vec![
        "iptables".into(),
        "-A".into(),
        "FORWARD".into(),
        "-i".into(),
        IF_NAME.into(),
        "-j".into(),
        "ACCEPT".into(),
    ])
    .await;
    let fwd_ret_ok = run(vec![
        "iptables".into(),
        "-A".into(),
        "FORWARD".into(),
        "-o".into(),
        IF_NAME.into(),
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
    setup_v6().await;
    nat_ok && fwd_out_ok && fwd_ret_ok
}

/// P5/S3b — enable IPv6 forwarding + MASQUERADE on an exit node (Linux). Best-
/// effort + logged independently of v4: a v4-only uplink (no v6, no `ip6tables`)
/// simply leaves v6 egress unavailable, and clients then stay v6-fail-closed.
#[cfg(target_os = "linux")]
async fn setup_v6() {
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
        IF_NAME.into(),
        "-j".into(),
        "ACCEPT".into(),
    ])
    .await;
    let fwd6_ret_ok = run(vec![
        "ip6tables".into(),
        "-A".into(),
        "FORWARD".into(),
        "-o".into(),
        IF_NAME.into(),
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
async fn setup(overlay_cidr: &str) -> bool {
    // P5/S3b — WinNAT (`New-NetNat`) has NO IPv6 API, so a Windows exit node
    // cannot NAT v6. Clients routing through a Windows exit stay v6-fail-closed
    // (their global v6 is encapsulated but dropped here — never leaked). v6 exit
    // egress is Linux-only; see docs/remote-control (S5).
    info!(
        "overlay: IPv6 exit NAT unavailable on Windows (WinNAT is v4-only); v6 stays fail-closed"
    );
    // Forwarding on the overlay NIC. The LAN adapter's forwarding is normally
    // already on; enabling every interface is heavy-handed, so we do the roomler
    // NIC and rely on WinNAT for the address translation.
    let _ = run(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        "Set-NetIPInterface -InterfaceAlias roomler -Forwarding Enabled \
         -ErrorAction SilentlyContinue"
            .into(),
    ])
    .await;
    // Create the NAT only if no existing WinNAT covers this prefix (Docker
    // Desktop / WSL2 also use WinNAT and overlapping prefixes are rejected).
    run(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        format!(
            "if (-not (Get-NetNat -ErrorAction SilentlyContinue | \
             Where-Object {{ $_.InternalIPInterfaceAddressPrefix -eq '{overlay_cidr}' }})) {{ \
             New-NetNat -Name {NAT_NAME} \
             -InternalIPInterfaceAddressPrefix '{overlay_cidr}' \
             -ErrorAction SilentlyContinue }}"
        ),
    ])
    .await
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
async fn setup(_overlay_cidr: &str) -> bool {
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
            let _ = std::process::Command::new("iptables")
                .args([
                    "-t",
                    "nat",
                    "-D",
                    "POSTROUTING",
                    "-s",
                    &self.overlay_cidr,
                    "-j",
                    "MASQUERADE",
                ])
                .output();
            // Mirror the P5/A4 FORWARD ACCEPT rules from `setup`.
            let _ = std::process::Command::new("iptables")
                .args(["-D", "FORWARD", "-i", IF_NAME, "-j", "ACCEPT"])
                .output();
            let _ = std::process::Command::new("iptables")
                .args([
                    "-D",
                    "FORWARD",
                    "-o",
                    IF_NAME,
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
                .args(["-D", "FORWARD", "-i", IF_NAME, "-j", "ACCEPT"])
                .output();
            let _ = std::process::Command::new("ip6tables")
                .args([
                    "-D",
                    "FORWARD",
                    "-o",
                    IF_NAME,
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
            let _ = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "Remove-NetNat -Name {NAT_NAME} -Confirm:$false \
                         -ErrorAction SilentlyContinue"
                    ),
                ])
                .output();
        }
        info!(overlay_cidr = %self.overlay_cidr, "overlay: subnet-router forwarding/NAT reverted");
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
