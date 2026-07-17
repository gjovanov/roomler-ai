//! CLI-archive asset resolution + download — legacy wrapper.
//!
//! P4a: the streaming mechanics moved to
//! `wizard_shared::asset_resolver`; this wrapper keeps the wizard's
//! identity — the `/api/tunnel/installer` proxy base + its env
//! overrides (incl. the legacy alias), the
//! `<temp>/roomler-tunnel-installer` staging namespace, the
//! historical `roomler-tunnel-installer/<v>` User-Agent, and the
//! `WizardArchiveHealth` shape — so behaviour stays byte-identical
//! while this legacy wizard ships. Retired with the crate in P4c.
//!
//! The wizard hits roomler.ai's `/api/tunnel/installer/{platform}`
//! proxy — the CLI tarball endpoint — instead of downloading
//! directly from `github.com`. Same rationale as the agent
//! installer's `asset_resolver`: corporate ESET / Defender allow-
//! lists are typically per-domain; `roomler.ai`'s TLS cert is
//! already trusted by IT, `github.com` is often blocked.
//!
//! NB: the wizard EXE itself is delivered via a **separate** endpoint
//! family (`/api/tunnel-wizard/<platform>`) that this module never
//! touches. The wizard downloads the CLI tarball from THIS endpoint,
//! extracts it, and adds the `roomler-tunnel(.exe)` binary inside to
//! the operator's PATH. Pointing this module at `/api/tunnel-wizard`
//! makes the wizard install itself (rc.60 bug, fixed rc.61).
//!
//! Override knob: env var `ROOMLER_TUNNEL_CLI_PROXY_BASE` swaps the
//! domain. Used by the integration tests in `crates/tests/` to point
//! the orchestrator at an in-process mock server. The legacy
//! `ROOMLER_TUNNEL_WIZARD_PROXY_BASE` is honoured as a fallback for
//! back-compat with any test fixture that already set it.
//! Production always uses `https://roomler.ai/api/tunnel/installer`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use wizard_shared::asset_resolver::{current_platform, verify_sha256};

/// Historical wire-visible User-Agent — preserved verbatim through
/// the P4a extraction (the moved core fns take the UA as a
/// parameter).
const USER_AGENT: &str = concat!("roomler-tunnel-installer/", env!("CARGO_PKG_VERSION"));

/// Legacy wizards keep their step-boundary-only cancel granularity:
/// the core `download` gained a mid-stream cancel check, and this
/// never-true flag opts the shipped wizard out of it.
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Default proxy base — the CLI tarball endpoint family.
///
/// rc.61 fix: this points at `/api/tunnel/installer` (which serves
/// the `roomler-tunnel` CLI tarball that the wizard installs), NOT
/// `/api/tunnel-wizard` (which serves the wizard EXE itself —
/// downloading that here would have the wizard install ITSELF, then
/// fail at the `find_tunnel_binary` step because the archive
/// contains `roomler-tunnel-installer.exe`, not `roomler-tunnel.exe`).
/// Field-reproduced 2026-05-25 on rc.60 with operator's PC.
///
/// Override via `ROOMLER_TUNNEL_CLI_PROXY_BASE` for staging / local-
/// server testing. The legacy `ROOMLER_TUNNEL_WIZARD_PROXY_BASE`
/// alias is honoured for backward-compat with any test fixture that
/// already set it before the rename.
const DEFAULT_PROXY_BASE: &str = "https://roomler.ai/api/tunnel/installer";

/// Resolve the proxy base at runtime so the wizard can hit a staging
/// API for testing. Always returns a URL without a trailing slash.
fn proxy_base() -> String {
    let raw = std::env::var("ROOMLER_TUNNEL_CLI_PROXY_BASE")
        .ok()
        .or_else(|| std::env::var("ROOMLER_TUNNEL_WIZARD_PROXY_BASE").ok())
        .unwrap_or_else(|| DEFAULT_PROXY_BASE.to_string());
    normalise_proxy_base(&raw)
}

