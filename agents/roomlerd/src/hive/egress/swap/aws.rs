// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-4-2 — AWS's signature, signed again. A request the session's
//! `aws` signed with a placeholder pair is checked with the secret key's
//! placeholder, then signed again with the value (the FR-90 spec, §3e
//! "P3b-4, AWS, as designed"; SigV4 itself is [`crate::vault_proxy::sigv4`]).
//!
//! | Step | Rule |
//! |---|---|
//! | which requests | one whose `Authorization` is AWS's (its scheme starts `AWS`) and names a placeholder this audience holds. Any other goes on as it came, and the swap passes AWS's by |
//! | its form | SigV4 (`AWS4-HMAC-SHA256`) alone, `Credential=` naming an `access_key_id` placeholder, the request's one `Authorization`. SigV4A, or any other naming a placeholder, is refused |
//! | the pair (decision 33) | the same secret's `secret_access_key` placeholder toward this audience is the key; its `session_token` placeholder, where `X-Amz-Security-Token` carries it, is the token |
//! | the body's hash | `x-amz-content-sha256` where the request carries one, the body going on as it came, signed chunks refused; where it carries none, the body read whole, up to [`MAX_BODY`], and hashed, at most [`super::super::MAX_BODIES`] at once a run |
//! | the check | the canonical request rebuilt from the request as it came, signed with the key's placeholder, gives the request's signature |
//! | signed again | the token's value in `X-Amz-Security-Token`, `Credential=` given the access key's value, the signature made with the key's. Every other signed header goes on as it came: one that would not (one leg's, hop-by-hop) refuses the request |
//!
//! ⚠️ The check is not the gate. A session that holds the placeholder pair
//! signs what it likes, as it could with the key. The check makes a
//! canonical request the proxy and the SDK build differently a refusal with
//! a reason, rather than AWS's `SignatureDoesNotMatch` after the value
//! signed it.
//!
//! ⚠️ The secret key's value goes nowhere but into the HMAC: `Credential=`
//! takes the value of an `access_key_id` placeholder alone, so a session that
//! wrote the secret key's placeholder there is refused, never sent its value.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Body as HttpBody, Frame, Incoming, SizeHint};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::http::request::Parts;
use hyper::{Request, StatusCode};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::debug;

use super::super::super::placeholder::{self, Bound};
use super::super::Values;
use super::{Conn, aws_signed};
use crate::vault_proxy::http::{Body, BoxError};
use crate::vault_proxy::sigv4::{self, Signed};

/// The most a body is read whole to hash it, where the request names no
/// `x-amz-content-sha256`.
pub(super) const MAX_BODY: usize = 8 * 1024 * 1024;

/// The fields a pair is found by (decision 33): the names P3-6e gives AWS's.
const ACCESS_KEY: &str = "access_key_id";
const SECRET_KEY: &str = "secret_access_key";
const SESSION_TOKEN: &str = "session_token";
/// The header AWS reads a session token from.
const TOKEN_HEADER: &str = "x-amz-security-token";

/// A request refused, never sent: its status, and why.
type Refused = (StatusCode, String);

fn refused(status: StatusCode, why: impl std::fmt::Display) -> Refused {
    (status, format!("roomler egress proxy: {why}"))
}

/// Whether `b` is a key that signs and is never sent: an AWS secret key's
/// placeholder, which the swap passes by, wherever a session writes it.
pub(super) fn signs_only(b: &Bound) -> bool {
    b.field == SECRET_KEY
}

/// Whether `headers` carry a SigV4 `Authorization` that signs `name`.
pub(super) fn signs(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| sigv4::parse(v).ok())
        .is_some_and(|s| s.headers.iter().any(|h| h == name))
}

/// `req` with its body boxed, as it came.
fn boxed(req: Request<Incoming>) -> Request<Body> {
    req.map(|b| b.map_err(BoxError::from).boxed())
}

