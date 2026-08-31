#!/usr/bin/env bash
# FR-41 (#965) — record the 90-second product demo.
#
# Sibling of record-video.sh, which records the *collaboration* walkthrough
# (roomler-intro.mp4) against a LOCAL dev stack. This one records the two lead
# pillars — a real machine's desktop in a browser tab, and the network around
# it — against whatever server you point it at, usually production.
#
# What you need before running:
#   - a demo organization with ONE device enrolled and ONLINE
#   - an account that can sign in to it
#   - bun (the spec is collected by node too, but bun is what CI uses here)
#   - ffmpeg, for the MP4 + GIF conversion
#
# Usage:
#   ROOMLER_DEMO_USER=…    ROOMLER_DEMO_PASS=… \
#   ROOMLER_DEMO_TENANT=…  ROOMLER_DEMO_AGENT=… \
#   ./scripts/record-demo.sh
#
# Output:
#   ui/e2e/video/output/roomler-demo.webm   (raw take)
#   ui/e2e/video/output/roomler-demo.mp4    (for linking)
#   ui/e2e/video/output/roomler-demo.gif    (for the README — auto-plays)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
UI_DIR="$PROJECT_DIR/ui"
OUT="$UI_DIR/e2e/video/output"

BASE_URL="${ROOMLER_DEMO_URL:-https://roomler.ai}"

# Credentials may come from the environment or from a 0600 file, so a re-record
# does not need them re-typed. The file is never echoed by this script.
for envfile in "${ROOMLER_DEMO_ENV:-}" "$HOME/.roomler-demo.env" ./.roomler-demo.env; do
  if [ -n "$envfile" ] && [ -f "$envfile" ]; then
    set -a; . "$envfile"; set +a
    echo "credentials: loaded from $envfile"
    break
  fi
done

: "${ROOMLER_DEMO_USER:?set ROOMLER_DEMO_USER, or put it in ~/.roomler-demo.env}"
: "${ROOMLER_DEMO_PASS:?set ROOMLER_DEMO_PASS, or put it in ~/.roomler-demo.env}"
: "${ROOMLER_DEMO_TENANT:?set ROOMLER_DEMO_TENANT — the demo org id}"

