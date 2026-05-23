# rc.53 plan — independent critique

Verdict: **GO-WITH-CHANGES** (15 required changes).

Date: 2026-05-23. Reviewer: Plan agent. Plan under review:
`docs/plans/rc53-ws-close-reasons-and-corp-tls.md`.

Findings ordered roughly by criticality (highest first). Each verified against
the actual source (file:line citations).

---

## 0. Plan points at the wrong file for Phase 2a

The plan says *"Two emit sites in `crates/api/src/ws/remote_control.rs`"*. The
`agent is quarantined or deleted; refusing WS` log line lives in
**`crates/api/src/ws/handler.rs:189`**, inside `ws_upgrade_agent`'s
`ws.on_upgrade(...)` closure — BEFORE `handle_agent_socket` is ever called.
The refusal works on the raw axum `WebSocket` post-upgrade, then `return`s
without ever entering `remote_control.rs`. Anyone implementing Phase 2a who
edits `remote_control.rs` will edit the wrong file.

The parallel tunnel-client refusal is at `handler.rs:128` (same pattern; same
fix needed).

**Implication:** the fix is to NOT return early — instead split the socket,
push a Goodbye text frame + a CloseFrame, await flush, then return. Same
change applies for `ws_upgrade_tunnel_client`'s mirror site (the plan ignores
tunnel-clients entirely; if rc.53 is the cycle for "put close-reasons on the
wire" they should land together — the tunnel-client path's existing
`ServerMsg::TunnelRevoked` already proves the pattern).

---

## 1. Phase 2 timing — partly wrong, partly correct

**2a, can we write before the agent's hello?** YES. The reviewer's concern
about "read/write split" is misplaced for this site: axum `WebSocket::split()`
returns independent halves; the server can push a Text frame and a Close frame
on the sink half without ever reading. The displaced agent's signaling loop
(`signaling.rs:269-352`) selects on `ws.next()` and WILL read the Text frame
before noticing the Close — it's the standard tungstenite frame ordering. The
agent's hello blocking is irrelevant; the server can write at any point
post-upgrade.

**But** there's no `socket.flush().await` on axum's `WebSocket` (it's an
`impl Sink + Stream`, but `SinkExt::flush` resolves only against the
underlying buffer; OS TCP-level delivery is not guaranteed). The plan's
"≤200 ms" guard is hand-wavy. The correct pattern is `send(Text(json)).await;
send(Close(Some(CloseFrame{...}))).await;` followed by dropping the sink.
Tungstenite serialises both frames into the same TCP buffer; the OS will
deliver them in order. The 200 ms wait does NOT need to be a `sleep` — it
should be the `.await` on the close-frame send, which inside tungstenite
includes the necessary flush.

**2b, Hub displacement.** The plan's understanding of the channel mechanics
is incomplete in two material ways:

- `SERVER_TX_CAPACITY = 64` (`hub.rs:29`). Goodbye `try_send` into a near-full
  channel can fail. Under contention the new connection may displace before
  the old pump has drained — the plan's *"drop the Err — the close happens
  anyway"* is fine for the operator-message but means the displaced agent
  learns nothing. The plan should escalate: use `try_send`, and if Full, log
  a warn at the server side so the operator knows the displaced agent likely
  missed the Goodbye.

- **`drop(prev)` does NOT close the displaced WS.** The pump task
  (`pump_server_messages` at `remote_control.rs:159-176`) is one of two
  halves of the WS. When `prev.tx` is dropped, the pump's `rx.recv()` returns
  `None` and the pump exits cleanly without sending a Close frame. The
  **read loop in `handle_agent_socket` (`remote_control.rs:85`) keeps
  polling `socket_rx.next()`** — so the displaced WS stays half-open until
  the displaced agent itself closes the socket (which it won't, because
  nothing told it to). With the plan's Goodbye-first-then-drop, the agent
  receives the Goodbye Text frame, enters its new 60 s back-off — but the
  OS-level WS connection stays parked. The agent's own keepalive Ping (every
  25 s) will eventually trigger a send error → transient reconnect, but for
  ~25 s the displaced socket lingers. Workable, not a P0, but the plan
  claims *"the displaced agent receives the Goodbye before the WS drops"*
  which is false.

  **Additionally — pre-existing-bug-discovered-while-reviewing:** the
  displaced `handle_agent_socket` eventually exits its read loop and calls
  `state.rc_hub.unregister_agent(agent_id)`. By then the registry entry is
  the NEW connection's. So `unregister_agent` removes the NEW connection's
  `ConnectedAgent` from the registry and terminates its in-flight sessions
  via `EndReason::AgentDisconnect`. Phase 2b should fix this
  displacement-vs-unregister race by tracking the displaced `tx`'s identity
  and only unregistering when the registered tx is still the local one. This
  is technically out of scope for "close-reasons on the wire" but if Phase 2b
  is touching the displacement path it should land the fix too.

