# MotionStage Companion UI: Architecture Decision

> **Implementation status (Phase 1 built, Blender):** Shipped behind the
> `companion-ui` cargo feature on `motionstage-server` (`src/companion_ui.rs`): an axum
> HTTP+WebSocket listener spawned on the SDK's Tokio runtime, the full `ServerToUi`
> state-push schema + Group A/B command path, a React SPA in `crates/motionstage-server/ui/`
> embedded via `rust_embed`, the `start_companion_ui()/companion_ui_url()` FFI, and a
> Blender "Open Companion UI" button. See `companion-ui-blueprint.md` for the build map.
> **Deferred:** `/ws` token + Origin validation (plumbed, not enforced — localhost MVP);
> Phases 2–4 (Tauri, SDK logic migration, second DCC).
> **Build order:** `npm run build` in `ui/` (commits `ui/dist/`) must precede the cargo
> build — `rust_embed`'s `#[folder = "ui/dist"]` reads at compile time.

**Status:** Decision-grade. Opinionated on purpose.
**Origin:** Synthesized from a four-angle research workflow (prior art, tech tradeoffs, codebase fit, cross-DCC contract) run 2026-06-14, grounded against the live tree.

---

## TL;DR

Build it, but build it for the right reason. The Blender refresh-cycle pain is the weak
justification and it has a cheap fix. The strong justification is the five-DCC roadmap:
native panels will never give an identical operator UX across Blender, Unreal, Unity,
Houdini, and Maya, and a web UI served from the shared Rust runtime is the only thing that can.

The architecture: one runtime, one extra WebSocket listener on the Tokio runtime we already
embed, a React SPA served from that runtime, browser for the MVP and Tauri when we want it
floating. Reuse the existing command verbs over the new socket, add a UI-shaped state-push
schema. The UI is 100% DCC-agnostic. Each new DCC writes one thin adapter.

The honest catch: the backend listener is trivial, but a React frontend (and later a signed
Tauri app) is a real second product for a small team. Price that in.

---

## 1. Verdict: worth building?

**Yes, conditional on the cross-DCC roadmap being real.** If Blender were the only target for
the foreseeable future, do the cheap fix below and improve native panels instead. Because
Unreal/Unity/Houdini/Maya are named commitments, build the companion UI. That roadmap tips it.

### The honest counter-argument: the refresh pain has a cheap fix

The Rust runtime **already has** the push primitives: `mode_updates: broadcast::Sender<Mode>`
(`motionstage-server/src/lib.rs:353`), `subscribe_mode_updates()` (`:940`), and
`last_published_snapshot()` (`:535`). The QUIC handler already consumes them to push without
polling (`:2654`). They are **walled off at the PyO3 boundary**: the FFI surface
(`motionstage_sdk_rust.pyi`) has zero callbacks, so the Python wrapper fakes events with a
background poll/diff thread (`server.py:478`), and the Blender addon runs its own 120Hz
main-thread timer on top.

Exposing the existing Rust push through the FFI as callbacks would kill the Python poll/diff
thread with zero frontend work. **That cheap fix is real and separable.** Note: the companion
UI does not need it — the WS listener lives in Rust and subscribes to the broadcast channels
directly, bypassing the Python poll entirely.

### What the companion UI removes, precisely

The 120Hz Blender timer does two jobs:
1. **Panel sync + `tag_redraw`** of VIEW_3D/PROPERTIES (`addon.py:1076-1107`). A web UI
   **removes this entirely** — operator controls render in the browser off WS push.
2. **Scene-apply tick**: drain `_pending_samples`, `_apply_sample_to_scene` (`addon.py:1050`,
   `:936`). This **survives** — `bpy` is main-thread-only and driving a virtual camera means
   the viewport must animate during Live. A `bpy` threading constraint, not a panel artifact.

So: the UI relocates the operator surface off the loop and (with the FFI fix) eliminates the
poll/diff fiction. The main-thread scene-write tick stays.

---

## 2. Recommended architecture

