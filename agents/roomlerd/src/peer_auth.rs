// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-100 P1a — this device's identity key, one per org enrollment
//! (`docs/fr/FR-100-controller-identity.md` §3a).
//!
//! An ECDSA P-256 key made HERE, on first use, and kept in its own file beside
//! the config: `<config dir>/peer-auth/<tenant id>.json`. Never in
//! `config.toml`, which has several writers and is reported to the server
//! (FR-92's state rule). Only the public half ever leaves the device, on the
//! hello: it is the key a controller's browser will pin (P2) and the key the
//! device will sign its session answers with (P1c).
//!
//! One key per enrollment, never shared between them: a key two orgs both saw
//! would let them correlate the device (`docs/multi-org.md`, the per-org WG
//! key rule).
//!
//! **A key that cannot be kept is never offered.** A key made in memory and
//! lost at the next start would break every browser's pin at every restart,
//! and teach people to click through the warning. So when the key cannot be
//! written, or read back as written, the hello carries none, and the device
//! reads as "cannot prove who it is". That is the SSH host key's rule
//! (FR-52 §2b).
//!
//! ⚠️ The file holds a PRIVATE key. Who may change the folder, by how the
//! daemon runs ([`Keeping`]):
//!
//! | Run | `peer-auth` |
//! |---|---|
//! | Unix | made 0700, its files 0600, whoever runs the daemon |
//! | Windows, the machine config (the SYSTEM service) | `machine_tree` seals and checks it: SYSTEM and Administrators |
//! | Windows, elevated or SYSTEM, any other config | sealed here to SYSTEM and Administrators, owner Administrators, read and written by handle |
//! | Windows, neither elevated nor SYSTEM (a per-user install) | the person's own profile, as it is: nothing is sealed |
//!
//! Where it is sealed, a file another account can change is not trusted: a
//! key file is not offered, and a browser-key file is not read. The first
//! seal keeps a key file only when Administrators or SYSTEM own it and its
//! folder; anything else is moved aside, never deleted, and a new key is
//! made. `ROOMLERD_PEER_AUTH_SEAL=0` in the service's `Environment` value
//! turns the seal off.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde::{Deserialize, Serialize};

/// The directory beside the config that holds one key file per enrollment.
pub const DIR_NAME: &str = "peer-auth";
const VERSION: u32 = 1;
/// ECDSA over P-256 with SHA-256, signatures as the fixed-size `r ‖ s` that
/// WebCrypto verifies.
const ALG: &str = "ecdsa-p256-sha256";

/// The file. A newer agent may write a newer `version`; an older one leaves
/// such a file alone rather than replace a key it cannot read.
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    alg: String,
    /// PKCS#8 v1, standard base64, as ring makes and reads it.
    pkcs8: String,
}

/// One enrollment's device key.
pub struct DeviceKey {
    pair: EcdsaKeyPair,
}

impl DeviceKey {
    /// The public half as the hello carries it: the standard base64 of the
    /// uncompressed point (`roomler_ai_remote_control::signaling::
    /// peer_auth_key_is_well_formed`).
    pub fn public_b64(&self) -> String {
        STANDARD.encode(self.pair.public_key().as_ref())
    }

    /// FR-100 P1b — `message` signed: ECDSA P-256 over SHA-256, the fixed
    /// 64-byte `r ‖ s` that WebCrypto verifies. `None` only if the system's
    /// randomness fails, and then nothing is signed.
    pub fn sign(&self, message: &[u8]) -> Option<Vec<u8>> {
        self.pair
            .sign(&SystemRandom::new(), message)
            .ok()
            .map(|sig| sig.as_ref().to_vec())
    }
}

/// FR-100 P1b — what the daemon needs to sign a session's answer, gathered as
/// the session's frames pass through it: the controller from `rc:request`,
/// then the offer's DTLS fingerprint and nonce from `rc:sdp.offer`.
///
/// Kept in the DAEMON, and held by `RemoteConfigServices` (one per daemon,
/// cloned into every org's loop), not in a static, so the many agents an
/// integration test runs in one process never see each other's sessions. The
/// daemon is also where the key is: a GUI worker that built the answer
/// (FR-43) never holds it, so the daemon signs what it relays.
#[derive(Default)]
pub struct PendingOffers {
    table: Mutex<Table>,
}

#[derive(Default)]
struct Table {
    sessions: HashMap<bson::oid::ObjectId, Pending>,
    /// Noting order, for evicting the oldest past the cap: a clock read in a
    /// tight loop can tie, an increment cannot.
    next_seq: u64,
}

struct Pending {
    user_id: bson::oid::ObjectId,
    /// `(offer fingerprint, nonce)` while an offer that asked is unanswered.
    asked: Option<(String, String)>,
    /// FR-100 P1c — what the device concluded about the browser that made the
    /// current offer, until its answer reports it. `None` before the offer is
    /// judged, and under `peer_auth = off`.
    verdict: Option<Verdict>,
    noted_at: std::time::Instant,
    seq: u64,
}

/// Sessions noted at once. A session that is never answered or ended is
/// forgotten by age or by this cap, so the table cannot grow without bound.
const MAX_PENDING: usize = 64;
const PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(600);

impl PendingOffers {
    /// An in-org session's request reached this daemon: remember who asked.
    /// An external session is never noted: FR-52's own MAC binds it.
    pub fn note_request(&self, session_id: bson::oid::ObjectId, user_id: bson::oid::ObjectId) {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        table
            .sessions
            .retain(|_, p| now.duration_since(p.noted_at) < PENDING_TTL);
        if table.sessions.len() >= MAX_PENDING
            && !table.sessions.contains_key(&session_id)
            && let Some(oldest) = table
                .sessions
                .iter()
                .min_by_key(|(_, p)| p.seq)
                .map(|(id, _)| *id)
        {
            table.sessions.remove(&oldest);
        }
        let seq = table.next_seq;
        table.next_seq += 1;
        table.sessions.insert(
            session_id,
            Pending {
                user_id,
                asked: None,
                verdict: None,
                noted_at: now,
                seq,
            },
        );
    }

    /// The session's offer reached this daemon. One that asks (a nonce) is
    /// remembered with its fingerprint, to sign the answer against; one that
    /// does not ask clears an earlier ask, so a re-offer that did not ask is
    /// never answered with a signature made for another offer.
    pub fn note_offer(
        &self,
        session_id: bson::oid::ObjectId,
        offer_sdp: &str,
        ask: Option<&roomler_ai_remote_control::signaling::PeerAuthOffer>,
    ) {
        use roomler_ai_remote_control::signaling::{
            peer_auth_nonce_is_well_formed, sdp_dtls_fingerprint,
        };
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pending) = table.sessions.get_mut(&session_id) else {
            if ask.is_some() {
                tracing::info!(%session_id, "peer-auth: an offer asked for a signed answer, but no in-org request for it reached this process; it will be answered unsigned");
            }
            return;
        };
        pending.asked = None;
        // A verdict belongs to one offer: a re-offer is judged afresh.
        pending.verdict = None;
        let Some(ask) = ask else {
            return;
        };
        if !peer_auth_nonce_is_well_formed(&ask.nonce) {
            tracing::warn!(%session_id, len = ask.nonce.len(), "peer-auth: the offer's nonce is not well formed; its answer goes unsigned");
            return;
        }
        match sdp_dtls_fingerprint(offer_sdp) {
            Ok(fingerprint) => pending.asked = Some((fingerprint, ask.nonce.clone())),
            Err(why) => {
                tracing::warn!(%session_id, ?why, "peer-auth: the offer names no single DTLS fingerprint; its answer goes unsigned");
            }
        }
    }

    /// The session ended: nothing more to sign for it.
    pub fn forget(&self, session_id: bson::oid::ObjectId) {
        self.table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .remove(&session_id);
    }

    /// What signing this offer's answer needs, consumed: one answer per
    /// offer. The controller stays noted for a re-offer.
    fn take(&self, session_id: bson::oid::ObjectId) -> Option<Asked> {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let pending = table.sessions.get_mut(&session_id)?;
        let (offer_fingerprint, nonce) = pending.asked.take()?;
        Some(Asked {
            user_id: pending.user_id,
            offer_fingerprint,
            nonce,
        })
    }

    /// FR-100 P1c — judge the browser behind `session_id`'s offer under
    /// `check`, and keep the verdict for the answer to report. Only for an
    /// in-org session this daemon noted at its request (an outsider's is
    /// judged by FR-52's MAC, at the answer). `None` under `peer_auth = off`.
    ///
    /// Called right after [`Self::note_offer`], in the daemon and ahead of
    /// any delegation, for the reason `note_offer` is.
    pub fn judge_offer(
        &self,
        session_id: bson::oid::ObjectId,
        offer_sdp: &str,
        ask: Option<&roomler_ai_remote_control::signaling::PeerAuthOffer>,
        check: &OfferCheck<'_>,
    ) -> Option<Verdict> {
        let user_id = {
            let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
            table.sessions.get(&session_id)?.user_id
        };
        let verdict = match check.policy {
            PeerAuthPolicy::Off => None,
            PeerAuthPolicy::Audit => Some(judge(session_id, user_id, offer_sdp, ask, check)),
        };
        if let Some(v) = verdict {
            tracing::info!(%session_id, verdict = v.wire(), "peer-auth: the browser behind this offer");
        }
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = table.sessions.get_mut(&session_id) {
            pending.verdict = verdict;
        }
        verdict
    }

    /// FR-100 P1c — the verdict on `session_id`'s current offer, consumed: one
    /// report per offer, as one signature per offer.
    fn take_verdict(&self, session_id: bson::oid::ObjectId) -> Option<Verdict> {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.sessions.get_mut(&session_id)?.verdict.take()
    }
}

/// An offer that asked for a signed answer, taken from the table to sign.
struct Asked {
    user_id: bson::oid::ObjectId,
    offer_fingerprint: String,
    nonce: String,
}

/// FR-100 P1b — the device's signature for `session_id`'s answer, when its
/// offer asked for one and this enrollment has a key it keeps: the transcript
/// (`peer_auth_transcript`, role `answer`) signed with the device key. `None`
/// otherwise, and the answer goes out unsigned, which is exactly what an offer
/// that did not ask expects.
pub fn sign_answer(
    offers: &PendingOffers,
    config_path: &Path,
    tenant_id: &str,
    agent_id: &str,
    session_id: bson::oid::ObjectId,
    answer_sdp: &str,
) -> Option<roomler_ai_remote_control::signaling::PeerAuthAnswer> {
    let asked = offers.take(session_id)?;
    let signed = signed_answer(
        config_path,
        tenant_id,
        agent_id,
        session_id,
        &asked,
        answer_sdp,
    );
    match signed {
        Ok(answer) => {
            tracing::info!(%session_id, "peer-auth: answer signed with this enrollment's device key");
            Some(answer)
        }
        Err(why) => {
            tracing::warn!(%session_id, %why, "peer-auth: the offer asked for a signed answer, but it goes unsigned");
            None
        }
    }
}

/// [`sign_answer`]'s work, with the reason it could not be done.
fn signed_answer(
    config_path: &Path,
    tenant_id: &str,
    agent_id: &str,
    session_id: bson::oid::ObjectId,
    asked: &Asked,
    answer_sdp: &str,
) -> Result<roomler_ai_remote_control::signaling::PeerAuthAnswer, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use roomler_ai_remote_control::signaling::{
        PeerAuthAnswer, peer_auth_transcript, sdp_dtls_fingerprint,
    };
    let answer_fingerprint = sdp_dtls_fingerprint(answer_sdp)
        .map_err(|why| format!("the answer names no single DTLS fingerprint ({why:?})"))?;
    let transcript = peer_auth_transcript(
        "answer",
        &session_id.to_hex(),
        agent_id,
        &asked.user_id.to_hex(),
        &asked.offer_fingerprint,
        &answer_fingerprint,
        &asked.nonce,
    )
    .ok_or("a transcript field is too long to frame")?;
    let key = device_key(config_path, tenant_id)?;
    let sig = key.sign(&transcript).ok_or("signing failed")?;
    Ok(PeerAuthAnswer {
        key: key.public_b64(),
        sig: URL_SAFE_NO_PAD.encode(sig),
    })
}

