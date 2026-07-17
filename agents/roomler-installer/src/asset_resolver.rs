//! MSI asset resolution + download — legacy wrapper.
//!
//! P4a: the streaming mechanics moved to
//! `wizard_shared::asset_resolver`; this wrapper keeps the wizard's
//! identity — the `/api/agent/installer` proxy base + its env
//! override, the `%TEMP%\roomler-installer` staging namespace, the
//! historical `roomler-installer/<v>` User-Agent, and the
//! `InstallerHealth` shape — so behaviour stays byte-identical while
//! this legacy wizard ships. Retired with the crate in P4c.
//!
//! W4 in the rc.28 plan + BLOCKER-10 fix from the critique. The
//! wizard hits roomler.ai's `/api/agent/installer/{flavour}` proxy
//! (rc.27 P4) instead of downloading directly from github.com:
//! corporate ESET / Defender allow-lists are typically per-domain;
//! roomler.ai's TLS cert is already trusted by IT (the agent's
//! signaling traffic uses it), github.com is often blocked outright.
//! the field-test host field repro 2026-05-11.
//!
//! Flow per install:
//!   1. `resolve(flavour, version)` → GET `/installer/{flavour}/health`
//!      → JSON metadata (tag, size, sha256 digest, canonical filename).
//!   2. `download(&health, on_progress)` → GET `/installer/{flavour}`
//!      → stream bytes to `%TEMP%\roomler-installer\{tag}\{filename}`,
//!      firing `on_progress(received_bytes)` per chunk.
//!   3. `verify_sha256(&staged, &expected)` → hash the staged file,
//!      compare to the digest from health. Mismatch → caller deletes
//!      the file + re-downloads (or surfaces a tampered-bytes error).
//!
//! Override knob: env var `ROOMLER_INSTALLER_PROXY_BASE` swaps the
//! domain. Local/staging testing only; production always uses
//! `https://roomler.ai/api/agent/installer`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use wizard_shared::asset_resolver::verify_sha256;

/// Historical wire-visible User-Agent — preserved verbatim through
/// the P4a extraction (the moved core fns take the UA as a
/// parameter).
const USER_AGENT: &str = concat!("roomler-installer/", env!("CARGO_PKG_VERSION"));

/// Legacy wizards keep their step-boundary-only cancel granularity:
/// the core `download` gained a mid-stream cancel check, and this
/// never-true flag opts the shipped wizard out of it.
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Default proxy base. Override via `ROOMLER_INSTALLER_PROXY_BASE`
/// for staging / local-server testing.
const DEFAULT_PROXY_BASE: &str = "https://roomler.ai/api/agent/installer";

/// Resolve the proxy base at runtime so the wizard can hit a staging
/// API for testing. Always returns a URL without a trailing slash.
fn proxy_base() -> String {
    let raw = std::env::var("ROOMLER_INSTALLER_PROXY_BASE")
        .ok()
        .unwrap_or_else(|| DEFAULT_PROXY_BASE.to_string());
    normalise_proxy_base(&raw)
}

/// Pure: strip the trailing slash from `raw`. Extracted so tests can
/// exercise the parsing logic without racing on `std::env`.
fn normalise_proxy_base(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

/// Mirror of the backend's `InstallerHealth` JSON. Wire shape is
/// pinned by `crates/api/src/routes/agent_release.rs`; keep in sync.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallerHealth {
    /// Resolved tag, e.g. `agent-v0.3.0-rc.28`.
    pub tag: String,
    /// Normalised flavour: `"peruser"` or `"permachine"`.
    pub flavour: String,
    /// Canonical asset filename, e.g.
    /// `roomler-agent-0.3.0-rc.28-perMachine-x86_64-pc-windows-msvc.msi`.
    pub filename: String,
    /// Asset size in bytes.
    pub size: u64,
    /// `"sha256:<hex>"` from GitHub's `digest` field. None on
    /// releases that pre-date the field.
    pub digest: Option<String>,
    /// Relative URI to stream the MSI bytes from. Composed against
    /// `proxy_base()` for the actual download GET.
    pub uri: String,
}

impl From<wizard_shared::asset_resolver::ArtifactHealth> for InstallerHealth {
    fn from(h: wizard_shared::asset_resolver::ArtifactHealth) -> Self {
        InstallerHealth {
            tag: h.tag,
            flavour: h.target,
            filename: h.filename,
            size: h.size,
            digest: h.digest,
            uri: h.uri,
        }
    }
}

/// Fetch the JSON metadata for the matching MSI. Used by the wizard
/// before kicking the download so the progress bar has a denominator
/// and the wizard can pre-validate the SHA256 digest format.
pub async fn resolve(flavour: &str, version: &str) -> Result<InstallerHealth> {
    let health =
        wizard_shared::asset_resolver::resolve(&proxy_base(), flavour, version, USER_AGENT).await?;
    Ok(health.into())
}

