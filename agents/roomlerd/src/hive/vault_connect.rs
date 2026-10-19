// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5b-2 — `vault_connect`: an endpoint this daemon serves for the
//! session's account, which authenticates to a database with a secret's
//! value, for as long as its leases. The session gets a connection string
//! with no credential in it; the value stays in daemon memory (AC12).
//!
//! | Step | What holds |
//! |---|---|
//! | the call | `handle` (a structured secret, by its path or this org's RRN, with no `#field`), `engine` (`postgres`), `upstream` (`host:port`, exactly as the secret's `audiences` names it), `database` (optional), `ca` (optional: a secret whose value is the upstream's CA, PEM), `ttl_s` (30 to 900, default 300) |
//! | the leases | the secret's `user` and `password`, and `ca`'s, each in `proxy` mode toward the upstream: the vault's `proxy-only-named-audiences` refuses an upstream the secret does not name, recorded. A lease asked for ahead toward the same upstream is taken as it is |
//! | the endpoint | Unix: `<runtime>/<session>/proxies/<id>/.s.PGSQL.5432`, which a Postgres client dials as `host=<that directory>`: both directories the daemon's own (`0711`), the socket handed to the session's account (`0600`), and each connection's peer uid checked again. Windows (P3-5b-4): `127.0.0.1` on a port the system picks, each connection's process found in the connection table and served only if its token is the session's account |
//! | a connection | [`crate::vault_proxy::pg`]: the client never authenticates; the upstream, by SCRAM-SHA-256 inside verified TLS, with the values read from the leases as each connection starts |
//! | its life | closed, with every connection it holds, when its first lease is let go: its expiry, `rc:vault.revoke`, the run's end; every one an earlier process left, at the daemon's start |
//!
//! ⚠️ `proxy` hands the session no value: what it holds is a socket only its
//! account can reach, for as long as the leases. It still acts as the
//! secret's user there, so the session's settings ASK for every call, as for
//! the other uses.
//!
//! Windows has no Unix socket most database clients can dial, so its
//! endpoint is a loopback port (P3-5b-4). Any process on the device can
//! connect to it; only one running as the session's account is served.

use std::sync::Arc;

use bson::oid::ObjectId;
use serde_json::Value;

use super::toolbelt::{PeerId, VaultTools};
use super::vault_exec::{ExecTarget, Handle, handle_of};

/// How long an endpoint lives when the call names no TTL.
const DEFAULT_TTL: u32 = 300;
/// The longest database name Postgres keeps (`NAMEDATALEN` less its NUL).
const MAX_DATABASE: usize = 63;

/// The engines a call may name: what the endpoint speaks to its client and
/// authenticates with upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Engine {
    Postgres,
}

impl Engine {
    fn of(raw: &str) -> Option<Self> {
        match raw {
            "postgres" => Some(Self::Postgres),
            _ => None,
        }
    }

    fn wire(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
        }
    }
}

/// One `vault_connect` call, checked: what is refused here never reaches the
/// vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectRequest {
    /// The structured secret whose `user` and `password` the endpoint
    /// authenticates with.
    pub(crate) secret: String,
    pub(crate) engine: Engine,
    /// The lease's audience, exactly as the call named it: the vault holds it
    /// to the secret's `audiences` as written.
    pub(crate) upstream: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) database: Option<String>,
    /// A secret whose value is the upstream's CA, PEM.
    pub(crate) ca: Option<Handle>,
    pub(crate) ttl: u32,
}

impl ConnectRequest {
    /// The values the endpoint is leased, in order: `user`, `password`, then
    /// the CA when the call names one.
    pub(crate) fn handles(&self) -> Vec<Handle> {
        let field = |f: &str| Handle {
            secret: self.secret.clone(),
            field: f.into(),
        };
        let mut handles = vec![field("user"), field("password")];
        handles.extend(self.ca.clone());
        handles
    }
}

