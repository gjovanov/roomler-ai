# rc.53 v2 — WS close-reasons, agent-side fatal handling, corp-TLS hardening

> **Status**: GO. Folds in 15 required changes from the independent critique (`docs/plans/rc53-critique.md`) on top of v0 (`docs/plans/rc53-ws-close-reasons-and-corp-tls.md`).
>
> **Single tag**: `agent-v0.3.0-rc.53`. ~510 LOC, ~17 new tests, ~1.5 ED realistic. Phase 6 (wizard soft-delete revive UX) is deferred to rc.54.

## Changes from v0

A reader who already knows v0 can scan this list to see the deltas. Every change is mechanically traceable to a numbered item in `docs/plans/rc53-critique.md`.

| # | What changed | Critique item |
|---|---|---|
| 1 | Phase 2a target file: `crates/api/src/ws/remote_control.rs` ❌ → `crates/api/src/ws/handler.rs:172-201` ✅ (the `ws_upgrade_agent` `on_upgrade` closure). Mirror change at `handler.rs:108-140` (`ws_upgrade_tunnel_client`). | #0, #1 |
| 2 | Phase 2a flush mechanics: dropped the "≤200 ms" guard. Use `.await` on `send(Text(...))` then `.await` on `send(Close(Some(CloseFrame { code: 4003, reason: "agent_deleted" })))`. No `sleep`. | #2 |
| 3 | Phase 2b actually closes the displaced WS: add a `tokio::sync::Notify` to `ConnectedAgent`; the old `handle_agent_socket` `select!`s on `socket_rx.next()` **and** the cancel notify. Without this the displaced WS lingers up to one 25 s keepalive interval. | #3 |
| 4 | Phase 2b also fixes a pre-existing race: pass the registered tx identity to `unregister_agent` and only remove if it still matches (otherwise the displaced read-loop's late unregister evicts the NEW connection's entry). | #4 |
| 5 | Phase 3 drops the fictional `notify::set_attention(AttentionReason::ReEnrollRequired)` reference. The real API is `raise_attention(message: &str) -> Result<PathBuf>` in `agents/roomler-agent/src/notify.rs:39`. Mirror the existing auth-failure call-site at `agents/roomler-agent/src/signaling.rs:128-143`. Structured-reason refactor moves to rc.54. | #5 |
| 6 | Phase 3 explicitly calls `close_all_peers` + `close_all_tunnel_peers` BEFORE `std::process::exit(AGENT_DELETED_EXIT_CODE)`. Cited as a smoke-matrix invariant. | #6 |
| 7 | Phase 3 sentinel-path routing: write to `%PROGRAMDATA%\roomler\roomler-agent\needs-attention.txt` when running as LocalSystem (`WorkerRole::SystemContext`); fall back to `notify::attention_path()` (the existing `%APPDATA%` path) for user-context workers. Log the resolved path at WARN. | #7 |
| 8 | Phase 3 defines `AGENT_DELETED_EXIT_CODE: i32 = 7` in `agents/roomler-agent/src/watchdog.rs` next to `STALL_EXIT_CODE`. Updates `should_record_supervisor_crash` (`win_service/supervisor.rs:588`) to exclude code 7. Adds a code-7-specific alarm-immediately path (fire `error!` on first failure, not after 8). Updates rc.51 supervisor unit tests. | #8 |
| 9 | Phase 1 wire-format tests include an unknown-variant graceful-decode test: `{"t":"rc:goodbye","reason":"xyzzy","message":"x"}` must decode to `PolicyRejected` (sensible non-panicking default). | #9 |
| 10 | Phase 5 verification adds `cargo tree -e features -i webpki-roots`. Backlog gains a "unify TLS stack in rc.54" note (reqwest=native-tls, tungstenite=rustls, vendored-webrtc-ice=rustls — three TLS stacks in one binary today). | #10 |
| 11 | **Phase 6 DROPPED to rc.54** (`docs/plans/rc54-wizard-deleted-row-banner.md` stub mentioned in backlog). Auth model + hostname-uniqueness was genuinely under-spec'd. | #11 |
| 12 | **Phase 7 ADDED** (~15 LOC stderr warning in `cmd_enroll` at `agents/roomler-agent/src/main.rs:792-860`) for the same-session `%APPDATA% ↔ %PROGRAMDATA%` asymmetry — PC55331's current pain. Cheaper + higher-value than Phase 6. | #12 |
| 13 | Smoke matrix gains SM-1b (delete agent with active WebRTC session — assert controller `Terminate`, agent Goodbye + exit 7, no zombie peers). SM-3 swaps mitmproxy hint for mkcert + stunnel. SM-5 added (rc.53 server sends Goodbye mid-session to rc.52 agent — assert no regression). | #13 |
| 14 | Displacement log line at `crates/remote_control/src/hub.rs:108-111` updated to `"reconnected; notifying previous connection with ReplacedByNewerConnection and dropping"` to reflect new behaviour. | #14 |
| 15 | Phase 1 wire-format tests: golden-JSON per `AgentCloseReason` variant + unknown-variant graceful-decode test (item 9). | #15 |
| — | **Phase ordering** restructured to P1 → P4 → P3 → P2a → P2b → P7 → P5. Lets the implementer finish wire/back-compat first, prove agent behaviour offline, then turn on the server emit path with confidence. | HANDOVER24 |

## Problem this cycle solves

Two failure modes burned operator hours this week (PC55331 + the rc.48-era SystemContext loop):

1. **The server has a perfectly good reason for closing the WS but the agent never sees it.** Today the backend handler does one of:
   - logs `agent is quarantined or deleted; refusing WS` and `return`s (close the socket, no protocol message) — `crates/api/src/ws/handler.rs:189`;
   - logs `agent reconnected; dropping previous connection` inside `Hub::register_agent` (drops the old `tx`; the old pump task exits) — `crates/remote_control/src/hub.rs:108-111`;
   - tunnel-client mirror: `crates/api/src/ws/handler.rs:128`.

   The agent sees a raw socket close, wraps it as `ws read`, and reconnects forever on the rc.52 backoff ladder. From outside this is indistinguishable from a network flap. PC55331 ate half a day on duplicate-instance + corporate-proxy hypotheses before someone read the actual server log. **Fix: put the reason on the wire.**

2. **Corporate networks with SSL-inspection middleboxes.** rc.31/rc.32 fixed the TURN/ICE path for ÖBB-style hosts (`Symantec Enterprise Mobile Root` in the OS trust store), but the WS path (`tokio-tungstenite` with `rustls-tls-webpki-roots`, see `Cargo.toml:168`) still uses the Mozilla bundle ONLY. The day an inspecting proxy starts MITM'ing `wss://roomler.ai/ws`, the agent breaks with `UnknownIssuer` just like ICE did pre-rc.32. Cheap to fix now, expensive when it bites.

