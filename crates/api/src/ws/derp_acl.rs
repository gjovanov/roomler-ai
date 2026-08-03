//! Overlay-ACL gate for the `/derp` relay tier.
//!
//! # Why this exists
//!
//! The overlay ACL shapes each recipient's netmap, so an HONEST client never
//! learns a withheld peer's WireGuard public key. But `/derp` is addressed BY
//! pubkey: a stale or modified client that already holds a denied peer's key
//! could hand it to the relay and be forwarded, routing straight around the
//! netmap. The TURN relay-grant path is gated in
//! `ws::overlay::handle_overlay_relay_request`; this closes the matching hole on
//! the DERP tier, which was the last "relay around the ACL" path.
//!
//! # Why a precomputed table
//!
//! [`super::derp::forward_frame`] is a **synchronous, per-datagram** hot path.
//! An async policy read there would be a serious throughput regression, so the
//! decision is precomputed into a per-network table and consulted with one
//! lock-free [`DashMap`] lookup per frame.
//!
//! The permission is exactly netmap visibility: if the ACL would let S see P,
//! S may relay to P. Reusing `evaluate_overlay` rather than inventing a second
//! predicate is what keeps the two tiers from drifting apart — a pair that is
//! invisible in the netmap can never become relayable here.
//!
//! # Failure posture: OPEN
//!
//! A missing table entry permits the frame. This matches
//! `ws::overlay::load_acl`, which also fails open, and the reasoning is the
//! same: a spurious deny here would sever a corp pair's ONLY working carrier
//! (DERP serves precisely the both-UDP-blocked pairs that have no alternative),
//! whereas a spurious allow costs one relayed datagram on a tier an honest
//! client would not have used. The table is rebuilt on every policy change and
//! on join, so the open window is a brief one after a restart.

use bson::oid::ObjectId;
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use roomler_ai_remote_control::models::OverlayNode;
use tunnel_core::policy::{OverlayPeerRef, evaluate_overlay};

use super::derp::DerpPubKey;
use crate::state::AppState;

/// One network's relay permissions: `src_pubkey → {dst_pubkey}`.
#[derive(Debug, Default)]
pub struct DerpAllowTable {
    /// Whether the tenant is ENFORCING. The table is built for `Warn` too, so
    /// the would-be denials are counted and logged before anyone flips —
    /// the same evidence-first cutover the netmap shaping uses.
    enforcing: bool,
    allow: HashMap<DerpPubKey, HashSet<DerpPubKey>>,
}

impl DerpAllowTable {
    /// May `src` relay a frame to `dst`? Always `true` when not enforcing.
    pub fn permits(&self, src: &DerpPubKey, dst: &DerpPubKey) -> bool {
        !self.enforcing || self.allow.get(src).is_some_and(|s| s.contains(dst))
    }

    /// `true` when a denial should actually drop the frame (vs. only be logged).
    pub fn enforcing(&self) -> bool {
        self.enforcing
    }

    /// Test-only builder so `ws::derp`'s tests can exercise the gate without
    /// standing up Mongo (the fields stay private to this module otherwise).
    #[cfg(test)]
    pub(crate) fn for_test(enforcing: bool, pairs: &[(DerpPubKey, DerpPubKey)]) -> Self {
        let mut allow: HashMap<DerpPubKey, HashSet<DerpPubKey>> = HashMap::new();
        for (s, d) in pairs {
            allow.entry(*s).or_default().insert(*d);
        }
        Self { enforcing, allow }
    }
}

/// `network_id → allow table`. Lives in [`AppState`]; absent = fail open.
pub type DerpAclCache = Arc<DashMap<ObjectId, Arc<DerpAllowTable>>>;

/// Decode a node's stored base64 WireGuard key into the 32-byte DERP address.
/// `None` for a malformed/short key — such a node simply gets no entry, which
/// (fail-open) leaves it ungated rather than unreachable.
fn pubkey_of(node: &OverlayNode) -> Option<DerpPubKey> {
    BASE64.decode(&node.wg_public_key).ok()?.try_into().ok()
}

