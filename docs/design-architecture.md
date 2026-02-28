# Design and Architecture

## Design Goals

- Server-authoritative runtime state
- Deterministic mapping and export behavior
- Separation of control plane, data plane, and DCC outputs
- Transport/version gates at decode boundaries
- Integrator-friendly surfaces for Rust and Python

## System Topology

CineMotion is composed of small crates with clear ownership boundaries.

- `cinemotion-server`: orchestration and lifecycle owner
- `cinemotion-core`: mode/scenes/mappings/update application
- `cinemotion-protocol`: cross-transport protocol model
- `cinemotion-transport-quic`: wire transport implementation
- `cinemotion-media` + `cinemotion-webrtc`: video negotiation/signaling/session glue
- `cinemotion-recording`: binary track format and index/read/write APIs
- `cinemotion-export-usd` + `cinemotion-export-chan`: deterministic DCC outputs
- `python/cinemotion_sdk`: OOP integrator API with optional native bridge

## Runtime Ownership Model

`ServerHandle` owns process-level runtime resources:

- QUIC runtime accept loop
- Discovery publisher lifecycle
- Scheduler loops (`tick_hz`, `publish_hz`)
- Session table and server metrics
- Optional active recording writer

This keeps integration logic outside of transport internals and ensures startup/shutdown is explicit via `start()` and `stop()`.

## Control Plane vs Data Plane

- Control plane: QUIC bidirectional stream carrying `ControlMessage` envelopes (`ServerHello`, registration, signaling, ping/pong, errors)
- Data plane: QUIC datagrams carrying `MotionDatagram` updates

Both planes are protocol-version tagged (`protocol_major`, `protocol_minor`) and validated at decode time.

## Authoritative State Machine

Session transitions:
- `Discovered`
- `TransportConnected`
- `HelloExchanged`
- `Authenticated`
- `Registered`
- `SceneSynced`
- `Active`
- `Closed`

Mode transitions:
- `Idle` <-> `Live`
- `Live` <-> `Recording`
- `Recording` -> `Idle`

Mapping mutations are blocked in `Recording` mode to preserve deterministic captures.

## Mapping and Transform Engine

The core runtime enforces:

- One active owner per target attribute (with lease/reclaim policy)
- Optional `component_mask` transforms
- Filter chains (`Passthrough`, `Ema`, `Deadband`, `Clamp`)

Supported transform patterns:
- Scalar source -> selected vector components
- Vector source component -> scalar target
- Vector subset copy -> vector target

## Recording Architecture

Recording is a server-owned writer pipeline.

- Canonical format: `CMTRK2`
- Backward read support: `CMTRK1`
- Captures frame data plus marker timeline (`ModeTransition`, mapping create/update/remove/lock)

Recording starts by forcing runtime mode into `Recording` and ends by returning to `Live`.

## Video Architecture

Video uses server-owned WebRTC peer sessions while signaling rides on QUIC control messages.

- DCC publishes a master descriptor (`width`, `height`, `fps`, dynamic range metadata)
- Clients negotiate capability against that descriptor
- HDR10 streams can fallback to SDR (`Hdr10ToSdr`) when required

## Security Model

Supported admission policies:
- `trusted_lan`
- `pairing_required`
- `api_key`
- `api_key_plus_pairing`

Admission and capacity checks happen during registration. Rejections emit explicit `RejectCode` outcomes.
