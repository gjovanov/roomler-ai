// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Authenticode verification for anything the agent is about to EXECUTE.
//!
//! ## Why this exists
//!
//! The self-updater downloads an installer and runs it as the daemon's
//! identity — **SYSTEM under the perMachine service, root under systemd**. Its
//! only integrity check was a size floor plus a SHA256 that is verified *when
//! the manifest carries one*, and that digest travels in the SAME manifest,
//! from the SAME origin, as the download URL. That makes it a corruption
//! check, not a tamper anchor: anyone able to serve the manifest (a
//! compromised release proxy or PoP, a stolen release token, a TLS-terminating
//! middlebox) rewrites the URL and the digest together and every agent
//! installs their payload. A SYSTEM-launched MSI raises no SmartScreen or UAC
//! prompt, so nothing else stands in the way.
//!
//! The fix is to verify a signature that the serving channel cannot forge,
//! against an expectation compiled into this binary. Windows releases are
//! Authenticode-signed by the publisher below (Azure Artifact Signing, and the
//! release workflow's `require` gate fails a release tag that would ship
//! unsigned), so the check is enforceable today.
//!
//! ## What is checked, and why both halves are needed
//!
//! 1. `WinVerifyTrust` — the file carries a valid, trusted, unrevoked
//!    Authenticode signature (chain + timestamp).
//! 2. The signer's common name contains [`EXPECTED_PUBLISHER`].
//!
//! Step 1 alone is **not** sufficient and it is the trap worth naming: it
//! proves only that *somebody* the machine trusts signed the file, and every
//! commercial code-signing certificate chains to a root Windows already
//! trusts. An attacker holding any such certificate would sail through. The
//! identity check is what makes the signature mean "this came from us".
//!
//! ## Platforms other than Windows
//!
//! `verify_publisher` reports [`Unsupported`](VerifyError::Unsupported) off
//! Windows. There the anchor is each release's detached GPG `.asc`, checked
//! against the signing key compiled into this binary (`pgp_verify.rs`): the
//! updater refuses a Linux or macOS artifact whose signature does not verify
//! (`updater.rs`). Both checks are one idea, an identity compiled in, never
//! one fetched from the release.

use std::fmt;
use std::path::Path;

/// The publisher every official Windows artifact is signed by — matched
/// against the certificate's simple display name.
///
/// Kept in step with `expect-subject-contains` in
/// `.github/actions/sign-windows`: the workflow refuses to publish an artifact
/// whose signer does not contain this, and this refuses to install one. If the
/// company name ever changes, BOTH move together, and old agents keep trusting
/// only the old name until they are updated by a build that still carries it.
pub const EXPECTED_PUBLISHER: &str = "G ROX LTD";

/// Why a payload was not accepted. Every variant is a refusal to execute.
#[derive(Debug)]
pub enum VerifyError {
    /// No Authenticode signature, or one that does not verify (bad chain,
    /// revoked, tampered payload).
    Untrusted(String),
    /// Verified, but signed by someone else. Carries the signer so the log
    /// says who actually signed it.
    WrongPublisher { found: String, expected: String },
    /// Signature verification is not implemented for this platform.
    Unsupported,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Untrusted(why) => write!(f, "signature not trusted: {why}"),
            Self::WrongPublisher { found, expected } => {
                write!(
                    f,
                    "signed by {found:?}, expected a publisher containing {expected:?}"
                )
            }
            Self::Unsupported => write!(
                f,
                "signature verification is not supported on this platform"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify `path` carries a trusted Authenticode signature from `expected`.
///
/// Returns the signer's display name on success.
#[cfg(windows)]
pub fn verify_publisher(path: &Path, expected: &str) -> Result<String, VerifyError> {
    win::verify_trust(path)?;
    let signer = win::signer_name(path)?;
    if signer.contains(expected) {
        Ok(signer)
    } else {
        Err(VerifyError::WrongPublisher {
            found: signer,
            expected: expected.to_string(),
        })
    }
}

#[cfg(not(windows))]
pub fn verify_publisher(_path: &Path, _expected: &str) -> Result<String, VerifyError> {
    Err(VerifyError::Unsupported)
}

/// [`verify_publisher`] for a file the caller holds open (#3246): the check
/// reads through `file`, and the signer's name comes from what that check
/// verified, not from a second look at the path. `path` is the same file's name,
/// which `WinVerifyTrust` wants as well. Returns the signer's display name.
#[cfg(windows)]
pub fn verify_open_publisher(
    file: &std::fs::File,
    path: &Path,
    expected: &str,
) -> Result<String, VerifyError> {
    let signer = win::verify_open(file, path)?;
    if signer.contains(expected) {
        Ok(signer)
    } else {
        Err(VerifyError::WrongPublisher {
            found: signer,
            expected: expected.to_string(),
        })
    }
}

#[cfg(not(windows))]
pub fn verify_open_publisher(
    _file: &std::fs::File,
    _path: &Path,
    _expected: &str,
) -> Result<String, VerifyError> {
    Err(VerifyError::Unsupported)
}

#[cfg(windows)]
mod win {
    use super::VerifyError;
    use std::ffi::OsStr;
    use std::iter::once;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        CERT_E_CHAINING, CERT_E_EXPIRED, CERT_E_REVOKED, CERT_E_UNTRUSTEDROOT,
        CRYPT_E_SECURITY_SETTINGS, ERROR_SUCCESS, HANDLE, TRUST_E_EXPLICIT_DISTRUST,
        TRUST_E_NOSIGNATURE, TRUST_E_SUBJECT_NOT_TRUSTED,
    };
    use windows_sys::Win32::Security::Cryptography::{
        CERT_CONTEXT, CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_NAME_SIMPLE_DISPLAY_TYPE,
        CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_FORMAT_FLAG_BINARY,
        CERT_QUERY_OBJECT_FILE, CMSG_SIGNER_INFO, CMSG_SIGNER_INFO_PARAM,
        CertFindCertificateInStore, CertFreeCertificateContext, CertGetNameStringW,
        CryptMsgGetParam, CryptQueryObject,
    };
    use windows_sys::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
        WTD_UI_NONE, WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain,
        WTHelperProvDataFromStateData, WinVerifyTrust,
    };

