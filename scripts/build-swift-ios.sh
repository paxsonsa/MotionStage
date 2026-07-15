#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE_MANIFEST="$ROOT_DIR/crates/motionstage-sdk-swift/Cargo.toml"
LIB_BASENAME="libmotionstage_sdk_swift.a"
HEADER_DIR="$ROOT_DIR/crates/motionstage-sdk-swift/include"
BUILD_DIR="$ROOT_DIR/target/swift-ios"
DIST_XCFRAMEWORK="$ROOT_DIR/dist/MotionStageSwiftFFI.xcframework"
SPM_XCFRAMEWORK="$ROOT_DIR/swift/MotionStageClient/Artifacts/MotionStageSwiftFFI.xcframework"
IOS_DEPLOYMENT_TARGET="${IOS_DEPLOYMENT_TARGET:-26.0}"

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

# `rustup target add` installs the iOS std libs into rustup's toolchain, but
# if a different Rust (Homebrew, a devtools install) shadows rustup's cargo on
# PATH, the build below uses THAT one and dies mid-compile with E0463
# ("can't find crate for `core`"). Catch the mismatch up front: the sysroot of
# the `rustc` cargo will invoke must live under a rustup toolchains directory.
ACTIVE_SYSROOT="$(rustc --print sysroot)"
if [[ "$ACTIVE_SYSROOT" != *"/toolchains/"* ]]; then
  echo "error: 'rustc' on PATH is not rustup-managed (sysroot: $ACTIVE_SYSROOT)."
  echo "       Cross-compiling to iOS needs rustup's toolchain. Fix with e.g.:"
  echo "         PATH=\"\$HOME/.cargo/bin:\$PATH\" $0"
  echo "       or 'brew unlink rust' if Homebrew's rust is shadowing rustup."
  exit 1
fi

rustup target add "${IOS_TARGETS[@]}"

for target in "${IOS_TARGETS[@]}"; do
  case "$target" in
    aarch64-apple-ios)
      target_rustflags="-C link-arg=-miphoneos-version-min=$IOS_DEPLOYMENT_TARGET"
      target_cflags="-miphoneos-version-min=$IOS_DEPLOYMENT_TARGET"
      ;;
    aarch64-apple-ios-sim|x86_64-apple-ios)
      target_rustflags="-C link-arg=-mios-simulator-version-min=$IOS_DEPLOYMENT_TARGET"
      target_cflags="-mios-simulator-version-min=$IOS_DEPLOYMENT_TARGET"
      ;;
    *)
      target_rustflags=""
      target_cflags=""
      ;;
  esac

  if [[ -n "${RUSTFLAGS:-}" ]]; then
    target_rustflags="${RUSTFLAGS} ${target_rustflags}"
  fi

  echo "Building motionstage-sdk-swift for $target..."
  target_env_suffix="${target//-/_}"
  target_cflags_key="CFLAGS_${target_env_suffix}"
  target_cxxflags_key="CXXFLAGS_${target_env_suffix}"

  env \
    "RUSTFLAGS=$target_rustflags" \
    "IPHONEOS_DEPLOYMENT_TARGET=$IOS_DEPLOYMENT_TARGET" \
    "$target_cflags_key=$target_cflags" \
    "$target_cxxflags_key=$target_cflags" \
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
