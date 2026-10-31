# FR-133: Nothing a person reads calls the daemon an "agent" — it is `roomlerd`

**Issue:** [#3177](https://github.com/gjovanov/roomler-ai/issues/3177) · **Status:** P1 in review 2026-10-31 (D1–D3 decided 2026-10-30); spec and claim 2026-10-30 ·
**Related:** FR-46 ([#1051](https://github.com/gjovanov/roomler-ai/issues/1051), the retired `roomler-agent` names; its "MSI installer identity — its own program" row is this FR's P1) ·
FR-21 (the retired-name guard, `scripts/name-audit.sh`) · FR-129 ([#3022](https://github.com/gjovanov/roomler-ai/issues/3022), the wizard; its phase C renames the wizard's own words)

## Goal

**The daemon has one name a person ever reads: `roomlerd` (or "the daemon" in a sentence).** The
wizard, the Windows installer's Apps & Features entry and its UAC prompt, the macOS and Linux
packages, the menu-bar/tray companion, the web app, the docs site, and every line an installer or
the CLI prints say it. "Agent" survives only where it is a wire or operating-system identity that a
rename would break, and each of those is listed below with its reason.

## Evidence

Anchors in this section are master `4bc9a9fc8` (the claim). P1 moved the
installer ones; the P1 row under Phases has where they are now.

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
| P1 | the install surfaces: the MSI ProductName, shortcut and title (both flavours), the Authenticode description, the `.deb`/`.rpm` summary; `install.ps1`'s two lookups and the verify script accept both names; `install.sh`/`install.ps1` output. The wizard's own words ride FR-129 phase C, and the macOS grant notices ride #3164. **Where it landed:** `agents/roomlerd/wix/main.wxs:63` (Name), `:304` (shortcut), `:324` (title); `agents/roomlerd/wix-perMachine/main.wxs:71`, `:308`; `.github/workflows/release-agent.yml:1948`; `agents/roomlerd/Cargo.toml:7` (roomlerd ships no `.rpm`: only the companion does, and its summary never said "agent"); `scripts/install.ps1:109` (the exact names, old and new, per flavour), `:294`, `:476`; `agents/roomlerd/scripts/m3-a1-verify-win11.ps1:387`. Proved by `.github/workflows/installer-smoke.yml:871` | copy: a revert. The lookups match both names, so an older or a newer MSI is always found | in review: draft PR [#3209](https://github.com/gjovanov/roomler-ai/pull/3209), field-verified 2026-10-31 on the CI MSI (the log below). The release half is owed: AC1 and AC2 name the first release, and the UAC text and `apt show` only exist on a release |
| P2 | the companion and the web app, the viewer's banners included, and the daemon's own notifications that still open "Roomler agent:" (`agents/roomlerd/src/signaling.rs:1095`, `:1149`, `:1364`, `agents/roomlerd/src/app.rs:2523`) | a revert | proposed |
| P3 | the docs site (`ui/docs/content`) | a revert | proposed |
| P4 | a guard in CI: person-facing strings may not say "agent" except the listed identities (an allowlist, in the shape of `name-audit.sh`) | the guard's own baseline | proposed |
| D | docs: `docs/installation.md` (what Windows lists), the `ship-it` skill (the signing description), this spec's log | — | `docs/installation.md` § Windows rides P1's PR, and so does `ship-it` § 2: the MSI's name is display only, and a change to it ships in one order, because `install.ps1` matches exact names and is served by the server image. Neither `ship-it` nor `docs/code-signing.md` ever named the signing description (checked 2026-10-31), so there was nothing to rename. This log, as phases land |

## Acceptance criteria

- [ ] **AC1** — A fresh Windows install from the released MSI (both flavours) appears in Apps &
      Features under the new name (D1), and its UAC prompt shows the new description.
- [ ] **AC2** — An upgrade from 0.4.188 (named "Roomler Agent…") to the first release with P1
      leaves ONE entry, under the new name, and keeps the service and its SystemContext mode.
- [ ] **AC3** — `install.ps1 -Uninstall` removes an install made before P1 and one made after it.
- [ ] **AC4** — `apt show roomlerd` and `dnf info roomlerd` print a summary without "agent".
      (Found 2026-10-31: roomlerd ships no `.rpm` at all, so `dnf info roomlerd` has nothing
      to show; only the companion ships one, and its `summary` never said "agent".)
- [ ] **AC5** — Every page, progress line and Done text of the released wizard, every companion
      page and the web app's person-facing strings contain no "agent" outside the allowlist
      (asserted by tests, then by P4's guard).
- [ ] **AC6** — The docs site's pages say `roomlerd`.
- [ ] **AC7** — Docs: `docs/installation.md` and the `ship-it` skill updated, with a
      `docs/README.md` row if a doc is added.

## Open decisions

| # | question | decision |
|---|---|---|
| D1 | the Apps & Features name: "Roomler", "Roomler Daemon", or "roomlerd"? | **DECIDED 2026-10-30 by the operator, as recommended: "Roomler"** (and "Roomler (per-Machine)"): the MSI installs the product — the daemon, the `roomler` CLI and the companion — not the daemon alone; the service is already displayed as "Roomler" |
| D2 | does the web app's Devices page keep any "agent" column or filter word? | **DECIDED 2026-10-30 by the operator, as recommended: no**: "daemon version", "daemon" vs "tunnel client" |
| D3 | rename the `agent-v*` tag family? | **DECIDED 2026-10-30 by the operator, as recommended: no**: every deployed updater polls it; a dual-accept migration is weeks of fleet turnover for a name no person should see once versions replace tags |

## Out of scope

- Wire names and OS identities (the table above).
- The retired `roomler-agent` names (FR-46).
- Engineering docs, code comments and log lines.

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-10-30 | released 0.4.188 (wizard + MSI), Windows 11 | ❌ the operator's run: the wizard's progress said "Resolved tag agent-v0.4.188"; the MSI's Windows Installer events name the product "Roomler Agent (per-Machine)"; the operator: "We shouldn't use anywhere agent" |
| 2026-10-31 | released 0.4.189 per-Machine MSI, Windows 11 (a dev machine; a read of the Uninstall keys, nothing installed) | ❌ as expected before P1: exactly one Roomler entry in Apps & Features, `'Roomler Agent (per-Machine)' 0.4.189`, in HKLM |
| 2026-10-31 | released 0.4.191 per-Machine MSI + SystemContext, a throwaway Win11 vmtest cell, a STANDARD enrollment (an ephemeral device unenrolls on the clean stop an upgrade makes) | ❌ before P1, as expected. Settings → Apps → Installed apps, searched "roomler": 1 app found, **Roomler Agent (per-Machine)** 0.4.191. Service Running, SystemContext on, the device online, and the daemon's own log says `auto-update disabled`. The MSI's tables: ProductName `Roomler Agent (per-Machine)`, UpgradeCode `{2A8E9C2D-3F1A-5E3F-C2B4-7C2F3D8F5E02}` |
| 2026-10-31 | the same cell: the released 0.4.191 binary against this branch's per-Machine MSI (the `installer-smoke` artifact, MSI 0.4.192.1), before installing it | ✅ `roomlerd artifact-version`, the deployed updater's own reader: `bound: … is agent-v0.4.192 (it says 0.4.192.1)`, exit 0. It is a keyed query, so the new name cannot move it |
| 2026-10-31 | the same cell: that MSI over the release, hand-run, no property | ✅ exit 0 in 10 s. The same search: 1 app found, **Roomler (per-Machine)** 0.4.192.1. The installer's log says how it found the old product: `FindRelatedProducts`, then `WIX_UPGRADE_DETECTED` = the 0.4.191 ProductCode, then `RemoveExistingProducts`; the two MSIs' tables share the UpgradeCode and differ in ProductName. Service Running, SystemContext kept, the SAME device row online. Host and worker started at the upgrade and were alive 1 min 44 s later, and `roomlerd.exe` is the unsigned CI build, so "online" is the new binary, which reports the same 0.4.191 |
| 2026-10-31 | the same cell: `install.ps1 -Uninstall` from this branch, in Windows PowerShell 5.1 | ✅ on the upgraded per-Machine product, on a fresh per-User install and on a real pre-P1 install (released 0.4.191 per-Machine): each time 0 entries, no service, no `roomlerd.exe` |
| 2026-10-31 | the same cell: this branch's per-User MSI, fresh | ✅ The same search: 1 app found, **Roomler** 0.4.192.1; Start menu `Roomler\Roomler.lnk`. The README as installed: 10 commands intact, none missing its space. `roomlerd --help` opens with the new summary. From an elevated SSH session the ARP record lands in HKLM, the mirror `installer-smoke` already documents: pre-existing, nothing to do with the name |
| 2026-10-31 | the same cell: the `install.ps1` production serves today, on that per-User install | ❌ as predicted, and why `ship-it` § 2 states an order: `no 'Roomler Agent' MSI products registered`, `done.`, exit 0, and the product stays installed. The image carrying this script is promoted BEFORE the first release with the new name |
