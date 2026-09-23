---
name: ship-it
description: How roomler releases and deploys — the hosted server image (Actions builds, GHCR serves, a dispatch promotes), the agent release lane (MSI/.deb/.pkg, code signing, GPG sidecars, the updater's trust chain), the break-glass build-host path, and the couplings that silently freeze the whole fleet's updates when they drift. Load BEFORE cutting an agent-v*/setup-v* tag, promoting or rolling back a server image, touching release-agent.yml / hosted-image.yml / promote.yml / the Dockerfile / the wxs installer, or changing a version number or signing step. FR-73 / FR-7.
---

# Shipping: the server image and the agent release

Reference docs: **[`docs/deployment.md`](../../../docs/deployment.md)** (server) and
**[`docs/installation.md`](../../../docs/installation.md)** +
**[`docs/code-signing.md`](../../../docs/code-signing.md)** (agent). This file is
the set of couplings that fail *silently*.

## 1 · Build ≠ deploy

The hosted image is built by GitHub Actions on **every merge to `master`** that
can change it (`hosted-image.yml`: `crates/**`, `ui/**`, `files/**`, `config/**`,
the two installer scripts, the Dockerfile, `Cargo.*`), smoke-booted with Mongo +
Redis, attested, and pushed as
`ghcr.io/gjovanov/roomler-ai:hosted-<YYYYMMDD>-<sha7>` plus the moving `hosted`
pointer.

**Nothing rolls until someone promotes.**

```bash
gh run list --workflow hosted-image.yml --limit 3        # find the image a merge produced
gh workflow run promote.yml -f tag=hosted-20260905-5ef0030   # empty tag = whatever `hosted` points at
# then FIELD-VERIFY from the fleet — see §5
```

⚠️ The lane **never writes `latest`** — that is the self-host `full` image (no
`saas`), owned by `publish-selfhost-image.yml`.
⚠️ `promote` runs in the **`release` environment**, whose secret
`DEPLOY_REPO_TOKEN` it uses after proving write access with a dry-run push. **An
environment secret is invisible to a job that does not declare the environment,
and the symptom is the job's "not set" branch, not an error.** Without it the job
prints the exact manual bump.
⚠️ It refuses anything that is not an existing `hosted-*` tag, and refuses while
the deploy repo's `newName` is not `ghcr.io/gjovanov/roomler-ai`.
⚠️ The e2e lane (`scripts/e2e-nightly.sh`, `e2e-run.sh`) reads the registry from
the deploy repo's `newName` — a bare tag is resolved against it; a full reference
containing `/` pins anything.

ArgoCD reconciles the deploy repo's `master` with **Automated + selfHeal +
prune** and a webhook: a push rolls out within ~5 s. The cluster pulls from GHCR
with **no pull secret** (the package is public; `regcred` is vestigial).

## 2 · The couplings that freeze the fleet

These do not error. They make every device quietly stop updating.

| Coupling | What drift does |
|---|---|
| `msi_product_version_for` (in `roomlerd`) **vs** the "Derive the MSI ProductVersion" step in `release-agent.yml` — the same `MAJOR.MINOR.RC` mapping, **twice** | Change one and **every agent refuses every update**: a silent fleet-wide freeze, not an error. Re-check with `cargo test -p roomlerd --lib -- --ignored real_published_msi` |
| The GPG sidecar job | Non-Windows `download_asset` **requires** `<asset-url>.asc` and refuses fail-closed. A release whose GPG job skipped freezes every Linux/macOS agent. `release-agent.yml` now hard-fails on a tag when `GPG_PRIVATE_KEY` is absent (graceful skip only on a branch dispatch), plus a post-signing assertion that every `.deb`/`.pkg`/`.msi` got a sidecar |
| The version scheme (rolling `0.4.<counter>`) | Six coupled MSI-mapping copies. Agents ≤ rc.483 **refuse** 0.4.x MSIs |
| Asset ORDER in a release | A frozen agent's picker takes the **first** `.deb` matching its arch, and `/api/agent/latest-release` forwards GitHub's order. Publishing the Linux companion `.deb` into the agent release froze every pre-0.4.16 Linux agent — they apt-installed the companion *as* their daemon update. Fixed server-side (`agent_release::order_assets_daemon_first`) **because a frozen agent cannot receive an agent-side fix** |

⚠️ **A tag can publish nothing.** Always check `gh release view <tag>` assets
before believing a release shipped; re-cut on a **fresh** number rather than
re-pushing a tag.
⚠️ `agent-v*` tags are a lineage **separate from master** — `git tag --contains
<master-sha>` misdates a fix. Test with `merge-base --is-ancestor` on the tag
lineage.

## 3 · The updater's trust chain

The updater runs what it downloads **as the daemon identity — SYSTEM on Windows,
root under systemd**. The manifest's SHA-256 is *not* a tamper anchor: the digest
arrives in the same manifest, from the same origin, as the `browser_download_url`.

| Platform | Anchor |
|---|---|
| Windows | `code_signature::verify_publisher` — `WinVerifyTrust` **plus** an assert that the signer contains `G ROX LTD`. ⚠️ **Both halves are load-bearing**: `WinVerifyTrust` alone proves only that *someone Windows trusts* signed it |
| Linux / macOS | `pgp_verify.rs` — the release signing subkey's ed25519 point is **pinned in the binary**; the `.asc` sidecar is **required** |
| Rollback | `artifact_version::verify_artifact_version` — the MSI's own `ProductVersion` (inside the signed envelope) must equal the tag the manifest claimed. Without it, a tampered manifest advertising a high tag while pointing at a genuinely-signed **older** build passed every check and downgraded the fleet |

⚠️ The version binding is deliberately `Unsupported` (not a refusal) for
`.deb`/`.pkg`: with no signature to anchor it, a version check there compares a
claim against a claim while reading like a control.
⚠️ Still open: the **manifest itself is unsigned** (version+url+hash not attested
as a unit), and the tunnel CLI's separate `self-update` does not share
`download_asset`, so it is not pinned — and it fails **open** when the manifest
carries no digest.

### Signing rules that break quietly

- Every azure-signing job **must** carry `environment: release` — the tenant
  rejects wildcard tag-subject federated credentials.
- MSI **payload** (`roomlerd.exe`, `roomler-shim.exe`) is signed **before**
  `cargo wix` harvests; third-party DLLs (wintun, VC-CRT) are staged **after**
  signing and must keep their original signers (CI asserts).
- **Nothing after payload signing may `cargo build`** — a relink strips
  signatures.
- VERSIONINFO comes from `build.rs` + `embed_resource::compile_for`. **Never**
  winres/winresource — its link-lib leaks a second `RT_VERSION` into the Tauri
  EXEs.
- Rehearse with `signing_mode=local` dispatches, **never** with throwaway
  `agent-v*` tags.
- macOS is all-or-nothing on six `APPLE_*` secrets.

## 4 · Break-glass: the build-host path

For a GitHub outage, or a fix that must not wait for a runner. Build, push to the
build host's registry, then set **both** `newName: registry.roomler.ai/roomler-ai`
**and** `newTag` in the deploy repo (`promote` refuses until `newName` is switched
back to GHCR). Full recipe: [`docs/deployment.md`](../../../docs/deployment.md).

⚠️ **Always `docker system prune -af && docker builder prune -f` after a
build-host deploy.** Every deploy bakes a fresh multi-stage image plus
intermediate layers and build cache; without pruning they pile up until the build
host's root FS fills (2026-07-12: `/` hit 100 % from ~13 GB of stale build images,
mid-deploy). `-a` drops images not backed by a *running* container, so the mongo +
registry containers are untouched; **no `--volumes`**, so mongo data is safe.
⚠️ `registry-retention.sh` (weekly, Sun 04:00) keeps at most 2 tags per repo. Run
it manually if the registry is fat — the blob store is the registry's own storage
and `docker system prune` never touches it.
⚠️ **Never touch `/var/lib/libvirt`** (the running k8s VM disks) or the active
container data volumes. The fattest safe reclaimables are the *other* projects'
Rust `target/` dirs on the build host.

## 5 · CI green ≠ done

Networking and remote-desktop changes are proven on the **real fleet** after every
roll — the workflow only proves the public `/health` kept answering:

```bash
# pods on the new image · online-agent count · an RC session ·
# an overlay pair · a tunnel forward
roomler exec <device> '<cmd>'      # ⚠️ argv is re-split: Windows targets need ONE quoted arg, ";" never "&&"
roomler peers
```

⚠️ An exec sweep is a **biased sample** — it only reaches devices that are
online. Query the server for the denominator.
⚠️ `roomler exec` on a host that restarts its own service answers "no answer
within 45 s" — the command ran.
