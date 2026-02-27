#!/bin/bash
# Record and convert the Roomler intro video
#
# Prerequisites:
#   - API running (cargo run) at port 5001
#   - Frontend running (cd ui && bun run dev) proxied at port 5000
#   - Playwright installed (cd ui && bun install && bunx playwright install chromium)
#   - ffmpeg installed (for MP4 conversion)
#   - Node.js v22+ via fnm (Playwright requires real Node, not bun's wrapper)
#
# Usage:
#   ./scripts/record-video.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
UI_DIR="$PROJECT_DIR/ui"
OUTPUT_DIR="$UI_DIR/e2e/video/output"

# Use real Node.js via fnm (not bun's wrapper) for Playwright compatibility
if [ -d "$HOME/.local/share/fnm" ]; then
  export PATH="$HOME/.local/share/fnm:$PATH"
  eval "$(fnm env)"
  fnm use --install-if-missing lts 2>/dev/null || fnm use default 2>/dev/null
fi

echo "=== Roomler Intro Video Recording ==="
echo "Node: $(which node) ($(node -e 'console.log(process.version)'))"
echo ""

# Ensure output directory exists
mkdir -p "$OUTPUT_DIR"

# Run the Playwright recording (test saves WebM to output dir automatically)
echo "[1/2] Recording video with Playwright..."
cd "$UI_DIR"
E2E_BASE_URL=http://localhost:5000 node ./node_modules/.bin/playwright test \
  e2e/video/record-intro.spec.ts \
  --config=playwright.video.config.ts \
  --reporter=list 2>&1 || true

# Check for the saved WebM
WEBM_FILE="$OUTPUT_DIR/roomler-intro.webm"

if [ ! -f "$WEBM_FILE" ]; then
  echo "ERROR: No video file found at $WEBM_FILE"
  echo "Check that the test ran successfully and video recording is enabled."
  exit 1
fi

echo "Found recording: $WEBM_FILE ($(du -h "$WEBM_FILE" | cut -f1))"

# Convert to MP4 with ffmpeg
MP4_FILE="$OUTPUT_DIR/roomler-intro.mp4"

if command -v ffmpeg &>/dev/null; then
  echo "[2/2] Converting to MP4..."
  ffmpeg -y -i "$WEBM_FILE" \
    -c:v libx264 \
    -preset slow \
    -crf 18 \
    -pix_fmt yuv420p \
    -movflags +faststart \
    "$MP4_FILE" 2>/dev/null

  echo ""
  echo "=== Done ==="
  echo "WebM: $WEBM_FILE"
  echo "MP4:  $MP4_FILE ($(du -h "$MP4_FILE" | cut -f1))"
  echo ""
  echo "Upload to YouTube, then update README.md with the video link."
else
  echo "[2/2] ffmpeg not found — skipping MP4 conversion."
  echo ""
  echo "=== Done ==="
  echo "WebM: $WEBM_FILE"
  echo ""
  echo "To convert manually:"
  echo "  ffmpeg -i \"$WEBM_FILE\" -c:v libx264 -crf 18 -pix_fmt yuv420p \"$MP4_FILE\""
fi
