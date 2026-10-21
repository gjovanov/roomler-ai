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
//!
//! P3-4e — a policy may allow a use only with a person's approval. The vault
//! then posts a card into the session's room and refuses
//! `awaiting_approval`, naming the approval; once a driver has said yes, the
//! same request is answered with a lease, which spends the approval.
//!
//! P3-7-1b — a session asks for its own egress with `rc:vault.net`: a SOCKS5
//! listener, or one forward, through one exit ([`NetKind`]). The vault
//! answers `rc:vault.net.granted`, a grant signed by the server's auth
//! service under an audience of its own (`net_grant`), or `rc:vault.refused`.
//! A grant is no lease: it carries no value, and nothing revokes it before
//! its end. A forward's host is decided and granted in one spelling,
//! [`canonical_net_host`]'s.
//!
//! P3-8-1a — a session asks to run one command on another device with
//! `rc:vault.ssh`. The vault answers `rc:vault.ssh.granted`, a ticket signed
//! under an audience of its own (`ssh_ticket`) that never names the asking
//! device, or `rc:vault.refused` (`no_target` for a target that is no live
//! device of the organization). A ticket, like a grant, is no lease.

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
    /// P3-4e — how long an approval card waits for a driver's answer, in
    /// seconds. Until then the same request is refused `awaiting_approval`,
    /// and asking again posts no second card.
    pub const APPROVAL_ANSWER_SECS: u32 = 600;
    /// P3-4e — how long a yes stays usable, in seconds from the answer: the
    /// session asks again within it, and the one lease it gets spends it.
    pub const APPROVAL_USE_SECS: u32 = 300;
    /// P3-7-1b — a forward's destination host, at most as long as DNS
    /// allows a name.
    pub const MAX_NET_HOST_LEN: usize = 253;
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
    /// `static` · `structured` · (P3-6b) `dynamic`.
    pub kind: String,
    /// P3-6b — a dynamic secret's provider ([`providers`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The fields a session may name: those that hold a value, `value` for
    /// a static secret; for a dynamic one, its minted credential's, never
    /// its issuer's.
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
    /// P3-4e — the vault's policies allow this only with a person's
    /// approval: a card waits in the session's room, and the same request is
    /// answered once a driver has said yes (a no makes it `denied`).
    AwaitingApproval,
    /// P3-7-1b — no live device of this organization by the exit's id.
    NoExit,
    /// P3-8-1a — no live device of this organization by the SSH target's id.
    NoTarget,
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
            Self::AwaitingApproval => "awaiting_approval",
            Self::NoExit => "no_exit",
            Self::NoTarget => "no_target",
        }
    }

    pub const ALL: [LeaseRefusal; 17] = [
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
        Self::AwaitingApproval,
        Self::NoExit,
        Self::NoTarget,
    ];

    /// Parse a refusal word. `None` for a word this build cannot name, which
    /// is still a refusal.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.wire() == s)
    }

    /// Whether the same request could be answered later: the vault was
    /// unavailable, the decision went unrecorded, the queue was full, or a
    /// person has yet to answer its approval card. Every other word is the
    /// vault's answer to THIS request as it stands.
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::Undecided | Self::Busy | Self::AwaitingApproval
        )
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

/// P3-7-1b — the egress a session asks for (`rc:vault.net`): a SOCKS5
/// listener, whose flows reach what the tunnel policies let through the
/// exit, or a forward to one destination. Cedar decides them as two actions,
/// `net:socks` and `net:forward`, so a permit for one opens nothing for the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetKind {
    Socks,
    Forward,
}

impl NetKind {
    /// Exactly what crosses the wire, and what a grant names. ⚠️ A
    /// compatibility surface, locked by test.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Socks => "socks",
            Self::Forward => "forward",
        }
    }

    pub const ALL: [NetKind; 2] = [Self::Socks, Self::Forward];

    /// `None` for a word this build cannot name: the vault refuses such a
    /// request `bad_request`, and a grant naming one is no grant.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.wire() == s)
    }
}

/// P3-7-1b — a forward's destination host in the one spelling the vault
/// decides and grants it in, and the network module (P3-7-2) and the device
/// (P3-7-3) compare it in:
/// - an IP address in its canonical form, an IPv4-mapped IPv6 address as its
///   IPv4;
/// - a DNS name lowercased, without the root's dot, in labels of letters,
///   digits, `-` and `_`, none empty, none longer than 63, none starting or
///   ending with `-`.
///
/// `None` for anything else: brackets, a zone, a port, a path, a space.
///
/// ⚠️ A policy compares `context.host` by equality, so the spelling a session
/// asks in must never decide whether a `forbid` applies: `DB.internal.` is
/// `db.internal`.
///
/// ⚠️ A name whose LAST label is a number (`10.0.0`, `167772165`,
/// `0xa000005`, `010.0.0.5`) is refused, never taken as a name. The system
/// resolver (`inet_aton`) reads each as an address, `010.0.0.5` in octal as
/// `8.0.0.5`, so a forward permitted toward a name could be dialled to an
/// address no policy saw. No top-level domain is numeric (RFC 3696 §2).
pub fn canonical_net_host(host: &str) -> Option<String> {
    if host.is_empty() || host.len() > lease_limits::MAX_NET_HOST_LEN {
        return None;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(ip.to_canonical().to_string());
    }
    let name = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
    };
    let numeric = |l: &str| {
        l.bytes().all(|b| b.is_ascii_digit())
            || l.strip_prefix("0x")
                .is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let last = name.rsplit('.').next()?;
    (name.split('.').all(label_ok) && !numeric(last)).then_some(name)
}

