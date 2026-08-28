# FR-21 — Retire the obsolete `roomler-agent` / `roomler-agent-tray` names

| | |
|---|---|
| **Issue** | [#809](https://github.com/gjovanov/roomler-ai/issues/809) |
| **Status** | design |
| **Opened** | 2026-08-28 |
| **Baseline** | master `fa364b12` (0.4.11) — every count and anchor below was measured against it |
| **Scope** | Windows · Linux · macOS — code, comments, docs, `CLAUDE.md`, file names, folder names, env vars |

---

## 1. Goal

**A reader — human or a fresh Claude session — must never meet a name the product no longer
uses.** Every occurrence of a retired name is, after this FR, in exactly one of two states:

1. **migrated** to the current name, or
2. **a compatibility anchor**: deliberately frozen, carrying a comment that says *why*, and
   listed in a machine-checked freeze list.

There is no third state. "Occurrence nobody has looked at yet" is the state this FR exists to
eliminate, and the CI guard in P0 is what stops it coming back.

The current names are the ones the product actually ships:

| retired | current | what it is |
|---|---|---|
| `roomler-agent` | **`roomlerd`** | the daemon on a controlled host |
| `roomler-agent-tray` | **`roomler-desktop`** | the desktop/tray companion |
| `roomler-tunnel` | **`roomler`** | the CLI |
| `roomler-installer`, `roomler-tunnel-installer` | **`roomler-setup`** | the unified wizard (legacy wizards deleted in P4c-2) |

---

## 2. Why this is an FR and not a `sed -i`

**The hard part is not renaming. It is knowing what must *not* be renamed.**

Four earlier slices — P3d A/B/C and P3e — already moved the *runtime identity* surfaces and
deliberately left **dual-read fallbacks** so hosts enrolled under the old names keep working.
Those fallbacks read as "obsolete name in the code" to any sweep, and a naive sweep deletes
them. Deleting one does not fail a build and does not fail CI; it strands whatever fraction of
the field is still on the old segment, silently, at the next update:

| anchor | file:line | what breaks if a sweep "fixes" it |
|---|---|---|
| `OLD_APP = "roomler-agent"` | `crates/agent-core/src/appdirs.rs:37` | a pre-rename host's enrolled `config.toml` is orphaned — the device drops off the mesh and re-enrolls as a stranger |
| `LEGACY_SERVICE_NAME = "RoomlerAgentService"` | `agents/roomler-agent/src/win_service/mod.rs:61` | the takeover install cannot find the running service to retire; two services, or none |
| `LEGACY_TASK_NAME = "RoomlerAgent"` | `agents/roomler-agent/src/service.rs:151` | the pre-rename scheduled task survives forever → two daemons |
| legacy log prefix `roomler-agent.log` | `crates/agent-core/src/logging.rs:245,312,440`; `agents/roomler-agent/src/logs_fetch.rs:54` | `rc:logs-fetch` returns "no log file" on an upgraded host that has not rolled yet |
| `LEGACY_INSTALL_FOLDER_NAME = "roomler-agent"` | `agents/roomler-agent/src/updater.rs:367` | the vacated install dir is never swept |
| `%h/.config/roomler-agent` in `ReadWritePaths` | `agents/roomler-agent/packaging/linux/roomler.service:39` | systemd denies the daemon its own legacy tree mid-migration |
| **the macOS `.app` bundle** — see §5, D5 | `packaging/macos/com.roomler.{agent,daemon}.plist`; `.github/workflows/ci.yml:391` | **TCC grants are voided** — every Mac silently loses Screen Recording + Accessibility until a human re-grants them |

So the deliverable is not a diff. It is **a classifier plus a guard**: an inventory that assigns
every occurrence to `migrate` / `freeze`, a CI job that fails on an unclassified occurrence, and
then the migration itself — done in that order, because without the guard the migration is
unverifiable and regresses on the next PR.

---

## 3. Measured inventory (master `fa364b12`)

```
roomler-agent               874 hits / 154 files
roomler_agent               201 hits /  40 files
roomler-agent-tray           21 hits /  15 files
roomler-tunnel              269 hits /  70 files
roomler_tunnel               22 hits /  14 files
ROOMLER_AGENT_*             407 hits /  69 files   (107 DISTINCT variables)
roomler-installer /
  roomler-tunnel-installer   14 hits /   5 files
                          ─────────────────────────
union                      1765 hits / 233 files
```

Concentration, for `roomler-agent` alone:

```
61  agents/roomler-agent       13  docs                6  .github/workflows
11  crates/agent-core           8  scripts             5  crates/tunnel-core
 7  ui/src                      6  agents/roomler-tunnel
 4  agents/roomler-agent-tray   3  crates/tests        3  docs/fr
 3  crates/roomler-setup-core   3  agents/roomler-setup
```

`docs/` carries 121 hits across 17 files. `CLAUDE.md` carries the retired name on 17 lines —
including the **Commands** block a new session copy-pastes (`CLAUDE.md:85-92`):

```bash
cargo build -p roomler-agent --release --features full
./target/release/roomler-agent enroll --server <url> --token <jwt> --name <label>
./target/release/roomler-agent run
```

That block was **wrong** (fixed in P1): the emitted binary is `roomlerd`
(`agents/roomler-agent/Cargo.toml:18`), so the documented `run` command does not exist at the
documented path. This is the cheapest possible demonstration that the residue costs something.

### Still-unmigrated build-graph identity

| where | current value | file:line |
|---|---|---|
| package | `roomler-agent` | `agents/roomler-agent/Cargo.toml:2` |
| lib | `roomler_agent` | `agents/roomler-agent/Cargo.toml:8` |
| bin (already correct) | `roomlerd` | `agents/roomler-agent/Cargo.toml:18` |
| package | `roomler-agent-tray` | `agents/roomler-agent-tray/Cargo.toml:2` |
| bin (already correct) | `roomler-desktop` | `agents/roomler-agent-tray/Cargo.toml:11` |
| package | `roomler-agent-core` | `crates/agent-core/Cargo.toml:2` |
| lib | `roomler_agent_core` | `crates/agent-core/Cargo.toml:8` |
| dependency edge | `roomler-agent = { path = "../../agents/roomler-agent" }` | `crates/tests/Cargo.toml:17` |
| **directories** | `agents/roomler-agent/`, `agents/roomler-agent-tray/` | — |

The `[[bin]]` comments state the intent plainly, and are the reason this half was left:

> *P3d Slice B: the OUTPUT binary is `roomlerd` (daemon). The package (`roomler-agent`) + lib
> (`roomler_agent`) names are UNCHANGED — only the emitted exe filename moves.*

That was the right call **then** — it kept the slice revertible. It is now the largest single
source of residue: 154 files reference a package name whose product no longer exists.

---

## 4. Classification — the three classes

Every occurrence gets exactly one label. This table *is* the spec for the P0 classifier.

### Class A — **migrate** (no runtime effect; the bulk of the work)

Prose, comments, doc files, `CLAUDE.md`, workflow `-p <pkg>` selectors, cargo package/lib names,
directory names, test names. *≈ 1 400 of the 1 765 hits.*

### Class B — **migrate with a dual-read fallback** (live surface; the old value must keep working)

Anything a deployed host — or a deployed *operator runbook* — already depends on:

- the **107 `ROOMLER_AGENT_*` variables**. An operator-facing surface: only **4** are documented
  in `crates/agent-core/src/config_surface.rs`, so the other 103 exist solely in code and in
  people's shell history.
- residual on-disk paths not yet behind `appdirs` (§5).
- the HTTP `user_agent("roomler-agent/…")` sent by the updater
  (`agents/roomler-agent/src/updater.rs:588,671,873`). It is **recorded** into device/session
  telemetry at `crates/api/src/ws/handler.rs:69` — confirm nothing *dispatches* on it before
  changing it, because changing it also changes what historical rows mean.

### Class C — **freeze** (renaming *is* the defect)

The seven anchors in §2, plus already-published GitHub release asset names (immutable), plus the
legacy `roomler-agent.service` unit name referenced by the duplicate-unit cleanup
(`packaging/linux/roomlerd.service:61`).

Every Class-C site gets a one-line marker so the classifier finds it mechanically rather than
through a hand-maintained path list that drifts:

```rust
// RETIRED-NAME-ANCHOR: pre-rename hosts still resolve this tree; see docs/fr/FR-21.
const OLD_APP: &str = "roomler-agent";
```

---

## 5. Field evidence — a live defect the residue is already causing

**The RC console's "open staging folder" affordance points at a path that does not exist on a
fresh install.**

`ui/src/views/remote/RemoteControl.vue:1673` hardcodes:

```js
const STAGING_PATH = 'C:\\ProgramData\\roomler\\roomler-agent\\staging'
```

But `appdirs::machine_global_dir()` (`crates/agent-core/src/appdirs.rs:73-92`) resolves
`%PROGRAMDATA%\roomler\<segment>`, where `<segment>` is `NEW_APP = "roomler"` unless a pre-rename
tree is present:

```rust
let new = base.join(NEW_APP);   // C:\ProgramData\roomler\roomler
let old = base.join(OLD_APP);   // C:\ProgramData\roomler\roomler-agent
if !new.exists() && old.exists() { old } else { new }
```

So the hardcoded path is correct **only** on a perMachine host that still carries the legacy
tree, and wrong on every fresh perMachine install — where the real directory is
`C:\ProgramData\roomler\roomler\staging`.

And unlike the per-user trees, `migrate_legacy_trees` (`appdirs.rs:133-175`) walks the three
`ProjectDirs` paths **only** — it never touches the machine-global tree — so the split is
permanent, not transitional.

⚠️ **To confirm in the field before P4 lands** (see §12). The prediction is falsifiable and is
recorded as such.

This is the argument for the FR in one example: the residue is not cosmetic. A hardcoded copy of
a name that a resolver has since made *conditional* is a defect waiting for the resolver to take
the other branch.

---

## 6. Key design

**One classifier, one guard, then the migration in risk order.**

```
scripts/name-audit.sh          # classifier + CI guard: one script, two modes
  --report   → every occurrence, labelled A/B/C, grouped by file
  --check    → exit 1 on any occurrence that is neither migrated nor a marked anchor
```

`--check` is what CI runs. It is deliberately **not** a "count must not increase" ratchet: a
ratchet happily passes a PR that deletes one anchor and adds one comment. The guard asserts the
stronger property — *zero unclassified occurrences* — which is only reachable after P1–P4, so
the job ships **advisory (warn-only)** in P0 and flips to **blocking** at the end of P5. That
flip is itself an acceptance criterion, because an advisory guard nobody flips is a guard that
does nothing.

Migration order is by blast radius, smallest first, each phase independently revertible:

```
P0  classifier + advisory guard        → no product change at all
P1  prose (docs, CLAUDE.md, comments)  → no runtime change; revert = git revert
P2  build graph (packages, libs, dirs) → no ARTIFACT change (bins are already correct)
P3  env vars, dual-read                → old names keep working forever
P4  residual live paths + the §5 defect → resolver with fallback, never a hardcoded constant
P5  ratify the freeze list, flip the guard to blocking
P6  field verification on all three OSes
```

**P2's safety property is checkable, and must be checked.** Renaming a cargo *package* changes
no emitted artifact, because every `[[bin]]` name is already correct. The P2 PR therefore has to
show a byte-identical artifact-name listing before and after. If any release asset name moves,
P2 is wrong and stops.

---

## 7. Phases

| # | Phase | Deliverable | Kill switch |
|---|---|---|---|
| **P0** | Classifier + advisory guard | `scripts/name-audit.sh`; CI job `name-audit` in `ci.yml` with `continue-on-error: true`; `RETIRED-NAME-ANCHOR:` markers on every Class-C site | the job is advisory — it cannot fail a PR |
| **P1** | Everything FALSE today | the two clap program names; the `CLAUDE.md` Commands block; the copy-paste install steps in `docs/tunnel-install.md`; script/manifest comments | `git revert` — no runtime surface touched |
| **P1b** | The bulk prose sweep | the ~1 700 remaining prose hits, *after* P2 has moved what they describe; historical appendices keep the retired name behind a block anchor | `git revert` |
| **P2a** | Build-graph identity, agent side | packages `roomler-agent`→`roomlerd`, `roomler-agent-tray`→`roomler-desktop`, `roomler-agent-core`→`roomler-core`; libs `roomler_agent`→`roomlerd`, `roomler_agent_core`→`roomler_core`; dirs `agents/roomler-agent`→`agents/roomlerd`, `agents/roomler-agent-tray`→`agents/roomler-desktop`; every `-p` selector in the workflows + `Dockerfile.agent-e2e` + `scripts/` | one atomic PR, `git revert`; gated on an **empty artifact-name diff** |
| **P2b** | Build-graph identity, CLI side | package `roomler-tunnel`→`roomler-cli`, lib `roomler_tunnel`→`roomler_cli`, dir `agents/roomler-tunnel`→`agents/roomler-cli`; `release-tunnel.yml` | same; separate PR so `release-tunnel.yml` moves in a reviewable diff |
| **P3** | Env-var namespace | 107 vars `ROOMLER_AGENT_<REST>` → **`ROOMLERD_<REST>`** (D1), **dual-read**: new spelling wins, old spelling honoured and logged once at WARN | the old spelling never stops working; the WARN is the only new behaviour |
| **P4** | Residual live paths | fix the §5 staging defect by calling a resolver instead of a constant (expose `machine_global_dir()` over LocalAPI); sweep the remaining hardcoded `roomler-agent` paths | every path keeps a new-then-old fallback, exactly like `appdirs` |
| **P5** | Ratify + enforce | freeze list reviewed and justified in-tree; `name-audit --check` flips to **blocking**; a deliberately-red run proves it fails | revert the `continue-on-error` removal |
| **P6** | Field verification | the §9.4 matrix on Windows / Linux / macOS, fresh **and** upgrade-from-0.4.x, including a pre-rename host | n/a — verification, not a change |

### 7a. Resequencing, measured during P1 — P1 was specced too large

The original plan put a ~1 400-hit "prose sweep" before the build-graph rename. **That is not
possible, and the measurement says so plainly.** Classifying the prose by whether it is *false
today*:

```
command forms (`roomler-agent run`, `roomler-tunnel enroll`)   26   <- FALSE today
path refs (`agents/roomler-agent/...`)                         55   <- TRUE today
cargo -p refs (`cargo build -p roomler-agent`)                 12   <- TRUE today
```

Almost all prose correctly describes the *current* build graph: the package really is called
`roomler-agent`, and the directory really is `agents/roomler-agent/`. Rewriting it before P2
would replace accurate documentation with a description of a state that does not exist yet.

So P1 shrank to *everything that is false today*, and the bulk sweep moved to **P1b, after P2**
— where it is no longer judgement work but a mechanical follow-on from the rename it describes.
The dead end is worth recording: "sweep the docs first, it's the safe phase" is the intuitive
order and it is wrong, because in a rename the docs are only wrong *after* the code moves.

**What P1 actually found and fixed** — the most valuable single line in the FR so far:

```rust
-#[command(name = "roomler-agent", version, about, long_about = None)]   // agents/roomler-agent/src/main.rs:39
+#[command(name = "roomlerd",      version, about, long_about = None)]
-#[command(name = "roomler-tunnel", ...)]                                // agents/roomler-tunnel/src/cli.rs:60
+#[command(name = "roomler",        ...)]
```

Both binaries **misnamed themselves in every `--help`, usage line and argument error**. Nothing
tested it, because clap's program name is display-only. Verified by running the built binary,
not by reading the diff: `Usage: roomlerd.exe` where it previously said `roomler-agent.exe`.

### 7b. Two hazards found while executing, both blocking their phase

1. **P2a would silently rename the Debian package.** `[package.metadata.deb]` in
   `agents/roomler-agent/Cargo.toml:550` sets no `name`, so cargo-deb derives `Package:` from the
   crate name — and `docs/installation.md:240` documents `apt remove roomler-agent`, confirming
   it. Renaming the crate therefore makes dpkg see a *different* package: the old one stays
   installed beside the new one, or the upgrade simply does not happen. **P2a must either pin
   `[package.metadata.deb] name = "roomler-agent"` as an anchor, or ship
   `Provides`/`Conflicts`/`Replaces` so dpkg performs a real takeover** — and whichever is
   chosen has to be proven by an actual `apt upgrade` on a host installed from the previous
   release, not by inspection.
2. **Historical appendices must keep the retired name.** `docs/remote-control.md` §17–19 are
   explicitly historical (`## 19. Appendix — resilience cycle (0.1.50 → 0.1.54, historical)`),
   and passages like *"anyone who tried `roomler-agent service install` after 0.1.50–0.1.52 saw
   the error"* are records of what people actually typed. Rewriting them falsifies the record.
   P1b marks those regions with a block anchor instead — which needs a
   `RETIRED-NAME-ANCHOR-BEGIN`/`-END` pair, since a line-counted span is too brittle for prose.

---

## 8. Acceptance criteria

- [ ] `scripts/name-audit.sh --check` exits 0 on master, and its report shows **0 unclassified
      occurrences** (down from 1 765).
- [ ] Every remaining occurrence is a `RETIRED-NAME-ANCHOR:` site whose comment states why it is
      frozen; the anchor count is asserted by a unit test, so silently deleting one fails a test
      rather than a field host.
- [ ] CI **fails** a PR that adds a new unclassified occurrence — proven by a deliberate red run
      linked from the issue, not by assertion.
- [ ] `CLAUDE.md`'s Commands block names binaries that exist (`roomlerd`, `roomler`,
      `roomler-desktop`); copy-pasting any line works on a real host.
