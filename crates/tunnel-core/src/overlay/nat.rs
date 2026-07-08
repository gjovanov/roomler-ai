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
//!   -s <overlay-cidr> -j MASQUERADE`.
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
    let active = setup(overlay_cidr).await;
    if active {
        info!(%overlay_cidr, routes = ?advertised_routes,
            "overlay: subnet-router forwarding + NAT enabled");
    } else {
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
    // it; we only remove our own NAT rule).
    let _ = run(vec![
        "sysctl".into(),
        "-w".into(),
        "net.ipv4.ip_forward=1".into(),
    ])
    .await;
    run(vec![
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
    .await
}

#[cfg(target_os = "windows")]
async fn setup(overlay_cidr: &str) -> bool {
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
