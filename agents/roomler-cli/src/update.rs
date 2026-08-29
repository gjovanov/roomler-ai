//! `roomler self-update` — check the server's tunnel latest-release,
//! download the newer binary through the roomler.ai installer proxy (corp-AV-
//! friendly, same origin as the manifest), verify its SHA-256, and self-replace
//! the running executable.
//!
//! Mirrors `roomler-agent self-update`, minus the MSI / Windows-service / UAC
//! machinery — the tunnel ships as a plain binary inside a release `.zip`, so a
//! self-update is just download → verify → swap the exe. Windows only for now
//! (the fleet's platform); other OSes are pointed at the manual download.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::TunnelConfig;

/// This build's version (the workspace `version`, e.g. `0.3.0-rc.150`).
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Installer-proxy platform token (matches the api `normalise_platform`).
/// Only `windows-x86_64` has a real self-update path (see `update` below); the
/// other tokens only fill the "download it yourself from {url}" hint. A
/// catch-all keeps the CLI COMPILING on targets the server has no artifact for
/// (e.g. aarch64 Linux — a source build on Fedora Asahi hit E0425 here) instead
/// of failing to build at all.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const PLATFORM: &str = "windows-x86_64";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PLATFORM: &str = "linux-x86_64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const PLATFORM: &str = "linux-aarch64";
#[cfg(target_os = "macos")]
const PLATFORM: &str = "macos";
#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    target_os = "macos",
)))]
const PLATFORM: &str = "unknown";

#[derive(serde::Deserialize)]
struct ReleaseAsset {
    name: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

/// Entry point for the `self-update` subcommand.
pub async fn self_update(cfg: &TunnelConfig, check_only: bool) -> Result<()> {
    let base = cfg.server_url.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .user_agent(concat!("roomler-tunnel/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;

    // 1) Latest release (public endpoint — no auth).
    let releases: Vec<Release> = http
        .get(format!("{base}/api/tunnel/latest-release"))
        .send()
        .await
        .context("GET /api/tunnel/latest-release")?
        .error_for_status()
        .context("latest-release HTTP status")?
        .json()
        .await
        .context("parse latest-release")?;
    let latest = releases
        .first()
        .context("server returned no tunnel releases")?;
    let latest_ver = latest
        .tag_name
        .trim_start_matches("tunnel-v")
        .trim_start_matches('v');
    println!("roomler-tunnel: current {CURRENT}, latest {latest_ver}");

    if !is_newer(latest_ver, CURRENT) {
        println!("Already up to date.");
        return Ok(());
    }
    if check_only {
        println!(
            "Update available: {CURRENT} -> {latest_ver} (run `self-update` without --check to install)."
        );
        return Ok(());
    }

    // 2) Only the Windows self-replace is wired (the fleet's platform).
    if PLATFORM != "windows-x86_64" {
        bail!(
            "self-update installs on Windows x86_64 only for now; download {latest_ver} manually from {base}/api/tunnel/installer/{PLATFORM}?version=latest"
        );
    }

    // 3) Download the archive through the roomler.ai proxy (same origin as the
    //    manifest; corporate AV trusts it over a raw github.com pull).
    println!("Downloading {latest_ver} ...");
    let bytes = http
        .get(format!(
            "{base}/api/tunnel/installer/{PLATFORM}?version=latest"
        ))
        .send()
        .await
        .context("download archive")?
        .error_for_status()
        .context("archive HTTP status")?
        .bytes()
        .await
        .context("read archive bytes")?;

    // 4) Verify SHA-256 against the release manifest (defense in depth).
    match latest
        .assets
        .iter()
        .find(|a| a.name.ends_with(".zip"))
        .and_then(|a| a.digest.as_deref())
    {
        Some(expected) => {
            verify_sha256(&bytes, expected)?;
            println!("SHA-256 verified.");
        }
        None => println!("(no digest in manifest — skipping hash check)"),
    }

    // 5) Extract the binary + self-replace.
    let new_exe = extract_windows_exe(&bytes)?;
    replace_self(&new_exe)?;
    println!("Updated to {latest_ver}. Restart roomler-tunnel to run the new version.");
    Ok(())
}

/// True when `latest` strictly outranks `current` — a real ordered compare
/// (mirrors the agent updater's `parse_version` tuple: final ranks above
/// every rc of the same core, `0.4.0 > 0.3.0-rc.482`). The old "different
/// string ⇒ update" fallback meant a stale manifest could DOWNGRADE a final
/// build; with the 0.4.x scheme finals are the common case, so the
/// comparator has to actually order them. Unparseable versions compare as
/// not-newer, so a malformed manifest can't force anything.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// `(major, minor, patch, pre_rank)`; `pre_rank` = the rc number for
/// `-rc.N`, `u64::MAX` for a final (or an unknown pre-release label, so a
/// forward-compat tag never ranks below an rc).
fn parse_version(v: &str) -> Option<(u64, u64, u64, u64)> {
    let s = v.trim_start_matches("tunnel-").trim_start_matches('v');
    let (core, pre) = match s.find('-') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mut it = core.split('.');
    let major: u64 = it.next()?.parse().ok()?;
    let minor: u64 = it.next()?.parse().ok()?;
    let patch: u64 = it.next()?.parse().ok()?;
    let pre_rank = match pre {
        None => u64::MAX,
        Some(p) => p
            .strip_prefix("rc.")
            .and_then(|n| n.parse().ok())
            .unwrap_or(u64::MAX),
    };
    Some((major, minor, patch, pre_rank))
}

/// Verify `bytes` hash matches `expected` (`sha256:<hex>` or a bare `<hex>`).
fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let want = expected
        .trim()
        .trim_start_matches("sha256:")
        .to_ascii_lowercase();
    let mut h = Sha256::new();
    h.update(bytes);
    let got = hex::encode(h.finalize());
    if got != want {
        bail!("SHA-256 mismatch: expected {want}, got {got}");
    }
    Ok(())
}

/// Pull the CLI exe out of the release zip. P3d Slice B renamed the OUTPUT
// RETIRED-NAME-ANCHOR-BEGIN
// Reads a PUBLISHED archive. The retired entry name is an input this must
// keep accepting: a fleet host running the previous CLI looks for exactly
// that name, and dropping the arm strands it on its current version.
/// binary `roomler-tunnel.exe` -> `roomler.exe`, but the CI ships BOTH names
/// in the zip and a deployed (pre-rename) fleet self-updater may still be
/// extracting `roomler-tunnel.exe`, so accept EITHER source entry. The on-disk
/// replace target ([`replace_self`]) is driven by `current_exe()`, so whatever
/// the running binary is named stays named — only the zip SOURCE entry name
/// widens here.
fn extract_windows_exe(zip_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).context("open release zip")?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).context("read zip entry")?;
        let name = f.name();
        if name.ends_with("roomler.exe") || name.ends_with("roomler-tunnel.exe") {
            let mut buf = Vec::with_capacity(f.size() as usize);
            std::io::Read::read_to_end(&mut f, &mut buf).context("read exe from zip")?;
            return Ok(buf);
        }
    }
    bail!("neither roomler.exe nor roomler-tunnel.exe found in the release archive");
    // RETIRED-NAME-ANCHOR-END
}

