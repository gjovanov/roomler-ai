#!/usr/bin/env python3
"""Rewrite real machine names to their product display names.

One map, four surfaces: the working tree, git history (via a generated
git-filter-repo expression file), the GitHub issue/PR/comment/release text,
and a CI guard. The map itself lives OUTSIDE the repo -- it is the one place a
real name and its replacement sit side by side, so it must never be committed.

The three rules that make this safe, each learned the hard way:

  * LONGEST KEY FIRST.  A `DESKTOP-`/`LAPTOP-` qualified hostname has to be
    rewritten before its bare tag, or the qualifier is orphaned onto an
    already-rewritten name and you get `DESKTOP-<alias>` where the map said
    `<alias>`.
  * WHOLE WORD ONLY.  This is what makes a 3-letter shorthand like `CORPLAP-3` safe
    to map at all.  Verified against every blob in this repo's history: all
    1145 bare `CORPLAP-3`/`CORPLAP-3` occurrences are prose, not one is a code identifier.
  * CASE-SENSITIVE MATCH, CASE-INSENSITIVE RESIDUAL SCAN.  The first pass of
    the 2026-08-28 sweep listed only UPPERCASE spellings, reported success,
    and left 60 real tags in the tree because half the prose was lowercase.
    The residual scan is what caught it.  A run that does not end
    `residual: none` has not finished, whatever else it printed.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import re
import sys
from pathlib import Path

# Binary and vendored things a text sweep must not walk into.
SKIP_DIRS = {
    ".git", "node_modules", "target", "dist", "build", ".venv", "venv",
    "__pycache__", ".next", "coverage", "playwright-report", "test-results",
}
SKIP_SUFFIXES = {
    ".png", ".jpg", ".jpeg", ".gif", ".ico", ".webp", ".svg", ".pdf",
    ".woff", ".woff2", ".ttf", ".otf", ".eot", ".mp4", ".webm", ".mp3",
    ".zip", ".gz", ".xz", ".bz2", ".7z", ".msi", ".exe", ".dll", ".pkg",
    ".deb", ".so", ".dylib", ".lib", ".pdb", ".bin", ".wasm",
}


def load_map(path):
    """Parse the map into (pairs, keep).

    `real = replacement` is a rewrite rule. `!keep <token>` declares a
    case-spelling that must NOT be rewritten and must NOT be reported as
    residue -- needed because the residual scan is case-insensitive on
    purpose, so mapping the hostname `NEO16` makes it shout about every
    `C:\\Users\\goran` path and every `goran.jovanov@` author address. Without
    the directive the only ways to silence that are to map the lowercase form
    (which would rewrite the author identity of every commit in the repo) or
    to stop trusting the residual line -- and the residual line is the whole
    reason the 2026-08-28 sweep's 60 missed tags were ever found.

    Pairs come back longest key first. Sorting here is what enforces the
    longest-first rule for every consumer -- the tree sweeper, the filter-repo
    generator and the GitHub sweeper all iterate this one list, so they cannot
    disagree about it.
    """
    pairs, keep = [], set()
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("!keep"):
            token = line[len("!keep"):].strip()
            if not token:
                sys.exit("%s:%d: !keep needs a token" % (path, lineno))
            keep.add(token)
            continue
        if "=" not in line:
            sys.exit("%s:%d: expected 'real = replacement', got %r" % (path, lineno, raw))
        real, repl = [part.strip() for part in line.split("=", 1)]
        if not real:
            sys.exit("%s:%d: empty key" % (path, lineno))
        pairs.append((real, repl))
    pairs.sort(key=lambda kv: len(kv[0]), reverse=True)

    # Case-SENSITIVE, because the whole point of the directive is that NEO16
    # is a rewrite key while goran is kept -- they differ only in case. A
    # !keep of the EXACT spelling of a rewrite key would silently disarm that
    # rule's residual check, which is what this rejects.
    clash = keep & {k for k, _ in pairs}
    if clash:
        sys.exit("%s: !keep %s is also a rewrite key" % (path, ", ".join(sorted(clash))))
    return pairs, keep


def build_regex(pairs, flags=0):
    """One alternation, longest branch first, so Python's leftmost-first
    alternation semantics give longest-key-first for free in a single pass.
    A per-key loop would re-scan text an already-applied rule had rewritten."""
    return re.compile(r"\b(?:" + "|".join(re.escape(k) for k, _ in pairs) + r")\b", flags)


def apply_text(text, pairs, rx):
    table = dict(pairs)
    state = {"hits": 0}

    def sub(m):
        state["hits"] += 1
        return table[m.group(0)]

    return rx.sub(sub, text), state["hits"]


def residual_scan(text, pairs, safe, keep=frozenset()):
    """Case-INSENSITIVE re-scan; catches a spelling the map does not list.

    Two exemptions, exempt on DIFFERENT terms. `safe` holds the lowercased
    replacements -- a replacement may legitimately contain a key (none does
    today, but `corplap-3 -> corplap-3` was one edit away from it), and case
    does not matter for those. `keep` is matched case-SENSITIVELY, because
    `NEO16` the hostname is rewritten while `goran` the profile directory is
    not, and the two differ only in case.
    """
    rx = build_regex(pairs, re.I)
    return {h for h in rx.findall(text) if h.lower() not in safe and h not in keep}


def walk(root):
    """Yield the files to sweep -- TRACKED files when this is a git repo.

    Asking git rather than the filesystem is both the fast answer and the
    correct one. Correct, because only committed content is published, and a
    checkout carries plenty that is not: this repo keeps ~20 stale worktrees
    under .claude/worktrees/, each a full old checkout, and walking those made
    the shape guard report 316 hits for names that had already been swept out
    of everything the repo actually contains. Fast, because os.walk over a
    Windows drive from WSL takes minutes on a tree this size and `git ls-files`
    takes under a second.
    """
    # --cached: what is committed. --others --exclude-standard: files that are
    # new but NOT ignored, i.e. exactly what a commit is about to add. Tracked
    # alone is not enough -- a name arrives in a NEW file, and a guard blind to
    # new files passes the very commit that introduces one. (Found by a canary:
    # every planted name was missed until this flag was added.) The ignore rules
    # still apply, which is what keeps the stale worktrees out.
    try:
        out = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z",
             "--cached", "--others", "--exclude-standard"],
            capture_output=True, check=True)
        names = [n for n in out.stdout.decode("utf-8", "replace").split("\0") if n]
        for name in names:
            p = root / name
            if p.suffix.lower() in SKIP_SUFFIXES or not p.is_file():
                continue
            yield p
        return
    except (subprocess.CalledProcessError, FileNotFoundError, OSError):
        pass  # not a git repo (or no git) -- fall back to walking the tree

    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            p = Path(dirpath) / name
            if p.suffix.lower() in SKIP_SUFFIXES:
                continue
            yield p


def read_text(p):
    # newline='' keeps CRLF exactly as found -- this working copy is checked
    # out with CRLF and a careless rewrite shows up as a whole-file diff on
    # every touched file, burying the real change.
    try:
        with open(p, "r", encoding="utf-8", newline="") as fh:
            return fh.read()
    except (UnicodeDecodeError, OSError):
        return None


def sweep_tree(root, pairs, keep, apply_changes):
    rx = build_regex(pairs)
    safe = {v.lower() for _, v in pairs}
    total = files = 0
    for p in walk(root):
        text = read_text(p)
        if text is None:
            continue
        new, hits = apply_text(text, pairs, rx)
        if hits:
            total += hits
            files += 1
            print("  %4d  %s" % (hits, p.relative_to(root).as_posix()))
            if apply_changes:
                with open(p, "w", encoding="utf-8", newline="") as fh:
                    fh.write(new)
    verb = "rewrote" if apply_changes else "would rewrite"
    print("%s: %d occurrences in %d files" % (verb, total, files))

    leftovers = set()
    for p in walk(root):
        text = read_text(p)
        if text is not None:
            leftovers |= residual_scan(text, pairs, safe, keep)
    print("residual: %s" % (", ".join(sorted(leftovers)) if leftovers else "none"))
    return (1 if leftovers else 0) if apply_changes else (1 if total else 0)


def gen_filter_repo(pairs, out):
    """Emit a git-filter-repo expression file, for BOTH of its message flags.

    ⚠️ The file must be passed TWICE, once per flag::

        git filter-repo --replace-text FILE --replace-message FILE ...

    `--replace-text` rewrites blob CONTENTS ONLY. Commit and annotated-tag
    messages are a separate surface behind `--replace-message`, and filter-repo
    says nothing when you omit it: the first run here reported success, left
    every blob clean, and left 745 real machine names sitting in the commit
    log -- which on this repo is the richer surface of the two, because the
    field-test narratives that name the machines are written in commit bodies.
    Verify a rewrite against `git log --all --format=%B`, never against the
    tree alone.

    `regex:` rather than a literal, so the same \\b whole-word rule the tree
    sweeper uses also governs the rewrite; the two must not be able to differ.
    """
    lines = ["regex:\\b%s\\b==>%s" % (re.escape(real), repl) for real, repl in pairs]
    out.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("wrote %d replace-text expressions -> %s" % (len(lines), out))


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--map", required=True, type=Path)
    ap.add_argument("--root", type=Path, default=Path("."))
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--apply", action="store_true", help="rewrite files in place")
    g.add_argument("--check", action="store_true",
                   help="report only; exit 1 if any real name is present")
    g.add_argument("--gen-filter-repo", type=Path, metavar="OUT",
                   help="emit a git-filter-repo --replace-text file and exit")
    args = ap.parse_args()

    pairs, keep = load_map(args.map)
    print("map: %d replacements, longest key %d chars" % (len(pairs), len(pairs[0][0])))

    if args.gen_filter_repo:
        gen_filter_repo(pairs, args.gen_filter_repo)
        return 0
    return sweep_tree(args.root, pairs, keep, apply_changes=args.apply)


if __name__ == "__main__":
    sys.exit(main())
