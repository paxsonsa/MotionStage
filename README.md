# CineMotion v1

CineMotion is a sidecar, server-authoritative runtime for virtual camera and motion workflows.

## Implemented baseline

- Headless Bevy ECS-backed runtime core (`cinemotion-core`)
- Session + handshake model (`cinemotion-server`)
- Runtime lifecycle ownership with sidecar start/stop, QUIC accept loop, discovery, and scheduler loops (`cinemotion-server`)
- Mapping lease/lock policy with recording freeze rules (`cinemotion-core`)
- Component-mask transform engine for scalar/vector attribute routing (`cinemotion-core`)
- QUIC/WebRTC-oriented protocol and transport contracts (`cinemotion-protocol`, `cinemotion-media`)
- QUIC transport implementation with control streams + motion datagrams (`cinemotion-transport-quic`)
- Server-owned WebRTC peer session path with SDP/ICE handling (`cinemotion-server`, `cinemotion-webrtc`)
- HDR10 video descriptor model + SDR fallback negotiation (`cinemotion-media`)
- Native `.cmtrk` recording format with `CMTRK2` markers + `CMTRK1` backward compatibility (`cinemotion-recording`)
- Deterministic USD/CHAN exporters (`cinemotion-export-usd`, `cinemotion-export-chan`)
- CLI skeleton (`cinemotion-cli`)
- Test harness (`cinemotion-testkit`)
- Python strict OOP delegate SDK + optional Rust bridge (`python/cinemotion_sdk`, `cinemotion-sdk-python`)

## Workspace layout

- `/crates/cinemotion-protocol`
- `/crates/cinemotion-core`
- `/crates/cinemotion-server`
- `/crates/cinemotion-discovery`
- `/crates/cinemotion-media`
- `/crates/cinemotion-transport-quic`
- `/crates/cinemotion-recording`
- `/crates/cinemotion-export-usd`
- `/crates/cinemotion-export-chan`
- `/crates/cinemotion-cli`
- `/crates/cinemotion-testkit`
- `/python/cinemotion_sdk`

Implementation tracker: [`docs/tasks.md`](/Users/apaxson/work/projects/cinemotion/docs/tasks.md)
Hardening gates: [`docs/hardening.md`](/Users/apaxson/work/projects/cinemotion/docs/hardening.md)

## Run

```bash
cargo test
cargo run -p cinemotion-cli -- serve
cargo run -p cinemotion-cli -- simulate --bind 127.0.0.1:7788 --sample-hz 120
python -m pip install -e ./python
python -m pytest -q python/tests
```

`simulate` starts a demo motion source with a mapped `vec3` sine wave and an interactive shell:
- `start` / `stop`
- `record start [path]` / `record stop`
- `amp <value>` / `freq <value>`
- `status`
