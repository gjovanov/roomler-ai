# FR-6: CI + release build-speed SLO — every lane ≤10 min warm, self-healing

**Status:** **closed 2026-10-30**, with every acceptance criterion field-verified and ticked and
documented in [`docs/build-caches.md`](../build-caches.md). It first closed that morning and
reopened in the afternoon for wave 33: from #3125 on, `ci.yml`'s Windows clippy lane built cold on
every run, because its cache was over its cap. The Actions cache had also started refusing
downloads (Open decisions). Wave 34 moved the Linux lanes off it, wave 35 skips the heavy lanes on
docs-only PRs, and wave 36 lets a cold unit or lint run finish its publish. Waves 33–35 are
field-verified in the log; wave 36 shows only on the next cold run. What is left is runner
queueing. First shipped and field-verified 2026-08-26
(retroactive FR per the CLAUDE.md standing rule — the program ran 2026-07-13 → 2026-08-26 across
~20 PRs). Tracking issue: `FR-6` (#773) in gjovanov/roomler-ai/issues.

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
  `target` drop over `SEED_TARGET_MAX_MB`, 3,000 MB by default and 4,500 MB on the four daemon
  jobs since wave 32; never file-level deletes under `target` — cargo fingerprints outlive
  outputs); `cache-janitor.yml` sweeps PR-ref/idle/tag-ref strays every 6 h.
- **A tag run reuses its rehearsal's compile** (wave 29). After a job's last cargo build,
  `.github/actions/seed-tree-stamp` writes `tree=<HEAD^{tree}> job=<workflow>/<job>` into
  `target/.seed-source-tree`. When the next run of that job restores the seed,
  `.github/actions/seed-tree-reuse` checks the stamp. If it names the same tree and the
  same job, and no tracked file is modified, every tracked file gets the mtime
  2000-01-01, and cargo, which judges path crates by mtime, reuses the build. Any other
  case changes nothing, and the stamp is consumed either way. A reused daemon must
  report the release version from `--version`. Where it cannot run before packaging,
  it must at least contain the version string: the macOS binary links
  `@rpath/libavcodec…`, which only the `.app` resolves. Kill switch: repository
  variable `SEED_REUSE_SAME_TREE=off`. ⚠️ Reuse is only as good as each build script's
  declared inputs: a watched path that does not exist counts as changed (wave 30's
  `capabilities`), and so does a path-crate build script with no `rerun-if` line at
  all (wave 29's `scrap`). `cargo build -v` prints each unit's `Dirty` reason.
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
| 17 | 10-01 | The Actions budget set to $10 (the operator): above the free 10 GB the pool had been **read-only** since the 20 GB raise. And the profiles job's SFU build deps (cmake + libclang) come from the lanes' cached apt sets instead of a raw `apt-get` against the mirror | #1805 |
| 18 | 10-01 | The macOS desktop companion builds in a parallel job (`build-macos-companion`, its own seed family `agent-macos-companion`) instead of serially at the end of `build-macos`, which collects the binary where it used to build it | #1804 |
| 19 | 10-01 | The release Linux x86_64 job installs every package from one cached apt set (`release-linux-u2204-v1`, the master rehearsal saves it, tags restore it) instead of three raw `apt-get` steps against the mirror | #1806 |
| 20 | 10-06 | The Linux desktop companion builds in a parallel job (`build-linux-companion`, its own seed family `agent-linux-companion`, the SAME cached apt set as `build-linux`) instead of serially at the end of `build-linux`, the Linux twin of wave 18 | #1938 |
| 21 | 10-10 | The cache limit back to the **free 10 GB** (the paid tier went read-only when another repository spent the account's budget), and two cuts to master's working set: the plain recorder lane restores `recorder-audio`'s family (works, ~600 MB); a `cargo-cache-trim` step dropped the crate archives before each save (**a no-op**: rust-cache's own `cargo metadata` downloads them again before saving) | #1939 |
| 21b | 10-10 | The trim removed from all 14 jobs. The integration lane builds with `line-tables-only` instead of full debuginfo | #1944 |
| 22 | 10-11 | **Six cargo caches move to the free release-asset store** (`seed-cache` assets `ci-<family>-<os>-<arch>-*`, `.github/actions/ci-seed-restore` + `ci-seed-save`): integration, both installer smokes, and `ci.yml`'s macOS overlay check and two Windows lanes. The Actions cache keeps only the seven Linux lanes plus apt and bun | #1954 |
| 23a | 10-19 | The cargo-cache salt step removes every Rust toolchain but the pinned one before rust-cache computes its key (Linux). rust-cache hashes `rustc -vV` of **every installed toolchain**, so the runner image's own `stable` was in every key: a GitHub image rollout (1.99.0 → 1.99.1) gave each family two keys, and the janitor deleted each in turn as superseded | #2544 |
| 23b | 10-19 | Integration-suite dispatch inputs `threads`, `mongo_tmpfs` and `report_time` (libtest's per-test durations; the slowest tests, seconds per module and mongod's descriptor peaks go to the step summary) | #2544 |
| 23c | 10-19 | The release-asset cache key covers every manifest: profile and feature settings shape the dependency builds, and Cargo.lock records neither | #2544 |
| 23d | 10-19 | The crypto crates (Argon2, OPAQUE, the curve and AEAD crates) build at `opt-level = 3` in the dev and test profiles: the integration suite's test time −34% | #2544 |
| 24 | 10-20 | **The integration suite runs as three shards**, each on its own runner with its own mongod at 2 threads. Each shard lists the same binaries, sorts the (test, binary) pairs and runs every third with `--exact`; each must report every test it was handed. Only the first shard publishes the cargo cache | #2596 |
| 25 | 10-20 | **A merge burst queues one run, not one per commit**: `ci.yml`, `integration-tests.yml` and `installer-smoke.yml` group master's runs by branch, so GitHub keeps one running (never cancelled) and one waiting, and a newer push cancels the waiting one. PR pushes cancel their superseded run; a dispatch gets a group of its own | #2624 |
| 26 | 10-23 | **The setup wizard's release lane gets seeds**: `release-setup.yml`'s four build jobs restore from the `seed-cache` release (`seed-setup-<platform>`) instead of Swatinem, rehearsals publish, and a `reseed` job dispatches a master rehearsal after every published release. The macOS job builds both triples in one cargo invocation, and both Linux jobs install their Tauri deps from cached apt sets (wave 19's pattern) | #2778 |
| 27 | 10-24 | **PR CI's two long poles.** The integration suite runs as four shards, not three. FR-90 P1i's Windows agent-session tests move out of `windows-overlay-clippy` into a parallel `windows-agent-sessions` job with its own seed family | #2844 |
| 28 | 10-24 | **The wizard lane: one run per release, and its apt packages from the seed store.** release-agent.yml's lockstep job skips its dispatch when a release-setup.yml run on the tag is already in flight or green. The wizard's Linux jobs install with new `apt-seed-install`/`apt-seed-save` actions: the .debs live on the `seed-cache` release (`apt-<family>-<ID><VERSION_ID>-<arch>`) instead of an LRU-evicted Actions-cache entry | #2847 |
| 29 | 10-25 | **A tag run reuses its rehearsal's compile.** Six of six releases checked (0.4.163 to 0.4.171) tagged exactly the tree their rehearsal had just built, and recompiled every crate of ours anyway. A stamp in the seed now records the tree and job a successful build compiled; the next run of that job on that tree marks its checkout unchanged and cargo reuses the build. The macOS job drops its cached libvpx build only when brew's Cellar path moved, and the vendored `scrap` build script declares `rerun-if-changed` | #2914 |
| 30 | 10-26 | **The companion jobs reuse too.** `tauri_build::build()` declares `cargo:rerun-if-changed=capabilities` whether or not the directory exists, and cargo treats a missing watched path as changed. `roomler-desktop` had no `capabilities/`, so every build re-ran its build script and recompiled it, including a reused tag run (114–149 s per companion job). An `agents/roomler-desktop/capabilities/README.md` makes the directory exist. The capability parser loads only `*.json`/`*.toml`, so the app's ACL is unchanged (still no capability). **And the release lane's two cached-apt steps are bounded.** One hung 23+ min on 0.4.175's tag run where it takes ~37 s. Each now has `timeout-minutes: 8` + `continue-on-error`, and falls through to the existing verify/apt-get fallback, which first repairs an interrupted dpkg | #2940 |
| 31 | 10-28 | **A release's first build no longer makes its tag run recompile.** Tauri's `generate_context!` writes `out/tauri-codegen-assets/<hash>` into `roomler-desktop`'s OUT_DIR *while that crate compiles*, only when the file is missing, and lists it as an input. So the first build of a new version (a release's rehearsal) left an input newer than the build itself, and the tag run, plus the reseed after it, recompiled `roomler-desktop` once. That cost about 2.5 min on macOS every release, and on Windows whenever the write and the compile's start fell in different seconds (the seed's tar keeps whole seconds). Two things found it: `CARGO_LOG=cargo::core::compiler::fingerprint=info` on the four companion build steps (#3024, diagnostic), and a probe that bumps only `roomler-desktop`'s version. On a same-tree reuse, `seed-tree-reuse` now marks the build's OUT_DIR files unchanged, as it already does the sources. Field-verified on 0.4.181: no companion recompiled on the tag run or the reseed after it, and the diagnostic came out again (#3037) | #3024, #3030, #3037 |
| 32 | 10-28 | **The daemon lanes' seed cap rises to 4,500 MB.** The accretion GC drops a seed's `target` whole once it passes `SEED_TARGET_MAX_MB`, 3,000 MB by default. A daemon seed restarts at ~2.2–2.7 GB after a cold build and grows ~350–400 MB with each release. Only a new version adds artifacts: cargo names them by name, version, features and profile, never by source, so a rebuild of the same version overwrites in place. The default therefore dropped a seed again within one or two releases: the Windows seed on four post-release reseeds in a row (0.4.180–0.4.183), and 0.4.183's reseed the `.pkg` and `.deb` aarch64 seeds too (574–580 crates cold, a 31 min run). The four daemon jobs' restore steps now set the cap to 4,500 MB, release-setup's macOS job already running at 5,000. That lasts about four releases, and a compressed seed (535–790 MiB) stays far under the 2 GiB asset limit. The GC line also prints the cap in effect and the volume's free space, so a bigger target's disk headroom shows on every run. The operator chose this over pruning stale units, the riskier change: rc.422–424's seed died on a deleted output whose fingerprint survived. Field-verified on 0.4.185's post-release reseed: at the tag's tree it kept the MSI seed at 3,037 MB and the `.deb` x86_64 seed at 3,367 MB and reused the build, with 0 compiles in all 8 jobs. The same run on 0.4.184 had dropped the MSI seed at 3,082 MB and compiled 586 crates cold | #3068 |
| 33 | 10-30 | **The Windows clippy lane's CI cache fits its cap again.** #3125 (for #3116) added the setup wizard's clippy and unit tests to `windows-overlay-clippy`, and the job's cold build then pruned to 4,440 MB, over `ci-seed-restore`'s default cap of 4,000 MB. Every restore dropped the cache whole (`restored target is 4440 MB (> 4000) — dropping it whole`), every run compiled or checked ~1,740 units cold, and each master run published the same 4,440 MB again. The GC's promise that "the next master run publishes a lean one" cannot hold once a lean one is over the cap. The lane took 7.4–8.3 min in the three runs before #3125 and 17.0–22.6 min in all 20 runs checked after it, the floor of each run's wall clock. The job now sets `max-target-mb: "6000"`, as the macOS overlay check and the integration shards already did. And a master run that publishes a cache its own restore would drop warns `CI cache over its cap` (the restore exports the cap it applied as `CI_SEED_MAX_TARGET_MB`) | #3153 |
| 34 | 10-30 | **The Linux lanes' cargo caches move to `seed-cache`.** From 2026-10-30 the Actions cache refused downloads under load (`Failed to restore: Egress is over the account limit.`), and Swatinem then built the lane cold: `Rust unit tests` compiled 1,795 crates in 23–27 min against ~8 (Open decisions). The seven Linux families now restore and publish through `ci-seed-restore` and `ci-seed-save`, as the macOS, Windows, integration and installer families have since wave 22. They are `lint`, `unit`, `agent`, `ffmpeg-encoder`, `recording-ffmpeg`, `profiles` and `recorder-audio`, which both recorder lanes restore. Each publishing job gets `contents: write`, and `CARGO_INCREMENTAL=0` is set where rust-cache used to set it. A target archive over the 2 GiB asset cap goes up as numbered 1,900 MiB parts that the restore concatenates (the unit lane's is ~2.5 GB). A publish writes one form and then retires the other. A whole asset wins over parts, and parts from two publishes fail zstd's checksum, so a mixed set builds cold rather than wrong. A local harness with real tar and zstd passed 8 of 8 restore cases and, against a fake `gh`, 6 of 6 publish cases. First caps are about 1.5× each lane's old Actions-cache archive at the integration lane's 4.3× unpack ratio. The Actions cache keeps the apt and bun sets | #3157 |
| 35 | 10-30 | **A docs-only PR skips the heavy jobs.** 22 of the last 60 CI runs on 2026-10-30 were docs by title, and each took all 20 jobs, ~10 GB of cache downloads and up to 44 min. A new `changes` job reads the PR's file list (`pulls/N/files`, old paths of renames included). When every path is under `docs/`, a top-level `*.md` or `.claude/`, the twelve heavy jobs skip: the five Rust lanes, the macOS overlay check, the three Windows jobs, both FFmpeg jobs and the profiles build. No Rust build, test or link reads those paths. Every job that does read them still runs: frontend builds the docs site and tests the UI that uses `docs/assets`, machine-names and commit-identity run `.claude`'s self-tests, and the docs audits read `docs/`. It fails safe every way. A push to master skips the gate, so every heavy job runs, and a superseded run can never leave a code commit unverified. A file list that does not load, or one at the API's 3,000-file limit, runs everything. "Rust checks" passes a docs PR whose lanes all skipped, and fails when the gate did not succeed. A local harness passed 18 of 18 cases: nine classifications against a fake `gh`, and nine aggregate outcomes over push, code-PR and docs-PR inputs | #3170 |
| 36 | 10-30 | **A cold unit or lint run fits its job's ceiling, publish included.** Wave 34's first master run to start (41107105820) found no `ci-unit` cache and built the lane cold: 1,795 crates, the tests, a prune from 14,068 to 10,373 MB, then a 2,587,763,998-byte archive published in two parts. The 30-minute ceiling cut the job off at 30 min 15 s with part 2 still uploading, and "Rust checks" failed on master. The part landed two seconds after the runner began its cleanup, so the next run restored the cache whole. Had it not landed, every later run would have built cold and been cut off the same way. The lint lane's cold run in that master run took 27 min 19 s, 91 % of its ceiling. `rust-unit` now allows 45 minutes and `rust-lint` 40 (`ci.yml:474`, `:159`), about 1.5 times each lane's measured cold run. Warm runs take 8–11 min, so only a cold or hung run comes near either | #3170 |

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
- [x] A changed or added CI step's dependency builds reach its job's cargo cache on the
      next master run, without a cold run (wave 16). Field: the first master run after
      the salt lands saves a new generation for every salted job from a prefix restore
      (`full match: false`, not `No cache found`), and a later edit to one job's steps
      rotates that job's key alone. Both halves, 2026-10-01: master CI re-run 36819352918
      (attempt 2) restored all 11 salted lanes from the pre-salt generation and saved
      each salted one, once the budget let writes through; then PR #1805, which edits only
      the profiles job, rotated `ci-profiles` alone (`…-767ec95a` against master's
      `…-4829dd40`, a prefix restore), while every other salted job restored master's key
      with a full match (run 36907082419)
