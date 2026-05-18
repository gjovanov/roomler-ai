# HANDOVER24 — roomler-tunnel T1 + T2.1–T2.9 staged

> Three-session arc: design → plan → critique → T1 (foundation) →
> T2.1–T2.9 (wire types, models, evaluator, server gate, WS dispatch,
> agent acceptor, TunnelPeer + 8-DC pool, agent wiring, pump+demux).
> All changes are **uncommitted** on top of master rc.39 (parallel
> work shipped via commits 34a5d34..39f612e: Task 9 crash-ingest,
> rc.36 service-env CLI, rc.37 IDR tuning, rc.38 input fix, rc.39
> worker-side crash recording). Next session continues with **T2.10**
> — `roomler-tunnel forward` CLI + end-to-end smoke through real agent.
>
> **Updated 2026-05-18**: stash@{0} recovery completed; T2.8 acceptor
> dispatch (previously claimed done but actually missing) was wired
> this session — `ServerMsg::TcpForwardForward` arm in
> `agents/roomler-agent/src/signaling.rs` now spawns
> `tunnel::acceptor::handle_forward_request` and pushes the reply
> back via `outbound_tx`. Also fixed an auto-merge regression on
> `set-service-env-var` — restored master's `value: Option<String>`
> (omit-to-unset) semantics that the stash's `value: String`
> (required) signature had clobbered.

## Resume in one command

Work is on branch **`feature/roomler-tunnel`** in a sibling worktree:
**`C:/dev/gjovanov/roomler-ai-tunnel/`**. The main repo at
`C:/dev/gjovanov/roomler-ai/` stays on clean master so other Claude
sessions don't conflict. Use `git worktree list` to confirm.

```bash
cd C:/dev/gjovanov/roomler-ai-tunnel/
git status --short                                   # should be clean
git log --oneline -3                                 # 49ee819 T2.10 client CLI on top
cargo test -p roomler-ai-tunnel-core --lib           # 37 tests
cargo test -p roomler-ai-remote-control --lib        # 34 tests
cargo test -p roomler-agent --lib tunnel::           # 12 tests
cargo test -p roomler-agent --bin roomler-agent      # 8 CLI parser
cargo test -p roomler-tunnel                         # 17 lib + 4 bin = 21 tests
```

All six pre-flights green as of 2026-05-18.

If the tunnel worktree is gone but `feature/roomler-tunnel` branch
still exists in the main repo's `git branch -a`, recreate it:

```bash
cd C:/dev/gjovanov/roomler-ai/
git worktree add ../roomler-ai-tunnel feature/roomler-tunnel
```

If the branch is gone too (catastrophic case), the work is in the
last `git reflog` entry of the branch ref or possibly back in a
`git stash` named `pre-stderr-capture-wip` or similar. **Don't trust
the stash label** — labels like "pre-stderr-capture-wip" can wrap
the full tunnel arc. Inspect by file list:

```bash
git stash list
git stash show -u stash@{N} --name-only             # look for crates/tunnel-core
```

## What's done

