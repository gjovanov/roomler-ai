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
}
