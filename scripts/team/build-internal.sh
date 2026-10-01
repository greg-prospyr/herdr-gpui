#!/usr/bin/env bash
# Build an unsigned Herdr.app for a team to share, zipped for download.
#
# It is ad-hoc signed only (no Developer ID, no keys of any kind), so macOS
# runs it when it arrives without a quarantine flag: downloaded with curl or
# gh, not a browser. The in-app updater stays off, as for every build made
# without the release update key.
#
# The app is built from a fresh clone of REF, an ordinary checkout, so it has
# the standard icon and no worktree label even when run from a worktree.
#
# Usage: scripts/team/build-internal.sh REF OUTPUT_DIR [VERSION]
set -euo pipefail

test "$(uname -s)" = Darwin || { echo "macOS only" >&2; exit 1; }
ref=${1:?usage: build-internal.sh REF OUTPUT_DIR [VERSION]}
out=${2:?usage: build-internal.sh REF OUTPUT_DIR [VERSION]}
version=${3:-team-$(date -u +%Y%m%d)}
case "$version" in
  *[!A-Za-z0-9._-]*) echo "invalid version: $version" >&2; exit 2 ;;
esac

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/herdr-team-build.XXXXXX")
trap 'rm -rf "$work"' EXIT

git clone --quiet --no-local --branch "$ref" "$repo" "$work/src"
cd "$work/src"
# Dependencies are reused between builds; the source is always fresh.
export CARGO_TARGET_DIR="${HERDR_TEAM_TARGET_DIR:-$HOME/.cache/herdr-team-build/target}"
HERDR_BUILD_PR_NUMBER= cargo build --locked --release -p herdr-gpui

app="$out/Herdr.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$CARGO_TARGET_DIR/release/herdr-gpui" "$app/Contents/MacOS/Herdr"
cp assets/macos/Info.plist "$app/Contents/Info.plist"
icons=$(python3 scripts/release/build-icon.py macos "$app/Contents/MacOS/Herdr")
cp "${icons%$'\n'*}" "$app/Contents/Resources/Herdr.icns"
cp "${icons##*$'\n'}" "$app/Contents/Resources/Assets.car"
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

zip="$out/Herdr-$version-macos-$(uname -m).zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
shasum -a 256 "$zip" | tee "$zip.sha256"
echo "built $zip from $(git rev-parse --short HEAD) ($ref)"
