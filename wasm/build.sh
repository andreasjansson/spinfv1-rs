#!/bin/sh
# Build spinfv1 as a browser WebAssembly module.
#
#   wasm/build.sh [OUT]     (default: wasm/dist/fv1.wasm)
#
# Needs Rust 1.85 or later with the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/dist/fv1.wasm}
cargo build --release --target wasm32-unknown-unknown --manifest-path "$here/Cargo.toml"
mkdir -p "$(dirname "$out")"
cp "$here/target/wasm32-unknown-unknown/release/spinfv1_wasm.wasm" "$out"
echo "wrote $out ($(wc -c < "$out" | tr -d ' ') bytes)"
