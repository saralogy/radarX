#!/usr/bin/env bash
# Build SurvivalDiscCore.xcframework. Needs macOS + Xcode and
# `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`.
set -euo pipefail
cd "$(dirname "$0")/.."
./scripts/gen-bindings.sh
for t in aarch64-apple-ios aarch64-apple-ios-sim; do
  cargo build --release -p sd-ffi --target "$t"
done
h=mobile/ios/Generated/headers
mkdir -p "$h"
cp mobile/ios/Generated/survival_discFFI.h "$h/"
cp mobile/ios/Generated/survival_discFFI.modulemap "$h/module.modulemap"
rm -rf mobile/ios/SurvivalDiscCore.xcframework
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libsurvival_disc.a -headers "$h" \
  -library target/aarch64-apple-ios-sim/release/libsurvival_disc.a -headers "$h" \
  -output mobile/ios/SurvivalDiscCore.xcframework