/// Pure: strip the trailing slash from `raw`. Extracted so tests can
/// exercise the parsing logic without racing on `std::env`.
fn normalise_proxy_base(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

/// Mirror of the backend's health JSON for the CLI-archive endpoint.
/// Wire shape is pinned by `crates/api/src/routes/tunnel_release.rs`;
/// keep in sync.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WizardArchiveHealth {
    /// Resolved tag, e.g. `tunnel-v0.3.0-rc.46`.
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

impl From<wizard_shared::asset_resolver::ArtifactHealth> for WizardArchiveHealth {
    fn from(h: wizard_shared::asset_resolver::ArtifactHealth) -> Self {
        WizardArchiveHealth {
            tag: h.tag,
            platform: h.target,
            filename: h.filename,
            size: h.size,
            digest: h.digest,
            uri: h.uri,
        }
    }
}

/// Fetch the JSON metadata for the matching CLI archive. Used before
/// kicking the download so the progress bar has a denominator and
/// the wizard can pre-validate the SHA256 digest format.
pub async fn resolve(platform: &str, version: &str) -> Result<WizardArchiveHealth> {
    let health =
        wizard_shared::asset_resolver::resolve(&proxy_base(), platform, version, USER_AGENT)
            .await?;
    Ok(health.into())
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
    on_progress: F,
) -> Result<PathBuf> {
    let dest = std::env::temp_dir()
        .join("roomler-tunnel-installer")
        .join(&health.tag)
        .join(&health.filename);
    let url = format!("{}{}", proxy_origin(), health.uri);
    let spec = wizard_shared::asset_resolver::DownloadSpec {
        url: &url,
        dest: &dest,
        user_agent: USER_AGENT,
        artifact_label: "CLI archive",
    };
    wizard_shared::asset_resolver::download(&spec, &NEVER_CANCELLED, on_progress).await?;
    Ok(dest)
}

/// The origin (scheme://host[:port]) part of `proxy_base()` — used
/// when composing absolute URLs from `health.uri`, which already
/// starts with `/api/tunnel/installer/...` so concatenation must not
/// double up the path.
fn proxy_origin() -> String {
    strip_cli_path_suffix(&proxy_base())
}

/// Pure: strip the trailing `/api/tunnel/installer` segment from
/// `base` so concatenation with `health.uri` (which already starts
/// with `/api/tunnel/installer/...`) doesn't double up. If the
/// suffix is missing (custom env var without the path segment),
/// returns `base` unchanged. Also strips the legacy `/api/tunnel-
/// wizard` suffix for backward-compat with the rc.59/rc.60 broken
/// build's env-var convention.
fn strip_cli_path_suffix(base: &str) -> String {
    if let Some(stripped) = base.strip_suffix("/api/tunnel/installer") {
        return stripped.to_string();
    }
    if let Some(stripped) = base.strip_suffix("/api/tunnel-wizard") {
        return stripped.to_string();
    }
    base.to_string()
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
            normalise_proxy_base("https://staging.local/api/tunnel/installer/"),
            "https://staging.local/api/tunnel/installer"
        );
    }

    #[test]
    fn normalise_proxy_base_passthrough_when_no_trailing_slash() {
        assert_eq!(
            normalise_proxy_base("https://roomler.ai/api/tunnel/installer"),
            "https://roomler.ai/api/tunnel/installer"
        );
    }

    #[test]
    fn strip_cli_path_suffix_removes_canonical_suffix() {
        // rc.61: the canonical suffix is `/api/tunnel/installer` (the
        // CLI tarball endpoint). The wizard EXE is delivered via a
        // separate endpoint family (`/api/tunnel-wizard/<platform>`)
        // that the install pipeline never touches.
        assert_eq!(
            strip_cli_path_suffix("https://roomler.ai/api/tunnel/installer"),
            "https://roomler.ai"
        );
    }

    #[test]
    fn strip_cli_path_suffix_back_compat_strips_wizard_alias() {
        // Anyone who set `ROOMLER_TUNNEL_WIZARD_PROXY_BASE` against
        // the rc.59/rc.60 broken build should still get a working
        // origin (the env-var pointed at the WRONG endpoint family,
        // but the suffix-strip behaviour stays back-compat).
        assert_eq!(
            strip_cli_path_suffix("https://staging.local/api/tunnel-wizard"),
            "https://staging.local"
        );
    }

    #[test]
    fn strip_cli_path_suffix_passthrough_when_suffix_absent() {
        assert_eq!(
            strip_cli_path_suffix("https://staging.local"),
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

    #[test]
    fn proxy_base_honours_legacy_wizard_env_fallback() {
        // The legacy `ROOMLER_TUNNEL_WIZARD_PROXY_BASE` fallback at
        // `proxy_base()` had NO coverage before P4a — lock it now
        // (the P4a wrapper must preserve the 2-var chain verbatim).
        // No other test in this crate touches these two vars, so the
        // process-global env mutation can't race.
        //
        // SAFETY: single test mutating two vars exclusive to it;
        // restored before the test returns.
        unsafe {
            std::env::remove_var("ROOMLER_TUNNEL_CLI_PROXY_BASE");
            std::env::set_var(
                "ROOMLER_TUNNEL_WIZARD_PROXY_BASE",
                "https://legacy.local/api/tunnel-wizard/",
            );
        }
        let via_legacy = proxy_base();
        // Primary var wins over the legacy alias when both are set.
        unsafe {
            std::env::set_var(
                "ROOMLER_TUNNEL_CLI_PROXY_BASE",
                "https://primary.local/api/tunnel/installer",
            );
        }
        let via_primary = proxy_base();
        unsafe {
            std::env::remove_var("ROOMLER_TUNNEL_CLI_PROXY_BASE");
            std::env::remove_var("ROOMLER_TUNNEL_WIZARD_PROXY_BASE");
        }
        assert_eq!(via_legacy, "https://legacy.local/api/tunnel-wizard");
        assert_eq!(via_primary, "https://primary.local/api/tunnel/installer");
    }
}
