# FR-46 — Eradicate `roomler-agent` as a live name

**Issue**: [#1051](https://github.com/gjovanov/roomler-ai/issues/1051) · **Status**: P0 (spec) · **Predecessor**: [FR-21](FR-21-retire-obsolete-binary-names.md) (#809, DONE)

## Goal

Remove `roomler-agent` / `roomler-agent-tray` / `roomler-tunnel` from every surface a
shipping code path **reads, writes or matches** — including the surfaces FR-21 deliberately
froze. Fleet re-enrollment is authorised (operator, 2026-08-31), which is what makes the
frozen list negotiable.

## This is not FR-21 reopened

FR-21 was scoped **migrate-or-anchor with no fleet disruption**, and it succeeded: 1 849 → 0
unclassified occurrences, 13/13 criteria field-verified, deployed. Its anchors are not
leftovers — each is a site where renaming *was* the defect under that constraint.

FR-46 changes exactly one input: **disruption is now permitted**. Everything else — the
classifier, the guard, the anchor inventory — is FR-21's output, reused.

## Root finding: the anchor set is two populations, not one

FR-21 marked every deliberate occurrence with one marker, because under its constraint they
all had the same consequence (delete it → strand a host). Under FR-46's constraint they split
cleanly, and the split is what makes this program measurable:

| class | what it is | can it ever reach zero? |
|---|---|---|
| **LIVE** | code that resolves, matches or writes the retired name at runtime — appdirs trees, service/task names, log prefixes, env arms, asset pickers | **yes** — this is the program |
| **RECORD** | prose and fixtures that name the retired thing *in order to explain it* — migration notes, field logs quoting real output, FR specs, tests whose input is by definition a historical artifact | **no, and must not** — renaming these falsifies the record |

⚠️ `RECORD` is not an escape hatch for work someone does not want to do. A migration note is
`RECORD`; the code the note describes is `LIVE`. The guard must make the distinction
falsifiable, or the classification becomes a way to launder `LIVE` sites into permanence.

**P1a therefore splits the marker** (`RETIRED-NAME-ANCHOR` stays live, `RETIRED-NAME-RECORD`
is new) and guards them differently: `live` ratchets **down** and must reach 0; `record` is
pinned **exactly**, so a site cannot be quietly reclassified to dodge the live ratchet.

⚠️ **An anchor's own explanation can be swept, and nothing catches it.** The historical-appendix
region in `docs/remote-control.md` read *"rewriting `roomlerd` to `roomlerd` here would falsify
the record"* — it landed correct in `f3cc9240` and was mangled by FR-21's own `--strict` sweep
(`21004d78`), which rewrote the retired name inside the sentence that exists to say the name
must not be rewritten. **The audit is structurally blind to this**: it counts occurrences of
retired names, so a name wrongly REPLACED by the current one simply reads as progress — the
count went down. (Restoring it here moved `anchors` 745 → 746, which is the same signal
arriving far too late to help.) Swept the tree for the shape — exactly one instance, fixed
here. The lesson is the sanitize skill's own: *the tool keeps working and only its
explanation rots, which is the failure mode that lasts, because nothing fails.*

## Root finding: the published-asset freeze is much narrower than FR-21 recorded

FR-21's D6 froze anchors on "already-published asset filenames are immutable and the updater
matches on them". The first half is true; **the second half is true on exactly one path**, and
that path already dual-matches. Measured against master:

| picker | matches on | prefix load-bearing? |
|---|---|---|
| Windows `pick_asset_for_windows` | `.msi` + `-permachine-` infix | **no** |
| macOS `pick_asset_for_unix` | `.pkg` | **no** |
| Linux ≥ 0.4.16 `is_daemon_asset` | `starts_with("roomler-agent")` OR `starts_with("roomlerd")` | **already dual** |
| Linux ≤ 0.4.15 | first `.deb` matching arch | **no** (order-dependent; fixed server-side) |
| server `order_assets_daemon_first` | `starts_with("roomler-desktop-")` | **no** — a companion *denylist*, not a daemon allowlist |
| `scripts/install.sh` | `<arch>-unknown-linux-gnu\.<fmt>` — a SUFFIX | **no** (relies on the same server ordering) |
| wizard `asset_resolver.rs` | nothing — `filename` is a plain `String` | **no**; its anchors are doc comments and test fixtures only |

⇒ **New releases can publish `roomlerd-<v>-…` without freezing any host**, including the
dormant pre-0.4.15 rows. The frozen surface is only the *already-published* files, which stop
being matched the moment nothing in the fleet is old enough to need them.

⚠️ Two things must move in the same PR as the rename or a guard goes stale silently:
`release-agent.yml`'s post-publish ordering check (`case "$first" in ""|roomler-agent-*)`) and
`is_daemon_asset`'s legacy arm, which becomes deletable only when no host below 0.4.16 is
being updated.

⚠️ `artifact_version.rs`'s fixtures (`roomler-agent.deb` / `.pkg` / `.tgz`) are anchored as
"PUBLISHED release asset" names and are **not** — no published asset has that spelling (real
ones carry a version), and the binding reads the MSI's own `ProductVersion`, using the filename
only to dispatch on extension. That anchor's reason is overstated; the fixtures are free.

## Root finding: the env prefix is a FLEET MIGRATION, not a cheap deletion

`ROOMLER_AGENT_*` is the largest single live class (~38 anchors) and the handover filed it
under "cheap classes". It is not. `env::node_env` reads three prefixes — `ROOMLERD_`,
`ROOMLER_NODE_`, `ROOMLER_AGENT_` — and the anchor on that chain claims real hosts still set the
retired spelling in **operator-authored systemd drop-ins that a package upgrade never
rewrites**.

**Field-measured 2026-08-31 rather than trusted**: all three cluster hosts carry exactly four
`ROOMLER_AGENT_VIRTUAL_DESKTOP*` entries in `/etc/systemd/system/roomlerd.service.d/`. The
claim is current, not stale.

⇒ **Ordering is load-bearing.** Delete the arm before rewriting the hosts and the daemon starts
fine and silently ignores four settings per node — no error, no log line, just a headless
cluster node that quietly stops offering a virtual desktop. The migration must be
make-before-break: rewrite every host that sets the retired spelling, confirm, *then* delete
the arm.

⚠️ A host is only discoverable this way if someone goes and looks. There is no inventory of
operator-authored drop-ins, so "every host that sets it" is a claim that has to be re-measured
across the whole fleet — not just the hosts that happen to be in a runbook — before the arm
comes out.
## The three one-way doors

### D1 — macOS bundle identity (the only genuine blocker)

`roomler-agent.app` / `CFBundleExecutable` / `com.roomler.agent` key the **TCC grants** for
Screen Recording and Accessibility. Renaming voids both on every Mac, with **no remote fix**.

⚠️ This is re-**consent**, not re-enrollment: a human must approve at each Mac. The operator's
authorisation covers fleet disruption; it does not conjure a person in front of a MacBook.
Ships **last**, staged one host, with FR-21's criterion re-used (Screen Recording +
Accessibility both granted in the agent log within ~30 s of start).


