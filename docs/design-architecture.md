# Design and Architecture

## Design Goals

- Server-authoritative runtime state
- Deterministic mapping and export behavior
- Separation of control plane, data plane, and DCC outputs
- Transport/version gates at decode boundaries
- Integrator-friendly surfaces for Rust, Python, and Swift
- One session/protocol model for every participant, regardless of transport

## Topology and Scope

The mental model is a **multiplayer game server**.

- The runtime is the authoritative simulation. All state lives in it; all
  changes flow through it; it replicates state changes out to everyone.
- Devices (phones, tablets, trackers) and the DCC are all **players**.
- The DCC is **the host**: a special player that authors the scene
  (object/attribute definitions), renders outputs, and runs the server
  in-process — a listen server. Host privileges are expressed as
  roles/capabilities (e.g. `SceneAuthor`), not as a separate API with
  different semantics.
- There is **one session model with two transports**: in-process for the
  host, QUIC for remote players. No state or notification may exist that
  only one transport can observe. The host's bridge registers a real
  session, holds roles, and consumes the same event stream as every other
  player.
- Joining follows the game pattern: connect → handshake → **initial world
  snapshot** (scene graph, mappings, mode, current sequence number) → ordered
  **delta replication** (state events). Reconnect is a rejoin: present the
  last seen sequence number, receive replay or a fresh snapshot.

Scope decisions:

- **Single stage, single host.** One DCC per session; multi-DCC is out of
  scope.
- **Devices are concurrent operators.** A single artist with a phone/iPad can
  run an almost-complete mocap session from the device: browse the scene,
  bind their device to targets, control data flow and recording, manage
  takes. Scene authoring remains a host capability.
- **Conflict handling stays lightweight.** Mapping contention is arbitrated
  by the exclusive-owner-per-target-attribute lease model. Mode/recording
  writes are last-write-wins, made safe by immediate event fan-out carrying
  the originating session.
- **Mode model is two axes**, `data_flow: on|off` and `recording: on|off`,
  replacing the earlier `Idle/Live/Recording` tristate. `recording=on`
  requires `data_flow=on`; mapping mutations are blocked while recording is
  active. (Sections below that reference the tristate describe the current
  implementation and migrate under this model.)
- **DCC-viewport→device video is a core feature**, not an experiment.

## System Topology

MotionStage is composed of small crates with clear ownership boundaries.

- `motionstage-server`: orchestration and lifecycle owner
- `motionstage-core`: mode/scenes/mappings/update application
- `motionstage-protocol`: cross-transport protocol model
- `motionstage-transport-quic`: wire transport implementation
- `motionstage-media` + `motionstage-webrtc`: video negotiation/signaling/session glue
- `motionstage-recording`: binary track format and index/read/write APIs
- `motionstage-export-usd` + `motionstage-export-chan`: deterministic DCC outputs
- `python/motionstage_sdk`: OOP integrator API backed by the native bridge

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
- Server-authoritative relative composition from per-attribute baseline (`default_value`)
- Filter chains (`Passthrough`, `Ema`, `Deadband`, `Clamp`)

Supported transform patterns:
- Scalar source -> selected vector components
- Vector source component -> scalar target
- Vector subset copy -> vector target

Relative composition behavior:
- scalar/vector: `output = baseline + delta`
- quaternion: `output = normalize(baseline * delta)`
- matrix: `output = baseline * delta`
- non-composable semantic types use absolute assignment

Baseline control actions:
- `ResetSceneToBaseline`
- `CommitSceneBaseline`
- `CommitObjectBaseline`

These operations are explicit control-plane actions and are not tied to mode transitions.

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
