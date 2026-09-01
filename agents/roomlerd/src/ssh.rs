// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Roomler SSH — the in-daemon SSH surface on this node's overlay address.
//!
//! # Where this is
//!
//! * **P1 — transport.** `tunnel_core::overlay::split_tun` diverts TCP for
//!   `<overlay ip>:<ssh_port>` into the daemon's userspace stack before the OS
//!   sees it, so the endpoint needs no socket bind, no firewall rule, and can
//!   coexist with an `sshd` that already holds `0.0.0.0:22`.
//! * **P2 — this file's server.** russh over that stream: publickey auth
//!   against [`ssh_authorized_keys`], and an `exec` channel routed through the
//!   daemon's existing [`crate::exec`] engine. PTY, SFTP and forwarding are
//!   refused with an explicit reason rather than left to hang.
//! * **P3a — the device half of authorization.** `rc:ssh.grant` carries a
//!   server-minted, single-use, short-lived authorization naming a roomler
//!   principal and an ephemeral public key; [`record_grant`] holds it and
//!   [`auth_publickey`] redeems it. The server half that decides *whether* to
//!   mint one (`SshPolicy`, `SSH_DEVICE`, the org kill-switch) is P3b.
//!
//! Built without the `ssh-server` feature the module still compiles and serves
//! the P1 banner+echo, which keeps the transport independently testable in a
//! build carrying no SSH code — and costs those builds nothing (russh measures
//! +1.86 MiB in `roomlerd`).
//!
//! # Two facts about identity, and the gap between them
//!
//! Anything arriving here has already been decrypted by WireGuard against a
//! peer key the coordination server put in our netmap, so the peer address is
//! the overlay address of a specific enrolled node in a specific org — not a
//! claim, a cryptographic fact. That is what the whole design rests on, and
//! what eventually lets roomler SSH have no key distribution at all.
//!
//! It is not *authorization*. An enrolled peer is authenticated, not entitled.
//! Two things close that gap, in this order:
//!
//! 1. A **server grant** ([`record_grant`]) names a roomler principal and one
//!    ephemeral key, is redeemed once, and dies in seconds. This is the path
//!    that will make key distribution unnecessary.
//! 2. [`ssh_authorized_keys`] — a device-owned list, empty by default, so
//!    `ssh_enabled` on its own grants nobody anything. It stays after P3 as
//!    the break-glass route for when the control plane is the broken thing,
//!    which is when a remote shell is wanted most.
//!
//! Grants are tried first and consumed; the list is the fallback.
//!
//! # What a session runs as
//!
//! Never something more privileged than was actually asked for — the rule
//! [`crate::exec::RunAs`] exists to enforce, applied to BOTH paths:
//!
//! * A **grant** carries the account mode from the device's server-side
//!   `SshPolicy`, which is authoritative for that session.
//! * A **key-list** session has no policy behind it, so it uses the
//!   device-owned `ssh_account_mode`. Unset — the default — means the session
//!   authenticates and then runs nothing, with the reason on stderr.
//!
//! That second rule is deliberate and was a fix: key-list sessions used to
//! fall back to the daemon's own identity, so listing a key quietly handed out
//! SYSTEM/root and a policy of `account_mode = console_user` was untrue for
//! that path. Authenticating-then-refusing is chosen over refusing the auth
//! because it can explain itself.
//!
//! [`ssh_authorized_keys`]: crate::config::AgentConfig::ssh_authorized_keys

use roomler_ai_remote_control::models::{SshActivityEvent, SshActivityKind};
use tracing::{debug, warn};

/// Process services a session depends on, threaded EXPLICITLY from the daemon
/// into the SSH server instead of discovered through process globals.
///
/// The broker is non-optional on purpose. "No broker ⇒ deny" used to be a doc
/// comment on a `static` (`consent::shared()`), and a doc comment is not a
/// property the compiler holds anyone to — a server that can be constructed at
/// all now provably has someone to ask. The struct exists (rather than a bare
/// broker parameter) so the NEXT thing a session needs — the P3c audit sink is
/// the known candidate — is one field here, not another parameter cascade.
#[derive(Clone)]
pub struct SessionServices {
    /// FR-55 — the activity counter, so a live SSH session holds the machine
    /// awake for as long as it lasts. Threaded here for the same reason the
    /// consent broker is: the server is built inside the overlay's TUN
    /// factory, far from where the keeper lives, and "the session was cut by
    /// an idle timer" must not be a state a server can be constructed into.
    ///
    /// `None` where no keeper exists, in which case nothing is held and
    /// behaviour is unchanged.
    ///
    /// ⚠️ Named `power_activity`, not `activity`: that name is already taken
    /// by the P8 [`ActivitySink`] which REPORTS what a session did. Two
    /// different meanings under one name is a trap.
    pub power_activity: Option<crate::power::ActivityCounter>,
    /// The operator-consent broker — the same instance the tray and the
    /// LocalAPI decide through, so an SSH prompt resolves from any of them.
    pub consent: crate::consent::ConsentBroker,
    /// FR-27 — the native prompt surface, threaded here for the same reason
    /// the broker is: an SSH server is built inside the overlay's TUN
    /// factory, far from `signaling::run`, and "the prompt reached no screen"
    /// must not be a state a server can be constructed into. Field-measured
    /// 2026-08-29 on NEO16: with only the companion path an SSH prompt from
    /// mars timed out on a desktop whose native panel had just served three
    /// exec prompts in a row.
    pub indicator: crate::indicator::ViewerIndicator,
    /// Where P8 activity reports go: this WS session's outbound queue.
    ///
    /// Scoped to the connection on purpose — a report belongs to the org
    /// whose control channel carried the grant, and a multi-org daemon must
    /// not leak one org's session activity onto another's WS. A reconnect
    /// builds a new sink; reports that raced the teardown are dropped, which
    /// is the right trade for a log line that must never block a session.
    pub activity: Option<ActivitySink>,
}

/// Fire-and-forget reporter for [`SshActivityKind`] events (P8).
///
/// Holds the gate as well as the channel so no call site has to remember to
/// check it: with `ssh_activity_log = false` the constructor returns `None`
/// and every `report` becomes a no-op at the type level.
#[derive(Clone)]
pub struct ActivitySink {
    tx: tokio::sync::mpsc::Sender<roomler_ai_remote_control::signaling::ClientMsg>,
}

impl ActivitySink {
    /// `None` when the device has not opted in — the default.
    pub fn new(
        cfg: &crate::config::AgentConfig,
        tx: tokio::sync::mpsc::Sender<roomler_ai_remote_control::signaling::ClientMsg>,
    ) -> Option<Self> {
        cfg.ssh_activity_log.then_some(Self { tx })
    }

    /// Report one event. Never blocks and never fails a session: a full
    /// outbound queue means the connection is already struggling, and losing
    /// a log line is strictly better than stalling the shell that produced it.
    pub fn report(
        &self,
        grant_id: Option<String>,
        caller: &str,
        kind: SshActivityKind,
        detail: Option<String>,
        exit_code: Option<i32>,
        allowed: bool,
    ) {
        use roomler_ai_remote_control::signaling::ClientMsg;

        let msg = ClientMsg::SshActivity {
            grant_id,
            caller: caller.to_string(),
            kind,
            detail: detail.map(|d| redact_and_cap(&d)),
            exit_code,
            allowed,
        };
        if self.tx.try_send(msg).is_err() {
            debug!("ssh: activity report dropped (outbound queue full or closed)");
        }
    }
}

/// Redact known secrets and bound the length BEFORE anything leaves the host.
///
/// A command line is operator-controlled text that we are about to ship off
/// the machine, so it gets the same treatment `exec` gives its output: the
/// registered-secret redactor (agent token, `Bearer …`, JWT-shaped strings),
/// then a hard cap. Truncation is MARKED — a silently shortened command would
/// read in the audit log as a different command from the one that ran.
fn redact_and_cap(detail: &str) -> String {
    let mut s = crate::exec::redactor().apply(detail);
    if s.chars().count() > SshActivityEvent::MAX_DETAIL {
        s = s.chars().take(SshActivityEvent::MAX_DETAIL).collect();
        s.push('…');
    }
    s
}

/// Decorate a TUN factory so TCP to `<overlay ip>:<ssh_port>` terminates in the
/// daemon instead of reaching the OS. Returns `inner` unchanged when the node
/// has not opted in, so the default path is byte-for-byte the old one.
///
/// The decoration happens per device build (i.e. per WS session), which is what
/// we want: a reconnect re-creates the netstack alongside the device it is
/// spliced into, and the previous one is torn down with it.
#[cfg(feature = "overlay-netstack")]
pub fn maybe_intercept(
    inner: tunnel_core::overlay::runtime::TunFactory,
    cfg: &crate::config::AgentConfig,
    services: SessionServices,
) -> tunnel_core::overlay::runtime::TunFactory {
    use std::sync::Arc;

    use tunnel_core::overlay::split_tun::{SplitTun, warn_if_os_listener};
    use tunnel_core::overlay::tun::TunIo;

    if !cfg.ssh_enabled {
        return inner;
    }
    let port = cfg.effective_ssh_port();
    // Resolve what we serve ONCE, here, rather than per connection: parsing the
    // host key and the authorized-key list is where an operator's typo shows
    // up, and it should surface at start-up in the daemon log, not as a silent
    // per-connection failure the caller sees as a dropped socket.
    let ctx = build_ctx(cfg, services);
    Box::new(move |ip, nm, mtu| {
        let ctx = ctx.clone();
        let dev = inner(ip, nm, mtu)?;
        // Loud about the one case where switching this on changes who answers
        // an address that already had a server: `devbox` (sshd bound to the
        // overlay IP) and the Linux boxes (sshd on `0.0.0.0:22`).
        warn_if_os_listener(ip, port);

        let split = SplitTun::wrap(dev, ip, crate::overlay::netmask_to_prefix(nm), mtu, port);

        // The accept loop must NOT hold a strong reference: `SplitTun` owns the
        // netstack, whose poll loop exits when the last handle drops. Upgrade
        // once to open the listener, then keep only the listener — which ends
        // by itself when the stack goes away, so the task cannot outlive the
        // session it belongs to.
        let weak = Arc::downgrade(&split);
        tokio::spawn(async move {
            let listener = {
                let Some(dev) = weak.upgrade() else { return };
                match dev.listen().await {
                    Ok(l) => l,
                    Err(e) => {
                        warn!(%e, port, "ssh: could not listen on the intercepted overlay port");
                        return;
                    }
                }
            };
            accept_loop(listener, port, ctx).await;
        });

        Ok(split as Arc<dyn TunIo>)
    })
}

/// Builds without the userspace stack cannot intercept anything; say so once
/// rather than leaving the operator to wonder why `ssh_enabled` did nothing.
#[cfg(not(feature = "overlay-netstack"))]
pub fn maybe_intercept(
    inner: tunnel_core::overlay::runtime::TunFactory,
    cfg: &crate::config::AgentConfig,
    _services: SessionServices,
) -> tunnel_core::overlay::runtime::TunFactory {
    if cfg.ssh_enabled {
        warn!(
            "ssh_enabled is set but this build lacks the `overlay-netstack` feature — \
             SSH is NOT being served on the overlay address"
        );
    }
    inner
}

/// Serve intercepted connections until the netstack goes away.
#[cfg(feature = "overlay-netstack")]
async fn accept_loop(
    mut listener: tunnel_core::overlay::netstack::NsListener,
    port: u16,
    ctx: ServeCtx,
) {
    use tracing::info;

    info!(port, "ssh: serving the intercepted overlay port");
    while let Some(stream) = listener.accept().await {
        tokio::spawn(serve_conn(stream, ctx.clone()));
    }
    info!(
        port,
        "ssh: intercepted port no longer served (session ended)"
    );
}

/// What a connection is served with, resolved once per session.
///
/// With the `ssh-server` feature this is the SSH server's configuration (host
/// key + the accepted public keys); without it, the P1 banner+echo, so the
/// transport stays independently testable in a build that carries no SSH code
/// at all.
#[cfg(all(feature = "overlay-netstack", feature = "ssh-server"))]
type ServeCtx = Option<std::sync::Arc<sshd::Ctx>>;
#[cfg(all(feature = "overlay-netstack", not(feature = "ssh-server")))]
type ServeCtx = ();

#[cfg(all(feature = "overlay-netstack", not(feature = "ssh-server")))]
fn build_ctx(cfg: &crate::config::AgentConfig, services: SessionServices) -> ServeCtx {
    let _ = (cfg, services);
    warn!(
        "ssh_enabled is on but this build lacks the `ssh-server` feature — \
         the intercepted port answers with the P1 echo, not SSH"
    );
}

/// P1 payload, kept for builds without `ssh-server`: prove the path, then get
/// out of the way. Echoes until EOF so a tester can confirm both directions.
#[cfg(all(feature = "overlay-netstack", not(feature = "ssh-server")))]
async fn serve_conn(mut stream: tunnel_core::overlay::netstack::NsTcpStream, _ctx: ServeCtx) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing::info;

    let peer = stream.peer_addr();
    info!(%peer, "ssh: intercepted connection accepted (P1 transport seam)");

    let banner = format!(
        "roomler-ssh P1: transport seam OK. You are {peer}, served in-process \
         with no OS socket. Echoing until EOF.\r\n"
    );
    if let Err(e) = stream.write_all(banner.as_bytes()).await {
        warn!(%peer, %e, "ssh: banner write failed");
        return;
    }

    let mut buf = [0u8; 2048];
    let mut echoed: u64 = 0;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = stream.write_all(&buf[..n]).await {
                    warn!(%peer, %e, echoed, "ssh: echo write failed");
                    return;
                }
                echoed += n as u64;
            }
            Err(e) => {
                warn!(%peer, %e, echoed, "ssh: read failed");
                return;
            }
        }
    }
    info!(%peer, echoed, "ssh: intercepted connection closed");
}

// ===========================================================================
// P2 — the SSH server.
// ===========================================================================

