// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-45 P3a — reach PipeWire without linking it.
//!
//! [P2b](super::screencast) ends with a PipeWire node id and a connection fd
//! the portal handed us. This is the step that uses that fd — and the step the
//! whole FR has been shaped around, because *how* we reach `libpipewire`
//! decides whether the daemon still starts on hosts that will never run a
//! portal.
//!
//! ## Why `dlopen`, spelled out once
//!
//! Linking `libpipewire` puts it in `roomlerd`'s `DT_NEEDED` on **every** Linux
//! build. A missing `.so` there does not degrade a feature — the loader refuses
//! to start the process at all. Cluster nodes, containers and headless fleet
//! hosts would carry it for nothing and break if it went missing. This project
//! has already paid for that once, when vendored FFmpeg dylibs baked a Homebrew
//! path into the macOS agent and dyld killed it at launch on every end-user
//! Mac.
//!
//! ⚠️ **A helper subcommand does not fix this.** `roomlerd portal-helper` is the
//! same ELF as `roomlerd`; linking for the helper's sake links for everyone.
//! The subcommand bought the *session context* (P2a); only `dlopen` buys the
//! *linkage*. It would have been easy to reach here believing P2a settled it.
//!
//! So: resolved at runtime, and every failure is a status this backend reports
//! rather than a condition anything else has to survive.
//!
//! ## What this phase does and does not prove
//!
//! It loads the library, initialises PipeWire, builds a context, and connects
//! **the portal's fd** to it. A successful connect means the socket was
//! accepted and a core proxy exists — the fd is real and usable.
//!
//! ⚠️ It does **not** prove frames flow. Format negotiation (SPA PODs) and
//! buffer delivery are P3b, and claiming otherwise from a non-null pointer
//! would be exactly the kind of unfalsifiable "it works" this FR keeps
//! rejecting.
//!
//! ## Dependencies added: none
//!
//! `libc` was already in the graph. There is no `pipewire` crate here on
//! purpose — adding one would link the library and undo the whole point.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::Arc;

/// Sonames to try, most specific first.
///
/// The versioned soname is what packages actually install; the bare `.so` is
/// the `-devel` symlink and is often absent on a runtime-only host. Trying
/// both costs nothing and covers distros that ship only one.
const CANDIDATES: &[&str] = &["libpipewire-0.3.so.0", "libpipewire-0.3.so"];

/// What came of trying to reach PipeWire with the portal's fd.
///
/// Reported rather than logged, because the three cases need different things
/// done about them and a bool would flatten them: *not tried* (the portal gave
/// us no fd), *connected*, and *the library is missing or refused us*.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PipeWireStatus {
    /// No fd to try — the handshake did not get that far.
    NotAttempted,
    /// The portal's fd is live PipeWire. ⚠️ This does **not** mean frames
    /// flow; format negotiation and buffers are P3b.
    Connected {
        library_version: String,
    },
    Failed(String),
}

impl std::fmt::Display for PipeWireStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipeWireStatus::NotAttempted => write!(f, "not attempted (no fd)"),
            PipeWireStatus::Connected { library_version } => {
                write!(f, "connected (libpipewire {library_version})")
            }
            PipeWireStatus::Failed(why) => write!(f, "unavailable — {why}"),
        }
    }
}

/// Try the whole thing: load the library, connect the fd, drop it again.
///
/// P3a keeps nothing: proving the fd is live PipeWire is the deliverable, and
/// holding a stream open would be P3b pretending to be finished.
pub fn probe(fd: std::os::fd::OwnedFd) -> PipeWireStatus {
    let lib = match Lib::load() {
        Ok(l) => l,
        Err(e) => return PipeWireStatus::Failed(e.to_string()),
    };
    match Connection::open(lib, fd) {
        Ok(conn) => PipeWireStatus::Connected {
            library_version: conn.library_version(),
        },
        Err(e) => PipeWireStatus::Failed(e.to_string()),
    }
}

/// Why PipeWire could not be reached. Every arm is reportable, because on a
/// host with no PipeWire this backend being unavailable is the *correct*
/// outcome, not a fault.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PwError {
    /// No candidate soname could be loaded. Carries what was tried, so the
    /// reader is not left guessing which name to install.
    NotFound {
        tried: Vec<String>,
        last: String,
    },
    /// The library loaded but is missing a symbol we need — a PipeWire too old
    /// or too new. Named, because "PipeWire failed" would send someone to the
    /// wrong place entirely.
    MissingSymbol(String),
    /// `pw_context_new` failed.
    NoContext,
    /// `pw_context_connect_fd` refused the portal's fd.
    ConnectFailed(String),
    Other(String),
}

