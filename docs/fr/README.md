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
| `FR-7` (#778, 10:07:14) vs `FR-7` (session-restore spec, 10:09:01) | spec → FR-8 |

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
| FR-2 | [#770](https://github.com/gjovanov/roomler-ai/issues/770) | Pin-update downgrade guard | in progress — spec lands with PR #771 |
| ~~FR-3~~ | — | *vacated* — the #773/#774 collision; both claimants renumbered | never reuse |
| [FR-4](FR-4-conference-media-path-integrity.md) | [#776](https://github.com/gjovanov/roomler-ai/issues/776) | Conference media-path integrity | closed — shipped + field-verified (retrospective) |
| [FR-5](FR-5-macos-unattended-update-chain.md) | [#774](https://github.com/gjovanov/roomler-ai/issues/774) | macOS unattended update chain | closed — field-closed at agent rc.482 (retrospective) |
| [FR-6](FR-6-ci-release-build-speed-slo.md) | [#773](https://github.com/gjovanov/roomler-ai/issues/773) | CI + release build-speed SLO — every lane ≤10 min warm | shipped + field-verified (retrospective) |
| FR-7 | [#778](https://github.com/gjovanov/roomler-ai/issues/778) | Signed releases — Windows Authenticode, Linux GPG + provenance, macOS identity | closed — retrospective; spec pending |
| [FR-8](FR-8-claude-session-restore.md) | — | Claude session restore after reboot (`crestore`) | shipped (retrospective); **needs an issue** — renumbered from FR-7, which #778 claimed 106 s earlier |
| FR-9 | [#768](https://github.com/gjovanov/roomler-ai/issues/768) | Two nodes sharing a LAN converge to a direct carrier | acceptance criteria met; **filed as `FR-01`** — issue + PR #769 spec still to be renamed |