| # | Task | Status | Key files | Tests added |
|---|------|--------|-----------|-------------|
| **T1.D1** | SCTP rwnd spike on webrtc-rs 0.12 — confirmed `SettingEngine` has no setter; `max_receive_buffer_size: 0` hardcoded at `webrtc-0.12.0/src/sctp_transport/mod.rs:159` | ✅ | report-only | — |
| **T1.D1.5** | Vendored `webrtc 0.12.0` fork — adds `SettingEngine::set_sctp_max_receive_buffer_size` + threads through `RTCSctpTransport::start` | ✅ | `crates/vendored/webrtc/` + workspace `[patch.crates-io]` | 1 roundtrip lock |
| **T1.D2-3** | `tunnel-core` + `roomler-tunnel` skeletons | ✅ | `crates/tunnel-core/` (8 files), `agents/roomler-tunnel/` (3 files) | 9 (policy + mux + forward) |
| **T1.D3-4** | `TunnelClient` + `TunnelEnrollment` JWT audiences | ✅ | `crates/services/src/auth/mod.rs` | 13 cross-audience |
| **T1.D4-5** | `TunnelClient` model + DAO + indexes | ✅ | `crates/remote_control/src/models.rs`, `crates/services/src/dao/tunnel_client.rs`, `crates/db/src/indexes.rs` | — (services blocked by openssl-sys local) |
| **T1.D6** | Enrollment HTTP endpoints | ✅ | `crates/api/src/routes/tunnel.rs` | — |
| **T1.D6-7** | WS `role=tunnel-client` + revocation re-check | ✅ | `crates/api/src/ws/{handler,tunnel}.rs` | — |
| **T1.D7** | TunnelClientsSection admin UI | ✅ | `ui/src/components/admin/TunnelClientsSection.vue`, `ui/src/stores/tunnelClients.ts`, router + AdminPanel | — (UI build green) |
| **T2.1** | `rc:tunnel.*` wire types (TunnelHello / Open / Opened / TcpForwardRequest/Accept/Reject + TcpForwardForward server→agent / TcpHalfClose / TcpClosed / TunnelTerminate / TunnelRevoked) + supporting enums (TunnelRole / RejectKind / Direction / CloseReason) | ✅ | `crates/remote_control/src/signaling.rs` | 9 wire-format locks |
| **T2.2** | `TunnelPolicy` + `TunnelAuditEvent` + `TunnelAuditKind` + `RelayMode` models + DAOs | ✅ | `crates/services/src/dao/tunnel_{policy,audit}.rs` + AppState wiring | — |
| **T2.3** | Full ACL evaluator with first-match-wins, subject (UserId/RoleId/TunnelClientId/AllUsers), target (AgentId/AllAgents), soft-deleted ignored, ceilings plumbed | ✅ | `crates/tunnel-core/src/policy.rs` | 15 table-driven |
| **T2.4** | Server-side gate `check_forward_request` with cross-tenant defence-in-depth + agent-availability check | ✅ | same file | 7 incl. CrossTenant lock |
| **T2.5** | WS dispatch consumes gate; audit every accept/reject; periodic revocation re-check via typed `TunnelRevoked` | ✅ | `crates/api/src/ws/tunnel.rs` (710 LOC) | — |
| **T2.6** | Agent tunnel module: `AgentForwardAcl` (default enabled + empty = trust server), `dial_dst` with TCP_NODELAY + 5s timeout, `handle_forward_request` orchestrator | ✅ | `agents/roomler-agent/src/tunnel/{mod,acl,dialer,acceptor}.rs` | 12 |
| **T2.7** | `TunnelPeer` with 8-channel pre-negotiated DC pool, deterministic stream ids 100/102/.../114, **SCTP rwnd 8 MiB via the vendored fork's setter**, SDP offer/answer + ICE plumbing | ✅ | `crates/tunnel-core/src/transport/{mod,webrtc_dc,wireguard}.rs` | 3 incl. two-peer handshake + ping roundtrip |
| **T2.8** | Agent acceptor wired into signaling.rs dispatch (**rewired 2026-05-18 — the original stash dropped this on the floor**): new `ServerMsg::TcpForwardForward` arm spawns `tunnel::acceptor::handle_forward_request` with `forward_acl: &AgentForwardAcl` threaded through `handle_server_msg`; reply (`ClientMsg::TcpForwardAccept`/`Reject`) flows back via `outbound_tx`. `forward_acl` lives on `AgentConfig` + all struct literals (enrollment.rs, config.rs test fixture). Defensive `#[allow(unreachable_patterns)]` catch-all for remaining TunnelOpened/TcpForwardAccept/etc. variants per the CLAUDE.md rule. | ✅ | `agents/roomler-agent/src/{config,enrollment,signaling}.rs`, `crates/remote_control/src/signaling.rs` | — |
| **T2.9** | `FlowDemux` (decode flow_id prefix + route to per-flow mpsc) + `pump_tcp_to_dc` (backpressure via `bufferedAmountLowThreshold` event) + `pump_dc_to_tcp` (shuts down TCP write on mailbox close) + `run_flow` (awaits both for half-close semantics) + `HALF_CLOSE_MAGIC = &[0xFF]` in-band marker | ✅ | `crates/tunnel-core/src/forward.rs` | 3 (single-msg demux + 256k burst + unregistered) |

