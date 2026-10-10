// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 — the Hive wire vocabulary: what the server and a device say ABOUT
//! an agent session, never what is in one.
//!
//! The lifecycle frames (`crate::signaling`): the server's `rc:hive.start`
//! and `rc:hive.stop`, the device's `rc:hive.start_ack`, `rc:hive.state` and
//! `rc:hive.turn`. None of them carries a prompt, a tool argument, a tool
//! output or a line of transcript, and the tests in `signaling` lock their
//! field sets so they cannot grow one: content travels device-to-browser
//! over the viewer peer and device-to-device between replicas, never through
//! the server (`docs/roomler-hive-design.md` §3.3).
//!
//! The viewer peer's signalling (P0d-2, `rc:hive.view.*`) is a grant, its
//! answer, and the SDP and ICE of a data-only WebRTC peer — the server relays
//! the handshake and never sees what then flows over the peer.
//!
//! The server pushes a Hive frame only to an agent advertising
//! [`crate::models::RpcCap::Hive`]: a caller is waiting for the answer, and a
//! pre-feature agent drops an unknown tag at `debug!`, which would read as a
//! hang (the `exec` rule).
//!
//! Everything a device REFUSES with is decoded leniently — an unknown word
//! from a newer agent is still a refusal ([`HiveRefusal::Other`]), and an
//! unknown run state is "a state this build cannot name" rather than an error
//! failing the whole frame. The reasons are the same as FR-83's grant ack;
//! see `grant_refusal_lenient` in `signaling`.

use serde::{Deserialize, Deserializer, Serialize};

/// The one harness P0 runs: Claude Code, headless on stream-json.
pub const HARNESS_CLAUDE_CODE: &str = "claude-code";

/// Server-side bounds on the Hive frames. Duplicated by no one: the device
/// reads them from here too, so a clamp cannot drift between the two ends.
pub mod hive_limits {
    /// How long a start request holds its caller for the device's answer.
    /// An ack that arrives later still lands on the record — the caller is
    /// told `starting` and learns the outcome from the session itself.
    pub const START_ACK_TIMEOUT_SECS: u64 = 10;
    /// A `starting` session still unanswered when its device reconnects is
    /// started again on that connection only within this window; past it,
    /// the start is marked `lost` rather than launching a session its owner
    /// may have given up on an hour ago.
    pub const START_REDELIVERY_WINDOW_SECS: i64 = 10 * 60;
    /// Session starts per (user, device) per minute, enforced AFTER the
    /// identity gates so a refusal is attributable (the exec rule).
    pub const START_RATE_PER_MINUTE: u32 = 10;
    /// The folder as typed. The device resolves and confines it; the server
    /// only refuses what no filesystem would accept.
    pub const MAX_FOLDER_LEN: usize = 1024;
    pub const MAX_TITLE_LEN: usize = 200;
    /// A device's `detail` is clamped by the device AND again on receipt.
    pub const MAX_DETAIL_LEN: usize = 512;
    /// FR-90 P1b — entries one `rc:hive.manifest` may carry. A device runs
    /// `hive_max_sessions`, a handful; a longer list is not a device's.
    pub const MAX_MANIFEST: usize = 256;
    /// FR-90 P1e — the most one rendered core-memory document (`CLAUDE.md`,
    /// the auto-memory `MEMORY.md`) may hold in `rc:hive.memory`. The
    /// budgets — 4,500 characters into `CLAUDE.md`, 800 into `MEMORY.md` —
    /// fit with room for headings even at four bytes a character; a larger
    /// one is no snapshot this server rendered, and the device drops it.
    pub const MAX_CORE_MEMORY_BYTES: usize = 32 * 1024;
    /// FR-90 P1j — keys one `rc:hive.adopt` may carry: the `hive_accounts`
    /// keys that map to one account. A handful; a longer list is no
    /// device's.
    pub const MAX_ADOPT_KEYS: usize = 8;
    /// A key as the device holds it: a user id, or an address.
    pub const MAX_ADOPT_KEY_LEN: usize = 320;
    /// The device's id for one offer, echoed by the ack.
    pub const MAX_ADOPT_ID_LEN: usize = 64;
    /// The local account name, as the device reports it.
    pub const MAX_ACCOUNT_LEN: usize = 64;
    /// Live adopted sessions one device may hold.
    pub const MAX_ADOPTED_PER_DEVICE: usize = 16;
    /// Adopt offers per device per minute — a terminal session is offered
    /// once, so more is a device in a loop or one that lies.
    pub const ADOPT_RATE_PER_MINUTE: u32 = 20;
}

