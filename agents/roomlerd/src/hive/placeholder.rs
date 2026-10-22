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
//! | the call | `env` (1 to 8 `NAME: handle`, each name one `vault_exec` would take), `audience` (an origin, `https://host[:port]`, its host in its one spelling, as the secret's audiences name it; or, P3b-3c-2, a list of up to [`MAX_AUDIENCES`], since `gh` sends one token to `https://api.github.com` and, through git, to `https://github.com`), `ttl_s` (30 to 900, default 300) |
//! | the leases | one per entry and audience, mode `placeholder`, toward that audience: the vault releases one only where `vault.placeholder` serves the organization and the secret names the audience (P3b-2). A lease asked for ahead toward the same audience is taken as it is |
//! | a placeholder | one per entry, whatever its audiences: `rph_` and 32 base32 characters, 160 bits from the OS CSPRNG, made here as its leases arrive, and known to nobody else |
//! | the answer | `env`: each name's placeholder; `audiences`; `ttl_s`, what its shortest lease has left; and a note |
//! | the run's placeholders | each bound once per audience, with that audience's lease, host and port: what the proxy swaps. A binding goes when its lease is let go (its expiry, `rc:vault.revoke`), and the others stay; all go when the run ends. At most [`MAX_PER_RUN`] bindings at once |
//!
//! ⚠️ P3b-3c-2: the toolbelt offers it ([`TOOL`]) only to a session whose
//! egress proxy is serving it, the proxy that swaps a placeholder in
//! (P3b-3c-1, `egress::swap`), and its settings ask a person on every call.

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
/// FR-90 P3b-3c-2 — the tool, as the toolbelt offers it.
pub const TOOL: &str = "vault_placeholder";
/// How long a placeholder's leases last when the call names no TTL.
const DEFAULT_TTL: u32 = 300;
pub(crate) const MIN_TTL: u32 = 30;
pub(crate) const MAX_TTL: u32 = 900;
/// The most variables one call sets.
pub(crate) const MAX_ENV: usize = 8;
/// FR-90 P3b-3c-2 — the most audiences one call names.
pub(crate) const MAX_AUDIENCES: usize = 4;
/// The most bindings (a placeholder toward an audience) a run holds at once.
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
    /// FR-90 P3b-4-2 — the secret and the field the lease is for: what pairs
    /// an AWS access key's placeholder with its secret key's (decision 33).
    pub secret: String,
    pub field: String,
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
/// what ends the run's, and of the proxy's swap.
#[cfg(test)]
pub(crate) fn bind_for_test(session: ObjectId, fence: u64, host: &str, port: u16) -> String {
    bind_as_for_test(session, fence, host, port, "test/secret", "value")
}

/// [`bind_for_test`], its secret and field named: for a test of what pairs
/// an AWS key's placeholders (P3b-4-2). Its lease is `test-lease-<token>`.
#[cfg(test)]
pub(crate) fn bind_as_for_test(
    session: ObjectId,
    fence: u64,
    host: &str,
    port: u16,
    secret: &str,
    field: &str,
) -> String {
    let token = mint().unwrap();
    runs().entry((session, fence)).or_default().push(Bound {
        token: token.clone(),
        lease_id: format!("test-lease-{token}"),
        host: host.to_string(),
        port,
        secret: secret.to_string(),
        field: field.to_string(),
    });
    token
}

/// `token`'s bindings gone from the run: every one, or the one with
/// `lease_id`.
fn forget(run: RunKey, token: &str, lease_id: Option<&str>) {
    let mut runs = runs();
    if let Some(all) = runs.get_mut(&run) {
        all.retain(|b| !(b.token == token && lease_id.is_none_or(|l| b.lease_id == l)));
        if all.is_empty() {
            runs.remove(&run);
        }
    }
}

/// `token` bound to the run toward the audience at `host:port`, with
/// `lease_id`, until that lease is let go: then this binding goes, and the
/// token's others stay. Refused where the run holds [`MAX_PER_RUN`]
/// bindings already, or the lease is no longer held.
fn bind(
    vault: &dyn VaultTools,
    run: RunKey,
    token: &str,
    lease_id: &str,
    (host, port): (&str, u16),
    handle: &Handle,
) -> Result<(), String> {
    {
        let mut runs = runs();
        let all = runs.entry(run).or_default();
        if all.len() >= MAX_PER_RUN {
            return Err(format!(
                "this session holds {MAX_PER_RUN} placeholders already; one goes as its lease ends"
            ));
        }
        all.push(Bound {
            token: token.to_string(),
            lease_id: lease_id.to_string(),
            host: host.to_string(),
            port,
            secret: handle.secret.clone(),
            field: handle.field.clone(),
        });
    }
    let stop = CancellationToken::new();
    if !vault.stop_with(lease_id, stop.clone()) {
        forget(run, token, Some(lease_id));
        return Err("its lease ended before its placeholder was made".into());
    }
    let (gone, lease) = (token.to_string(), lease_id.to_string());
    tokio::spawn(async move {
        stop.cancelled().await;
        forget(run, &gone, Some(&lease));
    });
    Ok(())
}

/// One `vault_placeholder` call, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlaceholderRequest {
    /// `NAME`, and the secret whose placeholder it is set to.
    pub(crate) env: Vec<(String, Handle)>,
    /// P3b-3c-2 — each audience's host, in its one spelling, and its port:
    /// one at least, each once.
    pub(crate) audiences: Vec<(String, u16)>,
    pub(crate) ttl: u32,
}

