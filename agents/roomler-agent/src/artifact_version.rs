// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Bind the version a release manifest CLAIMS to the version the downloaded
//! artifact actually IS.
//!
//! ## Why this exists
//!
//! [`crate::code_signature`] answers *whose* bytes these are. It does not
//! answer *which release* they are — and the updater decides those two
//! questions from different sources:
//!
//! - [`crate::updater::is_newer`] compares the **manifest's** tag against
//!   `CARGO_PKG_VERSION`.
//! - `verify_publisher` verifies the **artifact**.
//!
//! Nothing bound the two. An attacker able to serve the manifest (a
//! compromised release proxy or PoP, a stolen release token, a
//! TLS-terminating middlebox) advertises `agent-v0.3.0-rc.999` while pointing
//! `browser_download_url` at a genuinely-signed **older** build with a known
//! vulnerability. The signature passes — it really is ours. `is_newer` passes
//! — 999 outranks anything in the field. The fleet downgrades into a version
//! whose exploit is public, and the whole way through, nothing untrue was said
//! about the bytes. Publisher verification structurally cannot see this.
//!
//! The fix is to make the artifact state its own version and refuse when that
//! disagrees with what we were told it is. It works because the version lives
//! INSIDE the signed envelope: editing it invalidates the Authenticode
//! signature the previous check already enforces.
//!
//! ## ⚠️ The two checks are only meaningful together
//!
//! A version binding on its own proves nothing — whoever can replace the
//! artifact can equally set the version inside it. It is the *signature* that
//! makes the embedded version unforgeable, and the *embedded version* that
//! makes the signature mean "the release you asked for" rather than merely
//! "one of ours".
//!
//! That is why [`verify_artifact_version`] reports
//! [`Unsupported`](VersionError::Unsupported) rather than a refusal for the
//! Linux `.deb` and macOS `.pkg`: **this agent has authenticated nothing about
//! them**, so a binding there would check a claim against a claim while reading
//! like a control.
//!
//! Note the precise gap — the release pipeline is further along than the agent
//! is. Every published artifact already carries a detached OpenPGP `.asc`, and
//! `roomler-release-pubkey.asc` ships in the release (verified 2026-08-24
//! against `agent-v0.3.0-rc.458`: a good signature verifies and a single
//! flipped byte yields `BAD signature`). What is missing is the *client* half —
//! verifying it needs the release public key **pinned in this binary**, because
//! a key fetched from the release is the same-channel trust failure as the
//! SHA256. `.deb`/`.pkg` gain a real version binding once that lands (and
//! `pkgutil --check-signature` for the notarised `.pkg`), and not before.

use std::path::Path;

/// Why a downloaded artifact was not accepted as the release it was offered
/// as. `Mismatch` is a refusal; the rest are described at each variant.
#[derive(Debug)]
pub enum VersionError {
    /// The artifact says it is a different release than the manifest claimed.
    /// This is the downgrade signature, and it is always a refusal.
    Mismatch {
        claimed: String,
        expected: String,
        found: String,
    },
    /// The artifact's own version could not be read — a corrupt package, or a
    /// file that is not the format its name says it is. A refusal: we are
    /// about to run this as SYSTEM and cannot say what it is.
    Unreadable(String),
    /// The manifest's tag maps to no version the release workflow could have
    /// produced. A refusal — a release like that does not exist, so a
    /// manifest offering one is already lying.
    UnmappableClaim(String),
    /// No version binding is implemented for this artifact format. NOT a
    /// refusal; see the module docs for why this is the honest outcome for
    /// artifacts that carry no signature to anchor the version to.
    Unsupported(String),
}

impl std::fmt::Display for VersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mismatch {
                claimed,
                expected,
                found,
            } => write!(
                f,
                "offered as {claimed} (which must carry version {expected}) but the artifact says it is {found} — \
                 the manifest and the package disagree about which release this is"
            ),
            Self::Unreadable(why) => write!(f, "could not read the artifact's own version: {why}"),
            Self::UnmappableClaim(tag) => {
                write!(f, "no release version can be derived from the tag {tag:?}")
            }
            Self::Unsupported(fmt_name) => {
                write!(f, "no version binding is implemented for {fmt_name}")
            }
        }
    }
}