/// FR-90 P1b — one session a device runs, in `rc:hive.manifest`: its id and
/// fence, nothing more.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct HiveManifestEntry {
    #[serde(with = "crate::serde_helpers::oid_hex")]
    pub session_id: bson::oid::ObjectId,
    pub fence: u64,
}

/// FR-90 P0d-2 — bounds on a viewer peer, read by both ends.
pub mod view_limits {
    /// A view grant's life, sent as a RELATIVE `ttl_secs` so the device sets
    /// its own deadline on receipt — a device whose clock is off must not
    /// refuse every grant, nor keep one past its time. The browser renews at
    /// half-life while the room is open, and the server re-checks the
    /// viewer's right to read on every renewal: a member taken out of the
    /// room loses the view within this bound.
    pub const GRANT_TTL_SECS: u32 = 10 * 60;
    /// How long the server waits for the device to confirm a grant before it
    /// tells the browser no. The browser dials only after the device said
    /// yes (FR-83).
    pub const GRANT_ACK_TIMEOUT_SECS: u64 = 10;
    /// Viewer peers one device serves at once.
    pub const MAX_PER_DEVICE: usize = 32;
    /// Viewer peers one session has at once.
    pub const MAX_PER_SESSION: usize = 8;
    /// View opens per (user, session) per minute, after the identity gates.
    pub const OPEN_RATE_PER_MINUTE: u32 = 20;
}

/// Why a device refused a view grant, carried in `rc:hive.view.grant_ack`.
/// Absent = the device holds the session and will answer the viewer's offer.
///
/// ⚠️ Decoded LENIENTLY, like [`HiveRefusal`]: an unknown word is
/// [`Self::Other`], still a refusal — never "the browser may dial".
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HiveViewRefusal {
    /// The device's own `hive_enabled` is off.
    HiveDisabled,
    /// The device holds no transcript of this session — it never ran it, or
    /// its store was lost.
    NoSession,
    /// [`view_limits::MAX_PER_DEVICE`] or [`view_limits::MAX_PER_SESSION`].
    AtCapacity,
    /// A word this build does not know.
    Other,
}

impl HiveViewRefusal {
    /// The spelling on the wire. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HiveDisabled => "hive_disabled",
            Self::NoSession => "no_session",
            Self::AtCapacity => "at_capacity",
            Self::Other => "other",
        }
    }

    /// Every word this build knows.
    pub const ALL: [HiveViewRefusal; 4] = [
        Self::HiveDisabled,
        Self::NoSession,
        Self::AtCapacity,
        Self::Other,
    ];
}

/// Why a device refused `rc:hive.start`, carried in `rc:hive.start_ack`.
///
/// Absent from the ack = ACCEPTED: the device passed its own gates and is
/// launching the harness; what happens next arrives as `rc:hive.state`.
/// Present = no session runs, and the word says which of the device's gates
/// said no — each one has a different fix, which is why they are not folded.
///
/// ⚠️ Decoded LENIENTLY ([`refusal_lenient`]): an unknown word lands on
/// [`Self::Other`], still a refusal. It must never land on "accepted" — that
/// would show a session as starting on a device that has just said no.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HiveRefusal {
    /// Gate 4 — the device's own `hive_enabled` is off (the default).
    HiveDisabled,
    /// `hive_accounts` maps no local account to the starting user. Never a
    /// fallback to the daemon's own identity, which is SYSTEM/root.
    NoAccount,
    /// Windows: nobody is signed in at the console, so there is no account a
    /// session could run as (design D2 — no S4U, no stored credentials).
    NoConsoleUser,
    /// The folder does not resolve under a `hive_roots` entry, or
    /// `hive_roots` is empty — which means nowhere, never anywhere.
    FolderNotAllowed,
    /// The harness is not installed where the mapped account can run it.
    HarnessMissing,
    /// The harness was found but did not start.
    LaunchFailed,
    /// The device already runs `hive_max_sessions` sessions.
    AtCapacity,
    /// FR-90 decision 15 — macOS: the account the session would run as may
    /// use `sudo` without a password, and the device's own
    /// `hive_allow_passwordless_sudo` is off. A Mac's `sudo` reads the
    /// account's groups from the directory, so no group a session drops can
    /// stop it, and the session would have root.
    PasswordlessSudo,
    /// A word this build does not know.
    Other,
}

