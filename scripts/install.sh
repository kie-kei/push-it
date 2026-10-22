#!/bin/sh
# Installs the pushit CLI into ~/.cargo/bin (requires Rust: https://rustup.rs)
set -e
command -v cargo >/dev/null || {
    echo "cargo not found. Install Rust first: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
    exit 1
}
echo "Installing pushit (builds from source, may take a few minutes)..."
cargo install --git https://github.com/kie-kei/push-it pushit
command -v pushit >/dev/null && echo "Done: pushit installed to $(command -v pushit)"
