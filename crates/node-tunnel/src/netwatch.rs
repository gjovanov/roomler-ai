// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Feature-shaped bridge from the tunnel layer to `netstate` (R1, 2026-08-25).
//!
//! The tunnel flow supervisor and the route reconciler both want ONE thing
//! from the network monitor: "wake me when the network materially moved, so
//! I retry NOW instead of waiting out a backoff that a transition just
//! invalidated" — the same shape the control-WS reconnect ladder proved in
//! `signaling.rs` (its `next_major_netchange`). `netstate` compiles only
//! under tunnel-core's `overlay` feature, so this module owns the two-arm
//! cfg so its consumers don't repeat it.
//!
//! Storm guard: netstate itself damps to ≤1 material Major per 120 s
//! (`MAJOR_PUBLISH_COOLDOWN`), so a flapping network cannot pin retry
//! ladders to their floor — the #506 lesson, inherited rather than
//! re-implemented.

#[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
pub type NetRx = tokio::sync::broadcast::Receiver<tunnel_core::overlay::netstate::NetDelta>;
#[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
pub type NetRx = std::convert::Infallible;

/// Subscribe to netstate deltas, `None` when the monitor is absent
/// (feature off / disabled by config / backend failed).
pub fn subscribe() -> Option<NetRx> {
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    {
        tunnel_core::overlay::netstate::handle().map(|h| h.subscribe())
    }
    #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
    {
        None
    }
}

/// The live DERP mux of each org this daemon runs, registered by the overlay
/// runtime when it brings a mesh up. FR-123 P1 moved the registry here, on
/// the tunnel side of the crate boundary, so this crate never reaches up into
/// the daemon: `overlay.rs` registers into it and re-exports what the daemon
/// names. A `Weak`, so a torn-down mesh leaves no handle behind; the pubkey
/// hex beside it is the identity the tunnel transport presents.
static DERP_TUNNEL_MUXES: std::sync::Mutex<
    std::collections::BTreeMap<
        String,
        (
            std::sync::Weak<tunnel_core::transport::derp::DerpMux>,
            String,
        ),
    >,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The primary enrollment's tenant id, set once by the daemon at start. The
/// daemon's own path, `roomlerd::overlay::PRIMARY_TENANT_ID`, re-exports it.
pub static PRIMARY_TENANT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Register (or replace) a tenant's live DERP mux; the overlay runtime calls
/// it once per mesh start.
pub fn register_derp_tunnel_mux(
    tenant: &str,
    mux: &std::sync::Arc<tunnel_core::transport::derp::DerpMux>,
) {
    DERP_TUNNEL_MUXES.lock().unwrap().insert(
        tenant.to_string(),
        (
            std::sync::Arc::downgrade(mux),
            tunnel_core::transport::derp::pubkey_hex(&mux.self_pubkey()),
        ),
    );
}

/// R4 — the PRIMARY org's live DERP mux as a tunnel handle. `None` until the
/// overlay registered one; a build without an overlay never registers, so
/// those builds keep the classic ladder exactly as before.
pub fn primary_derp_tunnel_handle() -> Option<tunnel_core::transport::derp::DerpTunnelHandle> {
    derp_tunnel_handle(PRIMARY_TENANT_ID.get()?)
}

/// R4 — a specific tenant's live DERP mux as a tunnel handle.
pub fn derp_tunnel_handle(tenant: &str) -> Option<tunnel_core::transport::derp::DerpTunnelHandle> {
    let slots = DERP_TUNNEL_MUXES.lock().unwrap();
    let (weak, hex) = slots.get(tenant)?;
    let mux = weak.upgrade()?;
    Some(tunnel_core::transport::derp::DerpTunnelHandle {
        mux,
        self_pubkey_hex: hex.clone(),
    })
}

/// Yield the next material Major's summary (`Lagged` counts as one — the
/// conservative read); pend forever with no subscription. Cancel-safe, so
/// it can race a backoff sleep in `tokio::select!`.
pub async fn next_major(rx: &mut Option<NetRx>) -> String {
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    {
        tunnel_core::overlay::netstate::next_major(rx).await
    }
    #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
    {
        let _ = rx;
        std::future::pending().await
    }
}
