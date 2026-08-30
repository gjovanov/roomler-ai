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

**This file is written tag-free on purpose.** A sweep walks the repo, and the
first version of this document named real tags in its own prose — so the sweep
rewrote its explanations into nonsense.

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

## The four things that go wrong

**1. `--replace-text` does NOT touch commit messages.** It rewrites blob
contents only; commit and annotated-tag messages are a separate surface behind
`--replace-message`, and filter-repo says nothing at all when you omit it. The
first run of this sweep reported success, left every blob clean, and left 745
real names in the commit log — which on this repo is the *richer* of the two
surfaces, because the field-test narratives live in commit bodies. Always
verify against `git log --all --format=%B`, never the tree alone.

**2. `residual: none` is the only success condition.** The residual scan
re-reads everything case-INSENSITIVELY after the rewrite. The first pass of the
2026-08-28 sweep listed only uppercase spellings, reported success, and left 60
real tags in the tree because half the prose was lowercase. Never trust a run
that ends any other way, whatever else it printed.

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
