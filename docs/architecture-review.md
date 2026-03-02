# MotionStage Architecture Evaluation & Improvement Roadmap

> **Status legend**: `DONE` | `IN_PROGRESS` | `TODO` | `DEFERRED`

## Context

Full-system architecture review of the MotionStage/cinemotion codebase — a 14-crate Rust workspace with a QUIC-based real-time motion capture server, C FFI bridge for iOS/Swift, Python SDK, WebRTC video, mDNS discovery, and recording/export facilities.

**Scope:**
- `motionstage-server` (3,666 LOC) — session lifecycle, tick/publish loops
- `motionstage-core` (1,972 LOC) — scene graph, mappings, filters, ECS
- `motionstage-protocol` — control messages, session state machine, versioning
- `motionstage-transport-quic` — QUIC/TLS transport, control streams, datagrams
- `motionstage-sdk-swift` — C FFI + Swift wrapper + iOS app
- `motionstage-sdk-python` — PyO3 bindings
- `motionstage-recording` — CMTRK2 format, take catalog
- `motionstage-export-usd/chan` — USD and CHAN exporters
- `motionstage-media` + `motionstage-webrtc` — video signaling and WebRTC
- `motionstage-discovery` — mDNS advertisement

---

## Overall Assessment

The codebase is **architecturally sound and production-grade** in its core layers. Crate separation is clean, the concurrency model is correct (single coarse `Arc<RwLock<ServerState>>`, async-first with Tokio), the mapping/filter system is sophisticated, and test coverage of core transforms is excellent.

**Two major weak spots:**
1. **The FFI/SDK layer is tightly coupled to a single use-case** (camera tracking) and doesn't generalize.
2. **Certificate verification is skipped** — QUIC mandates TLS 1.3 (so traffic is encrypted), but `SkipServerVerification` means no certificate identity validation, leaving connections vulnerable to MITM on LAN.

---

## Key Design Tensions (Resolved)

### Generality vs. Ergonomics
**Resolution:** Both. The general `send_batch` API is the primary abstraction. A `CameraMotionFrame` convenience layer sits on top for the common camera-tracking use case — ergonomic without constraining other motion source types (hand tracking, body tracking, controllers).

### Synchronous FFI vs. Swift Concurrency
**Resolution:** Make it properly async at the FFI boundary. Swift 6 strict concurrency demands this. The correct implementation: the FFI exposes completion callbacks (C function pointers), Swift wraps them in `withCheckedThrowingContinuation`, and streaming events use `AsyncStream`. This integrates cleanly with structured concurrency and is the most performant path (no blocking thread per call, no `Task.detached` workarounds).

### Security vs. Zero-Config UX
**Resolution:** Zero-config UX is the priority; TOFU (Trust On First Use) is the right middle ground. QUIC already uses TLS 1.3, so traffic is encrypted — the gap is certificate identity (MITM protection). Server advertises cert fingerprint via mDNS TXT; iOS pins on first connection. Subsequent connections verify the fingerprint. No PKI, no user ceremony beyond first connect.

### Protocol Versioning
**Resolution:** We are in active development with no production compatibility requirements. Breaking changes are acceptable. Target protocol 2.0 (or reset to 1.0) for any restructuring work. Do not carry forward 1.x compatibility shims.

---

## Track 1: Protocol Design

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 1.1 | Typed attribute schema (`AttributeDescriptor` in `ClientHello`) | High | Large | `TODO` |
| 1.2 | Graceful disconnect message (`ClientGoodbye`) | Medium | Small | `TODO` |
| 1.3 | Decouple mode intent from mode state (`DataFlowState` × `RecordingState`) | Medium | Medium | `TODO` |
| 1.4 | Remove per-datagram protocol version revalidation | Low | Small | `TODO` |
| 1.5 | Field mask extensibility (addressed in Track 2 FFI) | High | Medium | `TODO` |

**1.1 Typed Attribute Schema**
- **Problem:** Attributes are free strings throughout. Typos (`"focal_lenght"`) produce zero mappings with no error — validation only happens at mapping time on the server. `ClientHello.advertised_attributes: Vec<String>` is the injection point.
- **Recommendation:** Introduce `AttributeDescriptor { path: String, value_type: AttributeKind }` in `motionstage-protocol`. Change `ClientHello.advertised_attributes` to `Vec<AttributeDescriptor>`. Server validates types at `RegisterRequest` time, rejecting mismatched devices at handshake rather than silently at mapping time.
- **Files:** `crates/motionstage-protocol/src/lib.rs`

