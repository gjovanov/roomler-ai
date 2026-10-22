// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-3a — `vault_placeholder`: a secret held as a placeholder, an
//! opaque token in the session's environment that its tools send as they
//! would send the value. The value never reaches the session: the daemon
//! holds it in the lease, and the session's egress proxy swaps it in on the
//! way out, toward the secret's audience only (P3b-3c).
//!
//! | Step | What holds |
//! |---|---|
//! | the call | `env` (1 to 8 `NAME: handle`, each name one `vault_exec` would take), `audience` (an origin, `https://host[:port]`, its host in its one spelling, as the secret's audiences name it), `ttl_s` (30 to 900, default 300) |
//! | the leases | one per entry, mode `placeholder`, toward the audience: the vault releases one only where `vault.placeholder` serves the organization and the secret names the audience (P3b-2). A lease asked for ahead toward the same audience is taken as it is |
//! | a placeholder | `rph_` and 32 base32 characters: 160 bits from the OS CSPRNG, made here as its lease arrives, and known to nobody else |
//! | the answer | `env`: each name's placeholder; the audience; `ttl_s`, what its shortest lease has left; and a note |
//! | the run's placeholders | each with its lease and its audience's host and port: what the proxy swaps. One goes when its lease is let go (its expiry, `rc:vault.revoke`, the run's end), all when the run ends. At most [`MAX_PER_RUN`] at once |
//!
//! ⚠️ No caller yet: P3b-3c offers the tool, once the proxy swaps a
//! placeholder in. Until then no session holds one, which would go out as
//! itself.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::vault::canonical_net_host;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::toolbelt::VaultTools;
use super::vault_exec::{Handle, Releasing, env_name_ok, handle_of};

/// What every placeholder starts with: what tells one apart in a request.
pub const PREFIX: &str = "rph_";
/// Its random part: 20 bytes, 160 bits, as base32.
pub const RANDOM_CHARS: usize = 32;
/// RFC 4648's base32 alphabet, in lower case.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The lease mode a placeholder's value is released in.
pub const MODE: &str = "placeholder";
/// How long a placeholder's leases last when the call names no TTL.
const DEFAULT_TTL: u32 = 300;
const MIN_TTL: u32 = 30;
const MAX_TTL: u32 = 900;
/// The most variables one call sets.
const MAX_ENV: usize = 8;
/// The most placeholders a run holds at once.
pub const MAX_PER_RUN: usize = 64;

type RunKey = (ObjectId, u64);

/// One of a run's placeholders: what the proxy needs to swap its value in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub token: String,
    pub lease_id: String,
    /// The audience's host, in its one spelling ([`canonical_net_host`]),
    /// and its port.
    pub host: String,
    pub port: u16,
}

/// Every run's placeholders.
static RUNS: LazyLock<Mutex<HashMap<RunKey, Vec<Bound>>>> = LazyLock::new(Mutex::default);

fn runs() -> MutexGuard<'static, HashMap<RunKey, Vec<Bound>>> {
    RUNS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A new placeholder: [`PREFIX`] and [`RANDOM_CHARS`] base32 characters from
/// the OS CSPRNG.
pub fn mint() -> Result<String, String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 20];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "the system's random source failed".to_string())?;
    Ok(format!("{PREFIX}{}", base32(&bytes)))
}

/// 20 bytes as 32 base32 characters: RFC 4648, in lower case. 160 bits are
/// 32 groups of five, so nothing is left over and nothing is padded.
fn base32(bytes: &[u8; 20]) -> String {
    let mut out = String::with_capacity(RANDOM_CHARS);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((acc >> bits) & 0x1f) as usize]));
        }
        acc &= (1 << bits) - 1;
    }
    out
}

/// Whether `s` has a placeholder's shape.
pub fn is_placeholder(s: &str) -> bool {
    s.strip_prefix(PREFIX)
        .is_some_and(|r| r.len() == RANDOM_CHARS && r.bytes().all(|b| ALPHABET.contains(&b)))
}