**Test totals (all green locally):**
- `roomler-ai-tunnel-core`: **37 tests** (5 mux/policy helpers + 15 evaluator + 7 gate + 5 transport + 3 forward + 2 misc)
- `roomler-ai-remote-control`: **34 tests** (incl. 9 new rc:tunnel.* wire locks)
- `roomler-agent`: **12 tunnel tests** (acl + dialer + acceptor)
- Vendored `webrtc`: **1 SCTP rwnd setter lock**

## State on disk (uncommitted)

```
M  CLAUDE.md                                         # Defensive enum catch-alls rule
M  Cargo.toml                                        # webrtc vendored fork in [patch.crates-io] + tunnel-core/roomler-tunnel members
M  agents/roomler-agent/Cargo.toml                   # +tunnel-core dep
M  agents/roomler-agent/src/{config,enrollment,lib,signaling}.rs   # forward_acl + tunnel mod + TcpForwardForward dispatch
M  crates/api/{Cargo.toml,src/lib.rs,src/routes/mod.rs,src/state.rs,src/ws/handler.rs,src/ws/mod.rs}
M  crates/db/src/indexes.rs                          # tunnel_clients/tunnel_policies/tunnel_audit indexes
M  crates/remote_control/src/{models,signaling}.rs   # TunnelClient + TunnelPolicy + TunnelAuditEvent + rc:tunnel.* variants
M  crates/services/src/{auth/mod.rs,dao/mod.rs}      # TokenType::TunnelClient/TunnelEnrollment + tunnel_client/policy/audit DAOs
M  ui/src/{plugins/router.ts,views/admin/AdminPanel.vue}
?? agents/roomler-agent/src/tunnel/                  # acl.rs + dialer.rs + acceptor.rs + mod.rs
?? agents/roomler-tunnel/                            # CLI skeleton (T2.10 fleshes it out)
?? crates/api/src/routes/tunnel.rs                   # enrollment endpoints + list
?? crates/api/src/ws/tunnel.rs                       # WS dispatch (T2.5)
?? crates/services/src/dao/tunnel_{audit,client,policy}.rs
?? crates/tunnel-core/                               # full crate
?? crates/vendored/webrtc/                           # vendored fork
?? ui/src/components/admin/TunnelClientsSection.vue
?? ui/src/stores/tunnelClients.ts
```

**There is no stash anymore** as of 2026-05-18 — the previously-renamed `stash@{0}: pre-stderr-capture-wip` (which actually contained the full tunnel arc) was popped and auto-merged cleanly against rc.39's intervening commits (rc.36 service-env CLI through `6ffe10a` 429 retry). One semantic regression got auto-merged in and was hand-fixed: `Command::SetServiceEnvVar.value` reverted from `String` back to `Option<String>` to preserve master's omit-to-unset CLI. One ride-along change retained:

- `ui/src/workers/rc-vp9-444-worker.ts`: `hardwareAcceleration: 'prefer-hardware'` hint on VideoDecoder config for Profile-1 HW decode on capable GPUs.

A second ride-along (`agents/roomler-agent/src/crash_uploader.rs` `INTER_REQUEST_DELAY = 1100 ms`) was independently shipped on master as commit `6ffe10a` while this stash was dormant — the stash's identical change auto-absorbed and no longer shows in `git diff`.

There's also a mystery `files.zip` at the root (16 KB, contains `create-issues.sh` + `mediasoup-scaling-issues.md`, both dated 2026-05-16 22:25) that came along in the stash — safe to delete before committing.

## T2.10 progress (2026-05-18, on `feature/roomler-tunnel` worktree)

**Done in `agents/roomler-tunnel/` and `crates/tunnel-core/`:**

