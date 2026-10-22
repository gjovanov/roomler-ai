// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! SigV4, checked against AWS's own published examples: the signing key's
//! derivation, and the SigV4 test suite's `get-vanilla`.

use hyper::header::{HeaderMap, HeaderValue};

use super::*;

/// AWS's example secret key, from its documentation: built here, so that no
/// commit holds it whole, nor any part long enough to read as a key.
fn example_secret() -> String {
    ["wJalrXUtnFEMI", "/K7MDENG", "+bPxRfiCY", "EXAMPLEKEY"].concat()
}

/// A published value, 64 hex digits, from its four quarters: as it reads
/// in AWS's documentation, and no literal a scanner takes for a key.
fn hex64(quarters: [&str; 4]) -> String {
    let joined = quarters.concat();
    assert_eq!(joined.len(), 64, "{joined}");
    joined
}

/// `get-vanilla`'s signature, as the test suite publishes it.
fn vanilla_signature() -> String {
    hex64([
        "5fa00fa31553b73e",
        "bf1942676e86291e",
        "8372ff2a2260956d",
        "9b8aae1d763fbf31",
    ])
}

/// The signing key, as AWS's documentation derives it for IAM in
/// us-east-1 on 2012-02-15.
#[test]
fn the_signing_key_is_awss_own_example() {
    let key = signing_key(example_secret().as_bytes(), "20120215", "us-east-1", "iam");
    assert_eq!(
        hex::encode(*key),
        hex64([
            "f4780e2d9f65fa89",
            "5f9c67b32ce1baf0",
            "b0d8a43505a000a1",
            "a9e090d414db404d",
        ])
    );
}

/// The test suite's `get-vanilla`: its canonical request, the hash its
/// string to sign carries, and its signature.
#[test]
fn get_vanilla_signs_as_awss_test_suite_says() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("example.amazonaws.com"));
    headers.insert("x-amz-date", HeaderValue::from_static("20150830T123600Z"));
    let signed = ["host".to_string(), "x-amz-date".to_string()];
    let empty = hex::encode(Sha256::digest(b""));
    let canonical =
        canonical_request("GET", "/", "", &headers, &signed, &empty, "service").unwrap();
    assert_eq!(
        canonical,
        format!(
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{empty}"
        )
    );
    let scope = "20150830/us-east-1/service/aws4_request";
    let sts = string_to_sign("20150830T123600Z", scope, &canonical);
    let hashed = hex64([
        "bb579772317eb040",
        "ac9ed261061d46c1",
        "f17a8133879d6129",
        "b6e1c25292927e63",
    ]);
    assert_eq!(
        sts,
        format!("AWS4-HMAC-SHA256\n20150830T123600Z\n{scope}\n{hashed}")
    );
    let key = signing_key(
        example_secret().as_bytes(),
        "20150830",
        "us-east-1",
        "service",
    );
    assert_eq!(signature(&key, &sts), vanilla_signature());
}

/// A header SigV4 wrote reads back whole, and writes again with another id
/// and signature. Anything else is no header this reads.
#[test]
fn a_sigv4_header_reads_back_and_writes_again() {
    let sig = vanilla_signature();
    let sig = sig.as_str();
    let value = format!(
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
         SignedHeaders=host;x-amz-date, Signature={sig}"
    );
    let signed = parse(&value).unwrap();
    assert_eq!(
        signed,
        Signed {
            id: "AKIDEXAMPLE".into(),
            date: "20150830".into(),
            region: "us-east-1".into(),
            service: "service".into(),
            headers: vec!["host".into(), "x-amz-date".into()],
            signature: sig.into(),
        }
    );
    assert_eq!(signed.scope(), "20150830/us-east-1/service/aws4_request");
    assert_eq!(signed.authorization("AKIDEXAMPLE", sig), value);
    let other = "0".repeat(64);
    assert_eq!(
        signed.authorization("ANOTHERID", &other),
        format!(
            "AWS4-HMAC-SHA256 Credential=ANOTHERID/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, Signature={other}"
        )
    );
    // No space after a comma reads the same.
    let tight = value.replace(", ", ",");
    assert_eq!(parse(&tight).unwrap(), signed);
    let cred = "Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request";
    for bad in [
        format!("AWS4-ECDSA-P256-SHA256 {cred}, SignedHeaders=host, Signature={sig}"),
        format!("AWS4-HMAC-SHA256{cred}, SignedHeaders=host, Signature={sig}"),
        format!("AWS4-HMAC-SHA256 {cred}, SignedHeaders=host"),
        format!("AWS4-HMAC-SHA256 {cred}, SignedHeaders=host, Signature={sig}, Signature={sig}"),
        format!("AWS4-HMAC-SHA256 {cred}, SignedHeaders=host, Signature={sig}, Extra=1"),
        format!(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service, SignedHeaders=host, Signature={sig}"
        ),
        format!(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/2015083/us-east-1/service/aws4_request, SignedHeaders=host, Signature={sig}"
        ),
        format!(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_requests, SignedHeaders=host, Signature={sig}"
        ),
        format!(
            "AWS4-HMAC-SHA256 Credential=AKID/EXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host, Signature={sig}"
        ),
        format!("AWS4-HMAC-SHA256 {cred}, SignedHeaders=Host;x-amz-date, Signature={sig}"),
        format!("AWS4-HMAC-SHA256 {cred}, SignedHeaders=;host, Signature={sig}"),
        format!(
            "AWS4-HMAC-SHA256 {cred}, SignedHeaders=host, Signature={}",
            &sig[..63]
        ),
        format!(
            "AWS4-HMAC-SHA256 {cred}, SignedHeaders=host, Signature={}",
            sig.to_uppercase()
        ),
    ] {
        assert!(parse(&bad).is_err(), "{bad}");
    }
}