impl HiveRefusal {
    /// The spelling on the wire and in the session record. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HiveDisabled => "hive_disabled",
            Self::NoAccount => "no_account",
            Self::NoConsoleUser => "no_console_user",
            Self::FolderNotAllowed => "folder_not_allowed",
            Self::HarnessMissing => "harness_missing",
            Self::LaunchFailed => "launch_failed",
            Self::AtCapacity => "at_capacity",
            Self::PasswordlessSudo => "passwordless_sudo",
            Self::Other => "other",
        }
    }

    /// Every word this build knows.
    pub const ALL: [HiveRefusal; 9] = [
        Self::HiveDisabled,
        Self::NoAccount,
        Self::NoConsoleUser,
        Self::FolderNotAllowed,
        Self::HarnessMissing,
        Self::LaunchFailed,
        Self::AtCapacity,
        Self::PasswordlessSudo,
        Self::Other,
    ];
}

/// FR-90 P1j — why the server did not adopt a terminal session, carried in
/// `rc:hive.adopt_ack`. Absent = adopted: the record exists and the device
/// mirrors the session.
///
/// ⚠️ Decoded LENIENTLY ([`adopt_refusal_lenient`]): an unknown word is
/// [`Self::Other`], still a refusal — never "adopted", which would have the
/// device mirror a terminal into a session nobody has a record of.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HiveAdoptRefusal {
    /// The device's organization is not one agent sessions serve
    /// (`hive.tenants`, P1g).
    HiveNotEnabled,
    /// No key the device sent names an account that exists here.
    NoAccount,
    /// The keys name two or more people: the account is shared, so whose
    /// terminal this is cannot be known, and it is never guessed.
    AmbiguousAccount,
    /// The one person the keys name is not a member of the device's org.
    NotAMember,
    /// The device already holds [`hive_limits::MAX_ADOPTED_PER_DEVICE`].
    AtCapacity,
    /// More than [`hive_limits::ADOPT_RATE_PER_MINUTE`] offers.
    RateLimited,
    /// A word this build does not know.
    Other,
}

impl HiveAdoptRefusal {
    /// The spelling on the wire and in `hive_audit`. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HiveNotEnabled => "hive_not_enabled",
            Self::NoAccount => "no_account",
            Self::AmbiguousAccount => "ambiguous_account",
            Self::NotAMember => "not_a_member",
            Self::AtCapacity => "at_capacity",
            Self::RateLimited => "rate_limited",
            Self::Other => "other",
        }
    }

    /// Every word this build knows.
    pub const ALL: [HiveAdoptRefusal; 7] = [
        Self::HiveNotEnabled,
        Self::NoAccount,
        Self::AmbiguousAccount,
        Self::NotAMember,
        Self::AtCapacity,
        Self::RateLimited,
        Self::Other,
    ];
}

/// Lenient decoder for `rc:hive.adopt_ack`'s `refused`, as
/// [`refusal_lenient`]: only absent or `null` means adopted.
pub(crate) fn adopt_refusal_lenient<'de, D>(de: D) -> Result<Option<HiveAdoptRefusal>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(de)? else {
        return Ok(None);
    };
    let serde_json::Value::String(word) = raw else {
        return Ok(Some(HiveAdoptRefusal::Other));
    };
    Ok(Some(
        HiveAdoptRefusal::deserialize(
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(word.as_str()),
        )
        .unwrap_or(HiveAdoptRefusal::Other),
    ))
}