#### P5 readiness — what the macOS pass actually needs (scoped 2026-08-31)

Re-consent is authorised and the Mac is to hand, so P5 is no longer *blocked*. It is still
**last**, and this records why it must be one focused pass rather than an increment.

**Surface**: ~13 files. Three launchd plists, `packaging/macos/postinstall` (30 occurrences —
the riskiest single file), `updater.rs` (the root-helper update chain, FR-5), `install.sh`,
`installer-smoke.yml` (13), `release-agent.yml` (12), the shim, the desktop companion, and
`ci.yml`'s self-containment gate. Four distinct identities move together or not at all:

| identity | where | breaks if wrong |
|---|---|---|
| `.app` bundle name | plists, postinstall, workflows | **TCC grants void** — no capture, no input |
| `CFBundleExecutable` | the bundle + `ci.yml` assert | the daemon does not launch |
| `/Library/Roomler/<bundle>` + its symlink | postinstall | `.pkg` installs to a path nothing runs |
| `/etc/roomler-agent/config.toml` | `com.roomler.daemon.plist` `--config` | **the daemon loses its enrolment** |

⚠️ The config path is the one sweep 3 found: on macOS the "legacy" appdirs path is the LIVE
one, passed explicitly as `--config`, so it does **not** move by itself when appdirs changes.
It cannot be migrated on-device either — the plist argument is explicit, so copying the config
to `/etc/roomler` changes nothing until a release ships a new plist.

**Why it cannot be iterated cheaply**: there is no macOS host in the build loop, macOS artifacts
are produced **only at tag time**, and each attempt therefore costs a tag plus a full build plus
an install. Two failure modes are already documented and both are silent-at-review:
a `postinstall` whose shebang was pushed off line 1 broke the `.pkg` at *"Validating packages"*,
and a bundle built on a runner with Homebrew baked `/opt/homebrew` paths into its dylibs and
**dyld killed the agent at launch on every end-user Mac**.

