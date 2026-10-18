// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-3b — the vault's wire vocabulary: what a device and the server say
//! about a LEASE, a value the vault releases to one agent session for a while.
//!
//! The frames (`crate::signaling`): the device's `rc:vault.lease`, and the
//! server's `rc:vault.leased` (the value, sealed to the daemon's vault key,
//! `AgentHello::vault_key`) or `rc:vault.refused` (one word from
//! [`LeaseRefusal`]). A value crosses ONLY sealed: the server's frame carries
//! a libsodium sealed box (`crypto_box_seal`) that only the daemon holding
//! the key opens, so neither a log line nor a relay between pods ever holds a
//! value in the clear.
//!
//! A refusal word from a newer server that this build cannot name is still a
//! refusal ([`LeaseRefusal::from_wire`] answers `None`), never a value.
//!
//! P3-3c — the server takes a lease back with `rc:vault.revoke`: the leases
//! it names, and one word for why ([`RevokeReason`]). A device drops every
//! lease the frame names, whatever the word, one it cannot name included.
//!
//! P3-4d — a device reports what it did with leased values in
//! `rc:vault.activity` ([`VaultActivityKind`]): a claim the server keeps
//! apart from its decisions, and drops unless every lease it names was
//! released to that device, for that session, in that mode and audience.

/// Bounds on a lease request, read by both ends: the server refuses past
/// them (`bad_request`), and a device never asks past them.
pub mod lease_limits {
    /// The device's own id for a request, echoed in the answer.
    pub const MAX_REQ_ID_LEN: usize = 64;
    /// A secret named by path (at most 256 characters) or by RRN
    /// (`rrn:{org}:secret:{path}`).
    pub const MAX_SECRET_LEN: usize = 400;
    /// A field name, as the vault spells fields (at most 64 characters).
    pub const MAX_FIELD_LEN: usize = 64;
    /// How the session will use the value (`env`, `file`, `exec`, `proxy`…).
    pub const MAX_MODE_LEN: usize = 32;
    /// Where the value is meant for: a host, a service, an upstream.
    pub const MAX_AUDIENCE_LEN: usize = 256;
    /// The shortest and longest lease the vault grants, in seconds. A request
    /// outside them is clamped, not refused, and a policy sees the clamped
    /// value (`context.ttl_s`), so it can hold a lease shorter still.
    pub const MIN_TTL_SECS: u32 = 30;
    pub const MAX_TTL_SECS: u32 = 900;
    /// Requests one connection may have waiting for an answer. One more is
    /// answered `busy`, so a device that floods the vault queues nothing.
    pub const QUEUE_PER_CONNECTION: usize = 16;
    /// Requests one session may make on its device per minute. Past it the
    /// answer is `busy` and nothing is recorded: the ceiling is what keeps a
    /// flood from becoming a flood of audit rows.
    pub const RATE_PER_MINUTE: u32 = 120;
    /// P3-3c — the leases one `rc:vault.revoke` names at most; more go in
    /// further frames.
    pub const MAX_REVOKE_IDS: usize = 64;
    /// P3-4b — the secrets one `rc:vault.listed` names at most; past it the
    /// answer says there is `more`, and a narrower prefix finds the rest.
    pub const MAX_LISTED: usize = 200;
    /// P3-4c — the longest program name `exec_allow` holds.
    pub const MAX_PROGRAM_LEN: usize = 64;
    /// P3-4d — the leases one `rc:vault.activity` names at most: the most
    /// values one use hands on.
    pub const MAX_ACTIVITY_LEASES: usize = 8;
    /// P3-4d — the reports one session may make on its device per minute.
    /// Past it the server drops them: a report is a claim, and a flood of
    /// claims is not worth a flood of rows.
    pub const ACTIVITY_PER_MINUTE: u32 = 120;
}

/// P3-4c — a program's BARE name, as `exec_allow` holds it and as a device
/// asks in `exec` mode (the lease's audience): letters, digits, `.`, `_`,
/// `+` and `-`, not starting with `.` or `-`. Never a path: a device resolves
/// the name among the system's own directories and hands a value to nothing
/// else, so a path could only ever match nothing. One definition for the
/// vault's write check and the device's own.
pub fn program_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= lease_limits::MAX_PROGRAM_LEN
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// P3-4b — what a session is told of a secret: its name and its rules, never
/// a value, nor a version's number, nor who wrote it.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VaultSecretMeta {
    /// `rrn:{org}:secret:{path}`: the handle a session's tools name it by.
    pub rrn: String,
    pub path: String,
    /// `static` · `structured`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The fields that hold a value: `value` for a static secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    /// Where a value may be presented.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audiences: Vec<String>,
    /// The programs a value may be handed to, as `argv[0]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec_allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// A disabled secret is never used: its lease would be refused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// Why the vault refused a lease. The words are a compatibility surface: a