/// Mint a fresh ed25519 host identity, OpenSSH-encoded.
///
/// Ed25519 only, and not by accident: it is the algorithm every client since
/// OpenSSH 6.5 supports, it needs no parameter choices that can be got wrong,
/// and it lets the `rsa` feature stay off — which keeps a `0.10.0-rc`
/// pre-release out of the dependency graph and ~0.32 MiB out of the binary.
///
/// The seed comes from `rand::random`, i.e. the OS CSPRNG, and the key is
/// constructed from those bytes rather than through an RNG-generic API so this
/// never has to agree with russh about which `rand_core` generation is in the
/// tree.
/// The OpenSSH **public** half of this device's host key, for the server to
/// publish so callers can verify what they dialled instead of trusting it on
/// first use (P6).
///
/// Empty when there is nothing to prove — no key stored, or a build without
/// `ssh-server`. Deliberately NOT an error: a device with SSH switched off is
/// in a perfectly normal state, and the empty value already means "cannot
/// prove itself" on the wire.
///
/// Derived on every hello rather than cached, so rotating `ssh_host_key` in
/// the config and restarting is all it takes for the fleet view to follow —
/// a cache here would leave the server advertising a key the device no longer
/// holds, which is worse than advertising none.
#[cfg(feature = "ssh-server")]
pub fn host_public_key(cfg: &crate::config::AgentConfig) -> String {
    let Some(pem) = cfg.ssh_host_key.as_deref() else {
        return String::new();
    };
    match russh::keys::ssh_key::PrivateKey::from_openssh(pem) {
        Ok(k) => k.public_key().to_openssh().unwrap_or_default(),
        Err(e) => {
            // Same unreadable-key case `Ctx::build` refuses to serve on; say
            // so once here too, because the symptom a user sees is remote
            // (their client cannot verify the host) and the cause is local.
            warn!(%e, "ssh: stored host key is unreadable — publishing no host pubkey");
            String::new()
        }
    }
}

#[cfg(not(feature = "ssh-server"))]
pub fn host_public_key(_cfg: &crate::config::AgentConfig) -> String {
    String::new()
}

#[cfg(feature = "ssh-server")]
pub fn generate_host_key() -> anyhow::Result<String> {
    use russh::keys::ssh_key::LineEnding;
    use russh::keys::ssh_key::private::{Ed25519Keypair, PrivateKey};

    let seed: [u8; 32] = rand::random();
    let keypair = Ed25519Keypair::from_seed(&seed);
    let key = PrivateKey::from(keypair);
    Ok(key.to_openssh(LineEnding::LF)?.to_string())
}

#[cfg(feature = "ssh-server")]
mod sshd {
    use std::sync::Arc;
    use std::time::Duration;

