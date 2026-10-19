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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use base64::Engine;
    use bson::oid::ObjectId;
    use roomler_ai_remote_control::signaling::ClientMsg;
    use roomler_ai_remote_control::vault::{
        LeaseRefusal, VaultActivityKind, VaultSecretMeta, lease_limits,
    };
    use tokio::sync::{mpsc, oneshot};
    use tracing::{debug, info, warn};
    use zeroize::Zeroizing;

    use super::key;

    /// How long a request waits for the vault's answer. A server without the
    /// vault mounted never answers one.
    pub const ANSWER_DEADLINE: Duration = Duration::from_secs(15);

    /// P3-4e-2 — how soon a request whose card waits for a person is asked
    /// again, and how seldom at most: each ask is a decision the vault
    /// records, so the pause doubles to its ceiling.
    const APPROVAL_FIRST_PAUSE: Duration = Duration::from_secs(2);
    const APPROVAL_MAX_PAUSE: Duration = Duration::from_secs(15);

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
        /// P3-4c-3 — asked for ahead (`vault_request`): a use tool asking
        /// for the same secret, mode and audience takes it, and no call lets
        /// it go. It ends at its expiry, its revocation or the run's end.
        pub ahead: bool,
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
        /// P3-4c-3 — what it was asked for toward, and whether ahead.
        pub audience: String,
        pub ahead: bool,
        /// When the server says it ends, in milliseconds of its clock.
        pub expires_at_ms: i64,
    }

    /// Why a request got no lease.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LeaseError {
        /// The vault refused, in one word: a `LeaseRefusal`'s, or a newer
        /// server's this build cannot name, which is a refusal all the same.
        Refused(String),
        /// P3-4e — the vault's policies allow this only with a person's yes,
        /// and its card waits in the session's room: the approval's id.
        /// [`Client::request_until_approved`] asks again while it waits.
        AwaitingApproval(String),
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
        /// P3-4c-2 — files a tool wrote with this lease's value: removed
        /// whenever the lease is let go, however that happens.
        files: Vec<std::path::PathBuf>,
        /// P3-5b-2 — what a tool serves with this lease's value (a proxy
        /// endpoint): cancelled whenever the lease is let go.
        stops: Vec<tokio_util::sync::CancellationToken>,
    }

    /// Let go of leases already taken out of the map: each value is zeroed as
    /// it drops, every file written with it is removed, and everything served
    /// with it is stopped. Answers how many.
    fn let_go(gone: Vec<Held>) -> usize {
        let n = gone.len();
        for h in gone {
            for s in &h.stops {
                s.cancel();
            }
            for f in &h.files {
                match std::fs::remove_file(f) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => warn!(
                        lease = %h.info.lease_id, path = %f.display(), %e,
                        "vault: a file written with a lease's value was not removed"
                    ),
                }
            }
        }
        n
    }

    /// P3-4b — what a session's role may list: metadata only, and whether
    /// the vault's bound left some out.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Listing {
        pub secrets: Vec<VaultSecretMeta>,
        pub more: bool,
    }

    /// What a request waits for, by kind.
    enum Waiting {
        Lease {
            ask: LeaseAsk,
            reply: oneshot::Sender<Result<LeaseInfo, LeaseError>>,
        },
        List {
            reply: oneshot::Sender<Result<Listing, LeaseError>>,
        },
        Describe {
            reply: oneshot::Sender<Result<VaultSecretMeta, LeaseError>>,
        },
    }

    impl Waiting {
        /// Answer whichever request this is with `why`.
        fn fail(self, why: LeaseError) {
            match self {
                Self::Lease { reply, .. } => {
                    let _ = reply.send(Err(why));
                }
                Self::List { reply } => {
                    let _ = reply.send(Err(why));
                }
                Self::Describe { reply } => {
                    let _ = reply.send(Err(why));
                }
            }
        }
    }

    /// One daemon's leases: the process has one ([`CLIENT`]), a test makes
    /// its own.
    #[derive(Default)]
    pub struct Client {
        outbound: Mutex<Option<mpsc::Sender<ClientMsg>>>,
        /// P3-4b — whether that connection takes leases: a session's vault
        /// tools are offered only then.
        takes: AtomicBool,
        /// P3-4d-2 — whether the device's owner has it report what it does
        /// with its leases (`vault_activity_log`).
        reports: AtomicBool,
        pending: Mutex<HashMap<String, Waiting>>,
        held: Mutex<HashMap<String, Held>>,
    }

    /// P3-4d-2 — one use of leased values, as the device reports it
    /// (`rc:vault.activity`): never a value.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Report {
        pub session_id: ObjectId,
        pub fence: u64,
        pub kind: VaultActivityKind,
        /// The leases whose values were used.
        pub lease_ids: Vec<String>,
        /// `exec`: the program's bare name; `render`: the file's.
        pub target: String,
        /// `exec`: how the command ended; `None` when it was killed.
        pub exit_code: Option<i32>,
        pub timed_out: bool,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forgets one request when its wait ends, however it ends.
    struct Forget<'a> {
        client: &'a Client,
        req_id: &'a str,
    }

    impl Drop for Forget<'_> {
        fn drop(&mut self) {
            lock(&self.client.pending).remove(self.req_id);
        }
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    fn new_req_id() -> String {
        format!("{:016x}", rand::random::<u64>())
    }

    impl Client {
        /// The primary connection's outbound queue, from its hello on, and
        /// whether that connection takes leases.
        pub fn connected(&self, tx: mpsc::Sender<ClientMsg>, takes: bool) {
            *lock(&self.outbound) = Some(tx);
            self.takes.store(takes, Ordering::Relaxed);
        }

        /// P3-4b — whether the primary connection takes leases: what offers a
        /// session its vault tools.
        pub fn takes_leases(&self) -> bool {
            self.takes.load(Ordering::Relaxed)
        }

        /// P3-4d-2 — whether to report what this daemon does with its leases:
        /// the device's own `vault_activity_log`, set at each primary hello.
        pub fn set_reports(&self, on: bool) {
            self.reports.store(on, Ordering::Relaxed);
        }

        /// P3-4d-2 — tell the vault this daemon used leased values
        /// (`rc:vault.activity`), where the device's owner has it report;
        /// nothing otherwise. Fire-and-forget: a full or closed connection
        /// drops the report rather than hold the call that made it. Answers
        /// whether it was queued.
        pub fn report(&self, r: Report) -> bool {
            if !self.reports.load(Ordering::Relaxed) || r.lease_ids.is_empty() {
                return false;
            }
            let Some(tx) = lock(&self.outbound).clone() else {
                return false;
            };
            let frame = ClientMsg::VaultActivity {
                session_id: r.session_id,
                fence: r.fence,
                kind: r.kind.wire().into(),
                lease_ids: r.lease_ids,
                target: r.target,
                exit_code: r.exit_code,
                timed_out: r.timed_out,
            };
            match tx.try_send(frame) {
                Ok(()) => true,
                Err(e) => {
                    debug!(%e, "vault: an activity report was dropped");
                    false
                }
            }
        }

        /// Send `frame` as request `req_id`, waiting as `waiting`.
        fn send(&self, req_id: &str, frame: ClientMsg, waiting: Waiting) -> Result<(), LeaseError> {
            let Some(tx) = lock(&self.outbound).clone() else {
                return Err(LeaseError::NotConnected);
            };
            lock(&self.pending).insert(req_id.to_string(), waiting);
            if tx.try_send(frame).is_err() {
                lock(&self.pending).remove(req_id);
                return Err(LeaseError::NotConnected);
            }
            Ok(())
        }

        /// Wait `deadline` for request `req_id`'s answer.
        async fn wait<T>(
            &self,
            req_id: &str,
            answer: oneshot::Receiver<Result<T, LeaseError>>,
            deadline: Duration,
        ) -> Result<T, LeaseError> {
            // P3-4b — however the wait ends, its request waits no longer:
            // answered, past its deadline, or its asker gone (a cancelled
            // tool call drops this future), so nothing is left behind for a
            // server that never answers.
            let _forget = Forget {
                client: self,
                req_id,
            };
            match tokio::time::timeout(deadline, answer).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) | Err(_) => Err(LeaseError::NoAnswer),
            }
        }

        /// The requests waiting for an answer.
        #[cfg(test)]
        pub(super) fn waiting(&self) -> usize {
            lock(&self.pending).len()
        }

        /// Ask for a lease, and wait [`ANSWER_DEADLINE`] for the answer.
        pub async fn request(&self, ask: LeaseAsk) -> Result<LeaseInfo, LeaseError> {
            self.request_within(ask, ANSWER_DEADLINE).await
        }

        /// P3-4e-2 — ask for a lease, and while the vault answers that its
        /// card waits for a person, ask again: 2 s later, then twice as long
        /// each time up to 15 s, until a lease, any other answer, or `window`
        /// since the first wait (a card's own: `APPROVAL_ANSWER_SECS`). A card
        /// still waiting then is the answer. A caller that stops waiting (a
        /// cancelled tool call drops this future) stops the asking with it.
        pub async fn request_until_approved(
            &self,
            ask: LeaseAsk,
            window: Duration,
        ) -> Result<LeaseInfo, LeaseError> {
            let mut since = None;
            let mut pause = APPROVAL_FIRST_PAUSE;
            loop {
                match self.request(ask.clone()).await {
                    Err(LeaseError::AwaitingApproval(id)) => {
                        let since = *since.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() + pause > window {
                            return Err(LeaseError::AwaitingApproval(id));
                        }
                        tokio::time::sleep(pause).await;
                        pause = (pause * 2).min(APPROVAL_MAX_PAUSE);
                    }
                    other => return other,
                }
            }
        }

        /// Ask for a lease, and wait `deadline` for the answer.
        pub async fn request_within(
            &self,
            ask: LeaseAsk,
            deadline: Duration,
        ) -> Result<LeaseInfo, LeaseError> {
            let req_id = new_req_id();
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
            self.send(&req_id, frame, Waiting::Lease { ask, reply })?;
            self.wait(&req_id, answer, deadline).await
        }

        /// P3-4b — what the session's role may list under `prefix`.
        pub async fn list(
            &self,
            session_id: ObjectId,
            fence: u64,
            prefix: &str,
        ) -> Result<Listing, LeaseError> {
            self.list_within(session_id, fence, prefix, ANSWER_DEADLINE)
                .await
        }

        /// P3-4b — [`Self::list`], waiting `deadline`.
        pub async fn list_within(
            &self,
            session_id: ObjectId,
            fence: u64,
            prefix: &str,
            deadline: Duration,
        ) -> Result<Listing, LeaseError> {
            let req_id = new_req_id();
            let frame = ClientMsg::VaultList {
                req_id: req_id.clone(),
                session_id,
                fence,
                prefix: prefix.to_string(),
            };
            let (reply, answer) = oneshot::channel();
            self.send(&req_id, frame, Waiting::List { reply })?;
            self.wait(&req_id, answer, deadline).await
        }

        /// P3-4b — one secret's metadata, by its path or RRN.
        pub async fn describe(
            &self,
            session_id: ObjectId,
            fence: u64,
            secret: &str,
        ) -> Result<VaultSecretMeta, LeaseError> {
            let req_id = new_req_id();
            let frame = ClientMsg::VaultDescribe {
                req_id: req_id.clone(),
                session_id,
                fence,
                secret: secret.to_string(),
            };
            let (reply, answer) = oneshot::channel();
            self.send(&req_id, frame, Waiting::Describe { reply })?;
            self.wait(&req_id, answer, ANSWER_DEADLINE).await
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
            let (ask, reply) = match lock(&self.pending).remove(req_id) {
                Some(Waiting::Lease { ask, reply }) => (ask, reply),
                // An answer of the wrong kind answers nothing it was asked.
                Some(other) => return other.fail(LeaseError::NoAnswer),
                None => {
                    debug!(req_id, "vault: a lease nobody here waits for — not opened");
                    return;
                }
            };
            let opened = base64::engine::general_purpose::STANDARD
                .decode(sealed)
                .ok()
                .and_then(|b| key().unseal(&b).ok())
                .map(Zeroizing::new);
            let Some(value) = opened else {
                let _ = reply.send(Err(LeaseError::Unopenable));
                return;
            };
            let left =
                (expires_at_ms - now_ms()).clamp(0, i64::from(lease_limits::MAX_TTL_SECS) * 1000);
            let until = Instant::now() + Duration::from_millis(u64::try_from(left).unwrap_or(0));
            let info = LeaseInfo {
                lease_id: lease_id.to_string(),
                session_id: ask.session_id,
                fence: ask.fence,
                secret: ask.secret,
                field: ask.field,
                n,
                mode: ask.mode,
                audience: ask.audience,
                ahead: ask.ahead,
                expires_at_ms,
            };
            lock(&self.held).insert(
                lease_id.to_string(),
                Held {
                    info: info.clone(),
                    until,
                    value,
                    files: Vec::new(),
                    stops: Vec::new(),
                },
            );
            if reply.send(Ok(info)).is_err() {
                // Its asker stopped waiting: nothing will use it.
                let gone = lock(&self.held).remove(lease_id);
                let_go(gone.into_iter().collect());
            }
        }

        /// `rc:vault.refused`: answer the request, whatever it asked, in the
        /// vault's word — P3-4e: `awaiting_approval` with the approval it
        /// names.
        pub fn on_refused(&self, req_id: &str, reason: &str, approval_id: Option<&str>) {
            if let Some(w) = lock(&self.pending).remove(req_id) {
                w.fail(if reason == LeaseRefusal::AwaitingApproval.wire() {
                    LeaseError::AwaitingApproval(approval_id.unwrap_or_default().to_string())
                } else {
                    LeaseError::Refused(reason.to_string())
                });
            }
        }

        /// P3-4b — `rc:vault.listed`.
        pub fn on_listed(&self, req_id: &str, secrets: Vec<VaultSecretMeta>, more: bool) {
            match lock(&self.pending).remove(req_id) {
                Some(Waiting::List { reply }) => {
                    let _ = reply.send(Ok(Listing { secrets, more }));
                }
                Some(other) => other.fail(LeaseError::NoAnswer),
                None => debug!(req_id, "vault: a listing nobody here waits for"),
            }
        }

        /// P3-4b — `rc:vault.described`.
        pub fn on_described(&self, req_id: &str, secret: VaultSecretMeta) {
            match lock(&self.pending).remove(req_id) {
                Some(Waiting::Describe { reply }) => {
                    let _ = reply.send(Ok(secret));
                }
                Some(other) => other.fail(LeaseError::NoAnswer),
                None => debug!(req_id, "vault: a description nobody here waits for"),
            }
        }

        /// `rc:vault.revoke`: let go of every lease it names, whatever the
        /// word. An id this daemon does not hold is ignored: a hello is told
        /// again of what an earlier process held. Answers how many went.
        pub fn on_revoke(&self, lease_ids: &[String], reason: &str) -> usize {
            let dropped = let_go(self.take_ids(lease_ids));
            if dropped > 0 {
                info!(dropped, reason, "vault: leases taken back");
            }
            dropped
        }

        /// P3-4c — a tool is done with these leases: let them go now, not at
        /// their expiry. Answers how many it held.
        pub fn release(&self, lease_ids: &[String]) -> usize {
            let_go(self.take_ids(lease_ids))
        }

        /// P3-4c-2 — `path` was written with `lease_id`'s value: it is removed
        /// when the lease is let go, however that happens. `false` when the
        /// lease is not held (any more): the caller removes the file itself.
        pub fn attach(&self, lease_id: &str, path: &std::path::Path) -> bool {
            match lock(&self.held).get_mut(lease_id) {
                Some(h) => {
                    h.files.push(path.to_path_buf());
                    true
                }
                None => false,
            }
        }

        /// P3-5b-2 — `stop` is cancelled when `lease_id` is let go, however
        /// that happens: what a tool serves with the lease's value (a proxy
        /// endpoint and its connections) ends with it. `false` when the
        /// lease is not held (any more): the caller stops itself.
        pub fn stop_with(&self, lease_id: &str, stop: tokio_util::sync::CancellationToken) -> bool {
            match lock(&self.held).get_mut(lease_id) {
                Some(h) => {
                    h.stops.push(stop);
                    true
                }
                None => false,
            }
        }

        /// P3-4c-3 — a lease asked for ahead for `ask`'s run, secret, field,
        /// mode and audience, with at least `at_least` left: the one held
        /// longest. A lease a call asked for itself is never found: that
        /// call lets it go when it ends. (`ask.ttl_secs` and `ask.ahead` are
        /// not read.)
        pub fn find_ahead(&self, ask: &LeaseAsk, at_least: Duration) -> Option<String> {
            self.prune();
            let now = Instant::now();
            lock(&self.held)
                .iter()
                .filter(|(_, h)| {
                    let i = &h.info;
                    i.ahead
                        && i.session_id == ask.session_id
                        && i.fence == ask.fence
                        && i.secret == ask.secret
                        && i.field == ask.field
                        && i.mode == ask.mode
                        && i.audience == ask.audience
                        && h.until.saturating_duration_since(now) >= at_least
                })
                .max_by_key(|(_, h)| h.until)
                .map(|(id, _)| id.clone())
        }

        /// P3-4c-3 — how long a held lease has left by this daemon's clock.
        pub fn left(&self, lease_id: &str) -> Option<Duration> {
            let now = Instant::now();
            lock(&self.held)
                .get(lease_id)
                .map(|h| h.until.saturating_duration_since(now))
        }

        /// The run of `session` at `fence` ended here: its leases go with it.
        pub fn drop_run(&self, session: ObjectId, fence: u64) -> usize {
            let_go(self.take(|h| h.info.session_id == session && h.info.fence == fence))
        }

        /// Let go of every lease past its time.
        pub fn prune(&self) -> usize {
            let now = Instant::now();
            let_go(self.take(|h| h.until <= now))
        }

        /// The held leases `lease_ids` names, taken out of the map.
        fn take_ids(&self, lease_ids: &[String]) -> Vec<Held> {
            let mut held = lock(&self.held);
            lease_ids
                .iter()
                .filter_map(|id| held.remove(id.as_str()))
                .collect()
        }

        /// Every held lease `f` picks, taken out of the map.
        fn take(&self, f: impl Fn(&Held) -> bool) -> Vec<Held> {
            let mut held = lock(&self.held);
            let ids: Vec<String> = held
                .iter()
                .filter(|(_, h)| f(h))
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| held.remove(id)).collect()
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

    /// The primary connection's outbound queue, at each of its hellos,
    /// whether it takes leases, and (P3-4d-2) whether the device reports what
    /// it does with them; the first also starts letting go of expired leases
    /// in the background.
    pub fn on_connected(tx: mpsc::Sender<ClientMsg>, takes: bool, reports: bool) {
        CLIENT.connected(tx, takes);
        CLIENT.set_reports(reports);
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
    pub fn on_refused(req_id: &str, reason: &str, approval_id: Option<&str>) {
        CLIENT.on_refused(req_id, reason, approval_id);
    }

    /// The daemon's [`Client::on_revoke`].
    pub fn on_revoke(lease_ids: &[String], reason: &str) {
        CLIENT.on_revoke(lease_ids, reason);
    }

    /// The daemon's [`Client::on_listed`].
    pub fn on_listed(req_id: &str, secrets: Vec<VaultSecretMeta>, more: bool) {
        CLIENT.on_listed(req_id, secrets, more);
    }

    /// The daemon's [`Client::on_described`].
    pub fn on_described(req_id: &str, secret: VaultSecretMeta) {
        CLIENT.on_described(req_id, secret);
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
        use roomler_ai_remote_control::vault::{VaultActivityKind, VaultSecretMeta, lease_limits};
        use tokio::sync::mpsc;

        use super::super::{Client, LeaseAsk, LeaseError, LeaseInfo, Listing, Report, hello_key};

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
                ahead: false,
            }
        }

        fn connected() -> (Arc<Client>, mpsc::Receiver<ClientMsg>) {
            let c = Arc::new(Client::default());
            let (tx, rx) = mpsc::channel(8);
            c.connected(tx, true);
            (c, rx)
        }

        fn meta(path: &str) -> VaultSecretMeta {
            VaultSecretMeta {
                rrn: format!("rrn:grox:secret:{path}"),
                path: path.into(),
                kind: "static".into(),
                description: String::new(),
                fields: vec!["value".into()],
                audiences: Vec::new(),
                exec_allow: Vec::new(),
                tags: Vec::new(),
                disabled: false,
            }
        }

        /// P3-4b — a listing and a description go out on the connection and
        /// come back to their askers, never a value among them.
        #[tokio::test]
        async fn a_listing_and_a_description_come_back_to_their_askers() {
            let (c, mut rx) = connected();
            assert!(c.takes_leases());
            let sid = ObjectId::new();
            let lister = {
                let c = c.clone();
                tokio::spawn(async move { c.list(sid, 3, "db/").await })
            };
            let req_id = match rx.recv().await.unwrap() {
                ClientMsg::VaultList {
                    req_id,
                    session_id,
                    fence,
                    prefix,
                } => {
                    assert_eq!((session_id, fence, prefix.as_str()), (sid, 3, "db/"));
                    req_id
                }
                other => panic!("{other:?}"),
            };
            c.on_listed(&req_id, vec![meta("db/main")], true);
            assert_eq!(
                lister.await.unwrap().unwrap(),
                Listing {
                    secrets: vec![meta("db/main")],
                    more: true,
                }
            );
            let describer = {
                let c = c.clone();
                tokio::spawn(async move { c.describe(sid, 3, "db/main").await })
            };
            let req_id = match rx.recv().await.unwrap() {
                ClientMsg::VaultDescribe { req_id, secret, .. } => {
                    assert_eq!(secret, "db/main");
                    req_id
                }
                other => panic!("{other:?}"),
            };
            c.on_described(&req_id, meta("db/main"));
            assert_eq!(describer.await.unwrap().unwrap(), meta("db/main"));
        }

        /// A refusal answers whichever request it names, and an answer of the
        /// wrong kind answers nothing that was asked: no lease is held for a
        /// listing.
        #[tokio::test]
        async fn a_refusal_answers_any_request_and_a_wrong_answer_none() {
            let (c, mut rx) = connected();
            let lister = {
                let c = c.clone();
                tokio::spawn(async move { c.list(ObjectId::new(), 1, "").await })
            };
            let req_id = match rx.recv().await.unwrap() {
                ClientMsg::VaultList { req_id, .. } => req_id,
                other => panic!("{other:?}"),
            };
            c.on_refused(&req_id, "no_role", None);
            assert_eq!(
                lister.await.unwrap(),
                Err(LeaseError::Refused("no_role".into()))
            );
            let asker = {
                let c = c.clone();
                tokio::spawn(async move { c.request(ask(ObjectId::new(), 1)).await })
            };
            let req_id = req_id_of(rx.recv().await.unwrap());
            c.on_listed(&req_id, vec![meta("db/main")], false);
            assert_eq!(asker.await.unwrap(), Err(LeaseError::NoAnswer));
            assert!(c.held().is_empty());
        }

        fn req_id_of(frame: ClientMsg) -> String {
            match frame {
                ClientMsg::VaultLease { req_id, .. } => req_id,
                other => panic!("{other:?}"),
            }
        }

        /// P3-4e-2 — the next lease request on the connection, within a
        /// minute of the test's paused clock: an ask that never comes fails
        /// the test rather than hanging it.
        async fn next_ask(rx: &mut mpsc::Receiver<ClientMsg>) -> String {
            let frame = tokio::time::timeout(Duration::from_secs(60), rx.recv())
                .await
                .expect("a lease request within a minute")
                .expect("the connection is open");
            req_id_of(frame)
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
            c.on_refused(&req_id_of(rx.recv().await.unwrap()), "denied", None);
            assert_eq!(
                asker.await.unwrap(),
                Err(LeaseError::Refused("denied".into()))
            );
            assert!(c.held().is_empty());
        }

        /// P3-4e-2 — a lease whose card waits for a person is asked for
        /// again, 2 s and then 4 s later, each time as a request of its own,
        /// until the yes leases it.
        #[tokio::test(start_paused = true)]
        async fn a_lease_a_card_waits_on_is_asked_again_until_a_yes() {
            let (c, mut rx) = connected();
            let sid = ObjectId::new();
            let asker = {
                let c = c.clone();
                tokio::spawn(async move {
                    c.request_until_approved(ask(sid, 1), Duration::from_secs(600))
                        .await
                })
            };
            let first = next_ask(&mut rx).await;
            let t0 = tokio::time::Instant::now();
            c.on_refused(&first, "awaiting_approval", Some("A1"));
            let second = next_ask(&mut rx).await;
            assert_ne!(second, first, "a request of its own");
            assert_eq!(t0.elapsed().as_secs(), 2);
            let t1 = tokio::time::Instant::now();
            c.on_refused(&second, "awaiting_approval", Some("A1"));
            let third = next_ask(&mut rx).await;
            assert_eq!(t1.elapsed().as_secs(), 4);
            c.on_leased(
                &third,
                "L1",
                1,
                &sealed_for_us(b"canary-vault-value"),
                in_ms(60_000),
            );
            assert_eq!(asker.await.unwrap().unwrap().lease_id, "L1");
        }

        /// P3-4e-2 — a no after a wait is the answer, and nothing asks again.
        #[tokio::test(start_paused = true)]
        async fn a_no_after_a_wait_is_the_answer() {
            let (c, mut rx) = connected();
            let asker = {
                let c = c.clone();
                tokio::spawn(async move {
                    c.request_until_approved(ask(ObjectId::new(), 1), Duration::from_secs(600))
                        .await
                })
            };
            let first = next_ask(&mut rx).await;
            c.on_refused(&first, "awaiting_approval", Some("A1"));
            c.on_refused(&next_ask(&mut rx).await, "denied", None);
            assert_eq!(
                asker.await.unwrap(),
                Err(LeaseError::Refused("denied".into()))
            );
            assert!(rx.try_recv().is_err(), "nothing asked again");
        }

        /// P3-4e-2 — a card nobody answers ends the wait at its window: asked
        /// at 0, 2, 6, 14 and 29 s (the pause doubling to 15 s), and the
        /// card still waiting is the answer, naming it.
        #[tokio::test(start_paused = true)]
        async fn a_card_nobody_answers_ends_the_wait_at_its_window() {
            let (c, mut rx) = connected();
            let t0 = tokio::time::Instant::now();
            let asker = {
                let c = c.clone();
                tokio::spawn(async move {
                    c.request_until_approved(ask(ObjectId::new(), 1), Duration::from_secs(30))
                        .await
                })
            };
            let mut at = Vec::new();
            for _ in 0..5 {
                let req_id = next_ask(&mut rx).await;
                at.push(t0.elapsed().as_secs());
                c.on_refused(&req_id, "awaiting_approval", Some("A1"));
            }
            assert_eq!(
                asker.await.unwrap(),
                Err(LeaseError::AwaitingApproval("A1".into()))
            );
            assert_eq!(at, [0, 2, 6, 14, 29]);
            assert!(rx.try_recv().is_err(), "nothing asked past the window");
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

        /// P3-4b — an asker that stops waiting (a cancelled tool call drops
        /// its future) leaves nothing waiting, and the answer that comes
        /// after opens nothing.
        #[tokio::test]
        async fn an_asker_that_stops_waiting_leaves_nothing_waiting() {
            let (c, mut rx) = connected();
            let asker = {
                let c = c.clone();
                tokio::spawn(async move { c.request(ask(ObjectId::new(), 1)).await })
            };
            let req_id = req_id_of(rx.recv().await.unwrap());
            assert_eq!(c.waiting(), 1);
            asker.abort();
            let _ = asker.await;
            assert_eq!(c.waiting(), 0, "the dropped wait forgot its request");
            c.on_leased(
                &req_id,
                "after",
                1,
                &sealed_for_us(b"canary-vault-value"),
                in_ms(60_000),
            );
            assert!(
                c.held().is_empty(),
                "an answer after its asker left is not held"
            );
            let lister = {
                let c = c.clone();
                tokio::spawn(async move { c.list(ObjectId::new(), 1, "").await })
            };
            match rx.recv().await.unwrap() {
                ClientMsg::VaultList { .. } => {}
                other => panic!("{other:?}"),
            }
            assert_eq!(c.waiting(), 1);
            lister.abort();
            let _ = lister.await;
            assert_eq!(c.waiting(), 0, "a listing's dropped wait, the same");
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

        /// P3-4c-2 — a file written with a lease's value goes whenever the
        /// lease does: at a revocation, a release, the run's end and its
        /// expiry. A lease no longer held takes no file.
        #[tokio::test]
        async fn a_leases_files_go_whenever_it_does() {
            let dir = tempfile::tempdir().unwrap();
            let file = |name: &str| {
                let p = dir.path().join(name);
                std::fs::write(&p, b"x").unwrap();
                p
            };
            let (c, mut rx) = connected();
            let sid = ObjectId::new();

            lease(&c, &mut rx, "R", sid, 1, in_ms(60_000)).await;
            let revoked = file("revoked");
            assert!(c.attach("R", &revoked));
            c.on_revoke(&["R".to_string()], "value_changed");
            assert!(!revoked.exists(), "a revocation takes the file");

            lease(&c, &mut rx, "S", sid, 1, in_ms(60_000)).await;
            let released = file("released");
            assert!(c.attach("S", &released));
            c.release(&["S".to_string()]);
            assert!(!released.exists(), "a release takes the file");

            lease(&c, &mut rx, "D", sid, 2, in_ms(60_000)).await;
            let ended = file("run-ended");
            assert!(c.attach("D", &ended));
            c.drop_run(sid, 2);
            assert!(!ended.exists(), "the run's end takes the file");

            lease(&c, &mut rx, "E", sid, 3, in_ms(1)).await;
            let expired = file("expired");
            assert!(c.attach("E", &expired));
            tokio::time::sleep(Duration::from_millis(20)).await;
            c.prune();
            assert!(!expired.exists(), "the expiry takes the file");

            assert!(
                !c.attach("gone", &dir.path().join("never")),
                "a lease not held takes no file"
            );
        }

        /// P3-5b-2 — what is served with a lease's value stops whenever the
        /// lease goes: at a revocation, a release, the run's end and its
        /// expiry. A lease no longer held takes no stop, and one still held
        /// stops nothing.
        #[tokio::test]
        async fn what_is_served_with_a_lease_stops_whenever_it_goes() {
            use tokio_util::sync::CancellationToken;
            let (c, mut rx) = connected();
            let sid = ObjectId::new();
            let served = |id: &str| {
                let stop = CancellationToken::new();
                assert!(c.stop_with(id, stop.clone()), "{id} is held");
                stop
            };

            lease(&c, &mut rx, "R", sid, 1, in_ms(60_000)).await;
            let revoked = served("R");
            lease(&c, &mut rx, "K", sid, 1, in_ms(60_000)).await;
            let kept = served("K");
            c.on_revoke(&["R".to_string()], "value_changed");
            assert!(revoked.is_cancelled(), "a revocation stops it");
            assert!(!kept.is_cancelled(), "another lease's stays");

            lease(&c, &mut rx, "S", sid, 1, in_ms(60_000)).await;
            let released = served("S");
            c.release(&["S".to_string()]);
            assert!(released.is_cancelled(), "a release stops it");

            lease(&c, &mut rx, "D", sid, 2, in_ms(60_000)).await;
            let ended = served("D");
            c.drop_run(sid, 2);
            assert!(ended.is_cancelled(), "the run's end stops it");

            lease(&c, &mut rx, "E", sid, 3, in_ms(1)).await;
            let expired = served("E");
            tokio::time::sleep(Duration::from_millis(20)).await;
            c.prune();
            assert!(expired.is_cancelled(), "the expiry stops it");

            assert!(
                !c.stop_with("gone", CancellationToken::new()),
                "a lease not held takes no stop"
            );
            assert!(!kept.is_cancelled());
        }

        /// P3-4c-3 — a lease asked for ahead is found only for exactly what
        /// it was asked for, while it has the time a call needs, and a lease
        /// a call asked for itself is never found.
        #[tokio::test]
        async fn a_lease_asked_for_ahead_is_found_only_for_what_it_was_asked_for() {
            let (c, mut rx) = connected();
            let sid = ObjectId::new();
            let exec_for = |audience: &str, ahead: bool| LeaseAsk {
                mode: "exec".into(),
                audience: audience.into(),
                ahead,
                ..ask(sid, 1)
            };
            for (id, ahead, ms) in [("A", true, 60_000), ("B", false, 120_000)] {
                let asker = {
                    let (c, a) = (c.clone(), exec_for("psql", ahead));
                    tokio::spawn(async move { c.request(a).await })
                };
                let req_id = req_id_of(rx.recv().await.unwrap());
                c.on_leased(&req_id, id, 1, &sealed_for_us(b"v"), in_ms(ms));
                asker.await.unwrap().unwrap();
            }
            let held = c.held();
            let a = held.iter().find(|i| i.lease_id == "A").unwrap();
            assert_eq!((a.audience.as_str(), a.ahead), ("psql", true));
            let soon = Duration::from_secs(10);
            assert_eq!(
                c.find_ahead(&exec_for("psql", false), soon).as_deref(),
                Some("A"),
                "the one asked for ahead, never B, which a call asked for itself"
            );
            assert!(c.left("A").unwrap() > Duration::from_secs(50));
            let elsewhere = [
                exec_for("kubectl", true),
                LeaseAsk {
                    mode: "file".into(),
                    ..exec_for("psql", true)
                },
                LeaseAsk {
                    field: "password".into(),
                    ..exec_for("psql", true)
                },
                LeaseAsk {
                    fence: 2,
                    ..exec_for("psql", true)
                },
                LeaseAsk {
                    session_id: ObjectId::new(),
                    ..exec_for("psql", true)
                },
                LeaseAsk {
                    secret: "db/other".into(),
                    ..exec_for("psql", true)
                },
            ];
            for other in elsewhere {
                assert_eq!(c.find_ahead(&other, soon), None, "{other:?}");
            }
            assert_eq!(
                c.find_ahead(&exec_for("psql", true), Duration::from_secs(120)),
                None,
                "a lease ending before the call would is not taken"
            );
            c.drop_run(sid, 1);
            assert_eq!(c.find_ahead(&exec_for("psql", true), soon), None);
        }

        /// P3-4d-2 — a report goes out on the connection only where the
        /// device's owner has it report, names its leases and never a value,
        /// and is dropped rather than wait on a full connection.
        #[tokio::test]
        async fn a_report_goes_out_only_where_the_owner_has_it_report() {
            let c = Arc::new(Client::default());
            let (tx, mut rx) = mpsc::channel(1);
            c.connected(tx, true);
            let sid = ObjectId::new();
            let report = || Report {
                session_id: sid,
                fence: 2,
                kind: VaultActivityKind::Exec,
                lease_ids: vec!["L1".into()],
                target: "psql".into(),
                exit_code: Some(0),
                timed_out: false,
            };
            assert!(!c.report(report()), "off by default: nothing sent");
            assert!(rx.try_recv().is_err());

            c.set_reports(true);
            assert!(c.report(report()));
            match rx.try_recv().unwrap() {
                ClientMsg::VaultActivity {
                    session_id,
                    fence,
                    kind,
                    lease_ids,
                    target,
                    exit_code,
                    timed_out,
                } => {
                    assert_eq!((session_id, fence), (sid, 2));
                    assert_eq!((kind.as_str(), target.as_str()), ("exec", "psql"));
                    assert_eq!(lease_ids, ["L1"]);
                    assert_eq!((exit_code, timed_out), (Some(0), false));
                }
                other => panic!("expected a report, got {other:?}"),
            }
            assert!(c.report(report()));
            assert!(
                !c.report(report()),
                "the connection holds one: the next is dropped, not waited for"
            );
            let _ = rx.try_recv();
            let mut no_lease = report();
            no_lease.lease_ids.clear();
            assert!(!c.report(no_lease), "no lease used, nothing to report");
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