**1.2 Graceful Disconnect Message**
- **Problem:** `disconnect()` calls `control.finish()` with no semantic distinction between clean shutdown vs. network drop. Server can't differentiate to provide useful diagnostics or operator UX.
- **Recommendation:** Add `ControlMessage::ClientGoodbye { reason: Option<String> }`. Client sends before `finish()`.
- **Files:** `crates/motionstage-protocol/src/lib.rs`, `crates/motionstage-sdk-swift/src/lib.rs`

**1.3 Decouple Mode Intent from Mode State**
- **Problem:** `Mode` conflates data-flow direction (`Idle`/`Live`) with recording activity (`Recording`/`Playback`). `Idle → Recording` is invalid even though it could logically mean "start recording immediately." `SetMode(Mode)` and `ModeState(Mode)` use the same type, making transition failure ambiguous.
- **Short-term:** Ensure `RegisterAccepted` carries current `Mode`; push `ModeState` proactively after every transition.
- **Long-term (2.0):** Split into `DataFlowState { Idle, Live }` and `RecordingState { Inactive, Recording, Playback }` as a composite.
- **Files:** `crates/motionstage-protocol/src/lib.rs`, `crates/motionstage-core/src/runtime.rs`

**1.4 Remove Per-Datagram Version Revalidation**
- **Problem:** `validate_wire_version` is called on every datagram. Version was already negotiated at handshake. Creates split-brain if either side is upgraded — rejects datagrams stamped with a newer compatible minor version.
- **Recommendation:** Strip version header from `MotionDatagramEnvelope`. Version is session-scoped state.
- **Files:** `crates/motionstage-transport-quic/src/lib.rs`

---

## Track 2: FFI Abstraction & Generalization

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 2.1 | Remove `MotionFrameFFI` positional coupling; add general batch update API | Critical | Medium | `TODO` |
| 2.2 | Replace CSV attribute API with array API | High | Small | `TODO` |
| 2.3 | Shared Tokio runtime across clients | Medium | Medium | `TODO` |
| 2.4 | Proper async Swift surface via C callbacks + continuations | High | Medium | `TODO` |
| 2.5 | C string ownership documentation in header | Medium | Small | `TODO` |

**2.1 Remove MotionFrameFFI Positional Coupling**
- **Problem:** `MotionFrameFFI` is camera-specific with implicit positional coupling (position=index 0, rotation=1, etc.). A hand-tracking or body-tracking developer has no generalization path. The `AttributeUpdateFrame` system in the transport layer is already general — only the FFI shim has positional semantics.
- **Recommendation:**
  - Add general batch update C API:
    ```c
    typedef struct {
        const char *attribute;      // "motion.position"
        const float *data;          // packed float values
        uint32_t component_count;   // 1, 2, 3, 4, 9, or 16
    } MotionAttributeUpdateC;

    int32_t motionstage_swift_client_send_batch(
        void *client,
        const MotionAttributeUpdateC *updates,
        uint32_t update_count
    );
    ```
  - Retain `MotionFrameFFI` and `send_motion_frame` as deprecated shims for the current iOS app.
  - Add `CameraMotionFrame` as a Swift-layer convenience type on top of `send_batch` (not in the C header).
- **Files:** `crates/motionstage-sdk-swift/src/lib.rs`, `crates/motionstage-sdk-swift/include/motionstage_swift.h`

**2.2 Replace CSV Attribute API**
- **Problem:** `motionstage_swift_client_new_multi(device_name, output_attributes_csv)` passes a CSV string. Ordering contract is invisible. CSV position maps to `MotionFrameFFI` field index — a maintenance hazard.
- **Recommendation:** Add `motionstage_swift_client_new_v2(device_name, attribute_count, attribute_names[])` taking an explicit count and array of C strings. Deprecate the CSV variant.
- **Files:** `crates/motionstage-sdk-swift/src/lib.rs`, `crates/motionstage-sdk-swift/include/motionstage_swift.h`