/// Stream the MSI bytes to `%TEMP%\roomler-installer\<tag>\<filename>`.
/// Fires `on_progress(received_bytes)` after each chunk; the SPA
/// throttles UI updates client-side. Returns the staged path on
/// success.
///
/// Does NOT call `verify_sha256` itself — the caller chains them so
/// the wizard's `cmd_install` orchestrator can emit a
/// `DownloadVerified` ProgressEvent only after both succeed.
pub async fn download<F: FnMut(u64)>(health: &InstallerHealth, on_progress: F) -> Result<PathBuf> {
    let dest = std::env::temp_dir()
        .join("roomler-installer")
        .join(&health.tag)
        .join(&health.filename);
    let url = format!("{}{}", proxy_base_without_path_segment(), health.uri);
    let spec = wizard_shared::asset_resolver::DownloadSpec {
        url: &url,
        dest: &dest,
        user_agent: USER_AGENT,
        artifact_label: "installer",
    };
    wizard_shared::asset_resolver::download(&spec, &NEVER_CANCELLED, on_progress).await?;
    Ok(dest)
}

/// The proxy base value WITHOUT the `/api/agent/installer` suffix —
/// used when composing absolute URLs from `health.uri`, which already
/// includes the `/api/agent/installer/...` prefix.
fn proxy_base_without_path_segment() -> String {
    strip_installer_path_suffix(&proxy_base())
}

/// Pure: strip the trailing `/api/agent/installer` segment from
/// `base` so concatenation with `health.uri` (which already starts
/// with `/api/agent/installer/...`) doesn't double up. If the suffix
/// is missing (custom env var without the path segment), returns
/// `base` unchanged.
fn strip_installer_path_suffix(base: &str) -> String {
    base.strip_suffix("/api/agent/installer")
        .map(|s| s.to_string())
        .unwrap_or_else(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wizard_shared::asset_resolver::hex_encode;

    #[test]
    fn verify_sha256_matches_correct_hash() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let digest = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(&path, digest).unwrap());
    }

    #[test]
    fn verify_sha256_rejects_wrong_hash() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        let bogus = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_sha256(&path, bogus).unwrap());
    }

    #[test]
    fn verify_sha256_accepts_bare_hex_without_prefix() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        // Same hash, no "sha256:" prefix.
        let bare = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(&path, bare).unwrap());
    }

    #[test]
    fn verify_sha256_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        let upper = "sha256:2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_sha256(&path, upper).unwrap());
    }

    #[test]
    fn verify_sha256_rejects_short_digest() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        let err = verify_sha256(&path, "sha256:abc").unwrap_err();
        assert!(format!("{err}").contains("64 hex"));
    }

    #[test]
    fn verify_sha256_rejects_non_hex() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hello.bin");
        std::fs::write(&path, b"hello").unwrap();
        let err = verify_sha256(
            &path,
            "sha256:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("hex"));
    }

    #[test]
    fn normalise_proxy_base_strips_trailing_slash() {
        assert_eq!(
            normalise_proxy_base("https://staging.local/api/agent/installer/"),
            "https://staging.local/api/agent/installer"
        );
    }

    #[test]
    fn normalise_proxy_base_passthrough_when_no_trailing_slash() {
        assert_eq!(
            normalise_proxy_base("https://roomler.ai/api/agent/installer"),
            "https://roomler.ai/api/agent/installer"
        );
    }

    #[test]
    fn strip_installer_path_suffix_removes_canonical_suffix() {
        assert_eq!(
            strip_installer_path_suffix("https://roomler.ai/api/agent/installer"),
            "https://roomler.ai"
        );
    }

    #[test]
    fn strip_installer_path_suffix_passthrough_when_suffix_absent() {
        // Operator-set env var without the path segment — e.g. they
        // run a custom proxy at https://staging.local that injects
        // the path internally. Pass through unchanged.
        assert_eq!(
            strip_installer_path_suffix("https://staging.local"),
            "https://staging.local"
        );
    }

    #[test]
    fn hex_encode_lowercase() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn installer_health_converts_from_unified_shape() {
        let unified = wizard_shared::asset_resolver::ArtifactHealth {
            tag: "agent-v0.3.0-rc.28".to_string(),
            target: "permachine".to_string(),
            filename: "roomler-agent-0.3.0-rc.28-perMachine-x86_64-pc-windows-msvc.msi".to_string(),
            size: 123,
            digest: None,
            uri: "/api/agent/installer/permachine?version=latest".to_string(),
        };
        let legacy: InstallerHealth = unified.into();
        assert_eq!(legacy.flavour, "permachine");
        assert_eq!(legacy.tag, "agent-v0.3.0-rc.28");
    }
}
