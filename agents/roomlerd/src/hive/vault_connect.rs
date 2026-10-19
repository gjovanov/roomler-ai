// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5b-2 — `vault_connect`: an endpoint this daemon serves for the
//! session's account, which authenticates to a database (from P3-5d-2, an
//! HTTP upstream too) with a secret's value, for as long as its leases. The
//! session gets a connection string with no credential in it; the value
//! stays in daemon memory (AC12).
//!
//! | Step | What holds |
//! |---|---|
//! | the call | `handle` (a structured secret, by its path or this org's RRN, with no `#field`), `engine` (`postgres`, from P3-5c-2 `mongodb`, from P3-5d-2 `http`), `upstream` (`host:port` for a database, an origin `https://host[:port]` for `http`, exactly as the secret's `audiences` names it), `auth` (`http`'s: `basic` or `bearer`), `database` (optional), `ca` (optional: a secret whose value is the upstream's CA, PEM), `auth_source` (MongoDB's: the database the user is defined in, default `admin`), `ttl_s` (30 to 900, default 300) |
//! | the leases | the secret's `user` and `password` (for `bearer`, its `token`), and `ca`'s, each in `proxy` mode toward the upstream: the vault's `proxy-only-named-audiences` refuses an upstream the secret does not name, recorded. A lease asked for ahead toward the same upstream is taken as it is |
//! | the endpoint | a database's on Unix: `<runtime>/<session>/proxies/<id>/.s.PGSQL.5432`, which a Postgres client dials as `host=<that directory>`, or `…/mongodb-27017.sock`, which a MongoDB client dials by its path: both directories the daemon's own (`0711`), the socket handed to the session's account (`0600`), and each connection's peer uid checked again. Every endpoint on Windows (P3-5b-4), and an HTTP one everywhere (P3-5d-2): `127.0.0.1` on a port the system picks, each connection's account read from the system's table of connections and served only if it is the session's |
//! | a connection | the call's engine, [`crate::vault_proxy::pg`], [`crate::vault_proxy::mongo`] or [`crate::vault_proxy::http`]: the client never authenticates; the upstream, by SCRAM-SHA-256 or by `Authorization` inside verified TLS, with the values read from the leases as each connection starts |
//! | its life | closed, with every connection it holds, when its first lease is let go: its expiry, `rc:vault.revoke`, the run's end; every one an earlier process left, at the daemon's start |
//!
//! ⚠️ `proxy` hands the session no value: what it holds is an endpoint that
//! serves only its account, for as long as the leases. It still acts as the
//! secret's user there, so the session's settings ASK for every call, as for
//! the other uses.
//!
//! A loopback port serves a client that cannot dial a Unix socket: most
//! database clients on Windows, and git and most HTTP clients everywhere. Any
//! process on the device can connect to one; only one running as the
//! session's account is served. Linux reads who connected from
//! `/proc/net/tcp`, Windows from its connection table, and macOS (P3-5d-3)
//! from the descriptors each process holds.

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
    /// P3-5c-2.
    Mongodb,
    /// P3-5d-2 — an HTTP upstream: an API, or a git server.
    Http,
}

impl Engine {
    fn of(raw: &str) -> Option<Self> {
        match raw {
            "postgres" => Some(Self::Postgres),
            "mongodb" => Some(Self::Mongodb),
            "http" => Some(Self::Http),
            _ => None,
        }
    }

    fn wire(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mongodb => "mongodb",
            Self::Http => "http",
        }
    }
}

/// P3-5d-2 — how an HTTP endpoint presents the secret's value: always in
/// `Authorization`, in the form the call names. The session never names a
/// header: one it could name is one an upstream might echo back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Auth {
    /// The secret's `user` and `password` (RFC 7617).
    Basic,
    /// The secret's `token` (RFC 6750).
    Bearer,
}

/// Whether `name` is a MongoDB database name: 1 to 63 bytes, none of the
/// characters a server refuses in one, and no control character.
fn mongo_database_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DATABASE
        && !name
            .chars()
            .any(|c| c.is_control() || "/\\. \"$*<>:|?".contains(c))
}

/// One `vault_connect` call, checked: what is refused here never reaches the
/// vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectRequest {
    /// The structured secret whose fields the endpoint authenticates with.
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
    /// MongoDB's: the database the secret's user is defined in. `None` for
    /// Postgres, whose user signs in to the database a client names, and
    /// for `http`.
    pub(crate) auth_source: Option<String>,
    /// P3-5d-2 — `http`'s: how the value is presented. `None` for a
    /// database.
    pub(crate) auth: Option<Auth>,
    pub(crate) ttl: u32,
}