- [x] No silent-save/skip path remains (verify-after-publish on every save)
- [x] **No cache write depends on the Actions budget** (waves 21–22): the limit is the free 10
      GB, master's working set fits it with room for a family's rotation (≤ ~8.5 GB
      steady), and master runs save with no "Cache reservation failed" warning. Field,
      2026-10-11: the pool settled at **7.48 GiB**, the seven Linux lanes (7,325 MiB) plus apt
      and bun, with no superseded or PR-ref entries. That leaves ~2.5 GiB for a rotation of
      the largest family (`ci-unit`, 2.1 GiB). Saves land with no budget warning, and the six
      moved families republished from master to the `seed-cache` release (log)
- [x] **First normal-delta `agent-v*` tag post-migration lands ≤10 min end-to-end** —
      **not met until wave 29.** agent-v0.4.115 (2026-10-01, run 36937794461, the first tag after
      waves 18 and 19, whose agent code is 0.4.114's) took **14.9 min**: Windows MSI
      13.9, macOS `.pkg` 10.4, Linux x86_64 9.9, aarch64 7.4. Over the 12 tags 0.4.102–
      0.4.115 the MSI job ran 9.2–14.9 min and is the steady long pole. Of its 13.9 min,
      586 s were the final single-crate compile of `roomlerd` under `codegen-units=1`, which
      took 349 s on 0.4.114 for the same code: runner speed, not work. The remaining
      levers are the operator's (see Open decisions)
      **Since wave 29 the compile is no longer the reason.** agent-v0.4.173 (2026-10-26,
      run 40250842840) reused its rehearsal's build: MSI 4.0 min, `.pkg` 2.0, Linux 1.9
      and 1.4. From the first build job to the post-release reseed, its own critical path
      was about 7 min. It still took **17.0 min** on the clock, because its jobs waited up
      to 10 min for a runner (the account runs at most 20 jobs at once). Still unmet: the
      box is ticked when a tag lands under 10 min on the clock.
      **Met on agent-v0.4.174** (2026-10-26, run 40287645036): 07:44:29 → 07:51:43Z,
      **7.2 min** on the clock. All 8 build jobs reused rehearsal 40267019868's build (same
      SHA `065e15308`, no release dispatch between them), and the four daemon jobs
      reported `roomlerd 0.4.174`. The longest jobs were the Windows companion (4.2 min)
      and the MSI (4.0); wave 30 takes the companions' remaining compile out.
- [x] **Docs** — [`docs/build-caches.md`](../build-caches.md) describes what this FR built, in
      house style: the seed store, a release's five runs, same-tree reuse and its traps, the
      accretion GC and its caps, the merge freeze, and the log lines a run is read by. It has
      a row in `docs/README.md`'s index (2026-10-29).

## Open decisions / residual levers (all trade-offs, not waste)

- **The Actions cache refuses downloads under load (from 2026-10-30).** A restore finds its key
  (`Cache hit for: v0-rust-ci-unit-…`), sits at `Received 0 of 2852032394` for 36–68 s, then
  fails with `Failed to restore: Egress is over the account limit.` Swatinem reports
  `No cache found`, and the lane compiles every dependency. `Rust unit tests` compiled 1,795
  crates instead of 73 and took 23.4 and 27.0 min instead of 6.5–9.6 (runs 41056324624 and
  41063619620). The second was the 0.4.188 bump PR's CI, 37.5 min end to end. Six of the twelve
  CI runs sampled between 12:03 and 14:07Z had a refused restore, most of them the apt sets,
  which fall back to the mirror. The limit is GitHub's, on the account's downloads, not the
  pool's 10 GB; other projects report the same message from 2026-10-24. The `seed-cache` release
  assets are not behind it: the Windows and macOS lanes restored from them in every run checked.
  Levers posed to the operator, who chose the first two on 2026-10-30:
  - **wave 34:** move the Linux lanes' cargo caches to `seed-cache`, as wave 22 did for the
    rest. The unit lane's archive is 2.85 GB, so its target goes up in parts under the 2 GiB
    asset cap;
  - **wave 35:** skip the Rust, Windows and macOS lanes on a docs-only change. That means
    fewer downloads, and fewer of the account's 20 concurrent job slots taken;
  - not chosen: a plan with more concurrent jobs (Pro 40, Team 60; macOS stays at 5).

  Both chosen levers shipped and are field-verified (waves 34 and 35, in the log).