# ⚠️ The toolchain is split on a Windows + WSL box: Playwright needs node/bun
# (Windows side), ffmpeg is in WSL. Rather than demand both on one side, fall
# back to WSL's ffmpeg over /mnt/c when the native one is absent.
FFMPEG="ffmpeg"; FFPROBE="ffprobe"
if ! command -v ffmpeg >/dev/null && command -v wsl >/dev/null; then
  if wsl -e bash -lc 'command -v ffmpeg' >/dev/null 2>&1; then
    # ⚠️ Two path traps here, and the first take hit both. Git Bash reports
    # paths as `/c/dev/...`, not `C:/dev/...`, so a drive-letter test never
    # matches; and MSYS then MANGLES a `/c/...` argument into `C:/...` on its
    # way to wsl.exe, which ffmpeg reads as an unknown URL protocol
    # ("Protocol not found. Did you mean file:C:/..."). So: translate
    # `/c/` → `/mnt/c/` ourselves, and set MSYS_NO_PATHCONV to stop the mangling.
    winpath() {
      case "$1" in
        /[A-Za-z]/*)   printf '/mnt%s' "$1" ;;
        [A-Za-z]:[/\\]*) printf '/mnt/%s' "$(printf '%s' "$1" | sed 's|^\(.\):|\L\1|; s|\\|/|g')" ;;
        *)             printf '%s' "$1" ;;
      esac
    }
    FFMPEG="wsl_ffmpeg"; FFPROBE="wsl_ffprobe"
    wsl_ffmpeg()  { local a=(); for x in "$@"; do a+=("$(winpath "$x")"); done; MSYS_NO_PATHCONV=1 wsl -e ffmpeg  "${a[@]}"; }
    wsl_ffprobe() { local a=(); for x in "$@"; do a+=("$(winpath "$x")"); done; MSYS_NO_PATHCONV=1 wsl -e ffprobe "${a[@]}"; }
    echo "ffmpeg: using WSL's (none on PATH here)"
  fi
fi

mkdir -p "$OUT"

echo "=== Roomler demo recording ==="
echo "server : $BASE_URL"
echo "org    : $ROOMLER_DEMO_TENANT"
echo "device : ${ROOMLER_DEMO_AGENT:-<first Connect button on the page>}"
echo ""

# ⚠️ The device MUST be online before recording. A take whose remote-desktop
# scene shows a spinner is a wasted run, and the spec deliberately fails rather
# than publish one — so check here, where it costs nothing.
echo "[1/4] Checking the server answers…"
if ! curl -fsS -o /dev/null "$BASE_URL/health"; then
  echo "ERROR: $BASE_URL/health did not answer. Is the server up, and the URL right?"
  exit 1
fi

echo "[2/4] Recording…"
cd "$UI_DIR"
E2E_BASE_URL="$BASE_URL" \
E2E_USERNAME="$ROOMLER_DEMO_USER" \
E2E_PASSWORD="$ROOMLER_DEMO_PASS" \
E2E_TENANT_ID="$ROOMLER_DEMO_TENANT" \
E2E_AGENT_ID="${ROOMLER_DEMO_AGENT:-}" \
E2E_AGENT_NAME="${ROOMLER_DEMO_AGENT_NAME:-}" \
  bunx playwright test e2e/video/record-demo.spec.ts \
    --config=playwright.video.config.ts --reporter=list
RC=$?

# Playwright writes video.webm under a per-test directory; find the newest.
WEBM="$(find "$OUT" -name '*.webm' -newermt '-30 minutes' -print0 2>/dev/null | xargs -0 ls -t 2>/dev/null | head -1)"
if [ -z "$WEBM" ]; then
  echo "ERROR: no .webm produced. Playwright exited $RC — read the scene log above."
  exit 1
fi
cp "$WEBM" "$OUT/roomler-demo.webm"
echo "      raw take: $OUT/roomler-demo.webm ($(du -h "$OUT/roomler-demo.webm" | cut -f1))"

if ! command -v "$FFMPEG" >/dev/null && [ "$FFMPEG" = "ffmpeg" ]; then
  echo "ffmpeg not found — stopping at the WebM. Install it for the MP4 + GIF."
  exit 0
fi

DURATION=$("$FFPROBE" -v error -show_entries format=duration -of csv=p=0 "$OUT/roomler-demo.webm" 2>/dev/null | cut -d. -f1)
echo "[3/4] Take is ${DURATION}s"
# ⚠️ 90 s is an acceptance criterion, not a preference: it is the length past
# which a launch-post embed stops being watched to the end.
if [ -n "$DURATION" ] && [ "$DURATION" -gt 90 ]; then
  echo "      ⚠️  OVER 90s — trim a scene or shorten the caption holds in the spec."
fi

echo "[4/4] Converting…"
"$FFMPEG" -y -loglevel error -i "$OUT/roomler-demo.webm" \
  -c:v libx264 -crf 20 -preset slow -pix_fmt yuv420p -an \
  "$OUT/roomler-demo.mp4"

# A README GIF auto-plays where an MP4 does not, which is the whole reason to
# make one. 12 fps at 960px keeps it under ~8 MB for a 90 s take; a palette
# pass is what stops screen content turning to mud.
"$FFMPEG" -y -loglevel error -i "$OUT/roomler-demo.webm" \
  -vf "fps=12,scale=960:-1:flags=lanczos,palettegen=stats_mode=diff" "$OUT/.palette.png"
"$FFMPEG" -y -loglevel error -i "$OUT/roomler-demo.webm" -i "$OUT/.palette.png" \
  -lavfi "fps=12,scale=960:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=3" \
  "$OUT/roomler-demo.gif"
rm -f "$OUT/.palette.png"

echo ""
echo "Done:"
ls -lh "$OUT"/roomler-demo.* | awk '{printf "  %-42s %s\n", $9, $5}'
echo ""
echo "Next: watch it. If a scene was skipped the log above says so, and the"
echo "take will be short — that is a selector problem, not a shorter video."
