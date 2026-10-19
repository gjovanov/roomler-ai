// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5d-1 — the HTTP proxy's session: one client connection on the
//! endpoint, each of its requests sent on to one upstream origin with the
//! secret's credential added.
//!
//! | Step | What happens |
//! |---|---|
//! | a request | origin-form only (`/path?query`): `CONNECT`, an absolute or an authority form and `*` are refused, since the endpoint stands for one upstream and is no forward proxy |
//! | its headers | the client's own `Authorization` and `Proxy-Authorization` dropped, with every hop-by-hop header and those `Connection` names; `Host` made the upstream's; the secret's `Authorization` added, marked sensitive |
//! | the upstream | TCP, then TLS verified ([`super::tls`]), HTTP/1.1: dialled at the connection's first request, and again whenever the upstream has closed it |
//! | the response | streamed back, never buffered. A `Location` on the upstream's own origin is pointed back at the endpoint; one anywhere else goes back unchanged, and the client follows it with no value. `Alt-Svc` and `Strict-Transport-Security`, which would describe the upstream as if it were the endpoint, are dropped |
//!
//! ⚠️ The credential goes only in `Authorization`, in the form the call
//! named (`Basic` or `Bearer`). A header the session could name is a header
//! an upstream might echo back in a response the session reads.
//!
//! ⚠️ The proxy never follows a redirect itself. The client does, and only
//! one that stays on the upstream comes back through the endpoint.
//!
//! ⚠️ The value is on the wire here, inside the TLS the proxy verified, and
//! it passes through the HTTP stack's buffers, which are not zeroed: unlike
//! SCRAM, an `Authorization` header shows the upstream the value itself.

use std::convert::Infallible;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tracing::debug;
use zeroize::Zeroizing;

use super::tls;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type Body = BoxBody<Bytes, BoxError>;

/// How long the upstream may take to accept a connection and finish TLS.
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// How long a client has to send a request's headers.
const HEADER_DEADLINE: Duration = Duration::from_secs(30);
/// What a request that is not for a path on the upstream is told.
const NOT_A_PROXY: &str = "this endpoint stands for one upstream: send it a request for a path, as \
                           you would the upstream";

/// How the secret's value is presented: always in `Authorization`, in the
/// form the call named.
pub(crate) enum Credential {
    /// RFC 7617: the secret's `user` and `password`.
    Basic {
        user: Zeroizing<String>,
        password: Zeroizing<String>,
    },
    /// RFC 6750: the secret's `token`.
    Bearer(Zeroizing<String>),
}

impl Credential {
    /// `Authorization`'s value, marked sensitive: `None` for a value it
    /// cannot carry. A user with a colon, which Basic cannot tell from the
    /// password; a token that is empty, or holds a space, a control
    /// character or a byte that is not ASCII.
    pub(crate) fn header(&self) -> Option<HeaderValue> {
        let raw = match self {
            Self::Basic { user, password } => {
                if user.contains(':') {
                    return None;
                }
                let pair = Zeroizing::new(format!("{}:{}", user.as_str(), password.as_str()));
                let encoded = Zeroizing::new(
                    base64::engine::general_purpose::STANDARD.encode(pair.as_bytes()),
                );
                Zeroizing::new(format!("Basic {}", encoded.as_str()))
            }
            Self::Bearer(token) => {
                if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
                    return None;
                }
                Zeroizing::new(format!("Bearer {}", token.as_str()))
            }
        };
        let mut value = HeaderValue::from_str(&raw).ok()?;
        value.set_sensitive(true);
        Some(value)
    }
}

/// Where the proxy sends a connection's requests, and with what.
pub(crate) struct Upstream {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) credential: Credential,
    pub(crate) tls: tokio_rustls::TlsConnector,
    /// The endpoint's own origin, such as `http://127.0.0.1:<port>`: where a
    /// redirect to the upstream is pointed back to.
    pub(crate) front: String,
}

