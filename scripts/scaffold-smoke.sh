#!/usr/bin/env bash
# Verify the distributed scaffold, including its deploy entry, with no WIT registry.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
binary=${PUSHIT_BIN:-"$repo/target/debug/pushit"}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export XDG_CACHE_HOME="$work/cache"
cd "$work"
"$binary" init scaffold-smoke
cd scaffold-smoke/fn
# The scaffold manifest intentionally points at its own target directory.
unset CARGO_TARGET_DIR
# Fetch only Rust dependencies before disabling network resolution. Local WIT
# must be sufficient for cargo-component's subsequent offline build.
# cargo-component requires a lock file even when every WIT dependency is local.
printf 'version = 1\n' > Cargo-component.lock
cargo fetch
cargo component build --release --target wasm32-wasip1 --offline
cd ..
python3 - <<'PY'
import pathlib
manifest = pathlib.Path('pushit.yaml').read_text()
entry = next(line.removeprefix('entry: ') for line in manifest.splitlines() if line.startswith('entry: '))
artifact = pathlib.Path(entry).read_bytes()
# Component binary header, not a bare WASI Preview 1 core module.
assert artifact[:8] == b'\x00asm\x0d\x00\x01\x00', artifact[:8]
assert 'wasm32-wasip1' in entry
print(f'Scaffold component matches deploy entry: {entry} ({len(artifact)} bytes)')
PY
