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

With `--staged --map <map>` it also refuses the map's own names wherever the
commit ADDS one, on a machine that has the map. That is the pre-commit hook's
mode; see the map gate below.
"""

from __future__ import annotations

import argparse
import codecs
import os
import subprocess
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from sanitize import (SKIP_DIRS, SKIP_SUFFIXES, build_regex, load_map,  # noqa: E402
                      read_text, walk)

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
#
# ⚠️⚠️ The edges are B and E below, NEVER `\b`. `\b` counts `_` as part of a
# word, so a tag glued to an identifier -- a test named `fn <tag>_layout()`, a
# fixture `<tag>_dump.txt` -- has no word boundary on that side, and every
# shape here would walk straight past it. Measured 2026-10-21: one asset tag
# sat in a Rust test name for three months and then outlived two whole-history
# rewrites, because the sweep's rules were `\b`-bounded too. A letter or a
# digit continues a hostname; `_`, `-`, `.` and everything else end it.
B = r"(?<![A-Za-z0-9])"  # start: not glued to a letter or digit before it
E = r"(?![A-Za-z0-9])"   # end: not glued to a letter or digit after it
SHAPES = [
    (r"DESKTOP-(?=[A-Z0-9]{7}" + E + r")[A-Z]*[0-9][A-Z0-9]*" + E,
     "Windows auto-generated desktop hostname"),
    (r"LAPTOP-(?=[A-Z0-9]{7,8}" + E + r")[A-Z]*[0-9][A-Z0-9]*" + E,
     "Windows auto-generated laptop hostname"),
    (r"WIN-(?=[A-Z0-9]{11}" + E + r")[A-Z]*[0-9][A-Z0-9]*" + E,
     "Windows Server auto-generated hostname"),
    (B + r"PC[0-9]{4,6}" + E, "corp asset tag, PC-prefixed"),
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
    (B + r"(?![0-9A-Fa-f]{3}[0-9]{5,}" + E + r")[A-Z]{3}[0-9]{5,}" + E,
     "corp asset tag, three-letter prefix + 5+ digits"),
    (B + r"[A-Za-z]{3,}-XMG-[A-Za-z0-9]+" + E, "owner-name-prefixed laptop hostname"),
    (B + r"[A-Za-z]{3,}s-MacBook[A-Za-z0-9-]*", "Apple default '<owner>s-MacBook'"),
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
# entry was also redundant: the shapes are already anchored by B and E, so
# they cannot match inside a longer number.
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
    # IEC 61966-2-1 is the sRGB standard, and WebCodecs and FFmpeg spell its
    # transfer `iec61966-2-1` / `IEC61966_2_1` (FR-130, 2026-10-30): the same
    # spec-number family as ISO10646, so it is listed the same way.
    r"|IEC[0-9]+"
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

    A binary file comes back with content None rather than not at all: its
    NAME is still scanned (see scan()), and a screenshot named after the
    machine it shows is the commonest way a binary would carry one.
    """
    names = subprocess.run(
        ["git", "-C", str(root), "diff", "--cached", "--name-only",
         "--diff-filter=ACMR", "-z"],
        capture_output=True, check=True)
    for name in names.stdout.decode("utf-8", "replace").split("\0"):
        if not name:
            continue
        if Path(name).suffix.lower() in SKIP_SUFFIXES:
            yield name, None
            continue
        blob = subprocess.run(["git", "-C", str(root), "show", ":" + name],
                              capture_output=True)
        if blob.returncode == 0:
            yield name, blob.stdout.decode("utf-8", "replace")


def matches(text):
    """(offset, token, why) for every shape hit the allowlist does not exempt."""
    for shape, why in SHAPES:
        for m in re.finditer(shape, text, re.I):
            if not ALLOW.fullmatch(m.group(0)):
                yield m.start(), m.group(0), why


