#!/usr/bin/env bash
# Runs the ignored tests of docsweep-browser in a real, headless browser against a
# fictional portal served by the tests themselves. They never contact a real
# portal. Pass --visible to also run the test that opens a browser window.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

chrome_found() {
  if [ -n "${CHROME:-}" ] && [ -e "$CHROME" ]; then
    return 0
  fi
  local name
  for name in google-chrome google-chrome-stable chromium chromium-browser chrome microsoft-edge msedge; do
    command -v "$name" >/dev/null 2>&1 && return 0
  done
  local path
  for path in \
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    "/Applications/Chromium.app/Contents/MacOS/Chromium" \
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge" \
    "/c/Program Files/Google/Chrome/Application/chrome.exe" \
    "/c/Program Files (x86)/Google/Chrome/Application/chrome.exe" \
    "/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe"; do
    [ -e "$path" ] && return 0
  done
  return 1
}

if ! chrome_found; then
  echo "No Chrome, Chromium or Edge found, so the browser tests cannot run." >&2
  echo "Install one of them, or point the CHROME environment variable at it." >&2
  exit 1
fi

skip=(--skip visible)
if [ "${1:-}" = "--visible" ]; then
  skip=()
fi

cd "$ROOT"
cargo test -p docsweep-browser --test portal -- --ignored --test-threads=1 "${skip[@]}"