---

## 2. `AGENT_DELETED_EXIT_CODE = 7` provides no observable signal in rc.51 supervisor

Verified by reading `supervisor.rs:573-580`:

```rust
pub fn decide_exit_reaction(code: u32, consecutive_failures: u32) -> (ExitReaction, u32) {
    if code == 0 { (ExitReaction::Respawn, 0) }
    else { let next = consecutive_failures.saturating_add(1); (ExitReaction::Backoff(next_backoff(next)), next) }
}
```

The supervisor branches ONLY on `code == 0` vs `!= 0`. Exit-code 7 behaves
identically to exit-code 1 or 42: backoff ladder, alarm-after-8, infinite
respawn. The plan's claim *"the SCM supervisor sees a distinctive non-zero
exit and surfaces it"* is wrong as written.

Existing exit codes in use:
- `0` — clean
- `2` — `STALL_EXIT_CODE` (watchdog) — already excluded from sidecar recording
- everything else — `should_record_supervisor_crash(code)` records a sidecar

Code `7` is currently unused, so the plan doesn't collide with anything. But
to make the exit code OBSERVABLY useful, Phase 3 must also:

1. Define `AGENT_DELETED_EXIT_CODE: i32 = 7` in `watchdog.rs` (or a sibling
   exit-codes module) alongside `STALL_EXIT_CODE`.
2. Add `pub const FATAL_GOODBYE_EXIT_CODES: &[u32] = &[7]` and teach
   `decide_exit_reaction` (or a sibling) to treat these as "give-up-style:
   emit a structured `error!` immediately, longer backoff (e.g. 5 min), do
   NOT respawn-spam." That contradicts the rc.51 "never give up" principle
   though, so the safer choice is:
3. Keep the infinite-respawn ladder, but on code-7 specifically, force the
   alarm to fire after **1** consecutive failure rather than 8. The
   "operator action required" signal needs to be visible in <1 minute, not
   in ~4 minutes (8 × 30 s avg backoff).
4. ALSO modify `should_record_supervisor_crash(code)` to exclude
   `AGENT_DELETED_EXIT_CODE` — the agent already raised an attention
   sentinel; double-recording inflates fleet crash metrics the same way the
   rc.51 `STALL_EXIT_CODE` exclusion does. The plan misses this.

Without these the exit-code is purely documentation: nothing acts on it.

---

## 3. Phase 3's `notify::set_attention(AttentionReason::ReEnrollRequired)` doesn't exist

Verified by reading `agents/roomler-agent/src/notify.rs` in full: the API is
`raise_attention(message: &str) -> Result<PathBuf>` writing to a single
`needs-attention.txt`. There is no `AttentionReason` enum, no
`set_attention`, no structured reason on disk. The plan references API that
doesn't exist.

Two consequences:

- Phase 3 must either (a) extend `notify.rs` with an `AttentionReason` enum
  and a structured sidecar (JSON with `reason`, `message`, `generated_at`),
  then update the existing call-site at `signaling.rs:128-143` to use it —
  adds ~50 LOC, not "low risk" anymore — or (b) just call
  `raise_attention(msg)` with a hand-written string like the existing
  auth-failure path does. **(b) is cheaper but the tray / admin UI gets no
  machine-readable signal to differentiate "AgentDeleted" from
  "PolicyRejected" from "AuthRejected".**

  Pick (b) for rc.53 to keep scope honest. The structured-reason refactor is
  a worthwhile rc.54 item.

