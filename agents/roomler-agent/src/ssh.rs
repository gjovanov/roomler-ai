//! Roomler SSH — the in-daemon SSH surface on this node's overlay address.
//!
//! # Slice P1 (this file today): the transport seam only
//!
//! Everything below the SSH protocol is here and provably working: the
//! interception decision, the in-process termination, the accept loop, and the
//! per-connection task. What is *served* on an accepted connection is currently
//! a banner + echo, deliberately — it makes the transport independently
//! field-testable (`nc <overlay-ip> 2222` from any peer) before a single byte of
//! SSH code exists, so a failure in a later slice can never be confused with a
//! failure of the plumbing.
//!
//! P2 replaces [`serve_conn`] with a russh server. Nothing else in this module
//! changes: an accepted [`NsTcpStream`] is an ordinary `AsyncRead + AsyncWrite`,
//! which is exactly what `russh::server::run_stream` wants.
//!
//! # Why the connection is trustworthy *as a transport*, and why that is not
//! enough
//!
//! Anything arriving here has already been decrypted by WireGuard against a
//! peer key the coordination server put in our netmap, so `peer_addr()` is the
//! overlay address of a specific enrolled node in a specific org — not a claim,
//! a cryptographic fact. That is the foundation the later slices build
//! authorization on (it is what lets roomler SSH have no `authorized_keys`).
//!
//! It is *not* authorization. An enrolled peer is authenticated, not entitled:
//! the caller's user, their permission bit, the target's policy and the
//! consent mode are all still to come in P3, and until then this module serves
//! nothing but an echo. Do not let a later slice skip that step because the
//! transport already "knows who it is".

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
    Box::new(move |ip, nm, mtu| {
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
            accept_loop(listener, port).await;
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
async fn accept_loop(mut listener: tunnel_core::overlay::netstack::NsListener, port: u16) {
    use tracing::info;

    info!(port, "ssh: serving the intercepted overlay port");
    while let Some(stream) = listener.accept().await {
        tokio::spawn(serve_conn(stream));
    }
    info!(
        port,
        "ssh: intercepted port no longer served (session ended)"
    );
}

/// P1 payload: prove the path, then get out of the way.
///
/// Announces the peer the daemon believes it is talking to — which doubles as
/// the field check that interception is picking up the *right* traffic — then
/// echoes until EOF so a tester can confirm both directions.
#[cfg(feature = "overlay-netstack")]
async fn serve_conn(mut stream: tunnel_core::overlay::netstack::NsTcpStream) {
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
