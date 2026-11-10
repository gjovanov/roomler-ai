// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3432 — the wizard's HTTP client, and the two failures it raises as
//! types of its own, so that the wizard can tell a person which one it was.
//!
//! Every request the wizard sends a server is built from [`builder`]: reqwest's
//! default client with one change. Names are resolved by [`Resolver`], which
//! does what reqwest's default resolver does (the system's `getaddrinfo`, on a
//! blocking thread) and fails with [`NameNotResolved`]. reqwest hands its own
//! resolver's error over inside hyper-util's `ConnectError("dns error")`, a type
//! no other crate can name, so without this a name that does not resolve could
//! be told from a refused connection only by reading the message. An IP literal
//! is never resolved, and behind a proxy only the proxy's own name is resolved
//! here, as before.
//!
//! [`HttpStatus`] is a server's answer that is not a success, so that the
//! answer is a type too: its status, the request and what the server said.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// A client builder that resolves names with [`Resolver`]. Each caller adds
/// its own User-Agent and timeout, as it did before.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().dns_resolver(Arc::new(Resolver))
}

/// A name the system's resolver did not turn into an address.
#[derive(Debug)]
pub struct NameNotResolved {
    /// The name, as the request carried it.
    pub name: String,
    /// What the resolver said.
    pub source: std::io::Error,
}

impl fmt::Display for NameNotResolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the name {} did not resolve", self.name)
    }
}

impl std::error::Error for NameNotResolved {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The wizard's name resolution: `getaddrinfo` through
/// `tokio::net::lookup_host`, which is what hyper-util's `GaiResolver` (the
/// one reqwest uses by default) runs, with a failure of our own type.
#[derive(Debug, Default, Clone, Copy)]
pub struct Resolver;

/// What reqwest's resolver hands back on a failure.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let name = name.as_str().to_string();
        Box::pin(async move {
            // Bound first: the lookup borrows `name`, which the failure keeps.
            let looked = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map(|found| found.collect::<Vec<SocketAddr>>());
            match looked {
                Ok(found) if !found.is_empty() => Ok(Box::new(found.into_iter()) as Addrs),
                Ok(_) => {
                    let source = std::io::Error::new(std::io::ErrorKind::NotFound, "no address");
                    Err(Box::new(NameNotResolved { name, source }) as BoxError)
                }
                Err(source) => Err(Box::new(NameNotResolved { name, source }) as BoxError),
            }
        })
    }
}

/// A server answered a request with a status that is not a success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpStatus {
    /// `GET` or `POST`.
    pub method: &'static str,
    pub url: String,
    pub status: u16,
    /// What the server said in its body, trimmed and cut short; empty when it
    /// said nothing.
    pub said: String,
}

/// How much of a refusal's body is kept: enough for the server's own
/// sentence, never a whole HTML page.
const SAID_MAX: usize = 300;

impl HttpStatus {
    /// Read the rest of `resp` into the error. Call only for a status that is
    /// not a success.
    pub async fn of(method: &'static str, resp: reqwest::Response) -> HttpStatus {
        let url = resp.url().to_string();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        HttpStatus {
            method,
            url,
            status,
            said: said_of(&body),
        }
    }

    /// The status's reason phrase, when it has one.
    pub fn reason(&self) -> Option<&'static str> {
        reqwest::StatusCode::from_u16(self.status)
            .ok()
            .and_then(|s| s.canonical_reason())
    }
}

/// The first `SAID_MAX` characters of `body`, whitespace folded.
fn said_of(body: &str) -> String {
    let folded = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match folded.char_indices().nth(SAID_MAX) {
        Some((cut, _)) => format!("{}…", &folded[..cut]),
        None => folded,
    }
}

impl fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} answered {}", self.method, self.url, self.status)?;
        if let Some(reason) = self.reason() {
            write!(f, " {reason}")?;
        }
        if !self.said.is_empty() {
            write!(f, ": {}", self.said)?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpStatus {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_reads_its_request_and_what_the_server_said() {
        let s = HttpStatus {
            method: "GET",
            url: "https://roomler.ai/api/x".into(),
            status: 503,
            said: String::new(),
        };
        assert_eq!(
            s.to_string(),
            "GET https://roomler.ai/api/x answered 503 Service Unavailable"
        );
        let s = HttpStatus {
            said: "Token expired".into(),
            status: 401,
            ..s
        };
        assert_eq!(
            s.to_string(),
            "GET https://roomler.ai/api/x answered 401 Unauthorized: Token expired"
        );
    }

    #[test]
    fn what_a_server_said_is_folded_and_cut_short() {
        assert_eq!(said_of("  a\n\n b\tc  "), "a b c");
        let long = "x".repeat(SAID_MAX + 50);
        let said = said_of(&long);
        assert_eq!(said.chars().count(), SAID_MAX + 1);
        assert!(said.ends_with('…'));
    }

    /// A name that cannot resolve (RFC 6761 keeps `.invalid` out of every
    /// zone) fails as our type, through a real request on the wizard's client.
    #[tokio::test]
    async fn a_name_that_does_not_resolve_fails_as_our_type() {
        let client = builder().build().unwrap();
        let err = client
            .get("http://roomler-setup-test.invalid/")
            .send()
            .await
            .expect_err("a .invalid name never resolves");
        let found = std::iter::successors(Some(&err as &(dyn std::error::Error + 'static)), |e| {
            e.source()
        })
        .find_map(|e| e.downcast_ref::<NameNotResolved>());
        let found = found.expect("NameNotResolved in the chain");
        assert_eq!(found.name, "roomler-setup-test.invalid");
    }
}