/// Where a running session is, as its device reports it in `rc:hive.state`.
/// The server's record mirrors it; nothing here says what the session is
/// doing, only whether it is.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HiveRunState {
    /// The harness is up and waiting for a prompt.
    Idle,
    /// A turn is running.
    Running,
    /// A tool call is waiting for a person's answer.
    AwaitingApproval,
    /// The harness exited and the session is over on this device — stopped,
    /// finished, or crashed (`detail` says which).
    Ended,
}

impl HiveRunState {
    /// The spelling on the wire and in the session record. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Ended => "ended",
        }
    }
}

/// How a turn stands, as its device reports it in `rc:hive.turn`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HiveTurnStatus {
    /// A prompt went in; the turn is under way.
    Running,
    /// The turn finished.
    Ok,
    /// The turn finished with an error (the harness's own `is_error`).
    Error,
    /// The harness stopped mid-turn — a stop, a crash, a daemon restart.
    Interrupted,
}

impl HiveTurnStatus {
    /// The spelling on the wire. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
        }
    }
}

/// FR-90 P1a-2 — how an approval stands, as its device reports it in
/// `rc:hive.approval`. Which tool and what it would do are not here and
/// cannot be: they travel device-to-browser over the viewer peer.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HiveApprovalStatus {
    /// Waiting for a driver.
    Open,
    /// A driver allowed it.
    Allowed,
    /// A driver denied it.
    Denied,
    /// Nobody answered in time.
    Expired,
    /// The harness let go, or the session ended, first.
    Withdrawn,
}

impl HiveApprovalStatus {
    /// The spelling on the wire and in `agent_approvals`. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Withdrawn => "withdrawn",
        }
    }

    /// Every word this build knows.
    pub const ALL: [HiveApprovalStatus; 5] = [
        Self::Open,
        Self::Allowed,
        Self::Denied,
        Self::Expired,
        Self::Withdrawn,
    ];
}

/// Lenient decoder for `rc:hive.approval`'s `status`, as
/// [`turn_status_lenient`]: a word this build cannot name is `None`, and the
/// stub keeps what it said.
pub(crate) fn approval_status_lenient<'de, D>(de: D) -> Result<Option<HiveApprovalStatus>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(de)? else {
        return Ok(None);
    };
    let serde_json::Value::String(word) = raw else {
        return Ok(None);
    };
    Ok(HiveApprovalStatus::deserialize(
        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(word.as_str()),
    )
    .ok())
}

/// Lenient decoder for `rc:hive.start_ack`'s `refused`. Only an absent or
/// `null` value means accepted; anything present that is not a known word —
/// an unknown string, a number, an object from some future shape — is
/// [`HiveRefusal::Other`].
pub(crate) fn refusal_lenient<'de, D>(de: D) -> Result<Option<HiveRefusal>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(de)? else {
        return Ok(None);
    };
    let serde_json::Value::String(word) = raw else {
        return Ok(Some(HiveRefusal::Other));
    };
    // Re-parse through the derive so the spellings live in exactly one place.
    Ok(Some(
        HiveRefusal::deserialize(
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(word.as_str()),
        )
        .unwrap_or(HiveRefusal::Other),
    ))
}

/// Lenient decoder for `rc:hive.view.grant_ack`'s `refused`, as
/// [`refusal_lenient`]: only absent or `null` lets the browser dial.
pub(crate) fn view_refusal_lenient<'de, D>(de: D) -> Result<Option<HiveViewRefusal>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(de)? else {
        return Ok(None);
    };
    let serde_json::Value::String(word) = raw else {
        return Ok(Some(HiveViewRefusal::Other));
    };
    Ok(Some(
        HiveViewRefusal::deserialize(
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(word.as_str()),
        )
        .unwrap_or(HiveViewRefusal::Other),
    ))
}

/// Lenient decoder for `rc:hive.state`'s `state`: a state this build cannot
/// name is `None` — the server keeps what it knew and logs the word — rather
/// than a hard error that would drop the frame and every later one shaped
/// like it.
pub(crate) fn run_state_lenient<'de, D>(de: D) -> Result<Option<HiveRunState>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(de)? else {
        return Ok(None);
    };
    let serde_json::Value::String(word) = raw else {
        return Ok(None);
    };
    Ok(HiveRunState::deserialize(
        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(word.as_str()),
    )
    .ok())
}

