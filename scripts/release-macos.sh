#!/usr/bin/env bash
# VoxBar macOS release pipeline.
#
# Produces four artifacts for a GitHub release:
#   VoxBar-<version>-macOS.zip   first install (Finder-usable .app)
#   VoxBar.app.tar.gz            updater payload (the .app dir is the archive root)
#   VoxBar.app.tar.gz.sig        minisign signature over the tar.gz
#   latest.json                  updater manifest (version, notes, per-platform signature)
#
# The order below is load-bearing (see BUILD.md "Releasing (macOS)"):
#   build WITHOUT updater artifacts -> re-sign the .app with the stable
#   designated requirement -> tar -> sign the tar.gz -> latest.json.
#   The tar.gz must be created from the FINISHED signed .app; re-signing
#   after tarring (or shipping a tar.gz made before the re-sign) either
#   invalidates the signature or silently ships the pre-re-sign app whose
#   signature still verifies. Never hand-publish artifacts that skipped a
#   step.
#
# The updater keypair lives at ~/.voxbar/updater-keys/ (never committed).
# Usage: bash scripts/release-macos.sh [assets-dir]
#   assets-dir defaults to
#   /Users/shahil/work/regenai-repo/mission-control/voxbar-build-docs/v<version>/release-assets
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

APP_PATH="src-tauri/target/release/bundle/macos/VoxBar.app"
TARGZ="src-tauri/target/release/bundle/macos/VoxBar.app.tar.gz"
KEY_DIR="$HOME/.voxbar/updater-keys"
KEY_FILE="$KEY_DIR/voxbar.key"
IDENTIFIER="com.voxbar.app"
# Identifier-only designated requirement: keeps the macOS TCC code identity
# stable across ad-hoc-signed builds so Accessibility grants survive
# updates. Tradeoff: any binary claiming this identifier satisfies it (no
# Developer ID); documented in BUILD.md.
DR='designated => identifier "com.voxbar.app"'

log() { printf '[release] %s\n' "$*"; }

# Apple Intelligence (FoundationModels) needs the FULL Xcode toolchain: a
# Command-Line-Tools-only selection compiles the stub and the app then
# reports AI unavailable on every machine. If full Xcode is installed and
# the builder did not pin a toolchain, prefer Xcode for this build without
# touching the machine's global xcode-select.
# The Swift AI bridge links FoundationModels from the SDK it compiled
# against; a stale or unset SDKROOT resolves a CommandLineTools SDK whose
# FoundationModels lacks the GenerationOptions init the bridge references
# (link-time undefined symbol on a fresh compile). Export the full-Xcode SDK
# for this build whenever it is unset, mirroring DEVELOPER_DIR above.
if [ -z "${SDKROOT:-}" ] && [ -d /Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk ]; then
  export SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk
  log "using full Xcode SDK for this build (SDKROOT=$SDKROOT)"
fi

if [ -z "${DEVELOPER_DIR:-}" ] && [ -d /Applications/Xcode.app/Contents/Developer ]; then
  export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
  log "using full Xcode toolchain for this build (DEVELOPER_DIR=$DEVELOPER_DIR)"
fi
die() { printf '[release] ERROR: %s\n' "$*" >&2; exit 1; }

# --- 1. Version agreement: tauri.conf.json is the source of truth ----------
VERSION="$(jq -r '.version' src-tauri/tauri.conf.json)"
[ -n "$VERSION" ] || die "could not read version from src-tauri/tauri.conf.json"
PKG_VERSION="$(jq -r '.version' package.json)"
CARGO_VERSION="$(grep -m1 '^version = ' src-tauri/Cargo.toml | sed -E 's/version = "(.*)"/\1/')"
[ "$VERSION" = "$PKG_VERSION" ] || die "version mismatch: tauri.conf.json $VERSION vs package.json $PKG_VERSION"
[ "$VERSION" = "$CARGO_VERSION" ] || die "version mismatch: tauri.conf.json $VERSION vs Cargo.toml $CARGO_VERSION"
log "version: $VERSION (tauri.conf.json, package.json, Cargo.toml agree)"

