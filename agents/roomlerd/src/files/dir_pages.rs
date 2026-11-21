//! #4121 — a folder listing's answer, as the frames that carry it.
//!
//! A webrtc-rs receiver reads each message into 65 535 bytes, and the SCTP
//! stack refuses to send one over 65 536: a listing longer than that, sent
//! as one message, never arrives. So a listing goes whole when it fits, as
//! before; in pages when it does not and the viewer asked for them
//! (`paged`); and otherwise as one `files:dir-error` saying why, which every
//! viewer shows.

use super::{DirEntryView, DirListing, FilesFrame, FilesOutgoing};
use std::ops::Range;

/// The longest frame every viewer takes whole: a native (webrtc-rs)
/// receiver's read buffer, one byte under the SCTP stack's own limit. A
/// frame of 65 536 bytes is sent, and closes such a receiver's stream for
/// good.
pub(crate) const FILES_FRAME_MAX: usize = 65_535;

/// The most one page of a listing sent in pages holds, measured on the frame
/// as serialized: the agent's envelope elsewhere (the logs fetch's 40 KiB
/// frames).
pub(crate) const DIR_PAGE_BUDGET: usize = 40 * 1024;

/// The answer to the listing request `req_id`, frame by frame, in the order
/// they go out ([`answer_measured`]).
#[cfg(test)]
pub(crate) fn answer(req_id: &str, listing: &DirListing, paged: bool) -> Vec<FilesFrame> {
    answer_measured(req_id, listing, paged).0
}

/// The answer to the listing request `req_id`, frame by frame, in the order
/// they go out, and the listing's length as one frame:
///
/// - the listing whole, as before, when that frame is at most
///   [`FILES_FRAME_MAX`] (no page fields, to every viewer);
/// - else, to a viewer that asked for pages, `files:dir-list` pages of at
///   most [`DIR_PAGE_BUDGET`] each, measured as serialized: every entry, in
///   order, each page numbered `page` of `pages` with the listing's `total`,
///   and the last one `capped` when the folder held more;
/// - else one `files:dir-error` naming the folder, its count and the
///   listing's size, within [`FILES_FRAME_MAX`].
pub(crate) fn answer_measured(
    req_id: &str,
    listing: &DirListing,
    paged: bool,
) -> (Vec<FilesFrame>, usize) {
    let Ok(whole) = list_frame(req_id, listing, ListPart::Whole) else {
        return (Vec::new(), 0);
    };
    let bytes = whole.text.len();
    if bytes <= FILES_FRAME_MAX {
        return (vec![whole], bytes);
    }
    if paged && let Some(pages) = paginate(req_id, listing) {
        return (pages, bytes);
    }
    (refusal(req_id, listing, bytes), bytes)
}

/// Which of a listing one `files:dir-list` carries.
enum ListPart {
    /// Every entry, as every agent before #4121 sent it.
    Whole,
    /// `entries[range]`, as page `page` of `pages`.
    Page {
        range: Range<usize>,
        page: usize,
        pages: usize,
    },
    /// No entry, with each number at its widest: the most a page carries
    /// besides its entries.
    Envelope,
}

fn list_frame(
    req_id: &str,
    listing: &DirListing,
    part: ListPart,
) -> serde_json::Result<FilesFrame> {
    let total = listing.entries.len();
    let (entries, numbers, capped): (&[DirEntryView], _, _) = match part {
        ListPart::Whole => (&listing.entries, None, listing.capped),
        ListPart::Page { range, page, pages } => (
            &listing.entries[range],
            Some((page, pages)),
            listing.capped && page + 1 == pages,
        ),
        ListPart::Envelope => (&[], Some((total, total)), true),
    };
    FilesOutgoing::DirList {
        req_id,
        path: &listing.path,
        parent: listing.parent.as_deref(),
        entries,
        page: numbers.map(|(page, _)| page),
        pages: numbers.map(|(_, pages)| pages),
        total: numbers.map(|_| total),
        capped,
    }
    .frame()
}

