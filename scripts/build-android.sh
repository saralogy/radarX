#!/usr/bin/env bash
# Cross-compile the core for Android. Needs: Android NDK, `cargo install cargo-ndk`,
# `rustup target add aarch64-linux-android armv7-linux-androideabi`.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo ndk -t arm64-v8a -t armeabi-v7a --platform 24 \
  -o mobile/android/app/src/main/jniLibs build --release -p sd-ffi
./scripts/gen-bindings.sh
