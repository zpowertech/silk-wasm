#!/usr/bin/env bash
#
# Build the SILK WebAssembly module and place it where web/silk.js expects it.
#
# The module is built with nothing but the Rust wasm32 target — no Emscripten, no
# wasm-bindgen CLI, no network access needed after the first dependency fetch.
#
# Usage:
#   ./build.sh            # release build (default)
#   ./build.sh --debug    # faster build, larger and slower module

set -euo pipefail

cd "$(dirname "$0")"

PROFILE_DIR="release"
CARGO_FLAGS=(--release)
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE_DIR="debug"
  CARGO_FLAGS=()
fi

# Keep the Cargo registry inside the project so the build is self-contained and
# never depends on (or writes to) a machine-global ~/.cargo.
export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo-home}"

# rustup does not always know about the target (for example when the standard
# library was installed directly into the sysroot), so probe the sysroot instead.
SYSROOT="$(rustc --print sysroot)"
if [[ ! -d "$SYSROOT/lib/rustlib/wasm32-unknown-unknown" ]]; then
  echo "==> wasm32-unknown-unknown standard library not found in $SYSROOT"
  echo "    installing via rustup (this needs network access)"
  rustup target add wasm32-unknown-unknown
fi

echo "==> building silk-core-wasm (${PROFILE_DIR})"
cargo build -p silk-core-wasm --target wasm32-unknown-unknown "${CARGO_FLAGS[@]}"

WASM="target/wasm32-unknown-unknown/${PROFILE_DIR}/silk_core_wasm.wasm"
cp "$WASM" web/silk_core.wasm

BYTES=$(wc -c < web/silk_core.wasm | tr -d ' ')
echo "==> wrote web/silk_core.wasm (${BYTES} bytes)"
echo "==> open web/index.html over HTTP to try it, e.g.:"
echo "      python3 -m http.server 8000 --directory web"