impl std::fmt::Display for PwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PwError::NotFound { tried, last } => write!(
                f,
                "libpipewire not present (tried {}): {last}",
                tried.join(", ")
            ),
            PwError::MissingSymbol(s) => {
                write!(f, "libpipewire is missing {s} — unexpected version")
            }
            PwError::NoContext => write!(f, "pw_context_new returned null"),
            PwError::ConnectFailed(e) => {
                write!(f, "pw_context_connect_fd refused the portal fd: {e}")
            }
            PwError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Declare the entry points once: field name **is** the C symbol name, and the
/// type is used both for the field and for the `transmute`.
///
/// 🔑 Worth the macro. Written by hand, a symbol string and its field can
/// drift apart — you resolve `"pw_context_connect"` into the field you call as
/// `pw_context_connect_fd` and get a signature mismatch at runtime, in FFI,
/// where the failure is memory corruption rather than an error. Here they are
/// the same token, so they cannot disagree.
macro_rules! pw_syms {
    ($($name:ident: $t:ty,)*) => {
        /// The resolved entry points.
        ///
        /// Only what P3a needs. Stream creation and SPA POD building arrive in
        /// P3b; resolving symbols we do not call yet would make a *newer*
        /// PipeWire that dropped one of them look broken here for no reason.
        struct Syms { $($name: $t,)* }

        impl Syms {
            /// # Safety
            /// `handle` must be a live `dlopen` handle for libpipewire.
            unsafe fn resolve(handle: *mut c_void) -> Result<Self, PwError> {
                Ok(Self { $($name: {
                    let c = CString::new(stringify!($name)).expect("ident has no NUL");
                    let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
                    if p.is_null() {
                        return Err(PwError::MissingSymbol(stringify!($name).into()));
                    }
                    // SAFETY: the signature is the one libpipewire declares
                    // for this symbol, transcribed from its headers. RTLD_NOW
                    // has already proven the symbol exists in the library we
                    // actually loaded.
                    unsafe { std::mem::transmute::<*mut c_void, $t>(p) }
                },)* })
            }
        }
    };
}

pw_syms! {
    pw_init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    pw_get_library_version: unsafe extern "C" fn() -> *const c_char,
    pw_main_loop_new: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    pw_main_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pw_main_loop_destroy: unsafe extern "C" fn(*mut c_void),
    pw_context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    pw_context_destroy: unsafe extern "C" fn(*mut c_void),
    pw_context_connect_fd:
        unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    pw_core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
}

/// A loaded `libpipewire`.
///
/// ⚠️ The handle is **never `dlclose`d**. PipeWire keeps global state and
/// spawns threads; unloading it underneath them is a crash waiting for a
/// quiet moment. Leaking one handle for the life of a process that has
/// decided to use PipeWire is the cheap, correct trade — the same reason
/// nobody `dlclose`s a plugin host.
pub struct Lib {
    syms: Syms,
}

// SAFETY: `Syms` holds only function pointers into a library that is never
// unloaded, so sharing them across threads is sound. PipeWire's own objects
// are NOT thread-safe and are confined to `Connection`, which is not `Send`.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

impl Lib {
    /// Load PipeWire, or say precisely why not.
    pub fn load() -> Result<Arc<Self>, PwError> {
        Self::load_from(CANDIDATES)
    }

    /// The load, parameterised over candidate names so the not-found path can
    /// be tested without uninstalling anything.
    pub fn load_from(candidates: &[&str]) -> Result<Arc<Self>, PwError> {
        let mut tried = Vec::new();
        let mut last = String::from("no candidates given");
        for name in candidates {
            tried.push((*name).to_string());
            let Ok(cname) = CString::new(*name) else {
                last = format!("{name}: not a valid library name");
                continue;
            };
            // RTLD_NOW so a missing symbol surfaces here rather than at the
            // first call, and RTLD_LOCAL so PipeWire's symbols cannot leak
            // into anything else this process later loads.
            let handle = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                last = dlerror_string().unwrap_or_else(|| format!("{name}: dlopen failed"));
                continue;
            }
            return Ok(Arc::new(Self {
                syms: unsafe { Syms::resolve(handle) }?,
            }));
        }
        Err(PwError::NotFound { tried, last })
    }

    pub fn version(&self) -> String {
        let p = unsafe { (self.syms.pw_get_library_version)() };
        if p.is_null() {
            return "unknown".into();
        }
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Deliberately terse: the interesting fact about a loaded library is which
/// version it is, and the resolved pointers are noise in any log line.
impl std::fmt::Debug for Lib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lib")
            .field("version", &self.version())
            .finish()
    }
}

