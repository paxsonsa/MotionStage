# DCC Integrators

## Goal

DCC integrators consume MotionStage recordings or runtime callbacks and map them into scene-authoring tools.

This repo provides two integration surfaces:
- Offline export surface (USD/CHAN from `.cmtrk`)
- Live callback surface (Python SDK delegate API, with Blender reference adapter)

## Quickest Path to First DCC Result

1. Generate a recording:

```bash
cargo run -p motionstage-cli -- simulate --server-bind 127.0.0.1:0
```

At prompt:

```text
record start recordings/demo.cmtrk
record stop
quit
```

2. Use exporter crates in a small Rust integration to emit DCC text formats.

There is currently no dedicated export CLI command in this repository; export is exposed as library APIs.

## Build + Test Path (Matrix)

| Phase | Goal | Commands | Pass Criteria |
|---|---|---|---|
| P1 | Validate local toolchain | `cargo test -p motionstage-recording -p motionstage-export-usd -p motionstage-export-chan` | Recording/export crates pass locally |
| P2 | Generate deterministic fixture recording | `cargo run -p motionstage-cli -- simulate --server-bind 127.0.0.1:0 --sample-hz 120` then `start`, `record start recordings/integration.cmtrk`, `record stop`, `quit` | `recordings/integration.cmtrk` exists and has non-zero frames |
| P3 | Validate Python integration surface | `python -m pip install -e ./python` and `python -m pytest -q python/tests/test_server.py python/tests/test_blender_adapter.py python/tests/test_video.py` | Delegate and video endpoint contracts pass |
| P4 | Validate exporter determinism in your adapter | Use the snippet below in your integration tests | Two consecutive exports are identical for the same input |

## Offline Export Integrators

### Recording input

Use `motionstage-recording::read_recording(path)` to load a `.cmtrk` file.

- Supports `CMTRK2` (markers + frames)
- Supports `CMTRK1` read compatibility

### USD export

`motionstage-export-usd::export(&recording)` returns deterministic USD text (`#usda 1.0`) for the input recording.

### CHAN export

`motionstage-export-chan::export(&recording)` returns deterministic channel text.

- `Vec3f` is expanded into `.tx/.ty/.tz`
- `Quatf` is expanded into `.qx/.qy/.qz/.qw`

### Minimal export adapter test pattern

```rust
use motionstage_recording::read_recording;

fn export_both(path: &str) -> (String, String) {
    let recording = read_recording(path).expect("recording should load");
    let usd = motionstage_export_usd::export(&recording);
    let chan = motionstage_export_chan::export(&recording);
    (usd, chan)
}

#[test]
fn export_is_stable() {
    let path = "recordings/integration.cmtrk";
    let (usd_a, chan_a) = export_both(path);
    let (usd_b, chan_b) = export_both(path);
    assert_eq!(usd_a, usd_b);
    assert_eq!(chan_a, chan_b);
}
```

## Live Python Integrators

Python package: `python/motionstage_sdk`

```bash
python -m pip install -e ./python
```

Key objects:
- `MotionStageServer`: runtime facade backed by the native Rust bridge
- `SceneUpdateDelegate`: callback contract for scene snapshots, attribute batches, mapping/mode/client/recording events
- `VideoStreamEndpoint`: pull/push video endpoint abstraction for DCC host integration

Strict runtime API notes:
- Scene authority is explicit: call `upsert_scene(scene_spec)` then `set_active_scene(scene_id)`.
- Mapping targets must use stable object UUIDs (`target_object_id`), not object names.
- Remote mode control requires `Operator` role.
- Runtime attribute batches contain server-resolved values (relative baseline composition already applied).
- Baseline controls are explicit: `reset_scene_to_baseline`, `commit_scene_baseline`, `commit_object_baseline`.

## Blender Reference Adapter

Reference module: `python/blender_adapter/motionstage_blender_adapter.py`

- Implements `SceneUpdateDelegate`
- Resolves Blender objects by name (`bpy.data.objects.get`)
- Applies `position` attribute batches into `obj.location`

Use `register_blender_delegate(server, adapter)` to bind callback wiring.

## Integration Contract Guidance

- Keep object identity stable between runtime and DCC object lookup.
- Normalize units/axis conventions in your adapter layer.
- Treat `.cmtrk` as authoritative capture output; do not mutate raw source data before archival.
- Keep export steps deterministic for reproducible editorial pipelines.