    use roomler_ai_remote_control::models::SshActivityKind;
    use russh::keys::ssh_key::{self, PublicKey};
    use russh::server::{Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use tracing::{info, warn};

    use super::ActivitySink;

    /// Everything a session needs, resolved once when the overlay device is
    /// built rather than per connection.
    pub struct Ctx {
        pub config: Arc<russh::server::Config>,
        /// Keys allowed to authenticate. Empty is a valid, safe state: the
        /// transport answers, every authentication attempt fails, and the
        /// operator sees exactly why in the log.
        pub authorized: Vec<PublicKey>,
        /// What a key-list session runs as, resolved once from
        /// `ssh_account_mode`.
        ///
        /// `Err` when the operator has not chosen — which is the default, and
        /// which refuses. A key-list session carries no server policy, so
        /// there is nothing to infer an identity from; the old behaviour of
        /// quietly using the daemon's own identity meant listing a key handed
        /// out SYSTEM/root without saying so, and made a device policy of
        /// `account_mode = console_user` untrue for this path.
        pub key_list_run_as: Result<crate::exec::RunAs, String>,
        /// M5: refuse a SERVER GRANT that asks to run as the daemon identity
        /// (SYSTEM/root), resolved once from `ssh_max_privilege`.
        ///
        /// Every other gate on an SSH session is decided by the server. This
        /// one is decided here, so it is the only one that still holds when
        /// the server is the compromised thing: a control plane that mints
        /// `account_mode = daemon, consent_mode = auto` otherwise gets an
        /// unattended root shell on every device that answers.
        ///
        /// Default `false` — permissive, no behaviour change — because a
        /// device-side ceiling that arrives switched ON revokes a working
        /// configuration mid-roll, and the device you lose root SSH to is
        /// liable to be the one you needed it for. The start-up log says so
        /// on every device that has not chosen.
        pub deny_daemon_grants: bool,
        /// Daemon services a session depends on — the consent broker, threaded
        /// from `run_cmd` so it provably exists wherever a server does.
        pub services: super::SessionServices,
        /// The operator's forward allowlist, snapshotted at server start —
        /// the ONLY gate on `direct-tcpip` (P7b), since no server authorizes
        /// an SSH forward. See [`forward_allowed`].
        pub forward_acl: crate::tunnel::acl::AgentForwardAcl,
        /// Ceiling on concurrent forwards, process-wide (P7b).
        ///
        /// `exec` caps its concurrency; a forward is the other thing a session
        /// can start unboundedly, and `ssh -D` opens one channel per browser
        /// connection — hundreds of live sockets is a resource story the
        /// operator never agreed to when they allowlisted one destination.
        /// Refuses rather than queues, matching `exec`: a forward that opens
        /// two minutes late is worse than one that fails now and says so.
        pub forwards: Arc<tokio::sync::Semaphore>,
    }

    /// Process-wide ceiling on live `direct-tcpip` channels. Generous enough
    /// for `-D` against a browser (which is the heaviest legitimate use),
    /// small enough that a runaway client cannot exhaust the device's fds.
    pub(super) const MAX_CONCURRENT_FORWARDS: usize = 64;

    impl Ctx {
        /// Returns `None` when the node has no usable host key — the caller
        /// then serves nothing, which is the right failure: an SSH endpoint
        /// with an improvised identity is worse than no endpoint.
        pub fn build(
            cfg: &crate::config::AgentConfig,
            services: super::SessionServices,
        ) -> Option<Arc<Self>> {
            let pem = cfg.ssh_host_key.as_deref()?;
            let host_key = match ssh_key::PrivateKey::from_openssh(pem) {
                Ok(k) => k,
                Err(e) => {
                    warn!(%e, "ssh: the stored host key is unreadable — not serving SSH");
                    return None;
                }
            };
            let host_fingerprint = host_key
                .public_key()
                .fingerprint(ssh_key::HashAlg::Sha256)
                .to_string();

            let mut authorized = Vec::new();
            for (i, line) in cfg.ssh_authorized_keys.iter().enumerate() {
                match PublicKey::from_openssh(line) {
                    Ok(k) => authorized.push(k),
                    // Name the index, never the line: a malformed entry is
                    // still key material and does not belong in a log.
                    Err(e) => warn!(
                        index = i,
                        %e, "ssh: ignoring an unparseable ssh_authorized_keys entry"
                    ),
                }
            }
            if authorized.is_empty() {
                warn!(
                    "ssh: no usable entries in ssh_authorized_keys — the port will answer \
                     but every authentication attempt will be refused"
                );
            }

            let config = russh::server::Config {
                // A session left open by a dropped carrier should not pin
                // resources forever; ten minutes of silence ends it.
                inactivity_timeout: Some(Duration::from_secs(600)),
                // Slow down guessing without making a legitimate retry painful.
                auth_rejection_time: Duration::from_secs(2),
                auth_rejection_time_initial: Some(Duration::from_secs(0)),
                keys: vec![host_key],
                ..Default::default()
            };

            // Resolve the key-list identity ONCE, here, so a missing or
            // malformed `ssh_account_mode` is a start-up log line rather than
            // a per-session surprise.
            let key_list_run_as = match cfg.ssh_account_mode.as_deref() {
                Some(m) => parse_account_mode(m),
                None => Err(
                    "ssh_account_mode is not set on this device, so keys in ssh_authorized_keys \
                     may authenticate but cannot run anything. Set it to daemon, console_user or \
                     named:<account> to choose what those keys get."
                        .to_string(),
                ),
            };
            if let Err(e) = &key_list_run_as
                && !authorized.is_empty()
            {
                warn!(%e, "ssh: ssh_authorized_keys is populated but has no account mode");
            }

            // Unset is permissive (see `Ctx::deny_daemon_grants`), so the
            // device that has not chosen is the one worth saying it out loud
            // on — an operator reading this log should not have to know the
            // key exists to learn that it is off.
            let deny_daemon_grants = cfg.ssh_max_privilege.as_deref() == Some("console_user");
            if !deny_daemon_grants {
                warn!(
                    "ssh: ssh_max_privilege is not set to console_user, so a server grant asking \
                     for the daemon identity would run as SYSTEM/root here. Set it if this device \
                     should refuse that even from a control plane that asks."
                );
            }

            info!(
                fingerprint = %host_fingerprint,
                authorized_keys = authorized.len(),
                key_list_run_as = key_list_run_as.as_ref().map(|r| r.label()).unwrap_or_else(|_| "unset".into()),
                %deny_daemon_grants,
                "ssh: server ready"
            );
            Some(Arc::new(Self {
                config: Arc::new(config),
                authorized,
                key_list_run_as,
                deny_daemon_grants,
                services,
                forward_acl: cfg.forward_acl.clone(),
                forwards: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FORWARDS)),
            }))
        }
    }

    /// Everything a session is allowed to do, decided in ONE place — at
    /// authentication, from the server grant or the device-owned key list —
    /// and the only shape the request handlers consume.
    ///
    /// This type exists to kill a bug class, not to organise code: rc.419
    /// (key-list identity fell through to SYSTEM) and P5d (`consent_mode`
    /// destructured away) were both fields that crossed into a session and
    /// were dropped on the floor, silently. Its constructors destructure
    /// their inputs EXHAUSTIVELY — no `..`, and every field either flows into
    /// the type or is consumed against a written decision — so the next field
    /// added to a grant fails to compile until someone decides what a session
    /// does with it.
    pub struct ResolvedSessionPolicy {
        /// What the session runs as. `Err` = authenticate-then-refuse with
        /// this reason when something is asked to run — a bare auth failure
        /// cannot explain itself, which is why the refusal is deferred.
        pub run_as: Result<crate::exec::RunAs, String>,
        /// Sentinel the operator is prompted on before anything runs. `None`
        /// = no prompt: either the policy said an explicit `Auto`, or this is
        /// a key-list session — the break-glass path, deliberately exempt
        /// from a gate the control plane owns.
        pub consent_sentinel: Option<String>,
        /// Hard end of the session, enforced by disconnect. `None` =
        /// unbounded, which ONLY the key-list path produces — deliberately:
        /// the break-glass session must not die under whoever is fixing the
        /// control plane. A grant always carries a bound (0 clamps to the
        /// ceiling), making `ssh_limits::MAX_SESSION_SECS`'s "after which the
        /// agent closes it" true rather than aspirational.
        pub deadline: Option<tokio::time::Instant>,
        /// Who to attribute every action to, in every log line.
        pub caller: String,
        /// The grant this session redeemed, for P8 activity reports —
        /// correlating a reported action back to the server's own decision
        /// row in `ssh_audit`. `None` for a key-list session, which no grant
        /// backs and which therefore has nothing to correlate to.
        pub grant_id: Option<String>,
    }

    impl ResolvedSessionPolicy {
        /// Resolve a redeemed server grant. The grant is authoritative for
        /// everything a session may do; nothing the CLIENT said is consulted.
        ///
        /// `deny_daemon_grants` is the one exception, and it is the device's,
        /// not the client's — the M5 ceiling from `ssh_max_privilege`. See
        /// [`Ctx::deny_daemon_grants`].
        pub(super) fn from_grant(
            grant: super::Grant,
            peer: std::net::SocketAddr,
            deny_daemon_grants: bool,
        ) -> Self {
            use roomler_ai_remote_control::models::{ConsentMode, ssh_limits};

            // EXHAUSTIVE on purpose — see the type doc. A field added to the
            // grant must be routed here, in writing, before this compiles.
            let super::Grant {
                grant_id,
                public_key,
                caller,
                account_mode,
                account,
                consent_mode,
                session_secs,
                deadline,
            } = grant;
            // Two fields were spent BEFORE resolution, and that is their
            // decision: `public_key` is how `take_grant_for` found this grant
            // (auth logged its fingerprint), and `deadline` is the redemption
            // window `prune` already enforced. Consumed here so the
            // destructure stays exhaustive and a NEW field cannot borrow this
            // line as precedent for being dropped.
            let _ = (public_key, deadline);

            // The ceiling REFUSES rather than downgrading. A caller who asked
            // for root and silently got a user shell has been told something
            // untrue about their own session — they would read a permission
            // error as the command's problem, not the device's answer. The
            // refusal is deferred to run time (an `Err` here) so it can say
            // this on stderr; an auth failure cannot explain itself.
            let run_as = match crate::exec::RunAs::from_wire(&account_mode, account.as_deref()) {
                Ok(crate::exec::RunAs::Daemon) if deny_daemon_grants => Err(
                    "this device refuses ssh sessions that run as the daemon identity \
                     (ssh_max_privilege = console_user). Ask for console_user instead."
                        .to_string(),
                ),
                other => other,
            };

            Self {
                run_as,
                // Only an explicit `Auto` skips the operator. `Prompt`,
                // `Email`, `Push` and a server that said NOTHING all ask —
                // the fail-safe direction for a gate whose whole purpose is a
                // human in the loop, and the same rule Fleet RPC applies. The
                // grant id doubles as the sentinel the decision arrives on.
                consent_sentinel: match consent_mode {
                    Some(ConsentMode::Auto) => None,
                    _ => Some(grant_id.clone()),
                },
                grant_id: Some(grant_id),
                // A grant session is ALWAYS time-bounded. `record_grant`
                // already clamped this, but resolving is total on its own:
                // 0 means the ceiling, never "forever".
                deadline: Some(
                    tokio::time::Instant::now()
                        + std::time::Duration::from_secs(ssh_limits::clamp_session_secs(
                            session_secs,
                        )),
                ),
                caller: format!("ssh:{caller}@{peer}"),
            }
        }

        /// Resolve a key-list session — the break-glass path with no server
        /// policy behind it. Everything it may do is device-owned config.
        pub(super) fn from_key_list(ctx: &Ctx, user: &str, peer: std::net::SocketAddr) -> Self {
            Self {
                // Resolved once at start-up (`Ctx::build`), so a typo'd
                // `ssh_account_mode` is a boot log line; unset refuses with
                // the reason on stderr rather than falling through to the
                // daemon's own identity (the rc.419 bug).
                run_as: ctx.key_list_run_as.clone(),
                // Deliberately EXEMPT from consent: this path exists for when
                // the control plane is the broken thing, and blocking it on a
                // policy the control plane owns would take the emergency
                // route away exactly when it is needed.
                consent_sentinel: None,
                // Deliberately UNBOUNDED for the same reason; the server's
                // inactivity timeout still reaps an abandoned session.
                deadline: None,
                // All a key-list session can honestly claim: the username the
                // client typed, and the overlay address WireGuard proved.
                caller: format!("ssh:{user}@{peer} (key-list)"),
                // No grant, so nothing to correlate to. Its activity is still
                // reported (it is the path most worth watching), just without
                // a decision row to join against — the `(key-list)` suffix on
                // `caller` is what identifies it.
                grant_id: None,
            }
        }
    }

    /// Parse the device-owned `ssh_account_mode` into a [`RunAs`].
    ///
    /// Accepts the same vocabulary the server's `SshPolicy.account_mode` uses,
    /// plus `named:<account>` — the wire carries the account in its own field,
    /// but a single config string needs somewhere to put it.
    ///
    /// [`RunAs`]: crate::exec::RunAs
    pub(super) fn parse_account_mode(mode: &str) -> Result<crate::exec::RunAs, String> {
        match mode.trim() {
            "daemon" => Ok(crate::exec::RunAs::Daemon),
            "console_user" => Ok(crate::exec::RunAs::ConsoleUser),
            other => match other.strip_prefix("named:") {
                Some(a) if !a.trim().is_empty() => {
                    Ok(crate::exec::RunAs::Named(a.trim().to_string()))
                }
                _ => Err(format!(
                    "ssh_account_mode {other:?} is not one of daemon | console_user | \
                     named:<account>"
                )),
            },
        }
    }

    /// Per-connection state.
    pub struct Handler {
        ctx: Arc<Ctx>,
        /// The calling node's overlay address. Unforgeable — the packet reached
        /// us only by clearing WireGuard against a key from our netmap — so it
        /// is what every log line and audit record is keyed on.
        peer: std::net::SocketAddr,
        /// What this session may do, resolved ONCE at authentication. `None`
        /// only before auth — russh does not deliver channel requests until
        /// auth succeeded, so the handlers treat its absence as a refusal
        /// rather than a reachable state.
        policy: Option<ResolvedSessionPolicy>,
        /// Whether the session-deadline watchdog is running; armed on the
        /// first channel open so multiplexed channels share one timer.
        deadline_armed: bool,
        /// Terminal type and geometry from `pty_request`, held until the shell
        /// starts. `Some` is also what distinguishes an interactive session
        /// from a one-shot command.
        pty_req: Option<(String, crate::pty::WinSize)>,
        /// Sink for client keystrokes, live only while a terminal is. Cleared
        /// on EOF so the shell sees end-of-input.
        channel_input: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
        /// Resize half of the live terminal.
        pty_handle: Option<crate::pty::PtyHandle>,
        /// FR-55 — holds the machine awake for as long as this session lives.
        ///
        /// A field rather than a call pair, for the same reason `Drop` below
        /// carries the activity report: a session ends in many ways and only
        /// one of them is a path anyone would remember to annotate. Dropping
        /// the handler is the event they all share, so the guard rides along
        /// with it and cannot be leaked.
        _awake: Option<crate::power::ActivityGuard>,
    }

    impl Handler {
        pub fn new(ctx: Arc<Ctx>, peer: std::net::SocketAddr) -> Self {
            // Taken before `ctx` moves into the struct below.
            let awake = ctx
                .services
                .power_activity
                .as_ref()
                .map(crate::power::ActivityGuard::new);
            Self {
                ctx,
                peer,
                policy: None,
                deadline_armed: false,
                pty_req: None,
                channel_input: None,
                pty_handle: None,
                _awake: awake,
            }
        }

        /// Start the one-per-session watchdog that ends the connection when
        /// the granted lifetime runs out. A deadline already in the past
        /// fires immediately, so a channel opened late in a session cannot
        /// outrun the bound.
        ///
        /// Disconnecting is the whole enforcement: the pty pump's writes then
        /// fail, which drops the terminal and kills its process group, and an
        /// exec still in flight is bounded by the engine's own wall-clock
        /// ceiling. Key-list sessions have no deadline (see
        /// [`ResolvedSessionPolicy::from_key_list`]) and never arm this.
        /// Report one P8 activity event for this session, if the device opted
        /// in AND the session has resolved a policy.
        ///
        /// Both conditions are checked HERE rather than at each call site, so
        /// adding a new kind of session action cannot accidentally report
        /// from a device that never consented to reporting.
        fn report(
            &self,
            kind: SshActivityKind,
            detail: Option<String>,
            exit_code: Option<i32>,
            allowed: bool,
        ) {
            let (Some(sink), Some(policy)) = (&self.ctx.services.activity, &self.policy) else {
                return;
            };
            sink.report(
                policy.grant_id.clone(),
                &policy.caller,
                kind,
                detail,
                exit_code,
                allowed,
            );
        }

        fn arm_session_deadline(&mut self, session: &mut Session) {
            if self.deadline_armed {
                return;
            }
            let Some(policy) = &self.policy else { return };
            let Some(deadline) = policy.deadline else {
                return;
            };
            let caller = policy.caller.clone();
            self.deadline_armed = true;
            let handle = session.handle();
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                warn!(%caller, "ssh: session reached its granted time limit — disconnecting");
                let _ = handle
                    .disconnect(
                        russh::Disconnect::ByApplication,
                        "roomler-ssh: session time limit reached".to_string(),
                        String::new(),
                    )
                    .await;
            });
        }

        /// Serve the `sftp` subsystem — which is also how modern `scp` moves
        /// files (OpenSSH 9 replaced the old rcp-over-exec protocol).
        ///
        /// **It spawns OpenSSH's `sftp-server` as the session's account rather
        /// than serving SFTP in-process**, and that is a privilege decision,
        /// not convenience. In-process file I/O would run as the DAEMON —
        /// SYSTEM on Windows, root under systemd — so a session resolved to
        /// `console_user` would silently read and write every file as root.
        /// `exec` and the pty both drop privilege by handing work to a child;
        /// doing anything else here would introduce a second, weaker privilege
        /// story for the one subsystem that exists to touch files. Same
        /// `apply_run_as`, same consent gate, same identity.
        ///
        /// The bytes are opaque to us: the channel is spliced to the child's
        /// stdio and roomler never parses a single SFTP packet.
        async fn start_sftp_session(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), russh::Error> {
            let Some(policy) = &self.policy else {
                return refuse(session, channel, self.peer, "sftp", "no session policy");
            };
            let run_as = match policy.run_as.clone() {
                Ok(r) => r,
                Err(why) => {
                    return refuse(session, channel, self.peer, "sftp", &why);
                }
            };

            // Windows can only spawn as another user through
            // `CreateProcessAsUserW`, and the streaming variant of that lives
            // in the pty module's pseudoconsole path — it has no pipes form
            // yet. Refusing is the honest answer; silently running the
            // transfer as SYSTEM would be the dangerous one.
            #[cfg(windows)]
            if !matches!(run_as, crate::exec::RunAs::Daemon) {
                return refuse(
                    session,
                    channel,
                    self.peer,
                    "sftp",
                    "file transfer as the signed-in user is not supported on Windows yet \
                     (it needs a piped CreateProcessAsUserW). Interactive shells and \
                     commands work; scp/sftp here would otherwise run as SYSTEM, which \
                     is why this refuses instead.",
                );
            }

            let program = match sftp_server_path() {
                Some(p) => p,
                None => {
                    return refuse(
                        session,
                        channel,
                        self.peer,
                        "sftp",
                        "no `sftp-server` binary on this device. roomler serves SFTP by \
                         handing the channel to OpenSSH's own server so transfers run as \
                         the session's account; install the OpenSSH server package to \
                         enable scp/sftp.",
                    );
                }
            };

            session.channel_success(channel)?;
            let handle = session.handle();
            let caller = policy.caller.clone();
            let consent = policy.consent_sentinel.clone();
            let broker = self.ctx.services.consent.clone();
            let indicator = self.ctx.services.indicator.clone();

            if let Some(sentinel) = consent
                && let Err(why) = await_consent(
                    &broker,
                    &indicator,
                    &handle,
                    channel,
                    &caller,
                    &sentinel,
                    "transfer files over SFTP",
                )
                .await
            {
                let _ = handle
                    .extended_data(channel, 1, format!("roomler: {why}\r\n").into_bytes())
                    .await;
                let _ = handle.close(channel).await;
                return Ok(());
            }

            let mut cmd = tokio::process::Command::new(&program);
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Err(why) = crate::exec::apply_run_as(&mut cmd, &run_as) {
                let _ = handle
                    .extended_data(channel, 1, format!("roomler: {why}\r\n").into_bytes())
                    .await;
                let _ = handle.close(channel).await;
                return Ok(());
            }

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = handle
                        .extended_data(
                            channel,
                            1,
                            format!("roomler: could not start sftp-server: {e}\r\n").into_bytes(),
                        )
                        .await;
                    let _ = handle.close(channel).await;
                    return Ok(());
                }
            };

            info!(
                peer = %self.peer, %caller, run_as = %run_as.label(),
                privileged = run_as.is_privileged(), server = %program.display(),
                "ssh: sftp session started"
            );

            // Bounded, for the same reason the pty's queue is: a client
            // streaming a large upload must not grow the daemon's memory
            // faster than sftp-server drains it.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            self.channel_input = Some(tx);

            let mut stdin = child.stdin.take().expect("stdin was piped");
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Some(chunk) = rx.recv().await {
                    if stdin.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
                // EOF to the child, so it finishes and exits rather than
                // waiting on a client that has already gone.
                let _ = stdin.shutdown().await;
            });

            let mut stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if handle.data(channel, buf[..n].to_vec()).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Reap before closing so the child cannot outlive the channel
                // as a zombie holding the user's files open.
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            });

            // sftp-server writes diagnostics here. They belong in OUR log, not
            // on the client's channel: the SFTP stream is a binary protocol
            // and injecting text into it corrupts the transfer.
            if let Some(mut stderr) = stderr {
                let peer = self.peer;
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut s = String::new();
                    if stderr.read_to_string(&mut s).await.is_ok() && !s.trim().is_empty() {
                        warn!(%peer, msg = %s.trim(), "ssh: sftp-server stderr");
                    }
                });
            }

            Ok(())
        }

        /// Start an interactive session on the terminal `pty_request` asked
        /// for.
        ///
        /// Runs the SAME two gates a one-shot command does, in the same order:
        /// resolve the identity from the server (never from the client), then
        /// settle operator consent. An interactive shell is strictly more than
        /// a single command, so it cannot be held to a weaker standard.
        async fn start_pty_session(
            &mut self,
            channel: ChannelId,
            command: Option<String>,
            session: &mut Session,
        ) -> Result<(), russh::Error> {
            let Some((term, size)) = self.pty_req.clone() else {
                return refuse(
                    session,
                    channel,
                    self.peer,
                    "shell",
                    "no terminal was requested for this session",
                );
            };

            // The session's whole entitlement was resolved at auth; requests
            // only READ it. Absent means unauthenticated, which russh does
            // not deliver requests for — refuse rather than reason about it.
            let Some(policy) = &self.policy else {
                return refuse(session, channel, self.peer, "shell", "no session policy");
            };
            let run_as = match policy.run_as.clone() {
                Ok(r) => r,
                Err(e) => {
                    warn!(peer = %self.peer, %e, "ssh: refusing a shell — unusable account mode");
                    let _ = session.extended_data(
                        channel,
                        1,
                        format!("roomler-ssh: {e}\r\n").into_bytes(),
                    );
                    session.channel_failure(channel)?;
                    return Ok(());
                }
            };

            session.channel_success(channel)?;
            let handle = session.handle();
            let caller = policy.caller.clone();
            let consent = policy.consent_sentinel.clone();
            let broker = self.ctx.services.consent.clone();
            let indicator = self.ctx.services.indicator.clone();

            if let Some(sentinel) = consent
                && let Err(why) = await_consent(
                    &broker,
                    &indicator,
                    &handle,
                    channel,
                    &caller,
                    &sentinel,
                    "open an interactive shell",
                )
                .await
            {
                let _ = handle
                    .extended_data(channel, 1, format!("roomler-ssh: {why}\r\n").into_bytes())
                    .await;
                let _ = handle.exit_status_request(channel, 1).await;
                let _ = handle.close(channel).await;
                return Ok(());
            }

            let pty = match crate::pty::PtyProcess::spawn(command.as_deref(), &term, size, &run_as)
            {
                Ok(p) => p,
                Err(e) => {
                    warn!(peer = %self.peer, %e, "ssh: could not start the terminal session");
                    let _ = handle
                        .extended_data(channel, 1, format!("roomler-ssh: {e}\r\n").into_bytes())
                        .await;
                    let _ = handle.exit_status_request(channel, 1).await;
                    let _ = handle.close(channel).await;
                    return Ok(());
                }
            };

            info!(
                peer = %self.peer, %caller, run_as = %run_as.label(),
                privileged = run_as.is_privileged(), %term,
                "ssh: interactive session started"
            );

            // A bounded queue: an operator holding a key down must not be able
            // to grow the daemon's memory faster than the shell drains it.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            self.channel_input = Some(tx);
            self.pty_handle = Some(pty.handle());

            let writer = pty.handle();
            tokio::spawn(async move {
                while let Some(chunk) = rx.recv().await {
                    if writer.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            });

            let caller_for_pump = caller.clone();
            tokio::spawn(async move {
                let mut pty = pty;
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match pty.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if handle.data(channel, buf[..n].to_vec()).await.is_err() {
                                // The client vanished. Drop the session rather
                                // than keep a shell alive against a channel
                                // nobody reads.
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(%e, "ssh: terminal read failed");
                            break;
                        }
                    }
                }
                let status = pty.wait().await;
                info!(caller = %caller_for_pump, status, "ssh: interactive session ended");
                let _ = handle.exit_status_request(channel, status).await;
                let _ = handle.close(channel).await;
                // `pty` drops here, which kills anything still in the group.
            });

            Ok(())
        }
    }

    /// Close the P8 activity envelope (see [`SshActivityKind::SessionClose`]).
    ///
    /// `Drop` rather than a disconnect hook because a session ends in many
    /// ways — clean exit, client vanishing, the `session_secs` deadline firing,
    /// a consent refusal — and only one of those is a code path anyone would
    /// remember to annotate. Dropping the handler is the single event they all
    /// share.
    ///
    /// Reporting is `try_send`, so this is safe in a destructor: no await, no
    /// runtime needed, and a full queue drops the line rather than blocking
    /// teardown.
    impl Drop for Handler {
        fn drop(&mut self) {
            self.report(SshActivityKind::SessionClose, None, None, true);
        }
    }

    impl russh::server::Handler for Handler {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            user: &str,
            key: &PublicKey,
        ) -> Result<Auth, Self::Error> {
            let fingerprint = key.fingerprint(ssh_key::HashAlg::Sha256).to_string();

            // A server-minted grant is tried FIRST, and consumed when it
            // matches. It is the path that carries a roomler identity, and it
            // is single-use — so a captured public key cannot be replayed into
            // a second session even within the grant's lifetime.
            if let Some(grant) = super::take_grant_for(key) {
                info!(
                    peer = %self.peer, %user, %fingerprint,
                    grant_id = %grant.grant_id, caller = %grant.caller,
                    account_mode = %grant.account_mode,
                    "ssh: authenticated (server grant)"
                );
                // Resolving HERE — not lazily at each request — is the P5
                // design: one place turns a grant into what a session may do,
                // and the request handlers can only read the result.
                self.policy = Some(ResolvedSessionPolicy::from_grant(
                    grant,
                    self.peer,
                    self.ctx.deny_daemon_grants,
                ));
                self.report(SshActivityKind::SessionOpen, None, None, true);
                return Ok(Auth::Accept);
            }

            // Otherwise the device-owned list. Compare KEY DATA, not the
            // `PublicKey` itself: the parsed value carries the trailing
            // comment, so the same key recorded with a different comment would
            // not be equal.
            let listed = self
                .ctx
                .authorized
                .iter()
                .any(|k| k.key_data() == key.key_data());
            if listed {
                info!(
                    peer = %self.peer, %user, %fingerprint,
                    "ssh: authenticated (ssh_authorized_keys)"
                );
                self.policy = Some(ResolvedSessionPolicy::from_key_list(
                    &self.ctx, user, self.peer,
                ));
                self.report(SshActivityKind::SessionOpen, None, None, true);
                Ok(Auth::Accept)
            } else {
                warn!(
                    peer = %self.peer, %user, %fingerprint,
                    pending_grants = super::pending_grants(),
                    "ssh: rejected — no live grant for this key and it is not in ssh_authorized_keys"
                );
                Ok(Auth::reject())
            }
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<Msg>,
            reply: russh::server::ChannelOpenHandle,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            self.arm_session_deadline(session);
            Ok(())
        }

        /// `ssh -L`, `-J` and `-W` all arrive here (P7b): the client asks this
        /// device to connect somewhere and splice the socket to the channel.
        ///
        /// Deliberately NOT gated on the session's account: a TCP connect runs
        /// no code and takes on no user identity, so `RunAs` has nothing to
        /// say about it. The gate is the operator's forward allowlist —
        /// default-deny, for the reason in [`forward_allowed`].
        ///
        /// **No interactive consent prompt here, on purpose.** The channel is
        /// not open yet — we are deciding whether to open it — so there is no
        /// stderr to announce a wait on, and a silent 30 s block ending in an
        /// unexplained failure is precisely what P5d rejected for grants. The
        /// operator's consent for a forward is the allowlist entry itself:
        /// given in advance, naming the destination, and default-deny until
        /// they write it. That is a more considered act than a prompt, not a
        /// weaker one.
        async fn channel_open_direct_tcpip(
            &mut self,
            channel: Channel<Msg>,
            host_to_connect: &str,
            port_to_connect: u32,
            _originator_address: &str,
            _originator_port: u32,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            use russh::ChannelOpenFailure;

            let host = host_to_connect.to_string();
            // A u32 on the wire; anything above 65535 is a malformed request,
            // not a port we should try.
            let Ok(port) = u16::try_from(port_to_connect) else {
                warn!(peer = %self.peer, port_to_connect, "ssh: forward refused — port out of range");
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
                return Ok(());
            };

            let Some(policy) = &self.policy else {
                warn!(peer = %self.peer, "ssh: forward refused — no session policy");
                reply
                    .reject(ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
                return Ok(());
            };
            let caller = policy.caller.clone();

            if let Err(why) = forward_allowed(&self.ctx.forward_acl, &host, port) {
                // `ssh` renders this as "administratively prohibited", which
                // tells the caller it was a policy decision and not a dead
                // destination. The *reason* stays here, next to the operator
                // who can act on it — it names internal topology.
                warn!(peer = %self.peer, %caller, %host, port, %why, "ssh: forward refused");
                // Reported as DENIED. A refused forward is the row an operator
                // most wants to see — it is someone probing what this device
                // can reach.
                self.report(
                    SshActivityKind::Forward,
                    Some(format!("{host}:{port}")),
                    None,
                    false,
                );
                reply
                    .reject(ChannelOpenFailure::AdministrativelyProhibited)
                    .await;
                return Ok(());
            }

            // Held for the life of the splice, released when the task ends.
            let Ok(permit) = self.ctx.forwards.clone().try_acquire_owned() else {
                warn!(
                    peer = %self.peer, %caller, %host, port,
                    cap = MAX_CONCURRENT_FORWARDS,
                    "ssh: forward refused — concurrent-forward ceiling reached"
                );
                reply.reject(ChannelOpenFailure::ResourceShortage).await;
                return Ok(());
            };

            // Connect BEFORE accepting, so a dead destination is reported as
            // `ConnectFailed` rather than an accepted channel that immediately
            // hangs up — which the client can only present as a mystery.
            let target = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(peer = %self.peer, %host, port, %e, "ssh: forward could not connect");
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                    return Ok(());
                }
            };
            let _ = target.set_nodelay(true);

            reply.accept().await;
            info!(peer = %self.peer, %caller, %host, port, "ssh: forward opened");
            self.report(
                SshActivityKind::Forward,
                Some(format!("{host}:{port}")),
                None,
                true,
            );
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                let mut target = target;
                // Bidirectional until either side closes — `copy_bidirectional`
                // already half-closes correctly, which is the thing a
                // hand-rolled select! gets wrong (see `sshcmd::pump`).
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
                drop(permit);
            });
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // The SSH exec payload is bytes, not necessarily UTF-8. Refuse
            // rather than lossily convert: a command with replacement
            // characters in it is not the command the caller asked for.
            let Ok(command) = std::str::from_utf8(data) else {
                warn!(peer = %self.peer, "ssh: exec payload is not valid UTF-8 — refused");
                session.channel_failure(channel)?;
                return Ok(());
            };
            let command = command.to_string();
            session.channel_success(channel)?;

            let handle = session.handle();
            // Everything this session may do — identity, consent, bound — was
            // resolved ONCE at authentication ([`ResolvedSessionPolicy`]);
            // this handler only reads it. Identity comes from the server or
            // the device's own config, never from anything the client said.
            let Some(policy) = &self.policy else {
                let _ = session.extended_data(
                    channel,
                    1,
                    b"roomler-ssh: no session policy\r\n".to_vec(),
                );
                session.exit_status_request(channel, 1)?;
                session.close(channel)?;
                return Ok(());
            };
            let run_as = match policy.run_as.clone() {
                Ok(r) => r,
                Err(e) => {
                    // An unreadable identity is a refusal, never a fallback:
                    // running as the daemon because the policy could not be
                    // parsed is exactly the silent escalation this design
                    // exists to prevent.
                    warn!(peer = %self.peer, %e, "ssh: refusing exec — unusable account mode");
                    let _ = session.extended_data(
                        channel,
                        1,
                        format!("roomler-ssh: {e}\r\n").into_bytes(),
                    );
                    session.exit_status_request(channel, 1)?;
                    session.close(channel)?;
                    return Ok(());
                }
            };
            let caller = policy.caller.clone();
            let consent = policy.consent_sentinel.clone();
            let broker = self.ctx.services.consent.clone();
            let indicator = self.ctx.services.indicator.clone();
            // Captured here, where the policy is still borrowed, and reported
            // from inside the task once the exit code exists.
            let activity = self
                .ctx
                .services
                .activity
                .clone()
                .map(|s| (s, policy.grant_id.clone()));
            tokio::spawn(async move {
                run_exec(
                    handle, channel, command, caller, run_as, consent, broker, indicator, activity,
                )
                .await;
            });
            Ok(())
        }

        /// A shell request without a prior PTY request is a non-interactive
        /// shell — rare, and not what this exists to serve. Refused with a
        /// reason rather than silently handed a terminal the client never
        /// asked for.
        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if self.pty_req.is_some() {
                // The shell's CONTENTS are never recorded; this row exists so
                // a reader can see that an interactive session happened.
                self.report(SshActivityKind::Shell, None, None, true);
                self.start_pty_session(channel, None, session).await
            } else {
                refuse(
                    session,
                    channel,
                    self.peer,
                    "shell",
                    "a shell needs a terminal here — drop `-T`, or use \
                     `ssh <node> <command>` for a one-shot command.",
                )
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn pty_request(
            &mut self,
            channel: ChannelId,
            term: &str,
            col_width: u32,
            row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // Recorded, not acted on: the terminal is allocated when the
            // shell starts, because that is when the identity it runs as
            // is known and when consent has been settled. Allocating here
            // would leave a pty (and later a shell) alive for a session
            // that is about to be refused.
            self.pty_req = Some((
                term.to_string(),
                crate::pty::WinSize {
                    cols: col_width as u16,
                    rows: row_height as u16,
                },
            ));
            info!(peer = %self.peer, %term, cols = col_width, rows = row_height, "ssh: pty requested");
            session.channel_success(channel)?;
            Ok(())
        }

        /// Client keystrokes. Only meaningful once a terminal exists; before
        /// that there is nothing to type into, and after the session ends the
        /// send simply fails and the data is dropped.
        async fn data(
            &mut self,
            _channel: ChannelId,
            data: &[u8],
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            if let Some(tx) = &self.channel_input {
                let _ = tx.send(data.to_vec()).await;
            }
            Ok(())
        }

        /// The client's window changed. Resizing the terminal is what makes
        /// the far side reflow. On Unix the kernel also SIGWINCHes the
        /// foreground group, which is how a full-screen application learns to
        /// redraw; ConPTY delivers the equivalent through the console host.
        async fn window_change_request(
            &mut self,
            _channel: ChannelId,
            col_width: u32,
            row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            if let Some(h) = &self.pty_handle {
                let size = crate::pty::WinSize {
                    cols: col_width as u16,
                    rows: row_height as u16,
                };
                if let Err(e) = h.resize(size) {
                    warn!(peer = %self.peer, %e, "ssh: could not resize the terminal");
                }
            }
            Ok(())
        }

        /// The client stopped sending. Close the terminal's input side so the
        /// shell sees end-of-input and exits on its own, rather than being
        /// killed — a shell that exits normally runs its logout hooks.
        async fn channel_eof(
            &mut self,
            _channel: ChannelId,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channel_input = None;
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            channel: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if name == "sftp" {
                // File operations happen inside `sftp-server`; this records
                // only that a transfer channel was opened.
                self.report(SshActivityKind::Sftp, None, None, true);
                return self.start_sftp_session(channel, session).await;
            }
            refuse(
                session,
                channel,
                self.peer,
                name,
                "only the `sftp` subsystem is implemented.",
            )
        }
    }

    /// May this session open a forward to `host:port`? (P7b)
    ///
    /// **Default-DENY, unlike the tunnel path that shares this ACL**, and the
    /// difference is not an oversight. For a tunnel the SERVER has already
    /// authorized the destination, so an empty `forward_acl.allowlist` sensibly
    /// means "no extra local restriction — trust that decision". A
    /// `direct-tcpip` channel has no server in the path at all: roomler hands
    /// the client a socket and never sees the bytes. The local allowlist is
    /// therefore the ONLY gate, and an empty one has to mean *nothing*, or
    /// every SSH session would silently be an open pivot into whatever the
    /// device can reach.
    ///
    /// So the operator opts in per destination, and the refusal names the key
    /// to set — an unexplained "administratively prohibited" is how `-L` turns
    /// into a mystery.
    pub(super) fn forward_allowed(
        acl: &crate::tunnel::acl::AgentForwardAcl,
        host: &str,
        port: u16,
    ) -> Result<(), String> {
        if !acl.enabled {
            return Err("port forwarding is switched off on this device \
                        (`forward_acl.enabled = false`)."
                .into());
        }
        if acl.allowlist.is_empty() {
            return Err(format!(
                "this device forwards nowhere by default. To allow {host}:{port}, add a rule \
                 to `forward_acl.allowlist` in its config. (Tunnels may still work: those are \
                 authorized by the server, and an SSH forward is not.)"
            ));
        }
        match acl.check(
            host,
            port,
            roomler_ai_remote_control::models::ProtocolKind::Tcp,
        ) {
            d if d.is_allow() => Ok(()),
            _ => Err(format!(
                "{host}:{port} is not in this device's `forward_acl.allowlist`."
            )),
        }
    }

    /// Where OpenSSH keeps `sftp-server` on this platform.
    ///
    /// Distribution-specific by nature — Debian puts it under `/usr/lib`,
    /// Red Hat under `/usr/libexec`, macOS ships its own — so this probes a
    /// list rather than hardcoding one and being wrong on half the fleet.
    /// `None` is a normal answer (no OpenSSH server installed) and the caller
    /// turns it into a refusal that says what to install.
    fn sftp_server_path() -> Option<std::path::PathBuf> {
        // `ROOMLER_SFTP_SERVER` first, for a host that keeps it somewhere
        // unusual — an escape hatch beats a support ticket.
        if let Ok(p) = std::env::var("ROOMLER_SFTP_SERVER") {
            let p = std::path::PathBuf::from(p);
            return p.is_file().then_some(p);
        }
        #[cfg(windows)]
        const CANDIDATES: &[&str] = &[
            r"C:\Windows\System32\OpenSSH\sftp-server.exe",
            r"C:\Program Files\OpenSSH\sftp-server.exe",
        ];
        #[cfg(target_os = "macos")]
        const CANDIDATES: &[&str] = &["/usr/libexec/sftp-server", "/usr/lib/ssh/sftp-server"];
        #[cfg(all(unix, not(target_os = "macos")))]
        const CANDIDATES: &[&str] = &[
            "/usr/lib/openssh/sftp-server",     // Debian, Ubuntu
            "/usr/libexec/openssh/sftp-server", // Fedora, RHEL
            "/usr/lib/ssh/sftp-server",         // Arch, Alpine
            "/usr/libexec/sftp-server",
        ];
        CANDIDATES
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.is_file())
    }

    /// Tell the client no, in a way a human reads on their terminal.
    ///
    /// A bare `channel_failure` surfaces as a blank "administratively
    /// prohibited", which is indistinguishable from a policy denial — so send
    /// the reason on stderr first.
    fn refuse(
        session: &mut Session,
        channel: ChannelId,
        peer: std::net::SocketAddr,
        what: &str,
        why: &str,
    ) -> Result<(), russh::Error> {
        warn!(%peer, request = %what, "ssh: refused an unsupported request");
        let _ = session.extended_data(channel, 1, format!("roomler-ssh: {why}\r\n").into_bytes());
        session.channel_failure(channel)?;
        Ok(())
    }

    /// Ask the operator at this device, and say so on stderr first.
    ///
    /// The advisory line matters: without it a policy of `prompt` looks to the
    /// caller like a 30-second hang, which is the same "a bare failure cannot
    /// explain itself" problem that shapes every other refusal here.
    ///
    /// The broker arrives through [`super::SessionServices`], so "nobody can
    /// be asked" is no longer a reachable state — a server that exists was
    /// handed one at construction. (It used to be a process global whose
    /// absence this function had to treat as a denial.)
    ///
    /// `what` names the activity being approved ("open an interactive shell",
    /// "run a command", "transfer files"). FR-27 puts it in front of the person
    /// deciding, not just in the daemon log: "grant SSH" and "give this account
    /// a root shell on my machine right now" are not the same question.
    async fn await_consent(
        broker: &crate::consent::ConsentBroker,
        indicator: &crate::indicator::ViewerIndicator,
        handle: &russh::server::Handle,
        channel: ChannelId,
        caller: &str,
        sentinel: &str,
        what: &str,
    ) -> Result<(), String> {
        let timeout = crate::consent::DEFAULT_PROMPT_TIMEOUT;
        let _ = handle
            .extended_data(
                channel,
                1,
                format!(
                    "roomler-ssh: waiting up to {}s for approval at the device…\r\n",
                    timeout.as_secs()
                )
                .into_bytes(),
            )
            .await;

        // FR-27 — the same surface chain as a screen-control or exec prompt:
        // the daemon's own panel first, the desktop companion (started if it
        // is not running) second, the CLI sentinel always. SSH used to write
        // the companion marker only, which on a host with the companion closed
        // meant no screen at all — while the native panel on the same desktop
        // was answering exec prompts a minute earlier. `what` is the lead,
        // because "grant SSH" and "open an interactive shell as SYSTEM" are not
        // the same question.
        let native = indicator.show_prompt(crate::indicator::PromptView {
            session_hex: sentinel.to_string(),
            title: "SSH session request".into(),
            lead: format!("{caller} wants to {what} on this device."),
            detail: String::new(),
            permissions: String::new(),
            org: String::new(),
            expires_at: std::time::Instant::now() + timeout,
        });
        let prompt = crate::consent::PendingPrompt {
            kind: crate::consent::PromptKind::Ssh,
            asked_by: caller,
            permissions: String::new(),
            detail: what.to_string(),
            org: String::new(),
            timeout,
            // Written either way — `roomlerd consent --list` must show a
            // natively-prompted session too; the field is what stops the
            // companion from asking the same question twice.
            surface: if native {
                crate::consent::PromptSurface::Native
            } else {
                crate::consent::PromptSurface::Companion
            },
        };
        let mut have_surface = native;
        if let Err(e) = broker.write_prompt(sentinel, &prompt) {
            if !native {
                have_surface = false;
            }
            warn!(%caller, %sentinel, %e, native, "ssh: could not write the .pending consent marker");
        }
        if !native {
            // Awaited, not spawned, and bounded: its verdict is what lets the
            // refusal below say "nobody could be asked" instead of "nobody
            // answered" — different fixes, and the caller cannot tell them
            // apart afterwards.
            let companion_up = tokio::time::timeout(
                crate::signaling::COMPANION_START_BUDGET,
                crate::companion::ensure_running(),
            )
            .await
            .unwrap_or(false);
            if !companion_up {
                have_surface = false;
            }
        }
        info!(%caller, %sentinel, native, have_surface, "consent prompt surface (ssh)");

        let decision = broker
            .request_with_mode(sentinel, crate::consent::Mode::Prompt { timeout })
            .await;
        // The single convergence point: answered, timed out, or resolved from
        // the CLI — the panel comes down here regardless of who decided.
        indicator.hide_prompt(sentinel);
        if decision.granted() {
            info!(%caller, %sentinel, "ssh: operator approved the session");
            return Ok(());
        }
        warn!(%caller, %sentinel, ?decision, have_surface, "ssh: operator did not approve the session");
        Err(match decision {
            crate::consent::Decision::Timeout if !have_surface => {
                "nobody could be asked at this device — it has no prompt surface (no one at its \
                 screen and the Roomler desktop app is not running); answer with `roomlerd \
                 consent` there, start the desktop app, or set this device's SSH policy to auto"
                    .to_string()
            }
            crate::consent::Decision::Timeout => format!(
                "nobody approved this session at the device within {}s",
                timeout.as_secs()
            ),
            _ => "the operator at this device denied this session".to_string(),
        })
    }

    /// Run one command through the daemon's existing execution engine and
    /// stream the result back over the channel.
    ///
    /// Deliberately NOT a fresh `Command::spawn`: [`crate::exec`] already owns
    /// the wall-clock timeout, the output ceiling, the per-device concurrency
    /// cap, secret redaction, and process-tree kill on timeout or cancel. An
    /// SSH transport is not a reason to re-implement any of that, and every
    /// bound it enforces is one the device owner already reasoned about for
    /// Fleet RPC.
    ///
    /// The cost is that output is delivered when the command finishes rather
    /// than as it is produced — the engine buffers to enforce its ceiling.
    /// P4's PTY path is what makes long-running output live.
    // 8 params: each is a distinct decision resolved at authentication
    // (identity, consent, bound, reporting) and bundling them into a struct
    // would only move the same fields behind a name that hides which of them
    // a reader must check.
    #[allow(clippy::too_many_arguments)]
    async fn run_exec(
        handle: russh::server::Handle,
        channel: ChannelId,
        command: String,
        caller: String,
        run_as: crate::exec::RunAs,
        consent: Option<String>,
        broker: crate::consent::ConsentBroker,
        indicator: crate::indicator::ViewerIndicator,
        activity: Option<(ActivitySink, Option<String>)>,
    ) {
        use roomler_ai_remote_control::models::exec_limits;

        // Ask BEFORE anything runs, and refuse rather than fall through. The
        // prompt is deliberately here and not at grant-arrival time: the server
        // has already told the caller where to dial by then, so a refusal there
        // would surface as a connection that rejects them for no stated reason.
        if let Some(sentinel) = consent
            && let Err(why) = await_consent(
                &broker,
                &indicator,
                &handle,
                channel,
                &caller,
                &sentinel,
                "run a command",
            )
            .await
        {
            let _ = handle
                .extended_data(channel, 1, format!("roomler-ssh: {why}\r\n").into_bytes())
                .await;
            let _ = handle.exit_status_request(channel, 1).await;
            let _ = handle.close(channel).await;
            return;
        }

        let request_id = format!("ssh-{:016x}", rand::random::<u64>());
        let identity = run_as.label();
        let privileged = run_as.is_privileged();
        let req = crate::exec::ExecRequest {
            request_id,
            // Empty = the host's own default shell, matching `roomler exec`.
            shell: String::new(),
            command: command.clone(),
            timeout_ms: exec_limits::MAX_TIMEOUT_MS,
            max_output_bytes: exec_limits::MAX_OUTPUT_BYTES,
            cwd: None,
            caller: caller.clone(),
            run_as,
        };

        let outcome = crate::exec::shared()
            .run(req, &crate::exec::redactor())
            .await;

        if !outcome.stdout.is_empty() {
            let _ = handle
                .data(channel, outcome.stdout.clone().into_bytes())
                .await;
        }
        if !outcome.stderr.is_empty() {
            let _ = handle
                .extended_data(channel, 1, outcome.stderr.clone().into_bytes())
                .await;
        }
        // A run that never reached a process still has to say so on stderr:
        // an empty stream plus exit 0 would read as "the command succeeded and
        // printed nothing", which is the opposite of what happened.
        if let Some(err) = &outcome.error {
            let _ = handle
                .extended_data(channel, 1, format!("roomler-ssh: {err}\r\n").into_bytes())
                .await;
        }

        // SSH carries an unsigned status, so anything that is not a clean
        // 0..=255 has to be mapped — and it must map to FAILURE. Clamping
        // instead would turn a negative code (a signal-encoded exit) into 0,
        // reporting success for a process that was killed. `None` means the
        // command never ran at all (refused, timed out, cancelled); 1 is the
        // conventional shell answer for both.
        let status = match outcome.exit_code {
            Some(c) if (0..=255).contains(&c) => c as u32,
            _ => 1,
        };
        let _ = handle.exit_status_request(channel, status).await;
        let _ = handle.eof(channel).await;
        let _ = handle.close(channel).await;

        // `privileged` is on the line on purpose: "who ran this, and was it as
        // root?" is the question anyone reading these logs after an incident
        // is actually asking, and it should not require joining against the
        // device's policy to answer.
        info!(
            %caller, run_as = %identity, privileged,
            exit = status, duration_ms = outcome.duration_ms,
            bytes = outcome.output_bytes(), truncated = outcome.truncated,
            "ssh: exec finished"
        );

        // P8 — reported AFTER the run, so the row carries the real exit code
        // rather than "a command was accepted". The command text is the only
        // thing recorded; its OUTPUT stays on this host, which is the whole
        // distinction this feature is built around.
        if let Some((sink, grant_id)) = activity {
            sink.report(
                grant_id,
                &caller,
                SshActivityKind::Exec,
                Some(command),
                outcome.exit_code,
                true,
            );
        }
    }
}