/// P3-6b — the providers a `dynamic` secret names (FR-90 §3e "P3-6,
/// `short-lived`, as designed"). Each fixes two sets of names, which the
/// server and the daemon must agree on:
/// - the ISSUER's fields, which the secret stores and a lease's bundle
///   carries to the device that mints;
/// - the MINTED credential's, which a session names.
///
/// ⚠️ No mode releases an issuer's field to a session. A lease of a dynamic
/// secret names no field at all: the device takes every field a call names
/// from the one credential it mints for the lease.
pub mod providers {
    /// Where a provider's credential is minted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MintsOn {
        /// On the device that runs the session. The lease's bundle carries
        /// the issuer's fields to it, and they stay in daemon memory, as a
        /// `proxy` value does.
        Device,
    }

    /// One provider: its name, its issuer's fields, its credential's.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Provider {
        /// Exactly what a secret's `provider` says. ⚠️ A compatibility
        /// surface, locked by test.
        pub name: &'static str,
        /// The issuer's fields: the vault refuses a write of any other.
        pub issuer: &'static [&'static str],
        /// The issuer's fields no mint can do without. A lease of a secret
        /// missing one is refused `no_value`.
        pub required: &'static [&'static str],
        /// The minted credential's fields: what a session names.
        pub minted: &'static [&'static str],
        pub mints_on: MintsOn,
    }

    /// A Postgres user, made for the lease by the issuer's account. The
    /// minted `host`, `port` and `database` say where it signs in: the
    /// secret's one audience, and its `database`.
    pub const POSTGRES: Provider = Provider {
        name: "postgres",
        issuer: &["admin_user", "admin_password", "database", "grants", "ca"],
        required: &["admin_user", "admin_password", "database"],
        minted: &["user", "password", "host", "port", "database"],
        mints_on: MintsOn::Device,
    };

    /// P3-6d-1b — a MySQL user, made for the lease by the issuer's account
    /// and ended by an event the mint creates. Its fields are Postgres's:
    /// `grants` names roles (`role` or `role@host`, one per line), and the
    /// minted `host`, `port` and `database` say where it signs in.
    pub const MYSQL: Provider = Provider {
        name: "mysql",
        issuer: &["admin_user", "admin_password", "database", "grants", "ca"],
        required: &["admin_user", "admin_password", "database"],
        minted: &["user", "password", "host", "port", "database"],
        mints_on: MintsOn::Device,
    };

    /// P3-6f-2 — a Kubernetes service account's token, minted for the lease
    /// bound to a Secret made for it, which its revocation deletes. The
    /// issuer signs in by `admin_token`, or by `admin_cert` and `admin_key`:
    /// one of the two, which the device's minter holds it to, since
    /// `required` names only what every issuer holds. Its credential is
    /// named `admin_*` so it shares no name with the session's `token`; `ca`
    /// and `namespace` are the same value on both sides.
    pub const KUBERNETES: Provider = Provider {
        name: "kubernetes",
        issuer: &[
            "namespace",
            "service_account",
            "admin_token",
            "admin_cert",
            "admin_key",
            "ca",
            "audiences",
        ],
        required: &["namespace", "service_account"],
        minted: &["token", "server", "ca", "namespace"],
        mints_on: MintsOn::Device,
    };

    /// Every provider this build knows.
    pub const ALL: &[Provider] = &[POSTGRES, MYSQL, KUBERNETES];

    /// The provider a secret names. `None` for a name this build cannot
    /// name, which nothing mints for.
    pub fn named(name: &str) -> Option<&'static Provider> {
        ALL.iter().find(|p| p.name == name)
    }
}