/// FR-100 P1b — sign `msg` in place if it is an answer: the choke point for
/// answers this process did not build itself (a GUI worker's, relayed up by
/// the delegation host). Whatever the worker put in `peer_auth` is not the
/// daemon's to vouch for, so it is replaced by the daemon's own signature, or
/// by none. FR-100 P1c — the same for `peer_auth_verdict`: the daemon judged
/// the offer before delegating it, and a worker holds no pins to judge with.
/// Run after FR-52's sealing, which is what marks an outsider's answer.
pub fn sign_outbound(
    offers: &PendingOffers,
    config_path: &Path,
    tenant_id: &str,
    agent_id: &str,
    policy: PeerAuthPolicy,
    msg: &mut roomler_ai_remote_control::signaling::ClientMsg,
) {
    if let roomler_ai_remote_control::signaling::ClientMsg::SdpAnswer {
        session_id,
        sdp,
        extauth_mac,
        peer_auth,
        peer_auth_verdict,
    } = msg
    {
        *peer_auth = sign_answer(offers, config_path, tenant_id, agent_id, *session_id, sdp);
        *peer_auth_verdict = answer_verdict(offers, policy, *session_id, extauth_mac.is_some());
    }
}

/// FR-100 P1c — the verdict `session_id`'s answer reports, as the wire spells
/// it. An outsider's session (its answer sealed under FR-52's login key) is
/// `verified`: FR-52 admitted it on a MAC the device checked, and refused it
/// otherwise. An in-org session reports what [`PendingOffers::judge_offer`]
/// found. Nothing under `peer_auth = off`.
pub fn answer_verdict(
    offers: &PendingOffers,
    policy: PeerAuthPolicy,
    session_id: bson::oid::ObjectId,
    external: bool,
) -> Option<String> {
    if policy == PeerAuthPolicy::Off {
        return None;
    }
    let verdict = if external {
        Some(Verdict::Verified)
    } else {
        offers.take_verdict(session_id)
    };
    verdict.map(|v| v.wire().to_string())
}

// ─── FR-100 P1c — the device checks the browser ─────────────────────────────

pub use crate::config::PeerAuthPolicy;

/// FR-100 P1c — what the device concluded about the browser that offered a
/// session. The words the answer carries are
/// [`roomler_ai_remote_control::signaling::PEER_AUTH_VERDICTS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The offer carried no browser signature.
    Absent,
    /// A signature that verifies, by a key not yet seen for this user. Under
    /// `audit` it is now remembered, so the user's next session from that
    /// browser reads `Verified`.
    Unknown,
    /// A signature that verifies, by a key remembered for this user; or an
    /// outsider admitted by FR-52's MAC.
    Verified,
    /// A signature that does not verify over this session, or an offer that
    /// names no single certificate for one to cover.
    Mismatch,
}

impl Verdict {
    /// The word on the wire and in the audit.
    pub fn wire(self) -> &'static str {
        match self {
            Verdict::Absent => "absent",
            Verdict::Unknown => "unknown",
            Verdict::Verified => "verified",
            Verdict::Mismatch => "mismatch",
        }
    }
}

/// FR-100 P1c — what judging an offer needs beyond the offer: the device's
/// policy at that moment, and the enrollment whose pins it reads.
pub struct OfferCheck<'a> {
    pub policy: PeerAuthPolicy,
    pub config_path: &'a Path,
    pub tenant_id: &'a str,
    pub agent_id: &'a str,
}

/// The verdict on one offer: verify the browser's signature over the
/// transcript (role `offer`, an EMPTY answer fingerprint), then look its key
/// up among the keys this enrollment has seen for the user, remembering it if
/// new. A key that cannot be remembered still reads `Unknown`: the signature
/// held, but nothing vouches for the key yet.
fn judge(
    session_id: bson::oid::ObjectId,
    user_id: bson::oid::ObjectId,
    offer_sdp: &str,
    ask: Option<&roomler_ai_remote_control::signaling::PeerAuthOffer>,
    check: &OfferCheck<'_>,
) -> Verdict {
    use roomler_ai_remote_control::signaling::{
        peer_auth_nonce_is_well_formed, peer_auth_transcript, peer_auth_verify,
        sdp_dtls_fingerprint,
    };
    let Some((key, sig, nonce)) =
        ask.and_then(|a| Some((a.key.as_deref()?, a.sig.as_deref()?, a.nonce.as_str())))
    else {
        return Verdict::Absent;
    };
    if !peer_auth_nonce_is_well_formed(nonce) {
        return Verdict::Mismatch;
    }
    let Ok(offer_fingerprint) = sdp_dtls_fingerprint(offer_sdp) else {
        return Verdict::Mismatch;
    };
    let Some(transcript) = peer_auth_transcript(
        "offer",
        &session_id.to_hex(),
        check.agent_id,
        &user_id.to_hex(),
        &offer_fingerprint,
        "",
        nonce,
    ) else {
        return Verdict::Mismatch;
    };
    if !peer_auth_verify(key, &transcript, sig) {
        return Verdict::Mismatch;
    }
    match remember(check.config_path, check.tenant_id, user_id, key) {
        Ok(Pinned::Before) => Verdict::Verified,
        // Refused: this folder keeps no browser keys, said once where that
        // was found.
        Ok(Pinned::Now | Pinned::Refused) => Verdict::Unknown,
        Err(why) => {
            tracing::warn!(%session_id, %why, "peer-auth: a browser key that cannot be remembered");
            Verdict::Unknown
        }
    }
}

/// FR-100 P1c — the browser keys this enrollment has seen, per user: what
/// `audit` remembers on first sight, so that `enforce` (P3) asks only about a
/// browser the device has never met. Public keys only; what matters is that
/// the file is the device's own memory, so it lives beside the device key, in
/// the same directory and with the same protections.
#[derive(Serialize, Deserialize, Clone, Default)]
struct PinFile {
    version: u32,
    /// The user's id (lowercase hex) → the keys seen for that user, oldest
    /// first.
    users: std::collections::BTreeMap<String, Vec<PinnedKey>>,
}

#[derive(Serialize, Deserialize, Clone)]
struct PinnedKey {
    /// As the offer carried it: the device key's shape, standard base64 of the
    /// 65-byte point.
    key: String,
    /// When the device first saw it, RFC 3339.
    first_seen: String,
}

/// A person with more browsers than this loses the oldest one's pin first.
const MAX_KEYS_PER_USER: usize = 16;
/// The file stays small on a device a whole org connects to: past this, the
/// user whose newest key is oldest is forgotten first.
const MAX_USERS: usize = 256;

/// Where `tenant_id`'s pins live: `<config dir>/peer-auth/<tenant id>.controllers.json`,
/// beside its key and under the same naming rule.
fn pins_path(config_path: &Path, tenant_id: &str) -> Result<PathBuf, String> {
    let key = key_path(config_path, tenant_id)?;
    Ok(key.with_file_name(format!(
        "{}.controllers.json",
        tenant_id.to_ascii_lowercase()
    )))
}

/// What [`remember`] found of a browser key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pinned {
    /// Seen before for this user: `verified`.
    Before,
    /// New, and now remembered: `unknown`.
    Now,
    /// This folder keeps no browser keys (one that cannot be sealed), which
    /// was said once where that was found: `unknown`, and nothing written.
    Refused,
}

/// Has `user_id` offered with `key` before, on this enrollment? Remembered now
/// if not ([`Pinned::Now`]). Read once per process and kept in memory; a new
/// key is written through, whole or not at all, BEFORE it counts as
/// remembered.
fn remember(
    config_path: &Path,
    tenant_id: &str,
    user_id: bson::oid::ObjectId,
    key: &str,
) -> Result<Pinned, String> {
    static PINS: OnceLock<Mutex<HashMap<PathBuf, PinFile>>> = OnceLock::new();
    let path = pins_path(config_path, tenant_id)?;
    let folder = Folder::of(config_path, keeping_for(config_path))?;
    if folder.keeps_no_pins()? {
        return Ok(Pinned::Refused);
    }
    let name = file_name(&path);
    let mut all = PINS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let file = match all.entry(path.clone()) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => e.insert(load_pins(&folder, &name)?),
    };
    let user = user_id.to_hex();
    if file
        .users
        .get(&user)
        .is_some_and(|keys| keys.iter().any(|k| k.key == key))
    {
        return Ok(Pinned::Before);
    }
    let mut next = file.clone();
    next.version = VERSION;
    let keys = next.users.entry(user).or_default();
    keys.push(PinnedKey {
        key: key.to_string(),
        first_seen: bson::DateTime::now()
            .try_to_rfc3339_string()
            .unwrap_or_default(),
    });
    if keys.len() > MAX_KEYS_PER_USER {
        let excess = keys.len() - MAX_KEYS_PER_USER;
        keys.drain(..excess);
    }
    while next.users.len() > MAX_USERS {
        let Some(stalest) = next
            .users
            .iter()
            .min_by(|a, b| newest(a.1).cmp(newest(b.1)))
            .map(|(u, _)| u.clone())
        else {
            break;
        };
        next.users.remove(&stalest);
    }
    let bytes = serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?;
    folder.write(&name, &bytes)?;
    *file = next;
    Ok(Pinned::Now)
}

/// The newest `first_seen` among `keys`: RFC 3339 in UTC sorts as text.
fn newest(keys: &[PinnedKey]) -> &str {
    keys.iter()
        .map(|k| k.first_seen.as_str())
        .max()
        .unwrap_or("")
}

/// The pins in `folder`'s file `name`. None yet is an empty set. A file that
/// is not a pin file starts over (it is only ever a cache of first sightings,
/// and the verdicts say so as `unknown` again); a NEWER version is left
/// alone, and nothing is remembered on top of it. Where the folder is sealed,
/// a file another account can change is not read: the set starts over, said
/// once, and the next write moves that file aside before it writes.
fn load_pins(folder: &Folder, name: &str) -> Result<PinFile, String> {
    let path = folder.dir().join(name);
    let path = path.as_path();
    match folder.read(name, PINS_MAX)? {
        Found::Absent => Ok(PinFile::default()),
        Found::TooLarge => {
            tracing::warn!(path = %path.display(), "peer-auth: the browser-key file is too large to be one; starting it over");
            Ok(PinFile::default())
        }
        Found::Refused(why) => {
            if first_time(format!("pins-refused {} {why}", path.display())) {
                tracing::warn!(
                    path = %path.display(), reason = %why,
                    "peer-auth: the browser-key file is not read, so each browser reads \
                     `unknown` until this device has seen it again"
                );
            }
            Ok(PinFile::default())
        }
        Found::Bytes(bytes) => {
            #[derive(Deserialize)]
            struct Header {
                version: u32,
            }
            match serde_json::from_slice::<Header>(&bytes) {
                Ok(h) if h.version != VERSION => Err(format!(
                    "{} is a pin file this build does not read (version {}); left alone",
                    path.display(),
                    h.version
                )),
                Ok(_) => serde_json::from_slice(&bytes).or_else(|e| {
                    tracing::warn!(path = %path.display(), %e, "peer-auth: the browser-key file is unreadable; starting it over");
                    Ok(PinFile::default())
                }),
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "peer-auth: the browser-key file is unreadable; starting it over");
                    Ok(PinFile::default())
                }
            }
        }
    }
}

/// What reading a key file found.
enum Read {
    /// Boxed: a key pair is ~240 bytes, the other two a `String` each.
    Key(Box<DeviceKey>),
    /// Not a key file at all (truncated, not JSON, a PKCS#8 ring refuses):
    /// nothing in it is worth keeping, so a new key replaces it.
    Corrupt(String),
    /// A key file this build does not understand (a newer version or
    /// algorithm). Left alone: replacing it would destroy a newer agent's key
    /// after a rollback.
    Foreign(String),
}

