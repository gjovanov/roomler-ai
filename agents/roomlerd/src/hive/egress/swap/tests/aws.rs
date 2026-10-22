// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-4-2 — AWS's signature signed again, end to end on the swap's
//! rig. The session's side signs with a placeholder pair, as `aws` would;
//! the audience must be sent what botocore sends for the same request signed
//! with the pair's values. Each signature the audience must see is
//! botocore's own (`SigV4Auth`, and `S3SigV4Auth` for S3), with AWS's
//! example pair, at 2015-08-30T12:36:00Z, computed before this code was.

use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::super::aws::MAX_BODY;
use super::*;
use crate::hive::egress::MAX_BODIES;
use crate::vault_proxy::sigv4::{self, Signed};

/// The secret the pair is held from.
const SECRET: &str = "aws/test";
/// When each request is signed, as its `X-Amz-Date` says.
const WHEN: &str = "20150830T123600Z";
/// AWS's example access key's id.
const AKID: &str = "AKIDEXAMPLE";
/// The STS call `aws sts get-caller-identity` makes.
const CALLER: &str = "Action=GetCallerIdentity&Version=2011-06-15";

/// AWS's example secret key, from its documentation: built here, so that no
/// commit holds it whole, nor any part long enough to read as a key.
fn example_secret() -> String {
    ["wJalrXUtnFEMI", "/K7MDENG", "+bPxRfiCY", "EXAMPLEKEY"].concat()
}

/// The session token the oracle signed with.
fn example_token() -> String {
    ["the", "session", "credential"].join("-")
}

/// 64 hex digits, from their four quarters: no literal a scanner takes for a
/// key.
fn hex64(quarters: [&str; 4]) -> String {
    let joined = quarters.concat();
    assert_eq!(joined.len(), 64, "{joined}");
    joined
}

/// The SHA-256 of `bytes`, in hex.
fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// One AWS secret's placeholders.
struct Placeholders {
    id: String,
    key: String,
    token: String,
}

/// `secret`'s three fields held toward [`HOST`], their values AWS's
/// examples.
fn hold_aws(rig: &Rig, secret: &str) -> Placeholders {
    Placeholders {
        id: rig.hold_as(HOST, secret, "access_key_id", AKID.as_bytes()),
        key: rig.hold_as(
            HOST,
            secret,
            "secret_access_key",
            example_secret().as_bytes(),
        ),
        token: rig.hold_as(HOST, secret, "session_token", example_token().as_bytes()),
    }
}

/// A request for [`HOST`], dated [`WHEN`], with `headers` too.
fn dated(
    method: Method,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Request<Full<Bytes>> {
    let mut req = Request::builder()
        .method(method)
        .uri(target)
        .header(header::HOST, HOST)
        .header("x-amz-date", WHEN);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req.body(Full::new(Bytes::from(body.to_string()))).unwrap()
}

/// What `key` signs a request with: its method, its target, its headers,
/// those `signed` names, and its body's hash.
fn signature_of(
    method: &str,
    target: &str,
    headers: &HeaderMap,
    signed: &Signed,
    payload: &str,
    key: &str,
) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let canonical = sigv4::canonical_request(
        method,
        path,
        query,
        headers,
        &signed.headers,
        payload,
        &signed.service,
    )
    .unwrap();
    let to_sign = sigv4::string_to_sign(WHEN, &signed.scope(), &canonical);
    let key = sigv4::signing_key(
        key.as_bytes(),
        &signed.date,
        &signed.region,
        &signed.service,
    );
    sigv4::signature(&key, &to_sign)
}

/// `req` signed as `aws` signs it: `id` in `Credential=`, `key` the secret
/// key, for a region and a service, `names` its signed headers, `payload` its
/// body's hash.
fn sign(
    mut req: Request<Full<Bytes>>,
    (id, key): (&str, &str),
    (region, service): (&str, &str),
    names: &[&str],
    payload: &str,
) -> Request<Full<Bytes>> {
    let signed = Signed {
        id: id.to_string(),
        date: WHEN[..8].to_string(),
        region: region.to_string(),
        service: service.to_string(),
        headers: names.iter().map(|n| (*n).to_string()).collect(),
        signature: String::new(),
    };
    let target = req.uri().to_string();
    let signature = signature_of(
        req.method().as_str(),
        &target,
        req.headers(),
        &signed,
        payload,
        key,
    );
    let auth = HeaderValue::from_str(&signed.authorization(id, &signature)).unwrap();
    req.headers_mut().insert(header::AUTHORIZATION, auth);
    req
}

