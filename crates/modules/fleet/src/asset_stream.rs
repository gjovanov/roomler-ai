// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 C2b — the client a release-asset proxy streams through.
//!
//! Three routes hand a person a GitHub release asset as the bytes arrive
//! (`Body::from_stream`), so the upstream body is read at the person's own pace:
//! `/api/setup/{platform}` (the wizard; since FR-129 C2 the ~16 MB EXE),
//! `/api/agent/installer/{flavour}` (a ~22.5 MB MSI) and
//! `/api/tunnel/installer/{platform}`. Each built its client with reqwest's
//! `timeout(60 s)`, which is a TOTAL deadline — "from when the request starts
//! connecting until the response body has finished" — so every download that
//! took longer than a minute was cut mid-body: the MSI below about 375 KB/s,
//! the EXE below about 270 KB/s. That is a hotel, a phone or a throttled
//! corporate link, exactly where an install has to work.
//!
//! So the bounds are a connect deadline and an idle bound on each read
//! (reqwest's `read_timeout`, which runs only while the body is being read and
//! resets with every chunk), and no total: a slow reader is never cut, an
//! upstream that stops sending is, after a minute of silence. The release-list
//! fetch (`releases.rs`, `FETCH_TIMEOUT`) reads a small JSON body at once and
//! keeps its total deadline.

use std::time::Duration;

use roomler_core::ApiError;

/// How long the TCP + TLS connect to the asset host may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one read may wait for the next bytes from the asset host.
pub const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The client every asset proxy streams through.
pub fn client() -> Result<reqwest::Client, ApiError> {
    client_with(CONNECT_TIMEOUT, READ_IDLE_TIMEOUT)
}

/// [`client`] with its two bounds as arguments, so a test can shrink them.
/// Deliberately no `.timeout(…)`: a total deadline cuts a slow reader.
fn client_with(connect: Duration, read_idle: Duration) -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .user_agent(concat!("roomler-ai-api/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(connect)
        .read_timeout(read_idle)
        .build()
        .map_err(|e| ApiError::Internal(format!("reqwest client build: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const BODY: usize = 256 * 1024;

    /// A local stand-in for the asset host: one HTTP/1.1 response of `BODY`
    /// bytes with a Content-Length, the first half at once, then `pause`, then
    /// the rest. Returns the URL.
    async fn upstream(pause: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {BODY}\r\nConnection: close\r\n\r\n"
            );
            let body = vec![7u8; BODY];
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body[..BODY / 2]).await;
            tokio::time::sleep(pause).await;
            let _ = sock.write_all(&body[BODY / 2..]).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}/asset")
    }

    /// Read the whole body the way a proxy does (`bytes_stream`), stopping
    /// for `reader_pause` after the first chunk — a person on a slow link, whom
    /// the proxy's response makes the upstream wait for. Ok(bytes) or the
    /// error the stream ended with.
    async fn read_slowly(
        client: &reqwest::Client,
        url: &str,
        reader_pause: Duration,
    ) -> Result<usize, reqwest::Error> {
        let mut stream = client.get(url).send().await?.bytes_stream();
        let mut got = 0;
        let mut first = true;
        while let Some(chunk) = stream.next().await {
            got += chunk?.len();
            if first {
                first = false;
                tokio::time::sleep(reader_pause).await;
            }
        }
        Ok(got)
    }

    /// The defect and the fix side by side, with the bounds shrunk to 400 ms:
    /// a reader that stops for 1.2 s mid-body is cut by a total deadline
    /// (what every proxy had: `.timeout(60 s)`), and reads the whole body
    /// through `client_with`, whose bounds only run while a read waits.
    #[tokio::test]
    async fn a_slow_reader_is_not_cut() {
        let bound = Duration::from_millis(400);
        let pause = Duration::from_millis(1200);

        let total_deadline = reqwest::Client::builder().timeout(bound).build().unwrap();
        let url = upstream(Duration::ZERO).await;
        let cut = read_slowly(&total_deadline, &url, pause).await;
        assert!(
            cut.as_ref().is_err_and(|e| e.is_timeout()),
            "a total deadline must cut the body (it is the defect): {cut:?}"
        );

        let fixed = client_with(bound, bound).unwrap();
        let url = upstream(Duration::ZERO).await;
        assert_eq!(read_slowly(&fixed, &url, pause).await.unwrap(), BODY);
    }

    /// The idle bound still holds: an upstream that goes silent mid-body for
    /// longer than it ends the download with a timeout, rather than holding
    /// the request open for good.
    #[tokio::test]
    async fn a_silent_upstream_is_cut_by_the_idle_bound() {
        let bound = Duration::from_millis(400);
        let fixed = client_with(bound, bound).unwrap();
        let url = upstream(Duration::from_secs(3)).await;
        let started = std::time::Instant::now();
        let cut = read_slowly(&fixed, &url, Duration::ZERO).await;
        assert!(cut.as_ref().is_err_and(|e| e.is_timeout()), "{cut:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cut by the idle bound, not by the upstream's own end: {:?}",
            started.elapsed()
        );
    }

    /// The production client is the tested shape with the production bounds.
    #[test]
    fn the_production_bounds() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));
        assert_eq!(READ_IDLE_TIMEOUT, Duration::from_secs(60));
        assert!(client().is_ok());
    }
}
