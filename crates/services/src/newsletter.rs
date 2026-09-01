// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-58 — newsletter rendering: markdown → the branded email HTML.
//!
//! Three properties are the contract, not implementation detail:
//!
//! 1. **Raw HTML never survives.** `render_markdown` filters
//!    `Event::Html`/`Event::InlineHtml` before `push_html`, so the only HTML
//!    in a rendered body is renderer-emitted. A pasted export cannot smuggle
//!    tags, tracking pixels or foreign styling into the wrapper — and the
//!    email pipeline has no other sanitizer, so this filter is the boundary.
//! 2. **Pre-render once, substitute per recipient.** The wrapper carries
//!    [`UNSUB_PLACEHOLDER`]; the fan-out substitutes each recipient's
//!    unsubscribe URL. Every recipient therefore receives byte-identical
//!    content except their link — which is what makes the admin preview THE
//!    sent artifact rather than a sibling of it.
//! 3. **Operator strings are escaped.** Subject/preheader/CTA/alt pass
//!    [`escape_html`] before entering the wrapper, and header-bound strings
//!    pass [`clean_header_text`] at the route (a `\r\n` in a subject is SMTP
//!    header injection on the lettre backend).

use pulldown_cmark::{Event, Options, Parser, html};
use roomler_ai_db::models::NewsletterIssue;

/// Sentinel the wrapper embeds where a recipient's unsubscribe URL belongs.
/// Chosen to survive both markdown rendering and HTML escaping unchanged; an
/// operator who literally types it into a body gets it substituted, which is
/// harmless (admin-only surface).
pub const UNSUB_PLACEHOLDER: &str = "%%UNSUBSCRIBE_URL%%";

