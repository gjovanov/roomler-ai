//! Roomler **LocalAPI** — the local control surface (P1: read-only).
//!
//! The unified daemon (`roomlerd`) will expose this over a local-only channel
//! (named pipe on Windows / unix socket elsewhere; ACL-authenticated — wired in
//! P1-cont) so thin clients — the CLI (`roomler`) and the desktop app (Roomler)
//! — can read live
//! node / peer / flow state without reaching into the daemon's internals. This
//! module is the **transport-agnostic protocol**: the request/response wire
//! types plus a pure [`handle`] dispatch over a [`LocalApiState`] snapshot. The
//! pipe listener + the daemon's `LocalApiState` impl (gathering real overlay /
//! tunnel / forward state) land in P1-cont; keeping the protocol pure here makes
//! it unit-testable with a mock and reusable by both the daemon and clients.
//!
//! Wire shape: newline-delimited JSON, adjacently tagged (`{"t":<verb>}` /
//! `{"t":<verb>,"d":<payload>}`) so a payload may be a struct OR a sequence.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// How this node currently reaches a peer — the Tailscale-style connection
/// type shown per device in the UI. `Tunnel` is the userspace SOCKS/forward
/// path (used when a corp full-tunnel VPN captures the overlay's routes);
/// `Blocked` = a peer with no working carrier; `Offline` = not currently up.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Direct,
    Relay,
    Tunnel,
    Blocked,
    Offline,
}

/// Which privilege mode the daemon is running in.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    /// SYSTEM service — full node (can *be accessed* + *reach others*).
    Service,
    /// Unprivileged user session — *reach others* only, no admin.
    User,
}

/// Snapshot of the local node.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    pub node_id: String,
    pub name: String,
    pub version: String,
    pub mode: DaemonMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    /// Connected to the coordination server.
    pub connected: bool,
}

/// A peer device as this node currently sees it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub node_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    pub online: bool,
    pub connection: ConnectionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
}

/// Whether a forward is a static `--remote` forward or a SOCKS5 listener.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    Forward,
    Socks5,
}

/// One active forward / SOCKS5 listener with cumulative throughput. Sourced
/// from the per-flow `forward::FlowStats` the data plane already records.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FlowInfo {
    pub id: String,
    pub kind: FlowKind,
    pub local_addr: String,
    /// `host:port` for a static forward; `None` for a SOCKS5 listener (its
    /// target is chosen per connection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Peer node this forward reaches (name or id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub transport: String,
    pub active_flows: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// A LocalAPI request. P1 exposes read-only verbs; create/kill/consent verbs
/// (P2+) extend this enum. Adjacently tagged on `t`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Request {
    /// Local node status.
    Status,
    /// Peers with their current connection type.
    Peers,
    /// Active forwards / SOCKS5 listeners + throughput.
    Flows,
}

/// A LocalAPI response. Adjacently tagged so a payload may be a struct
/// (`Status`) or a sequence (`Peers` / `Flows`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Response {
    Status(NodeStatus),
    Peers(Vec<PeerInfo>),
    Flows(Vec<FlowInfo>),
    /// The verb couldn't be served (bad request, state unavailable).
    Error {
        message: String,
    },
}

/// The overlay runtime's live view of the mesh, republished on a
/// [`tokio::sync::watch`] channel whenever the netmap / carrier state changes
/// (see `overlay::runtime`). NOT a wire type — it's the daemon-internal bridge
/// between the overlay runtime (which owns `by_node` inside its single
/// `select!` loop and has no other external accessor) and the daemon's
/// [`LocalApiState`] impl, which turns it into [`Response::Status`]'s
/// `overlay_ip` + [`Response::Peers`]. Kept here (not under the overlay
/// feature) so a daemon compiled WITHOUT `overlay-l3` can still hold an empty
/// `Default` one and answer `peers` with `[]`.
#[derive(Debug, Clone, Default)]
pub struct OverlayView {
    /// This node's assigned overlay IP (the netmap `self_ip`), once joined.
    pub self_ip: Option<String>,
    /// Peers as the runtime currently reaches them.
    pub peers: Vec<PeerInfo>,
}