/// The listing in pages of at most [`DIR_PAGE_BUDGET`] bytes, each filled
/// before the next begins; `None` when the folder's path alone leaves a page
/// no room for an entry. Every page's numbers are at most its listing's
/// total, so the envelope bounds each page's frame around its entries; the
/// pages are then measured as they go out.
fn paginate(req_id: &str, listing: &DirListing) -> Option<Vec<FilesFrame>> {
    let envelope = list_frame(req_id, listing, ListPart::Envelope)
        .ok()?
        .text
        .len();
    let mut ranges = Vec::new();
    let (mut from, mut used) = (0, envelope);
    for (i, entry) in listing.entries.iter().enumerate() {
        let len = serde_json::to_string(entry).ok()?.len();
        if i > from && used + 1 + len > DIR_PAGE_BUDGET {
            ranges.push(from..i);
            (from, used) = (i, envelope);
        }
        // A comma before every entry but a page's first.
        used += usize::from(i > from) + len;
        if used > DIR_PAGE_BUDGET {
            return None;
        }
    }
    ranges.push(from..listing.entries.len());
    let pages = ranges.len();
    let mut frames = Vec::with_capacity(pages);
    for (page, range) in ranges.into_iter().enumerate() {
        let frame = list_frame(req_id, listing, ListPart::Page { range, page, pages }).ok()?;
        if frame.text.len() > DIR_PAGE_BUDGET {
            return None;
        }
        frames.push(frame);
    }
    Some(frames)
}

/// #4121 (D1-A) — the one frame that tells a viewer why a listing of `bytes`
/// does not come: the folder, its count and the listing's size. A folder
/// whose path alone carries this frame past [`FILES_FRAME_MAX`] is named by
/// its last part, shortened until the frame fits. It promises nothing about
/// a reload: an old viewer's tab, or a self-hosted server's viewer, may
/// never get a new one.
fn refusal(req_id: &str, listing: &DirListing, bytes: usize) -> Vec<FilesFrame> {
    let more = if listing.capped { "more than " } else { "" };
    let count = listing.entries.len();
    let size = size_words(bytes);
    let refuse = |folder: &str| {
        let message = format!(
            "{folder} holds {more}{count} entries, {size} as a listing: \
             too large for this viewer to receive at once"
        );
        FilesOutgoing::DirError {
            req_id,
            message: &message,
        }
        .frame()
        .ok()
    };
    let fits = |frame: &FilesFrame| frame.text.len() <= FILES_FRAME_MAX;
    if let Some(frame) = refuse(&listing.path).filter(fits) {
        return vec![frame];
    }
    let last = std::path::Path::new(&listing.path).file_name().map_or_else(
        || listing.path.clone(),
        |n| n.to_string_lossy().into_owned(),
    );
    if let Some(frame) = refuse(&last).filter(fits) {
        return vec![frame];
    }
    // The last part shortened, with an ellipsis: the longest that fits, each
    // character kept adding to the frame.
    let chars: Vec<char> = last.chars().collect();
    let shortened = |keep: usize| -> String {
        let mut name: String = chars[..keep].iter().collect();
        name.push('…');
        name
    };
    let (mut lo, mut hi, mut best) = (0, chars.len(), None);
    while lo < hi {
        let keep = lo + (hi - lo) / 2;
        match refuse(&shortened(keep)).filter(fits) {
            Some(frame) => {
                best = Some(frame);
                lo = keep + 1;
            }
            None => hi = keep,
        }
    }
    // Nothing fits only beside a request id near the limit itself, which a
    // viewer chose: its send fails, and is logged.
    best.or_else(|| refuse(&shortened(0))).into_iter().collect()
}

