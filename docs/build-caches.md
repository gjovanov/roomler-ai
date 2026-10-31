# Build caches: seeds, same-tree reuse and the accretion GC

How the release lanes keep Rust builds warm, so that an `agent-v*` tag publishes in about
six minutes instead of the hour it took when FR-6 started (#773: ~60–69 min per release,
2026-07). Three mechanisms do the work:
- a **seed store** that a release can always restore;
- **same-tree reuse**, which lets a tag run skip the compile its rehearsal already did;
- an **accretion GC**, which keeps a seed from growing without bound.

All three live in composite actions under `.github/actions/`. Each one fails loudly on the
run page rather than silently going cold. The program's full history, wave by wave, is in
[FR-6](fr/FR-6-ci-release-build-speed-slo.md).

---

## 1. Where the caches live

| Lane | Store | Family per job | Who saves |
|---|---|---|---|
| `release-agent.yml`, 8 build jobs | `seed-cache` release assets | `seed-<shared-key>-<os>-<arch>`, e.g. `seed-agent-windows-vp9-444-Windows-X64` | non-tag runs: rehearsals, post-release reseeds, the Monday 03:17 UTC cron |
| `release-setup.yml`, 4 build jobs | `seed-cache` release assets | `seed-setup-<platform>-…` | non-tag runs |
| `integration-tests.yml`'s shards, `installer-smoke.yml`, every cargo cache in `ci.yml` (its Linux lanes since wave 34) | `seed-cache` release assets (`ci-<family>-…`, through `ci-seed-restore` / `ci-seed-save`) | one per lane; the two recorder lanes share one | master only |
| `ci.yml`'s apt and bun sets | the Actions cache | one per package set | any run |

**Why a release and not the Actions cache.** The 10 GB cache pool kept evicting the seeds.
LRU dropped a *verified* save 29 minutes after it landed (rc.472), and saves made from a tag
ref can never be restored by a later tag. Release assets have no LRU, no budget, no
reservations and no ref scoping.
- Each family is two assets, `<fam>-cargo.tar.zst` and `<fam>-target.tar.zst`. A CI
  family's target archive over the 2 GiB asset cap goes up in numbered 1,900 MiB parts,
  `<fam>-target.tar.zst.part00`, `.part01`, … (`ci-seed-save/action.yml:131`), which the
  restore concatenates (`ci-seed-restore/action.yml:111`). A publish writes one form and then
  retires the other (`ci-seed-save/action.yml:166`). A whole asset wins over parts, and parts
  from two publishes fail zstd's checksum, so a mixed set builds cold rather than wrong.
- `--clobber` on upload is the retention policy: keep the newest one
  (`.github/actions/seed-cache-save/action.yml:72`).
- `--latest=false` keeps the `seed-cache` release from ever becoming the
  `/releases/latest` that fielded agents poll for updates (`seed-cache-save/action.yml:67`).
- After uploading, the save checks through the API that both assets landed, and warns on
  the run page if they did not.

## 2. One release, five runs

```mermaid
sequenceDiagram
    autonumber
    participant C as Cutter
    participant H as Rehearsal (dispatch on the bump branch)
    participant T as Tag run (push agent-v)
    participant S as Post-release reseed (dispatch on master)
    participant W as Setup release and its reseed
    C->>H: open the bump PR (merge freeze), dispatch with publish_release=false
    H->>H: restore seed, accretion GC, compile the new version
    H->>H: stamp target with this tree and job, save the seed
    C->>T: squash-merge the bump (the same tree), push the tag
    T->>T: restore seed, no GC on a tag, same-tree reuse
    T->>T: nothing compiles, version check, sign, package, publish
    T->>S: reseed job dispatches release-agent.yml on master
    S->>S: restore, GC, reuse (master is still the tag tree) or compile, save
    T->>W: dispatch-setup-release tags setup-v and runs release-setup.yml
```

| Run | Restores | GC | Reuses | Compiles | Saves |
|---|---|---|---|---|---|
| Rehearsal | yes | yes | no: a new version is a new tree | our crates only, 6–22 units a job | yes |
| Tag run | yes | **no** | **yes** | nothing | no: a tag-ref save is unrestorable |
| Post-release reseed | yes | yes | yes, while master is still the tag's tree | nothing, or our crates if master moved | yes |
| Weekly cron / `seed-release-caches.yml` | yes | yes | rarely | as needed | yes |

