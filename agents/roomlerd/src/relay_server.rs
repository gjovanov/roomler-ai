// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-19 — start the org-relay server, if this device opted in.
//!
//! Process-wide and started once: the relay owns a single UDP socket, so it is
//! deliberately **not** per-org like [`crate::overlay::maybe_start`].
//!
//! # The default path costs nothing
//!
//! `relay_server_enabled` is FR-19's gate 4 — the refusal that survives a
//! compromised server — and it is opt-in, so a device that has not explicitly
//! turned this on binds no socket, spawns no task and logs nothing.
//!
//! # A failed bind is loud, and a successful one is not a promise
//!
//! If the port cannot be bound the daemon says so at `error!` with the reason,
//! rather than starting a relay that silently never answers. And the success
//! line states plainly that **binding is not reachability**: on a host with a
//! coturn DNAT the port is consumed in `PREROUTING` while `ss -ulnp` shows it
//! free and the socket receives nothing. That is not hypothetical — it is what
//! mars does, and it is why FR-19's E2E-3 nearly reached the opposite
//! conclusion about which port corporate egresses permit.
//!
//! # P2c: it forwards now
//!
//! Since P2c the socket is served by [`RelayServer`], which answers probes,
//! runs the authenticated bind, and forwards ciphertext between bound
//! members. Sessions still enter only through [`handle`] — P3 wires that to
//! the control-WS mint. Until then a relay serves probes and holds no sessions,
//! which is exactly P1's behaviour.

use std::sync::{Arc, OnceLock};

use tunnel_core::overlay::orgrelay;
use tunnel_core::overlay::orgrelay::bind::CookieKey;
use tunnel_core::overlay::orgrelay::responder::ResponderCounts;
use tunnel_core::overlay::orgrelay::server::{RelayCounts, RelayHandle, RelayServer, RelayStats};

/// Set **only after a successful bind**, so its presence means "this node is
/// actually serving", never merely "someone asked it to".
///
/// ⚠️ An earlier version set this before binding, which would have reported a
/// live relay on a node whose bind had failed — the precise confusion the
/// `Option` exists to prevent. A failed bind leaves this `None` and says so at
/// `error!`.
static RUNNING: OnceLock<Running> = OnceLock::new();

struct Running {
    listening: String,
    stats: Arc<RelayStats>,
    handle: RelayHandle,
}

/// Guards against a second start. Separate from [`RUNNING`] because that is
/// only populated once the socket exists, and the guard has to hold from the
/// moment the first call is made.
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Probe counters, or `None` when this node is not serving. Feeds the P1
/// `NodeStatus::org_relay` field.
pub fn stats() -> Option<ResponderCounts> {
    RUNNING.get().map(|r| r.stats.snapshot().probe)
}

/// The full relay counters, or `None` when not serving.
pub fn relay_stats() -> Option<RelayCounts> {
    RUNNING.get().map(|r| r.stats.snapshot())
}

/// The bound address and probe counters, or `None` when not serving. Feeds
/// `NodeStatus::org_relay`, so `roomler status` answers "is it working?"
/// immediately instead of waiting out the report loop's 300 s window.
pub fn status() -> Option<(String, ResponderCounts)> {
    RUNNING
        .get()
        .map(|r| (r.listening.clone(), r.stats.snapshot().probe))
}

/// The session-install handle, or `None` when not serving. P3 uses this to
/// install and revoke sessions from the control-WS mint messages.
pub fn handle() -> Option<RelayHandle> {
    RUNNING.get().map(|r| r.handle.clone())
}

/// How often the counters are summarised into the log. `roomler status` is
/// the immediate reader; this is the historical one.
const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// Start the relay if the device opted in. Safe to call once per process.
pub fn maybe_start() {
    if !orgrelay::relay_server_enabled() {
        return;
    }
    let port = orgrelay::relay_server_port();
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        tracing::warn!("org-relay server already started; ignoring second start");
        return;
    }
    tokio::spawn(async move {
        let sock = match tokio::net::UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                // Loud and specific. A relay that cannot bind must not look
                // like a relay that is merely quiet -- and RUNNING stays None,
                // so `roomler status` shows no relay rather than a phantom one.
                tracing::error!(
                    port,
                    error = %e,
                    "org-relay server NOT started: could not bind udp/{port}. \
                     Something else already owns the port, or the daemon lacks \
                     permission for it. The node will not answer relay probes."
                );
                return;
            }
        };
        let listening = sock
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| format!("0.0.0.0:{port}"));

        // The relay's own rotating cookie key. Fresh per start, never
        // persisted, never leaves the process: it exists so the relay can
        // re-derive a challenge without storing per-attempt state.
        let mut key = [0u8; 32];
        {
            use rand::RngCore;
            rand::rng().fill_bytes(&mut key);
        }
        let stats = Arc::new(RelayStats::default());
        let (server, handle) = RelayServer::new(CookieKey::from_bytes(key), stats.clone());
        let _ = RUNNING.set(Running {
            listening,
            stats: stats.clone(),
            handle,
        });
        tokio::spawn(report_loop(stats));
        server.serve(sock).await;
    });
}

