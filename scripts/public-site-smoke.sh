#!/usr/bin/env bash
#
# public-site-smoke.sh — assert how the public site is SERVED (FR-87, #1776).
#
#   scripts/public-site-smoke.sh <base-url>
#   scripts/public-site-smoke.sh http://localhost:8080      # the hosted-image smoke
#   scripts/public-site-smoke.sh https://roomler.ai         # production, after a roll
#
# WHY THIS EXISTS
#   The static site (docs, blog, the homepage) is where search engines meet
#   roomler.ai, and every defect FR-87 fixes was invisible from inside the build:
#     - `/docs` redirected to `http://` (a scheme downgrade);
#     - unknown paths answered 200 with the SPA shell (soft 404s);
#     - `/docs/x` and `/docs/x/` both answered 200 (duplicate URLs);
#     - a single `add_header` in an nginx location silently drops every
#       security header for that location — the one mistake that looks like a
#       harmless cache tweak in review.
#   This script checks the SERVED behaviour, so it runs against a real nginx:
#   the image in CI, production after every promote.
#
# Exit 0 = every check passed. Exit 1 = at least one failed (each is printed).
# Exit 2 = usage error.

set -u

BASE="${1:-}"
[ -n "$BASE" ] || { echo "usage: $0 <base-url>" >&2; exit 2; }
BASE="${BASE%/}"

FAIL=0
ok()   { printf '  \342\234\223 %s\n' "$1"; }
bad()  { printf '  \342\234\227 %s\n' "$1"; FAIL=1; }

# HEAD request → headers, lower-cased names, CR stripped. `-k` so a local
# self-signed front works; production is real TLS anyway.
headers() { curl -ksS -m 15 -I "$1" 2>/dev/null | tr -d '\r' | sed -E 's/^([A-Za-z0-9-]+):/\L\1:/'; }
status()  { curl -ksS -m 15 -o /dev/null -w '%{http_code}' "$1" 2>/dev/null; }
header()  { headers "$1" | grep -i "^$2:" | head -1 | sed -E 's/^[^:]+:[[:space:]]*//'; }

# The security headers every HTML page must carry exactly as `/` does. Sorted
# so the comparison is order-independent; values compared verbatim.
SEC='content-security-policy|strict-transport-security|x-frame-options|x-content-type-options|referrer-policy|permissions-policy|x-xss-protection'
secset() { headers "$1" | grep -Ei "^($SEC):" | sort; }

echo "public-site smoke against $BASE"

# 1. Relative redirects, one URL per page.
for path in /docs /docs/start; do
  code="$(status "$BASE$path")"
  loc="$(header "$BASE$path" location)"
  if [ "$code" = "301" ] && [ "${loc#/}" != "$loc" ] && [ "$loc" = "$path/" ]; then
    ok "$path -> 301 Location: $loc (relative, slash form)"
  else
    bad "$path -> $code Location: '${loc:-none}' (want 301 to '$path/', relative)"
  fi
done

# 2. Real 404s, never the SPA shell.
for path in /docs/fr87-smoke-missing/ /blog/fr87-smoke-missing/; do
  code="$(status "$BASE$path")"
  [ "$code" = "404" ] && ok "$path -> 404" || bad "$path -> $code (want 404; a 200 here is a soft 404)"
done

# 3. HTML is revalidated, so it is never the stale half of a hashed-asset pair.
cc="$(header "$BASE/docs/" cache-control)"
case "$cc" in
  *no-cache*) ok "/docs/ Cache-Control: $cc" ;;
  *) bad "/docs/ Cache-Control: '${cc:-none}' (want no-cache)" ;;
esac

# 4. Security headers identical to `/` on every static page family.
ROOT_SET="$(secset "$BASE/")"
if [ -z "$ROOT_SET" ]; then
  bad "/ carries none of the security headers — cannot compare"
else
  pages="/docs/ /docs/start/"
  [ "$(status "$BASE/blog/")" = "200" ] && pages="$pages /blog/"
  for path in $pages; do
    got="$(secset "$BASE$path")"
    if [ "$got" = "$ROOT_SET" ]; then
      ok "$path security headers == / ($(printf '%s\n' "$got" | wc -l | tr -d ' ') headers)"
    else
      bad "$path security headers differ from /:"
      diff <(printf '%s\n' "$ROOT_SET") <(printf '%s\n' "$got") | sed 's/^/      /'
    fi
  done
fi

# 5. The blog, when it exists: its feed is Atom. When it does not: a real 404.
blog="$(status "$BASE/blog/")"
if [ "$blog" = "200" ]; then
  ct="$(header "$BASE/blog/feed.xml" content-type)"
  case "$ct" in
    application/atom+xml*) ok "/blog/feed.xml Content-Type: $ct" ;;
    *) bad "/blog/feed.xml Content-Type: '${ct:-none}' (want application/atom+xml)" ;;
  esac
elif [ "$blog" = "404" ]; then
  ok "/blog/ -> 404 (no posts published yet; not a soft 404)"
else
  bad "/blog/ -> $blog (want 200 with posts, or 404 without)"
fi

if [ "$FAIL" = "0" ]; then echo "public-site smoke: all checks passed"; else echo "public-site smoke: FAILED"; fi
exit "$FAIL"
