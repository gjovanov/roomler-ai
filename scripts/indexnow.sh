#!/usr/bin/env bash
# FR-87 (#1776) P7 — IndexNow: tell the search engines that share
# api.indexnow.org (Bing, Yandex, Seznam, Naver …; Google does not take part —
# Search Console covers it) which public pages a roll changed, so they recrawl
# within minutes instead of whenever they next pass by.
#
#   scripts/indexnow.sh snapshot <base-url> <out.tsv>
#       loc<TAB>lastmod for every URL in every child of <base-url>/sitemap.xml
#   scripts/indexnow.sh changed <before.tsv> <after.tsv>
#       the URLs that are new in <after>, or carry a different lastmod
#   scripts/indexnow.sh submit <base-url> <key>   (URLs on stdin, one per line)
#       POST them; 200 or 202 means accepted
#
# `changed` diffs the SERVED sitemaps, before the bump and after the roll, so
# the answer is exactly what the site now claims changed: its lastmods come
# from git (FR-87 P2), never from the build clock, so a theme-only release
# re-dates nothing and submits nothing. An undated URL (the homepage, the
# generated listings) is submitted when it first appears, and not after.
#
# The key is public by design: an engine verifies a submission by fetching
# <base-url>/<key>.txt, which must contain exactly the key (`ui/public/`).
# Every failure is a WARNING: a missed hint to a crawler never fails a roll.
set -uo pipefail

warn() { echo "::warning title=IndexNow::$*"; }

snapshot() {
  local base="${1%/}" out="$2" index children child
  index="$(curl -fsS -m 20 "$base/sitemap.xml")" || { warn "could not fetch $base/sitemap.xml"; return 1; }
  children="$(printf '%s\n' "$index" | grep -oE '<loc>[^<]+</loc>' | sed -E 's#</?loc>##g')"
  [ -n "$children" ] || { warn "$base/sitemap.xml names no child sitemaps"; return 1; }
  : > "$out"
  for child in $children; do
    # The index names its children by absolute URL; read them from THIS
    # server (their path under <base-url>), so a snapshot is of what it serves.
    child="$base/${child#*://*/}"
    curl -fsS -m 20 "$child" | awk '
      /<url>/     { loc = ""; mod = "" }
      /<loc>/     { loc = $0; sub(/.*<loc>/, "", loc); sub(/<\/loc>.*/, "", loc) }
      /<lastmod>/ { mod = $0; sub(/.*<lastmod>/, "", mod); sub(/<\/lastmod>.*/, "", mod) }
      /<\/url>/   { if (loc != "") printf "%s\t%s\n", loc, mod }
    ' >> "$out" || { warn "could not fetch $child"; return 1; }
  done
  echo "snapshot: $(wc -l < "$out" | tr -d ' ') URLs from $(printf '%s\n' "$children" | wc -l | tr -d ' ') sitemap(s)" >&2
}

changed() {
  local before="$1" after="$2"
  # No baseline means no way to tell what changed: submit nothing rather
  # than everything.
  if [ ! -s "$before" ]; then warn "no before-snapshot ($before) — nothing submitted"; return 0; fi
  [ -s "$after" ] || { warn "no after-snapshot ($after) — nothing submitted"; return 0; }
  awk -F'\t' 'NR == FNR { seen[$0] = 1; next } !($0 in seen) { print $1 }' "$before" "$after"
}

submit() {
  local base="${1%/}" key="$2" host urls n body code served
  host="${base#*://}"; host="${host%%/*}"
  # Only this host's URLs: IndexNow answers 422 to a list with any other.
  urls="$(grep -E "^${base}/" | sort -u)"
  n="$(printf '%s' "$urls" | grep -c .)"
  if [ "$n" -eq 0 ]; then echo "IndexNow: nothing changed, nothing submitted"; return 0; fi
  served="$(curl -fsS -m 15 "$base/$key.txt" | tr -d '\r\n')" || true
  if [ "$served" != "$key" ]; then
    warn "$base/$key.txt does not serve the key (got '${served:0:40}') — engines would reject the submission; nothing submitted"
    return 1
  fi
  body="$(printf '%s\n' "$urls" | awk -v host="$host" -v key="$key" -v loc="$base/$key.txt" '
    { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); list = list (NR > 1 ? "," : "") "\"" $0 "\"" }
    END { printf "{\"host\":\"%s\",\"key\":\"%s\",\"keyLocation\":\"%s\",\"urlList\":[%s]}", host, key, loc, list }')"
  code="$(curl -sS -m 30 -o /dev/null -w '%{http_code}' -X POST \
    -H 'Content-Type: application/json; charset=utf-8' \
    --data-binary "$body" "${INDEXNOW_ENDPOINT:-https://api.indexnow.org/indexnow}")" || code=000
  case "$code" in
    200|202) echo "IndexNow: $n URL(s) accepted (HTTP $code)"; printf '%s\n' "$urls" | sed 's/^/  /' ;;
    403) warn "HTTP 403: the key is not valid for $host"; return 1 ;;
    422) warn "HTTP 422: a URL is not on $host, or the key does not match its file"; return 1 ;;
    429) warn "HTTP 429: too many requests; the next roll submits again"; return 1 ;;
    *)   warn "HTTP $code from the IndexNow endpoint; nothing confirmed"; return 1 ;;
  esac
}

cmd="${1:-}"; shift || true
case "$cmd" in
  snapshot) [ $# -eq 2 ] || { echo "usage: $0 snapshot <base-url> <out.tsv>" >&2; exit 2; }; snapshot "$@" ;;
  changed)  [ $# -eq 2 ] || { echo "usage: $0 changed <before.tsv> <after.tsv>" >&2; exit 2; }; changed "$@" ;;
  submit)   [ $# -eq 2 ] || { echo "usage: $0 submit <base-url> <key> < urls" >&2; exit 2; }; submit "$@" ;;
  *) echo "usage: $0 snapshot|changed|submit …" >&2; exit 2 ;;
esac