Rehearsals and reseeds share the concurrency group `agent-seed-rehearsal`, with
`cancel-in-progress`: a newer one cancels an older one. A tag run gets a group of its own
and is never cancelled (`.github/workflows/release-agent.yml:88-96`).

> ⚠️ **An open bump PR freezes every merge, docs included.** A merge between the rehearsal
> and the bump's squash gives the tag a different tree from the one the rehearsal stamped,
> so nothing reuses and the tag run compiles everything. Agent code also waits for the
> cutter's "published".

> ⚠️ **A change to `release-agent.yml` or the `seed-cache-*` actions dispatches a master
> rehearsal by itself** (`.github/workflows/seed-release-caches.yml:27-38`). That run joins
> `agent-seed-rehearsal` and cancels, or is cancelled by, whatever rehearsal is running. Merge
> such a change only when no rehearsal or reseed is in flight, and tell the next cutter.

## 3. Same-tree reuse

Cargo judges a path crate stale when any of its files is newer than its last build, and a
fresh checkout makes every file new. So before wave 29, every tag run recompiled all of our
crates, even though its rehearsal had built that exact tree minutes earlier.

1. **Stamp.** After a job's last cargo build, `seed-tree-stamp` writes
   `tree=<HEAD^{tree}> job=<workflow>/<job>` into `target/.seed-source-tree`
   (`.github/actions/seed-tree-stamp/action.yml:84`). A failed or skipped build never stamps.
2. **Reuse.** After restoring the seed, `seed-tree-reuse` reads the stamp and deletes it
   (`.github/actions/seed-tree-reuse/action.yml:34`). If the stamp names this tree and this
   job, and no tracked file is modified, it does three things:
   - sets every tracked file and directory to the mtime 2000-01-01;
   - does the same to the build's own OUT_DIR files (wave 31, `:73`);
   - exports `ROOMLER_SEED_REUSED_TREE` and prints a notice (`:85`).

   In any other case it changes nothing, and cargo rebuilds our crates as usual.
3. **Check.** On a reused build, `seed-tree-stamp` requires the daemon to print the release
   version, `reused build reports 'roomlerd X' (want X): OK`. Where the binary cannot run
   before packaging, it requires the version string inside the binary instead. The macOS
   daemon links an `@rpath` FFmpeg that only the `.app` resolves.

Kill switch: the repository variable `SEED_REUSE_SAME_TREE=off` (`seed-tree-reuse/action.yml:41`).

Reuse is only as good as each unit's declared inputs. Each of these cost a recompile on every
release until it was found:

| Trap | Found in | Fix |
|---|---|---|
| A build script with no `rerun-if` line is rerun on any change | wave 29, `scrap` | give it its `rerun-if` lines |
| A watched path that does not exist counts as changed | wave 30, tauri's `capabilities` | the directory exists (`agents/roomler-desktop/capabilities/README.md`) |
| A unit whose own compile writes an input into its OUT_DIR: tauri's `generate_context!` writes `out/tauri-codegen-assets/<hash>` while `roomler-desktop` compiles, only when the file is missing | wave 31, the macOS companion, ~2.5 min per release | reuse marks OUT_DIR files unchanged too |
| The seed's tar keeps whole seconds, and cargo treats an equal mtime as fresh | wave 31's probes | none needed: it is why the OUT_DIR trap struck Windows only when the write and the compile's start fell in different seconds |

`cargo build -v` prints each unit's `Dirty` reason. For the full story, put
`CARGO_LOG=cargo::core::compiler::fingerprint=info` on the build step.

## 4. Growth control: the accretion GC

Each run restores the previous seed, builds on top of it and saves everything. Without a
sweep, old artifacts pile up: one generation once grew from ~500 MB to 1.4–2.6 GB in four
days. So before every non-tag build, `seed-cache-restore` runs a GC step
(`.github/actions/seed-cache-restore/action.yml:63`):
1. It deletes registry `.crate` files older than four days. They are safe to sweep: a missing
   one is downloaded again.
2. It measures `target`. Over `SEED_TARGET_MAX_MB`, it drops the **whole** target (`:85-90`).

> ⚠️ **Never delete individual files under `target`.** Cargo decides whether to rerun a
> build script from `.fingerprint/`, not from the outputs. Remove an output while its
> fingerprint survives, and cargo silently never rebuilds it: rc.422–424 failed on a seed
> whose `env-libvpx-sys` `OUT_DIR/ffi.rs` had been GC'd. A whole-target drop costs one cold
> build and leaves a correct seed.