/// The public half of `tenant_id`'s device key for the hello, made on first
/// use. Empty when this enrollment has no key it can keep (see the module
/// docs), which the hello reports as "cannot prove who it is".
pub fn hello_key(config_path: &Path, tenant_id: &str) -> String {
    // FR-43 P3a — a GUI worker with no enrollment says hello to no one, and the
    // root signs its answers.
    if tenant_id.is_empty() {
        return String::new();
    }
    match device_key(config_path, tenant_id) {
        Ok(key) => key.public_b64(),
        Err(why) => {
            // Once per reason: every reconnect says hello again.
            if first_time(format!("no-key {tenant_id} {why}")) {
                tracing::warn!(tenant = %tenant_id, %why, "peer-auth: no device key this run, so the hello offers none");
            }
            String::new()
        }
    }
}

/// `tenant_id`'s device key: read from its file, or made and written there.
/// Kept in memory for the life of the process once it is on disk, so every
/// hello of one run offers the same key.
pub fn device_key(config_path: &Path, tenant_id: &str) -> Result<Arc<DeviceKey>, String> {
    static KEYS: OnceLock<Mutex<HashMap<PathBuf, Arc<DeviceKey>>>> = OnceLock::new();
    let path = key_path(config_path, tenant_id)?;
    let mut keys = KEYS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(key) = keys.get(&path) {
        return Ok(key.clone());
    }
    // Read, or made and written, under the lock: two callers for one
    // enrollment can never each write a different key, leaving one of them
    // offering a key that is no longer on disk.
    let folder = Folder::of(config_path, keeping_for(config_path))?;
    let key = Arc::new(load_or_make(&folder, &file_name(&path))?);
    keys.insert(path, key.clone());
    Ok(key)
}

/// The last component of a path [`key_path`] or [`pins_path`] made.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Where `tenant_id`'s key lives: `<config dir>/peer-auth/<tenant id>.json`.
/// The tenant id names a file, so only an object id's 24 hex digits may.
///
/// ⚠️ A config path with no directory part (a bare `config.toml`) gets no
/// key: its directory would be whatever the process's working directory
/// happens to be, which is no place for a private key. The daemon resolves
/// an absolute path; the test harnesses pass a bare name they never read.
fn key_path(config_path: &Path, tenant_id: &str) -> Result<PathBuf, String> {
    if tenant_id.len() != 24 || !tenant_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("tenant id {tenant_id:?} is not an object id"));
    }
    let dir = config_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| format!("config path {} has no directory", config_path.display()))?;
    Ok(dir
        .join(DIR_NAME)
        .join(format!("{}.json", tenant_id.to_ascii_lowercase())))
}

/// Read the key file `name` of `folder`, or make a key and write it there. A
/// key is returned only once it is on disk as the next start will read it.
fn load_or_make(folder: &Folder, name: &str) -> Result<DeviceKey, String> {
    let path = folder.dir().join(name);
    let path = path.as_path();
    // Present but unreadable (permissions, I/O): `?`, never overwriting a
    // file that may hold a good key this process merely cannot open.
    match folder.read(name, KEY_MAX)? {
        Found::Bytes(bytes) => match parse(&bytes) {
            Read::Key(key) => return Ok(*key),
            Read::Foreign(why) => {
                return Err(format!(
                    "{} is a key file this build does not read ({why}); left alone",
                    path.display()
                ));
            }
            Read::Corrupt(why) => tracing::warn!(
                path = %path.display(), %why,
                "peer-auth: the device key file is unreadable, so a new key replaces it; \
                 a browser that pinned the old one will warn"
            ),
        },
        Found::TooLarge => tracing::warn!(
            path = %path.display(),
            "peer-auth: the device key file is too large to be one, so a new key replaces it; \
             a browser that pinned the old one will warn"
        ),
        Found::Absent => {}
        // Sealed, and another account can change it: not offered, and left
        // as it is (a key that cannot be kept is never offered).
        Found::Refused(why) => {
            return Err(format!(
                "{} is not used, and is left as it is: {why}",
                path.display()
            ));
        }
    }
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
        .map_err(|_| "making a P-256 key failed".to_string())?;
    let stored = Stored {
        version: VERSION,
        alg: ALG.to_string(),
        pkcs8: STANDARD.encode(pkcs8.as_ref()),
    };
    let bytes = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
    folder.write(name, &bytes)?;
    let written = match folder.read(name, KEY_MAX) {
        Ok(Found::Bytes(written)) => written,
        Ok(Found::Absent | Found::TooLarge) => {
            return Err(format!("{} did not read back as written", path.display()));
        }
        Ok(Found::Refused(why)) | Err(why) => {
            return Err(format!("reading back {}: {why}", path.display()));
        }
    };
    match parse(&written) {
        Read::Key(key) => {
            tracing::info!(path = %path.display(), "peer-auth: made this enrollment's device key");
            Ok(*key)
        }
        Read::Corrupt(why) | Read::Foreign(why) => Err(format!(
            "{} did not read back as written: {why}",
            path.display()
        )),
    }
}

fn parse(bytes: &[u8]) -> Read {
    // Version first, from a shape every version shares: a newer file is
    // never mistaken for a corrupt one.
    #[derive(Deserialize)]
    struct Header {
        version: u32,
        #[serde(default)]
        alg: String,
    }
    let header: Header = match serde_json::from_slice(bytes) {
        Ok(h) => h,
        Err(e) => return Read::Corrupt(format!("not a key file: {e}")),
    };
    if header.version != VERSION || header.alg != ALG {
        return Read::Foreign(format!("version {} alg {:?}", header.version, header.alg));
    }
    let stored: Stored = match serde_json::from_slice(bytes) {
        Ok(s) => s,
        Err(e) => return Read::Corrupt(format!("not a version-{VERSION} key file: {e}")),
    };
    let pkcs8 = match STANDARD.decode(stored.pkcs8.trim()) {
        Ok(p) => p,
        Err(e) => return Read::Corrupt(format!("the key is not base64: {e}")),
    };
    match EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &pkcs8,
        &SystemRandom::new(),
    ) {
        Ok(pair) => Read::Key(Box::new(DeviceKey { pair })),
        Err(e) => Read::Corrupt(format!("the key is refused: {e}")),
    }
}

/// Write `bytes` to `path` whole or not at all (a sibling, synced, renamed),
/// by path: [`Keeping::Plain`]'s writer. On Unix readable by the daemon's own
/// identity alone; on Windows with what the folder hands down. Each error
/// keeps its kind, so a run that is not elevated can say why it was denied.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let context = |what: String| {
        move |e: std::io::Error| std::io::Error::new(e.kind(), format!("{what}: {e}"))
    };
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no directory", path.display()),
        )
    })?;
    std::fs::create_dir_all(dir).map_err(context(format!("making {}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(context(format!("restricting {}", dir.display())))?;
    }
    let tmp = path.with_extension("json.tmp");
    // A stale sibling would keep its own mode: `mode` applies only to a file
    // being made.
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .map_err(context(format!("making {}", tmp.display())))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(context(format!("writing {}", tmp.display())))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(context(format!("placing {}", path.display())))?;
    // The rename itself survives a power cut only once the directory is
    // synced. Best effort: the key is already whole on disk either way.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

// ─── Where the folder is kept ───────────────────────────────────────────────

/// A key file is a few hundred bytes; past this it is not one.
const KEY_MAX: u64 = 64 * 1024;
/// 256 users of 16 keys each stay well below this.
const PINS_MAX: u64 = 1024 * 1024;

/// How this process keeps the `peer-auth` folder of a config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keeping {
    /// Read, and written by path, with what the folder hands down ([`Plain`]
    /// says why).
    Plain(Plain),
    /// Windows, elevated or SYSTEM, a config outside the machine folder: the
    /// folder sealed to SYSTEM and Administrators, read and written by handle.
    Sealed,
}

/// Why a folder is kept [`Keeping::Plain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plain {
    /// Unix: the folder is made 0700 and its files 0600.
    NotWindows,
    /// The machine config's folder, which `machine_tree` seals and checks.
    MachineFolder,
    /// Neither elevated nor SYSTEM (a per-user install): this run may not
    /// seal, and changes nothing. A folder an elevated run sealed is closed
    /// to it.
    NotElevated,
    /// `ROOMLERD_PEER_AUTH_SEAL=0` in the service's `Environment` value.
    SwitchedOff,
}

/// The decision, pure. `seal_on` is read only when it decides.
pub(crate) fn keeping(
    windows: bool,
    elevated: bool,
    machine_folder: bool,
    seal_on: impl FnOnce() -> bool,
) -> Keeping {
    if !windows {
        Keeping::Plain(Plain::NotWindows)
    } else if machine_folder {
        Keeping::Plain(Plain::MachineFolder)
    } else if !elevated {
        Keeping::Plain(Plain::NotElevated)
    } else if !seal_on() {
        Keeping::Plain(Plain::SwitchedOff)
    } else {
        Keeping::Sealed
    }
}

/// What [`seal_enabled_in`] reads, after one of `tunnel_core::env::PREFIXES`.
const SEAL_SWITCH: &str = "PEER_AUTH_SEAL";

/// The seal's switch over the service's `Environment` value, read as #3814's
/// companion switch is: under the first of `tunnel_core::env::PREFIXES` it is
/// named with, in any case, as `flag` reads it. A value that could not be read
/// (`None`), or one that does not name it, leaves the seal on. Never the
/// process's own environment or the config: on an attended install both are
/// the signed-in person's to write.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn seal_enabled_in(service_env: Option<&[(String, String)]>) -> bool {
    let value = service_env.and_then(|env| {
        tunnel_core::env::PREFIXES.iter().find_map(|prefix| {
            let name = format!("{prefix}{SEAL_SWITCH}");
            env.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                .map(|(_, v)| v.as_str())
        })
    });
    tunnel_core::env::flag_value(value, true)
}

/// [`keeping`] for this process and `config_path`.
fn keeping_for(config_path: &Path) -> Keeping {
    #[cfg(test)]
    if let Some(kept) = KEPT_AS.with(std::cell::Cell::get) {
        return kept;
    }
    #[cfg(windows)]
    {
        let machine = roomler_node_core::machine_tree::MachineTree::here();
        keeping(
            true,
            process_elevated(),
            config_path.starts_with(machine.folder()),
            service_seal_enabled,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = config_path;
        keeping(false, false, false, || true)
    }
}

#[cfg(test)]
thread_local! {
    /// Tests only: how this thread's test keeps every folder.
    static KEPT_AS: std::cell::Cell<Option<Keeping>> = const { std::cell::Cell::new(None) };
}

#[cfg(windows)]
fn process_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(crate::win_identity::process_is_elevated)
}

/// [`seal_enabled_in`] for this install's service. The service host's
/// environment is fixed from its start, so the first read that succeeds
/// stands for this process's life; a read that fails leaves the seal on, and
/// the next asks again.
#[cfg(windows)]
fn service_seal_enabled() -> bool {
    static READ: OnceLock<bool> = OnceLock::new();
    if let Some(on) = READ.get() {
        return *on;
    }
    match crate::win_service::environment::read_service_env() {
        Ok(env) => *READ.get_or_init(|| seal_enabled_in(Some(&env))),
        Err(_) => seal_enabled_in(None),
    }
}

/// Whether `what` is said for the first time in this process: a line logged
/// once, not at every hello or offer. Bounded: past 256 it starts over.
fn first_time(what: String) -> bool {
    static SAID: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let mut said = SAID
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if said.len() >= 256 {
        said.clear();
    }
    said.insert(what)
}

/// The `peer-auth` folder of one config, as this process keeps it.
struct Folder {
    /// The config's own folder, from which a sealed `peer-auth` is reached.
    base: PathBuf,
    keeping: Keeping,
}

/// What [`Folder::read`] found at a name.
enum Found {
    Absent,
    Bytes(Vec<u8>),
    /// Larger than a file of its kind can be (a sealed read only).
    #[cfg_attr(not(windows), allow(dead_code))]
    TooLarge,
    /// Sealed, and not used: the folder or the file lets an account other
    /// than SYSTEM or Administrators change it, or it is not a plain file.
    #[cfg_attr(not(windows), allow(dead_code))]
    Refused(String),
}

