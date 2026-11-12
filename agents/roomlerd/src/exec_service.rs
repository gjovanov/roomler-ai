// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-94 — Fleet RPC as SYSTEM on Windows, the device's half
//! (`docs/fr/FR-94-fleet-rpc-system-on-windows.md`).
//!
//! On an attended install the worker, the process that holds the agent
//! socket, runs as the signed-in user, and everything it reads (the agent
//! token, the server origin, `exec_enabled`) sits in that user's writable
//! `%APPDATA%`. So the SYSTEM service in session 0 trusts none of it:
//!
//! - **The anchor** (`HKLM\SOFTWARE\Roomler\ExecService`, #3737) is the
//!   service's ONLY source of the server origin, the device, and its own
//!   credential. An elevated administrator writes it (`roomlerd anchor
//!   create`), and the service reads it only from that key, when SYSTEM or
//!   Administrators own it, its DACL lets no other account change it or read
//!   its values, and its parent lets no other account change it
//!   ([`load_anchor`], `roomler_node_core::machine_key`). Builds before #3737
//!   kept it in `%ProgramData%\roomler\roomler\exec-service\anchor.json`: the
//!   service's first start on this build copies one from there once, in the
//!   exact shape `roomlerd anchor create` wrote, and nothing reads that file
//!   after ([`store`]).
//! - **The doorbell** (`\\.\pipe\roomler-exec-service`) carries a request id
//!   and nothing else. The service claims the command from the server with
//!   its own credential, runs it, and posts the result to the server itself,
//!   so SYSTEM's output never passes through a process the user owns.
//! - **The worker** asks the person (consent), rings, and advertises
//!   `exec-system` only on the connection of the device the service is
//!   anchored for ([`advertised_for`]).
//!
//! ⚠️ Nothing the signed-in user can write may decide what SYSTEM runs
//! (FR-94 §4). A forged ring claims nothing the server did not queue for this
//! device; a squatted pipe is refused by its owner, and an anchor key by its
//! owner, its DACL and its parent's.
//!
//! Gate 4 has two halves on this path, and both must say yes: the person's
//! `exec_enabled`, checked by the worker before it rings, exactly as for a
//! command it runs itself, and the anchor's `exec_enabled`, the machine
//! administrator's, checked by the service before it claims.

use serde::{Deserialize, Serialize};

/// The doorbell's pipe. One per machine, like the daemon.
pub const PIPE_NAME: &str = r"\\.\pipe\roomler-exec-service";

/// `ROOMLERD_EXEC_VIA_SERVICE=0` (or `false`/`off`/`no`) withdraws
/// `exec-system` from every connection, so the server routes nothing to the
/// service. The FR's P3 kill switch.
pub const KILL_SWITCH_ENV: &str = "ROOMLERD_EXEC_VIA_SERVICE";

/// #3737 — `ROOMLERD_EXEC_ANCHOR_COPY=0` (or `false`/`off`/`no`), in the
/// SYSTEM service's own environment, stops the one-time copy of an anchor
/// from where builds before the registry key kept it: the first start
/// records `refused`, "switched off", and copies nothing. An administrator
/// anchors the device again to recover. It never makes a file be read as an
/// anchor.
pub const COPY_SWITCH_ENV: &str = "ROOMLERD_EXEC_ANCHOR_COPY";

/// The anchor file's schema version.
const ANCHOR_VERSION: u32 = 1;

/// A request or a reply on the doorbell is one line; anything longer is
/// refused unread.
const MAX_LINE: usize = 4 * 1024;

/// Renew the service credential once fewer than this many seconds of its
/// one-year life are left.
const RENEW_BEFORE_SECS: i64 = 120 * 24 * 60 * 60;

/// Is the kill switch set? Only an explicit "off" spelling counts: an unset
/// or unparseable value leaves the feature as the anchor says.
pub fn disabled_by(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// A Mongo ObjectId in hex: every id this module puts in a URL path or
/// compares (request, device, org). Strict, so a ring cannot smuggle a path
/// into the claim URL.
pub fn valid_object_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The server origin the service may talk to, normalised (no trailing `/`).
///
/// HTTPS only, except a loopback test server: the claim's answer IS the
/// command SYSTEM runs, so a plain-HTTP origin would let anyone on the path
/// hand SYSTEM a command of their choosing.
pub fn origin_ok(server: &str) -> Result<String, String> {
    let url =
        reqwest::Url::parse(server.trim()).map_err(|e| format!("{server:?} is not a URL: {e}"))?;
    let host = url.host_str().unwrap_or_default();
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        other => {
            return Err(format!(
                "the SYSTEM service talks to its server over https only, not {other}: the \
                 server's answer is the command SYSTEM runs (plain http is allowed for a \
                 loopback test server only)"
            ));
        }
    }
    if url.query().is_some() || url.fragment().is_some() || host.is_empty() {
        return Err(format!(
            "{server:?} must be a bare origin, like https://roomler.ai"
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// What an elevated administrator wrote, and all the SYSTEM service trusts.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Anchor {
    pub version: u32,
    /// The server's origin: where the service claims commands and posts
    /// results. Only ever from here, never from the user's config.
    pub server: String,
    pub tenant_id: String,
    pub agent_id: String,
    /// The device's name as the server knew it at anchoring, for
    /// `roomlerd anchor status`.
    #[serde(default)]
    pub device_name: String,
    /// The service's credential (`TokenType::AgentService`).
    pub service_token: String,
    /// Gate 4 for the SYSTEM path, the machine administrator's half. The
    /// worker checks the person's `exec_enabled` before it rings.
    pub exec_enabled: bool,
    /// RFC 3339.
    #[serde(default)]
    pub anchored_at: String,
    /// FR-105 — how far roomler SSH through this service may go, the
    /// machine administrator's (`roomlerd anchor ssh <…>`). Default off: an
    /// anchor made for exec never silently serves SYSTEM SSH, and an anchor
    /// file older than FR-105 reads as off.
    #[serde(default)]
    pub ssh: roomler_ai_remote_control::models::SshServiceCeiling,
    /// Keys a newer build wrote, kept so that this build's save (a renewal,
    /// `anchor exec`) never erases them: the #2078 shape, a clean save that
    /// drops what it does not know.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Never prints the credential: an anchor goes into logs and error messages.
impl std::fmt::Debug for Anchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anchor")
            .field("version", &self.version)
            .field("server", &self.server)
            .field("tenant_id", &self.tenant_id)
            .field("agent_id", &self.agent_id)
            .field("device_name", &self.device_name)
            .field("service_token", &"[redacted]")
            .field("exec_enabled", &self.exec_enabled)
            .field("anchored_at", &self.anchored_at)
            .field("ssh", &self.ssh)
            // Names only: a newer build's value is not this build's to judge
            // safe to print.
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Anchor {
    /// Everything [`load_anchor`] requires of a file's content once its
    /// owner and DACL have passed.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != ANCHOR_VERSION {
            return Err(format!(
                "anchor version {} is not {ANCHOR_VERSION}: anchor the device again with this \
                 build",
                self.version
            ));
        }
        origin_ok(&self.server)?;
        if !valid_object_id(&self.tenant_id) || !valid_object_id(&self.agent_id) {
            return Err("the anchor's org or device id is malformed".into());
        }
        if self.service_token.trim().is_empty() {
            return Err("the anchor holds no credential".into());
        }
        Ok(())
    }
}

/// One request on the doorbell. A request id is the most it ever carries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DoorbellRequest {
    /// Is the service anchored, and for which device? Answers no secret.
    Health,
    /// Claim this command from the server and run it.
    Ring { request_id: String },
    /// Kill this command, if the service is running it.
    Cancel { request_id: String },
    /// FR-105 — claim this SSH grant from the server and hold it for the
    /// service's own SSH endpoint (`ssh_service`).
    SshGrant { grant_id: String },
}

/// The service's answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DoorbellReply {
    Health {
        anchored: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(default)]
        exec_enabled: bool,
        /// Why an anchor that exists is not used (not trusted, malformed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<String>,
        version: String,
        /// FR-105 — the anchor's SSH ceiling, when this service serves SSH
        /// (its pipe is up). Absent from a service that does not, including
        /// every one older than FR-105.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh: Option<roomler_ai_remote_control::models::SshServiceCeiling>,
    },
    /// `claimed: true` = the service holds the command now, and the server
    /// hears the result from it. Anything else names why nothing runs.
    Rang {
        claimed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Cancelled {
        found: bool,
    },
    Refused {
        error: String,
    },
}

/// The `exp` of a JWT, read without verifying it: the service only uses it
/// to decide when to renew its own credential, which the server verifies.
pub fn token_expiry(token: &str) -> Option<i64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}

/// Renew when fewer than [`RENEW_BEFORE_SECS`] are left, or when the expiry
/// cannot be read at all.
pub fn renewal_due(exp: Option<i64>, now: i64) -> bool {
    exp.is_none_or(|exp| exp - now < RENEW_BEFORE_SECS)
}

/// Rings are the only request that costs the server a call (and, when
/// refused, an audit row), so they are rate limited: a token bucket of
/// [`RingLimiter::BURST`], refilled at [`RingLimiter::PER_MINUTE`].
#[derive(Debug)]
pub struct RingLimiter {
    tokens: f64,
    last: std::time::Instant,
}

impl Default for RingLimiter {
    fn default() -> Self {
        Self::new(std::time::Instant::now())
    }
}

impl RingLimiter {
    pub const BURST: f64 = 10.0;
    pub const PER_MINUTE: f64 = 30.0;

    pub fn new(now: std::time::Instant) -> Self {
        Self {
            tokens: Self::BURST,
            last: now,
        }
    }

    /// Take one ring's token at `now`, if there is one.
    pub fn allow(&mut self, now: std::time::Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * Self::PER_MINUTE / 60.0).min(Self::BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Read one `\n`-terminated line of at most [`MAX_LINE`] bytes.
async fn read_line<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<String, String> {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = r
            .read(&mut byte)
            .await
            .map_err(|e| format!("reading the doorbell: {e}"))?;
        if n == 0 {
            return Err("the other end closed the doorbell mid-line".into());
        }
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() >= MAX_LINE {
            return Err(format!("a doorbell line longer than {MAX_LINE} bytes"));
        }
        buf.push(byte[0]);
    }
    String::from_utf8(buf).map_err(|_| "a doorbell line that is not UTF-8".to_string())
}

async fn write_line<W: tokio::io::AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    value: &T,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;
    let mut line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("writing the doorbell: {e}"))?;
    w.flush()
        .await
        .map_err(|e| format!("writing the doorbell: {e}"))
}

/// The service's side of one connection: one request, one reply. Generic
/// over the stream so the tests can drive it without a pipe.
pub async fn serve_connection<S, F, Fut>(mut stream: S, handle: F)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnOnce(DoorbellRequest) -> Fut,
    Fut: std::future::Future<Output = DoorbellReply>,
{
    // A peer that connects and says nothing is dropped, so a handful of
    // idle connections cannot hold every instance.
    let line = match tokio::time::timeout(std::time::Duration::from_secs(5), read_line(&mut stream))
        .await
    {
        Ok(Ok(line)) => line,
        Ok(Err(e)) => {
            tracing::debug!(%e, "exec-service: a doorbell connection sent nothing usable");
            return;
        }
        Err(_) => return,
    };
    let reply = match serde_json::from_str::<DoorbellRequest>(&line) {
        Ok(req) => handle(req).await,
        Err(_) => DoorbellReply::Refused {
            error: "not a doorbell request".into(),
        },
    };
    if let Err(e) = write_line(&mut stream, &reply).await {
        tracing::debug!(%e, "exec-service: could not answer a doorbell connection");
    }
}

/// The worker's side of one connection: send `req`, wait up to `wait` for
/// the reply.
pub async fn exchange<S>(
    mut stream: S,
    req: &DoorbellRequest,
    wait: std::time::Duration,
) -> Result<DoorbellReply, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_line(&mut stream, req).await?;
    let line = tokio::time::timeout(wait, read_line(&mut stream))
        .await
        .map_err(|_| {
            format!(
                "the SYSTEM service did not answer within {} s",
                wait.as_secs()
            )
        })??;
    serde_json::from_str(&line).map_err(|e| format!("the SYSTEM service's answer: {e}"))
}

// ────────────────────────────────────────────────────────────────────────────
// The worker's side
// ────────────────────────────────────────────────────────────────────────────