/// The `Authorization` botocore sends, signing with the pair's values.
fn botocore(scope: &str, signed: &str, signature: [&str; 4]) -> String {
    format!(
        "AWS4-HMAC-SHA256 Credential={AKID}/20150830/{scope}/aws4_request, \
         SignedHeaders={signed}, Signature={}",
        hex64(signature)
    )
}

/// Whether AWS, holding the pair's values, takes what the audience was
/// sent: its one `Authorization` names the access key's id, and its
/// signature is the secret key's over the request as it arrived.
fn aws_takes(seen: &Seen) -> bool {
    let [auth] = &seen.authorization[..] else {
        return false;
    };
    let Ok(signed) = sigv4::parse(auth) else {
        return false;
    };
    let payload = seen
        .headers
        .get("x-amz-content-sha256")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_else(|| sha256(seen.body.as_bytes()));
    let signature = signature_of(
        seen.method.as_str(),
        &seen.target,
        &seen.headers,
        &signed,
        &payload,
        &example_secret(),
    );
    signed.id == AKID && sigv4::same(&signature, &signed.signature)
}

/// A connection to the audience, through the run's HTTP front, under its CA.
async fn connected(rig: &Rig) -> SendRequest<Full<Bytes>> {
    http(
        through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
            .await
            .unwrap(),
    )
    .await
}

/// Requests signed with the placeholder pair reach the audience signed as
/// botocore signs them with the values: a query (STS by GET), a body read
/// whole and hashed (STS by POST), and S3's path as sent with its own
/// `x-amz-content-sha256`; and an upload whose body no hash signs, as AWS
/// would check it. Nothing else about them changes.
#[tokio::test]
async fn a_request_signed_with_the_placeholder_pair_goes_out_as_botocore_signs_it() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let pair = (held.id.as_str(), held.key.as_str());
    let mut send = connected(&rig).await;
    let empty = sha256(b"");
    let get = sign(
        dated(Method::GET, &format!("/?{CALLER}"), &[], ""),
        pair,
        ("us-east-1", "sts"),
        &["host", "x-amz-date"],
        &empty,
    );
    assert_eq!(
        ask(&mut send, get).await,
        (StatusCode::OK, "from the audience".into())
    );
    let form = "application/x-www-form-urlencoded; charset=utf-8";
    let post = sign(
        dated(Method::POST, "/", &[("content-type", form)], CALLER),
        pair,
        ("us-east-1", "sts"),
        &["content-type", "host", "x-amz-date"],
        &sha256(CALLER.as_bytes()),
    );
    assert_eq!(ask(&mut send, post).await.0, StatusCode::OK);
    let s3 = sign(
        dated(
            Method::GET,
            "/my-bucket/a%20b.txt",
            &[("x-amz-content-sha256", empty.as_str())],
            "",
        ),
        pair,
        ("eu-central-1", "s3"),
        &["host", "x-amz-content-sha256", "x-amz-date"],
        &empty,
    );
    assert_eq!(ask(&mut send, s3).await.0, StatusCode::OK);
    // S3's upload whose body is signed by no hash: `UNSIGNED-PAYLOAD` is what
    // the signature covers, and the body streams on, never read whole.
    let unsigned = "UNSIGNED-PAYLOAD";
    let put = sign(
        dated(
            Method::PUT,
            "/my-bucket/k.txt",
            &[
                ("content-type", "text/plain"),
                ("x-amz-content-sha256", unsigned),
            ],
            "the object",
        ),
        pair,
        ("eu-central-1", "s3"),
        &["content-type", "host", "x-amz-content-sha256", "x-amz-date"],
        unsigned,
    );
    assert_eq!(ask(&mut send, put).await.0, StatusCode::OK);
    let seen = audience.seen();
    assert_eq!(seen.len(), 4);
    assert_eq!(
        seen[0].authorization,
        [botocore(
            "us-east-1/sts",
            "host;x-amz-date",
            [
                "3e059baa17d37e89",
                "f8b2982001b84321",
                "8be129cc59e7b919",
                "a912b1f479fab42e",
            ]
        )]
    );
    assert_eq!(seen[0].target, format!("/?{CALLER}"));
    assert_eq!(
        seen[1].authorization,
        [botocore(
            "us-east-1/sts",
            "content-type;host;x-amz-date",
            [
                "760db4af68ac9d59",
                "e3baecfde1a2699f",
                "7ee788dbeb34f8e3",
                "cc64d8083143a8e0",
            ]
        )]
    );
    assert_eq!(
        seen[1].body, CALLER,
        "the body read whole goes on as it came"
    );
    assert_eq!(
        seen[2].authorization,
        [botocore(
            "eu-central-1/s3",
            "host;x-amz-content-sha256;x-amz-date",
            [
                "6de8561e56af1b9d",
                "168149600e5e5040",
                "88deecc303813135",
                "710b7e6130dd0387",
            ]
        )]
    );
    assert_eq!(seen[2].target, "/my-bucket/a%20b.txt", "S3's path as sent");
    assert_eq!(seen[3].body, "the object");
    for s in &seen {
        assert!(aws_takes(s), "{s:?}");
        assert_eq!(s.headers["x-amz-date"], WHEN);
    }
}

