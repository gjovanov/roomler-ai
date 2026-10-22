// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The session's CA, checked as a TLS client checks it: rustls's own
//! verifier, with the CA its one root.

use std::time::UNIX_EPOCH;

use tokio_rustls::rustls::RootCertStore;
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::ServerCertVerifier;
use tokio_rustls::rustls::crypto::ring::default_provider;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{ServerName, UnixTime};

use super::*;

/// A verifier trusting `ca` alone.
fn verifier(ca: &CertificateDer<'static>) -> Arc<WebPkiServerVerifier> {
    let mut roots = RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::new(default_provider()))
        .build()
        .unwrap()
}

/// Whether `leaf` verifies as `name`, at `at`.
fn verifies(v: &WebPkiServerVerifier, leaf: &Leaf, name: &str, at: UnixTime) -> bool {
    let name = ServerName::try_from(name.to_string()).unwrap();
    v.verify_server_cert(&leaf.chain[0], &leaf.chain[1..], &name, &[], at)
        .is_ok()
}

/// The time `d` from now, ahead or behind.
fn from_now(d: Duration, ahead: bool) -> UnixTime {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    UnixTime::since_unix_epoch(if ahead { now + d } else { now - d })
}

/// A leaf the CA signs verifies as its host and as no other, against this
/// CA and no other run's; an address's leaf verifies as that address.
#[test]
fn a_session_ca_signs_leaves_that_verify_as_their_host_and_no_other() {
    let ca = SessionCa::new(ObjectId::new()).unwrap();
    let v = verifier(&ca.der());
    let leaf = ca.leaf_for("api.github.com").unwrap();
    assert_eq!(leaf.chain.len(), 2, "the leaf, then the CA");
    assert_eq!(leaf.chain[1], ca.der());
    let now = UnixTime::now();
    assert!(verifies(&v, &leaf, "api.github.com", now));
    assert!(!verifies(&v, &leaf, "github.com", now), "another host");
    assert!(!verifies(&v, &leaf, "evil.api.github.com", now));
    let other = SessionCa::new(ObjectId::new()).unwrap();
    assert!(
        !verifies(&verifier(&other.der()), &leaf, "api.github.com", now),
        "another run's CA trusts none of this one's leaves"
    );
    let ip = ca.leaf_for("10.0.0.5").unwrap();
    assert!(verifies(&v, &ip, "10.0.0.5", now));
    assert!(!verifies(&v, &ip, "10.0.0.6", now));
}

/// A leaf is valid from an hour before it was made, so a client a little
/// behind takes it, for thirty days.
#[test]
fn a_leaf_is_valid_from_before_it_was_made_for_thirty_days() {
    let ca = SessionCa::new(ObjectId::new()).unwrap();
    let v = verifier(&ca.der());
    let leaf = ca.leaf_for("api.github.com").unwrap();
    let day = Duration::from_secs(24 * 3600);
    let host = "api.github.com";
    assert!(verifies(
        &v,
        &leaf,
        host,
        from_now(Duration::from_secs(30 * 60), false)
    ));
    assert!(!verifies(
        &v,
        &leaf,
        host,
        from_now(Duration::from_secs(2 * 3600), false)
    ));
    assert!(verifies(&v, &leaf, host, from_now(day * 29, true)));
    assert!(
        !verifies(&v, &leaf, host, from_now(day * 31, true)),
        "ended"
    );
}

/// A host's leaf is made once and kept; past so many hosts no new one is
/// made, while the ones made are still served.
#[test]
fn a_leaf_is_made_once_a_host_and_for_so_many_hosts() {
    let ca = SessionCa::new(ObjectId::new()).unwrap();
    let first = ca.leaf_for("api.github.com").unwrap();
    assert!(Arc::ptr_eq(&first, &ca.leaf_for("api.github.com").unwrap()));
    for i in 1..MAX_LEAVES {
        ca.leaf_for(&format!("h{i}.example")).unwrap();
    }
    let refused = ca.leaf_for("one-more.example").err().unwrap();
    assert!(refused.contains("makes no more"), "{refused}");
    assert!(Arc::ptr_eq(&first, &ca.leaf_for("api.github.com").unwrap()));
}

/// The CA is a CA for one level that signs certificates and CRLs only, and
/// a leaf is no CA and serves a server only. That is what OpenSSL's clients
/// (curl, Python, git) check of them, and rustls does not ask of a root it
/// is given, so the verifier above cannot tell.
#[test]
fn the_ca_signs_one_level_down_and_a_leaf_only_serves() {
    use x509_parser::prelude::*;
    let ca = SessionCa::new(ObjectId::new()).unwrap();
    let der = ca.der();
    let (_, cert) = parse_x509_certificate(der.as_ref()).unwrap();
    let bc = cert
        .basic_constraints()
        .unwrap()
        .expect("its basic constraints");
    assert!(bc.value.ca);
    assert_eq!(bc.value.path_len_constraint, Some(0), "one level");
    let ku = cert.key_usage().unwrap().expect("its key usage");
    assert!(ku.value.key_cert_sign() && ku.value.crl_sign());
    assert!(!ku.value.digital_signature() && !ku.value.key_encipherment());
    let leaf = ca.leaf_for("api.github.com").unwrap();
    let (_, l) = parse_x509_certificate(leaf.chain[0].as_ref()).unwrap();
    assert!(
        l.basic_constraints().unwrap().is_none_or(|b| !b.value.ca),
        "a leaf is no CA"
    );
    let eku = l.extended_key_usage().unwrap().expect("its purposes");
    assert!(eku.value.server_auth && !eku.value.client_auth && !eku.value.any);
    let lku = l.key_usage().unwrap().expect("its key usage");
    assert!(lku.value.digital_signature() && !lku.value.key_cert_sign());
}

/// Each run has a CA of its own, and the CA reads back from its PEM.
#[test]
fn each_run_has_its_own_ca_and_it_reads_back_from_its_pem() {
    let a = SessionCa::new(ObjectId::new()).unwrap();
    let b = SessionCa::new(ObjectId::new()).unwrap();
    assert_ne!(a.der(), b.der());
    let back: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(a.pem().as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(back, [a.der()]);
}

/// The bundle is the system's roots, then the CA; there is none only where
/// no system root reads.
#[test]
fn the_bundle_is_the_systems_roots_then_the_ca() {
    let ca = SessionCa::new(ObjectId::new()).unwrap();
    let roots = rustls_native_certs::load_native_certs().certs;
    let Some(bundle) = bundle(&ca.pem()) else {
        assert!(roots.is_empty(), "no bundle only where no root reads");
        return;
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(bundle.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(certs.len(), roots.len() + 1);
    assert_eq!(certs[..roots.len()], roots[..], "the system's, as read");
    assert_eq!(certs.last(), Some(&ca.der()), "the CA, last");
}

/// A PEM block is its label's lines around base64 in lines of 64, and
/// reads back as it went in.
#[test]
fn a_pem_block_is_base64_in_lines_of_64() {
    let der: Vec<u8> = (0..=255u8).collect();
    let block = pem_block("CERTIFICATE", &der);
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(lines[0], format!("-----BEGIN {}-----", "CERTIFICATE"));
    assert_eq!(
        lines[lines.len() - 1],
        format!("-----END {}-----", "CERTIFICATE")
    );
    let body = &lines[1..lines.len() - 1];
    assert!(body[..body.len() - 1].iter().all(|l| l.len() == 64));
    assert!(body[body.len() - 1].len() <= 64);
    let back: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(block.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(back[0].as_ref(), &der[..]);
}
