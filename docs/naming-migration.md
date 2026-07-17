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

- **P4a**: `crates/roomler-setup-core` (lib `wizard_shared`) extracted from the two
  wizard crates — msi_runner / extract / integration / tunnel-enroll / asset_resolver
  relocated once, legacy wizards re-export path-compatibly (byte-identical
  behaviour); ONE unified `ProgressEvent` wire (tunnel tag style, live-union vocab)
  consumed only by the new `agents/roomler-setup` app (role picker: daemon
  perMachine-SCM / perUser-task / perMachine + tunnel-client).
- **P4b**: role→action matrix; both wxs gain `roomler.exe`; folder rename
  `roomler-agent\` → `Roomler\` (UpgradeCodes FROZEN); backend `setup_release.rs`
  (tag `setup-v*`).
- **P4c**: retire both wizard crates + `release-tunnel-wizard.yml`; `release-setup.yml`;
  fold the tunnel CLI self-update into the roomlerd MSI (one updater).
