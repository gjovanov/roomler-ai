// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5 — the vault's proxies: an endpoint the daemon serves that
//! authenticates upstream with a leased value, so the session, its model and
//! its transcript never hold the value (`docs/fr/FR-90-hive-agent-sessions.md`
//! §3e, "P3-5, `proxy` mode, as designed").
//!
//! | Module | What |
//! |---|---|
//! | [`scram`] | SCRAM-SHA-256, the client's half: a proof of the password, never the password |
//! | [`tls`] | TLS to an upstream, always verified: the system's roots, and a CA the secret holds |
//! | [`pg`] | one Postgres session: the client let in by the endpoint, the upstream by SCRAM |
//! | [`mongo`] | one MongoDB session: the same, by SCRAM in `saslStart` and `saslContinue` (P3-5c-1) |
//! | [`http`] | one HTTP client connection: each request sent on to one upstream origin, the secret's credential in `Authorization` (P3-5d-1) |
//!
//! P3-5b-1 builds these, P3-5c-1 the MongoDB engine and P3-5d-1 the HTTP
//! one; the endpoint (`hive::vault_connect`) serves them, one session per
//! connection.

/// P3-5d-1 builds the HTTP engine; P3-5d-2's endpoint is its first caller,
/// until which nothing outside its tests reaches it.
#[allow(dead_code)]
pub(crate) mod http;
pub(crate) mod mongo;
pub(crate) mod pg;
pub(crate) mod scram;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod tls;