## Goals

- A misbehaving agent surfaces the *reason* in its own log — not just `ws read`.
- An agent whose server-side row was deleted/quarantined STOPS reconnecting and tells the operator what to do.
- An agent that loses a Hub duel STOPS reconnecting and tells the operator there's a duplicate instance to find.
- The WS path trusts whatever Windows trusts (matches rc.32 ICE + reqwest's native-tls), so corporate TLS-inspection works.
- The operator running `enroll --machine-global` from a user shell gets a clear stderr warning that `roomler-agent run` from the same shell will read `%APPDATA%` (different config, different `machine_id`).

Non-goals (explicit defers):

- HTTP CONNECT proxy support for WS (no fielded host needs it yet).
- Switching Hub policy from newest-wins to oldest-wins (no field data to justify it; Phases 2b + 3 break the duel either way).
- Wizard soft-delete-revive UX — moved to rc.54 (Phase 6 in v0; see backlog).
- `notify::AttentionReason` structured enum + JSON sidecar — moved to rc.54.

## Phase order (rationale)

The implementer should land phases in this order so each step is independently testable:

1. **Phase 1** — wire format (no behaviour change; pure type addition + serde tests).
2. **Phase 4** — back-compat (one test confirming the existing `Err(e) => debug!` decoder at `agents/roomler-agent/src/signaling.rs:333` swallows unknown variants).
3. **Phase 3** — agent-side fatal handling (offline-testable with synthesised messages before the server ever emits one).
4. **Phase 2a** — `crates/api/src/ws/handler.rs` server emit at the deleted/quarantined refusal sites.
5. **Phase 2b** — `crates/remote_control/src/hub.rs` displacement notify-then-close.
6. **Phase 7** — `cmd_enroll` stderr warning (isolated 15 LOC, no protocol involvement).
7. **Phase 5** — `tokio-tungstenite` native cert roots feature swap + `cargo tree` verification.

---

## Phase 1 — `ServerMsg::Goodbye` wire variant + close-reason enum (~0.4 d, low risk)

**Files**: `crates/remote_control/src/signaling.rs`

### Type addition

Add at the top of the existing supporting-types block (sits well next to `CloseReason` / `RejectKind` at `signaling.rs:46-98`):

```rust
/// Server-initiated close reason for an agent WS connection. Carried
/// by `ServerMsg::Goodbye`. Distinct from the session-level
/// `EndReason` (which terminates one session) and from `CloseReason`
/// (which terminates one tunnel flow) — this is connection-level.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentCloseReason {
    /// Server-side `agents` row has `deleted_at != null` or is
    /// otherwise refused by the WS handler's lookup. The agent's
    /// stored token is cryptographically valid but useless. Re-enrol
    /// to revive (soft-deleted rows rehydrate on (tenant_id,
    /// machine_id) match — `Hub::register_agent` calls `rehydrate()`).
    AgentDeleted,
    /// A newer WS connection presented the SAME `agent_id`; the Hub
    /// kept the new one, dropped this old one. Indicates a duplicate
    /// install somewhere (another physical host with a copy of this
    /// `config.toml`, the tray companion, etc.).
    ReplacedByNewerConnection,
    /// Server-side policy refused (account suspended, tenant
    /// disabled, version too old). Reserved for future use; also the
    /// default the decoder picks for unknown-string variants so
    /// future rc.54+ variants don't hard-fault rc.53 agents in the
    /// field.
    PolicyRejected,
}
```

Add the new `ServerMsg` variant near the end of the existing enum (so test diffs are minimal):

```rust
/// Server-initiated close of an agent WS. Sent immediately before
/// the WS Close frame so the agent can surface a useful reason in
/// its log + decide whether to reconnect or stop.
#[serde(rename = "rc:goodbye")]
Goodbye {
    reason: AgentCloseReason,
    /// Human-readable, operator-targeted. Used verbatim in the
    /// agent's needs-attention sentinel + `tracing::error!` line.
    message: String,
},
```

### Tests (in `signaling::tests`)

Mirror the existing pattern (`tcp_closed_reason_roundtrip`, `tunnel_terminate_uses_close_reason`):

```rust
#[test]
fn agent_close_reason_serialises_snake_case() {
    for (variant, expected) in [
        (AgentCloseReason::AgentDeleted, "agent_deleted"),
        (AgentCloseReason::ReplacedByNewerConnection, "replaced_by_newer_connection"),
        (AgentCloseReason::PolicyRejected, "policy_rejected"),
    ] {
        let m = ServerMsg::Goodbye { reason: variant, message: "x".into() };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(&format!("\"reason\":\"{expected}\"")));
        assert!(s.contains(r#""t":"rc:goodbye""#));
    }
}

#[test]
fn goodbye_round_trips() {
    let m = ServerMsg::Goodbye {
        reason: AgentCloseReason::AgentDeleted,
        message: "re-enrol required".into(),
    };
    let s = serde_json::to_string(&m).unwrap();
    let back: ServerMsg = serde_json::from_str(&s).unwrap();
    assert!(matches!(back, ServerMsg::Goodbye {
        reason: AgentCloseReason::AgentDeleted, ..
    }));
}

#[test]
fn goodbye_with_unknown_reason_decodes_to_policy_rejected_default() {
    // Defensive: forward-compat with rc.54+ variants. If a fielded
    // rc.53 agent sees an `unknown_reason` it must NOT panic /
    // hard-fault; the decoder must round it to a sensible default.
    // Implementation hook: serde `#[serde(other)]` on the enum or
    // a custom deserialise that maps unknown to PolicyRejected.
    let json = r#"{"t":"rc:goodbye","reason":"xyzzy","message":"x"}"#;
    let back: ServerMsg = serde_json::from_str(json).unwrap();
    match back {
        ServerMsg::Goodbye { reason, .. } => {
            assert_eq!(reason, AgentCloseReason::PolicyRejected);
        }
        other => panic!("expected Goodbye, got {other:?}"),
    }
}
```

> Implementation hint for the unknown-variant test: serde supports `#[serde(other)]` ONLY on unit-variant fieldless enums when used with `untagged` or as the discriminator. Cleanest path: keep the enum as-is and write a custom `Deserialize` for `AgentCloseReason` that matches the three known strings + falls back to `PolicyRejected` for anything else. Implementer's call.

LOC budget: ~90 (type ~30, variant ~10, tests ~50).

---

## Phase 4 — back-compat smoke (~0.15 d, low risk)

**Files**: `crates/remote_control/src/signaling.rs` (test only), `agents/roomler-agent/src/signaling.rs` (one defensive assertion).

