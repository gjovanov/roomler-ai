---
paths:
  - "docs/fr/**"
  - "scripts/fr-*.sh"
  - "scripts/fr-ac-debt-baseline.txt"
---

# The FR workflow in full — the measurements behind each step

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps the six steps. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

## Functional Requirements (FR) workflow — STANDING RULE (operator, 2026-08-27)

Every substantial feature or multi-step program — a new capability, a performance
arc, a protocol change, anything bigger than a one-PR fix — gets a **Functional
Requirement** tracked in GitHub. **Creating the FR is a step of the PLAN**, not a
write-up filed afterwards. Registry and full protocol: **`docs/fr/README.md`**.

0. **Open the issue BEFORE you implement.** It is the only collision guard that
   actually works: a memory file is invisible to a parallel session, an open issue
   is not. Re-fetch master and list open FR issues before starting.
1. **Spec doc** `docs/fr/FR-N-<slug>.md`: goal, root-cause/field evidence, key
   design with `file:line` anchors verified against master, a phase/status table
   with each phase's kill switch, acceptance criteria as checkboxes, open
   decisions, out-of-scope, and a field-verification log.
   ⚠️ **Claim the number by adding your row to `docs/fr/README.md` in the same
   commit as the spec** — never by scanning the directory. Two sessions both read
   `max = N`, both write `FR-N+1`, and git merges them without a murmur because
   they touched *different* files (measured three times in one day, twice while a
   scan-and-re-verify rule was already in force). Editing one shared table makes
   git the arbiter instead: the loser's push is rejected as non-fast-forward.
   Enforced by `scripts/fr-registry-audit.sh` + CI.
   ⚠️ If one still slips through: **the LOWER issue number keeps `FR-N`, the HIGHER
   renumbers** — title, spec filename, ledger row and in-body references together,
   to the next free N, never into a vacated one. Issue ids are server-assigned and
   monotonic, so two sessions that never talk compute the same winner. Numbers
   already settled STAY settled.
2. **GitHub issue** `FR-N: <title>` with the spec link, and Goal / Key design /
   Acceptance criteria / Open decisions / Out of scope / Related.
3. **Comment as you ship** — a `## Step log` table appended as each PR merges, and
   a `## Result — field-verified on <version>` carrying the actual measurement.
   ⚠️ **CI green is not a result.** A field test must be shown to FAIL on the
   current deploy first, or its pass proves nothing — record both runs. Record the
   wrong turns too; a dead end documented is often the most valuable line in the log.
4. **Close** only when the acceptance criteria are field-verified; a regression
   reopens it with the evidence.
   ⚠️ **Tick the boxes in the same breath as the close** — the ticks *are* the
   record that the verification happened, and an unticked criterion on a closed
   issue cannot afterwards be told apart from one that was never verified.
   Measured: 8 closed FRs carrying 28 unticked criteria on 2026-09-01, **19
   carrying 74** three weeks later, entirely unobserved. Enforced by
   `scripts/fr-verification-debt.sh` + a daily lane — scheduled, not just
   push-triggered, because closing an issue leaves no trace in the repo at all.
   Pre-existing debt is pinned per-FR in `scripts/fr-ac-debt-baseline.txt`, each
   line saying *why* that FR closed without them.
5. **Docs before close** (operator, 2026-09-05): closing requires the docs that
   describe what it built to be updated or created, in the house style of the other
   `docs/*.md` — **mermaid diagrams**, tables, callouts, `file:line` anchors, and a
   row in `docs/README.md`'s index. It is a phase row *and* an acceptance criterion
   in every spec, ticked before the close.
   ⚠️ Enforced by the same `scripts/fr-verification-debt.sh`: a spec **bound** by
   this rule — its issue opened, or it closed, after the rule reached master
   (#1401, 2026-09-05T20:49Z) — must carry the docs criterion, recognised by its
   `docs/README.md` index-row commitment. Without it the ticked-box check cannot
   see the docs at all: FR-81 had every box ticked and nothing public documenting it.