impl std::error::Error for VersionError {}

/// Check that the artifact at `path` self-identifies as the release `claimed`
/// (a tag such as `agent-v0.3.0-rc.458`, or the bare semver).
///
/// Returns the artifact's own version string on success.
pub fn verify_artifact_version(path: &Path, claimed: &str) -> Result<String, VersionError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "msi" => verify_msi(path, claimed),
        "" => Err(VersionError::Unsupported(
            "a file with no extension".to_string(),
        )),
        other => Err(VersionError::Unsupported(format!(".{other} artifacts"))),
    }
}

#[cfg(windows)]
fn verify_msi(path: &Path, claimed: &str) -> Result<String, VersionError> {
    let expected = msi_product_version_for(claimed)
        .ok_or_else(|| VersionError::UnmappableClaim(claimed.to_string()))?;
    let found = win::product_version(path).map_err(VersionError::Unreadable)?;

    // Compare the numeric fields, not the strings: `0.3.458` and `0.3.458.0`
    // are the same ProductVersion to Windows Installer, and a leading zero is
    // not a different version either.
    let want = msi_fields(&expected).ok_or_else(|| {
        VersionError::UnmappableClaim(format!(
            "{claimed} mapped to {expected:?}, which is not a valid ProductVersion"
        ))
    })?;
    let got = msi_fields(&found).ok_or_else(|| {
        VersionError::Unreadable(format!(
            "ProductVersion {found:?} is not three numeric fields"
        ))
    })?;

    if want == got {
        Ok(found)
    } else {
        Err(VersionError::Mismatch {
            claimed: claimed.to_string(),
            expected,
            found,
        })
    }
}

#[cfg(not(windows))]
fn verify_msi(_path: &Path, _claimed: &str) -> Result<String, VersionError> {
    // `pick_asset_for_platform` is compiled per target, so a non-Windows agent
    // never selects an MSI — this arm exists so the dispatch above stays one
    // list instead of two cfg-split ones.
    Err(VersionError::Unsupported(
        ".msi artifacts off Windows".to_string(),
    ))
}

