# MotionStage Cross-Repo Consistency Review (July 2026)

Scope: `MotionStage` (server/protocol/engine/SDKs), `motionstage-blender`, `cinemotion-ios`.

This review answers three questions — (1) does the RPC/endpoint surface make sense,
(2) are we still adhering to the server-held scene-graph + mapping model, and
(3) do clients have what they need to make this a cooperative tool — and then
covers the Blender and iOS apps individually.

---

## Executive summary

The core engine (`motionstage-core`) is in good shape: server-authoritative
state, a clean mapping/lease/baseline model, deterministic transforms, and a
strict session/mode state machine that matches the docs. The problems are all
at the edges, and they share one root cause:

> **MotionStage has no server→client change notification of any kind.**
> Every mutation — mode, mapping CRUD, baseline actions, scene load, motion
> ingest — updates shared state and tells at most the caller. The publish loop
> (`spawn_scheduler_loops`, `crates/motionstage-server/src/lib.rs:447-467`)
> snapshots runtime state into `last_published_snapshot` **which nothing ever
> reads or sends**. "Clients do things and DCCs don't get UI updates" is not a
> bug in the Blender addon; it is the designed (or rather, undesigned) behavior
> of the protocol. The Blender addon papers over it with two competing polling
> loops, and that papering is where the addon's jank and races come from.

Secondary structural finding: there are **two disjoint API surfaces** —
the QUIC wire protocol (what devices use) and the in-process `ServerHandle`
API (what the DCC uses via the Python bridge) — and they expose very
different capability sets. Devices can't touch scenes or mappings; the DCC
can't be a remote client at all (it must host the server in-process). The iOS
app, meanwhile, integrates with neither: it is a UI scaffold with a faked
connection flow and no MotionStage SDK usage.

---

## 1. RPC / endpoint surface audit

### 1.1 The two surfaces

**Surface A — wire protocol** (`crates/motionstage-protocol/src/lib.rs:172-209`,
served by `handle_quic_peer` in `crates/motionstage-server/src/lib.rs:1305`):
`ServerHello`/`ClientHello`, `RegisterRequest/Accepted/Rejected`, `Ping/Pong`,
`SetMode`/`ModeState`, three baseline actions + `BaselineActionApplied`,
video signaling (`CreateVideoOffer`, `VideoOffer`, `VideoSignal`,
`DrainSignals`, `SignalsBatch`), `Error`. Data plane: `MotionDatagram`.

**Surface B — in-process `ServerHandle`** (`crates/motionstage-server/src/lib.rs:271-1303`,
exposed to Python via `crates/motionstage-sdk-python`): everything in A's
back end **plus** scene load / `set_active_scene`, all mapping CRUD,
recording start/stop, video descriptor management, session listing, metrics.

### 1.2 Asymmetries and holes

| Capability | Wire (devices/iOS) | Python (DCC) | Rust only |
|---|---|---|---|
| Push motion | yes | – | – |
| Set mode | yes (Operator) | yes | – |
| Baseline reset/commit | yes (Operator) | yes | – |
| Scene load / set active | **no** | yes | – |
| Read scene / attribute values | **no** | yes (poll) | – |
| Mapping create/remove | **no** | yes | – |
| Mapping **update** / **lock** | **no** | **no** | `update_mapping` (:821), `set_mapping_lock` (:869) |
| List mappings | **no** | **no** | snapshot only |
| Recording start/stop | **no** | yes | – |
| Set master video descriptor | **no** | **no** | `set_master_video_descriptor` (:1081) |
| List sessions / metrics | **no** | yes | – |

Consequences:

- **A network client cannot create, read, or release a mapping.** The iOS app
  can stream motion into a mapping but only if the DCC created it first, and it
  can never display "what am I mapped to."
- **`update_mapping` and `set_mapping_lock` are dead code for every shipping
  client** — reachable from neither Python nor the wire, despite locks being a
  documented part of the lease model.
- **Mappings aren't even *readable* from Python** — there is no
  `list_mappings`; the Blender addon keeps its own shadow copy and hopes it
  stays in sync (see §4).
