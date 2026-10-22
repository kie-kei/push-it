#!/bin/sh
# Removes the pushit CLI installed by scripts/install.sh
set -e
command -v cargo >/dev/null || { echo "cargo not found" >&2; exit 1; }
cargo uninstall pushit