/// device decides from them whether asking again could help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseRefusal {
    /// The vault does not serve this organization (`vault.tenants`).
    NotServed,
    /// This connection takes no leases: it advertised no `vault` verb, or its
    /// hello carried no vault key.
    Unsupported,
    /// The request is malformed or past a bound.
    BadRequest,
    /// No session of this organization runs on this device by that id.
    NotLocated,
    /// The session runs here at a newer fence: this request is from a run
    /// that no longer holds it.
    StaleFence,
    /// The session is over.
    Ended,
    /// The session took no vault role at its start, so nothing in the vault
    /// is allowed it.
    NoRole,
    /// No live secret of this organization by that path or RRN.
    NoSecret,
    /// The secret is disabled.
    Disabled,
    /// The field has no value.
    NoValue,
    /// The vault's policies do not allow it.
    Denied,
    /// The vault cannot open values now (no usable KEK) or could not read
    /// what it needed; asking later may help.
    Unavailable,
    /// The decision could not be recorded, so it was not acted on.
    Undecided,
    /// Too many of this connection's requests are waiting already.
    Busy,
}

impl LeaseRefusal {
    /// Exactly what crosses the wire. ⚠️ A compatibility surface, locked by
    /// test.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::NotServed => "not_served",
            Self::Unsupported => "unsupported",
            Self::BadRequest => "bad_request",
            Self::NotLocated => "not_located",
            Self::StaleFence => "stale_fence",
            Self::Ended => "ended",
            Self::NoRole => "no_role",
            Self::NoSecret => "no_secret",
            Self::Disabled => "disabled",
            Self::NoValue => "no_value",
            Self::Denied => "denied",
            Self::Unavailable => "unavailable",
            Self::Undecided => "undecided",
            Self::Busy => "busy",
        }
    }

    pub const ALL: [LeaseRefusal; 14] = [
        Self::NotServed,
        Self::Unsupported,
        Self::BadRequest,
        Self::NotLocated,
        Self::StaleFence,
        Self::Ended,
        Self::NoRole,
        Self::NoSecret,
        Self::Disabled,
        Self::NoValue,
        Self::Denied,
        Self::Unavailable,
        Self::Undecided,
        Self::Busy,
    ];

    /// Parse a refusal word. `None` for a word this build cannot name, which
    /// is still a refusal.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.wire() == s)
    }

    /// Whether the same request could be answered later: the vault was
    /// unavailable, the decision went unrecorded, or the queue was full.
    /// Every other word is the vault's answer to THIS request as it stands.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Unavailable | Self::Undecided | Self::Busy)
    }
}

/// P3-3c — why the vault took a lease back. The words are a compatibility
/// surface for the audit and the device's log, never for the decision to
/// drop: a revoked lease is dropped whatever its word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// The session is over, or a stop was ordered.
    SessionEnded,
    /// The session runs at a newer fence, or on another device.
    SessionMoved,
    /// The field has a newer version.
    ValueChanged,
    /// The secret was disabled.
    SecretDisabled,
    /// The secret was deleted.
    SecretDeleted,
    /// The device's vault key is not the one the value was sealed to: the
    /// daemon restarted, and the lease died with its memory.
    KeyChanged,
    /// The device was removed.
    DeviceRemoved,
    /// The vault no longer serves the organization (`vault.tenants`).
    NotServed,
}

impl RevokeReason {
    /// Exactly what crosses the wire. ⚠️ A compatibility surface, locked by
    /// test.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::SessionEnded => "session_ended",
            Self::SessionMoved => "session_moved",
            Self::ValueChanged => "value_changed",
            Self::SecretDisabled => "secret_disabled",
            Self::SecretDeleted => "secret_deleted",
            Self::KeyChanged => "key_changed",
            Self::DeviceRemoved => "device_removed",
            Self::NotServed => "not_served",
        }
    }

    pub const ALL: [RevokeReason; 8] = [
        Self::SessionEnded,
        Self::SessionMoved,
        Self::ValueChanged,
        Self::SecretDisabled,
        Self::SecretDeleted,
        Self::KeyChanged,
        Self::DeviceRemoved,
        Self::NotServed,
    ];

    /// Parse a revocation word. `None` for a word this build cannot name,
    /// which still revokes.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.wire() == s)
    }
}