impl ConnectRequest {
    /// The values the endpoint is leased, in order: `user` and `password`
    /// (a database's, or `basic`'s), or `token` (`bearer`'s); then the CA
    /// when the call names one.
    pub(crate) fn handles(&self) -> Vec<Handle> {
        let field = |f: &str| Handle {
            secret: self.secret.clone(),
            field: f.into(),
        };
        let mut handles = match self.auth {
            Some(Auth::Bearer) => vec![field("token")],
            Some(Auth::Basic) | None => vec![field("user"), field("password")],
        };
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

/// P3-5d-2 — an origin, as an audience names an HTTP upstream:
/// `https://<host>[:<port>]`, TLS always, with no path, query, user or
/// fragment, and the port written only where it is not 443, so an origin
/// has one spelling. A database's audience is `host:port`, a different
/// string, so a secret named for one never opens the other. Answers the
/// host, unbracketed, and the port.
pub(crate) fn origin_of(raw: &str) -> Result<(String, u16), String> {
    use roomler_ai_remote_control::vault::lease_limits::MAX_AUDIENCE_LEN;
    let bad = || {
        format!(
            "upstream must be an origin, https://host or https://host:port, as the secret's \
             audiences name it (such as https://github.com): got {raw:?}"
        )
    };
    if raw.len() > MAX_AUDIENCE_LEN {
        return Err(bad());
    }
    let authority = raw.strip_prefix("https://").ok_or_else(bad)?;
    if authority.contains(['/', '?', '#', '@']) {
        return Err(bad());
    }
    // A name with no colon, or a bracketed IPv6 address, names no port.
    let ported = authority.contains(':') && !authority.ends_with(']');
    let (host, port) = if ported {
        upstream_of(authority)
    } else {
        upstream_of(&format!("{authority}:443"))
    }
    .map_err(|_| bad())?;
    if ported && port == 443 {
        let plain = authority.strip_suffix(":443").unwrap_or(authority);
        return Err(format!(
            "upstream names port 443, which an origin leaves out: write https://{plain}"
        ));
    }
    Ok((host, port))
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
            "handle names the secret itself, with no #field: vault_connect leases the fields it \
             signs in with (got {raw:?})"
        ));
    }
    let engine = args
        .get("engine")
        .and_then(Value::as_str)
        .and_then(Engine::of)
        .ok_or("engine must be postgres, mongodb or http")?;
    let upstream = args.get("upstream").and_then(Value::as_str).ok_or(
        "upstream is required: host:port for a database, or https://host[:port] for http, as the \
         secret's audiences name it",
    )?;
    let (host, port) = match engine {
        Engine::Http => origin_of(upstream)?,
        Engine::Postgres | Engine::Mongodb => upstream_of(upstream)?,
    };
    let auth = match args.get("auth") {
        None | Some(Value::Null) if engine == Engine::Http => {
            return Err(
                "auth is required for http: basic (the secret's user and password) or bearer \
                 (its token)"
                    .into(),
            );
        }
        None | Some(Value::Null) => None,
        Some(_) if engine != Engine::Http => {
            return Err("auth is http's: a database is signed in to by SCRAM".into());
        }
        Some(Value::String(a)) if a == "basic" => Some(Auth::Basic),
        Some(Value::String(a)) if a == "bearer" => Some(Auth::Bearer),
        Some(_) => return Err("auth must be basic or bearer".into()),
    };
    let database = match args.get("database") {
        None | Some(Value::Null) => None,
        Some(_) if engine == Engine::Http => {
            return Err(
                "database is a database's: an http endpoint stands for an origin, and each \
                 request names its own path"
                    .into(),
            );
        }
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
    let auth_source = match args.get("auth_source") {
        None | Some(Value::Null) => (engine == Engine::Mongodb).then(|| "admin".to_string()),
        Some(_) if engine == Engine::Postgres => {
            return Err(
                "auth_source is MongoDB's: a Postgres user signs in to the database a client names"
                    .into(),
            );
        }
        Some(_) if engine == Engine::Http => {
            return Err(
                "auth_source is MongoDB's: an http endpoint presents the secret in Authorization"
                    .into(),
            );
        }
        Some(Value::String(a)) if mongo_database_ok(a) => Some(a.clone()),
        Some(_) => {
            return Err(format!(
                "auth_source must be a database name of 1 to {MAX_DATABASE} bytes, with none of \
                 / \\ . \" $ * < > : | ? and no control character"
            ));
        }
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
        auth_source,
        auth,
        ttl,
    })
}

/// What a session's `vault_connect` call answers: where its endpoint is, with
/// a connection string that holds no credential, and how long it lasts; or
/// why there is none. On Unix a database's endpoint is a socket handed to
/// `target`'s account, and a loopback port serves only that account; on
/// Windows (P3-5b-4) every endpoint is a loopback port whose every
/// connection must come from a process running as `peer`, the session's
/// account as its toolbelt takes it.
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

