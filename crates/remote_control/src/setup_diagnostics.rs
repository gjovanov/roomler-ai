// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P3b — the setup wizard's **opt-in** diagnostics, one record per
//! step boundary (`docs/growth-observability.md`, the install trace; spec
//! §3e item 5).
//!
//! Shared by both ends of `POST /api/setup/diagnostics`: the wizard builds a
//! [`SetupDiagnostic`] and the server validates it with
//! [`SetupDiagnostic::validate`] before storing it — the same vocabulary on
//! both sides, so a record is either exactly this shape or refused with a
//! code. Strings on the wire (not the enums), like [`crate::models::InstallInfo`]:
//! an older server meeting a spelling it does not know refuses that one
//! record and nothing else.
//!
//! What a record can carry is the whole privacy argument: a random per-run
//! id, a step name from a fixed list, an outcome from a fixed list, a
//! duration, an error **code** from a fixed list — never an error string, a
//! path, a hostname, a user name or the token — the OS family and the
//! wizard's version. The server adds its own clock and nothing about the
//! request (no address, no User-Agent).

use serde::{Deserialize, Deserializer, Serialize, de};

/// A run id is exactly this many lowercase hex characters (a v4 UUID's
/// simple form), minted by the wizard per launch and kept in memory only.
pub const RUN_LEN: usize = 32;

/// A step cannot take longer than a day; anything past this is a clock
/// gone wrong, not a measurement.
pub const ELAPSED_MS_MAX: u64 = 24 * 60 * 60 * 1000;

/// `wizard_version` is a Cargo version (`0.4.138`, `0.4.139-rc.1`): short,
/// starting with a digit, from a small alphabet.
pub const WIZARD_VERSION_MAX_LEN: usize = 32;

/// A record is a few short fields; a body past this is not one.
pub const MAX_BODY_BYTES: usize = 1024;

/// The one record of the wire. Every key is always present (`error_code`
/// as `null` on a step that did not fail), so a body carries exactly these
/// seven keys and nothing else — `deny_unknown_fields` refuses an eighth,
/// and a missing key fails the parse, `error_code` included
/// ([`present_or_null`]: serde would otherwise read an absent `Option`
/// field as `None`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetupDiagnostic {
    /// [`RUN_LEN`] lowercase hex characters, random, per wizard launch.
    pub run: String,
    /// One of [`Step`]'s spellings.
    pub step: String,
    /// One of [`Outcome`]'s spellings.
    pub outcome: String,
    /// How long the step took, in milliseconds (at most [`ELAPSED_MS_MAX`]).
    pub elapsed_ms: u64,
    /// One of [`ErrorCode`]'s spellings on a `failed` step; `null` otherwise.
    /// The key must be present.
    #[serde(deserialize_with = "present_or_null")]
    pub error_code: Option<String>,
    /// `windows` · `linux` · `macos` — the family, never a version.
    pub os: String,
    /// The wizard's own Cargo version.
    pub wizard_version: String,
}

/// `error_code` as a string or `null`, with the key **required**: serde's
/// own `Option` handling reads a missing field as `None`, which would let a
/// six-key body through. Going through `deserialize_any` makes an absent
/// key the `missing field` error it is for every other key.
fn present_or_null<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    struct StringOrNull;

    impl<'de> de::Visitor<'de> for StringOrNull {
        type Value = Option<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_owned()))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(StringOrNull)
        }
    }

    d.deserialize_any(StringOrNull)
}

/// The steps a wizard run passes, in the order a run reaches them. Three
/// are the wizard's pages (measured by the SPA), the rest the install
/// pipeline's (measured by the orchestrators). Not every run has every
/// step: the MSI steps are the daemon roles', extract and integrate the
/// tunnel role's, the companion and the anchor are conditional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Step {
    Welcome,
    Server,
    Token,
    Preflight,
    Resolve,
    Download,
    Verify,
    Msi,
    Extract,
    Integrate,
    Enroll,
    Companion,
    Anchor,
    Done,
}

impl Step {
    /// Every step, in run order — the digest's row order.
    pub const ALL: [Step; 14] = [
        Step::Welcome,
        Step::Server,
        Step::Token,
        Step::Preflight,
        Step::Resolve,
        Step::Download,
        Step::Verify,
        Step::Msi,
        Step::Extract,
        Step::Integrate,
        Step::Enroll,
        Step::Companion,
        Step::Anchor,
        Step::Done,
    ];