/// The device the service last said it is anchored for, from the health
/// watch. `None` = not anchored, no service, or not asked yet.
static ANCHORED_AGENT: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn anchored_agent() -> Option<String> {
    match ANCHORED_AGENT.read() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Only the Windows health watch sets it; elsewhere nothing is ever anchored.
#[cfg_attr(not(windows), allow(dead_code))]
fn set_anchored_agent(agent: Option<String>) {
    let mut g = match ANCHORED_AGENT.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *g != agent {
        tracing::info!(
            anchored_for = ?agent,
            "exec-service: the SYSTEM service's anchor changed"
        );
        *g = agent;
    }
}

/// Should the connection for `agent_id` advertise `exec-system`? Only when
/// this device's SYSTEM service answered that it is anchored for exactly that
/// device, and the kill switch is not set. The anchor names one device, so a
/// secondary org's connection never offers it.
pub fn advertised_for(agent_id: &str) -> bool {
    if disabled_by(std::env::var(KILL_SWITCH_ENV).ok().as_deref()) {
        return false;
    }
    anchored_agent().is_some_and(|a| a == agent_id)
}

/// FR-105 — the SSH ceiling the service last said it serves (its health
/// reply's `ssh`): `None` = no SSH pipe, no anchor, or a service older than
/// FR-105.
static SERVICE_SSH: std::sync::RwLock<
    Option<roomler_ai_remote_control::models::SshServiceCeiling>,
> = std::sync::RwLock::new(None);

/// Only the Windows health watch sets it.
#[cfg_attr(not(windows), allow(dead_code))]
fn set_service_ssh(ssh: Option<roomler_ai_remote_control::models::SshServiceCeiling>) {
    let mut g = match SERVICE_SSH.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *g != ssh {
        tracing::info!(?ssh, "ssh-service: the SYSTEM service's SSH changed");
        *g = ssh;
    }
}

/// FR-105 — does this device's SYSTEM service serve SSH for `agent_id`: anchored
/// for exactly that device, its SSH pipe up, and a ceiling above off? The
/// worker's half of `ssh-system` (`ssh_service::advertised_for`).
pub fn ssh_served_for(agent_id: &str) -> bool {
    let ssh = match SERVICE_SSH.read() {
        Ok(g) => *g,
        Err(poisoned) => *poisoned.into_inner(),
    };
    anchored_agent().is_some_and(|a| a == agent_id)
        && ssh.is_some_and(|c| c != roomler_ai_remote_control::models::SshServiceCeiling::Off)
}

/// Put `exec-system` in `rpc`, or take it out, by equality: `exec` is a
/// prefix of it and stays either way.
pub fn apply_word(rpc: &mut Vec<String>, on: bool) {
    let word = roomler_ai_remote_control::models::RpcCap::ExecSystem.wire();
    rpc.retain(|w| w != word);
    if on {
        rpc.push(word.to_string());
    }
}

/// How often the worker asks the service whether it is anchored. A change
/// reaches the server on the next heartbeat after it.
#[cfg(windows)]
const HEALTH_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Keep [`advertised_for`]'s answer fresh: ask the service now, then every
/// [`HEALTH_EVERY`]. A no-op off Windows.
pub fn spawn_health_watch() {
    #[cfg(windows)]
    tokio::spawn(async {
        loop {
            let (agent, ssh) = match win::ask(
                &DoorbellRequest::Health,
                std::time::Duration::from_secs(3),
            )
            .await
            {
                Ok(DoorbellReply::Health {
                    anchored: true,
                    agent_id,
                    problem: None,
                    ssh,
                    ..
                }) => (agent_id, ssh),
                Ok(DoorbellReply::Health {
                    problem: Some(p), ..
                }) => {
                    tracing::warn!(problem = %p, "exec-service: the SYSTEM service's anchor is not used");
                    (None, None)
                }
                Ok(_) => (None, None),
                Err(e) => {
                    tracing::debug!(%e, "exec-service: no answer from the SYSTEM service");
                    (None, None)
                }
            };
            set_anchored_agent(agent);
            set_service_ssh(ssh);
            tokio::time::sleep(HEALTH_EVERY).await;
        }
    });
}

/// Ring the service for `request_id`. `Ok` = the service claimed the command
/// and will post its result to the server itself. `Err` names why nothing
/// runs, for the worker to report as the command's answer.
pub async fn ring(request_id: &str) -> Result<(), String> {
    if !valid_object_id(request_id) {
        return Err("a malformed request id".into());
    }
    #[cfg(windows)]
    {
        let req = DoorbellRequest::Ring {
            request_id: request_id.to_string(),
        };
        // The service claims over HTTPS before it answers.
        match win::ask(&req, std::time::Duration::from_secs(30)).await? {
            DoorbellReply::Rang { claimed: true, .. } => Ok(()),
            DoorbellReply::Rang { error, .. } => {
                Err(error
                    .unwrap_or_else(|| "the SYSTEM service did not take the command".to_string()))
            }
            DoorbellReply::Refused { error } => Err(error),
            other => Err(format!("the SYSTEM service answered {other:?}")),
        }
    }
    #[cfg(not(windows))]
    {
        Err("only a Windows device runs commands through a SYSTEM service".into())
    }
}

/// FR-105 — hand an SSH grant to the service: it claims the grant from the
/// server with its own credential, and that claim is the server's
/// acknowledgement. `Err` names why the service did not take it, for the
/// worker to answer the server with a refusal.
pub async fn ring_ssh(grant_id: &str) -> Result<(), String> {
    if !valid_object_id(grant_id) {
        return Err("a malformed grant id".into());
    }
    #[cfg(windows)]
    {
        let req = DoorbellRequest::SshGrant {
            grant_id: grant_id.to_string(),
        };
        // Past the service's own claim timeout, and inside the server's
        // window for the claim (10 s from the push).
        match win::ask(&req, std::time::Duration::from_secs(10)).await? {
            DoorbellReply::Rang { claimed: true, .. } => Ok(()),
            DoorbellReply::Rang { error, .. } => Err(error
                .unwrap_or_else(|| "the SYSTEM service did not take the SSH grant".to_string())),
            DoorbellReply::Refused { error } => Err(error),
            other => Err(format!("the SYSTEM service answered {other:?}")),
        }
    }
    #[cfg(not(windows))]
    {
        Err("only a Windows device serves SSH through a SYSTEM service".into())
    }
}

/// Ask the service to kill `request_id`. `true` = it was running there.
pub async fn cancel(request_id: &str) -> bool {
    if !valid_object_id(request_id) {
        return false;
    }
    #[cfg(windows)]
    {
        let req = DoorbellRequest::Cancel {
            request_id: request_id.to_string(),
        };
        matches!(
            win::ask(&req, std::time::Duration::from_secs(5)).await,
            Ok(DoorbellReply::Cancelled { found: true })
        )
    }
    #[cfg(not(windows))]
    {
        false
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Where the anchor lives
// ────────────────────────────────────────────────────────────────────────────

/// The anchor, if there is one this process may trust. `Ok(None)` = none.
/// `Err` = one is there, but where it is or what it holds says nobody should
/// act on it.
#[cfg(windows)]
pub fn load_anchor() -> Result<Option<Anchor>, String> {
    store::load_anchor()
}

#[cfg(not(windows))]
pub fn load_anchor() -> Result<Option<Anchor>, String> {
    Ok(None)
}

/// FR-105 — the SSH pipe is made with the doorbell's DACL and opened with its
/// owner check.
#[cfg(all(windows, feature = "ssh-server"))]
pub(crate) use win::{open_system_pipe, pipe_instance};

/// FR-105 — the service's host key (OpenSSH PEM), if there is one this process
/// may trust: kept as the anchor is, under the same rules.
#[cfg(windows)]
pub fn load_host_key() -> Result<Option<String>, String> {
    store::load_host_key()
}

/// FR-105 — the host key, made now if there is none. Only an anchored device
/// ever holds one: the caller has the anchor already.
#[cfg(all(windows, feature = "ssh-server"))]
pub fn ensure_host_key() -> Result<String, String> {
    store::ensure_host_key()
}

/// #3737 — V2 of the one-time copy: the folder and the file are exactly as
/// `roomlerd anchor create` left them in every build that kept the anchor in
/// a file: Administrators own both, and each has a protected DACL of SYSTEM
/// and Administrators with full control and nothing else, handed down on the
/// folder (`OI`, `CI`) and not on the file. That writer never wrote any other
/// shape, so the copy takes no other.
#[cfg_attr(not(windows), allow(dead_code))]
fn legacy_shape_ok(
    folder: &roomler_node_core::machine_tree::Sd,
    file: &roomler_node_core::machine_tree::Sd,
) -> bool {
    use roomler_node_core::machine_tree::{
        ACE_ALLOW, ADMINISTRATORS, CONTAINER_INHERIT, FULL, OBJECT_INHERIT, SYSTEM, Sd,
    };
    let exactly = |sd: &Sd, flags: u8| {
        sd.owner == ADMINISTRATORS
            && sd.protected
            && sd.dacl.as_ref().is_some_and(|aces| {
                let mut sids: Vec<&str> = aces
                    .iter()
                    .filter(|a| a.kind == ACE_ALLOW && a.flags == flags && a.mask == FULL)
                    .map(|a| a.sid.as_str())
                    .collect();
                sids.sort_unstable();
                aces.len() == 2 && sids == [SYSTEM, ADMINISTRATORS]
            })
    };
    exactly(folder, OBJECT_INHERIT | CONTAINER_INHERIT) && exactly(file, 0)
}

/// #3737 — V1 and V3 of the one-time copy: the anchor passes FR-94's whole
/// check of what it holds ([`Anchor::validate`]), and its origin is https. A
/// loopback origin, which `origin_ok` allows over plain http, is a lab's and
/// never a field anchor's, so the copy takes none.
#[cfg_attr(not(windows), allow(dead_code))]
fn copyable(anchor: &Anchor) -> Result<(), String> {
    anchor.validate()?;
    let url = reqwest::Url::parse(anchor.server.trim()).map_err(|e| e.to_string())?;
    if url.scheme() != "https" {
        return Err(format!(
            "the anchor names {}, which is not an https origin",
            anchor.server
        ));
    }
    Ok(())
}

/// #3737 — where the anchor and the host key are kept: the registry key
/// `HKLM\SOFTWARE\Roomler\ExecService`, and the one-time copy from where
/// builds before it kept them.
///
/// Four invariants, each load-bearing:
///
/// - **[`load_anchor`] never names a path under `%ProgramData%`.** The key
///   is the only place an anchor is read from; the files of the anchor's
///   first home are read by the one-time copy alone ([`super::legacy`]).
/// - **No switch makes it.** No setting, variable or config makes the service
///   read a file as an anchor again: such a switch would undo, on the device
///   that set it, what the key is for. `ROOMLERD_EXEC_ANCHOR_COPY=0` only
///   stops the copy.
/// - **The key is written only by an elevated administrator** (`roomlerd
///   anchor`, Setup's elevated anchor step) **or SYSTEM** (the renewal, the
///   host key, the one-time copy). Its DACL names nobody else, and only they
///   may create a key below `HKLM\SOFTWARE`.
/// - **The copy runs at most once per machine.** Its marker, `LegacyCopy`, is
///   written before anything is read, and a start that finds it copies
///   nothing.
#[cfg(windows)]
pub(crate) mod store {
    use roomler_node_core::machine_key::{self, Key, KeyPath};
    use serde::{Deserialize, Serialize};

    use super::{Anchor, legacy};

    /// The anchor, the JSON `anchor.json` held: 64 Ki characters at most.
    const ANCHOR: &str = "Anchor";
    const MAX_ANCHOR: u32 = 128 * 1024;
    /// FR-105's host key, OpenSSH PEM: 16 Ki characters at most.
    const HOST_KEY: &str = "SshHostKey";
    const MAX_HOST_KEY: u32 = 32 * 1024;
    /// What the one-time copy did ([`LegacyCopy`]): no secret.
    const LEGACY_COPY: &str = "LegacyCopy";
    const MAX_LEGACY_COPY: u32 = 8 * 1024;

    /// `HKLM\SOFTWARE\Roomler\ExecService`. A test build takes its stand-in
    /// from [`super::test_fixture`], and stops when there is none.
    pub(crate) fn key() -> KeyPath {
        #[cfg(test)]
        {
            super::test_fixture::key()
        }
        #[cfg(not(test))]
        {
            KeyPath {
                hive: machine_key::Hive::LocalMachine,
                parent: r"SOFTWARE\Roomler".to_string(),
                name: "ExecService".to_string(),
            }
        }
    }

    fn holds(key: &Key, name: &str) -> Result<bool, String> {
        Ok(key.names()?.iter().any(|n| n == name))
    }

    /// The anchor, from the key alone, when the key passes the check
    /// (`machine_key::open_checked`). `Ok(None)`: no key, or no anchor in it.
    pub fn load_anchor() -> Result<Option<Anchor>, String> {
        let path = key();
        let Some(key) = machine_key::open_checked(&path)? else {
            return Ok(None);
        };
        let Some(text) = key.read(ANCHOR, MAX_ANCHOR)? else {
            return Ok(None);
        };
        let anchor: Anchor = serde_json::from_str(&text)
            .map_err(|e| format!("the anchor in {}: {e}", path.shown()))?;
        anchor.validate()?;
        Ok(Some(anchor))
    }

    /// FR-105's host key, from the same key under the same check.
    pub fn load_host_key() -> Result<Option<String>, String> {
        match machine_key::open_checked(&key())? {
            Some(key) => key.read(HOST_KEY, MAX_HOST_KEY),
            None => Ok(None),
        }
    }

    /// The host key, made now if there is none. A key that fails the check
    /// holds nothing worth keeping: the writer takes it over and empties it,
    /// so an administrator's `anchor create --ssh …` recovers it.
    #[cfg(feature = "ssh-server")]
    pub fn ensure_host_key() -> Result<String, String> {
        if let Ok(Some(pem)) = load_host_key() {
            return Ok(pem);
        }
        let pem = crate::ssh::generate_host_key().map_err(|e| format!("making a host key: {e}"))?;
        machine_key::create_sealed(&key(), true)?.write(HOST_KEY, &pem)?;
        tracing::info!("ssh-service: made the SYSTEM service's SSH host key");
        Ok(pem)
    }

    /// Write the anchor into the sealed key (taken over and emptied when it
    /// fails the check), then read it back through the service's own check:
    /// an anchor the service would not use is an error here, not a device
    /// that quietly stays unanchored.
    pub fn store_anchor(anchor: &Anchor) -> Result<(), String> {
        anchor.validate()?;
        let text = serde_json::to_string_pretty(anchor).map_err(|e| e.to_string())?;
        let path = key();
        machine_key::create_sealed(&path, true)?.write(ANCHOR, &text)?;
        match load_anchor()? {
            Some(back) if &back == anchor => Ok(()),
            _ => Err(format!(
                "{} does not read back the anchor just written",
                path.shown()
            )),
        }
    }

    /// `anchor create`'s half on this machine, once the server has anchored
    /// it: the copy's marker first when there is none, so a later first start
    /// never copies an older file over this anchor; then the anchor; then the
    /// files an earlier build kept are deleted (best effort, logged), so a
    /// rollback finds no anchor the server no longer accepts.
    pub fn anchor_created(anchor: &Anchor) -> Result<(), String> {
        {
            let key = machine_key::create_sealed(&key(), true)?;
            if !holds(&key, LEGACY_COPY)? {
                key.write(
                    LEGACY_COPY,
                    &LegacyCopy::now(CopyOutcome::Nothing, Some("anchored with this build")).json(),
                )?;
            }
        }
        store_anchor(anchor)?;
        legacy::remove_files();
        Ok(())
    }

    /// `anchor remove`'s half on this machine: the anchor and the host key
    /// out of the key, which keeps `LegacyCopy` so nothing is copied again,
    /// and the files an earlier build kept deleted. Whether there was an
    /// anchor in either place.
    pub fn remove() -> Result<bool, String> {
        let mut anchored = false;
        // Judged by nothing: taking a value away never makes anything trusted.
        if let Some(key) = machine_key::open_for_change(&key())? {
            anchored = key.delete(ANCHOR)?;
            key.delete(HOST_KEY)?;
        }
        let had_file = legacy::remove_files();
        Ok(anchored || had_file)
    }

    /// What the one-time copy did, kept in `LegacyCopy`. The census reads it.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LegacyCopy {
        /// RFC 3339.
        pub at: String,
        pub outcome: CopyOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub why: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub anchored_at: Option<String>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CopyOutcome {
        /// Written before anything is read; still there only when the start
        /// that wrote it ended before it could say more.
        Pending,
        Copied,
        /// There was nothing to copy.
        #[serde(rename = "none")]
        Nothing,
        Refused,
        /// A word a newer build wrote.
        #[serde(other)]
        Unknown,
    }

    impl std::fmt::Display for CopyOutcome {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(match self {
                CopyOutcome::Pending => "pending",
                CopyOutcome::Copied => "copied",
                CopyOutcome::Nothing => "none",
                CopyOutcome::Refused => "refused",
                CopyOutcome::Unknown => "unknown",
            })
        }
    }

    impl LegacyCopy {
        fn now(outcome: CopyOutcome, why: Option<&str>) -> Self {
            Self {
                at: chrono::Utc::now().to_rfc3339(),
                outcome,
                why: why.map(str::to_string),
                agent_id: None,
                anchored_at: None,
            }
        }

        fn json(&self) -> String {
            serde_json::to_string(self).unwrap_or_default()
        }
    }

    /// Why the one-time copy refused the anchor this machine had, while the
    /// key holds no anchor since: for the service's health and `anchor
    /// status`, with the way back.
    pub fn copy_refused() -> Option<String> {
        let key = machine_key::open_checked(&key()).ok()??;
        if holds(&key, ANCHOR).ok()? {
            return None;
        }
        let text = key.read(LEGACY_COPY, MAX_LEGACY_COPY).ok()??;
        let copy: LegacyCopy = serde_json::from_str(&text).ok()?;
        (copy.outcome == CopyOutcome::Refused).then(|| {
            copy.why
                .unwrap_or_else(|| "no reason was recorded".to_string())
        })
    }

    /// What the service's health and `anchor status` say about the one-time
    /// copy, with the way back. `None`: nothing to say.
    pub fn copy_problem() -> Option<String> {
        copy_refused().map(|why| super::copy_refused_problem(&why))
    }

    /// The SYSTEM service's first start on this build: copy the anchor
    /// builds before it kept in the machine folder into the key, once, and
    /// only one exactly as `roomlerd anchor create` wrote it. Only the SCM
    /// host calls it, before the doorbell, the renewal and the SSH publisher
    /// start; the command line and the worker never do. It logs what it did,
    /// once.
    pub fn copy_legacy_once() {
        match copy_once(&key()) {
            Ok(Some(done)) => tracing::info!(
                outcome = %done.outcome,
                why = done.why.as_deref().unwrap_or(""),
                agent_id = done.agent_id.as_deref().unwrap_or(""),
                "exec-service: one-time anchor copy"
            ),
            Ok(None) => tracing::debug!("exec-service: the one-time anchor copy ran before"),
            Err(e) => tracing::error!(
                %e,
                "exec-service: the one-time anchor copy did not run; nothing was copied"
            ),
        }
    }

    /// `Ok(None)`: the marker was there, so nothing ran. `Err`: the marker
    /// could not be written or read, so nothing was copied.
    fn copy_once(path: &KeyPath) -> Result<Option<LegacyCopy>, String> {
        // The writer without a takeover: a key that fails the check is not
        // this start's to empty, and an anchor copied into it would not be
        // read anyway.
        let key = machine_key::create_sealed(path, false)?;
        if holds(&key, LEGACY_COPY)? {
            return Ok(None);
        }
        // The marker first: from here on no start copies anything again,
        // whatever happens below.
        key.write(
            LEGACY_COPY,
            &LegacyCopy::now(CopyOutcome::Pending, None).json(),
        )?;
        let done = if super::disabled_by(std::env::var(super::COPY_SWITCH_ENV).ok().as_deref()) {
            LegacyCopy::now(
                CopyOutcome::Refused,
                Some(&format!("switched off ({}=0)", super::COPY_SWITCH_ENV)),
            )
        } else {
            copy_from_file(&key)
                .unwrap_or_else(|why| LegacyCopy::now(CopyOutcome::Refused, Some(&why)))
        };
        key.write(LEGACY_COPY, &done.json())?;
        Ok(Some(done))
    }

    /// The copy itself, after the marker: `Err` is a refusal, and says why.
    fn copy_from_file(key: &Key) -> Result<LegacyCopy, String> {
        let nothing = |why: &str| LegacyCopy::now(CopyOutcome::Nothing, Some(why));
        if holds(key, ANCHOR)? {
            return Ok(nothing("the key held an anchor already"));
        }
        // FR-94's reader: no link on the way, the folder and the file owned by
        // SYSTEM or Administrators with no write for anyone else (V4).
        let Some(read) = legacy::read(&legacy::anchor_path(), "the anchor file", 64 * 1024)? else {
            return Ok(nothing("no anchor file"));
        };
        let anchor: Anchor =
            serde_json::from_str(&read.text).map_err(|e| format!("the anchor file: {e}"))?;
        // V2: exactly what `anchor create` left.
        if !super::legacy_shape_ok(&read.folder, &read.file) {
            return Err(
                "the anchor file's folder or file does not have the exact owner and \
                        DACL `roomlerd anchor create` gives them"
                    .to_string(),
            );
        }
        // V1, V3: an https origin, and a valid anchor.
        super::copyable(&anchor)?;
        let host_key = legacy_host_key();
        if holds(key, ANCHOR)? {
            return Ok(nothing("the key held an anchor already"));
        }
        // The host key before the anchor, as every writer orders them: a
        // service that reads an SSH ceiling must find a key to publish.
        if let Some(pem) = &host_key {
            key.write(HOST_KEY, pem)?;
        }
        let text = serde_json::to_string_pretty(&anchor).map_err(|e| e.to_string())?;
        key.write(ANCHOR, &text)?;
        Ok(LegacyCopy {
            agent_id: Some(anchor.agent_id),
            anchored_at: Some(anchor.anchored_at),
            ..LegacyCopy::now(CopyOutcome::Copied, None)
        })
    }

    /// The host key beside the copied anchor, under the same rules (V2 and
    /// V4). One the copy does not take is left, and a new one is made when
    /// SSH through the service is used.
    fn legacy_host_key() -> Option<String> {
        match legacy::read(&legacy::host_key_path(), "the SSH host key", 16 * 1024) {
            Ok(Some(read)) if super::legacy_shape_ok(&read.folder, &read.file) => Some(read.text),
            Ok(Some(_)) => {
                tracing::warn!(
                    "exec-service: the one-time copy left the SSH host key file: its folder or \
                     file does not have the exact owner and DACL `roomlerd anchor create` gives \
                     them"
                );
                None
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(%e, "exec-service: the one-time copy left the SSH host key file");
                None
            }
        }
    }
}

/// #3737 — the anchor's first home, `%ProgramData%\roomler\roomler\exec-service`,
/// where builds before the registry key kept the anchor (`anchor.json`) and
/// FR-105's host key beside it. Only the SYSTEM service's one-time copy reads
/// it ([`store::copy_legacy_once`]), and `anchor create` and `anchor remove`
/// delete its files. Nothing else names it.
#[cfg(windows)]
pub(crate) mod legacy {
    use std::path::{Path, PathBuf};

    use roomler_node_core::machine_tree::PrivateRead;

    /// The host key's file name beside the anchor.
    pub(crate) const HOST_KEY_FILE: &str = "ssh_host_ed25519_key";

    /// `<ProgramData>\roomler\roomler\exec-service`: the current segment only
    /// (FR-94 came after the rename, so no build kept an anchor below the
    /// pre-rename one), rooted at the machine's `ProgramData` as the system
    /// names it, never at a variable (#3702).
    pub fn dir() -> PathBuf {
        let dir = roomler_node_core::appdirs::machine_program_data()
            .join("roomler")
            .join("roomler")
            .join("exec-service");
        #[cfg(test)]
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "a test named this computer's own machine folder: {}",
            dir.display()
        );
        dir
    }

    /// `…\exec-service\anchor.json`.
    pub fn anchor_path() -> PathBuf {
        dir().join("anchor.json")
    }

    /// `…\exec-service\ssh_host_ed25519_key`.
    pub fn host_key_path() -> PathBuf {
        dir().join(HOST_KEY_FILE)
    }

    /// `path`, a file in this folder, through FR-94's reader: it refuses a
    /// link on the way, and takes the folder and the file only when SYSTEM or
    /// Administrators own them and no other account may write them. With the
    /// owner and DACL it judged, for the copy's exact-shape check.
    pub fn read(path: &Path, what: &str, max_len: u64) -> Result<Option<PrivateRead>, String> {
        roomler_node_core::machine_tree::read_private_judged(&dir(), path, what, max_len)
    }

    /// Delete the anchor's file and the host key's, best effort and each
    /// logged. Never through a link: a file that is one, or sits below one,
    /// is left and logged (#3702's rule for what an administrator deletes).
    /// Whether the anchor's file was there.
    pub fn remove_files() -> bool {
        let mut had_anchor = false;
        for (path, is_anchor) in [(anchor_path(), true), (host_key_path(), false)] {
            match remove_plain(&path) {
                Ok(true) => {
                    tracing::info!(
                        path = %path.display(),
                        "exec-service: deleted a file of the anchor's first home"
                    );
                    had_anchor |= is_anchor;
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    %e,
                    path = %path.display(),
                    "exec-service: left a file of the anchor's first home"
                ),
            }
        }
        had_anchor
    }

    /// Delete `path` when it is a plain file with no link on the way.
    /// `Ok(false)`: it is not there.
    fn remove_plain(path: &Path) -> Result<bool, String> {
        if let Some(link) = roomler_node_core::recording_dir::link_component(path) {
            return Err(format!("{} is a link", link.display()));
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) if !meta.is_file() => {
                return Err(format!("{} is not a plain file", path.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
        #[cfg(test)]
        roomler_node_core::machine_tree::test_support::run_between();
        std::fs::remove_file(path)
            .map(|()| true)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Tests only: the anchor file `path` in the folder `dir`, through FR-94's
    /// reader, parsed and validated as builds before the registry key loaded
    /// it.
    #[cfg(test)]
    pub fn read_at(dir: &Path, path: &Path) -> Result<Option<super::Anchor>, String> {
        let Some(text) =
            roomler_node_core::machine_tree::read_private(dir, path, "the anchor file", 64 * 1024)?
        else {
            return Ok(None);
        };
        let anchor: super::Anchor =
            serde_json::from_str(&text).map_err(|e| format!("the anchor file: {e}"))?;
        anchor.validate()?;
        Ok(Some(anchor))
    }

    /// Tests only: write `bytes` as `name` in this folder exactly as every
    /// build before the registry key did, which is what an upgrade finds.
    #[cfg(test)]
    pub fn write(name: &str, bytes: &[u8]) -> Result<(), String> {
        writer::write(&dir(), name, bytes)
    }

    /// Tests only: [`write`], into the folder `dir` instead.
    #[cfg(test)]
    pub fn write_in(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
        writer::write(dir, name, bytes)
    }

    /// FR-94's writer, as it shipped until #3737 (`dda326009` and after).
    #[cfg(test)]
    mod writer {
        use std::os::windows::fs::MetadataExt as _;
        use std::path::Path;

        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT,
        };

        use super::super::win::{Sddl, wide};

        /// The anchor's directory: Administrators own it, SYSTEM and
        /// Administrators alone may touch it, inheritance off.
        const DIR_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
        /// The anchor file itself: the same, explicitly.
        const FILE_SDDL: &str = "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)";

        /// Make the anchor's directory private, or take over the plain
        /// directory already there.
        fn private_dir(dir: &Path) -> Result<(), String> {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            if let Some(link) = roomler_node_core::recording_dir::link_component(dir) {
                return Err(format!("{} is a link", link.display()));
            }
            let mut sd = Sddl::new(DIR_SDDL).map_err(|e| e.to_string())?;
            let name = wide(dir);
            match std::fs::symlink_metadata(dir) {
                Ok(m) => {
                    if !m.is_dir() || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        return Err(format!("{} is not a plain directory", dir.display()));
                    }
                    // SAFETY: a NUL-terminated path; the descriptor lives in `sd`.
                    let ok = unsafe {
                        SetFileSecurityW(
                            name.as_ptr(),
                            OWNER_SECURITY_INFORMATION
                                | DACL_SECURITY_INFORMATION
                                | PROTECTED_DACL_SECURITY_INFORMATION,
                            sd.psd,
                        )
                    };
                    if ok == 0 {
                        return Err(format!(
                            "{}: {}",
                            dir.display(),
                            std::io::Error::last_os_error()
                        ));
                    }
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // SAFETY: a NUL-terminated path; the attributes live in `sd`.
                    if unsafe { CreateDirectoryW(name.as_ptr(), sd.attributes()) } == 0 {
                        return Err(format!(
                            "{}: {}",
                            dir.display(),
                            std::io::Error::last_os_error()
                        ));
                    }
                    Ok(())
                }
                Err(e) => Err(format!("{}: {e}", dir.display())),
            }
        }

        /// Write `bytes` as `name` in `dir`, made private first, through a
        /// temporary file given the file's own DACL before it takes the name.
        pub(super) fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
            private_dir(dir)?;
            let path = dir.join(name);
            let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
            std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
            let sd = Sddl::new(FILE_SDDL).map_err(|e| e.to_string())?;
            let wide_tmp = wide(&tmp);
            // SAFETY: a NUL-terminated path; the descriptor lives in `sd`.
            let ok = unsafe {
                SetFileSecurityW(
                    wide_tmp.as_ptr(),
                    OWNER_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION
                        | PROTECTED_DACL_SECURITY_INFORMATION,
                    sd.psd,
                )
            };
            if ok == 0 {
                let e = std::io::Error::last_os_error();
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("{}: {e}", tmp.display()));
            }
            std::fs::rename(&tmp, &path).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("{}: {e}", path.display())
            })
        }
    }
}