- **The wire video path can never work end-to-end:** `CreateVideoOffer`
  requires the master descriptor, but `set_master_video_descriptor` is not
  exposed to Python or the wire, so no shipped client can set it —
  `ensure_video_session_ready` will always fail ("master video descriptor not
  set", server:1260). The frame pipeline (`VideoStreamEndpoint` /
  `FramePushSink` in `motionstage-media`, surfaced in
  `python/motionstage_sdk/video.py`) is never wired into `WebRtcSession`;
  `add_h264_track` creates a track nothing feeds. Video is signaling-only today.

### 1.3 Protocol-level defects

- **Version negotiation is discarded.** `negotiate_version`
  (`protocol.rs:230`) computes a selected minor, but `hello_exchanged`
  (server:565-573) only checks for `Err` and drops the result. Envelopes are
  always stamped with the local `PROTOCOL_MINOR` (=3)
  (`transport-quic:127,159`), and `validate_wire_version` (:395) rejects any
  envelope with minor > the receiver's own. Net effect: an older-minor client
  rejects the server's replies — the documented/tested backward-minor
  compatibility does not hold end-to-end.
- **Silent handshake drops.** Pre-register failures (version mismatch, empty
  roles/features, MotionSource with no advertised attributes) propagate via
  `?` and close the connection without any `Error`/`RegisterRejected`
  (server:1333-1338). Clients can't distinguish "server rejected me" from
  "network died". Only `register()` failures get a proper reject.
- **`SceneSynced` is a no-op.** `scene_synced()` (server:669) flips the state
  enum; no scene bytes are ever transmitted. `docs/protocol.md` step 7
  ("Session syncs scene state") describes something that doesn't exist.
- **Mode is poll-only for devices.** The only way a device learns of a mode
  change made by someone else is that `Ping` replies with `Pong` **and**
  `ModeState` (server:1381-1387) — an undocumented piggyback.
- **`set_mode_control_allowlist` is a deliberate no-op** (server:769-779) but
  is still surfaced in the Python SDK, where it silently does nothing. Either
  implement or remove it.
- **Unenforced roles.** `CameraController` and `VideoSink` exist in the enum
  and docs but are never checked; only `Operator` and `MotionSource` are
  enforced.

### 1.4 Naming inconsistencies across layers

- `target_object` (Rust core, wire, recording markers) vs `target_object_id`
  (Python dict key, pyo3 reader — `sdk-python:204`, `server.py:175`).
- Enum→string casing: pyo3 emits snake_case (`motion_source`), discovery TXT
  records emit Debug PascalCase (`MotionSource`, `discovery:33`), session
  state reaches Python as PascalCase and Python `.lower()`s it.
- Mode vocabulary: Rust enum `Idle/Live/Recording`; Python strings
  `idle/live/recording` plus aliases `stopped/stop/record`; Swift integer
  constants 0/1/2; Python `set_stopped_mode` maps to `Idle`.
- Rename residue: `legacy/` still holds the pre-rename CineMotion code; the
  recording magic bytes are `CMTRK1/2` ("CM" = CineMotion).

### 1.5 Recommendation

> **Design decisions confirmed (July 2026, owner):** (a) the DCC hosting the
> server in-process is the intended topology — devices are the only wire
> clients; (b) cooperation scope is one DCC + devices, not multi-DCC;
> (c) the `setDataFlow`/`setRecording` split from the in-flight iOS SDK is
> the new canonical mode model, replacing the `Idle/Live/Recording` tristate;
> (d) DCC-viewport→device video is a core feature. The recommendations below
> reflect those decisions.

**Formalize the wire as the device plane** (per decision a/b) — the in-process
`ServerHandle`/Python API stays the control plane where scenes and mappings
are authored. Document that explicitly, then close the gaps the device plane
actually has:

- **Read + subscribe:** devices need the scene/mapping/mode state relevant to
  them — a real `SceneSynced` snapshot plus the event stream (§3). A device
  should be able to render "I am mapped to camera.position" without the DCC
  side-channeling it.
- **Recording + mode-split control:** the iOS record button implies
  `SetRecording`/`SetDataFlow` as wire operations (Operator-gated). Recording
  start/stop is currently host-only; under decision (c) it becomes a
  first-class wire message pair.
- **Mapping CRUD stays host-side** (the DCC owns authoring), but add a
  device-scoped `ListMappings`/mapping events so clients can display their own
  bindings. Full wire mapping CRUD is explicitly out of scope for this
  topology.