# --- 2. Signing key custody -------------------------------------------------
[ -f "$KEY_FILE" ] || die "updater private key not found at $KEY_FILE (generate it once per BUILD.md)"
KEY_MODE="$(stat -f '%Lp' "$KEY_FILE")"
[ "$KEY_MODE" = "600" ] || die "updater key $KEY_FILE has mode $KEY_MODE, expected 600 (chmod 600 it)"
log "signing key present with mode 600"

# --- 3. Build (no updater artifacts; this pipeline signs manually) ----------
# The build log is kept (repo root) because the AI gate below greps it, and
# because it is the post-mortem record when a release goes wrong.
BUILD_LOG="$ROOT/release-build.log"
log "building (nice -n 15 bunx tauri build); this is the slow step"
nice -n 15 bunx tauri build 2>&1 | tee "$BUILD_LOG"
[ -d "$APP_PATH" ] || die "bundle app not found at $APP_PATH after build"

# --- 3b. AI build-integrity gate ---------------------------------------------
# Nothing above PROVES the release binary linked the real FoundationModels:
# if the DEVELOPER_DIR export silently failed, the build.rs auto-detection
# compiles the stub and the app reports AI unavailable on every machine.
# Prove both the linkage and the build log before signing or smoking.
BIN_PATH="$APP_PATH/Contents/MacOS/voxbar"
[ -f "$BIN_PATH" ] || die "bundle binary not found at $BIN_PATH"
if ! otool -L "$BIN_PATH" | grep -q 'FoundationModels.framework'; then
  die "$BIN_PATH does not link FoundationModels.framework: the toolchain resolved without full Xcode and the build used stubs. Install Xcode, verify the DEVELOPER_DIR export above, then rebuild."
fi
if grep -q 'Building Apple Intelligence with stubs' "$BUILD_LOG"; then
  die "build log $BUILD_LOG contains 'Building Apple Intelligence with stubs'; refusing to ship a stub-linked AI build"
fi
if grep -q 'Apple Intelligence SDK not found' "$BUILD_LOG"; then
  die "build log $BUILD_LOG contains 'Apple Intelligence SDK not found'; refusing to ship a stub-linked AI build"
fi
log "AI gate: $BIN_PATH links FoundationModels.framework and the build log shows no stub fallback"

# Smoke the fresh bundle before re-signing: launch it, confirm the process
# stays up, then quit it. Signatures and manifests cannot catch a startup
# crash; this can (and stops the release when it happens).
bash scripts/smoke-macos.sh "$APP_PATH"

# --- 4. Re-sign with the stable designated requirement ----------------------
log "re-signing $APP_PATH with the stable designated requirement"
codesign --force --sign - --timestamp=none --options runtime \
  --entitlements src-tauri/Entitlements.plist --identifier "$IDENTIFIER" \
  -r "=designated => identifier \"$IDENTIFIER\"" \
  "$APP_PATH"
codesign --verify --deep --strict "$APP_PATH"
ACTUAL_DR="$(codesign -d -r- "$APP_PATH" 2>&1 | grep '^designated =>' || true)"
[ "$ACTUAL_DR" = "$DR" ] || die "designated requirement is '$ACTUAL_DR', expected exactly '$DR'"
log "DR verified: $DR"

# --- 5. Plain first-install zip ---------------------------------------------
# Honesty note: the exact command that produced the v1.0.2 release zip is
# not recorded anywhere in this repo or in voxbar-build-docs/v1.0.2. From
# 1.1.0 the convention IS this script: ditto -c -k --keepParent, which
# preserves the bundle structure Finder expects.
ZIP_PATH="src-tauri/target/release/bundle/macos/VoxBar-${VERSION}-macOS.zip"
log "creating first-install zip: $ZIP_PATH"
( cd "$(dirname "$APP_PATH")" && ditto -c -k --keepParent "$(basename "$APP_PATH")" "$(basename "$ZIP_PATH")" )
[ -s "$ZIP_PATH" ] || die "zip was not created: $ZIP_PATH"