- Worse: `attention_path()` uses `directories::ProjectDirs` which resolves to
  **`%APPDATA%`**, not `%PROGRAMDATA%`. A machine-global SCM install runs
  the supervisor as LocalSystem, whose `%APPDATA%` is
  `C:\Windows\System32\config\systemprofile\AppData\Roaming\…` — invisible
  to the human operator. A user-context worker writes to the logged-in
  user's `%APPDATA%` — visible. The sentinel landing in the wrong location
  depending on worker role is the same `%APPDATA% / %PROGRAMDATA%`
  asymmetry the reviewer raised as a separate concern. Phase 3 must at
  minimum log the sentinel path it wrote to so the operator can find it.
  Better: write to `%PROGRAMDATA%\roomler\roomler-agent\needs-attention.txt`
  for machine-global installs.

---

## 4. Phase 3 mid-session displacement teardown — plan is silent

Confirmed by reading `signaling.rs:269-355`: a `ReplacedByNewerConnection`
received mid-session would land in `handle_server_msg` as an unknown variant
(until Phase 3 adds the match arm). After Phase 3, the new match arm would
set state to "back off ≥60 s" — but the plan doesn't say to tear down
`peers: HashMap<bson::oid::ObjectId, AgentPeer>` or `tunnel_peers`. Without
explicit teardown, the function returning Err(Transient) at the end of the
loop hits the existing `close_all_peers(&mut peers, &indicator).await;
close_all_tunnel_peers(&mut tunnel_peers).await;` cleanup on the connect-once
exit path — that handles it. So the implementation pattern is: convert
Goodbye into a `ConnectError::Transient` (for `ReplacedByNewerConnection`)
or a new `ConnectError::Fatal` (for `AgentDeleted`/`PolicyRejected`), and
let the existing exit-path cleanup run. The plan should explicitly call out
this routing instead of leaving it to the implementer to discover.

For `AgentDeleted` → `std::process::exit(AGENT_DELETED_EXIT_CODE)` directly
from the handler: that's fine but `close_all_peers` is then skipped, leaving
the webrtc-rs PeerConnections to be cleaned up by drop. Plan should
explicitly call `close_all_peers` + `close_all_tunnel_peers` before
`process::exit`, otherwise the displaced session's controllers (other side
of the WebRTC) don't get a clean ICE-restart hint — they have to detect the
silence via their own ICE-connection-state-changed handlers (10-30 s).

---

## 5. Phase 4 back-compat — re-confirmed correct as written, with one addition

Existing decoder at `agents/roomler-agent/src/signaling.rs:333` is:

```rust
Err(e) => debug!(%e, text = %text.as_str(), "ignoring non-rc:* frame"),
```

So a pre-rc.53 agent receiving the new `ServerMsg::Goodbye` discriminator
hits the `Err` arm and ignores it. No `#[serde(deny_unknown_fields)]` on
`ServerMsg`. No panic risk. **The plan's auto-fail condition #4 is satisfied
today.**

