# FR-133: Nothing a person reads calls the daemon an "agent" — it is `roomlerd`

**Issue:** [#3177](https://github.com/gjovanov/roomler-ai/issues/3177) · **Status:** proposed 2026-10-30 — spec and claim ·
**Related:** FR-46 ([#1051](https://github.com/gjovanov/roomler-ai/issues/1051), the retired `roomler-agent` names; its "MSI installer identity — its own program" row is this FR's P1) ·
FR-21 (the retired-name guard, `scripts/name-audit.sh`) · FR-129 ([#3022](https://github.com/gjovanov/roomler-ai/issues/3022), the wizard; its phase C renames the wizard's own words)

## Goal

**The daemon has one name a person ever reads: `roomlerd` (or "the daemon" in a sentence).** The
wizard, the Windows installer's Apps & Features entry and its UAC prompt, the macOS and Linux
packages, the menu-bar/tray companion, the web app, the docs site, and every line an installer or
the CLI prints say it. "Agent" survives only where it is a wire or operating-system identity that a
rename would break, and each of those is listed below with its reason.

## Evidence

The operator, 2026-10-30, after running the released 0.4.188 wizard: "the installer uses agent
instead of roomlerd, which was renamed agent to roomlerd originally. We shouldn't use anywhere
agent." What that run showed, and what master carries (counts are person-facing string lines,
approximate, wire/identity names excluded):

| surface | where | what a person sees |
|---|---|---|
| Windows Apps & Features | `agents/roomlerd/wix/main.wxs:56`, `agents/roomlerd/wix-perMachine/main.wxs:65` (ProductName); `:291-292` (Start-menu shortcut), `:312` / `:302` (the installer UI's title) | **Roomler Agent**, **Roomler Agent (per-Machine)** |
| the UAC prompt on every MSI | `.github/workflows/release-agent.yml:1948` (the Authenticode description) | "Roomler Agent installer" |
| `apt show` / `dnf info` summary | `agents/roomlerd/Cargo.toml:7` (`description`, the package summary) | "Roomler AI remote-control agent — …" (the extended description already says "Roomler Daemon") |
| the wizard | `agents/roomler-setup/src/front/app.js:1667` and ~35 more lines | "Resolved tag **agent**-v0.4.188" |
| the companion | `agents/roomler-desktop/src/` | ~17 lines |
| `install.sh` / `install.ps1` output | `scripts/` | ~10 lines |
| the web app | `ui/src/components`, `ui/src/views` | ~378 lines (e.g. "agent version" on the Devices page) |
| the docs site | `ui/docs/content` | ~237 lines |

FR-46 already classified the MSI's display identity as "renaming breaks upgrade detection — its
own program" (`docs/fr/FR-46-eradicate-the-retired-agent-name.md:276`). Measured on master:
Windows Installer matches an upgrade by its UpgradeCode, and `install_detect` keys on the
UpgradeCode's registry presence, so neither reads the ProductName. What does read it is
`scripts/install.ps1`: the post-install check (`:279`) and `-Uninstall` (`:453`) find the product by
`DisplayName -like 'Roomler Agent*'`, as does the M3 verify script
(`agents/roomlerd/scripts/m3-a1-verify-win11.ps1:385`). A rename is therefore safe for upgrades
and needs those lookups to accept both names.

## Key design

| class | examples | treatment |
|---|---|---|
| **person-facing text** | wizard pages and progress lines, the companion, the web app, the docs site, installer and CLI output, the viewer's banners | renamed: `roomlerd`, or "the daemon". A version is shown as a version (`0.4.188`), never as its tag (`agent-v0.4.188`) |
| **display identity** | the MSI ProductName (Apps & Features), its shortcut and UI title, the Authenticode description, the `.deb`/`.rpm` summary | renamed (D1). `install.ps1` and the verify script match the old and the new name. The old one is kept as a `RETIRED-NAME-ANCHOR` for the machines installed before |
| **wire names** | the `agent-v*` release tags every deployed updater polls; `/api/agent/*`; the `agents` collection; `TokenType::Agent`; `/ws?role=agent` | **kept**. Each is a fleet migration with a dual-accept window (FR-46 P4's pattern), and none of them is ever shown to a person once the text above shows versions |
| **OS identities** | `com.roomler.agent`, the macOS bundle id and launchd label that every Mac's TCC grants key on (FR-46 D1); Apple's own word "LaunchAgent" | **kept** |
| **engineering prose** | `docs/*.md`, code comments, `tracing` lines | not this FR |

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P0 | spec + ledger row + issue | — | this commit |
| P1 | the install surfaces: the MSI ProductName, shortcut and title (both flavours), the Authenticode description, the `.deb`/`.rpm` summary; `install.ps1`'s two lookups and the verify script accept both names; `install.sh`/`install.ps1` output. The wizard's own words ride FR-129 phase C, and the macOS grant notices ride #3164 | copy: a revert. The lookups match both names, so an older or a newer MSI is always found | proposed |
| P2 | the companion and the web app, the viewer's banners included | a revert | proposed |
| P3 | the docs site (`ui/docs/content`) | a revert | proposed |
| P4 | a guard in CI: person-facing strings may not say "agent" except the listed identities (an allowlist, in the shape of `name-audit.sh`) | the guard's own baseline | proposed |
| D | docs: `docs/installation.md` (what Windows lists), the `ship-it` skill (the signing description), this spec's log | — | owed |

## Acceptance criteria

- [ ] **AC1** — A fresh Windows install from the released MSI (both flavours) appears in Apps &
      Features under the new name (D1), and its UAC prompt shows the new description.
- [ ] **AC2** — An upgrade from 0.4.188 (named "Roomler Agent…") to the first release with P1
      leaves ONE entry, under the new name, and keeps the service and its SystemContext mode.
- [ ] **AC3** — `install.ps1 -Uninstall` removes an install made before P1 and one made after it.
- [ ] **AC4** — `apt show roomlerd` and `dnf info roomlerd` print a summary without "agent".
- [ ] **AC5** — Every page, progress line and Done text of the released wizard, every companion
      page and the web app's person-facing strings contain no "agent" outside the allowlist
      (asserted by tests, then by P4's guard).
- [ ] **AC6** — The docs site's pages say `roomlerd`.
- [ ] **AC7** — Docs: `docs/installation.md` and the `ship-it` skill updated, with a
      `docs/README.md` row if a doc is added.

## Open decisions

| # | question | recommendation |
|---|---|---|
| D1 | the Apps & Features name: "Roomler", "Roomler Daemon", or "roomlerd"? | **"Roomler"** (and "Roomler (per-Machine)"): the MSI installs the product — the daemon, the `roomler` CLI and the companion — not the daemon alone; the service is already displayed as "Roomler" |
| D2 | does the web app's Devices page keep any "agent" column or filter word? | no: "daemon version", "daemon" vs "tunnel client" |
| D3 | rename the `agent-v*` tag family? | no: every deployed updater polls it; a dual-accept migration is weeks of fleet turnover for a name no person should see once versions replace tags |

## Out of scope

- Wire names and OS identities (the table above).
- The retired `roomler-agent` names (FR-46).
- Engineering docs, code comments and log lines.

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-10-30 | released 0.4.188 (wizard + MSI), Windows 11 | ❌ the operator's run: the wizard's progress said "Resolved tag agent-v0.4.188"; the MSI's Windows Installer events name the product "Roomler Agent (per-Machine)"; the operator: "We shouldn't use anywhere agent" |
