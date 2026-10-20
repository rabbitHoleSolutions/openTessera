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
#   ./packaging/macos/build-app.sh --bundle-mpv copy Homebrew's libmpv and every
#                                               library it needs into the app
#   ./packaging/macos/build-app.sh --dmg        --bundle-mpv, then wrap the app
#                                               in target/Tessera-<ver>-macos-<arch>.dmg
#
# --default is opt-in on purpose, as on Linux: it takes over double-clicking
# every video. It prints the handlers it replaced so they can be restored
# (Get Info on a file → Open with → Change All). Folders are never claimed.
#
# By default the bundle does not carry libmpv: it loads Homebrew's (`brew
# install mpv`) from /opt/homebrew/lib, which is right for a machine that
# builds its own. --bundle-mpv makes an app that runs on a Mac without
# Homebrew: libmpv and its dependencies (ffmpeg and the rest, a few dozen)
# go into Contents/Frameworks, which tessera-mpv already tries first, with
# their references rewritten to point there. That is what the release
# workflow (.github/workflows/release.yml) ships.
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
bundle_mpv=0
dmg=0
for arg in "$@"; do
  case "$arg" in
    --no-build)  build=0 ;;
    --install)   install=1 ;;
    --uninstall) uninstall=1 ;;
    --default)   set_default=1 ;;
    --bundle-mpv) bundle_mpv=1 ;;
    --dmg)       dmg=1; bundle_mpv=1 ;;
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

# The icon, from the rounded-rect set (packaging/icons/make-icons.swift):
# macOS doesn't mask app icons, so the full-bleed Linux PNGs would sit in
# the Dock as hard-edged squares. iconutil wants the retina sizes as @2x of
# the size below.
ICONSET="$(mktemp -d)/tessera.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  cp "$REPO/packaging/icons/macos/icon-$s.png" "$ICONSET/icon_${s}x${s}.png"
  cp "$REPO/packaging/icons/macos/icon-$((s * 2)).png" "$ICONSET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/tessera.icns"
rm -rf "$(dirname "$ICONSET")"