/// Recompute one network's table and publish it atomically.
///
/// Build-then-swap, never invalidate-then-rebuild: there is no window in which
/// the network has no table (which, being fail-open, would be a window with no
/// gate at all). O(N²) over the network's nodes, but it runs only on policy
/// changes and joins — never per datagram, and never on an endpoint trickle,
/// which cannot change an ACL decision.
pub async fn rebuild(state: &AppState, tenant_id: ObjectId, network_id: ObjectId) {
    let acl = super::overlay::load_acl(state, tenant_id).await;
    // `Off` needs no table: `permits` would allow everything anyway, and
    // dropping the entry keeps a disabled tenant's memory at zero.
    if !acl.gating() {
        state.derp_acl.remove(&network_id);
        return;
    }
    let nodes = match state
        .overlay_nodes
        .list_active_in_network(tenant_id, network_id)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            // Keep the PREVIOUS table rather than dropping to fail-open on a
            // transient Mongo blip.
            tracing::error!(%tenant_id, %e, "derp acl: node list failed; keeping the previous table");
            return;
        }
    };

    // One `OverlaySource` per node (owner + roles), resolved once and reused
    // across the N² pair evaluation.
    let mut sources = Vec::with_capacity(nodes.len());
    for n in &nodes {
        sources.push(super::overlay::overlay_source_of(state, n).await);
    }

    let mut allow: HashMap<DerpPubKey, HashSet<DerpPubKey>> = HashMap::new();
    for (src_node, src) in nodes.iter().zip(&sources) {
        let Some(src_pk) = pubkey_of(src_node) else {
            continue;
        };
        let entry = allow.entry(src_pk).or_default();
        for peer in nodes.iter().filter(|p| p.id != src_node.id) {
            let Some(peer_pk) = pubkey_of(peer) else {
                continue;
            };
            let access = evaluate_overlay(
                &acl.policies,
                src,
                OverlayPeerRef {
                    node_id: peer.id.unwrap_or_default(),
                    overlay_ip: &peer.overlay_ip,
                    approved_routes: &peer.approved_routes,
                },
            );
            if access.visible {
                entry.insert(peer_pk);
            }
        }
    }

    let pairs: usize = allow.values().map(|s| s.len()).sum();
    tracing::info!(
        %tenant_id, %network_id,
        nodes = nodes.len(), pairs,
        enforcing = acl.enforcing(),
        "derp acl: allow table rebuilt"
    );
    state.derp_acl.insert(
        network_id,
        Arc::new(DerpAllowTable {
            enforcing: acl.enforcing(),
            allow,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> DerpPubKey {
        [b; 32]
    }

    fn table(enforcing: bool, pairs: &[(DerpPubKey, DerpPubKey)]) -> DerpAllowTable {
        DerpAllowTable::for_test(enforcing, pairs)
    }

    #[test]
    fn enforcing_table_permits_only_listed_pairs() {
        let t = table(true, &[(pk(1), pk(2))]);
        assert!(t.permits(&pk(1), &pk(2)));
        // Not symmetric by construction — the table stores what was evaluated.
        assert!(!t.permits(&pk(2), &pk(1)));
        // A pubkey with no entry at all is denied, not defaulted open: once a
        // tenant is enforcing, an unknown sender must not relay.
        assert!(!t.permits(&pk(9), &pk(2)));
        assert!(!t.permits(&pk(1), &pk(3)));
    }

    #[test]
    fn warn_table_permits_everything_including_unlisted() {
        // Built and populated, but non-enforcing: the counters are real while
        // no frame is ever dropped — the evidence-first cutover.
        let t = table(false, &[(pk(1), pk(2))]);
        assert!(t.permits(&pk(1), &pk(2)));
        assert!(t.permits(&pk(7), &pk(8)));
        assert!(!t.enforcing());
    }

    #[test]
    fn absent_table_is_fail_open_at_the_call_site() {
        // The cache miss (not the table) is what fails open; a DEFAULT table is
        // non-enforcing, so a bug that inserts one can't silently sever a
        // network either.
        let t = DerpAllowTable::default();
        assert!(t.permits(&pk(1), &pk(2)));
    }
}
