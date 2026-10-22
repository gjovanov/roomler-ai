// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-3b — a sandboxed session's own CA: what the egress proxy will
//! terminate TLS under, for an audience host of a placeholder the session
//! holds (P3b-3c), and what the session's tools are told to trust.
//!
//! | Piece | What holds |
//! |---|---|
//! | the CA | ECDSA P-256, made at each start of the run's proxy, its key in daemon memory only and never written: a new run, a new CA. A CA for one level (path length 0), signing certificates and CRLs only, its name the session's; valid from an hour before it was made, for [`CA_DAYS`] days |
//! | a leaf | one per host, made when first asked for and kept for the run, at most [`MAX_LEAVES`]: ECDSA P-256, the host its one subject alternative name (a DNS name, or an IP address), for server authentication only, valid from an hour before it was made for [`LEAF_DAYS`] days, and made again a day before that ends |
//! | the trust | the CA alone ([`SessionCa::pem`]), which `NODE_EXTRA_CA_CERTS` adds to Node's roots, and a bundle of the system's roots and the CA ([`bundle`]), for the variables that replace a tool's roots. With no system root to read there is no bundle: one of the CA alone would refuse every other host |
//!
//! ⚠️ No system trust store is ever changed: an EDR flags it, and it would
//! trust the CA for every process on the device.
//!
//! ⚠️ The CA signs a leaf for whatever host it is asked: what decides that a
//! host is terminated at all is the proxy (P3b-3c), and only for an audience
//! of a placeholder the session holds.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bson::oid::ObjectId;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

/// The files, in the run's own directory, that the session's settings name
/// for its trust: the CA alone, and the bundle.
pub const CA_FILE: &str = "ca.pem";
pub const BUNDLE_FILE: &str = "ca-bundle.pem";

/// How long a session's CA is valid: past any run's life.
pub const CA_DAYS: i64 = 400;
/// How long a leaf is valid; one is made again a day before it ends.
pub const LEAF_DAYS: i64 = 30;
/// The most hosts one run's CA makes a leaf for.
pub const MAX_LEAVES: usize = 64;
/// How far back a certificate is valid from: a client whose clock is a
/// little behind still takes it.
const BACKDATE: time::Duration = time::Duration::hours(1);

/// A leaf, as TLS presents it: the chain (the leaf, then the CA) and its key.
pub struct Leaf {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivatePkcs8KeyDer<'static>,
    /// When it stops being valid.
    pub not_after: SystemTime,
}

/// One run's CA, and the leaves it has made.
pub struct SessionCa {
    key: KeyPair,
    cert: Certificate,
    leaves: Mutex<HashMap<String, Arc<Leaf>>>,
}

impl SessionCa {
    /// A new CA for `session`'s run.
    pub fn new(session: ObjectId) -> Result<Self, String> {
        let failed = |e: rcgen::Error| format!("the session's CA could not be made: {e}");
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(failed)?;
        let mut params = CertificateParams::default();
        let hex = session.to_hex();
        let mut name = DistinguishedName::new();
        name.push(
            DnType::CommonName,
            format!("Roomler Hive session {}", &hex[hex.len() - 8..]),
        );
        name.push(DnType::OrganizationName, "Roomler Hive");
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - BACKDATE;
        params.not_after = now + time::Duration::days(CA_DAYS);
        let cert = params.self_signed(&key).map_err(failed)?;
        Ok(Self {
            key,
            cert,
            leaves: Mutex::default(),
        })
    }

    /// The CA's certificate, as PEM: what the session's tools trust.
    pub fn pem(&self) -> String {
        self.cert.pem()
    }

    /// The CA's certificate, as DER.
    pub fn der(&self) -> CertificateDer<'static> {
        self.cert.der().clone()
    }

    /// The leaf for `host` (in its one spelling: a DNS name, or an IP
    /// address): the one already made while it has more than a day left,
    /// else a new one. Refused past [`MAX_LEAVES`] hosts.
    pub fn leaf_for(&self, host: &str) -> Result<Arc<Leaf>, String> {
        let mut leaves = self.leaves.lock().unwrap_or_else(|e| e.into_inner());
        let soon = SystemTime::now() + Duration::from_secs(24 * 3600);
        if let Some(leaf) = leaves.get(host)
            && leaf.not_after > soon
        {
            return Ok(Arc::clone(leaf));
        }
        if !leaves.contains_key(host) && leaves.len() >= MAX_LEAVES {
            return Err(format!(
                "the session's CA has made leaves for {MAX_LEAVES} hosts, and makes no more"
            ));
        }
        let leaf = Arc::new(self.sign(host)?);
        leaves.insert(host.to_string(), Arc::clone(&leaf));
        Ok(leaf)
    }

    /// A new leaf for `host`, signed by the CA.
    fn sign(&self, host: &str) -> Result<Leaf, String> {
        let failed = |e: rcgen::Error| format!("a leaf for {host} could not be made: {e}");
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(failed)?;
        let mut params = CertificateParams::default();
        let san = match host.parse::<IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(host.try_into().map_err(failed)?),
        };
        params.subject_alt_names = vec![san];
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, host);
        params.distinguished_name = name;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let now = time::OffsetDateTime::now_utc();
        let not_after = now + time::Duration::days(LEAF_DAYS);
        params.not_before = now - BACKDATE;
        params.not_after = not_after;
        let cert = params
            .signed_by(&key, &self.cert, &self.key)
            .map_err(failed)?;
        Ok(Leaf {
            chain: vec![cert.der().clone(), self.der()],
            key: PrivatePkcs8KeyDer::from(key.serialize_der()),
            not_after: SystemTime::from(not_after),
        })
    }
}

/// The system's roots and then `ca_pem`, as one PEM bundle: what replaces a
/// tool's roots, so it trusts every host it did, and the session's CA. None
/// where no system root can be read.
pub fn bundle(ca_pem: &str) -> Option<String> {
    let roots = rustls_native_certs::load_native_certs().certs;
    if roots.is_empty() {
        return None;
    }
    let mut out = String::new();
    for root in &roots {
        out.push_str(&pem_block("CERTIFICATE", root.as_ref()));
    }
    out.push_str(ca_pem);
    Some(out)
}

/// `der` as one PEM block under `label`: base64 in lines of 64.
pub fn pem_block(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    let body = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests;
