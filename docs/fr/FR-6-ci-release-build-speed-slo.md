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
- **PR CI** (`ci.yml`): Swatinem, keyed on the environment and the lockfile. A member
  crate's `Cargo.toml` edit or a third-party `Cargo.lock` change rotates a family's key,
  and the next run restores the previous generation as a prefix match. A version bump
  rotates nothing (wave 15). Since wave 16 a job's own changed or added steps rotate
  that job's key too: `.github/actions/cargo-cache-salt` writes a comment-only
  `.cargo/config.toml` holding a hash of the job's definition, which the lockfile part
  of the key covers, so the restore still falls back to the previous generation. apt
  packages are cached via `cache-apt-pkgs-action` (mirror out of the hot path). Since wave 14 the Rust work runs as **five parallel lanes**
  (`rust-lint`, `rust-unit`, `rust-agent`, `rust-recorder`, `rust-recorder-audio`),
  each with its own cache family and the same system packages from one composite
  (`.github/actions/ci-linux-system-deps`); an aggregate job keeps the name
  "Rust checks". Every Swatinem block that runs on PRs **saves only from master**
  (`save-if`), `CARGO_PROFILE_DEV_DEBUG=line-tables-only` shrinks compile time and cache
  size, a `concurrency` group cancels a PR's superseded run, and `cache-janitor.yml`
  deletes any cargo cache on a PR ref and keeps one generation per family on master,
  running after every master CI run as well as on its cron.
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
| 10 | 08-21 | `wf-` hash in the CI cache key (a no-op, found in wave 15: rust-cache ignores `key` when `shared-key` is set) | #587 |
| 11 | 08-24 | Hard-bounded macOS GUI probe | #661 |
| 12 | 08-25 | Run-salted keys (reservation wedge) | #675, #677 |
| 13 | 08-26 | **Seeds → release assets** (+ shakeout: mktemp for tar paths, dispatcher watches composites, `contents: write` on the OIDC-narrowed Windows jobs) | #722, #725, #726, #728 |
| 14 | 09-27 | **PR CI regression**: the "Rust checks" monolith split into five parallel lanes + an aggregate; PR runs write no cargo caches (`save-if` in `ci.yml`, `integration-tests.yml`, `installer-smoke.yml`); `line-tables-only` debuginfo; PR-run `concurrency`; janitor sweeps PR-ref cargo caches and superseded master generations | #1743 |
| 15 | 09-28 | Actions-cache storage limit 10 → 20 GB (the operator, paid); the dead `key: wf-…` inputs removed, and the comments that relied on them corrected | #1772 |
| 16 | 09-29 | `cargo-cache-salt`: each of the 14 PR-facing Swatinem jobs (`ci.yml`, `integration-tests.yml`, `installer-smoke.yml`) salts its key with a hash of its own definition, so a new step's dependency builds reach the cache on the next master run (root cause 4, for real this time) | #1784 |
| 17 | 10-01 | The Actions budget set to $10 (the operator): above the free 10 GB the pool had been **read-only** since the 20 GB raise. And the profiles job's SFU build deps (cmake + libclang) come from the lanes' cached apt sets instead of a raw `apt-get` against the mirror | #PR |

## Acceptance criteria

- [x] Release lane restores its seeds from a store with no LRU/budget/reservations
      (field: `restored from: seed-agent-windows-…-X64 assets (2.0G target)`, 2026-08-26)