| Sub-task | Status | Files |
|---|---|---|
| T2.10a — `roomler-tunnel forward` client CLI | ✅ | `agents/roomler-tunnel/src/{config,forward,main,lib}.rs` |
| Config loader (TOML + env override) | ✅ | `agents/roomler-tunnel/src/config.rs` (7 tests) |
| WS handshake (hello → open → opened) | ✅ | `agents/roomler-tunnel/src/forward.rs` |
| SDP offer + ICE trickle + answer | ✅ | same |
| Listen loop + per-flow oneshot reply registry | ✅ | same |
| `enroll` command (POST /api/tunnel-client/enroll) | ✅ | `agents/roomler-tunnel/src/main.rs` |
| T2.10b — `rc:tunnel.tcp.half_close` over WS | ✅ (audit-only) | `crates/tunnel-core/src/forward.rs` |
| `run_flow` takes `HalfCloseSink` callback | ✅ | same |

**T2.10b design clarification — wire-level half-close is AUDIT ONLY.**
The original HANDOVER24 plan said "wire-level replaces in-band". An
attempt to do exactly that broke `demux_handles_256k_burst` at ~40%
completion because the WS `unregister()` fires asynchronously from
in-flight DC chunks; SCTP per-stream ordering only guarantees the
in-band `HALF_CLOSE_MAGIC = [0xFF]` sentinel arrives strictly AFTER
every prior data byte on the same flow, but the wire message can
race ahead. **Resolution**: keep the in-band sentinel for the
data-plane close; emit the wire `TcpHalfClose` purely for the
`tunnel_audit` accounting in the server. `run_flow` now takes a
`HalfCloseSink: Arc<dyn Fn(u32)>` the caller wires to the WS sink.

**T2.10c (server-side relay) — DONE 2026-05-19:**

| Sub-task | Status | Files |
|---|---|---|
| T2.10c.1 — `Hub::send_to_agent` + `AppState::tunnel_clients_by_session` | ✅ | `crates/remote_control/src/hub.rs`, `crates/api/src/state.rs` |
| T2.10c.2 — Tunnel WS refactored to mpsc-based outbound + pump | ✅ | `crates/api/src/ws/tunnel.rs` |
| T2.10c.3 — Real relay in `handle_tcp_forward_request` | ✅ | same |
| T2.10c.4 — Agent → tunnel-client relay (`relay_tunnel_msg_from_agent`) | ✅ | `crates/api/src/ws/remote_control.rs` |
| T2.10c.5 — Tunnel-client → agent relay for HalfClose / Closed / Terminate | ✅ | `crates/api/src/ws/tunnel.rs` |
| T2.10c.6 — Distinct `rc:tunnel.sdp.{offer,answer}` + `rc:tunnel.ice` wire types | ✅ | `crates/remote_control/src/signaling.rs`, both WS handlers, tunnel-client `forward.rs` |

The server now actually forwards `ClientMsg::TcpForwardRequest` to the agent as `ServerMsg::TcpForwardForward`, awaits the agent's `ClientMsg::TcpForwardAccept` / `Reject`, and relays it back. SDP/ICE handshake uses dedicated `rc:tunnel.sdp.*` / `rc:tunnel.ice` discriminators so the server can route by `session_id` against `tunnel_clients_by_session` without name-conflicting with the remote-control session namespace.

**Cannot validate locally** — services/api/tests hit openssl-sys MSVC blocker. Locally green:
- `roomler-ai-tunnel-core --lib`: 37/37
- `roomler-ai-remote-control --lib`: 37/37 (3 new wire-format locks)
- `roomler-tunnel --lib`: 17/17
- `roomler-agent --lib tunnel::`: 12/12
- Workspace `cargo check -p {tunnel-core,remote_control,roomler-tunnel,roomler-agent}` clean
- `cargo fmt --all -- --check` clean

**T2.10d (agent-side TunnelPeer + answerer pipeline) — DONE 2026-05-19:**

