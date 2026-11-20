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