- [x] All five lanes publish + restore asset pairs (10 assets live on `seed-cache`)
- [x] A failed/cancelled rehearsal cannot leave the lane silently cold (alert issue +
      run-page warnings, field-proven by #683 and the 403 warning that found #728)
- [x] PR CI warm ≤10 min with a hard cap — held at 4.9–7 min from #587, then
      **regressed** to 18–21 min warm and 35–45 min cold over 58 runs by 2026-09-27
      (the single "Rust checks" job had grown to ~35 serial steps, and PR-ref cache saves
      were evicting master's). Wave 14 restored it (field, run 36348497216 on #1745): the
      **whole CI in 6 min 21 s**, `Rust checks` critical path 5.8 min, every lane
      restoring master's cache with a full match; each lane has `timeout-minutes: 30`.
- [x] PR runs write no cargo caches, and master holds one generation per Swatinem
      family (wave 14; checked after a day of PR traffic). Field, 2026-09-28T21:26Z,
      25 h after the merge: 20 PR runs, and no `v0-rust-*` entry on any PR ref. None
      of the janitor's 22 sweeps in that window deleted one as `pr-cargo`, so none was
      ever written, not merely swept. Each of the 12 master families held exactly one
      generation; the janitor had retired 7 superseded ones
- [ ] A changed or added CI step's dependency builds reach its job's cargo cache on the
      next master run, without a cold run (wave 16). Field: the first master run after
      the salt lands saves a new generation for every salted job from a prefix restore
      (`full match: false`, not `No cache found`), and a later edit to one job's steps
      rotates that job's key alone
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
- **Resolved 2026-09-28: the cache pool was at capacity on master alone** (wave 14
  finding). With PR-ref saves gone, master's own families add up to ~11.6 GB: the five
  Rust lanes 5.6 GB (vs the single 4.1 GB `ci-linux` they replaced, since shared deps
  now sit in several families), `ci-integration` 1.1 GB, and the rest ~0.4–0.8 GB each.
  The pool read 10.2 GB right after seeding, and the first victims were the small,
  early-saved entries: Windows clippy, recording-ffmpeg, the apt sets. The expectation
  that LRU would keep the lanes warm (every PR restores them) and push the churn onto
  rarer jobs did not hold: over the next day the lanes themselves were evicted (lint
  overnight, unit at midday, agent and both recorder lanes in the evening), and each
  eviction cost the next PR a cold lane (log, 2026-09-28). The operator raised the
  repo's limit to **20 GB** (paid; `PUT repos/{repo}/actions/cache/storage-limit`
  answered 402 until the account had a payment method). ⚠️ The limit alone was not
  enough: with the Actions budget at its default, the pool above the free 10 GB was
  read-only from 09-28 21:37Z until the operator set a $10 budget on 10-01 (log).
  Trimming families (e.g.
  `recorder` + `recorder-audio` sharing one) stays in reserve. The registry copy each
  family carries is only ~167 MB for the whole lockfile, so deduplicating it is not
  the lever.
- **Resolved 2026-09-29 by wave 16, field check owed: a new CI step's dependency builds
  stayed out of the cache until the next key rotation** (wave 15 finding; root cause 4
  open again). The salt below shipped as `.github/actions/cargo-cache-salt`, hashing
  the calling job's own definition rather than the whole of `ci.yml`, so an edit to
  one job rotates that job alone. Wave 10 meant a `ci.yml`
  edit to rotate the key, but rust-cache ignores `key` when `shared-key` is set, so the
  key moves only with a member `Cargo.toml`, a third-party `Cargo.lock` change or the
  toolchain. Master saw such a change on 11 of the 30 days to 09-28, with one gap of
  17.3 days (09-08 → 09-25). A step added inside such a gap recompiles its new
  dependency builds on every run until the gap ends, as `Run API unit tests` did on
  08-21 (+7 min per run). Levers: bump `prefix-key` by hand (one cold master run per
  lane), or salt the lockfile part of the key. rust-cache hashes every
  `.cargo/config.toml` under the workspace byte for byte (`src/config.ts`, v2.9.2), so
  a CI step that writes a comment-only one outside any directory cargo runs from,
  holding a hash of `ci.yml`'s non-comment lines, would rotate the key on a real
  workflow change and still restore the previous generation as a prefix match. That
  relies on a rust-cache implementation detail; if it changes, the salt is ignored and
  the lane behaves as it does today.

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
- 2026-09-27: **PR CI regression found.** Runs 36331413828 (20.1 min, warm) and
  36339984289 (36.8 min, cold) computed the SAME key
  `v0-rust-ci-linux-Linux-x64-2b670344-977c0b24`; the first restored it with a full
  match at 15:58, the second got `No cache found` at 18:15. The pool read 15.0 GB
  against its 10 GB limit, 13.1 GB of it on `refs/pull/*/merge`: two concurrent PRs
  had each saved ~9–10 GB across eight job caches, and LRU evicted master's. On the
  warm run, test execution was ~3.5 of the 20 minutes; the rest was `roomlerd` and
  friends compiled in ~12 feature configurations, one after another. Wave 14 plan and
  numbers: #773.
- 2026-09-27: **Wave 14 (#1743) cold numbers.** Its own PR runs are cold by
  construction, since every lane is a new cache family and the new
  `CARGO_PROFILE_DEV_DEBUG` moves the env hash (the edit to `ci.yml` itself rotates
  nothing; see 2026-09-28): the `Rust checks` critical
  path fell from 36.8 to ~17 min cold and the whole CI from 36.9 to 18.2 min (run
  36345672545). Cold runner variance is ~30%: identical lint steps took 316 s and
  426 s on two runners. The first master run after the merge seeded every lane
  (36346857140, 21.8 min cold), and the janitor fired from its new `workflow_run`
  trigger 2 s after it finished. The pool then held one generation per family, all
  on master. A wrong turn, recorded: the first layout put the integration type-check
  in the unit lane, where it re-checked ~800 dependencies from scratch (369 s against
  31 s beside the clippy whose check-mode artifacts it reuses).
- 2026-09-27: **Wave 14 warm result** (#1745, run 36348497216, the first PR after the
  seeding). **Whole CI 6 min 21 s** (was 20+ warm, 36.8 cold). Lanes: lint 5.8, unit
  3.4, agent 4.3, recorder 4.7, recorder + audio 3.8 min. Each restored master's cache
  with a full match. Also Windows recorder 6.0 (was 7.3), macOS 3.0 (was 4.6), profiles
  1.6 min. The PR wrote no `v0-rust-*` cache. The pool read 10.2 GB: see the capacity
  item under Open decisions.
- 2026-09-28: **Wave 14 over a day** (every CI run from the merge to 21:05Z: 20 PR
  runs, 19 on master; timings are first attempts). PR runs: whole CI ≤10 min in 18 of
  20, median 7.3 min; `Rust checks` median 5.7 min. Five PR runs had a lane restore
  nothing, getting `No cache found` on a key master had saved hours earlier: lint at
  05:44, unit at 12:55 and 13:06, agent and both recorder lanes at 20:04 and 20:18. The
  two over 10 min were the big lanes cold (lint 17.7 min, unit 15.0 min); the smaller
  lanes stayed under 10 even cold. Cause: master's own families (~11.6 GB) against the
  10 GB limit; the janitor measured the pool at 9.6–11.7 GB before its sweeps. No PR
  run wrote a cargo cache (acceptance criteria). One PR run's first attempt failed in
  `ffmpeg-encoder` on a flaky test and passed on re-run; unrelated to caching.
- 2026-09-28: **Limit 10 → 20 GB** at 21:04Z by the operator (paid), read back as
  `{"max_cache_size_gb":20}`. Pool at 21:26Z: 10.6 GB in 24 entries, every family
  present except `ci-integration`, which its next master run re-saves. Still owed: a
  day of PR runs with no `No cache found` on a key master saved.
- 2026-09-28: **Correction: wave 10's `wf-` key never worked** (#773). rust-cache
  reads `key` only when `shared-key` is unset (`src/config.ts`, v2.9.2, the release
  `@v2` resolves to), and every lane's log shows the key it computed with no trace of
  the `wf-…` input. Field: #1728, #1735 (+95 lines) and #1736 each edited `ci.yml` on
  09-27, and each master run then restored `v0-rust-ci-linux-…-977c0b24` with a full
  match and saved nothing. The lockfile hash is narrower than its name: rust-cache
  blanks member versions and skips the workspace's own `Cargo.lock` entries, so four
  version bumps (0.4.108 → 0.4.113) kept `ci-lint`'s key, while #1741 and #1752 (a
  dependency each in `crates/api/Cargo.toml`) and #1744 (a feature in
  `agents/roomlerd/Cargo.toml`) each rotated it. #587 added its `key` beside an
  existing `shared-key: ci-linux`, so it was dead from the first run. The likeliest end
  of the 08-21 lockout is #590, which changed a member manifest 42 min after #587
  merged (an eviction and re-save would also have refreshed the entry). A
  wrong turn, recorded: the first correction on #773 cited #1744 as the proof that a
  `ci.yml` edit rotates nothing, but #1744 also changed a manifest and its key did
  rotate. The dead inputs are removed (wave 15), and the gap they were meant to close
  is an open decision again.