/// `req` as it goes on: signed again with the value where its
/// `Authorization` is AWS's and names a placeholder this audience holds, and
/// as it came otherwise. Err where it names one and is not signed again: the
/// request is then never sent.
pub(super) async fn resign(req: Request<Incoming>, conn: &Conn) -> Result<Request<Body>, Refused> {
    let written: Vec<&HeaderValue> = req
        .headers()
        .get_all(header::AUTHORIZATION)
        .iter()
        .collect();
    let aws: Vec<&str> = written
        .iter()
        .filter(|v| aws_signed(v.as_bytes()))
        .filter_map(|v| v.to_str().ok())
        .collect();
    if aws.is_empty() {
        return Ok(boxed(req));
    }
    let bound = placeholder::bound(conn.run.0, conn.run.1, &conn.target.host, conn.target.port);
    let Some(auth) = aws
        .iter()
        .find(|a| bound.iter().any(|b| a.contains(b.token.as_str())))
    else {
        // A signature naming none of this audience's placeholders is the
        // session's own: it goes on as it came.
        return Ok(boxed(req));
    };
    if written.len() > 1 {
        return Err(refused(
            StatusCode::BAD_REQUEST,
            "an AWS signature naming a placeholder is signed again only as the request's one \
             Authorization",
        ));
    }
    let signed = sigv4::parse(auth).map_err(|why| {
        if auth.split(' ').next() == Some(sigv4::ALGORITHM) {
            refused(
                StatusCode::BAD_REQUEST,
                format!("an AWS signature naming a placeholder does not read: {why}"),
            )
        } else {
            refused(
                StatusCode::NOT_IMPLEMENTED,
                format!(
                    "an AWS signature naming a placeholder is signed again as SigV4 ({}) alone: \
                     SigV4A, or any other, is not sent",
                    sigv4::ALGORITHM
                ),
            )
        }
    })?;
    let pair = pair(&bound, &signed)?;
    // Nothing a signature covers is changed but what is signed again, so a
    // signed header that does not go on refuses the request.
    if let Some(gone) = signed
        .headers
        .iter()
        .find(|h| !req.headers().contains_key(h.as_str()))
    {
        return Err(refused(
            StatusCode::NOT_IMPLEMENTED,
            format!(
                "the signed header {gone} does not go on (one that is one leg's, hop-by-hop, \
                 never does), so the request is not sent"
            ),
        ));
    }
    let amz_date = req
        .headers()
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| {
            refused(
                StatusCode::BAD_REQUEST,
                "an AWS signature naming a placeholder is signed again where the request \
                 carries X-Amz-Date",
            )
        })?;
    let (mut parts, body) = req.into_parts();
    let (hash, body) = payload(&parts.headers, body, &conn.state.bodies).await?;
    // The check: the request as it came, signed with the key's placeholder,
    // gives its signature.
    let sts = sigv4::string_to_sign(
        &amz_date,
        &signed.scope(),
        &canonical(&parts, &signed, &hash)?,
    );
    let key = pair
        .keys
        .iter()
        .find(|k| {
            let as_placeholder = sigv4::signing_key(
                k.token.as_bytes(),
                &signed.date,
                &signed.region,
                &signed.service,
            );
            sigv4::same(&sigv4::signature(&as_placeholder, &sts), &signed.signature)
        })
        .ok_or_else(|| {
            refused(
                StatusCode::FORBIDDEN,
                "the request's AWS signature does not check with its placeholder pair, so it is \
                 not signed with the value: sign with the placeholders vault_placeholder set, as \
                 botocore signs",
            )
        })?;
    let values = &*conn.seams.values;
    let gone = || {
        refused(
            StatusCode::FORBIDDEN,
            "a placeholder's lease has ended, so the request is not signed: ask \
             vault_placeholder again",
        )
    };
    // The token's placeholder, where the request carries it, swapped for its
    // value.
    let token = parts
        .headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|sent| pair.tokens.iter().find(|t| t.token == sent));
    if let Some(token) = token {
        let value = Values::value(values, &token.lease_id).ok_or_else(gone)?;
        let mut header = HeaderValue::from_bytes(&value).map_err(|_| {
            refused(
                StatusCode::BAD_GATEWAY,
                "the session token's value is not one a header can carry: the request was not \
                 sent",
            )
        })?;
        header.set_sensitive(true);
        parts.headers.insert(TOKEN_HEADER, header);
    }
    let id = Values::value(values, &pair.id.lease_id).ok_or_else(gone)?;
    let id = std::str::from_utf8(&id)
        .ok()
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_string)
        .ok_or_else(|| {
            refused(
                StatusCode::BAD_GATEWAY,
                format!(
                    "{}'s {ACCESS_KEY} is no access key's id: the request was not sent",
                    pair.id.secret
                ),
            )
        })?;
    let secret = Values::value(values, &key.lease_id).ok_or_else(gone)?;
    // Signed again, the token's value in where it is signed.
    let sts = sigv4::string_to_sign(
        &amz_date,
        &signed.scope(),
        &canonical(&parts, &signed, &hash)?,
    );
    let with_value = sigv4::signing_key(&secret, &signed.date, &signed.region, &signed.service);
    let mut header =
        HeaderValue::from_str(&signed.authorization(&id, &sigv4::signature(&with_value, &sts)))
            .map_err(|_| {
                refused(
                    StatusCode::BAD_GATEWAY,
                    "the signature signed again is not one a header can carry",
                )
            })?;
    header.set_sensitive(true);
    parts.headers.insert(header::AUTHORIZATION, header);
    debug!(
        session = %conn.run.0, fence = conn.run.1, target = %conn.target.shown,
        access_key = %pair.id.lease_id, secret_key = %key.lease_id, token = token.is_some(),
        "hive: an AWS signature signed again with the value"
    );
    Ok(Request::from_parts(parts, body))
}

