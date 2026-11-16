// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3840 — a TCP forwarder in front of a real Redis that can be told to stop
//! answering without closing a socket: the stand-in for a Redis node lost
//! without a TCP reset (a dropped network path, a frozen machine).
//!
//! - **pass**: bytes flow both ways.
//! - **swallow**: every byte read from either side is dropped and every socket
//!   stays open. This process's kernel keeps acknowledging what the client
//!   sends, as the kernel of a frozen Redis does, so the client's TCP stack
//!   never reports an error. A connection that dropped a byte never forwards
//!   again: a live TCP connection loses no byte, so one that did stands for a
//!   node that is gone. A connection accepted while swallowing is swallowed the
//!   same way, from its first byte.
//! - **one delayed reply**: the next chunk Redis sends on any connection is
//!   held for the given time, a slow answer that still arrives.
//!
//! Dropping the forwarder aborts every task it started, which closes its
//! sockets.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Default)]
struct Shared {
    swallow: AtomicBool,
    delay_next_reply_ms: AtomicU64,
    accepted: AtomicU64,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Shared {
    fn keep(&self, task: JoinHandle<()>) {
        // A std mutex, held for the push only, never across an await.
        self.tasks.lock().expect("forwarder task list").push(task);
    }
}

/// The forwarder. Start it with [`RedisBlackout::start`], point a server at
/// [`RedisBlackout::url_for`], then flip it.
pub struct RedisBlackout {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

/// The test Redis the integration lane uses: `ROOMLER__REDIS__URL`, else the
/// default `test_settings` names.
pub fn test_redis_url() -> String {
    std::env::var("ROOMLER__REDIS__URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

/// `host:port` of a `redis://[user:pass@]host:port[/db]` URL.
fn host_port(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(authority);
    if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:6379")
    }
}

impl RedisBlackout {
    /// Listen on a free loopback port and forward every connection to the
    /// Redis that `upstream_url` names.
    pub async fn start(upstream_url: &str) -> Self {
        let upstream = host_port(upstream_url);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the forwarder");
        let addr = listener.local_addr().expect("forwarder address");
        let shared = Arc::new(Shared::default());
        let accept = {
            let shared = shared.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((client, _)) = listener.accept().await else {
                        continue;
                    };
                    shared.accepted.fetch_add(1, Ordering::SeqCst);
                    let Ok(server) = TcpStream::connect(&upstream).await else {
                        // No upstream: close the client, as a refused Redis would.
                        drop(client);
                        continue;
                    };
                    let _ = client.set_nodelay(true);
                    let _ = server.set_nodelay(true);
                    let lost = Arc::new(AtomicBool::new(false));
                    let (client_rx, client_tx) = client.into_split();
                    let (server_rx, server_tx) = server.into_split();
                    let up = tokio::spawn(pump(
                        client_rx,
                        server_tx,
                        shared.clone(),
                        lost.clone(),
                        false,
                    ));
                    let down = tokio::spawn(pump(server_rx, client_tx, shared.clone(), lost, true));
                    shared.keep(up);
                    shared.keep(down);
                }
            })
        };
        Self {
            addr,
            shared,
            accept,
        }
    }

    /// The URL to give a server: `original` with its host and port replaced
    /// by the forwarder's (credentials and database index kept).
    pub fn url_for(&self, original: &str) -> String {
        let (scheme, rest) = original.split_once("://").unwrap_or(("redis", original));
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, format!("/{p}")),
            None => (rest, String::new()),
        };
        let userinfo = authority
            .rsplit_once('@')
            .map(|(u, _)| format!("{u}@"))
            .unwrap_or_default();
        format!("{scheme}://{userinfo}{}{path}", self.addr)
    }

    /// Stop answering: drop every byte in both directions, close nothing.
    pub fn swallow(&self) {
        self.shared.swallow.store(true, Ordering::SeqCst);
    }

    /// Answer again, on connections that never lost a byte and on new ones.
    pub fn pass(&self) {
        self.shared.swallow.store(false, Ordering::SeqCst);
    }

    /// Hold the next chunk Redis sends for `delay`, then deliver it.
    pub fn delay_next_reply(&self, delay: Duration) {
        self.shared
            .delay_next_reply_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// How many client connections the forwarder has accepted.
    pub fn accepted(&self) -> u64 {
        self.shared.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for RedisBlackout {
    fn drop(&mut self) {
        self.accept.abort();
        if let Ok(tasks) = self.shared.tasks.lock() {
            for t in tasks.iter() {
                t.abort();
            }
        }
    }
}

/// One direction of one connection. `reply` marks the Redis-to-client side,
/// the one a delayed reply applies to.
async fn pump(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    shared: Arc<Shared>,
    lost: Arc<AtomicBool>,
    reply: bool,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                // The peer closed: pass the close on, unless this connection
                // stands for a node that is gone (a gone node says nothing).
                if !lost.load(Ordering::SeqCst) {
                    let _ = to.shutdown().await;
                }
                return;
            }
            Ok(n) => n,
        };
        if reply {
            let delay = shared.delay_next_reply_ms.swap(0, Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        if shared.swallow.load(Ordering::SeqCst) || lost.load(Ordering::SeqCst) {
            lost.store(true, Ordering::SeqCst);
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::host_port;

    #[test]
    fn host_port_reads_every_url_shape_the_lane_uses() {
        assert_eq!(host_port("redis://localhost:6394"), "localhost:6394");
        assert_eq!(host_port("redis://127.0.0.1:6379/0"), "127.0.0.1:6379");
        assert_eq!(
            host_port("redis://:secret@10.0.0.5:6380/2"),
            "10.0.0.5:6380"
        );
        assert_eq!(host_port("redis://redis"), "redis:6379");
    }
}