    fn wide(path: &Path) -> Vec<u16> {
        OsStr::new(path).encode_wide().chain(once(0)).collect()
    }

    /// `WinVerifyTrust` against the generic Authenticode policy.
    pub(super) fn verify_trust(path: &Path) -> Result<(), VerifyError> {
        verified(path, null_mut(), |_| Ok(()))
    }

    /// [`verify_trust`] through an open handle, and the name of the signer it
    /// verified.
    pub(super) fn verify_open(file: &std::fs::File, path: &Path) -> Result<String, VerifyError> {
        verified(path, file.as_raw_handle(), verified_signer)
    }

    /// The leaf of the primary signer's chain, read from `WinVerifyTrust`'s own
    /// state: the certificate it verified, with no second parse of the file.
    fn verified_signer(state: HANDLE) -> Result<String, VerifyError> {
        let missing = || VerifyError::Untrusted("signer certificate not found".into());
        // SAFETY: `state` is the live state of a verify that succeeded, closed
        // only after this returns; each pointer is null-checked before the next
        // call reads through it, and all of them belong to that state.
        let cert = unsafe {
            let provider = WTHelperProvDataFromStateData(state);
            if provider.is_null() {
                return Err(missing());
            }
            let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
            if signer.is_null() {
                return Err(missing());
            }
            let leaf = WTHelperGetProvCertFromChain(signer, 0);
            if leaf.is_null() {
                return Err(missing());
            }
            (*leaf).pCert
        };
        if cert.is_null() {
            return Err(missing());
        }
        cert_name(cert)
    }

    /// Run the check on `path`, through `handle` when it is not null, and `read`
    /// from its state before that is released.
    fn verified<T>(
        path: &Path,
        handle: HANDLE,
        read: impl FnOnce(HANDLE) -> Result<T, VerifyError>,
    ) -> Result<T, VerifyError> {
        let file = wide(path);

        let mut file_info = WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as _,
            pcwszFilePath: file.as_ptr(),
            hFile: handle,
            pgKnownSubject: null_mut(),
        };

        let mut data = WINTRUST_DATA {
            cbStruct: size_of::<WINTRUST_DATA>() as _,
            pPolicyCallbackData: null_mut(),
            pSIPClientData: null_mut(),
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 {
                pFile: &mut file_info,
            },
            dwStateAction: WTD_STATEACTION_VERIFY,
            hWVTStateData: null_mut(),
            pwszURLReference: null_mut(),
            dwProvFlags: 0,
            dwUIContext: 0,
            pSignatureSettings: null_mut(),
        };

        let mut guid = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let status = unsafe {
            WinVerifyTrust(
                null_mut(),
                &mut guid as *mut _,
                &mut data as *mut _ as *mut core::ffi::c_void,
            )
        };

        const OK: i32 = ERROR_SUCCESS as i32;
        let read = (status == OK).then(|| read(data.hWVTStateData));

        // The state handle must be released with a CLOSE call whatever the
        // verdict was, or every check leaks it.
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        unsafe {
            WinVerifyTrust(
                null_mut(),
                &mut guid as *mut _,
                &mut data as *mut _ as *mut core::ffi::c_void,
            );
        }