- [ ] **P2 changed no artifact name**: the asset list for a dispatch build is byte-identical to
      the previous tag's, modulo version.
- [ ] All 107 `ROOMLER_AGENT_*` variables still work after P3 on a host that sets only the old
      spelling, and the daemon logs the deprecation exactly once per variable per start.
- [ ] The §5 staging path resolves correctly on **both** a fresh perMachine host and a pre-rename
      perMachine host — the SPA and `machine_global_dir()` now agree by construction.
- [ ] `cargo test -p roomler-ai-tests` green (floor: the current ~294 across 34 modules, 2 skips).
- [ ] `cd ui && bun run test:unit` green, including the `enrollCommands` retired-name lock.
- [ ] `cd ui && bun run e2e` — no new failures against `scripts/e2e-expected-failures.txt`.
- [ ] **macOS TCC grants survive the upgrade**: Screen Recording and Accessibility are *not*
      re-prompted, and remote control works with no human action.
- [ ] A pre-rename host (legacy `%PROGRAMDATA%\roomler\roomler-agent` tree present) keeps its
      enrolled identity — same `agent_id`, same overlay address — across the upgrade.
- [ ] Field: post-roll `roomler exec` sweep across the fleet on all three OSes; every host reports
      the expected binary, service/unit/launchd label, config path and log path.