The existing client-side decoder at `agents/roomler-agent/src/signaling.rs:333` is:

```rust
Err(e) => debug!(%e, text = %text.as_str(), "ignoring non-rc:* frame"),
```

A pre-rc.53 agent receiving the new `ServerMsg::Goodbye` discriminator hits this `Err` arm and ignores the message. No `#[serde(deny_unknown_fields)]` on `ServerMsg`. **The plan's v0 auto-fail condition #4 is satisfied today.**

### Tests

Add one test asserting that pre-rc.53 agents stay quiet:

```rust
#[test]
fn pre_rc53_agent_silently_ignores_goodbye_via_err_path() {
    // Synthesise a Goodbye JSON and feed it through serde_json::from_str::<OldServerMsg>
    // where OldServerMsg is a stripped enum without the Goodbye variant.
    // (In-test mock enum is fine — the real assertion is that the production
    // agent's `Err(e) => debug!` branch absorbs anything serde can't decode.)
}
```

Also add a positive test that a synthesised raw `Message::Close` (no Goodbye preceding) is still classified `Transient` by the agent's connect-once loop — defends against creeping "if I don't see a Goodbye assume server is old" heuristics (critique item 5 / Phase 5 confirmation):

```rust
// In agents/roomler-agent/src/signaling.rs tests:
// Drive handle_server_msg with the existing `Some(Ok(Message::Close(_)))` path,
// assert connect_once returns Ok(()) (which the outer loop already treats as
// "transient, reconnect") and does NOT call raise_attention.
```

LOC budget: ~30 (tests only).

---

## Phase 3 — agent treats `Goodbye` as fatal (~0.6 d, medium risk)

**Files**:
- `agents/roomler-agent/src/signaling.rs` (`handle_server_msg` at `:357`, `connect_once` exit paths at `:269-355`)
- `agents/roomler-agent/src/notify.rs` (add LocalSystem-aware path resolver)
- `agents/roomler-agent/src/watchdog.rs` (add `AGENT_DELETED_EXIT_CODE`)
- `agents/roomler-agent/src/win_service/supervisor.rs` (update `should_record_supervisor_crash` + add code-7 alarm-immediately path)

### Step 3.1 — Define the exit code

In `agents/roomler-agent/src/watchdog.rs`, after the existing `STALL_EXIT_CODE` constant (`watchdog.rs:60`):

```rust
/// Sentinel exit code reserved for "server-side Goodbye said this
/// agent's row is deleted / policy-rejected — operator action
/// required, do not respawn-spam." rc.51 supervisor treats this
/// distinctly from a generic non-zero exit: alarm fires on the FIRST
/// failure (not after 8), so the operator-action signal is visible
/// in <1 minute. `should_record_supervisor_crash` excludes this code
/// because the agent already raised an attention sentinel
/// (double-recording inflates fleet crash metrics — same rationale
/// as the STALL_EXIT_CODE exclusion).
pub const AGENT_DELETED_EXIT_CODE: i32 = 7;
```

### Step 3.2 — Supervisor changes

In `agents/roomler-agent/src/win_service/supervisor.rs`:

a. Update `should_record_supervisor_crash` at `:588`:

```rust
pub fn should_record_supervisor_crash(code: u32) -> bool {
    code != 0
        && code != crate::watchdog::STALL_EXIT_CODE as u32
        && code != crate::watchdog::AGENT_DELETED_EXIT_CODE as u32
}
```

b. In the respawn-alarm block at `:817-831`, add a code-7 fast-path BEFORE the existing `consecutive_failures >= RESPAWN_ALARM_THRESHOLD` gate:

```rust
let is_agent_deleted_exit = code == crate::watchdog::AGENT_DELETED_EXIT_CODE as u32;
let alarm_due = is_agent_deleted_exit
    || consecutive_failures >= RESPAWN_ALARM_THRESHOLD;
if alarm_due {
    let now = Instant::now();
    let throttle_passed = last_alarm_at.is_none_or(|t| {
        now.duration_since(t) >= Duration::from_secs(60)
    });
    if throttle_passed {
        last_alarm_at = Some(now);
        if is_agent_deleted_exit {
            tracing::error!(
                last_exit_code = code,
                "supervisor: worker exited with AGENT_DELETED_EXIT_CODE — server-side row was deleted or policy-rejected; operator action required (re-enrol with fresh token). Supervisor will keep respawning; expect successive code-7 exits until re-enrollment."
            );
        } else {
            tracing::error!(
                consecutive_failures,
                last_exit_code = code,
                "supervisor: worker has failed {} times in a row — host likely needs operator attention (still respawning)",
                consecutive_failures
            );
        }
    }
}
```

c. Unit test update for `should_record_supervisor_crash`:

```rust
#[test]
fn should_record_supervisor_crash_excludes_agent_deleted_and_stall() {
    assert!(!should_record_supervisor_crash(0));
    assert!(!should_record_supervisor_crash(crate::watchdog::STALL_EXIT_CODE as u32));
    assert!(!should_record_supervisor_crash(crate::watchdog::AGENT_DELETED_EXIT_CODE as u32));
    assert!(should_record_supervisor_crash(1));
    assert!(should_record_supervisor_crash(42));
}
```

### Step 3.3 — LocalSystem-aware sentinel path

In `agents/roomler-agent/src/notify.rs`, add a sibling to `attention_path()`:

```rust
/// Resolve the attention sentinel path with awareness of the
/// caller's worker context. When the current process is the
/// LocalSystem SCM worker, `%APPDATA%` resolves to
/// `C:\Windows\System32\config\systemprofile\AppData\Roaming\…` —
/// invisible to a human operator. Prefer `%PROGRAMDATA%\roomler\
/// roomler-agent\needs-attention.txt` in that case so a fleet-mgmt
/// scanner finds it AND a logged-in operator can `dir %PROGRAMDATA%`.
///
/// Returns `(path, was_machine_global)` so the caller can log the
/// resolved location for debuggability.
#[cfg(target_os = "windows")]
pub fn attention_path_for_worker() -> Option<(PathBuf, bool)> {
    use crate::system_context::worker_role::{probe_self, WorkerRole};
    if let Ok(WorkerRole::SystemContext) = probe_self() {
        // C:\ProgramData\roomler\roomler-agent\needs-attention.txt
        let pd = std::env::var_os("PROGRAMDATA")?;
        let path = PathBuf::from(pd)
            .join("roomler")
            .join("roomler-agent")
            .join(ATTENTION_FILENAME);
        return Some((path, true));
    }
    attention_path().map(|p| (p, false))
}

#[cfg(not(target_os = "windows"))]
pub fn attention_path_for_worker() -> Option<(PathBuf, bool)> {
    attention_path().map(|p| (p, false))
}

