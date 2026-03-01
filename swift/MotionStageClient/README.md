# MotionStageClient Swift Package

This package provides an ergonomic Swift wrapper around the Rust-based MotionStage iOS client bindings.

## Build Artifacts

Before using this package, generate the XCFramework artifact:

```bash
./scripts/build-swift-ios.sh
```

The script writes the required artifact to:

- `swift/MotionStageClient/Artifacts/MotionStageSwiftFFI.xcframework`

## API

- `MotionStageClient(deviceName:outputAttribute:)`
- `connect(serverAddress:pairingToken:apiKey:)`
- `sendPosition(x:y:z:)`
- `setMode(_:)`
- `sessionID`, `deviceID`, `lastErrorMessage`