/// P3-5d-2 — Linux: who owns a TCP connection's client end, read from the
/// kernel's own tables of sockets.
#[cfg(target_os = "linux")]
mod proc_net {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};

    /// The tables the kernel keeps, IPv4's and IPv6's: a client on a
    /// dual-stack socket shows in the second, its address IPv4 mapped.
    const TABLES: [&str; 2] = ["/proc/net/tcp", "/proc/net/tcp6"];
    /// A row's state in TIME_WAIT: no process owns it, and it reads uid 0.
    const TIME_WAIT: &str = "06";

    /// The uid that owns the socket at `client` connected to `server`:
    /// `None` where no live row names both ends, or two rows disagree. A row
    /// in TIME_WAIT, or with no inode, is an end no process holds any more.
    pub(crate) fn owner(client: SocketAddrV4, server: SocketAddrV4) -> Option<u32> {
        let mut owner = None;
        for table in TABLES {
            let Ok(text) = std::fs::read_to_string(table) else {
                continue;
            };
            for row in text.lines().skip(1) {
                let f: Vec<&str> = row.split_whitespace().collect();
                // sl, local, remote, st, tx:rx, tr:when, retrnsmt, uid,
                // timeout, inode, …
                let [_, local, remote, state, _, _, _, uid, _, inode, ..] = f.as_slice() else {
                    continue;
                };
                if *state == TIME_WAIT || *inode == "0" {
                    continue;
                }
                if end(local) != Some(client) || end(remote) != Some(server) {
                    continue;
                }
                let uid = uid.parse::<u32>().ok()?;
                if owner.replace(uid).is_some_and(|seen| seen != uid) {
                    return None;
                }
            }
        }
        owner
    }

    /// One end, as a row writes it: the address in hex, as the kernel holds
    /// it (network order, printed as native words), a colon, then the port
    /// in hex. IPv4, or an IPv4 address mapped into IPv6; `None` otherwise.
    pub(super) fn end(raw: &str) -> Option<SocketAddrV4> {
        let (addr, port) = raw.split_once(':')?;
        let port = u16::from_str_radix(port, 16).ok()?;
        let word = |hex: &str| u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes);
        let ip = match addr.len() {
            8 => Ipv4Addr::from(word(addr)?),
            32 => {
                let mut bytes = [0u8; 16];
                for (i, chunk) in bytes.chunks_exact_mut(4).enumerate() {
                    chunk.copy_from_slice(&word(addr.get(i * 8..i * 8 + 8)?)?);
                }
                Ipv6Addr::from(bytes).to_ipv4_mapped()?
            }
            _ => return None,
        };
        Some(SocketAddrV4::new(ip, port))
    }
}

