---
name: sanitize-hostnames
description: Rewrite real machine names (corp asset tags, Windows default hostnames, personal computer names) to the product display names, across the working tree, the whole git history, and the GitHub issues/PRs/comments/releases. Use before publishing docs, after field notes land, or when a review turns up a real hostname in the repo.
---

# Sanitising machine names

Field notes are written in the heat of debugging, and a debugging session
writes down the name of the machine it is debugging. That name is a corp asset
tag, or a Windows default hostname, or someone's computer named after them.
On a **public** repo those are reconnaissance value: they map the fleet and say
which host runs what.

This skill rewrites them to the display names the product itself shows, so the
narratives stay readable — "the corp laptop over the VPN" is still a useful
sentence — while the tags themselves are gone.

## The map lives OUTSIDE the repo

`C:\dev\gjovanov\sanitize-map.txt`

That path is deliberate and non-negotiable: the map is the one place a real
name and its replacement sit **side by side**, so committing it would publish
exactly what the sweep exists to remove. Never commit it, never quote it in a
commit message, never paste it into an issue or a PR.

**Every file in this directory is written tag-free on purpose, prose included.**
A sweep walks the repo and does not exempt its own tooling. The first version of
this document named real tags in its explanations and the sweep rewrote them
into nonsense; the second time round the Python docstrings did it, and a history
rewrite turned *"a three-letter shorthand like `<tag>`"* into a sentence naming
an alias, and *"`<old-alias>` → `<new-alias>`"* into `X → X`.

So: describe the RULES, never the instances. Check this directory against the
map after any sweep —

```bash
python3 .claude/skills/sanitize-hostnames/sanitize.py \
    --map <map> --root .claude/skills/sanitize-hostnames --check   # want 0 / none
```

⚠️ Note what survived both times: the regexes. `\bCLK…` has no word boundary
before the letters (the preceding `b` of `\b` is a word character), so patterns
are accidentally immune while the comments beside them are not. The tool keeps
working and only its explanation rots — which is the failure mode that lasts,
because nothing fails.

### Map format

```
real-name = replacement      # one pair per line, '#' starts a comment
!keep goran                  # a spelling that must survive verbatim
```

Matching is **case-sensitive, whole-word, literal** — so list every casing that
actually occurs. `!keep` declares a spelling that must never be rewritten *and*
must never be reported as residue; it is matched case-sensitively, because its
whole reason to exist is a token whose uppercase form is a hostname and whose
lowercase form is a profile directory and an email local part.

## Running it

```bash
R=/mnt/c/dev/gjovanov/roomler-ai; M=/mnt/c/dev/gjovanov/sanitize-map.txt
S=$R/.claude/skills/sanitize-hostnames

# 1. working tree
python3 $S/sanitize.py --map $M --root $R --apply     # or --check for CI

# 2. GitHub: issue + PR titles/bodies, comments, release notes
python3 $S/sanitize_github.py --map $M --apply        # or --check

# 3. whole git history — see the warnings below before running this
python3 $S/sanitize.py --map $M --gen-filter-repo ~/sanitize/replace.txt
git clone --mirror https://github.com/<owner>/<repo>.git ~/sanitize/repo.git
cd ~/sanitize/repo.git
git for-each-ref --format="delete %(refname)" refs/pull | git update-ref --stdin
python3 ~/bin/git-filter-repo \
    --replace-text    ~/sanitize/replace.txt \
    --replace-message ~/sanitize/replace.txt \
    --replace-refs delete-no-add --force
git push --mirror https://github.com/<owner>/<repo>.git      # irreversible
```

## Three layers, and only the first one is cheap

| layer | stops it at | enable |
|---|---|---|
| `.githooks/pre-commit` | the commit | `git config core.hooksPath .githooks` |
| CI job **No real machine names** | the merge | required status check on `master` |
| the sweep below | after publication | run it by hand |

Layer 3 has run three times now. Each run force-pushes ~700 branches and ~660
tags with GitHub Actions disabled around it, and it still **cannot remove
anything from GitHub** — the pre-rewrite commits stay reachable by SHA through
`refs/pull/*` until GitHub Support runs GC. Treat every sweep as damage
control, and the hook as the actual fix.

⚠️ **Set `core.hooksPath` in every clone.** It is per-clone config, so a fresh
clone has no hook until someone runs that line. Worktrees **share** that config
and the hook is tracked, so setting it once in the clone covers every worktree
made from it — but a worktree on a branch predating the hook still has no file,
which is the harmless `tool absent -- do not block` path.

### The hook's exit-code contract — the hard-won part