- 2026-09-29: **Wave 16, the salt, before the field run.** Mechanism: the lint lane's
  own log lists `crates/vendored/wintun-bindings/.cargo/config.toml` under "Lockfiles
  considered", so a nested `.cargo/config.toml` is hashed; one written under
  `ci-cache-salt/` sits where cargo never runs from. A prefix restore stays warm:
  rust-cache's pre-clean on a partial match drops at most one week-old entry per
  directory (`rmExcept` returns inside its loop). The action's real `run:` script,
  executed under GitHub's `bash --noprofile --norc -eo pipefail` against edited copies
  of `ci.yml`: all 11 jobs get distinct salts; a comment, blank-line or
  trailing-space edit, a workflow-level `env:` edit, a new job and a CRLF checkout
  rotate nothing; an edited step in `rust-recorder` rotates only `rust-recorder`; a
  step appended to `rust-unit`, or to `profiles` (the last job, which runs to EOF),
  rotates only that job. An unknown job id warns and salts the whole file; a missing
  workflow file warns and writes nothing. A wrong turn, recorded: the first "appended
  step" check targeted `profiles`, inserted before the next job and so changed
  nothing, and read as a salt failure until the check was made to prove its edit
  applied. The awk also reads to EOF instead of `exit`ing at the next job, because an
  early exit can SIGPIPE the upstream `tr`, which `pipefail` turns into a failed step.
  macOS `macos-mesh-test.yml` (manual dispatch only) is not salted.