/// Lenient decoder for `rc:hive.turn`'s `status`, as [`run_state_lenient`]:
/// a status this build cannot name is `None`, and the stub keeps what it said.
pub(crate) fn turn_status_lenient<'de, D>(de: D) -> Result<Option<HiveTurnStatus>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(de)? else {
        return Ok(None);
    };
    let serde_json::Value::String(word) = raw else {
        return Ok(None);
    };
    Ok(HiveTurnStatus::deserialize(
        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(word.as_str()),
    )
    .ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WIRE LOCK — the approval words, and a newer device's word is unnamed,
    /// never an error that drops the frame.
    #[test]
    fn approval_status_words_are_locked_and_unknown_ones_are_unnamed() {
        let words: Vec<&str> = HiveApprovalStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(words, ["open", "allowed", "denied", "expired", "withdrawn"]);
        for s in HiveApprovalStatus::ALL {
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::Value::String(s.as_str().into())
            );
        }
        #[derive(Deserialize)]
        struct A {
            #[serde(default, deserialize_with = "approval_status_lenient")]
            status: Option<HiveApprovalStatus>,
        }
        let a = |j: &str| serde_json::from_str::<A>(j).unwrap().status;
        assert_eq!(a(r#"{"status":"open"}"#), Some(HiveApprovalStatus::Open));
        assert_eq!(a(r#"{"status":"delegated"}"#), None);
        assert_eq!(a(r#"{"status":1}"#), None);
        assert_eq!(a("{}"), None);
    }

    #[test]
    fn turn_status_words_are_locked_and_unknown_ones_are_unnamed() {
        for (s, w) in [
            (HiveTurnStatus::Running, "running"),
            (HiveTurnStatus::Ok, "ok"),
            (HiveTurnStatus::Error, "error"),
            (HiveTurnStatus::Interrupted, "interrupted"),
        ] {
            assert_eq!(s.as_str(), w);
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(w));
        }
        #[derive(Deserialize)]
        struct T {
            #[serde(default, deserialize_with = "turn_status_lenient")]
            status: Option<HiveTurnStatus>,
        }
        let t = |j: &str| serde_json::from_str::<T>(j).unwrap().status;
        assert_eq!(t(r#"{"status":"ok"}"#), Some(HiveTurnStatus::Ok));
        assert_eq!(t(r#"{"status":"paused"}"#), None);
        assert_eq!(t("{}"), None);
    }

    /// WIRE LOCK — what a device sends and what the record stores. Spelled
    /// out literally so a rename has to be a deliberate edit here.
    #[test]
    fn refusal_words_are_locked_and_match_serde() {
        let words: Vec<&str> = HiveRefusal::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            words,
            [
                "hive_disabled",
                "no_account",
                "no_console_user",
                "folder_not_allowed",
                "harness_missing",
                "launch_failed",
                "at_capacity",
                "passwordless_sudo",
                "other",
            ]
        );
        for r in HiveRefusal::ALL {
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                serde_json::Value::String(r.as_str().into()),
                "{r:?}"
            );
        }
    }

    #[test]
    fn run_state_words_are_locked_and_match_serde() {
        for (s, w) in [
            (HiveRunState::Idle, "idle"),
            (HiveRunState::Running, "running"),
            (HiveRunState::AwaitingApproval, "awaiting_approval"),
            (HiveRunState::Ended, "ended"),
        ] {
            assert_eq!(s.as_str(), w);
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(w));
        }
    }

    #[derive(Deserialize)]
    struct Ack {
        #[serde(default, deserialize_with = "refusal_lenient")]
        refused: Option<HiveRefusal>,
    }

    fn ack(json: &str) -> Option<HiveRefusal> {
        serde_json::from_str::<Ack>(json)
            .expect("an ack decodes")
            .refused
    }

    /// The direction of the fallback is the point: anything PRESENT is a
    /// refusal, and only absence or `null` is acceptance.
    #[test]
    fn an_unknown_refusal_is_still_a_refusal() {
        assert_eq!(ack("{}"), None);
        assert_eq!(ack(r#"{"refused":null}"#), None);
        assert_eq!(
            ack(r#"{"refused":"no_account"}"#),
            Some(HiveRefusal::NoAccount)
        );
        assert_eq!(
            ack(r#"{"refused":"quota_exhausted"}"#),
            Some(HiveRefusal::Other),
            "a newer device's word"
        );
        assert_eq!(ack(r#"{"refused":7}"#), Some(HiveRefusal::Other));
        assert_eq!(ack(r#"{"refused":{"kind":"x"}}"#), Some(HiveRefusal::Other));
    }

    /// WIRE LOCK for the view refusals, and the fallback's direction: an
    /// unknown word still refuses — the browser must never be told to dial on
    /// a word nobody here understood.
    #[test]
    fn view_refusals_are_locked_and_an_unknown_one_still_refuses() {
        let words: Vec<&str> = HiveViewRefusal::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            words,
            ["hive_disabled", "no_session", "at_capacity", "other"]
        );
        for r in HiveViewRefusal::ALL {
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                serde_json::Value::String(r.as_str().into())
            );
        }
        #[derive(Deserialize)]
        struct V {
            #[serde(default, deserialize_with = "view_refusal_lenient")]
            refused: Option<HiveViewRefusal>,
        }
        let v = |j: &str| serde_json::from_str::<V>(j).unwrap().refused;
        assert_eq!(v("{}"), None);
        assert_eq!(v(r#"{"refused":null}"#), None);
        assert_eq!(
            v(r#"{"refused":"no_session"}"#),
            Some(HiveViewRefusal::NoSession)
        );
        assert_eq!(
            v(r#"{"refused":"viewer_banned"}"#),
            Some(HiveViewRefusal::Other)
        );
        assert_eq!(v(r#"{"refused":false}"#), Some(HiveViewRefusal::Other));
    }

    /// FR-90 P1j — WIRE LOCK for the adopt refusals, and the fallback's
    /// direction: an unknown word still refuses, so a device never mirrors a
    /// terminal into a session the server did not record.
    #[test]
    fn adopt_refusals_are_locked_and_an_unknown_one_still_refuses() {
        let words: Vec<&str> = HiveAdoptRefusal::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            words,
            [
                "hive_not_enabled",
                "no_account",
                "ambiguous_account",
                "not_a_member",
                "at_capacity",
                "rate_limited",
                "other"
            ]
        );
        for r in HiveAdoptRefusal::ALL {
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                serde_json::Value::String(r.as_str().into())
            );
        }
        #[derive(Deserialize)]
        struct A {
            #[serde(default, deserialize_with = "adopt_refusal_lenient")]
            refused: Option<HiveAdoptRefusal>,
        }
        let a = |j: &str| serde_json::from_str::<A>(j).unwrap().refused;
        assert_eq!(a("{}"), None);
        assert_eq!(a(r#"{"refused":null}"#), None);
        assert_eq!(
            a(r#"{"refused":"ambiguous_account"}"#),
            Some(HiveAdoptRefusal::AmbiguousAccount)
        );
        assert_eq!(
            a(r#"{"refused":"terminal_banned"}"#),
            Some(HiveAdoptRefusal::Other)
        );
        assert_eq!(a(r#"{"refused":0}"#), Some(HiveAdoptRefusal::Other));
    }

    #[derive(Deserialize)]
    struct State {
        #[serde(default, deserialize_with = "run_state_lenient")]
        state: Option<HiveRunState>,
    }

    #[test]
    fn an_unknown_run_state_is_unnamed_not_an_error() {
        let s = |j: &str| serde_json::from_str::<State>(j).expect("decodes").state;
        assert_eq!(s(r#"{"state":"running"}"#), Some(HiveRunState::Running));
        assert_eq!(s(r#"{"state":"compacting"}"#), None);
        assert_eq!(s(r#"{"state":3}"#), None);
        assert_eq!(s("{}"), None);
    }
}
