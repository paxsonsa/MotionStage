# CineMotion v1

CineMotion is a server-authoritative runtime for virtual camera and motion workflows.

## Get Started Fast

### Fastest path (single command, interactive demo)

This path gets you to a working motion stream and recording in minutes.

```bash
cargo run -p cinemotion-cli -- simulate --bind 127.0.0.1:7788 --sample-hz 120
```

Then in the `cinemotion-sim>` prompt:

```text
start
record start recordings/demo.cmtrk
status
record stop
quit
```

What you get:
- A local CineMotion runtime
- A demo scene and mapping (`demo.position` -> camera `position`)
- Live `vec3` motion samples
- A `.cmtrk` recording you can export with the DCC integrator crates

### Server-only path (for real device clients)

```bash
cargo run -p cinemotion-cli -- serve
```

This starts the runtime, QUIC control/datagram ingest, mDNS discovery, and scheduler loops.

## How It Works (90 seconds)

1. A device discovers and connects to CineMotion over QUIC.
2. The control handshake negotiates protocol/features and creates a session.
3. The server owns scene state, mapping rules, and runtime mode (`Idle`, `Live`, `Recording`).
4. Motion sources send datagrams with attribute updates.
5. CineMotion applies mapping transforms and filters, then publishes snapshots.
6. In recording mode, CineMotion writes `.cmtrk` (`CMTRK2`) with frame and marker events.
7. DCC integrators export recordings to deterministic USD or CHAN text output.

## Documentation

- [Design and Architecture](docs/design-architecture.md)
- [Concepts and Workflow](docs/concepts-workflow.md)
- [DCC Integrators](docs/dcc-integrators.md)
- [Device Integrators](docs/device-integrators.md)
- [Protocol Overview](docs/protocol.md)
- [Hardening Gates](docs/hardening.md)
- [Completion Matrix](docs/tasks.md)

### Integrator Quick Paths

- DCC integrators: follow [DCC Integrators](docs/dcc-integrators.md) `Build + Test Path (Matrix)` to generate a fixture recording, validate adapter contracts, and assert deterministic USD/CHAN output.
- Device integrators: follow [Device Integrators](docs/device-integrators.md) `Build + Test Path (Matrix)` and start with `Dry Run A` before hardware bring-up.

## Workspace Layout

- `crates/cinemotion-core`: runtime scene/mapping/mode model and transform/filter engine
- `crates/cinemotion-server`: authoritative server lifecycle, session state machine, scheduling, recording
- `crates/cinemotion-protocol`: wire contracts, roles/features, control messages, version negotiation
- `crates/cinemotion-transport-quic`: QUIC transport, control streams, motion datagrams
- `crates/cinemotion-discovery`: mDNS advertisement/browser (`_cinemotion._udp.local`)
- `crates/cinemotion-media`: video descriptor model, HDR10/SDR negotiation, signaling queue
- `crates/cinemotion-webrtc`: server-owned WebRTC peer/session helpers
- `crates/cinemotion-recording`: `.cmtrk` read/write/index support (`CMTRK1` + `CMTRK2`)
- `crates/cinemotion-export-usd`: deterministic USD text exporter
- `crates/cinemotion-export-chan`: deterministic CHAN exporter
- `crates/cinemotion-cli`: `serve` and `simulate` workflows
- `crates/cinemotion-testkit`: integration harness and soak helpers
- `python/cinemotion_sdk`: strict OOP delegate SDK and optional native Rust bridge
- `python/blender_adapter`: reference Blender delegate adapter

## Validation

```bash
cargo test
python -m pip install -e ./python
python -m pytest -q python/tests
```
