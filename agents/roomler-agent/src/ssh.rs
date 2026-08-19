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
//! # What a session can do
//!
//! Commands inherit the daemon's identity — **SYSTEM on Windows, root under
//! systemd** — exactly like Fleet RPC, and for the same reason (the
//! diagnostics this exists for need it). Privilege drop and local-account
//! mapping are P5. Anyone enabling this before then is granting root to
//! whoever holds a listed key.
//!
//! [`ssh_authorized_keys`]: crate::config::AgentConfig::ssh_authorized_keys

use tracing::warn;

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
    let ctx = build_ctx(cfg);
    Box::new(move |ip, nm, mtu| {
        let ctx = ctx.clone();
        let dev = inner(ip, nm, mtu)?;
        // Loud about the one case where switching this on changes who answers
        // an address that already had a server: `neo16` (sshd bound to the
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
fn build_ctx(cfg: &crate::config::AgentConfig) -> ServeCtx {
    let _ = cfg;
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

    use russh::keys::ssh_key::{self, PublicKey};
    use russh::server::{Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use tracing::{info, warn};

    /// Everything a session needs, resolved once when the overlay device is
    /// built rather than per connection.
    pub struct Ctx {
        pub config: Arc<russh::server::Config>,
        /// Keys allowed to authenticate. Empty is a valid, safe state: the
        /// transport answers, every authentication attempt fails, and the
        /// operator sees exactly why in the log.
        pub authorized: Vec<PublicKey>,
    }

    impl Ctx {
        /// Returns `None` when the node has no usable host key — the caller
        /// then serves nothing, which is the right failure: an SSH endpoint
        /// with an improvised identity is worse than no endpoint.
        pub fn build(cfg: &crate::config::AgentConfig) -> Option<Arc<Self>> {
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

            info!(
                fingerprint = %host_fingerprint,
                authorized_keys = authorized.len(),
                "ssh: server ready"
            );
            Some(Arc::new(Self {
                config: Arc::new(config),
                authorized,
            }))
        }
    }

    /// Per-connection state.
    pub struct Handler {
        ctx: Arc<Ctx>,
        /// The calling node's overlay address. Unforgeable — the packet reached
        /// us only by clearing WireGuard against a key from our netmap — so it
        /// is what every log line and audit record is keyed on.
        peer: std::net::SocketAddr,
        /// The SSH username offered. Recorded, never trusted — it is whatever
        /// the client typed. The authorized principal, when there is one, is
        /// [`Self::principal`].
        user: String,
        /// The roomler principal the server named in the grant. `None` means
        /// the session authenticated off the device-owned key list, where no
        /// roomler identity exists to name.
        principal: Option<String>,
        /// The redeemed grant, kept for the account mode it carries (P5 acts
        /// on it) and for the audit record.
        grant: Option<super::Grant>,
    }

    impl Handler {
        pub fn new(ctx: Arc<Ctx>, peer: std::net::SocketAddr) -> Self {
            Self {
                ctx,
                peer,
                user: String::new(),
                principal: None,
                grant: None,
            }
        }

        /// Who to attribute this session's actions to. Prefers the
        /// server-named principal; falls back to the SSH username and peer,
        /// which is all a key-list session can honestly claim.
        fn caller_label(&self) -> String {
            match &self.principal {
                Some(p) => format!("ssh:{p}@{}", self.peer),
                None => format!("ssh:{}@{} (key-list)", self.user, self.peer),
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
                self.user = user.to_string();
                self.principal = Some(grant.caller.clone());
                self.grant = Some(grant.clone());
                info!(
                    peer = %self.peer, %user, %fingerprint,
                    grant_id = %grant.grant_id, caller = %grant.caller,
                    account_mode = %grant.account_mode,
                    "ssh: authenticated (server grant)"
                );
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
                self.user = user.to_string();
                info!(
                    peer = %self.peer, %user, %fingerprint,
                    "ssh: authenticated (ssh_authorized_keys)"
                );
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
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
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
            let caller = self.caller_label();
            // Resolve the identity from the GRANT, not from anything the
            // client said. A key-list session has no grant and therefore no
            // policy behind it, so it gets the daemon identity — which is what
            // it already had, and what the config key's documentation warns
            // about in those words.
            let run_as = match &self.grant {
                Some(g) => crate::exec::RunAs::from_wire(&g.account_mode, g.account.as_deref()),
                None => Ok(crate::exec::RunAs::Daemon),
            };
            let run_as = match run_as {
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
            tokio::spawn(async move {
                run_exec(handle, channel, command, caller, run_as).await;
            });
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            refuse(
                session,
                channel,
                self.peer,
                "shell",
                "interactive shells arrive in P4 (PTY). Use `ssh <node> <command>` for now.",
            )
        }

        #[allow(clippy::too_many_arguments)]
        async fn pty_request(
            &mut self,
            channel: ChannelId,
            _term: &str,
            _col_width: u32,
            _row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            refuse(
                session,
                channel,
                self.peer,
                "pty",
                "PTY allocation arrives in P4. Add `-T` to skip it.",
            )
        }

        async fn subsystem_request(
            &mut self,
            channel: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // Named explicitly because `scp` on a modern OpenSSH silently
            // becomes an SFTP subsystem request, and "scp just hangs" is a
            // much worse diagnostic than "not implemented yet".
            let why = if name == "sftp" {
                "SFTP (and therefore scp) arrives in P7."
            } else {
                "subsystems are not implemented."
            };
            refuse(session, channel, self.peer, name, why)
        }
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
    async fn run_exec(
        handle: russh::server::Handle,
        channel: ChannelId,
        command: String,
        caller: String,
        run_as: crate::exec::RunAs,
    ) {
        use roomler_ai_remote_control::models::exec_limits;

        let request_id = format!("ssh-{:016x}", rand::random::<u64>());
        let identity = run_as.label();
        let privileged = run_as.is_privileged();
        let req = crate::exec::ExecRequest {
            request_id,
            // Empty = the host's own default shell, matching `roomler exec`.
            shell: String::new(),
            command,
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
    }
}

/// Build the per-session server context from config.
#[cfg(all(feature = "overlay-netstack", feature = "ssh-server"))]
fn build_ctx(cfg: &crate::config::AgentConfig) -> ServeCtx {
    sshd::Ctx::build(cfg)
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
pub fn record_grant(
    grant_id: String,
    public_key: String,
    caller: String,
    account_mode: String,
    account: Option<String>,
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

    /// Serve exactly one connection on a loopback port and hand back its
    /// address. The transport is a plain TCP socket here on purpose: P1's tests
    /// already prove the interception path, so this exercises only the SSH
    /// layer sitting on top of it.
    async fn serve_one(cfg: &crate::config::AgentConfig) -> std::net::SocketAddr {
        let ctx = super::sshd::Ctx::build(cfg).expect("a usable host key");
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
        addr
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

    /// The end-to-end P2 claim: a real SSH client runs a real command through
    /// the daemon's real exec engine and gets its output and exit status.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_authorized_key_runs_a_command_and_gets_its_output() {
        let (key, line) = client_key(4);
        let addr = serve_one(&cfg_with(vec![line])).await;
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
        channel.request_subsystem(true, "sftp").await.unwrap();

        let mut refused = false;
        let mut reason = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Failure => {
                    refused = true;
                    break;
                }
                russh::ChannelMsg::Success => panic!("sftp must not be accepted"),
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
            reason.contains("SFTP"),
            "the refusal must say why — a silent failure is how `scp` turns into a hang; got {reason:?}"
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

    fn grant_for(line: &str, ttl_ms: u64) -> Result<(), String> {
        super::record_grant(
            format!("g-{}", rand::random::<u32>()),
            line.to_string(),
            "alice@example.com".into(),
            "daemon".into(),
            None,
            now_ms() + ttl_ms,
            0,
        )
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
}