# --- 6. Updater archive: the .app dir must be the archive root --------------
# The updater's macOS extractor strips the first path component of every
# entry, so the root entry must be the .app directory itself.
log "creating updater tar.gz"
( cd "$(dirname "$APP_PATH")" && COPYFILE_DISABLE=1 /usr/bin/tar -czf VoxBar.app.tar.gz "$(basename "$APP_PATH")" )
FIRST_ENTRY="$(tar -tzf "$TARGZ" | head -1)"
[ "$FIRST_ENTRY" = "VoxBar.app/" ] || die "tar.gz first entry is '$FIRST_ENTRY', expected 'VoxBar.app/'"

# --- 7. Sign the tar.gz ------------------------------------------------------
# The pinned @tauri-apps/cli has no --app-version flag on `signer sign`
# (verified against 2.11.4); the .sig carries no app version. The version
# reaches users via latest.json (step 8) and the bundle itself.
log "signing $TARGZ"
bunx tauri signer sign -f "$KEY_FILE" --password "" "$TARGZ"
[ -s "$TARGZ.sig" ] || die "signature file not created: $TARGZ.sig"
SIG_CONTENT="$(cat "$TARGZ.sig")"
[ -n "$SIG_CONTENT" ] || die "signature file is empty: $TARGZ.sig"

# --- 8. latest.json ----------------------------------------------------------
PUB_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
UPDATE_URL="https://github.com/nexiouscaliver/voxbar/releases/download/v${VERSION}/VoxBar.app.tar.gz"
# Both platform keys carry the same entry: the updater looks for
# darwin-aarch64-app first (os-arch-bundle_type) and falls back to plain
# darwin-aarch64; shipping both is the belt-and-braces form.
LATEST_JSON_PATH="src-tauri/target/release/bundle/macos/latest.json"
jq -n \
  --arg version "$VERSION" \
  --arg notes "VoxBar ${VERSION}: phone dictation audio actually transmits now (the v1.6.2 resampler corrupted a sample at every block boundary and the Mac read the whole stream as silence, in both hold and tap-to-lock modes), the phone's level meter shows only audio that was really sent and decays to empty and dim when the gate is closed, the zero-audio recovery advice names the mode you were using in every supported language, and audio the Mac admits but has to drop is counted per phone, warned at the first occurrence, and totaled at disconnect instead of being eaten silently." \
  --arg pub_date "$PUB_DATE" \
  --arg sig "$SIG_CONTENT" \
  --arg url "$UPDATE_URL" \
  '{version: $version, notes: $notes, pub_date: $pub_date,
    platforms: {
      "darwin-aarch64-app": {signature: $sig, url: $url},
      "darwin-aarch64": {signature: $sig, url: $url}
    }}' > "$LATEST_JSON_PATH"
jq -e '.version == $v and (.platforms["darwin-aarch64-app"].signature | length > 0)' --arg v "$VERSION" "$LATEST_JSON_PATH" >/dev/null \
  || die "latest.json failed self-check"

# --- 9. Copy artifacts + manifest --------------------------------------------
DEFAULT_ASSETS_DIR="/Users/shahil/work/regenai-repo/mission-control/voxbar-build-docs/v${VERSION}/release-assets"
ASSETS_DIR="${1:-$DEFAULT_ASSETS_DIR}"
mkdir -p "$ASSETS_DIR"
cp "$ZIP_PATH" "$TARGZ" "$TARGZ.sig" "$LATEST_JSON_PATH" "$ASSETS_DIR/"

log "manifest (assets dir: $ASSETS_DIR)"
for f in "VoxBar-${VERSION}-macOS.zip" "VoxBar.app.tar.gz" "VoxBar.app.tar.gz.sig" "latest.json"; do
  SIZE="$(stat -f '%z' "$ASSETS_DIR/$f")"
  printf '  %-28s %s bytes\n' "$f" "$SIZE"
done
printf '  designated requirement: %s\n' "$DR"
log "done. Publishing the GitHub release v${VERSION} (tag + upload of the four files) is a manual, operator-only step."