impl Upstream {
    /// The upstream's authority, as `Host` names it: an IPv6 address in
    /// brackets, and the port only where it is not 443.
    fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 443 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// Serve one client connection until it ends: each request sent on to the
/// upstream with the credential, each response streamed back.
pub(crate) async fn serve<C>(client: C, up: &Upstream) -> Result<(), String>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let host = HeaderValue::from_str(&up.authority()).map_err(|e| e.to_string())?;
    let authorization = up.credential.header();
    // The connection's upstream leg, between its requests: hyper serves a
    // connection's requests one at a time, so it is taken out while one is
    // sent and put back with its response.
    let leg = Mutex::new(None::<Leg>);
    let service = hyper::service::service_fn(|req| {
        let (host, authorization, leg) = (&host, authorization.as_ref(), &leg);
        async move { Ok::<_, Infallible>(forward(req, up, host, authorization, leg).await) }
    });
    hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_DEADLINE)
        .serve_connection(TokioIo::new(client), service)
        .await
        .map_err(|e| e.to_string())
}

/// Tell a client why it is not served: its first request answered `503`
/// with the reason, and the connection closed.
pub(crate) async fn refuse<C>(client: C, why: &str) -> Result<(), String>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let service = hyper::service::service_fn(|_: Request<Incoming>| async move {
        Ok::<_, Infallible>(answer(StatusCode::SERVICE_UNAVAILABLE, why))
    });
    hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_DEADLINE)
        .keep_alive(false)
        .serve_connection(TokioIo::new(client), service)
        .await
        .map_err(|e| e.to_string())
}

/// One request, sent on and answered; or refused, with why.
async fn forward(
    req: Request<Incoming>,
    up: &Upstream,
    host: &HeaderValue,
    authorization: Option<&HeaderValue>,
    leg: &Mutex<Option<Leg>>,
) -> Response<Body> {
    let Some(authorization) = authorization else {
        return answer(
            StatusCode::BAD_GATEWAY,
            "the secret's value is not one an Authorization header can carry",
        );
    };
    let req = match outbound(req, host, authorization) {
        Ok(req) => req,
        Err(status) => return answer(status, NOT_A_PROXY),
    };
    let held = leg.lock().unwrap_or_else(|e| e.into_inner()).take();
    match send(held, req, up).await {
        Ok((resp, kept)) => {
            *leg.lock().unwrap_or_else(|e| e.into_inner()) = Some(kept);
            inbound(resp, up)
        }
        Err(why) => answer(StatusCode::BAD_GATEWAY, &why),
    }
}