/// Variant of [`raise_attention`] that routes to `%PROGRAMDATA%`
/// when running as LocalSystem. Logs the resolved path at WARN so
/// the operator can find the file.
pub fn raise_attention_machine_aware(message: &str) -> Result<PathBuf> {
    let (path, machine_global) = attention_path_for_worker()
        .context("no attention path resolvable")?;
    let parent = path.parent().context("attention path has no parent")?;
    let written = raise_attention_at(parent, message)?;
    tracing::warn!(
        path = %written.display(),
        machine_global,
        "raised needs-attention sentinel"
    );
    Ok(written)
}
```

Add tests:

```rust
#[cfg(target_os = "windows")]
#[test]
fn attention_path_for_worker_returns_programdata_when_system_context() {
    // gate this with cfg + a serial_test guard if needed; LocalSystem
    // detection is environment-dependent. At minimum assert the function
    // is callable + does not panic, and that when PROGRAMDATA env var
    // is set the user-context branch returns a path under %APPDATA%.
    let _ = attention_path_for_worker();
}
```

### Step 3.4 — `handle_server_msg` arm + connect-once routing

Extend `ConnectError` (`agents/roomler-agent/src/signaling.rs:182-188`) so the connect-once loop can distinguish fatal from transient:

```rust
use crate::signaling::AgentCloseReason;  // re-exported from remote_control crate

#[derive(Debug, thiserror::Error)]
enum ConnectError {
    #[error("auth rejected")]
    AuthRejected,
    #[error("fatal goodbye: {reason:?}")]
    FatalGoodbye {
        reason: AgentCloseReason,
        message: String,
    },
    #[error("replaced by newer connection")]
    ReplacedByNewer { message: String },
    #[error(transparent)]
    Transient(#[from] anyhow::Error),
}
```

Add the `handle_server_msg` arm (alongside the existing `ServerMsg::Request {...}` arm at `:373`):

```rust
ServerMsg::Goodbye { reason, message } => {
    tracing::error!(
        ?reason,
        %message,
        "server-side rc:goodbye received — stopping reconnect attempts"
    );
    match reason {
        AgentCloseReason::AgentDeleted | AgentCloseReason::PolicyRejected => {
            return Err(ConnectError::FatalGoodbye { reason, message });
        }
        AgentCloseReason::ReplacedByNewerConnection => {
            return Err(ConnectError::ReplacedByNewer { message });
        }
    }
}
```

Update the outer `run()` loop (`:88-162`) to handle the new error variants:

```rust
// Track recent replacements to escalate dueling duplicates.
let mut recent_replacements: Vec<std::time::Instant> = Vec::new();

loop {
    // ... existing shutdown check ...
    match connect_once(...).await {
        Ok(()) => { /* existing */ }
        Err(ConnectError::AuthRejected) => { /* existing */ }

        Err(ConnectError::FatalGoodbye { reason, message }) => {
            // CRITICAL TEARDOWN INVARIANT (critique #6): peers + tunnel_peers
            // are owned inside `connect_once`. They are already cleaned up
            // via `close_all_peers` + `close_all_tunnel_peers` on every
            // connect_once exit path (see :274, :283, :301, :342, :347).
            // No extra teardown here.
            let body = format!(
                "Roomler agent: server-side close — {reason:?}.\n\n{message}\n\n\
                 The agent will not reconnect. Re-enrol with a fresh enrollment \
                 JWT from the admin UI:\n\n\
                 \troomler-agent re-enroll --token <new-jwt>\n\n\
                 then restart the service (or wait for the supervisor to relaunch).");
            match notify::raise_attention_machine_aware(&body) {
                Ok(path) => warn!(path = %path.display(), "wrote needs-attention sentinel for FatalGoodbye"),
                Err(e) => warn!(error = %e, "failed to write needs-attention sentinel for FatalGoodbye"),
            }
            // Exit with the documented code so the supervisor's code-7
            // fast-alarm fires immediately (operator-action signal in <1 min).
            std::process::exit(watchdog::AGENT_DELETED_EXIT_CODE);
        }

        Err(ConnectError::ReplacedByNewer { message }) => {
            let now = std::time::Instant::now();
            recent_replacements.retain(|t| now.duration_since(*t) < Duration::from_secs(5 * 60));
            recent_replacements.push(now);
            warn!(%message, count = recent_replacements.len(),
                  "server signalled this connection was replaced; backing off 60 s");

            if recent_replacements.len() >= 3 {
                let body = format!(
                    "Roomler agent: duplicate-instance duel detected.\n\n{message}\n\n\
                     This connection has been displaced {} times in the last 5 minutes — \
                     another process (different physical host with a copy of this \
                     config.toml, or a tray companion) is using the same agent_id. \
                     Stop the duplicate or re-enrol THIS host with a fresh enrollment \
                     JWT to mint a new agent_id.",
                    recent_replacements.len()
                );
                match notify::raise_attention_machine_aware(&body) {
                    Ok(path) => warn!(path = %path.display(), "wrote needs-attention sentinel for ReplacedByNewer escalation"),
                    Err(e) => warn!(error = %e, "failed to write needs-attention sentinel for ReplacedByNewer escalation"),
                }
                std::process::exit(watchdog::AGENT_DELETED_EXIT_CODE);
            }

            // Back off 60 s minimum — long enough that two duelling
            // instances stagger out of phase and one wins.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {},
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return Ok(()); }
                },
            }
            backoff = Duration::from_secs(1);
        }

        Err(ConnectError::Transient(e)) => { /* existing */ }
    }
}
```

Note on teardown invariant: `connect_once`'s exit paths (`agents/roomler-agent/src/signaling.rs:274, 283, 301, 342, 347`) already call `close_all_peers` + `close_all_tunnel_peers`. The new `handle_server_msg` arm returns `Err(ConnectError::FatalGoodbye { ... })` which flows to the same `?` exit at `:331`, which is BEFORE those cleanup sites — so we must either (a) move the cleanup BEFORE the early-return from `handle_server_msg`, or (b) call it explicitly in the new arm. **Pick (a)**: have the new arm do `close_all_peers(peers, indicator).await; close_all_tunnel_peers(tunnel_peers).await;` BEFORE returning Err. Reference these calls explicitly to satisfy SM-1b's "no zombie peers" assertion.

### Tests (in `agents/roomler-agent/src/signaling.rs::tests`)

```rust
#[test]
fn agent_close_reason_agent_deleted_produces_fatal_goodbye_error() {
    // Construct a ServerMsg::Goodbye { AgentDeleted, "..." }, drive
    // through handle_server_msg with dummied-out peer maps,
    // assert: returns ConnectError::FatalGoodbye + raise_attention_machine_aware
    // was called (use a tempdir-backed `notify::raise_attention_at` mock).
}

