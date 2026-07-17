# Naming migration: `agent` → device / node (`roomlerd`)

Part of the node-stack unification (see the unification plan). The controlled-host
daemon `roomler-agent` is being renamed to **`roomlerd`**; a machine is a
**device** (UI) that joins the mesh as a **node** (wire/mesh). "Agent" no longer
describes the role (once tunnel + overlay fold in, the machine *reaches out*, not
just *gets controlled*) and collides with "AI agent".

**Rule of thumb:** rename the *surface* freely and now (UI, docs, installer,
desktop app, LocalAPI, CLI). Migrate *contracts* (things a field host or a stored token
depends on) only with a back-compat shim — never a big-bang, or you orphan the
installed fleet.

## Contract table

| Identifier | New | Migration |
|---|---|---|
| binary `roomler-agent` | `roomlerd` | rename at P3; ship `roomler-agent` alias 1 release |
| binary `roomler-tunnel` | `roomler` (CLI) | thin LocalAPI client; keep alias 1–2 releases |
| binary `roomler-agent-tray` | `roomler-desktop` (display "Roomler") | extend to the unified LocalAPI (both roles); the system-tray icon is just where it lives, not its name |
| **env `ROOMLER_AGENT_*`** | `ROOMLER_NODE_*` | **dual-read via `tunnel_core::env::node_env` — prefers `ROOMLER_NODE_<X>`, still honours `ROOMLER_AGENT_<X>`. Never drop the legacy prefix** (it's the MajorUpgrade-drops-env-vars bug — operators set these in the service Environment block). |
| service name `RoomlerAgent` | `Roomler` / `roomlerd` | installer stop-old / install-new |
| config `…/roomler-agent/config.toml` | `…/roomler/config.toml` | migrate-on-first-run OR read-both |
| **MSI UpgradeCode** | — | **PRESERVE** (changing it orphans every install) or a deliberate, tested MajorUpgrade |
| DB collection `agents`, wire `agent_id` | — | keep internal; UI relabels to "device". Not worth wire churn |
| JWT audience `Agent` / `Enrollment` | — | keep (enrolled tokens must stay valid) |
| API routes `/api/…/agent` | — | keep; add `/device` alias later if desired |
| UI `AgentsSection`, `agents` store | `Devices` | cheap, do on the surface pass |

## Done (P0)

- `tunnel_core::env::node_env(suffix)` — the dual-read helper.
- Migrated `ROOMLER_AGENT_OVERLAY_DIRECT` + `ROOMLER_AGENT_OVERLAY_QUIC`
  (`overlay/{direct,wg}.rs`) to `node_env` → operators can now set
  `ROOMLER_NODE_OVERLAY_*`, and existing `ROOMLER_AGENT_OVERLAY_*` keep working.

Remaining `ROOMLER_AGENT_*` reads (USE_FFMPEG, ENABLE_SYSTEM_SWAP, UNICODE_TEXT,
VP9_FPS, ENCODER, HW_AUTO, …) migrate to `node_env` in P3 alongside the binary
rename — same shim, no behaviour change.

## Done (P3d — rc.194)

- `[[bin]]` renames shipped: `roomlerd` / `roomler` / `roomler-desktop`
  (packages/libs unchanged). WiX ships `roomler-agent.exe` alias; tunnel zip
  ships `roomler-tunnel.exe` alias — both until the fleet crosses.
- Service takeover `RoomlerAgentService` → `Roomler` (create-new → delete-legacy;
  `resolved_service_name()` reads whichever exists). Scheduled Task + mutex same
  pattern. All 46 env reads on `node_env` dual-read.
- Config dirs: `appdirs` read-both (new `roomler` tree for fresh installs; upgraded
  hosts keep the old `roomler-agent` tree — copy-never-move, enrollment-safe).

## In flight (P4 — unified installer `roomler-setup`)

- **P4a** (MERGED, #117): `crates/roomler-setup-core` (lib `wizard_shared`) extracted
  from the two wizard crates — msi_runner / extract / integration / tunnel-enroll /
  asset_resolver relocated once, legacy wizards re-export path-compatibly
  (byte-identical behaviour); ONE unified `ProgressEvent` wire (tunnel tag style,
  live-union vocab) consumed only by the new `agents/roomler-setup` app (role picker:
  daemon perMachine-SCM / perUser-task / perMachine + tunnel-client).
- **P4b** (this branch, VM-gated): install folder renamed `roomler-agent\` → `Roomler\`
  in BOTH wxs (UpgradeCodes FROZEN — MajorUpgrade removes the old-folder product
  first). Both MSIs now carry the tunnel CLI `roomler.exe` + a PATH append
  (`TunnelExe` component) — Option A groundwork so a daemon install subsumes the CLI
  and the roomlerd MSI is the one updater. The rename rides the existing
  task/service/shortcut re-registration (all re-derive from `current_exe()`);
  config/`machine_id` are out-of-tree so enrollment survives. New belts:
  `cleanup-legacy-install` sweeps the vacated old-named dir (the only MSI-CA-reachable
  step — the cross-flavour arms are fast-path-gated), and the post-install watcher
  falls back to probing `…\Roomler\roomlerd.exe` so the rename hop reports
  `SucceededVerified`. `find_tunnel_binary` prefers `roomler(.exe)` over the legacy
  name. Backend `tunnel_wizard_release.rs` → `setup_release.rs` with dark
  `/api/setup/*` routes (tag `setup-v*`, asset prefix `roomler-setup-`). Terminal
  installers `scripts/install.{sh,ps1}` served live at `/api/setup/install.{sh,ps1}`
  drive the wizard's steps headlessly per OS.
- **P4c** (split, ship-then-retire): **P4c-1** ships the unified `roomler-setup`
  wizard for the first time — new `release-setup.yml` (3-platform, Windows
  signed-EXE-in-`.zip`), tag `setup-v*`, so the `/api/setup/*` routes (dark since
  P4b) go live; the two legacy wizard crates keep building as the safety net.
  **P4c-2** (gated on ≥1 real field onboarding via the unified wizard) atomically
  deletes both legacy wizard crates + `release-tunnel-wizard.yml` + the
  `roomler-installer` companions build. NB the tunnel CLI **self-update is KEPT** —
  it's the sole updater for tunnel-only hosts (the daemon roles get `roomler.exe`
  refreshed by the MSI; "one updater" is per-role, not literally one). Deferred to
  a later cleanup: dropping the legacy `/api/tunnel-wizard` route +
  `find_tunnel_binary`'s legacy fallback, once telemetry confirms no callers.