/// Summarise the counters whenever they change. Silent while nothing happens,
/// so an idle relay does not fill the log.
async fn report_loop(stats: Arc<RelayStats>) {
    let mut last = RelayCounts::default();
    loop {
        tokio::time::sleep(REPORT_EVERY).await;
        let now = stats.snapshot();
        if now == last {
            continue;
        }
        tracing::info!(
            probes_answered = now.probe.answered,
            forwarded = now.forwarded,
            bound = now.bound,
            sessions_installed = now.sessions_installed,
            sessions_revoked = now.sessions_revoked,
            sessions_reaped = now.sessions_reaped,
            drop_unbound_source = now.drop_unbound_source,
            drop_unknown_vni = now.drop_unknown_vni,
            drop_bad_tag1 = now.drop_bad_tag1,
            drop_bad_cookie = now.drop_bad_cookie,
            drop_bad_tag2 = now.drop_bad_tag2,
            refused_rate_limited = now.refused_rate_limited,
            panics_caught = now.panics_caught,
            "org-relay server counters"
        );
        last = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is off, and "off" must mean nothing was created — not a
    /// relay sitting on a socket with zeroed counters. `stats()` returning
    /// `None` is how a reader tells those apart.
    #[test]
    fn a_device_that_did_not_opt_in_reports_no_relay() {
        // No env set in this test process, so the gate is closed.
        assert!(!orgrelay::relay_server_enabled());
        maybe_start();
        assert!(
            stats().is_none() && handle().is_none(),
            "opting out must leave no relay, not an idle one"
        );
    }
}

/// FR-19 P4b — install a server-minted session into the running relay
/// (`rc:overlay.relay_serve`). Every lifetime is RE-CLAMPED against this
/// node's own ceilings and its own clock — server values only ever shorten
/// (the Roomler SSH rule). A malformed member (bad base64, wrong length)
/// drops the whole frame: a session with one unverifiable member is a
/// session nobody can bind to safely. A node that is not serving ignores it
/// with a warning: the server mints only onto nodes advertising
/// `relay-server`, so reaching here without a listener means a stale
/// capability, not a fault worth more than a log line.
pub fn install_from_wire(
    vni: u32,
    generation: u64,
    members: &[roomler_ai_remote_control::signaling::RelayMemberWire],
    bind_secs: u32,
    idle_secs: u32,
    max_lifetime_secs: u32,
) {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use tunnel_core::overlay::orgrelay::{
        bind::BindSecret,
        member::{MAX_BIND_BUDGET, MAX_LIFETIME},
        session::{IDLE_REFRESH, Member, Session},
    };
    let Some(h) = handle() else {
        tracing::warn!(
            vni,
            "org-relay: relay_serve received but this node is not serving; ignored"
        );
        return;
    };
    if members.len() != 2 {
        tracing::warn!(
            vni,
            n = members.len(),
            "org-relay: relay_serve must name exactly two members; ignored"
        );
        return;
    }
    let parse = |m: &roomler_ai_remote_control::signaling::RelayMemberWire| -> Option<Member> {
        let pk = BASE64.decode(&m.wg_public_key).ok()?;
        let sk = BASE64.decode(&m.bind_secret).ok()?;
        Some(Member {
            wg_public: <[u8; 32]>::try_from(pk.as_slice()).ok()?,
            secret: BindSecret::from_bytes(<[u8; 32]>::try_from(sk.as_slice()).ok()?),
        })
    };
    let (Some(a), Some(b)) = (parse(&members[0]), parse(&members[1])) else {
        tracing::warn!(
            vni,
            "org-relay: relay_serve carried a malformed member; ignored"
        );
        return;
    };
    let now = std::time::Instant::now();
    let secs = |s: u32| std::time::Duration::from_secs(u64::from(s));
    let session = Session {
        vni,
        generation,
        members: [a, b],
        bound: [None, None],
        max_lifetime: now + secs(max_lifetime_secs).min(MAX_LIFETIME),
        idle_deadline: now + secs(idle_secs).min(IDLE_REFRESH),
        bind_deadline: now + secs(bind_secs).min(MAX_BIND_BUDGET),
    };
    if h.install(session) {
        tracing::info!(vni, generation, "org-relay: session installed");
    } else {
        tracing::warn!(
            vni,
            "org-relay: session refused (table at capacity, or the relay stopped)"
        );
    }
}

/// FR-19 P4b — drop a session on `rc:overlay.relay_revoke`, if this node
/// holds it. Silent when it does not: the same frame reaches members too.
pub fn revoke_from_wire(vni: u32) {
    if let Some(h) = handle()
        && h.revoke(vni)
    {
        tracing::info!(vni, "org-relay: session revoked by the server");
    }
}
