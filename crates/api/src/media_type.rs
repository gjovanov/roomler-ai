// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! What an uploaded file actually IS, rather than what its uploader said.
//!
//! ## Why
//!
//! Both upload handlers stored `field.content_type()` verbatim — a value the
//! client writes into its own multipart part, so it is a claim and nothing
//! more. That claim is then used to decide how the file is presented:
//! `MessageBubble.vue` renders `<v-img :src=…>` for anything whose type starts
//! with `image/`, the file list picks its icon from it, the download route
//! echoes it as `Content-Type`, and `routes::integration` steers a recognition
//! backend with it.
//!
//! None of that is currently an XSS: the download route sends
//! `Content-Disposition: attachment`, nginx sends `X-Content-Type-Options:
//! nosniff`, and an SVG loaded through `<img>` cannot run script. So this is
//! not a patch for a live exploit — it is removing a lie from the data model
//! before something downstream starts trusting it. A stored field that says
//! `image/png` about a file that is not one is wrong today and dangerous the
//! first time someone adds an inline preview.
//!
//! ## The rule
//!
//! 1. **Magic bytes win.** If the content identifies itself, that is the
//!    answer, and the client's claim is discarded entirely.
//! 2. **Otherwise, a narrow extension map** for formats that have no magic
//!    bytes to read — plain text, CSV, JSON, Markdown.
//! 3. **Otherwise `application/octet-stream`** — "we do not know", which is
//!    honest and harmless.
//!
//! ⚠️ The extension map deliberately cannot produce `image/*`, `text/html`,
//! `image/svg+xml` or anything else a browser will render actively. Those are
//! exactly the types worth lying about, so they must be PROVEN by the bytes.
//! Naming a file `.png` gets you `application/octet-stream`, not `image/png`.
//!
//! ⚠️ This is not a whitelist of what a client may claim. A whitelist still
//! trusts the claim — it only narrows which lies are accepted.

const FALLBACK: &str = "application/octet-stream";

/// Types safe to infer from a filename when the bytes carry no signature.
///
/// Every entry is inert in a browser: displayed as text at worst. Nothing here
/// causes active rendering, which is why deriving them from an attacker-chosen
/// filename is acceptable where deriving `image/*` would not be.
const EXTENSION_MAP: &[(&str, &str)] = &[
    ("txt", "text/plain"),
    ("log", "text/plain"),
    ("md", "text/markdown"),
    ("csv", "text/csv"),
    ("tsv", "text/tab-separated-values"),
    ("json", "application/json"),
    ("yaml", "text/plain"),
    ("yml", "text/plain"),
    ("toml", "text/plain"),
];

/// Resolve the content type to STORE for an upload.
///
/// `claimed` is accepted only to be ignored — it is taken as a parameter so
/// call sites read as "we had a claim and did not use it" rather than silently
/// dropping it.
pub fn resolve(claimed: &str, filename: &str, bytes: &[u8]) -> String {
    let _ = claimed;

    if let Some(kind) = infer::get(bytes) {
        return kind.mime_type().to_string();
    }

    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();

    EXTENSION_MAP
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mime)| (*mime).to_string())
        .unwrap_or_else(|| FALLBACK.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest valid PNG: the 8-byte signature is what `infer` reads.
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00";
    const PDF: &[u8] = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n";

    #[test]
    fn the_bytes_win_over_the_claim() {
        // The whole point: an uploader saying "image/png" about a PDF does not
        // make it one, and saying "text/plain" about a PNG does not hide it.
        assert_eq!(resolve("image/png", "invoice.png", PDF), "application/pdf");
        assert_eq!(resolve("text/plain", "notes.txt", PNG), "image/png");
        assert_eq!(
            resolve("application/octet-stream", "x.bin", GIF),
            "image/gif"
        );
    }

    #[test]
    fn an_html_payload_cannot_present_itself_as_an_image() {
        // The case that matters for the chat renderer, which draws <v-img> for
        // anything typed `image/*`. Whatever the uploader claims and whatever
        // they name the file, an HTML body must not come back as an image.
        let html = b"<html><script>alert(1)</script></html>";
        for claimed in ["image/png", "image/svg+xml", "text/html"] {
            for name in ["pic.png", "pic.svg", "page.html"] {
                let got = resolve(claimed, name, html);
                assert!(
                    !got.starts_with("image/"),
                    "claimed {claimed:?} as {name:?} -> {got:?}"
                );
            }
        }

        // `infer` recognises HTML from its content, so the stored value is the
        // truth rather than the fallback — which is the better outcome and
        // worth pinning: the type is honest, and the download route's
        // `Content-Disposition: attachment` is what stops a browser rendering
        // it. If that header ever goes away, THIS is the line that should have
        // made someone think twice.
        assert_eq!(resolve("image/png", "pic.png", html), "text/html");
    }

    #[test]
    fn the_extension_map_cannot_produce_an_actively_rendered_type() {
        // A guard on the TABLE, not on one call: adding `("svg",
        // "image/svg+xml")` later would reintroduce exactly the lie this
        // module removes, and it would look reasonable in review.
        for (ext, mime) in EXTENSION_MAP {
            assert!(
                !mime.starts_with("image/") && *mime != "text/html",
                "extension {ext:?} maps to {mime:?}, which a browser renders actively — \
                 that must be proven by magic bytes, never inferred from a filename"
            );
        }
    }

    #[test]
    fn signature_less_text_falls_back_to_its_extension() {
        let csv = b"a,b,c\n1,2,3\n";
        assert_eq!(resolve("application/json", "data.csv", csv), "text/csv");
        assert_eq!(resolve("x/y", "README.md", b"# hi"), "text/markdown");
        assert_eq!(resolve("x/y", "a.JSON", b"{}"), "application/json");
    }

    #[test]
    fn anything_unrecognised_is_octet_stream() {
        assert_eq!(
            resolve("application/x-thing", "f.thing", b"\x01\x02\x03"),
            FALLBACK
        );
        assert_eq!(resolve("text/plain", "noextension", b"\x01\x02"), FALLBACK);
        assert_eq!(resolve("text/plain", "", b""), FALLBACK);
    }

    #[test]
    fn a_filename_with_several_dots_uses_the_last_extension() {
        // `archive.tar.gz` is gzip; more to the point, `evil.png.txt` must not
        // be read as a png.
        assert_eq!(resolve("x/y", "evil.png.txt", b"hello"), "text/plain");
    }
}