def scan(root: Path, staged=False):
    findings = []
    sources = (staged_files(root) if staged
               else ((p.relative_to(root).as_posix(),
                      None if p.suffix.lower() in SKIP_SUFFIXES else read_text(p))
                     for p in walk(root, include_binary=True)))
    for name, text in sources:
        # ⚠️ The PATH is a surface of its own. Content scanning never sees it,
        # and git-filter-repo's --replace-text never rewrites it -- so an e2e
        # spec NAMED after a corp laptop outlived two whole-history rewrites
        # (measured 2026-10-21), and this guard, reading content only, could
        # not have flagged it. A path hit reports `name` where a line number
        # would go.
        for _, tok, why in matches(name):
            findings.append((name, "name", tok, why))
        if text is None:
            continue
        for start, tok, why in matches(text):
            findings.append((name, text.count("\n", 0, start) + 1, tok, why))
    return findings


# ── The map gate (--staged --map) ────────────────────────────────────────────
#
# A name with no shape -- a personal laptop called after its owner -- passed
# every automatic layer until 2026-10-30: this guard, CI and the hook. Only the
# hand-run, map-driven sweep found one, after someone added it to the map, and
# the price was the fifth whole-history rewrite. So on a machine that HAS the
# map, the hook hands it over and the names the sweep would rewrite are refused
# at the commit. The map never enters the repo; CI has none and runs unchanged.
#
# ⚠️ ADDED lines and NEW paths only, never the whole staged blob. A name already
# in the tree is the sweep's to rewrite. Judging whole blobs would refuse every
# commit that merely touches such a file -- every file holding a name, from the
# moment the name joins the map until the sweep has run -- and a gate that
# refuses work nobody did is one people skip with --no-verify.
#
# ⚠️ The matcher is the sweep's own, imported: load_map(), build_regex(re.I) and
# the residual scan's two exemptions. A gate that disagreed with the sweep would
# refuse what it leaves alone, or pass what it rewrites.
#
# ⚠️⚠️ A mapped name is NEVER printed, only the alias the map gives it, and every
# path and shape token printed beside it goes through the map too. This output
# lands in terminals and agent transcripts, and from there in PR bodies and
# comments -- which is how names reached GitHub's edit history.
HUNK = re.compile(r"@@ -[0-9]+(?:,([0-9]+))? \+([0-9]+)(?:,([0-9]+))? @@")


class MapUnreadable(Exception):
    """load_map() refused the map."""


class MapGate:
    def __init__(self, path):
        try:
            self.pairs, self.keep = load_map(Path(path))
        except SystemExit:
            # load_map() refuses a malformed map with sys.exit(<message>). That
            # message quotes the offending line, which may BE a real name, and it
            # exits 1 -- the hook's "names found". Neither may get out.
            raise MapUnreadable() from None
        self.safe = {v.lower() for _, v in self.pairs}
        # An empty alternation matches the empty string at every offset.
        self.rx = build_regex(self.pairs, re.I) if self.pairs else None
        self.alias = {}
        for real, repl in self.pairs:
            self.alias.setdefault(real.lower(), repl)

    def _mapped(self, hit):
        return hit.lower() not in self.safe and hit not in self.keep

    def hits(self, text):
        """The alias of every mapped name in `text`."""
        if self.rx is None:
            return
        for m in self.rx.finditer(text):
            if self._mapped(m.group(0)):
                yield self.alias.get(m.group(0).lower(), "?")

    def redact(self, text):
        """`text` with every mapped name replaced by `<alias>`, fit to print."""
        if self.rx is None:
            return text
        return self.rx.sub(
            lambda m: ("<%s>" % self.alias.get(m.group(0).lower(), "?")
                       if self._mapped(m.group(0)) else m.group(0)), text)


def git_text(root, *args):
    return subprocess.run(
        ["git", "-C", str(root), "-c", "core.quotePath=false", *args],
        capture_output=True, check=True).stdout.decode("utf-8", "replace")


def unquote(path):
    """git C-quotes a path that holds a quote, a backslash or a control
    character, core.quotePath=false notwithstanding."""
    if len(path) >= 2 and path[0] == path[-1] == '"':
        raw = codecs.escape_decode(path[1:-1].encode("utf-8"))[0]
        return raw.decode("utf-8", "replace")
    return path