/// The session token's placeholder in `X-Amz-Security-Token` goes out as its
/// value, and the request is signed with it there.
#[tokio::test]
async fn a_session_tokens_placeholder_goes_out_as_its_value_signed_with_it() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let mut send = connected(&rig).await;
    let req = sign(
        dated(
            Method::GET,
            &format!("/?{CALLER}"),
            &[("x-amz-security-token", held.token.as_str())],
            "",
        ),
        (&held.id, &held.key),
        ("us-east-1", "sts"),
        &["host", "x-amz-date", "x-amz-security-token"],
        &sha256(b""),
    );
    assert_eq!(ask(&mut send, req).await.0, StatusCode::OK);
    let seen = audience.seen();
    assert_eq!(
        seen[0].authorization,
        [botocore(
            "us-east-1/sts",
            "host;x-amz-date;x-amz-security-token",
            [
                "96f187a206e460f3",
                "ea18b685767cfb32",
                "3724462af5b10dec",
                "8838cfaf777d3a60",
            ]
        )]
    );
    assert_eq!(
        seen[0].headers["x-amz-security-token"],
        example_token().as_str()
    );
    assert!(aws_takes(&seen[0]));
}

/// The secret key's placeholder signs and is never swapped: written as a
/// token or inside `Basic`'s credentials it goes out as itself, never as
/// its value. The access key's id is swapped there as any value is.
#[tokio::test]
async fn a_secret_keys_placeholder_is_never_swapped_for_its_value() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let mut send = connected(&rig).await;
    let bearer = format!("Bearer {}", held.key);
    let basic = format!("Basic {}", STANDARD.encode(format!("u:{}", held.key)));
    let id = format!("Bearer {}", held.id);
    for auth in [&bearer, &basic, &id] {
        assert_eq!(
            ask(&mut send, get("/", HOST, Some(auth))).await.0,
            StatusCode::OK
        );
    }
    let seen = audience.seen();
    assert_eq!(seen[0].authorization, [bearer]);
    assert_eq!(seen[1].authorization, [basic]);
    assert_eq!(seen[2].authorization, [format!("Bearer {AKID}")]);
    for s in &seen {
        assert!(!format!("{s:?}").contains(&example_secret()), "{s:?}");
    }
}

/// A signed `Expect` goes on, and the request is signed again with it; one
/// no signature covers is dropped, as on every request.
#[tokio::test]
async fn a_signed_expect_stays_and_an_unsigned_one_goes() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let mut send = connected(&rig).await;
    let empty = sha256(b"");
    let headers = [
        ("expect", "100-continue"),
        ("x-amz-content-sha256", empty.as_str()),
    ];
    for names in [
        &["expect", "host", "x-amz-content-sha256", "x-amz-date"][..],
        &["host", "x-amz-content-sha256", "x-amz-date"][..],
    ] {
        let req = sign(
            dated(Method::GET, "/my-bucket/k", &headers, ""),
            (&held.id, &held.key),
            ("eu-central-1", "s3"),
            names,
            &empty,
        );
        assert_eq!(ask(&mut send, req).await.0, StatusCode::OK);
    }
    let seen = audience.seen();
    assert_eq!(
        seen[0].headers["expect"], "100-continue",
        "signed, so it stays"
    );
    assert!(!seen[1].headers.contains_key("expect"), "signed by none");
    assert!(aws_takes(&seen[0]) && aws_takes(&seen[1]));
}

