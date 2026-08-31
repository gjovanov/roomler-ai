// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-43 P2a — the daemon side of the GUI-worker delegation channel.
//!
//! macOS forces two processes (a root LaunchDaemon has no WindowServer; a
//! GUI-session process cannot create a `utun`) but not two enrollments. The
//! daemon holds the enrollment and the worker holds the screen, so an rc
//! session has to cross between them. This module owns the crossing.
//!
//! **P2a is the channel only.** It carries the handshake and liveness; the nine
//! rc-session payloads land in P2b. The channel ships on its own because it is
//! the invasive half — an exception to LocalAPI's request/response invariant
//! (`localapi::Request::RcAttach`) — and an exception deserves to be proven
//! before anything is built on top of it.
//!
//! ## Why a secret, when every other verb trusts the socket
//!
//! For every other LocalAPI verb the 0600 socket **is** the authorisation: a
//! caller that can open it is the owning user or root, and the verbs are things
//! that user is entitled to do. Attaching is not like that. The legitimate
//! worker is an ordinary process in the user's session — and so is an
//! attacker's. Without a secret, any process in that session could volunteer to
//! serve the device's remote-control sessions: to be the thing that sees the
//! screen and receives the keystrokes.
//!
//! So the daemon mints a fresh secret for each worker it spawns and hands it
//! over **on the worker's stdin**. It is never written to disk, never in argv,
//! and never leaves the host.
//!
//! ⚠️ Two obvious alternatives were measured on a real Mac and rejected, and
//! both failures are silent, which is why they were measured rather than
//! assumed:
//!
//! - **the environment** — `sudo` in the spawn chain runs under the stock
//!   `Defaults env_reset` and discards it. This is not hypothetical: P1's
//!   `ROOMLER_MACOS_SUPERVISED` marker went out this way and never reached a
//!   single worker, unnoticed because nothing read it yet.
//! - **an inherited socketpair**, where possession of the fd would BE the
//!   authorisation — strictly better, but `sudo` closes inherited descriptors
//!   too. Recorded under "Dead hypotheses" in the FR, including that
//!   `launchctl asuser` alone preserves them, so it returns if the chain ever
//!   drops `sudo`.
//!
//! ## Default-deny, three ways
//!
//! 1. No secret issued means refuse. That is the state whenever the supervisor
//!    is off or launchd owns the worker, so "the feature is disabled" is not a
//!    separate code path — it is the absence of a secret.
//! 2. One attached worker at a time. A second attach is refused rather than
//!    displacing the first, because displacement is a denial-of-service
//!    primitive for any local process that guesses right.
//! 3. The secret is revoked when the worker stops, so a worker the supervisor
//!    has already released cannot come back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Bytes of entropy in a minted secret, hex-encoded on the wire. The channel is
/// local and the secret lives for one spawn, so this is far past what an
/// attacker could grind, and it costs nothing.
const SECRET_BYTES: usize = 32;

/// Directory holding the delegation socket.
///
/// Deliberately NOT the LocalAPI directory. `/var/run/roomler` is `0700 root`
/// so that nothing unprivileged can even reach the control socket, and that is
/// worth keeping exactly as it is: the worker needs to traverse ITS directory,
/// and widening the control socket's would trade a real protection for an
/// unrelated feature.
const DELEGATE_DIR: &str = "/var/run/roomler-delegate";

/// One frame on the delegation channel.
///
/// Newline-delimited JSON, matching the LocalAPI protocol's shape but not its
/// wire: this is a private protocol between two processes of one install, so it
/// lives here rather than in the shared protocol crate.
///
/// P2a carries the handshake and liveness. The rc-session payloads
/// (`SessionCreated` / `SdpOffer` / `SdpAnswer` / `Ice` / `Terminate` inbound,
/// `SdpAnswer` / `Ice` / `SessionStats` / `Terminate` outbound) land in P2b.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum DelegateFrame {
    /// Daemon → worker, first frame: the attach was accepted.
    Attached {
        /// The daemon's version, so a worker can refuse a mismatch loudly
        /// instead of failing obscurely on a payload it cannot parse. Both ends
        /// ship in one binary, but the update path replaces them independently.
        daemon_version: String,
    },
    /// Either direction — liveness. A channel with no traffic is
    /// indistinguishable from a wedged one, and the daemon must be able to tell
    /// "no sessions right now" from "the worker is gone" *before* a controller
    /// is waiting on it.
    Ping,
    /// Either direction — the answer to [`DelegateFrame::Ping`].
    Pong,
}