/// P3-5d-3 — macOS: whether a TCP connection's client end is an account's,
/// from the kernel's view of each process's descriptors (`libproc`). macOS
/// has no table of sockets with an owner in it, as Linux's `/proc/net/tcp`
/// is, nor says who made a socket (its `soi_stat` is never filled); it does
/// say which descriptors each process holds.
#[cfg(target_os = "macos")]
// The SDK's structs are mirrored whole, every field held for its place in
// the layout; only some are read.
#[allow(dead_code)]
mod libproc {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};
    use std::os::raw::c_int;

    /// `proc_listpids`: every process; those whose effective uid is the one
    /// asked.
    const PROC_ALL_PIDS: u32 = 1;
    const PROC_UID_ONLY: u32 = 4;
    /// `proc_pidfdinfo`: a socket's description.
    const PROC_PIDFDSOCKETINFO: c_int = 3;
    /// `socket_info.soi_kind`: a TCP socket.
    const SOCKINFO_TCP: c_int = 2;
    /// `in_sockinfo.insi_vflag`: which family its addresses are in.
    const INI_IPV4: u8 = 0x1;
    const INI_IPV6: u8 = 0x2;

    // `<sys/proc_info.h>`, as the SDK lays it out: the part read here.

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SockbufInfo {
        sbi_cc: u32,
        sbi_hiwat: u32,
        sbi_mbcnt: u32,
        sbi_mbmax: u32,
        sbi_lowat: u32,
        sbi_flags: i16,
        sbi_timeo: i16,
    }

    /// `struct in4in6_addr`: an IPv4 address in the last four bytes.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct In4In6Addr {
        pad32: [u32; 3],
        addr4: [u8; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union InAddr46 {
        ina_46: In4In6Addr,
        ina_6: [u8; 16],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InSockInfo {
        /// The ports, each as the stack holds it: network order.
        insi_fport: c_int,
        insi_lport: c_int,
        insi_gencnt: u64,
        insi_flags: u32,
        insi_flow: u32,
        insi_vflag: u8,
        insi_ip_ttl: u8,
        rfu_1: u32,
        insi_faddr: InAddr46,
        insi_laddr: InAddr46,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SocketInfo {
        /// Left zeroed by the kernel for a socket (`fill_socketinfo` never
        /// sets it): held for its place only.
        soi_stat: libc::vinfo_stat,
        soi_so: u64,
        soi_pcb: u64,
        soi_type: c_int,
        soi_protocol: c_int,
        soi_family: c_int,
        soi_options: i16,
        soi_linger: i16,
        soi_state: i16,
        soi_qlen: i16,
        soi_incqlen: i16,
        soi_qlimit: i16,
        soi_timeo: i16,
        soi_error: u16,
        soi_oobmark: u32,
        soi_rcv: SockbufInfo,
        soi_snd: SockbufInfo,
        soi_kind: c_int,
        rfu_1: u32,
        /// A union in the SDK; a TCP socket's starts with its `in_sockinfo`.
        soi_proto: InSockInfo,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SocketFdInfo {
        pfi: ProcFileInfo,
        psi: SocketInfo,
    }

    // The SDK's offsets, so a mistake in the layout above fails the build.
    const _: () = assert!(std::mem::size_of::<libc::vinfo_stat>() == 136);
    const _: () = assert!(std::mem::offset_of!(SocketInfo, soi_kind) == 232);
    const _: () = assert!(std::mem::offset_of!(SocketInfo, soi_proto) == 240);
    const _: () = assert!(std::mem::offset_of!(InSockInfo, insi_faddr) == 32);
    const _: () = assert!(std::mem::offset_of!(SocketFdInfo, psi) == 24);

    /// What the kernel fills: the whole `socket_fdinfo`, whose protocol
    /// union is larger than the part read here.
    #[repr(C, align(8))]
    struct Buf([u8; 2048]);

    /// Whether the socket at `client` connected to `server` is held, and
    /// held only, by processes running as `uid`: one held by any other
    /// account's process too is not the account's. The kernel never fills a
    /// socket's own `soi_stat`, so who made it cannot be read; who holds it
    /// can.
    pub(crate) fn held_by(client: SocketAddrV4, server: SocketAddrV4, uid: u32) -> bool {
        let theirs: std::collections::HashSet<c_int> =
            pids(PROC_UID_ONLY, uid).into_iter().collect();
        let mut held = false;
        for pid in pids(PROC_ALL_PIDS, 0) {
            let holds = socket_fds(pid).into_iter().any(|fd| {
                tcp_ends(pid, fd).is_some_and(|(local, remote)| local == client && remote == server)
            });
            if holds {
                if !theirs.contains(&pid) {
                    return false;
                }
                held = true;
            }
        }
        held
    }

    /// The pids `proc_listpids` names for `kind` (and its `info`).
    fn pids(kind: u32, info: u32) -> Vec<c_int> {
        let size = std::mem::size_of::<c_int>();
        // SAFETY: a size query, with no buffer.
        let bytes = unsafe { libc::proc_listpids(kind, info, std::ptr::null_mut(), 0) };
        let Ok(bytes) = usize::try_from(bytes) else {
            return Vec::new();
        };
        // Room for processes started since the query.
        let mut pids = vec![0 as c_int; bytes / size + 32];
        let room = c_int::try_from(pids.len() * size).unwrap_or(c_int::MAX);
        // SAFETY: the buffer holds `room` bytes.
        let filled = unsafe { libc::proc_listpids(kind, info, pids.as_mut_ptr().cast(), room) };
        let Ok(filled) = usize::try_from(filled) else {
            return Vec::new();
        };
        pids.truncate(filled / size);
        pids.retain(|p| *p > 0);
        pids
    }

    /// The descriptors of `pid` that are sockets.
    fn socket_fds(pid: c_int) -> Vec<c_int> {
        let size = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a size query, with no buffer.
        let bytes =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        let Ok(bytes) = usize::try_from(bytes) else {
            return Vec::new();
        };
        // Room for descriptors opened since the query.
        let mut fds = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            bytes / size + 32
        ];
        let room = c_int::try_from(fds.len() * size).unwrap_or(c_int::MAX);
        // SAFETY: the buffer holds `room` bytes.
        let filled = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), room)
        };
        let Ok(filled) = usize::try_from(filled) else {
            return Vec::new();
        };
        fds.truncate(filled / size);
        fds.into_iter()
            .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
            .map(|f| f.proc_fd)
            .collect()
    }

    /// What `pid`'s TCP sockets connect, as libproc tells it: for a test to
    /// show when an end is not found.
    #[cfg(test)]
    pub(crate) fn ends_of(pid: c_int) -> Vec<(SocketAddrV4, SocketAddrV4)> {
        socket_fds(pid)
            .into_iter()
            .filter_map(|fd| tcp_ends(pid, fd))
            .collect()
    }

    /// A TCP socket's two ends: `None` for any other socket, or one that is
    /// gone.
    fn tcp_ends(pid: c_int, fd: c_int) -> Option<(SocketAddrV4, SocketAddrV4)> {
        let mut buf = Buf([0; 2048]);
        let room = c_int::try_from(buf.0.len()).ok()?;
        // SAFETY: the buffer holds `room` bytes.
        let filled = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDSOCKETINFO,
                buf.0.as_mut_ptr().cast(),
                room,
            )
        };
        if usize::try_from(filled).ok()? < std::mem::size_of::<SocketFdInfo>() {
            return None;
        }
        // SAFETY: the kernel filled at least this much, and every field is
        // plain data, valid at any value.
        let info: SocketFdInfo = unsafe { std::ptr::read(buf.0.as_ptr().cast()) };
        let si = info.psi;
        if si.soi_kind != SOCKINFO_TCP {
            return None;
        }
        let ini = si.soi_proto;
        let ip = |a: InAddr46| -> Option<Ipv4Addr> {
            // SAFETY: both of the union's arms are plain bytes.
            if ini.insi_vflag & INI_IPV4 != 0 {
                Some(Ipv4Addr::from(unsafe { a.ina_46 }.addr4))
            } else if ini.insi_vflag & INI_IPV6 != 0 {
                Ipv6Addr::from(unsafe { a.ina_6 }).to_ipv4_mapped()
            } else {
                None
            }
        };
        // Each port as the stack holds it, in network order.
        let port = |p: c_int| u16::from_be(p as u16);
        Some((
            SocketAddrV4::new(ip(ini.insi_laddr)?, port(ini.insi_lport)),
            SocketAddrV4::new(ip(ini.insi_faddr)?, port(ini.insi_fport)),
        ))
    }
}

mod endpoint {
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::Duration;

