#!/bin/sh
# Linux launcher for the Keyboard Macros plugin.
#
# Dependency: Python 3.8 or newer (standard library only, no pip package).
# OpenDeck does not ship Python: it must be installed on the system
# (Debian/Ubuntu: apt install python3, Fedora: dnf install python3,
# Arch: pacman -S python). The OpenDeck Flatpak runtime already includes it.
#
# This script finds a suitable interpreter and starts plugin.py; if there is none,
# it reports the missing dependency instead of failing silently.

# Plugin folder, found without external tools.
DIR=${0%/*}
[ "$DIR" = "$0" ] && DIR=.

for py in python3 python; do
	if command -v "$py" >/dev/null 2>&1 \
		&& "$py" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' 2>/dev/null; then
		exec "$py" "$DIR/plugin.py" "$@"
	fi
done

MSG="Keyboard Macros: Python 3.8 or newer is required but was not found. Install it with your package manager (for example: apt install python3, dnf install python3, pacman -S python), then restart OpenDeck."
TS=$(date +%H:%M:%S 2>/dev/null)
echo "$TS ERROR $MSG" >> "$DIR/plugin.log" 2>/dev/null
echo "$MSG" >&2
exit 1