/// The run's placeholders whose audience is `host:port`, the host in its one
/// spelling: what the proxy swaps in on a connection there.
pub fn bound(session: ObjectId, fence: u64, host: &str, port: u16) -> Vec<Bound> {
    runs()
        .get(&(session, fence))
        .map(|all| {
            all.iter()
                .filter(|b| b.host == host && b.port == port)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// How many placeholders the run holds.
pub fn held(session: ObjectId, fence: u64) -> usize {
    runs().get(&(session, fence)).map_or(0, Vec::len)
}

/// Every placeholder of the run gone, as it ends. Answers whether it held
/// any.
pub fn drop_run(session: ObjectId, fence: u64) -> bool {
    runs().remove(&(session, fence)).is_some()
}

/// A placeholder bound to the run with no lease behind it: for a test of
/// what ends the run's (the supervisor's, which are Unix's).
#[cfg(all(test, unix))]
pub(crate) fn bind_for_test(session: ObjectId, fence: u64, host: &str, port: u16) -> String {
    let token = mint().unwrap();
    runs().entry((session, fence)).or_default().push(Bound {
        token: token.clone(),
        lease_id: format!("test-lease-{token}"),
        host: host.to_string(),
        port,
    });
    token
}

/// `token` gone from the run.
fn forget(run: RunKey, token: &str) {
    let mut runs = runs();
    if let Some(all) = runs.get_mut(&run) {
        all.retain(|b| b.token != token);
        if all.is_empty() {
            runs.remove(&run);
        }
    }
}

/// A new placeholder for `lease_id`, bound to the run and to the audience at
/// `host:port` until the lease is let go. Refused where the run holds
/// [`MAX_PER_RUN`] already, or the lease is no longer held.
fn bind(
    vault: &dyn VaultTools,
    run: RunKey,
    lease_id: &str,
    host: &str,
    port: u16,
) -> Result<String, String> {
    let token = mint()?;
    {
        let mut runs = runs();
        let all = runs.entry(run).or_default();
        if all.len() >= MAX_PER_RUN {
            return Err(format!(
                "this session holds {MAX_PER_RUN} placeholders already; one goes as its lease ends"
            ));
        }
        all.push(Bound {
            token: token.clone(),
            lease_id: lease_id.to_string(),
            host: host.to_string(),
            port,
        });
    }
    let stop = CancellationToken::new();
    if !vault.stop_with(lease_id, stop.clone()) {
        forget(run, &token);
        return Err("its lease ended before its placeholder was made".into());
    }
    let gone = token.clone();
    tokio::spawn(async move {
        stop.cancelled().await;
        forget(run, &gone);
    });
    Ok(token)
}

/// One `vault_placeholder` call, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlaceholderRequest {
    /// `NAME`, and the secret whose placeholder it is set to.
    pub(crate) env: Vec<(String, Handle)>,
    /// The audience's host, in its one spelling, and its port.
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) ttl: u32,
}

impl PlaceholderRequest {
    /// The audience, as the leases name it.
    pub(crate) fn audience(&self) -> String {
        origin(&self.host, self.port)
    }
}

/// `https://host[:port]`, an IPv6 address bracketed and port 443 left out:
/// an origin as `proxy` mode spells one ([`crate::vault_proxy::origin_of`]).
pub(crate) fn origin(host: &str, port: u16) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    if port == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{port}")
    }
}