**2.3 Shared Tokio Runtime**
- **Problem:** Each `_new_multi()` creates a new multi-threaded Tokio runtime. Multiple clients → proportionally many thread pools.
- **Recommendation:** `OnceLock<Runtime>` global runtime. Add `motionstage_swift_runtime_init(thread_count: u32)` for host configuration. Clients hold `&'static Runtime` reference.
- **Files:** `crates/motionstage-sdk-swift/src/lib.rs`

**2.4 Proper Async Swift Surface**
- **Problem:** All FFI calls use `runtime.block_on()`, blocking the calling thread. The iOS app works around this with `Task.detached` — undiscoverable and incorrect for Swift 6 strict concurrency.
- **Recommendation (correct approach):** Expose async operations via C completion callbacks:
  ```c
  // Rust calls callback on completion; Swift wraps in withCheckedThrowingContinuation
  typedef void (*MotionStageConnectCallback)(int32_t status, const char *error, void *context);
  void motionstage_swift_client_connect_async(
      void *client,
      const char *server_addr,
      const char *pairing_token,
      const char *api_key,
      MotionStageConnectCallback callback,
      void *context
  );
  ```
  Swift wrapper:
  ```swift
  public func connect(serverAddress: String, ...) async throws {
      try await withCheckedThrowingContinuation { continuation in
          motionstage_swift_client_connect_async(ptr, serverAddress, ...) { status, error, ctx in
              let cont = Unmanaged<CheckedContinuation<Void, Error>>.fromOpaque(ctx!).takeRetainedValue()
              if status == 0 { cont.resume() } else { cont.resume(throwing: ...) }
          }
      }
  }
  ```
  For streaming events (connection state, mode changes), use `AsyncStream` with `Continuation`.
- **Files:** `crates/motionstage-sdk-swift/src/lib.rs`, `crates/motionstage-sdk-swift/include/motionstage_swift.h`, `swift/MotionStageClient/Sources/MotionStageClient/MotionStageClient.swift`

**2.5 C String Ownership Documentation**
- **Problem:** `session_id`, `device_id`, `last_error` return `*mut c_char` requiring manual `motionstage_swift_string_free`. No documentation in header.
- **Recommendation:** Add `// OWNERSHIP: caller must free with motionstage_swift_string_free()` to each returning function.
- **Files:** `crates/motionstage-sdk-swift/include/motionstage_swift.h`

---

## Track 3: Security

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 3.1 | Certificate pinning / TOFU via mDNS fingerprint | High | Large | `TODO` |
| 3.2 | Harden default auth mode (no silent fallback token) | Medium | Small | `TODO` |
| 3.3 | Session ID → UUID v4 | Low | Small | `TODO` |

> **Note on QUIC + TLS:** QUIC mandates TLS 1.3 — traffic **is** encrypted. The security gap is `SkipServerVerification` which bypasses certificate identity validation. The fix is certificate pinning, not adding TLS (which is already there).

**3.1 Certificate Pinning / TOFU**
- **Problem:** `QuicClient::new_insecure_for_local_dev` skips all certificate verification and is the only client constructor. API key and pairing token credentials travel over an unverified connection, enabling MITM on any LAN.
- **Recommendation:**
  1. Server computes SHA-256 fingerprint of its self-signed cert at startup.
  2. Advertise fingerprint in mDNS TXT record: `cert_fp=SHA256:abc123...`.
  3. iOS app reads fingerprint from `DiscoveredService` metadata; stores it per server (TOFU).
  4. Add `QuicClient::new_with_pinned_cert(fingerprint: [u8; 32])` with a verifier that checks only cert fingerprint match.
  5. Gate `new_insecure_for_local_dev` behind `#[cfg(feature = "insecure")]` for testkit/dev only.
- **Files:** `crates/motionstage-transport-quic/src/lib.rs`, `crates/motionstage-discovery/src/lib.rs`, `motionstage-ios/Services/DiscoveryService.swift`

**3.2 Harden Default Auth Mode**
- **Problem:** `ensure_auth` falls back to the hardcoded token `"motionstage"` when `pairing_token` is `None`, making `PairingRequired` mode trivially bypassable if the config field is unset.
- **Recommendation:** Return `Err(RejectCode::AuthFailed)` when mode requires auth but config field is `None`. Log a startup warning when `TrustedLan` mode is active.
- **Files:** `crates/motionstage-server/src/lib.rs`