/// The socket a worker running as `uid` dials.
///
/// Per-uid, so a console-user change cannot leave the new worker dialling the
/// old one's endpoint.
pub fn socket_path(uid: u32) -> std::path::PathBuf {
    std::path::Path::new(DELEGATE_DIR).join(format!("{uid}.sock"))
}

/// The daemon's end of the delegation channel.
#[derive(Clone, Default)]
pub struct DelegateHost {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The secret the currently-spawned worker was given. `None` means nobody
    /// may attach — the default, and the disabled state.
    secret: Mutex<Option<String>>,
    attached: AtomicBool,
    /// The socket path currently bound, so [`DelegateHost::revoke`] can unlink
    /// it. `None` = not listening.
    listening: Mutex<Option<std::path::PathBuf>>,
}

/// `chown` a path to `uid`, keeping its existing group.
fn chown_to(path: &std::path::Path, uid: u32) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("path contains a NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call;
    // `-1` for gid means "leave the group unchanged", which is the documented
    // contract of chown(2).
    let rc = unsafe { libc::chown(c.as_ptr(), uid, u32::MAX) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl DelegateHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open (or re-open) the delegation socket for `uid` and mint the secret
    /// that authorises a worker on it. Returns the secret to hand the child.
    ///
    /// Two independent gates, and both are needed:
    ///
    /// - **the socket** is `0600` owned by `uid`, in a `0711` directory, so no
    ///   other unprivileged user can even open it;
    /// - **the secret** is what stops any OTHER process of that same uid — the
    ///   worker is an ordinary user process, and so is an attacker's.
    ///
    /// Errors are logged and swallowed: a daemon that cannot open this socket
    /// still supervises a worker that serves its own sessions, which is P1
    /// behaviour and fine. Refusing to spawn would trade a missing feature for
    /// a missing remote-desktop half.
    pub fn open_for(&self, uid: u32) -> String {
        let secret = self.mint();
        if let Err(e) = self.listen(uid) {
            tracing::warn!(uid, error = %e, "delegation: could not open the worker socket");
        }
        secret
    }

    /// Bind the per-uid socket and serve attaches on it until [`revoke`].
    fn listen(&self, uid: u32) -> std::io::Result<()> {
        self.listen_in(std::path::Path::new(DELEGATE_DIR), uid)
    }

    /// [`listen`] with the directory injected, so the permission bits — which
    /// are one of the two authorisation gates, not decoration — can be
    /// asserted by a test that is not root and cannot write to `/var/run`.
    fn listen_in(&self, dir: &std::path::Path, uid: u32) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir)?;
        // 0711: a worker must TRAVERSE to reach its socket, but nothing needs
        // to enumerate the directory, and not listing it means one user cannot
        // even learn that another has a worker.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o711))?;

        let path = dir.join(format!("{uid}.sock"));
        // A stale socket from a previous daemon would make bind() fail with
        // EADDRINUSE; nothing else may live at this path.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        // chown AFTER chmod: the window between them is 0600 root-owned, which
        // is closed, whereas the reverse order would briefly leave a uid-owned
        // socket at the default mode.
        chown_to(&path, uid)?;

        *self.inner.listening.lock().expect("listen mutex") = Some(path.clone());
        let host = self.clone();
        tokio::spawn(async move {
            tracing::info!(uid, path = %path.display(), "delegation: listening for the worker");
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let host = host.clone();
                        tokio::spawn(async move { host.serve_stream(stream).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "delegation: accept failed; stopping listener");
                        return;
                    }
                }
            }
        });
        Ok(())
    }

    /// Read the attach line off a fresh connection, then serve it.
    async fn serve_stream(&self, stream: UnixStream) {
        let (rd, wr) = tokio::io::split(stream);
        let mut lines = BufReader::new(rd).lines();
        let offered = match lines.next_line().await {
            Ok(Some(line)) => line.trim().to_string(),
            _ => {
                tracing::debug!("delegation: connection closed before offering a secret");
                return;
            }
        };
        self.serve(&offered, Box::new(lines.into_inner()), Box::new(wr))
            .await;
    }

    /// Mint a fresh secret, replacing any previous one.
    ///
    /// Synchronous on purpose: the critical section is one `Option<String>`
    /// swap, and an async lock would force every caller to be async —
    /// including `stop_worker`, which is deliberately blocking because it
    /// waits out a SIGTERM grace period.
    fn mint(&self) -> String {
        use rand::RngCore;
        let mut raw = [0u8; SECRET_BYTES];
        rand::rng().fill_bytes(&mut raw);
        let secret = raw.iter().map(|b| format!("{b:02x}")).collect::<String>();
        *self.inner.secret.lock().expect("secret mutex") = Some(secret.clone());
        secret
    }

    /// Forget the current secret — the worker it belonged to is gone.
    ///
    /// [`crate::macos_supervisor`] calls this from `stop_worker`, which takes a
    /// `&DelegateHost` for exactly this reason: revocation is not something a
    /// caller can forget at one of the four places a worker can stop.
    pub fn revoke(&self) {
        *self.inner.secret.lock().expect("secret mutex") = None;
        // Unlink the socket too. The accept loop ends when the listener drops
        // with the process, but a path left behind is an endpoint a later
        // worker could dial and sit on forever waiting for a greeting.
        if let Some(path) = self.inner.listening.lock().expect("listen mutex").take() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Serve one attach attempt. Returns when the channel closes.
    ///
    /// ⚠️ Every refusal is silent and identical from the caller's side: the
    /// connection simply closes. Distinguishing "wrong secret" from "already
    /// attached" from "not accepting" would hand a local attacker an oracle,
    /// and there is nothing a legitimate worker could usefully do differently.
    /// The daemon log says which, because the operator is not the attacker.
    pub async fn serve(
        &self,
        offered: &str,
        rd: Box<dyn AsyncRead + Send + Unpin>,
        wr: Box<dyn AsyncWrite + Send + Unpin>,
    ) {
        {
            // Scoped so the (std, non-async) guard is dropped before any await
            // below — holding it across one would be a deadlock waiting to
            // happen and a clippy error besides.
            let held = self.inner.secret.lock().expect("secret mutex");
            let Some(expected) = held.as_deref() else {
                tracing::warn!("delegation attach refused: no worker secret is currently issued");
                return;
            };
            if !secret_eq(expected, offered) {
                tracing::warn!("delegation attach refused: secret mismatch");
                return;
            }
        }
        if self
            .inner
            .attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::warn!("delegation attach refused: a worker is already attached");
            return;
        }
        tracing::info!("delegation channel attached");
        let result = self.run(rd, wr).await;
        self.inner.attached.store(false, Ordering::Release);
        match result {
            Ok(()) => tracing::info!("delegation channel closed"),
            Err(e) => tracing::warn!(error = %e, "delegation channel ended with an error"),
        }
    }

    /// The frame loop. P2a answers liveness and nothing else.
    async fn run(
        &self,
        rd: Box<dyn AsyncRead + Send + Unpin>,
        mut wr: Box<dyn AsyncWrite + Send + Unpin>,
    ) -> std::io::Result<()> {
        write_frame(
            &mut wr,
            &DelegateFrame::Attached {
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await?;

        let mut lines = BufReader::new(rd).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<DelegateFrame>(&line) {
                Ok(DelegateFrame::Ping) => write_frame(&mut wr, &DelegateFrame::Pong).await?,
                Ok(DelegateFrame::Pong) => {}
                Ok(DelegateFrame::Attached { .. }) => {
                    // Daemon to worker only. A worker sending it is confused
                    // about which end it is; say so rather than ignore it.
                    tracing::warn!("delegation: worker sent an `attached` frame; ignoring");
                }
                Err(e) => {
                    // Do NOT close on an unknown frame: a NEWER worker may send
                    // something this daemon has never heard of, and an additive
                    // protocol is only additive if old readers skip what they
                    // do not know. Same rule as `AgentCaps.rpc`.
                    tracing::debug!(error = %e, "delegation: skipping an unparseable frame");
                }
            }
        }
        Ok(())
    }
}