#[test]
fn agent_close_reason_replaced_by_newer_returns_replaced_variant() { ... }

#[test]
fn three_replacements_in_5_min_escalates_to_fatal() {
    // Drive run() with synthesised Goodbyes 3× within 5 min, assert
    // raise_attention_machine_aware was called + std::process::exit
    // was reached (intercept via a test-only exit hook).
}

#[test]
fn raw_close_without_goodbye_stays_transient() {
    // Synthesise Some(Ok(Message::Close(_))) → outer loop sees Ok(()),
    // reconnect backoff resets to 1 s, no attention sentinel written.
}
```

LOC budget: ~170 (handle_server_msg arm ~25, ConnectError variants ~10, run() loop changes ~70, notify helpers ~50, supervisor changes ~25, tests separate ~100).

---

## Phase 2a — server emits Goodbye at WS-handler refusal sites (~0.3 d, low risk)

**Files**: `crates/api/src/ws/handler.rs`

### 2a.1 — Agent refusal site

In `ws_upgrade_agent`'s `on_upgrade` closure at `crates/api/src/ws/handler.rs:172-201`, replace the current `info!(...); return;` at `:189-190` with:

```rust
if agent.deleted_at.is_some()
    || matches!(
        agent.status,
        roomler_ai_remote_control::models::AgentStatus::Quarantined
    )
{
    info!(%agent_id, "agent is quarantined or deleted; refusing WS with rc:goodbye");
    let goodbye = roomler_ai_remote_control::signaling::ServerMsg::Goodbye {
        reason: roomler_ai_remote_control::signaling::AgentCloseReason::AgentDeleted,
        message: "This agent's server-side row was deleted (or quarantined). \
                  Re-enrol with a fresh enrollment token from the admin UI \
                  to revive (soft-deleted rows rehydrate by (tenant_id, machine_id)).".into(),
    };
    send_goodbye_and_close(socket, &goodbye, 4003, "agent_deleted").await;
    return;
}
```

Add the helper at the bottom of the file (near `handle_socket`):

```rust
/// Push a `ServerMsg::Goodbye` text frame + a Close frame to a raw
/// WebSocket and await both. Tungstenite serialises both frames into
/// the same TCP buffer; the OS delivers them in order. No `sleep`
/// guard needed — the `.await` on `send(Close)` flushes via
/// tungstenite's internal sink.
///
/// Used at WS-refusal sites (`ws_upgrade_agent` /
/// `ws_upgrade_tunnel_client`) where we want the agent to learn
/// WHY the connection is being closed before the socket drops.
async fn send_goodbye_and_close<M: serde::Serialize>(
    socket: axum::extract::ws::WebSocket,
    msg: &M,
    close_code: u16,
    close_reason: &str,
) {
    use axum::extract::ws::{CloseFrame, Message};
    use futures::SinkExt;
    let mut socket = socket;
    let json = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            warn!(%e, "failed to serialise Goodbye; closing without it");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    if let Err(e) = socket.send(Message::Text(json.into())).await {
        warn!(%e, "Goodbye text-send failed; attempting close anyway");
    }
    if let Err(e) = socket.send(Message::Close(Some(CloseFrame {
        code: close_code,
        reason: close_reason.into(),
    }))).await {
        debug!(%e, "Goodbye close-send failed (socket may already be dropped)");
    }
    // socket drops here; tungstenite serialises both frames into the
    // outbound TCP buffer before returning from the close-send .await.
}
```

### 2a.2 — Tunnel-client refusal site

Mirror at `crates/api/src/ws/handler.rs:108-140` (`ws_upgrade_tunnel_client`). The existing pattern already has `ServerMsg::TunnelRevoked` so it's a natural extension — use `TunnelRevoked` for tunnel-clients (NOT `Goodbye`; the tunnel-client wire vocabulary already has its own taxonomy):

```rust
if client.deleted_at.is_some()
    || matches!(
        client.status,
        roomler_ai_remote_control::models::AgentStatus::Quarantined
    )
{
    info!(%tunnel_client_id, "tunnel-client is quarantined or deleted; refusing WS with rc:tunnel.revoked");
    let revoked = roomler_ai_remote_control::signaling::ServerMsg::TunnelRevoked {
        reason: "tunnel-client row was deleted or quarantined; re-enrol to revive".into(),
    };
    send_goodbye_and_close(socket, &revoked, 4003, "tunnel_client_deleted").await;
    return;
}
```

### Tests (in `crates/api/src/ws/handler.rs::tests` or `crates/tests/`)

```rust
#[tokio::test]
async fn ws_upgrade_agent_emits_goodbye_then_close_for_deleted_row() {
    // Spawn a TestApp, soft-delete an agent row, open `/ws?role=agent&token=...`,
    // assert the first WS frame is a text frame with `"t":"rc:goodbye"` +
    // `"reason":"agent_deleted"`, second frame is Close(4003).
}

