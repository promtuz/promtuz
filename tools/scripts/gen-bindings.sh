#!/bin/sh
# Generate uniffi client bindings from libcore. Run from anywhere in the repo.
#   ./tools/scripts/gen-bindings.sh            # both kotlin + swift
#   ./tools/scripts/gen-bindings.sh kotlin     # just one
#
# Android normally generates Kotlin in-build (the `generateUniffiBindings`
# Gradle task). This script is for manual regen, CI, and the iOS/Swift side.
# It emits binding SOURCE only; assembling the iOS XCFramework (cargo build
# --target aarch64-apple-ios{,-sim} + xcodebuild -create-xcframework) is a
# macOS step layered on top.
set -eu

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

LANGS="${*:-kotlin swift}"
OUT="$ROOT/target/bindings"

echo "building libcore cdylib (host) for uniffi metadata..."
cargo build -q -p core

LIB="$ROOT/target/debug/libcore.so"
[ -f "$LIB" ] || LIB="$ROOT/target/debug/libcore.dylib" # macOS host

for lang in $LANGS; do
    echo "generating $lang -> $OUT/$lang"
    cargo run -q -p uniffi-bindgen -- generate \
        --library "$LIB" \
        --language "$lang" \
        --out-dir "$OUT/$lang"
done

echo "done. bindings under $OUT/"