def staged_additions(root):
    """(path, line-or-"name", text) for what the commit ADDS: the name of each
    new path, and each added line of each text file, numbered as committed."""
    for name in git_text(root, "diff", "--cached", "--name-only",
                         "--diff-filter=ACR", "-z").split("\0"):
        if name:
            yield name, "name", name
    diff = git_text(root, "diff", "--cached", "-U0", "--no-color", "--no-ext-diff",
                    "--no-textconv", "--diff-filter=ACMR",
                    "--src-prefix=a/", "--dst-prefix=b/")
    path, line_no, old_left, new_left = None, 0, 0, 0
    for line in diff.split("\n"):
        if old_left > 0 or new_left > 0:
            # Inside a hunk the counts decide what a line is. An added line can
            # itself begin `++ ` or `@@ ` -- a diff quoted in a doc -- and read as
            # a header it would move every later finding to the wrong file.
            if line.startswith("+"):
                if path is not None:
                    yield path, line_no, line[1:]
                line_no += 1
                new_left -= 1
            elif line.startswith("-"):
                old_left -= 1
            continue
        if line.startswith("diff --git "):
            path = None
        elif line.startswith("+++ "):
            dst = unquote(line[4:])
            path = dst[2:] if dst.startswith("b/") else None
            if path is not None and Path(path).suffix.lower() in SKIP_SUFFIXES:
                path = None   # its NAME was judged above, as scan() does
        else:
            m = HUNK.match(line)
            if m:
                old_left = int(m.group(1)) if m.group(1) is not None else 1
                line_no = int(m.group(2))
                new_left = int(m.group(3)) if m.group(3) is not None else 1


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", type=Path, default=Path("."))
    ap.add_argument("--staged", action="store_true",
                    help="check the staged blobs only (for a pre-commit hook)")
    ap.add_argument("--map", type=Path, metavar="MAP",
                    help="with --staged: also refuse every name in this sanitize "
                         "map that the commit adds (printed as its alias only)")
    args = ap.parse_args()
    if args.map is not None and not args.staged:
        ap.error("--map needs --staged")

    gate = None
    if args.map is not None:
        try:
            gate = MapGate(args.map)
        except MapUnreadable:
            print("machine-name guard could not run: the sanitize map %s is "
                  "malformed; `sanitize.py --map <map> --check` names the line"
                  % args.map, file=sys.stderr)
            return EXIT_ERROR

    findings = scan(args.root, staged=args.staged)
    mapped = []
    if gate is not None:
        for path, where, text in staged_additions(args.root):
            for alias in gate.hits(text):
                mapped.append((gate.redact(path), where, alias))
    if not findings and not mapped:
        print("machine-name shapes: none found")
        if gate is not None:
            print("sanitize map: no mapped name added")
        return EXIT_CLEAN

    show = gate.redact if gate is not None else (lambda s: s)
    if findings:
        seen = set()
        print("Real machine names look like they are committed here:\n")
        for path, line, tok, why in findings:
            print("  %s:%s  %s" % (show(path), line, show(tok)))
            if show(tok) not in seen:
                seen.add(show(tok))
        print("\n%d occurrence(s), %d distinct name(s): %s"
              % (len(findings), len(seen), ", ".join(sorted(seen))))
        print("""
Fix: add each real name to the sanitisation map -- which lives OUTSIDE this
repo, because it is the one place a real name and its replacement sit side by
side -- then re-run the sweep:

    python3 .claude/skills/sanitize-hostnames/sanitize.py \\
        --map <path-to-map> --root . --apply

Do NOT add a real machine name to this file's allowlist. See SKILL.md.""")
    if mapped:
        aliases = sorted({alias for _, _, alias in mapped})
        print("%sNames from the sanitize map are ADDED here, each shown by its "
              "alias:\n" % ("\n" if findings else ""))
        for path, where, alias in mapped:
            print("  %s:%s  -> %s" % (path, where, alias))
        print("\n%d addition(s), %d mapped name(s): %s"
              % (len(mapped), len(aliases), ", ".join(aliases)))
        print("""
Fix: write the alias in place of the name, then re-stage. A name that lands in
a commit costs a whole-history rewrite, and GitHub keeps it reachable through
refs/pull and in edit history until Support purges it. A spelling that is not
the machine at all gets a `!keep` line in the map, never --no-verify.""")
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