#[tokio::test]
async fn ws_upgrade_tunnel_client_emits_revoked_then_close_for_deleted_row() { ... }
```

LOC budget: ~70 (helper ~30, two call-site edits ~20, tests ~20 in-test-app shell — most of the cost is test scaffolding which the existing tests already provide).

---

## Phase 2b — Hub displacement: notify + cleanly close old WS (~0.5 d, medium risk)

**Files**: `crates/remote_control/src/hub.rs`, `crates/api/src/ws/remote_control.rs` (read-loop select)

### 2b.1 — Add cancel-notify to `ConnectedAgent`

In `crates/remote_control/src/hub.rs:33-41`:

```rust
pub struct ConnectedAgent {
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub owner_user_id: ObjectId,
    pub os: OsKind,
    pub tx: ClientTx,
    pub active_sessions: u8,
    pub max_sessions: u8,
    /// Signalled by `register_agent` when a newer connection displaces
    /// this one. The handle_agent_socket read-loop should
    /// `select!` on `socket_rx.next()` AND `notify.notified()` and
    /// exit cleanly when it fires.
    pub cancel: Arc<tokio::sync::Notify>,
}
```

### 2b.2 — `register_agent` notify-then-close

Update `crates/remote_control/src/hub.rs:87-116`:

```rust
pub fn register_agent(...) -> (Arc<tokio::sync::Notify>, mpsc::Receiver<ServerMsg>) {
    let (tx, rx) = mpsc::channel(SERVER_TX_CAPACITY);
    let cancel = Arc::new(tokio::sync::Notify::new());
    let entry = ConnectedAgent {
        agent_id, tenant_id, owner_user_id, os, tx: tx.clone(),
        active_sessions: 0, max_sessions,
        cancel: cancel.clone(),
    };
    if let Some(prev) = self.inner.agents.insert(agent_id, entry) {
        // critique #14: log line updated.
        warn!(
            "agent {} reconnected; notifying previous connection with ReplacedByNewerConnection and dropping",
            agent_id
        );
        let goodbye = ServerMsg::Goodbye {
            reason: AgentCloseReason::ReplacedByNewerConnection,
            message: format!(
                "Another agent is connecting with the same agent_id ({}); \
                 this connection is being closed. Check for a duplicate install \
                 (another physical host with a copy of this config.toml, the tray \
                 companion, etc.) or re-enrol to mint a fresh agent_id.",
                agent_id
            ),
        };
        match prev.tx.try_send(goodbye) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // critique #3 (sub-bullet): SERVER_TX_CAPACITY=64 may be near-full
                // under contention. Operator knows the displaced agent likely missed
                // the message.
                warn!("agent {} displacement goodbye dropped (channel full); displaced agent will see raw close only", agent_id);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // The displaced agent's pump task already exited; the socket
                // close path will fire on next read.
            }
        }
        // Signal the read-loop to exit (critique #3): drop(prev) alone
        // would let the read-loop poll socket_rx.next() until the agent's
        // own 25 s keepalive triggers a send error.
        prev.cancel.notify_waiters();
        drop(prev);
    }
    info!("agent {} online", agent_id);
    (cancel, rx)
}
```

### 2b.3 — `handle_agent_socket` selects on cancel notify

In `crates/api/src/ws/remote_control.rs` (the read-loop body), wrap the existing `socket_rx.next()` await in a `tokio::select!`:

```rust
loop {
    tokio::select! {
        biased; // poll cancel first so a displacement wins races with a stale message
        _ = cancel.notified() => {
            info!(%agent_id, "agent connection cancelled by Hub (replaced by newer); exiting read-loop");
            break;
        }
        msg = socket_rx.next() => {
            match msg {
                // ... existing arms ...
            }
        }
    }
}
```

`cancel` is the `Arc<Notify>` returned from `register_agent` — thread it through `handle_agent_socket`'s signature.

### 2b.4 — Fix pre-existing `unregister_agent` race (critique #4)

Today `Hub::unregister_agent(agent_id)` (`crates/remote_control/src/hub.rs:118-135`) unconditionally removes the registry entry. After Phase 2b, the displaced `handle_agent_socket` eventually exits its read-loop and calls `unregister_agent` — by then the registry entry is the NEW connection's, and the existing code would evict it (terminating in-flight sessions via `EndReason::AgentDisconnect`). Fix:

```rust
pub fn unregister_agent(&self, agent_id: ObjectId, tx: &ClientTx) {
    // Only remove if the registered tx is still ours. Prevents the
    // displaced connection's late unregister from evicting the NEW
    // connection's entry (which Phase 2b creates with the
    // displacement-notify path).
    let should_remove = self.inner.agents
        .get(&agent_id)
        .map(|a| ptr_eq(&a.tx, tx))
        .unwrap_or(false);
    if !should_remove {
        return;
    }
    if self.inner.agents.remove(&agent_id).is_some() {
        info!("agent {} offline", agent_id);
        // ... existing session-termination loop ...
    }
}
```

Callers of `unregister_agent` must thread the `tx` they captured at registration time. **Known caller sites** (verified by grep):

1. `crates/api/src/ws/remote_control.rs` — `handle_agent_socket` cleanup. Has the `tx` from registration; thread it through.
2. `crates/api/src/routes/remote_control.rs:255` — admin-driven kick path. **No `tx` available here** (admin force-evicts by `agent_id`). Options:
   - (a) keep an `unregister_agent(agent_id)` siblign with no tx-match (`pub fn force_unregister_agent(&self, agent_id: ObjectId)`) for the admin path, OR
   - (b) accept `Option<&ClientTx>` on the single function and treat `None` as "always remove" (admin override).
   Pick (b) — fewer signatures. Document the override in a doc-comment so future readers see why the tx-match check is gated.

### Tests (in `crates/remote_control/src/hub.rs::tests`)

```rust
#[tokio::test]
async fn displacement_sends_goodbye_then_notifies_cancel() {
    let hub = test_hub().await;
    let agent_id = ObjectId::new();
    let (cancel1, mut rx1) = hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);
    let (_cancel2, _rx2) = hub.register_agent(agent_id, ObjectId::new(), ObjectId::new(), OsKind::Linux, 3);

    // First connection should see a Goodbye on rx1...
    let msg = rx1.try_recv().expect("displaced connection should receive Goodbye");
    assert!(matches!(msg, ServerMsg::Goodbye { reason: AgentCloseReason::ReplacedByNewerConnection, .. }));

    // ...and cancel1 should be notified.
    // Use timeout(50ms, cancel1.notified()) — if it doesn't resolve, the notify wasn't fired.
    tokio::time::timeout(Duration::from_millis(50), cancel1.notified())
        .await
        .expect("cancel1 should have been notified by displacement");
}

#[tokio::test]
async fn unregister_agent_with_stale_tx_is_noop() {
    // Register tx1, displace with tx2, call unregister_agent(id, &tx1).
    // Assert hub.is_agent_online(id) still returns true (tx2's entry survived).
}