**3.3 Session ID → UUID v4**
- **Problem:** `Uuid::now_v7()` is time-ordered and guessable to within a millisecond.
- **Recommendation:** Switch to `Uuid::new_v4()` (random). One-line change.
- **Files:** `crates/motionstage-server/src/lib.rs`

---

## Track 4: Transport

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 4.1 | Configurable timeouts via `ClientConfig` | Medium | Small | `TODO` |
| 4.2 | Reconnection logic with exponential backoff | High | Large | `TODO` |
| 4.3 | Application-layer `Ping` heartbeat from SDK | Medium | Small | `TODO` |
| 4.4 | Session idle timeout in `LeaseConfig` | Low | Small | `TODO` |

**4.1 Configurable Timeouts**
- **Problem:** `HANDSHAKE_TIMEOUT`, `MODE_REPLY_TIMEOUT`, `RESET_SCENE_TIMEOUT` are hardcoded `Duration::from_secs(5)` in the FFI crate. Busy LAN conditions cause spurious failures.
- **Recommendation:** Add a `MotionStageClientConfig` C struct. Add `_new_multi_with_config()` constructor.
- **Files:** `crates/motionstage-sdk-swift/src/lib.rs`, `crates/motionstage-sdk-swift/include/motionstage_swift.h`

**4.2 Reconnection Logic**
- **Problem:** A brief Wi-Fi disruption requires the user to manually reconnect and re-register. No auto-reconnect exists anywhere.
- **Recommendation:** `ReconnectPolicy { max_attempts, initial_delay_ms, max_delay_ms, backoff_factor }`. Background task monitors QUIC health and triggers reconnect with exponential backoff. Expose connection state via `AsyncStream` events (see 2.4).
- **Files:** `crates/motionstage-sdk-swift/src/lib.rs`

**4.3 Application-Layer Heartbeat**
- **Problem:** `Ping/Pong` messages exist but are never sent by the SDK. Idle clients (no motion datagrams) have stale mapping heartbeats on the server.
- **Recommendation:** Reconnection background task sends `Ping` every 2 seconds when no motion datagrams have been sent recently.
- **Files:** `crates/motionstage-sdk-swift/src/lib.rs`

**4.4 Session Idle Timeout**
- **Problem:** No session-level idle timeout. Crashed clients leave `Active` sessions until QUIC transport timeout.
- **Recommendation:** Add `session_idle_timeout_ns` to `LeaseConfig`. Tick loop evicts stale sessions.
- **Files:** `crates/motionstage-core/src/model.rs`, `crates/motionstage-server/src/lib.rs`

---

## Track 5: Swift SDK

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 5.1 | Typed attribute namespace (`Attribute.Motion.position`, `Attribute.Camera.focalLength`) | High | Medium | `TODO` |
| 5.2 | Typed error enum (replace raw `statusCode: Int32`) | Medium | Small | `TODO` |
| 5.3 | State change `AsyncStream` events (connection state, mode) | Medium | Medium | `TODO` |
| 5.4 | Decouple camera from SDK core; `CameraMotionFrame` as opt-in extension | Medium | Medium | `TODO` |

**5.1 Typed Attribute Namespace**
- **Recommendation:**
  ```swift
  public enum Attribute {
      public enum Motion {
          public static let position = AttributeKey("motion.position", type: .vec3f)
          public static let rotation = AttributeKey("motion.rotation", type: .quatf)
          public static let velocity = AttributeKey("motion.velocity", type: .vec3f)
      }
      public enum Camera {
          public static let focalLength   = AttributeKey("camera.focal_length",   type: .float32)
          public static let focusDistance = AttributeKey("camera.focus_distance",  type: .float32)
          public static let aperture      = AttributeKey("camera.aperture",        type: .float32)
      }
  }
  ```
  `MotionStageClient.init` accepts `[AttributeKey]` instead of `[String]`. Remove `StandardAttributes` from the iOS app.
- **Files:** `swift/MotionStageClient/Sources/MotionStageClient/MotionStageClient.swift`

**5.2 Typed Error Enum**
```swift
public enum MotionStageError: Error {
    case invalidArgument(String), notConnected, alreadyConnected
    case protocolError(String), transportError(String), internalError(String)
}
```

**5.3 AsyncStream Events**
- Connection state changes, mode changes, and errors surfaced via `AsyncStream<ConnectionEvent>` on the client. Powers the reactive `MotionStageService` without polling.
- **Files:** `swift/MotionStageClient/Sources/MotionStageClient/MotionStageClient.swift`

