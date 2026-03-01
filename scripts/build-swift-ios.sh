#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE_MANIFEST="$ROOT_DIR/crates/motionstage-sdk-swift/Cargo.toml"
LIB_BASENAME="libmotionstage_sdk_swift.a"
HEADER_DIR="$ROOT_DIR/crates/motionstage-sdk-swift/include"
BUILD_DIR="$ROOT_DIR/target/swift-ios"
DIST_XCFRAMEWORK="$ROOT_DIR/dist/MotionStageSwiftFFI.xcframework"
SPM_XCFRAMEWORK="$ROOT_DIR/swift/MotionStageClient/Artifacts/MotionStageSwiftFFI.xcframework"

IOS_TARGETS=(
  "aarch64-apple-ios"
  "aarch64-apple-ios-sim"
  "x86_64-apple-ios"
)

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: iOS XCFramework build requires macOS (xcodebuild + lipo)."
  exit 1
fi

for cmd in cargo rustup xcodebuild lipo; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: required command not found: $cmd"
    exit 1
  fi
done

rustup target add "${IOS_TARGETS[@]}"

for target in "${IOS_TARGETS[@]}"; do
  echo "Building motionstage-sdk-swift for $target..."
  cargo build \
    --manifest-path "$CRATE_MANIFEST" \
    --release \
    --target "$target" \
    --locked
done

DEVICE_LIB="$ROOT_DIR/target/aarch64-apple-ios/release/$LIB_BASENAME"
SIM_ARM64_LIB="$ROOT_DIR/target/aarch64-apple-ios-sim/release/$LIB_BASENAME"
SIM_X86_64_LIB="$ROOT_DIR/target/x86_64-apple-ios/release/$LIB_BASENAME"
SIM_UNIVERSAL_LIB="$BUILD_DIR/$LIB_BASENAME"

for file in "$DEVICE_LIB" "$SIM_ARM64_LIB" "$SIM_X86_64_LIB"; do
  if [[ ! -f "$file" ]]; then
    echo "error: expected build artifact not found: $file"
    exit 1
  fi
done

mkdir -p "$BUILD_DIR" "$ROOT_DIR/dist" "$(dirname "$SPM_XCFRAMEWORK")"

lipo -create \
  -output "$SIM_UNIVERSAL_LIB" \
  "$SIM_ARM64_LIB" \
  "$SIM_X86_64_LIB"

rm -rf "$DIST_XCFRAMEWORK" "$SPM_XCFRAMEWORK"

xcodebuild -create-xcframework \
  -library "$DEVICE_LIB" -headers "$HEADER_DIR" \
  -library "$SIM_UNIVERSAL_LIB" -headers "$HEADER_DIR" \
  -output "$DIST_XCFRAMEWORK"

cp -R "$DIST_XCFRAMEWORK" "$SPM_XCFRAMEWORK"

echo "Created XCFramework: $DIST_XCFRAMEWORK"
echo "Updated Swift package artifact: $SPM_XCFRAMEWORK"