fn dlerror_string() -> Option<String> {
    let p = unsafe { libc::dlerror() };
    if p.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// A live connection to the session's PipeWire, made through the portal's fd.
///
/// Not `Send`: PipeWire's loop and core belong to the thread that made them.
pub struct Connection {
    lib: Arc<Lib>,
    main_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
}

impl Connection {
    /// Connect the portal's fd to PipeWire.
    ///
    /// ⚠️ **`pw_context_connect_fd` takes ownership of the fd** and closes it
    /// on disconnect, so the fd is handed over as a raw descriptor and must
    /// not be closed here. Passing a borrowed fd and letting Rust close it too
    /// is a double close — which on a busy process closes *somebody else's*
    /// descriptor, the ugliest class of bug to chase.
    pub fn open(lib: Arc<Lib>, fd: std::os::fd::OwnedFd) -> Result<Self, PwError> {
        use std::os::fd::IntoRawFd;

        // `pw_init` is process-global and must run once. Running it per
        // connection is documented as harmless but pointless; running it never
        // leaves the type system uninitialised.
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| unsafe {
            (lib.syms.pw_init)(std::ptr::null_mut(), std::ptr::null_mut());
        });

        let main_loop = unsafe { (lib.syms.pw_main_loop_new)(std::ptr::null()) };
        if main_loop.is_null() {
            return Err(PwError::Other("pw_main_loop_new returned null".into()));
        }
        let loop_ = unsafe { (lib.syms.pw_main_loop_get_loop)(main_loop) };
        let context = unsafe { (lib.syms.pw_context_new)(loop_, std::ptr::null_mut(), 0) };
        if context.is_null() {
            unsafe { (lib.syms.pw_main_loop_destroy)(main_loop) };
            return Err(PwError::NoContext);
        }

        let raw = fd.into_raw_fd();
        let core =
            unsafe { (lib.syms.pw_context_connect_fd)(context, raw, std::ptr::null_mut(), 0) };
        if core.is_null() {
            let why = std::io::Error::last_os_error().to_string();
            // The fd's ownership only transfers on success, so close it here.
            unsafe { libc::close(raw) };
            unsafe { (lib.syms.pw_context_destroy)(context) };
            unsafe { (lib.syms.pw_main_loop_destroy)(main_loop) };
            return Err(PwError::ConnectFailed(why));
        }

        Ok(Self {
            lib,
            main_loop,
            context,
            core,
        })
    }

    pub fn library_version(&self) -> String {
        self.lib.version()
    }
}

impl Drop for Connection {
    /// ⚠️ Order is load-bearing and is the reverse of construction: the core
    /// is a proxy inside the context, and the context runs on the loop.
    /// Destroying the loop first frees memory the other two still reference.
    fn drop(&mut self) {
        unsafe {
            (self.lib.syms.pw_core_disconnect)(self.core);
            (self.lib.syms.pw_context_destroy)(self.context);
            (self.lib.syms.pw_main_loop_destroy)(self.main_loop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with no PipeWire must get an answer that names what was looked
    /// for. "PipeWire unavailable" sends an operator hunting; a list of
    /// sonames is a next step.
    #[test]
    fn a_missing_library_names_what_was_tried() {
        let err = Lib::load_from(&["libdefinitely-not-here-xyz.so.9"]).unwrap_err();
        match &err {
            PwError::NotFound { tried, .. } => {
                assert_eq!(tried, &["libdefinitely-not-here-xyz.so.9"]);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("libdefinitely-not-here-xyz.so.9"), "{msg}");
    }

    /// An empty candidate list must not silently look like success.
    #[test]
    fn no_candidates_is_an_error_not_a_load() {
        assert!(matches!(Lib::load_from(&[]), Err(PwError::NotFound { .. })));
    }

    /// The versioned soname has to come first: it is what packages install,
    /// while the bare `.so` is a `-devel` symlink that is usually absent on a
    /// runtime-only host. Reversing these would work on developer machines and
    /// fail across the fleet — the worst possible way round.
    #[test]
    fn the_versioned_soname_is_tried_first() {
        assert_eq!(CANDIDATES[0], "libpipewire-0.3.so.0");
        assert!(CANDIDATES.contains(&"libpipewire-0.3.so"));
    }

    /// Every failure must be reportable rather than fatal — on a host with no
    /// PipeWire, this backend being unavailable is the correct outcome.
    #[test]
    fn every_error_says_something_useful() {
        let all = [
            PwError::NotFound {
                tried: vec!["a".into()],
                last: "no such file".into(),
            },
            PwError::MissingSymbol("pw_init".into()),
            PwError::NoContext,
            PwError::ConnectFailed("EBADF".into()),
            PwError::Other("x".into()),
        ];
        for e in all {
            assert!(!e.to_string().is_empty(), "{e:?} has no message");
        }
    }
}
