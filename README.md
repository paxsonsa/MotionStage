<p align="center">
  <img src="docs/assets/motionstage-icon.svg" width="144" alt="MotionStage icon" />
</p>

# MotionStage

<p align="center">
  <strong>Server-authoritative runtime for virtual camera and motion workflows.</strong>
</p>

<p align="center">
  <a href="https://github.com/paxsonsa/motionstage/actions/workflows/rust.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/paxsonsa/motionstage/rust.yml?label=ci&logo=github" /></a>
  <img alt="Version 0.1.0" src="https://img.shields.io/badge/version-0.1.0-0ea5e9" />
  <img alt="Rust 2021" src="https://img.shields.io/badge/rust-2021-000000?logo=rust" />
  <img alt="Python 3.10+" src="https://img.shields.io/badge/python-3.10%2B-3776AB?logo=python&logoColor=white" />
  <img alt="License MIT" src="https://img.shields.io/badge/license-MIT-16a34a" />
</p>

## What MotionStage Is

MotionStage provides a deterministic motion runtime where the server owns scene state, mapping, and recording behavior. It is designed for:

- virtual camera ingest from devices over QUIC
- real-time mapping and filter pipelines for scene attributes
- deterministic `.cmtrk` capture and export to USD/CHAN
- integration across Rust crates and Python-based DCC adapters

## Why MotionStage Exists

Virtual production tooling is often expensive, fragmented, or built for large studios. MotionStage exists to make the same core workflow available to everyday artists, small teams, and indie creators.

The goal is practical access:

- capture and stream motion without studio-scale infrastructure
- keep data portable with open, deterministic recording and export paths
- let artists connect familiar DCC tools through clear SDK and adapter contracts
- make iterative VP workflows reproducible on normal development hardware

MotionStage focuses on the runtime layer so creators can spend less time wiring systems together and more time making shots.

## Get Started

### Interactive demo (fastest path)

```bash
cargo run -p motionstage-cli -- simulate --server-bind 127.0.0.1:0 --sample-hz 120
```

Then in `motionstage-sim>`:

```text
start
record start recordings/demo.cmtrk
status
record stop
quit
```

This gives you a local runtime, live motion samples, and a recording exportable by integrator crates.
`simulate` runs an embedded server and a simulated client that connects through the same QUIC
handshake/control/datagram path used by real devices.
`--server-bind` controls the embedded server bind address; it is not a remote server connect flag.

### Simulated client to an existing server

```bash
cargo run -p motionstage-cli -- simulate --connect 127.0.0.1:7788 --output-attribute demo.position
```

This runs in client-only mode and does not start an embedded server. Ensure scene/mapping/mode are
already configured on the target server.
Source outputs are normalized to fully-qualified IDs:
`<device-id>.<attr>[.<component>]`.

You can also discover a server over mDNS:

```bash
# auto-connect when exactly one server is discoverable
cargo run -p motionstage-cli -- simulate --connect discover

# select a specific server by advertised discovery name
cargo run -p motionstage-cli -- simulate --connect discover:motionstage-blender
```

### Server mode (for real clients)

```bash
cargo run -p motionstage-cli -- serve
```

This starts QUIC control/datagram ingest, mDNS discovery, and scheduler loops.

## Runtime Flow

1. Device discovers and connects over QUIC.
2. Control handshake negotiates versions/features and opens a session.
3. Server applies mapping transforms and filters to incoming attributes.
4. Runtime publishes snapshots for downstream systems.
5. Recording mode persists `.cmtrk` (`CMTRK2`) events.
6. Export crates produce deterministic USD or CHAN output.

## Docs

- [Design and Architecture](docs/design-architecture.md)
- [Concepts and Workflow](docs/concepts-workflow.md)
- [DCC Integrators](docs/dcc-integrators.md)
- [Device Integrators](docs/device-integrators.md)
- [Protocol Overview](docs/protocol.md)
- [Hardening Gates](docs/hardening.md)
- [Completion Matrix](docs/tasks.md)

## Workspace Layout

- `crates/motionstage-core`: scene/mapping/mode model, transform/filter engine
- `crates/motionstage-server`: authoritative lifecycle, sessions, scheduling, recording
- `crates/motionstage-protocol`: wire contracts, role/features, control negotiation
- `crates/motionstage-transport-quic`: QUIC transport, control streams, motion datagrams
- `crates/motionstage-discovery`: mDNS advertisement/browser (`_motionstage._udp.local`)
- `crates/motionstage-media`: video descriptor model and signaling queue
- `crates/motionstage-webrtc`: server-owned WebRTC helpers
- `crates/motionstage-recording`: `.cmtrk` read/write/index (`CMTRK1` + `CMTRK2`)
- `crates/motionstage-export-usd`: deterministic USD text exporter
- `crates/motionstage-export-chan`: deterministic CHAN exporter
- `crates/motionstage-cli`: `serve` and `simulate` workflows
- `crates/motionstage-testkit`: integration harness and soak helpers
- `python/motionstage_sdk`: strict OOP delegate SDK backed by the native Rust bridge
- `python/blender_adapter`: reference Blender delegate adapter

## Validate

```bash
cargo test --workspace
python -m pip install -e ./python
python -m pytest -q python/tests
```