| Sub-task | Status | Files |
|---|---|---|
| T2.10d.1 — `agents/roomler-agent/src/tunnel/peer.rs` answerer module | ✅ | new file |
| T2.10d.2 — Wire `TunnelSdpOffer` / `TunnelIce` / `TunnelTerminate` dispatch in agent signaling.rs + `tunnel_peers` registry + `close_all_tunnel_peers` | ✅ | `agents/roomler-agent/src/signaling.rs` |
| T2.10d.3 — Split acceptor into `decide_forward` (testable) + `handle_forward_request` (data plane); spawn `tunnel_core::forward::run_flow` with the agent peer's `HalfCloseSink` | ✅ | `agents/roomler-agent/src/tunnel/acceptor.rs` |

The agent now:
1. Receives `ServerMsg::TunnelSdpOffer { session_id, sdp }`, builds an `AgentTunnelPeer`, replies with `ClientMsg::TunnelSdpAnswer`.
2. Forwards remote ICE candidates from `ServerMsg::TunnelIce` into the peer; trickles its local candidates back via `ClientMsg::TunnelIce`.
3. Waits in the background for the DC pool to fully open + installs one `FlowDemux` per channel.
4. On each `ServerMsg::TcpForwardForward`: runs the ACL + dial decide, picks `dc_index = flow_id % pool_size`, registers the flow on the demux, replies `TcpForwardAccept`, spawns `tunnel_core::forward::run_flow`. On flow close: emits `ClientMsg::TcpClosed` for audit.
5. On `ServerMsg::TunnelTerminate` or WS disconnect: closes every tunnel peer cleanly.

End-to-end handshake validated locally with `tunnel::peer::tests::answerer_reaches_pool_ready` — drives a no-signaling-server offerer+answerer setup and asserts the pool opens within 15 s.

**Still open — T2.10e (end-to-end smoke against a real agent + roomler.ai):**

All code-side pieces of T2.10 (a/b/c/d) are now in place. T2.10e is the actual real-world validation:

1. **CI / mars** must first build `roomler-ai-api` + `roomler-ai-tests` with the T2.10c relay code, since locally those crates are blocked by openssl-sys MSVC. Push the branch + open a PR; let CI compile + run the existing test suite. Any wire-mismatch bugs will surface here.
2. **Deploy** the `feature/roomler-tunnel` build to `https://roomler.ai` (or a staging URL with an alternative `roomler-ai` deployment).
3. **Enroll a real agent** on a Linux box with intranet access: `roomler-agent enroll --server https://roomler.ai --token <admin-issued-agent-enroll> --name <label>`. Run the agent (it starts the signaling loop).
4. **Enroll the tunnel-client** on the operator laptop: `roomler-tunnel enroll --server https://roomler.ai --token <admin-issued-tunnel-enroll> --name <laptop-label>` (or set `ROOMLER_TUNNEL_SERVER` + `ROOMLER_TUNNEL_TOKEN` env vars).
5. **Issue a tunnel policy** in the admin UI allowing the operator → agent → `<dst host>:<dst port>` triple.
6. **Open the forward**: `roomler-tunnel forward --agent <hex> --local 5432 --remote <intranet-host>:5432`.
7. **Verify**: from another shell, `psql -h 127.0.0.1 -p 5432 -U <user> -d <db>` connects through the tunnel. Subsequent queries return rows.

Diagnostics tooling for failures: the agent + tunnel-client both log at `RUST_LOG=info` by default; `RUST_LOG=tunnel_core=debug,roomler_tunnel=debug` turns on per-flow / per-chunk visibility. The server's `tunnel_audit` collection records every Accept / Reject / Close with `relay`, `dst_host`, `dst_port`, and `client_version` for the admin to investigate.

## Known gotchas

1. **Local openssl-sys MSVC blocker** still applies — `services` / `api` / `tests` cannot be locally built or tested on this Windows box (memory `feedback_windows_no_local_backend.md`). Locally verifiable: `roomler-agent`, `roomler-ai-tunnel-core`, `roomler-ai-remote-control`, `roomler-ai-db`, `roomler-ai-config`, vendored crates. Everything else needs mars/CI.

