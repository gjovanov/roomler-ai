// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-45 P2b — open a ScreenCast session through the desktop portal.
//!
//! [P2a](super::helper) established that the daemon can *reach* the portal, by
//! running a helper inside the console user's session. This is what that helper
//! goes on to do: the four-step ScreenCast handshake, ending with a PipeWire
//! node id and a connection fd.
//!
//! ## The Request/Response pattern, and the one way to get it wrong
//!
//! Portal methods do not return their result. They return the object path of a
//! `Request`, and the answer arrives later as a `Response` **signal** on that
//! path — because a portal call can take as long as a human takes to read a
//! dialog.
//!
//! ⚠️ The subscription must be armed **before** the method is called. The
//! request path is derivable in advance (that is why `handle_token` exists),
//! and arming afterwards is a race the portal wins whenever it answers without
//! asking anyone — exactly the restore-token case this phase depends on.
//!
//! ## 🔑 The PipeWire fd stays HERE
//!
//! The FR-45 plan originally had this phase pass the fd back to the daemon
//! over `SCM_RIGHTS`, inherited from FR-36's design, while also saying P3 would
//! consume PipeWire *inside the helper*. Those cannot both be true, so the
//! question got decided here rather than left to P3:
//!
//! **The helper consumes PipeWire; the fd never crosses to the daemon.**
//!
//! 1. *Fault isolation.* This project already paid for putting third-party
//!    driver code in the daemon's address space: a vendor probe that faulted
//!    took `roomlerd` down and the service manager restarted it straight back
//!    into the same fault — a crash-loop, not a degraded agent (`encode::caps`
//!    is a child process for that reason). `libpipewire` loads SPA plugins and
//!    sits on the same vendor GPU stacks. A fault there should cost capture,
//!    not the daemon.
//!  2. *It is not the slower choice.* PipeWire negotiates a small **pool** of
//!    buffers (dmabuf or memfd). Those fds cross once, at negotiation; per
//!    frame only "buffer N is ready" flows. Frames crossing a process boundary
//!    does not mean pixels being copied.
//! 3. *The helper already runs as the session's user*, whose PipeWire this is.
//!
//! So `SCM_RIGHTS` still appears in FR-45, but in P3 and carrying **buffer**
//! fds — not here carrying the connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use zbus::zvariant::{OwnedValue, Value};

use super::{PORTAL_BUS, PORTAL_PATH, SCREENCAST_IFACE};

const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

/// `SelectSources.types` — monitors only.
///
/// ⚠️ Deliberately NOT `VIRTUAL` (4). A virtual source asks the compositor to
/// *create a new monitor*, which changes the desktop the operator is looking
/// at. That may become a real option for headless hosts, but it must be an
/// explicit choice, never a default that silently rearranges someone's screen.
const SOURCE_MONITOR: u32 = 1;

/// `SelectSources.cursor_mode` values.
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;

/// `SelectSources.persist_mode` — persist until the user revokes it.
///
/// This is what makes the phase testable without a human in the loop twice:
/// the first `Start` prompts and hands back a `restore_token`; a later start
/// carrying that token is answered without a dialog.
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// Portal response codes.
const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;

/// One screencast stream the portal handed us.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StreamInfo {
    /// The PipeWire node to attach to. The whole point of the handshake.
    pub node_id: u32,
    /// Advertised size, when the portal gives one. `None` is not an error —
    /// the authoritative size comes from PipeWire's format negotiation in P3,
    /// and treating this as authoritative would bake in a guess.
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub source_type: Option<u32>,
}

