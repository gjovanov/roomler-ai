// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The DERP registry's key and map types (FR-69 P7a). The relay itself — the
//! `/derp` upgrade, the forwarding loop, the cluster convergence sweep — is
//! still the host's until P7b; the overlay engine and the ACL cache moved
//! here first and address the registry by these types, so the types came
//! with them and the host's `ws/derp.rs` re-exports them under its old
//! names.

use std::sync::Arc;

use bson::oid::ObjectId;
use dashmap::DashMap;
use tokio::sync::mpsc;

/// 32-byte WireGuard public key — the DERP addressing unit.
pub type DerpPubKey = [u8; 32];

/// Registry key. A pubkey is only reachable WITHIN its overlay network, so the
/// network id is part of the key — a forward lookup can never cross a network
/// boundary (the same hard isolation the netmap enforces).
pub type DerpKey = (ObjectId, DerpPubKey);

/// `(network_id, dst_pubkey)` → a bounded sender feeding that peer's live WS
/// write task. Shared across every `/derp` connection (lives on the network
/// module's state).
pub type DerpRegistry = Arc<DashMap<DerpKey, mpsc::Sender<Vec<u8>>>>;

/// C-5 — per-connection close signal: the cluster convergence sweep
/// (`ws/derp_cluster.rs`) fires it to rehome a socket parked on the
/// wrong pod; the socket loop's cancel arm breaks, teardown releases
/// the directory record, and the client's reconnect re-lands per the
/// current LB map.
pub type DerpCancelRegistry = Arc<DashMap<DerpKey, Arc<tokio::sync::Notify>>>;
