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
//! ⚠️ The file holds a PRIVATE key, and gets the config's own protection. On
//! Windows the machine-global config directory grants only SYSTEM and
//! Administrators, inherited by what is made inside it; a per-user install's
//! is the person's own profile. On Unix the directory is made 0700 and the
//! file 0600, whoever runs the daemon.

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
/// by none.
pub fn sign_outbound(
    offers: &PendingOffers,
    config_path: &Path,
    tenant_id: &str,
    agent_id: &str,
    msg: &mut roomler_ai_remote_control::signaling::ClientMsg,
) {
    if let roomler_ai_remote_control::signaling::ClientMsg::SdpAnswer {
        session_id,
        sdp,
        peer_auth,
        ..
    } = msg
    {
        *peer_auth = sign_answer(offers, config_path, tenant_id, agent_id, *session_id, sdp);
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
    match device_key(config_path, tenant_id) {
        Ok(key) => key.public_b64(),
        Err(why) => {
            tracing::warn!(tenant = %tenant_id, %why, "peer-auth: no device key this run, so the hello offers none");
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
    let key = Arc::new(load_or_make(&path)?);
    keys.insert(path, key.clone());
    Ok(key)
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

/// Read the key at `path`, or make one and write it there. A key is returned
/// only once it is on disk as the next start will read it.
fn load_or_make(path: &Path) -> Result<DeviceKey, String> {
    match std::fs::read(path) {
        Ok(bytes) => match parse(&bytes) {
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Present but unreadable (permissions, I/O): never overwrite a file
        // that may hold a good key this process merely cannot open.
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
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
    write_private(path, &bytes)?;
    let written =
        std::fs::read(path).map_err(|e| format!("reading back {}: {e}", path.display()))?;
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
/// readable by the daemon's own identity alone.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("restricting {}: {e}", dir.display()))?;
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
        .map_err(|e| format!("making {}: {e}", tmp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| format!("placing {}: {e}", path.display()))?;
    // The rename itself survives a power cut only once the directory is
    // synced. Best effort: the key is already whole on disk either way.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
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

    #[test]
    fn a_key_is_made_once_and_read_back_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&config_in(&dir), TENANT_A).unwrap();
        assert!(!path.exists());
        let made = load_or_make(&path).unwrap().public_b64();
        assert!(path.exists());
        assert!(peer_auth_key_is_well_formed(&made), "{made}");
        // A second read — a restart — finds the same key, and makes none.
        assert_eq!(load_or_make(&path).unwrap().public_b64(), made);
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
            load_or_make(&a).unwrap().public_b64(),
            load_or_make(&b).unwrap().public_b64()
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
        let key = load_or_make(&path).unwrap().public_b64();
        assert!(peer_auth_key_is_well_formed(&key));
        assert_eq!(
            load_or_make(&path).unwrap().public_b64(),
            key,
            "the replacement stuck"
        );
        std::fs::write(&path, b"\0\0\0\0").unwrap();
        assert!(
            load_or_make(&path).is_ok(),
            "NUL-filled, as an unclean shutdown leaves"
        );
    }

    /// A rollback must not destroy the newer agent's key: its file is left
    /// as it was, and this build offers none.
    #[test]
    fn a_newer_key_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&config_in(&dir), TENANT_A).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let newer = br#"{"version":2,"alg":"ecdsa-p256-sha256","pkcs8":"from the future"}"#;
        std::fs::write(&path, newer).unwrap();
        assert!(load_or_make(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), newer);
    }

    /// A key that cannot be written is not offered: `peer-auth` is a FILE
    /// here, so the directory cannot be made.
    #[test]
    fn a_key_that_cannot_be_kept_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DIR_NAME), b"in the way").unwrap();
        assert_eq!(hello_key(&config_in(&dir), TENANT_A), "");
        assert!(
            device_key(&config_in(&dir), TENANT_A).is_err(),
            "and nothing was cached"
        );
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
        load_or_make(&path).unwrap();
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
        PeerAuthOffer { nonce: nonce() }
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
        };
        sign_outbound(&offers, &cfg, TENANT_A, AGENT, &mut msg);
        let ClientMsg::SdpAnswer {
            peer_auth: Some(signed),
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

        // Nothing noted for this one: the worker's claim is dropped, not kept.
        let mut msg = ClientMsg::SdpAnswer {
            session_id: ObjectId::new(),
            sdp: ANSWER_SDP.into(),
            extauth_mac: None,
            peer_auth: forged(),
        };
        sign_outbound(&offers, &cfg, TENANT_A, AGENT, &mut msg);
        assert!(matches!(
            msg,
            ClientMsg::SdpAnswer {
                peer_auth: None,
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
}