/// Read-only snapshot the daemon provides to [`handle`]. The daemon's impl
/// gathers this from its live overlay / tunnel / forward state; the trait keeps
/// the protocol unit-testable with a mock and free of daemon internals.
pub trait LocalApiState: Send + Sync {
    fn status(&self) -> NodeStatus;
    fn peers(&self) -> Vec<PeerInfo>;
    fn flows(&self) -> Vec<FlowInfo>;
}

/// Pure dispatch: map a [`Request`] to a [`Response`] over a state snapshot.
/// No I/O — the pipe listener (P1-cont) reads a JSON line, deserialises a
/// [`Request`], calls this, and writes the [`Response`] back.
pub fn handle(req: &Request, state: &dyn LocalApiState) -> Response {
    match req {
        Request::Status => Response::Status(state.status()),
        Request::Peers => Response::Peers(state.peers()),
        Request::Flows => Response::Flows(state.flows()),
    }
}

/// Serve one LocalAPI client connection to completion: read
/// newline-delimited JSON [`Request`]s, [`handle`] each against `state`,
/// write the newline-delimited JSON [`Response`] back, and loop until the
/// client closes the stream (EOF). A line that isn't a valid `Request`
/// gets an [`Response::Error`] and the connection stays open (so a client
/// can recover). **Transport-agnostic** — the platform listeners (Windows
/// named pipe with an ACL'd security descriptor, unix socket; P1-cont)
/// accept a connection and hand the accepted stream here. The daemon
/// spawns one task per connection: `serve_connection(stream, state.as_ref())`.
pub async fn serve_connection<S>(stream: S, state: &dyn LocalApiState) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (rd, mut wr) = tokio::io::split(stream);
    let mut lines = tokio::io::BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(&req, state),
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        // A Response always serialises; fall back to an Error line if a
        // custom serializer ever failed, so we never break the frame.
        let mut out = serde_json::to_vec(&resp).unwrap_or_else(|e| {
            serde_json::to_vec(&Response::Error {
                message: format!("encode error: {e}"),
            })
            .expect("Error response always serialises")
        });
        out.push(b'\n');
        wr.write_all(&out).await?;
        wr.flush().await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Platform listener (unification P1-cont)
//
// The daemon (`roomlerd` / today's `roomler-agent`) calls [`serve`] once at
// startup. It binds the local-only control endpoint — a named pipe on Windows,
// a unix socket elsewhere — restricts it to trusted local principals via the
// pipe/socket ACL (no token: the OS enforces WHO can connect), and serves each
// accepted connection with [`serve_connection`]. Returns when `shutdown` flips
// true (or on a fatal bind error, which the daemon logs without dying).
// ---------------------------------------------------------------------------

/// Bind the platform LocalAPI endpoint and serve clients until `shutdown`
/// fires. Auth is the endpoint ACL, not a token:
/// - **Windows**: named pipe `\\.\pipe\roomler` with a security descriptor
///   granting only `SYSTEM`, `Administrators`, and the interactive user — so a
///   low-privilege local process can't read node state (and, once mutating
///   verbs land in P2, can't drive the daemon).
/// - **Unix**: socket at `$XDG_RUNTIME_DIR/roomler.sock` (per-user, 0700 dir),
///   chmod `0600` — owner-only.
///
/// Each accepted connection is served on its own task; a slow or misbehaving
/// client can't stall the accept loop or another client.
pub async fn serve(
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        serve_windows(state, shutdown).await
    }
    #[cfg(not(windows))]
    {
        serve_unix(state, shutdown).await
    }
}

// ---- Windows: named pipe + SDDL security descriptor ----------------------

/// The LocalAPI named-pipe path. Fixed name so thin clients (CLI, desktop app)
/// know where to connect.
#[cfg(windows)]
const LOCALAPI_PIPE_NAME: &str = r"\\.\pipe\roomler";

