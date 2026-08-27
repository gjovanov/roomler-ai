# FR-6: CI + release build-speed SLO — every lane ≤10 min warm, self-healing

**Status:** shipped + field-verified through 2026-08-26 (retroactive FR per the CLAUDE.md
standing rule — the program ran 2026-07-13 → 2026-08-26 across ~20 PRs). Tracking issue:
`FR-6` (#773) in gjovanov/roomler-ai/issues.

## Goal

A push to `master` (PR CI) and an `agent-v*` tag (release) must complete their gating
lanes in **≤10 minutes warm**, and the system must **self-heal**: no silent cold builds,
no cache state that a human has to notice and repair, and every degradation announcing
itself on the affected run's page. Baseline when the program started: releases took
**~60–69 min** (rc.176: 29m09s rebuilding FFmpeg from source per tag), PR CI drifted to
~30 min under mirror stalls.

## Root causes (field-evidenced, in the order they were peeled)

1. **FFmpeg/libvpx rebuilt from source per tag** — the vendored zips existed
   (`vendored-ffmpeg-8.1.2` release) but `build-windows` never consumed them.
2. **GitHub's 10 GB Actions-cache pool cannot host the release seeds** alongside the
   legitimately hot CI caches. Every scheme change only rotated the eviction victim:
   tag-ref saves are unrestorable by design; the env-hash key component flaps with the
   runner-image lottery (`0c34b334` vs `e76361e6`, same toolchain + lockfile, run
   30707448990); flat keys are immutable so refreshes silently skip-save; delete-based
   refreshes open absent-key races (rc.382 tagged inside one); LRU evicted a **verified**
   save 29 minutes later (rc.472, run 32892784757).
3. **Serial waste in the MSI job**: a debug-profile mismatched-feature link test (5m33s),
   tray/installer built serially (~9 min tail), `cargo-wix` compiled per tag.
4. **Non-cache axes**: Ubuntu-mirror apt stalls (25m47s in one step, run 32191779671),
   Windows-runner queue starvation during release bursts (70 min queue, rc.424), a CI
   step added without a lockfile change being permanently locked out of the immutable
   cache (`Run API unit tests`, +7 min/run), and a GUI smoke probe that hung forever the
   day the binary it launched stopped crashing (#659 → run 32738472108).

## Key design (as shipped, anchors verified 2026-08-27)

- **Seeds live on a rolling `seed-cache` RELEASE, not in the cache pool** —
  `.github/actions/seed-cache-restore/action.yml` + `seed-cache-save/action.yml`: two
  assets per family (`<fam>-cargo.tar.zst` + `<fam>-target.tar.zst`), `--clobber` is the
  keep-newest-1 retention, `--latest=false` (the fleet self-updater polls
  `/releases/latest`). No LRU, no budget, no reservations, repo-global (no ref scoping).
- **Reseed choreography** (`.github/workflows/release-agent.yml`): a `reseed` job
  dispatches an artifacts-only rehearsal after every successful tag; weekly cron +
  `seed-release-caches.yml` (paths: `rust-toolchain.toml`, `release-agent.yml`,
  `.github/actions/seed-cache-*/**`) re-seed on the events that reshape a seed.
  Rehearsals share a cancel-in-progress concurrency group; tag runs are never cancelled.
- **Nothing fails silently**: `seed-cache-save` verifies via the API that its assets
  exist and `::warning::`s the run page otherwise; `seed-cache-restore` warns on a cold
  TAG build; failed rehearsals file a GitHub issue (`seed-failure-alert` job); saves run
  under `if: always()` so a downstream failure still seeds.
- **Growth control**: accretion GC before the build (registry mtime sweep + all-or-nothing
  `target` drop over `SEED_TARGET_MAX_MB`, never file-level deletes under `target` —
  cargo fingerprints outlive outputs); `cache-janitor.yml` sweeps PR-ref/idle/tag-ref
  strays every 6 h.
- **PR CI** (`ci.yml`): Swatinem with `key: wf-<hash of ci.yml>` (workflow edits rotate
  the key — new steps can't be locked out), apt packages cached via
  `cache-apt-pkgs-action` (mirror out of the hot path), `timeout-minutes: 20`.
- **MSI job**: vendored FFmpeg/libvpx fetched from release assets (sha256-verified, exact
  legacy paths so baked `.pc` prefixes hold); FFmpeg link asserted inside `encoder-smoke`
  on the built EXE (the 5m33s cargo-test step is gone); tray/desktop companion in a
  parallel job; GUI smoke probes hard-bounded (background + `kill -9` after 5 s).

## Phase / wave table

| Wave | Date | Change | PR(s) |
|---|---|---|---|
| 1 | 07-13 | Vendored FFmpeg/libvpx consumption, release-profile link test, parallel companions, `save-if` on tag refs, Swatinem in ci.yml | #105→#108 (squash) |
| 2 | 07-23 | Smoke-side FFmpeg assert; weekly cron seed mode; toolchain-bump dispatcher | #161 |
| 3 | 07-30 | `cache-on-failure`; seed-failure-alert issue filer | #258 |
| 4 | 08-01 | Env-hash removed from keys; post-release reseed job; cache janitor | #266 |
| 5 | 08-15 | Lockfile-generation keys via local composites (rust-cache retired from the lane) | #488 |
| 6 | 08-19 | apt-package caching + 20-min CI timeout | #535 |
| 7–8 | 08-19/20 | Inline generation retirement; accretion GC (+ the #660-era fingerprint-safety hardening) | #537, #549 |
| 9 | 08-20 | Rehearsal concurrency (cancel-in-progress) | #556 |
| 10 | 08-21 | `wf-` hash in the CI cache key | #587 |
| 11 | 08-24 | Hard-bounded macOS GUI probe | #661 |
| 12 | 08-25 | Run-salted keys (reservation wedge) | #675, #677 |
| 13 | 08-26 | **Seeds → release assets** (+ shakeout: mktemp for tar paths, dispatcher watches composites, `contents: write` on the OIDC-narrowed Windows jobs) | #722, #725, #726, #728 |

## Acceptance criteria

- [x] Release lane restores its seeds from a store with no LRU/budget/reservations
      (field: `restored from: seed-agent-windows-…-X64 assets (2.0G target)`, 2026-08-26)
- [x] All five lanes publish + restore asset pairs (10 assets live on `seed-cache`)
- [x] A failed/cancelled rehearsal cannot leave the lane silently cold (alert issue +
      run-page warnings, field-proven by #683 and the 403 warning that found #728)
- [x] PR CI warm ≤10 min with a hard cap (4.9–7 min band since #587; `timeout-minutes: 20`)
- [x] No silent-save/skip path remains (verify-after-publish on every save)
- [ ] **First normal-delta `agent-v*` tag post-migration lands ≤10 min end-to-end** —
      pending the next tag; 2026-08-26's warm Windows execution was 18.3 min against a
      dozen-PR same-day delta + signing steps (see Open decisions)

## Open decisions / residual levers (all trade-offs, not waste)

- The warm Windows floor now includes per-release workspace rebuilds (`version.workspace`
  bump invalidates every crate) under the size-optimized `cgu=1` profile, plus Azure
  signing steps. If normal-delta tags still exceed 10 min: relax `cgu=1` in CI (undoes
  part of P3e's size wins), decouple the per-release version bump (touches self-update
  identity), or paid 8-core runners.
- Runner-pool queue time is outside repo control (observed 35–70 min during bursts);
  rehearsal concurrency caps our own contribution. A self-hosted Windows runner is the
  reserve option.

## Out of scope

- The tag-race double-runs during release bursts (release-cutting process: `ls-remote`
  before tagging).
- CI-lane cache sizing owned by other programs (`ci-integration`, `ci-ffmpeg-encoder`…) —
  the pool is theirs now; the janitor governs it.

## Field-verification log

- 2026-07-14: rc.181/182/183 at ~21 min vs rc.180's 69 (wave 1).
- 2026-07-23: rc.210 9m08s / rc.211 8m25s (waves 2–4 steady state).
- 2026-08-16: `Cache hit for restore-key: …lock-f9806b32…` — prefix generations working.
- 2026-08-26: all 10 assets live; Linux warm-from-assets 5.5 min, macOS 6.1 min; Windows
  warm restore verified (2.0 G target), execution 18.3 min on a dozen-PR delta.
- Next: first normal-delta tag → check the run's warnings (none expected) and total time.