| Decision | Call | Why |
|---|---|---|
| Who serves the UI | **The embedded Rust runtime** | Only component identical across DCCs. Host once, every DCC inherits it. |
| One server or two | **One.** A second listener on the existing `ServerHandle` Tokio runtime | The Python SDK already owns a `ServerHandle` + Tokio `Runtime`. An axum WS task is `rt.spawn`'d against a clone (`ServerHandle` is `#[derive(Clone)]`, `Arc<RwLock>` inside). A new door on the house, not a new server. |
| Wire protocol | **Reuse command verbs, new schema for state push** | See below. |
| Transport | **WebSocket** | Full-duplex, sub-100ms on localhost, native in axum. SSE needs a second up-channel; WebRTC is NAT machinery you don't need on localhost; IPC breaks the browser MVP. |
| Shell | **Browser now, Tauri when floating matters** | Both are just WS clients. Browser = zero-install MVP + permanent fallback. Tauri (3-10MB, OS webview) floats borderless always-on-top. Electron is 80-200MB for nothing needed. |
| Static serving | **Embed the SPA in the Rust binary** (`rust_embed`) | One artifact, zero loose files per DCC. |

### Wire protocol split

- **Client to server: reuse the existing verbs.** `SetDataFlow`, `SetRecording`, the
  take/playback family, baseline commit already exist, derive serde, and are gated behind
  `ClientRole::Operator` (`protocol/src/lib.rs:25`, server gates at `:1903`+). The UI connects
  as `Operator` and calls these. It only ever touches ~10-15 of the ~41 variants; device
  handshake/auth variants never reach it.
- **Server to client: a new UI-shaped state schema.** The state the UI needs (scene snapshot,
  attribute values, sessions, metrics, video status) is **not** `ControlMessage`-shaped.
  `RuntimeSnapshot`'s inner types already derive `Serialize`; add the derive to the snapshot
  and push snapshot-on-connect, then deltas, driven off the broadcast channels. Subscribe-then-
  push-on-change (the OBS/Resolume shape). Throttle to ~30Hz / on-change; never mirror 120Hz.

One server. Command verbs reused. State push is a new, stable, UI-shaped projection. Both call
the same `ServerHandle` methods underneath.

---

## 3. The cross-DCC contract

Trace one motion update: the runtime computes the final value and `_apply_sample_to_scene`
(`addon.py:936`) does `obj.location[0] = float(value[0])`. **No math** — no mapping, no
filtering, no transform. The plugin enumerates objects and pokes numbers. That is the whole job.

```
interface DccAdapter:
    # Scene -> runtime (DCC-specific read)
    enumerate_scene() -> SceneSpec            # native objects + typed attributes
    ensure_stable_id(native_obj) -> ObjectId  # survives rename/re-enum
    resolve_object(id, name) -> native_obj?    # id-first, name fallback
    # Runtime -> scene (DCC-specific write)
    apply_attribute(id, name, attr, value, frame?) -> bool  # the ONLY write path
    # Coordinate boundary (DCC-specific convert)
    encode_value(kind, native) -> runtime_value   # e.g. Blender wxyz -> runtime xyzw
    decode_value(kind, runtime) -> native_value
    # Optional video (capability-gated)
    supports_video() -> bool
    video_descriptor() -> (w, h, fps, pixel_format)
    capture_frame() -> bytes?
    # Host integration (DCC-specific scheduling)
    schedule_tick(callback, interval) -> handle   # host's timer (bpy.app.timers, etc.)
    run_on_main_thread(fn)                         # host's safe-thread mechanism
```

**Shared (write once in the Rust runtime / SDK):** the entire React app and WS protocol, the
scene model, mapping engine, mode state machine, recording/takes/playback, baseline/commit,
session catalog, metrics, video signaling, and the whole iOS-facing side.

**Control-flow constraint that makes or breaks portability:** the apply side must be pull-based.
The runtime must never call into the DCC on its own thread. Every host has a different
main-thread model (Unreal game thread, Maya `executeDeferred`, Houdini main thread, Unity main
thread). The runtime exposes "drain pending updates"; the host decides when to apply. That is
why `schedule_tick`/`run_on_main_thread` are adapter methods.

**The UI cannot tell which DCC it talks to.** The adapter sits *below* the runtime (scene in,
values out); the UI sits *beside* it (state out, control in). They never touch. Coordinate
quirks are absorbed at encode/decode before state reaches the runtime, so the UI always sees
normalized values.

```
        Companion UI (React / Tauri)
               |  WebSocket: command verbs + UI state push, Operator role
               v
   +-------------------------------+
   |  EMBEDDED RUNTIME (identical  | <-- QUIC -- iOS app (MotionSource)
   |  in every DCC plugin)         |
   |  scene . mapping . mode .     |
   |  recording . video . sessions |
   +-------------------------------+
               ^   |
   enumerate_scene |  apply_attribute    <- DccAdapter (the ONLY per-DCC code)
               |   v
       Native DCC objects (Blender / Unreal / Maya / Houdini / Unity)
```