/// What the helper reports back about a session it opened.
///
/// Serialisable because it crosses the helper→daemon process boundary. The fd
/// deliberately is not in here; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionReport {
    pub streams: Vec<StreamInfo>,
    /// Whether a grant was persisted for next time — **not the token itself**.
    ///
    /// ⚠️ The token deliberately does not travel in this struct. It is a
    /// standing grant to screencast someone's desktop without asking again,
    /// the child both stores and reloads it, and the daemon has no use for it.
    /// Carrying it here would have put a credential on a pipe and into every
    /// future `Debug` of this type for no purpose — the first version of this
    /// struct did exactly that while its own docs claimed the daemon never
    /// sees it. `false` means the portal declined to persist the grant, which
    /// is its right, and costs one dialog next time.
    pub restore_token_stored: bool,
    /// Whether `OpenPipeWireRemote` actually yielded a usable fd. Reported
    /// rather than assumed: everything up to it can succeed and still leave
    /// nothing to connect to.
    pub pipewire_fd_ok: bool,
    /// Whether we SENT a restore token. Paired with `elapsed_ms` this is what
    /// makes "did it prompt?" answerable: a restored session returns in
    /// milliseconds, a prompted one takes as long as a human takes.
    pub restore_token_sent: bool,
    pub elapsed_ms: u64,
    /// What the portal says it can do, for the log. Cheap, and the difference
    /// between "we chose hidden" and "hidden was all it had".
    pub cursor_mode_used: u32,
    pub available_cursor_modes: Option<u32>,
    pub available_source_types: Option<u32>,
}

/// Why a session could not be opened. Separated from a bare string because
/// **cancelled is not a failure** — it is a person saying no, and the caller
/// must not retry it as though it were a transient fault.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpenError {
    /// The human declined, or the dialog was dismissed.
    Cancelled,
    /// The portal ended the request for its own reasons.
    Ended,
    /// Anything else, with the detail.
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Cancelled => write!(f, "the person at the screen declined the request"),
            OpenError::Ended => write!(f, "the portal ended the request"),
            OpenError::Failed(why) => write!(f, "{why}"),
        }
    }
}

impl OpenError {
    fn failed(why: impl std::fmt::Display) -> Self {
        OpenError::Failed(why.to_string())
    }
}

/// Run the handshake.
///
/// Blocking, and it can block for as long as a human takes to answer a dialog
/// — the caller bounds it, not this function.
///
/// ⚠️ **Runs on its own thread, and that is not optional.** `zbus`'s blocking
/// API panics when called from inside a tokio runtime, and `portal-helper` is
/// dispatched from `#[tokio::main]`. [`super::detect`] already carried this
/// guard and this function was written without it, which cost a field run:
/// *"Cannot start a runtime from within a runtime"*, before any portal call
/// was made. The guard belongs HERE rather than at each call site, so the next
/// entry point cannot reintroduce it.
pub fn open() -> Result<SessionReport, OpenError> {
    std::thread::spawn(open_blocking)
        .join()
        .unwrap_or_else(|_| Err(OpenError::failed("the portal handshake thread panicked")))
}

