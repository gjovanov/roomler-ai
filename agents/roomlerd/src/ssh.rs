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
//!   daemon's existing [`crate::exec`] engine — since FR-89 its streaming
//!   variant, so output flows as it is produced with no ceiling and the
//!   channel owns the command's lifetime. PTY, SFTP and forwarding came later
//!   (P4, P7); anything else is refused with an explicit reason rather than
//!   left to hang.
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
/// The asker is non-optional on purpose. "No broker ⇒ deny" used to be a doc
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
    /// Who asks the person at the device before a session runs anything.
    pub asker: Asker,
    /// Where P8 activity reports go: this WS session's outbound queue.
    ///
    /// Scoped to the connection on purpose — a report belongs to the org
    /// whose control channel carried the grant, and a multi-org daemon must
    /// not leak one org's session activity onto another's WS. A reconnect
    /// builds a new sink; reports that raced the teardown are dropped, which
    /// is the right trade for a log line that must never block a session.
    pub activity: Option<ActivitySink>,
}

/// Who asks the person at the device before a session runs anything.
///
/// FR-105 — a session the SYSTEM service serves runs in session 0, where no
/// panel reaches anyone. The person is at the console, where the worker that
/// carried the connection runs, so the service asks THAT worker over the SSH
/// pipe, and the worker asks exactly as it does for a session of its own. A
/// worker that answers "yes" itself lets through only a session the server
/// already granted (FR-94's position on consent).
#[derive(Clone)]
pub enum Asker {
    /// This process asks.
    Here {
        /// The operator-consent broker — the same instance the tray and the
        /// LocalAPI decide through, so an SSH prompt resolves from any of
        /// them.
        broker: crate::consent::ConsentBroker,
        /// FR-27 — the native prompt surface, threaded here for the same
        /// reason the broker is: an SSH server is built inside the overlay's
        /// TUN factory, far from `signaling::run`, and "the prompt reached no
        /// screen" must not be a state a server can be constructed into.
        /// Field-measured 2026-08-29 on NEO16: with only the companion path an
        /// SSH prompt from mars timed out on a desktop whose native panel had
        /// just served three exec prompts in a row.
        indicator: crate::indicator::ViewerIndicator,
        /// Whether the desktop companion can show the question: started if it
        /// is installed and not running. [`Companion::System`] in the daemon;
        /// a test holds its start open with a fixed one (#3442).
        ///
        /// [`Companion::System`]: crate::companion::Companion::System
        companion: crate::companion::Companion,
    },
    /// FR-105 — the worker on the other end of this connection asks: the
    /// SYSTEM service's server, one relay per connection.
    Worker(crate::ssh_service::ConsentRelay),
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
    // FR-105 — on Windows, the SYSTEM service's port too: carried to the
    // service's pipe, never served here. Not when the kill switch is set, nor
    // when this node's own port is configured onto it.
    #[cfg(all(windows, feature = "ssh-server"))]
    let service = {
        use roomler_ai_remote_control::models::ssh_limits::SERVICE_PORT;
        if crate::ssh_service::disabled() {
            None
        } else if port == SERVICE_PORT {
            warn!(
                port,
                "ssh: ssh_port is the SYSTEM service's port — SSH through the service is off \
                 on this device"
            );
            None
        } else {
            Some((SERVICE_PORT, services.clone()))
        }
    };
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

        #[cfg(all(windows, feature = "ssh-server"))]
        let ports: Vec<u16> = match &service {
            Some((service_port, _)) => vec![port, *service_port],
            None => vec![port],
        };
        #[cfg(not(all(windows, feature = "ssh-server")))]
        let ports = [port];
        let split =
            SplitTun::wrap_ports(dev, ip, crate::overlay::netmask_to_prefix(nm), mtu, &ports);