/// Tests only: the anchor key under a stand-in the test names itself, below
/// the person's own hive. A release build has no such switch.
#[cfg(all(windows, test))]
pub(crate) mod test_fixture {
    use std::sync::OnceLock;

    use roomler_node_core::machine_key::{Hive, KeyPath};

    static KEY: OnceLock<KeyPath> = OnceLock::new();

    /// The anchor key is `<parent>\ExecService` below `hive` for the rest of
    /// this test process. One stand-in per process.
    pub fn anchor_key_is(hive: Hive, parent: &str) {
        let key = KeyPath {
            hive,
            parent: parent.to_string(),
            name: "ExecService".to_string(),
        };
        if let Err(key) = KEY.set(key) {
            assert_eq!(KEY.get(), Some(&key), "one stand-in per process");
        }
    }

    /// The stand-in. A test that names the anchor key without one stops here,
    /// so no test run ever touches this computer's own key.
    pub fn key() -> KeyPath {
        KEY.get()
            .cloned()
            .expect("a test named the machine's anchor key without a stand-in")
    }
}

// ────────────────────────────────────────────────────────────────────────────
// The service's side
// ────────────────────────────────────────────────────────────────────────────

/// What a claim hands the service: the command, from the server. The
/// server's `ClaimedExec`.
#[derive(Debug, Clone, Deserialize)]
pub struct Claimed {
    pub request_id: String,
    pub shell: String,
    pub command: String,
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
    #[serde(default)]
    pub cwd: Option<String>,
    pub caller: String,
    /// `system` or `console_user`.
    pub run_as: String,
}

/// What the service posts: the server's `ExecOutcome`.
#[derive(Debug, Serialize)]
pub struct Posted<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub truncated: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
    /// The identity it ran as; absent when it never ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran_as: Option<&'a str>,
}

/// The identity a claim's `run_as` names, and since FR-97 P5a an
/// `rc:rpc.exec` frame's on Linux and macOS. An unknown word is a refusal,
/// never the daemon: a newer server naming a mode this build does not know
/// must not have its intent upgraded (`exec::RunAs::from_wire`'s rule).
pub fn run_as_of(word: &str) -> Result<(crate::exec::RunAs, &'static str), String> {
    match word {
        "system" => Ok((crate::exec::RunAs::Daemon, "system")),
        "console_user" => Ok((crate::exec::RunAs::ConsoleUser, "console_user")),
        other => Err(format!(
            "the server asked to run as {other:?}, which this build does not know; refusing \
             rather than run it as SYSTEM or root"
        )),
    }
}

/// Start the doorbell and the credential renewal in the SYSTEM service, on
/// a thread and a runtime of their own: `service_main` is synchronous and
/// must never wait on either. A no-op off Windows.
pub fn spawn_service_runner() {
    #[cfg(windows)]
    {
        let spawned = std::thread::Builder::new()
            .name("roomler-svc-exec".into())
            .spawn(|| {
                match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("roomler-svc-exec")
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(svc::run()),
                    Err(e) => tracing::error!(%e, "exec-service: no runtime; the doorbell is down"),
                }
            });
        if let Err(e) = spawned {
            tracing::error!(%e, "exec-service: could not start the doorbell thread");
        }
    }
}

#[cfg(windows)]
mod svc {
    //! The doorbell server and the runner, in the SYSTEM service.

    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::{Mutex, Semaphore};
    use tracing::{error, info, warn};

    use super::{
        Anchor, Claimed, DoorbellReply, DoorbellRequest, Posted, RingLimiter, load_anchor,
        renewal_due, run_as_of, token_expiry, valid_object_id,
    };

