// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4053 — whether another console session has started on this Mac since a
//! watch began, from what the window server posts when one does.
//!
//! The root daemon's consent panel ([`super::mac`]) reads it at each question,
//! through [`super::panel_gate`]: after a log-out and a login, a fast user
//! switch or a restart of loginwindow, the person the panel could reach when
//! it came up is no longer the one at the screen. Only a root process watches
//! ([`super::panel_gate::reads_console_session`]): a per-user process's own
//! GUI session lives through a fast user switch.
//!
//! CoreGraphics names the two notifications in its public `CGSession.h`
//! (`kCGNotifyGUIConsoleSessionChanged`, `kCGNotifyGUISessionUserChanged`) and
//! posts them through notify(3), a name space that every process on the Mac
//! shares, a root daemon's included. `CGSessionCopyCurrentDictionary` cannot
//! answer here: it returns `NULL` in any process outside a login session, a
//! LaunchDaemon's too. A name registered with `notify_register_check` is read
//! with `notify_check`: a read of shared memory, with no thread and no run
//! loop.

use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};

/// `NOTIFY_STATUS_OK`, `<notify.h>`.
const NOTIFY_STATUS_OK: u32 = 0;

unsafe extern "C" {
    /// notify(3): a token for `name` that `notify_check` reads.
    fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
    /// notify(3): `*check` is 1 when `name` was posted since this token's
    /// last check, and always on a token's first check.
    fn notify_check(token: c_int, check: *mut c_int) -> u32;
    /// notify(3): the token is let go.
    fn notify_cancel(token: c_int) -> u32;
}

/// What the window server posts when the console session changes: another
/// session comes to the console, or the console's session changes hands.
pub(crate) const CONSOLE_SESSION_NAMES: [&CStr; 2] = [
    c"com.apple.coregraphics.GUIConsoleSessionChanged",
    c"com.apple.coregraphics.GUISessionUserChanged",
];

/// A watch on notify(3) names: how many of its reads since it began found one
/// of them posted.
pub(crate) struct ConsoleWatch {
    /// One token a name; none when a registration failed.
    tokens: Vec<c_int>,
    /// The reads since the watch began that found a name posted.
    posts: AtomicU64,
}

impl ConsoleWatch {
    /// The watch on the window server's console-session names, from now.
    pub(crate) fn console() -> Self {
        Self::on(&CONSOLE_SESSION_NAMES)
    }

    /// A watch on `names`, from now: each registered, and its first check,
    /// which always says posted, taken here. A name that cannot be
    /// registered leaves no watch at all, said once: each read is then
    /// `None`, and the panel's gate leaves the connection to decide.
    pub(crate) fn on(names: &[&CStr]) -> Self {
        let mut tokens = Vec::with_capacity(names.len());
        for name in names {
            let mut token: c_int = 0;
            // SAFETY: `name` is NUL-terminated and outlives the call, and
            // `token` is a valid place for the token.
            let status = unsafe { notify_register_check(name.as_ptr(), &mut token) };
            if status != NOTIFY_STATUS_OK {
                tracing::warn!(
                    name = %name.to_string_lossy(),
                    status,
                    "macOS consent panel: cannot watch the console session (notify_register_check \
                     refused), so a log-out shows as a lost window-server connection only (#4053)"
                );
                for token in tokens {
                    // SAFETY: a token this watch registered, let go once.
                    unsafe { notify_cancel(token) };
                }
                return Self {
                    tokens: Vec::new(),
                    posts: AtomicU64::new(0),
                };
            }
            let mut first: c_int = 0;
            // SAFETY: a token just registered, and a valid place for the answer.
            unsafe { notify_check(token, &mut first) };
            tokens.push(token);
        }
        Self {
            tokens,
            posts: AtomicU64::new(0),
        }
    }

    /// The console session now: how many reads since the watch began found a
    /// name posted, so `Some(0)` until another session starts. `None` with
    /// no watch, or when a check fails; a post that read found is counted
    /// all the same, for the next read to show.
    pub(crate) fn session(&self) -> Option<u64> {
        if self.tokens.is_empty() {
            return None;
        }
        let (mut posted, mut failed) = (false, false);
        for &token in &self.tokens {
            let mut check: c_int = 0;
            // SAFETY: a token this watch registered and has not let go, and a
            // valid place for the answer.
            if unsafe { notify_check(token, &mut check) } == NOTIFY_STATUS_OK {
                posted |= check != 0;
            } else {
                failed = true;
            }
        }
        let posts = if posted {
            self.posts.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            self.posts.load(Ordering::SeqCst)
        };
        (!failed).then_some(posts)
    }
}

impl Drop for ConsoleWatch {
    fn drop(&mut self) {
        for &token in &self.tokens {
            // SAFETY: a token this watch registered, let go once.
            unsafe { notify_cancel(token) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    unsafe extern "C" {
        /// notify(3): `name` is posted to every process that watches it.
        fn notify_post(name: *const c_char) -> u32;
    }

    /// The reader on a real notify(3), with a name of this test's own, never
    /// the window server's: the watch reads `Some(0)` until its name is
    /// posted, then `Some(1)`, and a read after that with no post since stays
    /// `Some(1)`. The window server's two names are read the same way; which
    /// of a Mac's events post them is the release lab's cell.
    #[test]
    fn a_post_after_the_watch_began_reads_as_another_session_once() {
        let name = std::ffi::CString::new(format!(
            "live.roomler.test.console-watch.{}",
            std::process::id()
        ))
        .unwrap();
        let watch = ConsoleWatch::on(&[name.as_c_str()]);
        assert_eq!(watch.session(), Some(0), "the first check was not taken");
        assert_eq!(watch.session(), Some(0));
        // SAFETY: NUL-terminated, and it outlives the call.
        assert_eq!(unsafe { notify_post(name.as_ptr()) }, NOTIFY_STATUS_OK);
        // notifyd takes the post; wait for it to land, 5 s at most.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut read = watch.session();
        while read == Some(0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            read = watch.session();
        }
        assert_eq!(read, Some(1), "the post was not seen");
        assert_eq!(watch.session(), Some(1), "one post counted twice");
    }

    /// The window server's own two names register on a Mac, and a watch on
    /// them reads `Some(0)`: no other console session starts while the test
    /// runs. Registering posts nothing.
    #[test]
    fn the_window_servers_names_register_and_read_as_the_same_session() {
        let watch = ConsoleWatch::console();
        assert_eq!(watch.tokens.len(), CONSOLE_SESSION_NAMES.len());
        assert_eq!(watch.session(), Some(0));
    }
}
