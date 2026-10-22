// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-4-1 — AWS's Signature Version 4, as the daemon computes it: the
//! canonical request, the string to sign, the signing key and the signature.
//! Its caller is the session's egress proxy (P3b-4-2,
//! `hive::egress::swap::aws`), which checks a signature made with a
//! placeholder pair and signs the request again with the value (the FR-90
//! spec, §3e "P3b-4, AWS, as designed").
//!
//! | Step | Rule |
//! |---|---|
//! | the header | `AWS4-HMAC-SHA256 Credential=<id>/<date>/<region>/<service>/aws4_request, SignedHeaders=<a;b;…>, Signature=<64 hex>` ([`parse`]). Any other algorithm (SigV4A's `AWS4-ECDSA-P256-SHA256` among them) is no header this reads |
//! | the canonical request | the method; the path (S3's as sent, every other service's with its dot segments and empty segments gone and encoded once more); the query's pairs as sent, sorted; each signed header, its name lower-cased, its values joined by `,`, each value's white space runs made one space; the signed headers' names; the payload's hash ([`canonical_request`]) |
//! | the string to sign | the algorithm, the request's `X-Amz-Date`, the scope (`<date>/<region>/<service>/aws4_request`), and the canonical request's SHA-256 in hex ([`string_to_sign`]) |
//! | the signing key | HMAC-SHA256 from `AWS4` and the secret key, through the date, the region, the service and `aws4_request` ([`signing_key`]) |
//! | the signature | HMAC-SHA256 of the string to sign under the signing key, in hex ([`signature`]); two compared in constant time ([`same`]) |
//!
//! ⚠️ The canonical request is rebuilt from the request as the SDK sent it,
//! so its rules are the SDK's: botocore's for the path (S3's untouched, every
//! other service's normalized and quoted again with `/` and `~` kept) and for
//! the query (the pairs as they stand in the URL, sorted). Where they part
//! ways with another SDK's, a signature made with the placeholder does not
//! check, and the request is refused rather than sent.

use hmac::{Hmac, Mac};
use hyper::header::HeaderMap;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// The one algorithm this signs and checks.
pub(crate) const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// The scope's last part.
const TERMINATOR: &str = "aws4_request";

/// What an `Authorization` header SigV4 wrote names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signed {
    /// The access key's id.
    pub(crate) id: String,
    /// The scope's date, `YYYYMMDD`.
    pub(crate) date: String,
    pub(crate) region: String,
    pub(crate) service: String,
    /// The signed headers' names, lower-cased, as listed.
    pub(crate) headers: Vec<String>,
    /// 64 lower-case hex digits.
    pub(crate) signature: String,
}

impl Signed {
    /// `<date>/<region>/<service>/aws4_request`.
    pub(crate) fn scope(&self) -> String {
        format!(
            "{}/{}/{}/{TERMINATOR}",
            self.date, self.region, self.service
        )
    }

    /// The header again, with `id` and `signature` in place of its own.
    pub(crate) fn authorization(&self, id: &str, signature: &str) -> String {
        format!(
            "{ALGORITHM} Credential={id}/{}, SignedHeaders={}, Signature={signature}",
            self.scope(),
            self.headers.join(";")
        )
    }
}

/// Read an `Authorization` header's value as SigV4 wrote it; `Err` names
/// what is not.
pub(crate) fn parse(value: &str) -> Result<Signed, String> {
    let rest = value
        .strip_prefix(ALGORITHM)
        .filter(|r| r.starts_with(' '))
        .ok_or_else(|| format!("not {ALGORITHM}"))?;
    let (mut credential, mut headers, mut signature) = (None, None, None);
    for part in rest.split(',') {
        let part = part.trim();
        let (key, val) = part
            .split_once('=')
            .ok_or_else(|| format!("{part:?} is no key=value"))?;
        let slot = match key {
            "Credential" => &mut credential,
            "SignedHeaders" => &mut headers,
            "Signature" => &mut signature,
            other => return Err(format!("{other:?} is no part of a SigV4 header")),
        };
        if slot.replace(val).is_some() {
            return Err(format!("{key} named twice"));
        }
    }
    let (Some(credential), Some(headers), Some(signature)) = (credential, headers, signature)
    else {
        return Err("a SigV4 header names Credential, SignedHeaders and Signature".into());
    };
    let parts: Vec<&str> = credential.split('/').collect();
    let [id, date, region, service, TERMINATOR] = parts[..] else {
        return Err(format!(
            "Credential is <id>/<date>/<region>/<service>/{TERMINATOR}"
        ));
    };
    let plain = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    };
    if !plain(id) || !plain(region) || !plain(service) {
        return Err("Credential's id, region and service are plain names".into());
    }
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Credential's date is YYYYMMDD".into());
    }
    let headers: Vec<String> = headers.split(';').map(str::to_string).collect();
    if headers.iter().any(|h| {
        h.is_empty()
            || !h
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
    }) {
        return Err("SignedHeaders names lower-case headers, by `;`".into());
    }
    if signature.len() != 64
        || !signature
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Signature is 64 lower-case hex digits".into());
    }
    Ok(Signed {
        id: id.to_string(),
        date: date.to_string(),
        region: region.to_string(),
        service: service.to_string(),
        headers,
        signature: signature.to_string(),
    })
}