fn open_blocking() -> Result<SessionReport, OpenError> {
    let started = Instant::now();
    // 🔑 The token is loaded and stored HERE, so it never leaves this module.
    // A caller that cannot hold the credential cannot leak it — which is a
    // stronger guarantee than a caller that holds it and is careful.
    let store = TokenStore::for_current_user();
    let restore_token = store.load();
    let restore_token = restore_token.as_deref();
    let restore_token_sent = restore_token.is_some();
    let conn = zbus::blocking::Connection::session()
        .map_err(|e| OpenError::failed(format!("session bus: {e}")))?;
    let portal = zbus::blocking::Proxy::new(&conn, PORTAL_BUS, PORTAL_PATH, SCREENCAST_IFACE)
        .map_err(|e| OpenError::failed(format!("portal proxy: {e}")))?;

    let available_cursor_modes = portal.get_property::<u32>("AvailableCursorModes").ok();
    let available_source_types = portal.get_property::<u32>("AvailableSourceTypes").ok();

    // Ask for an embedded cursor when the portal offers one — matching what
    // the DRM backend produces — and fall back rather than failing, since a
    // stream with no pointer still beats no stream.
    let cursor_mode = match available_cursor_modes {
        Some(m) if m & CURSOR_EMBEDDED != 0 => CURSOR_EMBEDDED,
        _ => CURSOR_HIDDEN,
    };

    // ── 1. CreateSession ────────────────────────────────────────────────
    let (req_token, sess_token) = (next_token("cs"), next_token("ss"));
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("handle_token", Value::from(req_token.as_str()));
    opts.insert("session_handle_token", Value::from(sess_token.as_str()));
    let results = call_with_response(&conn, &portal, "CreateSession", &(opts,), &req_token)?;
    let session_handle = results
        .get("session_handle")
        .and_then(string_of)
        .ok_or_else(|| OpenError::failed("CreateSession returned no session_handle"))?;

    // ── 2. SelectSources ────────────────────────────────────────────────
    let req_token = next_token("sel");
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("handle_token", Value::from(req_token.as_str()));
    opts.insert("types", Value::from(SOURCE_MONITOR));
    opts.insert("multiple", Value::from(false));
    opts.insert("cursor_mode", Value::from(cursor_mode));
    opts.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
    if let Some(tok) = restore_token {
        opts.insert("restore_token", Value::from(tok));
    }
    // ⚠️ `session_handle` is an object path on the wire, not a string. Sending
    // it as `s` is accepted by serde and rejected by the portal with a
    // signature mismatch that names neither the argument nor the call.
    let sess_path = object_path(&session_handle)?;
    call_with_response(
        &conn,
        &portal,
        "SelectSources",
        &(&sess_path, opts),
        &req_token,
    )?;

    // ── 3. Start — this is the step that can show a dialog ───────────────
    let req_token = next_token("start");
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("handle_token", Value::from(req_token.as_str()));
    let results = call_with_response(&conn, &portal, "Start", &(&sess_path, "", opts), &req_token)?;

    let streams = results
        .get("streams")
        .map(parse_streams)
        .transpose()?
        .unwrap_or_default();
    if streams.is_empty() {
        return Err(OpenError::failed(
            "the portal granted the request but returned no streams",
        ));
    }
    // Persisted immediately, before anything is reported: a caller that reads
    // the report and re-runs must find the token already on disk, or the
    // second run prompts again and the whole point of persisting is lost.
    let restore_token_stored = match results.get("restore_token").and_then(string_of) {
        Some(tok) => match store.save(&tok) {
            Ok(()) => true,
            Err(e) => {
                // Not fatal: the session is open and usable. It costs one
                // dialog next time, and saying so beats silence.
                tracing::warn!(%e, "portal: could not persist the restore token");
                false
            }
        },
        None => false,
    };

    // ── 4. OpenPipeWireRemote — a direct call, NOT a Request ────────────
    let pipewire_fd_ok = match portal.call_method(
        "OpenPipeWireRemote",
        &(&sess_path, HashMap::<&str, Value>::new()),
    ) {
        Ok(msg) => match msg.body().deserialize::<zbus::zvariant::OwnedFd>() {
            Ok(fd) => {
                // Held only to prove the chain works, then dropped: P2b opens
                // no stream. P3 is where this fd starts a PipeWire connection.
                let fd: std::os::fd::OwnedFd = fd.into();
                drop(fd);
                true
            }
            Err(e) => {
                tracing::warn!(%e, "portal: OpenPipeWireRemote returned no usable fd");
                false
            }
        },
        Err(e) => {
            tracing::warn!(%e, "portal: OpenPipeWireRemote failed");
            false
        }
    };

    Ok(SessionReport {
        streams,
        restore_token_stored,
        pipewire_fd_ok,
        restore_token_sent,
        elapsed_ms: started.elapsed().as_millis() as u64,
        cursor_mode_used: cursor_mode,
        available_cursor_modes,
        available_source_types,
    })
}