        // Not re-exported by windows-sys 0.59's Foundation module, and it is
        // the verdict that matters most: the signature is intact but the file
        // no longer hashes to what was signed, i.e. the payload was modified
        // in transit. Field-checked by flipping four bytes in a released MSI.
        const TRUST_E_BAD_DIGEST: i32 = 0x8009_6010_u32 as i32;
        match status {
            OK => read.unwrap_or_else(|| Err(VerifyError::Untrusted("no verdict".into()))),
            TRUST_E_BAD_DIGEST => Err(VerifyError::Untrusted(
                "the payload does not match its signature — it was modified after signing".into(),
            )),
            TRUST_E_NOSIGNATURE => Err(VerifyError::Untrusted("the file is not signed".into())),
            TRUST_E_EXPLICIT_DISTRUST => Err(VerifyError::Untrusted(
                "the signature is present but explicitly distrusted".into(),
            )),
            TRUST_E_SUBJECT_NOT_TRUSTED => Err(VerifyError::Untrusted(
                "the signature is present but not trusted".into(),
            )),
            CRYPT_E_SECURITY_SETTINGS => Err(VerifyError::Untrusted(
                "local policy rejected the publisher or hash".into(),
            )),
            // #3246 — said apart from a bad file, because the way on differs: a
            // machine whose root certificates Windows Update does not keep
            // current refuses a genuine file, and its administrator can fix that.
            CERT_E_UNTRUSTEDROOT | CERT_E_CHAINING => Err(VerifyError::Untrusted(
                "this computer does not trust the root of the certificate that signed it, which \
                 Windows Update normally keeps current, so an administrator may need to run it"
                    .into(),
            )),
            CERT_E_EXPIRED => Err(VerifyError::Untrusted(
                "the certificate that signed it has expired".into(),
            )),
            CERT_E_REVOKED => Err(VerifyError::Untrusted(
                "the certificate that signed it has been revoked".into(),
            )),
            other => Err(VerifyError::Untrusted(format!(
                "WinVerifyTrust returned 0x{other:08X}"
            ))),
        }
    }

    /// The signer certificate's simple display name (the common name shown in
    /// the file's Digital Signatures tab).
    pub(super) fn signer_name(path: &Path) -> Result<String, VerifyError> {
        let file = wide(path);

        let mut encoding = 0u32;
        let mut content_type = 0u32;
        let mut format_type = 0u32;
        let mut store = null_mut();
        let mut msg = null_mut();

        let ok = unsafe {
            CryptQueryObject(
                CERT_QUERY_OBJECT_FILE,
                file.as_ptr() as *const core::ffi::c_void,
                CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
                CERT_QUERY_FORMAT_FLAG_BINARY,
                0,
                &mut encoding,
                &mut content_type,
                &mut format_type,
                &mut store,
                &mut msg,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(VerifyError::Untrusted(format!(
                "CryptQueryObject failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Two-call idiom: ask for the size, allocate, ask again.
        let mut len = 0u32;
        let ok = unsafe { CryptMsgGetParam(msg, CMSG_SIGNER_INFO_PARAM, 0, null_mut(), &mut len) };
        if ok == 0 || (len as usize) < size_of::<CMSG_SIGNER_INFO>() {
            return Err(VerifyError::Untrusted("signer info unavailable".into()));
        }
        let mut buf = vec![0u8; len as usize];
        let ok = unsafe {
            CryptMsgGetParam(
                msg,
                CMSG_SIGNER_INFO_PARAM,
                0,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                &mut len,
            )
        };
        if ok == 0 {
            return Err(VerifyError::Untrusted("signer info unreadable".into()));
        }

        // Look the signer's certificate up in the message's own store by
        // (issuer, serial) — the pair that identifies which cert signed this.
        let signer = buf.as_ptr() as *const CMSG_SIGNER_INFO;
        let mut want: CERT_INFO = unsafe { std::mem::zeroed() };
        want.Issuer = unsafe { (*signer).Issuer };
        want.SerialNumber = unsafe { (*signer).SerialNumber };

        let cert = unsafe {
            CertFindCertificateInStore(
                store,
                encoding,
                0,
                CERT_FIND_SUBJECT_CERT,
                &want as *const _ as *const core::ffi::c_void,
                null_mut(),
            )
        };
        if cert.is_null() {
            return Err(VerifyError::Untrusted(
                "signer certificate not found".into(),
            ));
        }
        let name = cert_name(cert);
        unsafe { CertFreeCertificateContext(cert) };
        name
    }

    /// A certificate's simple display name. `cert` stays the caller's.
    fn cert_name(cert: *const CERT_CONTEXT) -> Result<String, VerifyError> {
        let needed = unsafe {
            CertGetNameStringW(
                cert,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                null_mut(),
                null_mut(),
                0,
            )
        };
        if needed == 0 {
            return Err(VerifyError::Untrusted("signer name unavailable".into()));
        }
        let mut name = vec![0u16; needed as usize];
        let written = unsafe {
            CertGetNameStringW(
                cert,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                null_mut(),
                name.as_mut_ptr(),
                needed,
            )
        };
        if written == 0 {
            return Err(VerifyError::Untrusted("signer name unreadable".into()));
        }

        // `written` counts the NUL; drop it before decoding.
        let end = (written as usize).saturating_sub(1);
        Ok(String::from_utf16_lossy(&name[..end]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_file_is_refused() {
        // A file that is definitely not a signed PE: our own source.
        let src = Path::new(file!());
        match verify_publisher(src, EXPECTED_PUBLISHER) {
            // Windows: no signature.
            Err(VerifyError::Untrusted(_)) => {}
            // Elsewhere: the check cannot run, and says so rather than passing.
            Err(VerifyError::Unsupported) => {}
            other => panic!("an unsigned file must never verify, got {other:?}"),
        }
    }

    /// #3246 — through a handle, the same refusal.
    #[test]
    fn an_unsigned_open_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.msi");
        std::fs::write(&src, b"not signed").unwrap();
        let open = std::fs::File::open(&src).unwrap();
        match verify_open_publisher(&open, &src, EXPECTED_PUBLISHER) {
            Err(VerifyError::Untrusted(_)) | Err(VerifyError::Unsupported) => {}
            other => panic!("an unsigned file must never verify, got {other:?}"),
        }
    }

    /// #3246 — a REAL signed release artifact, read the way the wizard reads it:
    /// through a handle that refuses writers. It verifies; a copy with four
    /// bytes flipped does not; and the publisher check compares the name it
    /// read. Ignored like the test below; same variable.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn a_held_release_artifact_verifies_and_a_tampered_copy_does_not() {
        use std::os::windows::fs::OpenOptionsExt;
        let Ok(p) = std::env::var("ROOMLER_TEST_SIGNED_MSI") else {
            eprintln!("set ROOMLER_TEST_SIGNED_MSI to a downloaded release MSI");
            return;
        };
        let held = |path: &Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1) // FILE_SHARE_READ
                .open(path)
                .unwrap()
        };
        let path = Path::new(&p);
        let signer = verify_open_publisher(&held(path), path, EXPECTED_PUBLISHER)
            .expect("a released artifact must verify through a handle");
        println!("signer: {signer}");
        assert!(signer.contains(EXPECTED_PUBLISHER));

        match verify_open_publisher(&held(path), path, "Someone Else Ltd") {
            Err(VerifyError::WrongPublisher { found, .. }) => assert_eq!(found, signer),
            other => panic!("the wrong name must be refused, got {other:?}"),
        }

        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("tampered.msi");
        let mut bytes = std::fs::read(path).unwrap();
        let at = bytes.len() / 2;
        bytes[at..at + 4].iter_mut().for_each(|b| *b ^= 0xff);
        std::fs::write(&copy, &bytes).unwrap();
        match verify_open_publisher(&held(&copy), &copy, EXPECTED_PUBLISHER) {
            Err(VerifyError::Untrusted(why)) => println!("tampered: {why}"),
            other => panic!("a tampered copy must be refused, got {other:?}"),
        }
    }

    #[test]
    fn wrong_publisher_message_names_both_sides() {
        let e = VerifyError::WrongPublisher {
            found: "Evil Corp".into(),
            expected: EXPECTED_PUBLISHER.into(),
        };
        let s = e.to_string();
        assert!(s.contains("Evil Corp"), "{s}");
        assert!(s.contains(EXPECTED_PUBLISHER), "{s}");
    }

    /// Verifies a REAL signed release artifact when one is present.
    ///
    /// Ignored by default because it needs a downloaded MSI; run with
    /// `ROOMLER_TEST_SIGNED_MSI=<path> cargo test -p roomlerd --lib
    /// signed_release_artifact -- --ignored --nocapture` on Windows.
    #[test]
    #[ignore]
    fn signed_release_artifact_verifies_as_the_expected_publisher() {
        let Ok(p) = std::env::var("ROOMLER_TEST_SIGNED_MSI") else {
            eprintln!("set ROOMLER_TEST_SIGNED_MSI to a downloaded release MSI");
            return;
        };
        let signer = verify_publisher(Path::new(&p), EXPECTED_PUBLISHER)
            .expect("a released artifact must verify");
        println!("signer: {signer}");
        assert!(signer.contains(EXPECTED_PUBLISHER));
    }
}