/// A listing's size, as a person reads it.
fn size_words(bytes: usize) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{} KB", bytes.div_ceil(1_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::DirEntryView;
    use serde_json::{Value, json};

    /// `files:dir-list` as every agent before #4121 wrote it.
    #[derive(serde::Serialize)]
    #[serde(tag = "t")]
    enum Today<'a> {
        #[serde(rename = "files:dir-list")]
        DirList {
            req_id: &'a str,
            path: &'a str,
            parent: Option<&'a str>,
            entries: &'a [DirEntryView],
        },
    }

    /// An empty file called `name`, as a listing shows it.
    fn file(name: String) -> DirEntryView {
        DirEntryView {
            name,
            is_dir: false,
            size: Some(0),
            mtime_unix: Some(1_763_683_200),
        }
    }

    /// The `i`th name of a folder whose names are `len` characters long.
    fn name(i: usize, len: usize) -> String {
        let head = format!("{i:05}-");
        let pad = len.saturating_sub(head.len());
        format!("{head}{}", "x".repeat(pad))
    }

    /// The folder `path`, of `n` empty files whose names are `len`
    /// characters long.
    fn folder(path: &str, n: usize, len: usize) -> DirListing {
        DirListing {
            path: path.to_string(),
            parent: Some("/home/person".to_string()),
            entries: (0..n).map(|i| file(name(i, len))).collect(),
            capped: false,
        }
    }

    fn json_of(frame: &FilesFrame) -> Value {
        serde_json::from_str(&frame.text).expect("a frame is JSON")
    }

    fn kinds(frames: &[FilesFrame]) -> Vec<&'static str> {
        frames.iter().map(|f| f.kind).collect()
    }

    fn largest(frames: &[FilesFrame]) -> usize {
        frames.iter().map(|f| f.text.len()).max().unwrap_or(0)
    }

    fn names(listing: &DirListing) -> Vec<String> {
        listing.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// The names a listing's pages carry, joined in the order they go out,
    /// once each page is checked whole and numbered: page `i` of `n`, every
    /// one naming the request, the folder, its parent and the listing's
    /// total.
    fn joined(frames: &[FilesFrame], req_id: &str, listing: &DirListing) -> Vec<String> {
        let n = frames.len();
        let mut names = Vec::new();
        for (i, frame) in frames.iter().enumerate() {
            let page = json_of(frame);
            assert_eq!(
                (frame.kind, frame.id.as_str()),
                ("files:dir-list", req_id),
                "page {i}"
            );
            assert_eq!(page["t"], "files:dir-list", "page {i}");
            assert_eq!(page["req_id"], req_id, "page {i}");
            assert_eq!(page["path"], listing.path.as_str(), "page {i}");
            assert_eq!(page["parent"], json!(listing.parent), "page {i}");
            assert_eq!(page["page"], i, "page {i}");
            assert_eq!(page["pages"], n, "page {i}");
            assert_eq!(page["total"], listing.entries.len(), "page {i}");
            for entry in page["entries"].as_array().expect("a page's entries") {
                names.push(entry["name"].as_str().expect("a name").to_string());
            }
        }
        names
    }

    /// A listing that fits in one frame goes as today's bytes, to a viewer
    /// that joins pages and to one that does not.
    #[test]
    fn a_listing_that_fits_is_todays_frame() {
        let listing = folder("/home/person/w23-21", 21, 120);
        let today = serde_json::to_string(&Today::DirList {
            req_id: "r1",
            path: &listing.path,
            parent: listing.parent.as_deref(),
            entries: &listing.entries,
        })
        .expect("today's frame");
        for paged in [false, true] {
            let frames = answer("r1", &listing, paged);
            let texts: Vec<&str> = frames.iter().map(|f| f.text.as_str()).collect();
            assert_eq!(texts, [today.as_str()], "paged: {paged}");
            assert_eq!(kinds(&frames), ["files:dir-list"], "paged: {paged}");
            assert_eq!(frames[0].id, "r1", "paged: {paged}");
        }
    }

    /// #4121 — 10 000 names of 120 characters (about 1.8 MB as one frame)
    /// reach a viewer that joins pages: every entry, in order, each page
    /// within the budget and filled.
    #[test]
    fn pages_hold_every_entry_in_order_within_the_budget() {
        let listing = folder("/home/person/w23-10000", 10_000, 120);
        let frames = answer("r2", &listing, true);
        assert!(
            frames.len() > 1 && largest(&frames) <= DIR_PAGE_BUDGET,
            "{} frame(s), the largest {} bytes: one whole ~1.8 MB frame (#4121)",
            frames.len(),
            largest(&frames)
        );
        assert_eq!(joined(&frames, "r2", &listing), names(&listing));
        let last = frames.len() - 1;
        for (i, frame) in frames.iter().enumerate() {
            assert!(
                i == last || frame.text.len() > DIR_PAGE_BUDGET - 1_024,
                "page {i} of {} holds {} bytes: a page is filled before the next begins",
                frames.len(),
                frame.text.len()
            );
            assert!(
                json_of(frame).get("capped").is_none(),
                "page {i} of a folder listed whole says it was cut"
            );
        }
    }

    /// #4121 (D1-A) — the same listing, asked by a viewer that does not join
    /// pages: one `files:dir-error` small enough for any viewer, naming the
    /// folder, its count and the listing's size, and promising nothing.
    #[test]
    fn an_old_viewer_gets_one_refusal_naming_the_folder() {
        let listing = folder("/home/person/w23-10000", 10_000, 120);
        let frames = answer("r3", &listing, false);
        assert!(
            kinds(&frames) == ["files:dir-error"],
            "{:?}, the largest {} bytes: one whole ~1.8 MB frame (#4121)",
            kinds(&frames),
            largest(&frames)
        );
        let refusal = json_of(&frames[0]);
        assert_eq!(frames[0].id, "r3");
        assert_eq!(refusal["req_id"], "r3");
        assert!(frames[0].text.len() <= FILES_FRAME_MAX);
        let message = refusal["message"].as_str().expect("a message");
        for named in ["/home/person/w23-10000", "10000", "MB"] {
            assert!(message.contains(named), "{named:?} is not in {message:?}");
        }
        assert!(
            !message.to_lowercase().contains("reload"),
            "an old viewer's tab may never get a new viewer: {message:?}"
        );
    }

    /// #4121 — a control character is six bytes once escaped (`\u0001`):
    /// pages are sized on the frame as serialized, never on the names'
    /// lengths.
    #[test]
    fn escaped_names_are_measured_not_estimated() {
        let listing = DirListing {
            path: "/home/person/w23-odd".to_string(),
            parent: Some("/home/person".to_string()),
            entries: (0..1_000)
                .map(|i| file(format!("{i:05}{}", "\u{1}".repeat(115))))
                .collect(),
            capped: false,
        };
        let frames = answer("r4", &listing, true);
        assert!(
            frames.len() > 1 && largest(&frames) <= DIR_PAGE_BUDGET,
            "{} frame(s), the largest {} bytes: one whole ~740 KB frame (#4121)",
            frames.len(),
            largest(&frames)
        );
        assert_eq!(joined(&frames, "r4", &listing), names(&listing));
    }

    /// #4121 — a folder whose path alone carries a frame past what every
    /// viewer takes (and a page past the budget) is named by its last part,
    /// shortened to fit when that part is too long as well.
    #[test]
    fn a_path_too_long_for_a_frame_is_named_by_its_last_part() {
        let deep = format!("/{}/w23-last-part", "p".repeat(70_000));
        let one_part = format!("/{}", "q".repeat(70_000));
        let qs = "q".repeat(64);
        for (path, named) in [(&deep, "w23-last-part"), (&one_part, qs.as_str())] {
            let listing = DirListing {
                path: path.clone(),
                parent: path.rsplit_once('/').map(|(p, _)| p.to_string()),
                entries: (0..3).map(|i| file(name(i, 20))).collect(),
                capped: false,
            };
            for paged in [false, true] {
                let frames = answer("r5", &listing, paged);
                assert!(
                    kinds(&frames) == ["files:dir-error"] && largest(&frames) <= FILES_FRAME_MAX,
                    "{:?}, the largest {} bytes (paged: {paged}): one whole ~140 KB frame (#4121)",
                    kinds(&frames),
                    largest(&frames)
                );
                let message = json_of(&frames[0])["message"]
                    .as_str()
                    .expect("a message")
                    .to_string();
                assert!(
                    message.contains(named),
                    "the folder's last part is not in a {} byte message (paged: {paged})",
                    message.len()
                );
            }
        }
    }

    /// #4121 (D3-A) — a folder that held more than a listing takes says so on
    /// the last page only, and its refusal says "more than".
    #[test]
    fn a_capped_listing_says_so_on_its_last_page() {
        let mut listing = folder("/home/person/w23-10001", 10_000, 120);
        listing.capped = true;
        let frames = answer("r6", &listing, true);
        let capped: Vec<bool> = frames
            .iter()
            .map(|f| json_of(f)["capped"] == true)
            .collect();
        assert!(
            capped.len() > 1
                && capped.last() == Some(&true)
                && capped.iter().filter(|c| **c).count() == 1,
            "the last of {} page(s) says the folder held more: {capped:?} (#4121)",
            frames.len()
        );
        let refused = answer("r6", &listing, false);
        let message = refused
            .first()
            .map(|f| json_of(f)["message"].as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        assert!(
            message.contains("more than 10000"),
            "{:?}: {message:?}",
            kinds(&refused)
        );
    }
}