/// Replace the running executable with `new_exe` bytes. Windows lets a running
/// image be RENAMED (not deleted), so move the current exe aside to `.old`, then
/// write the new bytes at the original path. The `.old` is cleared on the next
/// update. Rolls back on a write failure so the tool is never left binary-less.
fn replace_self(new_exe: &[u8]) -> Result<()> {
    let cur = std::env::current_exe().context("resolve current exe path")?;
    let old = cur.with_extension("old");
    let _ = std::fs::remove_file(&old); // clear any prior `.old`
    std::fs::rename(&cur, &old).context("move running exe aside")?;
    if let Err(e) = std::fs::write(&cur, new_exe) {
        let _ = std::fs::rename(&old, &cur); // roll back
        return Err(e).context("write new exe");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc_compare() {
        assert!(is_newer("0.3.0-rc.151", "0.3.0-rc.150"));
        assert!(!is_newer("0.3.0-rc.150", "0.3.0-rc.150"));
        assert!(!is_newer("0.3.0-rc.149", "0.3.0-rc.150"));
        // rc integer compare, not lexical (rc.9 < rc.100).
        assert!(is_newer("0.3.0-rc.100", "0.3.0-rc.9"));
        // Ordered across cores now (was "different string ⇒ update").
        assert!(is_newer("0.4.0-rc.1", "0.3.0-rc.150"));
        // The 0.4.x final scheme: finals outrank every rc of their core,
        // order among themselves, and a stale manifest can no longer
        // DOWNGRADE (the old fallback made any different string an update).
        assert!(is_newer("0.4.0", "0.3.0-rc.482"));
        assert!(is_newer("0.4.0", "0.4.0-rc.3"));
        assert!(is_newer("0.4.2", "0.4.1"));
        assert!(!is_newer("0.4.0", "0.4.1"));
        assert!(!is_newer("0.3.0-rc.482", "0.4.0"));
        // Malformed input can't force an update.
        assert!(!is_newer("banana", "0.4.0"));
    }

    #[test]
    fn sha256_roundtrip() {
        // echo -n "hello" | sha256sum
        let want = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(b"hello", want).is_ok());
        assert!(verify_sha256(b"hello", &format!("sha256:{want}")).is_ok());
        assert!(verify_sha256(b"world", want).is_err());
    }
}
