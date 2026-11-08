#!/usr/bin/env bash
# Cargo runner on macOS (see .cargo/config.toml). Debug builds are only signed ad
# hoc, so the keychain ties "Always Allow" to that one binary and asks again after
# every rebuild. With DOCSWEEP_CODESIGN_IDENTITY set to a code signing identity
# (`security find-identity -v -p codesigning`; the hash if names repeat), the app
# and the CLI are signed with it before they start, under a fixed identifier, so
# the permission survives rebuilds. Without the variable, or for any other binary
# such as tests, this only runs the program.
#
# The identity must not be revoked: `find-identity -v` still lists revoked
# certificates as valid, but macOS then reports the binary as malware and moves
# it to the trash. Delete revoked certificates from the keychain first.
set -euo pipefail

binary="$1"
shift

if [ -n "${DOCSWEEP_CODESIGN_IDENTITY:-}" ]; then
  root="$(cd "$(dirname "$0")/.." && pwd)"
  # The Bundle ID has a single source: `identifier` in tauri.conf.json.
  bundle_id="$(sed -n 's/^ *"identifier": *"\([^"]*\)".*/\1/p' "$root/crates/docsweep-gui/tauri.conf.json" | head -n 1)"
  case "$(basename "$binary")" in
    docsweep-gui) identifier="$bundle_id" ;;
    docsweep) identifier="$bundle_id.cli" ;;
    *) identifier="" ;;
  esac
  if [ -n "$identifier" ] && [ -n "$bundle_id" ]; then
    codesign --force --sign "$DOCSWEEP_CODESIGN_IDENTITY" --identifier "$identifier" "$binary"
  fi
fi

exec "$binary" "$@"