- **Mode-split migration (decision c):** replace the core `Mode` tristate
  with two axes (`data_flow: on/off`, `recording: on/off`) through Rust core,
  wire, Python, and Blender. One semantic must be nailed down before
  migrating: the tristate's "mapping mutations blocked in Recording" rule
  becomes "blocked while `recording` is active", and `recording=on` with
  `data_flow=off` should be rejected (or auto-enable data flow) —
  recording dead air must not be representable. Bundle this with the
  event-plane protocol bump so the wire breaks once, not twice.

Independent of topology: fix version negotiation (store and stamp the
negotiated minor), always send a typed `Error`/`RegisterRejected` before
closing a handshake, expose or delete
`update_mapping`/`set_mapping_lock`/allowlist, and normalize naming (one
casing at every serialization boundary; pick `target_object_id` everywhere;
drop the `stopped` alias or make it official).

---

## 2. Scene-graph + mapping model adherence

The internal model is faithful to the design:

- Authoritative graph in `RuntimeCore` (`core/src/runtime.rs:52-61`):
  `Scene → SceneObject (BTreeMap by ObjectId) → SceneAttribute` with
  `default_value` doubling as the baseline (`core/src/model.rs:40-118`).
- `Mapping` binds `source_device + source_output` → `scene/object/attribute`
  with component masks, exclusive ownership per target attribute,
  lease/heartbeat reclaim, and Recording-mode mutation blocks
  (`runtime.rs:162-273`) — all matching `docs/design-architecture.md`.
- Relative composition and filter chains are implemented as documented.

Where the model frays is **at the DCC boundary**:

1. **The server's mapping stops at its own scene graph.** The binding from a
   MotionStage `SceneObject` to a DCC object is entirely client-side and
   by-name: the Blender reference adapter resolves `bpy.data.objects.get(name)`
   (`python/blender_adapter/motionstage_blender_adapter.py:21-44`), while
   `docs/dcc-integrators.md:98` insists mappings use stable UUIDs. The addon
   does better (it stamps `motionstage_object_id` custom properties on
   datablocks), but the server has no concept of a DCC-side handle, so every
   integrator re-invents the correspondence. **Recommendation:** add an
   optional `dcc_handle`/`external_ref` field on `SceneObject` (opaque string,
   set by the scene author) so the correspondence is stored server-side, is
   part of scene sync, and survives renames.
2. **The scene graph never crosses the wire.** Devices reach `SceneSynced`
   without receiving a scene, so no wire client can render or pick targets
   from the graph. This breaks the "server holds the scene graph" story for
   everyone except the in-process host.
3. **Mapping state is not observable.** No `list_mappings` in Python, no
   mapping messages on the wire, and the Python delegate's `on_mapping_event`
   is never emitted (see §3). The Blender addon consequently keeps a shadow
   `SERVICE.mappings` dict that desyncs on reconnect/file-load (§4.2).

Verdict: **the model is intact in the core but not actually delivered to
integrators.** The fix is not a redesign — it's exposing what already exists:
scene snapshot + mapping list reads, and change events.

---

## 3. Cooperative multi-client operation (the headline gap)

What exists today:

- **No broadcast bus, observer, or per-session outbound channel anywhere.**
  Confirmed by search across all crates. Mutations touch `RuntimeCore` behind
  one `RwLock` and reply only to the caller.
- The publish loop writes `state.last_published_snapshot` every tick
  (server:461); the only reader is a test accessor (:403). **It publishes to
  nothing.**
- The Python "event" system is a client-side polling thread
  (`python/motionstage_sdk/server.py:241-294`): sessions at 1 Hz, mode at
  20 Hz, attribute values at 120 Hz — diffed and turned into delegate
  callbacks. Three of the six documented delegate callbacks
  (`on_scene_snapshot`, `on_mapping_event`, `on_recording_event`) are **never
  emitted** by that loop, even though the Blender adapter implements them and
  `docs/dcc-integrators.md:92` advertises them.
- Wire clients get nothing at all except `ModeState` piggybacked on `Pong`.
- Signaling is pull-only (`SignalingHub`, `media:136-152`): queued until the
  recipient sends `DrainSignals`; dropped if never drained.

This is exactly the reported symptom: an iOS client flips mode or a baseline,
the server mutates state, and Blender finds out only if/when its pollers
happen to diff it — and the addon's UI refresh is further gated behind its own
change-detection heuristics (§4.1).

