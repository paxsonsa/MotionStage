# Design: Scene Management — Takes, Tracks, Replay, and USD

Status: proposal (P6 in `roadmap-v2.md`). Builds on the topology model in
`design-architecture.md` (authoritative simulation, players, host DCC) and
assumes the P1 event plane and P3 operator plane exist.

## 1. Goals

- A **take system**: recordings become first-class, named, numbered entities
  the server owns and every player can browse.
- **Recording tracks**: a take is a set of typed, per-target-attribute
  sample curves plus a marker timeline — not an opaque file.
- **Replay**: the server can play a take back into the scene through the
  same pipeline live motion uses, with transport controls.
- **USD-compatible persistence**: saved data should open in DCCs and
  pipeline tools without a bespoke importer.

The game-server framing extends naturally: a take is a **demo recording**,
replay is a **demo playback** driven by a virtual player, and the take
library is the match-history screen.

## 2. Take system

### 2.1 Entities

```
TakeId   = Uuid (v7, time-ordered)
Take {
    id: TakeId,
    number: u32,              // server-assigned, monotonic per scene
    slate: String,            // human label, e.g. "sc04_setupB"
    scene_id: SceneId,
    scene_snapshot: SceneSnapshot,   // graph as it was at record start
    started_ns / stopped_ns,
    tracks: Vec<Track>,
    markers: Vec<Marker>,     // mode transitions, mapping events (CMTRK2 today)
    rating: Option<u8>,       // circle-take workflow: NG / keep / select
}
Track {
    target: (ObjectId, attribute name),
    source: (device_id, source_output),  // which mapping produced it
    value_type: AttributeValueType,
    samples: [(timestamp_ns, AttributeValue)],
}
```

Key decisions:

- **The server owns take identity.** `StartTake` returns `{take_id, number,
  slate}` assigned by the server (P3 wire op). The phone's record button
  creates a real take; clients never invent take names/paths. The current
  API's caller-supplied filesystem path (`start_recording(path)`) inverts
  this — replace it with a server-managed take library directory.
- **Takes embed the scene snapshot at record start.** A take must replay
  correctly even after the scene is edited later. The snapshot also makes a
  take exportable standalone.
- **Tracks are per target attribute**, not per device — that matches how
  they're consumed (curves on scene objects) and how CMTRK2 frames are
  already keyed (`object_id`, `attribute`).
- **Numbering/slating** follows set practice: monotonic take number per
  scene, free-text slate, and a rating flag so a "circle take" survives into
  editorial. All mutable post-record (rename/rate/delete are wire ops with
  events).

### 2.2 Library and events

- Library root: server-configured directory; one subdirectory per scene;
  one capture file per take plus a JSON manifest index the server maintains.
- New wire ops (extending P3): `ListTakes`, `GetTake`, `RenameTake`,
  `RateTake`, `DeleteTake`.
- New events: `TakeCreated`, `TakeUpdated`, `TakeDeleted`,
  `PlaybackStateChanged` — so every player's take browser stays live.

### 2.3 Timecode

Today timestamps are raw `timestamp_ns` from the sender. Takes need a
shared clock story:

- Server stamps all track samples against the **session clock**
  (server-monotonic ns), converting device timestamps at ingest using the
  existing ping/heartbeat exchange for offset estimation.
- A take stores `timebase = (started_ns, fps_hint)`; SMPTE timecode is a
  derived display/export concern, not a storage format.

## 3. Replay

Replay is a **virtual player**: a server-side session (role
`PlaybackSource`) whose "device outputs" are the take's tracks.

- Playback feeds samples through the **same apply path as live motion**
  (`apply_updates` → transforms → filters → current values → data-plane
  replication). No second code path; what you replay is what you recorded,
  modified only by whatever filters/mappings are active — by default replay
  bypasses filter chains and applies recorded values verbatim.
- **Ownership uses the existing lease model.** Starting playback on a take
  claims the target attributes of its tracks; a live mapping that owns one
  of those attributes blocks playback of that track (or requires `Operator`
  override) — same arbitration as two devices fighting, no new rules.
- Transport controls as wire ops: `PlayTake {take_id, rate, loop}`,
  `PausePlayback`, `SeekPlayback {t}`, `StopPlayback`. State broadcast via
  `PlaybackStateChanged {take_id, position, rate, playing}` so every HUD can
  render a scrubber.
- **Recording during playback is allowed** (re-recording one object while
  replaying the rest — the core layered-mocap workflow). The tracks being
  recorded are whatever mappings are live; replayed tracks pass through into
  the new take unless muted. Mute/solo per track is a playback option, not a
  scene edit.

## 4. USD evaluation

### 4.1 Where we are

The current exporter (`crates/motionstage-export-usd`) is USD in syntax
only: it emits one `Scope` per frame with stringly-typed `custom string`
attributes. No `timeSamples`, no `Xform` ops, no cameras. Nothing downstream
can consume it as animation; it satisfies the determinism test and nothing
else. Treat it as a placeholder, not a foundation.

### 4.2 What MotionStage data looks like as real USD

The mapping is almost embarrassingly direct:

```usda
#usda 1.0
(
    upAxis = "Z"                 # match Blender; convert for Y-up consumers
    metersPerUnit = 1
    timeCodesPerSecond = 120     # = capture rate; exact ns kept in customData
    customLayerData = {
        string motionstage_take_id = "..."
        string slate = "sc04_setupB_t007"
        int take_number = 7
    }
)

def Xform "hero_cam" (
    customData = { string motionstage_object_id = "<uuid>" }
)
{
    double3 xformOp:translate.timeSamples = { 0: (0,0,1.6), 1: (0.001,0,1.6), ... }
    quatf xformOp:orient.timeSamples = { 0: (1,0,0,0), ... }
    uniform token[] xformOpOrder = ["xformOp:translate", "xformOp:orient"]

    def Camera "shape" {
        float focalLength.timeSamples = { 0: 35.0, 240: 50.0 }   # lens ring!
        float fStop.timeSamples = { ... }
        float focusDistance.timeSamples = { ... }
    }
}
```

- `position`/`rotation` tracks → `xformOp` timeSamples.
- The iOS lens parameters (focal length, focus, aperture) → `UsdGeomCamera`
  attributes **natively** — the schema already has exactly these fields.
- `ObjectId` UUIDs → prim `customData`, preserving the stable-identity
  contract across renames. The planned `dcc_handle` (review §2) also lands
  in `customData`/`assetInfo`.
- Baselines → the attribute's default (non-time-sampled) value; recorded
  motion → timeSamples. USD's default-vs-timeSample distinction models the
  baseline/current split for free.
- Markers → a `Scope "markers"` with typed custom attributes, or layer
  `customLayerData` for take-level events.

### 4.3 Adoption levels

**Level A — real USD export (recommended now, small effort).**
Rewrite `motionstage-export-chan`-style deterministic *text* authoring to
emit the structure above (`UsdGeomXform` + `timeSamples` + `UsdGeomCamera`).
No new dependencies — hand-authored `.usda` text stays deterministic and
CI-testable; correctness validated in CI by a Python job using the official
`usd-core` wheel (`pip install usd-core`) to open and sanity-check the
output. Also add the missing export CLI (`motionstage-cli export --usd`).
Result: every `.cmtrk` becomes something Blender/Houdini/Maya/usdview can
open today.

**Level B — takes as USD layers (recommended target for the take library).**
The take library becomes a USD **stage**:

- `stage.usda` — root layer: scene description (objects, attributes,
  identities), sublayer list.
- `takes/t007_sc04.usda` — one layer per take: the Level-A content. Opening
  the stage with a take's layer active shows that take; layer muting is
  take-switching.
- `.cmtrk` (CMTRK2) **remains the capture format** — it is the deterministic,
  marker-complete, append-friendly wire/disk format recording writes under
  pressure. On `StopTake`, the server materializes the USD layer from it.
  CMTRK = the demo file; USD = the published, interchange-ready form. Both
  are kept; the manifest links them.

This split matters: recording must never depend on a composition engine's
performance or a C++ dependency's presence, and USD is not an
append-friendly capture format. Materialize-on-stop gets full compatibility
without touching the hot path.

- Rust-side authoring stays deterministic text for the layer files. If/when
  composition features are needed server-side (flattening, value clips for
  very long takes), do it in the Python surface via `usd-core` rather than
  binding C++ USD into the Rust runtime.

**Level C — USD as the runtime scene graph (rejected).**
Replacing `RuntimeCore`'s model with a live USD stage would buy composition
semantics nobody has asked for at the cost of: a C++ dependency in the
Rust server (no official Rust bindings exist; community bindings are
immature), mobile/FFI build complexity, and 120 Hz mutation patterns USD is
not built for. The runtime stays the small deterministic Rust graph; USD is
the persistence/interchange projection of it.

### 4.4 Compatibility notes and risks

- **Axis/units:** declare `upAxis` and `metersPerUnit` explicitly and pick
  one convention at the server (proposal: Z-up, meters — matches Blender and
  the existing iOS conversion). Exporters for Y-up consumers re-express at
  export, never at capture.
- **Time:** choose `timeCodesPerSecond` = capture rate and emit exact
  fractional timeCodes from ns timestamps; never resample at export.
- **Blender:** bundles USD import/export and recent versions expose the
  bundled `pxr` Python module — verify the minimum Blender version we claim
  (ties into the existing `bl_info`/manifest cleanup); worst case the addon
  imports takes via Blender's native USD importer with zero extra code.
- **iOS bonus:** a USDZ package of a take is AR-QuickLook-viewable on the
  phone — a free "review the take in AR" feature once Level A exists.
- **Naming:** prim names must be USD-legal identifiers; scene object names
  need sanitization + the UUID customData as ground truth (already the
  identity model).

## 5. Suggested build order (within roadmap P6)

1. Take entity + server-owned library + `StartTake`/`StopTake`/`ListTakes`
   (+ events) — replaces the caller-supplied path API.
2. Level A USD export + export CLI + `usd-core` validation in CI.
3. Replay virtual player with transport controls and lease arbitration.
4. Level B stage/layer library, materialize-on-stop.
5. Mute/solo + record-over-playback (layered mocap).
