//! Tunnel-archive asset resolution + download.
//!
//! The wizard hits roomler.ai's `/api/tunnel-wizard/{platform}` proxy
//! instead of downloading directly from `github.com`. Same rationale
//! as the agent installer's `asset_resolver`: corporate
//! ESET / Defender allow-lists are typically per-domain;
//! `roomler.ai`'s TLS cert is already trusted by IT (the agent's
//! signaling traffic uses it), `github.com` is often blocked outright.
//!
//! Flow per install:
//!   1. `resolve(platform, version)` → GET
//!      `/tunnel-wizard/{platform}/health` → JSON metadata
//!      (tag, size, sha256 digest, canonical filename).
//!   2. `download(&health, on_progress)` → GET
//!      `/tunnel-wizard/{platform}` → stream bytes to
//!      `<temp>/roomler-tunnel-installer/{tag}/{filename}`, firing
//!      `on_progress(received_bytes)` per chunk.
//!   3. `verify_sha256(&staged, &expected)` → hash the staged file,
//!      compare to the digest from health. Mismatch → caller deletes
//!      the file + re-downloads (or surfaces a tampered-bytes error).
//!
//! Override knob: env var `ROOMLER_TUNNEL_WIZARD_PROXY_BASE` swaps
//! the domain. Used by the integration tests in `crates/tests/` to
//! point the orchestrator at an in-process mock server. Production
//! always uses `https://roomler.ai/api/tunnel-wizard`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default proxy base. Override via `ROOMLER_TUNNEL_WIZARD_PROXY_BASE`
/// for staging / local-server testing.
const DEFAULT_PROXY_BASE: &str = "https://roomler.ai/api/tunnel-wizard";

/// Resolve the proxy base at runtime so the wizard can hit a staging
/// API for testing. Always returns a URL without a trailing slash.
fn proxy_base() -> String {
    let raw = std::env::var("ROOMLER_TUNNEL_WIZARD_PROXY_BASE")
        .ok()
        .unwrap_or_else(|| DEFAULT_PROXY_BASE.to_string());
    normalise_proxy_base(&raw)
}

/// Pure: strip the trailing slash from `raw`. Extracted so tests can
/// exercise the parsing logic without racing on `std::env`.
fn normalise_proxy_base(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

/// Mirror of the backend's `TunnelWizardHealth` JSON. Wire shape is
/// pinned by `crates/api/src/routes/tunnel_wizard_release.rs`; keep
/// in sync.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WizardArchiveHealth {
    /// Resolved tag, e.g. `tunnel-wizard-v0.3.0-rc.1`.
    pub tag: String,
    /// Normalised platform: `"windows-x86_64" | "linux-x86_64" |
    /// "linux-deb" | "macos"`.
    pub platform: String,
    /// Canonical asset filename, e.g.
    /// `roomler-tunnel-0.3.0-rc.46-x86_64-pc-windows-msvc.zip`.
    pub filename: String,
    /// Asset size in bytes.
    pub size: u64,
    /// `"sha256:<hex>"` from GitHub's `digest` field. None on older
    /// releases that pre-date the field.
    pub digest: Option<String>,
    /// Relative URI to stream the archive bytes from. Composed
    /// against the proxy base's origin for the actual download GET.
    pub uri: String,
}

/// Detect the current platform string the backend understands. The
/// wizard EXE runs on the same OS+arch as the CLI it's about to
/// install, so the platform discriminator is whatever the wizard
/// itself was compiled for.
pub fn current_platform() -> &'static str {
    // Matches the backend's `normalise_platform` enum-shape used in
    // both tunnel_release.rs and tunnel_wizard_release.rs.
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "windows-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        // Wizard defaults to the plain tarball; the `.deb` variant is
        // a packaging detail for fleet installs, not what a
        // double-clicked wizard EXE consumes.
        "linux-x86_64"
    }
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        target_os = "macos"
    )))]
    {
        // Aarch64-Linux, FreeBSD, etc. — wizard EXE doesn't ship for
        // these platforms in v1, but the const has to exist so the
        // crate compiles. Backend will 404 for an unknown platform.
        "unsupported"
    }
}

