# Device Integrators

## Goal

Device integrators connect cameras, trackers, controllers, or companion apps to MotionStage’s server-authoritative runtime.

## Build + Test Path (Matrix)

| Phase | Goal | Commands | Pass Criteria |
|---|---|---|---|
| P1 | Validate runtime and protocol baseline | `cargo test -p motionstage-server -p motionstage-transport-quic -p motionstage-testkit` | Handshake, signaling, ingest, and soak tests pass |
| P2 | Dry run with built-in simulated device | `cargo run -p motionstage-cli -- simulate --server-bind 127.0.0.1:0 --sample-hz 120` then `start`, `status`, `record start recordings/device-dryrun.cmtrk`, `record stop`, `quit` | Motion counters increase and recording is generated |
| P3 | Loopback transport dry run (real QUIC path) | `cargo test -p motionstage-server quic_runtime_accepts_session_and_ingests_motion -- --nocapture` and `cargo test -p motionstage-server quic_control_routes_and_drains_video_signals -- --nocapture` | QUIC control + datagram path passes end-to-end |
| P4 | Bring up your real device client | Point your client at `cargo run -p motionstage-cli -- serve` endpoint | Client reaches `Active` session state and motion updates apply |

## Quickest Path to First Device Integration

Use the local simulator first to validate your expected cadence and mapping behavior:

```bash
cargo run -p motionstage-cli -- simulate --server-bind 127.0.0.1:0 --sample-hz 120
```

Then switch to `serve` and replace the simulated client with your device-side client implementation.

## Dry Run Modes

Use these before real hardware bring-up.

### Dry Run A: Embedded simulator (fastest)

- Runs server + simulated motion source together.
- Simulated source uses real QUIC session flow (`ServerHello`/`ClientHello`/`RegisterRequest`) and motion datagrams.
- No network discovery required.
- Best for quick mapping/recording pipeline checks.

Command:

```bash
cargo run -p motionstage-cli -- simulate --server-bind 127.0.0.1:0 --sample-hz 120
```

Smoke commands:

```text
start
status
record start recordings/device-dryrun.cmtrk
record stop
quit
```

### Dry Run B: Local QUIC loopback (transport validation)

- Uses real QUIC control/datagram flow on localhost.
- Validates your assumptions about handshake + ingest + signaling behavior.

Commands:

```bash
cargo test -p motionstage-server quic_runtime_accepts_session_and_ingests_motion -- --nocapture
cargo test -p motionstage-server quic_control_routes_and_drains_video_signals -- --nocapture
```

### Dry Run C: Client-only simulate against an existing server

- Runs only the simulated client against your already running server.
- Does not create scenes or mappings on the server.
- Requires server-side scene/mapping/mode to be configured before streaming.

Command:

```bash
cargo run -p motionstage-cli -- simulate --connect 127.0.0.1:7788 --output-attribute demo.position
# or discover via mDNS (single-server auto-select):
cargo run -p motionstage-cli -- simulate --connect discover --output-attribute demo.position
# or select a specific discovery service:
cargo run -p motionstage-cli -- simulate --connect discover:motionstage-blender --output-attribute demo.position
```

Smoke commands:

```text
start
status
quit
```

## Transport Contract

- Discovery: mDNS `_motionstage._udp.local.`
- Transport: QUIC
- Control channel: bi-directional stream (`ControlMessage`)
- Motion channel: QUIC datagrams (`MotionDatagram`)
- Wire compatibility: major/minor protocol validation on decode

## Handshake Contract

Expected control sequence:

1. Receive `ServerHello`
2. Send `ClientHello` including advertised output attributes for `MotionSource` roles
3. Send `RegisterRequest`
4. Receive `RegisterAccepted` or `RegisterRejected`
5. Session enters active loop for ping/signaling/control

A session must reach `Active` before it can participate in signaling and motion/video operations.

## Roles and Features

Roles:
- `MotionSource`
- `CameraController`
- `VideoSink`
- `Operator`

Features:
- `Motion`
- `Mapping`
- `Recording`
- `Video`
- `Hdr10`
- `SdrFallback`

Registration negotiates intersection between client-declared and server-supported features.

## Motion Ingest Contract

`MotionDatagram` payload:
- `device_id`
- `timestamp_ns`
- `updates[]` (attribute key + typed value)

Server behavior:
- Rejects updates without active mapping
- Applies component-mask transform and filter chain
- Increments ingest metrics (`motion_datagrams`, `motion_updates`)

## Video/Signaling Contract

Control messages include:
- `CreateVideoOffer { stream_id, track_id }`
- `VideoOffer(SdpMessage)`
- `VideoSignal(SignalMessage)`
- `DrainSignals` / `SignalsBatch`

Signaling can be:
- Server-owned peer flow (device signals to itself for offer/answer lifecycle)
- Device-to-device routed flow via server queue

## Security and Admission

Security modes:
- `trusted_lan`
- `pairing_required`
- `api_key`
- `api_key_plus_pairing`

Registration failures return explicit reject codes (auth, capacity, compatibility, role/feature mismatch).

## Practical Integration Checklist

- Implement protocol version negotiation fallback on minor versions.
- Send heartbeat-like motion traffic at stable cadence.
- Keep source output names stable and fully-qualified as
  `<device-id>.<attr>[.<component>]` because mappings are key-based.
- Handle `Error` control messages as actionable protocol failures.
- Reconnect by repeating full handshake and registration.
