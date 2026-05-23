# rc.53 plan — WS close-reasons, agent-side fatal handling, corp-TLS hardening

## Problem this cycle solves

Two failure modes burned operator hours in the field this week (PC55331 +
the rc.48-era SystemContext loop):

1. **The server has a perfectly good reason for closing the WS, but
   the agent never sees it.** Today the backend handler does one of:
   - logs `agent is quarantined or deleted; refusing WS` and `return`s
     (close the socket, no protocol message),
   - logs `agent reconnected; dropping previous connection` inside
     `Hub::register_agent` (drop the old `tx`, the old pump task
     exits, the old WS closes),
   - logs `agent opened WS without rc:agent.hello — closing` if the
     first message is wrong.

   The agent sees a raw socket close, wraps it as `ws read`, and
   reconnects forever on the rc.52 backoff ladder. From outside it
   looks identical to a network flap. Today we spent half a day
   chasing duplicate-instances + corporate-proxy hypotheses before
   reading the actual server log and finding the `deleted; refusing
   WS` line. **The fix is to put the reason on the wire** so the
   agent can log + react.

2. **Corporate networks with SSL-inspection middleboxes.** rc.31/rc.32
   fixed the TURN/ICE path for ÖBB-style hosts (`Symantec Enterprise
   Mobile Root` in the trust store), but the WS path
   (`tokio-tungstenite` with `rustls-tls-webpki-roots`) still uses the
   Mozilla bundle ONLY. The day an inspecting proxy starts MITM'ing
   `wss://roomler.ai/ws` (today it doesn't — ÖBB tunnels direct via
   CONNECT), the agent breaks with `UnknownIssuer` just like ICE
   did pre-rc.32. Cheap to fix now, expensive when it bites.

## Goals

- A misbehaving agent surfaces the *reason* in its own log — not just
  `ws read`.
- An agent whose server-side row was deleted / quarantined STOPS
  reconnecting and tells the operator what to do.
- An agent that loses a Hub duel STOPS reconnecting and tells the
  operator there's a duplicate instance to find.