/// FR-90 P3-7-2a — the `rc:error` code that refuses a `rc:tunnel.open` whose
/// session grant does not hold; its message is the failing check's words.
/// The device reads it as the end of that grant, never a reason to retry:
/// the session asks the vault again. Wire-stable, do not reword.
pub const NET_GRANT_REFUSED: &str = "net_grant_refused";

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
                "awaiting_approval",
                "no_exit",
                "no_target",
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

    /// P3-7-1b — spelled out literally, so a rename is a deliberate edit
    /// here.
    #[test]
    fn net_kinds_are_locked_and_round_trip() {
        let words: Vec<&str> = NetKind::ALL.iter().map(|k| k.wire()).collect();
        assert_eq!(words, ["socks", "forward"]);
        for k in NetKind::ALL {
            assert_eq!(NetKind::from_wire(k.wire()), Some(k));
        }
        assert_eq!(NetKind::from_wire("udp"), None);
        assert_eq!(NetKind::from_wire("SOCKS"), None, "one spelling");
    }

    /// P3-7-1b — one spelling per destination, so a `forbid` cannot be
    /// stepped around by asking in another, and nothing a resolver would read
    /// as some other address.
    #[test]
    fn a_forwards_host_has_one_spelling() {
        let longest = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        for (asked, canonical) in [
            ("db.internal", "db.internal"),
            ("DB.Internal.", "db.internal"),
            ("db", "db"),
            ("db-1.corp.example", "db-1.corp.example"),
            ("_ldap._tcp.corp.example", "_ldap._tcp.corp.example"),
            ("xn--bcher-kva.example", "xn--bcher-kva.example"),
            ("localhost", "localhost"),
            ("10.0.0.5", "10.0.0.5"),
            ("::1", "::1"),
            ("2001:DB8::1", "2001:db8::1"),
            ("::ffff:10.0.0.5", "10.0.0.5"),
            ("::ffff:a00:5", "10.0.0.5"),
            (longest.as_str(), longest.as_str()),
        ] {
            assert_eq!(
                canonical_net_host(asked).as_deref(),
                Some(canonical),
                "{asked}"
            );
        }
        for bad in [
            "",
            ".",
            "db..internal",
            ".db",
            "-db.internal",
            "db-.internal",
            "db.internal:5432",
            "[::1]",
            "fe80::1%eth0",
            "db internal",
            "db/internal",
            "d\u{e9}.example",
            "10.0.0",
            "167772165",
            "0xa000005",
            "0XA000005",
            "010.0.0.5",
            "1.2.3.4.5",
            &"a".repeat(64),
            &format!("{}.example", "a".repeat(64)),
            &format!("{longest}x"),
        ] {
            assert_eq!(canonical_net_host(bad), None, "{bad:?}");
        }
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
                LeaseRefusal::Busy,
                LeaseRefusal::AwaitingApproval,
            ]
        );
    }

    #[test]
    fn a_yes_is_usable_for_less_time_than_a_card_waits() {
        const {
            assert!(lease_limits::APPROVAL_USE_SECS > 0);
            assert!(lease_limits::APPROVAL_USE_SECS <= lease_limits::APPROVAL_ANSWER_SECS);
        };
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

    /// P3-6b — a provider's names are what a secret stores and a session
    /// names on two builds at once: spelled out literally, so a rename is a
    /// deliberate edit here.
    #[test]
    fn a_providers_names_are_locked() {
        use providers::*;
        let names: Vec<&str> = ALL.iter().map(|p| p.name).collect();
        assert_eq!(names, ["postgres", "mysql", "kubernetes"]);
        for p in [POSTGRES, MYSQL] {
            assert_eq!(
                (p.issuer, p.required, p.minted),
                (
                    &["admin_user", "admin_password", "database", "grants", "ca"][..],
                    &["admin_user", "admin_password", "database"][..],
                    &["user", "password", "host", "port", "database"][..],
                ),
                "{}",
                p.name
            );
            assert_eq!(p.mints_on, MintsOn::Device, "{}", p.name);
        }
        // P3-6f-2 — the credential is `admin_*`, never the session's `token`.
        assert_eq!(
            (KUBERNETES.issuer, KUBERNETES.required, KUBERNETES.minted),
            (
                &[
                    "namespace",
                    "service_account",
                    "admin_token",
                    "admin_cert",
                    "admin_key",
                    "ca",
                    "audiences"
                ][..],
                &["namespace", "service_account"][..],
                &["token", "server", "ca", "namespace"][..],
            )
        );
        assert_eq!(KUBERNETES.mints_on, MintsOn::Device);
        assert_eq!(named("postgres"), Some(&POSTGRES));
        assert_eq!(named("mysql"), Some(&MYSQL));
        assert_eq!(named("kubernetes"), Some(&KUBERNETES));
        assert_eq!(named("Postgres"), None, "one spelling");
        assert_eq!(named("k8s"), None, "one spelling");
        assert_eq!(named("MySQL"), None, "one spelling");
        assert_eq!(
            named("mssql"),
            None,
            "nothing mints for a name this build cannot name"
        );
    }

    #[test]
    fn a_provider_requires_only_its_issuers_fields() {
        for p in providers::ALL {
            assert!(!p.issuer.is_empty() && !p.minted.is_empty(), "{}", p.name);
            for f in p.required {
                assert!(p.issuer.contains(f), "{}: {f}", p.name);
            }
            let mut seen = std::collections::BTreeSet::new();
            for f in p.issuer {
                assert!(seen.insert(*f), "{}: {f} twice", p.name);
            }
        }
    }
}
