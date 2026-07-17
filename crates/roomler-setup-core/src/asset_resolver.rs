//! Artifact resolution + download — the shared streaming machinery
//! behind both wizards' `asset_resolver` wrappers (and the unified
//! `roomler-setup` app).
//!
//! The wizards hit roomler.ai's installer proxies (`/api/agent/
//! installer/{flavour}`, `/api/tunnel/installer/{platform}`) instead
//! of downloading directly from `github.com`: corporate ESET /
//! Defender allow-lists are typically per-domain; `roomler.ai`'s TLS
//! cert is already trusted by IT, `github.com` is often blocked
//! outright (field-reproduced 2026-05-11).
//!
//! Layering (P4a): this module owns the wire mechanics — health GET,
//! chunked streaming download, SHA256 verification. The CALLERS own
//! everything identity-shaped: proxy-base env-var resolution, temp
//! staging namespaces, User-Agent strings, and origin/suffix
//! stripping. That split is what lets the two legacy wizards keep
//! byte-identical wire behaviour through their wrappers while the
//! unified app uses the same code with its own identity.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Unified mirror of the backend's installer-health JSON. The agent
/// endpoint (`agent_release.rs`) returns the discriminator as
/// `flavour`, the tunnel endpoint (`tunnel_release.rs`) as
/// `platform` — the serde aliases fold either into [`Self::target`].
/// Deser-only unification is safe: nothing re-serializes health onto
/// a wire (the `Serialize` derive is kept for parity with the legacy
/// structs' derives).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactHealth {
    /// Resolved tag, e.g. `agent-v0.3.0-rc.28` / `tunnel-v0.3.0-rc.46`.
    pub tag: String,
    /// Normalised flavour/platform discriminator — whatever the
    /// caller asked the endpoint for (`"peruser"`, `"permachine"`,
    /// `"windows-x86_64"`, …).
    #[serde(alias = "flavour", alias = "platform")]
    pub target: String,
    /// Canonical asset filename, e.g.
    /// `roomler-agent-0.3.0-rc.28-perMachine-x86_64-pc-windows-msvc.msi`.
    pub filename: String,
    /// Asset size in bytes.
    pub size: u64,
    /// `"sha256:<hex>"` from GitHub's `digest` field. None on
    /// releases that pre-date the field.
    pub digest: Option<String>,
    /// Relative URI to stream the artifact bytes from. The caller
    /// composes it against its proxy origin for the download GET.
    pub uri: String,
}

/// Fetch the JSON metadata for the matching artifact. Used before
/// kicking the download so the progress bar has a denominator and the
/// wizard can pre-validate the SHA256 digest format.
///
/// `base` is the caller's already-normalised proxy base (no trailing
/// slash); `user_agent` is caller-supplied so each wizard keeps its
/// historical wire-visible UA.
pub async fn resolve(
    base: &str,
    target: &str,
    version: &str,
    user_agent: &str,
) -> Result<ArtifactHealth> {
    let url = format!("{base}/{target}/health?version={version}");
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
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
    let health: ArtifactHealth = resp.json().await.context("parsing installer-health JSON")?;
    Ok(health)
}

/// Identity-shaped inputs to [`download`] the caller owns: the fully
/// composed URL, the full staging destination path, the User-Agent,
/// and the artifact label used in error messages (`"installer"` for
/// the agent wizard, `"CLI archive"` for the tunnel wizard — kept
/// caller-supplied so the legacy error strings stay byte-identical).
pub struct DownloadSpec<'a> {
    /// Absolute URL (proxy origin + `health.uri`), caller-composed.
    pub url: &'a str,
    /// Full staging path (the caller owns the temp-namespace
    /// convention, e.g. `%TEMP%\roomler-installer\{tag}\{filename}`).
    pub dest: &'a Path,
    pub user_agent: &'a str,
    /// Label interpolated into error messages.
    pub artifact_label: &'a str,
}

/// Stream the artifact bytes to `spec.dest`, firing
/// `on_progress(received_bytes)` after each chunk; the SPA throttles
/// UI updates client-side.
///
/// `cancel` is checked before each chunk write — callers that don't
/// want mid-stream abort (the legacy wizards, preserving their
/// step-boundary-only cancel granularity) pass a never-true static.
///
/// Does NOT call [`verify_sha256`] itself — the caller chains them so
/// the orchestrator can emit a `DownloadVerified` ProgressEvent only
/// after both succeed.
pub async fn download<F: FnMut(u64)>(
    spec: &DownloadSpec<'_>,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<()> {
    if let Some(parent) = spec.dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating staging dir {}", parent.display()))?;
    }

    let client = reqwest::Client::builder()
        .user_agent(spec.user_agent)
        .timeout(Duration::from_secs(15 * 60)) // 15 min for slow corp networks
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(spec.url)
        .send()
        .await
        .with_context(|| format!("GET {}", spec.url))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "{} GET {} returned {}",
            spec.artifact_label,
            spec.url,
            resp.status()
        ));
    }

    use futures::StreamExt;
    let mut file = tokio::fs::File::create(spec.dest)
        .await
        .with_context(|| format!("creating staging file {}", spec.dest.display()))?;
    let mut received: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("download cancelled"));
        }
        let bytes = chunk.with_context(|| format!("reading {} chunk", spec.artifact_label))?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("writing staging file {}", spec.dest.display()))?;
        received += bytes.len() as u64;
        on_progress(received);
    }
    use tokio::io::AsyncWriteExt;
    file.flush().await.context("flushing staging file")?;
    drop(file);

    Ok(())
}