/// [`Keeping`], once this process has tried to seal the folder.
enum Mode {
    Plain(Plain),
    #[cfg(windows)]
    Sealed,
    /// The config's folder cannot be held as itself (a share, a link, a
    /// redirected profile): the key is kept as before, the browser keys not
    /// at all.
    #[cfg(windows)]
    Unsealable,
}

impl Folder {
    fn of(config_path: &Path, keeping: Keeping) -> Result<Self, String> {
        let base = config_path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .ok_or_else(|| format!("config path {} has no directory", config_path.display()))?;
        if keeping == Keeping::Plain(Plain::SwitchedOff)
            && first_time(format!("seal switched off {}", base.display()))
        {
            tracing::warn!(
                path = %base.join(DIR_NAME).display(),
                "peer-auth: ROOMLERD_PEER_AUTH_SEAL=0 in the service's environment, so the \
                 folder is not sealed: it is read and written as before"
            );
        }
        Ok(Self {
            base: base.to_path_buf(),
            keeping,
        })
    }

    fn dir(&self) -> PathBuf {
        self.base.join(DIR_NAME)
    }

    fn mode(&self) -> Result<Mode, String> {
        match self.keeping {
            Keeping::Plain(why) => Ok(Mode::Plain(why)),
            #[cfg(windows)]
            Keeping::Sealed => Ok(Mode::Plain(Plain::MachineFolder)), // RED: not wired yet
            #[cfg(not(windows))]
            Keeping::Sealed => Ok(Mode::Plain(Plain::NotWindows)),
        }
    }

    /// No browser keys are read or kept here: a folder that cannot be sealed.
    fn keeps_no_pins(&self) -> Result<bool, String> {
        let mode = self.mode()?;
        #[cfg(windows)]
        let none = matches!(mode, Mode::Unsealable);
        #[cfg(not(windows))]
        let none = {
            let _ = mode;
            false
        };
        Ok(none)
    }

    /// The file `name`: by handle and judged where the folder is sealed (at
    /// most `max_len` bytes), by path otherwise.
    fn read(&self, name: &str, max_len: u64) -> Result<Found, String> {
        let why = match self.mode()? {
            Mode::Plain(why) => Some(why),
            #[cfg(windows)]
            Mode::Sealed => return sealed::read(&self.base, name, max_len),
            #[cfg(windows)]
            Mode::Unsealable => None,
        };
        let _ = max_len;
        let path = self.dir().join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Found::Bytes(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Found::Absent),
            Err(e) => Err(self
                .denied(&e, why)
                .unwrap_or_else(|| format!("reading {}: {e}", path.display()))),
        }
    }

    /// Write `bytes` as the file `name`: sealed and by handle where the
    /// folder is sealed, by path otherwise.
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let why = match self.mode()? {
            Mode::Plain(why) => Some(why),
            #[cfg(windows)]
            Mode::Sealed => return sealed::write(&self.base, name, bytes),
            #[cfg(windows)]
            Mode::Unsealable => None,
        };
        write_private(&self.dir().join(name), bytes)
            .map_err(|e| self.denied(&e, why).unwrap_or_else(|| e.to_string()))
    }

    /// A run that is neither elevated nor SYSTEM, denied: the cause, and what
    /// to do about it. It deletes nothing.
    fn denied(&self, e: &std::io::Error, why: Option<Plain>) -> Option<String> {
        (why == Some(Plain::NotElevated) && e.kind() == std::io::ErrorKind::PermissionDenied).then(
            || {
                format!(
                    "{} is closed to this run ({e}): an elevated run seals it to SYSTEM and \
                     Administrators, and this run is neither elevated nor SYSTEM. Start \
                     roomlerd elevated again, as its service does; or an administrator moves \
                     that folder away, and a new device key is made, which each browser that \
                     knew this device warns about once",
                    self.dir().display()
                )
            },
        )
    }
}

/// [`Keeping::Sealed`]: the seal, and the reads and writes by handle
/// (`machine_tree::HeldPath`, FR-145's rule).
#[cfg(windows)]
mod sealed {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use roomler_node_core::machine_tree::{
        self, ADMINISTRATORS, HeldPath, OpenFor, Sd, Seal, WRITE_LIKE, Why, trusted, verdict,
    };

    use super::{DIR_NAME, Found, KEY_MAX, Mode, PINS_MAX, first_time};

    /// Per config folder, once this process sealed it: whether it cannot be
    /// sealed here. Its lock is also held by every sealed write, so two never
    /// move or seal the same entry at once.
    fn state() -> MutexGuard<'static, HashMap<PathBuf, bool>> {
        static STATE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
        STATE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The first time this process keeps `base`'s folder, [`seal`] it; after
    /// that, what that found. A folder that cannot be sealed is said once.
    pub(super) fn ensure(base: &Path) -> Result<Mode, String> {
        let mut state = state();
        if let Some(&unsealable) = state.get(base) {
            return Ok(if unsealable {
                Mode::Unsealable
            } else {
                Mode::Sealed
            });
        }
        match seal(base) {
            Ok(_) => {
                state.insert(base.to_path_buf(), false);
                Ok(Mode::Sealed)
            }
            Err(Failure::Unsealable(why)) => {
                tracing::warn!(
                    path = %base.join(DIR_NAME).display(), reason = %why,
                    "peer-auth: the folder cannot be sealed here, so the device key is read and \
                     written as before, and the browser keys are neither read nor kept: each \
                     browser reads `unknown`"
                );
                state.insert(base.to_path_buf(), true);
                Ok(Mode::Unsealable)
            }
            Err(Failure::Failed(why)) => Err(why),
        }
    }

    /// `name`, written sealed and by handle, after the seal's as-found rule
    /// ran again: whatever changed in the folder since is judged first.
    pub(super) fn write(base: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
        let _one = state();
        let held = seal(base).map_err(Failure::into_reason)?;
        held.write_file(name, bytes, Seal::Private)
            .map_err(|r| r.to_string())
    }

    /// `name`, read by handle: the folder held below `base`, its owner and
    /// DACL judged, the file opened below it and judged the same way
    /// ([`verdict`]: SYSTEM or Administrators own it, and no other account
    /// may change it), then read through that handle.
    pub(super) fn read(base: &Path, name: &str, max_len: u64) -> Result<Found, String> {
        let dir = base.join(DIR_NAME);
        let folder = match machine_tree::held_below(base, &dir) {
            Ok(Some(folder)) => folder,
            Ok(None) => return Ok(Found::Absent),
            Err(why) => return Ok(Found::Refused(why)),
        };
        let judged = folder
            .security()
            .and_then(|sd| verdict(&sd, WRITE_LIKE, &dir.display().to_string()));
        if let Err(why) = judged {
            return Ok(Found::Refused(why));
        }
        let file = match folder.file(name, OpenFor::Read) {
            Ok(Some(file)) => file,
            Ok(None) => return Ok(Found::Absent),
            Err(why) => return Ok(Found::Refused(why)),
        };
        let path = dir.join(name);
        let judged = file
            .security()
            .and_then(|sd| verdict(&sd, WRITE_LIKE, &path.display().to_string()));
        if let Err(why) = judged {
            return Ok(Found::Refused(why));
        }
        Ok(match file.read_bytes(max_len)? {
            Some(bytes) => Found::Bytes(bytes),
            None => Found::TooLarge,
        })
    }

    pub(super) enum Failure {
        /// The config's folder cannot be held as itself: a share, a link, a
        /// redirected profile.
        Unsealable(String),
        /// Anything else, this time.
        Failed(String),
    }

    impl Failure {
        fn into_reason(self) -> String {
            match self {
                Self::Unsealable(why) | Self::Failed(why) => why,
            }
        }
    }

    /// The folder as found, before anything is sealed.
    enum AsFound {
        Absent,
        /// SYSTEM or Administrators own it; `closed`: no other account could
        /// change what it holds.
        Ours {
            closed: bool,
        },
        /// Another account owns it.
        Foreign(String),
        /// A link, or not a folder.
        NotAFolder(String),
        /// Anything else that kept it from being read.
        Unread(String),
    }