/// Where the portal's `restore_token` lives between runs.
///
/// ⚠️ **In the USER's state directory, never the daemon's config.** The token
/// is a standing grant to screencast this person's desktop without asking
/// again — it is theirs, it is scoped to their session, and a root-owned
/// host-global config file is the wrong shape for it in every respect. The
/// helper already runs as them, so it reads and writes this itself and the
/// token never reaches the daemon at all.
///
/// ⚠️ Mode 0600, and the directory 0700, for the same reason `config.toml` is:
/// anything that can read it can re-open the grant.
pub struct TokenStore {
    path: std::path::PathBuf,
}

impl TokenStore {
    /// `$XDG_STATE_HOME/roomler/portal-restore-token`, falling back to the
    /// spec's default of `$HOME/.local/state`.
    pub fn for_current_user() -> Self {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".local").join("state"))
            })
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        Self {
            path: base.join("roomler").join("portal-restore-token"),
        }
    }

    /// A store at an explicit path. Exists so the behaviour can be tested
    /// without mutating `XDG_STATE_HOME`, which is process-global and would
    /// make the test order-dependent against every other test in the binary.
    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// The stored token, or `None` — including when the file is absent, empty
    /// or unreadable. A missing token is not an error; it costs one dialog.
    pub fn load(&self) -> Option<String> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let tok = raw.trim();
        (!tok.is_empty()).then(|| tok.to_string())
    }

    pub fn save(&self, token: &str) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        // Written through a temp file and renamed: a half-written token would
        // be silently useless (one surprise dialog), and the atomic swap costs
        // nothing here.
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }
}

// ── the Request/Response plumbing ───────────────────────────────────────

/// Arm the `Response` subscription, make the call, wait for the answer.
///
/// ⚠️ The order inside is the whole point: subscribe, *then* call. See the
/// module docs — a portal that answers without asking a human (the restore
/// path) can respond before a subscription made afterwards exists.
fn call_with_response<B>(
    conn: &zbus::blocking::Connection,
    portal: &zbus::blocking::Proxy<'_>,
    method: &str,
    body: &B,
    handle_token: &str,
) -> Result<HashMap<String, OwnedValue>, OpenError>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    let path = request_path(conn, handle_token)?;
    let req = zbus::blocking::Proxy::new(conn, PORTAL_BUS, path.as_str(), REQUEST_IFACE)
        .map_err(|e| OpenError::failed(format!("request proxy for {method}: {e}")))?;
    let mut signals = req
        .receive_signal("Response")
        .map_err(|e| OpenError::failed(format!("subscribing to {method} response: {e}")))?;

    portal
        .call_method(method, body)
        .map_err(|e| OpenError::failed(format!("{method}: {e}")))?;

    let msg = signals
        .next()
        .ok_or_else(|| OpenError::failed(format!("{method}: the response stream ended")))?;
    let (code, results) = msg
        .body()
        .deserialize::<(u32, HashMap<String, OwnedValue>)>()
        .map_err(|e| OpenError::failed(format!("{method} response body: {e}")))?;

    match code {
        RESPONSE_OK => Ok(results),
        RESPONSE_CANCELLED => Err(OpenError::Cancelled),
        _ => Err(OpenError::Ended),
    }
}

/// Where the portal will publish the `Response` for `token`.
///
/// Derived, not discovered, because it has to be known *before* the call.
fn request_path(conn: &zbus::blocking::Connection, token: &str) -> Result<String, OpenError> {
    let unique = conn
        .unique_name()
        .ok_or_else(|| OpenError::failed("the session connection has no unique name"))?;
    Ok(request_path_for(unique.as_str(), token))
}