    /// Connections handled at once; the accept loop waits beyond it.
    const MAX_CONNECTIONS: usize = 16;
    /// How often the service checks whether its credential needs renewing.
    const RENEW_CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);

    pub async fn run() {
        // #3737 — before the doorbell, the renewal and the SSH publisher.
        super::store::copy_legacy_once();
        tokio::spawn(renew_loop());
        // FR-105 — the SSH pipe and the endpoint publisher, beside the
        // doorbell. They serve nothing until an administrator turns the
        // anchor's `ssh` on.
        #[cfg(feature = "ssh-server")]
        tokio::spawn(crate::ssh_service::svc::run());
        let limiter = Arc::new(Mutex::new(RingLimiter::default()));
        loop {
            if let Err(e) = super::win::serve(Arc::clone(&limiter)).await {
                // A pipe someone else created first (`first_pipe_instance`),
                // or no pipe at all: say so, and try again, because a
                // squatter may exit.
                error!(%e, "exec-service: the doorbell is down; retrying in 30 s");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    pub fn connection_slots() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(MAX_CONNECTIONS))
    }

    /// One request, answered.
    pub async fn handle(req: DoorbellRequest, limiter: Arc<Mutex<RingLimiter>>) -> DoorbellReply {
        match req {
            DoorbellRequest::Health => health(),
            DoorbellRequest::Ring { request_id } => ring(request_id, limiter).await,
            DoorbellRequest::Cancel { request_id } => {
                let found =
                    valid_object_id(&request_id) && crate::exec::shared().cancel(&request_id).await;
                info!(%request_id, found, "exec-service: cancel");
                DoorbellReply::Cancelled { found }
            }
            DoorbellRequest::SshGrant { grant_id } => ssh_grant(grant_id, limiter).await,
        }
    }

    fn health() -> DoorbellReply {
        let version = env!("CARGO_PKG_VERSION").to_string();
        match load_anchor() {
            Ok(Some(a)) => DoorbellReply::Health {
                anchored: true,
                agent_id: Some(a.agent_id.clone()),
                exec_enabled: a.exec_enabled,
                problem: None,
                version,
                ssh: crate::ssh_service::served_ceiling(&a),
            },
            // #3737 — no anchor in the key: when the one-time copy refused
            // the anchor this device had, the worker logs why and how back.
            Ok(None) => DoorbellReply::Health {
                anchored: false,
                agent_id: None,
                exec_enabled: false,
                problem: super::store::copy_refused().map(|why| super::copy_refused_problem(&why)),
                version,
                ssh: None,
            },
            Err(e) => DoorbellReply::Health {
                anchored: false,
                agent_id: None,
                exec_enabled: false,
                problem: Some(e),
                version,
                ssh: None,
            },
        }
    }

    /// FR-105 — claim an SSH grant for the service's own endpoint. Rate
    /// limited with the rings: a claim costs the server a call and, refused,
    /// an audit row.
    async fn ssh_grant(grant_id: String, limiter: Arc<Mutex<RingLimiter>>) -> DoorbellReply {
        if !valid_object_id(&grant_id) {
            return DoorbellReply::Refused {
                error: "a malformed grant id".into(),
            };
        }
        if !limiter.lock().await.allow(std::time::Instant::now()) {
            warn!(%grant_id, "ssh-service: grant refused, too many rings");
            return refused("the SYSTEM service is refusing rings for a moment: too many");
        }
        let anchor = match load_anchor() {
            Ok(Some(a)) => a,
            Ok(None) => {
                return refused("this device is not anchored (`roomlerd anchor create`, elevated)");
            }
            Err(e) => {
                error!(%e, "ssh-service: the anchor cannot be trusted");
                return refused(format!("this device's anchor cannot be trusted: {e}"));
            }
        };
        #[cfg(feature = "ssh-server")]
        {
            match crate::ssh_service::claim_grant(&anchor, &grant_id).await {
                Ok(()) => DoorbellReply::Rang {
                    claimed: true,
                    error: None,
                },
                Err(e) => {
                    warn!(%grant_id, %e, "ssh-service: grant not claimed");
                    refused(e)
                }
            }
        }
        #[cfg(not(feature = "ssh-server"))]
        {
            let _ = anchor;
            refused("this build of roomlerd serves no SSH")
        }
    }

    fn refused(error: impl Into<String>) -> DoorbellReply {
        DoorbellReply::Rang {
            claimed: false,
            error: Some(error.into()),
        }
    }

    async fn ring(request_id: String, limiter: Arc<Mutex<RingLimiter>>) -> DoorbellReply {
        if !valid_object_id(&request_id) {
            return DoorbellReply::Refused {
                error: "a malformed request id".into(),
            };
        }
        if !limiter.lock().await.allow(std::time::Instant::now()) {
            warn!(%request_id, "exec-service: ring refused, too many rings");
            return refused("the SYSTEM service is refusing rings for a moment: too many");
        }
        let anchor = match load_anchor() {
            Ok(Some(a)) => a,
            Ok(None) => {
                return refused(
                    "this device is not anchored for SYSTEM exec (`roomlerd anchor create`, \
                     elevated)",
                );
            }
            Err(e) => {
                error!(%e, "exec-service: the anchor cannot be trusted");
                return refused(format!("this device's anchor cannot be trusted: {e}"));
            }
        };
        if !anchor.exec_enabled {
            return refused(
                "remote execution through the SYSTEM service is off on this device \
                 (`roomlerd anchor exec on`, elevated)",
            );
        }
        let claimed = match claim(&anchor, &request_id).await {
            Ok(c) => c,
            Err(e) => {
                warn!(%request_id, %e, "exec-service: claim refused");
                return refused(e);
            }
        };
        let (run_as, ran_as) = match run_as_of(&claimed.run_as) {
            Ok(v) => v,
            Err(e) => {
                // Claimed, so the server waits for THIS service's answer.
                post_result(&anchor, &request_id, &failed(&e), None).await;
                return DoorbellReply::Rang {
                    claimed: true,
                    error: None,
                };
            }
        };
        info!(
            %request_id, caller = %claimed.caller, run_as = ran_as,
            "exec-service: running a command for the server"
        );
        tokio::spawn(run_and_post(anchor, claimed, run_as, ran_as));
        DoorbellReply::Rang {
            claimed: true,
            error: None,
        }
    }

    fn failed(error: &str) -> crate::exec::ExecOutcome {
        crate::exec::ExecOutcome {
            error: Some(error.to_string()),
            ..Default::default()
        }
    }

    /// `total` bounds each request. A claim's is shorter than the worker's
    /// wait for the doorbell's answer (30 s), so a slow server is reported to
    /// the worker rather than abandoned by it.
    fn client(total: Duration) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .user_agent(concat!("roomlerd-exec-service/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(total)
            .build()
            .map_err(|e| format!("building the HTTP client: {e}"))
    }

    const CLAIM_TIMEOUT: Duration = Duration::from_secs(20);
    const POST_TIMEOUT: Duration = Duration::from_secs(30);

    fn url(anchor: &Anchor, path: &str) -> String {
        // `tid` lets the front proxy send the call to the org's pod, where the
        // caller usually waits; any other pod works too, a little later.
        format!("{}{path}?tid={}", anchor.server, anchor.tenant_id)
    }

    async fn claim(anchor: &Anchor, request_id: &str) -> Result<Claimed, String> {
        let resp = client(CLAIM_TIMEOUT)?
            .post(url(
                anchor,
                &format!("/api/agent/service/rpc/{request_id}/claim"),
            ))
            .bearer_auth(&anchor.service_token)
            .send()
            .await
            .map_err(|e| format!("the SYSTEM service could not reach the server: {e}"))?;
        match resp.status().as_u16() {
            200 => {
                let c: Claimed = resp
                    .json()
                    .await
                    .map_err(|e| format!("the server's claim answer: {e}"))?;
                if c.request_id != request_id {
                    return Err("the server answered a claim for another command".into());
                }
                Ok(c)
            }
            404 => Err("the server has no such command pending for this device".into()),
            401 => Err(
                "this device's SYSTEM service credential is no longer accepted (re-anchored or \
                 un-anchored); anchor it again"
                    .into(),
            ),
            s => Err(format!("the server refused the claim: HTTP {s}")),
        }
    }

    async fn run_and_post(
        anchor: Anchor,
        claimed: Claimed,
        run_as: crate::exec::RunAs,
        ran_as: &'static str,
    ) {
        // Masked out of whatever the command prints, as the agent token is.
        crate::exec::register_secret(&anchor.service_token);
        let request_id = claimed.request_id.clone();
        let req = crate::exec::ExecRequest {
            request_id: claimed.request_id,
            shell: claimed.shell,
            command: claimed.command,
            timeout_ms: claimed.timeout_ms,
            max_output_bytes: claimed.max_output_bytes,
            cwd: claimed.cwd,
            caller: claimed.caller,
            run_as,
            // Fleet RPC: where it always started (FR-97 P5b is SSH's).
            start_in_home: false,
            session_owned: false,
        };
        let outcome = crate::exec::shared()
            .run(req, &crate::exec::redactor())
            .await;
        post_result(&anchor, &request_id, &outcome, Some(ran_as)).await;
    }

    /// Post the outcome, retrying a network failure: the caller waits on it.
    /// A 4xx is final (the row is gone, or the credential is).
    async fn post_result(
        anchor: &Anchor,
        request_id: &str,
        outcome: &crate::exec::ExecOutcome,
        ran_as: Option<&str>,
    ) {
        let body = Posted {
            exit_code: outcome.exit_code,
            stdout: &outcome.stdout,
            stderr: &outcome.stderr,
            truncated: outcome.truncated,
            duration_ms: outcome.duration_ms,
            error: outcome.error.as_deref(),
            ran_as,
        };
        let client = match client(POST_TIMEOUT) {
            Ok(c) => c,
            Err(e) => {
                error!(%request_id, %e, "exec-service: cannot post the result");
                return;
            }
        };
        for (attempt, backoff) in [(1, 2u64), (2, 5), (3, 0)] {
            let sent = client
                .post(url(
                    anchor,
                    &format!("/api/agent/service/rpc/{request_id}/result"),
                ))
                .bearer_auth(&anchor.service_token)
                .json(&body)
                .send()
                .await;
            match sent {
                Ok(r) if r.status().is_success() => {
                    info!(%request_id, exit_code = ?outcome.exit_code, "exec-service: result posted");
                    return;
                }
                Ok(r) if r.status().is_client_error() => {
                    error!(%request_id, status = %r.status(), "exec-service: the server refused the result");
                    return;
                }
                Ok(r) => {
                    warn!(%request_id, attempt, status = %r.status(), "exec-service: result post failed")
                }
                Err(e) => warn!(%request_id, attempt, %e, "exec-service: result post failed"),
            }
            if backoff > 0 {
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            }
        }
        error!(%request_id, "exec-service: the result never reached the server");
    }

    /// Renew the credential well before it expires, under the same anchor.
    async fn renew_loop() {
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        loop {
            if let Err(e) = renew_if_due().await {
                warn!(%e, "exec-service: credential renewal failed; will retry");
            }
            tokio::time::sleep(RENEW_CHECK_EVERY).await;
        }
    }

    async fn renew_if_due() -> Result<(), String> {
        let Some(anchor) = load_anchor()? else {
            return Ok(());
        };
        let now = chrono::Utc::now().timestamp();
        if !renewal_due(token_expiry(&anchor.service_token), now) {
            return Ok(());
        }
        let resp = client(POST_TIMEOUT)?
            .post(url(&anchor, "/api/agent/service/renew"))
            .bearer_auth(&anchor.service_token)
            .send()
            .await
            .map_err(|e| format!("could not reach the server: {e}"))?;
        if resp.status().as_u16() == 401 {
            return Err(
                "the server no longer accepts this anchor's credential; anchor the device again"
                    .into(),
            );
        }
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        #[derive(serde::Deserialize)]
        struct Renewed {
            service_token: String,
        }
        let renewed: Renewed = resp.json().await.map_err(|e| e.to_string())?;
        // Swap the credential into the anchor as it stands NOW: an
        // administrator may have changed it during the call (`exec off`), and
        // writing back the copy loaded before it would undo that. Anchored
        // again meanwhile: the new anchor's credential stands.
        let Some(mut current) = load_anchor()? else {
            return Ok(());
        };
        if current.agent_id != anchor.agent_id || current.service_token != anchor.service_token {
            return Ok(());
        }
        current.service_token = renewed.service_token;
        super::store::store_anchor(&current)?;
        info!("exec-service: credential renewed");
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// `roomlerd anchor`
// ────────────────────────────────────────────────────────────────────────────

/// What `roomlerd anchor` was asked to do.
#[derive(Debug, Clone)]
pub enum AnchorAction {
    /// Anchor this machine's device for SYSTEM exec.
    Create {
        server: String,
        token: String,
        device: String,
        force: bool,
        exec_enabled: bool,
        /// FR-105 — the SSH ceiling to anchor with. `None` keeps the one
        /// the replaced anchor of the SAME device had, and is off otherwise.
        ssh: Option<roomler_ai_remote_control::models::SshServiceCeiling>,
    },
    /// Show the anchor (never its credential).
    Status,
    /// Turn the anchor's gate 4 on or off.
    Exec { on: bool },
    /// FR-105 — how far roomler SSH through the SYSTEM service may go.
    Ssh {
        ceiling: roomler_ai_remote_control::models::SshServiceCeiling,
    },
    /// Delete the anchor from this machine.
    Remove,
}

/// FR-105 — the CLI's spelling of a ceiling (`off`, `console_user`,
/// `system`), the wire's.
pub fn parse_ssh_ceiling(
    word: &str,
) -> Result<roomler_ai_remote_control::models::SshServiceCeiling, String> {
    use roomler_ai_remote_control::models::SshServiceCeiling;
    match word.trim() {
        "off" => Ok(SshServiceCeiling::Off),
        "console_user" => Ok(SshServiceCeiling::ConsoleUser),
        "system" => Ok(SshServiceCeiling::System),
        other => Err(format!(
            "{other:?} is not one of off | console_user | system"
        )),
    }
}

/// #3737 — what the service's health and `anchor status` say when the
/// one-time copy refused the anchor this device had (`why`), with the way
/// back.
#[cfg_attr(not(windows), allow(dead_code))]
fn copy_refused_problem(why: &str) -> String {
    format!(
        "the anchor this device had before its upgrade was not copied into the registry key \
         ({why}); anchor it again from an elevated prompt: roomlerd anchor create --server \
         <origin> --token <token> --device <device id> --force"
    )
}

/// `roomlerd anchor …`. Every action but `status` needs an elevated prompt;
/// `status` does too, in practice, because the key is SYSTEM and
/// Administrators only.
pub async fn anchor_cli(action: AnchorAction) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        win::anchor_cli(action).await
    }
    #[cfg(not(windows))]
    {
        let _ = action;
        anyhow::bail!(
            "only a Windows device is anchored: a Linux or macOS daemon already runs commands as \
             root"
        )
    }
}

#[cfg(windows)]
mod win {
    //! Windows: the doorbell pipe, elevation, and the `roomlerd anchor` verbs.

    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;
    use std::sync::Arc;

    use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
    use tokio::sync::Mutex;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_BUSY, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
        SE_KERNEL_OBJECT, SE_OBJECT_TYPE,
    };
    use windows_sys::Win32::Security::{
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_WRITE_DATA, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };

    use super::{Anchor, AnchorAction, DoorbellReply, DoorbellRequest, RingLimiter};

    /// The doorbell. SYSTEM owns it (the worker checks), and makes every
    /// instance. Administrators and interactive users may read it and write
    /// data to it (`0x12008b`), and nothing more: not
    /// `FILE_CREATE_PIPE_INSTANCE`, with which a user could add an instance
    /// of their own and take the next worker's connection. Below Medium
    /// integrity nothing may write to it.
    const PIPE_SDDL: &str =
        "O:SYD:P(A;;GA;;;SY)(A;;0x12008b;;;BA)(A;;0x12008b;;;IU)S:(ML;;NW;;;ME)";

    /// A security descriptor from SDDL, and the `SECURITY_ATTRIBUTES` that
    /// hand it to a directory or a pipe instance. Freed on drop.
    pub(super) struct Sddl {
        sa: SECURITY_ATTRIBUTES,
        pub(super) psd: PSECURITY_DESCRIPTOR,
    }

    // SAFETY: a LocalAlloc'd buffer with no thread affinity; the accept loop
    // holds one across `.await`.
    unsafe impl Send for Sddl {}

    impl Sddl {
        pub(super) fn new(sddl: &str) -> std::io::Result<Self> {
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: a NUL-terminated string; `psd` receives a LocalAlloc'd
            // descriptor, freed in Drop; the size out-param is optional.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    1,
                    &mut psd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                sa: SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: psd,
                    bInheritHandle: 0,
                },
                psd,
            })
        }

        pub(super) fn attributes(&mut self) -> *mut SECURITY_ATTRIBUTES {
            &raw mut self.sa
        }
    }

    impl Drop for Sddl {
        fn drop(&mut self) {
            // SAFETY: the one descriptor ConvertStringSecurityDescriptor… made.
            unsafe { LocalFree(self.psd as _) };
        }
    }

    pub(super) fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn is_well_known(sid: PSID, kind: i32) -> bool {
        // SAFETY: `sid` points into a descriptor that outlives the call.
        !sid.is_null() && unsafe { IsWellKnownSid(sid, kind) } != 0
    }

    /// Is this process elevated (or SYSTEM)? Only such a process may write
    /// the anchor, and saying so up front beats an access-denied halfway.
    fn process_is_elevated() -> bool {
        crate::win_identity::process_is_elevated()
    }

    /// Is the doorbell's server end owned by SYSTEM? A standard user cannot
    /// make an object SYSTEM owns, so a pipe someone squatted fails this.
    fn pipe_owned_by_system(handle: HANDLE) -> Result<bool, String> {
        let kinds: [SE_OBJECT_TYPE; 2] = [SE_FILE_OBJECT, SE_KERNEL_OBJECT];
        let mut last = 0u32;
        for kind in kinds {
            let mut owner: PSID = std::ptr::null_mut();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: a live pipe handle opened with READ_CONTROL (inside
            // GENERIC_READ); `owner` points into `psd`, freed below.
            let rc = unsafe {
                GetSecurityInfo(
                    handle,
                    kind,
                    OWNER_SECURITY_INFORMATION,
                    &mut owner,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut psd,
                )
            };
            if rc == 0 {
                let system = is_well_known(owner, WinLocalSystemSid);
                // SAFETY: the descriptor GetSecurityInfo allocated.
                unsafe { LocalFree(psd as _) };
                return Ok(system);
            }
            last = rc;
        }
        Err(format!(
            "reading the doorbell's owner: {}",
            std::io::Error::from_raw_os_error(last as i32)
        ))
    }

    /// Open the doorbell with exactly the access its DACL grants, at the
    /// identification level (the service never acts as the worker), and
    /// refuse it unless SYSTEM owns it. `Ok(None)` = every instance is busy.
    fn open_doorbell() -> Result<Option<NamedPipeClient>, String> {
        open_system_pipe(super::PIPE_NAME, "doorbell")
    }

    /// Open one of the SYSTEM service's pipes (`what` names it in errors) the
    /// way [`open_doorbell`] opens the doorbell: the access its DACL grants,
    /// identification level, and SYSTEM must own it, so a pipe a user squatted
    /// is refused. FR-105's SSH pipe is opened the same way.
    pub(crate) fn open_system_pipe(
        pipe: &str,
        what: &str,
    ) -> Result<Option<NamedPipeClient>, String> {
        let name = wide(Path::new(pipe));
        // SAFETY: a NUL-terminated name; the handle goes to tokio or is
        // closed.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | FILE_WRITE_DATA,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
                return Ok(None);
            }
            return Err(format!("no SYSTEM service {what}: {e}"));
        }
        match pipe_owned_by_system(handle) {
            Ok(true) => {}
            Ok(false) => {
                // SAFETY: ours, closed once.
                unsafe { CloseHandle(handle) };
                return Err(format!(
                    "the {what} pipe is not owned by SYSTEM: refusing it (another process \
                     created it first?)"
                ));
            }
            Err(e) => {
                // SAFETY: ours, closed once.
                unsafe { CloseHandle(handle) };
                return Err(e);
            }
        }
        // SAFETY: a pipe handle opened overlapped, owned by tokio from here.
        match unsafe { NamedPipeClient::from_raw_handle(handle as _) } {
            Ok(c) => Ok(Some(c)),
            Err(e) => {
                // SAFETY: tokio did not take it; closed once.
                unsafe { CloseHandle(handle) };
                Err(format!("the {what}: {e}"))
            }
        }
    }

    /// One request to the service, and its answer. Every instance busy is
    /// retried briefly: the service makes the next one as soon as it takes
    /// a connection.
    pub async fn ask(
        req: &DoorbellRequest,
        wait: std::time::Duration,
    ) -> Result<DoorbellReply, String> {
        for _ in 0..20 {
            if let Some(pipe) = open_doorbell()? {
                return super::exchange(pipe, req, wait).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Err("the SYSTEM service's doorbell stayed busy".into())
    }

    fn server_instance(first: bool) -> std::io::Result<NamedPipeServer> {
        pipe_instance(super::PIPE_NAME, first)
    }

    /// One server instance of a SYSTEM service pipe, with the doorbell's
    /// DACL ([`PIPE_SDDL`]): SYSTEM owns it, Administrators and interactive
    /// users may read and write data, and nobody else may add an instance.
    /// `first` refuses a name something else already holds. FR-105's SSH pipe
    /// is made the same way.
    pub(crate) fn pipe_instance(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut sd = Sddl::new(PIPE_SDDL)?;
        // SAFETY: the attributes live in `sd` until the call returns; the
        // system copies the descriptor into the instance.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(name, sd.attributes().cast())
        }
    }

    /// Serve the doorbell until an instance cannot be made.
    pub async fn serve(limiter: Arc<Mutex<RingLimiter>>) -> Result<(), String> {
        // `first_pipe_instance`: if anything else holds the name, fail rather
        // than join it.
        let mut server = server_instance(true).map_err(|e| format!("the doorbell pipe: {e}"))?;
        tracing::info!(pipe = super::PIPE_NAME, "exec-service: the doorbell is up");
        let slots = super::svc::connection_slots();
        loop {
            if let Err(e) = server.connect().await {
                // One peer's failed connection (it hung up first) must not
                // take the doorbell down for everyone: replace the instance.
                tracing::debug!(%e, "exec-service: a doorbell connection failed");
                server = server_instance(false).map_err(|e| format!("the doorbell pipe: {e}"))?;
                continue;
            }
            let connected = server;
            server = server_instance(false).map_err(|e| format!("the doorbell pipe: {e}"))?;
            let permit = Arc::clone(&slots)
                .acquire_owned()
                .await
                .map_err(|e| e.to_string())?;
            let limiter = Arc::clone(&limiter);
            tokio::spawn(async move {
                super::serve_connection(connected, |req| super::svc::handle(req, limiter)).await;
                drop(permit);
            });
        }
    }

    /// A ceiling as `anchor status` and `anchor ssh` say it.
    fn ssh_words(ceiling: roomler_ai_remote_control::models::SshServiceCeiling) -> &'static str {
        use roomler_ai_remote_control::models::SshServiceCeiling;
        match ceiling {
            SshServiceCeiling::Off => "OFF",
            SshServiceCeiling::ConsoleUser => "ON, as the signed-in user only",
            SshServiceCeiling::System => "ON, as SYSTEM or the signed-in user",
        }
    }

    fn require_elevated() -> anyhow::Result<()> {
        if !process_is_elevated() {
            anyhow::bail!(
                "run this from an elevated prompt (Run as administrator): the anchor is \
                 Administrators-only"
            );
        }
        Ok(())
    }

    pub async fn anchor_cli(action: AnchorAction) -> anyhow::Result<()> {
        match action {
            AnchorAction::Create {
                server,
                token,
                device,
                force,
                exec_enabled,
                ssh,
            } => create(&server, &token, &device, force, exec_enabled, ssh).await,
            AnchorAction::Status => status(),
            AnchorAction::Ssh { ceiling } => {
                require_elevated()?;
                let mut anchor = super::load_anchor()
                    .map_err(anyhow::Error::msg)?
                    .ok_or_else(|| anyhow::anyhow!("this device is not anchored"))?;
                anchor.ssh = ceiling;
                // The key before the ceiling: a service that reads the new
                // ceiling must find a key to publish with it.
                #[cfg(feature = "ssh-server")]
                if ceiling != roomler_ai_remote_control::models::SshServiceCeiling::Off {
                    super::ensure_host_key().map_err(anyhow::Error::msg)?;
                }
                super::store::store_anchor(&anchor).map_err(anyhow::Error::msg)?;
                println!(
                    "SSH through the SYSTEM service is now {} on this device.\n\
                     The service publishes it to the server within a minute; nothing needs \
                     a restart.",
                    ssh_words(ceiling)
                );
                #[cfg(not(feature = "ssh-server"))]
                println!("Note: this build of roomlerd serves no SSH, so nothing uses it yet.");
                Ok(())
            }
            AnchorAction::Exec { on } => {
                require_elevated()?;
                let mut anchor = super::load_anchor()
                    .map_err(anyhow::Error::msg)?
                    .ok_or_else(|| anyhow::anyhow!("this device is not anchored"))?;
                anchor.exec_enabled = on;
                super::store::store_anchor(&anchor).map_err(anyhow::Error::msg)?;
                println!(
                    "Remote execution through the SYSTEM service is now {} on this device.",
                    if on { "ON" } else { "OFF" }
                );
                Ok(())
            }
            AnchorAction::Remove => {
                require_elevated()?;
                if !super::store::remove().map_err(anyhow::Error::msg)? {
                    println!("This device was not anchored.");
                    return Ok(());
                }
                println!(
                    "The anchor is gone from this machine: commands go to the worker again \
                     (within a heartbeat), or are refused where the org requires SYSTEM.\n\
                     The server still records the anchor until an administrator removes it \
                     from the device's page."
                );
                Ok(())
            }
        }
    }

    async fn create(
        server: &str,
        token: &str,
        device: &str,
        force: bool,
        exec_enabled: bool,
        ssh: Option<roomler_ai_remote_control::models::SshServiceCeiling>,
    ) -> anyhow::Result<()> {
        require_elevated()?;
        let server = super::origin_ok(server).map_err(anyhow::Error::msg)?;
        let device = device.trim();
        if !super::valid_object_id(device) {
            anyhow::bail!(
                "--device takes the device's id from its page in the dashboard (24 hex digits)"
            );
        }
        let host = std::env::var("COMPUTERNAME").unwrap_or_default();
        #[derive(serde::Deserialize)]
        struct Anchored {
            agent_id: String,
            tenant_id: String,
            name: String,
            service_token: String,
        }
        let resp = reqwest::Client::builder()
            .user_agent(concat!("roomlerd-anchor/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()?
            .post(format!("{server}/api/agent/anchor"))
            .json(&serde_json::json!({
                "enrollment_token": token,
                "agent_id": device,
                "host_name": host,
                "force": force,
            }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("the server refused the anchor (HTTP {status}): {body}");
        }
        let anchored: Anchored = resp.json().await?;
        if anchored.agent_id != device {
            anyhow::bail!("the server anchored a different device than asked; nothing written");
        }
        let replaced = super::load_anchor().ok().flatten();
        // FR-105 — an administrator re-anchoring the SAME device (a fresh
        // credential, a re-run wizard) keeps the SSH ceiling an administrator
        // chose for it; any other device starts off.
        let ssh = ssh.unwrap_or_else(|| {
            replaced
                .as_ref()
                .filter(|old| old.agent_id == anchored.agent_id)
                .map(|old| old.ssh)
                .unwrap_or_default()
        });
        #[cfg(feature = "ssh-server")]
        if ssh != roomler_ai_remote_control::models::SshServiceCeiling::Off {
            super::ensure_host_key().map_err(anyhow::Error::msg)?;
        }
        let anchor = Anchor {
            version: super::ANCHOR_VERSION,
            server: server.clone(),
            tenant_id: anchored.tenant_id,
            agent_id: anchored.agent_id,
            device_name: anchored.name,
            service_token: anchored.service_token,
            exec_enabled,
            anchored_at: chrono::Utc::now().to_rfc3339(),
            ssh,
            extra: Default::default(),
        };
        super::store::anchor_created(&anchor).map_err(anyhow::Error::msg)?;
        if let Some(old) = replaced {
            println!(
                "Replaced the anchor for device {} ({}).",
                old.device_name, old.agent_id
            );
        }
        println!(
            "Anchored device {} ({}) at {server}.\n\
             Remote execution through the SYSTEM service: {}.\n\
             The worker offers it to the server within a minute; nothing needs a restart.",
            anchor.device_name,
            anchor.agent_id,
            if exec_enabled {
                "ON"
            } else {
                "OFF (`roomlerd anchor exec on`)"
            },
        );
        Ok(())
    }

    fn status() -> anyhow::Result<()> {
        match super::load_anchor() {
            Ok(Some(a)) => {
                let expires = super::token_expiry(&a.service_token)
                    .and_then(|exp| chrono::DateTime::from_timestamp(exp, 0))
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "unknown".into());
                println!(
                    "Anchored: device {} ({})\n  server:      {}\n  org:         {}\n  \
                     exec:        {}\n  ssh:         {}\n  anchored at: {}\n  credential:  \
                     renews itself; current one expires {expires}",
                    a.device_name,
                    a.agent_id,
                    a.server,
                    a.tenant_id,
                    if a.exec_enabled { "on" } else { "off" },
                    ssh_words(a.ssh),
                    a.anchored_at,
                );
                // FR-105 — the key a caller pins on the service's port.
                #[cfg(feature = "ssh-server")]
                if let Ok(Some(pem)) = super::load_host_key()
                    && let Some(fp) = crate::ssh_service::fingerprint_of(&pem)
                {
                    println!("  ssh host key: {fp}");
                }
                Ok(())
            }
            Ok(None) => {
                println!("This device is not anchored for SYSTEM exec.");
                // #3737 — and why, when the upgrade's copy refused one.
                if let Some(why) = super::store::copy_refused() {
                    println!("Note: {}", super::copy_refused_problem(&why));
                }
                Ok(())
            }
            Err(e) if !process_is_elevated() => anyhow::bail!(
                "could not read the anchor ({e}); it is SYSTEM and Administrators only, so run \
                 this elevated"
            ),
            Err(e) => anyhow::bail!("the anchor here cannot be trusted: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kill_switch_is_an_explicit_off() {
        for off in ["0", "false", "OFF", " no "] {
            assert!(disabled_by(Some(off)), "{off}");
        }
        for on in [None, Some(""), Some("1"), Some("true"), Some("maybe")] {
            assert!(!disabled_by(on), "{on:?}");
        }
    }

    #[test]
    fn only_an_object_id_reaches_a_url() {
        assert!(valid_object_id("6ace07367d06cf41e19e54b0"));
        for bad in [
            "",
            "6ace07367d06cf41e19e54b",
            "6ace07367d06cf41e19e54b00",
            "../../api/agent/anchor0000",
            "6ace07367d06cf41e19e54g0",
        ] {
            assert!(!valid_object_id(bad), "{bad}");
        }
    }

    /// The claim's answer is the command SYSTEM runs, so its origin must be
    /// authenticated: https, or a loopback test server.
    #[test]
    fn the_service_talks_https_only() {
        assert_eq!(
            origin_ok("https://roomler.ai/").unwrap(),
            "https://roomler.ai"
        );
        assert!(origin_ok("http://127.0.0.1:5001").is_ok());
        assert!(origin_ok("http://localhost:3000").is_ok());
        assert!(origin_ok("http://[::1]:3000").is_ok());
        assert!(origin_ok("http://roomler.ai").is_err());
        assert!(origin_ok("http://10.0.0.5").is_err());
        assert!(origin_ok("ftp://roomler.ai").is_err());
        assert!(origin_ok("https://roomler.ai/?x=1").is_err());
        assert!(origin_ok("roomler.ai").is_err());
    }

    fn anchor() -> Anchor {
        Anchor {
            version: 1,
            server: "https://roomler.ai".into(),
            tenant_id: "69a1dbbad2000f26adc875ce".into(),
            agent_id: "6ace07367d06cf41e19e54b0".into(),
            device_name: "HOST".into(),
            service_token: "test-only-service-credential-not-a-jwt".into(),
            exec_enabled: true,
            anchored_at: "2026-10-13T12:00:00Z".into(),
            ssh: Default::default(),
            extra: Default::default(),
        }
    }

    #[test]
    fn an_anchor_never_prints_its_credential() {
        let shown = format!("{:?}", anchor());
        assert!(!shown.contains("test-only-service-credential"), "{shown}");
        assert!(shown.contains("[redacted]"));
    }

    #[test]
    fn an_anchor_is_validated_whole() {
        assert!(anchor().validate().is_ok());
        let mut a = anchor();
        a.server = "http://roomler.ai".into();
        assert!(a.validate().is_err(), "plain http");
        let mut a = anchor();
        a.agent_id = "nope".into();
        assert!(a.validate().is_err(), "a malformed device");
        let mut a = anchor();
        a.version = 2;
        assert!(a.validate().is_err(), "a newer schema");
        let mut a = anchor();
        a.service_token = " ".into();
        assert!(a.validate().is_err(), "no credential");
        let back: Anchor =
            serde_json::from_str(&serde_json::to_string(&anchor()).unwrap()).unwrap();
        assert_eq!(back, anchor());
    }

    /// The wire is what an older service and a newer worker (or the reverse)
    /// both read, so its spellings are locked.
    #[test]
    fn the_doorbell_wire_is_locked() {
        assert_eq!(
            serde_json::to_string(&DoorbellRequest::Health).unwrap(),
            r#"{"t":"health"}"#
        );
        assert_eq!(
            serde_json::to_string(&DoorbellRequest::Ring {
                request_id: "6ace07367d06cf41e19e54b0".into()
            })
            .unwrap(),
            r#"{"t":"ring","request_id":"6ace07367d06cf41e19e54b0"}"#
        );
        assert_eq!(
            serde_json::to_string(&DoorbellReply::Rang {
                claimed: true,
                error: None
            })
            .unwrap(),
            r#"{"t":"rang","claimed":true}"#
        );
        let health: DoorbellReply = serde_json::from_str(
            r#"{"t":"health","anchored":true,"agent_id":"6ace07367d06cf41e19e54b0","exec_enabled":true,"version":"0.4.133"}"#,
        )
        .unwrap();
        assert!(matches!(
            health,
            DoorbellReply::Health {
                anchored: true,
                problem: None,
                ..
            }
        ));
    }

    #[test]
    fn exec_system_is_added_and_removed_by_equality() {
        let mut rpc: Vec<String> = ["exec", "originate"].map(String::from).to_vec();
        apply_word(&mut rpc, true);
        apply_word(&mut rpc, true);
        assert_eq!(rpc, ["exec", "originate", "exec-system"]);
        apply_word(&mut rpc, false);
        assert_eq!(rpc, ["exec", "originate"], "`exec` stays");
    }

    #[test]
    fn a_token_renews_well_before_it_expires() {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"x","exp":2000000000}"#);
        let token = format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig");
        assert_eq!(token_expiry(&token), Some(2_000_000_000));
        assert_eq!(token_expiry("not-a-jwt"), None);
        let now = 2_000_000_000 - 200 * 24 * 60 * 60;
        assert!(!renewal_due(Some(2_000_000_000), now), "200 days left");
        assert!(
            renewal_due(Some(2_000_000_000), 2_000_000_000 - 60),
            "a minute left"
        );
        assert!(renewal_due(None, now), "unreadable: renew");
    }

    #[test]
    fn rings_are_rate_limited() {
        let t0 = std::time::Instant::now();
        let mut l = RingLimiter::new(t0);
        let allowed = (0..20).filter(|_| l.allow(t0)).count();
        assert_eq!(
            allowed,
            RingLimiter::BURST as usize,
            "a burst, then no more"
        );
        // Two seconds refill one ring at 30 a minute.
        assert!(l.allow(t0 + std::time::Duration::from_secs(2)));
        assert!(!l.allow(t0 + std::time::Duration::from_secs(2)));
    }

    /// An unknown identity is a refusal, never SYSTEM.
    #[test]
    fn an_unknown_run_as_is_refused() {
        assert_eq!(run_as_of("system").unwrap().1, "system");
        assert_eq!(run_as_of("console_user").unwrap().1, "console_user");
        assert!(run_as_of("named").is_err());
        assert!(run_as_of("").is_err());
    }

    /// One request, one reply, over any stream; garbage and an over-long
    /// line are answered or dropped, never acted on.
    #[tokio::test]
    async fn one_request_one_reply() {
        let (client, server) = tokio::io::duplex(8 * 1024);
        let served = tokio::spawn(serve_connection(server, |req| async move {
            match req {
                DoorbellRequest::Ring { request_id } => DoorbellReply::Rang {
                    claimed: request_id == "6ace07367d06cf41e19e54b0",
                    error: None,
                },
                _ => DoorbellReply::Refused { error: "no".into() },
            }
        }));
        let reply = exchange(
            client,
            &DoorbellRequest::Ring {
                request_id: "6ace07367d06cf41e19e54b0".into(),
            },
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(
            reply,
            DoorbellReply::Rang {
                claimed: true,
                error: None
            }
        );
        served.await.unwrap();

        // Not a request: refused, unread.
        let (mut client, server) = tokio::io::duplex(8 * 1024);
        let served = tokio::spawn(serve_connection(server, |_| async {
            never("a garbage line must never reach the handler")
        }));
        use tokio::io::AsyncWriteExt as _;
        client
            .write_all(b"{\"t\":\"exec\",\"command\":\"whoami\"}\n")
            .await
            .unwrap();
        let line = read_line(&mut client).await.unwrap();
        assert!(line.contains("refused"), "{line}");
        served.await.unwrap();

        // Longer than a line may be: dropped without an answer.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let served = tokio::spawn(serve_connection(server, |_| async {
            never("an over-long line must never reach the handler")
        }));
        client.write_all(&vec![b'x'; MAX_LINE + 10]).await.unwrap();
        served.await.unwrap();
    }

    fn never(why: &str) -> DoorbellReply {
        panic!("{why}")
    }

    /// The one-time copy reads the anchor's first home through FR-94's
    /// reader, which takes a file only from a folder and a file owned by
    /// SYSTEM or Administrators that give no other account write. A folder an
    /// ordinary account made, holding a well-formed anchor, is refused: by its
    /// owner when this runs unelevated, by its inherited writable DACL when it
    /// runs elevated (Administrators then own it). Either way, never read.
    #[cfg(windows)]
    #[test]
    fn a_legacy_anchor_in_a_folder_a_user_made_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchor.json");
        std::fs::write(&path, serde_json::to_string(&anchor()).unwrap()).unwrap();
        let err = legacy::read_at(dir.path(), &path).unwrap_err();
        assert!(err.contains("SYSTEM or Administrators"), "{err}");
        // And no file is not an error: nothing to trust.
        let empty = tempfile::tempdir().unwrap();
        let missing = empty.path().join("anchor.json");
        assert!(matches!(
            legacy::read_at(&empty.path().join("nope"), &missing),
            Ok(None)
        ));
    }

    /// R3 — the one-time copy takes only the folder and the file exactly as
    /// `roomlerd anchor create` left them (every build wrote `O:BA` and a
    /// protected DACL of SYSTEM and Administrators alone), and only an anchor
    /// at an https origin: a loopback origin is a lab's.
    #[test]
    fn only_the_shape_anchor_create_leaves_is_copied() {
        use roomler_node_core::machine_tree::{
            ACE_ALLOW, ADMINISTRATORS, Ace, CONTAINER_INHERIT, FULL, INHERITED, OBJECT_INHERIT,
            READ_EXECUTE, SYSTEM, Sd,
        };
        const USERS: &str = "S-1-5-32-545";
        let ace = |flags: u8, mask: u32, sid: &str| Ace {
            kind: ACE_ALLOW,
            flags,
            mask,
            sid: sid.into(),
        };
        let oici = OBJECT_INHERIT | CONTAINER_INHERIT;
        let folder = Sd {
            owner: ADMINISTRATORS.into(),
            protected: true,
            dacl: Some(vec![
                ace(oici, FULL, SYSTEM),
                ace(oici, FULL, ADMINISTRATORS),
            ]),
        };
        let file = Sd {
            owner: ADMINISTRATORS.into(),
            protected: true,
            dacl: Some(vec![ace(0, FULL, SYSTEM), ace(0, FULL, ADMINISTRATORS)]),
        };
        let with = |sd: &Sd, change: &dyn Fn(&mut Sd)| {
            let mut sd = sd.clone();
            change(&mut sd);
            sd
        };
        assert!(legacy_shape_ok(&folder, &file), "what anchor create leaves");
        assert!(
            legacy_shape_ok(
                &with(&folder, &|s| s.dacl.as_mut().unwrap().reverse()),
                &file
            ),
            "the same two entries in the other order"
        );
        let refused: [(&str, Sd, Sd); 11] = [
            (
                "a folder SYSTEM owns",
                with(&folder, &|s| s.owner = SYSTEM.into()),
                file.clone(),
            ),
            (
                "a file SYSTEM owns",
                folder.clone(),
                with(&file, &|s| s.owner = SYSTEM.into()),
            ),
            (
                "a third entry on the folder",
                with(&folder, &|s| {
                    s.dacl
                        .as_mut()
                        .unwrap()
                        .push(ace(oici, READ_EXECUTE, USERS))
                }),
                file.clone(),
            ),
            (
                "a third entry on the file",
                folder.clone(),
                with(&file, &|s| {
                    s.dacl.as_mut().unwrap().push(ace(0, READ_EXECUTE, USERS))
                }),
            ),
            (
                "a folder that inherits",
                with(&folder, &|s| s.protected = false),
                file.clone(),
            ),
            (
                "a file that inherits",
                folder.clone(),
                with(&file, &|s| s.protected = false),
            ),
            (
                "the file with the folder's inheritance flags",
                folder.clone(),
                with(&file, &|s| {
                    s.dacl = Some(vec![
                        ace(oici, FULL, SYSTEM),
                        ace(oici, FULL, ADMINISTRATORS),
                    ])
                }),
            ),
            (
                "a folder that hands nothing down",
                with(&folder, &|s| {
                    s.dacl = Some(vec![ace(0, FULL, SYSTEM), ace(0, FULL, ADMINISTRATORS)])
                }),
                file.clone(),
            ),
            (
                "inherited entries",
                folder.clone(),
                with(&file, &|s| {
                    s.dacl = Some(vec![
                        ace(INHERITED, FULL, SYSTEM),
                        ace(INHERITED, FULL, ADMINISTRATORS),
                    ])
                }),
            ),
            (
                "Administrators with less than full control",
                folder.clone(),
                with(&file, &|s| s.dacl.as_mut().unwrap()[1].mask = READ_EXECUTE),
            ),
            (
                "a NULL DACL",
                folder.clone(),
                with(&file, &|s| s.dacl = None),
            ),
        ];
        for (case, folder, file) in &refused {
            assert!(!legacy_shape_ok(folder, file), "{case}");
        }
        assert_eq!(copyable(&anchor()), Ok(()));
        for origin in [
            "http://127.0.0.1:5001",
            "http://localhost:3000",
            "http://[::1]:3000",
        ] {
            let mut a = anchor();
            a.server = origin.into();
            assert!(copyable(&a).is_err(), "{origin}");
        }
        let mut a = anchor();
        a.agent_id = "nope".into();
        assert!(copyable(&a).is_err(), "a malformed anchor");
    }

    /// No origin on this machine is copied, over https or not: a loopback
    /// name or address, a name below `.localhost`, an IPv4 loopback address
    /// written as IPv6, or an unspecified address.
    #[test]
    fn no_origin_on_this_machine_is_copyable() {
        for origin in [
            "https://localhost",
            "https://localhost:8443",
            "https://LOCALHOST.",
            "https://app.localhost",
            "https://127.0.0.1:5001",
            "https://127.1",
            "https://[::1]:3000",
            "https://[::ffff:127.0.0.1]",
            "https://0.0.0.0",
            "https://[::]",
        ] {
            let mut a = anchor();
            a.server = origin.into();
            assert!(copyable(&a).is_err(), "{origin}");
        }
        assert_eq!(copyable(&anchor()), Ok(()));
    }

    /// R4-R9 — where the anchor lives on a machine, each case in a child
    /// process of this test binary. A case names its own stand-ins for the
    /// machine's `%ProgramData%` (#3702's `program_data_is`) and for
    /// `HKLM\SOFTWARE` (an HKCU key the parent makes before and removes
    /// after, with `Roomler` below it), which are process-wide. Only an
    /// elevated token sets Administrators as an owner, so a case says so and
    /// stops unelevated, and fails in CI's elevated lane.
    #[cfg(windows)]
    mod on_the_machine {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use roomler_node_core::machine_key::test_support as keys;
        use roomler_node_core::machine_key::{Hive, KeyPath};
        use roomler_node_core::machine_tree::ADMINISTRATORS;
        use roomler_node_core::machine_tree::test_support::{
            aces, between_check_and_use, elevated_or_skip, junction, me, owner, program_data,
            protected, set_dacl,
        };

        use super::super::{Anchor, legacy, load_anchor, load_host_key, store};
        use super::anchor;

        const ROOT: &str = "ROOMLERD_TEST_EXEC_ANCHOR_ROOT";
        const KEY: &str = "ROOMLERD_TEST_EXEC_ANCHOR_KEY";
        const CHECKED: &str = "exec-anchor case: checked";
        /// What the writer makes.
        const SEALED: &str = "O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)";
        /// The sealed key with Users' full control added, as an administrator
        /// troubleshooting it might leave it.
        const LOOSE: &str = "O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KA;;;BU)";
        /// `HKLM\SOFTWARE`'s own entries, protected here: what a parent made
        /// below its stand-in inherits.
        const SOFTWARE: &str = "O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KR;;;BU)(A;CIIO;KA;;;CO)";
        /// Where every case's stand-in keys live, below the person's own hive.
        const SCRATCH: &str = r"Software\RoomlerTest";
        const HOST_KEY: &str = "test-only host key, not a key";
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        /// Removes the case's stand-in keys whatever the child did, and the
        /// scratch key above them when no other case still uses it.
        struct Cleanup(String);

        impl Drop for Cleanup {
            fn drop(&mut self) {
                keys::remove_tree(Hive::CurrentUser, &self.0);
                keys::remove_if_empty(Hive::CurrentUser, SCRATCH);
            }
        }

        /// The parent's half: an elevated token, a stand-in `%ProgramData%`
        /// whose machine folder inherits what `%ProgramData%` hands Users (an
        /// attended host's), and a stand-in for `HKLM\SOFTWARE\Roomler` as a
        /// machine has it; then the case `name` in a child, which must say it
        /// ran.
        fn run(name: &str, case: &str) {
            run_case(name, case, Some(keys::PARENT_SDDL), None);
        }

        /// [`run`], with the parent stand-in made with `parent_sddl` (`None`:
        /// not made, so the writer makes it below the stand-in for
        /// `HKLM\SOFTWARE`), and the copy's switch set to `switch` in the
        /// child (`None`: not set).
        fn run_case(name: &str, case: &str, parent_sddl: Option<&str>, switch: Option<&str>) {
            if !elevated_or_skip(case) {
                return;
            }
            let root = tempfile::tempdir().unwrap();
            let program_data = program_data(root.path());
            std::fs::create_dir_all(program_data.join("roomler").join("roomler")).unwrap();
            keys::ensure(Hive::CurrentUser, SCRATCH);
            let sub = format!(
                r"{SCRATCH}\exec-anchor-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            );
            keys::make(Hive::CurrentUser, &sub, SOFTWARE);
            let _cleanup = Cleanup(sub.clone());
            if let Some(sddl) = parent_sddl {
                keys::make(Hive::CurrentUser, &format!(r"{sub}\Roomler"), sddl);
            }
            let module = module_path!();
            let test = format!(
                "{}::{name}",
                module.split_once("::").map_or(module, |(_, rest)| rest)
            );
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([test.as_str(), "--exact", "--nocapture", "--test-threads=1"])
                .env(ROOT, root.path())
                .env(KEY, &sub)
                .env_remove(super::super::COPY_SWITCH_ENV);
            if let Some(value) = switch {
                child.env(super::super::COPY_SWITCH_ENV, value);
            }
            let out = child.output().unwrap();
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success(), "{said}");
            assert!(said.contains(CHECKED), "the child ran the case: {said}");
        }

        /// The child's half: both stand-ins, before anything names the machine
        /// tree or the key. `None` in the parent.
        fn child() -> Option<KeyPath> {
            let root = PathBuf::from(std::env::var_os(ROOT)?);
            roomler_node_core::appdirs::test_fixture::program_data_is(root.join("ProgramData"));
            crate::exec_service::test_fixture::anchor_key_is(
                Hive::CurrentUser,
                &format!(r"{}\Roomler", software()),
            );
            Some(crate::exec_service::test_fixture::key())
        }

        /// In a case: the stand-in for `HKLM\SOFTWARE`, which holds the parent
        /// stand-in and whatever else the case makes.
        fn software() -> String {
            std::env::var(KEY).expect("the stand-in key")
        }

        /// In a case: a folder beside the stand-in `%ProgramData%`, which
        /// nothing roomlerd names.
        fn elsewhere() -> PathBuf {
            PathBuf::from(std::env::var_os(ROOT).expect("in a case")).join("elsewhere")
        }

        fn sub(key: &KeyPath) -> String {
            format!("{}\\{}", key.parent, key.name)
        }

        /// From here on what this process makes is owned by its own account,
        /// as with the "Default owner for objects created by members of the
        /// Administrators group: Object creator" policy, not by
        /// Administrators.
        fn objects_owned_by_this_account() {
            use windows_sys::Win32::Security::{
                SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
                TokenOwner, TokenUser,
            };
            let token = crate::win_token::own_token(TOKEN_QUERY | TOKEN_ADJUST_DEFAULT)
                .expect("this process's token");
            let user = crate::win_token::token_info(token.raw(), TokenUser).expect("its user");
            // SAFETY: a TOKEN_USER whose SID lives in `user`, alive across the
            // call below.
            let owner = TOKEN_OWNER {
                Owner: unsafe { (*(user.as_ptr() as *const TOKEN_USER)).User.Sid },
            };
            // SAFETY: `owner` and the SID it names outlive the call.
            let ok = unsafe {
                SetTokenInformation(
                    token.raw(),
                    TokenOwner,
                    (&raw const owner).cast(),
                    std::mem::size_of::<TOKEN_OWNER>() as u32,
                )
            };
            assert_ne!(
                ok, 0,
                "this account as the owner of what this process makes"
            );
        }

        /// The key is the one the writer makes: Administrators', SYSTEM and
        /// Administrators alone, not inheriting.
        fn sealed_here(key: &KeyPath) -> Result<(), String> {
            let sddl = keys::sddl(Hive::CurrentUser, &sub(key));
            let mut got = aces(&sddl);
            got.sort();
            if owner(&sddl) == "BA" && protected(&sddl) && got == ["A;CI;KA;;;BA", "A;CI;KA;;;SY"] {
                Ok(())
            } else {
                Err(sddl)
            }
        }

        /// An anchor where every build before this one kept it, as they wrote
        /// it: a sealed `exec-service` folder and file in the machine folder.
        fn write_legacy(anchor: &Anchor) {
            let text = serde_json::to_string_pretty(anchor).unwrap();
            legacy::write("anchor.json", text.as_bytes()).expect("the legacy writer");
        }

        /// R4 — once the first start of this build has run, a well-formed,
        /// sealed anchor file in an open machine folder is not read: the
        /// anchor comes from the key alone, and the key holds none.
        #[test]
        fn an_anchor_in_the_machine_folder_is_not_read_after_the_copy_ran() {
            let Some(key) = child() else {
                return run(
                    "an_anchor_in_the_machine_folder_is_not_read_after_the_copy_ran",
                    "R4: an anchor file after the first start",
                );
            };
            write_legacy(&anchor());
            keys::make(Hive::CurrentUser, &sub(&key), SEALED);
            keys::put(
                &key,
                "LegacyCopy",
                r#"{"at":"2026-11-12T00:00:00Z","outcome":"none"}"#,
            );
            assert_eq!(load_anchor(), Ok(None), "the file is not read");
            println!("{CHECKED}");
        }

        /// R5 — the first start leaves its marker, and nothing that appears in
        /// the machine folder after it is copied at a later start.
        #[test]
        fn nothing_put_in_the_machine_folder_after_the_first_start_is_ever_copied() {
            let Some(key) = child() else {
                return run(
                    "nothing_put_in_the_machine_folder_after_the_first_start_is_ever_copied",
                    "R5: an anchor file that appears after the first start",
                );
            };
            store::copy_legacy_once();
            let marker = keys::get(&key, "LegacyCopy").expect("the first start leaves its marker");
            assert!(marker.contains(r#""outcome":"none""#), "{marker}");
            write_legacy(&anchor());
            store::copy_legacy_once();
            assert!(
                !keys::names(&key).iter().any(|n| n == "Anchor"),
                "nothing is copied at a later start: {:?}",
                keys::names(&key)
            );
            assert_eq!(load_anchor(), Ok(None));
            println!("{CHECKED}");
        }

        /// R6 (control) — `anchor create` writes the anchor to the sealed key,
        /// which the service reads it from, leaves the copy's marker, and
        /// deletes what an earlier build kept in the machine folder.
        #[test]
        fn a_created_anchor_loads_from_the_key_and_leaves_no_file() {
            let Some(key) = child() else {
                return run(
                    "a_created_anchor_loads_from_the_key_and_leaves_no_file",
                    "R6: a created anchor",
                );
            };
            let mut earlier = anchor();
            earlier.device_name = "EARLIER".into();
            write_legacy(&earlier);
            legacy::write(legacy::HOST_KEY_FILE, HOST_KEY.as_bytes()).unwrap();
            let a = anchor();
            store::anchor_created(&a).unwrap();
            assert_eq!(load_anchor(), Ok(Some(a)), "it loads");
            assert!(
                !legacy::anchor_path().exists(),
                "no anchor file is left in the machine folder"
            );
            assert!(
                !legacy::host_key_path().exists(),
                "no host key file is left in the machine folder"
            );
            assert_eq!(sealed_here(&key), Ok(()));
            let marker = keys::get(&key, "LegacyCopy").expect("the copy's marker");
            assert!(marker.contains(r#""outcome":"none""#), "{marker}");
            println!("{CHECKED}");
        }

        /// R7 (control) — an upgrade copies an anchor in the exact shape every
        /// build wrote, whole, with its host key, once, and leaves the file
        /// for a rollback.
        #[test]
        fn an_upgrade_copies_a_real_anchor_once_and_keeps_the_file() {
            let Some(key) = child() else {
                return run(
                    "an_upgrade_copies_a_real_anchor_once_and_keeps_the_file",
                    "R7: an upgrade with an anchor",
                );
            };
            let mut a = anchor();
            a.extra.insert(
                "written_by_a_newer_build".into(),
                serde_json::json!({ "kept": true }),
            );
            write_legacy(&a);
            legacy::write(legacy::HOST_KEY_FILE, HOST_KEY.as_bytes()).unwrap();
            store::copy_legacy_once();
            let copied: Anchor =
                serde_json::from_str(&keys::get(&key, "Anchor").expect("the anchor was copied"))
                    .unwrap();
            assert_eq!(copied, a, "the whole anchor, a newer build's keys included");
            assert_eq!(
                keys::get(&key, "SshHostKey").as_deref(),
                Some(HOST_KEY),
                "the host key with it"
            );
            let marker: serde_json::Value =
                serde_json::from_str(&keys::get(&key, "LegacyCopy").expect("the marker")).unwrap();
            assert_eq!(marker["outcome"], "copied", "{marker}");
            assert_eq!(marker["agent_id"], a.agent_id.as_str(), "{marker}");
            assert_eq!(marker["anchored_at"], a.anchored_at.as_str(), "{marker}");
            assert!(
                legacy::anchor_path().exists(),
                "the file stays, for a rollback"
            );
            assert_eq!(load_anchor(), Ok(Some(a.clone())));
            assert_eq!(sealed_here(&key), Ok(()));
            let mut later = a.clone();
            later.device_name = "LATER".into();
            write_legacy(&later);
            store::copy_legacy_once();
            assert_eq!(
                load_anchor(),
                Ok(Some(a)),
                "a later start copies nothing, whatever the file says now"
            );
            println!("{CHECKED}");
        }

        /// R8 — a key that fails the check (one more entry, for Users' read)
        /// is taken over and emptied before an administrator's write: the
        /// sealed owner and DACL, and nothing it held before.
        #[test]
        fn a_key_that_fails_the_verdict_is_taken_over_and_emptied() {
            let Some(key) = child() else {
                return run(
                    "a_key_that_fails_the_verdict_is_taken_over_and_emptied",
                    "R8: a key that fails the check",
                );
            };
            keys::make(
                Hive::CurrentUser,
                &sub(&key),
                "O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;0x1;;;BU)",
            );
            keys::put(&key, "SshHostKey", "a value roomlerd never wrote");
            let refused = load_anchor().expect_err("the reader refuses a key Users may read");
            assert!(refused.contains("read what it holds"), "{refused}");
            let a = anchor();
            store::store_anchor(&a).unwrap();
            assert_eq!(sealed_here(&key), Ok(()));
            assert_eq!(
                keys::names(&key),
                ["Anchor"],
                "nothing the key held before is kept"
            );
            assert_eq!(load_anchor(), Ok(Some(a)));
            println!("{CHECKED}");
        }

        /// R9 — `anchor remove` takes the anchor and the host key out of the
        /// key and deletes the files an earlier build kept, and keeps the
        /// copy's marker, so nothing is ever copied again.
        #[test]
        fn anchor_remove_takes_the_key_values_and_the_legacy_file() {
            let Some(key) = child() else {
                return run(
                    "anchor_remove_takes_the_key_values_and_the_legacy_file",
                    "R9: anchor remove",
                );
            };
            let a = anchor();
            keys::make(Hive::CurrentUser, &sub(&key), SEALED);
            keys::put(&key, "SshHostKey", HOST_KEY);
            keys::put(&key, "Anchor", &serde_json::to_string(&a).unwrap());
            keys::put(
                &key,
                "LegacyCopy",
                r#"{"at":"2026-11-12T00:00:00Z","outcome":"copied"}"#,
            );
            write_legacy(&a);
            legacy::write(legacy::HOST_KEY_FILE, HOST_KEY.as_bytes()).unwrap();
            assert_eq!(store::remove(), Ok(true));
            assert_eq!(
                keys::names(&key),
                ["LegacyCopy"],
                "the marker stays, so nothing is copied again"
            );
            assert!(!legacy::anchor_path().exists(), "no anchor file is left");
            assert!(
                !legacy::host_key_path().exists(),
                "no host key file is left"
            );
            assert_eq!(load_anchor(), Ok(None));
            assert_eq!(store::remove(), Ok(false), "a second remove finds nothing");
            println!("{CHECKED}");
        }

        /// The copy refuses an anchor file whose DACL is not the exact one
        /// `anchor create` gave it (one more entry here, Users' read, which
        /// FR-94's reader alone would take), records why, and the service's
        /// health and `anchor status` say so with the way back.
        #[test]
        fn a_legacy_anchor_in_another_shape_is_refused_and_says_why() {
            let Some(key) = child() else {
                return run(
                    "a_legacy_anchor_in_another_shape_is_refused_and_says_why",
                    "the copy's exact-shape refusal",
                );
            };
            write_legacy(&anchor());
            set_dacl(
                &legacy::anchor_path(),
                "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;BU)",
            );
            assert!(
                legacy::read_at(&legacy::dir(), &legacy::anchor_path()).is_ok_and(|a| a.is_some()),
                "FR-94's reader alone takes it"
            );
            store::copy_legacy_once();
            assert!(
                !keys::names(&key).iter().any(|n| n == "Anchor"),
                "nothing is copied"
            );
            let marker: serde_json::Value =
                serde_json::from_str(&keys::get(&key, "LegacyCopy").expect("the marker")).unwrap();
            assert_eq!(marker["outcome"], "refused", "{marker}");
            let why = store::copy_refused().expect("the refusal is reported");
            assert!(why.contains("exact owner and DACL"), "{why}");
            let problem = super::super::copy_refused_problem(&why);
            assert!(problem.contains("roomlerd anchor create"), "{problem}");
            assert_eq!(load_anchor(), Ok(None));
            assert!(legacy::anchor_path().exists(), "the file stays");
            println!("{CHECKED}");
        }

        /// `ROOMLERD_EXEC_ANCHOR_COPY=0` in the service's environment: the
        /// first start copies nothing, and records why.
        #[test]
        fn the_copy_switch_copies_nothing() {
            let Some(key) = child() else {
                return run_case(
                    "the_copy_switch_copies_nothing",
                    "the copy's switch",
                    Some(keys::PARENT_SDDL),
                    Some("0"),
                );
            };
            write_legacy(&anchor());
            store::copy_legacy_once();
            assert!(
                !keys::names(&key).iter().any(|n| n == "Anchor"),
                "nothing is copied"
            );
            let marker: serde_json::Value =
                serde_json::from_str(&keys::get(&key, "LegacyCopy").expect("the marker")).unwrap();
            assert_eq!(marker["outcome"], "refused", "{marker}");
            assert!(
                marker["why"]
                    .as_str()
                    .is_some_and(|why| why.contains("switched off")),
                "{marker}"
            );
            assert_eq!(load_anchor(), Ok(None));
            println!("{CHECKED}");
        }

        /// A parent that hands Users a right to change a key made below it
        /// fails closed: the reader refuses the key below it, and a writer
        /// writes nothing there.
        #[test]
        fn a_key_below_a_parent_another_account_may_change_is_refused() {
            let Some(key) = child() else {
                return run_case(
                    "a_key_below_a_parent_another_account_may_change_is_refused",
                    "a parent another account may change",
                    Some(&format!("{}(A;CIIO;KW;;;BU)", keys::PARENT_SDDL)),
                    None,
                );
            };
            keys::make(Hive::CurrentUser, &sub(&key), SEALED);
            keys::put(&key, "Anchor", &serde_json::to_string(&anchor()).unwrap());
            let refused = load_anchor().expect_err("the reader refuses the key");
            assert!(refused.contains("a key made below it"), "{refused}");
            assert!(
                store::store_anchor(&anchor()).is_err(),
                "the writer writes nothing there"
            );
            println!("{CHECKED}");
        }

        /// Between the folder's check and the first file opened in it, move
        /// `exec-service` aside and make a junction to `to` in its name. Where
        /// the folder went.
        fn swap_for_a_junction_after_the_check(to: &std::path::Path) -> PathBuf {
            let dir = legacy::dir();
            let aside = dir.with_file_name("exec-service-aside");
            let (moved, to) = (aside.clone(), to.to_path_buf());
            between_check_and_use(move || {
                std::fs::rename(&dir, &moved).expect("the folder moves aside");
                junction(&dir, &to);
            });
            aside
        }

        /// `anchor remove` deletes only through the folder it checked:
        /// when `exec-service` is moved aside and a junction takes its name
        /// between the check and the delete, nothing the junction leads to is
        /// deleted, and the file of the folder that was checked is.
        #[test]
        fn a_folder_swapped_for_a_junction_after_its_check_loses_nothing_elsewhere() {
            let Some(_key) = child() else {
                return run(
                    "a_folder_swapped_for_a_junction_after_its_check_loses_nothing_elsewhere",
                    "a folder swapped for a junction after its check",
                );
            };
            write_legacy(&anchor());
            std::fs::create_dir_all(elsewhere()).unwrap();
            let bystander = elsewhere().join("anchor.json");
            std::fs::write(&bystander, b"a file an administrator may delete").unwrap();
            let aside = swap_for_a_junction_after_the_check(&elsewhere());
            let _ = store::remove();
            assert!(
                bystander.exists(),
                "nothing the junction leads to is deleted"
            );
            assert!(
                !aside.join("anchor.json").exists(),
                "the file of the folder that was checked is deleted"
            );
            println!("{CHECKED}");
        }

        /// Only a folder and a file each in exactly the shape `anchor
        /// create` gave them lose a file: a folder made another way, or a
        /// file with another DACL in the exact folder, is left, whatever its
        /// name.
        #[test]
        fn only_a_folder_and_a_file_in_the_exact_shape_lose_a_file() {
            let Some(_key) = child() else {
                return run(
                    "only_a_folder_and_a_file_in_the_exact_shape_lose_a_file",
                    "a folder or a file in another shape",
                );
            };
            // A folder made here: it inherits the open machine folder's entries.
            std::fs::create_dir(legacy::dir()).unwrap();
            std::fs::write(legacy::anchor_path(), b"no anchor create wrote this").unwrap();
            std::fs::write(legacy::host_key_path(), HOST_KEY).unwrap();
            store::anchor_created(&anchor()).unwrap();
            let _ = store::remove();
            assert!(
                legacy::anchor_path().exists(),
                "a folder in another shape keeps its anchor file"
            );
            assert!(
                legacy::host_key_path().exists(),
                "a folder in another shape keeps its host key file"
            );
            std::fs::remove_file(legacy::anchor_path()).unwrap();
            std::fs::remove_file(legacy::host_key_path()).unwrap();
            std::fs::remove_dir(legacy::dir()).unwrap();
            // The exact folder, holding files in another shape.
            write_legacy(&anchor());
            set_dacl(
                &legacy::anchor_path(),
                "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;BU)",
            );
            std::fs::write(legacy::host_key_path(), HOST_KEY).unwrap();
            store::anchor_created(&anchor()).unwrap();
            let _ = store::remove();
            assert!(
                legacy::anchor_path().exists(),
                "a file with one more entry is left"
            );
            assert!(
                legacy::host_key_path().exists(),
                "a file that inherits its entries is left"
            );
            println!("{CHECKED}");
        }

        /// A junction at `exec-service` is never followed: `anchor
        /// create` and `anchor remove` delete nothing it leads to, even files
        /// in the exact shape.
        #[test]
        fn a_junction_at_exec_service_deletes_nothing() {
            let Some(_key) = child() else {
                return run(
                    "a_junction_at_exec_service_deletes_nothing",
                    "a junction at exec-service",
                );
            };
            let text = serde_json::to_string_pretty(&anchor()).unwrap();
            legacy::write_in(&elsewhere(), "anchor.json", text.as_bytes()).unwrap();
            legacy::write_in(&elsewhere(), legacy::HOST_KEY_FILE, HOST_KEY.as_bytes()).unwrap();
            junction(&legacy::dir(), &elsewhere());
            store::anchor_created(&anchor()).unwrap();
            let _ = store::remove();
            assert!(
                elsewhere().join("anchor.json").exists(),
                "the anchor file the junction leads to stays"
            );
            assert!(
                elsewhere().join(legacy::HOST_KEY_FILE).exists(),
                "the host key file the junction leads to stays"
            );
            println!("{CHECKED}");
        }

        /// The one-time copy reads the anchor file of the folder it
        /// checked, below that folder's handle: a junction that takes the
        /// folder's name after the check brings no other anchor in.
        #[test]
        fn the_copy_reads_the_file_of_the_folder_it_checked() {
            let Some(key) = child() else {
                return run(
                    "the_copy_reads_the_file_of_the_folder_it_checked",
                    "the copy reads below the folder it checked",
                );
            };
            let here = anchor();
            write_legacy(&here);
            let mut other = anchor();
            other.device_name = "ELSEWHERE".into();
            let text = serde_json::to_string_pretty(&other).unwrap();
            legacy::write_in(&elsewhere(), "anchor.json", text.as_bytes()).unwrap();
            swap_for_a_junction_after_the_check(&elsewhere());
            store::copy_legacy_once();
            let copied = keys::get(&key, "Anchor")
                .map(|text| serde_json::from_str::<Anchor>(&text).expect("an anchor"));
            assert_ne!(
                copied.as_ref(),
                Some(&other),
                "the anchor the junction leads to is never copied"
            );
            assert_eq!(copied, Some(here), "the checked folder's anchor is");
            println!("{CHECKED}");
        }

        /// A registry link at the anchor key, or at its parent, is
        /// refused by the reader and by every writer, and nothing changes
        /// where it leads.
        #[test]
        fn a_registry_link_at_the_anchor_key_or_its_parent_is_refused() {
            let Some(key) = child() else {
                return run(
                    "a_registry_link_at_the_anchor_key_or_its_parent_is_refused",
                    "a registry link",
                );
            };
            let a = anchor();
            let mut b = anchor();
            b.device_name = "THROUGH-A-LINK".into();
            let held = |path: &KeyPath| {
                (
                    keys::names(path),
                    keys::sddl(Hive::CurrentUser, &sub(path)),
                    keys::get(path, "Anchor"),
                )
            };
            let refused_everywhere = |what: &str| {
                assert!(load_anchor().is_err(), "the reader refuses {what}");
                assert!(store::store_anchor(&b).is_err(), "a writer refuses {what}");
                assert!(
                    store::anchor_created(&b).is_err(),
                    "anchor create refuses {what}"
                );
                assert!(store::remove().is_err(), "anchor remove refuses {what}");
                store::copy_legacy_once();
            };

            // The key a link to a sealed key that holds an anchor.
            let target = KeyPath {
                hive: Hive::CurrentUser,
                parent: software(),
                name: "Target".into(),
            };
            keys::make(Hive::CurrentUser, &sub(&target), SEALED);
            keys::put(&target, "Anchor", &serde_json::to_string(&a).unwrap());
            keys::link(
                Hive::CurrentUser,
                &sub(&key),
                &keys::kernel_name(Hive::CurrentUser, &sub(&target)),
            );
            let before = held(&target);
            refused_everywhere("a key that is a link");
            assert_eq!(held(&target), before, "nothing changes where it leads");

            // The parent a link to one that holds a sealed key with an anchor.
            keys::remove_tree(Hive::CurrentUser, &key.parent);
            let real = KeyPath {
                hive: Hive::CurrentUser,
                parent: format!(r"{}\RealParent", software()),
                name: "ExecService".into(),
            };
            keys::make(Hive::CurrentUser, &real.parent, keys::PARENT_SDDL);
            keys::make(Hive::CurrentUser, &sub(&real), SEALED);
            keys::put(&real, "Anchor", &serde_json::to_string(&a).unwrap());
            keys::link(
                Hive::CurrentUser,
                &key.parent,
                &keys::kernel_name(Hive::CurrentUser, &real.parent),
            );
            let before = held(&real);
            refused_everywhere("a parent that is a link");
            assert_eq!(held(&real), before, "nothing changes where it leads");
            println!("{CHECKED}");
        }

        /// A key that fails the check is deleted and made again, sealed,
        /// before an administrator's write: a handle opened while it was loose
        /// writes nothing the service reads, nothing the key held is kept (a
        /// value whose name holds a NUL included), and its `LegacyCopy` is.
        #[test]
        fn a_loosened_key_is_made_again_keeping_only_its_marker() {
            let Some(key) = child() else {
                return run(
                    "a_loosened_key_is_made_again_keeping_only_its_marker",
                    "a loosened key taken over",
                );
            };
            keys::make(Hive::CurrentUser, &sub(&key), LOOSE);
            let marker = r#"{"at":"2026-11-12T00:00:00Z","outcome":"copied","agent_id":"6ace07367d06cf41e19e54b0","anchored_at":"2026-10-13T12:00:00Z"}"#;
            keys::put(&key, "LegacyCopy", marker);
            keys::put(&key, "SshHostKey", "a value roomlerd never wrote");
            let nul_name: Vec<u16> = "Stray\u{0}Name".encode_utf16().collect();
            keys::put_raw(&key, &nul_name, "a value whose name holds a NUL");
            let held = keys::opened(&key).expect("a handle opened while the key is loose");
            let a = anchor();
            store::store_anchor(&a).unwrap();
            let mut theirs = anchor();
            theirs.server = "https://elsewhere.example".into();
            let wrote = held.write("Anchor", &serde_json::to_string(&theirs).unwrap());
            assert!(
                wrote.is_err(),
                "a handle opened while the key was loose writes nothing: {wrote:?}"
            );
            assert_eq!(load_anchor(), Ok(Some(a)));
            assert_eq!(sealed_here(&key), Ok(()));
            assert_eq!(
                keys::names(&key),
                ["Anchor", "LegacyCopy"],
                "nothing else the key held is kept"
            );
            assert_eq!(
                keys::value_count(&key),
                2,
                "no value of the loose key is kept, whatever its name"
            );
            assert_eq!(
                keys::get(&key, "LegacyCopy").as_deref(),
                Some(marker),
                "the copy's marker is kept"
            );
            println!("{CHECKED}");
        }

        /// A takeover stopped after any of its steps leaves the key
        /// refused, gone or empty: never sealed around a value the loose key
        /// held.
        #[test]
        fn a_takeover_stopped_midway_leaves_nothing_trusted() {
            let Some(key) = child() else {
                return run(
                    "a_takeover_stopped_midway_leaves_nothing_trusted",
                    "a takeover stopped midway",
                );
            };
            let a = anchor();
            let mut theirs = anchor();
            theirs.server = "https://elsewhere.example".into();
            for step in 1..=3 {
                keys::remove_tree(Hive::CurrentUser, &sub(&key));
                keys::make(Hive::CurrentUser, &sub(&key), LOOSE);
                keys::put(&key, "Anchor", &serde_json::to_string(&theirs).unwrap());
                keys::put(&key, "SshHostKey", "a value roomlerd never wrote");
                keys::stop_takeover_after(step);
                let wrote = store::store_anchor(&a);
                keys::stop_takeover_after(0);
                let read = load_anchor();
                assert!(
                    !matches!(&read, Ok(Some(got)) if *got != a),
                    "stopped after step {step} ({wrote:?}), the service reads {read:?}"
                );
                assert!(
                    !matches!(load_host_key(), Ok(Some(_))),
                    "stopped after step {step}, the service reads the loose key's host key"
                );
            }
            println!("{CHECKED}");
        }

        /// The parent the writer makes is owned by Administrators, also
        /// for a caller whose new objects its own account owns (the "Object
        /// creator" default-owner policy), and inherits what the key above it
        /// hands down.
        #[test]
        fn a_parent_the_writer_makes_is_owned_by_administrators() {
            let Some(key) = child() else {
                return run_case(
                    "a_parent_the_writer_makes_is_owned_by_administrators",
                    "the parent the writer makes",
                    None,
                    None,
                );
            };
            objects_owned_by_this_account();
            let a = anchor();
            store::anchor_created(&a).unwrap();
            assert_eq!(
                keys::owner_sid(Hive::CurrentUser, &key.parent),
                ADMINISTRATORS
            );
            let parent = keys::sddl(Hive::CurrentUser, &key.parent);
            assert!(!protected(&parent), "it inherits: {parent}");
            assert_eq!(load_anchor(), Ok(Some(a)));
            println!("{CHECKED}");
        }

        /// `anchor create` gives a parent its caller's own account owns to
        /// Administrators when nothing else stands in the way. No other writer
        /// changes an owner, and a parent that also fails for another reason
        /// is left as it is.
        #[test]
        fn anchor_create_gives_a_parent_its_caller_owns_to_administrators() {
            let Some(key) = child() else {
                return run_case(
                    "anchor_create_gives_a_parent_its_caller_owns_to_administrators",
                    "a parent its caller owns",
                    Some(&format!(
                        "O:{}D:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KR;;;BU)(A;CIIO;KA;;;CO)",
                        me()
                    )),
                    None,
                );
            };
            let a = anchor();
            assert!(
                store::store_anchor(&a).is_err(),
                "a writer other than anchor create changes no owner"
            );
            assert_eq!(keys::owner_sid(Hive::CurrentUser, &key.parent), me());
            store::anchor_created(&a).unwrap();
            assert_eq!(
                keys::owner_sid(Hive::CurrentUser, &key.parent),
                ADMINISTRATORS
            );
            assert_eq!(load_anchor(), Ok(Some(a.clone())));
            keys::remove_tree(Hive::CurrentUser, &key.parent);
            keys::make(
                Hive::CurrentUser,
                &key.parent,
                &format!("O:{}D:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KW;;;BU)", me()),
            );
            assert!(
                store::anchor_created(&a).is_err(),
                "a parent Users may change too is refused"
            );
            assert_eq!(
                keys::owner_sid(Hive::CurrentUser, &key.parent),
                me(),
                "and keeps its owner"
            );
            println!("{CHECKED}");
        }

        /// A copy that did not finish (its marker still `pending`) is
        /// reported with the way back, as a refused one is; with an anchor in
        /// the key there is nothing to report.
        #[test]
        fn a_copy_that_did_not_finish_is_reported() {
            let Some(key) = child() else {
                return run(
                    "a_copy_that_did_not_finish_is_reported",
                    "a copy that did not finish",
                );
            };
            keys::make(Hive::CurrentUser, &sub(&key), SEALED);
            keys::put(
                &key,
                "LegacyCopy",
                r#"{"at":"2026-11-12T00:00:00Z","outcome":"pending"}"#,
            );
            let problem = store::copy_problem().expect("a copy that did not finish is reported");
            assert!(problem.contains("did not finish"), "{problem}");
            assert!(problem.contains("roomlerd anchor create"), "{problem}");
            keys::put(&key, "Anchor", &serde_json::to_string(&anchor()).unwrap());
            assert_eq!(
                store::copy_problem(),
                None,
                "an anchored device has nothing to report"
            );
            println!("{CHECKED}");
        }

        /// After `anchor remove`, a copy refused long ago no longer asks
        /// for a re-anchor: the marker says `removed`, and when, and keeps
        /// what it said before.
        #[test]
        fn anchor_remove_retires_a_refused_copys_report_and_keeps_its_record() {
            let Some(key) = child() else {
                return run(
                    "anchor_remove_retires_a_refused_copys_report_and_keeps_its_record",
                    "anchor remove after a refused copy",
                );
            };
            keys::make(Hive::CurrentUser, &sub(&key), SEALED);
            keys::put(
                &key,
                "LegacyCopy",
                r#"{"at":"2026-11-12T00:00:00Z","outcome":"refused","why":"the anchor file's folder or file does not have the exact owner and DACL"}"#,
            );
            assert!(store::copy_problem().is_some(), "the refusal is reported");
            store::remove().unwrap();
            assert_eq!(
                store::copy_problem(),
                None,
                "a removed anchor asks for nothing"
            );
            let marker: serde_json::Value =
                serde_json::from_str(&keys::get(&key, "LegacyCopy").expect("the marker stays"))
                    .unwrap();
            assert_eq!(marker["outcome"], "removed", "{marker}");
            assert_ne!(marker["at"], "2026-11-12T00:00:00Z", "{marker}");
            assert_eq!(marker["earlier"][0]["outcome"], "refused", "{marker}");
            assert_eq!(
                marker["earlier"][0]["at"], "2026-11-12T00:00:00Z",
                "{marker}"
            );
            assert!(
                marker["earlier"][0]["why"]
                    .as_str()
                    .is_some_and(|why| why.contains("exact owner")),
                "{marker}"
            );
            println!("{CHECKED}");
        }

        /// A value name is compared as the registry compares it, without
        /// regard to case: a marker or an anchor written as `legacycopy` or
        /// `anchor` counts as there.
        #[test]
        fn a_value_named_in_another_case_counts_as_there() {
            let Some(key) = child() else {
                return run(
                    "a_value_named_in_another_case_counts_as_there",
                    "a value named in another case",
                );
            };
            let mut legacy_anchor = anchor();
            legacy_anchor.device_name = "LEGACY".into();
            write_legacy(&legacy_anchor);
            keys::make(Hive::CurrentUser, &sub(&key), SEALED);
            keys::put(
                &key,
                "legacycopy",
                r#"{"at":"2026-11-12T00:00:00Z","outcome":"none"}"#,
            );
            store::copy_legacy_once();
            assert_eq!(
                load_anchor(),
                Ok(None),
                "a marker named in another case shuts the copy"
            );
            keys::remove_tree(Hive::CurrentUser, &sub(&key));
            keys::make(Hive::CurrentUser, &sub(&key), SEALED);
            let a = anchor();
            keys::put(&key, "anchor", &serde_json::to_string(&a).unwrap());
            store::copy_legacy_once();
            assert_eq!(
                load_anchor(),
                Ok(Some(a)),
                "an anchor named in another case is not copied over"
            );
            println!("{CHECKED}");
        }
    }
}