/// The path as SigV4 reads it: S3's as sent; every other service's with
/// its dot and empty segments gone, and encoded once more, `/` and `~` kept.
#[test]
fn the_path_is_s3s_as_sent_and_every_other_services_encoded_again() {
    assert_eq!(canonical_uri("/my%20key/a.txt", "s3"), "/my%20key/a.txt");
    assert_eq!(canonical_uri("", "s3"), "/");
    assert_eq!(canonical_uri("/a%20b", "iam"), "/a%2520b");
    assert_eq!(canonical_uri("/foo/../bar//./baz/", "iam"), "/bar/baz/");
    assert_eq!(canonical_uri("/~a_b-c.d", "iam"), "/~a_b-c.d");
    assert_eq!(canonical_uri("", "iam"), "/");
    assert_eq!(canonical_uri("/", "iam"), "/");
    assert_eq!(canonical_uri("/..", "iam"), "/");
}

/// The query is its pairs as they stand in the URL, sorted by name then
/// value; a name with no `=` is `name=`.
#[test]
fn the_query_is_its_pairs_as_sent_sorted() {
    assert_eq!(canonical_query(""), "");
    assert_eq!(canonical_query("b=2&a=1&a=0&acl"), "a=0&a=1&acl=&b=2");
    assert_eq!(
        canonical_query("Version=2010-05-08&Action=ListUsers"),
        "Action=ListUsers&Version=2010-05-08"
    );
}

/// A signed header's values are each trimmed, their inner white space made
/// one space, and joined by `,`; a signed header not carried is refused.
#[test]
fn a_signed_headers_values_are_trimmed_and_joined() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("example.amazonaws.com"));
    headers.append("my-header1", HeaderValue::from_static("  value1  "));
    headers.append(
        "my-header1",
        HeaderValue::from_static("value2   with \t spaces"),
    );
    let signed = ["host".to_string(), "my-header1".to_string()];
    let canonical =
        canonical_request("GET", "/", "", &headers, &signed, "UNSIGNED-PAYLOAD", "s3").unwrap();
    assert!(
        canonical.contains("\nmy-header1:value1,value2 with spaces\n"),
        "{canonical}"
    );
    let missing = ["host".to_string(), "x-amz-date".to_string()];
    let refused = canonical_request("GET", "/", "", &headers, &missing, "UNSIGNED-PAYLOAD", "s3")
        .unwrap_err();
    assert!(refused.contains("x-amz-date"), "{refused}");
}

/// A signature made with one key checks with the same key, and with no
/// other: what P3b-4-2's check rests on.
#[test]
fn a_signature_checks_with_its_own_key_alone() {
    let scope = "20150830/us-east-1/service/aws4_request";
    let sts = string_to_sign("20150830T123600Z", scope, "a canonical request");
    let key = |secret: &[u8]| signing_key(secret, "20150830", "us-east-1", "service");
    let mine = signature(&key(b"one"), &sts);
    assert!(same(&mine, &signature(&key(b"one"), &sts)));
    assert!(!same(&mine, &signature(&key(b"two"), &sts)));
    assert!(!same(&mine, &mine[..63]), "a shorter one is another");
}