/// The placeholders a signature is made with: the access key's, the secret
/// keys' it may be signed with, the session tokens' it may carry.
struct Pair<'a> {
    id: &'a Bound,
    keys: Vec<&'a Bound>,
    tokens: Vec<&'a Bound>,
}

/// `signed`'s pair among `bound` (decision 33): `Credential=` names an
/// access key's placeholder, and the same secret's fields are its key and
/// token.
fn pair<'a>(bound: &'a [Bound], signed: &Signed) -> Result<Pair<'a>, Refused> {
    let id = bound.iter().find(|b| b.token == signed.id).ok_or_else(|| {
        refused(
            StatusCode::FORBIDDEN,
            format!(
                "the AWS signature names a placeholder, but not in Credential, where an \
                 {ACCESS_KEY}'s goes: the request was not sent"
            ),
        )
    })?;
    if id.field != ACCESS_KEY {
        return Err(refused(
            StatusCode::FORBIDDEN,
            format!(
                "Credential names {}'s {} placeholder, where an {ACCESS_KEY}'s goes: the request \
                 was not sent",
                id.secret, id.field
            ),
        ));
    }
    let of = |field: &str| -> Vec<&'a Bound> {
        bound
            .iter()
            .filter(|b| b.secret == id.secret && b.field == field)
            .collect()
    };
    let keys = of(SECRET_KEY);
    if keys.is_empty() {
        return Err(refused(
            StatusCode::FORBIDDEN,
            format!(
                "{} has no {SECRET_KEY} placeholder toward this host, to sign with: set it in \
                 the vault_placeholder call that sets its {ACCESS_KEY}",
                id.secret
            ),
        ));
    }
    Ok(Pair {
        id,
        keys,
        tokens: of(SESSION_TOKEN),
    })
}

/// The canonical request the signature covers, from the request as it
/// stands.
fn canonical(parts: &Parts, signed: &Signed, hash: &str) -> Result<String, Refused> {
    sigv4::canonical_request(
        parts.method.as_str(),
        parts.uri.path(),
        parts.uri.query().unwrap_or(""),
        &parts.headers,
        &signed.headers,
        hash,
        &signed.service,
    )
    .map_err(|why| {
        refused(
            StatusCode::BAD_REQUEST,
            format!("an AWS signature's request does not read: {why}"),
        )
    })
}

/// The payload's hash the signature covers, and the body to send: the value
/// of `x-amz-content-sha256` where the request carries one, the body going
/// on as it came; else the body read whole, up to [`MAX_BODY`], under one of
/// `bodies`' permits, and hashed.
async fn payload(
    headers: &HeaderMap,
    body: Incoming,
    bodies: &Arc<Semaphore>,
) -> Result<(String, Body), Refused> {
    if let Some(declared) = headers.get("x-amz-content-sha256") {
        let declared = declared
            .to_str()
            .map_err(|_| refused(StatusCode::BAD_REQUEST, "x-amz-content-sha256 is not text"))?;
        // Signed chunks, by HMAC or ECDSA, with a trailer or none: each
        // chunk's signature chains from the header's.
        if declared.starts_with("STREAMING-AWS4-") {
            return Err(refused(
                StatusCode::NOT_IMPLEMENTED,
                format!(
                    "a body in signed chunks ({declared}) is not signed again: each chunk's \
                     signature chains from the header's"
                ),
            ));
        }
        return Ok((declared.to_string(), body.map_err(BoxError::from).boxed()));
    }
    let too_large = || {
        refused(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "a body past {} MiB with no x-amz-content-sha256 is not read whole to sign again",
                MAX_BODY >> 20
            ),
        )
    };
    if HttpBody::size_hint(&body).lower() > MAX_BODY as u64 {
        return Err(too_large());
    }
    let permit = bodies.clone().acquire_owned().await.map_err(|_| {
        refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "the run's egress proxy has ended",
        )
    })?;
    let collected = Limited::new(body, MAX_BODY).collect().await.map_err(|e| {
        if e.downcast_ref::<LengthLimitError>().is_some() {
            too_large()
        } else {
            refused(
                StatusCode::BAD_REQUEST,
                format!("the request's body did not read: {e}"),
            )
        }
    })?;
    let bytes = collected.to_bytes();
    let hash = hex::encode(Sha256::digest(&bytes));
    let held = Held {
        body: Full::new(bytes),
        _permit: permit,
    };
    Ok((hash, held.boxed()))
}

/// A body read whole, and the permit it was read under, which goes when it
/// does: sent, or dropped.
struct Held {
    body: Full<Bytes>,
    _permit: OwnedSemaphorePermit,
}

impl HttpBody for Held {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        Pin::new(&mut self.get_mut().body)
            .poll_frame(cx)
            .map_err(|never| match never {})
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