/// SDDL for the pipe DACL: allow (`A`) generic-all (`GA`) to Local `SY`stem,
/// `B`uiltin `A`dministrators, and `I`nteractive `U`sers — and, by omission,
/// deny everyone else. IU covers the desktop app / CLI running in the operator's
/// interactive session (including a non-elevated admin, whose Administrators SID
/// is deny-only but who still matches IU). No OWNER is set — a user-mode daemon
/// can't assign one it doesn't hold, and the creator is a valid owner anyway.
#[cfg(windows)]
const LOCALAPI_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)";

/// `SDDL_REVISION_1` — the only defined SDDL revision.
#[cfg(windows)]
const SDDL_REVISION_1: u32 = 1;

/// A security descriptor built from an SDDL string plus the
/// `SECURITY_ATTRIBUTES` that `create_with_security_attributes_raw` consumes.
/// Owns the `LocalAlloc`'d descriptor and `LocalFree`s it on drop.
#[cfg(windows)]
struct PipeSecurity {
    sa: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
    psd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
}

// SAFETY: the security descriptor is a plain `LocalAlloc`'d heap buffer with no
// thread affinity — moving ownership of `PipeSecurity` (and thus the pointer)
// to another thread and calling `LocalFree` there is sound. Needed because the
// accept loop holds it across `.await`, so `localapi::serve`'s future must be
// `Send` for `tokio::spawn`.
#[cfg(windows)]
unsafe impl Send for PipeSecurity {}

#[cfg(windows)]
impl PipeSecurity {
    fn new(sddl: &str) -> std::io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer; `psd` is a valid
        // out-pointer; the size-out argument is null (documented optional).
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd,
            bInheritHandle: 0,
        };
        Ok(Self { sa, psd })
    }

    /// Pointer to the `SECURITY_ATTRIBUTES`, valid while `self` lives. The OS
    /// copies the descriptor when the pipe instance is created, so reusing this
    /// across instances is fine.
    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        &raw mut self.sa as *mut core::ffi::c_void
    }
}

#[cfg(windows)]
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.psd.is_null() {
            // SAFETY: `psd` was allocated by ConvertStringSecurityDescriptor…
            // (LocalAlloc); LocalFree is the documented release.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.psd as _);
            }
        }
    }
}

#[cfg(windows)]
async fn serve_windows(
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    serve_windows_at(LOCALAPI_PIPE_NAME, state, shutdown).await
}

