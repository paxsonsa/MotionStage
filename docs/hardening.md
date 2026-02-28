# CineMotion v1 Hardening Gates

## Performance Gates

- Ingest target: `120 Hz` sustained per active motion source.
- Publish target: `60 Hz` scene publication cadence.
- Baseline soak tests:
  - `cinemotion-testkit::soak_test_hits_motion_pipeline`
  - `cinemotion-testkit::soak_test_reaches_120hz_ingest_target_window`

## Metrics Contract

`ServerMetrics` is the v1 baseline metrics surface:

- `accepted_sessions`
- `rejected_sessions`
- `motion_datagrams`
- `motion_updates`
- `signaling_messages`

These counters are monotonic over process lifetime and are used by integration tests and runtime health checks.

## Tracing Contract

Server emits structured tracing events for:

- server lifecycle start/stop
- session discovery and registration decisions
- motion datagram ingest

Operators should route tracing to their observability backend and create dashboards for:

- registration success/reject ratio
- motion datagram throughput
- signaling volume by session

## Failure Budget (v1 Default)

- Session admission failures (`rejected_sessions / (accepted + rejected)`) should remain below `1%` on trusted LAN deployments.
- Motion ingest drop events (difference between client-sent samples and `motion_updates`) should remain below `0.1%` during soak validation.
- If any budget is violated in certification tests, release candidate is blocked pending remediation.

## CI Gates

- Rust compile + tests: `cargo build --verbose`, `cargo test --verbose`.
- Python package tests: `python -m pip install -e ./python` followed by `python -m pytest -q python/tests`.
- Native extension gate: `maturin build --manifest-path crates/cinemotion-sdk-python/Cargo.toml` and import smoke test for `cinemotion_sdk_rust`.
