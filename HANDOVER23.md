# HANDOVER23 — rc.28 → rc.32 ship cycle + in-flight tunnel work

> rc.28 install wizard shipped, rc.29 hotfixed within the hour
> (perMachine flavour misroute), rc.30 closed two field-reported
> wizard bugs (SystemContext early-error + Done-page blank-screen),
> rc.31 added TURNS/TCP relay for UDP-blocked corporate hosts, and
> rc.32 fixed the TLS-inspected-proxy `UnknownIssuer` failure that
> surfaced on the first rc.31 field test. Five tags pushed in 26 h.
> A substantial `roomler-tunnel` workspace member is in-flight from
> a parallel session — staged + untracked, NOT in this cycle.

## Ship status snapshot

| Tag | What shipped | GitHub Release |
|---|---|---|
| `agent-v0.3.0-rc.28` | Install + onboarding wizard EXE (Tauri 2, lib `wizard_core` to dodge UAC heuristic) + rc.27 backend (`/api/agent/installer/{flavour}` proxy + `/health`) | ✓ 2026-05-15 19:51Z |
| `agent-v0.3.0-rc.29` | B6 hotfix: thread operator flavour into `spawn_installer_for_flavour` so perMachine doesn't auto-route to perUser | ✓ 2026-05-15 20:34Z |
| `agent-v0.3.0-rc.30` | Two field-reported wizard fixes: SystemContext early-error removed (`6d73f80`) + Done-page blank-screen fixed (`5d3c97d`, camelCase arg mismatch + defensive renderDone) | ✓ 2026-05-15 21:58Z |
| `agent-v0.3.0-rc.31` | Vendored webrtc-ice 0.12.0 with TURNS/TCP relay path (upstream NOT_PLANNED). `crates/vendored/webrtc-ice/` + `TcpTurnConn` Conn-adapter + ChannelData 4-byte alignment per RFC 5766 §11.5. 10 framer tests + 96 upstream tests green | ✓ 2026-05-15 22:02Z |
| `agent-v0.3.0-rc.32` | TURNS/TCP `UnknownIssuer` fix: load OS-native cert store (Windows Schannel store carries IT's TLS-inspection CA) before adding webpki-roots. `rustls-native-certs = "0.8"` added to vendored webrtc-ice Cargo.toml | ✓ tagged 22:20Z; release workflow in flight (~3m50s in at handover) |

**Prod backend** (`https://roomler.ai/health` at handover time): still serving `0.3.0-rc.28`. Installer proxy resolves correctly — `/api/agent/installer/peruser/health` reports `agent-v0.3.0-rc.31` (latest cached GitHub Release). The pod-restart-busts-cache trick from rc.29 still works if a redeploy is needed to surface rc.32 health metadata.

## Two field bugs closed in rc.30

### Bug 1: SystemContext install error-early (`6d73f80`)
Operator picked `permachine-system-context` → wizard returned "Install failed" before spawning msiexec. Cause: `install_orchestrator::run_install` had a guard arm that bailed out for the SystemContext flavour pending the planned automated SCM env-var + service restart path.

Fix: removed the early-return. Now the orchestrator runs the plain perMachine MSI install end-to-end and emits a `PreflightWarning` explaining the deferred follow-up. The Done page surfaces the elevated PowerShell snippet (`roomler-agent set-service-env-var --name ROOMLER_AGENT_ENABLE_SYSTEM_SWAP --value 1 && roomler-agent restart-service`).

### Bug 2: Done page blank for plain perMachine (`5d3c97d`)
Operator successfully installed perMachine; Done step rendered blank with no Close button. Root cause: Tauri 2 changed default command-arg casing from v1 camelCase → v2 snake_case. JS called `invoke('cmd_install', { deviceName, onEvent })` while the Rust handler signature was `device_name: String, on_event: ipc::Channel<...>`. Silent deserialise-failure → `device_name = ""` + channel `null` → orchestrator emitted no progress → SPA never advanced → Done step never painted.

Fix three-pronged:
1. `#[tauri::command(rename_all = "camelCase")]` on `cmd_install` (back-compat with v1 JS-side calls).
2. Defensive `renderDone()` — accepts both snake_case and camelCase return shapes, falls back to placeholders, never short-circuits.
3. `runInstall` now paints Done step FIRST (`state.step = "done"; render()`) then `persistState().catch()` in background. Global `window.onerror` + `unhandledrejection` handlers + 30s sticky snackbar surface any silent failure to the operator immediately.

## rc.31 / rc.32 — TURNS/TCP for corporate-firewalled hosts

### rc.31: vendor webrtc-ice
Field problem on PC55331 (rc.28): host blocks ALL outbound UDP incl. UDP/443. webrtc-ice 0.12.0's `gather_candidates_relay` only handles `turn:HOST:port` UDP — `turn:HOST?transport=tcp`, `turns:`, all fall through to `log::warn!("Unable to handle URL")` and return zero candidates. Upstream issue webrtc-rs/webrtc#690 closed NOT_PLANNED 2026-01-31.

Fix: vendored upstream 0.12.0 verbatim except:
- `src/agent/agent_gather.rs`: added TURNS-over-TLS-over-TCP branch.
- `src/agent/mod.rs`: declares `tcp_turn_conn` submodule.
- `src/agent/tcp_turn_conn.rs`: NEW Conn-trait adapter that frames a `tokio_rustls::TlsStream<TcpStream>` as one STUN/ChannelData frame per `recv_from()` call (what `turn::client` expects).
- `Cargo.toml`: `tokio-rustls 0.26` + `webpki-roots 0.26` added.

Wired via `[patch.crates-io]` in workspace root `Cargo.toml`. 10 framer unit tests + 96 unchanged upstream tests green.

### rc.32: load OS-native cert store
Field test of rc.31 on PC55331: TURNS/TCP handshake failed with `invalid peer certificate: UnknownIssuer`. Corporate proxy doing TLS inspection — presents cert signed by private CA pushed into the Windows cert store. Browsers + reqwest (Schannel) trust it; webpki-roots-only build did not.

Fix: `rustls-native-certs = "0.8"` added to vendored webrtc-ice `Cargo.toml`. `tls_client_config` extends `RootCertStore` with OS-native certs first, then adds webpki-roots. Native-load failures non-fatal (Linux without `ca-certificates` falls through to Mozilla bundle).

## In-flight: roomler-tunnel workspace member (NOT in this cycle)

Working tree at handover has substantial untracked + staged work for a NEW `roomler-tunnel` subsystem — clearly a parallel session's work-in-progress. It is NOT tagged into rc.32 and NOT shipped to prod. The relevant artifacts:

**Untracked**:
- `agents/roomler-tunnel/` — new agent binary
- `crates/tunnel-core/` — new crate
- `crates/vendored/webrtc/` — another vendored upstream (SCTP `a_rwnd` tunability — see workspace Cargo.toml lines 49-57 for the patch notes)
- `crates/api/src/routes/tunnel.rs`
- `crates/api/src/ws/tunnel.rs`
- `crates/services/src/dao/tunnel_audit.rs`, `tunnel_client.rs`, `tunnel_policy.rs`
- `ui/src/components/admin/TunnelClientsSection.vue`
- `ui/src/stores/tunnelClients.ts`

**Staged** (modifies wiring to expose the tunnel routes/ws):
- `Cargo.toml`, `Cargo.lock` — workspace members + dependency updates
- `crates/api/{Cargo.toml, src/lib.rs, src/routes/mod.rs, src/state.rs, src/ws/handler.rs, src/ws/mod.rs}`
- `crates/db/src/indexes.rs`
- `crates/remote_control/src/{models.rs, signaling.rs}`
- `crates/services/src/{auth/mod.rs, dao/mod.rs}`
- `ui/src/{plugins/router.ts, views/admin/AdminPanel.vue}`

**Unresolved 3-way merge**: `crates/vendored/webrtc-ice/Cargo.toml` — working-tree copy is clean (no `<<<<<<<` markers) and looks correct (includes the rc.32 `rustls-native-certs` dep). The file just needs to be `git add`'d to clear the unmerged state. No MERGE_HEAD exists — likely a stash-apply or partial merge left this hanging.

**Next-session decision** (the parallel session may already have a plan): ship the tunnel work as its own cycle (rc.33+ or a separate `tunnel-v0.1.0` tag), or bundle it. Either way, this handover does NOT touch the staged work.

## Still pending / known caveats

- **W10 manual smoke** on a fresh Win11 VM — wizard EXE has been used in the field (operator drove it twice, both bugs surfaced + fixed in rc.30), but the formal S1-S10 checklist from HANDOVER22 has not been ticked off scenario-by-scenario. Operator-territory.
- **Authenticode signing** — `WIN_CODESIGN_PFX_BASE64` GitHub secret still unset; all rc.28-32 Windows artifacts ship with `-unsigned` filename suffix. Corporate AV may quarantine. Pending operator action.
- **SystemContext mode full automation** — rc.30 surfaces the manual PowerShell snippet on the Done page. Full automatic SCM env-var write + service restart needs a clean self-elevation path that doesn't surface a second UAC prompt mid-flow. Tracked for a future cycle (likely tied to a WiX custom action gated on `ENABLE_SYSTEM_CONTEXT=1` MSI property).
- **Prod backend version** — `/health` still reports `0.3.0-rc.28`. Cosmetic; functionally rc.28 + rc.29 + rc.30 backend bits are identical (rc.29 + rc.30 were agent/wizard-only). rc.31 + rc.32 are agent-only (vendored webrtc-ice). No prod redeploy is functionally required, but a tag-bump to surface `0.3.0-rc.32` in `/health` is nice-to-have.
- **Tunnel work in flight** (see prior section) — not in this cycle, decision needed next session.
- **rc.32 release workflow** still in flight at handover (3m50s in of typical 26m). Verify completion before claiming rc.32 fully shipped.

## How to pick up next session

1. **Read this file first**, then `HANDOVER22.md` for the rc.28 cycle base.
2. **Verify rc.32 release workflow finished green**: `gh run list --workflow release-agent.yml --limit 1`. If failed, debug + ship rc.33 hotfix. If green, confirm 4 Windows artifacts (perUser MSI, perMachine MSI, tray EXE, installer EXE) on the GitHub Release page.
3. **Optional cosmetic redeploy**: `/health` reports the latest tag. Per `reference_deploy_workflow.md` — tmux `deploy` session on mars → `docker build` → tag-bump kustomize → ArgoCD reconciles in ~5s.
4. **Decide on tunnel work**: read `agents/roomler-tunnel/`, `crates/tunnel-core/`, `crates/api/src/routes/tunnel.rs` to understand scope. Resolve the unmerged `crates/vendored/webrtc-ice/Cargo.toml` via `git add` (working-tree copy is already correct). Then either commit + ship as its own cycle or fold into a multi-feature release.
5. **W10 formal smoke** (if not already done) — distribute the rc.32 installer EXE to operators, walk S1-S10 from HANDOVER22.

## Operator install walkthrough (for support / docs)

1. Download `roomler-installer-0.3.0-rc.32-x86_64-pc-windows-msvc-unsigned.exe` from the latest GitHub Release.
2. Double-click. Wizard opens (no UAC prompt for the wizard itself — UAC fires only when picking perMachine + msiexec spawns).
3. **Welcome step**: pick deployment mode (perUser / perMachine / perMachine + SystemContext). If a prior install is detected and you're switching flavours, ack the "enrollment will be lost" checkbox.
4. **Server step**: leave `https://roomler.ai` as-is unless on staging.
5. **Token step**: paste the enrollment JWT from the admin UI. Wizard shows "Valid; issuer roomler-ai; expires in N min".
6. **Install step**: streams MSI from `roomler.ai/api/agent/installer/{flavour}`, verifies SHA256, spawns msiexec (UAC fires here for perMachine), waits + decodes exit code, enrolls. Live progress bar.
7. **Done step**: shows agent_id + tenant_id. If SystemContext mode, copy the PowerShell snippet from the Done page, run elevated, restart the agent service.

If the wizard is force-killed mid-flow, relaunching resumes on the same step with form fields pre-filled (token is NEVER persisted — operator re-pastes if needed).

## Files touched in rc.28-32 cycle

Top-level overview only — `git log --stat agent-v0.3.0-rc.28..agent-v0.3.0-rc.32` is authoritative.

- `agents/roomler-installer/**` — entire workspace member (rc.28 W0-W11)
- `agents/roomler-agent/src/{install_detect/**, jwt_introspect.rs, win_service/environment.rs, updater.rs}` — rc.27 foundation
- `crates/api/src/routes/agent_release.rs` — `/api/agent/installer/{flavour}` proxy
- `crates/vendored/webrtc-ice/**` — rc.31 + rc.32 vendored fork
- `Cargo.toml`, `Cargo.lock` — workspace member + `[patch.crates-io]` for webrtc-ice
- `.github/workflows/release-agent.yml` — added installer EXE build matrix + signing step

## Memory hygiene

Add a memory entry for rc.31/32 TURNS/TCP shipped (the existing `project_rc31_turns_tcp.md` entry says "vendored work in progress" — update to "shipped + native-cert fix in rc.32"). The existing `reference_webrtc_rs_turn_gap.md` entry can stay as-is — it documents the upstream-gap rationale.