### Recommended design: a real event plane

Add a server→client event stream, and make **both** SDK surfaces consume it:

1. **Event model** (in `motionstage-protocol`): a `StateEvent` enum —
   `ModeChanged`, `SceneLoaded/Activated`, `SceneDelta` (object/attribute
   upserts), `MappingCreated/Updated/Removed/Locked/Released`,
   `BaselineApplied`, `SessionJoined/Left`, `RecordingStarted/Stopped` —
   each carrying a monotonic `seq` and the originating `session_id` (so
   clients can ignore their own echoes or display "who did this").
2. **Transport:** a QUIC unidirectional stream per session for events (keeps
   the bidi control stream request/reply-only), or simply new
   server-initiated `ControlMessage` variants on the existing stream. High-rate
   attribute values stay on datagrams; a throttled `AttributeBatch` event
   (reuse the existing `publish_hz` loop — it already snapshots, it just
   needs somewhere to send) serves observers that don't want raw motion rate.
3. **Initial sync + reconnect:** on entering `SceneSynced`, actually send a
   full `SceneSnapshot` + mapping list + mode + current `seq` — making the
   state name true. On reconnect, client presents its last `seq`; server
   replays or sends a fresh snapshot if the gap is too old. This gives clients
   a simple invariant: *snapshot + ordered deltas = truth*.
4. **In-process path:** replace the Python polling pump with a native event
   subscription (pyo3 callback or a drainable event queue fed by the same
   bus). The delegate contract already has the right shape — wire up the three
   never-fired callbacks and delete the 3-cadence poller.
5. **Cooperative niceties this unlocks cheaply:** session list with
   roles/device names in every client and "mapping owned by <device>" labels.
   (Scope note: cooperation target is confirmed as one DCC + devices — no
   multi-DCC attribution or conflict semantics needed. Keep `seq` and
   originating `session_id` in the event envelope anyway: `seq` is required
   for reconnect correctness regardless, and the originator field costs
   nothing now but avoids a breaking change if multi-user ever arrives.)

This is the single highest-leverage change in the system; almost every other
complaint (Blender jank, iOS emptiness, doc divergence) gets simpler once
state changes are pushed instead of scraped.

---

## 4. Blender addon (`motionstage-blender`)

Architecture note: the addon does not connect to a server — it **hosts** the
server in-process via `motionstage_sdk` → native `motionstage_sdk_rust`
(`motionstage_blender/service.py:79`). Phones connect to Blender.

### 4.1 Why the UI misses updates (ranked)

1. **`EnumProperty` items-callback string lifetime bug.**
   `_client_enum_items` / `_source_output_enum_items` (`addon.py:71-91`)
   return freshly built tuple lists without retaining a Python reference.
   Blender does not copy those strings; when the client set changes, dropdowns
   show stale/blank/garbage entries. This is the classic Blender pitfall and
   the most likely direct cause of "client connected but the dropdown doesn't
   show it." Fix: cache the items list on a module-level variable keyed by
   catalog revision.
2. **Unsynchronized cross-thread state.** The SDK's daemon thread mutates
   `_client_names`/`_client_source_outputs` (`service.py:337-373`) while the
   main thread iterates them during draw/timer
   (`service.py:414-419,452-495`). No lock anywhere → intermittent
   `RuntimeError: dictionary changed size during iteration`, swallowed by the
   panel's try/except into a "Panel error" label — i.e. the update is silently
   dropped. Fix: a single `threading.Lock` around service state, or better,
   make the background thread only enqueue and do *all* state mutation on the
   main-thread timer.
3. **Two redundant pollers fighting.** The SDK pump thread *and* the addon's
   120 Hz timer (`addon.py:731-771`) both poll the same native server and both
   mutate the same service state; the timer then only redraws `if
   catalog_changed or runtime_changed`, and the pump may have already consumed
   the delta. The revision backstop (`addon.py:753-756`) usually saves it, but
   any exception mid-refresh (`service.py:445-447`) skips the redraw entirely.
   Fix: one update path. Either disable the SDK pump and poll only from the
   timer, or (after §3) subscribe to events and drop polling.
4. **Selection reconciliation only runs inside operators**
   (`_reconcile_source_selection`, `addon.py:514`), so after the client set
   changes, dropdown selections can point at stale entries until the user
   happens to run an operator.