        #[cfg(all(windows, feature = "ssh-server"))]
        if let Some((service_port, services)) = service.clone() {
            let weak = Arc::downgrade(&split);
            tokio::spawn(async move {
                let listener = {
                    let Some(dev) = weak.upgrade() else { return };
                    match dev.listen_on(service_port).await {
                        Ok(l) => l,
                        Err(e) => {
                            warn!(%e, port = service_port, "ssh: could not listen on the SYSTEM service's overlay port");
                            return;
                        }
                    }
                };
                carry_loop(listener, service_port, services).await;
            });
        }

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

/// FR-105 — carry each connection to the SYSTEM service's port to the
/// service, until the netstack goes away. Nothing is served here: the worker
/// holds no key for these sessions.
#[cfg(all(windows, feature = "ssh-server"))]
async fn carry_loop(
    mut listener: tunnel_core::overlay::netstack::NsListener,
    port: u16,
    services: SessionServices,
) {
    use tracing::info;

    info!(
        port,
        "ssh: carrying the SYSTEM service's overlay port to its pipe"
    );
    while let Some(stream) = listener.accept().await {
        let peer = stream.peer_addr();
        let services = services.clone();
        tokio::spawn(async move {
            match crate::ssh_service::carry(stream, peer, services).await {
                Ok(()) => info!(%peer, "ssh: a SYSTEM service session closed"),
                Err(e) => warn!(%peer, %e, "ssh: a SYSTEM service session ended"),
            }
        });
    }
    info!(
        port,
        "ssh: the SYSTEM service's port is no longer carried (session ended)"
    );
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

/// Whether the stored host key can actually be used — parsed, not merely
/// present.
///
/// The distinction is the whole point. `Some(garbage)` and `None` look the same
/// to every caller that only asks whether a key is configured, and they are
/// opposite states: `None` is a device that has not started SSH yet and will
/// mint one, while `Some(garbage)` is a device that believes it is done and
/// serves nothing, forever.
///
/// Measured in the field 2026-09-09 on a corp laptop running 0.4.96: its stored
/// key had picked up literal `\r` escape text inside the PEM, so its BEGIN line
/// was not newline-terminated (`PEM error in pre-encapsulation boundary`). The
/// daemon correctly refused to serve SSH and correctly published no host
/// pubkey — and then never recovered, because the mint in `main.rs` was gated
/// on `is_none()`. Every restart re-logged the same warning. Nothing surfaced
/// it: not `roomler status`, not `roomler config ls`, not the dashboard; and
/// what a caller saw was *"has not published an SSH host key … needs an agent
/// that has had SSH enabled at least once (rc.444+)"*, i.e. advice to upgrade an
/// agent that was already current.
#[cfg(feature = "ssh-server")]
pub fn stored_host_key_is_usable(cfg: &crate::config::AgentConfig) -> bool {
    cfg.ssh_host_key
        .as_deref()
        .is_some_and(|pem| russh::keys::ssh_key::PrivateKey::from_openssh(pem).is_ok())
}

#[cfg(not(feature = "ssh-server"))]
pub fn stored_host_key_is_usable(_cfg: &crate::config::AgentConfig) -> bool {
    true
}

/// FR-105 — how a worker answers the question its SYSTEM service relays.
/// #4140 — and, on every OS, how a test asks a question under the tray's
/// stand-in (`companion::tray_start_tests`).
#[cfg(all(any(windows, test), feature = "ssh-server"))]
pub(crate) use sshd::ask_here;

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

    use super::{ActivitySink, Asker};

    /// Everything a session needs, resolved once when the overlay device is
    /// built rather than per connection.
    pub struct Ctx {
        pub config: Arc<russh::server::Config>,
        /// The break-glass keys, read only when no server grant matches. Empty
        /// is the default and a safe state: a grant still signs in, a key-list
        /// session is refused, and the start-up log says so (#4325).
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
        /// FR-89 — `ssh_exec_streaming`: a one-shot command's output goes to
        /// the channel as it is produced, with no ceiling and no wall clock
        /// (the default), or — the kill switch — through the engine's
        /// buffered path with its 1 MiB ceiling and 300 s limit, exactly as
        /// before FR-89.
        pub exec_streaming: bool,
        /// FR-105 — the anchor's `ssh` ceiling, on the SYSTEM service's
        /// server; `None` on the worker's. The machine administrator's, in a
        /// file only SYSTEM and Administrators can write.
        pub service_ceiling: Option<roomler_ai_remote_control::models::SshServiceCeiling>,
        /// FR-105 — refuse every `direct-tcpip` forward, with this reason, on
        /// the SYSTEM service's server: the allowlist that would gate one
        /// there is the signed-in user's to edit.
        pub forward_refusal: Option<&'static str>,
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
            // #4325 — what the list as a whole leaves able to sign in.
            match super::key_list_line(cfg.ssh_authorized_keys.len(), authorized.len()) {
                Some(super::KeyListLine::Info(line)) => info!("{line}"),
                Some(super::KeyListLine::Warn(line)) => warn!("{line}"),
                None => {}
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
                exec_streaming = cfg.ssh_exec_streaming,
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
                exec_streaming: cfg.ssh_exec_streaming,
                service_ceiling: None,
                forward_refusal: None,
            }))
        }

        /// FR-105 — the SYSTEM service's server, built per connection: its
        /// own host key, the anchor's ceiling, and nothing from any config
        /// the signed-in user can write. No key list (that list is the user's
        /// to edit, so a key in it would be SYSTEM for the asking), and no
        /// forwards (their allowlist is the user's too). `services.asker` is
        /// that connection's worker ([`super::Asker::Worker`]). `None` when
        /// the key does not parse.
        pub fn for_service(
            host_key_pem: &str,
            ceiling: roomler_ai_remote_control::models::SshServiceCeiling,
            services: super::SessionServices,
        ) -> Option<Arc<Self>> {
            let host_key = match ssh_key::PrivateKey::from_openssh(host_key_pem) {
                Ok(k) => k,
                Err(e) => {
                    warn!(%e, "ssh-service: the service's host key is unreadable — not serving SSH");
                    return None;
                }
            };
            let fingerprint = host_key
                .public_key()
                .fingerprint(ssh_key::HashAlg::Sha256)
                .to_string();
            let config = russh::server::Config {
                // The worker's values: an abandoned session ends after ten
                // minutes of silence, and guessing is slowed.
                inactivity_timeout: Some(Duration::from_secs(600)),
                auth_rejection_time: Duration::from_secs(2),
                auth_rejection_time_initial: Some(Duration::from_secs(0)),
                keys: vec![host_key],
                ..Default::default()
            };
            info!(%fingerprint, ?ceiling, "ssh-service: serving a connection");
            Some(Arc::new(Self {
                config: Arc::new(config),
                authorized: Vec::new(),
                key_list_run_as: Err("keys in ssh_authorized_keys are not accepted on the \
                                      SYSTEM service's port: that list is the signed-in \
                                      user's to edit"
                    .to_string()),
                // The anchor's ceiling decides instead (`service_ceiling`).
                deny_daemon_grants: false,
                services,
                // Off as well as refused below, so no path to a forward
                // exists here even past `forward_refusal`.
                forward_acl: crate::tunnel::acl::AgentForwardAcl {
                    enabled: false,
                    allowlist: Vec::new(),
                },
                forwards: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FORWARDS)),
                exec_streaming: true,
                service_ceiling: Some(ceiling),
                forward_refusal: Some(
                    "port forwarding is not served through the SYSTEM service: the allowlist \
                     that would gate it is the signed-in user's to edit",
                ),
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
        /// The grant the operator is asked about before anything runs. Each
        /// question is filed under an id of its own, not this (#3442). `None`
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
        /// #2815 — who the consent prompt says is asking: the grant's caller
        /// as the server worded it, `«name» (via «device»)`, the way the
        /// screen-control and exec prompts word theirs. [`Self::caller`]
        /// stays the technical identity for the logs and activity reports;
        /// its `ssh:` prefix, overlay address and port mean nothing to the
        /// person deciding.
        pub prompt_name: String,
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
                // question is about this grant; the decision arrives under the
                // question's own id (#3442).
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
                prompt_name: caller.clone(),
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
                // Never shown: this path is exempt from consent. Were it ever
                // asked about, the username the client typed is all it has.
                prompt_name: user.to_string(),
                // No grant, so nothing to correlate to. Its activity is still
                // reported (it is the path most worth watching), just without
                // a decision row to join against — the `(key-list)` suffix on
                // `caller` is what identifies it.
                grant_id: None,
            }
        }

        /// FR-105 — hold a resolved grant to the anchor's `ssh` ceiling, on
        /// the SYSTEM service's server. It REFUSES, never downgrades (the M5
        /// rule): a caller who asked for SYSTEM and quietly got the signed-in
        /// user would read the device's answer as their command's. The server
        /// routes only grants the published ceiling permits, so this bites
        /// only when an administrator lowered it since.
        pub(super) fn within_service_ceiling(
            mut self,
            ceiling: roomler_ai_remote_control::models::SshServiceCeiling,
        ) -> Self {
            use roomler_ai_remote_control::models::{SshAccountMode, SshServiceCeiling};
            if let Ok(run_as) = &self.run_as {
                let mode = match run_as {
                    crate::exec::RunAs::Daemon => Some(SshAccountMode::Daemon),
                    crate::exec::RunAs::ConsoleUser => Some(SshAccountMode::ConsoleUser),
                    crate::exec::RunAs::Named(_) => None,
                };
                if !mode.is_some_and(|m| ceiling.permits(m)) {
                    let allowed = match ceiling {
                        SshServiceCeiling::Off => "not at all",
                        SshServiceCeiling::ConsoleUser => "only as the signed-in user",
                        SshServiceCeiling::System => "as SYSTEM or the signed-in user",
                    };
                    self.run_as = Err(format!(
                        "this device's administrator allows SSH through its SYSTEM service \
                         {allowed} (`roomlerd anchor ssh`), and this session asked to run as \
                         {}",
                        run_as.label()
                    ));
                }
            }
            self
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

    /// One channel's own state: the terminal its `pty_request` asked for, the
    /// input of the process it started, and that process's terminal.
    ///
    /// One connection carries several channels at once (commands, a terminal,
    /// `sftp`, `direct-tcpip` forwards), and russh names the channel on every
    /// callback. Each of these belongs to the channel that asked for it, so a
    /// channel's data, EOF, window change and pty request act on that
    /// channel's entry alone, never on another channel's process.
    #[derive(Default)]
    struct ChannelState {
        /// Terminal type and geometry from this channel's `pty_request`, held
        /// until its shell starts. `Some` is also what distinguishes an
        /// interactive request from a one-shot command.
        pty_req: Option<(String, crate::pty::WinSize)>,
        /// The client's bytes for this channel's process: keystrokes for a
        /// terminal, stdin for a command, the protocol for `sftp-server`.
        /// Dropped at this channel's EOF, which ends that input alone.
        input: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
        /// Resize half of this channel's terminal.
        terminal: Option<crate::pty::PtyHandle>,
        /// Set when the channel was opened as a `direct-tcpip` forward, the
        /// one kind besides `session` this server accepts, and alive while
        /// its splice runs. RFC 4254 §6 gives pty-req, shell, exec, subsystem
        /// and env to session channels only, so each of them on a forward is
        /// answered with a failure and starts nothing.
        forward: Option<std::sync::Weak<()>>,
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
        /// Each channel's pty request, process input and terminal, by the
        /// channel that asked for them ([`ChannelState`]).
        channels: std::collections::HashMap<ChannelId, ChannelState>,
        /// FR-89 — one abort per exec channel (FR-97 P4b: and per Windows console-user
        /// sftp channel), fired when the client closes that channel
        /// ([`channel_close`](russh::server::Handler::channel_close)) or when the session
        /// ends (`Drop`). A streamed command has no wall clock, so THIS is what ends
        /// one whose reader has gone; a token cancels synchronously, which is what
        /// `Drop` needs. Minted before the consent wait, so a channel closed during the
        /// prompt leaves nothing to run (the engine checks it before spawning).
        exec_aborts: std::collections::HashMap<ChannelId, tokio_util::sync::CancellationToken>,
        /// FR-55 — holds the machine awake for as long as this session lives.
        ///
        /// A field rather than a call pair, for the same reason `Drop` below
        /// carries the activity report: a session ends in many ways and only
        /// one of them is a path anyone would remember to annotate. Dropping
        /// the handler is the event they all share, so the guard rides along
        /// with it and cannot be leaked.
        _awake: Option<crate::power::ActivityGuard>,
        /// Who asks the person before anything runs: this process, or
        /// (FR-105) the worker that carried this connection.
        asker: Asker,
        /// #4168 — refused channels ([`answer_refusal`](Self::answer_refusal))
        /// whose close russh still holds behind the reason: the client's
        /// window had no room for all of it, or a key exchange was running.
        /// Until that close goes out, russh still delivers what the client
        /// sends on the channel; here a request on it is answered with a
        /// failure, and its data, EOF and window changes are dropped. A
        /// channel whose close went out at once is not kept: russh drops what
        /// arrives after it.
        refused: std::collections::HashSet<ChannelId>,
    }

    impl Handler {
        pub fn new(ctx: Arc<Ctx>, peer: std::net::SocketAddr) -> Self {
            // Taken before `ctx` moves into the struct below.
            let awake = ctx
                .services
                .power_activity
                .as_ref()
                .map(crate::power::ActivityGuard::new);
            let asker = ctx.services.asker.clone();
            Self {
                ctx,
                peer,
                policy: None,
                deadline_armed: false,
                channels: std::collections::HashMap::new(),
                exec_aborts: std::collections::HashMap::new(),
                _awake: awake,
                asker,
                refused: std::collections::HashSet::new(),
            }
        }

        /// Start the one-per-session watchdog that ends the connection when
        /// the granted lifetime runs out. A deadline already in the past
        /// fires immediately, so a channel opened late in a session cannot
        /// outrun the bound.
        ///
        /// Disconnecting is the whole enforcement: the pty pump's writes then
        /// fail, which drops the terminal and kills its process group, and an
        /// exec still in flight is cancelled by the handler's `Drop` (FR-89 —
        /// a streamed command has no wall clock of its own; the buffered
        /// kill-switch path is additionally bounded by the engine's).
        /// Key-list sessions have no deadline (see
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
                warn!(
                    ?caller,
                    "ssh: session reached its granted time limit — disconnecting"
                );
                let _ = handle
                    .disconnect(
                        russh::Disconnect::ByApplication,
                        "roomler-ssh: session time limit reached".to_string(),
                        String::new(),
                    )
                    .await;
            });
        }

        /// A channel opens: the state of every channel russh no longer holds
        /// goes. A channel the server closed (its process ended, or it was
        /// refused) is one of them: russh forgets a channel it closed, so the
        /// client's answering close never reaches `channel_close`, and its
        /// state, a finished terminal included, would stay until the
        /// connection ends. The channel opening now is not held until it is
        /// accepted, so it starts with no state either.
        ///
        /// russh has no `is_open`. The packet size it reports is 0 for a
        /// channel it does not hold, and this server's own (32 KiB, never 0)
        /// for one it does. A forward stays while its splice runs as well:
        /// russh holds an accepted channel only once its answer has gone
        /// out, which can come after the next open, and the forward's mark
        /// must be there for every request on it.
        fn channel_opens(&mut self, session: &Session) {
            self.channels.retain(|id, state| {
                session.max_packet_size(id) > 0
                    || state.forward.as_ref().is_some_and(|f| f.strong_count() > 0)
            });
        }

        /// A channel opened as a forward, not a session
        /// ([`ChannelState::forward`]).
        fn is_forward(&self, channel: ChannelId) -> bool {
            self.channels
                .get(&channel)
                .is_some_and(|c| c.forward.is_some())
        }

        /// Serve the `sftp` subsystem — which is also how modern `scp` moves
        /// files (OpenSSH 9 replaced the old rcp-over-exec protocol).
        ///
        /// **It spawns OpenSSH's `sftp-server` as the session's account rather
        /// than serving SFTP in-process**, and that is a privilege decision,
        /// not convenience. In-process file I/O would run as the DAEMON — root
        /// under systemd, SYSTEM in the service or a SystemContext worker — so
        /// a `console_user` session would silently touch every file as that.
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
                return self.refuse(session, channel, "sftp", "no session policy");
            };
            let run_as = match policy.run_as.clone() {
                Ok(r) => r,
                Err(why) => {
                    return self.refuse(session, channel, "sftp", &why);
                }
            };

            // Windows can only spawn as another user through
            // `CreateProcessAsUserW`, which `tokio::process` cannot do: a
            // `console_user` session takes `exec`'s console-user spawn instead
            // (FR-97 P4b, below). A named account is refused there as it is
            // everywhere on Windows: becoming one needs its credentials.
            #[cfg(windows)]
            if let crate::exec::RunAs::Named(account) = &run_as {
                return self.refuse(
                    session,
                    channel,
                    "sftp",
                    &format!(
                        "cannot transfer files as the local account {account:?}: becoming an \
                         arbitrary user on Windows requires that user's credentials. Use \
                         account_mode `console_user` for the signed-in user."
                    ),
                );
            }
            #[cfg(windows)]
            if matches!(run_as, crate::exec::RunAs::ConsoleUser) && !sftp_as_user_enabled() {
                return self.refuse(
                    session,
                    channel,
                    "sftp",
                    "SFTP as the signed-in user is switched off on this device \
                     (ROOMLERD_SFTP_AS_USER=0).",
                );
            }

            let program = match sftp_server_path() {
                Some(p) => p,
                None => {
                    return self.refuse(
                        session,
                        channel,
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
            let prompt_name = policy.prompt_name.clone();
            let consent = policy.consent_sentinel.clone();
            let asker = self.asker.clone();

            if let Some(sentinel) = consent
                && let Err(why) = await_consent(
                    &asker,
                    &handle,
                    channel,
                    &prompt_name,
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

            // FR-97 P4b — Windows `console_user`: `sftp-server` as the person
            // signed in at the console, through `CreateProcessAsUserW`.
            #[cfg(windows)]
            if matches!(run_as, crate::exec::RunAs::ConsoleUser) {
                self.sftp_as_console_user(program, handle, channel, caller);
                return Ok(());
            }

            let mut cmd = tokio::process::Command::new(&program);
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            // FR-97 P5b — in the person's home, where sshd starts `sftp-server`:
            // a client resolves every relative path against it.
            if let Err(why) = crate::exec::apply_run_as_at_home(&mut cmd, &run_as).await {
                let _ = handle
                    .extended_data(channel, 1, format!("roomler: {why}\r\n").into_bytes())
                    .await;
                let _ = handle.close(channel).await;
                return Ok(());
            }
            // It keeps its three pipes and nothing else, as every program a
            // session runs does.
            #[cfg(unix)]
            tunnel_core::child::inherit_only_stdio_async(&mut cmd);

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
                peer = %self.peer, ?caller, run_as = ?run_as.label(),
                privileged = run_as.is_privileged(), server = %program.display(),
                "ssh: sftp session started"
            );

            // Bounded, for the same reason the pty's queue is: a client
            // streaming a large upload must not grow the daemon's memory
            // faster than sftp-server drains it. This channel's own input.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            self.channels.entry(channel).or_default().input = Some(tx);

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
                // as a zombie holding the user's files open — but WAIT for it
                // first rather than killing it. It closed stdout because it is
                // finishing, and a kill here would throw away the exit code the
                // client is about to be told.
                //
                // ⚠️ That status is not a nicety: `scp` reads it as the verdict
                // on the whole transfer. Without an `exit-status` message
                // OpenSSH reports `Exit status -1` and exits 1 — so before this
                // every scp against a roomler device FAILED on the caller's
                // side while the file arrived complete and byte-correct on the
                // device (measured 2026-09-09 from mars to a corp laptop: 2 MiB
                // uploaded, SHA matched on the far end, `scp rc=1`). A script
                // that branches on that status retries a transfer that already
                // worked. Every other channel path here — pty, exec, and each
                // refusal — sends it; the sftp subsystem was the one that did
                // not.
                let status =
                    match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                        .await
                    {
                        Ok(Ok(st)) => st.code().unwrap_or(0).max(0) as u32,
                        Ok(Err(_)) => 1,
                        Err(_) => {
                            // Only now, and the status is the timeout's, not the
                            // child's — a hung sftp-server must not hang the
                            // channel too.
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            1
                        }
                    };
                let _ = handle.exit_status_request(channel, status).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            });

            // sftp-server writes diagnostics here. They belong in OUR log, not
            // on the client's channel: the SFTP stream is a binary protocol
            // and injecting text into it corrupts the transfer. One escaped
            // line when it closes its stderr, 4 KiB at most (#3890).
            if let Some(mut stderr) = stderr {
                let peer = self.peer;
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut said = SftpStderr::default();
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match stderr.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => said.push(&buf[..n]),
                        }
                    }
                    said.log(peer);
                });
            }

            Ok(())
        }

        /// FR-97 P4b — the Windows `console_user` half of
        /// [`start_sftp_session`](Self::start_sftp_session): `sftp-server` as
        /// the person signed in at the console, through `exec`'s
        /// `CreateProcessAsUserW` spawn, which `tokio::process` cannot do.
        ///
        /// The same splice as the `tokio::process` path, for the same reasons:
        /// the client's bytes feed its stdin through a bounded queue, and the
        /// client's EOF (the queue's last sender dropping) ends its input. Its
        /// stdout reaches the channel raw, never redacted, because SFTP is a
        /// binary protocol. Its stderr goes to this log, never the channel, as
        /// one escaped line when it ends ([`SftpStderr`], #3890).
        /// When it exits, the client gets the exit status (`scp`'s verdict on
        /// the whole transfer), then EOF, then close. A client that has gone
        /// kills the process tree: the channel closed or the session ended
        /// (its abort sits beside the exec channels', FR-89), or a send
        /// failed. It starts in the person's profile, where OpenSSH starts
        /// it, so a relative path lands in their home.
        #[cfg(windows)]
        fn sftp_as_console_user(
            &mut self,
            program: std::path::PathBuf,
            handle: russh::server::Handle,
            channel: ChannelId,
            caller: String,
        ) {
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            self.channels.entry(channel).or_default().input = Some(tx);
            let (raw_tx, mut raw_rx) = tokio::sync::mpsc::channel::<crate::exec::Chunk>(4);
            let abort = tokio_util::sync::CancellationToken::new();
            self.exec_aborts.insert(channel, abort.clone());
            let peer = self.peer;
            info!(
                %peer, ?caller, run_as = "console_user", privileged = false,
                server = %program.display(),
                "ssh: sftp session started (CreateProcessAsUserW)"
            );

            let pump_handle = handle.clone();
            let pump_abort = abort.clone();
            let pump = tokio::spawn(async move {
                let mut said = SftpStderr::default();
                while let Some(chunk) = raw_rx.recv().await {
                    match chunk.stream {
                        crate::exec::OutStream::Stdout => {
                            if pump_handle.data(channel, chunk.bytes).await.is_err() {
                                pump_abort.cancel();
                                break;
                            }
                        }
                        crate::exec::OutStream::Stderr => said.push(&chunk.bytes),
                    }
                }
                said.log(peer);
            });

            let program = program.to_string_lossy().into_owned();
            tokio::spawn(async move {
                let outcome =
                    crate::exec::run_console_user_program(&program, rx, raw_tx, abort).await;
                // Every byte reaches the client before its verdict does.
                let _ = pump.await;
                if let Some(e) = &outcome.error {
                    warn!(%peer, error = %e, "ssh: sftp-server as the console user");
                    // The stderr stream, beside the binary one: the client
                    // prints it, and the transfer was over anyway.
                    let _ = handle
                        .extended_data(channel, 1, format!("roomler: {e}\r\n").into_bytes())
                        .await;
                }
                let status = outcome.exit_code.map(|c| c.max(0) as u32).unwrap_or(1);
                let _ = handle.exit_status_request(channel, status).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            });
        }

        /// Start an interactive session on the terminal this channel's
        /// `pty_request` asked for.
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
            let Some((term, size)) = self.channels.get(&channel).and_then(|c| c.pty_req.clone())
            else {
                return self.refuse(
                    session,
                    channel,
                    "shell",
                    "no terminal was requested for this session",
                );
            };

            // The session's whole entitlement was resolved at auth; requests
            // only READ it. Absent means unauthenticated, which russh does
            // not deliver requests for — refuse rather than reason about it.
            let Some(policy) = &self.policy else {
                return self.refuse(session, channel, "shell", "no session policy");
            };
            let run_as = match policy.run_as.clone() {
                Ok(r) => r,
                Err(e) => {
                    warn!(peer = %self.peer, %e, "ssh: refusing a shell — unusable account mode");
                    // #4168 — answered as `refuse` answers, so `ssh` shows it.
                    return self.answer_refusal(session, channel, &e.to_string());
                }
            };

            session.channel_success(channel)?;
            let handle = session.handle();
            let caller = policy.caller.clone();
            let prompt_name = policy.prompt_name.clone();
            let consent = policy.consent_sentinel.clone();
            let asker = self.asker.clone();

            if let Some(sentinel) = consent
                && let Err(why) = await_consent(
                    &asker,
                    &handle,
                    channel,
                    &prompt_name,
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

            // #3822 — a Unix spawn asks who `console_user` is (logind, on
            // Linux) from this task, so it is awaited; a Windows spawn takes
            // the person's token itself.
            #[cfg(unix)]
            let spawned =
                crate::pty::PtyProcess::spawn(command.as_deref(), &term, size, &run_as).await;
            #[cfg(windows)]
            let spawned = crate::pty::PtyProcess::spawn(command.as_deref(), &term, size, &run_as);
            let pty = match spawned {
                Ok(p) => p,
                Err(e) => {
                    warn!(peer = %self.peer, ?e, "ssh: could not start the terminal session");
                    let _ = handle
                        .extended_data(channel, 1, format!("roomler-ssh: {e}\r\n").into_bytes())
                        .await;
                    let _ = handle.exit_status_request(channel, 1).await;
                    let _ = handle.close(channel).await;
                    return Ok(());
                }
            };

            info!(
                peer = %self.peer, ?caller, run_as = ?run_as.label(),
                privileged = run_as.is_privileged(), ?term,
                "ssh: interactive session started"
            );

            // A bounded queue: an operator holding a key down must not be able
            // to grow the daemon's memory faster than the shell drains it.
            // This channel's own input and terminal.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            let state = self.channels.entry(channel).or_default();
            state.input = Some(tx);
            state.terminal = Some(pty.handle());

            let writer = pty.handle();
            tokio::spawn(async move {
                while let Some(chunk) = rx.recv().await {
                    if writer.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            });

            // #3303 — `channel_close` and this handler's `Drop` fire this, as they
            // do for a command. The pump cannot hear a client go by itself: a
            // shell at a prompt writes nothing that could fail.
            let abort = tokio_util::sync::CancellationToken::new();
            self.exec_aborts.insert(channel, abort.clone());

            let caller_for_pump = caller.clone();
            tokio::spawn(async move {
                let mut pty = pty;
                let mut buf = vec![0u8; 16 * 1024];
                let client_gone = loop {
                    let read = tokio::select! {
                        read = pty.read(&mut buf) => read,
                        _ = abort.cancelled() => break true,
                    };
                    match read {
                        Ok(0) => break false,
                        Ok(n) => {
                            // A client that only stopped reading parks this, and
                            // the abort ends the wait.
                            let sent = tokio::select! {
                                sent = handle.data(channel, buf[..n].to_vec()) => sent.is_ok(),
                                _ = abort.cancelled() => false,
                            };
                            if !sent {
                                break true;
                            }
                        }
                        Err(e) => {
                            warn!(%e, "ssh: terminal read failed");
                            break false;
                        }
                    }
                };
                if client_gone {
                    // Nobody is left to tell how it ended, and waiting for that
                    // would keep a shell alive against a channel nobody reads:
                    // `pty` drops here, which ends the session.
                    info!(caller = ?caller_for_pump, "ssh: interactive session ended: its client is gone");
                    return;
                }
                let status = pty.wait().await;
                info!(caller = ?caller_for_pump, status, "ssh: interactive session ended");
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
    ///
    /// FR-89 — the same event ends every exec the session still has running:
    /// a client that disconnected (or whose carrier died, or whose grant
    /// reached its time limit) no longer has a channel the output could go
    /// to, and a streamed command has no wall clock that would end it
    /// otherwise. Cancelling a token is synchronous, so this needs no
    /// runtime either.
    impl Drop for Handler {
        fn drop(&mut self) {
            self.report(SshActivityKind::SessionClose, None, None, true);
            for (_, abort) in self.exec_aborts.drain() {
                abort.cancel();
            }
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
                    peer = %self.peer, ?user, %fingerprint,
                    grant_id = %grant.grant_id, caller = ?grant.caller,
                    account_mode = %grant.account_mode,
                    "ssh: authenticated (server grant)"
                );
                // Resolving HERE — not lazily at each request — is the P5
                // design: one place turns a grant into what a session may do,
                // and the request handlers can only read the result.
                let mut policy = ResolvedSessionPolicy::from_grant(
                    grant,
                    self.peer,
                    self.ctx.deny_daemon_grants,
                );
                // FR-105 — on the SYSTEM service's server, the anchor's
                // ceiling as well.
                if let Some(ceiling) = self.ctx.service_ceiling {
                    policy = policy.within_service_ceiling(ceiling);
                }
                self.policy = Some(policy);
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
                    peer = %self.peer, ?user, %fingerprint,
                    "ssh: authenticated (ssh_authorized_keys)"
                );
                self.policy = Some(ResolvedSessionPolicy::from_key_list(
                    &self.ctx, user, self.peer,
                ));
                self.report(SshActivityKind::SessionOpen, None, None, true);
                Ok(Auth::Accept)
            } else {
                warn!(
                    peer = %self.peer, ?user, %fingerprint,
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
            // #4168 — a refused channel whose close has gone out since is let
            // go, so `refused` never holds more than the channels russh itself
            // still holds, and an id russh hands out again starts clean.
            self.refused.retain(|&c| session.has_pending_data(c));
            self.channel_opens(session);
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
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            use russh::ChannelOpenFailure;

            // A channel opening all the same. A forward's bytes go to its own
            // stream (below); its only state here is its mark, set once the
            // forward is accepted.
            self.channel_opens(session);
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

            // FR-105 — no forward at all on the SYSTEM service's server.
            let verdict = match self.ctx.forward_refusal {
                Some(why) => Err(why.to_string()),
                None => forward_allowed(&self.ctx.forward_acl, &host, port),
            };
            if let Err(why) = verdict {
                // `ssh` renders this as "administratively prohibited", which
                // tells the caller it was a policy decision and not a dead
                // destination. The *reason* stays here, next to the operator
                // who can act on it — it names internal topology.
                warn!(peer = %self.peer, ?caller, ?host, port, ?why, "ssh: forward refused");
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
                    peer = %self.peer, ?caller, ?host, port,
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
                    warn!(peer = %self.peer, ?host, port, %e, "ssh: forward could not connect");
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                    return Ok(());
                }
            };
            let _ = target.set_nodelay(true);

            // Marked before the answer goes out, so the first request on it
            // finds it: no session request runs on a forward.
            let splicing = std::sync::Arc::new(());
            self.channels.entry(channel.id()).or_default().forward =
                Some(std::sync::Arc::downgrade(&splicing));
            reply.accept().await;
            info!(peer = %self.peer, ?caller, ?host, port, "ssh: forward opened");
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
                drop(splicing);
            });
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // #4168 — nothing runs on a refused channel (see `refused`).
            if self.refused.contains(&channel) {
                return session.channel_failure(channel);
            }
            // Nor on a forward (see `ChannelState::forward`).
            if self.is_forward(channel) {
                return session.channel_failure(channel);
            }
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
            let prompt_name = policy.prompt_name.clone();
            let consent = policy.consent_sentinel.clone();
            let asker = self.asker.clone();
            // Captured here, where the policy is still borrowed, and reported
            // from inside the task once the exit code exists.
            let activity = self
                .ctx
                .services
                .activity
                .clone()
                .map(|s| (s, policy.grant_id.clone()));
            // The client's stdin, to the command. Without this `data()` had
            // nowhere to put it and dropped every byte, so the command read
            // NUL and `ssh node 'cat > f' < file` wrote an empty file with
            // exit 0. Bounded like the pty's and sftp's queues: a client
            // streaming into a command that is not reading is held back by
            // SSH flow control rather than by the daemon's memory. Bytes that
            // arrive while consent is pending wait here; a refusal drops them.
            // This channel's own stdin: another channel's bytes never reach it.
            let (tx, stdin) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            self.channels.entry(channel).or_default().input = Some(tx);
            // FR-89 — minted HERE, before the consent wait, so a channel that
            // closes while the operator is being asked leaves nothing to run.
            let abort = tokio_util::sync::CancellationToken::new();
            self.exec_aborts.insert(channel, abort.clone());
            let job = ExecJob {
                handle,
                channel,
                command,
                caller,
                prompt_name,
                run_as,
                consent,
                asker,
                activity,
                stdin,
                abort,
                streaming: self.ctx.exec_streaming,
            };
            tokio::spawn(run_exec(job));
            Ok(())
        }

        /// A shell request without a prior PTY request on its own channel is
        /// a non-interactive shell — rare, and not what this exists to serve.
        /// Refused with a reason rather than silently handed a terminal the
        /// client never asked for, or one another channel asked for.
        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // #4168 — nor does a shell (see `refused`).
            if self.refused.contains(&channel) {
                return session.channel_failure(channel);
            }
            // Nor on a forward (see `ChannelState::forward`).
            if self.is_forward(channel) {
                return session.channel_failure(channel);
            }
            if self
                .channels
                .get(&channel)
                .is_some_and(|c| c.pty_req.is_some())
            {
                // The shell's CONTENTS are never recorded; this row exists so
                // a reader can see that an interactive session happened.
                self.report(SshActivityKind::Shell, None, None, true);
                self.start_pty_session(channel, None, session).await
            } else {
                self.refuse(
                    session,
                    channel,
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
            // #4168 — a refused channel gets no terminal (see `refused`).
            if self.refused.contains(&channel) {
                return session.channel_failure(channel);
            }
            // Nor does a forward (see `ChannelState::forward`).
            if self.is_forward(channel) {
                return session.channel_failure(channel);
            }
            // Recorded, not acted on: the terminal is allocated when the
            // shell starts, because that is when the identity it runs as
            // is known and when consent has been settled. Allocating here
            // would leave a pty (and later a shell) alive for a session
            // that is about to be refused. Recorded for THIS channel: a
            // shell on another channel never takes it.
            self.channels.entry(channel).or_default().pty_req = Some((
                term.to_string(),
                crate::pty::WinSize {
                    cols: col_width as u16,
                    rows: row_height as u16,
                },
            ));
            info!(peer = %self.peer, ?term, cols = col_width, rows = row_height, "ssh: pty requested");
            session.channel_success(channel)?;
            Ok(())
        }

        /// Client bytes for this channel's process: keystrokes for a terminal,
        /// the client's stdin for an exec'd command, the protocol for
        /// `sftp-server`. Never another channel's. With nothing to deliver
        /// to (no process yet, a forward, whose bytes russh hands to its own
        /// stream, or a process that has ended) the data is dropped.
        async fn data(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            // #4168 — a refused channel's bytes reach no process (see `refused`).
            if self.refused.contains(&channel) {
                return Ok(());
            }
            if let Some(tx) = self.channels.get(&channel).and_then(|c| c.input.as_ref()) {
                let _ = tx.send(data.to_vec()).await;
            }
            Ok(())
        }

        /// The client's window changed. Resizing this channel's terminal is
        /// what makes the far side reflow. On Unix the kernel also SIGWINCHes
        /// the foreground group, which is how a full-screen application learns
        /// to redraw; ConPTY delivers the equivalent through the console host.
        /// A channel with no terminal of its own resizes nothing.
        async fn window_change_request(
            &mut self,
            channel: ChannelId,
            col_width: u32,
            row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            // #4168 — nor resize a terminal (see `refused`).
            if self.refused.contains(&channel) {
                return Ok(());
            }
            if let Some(h) = self
                .channels
                .get(&channel)
                .and_then(|c| c.terminal.as_ref())
            {
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

        /// The client stopped sending on this channel: its process's input
        /// ends, and no other's. A command or `sftp-server` reads end-of-input
        /// and finishes on its own; a terminal takes no more keystrokes, and
        /// its shell runs until it exits or the channel closes.
        async fn channel_eof(
            &mut self,
            channel: ChannelId,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            // #4168 — nor end a process's input (see `refused`).
            if self.refused.contains(&channel) {
                return Ok(());
            }
            if let Some(state) = self.channels.get_mut(&channel) {
                state.input = None;
            }
            Ok(())
        }

        /// The client closed the channel (FR-89). A command still running on
        /// it has nowhere to write to — russh accepts and DISCARDS data for a
        /// closed channel, so the pump cannot tell — and no wall clock would
        /// end it, so it is ended here: the abort kills the process tree.
        /// A channel whose command already finished has a token that fires
        /// into nothing. The channel's input, terminal and pty request go
        /// with it.
        async fn channel_close(
            &mut self,
            channel: ChannelId,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            if let Some(abort) = self.exec_aborts.remove(&channel) {
                abort.cancel();
            }
            self.channels.remove(&channel);
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            channel: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // #4168 — nor a subsystem (see `refused`).
            if self.refused.contains(&channel) {
                return session.channel_failure(channel);
            }
            // Nor on a forward (see `ChannelState::forward`).
            if self.is_forward(channel) {
                return session.channel_failure(channel);
            }
            if name == "sftp" {
                // File operations happen inside `sftp-server`; this records
                // only that a transfer channel was opened.
                self.report(SshActivityKind::Sftp, None, None, true);
                return self.start_sftp_session(channel, session).await;
            }
            self.refuse(
                session,
                channel,
                name,
                "only the `sftp` subsystem is implemented.",
            )
        }

        /// A variable for the session's environment. Nothing here takes one,
        /// and a session channel's request is left unanswered, as russh's
        /// default leaves it. On a forward it is answered with a failure
        /// (see [`ChannelState::forward`]).
        async fn env_request(
            &mut self,
            channel: ChannelId,
            _variable_name: &str,
            _variable_value: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if self.is_forward(channel) {
                return session.channel_failure(channel);
            }
            Ok(())
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
        #[cfg(test)]
        if let Some(p) = STAND_IN_SFTP_SERVER.with(|s| s.borrow().clone()) {
            return Some(p);
        }
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

    #[cfg(test)]
    thread_local! {
        /// Tests only: the `sftp-server` a server on this thread starts in
        /// place of OpenSSH's (#3890). A `#[tokio::test]` serves its
        /// connection on its own thread, so no other test's server sees it.
        pub(super) static STAND_IN_SFTP_SERVER: std::cell::RefCell<Option<std::path::PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// #3890 — the most of `sftp-server`'s stderr the daemon's log keeps.
    const SFTP_STDERR_KEPT: usize = 4096;

    /// #3890 — what `sftp-server` wrote to its stderr, kept for ONE log line:
    /// its first [`SFTP_STDERR_KEPT`] bytes and a count of the bytes after
    /// them, so a child that writes a lot grows neither the daemon's memory
    /// nor one line of its log. Both spawns feed it: the `tokio::process` one
    /// from the pipe, the Windows `console_user` one chunk by chunk.
    #[derive(Default)]
    struct SftpStderr {
        kept: Vec<u8>,
        cut: usize,
    }

    impl SftpStderr {
        fn push(&mut self, bytes: &[u8]) {
            let take = SFTP_STDERR_KEPT
                .saturating_sub(self.kept.len())
                .min(bytes.len());
            self.kept.extend_from_slice(&bytes[..take]);
            self.cut += bytes.len() - take;
        }

        /// One `WARN` line once the child has closed its stderr; none for a
        /// stderr of whitespace alone. The text is ONE field, escaped (`?`),
        /// like the text the server takes from the wire (#3815): a line
        /// break or a control character the child wrote, or a path it quotes
        /// from the client, stays inside this line.
        fn log(&self, peer: std::net::SocketAddr) {
            let text = String::from_utf8_lossy(&self.kept);
            let msg = text.trim();
            if self.cut > 0 {
                warn!(%peer, ?msg, cut_bytes = self.cut, "ssh: sftp-server stderr");
            } else if !msg.is_empty() {
                warn!(%peer, ?msg, "ssh: sftp-server stderr");
            }
        }
    }

    /// FR-97 P4b's kill switch: `ROOMLERD_SFTP_AS_USER=0` puts back the
    /// refusal a Windows `console_user` session got before P4b.
    #[cfg(windows)]
    fn sftp_as_user_enabled() -> bool {
        tunnel_core::env::flag("SFTP_AS_USER", true)
    }

    /// #4168 — the exit status a refused request ends with: what sshd ends a
    /// session with when it cannot serve it, and what a command here that
    /// never ran already ends with ([`run_exec`]).
    const REFUSED_EXIT_STATUS: u32 = 1;

    impl Handler {
        /// Tell the client no, in a way a human reads on their terminal: log
        /// the refusal, then [`answer_refusal`](Self::answer_refusal).
        fn refuse(
            &mut self,
            session: &mut Session,
            channel: ChannelId,
            what: &str,
            why: &str,
        ) -> Result<(), russh::Error> {
            warn!(peer = %self.peer, request = ?what, "ssh: refused an unsupported request");
            self.answer_refusal(session, channel, why)
        }

        /// The answer to a request this channel will not serve, for a caller
        /// that has logged its own line.
        ///
        /// The request is answered with success, then `why` on stderr,
        /// [`REFUSED_EXIT_STATUS`], EOF and the close: how a program that
        /// could not start ends (#4168). A failure would be the plain answer,
        /// but OpenSSH's client takes a failed shell or subsystem request as
        /// fatal: it prints `shell request failed on channel 0` (`subsystem`
        /// under `sftp` and `scp`) and exits 255 without writing the stderr it
        /// was sent first, so the reason never reached the person.
        ///
        /// The success starts nothing. The close goes out right behind the
        /// reason, and russh drops whatever the client sends on a channel it
        /// has closed. Where russh holds the close back behind the reason,
        /// `refused` keeps the channel until it goes, and the handlers turn
        /// away what arrives on it.
        fn answer_refusal(
            &mut self,
            session: &mut Session,
            channel: ChannelId,
            why: &str,
        ) -> Result<(), russh::Error> {
            session.channel_success(channel)?;
            let _ =
                session.extended_data(channel, 1, format!("roomler-ssh: {why}\r\n").into_bytes());
            session.exit_status_request(channel, REFUSED_EXIT_STATUS)?;
            session.eof(channel)?;
            session.close(channel)?;
            if session.has_pending_data(channel) {
                self.refused.insert(channel);
            }
            Ok(())
        }
    }

    /// Ask the operator at this device, and say so on stderr first.
    ///
    /// The advisory line matters: without it a policy of `prompt` looks to the
    /// caller like a 30-second hang, which is the same "a bare failure cannot
    /// explain itself" problem that shapes every other refusal here.
    ///
    /// Who asks arrives with the handler ([`Asker`]): the broker through
    /// [`super::SessionServices`], or (FR-105) the worker's relay. Either way
    /// "nobody can be asked" is not a reachable state — a server that exists
    /// was handed one at construction. (It used to be a process global whose
    /// absence this function had to treat as a denial.)
    ///
    /// `what` names the activity being approved ("open an interactive shell",
    /// "run a command", "transfer files"). FR-27 puts it in front of the person
    /// deciding, not just in the daemon log: "grant SSH" and "give this account
    /// a root shell on my machine right now" are not the same question.
    ///
    /// `grant` is what the question is about, never the id it is asked under
    /// (#3442, [`question`]).
    async fn await_consent(
        asker: &Asker,
        handle: &russh::server::Handle,
        channel: ChannelId,
        caller: &str,
        grant: &str,
        what: &'static str,
    ) -> Result<(), String> {
        let timeout = crate::consent::DEFAULT_PROMPT_TIMEOUT;
        let (asked, joined) = question(asker, caller, grant, what, timeout);
        // A channel that joins a standing question waits for what is left of
        // that question's window, not for a window of its own.
        let note = if joined {
            format!(
                "roomler-ssh: this is being asked at the device already; waiting up to the rest \
                 of its {}s for that answer…\r\n",
                timeout.as_secs()
            )
        } else {
            format!(
                "roomler-ssh: waiting up to {}s for approval at the device…\r\n",
                timeout.as_secs()
            )
        };
        let _ = handle.extended_data(channel, 1, note.into_bytes()).await;
        asked.await
    }

    /// A question to the person at the device, as every channel waiting on it
    /// holds it.
    type Asked = futures::future::BoxFuture<'static, Result<(), String>>;

    /// What a question is about: its grant, and the activity it names.
    type Topic = (String, &'static str);

    /// A standing question, held weakly, with the turn it was asked on and
    /// the id it is asked under.
    type Standing = (u64, String, futures::future::WeakShared<Asked>);

    /// The questions standing, by what each is about.
    static ASKING: std::sync::Mutex<std::collections::BTreeMap<Topic, Standing>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());

    /// The question about `what` under `grant`: the one standing, if another
    /// channel of the session asks it already, or a new one under an id of
    /// its own.
    ///
    /// ⚠️ One question per grant AND activity (#3442). A grant is single-use,
    /// so every channel under it belongs to one connection, and they can ask
    /// at once: a command waits in a task of its own. Each used to ask under
    /// the grant's id, and the broker keeps one question per id, the newer one
    /// winning (#3434): the older question was withdrawn, and its channel
    /// refused although nobody had said no. A channel asking about the same
    /// activity now waits on the standing question, and one answer settles
    /// every channel waiting on it. A different activity is a different
    /// question, since the question names it (FR-27): an Allow given for a
    /// command must never open a shell.
    ///
    /// ⚠️ An id of its own, never the grant's: two questions standing under
    /// one grant must not share one. The broker would take the older off its
    /// books, and a surface dismissed by id (a desktop notification) would
    /// come down for both. The grant is logged beside it.
    ///
    /// The question stands while any channel waits on it: one channel closing
    /// does not withdraw it from the others. It comes off [`ASKING`] once it
    /// is answered, before the answer reaches any channel, or when the last
    /// channel waiting on it goes. A channel that asks in between the answer
    /// and that moment takes the answer: the same caller asked about the same
    /// activity, so the person could not have told the two channels apart.
    ///
    /// The second value says whether this channel joined a question that was
    /// standing already.
    fn question(
        asker: &Asker,
        caller: &str,
        grant: &str,
        what: &'static str,
        timeout: Duration,
    ) -> (futures::future::Shared<Asked>, bool) {
        use futures::FutureExt;
        static TURN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let key = (grant.to_string(), what);
        let mut asking = ASKING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((id, standing)) = asking
            .get(&key)
            .and_then(|(_, id, q)| q.upgrade().map(|q| (id.clone(), q)))
        {
            info!(?caller, %grant, question = %id, what, "ssh: this is asked already — waiting on that question");
            return (standing, true);
        }
        let turn = TURN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = bson::oid::ObjectId::new().to_hex();
        info!(?caller, %grant, question = %id, what, "ssh: asking the person at the device");
        let leave = Leave {
            key: key.clone(),
            turn,
        };
        let asked_as = id.clone();
        let (asker, caller, grant) = (asker.clone(), caller.to_string(), grant.to_string());
        let asked = async move {
            let verdict = ask(&asker, &caller, &grant, &id, what, timeout).await;
            // Off the table before the answer reaches anyone.
            drop(leave);
            verdict
        }
        .boxed()
        .shared();
        let weak = asked.downgrade().expect("a question not yet polled");
        asking.insert(key, (turn, asked_as, weak));
        (asked, false)
    }

    /// Takes its question off [`ASKING`]: once it is answered, or with the
    /// question when the last channel waiting on it goes. Only its own entry:
    /// a newer question about the same thing has a turn of its own.
    struct Leave {
        key: Topic,
        turn: u64,
    }

    impl Drop for Leave {
        fn drop(&mut self) {
            let mut asking = ASKING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if asking
                .get(&self.key)
                .is_some_and(|(turn, _, _)| *turn == self.turn)
            {
                asking.remove(&self.key);
            }
        }
    }

    /// Ask once, under `id`: here, or (FR-105) through the worker.
    async fn ask(
        asker: &Asker,
        caller: &str,
        grant: &str,
        id: &str,
        what: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        match asker {
            Asker::Here {
                broker,
                indicator,
                companion,
            } => {
                ask_here(
                    broker,
                    indicator,
                    *companion,
                    caller,
                    Some(grant),
                    id,
                    what,
                    timeout,
                )
                .await
            }
            Asker::Worker(relay) => {
                let verdict = relay.ask(id, caller, what, timeout).await;
                match &verdict {
                    Ok(()) => info!(
                        ?caller, %grant, question = %id,
                        "ssh-service: the person at the device approved the session"
                    ),
                    Err(why) => warn!(
                        ?caller, %grant, question = %id, %why,
                        "ssh-service: the person at the device did not approve the session"
                    ),
                }
                verdict
            }
        }
    }

    /// Ask the person at this device from THIS process: the native panel,
    /// the desktop companion, the CLI sentinel. `Err` is what the caller's
    /// terminal is told. FR-105 — also how a worker answers the question its
    /// SYSTEM service relays. `sentinel` is the question's own id (#3442),
    /// and `grant` what it is about, for the log: `None` where a worker asks
    /// for its service, whose own line names the grant.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn ask_here(
        broker: &crate::consent::ConsentBroker,
        indicator: &crate::indicator::ViewerIndicator,
        companion: crate::companion::Companion,
        caller: &str,
        grant: Option<&str>,
        sentinel: &str,
        what: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        // ⚠️ Armed before anything shows it (#3442): an answer counts from the
        // moment the question is up. The panel, the marker, the companion
        // (which may have to be started, up to 3 s) and the notification all
        // go up after this. Armed last, an answer given while they went up was
        // refused ("no active prompt"), and the native panel went down with
        // it, leaving the question asking with nothing on the screen. The
        // window still runs from the show: the prompt starts its clock at its
        // first look, below.
        let asking = broker.request_with_mode(sentinel, crate::consent::Mode::Prompt { timeout });

        // FR-27 — the same surface chain as a screen-control or exec prompt:
        // the daemon's own panel first, the desktop companion (started if it
        // is not running) second, the CLI sentinel always. SSH used to write
        // the companion marker only, which on a host with the companion closed
        // meant no screen at all — while the native panel on the same desktop
        // was answering exec prompts a minute earlier. `what` is the lead,
        // because "grant SSH" and "open an interactive shell as SYSTEM" are not
        // the same question.
        let view = crate::indicator::PromptView {
            session_hex: sentinel.to_string(),
            title: "SSH session request".into(),
            lead: format!("{caller} wants to {what} on this device."),
            detail: String::new(),
            permissions: String::new(),
            org: String::new(),
            expires_at: std::time::Instant::now() + timeout,
        };
        // FR-149 P1 — on a one-row Mac's root daemon the screen worker's
        // panel first, `PANEL_WITHIN` at most (`companion::COMPANION_START_BUDGET`'s
        // assertion counts it).
        let on_screen = indicator.show_prompt(view.clone()).await;
        let native = on_screen.native;
        let mut prompt = crate::consent::PendingPrompt {
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
        // FR-149 P1 — and read on the session panel's books after it.
        let marker = match crate::indicator::session_panel::write_marker(
            broker, sentinel, &prompt, &on_screen,
        ) {
            Ok(_) => true,
            Err(e) => {
                warn!(?caller, %sentinel, %e, native, "ssh: could not write the .pending consent marker");
                false
            }
        };
        let mut have_surface = native;
        let mut notified = false;
        // #4140 — why nothing showed it, if nothing does: the refusal's words.
        let mut why_not = crate::consent::NoSurface::NothingUp;
        if !native {
            // Awaited, not spawned, and bounded: its verdict is what lets the
            // refusal below say "nobody could be asked" instead of "nobody
            // answered" — different fixes, and the caller cannot tell them
            // apart afterwards. #4140 — and it keeps a Mac's closed road to
            // the tray apart from a tray that is not up.
            let tray = companion.verdict().await;
            let companion_up = tray.up();
            // FR-93 P6f — the last on-screen surface, as for a screen-control
            // request: a desktop notification where the companion cannot show
            // the question.
            if !(companion_up && marker) {
                notified = indicator.show_notification_prompt(view).await;
            }
            if notified && marker {
                prompt.surface = crate::consent::PromptSurface::Notification;
                if let Err(e) = broker.write_prompt(sentinel, &prompt) {
                    warn!(?caller, %sentinel, %e, "ssh: could not mark the consent prompt as shown by a notification");
                }
            }
            // A companion that is up and has the marker to read IS a surface:
            // this read `native` alone, so a question the companion showed
            // and nobody answered was reported as one nobody could be asked.
            have_surface = (companion_up && marker) || notified;
            // #4164 — a panel busy with another question wins over the app's
            // cause (`session_panel::Prompted::why_not_shown`).
            why_not = on_screen.why_not_shown(tray.why_not_shown(marker));
        }
        info!(
            ?caller, %sentinel, native, panel = on_screen.panel(),
            session_panel = on_screen.session_panel(), why = on_screen.why(),
            panel_busy = on_screen.panel_busy(), notified, have_surface,
            "consent prompt surface (ssh)"
        );

        let decision = asking.await;
        // The single convergence point: answered, timed out, or resolved from
        // the CLI — the panel comes down here regardless of who decided.
        indicator.hide_prompt(sentinel);
        // The grant beside the question's id: a channel that joined the
        // question logs that id, and is matched to this answer by it.
        let grant = grant.unwrap_or("-");
        if decision.granted() {
            info!(?caller, %grant, %sentinel, "ssh: operator approved the session");
            return Ok(());
        }
        warn!(?caller, %grant, %sentinel, ?decision, have_surface, "ssh: operator did not approve the session");
        Err(match decision {
            // #3982 — one text with an exec command that no surface showed
            // (`consent::nobody_could_be_asked`); a test there pins this
            // sentence. #4140 — for the cause found above.
            crate::consent::Decision::Timeout if !have_surface => {
                crate::consent::nobody_could_be_asked("SSH", why_not)
            }
            crate::consent::Decision::Timeout => format!(
                "nobody approved this session at the device within {}s",
                timeout.as_secs()
            ),
            // #1632 — nothing withdraws an SSH prompt today (the cancel path is
            // the RC `Terminate`), but "denied" would be the wrong story if
            // one ever did: nobody said no.
            crate::consent::Decision::Cancelled => {
                "the request was withdrawn before the operator at this device answered".to_string()
            }
            // `Granted` returned `Ok` above; only `Denied` reaches this arm.
            // Spelled out rather than `_` so a new variant fails to compile
            // instead of quietly reading as a refusal.
            crate::consent::Decision::Granted | crate::consent::Decision::Denied => {
                "the operator at this device denied this session".to_string()
            }
        })
    }

    /// Everything [`run_exec`] needs. Each field is a distinct decision —
    /// resolved at authentication (identity, consent, reporting) or in
    /// `exec_request` (the client's input, the abort, the device's streaming
    /// switch) — named so a reader can see which of them a path must check;
    /// nothing here is derived from anything the client said.
    struct ExecJob {
        handle: russh::server::Handle,
        channel: ChannelId,
        command: String,
        caller: String,
        /// #2815 — who the prompt names; see `ResolvedSessionPolicy::prompt_name`.
        prompt_name: String,
        run_as: crate::exec::RunAs,
        /// The consent sentinel to prompt on; `None` = no prompt.
        consent: Option<String>,
        /// Who asks the person (FR-105: this process, or the worker).
        asker: Asker,
        /// The P8 sink and the grant to attribute the report to.
        activity: Option<(ActivitySink, Option<String>)>,
        stdin: crate::exec::StdinFeed,
        /// FR-89 — fired when the channel closes or the session ends.
        abort: tokio_util::sync::CancellationToken,
        /// FR-89 — the device's `ssh_exec_streaming`.
        streaming: bool,
    }

    /// What a run came to, whichever path ran it.
    struct Finished {
        exit_code: Option<i32>,
        duration_ms: u64,
        /// Bytes that reached the channel, both streams.
        bytes: u64,
        error: Option<String>,
    }

    /// Chunks waiting between the engine's redaction stage and the channel.
    /// Small on purpose: the whole point of streaming is that a slow client
    /// stops the command rather than filling the daemon (FR-89 D1).
    const SINK_QUEUE: usize = 8;

    /// Run one command through the daemon's existing execution engine and
    /// stream the result back over the channel.
    ///
    /// Deliberately NOT a fresh `Command::spawn`: [`crate::exec`] already owns
    /// the identity model, the per-device concurrency cap, secret redaction,
    /// and process-tree kill. An SSH transport is not a reason to re-implement
    /// any of that.
    ///
    /// FR-89: the default path is the engine's **streaming** variant —
    /// output goes to the channel as it is produced, with no ceiling and no
    /// wall clock; what ends the command is the channel (`abort`). The
    /// buffered variant, with the engine's 1 MiB ceiling and 300 s limit,
    /// stays whole behind `ssh_exec_streaming = false`.
    ///
    /// `stdin` is the client's stdin, fed to the command in order and closed
    /// at the client's EOF. So a command that reads stdin waits for it, as
    /// under OpenSSH — and because the output streams, a command that
    /// PROMPTS now shows its prompt. `ssh -n` is still the answer for
    /// commands that must not read stdin.
    async fn run_exec(job: ExecJob) {
        use roomler_ai_remote_control::models::exec_limits;

        let ExecJob {
            handle,
            channel,
            command,
            caller,
            prompt_name,
            run_as,
            consent,
            asker,
            activity,
            stdin,
            abort,
            streaming,
        } = job;

        // Ask BEFORE anything runs, and refuse rather than fall through. The
        // prompt is deliberately here and not at grant-arrival time: the server
        // has already told the caller where to dial by then, so a refusal there
        // would surface as a connection that rejects them for no stated reason.
        if let Some(sentinel) = consent
            && let Err(why) = await_consent(
                &asker,
                &handle,
                channel,
                &prompt_name,
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

        // FR-89 — the channel closed while the operator was being asked. The
        // consent prompt ran its course (its own teardown is one place, and
        // stays so); what must not happen is the command starting now, as
        // SYSTEM/root, for a reader that left. The engine checks the token
        // again before it spawns; this check just spares the log a run that
        // was never going to deliver anything.
        if abort.is_cancelled() {
            info!(
                ?caller,
                "ssh: the channel closed before the command started — not running it"
            );
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
            // Read by the buffered path only; the streamed path has no wall
            // clock and no ceiling (FR-89 D4).
            timeout_ms: exec_limits::MAX_TIMEOUT_MS,
            max_output_bytes: exec_limits::MAX_OUTPUT_BYTES,
            cwd: None,
            caller: caller.clone(),
            run_as,
            // FR-97 P5b — in the person's home, where sshd runs a command.
            start_in_home: true,
            // #3303 — it ends with this session, however the daemon leaves.
            session_owned: true,
        };

        let finished = if streaming {
            stream_to_channel(&handle, channel, req, stdin, abort).await
        } else {
            buffer_to_channel(&handle, channel, req, stdin).await
        };

        // A run that never reached a process still has to say so on stderr:
        // an empty stream plus exit 0 would read as "the command succeeded and
        // printed nothing", which is the opposite of what happened.
        if let Some(err) = &finished.error {
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
        let status = match finished.exit_code {
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
            ?caller, run_as = ?identity, privileged,
            exit = status, duration_ms = finished.duration_ms,
            bytes = finished.bytes, streamed = streaming,
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
                finished.exit_code,
                true,
            );
        }
    }

    /// FR-89 — the streamed path: a pump task moves redacted chunks from the
    /// engine's sink onto the channel as they come, and the engine run is
    /// awaited beside it. The pump ends when the sink closes, which the
    /// engine does only after the last chunk, so by the time this returns
    /// everything the command wrote has been handed to russh — the exit
    /// status the caller sends next cannot overtake the output.
    ///
    /// Backpressure is the chain of bounded queues (`docs/roomler-ssh.md`
    /// §3): `handle.data` blocks while the client's window is spent, the sink
    /// fills, the engine's readers stop, the pipe fills, the child blocks.
    async fn stream_to_channel(
        handle: &russh::server::Handle,
        channel: ChannelId,
        req: crate::exec::ExecRequest,
        stdin: crate::exec::StdinFeed,
        abort: tokio_util::sync::CancellationToken,
    ) -> Finished {
        use crate::exec::{Chunk, OutStream};

        let (sink, mut chunks) = tokio::sync::mpsc::channel::<Chunk>(SINK_QUEUE);
        let pump_handle = handle.clone();
        let pump_abort = abort.clone();
        let pump = tokio::spawn(async move {
            let mut delivered = 0u64;
            while let Some(chunk) = chunks.recv().await {
                let n = chunk.bytes.len() as u64;
                let sent = match chunk.stream {
                    OutStream::Stdout => pump_handle.data(channel, chunk.bytes).await.is_ok(),
                    OutStream::Stderr => pump_handle
                        .extended_data(channel, 1, chunk.bytes)
                        .await
                        .is_ok(),
                };
                if !sent {
                    // The SESSION is gone (a closed channel still accepts and
                    // discards — `channel_close` covers that one). Nothing can
                    // be delivered any more, so end the command rather than
                    // let it run on for nobody.
                    pump_abort.cancel();
                    break;
                }
                delivered += n;
            }
            delivered
        });

        let outcome = crate::exec::shared()
            .run_streamed(req, &crate::exec::redactor(), Some(stdin), sink, abort)
            .await;
        let delivered = pump.await.unwrap_or(0);
        Finished {
            exit_code: outcome.exit_code,
            duration_ms: outcome.duration_ms,
            bytes: delivered,
            error: outcome.error,
        }
    }

    /// The pre-FR-89 path, kept whole behind `ssh_exec_streaming = false`:
    /// the engine's wall clock and ceiling, the output after the run, and a
    /// notice when the ceiling cut it.
    async fn buffer_to_channel(
        handle: &russh::server::Handle,
        channel: ChannelId,
        req: crate::exec::ExecRequest,
        stdin: crate::exec::StdinFeed,
    ) -> Finished {
        use roomler_ai_remote_control::models::exec_limits;

        let outcome = crate::exec::shared()
            .run_fed(req, &crate::exec::redactor(), Some(stdin))
            .await;

        // The BYTES, not the text view: an SSH channel is a byte stream, and
        // `ssh <node> cat f > f` must land the file as it is on the device.
        // Sending the decoded text turned every byte that is not UTF-8 into
        // U+FFFD — a 4.4 MB MP4 arrived as 1.9 MB of noise (2026-10-06).
        if !outcome.stdout_bytes.is_empty() {
            let _ = handle.data(channel, outcome.stdout_bytes.clone()).await;
        }
        if !outcome.stderr_bytes.is_empty() {
            let _ = handle
                .extended_data(channel, 1, outcome.stderr_bytes.clone())
                .await;
        }
        // The ceiling is the exec engine's (`docs/roomler-ssh.md` §3). Hitting
        // it must say so: the output simply stopping, with exit 1 and no word,
        // read as a corrupt transfer rather than a limit.
        if outcome.truncated {
            let _ = handle
                .extended_data(
                    channel,
                    1,
                    format!(
                        "roomler-ssh: output stopped at {} bytes, the exec ceiling (ssh_exec_streaming is off on this device). Copy files with scp or sftp, which stream them whole.\r\n",
                        exec_limits::MAX_OUTPUT_BYTES
                    )
                    .into_bytes(),
                )
                .await;
        }
        Finished {
            exit_code: outcome.exit_code,
            duration_ms: outcome.duration_ms,
            bytes: outcome.output_bytes(),
            error: outcome.error,
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
    let _live = LiveConnection::open();
    let handler = sshd::Handler::new(ctx.clone(), peer);
    match russh::server::run_stream(ctx.config.clone(), stream, handler).await {
        Ok(session) => match session.await {
            Ok(()) => info!(%peer, "ssh: session closed"),
            Err(e) => info!(%peer, %e, "ssh: session ended"),
        },
        Err(e) => warn!(%peer, %e, "ssh: handshake failed"),
    }
}

/// FR-105 — serve one SSH connection for the SYSTEM service, over `stream`
/// (the service's end of the SSH pipe, after the framing): the service's own
/// host key, the anchor's ceiling, and the person asked through `relay`, the
/// worker that carried the connection. Returns when the session ends.
///
/// Nothing is held awake and nothing is reported from here: the service has
/// no power keeper and no control socket (activity reports from the service
/// are a later phase).
#[cfg(feature = "ssh-server")]
pub async fn serve_for_service<S>(
    stream: S,
    peer: std::net::SocketAddr,
    host_key_pem: &str,
    ceiling: roomler_ai_remote_control::models::SshServiceCeiling,
    relay: crate::ssh_service::ConsentRelay,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let services = SessionServices {
        power_activity: None,
        asker: Asker::Worker(relay),
        activity: None,
    };
    let Some(ctx) = sshd::Ctx::for_service(host_key_pem, ceiling, services) else {
        return;
    };
    let handler = sshd::Handler::new(ctx.clone(), peer);
    match russh::server::run_stream(ctx.config.clone(), stream, handler).await {
        Ok(session) => match session.await {
            Ok(()) => tracing::info!(%peer, "ssh-service: session closed"),
            Err(e) => tracing::info!(%peer, %e, "ssh-service: session ended"),
        },
        Err(e) => warn!(%peer, %e, "ssh-service: handshake failed"),
    }
}

/// FR-96 P2b, #3429 — SSH connections open on this device right now. A
/// restart ends every one of them, so the restart onto a replaced binary and
/// the periodic update wait while any is open: the likeliest way a binary gets
/// replaced is an admin's `apt upgrade` typed INTO one of these sessions.
static LIVE_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many SSH connections are open right now; zero in a build without the
/// SSH server.
pub fn live_connections() -> usize {
    LIVE_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Counts one connection in [`LIVE_CONNECTIONS`] for as long as it lives,
/// whichever way `serve_conn`, or the carry to the SYSTEM service, returns.
#[cfg(all(feature = "overlay-netstack", feature = "ssh-server"))]
pub(crate) struct LiveConnection;

#[cfg(all(feature = "overlay-netstack", feature = "ssh-server"))]
impl LiveConnection {
    pub(crate) fn open() -> Self {
        LIVE_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

#[cfg(all(feature = "overlay-netstack", feature = "ssh-server"))]
impl Drop for LiveConnection {
    fn drop(&mut self) {
        LIVE_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
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
    /// LOCAL deadline after which this grant is dead: its arrival plus
    /// `ssh_limits::GRANT_TTL_SECS`, on this machine's monotonic clock.
    ///
    /// Never derived from the server's wall-clock `expires_at_ms` (#3511).
    /// The two machines' clocks can differ by minutes, so comparing the
    /// timestamp with this clock measured their skew, not the grant's age: a
    /// device a minute ahead of the server refused every grant, and one
    /// behind it took a late grant for the full window. So a skewed or
    /// malformed timestamp can neither shorten this nor stretch it, and no
    /// grant outlives the TTL from its arrival.
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

/// Why [`record_grant`] would not hold a grant.
///
/// A type rather than the string it used to be because the caller now
/// ANSWERS with it (FR-83): the variant crosses the wire in
/// `rc:ssh.grant_ack`, and the person who asked for the session is told which
/// one it was instead of meeting `Permission denied (publickey)`. The text a
/// log line prints is unchanged.
///
/// #3511 — there is no "expired on arrival" any more: a grant's window starts
/// when it arrives (`Grant::deadline`). The wire keeps
/// `SshGrantRefusal::Expired`, because agents released before the fix still
/// send it.
#[cfg(feature = "ssh-server")]
#[derive(Debug)]
pub enum GrantRejected {
    /// The key the server validated does not parse here.
    UnparseableKey(String),
    /// #3558 — a copy of a grant id this process still holds, or let go of
    /// inside [`GRANT_ID_HORIZON`]. It is not recorded, so the first copy's
    /// entry, window and acknowledgement stay exactly as they were.
    Duplicate(GrantSeen),
}

#[cfg(feature = "ssh-server")]
impl GrantRejected {
    /// The refusal as the server — and through it the caller — hears it.
    pub fn refusal(&self) -> roomler_ai_remote_control::models::SshGrantRefusal {
        use roomler_ai_remote_control::models::SshGrantRefusal;
        match self {
            // `Invalid` is "the device could not use what the server
            // validated", which the caller hears as `grant_refused`. A copy
            // is not `Other`, a reason this build would not know, and not
            // `Expired`, which #3511 retired. On the worker's own path the
            // copy's refusal reaches no waiter anyway: the first copy's
            // acknowledgement already removed it (`SshGrantAcks::deliver`).
            Self::UnparseableKey(_) | Self::Duplicate(_) => SshGrantRefusal::Invalid,
        }
    }
}

#[cfg(feature = "ssh-server")]
impl std::fmt::Display for GrantRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnparseableKey(e) => write!(f, "grant carries an unparseable public key: {e}"),
            Self::Duplicate(seen) => {
                write!(f, "a grant id is honoured at most once, and {seen}")
            }
        }
    }
}

/// Record an `rc:ssh.grant`.
///
/// On `Ok` the grant is in `GRANTS` — the table the auth path reads — so a
/// connection authenticating with its key succeeds from this moment. That is
/// what the caller's `rc:ssh.grant_ack` promises the server (FR-83), which is
/// why the ack is sent only after this returns.
///
/// `expires_at_ms` is the server's wall clock, and it is only logged: the
/// grant lives `ssh_limits::GRANT_TTL_SECS` from now, whatever it says
/// (#3511, `Grant::deadline`).
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
) -> Result<(), GrantRejected> {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use roomler_ai_remote_control::models::ssh_limits;

    // Parse now, not at authentication time: a malformed key should be a
    // start-up-shaped error in the log, not a mysterious auth failure later.
    russh::keys::ssh_key::PublicKey::from_openssh(&public_key)
        .map_err(|e| GrantRejected::UnparseableKey(e.to_string()))?;

    // #3511 — the window starts at ARRIVAL, on this machine's monotonic
    // clock, and is the TTL whatever the server's timestamp says. Lateness is
    // bounded on the server's clock instead: the caller is told where to dial
    // only once this function's ack is back inside
    // `GRANT_ACK_TIMEOUT_SECS` (FR-83), and a via-service grant can be
    // claimed only inside that bound (FR-105).
    let window = Duration::from_secs(ssh_limits::GRANT_TTL_SECS);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // What the timestamp leaves by THIS clock, for the log and never for a
    // decision; negative = already past here.
    let server_expiry_in_ms = (i128::from(expires_at_ms) - i128::from(now_ms))
        .clamp(i64::MIN.into(), i64::MAX.into()) as i64;
    if server_expiry_in_ms <= 0 {
        // What an agent before #3511 refused as `expired`. Still worth a
        // line: this clock is likely ahead of the server's.
        tracing::warn!(
            %grant_id, server_expiry_in_ms,
            "ssh: the server's expiry for this grant is already past by this device's clock \
             (is the clock ahead?); holding it the full window from arrival"
        );
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
    let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    prune(&mut grants, &mut spent, now);
    // #3558 — a grant id is honoured at most once. Checked before the drop
    // below, so a copy never evicts a live grant, and before the push, so it
    // never becomes a second redeemable entry with a window of its own.
    let seen = if grants.iter().any(|g| g.grant_id == grant.grant_id) {
        Some(GrantSeen::Pending)
    } else {
        spent.recall(&grant.grant_id, now)
    };
    if let Some(seen) = seen {
        return Err(GrantRejected::Duplicate(seen));
    }
    if grants.len() >= MAX_PENDING_GRANTS {
        // Drop the OLDEST rather than refusing the newest: the newest is the
        // one a caller is actively waiting on, and the oldest is within
        // seconds of expiring anyway.
        let dropped = grants.remove(0);
        tracing::warn!(
            grant_id = %dropped.grant_id,
            "ssh: pending-grant table full — dropped the oldest"
        );
        // Burned, as an expired one is: a late copy gets no window of its own.
        spent.remember(dropped.grant_id, GrantSeen::Dropped, now);
    }
    tracing::info!(
        grant_id = %grant.grant_id, caller = ?grant.caller,
        account_mode = %grant.account_mode, ttl_secs = window.as_secs(),
        server_expiry_in_ms,
        "ssh: grant recorded"
    );
    grants.push(grant);
    Ok(())
}

/// Drop everything past its deadline, and remember each dropped id (#3558):
/// a grant that timed out unredeemed is burned, as a redeemed one is.
#[cfg(feature = "ssh-server")]
fn prune(grants: &mut Vec<Grant>, spent: &mut RememberedIds<GrantSeen>, now: std::time::Instant) {
    grants.retain(|g| {
        let live = g.deadline > now;
        if !live {
            tracing::debug!(grant_id = %g.grant_id, "ssh: grant expired unredeemed");
            spent.remember(g.grant_id.clone(), GrantSeen::Expired, now);
        }
        live
    });
}

/// Consume the live grant admitting `key`, if any. Single use: a redeemed
/// grant is gone, so a captured public key cannot be replayed into a second
/// session — and its id is remembered (#3558), so a copy of the grant that
/// arrives later is not recorded as new.
#[cfg(feature = "ssh-server")]
fn take_grant_for(key: &russh::keys::ssh_key::PublicKey) -> Option<Grant> {
    let mut grants = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    prune(&mut grants, &mut spent, now);
    let idx = grants.iter().position(|g| {
        russh::keys::ssh_key::PublicKey::from_openssh(&g.public_key)
            .is_ok_and(|k| k.key_data() == key.key_data())
    })?;
    let grant = grants.remove(idx);
    spent.remember(grant.grant_id.clone(), GrantSeen::Redeemed, now);
    Some(grant)
}

/// How many grants are pending — surfaced in diagnostics.
#[cfg(feature = "ssh-server")]
pub fn pending_grants() -> usize {
    let mut grants = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut grants, &mut spent, std::time::Instant::now());
    grants.len()
}

// ── #3558: a grant id is honoured at most once ──────────────────────────────
//
// A grant delivered twice used to become two redeemable entries, and a copy
// that arrived after its grant was redeemed was recorded as new, with a fresh
// window of its own (#3511): a captured key could open a second session. Now
// a process refuses a copy whose id it still holds, or let go of within
// `GRANT_ID_HORIZON`, whichever way the first copy left: redeemed, expired
// unredeemed, or dropped from a full table.

/// How long a grant id is remembered after it left the table, so that a copy
/// of it is refused. While the grant is pending the table itself refuses a
/// copy, so an id is refused for its whole window and this long after: ten
/// windows (`ssh_limits::GRANT_TTL_SECS`, 60 s), at least one by construction.
#[cfg(feature = "ssh-server")]
const GRANT_ID_HORIZON: std::time::Duration = std::time::Duration::from_secs(
    10 * roomler_ai_remote_control::models::ssh_limits::GRANT_TTL_SECS,
);

/// Most grant ids one memory holds; when it is full the oldest is forgotten
/// first. An id is a few dozen bytes, so this is a few KiB, and the server
/// mints one grant per session asked for: 256 inside the horizon is a scripted
/// loop, not a person.
#[cfg(feature = "ssh-server")]
const MAX_REMEMBERED_GRANTS: usize = 256;

/// What became of the first copy of a grant id, when a second one arrives.
#[cfg(feature = "ssh-server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantSeen {
    /// It is in the table, waiting for its connection.
    Pending,
    /// A connection redeemed it.
    Redeemed,
    /// Its window ended with nobody redeeming it.
    Expired,
    /// A full table dropped it to make room for a newer grant.
    Dropped,
}

#[cfg(feature = "ssh-server")]
impl std::fmt::Display for GrantSeen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => "its first copy is still pending",
            Self::Redeemed => "its first copy was redeemed",
            Self::Expired => "its first copy expired unredeemed",
            Self::Dropped => "its first copy was dropped from a full table",
        })
    }
}

/// Grant ids a process is done with, each until its time to be forgotten,
/// oldest first. Every entry is kept for the same horizon, so the order of
/// arrival is the order of forgetting, by age or by the bound alike.
#[cfg(feature = "ssh-server")]
#[derive(Debug)]
struct RememberedIds<T> {
    entries: std::collections::VecDeque<(String, T, std::time::Instant)>,
}

#[cfg(feature = "ssh-server")]
impl<T> RememberedIds<T> {
    const fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::new(),
        }
    }
}

#[cfg(feature = "ssh-server")]
impl<T: Copy> RememberedIds<T> {
    /// What is remembered about `grant_id`, if it is still remembered.
    fn recall(&mut self, grant_id: &str, now: std::time::Instant) -> Option<T> {
        self.forget_past(now);
        self.entries
            .iter()
            .find(|(id, _, _)| id == grant_id)
            .map(|(_, what, _)| *what)
    }

    /// Remember `grant_id` for [`GRANT_ID_HORIZON`] from `now`. A full
    /// memory forgets its oldest id first.
    fn remember(&mut self, grant_id: String, what: T, now: std::time::Instant) {
        self.forget_past(now);
        if self.entries.len() >= MAX_REMEMBERED_GRANTS
            && let Some((oldest, _, _)) = self.entries.pop_front()
        {
            tracing::debug!(grant_id = %oldest, "ssh: grant-id memory full — forgot the oldest");
        }
        self.entries
            .push_back((grant_id, what, now + GRANT_ID_HORIZON));
    }

    fn forget_past(&mut self, now: std::time::Instant) {
        self.entries.retain(|(_, _, forget_at)| *forget_at > now);
    }
}

/// The ids that left [`GRANTS`]: redeemed, expired unredeemed, or dropped.
///
/// Locked only while `GRANTS` is held, and always after it: one lock order,
/// and no moment at which a grant has left the table and its id is not yet
/// remembered.
#[cfg(feature = "ssh-server")]
static SPENT: std::sync::Mutex<RememberedIds<GrantSeen>> =
    std::sync::Mutex::new(RememberedIds::new());

/// The worker's memory of the grants it was asked to hand to this device's
/// SYSTEM service (FR-105). Never [`GRANTS`] or [`SPENT`]: those back the
/// table this process's own server reads, and a via-service grant is never
/// redeemable here.
#[cfg(feature = "ssh-server")]
static SEEN_VIA_SERVICE: std::sync::Mutex<RememberedIds<()>> =
    std::sync::Mutex::new(RememberedIds::new());

/// #3558 — whether this is the first copy of a via-service grant this worker
/// has seen. On `false` the caller logs the copy and drops it: no second ring,
/// no acknowledgement.
///
/// A copy answers nothing on this path, where the worker's own path refuses
/// one ([`GrantRejected::Duplicate`]), because here a refusal could undo the
/// first copy's outcome. The service's claim is single use on the server, so
/// a copy's ring ends in a refused claim, and the worker would acknowledge
/// that as a refusal. With two server instances, the first copy's claim can
/// land on the one that does not hold the caller's wait: that wait then stays
/// open and still takes a worker's refusal, and the caller is denied while
/// the service holds the grant.
///
/// Remembered at first sight, before any outcome: a grant the worker refused
/// is as dead as one the service took, and the first refusal already
/// answered.
#[cfg(feature = "ssh-server")]
pub(crate) fn first_sight_via_service(grant_id: &str) -> bool {
    let mut seen = SEEN_VIA_SERVICE.lock().unwrap_or_else(|e| e.into_inner());
    first_sight(&mut seen, grant_id, std::time::Instant::now())
}

#[cfg(feature = "ssh-server")]
fn first_sight(seen: &mut RememberedIds<()>, grant_id: &str, now: std::time::Instant) -> bool {
    if seen.recall(grant_id, now).is_some() {
        return false;
    }
    seen.remember(grant_id.to_owned(), (), now);
    true
}

/// Test-only: empty the table, and the ids it let go of, so cases cannot
/// leak into each other through process-global state.
#[cfg(all(test, feature = "ssh-server"))]
fn clear_grants() {
    GRANTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    SPENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .clear();
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

    #[test]
    fn an_unreadable_stored_key_counts_as_no_key_so_the_daemon_can_heal() {
        // The bug this locks: `main.rs` minted only when the key was ABSENT, so
        // a key that was present-but-garbage wedged SSH on that device forever —
        // refusing to serve, publishing nothing, and never replacing it. The
        // device was one restart away from being fine and could not take it.
        //
        // Field case, 0.4.96 on a corp laptop: the stored PEM had picked up
        // literal `\r` escape text, so the BEGIN line was not newline-terminated
        // and the parse failed with `PEM error in pre-encapsulation boundary`.
        // That exact shape is the first case below, because a test written
        // around a tidily-invalid key would not have caught it.
        let mut cfg = cfg_with(vec![]);

        cfg.ssh_host_key = Some(
            "-----BEGIN OPENSSH PRIVATE KEY-----\\r\\r\\\nAAAA\n\
             -----END OPENSSH PRIVATE KEY-----\n"
                .into(),
        );
        assert!(
            !super::stored_host_key_is_usable(&cfg),
            "a PEM whose boundary line is mangled must read as unusable, not as present"
        );

        cfg.ssh_host_key = None;
        assert!(!super::stored_host_key_is_usable(&cfg));

        cfg.ssh_host_key = Some(String::new());
        assert!(!super::stored_host_key_is_usable(&cfg));

        // …and the positive control, without which the three above would pass
        // on a function that simply always returned false.
        cfg.ssh_host_key = Some(super::generate_host_key().unwrap());
        assert!(super::stored_host_key_is_usable(&cfg));
        assert_ne!(super::host_public_key(&cfg), "");
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
        // No companion: a test must never start the real app.
        test_services_with(crate::companion::Companion::Fixed(false))
    }

    /// [`test_services`] with the companion a test needs: one that is still
    /// starting while the question is up, for one (#3442).
    fn test_services_with(companion: crate::companion::Companion) -> super::SessionServices {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("roomler-ssh-consent-{nanos}"));
        super::SessionServices {
            // FR-55 — tests hold nothing awake. `None` is the real production
            // value anywhere no power keeper exists, not a stub.
            power_activity: None,
            asker: super::Asker::Here {
                broker: crate::consent::ConsentBroker::new(crate::consent::Mode::AutoGrant, dir)
                    .expect("a temp sentinel dir"),
                indicator: crate::indicator::ViewerIndicator::disabled(),
                companion,
            },
            // Reporting off in tests unless a case opts in — the default a
            // device ships with.
            activity: None,
        }
    }

    /// The broker a test's services ask through, so the test can play the
    /// operator.
    fn broker_of(services: &super::SessionServices) -> crate::consent::ConsentBroker {
        match &services.asker {
            super::Asker::Here { broker, .. } => broker.clone(),
            super::Asker::Worker(_) => panic!("a worker's server asks here"),
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
        serve_one_asking(cfg, crate::companion::Companion::Fixed(false)).await
    }

    /// [`serve_one_with`], asking through `companion`.
    async fn serve_one_asking(
        cfg: &crate::config::AgentConfig,
        companion: crate::companion::Companion,
    ) -> (std::net::SocketAddr, crate::consent::ConsentBroker) {
        let services = test_services_with(companion);
        let broker = broker_of(&services);
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
        connect_with(addr, key, russh::client::Config::default()).await
    }

    /// [`connect`], as a client configured with `config`.
    async fn connect_with(
        addr: std::net::SocketAddr,
        key: PrivateKey,
        config: russh::client::Config,
    ) -> russh::client::Handle<TestClient> {
        let config = Arc::new(config);
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

    /// An empty list is the default, and with no grant to redeem it must deny.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_authorized_list_denies_a_key_with_no_grant() {
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
            super::sshd::parse_account_mode("named:casey").unwrap(),
            RunAs::Named("casey".into())
        );
        // Whitespace is tolerated; an empty account and an unknown word are not.
        assert_eq!(
            super::sshd::parse_account_mode(" named: casey ").unwrap(),
            RunAs::Named("casey".into())
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
        let _slot = exec_slot().await;
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

    /// The client's stdin reaches an exec'd command, and the client's EOF
    /// ends it: `ssh node 'cat > f' < file`, `tar c . | ssh node 'tar x'`.
    /// Before, `data()` had nowhere to put the bytes and dropped them. The
    /// command read NUL and succeeded with nothing, so the client saw exit 0
    /// and an empty file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_clients_stdin_reaches_an_execd_command() {
        let _slot = exec_slot().await;
        let (key, line) = client_key(44);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        let session = connect(addr, key).await;

        let mut channel = session.channel_open_session().await.unwrap();
        let echo_stdin = if cfg!(windows) {
            "[Console]::In.ReadToEnd()"
        } else {
            "cat"
        };
        channel.exec(true, echo_stdin).await.unwrap();
        channel.data(&b"piped-into-"[..]).await.unwrap();
        channel.data(&b"roomler-ssh"[..]).await.unwrap();
        channel.eof().await.unwrap();

        let mut stdout = Vec::new();
        let mut exit = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => stdout.extend_from_slice(data),
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }

        let stdout = String::from_utf8_lossy(&stdout);
        assert!(
            stdout.contains("piped-into-roomler-ssh"),
            "the client's stdin never reached the command: stdout {stdout:?}"
        );
        assert_eq!(exit, Some(0));
    }

    /// A command that emits `bytes` bytes of output in ONE fast operation —
    /// far past the exec engine's 1 MiB ceiling. One operation rather than a
    /// shell loop on purpose: these run against the process-wide
    /// `exec::shared()` engine, whose 4-permit cap the whole SSH suite
    /// competes for, and a slow command holds its permit long enough to
    /// starve the others. A single write also arrives as one long
    /// whitespace-free run, which exercises the redactor's force-settle path.
    fn big_output(bytes: usize) -> String {
        if cfg!(windows) {
            format!("[Console]::Out.Write('x' * {bytes})")
        } else {
            format!("head -c {bytes} /dev/zero | tr '\\0' x")
        }
    }

    /// Drive a command over a real SSH connection and return
    /// `(stdout_len, stderr_text, exit)`.
    async fn run_over_ssh(
        cfg: &crate::config::AgentConfig,
        key: PrivateKey,
        cmd: &str,
    ) -> (usize, String, Option<u32>) {
        let _slot = exec_slot().await;
        let addr = serve_one(cfg).await;
        let session = connect(addr, key).await;
        let mut channel = session.channel_open_session().await.unwrap();
        channel.exec(true, cmd).await.unwrap();
        let mut stdout_len = 0usize;
        let mut stderr = Vec::new();
        let mut exit = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => stdout_len += data.len(),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    stderr.extend_from_slice(data)
                }
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }
        (
            stdout_len,
            String::from_utf8_lossy(&stderr).into_owned(),
            exit,
        )
    }

    /// AC1 — a command whose output is far larger than the engine's 1 MiB
    /// ceiling arrives WHOLE, with exit 0 and no ceiling notice: the streamed
    /// path (the default) has no ceiling. This is the file `ssh <node> cat
    /// bigfile > f` could not move before FR-89.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_large_command_output_streams_whole() {
        let (key, line) = client_key(60);
        let (stdout_len, stderr, exit) = run_over_ssh(
            &cfg_with_mode(vec![line], Some("daemon")),
            key,
            &big_output(1_500_000),
        )
        .await;
        assert_eq!(exit, Some(0), "stderr: {stderr}");
        assert!(
            stdout_len > 1024 * 1024,
            "streamed output was capped at {stdout_len} bytes — the ceiling was not lifted"
        );
        assert!(
            !stderr.contains("ceiling"),
            "a streamed run must not announce a ceiling it does not have: {stderr:?}"
        );
    }

    /// AC9 — the kill switch restores the pre-FR-89 behaviour exactly: the
    /// same large command, with `ssh_exec_streaming = false`, is capped at
    /// 1 MiB and says so on stderr.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_kill_switch_restores_the_buffered_ceiling() {
        let (key, line) = client_key(61);
        let mut cfg = cfg_with_mode(vec![line], Some("daemon"));
        cfg.ssh_exec_streaming = false;
        let (stdout_len, stderr, _exit) = run_over_ssh(&cfg, key, &big_output(1_500_000)).await;
        assert!(
            stdout_len <= 1024 * 1024,
            "the buffered path must cap at 1 MiB, got {stdout_len} bytes"
        );
        assert!(
            stderr.contains("exec ceiling"),
            "hitting the ceiling must be announced on stderr: {stderr:?}"
        );
    }

    /// #4168 — one message a client reads on a channel.
    #[derive(Debug, PartialEq)]
    enum Read {
        Success,
        Failure,
        Stdout(String),
        /// Stderr messages that came one after another, run together.
        Stderr(String),
        Exit(u32),
        Eof,
        Close,
    }

    /// The next message `channel` reads, passing over those a refusal never
    /// sends (a window adjustment, say); `None` once the channel has ended.
    async fn next_read(channel: &mut russh::Channel<russh::client::Msg>) -> Option<Read> {
        loop {
            let read = match channel.wait().await? {
                russh::ChannelMsg::Success => Read::Success,
                russh::ChannelMsg::Failure => Read::Failure,
                russh::ChannelMsg::Data { ref data } => {
                    Read::Stdout(String::from_utf8_lossy(data).into_owned())
                }
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    Read::Stderr(String::from_utf8_lossy(data).into_owned())
                }
                russh::ChannelMsg::ExitStatus { exit_status } => Read::Exit(exit_status),
                russh::ChannelMsg::Eof => Read::Eof,
                russh::ChannelMsg::Close => Read::Close,
                _ => continue,
            };
            return Some(read);
        }
    }

    /// [`next_read`], within 10 s; `what` names the read for a timeout.
    async fn next_read_soon(
        channel: &mut russh::Channel<russh::client::Msg>,
        what: &str,
    ) -> Option<Read> {
        tokio::time::timeout(std::time::Duration::from_secs(10), next_read(channel))
            .await
            .unwrap_or_else(|_| panic!("{what}: the channel said nothing within 10 s"))
    }

    /// What a client reads on `channel`, in order, until the channel ends or
    /// answers with a failure: a channel that answered a failure stays open,
    /// as one a refusal answered did before #4168. Within 30 s.
    async fn read_in_order(mut channel: russh::Channel<russh::client::Msg>) -> Vec<Read> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async move {
            let mut reads: Vec<Read> = Vec::new();
            while let Some(read) = next_read(&mut channel).await {
                let failed = read == Read::Failure;
                let merged = match (&read, reads.last_mut()) {
                    (Read::Stderr(more), Some(Read::Stderr(said))) => {
                        said.push_str(more);
                        true
                    }
                    _ => false,
                };
                if !merged {
                    reads.push(read);
                }
                if failed {
                    break;
                }
            }
            reads
        })
        .await
        .expect("the channel ends, or answers with a failure, within 30 s")
    }

    /// #4168 — `reads` is a refusal as OpenSSH's client can show it: success,
    /// the reason on stderr (holding `says`), exit-status 1, EOF and close,
    /// in that order. A failure instead is fatal to `ssh`, which then exits
    /// 255 without writing the reason it was sent.
    fn assert_refused_in_order(reads: &[Read], says: &str) {
        match reads {
            [
                Read::Success,
                Read::Stderr(said),
                Read::Exit(1),
                Read::Eof,
                Read::Close,
            ] if said.starts_with("roomler-ssh: ") && said.contains(says) => {}
            _ => panic!(
                "a refusal is read as success, the reason on stderr (naming {says:?}), \
                 exit-status 1, EOF and close, in that order; the client read {reads:?}"
            ),
        }
    }

    /// The line a refusal logs holds the peer and the request it refused,
    /// and the server wrote no `started` line (one is written once a
    /// process is up).
    fn assert_refused_and_nothing_started(log: &str, request: &str, started: &str) {
        let refused = log
            .lines()
            .find(|l| l.contains("ssh: refused an unsupported request"))
            .unwrap_or_else(|| panic!("no refusal line in:\n{log}"));
        let request = format!("request={request:?}");
        for field in ["peer=", request.as_str()] {
            assert!(refused.contains(field), "{field} on {refused:?}");
        }
        assert!(
            !log.contains(started),
            "a refused request started something ({started:?}):\n{log}"
        );
    }

    /// A refused request must say why. `scp` turning into a silent hang is the
    /// specific failure this guards against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unsupported_requests_are_refused_with_a_reason() {
        let (key, line) = client_key(5);
        let addr = serve_one(&cfg_with(vec![line])).await;
        let session = connect(addr, key).await;

        let channel = session.channel_open_session().await.unwrap();
        // `request_subsystem` only reports that the request was *sent*; the
        // verdict comes back on the channel, so read it rather than trusting
        // the send to mean acceptance.
        // `sftp` is served since P7, so the unsupported case needs a
        // subsystem we genuinely do not implement. The point of the test is
        // unchanged: a refusal must arrive WITH a reason, and (#4168) in the
        // order a client can show it in.
        channel.request_subsystem(true, "netconf").await.unwrap();
        assert_refused_in_order(&read_in_order(channel).await, "subsystem");
    }

    /// SFTP inherits the session's identity, so a session that resolved to NO
    /// account must not transfer files either.
    ///
    /// This is the same rule P5c set for commands — a key-list session with
    /// `ssh_account_mode` unset authenticates and then runs nothing rather
    /// than quietly taking SYSTEM/root. File transfer is the subsystem where
    /// getting that wrong would be worst, so it is asserted separately
    /// instead of assumed to follow.
    ///
    /// #4168 — the refusal reads as success, the reason, exit-status 1, EOF
    /// and close, so `sftp` and `scp` show the reason before they stop, and
    /// no `sftp-server` starts. One thread, so the log is the server's.
    #[tokio::test]
    async fn sftp_is_refused_when_the_session_resolved_to_no_account() {
        let log = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
        let (key, line) = client_key(9);
        // `cfg_with` leaves `ssh_account_mode` unset — the default.
        let addr = serve_one(&cfg_with(vec![line])).await;
        let session = connect(addr, key).await;

        let channel = session.channel_open_session().await.unwrap();
        channel.request_subsystem(true, "sftp").await.unwrap();
        assert_refused_in_order(&read_in_order(channel).await, "ssh_account_mode");
        assert_refused_and_nothing_started(&log.text(), "sftp", "ssh: sftp session started");
    }

    /// #4168 — a refused channel takes nothing more while its close waits
    /// behind the reason. A client window with no room for the reason holds
    /// russh's EOF and close back, and russh still delivers what the client
    /// sends on that channel meanwhile. A command, a terminal and a second
    /// shell asked for there are each answered with a failure, and none
    /// starts; the data and EOF sent there reach no process.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_channel_takes_nothing_more_while_its_close_waits() {
        let _slot = exec_slot().await;
        let (key, line) = client_key(160);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        // No window: the server can send no data on any channel, so the
        // reason waits, and the EOF and close wait behind it.
        let no_window = russh::client::Config {
            window_size: 0,
            ..Default::default()
        };
        let session = connect_with(addr, key, no_window).await;
        let marks = tempfile::tempdir().expect("a temp dir");
        let ran = marks.path().join("ran");
        let kept = marks.path().join("kept");
        let done = marks.path().join("done");

        let mut refused = session.channel_open_session().await.unwrap();
        refused.request_shell(true).await.unwrap();
        // Neither is data, so both arrive.
        let answer = next_read_soon(&mut refused, "the shell's answer").await;
        assert_eq!(answer, Some(Read::Success));
        let status = next_read_soon(&mut refused, "the shell's exit status").await;
        assert_eq!(status, Some(Read::Exit(1)));

        let touch = if cfg!(windows) {
            format!("Set-Content -LiteralPath '{}' -Value ran", ran.display())
        } else {
            format!(": > '{}'", ran.display())
        };
        refused.exec(true, touch.as_str()).await.unwrap();
        assert_eq!(
            next_read_soon(&mut refused, "a command's answer").await,
            Some(Read::Failure),
            "a command on the refused channel"
        );
        refused
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        assert_eq!(
            next_read_soon(&mut refused, "a terminal's answer").await,
            Some(Read::Failure),
            "a terminal on the refused channel"
        );
        refused.request_shell(true).await.unwrap();
        assert_eq!(
            next_read_soon(&mut refused, "a second shell's answer").await,
            Some(Read::Failure),
            "a second shell on the refused channel"
        );

        // A command on a channel of its own keeps what it reads on its stdin,
        // then leaves a mark. The mark, not its exit status: while a
        // channel's data waits on the window, russh sends nothing that a
        // session's task hands it, on any channel.
        let keep = if cfg!(windows) {
            format!(
                "[Console]::In.ReadToEnd() | Set-Content -LiteralPath '{}' -NoNewline; \
                 Set-Content -LiteralPath '{}' -Value done",
                kept.display(),
                done.display()
            )
        } else {
            format!("cat > '{}'; : > '{}'", kept.display(), done.display())
        };
        let mut command = session.channel_open_session().await.unwrap();
        command.exec(true, keep.as_str()).await.unwrap();
        assert_eq!(
            next_read_soon(&mut command, "the other channel's command").await,
            Some(Read::Success)
        );
        refused
            .data(&b"from-the-refused-channel"[..])
            .await
            .unwrap();
        command.data(&b"from-its-own"[..]).await.unwrap();
        refused.eof().await.unwrap();
        command.data(&b"-channel"[..]).await.unwrap();
        command.eof().await.unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !done.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the command on its own channel did not finish within 30 s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let kept = std::fs::read_to_string(&kept).expect("the command wrote what it read");
        assert_eq!(
            kept.trim_start_matches('\u{feff}').trim(),
            "from-its-own-channel",
            "the refused channel's data or EOF reached a process"
        );
        assert!(!ran.exists(), "a command on the refused channel ran");
    }

    /// #3822 — a `console_user` session this device cannot serve says why on
    /// the SSH client's stderr, for a command, a shell and SFTP alike, and
    /// runs nothing. Over a real SSH connection, against this host's own
    /// seat0 lookup (`exec::console_account`): nobody is at a CI runner's
    /// screen, so each is refused. When the lookup gives no answer in time
    /// the same refusal carries the try-again text instead
    /// (`exec::console_lookup_tests`). A desktop running the tests as the
    /// person at its screen can serve the session, so there this test proves
    /// nothing and says so.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_console_user_refusal_reaches_the_ssh_clients_stderr() {
        let _slot = exec_slot().await;
        let (key, line) = client_key(71);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("console_user"))).await;
        let session = connect(addr, key).await;
        let marks = tempfile::tempdir().expect("a temp dir");
        let mark = marks.path().join("ran");

        // A command.
        let command = session.channel_open_session().await.unwrap();
        command
            .exec(true, format!(": > '{}'", mark.display()).as_str())
            .await
            .unwrap();
        let (_, err, exit) = read_channel(command, None).await;
        if exit == Some(0) {
            eprintln!("skipped: someone is at this machine's screen, so console_user can run");
            return;
        }
        assert!(
            err.contains("roomler-ssh: account_mode `console_user`"),
            "a command's refusal on stderr: {err:?}"
        );
        assert_eq!(exit, Some(1), "a command: {err:?}");
        assert!(!mark.exists(), "the refused command ran");

        // A shell: asked to leave at once, in case it starts.
        let shell = session.channel_open_session().await.unwrap();
        shell
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        shell.request_shell(true).await.unwrap();
        let _ = shell.data(&b"exit\n"[..]).await;
        let (_, err, exit) = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            read_channel(shell, None),
        )
        .await
        .expect("the shell's channel ends");
        assert!(
            err.contains("roomler-ssh: account_mode `console_user`"),
            "a shell's refusal on stderr: {err:?}"
        );
        assert_eq!(exit, Some(1), "a shell: {err:?}");

        // SFTP: its account is reached only where there is an `sftp-server`
        // to start; without one, it is refused for that, before.
        let mut sftp = session.channel_open_session().await.unwrap();
        sftp.request_subsystem(true, "sftp").await.unwrap();
        let mut err = Vec::new();
        while let Some(msg) = sftp.wait().await {
            match msg {
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => err.extend_from_slice(data),
                russh::ChannelMsg::Failure | russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        let err = String::from_utf8_lossy(&err);
        if err.contains("no `sftp-server` binary") {
            eprintln!("SFTP's account is not reached here: this host has no sftp-server");
            return;
        }
        assert!(
            err.contains("roomler: account_mode `console_user`"),
            "SFTP's refusal on stderr: {err:?}"
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

    /// The SSH server runs commands through the PROCESS-WIDE exec engine
    /// (`crate::exec::shared()`), whose per-device cap —
    /// `exec_limits::MAX_CONCURRENT_PER_AGENT`, 4 — is a product invariant,
    /// not a test knob. `cargo test` runs this whole suite in one process on
    /// many threads, so every test that actually runs a command holds one of
    /// exactly that many slots for its duration: the suite can then never
    /// have more commands in flight than the engine admits. Without this a
    /// fifth overlapping test is refused with "device is already running 4
    /// commands" and reads as a product bug — measured 2026-10-07 as two
    /// grant tests, then the kill-switch test, each failing on an empty
    /// stdout while passing serialized.
    static EXEC_TEST_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(
        roomler_ai_remote_control::models::exec_limits::MAX_CONCURRENT_PER_AGENT,
    );

    /// Hold a slot for the life of a test that runs a command. Taken AFTER
    /// [`grant_test`] wherever both apply, so there is exactly one lock order.
    async fn exec_slot() -> tokio::sync::SemaphorePermit<'static> {
        EXEC_TEST_SLOTS
            .acquire()
            .await
            .expect("the slot semaphore is never closed")
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

    /// Returns the grant id on success. A question asked under the grant has
    /// an id of its own (#3442): the operator-approval tests answer it under
    /// the id [`questions_up`] reads off the broker, as every surface does.
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
        .map_err(|e| e.to_string())
    }

    /// A grant admits its key, and admits it exactly once. Without single-use,
    /// a captured public key would be a standing credential for the whole
    /// grant lifetime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_grant_admits_its_key_exactly_once() {
        let _lock = grant_test().await;
        let _slot = exec_slot().await;
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

    /// An expired grant is not a grant. The deadline is LOCAL, arrival plus
    /// the TTL (#3511), so this puts it in the past rather than waiting the
    /// TTL out, and nothing here depends on the two clocks agreeing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_expired_grant_admits_nobody() {
        let _lock = grant_test().await;
        let (key, line) = client_key(11);
        let addr = serve_one(&cfg_with(Vec::new())).await;

        grant_for(&line, 30_000).unwrap();
        for grant in super::GRANTS.lock().unwrap().iter_mut() {
            grant.deadline = std::time::Instant::now();
        }

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
        assert!(!res.success(), "a grant past its deadline admits nobody");
        assert_eq!(super::pending_grants(), 0, "and it is gone from the table");

        // FR-83 — a refusal reaches the caller and says WHICH. An unusable
        // key means "report a bug", and it is refused before anything is
        // stored.
        use roomler_ai_remote_control::models::SshGrantRefusal;
        let garbage = super::record_grant(
            "g-garbage".into(),
            "not a key".into(),
            "alice@example.com".into(),
            "daemon".into(),
            None,
            None,
            now_ms() + 30_000,
            0,
        )
        .unwrap_err();
        assert_eq!(garbage.refusal(), SshGrantRefusal::Invalid);
        assert_eq!(super::pending_grants(), 0);
    }

    /// #3511 — the window is the TTL from ARRIVAL, whatever the two clocks
    /// say. Each case is a grant minted now by a server whose clock differs
    /// from this one by the skew (positive = this device AHEAD), with the
    /// timestamp both kinds of server write: mint + TTL (before #3511) and
    /// mint + TTL + the device-leg slack. Before the fix +73 s (the field
    /// case) and +120 s were refused as expired, +30 s got half a window, and
    /// a timestamp of 0 was refused.
    #[tokio::test]
    async fn the_window_is_the_ttl_from_arrival_whatever_the_clocks_say() {
        use roomler_ai_remote_control::models::ssh_limits;
        use std::time::{Duration, Instant};

        let _lock = grant_test().await;
        let ttl = Duration::from_secs(ssh_limits::GRANT_TTL_SECS);
        let ttl_ms = ssh_limits::GRANT_TTL_SECS * 1000;
        let slack_ms = ssh_limits::DEVICE_EXPIRY_SLACK_SECS * 1000;
        let mut cases: Vec<(String, u64)> = Vec::new();
        for skew_secs in [-120i64, -30, 0, 30, 73, 120] {
            let server_now_ms = now_ms().checked_add_signed(-skew_secs * 1000).unwrap();
            cases.push((
                format!("{skew_secs:+} s, old server"),
                server_now_ms + ttl_ms,
            ));
            cases.push((
                format!("{skew_secs:+} s, slacked server"),
                server_now_ms + ttl_ms + slack_ms,
            ));
        }
        cases.push(("a zero timestamp".into(), 0));
        cases.push(("a huge timestamp".into(), u64::MAX));
        cases.push(("a year ahead".into(), now_ms() + 365 * 24 * 3600 * 1000));

        for (i, (case, expires_at_ms)) in cases.iter().enumerate() {
            super::clear_grants();
            let (_key, line) = client_key(200u8.wrapping_add(i as u8));
            super::record_grant(
                format!("g-skew-{i}"),
                line,
                "alice@example.com".into(),
                "daemon".into(),
                None,
                None,
                *expires_at_ms,
                0,
            )
            .unwrap_or_else(|e| panic!("{case}: refused: {e}"));
            let left = super::GRANTS.lock().unwrap()[0]
                .deadline
                .saturating_duration_since(Instant::now());
            assert!(
                left <= ttl,
                "{case}: no grant outlives the TTL from its arrival; got {left:?}"
            );
            assert!(
                left > ttl - Duration::from_secs(5),
                "{case}: the whole window, not what the clocks leave of it; got {left:?}"
            );
        }
    }

    /// The server's timestamp can never LENGTHEN the local window. A grant
    /// claiming a year of validity — a skewed clock, or a compromised control
    /// plane — still dies at the local ceiling.
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

    // ── #3558: a grant id is honoured at most once ───────────────────────

    /// Record a grant under a FIXED id, as a duplicated or replayed
    /// `rc:ssh.grant` carries it. [`grant_for`] draws a fresh id on every
    /// call, which is exactly what these cases must not do.
    fn record_copy(grant_id: &str, line: &str) -> Result<(), super::GrantRejected> {
        super::record_grant(
            grant_id.to_string(),
            line.to_string(),
            "alice@example.com".into(),
            "daemon".into(),
            None,
            Some(roomler_ai_remote_control::models::ConsentMode::Auto),
            now_ms() + 30_000,
            0,
        )
    }

    /// Whether a connection authenticating with `key` gets in. Unlike
    /// [`connect`], a refusal is an answer here, not a failed test.
    async fn admits(addr: std::net::SocketAddr, key: PrivateKey) -> bool {
        let config = Arc::new(russh::client::Config::default());
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut session = russh::client::connect_stream(config, stream, TestClient)
            .await
            .expect("ssh handshake");
        let hash = session.best_supported_rsa_hash().await.unwrap().flatten();
        session
            .authenticate_publickey(
                "roomler",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .expect("auth exchange")
            .success()
    }

    /// #3558 — a grant delivered twice is held once. The copy is refused
    /// while its id is pending, so the table holds one entry and the key opens
    /// one session. Before the fix the copy was a second redeemable entry, and
    /// a second connection with the same key got in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_same_grant_twice_is_held_once_and_admits_one_session() {
        use roomler_ai_remote_control::models::SshGrantRefusal;

        let _lock = grant_test().await;
        let (key, line) = client_key(13);
        let addr = serve_one(&cfg_with(Vec::new())).await;
        let addr2 = serve_one(&cfg_with(Vec::new())).await;

        record_copy("g-3558-twice", &line).expect("the first copy is held");
        let copy = record_copy("g-3558-twice", &line);
        let held = super::pending_grants();

        let _first = connect(addr, key.clone()).await;
        assert!(
            !admits(addr2, key).await,
            "a second connection with the same key is refused after the first redeems"
        );
        assert_eq!(held, 1, "the table holds one entry for the two copies");
        let refused = copy.expect_err("a copy whose id is pending is refused");
        assert_eq!(refused.refusal(), SshGrantRefusal::Invalid);
        assert_eq!(super::pending_grants(), 0);
    }

    /// #3558 — a copy that arrives after its grant was redeemed is refused,
    /// and the key opens nothing more. Before the fix redemption removed the
    /// entry, so the copy was recorded as new, with a fresh window of its own
    /// (#3511).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_copy_of_a_redeemed_grant_is_refused() {
        use roomler_ai_remote_control::models::SshGrantRefusal;

        let _lock = grant_test().await;
        let (key, line) = client_key(14);
        let addr = serve_one(&cfg_with(Vec::new())).await;
        let addr2 = serve_one(&cfg_with(Vec::new())).await;

        record_copy("g-3558-redeemed", &line).expect("the grant is held");
        let _first = connect(addr, key.clone()).await;
        assert_eq!(
            super::pending_grants(),
            0,
            "redeeming the grant consumed it"
        );

        let copy = record_copy("g-3558-redeemed", &line);
        assert!(
            !admits(addr2, key).await,
            "a connection with the key is refused after its grant was redeemed"
        );
        let refused = copy.expect_err("a copy of a redeemed grant is refused");
        assert_eq!(refused.refusal(), SshGrantRefusal::Invalid);
        assert_eq!(super::pending_grants(), 0);
    }

    /// #3558 — a second copy does not extend the first one's window. The copy
    /// arrives while the first is pending and is refused, and it leaves the
    /// first copy's deadline as it was. Once that deadline passes, nothing
    /// admits the key. The deadline is driven as in
    /// [`an_expired_grant_admits_nobody`]. Before the fix the copy was held
    /// with a window from its OWN arrival, so the key outlived the first one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_second_copy_does_not_extend_the_first_ones_window() {
        let _lock = grant_test().await;
        let (key, line) = client_key(15);
        let addr = serve_one(&cfg_with(Vec::new())).await;

        record_copy("g-3558-window", &line).expect("the first copy is held");
        let deadline = super::GRANTS.lock().unwrap()[0].deadline;
        let copy = record_copy("g-3558-window", &line);
        assert_eq!(
            super::GRANTS.lock().unwrap()[0].deadline,
            deadline,
            "the copy leaves the first copy's window as it was"
        );

        // The first copy's window ends. Entry 0 is the oldest: the first copy.
        super::GRANTS.lock().unwrap()[0].deadline = std::time::Instant::now();
        assert!(
            !admits(addr, key).await,
            "once the first copy's window ends, the key opens nothing"
        );
        assert!(copy.is_err(), "the copy was refused on arrival");
        assert_eq!(super::pending_grants(), 0);
    }

    /// #3558 — a grant that timed out unredeemed is burned as well: a late
    /// copy of it is refused, not given a fresh window from its own arrival.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_copy_of_a_grant_that_expired_unredeemed_is_refused() {
        use roomler_ai_remote_control::models::SshGrantRefusal;

        let _lock = grant_test().await;
        let (key, line) = client_key(16);
        let addr = serve_one(&cfg_with(Vec::new())).await;

        record_copy("g-3558-expired", &line).expect("the grant is held");
        super::GRANTS.lock().unwrap()[0].deadline = std::time::Instant::now();
        assert_eq!(super::pending_grants(), 0, "its window ended unredeemed");

        let copy = record_copy("g-3558-expired", &line);
        assert!(
            !admits(addr, key).await,
            "a late copy of an expired grant opens nothing"
        );
        let refused = copy.expect_err("a late copy of an expired grant is refused");
        assert_eq!(refused.refusal(), SshGrantRefusal::Invalid);
        assert_eq!(super::pending_grants(), 0);
    }

    /// #3558 — recording many distinct ids, each followed by a copy, keeps
    /// both tables within their bounds: the pending grants and the ids that
    /// left them, some redeemed and the rest pushed out of the full table.
    #[tokio::test]
    async fn both_tables_stay_within_their_bounds() {
        use super::{MAX_PENDING_GRANTS, MAX_REMEMBERED_GRANTS};

        let _lock = grant_test().await;
        let (_key, line) = client_key(17);
        let parsed = russh::keys::ssh_key::PublicKey::from_openssh(&line).unwrap();
        let remembered = || super::SPENT.lock().unwrap().entries.len();

        let ids = MAX_REMEMBERED_GRANTS + 4 * MAX_PENDING_GRANTS;
        for i in 0..ids {
            let id = format!("g-3558-bound-{i}");
            record_copy(&id, &line).expect("a new id is held");
            assert!(
                record_copy(&id, &line).is_err(),
                "{id}: its copy is refused"
            );
            if i % 3 == 0 {
                assert!(
                    super::take_grant_for(&parsed).is_some(),
                    "{id}: one redeems"
                );
            }
            assert!(
                super::pending_grants() <= MAX_PENDING_GRANTS,
                "{id}: pending"
            );
            assert!(remembered() <= MAX_REMEMBERED_GRANTS, "{id}: remembered");
        }
        // 320 ids, 16 of them still pending: 304 left the table, more than
        // the memory holds.
        assert_eq!(super::pending_grants(), MAX_PENDING_GRANTS);
        assert_eq!(remembered(), MAX_REMEMBERED_GRANTS);
    }

    /// #3558 — the worker hands a grant for the SYSTEM service on at first
    /// sight only (FR-105): a copy of an id it has seen is dropped, whether
    /// the first copy rang the service or was refused here.
    #[test]
    fn a_via_service_grant_is_handed_on_at_first_sight_only() {
        let now = std::time::Instant::now();
        let mut seen = super::RememberedIds::new();
        assert!(super::first_sight(&mut seen, "g-a", now), "the first copy");
        assert!(!super::first_sight(&mut seen, "g-a", now), "a copy of it");
        assert!(super::first_sight(&mut seen, "g-b", now), "another grant");

        // The worker's own memory, under an id no other case uses.
        let id = format!("g-3558-service-{}", rand::random::<u64>());
        assert!(super::first_sight_via_service(&id), "the first copy");
        assert!(!super::first_sight_via_service(&id), "a copy of it");
    }

    /// #3558 — the via-service memory holds at most `MAX_REMEMBERED_GRANTS`
    /// ids, and forgets the oldest first.
    #[test]
    fn the_via_service_memory_stays_within_its_bound() {
        use super::MAX_REMEMBERED_GRANTS;

        let now = std::time::Instant::now();
        let mut seen = super::RememberedIds::new();
        let ids = MAX_REMEMBERED_GRANTS + 50;
        for i in 0..ids {
            assert!(super::first_sight(&mut seen, &format!("g-{i}"), now));
            assert!(seen.entries.len() <= MAX_REMEMBERED_GRANTS);
        }
        assert_eq!(seen.entries.len(), MAX_REMEMBERED_GRANTS);
        assert!(
            !super::first_sight(&mut seen, &format!("g-{}", ids - 1), now),
            "the newest id is still remembered"
        );
        assert!(
            super::first_sight(&mut seen, "g-0", now),
            "the oldest was forgotten first"
        );
    }

    /// #3558 — an id is remembered for the whole horizon, and forgotten once
    /// it has passed.
    #[test]
    fn a_via_service_id_is_forgotten_after_the_horizon() {
        use super::GRANT_ID_HORIZON;
        use std::time::Duration;

        let now = std::time::Instant::now();
        let mut seen = super::RememberedIds::new();
        assert!(super::first_sight(&mut seen, "g-h", now));
        let inside = now + GRANT_ID_HORIZON - Duration::from_millis(1);
        assert!(
            !super::first_sight(&mut seen, "g-h", inside),
            "still remembered just inside the horizon"
        );
        assert!(
            super::first_sight(&mut seen, "g-h", now + GRANT_ID_HORIZON),
            "forgotten once the horizon has passed"
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

    /// #2815 — the prompt read `ssh:«name» (via «device»)@«overlay ip»:«port»`.
    /// The person deciding gets the grant's own wording, as the screen-control
    /// and exec prompts word theirs; the logs and activity reports keep the
    /// technical identity, overlay address included.
    #[test]
    fn the_prompt_names_the_caller_as_the_grant_words_it() {
        let mut grant = test_grant(None, 0);
        grant.caller = "Alice (via build-01)".into();
        let resolved = super::sshd::ResolvedSessionPolicy::from_grant(grant, test_peer(), false);
        assert_eq!(resolved.prompt_name, "Alice (via build-01)");
        assert_eq!(resolved.caller, "ssh:Alice (via build-01)@100.64.0.9:55555");
        assert!(
            !resolved.prompt_name.contains("ssh:") && !resolved.prompt_name.contains("100.64."),
            "the prompt must not show the technical identity: {}",
            resolved.prompt_name
        );
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

    /// The questions standing at the device, by the id each is shown and
    /// answered under: the `.pending` markers the companion and `roomlerd
    /// consent --list` read.
    fn questions_up(broker: &crate::consent::ConsentBroker) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(broker.sentinel_dir().unwrap())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".pending"))
                    .map(str::to_string)
            })
            .collect();
        ids.sort();
        ids
    }

    /// What stands once `n` questions do, or after 5 s.
    async fn questions_once_up(broker: &crate::consent::ConsentBroker, n: usize) -> Vec<String> {
        for _ in 0..250 {
            let ids = questions_up(broker);
            if ids.len() >= n {
                return ids;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        questions_up(broker)
    }

    /// What a channel said: stdout, stderr, exit status.
    type Said = (String, String, Option<u32>);

    /// Read `channel` to its end. `asked` fires once the server says the
    /// channel waits on the person at the device.
    async fn read_channel(
        mut channel: russh::Channel<russh::client::Msg>,
        mut asked: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Said {
        let (mut out, mut err, mut exit) = (Vec::new(), Vec::new(), None);
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => out.extend_from_slice(data),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    err.extend_from_slice(data);
                    if String::from_utf8_lossy(&err).contains("waiting up to")
                        && let Some(tx) = asked.take()
                    {
                        let _ = tx.send(());
                    }
                }
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }
        (
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
            exit,
        )
    }

    /// A command running on a channel of its own.
    struct Running {
        /// Fires once the server says the channel waits on the person.
        asked: tokio::sync::oneshot::Receiver<()>,
        done: tokio::task::JoinHandle<Said>,
    }

    async fn exec_in_background(session: &russh::client::Handle<TestClient>, cmd: &str) -> Running {
        let channel = session.channel_open_session().await.unwrap();
        channel.exec(true, cmd).await.unwrap();
        let (tx, asked) = tokio::sync::oneshot::channel();
        Running {
            asked,
            done: tokio::spawn(read_channel(channel, Some(tx))),
        }
    }

    /// Drive one exec through a consent-gated session and collect the outcome.
    /// The operator's decision is played through the SAME broker the server
    /// was built with — `record_decision` only lands while the prompt is
    /// actually pending (the confused-deputy guard), so the approver polls.
    async fn exec_with_operator_decision(
        approve: bool,
        key_seed: u8,
    ) -> (String, String, Option<u32>, bool) {
        use roomler_ai_remote_control::models::ConsentMode;

        // The callers hold `grant_test()` already; the slot comes second.
        let _slot = exec_slot().await;
        let (key, line) = client_key(key_seed);
        let (addr, broker) = serve_one_with(&cfg_with(Vec::new())).await;
        grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();

        // Answered under the id the question is shown under, as the panel,
        // the companion and `roomlerd consent` answer it.
        let operator = tokio::spawn(async move {
            for _ in 0..200 {
                if questions_up(&broker)
                    .iter()
                    .any(|id| broker.record_decision(id, approve))
                {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            false
        });

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

    /// #3442 — an answer given the moment the question is up counts, while
    /// the desktop companion is still being started. The question used to be
    /// shown first and armed last: an answer in between was refused ("no
    /// active prompt"), and on the native panel the panel went down with it,
    /// leaving the question asking with nothing on the screen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_answer_given_as_the_ssh_question_goes_up_counts() {
        use roomler_ai_remote_control::models::ConsentMode;

        let _lock = grant_test().await;
        let _slot = exec_slot().await;
        let (key, line) = client_key(85);
        // A companion that takes 2 s to start: the question is up, its marker
        // written, for all of it.
        let (addr, broker) = serve_one_asking(
            &cfg_with(Vec::new()),
            crate::companion::Companion::Starting(std::time::Duration::from_secs(2)),
        )
        .await;
        grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();
        let session = connect(addr, key).await;
        let command = exec_in_background(&session, "echo early-answer-ran").await;

        // One answer, the moment the question is up, never retried: a refused
        // answer fails the test rather than being tried until it lands.
        let up = questions_once_up(&broker, 1).await;
        let id = up.first().expect("the question went up");
        assert!(
            broker.record_decision(id, true),
            "an answer given as the question went up was refused"
        );
        let (out, err, exit) = command.done.await.unwrap();
        assert!(
            out.contains("early-answer-ran"),
            "stdout {out:?}, stderr {err:?}"
        );
        assert_eq!(exit, Some(0));
    }

    /// #3442 — two commands of one session that ask at once wait on ONE
    /// question, and one Allow runs both. Each used to be asked under the
    /// grant's id, so the second question withdrew the first (since #3434 the
    /// newer question for an id wins), and the first command was refused
    /// although nobody had said no.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_commands_asking_at_once_share_one_question() {
        let [first, second] = two_commands_one_answer(true, 86).await;
        let (out, err, exit) = first;
        assert!(
            out.contains("first-ran"),
            "the first command: stdout {out:?}, stderr {err:?}"
        );
        assert_eq!(exit, Some(0));
        let (out, err, exit) = second;
        assert!(
            out.contains("second-ran"),
            "the second command: stdout {out:?}, stderr {err:?}"
        );
        assert_eq!(exit, Some(0));
    }

    /// #3442 — one No reaches every channel waiting on the question: both
    /// commands are refused, each told why, and neither runs. (A timeout
    /// reaches them the same way, through the same shared answer, but the
    /// window is the 30 s default with no seam to shorten it, so no test
    /// waits it out.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_no_refuses_every_command_waiting_on_the_question() {
        for (n, (out, err, exit)) in two_commands_one_answer(false, 88)
            .await
            .into_iter()
            .enumerate()
        {
            assert!(
                !out.contains("-ran"),
                "command {n} ran despite the No: stdout {out:?}"
            );
            assert!(
                err.contains("denied"),
                "command {n} must be told why it was refused, got stderr {err:?}"
            );
            assert_eq!(exit, Some(1), "command {n}");
        }
    }

    /// Two commands of one session that ask at once, the second while the
    /// first one's question stands, and ONE answer to the one question.
    async fn two_commands_one_answer(allow: bool, key_seed: u8) -> [Said; 2] {
        use roomler_ai_remote_control::models::ConsentMode;

        let _lock = grant_test().await;
        // Two commands at once: two of the engine's slots, taken together.
        let _slots = EXEC_TEST_SLOTS
            .acquire_many(2)
            .await
            .expect("the slot semaphore is never closed");
        let (key, line) = client_key(key_seed);
        let (addr, broker) = serve_one_with(&cfg_with(Vec::new())).await;
        grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();
        let session = connect(addr, key).await;

        let first = exec_in_background(&session, "echo first-ran").await;
        questions_once_up(&broker, 1).await;
        let second = exec_in_background(&session, "echo second-ran").await;
        second
            .asked
            .await
            .expect("the second command is told it waits");
        // Past the poll that would find the first question withdrawn.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let up = questions_up(&broker);
        assert_eq!(up.len(), 1, "one question for both commands: {up:?}");
        assert!(
            broker.record_decision(&up[0], allow),
            "the question was not asking"
        );
        [first.done.await.unwrap(), second.done.await.unwrap()]
    }

    /// #3442 — a command and a shell of one session that ask at once are two
    /// questions, each under an id of its own, and neither withdraws the
    /// other. The question names the activity (FR-27), so an Allow for one is
    /// never stretched over the other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_command_and_a_shell_asking_at_once_are_two_questions() {
        use roomler_ai_remote_control::models::ConsentMode;

        let _lock = grant_test().await;
        let _slot = exec_slot().await;
        let (key, line) = client_key(87);
        let (addr, broker) = serve_one_with(&cfg_with(Vec::new())).await;
        let grant_id = grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();
        let session = connect(addr, key).await;

        let command = exec_in_background(&session, "echo command-ran").await;
        questions_once_up(&broker, 1).await;
        let shell = session.channel_open_session().await.unwrap();
        shell
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        shell.request_shell(true).await.unwrap();

        let up = questions_once_up(&broker, 2).await;
        assert_eq!(up.len(), 2, "one question per activity: {up:?}");
        assert!(
            !up.contains(&grant_id),
            "a question's id is its own, not its grant's: {up:?}"
        );
        for id in &up {
            assert!(broker.record_decision(id, true), "{id} was not asking");
        }

        let leave: &[u8] = if cfg!(windows) {
            b"exit\r\n"
        } else {
            b"exit 0\n"
        };
        shell.data(leave).await.unwrap();
        let (_, err, _) = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            read_channel(shell, None),
        )
        .await
        .expect("the shell leaves when told to");
        // Past the line that says it waits, the server says nothing on
        // stderr: a refusal would say why there.
        let refused = err
            .lines()
            .any(|l| l.contains("roomler-ssh:") && !l.contains("waiting up to"));
        assert!(!refused, "the shell did not start: {err:?}");
        let (out, err, exit) = command.done.await.unwrap();
        assert!(
            out.contains("command-ran"),
            "the command: stdout {out:?}, stderr {err:?}"
        );
        assert_eq!(exit, Some(0));
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

    /// One process of a session, from outside it. Signal 0 only asks, and a
    /// process nobody has reaped yet still answers; the runtime reaps a shell.
    #[cfg(unix)]
    struct Watched(i32);

    #[cfg(unix)]
    impl Watched {
        fn open(pid: u32) -> Self {
            Self(pid as i32)
        }

        fn gone_within(&self, within: std::time::Duration) -> bool {
            let deadline = std::time::Instant::now() + within;
            // SAFETY: signal 0 delivers nothing.
            while unsafe { libc::kill(self.0, 0) } == 0 {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            true
        }
    }

    #[cfg(windows)]
    use crate::exec::session_lifetime::Watched;

    /// #3303 — a client on an interactive shell that sits at its prompt, having
    /// written nothing since it reported itself. On Windows it has also started
    /// a child, which a job reaches; a Unix shell's background job is its own to
    /// hang up, and depends on which shell this is.
    async fn a_client_on_a_quiet_shell(
        seed: u8,
    ) -> (
        russh::client::Handle<TestClient>,
        russh::Channel<russh::client::Msg>,
        Vec<Watched>,
    ) {
        let (key, line) = client_key(seed);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        let session = connect(addr, key).await;
        let channel = session.channel_open_session().await.unwrap();
        channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        channel.request_shell(true).await.unwrap();

        let dir =
            std::env::temp_dir().join(format!("roomlerd-3303-ssh-{}-{seed}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        let (typed, reports) = (
            format!(
                "{}\r\n",
                crate::exec::session_lifetime::starts_a_child_then(&dir, "")
            ),
            2,
        );
        #[cfg(unix)]
        let (typed, reports) = (
            format!("echo $$ > '{}'\n", dir.join("started").display()),
            1,
        );
        channel.data(typed.as_bytes()).await.unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let pids = loop {
            let pids: Vec<u32> = std::fs::read_to_string(dir.join("started"))
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if pids.len() == reports {
                break pids;
            }
            if std::time::Instant::now() >= deadline {
                // #4444 — on Windows, where its line stopped.
                #[cfg(windows)]
                panic!(
                    "the shell never reported itself\n{}",
                    crate::exec::session_lifetime::where_a_command_stopped(&dir)
                );
                #[cfg(unix)]
                panic!("the shell never reported itself");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let _ = std::fs::remove_dir_all(&dir);
        let processes = pids.into_iter().map(Watched::open).collect();
        (session, channel, processes)
    }

    /// How long a lost session's processes may take to go, on a loaded runner.
    const A_LOST_SHELL_GOES_WITHIN: std::time::Duration = std::time::Duration::from_secs(15);

    /// No close and no goodbye: the transport goes, as a killed client's does or
    /// a dead carrier's. A failed write can never tell a shell that writes
    /// nothing, and it ran on for as long as the daemon lived.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_client_vanishing_ends_a_quiet_interactive_shell() {
        // #4444 — one PowerShell `Start-Process` cell at a time.
        #[cfg(windows)]
        let _turn = crate::exec::session_lifetime::Turn::wait_async().await;
        let (session, channel, processes) = a_client_on_a_quiet_shell(47).await;
        drop(channel);
        drop(session);
        for p in &processes {
            assert!(
                p.gone_within(A_LOST_SHELL_GOES_WITHIN),
                "a process of the session outlived its client"
            );
        }
    }

    /// The channel closes and the connection stays, as a multiplexing client's
    /// does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_client_closing_the_channel_ends_a_quiet_interactive_shell() {
        // #4444 — one PowerShell `Start-Process` cell at a time.
        #[cfg(windows)]
        let _turn = crate::exec::session_lifetime::Turn::wait_async().await;
        let (session, channel, processes) = a_client_on_a_quiet_shell(48).await;
        channel.close().await.unwrap();
        for p in &processes {
            assert!(
                p.gone_within(A_LOST_SHELL_GOES_WITHIN),
                "a process of the session outlived its channel"
            );
        }
        drop(session);
    }

    /// A shell without a terminal is refused with a reason rather than handed
    /// one it never asked for. Both platforms, one code path (P4b).
    ///
    /// #4168 — read as success, the reason, exit-status 1, EOF and close, so
    /// `ssh -T` prints the reason and exits 1, and no shell starts. One
    /// thread, so the log is the server's.
    #[tokio::test]
    async fn a_shell_without_a_pty_is_refused_with_a_reason() {
        let log = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
        let (key, line) = client_key(41);
        let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
        let session = connect(addr, key).await;

        let channel = session.channel_open_session().await.unwrap();
        channel.request_shell(true).await.unwrap();
        assert_refused_in_order(&read_in_order(channel).await, "terminal");
        assert_refused_and_nothing_started(
            &log.text(),
            "shell",
            "ssh: interactive session started",
        );
    }

    /// The identity gate applies to interactive sessions exactly as it does to
    /// one-shot commands: an unset key-list account mode means the client gets
    /// a reason, not a root shell. Both platforms — the refusal happens before
    /// any terminal is allocated.
    ///
    /// #4168 — the shell is answered as `refuse` answers: success, the
    /// reason, exit-status 1, EOF and close, so `ssh` prints the reason and
    /// exits 1. Its log line is still the account-mode one, and no terminal
    /// or shell starts. One thread, so the log is the server's.
    #[tokio::test]
    async fn an_interactive_session_without_an_account_mode_gets_no_shell() {
        let log = crate::peer::files_log_tests::Logged::start(tracing::Level::INFO);
        let (key, line) = client_key(42);
        let addr = serve_one(&cfg_with_mode(vec![line], None)).await;
        let session = connect(addr, key).await;

        let channel = session.channel_open_session().await.unwrap();
        channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        channel.request_shell(true).await.unwrap();

        let reads = read_in_order(channel).await;
        // `pty_request` only records the terminal; it is allocated when the
        // shell starts, so this success allocates nothing.
        assert_eq!(
            reads.first(),
            Some(&Read::Success),
            "the terminal's answer, in {reads:?}"
        );
        assert_refused_in_order(&reads[1..], "ssh_account_mode");
        let log = log.text();
        assert!(
            log.contains("ssh: refusing a shell — unusable account mode"),
            "no account-mode refusal in:\n{log}"
        );
        for started in [
            "ssh: interactive session started",
            "ssh: could not start the terminal session",
        ] {
            assert!(
                !log.contains(started),
                "a refused shell got as far as {started:?}:\n{log}"
            );
        }
    }

    /// #4168 — a shell refused for its account mode holds its channel as a
    /// `refuse` does. With a client window of 0 holding the close back, a
    /// second shell, a command and `sftp` asked for on that channel are each
    /// answered with a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_shell_refused_for_its_account_mode_takes_nothing_more() {
        let (key, line) = client_key(161);
        let addr = serve_one(&cfg_with_mode(vec![line], None)).await;
        let no_window = russh::client::Config {
            window_size: 0,
            ..Default::default()
        };
        let session = connect_with(addr, key, no_window).await;

        let mut channel = session.channel_open_session().await.unwrap();
        channel
            .request_pty(true, "xterm", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        channel.request_shell(true).await.unwrap();
        for what in ["the terminal's answer", "the shell's answer"] {
            assert_eq!(
                next_read_soon(&mut channel, what).await,
                Some(Read::Success)
            );
        }
        assert_eq!(
            next_read_soon(&mut channel, "the shell's exit status").await,
            Some(Read::Exit(1))
        );

        channel.request_shell(true).await.unwrap();
        assert_eq!(
            next_read_soon(&mut channel, "a second shell's answer").await,
            Some(Read::Failure)
        );
        channel.exec(true, "whoami").await.unwrap();
        assert_eq!(
            next_read_soon(&mut channel, "a command's answer").await,
            Some(Read::Failure)
        );
        channel.request_subsystem(true, "sftp").await.unwrap();
        assert_eq!(
            next_read_soon(&mut channel, "sftp's answer").await,
            Some(Read::Failure)
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

    // ── FR-105 — the SYSTEM service's server, through the framed pipe ──────

    /// The questions the worker was asked, in order.
    type Asks = Arc<std::sync::Mutex<Vec<crate::ssh_service::ConsentAsk>>>;

    /// One connection to the SYSTEM service's port, carried the way a device
    /// carries it: the returned stream is the caller's connection, the worker
    /// splices it into the "pipe", and the service serves it with its own
    /// host key under `ceiling`. The worker answers every question with
    /// `granted` and `reason`.
    fn through_the_service(
        ceiling: roomler_ai_remote_control::models::SshServiceCeiling,
        granted: bool,
        reason: Option<&'static str>,
    ) -> (tokio::io::DuplexStream, Asks) {
        let pem = super::generate_host_key().unwrap();
        let (service_pipe, worker_pipe) = tokio::io::duplex(256 * 1024);
        tokio::spawn(crate::ssh_service::serve_framed(
            service_pipe,
            move |stream, peer, relay| async move {
                super::serve_for_service(stream, peer, &pem, ceiling, relay).await;
            },
        ));
        let (caller, carried) = tokio::io::duplex(256 * 1024);
        let asks: Asks = Default::default();
        let seen = Arc::clone(&asks);
        tokio::spawn(crate::ssh_service::splice_framed(
            carried,
            worker_pipe,
            "100.64.0.9:41000".parse().unwrap(),
            move |ask: crate::ssh_service::ConsentAsk| {
                seen.lock().unwrap().push(ask.clone());
                async move {
                    crate::ssh_service::ConsentAnswer {
                        id: ask.id,
                        granted,
                        reason: reason.map(String::from),
                    }
                }
            },
        ));
        (caller, asks)
    }

    /// Handshake and authenticate over `stream`; `Err` = refused.
    async fn connect_over(
        stream: tokio::io::DuplexStream,
        key: PrivateKey,
    ) -> Result<russh::client::Handle<TestClient>, ()> {
        let config = Arc::new(russh::client::Config::default());
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
        if res.success() { Ok(session) } else { Err(()) }
    }

    /// Run `cmd` on a new channel: (stdout, stderr, exit status).
    async fn exec_over(
        session: &russh::client::Handle<TestClient>,
        cmd: &str,
    ) -> (String, String, Option<u32>) {
        let mut channel = session.channel_open_session().await.unwrap();
        channel.exec(true, cmd).await.unwrap();
        let (mut out, mut err, mut exit) = (Vec::new(), Vec::new(), None);
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => out.extend_from_slice(data),
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => err.extend_from_slice(data),
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                _ => {}
            }
        }
        (
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
            exit,
        )
    }

    /// FR-105 — a granted session on the SYSTEM service's port runs once the
    /// person says yes, asked through the worker that carried the
    /// connection, about exactly this grant.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_service_session_runs_once_the_worker_says_the_person_approved() {
        use roomler_ai_remote_control::models::{ConsentMode, SshServiceCeiling};
        let _lock = grant_test().await;
        let _slot = exec_slot().await;
        let (key, line) = client_key(80);
        let grant_id = grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();
        let (caller, asks) = through_the_service(SshServiceCeiling::System, true, None);
        let session = connect_over(caller, key)
            .await
            .expect("the grant admits its key");
        let (out, err, exit) = exec_over(&session, "echo through-the-service").await;
        assert!(
            out.contains("through-the-service"),
            "stdout {out:?}, stderr {err:?}"
        );
        assert_eq!(exit, Some(0));
        let asks = asks.lock().unwrap().clone();
        assert_eq!(asks.len(), 1, "asked once: {asks:?}");
        // #3442 — under an id of its own, which the worker files the answer
        // under: never the grant's, which two questions of one session share.
        assert_ne!(asks[0].sentinel, grant_id);
        assert!(
            crate::exec_service::valid_object_id(&asks[0].sentinel),
            "the worker takes only an object id: {asks:?}"
        );
        assert_eq!(asks[0].what, "run a command");
        assert!(asks[0].caller.contains("alice@example.com"), "{asks:?}");
    }

    /// FR-105 — the person's no reaches the caller in words, and nothing runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_service_session_the_person_refuses_runs_nothing() {
        use roomler_ai_remote_control::models::{ConsentMode, SshServiceCeiling};
        let _lock = grant_test().await;
        let _slot = exec_slot().await;
        let (key, line) = client_key(81);
        grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();
        let (caller, asks) =
            through_the_service(SshServiceCeiling::System, false, Some("not today"));
        let session = connect_over(caller, key).await.expect("admitted");
        let (out, err, exit) = exec_over(&session, "echo should-not-run").await;
        assert!(!out.contains("should-not-run"), "it ran: {out:?}");
        assert!(err.contains("not today"), "stderr {err:?}");
        assert_eq!(exit, Some(1));
        assert_eq!(asks.lock().unwrap().len(), 1);
    }

    /// FR-105 — the anchor's ceiling refuses what it does not permit, before
    /// anyone is asked: a SYSTEM session under a ceiling of `console_user`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_anchor_ceiling_refuses_a_system_session_before_asking() {
        use roomler_ai_remote_control::models::{ConsentMode, SshServiceCeiling};
        let _lock = grant_test().await;
        let _slot = exec_slot().await;
        let (key, line) = client_key(82);
        // `daemon`, the account mode `grant_for_with_consent` asks for.
        grant_for_with_consent(&line, 30_000, Some(ConsentMode::Prompt)).unwrap();
        let (caller, asks) = through_the_service(SshServiceCeiling::ConsoleUser, true, None);
        let session = connect_over(caller, key).await.expect("admitted");
        let (out, err, exit) = exec_over(&session, "echo should-not-run").await;
        assert!(!out.contains("should-not-run"), "it ran: {out:?}");
        assert!(err.contains("only as the signed-in user"), "stderr {err:?}");
        assert_eq!(exit, Some(1));
        assert!(asks.lock().unwrap().is_empty(), "nobody is asked");
    }

    /// FR-105 — nothing the signed-in user can write opens the service's
    /// port: a key with no grant is refused (the service has no key list),
    /// and a granted session cannot open a forward (no allowlist either).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_service_port_takes_no_unlisted_key_and_opens_no_forward() {
        use roomler_ai_remote_control::models::SshServiceCeiling;
        let _lock = grant_test().await;
        let (key, _) = client_key(83);
        let (caller, _) = through_the_service(SshServiceCeiling::System, true, None);
        assert!(
            connect_over(caller, key).await.is_err(),
            "no grant, no session"
        );

        let (key, line) = client_key(84);
        grant_for(&line, 30_000).unwrap();
        let (caller, _) = through_the_service(SshServiceCeiling::System, true, None);
        let session = connect_over(caller, key).await.expect("admitted");
        let opened = session
            .channel_open_direct_tcpip("127.0.0.1", 22, "127.0.0.1", 5555)
            .await;
        assert!(opened.is_err(), "a forward through the SYSTEM service");
    }

    /// #3815 — the SSH server logs what reaches it from outside escaped, by
    /// #3812's rule (`docs/security-baseline.md`): what a client sends (the
    /// user it logs in as, its terminal, a subsystem's name, where it asks to
    /// forward to), the server's name for who asked (`caller`), and the
    /// account a policy names. A line break in any of them stays inside its
    /// own log line, and a control character is never written as it came.
    ///
    /// Each case runs on one thread (`#[tokio::test]`), where
    /// [`crate::peer::files_log_tests::Logged`] hears every line the server,
    /// the client and the exec engine write. At the end of the file, so no
    /// line above moves.
    mod wire_text_log_tests {
        use super::*;
        use crate::peer::files_log_tests::Logged;
        use roomler_ai_remote_control::models::{ConsentMode, SshServiceCeiling};

        /// What follows each line break below: a log line starting with it
        /// is one the text started.
        const AFTER_A_BREAK: &str = "SECOND LINE";

        /// `s` as a log line holds it: quoted and escaped.
        fn escaped(s: &str) -> String {
            format!("{s:?}")
        }

        /// The start of [`escaped`], for a value that begins with `s`.
        fn escaped_start(s: &str) -> String {
            let quoted = escaped(s);
            quoted[..quoted.len() - 1].to_string()
        }

        /// The first line of `log` holding `message`.
        fn line_with<'a>(log: &'a str, message: &str) -> &'a str {
            log.lines()
                .find(|l| l.contains(message))
                .unwrap_or_else(|| panic!("no {message:?} line in:\n{log}"))
        }

        /// No line of `log` starts with what followed a line break, and the
        /// control character `raw` is nowhere in it as it came.
        fn assert_one_line_each(log: &str, raw: char) {
            let strays: Vec<&str> = log
                .lines()
                .filter(|l| l.trim_start().starts_with(AFTER_A_BREAK))
                .collect();
            assert!(
                strays.is_empty(),
                "text from outside started a log line of its own: {strays:#?}\nin:\n{log}"
            );
            assert!(
                !log.contains(raw),
                "{raw:?} reached the log as it came:\n{log}"
            );
        }

        /// [`connect`] as `user`, and where the server sees it come from.
        /// `Err` = refused.
        async fn connect_as(
            addr: std::net::SocketAddr,
            key: PrivateKey,
            user: &str,
        ) -> (
            Result<russh::client::Handle<TestClient>, ()>,
            std::net::SocketAddr,
        ) {
            let config = Arc::new(russh::client::Config::default());
            let stream = TcpStream::connect(addr).await.unwrap();
            let from = stream.local_addr().unwrap();
            let mut session = russh::client::connect_stream(config, stream, TestClient)
                .await
                .expect("ssh handshake");
            let hash = session.best_supported_rsa_hash().await.unwrap().flatten();
            let res = session
                .authenticate_publickey(
                    user,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .expect("auth exchange");
            (if res.success() { Ok(session) } else { Err(()) }, from)
        }

        /// A grant from `caller` for the key `line`, as the server pushes one.
        fn grant_from(
            caller: &str,
            line: &str,
            account_mode: &str,
            account: Option<&str>,
            consent: ConsentMode,
        ) {
            super::super::record_grant(
                format!("g-{}", rand::random::<u32>()),
                line.to_string(),
                caller.to_string(),
                account_mode.to_string(),
                account.map(str::to_string),
                Some(consent),
                now_ms() + 30_000,
                0,
            )
            .expect("a grant with a valid key is held");
        }

        /// A client's own strings: the user it logs in as (admitted by the
        /// key list, and refused), the terminal it asks for, a subsystem's
        /// name, and two places it asks to forward to (one the allowlist
        /// refuses, one it lets through to a connect that fails). The
        /// key-list session's `caller` holds the client's user too.
        #[tokio::test]
        async fn a_clients_user_term_subsystem_and_host_never_start_a_log_line_of_their_own() {
            const USER: &str = "alice\nSECOND LINE from a user\u{1b}[2J";
            const TERM: &str = "xterm\nSECOND LINE from a term\u{1b}[2J";
            const SUBSYSTEM: &str = "netconf\nSECOND LINE from a subsystem\u{1b}[2J";
            const REFUSED_HOST: &str = "db\nSECOND LINE from a host\u{1b}[2J";
            // `.invalid` never resolves (RFC 6761), so this forward passes
            // the allowlist and then fails at its connect.
            const ALLOWED_HOST: &str = "db\nSECOND LINE from an allowed host\u{1b}[2J.invalid";
            const REFUSED_USER: &str = "mallory\nSECOND LINE from a refused user\u{1b}[2J";

            let log = Logged::start(tracing::Level::INFO);
            let (key, line) = client_key(150);
            let mut cfg = cfg_with(vec![line]);
            cfg.forward_acl = acl_allowing(ALLOWED_HOST, 5432, 5432);
            let addr = serve_one(&cfg).await;
            let (session, from) = connect_as(addr, key, USER).await;
            let session = session.expect("the listed key is admitted");

            let mut channel = session.channel_open_session().await.unwrap();
            channel
                .request_pty(true, TERM, 80, 24, 0, 0, &[])
                .await
                .unwrap();
            channel.request_subsystem(true, SUBSYSTEM).await.unwrap();
            // The server answers in order: the subsystem's refusal comes
            // after the terminal's request is logged, and after its own line.
            while let Some(msg) = channel.wait().await {
                if matches!(msg, russh::ChannelMsg::Failure) {
                    break;
                }
            }
            assert!(
                session
                    .channel_open_direct_tcpip(REFUSED_HOST, 5432, "127.0.0.1", 5555)
                    .await
                    .is_err(),
                "a forward the allowlist does not name is refused"
            );
            // Its outcome is the resolver's; the line is what is asserted.
            let _ = session
                .channel_open_direct_tcpip(ALLOWED_HOST, 5432, "127.0.0.1", 5556)
                .await;

            let (key, _) = client_key(151);
            let addr = serve_one(&cfg_with(Vec::new())).await;
            let (refused, _) = connect_as(addr, key, REFUSED_USER).await;
            assert!(refused.is_err(), "a key with no grant and no listing");

            let text = log.text();
            assert_one_line_each(&text, '\u{1b}');
            let caller = format!("ssh:{USER}@{from} (key-list)");
            let lines: [(&str, Vec<String>); 5] = [
                (
                    "ssh: authenticated (ssh_authorized_keys)",
                    vec![format!("user={}", escaped(USER))],
                ),
                (
                    "ssh: pty requested",
                    vec![format!("term={}", escaped(TERM))],
                ),
                (
                    "ssh: refused an unsupported request",
                    vec![format!("request={}", escaped(SUBSYSTEM))],
                ),
                (
                    "ssh: forward refused",
                    vec![
                        format!("caller={}", escaped(&caller)),
                        format!("host={}", escaped(REFUSED_HOST)),
                        // The reason names the host it refused.
                        format!("why={}", escaped_start(&format!("{REFUSED_HOST}:5432"))),
                    ],
                ),
                (
                    "ssh: rejected — no live grant for this key",
                    vec![format!("user={}", escaped(REFUSED_USER))],
                ),
            ];
            for (message, fields) in &lines {
                let at = line_with(&text, message);
                for field in fields {
                    assert!(at.contains(field.as_str()), "{field} on {at:?}");
                }
            }
            let allowed = format!("host={}", escaped(ALLOWED_HOST));
            assert!(
                text.lines()
                    .any(|l| (l.contains("ssh: forward could not connect")
                        || l.contains("ssh: forward opened"))
                        && l.contains(&allowed)),
                "{allowed} on the allowed forward's line:\n{text}"
            );
        }

        /// The server's name for who asked (`caller`), as a grant brings it:
        /// on the grant's own lines, through the question to the person at
        /// the device, and into the exec engine's lines, both here and on the
        /// SYSTEM service's port (FR-105), where the worker is asked.
        #[tokio::test]
        async fn a_callers_name_never_starts_a_log_line_of_its_own() {
            const CALLER: &str = "Mallory\nSECOND LINE from a caller\u{7}";
            let _lock = grant_test().await;
            let _slot = exec_slot().await;
            let log = Logged::start(tracing::Level::INFO);

            // Asked here: the person at the device approves.
            let (key, line) = client_key(152);
            let (addr, broker) = serve_one_with(&cfg_with(Vec::new())).await;
            grant_from(CALLER, &line, "daemon", None, ConsentMode::Prompt);
            let operator = tokio::spawn(async move {
                for _ in 0..250 {
                    if questions_up(&broker)
                        .iter()
                        .any(|id| broker.record_decision(id, true))
                    {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                false
            });
            let (session, from) = connect_as(addr, key, "roomler").await;
            let session = session.expect("the grant admits its key");
            let (out, err, _) = exec_over(&session, "echo caller-ran").await;
            assert!(out.contains("caller-ran"), "stdout {out:?}, stderr {err:?}");
            assert!(operator.await.unwrap(), "the person was asked");

            // Asked through the worker, on the SYSTEM service's port.
            let (key, line) = client_key(153);
            grant_from(CALLER, &line, "daemon", None, ConsentMode::Prompt);
            let (stream, _) = through_the_service(SshServiceCeiling::System, true, None);
            let session = connect_over(stream, key)
                .await
                .expect("the grant admits its key");
            let (out, err, _) = exec_over(&session, "echo through-the-service").await;
            assert!(
                out.contains("through-the-service"),
                "stdout {out:?}, stderr {err:?}"
            );

            let text = log.text();
            assert_one_line_each(&text, '\u{7}');
            // The grant's own name for who asked, and the session's: `ssh:`,
            // that name, and where the connection came from.
            let named = format!("caller={}", escaped(CALLER));
            let attributed = format!("caller={}", escaped(&format!("ssh:{CALLER}@{from}")));
            for (message, field) in [
                ("ssh: grant recorded", &named),
                ("ssh: authenticated (server grant)", &named),
                ("ssh: asking the person at the device", &named),
                ("consent prompt surface (ssh)", &named),
                ("ssh: operator approved the session", &named),
                ("exec: streaming command", &attributed),
                ("ssh: exec finished", &attributed),
                (
                    "ssh-service: the person at the device approved the session",
                    &named,
                ),
            ] {
                let at = line_with(&text, message);
                assert!(at.contains(field.as_str()), "{field} on {at:?}");
            }
        }

        /// The account a policy names, which an admin typed and the server
        /// delivers: the exec engine's refusal and the session's last line
        /// hold it escaped. This host has no such account, so nothing runs.
        #[tokio::test]
        async fn a_named_accounts_name_never_starts_a_log_line_of_its_own() {
            const ACCOUNT: &str = "nobody\nSECOND LINE from an account\u{1b}[2J";
            let _lock = grant_test().await;
            let _slot = exec_slot().await;
            let log = Logged::start(tracing::Level::INFO);

            let (key, line) = client_key(154);
            let addr = serve_one(&cfg_with(Vec::new())).await;
            grant_from(
                "alice@example.com",
                &line,
                "named",
                Some(ACCOUNT),
                ConsentMode::Auto,
            );
            let (session, _) = connect_as(addr, key, "roomler").await;
            let session = session.expect("the grant admits its key");
            let (out, err, exit) = exec_over(&session, "echo should-not-run").await;
            assert!(!out.contains("should-not-run"), "it ran: {out:?}, {err:?}");
            assert_eq!(exit, Some(1), "stderr {err:?}");

            let text = log.text();
            assert_one_line_each(&text, '\u{1b}');
            let run_as = format!("run_as={}", escaped(&format!("named:{ACCOUNT}")));
            for message in [
                "exec: refusing to run — the requested identity is unavailable",
                "ssh: exec finished",
            ] {
                let at = line_with(&text, message);
                assert!(at.contains(&run_as), "{run_as} on {at:?}");
            }
        }

        /// The line `sftp-server`'s stderr is logged on.
        const SFTP_STDERR: &str = "ssh: sftp-server stderr";

        /// #3890 — a stand-in `sftp-server` for the server on this thread,
        /// gone with this guard: it writes its lines to its stderr and exits
        /// 0. Unix: `/bin/sh`, running the script the client sends it on the
        /// channel, so no file is written and then run. Windows: a `.cmd`
        /// that echoes them, and the client sends nothing.
        struct StandInSftpServer {
            script: String,
            dir: Option<std::path::PathBuf>,
        }

        impl StandInSftpServer {
            fn writing(lines: &[&str]) -> Self {
                let (program, script, dir) = if cfg!(windows) {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos();
                    let dir = std::env::temp_dir().join(format!("roomler-sftp-stand-in-{nanos}"));
                    std::fs::create_dir_all(&dir).unwrap();
                    let mut cmd = String::from("@echo off\r\n");
                    for line in lines {
                        cmd.push_str(&format!(">&2 echo {line}\r\n"));
                    }
                    cmd.push_str("exit /b 0\r\n");
                    let program = dir.join("sftp-server.cmd");
                    std::fs::write(&program, cmd).unwrap();
                    (program, String::new(), Some(dir))
                } else {
                    let script = lines
                        .iter()
                        .map(|line| format!("printf '%s\\n' '{line}' >&2\n"))
                        .collect();
                    (std::path::PathBuf::from("/bin/sh"), script, None)
                };
                super::super::sshd::STAND_IN_SFTP_SERVER.with(|s| *s.borrow_mut() = Some(program));
                Self { script, dir }
            }

            /// What it writes, as the server reads it trimmed: its lines,
            /// each ended as this OS's `echo` or `printf` ends one.
            fn wrote(lines: &[&str]) -> String {
                lines.join(if cfg!(windows) { "\r\n" } else { "\n" })
            }

            /// SFTP on a key-list session that runs as the daemon: the
            /// stand-in gets its script and EOF, and the channel closes
            /// with its exit status. Then the log, once its stderr line is
            /// in, which its own task writes: 10 s at most.
            async fn run(&self, seed: u8, log: &Logged) -> String {
                let (key, line) = client_key(seed);
                let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
                let session = connect(addr, key).await;
                let mut channel = session.channel_open_session().await.unwrap();
                channel.request_subsystem(true, "sftp").await.unwrap();
                if !self.script.is_empty() {
                    channel.data(self.script.as_bytes()).await.unwrap();
                }
                channel.eof().await.unwrap();
                let mut exit = None;
                while let Some(msg) = channel.wait().await {
                    match msg {
                        russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                        russh::ChannelMsg::Failure => panic!("sftp refused:\n{}", log.text()),
                        _ => {}
                    }
                }
                assert_eq!(exit, Some(0), "the stand-in's exit:\n{}", log.text());
                for _ in 0..500 {
                    if log.text().contains(SFTP_STDERR) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                log.text()
            }
        }

        impl Drop for StandInSftpServer {
            fn drop(&mut self) {
                super::super::sshd::STAND_IN_SFTP_SERVER.with(|s| s.borrow_mut().take());
                if let Some(dir) = &self.dir {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
        }

        /// The lines of `log` that hold `sftp-server`'s stderr.
        fn sftp_stderr_lines(log: &str) -> Vec<&str> {
            log.lines().filter(|l| l.contains(SFTP_STDERR)).collect()
        }

        /// #3890 — `sftp-server`'s stderr goes to the daemon's log, never
        /// the client's channel, as ONE escaped field of one line, like the
        /// text the server takes from the wire: two lines and a control
        /// character from the child stay inside it.
        #[tokio::test]
        async fn sftp_servers_stderr_never_starts_a_log_line_of_its_own() {
            const LINES: [&str; 2] = [
                "sftp-server: first line",
                "SECOND LINE from sftp-server\u{1b}[2J",
            ];
            let log = Logged::start(tracing::Level::INFO);
            let stand_in = StandInSftpServer::writing(&LINES);
            let text = stand_in.run(155, &log).await;

            assert_one_line_each(&text, '\u{1b}');
            let lines = sftp_stderr_lines(&text);
            assert_eq!(lines.len(), 1, "one line for its stderr:\n{text}");
            let msg = format!("msg={}", escaped(&StandInSftpServer::wrote(&LINES)));
            assert!(lines[0].contains(&msg), "{msg} on {:?}", lines[0]);
        }

        /// #3890 — and at most its first 4 KiB, with a count of the bytes
        /// after them: a child that writes a lot to its stderr cannot grow
        /// the daemon's memory, or one line of its log, past that.
        #[tokio::test]
        async fn sftp_servers_stderr_is_logged_4_kib_at_most() {
            let long = "x".repeat(6000);
            let log = Logged::start(tracing::Level::INFO);
            let stand_in = StandInSftpServer::writing(&[long.as_str()]);
            let text = stand_in.run(156, &log).await;

            let lines = sftp_stderr_lines(&text);
            assert_eq!(lines.len(), 1, "one line for its stderr:\n{text}");
            let written = StandInSftpServer::wrote(&[long.as_str()]).len()
                + if cfg!(windows) { 2 } else { 1 };
            let kept = format!("msg={}", escaped(&"x".repeat(4096)));
            let cut = format!("cut_bytes={}", written - 4096);
            for field in [&kept, &cut] {
                assert!(
                    lines[0].contains(field.as_str()),
                    "{field} on {:?}",
                    lines[0]
                );
            }
        }
    }

    /// One connection carries several channels, and each channel's data, EOF,
    /// window change and pty request act on that channel alone: never on a
    /// process, a terminal or a pty request of another channel. At the end of
    /// the file, so no line above moves.
    mod channel_scope_tests {
        use super::*;

        /// How long a test waits for what a channel should say: long, because
        /// a loaded runner starts PowerShell slowly. A channel that has not
        /// said it by then fails the test.
        const WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

        /// [`exec_slot`], for a test that runs `n` commands at once.
        async fn exec_slots(n: u32) -> tokio::sync::SemaphorePermit<'static> {
            EXEC_TEST_SLOTS
                .acquire_many(n)
                .await
                .expect("the slot semaphore is never closed")
        }

        /// A key-list session that runs as the daemon's own account.
        async fn a_connection(seed: u8) -> russh::client::Handle<TestClient> {
            let (key, line) = client_key(seed);
            let addr = serve_one(&cfg_with_mode(vec![line], Some("daemon"))).await;
            connect(addr, key).await
        }

        /// A command that writes back its stdin once its stdin ends.
        fn echo_stdin() -> &'static str {
            if cfg!(windows) {
                "[Console]::In.ReadToEnd()"
            } else {
                "cat"
            }
        }

        /// `line` as typed at a terminal, Enter included.
        fn typed(line: &str) -> Vec<u8> {
            let enter = if cfg!(windows) { "\r\n" } else { "\n" };
            format!("{line}{enter}").into_bytes()
        }

        /// A command the shell answers with `<tag>-42`. The terminal's echo
        /// of the command itself never holds that, so a test that sees it
        /// knows the shell ran the command.
        fn computed(tag: &str) -> String {
            if cfg!(windows) {
                format!("echo {tag}-$(6*7)")
            } else {
                format!("echo {tag}-$((6*7))")
            }
        }

        /// A command the shell answers with its terminal's size, as `tag`,
        /// the size in [`size`]'s form, then `:END`.
        fn size_query(tag: &str) -> String {
            if cfg!(windows) {
                format!(
                    "echo (\"{tag}\" + [Console]::WindowWidth + \"x\" + \
                     [Console]::WindowHeight + \":END\")"
                )
            } else {
                format!("echo {tag}$(stty size):END")
            }
        }

        /// A terminal's size as [`size_query`] reports it.
        fn size(cols: u32, rows: u32) -> String {
            if cfg!(windows) {
                format!("{cols}x{rows}")
            } else {
                format!("{rows} {cols}")
            }
        }

        /// A channel with a terminal of `cols` × `rows` and a shell on it.
        async fn open_terminal(
            session: &russh::client::Handle<TestClient>,
            cols: u32,
            rows: u32,
        ) -> russh::Channel<russh::client::Msg> {
            let channel = session.channel_open_session().await.unwrap();
            channel
                .request_pty(true, "xterm", cols, rows, 0, 0, &[])
                .await
                .unwrap();
            channel.request_shell(true).await.unwrap();
            channel
        }

        /// What a channel has said so far.
        #[derive(Default)]
        struct Heard {
            out: Vec<u8>,
            err: Vec<u8>,
            exit: Option<u32>,
            /// A request on it was answered with a failure.
            failed: bool,
            /// The server closed it.
            ended: bool,
        }

        impl Heard {
            fn out(&self) -> String {
                String::from_utf8_lossy(&self.out).into_owned()
            }

            fn err(&self) -> String {
                String::from_utf8_lossy(&self.err).into_owned()
            }
        }

        /// Read `channel` into `heard` until `enough` holds, the channel ends,
        /// or [`WITHIN`] passes. True if `enough` held.
        async fn hear(
            channel: &mut russh::Channel<russh::client::Msg>,
            heard: &mut Heard,
            enough: impl Fn(&Heard) -> bool,
        ) -> bool {
            let deadline = tokio::time::Instant::now() + WITHIN;
            while !enough(heard) {
                if heard.ended {
                    return false;
                }
                match tokio::time::timeout_at(deadline, channel.wait()).await {
                    Err(_) => return false,
                    Ok(None) => heard.ended = true,
                    Ok(Some(msg)) => match msg {
                        russh::ChannelMsg::Data { ref data } => heard.out.extend_from_slice(data),
                        russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                            heard.err.extend_from_slice(data)
                        }
                        russh::ChannelMsg::ExitStatus { exit_status } => {
                            heard.exit = Some(exit_status)
                        }
                        russh::ChannelMsg::Failure => heard.failed = true,
                        _ => {}
                    },
                }
            }
            true
        }

        /// What the shell printed after `tag`, up to `:END`. Only plain text
        /// counts, so the terminal's echo of the typed command (`$(…)`,
        /// quotes, colour codes) is never taken for the answer.
        fn answer(text: &str, tag: &str) -> Option<String> {
            text.match_indices(tag).find_map(|(at, _)| {
                let rest = &text[at + tag.len()..];
                let value = &rest[..rest.find(":END")?];
                let plain = !value.is_empty()
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-'));
                plain.then(|| value.to_string())
            })
        }

        /// Ask the shell on `terminal` a [`size_query`] under `tag`, and read
        /// its answer.
        async fn size_of(
            terminal: &mut russh::Channel<russh::client::Msg>,
            heard: &mut Heard,
            tag: &str,
        ) -> Option<String> {
            terminal.data(&typed(&size_query(tag))[..]).await.unwrap();
            hear(terminal, heard, |h| answer(&h.out(), tag).is_some()).await;
            answer(&heard.out(), tag)
        }

        /// End the shell on `terminal` and read the channel to its end.
        async fn exit_shell(terminal: &mut russh::Channel<russh::client::Msg>, heard: &mut Heard) {
            terminal.data(&typed("exit 0")[..]).await.unwrap();
            hear(terminal, heard, |h| h.ended).await;
        }

        /// Two commands on one connection, each on a channel of its own: each
        /// reads only its own channel's bytes, and only its own channel's EOF
        /// ends its input.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn two_commands_on_one_connection_each_read_only_their_own_input() {
            let _slots = exec_slots(2).await;
            let session = a_connection(170).await;
            let mut first = session.channel_open_session().await.unwrap();
            let mut second = session.channel_open_session().await.unwrap();
            first.exec(true, echo_stdin()).await.unwrap();
            second.exec(true, echo_stdin()).await.unwrap();
            first.data(&b"first-channel-bytes"[..]).await.unwrap();
            second.data(&b"second-channel-bytes"[..]).await.unwrap();
            first.eof().await.unwrap();

            let mut heard = Heard::default();
            hear(&mut first, &mut heard, |h| h.ended).await;
            let out = heard.out();
            assert!(
                out.contains("first-channel-bytes"),
                "the first command never read its own channel's bytes: {out:?}"
            );
            assert!(
                !out.contains("second-channel"),
                "the first command read the second channel's bytes: {out:?}"
            );
            assert_eq!(heard.exit, Some(0));

            // The first channel's EOF was the first channel's: the second
            // command still reads.
            second.data(&b"-and-after"[..]).await.unwrap();
            second.eof().await.unwrap();
            let mut heard = Heard::default();
            hear(&mut second, &mut heard, |h| h.ended).await;
            let out = heard.out();
            assert!(
                out.contains("second-channel-bytes-and-after"),
                "the second command's input ended before its own channel's EOF: {out:?}"
            );
            assert!(
                !out.contains("first-channel"),
                "the second command read the first channel's bytes: {out:?}"
            );
            assert_eq!(heard.exit, Some(0));
        }

        /// A forward beside a command on one connection: the forward's bytes
        /// and its EOF reach the forward's destination and never the command,
        /// which reads its own channel up to its own EOF.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_forwards_bytes_and_eof_reach_no_command_on_its_connection() {
            use tokio::io::AsyncReadExt;

            let _slot = exec_slot().await;
            let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = destination.local_addr().unwrap().port();
            let arrived = tokio::spawn(async move {
                let (mut stream, _) = destination.accept().await.unwrap();
                let mut got = Vec::new();
                stream.read_to_end(&mut got).await.unwrap();
                got
            });
            let (key, line) = client_key(171);
            let mut cfg = cfg_with_mode(vec![line], Some("daemon"));
            cfg.forward_acl = acl_allowing("127.0.0.1", port, port);
            let session = connect(serve_one(&cfg).await, key).await;

            let mut command = session.channel_open_session().await.unwrap();
            command.exec(true, echo_stdin()).await.unwrap();
            command.data(&b"command-bytes"[..]).await.unwrap();
            let forward = session
                .channel_open_direct_tcpip("127.0.0.1", u32::from(port), "127.0.0.1", 5555)
                .await
                .expect("the allowlist names the destination");
            forward.data(&b"forward-bytes"[..]).await.unwrap();
            forward.eof().await.unwrap();
            let arrived = tokio::time::timeout(WITHIN, arrived)
                .await
                .expect("the forward's EOF reached its destination")
                .unwrap();
            assert_eq!(
                arrived, b"forward-bytes",
                "the forward carries its own bytes"
            );

            command.data(&b"-and-after"[..]).await.unwrap();
            command.eof().await.unwrap();
            let mut heard = Heard::default();
            hear(&mut command, &mut heard, |h| h.ended).await;
            let out = heard.out();
            assert!(
                !out.contains("forward-bytes"),
                "the forward's bytes reached the command: {out:?}"
            );
            assert!(
                out.contains("command-bytes-and-after"),
                "the command's input ended before its own channel's EOF: {out:?}"
            );
            assert_eq!(heard.exit, Some(0));
        }

        /// A window change on another channel leaves a terminal its size, and
        /// the terminal's own window change still resizes it.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_window_change_on_another_channel_leaves_a_terminal_its_size() {
            let session = a_connection(172).await;
            let mut terminal = open_terminal(&session, 100, 30).await;
            let other = session.channel_open_session().await.unwrap();
            other.window_change(50, 10, 0, 0).await.unwrap();

            let mut heard = Heard::default();
            assert_eq!(
                size_of(&mut terminal, &mut heard, "SIZE-A:").await,
                Some(size(100, 30)),
                "another channel's window change resized this terminal: {:?}",
                heard.out()
            );

            // A console applies a resize on its own time, so this asks until
            // the answer changes.
            terminal.window_change(132, 43, 0, 0).await.unwrap();
            let mut resized = None;
            for attempt in 0..20 {
                resized = size_of(&mut terminal, &mut heard, &format!("SIZE-B{attempt}:")).await;
                if resized.is_none() || resized == Some(size(132, 43)) {
                    break;
                }
            }
            assert_eq!(
                resized,
                Some(size(132, 43)),
                "the terminal's own window change did not resize it: {:?}",
                heard.out()
            );
            exit_shell(&mut terminal, &mut heard).await;
            assert_eq!(heard.exit, Some(0));
        }

        /// A pty request on another channel changes neither the request this
        /// channel recorded nor the terminal its shell then starts on, and
        /// that channel's own terminal is what it asked for.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_pty_request_on_another_channel_leaves_a_terminals_own_alone() {
            let session = a_connection(173).await;
            let mut first = session.channel_open_session().await.unwrap();
            first
                .request_pty(true, "xterm", 100, 30, 0, 0, &[])
                .await
                .unwrap();
            let mut second = session.channel_open_session().await.unwrap();
            second
                .request_pty(true, "vt100", 60, 20, 0, 0, &[])
                .await
                .unwrap();
            first.request_shell(true).await.unwrap();

            let mut heard_first = Heard::default();
            assert_eq!(
                size_of(&mut first, &mut heard_first, "SIZE-A:").await,
                Some(size(100, 30)),
                "the first terminal started on the second channel's pty request: {:?}",
                heard_first.out()
            );
            // A Windows console has no terminal type.
            #[cfg(unix)]
            {
                first
                    .data(&typed("echo TERM-A:$TERM:END")[..])
                    .await
                    .unwrap();
                hear(&mut first, &mut heard_first, |h| {
                    answer(&h.out(), "TERM-A:").is_some()
                })
                .await;
                assert_eq!(
                    answer(&heard_first.out(), "TERM-A:").as_deref(),
                    Some("xterm"),
                    "the first terminal took the second channel's terminal type: {:?}",
                    heard_first.out()
                );
            }

            second.request_shell(true).await.unwrap();
            let mut heard_second = Heard::default();
            assert_eq!(
                size_of(&mut second, &mut heard_second, "SIZE-B:").await,
                Some(size(60, 20)),
                "the second terminal is not the one its channel asked for: {:?}",
                heard_second.out()
            );
            #[cfg(unix)]
            {
                second
                    .data(&typed("echo TERM-B:$TERM:END")[..])
                    .await
                    .unwrap();
                hear(&mut second, &mut heard_second, |h| {
                    answer(&h.out(), "TERM-B:").is_some()
                })
                .await;
                assert_eq!(
                    answer(&heard_second.out(), "TERM-B:").as_deref(),
                    Some("vt100"),
                    "the second terminal is not the type its channel asked for: {:?}",
                    heard_second.out()
                );
            }

            assert_eq!(
                size_of(&mut first, &mut heard_first, "SIZE-C:").await,
                Some(size(100, 30)),
                "the second terminal's start changed the first: {:?}",
                heard_first.out()
            );
            exit_shell(&mut second, &mut heard_second).await;
            exit_shell(&mut first, &mut heard_first).await;
            assert_eq!(heard_first.exit, Some(0));
            assert_eq!(heard_second.exit, Some(0));
        }

        /// A shell on a channel that asked for no terminal is refused even
        /// while another channel's pty request stands, and that request is
        /// still there for its own channel's shell. Read whether the refusal
        /// is a failure or the reason, an exit status and a close.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_shell_never_takes_another_channels_pty_request() {
            let session = a_connection(174).await;
            let mut terminal = session.channel_open_session().await.unwrap();
            terminal
                .request_pty(true, "xterm", 80, 24, 0, 0, &[])
                .await
                .unwrap();
            let mut bare = session.channel_open_session().await.unwrap();
            bare.request_shell(true).await.unwrap();

            let mut heard = Heard::default();
            hear(&mut bare, &mut heard, |h| h.failed || !h.out.is_empty()).await;
            assert!(
                heard.out.is_empty(),
                "a channel that asked for no terminal got a shell: {:?}",
                heard.out()
            );
            assert!(
                heard.err().contains("terminal"),
                "the refusal must say why, got {:?}",
                heard.err()
            );

            terminal.request_shell(true).await.unwrap();
            let line = format!("{}; exit 0", computed("terminal-shell"));
            terminal.data(&typed(&line)[..]).await.unwrap();
            let mut heard = Heard::default();
            hear(&mut terminal, &mut heard, |h| h.ended).await;
            assert!(
                heard.out().contains("terminal-shell-42"),
                "the channel's own pty request no longer stood: {:?}",
                heard.out()
            );
            assert_eq!(heard.exit, Some(0));
        }

        /// A second terminal on one connection is a terminal of its own: each
        /// shell reads only the keystrokes typed on its own channel.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_second_terminal_on_one_connection_gets_input_of_its_own() {
            let session = a_connection(175).await;
            let mut first = open_terminal(&session, 80, 24).await;
            let mut second = open_terminal(&session, 80, 24).await;
            let line = format!("{}; exit 0", computed("first-terminal"));
            first.data(&typed(&line)[..]).await.unwrap();
            let line = format!("{}; exit 0", computed("second-terminal"));
            second.data(&typed(&line)[..]).await.unwrap();

            let mut heard_second = Heard::default();
            hear(&mut second, &mut heard_second, |h| h.ended).await;
            let out = heard_second.out();
            assert!(
                !out.contains("first-terminal"),
                "the second terminal read the first's keystrokes: {out:?}"
            );
            assert!(
                out.contains("second-terminal-42"),
                "the second terminal never ran what was typed on it: {out:?}"
            );
            assert_eq!(heard_second.exit, Some(0));

            let mut heard_first = Heard::default();
            hear(&mut first, &mut heard_first, |h| h.ended).await;
            let out = heard_first.out();
            assert!(
                !out.contains("second-terminal"),
                "the first terminal read the second's keystrokes: {out:?}"
            );
            assert!(
                out.contains("first-terminal-42"),
                "the first terminal never ran what was typed on it: {out:?}"
            );
            assert_eq!(heard_first.exit, Some(0));
        }

        /// A terminal's EOF ends its input and nothing more: what was typed
        /// before it still reaches the shell, which finishes by itself with
        /// its own status.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_terminals_eof_leaves_its_shell_to_finish_by_itself() {
            let session = a_connection(176).await;
            let mut terminal = open_terminal(&session, 80, 24).await;
            let line = format!("{}; exit 3", computed("typed-before-eof"));
            terminal.data(&typed(&line)[..]).await.unwrap();
            terminal.eof().await.unwrap();

            let mut heard = Heard::default();
            hear(&mut terminal, &mut heard, |h| h.ended).await;
            assert!(
                heard.out().contains("typed-before-eof-42"),
                "what was typed before the EOF never ran: {:?}",
                heard.out()
            );
            assert_eq!(heard.exit, Some(3), "the shell's own exit status");
        }

        /// The next answer to a request on `channel`, passing over what else
        /// it carries; `None` once the channel has ended.
        async fn next_answer(channel: &mut russh::Channel<russh::client::Msg>) -> Option<bool> {
            let wait = async {
                loop {
                    match channel.wait().await? {
                        russh::ChannelMsg::Success => return Some(true),
                        russh::ChannelMsg::Failure => return Some(false),
                        _ => {}
                    }
                }
            };
            tokio::time::timeout(WITHIN, wait)
                .await
                .expect("the server answers a request within the wait")
        }

        /// A channel opened as a forward takes no session request (RFC 4254
        /// §6): a command, a terminal, a shell, a subsystem and a variable
        /// asked for on it are each answered with a failure, nothing starts,
        /// and the forward still carries its own bytes.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_forward_takes_no_session_request() {
            use tokio::io::AsyncReadExt;

            let _slot = exec_slot().await;
            let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = destination.local_addr().unwrap().port();
            let arrived = tokio::spawn(async move {
                let (mut stream, _) = destination.accept().await.unwrap();
                let mut got = Vec::new();
                stream.read_to_end(&mut got).await.unwrap();
                got
            });
            let (key, line) = client_key(177);
            let mut cfg = cfg_with_mode(vec![line], Some("daemon"));
            cfg.forward_acl = acl_allowing("127.0.0.1", port, port);
            let session = connect(serve_one(&cfg).await, key).await;
            let marks = tempfile::tempdir().expect("a temp dir");
            let ran = marks.path().join("ran");
            let touch = if cfg!(windows) {
                format!("Set-Content -LiteralPath '{}' -Value ran", ran.display())
            } else {
                format!(": > '{}'", ran.display())
            };

            let mut forward = session
                .channel_open_direct_tcpip("127.0.0.1", u32::from(port), "127.0.0.1", 5555)
                .await
                .expect("the allowlist names the destination");
            forward.exec(true, touch.as_str()).await.unwrap();
            assert_eq!(next_answer(&mut forward).await, Some(false), "a command");
            forward
                .request_pty(true, "xterm", 80, 24, 0, 0, &[])
                .await
                .unwrap();
            assert_eq!(next_answer(&mut forward).await, Some(false), "a terminal");
            forward.request_shell(true).await.unwrap();
            assert_eq!(next_answer(&mut forward).await, Some(false), "a shell");
            forward.request_subsystem(true, "sftp").await.unwrap();
            assert_eq!(next_answer(&mut forward).await, Some(false), "a subsystem");
            forward.set_env(true, "ROOMLER_TEST", "1").await.unwrap();
            assert_eq!(next_answer(&mut forward).await, Some(false), "a variable");

            forward.data(&b"forward-bytes"[..]).await.unwrap();
            forward.eof().await.unwrap();
            let arrived = tokio::time::timeout(WITHIN, arrived)
                .await
                .expect("the forward's EOF reached its destination")
                .unwrap();
            assert_eq!(
                arrived, b"forward-bytes",
                "the forward carries its own bytes"
            );
            assert!(!ran.exists(), "a command asked for on the forward ran");
        }
    }

    // ── #4325: the start-up line about the key list as a whole ────────────
    //
    // A server grant is tried before the list and needs no entry in it
    // (`a_grant_admits_its_key_exactly_once` signs in with the list EMPTY),
    // so the line says which sign-ins remain, never that every attempt is
    // refused. Each case reads the line `Ctx::build` writes, level first.

    /// What `f` logs, as the daemon's log file writes it.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
    )]
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
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
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    /// What the worker's server logs as it is built for `cfg`.
    fn build_log(cfg: &crate::config::AgentConfig) -> String {
        logged(|| {
            super::sshd::Ctx::build(cfg, test_services()).expect("a usable host key");
        })
    }

    /// The lines of `log` at `level` that name `ssh_authorized_keys`, trimmed,
    /// so each reads `LEVEL message fields`.
    fn key_list_lines<'a>(log: &'a str, level: &str) -> Vec<&'a str> {
        log.lines()
            .map(str::trim)
            .filter(|l| l.starts_with(level) && l.contains("ssh_authorized_keys"))
            .collect()
    }

    /// The default: no break-glass key. A server grant still signs in, so the
    /// line says that at INFO, and nothing about the list is a WARN.
    #[test]
    fn an_empty_key_list_says_only_server_grants_sign_in_at_info() {
        let log = build_log(&cfg_with(Vec::new()));
        assert_eq!(
            key_list_lines(&log, "INFO"),
            [
                "INFO ssh: no break-glass key is set (ssh_authorized_keys is empty), so only \
                 server grants (roomler ssh) can sign in"
            ],
            "the start line, at INFO:\n{log}"
        );
        assert_eq!(
            key_list_lines(&log, "WARN"),
            Vec::<&str>::new(),
            "no WARN about the list:\n{log}"
        );
    }

    /// Entries, and none parses: the operator meant to set a break-glass key
    /// and none works. Each entry is still warned about by its index alone,
    /// and the summary is a WARN that counts them.
    #[test]
    fn a_key_list_where_no_entry_parses_warns_with_the_count() {
        // Shaped like a key, so the parse fails on the key data. Its comment
        // must not reach the log: a malformed entry is still key material.
        let bad = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIL4325notakey i4325-entry-comment";
        let cases = [
            (
                vec![bad.to_string()],
                "WARN ssh: ssh_authorized_keys has 1 entry and it does not parse, so no \
                 break-glass key works: only server grants (roomler ssh) can sign in",
            ),
            (
                vec![bad.to_string(), "not a key".to_string()],
                "WARN ssh: ssh_authorized_keys has 2 entries and none parses, so no \
                 break-glass key works: only server grants (roomler ssh) can sign in",
            ),
        ];
        for (entries, summary) in cases {
            let n = entries.len();
            let log = build_log(&cfg_with(entries));
            let warns = key_list_lines(&log, "WARN");
            assert_eq!(
                warns
                    .iter()
                    .filter(|l| l.contains("ignoring an unparseable ssh_authorized_keys entry"))
                    .count(),
                n,
                "one WARN per entry, as before:\n{log}"
            );
            assert!(
                warns.contains(&summary),
                "the summary WARN, counting {n}:\n{log}"
            );
            assert_eq!(key_list_lines(&log, "INFO"), Vec::<&str>::new(), "{log}");
            assert!(
                !log.contains("i4325-entry-comment"),
                "an entry's text reached the log:\n{log}"
            );
        }
    }

    /// One key that parses: the list works as the fallback, so no summary is
    /// added (`ssh: server ready` counts the keys), whatever else it holds.
    #[test]
    fn a_key_list_with_one_good_key_adds_no_summary_line() {
        let (_, good) = client_key(43);
        let log = build_log(&cfg_with_mode(vec![good.clone()], Some("console_user")));
        assert_eq!(key_list_lines(&log, "WARN"), Vec::<&str>::new(), "{log}");
        assert_eq!(key_list_lines(&log, "INFO"), Vec::<&str>::new(), "{log}");
        assert!(log.contains("authorized_keys=1"), "{log}");

        // Beside an entry that does not parse: that entry's own WARN only.
        let log = build_log(&cfg_with_mode(
            vec!["not a key".to_string(), good],
            Some("console_user"),
        ));
        let warns = key_list_lines(&log, "WARN");
        assert_eq!(warns.len(), 1, "{log}");
        assert!(
            warns[0].contains("ignoring an unparseable ssh_authorized_keys entry"),
            "{log}"
        );
        assert!(log.contains("authorized_keys=1"), "{log}");
    }
}