    pub fn wire(self) -> &'static str {
        match self {
            Step::Welcome => "welcome",
            Step::Server => "server",
            Step::Token => "token",
            Step::Preflight => "preflight",
            Step::Resolve => "resolve",
            Step::Download => "download",
            Step::Verify => "verify",
            Step::Msi => "msi",
            Step::Extract => "extract",
            Step::Integrate => "integrate",
            Step::Enroll => "enroll",
            Step::Companion => "companion",
            Step::Anchor => "anchor",
            Step::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Step> {
        Step::ALL.into_iter().find(|v| v.wire() == s)
    }

    /// The step's position in run order, for sorting rows read back.
    pub fn index(self) -> usize {
        Step::ALL
            .iter()
            .position(|s| *s == self)
            .unwrap_or(usize::MAX)
    }
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Outcome {
    /// The boundary was passed.
    Ok,
    /// The step errored; `error_code` says how.
    Failed,
    /// The operator cancelled during the step.
    Cancelled,
}

impl Outcome {
    pub const ALL: [Outcome; 3] = [Outcome::Ok, Outcome::Failed, Outcome::Cancelled];

    pub fn wire(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Outcome> {
        Outcome::ALL.into_iter().find(|v| v.wire() == s)
    }
}

/// Why a step failed — a code, never the message. The vocabulary is the
/// wizard's own error sites; a site that fits none says `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorCode {
    /// A route list in the Advanced options did not parse (before the MSI).
    AdvancedOptions,
    /// The role has no pipeline on this OS or architecture.
    UnsupportedPlatform,
    /// The pre-flight probe failed.
    Preflight,
    /// The installer proxy did not resolve a release.
    Resolve,
    /// The download did not complete.
    Download,
    /// The downloaded artifact's SHA-256 did not match.
    Sha256Mismatch,
    /// msiexec could not be started.
    MsiSpawn,
    /// Waiting for msiexec failed (never its exit code).
    MsiWait,
    /// The operator declined the UAC prompt (exit 1602).
    MsiUserCancel,
    /// Another Windows Installer operation was running (exit 1618).
    MsiAnotherInstall,
    /// The MSI failed (exit 1603).
    MsiFatal,
    /// msiexec exited with another code.
    MsiOther,
    /// The archive did not extract or held no binary.
    Extract,
    /// PATH / symlink integration failed.
    Integrate,
    /// The server refused the enrollment (a bad or spent token, a policy).
    EnrollRejected,
    /// The enrollment request did not reach a server answer.
    EnrollNetwork,
    /// The enrolled config could not be written.
    ConfigWrite,
    /// The desktop companion was not placed (best-effort; the install stands).
    Companion,
    /// The device was not anchored (best-effort; the install stands).
    Anchor,
    /// None of the above.
    Other,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 20] = [
        ErrorCode::AdvancedOptions,
        ErrorCode::UnsupportedPlatform,
        ErrorCode::Preflight,
        ErrorCode::Resolve,
        ErrorCode::Download,
        ErrorCode::Sha256Mismatch,
        ErrorCode::MsiSpawn,
        ErrorCode::MsiWait,
        ErrorCode::MsiUserCancel,
        ErrorCode::MsiAnotherInstall,
        ErrorCode::MsiFatal,
        ErrorCode::MsiOther,
        ErrorCode::Extract,
        ErrorCode::Integrate,
        ErrorCode::EnrollRejected,
        ErrorCode::EnrollNetwork,
        ErrorCode::ConfigWrite,
        ErrorCode::Companion,
        ErrorCode::Anchor,
        ErrorCode::Other,
    ];

    pub fn wire(self) -> &'static str {
        match self {
            ErrorCode::AdvancedOptions => "advanced_options",
            ErrorCode::UnsupportedPlatform => "unsupported_platform",
            ErrorCode::Preflight => "preflight",
            ErrorCode::Resolve => "resolve",
            ErrorCode::Download => "download",
            ErrorCode::Sha256Mismatch => "sha256_mismatch",
            ErrorCode::MsiSpawn => "msi_spawn",
            ErrorCode::MsiWait => "msi_wait",
            ErrorCode::MsiUserCancel => "msi_user_cancel",
            ErrorCode::MsiAnotherInstall => "msi_another_install",
            ErrorCode::MsiFatal => "msi_fatal",
            ErrorCode::MsiOther => "msi_other",
            ErrorCode::Extract => "extract",
            ErrorCode::Integrate => "integrate",
            ErrorCode::EnrollRejected => "enroll_rejected",
            ErrorCode::EnrollNetwork => "enroll_network",
            ErrorCode::ConfigWrite => "config_write",
            ErrorCode::Companion => "companion",
            ErrorCode::Anchor => "anchor",
            ErrorCode::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<ErrorCode> {
        ErrorCode::ALL.into_iter().find(|v| v.wire() == s)
    }
}

/// The OS families a record may name — the spellings `agents.os` uses.
pub const OS_FAMILIES: [&str; 3] = ["windows", "linux", "macos"];

/// Pure: a run id as the wizard mints it.
pub fn is_run_id(s: &str) -> bool {
    s.len() == RUN_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Pure: a Cargo version, bounded and from a small alphabet.
pub fn is_wizard_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= WIZARD_VERSION_MAX_LEN
        && s.as_bytes()[0].is_ascii_digit()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

impl SetupDiagnostic {
    /// Every field against its vocabulary or format. `Err` is the refusal
    /// code the server answers with — a stable word, never the value.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !is_run_id(&self.run) {
            return Err("invalid_run");
        }
        Step::parse(&self.step).ok_or("unknown_step")?;
        let outcome = Outcome::parse(&self.outcome).ok_or("unknown_outcome")?;
        if self.elapsed_ms > ELAPSED_MS_MAX {
            return Err("elapsed_out_of_range");
        }
        match (outcome, self.error_code.as_deref()) {
            (Outcome::Failed, Some(code)) => {
                ErrorCode::parse(code).ok_or("unknown_error_code")?;
            }
            (Outcome::Failed, None) => return Err("missing_error_code"),
            (_, Some(_)) => return Err("unexpected_error_code"),
            (_, None) => {}
        }
        if !OS_FAMILIES.contains(&self.os.as_str()) {
            return Err("unknown_os");
        }
        if !is_wizard_version(&self.wizard_version) {
            return Err("invalid_wizard_version");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> SetupDiagnostic {
        SetupDiagnostic {
            run: "0123456789abcdef0123456789abcdef".into(),
            step: "download".into(),
            outcome: "ok".into(),
            elapsed_ms: 1234,
            error_code: None,
            os: "windows".into(),
            wizard_version: "0.4.138".into(),
        }
    }

    /// The wire spellings are the stored spellings and the digest's grouping
    /// keys: a rename splits a series, so they are locked.
    #[test]
    fn the_vocabularies_are_locked_and_round_trip() {
        let steps: Vec<&str> = Step::ALL.iter().map(|s| s.wire()).collect();
        assert_eq!(
            steps,
            [
                "welcome",
                "server",
                "token",
                "preflight",
                "resolve",
                "download",
                "verify",
                "msi",
                "extract",
                "integrate",
                "enroll",
                "companion",
                "anchor",
                "done"
            ]
        );
        for s in Step::ALL {
            assert_eq!(Step::parse(s.wire()), Some(s));
        }
        assert_eq!(Step::parse("Download"), None);
        for o in Outcome::ALL {
            assert_eq!(Outcome::parse(o.wire()), Some(o));
        }
        assert_eq!(
            Outcome::ALL.map(|o| o.wire()),
            ["ok", "failed", "cancelled"]
        );
        let mut seen = std::collections::HashSet::new();
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(c.wire()), Some(c));
            assert!(seen.insert(c.wire()), "duplicate spelling {}", c.wire());
            assert!(
                c.wire()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{} is not a code",
                c.wire()
            );
        }
        assert_eq!(Step::Welcome.index(), 0);
        assert_eq!(Step::Done.index(), 13);
    }

    /// A body carries exactly the seven documented keys, `error_code`
    /// included as `null` — what AC12's "exactly the documented fields"
    /// compares against.
    #[test]
    fn a_record_serialises_exactly_seven_keys() {
        let v = serde_json::to_value(valid()).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "run",
                "step",
                "outcome",
                "elapsed_ms",
                "error_code",
                "os",
                "wizard_version"
            ]
        );
        assert!(v["error_code"].is_null());
        let back: SetupDiagnostic = serde_json::from_value(v).unwrap();
        assert_eq!(back, valid());
    }

    #[test]
    fn an_unknown_key_is_refused_at_parse() {
        let mut v = serde_json::to_value(valid()).unwrap();
        v["hostname"] = serde_json::json!("DESKTOP-1");
        assert!(serde_json::from_value::<SetupDiagnostic>(v).is_err());
    }

    /// Every key is required — `error_code` too, which serde would
    /// otherwise default to `None` when the key is simply left out.
    #[test]
    fn a_missing_key_is_refused_at_parse_error_code_included() {
        for key in [
            "run",
            "step",
            "outcome",
            "elapsed_ms",
            "error_code",
            "os",
            "wizard_version",
        ] {
            let mut v = serde_json::to_value(valid()).unwrap();
            v.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<SetupDiagnostic>(v).is_err(),
                "a body without {key} parsed"
            );
        }
        // Present as null, and present as a string, both read.
        let v = serde_json::json!({
            "run": valid().run, "step": "msi", "outcome": "failed", "elapsed_ms": 1,
            "error_code": "msi_fatal", "os": "windows", "wizard_version": "0.4.138",
        });
        let d: SetupDiagnostic = serde_json::from_value(v).unwrap();
        assert_eq!(d.error_code.as_deref(), Some("msi_fatal"));
        assert_eq!(d.validate(), Ok(()));
        let v = serde_json::json!({
            "run": valid().run, "step": "msi", "outcome": "ok", "elapsed_ms": 1,
            "error_code": null, "os": "windows", "wizard_version": "0.4.138",
        });
        let d: SetupDiagnostic = serde_json::from_value(v).unwrap();
        assert_eq!(d.error_code, None);
        // A number is neither.
        let v = serde_json::json!({
            "run": valid().run, "step": "msi", "outcome": "ok", "elapsed_ms": 1,
            "error_code": 7, "os": "windows", "wizard_version": "0.4.138",
        });
        assert!(serde_json::from_value::<SetupDiagnostic>(v).is_err());
    }

    #[test]
    fn a_valid_record_validates_and_each_field_is_checked() {
        assert_eq!(valid().validate(), Ok(()));
        let mut d = valid();
        d.outcome = "failed".into();
        d.error_code = Some("msi_fatal".into());
        assert_eq!(d.validate(), Ok(()));
        d.outcome = "cancelled".into();
        d.error_code = None;
        assert_eq!(d.validate(), Ok(()));

        let check = |f: fn(&mut SetupDiagnostic), code: &str| {
            let mut d = valid();
            f(&mut d);
            assert_eq!(d.validate(), Err(code), "{d:?}");
        };
        check(
            |d| d.run = "0123456789ABCDEF0123456789abcdef".into(),
            "invalid_run",
        );
        check(|d| d.run = "0123456789abcdef".into(), "invalid_run");
        check(|d| d.run = String::new(), "invalid_run");
        check(|d| d.step = "install".into(), "unknown_step");
        check(|d| d.outcome = "error".into(), "unknown_outcome");
        check(
            |d| d.elapsed_ms = ELAPSED_MS_MAX + 1,
            "elapsed_out_of_range",
        );
        check(
            |d| {
                d.outcome = "failed".into();
                d.error_code = Some("C:\\Users\\me\\log.txt".into());
            },
            "unknown_error_code",
        );
        check(
            |d| {
                d.outcome = "failed".into();
                d.error_code = None;
            },
            "missing_error_code",
        );
        check(
            |d| d.error_code = Some("other".into()),
            "unexpected_error_code",
        );
        check(|d| d.os = "Windows 11".into(), "unknown_os");
        check(|d| d.os = "freebsd".into(), "unknown_os");
        check(
            |d| d.wizard_version = "v0.4.138".into(),
            "invalid_wizard_version",
        );
        check(
            |d| d.wizard_version = String::new(),
            "invalid_wizard_version",
        );
        check(
            |d| d.wizard_version = "0.4.138 (DESKTOP-1)".into(),
            "invalid_wizard_version",
        );
        check(
            |d| d.wizard_version = "0".repeat(33),
            "invalid_wizard_version",
        );
    }

    #[test]
    fn a_body_is_small() {
        let s = serde_json::to_string(&SetupDiagnostic {
            run: "f".repeat(RUN_LEN),
            step: "integrate".into(),
            outcome: "failed".into(),
            elapsed_ms: ELAPSED_MS_MAX,
            error_code: Some("msi_another_install".into()),
            os: "windows".into(),
            wizard_version: "0".repeat(WIZARD_VERSION_MAX_LEN),
        })
        .unwrap();
        assert!(s.len() < MAX_BODY_BYTES, "{}", s.len());
    }
}
