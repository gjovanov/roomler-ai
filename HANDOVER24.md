# HANDOVER24 — rc.51 / rc.52 shipped, rc.53 plan + critique ready for next session

## Where we are

- **rc.51 + rc.52 shipped green** — SystemContext crash-loop end-to-end fix.
  - rc.51: supervisor counter bug (`ActiveWorker` struct + uptime-gated reset +
    `SessionChange` clears `respawn_at` + `RESPAWN_ALARM_THRESHOLD=8` loud alarm
    + crash-recorder `suppressed_since_last`). Stops the runaway respawn on every
    fielded host.
  - rc.52: machine-global `%PROGRAMDATA%\roomler\roomler-agent\config.toml`
    + `icacls` dir-ACL + `enroll --machine-global` + `re_enroll_cmd` preserves
    `machine_id` + Phase 4 self-heal on first healthy post-logon run.
- **PC55331 field investigation** — diagnosed end-to-end. The hours of WS-fail
  hunting (corporate proxy, TLS inspection, dueling instances) reduced to a
  single backend log line: `agent is quarantined or deleted; refusing WS
  agent_id=6a074fe5…`. MongoDB confirmed `deleted_at=2026-05-23T09:04:15Z`
  (admin-UI batch delete). The agent's local `config.toml` still held a
  cryptographically-valid token for that now-deleted row. Soft-delete is
  intentional (referential integrity + `rehydrate()` on re-enrol preserves
  identity); the miss is the **agent-side blindness** — no close-reason on the
  wire, so the operator can't see "this row was deleted" without backend log
  access.
- **Open user-facing issue at session end:** PC55331 operator is hitting the
  `%APPDATA%` ↔ `%PROGRAMDATA%` same-session asymmetry: `enroll
  --machine-global` writes `%PROGRAMDATA%` (machine_id `6c87c03b…`), but
  `roomler-agent run` from a user shell is a `WorkerRole::User` worker and reads
  `%APPDATA%` (machine_id `fa8e4ccb…` — the deleted row's). The rc.53 plan
  adds a Phase 7 stderr warning for this exact case (added during critique).

## Two artefacts ready for next session

### 1. rc.53 plan
`docs/plans/rc53-ws-close-reasons-and-corp-tls.md`

WS close-reasons (`AgentDeleted` / `ReplacedByNewerConnection` / `PolicyRejected`),
agent treats them as fatal, mirror of rc.32's native-cert-roots swap for
`tokio-tungstenite`. Original v0 was 6 phases; critique reshuffled to 7 (drop
Phase 6, add Phase 7).

### 2. Independent critique
The Plan agent returned **GO-WITH-CHANGES** with **15 concrete required changes** — see the transcript at the end of this handover. Highlights:

| # | Finding | Severity |
|---|---|---|
| 0 | **Plan points at the wrong file for Phase 2a.** The `agent deleted` refusal is at `crates/api/src/ws/handler.rs:189`, NOT `remote_control.rs`. Implementer would have edited the wrong file. | **HIGH** — wasted day |
| 2 | **`AGENT_DELETED_EXIT_CODE=7` is observably useless without rc.51 supervisor changes.** `decide_exit_reaction` branches only on `0` vs `!=0`. Code-7 needs alarm-immediately treatment AND `should_record_supervisor_crash` exclusion. | **HIGH** — silent feature |
| 3 | **`notify::set_attention(AttentionReason::ReEnrollRequired)` is fictional API.** Real API is `raise_attention(&str)`. Pick either (a) call existing with a hand-written string, or (b) budget the `notify.rs` refactor honestly (+50 LOC). | **HIGH** — won't compile |
| 1 | **Phase 2b displacement won't actually close the displaced WS.** `drop(prev.tx)` causes the pump to exit, but the read-loop keeps polling `socket_rx.next()`. Needs a `tokio::sync::Notify` or oneshot-cancel to signal the read-loop. | **MED** |
| 1+ | **Pre-existing displacement-vs-unregister race.** Displaced `handle_agent_socket` eventually `unregister_agent`s the NEW connection's entry, killing its sessions. Fix while Phase 2b is in the file. | **MED** |
| 8 | **Missing the same-session `%APPDATA% / %PROGRAMDATA%` asymmetry.** PC55331's current pain. Added as Phase 7 (~15 LOC stderr warn in `cmd_enroll`). | **MED-add** |
| 11 | **Phase 6 wizard UX is genuinely under-spec'd.** Recommend defer to rc.54. | **LOW-cut** |
| 5 | **Phase 5 `rustls-tls-native-roots` works**, but agent already has 3 TLS stacks (reqwest native-tls, tungstenite rustls-webpki, vendored webrtc-ice rustls-native). Worth landing + backlog "unify TLS in rc.54". | **LOW** |

(Full numbered list of 15 changes in the critique transcript below.)

## Recommended path for next session

1. **Read this handover + the rc.53 plan + the critique transcript** (in that order).
2. **Decide** whether to:
   - **a)** Have me revise the plan into v2 incorporating all 15 changes, then implement.
   - **b)** Implement straight from the critique-annotated v0 (the v0 plan is correct in shape; the 15 items are concrete enough to apply without a v2 rewrite).