---

## 4. Phased plan

**Phase 0: Expose Rust push through the FFI.** Surface `subscribe_mode_updates`/snapshot deltas
as FFI callbacks; kills the Python poll/diff thread (`server.py:478`). Standalone value,
separable. *Not on the companion-UI critical path* (the WS listener subscribes in Rust).

**Phase 1: WS listener + SPA, Blender only.** Add the axum listener on the existing runtime.
Embed a React SPA with `rust_embed`. Define the UI WS schema (snapshot on connect, deltas
after; command verbs up). Run it in a browser tab. The Blender native panel becomes one
optional view among peers. Only new FFI surface: `start_companion_ui() -> port`.

**Phase 2: Tauri shell + video preview.** Wrap the same localhost URL in Tauri (always-on-top,
borderless). Add viewport preview by joining the existing WebRTC path as a `VideoSink`, reusing
the `SignalMessage`/SDP/ICE flow.

**Phase 3: Migrate agnostic logic down into the shared SDK.** Mapping resolution, source
catalog, queue/drain, run save/load, mode polling currently live in Blender's `service.py`.
Until that sinks into the shared Rust/SDK, the next DCC reimplements it. The `DccAdapter` is
what's left after this extraction.

**Phase 4: Prove generalization with one second DCC** (Unreal or Unity). Implement only the
`DccAdapter`; the UI/WS/host inherit unchanged. If the same React build drives DCC #2 with no
UI changes, the contract is proven.

---

## 5. Risks and costs

- **The backend is cheap; the frontend is a second product.** A React app is a new codebase,
  language, build chain, release cadence, and versioned UI protocol. Tauri adds macOS
  signing/notarization and per-OS webview testing. Budget it honestly.
- **Security: localhost binding is not enough.** Any website can `fetch`/WS to `127.0.0.1`
  (DNS-rebinding, CSRF-to-localhost). Require the existing `pairing_token`/`api_key`
  (`protocol:244-245`) on the WS upgrade **and** validate the `Origin` header.
- **Panic isolation across the FFI boundary.** A panic in the WS task must not unwind into
  Blender's Python. Supervise/catch the UI task; device motion + video must keep running. The
  UI is strictly additive and fails in isolation.
- **Port discovery with multiple instances.** Two Blender windows = two servers = two ports.
  Bind `:0`, surface the chosen port from `ServerHandle`, hand it to the browser/Tauri launch.
- **The seam can be mis-drawn.** If mapping/transform logic creeps into the per-DCC adapter, you
  reimplement the hard part five times. The adapter only enumerates, writes, converts
  coordinates, and schedules. Phase 4 is the test that catches a leak early.

**What would make this a mistake:** the five-DCC roadmap is aspirational and Blender is the real
target for a year-plus; or the team can't sustain a web frontend's release cadence; or you skip
Phase 3 and jump to a Blender-only React app, leaving agnostic logic stuck in `service.py`.

---

## Files grounding this

- `motionstage/crates/motionstage-server/src/lib.rs` — `ServerHandle` (`:351`, `Clone`), mode
  broadcast (`:353`, `:940`), snapshot publish (`:535`, `:613`), Operator gates (`:1903`+),
  QUIC push seam (`:2654`)
- `motionstage/crates/motionstage-protocol/src/lib.rs` — serde everywhere,
  `ClientRole::Operator` (`:25`), `AttributeKind` (`:39`), `pairing_token`/`api_key`
  (`:244-245`), control verbs (`:301`+), video signaling
- `motionstage/crates/motionstage-core/src/runtime.rs` — `RuntimeSnapshot` (`:39`; inner types
  already `Serialize`)
- `motionstage/crates/motionstage-sdk-python/src/lib.rs` — in-process `ServerHandle` + owned
  Tokio runtime (`:29-30`), spawn pattern (`enqueue_video_frame`)
- `motionstage/python/motionstage_sdk_rust.pyi` — FFI surface, zero callbacks (pull-only)
- `motionstage/python/motionstage_sdk/server.py` — poll/diff event thread (`:478`)
- `motionstage-blender/motionstage_blender/addon.py` — `_apply_sample_to_scene` (`:936`),
  drain (`:1050`), `tag_redraw` (`:1107`), coordinate hooks (`:178`)
- `motionstage-blender/motionstage_blender/service.py` — agnostic logic to migrate
  (mapping/catalog/queue), thread no-op comments (`:535-563`)
