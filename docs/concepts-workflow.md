# Concepts and Workflow

## Core Concepts

- `Scene`: logical graph of objects and attributes
- `SceneObject`: named object with typed attributes
- `SceneAttribute`: value + live/record flags + filter chain
- `Mapping`: source output -> target scene/object/attribute binding
- `Mode`: `Idle`, `Live`, `Recording`
- `Baseline`: per-attribute `default_value` used for relative motion composition and explicit reset/commit actions
- `Session`: authenticated client lifecycle with negotiated roles/features

## End-to-End Workflow

1. Boot runtime with `serve` or `simulate`.
2. Device performs handshake and becomes `Active`.
3. Scene is loaded and active scene is selected.
4. Mappings are created from device outputs to scene attributes.
5. Runtime enters `Live` mode.
6. Motion updates are ingested and applied through transform/filter pipeline (`baseline + delta` for transform-capable targets).
7. Publish loop snapshots runtime state at `publish_hz`.
8. Optional recording writes frame + marker timeline as `.cmtrk`.
9. Recording can be exported to USD/CHAN for DCC workflows.

## Quickstart Workflow (Fastest)

```bash
cargo run -p motionstage-cli -- simulate --server-bind 127.0.0.1:0 --sample-hz 120
```

In interactive prompt:

```text
start
record start recordings/demo.cmtrk
status
record stop
quit
```

What `simulate` does automatically:
- Starts a server runtime
- Creates one demo scene/object/attribute (`position` as `vec3`)
- Creates and validates a mapping for `demo.position`
- Connects one simulated motion client over QUIC and drives it to `Active`
- Emits sine-wave samples at the selected rate

`simulate` can also run in client-only mode (`--connect <addr>` or
`--connect discover[:service-name]`) to attach to an existing server.
In that mode it does not create scenes or mappings.

## Mapping and Lease Workflow

- Mapping creation validates target path and component mask.
- Active mapping ownership is exclusive per target attribute.
- Locked mappings cannot be modified until unlocked.
- Disconnect/heartbeat timeouts drive reclaim behavior.
- Reclaim grace protects against short disconnect churn.
- Relative motion is server-authoritative:
  - scalar/vector targets compose additively from baseline
  - `quatf` composes via normalized quaternion multiplication
  - `mat4f` composes via matrix multiplication (`base * delta`)
- Non-composable targets continue to use absolute assignment.

## Baseline Control Workflow

- `reset_scene_to_baseline(scene_id)` restores active values from defaults for the scene.
- `commit_scene_baseline(scene_id)` promotes active values to new defaults for the scene.
- `commit_object_baseline(scene_id, object_id)` promotes active values to new defaults for one object.
- Baseline actions are explicit and are not triggered by mode transitions.

## Recording Workflow

- `start_recording(path, now_ns)`:
  - Ensures active scene exists
  - Moves mode into `Recording`
  - Seeds timeline with mode and active mapping markers
- Each ingest update adds frame data for mapped attributes
- `stop_recording()`:
  - Writes `CMTRK2` output
  - Appends terminal mode transition marker
  - Returns manifest with frame count and IDs

## Observability Workflow

`ServerMetrics` provides baseline counters:
- `accepted_sessions`
- `rejected_sessions`
- `motion_datagrams`
- `motion_updates`
- `signaling_messages`
- `scheduler_ticks`
- `publish_ticks`

Use these with tracing events to validate throughput and reliability against hardening targets.
