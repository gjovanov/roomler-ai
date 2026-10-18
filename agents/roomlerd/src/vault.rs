// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-3 — the device's half of the vault: the key a leased value is
//! sealed to.
//!
//! The vault (`crates/modules/vault`) sends a value it leases to a session on
//! this device as a sealed box for a key only this daemon holds. That key is
//! made here: an X25519 key, made at the first hello that asks for it while
//! the device's own `vault_enabled` is on, kept in this process's memory and
//! written nowhere. Its public half rides the PRIMARY enrollment's hello
//! (`AgentHello::vault_key`), and the server stores it on every hello.
//!
//! Memory, not the config where the overlay and SSH keys live: those are
//! identities a peer pins, and a pin must outlive a restart. Nothing pins this
//! key. A leased value lives in daemon memory for its lease only, so a restart
//! ends every lease whatever becomes of the key, and a key kept on disk would
//! add one thing only: a way for whoever reads the config, or a backup of it,
//! to open a sealed value captured earlier. A new key at each start leaves
//! nothing on disk to read.
//!
//! ⚠️ Never the WireGuard key, and never on a secondary org's connection: that
//! org's server would learn a key another org's server holds too, which ties
//! the device's enrollments together (`docs/multi-org.md`).
//!
//! A build without the `vault` feature makes no key, its hello carries none,
//! and the vault leases it nothing.

/// The public half of this daemon's vault key for a hello, or the empty
/// string.
///
/// Empty unless the device's owner switched `vault_enabled` on, the
/// connection is the primary enrollment's, and this build has the `vault`
/// feature. The first call that passes makes the key; every later one gives
/// the same public half until the process ends.
pub fn hello_key(vault_enabled: bool, is_primary: bool) -> String {
    if !vault_enabled || !is_primary {
        return String::new();
    }
    public_key()
}

#[cfg(feature = "vault")]
static KEY: std::sync::OnceLock<crypto_box::SecretKey> = std::sync::OnceLock::new();

/// The process's key, made on first use.
#[cfg(feature = "vault")]
fn key() -> &'static crypto_box::SecretKey {
    KEY.get_or_init(|| {
        tracing::info!("vault: made this run's vault key (memory only)");
        crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng)
    })
}

#[cfg(feature = "vault")]
fn public_key() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(key().public_key().as_bytes())
}

#[cfg(not(feature = "vault"))]
fn public_key() -> String {
    String::new()
}

/// P3-4a — whether this daemon takes leases on a connection: a build that
/// has the `vault` feature and runs agent sessions, its owner's
/// `vault_enabled`, and the primary enrollment's connection. The vault
/// answers a lease only from a connection that advertises `vault` AND whose
/// hello carried a key, so the word follows the key ([`hello_key`]).
pub fn takes_leases(vault_enabled: bool, is_primary: bool) -> bool {
    cfg!(all(feature = "vault", hive_host)) && vault_enabled && is_primary
}

/// P3-4a — `vault` in a connection's `rpc` list, on exactly the connections
/// that [`takes_leases`]. The hello and every re-announcement apply it: the
/// server replaces its stored caps with each.
pub fn apply_word(rpc: &mut Vec<String>, on: bool) {
    let word = roomler_ai_remote_control::models::RpcCap::Vault.wire();
    rpc.retain(|w| w != word);
    if on {
        rpc.push(word.to_string());
    }
}

#[cfg(feature = "vault")]
pub use client::*;

/// P3-4a — the device's lease client: ask the vault for a value over the
/// primary connection (`rc:vault.lease`), hold what it seals to this
/// daemon's key in this process's memory, and let it go at its expiry, at
/// `rc:vault.revoke`, or when its session's run ends here.
///
/// ⚠️ A value never leaves this process: [`Client::with_value`] lends it to a
/// closure and nothing returns it, and it is zeroed when it is let go.
///
/// ⚠️ A lease is held until the server's expiry as measured on this clock,
/// and never longer than the longest lease the vault grants
/// (`lease_limits::MAX_TTL_SECS`): a device whose clock runs behind cannot
/// hold a value past its lease by more than that.
#[cfg(feature = "vault")]
mod client {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use base64::Engine;
    use bson::oid::ObjectId;
    use roomler_ai_remote_control::signaling::ClientMsg;
    use roomler_ai_remote_control::vault::lease_limits;
    use tokio::sync::{mpsc, oneshot};
    use tracing::{debug, info};
    use zeroize::Zeroizing;

