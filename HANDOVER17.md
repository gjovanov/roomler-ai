# HANDOVER17 — rc.19 shipped (agent + browser); web deploy + PC50045 smoke pending

> Continuation of HANDOVER16. The browser-side P4-P7 ended up shipping
> the same session as the agent-side rc.19. Tag pushed; release
> workflow is mid-build. K8s web deploy + manual PC50045 smoke are
> the two remaining items.

## State at session end

- **Master HEAD**: `8440a50` (chore(release): bump workspace to 0.3.0-rc.19)
- **Agent tag**: `agent-v0.3.0-rc.19` pushed 2026-05-11 — release-agent.yml workflow run **25678701269** queued at 15:09 UTC (perUser MSI + perMachine MSI + .deb + tray EXE; ~20-25 min to publish).
- **Web image on prod**: unchanged at `v20260511-5ce2ab39e461` (rc.18 baseline). Web deploy with the rc.19 browser changes is pending your mars tmux.
- **Lib tests**: 272 agent. **Integration**: 13 file_dc. **Frontend**: 397 Vitest. **Type check**: vue-tsc --noEmit clean. **Clippy + fmt**: clean.

## What shipped this session (8 commits)

| Commit | Phase | What |
|---|---|---|
| `0f109b1` | P0 | Wire format (`Resume`/`Resumed` variants), caps `"resume"`, `PARTIAL_REGISTRY` + `ACTIVE_TRANSFERS` skeletons, 7 serde-lock tests |
| `9ba6f81` | P1 | Agent staging (`<dest_dir>/.roomler-partial/<id>/`), `meta.json`, `sync_data` per 1 MiB (B2 fix), `sweep_orphans` pre-WS connect (B1 fix), unique_path-at-rename (M4 fix) |
| `a4be61f` | P2 | `resume_incoming` handler with 256 KiB-aligned truncate (B3 fix), upload-side Cancel arm |
| `ae9101e` | P3 | RAII `ActiveTransferGuard`, updater 1h-defer / 7-force gate (M3 fix) |
| `b23e9ea` | P6 | Wire-format integration test for unknown-id resume → error |
| `7a4fba2` | docs | HANDOVER16 + CLAUDE.md mid-cycle handover |
| `d17066f` | **P4-P5+P7** | Browser auto-resume: `useRemoteControl(agent)` + `supportsResume` + `innerPump` re-reads live channel (P0-3 fix) + `pendingResumePromises` Map (P0-4 fix) + 6-attempt retry loop + `'reconnecting'` Transfer status + Transfers panel `v-if` extended (P1-5 fix) |
| `8440a50` | release | Version bump to `0.3.0-rc.19`, CLAUDE.md status block flipped to "shipped" |

## Field bug 2026-05-11 — status

35 MB xlsx upload crashing at 4% (1.3 MB sent) when the agent's auto-update timer fired mid-flight. **rc.19 fixes both sides**:

- Agent: `ACTIVE_TRANSFERS` counter prevents the updater from firing while any transfer is in flight (defers up to 7h, forces after 7 consecutive 1h-cycle defers). Staging + sync_data per 1 MiB preserves the partial across hard kills.
- Browser: `uploadOneResumable` wrapper auto-issues `files:resume` on DC drop, re-pumps from the agent's accepted offset. 6-attempt budget matches the reconnect ladder. On budget exhaustion sends `files:cancel` so the staging dir doesn't leak.

Backward compat: rc.18 browsers see no `"resume"` cap and use the legacy fail-fast path. rc.19 browsers + rc.18 agents see no cap and also fall back to legacy.

## Remaining (P8 + web deploy)

### Web deploy (you trigger via mars tmux)

```bash
ssh mars
cd /home/gjovanov/roomler-ai && git pull
docker build -t registry.roomler.ai/roomler-ai:build-$$ .
TAG="v$(date +%Y%m%d)-$(docker images -q registry.roomler.ai/roomler-ai:build-$$ | head -c 12)"
docker tag registry.roomler.ai/roomler-ai:build-$$ registry.roomler.ai/roomler-ai:$TAG
docker tag registry.roomler.ai/roomler-ai:build-$$ registry.roomler.ai/roomler-ai:latest
docker push registry.roomler.ai/roomler-ai:$TAG
docker push registry.roomler.ai/roomler-ai:latest

cd /home/gjovanov/roomler-ai-deploy && git checkout master && git pull
sed -i "s|newTag:.*|newTag: $TAG|" k8s/overlays/prod/kustomization.yaml
git commit -am "chore(k8s): bump roomler-ai to $TAG"
git push

argocd app sync roomler-ai --grpc-web || curl -sI https://roomler.ai/health
```

### P8 smoke on PC50045

After the release workflow completes (~25 min from tag push), the perMachine MSI auto-update timer on rc.18 hosts polls every 24h. Wait or trigger manually:

```powershell
& 'C:\Program Files\roomler-agent\roomler-agent.exe' self-update
# UAC prompt → install → service auto-restarts to rc.19
& 'C:\Program Files\roomler-agent\roomler-agent.exe' --version
```

Verification scenarios (per the plan):
- [ ] Drop 35 MB xlsx; `Stop-Process -Name roomler-agent` at 4%; observe Transfers panel "Reconnecting 1/6" → "Running" → final SHA matches local.
- [ ] Repeat at 25%, 50%, 90% kill positions.
- [ ] Set `ROOMLER_AGENT_UPDATE_INTERVAL_H=0` env, start an upload, observe agent log "auto-updater: deferring — transfers in flight"; upload completes, then auto-update fires on next 1h cycle.
- [ ] Folder upload of 12 mixed-size files; kill agent during 3rd file; observe per-file resume on reconnect; all 12 SHAs match.
- [ ] Trigger 7 forced reconnects in a row → 7th transfer settles `error`; browser sends `files:cancel`; agent log shows staging dir removed (not waiting 24h sweep).
- [ ] Set system clock +25h, restart agent; observe `sweep_orphans` log "complete kept=N swept=M"; old `.roomler-partial/` dirs gone from Downloads.
- [ ] sync_data perf re-measure: re-time the 35 MB upload, confirm total overhead < 10% vs rc.18 baseline. If higher (chronic Defender host), bump `FSYNC_THRESHOLD_BYTES` from 1 MiB to 4 MiB in code review.
- [ ] rc.18 browser + rc.19 agent upload succeeds via legacy path; rc.19 staging dir IS used (always-on per cap) and cleaned post-completion.
- [ ] rc.19 browser + rc.18 agent (test by enrolling a host on rc.18): browser sees no `"resume"` cap, uses legacy path, "channel closed" error on DC drop unchanged.

## Files touched this session

### Agent (Rust)
- `Cargo.toml` — version 0.3.0-rc.18 → 0.3.0-rc.19
- `agents/roomler-agent/src/files.rs` — most of the rc.19 work
- `agents/roomler-agent/src/peer.rs` — `Resume`/`Cancel` arms in handle_files_control
- `agents/roomler-agent/src/encode/caps.rs` — `"resume"` in caps.files
- `agents/roomler-agent/src/updater.rs` — `decide_defer` + `run_periodic` gate
- `agents/roomler-agent/src/main.rs` — `sweep_orphans` pre-WS connect
- `agents/roomler-agent/src/config.rs` — `CURRENT_SCHEMA_VERSION` bump
- `agents/roomler-agent-tray/tauri.conf.json` — version bump
- `agents/roomler-agent/tests/file_dc.rs` — `Resume`/`Resumed` test enums + unknown-id integration test

### Browser (TypeScript + Vue)
- `ui/src/composables/useRemoteControl.ts` — composable arg + `supportsResume` + `UploadEntry` + `pendingResumePromises` + `innerPump` + `uploadOne` wrapper
- `ui/src/views/remote/RemoteControl.vue` — pass `agent` to composable, Transfers panel v-if, 'reconnecting' pill colour/label

### Docs
- `CLAUDE.md` — "Status at 0.3.0-rc.19" block; resumption note → HANDOVER17
- `HANDOVER16.md` — mid-cycle handover (kept as historical)
- `HANDOVER17.md` — this file
- `~/.claude/projects/.../memory/project_rc19_resumable_transfers.md` — mark fully shipped

## Known gaps not covered by rc.19 (v2 candidates)

- **Per-chunk SHA256 verification** — wire format already reserves `sha256_prefix` on `files:resume`; v1 agent ignores it. Adds bit-flip detection mid-stream.
- **Download-resume** (host → browser) — only upload-resume in rc.19. Would mirror the staging logic for downloads, with the browser persisting `bytesReceived` per active download.
- **Sidecar partial registry** at `%LOCALAPPDATA%\roomler\partial-index\` so non-Downloads `dest_path` uploads survive agent restart. Current rc.19 sweep only walks Downloads — non-Downloads partials are lost on agent restart (browser falls back to fresh begin with new id, agent's stale dir leaks until 24h orphan sweep).
- **DC-recreate full-resume integration test** — prototyped but deferred; webrtc-rs loopback SCTP teardown races on Windows. Could be picked up by either rewriting the harness with explicit teardown wait, or by adding a Playwright E2E test driving real browser DCs.

## How to pick up next session

1. Verify release workflow completed: `gh run view 25678701269 --log` or check https://github.com/gjovanov/roomler-ai/actions/runs/25678701269.
2. Confirm rc.19 MSIs are published at https://github.com/gjovanov/roomler-ai/releases/tag/agent-v0.3.0-rc.19.
3. Deploy web (see commands above; mars tmux).
4. Run P8 smoke scenarios on PC50045.
5. If smoke passes: declare rc.19 stable.
6. If smoke regresses: cut rc.20 hotfix based on field findings.
