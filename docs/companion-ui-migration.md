# Companion UI: Full Operator Migration (Phase 2/3)

> **Status: implemented (Blender).** Stage 1 (host-request bridge + takes/playback snapshot
> + WS commands), Stage 2 (Blender bridge drain in the tick + addon slimmed to launcher +
> DccAdapter), Stage 3 (React forms: mapping create, object baseline, takes/playback,
> host actions). Verified: `cargo test --workspace` (111), SDK + Blender pytest, FFI bridge
> smoke, headless register smoke, live cockpit screenshot. Per-DCC bridge handlers for
> Unreal/Unity/etc. reuse the same `HostRequest` queue.

Goal: the server-served companion UI becomes the single operator cockpit; the Blender
addon shrinks to **launcher + DccAdapter + bridge handler**. Identical UI across every DCC.

Grounded in `docs/companion-ui-architecture.md` and a parity audit of `addon.py` /
`service.py` vs `companion_ui.rs` vs `App.tsx` vs `ServerHandle`.

## What goes where

- **Companion UI owns** (runtime-agnostic operator workflow): mode, recording, baseline
  reset/commit (scene + object), mapping create/update/remove + source assignment +
  defaults, scene selection, takes list + playback transport + delete.
- **Blender keeps** (only it can): starting the in-process runtime (bootstrap — it serves
  the UI), reading/writing bpy (enumerate scene, apply values), GPU video capture, and the
  apply-side transforms (position routing world/camera-relative, rotation re-anchoring).
  These are *correct* in the plugin; they do not migrate into the runtime.

## The keystone: UI → plugin bridge

Today every WS command calls `ServerHandle` only; there is no path from the UI to the DCC
plugin. For host-side actions the UI must still *drive* (resync scene, start/stop video,
bake take to timeline), add a request queue:

1. **UI → runtime:** new `UiCommand` variants enqueue a `HostRequest`.
2. **Runtime:** `ServerHandle` holds `host_requests: Arc<Mutex<Vec<HostRequest>>>` with
   `enqueue_host_request` / `drain_host_requests`.
3. **Runtime → plugin:** the plugin polls `drain_host_requests()` (FFI) on its existing
   main-thread tick (`addon.py:_live_update_timer`) and executes each on the main thread.

`HostRequest` (initial): `ResyncScene`, `StartVideo { width, height, fps, source }`,
`StopVideo`, `BakeTake { take_id, fps, start_frame }`. Build the bridge once; every future
DCC reuses it.

## Snapshot additions (server → UI)

- `takes: Vec<UiTake>` (from `list_takes`), `playback: UiPlayback { state, take_id,
  position_ns, duration_ns, looping }` — for the playback transport.
- Defaults heuristic stays attribute-name based (device `position`→object `location`,
  `rotation`→`rotation_quaternion`) so no `object_type` field is needed in the core model.

## WS command additions (UI → server)

Already wired (Group A/B): SetDataFlow, SetRecording, Reset/Commit scene baseline,
CommitObjectBaseline, SelectTake, DeleteTake, PlaybackControl, CreateMapping, UpdateMapping,
RemoveMapping, set_active_scene. **New (Group B → HostRequest):** `resync_scene`,
`start_video`, `stop_video`, `bake_take`.

## React forms to add

Mapping create/edit (source device+output pickers from sessions, target object+attribute
from scene snapshot, component mask), object baseline commit, default-mappings button,
takes list + playback transport (play/pause/stop/seek/delete + bake), resync / video
start-stop buttons.

## Blender addon slimming

Reduce to one launcher panel (Start/Stop Runtime, Open UI, status) + the DccAdapter
(enumerate/apply/capture) + the bridge drain in the tick. Remove the operator-heavy
mode/mapping/baseline/runs panels and their operators (the UI owns them now). Keep the
apply transforms and the bootstrap.

## Staged sequence (each independently verified)

1. **Backend bridge + snapshot** (Rust): HostRequest queue, takes/playback in snapshot, new
   UiCommands, PyO3 `drain_host_requests`. Verify: Rust WS test enqueues + drains.
2. **Blender bridge + slim** (Python): drain host-requests in the tick and execute; reduce
   panels to launcher. Verify: pytest + headless register smoke.
3. **React forms** (TS): mapping/baseline/takes/playback/host-action UI. Verify: build +
   server-rendered screenshots.
4. **End-to-end**: full build + cargo test --workspace + pytest + Blender smoke + live screenshots.