    use super::key;

    /// How long a request waits for the vault's answer. A server without the
    /// vault mounted never answers one.
    pub const ANSWER_DEADLINE: Duration = Duration::from_secs(15);

    /// How often the daemon lets go of what has expired, though nothing asked.
    const PRUNE_EVERY: Duration = Duration::from_secs(5);

    /// What a session asks for.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LeaseAsk {
        pub session_id: ObjectId,
        /// The fence the session runs at here: the vault believes a request
        /// only at its record's fence.
        pub fence: u64,
        /// A path, or an RRN of this organization.
        pub secret: String,
        /// Empty for a static secret's one field.
        pub field: String,
        pub mode: String,
        pub audience: String,
        pub ttl_secs: u32,
    }

    /// A lease this daemon holds: what it is, never its value.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LeaseInfo {
        pub lease_id: String,
        pub session_id: ObjectId,
        pub fence: u64,
        pub secret: String,
        pub field: String,
        pub n: i64,
        pub mode: String,
        /// When the server says it ends, in milliseconds of its clock.
        pub expires_at_ms: i64,
    }

    /// Why a request got no lease.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LeaseError {
        /// The vault refused, in one word: a `LeaseRefusal`'s, or a newer
        /// server's this build cannot name, which is a refusal all the same.
        Refused(String),
        /// No connection to ask on.
        NotConnected,
        /// No answer within the deadline.
        NoAnswer,
        /// The answer did not open with this daemon's key.
        Unopenable,
    }

    struct Held {
        info: LeaseInfo,
        until: Instant,
        value: Zeroizing<Vec<u8>>,
    }

    struct Pending {
        ask: LeaseAsk,
        reply: oneshot::Sender<Result<LeaseInfo, LeaseError>>,
    }

    /// One daemon's leases: the process has one ([`CLIENT`]), a test makes
    /// its own.
    #[derive(Default)]
    pub struct Client {
        outbound: Mutex<Option<mpsc::Sender<ClientMsg>>>,
        pending: Mutex<HashMap<String, Pending>>,
        held: Mutex<HashMap<String, Held>>,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    impl Client {
        /// The primary connection's outbound queue, from its hello on.
        pub fn connected(&self, tx: mpsc::Sender<ClientMsg>) {
            *lock(&self.outbound) = Some(tx);
        }

        /// Ask for a lease, and wait [`ANSWER_DEADLINE`] for the answer.
        pub async fn request(&self, ask: LeaseAsk) -> Result<LeaseInfo, LeaseError> {
            self.request_within(ask, ANSWER_DEADLINE).await
        }

        /// Ask for a lease, and wait `deadline` for the answer.
        pub async fn request_within(
            &self,
            ask: LeaseAsk,
            deadline: Duration,
        ) -> Result<LeaseInfo, LeaseError> {
            let Some(tx) = lock(&self.outbound).clone() else {
                return Err(LeaseError::NotConnected);
            };
            let req_id = format!("{:016x}", rand::random::<u64>());
            let frame = ClientMsg::VaultLease {
                req_id: req_id.clone(),
                session_id: ask.session_id,
                fence: ask.fence,
                secret: ask.secret.clone(),
                field: ask.field.clone(),
                mode: ask.mode.clone(),
                audience: ask.audience.clone(),
                ttl_secs: ask.ttl_secs,
            };
            let (reply, answer) = oneshot::channel();
            lock(&self.pending).insert(req_id.clone(), Pending { ask, reply });
            if tx.try_send(frame).is_err() {
                lock(&self.pending).remove(&req_id);
                return Err(LeaseError::NotConnected);
            }
            match tokio::time::timeout(deadline, answer).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) | Err(_) => {
                    lock(&self.pending).remove(&req_id);
                    Err(LeaseError::NoAnswer)
                }
            }
        }

        /// `rc:vault.leased`: open it, hold it, and answer the request.
        pub fn on_leased(
            &self,
            req_id: &str,
            lease_id: &str,
            n: i64,
            sealed: &str,
            expires_at_ms: i64,
        ) {
            let Some(p) = lock(&self.pending).remove(req_id) else {
                debug!(req_id, "vault: a lease nobody here waits for — not opened");
                return;
            };
            let opened = base64::engine::general_purpose::STANDARD
                .decode(sealed)
                .ok()
                .and_then(|b| key().unseal(&b).ok())
                .map(Zeroizing::new);
            let Some(value) = opened else {
                let _ = p.reply.send(Err(LeaseError::Unopenable));
                return;
            };
            let left =
                (expires_at_ms - now_ms()).clamp(0, i64::from(lease_limits::MAX_TTL_SECS) * 1000);
            let until = Instant::now() + Duration::from_millis(u64::try_from(left).unwrap_or(0));
            let info = LeaseInfo {
                lease_id: lease_id.to_string(),
                session_id: p.ask.session_id,
                fence: p.ask.fence,
                secret: p.ask.secret,
                field: p.ask.field,
                n,
                mode: p.ask.mode,
                expires_at_ms,
            };
            lock(&self.held).insert(
                lease_id.to_string(),
                Held {
                    info: info.clone(),
                    until,
                    value,
                },
            );
            if p.reply.send(Ok(info)).is_err() {
                // Its asker stopped waiting: nothing will use it.
                lock(&self.held).remove(lease_id);
            }
        }

        /// `rc:vault.refused`: answer the request in the vault's word.
        pub fn on_refused(&self, req_id: &str, reason: &str) {
            if let Some(p) = lock(&self.pending).remove(req_id) {
                let _ = p.reply.send(Err(LeaseError::Refused(reason.to_string())));
            }
        }

        /// `rc:vault.revoke`: let go of every lease it names, whatever the
        /// word. An id this daemon does not hold is ignored: a hello is told
        /// again of what an earlier process held. Answers how many went.
        pub fn on_revoke(&self, lease_ids: &[String], reason: &str) -> usize {
            let dropped = {
                let mut held = lock(&self.held);
                lease_ids
                    .iter()
                    .filter(|id| held.remove(id.as_str()).is_some())
                    .count()
            };
            if dropped > 0 {
                info!(dropped, reason, "vault: leases taken back");
            }
            dropped
        }

        /// The run of `session` at `fence` ended here: its leases go with it.
        pub fn drop_run(&self, session: ObjectId, fence: u64) -> usize {
            let mut held = lock(&self.held);
            let before = held.len();
            held.retain(|_, h| !(h.info.session_id == session && h.info.fence == fence));
            before - held.len()
        }

        /// Let go of every lease past its time.
        pub fn prune(&self) -> usize {
            let now = Instant::now();
            let mut held = lock(&self.held);
            let before = held.len();
            held.retain(|_, h| h.until > now);
            before - held.len()
        }

        /// Use a held value without it leaving this daemon: `None` once the
        /// lease has expired or been taken back.
        pub fn with_value<R>(&self, lease_id: &str, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
            self.prune();
            lock(&self.held).get(lease_id).map(|h| f(&h.value))
        }

        /// What this daemon holds, never a value.
        pub fn held(&self) -> Vec<LeaseInfo> {
            self.prune();
            lock(&self.held).values().map(|h| h.info.clone()).collect()
        }

        /// How long this daemon holds a lease from now.
        #[cfg(test)]
        pub(super) fn held_for(&self, lease_id: &str) -> Option<Duration> {
            lock(&self.held)
                .get(lease_id)
                .map(|h| h.until.saturating_duration_since(Instant::now()))
        }
    }

    /// The daemon's own leases.
    pub static CLIENT: LazyLock<Client> = LazyLock::new(Client::default);

    static PRUNER: OnceLock<()> = OnceLock::new();

    /// The primary connection's outbound queue, at each of its hellos; the
    /// first also starts letting go of expired leases in the background.
    pub fn on_connected(tx: mpsc::Sender<ClientMsg>) {
        CLIENT.connected(tx);
        PRUNER.get_or_init(|| {
            tokio::spawn(async {
                loop {
                    tokio::time::sleep(PRUNE_EVERY).await;
                    CLIENT.prune();
                }
            });
        });
    }

    /// The daemon's [`Client::on_leased`].
    pub fn on_leased(req_id: &str, lease_id: &str, n: i64, sealed: &str, expires_at_ms: i64) {
        CLIENT.on_leased(req_id, lease_id, n, sealed, expires_at_ms);
    }

    /// The daemon's [`Client::on_refused`].
    pub fn on_refused(req_id: &str, reason: &str) {
        CLIENT.on_refused(req_id, reason);
    }

    /// The daemon's [`Client::on_revoke`].
    pub fn on_revoke(lease_ids: &[String], reason: &str) {
        CLIENT.on_revoke(lease_ids, reason);
    }

    /// The daemon's [`Client::drop_run`]: hive's supervisor calls it when a
    /// run stops being live here.
    pub fn drop_run(session: ObjectId, fence: u64) {
        let dropped = CLIENT.drop_run(session, fence);
        if dropped > 0 {
            info!(%session, fence, dropped, "vault: a run's leases let go with it");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off, or on a secondary org's connection, there is no key to give.
    #[test]
    fn no_key_while_off_or_on_a_secondary_connection() {
        assert_eq!(hello_key(false, true), "");
        assert_eq!(hello_key(false, false), "");
        assert_eq!(
            hello_key(true, false),
            "",
            "a secondary org never learns it"
        );
    }

    /// A build without the feature has no key to give, even switched on.
    #[cfg(not(feature = "vault"))]
    #[test]
    fn a_build_without_the_vault_has_no_key() {
        assert_eq!(hello_key(true, true), "");
    }

    /// On the primary connection: a key the server will store, and the same
    /// one at every hello this process says.
    #[cfg(feature = "vault")]
    #[test]
    fn the_primary_hello_carries_one_key_for_the_life_of_the_process() {
        use roomler_ai_remote_control::signaling::vault_key_is_well_formed;
        let first = hello_key(true, true);
        assert!(vault_key_is_well_formed(&first), "{first}");
        assert_eq!(
            hello_key(true, true),
            first,
            "a reconnect says the same key"
        );
    }

    /// What the vault seals to the published half, this process opens; a box
    /// for any other key it does not. Sealed here the way the server will,
    /// with the same crate.
    #[cfg(feature = "vault")]
    #[test]
    fn a_box_sealed_to_the_published_half_opens_here_and_no_other_does() {
        use base64::Engine;
        let rng = &mut crypto_box::aead::OsRng;
        let published = base64::engine::general_purpose::STANDARD
            .decode(hello_key(true, true))
            .unwrap();
        let to_us = crypto_box::PublicKey::from_slice(&published).unwrap();
        let sealed = to_us.seal(rng, b"canary-vault-value").unwrap();
        assert_eq!(key().unseal(&sealed).unwrap(), b"canary-vault-value");

        let other = crypto_box::SecretKey::generate(rng).public_key();
        let elsewhere = other.seal(rng, b"canary-vault-value").unwrap();
        assert!(key().unseal(&elsewhere).is_err());
        let mut tampered = sealed;
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(key().unseal(&tampered).is_err());
    }

    /// P3-4a — the word travels with the key: on the connections that take
    /// leases, once, and on no other.
    #[test]
    fn the_vault_word_follows_the_key() {
        let mut rpc = vec!["exec".to_string(), "vault".to_string()];
        apply_word(&mut rpc, false);
        assert_eq!(rpc, ["exec"]);
        apply_word(&mut rpc, true);
        apply_word(&mut rpc, true);
        assert_eq!(rpc, ["exec", "vault"], "once, however often applied");
        assert!(!takes_leases(false, true), "off");
        assert!(
            !takes_leases(true, false),
            "never a secondary org's connection"
        );
        assert_eq!(
            takes_leases(true, true),
            cfg!(all(feature = "vault", hive_host))
        );
    }

    #[cfg(feature = "vault")]
    mod client {
        use std::sync::Arc;
        use std::time::Duration;

        use base64::Engine;
        use bson::oid::ObjectId;
        use roomler_ai_remote_control::signaling::ClientMsg;
        use roomler_ai_remote_control::vault::lease_limits;
        use tokio::sync::mpsc;

        use super::super::{Client, LeaseAsk, LeaseError, LeaseInfo, hello_key};

        fn in_ms(ms: i64) -> i64 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis();
            i64::try_from(now).unwrap() + ms
        }

        /// A value sealed the way the vault seals it: to the half this
        /// daemon's hello publishes.
        fn sealed_for_us(value: &[u8]) -> String {
            let b64 = base64::engine::general_purpose::STANDARD;
            let to_us =
                crypto_box::PublicKey::from_slice(&b64.decode(hello_key(true, true)).unwrap())
                    .unwrap();
            b64.encode(to_us.seal(&mut crypto_box::aead::OsRng, value).unwrap())
        }

        fn ask(session: ObjectId, fence: u64) -> LeaseAsk {
            LeaseAsk {
                session_id: session,
                fence,
                secret: "db/main".into(),
                field: String::new(),
                mode: "env".into(),
                audience: String::new(),
                ttl_secs: 60,
            }
        }

        fn connected() -> (Arc<Client>, mpsc::Receiver<ClientMsg>) {
            let c = Arc::new(Client::default());
            let (tx, rx) = mpsc::channel(8);
            c.connected(tx);
            (c, rx)
        }

        fn req_id_of(frame: ClientMsg) -> String {
            match frame {
                ClientMsg::VaultLease { req_id, .. } => req_id,
                other => panic!("{other:?}"),
            }
        }

        /// Lease `lease_id` for (`session`, `fence`), the vault answering
        /// with `expires_at_ms`.
        async fn lease(
            c: &Arc<Client>,
            rx: &mut mpsc::Receiver<ClientMsg>,
            lease_id: &str,
            session: ObjectId,
            fence: u64,
            expires_at_ms: i64,
        ) -> LeaseInfo {
            let asker = {
                let c = c.clone();
                tokio::spawn(async move { c.request(ask(session, fence)).await })
            };
            let req_id = req_id_of(rx.recv().await.unwrap());
            c.on_leased(
                &req_id,
                lease_id,
                1,
                &sealed_for_us(b"canary-vault-value"),
                expires_at_ms,
            );
            asker.await.unwrap().unwrap()
        }

        /// A request goes out on the connection, bounded as the vault bounds
        /// it; the answer opens with this daemon's key and is held.
        #[tokio::test]
        async fn a_lease_is_asked_for_on_the_connection_opened_and_held() {
            let (c, mut rx) = connected();
            let sid = ObjectId::new();
            let asker = {
                let c = c.clone();
                tokio::spawn(async move { c.request(ask(sid, 3)).await })
            };
            let req_id = match rx.recv().await.unwrap() {
                ClientMsg::VaultLease {
                    req_id,
                    session_id,
                    fence,
                    secret,
                    mode,
                    ttl_secs,
                    ..
                } => {
                    assert_eq!(
                        (session_id, fence, secret.as_str(), mode.as_str(), ttl_secs),
                        (sid, 3, "db/main", "env", 60)
                    );
                    assert!(req_id.len() <= lease_limits::MAX_REQ_ID_LEN);
                    req_id
                }
                other => panic!("{other:?}"),
            };
            c.on_leased(
                &req_id,
                "L1",
                7,
                &sealed_for_us(b"canary-vault-value"),
                in_ms(60_000),
            );
            let info = asker.await.unwrap().unwrap();
            assert_eq!(
                (info.lease_id.as_str(), info.n, info.session_id, info.fence),
                ("L1", 7, sid, 3)
            );
            assert_eq!(
                c.with_value("L1", |v| v.to_vec()).unwrap(),
                b"canary-vault-value"
            );
            assert_eq!(c.held(), [info]);
        }

        /// A refusal is answered in the vault's word, and nothing is held.
        #[tokio::test]
        async fn a_refusal_answers_in_the_vaults_word() {
            let (c, mut rx) = connected();
            let asker = {
                let c = c.clone();
                tokio::spawn(async move { c.request(ask(ObjectId::new(), 1)).await })
            };
            c.on_refused(&req_id_of(rx.recv().await.unwrap()), "denied");
            assert_eq!(
                asker.await.unwrap(),
                Err(LeaseError::Refused("denied".into()))
            );
            assert!(c.held().is_empty());
        }

        /// Without a connection there is no request; without an answer in
        /// time there is no lease, and an answer that comes late opens
        /// nothing.
        #[tokio::test]
        async fn without_a_connection_or_an_answer_there_is_no_lease() {
            let lone = Client::default();
            assert_eq!(
                lone.request(ask(ObjectId::new(), 1)).await,
                Err(LeaseError::NotConnected)
            );
            let (c, mut rx) = connected();
            assert_eq!(
                c.request_within(ask(ObjectId::new(), 1), Duration::from_millis(50))
                    .await,
                Err(LeaseError::NoAnswer)
            );
            let late = req_id_of(rx.try_recv().unwrap());
            c.on_leased(
                &late,
                "late",
                1,
                &sealed_for_us(b"canary-vault-value"),
                in_ms(60_000),
            );
            assert!(c.held().is_empty(), "a late answer is not held");
        }

        /// A box sealed to any other key does not open, and is not held.
        #[tokio::test]
        async fn a_box_for_another_key_is_unopenable() {
            let (c, mut rx) = connected();
            let asker = {
                let c = c.clone();
                tokio::spawn(async move { c.request(ask(ObjectId::new(), 1)).await })
            };
            let rng = &mut crypto_box::aead::OsRng;
            let elsewhere = crypto_box::SecretKey::generate(rng).public_key();
            let sealed = base64::engine::general_purpose::STANDARD
                .encode(elsewhere.seal(rng, b"canary-vault-value").unwrap());
            c.on_leased(
                &req_id_of(rx.recv().await.unwrap()),
                "L",
                1,
                &sealed,
                in_ms(60_000),
            );
            assert_eq!(asker.await.unwrap(), Err(LeaseError::Unopenable));
            assert!(c.held().is_empty());
        }

        /// A revocation lets go of what it names, and an id this daemon does
        /// not hold changes nothing.
        #[tokio::test]
        async fn a_revocation_lets_go_of_what_it_names_and_nothing_else() {
            let (c, mut rx) = connected();
            let sid = ObjectId::new();
            lease(&c, &mut rx, "L1", sid, 1, in_ms(60_000)).await;
            let kept = lease(&c, &mut rx, "L2", sid, 1, in_ms(60_000)).await;
            assert_eq!(
                c.on_revoke(
                    &["L1".into(), "an-earlier-process-held".into()],
                    "session_ended"
                ),
                1
            );
            assert!(c.with_value("L1", |_| ()).is_none());
            assert_eq!(c.held(), [kept]);
        }

        /// A run that ends takes its own leases, and only those: another
        /// fence of the session is another run.
        #[tokio::test]
        async fn a_run_that_ends_takes_only_its_own_leases() {
            let (c, mut rx) = connected();
            let (sid, other) = (ObjectId::new(), ObjectId::new());
            lease(&c, &mut rx, "L1", sid, 3, in_ms(60_000)).await;
            lease(&c, &mut rx, "L2", sid, 4, in_ms(60_000)).await;
            lease(&c, &mut rx, "L3", other, 3, in_ms(60_000)).await;
            assert_eq!(c.drop_run(sid, 3), 1);
            let mut left: Vec<String> = c.held().into_iter().map(|l| l.lease_id).collect();
            left.sort();
            assert_eq!(left, ["L2", "L3"]);
        }

        /// A lease is let go at its expiry, and held no longer than the
        /// longest lease the vault grants, whatever the server's clock says.
        #[tokio::test]
        async fn an_expired_lease_is_let_go_and_none_outlives_the_longest() {
            let (c, mut rx) = connected();
            let sid = ObjectId::new();
            lease(&c, &mut rx, "soon", sid, 1, in_ms(150)).await;
            assert!(c.with_value("soon", |_| ()).is_some());
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                c.with_value("soon", |_| ()).is_none(),
                "let go at its expiry"
            );
            lease(&c, &mut rx, "far", sid, 1, in_ms(10 * 24 * 3_600_000)).await;
            let held = c.held_for("far").unwrap();
            assert!(
                held <= Duration::from_secs(u64::from(lease_limits::MAX_TTL_SECS)),
                "{held:?}"
            );
        }
    }
}
