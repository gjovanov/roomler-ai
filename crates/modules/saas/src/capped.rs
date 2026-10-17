// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P7 — a response body read with a ceiling.
//!
//! Every growth puller reads a server we do not run. An answer far bigger
//! than any real one is refused before it is all in memory: by its
//! `Content-Length` when it states one, else as the stream passes the cap.
//! Each client picks a cap several times its largest real answer (a page of
//! 100 GitHub releases measured 4.96 MB on 2026-10-17).

/// Why a capped read gave nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    /// The body is larger than the cap (in bytes).
    TooLarge(usize),
    Timeout,
    Transport(String),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(cap) => write!(f, "answer larger than {} KiB", cap / 1024),
            Self::Timeout => write!(f, "timed out reading the answer"),
            Self::Transport(e) => write!(f, "{e}"),
        }
    }
}

/// The whole body of `resp`, or [`BodyError::TooLarge`] past `cap` bytes.
pub async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, BodyError> {
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return Err(BodyError::TooLarge(cap));
    }
    let mut out = Vec::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            if e.is_timeout() {
                BodyError::Timeout
            } else {
                BodyError::Transport(e.without_url().to_string().chars().take(120).collect())
            }
        })?;
        let Some(chunk) = chunk else {
            return Ok(out);
        };
        if out.len() + chunk.len() > cap {
            return Err(BodyError::TooLarge(cap));
        }
        out.extend_from_slice(&chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, response::Response, routing::get};

    async fn serve() -> String {
        let router = Router::new()
            .route("/small", get(|| async { "hello" }))
            // A stated length past the cap: refused before the body is read.
            .route("/big", get(|| async { "x".repeat(4096) }))
            // No length (chunked): refused as the stream passes the cap.
            .route(
                "/stream",
                get(|| async {
                    let chunks = (0..8).map(|_| Ok::<_, std::io::Error>("y".repeat(1024)));
                    Response::new(Body::from_stream(futures::stream::iter(chunks)))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_body_past_its_cap_is_refused_with_or_without_a_length() {
        let base = serve().await;
        let http = reqwest::Client::new();
        let get = |p: &str| http.get(format!("{base}{p}")).send();
        let small = read_capped(get("/small").await.unwrap(), 1024).await;
        assert_eq!(small.unwrap(), b"hello");
        let big = read_capped(get("/big").await.unwrap(), 1024).await;
        assert_eq!(big, Err(BodyError::TooLarge(1024)));
        let stream = get("/stream").await.unwrap();
        assert_eq!(
            stream.content_length(),
            None,
            "the case under test: no length"
        );
        assert_eq!(
            read_capped(stream, 4096).await,
            Err(BodyError::TooLarge(4096))
        );
        let whole = read_capped(get("/stream").await.unwrap(), 8192).await;
        assert_eq!(whole.unwrap().len(), 8192);
        assert_eq!(
            BodyError::TooLarge(32 << 20).to_string(),
            "answer larger than 32768 KiB"
        );
    }
}
