#!/bin/bash
# Build outside the package and install to ~/.local/bin/utsurigi-memory.
set -euo pipefail
if [ -f "$HOME/.cargo/env" ]; then source "$HOME/.cargo/env"; fi
HERE="$(cd "$(dirname "$0")" && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/utsurigi-memory-target}"
cargo build --locked --release --manifest-path "$HERE/Cargo.toml"
mkdir -p "$HOME/.local/bin"
install -m 755 "$CARGO_TARGET_DIR/release/utsurigi-memory" "$HOME/.local/bin/utsurigi-memory"
echo "installed $HOME/.local/bin/utsurigi-memory"