/// `host:port`, as an audience names an upstream: a host name (`_` allowed:
/// a Compose service may carry one), an IPv4 address
/// or a bracketed IPv6 one, then a port written plainly (no sign, no leading
/// zero). Answers the host, unbracketed, and the port.
pub(crate) fn upstream_of(raw: &str) -> Result<(String, u16), String> {
    use roomler_ai_remote_control::vault::lease_limits::MAX_AUDIENCE_LEN;
    let bad = || {
        format!(
            "upstream must be host:port, as the secret's audiences name it (such as \
             db.internal:5432, or [fd00::5]:5432): got {raw:?}"
        )
    };
    if raw.len() > MAX_AUDIENCE_LEN {
        return Err(bad());
    }
    let (host, port) = raw.rsplit_once(':').ok_or_else(bad)?;
    // Digits only, and no leading zero: one spelling per port, so the
    // audience the vault matches is the port dialled.
    let plain =
        !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && !port.starts_with('0');
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|p| plain && *p != 0)
        .ok_or_else(bad)?;
    let host = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => {
            v6.parse::<std::net::Ipv6Addr>().map_err(|_| bad())?;
            v6
        }
        None => {
            let name_ok = !host.is_empty()
                && host.len() <= 253
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                && !host.starts_with(['.', '-']);
            if !name_ok {
                return Err(bad());
            }
            host
        }
    };
    Ok((host.to_string(), port))
}

pub(crate) fn parse(args: &Value) -> Result<ConnectRequest, String> {
    use roomler_ai_remote_control::vault::lease_limits::{MAX_TTL_SECS, MIN_TTL_SECS};
    let raw = args
        .get("handle")
        .and_then(Value::as_str)
        .ok_or("handle is required: a structured secret's path, or rrn:<org>:secret:<path>")?;
    let handle = handle_of(raw)?;
    if !handle.field.is_empty() {
        return Err(format!(
            "handle names the secret itself, with no #field: vault_connect leases its user and \
             password fields (got {raw:?})"
        ));
    }
    let engine = args
        .get("engine")
        .and_then(Value::as_str)
        .and_then(Engine::of)
        .ok_or("engine must be postgres")?;
    let upstream = args
        .get("upstream")
        .and_then(Value::as_str)
        .ok_or("upstream is required: host:port, as the secret's audiences name it")?;
    let (host, port) = upstream_of(upstream)?;
    let database = match args.get("database") {
        None | Some(Value::Null) => None,
        Some(Value::String(d))
            if !d.is_empty() && d.len() <= MAX_DATABASE && !d.chars().any(char::is_control) =>
        {
            Some(d.clone())
        }
        Some(_) => {
            return Err(format!(
                "database must be a name of 1 to {MAX_DATABASE} bytes, with no control character"
            ));
        }
    };
    let ca = match args.get("ca") {
        None | Some(Value::Null) => None,
        Some(Value::String(c)) => Some(handle_of(c)?),
        Some(_) => return Err("ca must name a secret whose value is a PEM CA".into()),
    };
    let ttl = match args.get("ttl_s") {
        None | Some(Value::Null) => DEFAULT_TTL,
        Some(v) => match v.as_u64().and_then(|s| u32::try_from(s).ok()) {
            Some(s) if (MIN_TTL_SECS..=MAX_TTL_SECS).contains(&s) => s,
            _ => {
                return Err(format!("ttl_s must be {MIN_TTL_SECS} to {MAX_TTL_SECS}"));
            }
        },
    };
    Ok(ConnectRequest {
        secret: handle.secret,
        engine,
        upstream: upstream.to_string(),
        host,
        port,
        database,
        ca,
        ttl,
    })
}

/// What a session's `vault_connect` call answers: where its endpoint is, with
/// a connection string that holds no credential, and how long it lasts; or
/// why there is none. On Unix the endpoint is a socket handed to `target`'s
/// account; on Windows (P3-5b-4) a loopback port whose every connection must
/// come from a process running as `peer`, the session's account as its
/// toolbelt takes it.
pub(crate) async fn call(
    vault: Arc<dyn VaultTools>,
    session: ObjectId,
    fence: u64,
    target: Option<&ExecTarget>,
    peer: &PeerId,
    args: &Value,
) -> Result<Value, String> {
    let req = parse(args)?;
    #[cfg(unix)]
    let at = {
        let _ = peer;
        endpoint::At(target.ok_or("this device serves no endpoint for a session")?)
    };
    #[cfg(windows)]
    let at = {
        let _ = target;
        endpoint::At(peer.as_str())
    };
    endpoint::open(vault, session, fence, at, req).await
}

/// Every endpoint directory an earlier process left under `runtime_root`: no
/// lease outlives the process that held it, so none of them serves anyone
/// any more. Answers how many went.
#[cfg(unix)]
pub(crate) fn sweep(runtime_root: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(runtime_root) else {
        return 0;
    };
    let mut gone = 0;
    for entry in entries.flatten() {
        let dir = entry.path().join(endpoint::PROXIES_DIR);
        let is_dir = std::fs::symlink_metadata(&dir)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        if is_dir && std::fs::remove_dir_all(&dir).is_ok() {
            gone += 1;
        }
    }
    gone
}