- The WS path trusts whatever Windows trusts (matches rc.32 ICE +
  reqwest's native-tls), so corporate TLS-inspection works.

Non-goals (deferred):
- HTTP CONNECT proxy support for WS. Today direct outbound 443 from
  the agent works in every fielded environment we know of; revisit
  only if a corporate network actually starts blocking it.
- Switching Hub policy from newest-wins to oldest-wins. Discussed
  but the right answer depends on field data we don't have yet; #1+#2
  break the duel either way.

## Phases

### Phase 1 — `ServerMsg::Goodbye` wire variant + close-reason enum (~0.5 d, low risk)

`crates/remote_control/src/signaling.rs`:

```rust
#[derive(Serialize, Deserialize, ...)]
#[serde(rename_all = "snake_case")]
pub enum AgentCloseReason {
    /// Server-side `agents` row has `deleted_at != null` or is
    /// otherwise refused by the WS handler's lookup. The agent's
    /// stored token is cryptographically valid but useless. Re-enrol
    /// to revive (soft-deleted rows rehydrate on (tenant_id,
    /// machine_id) match).
    AgentDeleted,
    /// A newer WS connection presented the SAME `agent_id`; the Hub
    /// kept the new one, dropped this old one. Indicates a duplicate
    /// install somewhere (another physical host with a copy of this
    /// `config.toml`, the tray companion, etc.).
    ReplacedByNewerConnection,
    /// Server-side policy refused (account suspended, tenant
    /// disabled, version too old). Reserved for future use.
    PolicyRejected,
}

pub enum ServerMsg {
    // existing variants…
    #[serde(rename = "rc:goodbye")]
    Goodbye {
        reason: AgentCloseReason,
        message: String, // human-readable, operator-targeted
    },
}
```

Wire-format lock via the existing `signaling::tests::*_serde_*` style
(round-trip + golden-JSON tests for each variant).

### Phase 2 — server emits `Goodbye` before closing (~0.5 d, low risk)

Two emit sites in `crates/api/src/ws/remote_control.rs`:

**2a — `handle_agent_socket` entry, before the existing "deleted/quarantined" `return`:**

Send `Goodbye { reason: AgentDeleted, message: "This agent's server-side row was deleted (or quarantined). Re-enrol with a fresh enrollment token from the admin UI to revive." }` over the WS, then close cleanly with `Close(Some(CloseFrame { code: 4003, reason: "agent_deleted" }))` before returning. Wait briefly (≤200 ms) for the frame to flush before drop.

**2b — Hub `register_agent` displacement path
(`crates/remote_control/src/hub.rs:105-113`):**

When the `insert` returns `Some(prev)`, the old `prev.tx` is the
mpsc that pumps to the displaced WS. Send `Goodbye {
ReplacedByNewerConnection, "Another agent is connecting with the
same agent_id; this connection is being closed. Check for a
duplicate install or re-enrol to mint a fresh agent_id." }` through
`prev.tx` BEFORE dropping it (one-shot, fire-and-forget; if the
channel is already full or closed, drop the Err — the close happens
anyway). The pump task receives one last message, forwards it to
the WS, then sees the channel close and exits cleanly. Pump's
write must precede its `socket.close()` so the displaced agent
receives the Goodbye before the WS drops.

Both sites get unit tests at the Hub / WS-handler boundary using the
existing mock-`tx` infrastructure.

### Phase 3 — agent treats `Goodbye` as fatal (~0.5 d, low risk)

`agents/roomler-agent/src/signaling.rs` (or the `peer.rs` equivalent
that handles inbound `ServerMsg` for the agent role):

- On `ServerMsg::Goodbye { reason, message }`:
  - `tracing::error!(?reason, %message, "server-side goodbye — stopping reconnect attempts");`
  - Match on `reason`:
    - `AgentDeleted` → stop reconnecting **indefinitely**. Operator
      action is the only recovery. Surface via the existing
      `notify::set_attention(AttentionReason::ReEnrollRequired)`
      sentinel so the tray / admin UI shows a clear "re-enrol
      required" badge. Exit the process with a documented exit code
      (`AGENT_DELETED_EXIT_CODE = 7`) so the SCM supervisor sees a
      distinctive non-zero exit and surfaces it instead of
      respawning forever — pairs cleanly with rc.51's
      `RESPAWN_ALARM_THRESHOLD` alarm.
    - `ReplacedByNewerConnection` → back off for **60 s minimum**
      (long enough that two dueling instances stagger out of phase
      and one wins), then resume. If we get a *second*
      `ReplacedByNewerConnection` within ~5 min, escalate to "stop
      reconnecting + set attention sentinel" — at that point the
      duel is real and the operator needs to find the duplicate.
    - `PolicyRejected` → stop reconnecting indefinitely (same as
      `AgentDeleted`).

- Existing transport-level `ws read` / `ws connect` errors continue
  to back off on the rc.52 ladder. Only the *explicit*
  `ServerMsg::Goodbye` triggers the new fatal path.

Unit tests: feed a synthetic `Goodbye` into the inbound handler,
assert reconnect-state transitions + that `notify::set_attention`
fired.

### Phase 4 — back-compat (the part everyone forgets) (~0.25 d, low risk)

rc.52-and-earlier agents won't know `ServerMsg::Goodbye` and the
existing `ClientMsg` / `ServerMsg` matches are exhaustive in some
consumers. Two precautions:

- New `Goodbye` variant lands in `signaling.rs` BEFORE the agent-side
  consumer match adds a defensive `_ => {}` arm — otherwise CI clippy
  on rc.52-era consumers fails on `unreachable_patterns`. (Same rule
  as the existing CLAUDE.md note: *"Defensive enum catch-alls need
  `#[allow(unreachable_patterns)]`"* — apply it here.)
- Old agents that don't deserialize the variant just hit the existing
  `Err(e) => debug!(ignoring non-rc:* message)` path on the
  client-side decoder. They miss the operator hint but don't crash.
  The server still sends the WS Close frame, so the old agent
  experiences the close the same way it does today (no regression).

### Phase 5 — `tokio-tungstenite` native cert roots (~0.25 d + lock churn)

`Cargo.toml`:

```diff
-tokio-tungstenite = { version = "0.28", features = ["rustls-tls-webpki-roots"] }
+tokio-tungstenite = { version = "0.28", features = ["rustls-tls-native-roots"] }
```

Same swap rc.32 did for the vendored `webrtc-ice`. On hosts with no
corporate inspection (the majority), webpki-roots-only would work too
— but native-roots additionally trusts whatever Windows trusts (incl.
the corporate CA from a `Symantec Enterprise Mobile Root` etc.). Zero
behaviour change on plain-internet hosts.

Verify by:
- `cargo check --workspace` clean
- on a normal-internet box: `roomler-agent run` reaches the server as
  before
- (manual, future) on a corporate-MITM box: WS handshake succeeds
  where rc.52 fails

### Phase 6 — wizard-side soft-delete-revive UX (NEW, ~0.5 d, low risk)

When the installer wizard enrols a host whose `derive_machine_id`
would produce a DIFFERENT `machine_id` from any existing
soft-deleted row for the same hostname, today it silently creates a
new row and the operator ends up with two rows in the admin UI for
"the same" host. The soft-delete identity-preservation benefit gets
bypassed every time the operator switches install flavour (perUser
→ perMachine, perMachine → perMachine + SystemContext).

Surface this on the wizard's Welcome step: when `cmd_detect_install`
finds any deleted-but-same-hostname row server-side, show a banner
*"A previous enrolment for this host was deleted on YYYY-MM-DD.
Continuing will create a new agent identity; the old sessions/audit
log will remain associated with the deleted row."* Operator can
proceed or cancel.

(This needs a new `/api/agent/enroll/probe` endpoint or piggyback on
the existing enroll path — pick whichever fits. ~50 LOC backend +
~30 LOC wizard SPA. Low priority within rc.53 — drop to rc.54 if the
cycle gets tight.)

## Answers to your "are these all needed" question

| v0 item | Rename in v1 | Need? | Why |
|---|---|---|---|
| #1 close-frame on Hub replace | Phase 2b | **Yes** | Single most impactful diagnostic |
| #2 agent stops reconnecting on `replaced_by_newer_connection` | Phase 3 (one of two reasons) | **Yes** | Without it the duel never ends + operator never sees the cause |
| #3 `rustls-tls-native-roots` for WSS | Phase 5 | **Yes-but-cheap** | Doesn't fix anything biting today, but ~15 lines + a Cargo.lock churn for permanent immunity to corporate TLS inspection. Same rationale as rc.32 ICE. Worth landing in the same cycle. |
| #4 newest-wins vs oldest-wins policy call | **Drop** | **No, deferred** | With #1+#2 shipping, neither policy "duels". Pick later if field data shows a problem. |
| #5 proxy-aware WS connect | **Drop** | **No, deferred** | Not biting any fielded host. Defer until it does. |

**New in v1** (driven by today's PC55331 finding, was missing from v0):

- Phase 1's `AgentDeleted` close-reason — covers the *actual* cause of
  today's hours of confusion (soft-deleted agent row).
- Phase 6 wizard-side soft-delete-revive UX — closes the identity-
  preservation gap when the operator switches install flavours.

## Phase totals + risk + tests

| Phase | LOC | Risk | Tests |
|---|---|---|---|
| 1 wire-format | ~80 | L | serde round-trip + golden JSON per variant (3) |
| 2 server emit | ~60 | L | Hub displacement test + WS-handler refusal test (2) |
| 3 agent fatal | ~120 | M | inbound dispatch test per variant; reconnect-state assertions (4) |
| 4 back-compat | ~5 | L | (covered by existing `#[allow(unreachable_patterns)]` rule) |
| 5 native cert roots | ~5 + lock | L | workspace `cargo check` + ~30 min manual smoke |
| 6 wizard UX | ~80 | L | optional — see note |
| **Total** | ~350 | L–M | ~10 new unit tests |

Engineer-day budget: **1.5 days realistic** if Phase 6 lands; **~1 day** without it.

## Commit / tag split

Single tag, `agent-v0.3.0-rc.53`. Phases 1–5 are tightly coupled (wire
format + emit + receive + back-compat + TLS). Phase 6 is a nice-to-have
that can slip to rc.54 if cycle pressure appears — flag it as a P1
during code review rather than a P0.

## Smoke matrix (manual on a Win11 VM, after CI green)

- **SM-1** — Delete a live agent row from the admin UI while the agent
  is connected. Agent log shows `server-side goodbye` `agent_deleted`
  ERROR + attention sentinel + clean exit code 7. Service supervisor
  sees code 7 + the rc.51 alarm fires after 8 consecutive code-7s.
- **SM-2** — Start two `roomler-agent run` instances with copies of
  the same `config.toml`. Both eventually see one
  `replaced_by_newer_connection`, back off 60 s, and one stays
  connected; the other gets a *second* notice and goes fatal. Admin
  UI's "agent online" indicator stays stable on one host.
- **SM-3** — On a corporate-MITM box (we don't have one in CI;
  simulate by trusting a self-signed corporate CA in the Windows
  store and routing through a `mitmproxy` that re-signs with that
  CA): `roomler-agent run` reaches the WS handshake under rc.53 +
  fails the same handshake under rc.52. Confirms Phase 5.
- **SM-4** — Pre-rc.53 agent (rc.52 binary) against post-rc.53
  server: the server-side close still happens; the old agent sees
  the existing `ws read` close and reconnects. No regression, just
  no benefit. Confirms Phase 4 back-compat.

## Auto-fail conditions

- Phase 2 sends the Goodbye but the WS closes before the frame
  flushes (need the small flush delay or a proper async sequencing
  with `socket.flush().await` before `close()`).
- Phase 3 treats `ws read` (network blip) as fatal — must distinguish
  the EXPLICIT `Goodbye` from raw socket close. Test SM-1's symptom
  vs a `tc qdisc add … netem` flake on the wire.
- Phase 5 breaks plain-internet enrolment because of a feature-set
  conflict between `tokio-tungstenite`'s native-roots and another
  workspace dep that pulled webpki-roots. Catch in `cargo check
  --workspace` + CI.
- Old client + new server: the existing client decoder must continue
  to silently ignore the unknown `ServerMsg::Goodbye` variant (serde
  `Err` → existing `debug!` log → no panic). If `signaling::ClientMsg`
  has been migrated to `#[serde(deny_unknown_fields)]` since rc.52,
  Phase 4 is bigger than expected — re-check.
