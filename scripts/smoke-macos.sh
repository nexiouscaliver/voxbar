#!/usr/bin/env bash
# macOS release smoke: launch the built VoxBar bundle, confirm the process
# comes up and stays up, then quit it cleanly. Catches startup crashes that
# signature and manifest verification cannot (a binary that dies at launch
# would otherwise ship inside a complete, correctly-signed release).
#
# Usage: bash scripts/smoke-macos.sh <path-to-VoxBar.app>
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <path-to-VoxBar.app>" >&2
  exit 2
fi

BUNDLE="$1"
# The launched process name is the LOWERCASE binary (Cargo name "voxbar"),
# not the productName; pgrep -x is case-sensitive, so matching "VoxBar"
# can never see the app (and misses an already-running instance).
APP_NAME="voxbar"
APP_ID="com.voxbar.app"   # tauri.conf.json identifier
LAUNCH_TIMEOUT_S=45  # first launch of a freshly built bundle can take well over 20s through LaunchServices
QUIT_GRACE_S=10

log() { printf '[smoke] %s\n' "$*"; }
die() { printf '[smoke] ERROR: %s\n' "$*" >&2; exit 1; }

[ -d "$BUNDLE" ] || die "bundle not found: $BUNDLE"

# An already-running instance would make every check below lie (`open` would
# merely activate it and pgrep would match the old process), so refuse
# rather than false-pass.
if pgrep -x "$APP_NAME" >/dev/null; then
  die "$APP_NAME is already running; quit it before the release smoke"
fi

log "launching $BUNDLE"
smoke_launched=1
cleanup() { [ "${smoke_launched:-0}" -eq 1 ] && osascript -e 'quit app id "com.voxbar.app"' >/dev/null 2>&1; }
trap cleanup EXIT
open "$BUNDLE"

launched=0
for _ in $(seq 1 "$LAUNCH_TIMEOUT_S"); do
  if pgrep -x "$APP_NAME" >/dev/null; then
    launched=1
    break
  fi
  sleep 1
done
[ "$launched" -eq 1 ] || die "$APP_NAME did not appear within ${LAUNCH_TIMEOUT_S}s of launch"

# Give a crash-on-startup a moment to happen before trusting the process.
sleep 2
pgrep -x "$APP_NAME" >/dev/null || die "$APP_NAME exited shortly after launch"

log "process is up; quitting"
osascript -e "tell application id \"$APP_ID\" to quit"

for _ in $(seq 1 "$QUIT_GRACE_S"); do
  if ! pgrep -x "$APP_NAME" >/dev/null; then
    log "smoke ok: launched and quit cleanly"
    exit 0
  fi
  sleep 1
done

die "$APP_NAME still running ${QUIT_GRACE_S}s after quit"