| Jobs | Cap | Set at |
|---|---|---|
| Default (companions, everything else) | 3,000 MB | `seed-cache-restore/action.yml:85` |
| The four daemon jobs (`.deb` ×2, MSI, `.pkg`) | 4,500 MB | `release-agent.yml:393`, `:1134`, `:1654`, `:2625` |
| The wizard's macOS universal job (two triples) | 5,000 MB | `release-setup.yml:396` |
| CI caches in the release-asset store (`ci-seed-restore`), default | 4,000 MB | `ci-seed-restore/action.yml:29` |
| The integration shards, `ci.yml`'s macOS overlay check and its Windows clippy lane | 6,000 MB | `integration-tests.yml:168`, `ci.yml:1368`, `:1617` |
| `ci.yml`'s Linux lanes (wave 34): unit, lint, agent; recorder-audio, ffmpeg-encoder, recording-ffmpeg, profiles | 16,000, 10,000, 7,000; 5,000 each | `ci.yml:504`, `:231`, `:655`; `:1170`, `:1928`, `:2108`, `:2601` |

**A cold build has to fit under its cap.** If it does not, every restore drops the cache, every
run builds cold, and each master run publishes the same size again, so the GC never recovers.
#3116's wizard step put the Windows clippy lane there on 2026-10-30: its cold build pruned to
4,440 MB against the default 4,000, and the lane took 17–23 min on every CI run instead of
~8 (FR-6 wave 33). A master run that publishes a CI cache over its cap now warns
`CI cache over its cap` on the run page (`ci-seed-save/action.yml:95`).

**A cold build also has to fit its job's ceiling, publish included.** A run cut off before
it publishes leaves the next run cold as well, and that one is cut off the same way. Wave 34's
first master run built the unit lane cold (1,795 crates) and hit the 30-minute ceiling at
30 min 15 s, with the second part of its 2.6 GB archive still uploading. That part landed two
seconds after the cut. The unit and lint lanes now allow 45 and 40 minutes (`ci.yml:474`,
`:159`), about 1.5 times their measured cold runs (FR-6 wave 36). The macOS overlay check
allows 35 (`ci.yml:1340`): its cold run took 22 min 33 s on 2026-10-30, after its cache crossed
its 6,000 MB cap, against a 25-minute ceiling (FR-6 wave 37). Every other `ci.yml` lane with a
cold run on record allows about 1.5 times it or more. Windows sessions and the FR-85 recorder
lane have none.

**A cache that grows past its cap is the GC at work, not that failure.** The bump to 0.4.191
published the macOS overlay cache at 6,440 MB, over its 6,000, and the warning fired. The next
three macOS jobs dropped it and built cold, in 9.2, 20.9 and 22.1 min, inside the 35-minute
ceiling. The master run among them published 4,664 MB 23 minutes after the oversize publish,
and the job after that was warm. So after that warning, read the next master publish. Under the
cap, nothing is owed. Only a cold build that publishes over the cap again calls for raising it.

**A macOS job's time does not say whether it ran cold.** Two of those jobs built the same 976
crates and started 88 s apart: one took 9.2 min, the other 22.1. Warm jobs on that lane took
6.9–17.3 min. Read the job's log instead: `restored target is N MB (> C) — dropping it whole`
is a cold run, and `restored <family>: target N MB, same key` or `key changed` is a warm one.
A warm job compiled 41 crates against the cold job's 976.

The Linux lanes' caps began as estimates, about 1.5 times each lane's old Actions-cache archive
at the 4.3× unpack ratio the integration lane measures (its 1,181 MB asset restores to
5,077 MB). Wave 34's first master publish (run 41107105820) put every lane at 32–65 % of its
cap: unit 10,373 MB of 16,000, lint 6,081 of 10,000, agent 3,268 of 7,000, and the four
5,000 MB lanes at 1,601–2,445.

**A seed grows per release, not per build.** Cargo names an artifact by the unit's name,
version, features and profile, never by its source. So a rebuild of the same version
overwrites in place, and only a new version adds artifacts: ~350–400 MB per daemon lane per
release. A fresh Windows seed (~2.7 GB) therefore lasts about four releases under 4,500 MB.
Under 3,000 MB it was dropped one release later, every time.

Measured on two consecutive releases (FR-6 wave 32):

