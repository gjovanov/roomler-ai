#!/usr/bin/env python3
"""Fail if a token SHAPED like a real machine name is in the tree.

This is the half of the sweep that does not need the map -- and therefore the
half that can run in CI on a public repo, where the map must never go.

Why it exists: a map can only find what someone already thought to list. The
2026-08-28 sweep listed four names and passed. The 2026-08-30 whole-history
sweep started from that same map and, having applied it everywhere, still found
two more machines that had never been on anyone's list -- both Windows
auto-generated `DESKTOP-<7 alnum>` hostnames, one of which had been sitting in
a published FR spec since the PR that created it. Neither was findable by name.
Both were trivially findable by shape.

So the map catches the machines you know about, and this catches the class.
When it fires, the fix is: add the real name to the map (which lives outside
the repo), re-run the sweep, and let this go green -- never add the real name
to the allowlist below.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from sanitize import SKIP_DIRS, SKIP_SUFFIXES, read_text, walk  # noqa: E402

# Exit codes. These are a CONTRACT with .githooks/pre-commit, which blocks a
# commit on EXIT_FOUND and on nothing else.
#
# ⚠️⚠️ The reason they exist: until 2026-09-04 "a real name is staged" and "the
# guard could not run" were the SAME exit code, so the hook could not tell them
# apart and reported every failure as `COMMIT REFUSED: this change carries what
# looks like a real machine name`. Measured that day: a file containing nothing
# but ordinary prose was refused, because the hook passed `--staged` to a
# checkout of this script from before that flag existed and argparse exited
# non-zero. A guard that says "I found a hostname" when it means "I crashed"
# teaches people to reach for --no-verify, which removes the layer entirely.
#
# ⚠️ EXIT_ERROR is deliberately NOT 1 and NOT 2: an unhandled Python exception
# exits 1 and argparse exits 2, so a distinct value is the only way the caller
# can be sure the run actually completed. main() catches everything for the
# same reason -- see the wrapper at the bottom of this file.
EXIT_CLEAN = 0
EXIT_FOUND = 1
EXIT_ERROR = 20

# Each shape, with why a match identifies a physical machine.
#
# ⚠️⚠️ MATCHED CASE-INSENSITIVELY (re.I, applied in scan()). This is the single
# most important line in the file. The shapes were originally written
# uppercase-only, which is how a leak got through on 2026-09-04: a field log
# wrote the same two asset tags in LOWERCASE, the guard reported "none found",
# CI went green, and the names reached a public repo and 15 GitHub items.
#
# That is the identical mistake the map made on 2026-08-28 -- listing only
# uppercase spellings, passing, and leaving 60 real tags behind -- and it was
# already written down in this very directory as the reason the residual scan
# is case-insensitive. It got rebuilt anyway, one file over. People write a
# hostname however it came out of their terminal; a guard that assumes a casing
# is not a guard.
#
# ⚠️ The DESKTOP-/LAPTOP-/WIN- suffixes require AT LEAST ONE DIGIT. Windows
# generates them from an alphanumeric charset, so a real one essentially always
# carries a digit; an English word never does. Without that constraint,
# case-insensitive matching flags `desktop-classic`, `desktop-content` and
# `desktop-rebound` -- all real strings in this repo -- and a guard that cries
# wolf on ordinary prose is a guard someone deletes. The map-based sweep is
# what covers the ~10% of generated names that happen to be all letters; this
# guard is for the class, not for completeness.
SHAPES = [
    (r"DESKTOP-(?=[A-Z0-9]{7}\b)[A-Z]*[0-9][A-Z0-9]*\b",
     "Windows auto-generated desktop hostname"),
    (r"LAPTOP-(?=[A-Z0-9]{7,8}\b)[A-Z]*[0-9][A-Z0-9]*\b",
     "Windows auto-generated laptop hostname"),
    (r"WIN-(?=[A-Z0-9]{11}\b)[A-Z]*[0-9][A-Z0-9]*\b",
     "Windows Server auto-generated hostname"),
    (r"\bPC[0-9]{4,6}\b", "corp asset tag, PC-prefixed"),
    # Generalised from a single hard-coded prefix on 2026-09-05. A corp DNS
    # server surfaced in `nslookup` output during a field test and NOTHING
    # caught it: not the map (unlisted) and not this file, whose only
    # three-letter rule named one specific prefix. An arbitrary corp naming
    # scheme has no prefix we can know in advance, so the shape has to be the
    # shape.
    #
    # ⚠️ The lookahead drops tokens whose letters are ALL hex digits, because
    # those are GUID/UUID fragments, not hostnames. Measured over this repo's
    # entire history — 16,162 blobs, every commit message, the whole master
    # tree — the generalised form matched exactly four token families, and
    # three were hex: `{12345678-1234-5678-9ABC-DEF012345678}` (an MSI GUID
    # example), the tail of a codecov UUID, and a font's ISO charset number.
    # A hex run is hex by construction; a hostname is not.
    #
    # ⚠️ The cost is a tag whose three letters happen to all fall in a-f
    # (~1.2% of prefixes) — those fall to the map, exactly as the ~10% of
    # all-letter Windows names above do. This guard is for the class.
    (r"\b(?![0-9A-Fa-f]{3}[0-9]{5,}\b)[A-Z]{3}[0-9]{5,}\b",
     "corp asset tag, three-letter prefix + 5+ digits"),
    (r"\b[A-Za-z]{3,}-XMG-[A-Za-z0-9]+\b", "owner-name-prefixed laptop hostname"),
    (r"\b[A-Za-z]{3,}s-MacBook[A-Za-z0-9-]*", "Apple default '<owner>s-MacBook'"),
]

# Tokens that match a shape and are NOT machine names. Every entry is a
# non-machine string that happens to fit -- a pixel format, a spec number, a
# colour. A REAL machine name must never be added here; it belongs in the map.
#
# ⚠️ Anchored with fullmatch, never a prefix match. An earlier version used
# `re.match` plus an entry `PC[0-9]{4,6}(?=[0-9])`, meant to skip a longer
# numeric run -- but every real PC-prefixed asset tag here is PC + 5 digits, so
# that entry matched the tags themselves and the guard silently ignored the
# whole class it exists to catch. A planted canary is what exposed it. The
# entry was also redundant: the shapes are already \b-anchored, so they cannot
# match inside a longer number.
#
# ⚠️ This file and sanitize.py are themselves SCANNED, so neither may name a
# real machine even as an example -- the guard caught sanitize.py's own
# docstring doing exactly that. Keep the examples generic. (`DESKTOP-WINHOST`
# below is an alias fragment, not a machine: the shape stops at 7 characters,
# so a qualified alias arrives here with its trailing `-<letter>` clipped off.)
#
# ⚠️ Also case-insensitive, so it keeps exempting an alias written in prose as
# `corplap-3` rather than only `CORPLAP-3` -- otherwise making SHAPES
# case-insensitive would turn every lowercase alias into a false positive and
# the guard would be switched off within a week.
ALLOW = re.compile(
    r"WINHOST-[A-Z]|CORPLAP-[0-9]|DESKTOP-WINHOST|MacBook-1"  # our replacements
    r"|ARGB2101010|XRGB8888|RGBA[0-9]+"                       # pixel formats
    # Spec numbers that fit the three-letter shape. ISO10646 is the Unicode
    # charset named in an X11 font string; it is the ONLY non-hex false
    # positive the generalised shape produced across the whole history, so it
    # is listed rather than weakening the shape.
    r"|ISO[0-9]+"                                             # spec numbers
    # A RETIRED canary, not a machine -- it never named anything. Writing up
    # this guard published it into a GitHub issue, so step 2b's scan of
    # PUBLISHED content reported it on every run, and a standing "known
    # benign" hit is how a check earns the noise that gets it deleted.
    #
    # ⚠️ It was rotated OUT of selftest.sh's CAUGHT list in the same commit,
    # and into its IGNORED list. Both halves are load-bearing: allowlisting a
    # value that is still asserted as CAUGHT makes the canary vacuous, and the
    # selftest fails loudly if anyone tries (`MISSED:`, both casings). Listing
    # it as IGNORED is what turns this line from an assumption into an
    # assertion. Remove the two together or not at all.
    r"|ZQX55555",                                             # retired canary
    re.I,
)


def staged_files(root):
    """(path, content) for everything about to be committed.

    A pre-commit hook must judge the STAGED blob, not the file on disk: those
    differ under `git add -p`, and the working-tree copy can be clean while the
    staged one is not. Reads content out of the index with `git show :<path>`.
    """
    names = subprocess.run(
        ["git", "-C", str(root), "diff", "--cached", "--name-only",
         "--diff-filter=ACMR", "-z"],
        capture_output=True, check=True)
    for name in names.stdout.decode("utf-8", "replace").split("\0"):
        if not name or Path(name).suffix.lower() in SKIP_SUFFIXES:
            continue
        blob = subprocess.run(["git", "-C", str(root), "show", ":" + name],
                              capture_output=True)
        if blob.returncode == 0:
            yield name, blob.stdout.decode("utf-8", "replace")


def scan(root: Path, staged=False):
    findings = []
    sources = (staged_files(root) if staged
               else ((str(p.relative_to(root).as_posix()), read_text(p)) for p in walk(root)))
    for name, text in sources:
        p = Path(name)
        if text is None:
            continue
        for shape, why in SHAPES:
            for m in re.finditer(shape, text, re.I):
                tok = m.group(0)
                if ALLOW.fullmatch(tok):
                    continue
                line = text.count("\n", 0, m.start()) + 1
                findings.append((p.as_posix(), line, tok, why))
    return findings


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", type=Path, default=Path("."))
    ap.add_argument("--staged", action="store_true",
                    help="check the staged blobs only (for a pre-commit hook)")
    args = ap.parse_args()

    findings = scan(args.root, staged=args.staged)
    if not findings:
        print("machine-name shapes: none found")
        return EXIT_CLEAN

    seen = set()
    print("Real machine names look like they are committed here:\n")
    for path, line, tok, why in findings:
        print("  %s:%s  %s" % (path, line, tok))
        if tok not in seen:
            seen.add(tok)
    print("\n%d occurrence(s), %d distinct name(s): %s"
          % (len(findings), len(seen), ", ".join(sorted(seen))))
    print("""
Fix: add each real name to the sanitisation map -- which lives OUTSIDE this
repo, because it is the one place a real name and its replacement sit side by
side -- then re-run the sweep:

    python3 .claude/skills/sanitize-hostnames/sanitize.py \\
        --map <path-to-map> --root . --apply

Do NOT add a real machine name to this file's allowlist. See SKILL.md.""")
    return EXIT_FOUND


if __name__ == "__main__":
    # Any failure that is NOT "a name was found" must leave through EXIT_ERROR,
    # so the hook can tell a verdict from a crash. Without this a git failure
    # inside staged_files() (`subprocess.run(..., check=True)`) would surface as
    # a bare exit 1 -- indistinguishable from a real finding, and reported to
    # the author as a hostname they did not write.
    #
    # SystemExit passes through untouched: it carries main()'s own return value
    # and argparse's exit 2, both of which already mean something specific.
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except KeyboardInterrupt:
        sys.exit(EXIT_ERROR)
    except BaseException as exc:  # noqa: BLE001 - deliberate: see above
        print("machine-name guard could not run: %s: %s"
              % (type(exc).__name__, exc), file=sys.stderr)
        sys.exit(EXIT_ERROR)