- 2026-09-29: **Correction: the 20 GB raise made the cache read-only.** It was
  "verified" by reading the limit back, the wrong check. The pool was 10.58 GB, above
  the free 10 GB, and with the account's Actions budget at its default every save from
  then on logged `Cache reservation failed: You have reached your configured budget,
  your cache is now read only to prevent additional charges.` as a warning, inside a
  job that stayed green. Last successful saves 09-28 19:43 and 20:17Z; limit raised
  21:04Z; first refusal 21:37Z; then 20 of 20 save attempts across 78 master runs
  refused, including all 13 salted jobs of wave 16's merge run. The newest cache entry
  on any ref stayed at 09-28 20:24Z and the janitor logged the same 10576 MB pool on
  every sweep. Cost: `integration-tests` ran cold every time, 22.3–24.8 min against
  13–19 min before (its family was missing and could not be re-saved). PR lanes kept
  restoring the pre-salt generation and stayed near-warm (whole CI median 6.8 min over
  19 PR runs, 09-29 19:00Z to 09-30 22:00Z).
- 2026-09-30: **A mirror stall in the profiles job** (wave 17). `Build profiles`, ~2 min,
  took 9.3, 10.3 and 10.6 min in three runs between 15:47 and 16:20Z. Its step "Build
  deps for the SFU worker (collab)" ran a raw `apt-get install` against
  `azure.archive.ubuntu.com`: 113 s for the 28.8 MB `libclang-18-dev`, then 300 s for
  the 5 kB `libclang-dev` (run 36739413410). The Rust lanes take the same packages from
  `cache-apt-pkgs-action` and were unaffected. Fix: the job uses the lanes' composite.
  Its cached sets carry everything that step installed (`cmake`, `cmake-data`,
  `libclang-18-dev`, `libclang-dev`, `libjsoncpp25`, `librhash0`); `python3-pip` ships
  with the runner image ("already the newest version"). The composite costs ~21 s in a
  lane, against ~14 s for the raw step on a good day.
- 2026-10-01: **Budget $10 (the operator); saves resume, and wave 16 in the field.**
  Re-ran master CI at `37fa997ae` (run 36819352918, attempt 2, 7.8 min): all 11 salted
  `ci.yml` jobs restored the pre-salt generation (`full match: false`, none cold) and
  SAVED their salted one, every key the one PR #1784's run had computed, the first
  writes since 09-28 20:24Z. installer-smoke's Windows job (run 36899658523) did the
  same. The janitor's `workflow_run` sweep then retired all 11 superseded generations,
  freeing 9.5 GB; the pool had peaked at 20.0 GB, both generations side by side.
  A dispatched `integration-tests` (run 36899638438, 21 min cold) saved `ci-integration`
  (1.1 GB), missing since 09-28, and installer-smoke's macOS job (13.7 min cold) saved
  its first `macos-pkg-smoke` entry. Pool afterwards: 12.9 GB in 24 entries.
  A detector note: rust-cache v2.9.2 logs a successful save only as `Sent N of N
  (100.0%)`; there is no "Cache saved" line to grep for.
