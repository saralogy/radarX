#!/usr/bin/env bash
# Generate Swift and Kotlin bindings from the compiled library (UniFFI library mode).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -p sd-ffi
lib=target/debug/libsurvival_disc.so
[ -f "$lib" ] || lib=target/debug/libsurvival_disc.dylib
cargo run -q -p sd-ffi --features bindgen --bin uniffi-bindgen -- generate \
  --library "$lib" --language kotlin --no-format --out-dir mobile/android/app/src/main/kotlin
cargo run -q -p sd-ffi --features bindgen --bin uniffi-bindgen -- generate \
  --library "$lib" --language swift --no-format --out-dir mobile/ios/Generated