/// Build the per-session server context from config.
#[cfg(all(feature = "overlay-netstack", feature = "ssh-server"))]
fn build_ctx(cfg: &crate::config::AgentConfig, services: SessionServices) -> ServeCtx {
    sshd::Ctx::build(cfg, services)
}

/// Run the SSH protocol over one intercepted connection.
#[cfg(all(feature = "overlay-netstack", feature = "ssh-server"))]
async fn serve_conn(stream: tunnel_core::overlay::netstack::NsTcpStream, ctx: ServeCtx) {
    use tracing::info;

    let peer = stream.peer_addr();
    let Some(ctx) = ctx else {
        // No host key ⇒ nothing to serve. Drop the connection rather than
        // leaving the client hanging on a banner that never arrives.
        warn!(%peer, "ssh: connection dropped — this node has no usable host key");
        return;
    };

    info!(%peer, "ssh: session opening");
    let handler = sshd::Handler::new(ctx.clone(), peer);
    match russh::server::run_stream(ctx.config.clone(), stream, handler).await {
        Ok(session) => match session.await {
            Ok(()) => info!(%peer, "ssh: session closed"),
            Err(e) => info!(%peer, %e, "ssh: session ended"),
        },
        Err(e) => warn!(%peer, %e, "ssh: handshake failed"),
    }
}