### 4.2 State-model defects

- **Mappings lost on file reload.** Authoritative mapping state is a module
  singleton dict (`service.py:60`); the persisted `settings.mappings`
  CollectionProperty is a read-only mirror that gets **wiped** by the first
  `_sync_mappings` after load (`addon.py:295-296`). No `load_post` handler, no
  rehydration. Saved run files always persist `remote_mapping_id=None`
  (`service.py:592-604`), so loading a run never re-registers server mappings
  either.
- **Reconnect desync.** `SERVICE.stop()` keeps `self.mappings` but their
  `remote_mapping_id`s point at a dead server; nothing re-creates them on
  restart (`service.py:101-116`).
- **No undo handling** — undo restores PropertyGroups but not the module
  singleton; the two diverge silently.
- Fix direction: make the server the single source of truth (needs
  `list_mappings` — §1.2), persist only *intent* (target object id + source
  attribute) in the .blend, and re-establish mappings from intent in a
  `load_post` handler and on runtime start.

### 4.3 UI jank

- JSON-serializing the entire state inside `draw` on every redraw
  (`addon.py:1526`, `350-357`) at up to 120 Hz-triggered redraws.
- The timer overwrites `settings.mapping_target_object` from the viewport
  selection every tick (`addon.py:765-768`) while the mapping UIList hides all
  mappings that don't match the active object (`addon.py:1447-1453`) — so
  clicking another object silently empties the mapping list. Make
  "filter to selection" an explicit toggle instead.
- Reading object transforms **mutates** every object's `rotation_mode`
  (QUATERNION→XYZ→restore) on each resync/baseline commit
  (`addon.py:436-456`), dirtying datablocks and triggering depsgraph churn.
  Compute quaternions from `matrix_basis` instead.
- 12 registered subpanels (6 duplicated across VIEW_3D and Properties). Pick
  the N-panel as the home; keep at most a status readout in Properties.
- No connection-health surfacing: `is_connected` is just `server is not None`
  (`service.py:619`) and refresh exceptions are swallowed — a wedged native
  server still shows "Connected". Surface last-error and last-successful-poll
  age in the Status panel.

### 4.4 Packaging / compatibility

- `bl_info` says Blender 4.0 (`__init__.py:7`) vs `blender_manifest.toml`
  `blender_version_min = "4.2.0"`; `scripts/version.py` doesn't check this.
- CI builds a cp311-only wheel; the extension is ABI-locked to Blender builds
  shipping Python 3.11. Consider abi3 (`pyo3/abi3-py311`) to cover future
  Blender Python bumps.
- **Likely broken bundle:** CI runs `maturin build --manifest-path
  crates/motionstage-sdk-python/Cargo.toml`, but the `python-source` config
  that pulls the pure-Python `motionstage_sdk` package into the wheel lives in
  `MotionStage/python/pyproject.toml`, not the crate dir. The bundle likely
  ships only `motionstage_sdk_rust` and omits `motionstage_sdk`, which the
  addon imports (`service.py:677`). The bundle test can't catch this because
  it fabricates a fake wheel that does contain the package
  (`tests/test_build_bundle.py:9-24`). Verify with a real CI artifact and
  build via the pyproject instead.
- Tests never touch `addon.py` (UI/operators/timer/enum callbacks — where the
  reported bugs live), and there's no Blender-in-CI smoke test
  (`scripts/smoke_addon.py` runs without real `bpy`). Adding a headless
  `blender --background --python` CI job would have caught most of §4.1.

---

## 5. iOS app (`motionstage-ios`)

> Note: this section originally reviewed `cinemotion-ios`, which is now
> **obsolete** — it was a UI scaffold with a faked connect flow (hardcoded 3 s
> sleep), an empty `Networking` actor, no SDK/QUIC/CoreMotion usage, and
> crash-on-error WebRTC experiments. Its successor, `motionstage-ios`, was
> audited in its place; the old repo should be archived.

The rewrite is a genuinely substantial, well-structured SwiftUI app (38 Swift
files; Observation-framework services, viewport with Metal grid + video
renderer + HUD, Live/Record/Reset/Zero/lens-ring controls, connection sheet,
settings). It fixes essentially everything wrong with the old app:

- **Real connection**: `MotionStageService.connect` calls the SDK over the
  Rust QUIC FFI, sets `.error` on failure — no fake path remains
  (`Services/MotionStageService.swift:50-76`).
- **Real discovery**: `NWBrowser` Bonjour browse of `_motionstage._udp`, TXT
  parsing (`port`, `cert_fp`), IPv4/IPv6 socket-address formatting for Rust
  (`Services/DiscoveryService.swift:28-251`).
- **Real motion**: ARKit 6DOF at ~60 Hz with Y-up→Z-up conversion and
  quaternion continuity, decoupled from a `CADisplayLink` send loop
  (30-120 Hz, last-value-wins), idle-gated
  (`Services/MotionTrackingService.swift:110-179`,
  `Services/MotionPipeline.swift:64-111`).
- **Mode confirmed by server**: optimistic flip, then reconcile/revert from
  the server response (`MotionStageService.swift:144-162`).
- Clean error hygiene: no `fatalError`/`try!`/`as!`/TODOs in app code; video
  errors surface to UI state.

### Critical finding: the app builds against a phantom SDK

The Xcode project references a **local** package at
`../cinemotion/swift/MotionStageClient` (`project.pbxproj:208, 630-632`) —
i.e. a sibling checkout of the server repo on the developer's machine. The
API surface the app consumes does not exist in the pushed MotionStage repo:

| App expects | Pushed SDK/FFI has |
|---|---|
| `init(deviceName:outputAttributes:[AttributeKey])` (typed, plural) | single `outputAttribute: String` |
| `connect(...fingerprint:)` (cert pinning) | no fingerprint param |
| `setDataFlow(_:)` + `setRecording(_:)` mode split | flat `setMode(RuntimeMode)` |
| `sendMotionFrame` (`CameraMotionFrame`), `sendFloat(attribute:value:)` | `sendPosition(x:y:z:)` only |
| `resetScene()` | — |
| `client.events` async stream (connected/reconnecting/…) | — |
| Video signaling (`createVideoOffer`, `sendVideoAnswer`, `sendVideoIce`, `nextVideoSignal`, `videoStreamStatus`) | — |

The FFI header (`crates/motionstage-sdk-swift/include/motionstage_swift.h:23-56`)
confirms only `new/connect/disconnect/send_vec3f/set_mode/session_id/device_id/
last_error`. **No remote branch of MotionStage contains the richer SDK either**
(`swiftbridge-prototype`, `reengine`, `clienttui` are older divergent trees).
So the SDK — and presumably matching server-side protocol work
(`setDataFlow`/`setRecording` split, `resetScene` over the wire, connection
events, client-side video signaling) — exists only in an unpushed local
working copy. Until that lands, the app cannot be built from the pushed
repos, and this review's protocol audit (§1) understates the in-flight
protocol. **Pushing that SDK/server work is the highest-priority iOS action.**

### Remaining gaps in the app itself

- **Connection events dropped on the floor.** The app consumes `client.events`
  but only logs them (`MotionStageService.swift:219-238`); a transport drop
  leaves the UI showing connected until a 30-consecutive-send-failure circuit
  breaker trips (`:19-20, 180-185`). Reconnect state should feed
  `connectionState` and the HUD.
- **MappingView is a static mock** — hardcoded attribute list, `"--"` targets
  (`Views/Mapping/MappingView.swift:26-33`, `AttributeRow.swift:22`). No
  mapping RPCs exist for it to call (§1.2) — this is the concrete iOS
  casualty of the missing wire mapping surface.
- **Two divergent "zero" semantics**: HUD "Zero Origin" calls server
  `resetScene()` (`ViewportView.swift:53-55`) while the sidebar ZERO button
  resets only the local ARKit origin (`SetOriginButton.swift:7-8`). Likely a
  bug; at minimum confusing.
- **Manual connect can't cert-pin** — discovered-server connect passes the
  TXT `cert_fp`, manual entry omits fingerprint (`ConnectionSheet.swift:221-236`).
- No UI for: baseline commit (only reset), take management/export, session
  roster, mapping management — mostly blocked on §1/§3 server work.
- **Tests are empty stubs** (one assertion-free `@Test`, template UI tests) —
  though coordinate conversion, socket-address formatting, TXT parsing, and
  mode reconciliation are all pure and easily testable. A good DEBUG preview
  harness exists (`PreviewHarness`/`PreviewData`) but nothing automated.