// ===========================================================================
// #4325 — the start-up line about `ssh_authorized_keys` as a whole.
// ===========================================================================

/// The line `sshd::Ctx::build` logs about `ssh_authorized_keys` as a whole,
/// at the level it is logged at.
#[cfg(feature = "ssh-server")]
enum KeyListLine {
    Info(String),
    Warn(String),
}

/// What `listed` entries, `usable` of which parse, leave able to sign in.
/// A server grant is tried before the list and needs no entry in it
/// (`auth_publickey`), so whatever the list holds, a grant still signs in.
/// `None` adds no line: `ssh: server ready` counts the usable keys.
#[cfg(feature = "ssh-server")]
fn key_list_line(listed: usize, usable: usize) -> Option<KeyListLine> {
    const ONLY_GRANTS: &str = "only server grants (roomler ssh) can sign in";
    if usable > 0 {
        return None;
    }
    Some(match listed {
        // The default. Nobody asked for a break-glass key, so this is no
        // warning.
        0 => KeyListLine::Info(format!(
            "ssh: no break-glass key is set (ssh_authorized_keys is empty), so {ONLY_GRANTS}"
        )),
        // Someone wrote these entries to have a break-glass key, and none
        // works: they would find out when the server cannot mint grants, the
        // one time that route is for. Each entry has its own WARN, from
        // `Ctx::build`.
        1 => KeyListLine::Warn(format!(
            "ssh: ssh_authorized_keys has 1 entry and it does not parse, so no break-glass key \
             works: {ONLY_GRANTS}"
        )),
        n => KeyListLine::Warn(format!(
            "ssh: ssh_authorized_keys has {n} entries and none parses, so no break-glass key \
             works: {ONLY_GRANTS}"
        )),
    })
}