// ===========================================================================
// P3 — server-minted session grants.
// ===========================================================================

/// One authorization to open a session, pushed by the server as
/// `rc:ssh.grant` after it cleared the org kill-switch, the caller's
/// `SSH_DEVICE` permission and this device's `SshPolicy`.
///
/// The agent does not verify a signature on this. It does not need to: the
/// frame arrived over the control WebSocket the daemon is already
/// authenticated on, which is the same trust path `rc:request` uses to open a
/// remote-control session. What the agent *does* enforce are the bounds it can
/// check locally — expiry, single use, and a cap on how many can be pending —
/// because "the server said so" is not a reason to accept an unbounded table
/// or an eternal key.
#[cfg(feature = "ssh-server")]
#[derive(Debug, Clone)]
pub struct Grant {
    pub grant_id: String,
    /// The public key this grant admits, in OpenSSH form. Matched by key data.
    pub public_key: String,
    /// Display name of the acting principal, for the log and the audit record.
    pub caller: String,
    /// Which local account the session runs as. Carried through now; P5 is
    /// what makes anything other than `daemon` actually happen.
    pub account_mode: String,
    pub account: Option<String>,
    /// Whether the operator AT the device has to approve this session before
    /// anything runs, per the device's `SshPolicy`.
    ///
    /// `None` means the server said nothing, which is treated exactly like
    /// `Prompt` — the fail-safe direction for a gate whose whole purpose is to
    /// put a human in the loop. Only an explicit `Auto` skips the prompt.
    pub consent_mode: Option<roomler_ai_remote_control::models::ConsentMode>,
    /// Server-clamped session lifetime once redeemed.
    pub session_secs: u64,
    /// LOCAL deadline after which this grant is dead.
    ///
    /// Deliberately an `Instant` derived from arrival rather than the server's
    /// wall-clock `expires_at_ms`: the two machines' clocks can differ by
    /// minutes, and a skewed or malformed timestamp must not be able to
    /// produce a grant that never expires. The server's value only ever
    /// SHORTENS the local ceiling.
    deadline: std::time::Instant,
}

/// Pending grants, oldest first.
///
/// A `Vec` rather than a map because the lookup key is "whichever entry's key
/// data matches", the table is capped at a handful of entries, and a linear
/// scan of eight items is not worth a hashing strategy.
#[cfg(feature = "ssh-server")]
static GRANTS: std::sync::LazyLock<std::sync::Mutex<Vec<Grant>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Most grants we will hold at once. A grant is redeemed within seconds of
/// being issued, so a backlog means something is wrong — and an unbounded
/// table would be a memory sink reachable from the control plane.
#[cfg(feature = "ssh-server")]
const MAX_PENDING_GRANTS: usize = 16;

/// Record an `rc:ssh.grant`. Returns an error string to log when the grant is
/// unusable; the caller does not answer the server, because the caller of the
/// SSH session (a different device) is the one waiting, and it learns the
/// outcome by its connection succeeding or not.
#[cfg(feature = "ssh-server")]
#[allow(clippy::too_many_arguments)]
pub fn record_grant(
    grant_id: String,
    public_key: String,
    caller: String,
    account_mode: String,
    account: Option<String>,
    consent_mode: Option<roomler_ai_remote_control::models::ConsentMode>,
    expires_at_ms: u64,
    session_secs: u64,
) -> Result<(), String> {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use roomler_ai_remote_control::models::ssh_limits;

    // Parse now, not at authentication time: a malformed key should be a
    // start-up-shaped error in the log, not a mysterious auth failure later.
    russh::keys::ssh_key::PublicKey::from_openssh(&public_key)
        .map_err(|e| format!("grant carries an unparseable public key: {e}"))?;

    // Re-clamp the lifetime against our own clock. The server's timestamp can
    // only make the window SMALLER than the local ceiling, never larger.
    let ceiling = Duration::from_secs(ssh_limits::GRANT_TTL_SECS);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let server_window = Duration::from_millis(expires_at_ms.saturating_sub(now_ms));
    let window = server_window.min(ceiling);
    if window.is_zero() {
        return Err("grant is already expired on arrival (clock skew?)".into());
    }

    let grant = Grant {
        grant_id,
        public_key,
        caller,
        account_mode,
        account,
        consent_mode,
        session_secs: ssh_limits::clamp_session_secs(session_secs),
        deadline: Instant::now() + window,
    };

    let mut grants = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut grants);
    if grants.len() >= MAX_PENDING_GRANTS {
        // Drop the OLDEST rather than refusing the newest: the newest is the
        // one a caller is actively waiting on, and the oldest is within
        // seconds of expiring anyway.
        grants.remove(0);
        tracing::warn!("ssh: pending-grant table full — dropped the oldest");
    }
    tracing::info!(
        grant_id = %grant.grant_id, caller = %grant.caller,
        account_mode = %grant.account_mode, ttl_secs = window.as_secs(),
        "ssh: grant recorded"
    );
    grants.push(grant);
    Ok(())
}

/// Drop everything past its deadline.
#[cfg(feature = "ssh-server")]
fn prune(grants: &mut Vec<Grant>) {
    let now = std::time::Instant::now();
    grants.retain(|g| {
        let live = g.deadline > now;
        if !live {
            tracing::debug!(grant_id = %g.grant_id, "ssh: grant expired unredeemed");
        }
        live
    });
}