    use bson::oid::ObjectId;
    use serde_json::{Value, json};
    use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, warn};
    use zeroize::Zeroizing;

    use super::super::toolbelt::VaultTools;
    use super::super::vault_exec::Releasing;
    use super::{Auth, ConnectRequest, Engine};
    use crate::vault_proxy::{http, mongo, pg, tls};

    #[cfg(unix)]
    pub(crate) use self::socket::PROXIES_DIR;

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

    /// Where an endpoint is served, and for whom: on Unix the session's
    /// runtime directory and its account; on Windows its account, by SID.
    #[cfg(unix)]
    pub(crate) struct At<'a>(pub(crate) &'a super::ExecTarget);
    #[cfg(windows)]
    pub(crate) struct At<'a>(pub(crate) &'a str);

    #[cfg(unix)]
    impl At<'_> {
        /// The one uid a connection is served for: the session's account,
        /// or this daemon's own where it runs the session as itself (a test
        /// does).
        fn uid(&self) -> u32 {
            // SAFETY: geteuid reads our own credentials; it cannot fail.
            self.0
                .owner
                .map_or_else(|| unsafe { libc::geteuid() }, |(u, _)| u)
        }
    }

    /// A bound endpoint: what it accepts connections on, and whom it serves.
    pub(crate) trait Place: Send + Sync + 'static {
        type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
        fn accept(&self) -> impl Future<Output = std::io::Result<Self::Stream>> + Send;
        /// Whether a connection comes from the session's account.
        fn admits(&self, stream: &Self::Stream) -> Result<(), String>;
        /// Close it, and remove whatever it left.
        fn close(self);
    }

    /// What every connection of one endpoint shares: where it authenticates,
    /// and the leases its values are read from as each connection starts.
    struct Endpoint {
        vault: Arc<dyn VaultTools>,
        /// The leases the values are read from, by id, in the order the
        /// call leased them: never a value.
        values: Vec<String>,
        host: String,
        port: u16,
        tls: tokio_rustls::TlsConnector,
        /// The engine it speaks, MongoDB's auth database, and (P3-5d-2) how
        /// an HTTP endpoint presents the value.
        engine: Engine,
        auth_source: Option<String>,
        auth: Option<Auth>,
        /// P3-5d-2 — the endpoint's own origin, where an HTTP endpoint
        /// points a redirect to its upstream back to.
        front: String,
    }

    /// One connection's upstream, in its engine.
    enum Session {
        Pg(pg::Upstream),
        Mongo(mongo::Upstream),
        Http(http::Upstream),
    }

    impl Endpoint {
        /// The `i`th of the call's values, read from its lease now.
        fn read(&self, i: usize) -> Option<Zeroizing<String>> {
            text(self.vault.value(self.values.get(i)?)?)
        }

        /// The upstream for one connection, its values read from the leases
        /// now: `None` once any is let go.
        fn session(&self) -> Option<Session> {
            let (host, port, tls) = (self.host.clone(), self.port, self.tls.clone());
            Some(match self.engine {
                Engine::Postgres => Session::Pg(pg::Upstream {
                    host,
                    port,
                    user: self.read(0)?.to_string(),
                    password: self.read(1)?,
                    tls,
                }),
                Engine::Mongodb => Session::Mongo(mongo::Upstream {
                    host,
                    port,
                    user: self.read(0)?.to_string(),
                    password: self.read(1)?,
                    auth_source: self.auth_source.clone().unwrap_or_else(|| "admin".into()),
                    tls,
                }),
                Engine::Http => Session::Http(http::Upstream {
                    host,
                    port,
                    credential: credential(self.auth?, |i| self.read(i))?,
                    tls,
                    front: self.front.clone(),
                }),
            })
        }
    }

    /// P3-5d-2 — what an HTTP endpoint presents, from the call's values in
    /// the order it leased them: `None` where one cannot be read.
    fn credential(
        auth: Auth,
        read: impl Fn(usize) -> Option<Zeroizing<String>>,
    ) -> Option<http::Credential> {
        Some(match auth {
            Auth::Basic => http::Credential::Basic {
                user: read(0)?,
                password: read(1)?,
            },
            Auth::Bearer => http::Credential::Bearer(read(0)?),
        })
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
        /// `None` for a socket a client dials by its path (MongoDB on Unix).
        port: Option<u16>,
        uri: String,
        env: serde_json::Map<String, Value>,
    }

    /// What an endpoint is handed as it starts: its leases, those its call
    /// asked for itself, and how it reaches the upstream.
    struct Handover<'a> {
        vault: &'a Arc<dyn VaultTools>,
        leases: &'a [String],
        /// The leases the call asked for itself: let go when it closes.
        own: Vec<String>,
        /// How many of `leases` hold the values it signs in with; a CA's
        /// comes after them.
        values: usize,
        tls: tokio_rustls::TlsConnector,
        slot: Slot,
    }

    pub(crate) async fn open(
        vault: Arc<dyn VaultTools>,
        session: ObjectId,
        fence: u64,
        at: At<'_>,
        req: ConnectRequest,
    ) -> Result<Value, String> {
        if req.engine == Engine::Http {
            loopback::identifies()?;
        }
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
        let count = handles.len() - usize::from(req.ca.is_some());
        let mut values = Vec::with_capacity(count);
        for (lease, h) in leases.iter().zip(&handles).take(count) {
            values.push(value(lease, h)?);
        }
        match req.auth {
            None => {
                let user = &values[0];
                if user.is_empty() || user.chars().any(char::is_control) {
                    return Err(format!(
                        "{}: not a user name the upstream takes",
                        handles[0]
                    ));
                }
            }
            Some(auth) => {
                let carried = credential(auth, |i| values.get(i).cloned())
                    .and_then(|c| c.header())
                    .is_some();
                if !carried {
                    return Err(format!(
                        "{}: not a value an Authorization header can carry (a user with a \
                         colon; a token that is empty, or holds a space, a control character \
                         or a byte that is not ASCII)",
                        req.secret
                    ));
                }
            }
        }
        drop(values);
        let ca = match req.ca {
            Some(_) => Some(value(&leases[count], &handles[count])?),
            None => None,
        };
        let tls = tls::connector(ca.as_deref().map(String::as_str))
            .map_err(|e| format!("the upstream's TLS: {e}"))?;
        let handover = Handover {
            vault: &vault,
            leases: &leases,
            own: held.leases.clone(),
            values: count,
            tls,
            slot,
        };
        let answer = match req.engine {
            Engine::Http => {
                let bound = loopback::bind(&at)?;
                let dial = bound.dial(req.engine, None, &req.upstream);
                let front = bound.front();
                start(bound, dial, front, &req, handover)?
            }
            #[cfg(unix)]
            Engine::Postgres | Engine::Mongodb => {
                let bound = socket::bind(&at, req.engine)?;
                let dial = bound.dial(req.engine, req.database.as_deref());
                start(bound, dial, String::new(), &req, handover)?
            }
            #[cfg(windows)]
            Engine::Postgres | Engine::Mongodb => {
                let bound = loopback::bind(&at)?;
                let dial = bound.dial(req.engine, req.database.as_deref(), &req.upstream);
                start(bound, dial, String::new(), &req, handover)?
            }
        };
        // Handed to the endpoint, which lets them go when it closes.
        held.keep();
        Ok(answer)
    }

    /// Hand a bound endpoint over to its leases and serve it: whichever is
    /// let go first closes it, and every connection it holds (`stop_with`).
    /// Answers what the session is told.
    fn start<P: Place>(
        bound: P,
        dial: Dial,
        front: String,
        req: &ConnectRequest,
        h: Handover<'_>,
    ) -> Result<Value, String> {
        let stop = CancellationToken::new();
        for lease in h.leases {
            if !h.vault.stop_with(lease, stop.clone()) {
                stop.cancel();
                bound.close();
                return Err(
                    "a lease ended before the endpoint was handed over: it is closed".into(),
                );
            }
        }
        let ttl_s = h
            .leases
            .iter()
            .filter_map(|l| h.vault.left(l))
            .min()
            .map_or(0, |d| d.as_secs());
        let endpoint = Arc::new(Endpoint {
            vault: Arc::clone(h.vault),
            values: h.leases[..h.values].to_vec(),
            host: req.host.clone(),
            port: req.port,
            tls: h.tls,
            engine: req.engine,
            auth_source: req.auth_source.clone(),
            auth: req.auth,
            front,
        });
        tokio::spawn(serve(bound, endpoint, stop, h.own, h.slot));
        let note = match req.engine {
            Engine::Http => {
                "Send requests to uri as you would to the upstream, with no credential: the \
                 endpoint adds the secret's. For git, set env, and use the upstream's own URLs. \
                 It closes, with its connections, when its lease ends."
            }
            Engine::Postgres | Engine::Mongodb => {
                "Connect with any user name and no password: the endpoint signs in to the \
                 upstream as the secret's user. It closes, with its connections, when its lease \
                 ends."
            }
        };
        Ok(json!({
            "engine": req.engine.wire(),
            "upstream": req.upstream,
            "host": dial.host,
            "port": dial.port,
            "database": req.database,
            "uri": dial.uri,
            "env": dial.env,
            "ttl_s": ttl_s,
            "note": note,
        }))
    }

    /// Serve one endpoint until its first lease is let go: each connection
    /// from the session's account, at most [`MAX_CONNECTIONS`] at once, one
    /// engine session each. Then it closes, the leases its call asked for
    /// itself (`own`) are let go rather than held to their expiry, and its
    /// place among the run's endpoints goes with it.
    async fn serve<P: Place>(
        bound: P,
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
                    endpoint.engine,
                    stream,
                    format!("the endpoint serves {MAX_CONNECTIONS} connections at once"),
                ));
                continue;
            };
            let (endpoint, stop) = (Arc::clone(&endpoint), stop.clone());
            tokio::spawn(async move {
                let _permit = permit;
                let Some(session) = endpoint.session() else {
                    return refuse(
                        endpoint.engine,
                        stream,
                        "the endpoint's lease has ended".into(),
                    )
                    .await;
                };
                let served = async {
                    match &session {
                        Session::Pg(up) => pg::serve(stream, up).await,
                        Session::Mongo(up) => mongo::serve(stream, up).await,
                        Session::Http(up) => http::serve(stream, up).await,
                    }
                };
                tokio::select! {
                    _ = stop.cancelled() => {}
                    served = served => {
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

    /// Tell a client why it is not served, in its engine's wire, and hang
    /// up: a Postgres client is sent a fatal error; a MongoDB client, which
    /// speaks first, has its first message answered; an HTTP client, its
    /// first request.
    async fn refuse<S: AsyncRead + AsyncWrite + Unpin>(engine: Engine, mut stream: S, why: String) {
        let told = async {
            match engine {
                Engine::Postgres => stream.write_all(&pg::fatal(&why)).await,
                Engine::Mongodb => mongo::refuse(&mut stream, &why).await,
                Engine::Http => {
                    if let Err(e) = http::refuse(&mut stream, &why).await {
                        debug!(%e, "vault: a refused HTTP client was not told why");
                    }
                    Ok(())
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), told).await;
    }

    /// Unix: a socket in a directory of the session's runtime directory,
    /// handed to the session's account, each connection's uid checked. A
    /// database's endpoint.
    #[cfg(unix)]
    mod socket {
        use std::path::{Path, PathBuf};

        use serde_json::json;
        use tokio::net::{UnixListener, UnixStream};

        use super::{At, Dial, Engine, Place, encode};

        /// Each session's endpoints, under its runtime directory.
        pub(crate) const PROXIES_DIR: &str = "proxies";
        /// The port a Postgres endpoint's socket is named for: a client dials
        /// `host=<directory>` and, unless told otherwise, port 5432.
        pub(crate) const PG_PORT: u16 = 5432;
        /// P3-5c-2 — a MongoDB endpoint's socket: a client dials a host that
        /// ends in `.sock` as a socket's path.
        pub(crate) const MONGO_SOCKET: &str = "mongodb-27017.sock";

        /// A bound endpoint: its socket, its directory, and the one uid a
        /// connection is taken from.
        pub(crate) struct Bound {
            listener: UnixListener,
            dir: PathBuf,
            socket: PathBuf,
            peer: u32,
        }

        impl Place for Bound {
            type Stream = UnixStream;

            async fn accept(&self) -> std::io::Result<UnixStream> {
                self.listener.accept().await.map(|(stream, _)| stream)
            }

            /// Whether a connection comes from the session's account.
            fn admits(&self, stream: &UnixStream) -> Result<(), String> {
                match stream.peer_cred() {
                    Ok(c) if c.uid() == self.peer => Ok(()),
                    Ok(c) => Err(format!("uid {}", c.uid())),
                    Err(e) => Err(format!("a peer that could not be identified: {e}")),
                }
            }

            /// Close the socket, and remove its directory.
            fn close(self) {
                drop(self.listener);
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        impl Bound {
            pub(crate) fn dial(&self, engine: Engine, database: Option<&str>) -> Dial {
                let mut env = serde_json::Map::new();
                if engine == Engine::Mongodb {
                    // The socket's whole path, `/` and all, encoded as the host.
                    let host = self.socket.to_string_lossy().into_owned();
                    let uri = format!(
                        "mongodb://{}/{}?directConnection=true",
                        encode(&host, false),
                        database.map(|db| encode(db, false)).unwrap_or_default()
                    );
                    env.insert("MONGODB_URI".into(), json!(uri));
                    return Dial {
                        host,
                        port: None,
                        uri,
                        env,
                    };
                }
                let host = self.dir.to_string_lossy().into_owned();
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
                    port: Some(PG_PORT),
                    uri,
                    env,
                }
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
        pub(crate) fn bind(at: &At<'_>, engine: Engine) -> Result<Bound, String> {
            use std::os::unix::fs::PermissionsExt;
            let super::super::ExecTarget {
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
                let socket = dir.join(match engine {
                    Engine::Mongodb => MONGO_SOCKET.to_string(),
                    Engine::Postgres | Engine::Http => format!(".s.PGSQL.{PG_PORT}"),
                });
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
                Ok((listener, socket))
            })();
            match made {
                Ok((listener, socket)) => Ok(Bound {
                    listener,
                    dir,
                    socket,
                    peer: at.uid(),
                }),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    Err(e)
                }
            }
        }
    }

    /// P3-5b-4, P3-5d-2 — a loopback port, for a client that cannot dial a
    /// Unix socket: every endpoint on Windows, whose database clients mostly
    /// cannot, and an HTTP one everywhere, since git cannot. Each
    /// connection's account is read from the system (its table of
    /// connections; on macOS the descriptors each process holds), and only
    /// the session's is served.
    ///
    /// ⚠️ Any process on the device can connect to the port; only the
    /// session's account is served. The port cannot be taken over by another
    /// account's socket. Windows makes a bound port shareable only where its
    /// first socket asked for that (`SO_REUSEADDR`), which this one does not,
    /// and SYSTEM is the only exception. Linux shares one only between
    /// sockets of one owner that both set `SO_REUSEPORT`, and macOS only
    /// between sockets that all set it, which this one does not.
    mod loopback {
        use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

        use serde_json::json;
        use tokio::net::{TcpListener, TcpStream};

        use super::{At, Dial, Engine, Place, encode};

        /// Whether this system can say which account a loopback connection
        /// comes from: Windows by its connection table, Linux by
        /// `/proc/net/tcp`, macOS (P3-5d-3) by the descriptors each process
        /// holds.
        const IDENTIFIES: bool = cfg!(any(windows, target_os = "linux", target_os = "macos"));

        /// A bound endpoint: its listener, its address, and the one account a
        /// connection is taken from.
        pub(crate) struct Bound {
            listener: TcpListener,
            addr: SocketAddrV4,
            #[cfg(unix)]
            peer: u32,
            #[cfg(windows)]
            peer: String,
        }

        impl Place for Bound {
            type Stream = TcpStream;

            async fn accept(&self) -> std::io::Result<TcpStream> {
                self.listener.accept().await.map(|(stream, _)| stream)
            }

            /// Whether a connection comes from a process running as the
            /// session's account.
            fn admits(&self, stream: &TcpStream) -> Result<(), String> {
                let SocketAddr::V4(client) = stream.peer_addr().map_err(|e| e.to_string())? else {
                    return Err("a client that is not IPv4 loopback".into());
                };
                self.account(client)
            }

            fn close(self) {
                drop(self.listener);
            }
        }

        impl Bound {
            /// Windows: the connection's row of the table names its process,
            /// and the process's token the account.
            #[cfg(windows)]
            fn account(&self, client: SocketAddrV4) -> Result<(), String> {
                let pid = crate::hive_win::tcp_client_pid(client, self.addr)
                    .ok_or("a connection no process owns any more")?;
                let sid = crate::hive_win::process_sid(pid)?;
                if sid == self.peer {
                    Ok(())
                } else {
                    Err(format!("{sid} (pid {pid})"))
                }
            }

            /// Linux (P3-5d-2): the kernel's row for the connection's client
            /// end names the account that owns its socket.
            #[cfg(target_os = "linux")]
            fn account(&self, client: SocketAddrV4) -> Result<(), String> {
                match super::super::proc_net::owner(client, self.addr) {
                    Some(uid) if uid == self.peer => Ok(()),
                    Some(uid) => Err(format!("uid {uid}")),
                    None => Err("a connection no live socket owns".into()),
                }
            }

            /// macOS (P3-5d-3): the connection's client end is held, and held
            /// only, by processes running as the session's account.
            #[cfg(target_os = "macos")]
            fn account(&self, client: SocketAddrV4) -> Result<(), String> {
                if super::super::libproc::held_by(client, self.addr, self.peer) {
                    Ok(())
                } else {
                    Err(format!(
                        "a client end not held by processes of uid {} alone",
                        self.peer
                    ))
                }
            }

            /// Elsewhere: nothing says who connected, so nobody is served.
            #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
            fn account(&self, _client: SocketAddrV4) -> Result<(), String> {
                Err(format!(
                    "this system cannot say which account connected (uid {} is served)",
                    self.peer
                ))
            }

            /// The endpoint's own origin: where an HTTP endpoint points a
            /// redirect to its upstream back to.
            pub(crate) fn front(&self) -> String {
                format!("http://{}", self.addr)
            }

            pub(crate) fn dial(
                &self,
                engine: Engine,
                database: Option<&str>,
                upstream: &str,
            ) -> Dial {
                let (host, port) = (self.addr.ip().to_string(), self.addr.port());
                let mut env = serde_json::Map::new();
                match engine {
                    Engine::Http => {
                        let uri = self.front();
                        // git's own configuration, by environment: the
                        // upstream's URLs go through the endpoint, and a
                        // refusal fails at once rather than waiting at a
                        // prompt nobody answers.
                        env.insert("GIT_CONFIG_COUNT".into(), json!("1"));
                        env.insert(
                            "GIT_CONFIG_KEY_0".into(),
                            json!(format!("url.{uri}/.insteadOf")),
                        );
                        env.insert("GIT_CONFIG_VALUE_0".into(), json!(format!("{upstream}/")));
                        env.insert("GIT_TERMINAL_PROMPT".into(), json!("0"));
                        Dial {
                            host,
                            port: Some(port),
                            uri,
                            env,
                        }
                    }
                    Engine::Mongodb => {
                        let uri = format!(
                            "mongodb://{host}:{port}/{}?directConnection=true",
                            database.map(|db| encode(db, false)).unwrap_or_default()
                        );
                        env.insert("MONGODB_URI".into(), json!(uri));
                        Dial {
                            host,
                            port: Some(port),
                            uri,
                            env,
                        }
                    }
                    Engine::Postgres => {
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
                            port: Some(port),
                            uri,
                            env,
                        }
                    }
                }
            }
        }

        /// Whether this system can say who connects to a loopback port: a
        /// call it cannot serve is refused before anything is leased.
        pub(crate) fn identifies() -> Result<(), String> {
            if IDENTIFIES {
                Ok(())
            } else {
                Err(
                    "this device cannot tell which account connects to a loopback port, so it \
                     serves no endpoint there"
                        .into(),
                )
            }
        }

        /// A listener on `127.0.0.1`, on a port the system picks, for the
        /// session's account; refused where the system cannot say who
        /// connects to it.
        pub(crate) fn bind(at: &At<'_>) -> Result<Bound, String> {
            identifies()?;
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
                #[cfg(unix)]
                peer: at.uid(),
                #[cfg(windows)]
                peer: at.0.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests;
