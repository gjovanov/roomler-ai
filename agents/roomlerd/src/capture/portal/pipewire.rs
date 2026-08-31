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
    /// The portal's fd is live PipeWire, but no stream was opened. ⚠️ This
    /// does **not** mean frames flow — it is P3a's answer, kept because
    /// "reached the daemon" and "agreed a format" fail for different reasons
    /// and an operator needs to know which happened.
    Connected {
        library_version: String,
    },
    /// A stream connected and the compositor agreed a concrete format. ⚠️
    /// Still not frames: buffer delivery is P3c.
    Negotiated {
        library_version: String,
        format: NegotiatedFormat,
    },
    Failed(String),
}

impl std::fmt::Display for PipeWireStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipeWireStatus::NotAttempted => write!(f, "not attempted (no fd)"),
            PipeWireStatus::Connected { library_version } => {
                write!(f, "connected (libpipewire {library_version}), no stream")
            }
            PipeWireStatus::Negotiated {
                library_version,
                format,
            } => write!(f, "negotiated {format} (libpipewire {library_version})"),
            PipeWireStatus::Failed(why) => write!(f, "unavailable — {why}"),
        }
    }
}

/// P3a — load the library and connect the fd, keeping nothing.
///
/// Superseded by [`negotiate_status`] for the capture path, and kept because
/// it isolates one question: is the fd live PipeWire at all? When negotiation
/// fails, running this says whether the problem is the connection or the
/// format.
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

/// P3b-ii — connect a stream to the portal's node and report what the
/// compositor agreed to.
pub fn negotiate_status(fd: std::os::fd::OwnedFd, node_id: u32, max_fps: u32) -> PipeWireStatus {
    // Read before the fd moves: the version is worth reporting on both arms,
    // and by the time negotiation fails the library handle is gone.
    let library_version = match Lib::load() {
        Ok(l) => l.version(),
        Err(e) => return PipeWireStatus::Failed(e.to_string()),
    };
    match negotiate(fd, node_id, max_fps) {
        Ok(format) => PipeWireStatus::Negotiated {
            library_version,
            format,
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
    pw_main_loop_run: unsafe extern "C" fn(*mut c_void) -> c_int,
    pw_main_loop_quit: unsafe extern "C" fn(*mut c_void) -> c_int,
    pw_context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    pw_context_destroy: unsafe extern "C" fn(*mut c_void),
    pw_context_connect_fd:
        unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    pw_core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    // ⚠️ `_new_string`, not `pw_properties_new` — the latter is VARARGS, which
    // cannot be called through a plain `dlsym`'d function pointer without
    // pinning the exact argument list. This one takes "k=v k=v" and is a
    // normal C function.
    pw_properties_new_string: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    pw_stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    pw_stream_add_listener:
        unsafe extern "C" fn(*mut c_void, *mut c_void, *const PwStreamEvents, *mut c_void),
    pw_stream_connect: unsafe extern "C" fn(
        *mut c_void,
        c_int,
        u32,
        u32,
        *const *const c_void,
        u32,
    ) -> c_int,
    pw_stream_destroy: unsafe extern "C" fn(*mut c_void),
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

// ── P3b-ii: connect a stream and negotiate a format ─────────────────────

/// `struct pw_stream_events`, transcribed from `pipewire/stream.h`.
///
/// ⚠️ **Every field and its order is load-bearing.** This is a vtable the
/// library indexes by offset: one missing or reordered pointer means it calls
/// the wrong function with the wrong arguments. The list is complete as of
/// `PW_VERSION_STREAM_EVENTS = 2` — `version` tells the library how much of
/// this struct it may read, so declaring 2 while providing fewer fields would
/// invite it past the end.
#[repr(C)]
struct PwStreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// `struct spa_hook` — six pointers (`spa_list` × 2, `spa_callbacks` × 2,
/// `removed`, `priv`). Opaque to us: the library initialises and owns it, we
/// only have to provide storage that does not move and outlives the stream.
#[repr(C)]
struct SpaHook([usize; 6]);

/// `enum pw_stream_flags`, and the direction.
const PW_DIRECTION_INPUT: c_int = 0;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
/// `enum spa_param_type` — the *negotiated* format (not `EnumFormat`).
const SPA_PARAM_FORMAT: u32 = 4;

/// The format the compositor actually agreed to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NegotiatedFormat {
    /// `enum spa_video_format` — 8 is BGRx, 12 BGRA, 7 RGBx, 11 RGBA.
    pub video_format: u32,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_denom: u32,
}

impl std::fmt::Display for NegotiatedFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.video_format {
            7 => "RGBx",
            8 => "BGRx",
            11 => "RGBA",
            12 => "BGRA",
            other => return write!(f, "format#{other} {}x{}", self.width, self.height),
        };
        write!(
            f,
            "{name} {}x{} @ {}/{}",
            self.width, self.height, self.fps_num, self.fps_denom
        )
    }
}