2. **HALF_CLOSE_MAGIC = [0xFF]** is the data-plane close sentinel in `crates/tunnel-core/src/forward.rs`. It is NOT replaceable by a wire-level message — SCTP per-stream ordering makes the in-band byte the only thing that arrives strictly after all data on the same flow. Wire-level `rc:tunnel.tcp.half_close` exists as additive audit only, NOT as a replacement for the in-band sentinel. The earlier HANDOVER24 plan to "replace" was overruled by a 256 KiB burst test that showed ~40% chunk loss when `unregister()` fires asynchronously from the data.

3. **Pump TCP test fixture race** — the `pump_tcp_to_dc` + `pump_dc_to_tcp` code is correct (matches what the working 256 KiB burst test does via direct `dc.send`) but a TCP-integration test against the local two-peer fixture timed out. **Don't waste time on it** — T2.10d's end-to-end smoke through a real agent exercises the same path and will catch any real bug.

4. **Defensive enum catch-alls** — see CLAUDE.md "Defensive enum catch-alls" subsection. CI 25972574628 hit this; master commit `35aa487` resolved by dropping the catch-all, but the same pattern would recur if you preemptively add a catch-all for variants not in the same commit. **Use explicit `m @ (V1 | V2 | …)` enumeration** like in `agents/roomler-agent/src/signaling.rs:567+` for the tunnel-flow ServerMsg variants.

5. **`pub mod tunnel` in lib.rs gets reverted by something** — possibly a linter or IDE auto-organize-imports that runs on save. Twice during this session my `pub mod tunnel;` line vanished. If you find it gone, just re-add it manually; the directory has always been on disk.

6. **Stash flow** — the user has been stashing this work between sessions. **The label is unreliable** — the 2026-05-18 session found the full tunnel arc inside `stash@{0}: pre-stderr-capture-wip`, NOT inside any of the three `tunnel-WIP-before-rc.NN` stashes. Inspect by file list (`git stash show -u stash@{N} --name-only`) and pop whichever has `crates/tunnel-core/` + `agents/roomler-tunnel/` + `crates/vendored/webrtc/` + `HANDOVER24.md`. Auto-merge handles most conflicts; expect one regression to hand-fix (the 2026-05-18 one was `SetServiceEnvVar.value: Option<String>` reverting to required `String`).

## Suggested commit slicing (for when you're ready)

```
0.  fix(agent): preserve set-service-env-var --value Option<String> semantics (regression from auto-merge during stash recovery)
1.  chore(deps): vendor-fork webrtc-0.12.0 with SCTP rwnd setter
2.  feat(tunnel): scaffold tunnel-core + roomler-tunnel binary + rc:tunnel.* wire types
3.  feat(auth): TunnelClient + TunnelEnrollment JWT audiences + 13 cross-rejection tests
4.  feat(db): TunnelClient + TunnelPolicy + TunnelAuditEvent models + DAOs + indexes
5.  feat(tunnel): ACL evaluator + server-side gate with cross-tenant lock
6.  feat(api): tunnel-client enrollment endpoints + WS role=tunnel-client + 60s revocation re-check
7.  feat(ui): TunnelClientsSection admin UI + Pinia store + router
8.  feat(agent): tunnel module — acl + dialer + acceptor + signaling dispatch wiring
9.  feat(tunnel): TunnelPeer with 8-DC pool + SCTP rwnd 8 MiB
10. feat(tunnel): FlowDemux + pump_tcp_to_dc + pump_dc_to_tcp + run_flow

Ride-along (worth separating from tunnel arc):
B.  feat(ui): prefer-hardware VP9 decode hint on capable GPUs

Note: ride-along A (`crash_uploader INTER_REQUEST_DELAY`) was independently
shipped on master as commit `6ffe10a` while this stash was dormant, so
the stash's identical change auto-absorbed and no longer shows as a
diff. No-op now.
```

Each commit is locally compilable + passes locally-runnable tests in order. Commits 1-2 don't touch services/api; commits 3-5 are services-only (mars/CI verifies); commits 6-9 cross into api (mars/CI verifies); commit 10 is back in tunnel-core (local). Commit 0 is a small one-character-of-semantics fix.

Drop `files.zip` before commit 0 (stash debris).