---

## 9. Test plan

### 9.1 Unit

- **Classifier**: table-driven over a fixture tree — an unmarked Class-C anchor must be reported;
  a marked one must not.
- **Anchor count**: asserted, so deleting an anchor fails a test.
- **Env dual-read (P3)**: old spelling alone → honoured; both set → new wins; neither → default.
- **Path resolver (P4)**: new-tree-present / old-tree-present / both / neither.

### 9.2 Integration (`crates/tests`)

- The suite drives the agent library **in-process**, so the P2 package rename is exercised by the
  whole `agent` + `remote_control` module set (`rc:*` round-trip) rather than by a compile check.
- **Add**: a `logs-fetch` case against a fixture dir containing *only* legacy
  `roomler-agent.log.*` files — the Class-C log anchor, locked by a test instead of a comment.

### 9.3 E2E (`ui/e2e`)

- The 24 existing specs stay at parity with `scripts/e2e-expected-failures.txt`.
- The RC console spec gains a check that the staging affordance resolves to the path the **agent
  reports**, not to a constant compiled into the SPA (§5).

### 9.4 Field matrix — the acceptance surface

| OS | install | what is verified |
|---|---|---|
| Windows | fresh perMachine MSI | service `Roomler`; `roomlerd.exe` + `roomler.exe` + `roomler-desktop.exe` present; config under `%PROGRAMDATA%\roomler\roomler`; staging affordance opens the real dir |
| Windows | fresh perUser MSI | scheduled task `Roomler`; `%APPDATA%` tree on the new segment |
| Windows | **upgrade from 0.4.x, pre-rename tree** | identity preserved; legacy service/task retired, not duplicated; legacy log files still fetchable |
| Linux | fresh `.deb` | `roomlerd.service` active; `/etc/roomler/config.toml`; `roomler peers` OK |
| Linux | **upgrade from 0.4.x** | no duplicate unit (the `SubState=auto-restart` storm class); enrolled identity preserved |
| macOS | fresh `.pkg` | LaunchAgent + opt-in LaunchDaemon load; capture and input work |
| macOS | **upgrade from 0.4.x** | **TCC grants survive** — the single highest-risk assertion in this FR |