#[tokio::test]
async fn displacement_goodbye_dropped_when_channel_full_logs_warn() {
    // Fill the prev.tx channel to capacity, displace, assert warn-log
    // line "displacement goodbye dropped". Use a tracing_test::traced_test
    // to capture log output.
}
```

LOC budget: ~120 (hub changes ~60, handle_agent_socket select-wrap ~20, unregister_agent fix ~10, tests ~30).

---

## Phase 7 — `cmd_enroll` stderr warning for `%APPDATA% / %PROGRAMDATA%` asymmetry (~0.1 d, low risk)

**Files**: `agents/roomler-agent/src/main.rs`

In `enroll_cmd` at `agents/roomler-agent/src/main.rs:792-860`, after the existing `tracing::info!(%machine_id, machine_global, "derived machine fingerprint");` at `:821`, add (Windows-only):

```rust
#[cfg(target_os = "windows")]
if machine_global {
    use crate::system_context::worker_role::{probe_self, WorkerRole};
    let is_local_system = matches!(probe_self(), Ok(WorkerRole::SystemContext));
    if !is_local_system {
        eprintln!();
        eprintln!("NOTE: --machine-global wrote config to %PROGRAMDATA%, which is read by");
        eprintln!("      the LocalSystem service worker. A `roomler-agent run` from THIS user");
        eprintln!("      shell will instead read %APPDATA% (a separate config, different");
        eprintln!("      machine_id) and will look like a different host to the server.");
        eprintln!();
        eprintln!("      Either:");
        eprintln!("       (a) start the service: `sc start roomler-agent`  — uses %PROGRAMDATA%;");
        eprintln!("       (b) re-run `enroll` without --machine-global if you want to test in");
        eprintln!("           THIS user shell (will produce a different agent_id).");
        eprintln!();
    }
}
```

### Tests

```rust
#[cfg(target_os = "windows")]
#[test]
fn enroll_warning_message_contains_expected_lines() {
    // Build a String via the same eprintln body (extract to a helper
    // `warning_message_for_user_context_enroll() -> String`) and assert
    // it contains the marker phrases: "sc start roomler-agent",
    // "%APPDATA%", "without --machine-global".
}
```

LOC budget: ~25.

---

## Phase 5 — `tokio-tungstenite` native cert roots (~0.25 d + lock churn)

**Files**: `Cargo.toml`

Edit `Cargo.toml:168`:

```diff
-tokio-tungstenite = { version = "0.28", features = ["rustls-tls-webpki-roots"] }
+tokio-tungstenite = { version = "0.28", features = ["rustls-tls-native-roots"] }
```

Same swap rc.32 did for the vendored `webrtc-ice`. On hosts with no corporate inspection (the majority), webpki-roots-only would work too — but native-roots additionally trusts whatever the OS trusts (incl. the corporate CA from a `Symantec Enterprise Mobile Root` etc.). Zero behaviour change on plain-internet hosts.

### Verification

```bash
cargo check --workspace
cargo tree -e features -i webpki-roots   # critique #10: must show no consumers, or only intentional ones
cargo build -p roomler-agent --release --features full
# manual smoke on a normal-internet box: roomler-agent run reaches the server as before
# manual smoke on corp-MITM box: see SM-3 below
```

### Backlog note

Today the agent binary carries THREE distinct TLS stacks:

1. `reqwest` → `default-tls` = `native-tls` → Schannel on Windows (enrollment, file uploads)
2. `tokio-tungstenite` → rustls + webpki-roots (WS signaling)
3. vendored `webrtc-ice` → rustls + native roots (ICE TURNS/TCP, rc.32)

Phase 5 makes #2 align with #3 (rustls + native roots) but #1 stays divergent. **rc.54 backlog**: unify all three to a single stack. Either go all-rustls (`reqwest` with `default-features=false, rustls-tls-native-roots`) or all-native-tls. One cert-chain bug surface per stack × per OS is the status quo cost.

LOC budget: ~3 (one feature flip + the backlog note in this plan).

---

## Phase 6 — DROPPED, deferred to rc.54

The wizard-side soft-delete-revive UX is genuinely under-spec'd (critique #11) — the `/api/agent/enroll/probe` endpoint, the hostname-uniqueness handling, and the wizard SPA banner all need ~150 + ~80 LOC and proper tests, not the v0 "~80 + ~30 LOC" estimate.

Placeholder for rc.54: `docs/plans/rc54-wizard-deleted-row-banner.md` (to be written when rc.53 ships).

---

## Phase totals + risk + tests

| Phase | LOC | Risk | New tests |
|---|---|---|---|
| 1 wire-format + unknown-decode | ~90 | L | 3 (snake_case, round-trip, unknown-variant) |
| 4 back-compat | ~30 | L | 2 (old-agent-ignores, raw-close-stays-transient) |
| 3 agent fatal + supervisor + notify | ~170 | M | 5 (FatalGoodbye, Replaced, 3× escalation, raw-close, attention-path) + 1 supervisor unit test update |
| 2a server emit (handler.rs + tunnel mirror) | ~70 | L | 2 (agent refusal Goodbye, tunnel-client TunnelRevoked) |
| 2b Hub displacement + notify + unregister race fix | ~120 | M | 3 (displacement-Goodbye + cancel notify, stale-tx noop, channel-full warn) |
| 7 `cmd_enroll` stderr warning | ~25 | L | 1 (message marker phrases) |
| 5 native cert roots | ~3 + lock | L | manual smoke (SM-3) |
| **Total** | **~510** | **L–M** | **~17 new tests** |

Engineer-day budget: **~1.5 days realistic, ~2 days defensive** (Phase 3 + Phase 2b are the medium-risk items that drive the schedule; Phases 1, 4, 7, 5 are an afternoon each). If Phase 2b's `Notify` integration runs into DashMap re-entrance issues, the `oneshot::Sender<()>` fallback (per auto-fail #5) adds ~1 hour and is the safety hatch.

---

## Smoke matrix (manual on a Win11 VM, after CI green)

- **SM-1** — Delete a live agent row from the admin UI while the agent is connected. Agent log shows `server-side rc:goodbye received` ERROR + needs-attention sentinel + clean exit code 7. Supervisor's code-7 fast-path fires the `error!` on the FIRST failure (not after 8). `should_record_supervisor_crash` returns false → no `supervisor_crashes` row written.

- **SM-1b** *(NEW per critique #13)* — Delete a live agent row WHILE A REMOTE-CONTROL SESSION IS ACTIVE. Pre-conditions: controller is viewing the agent's desktop; admin clicks Delete in the admin UI. Assert:
  - (a) controller sees `ServerMsg::Terminate { reason: AgentDisconnect }` for the session
  - (b) agent emits Goodbye + exits 7
  - (c) `agents.active_sessions` decrements
  - (d) NO zombie webrtc-rs `PeerConnection` left on the agent side (verify via the `close_all_peers` + `close_all_tunnel_peers` invariant; supervisor's next worker spawn shows the indicator overlay cleared)
  - (e) The teardown invariant from Phase 3 step "Note on teardown invariant" actually runs.

- **SM-2** — Start two `roomler-agent run` instances with copies of the same `config.toml`. Expected timing: new connects at t=0 (displaces old), old reconnects at t=60s (displaces new), new at t=120s, … 3 displacements in the first 5 min triggers the escalation: needs-attention sentinel + exit 7 on whichever side hit count==3 first. The OTHER side stays connected. Admin UI's "online" indicator stabilises on one host.

- **SM-3** *(REVISED per critique #13)* — Corporate-MITM simulation. **Recommended path**: install `mkcert` on the test Win11 VM, generate a root CA, install in Local Machine Trusted Roots, run `stunnel` (or `mitmproxy --certs ...`) with that cert in front of `roomler.ai` pointing at `127.0.0.1`. Then `roomler-agent run` with the rc.52 binary FAILS the WS handshake (`UnknownIssuer`); with rc.53 it SUCCEEDS. Confirms Phase 5.

- **SM-4** — Pre-rc.53 agent (rc.52 binary) against post-rc.53 server: the server-side close still happens; the old agent sees the existing `ws read` close and reconnects. No regression, just no benefit. Confirms Phase 4 back-compat.

- **SM-5** *(NEW per critique #13)* — rc.53 server sends Goodbye mid-session to a rc.52 agent. Connect rc.52 agent, start a session, then trigger server-side delete. The rc.52 agent hits the existing `Err(e) => debug!(ignoring non-rc:* frame)` path on the `Goodbye` text frame, then sees the Close frame and reconnects (transient). Should pass identically to today.

- **SM-6** *(implicit, per Phase 7)* — On a non-elevated user PowerShell on Win11, run `roomler-agent enroll --machine-global --server https://roomler.ai --token … --name test-host`. Assert: the stderr warning block appears AFTER the success line, mentions both `sc start roomler-agent` AND `without --machine-global`.