/// P3-4d — what a device reports it did with leased values
/// (`rc:vault.activity`). A CLAIM, by a host that may be compromised: the
/// server keeps it in `vault_activity`, apart from its own decisions in
/// `vault_audit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultActivityKind {
    /// `vault_exec` ran a command with the values: the target is the program,
    /// and the report says how the command ended.
    Exec,
    /// `vault_render` wrote a file with the values: the target is its name.
    Render,
}

impl VaultActivityKind {
    /// Exactly what crosses the wire. ⚠️ A compatibility surface, locked by
    /// test.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Render => "render",
        }
    }

    pub const ALL: [VaultActivityKind; 2] = [Self::Exec, Self::Render];

    /// The lease mode a use of this kind takes: a report must name leases
    /// released in it.
    pub const fn mode(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Render => "file",
        }
    }

    /// Parse a report's kind. `None` for a word this build cannot name: the
    /// server drops such a report, as an additive wire asks.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.wire() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spelled out literally, so a rename is a deliberate edit here.
    #[test]
    fn lease_refusal_words_are_locked_and_round_trip() {
        let words: Vec<&str> = LeaseRefusal::ALL.iter().map(|r| r.wire()).collect();
        assert_eq!(
            words,
            [
                "not_served",
                "unsupported",
                "bad_request",
                "not_located",
                "stale_fence",
                "ended",
                "no_role",
                "no_secret",
                "disabled",
                "no_value",
                "denied",
                "unavailable",
                "undecided",
                "busy",
            ]
        );
        for r in LeaseRefusal::ALL {
            assert_eq!(LeaseRefusal::from_wire(r.wire()), Some(r));
        }
        assert_eq!(LeaseRefusal::from_wire("from_a_newer_server"), None);
    }

    /// P3-4d — spelled out literally, so a rename is a deliberate edit here;
    /// each kind names the lease mode its use takes.
    #[test]
    fn activity_kinds_are_locked_and_name_their_mode() {
        let words: Vec<(&str, &str)> = VaultActivityKind::ALL
            .iter()
            .map(|k| (k.wire(), k.mode()))
            .collect();
        assert_eq!(words, [("exec", "exec"), ("render", "file")]);
        for k in VaultActivityKind::ALL {
            assert_eq!(VaultActivityKind::from_wire(k.wire()), Some(k));
        }
        assert_eq!(VaultActivityKind::from_wire("teleport"), None);
    }

    #[test]
    fn only_a_passing_condition_is_worth_asking_again() {
        let retry: Vec<LeaseRefusal> = LeaseRefusal::ALL
            .into_iter()
            .filter(|r| r.is_retryable())
            .collect();
        assert_eq!(
            retry,
            [
                LeaseRefusal::Unavailable,
                LeaseRefusal::Undecided,
                LeaseRefusal::Busy
            ]
        );
    }

    /// P3-4c — a bare name only: never a path, a flag, a dot entry or a
    /// space.
    #[test]
    fn a_program_is_named_by_its_bare_name() {
        for ok in [
            "psql",
            "kubectl",
            "aws",
            "python3.12",
            "g++",
            "pg_dump",
            "x-y",
        ] {
            assert!(program_name_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "/usr/bin/psql",
            "bin/psql",
            "..",
            ".hidden",
            "-rf",
            "psql ",
            "ps ql",
            "psql\0",
            "C:\\psql.exe",
            &"p".repeat(lease_limits::MAX_PROGRAM_LEN + 1),
        ] {
            assert!(!program_name_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_ttl_bounds_are_ordered() {
        const { assert!(lease_limits::MIN_TTL_SECS < lease_limits::MAX_TTL_SECS) };
    }

    /// Spelled out literally, so a rename is a deliberate edit here.
    #[test]
    fn revoke_words_are_locked_and_round_trip() {
        let words: Vec<&str> = RevokeReason::ALL.iter().map(|r| r.wire()).collect();
        assert_eq!(
            words,
            [
                "session_ended",
                "session_moved",
                "value_changed",
                "secret_disabled",
                "secret_deleted",
                "key_changed",
                "device_removed",
                "not_served",
            ]
        );
        for r in RevokeReason::ALL {
            assert_eq!(RevokeReason::from_wire(r.wire()), Some(r));
        }
        assert_eq!(RevokeReason::from_wire("from_a_newer_server"), None);
    }
}
