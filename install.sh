#!/usr/bin/env bash
# Builds the plugin from source and installs it into OpenDeck (Flatpak or native).
# Requires Rust (https://rustup.rs). Users who just want the plugin can install the
# release zip from OpenDeck instead.
set -euo pipefail
cd "$(dirname "$0")"
PLUGIN=com.mrbretzel.keymacro.sdPlugin

ARCH=$(uname -m)                         # x86_64 or aarch64
TARGET="$ARCH-unknown-linux-musl"        # static executable: no system library needed
rustup target add "$TARGET" >/dev/null 2>&1 || true
cargo build --release --locked --target "$TARGET"
cp "target/$TARGET/release/keymacro" "$PLUGIN/keymacro-linux-$ARCH"

FLATPAK_DIR="$HOME/.var/app/me.amankhanna.opendeck/config/opendeck/plugins"
NATIVE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/opendeck/plugins"
if [ -d "$FLATPAK_DIR" ]; then DEST="$FLATPAK_DIR"; else DEST="$NATIVE_DIR"; mkdir -p "$DEST"; fi
TARGET_DIR="$DEST/$PLUGIN"

rm -rf "$TARGET_DIR.new"
cp -r "$PLUGIN" "$TARGET_DIR.new"
rm -f "$TARGET_DIR.new/plugin.log"
# Keep an existing log file.
[ -f "$TARGET_DIR/plugin.log" ] && cp "$TARGET_DIR/plugin.log" "$TARGET_DIR.new/" || true
rm -rf "$TARGET_DIR" && mv "$TARGET_DIR.new" "$TARGET_DIR"
echo "Installed to: $TARGET_DIR"
echo "Restart OpenDeck to load the plugin."