Field verification runs through `roomler exec` after the roll, per the standing rule:
**CI green ≠ done.**

---

## 10. Decisions

**D1 — target namespace for the 107 env vars: `ROOMLERD_*` or `ROOMLER_*`?**
**RESOLVED 2026-08-28 (operator): `ROOMLERD_*`.** The precedent exists already
(`ROOMLERD_CONFIG`, used by `packaging/linux/roomlerd.service:48`), and `ROOMLER_*` sits one
underscore from the **server's** `ROOMLER__` double-underscore config prefix —
`ROOMLER_OVERLAY_DEMOTE` beside `ROOMLER__OVERLAY__…` is a confusion this FR would be
*creating*, not removing.

The full mapping is therefore mechanical: `ROOMLER_AGENT_<REST>` → `ROOMLERD_<REST>`, for all
107, with the old spelling honoured forever (D2).

**D2 — do the 107 vars move at all?** They are the largest Class-B block and the lowest
reader-value: almost none appear in docs. **RESOLVED: yes, but last (P3), dual-read, and the old
spelling is never removed** — the cost of keeping it is one match arm, and the alternative is
breaking shell history and runbooks nobody has an inventory of.

**D3 — what does `roomler-agent-core` become?** `roomlerd-core` reads as daemon-only, but the
crate exists **because** the desktop companion deps it *without* the daemon (P3e lever E).
**RESOLVED: `roomler-core`** (lib `roomler_core`) — it is the node's shared core, not the
daemon's.

