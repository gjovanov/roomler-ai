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
| P1b | publish daemon assets as `roomlerd-*`; move the workflow guard | revert the workflow; assets are additive | |
| P2a | env prefix: rewrite every host that sets the retired spelling (make-before-break) | both spellings kept; `.bak` per host | **3 cluster hosts done** |
| P2b | env prefix: delete the legacy arm — ONLY after a fleet-wide re-measure finds no setter | restore the arm | blocked on that sweep |
| P2c | remaining cheap classes: log filenames, install/staging paths, e2e image, `TermsView`, wizard PATH | per-item revert | |
| P3 | re-enrollment classes: appdirs trees, Windows service + task, install folder, systemd `ReadWritePaths` | staged rollout + rollback build | |
| P4 | wire values (QUIC ALPN, WebRTC stream id) — dual-accept window, then cleanup | dual-accept stays until cleanup | |
| P5 | macOS bundle (D1) — one host, verified, then fleet | do not proceed past host 1 | |
| P6 | retire the machinery: `anchors=0`, guard flips to record-only | — | |

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
| 1 | The dormant pre-0.4.15 rows: re-enroll or accept losing them? | operator | **open** |
| 2 | macOS re-consent — who is physically at each Mac, and when? | operator | **open** (gates P5) |
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
| 2026-08-31 | fleet, live | `ROOMLER_AGENT_VIRTUAL_DESKTOP*` is STILL SET on all three cluster hosts (4 entries each, operator-authored drop-in) — so the arm is load-bearing today, and the handover's "cheap class" framing was wrong. Migrated all three make-before-break: both spellings, identical values, `.bak` kept; `systemctl show` resolves 8 entries of which 4 are `ROOMLERD_`; daemons untouched and still `active` |
