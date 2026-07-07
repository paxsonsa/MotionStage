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

- `MotionStageClient(deviceName:outputAttribute:)` / `MotionStageClient(deviceName:outputAttributes:)`
- `connect(serverAddress:pairingToken:apiKey:)` / `connect(serverAddress:fingerprint:pairingToken:apiKey:)`
- `sendPosition(x:y:z:)`, `sendVec3(attribute:value:)`, `sendQuaternion(attribute:value:)`, `sendFloat(attribute:value:)`, `sendBatch(_:)`
- `setDataFlow(_:)`, `setRecording(_:)`, `resetScene()`
- `videoStreamStatus()`, `createVideoOffer(streamID:trackID:)`, `sendVideoSdp(type:sdp:)`, `sendVideoIce(candidate:sdpMid:sdpMLineIndex:)`, `nextVideoSignal()`
- `sessionID`, `deviceID`, `lastErrorMessage`
- `events: AsyncStream<ConnectionEvent>` — connection lifecycle

### Operator plane (protocol 2.1)

- `stateEvents: AsyncStream<StateEventUpdate>` — replicated server state:
  `.event(StateEventEnvelope)` for every `StateEvent` (your own mutations echo
  back with `originSession == sessionID`; there is no echo suppression) and
  `.snapshot(SceneSnapshot)` for unsolicited full-world snapshots (handshake,
  resync, lag recovery).
- `createMapping(sourceOutput:targetObject:targetAttribute:sourceDevice:targetScene:componentMask:)`
  — `sourceDevice` nil = own device, `targetScene` nil = active scene; returns `MappingSummary`.
- `updateMapping(mappingID:sourceOutput:targetObject:targetAttribute:sourceDevice:targetScene:componentMask:)`
  — full replacement; returns the updated `MappingSummary`.
- `removeMapping(mappingID:)`, `setMappingLock(mappingID:lock:)`
- `startTake() -> UUID`, `stopTake() -> TakeInfo` (Operator role required)
- `sceneSnapshot() -> SceneSnapshot` — on-demand world snapshot for target pickers
- Typed server rejections throw `MotionStageError.operationRejected(code: RejectCode, reason: String)`.
