# MotionStage v1 Hardening Gates

## Performance Gates

- Ingest target: `120 Hz` sustained per active motion source.
- Publish target: `60 Hz` scene publication cadence.
- Baseline soak tests:
  - `motionstage-testkit::soak_test_hits_motion_pipeline`
  - `motionstage-testkit::soak_test_reaches_120hz_ingest_target_window`

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

## Video pipeline

The DCC-render-to-client video path streams the host viewport to WebRTC video
sinks (e.g. an iPad) as H.264. It is a push pipeline authored by the DCC and
relayed by the server; the server never pulls or re-encodes.

### End-to-end data path

```
DCC render (Blender main draw thread)
  -> pyo3 push_video_frame / push_video_frame_bgra   (returns immediately)
     -> enqueue_video_frame: single-flight gate, then rt.spawn (off main thread)
        -> ServerHandle::take_keyframe_needed()  (force IDR if a peer just joined)
        -> H264Encoder::encode_rgba / encode_bgra  (Annex B access unit)
           -> ServerHandle::push_video_frame(encoded, duration)
              -> for each video peer with a negotiated track:
                 WebRtcSession::write_sample()  -> TrackLocalStaticSample -> RTP
```

Nodes and guarantees:

- **Single-flight drop-frame policy** (`enqueue_video_frame`, pyo3): a frame
  that arrives while an encode+push is still in flight is *dropped*, not
  queued. Live feedback wants the freshest frame; an unbounded queue would grow
  memory and add latency under backpressure. The in-flight flag is released by
  an RAII guard so it is cleared on every task exit path (encode error, missing
  encoder, or success).
- **Off-thread encode**: the CPU-bound encode and the network push run on the
  Tokio runtime, never on the caller's thread. Inline encode on Blender's draw
  thread froze the viewport and held the GIL, starving the SDK poller.
- **Keyframe-on-join**: when a track is added for a newly-negotiated peer
  (`create_video_offer` / `handle_video_signal`), the server arms a one-shot
  `video_keyframe_needed` latch. The next encode consumes it via
  `take_keyframe_needed()` and calls `H264Encoder::force_keyframe()`, so the new
  peer receives an IDR (SPS+PPS+IDR) before its first inter-frame and can start
  decoding immediately. The loopback delivery test replicates this exact
  sequence — take the latch, `force_keyframe`, encode — then asserts the
  resulting first access unit is a complete SPS+PPS+IDR (via the Annex B NAL
  scan) *and* that it reaches the answering peer over the wire. So the guarantee
  is proven as a produced-and-delivered IDR, not merely as the boolean latch.
- **Descriptor precondition**: `push_video_frame` rejects frames with
  `master video descriptor not set` until the DCC publishes a master
  `VideoStreamDescriptor`. This mirrors the gate in `ensure_video_session_ready`
  so the ingest path never ships frames the negotiation layer did not sanction.
- **No-peers no-op**: pushing with the descriptor set but no joined peers is a
  valid no-op (the DCC may render before any client connects). The frame
  timestamp is still recorded for liveness in `video_stream_status`.

### Proof and observability

The video path is proven at two levels — a fast relay check and a real
over-the-wire delivery test — plus the descriptor/encoder unit checks:

- **Real wire delivery** (`motionstage-server::push_video_frame_delivers_rtp_to_loopback_peer`):
  the authoritative end-to-end proof. It stands up an in-process webrtc-rs
  answering peer, runs a full offer/answer exchange against the *real* server
  peer (`create_video_offer` produces the offer; the answer is consumed through
  `handle_video_signal`), connects ICE + DTLS over host loopback, then pushes
  encoder-produced frames through `ServerHandle::push_video_frame` and asserts
  RTP actually arrives at the answering peer's `on_track` (a packet is read from
  the remote track). Candidates are exchanged non-trickle (each side embeds its
  gathered host candidates in the SDP), so no separate signaling channel is
  needed. This test also drives the keyframe-on-join path the way production
  does — see the keyframe bullet above — and asserts the delivered first access
  unit is a complete SPS+PPS+IDR. It runs in well under a second on the CI
  sandbox. If a host cannot open loopback UDP, this is the one test that would
  fail (at the ICE-connect wait); everything below still holds.
- **Relay seam** (`motionstage-server::push_video_frame_relays_to_track_writer_and_arms_keyframe_on_join`):
  a faster check that drives the real server path (set descriptor -> activate a
  `VideoSink` -> `create_video_offer` adds an H.264 track -> push 3
  encoder-produced frames) up to `WebRtcSession::write_sample`. The observable
  seam is a per-peer `frames_written` counter (`VideoPeerSession`) incremented
  on every successful `write_sample`, surfaced by
  `ServerHandle::video_frames_written(device_id)`. It also asserts the
  keyframe-on-join latch fires once and is one-shot. This proves the *relay*
  seam only, **not** wire delivery: it applies no remote answer/ICE, so
  `TrackLocalStaticSample::write_sample` returns `Ok` against an unbound track.
  Delivery is proven by the loopback test above.
- `push_video_frame_rejected_without_master_descriptor` and
  `push_video_frame_with_no_peers_is_noop` cover the descriptor precondition and
  the no-peers no-op.
- `motionstage-media::first_frame_from_fresh_encoder_is_idr_access_unit` and
  `encode_1280x720_produces_valid_h264` assert the first encoded frame is a
  complete access unit (SPS type 7 + PPS type 8 + IDR type 5) by scanning
  Annex B NAL units (`motionstage_media::encoder::annexb_nal_types`).

### Encode throughput baseline

Timed by `motionstage-media::encode_throughput_1280x720` (`--release`, run with
`--nocapture`). Numbers below are from the CI sandbox (shared/underclocked); a
DCC workstation with a dedicated core encodes several times faster.

| Content (1280x720, openh264 baseline) | ms/frame (release, CI sandbox) | approx fps |
| --- | --- | --- |
| High-entropy worst case (per-pixel noise) | ~57 ms | ~18 fps |
| Realistic viewport gradient (compressible) | ~39 ms | ~26 fps |

Interpretation: the debug build (~315 ms/frame) is not representative — always
measure encode in release. The single-flight drop-frame policy means the encoder
never falls behind: if a frame cannot be encoded before the next arrives it is
dropped, so end-to-end latency stays bounded to roughly one encode interval
regardless of source cadence. To sustain the `60 Hz` publish target for video on
constrained hosts, lower the descriptor resolution/fps or run the encode on a
dedicated core; the pipeline degrades by dropping frames, never by queueing.

## CI Gates

- Rust compile + tests: `cargo build --verbose`, `cargo test --verbose`.
- Video pipeline: `cargo test -p motionstage-server -p motionstage-webrtc -p motionstage-media`
  covers the loopback wire-delivery proof, the relay seam, WebRTC track/session,
  and encoder access-unit checks. The loopback test needs host loopback UDP; it
  runs in CI without special privileges.
- Python package tests: `python -m pip install -e ./python` followed by `python -m pytest -q python/tests`.
- Native extension gate: `maturin build --manifest-path crates/motionstage-sdk-python/Cargo.toml --features extension-module` and import smoke test for `motionstage_sdk_rust`.
- Swift iOS SDK gate: `./scripts/build-swift-ios.sh` followed by `xcodebuild -scheme MotionStageClient -destination 'generic/platform=iOS Simulator' build` in `swift/MotionStageClient`.
