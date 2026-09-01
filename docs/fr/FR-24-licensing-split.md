# FR-24 — Licensing split: AGPL control plane, MPL everything the customer runs

**Issue:** [#838](https://github.com/gjovanov/roomler-ai/issues/838) · **Status:** P1–P4a shipped, P3-CLA + P4b open · **Owner:** @gjovanov

## Goal

Replace the single root MIT licence with a per-component split that makes
commercial **re-hosting** of the control plane unattractive, while leaving every
binary a customer installs on their own machines free of copyleft that
procurement will refuse. Keep the source fully auditable — that is the only
trust substitute an unknown vendor has when asking people to run a privileged
daemon.

**Explicit non-goal:** protecting the differentiated code from being copied. No
OSI licence does that, and §"What AGPL actually buys" measures exactly how
little this split protects. It is chosen with that measurement in hand, not in
spite of it.

## Field evidence

Measured against `origin/master`, not assumed from the README.

### The contributor-consent gate is a no-op

```
$ git log --format='%aN <%aE>' | sort | uniq -c | sort -rn
    961 gjovanov <goran.jovanov@gmail.com>
    756 Goran Jovanov <goran.jovanov@gmail.com>
      3 Goran Jovanov <goran.jovanov@gmail.com>
```

One human, three identity strings, two of his own addresses. **Sole copyright
holder ⇒ no consent campaign, no consent log, no rewrite-their-surviving-lines
fallback.** An earlier strategy brief treated this as a blocking gate with its
own document set; all of it is deleted.

The MIT grant on published history stays irrevocable, and `LICENSE-MIT` retains
it verbatim. Anyone may fork the pre-split tree under MIT forever. That is
stated in `LICENSING.md` rather than worked around.

A CLA is still worth having — **prospectively**, before the first external
contribution, because that is the moment dual licensing would otherwise become
impossible again.

### Crate classification, from the dependency graph

| Class | Crates | Rust LOC |
|---|---|---|
| **SERVER** | `api`, `services`, `db`, `config`, `derp-relay`, `tests` | 54 157 |
| **SERVER (web)** | `ui/src` | 53 717 |
| **SHARED** | `tunnel-core` 55 593 · `remote_control` 12 883 · `localapi` 3 012 · `tcp-turn-conn` 518 | **72 006** |
| **CLIENT** | `roomlerd` 89 737 · `roomler-core` · `roomler-cli` · `roomler-setup` · `roomler-setup-core` · `roomler-desktop` · `roomler-cli-shim` | 110 911 |
| **THIRD-PARTY** | `crates/vendored/*` | untouched |

SHARED is measured, not guessed: `crates/api/Cargo.toml` deps
`roomler-ai-remote-control` and `roomler-ai-tunnel-core` unconditionally, with
19 live `tunnel_core::` call sites in `crates/api/src`.

### What AGPL actually buys — the finding that shaped this FR

The originating brief justified the split by saying MIT lets a competitor "lift
the encoder cascade and the mesh carrier logic." **Both land on the permissive
side of its own proposal:**

| Named asset | Lives in | Class | Licence here |
|---|---|---|---|
| Mesh carrier cascade | `crates/tunnel-core/src/overlay/` | SHARED | **MPL-2.0** |
| Encoder cascade | `agents/roomlerd/src/encode/` | CLIENT | **MPL-2.0** |

**AGPL therefore covers ~37% of first-party code and 0% of the code the split
was argued for.** What it covers is routes, DAOs, Mongo models and the Vue app.

That is not a reason to abandon it — it is a reason to state the purpose
honestly. AGPL's only trigger is **network distribution to third parties**, i.e.
hosting, which is the one competitive threat copyleft can block. And its real
commercial value is that **AGPL + a sellable exception is a revenue mechanism**:
the buyer is a procurement department reacting to the letters, not an engineer
reading the dependency graph. The cascades are defended by field-validation
velocity ("CI green ≠ done"), not by a licence file.

### Decision on SHARED: MPL-2.0

Chosen with the trade-off measured, not by default.

- Keeps `roomlerd` free of AGPL. An AGPL agent on a customer endpoint is a
  procurement blocker and is exactly what pushes MSPs toward paid proprietary
  alternatives; that adoption cost outweighs the marginal protection.
- MPL's file-level copyleft still returns improvements to *existing* `overlay/`
  files — the realistic threat (adapting the cascade) more than a clean-room
  rewrite.
- OSI-approved and SPDX-known. Packagers and enterprise legal accept it without
  argument.
- **Accepts:** a competitor may take `tunnel-core` and `roomlerd` into a
  proprietary product, owing back only changes to our files.

Rejected, recorded so they are not re-litigated:

- **AGPL + linking exception on SHARED** — maximum protection, but custom
  licence text (lawyer cost) and non-standard licences spook packagers. Held in
  reserve; the one-file swap in `scripts/licence-classes.sh` makes it cheap.
- **Shrink the shared surface first** — give `tunnel-core` a `server` feature
  the way `remote_control` already has one, so `api` deps only the server half,
  then AGPL that half. Best end state, real refactor; deliberately not a
  precondition.
- **Apache-2.0 on SHARED** — zero reciprocity on any differentiated code.
- **BUSL-1.1 / FSL** — strongest protection, but not OSI-approved, and open
  source *is* the distribution channel. Rejected on strategy, not on legals.

### Third-party obligations surfaced by the audit

Both pre-existing; neither is caused by this FR.

1. ⚠️ **Windows statically links LGPL FFmpeg** (`x64-windows-static-md`,
   `vendor-ffmpeg-windows.yml`), triggering LGPL-2.1 **§6**'s relink
   requirement. Nothing discharged it. Linux/macOS use `--enable-shared` and are
   clean under §6(b). *Contrary to the brief's fear:* neither vendor workflow
   passes `--enable-gpl` or `--enable-nonfree` — both are `--disable-everything`
   plus an explicit encoder allowlist, and CI already fails on an x264/x265 lib.
   **There is no GPL contamination.**
2. ⚠️ **openh264 is compiled from source** (`features = ["source"]`), so Cisco's
   binary patent grant — which attaches to Cisco's own prebuilt module — does
   **not** apply. The code licence (BSD-2-Clause) is fine; AVC pool exposure is
   a business risk, recorded in `THIRD-PARTY-NOTICES.md`.

### Legal entity

Copyright is **G ROX EOOD**, matching the imprint, privacy policy, terms and
landing footer shipped by FR-23. ⚠️ Not `G ROX LTD`: that is the **Windows
code-signing certificate subject only**, a fourth rendering of the same entity
that FR-23 explicitly left in place because changing it needs a new Azure
identity validation (current one valid to 2028-11-21). Using LTD for copyright
would add a fifth inconsistency pointing away from the register.

## Key design

### One place to change the licence

`scripts/licence-classes.sh` holds the SPDX identifiers, the path
classification, the crate lists and the copyright holder. `apply-spdx.sh`,
`licensing.yml` and the manifest audit all source it. Swapping AGPL for
FSL/BUSL later is one line plus a licence text file.

### The check that stops the split rotting

`licensing.yml` runs three checks; the third is the load-bearing one:

1. every workspace member declares an explicit `license` matching its class —
   ⚠️ read from `[package]` only, because `[package.metadata.wix]` also has a
   `license` key meaning something entirely different (it false-positived once
   during implementation);
2. every classified source file carries the right SPDX header
   (`apply-spdx.sh --check`);
3. **no SERVER crate appears in any shipped agent binary's dependency graph.**

(1) and (2) catch a forgotten file. (3) catches the change that would actually
falsify `LICENSING.md`'s MSP paragraph — and nothing else would, because the
violating edge is a one-line `Cargo.toml` change that compiles fine. Uses
`-e normal` so a dev-dependency edge, which cannot reach a shipped binary, is
not a false positive that trains people to disable the check.

### LGPL §6

The FFmpeg builds already publish to a **permanent GitHub Release**
(`vendored-ffmpeg-8.1.2`) with SHA-256 sidecars — but binaries only. An Actions
artifact expires in 90 days and cannot carry a three-year offer, which is why
the release, not the artifact, is the anchor.

`lgpl-source-offer.yml` publishes the **corresponding source** — pristine
upstream tarball archived by us (an upstream tag can move) plus the complete
build recipe — to that same release, and then asserts the asset actually
resolves. ⚠️ A written offer pointing at a missing asset claims compliance we do
not have, so the verification step is part of the job, not a nicety.

## Phases

| P | Scope | Status |
|---|---|---|
| **P1** | Licence files, `LICENSING.md`, `COMMERCIAL.md`, README | ✅ shipped |
| **P2** | `license` on all 17 manifests, `ui/package.json`, 625 SPDX headers, `REUSE.toml` | ✅ shipped |
| **P3** | `CONTRIBUTING.md`, `SECURITY.md`, `security.txt`, `docs/CLA.md` (Apache-ICLA-derived), disabled `cla.yml` | ✅ shipped; ⚠️ CLA still needs legal review before the bot is enabled |
| **P4a** | `THIRD-PARTY-NOTICES.md` + written offer + `lgpl-source-offer.yml` | ✅ shipped; workflow needs its first dispatch (blocked until this merges — `workflow_dispatch` only resolves on the default branch) |
| **P4b** | Close the §6 relink gap | ✅ **resolved by publication** — see below; `docs/lgpl-relink.md` |
| **P5** | *(optional)* `server` feature on `tunnel-core` so AGPL reaches further | not planned |
| **P6** | OCI image labels on the runtime stage + build-args in the deploy recipe | ✅ shipped |

### P4b — the relink gap was smaller than it looked

The first pass assumed §6 obliged us to ship **object files**, and recorded
two structural fixes (shared FFmpeg on Windows, or a per-release object
archive). Re-reading the clause closed it without either.

§6(a) asks for the "work that uses the Library" **"as object code and/or
source code"**. Roomler's agent is MPL-2.0, every crate it links is in this
repository, and the Windows build is a public `cargo build` that finds FFmpeg
through `pkg-config` — so a recipient can modify FFmpeg, rebuild, point
`PKG_CONFIG_PATH` at their tree and relink, using nothing we have not already
published. **Being open source discharges the half we thought we owed.**

`docs/lgpl-relink.md` makes that exercisable rather than merely arguable:
the exact per-platform feature lists, the `.pc` layout the build requires, and
`encoder-smoke --codec hevc` as the check that the relinked binary really uses
the new library. ⚠️ It also states plainly that a relinked binary is unsigned
and our updater will refuse it — a security control, not a §6 restriction.

Shared-FFmpeg-on-Windows is therefore no longer a compliance item. It remains
available as a simplification, but it would **add** to the install (the Linux
minimal shared tree is ~2.5 MB of libraries, against a measured 0.29 MiB true
closure for the ten HW encoders when statically linked), so it is not obviously
a win and needs measuring on its own merits, not as a legal necessity.

## Acceptance criteria

- [x] Every first-party source file carries a correct SPDX header (625 files)
- [x] Every workspace member declares `license` explicitly
- [x] **No SERVER crate reaches any shipped agent binary**, asserted in CI across all five
- [x] The graph assertion is **falsifiable** — verified to trip when a crate that *is* in `roomlerd`'s graph is classified SERVER
- [x] `LICENSING.md` answers the seven practical questions, MSP paragraph included
- [x] `LICENSING.md` states the AGPL coverage limit plainly rather than letting a reader discover it from the dep graph
- [x] Pre-split MIT history retained and its permanence stated
- [x] `SECURITY.md` + `/.well-known/security.txt` published and present in the built bundle
- [x] FFmpeg build flags documented; openh264 grant gap and HEVC exposure recorded
- [x] `cargo check` green on both lanes; `ui` build + 858 unit tests green
- [x] `cargo deny check licenses` passes in CI — three real findings triaged (WTFPL `tun`, MIT-0 `dcv-color-primitives`, and our own AGPL members, fixed with `publish = false` + `private.ignore` rather than allowlisting AGPL)
- [ ] `lgpl-source-offer.yml` dispatched; corresponding-source asset live *(blocked: `workflow_dispatch` resolves only on the default branch, so it runs the moment this merges)*
- [x] CLA rewritten on the Apache ICLA 2.0 skeleton; `cla.yml` present but `if: false`
- [ ] CLA reviewed by a lawyer and the bot enabled *(the only thing still gating P3)*
- [x] OCI `licenses` label set on the runtime stage, landing WITH this FR — plus title/description/url/source/documentation/vendor, and `VERSION`/`GIT_SHA` build-args wired into the deploy recipe
- [x] LGPL §6 relink right is **exercisable and documented** (`docs/lgpl-relink.md`), not merely offered by mail

### Known friction, accepted deliberately

Once this is on master, **any PR that adds a source file fails `Licence split
integrity` until its author runs `scripts/apply-spdx.sh`**. That is intended —
a file shipping with no licence header is a real defect, and the failure names
the one command that fixes it — but it is new friction on every contributor,
so it is recorded rather than discovered.

⚠️ It also means **master itself can go red** when a PR merges that was opened
before this landed. `pull_request` CI checks out the branch MERGED WITH master,
so such a PR is caught before merge; a PR merged in the window between its last
CI run and this landing is not. The fix is the same one command.

## Open decisions

1. **P4b**: shared FFmpeg on Windows vs. per-release object archive. Needs the
   installer-size delta measured before choosing.
2. ~~Whether to pursue an HEVC pool licence~~ — **decided 2026-09-01: HEVC stays
   ON by default.** Only vendor *hardware* encoders ship, so we distribute no
   HEVC implementation and the licence sits with the silicon vendor — the
   position RustDesk and the wider field take. Recorded with its three
   load-bearing conditions in `THIRD-PARTY-NOTICES.md`; ⚠️ adding any SOFTWARE
   HEVC encoder would invalidate it.

## Out of scope

- SOC 2 / ISO 27001 / pentest procurement.
- Any change to the free or self-hosted feature set. Nothing is removed from the
  community edition — that would invert the strategy this FR serves.
- Pricing and entitlements (separate program; see FR-20 for the metering that
  has to land before any tier reshape).
- Repo positioning and metadata — description, topics, OCI labels, social card.
  Verified stale and cheap to fix, but a distinct piece of work.

## Field-verification log

| date | what | result |
|---|---|---|
| 2026-08-27 | Dependency-graph classification of all 17 members | SHARED = 4 crates / 72 006 LOC; `api` deps `tunnel-core` unconditionally |
| 2026-08-27 | `git log` contributor census | 1 human ⇒ consent gate deleted |
| 2026-08-27 | FFmpeg vendor workflows audited for GPL flags | no `--enable-gpl`/`--enable-nonfree`; x264/x265 already CI-blocked |
| 2026-08-27 | FFmpeg link mode per platform | Windows `static-md` ⇒ §6 open; Linux/macOS shared ⇒ clean |
| 2026-08-28 | SPDX sweep applied, then re-run | 625 added; second run 0 added / 625 already correct ⇒ **idempotent** |
| 2026-08-28 | AGPL-in-agent graph check, all 5 shipped binaries | `none` for each |
| 2026-08-28 | Same check with `remote_control` forced into SERVER_CRATES | **tripped** ⇒ assertion is falsifiable, not vacuous |
| 2026-08-28 | `cargo check` agent crates (native) / server crates (WSL) | both clean |
| 2026-08-28 | `ui`: `vue-tsc` + `vite build` + `vitest` | green; 858/858 |
| 2026-08-28 | `dist/.well-known/security.txt` after build | present |
| 2026-08-28 | First CI run of `licensing.yml` | manifest audit ✓ · SPDX check ✓ · **AGPL-in-agent graph check ✓** · `cargo deny` ✗ |
| 2026-08-28 | `cargo deny` failure triaged | two real findings: our own AGPL members were checked as "dependencies", and `tun` 0.8.10 is **WTFPL** (unvetted). The invented `ring` clarify never matched and was removed. |
| 2026-08-29 | Re-read LGPL-2.1 §6(a) against the actual build | "object code **and/or source code**" ⇒ open-sourcing the agent already discharges the half we thought needed an object archive; P4b closed without shipping one |
| 2026-08-29 | `gh workflow run lgpl-source-offer.yml --ref fr24-licensing-split` | **HTTP 404** — `workflow_dispatch` resolves only on the default branch; the first run has to wait for the merge |
<!-- RETIRED-NAME-ANCHOR(2): the row below RECORDS the rename, so it must name both sides. -->
| 2026-08-29 | Merged master (FR-21 renames + FR-25/26/28/29) into the branch | `agents/roomler-agent`→`roomlerd`, `roomler-tunnel`→`roomler-cli`, `roomler-agent-tray`→`roomler-desktop`, `roomler-agent-core`→`roomler-core`; classification and crate lists repointed |
| 2026-08-29 | **The rename exposed a hole in the sweep** | an unclassified path was a silent `continue`, so a renamed directory would have dropped the whole daemon out of the sweep while `--check` still reported OK. Unclassified is now a hard failure; it immediately caught 10 real files (`ui/*.ts`, `ui/index.html`, `ui/public/*`) that `ui/src` never covered ⇒ SERVER_PATHS broadened to `ui` |
| 2026-08-29 | Post-merge re-verification | SPDX 647/647 · manifest audit exit 0 · graph check `none` × 5 · fmt clean · agent crates check · ui build + **898/898** vitest |