---

## Track 6: iOS App

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 6.1 | Fix CoreMotion velocity semantics (bug: using acceleration as velocity) | Critical | Small | `TODO` |
| 6.2 | Dynamic field mask (only send changed camera scalars) | Medium | Small | `TODO` |
| 6.3 | Discovery: read port from mDNS TXT records (eliminates NWConnection probe) | Medium | Small | `TODO` |
| 6.4 | Frame send error surfacing (rolling error counter → connection error state) | Medium | Small | `TODO` |
| 6.5 | Remove duplicate `import ARKit` in MotionTrackingService.swift | Low | Small | `TODO` |

**6.1 Fix CoreMotion Velocity (Bug)**
- **Problem:** `handleCoreMotion()` uses `motion.userAcceleration` (units of g, not integrated) as "velocity." This is physically wrong — any DCC tool treating it as velocity in m/s will produce bad results.
- **Fix:** Send `.zero` for velocity in CoreMotion mode (position is also zero; no spatial data available). Add comment explaining limitation.
- **File:** `motionstage-ios/motionstage-ios/Services/MotionTrackingService.swift` (~line 151)

**6.2 Dynamic Field Mask**
- Track `pendingCameraFields: FieldMask` dirty flag. Set when camera scalars change. Typical tick sends only `.allMotion` (3 updates instead of 6).

**6.3 Discovery Port from TXT**
- Depends on server-side fix (10.3). iOS reads port from `NWBrowser.Result.metadata` TXT records — eliminates the temporary NWConnection probe that causes the 10-second resolution delay.
- **File:** `motionstage-ios/motionstage-ios/Services/DiscoveryService.swift`

**6.4 Frame Send Error Surfacing**
- Rolling counter: after 30 consecutive frame send failures (~0.5s at 60 Hz), set `connectionState = .error(...)`. Reset counter on success.
- **File:** `motionstage-ios/motionstage-ios/Services/MotionStageService.swift`

---

## Track 7: Server & Core Framework

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 7.1 | Mapping lookup indexing + deduplicate `source_output_matches` | Medium | Small | `TODO` |
| 7.2 | Control message handler decomposition (extract from monolithic match) | Medium | Medium | `TODO` |
| 7.3 | Remove `bevy_ecs` from RuntimeCore (single unused resource) | Low | Medium | `TODO` |
| 7.4 | ServerState substructure (only after profiling confirms contention) | Low | Large | `DEFERRED` |
| 7.5 | Session event recording markers (`ClientJoined`, `ClientLeft`) | Low | Small | `TODO` |
| 7.6 | Snapshot clone cost (only after profiling) | Low | Small | `DEFERRED` |

**7.1 Mapping Lookup Indexing**
- **Problem:** `apply_updates()` does O(M) linear scan through all mappings per motion update. `source_output_matches` is duplicated between `core/runtime.rs` (lines 928–943) and `server/lib.rs` (lines 2241–2257).
- **Recommendation:** Add `mapping_index: HashMap<(Uuid, String), MappingId>` to `RuntimeCore`. Move deduplicated `source_output_matches` to `motionstage-core` shared utility.

**7.2 Control Message Handler Decomposition**
- **Problem:** `handle_quic_peer` is ~2,000 lines with a flat match on 40+ `ControlMessage` variants. Hard to extend or test in isolation.
- **Recommendation:** Extract focused async functions: `handle_set_mode`, `handle_recording_control`, `handle_playback_control`, `handle_bake_cursor`, `handle_video_signaling`. Match becomes a routing dispatch.

**7.3 Remove bevy_ecs from RuntimeCore**
- **Problem:** `RuntimeCore` uses `bevy_ecs::World` to store one resource: `RuntimeStats { tick_count: u64 }`. Pulls in ~40 transitive crates, hurting compile time.
- **Recommendation:** Move `tick_count` to a direct field on `RuntimeCore`. Remove `bevy_ecs` workspace dependency entirely.

---

## Track 8: Python SDK

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 8.1 | Typed return types + `.pyi` stubs | High | Medium | `TODO` |
| 8.2 | Attribute value type disambiguation (explicit type tags) | Medium | Small | `TODO` |
| 8.3 | Streaming/event API + `TakeBakeCursor` as iterator | Medium | Large | `TODO` |
| 8.4 | Dict spec validation with descriptive Python errors | Medium | Small | `TODO` |