/// Shared with the C callbacks. Reached only from the loop thread.
struct Shared {
    lib: Arc<Lib>,
    main_loop: *mut c_void,
    outcome: Option<Result<NegotiatedFormat, String>>,
    /// How many Format params arrived that could not be used yet. Reported so
    /// a timeout can say whether nothing came at all or several came and none
    /// fixated — different problems with different fixes.
    rejected: u32,
}

impl Shared {
    /// Record a verdict and stop the loop. Only the FIRST verdict counts: a
    /// stream can report an error after a good format, and overwriting a
    /// success with a late teardown message would lose the answer.
    fn finish(&mut self, r: Result<NegotiatedFormat, String>) {
        if self.outcome.is_none() {
            self.outcome = Some(r);
        }
        unsafe { (self.lib.syms.pw_main_loop_quit)(self.main_loop) };
    }
}

/// ⚠️ A panic must never cross back into C — that is undefined behaviour, not
/// a crash with a message. Every callback body goes through this.
fn guard(data: *mut c_void, f: impl FnOnce(&mut Shared)) {
    if data.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `data` is the `Shared` we handed to `pw_stream_add_listener`,
        // which outlives the stream, and PipeWire calls back only from the
        // loop thread, so there is no aliasing.
        let shared = unsafe { &mut *(data as *mut Shared) };
        f(shared)
    }));
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    guard(data, |shared| {
        // -1 is PW_STREAM_STATE_ERROR. Anything else is progress, and the
        // format arrives via param_changed rather than here.
        if state == -1 {
            let why = if error.is_null() {
                "the stream reported an error with no message".to_string()
            } else {
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            shared.finish(Err(why));
        }
    });
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    guard(data, |shared| {
        // A null param means "this parameter was cleared", and ids other than
        // Format are none of our business — neither is an error.
        if id != SPA_PARAM_FORMAT || param.is_null() {
            return;
        }
        let bytes = match unsafe { copy_pod(param) } {
            Some(b) => b,
            None => return shared.finish(Err("the format param was unreadable".into())),
        };
        match parse_format(&bytes) {
            Ok(f) => shared.finish(Ok(f)),
            // ⚠️ Do NOT finish here. `param_changed` can fire more than once,
            // and an early one may still carry choices where the final one
            // carries values — PipeWire fixates as it negotiates. Treating the
            // first Format as final turned a working stream into
            // "the negotiated video format is not a plain id" in the field.
            // Keep waiting; the deadline is the backstop.
            Err(why) => {
                shared.rejected += 1;
                eprintln!(
                    "portal-helper: format #{} not usable yet ({why}); contents: {}",
                    shared.rejected,
                    describe(&bytes)
                );
            }
        }
    });
}

/// A one-line summary of a format POD, for when it could not be used.
///
/// Exists because "not a plain id" on its own does not say *what* arrived, and
/// the alternative is another build-and-deploy cycle to find out.
fn describe(bytes: &[u8]) -> String {
    use super::pod::{ParsedValue, parse_object};
    match parse_object(bytes) {
        Err(e) => format!("<unparseable: {e}>"),
        Ok(o) => {
            let props: Vec<String> = o
                .props
                .iter()
                .map(|(k, v)| {
                    let v = match v {
                        ParsedValue::Id(x) => format!("Id({x})"),
                        ParsedValue::Int(x) => format!("Int({x})"),
                        ParsedValue::Rectangle { width, height } => format!("{width}x{height}"),
                        ParsedValue::Fraction { num, denom } => format!("{num}/{denom}"),
                        ParsedValue::Choice { kind, first } => {
                            format!("choice(kind={kind}, {first:?})")
                        }
                        ParsedValue::Unsupported { pod_type } => format!("pod-type#{pod_type}"),
                    };
                    format!("0x{k:x}={v}")
                })
                .collect();
            format!(
                "object#{:x} id={} [{}]",
                o.object_type,
                o.id,
                props.join(" ")
            )
        }
    }
}

/// Copy a POD out of the library's memory so it can be parsed safely.
///
/// ⚠️ The length comes from the data itself, so it is sanity-bounded before
/// being used to build a slice: a corrupt header must not become a read of
/// gigabytes from another process's heap.
///
/// # Safety
/// `p` must point to a readable `spa_pod` header.
unsafe fn copy_pod(p: *const c_void) -> Option<Vec<u8>> {
    const MAX_POD: usize = 1 << 20;
    let header = unsafe { std::slice::from_raw_parts(p as *const u8, 8) };
    let size = u32::from_ne_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if size > MAX_POD {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(p as *const u8, 8 + size) }.to_vec())
}