/// Consume the live grant admitting `key`, if any. Single use: a redeemed
/// grant is gone, so a captured public key cannot be replayed into a second
/// session.
#[cfg(feature = "ssh-server")]
fn take_grant_for(key: &russh::keys::ssh_key::PublicKey) -> Option<Grant> {
    let mut grants = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut grants);
    let idx = grants.iter().position(|g| {
        russh::keys::ssh_key::PublicKey::from_openssh(&g.public_key)
            .is_ok_and(|k| k.key_data() == key.key_data())
    })?;
    Some(grants.remove(idx))
}

/// How many grants are pending — surfaced in diagnostics.
#[cfg(feature = "ssh-server")]
pub fn pending_grants() -> usize {
    let mut grants = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut grants);
    grants.len()
}

/// Test-only: empty the table so cases cannot leak into each other through
/// process-global state.
#[cfg(all(test, feature = "ssh-server"))]
fn clear_grants() {
    GRANTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(all(test, feature = "ssh-server"))]
mod tests {
    use roomler_ai_remote_control::models::{SshActivityEvent, SshActivityKind};
    use std::sync::Arc;

    use russh::keys::ssh_key::private::{Ed25519Keypair, PrivateKey};
    use tokio::net::{TcpListener, TcpStream};

    /// A client key pair: the private key to authenticate with, and its
    /// `authorized_keys` line for the server side.
    fn client_key(seed: u8) -> (PrivateKey, String) {
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
        let line = key.public_key().to_openssh().unwrap();
        (key, line)
    }

    fn cfg_with(authorized: Vec<String>) -> crate::config::AgentConfig {
        let mut cfg = crate::config::test_fixture();
        cfg.ssh_enabled = true;
        cfg.ssh_host_key = Some(super::generate_host_key().unwrap());
        cfg.ssh_authorized_keys = authorized;
        cfg
    }

    #[test]
    fn host_public_key_is_the_public_half_of_the_stored_key() {
        // The point of publishing this is that a client can compare what it
        // dialled against it, so it has to be the SAME key the server will
        // actually present — not merely a well-formed one.
        let cfg = cfg_with(vec![]);
        let published = super::host_public_key(&cfg);
        assert!(
            !published.is_empty(),
            "an SSH-enabled device must publish one"
        );

        let served = PrivateKey::from_openssh(cfg.ssh_host_key.as_deref().unwrap())
            .unwrap()
            .public_key()
            .to_openssh()
            .unwrap();
        assert_eq!(published, served);

        // And it must be parseable BY A CLIENT, since that is the only thing
        // that ever consumes it.
        let parsed = russh::keys::ssh_key::PublicKey::from_openssh(&published).unwrap();
        assert_eq!(parsed.algorithm().as_str(), "ssh-ed25519");
    }

    #[test]
    fn a_device_with_no_host_key_publishes_nothing_rather_than_junk() {
        // Empty is a meaningful answer on the wire ("cannot prove itself").
        // Returning a placeholder, or panicking, would both be worse: one
        // invites a false verify, the other takes the hello down over a
        // device that is simply not serving SSH.
        let mut cfg = cfg_with(vec![]);
        cfg.ssh_host_key = None;
        assert_eq!(super::host_public_key(&cfg), "");

        // Same for a corrupted key — `Ctx::build` already refuses to serve on
        // it, and publishing an identity the device cannot back would be the
        // one genuinely dangerous outcome.
        cfg.ssh_host_key = Some("-----BEGIN OPENSSH PRIVATE KEY-----\nnope\n".into());
        assert_eq!(super::host_public_key(&cfg), "");
    }

    struct TestClient;

    impl russh::client::Handler for TestClient {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            _key: &russh::keys::ssh_key::PublicKey,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    /// A fresh services bundle on a unique sentinel dir, exactly what the
    /// daemon hands the server — the startup mode is irrelevant because the
    /// server always prompts with an explicit per-session mode.
    fn test_services() -> super::SessionServices {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("roomler-ssh-consent-{nanos}"));
        super::SessionServices {
            // FR-55 — tests hold nothing awake. `None` is the real production
            // value anywhere no power keeper exists, not a stub.
            power_activity: None,
            consent: crate::consent::ConsentBroker::new(crate::consent::Mode::AutoGrant, dir)
                .expect("a temp sentinel dir"),
            indicator: crate::indicator::ViewerIndicator::disabled(),
            // Reporting off in tests unless a case opts in — the default a
            // device ships with.
            activity: None,
        }
    }

