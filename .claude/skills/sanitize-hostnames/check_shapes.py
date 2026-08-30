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
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from sanitize import SKIP_DIRS, SKIP_SUFFIXES, read_text, walk  # noqa: E402

# Each shape, with why a match identifies a physical machine.
SHAPES = [
    (r"DESKTOP-[A-Z0-9]{7}\b", "Windows auto-generated desktop hostname"),
    (r"LAPTOP-[A-Z0-9]{7,8}\b", "Windows auto-generated laptop hostname"),
    (r"WIN-[A-Z0-9]{11}\b", "Windows Server auto-generated hostname"),
    (r"\bPC[0-9]{4,6}\b", "corp asset tag, PC-prefixed"),
    (r"\bCLK[0-9]{5,}\b", "corp asset tag, three-letter prefix + 5-8 digits"),
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
ALLOW = re.compile(
    r"WINHOST-[A-Z]|CORPLAP-[0-9]|DESKTOP-WINHOST|MacBook-1"  # our replacements
    r"|ARGB2101010|XRGB8888|RGBA[0-9]+"                       # pixel formats
)


def scan(root: Path):
    findings = []
    for p in walk(root):
        text = read_text(p)
        if text is None:
            continue
        for shape, why in SHAPES:
            for m in re.finditer(shape, text):
                tok = m.group(0)
                if ALLOW.fullmatch(tok):
                    continue
                line = text.count("\n", 0, m.start()) + 1
                findings.append((p.relative_to(root).as_posix(), line, tok, why))
    return findings


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", type=Path, default=Path("."))
    args = ap.parse_args()

    findings = scan(args.root)
    if not findings:
        print("machine-name shapes: none found")
        return 0

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
    return 1


if __name__ == "__main__":
    sys.exit(main())
