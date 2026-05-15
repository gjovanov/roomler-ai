//! MSI asset resolution + download.
//!
//! W4 in the rc.28 plan + BLOCKER-10 fix from the critique. The
//! wizard hits roomler.ai's `/api/agent/installer/{flavour}` proxy
//! (rc.27 P4) instead of downloading directly from github.com:
//! corporate ESET / Defender allow-lists are typically per-domain;
//! roomler.ai's TLS cert is already trusted by IT (the agent's
//! signaling traffic uses it), github.com is often blocked outright.
//! PC50045 field repro 2026-05-11.
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

/// Fetch the JSON metadata for the matching MSI. Used by the wizard
/// before kicking the download so the progress bar has a denominator
/// and the wizard can pre-validate the SHA256 digest format.
pub async fn resolve(flavour: &str, version: &str) -> Result<InstallerHealth> {
    let url = format!("{}/{}/health?version={}", proxy_base(), flavour, version);
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-installer/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "installer-health GET {url} returned {}",
            resp.status()
        ));
    }
    let health: InstallerHealth = resp.json().await.context("parsing installer-health JSON")?;
    Ok(health)
}

/// Stream the MSI bytes to `%TEMP%\roomler-installer\<tag>\<filename>`.
/// Fires `on_progress(received_bytes)` after each chunk; the SPA
/// throttles UI updates client-side. Returns the staged path on
/// success.
///
/// Does NOT call `verify_sha256` itself — the caller chains them so
/// the wizard's `cmd_install` orchestrator can emit a
/// `DownloadVerified` ProgressEvent only after both succeed.
pub async fn download<F: FnMut(u64)>(
    health: &InstallerHealth,
    mut on_progress: F,
) -> Result<PathBuf> {
    let dest_dir = std::env::temp_dir()
        .join("roomler-installer")
        .join(&health.tag);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating staging dir {}", dest_dir.display()))?;
    let dest = dest_dir.join(&health.filename);

    let url = format!("{}{}", proxy_base_without_path_segment(), health.uri);
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-installer/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15 * 60)) // 15 min for slow networks
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("installer GET {url} returned {}", resp.status()));
    }

    use futures::StreamExt;
    let mut file = tokio::fs::File::create(&dest)
        .await
        .with_context(|| format!("creating staging file {}", dest.display()))?;
    let mut received: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("reading installer chunk")?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("writing staging file {}", dest.display()))?;
        received += bytes.len() as u64;
        on_progress(received);
    }
    use tokio::io::AsyncWriteExt;
    file.flush().await.context("flushing staging file")?;
    drop(file);

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

/// Verify a staged MSI's SHA256 against the digest from
/// [`InstallerHealth`]. Returns `Ok(true)` on match, `Ok(false)` on
/// mismatch, `Err` on malformed digest or read error.
///
/// Accepts `digest` either as `"sha256:<hex>"` (the canonical GitHub
/// shape forwarded by the rc.27 backend) or as bare hex. Hex is
/// case-insensitive.
pub fn verify_sha256(staged: &Path, expected_digest: &str) -> Result<bool> {
    let hex = expected_digest
        .strip_prefix("sha256:")
        .unwrap_or(expected_digest);
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "expected sha256 digest as 64 hex chars (optional sha256: prefix); got {expected_digest:?}"
        ));
    }
    let bytes = std::fs::read(staged).with_context(|| format!("reading {}", staged.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = hasher.finalize();
    let actual_hex = hex_encode(&actual);
    Ok(actual_hex.eq_ignore_ascii_case(hex))
}

/// Lowercase hex encoder. `hex::encode` would suffice but we already
/// have `sha2` and avoiding the extra dep for this 6-line helper
/// keeps the build footprint trim.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
}