#[cfg(all(unix, test))]
pub(crate) use endpoint::MAX_ENDPOINTS;

mod endpoint {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::Duration;

    use bson::oid::ObjectId;
    use serde_json::{Value, json};
    use tokio::io::{AsyncWrite, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, warn};
    use zeroize::Zeroizing;

    use super::super::toolbelt::VaultTools;
    use super::super::vault_exec::Releasing;
    use super::{ConnectRequest, Engine};
    use crate::vault_proxy::{pg, tls};

    #[cfg(unix)]
    pub(crate) use self::place::PROXIES_DIR;
    pub(crate) use self::place::{At, bind};

    /// Endpoints one run holds open at once. Taken before anything is
    /// leased, so a call past it asks the vault nothing.
    pub(crate) const MAX_ENDPOINTS: usize = 8;
    /// Connections one endpoint serves at once.
    pub(crate) const MAX_CONNECTIONS: usize = 16;

    /// How many endpoints each run holds open.
    static OPEN: LazyLock<Mutex<HashMap<(ObjectId, u64), usize>>> = LazyLock::new(Default::default);

    /// One run's place among its endpoints, given back when the endpoint
    /// ends, however it ends.
    pub(crate) struct Slot {
        run: (ObjectId, u64),
    }

    impl Slot {
        pub(crate) fn take(session: ObjectId, fence: u64) -> Option<Self> {
            let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
            let n = open.entry((session, fence)).or_default();
            if *n >= MAX_ENDPOINTS {
                return None;
            }
            *n += 1;
            Some(Self {
                run: (session, fence),
            })
        }
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = open.get_mut(&self.run) {
                *n -= 1;
                if *n == 0 {
                    open.remove(&self.run);
                }
            }
        }
    }

    /// What every connection of one endpoint shares: where it authenticates,
    /// and the leases its values are read from as each connection starts.
    struct Endpoint {
        vault: Arc<dyn VaultTools>,
        /// The leases the values are read from, by id: never a value.
        user_lease: String,
        password_lease: String,
        host: String,
        port: u16,
        tls: tokio_rustls::TlsConnector,
    }

    impl Endpoint {
        /// The upstream for one connection, its values read from the leases
        /// now: `None` once either is let go.
        fn upstream(&self) -> Option<pg::Upstream> {
            Some(pg::Upstream {
                host: self.host.clone(),
                port: self.port,
                user: text(self.vault.value(&self.user_lease)?)?.to_string(),
                password: text(self.vault.value(&self.password_lease)?)?,
                tls: self.tls.clone(),
            })
        }
    }

    /// A leased value as text, copied once, into memory that is zeroed.
    fn text(v: Zeroizing<Vec<u8>>) -> Option<Zeroizing<String>> {
        std::str::from_utf8(&v)
            .ok()
            .map(|s| Zeroizing::new(s.to_string()))
    }

    /// Percent-encode `s` for a URI, leaving what reads plainly: letters,
    /// digits, `-._~`, and `/` where `slash` says.
    fn encode(s: &str, slash: bool) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (slash && b == b'/') {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    /// How a client reaches an endpoint, as the answer says it.
    pub(crate) struct Dial {
        host: String,
        port: u16,
        uri: String,
        env: serde_json::Map<String, Value>,
    }

    pub(crate) async fn open(
        vault: Arc<dyn VaultTools>,
        session: ObjectId,
        fence: u64,
        at: At<'_>,
        req: ConnectRequest,
    ) -> Result<Value, String> {
        let Engine::Postgres = req.engine;
        let slot = Slot::take(session, fence).ok_or_else(|| {
            format!("this session holds {MAX_ENDPOINTS} endpoints already: let one end first")
        })?;
        // The leases this call asks for itself: let go if no endpoint opens.
        let handles = req.handles();
        let mut held = Releasing {
            vault: vault.as_ref(),
            leases: Vec::with_capacity(handles.len()),
        };
        let mut leases = Vec::with_capacity(handles.len());
        let wanted = Duration::from_secs(u64::from(req.ttl));
        for h in &handles {
            // P3-4c-3 — a lease asked for ahead toward this upstream, lasting
            // at least as long as the endpoint is asked to, is taken as it is.
            if let Some(lease) = vault.ahead(session, fence, h, "proxy", &req.upstream, wanted) {
                leases.push(lease);
                continue;
            }
            let lease = vault
                .lease(session, fence, h, "proxy", &req.upstream, req.ttl)
                .await
                .map_err(|why| format!("{h}: {why}"))?;
            held.leases.push(lease.clone());
            leases.push(lease);
        }
        // The values, checked once before anything is served: the endpoint
        // reads them again as each connection starts.
        let value = |lease: &String, h: &super::Handle| {
            vault
                .value(lease)
                .and_then(text)
                .ok_or_else(|| format!("{h}: the lease ended, or its value is not text"))
        };
        let user = value(&leases[0], &handles[0])?;
        if user.is_empty() || user.chars().any(char::is_control) {
            return Err(format!("{}: not a user name Postgres takes", handles[0]));
        }
        value(&leases[1], &handles[1])?;
        let ca = match leases.get(2) {
            Some(lease) => Some(value(lease, &handles[2])?),
            None => None,
        };
        let tls = tls::connector(ca.as_deref().map(String::as_str))
            .map_err(|e| format!("the upstream's TLS: {e}"))?;
        let bound = bind(&at)?;
        let stop = CancellationToken::new();
        // The endpoint lives with its leases: whichever is let go first
        // closes it, and every connection it holds.
        for lease in &leases {
            if !vault.stop_with(lease, stop.clone()) {
                stop.cancel();
                bound.close();
                return Err(
                    "a lease ended before the endpoint was handed over: it is closed".into(),
                );
            }
        }
        // Handed to the endpoint, which lets them go when it closes.
        let own = held.leases.clone();
        held.keep();
        let ttl_s = leases
            .iter()
            .filter_map(|l| vault.left(l))
            .min()
            .map_or(0, |d| d.as_secs());
        let dial = bound.dial(req.database.as_deref());
        let endpoint = Arc::new(Endpoint {
            vault: Arc::clone(&vault),
            user_lease: leases[0].clone(),
            password_lease: leases[1].clone(),
            host: req.host.clone(),
            port: req.port,
            tls,
        });
        tokio::spawn(serve(bound, endpoint, stop, own, slot));
        Ok(json!({
            "engine": req.engine.wire(),
            "upstream": req.upstream,
            "host": dial.host,
            "port": dial.port,
            "database": req.database,
            "uri": dial.uri,
            "env": dial.env,
            "ttl_s": ttl_s,
            "note": "Connect with any user name and no password: the endpoint signs in to the \
                     upstream as the secret's user. It closes, with its connections, when its \
                     lease ends.",
        }))
    }

    /// Serve one endpoint until its first lease is let go: each connection
    /// from the session's account, at most [`MAX_CONNECTIONS`] at once, one
    /// Postgres session each. Then it closes, the leases its call asked for
    /// itself (`own`) are let go rather than held to their expiry, and its
    /// place among the run's endpoints goes with it.
    async fn serve(
        bound: place::Bound,
        endpoint: Arc<Endpoint>,
        stop: CancellationToken,
        own: Vec<String>,
        _slot: Slot,
    ) {
        let open = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            let got = tokio::select! {
                _ = stop.cancelled() => break,
                got = bound.accept() => got,
            };
            let stream = match got {
                Ok(stream) => stream,
                Err(e) => {
                    // Out of descriptors and the like: back off rather than spin.
                    warn!(%e, "vault: a proxy endpoint could not accept");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            if let Err(why) = bound.admits(&stream) {
                warn!(%why, "vault: a proxy connection not from the session's account — refused");
                continue;
            }
            let Ok(permit) = Arc::clone(&open).try_acquire_owned() else {
                tokio::spawn(refuse(
                    stream,
                    format!("the endpoint serves {MAX_CONNECTIONS} connections at once"),
                ));
                continue;
            };
            let (endpoint, stop) = (Arc::clone(&endpoint), stop.clone());
            tokio::spawn(async move {
                let _permit = permit;
                let Some(upstream) = endpoint.upstream() else {
                    return refuse(stream, "the endpoint's lease has ended".into()).await;
                };
                tokio::select! {
                    _ = stop.cancelled() => {}
                    served = pg::serve(stream, &upstream) => {
                        if let Err(why) = served {
                            debug!(%why, "vault: a proxy connection did not start");
                        }
                    }
                }
            });
        }
        bound.close();
        endpoint.vault.release(&own);
    }

    /// Tell a client why it is not served, as Postgres would, and hang up.
    async fn refuse<S: AsyncWrite + Unpin>(mut stream: S, why: String) {
        let _ =
            tokio::time::timeout(Duration::from_secs(2), stream.write_all(&pg::fatal(&why))).await;
    }

    /// Unix: a socket in a directory of the session's runtime directory,
    /// handed to the session's account, each connection's uid checked.
    #[cfg(unix)]
    mod place {
        use std::path::{Path, PathBuf};

        use serde_json::json;
        use tokio::net::{UnixListener, UnixStream};

        use super::super::super::vault_exec::ExecTarget;
        use super::{Dial, encode};

        /// Each session's endpoints, under its runtime directory.
        pub(crate) const PROXIES_DIR: &str = "proxies";
        /// The port a Postgres endpoint's socket is named for: a client dials
        /// `host=<directory>` and, unless told otherwise, port 5432.
        pub(crate) const PG_PORT: u16 = 5432;

        /// Where an endpoint is served: the session's runtime directory, and
        /// the account its socket is handed to.
        pub(crate) struct At<'a>(pub(crate) &'a ExecTarget);

        /// A bound endpoint: its socket, its directory, and the one uid a
        /// connection is taken from.
        pub(crate) struct Bound {
            listener: UnixListener,
            dir: PathBuf,
            peer: u32,
        }

        impl Bound {
            pub(crate) async fn accept(&self) -> std::io::Result<UnixStream> {
                self.listener.accept().await.map(|(stream, _)| stream)
            }

            /// Whether a connection comes from the session's account.
            pub(crate) fn admits(&self, stream: &UnixStream) -> Result<(), String> {
                match stream.peer_cred() {
                    Ok(c) if c.uid() == self.peer => Ok(()),
                    Ok(c) => Err(format!("uid {}", c.uid())),
                    Err(e) => Err(format!("a peer that could not be identified: {e}")),
                }
            }

            pub(crate) fn dial(&self, database: Option<&str>) -> Dial {
                let host = self.dir.to_string_lossy().into_owned();
                let mut env = serde_json::Map::new();
                env.insert("PGHOST".into(), json!(host));
                env.insert("PGPORT".into(), json!(PG_PORT.to_string()));
                let mut uri = String::from("postgresql:///");
                if let Some(db) = database {
                    uri.push_str(&encode(db, false));
                    env.insert("PGDATABASE".into(), json!(db));
                }
                uri.push_str(&format!("?host={}&port={PG_PORT}", encode(&host, true)));
                Dial {
                    host,
                    port: PG_PORT,
                    uri,
                    env,
                }
            }

            /// Close the socket, and remove its directory.
            pub(crate) fn close(self) {
                drop(self.listener);
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        /// `dir`, made if absent: the daemon's own, `0711`, so the session's
        /// account reaches what it names and lists nothing. A link or
        /// anything but a directory in its place is refused, never followed.
        fn private_dir(dir: &Path) -> Result<(), String> {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::symlink_metadata(dir) {
                Ok(m) if !m.file_type().is_dir() => {
                    return Err(format!("{} is not a plain directory", dir.display()));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                }
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o711))
                .map_err(|e| format!("{}: {e}", dir.display()))
        }

        /// A new endpoint's directory, `<session_dir>/proxies/<id>`, and its
        /// socket, bound and handed to the session's account at `0600`. The
        /// directory is removed again if any step fails.
        pub(crate) fn bind(at: &At<'_>) -> Result<Bound, String> {
            use std::os::unix::fs::PermissionsExt;
            let ExecTarget {
                session_dir, owner, ..
            } = at.0;
            let root = session_dir.join(PROXIES_DIR);
            private_dir(&root)?;
            // A short id, so the socket's path stays well inside `sun_path`
            // (104 bytes on macOS); `create_dir`, not `create_dir_all`, so a
            // name already there is drawn again, never joined.
            let mut drawn = 0;
            let dir = loop {
                let dir = root.join(format!("{:08x}", rand::random::<u32>()));
                match std::fs::create_dir(&dir) {
                    Ok(()) => break dir,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && drawn < 3 => {
                        drawn += 1
                    }
                    Err(e) => return Err(format!("{}: {e}", dir.display())),
                }
            };
            let made = (|| {
                private_dir(&dir)?;
                let socket = dir.join(format!(".s.PGSQL.{PG_PORT}"));
                let listener = UnixListener::bind(&socket)
                    .map_err(|e| format!("{}: {e}", socket.display()))?;
                // Bound under the daemon's umask: until these two calls, only
                // the daemon can connect. Then the session's account, and
                // nobody else.
                owner
                    .map(|(u, g)| std::os::unix::fs::chown(&socket, Some(u), Some(g)))
                    .unwrap_or(Ok(()))
                    .and_then(|()| {
                        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                    })
                    .map_err(|e| format!("{}: {e}", socket.display()))?;
                Ok(listener)
            })();
            match made {
                Ok(listener) => Ok(Bound {
                    listener,
                    dir,
                    // SAFETY: geteuid reads our own credentials; it cannot fail.
                    peer: owner.map_or_else(|| unsafe { libc::geteuid() }, |(u, _)| u),
                }),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    Err(e)
                }
            }
        }
    }

    /// P3-5b-4 — Windows: a loopback port, each connection's process found
    /// in the system's connection table and refused unless its token is the
    /// session's account. Most Windows database clients cannot dial a Unix
    /// socket.
    ///
    /// ⚠️ Any process on the device can connect to the port; only the
    /// session's account is served. The port cannot be taken over by another
    /// account's socket: Windows makes a bound port shareable only where its
    /// first socket asked for that (`SO_REUSEADDR`), which this one does not,
    /// and the daemon's own account (SYSTEM) is the only exception.
    #[cfg(windows)]
    mod place {
        use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

        use serde_json::json;
        use tokio::net::{TcpListener, TcpStream};

        use super::{Dial, encode};

        /// Where an endpoint is served: for the session's account, by SID.
        pub(crate) struct At<'a>(pub(crate) &'a str);

        /// A bound endpoint: its listener, its address, and the one account a
        /// connection is taken from.
        pub(crate) struct Bound {
            listener: TcpListener,
            addr: SocketAddrV4,
            peer: String,
        }

        impl Bound {
            pub(crate) async fn accept(&self) -> std::io::Result<TcpStream> {
                self.listener.accept().await.map(|(stream, _)| stream)
            }

            /// Whether a connection comes from a process running as the
            /// session's account: its row of the connection table names the
            /// process, and the process's token the account.
            pub(crate) fn admits(&self, stream: &TcpStream) -> Result<(), String> {
                let SocketAddr::V4(client) = stream.peer_addr().map_err(|e| e.to_string())? else {
                    return Err("a client that is not IPv4 loopback".into());
                };
                let pid = crate::hive_win::tcp_client_pid(client, self.addr)
                    .ok_or("a connection no process owns any more")?;
                let sid = crate::hive_win::process_sid(pid)?;
                if sid == self.peer {
                    Ok(())
                } else {
                    Err(format!("{sid} (pid {pid})"))
                }
            }

            pub(crate) fn dial(&self, database: Option<&str>) -> Dial {
                let (host, port) = (self.addr.ip().to_string(), self.addr.port());
                let mut env = serde_json::Map::new();
                env.insert("PGHOST".into(), json!(host));
                env.insert("PGPORT".into(), json!(port.to_string()));
                // The loopback leg is plain; the upstream's is TLS.
                env.insert("PGSSLMODE".into(), json!("disable"));
                let mut uri = format!("postgresql://{host}:{port}/");
                if let Some(db) = database {
                    uri.push_str(&encode(db, false));
                    env.insert("PGDATABASE".into(), json!(db));
                }
                uri.push_str("?sslmode=disable");
                Dial {
                    host,
                    port,
                    uri,
                    env,
                }
            }

            pub(crate) fn close(self) {
                drop(self.listener);
            }
        }

        /// A listener on `127.0.0.1`, on a port the system picks.
        pub(crate) fn bind(at: &At<'_>) -> Result<Bound, String> {
            let failed = |e: std::io::Error| format!("a loopback port: {e}");
            let std = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(failed)?;
            std.set_nonblocking(true).map_err(failed)?;
            let listener = TcpListener::from_std(std).map_err(failed)?;
            let SocketAddr::V4(addr) = listener.local_addr().map_err(failed)? else {
                return Err("a loopback port that is not IPv4".into());
            };
            Ok(Bound {
                listener,
                addr,
                peer: at.0.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests;
