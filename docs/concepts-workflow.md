# Concepts and Workflow

## Core Concepts

- `Scene`: logical graph of objects and attributes
- `SceneObject`: named object with typed attributes
- `SceneAttribute`: value + live/record flags + filter chain
- `Mapping`: source output -> target scene/object/attribute binding
- `Mode`: `Idle`, `Live`, `Recording`
- `Session`: authenticated client lifecycle with negotiated roles/features

## End-to-End Workflow

1. Boot runtime with `serve` or `simulate`.
2. Device performs handshake and becomes `Active`.
3. Scene is loaded and active scene is selected.
4. Mappings are created from device outputs to scene attributes.
5. Runtime enters `Live` mode.
6. Motion updates are ingested and applied through transform/filter pipeline.
7. Publish loop snapshots runtime state at `publish_hz`.
8. Optional recording writes frame + marker timeline as `.cmtrk`.
9. Recording can be exported to USD/CHAN for DCC workflows.

## Quickstart Workflow (Fastest)

```bash
cargo run -p cinemotion-cli -- simulate --bind 127.0.0.1:7788 --sample-hz 120
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
- Bootstraps one active simulated motion client
- Emits sine-wave samples at the selected rate

## Mapping and Lease Workflow

- Mapping creation validates target path and component mask.
- Active mapping ownership is exclusive per target attribute.
- Locked mappings cannot be modified until unlocked.
- Disconnect/heartbeat timeouts drive reclaim behavior.
- Reclaim grace protects against short disconnect churn.

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