    /// A file the folder holds that this module reads.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Key,
        Pins,
    }

    /// `<tenant id>.json` or `<tenant id>.controllers.json` (`key_path`,
    /// `pins_path`); nothing else is read, sealed or moved.
    fn kind_of(name: &str) -> Option<Kind> {
        let id = |id: &str| id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit());
        if let Some(tenant) = name.strip_suffix(".controllers.json") {
            return id(tenant).then_some(Kind::Pins);
        }
        name.strip_suffix(".json")
            .filter(|tenant| id(tenant))
            .map(|_| Kind::Key)
    }

    /// Owner Administrators, protected, SYSTEM and Administrators full
    /// control and nothing else: what [`Seal::Private`] gives a file.
    fn exactly_sealed(sd: &Sd) -> bool {
        let want = Seal::Private.aces(0);
        sd.owner == ADMINISTRATORS
            && sd.protected
            && sd.dacl.as_ref().is_some_and(|aces| {
                aces.len() == want.len() && want.iter().all(|w| aces.contains(w))
            })
    }

    /// Seal `<base>/peer-auth`, reading as found first, because a seal makes
    /// every later check pass on whatever the folder holds:
    ///
    /// - the folder: made sealed when absent, sealed in place when SYSTEM or
    ///   Administrators own it, and moved aside whole (never followed, nothing
    ///   in it deleted) when another account owns it or it is not a folder
    ///   (`HeldPath::sealed_dir`'s leaf policy);
    /// - a key file is kept, with the same bytes, only when SYSTEM or
    ///   Administrators own it and its folder: an elevated or SYSTEM run made
    ///   both. Any other is moved aside, and a new key is made;
    /// - a browser-key file is kept only when its folder and the file were
    ///   both closed to every other account as found; any other is moved
    ///   aside, and each browser reads `unknown` once more.
    ///
    /// A file kept is written fresh with the seal and renamed into place by
    /// handle, unless it already has exactly the seal. What is moved aside
    /// goes to the config's folder as `peer-auth.<name>.not-sealed-<UTC
    /// time>`. Nothing is deleted. Nothing changes, and nothing is said, when
    /// everything already has the seal.
    pub(super) fn seal(base_path: &Path) -> Result<HeldPath, Failure> {
        let base = HeldPath::base(base_path).map_err(|r| Failure::Unsealable(r.to_string()))?;
        let dir = base_path.join(DIR_NAME);
        let found = match base.open_dir(DIR_NAME) {
            Ok(None) => AsFound::Absent,
            Ok(Some(held)) => match held.folder().security() {
                Ok(sd) if trusted(&sd.owner) => AsFound::Ours {
                    closed: verdict(&sd, WRITE_LIKE, "the folder").is_ok(),
                },
                Ok(sd) => AsFound::Foreign(sd.owner),
                Err(why) => AsFound::Unread(why),
            },
            Err(refusal) => match refusal.why {
                Why::Link | Why::NotFolder => AsFound::NotAFolder(refusal.to_string()),
                _ => AsFound::Unread(refusal.to_string()),
            },
        };
        let held = base
            .sealed_dir(DIR_NAME, Seal::Private)
            .map_err(|r| Failure::Failed(r.to_string()))?;
        let folder_closed = match &found {
            AsFound::Absent => true,
            AsFound::Ours { closed } => *closed,
            AsFound::Foreign(owner) => {
                tracing::warn!(
                    path = %dir.display(), %owner,
                    "peer-auth: the folder was owned by an account other than SYSTEM or \
                     Administrators, so the device keys in it are not kept: it is moved aside \
                     whole, and nothing in it is deleted. New device keys are made, and each \
                     browser that knew this device's old key warns once"
                );
                false
            }
            AsFound::NotAFolder(why) => {
                tracing::warn!(
                    path = %dir.display(), reason = %why,
                    "peer-auth: the folder was not a plain folder: it is moved aside, never \
                     followed, and nothing is deleted. New device keys are made, and each \
                     browser that knew this device's old key warns once"
                );
                false
            }
            AsFound::Unread(why) => {
                tracing::warn!(
                    path = %dir.display(), reason = %why,
                    "peer-auth: the folder could not be read as found, so no browser-key file \
                     in it is kept"
                );
                false
            }
        };
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let mut sealed = held.resealed();
        let entries = held.entries().map_err(|r| Failure::Failed(r.to_string()))?;
        for entry in entries {
            let Some(kind) = kind_of(&entry.name) else {
                continue;
            };
            let path = dir.join(&entry.name);
            if entry.is_dir || entry.is_link {
                if first_time(format!("not a plain file {}", path.display())) {
                    tracing::warn!(
                        path = %path.display(),
                        "peer-auth: not a plain file, so it is not used, and it is left as it is"
                    );
                }
                continue;
            }
            let file = match held.folder().file(&entry.name, OpenFor::Read) {
                Ok(Some(file)) => file,
                Ok(None) => continue,
                Err(why) => return Err(Failure::Failed(why)),
            };
            let sd = file.security().map_err(Failure::Failed)?;
            let keep = match kind {
                Kind::Key if trusted(&sd.owner) => Ok(()),
                Kind::Key => Err(format!(
                    "it was owned by {}, an account other than SYSTEM or Administrators",
                    sd.owner
                )),
                Kind::Pins if folder_closed => verdict(&sd, WRITE_LIKE, "it"),
                Kind::Pins => Err(
                    "its folder let an account other than SYSTEM or Administrators change \
                     what it held"
                        .to_string(),
                ),
            };
            if keep.is_ok() && exactly_sealed(&sd) {
                continue;
            }
            let max_len = if kind == Kind::Key { KEY_MAX } else { PINS_MAX };
            let bytes = match keep {
                Ok(()) => file.read_bytes(max_len).map_err(Failure::Failed)?,
                Err(_) => None,
            };
            drop(file);
            if let Some(bytes) = bytes {
                held.write_file(&entry.name, &bytes, Seal::Private)
                    .map_err(|r| Failure::Failed(r.to_string()))?;
                sealed = true;
                continue;
            }
            let why = keep
                .err()
                .unwrap_or_else(|| "it is too large to be one".to_string());
            let aside = move_aside(&held, &base, &entry.name, &stamp)?;
            match kind {
                Kind::Key => tracing::warn!(
                    path = %path.display(), moved_to = %aside.display(), reason = %why,
                    "peer-auth: the device key in this file is not kept: it is moved aside, and \
                     nothing is deleted. A new device key is made, and each browser that knew \
                     this device's old key warns once"
                ),
                Kind::Pins => tracing::warn!(
                    path = %path.display(), moved_to = %aside.display(), reason = %why,
                    "peer-auth: the browser-key file is not kept: it is moved aside, and \
                     nothing is deleted. Each browser reads `unknown` once more"
                ),
            }
        }
        if sealed {
            tracing::warn!(
                path = %dir.display(),
                "peer-auth: sealed the folder in place to SYSTEM and Administrators; the device \
                 keys in it are kept"
            );
        }
        Ok(held)
    }

    /// Move the file `name` of `held` to `base` as `peer-auth.<name>.not-sealed-
    /// <stamp>`, by its handle: never through a link, and nothing replaced.
    fn move_aside(
        held: &HeldPath,
        base: &HeldPath,
        name: &str,
        stamp: &str,
    ) -> Result<PathBuf, Failure> {
        let mut last = String::new();
        for n in 0..10 {
            let aside = if n == 0 {
                format!("{DIR_NAME}.{name}.not-sealed-{stamp}")
            } else {
                format!("{DIR_NAME}.{name}.not-sealed-{stamp}-{n}")
            };
            match held.rename_file(name, base, &aside, false) {
                Ok(()) => return Ok(base.path().join(aside)),
                Err(refusal) => match &refusal.why {
                    Why::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        last = refusal.to_string();
                    }
                    _ => return Err(Failure::Failed(refusal.to_string())),
                },
            }
        }
        Err(Failure::Failed(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::signaling::peer_auth_key_is_well_formed;

    const TENANT_A: &str = "66f0c0ffee00000000000a01";
    const TENANT_B: &str = "66f0c0ffee00000000000b02";

    fn config_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("config.toml")
    }

    /// [`load_or_make`] for the key file at `path` (`key_path`'s), kept as
    /// this process keeps that config's folder.
    fn load_at(path: &Path) -> Result<DeviceKey, String> {
        let cfg = path.parent().unwrap().parent().unwrap().join("config.toml");
        let folder = Folder::of(&cfg, keeping_for(&cfg))?;
        load_or_make(&folder, &file_name(path))
    }

    /// `f`, with every folder this thread's test touches kept as `kept`.
    fn kept_as<T>(kept: Keeping, f: impl FnOnce() -> T) -> T {
        struct Reset(Option<Keeping>);
        impl Drop for Reset {
            fn drop(&mut self) {
                KEPT_AS.with(|k| k.set(self.0));
            }
        }
        let _reset = Reset(KEPT_AS.with(|k| k.replace(Some(kept))));
        f()
    }

    /// What a per-user install does on this platform: nothing sealed.
    fn plain() -> Keeping {
        Keeping::Plain(if cfg!(windows) {
            Plain::NotElevated
        } else {
            Plain::NotWindows
        })
    }

    #[test]
    fn a_key_is_made_once_and_read_back_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&config_in(&dir), TENANT_A).unwrap();
        assert!(!path.exists());
        let made = load_at(&path).unwrap().public_b64();
        assert!(path.exists());
        assert!(peer_auth_key_is_well_formed(&made), "{made}");
        // A second read — a restart — finds the same key, and makes none.
        assert_eq!(load_at(&path).unwrap().public_b64(), made);
    }

    /// AC7 at the unit level: two enrollments on one device, two keys, two
    /// files.
    #[test]
    fn two_enrollments_get_two_keys() {
        let dir = tempfile::tempdir().unwrap();
        let a = key_path(&config_in(&dir), TENANT_A).unwrap();
        let b = key_path(&config_in(&dir), TENANT_B).unwrap();
        assert_ne!(a, b);
        assert_ne!(
            load_at(&a).unwrap().public_b64(),
            load_at(&b).unwrap().public_b64()
        );
    }

    #[test]
    fn the_hello_offers_the_same_key_for_the_whole_run() {
        let dir = tempfile::tempdir().unwrap();
        let first = hello_key(&config_in(&dir), TENANT_A);
        assert!(peer_auth_key_is_well_formed(&first), "{first}");
        assert_eq!(hello_key(&config_in(&dir), TENANT_A), first);
    }

    #[test]
    fn a_corrupt_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&config_in(&dir), TENANT_A).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            b"{\"version\":1,\"alg\":\"ecdsa-p256-sha256\",\"pkcs8\":\"AAAA\"}",
        )
        .unwrap();
        let key = load_at(&path).unwrap().public_b64();
        assert!(peer_auth_key_is_well_formed(&key));
        assert_eq!(
            load_at(&path).unwrap().public_b64(),
            key,
            "the replacement stuck"
        );
        std::fs::write(&path, b"\0\0\0\0").unwrap();
        assert!(
            load_at(&path).is_ok(),
            "NUL-filled, as an unclean shutdown leaves"
        );
    }

    /// A rollback must not destroy the newer agent's key: its file is left
    /// as it was, and this build offers none.
    #[test]
    fn a_newer_key_file_is_left_alone() {
        kept_as(plain(), || {
            let dir = tempfile::tempdir().unwrap();
            let path = key_path(&config_in(&dir), TENANT_A).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let newer = br#"{"version":2,"alg":"ecdsa-p256-sha256","pkcs8":"from the future"}"#;
            std::fs::write(&path, newer).unwrap();
            assert!(load_at(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), newer);
        });
    }

    /// A key that cannot be written is not offered: `peer-auth` is a FILE
    /// here, so the directory cannot be made. (Where the folder is sealed,
    /// what is in the way is moved aside instead.)
    #[test]
    fn a_key_that_cannot_be_kept_is_not_offered() {
        kept_as(plain(), || {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(DIR_NAME), b"in the way").unwrap();
            assert_eq!(hello_key(&config_in(&dir), TENANT_A), "");
            assert!(
                device_key(&config_in(&dir), TENANT_A).is_err(),
                "and nothing was cached"
            );
        });
    }

    /// A bare config name has no directory of its own: no key, and nothing
    /// written where the process happens to be standing.
    #[test]
    fn a_config_path_without_a_directory_gets_no_key() {
        let bare = Path::new("unused-in-tests.toml");
        assert!(key_path(bare, TENANT_A).is_err());
        assert_eq!(hello_key(bare, TENANT_A), "");
        assert!(
            !Path::new(DIR_NAME).exists(),
            "nothing made in the working directory"
        );
    }

    #[test]
    fn only_an_object_id_names_a_key_file() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "",
            "../../etc/passwd",
            "66f0c0ffee00000000000a0",
            "66f0c0ffee00000000000a01x",
            "zzf0c0ffee00000000000a01",
        ] {
            assert!(key_path(&config_in(&dir), bad).is_err(), "{bad:?}");
        }
        let upper = key_path(&config_in(&dir), &TENANT_A.to_ascii_uppercase()).unwrap();
        assert_eq!(
            upper,
            key_path(&config_in(&dir), TENANT_A).unwrap(),
            "one file per tenant"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&config_in(&dir), TENANT_A).unwrap();
        load_at(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    // ─── P1b — signing the answer ───────────────────────────────────────

    use bson::oid::ObjectId;
    use roomler_ai_remote_control::signaling::{
        ClientMsg, PeerAuthAnswer, PeerAuthOffer, peer_auth_transcript, peer_auth_verify,
    };

    const OFFER_SDP: &str = "v=0\r\na=fingerprint:sha-256 aa:bb:cc\r\n";
    const ANSWER_SDP: &str = "v=0\r\na=fingerprint:sha-256 dd:ee:ff\r\n";
    const AGENT: &str = "66f0c0ffee00000000000d01";

    /// A browser's nonce: bytes 0 to 15, base64url without padding. Built,
    /// not written as a literal: a secret scanner cannot tell a counting
    /// sequence from a key.
    fn nonce() -> String {
        base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            (0u8..16).collect::<Vec<u8>>(),
        )
    }

    fn ask() -> PeerAuthOffer {
        PeerAuthOffer {
            nonce: nonce(),
            key: None,
            sig: None,
        }
    }

    /// The transcript the browser would rebuild for this session's answer.
    fn transcript_for(session: ObjectId, user: ObjectId) -> Vec<u8> {
        peer_auth_transcript(
            "answer",
            &session.to_hex(),
            AGENT,
            &user.to_hex(),
            "sha-256 AA:BB:CC",
            "sha-256 DD:EE:FF",
            &nonce(),
        )
        .unwrap()
    }

    #[test]
    fn an_asked_answer_is_signed_over_the_sessions_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let offers = PendingOffers::default();
        let (session, user) = (ObjectId::new(), ObjectId::new());
        offers.note_request(session, user);
        offers.note_offer(session, OFFER_SDP, Some(&ask()));
        let signed = sign_answer(&offers, &cfg, TENANT_A, AGENT, session, ANSWER_SDP)
            .expect("an asked answer is signed");
        // The key that signed is this enrollment's, the one its hello offers.
        assert_eq!(signed.key, hello_key(&cfg, TENANT_A));
        assert!(peer_auth_verify(
            &signed.key,
            &transcript_for(session, user),
            &signed.sig
        ));
        // …and over that session's transcript only: another controller's
        // would not verify.
        assert!(!peer_auth_verify(
            &signed.key,
            &transcript_for(session, ObjectId::new()),
            &signed.sig
        ));
        // One answer per offer; a re-offer that asks is answered again, the
        // controller having stayed noted.
        assert!(sign_answer(&offers, &cfg, TENANT_A, AGENT, session, ANSWER_SDP).is_none());
        offers.note_offer(session, OFFER_SDP, Some(&ask()));
        assert!(sign_answer(&offers, &cfg, TENANT_A, AGENT, session, ANSWER_SDP).is_some());
    }

    #[test]
    fn an_answer_nobody_asked_to_sign_goes_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let offers = PendingOffers::default();
        let (session, user) = (ObjectId::new(), ObjectId::new());
        let sign = |offers: &PendingOffers, session| {
            sign_answer(offers, &cfg, TENANT_A, AGENT, session, ANSWER_SDP)
        };
        offers.note_request(session, user);

        offers.note_offer(session, OFFER_SDP, None);
        assert!(sign(&offers, session).is_none(), "the offer did not ask");
        let bad = PeerAuthOffer {
            nonce: "too-short".into(),
            key: None,
            sig: None,
        };
        offers.note_offer(session, OFFER_SDP, Some(&bad));
        assert!(
            sign(&offers, session).is_none(),
            "a nonce that is not well formed"
        );
        offers.note_offer(session, "v=0\r\n", Some(&ask()));
        assert!(
            sign(&offers, session).is_none(),
            "an offer with no fingerprint"
        );
        offers.note_offer(session, OFFER_SDP, Some(&ask()));
        offers.note_offer(session, OFFER_SDP, None);
        assert!(
            sign(&offers, session).is_none(),
            "a re-offer that did not ask"
        );
        offers.note_offer(session, OFFER_SDP, Some(&ask()));
        offers.forget(session);
        assert!(sign(&offers, session).is_none(), "an ended session");

        // A session whose request never reached this process: an outsider's
        // (bound by FR-52 instead), or one another daemon admitted.
        let stranger = ObjectId::new();
        offers.note_offer(stranger, OFFER_SDP, Some(&ask()));
        assert!(sign(&offers, stranger).is_none());
    }

    #[test]
    fn an_enrollment_without_a_kept_key_signs_nothing() {
        kept_as(plain(), || {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(DIR_NAME), b"in the way").unwrap();
            let offers = PendingOffers::default();
            let (session, user) = (ObjectId::new(), ObjectId::new());
            offers.note_request(session, user);
            offers.note_offer(session, OFFER_SDP, Some(&ask()));
            assert!(
                sign_answer(
                    &offers,
                    &config_in(&dir),
                    TENANT_A,
                    AGENT,
                    session,
                    ANSWER_SDP
                )
                .is_none()
            );
        });
    }

    /// The FR-43 choke point: a worker's answer leaves signed by the daemon,
    /// and whatever the worker claimed is never passed on.
    #[test]
    fn a_workers_answer_is_signed_by_the_daemon_and_its_own_claim_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let offers = PendingOffers::default();
        let (session, user) = (ObjectId::new(), ObjectId::new());
        offers.note_request(session, user);
        offers.note_offer(session, OFFER_SDP, Some(&ask()));
        let forged = || {
            Some(PeerAuthAnswer {
                key: "a worker's key".into(),
                sig: "a worker's signature".into(),
            })
        };
        let mut msg = ClientMsg::SdpAnswer {
            session_id: session,
            sdp: ANSWER_SDP.into(),
            extauth_mac: None,
            peer_auth: forged(),
            // FR-100 P1c — a worker's verdict is not the daemon's either.
            peer_auth_verdict: Some("verified".into()),
        };
        sign_outbound(
            &offers,
            &cfg,
            TENANT_A,
            AGENT,
            PeerAuthPolicy::Audit,
            &mut msg,
        );
        let ClientMsg::SdpAnswer {
            peer_auth: Some(signed),
            peer_auth_verdict,
            ..
        } = msg
        else {
            panic!("the daemon signs the worker's answer");
        };
        assert!(peer_auth_verify(
            &signed.key,
            &transcript_for(session, user),
            &signed.sig
        ));
        assert_eq!(
            peer_auth_verdict, None,
            "the daemon judged nothing here, so it reports nothing"
        );

        // Nothing noted for this one: the worker's claim is dropped, not kept.
        let mut msg = ClientMsg::SdpAnswer {
            session_id: ObjectId::new(),
            sdp: ANSWER_SDP.into(),
            extauth_mac: None,
            peer_auth: forged(),
            peer_auth_verdict: Some("verified".into()),
        };
        sign_outbound(
            &offers,
            &cfg,
            TENANT_A,
            AGENT,
            PeerAuthPolicy::Audit,
            &mut msg,
        );
        assert!(matches!(
            msg,
            ClientMsg::SdpAnswer {
                peer_auth: None,
                peer_auth_verdict: None,
                ..
            }
        ));
    }

    #[test]
    fn the_table_forgets_its_oldest_session_past_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let offers = PendingOffers::default();
        let first = ObjectId::new();
        offers.note_request(first, ObjectId::new());
        for _ in 0..MAX_PENDING {
            offers.note_request(ObjectId::new(), ObjectId::new());
        }
        assert_eq!(offers.table.lock().unwrap().sessions.len(), MAX_PENDING);
        offers.note_offer(first, OFFER_SDP, Some(&ask()));
        assert!(
            sign_answer(
                &offers,
                &config_in(&dir),
                TENANT_A,
                AGENT,
                first,
                ANSWER_SDP
            )
            .is_none(),
            "the oldest session was forgotten"
        );
    }

    // ─── P1c — the device checks the browser ────────────────────────────

    /// A browser: a P-256 key the test holds, its public half as an offer
    /// carries it. Made fresh, never a literal.
    struct Browser {
        pair: EcdsaKeyPair,
        key: String,
    }

    fn browser() -> Browser {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let key = STANDARD.encode(pair.public_key().as_ref());
        Browser { pair, key }
    }

    /// `browser`'s offer for this session and user, signed over the
    /// transcript with role `offer` and an EMPTY answer fingerprint.
    fn signed_offer(browser: &Browser, session: ObjectId, user: ObjectId) -> PeerAuthOffer {
        let transcript = peer_auth_transcript(
            "offer",
            &session.to_hex(),
            AGENT,
            &user.to_hex(),
            "sha-256 AA:BB:CC",
            "",
            &nonce(),
        )
        .unwrap();
        let sig = browser
            .pair
            .sign(&SystemRandom::new(), &transcript)
            .unwrap();
        PeerAuthOffer {
            nonce: nonce(),
            key: Some(browser.key.clone()),
            sig: Some(base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                sig.as_ref(),
            )),
        }
    }

    fn check<'a>(cfg: &'a Path, policy: PeerAuthPolicy) -> OfferCheck<'a> {
        OfferCheck {
            policy,
            config_path: cfg,
            tenant_id: TENANT_A,
            agent_id: AGENT,
        }
    }

    /// One in-org offer, noted and judged as the daemon does it.
    fn judged(
        offers: &PendingOffers,
        cfg: &Path,
        policy: PeerAuthPolicy,
        session: ObjectId,
        user: ObjectId,
        ask: &PeerAuthOffer,
    ) -> Option<Verdict> {
        offers.note_request(session, user);
        offers.note_offer(session, OFFER_SDP, Some(ask));
        offers.judge_offer(session, OFFER_SDP, Some(ask), &check(cfg, policy))
    }

    #[test]
    fn an_offer_nobody_signed_reads_absent_and_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let offers = PendingOffers::default();
        let session = ObjectId::new();
        let verdict = judged(
            &offers,
            &config_in(&dir),
            PeerAuthPolicy::Audit,
            session,
            ObjectId::new(),
            &ask(),
        );
        assert_eq!(verdict, Some(Verdict::Absent));
        assert_eq!(
            answer_verdict(&offers, PeerAuthPolicy::Audit, session, false).as_deref(),
            Some("absent")
        );
        assert_eq!(
            answer_verdict(&offers, PeerAuthPolicy::Audit, session, false),
            None,
            "one report per offer"
        );
        // An offer that did not even ask is judged too: nobody signed it.
        let other = ObjectId::new();
        offers.note_request(other, ObjectId::new());
        offers.note_offer(other, OFFER_SDP, None);
        assert_eq!(
            offers.judge_offer(
                other,
                OFFER_SDP,
                None,
                &check(&config_in(&dir), PeerAuthPolicy::Audit)
            ),
            Some(Verdict::Absent)
        );
    }

    /// Trust on first use, per user: a browser's first offer is `unknown` and
    /// remembered; its next is `verified`; the same key for ANOTHER user is
    /// that user's first sighting.
    #[test]
    fn a_browser_is_unknown_once_then_verified_per_user() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let offers = PendingOffers::default();
        let chrome = browser();
        let user = ObjectId::new();
        let first = ObjectId::new();
        let ask = signed_offer(&chrome, first, user);
        assert_eq!(
            judged(&offers, &cfg, PeerAuthPolicy::Audit, first, user, &ask),
            Some(Verdict::Unknown)
        );
        assert!(
            pins_path(&cfg, TENANT_A).unwrap().exists(),
            "remembered on disk"
        );
        let second = ObjectId::new();
        let ask = signed_offer(&chrome, second, user);
        assert_eq!(
            judged(&offers, &cfg, PeerAuthPolicy::Audit, second, user, &ask),
            Some(Verdict::Verified)
        );
        let someone_else = ObjectId::new();
        let third = ObjectId::new();
        let ask = signed_offer(&chrome, third, someone_else);
        assert_eq!(
            judged(
                &offers,
                &cfg,
                PeerAuthPolicy::Audit,
                third,
                someone_else,
                &ask
            ),
            Some(Verdict::Unknown)
        );
    }

    /// A signature that does not cover THIS session is a mismatch, whatever
    /// it would verify over: another session, another user, a nonce that is
    /// not a nonce. So is an offer that names no single certificate.
    #[test]
    fn a_signature_over_anything_else_is_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let offers = PendingOffers::default();
        let chrome = browser();
        let user = ObjectId::new();

        let session = ObjectId::new();
        let for_another_session = signed_offer(&chrome, ObjectId::new(), user);
        assert_eq!(
            judged(
                &offers,
                &cfg,
                PeerAuthPolicy::Audit,
                session,
                user,
                &for_another_session
            ),
            Some(Verdict::Mismatch)
        );

        let session = ObjectId::new();
        let for_another_user = signed_offer(&chrome, session, ObjectId::new());
        assert_eq!(
            judged(
                &offers,
                &cfg,
                PeerAuthPolicy::Audit,
                session,
                user,
                &for_another_user
            ),
            Some(Verdict::Mismatch)
        );

        let session = ObjectId::new();
        let mut bad_nonce = signed_offer(&chrome, session, user);
        bad_nonce.nonce = "too-short".into();
        assert_eq!(
            judged(
                &offers,
                &cfg,
                PeerAuthPolicy::Audit,
                session,
                user,
                &bad_nonce
            ),
            Some(Verdict::Mismatch)
        );

        let session = ObjectId::new();
        let ask = signed_offer(&chrome, session, user);
        offers.note_request(session, user);
        offers.note_offer(session, "v=0\r\n", Some(&ask));
        assert_eq!(
            offers.judge_offer(
                session,
                "v=0\r\n",
                Some(&ask),
                &check(&cfg, PeerAuthPolicy::Audit)
            ),
            Some(Verdict::Mismatch),
            "no certificate for the signature to cover"
        );
        assert!(
            !pins_path(&cfg, TENANT_A).unwrap().exists(),
            "a mismatch is never remembered"
        );
    }

    /// `off` judges nothing, remembers nothing and reports nothing, for an
    /// outsider's session too.
    #[test]
    fn off_judges_remembers_and_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let offers = PendingOffers::default();
        let session = ObjectId::new();
        let user = ObjectId::new();
        let ask = signed_offer(&browser(), session, user);
        assert_eq!(
            judged(&offers, &cfg, PeerAuthPolicy::Off, session, user, &ask),
            None
        );
        assert_eq!(
            answer_verdict(&offers, PeerAuthPolicy::Off, session, false),
            None
        );
        assert_eq!(
            answer_verdict(&offers, PeerAuthPolicy::Off, session, true),
            None
        );
        assert!(!pins_path(&cfg, TENANT_A).unwrap().exists());
    }

    /// An outsider's session is never noted in-org: its answer reports
    /// `verified`, because FR-52 admitted it on a MAC the device checked.
    #[test]
    fn an_outsiders_answer_reports_verified() {
        let offers = PendingOffers::default();
        assert_eq!(
            answer_verdict(&offers, PeerAuthPolicy::Audit, ObjectId::new(), true).as_deref(),
            Some("verified")
        );
        assert_eq!(
            answer_verdict(&offers, PeerAuthPolicy::Audit, ObjectId::new(), false),
            None,
            "an in-org session nobody judged reports nothing"
        );
    }

    /// A re-offer is judged afresh: the first offer's verdict never answers
    /// the second.
    #[test]
    fn a_re_offer_drops_the_earlier_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let offers = PendingOffers::default();
        let session = ObjectId::new();
        let user = ObjectId::new();
        let ask = signed_offer(&browser(), session, user);
        judged(&offers, &cfg, PeerAuthPolicy::Audit, session, user, &ask);
        offers.note_offer(session, OFFER_SDP, None);
        assert_eq!(
            answer_verdict(&offers, PeerAuthPolicy::Audit, session, false),
            None,
            "noted but not yet judged"
        );
    }

    /// A person with more browsers than the cap loses the OLDEST one's pin.
    #[test]
    fn a_users_pins_are_bounded_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        let user = ObjectId::new();
        let keys: Vec<String> = (0..=MAX_KEYS_PER_USER).map(|_| browser().key).collect();
        for key in &keys {
            assert_eq!(remember(&cfg, TENANT_A, user, key), Ok(Pinned::Now));
        }
        let stored: PinFile =
            serde_json::from_slice(&std::fs::read(pins_path(&cfg, TENANT_A).unwrap()).unwrap())
                .unwrap();
        let kept: Vec<&str> = stored.users[&user.to_hex()]
            .iter()
            .map(|k| k.key.as_str())
            .collect();
        assert_eq!(kept.len(), MAX_KEYS_PER_USER);
        assert!(!kept.contains(&keys[0].as_str()), "the oldest went first");
        assert!(kept.contains(&keys[MAX_KEYS_PER_USER].as_str()));
    }

    /// A rollback must not destroy a newer agent's pins: the file is left as
    /// it was, and nothing is remembered on top of it (the verdict still says
    /// `unknown`, honestly).
    #[test]
    fn a_newer_pin_file_is_left_alone() {
        kept_as(plain(), || {
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_in(&dir);
            let path = pins_path(&cfg, TENANT_A).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let newer = br#"{"version":2,"users":{"from":"the future"}}"#;
            std::fs::write(&path, newer).unwrap();
            let offers = PendingOffers::default();
            let session = ObjectId::new();
            let user = ObjectId::new();
            let ask = signed_offer(&browser(), session, user);
            assert_eq!(
                judged(&offers, &cfg, PeerAuthPolicy::Audit, session, user, &ask),
                Some(Verdict::Unknown)
            );
            assert_eq!(std::fs::read(&path).unwrap(), newer);
        });
    }

    /// A bare config name has no directory: nothing is remembered, nothing is
    /// written where the process stands, and the verdict says `unknown`.
    #[test]
    fn a_config_path_without_a_directory_remembers_nothing() {
        let bare = Path::new("unused-in-tests.toml");
        let offers = PendingOffers::default();
        let session = ObjectId::new();
        let user = ObjectId::new();
        let ask = signed_offer(&browser(), session, user);
        assert_eq!(
            judged(&offers, bare, PeerAuthPolicy::Audit, session, user, &ask),
            Some(Verdict::Unknown)
        );
        assert!(!Path::new(DIR_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_pins_are_readable_by_their_owner_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_in(&dir);
        remember(&cfg, TENANT_A, ObjectId::new(), &browser().key).unwrap();
        let path = pins_path(&cfg, TENANT_A).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // ─── where the folder is kept ───────────────────────────────────────

    /// The seal applies on Windows to an elevated or SYSTEM run whose config
    /// is outside the machine folder, unless the service's switch is off; the
    /// switch is read only when it decides.
    #[test]
    fn the_seal_applies_to_an_elevated_windows_run_outside_the_machine_folder() {
        let unread = || -> bool { panic!("the switch is read only when it decides") };
        assert_eq!(
            keeping(false, true, false, unread),
            Keeping::Plain(Plain::NotWindows)
        );
        assert_eq!(
            keeping(true, true, true, unread),
            Keeping::Plain(Plain::MachineFolder)
        );
        assert_eq!(
            keeping(true, false, false, unread),
            Keeping::Plain(Plain::NotElevated),
            "a per-user install is left as it is"
        );
        assert_eq!(
            keeping(true, true, false, || false),
            Keeping::Plain(Plain::SwitchedOff)
        );
        assert_eq!(keeping(true, true, false, || true), Keeping::Sealed);
    }

    /// The switch comes from the service's `Environment` value, under either
    /// prefix and in any case; nothing read, or nothing named, leaves it on.
    #[test]
    fn the_switch_is_read_from_the_services_environment() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect()
        };
        assert!(seal_enabled_in(None), "a value that could not be read");
        assert!(seal_enabled_in(Some(&env(&[]))));
        assert!(seal_enabled_in(Some(&env(&[("ROOMLERD_OTHER", "0")]))));
        for off in [
            ("ROOMLERD_PEER_AUTH_SEAL", "0"),
            ("roomlerd_peer_auth_seal", "off"),
            ("ROOMLER_NODE_PEER_AUTH_SEAL", "false"),
        ] {
            assert!(!seal_enabled_in(Some(&env(&[off]))), "{off:?}");
        }
        assert!(seal_enabled_in(Some(&env(&[(
            "ROOMLERD_PEER_AUTH_SEAL",
            "1"
        )]))));
    }

    /// What `f` logged, with what it returned.
    #[cfg(windows)]
    fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        // Two dispatchers, never one: with one, a callsite's interest is
        // taken once for every thread.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        (out, text)
    }

    /// The Windows cases. Each needs a token that may name Administrators as
    /// an owner, so each runs elevated: CI's elevated `--lib` lane runs them,
    /// and an unelevated local run says it skipped them.
    #[cfg(windows)]
    mod sealed_folder {
        use super::*;
        use roomler_node_core::machine_tree::test_support as ts;

        /// `BUILTIN\Guests`: an account that is neither SYSTEM nor an
        /// administrator.
        const GUESTS: &str = "*S-1-5-32-546";

        /// `path` has exactly the seal: owner Administrators, protected,
        /// SYSTEM and Administrators full control and nothing else, handed
        /// down when it is a folder.
        fn assert_sealed(path: &Path, folder: bool) {
            let sddl = ts::sddl(path);
            assert_eq!(ts::owner(&sddl), "BA", "{}: {sddl}", path.display());
            assert!(ts::protected(&sddl), "{}: {sddl}", path.display());
            let flags = if folder { "OICI" } else { "" };
            let mut aces = ts::aces(&sddl);
            aces.sort();
            let mut want = vec![format!("A;{flags};FA;;;BA"), format!("A;{flags};FA;;;SY")];
            want.sort();
            assert_eq!(aces, want, "{}: {sddl}", path.display());
        }

        /// The entries of `dir` whose names start with `prefix`.
        fn named(dir: &Path, prefix: &str) -> Vec<PathBuf> {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
                .collect()
        }

        /// A key file as an earlier build wrote it, by path: its public half.
        fn made_plain(cfg: &Path, tenant: &str) -> String {
            kept_as(plain(), || {
                let folder = Folder::of(cfg, keeping_for(cfg)).unwrap();
                let name = file_name(&key_path(cfg, tenant).unwrap());
                load_or_make(&folder, &name).unwrap().public_b64()
            })
        }

        /// A browser-key file listing `browser` for `user`.
        fn pins_listing(browser: &Browser, user: ObjectId) -> Vec<u8> {
            let mut file = PinFile {
                version: VERSION,
                ..Default::default()
            };
            file.users.insert(
                user.to_hex(),
                vec![PinnedKey {
                    key: browser.key.clone(),
                    first_seen: "2026-11-01T00:00:00Z".into(),
                }],
            );
            serde_json::to_vec_pretty(&file).unwrap()
        }

        #[test]
        fn a_new_folder_is_made_sealed_and_its_files_written_sealed() {
            if !ts::elevated_or_skip("a_new_folder_is_made_sealed_and_its_files_written_sealed") {
                return;
            }
            kept_as(Keeping::Sealed, || {
                let dir = tempfile::tempdir().unwrap();
                let cfg = config_in(&dir);
                assert!(peer_auth_key_is_well_formed(&hello_key(&cfg, TENANT_A)));
                assert_eq!(
                    remember(&cfg, TENANT_A, ObjectId::new(), &browser().key),
                    Ok(Pinned::Now)
                );
                let folder = dir.path().join(DIR_NAME);
                assert_sealed(&folder, true);
                assert_sealed(&key_path(&cfg, TENANT_A).unwrap(), false);
                assert_sealed(&pins_path(&cfg, TENANT_A).unwrap(), false);
                // Written by handle, renamed into place: no temp file stays.
                assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 2);
            });
        }

        /// An earlier build's folder, owned by Administrators and inheriting
        /// an entry for another account: sealed in place, the device key kept
        /// byte for byte, the browser keys moved aside (nothing deleted), and
        /// nothing said again at the next start.
        #[test]
        fn a_folder_administrators_own_is_sealed_in_place_and_keeps_its_key() {
            if !ts::elevated_or_skip(
                "a_folder_administrators_own_is_sealed_in_place_and_keeps_its_key",
            ) {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_in(&dir);
            let before = made_plain(&cfg, TENANT_A);
            let folder = dir.path().join(DIR_NAME);
            let key_file = key_path(&cfg, TENANT_A).unwrap();
            let pins_file = pins_path(&cfg, TENANT_A).unwrap();
            let pins = pins_listing(&browser(), ObjectId::new());
            write_private(&pins_file, &pins).unwrap();
            assert!(ts::icacls(
                &folder,
                &["/grant", &format!("{GUESTS}:(OI)(CI)(M)")]
            ));
            for path in [&folder, &key_file, &pins_file] {
                assert!(ts::icacls(path, &["/setowner", "*S-1-5-32-544"]));
            }
            let key_bytes = std::fs::read(&key_file).unwrap();

            let (after, said) = logged(|| kept_as(Keeping::Sealed, || hello_key(&cfg, TENANT_A)));
            assert_eq!(after, before, "the same device key: no browser warns");
            assert_eq!(
                std::fs::read(&key_file).unwrap(),
                key_bytes,
                "byte for byte"
            );
            assert_sealed(&folder, true);
            assert_sealed(&key_file, false);
            assert_eq!(
                said.matches("sealed the folder in place").count(),
                1,
                "{said}"
            );
            assert!(!pins_file.exists(), "the browser keys are not kept");
            let moved = named(
                dir.path(),
                "peer-auth.66f0c0ffee00000000000a01.controllers.json.not-sealed-",
            );
            assert_eq!(moved.len(), 1, "{moved:?}");
            assert_eq!(
                std::fs::read(&moved[0]).unwrap(),
                pins,
                "moved aside, not deleted"
            );

            // The next start finds it sealed: nothing changes, nothing is said.
            let (again, said) = logged(|| sealed::seal(dir.path()).map(|_| ()));
            assert!(again.is_ok());
            assert!(!said.contains("peer-auth"), "{said}");
            assert_eq!(std::fs::read(&key_file).unwrap(), key_bytes);
        }

        /// A folder another account owns (an earlier per-user install's, or
        /// one put in place later) is never adopted: moved aside whole,
        /// nothing in it deleted, and a new key made in a sealed folder.
        #[test]
        fn a_folder_another_account_owns_is_moved_aside_and_a_new_key_made() {
            if !ts::elevated_or_skip(
                "a_folder_another_account_owns_is_moved_aside_and_a_new_key_made",
            ) {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_in(&dir);
            let before = made_plain(&cfg, TENANT_A);
            let folder = dir.path().join(DIR_NAME);
            let key_file = key_path(&cfg, TENANT_A).unwrap();
            let key_bytes = std::fs::read(&key_file).unwrap();
            assert!(ts::icacls(&key_file, &["/setowner", GUESTS]));
            assert!(ts::icacls(&folder, &["/setowner", GUESTS]));

            let (after, said) = logged(|| kept_as(Keeping::Sealed, || hello_key(&cfg, TENANT_A)));
            assert!(peer_auth_key_is_well_formed(&after));
            assert_ne!(after, before, "a key another account held is never used");
            assert_sealed(&folder, true);
            assert_sealed(&key_file, false);
            let moved = named(dir.path(), "peer-auth.moved-");
            assert_eq!(moved.len(), 1, "{moved:?}");
            let old = moved[0].join(key_file.file_name().unwrap());
            assert_eq!(std::fs::read(old).unwrap(), key_bytes, "nothing deleted");
            assert!(said.contains("S-1-5-32-546"), "the owner is named: {said}");
            assert!(said.contains("warns once"), "{said}");
        }

        /// The same for one key file another account owns, in a folder
        /// Administrators own: that file moved aside, a new key for it.
        #[test]
        fn a_key_file_another_account_owns_is_moved_aside_and_a_new_key_made() {
            if !ts::elevated_or_skip(
                "a_key_file_another_account_owns_is_moved_aside_and_a_new_key_made",
            ) {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_in(&dir);
            let before = made_plain(&cfg, TENANT_A);
            let folder = dir.path().join(DIR_NAME);
            let key_file = key_path(&cfg, TENANT_A).unwrap();
            let key_bytes = std::fs::read(&key_file).unwrap();
            assert!(ts::icacls(&folder, &["/setowner", "*S-1-5-32-544"]));
            assert!(ts::icacls(&key_file, &["/setowner", GUESTS]));

            let (after, said) = logged(|| kept_as(Keeping::Sealed, || hello_key(&cfg, TENANT_A)));
            assert!(peer_auth_key_is_well_formed(&after));
            assert_ne!(after, before);
            assert_sealed(&key_file, false);
            let moved = named(
                dir.path(),
                "peer-auth.66f0c0ffee00000000000a01.json.not-sealed-",
            );
            assert_eq!(moved.len(), 1, "{moved:?}");
            assert_eq!(
                std::fs::read(&moved[0]).unwrap(),
                key_bytes,
                "nothing deleted"
            );
            assert!(said.contains("S-1-5-32-546"), "{said}");
        }

        /// A browser-key file another account may change is not read: the
        /// browser it lists reads `unknown`, said once.
        #[test]
        fn a_pin_file_another_account_may_change_is_not_read() {
            if !ts::elevated_or_skip("a_pin_file_another_account_may_change_is_not_read") {
                return;
            }
            kept_as(Keeping::Sealed, || {
                let dir = tempfile::tempdir().unwrap();
                let cfg = config_in(&dir);
                assert!(
                    !hello_key(&cfg, TENANT_A).is_empty(),
                    "the folder is sealed"
                );
                let (browser, user) = (browser(), ObjectId::new());
                let pins_file = pins_path(&cfg, TENANT_A).unwrap();
                std::fs::write(&pins_file, pins_listing(&browser, user)).unwrap();
                assert!(ts::icacls(
                    &pins_file,
                    &["/grant", &format!("{GUESTS}:(M)")]
                ));
                let offers = PendingOffers::default();
                let offer = |session| signed_offer(&browser, session, user);
                let (verdicts, said) = logged(|| {
                    let (first, second) = (ObjectId::new(), ObjectId::new());
                    let audit = PeerAuthPolicy::Audit;
                    (
                        judged(&offers, &cfg, audit, first, user, &offer(first)),
                        judged(&offers, &cfg, audit, second, user, &offer(second)),
                    )
                });
                assert_eq!(
                    verdicts.0,
                    Some(Verdict::Unknown),
                    "listed, but not trusted"
                );
                assert_eq!(
                    verdicts.1,
                    Some(Verdict::Verified),
                    "the device's own sealed memory of it"
                );
                assert_eq!(
                    said.matches("the browser-key file is not read").count(),
                    1,
                    "{said}"
                );
            });
        }

        /// A key file another account may change is not offered, is left as
        /// it is, and the cause is said once.
        #[test]
        fn a_key_file_another_account_may_change_is_not_offered() {
            if !ts::elevated_or_skip("a_key_file_another_account_may_change_is_not_offered") {
                return;
            }
            kept_as(Keeping::Sealed, || {
                let dir = tempfile::tempdir().unwrap();
                let cfg = config_in(&dir);
                assert!(
                    !hello_key(&cfg, TENANT_A).is_empty(),
                    "the folder is sealed"
                );
                let other = tempfile::tempdir().unwrap();
                made_plain(&config_in(&other), TENANT_B);
                let bytes = std::fs::read(key_path(&config_in(&other), TENANT_B).unwrap()).unwrap();
                let key_file = key_path(&cfg, TENANT_B).unwrap();
                std::fs::write(&key_file, &bytes).unwrap();
                assert!(ts::icacls(&key_file, &["/grant", &format!("{GUESTS}:(M)")]));
                let (keys, said) =
                    logged(|| (hello_key(&cfg, TENANT_B), hello_key(&cfg, TENANT_B)));
                assert_eq!(keys, (String::new(), String::new()), "no key offered");
                assert_eq!(said.matches("no device key this run").count(), 1, "{said}");
                assert!(
                    said.contains("lets an account other than SYSTEM or Administrators change it"),
                    "the cause: {said}"
                );
                assert_eq!(std::fs::read(&key_file).unwrap(), bytes, "left as it is");
            });
        }

        /// A config folder that cannot be held as itself (here a junction):
        /// one WARN, the key still offered, the browser keys never kept.
        #[test]
        fn a_folder_that_cannot_be_sealed_keeps_signing_and_refuses_pins() {
            if !ts::elevated_or_skip(
                "a_folder_that_cannot_be_sealed_keeps_signing_and_refuses_pins",
            ) {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let real = root.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let link = root.path().join("link");
            ts::junction(&link, &real);
            let cfg = link.join("config.toml");
            kept_as(Keeping::Sealed, || {
                let (browser, user) = (browser(), ObjectId::new());
                let offers = PendingOffers::default();
                let offer = |session| signed_offer(&browser, session, user);
                let (out, said) = logged(|| {
                    let (first, second) = (ObjectId::new(), ObjectId::new());
                    let audit = PeerAuthPolicy::Audit;
                    (
                        hello_key(&cfg, TENANT_A),
                        judged(&offers, &cfg, audit, first, user, &offer(first)),
                        judged(&offers, &cfg, audit, second, user, &offer(second)),
                    )
                });
                assert!(peer_auth_key_is_well_formed(&out.0), "signing goes on");
                assert_eq!(out.1, Some(Verdict::Unknown));
                assert_eq!(out.2, Some(Verdict::Unknown), "never remembered");
                assert_eq!(said.matches("cannot be sealed here").count(), 1, "{said}");
                assert!(!pins_path(&cfg, TENANT_A).unwrap().exists());
            });
        }

        /// The switch off: today's folder, inheriting what its parent hands
        /// down, and the switch said once.
        #[test]
        fn with_the_switch_off_the_folder_is_kept_as_before() {
            if !ts::elevated_or_skip("with_the_switch_off_the_folder_is_kept_as_before") {
                return;
            }
            kept_as(Keeping::Plain(Plain::SwitchedOff), || {
                let dir = tempfile::tempdir().unwrap();
                let cfg = config_in(&dir);
                let (key, said) = logged(|| {
                    let key = hello_key(&cfg, TENANT_A);
                    remember(&cfg, TENANT_A, ObjectId::new(), &browser().key).unwrap();
                    key
                });
                assert!(peer_auth_key_is_well_formed(&key));
                for path in [dir.path().join(DIR_NAME), key_path(&cfg, TENANT_A).unwrap()] {
                    assert!(!ts::protected(&ts::sddl(&path)), "{}", path.display());
                }
                assert_eq!(
                    said.matches("ROOMLERD_PEER_AUTH_SEAL=0").count(),
                    1,
                    "{said}"
                );
            });
        }

        /// A run that is neither elevated nor SYSTEM seals nothing: a
        /// per-user install's folder keeps what it had.
        #[test]
        fn a_run_that_is_not_elevated_changes_nothing() {
            if !ts::elevated_or_skip("a_run_that_is_not_elevated_changes_nothing") {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_in(&dir);
            let folder = dir.path().join(DIR_NAME);
            std::fs::create_dir(&folder).unwrap();
            assert!(ts::icacls(
                &folder,
                &["/grant", &format!("{GUESTS}:(OI)(CI)(M)")]
            ));
            let as_found = ts::sddl(&folder);
            kept_as(Keeping::Plain(Plain::NotElevated), || {
                assert!(peer_auth_key_is_well_formed(&hello_key(&cfg, TENANT_A)));
                assert_eq!(
                    remember(&cfg, TENANT_A, ObjectId::new(), &browser().key),
                    Ok(Pinned::Now)
                );
            });
            assert_eq!(ts::sddl(&folder), as_found, "the folder is as it was");
            for path in [
                key_path(&cfg, TENANT_A).unwrap(),
                pins_path(&cfg, TENANT_A).unwrap(),
            ] {
                assert!(!ts::protected(&ts::sddl(&path)), "{}", path.display());
            }
            assert!(named(dir.path(), "peer-auth.").is_empty(), "nothing moved");
        }

        /// A run that is neither elevated nor SYSTEM, at a key an elevated run
        /// sealed: no key, the cause and the remedy said once, nothing deleted.
        #[test]
        fn a_run_that_is_not_elevated_says_why_a_sealed_key_is_not_offered() {
            if !ts::elevated_or_skip(
                "a_run_that_is_not_elevated_says_why_a_sealed_key_is_not_offered",
            ) {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_in(&dir);
            let key_file = key_path(&cfg, TENANT_A).unwrap();
            let folder = Folder::of(&cfg, Keeping::Sealed).unwrap();
            load_or_make(&folder, &file_name(&key_file)).unwrap();
            let bytes = std::fs::read(&key_file).unwrap();
            // Closed to this token as the seal closes it to a person's own:
            // SYSTEM alone may read it.
            ts::set_dacl(&key_file, "D:P(A;;FA;;;SY)");
            let (keys, said) = kept_as(Keeping::Plain(Plain::NotElevated), || {
                logged(|| (hello_key(&cfg, TENANT_A), hello_key(&cfg, TENANT_A)))
            });
            ts::set_dacl(&key_file, "D:P(A;;FA;;;SY)(A;;FA;;;BA)");
            assert_eq!(keys, (String::new(), String::new()));
            assert_eq!(said.matches("is closed to this run").count(), 1, "{said}");
            assert!(said.contains("Start roomlerd elevated again"), "{said}");
            assert_eq!(std::fs::read(&key_file).unwrap(), bytes, "nothing deleted");
        }

        /// A junction at `peer-auth` is never followed: moved aside, and what
        /// it led to is left as it was.
        #[test]
        fn a_link_at_the_folder_is_never_followed() {
            if !ts::elevated_or_skip("a_link_at_the_folder_is_never_followed") {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_in(&dir);
            let elsewhere = dir.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            std::fs::write(elsewhere.join("kept.txt"), b"untouched").unwrap();
            let as_found = ts::sddl(&elsewhere);
            ts::junction(&dir.path().join(DIR_NAME), &elsewhere);
            kept_as(Keeping::Sealed, || {
                assert!(peer_auth_key_is_well_formed(&hello_key(&cfg, TENANT_A)));
            });
            assert_sealed(&dir.path().join(DIR_NAME), true);
            assert_eq!(ts::sddl(&elsewhere), as_found);
            assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 1);
        }
    }
}