/// The MSI `ProductVersion` a given release must carry.
///
/// Windows Installer's version is three numeric fields (`MAJOR.MINOR.BUILD`,
/// build ≤ 65535) and it IGNORES a 4th, so `0.3.0-rc.458` cannot be stored
/// literally. `release-agent.yml` therefore maps the rc number into the build
/// field — `MAJOR.MINOR.RC`, a final release taking 65535 so it outranks every
/// rc of that minor — and passes the result to `cargo wix --install-version`,
/// which lands it in `Product/@Version`.
///
/// ⚠️ This function and the workflow's `Resolve version` step ("Derive the MSI
/// ProductVersion") are two copies of one rule and MUST move together. If they
/// diverge, every agent refuses every update and the symptom is a silent
/// fleet-wide update freeze, not an error — the same coupling
/// [`crate::code_signature::EXPECTED_PUBLISHER`] has with the signing action,
/// and the same consequence.
///
/// `None` for any shape the workflow itself refuses to build: a non-`rc.N`
/// pre-release, an rc with a non-zero patch, an rc above the build field's
/// ceiling.
///
/// ## ⚠️ Releases before `agent-v0.3.0-rc.104` do not follow this mapping
///
/// The rc-into-the-build-field fix landed in `6bc9d58d` (2026-06-01), first
/// released as **rc.104**. MSIs older than that carry cargo-wix's raw
/// derivation — `0.3.0-rc.N` became `0.3.0.N`, i.e. the same three significant
/// fields `0.3.0` for every rc, which is precisely the collision the fix
/// removed. So this function's answer is **wrong for a pre-rc.104 tag**, and a
/// download of one is refused.
///
/// That is accepted rather than special-cased. The only path that fetches an
/// OLDER release is `updater::pin_version` — crash-loop rollback to
/// `last_known_good_version`, or an operator-chosen `rc:agent.update` tag — and
/// a refusal there folds into `CheckOutcome::Skipped`, which `main.rs` already
/// handles by raising the attention sentinel and leaving the operator to act.
/// So the failure mode is "rollback needs a human", not a wedge.
///
/// Teaching this function the legacy mapping would be worse than that: every
/// pre-rc.104 `0.3.0-rc.*` MSI shares the fields `0.3.0`, so accepting it would
/// make any one of them substitutable for any other — re-creating inside the
/// check the exact ambiguity the release fix removed.
pub fn msi_product_version_for(release: &str) -> Option<String> {
    // Accept a tag or a bare semver — every caller has a tag, and a helper
    // that silently mis-parses `agent-v…` would be a trap.
    let s = release.trim_start_matches("agent-");
    let s = s.trim_start_matches('v');

    let (core, pre) = match s.find('-') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };

    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let major: u64 = parts[0].parse().ok()?;
    let minor: u64 = parts[1].parse().ok()?;
    let patch: u64 = parts[2].parse().ok()?;

    match pre {
        // Finals. Two eras, split at 0.4 (the rc train ended with
        // 0.3.0-rc.*):
        //
        //  * 0.4+ — the project ships plain `MAJOR.MINOR.PATCH` releases, so
        //    the PATCH is the build field: 0.4.5 → `0.4.5`. The old
        //    every-final-is-65535 rule would collapse ALL 0.4.x MSIs into
        //    one ProductVersion — Windows MajorUpgrade would never see a
        //    newer version within the minor, and any 0.4.x MSI would verify
        //    as any other (the substitution ambiguity this check exists to
        //    prevent).
        //  * pre-0.4 — legacy rule kept: build = 65535 so a hypothetical
        //    0.3 final outranks every 0.3 rc. No such final was ever
        //    published; the arm exists so historical answers don't change.
        //
        // ⚠️ Rollout note: agents ≤ 0.3.0-rc.485 carry the legacy rule and
        // EXPECT `x.y.65535` for any 0.4.x tag — they refuse a 0.4.x MSI
        // (CheckOutcome::Skipped, attention sentinel) until they are first
        // moved to a transition rc that has this arm. Push the transition
        // rc pinned to any laggard before pointing it at 0.4.
        None if (major, minor) >= (0, 4) => {
            if patch > 65535 {
                return None;
            }
            Some(format!("{major}.{minor}.{patch}"))
        }
        None => Some(format!("{major}.{minor}.65535")),
        Some(pre) => {
            // Only `rc.N`. The workflow's regex is equally strict and fails
            // the build for anything else, so a `-beta.1` release cannot
            // exist to be installed.
            let rc: u64 = pre.strip_prefix("rc.")?.parse().ok()?;
            // Both of these are hard errors in the workflow: a patched rc
            // would collide with a plain rc under this mapping, and the build
            // field cannot hold more than 65535.
            if patch != 0 || rc > 65535 {
                return None;
            }
            Some(format!("{major}.{minor}.{rc}"))
        }
    }
}