/// Write one newline-delimited frame and flush it.
///
/// Flushing per frame is deliberate: this channel is latency-sensitive at
/// session setup and idle the rest of the time, so there is nothing to batch
/// and a buffered ICE candidate is a slower session for no gain.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    wr: &mut W,
    frame: &DelegateFrame,
) -> std::io::Result<()> {
    let line = serde_json::to_string(frame).expect("DelegateFrame serialises");
    wr.write_all(line.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    wr.flush().await
}

/// Constant-time comparison.
///
/// The channel is local, so a timing side channel is a stretch — but this is
/// the authorisation path, the fix is three lines, and "the attacker is already
/// local" is exactly the threat this secret exists for. Length is compared
/// first and in variable time on purpose: the length of a hex secret is not a
/// secret.
fn secret_eq(expected: &str, offered: &str) -> bool {
    use subtle::ConstantTimeEq;
    let a = expected.as_bytes();
    let b = offered.as_bytes();
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one line from the client side, or `None` if the daemon just closed.
    async fn first_line(client: tokio::io::DuplexStream) -> Option<String> {
        let (rd, _wr) = tokio::io::split(client);
        BufReader::new(rd).lines().next_line().await.unwrap()
    }

    #[tokio::test]
    async fn refuses_when_no_secret_is_issued() {
        let host = DelegateHost::new();
        let (client, server) = tokio::io::duplex(1024);
        let (rd, wr) = tokio::io::split(server);
        host.serve("anything", Box::new(rd), Box::new(wr)).await;
        // A refusal is a close with no bytes written: the caller learns nothing.
        assert!(first_line(client).await.is_none());
    }

    #[tokio::test]
    async fn refuses_a_wrong_secret() {
        let host = DelegateHost::new();
        let _secret = host.mint();
        let (client, server) = tokio::io::duplex(1024);
        let (rd, wr) = tokio::io::split(server);
        host.serve("not-it", Box::new(rd), Box::new(wr)).await;
        assert!(
            first_line(client).await.is_none(),
            "a wrong secret must not be greeted"
        );
    }

    #[tokio::test]
    async fn accepts_the_right_secret_and_answers_liveness() {
        let host = DelegateHost::new();
        let secret = host.mint();
        assert_eq!(
            secret.len(),
            SECRET_BYTES * 2,
            "hex of {SECRET_BYTES} bytes"
        );

        let (client, server) = tokio::io::duplex(1024);
        let (rd, wr) = tokio::io::split(server);
        let h = host.clone();
        let task = tokio::spawn(async move { h.serve(&secret, Box::new(rd), Box::new(wr)).await });

        let (crd, mut cwr) = tokio::io::split(client);
        let mut lines = BufReader::new(crd).lines();
        let greeting = lines.next_line().await.unwrap().expect("greeting");
        assert!(greeting.contains("\"attached\""), "got {greeting}");

        cwr.write_all(b"{\"t\":\"ping\"}\n").await.unwrap();
        let pong = lines.next_line().await.unwrap().expect("pong");
        assert!(pong.contains("\"pong\""), "got {pong}");

        drop(cwr);
        drop(lines);
        task.await.unwrap();
    }

    /// An unknown frame must not close the channel: a NEWER worker may send one,
    /// and an additive protocol is only additive if old readers skip it.
    #[tokio::test]
    async fn an_unknown_frame_does_not_close_the_channel() {
        let host = DelegateHost::new();
        let secret = host.mint();
        let (client, server) = tokio::io::duplex(1024);
        let (rd, wr) = tokio::io::split(server);
        let h = host.clone();
        let task = tokio::spawn(async move { h.serve(&secret, Box::new(rd), Box::new(wr)).await });

        let (crd, mut cwr) = tokio::io::split(client);
        let mut lines = BufReader::new(crd).lines();
        let _greeting = lines.next_line().await.unwrap().expect("greeting");

        cwr.write_all(b"{\"t\":\"from_the_future\"}\n")
            .await
            .unwrap();
        cwr.write_all(b"{\"t\":\"ping\"}\n").await.unwrap();
        let pong = lines.next_line().await.unwrap().expect("still alive");
        assert!(pong.contains("\"pong\""), "got {pong}");

        drop(cwr);
        drop(lines);
        task.await.unwrap();
    }

    /// Revocation is what stops a worker the supervisor already released from
    /// coming back — the stale-orphan class that cost two releases in P1.
    #[tokio::test]
    async fn a_revoked_secret_stops_working() {
        let host = DelegateHost::new();
        let secret = host.mint();
        host.revoke();
        let (client, server) = tokio::io::duplex(1024);
        let (rd, wr) = tokio::io::split(server);
        host.serve(&secret, Box::new(rd), Box::new(wr)).await;
        assert!(first_line(client).await.is_none());
    }

    /// A second attach is refused, not allowed to displace the first: otherwise
    /// any local process that learned the secret could knock the real worker
    /// off at will.
    #[tokio::test]
    async fn a_second_attach_is_refused_rather_than_displacing() {
        let host = DelegateHost::new();
        let secret = host.mint();

        let (client1, server1) = tokio::io::duplex(1024);
        let (rd1, wr1) = tokio::io::split(server1);
        let h = host.clone();
        let s1 = secret.clone();
        let first = tokio::spawn(async move { h.serve(&s1, Box::new(rd1), Box::new(wr1)).await });

        let (crd1, cwr1) = tokio::io::split(client1);
        let mut l1 = BufReader::new(crd1).lines();
        assert!(
            l1.next_line().await.unwrap().unwrap().contains("attached"),
            "the first attach should be greeted"
        );

        let (client2, server2) = tokio::io::duplex(1024);
        let (rd2, wr2) = tokio::io::split(server2);
        host.serve(&secret, Box::new(rd2), Box::new(wr2)).await;
        assert!(
            first_line(client2).await.is_none(),
            "the second attach must be refused, not served"
        );

        drop(cwr1);
        drop(l1);
        let _ = first.await;
    }

    #[test]
    fn secret_comparison_rejects_prefixes_and_lengths() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "ab"));
        assert!(!secret_eq("ab", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("", "x"));
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// The socket's mode and ownership ARE an authorisation gate — the secret
    /// stops another process of the same user, and these stop every other user.
    /// A future change that widened them would look like a permissions tidy-up
    /// in review, so they are asserted.
    #[tokio::test]
    async fn the_socket_is_0600_and_the_directory_is_traversable_only() {
        let tmp = std::env::temp_dir().join(format!("roomler-deleg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let host = DelegateHost::new();
        let uid = unsafe { libc::getuid() };
        host.listen_in(&tmp, uid).expect("listen");

        let dir_mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o711,
            "directory must be traversable, not listable"
        );

        let sock = tmp.join(format!("{uid}.sock"));
        let md = std::fs::metadata(&sock).unwrap();
        assert_eq!(
            md.permissions().mode() & 0o777,
            0o600,
            "socket must be 0600"
        );
        assert_eq!(md.uid(), uid, "socket must belong to the worker's user");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// End to end over a real unix socket: the right secret is greeted, and a
    /// wrong one gets a close with no bytes — the property the whole design
    /// rests on, exercised through the actual transport rather than in-process.
    #[tokio::test]
    async fn a_real_connection_is_greeted_or_silently_closed() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let tmp = std::env::temp_dir().join(format!("roomler-deleg-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let host = DelegateHost::new();
        let uid = unsafe { libc::getuid() };
        let secret = host.mint();
        host.listen_in(&tmp, uid).expect("listen");
        let sock = tmp.join(format!("{uid}.sock"));

        // Wrong secret: closed, no bytes.
        let mut bad = tokio::net::UnixStream::connect(&sock).await.unwrap();
        bad.write_all(
            b"not-the-secret
",
        )
        .await
        .unwrap();
        let (brd, _bwr) = tokio::io::split(bad);
        assert!(
            BufReader::new(brd)
                .lines()
                .next_line()
                .await
                .unwrap()
                .is_none(),
            "a wrong secret must get no bytes at all"
        );

        // Right secret: greeted.
        let mut good = tokio::net::UnixStream::connect(&sock).await.unwrap();
        good.write_all(
            format!(
                "{secret}
"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let (grd, _gwr) = tokio::io::split(good);
        let greeting = BufReader::new(grd).lines().next_line().await.unwrap();
        assert!(
            greeting.is_some_and(|g| g.contains("attached")),
            "the right secret must be greeted"
        );

        host.revoke();
        assert!(!sock.exists(), "revoke must unlink the socket");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
