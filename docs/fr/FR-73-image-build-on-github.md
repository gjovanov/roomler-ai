# FR-73: The prod image is built by GitHub Actions, served from GHCR, and promoted by a dispatch

**Status**: **proposed 2026-09-05** — P0 claim ·
**Owner**: deploy / build ·
**Issue**: [#1389](https://github.com/gjovanov/roomler-ai/issues/1389) ·
**Related**: FR-6 (build-speed SLO — this lane inherits its ≤10 min warm target as an aspiration, not a gate), FR-69 (the publish workflow this one copies its smoke from), FR-37 (the e2e lane, which pins by image tag and gains a second registry to pin from)

## Goal

Move the hosted (prod) image's build and storage off the build host and onto GitHub:

- **build**: a merge to `master` that touches the server, the SPA or the image recipe produces
  `ghcr.io/gjovanov/roomler-ai:hosted-<YYYYMMDD>-<sha7>` with no human step, smoke-booted and
  attested before it is pushed;
- **registry**: the cluster pulls that image from GHCR — the same public package the self-host
  images already live in — instead of the registry container on the build host;
- **promote**: a `workflow_dispatch` bumps the deploy repo's tag, and ArgoCD rolls as it does
  today. Deliberately **not** continuous deployment (D5).

What it replaces: the deploy recipe in `CLAUDE.md` — ssh to the build host, `docker build`
(5–15 min warm, competing with every other project's builds and the registry on the same box),
`docker push` to `registry.roomler.ai`, `docker system prune`, then a hand-edited `newTag` in the
deploy repo. The build host stays the k8s utility worker and keeps its registry for the other
projects; roomler-ai's image simply stops depending on it being healthy or idle.

## Why now

- Every FR of the last month rolled prod at least once, each roll a manual recipe run on a shared
  host. The 2026-07-12 incident (`/` at 100 % from stale build images mid-deploy) is the shape of
  the risk: the build path shares a disk with the registry that serves the cluster.
- The self-host publish workflow (FR-69 P8, `publish-selfhost-image.yml`) already builds this
  exact Dockerfile on Actions, smoke-boots it, pushes it to GHCR and attests it. The hosted lane
  is that workflow with `SAAS=1`, a different tag family and a trigger — not new machinery.
- Measured on Actions: a cold `full` build is **17 min 35 s** (FR-69 AC4). The expected band with
  a working cache is 8–12 min for a Rust change and ~2 min for a UI-only change; the current
  Dockerfile cannot reach either (D6), which is why this FR has a Dockerfile phase.

## Key design — every decision with its alternatives

**D1 — Registry: GHCR.** Alternative: keep `registry.roomler.ai` and have Actions push to it
(it is internet-reachable with basic auth + an acme cert). Pros of the alternative: the cluster's
pull stays LAN-fast. Cons: the registry is still the build host's disk and uptime; a push
credential for it becomes a GitHub secret; it is a second channel next to the GHCR package the
self-host images already use. **Chosen: GHCR** — `GITHUB_TOKEN` suffices (no new secret),
provenance attestation comes for free, and the pull cost is bounded (D8).

**D2 — One package, two tag families.** The hosted image lands in the existing public package
`ghcr.io/gjovanov/roomler-ai` as `hosted-<date>-<sha7>` plus a moving `hosted` pointer.
Alternative: a second package `roomler-ai-hosted`. Pros of two: no self-hoster can pull the
wrong thing by accident. Cons: a second visibility setting, a second retention job, a second
attestation subject. **Chosen: one package** — the tag prefix is the separation; `latest` stays
reserved for the self-host `full` image (the self-host workflow's rule), and the hosted lane
**never** writes `latest`. The hosted image carries the `saas` module (Stripe webhook, newsletter);
it holds no secret (configuration is environment), so its being public changes nothing the AGPL
source does not already disclose — but `docs/self-hosting.md` says plainly that `hosted-*` tags
are not for self-hosters.

**D3 — Visibility: public, as it already is.** Verified 2026-09-05: `ghcr.io/gjovanov/roomler-ai`
answers an anonymous `tags/list` with 200 (tags `latest`, `v0.4.43`, `v0.4.45`, per-arch). A new
tag in a public package is public. Consequence: **the cluster needs no pull secret** for GHCR;
`regcred` stays on the Deployment (harmless, used by nothing) until the build-host registry is
retired for this image. Alternative: a private package with a fine-grained read-only PAT in a
`ghcr-pull` secret — rejected as a rotating credential that buys nothing for public code.

**D4 — Trigger: every push to `master` that can change the image, plus dispatch.** Paths:
`crates/**`, `ui/**`, `Dockerfile`, `files/**`, `config/**`, `Cargo.toml`, `Cargo.lock`, the
workflow itself. `concurrency: hosted-image` with `cancel-in-progress` so a burst of merges builds
only the newest. Alternative: dispatch-only. Pros of dispatch-only: no runner minutes on merges
nobody will roll. Cons: the image is not there when someone wants to roll, which is the wait this
FR exists to remove. **Chosen: on merge** — the repository is public, so standard runners cost
nothing; the cache is registry-backed (D6) so a build that nobody rolls still warms the next one.

**D5 — Deploy: build always, promote by dispatch — not continuous deployment.** A roll replaces a
pod, and every long-lived socket on it re-homes: agents reconnect, RC sessions drop, tunnels and
DERP re-register. `master` takes 10–20 merges on a busy day; rolling each of them is 10–20 fleet
reconnect waves, and each roll is field-verified by hand today. **Chosen:** the build job runs
on every merge; a separate `promote` dispatch (input: the hosted tag, default = the newest) bumps
`newTag` in the deploy repo. Flipping to CD later is one `if:` on the promote job. The bump needs
write access to the private deploy repo: a fine-grained PAT scoped to `roomler-ai-deploy`
(contents: write) stored as `DEPLOY_REPO_TOKEN` — the one operator-created secret in this FR.
Until it exists the promote job prints the exact bump it would have made, and the bump is done by
hand from any machine with the deploy repo (as today, minus the build).

**D6 — Cache: registry-backed BuildKit cache, and a Dockerfile that can use it.** The self-host
workflow uses `type=gha` (10 GB per repo, LRU) scoped per arch and profile; a lane that runs on
every merge would evict the self-host scopes. **Chosen:** `type=registry` at
`ghcr.io/gjovanov/roomler-ai:buildcache-hosted` with `mode=max` — no cap, survives runner
churn, public like the package. And the honest part: the Dockerfile today does `COPY . .` before
`cargo build`, so **any** source change invalidates the Rust layer and the cache buys nothing but
the base image; a UI-only edit rebuilds the whole server. P1b restructures the builder stage:
a `cargo chef` plan/cook pair so the dependency graph (including the mediasoup C++ worker) is a
cached layer keyed by `Cargo.lock`, and the Rust stage copies only the Rust sources (`Cargo.*`,
`crates/`, `agents/`, plus whatever `include_str!`/`build.rs` reach — audited in P1b) so a UI-only
change never touches it. Alternative: `RUN --mount=type=cache` for `target/` — rejected because
cache mounts are not exported by `cache-to` and a fresh runner starts empty every time.

**D7 — Tag scheme: `hosted-<YYYYMMDD>-<sha7>`.** From the git sha, not the image id the build-host
recipe used (`v<date>-<12 hex of the image id>`), so a running pod's tag names its commit.
`VERSION` stays `git describe --tags --always` (needs `fetch-depth: 0`) and `GIT_SHA` the full
sha, both into the OCI labels the Dockerfile already declares.

**D8 — What gets slower, measured, not guessed.** The pull: each of the two high-performance
workers pulls the changed layers over the site's uplink instead of the LAN (~100 MB compressed
for a new binary layer; seconds when only the SPA layer changed). `RollingUpdate` is
`maxSurge 0 / maxUnavailable 1`, so the pull happens one pod at a time and lengthens the roll
without touching availability. AC3 records the per-node pull time from the pod events; if it is
ever the long pole, a registry mirror on the build host (`registry.roomler.ai` as a pull-through
cache of GHCR) is the lever — it keeps the build off the host and only caches the pull.

**D9 — Retention on GHCR.** The build-host registry had `registry-retention.sh` (2 tags per repo,
weekly); GHCR has nothing until someone adds it. A weekly job deletes untagged versions and keeps
the newest N `hosted-*` tags (N = 20, about a fortnight of rolls), never touching `latest`,
`v*`, `*-<profile>` or `buildcache-*`. Alternative: leave it — GHCR storage for public packages is
free, but an unbounded tag list makes `hosted` un-navigable and the e2e lane's pin-by-tag noisy.

**D10 — The build host keeps the break-glass path.** The `CLAUDE.md` recipe survives as the
fallback for a GitHub outage, marked as such; the registry container stays for the other
projects. Nothing on the host is torn down by this FR.

## Phases

| Phase | What | PR | Kill switch | Status |
|---|---|---|---|---|
| P0 | This spec, the ledger row, the issue | [#1390](https://github.com/gjovanov/roomler-ai/pull/1390) | — | ✅ merged `5a9f357a1` |
| P1 | `.github/workflows/hosted-image.yml`: build on merge / dispatch, registry cache, smoke (`/health` = all six modules incl. `saas`, device route 401, SPA served), push `hosted-<date>-<sha7>` + `hosted`, attest, a summary with the measured build time | [#1391](https://github.com/gjovanov/roomler-ai/pull/1391) | `gh workflow disable hosted-image` | ✅ merged `5ef003087` — its merge is the cold run |
| P1b | Dockerfile: `cargo chef` dependency layer + Rust stage copies only Rust sources; base image matches the pinned toolchain; measured against P1's numbers (cold, warm-no-change, warm-Rust-change, warm-UI-only) | [#1392](https://github.com/gjovanov/roomler-ai/pull/1392) | revert the Dockerfile PR — the workflow is indifferent to the layering | dry-run validated on the branch (second attempt — see the field log) |
| P2 | The cluster pulls from GHCR: deploy repo `newName: ghcr.io/gjovanov/roomler-ai`, `newTag: hosted-…`; one roll, field-verified from the fleet; per-node pull time recorded | deploy repo | revert `newName`/`newTag` — the build-host registry still holds the previous tag | |
| P3 | `promote` dispatch: bump `newTag` in the deploy repo with `DEPLOY_REPO_TOKEN`; prints the bump when the secret is absent; refuses while `newName` is not GHCR | [#1393](https://github.com/gjovanov/roomler-ai/pull/1393) | remove the secret | |
| P4 | GHCR retention job ([#1395](https://github.com/gjovanov/roomler-ai/pull/1395)); `CLAUDE.md` deploy section rewritten (Actions path first, build-host path as break-glass); `docs/self-hosting.md` on the `hosted-*` family; the e2e lane doc notes it can pin either registry | | — | |

## Acceptance criteria

- [ ] **AC1** A merge to `master` touching the server or the SPA produces
      `ghcr.io/gjovanov/roomler-ai:hosted-<date>-<sha7>` with no human step; the smoke inside the
      workflow asserts `/health` mounts `chat conference fleet network remote saas`, the device
      route answers 401, and `/` serves the SPA — before the push.
- [ ] **AC2** Build time on Actions recorded for four cases — cold; warm with no source change;
      warm after a Rust change; warm after a UI-only change — before and after P1b. Targets after
      P1b: Rust change ≤ 12 min, UI-only ≤ 4 min (the estimate this FR was opened against).
- [ ] **AC3** Both prod pods run a `hosted-*` image pulled from GHCR; the per-node pull time is
      read from the pod events and recorded; the roll is field-verified from the fleet exactly as
      every roll (online-agent count unchanged, an RC session, an overlay pair, a tunnel forward).
- [ ] **AC4** A `promote` dispatch bumps the deploy repo and ArgoCD rolls; elapsed merge → pods on
      the new image recorded against the 10–15 min estimate.
- [ ] **AC5** `latest` on GHCR still resolves to the self-host `full` image after the hosted lane
      has run; the hosted lane has no code path that writes it.
- [ ] **AC6** The hosted image carries a provenance attestation and
      `org.opencontainers.image.revision` equal to the commit it was built from.
- [ ] **AC7** Retention: `hosted-*` tags are pruned automatically to the newest N; `latest`, `v*`,
      `*-<profile>` and `buildcache-*` are never touched (asserted in the job).
- [ ] **AC8** The break-glass path is documented and was exercised once after the switch (a
      build-host build pushed to the old registry, not deployed).

## Open decisions

- N for retention (20 proposed).
- Whether the `promote` job should also wait for `/health` on the public URL and post the roll
  to the FR issue of the change being rolled — nice, not required.
- Whether the e2e nightly's pin should move to `hosted-*` tags (it pins "the current prod tag" by
  reading the deploy repo, so it follows automatically; the doc just needs to say so).

## Out of scope

Continuous deployment (D5 keeps the human on the trigger); multi-arch hosted images (the cluster
is amd64); moving the deploy repo or ArgoCD; the agent releases (they have their own workflows);
retiring the build-host registry for the other projects; a registry mirror on the build host
(named in D8 as the lever if the pull ever matters).

## Field-verification log

_(appended per phase — the numbers, wrong turns included)_