/// Minimal, complete HTML escaping for text landing in the wrapper.
/// (First in the tree — the transactional templates interpolate unescaped,
/// which FR-58's operator-authored fields must not.)
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Single-line header/metadata hygiene: strips ALL control characters
/// (CR/LF included — SMTP header injection), trims, and caps at `max_chars`
/// on a char boundary. Truncation over rejection: these are operator-supplied
/// fields on the operator's own issue.
pub fn clean_header_text(s: &str, max_chars: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Markdown → HTML with raw-HTML events dropped structurally (property 1).
pub fn render_markdown(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser =
        Parser::new_ext(md, opts).filter(|e| !matches!(e, Event::Html(_) | Event::InlineHtml(_)));
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// Inline-style pass over renderer output. Mail clients strip `<style>`
/// blocks, so the tags the renderer emits get their styles injected here.
/// ⚠️ Safe ONLY because the input is `render_markdown` output — every tag
/// below is renderer-emitted with a known shape; never run this over
/// arbitrary HTML.
fn style_rendered_html(body: &str) -> String {
    body
        // `<pre><code…` first, so the inner code block doesn't take the
        // inline-code style afterwards (`<code>` with no attrs won't match it
        // once this inserted `style=`).
        .replace(
            "<pre><code",
            "<pre style=\"background:#1a1a2e;color:#e0f2f1;padding:14px 16px;border-radius:8px;overflow-x:auto;font-size:14px;line-height:1.5;\"><code style=\"font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;\"",
        )
        .replace(
            "<code>",
            "<code style=\"font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:rgba(0,150,136,0.10);padding:1px 5px;border-radius:4px;font-size:14px;\">",
        )
        .replace("<a href=", "<a style=\"color:#00796B;\" href=")
        .replace(
            "<h2>",
            "<h2 style=\"margin:24px 0 12px;font-size:21px;line-height:1.3;color:#1a1a2e;\">",
        )
        .replace(
            "<h3>",
            "<h3 style=\"margin:20px 0 10px;font-size:17px;line-height:1.3;color:#1a1a2e;\">",
        )
        .replace(
            "<blockquote>",
            "<blockquote style=\"margin:16px 0;padding:8px 16px;border-left:3px solid #009688;background:rgba(0,150,136,0.06);border-radius:4px;color:#1a1a2e;\">",
        )
        .replace(
            "<table>",
            "<table style=\"border-collapse:collapse;margin:16px 0;width:100%;\">",
        )
        .replace(
            "<th>",
            "<th style=\"border:1px solid rgba(0,150,136,0.25);padding:6px 10px;text-align:left;background:rgba(0,150,136,0.08);\">",
        )
        .replace(
            "<td>",
            "<td style=\"border:1px solid rgba(0,150,136,0.25);padding:6px 10px;\">",
        )
        .replace(
            "<hr />",
            "<hr style=\"border:none;border-top:1px solid rgba(0,150,136,0.25);margin:24px 0;\" />",
        )
}

const FONT_STACK: &str = "system-ui,-apple-system,'Segoe UI',Roboto,sans-serif";

/// The branded 600 px light-only wrapper (Roomler Field Notes). Returns the
/// COMPLETE email HTML with [`UNSUB_PLACEHOLDER`] where the recipient's
/// unsubscribe URL belongs — the exact bytes a recipient receives, which is
/// also exactly what the admin preview serves.
pub fn render_issue_html(issue: &NewsletterIssue) -> String {
    let subject = escape_html(&issue.subject);
    let preheader = escape_html(&issue.preheader);
    let body = style_rendered_html(&render_markdown(&issue.body_md));

    let hero = match (&issue.hero_url, &issue.hero_alt) {
        (Some(url), alt) => format!(
            "<img src=\"{}\" width=\"600\" alt=\"{}\" style=\"width:100%;height:auto;display:block;\" />",
            escape_html(url),
            escape_html(alt.as_deref().unwrap_or("")),
        ),
        (None, _) => String::new(),
    };

    let cta = match (&issue.cta_text, &issue.cta_url) {
        (Some(text), Some(url)) => format!(
            "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"margin:28px auto 4px;\"><tr>\
             <td style=\"background:#009688;border-radius:6px;\">\
             <a href=\"{}\" style=\"display:inline-block;padding:12px 28px;color:#ffffff;font-family:{};font-size:16px;font-weight:600;text-decoration:none;\">{}</a>\
             </td></tr></table>",
            escape_html(url),
            FONT_STACK,
            escape_html(text),
        ),
        _ => String::new(),
    };

    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\" />\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
         <meta name=\"color-scheme\" content=\"light only\" />\n\
         <title>{subject}</title>\n</head>\n\
         <body style=\"margin:0;padding:0;background:#f5f7fa;\">\n\
         <div style=\"display:none;max-height:0;overflow:hidden;mso-hide:all;\">{preheader}</div>\n\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#f5f7fa;\"><tr>\
         <td align=\"center\" style=\"padding:24px 12px;\">\n\
         <table role=\"presentation\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:600px;max-width:100%;\">\n\
         <tr><td style=\"padding:0 8px 14px;font-family:{font};font-size:18px;font-weight:700;color:#1a1a2e;\">\
         Roomler <span style=\"color:#009688;\">Field Notes</span></td></tr>\n\
         <tr><td style=\"background:#ffffff;border:1px solid rgba(0,150,136,0.18);border-radius:12px;overflow:hidden;\">\n\
         {hero}\n\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\"><tr>\
         <td style=\"padding:28px 32px;font-family:{font};font-size:16px;line-height:1.6;color:#1a1a2e;\">\n\
         <h1 style=\"margin:0 0 16px;font-size:26px;line-height:1.25;color:#1a1a2e;\">{subject}</h1>\n\
         {body}\n{cta}\n\
         </td></tr></table>\n\
         </td></tr>\n\
         <tr><td style=\"padding:20px 8px 0;font-family:{font};font-size:12.5px;line-height:1.7;color:#5b6570;text-align:center;\">\n\
         You subscribed at roomler.ai. Product updates only, never more than monthly.<br />\n\
         No tracking pixels — the only per-recipient thing in this email is your unsubscribe link.<br />\n\
         <a href=\"{unsub}\" style=\"color:#00796B;\">Unsubscribe</a>&nbsp;&middot;&nbsp;<a href=\"https://roomler.ai\" style=\"color:#00796B;\">roomler.ai</a><br />\n\
         Roomler &middot; G ROX LTD, Pazardzhik, Bulgaria\n\
         </td></tr>\n\
         </table>\n</td></tr></table>\n</body>\n</html>\n",
        subject = subject,
        preheader = preheader,
        font = FONT_STACK,
        hero = hero,
        body = body,
        cta = cta,
        unsub = UNSUB_PLACEHOLDER,
    )
}

/// Per-recipient substitution (property 2). Hex tokens need no escaping; the
/// URL is ours, built server-side.
pub fn substitute_recipient(rendered: &str, unsubscribe_url: &str) -> String {
    rendered.replace(UNSUB_PLACEHOLDER, unsubscribe_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::DateTime;
    use roomler_ai_db::models::IssueStatus;

    fn issue(body_md: &str) -> NewsletterIssue {
        NewsletterIssue {
            id: None,
            slug: "test-issue".into(),
            subject: "A <subject> & so".into(),
            preheader: "Preview line".into(),
            body_md: body_md.into(),
            hero_url: Some("https://roomler.ai/newsletter-img/test-v1.png".into()),
            hero_alt: Some("An \"alt\" text".into()),
            cta_text: Some("Try it".into()),
            cta_url: Some("https://roomler.ai/".into()),
            status: IssueStatus::Draft,
            claimed_by: None,
            claimed_at: None,
            counts: None,
            created_at: DateTime::now(),
            updated_at: DateTime::now(),
            sent_at: None,
        }
    }

    #[test]
    fn raw_html_is_dropped_structurally() {
        let out = render_markdown(
            "hello <b onmouseover=\"x()\">bold</b>\n\n<script>alert(1)</script>\n\n<img src=x onerror=steal()>\n\nworld",
        );
        assert!(
            !out.contains("<script"),
            "block HTML must be dropped: {out}"
        );
        assert!(!out.contains("<b"), "inline HTML must be dropped: {out}");
        assert!(
            !out.contains("onerror"),
            "attributes must be dropped: {out}"
        );
        assert!(out.contains("hello"), "the text around it survives: {out}");
        assert!(out.contains("world"));
    }

    #[test]
    fn markdown_basics_render() {
        let out = render_markdown(
            "## Heading\n\n- one\n- two\n\n[link](https://example.com)\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n~~gone~~\n\n`code` and\n\n```\nblock\n```\n",
        );
        assert!(out.contains("<h2>"), "{out}");
        assert!(out.contains("<li>one</li>"), "{out}");
        assert!(out.contains("<a href=\"https://example.com\""), "{out}");
        assert!(out.contains("<table>"), "{out}");
        assert!(out.contains("<del>gone</del>"), "{out}");
        assert!(out.contains("<code>code</code>"), "{out}");
        assert!(out.contains("<pre><code>block"), "{out}");
    }

    #[test]
    fn escape_html_covers_the_five() {
        assert_eq!(
            escape_html("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn clean_header_text_strips_controls_and_caps() {
        assert_eq!(
            clean_header_text("  Subject\r\nBcc: evil@x\ttail  ", 200),
            "SubjectBcc: evil@xtail"
        );
        assert_eq!(clean_header_text("abcdef", 3), "abc");
    }

    #[test]
    fn wrapper_escapes_operator_strings_and_carries_the_contract() {
        let html = render_issue_html(&issue("Body **text** with a [link](https://example.com)."));
        // Subject escaped, twice (title + h1) — never raw.
        assert!(!html.contains("A <subject>"), "raw subject leaked");
        assert!(html.contains("A &lt;subject&gt; &amp; so"));
        // The per-recipient placeholder is present for the fan-out.
        assert!(html.contains(UNSUB_PLACEHOLDER));
        // Hero + alt escaped; CTA present.
        assert!(html.contains("newsletter-img/test-v1.png"));
        assert!(html.contains("An &quot;alt&quot; text"));
        assert!(html.contains(">Try it</a>"));
        // Rendered links get the inline brand style (mail clients strip
        // <style> blocks, so this is the only styling path).
        assert!(html.contains("<a style=\"color:#00796B;\" href=\"https://example.com\""));
        // The footer promises, verbatim enough to notice drift.
        assert!(html.contains("No tracking pixels"));
        assert!(html.contains("never more than monthly"));
        assert!(html.contains("G ROX LTD"));
        // Light-only — the wrapper commits to one look.
        assert!(html.contains("color-scheme"));
    }

    #[test]
    fn wrapper_omits_hero_and_cta_when_absent() {
        let mut i = issue("plain");
        i.hero_url = None;
        i.cta_text = None;
        i.cta_url = None;
        let html = render_issue_html(&i);
        assert!(!html.contains("<img"));
        assert!(!html.contains("Try it"));
    }

    #[test]
    fn substitution_replaces_every_occurrence() {
        let html = render_issue_html(&issue("body"));
        let out = substitute_recipient(&html, "https://roomler.ai/api/subscribe/unsubscribe/abc");
        assert!(!out.contains(UNSUB_PLACEHOLDER));
        assert!(out.contains("unsubscribe/abc"));
    }
}