impl PlaceholderRequest {
    /// The audiences, as the leases name them.
    pub(crate) fn audiences(&self) -> Vec<String> {
        self.audiences
            .iter()
            .map(|(host, port)| origin(host, *port))
            .collect()
    }
}

/// P3b-3c-2 — an audience as a call or `vault_request` names it: an origin,
/// its host in the one spelling the proxy gives every host it is asked for.
/// Its host and port.
pub(crate) fn audience_of(raw: &str) -> Result<(String, u16), String> {
    let (host, port) =
        crate::vault_proxy::origin_of(raw).map_err(|e| e.replacen("upstream", "audience", 1))?;
    match canonical_net_host(&host) {
        Some(one) if one == host => Ok((host, port)),
        Some(one) => Err(format!(
            "audience names its host in one spelling: write {}",
            origin(&one, port)
        )),
        None => Err(format!("audience {raw:?} names no host a connection dials")),
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
    // P3b-3c-2 — one origin, or a list of them: each where the value goes.
    let required = || {
        format!(
            "audience is required: the origin the value goes to, as the secret's audiences name \
             it (such as https://api.github.com), or a list of 1 to {MAX_AUDIENCES} of them"
        )
    };
    let raws: Vec<&str> = match args.get("audience") {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(list)) if (1..=MAX_AUDIENCES).contains(&list.len()) => list
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| "each audience is a string".to_string())
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(required()),
    };
    let mut audiences: Vec<(String, u16)> = Vec::with_capacity(raws.len());
    for raw in raws {
        let audience = audience_of(raw)?;
        if audiences.contains(&audience) {
            return Err(format!(
                "audience names {} twice",
                origin(&audience.0, audience.1)
            ));
        }
        audiences.push(audience);
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
        audiences,
        ttl,
    })
}

/// A `vault_placeholder` call: each variable's lease in [`MODE`] toward each
/// audience, its one placeholder bound to the run and every audience, and
/// the assignments to set. Every lease the call asked for itself is let go,
/// and every binding it made taken back, on any refusal.
///
/// P3b-3c-2 — offered by the toolbelt ([`TOOL`]).
pub(crate) async fn call(
    vault: &dyn VaultTools,
    session: ObjectId,
    fence: u64,
    args: &Value,
) -> Result<Value, String> {
    let req = parse(args)?;
    let run = (session, fence);
    let audiences = req.audiences();
    if held(session, fence) + req.env.len() * audiences.len() > MAX_PER_RUN {
        return Err(format!(
            "this session would hold more than {MAX_PER_RUN} placeholders; one goes as its \
             lease ends"
        ));
    }
    let mut asked = Releasing {
        vault,
        leases: Vec::new(),
    };
    // Each variable's lease toward each audience, in the audiences' order.
    let mut leases: Vec<(&String, &Handle, Vec<String>)> = Vec::with_capacity(req.env.len());
    for (name, handle) in &req.env {
        let mut each = Vec::with_capacity(audiences.len());
        for audience in &audiences {
            let covers = Duration::from_secs(u64::from(MIN_TTL));
            let lease = match vault.ahead(session, fence, handle, MODE, audience, covers) {
                Some(lease) => lease,
                None => {
                    let lease = vault
                        .lease(session, fence, handle, MODE, audience, req.ttl)
                        .await
                        .map_err(|why| format!("{name}: {why}"))?;
                    asked.leases.push(lease.clone());
                    lease
                }
            };
            each.push(lease);
        }
        leases.push((name, handle, each));
    }
    let mut env = Map::new();
    let mut made: Vec<String> = Vec::with_capacity(leases.len());
    let unmake = |made: &[String]| {
        for token in made {
            forget(run, token, None);
        }
    };
    for (name, handle, each) in &leases {
        let token = match mint() {
            Ok(token) => token,
            Err(why) => {
                unmake(&made);
                return Err(why);
            }
        };
        made.push(token.clone());
        for (lease, (host, port)) in each.iter().zip(&req.audiences) {
            if let Err(why) = bind(vault, run, &token, lease, (host, *port), handle) {
                unmake(&made);
                return Err(format!("{name}: {why}"));
            }
        }
        env.insert((*name).clone(), json!(token));
    }
    // They hold the values the proxy swaps in: they last their TTL.
    asked.keep();
    let ttl_s = leases
        .iter()
        .flat_map(|(_, _, each)| each)
        .filter_map(|lease| vault.left(lease))
        .min()
        .map_or(req.ttl, |left| {
            u32::try_from(left.as_secs()).unwrap_or(req.ttl)
        });
    let toward = audiences.join(", ");
    Ok(json!({
        "env": env,
        "audiences": audiences,
        "ttl_s": ttl_s,
        "note": format!(
            "Set each in the environment of the command that needs it (NAME=placeholder before \
             the command, or exported in the session's shell). A placeholder is not the value: \
             this device's egress proxy swaps the value in on the way to {toward}, and nowhere \
             else, inside TLS it terminates for those hosts, and only in a request's \
             Authorization header (as a bearer token, or the password of basic authentication). \
             Sent anywhere else, or in a URL, another header or a body, it goes out as itself \
             and carries nothing. It works for ttl_s seconds; ask again after."
        ),
    }))
}

#[cfg(test)]
mod tests;