**Order for the pass**, once P2b and P4 are done:

1. Move all four identities in one commit; `ci.yml`'s self-containment and `CFBundleExecutable`
   asserts are the cheap gate that must fail first if the set is inconsistent.
2. Carry the config path as a **dual read** for exactly one release — old plist arg and new both
   resolving — so a host that takes the update out of order is not stranded.
3. Tag, build, install on the one Mac, re-approve Screen Recording + Accessibility by hand.
4. Acceptance is FR-21's: both grants present in the agent log within ~30 s of start, plus a
   remote-control session that actually paints and accepts input.
5. Only then the second Mac.

### D2 — the updater chicken-and-egg

A frozen agent cannot receive its own fix. Precedent: publishing a second Linux `.deb` froze
every pre-0.4.16 agent, and the fix had to be **server-side** for exactly that reason. Rule for
every phase: *if this is wrong, can the fleet still take the next update?* If no, the
server-side escape hatch ships first.

### D3 — history is not reachable

Roughly three thousand commit messages and blobs carry the name. A `filter-repo` mirror
rewrite is possible but **every SHA changes** and old commits stay reachable through
`refs/pull/*` indefinitely. Out of scope — see the acceptance framing.

## Phases

| # | phase | kill switch | status |
|---|---|---|---|
| P0 | spec + issue + taxonomy decision | — | **this doc** |
| P1a | split the marker into ANCHOR (live) / `RETIRED-NAME-RECORD` (history); `records` pinned exactly so nothing can be laundered | revert the audit script | **shipped** |
| P1b | publish daemon assets as `roomlerd-*`; guard rewritten as a companion denylist | revert the workflow; published assets are immutable and additive | **shipped — needs a release to field-prove** |
| P2a | env prefix: rewrite every host that sets the retired spelling (make-before-break) | both spellings kept; `.bak` / additive reg key | **4 hosts done** (3 Linux + 1 Windows) |
| P2b | env prefix: delete the legacy arm | restore the arm | **blocked on 4 OFFLINE devices only** — every online device is now measured and migrated |
| P2c | remaining cheap classes: log filenames, install/staging paths, e2e image, wizard PATH | per-item revert | **`TermsView` done**; the rest open |
| P3 | re-enrollment classes: appdirs trees, Windows service + task, install folder, systemd `ReadWritePaths` | staged rollout + rollback build | **unblocked; Linux measured CLEAN, macOS is NOT** (below) |
| P4 | wire values (QUIC ALPN, WebRTC stream id) — dual-accept window, then cleanup | dual-accept stays until cleanup | |
| P5 | macOS bundle (D1) — one host, verified, then fleet | do not proceed past host 1 | |
| P6 | retire the machinery: `anchors=0`, guard flips to record-only | — | |


⚠️ **Check each anchor's stated reason before paying its price.** Two of them have now been
found false on inspection: `artifact_version.rs` calls its fixtures "PUBLISHED release asset"
names that no release ever carried, and `TermsView` called a section heading a "defined term in
the Terms" when the Terms define the word in the body and reference the heading nowhere. Both
were free. An anchor is a *claim*, and FR-21 wrote them under time pressure across ~1 849 sites.

## Acceptance criteria

- [ ] `name-audit.sh` reports **`anchors = 0`** — the counter is the LIVE class — and CI blocks on it
- [ ] no runtime path, service name, scheduled task, env var, log filename, wire value or
      bundle identity read or written by shipping code contains a retired name
- [ ] every remaining occurrence is `RECORD`-classified with a stated reason, and the record
      count is pinned exactly (a new one is a deliberate, reviewed act)
- [ ] a pre-rename host upgrades to the FR-46 build and keeps its enrolled identity **or** is
      re-enrolled by a documented, proven procedure — decided per class, recorded here
- [ ] `roomlerd-*` assets are published and every picker in the fleet finds them (measured, not
      reasoned: one host per OS takes an update across the rename)
- [ ] macOS: Screen Recording + Accessibility granted in the agent log within ~30 s of start on
      the staged host, before any second Mac is touched

## Open decisions

| # | decision | owner | state |
|---|---|---|---|
| 1 | The dormant pre-0.4.15 rows: re-enroll or accept losing them? | operator | **ANSWERED 2026-08-31 — either is acceptable.** So the appdirs / service / task anchors (P3) are no longer gated on preserving them |
| 2 | macOS re-consent — a human at each Mac | operator | **ANSWERED 2026-08-31 — authorised.** P5 is unblocked, but stays LAST and staged one host: the grants cannot be re-approved remotely, so a wrong rename is a fleet-wide capture/input outage until someone visits |
| 3 | Does "complete" include git history (D3)? | operator | proposed: **no** |
| 4 | Is `roomlerd-*` the final published asset spelling? | — | proposed: **yes** |