---

## Auto-fail conditions

- Phase 2a sends the Goodbye but the WS closes before the frame flushes. **Mitigation**: rely on `.await` on `send(Close)` semantics — tungstenite serialises both frames into the same TCP buffer. NO `sleep`. If field smoke shows the frame is missing, escalate to `socket.flush().await` before `socket.send(Close)`.
- Phase 3 treats `ws read` (network blip) as fatal. **Defence**: the new `ConnectError::FatalGoodbye` / `ReplacedByNewer` variants are constructed ONLY from inside `handle_server_msg`'s explicit `ServerMsg::Goodbye` arm. The `Some(Err(e))` → `ConnectError::Transient` path at `signaling.rs:346-349` is untouched. SM-1 vs `tc qdisc add … netem` flake on the wire confirms the asymmetry.
- Phase 5 breaks plain-internet enrolment because of a feature-set conflict. **Catch**: `cargo check --workspace` + `cargo tree -e features -i webpki-roots` in CI. If `webpki-roots` is still transitively pulled, the binary inflates but doesn't break — file as rc.54 cleanup, don't gate rc.53 on it.
- Old client + new server: the existing client decoder at `agents/roomler-agent/src/signaling.rs:333` must continue to silently ignore the unknown `ServerMsg::Goodbye` variant. If `signaling::ServerMsg` has been migrated to `#[serde(deny_unknown_fields)]` since rc.52, Phase 4 is bigger than expected — re-check (current source: NO such attribute, confirmed by reading `crates/remote_control/src/signaling.rs:358-360`).
- Phase 2b's `prev.cancel.notify_waiters()` races against the new connection's read-loop registering — if the notify fires before the old read-loop has wrapped its `socket_rx.next()` in the `select!`, the notification is lost (`Notify::notify_waiters` only wakes currently-parked waiters). **Mitigation**: register the cancel BEFORE returning from `register_agent` and have `handle_agent_socket` enter its select-loop immediately after receiving the cancel handle. If field repro shows lost notifies, swap `Notify` for a `oneshot::Sender<()>` (one-shot, latches the signal even if no waiter is parked).
- The "unknown-variant graceful default" test (Phase 1) requires a custom `Deserialize` for `AgentCloseReason`. **Risk**: the wrong implementation could mask a serde panic; mitigation = the test itself.

---

## Commit / tag split

Single tag, `agent-v0.3.0-rc.53`. All seven phases are tightly coupled (wire format + back-compat + agent receive + server emit + Hub displacement + corp-TLS + UX warning). The phase ORDER above is for the implementer's testing rhythm; commits can be one-per-phase but the tag is at the end.

---

## Files most critical for implementation

- `C:\dev\gjovanov\roomler-ai\crates\remote_control\src\signaling.rs` — Phase 1 wire format + tests, `AgentCloseReason` + `ServerMsg::Goodbye`.
- `C:\dev\gjovanov\roomler-ai\crates\api\src\ws\handler.rs` — Phase 2a edits at `:172-201` (agent) + `:108-140` (tunnel-client mirror). NOT `remote_control.rs`.
- `C:\dev\gjovanov\roomler-ai\crates\remote_control\src\hub.rs` — Phase 2b displacement at `:87-116`, log line at `:108-111`, `unregister_agent` race fix at `:118-135`.
- `C:\dev\gjovanov\roomler-ai\crates\api\src\ws\remote_control.rs` — Phase 2b `handle_agent_socket` read-loop `select!` wrap.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\signaling.rs` — Phase 3 `handle_server_msg` new arm at `:373`, `ConnectError` variant additions at `:182-188`, `run()` outer loop changes at `:88-162`. Teardown invariant: `close_all_peers` + `close_all_tunnel_peers` already called at `:274, :283, :301, :342, :347` — Phase 3's new arm must invoke them before returning Err.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\notify.rs` — Phase 3 add `attention_path_for_worker()` + `raise_attention_machine_aware()`.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\watchdog.rs` — Phase 3 `AGENT_DELETED_EXIT_CODE: i32 = 7` next to `STALL_EXIT_CODE` at `:60`.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\win_service\supervisor.rs` — Phase 3 `should_record_supervisor_crash` update at `:588`, code-7 alarm-immediately fast-path at `:817-831`.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\main.rs` — Phase 7 stderr warning in `enroll_cmd` at `:792-860`.
- `C:\dev\gjovanov\roomler-ai\Cargo.toml` — Phase 5 feature swap at `:168`.

---

## Backlog (post-rc.53, candidate rc.54 items)

- **Wizard soft-delete-revive UX** (deferred Phase 6): spec `/api/agent/enroll/probe` endpoint properly. Auth: enrollment JWT scopes the probe by tenant. Response: list of `{agent_id, hostname, machine_id, deleted_at}` for deleted-rows-matching-current-hostname (hostnames are NOT unique within a tenant — return a list, wizard shows most-recent + "show N more"). ~150 LOC backend + ~80 SPA. Plan: `docs/plans/rc54-wizard-deleted-row-banner.md` (placeholder; to be written when rc.53 ships).
- **Unify TLS stack**: pick all-rustls (reqwest + tungstenite + webrtc-ice with `rustls-tls-native-roots`) or all-native-tls. Today 3 distinct TLS code paths in one binary (critique #10).
- **`notify::AttentionReason` enum + structured JSON sidecar**: so the tray icon + admin UI can differentiate `AgentDeleted` / `PolicyRejected` / `ReplacedByNewer` / `AuthRejected` programmatically rather than scraping the message text. Includes refactoring the existing auth-failure call-site at `agents/roomler-agent/src/signaling.rs:128-143` to use the new structured API.
- **Hub `unregister_agent` race**: if Phase 2b's fix turns out to be incomplete (e.g. the tx identity comparison fails under DashMap re-entrance), revisit with a UUID-tagged session-handle rather than pointer-equality.

---

(End of v2 plan content.)