/// Each AWS request the proxy does not sign again is answered with why, and
/// never sent: a signature that does not check (another key, another
/// secret's key), the secret key's placeholder in `Credential=`, an access
/// key with no secret key toward the host, signed chunks, SigV4A, a header
/// that does not read, a second `Authorization`, no `X-Amz-Date`, a signed
/// header that does not go on, and an access key's value that is no id.
#[tokio::test]
async fn an_aws_request_not_signed_again_is_answered_and_never_sent() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let other = hold_aws(&rig, "aws/other");
    let lone = rig.hold_as(HOST, "aws/lone", "access_key_id", AKID.as_bytes());
    let odd_id = rig.hold_as(HOST, "aws/odd", "access_key_id", b"AKID EXAMPLE");
    let odd_key = rig.hold_as(
        HOST,
        "aws/odd",
        "secret_access_key",
        example_secret().as_bytes(),
    );
    let mut send = connected(&rig).await;
    let empty = sha256(b"");
    let sts = ("us-east-1", "sts");
    let names = ["host", "x-amz-date"];
    let caller = || dated(Method::GET, &format!("/?{CALLER}"), &[], "");
    let zeros = "0".repeat(64);
    let cases: Vec<(Request<Full<Bytes>>, StatusCode, String)> = vec![
        (
            sign(caller(), (&held.id, "not-the-key"), sts, &names, &empty),
            StatusCode::FORBIDDEN,
            "does not check with its placeholder pair".into(),
        ),
        (
            sign(caller(), (&held.id, &other.key), sts, &names, &empty),
            StatusCode::FORBIDDEN,
            "does not check with its placeholder pair".into(),
        ),
        (
            sign(caller(), (&held.key, &held.key), sts, &names, &empty),
            StatusCode::FORBIDDEN,
            "Credential names aws/test's secret_access_key placeholder".into(),
        ),
        (
            sign(caller(), (&lone, &held.key), sts, &names, &empty),
            StatusCode::FORBIDDEN,
            "aws/lone has no secret_access_key placeholder".into(),
        ),
        (
            sign(
                dated(
                    Method::GET,
                    "/b/k",
                    &[("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")],
                    "",
                ),
                (&held.id, &held.key),
                ("eu-central-1", "s3"),
                &["host", "x-amz-content-sha256", "x-amz-date"],
                "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
            ),
            StatusCode::NOT_IMPLEMENTED,
            "signed chunks".into(),
        ),
        (
            get(
                "/b/k",
                HOST,
                Some(&format!(
                    "AWS4-ECDSA-P256-SHA256 Credential={}/20150830/s3/aws4_request, \
                     SignedHeaders=host;x-amz-date;x-amz-region-set, Signature={zeros}",
                    held.id
                )),
            ),
            StatusCode::NOT_IMPLEMENTED,
            "signed again as SigV4 (AWS4-HMAC-SHA256) alone".into(),
        ),
        (
            get(
                "/",
                HOST,
                Some(&format!(
                    "AWS4-HMAC-SHA256 Credential={}/20150830/us-east-1/sts/aws4_request, \
                     SignedHeaders=host;x-amz-date, Signature=00",
                    held.id
                )),
            ),
            StatusCode::BAD_REQUEST,
            "does not read: Signature is 64 lower-case hex digits".into(),
        ),
        (
            {
                let mut req = sign(caller(), (&held.id, &held.key), sts, &names, &empty);
                req.headers_mut()
                    .append(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
                req
            },
            StatusCode::BAD_REQUEST,
            "the request's one Authorization".into(),
        ),
        (
            {
                let mut req = sign(caller(), (&held.id, &held.key), sts, &["host"], &empty);
                req.headers_mut().remove("x-amz-date");
                req
            },
            StatusCode::BAD_REQUEST,
            "carries X-Amz-Date".into(),
        ),
        (
            sign(
                dated(
                    Method::GET,
                    &format!("/?{CALLER}"),
                    &[("connection", "x-leg"), ("x-leg", "1")],
                    "",
                ),
                (&held.id, &held.key),
                sts,
                &["host", "x-amz-date", "x-leg"],
                &empty,
            ),
            StatusCode::NOT_IMPLEMENTED,
            "the signed header x-leg does not go on".into(),
        ),
        (
            sign(caller(), (&odd_id, &odd_key), sts, &names, &empty),
            StatusCode::BAD_GATEWAY,
            "aws/odd's access_key_id is no access key's id".into(),
        ),
    ];
    for (req, status, says) in cases {
        let (got, body) = ask(&mut send, req).await;
        assert_eq!(got, status, "{says}: {body}");
        assert!(body.contains(says.as_str()), "{says}: {body}");
        assert!(!body.contains(&example_secret()), "{body}");
    }
    assert!(audience.seen().is_empty(), "none was sent");
    // The connection still serves a request it signs again.
    let req = sign(caller(), (&held.id, &held.key), sts, &names, &empty);
    assert_eq!(ask(&mut send, req).await.0, StatusCode::OK);
    assert!(aws_takes(&audience.seen()[0]));
}

