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
- `Live` <-> `Playback`
- `Playback` -> `Idle`

Mapping mutations are blocked in `Recording` mode to preserve deterministic captures.
Runtime ingest is ignored in `Playback` mode so take review remains deterministic.

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
Each completed recording is registered as a take in a server-owned catalog (`take_id`, scene, path, frame count, selection state).

## Take Playback and Bake Cursor

- Take playback is server-authoritative and applies recorded frame values back into runtime scene attributes.
- Bake cursors provide pull-based frame iteration (`captured` timing or `fixed:<fps>` resampling) for DCC integrations to bake keys frame-by-frame.
- Take deletion uses tombstone + immediate purge of underlying `.cmtrk` and catalog row.

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

### Identity and the trust boundary

The `ClientHello` is **self-asserted**: a peer chooses its own `device_id` and
declares its own `roles` (including `Operator`). These claims are the *input*
to admission, never the basis for later authorization. The trust boundary is:

- **Identity is the authenticated session, not repeated self-claims.** At
  registration the server records the session's `device_id` and *admitted*
  `roles` in its `SessionInfo`. Every operator-plane permission check
  (own-source mapping management, the `Operator` gate on mode/recording/take
  control, baseline actions) reads that server-side record via
  `resolve_wire_actor` / `session_is_operator`. Fields re-sent on the wire per
  request — e.g. a `source_device` naming another device — are only honored
  when they equal the session's own device or the session is `Operator`. A
  peer therefore cannot escalate by re-declaring `roles:[Operator]` or by
  naming a victim's `device_id` in a request.

- **Roles are gated by admission policy, not merely declared.** `authorize_roles`
  is the single choke point that turns declared roles into *granted* roles and
  records the basis (`RoleGrant`). Under `trusted_lan`, every peer that reaches
  the socket is trusted (the LAN is the security boundary) and all declared
  roles — including `Operator` — are granted; run a credentialed mode if the
  LAN is not trusted. Under credentialed modes the shared credential authorized
  the connection; MotionStage does not yet carry a per-credential role map, so
  declared roles are still granted, but `RoleGrant::Credential` is recorded and
  `authorize_roles` is where a per-credential/identity role allow-list belongs
  (see its `TODO(security)`).

- **Reconnect does not freely supersede a live session.** Sessions are keyed by
  the self-claimed `device_id`, so a reconnecting connection claiming a live
  device's id could otherwise evict it. The old session's terminal
  `SessionLeft` is therefore **deferred until the superseding connection is
  itself admitted** (passes `register`): a pre-auth or failed-admission
  reconnect never retires the live session from the replicated event stream.
  Residual caveat: the new record overwrites the old one's mutable handshake
  fields immediately (one map slot per `device_id`); deferring the
  `SessionLeft` is what makes *admission*, not mere reconnection, the gate for
  evicting a live session off the stream.

Known limitation: `device_id` is not cryptographically bound to a credential.
Under credentialed modes, spoofing another device's `device_id` still requires
passing admission with a valid credential; binding identity to the credential
is future work alongside the per-credential role ACL.
