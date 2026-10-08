#!/usr/bin/env bash
#
# Installs Tessera for the current user: binary, icons and desktop entry,
# all under $HOME. Nothing here needs root and nothing touches system paths.
#
#   ./packaging/install.sh              build (release) and install
#   ./packaging/install.sh --no-build   install whatever is already built
#   ./packaging/install.sh --default    also make Tessera the default video
#                                       player for the MIME types it handles
#   ./packaging/install.sh --uninstall  remove everything this script installs
#
# --default is opt-in on purpose: it takes over double-clicking every video
# on the machine. Without it Tessera simply appears in "Open With".
#
# Config (~/.config/opentessera) is never touched, by install or uninstall.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# macOS installs an app bundle instead; same flags, its own script.
if [ "$(uname -s)" = Darwin ]; then
  case " $* " in
    *" --uninstall "*) exec "$REPO/packaging/macos/build-app.sh" "$@" ;;
    *) exec "$REPO/packaging/macos/build-app.sh" --install "$@" ;;
  esac
fi

BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
APP_DIR="$DATA_DIR/applications"
ICON_ROOT="$DATA_DIR/icons/hicolor"
SIZES=(16 24 32 48 64 128 256 512)
MIMES=(
  video/x-matroska video/mp4 video/mpeg video/quicktime video/x-msvideo
  video/x-ms-wmv video/webm video/x-flv video/3gpp video/ogg video/mp2t
  video/x-m4v video/avi
)

build=1
set_default=0
uninstall=0
for arg in "$@"; do
  case "$arg" in
    --no-build)  build=0 ;;
    --default)   set_default=1 ;;
    --uninstall) uninstall=1 ;;
    -h|--help)   awk 'NR>1 && /^#/ {sub(/^# ?/, ""); print; next} NR>1 {exit}' "$0"; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

refresh_caches() {
  command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" 2>/dev/null || true
  command -v gtk-update-icon-cache   >/dev/null && gtk-update-icon-cache -f -t "$ICON_ROOT" 2>/dev/null || true
}

if [ "$uninstall" = 1 ]; then
  rm -f "$BIN_DIR/tessera" "$APP_DIR/tessera.desktop"
  for s in "${SIZES[@]}"; do rm -f "$ICON_ROOT/${s}x${s}/apps/tessera.png"; done
  refresh_caches
  echo "removed Tessera (config in ~/.config/opentessera left alone)"
  exit 0
fi

if [ "$build" = 1 ]; then
  echo "building release binary..."
  ( cd "$REPO" && cargo build --release )
fi

BIN="$REPO/target/release/tessera"
[ -x "$BIN" ] || { echo "no binary at $BIN — run without --no-build" >&2; exit 1; }

install -d "$BIN_DIR" "$APP_DIR"
# Staged then renamed, rather than written in place: writing over a running
# executable fails with ETXTBSY, and now that Tessera is something people
# leave open, reinstalling mid-session is the normal case. rename(2) within
# one directory is atomic, and a running instance simply keeps the old
# inode until it is restarted.
install -m755 "$BIN" "$BIN_DIR/tessera.new"
mv -f "$BIN_DIR/tessera.new" "$BIN_DIR/tessera"

for s in "${SIZES[@]}"; do
  install -d "$ICON_ROOT/${s}x${s}/apps"
  install -m644 "$REPO/packaging/icons/tessera-${s}.png" "$ICON_ROOT/${s}x${s}/apps/tessera.png"
done

install -m644 "$REPO/packaging/tessera.desktop" "$APP_DIR/tessera.desktop"
if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$APP_DIR/tessera.desktop" || echo "warning: desktop entry did not validate cleanly"
fi
refresh_caches

if [ "$set_default" = 1 ]; then
  for m in "${MIMES[@]}"; do xdg-mime default tessera.desktop "$m"; done
  echo "Tessera is now the default player for ${#MIMES[@]} video types"
fi

echo
echo "installed:"
echo "  $BIN_DIR/tessera"
echo "  $APP_DIR/tessera.desktop"
echo "  $ICON_ROOT/{${SIZES[0]}x${SIZES[0]},...}/apps/tessera.png"
echo
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "note: $BIN_DIR is not on your PATH — add it to run 'tessera' from a shell"; echo ;;
esac
echo "It should now appear in your app menu, and under 'Open With' for videos."
[ "$set_default" = 1 ] || echo "Run with --default to also make it the default video player."