---

## 6. Smaller cleanups worth batching

- Remove the vestigial `bevy_ecs::World` in `RuntimeCore` — a full ECS
  dependency holding one tick counter (`runtime.rs:364`).
- Dead branches: Python poll-interval check for mode `"record"` (never
  produced); unreachable "no active mapping" re-derivation in
  `ingest_motion_samples` (server:911-930).
- Delete or implement the mode-control allowlist (currently a documented
  no-op surfaced in Python).
- Decide the fate of `legacy/` (unbuilt pre-rename tree) — archive branch or
  delete.
- Auth defaults to literal `"motionstage"` token/API key when unset
  (server:167-186); make unset mean *disabled policy* or a generated secret,
  never a well-known constant.
- Add the missing export CLI (`docs/dcc-integrators.md` admits exports are
  library-only) — a `motionstage-cli export --usd/--chan <cmtrk>` is cheap and
  closes a documented workflow gap.
- Docs to correct once the above land: `protocol.md` (scene sync step, version
  gates), `dcc-integrators.md` (delegate events that actually fire), README
  ("publishes snapshots for downstream systems").

---

## 7. Suggested sequencing

| Order | Work | Why first |
|---|---|---|
| 1 | Event plane (§3): event bus + wire events + real `SceneSynced` snapshot + Python delegate fed by events | Root cause of the reported symptom; unblocks everything below |
| 2 | Blender addon stabilization (§4.1–4.2): single update path, lock, enum-item caching, mapping persistence/rehydration | Converts the addon from two racing pollers to one event consumer; fixes the jank users feel |
| 3 | Device-plane completion (§1.5): scene/mapping read + events over QUIC, `SetDataFlow`/`SetRecording` wire ops, mode-split migration through core/Python/Blender | Devices become first-class per the confirmed embedded topology; single protocol bump together with #1 and #5 |
| 4 | iOS: **push the local SDK/server work** the `motionstage-ios` app builds against (phantom `../cinemotion` package, §5), then reconcile it with the event plane; wire reconnect events into UI; real mapping display via device-scoped `ListMappings` | The app is already substantial but unbuildable from the pushed repos; landing the in-flight SDK changes the protocol picture in §1 |
| 5 | Protocol hygiene: version negotiation fix, handshake error replies, naming normalization (§1.3–1.4) | Cheap, but touches wire compat — bundle with the event-plane minor bump |
| 6 | Video (confirmed core feature): expose descriptor management to the host/Python API, wire the frame pipeline into `WebRtcSession`, fix the Blender capture hot path (§ addendum), land the iOS receive path | Both client ends already invested; the server middle is the missing third of the pipeline |

---

## Addendum: re-audit of `motionstage/stability-and-state-simplification` (blender addon)

The branch (+2,260 lines over `main`) was re-audited against the ten defects in
§4. It makes real progress on the threading story and adds significant new
surface (GPU offscreen video capture, quaternion calibration, camera-relative
routing, one-click default mappings, companion UI).

### Scorecard vs the original findings

| # | Finding (§4) | Verdict |
|---|---|---|
| 1 | EnumProperty items string-lifetime bug | **Not fixed** — callbacks (`addon.py:120-186`) still return fresh lists; exposure reduced only because the mapping sub-panels were unregistered (`_CLASSES`, 2807-2815), but the properties/callbacks are still live |
| 2 | Cross-thread dict races | **Fixed** for mode + client/source catalog — Phase 2 makes the SDK pump log-only (`service.py:568-596`); main-thread `refresh_*` is the single writer, locked in by a test. Residual: `capture_attribute_batch` still appends `samples` from the bg thread (598-635) — benign under the GIL, a lock would make it airtight |
| 3 | Silent redraw skips on exception | **Partially fixed** — timer body and all draws now log/render errors; change-detection gating retained by design |
| 4 | Two redundant pollers | **Fixed** — single writer on the main-thread timer; pump only feeds the sample queue |
| 5 | Mapping persistence / rehydration / undo | **Not fixed** — still no `load_post`/undo handlers; `SERVICE.mappings` is still a module singleton wiped on reload while the `.blend` mirror is one-way. New partial runtime self-heal: `_clear_disconnected_mapping_sources` (`service.py:1178-1206`) |
| 6 | `rotation_mode` mutation on resync/commit | **Not fixed** (unchanged; live-apply path does it per sample too) |
| 7 | Selection clobbers mapping target + UIList hiding | **Mostly moot by removal** — the mapping panel/UIList are unregistered (moved to companion UI), but the per-tick overwrite of `mapping_target_object` still runs (`addon.py:1342-1346`) |
| 8 | JSON logging in `draw` | **Not fixed** — `_draw_main_header` still builds the full state payload + `json.dumps` per redraw (`addon.py:2389`, `625-632`) |
| 9 | Connection-health surfacing | **Not fixed** — `is_connected` is still `server is not None` (`service.py:1003`); errors now logged; new health is video-only |
| 10 | Packaging | **Partially fixed** — `build_bundle.py --python-sdk-dir` can now vendor the pure-Python SDK (101-119), **but CI never passes the flag** (build-bundle.yml:95, release.yml:76), so released bundles still risk missing `motionstage_sdk`. cp311 pin, `bl_info` 4.0 vs manifest 4.2 mismatch, and fake-wheel bundle test all unchanged |

