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
//! over in the spawn environment. It is never written to disk and never leaves
//! the host. `sudo` in the spawn chain closes inherited descriptors, which is
//! why this is a secret rather than the structurally safer inherited
//! socketpair — measured, and recorded under "Dead hypotheses" in the FR.
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

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use tunnel_core::localapi::DelegateFrame;

/// Environment variable carrying the per-spawn attach secret to the worker.
///
/// ⚠️ Environment, deliberately, not a file: a file would have to be created,
/// ACL'd and cleaned up on every path out of the supervisor — including the
/// ones that end in SIGKILL — and a stale one is a standing credential. The
/// environment of a process the daemon spawned is readable by root and by that
/// user, which is the same audience the secret already has to trust.
pub const ATTACH_SECRET_ENV: &str = "ROOMLER_MACOS_ATTACH_SECRET";

/// Bytes of entropy in a minted secret, hex-encoded on the wire. The channel is
/// local and the secret lives for one spawn, so this is far past what an
/// attacker could grind, and it costs nothing.
const SECRET_BYTES: usize = 32;

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
}

impl DelegateHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh secret for a worker about to be spawned, replacing any
    /// previous one. Returns the value to put in the child's environment.
    ///
    /// Synchronous on purpose: the critical section is one `Option<String>`
    /// swap, and an async lock would force every caller to be async —
    /// including `stop_worker`, which is deliberately blocking because it
    /// waits out a SIGTERM grace period.
    pub fn mint(&self) -> String {
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
