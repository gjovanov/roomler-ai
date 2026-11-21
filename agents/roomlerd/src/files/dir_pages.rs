//! #4121 — a folder listing's answer, as the frames that carry it.
//!
//! A webrtc-rs receiver reads each message into 65 535 bytes, and the SCTP
//! stack refuses to send one over 65 536: a listing longer than that, sent
//! as one message, never arrives.

use super::{DirListing, FilesFrame, FilesOutgoing};

/// The longest frame every viewer takes whole: a native (webrtc-rs)
/// receiver's read buffer, one byte under the SCTP stack's own limit. A
/// frame of 65 536 bytes is sent, and closes such a receiver's stream for
/// good.
#[allow(dead_code)] // the pages' limit, in the next commit
pub(crate) const FILES_FRAME_MAX: usize = 65_535;

/// The most one page of a listing sent in pages holds, measured on the frame
/// as serialized: the agent's envelope elsewhere (the logs fetch's 40 KiB
/// frames).
#[allow(dead_code)] // the pages' limit, in the next commit
pub(crate) const DIR_PAGE_BUDGET: usize = 40 * 1024;

/// The answer to the listing request `req_id`, frame by frame, in the order
/// they go out: the listing, whole.
pub(crate) fn answer(req_id: &str, listing: &DirListing, paged: bool) -> Vec<FilesFrame> {
    let _ = paged;
    FilesOutgoing::DirList {
        req_id,
        path: &listing.path,
        parent: listing.parent.as_deref(),
        entries: &listing.entries,
        page: None,
        pages: None,
        total: None,
        capped: false,
    }
    .frame()
    .into_iter()
    .collect()
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
