#!/usr/bin/env bash
#
# Builds Tessera.app for macOS, and optionally installs it for the current
# user. Nothing here needs root.
#
#   ./packaging/macos/build-app.sh              build target/Tessera.app
#   ./packaging/macos/build-app.sh --install    ...and copy it to ~/Applications
#   ./packaging/macos/build-app.sh --no-build   bundle whatever is already built
#   ./packaging/macos/build-app.sh --default    with --install: also make it the
#                                               default player for video types
#   ./packaging/macos/build-app.sh --uninstall  remove ~/Applications/Tessera.app
#
# --default is opt-in on purpose, as on Linux: it takes over double-clicking
# every video. It prints the handlers it replaced so they can be restored
# (Get Info on a file → Open with → Change All). Folders are never claimed.
#
# The bundle does not carry libmpv: it loads Homebrew's (`brew install mpv`)
# from /opt/homebrew/lib. A distributable build would copy libmpv and its
# dylibs into Contents/Frameworks, which the loader already looks in first.
#
# Config (~/.config/opentessera) is never touched, by install or uninstall.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEST_DIR="$HOME/Applications"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
# The concrete video types the bundle claims (Info.plist). Not folders, and
# not the abstract parents (public.movie/video): each one costs the user a
# confirmation prompt, and files are matched by their concrete type.
VIDEO_TYPES=(
  public.mpeg-4 com.apple.quicktime-movie org.matroska.mkv public.avi
  org.webmproject.webm com.apple.m4v-video public.mpeg
  public.mpeg-2-transport-stream com.microsoft.windows-media-wmv public.3gpp
)

build=1
install=0
uninstall=0
set_default=0
for arg in "$@"; do
  case "$arg" in
    --no-build)  build=0 ;;
    --install)   install=1 ;;
    --uninstall) uninstall=1 ;;
    --default)   set_default=1 ;;
    -h|--help)   awk 'NR>1 && /^#/ {sub(/^# ?/, ""); print; next} NR>1 {exit}' "$0"; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if [ "$uninstall" = 1 ]; then
  if [ -d "$DEST_DIR/Tessera.app" ]; then
    "$LSREGISTER" -u "$DEST_DIR/Tessera.app" 2>/dev/null || true
    rm -rf "$DEST_DIR/Tessera.app"
  fi
  echo "removed Tessera (config in ~/.config/opentessera left alone)"
  exit 0
fi

if [ "$build" = 1 ]; then
  echo "building release binary..."
  ( cd "$REPO" && cargo build --release )
fi

BIN="$REPO/target/release/tessera"
[ -x "$BIN" ] || { echo "no binary at $BIN — run without --no-build" >&2; exit 1; }

VERSION="$(awk -F'"' '/^\[workspace.package\]/ {p=1} p && /^version/ {print $2; exit}' "$REPO/Cargo.toml")"
APP="$REPO/target/Tessera.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/tessera"
sed "s/@VERSION@/$VERSION/g" "$REPO/packaging/macos/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint -s "$APP/Contents/Info.plist"

# The icon, from the same PNGs the Linux install uses. iconutil wants the
# retina sizes as @2x of the size below; there is no 1024 master, so the
# 512@2x slot is left empty.
ICONSET="$(mktemp -d)/tessera.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  cp "$REPO/packaging/icons/tessera-$s.png" "$ICONSET/icon_${s}x${s}.png"
done
for s in 16 32 128 256; do
  cp "$REPO/packaging/icons/tessera-$((s * 2)).png" "$ICONSET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/tessera.icns"
rm -rf "$(dirname "$ICONSET")"

# Ad-hoc signature over the whole bundle. Apple Silicon refuses unsigned
# code outright, and copying the binary in invalidated the linker's own.
codesign --force --sign - "$APP"

echo "built $APP ($VERSION)"

if [ "$install" = 1 ]; then
  mkdir -p "$DEST_DIR"
  rm -rf "$DEST_DIR/Tessera.app"
  ditto "$APP" "$DEST_DIR/Tessera.app"
  # Register now, so Open With lists Tessera without waiting for a rescan —
  # and remove the build copy: it has the same bundle ID (running it once
  # registers it, and Spotlight re-finds it), so it would compete to be what
  # opens files, and it is overwritten by every build.
  "$LSREGISTER" -u "$APP" 2>/dev/null || true
  rm -rf "$APP"
  "$LSREGISTER" -f "$DEST_DIR/Tessera.app"
  echo "installed $DEST_DIR/Tessera.app"
  echo "It is under 'Open With' for videos and folders; it needs Homebrew's mpv (brew install mpv)."
fi

if [ "$set_default" = 1 ]; then
  if [ "$install" != 1 ]; then
    echo "--default needs --install (the default has to be the installed copy)" >&2
    exit 2
  fi
  if ! command -v swift >/dev/null; then
    echo "--default needs Swift (Xcode or the Command Line Tools); otherwise:" \
         "Get Info on a video → Open with: Tessera → Change All" >&2
    exit 1
  fi
  echo "default player for video types — macOS asks you to confirm each one:"
  swift "$REPO/packaging/macos/set-default.swift" "$DEST_DIR/Tessera.app" "${VIDEO_TYPES[@]}" ||
    echo "some types were not changed (declined or unanswered); rerun, or use Get Info → Change All"
fi