/// The request as the upstream gets it: or the status that refuses one
/// that is not for a path on the upstream.
fn outbound(
    req: Request<Incoming>,
    host: &HeaderValue,
    authorization: &HeaderValue,
) -> Result<Request<Incoming>, StatusCode> {
    if req.method() == Method::CONNECT {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    let uri = req.uri();
    let path = match (uri.scheme(), uri.authority(), uri.path_and_query()) {
        (None, None, Some(pq)) if pq.as_str().starts_with('/') => pq.clone(),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let (mut parts, body) = req.into_parts();
    parts.uri = Uri::from(path);
    parts.version = hyper::Version::HTTP_11;
    parts.extensions = Default::default();
    hop_by_hop(&mut parts.headers);
    // The client's own credentials never leave: the endpoint is its
    // credential, and the secret's goes in their place.
    for name in [header::AUTHORIZATION, header::HOST, header::EXPECT] {
        parts.headers.remove(name);
    }
    parts.headers.insert(header::HOST, host.clone());
    parts
        .headers
        .insert(header::AUTHORIZATION, authorization.clone());
    Ok(Request::from_parts(parts, body))
}

/// The upstream's response as the client gets it.
fn inbound(resp: Response<Incoming>, up: &Upstream) -> Response<Body> {
    let (mut parts, body) = resp.into_parts();
    hop_by_hop(&mut parts.headers);
    parts.headers.remove(header::ALT_SVC);
    parts.headers.remove(header::STRICT_TRANSPORT_SECURITY);
    if let Some(back) = parts
        .headers
        .get(header::LOCATION)
        .and_then(|l| point_back(l, up))
    {
        parts.headers.insert(header::LOCATION, back);
    }
    Response::from_parts(parts, body.map_err(BoxError::from).boxed())
}

/// Drop the hop-by-hop headers, and those `Connection` names: they are one
/// leg's, never the next one's (RFC 9110 §7.6.1). `Proxy-Authorization`
/// among them, so a client's never leaves.
fn hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}

/// A redirect to the upstream's own origin, pointed back at the endpoint so
/// it stays authenticated: `None` for one anywhere else, which the client
/// follows with no value. The origin is matched whole (`https`, the host,
/// the port), never as a prefix: `https://host.elsewhere` is not `https://host`.
fn point_back(location: &HeaderValue, up: &Upstream) -> Option<HeaderValue> {
    let raw = location.to_str().ok()?;
    let (head, fragment) = raw.split_at(raw.find('#').unwrap_or(raw.len()));
    let uri: Uri = head.parse().ok()?;
    let authority = uri.authority()?;
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let ours = uri
        .scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
        && !authority.as_str().contains('@')
        && host.eq_ignore_ascii_case(&up.host)
        && authority.port_u16().unwrap_or(443) == up.port;
    if !ours {
        return None;
    }
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let slash = if path.starts_with('/') { "" } else { "/" };
    HeaderValue::from_str(&format!("{}{slash}{path}{fragment}", up.front)).ok()
}

/// The endpoint's own answer: `status`, and why, as plain text.
fn answer(status: StatusCode, why: &str) -> Response<Body> {
    let body = Full::new(Bytes::from(format!("{why}\n")))
        .map_err(|never| match never {})
        .boxed();
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// One client connection's upstream leg.
struct Leg {
    send: SendRequest<Incoming>,
    /// The connection's own task, ended with the leg.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Leg {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Send `req` on `held`, or on a new leg where there is none or the
/// upstream has closed it; once more on a new one if the upstream closed it
/// before the request went out. Answers the response and the leg to keep.
async fn send(
    mut held: Option<Leg>,
    mut req: Request<Incoming>,
    up: &Upstream,
) -> Result<(Response<Incoming>, Leg), String> {
    for _ in 0..2 {
        let mut leg = match held.take() {
            Some(mut leg) => match leg.send.ready().await {
                Ok(()) => leg,
                Err(_) => dial(up).await?,
            },
            None => dial(up).await?,
        };
        match leg.send.try_send_request(req).await {
            Ok(resp) => return Ok((resp, leg)),
            Err(mut e) => match e.take_message() {
                // Never written: the upstream closed the leg first.
                Some(back) => req = back,
                None => return Err(format!("{}: {}", up.authority(), e.into_error())),
            },
        }
    }
    Err(format!(
        "{} closed every connection before the request went out",
        up.authority()
    ))
}

/// A new leg: TCP, TLS verified, an HTTP/1.1 handshake.
async fn dial(up: &Upstream) -> Result<Leg, String> {
    let name = tls::server_name(&up.host)?;
    let connected = async {
        let tcp = TcpStream::connect((up.host.as_str(), up.port))
            .await
            .map_err(|e| format!("{}: {e}", up.authority()))?;
        up.tls
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS to {}: {e}", up.host))
    };
    let stream = tokio::time::timeout(CONNECT_DEADLINE, connected)
        .await
        .map_err(|_| {
            format!(
                "{} did not answer within {CONNECT_DEADLINE:?}",
                up.authority()
            )
        })??;
    let (send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("{}: {e}", up.authority()))?;
    let task = tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!(%e, "vault: an HTTP proxy's upstream connection ended with an error");
        }
    });
    Ok(Leg { send, task })
}

#[cfg(test)]
mod tests;