/// The named-pipe accept loop, parameterised on the pipe name so tests can use
/// a private one. Builds the ACL once, then serves clients: on each connect it
/// hands the connected instance to a task and pre-creates the next instance so
/// a second client racing the handoff isn't refused with `ERROR_PIPE_BUSY`.
#[cfg(windows)]
pub(crate) async fn serve_windows_at(
    pipe_name: &str,
    state: Arc<dyn LocalApiState>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    if *shutdown.borrow() {
        return Ok(());
    }
    let mut security = PipeSecurity::new(LOCALAPI_SDDL)?;
    // SAFETY: `security.as_ptr()` stays valid for the lifetime of `security`,
    // which outlives every create call below.
    let mut server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(pipe_name, security.as_ptr())?
    };
    tracing::info!(
        pipe = pipe_name,
        "localapi: named-pipe listener up (SYSTEM + Administrators + interactive user)"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("localapi: shutdown; pipe listener exiting");
                    return Ok(());
                }
            }
            conn = server.connect() => match conn {
                Ok(()) => {
                    let connected = server;
                    // SAFETY: same invariant as the first create.
                    server = unsafe {
                        ServerOptions::new()
                            .create_with_security_attributes_raw(pipe_name, security.as_ptr())?
                    };
                    let st = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(connected, &*st).await {
                            tracing::debug!(error = %e, "localapi: pipe client ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "localapi: pipe connect failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
}

// ---- Unix: socket in the per-user runtime dir, chmod 0600 -----------------

/// The LocalAPI socket file name (under the per-user runtime dir).
#[cfg(unix)]
const LOCALAPI_SOCKET_NAME: &str = "roomler.sock";

/// Resolve the LocalAPI socket path: `$XDG_RUNTIME_DIR/roomler.sock` when the
/// per-user runtime dir is set (systemd guarantees it's 0700 + user-owned — the
/// right home for a control socket), else a `roomler/` subdir under the temp
/// dir (locked to 0700 by the listener). The socket itself is chmod 0600.
#[cfg(unix)]
pub(crate) fn unix_socket_path() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(dir).join(LOCALAPI_SOCKET_NAME);
    }
    // No runtime dir (common on macOS, where temp_dir() is already per-user).
    std::env::temp_dir()
        .join("roomler")
        .join(LOCALAPI_SOCKET_NAME)
}

#[cfg(unix)]
async fn serve_unix(
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    serve_unix_at(unix_socket_path(), state, shutdown).await
}

/// The unix-socket accept loop, parameterised on the path so tests can use a
/// private one.
#[cfg(unix)]
pub(crate) async fn serve_unix_at(
    path: std::path::PathBuf,
    state: Arc<dyn LocalApiState>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    if *shutdown.borrow() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // Lock the parent to owner-only when we own it (the temp-subdir case);
        // for $XDG_RUNTIME_DIR this is already true and the chmod is harmless
        // (ignored if we don't own it).
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    // A stale socket from an unclean exit makes bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Owner-only: no other local user can open the control socket.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(path = %path.display(), "localapi: unix-socket listener up (0600)");

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = std::fs::remove_file(&path);
                    tracing::info!("localapi: shutdown; unix listener exiting");
                    return Ok(());
                }
            }
            accept = listener.accept() => match accept {
                Ok((stream, _addr)) => {
                    let st = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(stream, &*st).await {
                            tracing::debug!(error = %e, "localapi: unix client ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "localapi: unix accept failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock;
    impl LocalApiState for Mock {
        fn status(&self) -> NodeStatus {
            NodeStatus {
                node_id: "n1".into(),
                name: "neo16".into(),
                version: "0.3.0-rc.154".into(),
                mode: DaemonMode::Service,
                tenant_id: Some("t1".into()),
                overlay_ip: Some("100.64.0.2".into()),
                connected: true,
            }
        }
        fn peers(&self) -> Vec<PeerInfo> {
            vec![
                PeerInfo {
                    node_id: "n2".into(),
                    name: "pc50045".into(),
                    overlay_ip: Some("100.64.0.1".into()),
                    online: true,
                    connection: ConnectionType::Tunnel,
                    rtt_ms: Some(52),
                    last_seen_ms: Some(1000),
                },
                PeerInfo {
                    node_id: "n3".into(),
                    name: "home".into(),
                    overlay_ip: Some("100.64.0.9".into()),
                    online: true,
                    connection: ConnectionType::Direct,
                    rtt_ms: Some(3),
                    last_seen_ms: Some(1200),
                },
            ]
        }
        fn flows(&self) -> Vec<FlowInfo> {
            vec![FlowInfo {
                id: "f1".into(),
                kind: FlowKind::Socks5,
                local_addr: "127.0.0.1:1080".into(),
                target: None,
                node: Some("pc50045".into()),
                transport: "quic-v1".into(),
                active_flows: 2,
                bytes_in: 4096,
                bytes_out: 8192,
            }]
        }
    }

    #[test]
    fn handle_dispatches_each_verb() {
        let s = Mock;
        match handle(&Request::Status, &s) {
            Response::Status(st) => {
                assert_eq!(st.overlay_ip.as_deref(), Some("100.64.0.2"));
                assert_eq!(st.mode, DaemonMode::Service);
            }
            other => panic!("expected Status, got {other:?}"),
        }
        match handle(&Request::Peers, &s) {
            Response::Peers(p) => {
                assert_eq!(p.len(), 2);
                assert_eq!(p[0].connection, ConnectionType::Tunnel);
                assert_eq!(p[1].connection, ConnectionType::Direct);
            }
            other => panic!("expected Peers, got {other:?}"),
        }
        match handle(&Request::Flows, &s) {
            Response::Flows(f) => {
                assert_eq!(f.len(), 1);
                assert_eq!(f[0].kind, FlowKind::Socks5);
                assert!(f[0].target.is_none());
            }
            other => panic!("expected Flows, got {other:?}"),
        }
    }

    #[test]
    fn request_wire_shape_is_stable() {
        assert_eq!(
            serde_json::to_string(&Request::Status).unwrap(),
            r#"{"t":"status"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"t":"peers"}"#).unwrap(),
            Request::Peers
        );
    }

    #[test]
    fn response_round_trips_struct_and_sequence_payloads() {
        // Adjacently-tagged so a sequence payload (Peers) is legal where an
        // internally-tagged enum would reject it — locks that choice.
        let peers = handle(&Request::Peers, &Mock);
        let s = serde_json::to_string(&peers).unwrap();
        assert!(s.starts_with(r#"{"t":"peers","d":["#), "got {s}");
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), peers);

        let status = handle(&Request::Status, &Mock);
        let s = serde_json::to_string(&status).unwrap();
        assert!(s.contains(r#""t":"status""#));
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), status);

        let err = Response::Error {
            message: "nope".into(),
        };
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"t":"error","d":{"message":"nope"}}"#
        );

        // Connection types serialise snake_case (UI + wire contract).
        assert_eq!(
            serde_json::to_string(&ConnectionType::Tunnel).unwrap(),
            r#""tunnel""#
        );
    }

    #[tokio::test]
    async fn serve_connection_round_trips_and_recovers_from_garbage() {
        // In-memory duplex stands in for the named pipe / unix socket, so the
        // dispatch loop is transport-independently tested.
        let (client, server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let state = Mock;
            serve_connection(server, &state).await
        });
        let (crd, mut cwr) = tokio::io::split(client);
        let mut clines = tokio::io::BufReader::new(crd).lines();

        cwr.write_all(b"{\"t\":\"status\"}\n").await.unwrap();
        let r = clines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&r).unwrap(),
            Response::Status(_)
        ));

        cwr.write_all(b"{\"t\":\"peers\"}\n").await.unwrap();
        let r = clines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&r).unwrap(),
            Response::Peers(p) if p.len() == 2
        ));

        // Garbage line → Error response, and the connection survives for the
        // next request (recoverable, not a frame break).
        cwr.write_all(b"not json\n").await.unwrap();
        let r = clines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&r).unwrap(),
            Response::Error { .. }
        ));

        // Client closes the stream → serve_connection returns Ok(()).
        drop(cwr);
        drop(clines);
        srv.await.unwrap().unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn serve_windows_named_pipe_round_trips() {
        // Exercises the real named-pipe listener + the SDDL security descriptor
        // (PipeSecurity::new must convert the SDDL, or bind fails). A private
        // pipe name avoids colliding with a real daemon on the dev box.
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::windows::named_pipe::ClientOptions;

        let pipe = format!(r"\\.\pipe\roomler-test-{}", std::process::id());
        let (sd_tx, sd_rx) = watch::channel(false);
        let state: Arc<dyn LocalApiState> = Arc::new(Mock);
        let pipe_srv = pipe.clone();
        let srv = tokio::spawn(async move { serve_windows_at(&pipe_srv, state, sd_rx).await });

        // Retry until the first pipe instance exists.
        let mut client = None;
        for _ in 0..200 {
            match ClientOptions::new().open(&pipe) {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        let client = client.expect("connect to the LocalAPI pipe");
        let (rd, mut wr) = tokio::io::split(client);
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"t\":\"peers\"}\n").await.unwrap();
        let line = lines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&line).unwrap(),
            Response::Peers(p) if p.len() == 2
        ));

        sd_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), srv).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_unix_socket_round_trips_and_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let dir = std::env::temp_dir().join(format!("roomler-lat-{}", std::process::id()));
        let path = dir.join("s.sock");
        let (sd_tx, sd_rx) = watch::channel(false);
        let state: Arc<dyn LocalApiState> = Arc::new(Mock);
        let p = path.clone();
        let srv = tokio::spawn(async move { serve_unix_at(p, state, sd_rx).await });

        for _ in 0..200 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stream = UnixStream::connect(&path)
            .await
            .expect("connect to the LocalAPI socket");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"t\":\"status\"}\n").await.unwrap();
        let line = lines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&line).unwrap(),
            Response::Status(_)
        ));

        // The control socket must be private to the owner.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "control socket must be 0600");

        sd_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), srv).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
