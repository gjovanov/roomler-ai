# HANDOVER15 — rc.18 shipped; rc.19 plan = resumable file-DC transfers

> Continuation of HANDOVER14 (which closed at rc.7 with SystemContext
> field-validated). This session shipped four follow-on plans across
> rc.16 → rc.18, plus the Tauri tray companion, then hit a field bug
> in the file-DC's mid-upload-channel-close path that motivates the
> rc.19 cycle.

## State at session end

- **Master HEAD**: `f0e8a54` (workflow fix — `.exe` in flatten step)
- **Latest agent tag**: `agent-v0.3.0-rc.18` published 2026-05-10 23:38 UTC
- **Latest web image**: `registry.roomler.ai/roomler-ai:v20260511-5ce2ab39e461` live on prod
- **Field host**: PC50045 (e069019l), Win11 24H2, perMachine MSI
- **Lib tests**: 252 with default features
- **Vitest**: 130 tests in `useRemoteControl.spec.ts`

## What shipped this session

| Tag / image | Phase | What |
|---|---|---|
| (pre-session) | Plan 1 | Wire-format integration test in `agents/roomler-agent/tests/file_dc.rs` — 12 webrtc-rs loopback tests, no MongoDB |
| (pre-session) | Plan 2 | Pivoted from Playwright E2E → Vitest: extracted `nextDirPath` pure helper + 10 cases pinning the `\\?\C:\` regression |
| `agent-v0.3.0-rc.17` | Plan 3 | Operator-consent broker — `auto_grant_session` config flag (default true), sentinel-file decision flow, `roomler-agent consent` CLI subcommand |
| `agent-v0.3.0-rc.17` | Plan 4 | Mobile virtual keyboard — hidden 1×1px textarea + special-keys toolbar + IME compositionend flush + sticky modifiers; 15 Vitest cases |
| `agent-v0.3.0-rc.18` | Plan rc.18 P1 | perMachine UAC self-update via `ShellExecuteExW` + `/qb!`; manual self-update writes `last-install.json` |
| `agent-v0.3.0-rc.18` | Plan rc.18 P2 | Cross-flavour cleanup CLI (`cleanup-legacy-install`); both directions; reuses `system_context::user_profile::active_user_profile_root` |
| `agent-v0.3.0-rc.18` | Plan rc.18 P3 | WiX dropped cross-flavour LaunchCondition refusal; added `CleanupLegacy*` deferred CAs |
| `agent-v0.3.0-rc.18` | Plan rc.18 P4 | `config::migrate` + `config_schema_version` stamp; runs from `run_cmd` |
| `v20260511-5ce2ab39e461` | Plan rc.18 P5 | Auto Ctrl+C remote→browser clipboard mirror (25ms timer + `getAgentClipboard` + `writeText`) + nav-drawer focus-blur on pointerenter + capture-phase keydown |
| `agent-v0.3.0-rc.18` | Plan rc.18 P6-P7 | New `agents/roomler-agent-tray/` workspace member — Tauri 2.x onboarding + status SPA + system-tray icon |
| `agent-v0.3.0-rc.18` | Plan rc.18 P8 | `release-agent.yml` builds tray EXE alongside MSIs (Windows; macOS/Linux deferred) |
| `f0e8a54` | follow-up | Workflow flatten step now includes `*.exe` so future RCs auto-publish the tray EXE |

Per-RC behaviour unchanged for everyone NOT opting into the new
features: `auto_grant_session=true` default, perUser MSI's `/qn`
path unchanged, viewer's existing keystroke pipeline preserved.

## Field bug 2026-05-11 — what motivates rc.19

35 MB .xlsx upload from the browser → PC50045 failed at 4 %
(1,376,256 / 36,268,790 bytes):

> Upload failed: files channel closed mid-upload at 4%
> (1376256/36268790 bytes). Most likely the remote agent restarted
> (auto-update / crash / network drop). Reconnect and retry.

The error message itself comes from `useRemoteControl.ts::
channelClosedError`. The current file-DC v2 has NO retry / NO
resumption — a DC close mid-upload is permanent for that File.

**Why probably auto-update**: PC50045 had just been upgraded to
rc.17 manually + auto-update polls every 24 h. If the agent's
auto-update timer fires WHILE a transfer is in flight, the agent
spawns msiexec + exits, the WebRTC peer + file-DC die with it.

**Why probably not network drop**: WS reconnect ladder
(`RC_RECONNECT_LADDER_MS`) wasn't observed firing in the operator's
shell session — the browser stayed connected; just the file-DC
underneath went dead.

## rc.19 plan = resumable transfers

Decision recorded 2026-05-11: ship **resumable transfers** rather
than the cheaper retry-from-byte-0. User chose the bigger fix
because their use case (multi-MB xlsx) makes re-uploading from 0
unacceptable.

See `~/.claude/projects/C--dev-gjovanov-roomler-ai/memory/project_rc19_resumable_transfers.md`
for the full design sketch. Summary:

1. New wire envelope on the existing `files` DC:
   - `files:resume { id, offset, sha256_prefix? }` (browser → agent)
   - `files:resumed { id, accepted_offset }` (agent → browser)
   - Optional `files:chunk-ack { id, bytes }` every ~1 MB so the
     browser knows what the agent has persisted even before a drop.
2. Agent persists partial files under `<downloads>/.roomler-partial/
   <id>` (or similar); 24 h orphan-sweep at startup.
3. Browser tracks `bytesAcked` per upload; on DC close stashes
   `File + bytesAcked + retry budget`; on reconnect re-issues
   `files:resume` from `bytesAcked`.
4. Per-chunk SHA256 — optional in v1, locked in v2. A network glitch
   flipping bytes mid-stream goes undetected without it.
5. Capability handshake — agent advertises `caps.files += ["resume"]`
   on `rc:agent.hello`; a future browser-vs-agent mismatch falls
   through cleanly to the rc.18 "fail + reconnect" path.

**Also queue for rc.19**: defer agent auto-update while any
transfer is in flight (~50 LOC, sibling check on
`updater::run_periodic`). Pair with resumable transfers so even
unattended fleet updates don't kill long uploads.

## Files to touch next session

| File | Why |
|---|---|
| `agents/roomler-agent/src/files.rs` | partial-file persistence; offset-aware `begin` variant |
| `agents/roomler-agent/src/peer.rs` | `Resume` arm of `FilesIncoming`; `Resumed` + `ChunkAck` emit |
| `ui/src/composables/useRemoteControl.ts` | `bytesAcked` tracking; `files:resume` on reconnect |
| `agents/roomler-agent/src/updater.rs` | suppress auto-update spawn when transfers are active |
| `crates/remote_control/src/signaling.rs` | `caps.files` extension |
| `agents/roomler-agent/tests/file_dc.rs` | new resume round-trip + SHA mismatch + sweep tests |

## Field-validation TODO for rc.18 (independent of rc.19)

- [ ] Manual `roomler-agent self-update` from non-elevated PowerShell
      on perMachine: confirm UAC prompts, install completes, new
      binary reports `--version` rc.18.
- [ ] Cross-flavour install — drop a fresh perUser MSI on a
      perMachine host (or vice versa); confirm `cleanup-legacy-install`
      ran (msiexec log line / removed schtasks / removed service).
- [ ] Auto Ctrl+C over viewer — Ctrl+C in the remote's text editor →
      browser's local clipboard now holds the same text.
- [ ] Focus-blur — click Dashboard nav item, then connect to viewer,
      press Enter — nav-drawer doesn't navigate.
- [ ] Tray EXE — download from release page, run, paste a fresh
      enrollment token, confirm green icon.

## Known gaps not covered by rc.19

- Tray Linux build (rc.18 is Windows + macOS); Tauri bundle MSI/.pkg
  via `cargo tauri build` (today's tray artifact is plain EXE).
- SCM-service-driven BG auto-update for unattended perMachine
  fleets (where UAC consent isn't reachable).
- `getAgentClipboard` rejection fallback could open a tiny invisible
  textarea + select-all + `document.execCommand('copy')` to preserve
  the user-gesture chain on the click event — current fallback just
  shows the text in a snackbar.

## How to pick up next session

1. Read `~/.claude/projects/C--dev-gjovanov-roomler-ai/memory/
   project_rc19_resumable_transfers.md` first.
2. `/plan` a fresh `rc19-resumable-transfers.md` in `~/.claude/plans/`
   that elaborates the design above. Apply the planner-critique
   discipline (spawn an independent review before ExitPlanMode).
3. Implement, ship, tag `agent-v0.3.0-rc.19`.