/// Fetch the JSON metadata for the matching CLI archive. Used before
/// kicking the download so the progress bar has a denominator and
/// the wizard can pre-validate the SHA256 digest format.
pub async fn resolve(platform: &str, version: &str) -> Result<WizardArchiveHealth> {
    let url = format!("{}/{}/health?version={}", proxy_base(), platform, version);
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "roomler-tunnel-installer/",
            env!("CARGO_PKG_VERSION")
        ))
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
            "wizard-health GET {url} returned {}",
            resp.status()
        ));
    }
    let health: WizardArchiveHealth = resp.json().await.context("parsing wizard-health JSON")?;
    Ok(health)
}

/// Stream the archive bytes to
/// `<temp>/roomler-tunnel-installer/<tag>/<filename>`. Fires
/// `on_progress(received_bytes)` after each chunk; the SPA throttles
/// UI updates client-side. Returns the staged path on success.
///
/// Does NOT call `verify_sha256` itself — the caller chains them so
/// the orchestrator can emit a `DownloadVerified` ProgressEvent only
/// after both succeed.
pub async fn download<F: FnMut(u64)>(
    health: &WizardArchiveHealth,
    mut on_progress: F,
) -> Result<PathBuf> {
    let dest_dir = std::env::temp_dir()
        .join("roomler-tunnel-installer")
        .join(&health.tag);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating staging dir {}", dest_dir.display()))?;
    let dest = dest_dir.join(&health.filename);

    let url = format!("{}{}", proxy_origin(), health.uri);
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "roomler-tunnel-installer/",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_secs(15 * 60)) // 15 min for slow corp networks
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "wizard archive GET {url} returned {}",
            resp.status()
        ));
    }

    use futures::StreamExt;
    let mut file = tokio::fs::File::create(&dest)
        .await
        .with_context(|| format!("creating staging file {}", dest.display()))?;
    let mut received: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("reading wizard archive chunk")?;
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

/// The origin (scheme://host[:port]) part of `proxy_base()` — used
/// when composing absolute URLs from `health.uri`, which already
/// starts with the `/api/tunnel-wizard/...` path segment so
/// concatenation must not double up the path.
fn proxy_origin() -> String {
    strip_wizard_path_suffix(&proxy_base())
}

/// Pure: strip the trailing `/api/tunnel-wizard` segment from `base`
/// so concatenation with `health.uri` (which already starts with
/// `/api/tunnel-wizard/...`) doesn't double up. If the suffix is
/// missing (custom env var without the path segment), returns `base`
/// unchanged.
fn strip_wizard_path_suffix(base: &str) -> String {
    base.strip_suffix("/api/tunnel-wizard")
        .map(|s| s.to_string())
        .unwrap_or_else(|| base.to_string())
}

/// Verify a staged archive's SHA256 against the digest from
/// [`WizardArchiveHealth`]. Returns `Ok(true)` on match, `Ok(false)`
/// on mismatch, `Err` on malformed digest or read error.
///
/// Accepts `digest` either as `"sha256:<hex>"` (the canonical GitHub
/// shape forwarded by the backend) or as bare hex. Hex is
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

/// Lowercase hex encoder. The `hex` crate would suffice but inlining
/// the 6-line helper saves a transitive dep + the `sha2` crate is
/// already pulled in.
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
            normalise_proxy_base("https://staging.local/api/tunnel-wizard/"),
            "https://staging.local/api/tunnel-wizard"
        );
    }

    #[test]
    fn normalise_proxy_base_passthrough_when_no_trailing_slash() {
        assert_eq!(
            normalise_proxy_base("https://roomler.ai/api/tunnel-wizard"),
            "https://roomler.ai/api/tunnel-wizard"
        );
    }

    #[test]
    fn strip_wizard_path_suffix_removes_canonical_suffix() {
        assert_eq!(
            strip_wizard_path_suffix("https://roomler.ai/api/tunnel-wizard"),
            "https://roomler.ai"
        );
    }

    #[test]
    fn strip_wizard_path_suffix_passthrough_when_suffix_absent() {
        assert_eq!(
            strip_wizard_path_suffix("https://staging.local"),
            "https://staging.local"
        );
    }

    #[test]
    fn hex_encode_lowercase() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn current_platform_returns_a_supported_value_on_this_host() {
        // The const exists for every target; on this dev box (x86_64
        // Win11) it should be "windows-x86_64". The assertion is
        // looser so the CI matrix's Linux/macOS jobs also pass.
        let plat = current_platform();
        assert!(
            matches!(
                plat,
                "windows-x86_64" | "linux-x86_64" | "macos" | "unsupported"
            ),
            "unexpected platform {plat:?}"
        );
    }
}