/// The path-mangling rule from the portal spec, split out so it can be tested
/// without a bus: strip the leading `:` and turn every `.` into `_`.
fn request_path_for(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

/// A fresh `handle_token`. Must be unique per call and a legal D-Bus path
/// element, so it is `[A-Za-z0-9_]` only — no uuid crate, no randomness
/// needed, since uniqueness is only required within this connection.
fn next_token(kind: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "roomler_{kind}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn object_path(s: &str) -> Result<zbus::zvariant::ObjectPath<'static>, OpenError> {
    zbus::zvariant::ObjectPath::try_from(s.to_string())
        .map_err(|e| OpenError::failed(format!("session handle {s:?} is not an object path: {e}")))
}

// ── value plucking ──────────────────────────────────────────────────────

/// A `String` out of an `a{sv}` value, whatever wrapping it arrived in.
fn string_of(v: &OwnedValue) -> Option<String> {
    match <&Value>::from(v) {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    }
}

/// `streams` is `a(ua{sv})`.
///
/// Parsed structurally rather than through a derive, because a portal that
/// adds a field to the stream dict must not turn into a failure to capture.
/// Anything unrecognised is skipped; only the node id is required.
fn parse_streams(v: &OwnedValue) -> Result<Vec<StreamInfo>, OpenError> {
    let Value::Array(arr) = <&Value>::from(v) else {
        return Err(OpenError::failed("streams was not an array"));
    };
    let mut out = Vec::new();
    for item in arr.inner() {
        let Value::Structure(s) = item else { continue };
        let fields = s.fields();
        let Some(Value::U32(node_id)) = fields.first() else {
            continue;
        };
        let (mut width, mut height, mut source_type) = (None, None, None);
        if let Some(Value::Dict(d)) = fields.get(1) {
            for (k, val) in d.iter() {
                let Value::Str(k) = k else { continue };
                // Values in an a{sv} arrive boxed in a variant.
                let val = match val {
                    Value::Value(inner) => inner.as_ref(),
                    other => other,
                };
                match k.as_str() {
                    "size" => {
                        if let Value::Structure(sz) = val {
                            let f = sz.fields();
                            if let (Some(Value::I32(w)), Some(Value::I32(h))) =
                                (f.first(), f.get(1))
                            {
                                width = Some(*w);
                                height = Some(*h);
                            }
                        }
                    }
                    "source_type" => {
                        if let Value::U32(t) = val {
                            source_type = Some(*t);
                        }
                    }
                    _ => {}
                }
            }
        }
        out.push(StreamInfo {
            node_id: *node_id,
            width,
            height,
            source_type,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mangling rule is in the portal spec and is not guessable from the
    /// result: get it wrong and the subscription sits on a path the portal
    /// never publishes to, so the call hangs forever instead of failing.
    #[test]
    fn request_path_strips_the_colon_and_replaces_dots() {
        assert_eq!(
            request_path_for(":1.284", "roomler_cs_9_0"),
            "/org/freedesktop/portal/desktop/request/1_284/roomler_cs_9_0"
        );
    }

    /// Tokens must be legal D-Bus path elements and must not repeat within a
    /// connection — two calls sharing one token would have their responses
    /// delivered to the same subscription.
    #[test]
    fn tokens_are_unique_and_path_safe() {
        let a = next_token("cs");
        let b = next_token("cs");
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert!(
                t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{t} is not a legal path element"
            );
        }
    }

    /// Build the `a{sv}` half of a stream entry.
    fn props(pairs: Vec<(&'static str, Value<'static>)>) -> zbus::zvariant::Dict<'static, 'static> {
        let mut d = zbus::zvariant::Dict::new(
            &zbus::zvariant::Signature::Str,
            &zbus::zvariant::Signature::Variant,
        );
        for (k, v) in pairs {
            // ⚠️ Boxed EXPLICITLY as a variant. `Value::from(some_value)` is
            // identity, not a box, so writing that here would build an a{sv}
            // whose values are bare — a shape the portal never sends, and the
            // test would then be checking a case that cannot occur.
            d.append(Value::from(k), Value::Value(Box::new(v))).unwrap();
        }
        d
    }

    /// `(ua{sv})`. Built with the builder because `Dict` is not `Type`, so the
    /// tuple `From` impl does not apply to it.
    fn stream_entry(node: u32, p: zbus::zvariant::Dict<'static, 'static>) -> Value<'static> {
        Value::from(
            zbus::zvariant::StructureBuilder::new()
                .add_field(node)
                .append_field(Value::from(p))
                .build()
                .unwrap(),
        )
    }

    fn dict_stream(node: u32, w: i32, h: i32) -> Value<'static> {
        stream_entry(
            node,
            props(vec![
                ("size", Value::from((w, h))),
                ("source_type", Value::from(1u32)),
            ]),
        )
    }

    /// Wrap entries as the portal's `a(ua{sv})`.
    ///
    /// ⚠️ NOT `Value::from(vec![…])`. That builds an `av` — an array of
    /// *variants* — so every element arrives boxed, which is a shape the
    /// portal never sends. The first draft of these tests did exactly that and
    /// failed against correct parsing code, which is the right way round: the
    /// element signature is taken from a real entry so the array can only have
    /// the shape the entries actually are.
    fn streams_value(entries: Vec<Value<'static>>) -> OwnedValue {
        let sig = entries
            .first()
            .expect("need at least one entry to take a signature from")
            .value_signature()
            .clone();
        let mut arr = zbus::zvariant::Array::new(&sig);
        for e in entries {
            arr.append(e).unwrap();
        }
        OwnedValue::try_from(Value::from(arr)).unwrap()
    }

    #[test]
    fn streams_are_parsed_out_of_the_portal_shape() {
        let owned = streams_value(vec![dict_stream(42, 1920, 1080)]);
        let got = parse_streams(&owned).unwrap();
        assert_eq!(
            got,
            vec![StreamInfo {
                node_id: 42,
                width: Some(1920),
                height: Some(1080),
                source_type: Some(1),
            }]
        );
    }

    /// A portal that grows a field must not cost us the capture. Only the node
    /// id is required; everything else is best-effort.
    #[test]
    fn an_unknown_stream_property_is_ignored_not_fatal() {
        let owned = streams_value(vec![stream_entry(
            7,
            props(vec![("something_new_in_2027", Value::from("surprise"))]),
        )]);
        let got = parse_streams(&owned).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].node_id, 7);
        assert_eq!(got[0].width, None);
    }

    /// The token is a standing grant to screencast someone's desktop without
    /// asking again. It round-trips, and it lands 0600 — anything that can
    /// read it can re-open the grant.
    #[test]
    fn the_restore_token_round_trips_and_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("roomler-portal-tok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = TokenStore::at(dir.join("roomler").join("portal-restore-token"));

        // Absent is not an error — it costs one dialog, nothing more.
        assert_eq!(store.load(), None);

        store.save("tok-abc123").unwrap();
        assert_eq!(store.load().as_deref(), Some("tok-abc123"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("roomler").join("portal-restore-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "restore token must not be readable by others");
        }

        // Overwriting must replace, not append — a concatenated token is a
        // silent failure that looks exactly like a revoked grant.
        store.save("tok-second").unwrap();
        assert_eq!(store.load().as_deref(), Some("tok-second"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty or whitespace-only file must read as "no token", not as a
    /// token that is the empty string — the portal would reject that and the
    /// failure would look like a revoked grant rather than a corrupt file.
    #[test]
    fn a_blank_token_file_reads_as_absent() {
        let dir = std::env::temp_dir().join(format!("roomler-portal-blank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("portal-restore-token");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(TokenStore::at(path).load(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cancelled has to stay distinguishable from a fault all the way out:
    /// a person saying no must never be retried as a transient error.
    #[test]
    fn cancelled_reads_as_a_person_not_a_fault() {
        assert!(OpenError::Cancelled.to_string().contains("declined"));
        assert_ne!(OpenError::Cancelled, OpenError::Ended);
    }
}
