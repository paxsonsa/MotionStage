# iOS Integrators

## Goal

Provide a repeatable build process that exposes MotionStage's Rust QUIC client to iOS apps through a Swift API.

## Build + Validation Matrix

| Phase | Goal | Commands | Pass Criteria |
|---|---|---|---|
| P1 | Build Rust Swift-FFI crate | `cargo test -p motionstage-sdk-swift` | FFI crate tests pass and handshake smoke test succeeds. |
| P2 | Build iOS slices | `./scripts/build-swift-ios.sh` | Static libs are built for `aarch64-apple-ios`, `aarch64-apple-ios-sim`, and `x86_64-apple-ios`. |
| P3 | Create XCFramework | `./scripts/build-swift-ios.sh` | `dist/MotionStageSwiftFFI.xcframework` is generated successfully. |
| P4 | Expose Swift client API | `cd swift/MotionStageClient && xcodebuild -scheme MotionStageClient -destination 'generic/platform=iOS Simulator' build` | Swift package compiles for iOS simulator and exports `MotionStageClient`. |
| P5 | Device-app runtime smoke | Connect to running server and send test samples | Session registers (`RegisterAccepted`) and motion datagrams are sent without transport errors. |

## Local Build Command

```bash
./scripts/build-swift-ios.sh
```

This script performs all of the following:

- installs required Rust iOS targets
- builds `motionstage-sdk-swift` for device + simulator architectures
- merges simulator slices with `lipo`
- creates `MotionStageSwiftFFI.xcframework` via `xcodebuild -create-xcframework`
- copies the XCFramework into `swift/MotionStageClient/Artifacts/` for SwiftPM usage

## Swift Package Entry Point

Use the wrapper package at:

- `swift/MotionStageClient`

Primary API surface:

- `MotionStageClient(deviceName:outputAttribute:)`
- `connect(serverAddress:pairingToken:apiKey:)`
- `sendPosition(x:y:z:)`
- `setMode(_:)`
- `sessionID`, `deviceID`, `lastErrorMessage`

## FFI Contract

Public C header:

- `crates/motionstage-sdk-swift/include/motionstage_swift.h`

Stable status/mode constants are defined in the header and mirrored in Swift.

## Runtime Notes

- Server endpoint format is `host:port`.
- Source output attributes are auto-qualified to `<device-id>.<attribute>` if not already qualified.
- The client currently uses local-dev insecure certificate verification (`QuicClient::new_insecure_for_local_dev`) to match existing simulator behavior. Use production certificate validation before shipping public builds.