## Out of scope

- Git history and already-published GitHub release files (D3) — unreachable, and framing the
  goal as behaviour rather than `git grep` is what keeps this FR honest.
- Forks and local clones.
- The `roomler-desktop` / `roomlerd` / `roomler` names themselves — those are the destination.

## Field-verification log

| date | build | what was proven |
|---|---|---|
| 2026-08-31 | fleet, live | **Sweep 1 (systemd):** `ROOMLER_AGENT_VIRTUAL_DESKTOP*` is STILL SET on all three cluster hosts (4 entries each, operator-authored drop-in) — so the arm is load-bearing today and the handover's "cheap class" framing was wrong. Migrated all three make-before-break: both spellings, identical values, `.bak` kept, `systemctl show` resolves 8 of which 4 are `ROOMLERD_`, daemons untouched and still `active` |
| 2026-08-31 | fleet, live | **Sweep 2 (whole fleet, via Fleet RPC):** probed all 12 online devices through `roomler exec`, whose child inherits the daemon's own environment — so this reads what the daemon ACTUALLY has, not what a config file claims. Found a **second, independent setter the systemd-only theory would have missed**: a Windows host carries `ROOMLER_AGENT_VP9_FPS=60` **machine-wide in HKLM**, not in any unit file. Migrated additively (`ROOMLERD_VP9_FPS=60` added, legacy kept). ⚠️ **7 devices remain unverifiable** — 3 online with `exec_enabled` false (gate 4, which is exactly the gate a server cannot overrule) and 4 offline. So P2b stays blocked on evidence, not on effort |
| 2026-08-31 | fleet, live | **Sweep 3 (appdirs trees, gates P3):** on the reachable Unix devices the legacy tree is **already gone on Linux** — all three carry `/etc/roomler` + `/root/.config/roomler` and neither legacy path — so the appdirs dual-read costs Linux nothing to remove. **macOS is the opposite**: it has `/etc/roomler-agent` and **no** `/etc/roomler`, i.e. on that platform the "legacy" path is the *live* one the `com.roomler.daemon.plist` passes as `--config`. So removing the appdirs fallback is NOT one change — it is free on Linux and a coordinated plist + config move on macOS, which belongs next to P5, not before it. ⚠️ The Windows per-user tree is **still unmeasured**: `roomler exec` runs as SYSTEM, whose `%APPDATA%` is the service profile, while enrollment writes the *enrolling user's* profile |
| 2026-08-31 | fleet, live | **Sweep 4 (the one that found the method was wrong).** Re-swept every online device after two more got `exec_enabled`. Three corrections to sweep 2, all of them mine: (1) a `tail -1` had hidden a SECOND var on the Windows host — `ROOMLER_AGENT_LOCAL_TURN=1` beside `VP9_FPS`; (2) the probe read the PROCESS environment, and **Windows drops empty variables**, so three more legacy entries sat in the registry invisible to it (`VIEWER_RATE_RECOVER`, `OVERLAY_VPN_BYPASS`, `OVERLAY_UPLINK_IF`) — the registry is the surface a FUTURE start reads, and it is the one that matters; (3) the dev box and its WSL sibling report `exec_enabled` false and did not need it — they are **this** machine, inspectable directly, and the box carried a third setter (`ROOMLER_AGENT_GPU_CLOCK_PIN`). Actions: `LOCAL_TURN` mirrored; the four inert entries DELETED (empty parses identically to absent for all of them, and `VIEWER_RATE_RECOVER` has **no reader in the tree at all**); the dead legacy install directory removed from the machine PATH (it did not exist on disk). Every other Windows host is clean at registry level, as are the Asahi and macOS hosts. ⇒ **only the 4 OFFLINE devices are now unmeasured** |
| 2026-08-31 | code read | **The instrumentation to answer P2b already exists and is write-only.** `env::note_legacy_use` emits a WARN — *"value read through a RETIRED variable name"* — the first time any legacy prefix is read. It has **no counter, no LocalAPI field and no consumer**, and it fires once near startup, so `roomler logs --grep` (a ≤64 KiB TAIL) cannot answer for a long-running daemon. Surfacing it as a counter in `peers --json` would turn P2b from a manual sweep into a fleet-wide read, the same shape the overlay-ACL rollout used with `rx_denied` |