3. **Implement rc.53 phases in this order** (based on critique re-ranking):
   - **Phase 1** wire format (`signaling.rs` `AgentCloseReason` + `ServerMsg::Goodbye` + serde tests including unknown-variant-graceful-default).
   - **Phase 4** back-compat smoke (`Err(e) => debug!(…)` path confirmed; add test that synthesises unknown-variant JSON).
   - **Phase 3** agent-side handling:
     - call `raise_attention(&str)` with multi-line operator message (NOT the fictional `set_attention(AttentionReason::*)`).
     - define `AGENT_DELETED_EXIT_CODE: i32 = 7` in `agents/roomler-agent/src/watchdog.rs`.
     - explicitly `close_all_peers + close_all_tunnel_peers` before `process::exit`.
     - sentinel path: `%PROGRAMDATA%\…\needs-attention.txt` for LocalSystem, `%APPDATA%` for user — log resolved path at WARN.
     - rc.51 supervisor update: code-7 fires alarm on first failure (not after 8), `should_record_supervisor_crash` excludes 7.
   - **Phase 2a** edits in `crates/api/src/ws/handler.rs:172-201` (NOT remote_control.rs; mirror at `:108-140` for tunnel-client path). Use `.await` on `send(Close)` for flush — no `sleep`.
   - **Phase 2b** in `crates/remote_control/src/hub.rs`: `try_send(Goodbye)`, log warn on Full, add `Notify`/oneshot to signal old read-loop to exit. Fix the displacement-vs-unregister race while you're there. Update log line at `:108-111`.
   - **Phase 7** (NEW) stderr warning in `cmd_enroll` when `--machine-global` + not running as LocalSystem.
   - **Phase 5** Cargo.toml swap + `cargo tree -e features -i webpki-roots` verification.
   - **Phase 6** DROP — defer to rc.54.
4. **Smoke matrix per critique #13** — including SM-1b (delete-with-active-WebRTC-session) and SM-5 (rc.53 server → rc.52 agent mid-session, no regression).
5. **Tag `agent-v0.3.0-rc.53`** as a single tag; ~350 LOC, ~10 new tests, 1 ED realistic.

## After rc.53 ships — backlog

- **rc.54 candidates** (in no particular order):
  - Wizard soft-delete-revive UX (deferred Phase 6) — spec `/api/agent/enroll/probe` endpoint properly, ~150 LOC backend + ~80 SPA.
  - Unify TLS stack — pick all-rustls (reqwest+tungstenite+webrtc-ice with native roots) or all-native-tls. Today 3 distinct TLS code paths in one binary.
  - `notify::AttentionReason` enum + structured sentinel JSON (so tray / admin UI can differentiate reasons).
  - `unregister_agent` race in Hub if Phase 2b only addresses it as a side-fix.

## Pending operator-territory items (carried from prior handovers)

- W10/W11 VM formal smoke matrix for rc.52 (SM-1…SM-7 from `docs/operator-systemcontext-smoke.md`).
- `WIN_CODESIGN_PFX_BASE64` GitHub secret still unset — all Windows artifacts ship `-unsigned`.
- PC55331 specifically: operator needs to either start the SCM service (to use the machine-global config they enrolled) OR re-enrol without `--machine-global` (so user-context `run` reads %APPDATA%). Doc has the matrix at section 6.

## Files of interest

- `docs/plans/rc53-ws-close-reasons-and-corp-tls.md` — the plan
- `docs/operator-systemcontext-smoke.md` — rc.52 operator smoke checklist
- `agents/roomler-agent/src/win_service/supervisor.rs` — rc.51's supervisor (counter + alarm)
- `agents/roomler-agent/src/main.rs` — rc.52 ladder + self-heal + enroll-cmd
- `crates/api/src/ws/handler.rs` — **Phase 2a edits land here** (critique #0)
- `crates/remote_control/src/hub.rs` — Phase 2b displacement
- `crates/remote_control/src/signaling.rs` — Phase 1 wire format
- `agents/roomler-agent/src/signaling.rs` — Phase 3 client-side Goodbye handling

## Critique transcript

Saved verbatim at **`docs/plans/rc53-critique.md`** — 15 numbered required
changes with file:line references, all verified against current source. Read
top-to-bottom; the items are ordered roughly by criticality.
