//! The agent daemon's [`LocalApiState`] — the live data the LocalAPI serves.
//!
//! Thin adapter (unification P1): turns the agent's enrolled identity + a
//! "connected" flag + the overlay runtime's published [`OverlayView`] into the
//! read-only `status` / `peers` / `flows` that the CLI (`roomler`) and the
//! desktop app read over the local pipe/socket (`tunnel_core::localapi::serve`).
//!
//! Wired in `run_cmd`: the connected flag and the overlay `watch` channel are
//! created there (stable across WS reconnects), the signaling loop updates them,
//! and the listener reads this state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;
use tunnel_core::localapi::{
    ConsentRequest, DaemonMode, FlowInfo, LocalApiState, NodeStatus, OverlayView, PeerInfo,
};

/// Live daemon state behind the LocalAPI. Built once in `run_cmd`, wrapped in an
/// `Arc<dyn LocalApiState>` for the listener; reads are cheap clones off a
/// `watch` borrow + an atomic load.
pub struct DaemonState {
    node_id: String,
    name: String,
    version: String,
    mode: DaemonMode,
    tenant_id: Option<String>,
    /// Set while a signaling WS connection is live (updated by the signaling
    /// loop's per-connection guard). `peers()` reports none when this is false,
    /// since the overlay carriers are torn down on WS drop and the last view is
    /// stale.
    connected: Arc<AtomicBool>,
    /// The overlay runtime's latest view. An empty `Default` when overlay is
    /// disabled or this build lacks `overlay-l3` (nothing publishes) — so
    /// `peers()` is simply empty there.
    overlay: watch::Receiver<OverlayView>,
}

impl DaemonState {
    /// Build from the enrolled config identity + the live handles. `mode` is the
    /// privilege the daemon runs at (today's agent is always the full "be
    /// accessed" service node → [`DaemonMode::Service`]; the unprivileged
    /// user-mode daemon arrives with the binary unification at P3).
    pub fn new(
        node_id: String,
        name: String,
        mode: DaemonMode,
        tenant_id: Option<String>,
        connected: Arc<AtomicBool>,
        overlay: watch::Receiver<OverlayView>,
    ) -> Self {
        Self {
            node_id,
            name,
            version: env!("CARGO_PKG_VERSION").to_string(),
            mode,
            tenant_id,
            connected,
            overlay,
        }
    }
}

impl LocalApiState for DaemonState {
    fn status(&self) -> NodeStatus {
        NodeStatus {
            node_id: self.node_id.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
            mode: self.mode,
            tenant_id: self.tenant_id.clone(),
            // The overlay IP the runtime last assigned us — a stable identity,
            // so it's kept even across a brief disconnect.
            overlay_ip: self.overlay.borrow().self_ip.clone(),
            connected: self.connected.load(Ordering::Relaxed),
        }
    }

    fn peers(&self) -> Vec<PeerInfo> {
        // A peer list from a dropped connection is misleading — the carriers are
        // gone. Report none until reconnected + re-synced.
        if !self.connected.load(Ordering::Relaxed) {
            return Vec::new();
        }
        self.overlay.borrow().peers.clone()
    }

    fn flows(&self) -> Vec<FlowInfo> {
        // The agent is the "be accessed" side; outbound forwards / SOCKS5
        // listeners (the `FlowStats`-instrumented data plane) live in the
        // tunnel-client, which folds into the daemon at P3. Until then this
        // daemon runs none — an honest empty, not a stub.
        Vec::new()
    }

