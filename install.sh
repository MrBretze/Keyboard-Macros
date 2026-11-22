#!/usr/bin/env bash
# Copies the plugin into OpenDeck's plugin folder (Flatpak or native install).
set -euo pipefail
SRC="$(cd "$(dirname "$0")" && pwd)/com.mrbretzel.keymacro.sdPlugin"
FLATPAK_DIR="$HOME/.var/app/me.amankhanna.opendeck/config/opendeck/plugins"
NATIVE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/opendeck/plugins"
if [ -d "$FLATPAK_DIR" ]; then DEST="$FLATPAK_DIR"; else DEST="$NATIVE_DIR"; mkdir -p "$DEST"; fi
TARGET="$DEST/com.mrbretzel.keymacro.sdPlugin"

rm -rf "$TARGET.new"
cp -r "$SRC" "$TARGET.new"
# Drop build artifacts and the Windows executable, keep an existing log file.
find "$TARGET.new" -name "__pycache__" -type d -prune -exec rm -rf {} +
rm -f "$TARGET.new/plugin.exe" "$TARGET.new/plugin.log"
[ -f "$TARGET/plugin.log" ] && cp "$TARGET/plugin.log" "$TARGET.new/" || true
rm -rf "$TARGET" && mv "$TARGET.new" "$TARGET"
chmod +x "$TARGET/plugin.py"
echo "Installed to: $TARGET"
echo "Restart OpenDeck to load the plugin."