/// The three significant fields of an MSI `ProductVersion`. A 4th field is
/// legal and ignored — Windows Installer ignores it for version comparison,
/// and so must we or a legitimately published `0.3.458.0` would be refused.
///
/// Windows-only, like the thing it interprets: off Windows no artifact carries
/// a ProductVersion for this to read. (`msi_product_version_for` above stays
/// cross-platform — it is a pure string mapping, and compiling its tests
/// everywhere is worth more than symmetry.)
#[cfg(windows)]
fn msi_fields(v: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = v.trim().split('.').collect();
    if parts.len() < 3 || parts.len() > 4 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
        MSIDBOPEN_READONLY, MSIHANDLE, MsiCloseHandle, MsiDatabaseOpenViewW, MsiOpenDatabaseW,
        MsiRecordGetStringW, MsiViewExecute, MsiViewFetch,
    };

    /// Closes an MSI handle on drop. Every path out of [`product_version`] is
    /// an early return, and a leaked `MSIHANDLE` holds the database file open
    /// — which on the refusal path would leave the staged installer
    /// undeletable, i.e. the rejected payload stranded on disk.
    struct Handle(MSIHANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: a handle this type owns came from a successful Msi*
                // call and is closed exactly once, here.
                unsafe { MsiCloseHandle(self.0) };
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(once(0)).collect()
    }

    fn wide_path(p: &Path) -> Vec<u16> {
        OsStr::new(p).encode_wide().chain(once(0)).collect()
    }

    /// Read `ProductVersion` out of an MSI's `Property` table.
    ///
    /// This is a database read, not a package open: no install sequence runs,
    /// no custom action executes, the Windows Installer service is not
    /// involved. That distinction matters here — the file being opened is one
    /// we have not yet decided to trust.
    pub(super) fn product_version(path: &Path) -> Result<String, String> {
        let file = wide_path(path);

        let mut raw: MSIHANDLE = 0;
        // SAFETY: `file` is a NUL-terminated wide string that outlives the
        // call; `MSIDBOPEN_READONLY` is the documented sentinel (not a real
        // pointer) for the persist argument; `raw` is a valid out-param.
        let rc = unsafe { MsiOpenDatabaseW(file.as_ptr(), MSIDBOPEN_READONLY, &mut raw) };
        if rc != ERROR_SUCCESS {
            return Err(format!(
                "MsiOpenDatabaseW failed with {rc} (not a readable MSI?)"
            ));
        }
        let db = Handle(raw);

        let query = wide("SELECT `Value` FROM `Property` WHERE `Property` = 'ProductVersion'");
        let mut raw: MSIHANDLE = 0;
        // SAFETY: `db` is open for the duration; `query` is NUL-terminated and
        // outlives the call; `raw` is a valid out-param.
        let rc = unsafe { MsiDatabaseOpenViewW(db.0, query.as_ptr(), &mut raw) };
        if rc != ERROR_SUCCESS {
            return Err(format!("MsiDatabaseOpenViewW failed with {rc}"));
        }
        let view = Handle(raw);

        // No parameters in the query, so the parameter record is the null
        // handle.
        // SAFETY: `view` is an open view for the duration of the call.
        let rc = unsafe { MsiViewExecute(view.0, 0) };
        if rc != ERROR_SUCCESS {
            return Err(format!("MsiViewExecute failed with {rc}"));
        }

        let mut raw: MSIHANDLE = 0;
        // SAFETY: `view` has been executed; `raw` is a valid out-param.
        let rc = unsafe { MsiViewFetch(view.0, &mut raw) };
        if rc != ERROR_SUCCESS {
            // ERROR_NO_MORE_ITEMS lands here too: an MSI with no
            // ProductVersion row. Refusing is right — we cannot say what
            // release it is.
            return Err(format!(
                "no ProductVersion row in the Property table (MsiViewFetch returned {rc})"
            ));
        }
        let record = Handle(raw);

        // Two-call idiom, MSI flavour: `pcch` is the buffer size in chars
        // EXCLUDING the terminator, and on ERROR_MORE_DATA it comes back as
        // the length needed (also excluding it). A ProductVersion is a dozen
        // characters, so the first buffer is nearly always enough.
        let mut buf = vec![0u16; 64];
        let mut cch = (buf.len() - 1) as u32;
        // SAFETY: `record` is a fetched record; `buf` has `cch + 1` elements
        // so the callee may write its terminator.
        let mut rc = unsafe { MsiRecordGetStringW(record.0, 1, buf.as_mut_ptr(), &mut cch) };
        if rc == ERROR_MORE_DATA {
            buf = vec![0u16; cch as usize + 1];
            cch = (buf.len() - 1) as u32;
            // SAFETY: as above, with a buffer of the size the first call asked
            // for.
            rc = unsafe { MsiRecordGetStringW(record.0, 1, buf.as_mut_ptr(), &mut cch) };
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("MsiRecordGetStringW failed with {rc}"));
        }

        let len = cch as usize;
        if len > buf.len() {
            return Err(format!(
                "MsiRecordGetStringW reported {len} chars into a {} char buffer",
                buf.len()
            ));
        }
        Ok(String::from_utf16_lossy(&buf[..len]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rc_releases_map_the_rc_into_the_build_field() {
        // The live example: what agent-v0.3.0-rc.458 must carry.
        assert_eq!(
            msi_product_version_for("agent-v0.3.0-rc.458").as_deref(),
            Some("0.3.458")
        );
        assert_eq!(
            msi_product_version_for("0.3.0-rc.1").as_deref(),
            Some("0.3.1")
        );
        assert_eq!(
            msi_product_version_for("v1.2.0-rc.65535").as_deref(),
            Some("1.2.65535")
        );
    }

    #[test]
    fn final_releases_take_the_top_of_the_build_field() {
        // Legacy (pre-0.4) rule only: 65535 so a final outranks every rc of
        // that minor. Historical answers must not change.
        assert_eq!(
            msi_product_version_for("agent-v0.3.0").as_deref(),
            Some("0.3.65535")
        );
        assert_eq!(
            msi_product_version_for("0.2.7").as_deref(),
            Some("0.2.65535")
        );
    }

    #[test]
    fn point_four_finals_map_patch_into_the_build_field() {
        // The 0.4+ scheme: PATCH is the build field, so ProductVersions are
        // monotonic WITHIN the minor (0.4.0 < 0.4.1 < …) — MajorUpgrade
        // keeps upgrading and no two 0.4.x MSIs share a version.
        assert_eq!(
            msi_product_version_for("agent-v0.4.0").as_deref(),
            Some("0.4.0")
        );
        assert_eq!(
            msi_product_version_for("agent-v0.4.17").as_deref(),
            Some("0.4.17")
        );
        assert_eq!(msi_product_version_for("v1.2.3").as_deref(), Some("1.2.3"));
        // Build-field ceiling still applies.
        assert_eq!(msi_product_version_for("0.4.65536"), None);
    }

    #[test]
    fn shapes_the_release_workflow_refuses_to_build_map_to_nothing() {
        // Each of these is a hard `::error::` in release-agent.yml, so no such
        // release can exist — a manifest offering one is lying.
        assert_eq!(msi_product_version_for("agent-v0.3.1-rc.5"), None); // patched rc
        assert_eq!(msi_product_version_for("agent-v0.3.0-rc.65536"), None); // over the ceiling
        assert_eq!(msi_product_version_for("agent-v0.3.0-beta.1"), None); // not an rc
        assert_eq!(msi_product_version_for("agent-v0.3"), None); // not three fields
        assert_eq!(msi_product_version_for("agent-v0.3.0.1"), None); // four fields
        assert_eq!(msi_product_version_for(""), None);
        assert_eq!(msi_product_version_for("agent-vNaN.3.0"), None);
    }

    #[test]
    fn pre_rc104_releases_are_mapped_by_todays_rule_and_so_will_be_refused() {
        // Deliberate, documented on `msi_product_version_for`: rc.104
        // (2026-06-01) is where MSIs began carrying MAJOR.MINOR.RC. An older
        // MSI carries `0.3.0.N`, i.e. fields (0, 3, 0), which this expectation
        // cannot match — so a rollback pinning a pre-rc.104 tag is refused and
        // falls through to the operator sentinel.
        //
        // This is locked so nobody "fixes" it by teaching the function the
        // legacy shape: every pre-rc.104 `0.3.0-rc.*` MSI shares the fields
        // `0.3.0`, so accepting that would make any one of them substitutable
        // for any other.
        assert_eq!(
            msi_product_version_for("agent-v0.3.0-rc.90").as_deref(),
            Some("0.3.90")
        );
        assert_ne!(
            msi_product_version_for("agent-v0.3.0-rc.90").as_deref(),
            Some("0.3.0")
        );
    }

    #[test]
    fn a_forged_high_version_still_has_to_produce_a_matching_artifact() {
        // The downgrade attack in the module docs: the manifest claims rc.999,
        // so the artifact must carry 0.3.999. A genuinely-signed rc.458 build
        // carries 0.3.458 and cannot satisfy it.
        assert_eq!(
            msi_product_version_for("agent-v0.3.0-rc.999").as_deref(),
            Some("0.3.999")
        );
        assert_ne!(
            msi_product_version_for("agent-v0.3.0-rc.999"),
            msi_product_version_for("agent-v0.3.0-rc.458")
        );
    }

    #[cfg(windows)]
    #[test]
    fn product_version_fields_ignore_a_fourth_field_and_reject_junk() {
        // Windows Installer ignores the 4th field; refusing `0.3.458.0` would
        // reject a legitimately published MSI.
        assert_eq!(msi_fields("0.3.458"), msi_fields("0.3.458.0"));
        assert_eq!(msi_fields("0.3.458"), Some((0, 3, 458)));
        assert_eq!(msi_fields("  0.3.458  "), Some((0, 3, 458)));
        assert_eq!(msi_fields("0.3"), None);
        assert_eq!(msi_fields("0.3.458.0.1"), None);
        assert_eq!(msi_fields("0.3.x"), None);
    }

    #[test]
    fn unsigned_formats_report_unsupported_rather_than_refusing() {
        // .deb/.pkg carry no signature for a version to be anchored to, so the
        // binding is reported as absent, not as a failure. See module docs.
        for name in [
            "roomler-agent.deb",
            "roomler-agent.pkg",
            "roomler-agent.tgz",
        ] {
            let err = verify_artifact_version(&PathBuf::from(name), "agent-v0.3.0-rc.458")
                .expect_err("no binding is implemented for this format");
            assert!(
                matches!(err, VersionError::Unsupported(_)),
                "{name} should report Unsupported, got {err:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_file_that_is_not_an_msi_is_refused_not_waved_through() {
        // Naming a payload `.msi` must not be enough to skip the binding: the
        // database open fails and that is a refusal, not an Unsupported.
        let path = std::env::temp_dir().join("roomler-artifact-version-test.msi");
        std::fs::write(&path, b"this is not an MSI").expect("writing the decoy");
        let err = verify_artifact_version(&path, "agent-v0.3.0-rc.458")
            .expect_err("a non-MSI named .msi must not verify");
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(err, VersionError::Unreadable(_)),
            "expected Unreadable, got {err:?}"
        );
    }

    /// The mapping above is derived from reading `release-agent.yml`, and a
    /// wrong reading of it would not fail loudly — it would make every agent
    /// refuse every update, which looks like nothing happening. So it is
    /// checked against a REAL published MSI rather than against itself.
    ///
    /// `#[ignore]` because it needs an artifact this repo does not carry. Run
    /// it after any change to the workflow's version derivation:
    ///
    /// ```text
    /// gh release download agent-v0.3.0-rc.458 --repo gjovanov/roomler-ai \
    ///   --pattern '*perMachine*.msi' --dir /tmp
    /// ROOMLER_TEST_MSI=/tmp/roomler-agent-0.3.0-rc.458-perMachine-x86_64-pc-windows-msvc.msi \
    /// ROOMLER_TEST_MSI_TAG=agent-v0.3.0-rc.458 \
    ///   cargo test -p roomler-agent --lib -- --ignored real_published_msi
    /// ```
    #[cfg(windows)]
    #[test]
    #[ignore = "needs a published MSI; see the doc comment for the invocation"]
    fn real_published_msi_binds_to_its_own_tag_and_refuses_a_forged_one() {
        let Ok(msi) = std::env::var("ROOMLER_TEST_MSI") else {
            panic!("set ROOMLER_TEST_MSI to a downloaded release MSI");
        };
        let tag = std::env::var("ROOMLER_TEST_MSI_TAG")
            .expect("set ROOMLER_TEST_MSI_TAG to the tag that MSI was published under");
        let path = PathBuf::from(&msi);

        let found = verify_artifact_version(&path, &tag)
            .unwrap_or_else(|e| panic!("{tag} should verify against its own MSI, got: {e}"));
        assert_eq!(
            Some(found.as_str()),
            msi_product_version_for(&tag).as_deref(),
            "the MSI's ProductVersion and the mapping disagree — the workflow's \
             derivation has changed and this module has not"
        );

        // The attack itself: same genuinely-signed bytes, a tag claiming a
        // release they are not.
        let forged = "agent-v0.3.0-rc.65000";
        assert_ne!(
            tag, forged,
            "pick a forged tag that differs from the real one"
        );
        let err = verify_artifact_version(&path, forged)
            .expect_err("a forged high version must not accept an older artifact");
        assert!(
            matches!(err, VersionError::Mismatch { .. }),
            "expected Mismatch, got {err:?}"
        );
    }
}