/// A companion asset located by [`find_release_asset`] — a binary
/// served straight off a GitHub Release (e.g. the `roomler-desktop`
/// GUI EXE) rather than through a health-manifest installer proxy.
#[derive(Clone, Debug)]
pub struct ReleaseAsset {
    /// Download URL (GitHub `browser_download_url` from the
    /// latest-release list).
    pub url: String,
    pub filename: String,
    /// `"sha256:<hex>"` when the release carries the digest field.
    pub digest: Option<String>,
}

// Deser-only mirrors of the `/api/*/latest-release` JSON. Local to
// core (NO roomler-agent dep — D8 dep hygiene; core never links the
// agent/tunnel crates).
#[derive(Deserialize)]
struct RelListEntry {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<RelListAsset>,
}
#[derive(Deserialize)]
struct RelListAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

/// GET a `.../latest-release` list and locate a companion asset: the
/// first non-draft release whose tag starts with `tag_prefix`, and
/// within it the first asset whose lower-cased name CONTAINS
/// `name_contains` and ends with `suffix` (skipping `.sha256`
/// sidecars). Returns `None` when nothing matches (older server /
/// missing asset) so the caller degrades gracefully. Used by the
/// daemon orchestrator to place `roomler-desktop` (GAP-A / P6) — the
/// GUI companion isn't in the MSI, it's a standalone release EXE.
pub async fn find_release_asset(
    latest_release_url: &str,
    tag_prefix: &str,
    name_contains: &str,
    suffix: &str,
    user_agent: &str,
) -> Result<Option<ReleaseAsset>> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(30))
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(latest_release_url)
        .send()
        .await
        .with_context(|| format!("GET {latest_release_url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "GET {latest_release_url} returned {}",
            resp.status()
        ));
    }
    let releases: Vec<RelListEntry> = resp.json().await.context("parse latest-release list")?;
    let nc = name_contains.to_ascii_lowercase();
    let sfx = suffix.to_ascii_lowercase();
    for rel in releases {
        if rel.draft || !rel.tag_name.starts_with(tag_prefix) {
            continue;
        }
        for a in rel.assets {
            let lower = a.name.to_ascii_lowercase();
            if lower.ends_with(".sha256") || !lower.contains(&nc) || !lower.ends_with(&sfx) {
                continue;
            }
            return Ok(Some(ReleaseAsset {
                url: a.browser_download_url,
                filename: a.name,
                digest: a.digest,
            }));
        }
    }
    Ok(None)
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

/// Verify a staged artifact's SHA256 against the digest from
/// [`ArtifactHealth`]. Returns `Ok(true)` on match, `Ok(false)` on
/// mismatch, `Err` on malformed digest or read error.
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

/// Lowercase hex encoder. The `hex` crate would suffice but the
/// 6-line helper predates this crate and the legacy wizards' test
/// suites pin its behaviour; public so the wrapper crates' tests can
/// keep exercising it through `use`.
pub fn hex_encode(bytes: &[u8]) -> String {
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
    fn hex_encode_lowercase() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn current_platform_returns_a_supported_value_on_this_host() {
        let plat = current_platform();
        assert!(
            matches!(
                plat,
                "windows-x86_64" | "linux-x86_64" | "macos" | "unsupported"
            ),
            "unexpected platform {plat:?}"
        );
    }

    #[test]
    fn artifact_health_deserialises_flavour_alias() {
        // Agent endpoint shape (agent_release.rs): `flavour`.
        let h: ArtifactHealth = serde_json::from_str(
            r#"{"tag":"agent-v0.3.0-rc.28","flavour":"permachine",
                "filename":"roomler-agent-0.3.0-rc.28-perMachine-x86_64-pc-windows-msvc.msi",
                "size":123,"digest":null,
                "uri":"/api/agent/installer/permachine?version=latest"}"#,
        )
        .unwrap();
        assert_eq!(h.target, "permachine");
    }

    #[test]
    fn artifact_health_deserialises_platform_alias() {
        // Tunnel endpoint shape (tunnel_release.rs): `platform`.
        let h: ArtifactHealth = serde_json::from_str(
            r#"{"tag":"tunnel-v0.3.0-rc.46","platform":"windows-x86_64",
                "filename":"roomler-tunnel-0.3.0-rc.46-x86_64-pc-windows-msvc.zip",
                "size":456,"digest":"sha256:00","uri":"/api/tunnel/installer/windows-x86_64?version=latest"}"#,
        )
        .unwrap();
        assert_eq!(h.target, "windows-x86_64");
        assert_eq!(h.digest.as_deref(), Some("sha256:00"));
    }
}