if [ "$bundle_mpv" = 1 ]; then
  BREW="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
  MPV_DYLIB="${MPV_DYLIB:-$BREW/lib/libmpv.2.dylib}"
  [ -f "$MPV_DYLIB" ] || { echo "no libmpv at $MPV_DYLIB — brew install mpv" >&2; exit 1; }
  FW="$APP/Contents/Frameworks"
  mkdir -p "$FW"
  WORK="$(mktemp -d)"
  # Every library lands flat in Frameworks under its own file name; `origin`
  # remembers where each came from, because @loader_path and LC_RPATH are
  # relative to the original, not the copy. One name, one copy: Homebrew
  # refers to the same library by its opt/ and Cellar/ paths alike.
  : > "$WORK/todo"; : > "$WORK/origin"
  bring() { # bring <file name in Frameworks> <source path>
    cp -L "$2" "$FW/$1"
    chmod u+w "$FW/$1"
    # Resolved: brew's lib/ entries are symlinks into Cellar, and
    # @loader_path means the real file's directory.
    printf '%s\t%s\n' "$1" "$(readlink -f "$2")" >> "$WORK/origin"
    echo "$1" >> "$WORK/todo"
  }
  # Where a dependency, as written in <origin>'s load commands, really is.
  resolve() { # resolve <dep> <origin path> <copy>
    local dep="$1" dir rp rest
    dir="$(dirname "$2")"
    case "$dep" in
      @rpath/*)
        rest="${dep#@rpath/}"
        for rp in $(otool -l "$3" | awk '$1=="cmd" && $2=="LC_RPATH" {r=1} r && $1=="path" {print $2; r=0}'); do
          rp="${rp/@loader_path/$dir}"
          [ -e "$rp/$rest" ] && { echo "$rp/$rest"; return; }
        done
        [ -e "$BREW/lib/$rest" ] && { echo "$BREW/lib/$rest"; return; } ;;
      @loader_path/*) echo "$dir/${dep#@loader_path/}"; return ;;
      /*) [ -e "$dep" ] && { echo "$dep"; return; } ;;
    esac
    echo "cannot find $dep (needed by $2)" >&2
    exit 1
  }
  bring libmpv.2.dylib "$MPV_DYLIB"
  i=0
  while :; do
    i=$((i + 1))
    name="$(sed -n "${i}p" "$WORK/todo")"
    [ -n "$name" ] || break
    lib="$FW/$name"
    origin="$(awk -F'\t' -v n="$name" '$1==n {print $2; exit}' "$WORK/origin")"
    # A library lists its own id first among what it links; that one is
    # renamed with -id, not followed. (install_name_tool's stderr is its
    # warning that the signature is now invalid, which is re-done below.)
    self="$(otool -D "$lib" | sed -n 2p)"
    deps="$(otool -L "$lib" | tail -n +2 | awk '{print $1}')"
    install_name_tool -id "@executable_path/../Frameworks/$name" "$lib" 2>/dev/null
    for dep in $deps; do
      case "$dep" in /usr/lib/*|/System/*) continue ;; esac
      [ "$dep" = "$self" ] && continue
      base="$(basename "$dep")"
      # Not `[ -e ] || bring`: set -e is off on the right of ||, so a
      # library that couldn't be found would be skipped, not fatal.
      if [ ! -e "$FW/$base" ]; then
        src="$(resolve "$dep" "$origin" "$lib")"
        bring "$base" "$src"
      fi
      install_name_tool -change "$dep" "@executable_path/../Frameworks/$base" "$lib" 2>/dev/null
    done
  done
  # Anything still pointing outside the bundle would work here and fail on
  # a Mac without Homebrew, which is exactly the Mac this is for.
  stray="$(for f in "$FW"/*; do otool -L "$f" | tail -n +2 | awk '{print $1}'; done |
           grep -vE '^(/usr/lib/|/System/|@executable_path/\.\./Frameworks/)' | sort -u || true)"
  if [ -n "$stray" ]; then
    echo "bundled libraries still refer outside the app:" >&2
    echo "$stray" >&2
    exit 1
  fi
  # The bottles were built for the macOS they were built on, and a library
  # that needs a newer one fails to load with nothing but a crash to show
  # for it. Saying so in the plist makes Finder refuse it with a reason.
  minos="$(for f in "$FW"/* "$APP/Contents/MacOS/tessera"; do
             otool -l "$f" | awk '$2=="LC_BUILD_VERSION" {b=1} b && $1=="minos" {print $2; b=0}
                                  $2=="LC_VERSION_MIN_MACOSX" {m=1} m && $1=="version" {print $2; m=0}'
           done | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
  [ -n "$minos" ] && plutil -replace LSMinimumSystemVersion -string "$minos" "$APP/Contents/Info.plist"
  rm -rf "$WORK"
  echo "bundled $(ls "$FW" | wc -l | tr -d ' ') libraries ($(du -sh "$FW" | cut -f1)); needs macOS ${minos:-11.0}+"
  # Signed one by one, inside out: rewriting the load commands broke each
  # library's signature, and signing the app doesn't reach into Frameworks
  # (--deep would, but Apple advises against it).
  for f in "$FW"/*; do codesign --force --sign - "$f"; done
fi

# Ad-hoc signature over the whole bundle. Apple Silicon refuses unsigned
# code outright, and copying the binary in invalidated the linker's own.
codesign --force --sign - "$APP"

echo "built $APP ($VERSION)"

if [ "$dmg" = 1 ]; then
  ARCH="$(lipo -archs "$APP/Contents/MacOS/tessera" | tr ' ' '+')"
  DMG="$REPO/target/Tessera-$VERSION-macos-$ARCH.dmg"
  STAGE="$(mktemp -d)"
  ditto "$APP" "$STAGE/Tessera.app"
  # The usual drag-to-install window: the app beside a link to Applications.
  ln -s /Applications "$STAGE/Applications"
  rm -f "$DMG"
  hdiutil create -volname "Tessera $VERSION" -srcfolder "$STAGE" -format UDZO -quiet "$DMG"
  rm -rf "$STAGE"
  echo "built $DMG"
fi

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
  if [ "$bundle_mpv" = 1 ]; then
    echo "It is under 'Open With' for videos and folders."
  else
    echo "It is under 'Open With' for videos and folders; it needs Homebrew's mpv (brew install mpv)."
  fi
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