/// The canonical request, from the request as it was sent: `path` before
/// any `?`, `query` after it (empty for none), the headers it carries, the
/// ones signed, and the payload's hash. `Err` where a signed header is not
/// carried, or a value is not text.
pub(crate) fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &HeaderMap,
    signed: &[String],
    payload_hash: &str,
    service: &str,
) -> Result<String, String> {
    let mut lines = Vec::with_capacity(signed.len());
    for name in signed {
        let mut values = Vec::new();
        for value in headers.get_all(name.as_str()) {
            let text = value
                .to_str()
                .map_err(|_| format!("the signed header {name} is not text"))?;
            values.push(text.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        if values.is_empty() {
            return Err(format!("the signed header {name} is not carried"));
        }
        lines.push(format!("{name}:{}", values.join(",")));
    }
    Ok(format!(
        "{method}\n{}\n{}\n{}\n\n{}\n{payload_hash}",
        canonical_uri(path, service),
        canonical_query(query),
        lines.join("\n"),
        signed.join(";"),
    ))
}

/// The path as SigV4 reads it: S3's as sent, every other service's with its
/// dot and empty segments gone, and encoded once more, `/` and `~` kept.
fn canonical_uri(path: &str, service: &str) -> String {
    if service == "s3" {
        return if path.is_empty() {
            "/".into()
        } else {
            path.into()
        };
    }
    let normalized = normalize(path);
    let mut out = String::with_capacity(normalized.len());
    for b in normalized.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// RFC 3986's dot segments removed, and empty segments with them, as
/// botocore does: a leading `/` and a trailing one kept.
fn normalize(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    let mut kept: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                kept.pop();
            }
            s => kept.push(s),
        }
    }
    let first = if path.starts_with('/') { "/" } else { "" };
    let last = if path.ends_with('/') && !kept.is_empty() {
        "/"
    } else {
        ""
    };
    format!("{first}{}{last}", kept.join("/"))
}

/// The query's pairs as they stand in the URL, sorted by name then value; a
/// name with no `=` taken as `name=`.
fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The string to sign: the algorithm, the request's date and time
/// (`X-Amz-Date`), the scope, and the canonical request's hash.
pub(crate) fn string_to_sign(amz_date: &str, scope: &str, canonical: &str) -> String {
    format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    )
}

/// HMAC-SHA256 of `msg` under `key`.
fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// The signing key: `AWS4` and the secret key, through the date, the
/// region, the service and `aws4_request`.
pub(crate) fn signing_key(
    secret: &[u8],
    date: &str,
    region: &str,
    service: &str,
) -> Zeroizing<[u8; 32]> {
    let mut first = Zeroizing::new(Vec::with_capacity(4 + secret.len()));
    first.extend_from_slice(b"AWS4");
    first.extend_from_slice(secret);
    let k_date = Zeroizing::new(hmac(&first, date.as_bytes()));
    let k_region = Zeroizing::new(hmac(&*k_date, region.as_bytes()));
    let k_service = Zeroizing::new(hmac(&*k_region, service.as_bytes()));
    Zeroizing::new(hmac(&*k_service, TERMINATOR.as_bytes()))
}

/// The signature, in lower-case hex.
pub(crate) fn signature(key: &[u8; 32], string_to_sign: &str) -> String {
    hex::encode(hmac(key, string_to_sign.as_bytes()))
}

/// Whether two signatures are the same, compared in constant time.
pub(crate) fn same(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

#[cfg(test)]
mod tests;