### New issues introduced on the branch

- **Capture hot path cost** (`capture.py:46-128`): per frame — full offscreen
  draw, `read_color` readback, `bytes()` copy, then a pure-Python per-row flip
  (two full-frame copies, ~3-4 MB each) synchronously inside a `POST_PIXEL`
  draw handler at up to 30 fps. Fine at 720p, will hurt at 1080p+. Preallocate
  and flip in the consumer (or negotiate stride/orientation) instead.
- **Debug cruft in production path**: `MOTIONSTAGE_TEST_PATTERN` renders the
  scene then discards it for a Python-loop gradient (72-88); `_save_debug_png`
  writes hardcoded `/tmp/motionstage_debug.png` from inside the draw callback
  (196-228).
- **No-viewport blind spot**: streaming "on" with no visible VIEW_3D means the
  draw handler never fires — status shows configured forever with no frames
  and no user-visible warning.
- **Camera-relative position math** (`_apply_live_position_camera_relative`,
  `addon.py:257-302`): anchors orientation from `matrix_world` but writes
  `obj.location` (local) — inconsistent frame for parented objects; unlike the
  rotation path there is **no re-anchor threshold**, so a device re-zero
  mid-stream jumps the object. The Rx(−90) `(x, z, -y)` re-express has zero
  tests. (The quaternion rotation path itself — shortest-path sign fix,
  normalization, jump re-anchor — is sound.)
- **`start_companion_ui` mutates `os.environ`** based on a hardcoded
  sibling-repo dev path (`service.py:227-231`) — silent env pollution;
  harmless when the path is absent but shouldn't ship.
- **RGBA/BGRA is env-var-selected** (`MOTIONSTAGE_VIDEO_PIXEL_FORMAT`,
  `service.py:895-896`) — silent red/blue swap if encoder expectations differ.
- **Throttling latency**: idle tick is 66 ms and external mode changes poll at
  250 ms, so motion onset can lag ~0.25-0.3 s before the timer ramps to
  120 Hz, and companion-UI actions wait up to a tick to be drained. Samples
  aren't lost (deque), but first frames of a take apply late.
- **Draw-handler lifecycle is solid** (guarded add, safe remove order,
  deferred teardown after repeated failures, unregister cleanup) — credit
  where due.

### State-model verdict

The **concurrency** model is genuinely simpler (single writer for mode +
catalog; server clearly authoritative for those). The **state topology** is
not: state now lives in five places (Rust runtime, `SERVICE` singleton, the
persisted PropertyGroup mirror, module-global calibration/timer/video dicts,
and the companion UI), and the mapping double-truth (§4.2) is untouched. Tests
improved meaningfully at the service layer, but `addon.py` and `capture.py` —
including all the new math — remain entirely untested.

### Priority residuals on this branch

1. Enum-items string retention (one small cache keyed by catalog revision).
2. Mapping persistence/rehydration + `load_post`/undo handlers — the remaining
   data-loss path.
3. CI: pass `--python-sdk-dir` in both workflows; fix `bl_info` vs manifest;
   test against a real wheel.
4. Position-path re-anchor + frame consistency, with tests for both
   calibration functions (pure-math, easily testable without `bpy`).
5. Drop the JSON state dump from `draw`.
