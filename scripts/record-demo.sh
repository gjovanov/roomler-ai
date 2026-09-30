#!/usr/bin/env bash
# The README demo: real machines, one browser tab, each desktop full screen.
#
# Records against whatever server you point it at, usually production: for each
# device in ROOMLER_DEMO_DEVICES it opens the device's remote page with the
# sidebar collapsed, presses Connect, and enters the viewer's fullscreen. Then
# ui/e2e/video/cut-demo.ts cuts the take into the README MP4 and GIF.
#
# What you need before running:
#   - an account that can sign in, and the org (tenant) id the devices are in
#   - each device ONLINE. ROOMLER_DEMO_DEVICES finds it by display name, and
#     ROOMLER_DEMO_LABELS sets the name the video SHOWS: a display name says
#     whose machine it is, so give every device a neutral label
#   - bun, and ffmpeg on PATH (or in WSL)
#
# Usage:
#   ROOMLER_DEMO_USER=…  ROOMLER_DEMO_PASS=…  ROOMLER_DEMO_TENANT=<org id> \
#   ROOMLER_DEMO_DEVICES="office-mac,laptop-17,laptop-42" \
#   ROOMLER_DEMO_LABELS="MacBook|Windows laptop|Second laptop" \
#   ROOMLER_DEMO_CAPTIONS="A MacBook — in a browser tab|…|…" \
#   ROOMLER_DEMO_BLUR='[{"device":0,"phase":"full","rect":[136,46,400,394]}]' \
#   ./scripts/record-demo.sh
#
# ROOMLER_DEMO_BLUR (optional) blurs regions of a device's desktop, per layout:
# see cut-demo.ts. Measure the rectangles on the take's own frames.
#
# Output, in the take folder (default ~/Videos/Roomler/demo/<date-time>):
#   frames/ + take.json    the raw take: every composited frame, and the marks
#   roomler-demo.mp4       1080p, for linking            → ./roomler-demo.mp4
#   demo-preview.gif       1280 px, auto-plays in README → docs/assets/demo-preview.gif
#   demo-preview-blog.gif  560 px, under the docs build's 3 MiB limit
#                                                        → ui/blog/assets/demo-preview.gif
#
# ⚠️ The screens are filmed AS THEY ARE, and a machine that is not yours to
# dress can say more than its name. Found on 2026-09-30:
#   - a managed laptop's lock screen printed its hostname, its addresses and
#     where it sits, on every frame (dropped until it was signed in);
#   - an unlocked Mac showed the operator's own dashboard, naming the whole
#     fleet (its window was minimized before the take), then a weather widget
#     naming a place and the host's "being viewed" banner naming the org (both
#     blurred);
#   - a laptop's system tray showed its corporate VPN client's icon (blurred:
#     a product name is a clue to whose network it is).
# A small region such as a tray icon is not text, so no OCR can confirm its
# blur: compare crops of the blurred and the unblurred cut, by eye, in both
# layouts and across the fullscreen switch.
# The take never visits the device list, the dashboard or the network pages,
# but only looking at the frames tells you what the DESKTOPS show. Watch the
# MP4 end to end before publishing either file.
#
# ⚠️ Write the captions from THIS take's frames. The path a machine takes is
# measured, not assumed: a laptop that went through a relay in one take went
# direct an hour later, once its VPN was off. The viewer's stats pills in the
# page frames say which.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
UI_DIR="$PROJECT_DIR/ui"

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
: "${ROOMLER_DEMO_TENANT:?set ROOMLER_DEMO_TENANT — the org id the devices are in}"
: "${ROOMLER_DEMO_DEVICES:?set ROOMLER_DEMO_DEVICES — display names, comma-separated, in filming order}"

# ⚠️ OUTSIDE ui/e2e/video/output: Playwright empties its outputDir at the start
# of every run, so a take kept there is lost to the next run of anything.
TAKE="${ROOMLER_DEMO_TAKE:-$HOME/Videos/Roomler/demo/$(date +%Y-%m-%d-%H%M%S)}"
mkdir -p "$TAKE"

echo "=== Roomler demo recording ==="
echo "server  : $BASE_URL"
echo "org     : $ROOMLER_DEMO_TENANT"
echo "devices : $ROOMLER_DEMO_DEVICES"
echo "take    : $TAKE"
echo ""

echo "[1/3] Checking the server answers…"
if ! curl -fsS -o /dev/null "$BASE_URL/health"; then
  echo "ERROR: $BASE_URL/health did not answer. Is the server up, and the URL right?"
  exit 1
fi

echo "[2/3] Recording…"
cd "$UI_DIR" || exit 1
# The spec refuses an unknown or offline device before it films anything.
E2E_BASE_URL="$BASE_URL" \
E2E_USERNAME="$ROOMLER_DEMO_USER" \
E2E_PASSWORD="$ROOMLER_DEMO_PASS" \
E2E_TENANT_ID="$ROOMLER_DEMO_TENANT" \
E2E_DEMO_DEVICES="$ROOMLER_DEMO_DEVICES" \
E2E_DEMO_LABELS="${ROOMLER_DEMO_LABELS:-}" \
E2E_DEMO_OUT="$TAKE" \
  bunx playwright test e2e/video/record-demo.spec.ts \
    --config=playwright.video.config.ts --reporter=list
RC=$?
if [ $RC -ne 0 ] || [ ! -f "$TAKE/take.json" ]; then
  echo "ERROR: the take did not finish (Playwright exited $RC) — read the log above."
  exit 1
fi

echo "[3/3] Cutting…"
bun e2e/video/cut-demo.ts "$TAKE" "$TAKE" || exit 1

echo ""
echo "Next: WATCH $TAKE/roomler-demo.mp4 end to end (see the note at the top of"
echo "this script), then copy it to ./roomler-demo.mp4 and demo-preview.gif to"
echo "docs/assets/ and ui/blog/assets/."