**8.1 Typed Return Types**
- **Problem:** All methods return raw tuples with mixed types and undocumented positions. No type stubs, no type checking support.
- **Recommendation:** `TypedDict` / `dataclass` returns for all compound results. Add `py.typed` marker and `.pyi` stub file.

**8.2 Attribute Type Disambiguation**
- **Problem:** Numeric value inference is ambiguous (`int` → `Int32`, `float` → `Float64`, can't distinguish `Float32`).
- **Recommendation:** Require explicit type tags: `{"type": "float32", "value": 1.0}`. Retain inference as lenient fallback with deprecation warning.

**8.3 Streaming/Event API**
- **Problem:** Session inspection requires polling. Bake cursor API (open/read/seek/close) is awkward vs. Python idioms.
- **Recommendation:** `TakeBakeCursor` context manager yielding `BakeFrame` objects. `asyncio`-compatible event subscription for session events.

---

## Track 9: Export Crates

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 9.1 | USD typed time-sampled output (replace custom string metadata) | Medium | Large | `TODO` |
| 9.2 | Export from live stream snapshots | Low | Medium | `TODO` |
| 9.3 | Export customization options (fps, attribute filter, object filter) | Low | Small | `TODO` |

**9.1 USD Export Quality**
- **Problem:** Current exporter outputs attributes as `custom string` metadata rather than typed USD properties with time samples. Not USD-schema-compliant; DCC tools expecting typed time-sampled attributes won't work with the output.
- **Recommendation:** Rewrite to use USD time samples:
  ```
  def Xform "Object_<id>" {
      float3 xformOp:translate.timeSamples = { 0: (x, y, z), 1001: (x2, y2, z2) }
  }
  ```

---

## Track 10: Media & WebRTC

| # | Item | Priority | Effort | Status |
|---|------|----------|--------|--------|
| 10.1 | Codec negotiation (`VideoCodec` field in `VideoStreamDescriptor`) | Medium | Medium | `TODO` |
| 10.3 | Port in mDNS TXT records (enables iOS discovery fix 6.3) | Medium | Small | `TODO` |

> **Note:** STUN/TURN configuration (10.2) is **not needed** — this system runs on LAN only.

**10.1 Codec Negotiation**
- **Problem:** `add_h264_track` hardcodes H.264 with no fallback. Devices without H.264 hardware encoding have no alternative.
- **Recommendation:** Add `codec: VideoCodec { H264, Hevc, Vp9, Av1 }` to `VideoStreamDescriptor`. Negotiate codec in `negotiate_stream()` analogous to HDR negotiation.

**10.3 Port in mDNS TXT Records**
- **Problem:** `DiscoveryAdvertisement.to_txt_records()` doesn't include the port, forcing the iOS discovery service to probe with a temporary NWConnection.
- **Recommendation:** Add `format!("port={}", self.bind_port)` to `to_txt_records()`.
- **File:** `crates/motionstage-discovery/src/lib.rs`

---

## Phase Execution Plan

### Phase 1 — Correctness & Quick Wins
*No breaking changes. No protocol version bump required.*

- [ ] **6.1** Fix CoreMotion velocity bug (`.zero` in CoreMotion mode)
- [ ] **6.5** Remove duplicate `import ARKit`
- [ ] **6.4** Frame send error surfacing (rolling counter)
- [ ] **6.2** Dynamic field mask in MotionPipeline
- [ ] **5.2** Typed Swift error enum
- [ ] **2.5** C string ownership docs in header
- [ ] **3.2** Harden default auth mode
- [ ] **3.3** Session ID → UUID v4
- [ ] **7.3** Remove `bevy_ecs` from RuntimeCore
- [ ] **7.1** Mapping index + deduplicate `source_output_matches`
- [ ] **4.1** Configurable timeouts via `ClientConfig`
- [ ] **10.3** Port in mDNS TXT records (server-side prereq for 6.3)
- [ ] **6.3** iOS discovery reads port from TXT (eliminates NWConnection probe)

### Phase 2 — FFI Generalization
*New APIs alongside deprecated existing ones.*

- [ ] **2.2** Array-based attribute API (`_new_v2`), deprecate CSV constructor
- [ ] **2.3** Shared Tokio runtime (`OnceLock<Runtime>`)
- [ ] **2.4** Async Swift surface via C callbacks + `withCheckedThrowingContinuation`
- [ ] **5.1** Typed attribute namespace in Swift SDK
- [ ] **5.3** `AsyncStream` connection state events
- [ ] **2.1** General `send_batch` API; `MotionFrameFFI` kept as deprecated shim
- [ ] **5.4** `CameraMotionFrame` as Swift extension target, decoupled from SDK core
- [ ] **7.2** Extract control message handlers from monolithic match
- [ ] **8.1** Python SDK typed return types + `.pyi` stubs
- [ ] **8.2** Attribute type disambiguation
- [ ] **8.4** Dict spec validation

### Phase 3 — Protocol Evolution (Protocol 2.0)
*Coordinated client + server updates. No backwards compat needed.*

- [ ] **1.2** `ClientGoodbye` message
- [ ] **1.4** Remove per-datagram version header from `MotionDatagramEnvelope`
- [ ] **1.1** `AttributeDescriptor` in `ClientHello.advertised_attributes`
- [ ] **1.3** Mode decoupling (`DataFlowState` × `RecordingState`)
- [ ] **4.3** Application-layer `Ping` heartbeat from SDK client
- [ ] **4.2** Reconnection logic with `ReconnectPolicy`
- [ ] **7.5** Session event recording markers (`ClientJoined`/`ClientLeft`)
- [ ] **4.4** Session idle timeout in `LeaseConfig`

### Phase 4 — Security Hardening
*Requires iOS UI changes for TOFU pairing flow.*

- [ ] **3.1** Certificate pinning / TOFU (server fingerprint → mDNS TXT → iOS pin)

### Phase 5 — Architecture Refactor
*Long-term structural improvements. Requires careful coordination.*

- [ ] **8.3** Python streaming/event API + `TakeBakeCursor` iterator
- [ ] **9.1** USD typed time-sampled export
- [ ] **9.2** Export from live stream snapshots
- [ ] **9.3** Export customization options
- [ ] **10.1** Video codec negotiation
- [ ] **7.4** `ServerState` substructure *(only if profiling confirms contention)*
- [ ] **7.6** Snapshot `Arc` clone optimization *(only if profiling confirms cost)*

---

## Critical Files

| File | Tracks |
|------|--------|
| `crates/motionstage-sdk-swift/src/lib.rs` | 2.1, 2.2, 2.3, 2.4, 4.1, 4.2, 4.3 |
| `crates/motionstage-sdk-swift/include/motionstage_swift.h` | 2.1, 2.2, 2.4, 2.5 |
| `crates/motionstage-protocol/src/lib.rs` | 1.1, 1.2, 1.3, 1.4 |
| `crates/motionstage-transport-quic/src/lib.rs` | 1.4, 3.1 |
| `crates/motionstage-server/src/lib.rs` | 3.2, 7.2, 7.4, 7.5 |
| `crates/motionstage-core/src/runtime.rs` | 7.1, 7.3, 7.6 |
| `crates/motionstage-core/src/model.rs` | 1.3, 4.4 |
| `crates/motionstage-discovery/src/lib.rs` | 10.3 |
| `crates/motionstage-sdk-python/src/lib.rs` | 8.1, 8.2, 8.3, 8.4 |
| `crates/motionstage-export-usd/src/lib.rs` | 9.1, 9.2, 9.3 |
| `swift/MotionStageClient/Sources/MotionStageClient/MotionStageClient.swift` | 2.4, 5.1, 5.2, 5.3, 5.4 |
| `motionstage-ios/motionstage-ios/Services/MotionTrackingService.swift` | 6.1, 6.5 |
| `motionstage-ios/motionstage-ios/Services/MotionPipeline.swift` | 6.2 |
| `motionstage-ios/motionstage-ios/Services/DiscoveryService.swift` | 6.3 |
| `motionstage-ios/motionstage-ios/Services/MotionStageService.swift` | 6.4 |

---

## Verification

When implementing each phase:
- `cargo test -q` — all Rust unit + integration tests pass
- `cargo clippy --workspace` — no new warnings
- `python3 -m pytest -q python/tests` — Python SDK tests pass
- Build XCFramework: `./scripts/build-swift-ios.sh`
- iOS app: build and run on simulator, verify ARKit mode sends correct motion data and CoreMotion mode sends zero velocity