`.githooks/pre-commit` blocks on **`EXIT_FOUND` (1) and nothing else**. Every
other status means *the guard did not answer*, and the hook says so and lets the
commit through, because the required CI check still refuses the merge.

| status | meaning | hook |
|---|---|---|
| `0` | clean | commit |
| `1` | names found | **REFUSE** |
| `2` | bad arguments — hook and guard drifted | warn, allow |
| `20` | guard raised, or a git call inside it failed | warn, allow |

⚠️ **This existed as one status until 2026-09-04, and it was worse than useless.**
`if ! out=$(run_guard)` collapsed every failure into "a real machine name",
so the hook told authors they had committed a hostname when it had merely
crashed. Measured twice that day, from two unrelated causes:

- a worktree whose `check_shapes.py` predated `--staged`, so argparse exited 2;
- **every** worktree on Windows, where `.git` is a file holding a Windows path
  WSL's git cannot follow — so `git` inside the WSL fallback exited 128 and the
  guard was inert in ~53 worktrees while working fine in the main clone. The
  hook now resolves the git dir natively and exports it across the boundary.

🔑 The rule this is an instance of, already written three times in this
directory: **a check whose result cannot distinguish "passed" from "never
answered" is not a check.** A guard that cries hostname when it means "I
crashed" is one somebody silences with `--no-verify`, and that removes the layer
permanently. `selftest.sh` asserts each status leads to its own outcome, and
asserts a **non-1** status specifically — "non-zero" would pass on the bug.

⚠️ The hook must be committed **mode 755**. Git skips a non-executable hook in
silence, which is layer 1 disarmed with nothing anywhere to say so; the selftest
checks this too.

## The four things that go wrong

**1. `--replace-text` does NOT touch commit messages.** It rewrites blob
contents only; commit and annotated-tag messages are a separate surface behind
`--replace-message`, and filter-repo says nothing at all when you omit it. The
first run of this sweep reported success, left every blob clean, and left 745
real names in the commit log — which on this repo is the *richer* of the two
surfaces, because the field-test narratives live in commit bodies. Always
verify against `git log --all --format=%B`, never the tree alone.

**2. `residual: none` is the only success condition, and CASE IS THE TRAP.**
The residual scan re-reads everything case-INSENSITIVELY. The first pass of the
2026-08-28 sweep listed only uppercase spellings, reported success, and left 60
real tags in the tree because half the prose was lowercase.

⚠️ That exact mistake was then **rebuilt one file over**: `check_shapes.py`
shipped with uppercase-only patterns, and on 2026-09-04 a field log wrote two
asset tags in lowercase — the guard printed `none found`, CI went green, and
the names reached a public repo and 15 GitHub items. Everything here matches
case-insensitively now, and `selftest.sh` carries every canary in both casings
so the regression cannot come back quietly. People write a hostname however it
came out of their terminal; a check that assumes a casing is not a check.

**3. Longest key first, or you orphan a prefix.** A `DESKTOP-`/`LAPTOP-`
qualified form has to be rewritten before its bare tag, or the qualifier is
left stranded on an already-rewritten name. `load_map` sorts by key length and
every consumer iterates that one list, so the rule cannot be applied in one
place and forgotten in another.

**4. Whole-word matching is what makes short keys safe** — and it has to be
*verified*, not assumed. Before mapping a 3-letter shorthand, dump every blob
in history and read the contexts:

```bash
git cat-file --batch-all-objects --batch-check='%(objecttype) %(objectname)' \
  | awk '$1=="blob"{print $2}' | git cat-file --batch \
  | grep -aoE '.{0,45}\b<KEY>\b.{0,35}' | sort -u
```

If any hit is a code identifier rather than prose, the key is not safe to map.

## Before rewriting history, know what it does and does not achieve

- **It does not remove anything from GitHub.** Old commits stay reachable by
  SHA through the `refs/pull/*` refs, effectively forever, unless GitHub
  Support runs GC on the repo. Drop those refs locally before filtering (you
  cannot push to them anyway) and treat the rewrite as cleaning what people
  *read*, not what a determined fetch can still reach.
- **Every SHA changes.** Clones elsewhere must re-clone or hard-reset; SHAs
  quoted in docs, issues and release notes dangle.
- **Back up first**: `git bundle create <path>.bundle --all` captures every ref
  in one file and restores with a plain `git clone`.
- Branches that exist only locally are not in a mirror clone and so are not
  rewritten. They keep the old content until deleted or rebuilt.

## Adding a name

Find the replacement, do not invent one. In order of authority: the
`agents.display_name` column in the product database; then the alias table
recorded in the earlier privacy commit (`git log --grep='role-based aliases'`),
which is where the existing generic host aliases come from; then, only if
neither has it, the next free slot in the series already in use.