fn parse_format(bytes: &[u8]) -> Result<NegotiatedFormat, String> {
    use super::pod::{ParsedValue, parse_object, ty};
    let o = parse_object(bytes).map_err(|e| format!("parsing the negotiated format: {e}"))?;

    // ⚠️ Every lookup goes through `.fixed()`. SPA writes a settled value as a
    // `Choice(None)` holding one element — a negotiated format on GNOME 48
    // arrives with EVERY property wrapped that way, `mediaType` included. A
    // `Range` or `Enum` still means "not settled yet" and `.fixed()` returns
    // None, which keeps the caller waiting instead of reporting a default as
    // though it were agreed.
    let Some(ParsedValue::Id(video_format)) = o
        .get(ty::FORMAT_VIDEO_FORMAT)
        .ok_or("the negotiated format has no video format")?
        .fixed()
    else {
        return Err("the video format is not settled yet".into());
    };
    let Some(ParsedValue::Rectangle { width, height }) = o
        .get(ty::FORMAT_VIDEO_SIZE)
        .ok_or("the negotiated format has no size")?
        .fixed()
    else {
        return Err("the size is not settled yet".into());
    };
    let (video_format, width, height) = (*video_format, *width, *height);
    // Framerate is genuinely optional — some sources leave it unset rather
    // than committing to a rate — so its absence is 0/1, not a failure.
    let (fps_num, fps_denom) = match o.get(ty::FORMAT_VIDEO_FRAMERATE).and_then(|v| v.fixed()) {
        Some(ParsedValue::Fraction { num, denom }) => (*num, *denom),
        _ => (0, 1),
    };
    Ok(NegotiatedFormat {
        video_format,
        width,
        height,
        fps_num,
        fps_denom,
    })
}

/// Connect a stream to `node_id` and return the format the compositor agreed
/// to.
///
/// ⚠️ Runs the PipeWire loop on a **detached** thread and waits with a
/// deadline. `pw_main_loop_run` blocks, and `pw_main_loop_quit` is only safe
/// from the loop's own thread — so on a timeout the loop is left running and
/// the process is expected to exit shortly after reporting. That is acceptable
/// precisely because this is a short-lived helper; it would not be in the
/// daemon, which is one more reason the daemon does not do this.
pub fn negotiate(
    fd: std::os::fd::OwnedFd,
    node_id: u32,
    max_fps: u32,
) -> Result<NegotiatedFormat, PwError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = negotiate_blocking(fd, node_id, max_fps);
        // The receiver may already have timed out; nothing to do about that.
        let _ = tx.send(r);
    });
    rx.recv_timeout(NEGOTIATE_TIMEOUT)
        .unwrap_or_else(|_| Err(PwError::Other("format negotiation timed out".into())))
}

const NEGOTIATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn negotiate_blocking(
    fd: std::os::fd::OwnedFd,
    node_id: u32,
    max_fps: u32,
) -> Result<NegotiatedFormat, PwError> {
    let lib = Lib::load()?;
    let conn = Connection::open(lib.clone(), fd)?;

    let props_str = CString::new("media.type=Video media.category=Capture media.role=Screen")
        .map_err(|e| PwError::Other(e.to_string()))?;
    let props = unsafe { (lib.syms.pw_properties_new_string)(props_str.as_ptr()) };
    let name = CString::new("roomler-portal-capture").expect("literal has no NUL");
    // ⚠️ `pw_stream_new` TAKES OWNERSHIP of `props` — it must not be freed
    // here, on success or failure.
    let stream = unsafe { (lib.syms.pw_stream_new)(conn.core, name.as_ptr(), props) };
    if stream.is_null() {
        return Err(PwError::Other("pw_stream_new returned null".into()));
    }

    // These three must outlive the stream: the library keeps the pointers.
    let mut shared = Box::new(Shared {
        lib: lib.clone(),
        main_loop: conn.main_loop,
        outcome: None,
        rejected: 0,
    });
    let events = Box::new(PwStreamEvents {
        version: 2,
        destroy: None,
        state_changed: Some(on_state_changed),
        control_info: None,
        io_changed: None,
        param_changed: Some(on_param_changed),
        add_buffer: None,
        remove_buffer: None,
        process: None,
        drained: None,
        command: None,
        trigger_done: None,
    });
    let mut hook = Box::new(SpaHook([0; 6]));

    unsafe {
        (lib.syms.pw_stream_add_listener)(
            stream,
            &mut *hook as *mut SpaHook as *mut c_void,
            &*events,
            &mut *shared as *mut Shared as *mut c_void,
        );
    }

    let param = super::pod::video_enum_format(max_fps)
        .map_err(PwError::Other)?
        .to_pod();
    let params = [param.as_ptr() as *const c_void];
    let rc = unsafe {
        (lib.syms.pw_stream_connect)(
            stream,
            PW_DIRECTION_INPUT,
            node_id,
            PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
            params.as_ptr(),
            1,
        )
    };
    if rc < 0 {
        unsafe { (lib.syms.pw_stream_destroy)(stream) };
        return Err(PwError::Other(format!(
            "pw_stream_connect failed: {}",
            std::io::Error::from_raw_os_error(-rc)
        )));
    }

    // Blocks until a callback calls `pw_main_loop_quit`.
    unsafe { (lib.syms.pw_main_loop_run)(conn.main_loop) };

    let outcome = shared.outcome.take();
    unsafe { (lib.syms.pw_stream_destroy)(stream) };
    drop(conn);

    match outcome {
        Some(Ok(f)) => Ok(f),
        Some(Err(e)) => Err(PwError::Other(e)),
        None => Err(PwError::Other(
            "the PipeWire loop ended without negotiating a format".into(),
        )),
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