But the reviewer's "new-agent + old-server" question is real and the plan
doesn't address it: rc.53 agent against rc.52 server experiences a raw
socket close on a deleted-row case. The plan's only mention of this
direction is implicit (*"Only the explicit ServerMsg::Goodbye triggers the
new fatal path"*). Phase 3's match arm should be guarded explicitly so that
the existing `Some(Ok(Message::Close(_))) | None => return Ok(())` and
`Some(Err(e)) => return Err(ConnectError::Transient(...))` paths stay
untouched. Add a test that synthesises a raw Close (not a Goodbye) and
asserts the agent treats it as transient — defends against the "if we don't
see a Goodbye assume server is old" heuristic the reviewer worried about
creeping in.

---

## 6. Phase 5 — feature exists, but mixed TLS stacks is real

Verified at
`C:/Users/goran/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-tungstenite-0.28.0/Cargo.toml`:

```
native-tls = [
native-tls-vendored = [
rustls-tls-native-roots = [
rustls-tls-webpki-roots = [
```

Both `rustls-tls-native-roots` and `native-tls` features exist. Plan's swap
to `rustls-tls-native-roots` is valid.

**But the reviewer's concern about dual TLS stacks is real.** Workspace
Cargo.toml line 103: `reqwest = { version = "0.12", features = ["json",
"multipart", "cookies", "stream"] }` — no `default-features = false`, so
reqwest pulls `default-tls` which is `native-tls` (Schannel on Windows). So
the agent binary today carries:

1. `reqwest` → `native-tls` → Schannel (enrollment, file uploads)
2. `tokio-tungstenite` → rustls + `webpki-roots` (WS signaling)
3. `webrtc-ice` (vendored, rc.32 swap) → rustls + native roots (ICE
   TURNS/TCP)

Phase 5 makes #2 use rustls + native roots, aligning with #3 but still
divergent from #1. Two TLS implementations in one binary is the status quo
+ a small change. Worth landing — but the plan should call out the
divergence and put "unify TLS stacks in rc.54" on the backlog. Either go
all-rustls (reqwest's `rustls-tls-native-roots` feature) or all-native-tls.
Today the agent has one cert-chain bug surface per stack × per OS.

Phase 5's `cargo check --workspace` is necessary but not sufficient as a
smoke. The real risk is a workspace-dep that pulled `webpki-roots`
transitively (we have `webrtc-0.12` vendored, and `tokio-tungstenite` itself
when activated with webpki-roots adds it). Switching the feature can leave
`webpki-roots` unused but still compiled, inflating binary size. Plan
should add `cargo tree -e features -i webpki-roots` to the verification —
confirms no other consumer.

---

## 7. Phase 6 — `/api/agent/enroll/probe` is genuinely under-spec'd

The plan acknowledges this in a parenthetical: *"This needs a new
`/api/agent/enroll/probe` endpoint or piggyback on the existing enroll path
— pick whichever fits."* That's not a plan; it's a TODO. The reviewer is
right to flag.

The auth model is non-trivial:
- The wizard has an enrollment JWT (single-use, short-lived) but no agent
  token.
- Probing by "hostname + tenant from enrollment JWT" is fine for the tenant
  scoping; it's not a security regression because anyone with a valid
  enrollment JWT for that tenant can already create agents.
- Hostnames within a tenant ARE NOT unique. Probe returns a LIST of
  deleted-but-same-hostname rows; wizard shows the most-recent one in the
  banner, links to "show N more deleted enrolments" if N>1.

Either commit to specifying this concretely in rc.53 (~150 LOC backend
incl. tests + ~80 LOC wizard SPA, not the plan's ~80 backend / ~30 wizard)
or **definitively defer to rc.54**. As written it's neither. Recommend
defer — Phase 6 is the most speculative item, has no fielded user pain
driving it, and the cycle without it ships in ~1 day.

---

## 8. The `%APPDATA% / %PROGRAMDATA%` same-session asymmetry — completely missing from plan

The reviewer's point is exactly right: the user TODAY runs `enroll
--machine-global` (writes %PROGRAMDATA%), then `roomler-agent run` from user
PowerShell (reads %APPDATA% — different file, different machine_id, deleted
row). The plan adds Phase 6 for cross-session "two rows in admin UI" but
nothing for same-session "user shell reads wrong config."

This deserves a Phase 7 (~30 LOC stderr warning in `cmd_enroll`):

```rust
if args.machine_global && !running_as_localsystem() {
    eprintln!("\nNote: --machine-global wrote config to %PROGRAMDATA%, which is read by\n\
               the LocalSystem service worker. A `roomler-agent run` from this user\n\
               shell will instead read %APPDATA% (a separate config, different\n\
               machine_id) and will look like a different host to the server. To\n\
               test as a user-context worker, re-run `enroll` without --machine-global,\n\
               or stop here and start the service via `sc start roomler-agent`.");
}
```

The plan's "low priority" framing for Phase 6 makes it tempting to drop, but
the wizard work is fungible and this stderr warning is not. **Reshuffle: drop
Phase 6, add Phase 7 stderr warning. Same total LOC.**

---

## 9. Smoke matrix gaps

- **SM-1 missing the worst case.** Plan's SM-1: delete a live agent,
  observe Goodbye + exit 7. The worst real-world case is **delete an agent
  THAT HAS AN ACTIVE WEBRTC SESSION** — that's the Phase 2b mid-session
  displacement scenario combined with admin-delete. Need an SM-1b: connect
  a controller, start a session, then delete the agent server-side.
  Assert: (a) controller sees `ServerMsg::Terminate` with
  `EndReason::AgentDisconnect`, (b) agent sees Goodbye + exits 7,
  (c) `agents.active_sessions` counter decrements, (d) no zombie webrtc-rs
  PeerConnection on the agent side (check via the watchdog gate
  transitioning false).

- **SM-2 race-window too narrow.** "60 s backoff + escalate after 2nd within
  5 min" math: 8 displacement notices in 5 min × 60 s back-off = the new
  agent connects at t=0, displaces the old; old connects at t=60 s,
  displaces the new; new at t=120 s, … in 5 min that's 5 cycles ≈ alarm
  fires on the 3rd cycle. The reviewer's check (5 displacement notices then
  fatal) is correct, but SM-2 should explicitly assert this timing (third
  displacement within 5 minutes triggers fatal exit), not just "both
  eventually see one ReplacedByNewerConnection."

- **SM-3 — mitmproxy + Windows cert store is hard to set up.** Realistic
  alternative: install `mkcert` on the test Win11 VM, generate a root CA,
  install it in Local Machine Trusted Roots, run a stunnel/squid
  intermediary with that cert in front of roomler.ai's domain pointing at
  `127.0.0.1`. Then `cargo run --release -- run` with the rc.52 binary
  fails the WS handshake; with rc.53 it succeeds. The plan's mitmproxy hint
  is OK but the operator should know mkcert is the lighter path.

- **No "old agent + new server with active session" smoke.** SM-4 covers
  connect-time; nothing exercises an in-flight session when the rc.53
  server sends Goodbye to a rc.52 agent. Should pass identically to today
  (raw close on the agent side), but worth confirming.

---

## 10. Two more issues

- **`AgentCloseReason` exhaustiveness in consumers.** Phase 1's enum is
  `serde(rename_all = "snake_case")`. The existing `EndReason` /
  `CloseReason` / `RejectKind` enums in `signaling.rs` all use this pattern
  and have call-sites that do `match` exhaustively. The plan's Phase 4 says
  *"rc.52-and-earlier agents won't know `ServerMsg::Goodbye`"* — fine, the
  discriminator-level decoder catches that. But ANY new rc.53+ code that
  matches on `AgentCloseReason` (the operator-message formatter, the
  wizard's UX hint, the tray icon code) needs
  `#[allow(unreachable_patterns)]` per CLAUDE.md rule, OR exhaustive
  `_ => "unknown".to_string()` arms with the rationale comment. The plan
  says "Phase 4 covers Goodbye back-compat" but doesn't address adding new
  `AgentCloseReason` variants in rc.54+ (e.g. `VersionTooOld`,
  `LicenseExpired`). Bake this into Phase 1's wire-format-lock tests: a
  golden-JSON test that asserts the agent's decoder treats an unknown
  `reason: "xyzzy"` variant as `AgentCloseReason::PolicyRejected` (sensible
  default — "server policy refused you") rather than a panic / hard error.
  Otherwise the next time anyone adds a variant, fielded rc.53 agents hard-
  fault on it.

- **`session.rs:107-112` info-log says "agent X reconnected; dropping previous
  connection".** The plan replaces the silent drop with a Goodbye, but the
  LOG LINE stays — except now it's wrong, because we're not just dropping,
  we're notifying. Update the log line to *"agent reconnected; notifying
  previous connection with ReplacedByNewerConnection and dropping"*. Tiny,
  but matters for log-grep'ing the duel scenario.

---

## Required changes if GO-WITH-CHANGES

1. Move Phase 2a edit-site from `crates/api/src/ws/remote_control.rs` to
   `crates/api/src/ws/handler.rs:172-201` (the `ws_upgrade_agent`
   `on_upgrade` closure). Mirror change at `handler.rs:108-140` for the
   tunnel-client path (use the existing `ServerMsg::TunnelRevoked`
   taxonomy already in place there + the new fix-up to actually close,
   since today it just returns).
2. Replace the plan's `≤200 ms flush wait` with explicit `.await` on both
   `send(Text(...))` and `send(Close(Some(CloseFrame{code: 4003, reason:
   "agent_deleted"})))`. Confirm via `cargo doc` that axum's `WebSocket:
   Sink` flushes on close-frame send. No `sleep`.
3. Make Phase 2b actually close the displaced WS: after
   `prev.tx.try_send(Goodbye)`, the new connection must signal the old
   read-loop to exit. Cheapest: convert `ConnectedAgent` to carry a
   `tokio::sync::Notify` or oneshot-cancel; old `handle_agent_socket`
   selects on `socket_rx.next()` AND the cancel signal. Without this the
   displaced WS lingers for up to one keepalive interval (25 s). Note in
   plan.
4. Fix the pre-existing `unregister_agent` race in the displacement path:
   pass the registered tx (or an equivalent identity token) to
   `unregister_agent` and only remove if it matches.
5. Phase 3: scrap the `set_attention(AttentionReason::ReEnrollRequired)`
   API reference. Either (a) call existing `raise_attention(&str)` with a
   multi-line operator message (mirror the auth-failure call-site at
   `signaling.rs:128-143`), or (b) commit to a `notify.rs` refactor and
   budget the LOC honestly.
6. Phase 3: explicitly call `close_all_peers` + `close_all_tunnel_peers`
   before `process::exit(AGENT_DELETED_EXIT_CODE)`. Cite this in the smoke
   matrix as a checked teardown invariant.
7. Phase 3: write the attention sentinel to
   **`%PROGRAMDATA%\roomler\roomler-agent\needs-attention.txt`** when
   running as LocalSystem, fall back to `attention_path()` (the existing
   `%APPDATA%` path) for user-context workers. Log the resolved path at
   WARN level so the operator can find the file.
8. Phase 3: define `AGENT_DELETED_EXIT_CODE: i32 = 7` in
   `agents/roomler-agent/src/watchdog.rs` (or a new `exit_codes.rs`) so
   the constant lives near `STALL_EXIT_CODE`. Update
   `should_record_supervisor_crash` at `supervisor.rs:588` to exclude
   code 7. Add a code-7-specific alarm-immediately path (fire `error!`
   on first failure, not after 8) so the operator-action signal is
   visible in <1 minute. Update the rc.51 supervisor unit tests
   accordingly.
9. Phase 4: add a unit test that constructs a JSON
   `{"t":"rc:goodbye","reason":"xyzzy","message":"x"}` and asserts the
   rc.53 agent's match arm treats unknown reasons as `PolicyRejected` (or
   another sensible non-panicking default). Defends future variant
   additions.
10. Phase 5: add `cargo tree -e features -i webpki-roots` to the
    verification list. Add a backlog note "unify TLS stack to rustls +
    native roots in rc.54" so the dual-stack divergence
    (reqwest=native-tls, tungstenite=rustls,
    vendored-webrtc-ice=rustls) gets resolved.
11. **Drop Phase 6 to rc.54** with a one-line "out of scope for rc.53, see
    rc54-wizard-deleted-row-banner.md" stub. The auth model +
    hostname-uniqueness handling is genuinely under-spec'd; defer it.
12. **Add Phase 7** (~15 LOC stderr warning in `cmd_enroll` when
    `--machine-global` is set and the process isn't running as
    LocalSystem) for the same-session `%APPDATA% / %PROGRAMDATA%`
    asymmetry. This is the user's CURRENT pain and is cheaper + higher-
    value than Phase 6.
13. Smoke matrix: add SM-1b (delete agent with active WebRTC session —
    assert controller `Terminate`, agent Goodbye + exit 7, no zombie
    peers). Replace SM-3's mitmproxy hint with mkcert + stunnel for ease.
    Add SM-5 (rc.53 server sends Goodbye mid-session to rc.52 agent —
    assert no regression).
14. Update the displacement log line at `hub.rs:108-111` to reflect the
    new behaviour ("reconnected; notifying previous connection with
    ReplacedByNewerConnection and dropping").
15. Wire-format test for Phase 1: golden-JSON for each
    `AgentCloseReason` variant, AND a "decoder accepts unknown variant
    string" test (per item 9).

---

## Files most critical for implementation

- `C:\dev\gjovanov\roomler-ai\crates\api\src\ws\handler.rs` — Phase 2a
  edits live here, NOT `remote_control.rs`.
- `C:\dev\gjovanov\roomler-ai\crates\remote_control\src\hub.rs` — Phase 2b
  displacement, the goodbye-then-drop sequence, displacement-vs-unregister
  race.
- `C:\dev\gjovanov\roomler-ai\crates\remote_control\src\signaling.rs` —
  Phase 1 wire format + tests.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\signaling.rs` —
  Phase 3 fatal-Goodbye handling, AGENT_DELETED_EXIT_CODE call site,
  peer teardown before exit.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\win_service\supervisor.rs`
  — rc.51 supervisor: code-7-specific alarm-now path,
  `should_record_supervisor_crash` exclusion.

**VERDICT: GO-WITH-CHANGES**
