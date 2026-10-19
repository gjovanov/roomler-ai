// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5b — TLS to a vault proxy's upstream, always verified: against
//! the system's roots (a corporate CA in the OS store included, as the
//! daemon's own WS path trusts it), and a CA the secret may hold.
//!
//! ⚠️ Never unverified. A proxy authenticates with a value, and a value
//! presented to an upstream nobody verified is presented to whoever
//! answered.

use std::sync::Arc;

use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

/// A connector that trusts the system's roots, and `ca` (PEM, one or more
/// certificates) when the secret holds one.
pub(crate) fn connector(ca: Option<&str>) -> Result<TlsConnector, String> {
    let mut roots = RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        // One root the store cannot take is not a reason to trust none.
        let _ = roots.add(cert);
    }
    if let Some(pem) = ca {
        let mut added = 0;
        for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            let cert = cert.map_err(|e| format!("the secret's CA does not read as PEM: {e}"))?;
            roots
                .add(cert)
                .map_err(|e| format!("the secret's CA is not a certificate TLS can trust: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err("the secret's CA holds no certificate".into());
        }
    }
    if roots.is_empty() {
        return Err("no root to verify an upstream against".into());
    }
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// The name the upstream's certificate must carry: a DNS name or an IP
/// address.
pub(crate) fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string())
        .map_err(|_| format!("{host} is not a name a certificate can carry"))
}
