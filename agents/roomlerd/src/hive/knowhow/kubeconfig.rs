// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-2 — the kube contexts a kubeconfig holds (decision 46), in
//! the child, as the account that owns the file: each context's name, its
//! cluster's server, its namespace, and its CA as the SHA-256 of the
//! certificate, the fingerprint `openssl x509 -noout -fingerprint -sha256`
//! prints.
//!
//! ⚠️ `users` is never read: the reader passes it by without keeping a byte
//! ([`super::yaml::documents`]'s `skip`). A context's credentials stay where
//! they are, and nothing here could carry one.

use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use roomler_ai_remote_control::knowhow::{KubeContext, MAX_TEXT};
use sha2::{Digest, Sha256};

use super::yaml::{self, Node};

/// The most bytes of a kubeconfig read: certificates make it large.
const MAX_KUBECONFIG: u64 = 1024 * 1024;
/// The most bytes of a CA file a cluster names read.
const MAX_CA_FILE: u64 = 64 * 1024;

/// The contexts in the kubeconfig at `path`, or why not.
pub(super) fn contexts(path: &Path) -> Result<Vec<KubeContext>, String> {
    let text = read(path, MAX_KUBECONFIG)
        .ok_or_else(|| format!("{}: unreadable, too large or not text", path.display()))?;
    let docs =
        yaml::documents(&text, &["users"]).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc = docs
        .first()
        .ok_or_else(|| format!("{}: empty", path.display()))?;
    if doc.get("contexts").is_none() || doc.get("clusters").is_none() {
        return Err(format!("{}: not a kubeconfig", path.display()));
    }
    let dir = path.parent().unwrap_or(Path::new("/"));
    let clusters: Vec<(&str, &Node)> = doc
        .get("clusters")
        .map(Node::items)
        .unwrap_or_default()
        .iter()
        .filter_map(|c| Some((c.get("name")?.text()?, c.get("cluster")?)))
        .collect();
    let mut out = Vec::new();
    for ctx in doc.get("contexts").map(Node::items).unwrap_or_default() {
        let (Some(name), Some(spec)) = (ctx.get("name").and_then(Node::text), ctx.get("context"))
        else {
            continue;
        };
        let Some(cluster) = spec
            .get("cluster")
            .and_then(Node::text)
            .and_then(|c| clusters.iter().find(|(n, _)| *n == c).map(|(_, v)| *v))
        else {
            continue;
        };
        let Some(server) = cluster
            .get("server")
            .and_then(Node::text)
            .and_then(super::checkout::clean_url)
        else {
            continue;
        };
        if name.len() > MAX_TEXT {
            continue;
        }
        out.push(KubeContext {
            name: name.to_string(),
            server,
            namespace: spec
                .get("namespace")
                .and_then(Node::text)
                .filter(|n| !n.is_empty() && n.len() <= MAX_TEXT)
                .map(str::to_string),
            ca_sha256: ca_fingerprint(cluster, dir),
        });
    }
    Ok(out)
}

/// The SHA-256 of the cluster's CA certificate, hex: from its
/// `certificate-authority-data`, or the file `certificate-authority` names.
/// None when it skips verification or names neither.
fn ca_fingerprint(cluster: &Node, dir: &Path) -> Option<String> {
    let pem = if let Some(data) = cluster
        .get("certificate-authority-data")
        .and_then(Node::text)
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.split_whitespace().collect::<String>())
            .ok()?;
        String::from_utf8(bytes).ok()?
    } else {
        let file = cluster.get("certificate-authority").and_then(Node::text)?;
        let file = Path::new(file);
        read(
            &if file.is_absolute() {
                file.to_path_buf()
            } else {
                dir.join(file)
            },
            MAX_CA_FILE,
        )?
    };
    first_certificate(&pem).map(|der| hex::encode(Sha256::digest(der)))
}

/// The first certificate in a PEM text, as DER.
fn first_certificate(pem: &str) -> Option<Vec<u8>> {
    let begin = format!("-----{} CERTIFICATE-----", "BEGIN");
    let end = format!("-----{} CERTIFICATE-----", "END");
    let start = pem.find(&begin)? + begin.len();
    let stop = start + pem[start..].find(&end)?;
    let body: String = pem[start..stop].split_whitespace().collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

fn read(path: &Path, max: u64) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > max {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A certificate's PEM, its header built here so no scanner reading this
    /// repository takes a test for a key.
    fn pem(der: &[u8]) -> String {
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        format!(
            "-----{b} CERTIFICATE-----\n{body}\n-----{e} CERTIFICATE-----\n",
            b = "BEGIN",
            e = "END"
        )
    }

    fn fingerprint(der: &[u8]) -> String {
        hex::encode(Sha256::digest(der))
    }

    /// Each context's name, server and namespace, its CA's fingerprint from
    /// the data or from the file it names, none when it skips verification;
    /// a context naming no cluster passed by, and `users` never read.
    #[test]
    fn a_kubeconfigs_contexts_are_read_and_its_users_never() {
        let tmp = tempfile::tempdir().unwrap();
        let (prod, dev) = (b"prod ca der".as_slice(), b"dev ca der".as_slice());
        let data = base64::engine::general_purpose::STANDARD.encode(pem(prod));
        std::fs::write(tmp.path().join("ca.crt"), pem(dev)).unwrap();
        let marker = "users-marker-7";
        let config = tmp.path().join("config");
        std::fs::write(
            &config,
            format!(
                "apiVersion: v1\nkind: Config\nclusters:\n\
                 - name: prod\n  cluster:\n    server: https://k8s.example.com:6443\n    certificate-authority-data: {data}\n\
                 - name: dev\n  cluster:\n    server: https://dev.example.com:6443\n    certificate-authority: ca.crt\n\
                 - name: lab\n  cluster:\n    server: https://lab.example.com\n    insecure-skip-tls-verify: true\n\
                 contexts:\n\
                 - name: prod-admin\n  context:\n    cluster: prod\n    namespace: payments\n    user: admin\n\
                 - name: dev\n  context:\n    cluster: dev\n    user: dev\n\
                 - name: lab\n  context:\n    cluster: lab\n    user: dev\n\
                 - name: orphan\n  context:\n    cluster: nowhere\n    user: dev\n\
                 users:\n- name: admin\n  user:\n    password: {marker}\n"
            ),
        )
        .unwrap();

        let found = contexts(&config).unwrap();
        assert_eq!(
            found,
            [
                KubeContext {
                    name: "prod-admin".into(),
                    server: "https://k8s.example.com:6443".into(),
                    namespace: Some("payments".into()),
                    ca_sha256: Some(fingerprint(prod)),
                },
                KubeContext {
                    name: "dev".into(),
                    server: "https://dev.example.com:6443".into(),
                    namespace: None,
                    ca_sha256: Some(fingerprint(dev)),
                },
                KubeContext {
                    name: "lab".into(),
                    server: "https://lab.example.com".into(),
                    namespace: None,
                    ca_sha256: None,
                },
            ]
        );
        assert!(!format!("{found:?}").contains(marker));
    }

    /// A file that is not a kubeconfig is refused, never read as an empty one.
    #[test]
    fn a_file_that_is_not_a_kubeconfig_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("compose.yaml");
        std::fs::write(&file, "services:\n  web:\n    image: nginx\n").unwrap();
        assert!(contexts(&file).unwrap_err().contains("not a kubeconfig"));
        assert!(contexts(&tmp.path().join("missing")).is_err());
    }
}