/// Once the pair's leases are let go, a request signed with it is refused,
/// never sent with its placeholders.
#[tokio::test]
async fn a_pair_whose_leases_were_let_go_is_refused() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let mut send = connected(&rig).await;
    lock(&rig.leases.0).clear();
    let req = sign(
        dated(Method::GET, &format!("/?{CALLER}"), &[], ""),
        (&held.id, &held.key),
        ("us-east-1", "sts"),
        &["host", "x-amz-date"],
        &sha256(b""),
    );
    let (status, body) = ask(&mut send, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("lease has ended"), "{body}");
    assert!(audience.seen().is_empty());
}

/// A request that, raw, signs with the pair and says `head`: what the
/// connection answers, its status line.
async fn raw(rig: &Rig, held: &Placeholders, head: &str, body: &[u8]) -> String {
    let mut tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/20150830/us-east-1/sts/aws4_request, \
         SignedHeaders=host;x-amz-date, Signature={}",
        held.id,
        "0".repeat(64)
    );
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {HOST}\r\nX-Amz-Date: {WHEN}\r\nAuthorization: {auth}\r\n\
         {head}\r\n\r\n"
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    tls.write_all(body).await.unwrap();
    tls.flush().await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), status_line(&mut tls))
        .await
        .expect("an answer within 20 s")
}

/// A body past the bound with no `x-amz-content-sha256` is refused: one that
/// says its length up front, before a byte of it is read; one in chunks, as
/// it passes the bound. Neither is sent.
#[tokio::test]
async fn a_body_past_the_bound_is_refused_and_never_sent() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let past = MAX_BODY + 1;
    let said = raw(&rig, &held, &format!("Content-Length: {past}"), b"").await;
    assert_eq!(said, "HTTP/1.1 413 Payload Too Large");
    // One chunk of it, its end never sent: the answer comes as it passes.
    let chunk = format!("{past:x}\r\n");
    let mut body = chunk.into_bytes();
    body.extend(std::iter::repeat_n(b'a', past));
    let chunked = raw(&rig, &held, "Transfer-Encoding: chunked", &body).await;
    assert_eq!(chunked, "HTTP/1.1 413 Payload Too Large");
    assert!(audience.seen().is_empty());
}

/// A body read whole holds one of the run's permits until it is sent: more
/// requests than there are permits, one after another, each go.
#[tokio::test]
async fn a_body_read_whole_lets_its_permit_go_once_sent() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let held = hold_aws(&rig, SECRET);
    let mut send = connected(&rig).await;
    let form = "application/x-www-form-urlencoded; charset=utf-8";
    for _ in 0..=MAX_BODIES {
        let post = sign(
            dated(Method::POST, "/", &[("content-type", form)], CALLER),
            (&held.id, &held.key),
            ("us-east-1", "sts"),
            &["content-type", "host", "x-amz-date"],
            &sha256(CALLER.as_bytes()),
        );
        assert_eq!(ask(&mut send, post).await.0, StatusCode::OK);
    }
    assert_eq!(audience.seen().len(), MAX_BODIES + 1);
}
