# Functional Requirements

Every substantial feature or multi-step program gets an **FR**: a spec in this directory
and a tracking issue in [`gjovanov/roomler-ai/issues`](https://github.com/gjovanov/roomler-ai/issues),
modeled on [`gjovanov/lgr#21`](https://github.com/gjovanov/lgr/issues/21). The convention
itself lives in `CLAUDE.md` (`## Functional Requirements (FR) workflow`) so that it loads
in every session; **this file is the number registry.**

## Claiming a number

Add your row to the registry below **in the same commit as your spec file**, then push.

That is the whole protocol — and the reason it is one shared table rather than "scan
`docs/fr/` for the highest N" is that scanning **cannot** work, with or without a
post-create re-check. Two sessions both read `max = N`, both write `FR-N+1`, and git merges
them without a murmur because they touched *different* files. Not hypothetical: the
convention collided **four times on its first day**, the closest pair seven seconds apart —
and the last two happened *after* a scan-and-re-verify rule was already in force.

| collided | resolution |
|---|---|
| `FR-1` (#767, 09:55:42) vs `FR-01` (#768, 09:57:20) | #768 → FR-9 |
| `FR-3` (#773, 10:04:58) vs `FR-3` (#774, 10:05:05) | → FR-6 / FR-5 |
| `FR-5` — three sessions within minutes | → FR-4 / FR-5 / FR-6 |
| `FR-7` (#778, 10:07:14) vs `FR-7` (#780, 10:09:41) | #780 → FR-8 |

The tie-break when one slips through anyway: **the younger claim renumbers** (ambiguous ⇒
the closed/retrospective one). That is repair, though — not allocation.

Editing **one** file makes git the arbiter instead: the second push is rejected as
non-fast-forward, and the rebase shows the number is already taken *before* anything is
published. Same shape as the overlay block allocator, where a unique index arbitrates
concurrent slot claims rather than a lock.

⚠️ Claim **immediately before** you write, never at the start of a long session — the same
discipline as the release-tag race.

⚠️ A number is never reused, including by a withdrawn or superseded FR. Mark the row
instead: a reader who meets `FR-4` in an old commit message must land on the right document.

## Registry

| FR | Issue | Title | Status |
|---|---|---|---|
| [FR-1](FR-1-remote-desktop-drag-smoothness.md) | [#767](https://github.com/gjovanov/roomler-ai/issues/767) | RustDesk-parity remote-desktop drag smoothness | in progress — P1–P4 shipped + field-verified, P5–P7 open |
| [FR-2](FR-2-pin-update-downgrade-guard.md) | [#770](https://github.com/gjovanov/roomler-ai/issues/770) | Pin-update downgrade guard | closed — shipped (#771) + field-verified on prod 2026-08-27 |
| ~~FR-3~~ | — | *vacated* — the #773/#774 collision; both claimants renumbered | never reuse |
| [FR-4](FR-4-conference-media-path-integrity.md) | [#776](https://github.com/gjovanov/roomler-ai/issues/776) | Conference media-path integrity | closed — shipped + field-verified (retrospective) |
| [FR-5](FR-5-macos-unattended-update-chain.md) | [#774](https://github.com/gjovanov/roomler-ai/issues/774) | macOS unattended update chain | closed — field-closed at agent rc.482 (retrospective) |
| [FR-6](FR-6-ci-release-build-speed-slo.md) | [#773](https://github.com/gjovanov/roomler-ai/issues/773) | CI + release build-speed SLO — every lane ≤10 min warm | shipped + field-verified (retrospective) |
| [FR-7](FR-7-signed-releases.md) | [#778](https://github.com/gjovanov/roomler-ai/issues/778) | Signed releases — Windows Authenticode, Linux GPG + provenance, macOS identity | in progress — retrospective; all criteria field-verified except Apple Developer ID (enrolment 5XS5WN8R99 under review); closes on `spctl … Notarized Developer ID` |
| [FR-8](FR-8-claude-session-restore.md) | [#780](https://github.com/gjovanov/roomler-ai/issues/780) | Claude session restore after reboot (`crestore`) | closed — shipped (retrospective); renumbered from FR-7, which #778 claimed 147 s earlier |
| FR-9 | [#768](https://github.com/gjovanov/roomler-ai/issues/768) | Two nodes sharing a LAN converge to a direct carrier | acceptance criteria met; renumbered from `FR-01`; **spec lands with PR #769** |

| [FR-10](FR-10-relay-drag-quality.md) | [#783](https://github.com/gjovanov/roomler-ai/issues/783) | Relay drag quality — IDR thrift on constrained transports | in progress — spec on master (`2366859d`); kept FR-10 by the lower-issue-id rule vs #784 |
| [FR-11](FR-11-server-grids-members-files-invites.md) | [#784](https://github.com/gjovanov/roomler-ai/issues/784) | Server-side grids — members/files/invites, devices online-first default sort, mesh display names | in progress — P1 backend in PR; renumbered from FR-10 (lower-issue-id rule, #783 keeps it) |
| [FR-12](FR-12-onboarding-tutorial.md) | [#788](https://github.com/gjovanov/roomler-ai/issues/788) | Onboarding tutorial — guided welcome tour, callable anytime | design+plan — spec lands with the FR-11 P1 PR |
| [FR-13](FR-13-rc-keyboard-hid-mapping-mac-chords.md) | [#789](https://github.com/gjovanov/roomler-ai/issues/789) | RC keyboard: unmapped HID usages type garbage (CapsLock→`9`); mac Ctrl→Cmd chords | investigated — root causes pinned in spec; fix = agent half + viewer half |