    fn consent_pending(&self) -> Vec<ConsentRequest> {
        // Scan OUR OWN sentinel dir — resolved in-process, so it's the daemon's
        // real profile even under SystemContext, where the interactive-user tray
        // reading the dir directly would look in the WRONG profile (the P2b bug
        // fix). Same parse the tray's cmd_get_pending_consents used to do.
        let Ok(dir) = crate::consent::ConsentBroker::default_sentinel_dir() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new(); // dir not created yet ⇒ nothing pending
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pending") {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&path)
                && let Ok(pc) = serde_json::from_str::<ConsentRequest>(&body)
            {
                out.push(pc);
            }
        }
        out
    }

    fn consent_decide(&self, session_id: &str, allow: bool) -> bool {
        // SECURITY: the session id becomes a sentinel FILE NAME, so reject
        // anything that isn't a 24-char hex ObjectId before it ever touches the
        // filesystem (path-traversal / injection guard). The pipe SDDL already
        // limits WHO can call this (SYSTEM + Administrators + interactive user).
        if !is_hex_object_id(session_id) {
            tracing::warn!(
                session = %session_id,
                "localapi: rejecting consent decision — session id is not a 24-char hex ObjectId"
            );
            return false;
        }
        let Ok(dir) = crate::consent::ConsentBroker::default_sentinel_dir() else {
            return false;
        };
        // Mode is irrelevant — we only use write_sentinel (same pattern as the
        // pre-P2b tray cmd_consent_approve/deny).
        let Ok(broker) = crate::consent::ConsentBroker::new(crate::consent::Mode::AutoGrant, dir)
        else {
            return false;
        };
        let kind = if allow {
            crate::consent::SentinelKind::Approve
        } else {
            crate::consent::SentinelKind::Deny
        };
        match broker.write_sentinel(session_id, kind) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(session = %session_id, %e, "localapi: writing consent sentinel failed");
                false
            }
        }
    }
}

/// A 24-char hex ObjectId — the only shape a session id may take before it's
/// used as a sentinel filename. Guards [`DaemonState::consent_decide`] against a
/// caller smuggling path separators / traversal into the filename.
fn is_hex_object_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_core::localapi::ConnectionType;

    fn view() -> OverlayView {
        OverlayView {
            self_ip: Some("100.64.0.2".into()),
            peers: vec![PeerInfo {
                node_id: "n2".into(),
                name: "peer".into(),
                overlay_ip: Some("100.64.0.1".into()),
                online: true,
                connection: ConnectionType::Relay,
                rtt_ms: None,
                last_seen_ms: None,
            }],
        }
    }

    #[test]
    fn consent_decide_hex_guard_rejects_unsafe_session_ids() {
        // The guard fires BEFORE any filesystem write, so a bad id is a pure
        // no-op — traversal / separators / wrong-length are all rejected.
        assert!(is_hex_object_id("0123456789abcdef01234567"));
        assert!(is_hex_object_id("6A11682E804368D30EDF57C6")); // upper-case hex ok
        assert!(!is_hex_object_id("6a11682e804368d30edf57c")); // 23 chars
        assert!(!is_hex_object_id("6a11682e804368d30edf57c6z")); // 25 / non-hex
        assert!(!is_hex_object_id("../../etc/passwd"));
        assert!(!is_hex_object_id("6a11682e804368d30edf57c6.approve"));
        assert!(!is_hex_object_id(""));
    }

    #[test]
    fn status_and_peers_track_connected_flag() {
        let connected = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = watch::channel(view());
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            Some("tid".into()),
            connected.clone(),
            rx,
        );

        // Identity + overlay IP are always reported; connected reflects the flag.
        let s = st.status();
        assert_eq!(s.node_id, "aid");
        assert_eq!(s.name, "host");
        assert_eq!(s.tenant_id.as_deref(), Some("tid"));
        assert_eq!(s.overlay_ip.as_deref(), Some("100.64.0.2"));
        assert!(!s.connected);

        // Peers hidden while disconnected…
        assert!(st.peers().is_empty(), "no peers reported while WS is down");

        // …shown once connected.
        connected.store(true, Ordering::Relaxed);
        assert!(st.status().connected);
        let peers = st.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].connection, ConnectionType::Relay);

        // Flows are empty on the agent side in P1.
        assert!(st.flows().is_empty());
    }
}
