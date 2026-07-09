# Naming migration: `agent` → device / node (`roomlerd`)

Part of the node-stack unification (see the unification plan). The controlled-host
daemon `roomler-agent` is being renamed to **`roomlerd`**; a machine is a
**device** (UI) that joins the mesh as a **node** (wire/mesh). "Agent" no longer
describes the role (once tunnel + overlay fold in, the machine *reaches out*, not
just *gets controlled*) and collides with "AI agent".

**Rule of thumb:** rename the *surface* freely and now (UI, docs, installer, tray,
LocalAPI, CLI). Migrate *contracts* (things a field host or a stored token
depends on) only with a back-compat shim — never a big-bang, or you orphan the
installed fleet.

## Contract table

| Identifier | New | Migration |
|---|---|---|
| binary `roomler-agent` | `roomlerd` | rename at P3; ship `roomler-agent` alias 1 release |
| binary `roomler-tunnel` | `roomler` (CLI) | thin LocalAPI client; keep alias 1–2 releases |
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