/// Check a call's arguments: what is refused here never reaches the vault.
pub(crate) fn parse(args: &Value) -> Result<PlaceholderRequest, String> {
    let env = match args.get("env").and_then(Value::as_object) {
        Some(e) if !e.is_empty() && e.len() <= MAX_ENV => e,
        _ => {
            return Err(format!(
                "env must name 1 to {MAX_ENV} variables, each NAME: secret (such as \
                 GH_TOKEN: github/bot#token)"
            ));
        }
    };
    let mut pairs = Vec::with_capacity(env.len());
    for (name, raw) in env {
        if !env_name_ok(name) {
            return Err(format!(
                "{name:?} is not a variable a placeholder is set in: upper-case letters, digits \
                 and _, not starting with a digit, and none the shell or the loader reads \
                 (PATH, HOME, LD_*, …)"
            ));
        }
        let raw = raw
            .as_str()
            .ok_or_else(|| format!("{name}: name its secret as a string"))?;
        pairs.push((name.clone(), handle_of(raw)?));
    }
    let raw = args.get("audience").and_then(Value::as_str).ok_or(
        "audience is required: the origin the value goes to, as the secret's audiences name it \
         (such as https://api.github.com)",
    )?;
    let (host, port) =
        crate::vault_proxy::origin_of(raw).map_err(|e| e.replacen("upstream", "audience", 1))?;
    // The host the proxy matches a connection's against, in the one spelling
    // the proxy gives every host it is asked for.
    match canonical_net_host(&host) {
        Some(one) if one == host => {}
        Some(one) => {
            return Err(format!(
                "audience names its host in one spelling: write {}",
                origin(&one, port)
            ));
        }
        None => return Err(format!("audience {raw:?} names no host a connection dials")),
    }
    let ttl = match args.get("ttl_s") {
        None => DEFAULT_TTL,
        Some(v) => v
            .as_u64()
            .and_then(|t| u32::try_from(t).ok())
            .filter(|t| (MIN_TTL..=MAX_TTL).contains(t))
            .ok_or_else(|| {
                format!("ttl_s must be a whole number of seconds, {MIN_TTL} to {MAX_TTL}")
            })?,
    };
    Ok(PlaceholderRequest {
        env: pairs,
        host,
        port,
        ttl,
    })
}

/// A `vault_placeholder` call: each variable's lease in [`MODE`], its
/// placeholder bound to the run and the audience, and the assignments to set.
/// Every lease the call asked for itself is let go on any refusal.
///
/// Called by no tool until P3b-3c offers it: only the tests call it, so the
/// build without them keeps it, and what it calls, all the same.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn call(
    vault: &dyn VaultTools,
    session: ObjectId,
    fence: u64,
    args: &Value,
) -> Result<Value, String> {
    let req = parse(args)?;
    let run = (session, fence);
    let audience = req.audience();
    if held(session, fence) + req.env.len() > MAX_PER_RUN {
        return Err(format!(
            "this session would hold more than {MAX_PER_RUN} placeholders; one goes as its \
             lease ends"
        ));
    }
    let mut asked = Releasing {
        vault,
        leases: Vec::new(),
    };
    let mut leases = Vec::with_capacity(req.env.len());
    for (name, handle) in &req.env {
        let covers = Duration::from_secs(u64::from(MIN_TTL));
        let lease = match vault.ahead(session, fence, handle, MODE, &audience, covers) {
            Some(lease) => lease,
            None => {
                let lease = vault
                    .lease(session, fence, handle, MODE, &audience, req.ttl)
                    .await
                    .map_err(|why| format!("{name}: {why}"))?;
                asked.leases.push(lease.clone());
                lease
            }
        };
        leases.push((name, lease));
    }
    let mut env = Map::new();
    let mut made = Vec::with_capacity(leases.len());
    for (name, lease) in &leases {
        match bind(vault, run, lease, &req.host, req.port) {
            Ok(token) => {
                env.insert((*name).clone(), json!(token));
                made.push(token);
            }
            Err(why) => {
                for token in &made {
                    forget(run, token);
                }
                return Err(format!("{name}: {why}"));
            }
        }
    }
    // They hold the values the proxy swaps in: they last their TTL.
    asked.keep();
    let ttl_s = leases
        .iter()
        .filter_map(|(_, lease)| vault.left(lease))
        .min()
        .map_or(req.ttl, |left| {
            u32::try_from(left.as_secs()).unwrap_or(req.ttl)
        });
    Ok(json!({
        "env": env,
        "audience": audience,
        "ttl_s": ttl_s,
        "note": format!(
            "Set each in the environment of the command that needs it (NAME=placeholder before \
             the command, or exported in the session's shell). A placeholder is not the value: \
             this device's egress proxy swaps the value in on the way to {audience}, and nowhere \
             else, inside TLS it terminates for that host. Sent anywhere else, it goes out as \
             itself and carries nothing. It works for ttl_s seconds; ask again after."
        ),
    }))
}

#[cfg(test)]
mod tests;