**D4 — rename the directories, or only the package names?** Renaming `agents/roomler-agent/`
costs `git log` legibility for the naive reader. **RESOLVED: rename, in a commit that changes
*only* paths** so `git log --follow` and `git blame` track cleanly — never mixed with content
edits.

**D5 — the macOS bundle.** Freeze, or migrate with a paired TCC re-grant campaign? Freezing
leaves `roomler-agent` visible at `/Library/Roomler/roomler-agent.app` and in both launchd labels
forever. That is ugly, and it is still right: the alternative asks every Mac user to re-grant
Screen Recording and Accessibility, and a missed re-grant is a device that looks enrolled and
cannot be controlled. **RESOLVED: freeze** — revisit only with an installer that can re-register
the bundle without a human. The lock already exists on both sides —
`ui/src/__tests__/utils/enrollCommands.spec.ts:129-147` and `.github/workflows/ci.yml:391`.

**D6 — `roomler-tunnel`.** 269 hits, and the enroll-command test already treats it as retired
(`enrollCommands.spec.ts:143`). **RESOLVED: fold in** — same sweep, same guard — but as its own
sub-phase **P2b**, landing after P2a (the agent-side packages), so each rename stays separately
revertible and `release-tunnel.yml` moves in a diff a reviewer can hold in their head.
Package `roomler-tunnel` → **`roomler-cli`** (lib `roomler_cli`): the emitted bin is already
`roomler`, and the command surface lives in the lib (`roomler_tunnel::cli`, P3e lever D), so
"cli" names what the crate now is — the tunnel client is one of the things it drives, not the
whole of it.

