# MotionStage v1 Completion Matrix

Status legend: `DONE`, `IN_PROGRESS`, `TODO`

| Check | Phase | Body of Work | Detailed Steps | Exit Criteria | Status |
|---|---|---|---|---|---|
| [x] | P1 | Runtime Lifecycle | Server owns QUIC runtime handle, discovery publisher, and scheduler tasks. `start()/stop()` manage resource lifecycle and expose effective bind address. | `serve` starts runtime and graceful stop tears down runtime-owned resources. | DONE |
| [x] | P2 | QUIC Video Signaling | Added control-message variants for signaling/offer flow and explicit protocol errors. QUIC peer loop handles signaling and drain requests. | Offer/answer/ICE exchange works over QUIC control channel only. | DONE |
| [x] | P3 | Mapping Transform Engine | Implemented `component_mask` validation and transform execution for scalar/vector routing and vector subset updates. | Mapping transform tests cover scalar->vector, vector->scalar, vector subset copy, and invalid masks/pairs. | DONE |
| [x] | P4 | Recording Markers (`CMTRK2`) | Added `CMTRK2` writer/reader with marker events and dual reader compatibility for legacy `CMTRK1`. Server emits mode/mapping markers in recording paths. | Marker-inclusive recording roundtrip passes and legacy files remain readable. | DONE |
| [x] | P5 | Tick/Publish Scheduler | Added configurable tick/publish scheduler loops using `tick_hz` and `publish_hz`; publish snapshots and counters are tracked in server metrics. | Scheduler counters progress and remain observable through metrics APIs. | DONE |
| [x] | P6 | Python Packaging + Native Bridge | Python package is installable via `pip install -e ./python`; `maturin` config added for Rust extension build; fallback behavior preserved. | Imports/tests succeed without `PYTHONPATH`; native wheel builds and imports. | DONE |
| [x] | P7 | Hardening + Observability | Expanded soak tests (multi-client + scheduler checks), structured tracing for signaling/scheduler/motion paths, and CI gates for Rust/Python/native packaging. | CI matrix and local validation pass. | DONE |
| [x] | P8 | Docs + Final Matrix | Updated protocol/hardening/readme docs for signaling, recording compatibility, runtime lifecycle, and quality gates. | Docs align with implementation and no stale TODO rows remain. | DONE |

## Validation Snapshot

- `cargo test -q`
- `python3 -m pip install -e ./python`
- `python3 -m pytest -q python/tests`
- `python3 -m maturin build --manifest-path crates/motionstage-sdk-python/Cargo.toml --features extension-module -o target/wheels`
- `python3 -m pip install --force-reinstall target/wheels/*.whl`
- `python3 -c "import motionstage_sdk_rust"`