    /// Serve exactly one connection on a loopback port and hand back its
    /// address plus the consent broker the server was built with, so a test
    /// can play the operator. The transport is a plain TCP socket here on
    /// purpose: P1's tests already prove the interception path, so this
    /// exercises only the SSH layer sitting on top of it.
    async fn serve_one_with(
        cfg: &crate::config::AgentConfig,
    ) -> (std::net::SocketAddr, crate::consent::ConsentBroker) {
        let services = test_services();
        let broker = services.consent.clone();
        let ctx = super::sshd::Ctx::build(cfg, services).expect("a usable host key");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let handler = super::sshd::Handler::new(ctx.clone(), peer);
            if let Ok(session) =
                russh::server::run_stream(ctx.config.clone(), stream, handler).await
            {
                let _ = session.await;
            }
        });
        (addr, broker)
    }

    async fn serve_one(cfg: &crate::config::AgentConfig) -> std::net::SocketAddr {
        serve_one_with(cfg).await.0
    }

    async fn connect(
        addr: std::net::SocketAddr,
        key: PrivateKey,
    ) -> russh::client::Handle<TestClient> {
        let config = Arc::new(russh::client::Config::default());
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut session = russh::client::connect_stream(config, stream, TestClient)
            .await
            .expect("ssh handshake");
        let hash = session.best_supported_rsa_hash().await.unwrap().flatten();
        let res = session
            .authenticate_publickey(
                "roomler",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .expect("auth exchange");
        assert!(res.success(), "expected the authorized key to be accepted");
        session
    }

    #[test]
    fn a_generated_host_key_round_trips_as_ed25519() {
        let pem = super::generate_host_key().unwrap();
        let parsed = PrivateKey::from_openssh(&pem).expect("parses back");
        assert_eq!(
            parsed.algorithm(),
            russh::keys::ssh_key::Algorithm::Ed25519,
            "the host identity must be ed25519 so the `rsa` feature can stay off"
        );
        // Two calls must not produce the same key, or every device in the fleet
        // would share one host identity.
        assert_ne!(pem, super::generate_host_key().unwrap());
    }

    /// The property that makes `ssh_enabled` safe to ship default-off-but-on:
    /// reaching the port is not the same as getting in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_key_outside_the_list_cannot_authenticate() {
        let (_authorized_key, authorized_line) = client_key(1);
        let (stranger, _) = client_key(2);
        let addr = serve_one(&cfg_with(vec![authorized_line])).await;

        let config = Arc::new(russh::client::Config::default());
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut session = russh::client::connect_stream(config, stream, TestClient)
            .await
            .expect("ssh handshake");
        let hash = session.best_supported_rsa_hash().await.unwrap().flatten();
        let res = session
            .authenticate_publickey(
                "roomler",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(stranger), hash),
            )
            .await
            .expect("auth exchange completes");
        assert!(
            !res.success(),
            "a key absent from ssh_authorized_keys must be refused"
        );
    }

    /// An empty list is the default, and it must deny rather than allow.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_authorized_list_denies_everyone() {
        let (key, _) = client_key(3);
        let addr = serve_one(&cfg_with(Vec::new())).await;

        let config = Arc::new(russh::client::Config::default());
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut session = russh::client::connect_stream(config, stream, TestClient)
            .await
            .expect("ssh handshake");
        let hash = session.best_supported_rsa_hash().await.unwrap().flatten();
        let res = session
            .authenticate_publickey(
                "roomler",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .expect("auth exchange completes");
        assert!(!res.success(), "ssh_enabled alone must grant nobody access");
    }

    // ── The key-list account mode ─────────────────────────────────────────

    fn cfg_with_mode(authorized: Vec<String>, mode: Option<&str>) -> crate::config::AgentConfig {
        let mut cfg = cfg_with(authorized);
        cfg.ssh_account_mode = mode.map(str::to_string);
        cfg
    }

    #[test]
    fn account_mode_parses_the_documented_vocabulary() {
        use crate::exec::RunAs;
        assert_eq!(
            super::sshd::parse_account_mode("daemon").unwrap(),
            RunAs::Daemon
        );
        assert_eq!(
            super::sshd::parse_account_mode("console_user").unwrap(),
            RunAs::ConsoleUser
        );
        assert_eq!(
            super::sshd::parse_account_mode("named:goran").unwrap(),
            RunAs::Named("goran".into())
        );
        // Whitespace is tolerated; an empty account and an unknown word are not.
        assert_eq!(
            super::sshd::parse_account_mode(" named: goran ").unwrap(),
            RunAs::Named("goran".into())
        );
        assert!(super::sshd::parse_account_mode("named:").is_err());
        assert!(super::sshd::parse_account_mode("root").is_err());
        assert!(super::sshd::parse_account_mode("").is_err());
    }

    /// THE regression this slice exists for.
    ///
    /// A key-list session used to fall back to `RunAs::Daemon` — SYSTEM/root —
    /// no matter what the device policy said. Listing a key silently handed out
    /// the most privileged identity on the box, and a policy of
    /// `account_mode = console_user` was simply untrue for that path.
    #[test]
    fn an_unset_account_mode_refuses_rather_than_granting_the_daemon_identity() {
        let ctx = super::sshd::Ctx::build(
            &cfg_with_mode(vec![client_key(20).1], None),
            test_services(),
        )
        .expect("a usable host key");

        let err = ctx
            .key_list_run_as
            .as_ref()
            .expect_err("an unset account mode must NOT resolve to an identity");
        assert!(
            err.contains("ssh_account_mode"),
            "the refusal must name the key to set, got {err:?}"
        );

        // The specific bug: it must not be Daemon.
        assert!(
            !matches!(ctx.key_list_run_as, Ok(crate::exec::RunAs::Daemon)),
            "unset must never mean the daemon's own identity"
        );
    }

    #[test]
    fn a_set_account_mode_is_what_key_list_sessions_get() {
        let ctx = super::sshd::Ctx::build(
            &cfg_with_mode(vec![client_key(21).1], Some("console_user")),
            test_services(),
        )
        .expect("a usable host key");
        assert_eq!(
            ctx.key_list_run_as.as_ref().unwrap(),
            &crate::exec::RunAs::ConsoleUser
        );

        // `daemon` stays available — but only when asked for by name.
        let ctx = super::sshd::Ctx::build(
            &cfg_with_mode(vec![client_key(22).1], Some("daemon")),
            test_services(),
        )
        .expect("a usable host key");
        assert_eq!(
            ctx.key_list_run_as.as_ref().unwrap(),
            &crate::exec::RunAs::Daemon
        );
    }

    /// A malformed value must not degrade into a working-but-wrong identity.
    #[test]
    fn a_malformed_account_mode_refuses() {
        let ctx = super::sshd::Ctx::build(
            &cfg_with_mode(vec![client_key(23).1], Some("administrator")),
            test_services(),
        )
        .expect("a usable host key");
        assert!(ctx.key_list_run_as.is_err());
    }

    /// End to end over a real SSH connection: an authorized key with no account
    /// mode authenticates — so the operator gets a readable explanation rather
    /// than a bare auth failure — and then runs nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_key_list_session_without_an_account_mode_authenticates_but_runs_nothing() {
        let (key, line) = client_key(24);
        let addr = serve_one(&cfg_with_mode(vec![line], None)).await;
        let session = connect(addr, key).await; // auth SUCCEEDS

        let mut channel = session.channel_open_session().await.unwrap();
        channel.exec(true, "echo should-never-run").await.unwrap();

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => stdout.extend_from_slice(data),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    stderr.extend_from_slice(data)
                }
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }

        let out = String::from_utf8_lossy(&stdout);
        let err = String::from_utf8_lossy(&stderr);
        assert!(
            !out.contains("should-never-run"),
            "the command ran without an account mode — the silent-SYSTEM bug is back"
        );
        assert!(
            err.contains("ssh_account_mode"),
            "the caller must be told which key to set, got stderr {err:?}"
        );
        assert_eq!(exit, Some(1));
    }

    /// The end-to-end P2 claim: a real SSH client runs a real command through
    /// the daemon's real exec engine and gets its output and exit status.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_authorized_key_runs_a_command_and_gets_its_output() {
        let (key, line) = client_key(4);
        // `daemon` is stated explicitly. It used to be what an unset mode
        // silently produced, and this test passing without naming it was how
        // that went unnoticed — listing a key handed out SYSTEM/root and the
        // suite called it success.
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        channel.exec(true, "echo roomler-ssh-p2-ok").await.unwrap();

        let mut stdout = Vec::new();
        let mut exit = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => stdout.extend_from_slice(data),
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                russh::ChannelMsg::Close | russh::ChannelMsg::Eof => {}
                _ => {}
            }
        }

        let stdout = String::from_utf8_lossy(&stdout);
        assert!(
            stdout.contains("roomler-ssh-p2-ok"),
            "expected the command's output, got {stdout:?}"
        );
        assert_eq!(exit, Some(0), "a successful command reports exit 0");
    }

    /// A refused request must say why. `scp` turning into a silent hang is the
    /// specific failure this guards against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unsupported_requests_are_refused_with_a_reason() {
        let (key, line) = client_key(5);
        let addr = serve_one(&cfg_with(vec![line])).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        // `request_subsystem` only reports that the request was *sent*; the
        // verdict comes back on the channel, so read it rather than trusting
        // the send to mean acceptance.
        // `sftp` is served since P7, so the unsupported case needs a
        // subsystem we genuinely do not implement. The point of the test is
        // unchanged: a refusal must arrive WITH a reason.
        channel.request_subsystem(true, "netconf").await.unwrap();

        let mut refused = false;
        let mut reason = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Failure => {
                    refused = true;
                    break;
                }
                russh::ChannelMsg::Success => panic!("an unknown subsystem must not be accepted"),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    reason.extend_from_slice(data)
                }
                _ => {}
            }
        }

        assert!(
            refused,
            "the server must answer sftp with a channel failure"
        );
        let reason = String::from_utf8_lossy(&reason);
        assert!(
            reason.contains("subsystem"),
            "the refusal must say why — a silent failure is how a client turns into a hang; got {reason:?}"
        );
    }

    /// SFTP inherits the session's identity, so a session that resolved to NO
    /// account must not transfer files either.
    ///
    /// This is the same rule P5c set for commands — a key-list session with
    /// `ssh_account_mode` unset authenticates and then runs nothing rather
    /// than quietly taking SYSTEM/root. File transfer is the subsystem where
    /// getting that wrong would be worst, so it is asserted separately
    /// instead of assumed to follow.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sftp_is_refused_when_the_session_resolved_to_no_account() {
        let (key, line) = client_key(9);
        // `cfg_with` leaves `ssh_account_mode` unset — the default.
        let addr = serve_one(&cfg_with(vec![line])).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        channel.request_subsystem(true, "sftp").await.unwrap();

        let mut refused = false;
        let mut reason = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Failure => {
                    refused = true;
                    break;
                }
                russh::ChannelMsg::Success => {
                    panic!("sftp must not open for a session with no account")
                }
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    reason.extend_from_slice(data)
                }
                _ => {}
            }
        }
        assert!(refused, "sftp must be refused without a resolved account");
        assert!(
            !reason.is_empty(),
            "and the refusal must explain itself, or scp just hangs"
        );
    }

    // ── P3: server-minted grants ──────────────────────────────────────────

    /// The grant table is process-global, and `cargo test` runs these
    /// concurrently — so every grant test takes this first and starts from an
    /// empty table. Without it, the capped-table case (which deliberately
    /// overflows the table) evicts the grants the other cases are relying on,
    /// and the failures look like product bugs.
    static GRANT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Take the lock and hand back an empty table.
    async fn grant_test<'a>() -> tokio::sync::MutexGuard<'a, ()> {
        let guard = GRANT_TEST_LOCK.lock().await;
        super::clear_grants();
        guard
    }

    fn now_ms() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// A grant that needs no operator approval.
    ///
    /// `Auto` is stated explicitly rather than left to the default because the
    /// default is the OPPOSITE: an absent consent mode means prompt. These
    /// tests assert that a grant runs, so they have to say why nobody is asked.
    fn grant_for(line: &str, ttl_ms: u64) -> Result<String, String> {
        grant_for_with_consent(
            line,
            ttl_ms,
            Some(roomler_ai_remote_control::models::ConsentMode::Auto),
        )
    }

    /// Returns the grant id on success — it doubles as the consent sentinel,
    /// which the operator-approval tests need to answer the prompt.
    fn grant_for_with_consent(
        line: &str,
        ttl_ms: u64,
        consent: Option<roomler_ai_remote_control::models::ConsentMode>,
    ) -> Result<String, String> {
        let grant_id = format!("g-{}", rand::random::<u32>());
        super::record_grant(
            grant_id.clone(),
            line.to_string(),
            "alice@example.com".into(),
            "daemon".into(),
            None,
            consent,
            now_ms() + ttl_ms,
            0,
        )
        .map(|()| grant_id)
    }

    /// A grant admits its key, and admits it exactly once. Without single-use,
    /// a captured public key would be a standing credential for the whole
    /// grant lifetime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_grant_admits_its_key_exactly_once() {
        let _lock = grant_test().await;
        let (key, line) = client_key(10);
        // The device-owned list is EMPTY: this proves the grant alone let the
        // session in.
        let addr = serve_one(&cfg_with(Vec::new())).await;
        grant_for(&line, 30_000).unwrap();
        assert_eq!(super::pending_grants(), 1);

        let session = connect(addr, key.clone()).await;
        assert_eq!(
            super::pending_grants(),
            0,
            "redeeming a grant must consume it"
        );

        // Prove it is a working session, not just an accepted handshake.
        let mut channel = session.channel_open_session().await.unwrap();
        channel.exec(true, "echo granted").await.unwrap();
        let mut out = Vec::new();
        while let Some(msg) = channel.wait().await {
            if let russh::ChannelMsg::Data { ref data } = msg {
                out.extend_from_slice(data);
            }
        }
        assert!(String::from_utf8_lossy(&out).contains("granted"));

        // The same key a second time now has nothing backing it.
        let addr2 = serve_one(&cfg_with(Vec::new())).await;
        let config = Arc::new(russh::client::Config::default());
        let stream = TcpStream::connect(addr2).await.unwrap();
        let mut replay = russh::client::connect_stream(config, stream, TestClient)
            .await
            .unwrap();
        let hash = replay.best_supported_rsa_hash().await.unwrap().flatten();
        let res = replay
            .authenticate_publickey(
                "roomler",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .unwrap();
        assert!(!res.success(), "a redeemed grant must not be replayable");
    }

    /// An expired grant is not a grant. The deadline is derived from arrival,
    /// so this does not depend on the two clocks agreeing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_expired_grant_admits_nobody() {
        let _lock = grant_test().await;
        let (key, line) = client_key(11);
        let addr = serve_one(&cfg_with(Vec::new())).await;

        // Arrives already past its expiry — rejected outright rather than
        // stored as a live credential.
        assert!(grant_for(&line, 0).is_err());
        assert_eq!(super::pending_grants(), 0);

        let config = Arc::new(russh::client::Config::default());
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut session = russh::client::connect_stream(config, stream, TestClient)
            .await
            .unwrap();
        let hash = session.best_supported_rsa_hash().await.unwrap().flatten();
        let res = session
            .authenticate_publickey(
                "roomler",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .unwrap();
        assert!(!res.success());
    }

    /// The server can only ever SHORTEN the local window. A grant claiming a
    /// year of validity — a skewed clock, or a compromised control plane —
    /// still dies at the local ceiling.
    #[tokio::test]
    async fn a_far_future_expiry_is_clamped_to_the_local_ceiling() {
        use roomler_ai_remote_control::models::ssh_limits;

        let _lock = grant_test().await;
        let (_key, line) = client_key(12);
        grant_for(&line, 365 * 24 * 3600 * 1000).unwrap();

        let grants = super::GRANTS.lock().unwrap();
        let remaining = grants[0]
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        assert!(
            remaining.as_secs() <= ssh_limits::GRANT_TTL_SECS,
            "a grant must never outlive the local ceiling; got {remaining:?}"
        );
    }

    /// A malformed key is refused when the grant arrives, not discovered later
    /// as an unexplained authentication failure.
    #[tokio::test]
    async fn a_grant_with_an_unparseable_key_is_refused_on_arrival() {
        let _lock = grant_test().await;
        assert!(grant_for("ssh-ed25519 not-actually-base64", 30_000).is_err());
        assert_eq!(super::pending_grants(), 0);
    }

    /// The table cannot be grown without bound from the control plane.
    #[tokio::test]
    async fn the_pending_table_is_capped() {
        let _lock = grant_test().await;
        for i in 0..40u8 {
            let (_k, line) = client_key(100u8.wrapping_add(i));
            grant_for(&line, 30_000).unwrap();
        }
        assert!(
            super::pending_grants() <= super::MAX_PENDING_GRANTS,
            "pending grants must stay capped, got {}",
            super::pending_grants()
        );
    }

    // ── Resolution (A1) — the one place session entitlements are decided ──

    fn test_grant(
        consent: Option<roomler_ai_remote_control::models::ConsentMode>,
        session_secs: u64,
    ) -> super::Grant {
        super::Grant {
            grant_id: "g-1".into(),
            public_key: String::new(),
            caller: "alice@example.com".into(),
            account_mode: "daemon".into(),
            account: None,
            consent_mode: consent,
            session_secs,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
        }
    }

    fn test_peer() -> std::net::SocketAddr {
        "100.64.0.9:55555".parse().unwrap()
    }

    /// Only an explicit `Auto` skips the prompt. Everything else — including a
    /// server that said nothing — puts a human in the loop. The rule now lives
    /// in resolution, the only path a session's consent requirement can take.
    #[test]
    fn resolve_only_an_explicit_auto_skips_the_operator_prompt() {
        use roomler_ai_remote_control::models::ConsentMode;

        let auto = super::sshd::ResolvedSessionPolicy::from_grant(
            test_grant(Some(ConsentMode::Auto), 0),
            test_peer(),
            false,
        );
        assert!(
            auto.consent_sentinel.is_none(),
            "auto is the one mode that runs without asking"
        );
        for m in [
            Some(ConsentMode::Prompt),
            Some(ConsentMode::Email),
            Some(ConsentMode::Push),
            None, // the fail-safe: absent directive ⇒ ask
        ] {
            let resolved = super::sshd::ResolvedSessionPolicy::from_grant(
                test_grant(m, 0),
                test_peer(),
                false,
            );
            assert_eq!(
                resolved.consent_sentinel.as_deref(),
                Some("g-1"),
                "{m:?} must require approval and prompt on the grant id"
            );
        }
    }

    /// A grant session is ALWAYS time-bounded — 0 means the ceiling, never
    /// "forever" — and only the key-list break-glass path is unbounded.
    /// `session_secs` is the third member of the ignored-field class: it was
    /// carried, clamped and stored while nothing ever acted on it.
    #[test]
    fn resolve_bounds_grant_sessions_and_exempts_the_key_list() {
        use roomler_ai_remote_control::models::ssh_limits;

        let secs_until = |deadline: tokio::time::Instant| {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_secs()
        };

        let bounded = super::sshd::ResolvedSessionPolicy::from_grant(
            test_grant(None, 3600),
            test_peer(),
            false,
        );
        let left = secs_until(bounded.deadline.expect("a grant session has a deadline"));
        assert!(
            (3595..=3600).contains(&left),
            "expected ≈3600s, got {left}s"
        );

        let zero =
            super::sshd::ResolvedSessionPolicy::from_grant(test_grant(None, 0), test_peer(), false);
        let left = secs_until(zero.deadline.expect("0 must mean the ceiling, not forever"));
        assert!(
            (ssh_limits::MAX_SESSION_SECS - 5..=ssh_limits::MAX_SESSION_SECS).contains(&left),
            "expected ≈the ceiling, got {left}s"
        );

        // The break-glass path: unbounded, consent-exempt, honestly labelled.
        let ctx = super::sshd::Ctx::build(
            &cfg_with_mode(vec![client_key(25).1], Some("console_user")),
            test_services(),
        )
        .expect("a usable host key");
        let key_list =
            super::sshd::ResolvedSessionPolicy::from_key_list(&ctx, "roomler", test_peer());
        assert!(key_list.deadline.is_none());
        assert!(key_list.consent_sentinel.is_none());
        assert!(key_list.caller.contains("(key-list)"));
        assert_eq!(key_list.run_as.unwrap(), crate::exec::RunAs::ConsoleUser);
    }

    // ── M5: the device-side privilege ceiling ─────────────────────────────

    /// The hole this closes: every other gate on an SSH session is the
    /// server's, so a control plane that mints `account_mode = daemon` +
    /// `consent_mode = auto` gets an unattended SYSTEM/root shell on every
    /// device that answers. `ssh_max_privilege` is the device's own answer,
    /// and it is the only one that still holds when the server is the
    /// compromised thing.
    #[test]
    fn the_ceiling_refuses_a_daemon_grant_rather_than_downgrading_it() {
        // `test_grant` asks for `daemon` — the shape that matters.
        let refused =
            super::sshd::ResolvedSessionPolicy::from_grant(test_grant(None, 0), test_peer(), true);
        // REFUSED, not downgraded: a caller told they have a session that
        // silently is not the one they asked for reads every later permission
        // error as the command's problem.
        let why = refused
            .run_as
            .expect_err("a daemon grant must be refused, never quietly substituted");
        assert!(
            why.contains("ssh_max_privilege"),
            "the refusal must name the setting that caused it, got {why:?}"
        );
    }

    /// The ceiling bounds ONE thing. A grant that never wanted the daemon
    /// identity is untouched, and so is everything else the grant decides —
    /// otherwise "turn the ceiling on" would be a change nobody could scope.
    #[test]
    fn the_ceiling_leaves_a_console_user_grant_alone() {
        let mut grant = test_grant(None, 3600);
        grant.account_mode = "console_user".into();
        let resolved = super::sshd::ResolvedSessionPolicy::from_grant(grant, test_peer(), true);
        assert_eq!(
            resolved.run_as.expect("console_user is under the ceiling"),
            crate::exec::RunAs::ConsoleUser
        );
        assert_eq!(resolved.consent_sentinel.as_deref(), Some("g-1"));
        assert!(resolved.deadline.is_some());
    }

    /// Unset is PERMISSIVE, deliberately: a device-side ceiling that arrives
    /// switched on revokes a working configuration in the middle of a fleet
    /// roll, and the device you lose root SSH to is liable to be the one you
    /// needed it for. This test is the record of that decision — if it ever
    /// flips, it should be because someone chose to, not by drift.
    #[test]
    fn an_unset_ceiling_changes_nothing() {
        let cfg = cfg_with_mode(vec![client_key(26).1], Some("console_user"));
        assert!(cfg.ssh_max_privilege.is_none(), "unset is the default");
        let ctx = super::sshd::Ctx::build(&cfg, test_services()).expect("a usable host key");
        assert!(
            !ctx.deny_daemon_grants,
            "an unset ceiling must not refuse anything"
        );

        let allowed = super::sshd::ResolvedSessionPolicy::from_grant(
            test_grant(None, 0),
            test_peer(),
            ctx.deny_daemon_grants,
        );
        assert_eq!(
            allowed.run_as.expect("unset ⇒ pre-M5 behaviour"),
            crate::exec::RunAs::Daemon
        );
    }

    /// `console_user` is the ONLY value that arms it — a typo must not read
    /// as "on" (which would break a working device) and must not read as a
    /// silent "off" either, which is why `config set` rejects the value on
    /// the way in rather than here.
    #[test]
    fn only_console_user_arms_the_ceiling() {
        for (value, armed) in [
            (Some("console_user"), true),
            (Some("daemon"), false),
            (None, false),
        ] {
            let mut cfg = cfg_with_mode(vec![client_key(27).1], Some("console_user"));
            cfg.ssh_max_privilege = value.map(str::to_string);
            let ctx = super::sshd::Ctx::build(&cfg, test_services()).expect("a usable host key");
            assert_eq!(
                ctx.deny_daemon_grants, armed,
                "ssh_max_privilege = {value:?} should arm={armed}"
            );
        }
    }

    // ── Operator consent ──────────────────────────────────────────────────

    /// Drive one exec through a consent-gated session and collect the outcome.
    /// The operator's decision is played through the SAME broker the server
    /// was built with — `record_decision` only lands while the prompt is
    /// actually pending (the confused-deputy guard), so the approver polls.
    async fn exec_with_operator_decision(
        approve: bool,
        key_seed: u8,
    ) -> (String, String, Option<u32>, bool) {
        use roomler_ai_remote_control::models::ConsentMode;

        let (key, line) = client_key(key_seed);
        let (addr, broker) = serve_one_with(&cfg_with(Vec::new())).await;
        let grant_id = grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();

        let operator = {
            let sid = grant_id.clone();
            tokio::spawn(async move {
                for _ in 0..200 {
                    if broker.record_decision(&sid, approve) {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                false
            })
        };

        let session = connect(addr, key).await; // auth SUCCEEDS — consent is not auth
        let mut channel = session.channel_open_session().await.unwrap();
        channel.exec(true, "echo consent-gated-ran").await.unwrap();

        let (mut stdout, mut stderr, mut exit) = (Vec::new(), Vec::new(), None);
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => stdout.extend_from_slice(data),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    stderr.extend_from_slice(data)
                }
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }
        let decided = operator.await.unwrap();
        (
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
            exit,
            decided,
        )
    }

    /// THE positive half P5d could never test: a policy of `prompt` runs the
    /// command once — and only once — a human approves, end to end through a
    /// real broker. Before A2 the broker was a process global no test could
    /// safely publish, so only the refusal side was ever exercised.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_grant_requiring_consent_runs_after_the_operator_approves() {
        let _lock = grant_test().await;
        let (out, err, exit, decided) = exec_with_operator_decision(true, 30).await;
        assert!(decided, "the approval must have been recorded");
        assert!(
            err.contains("waiting up to"),
            "the caller must be warned it is waiting on a human, got stderr {err:?}"
        );
        assert!(
            out.contains("consent-gated-ran"),
            "an approved session must run the command, got {out:?}"
        );
        assert_eq!(exit, Some(0));
    }

    /// The refusal half: a denial refuses with the reason on stderr, and the
    /// command never runs. (`consent_mode` being IGNORED — the P5d bug — would
    /// run it regardless of the decision.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_grant_requiring_consent_refuses_when_the_operator_denies() {
        let _lock = grant_test().await;
        let (out, err, exit, decided) = exec_with_operator_decision(false, 32).await;
        assert!(decided, "the denial must have been recorded");
        assert!(
            !out.contains("consent-gated-ran"),
            "the command ran despite a denial — the ignored-consent_mode bug is back"
        );
        assert!(
            err.contains("denied"),
            "the caller must be told WHY it was refused, got stderr {err:?}"
        );
        assert_eq!(exit, Some(1));
    }

    /// THE session bound (A1): a grant's `session_secs` now ENDS the session.
    /// `ssh_limits::MAX_SESSION_SECS` always said "after which the agent
    /// closes it" — before this slice, nothing did, and the field was carried,
    /// clamped, stored and ignored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_granted_session_ends_at_its_time_bound() {
        use roomler_ai_remote_control::models::ConsentMode;

        let _lock = grant_test().await;
        let (key, line) = client_key(33);
        let addr = serve_one(&cfg_with(Vec::new())).await;
        super::record_grant(
            "g-deadline".into(),
            line,
            "alice@example.com".into(),
            "daemon".into(),
            None,
            Some(ConsentMode::Auto),
            now_ms() + 30_000,
            1, // a one-second session budget
        )
        .unwrap();

        let start = std::time::Instant::now();
        let session = connect(addr, key).await;
        let mut channel = session.channel_open_session().await.unwrap();
        // No command — just hold the channel and wait for the SERVER to end
        // the session. Nothing on the client side closes anything.
        let ended = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while channel.wait().await.is_some() {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "the server must disconnect a session past its granted lifetime"
        );
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(900),
            "the session ended before its budget: {:?}",
            start.elapsed()
        );
    }

    // ── Interactive sessions (P4a) ────────────────────────────────────────

    /// THE feature: `ssh <node>` with no command gets a real shell on a real
    /// terminal, types into it, and gets the output back.
    ///
    /// Deliberately asserts on `tty`'s answer rather than just on echoed text —
    /// a pair of pipes would also echo. Only a pty makes `tty` print a device.
    ///
    /// Unix-only because the ASSERTIONS are (`tty`, `tput`); the Windows twin
    /// below covers the same claim with what a console there can answer.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_interactive_session_gets_a_real_terminal() {
        let (key, line) = client_key(40);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        channel
            .request_pty(true, "xterm-256color", 120, 40, 0, 0, &[])
            .await
            .unwrap();
        channel.request_shell(true).await.unwrap();
        channel
            .data(&b"tty; echo COLS=$(tput cols); exit 0\n"[..])
            .await
            .unwrap();

        let mut out = Vec::new();
        let mut exit = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => out.extend_from_slice(data),
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("/dev/pts/") || text.contains("/dev/ttys"),
            "the session must be on a real tty, got {text:?}"
        );
        assert!(
            text.contains("COLS=120"),
            "the shell must see the geometry the client asked for, got {text:?}"
        );
        assert_eq!(exit, Some(0));
    }

    /// THE P4b feature, at the SSH layer: `ssh <windows-node>` with no command
    /// gets a real ConPTY shell, takes typed input, and streams back what the
    /// shell printed.
    ///
    /// The console host's own VT would arrive even from a BROKEN session (that
    /// was precisely the P4b symptom — title and `ESC[2J` and nothing else), so
    /// this asserts on a marker the SHELL produced. Geometry is not asserted:
    /// `mode con` reports the pseudoconsole's buffer, not the client's request,
    /// and pinning that would test the console host rather than us.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_interactive_session_gets_a_real_console() {
        let (key, line) = client_key(43);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        channel
            .request_pty(true, "xterm-256color", 120, 40, 0, 0, &[])
            .await
            .unwrap();
        channel.request_shell(true).await.unwrap();
        channel
            .data(&b"echo roomler-conpty-ssh-ok\r\nexit\r\n"[..])
            .await
            .unwrap();

        let mut out = Vec::new();
        let mut exit = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => out.extend_from_slice(data),
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("roomler-conpty-ssh-ok"),
            "the shell's own output must reach the client — the console host's \
             VT alone is exactly the P4b defect; got {text:?}"
        );
        assert!(exit.is_some(), "the session must report an exit status");
    }

    /// A shell without a terminal is refused with a reason rather than handed
    /// one it never asked for. Both platforms, one code path (P4b).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_shell_without_a_pty_is_refused_with_a_reason() {
        let (key, line) = client_key(41);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        channel.request_shell(true).await.unwrap();

        let mut refused = false;
        let mut reason = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Failure => {
                    refused = true;
                    break;
                }
                russh::ChannelMsg::Success => panic!("a shell with no pty must not be accepted"),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    reason.extend_from_slice(data)
                }
                _ => {}
            }
        }
        assert!(refused, "expected a channel failure");
        assert!(
            String::from_utf8_lossy(&reason).contains("terminal"),
            "the refusal must explain itself, got {:?}",
            String::from_utf8_lossy(&reason)
        );
    }

    /// The identity gate applies to interactive sessions exactly as it does to
    /// one-shot commands: an unset key-list account mode means the client gets
    /// a channel failure and a reason, not a root shell. Both platforms —
    /// the refusal happens before any terminal is allocated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_interactive_session_without_an_account_mode_gets_no_shell() {
        let (key, line) = client_key(42);
        let addr = serve_one(&cfg_with_mode(vec![line], None)).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        channel.request_shell(true).await.unwrap();

        let mut reason = Vec::new();
        let mut out = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => out.extend_from_slice(data),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    reason.extend_from_slice(data)
                }
                russh::ChannelMsg::Failure => break,
                _ => {}
            }
        }
        assert!(
            out.is_empty(),
            "no shell output may reach a session with no account mode, got {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            String::from_utf8_lossy(&reason).contains("ssh_account_mode"),
            "the refusal must name the key to set, got {:?}",
            String::from_utf8_lossy(&reason)
        );
    }

    /// The mode survives the trip into the grant table, so what the server
    /// asked for is what redemption sees.
    #[tokio::test]
    async fn a_recorded_grant_keeps_its_consent_mode() {
        use roomler_ai_remote_control::models::ConsentMode;

        let _lock = grant_test().await;
        let (key, line) = client_key(31);
        grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();

        let parsed = russh::keys::ssh_key::PublicKey::from_openssh(&line).unwrap();
        let taken = super::take_grant_for(&parsed).expect("the grant is redeemable");
        assert_eq!(taken.consent_mode, Some(ConsentMode::Prompt));
        let _ = key;
    }

    // ── P7b: the forward gate ────────────────────────────────────────────
    //
    // These lock the ONE thing that distinguishes an SSH forward from a
    // tunnel forward: the empty allowlist. Both read the same struct, and
    // the tunnel side treats empty as "trust the server" — so if someone
    // ever "unifies" the two readers, `an_empty_allowlist_forwards_nowhere`
    // is what fails, and it says why in its name.

    use crate::tunnel::acl::AgentForwardAcl;
    use roomler_ai_remote_control::models::{
        DestinationRule, HostPattern, PortRange, ProtocolKind,
    };

    fn acl_allowing(host: &str, low: u16, high: u16) -> AgentForwardAcl {
        AgentForwardAcl {
            enabled: true,
            allowlist: vec![DestinationRule {
                host_pattern: HostPattern::Exact(host.into()),
                port_range: PortRange { low, high },
                proto: ProtocolKind::Any,
            }],
        }
    }

    #[test]
    fn an_empty_allowlist_forwards_nowhere() {
        // The DEFAULT config. A tunnel would be allowed here (the server
        // already authorized it); an SSH forward has no such prior decision,
        // so it must be refused or every session is an open pivot.
        let acl = AgentForwardAcl::default();
        assert!(acl.allowlist.is_empty() && acl.enabled, "test premise");
        let why = super::sshd::forward_allowed(&acl, "db.intranet", 5432)
            .expect_err("default config must not forward");
        assert!(
            why.contains("forward_acl.allowlist"),
            "the refusal must name the key to set, got {why:?}"
        );
    }

    #[test]
    fn a_listed_destination_is_allowed_and_only_that_one() {
        let acl = acl_allowing("db.intranet", 5432, 5432);
        assert!(super::sshd::forward_allowed(&acl, "db.intranet", 5432).is_ok());
        // Same host, different port.
        assert!(super::sshd::forward_allowed(&acl, "db.intranet", 22).is_err());
        // Different host, listed port.
        assert!(super::sshd::forward_allowed(&acl, "other.intranet", 5432).is_err());
    }

    #[test]
    fn the_master_switch_beats_a_populated_allowlist() {
        let mut acl = acl_allowing("db.intranet", 5432, 5432);
        acl.enabled = false;
        let why = super::sshd::forward_allowed(&acl, "db.intranet", 5432)
            .expect_err("`enabled = false` must refuse everything");
        assert!(
            why.contains("forward_acl.enabled"),
            "the refusal must name the switch that closed it, got {why:?}"
        );
    }

    #[test]
    fn the_forward_ceiling_refuses_rather_than_queues() {
        // Matches `exec`'s semantics deliberately: a forward that opens two
        // minutes late is worse than one that fails now and says so. If this
        // ever becomes `acquire().await`, a wedged forward silently becomes a
        // wedged *session*.
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
            super::sshd::MAX_CONCURRENT_FORWARDS,
        ));
        let held: Vec<_> = (0..super::sshd::MAX_CONCURRENT_FORWARDS)
            .map(|_| sem.clone().try_acquire_owned().expect("under the cap"))
            .collect();
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "the {}th concurrent forward must be refused, not queued",
            super::sshd::MAX_CONCURRENT_FORWARDS + 1
        );
        // …and a finished splice returns its permit.
        drop(held);
        assert!(sem.try_acquire_owned().is_ok(), "permits must be released");
    }

    #[test]
    fn a_port_above_the_u16_range_is_not_silently_truncated() {
        // The wire carries a u32. 65536+22 must not become 22 — the
        // conversion lives in the handler, so this locks the reasoning
        // rather than the call.
        assert!(u16::try_from(65_536u32 + 22).is_err());
    }

    // ── P8: activity reporting ───────────────────────────────────────────

    /// Reporting is the DEVICE's decision and it is off unless asked for.
    /// `ActivitySink::new` returning `None` is what makes that structural —
    /// with the gate here, no call site can report from a device that never
    /// opted in, however many new event kinds get added later.
    #[test]
    fn a_device_that_did_not_opt_in_has_no_sink_at_all() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut cfg = crate::config::test_fixture();
        assert!(!cfg.ssh_activity_log, "off must be the shipped default");
        assert!(
            super::ActivitySink::new(&cfg, tx.clone()).is_none(),
            "no sink unless the device opted in"
        );

        cfg.ssh_activity_log = true;
        assert!(super::ActivitySink::new(&cfg, tx).is_some());
    }

    #[tokio::test]
    async fn a_report_carries_the_grant_id_and_the_command() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut cfg = crate::config::test_fixture();
        cfg.ssh_activity_log = true;
        let sink = super::ActivitySink::new(&cfg, tx).expect("opted in");

        sink.report(
            Some("grant-7".into()),
            "ssh:Someone@100.65.4.2:1",
            SshActivityKind::Exec,
            Some("uptime".into()),
            Some(0),
            true,
        );

        let msg = rx.try_recv().expect("a frame was queued");
        match msg {
            roomler_ai_remote_control::signaling::ClientMsg::SshActivity {
                grant_id,
                kind,
                detail,
                exit_code,
                allowed,
                ..
            } => {
                assert_eq!(grant_id.as_deref(), Some("grant-7"));
                assert_eq!(kind, SshActivityKind::Exec);
                assert_eq!(detail.as_deref(), Some("uptime"));
                assert_eq!(exit_code, Some(0));
                assert!(allowed);
            }
            other => panic!("expected SshActivity, got {other:?}"),
        }
    }

    /// A command line is about to leave the host, so it gets the same
    /// treatment `exec` gives its output. Without this, `curl -H 'Authorization:
    /// Bearer …'` would put a live token in the org's audit log.
    #[test]
    fn a_reported_command_is_redacted_before_it_leaves_the_host() {
        let out = super::redact_and_cap("curl -H 'Authorization: Bearer sk-not-a-real-token'");
        assert!(
            !out.contains("sk-not-a-real-token"),
            "the bearer token survived redaction: {out}"
        );
    }

    /// Truncation must be VISIBLE. A silently shortened command reads in the
    /// log as a different command from the one that ran.
    #[test]
    fn an_overlong_command_is_capped_and_marked() {
        let long = "x".repeat(SshActivityEvent::MAX_DETAIL * 2);
        let out = super::redact_and_cap(&long);
        assert_eq!(out.chars().count(), SshActivityEvent::MAX_DETAIL + 1);
        assert!(out.ends_with('…'), "truncation must be marked: {out}");
    }

    /// A full outbound queue must lose the log line, never block the session
    /// that produced it. `report` takes `&self` and cannot await, so this is
    /// really a lock on `try_send` never becoming `send`.
    #[test]
    fn a_full_queue_drops_the_report_instead_of_blocking() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut cfg = crate::config::test_fixture();
        cfg.ssh_activity_log = true;
        let sink = super::ActivitySink::new(&cfg, tx).expect("opted in");

        // Two reports into a 1-slot channel nobody is draining. The second has
        // nowhere to go; the call must still return.
        for _ in 0..2 {
            sink.report(None, "c", SshActivityKind::Shell, None, None, true);
        }
    }
}