- The warm Windows floor is the per-release rebuild of every path crate under the
  size-optimized `cgu=1` profile, plus Azure signing steps. **Measured 2026-10-01, now
  that macOS and Linux are off the critical path (waves 18–19):** normal-delta tags still
  exceed 10 min (agent-v0.4.115, 14.9 min). The MSI job is the pole (9.2–14.9 min across
  12 tags), and most of it is `roomlerd`'s own compile. Three levers were proposed:
  relax `cgu=1`, decouple the per-release version bump, or pay for 8-core runners. The
  2026-10-06 probe (log) narrowed them to one:
  - **Decoupling the version bump cannot help.** The path crates rebuild because the
    checkout gives every source file a newer mtime than the seed's dep-info, not because
    of the version. A rehearsal on the SAME commit as its seed rebuilt all 19 of them, and
    cargo's fingerprint log names the cause for each: `StaleItem(ChangedFile { stale:
    <source>, … })`, at an identical 0.4.116.
  - **Bigger runners cannot help on their own.** Under `cgu=1` the compile runs on one
    core, so extra cores sit idle. The runner is an AMD EPYC 7763 with **2 physical cores
    (4 threads)**, and Defender real-time scanning is already off (exclusions `C:\`,
    `D:\`), so that is not a lever either.
  - **`codegen-units` for `roomlerd` alone is the one real lever, and it buys ~2 min:**
    roomlerd's lib + bin took 355 → 244 s (`cgu=4`, −31%) on one runner and 496 → 358 s
    (−28%) on another; `cgu=16` was no better than 4, and `opt-level=2` at `cgu=1` saved
    nothing. Its size cost on the shipped binary is not yet measured. The bin (`main.rs`)
    alone takes 100–230 s after the lib, much of it plausibly the lib's generics
    monomorphized again, since release builds don't share generics across crates. Moving
    `main.rs` into the lib is the structural candidate, unmeasured. Runner variance is
    ±40% on identical work.
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
  **Reversed 2026-10-10 by wave 21: back to the free 10 GB, and master's working set cut
  to fit it.** The budget (by then $20) is the ACCOUNT's, and the private `lgr` repository's
  hosted minutes exhausted it on 10-07 (6,667 weighted minutes on October 1–9 against
  2,000 included; macOS 57% of it). The pool went read-only again on 2026-10-09 ("Cache
  reservation failed: You have reached your configured budget"), so every lane restored
  an ever-older generation, though roomler-ai's own share of the bill was ~1 GB of
  storage. A paid tier that another repository can switch off is not a design. Wave 21
  took the two cuts this paragraph once held in reserve, and **only one of them works**.
  - The plain recorder lane restores `recorder-audio`'s family and keeps none of its own
    (~600 MB). `cargo tree` resolves the two TEST graphs to identical features for every
    shared crate. The lane's non-test `cargo build` still rebuilds tokio and ~25
    dependents, in a variant without dev-dependency features that the audio lane never
    builds. That cost was within master's range (log).
  - ⚠ **Dropping the crate archives cannot work, and wave 21b removed it.** Every family
    does hold the whole lockfile's archives (179 MB, 1,186 packages, ≈2.1 GB across 12),
    but rust-cache's post step runs `cargo metadata --all-features` before it saves, and
    cargo needs a crate's `.crate` file even when the source is already unpacked. It
    downloaded all 183 MB again, silently (rust-cache captures the output), and saved
    them. The family sizes did not move (log). The old line, "deduplicating it is not
    the lever", was right, for this reason rather than its size.
  - Wave 21b: the integration lane builds with line tables instead of FULL debuginfo,
    which its 1.1 GB family still carried. Its `RUST_BACKTRACE=1` keeps file:line.
  - **Wave 22, the operator's choice (2026-10-11): keep the free tier permanently, by moving
    caches out of the pool rather than paying for a bigger one.** The ten hot `ci.yml`
    families alone came to ~9.3 GiB, so moving only the three rarely-run families
    (integration, both installer smokes) would still have left the pool at its limit. Six
    jobs moved: those three plus `ci.yml`'s macOS overlay check and two Windows lanes.
    They restore from and publish to the `seed-cache` release, like the release seeds.
    `ci-seed-save` prunes local packages' units with their fingerprints and skips an
    unchanged cache, as rust-cache does. The pool keeps the seven Linux lanes plus apt and
    bun, ~7.6 GiB, which leaves room for a full rotation.
- **Resolved by wave 16 (merged 09-29, field-verified 10-01): a new CI step's dependency builds
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
- **Resolved 2026-10-01 by wave 19 (#1806): the release Linux x86_64 job installed from the mirror** (found by wave 18's
  rehearsal). Its two raw `apt-get` steps cost 18 min on 10-01 from 17:38Z, the window
  in which the same Azure mirror also stalled PR #1804's profiles job (on the pre-wave-17
  step). The mirror had stalled the profiles job on 09-30 15:47Z too, so this recurs. A
  tag built in such a window misses the 10-min goal on this job alone. Lever: the CI
  lanes' answer, `cache-apt-pkgs-action` sets that the master rehearsal saves and tag
  builds restore. Cost: a second package list to keep in step.
- **The integration suite after wave 23 (2026-10-19): ~18–21 min, above the 10-min goal.**
  Measured levers, from the field log:
  - Every third-party crate at opt-level 2 in dev/test (`[profile.dev.package."*"]`): test
    time 1,725 s against 2,488 s with wave 23d's crypto list. It costs one ~12-min cold
    rebuild of every dependency in every dev-profile lane, slower cold local builds, and
    deps that a debugger steps through optimized.
  - More threads need fewer open files in mongod, not just more CPU: each test creates and
    drops ~246 WiredTiger files, and 4 threads exhausted the 64,000-descriptor ceiling. The
    options are a higher ceiling (the leaked-database check catches a real leak either
    way), sharding the suite across runners (each with its own mongod, but each also takes
    one of the account's 20 concurrent job slots, which were full on 10-19), or a pool of
    pre-indexed test databases emptied between tests instead of dropped and re-created
    (removes the churn and ~1 s of setup per test, at the price of a harness change every
    module's tests ride on).
  - The 43 tests over 10 s (16% of test time) wait out real windows (an unanswered start, a
    silent device, a throttle). Shorter windows under test settings would cut most of it;
    those belong to the FR-90 and FR-52 owners.

## Out of scope

- The tag-race double-runs during release bursts (release-cutting process: `ls-remote`
  before tagging).
- CI-lane cache sizing owned by other programs (`ci-integration`, `ci-ffmpeg-encoder`…).
  Wave 34 moved their store and set first caps; tuning a lane's cap is its owner's.

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
  Re-ran master CI at `1dd1d08be` (run 36819352918, attempt 2, 7.8 min): all 11 salted
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
- 2026-10-01: **Wave 16's second field half, and wave 17 under a live stall.** PR #1805
  edits only the profiles job. Its run (36907082419, whole CI 8.5 min) rotated
  `ci-profiles` alone (`…-767ec95a` against master's `…-4829dd40`, a prefix restore);
  the other ten salted jobs restored master's key with a full match. Its cached apt step
  took 21 s and the job 2.0 min, where PR #1804's run 50 min earlier, on the old raw
  step, had spent 455 s in apt during the mirror stall described below.
- 2026-10-01: **Wave 18, pre-merge rehearsal** (run 36900740850, on the branch,
  artifacts-only). `Build .pkg (macOS arm64)` took 10.6 min, against 12.2 on
  agent-v0.4.114. Agent 478 s, then it waited 1 s for the companion (finished at
  17:44:29, before the agent build did), downloaded it in 1 s and checked it in 0 s;
  staging, codesign and notarisation ran unchanged. A tag skips the 53 s seed save, so
  about 9.7 min there. The new `Build desktop companion (macOS arm64)` took 6.6 min cold
  (342 s build, no seed family yet) and saved its first seed. The same rehearsal also
  showed two slow jobs this wave does not touch:
  - `Build .deb (Linux x86_64)` took 35.5 min, 18 of them in raw `apt-get` against the
    Azure Ubuntu mirror ("Install system build deps" 594 s, the companion's deps 512 s,
    against about 22 s each on 09-28). Its aarch64 sibling (`ports.ubuntu.com`) installed
    in 27 s.
  - `Build .msi (Windows x86_64)` took 22.7 min, its agent compile 1085 s against 408 s
    on the tag, building against seeds three days old. The rehearsal saved fresh ones.
- 2026-10-01: **Wave 19, pre-merge rehearsal** (run 36934413598, on the branch). The
  cached-apt step missed, as a first run must, and cached 146 packages (180 MB) under its
  own key. Its install spent ~8.5 min in the mirror, still stalling at 22:20Z; the
  caching itself took 9 s. The verify step passed in 0 s on the real 22.04 runner: every
  pkg-config module, the loader, cmake/patchelf and libclang, with no fallback warning
  emitted. On master (rehearsal 36936407900) the set missed again, because a branch's
  caches are invisible to master. It installed in 40 s, the mirror having recovered, and
  saved on master at 22:40:57Z.
- 2026-10-01: **agent-v0.4.115, the field test of waves 18 and 19** (run 36937794461,
  tagged 22:54:13Z on `c2eaff8e1`, published 23:08:48Z). 14.9 min end to end: MSI 13.9
  (the pole), `.pkg` 10.4 (12.2 on 0.4.114, median 12.3 over the last 12 tags), Linux
  x86_64 9.9, aarch64 7.4, the companions 3.3 (macOS) and 6.1 (Windows).
  - Release: 28 assets, names identical to 0.4.114's apart from the version, each
    artifact with its `.asc` and `.sha256`, and no intermediate leaked.
    `/api/agent/latest-release` served 0.4.115 with both daemon `.deb`s before the
    companion's.
  - Lockstep: `setup-v0.4.115` published at 23:26:27Z on the same commit, and
    `/api/setup/{windows,linux,macos}` all serve 0.4.115 filenames.
  - zeus (Linux x86_64, systemd-supervised, checked first): `self-update --check-only`
    verified the `.deb`'s `.asc` against the pinned release key, and the real update
    installed and verified itself (`SucceededVerified`). A CLI-run self-update leaves
    the live daemon on the deleted inode, as `dpkg -i` does, so a `systemd-run`
    scheduled restart moved it to 0.4.115. The server recorded it at once, and the
    overlay came back on DERP, then re-upgraded to direct 2.3 min later.
  - The operator's MacBook, read-only: the `.pkg`'s checksum matched; it is Developer ID
    signed and notarised (`spctl`: accepted, source=Notarized Developer ID). The
    companion inside it, built by the new parallel job, is an arm64 Mach-O whose `.app`
    passes `codesign --verify --deep --strict` (team 4TG7586MY5) and links only system
    libraries.
  - Fleet uptake at 23:25Z: 1 of 8 online devices on 0.4.115 (zeus, updated by hand);
    the rest follow their updaters (~4 h, the Mac's helper ~6 h). 0.4.115 changes nothing
    a device runs, so uptake proves the artifacts, not a behaviour.
- 2026-10-06: **wave 20 rehearsal** (run 37538752420, `publish_release=false` on the
  branch). Every job green. `build-linux-companion` built cold in 7.4 min ("no
  seed-agent-linux-companion-Linux-X64 seed yet"), published its family (447 MB target +
  70 MB cargo), and restored the same apt entry as `build-linux`
  (`cache-apt-pkgs_c4273460…`) with no fallback. `build-linux` took 9.1 min including a
  69 s seed save that tag builds skip, so about 8.0 min on a tag, against 10.8 on
  agent-v0.4.116 and 10.2 on the master rehearsal before it (37237655417).
- 2026-10-06: **the codegen probe** (branch `ci/fr6-cgu-probe`, never merged, run
  37539178203). It restored the release lane's Windows seed read-only and rebuilt
  `roomlerd` once per variant on one machine, two machines in opposite orders. lib + bin
  seconds:

  | Variant | Runner A | Runner B |
  |---|---|---|
  | `cgu=1` (as released) | 355 (repeat: 354) | 496 |
  | `cgu=4` | 244 | 358 |
  | `cgu=16` | 258 | 384 |
  | `cgu=1`, `opt-level=2` | 369 | 502 (repeat: 502) |

  The bin alone took 100–230 s after the lib. Build 1 also logged cargo's fingerprint
  reasons: every path crate was `StaleItem(ChangedFile)` (checkout mtime newer than the
  seed's dep-info) at the seed's own version, which retires "decouple the version bump" as
  a lever. Runner facts: AMD EPYC 7763, 2 cores / 4 threads, Defender real-time
  protection off with `C:\` and `D:\` excluded. The first attempt (run 37537408392) built
  green in 10m35s and then died on its own `grep`: GitHub runs `bash -e -o pipefail`, and
  `dtolnay/rust-toolchain` sets `CARGO_TERM_COLOR=always`, so an anchored grep over the
  log matched nothing.
- 2026-10-09/10: **the paid cache tier went read-only again; wave 21.**
  - The pool was read-only: master CI run 37904595751's lint lane restored a stale
    generation (`full match: false`), then `Cache reservation failed: You have reached your
    configured budget, your cache is now read only to prevent additional charges`. The pool
    held 11.48 GB in 16 entries against the 20 GB limit.
  - The budget went elsewhere. roomler-ai's minutes are free (public repository), and its
    storage was ~1 GB over the free tier. The private `lgr` repository's hosted jobs on
    October 1–9, from the jobs API (the run-timing API's `billable` field now reads 0 for
    every run, private ones included, so it measures nothing): Linux 1,895 min, Windows 491,
    macOS 379. That is 6,667 weighted minutes against 2,000 included, macOS 57% of them, and
    one Linux release job hung for 361 minutes. lgr's own fixes are lgr#125, which made macOS
    and Windows on-demand, and lgr#126 (a self-hosted runner, and a 90 min cap on the
    release build).
  - Same day: the limit went back to 10 GB (`PUT …/actions/cache/storage-limit
    max_cache_size_gb=10`, read back as `{"max_cache_size_gb":10}`).
  - The family sizes the cuts were planned from (MB): unit 2148, lint 1585, agent 846,
    macos-pkg-smoke 813, profiles 796, macos-overlay 760, windows-recorder 728,
    recording-ffmpeg 693, ffmpeg-encoder 651, recorder-audio 594, recorder 573,
    windows-clippy 417, plus apt and bun 333. The crate archives in master's lockfile total
    179 MB (1,186 packages), and every family holds them.
  - rust-cache's `cleanRegistry` (v2) keeps the extracted sources of `-sys` crates. A
    re-extraction would bump directory mtimes that their build scripts watch. So the trim
    deletes only the `.crate` files and keeps the directories, which that cleanup scans.
  - `cargo tree -p roomlerd -e normal,build,dev --target x86_64-unknown-linux-gnu`: the plain
    recorder graph and the `+audio` graph differ in roomlerd itself and 7 audio-only crates
    (alsa, alsa-sys, audiopus, audiopus_sys, cmake, cpal, dasp_sample), and in nothing else.
    ⚠ That comparison covered the TEST graphs only. On the PR (run 38081437008) the plain
    lane restored `v0-rust-ci-recorder-audio-…` and still compiled 40 crates against
    master's 15. The extra 25 (tokio, hyper, reqwest, quinn, openssl, webrtc-dtls, …) are
    the variant its non-test `cargo build -p roomlerd` needs: resolver v2 unifies no
    dev-dependency features there, and the audio lane never builds it. The lane took 273 s
    against 221–295 s over master's last six runs, off the CI critical path. The trim logged
    `dropped 183 MB of .crate archives`.
  - The 10 GB limit alone already made saves land again. Master run 38080443848 saved
    `ci-recorder-audio` (`Sent 623761050 of 623761050 (100.0%)`), most families had fresh
    entries from 19:24–19:54Z, and no budget warning appeared. But LRU evicted
    `ci-unit` and `ci-lint` at once: two families absent from the 10-09 measurement
    (`ci-integration` 1,124 MB and `installer-smoke-windows` 594 MB) make the full working
    set ~13.4 GB, not 10.9. By that arithmetic wave 21's cuts leave ~10.4 GB, still over
    the limit. The next cut is sized from the first master run's measured families, not
    from this estimate.
- 2026-10-10: **wave 21's crate-archive trim was a no-op** (found on its first master run,
  38083571444; removed by wave 21b):
  - Every lane logged `dropped 183 MB of .crate archives`, and no family got smaller. The
    unit lane restored 2,252,680,759 B and saved 2,252,534,802 B (146 KB apart). lint,
    macOS overlay and recorder-audio were all within 0.1% of their sizes before the trim.
  - A re-run of that unit job restored the post-trim archive (`full match: true`).
    Cargo downloaded nothing, and the trim again found 183 MB of archives. They were in
    the saved archive.
  - The mechanism, reproduced in WSL with a throwaway `CARGO_HOME`: build a crate,
    delete its `.crate` (its source stays unpacked), run `cargo metadata --all-features`,
    and it prints `Downloaded itoa v1.0.18`. Cargo opens the archive before it checks the
    unpacked source. rust-cache's post step runs exactly that call before it saves, with
    the output captured, so the deleted archives come back unseen. The trim also cost
    every job that hidden re-download.
- 2026-10-11: **wave 22 validated on its own PR (#1954)**. A temporary clause let the PR
  branch publish, so the PR could prove the whole cycle before merging; it was removed
  before merge.
  - Run 1 (cold): every one of the six found no `ci-*` asset, built cold, pruned and
    published.

    | Job | Cold | Target (MB, pruned → kept) | Published (cargo + target) |
    |---|---|---|---|
    | integration | 25.2 min | 6,302 → 3,464 | 191 + 856 MB |
    | installer-smoke-windows | 18.2 | 1,715 → 1,501 | 196 + 416 |
    | windows-recorder | 13.3 | 3,204 → 2,278 | 191 + 591 |
    | macos-pkg-smoke | 11.5 | 2,437 → 1,998 | 191 + 595 |
    | windows-clippy | 11.4 | 2,703 → 2,108 | 191 + 555 |
    | macos-overlay-check | 11.0 | 3,720 → 3,038 | 190 + 844 |

  - Run 2 (`gh run rerun`, same commit): all six restored `same key`, logged `cache
    up-to-date`, and published nothing. Warm times: integration 12.3 min, Windows MSI 10.2,
    Windows recorder 6.4, macOS pkg 6.1, Windows clippy 5.2, macOS overlay 7.3. The
    cargo-installed `cargo-wix` came back from the cache: the WiX step took 68 s
    (compiling it) on run 1 and 11 s on run 2.
  - The macOS overlay check's restore cap went 4,000 → 6,000 MB, since its fresh target is
    already 3,038 MB.
  - A dry run in WSL caught one bug before any of this: without `zstd`, `tar` left 0-byte
    archives, and the first draft uploaded them over the good cache. `ci-seed-save` now
    refuses a failed pack.
- 2026-10-11: **wave 22 on master** (`f1f861c19`, #1954).
  - The first master runs (CI 38096524857, integration-tests 38096524893, installer-smoke
    38096524865) restored each of the six from the `seed-cache` release. Each saw the key
    change, since removing the validation clause changed each job's definition, and
    republished under master's key (`cache published: ci-…`). Times: macOS overlay 7.3 min,
    macOS pkg 8.5, Windows clippy 6.7, Windows recorder 8.2, Windows MSI 12.2, integration
    13.3. The jobs' annotations held only GitHub's Node.js 20 notice and a macOS capacity
    notice, no cache warning.
  - The six families' old Actions-cache entries were deleted by hand (8 entries,
    ~6.4 GiB), along with the orphaned plain-recorder family. One more
    `installer-smoke-windows` entry appeared at 00:00:31Z, written by the last run on the
    old workflow (38095828862, `e4e3896bf`, started before the merge), and was deleted.
  - Settled pool: **7.48 GiB in 12 entries**, against the free 10 GiB.
- 2026-10-19: **"the tests got slow": two causes, wave 23 (#2544).**
  - **CI lanes cold.** PR run 39288740822: lint 22.5 min, unit tests 19.2, profiles 10.1,
    each with `No cache found`. The jobs that missed ran on runner image 20261018.339.1 and
    the ones that hit on 20261012.334.1. rust-cache's "Rust Versions" listed two toolchains,
    1.95.0 (ours) and the image's own `stable`, 1.99.0 on the old image and 1.99.1 on the
    new, so each family had two keys (`…-6a930126-…`, `…-013f3a70-…`). Each miss built cold
    and saved, and the janitor then deleted the other image's key as superseded: `ci-lint`
    at 16:38, 17:23 and 17:58 UTC. With wave 23a, every Linux job on both images lists only
    1.95.0 and computes one env hash, `0e351699` (PR run 39305294563). The salt step costs
    1–2 s.
  - **The integration suite**, run 39288740923: the build was warm (2.4 min), and the suite
    was 30.6 min. Its 819 tests in one binary at `--test-threads=2` took 1,494 s. On master
    the suite step had grown from a mean of 19.5 min on 10-14 to 27–29 min on 10-17..10-19
    (max 35.7). With `report_time` (839 tests including the hive binaries):

    | arm (dispatch run) | threads | Mongo data | test time | median | mongod fd peak |
    |---|---|---|---|---|---|
    | A, as is (39305102217) | 2 | disk | 3,772 s | 3.88 s | 19.4 k |
    | B (39305113225) | 2 | tmpfs | 3,281 s | 3.34 s | 28.2 k |
    | C (39305123577) | 4 | disk | 6,481 s | 7.27 s | 32.8 k |
    | D (39305139703) | 4 | tmpfs | 4,823 s | 5.20 s | 31.4 k |
    | crypto opt-level 3 (39312440997) | 2 | disk | 2,488 s | 2.48 s | 22.4 k |
    | all deps opt-level 2 (39312466654) | 2 | disk | 1,725 s | 1.57 s | 35.2 k |
    | crypto, 4 threads (39312454242) | 4 | tmpfs | **mongod died** | | 63.8 k |

  - Password hashing was most of it. `health_check_returns_ok` took 1.1 s, a register 2.1 s
    and a register + login 2.5 s: one unoptimized `Argon2::default()` op cost 0.5–0.9 s, and
    `seed_tenant` (612 call sites) does four. 547 of the 839 tests took 2–5 s. Optimizing
    the crypto crates alone took the extauth KE1 throttle test from 23.1 s to 3.9.
  - Doubling the threads nearly doubled each test's time (arm C): the suite is CPU-bound on
    a 4-vCPU runner, so threads were not the lever until the crypto was optimized.
  - ⚠️ **More threads against ONE mongod hit its descriptor ceiling.** Every test creates
    and drops a database of ~246 WiredTiger files, and the dropped ones stay open until
    mongod reaps them. At 2 threads the curve is a sawtooth (22 k peaks, cleared every
    4–6 min); with fast tests at 4 threads it climbed ~10 k a minute to 63,769 of 64,000,
    and mongod aborted. Each later test then waited out the driver's 30 s, and the job
    hit its 75-min limit. Faster tests mean more churn, so the 2-thread margin shrinks too
    (35 k with every dependency optimized).
  - The PR's CI run waited 20–64 min in the queue before its jobs started. The account's
    20 concurrent jobs were full (experiment runs, other sessions' PRs, a release), so
    sharding the suite across runners costs queue slots as well as minutes.
  - Not shipped: all third-party crates at opt-level 2 (test time −54% against −34%). It
    costs a ~12-min cold rebuild of every dependency in every dev-profile lane, and slower
    cold local builds. That trade-off is the operator's (see Open decisions).
- 2026-10-20: **wave 24, the suite as three shards** (the operator's choice of the levers
  above). Dispatch run 39386817512 on the branch, with wave 23 in it:
  - **12.0 min wall-clock** (was ~30). The suite listed 874 tests (35 more than the night
    before). The shards were handed 292, 291 and 291, and each passed exactly what it was
    handed.

    | shard | job | build | suite | test time | mongod fd peak |
    |---|---|---|---|---|---|
    | 1/3 | 9.6 min | 2.5 | 5.3 | 613 s | 34.3 k |
    | 2/3 | 11.5 min | 3.7 | 6.4 | 741 s | 25.2 k |
    | 3/3 | 11.7 min | 3.3 | 6.6 | 756 s | 23.0 k |

  - Each build restored the integration cache under a changed key (wave 23 had not reached
    master yet), so it recompiled the crypto crates on top of the warm target. Once
    master republishes, a warm build is ~2–2.5 min and a shard ~10 min.
  - The partition was dry-run first with two fake libtest binaries (103 tests): union ==
    listing, no duplicates, the skipped test excluded, a module's tests spread 8/8/9.
- 2026-10-20, evening: **the shards on master, and the queue** (wave 25).
  - ⚠️ The forecast above was wrong. Of the six master runs since wave 24 (36cb849ec
    through 39de8fe41), five took **11.7–15.3 min wall-clock**. Their shards ran 9.6–12.9
    min: builds 1.9–3.8 min, suites 5.4–8.2. The sixth took 24.8 min: its shards queued
    3–7 min, and one shard's build ran 10.1 min. The build does not drop to ~2 min,
    because the cache holds third-party crates only and every shard rebuilds the workspace
    crates. The lane stays above the 10-min goal; the levers left are in Open decisions.
  - The queue mattered more than any job. CI and integration runs were grouped per commit,
    and ten FR-116 squash-merges queued ten full CI runs (170 jobs) against the account's
    20 concurrent slots. A session cancelled its own superseded runs by hand, and #2596's
    own CI took 58 min, most of it waiting for slots. Wave 25 groups master's runs by
    branch, so a burst queues one run while the running one finishes.
- 2026-10-23: **the setup wizard's release lane built cold for nine days** (wave 26).
  Asked: "too long for mac but also win" (run 39943332027, setup-v0.4.156: macOS 21.8 min,
  Windows 15.8, Linux 8.7 and 8.1).
  - Every step that mattered was a compile: macOS x86_64 11.2 + aarch64 9.0 min back to
    back, Windows 14.7, Linux 8.0. Signing took under a minute.
  - Each build job logged `No cache found`. Its Swatinem cache saved only on non-tag runs,
    and release-agent.yml's lockstep job dispatches this file on the TAG ref. The one
    master run (10-14 06:10Z) seeded it, and setup-v0.4.135 two hours later was the last
    warm release (full match in all three jobs: macOS 10.3 min, Windows 8.4, Linux 3.9).
    From 0.4.140 to 0.4.156 every sampled job (0.4.140, .145, .150, .155, .156) found no
    cache, and the pool held no `setup-*` entry. 22 wizard releases ran in those nine
    days, at 16–41 min each.
  - The warm floor is not the seed's to remove: the wizard links `roomlerd`, `roomler-cli`
    and `remote_control`, and those workspace crates rebuild at release opt-level on every
    run (4.3–4.4 min per macOS triple, 6.7 on Windows, on 0.4.135).
  - Two rehearsals on the branch:

    | job | rehearsal 1 (39955191719), cold + publish | rehearsal 2 (39958936515), restored | 0.4.156 release |
    |---|---|---|---|
    | macOS | 23.1 min (build 20.6) | 10.6 (build **8.2**) | 21.8 |
    | Windows | 11.0 (build 9.3) | 7.6 (build **5.5**) | 15.8 |
    | Linux x86_64 | 9.2 (build 7.8) | 24.8 (build **3.3**) | 8.7 |
    | Linux aarch64 | 8.4 (build 6.1) | 4.6 (build **2.8**) | 8.1 |

    Every job in rehearsal 2 logged `restored from: seed-setup-…`. The seeds' target
    archives are 573–801 MiB (macOS, two triples: 801 MiB, 41% of the per-asset cap).
    Publishing cost 0.5–1.1 min a job, which a tag run never pays. One cargo invocation for
    both macOS triples built in 8.2 min, against 4.4 + 4.3 for two on 0.4.135.
  - ⚠️ Rehearsal 2's Linux x86_64 job took 24.8 min for a 3.3-min build. Its raw `apt-get`
    fetched 61.2 MB in 19 min 34 s (52 kB/s) from azure.archive.ubuntu.com, root cause 4
    again: the release lane's Linux jobs have installed from a cached apt set since wave
    19, and the wizard's never did. Both now do (`setup-linux-u2404-v1`,
    `setup-linux-arm64-u2204-v1`, each keyed to its runner's Ubuntu release), with the
    release lane's pkg-config guard and `apt-get` fallback.
  - Rehearsal 3 (39963375648): both apt caches missed and were saved (127 MB, 112 MB), and
    the guard did not fall back.
  - Rehearsal 4 (39965573505), **10.2 min wall-clock**: both apt steps were hits (5–6 s),
    and every seed restored.

    | job | rehearsal 4 job | build step | a tag run (no publish) | 0.4.156 release |
    |---|---|---|---|---|
    | macOS universal | 7.6 min | 5.6 | ~6.8 | 21.8 |
    | Windows x86_64 | 9.9 | 7.3 | ~8.8 | 15.8 |
    | Linux x86_64 | 4.7 | 3.5 | ~4.2 | 8.7 |
    | Linux aarch64 | 4.4 | 2.8 | ~3.7 | 8.1 |

    Across rehearsals 2–4, warm builds ran 5.6–9.1 min on macOS and 5.5–7.3 on Windows;
    the spread is runner speed. Windows is the wizard lane's long pole now.
- 2026-10-24: **a review of every lane** (wave 27). Master on 10-24, mean job times over
  13 CI runs, 12 integration runs and 7 installer runs:

  | lane | job | mean | max | against 10 min |
  |---|---|---|---|---|
  | CI | Windows overlay clippy | 11.2 | 13.3 | over |
  | CI | Rust lint | 9.0 | 11.4 | close |
  | CI | macOS overlay check | 8.6 | 12.1 | close |
  | CI | the other six jobs | 4.2–8.4 | | under |
  | integration | each of three shards | 12.4–12.6 | 15.5 | over |
  | installer smoke | MSI / macOS pkg | 10.7 / 9.8 | 13.4 / 11.6 | close |

  Every cache restored warm; nothing was cold by mistake. The jobs grew: Windows overlay
  clippy now carries FR-90 P1i's agent-session tests (4.3–4.7 min) and FR-99 P1b's privacy
  curtain (2.8–3.1 min), and lint runs eight clippy passes. The operator chose four levers.
  This wave takes the two in PR CI: a fourth integration shard, and the agent-session
  tests in their own Windows job.
  - Wizard tag runs were 10–15 min end to end (was 16–41), but two defects showed. Every
    release since 0.4.164 ran the wizard TWICE (the `setup-v` tag push, then the lockstep
    dispatch 13–25 min later). The cached apt set hit in only two of six tag runs: master's
    copy was LRU-evicted twice (the wizard touches it a few times a day, so a CI rotation
    that pushes the pool past 10 GB takes it first), and ubuntu-latest's move to 26.04
    changed its key. One miss waited 19.2 min on the mirror. Both are the next wave.
  - ⚠️ ubuntu-latest is 26.04 now for some jobs (the 0.4.165 wizard built there). Both
    0.4.163's and 0.4.165's x86_64 wizards need GLIBC_2.39, so nothing broke yet. But
    that job floats with the image, and the binary has never run on 22.04 or Debian 12,
    where the arm64 wizard and the agent `.deb` pin 22.04 for exactly that floor.
- 2026-10-24, later: **wave 27 on its PR, and wave 28.**
  - Wave 27's PR run (#2844): Windows overlay clippy 7.0 min (11.2 mean that day), the new
    `windows-agent-sessions` 7.6 cold. The integration suite listed 973 tests (875 on
    10-20); four shards were handed 244/243/243/243, each reported passed + ignored ==
    handed, and the jobs took 10.8–11.9 min. Per-shard suites ran 5.6–6.9 min, so the build
    (3.3–4.2) is now the larger part of a shard.
  - **The double run.** Since 0.4.164 a `setup-v` tag was pushed with a token that
    triggers workflows, so release-setup.yml started through `push: tags`. Then
    release-agent.yml's lockstep job found the tag at the right commit, reused it, and
    dispatched again: 0.4.164 at 15:51 (push) and 16:04 (dispatch), 0.4.165 at 17:37 and
    18:02. The skip query against the real tags: `setup-v0.4.165` = 2 (in flight or green),
    `setup-v0.4.163` = 1, an unknown tag = 0.
  - **The apt cache.** Wave 26's cache-apt-pkgs-action entries hit in two of six tag runs
    on 10-24. Master's copy of `cache-apt-pkgs_208e5ef8…` was gone by 10:14 (LRU), and
    ubuntu-latest's 26.04 changed the key to `…7ae18ca6…`, whose master copy was gone
    again before 17:37. 0.4.164's dispatch run missed and waited 1,154 s on the mirror.
  - Apt rehearsal 1 (40123098836): no seed yet. x86_64 (ubuntu-26.04) fetched 72.5 MB in
    17 s and published `apt-setup-linux-ubuntu26.04-X64` (181 .debs, 69 MiB); arm64
    (22.04) fetched 51.6 MB in 4 s and published `apt-setup-linux-arm64-ubuntu22.04-ARM64`
    (121 .debs, 49 MiB). apt kept its downloads, so the save step found them.
  - Apt rehearsal 2 (40125690002): `restored 181 .debs` → `Need to get 0 B/72.5 MB` on
    x86_64 (apt step 62 s, mostly unpack and configure), `restored 121 .debs` →
    `Need to get 0 B/51.6 MB` on arm64 (25 s). A mirror stall can no longer reach a tag run
    except through the packages Ubuntu changed since the last rehearsal.
- 2026-10-25: **wave 28 on the first release, and the case for wave 29.**
  - 0.4.168 (tag run 40144260589): one wizard run (40145679114). The lockstep created
    `setup-v0.4.168` itself, so the new skip branch has not run in the field yet. Both
    Linux wizard jobs restored their apt seed: x86_64 `restored 181 .debs` →
    `Need to get 0 B/72.5 MB` (61 s), arm64 121 → `0 B/51.6 MB` (32 s). The tag run
    skipped the publish step, as designed.
  - **Where an agent release's time goes**, in the 0.4.171 rehearsal (40217908387):
    - The MSI job took 19.0 min, 13.8 of it the build step. The .pkg job took 22.4 min,
      18.2 of it the build step.
    - Both seeds restored (2,963 MB and 2,845 MB of target, kept intact). Only about 20
      crates compiled, all ours or the vendored webrtc chain.
    - After the last `Compiling` line (`roomler-node-core`) the build ran on for 12.3 min
      on Windows and 17.3 on macOS. That tail is `roomlerd` itself (353 files, 13 MB of
      source) at `codegen-units = 1`.
  - macOS is noisy: the same ~20-crate build took 8.3–19.2 min across the 35 runs of
    10-24/25. The Windows build step held 13.2–14.6, apart from a few runs at 8.9–9.8
    (faster hosts).
  - **The tag recompiles its rehearsal's tree.**
    - Rehearsal and tag trees compared for 0.4.163, 0.4.166, 0.4.168, 0.4.169, 0.4.170 and
      0.4.171: identical every time.
    - 0.4.171's tag (40219775103) then compiled for another 13.6 min (MSI), 14.0 (pkg) and
      9.7 (.deb), and the whole run took 20.4 min.
    - Why: cargo judges a path crate by mtime, and a checkout makes every file new.
  - **Wave 29 probe** (40228482936). It is a temporary push-triggered workflow on
    `exp/fr6-w29-probe` that restores seeds but never saves one. Each job ran the real
    release setup and build, then the tag run's case on the same runner: stamp, make every
    file new, two negative controls, the positive case, and a rebuild.
    - Windows:
      - A modified tracked file blocked reuse, and so did another job's stamp.
      - The same tree and job reused the build. The rebuild took **1 s**, against 565 s
        for the release build before it, and rewrote no artifact.
      - The reused `roomlerd.exe` reported `roomlerd 0.4.171`.
    - macOS:
      - The same four results; the rebuild took 4 s, against 861 s.
      - The second libvpx pass found the same Cellar (`/opt/homebrew/Cellar/libvpx/1.17.0`)
        and kept its build.
      - The version check then failed: the `roomlerd` in `target/` links
        `@rpath/libavcodec.63.dylib`, which only the staged `.app` resolves
        (`dyld: Library not loaded`). Fixed in #2914: the check now gets the vendored FFmpeg
        dir as its library path, and falls back to the version string compiled into the
        binary. The fix was re-probed in run 40230929108; its result goes in the next
        entry, with 0.4.172's field run.
    - **codegen-units for our crates only** (dependencies stay at 1), Windows, all three
      builds on one runner:

      | codegen-units | build | `roomlerd` lib + bin | `roomlerd.exe` bytes | gzip -9 |
      |---|---|---|---|---|
      | 1 (shipped) | 565 s | 322 + 172 s | 64,755,712 | 22,479,787 |
      | 4 | 426 s | 231 + 122 s | 75,412,480 (+16.5%) | 25,548,333 (+13.7%) |
      | 16 | 463 s | 250 + 140 s | 80,180,736 (+23.8%) | 26,865,851 (+19.5%) |

      On four cores, 4 units beat 16. The macOS release build spent 564 s in the
      `roomlerd` lib and 229 s in its bin (`main.rs`) of 858 s. The two run one after the
      other, and each is a single crate.
- 2026-10-26: **wave 29 field-verified on agent-v0.4.173.**
  - The macOS re-probe (40230929108) passed. The fixed check ran the reused binary
    with the vendored FFmpeg as its library path: `reused build reports 'roomlerd 0.4.171'
    (want 0.4.171): OK`. On macOS, codegen-units 4 took 494 s and 16 took 439 s, against
    766 s at 1, for +14.2% and +23.7% gzip size.
  - **The real workflow, before any release depended on it:**
    - The post-merge master reseed (40244838141, `5b55f04c2`) stamped all 8 build jobs
      (`tree=1a584f76… job=Release roomlerd/<job>`), the two Windows jobs included, so a
      fresh Windows checkout reads as clean.
    - A same-tree dress rehearsal (40247220674, on a temporary branch at `5b55f04c2`)
      then reused in all 8 jobs. The daemon build steps took 3–43 s, and all four version
      checks passed, the two Linux jobs that no probe had covered included.
  - **0.4.173**, cut by roomler-ai-7e at `ffffdacb7` (tree `99bc30a6276b`), with no release
    dispatch between rehearsal and tag:
    - Rehearsal 40249389321: every job found the reseed's stamp for another tree,
      rebuilt as before, and stamped its own: 8 of 8.
    - Tag 40250842840: 8 of 8 jobs reused the build, and the 4 daemon jobs reported
      `roomlerd 0.4.173`.

    | job | 0.4.172 tag | 0.4.173 rehearsal | 0.4.173 tag | build step |
    |---|---|---|---|---|
    | .msi (Windows) | 17.6 min | 16.1 | **4.0** | 38 s |
    | .pkg (macOS) | 12.8 | 16.3 | **2.0** | 8 s |
    | .deb (Linux x86_64) | 11.5 | 12.4 | **1.9** | 6 s |
    | .deb (Linux aarch64) | 10.7 | 11.8 | **1.4** | 4 s |
    | the four companions | 3.7–6.2 | 4.4–7.0 | 3.0–4.2 | 114–149 s |

  - The build jobs' runner time fell from about 72 min to about 24.
  - End-to-end, the run took 17.0 min against 18.7. The jobs queued for runners: the first
    build job started 3 min in, and the MSI job 10 min in. Their own critical path was
    about 7 min.
  - ⚠️ The companions still recompile `roomler-desktop`. Tauri regenerates its gitignored
    `gen/schemas/` (#2894), so its build script reruns on a fresh checkout. Carrying
    `agents/*/gen/` in the seed is the open fix.
- 2026-10-26, later: **0.4.174 lands in 7.2 min, and the companions' recompile is explained
  and fixed (wave 30).**
  - agent-v0.4.174 (tag run 40287645036) ran 07:44:29 → 07:51:43Z: **7.2 min** on the clock,
    so the ≤10 min criterion is met.
    - All 8 build jobs reused rehearsal 40267019868's build. It ran at the same SHA
      `065e15308`, 3.7 h earlier, with no release dispatch between them.
    - Daemon jobs: MSI 4.0 min, `.pkg` 3.1, `.deb` 1.6 and 1.5. The longest jobs were the
      companions: Windows 4.2, aarch64 4.0.
  - **Correction to the previous entry: `gen/schemas/` was not the cause.**
    - Probe 40299709033 ran three Windows jobs, restoring seeds but never saving one, and
      rebuilt with `-v` after a reuse.
    - In all three variants it printed `Dirty roomler-desktop v0.4.174: the file
      'agents\roomler-desktop\capabilities' is missing`. The variants were: nothing
      restored; `gen/` restored with its mtimes; `gen/` restored and marked old.
    - `tauri_build::build()` declares `cargo:rerun-if-changed=capabilities` unless the app
      sets a capabilities pattern (tauri-build 2.6.2, `acl::build`). The directory had never
      existed. So every build of `roomler-desktop` re-ran its build script and recompiled it:
      local, CI, rehearsal or reused tag.
  - With `agents/roomler-desktop/capabilities/README.md` in place (probe 40301394043), the
    reused rebuild took **0.9 s** in all three variants, with nothing compiled, against
    157–251 s before. A normal build still passes. The capability set is unchanged and
    empty: the parser loads only `*.json`/`*.toml` files.
- 2026-10-26, midday: **agent-v0.4.175, cut by roomler-ai-df.** Rehearsal 40308879884 and tag
  run 40312190685 were both at `45d52aa36`, with no dispatch between them.
  - Reuse held in every job, as reported by df: MSI 5.3 min, `.pkg` 6.6, `.deb` aarch64
    1.6. The Linux x86_64 build step took 4 s.
  - The run was created at 11:03:40, but its jobs only started at 11:09:59, after 6 min
    waiting for runners.
  - ⚠️ In attempt 1, the Linux x86_64 job **hung 23+ min in "Install system build deps
    (cached apt)"** (`awalsh128/cache-apt-pkgs-action`), from 11:11:16. The same step took
    37 s in the rehearsal and 37 s on the re-run. It had no limit of its own, so only the
    job's 60 min would have ended it.
  - df cancelled it at 11:34:47 and re-ran the failed jobs (attempt 2: 11:36:55–11:38:54).
    The release published at 11:39:58, **36.3 min** after the tag, against 7.2 min for
    0.4.174.
  - Wave 30 bounds both cached-apt steps (8 min + `continue-on-error`) into the existing
    fallback. A hang now costs at most ~8 min plus a direct apt-get, and never the job.
- 2026-10-27: **wave 30 field-verified on agent-v0.4.179** (cut by the stardesk session, with
  #2940 on board, on one SHA for rehearsal and tag).
  - Tag run 40554147480: the tag was pushed at 17:49:08Z and the release published at
    17:58:18Z, **9 min 10 s** from tag to publish. That is under the ≤10 min target for the
    second time, after 0.4.174.
  - None of the four companion jobs has a `Compiling roomler-desktop` line: Windows EXE
    5 min, Linux x86_64 1 min, aarch64 1 min, macOS 3 min. On 0.4.174's tag they had spent
    114–149 s on it. ⚠️ **Wrong, corrected in the next entry:** the Windows EXE and macOS
    jobs did compile it (4 min 03 s, 2 min 26 s). This grep could not match a colour-coded line.
  - Both cached-apt steps finished well inside their new bound: 7 s for the daemon .deb
    and 6 s for the companion .deb. The aarch64 jobs use the uncached step (22 s and 43 s).
  - All 8 build jobs carry the reuse notice. The four daemon jobs printed `reused build
    reports 'roomlerd 0.4.179' (want 0.4.179): OK`. The MSI's ProductVersion reads 0.4.179,
    and its G ROX LTD Authenticode signature is valid.
  - Tags in between, all with the reuse held, as their cutters reported:
    - 0.4.176 (2e) lost its first MSI attempt at "Sign MSIs": `azure/login` could not fetch
      the OIDC token. A re-run of the failed jobs published it.
    - 0.4.178 (df) took 15.0 min from tag to publish, before wave 30: MSI 6.8 min,
      companion EXE 5.8.
  - Next on this lane is FR-123 P2 (#2999, merged as `de813b504`): the daemon compiles as
    one unit, measured −14 % on Windows and −30 % on macOS on the rehearsal's critical
    path. Its field check is 0.4.180's rehearsal.
- 2026-10-27, evening: **agent-v0.4.180 (cut by roomler-ai-2e) published 5 min 28 s after its
  tag, the fastest yet. Also a correction to the 0.4.179 entry.**
  - Rehearsal 40574588069 and tag run 40577974956 both ran at `317f2a6ab`, the first release
    with FR-123 P2. The run was created at 20:25:51Z and the release published at
    20:31:19Z.
    - All 8 build jobs carry the reuse notice. The four daemon jobs compiled nothing and
      printed `reused build reports 'roomlerd 0.4.180' (want 0.4.180): OK`.
    - Three jobs ended together at 20:30:11Z, so they share the critical path:
      - the Windows companion EXE: signing 62 s, then its post step 40 s;
      - the MSI: payload signing 47 s, MSI signing 34 s;
      - the `.pkg`, which waited 119 s for the macOS companion.
  - **Correction to the 0.4.179 entry: two of the four companion jobs did compile
    `roomler-desktop`.** The Windows EXE took 4 min 03 s and the macOS companion 2 min 26 s;
    the two Linux jobs compiled nothing.
    - That check grepped the raw logs. Cargo writes `Compiling\x1b[0m roomler-desktop`, and
      the colour reset between the words means a plain grep for `Compiling roomler-desktop`
      can never match: it counted 0 whatever happened. The cutter's check for 0.4.180 had
      the same blind spot.
    - With colours stripped: on 0.4.180's tag run only the macOS companion compiled it
      (2 min 30 s). The reseed after it (40578859007) did the same (142 s). No Linux
      companion compiled it on either run, nor did 0.4.180's Windows one.
    - ⚠️ Prove a "none" count with a positive control: run the same grep on a log where the
      answer must be at least 1, such as the rehearsal's.
  - **Why the macOS companion recompiles is not yet known.** Probe branch
    `exp/fr6-w31-companion-probe` restores the seed read-only and rebuilds with `-v` and
    cargo's fingerprint log. It never stamps or saves a seed.
    - Runs: v1 40583704652, v2 40584382028, v3 40585289155, v4 40586307513,
      v5 40587490912.
    - v1: the reseed's seed, rebuilt as a tag run does on another runner, was fresh (4.4 s).
    - v2 to v4 each did a rehearsal-style build, then a tag-style rebuild on the same
      runner. All were fresh (0.9–1.5 s), including:
      - with the gitignored `gen/` removed, as a fresh checkout has it;
      - with the seed's own `tar --zstd` round trip in between.
    - v5 had the field's exact shape: a rehearsal-style build on runner A, the target
      packed with `tar --zstd`, restored on runner B, then rebuilt as a tag run does. It was
      fresh too (5.3 s, 0 of the dep-info's 48 inputs newer than its reference).
    - Measured on the way:
      - macOS bsdtar 3.5.3 stores mtimes to the whole second, cargo's reference included
        (`…387.712610` → `…387.000000`). Windows' bsdtar 3.8.9 does the same to a 72- and a
        167-character path, so no input can overtake its reference in the round trip.
      - None of the tauri `OUT_DIR` inputs in the companion's dep-info is missing after a
        restore.
    - So the trigger lives in something only the real release jobs carry: their
      environment (`SEED_REUSE_SAME_TREE`, `ROOMLER_SEED_REUSED_TREE`, the rehearsal branch
      against the tag ref), or the steps around the build (stamp, artifact upload, seed
      save). The way to read it is cargo's own answer in a real job:
      `CARGO_LOG=cargo::core::compiler::fingerprint=info` on the companion build steps, a
      log-only change. It is not on today's critical path: the Windows companion and the
      MSI ended at the same second as the `.pkg`.
  - The reseed's MSI job compiled from scratch: 586 crates, 21 min 38 s. Its restored seed
    held 3,042 MB, over the 3,000 MB cap, and was dropped whole by design. A reseed absorbs
    that cold build, so the next rehearsal starts from a fresh seed.
  - FR-123's reading of the same rehearsal is in its spec, §8.1: −19 % and −12 % of the
    daemon's compile on the two Linux lanes. The MSI build step took 542 s, the fastest of
    the 40 release runs since 10-24.
- 2026-10-28: **wave 31 (#3024), the operator's go: the four companion build steps log
  cargo's fingerprint decisions.**
  - The reseed its merge dispatches builds a new tree, so it recompiles our crates by design.
    Its companion logs are the positive control: each dirty unit must print its reason.
  - The first reused same-tree run after it names the macOS companion's reason: the next tag
    run, or the reseed after a release. Its read goes here.
- 2026-10-28, night: **the reason, read off cargo, and the fix (#3030).**
  - R1, reseed 40602653917 (a new tree), was the positive control. Its macOS companion printed
    22 fingerprint lines, each a source with both mtimes.
  - R2, 40605173133, ran the same tree on a branch pinned to R1's commit. It was **fresh**
    (5.50 s). A reseed's seed reuses cleanly; something only a release's rehearsal does
    leaves the trap. The rehearsal is the one that builds a new version, so every one of
    our units gets a new hash and compiles for the first time.
  - Probe v6, 40605635409, bumped `roomler-desktop` alone to 0.4.999, so its build is a
    first build. The rebuild on the same runner was dirty:
    `stale: changed …/out/tauri-codegen-assets/8068…ba4.js (vs) …/dep-bin-roomler-desktop`,
    `1793145568.351986499 < 1793145569.649334416`.
    - The asset was written 1.3 s after the compile started, by tauri's `generate_context!`.
      It writes the file during the compile, only when missing, and lists it as an input.
    - Restored from the tar on another runner, the rebuild was dirty the same way (`568 <
      569`, the tar keeping whole seconds). It recompiled in 2 min 22 s, the field's figure.
    - That explains every reading: the macOS companion every release; the Windows one on
      0.4.179 and not on 0.4.180, depending on whether the write crossed a second; and the
      once-only recompile, because the file then exists and is never rewritten.
  - The fix: on a same-tree reuse, `seed-tree-reuse` marks every file under
    `target/*/build/*/out/` and `target/*/*/build/*/out/` old, as it does the sources. A
    build script that does rerun rewrites its OUT_DIR, which is new again.
  - Probe v7, 40607295709, applied the fix verbatim after a first build. All three were
    fresh: macOS on the same runner 1.98 s, macOS on another runner 4.87 s, Windows 0.68 s.
  - Field proof owed, on the next release's tag run:
    - no `Compiling roomler-desktop` in any companion job (strip colours first);
    - no `fingerprint dirty` line in their logs.
    Then the `CARGO_LOG` lines come out of the four steps.
- 2026-10-28, morning: **wave 31 field-verified on agent-v0.4.181**, cut by stardesk-teardown-roomler.
  The rehearsal (run 40627954705) and the tag run (40630476314) built the same tree, `c6406facc817`.
  - The rehearsal is the positive control, and it passed. Each of the four companion jobs compiled
    `roomler-desktop` once, and `CARGO_LOG` printed 12 fingerprint lines there. On the first build of
    a new version these read `fingerprint error … failed to read …/bin-roomler-desktop`, not
    `fingerprint dirty`, because there is no previous fingerprint to compare.
  - **The tag run is clean.** In all four companion jobs there is no `Compiling roomler-desktop` and
    no fingerprint line, the only `CARGO_LOG` trace being the env line. Against 0.4.180:
    - the macOS companion job took 52 s, against 184 s;
    - the Windows companion took 111 s, against 223 s;
    - the `.pkg` took 130 s, against 247 s, because it no longer waits for the companion.
    The four daemon jobs reused as before and reported `roomlerd 0.4.181`.
  - **The reseed after it is clean too.** Run 40631116972 on master, the same tree: all four
    companion jobs reused, with no compile and no fingerprint line. On 0.4.180 this run had recompiled
    the macOS companion (142 s).
    - Two of its daemon jobs compiled cold anyway. Their seeds had passed the 3,000 MB cap (Linux
      x86_64 at 3,306 MB: 580 crates, 12 min 47 s) and were dropped whole, as designed. A reseed
      absorbs that cost, so no release pays it.
  - **Tag to publish took 6 min 28 s** (04:06:56 → 04:13:24Z). The critical path is now the MSI: its
    job waited 70 s for a Windows runner, then took 274 s. The companion fix shortens the `.pkg`'s
    path, not this one.
  - The diagnostic came out of the four steps again (#3037). To read a future recompile, put
    `CARGO_LOG=cargo::core::compiler::fingerprint=info` back on the step, or run `cargo build -v`.
- 2026-10-28, afternoon: **agent-v0.4.182, cut by stardesk-teardown-roomler.** Wave 31 held on its
  second release. Rehearsal 40723384188, tag run 40730055980 and the bump's squash `e0a445f9c`
  all had tree `8048f68fca0c`.
  - The rehearsal compiled `roomler-desktop` once in each companion job: the positive control.
  - **The tag run is clean.** All 8 jobs carry the reuse notice. The four daemon jobs compiled
    nothing and reported `roomlerd 0.4.182`. The four companion jobs compiled nothing either.
  - **Tag to publish took 13 min 09 s**, all of it runner queueing, none of it builds. The run was
    created at 16:51:21Z and validated by 16:51:52Z. The Linux jobs started at 16:51:54–16:52:53Z,
    the Windows companion at 16:53:37Z, the MSI at 16:53:58Z, the macOS companion at 16:58:44Z, and
    the `.pkg` only at 17:00:01Z, 8 min after validation. The release published at 17:04:30Z.
    Each build job itself took 99–339 s.
  - The reseed after it (40732757710, the same tree): every companion job reused with no compile,
    and the three daemon jobs that reused (the `.pkg` and both `.deb`) reported `roomlerd 0.4.182`.
    Its MSI compiled cold: the seed held 3,024 MB, over the 3,000 MB cap, and was dropped whole
    (586 crates, 20 min 47 s).
  - That is the third post-release reseed in a row to drop the Windows seed, after 0.4.180 (3,042 MB)
    and 0.4.181 (3,022 MB). Each cycle (reseed, rehearsal, tag, reseed) grows it past the cap again, so the cap
    now costs one cold Windows build per release. It lands on the reseed, never on the tag run. The
    next lever: prune units older than the last few generations instead of dropping the whole target.
    Not started.
  - Two seeds are at the cap's edge: the `.pkg` at 2,995 MB and the `.deb` aarch64 at 2,902 MB, so
    the next rehearsal or reseed will likely drop them and compile those jobs cold once. A tag run
    skips that check ("tag run — skipping accretion GC"), so no release pays it.
- 2026-10-28, evening: **agent-v0.4.183, cut by stardesk-teardown-roomler.** Wave 31 held on its
  third release. Rehearsal 40742354169, tag run 40751997218 and the bump's squash `4bbd94e5d` all
  had tree `7daedbf21ffd`.
  - The rehearsal compiled `roomler-desktop` once in each companion job: the positive control.
  - **The tag run is clean.** All 8 jobs carry the reuse notice and compiled nothing, and the four
    daemon jobs reported `roomlerd 0.4.183`.
  - **Tag to publish took 16 min 00 s** (19:11:09 → 19:27:09Z), again almost all of it runner
    queueing. The `.pkg` waited 8 min 12 s for a macOS runner (0.4.182's waited 8 min 09 s), then
    took 343 s, 10 s of it the build step. On both releases the ≤10 min mark failed on macOS runner
    queueing alone.
  - The reseed (40754452329) ran on master `0a70ac83b`: a docs PR merged after the bump, so its tree
    differed from the tag's and every job compiled, as designed for a new tree. It dropped three
    seeds over the cap, the MSI's at 3,025 MB (the fourth reseed in a row), the `.pkg`'s at
    3,332 MB and the `.deb` aarch64's at 3,290 MB; those jobs compiled 574–580 crates cold, and the
    run took 31 min. Wave 32 raises the four daemon jobs' cap to 4,500 MB.
  - The rehearsal's `.pkg` build step read 1,519 s, a new high: the daemon lib's codegen took
    1,374 s against 672 s on 0.4.182. The reseed compiled the same code on another runner in 713 s,
    so it was the runner (FR-123 §8, decision 8).
- 2026-10-29, small hours: **agent-v0.4.184, cut by fr94-followup-issues.** Wave 31 held on its
  fourth release.
  - **The tag run is clean** (40792470875). All 8 jobs carry the reuse notice and compiled nothing,
    the four daemon jobs reported `roomlerd 0.4.184`, and no companion compiled. Tag to publish
    took 5 min 32 s (00:12:04 → 00:17:36Z).
  - **The post-release reseed is wave 32's red** (40793113939, at the tag's tree, the old cap). The
    MSI restored `target is 3082 MB (> 3000) — dropping it whole` and compiled 586 crates cold,
    26.3 min, where it could have reused. The other seven jobs kept their seeds and reused. It was
    the fifth reseed in a row to drop the Windows seed. The `.pkg` job's first attempt failed only
    in its Notarise step (Apple's stapler, `-1005`), after its seed had published; a re-run passed.
- 2026-10-29, small hours: **wave 32 (#3068) merged as `5baaf79cb`.** The reseed it dispatched
  (40797998801) printed the cap in effect in every job: `(cap 4500)` on the four daemon jobs and
  `(cap 3000)` on the companions. The macOS runners had ~40 GB free, so a bigger seed is no disk
  risk there.
- 2026-10-29, early morning: **agent-v0.4.185, cut by fr94-followup-issues.**
  - The rehearsal (40808801238) could not tell the two caps apart: every daemon seed it restored
    was under 3,000 MB. It corrected the growth model instead. The MSI restored exactly 2,668 MB,
    although #3068's reseed had recompiled 20 crates at 0.4.184 in between, so a seed grows per new
    version, not per build.
  - **The tag run is clean** (40812002848): reuse in 8 of 8 jobs, nothing compiled, `roomlerd
    0.4.185` in 4 of 4, no companion compile. Tag to publish took 6 min 38 s (03:24:58 →
    03:31:36Z); the MSI was the longest job, at 263 s.
  - **The post-release reseed is wave 32's green** (40812653908, at the tag's tree). It kept the MSI
    seed at 3,037 MB and the `.deb` x86_64 seed at 3,367 MB, both over the old cap, and every job
    reused the build with 0 compiles. The MSI job took 6.0 min and the whole run 6.7 min, against
    26.3 min for the MSI's cold build one release earlier.
- 2026-10-29, evening: **agent-v0.4.187, cut by roomler-ai-de.**
  - **The tag run is clean** (40929759871): reuse in 8 of 8 jobs, nothing compiled, `roomlerd
    0.4.187` in 4 of 4, no companion compile. Tag to publish took 5 min 33 s (18:19:47 →
    18:25:20Z).
  - **The post-release reseed** (40930679656) kept all four daemon seeds under 4,500 MB and reused
    the build: MSI 3,765, `.deb` x86_64 4,153, `.pkg` 3,386 and `.deb` aarch64 3,321 MB. The
    Windows companion EXE's seed reached its 3,000 MB cap for the first time (2,877 → 2,972 →
    3,068 MB over three reseeds) and was dropped whole, so its build ran cold, 6 min 6 s, after the
    release had published.
  - That is the cycle `docs/build-caches.md` §4 describes, not a regression. A tag run skips the
    GC, and a rehearsal restores a seed the last reseed already checked, so a drop lands on a
    reseed. At ~390 MB a release, the `.deb` x86_64 seed should drop on 0.4.188's reseed and the
    MSI's about two releases later.
- 2026-10-30: **closed**, with every criterion ticked and the docs merged in #3107 (`d738146ad`).
- 2026-10-30, afternoon: **reopened for wave 33.** The operator flagged the 0.4.188 bump PR's CI
  (run 41063619620, 37.5 min), which held the release. The bump PR opened at 13:29Z, its CI was
  green at 14:06, and the release published at 14:21:22, 12 min after the tag.
  - **Its long pole was the Actions cache refusing a download** (Open decisions). `Rust unit
    tests` waited 9.4 min for a runner, got `Failed to restore: Egress is over the account
    limit.`, and compiled 1,795 crates cold in 27.0 min. The same lane went cold the same way in
    run 41056324624 (12:46Z, 23.4 min).
  - **Behind it sat a regression in this FR's own caches.** From #3125 (10:46Z) on, `Windows
    overlay clippy` restored `ci-windows-clippy-Windows-X64` at 4,440 MB, over its 4,000 MB
    cap, dropped it and built cold: 297 + 329 + 42 + 821 + 252 units across its five steps,
    the wizard's step alone 503 s. Each master run then published `pruned target: 5809 MB ->
    4440 MB` again (41063266842, 41069134834). The lane took 7.4–8.3 min in the three runs before #3125
    (41041779768, 41042348489, 41042618394) and 17.0–22.6 min in all 20 runs checked after it.
    So even without the refused download, the bump PR's CI would have taken ~22 min. Wave 33
    (#3153) is the fix.
  - **The operator chose two of the three egress levers**: move the Linux lanes to `seed-cache`
    (wave 34), and skip the heavy lanes on docs-only PRs (wave 35). A bigger plan was not chosen.
- 2026-10-30, 15:31Z: **wave 33 (#3153) merged as `4c43b714a`.** Its own PR run (41076872429)
  restored `ci-windows-clippy-Windows-X64: target 4440 MB, key changed` and kept it, compiled or
  checked 98 units instead of ~1,740, and took 11.2 min against 17.0–22.6 cold. The wizard's step
  took 179 s, against 503 s cold. The macOS overlay check (13.8 min) is now each run's floor.
- 2026-10-30, 16:00Z: **wave 33 holds.** Master run 41081498611 (attempt 1, at the merge)
  restored `ci-windows-clippy-Windows-X64` at 4,440 MB and kept it under the new 6,000 MB cap,
  compiled or checked 98 units, republished `pruned target: 5810 MB -> 4440 MB`, and its Windows
  clippy job took 13.2 min. #3157's PR run (41085608470) then restored the same cache `same key`
  in 10.4 min, against 17.0–22.6 min cold.
- 2026-10-30, 18:07Z: **wave 34 (#3157) merged as `224c79615`.** The three master runs queued
  behind the next merges were cancelled before they started, so the first to run it was
  41107105820 (`ff0074175`, 18:27Z). Every Linux family started cold (`no ci-<family>-Linux-X64
  cache on the seed-cache release yet`) and published:

  | Family | Pruned target | Job |
  |---|---|---|
  | lint | 6,830 → 6,081 MB | 27 min 19 s |
  | unit | 14,068 → 10,373 MB, two parts | cut off at 30 min 15 s (wave 36) |
  | agent | 4,944 → 3,268 MB | 14 min 18 s |
  | profiles | 2,588 → 2,445 MB | 9 min 1 s |
  | recording-ffmpeg | 3,120 → 1,848 MB | 7 min 2 s |
  | recorder-audio | 3,172 → 1,601 MB | 7 min 49 s |
  | ffmpeg-encoder | 2,198 → 1,675 MB | 4 min 43 s |

  - **The caps stand.** Every cache sits at 32–65 % of its cap, and no `CI cache over its cap`
    warning fired.
  - **The unit lane's publish raced its ceiling.** Its 2,587,763,998-byte archive went up as two
    parts, and the 30-minute ceiling cut the job off with the second part still uploading. That
    part landed 2 s later, but "Rust checks" failed on that master run. Wave 36 is the fix.
  - **Restores:** the next master run (41111211419, `392f09635`) restored all seven `same key`
    and published nothing new. The unit lane joined its two parts into 10,380 MB and took
    8 min 34 s (73 units). The first PR run after it (41118044702) restored the same seven
    `same key`: unit 7 min 31 s, lint 11 min 4 s, agent 8 min 13 s, the other five
    2.5–7.5 min. Before wave 34, a refused Actions-cache download built the unit lane cold in
    23–27 min.
  - `Recorder tests (FR-85)` restores the shared `recorder-audio` cache under its own key, so it
    always logs `key changed` and builds ~56 units on top. Only `rust-recorder-audio` publishes
    that family, by design.
- 2026-10-30, 20:41Z: **agent-v0.4.189 published 17.0 min after its tag run started, all of the
  gap runner queueing.** The tag run (41122965565) reused its rehearsal in all eight jobs: no
  daemon compiled, `roomler-desktop` compiled in no companion, `roomlerd 0.4.189` in 4 of 4.
  Its build steps took 3–41 s. The MSI job waited 9 min 16 s for a Windows runner and the `.pkg`
  6 min 28 s for a macOS one, against 0–100 s on 0.4.184–0.4.185.
- 2026-10-30, 21:09Z: **waves 35 and 36 (#3170) merged as `ead2192c9`.** The PR's own run
  (41124429682) was a code PR: `What changed` found `.github/workflows/ci.yml` outside the docs,
  set `heavy=true`, and every job ran. The slowest took 10.7 min (Windows clippy).
- 2026-10-30, 21:18Z: **wave 35 holds on this close-out PR (#3184), the first docs-only PR after
  it.** In its run (41130010043), `What changed` read 3 paths, none outside the docs, set
  `heavy=false` and posted the skip notice. The twelve heavy jobs skipped, and the other nine
  passed, "Rust checks" included. The nine ran 3 s to 2 min 40 s each, yet the run took 22 min,
  nearly all of it waiting for runners: `What changed` waited 6 min, the last audits 21. The
  twelve jobs it skipped had taken 84 runner-minutes on #3170's code-PR run, three Windows jobs
  and the macOS one among them.
- 2026-10-30, evening: **closed again**, waves 33–36 shipped; #773's Result comment carries the
  evidence.
