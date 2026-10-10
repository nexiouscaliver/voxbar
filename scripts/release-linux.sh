#!/usr/bin/env bash
# VoxBar Linux release build pipeline.
#
# Mirrors scripts/release-macos.sh in structure (version agreement check,
# capped build, artifact listing) but there is nothing to sign: the in-app
# updater ships macOS artifacts only (latest.json carries darwin entries
# alone), so Linux users install from these files directly.
#
# Produces, under src-tauri/target/release/bundle/:
#   deb/voxbar_<version>_amd64.deb
#   rpm/voxbar-<version>-1.x86_64.rpm (names follow the distro's conventions)
#   appimage/VoxBar_<version>_amd64.AppImage
# plus sha256 checksums for each, copied to the assets dir.
#
# The bundle target list comes from src-tauri/tauri.linux.conf.json (deb,
# rpm, appimage). Pass target names as arguments to build a subset:
#   bash scripts/release-linux.sh deb            # skip rpm/AppImage
#   NO_STRIP=true bash scripts/release-linux.sh  # linuxdeploy old-strip workaround
#
# NO_STRIP is passed through for rolling-release distros where linuxdeploy's
# bundled strip binary is too old for system libraries (see BUILD.md
# "AppImage build fails on Arch / rolling-release distros").
#
# Publishing the GitHub release (tag + upload) is a manual, operator-only
# step, exactly like the macOS pipeline.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

BUNDLE_ROOT="src-tauri/target/release/bundle"
KEY_DIR="$HOME/.voxbar/updater-keys"

log() { printf '[release-linux] %s\n' "$*"; }
die() { printf '[release-linux] ERROR: %s\n' "$*" >&2; exit 1; }

# --- 1. Version agreement: tauri.conf.json is the source of truth ----------
VERSION="$(jq -r '.version' src-tauri/tauri.conf.json)"
[ -n "$VERSION" ] || die "could not read version from src-tauri/tauri.conf.json"
PKG_VERSION="$(jq -r '.version' package.json)"
CARGO_VERSION="$(grep -m1 '^version = ' src-tauri/Cargo.toml | sed -E 's/version = "(.*)"/\1/')"
[ "$VERSION" = "$PKG_VERSION" ] || die "version mismatch: tauri.conf.json $VERSION vs package.json $PKG_VERSION"
[ "$VERSION" = "$CARGO_VERSION" ] || die "version mismatch: tauri.conf.json $VERSION vs Cargo.toml $CARGO_VERSION"
log "version: $VERSION (tauri.conf.json, package.json, Cargo.toml agree)"

# --- 2. Build ---------------------------------------------------------------
# Targets come from the tauri.linux.conf.json overlay; arguments narrow them
# (bunx tauri build --bundles accepts comma-separated names).
BUILD_CMD=(nice -n 15 bunx tauri build)
if [ "$#" -gt 0 ]; then
  BUILD_CMD+=(--bundles "$(IFS=,; echo "$*")")
fi
if [ "${NO_STRIP:-}" = "true" ] || [ "${NO_STRIP:-}" = "1" ]; then
  log "building with: NO_STRIP=true ${BUILD_CMD[*]}"
  NO_STRIP=true "${BUILD_CMD[@]}"
else
  log "building with: ${BUILD_CMD[*]}"
  "${BUILD_CMD[@]}"
fi
[ -d "$BUNDLE_ROOT" ] || die "bundle directory not found at $BUNDLE_ROOT after build"

# --- 3. Collect artifacts + checksums ----------------------------------------
DEFAULT_ASSETS_DIR="/Users/shahil/work/regenai-repo/mission-control/voxbar-build-docs/v${VERSION}/release-assets-linux"
ASSETS_DIR="${DEFAULT_ASSETS_DIR}"
# The last argument that names an existing directory overrides the assets dir;
# otherwise every argument is a bundle target (handled above).
for arg in "$@"; do
  if [ -d "$arg" ]; then
    ASSETS_DIR="$arg"
  fi
done
mkdir -p "$ASSETS_DIR"

shopt -s nullglob
ARTIFACTS=()
for pattern in \
  "$BUNDLE_ROOT/deb/voxbar_${VERSION}_amd64.deb" \
  "$BUNDLE_ROOT/deb/voxbar_${VERSION}_*.deb" \
  "$BUNDLE_ROOT/rpm/voxbar-${VERSION}"*.rpm \
  "$BUNDLE_ROOT/rpm/voxbar_${VERSION}"*.rpm \
  "$BUNDLE_ROOT/appimage/VoxBar_${VERSION}_amd64.AppImage" \
  "$BUNDLE_ROOT/appimage/VoxBar_${VERSION}"*.AppImage; do
  for file in $pattern; do
    case " ${ARTIFACTS[*]:-} " in
      *" $file "*) ;; # already collected via a wider pattern
      *) ARTIFACTS+=("$file") ;;
    esac
  done
done
shopt -u nullglob

if [ "${#ARTIFACTS[@]}" -eq 0 ]; then
  die "no bundles found under $BUNDLE_ROOT (expected deb/rpm/appimage; pass targets as arguments if you built a subset)"
fi

CHECKSUMS="$ASSETS_DIR/checksums-linux-v${VERSION}.txt"
: > "$CHECKSUMS"
for artifact in "${ARTIFACTS[@]}"; do
  cp "$artifact" "$ASSETS_DIR/"
  sha256sum "$artifact" >> "$CHECKSUMS"
done

log "artifacts (assets dir: $ASSETS_DIR):"
for artifact in "${ARTIFACTS[@]}"; do
  SIZE="$(stat -c '%s' "$artifact" 2>/dev/null || stat -f '%z' "$artifact")"
  printf '  %-40s %s bytes\n' "$(basename "$artifact")" "$SIZE"
done
printf '  checksums: %s\n' "$CHECKSUMS"
log "done. Publishing the GitHub release v${VERSION} (tag + upload of the files above) is a manual, operator-only step."
log "note: do NOT add these files to latest.json; the in-app updater is macOS-only until signed Linux artifacts exist."