---

## 11. Out of scope

- **Published GitHub release assets.** Already-published names are immutable. The updater's
  pickers key on extension + arch + the `-permachine-` infix, **not** on the `roomler-agent-`
  prefix (`agents/roomler-agent/src/updater.rs:414-484`) — verified, not assumed. This is why P2
  is low-risk and why no asset rename is forced.
- The `ROOMLER__` **server** config prefix — a different product surface, not a retired name.
- `roomler-setup`'s own naming, including the UAC lib-naming rule (`wizard_app` / `wizard_shared`
  dodge "install"/"setup"/"update"/"patch" deliberately).
- `derp-relay`, `tcp-turn-conn`, `tunnel-core`, `localapi` — current names.
- **Any behaviour change.** This FR moves names and adds a guard. If a phase needs a behaviour
  change to make a rename tractable, that is a separate FR.

---

## 12. Field-verification log

| date | version | host / OS | result |
|---|---|---|---|
| — | — | — | *(P6 not started)* |

**To confirm before P4 lands** (§5): read the actual staging directory on a fresh perMachine
Windows host and on a pre-rename perMachine host. The prediction is that they differ
(`…\roomler\roomler\staging` vs `…\roomler\roomler-agent\staging`) and that the SPA affordance is
wrong on the first. If they do **not** differ, §5 is wrong and P4 is rescoped — record that here
either way, per the standing rule that a documented dead end is often the most valuable line in
the log.

---

## Related

- `CLAUDE.md` → *Install-size trim (P3e)*, *P3e Phase 2/3* — the slices that renamed the binaries
  and deliberately left the package names.
- [FR-7](FR-7-signed-releases.md) — release asset naming and the signing pipeline; P2 must not
  disturb the signed payload names.
- [FR-5](FR-5-macos-unattended-update-chain.md) — the macOS update chain that D5's TCC risk rides
  on.