| Post-release reseed | Cap | MSI seed restored | Outcome | MSI job |
|---|---|---|---|---|
| 0.4.184 | 3,000 MB | 3,082 MB | dropped whole, 586 crates compiled cold | 26.3 min |
| 0.4.185 | 4,500 MB | 3,037 MB | kept, reused, 0 compiles (the `.deb` x86_64's 3,367 MB too) | 6.0 min |

The GC line reports the cap in effect and the disk left on the volume:
`target is N MB (cap C) — kept intact; F MB free on the volume`, or
`target is N MB (> C) — dropping it whole; …`. Measured on 2026-10-29: macOS runners
~40 GB free, Linux 84–109 GB, Windows 147–222 GB. A compressed seed is 535–818 MiB, far under
GitHub's 2 GiB limit per asset.

## 5. Reading a release run

Cargo prints `Compiling\x1b[0m <crate>`, so a raw grep for `Compiling <crate>` counts 0 even
when the crate compiled. Strip the colour codes first, and check every zero against a
positive control: the rehearsal compiles each companion exactly once.

```bash
gh api repos/<owner>/<repo>/actions/jobs/<job-id>/logs | sed -E 's/\x1b\[[0-9;]*m//g' > job.log
grep -c '##\[notice\]the seed was built from tree' job.log        # reuse
grep -cE '^\S+Z\s+Compiling ' job.log                             # units compiled
grep -oE "reused build reports '[^']+' \(want [0-9.]+\): OK" job.log
grep -oE 'target is [0-9]+ MB.*' job.log                          # GC line, non-tag runs
```

| Line | Means |
|---|---|
| `the seed was built from tree <12 hex> by this job` | same-tree reuse fired |
| `seed carries no source-tree stamp` | the seed was built by a run that never stamped |
| `seed stamp is '…', this job is '…'` | a different tree or job: a normal rebuild |
| `same tree, but tracked files differ from HEAD` | a step modified tracked files before the restore |
| `::warning title=Seed publish did not land::` | the save's own check failed; the next run may build cold |
| `tag run — skipping accretion GC` | expected on every tag run |

A failed rehearsal opens a GitHub issue (`release-agent.yml:3729`), and the next green one
closes it (`:3742`).

## 6. The waves that built this

| Wave | Change | Effect |
|---|---|---|
| 13 | seeds move from the Actions cache to `seed-cache` release assets | no more evicted seeds |
| 29 (#2914) | a tag run reuses its rehearsal's compile | tag to publish 14.9 → 7.2 min (0.4.115 → 0.4.174) |
| 30 (#2940) | the companion's `capabilities/` exists, cached-apt steps bounded | the Linux companions stop recompiling |
| 31 (#3030) | reuse marks OUT_DIR files unchanged | no companion recompiles on a tag run since 0.4.181 |
| 32 (#3068) | the daemon seeds' cap rises to 4,500 MB, and the GC line reports free disk | a reseed reuses instead of a 26-minute cold MSI |
| 33 (#3153) | the Windows clippy lane's CI cache cap rises to 6,000 MB, and a master run warns when it publishes a cache over its cap | ends a cold build of that lane on every CI run since #3125 (17–23 min, against 7.4–8.3 before it) |
| 34 (#3157) | `ci.yml`'s seven Linux cargo caches move from the Actions cache to `seed-cache`, the largest in parts | a download the Actions cache refuses ("Egress is over the account limit") no longer builds a lane cold (the unit lane: 27 min against ~8) |
| 35 (#3170) | a docs-only PR skips the twelve heavy jobs; a push to master still runs every job | a docs PR's CI runs the audits and the frontend build, not 20 jobs (22 of 60 runs on 2026-10-30 were docs, each running all 20 jobs, up to 44 min) |
| 36 (#3170) | the unit and lint lanes' job ceilings rise from 30 to 45 and 40 min | a cold run of either lane finishes its publish (wave 34's first cold unit run was cut off mid-upload at 30 min 15 s) |
| 37 (#3192) | the macOS overlay check's ceiling rises from 25 to 35 min | its cold run (22 min 33 s, measured) finishes its publish |

What is left is runner queueing, not compiling. On 0.4.182 and 0.4.183 the `.pkg` waited
about 8 minutes for a macOS runner, and tag to publish took 13–16 minutes. On 0.4.184 and
0.4.185 every job started within 100 s, and it took 5 min 32 s and 6 min 38 s.
