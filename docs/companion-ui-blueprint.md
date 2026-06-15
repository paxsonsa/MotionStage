# (auto-saved from blueprint workflow wf_bfaa389a-be5)

Advisor confirms direction and sharpened three conflict resolutions. Writing the blueprint now.

# MotionStage Companion UI — Phase 1 Build Blueprint

Repo root: `/Users/apaxson/work/projects/cinemotion` (`motionstage/` symlink). Blender: `/Users/apaxson/work/projects/motionstage-blender`. All `file:line` anchors verified against the live tree.

---

## 0. Three resolved conflicts (read first — the probes disagreed)

**C1 — The choke point is ONE thing: the `ServerHandle` async method set. Not a message dispatcher.**
The VERBS probe floated two options (A: refactor `ControlChannel` to a trait and reuse `handle_*`; B: call `ServerHandle` methods directly). **Decision: Option B.** The QUIC dispatch is an inline `match` at `crates/motionstage-server/src/lib.rs:2537`, and every handler (`handle_set_data_flow` `:1897`, etc.) is hard-bound to `&mut ControlChannel` as its reply sink — unusable from a WebSocket. The WS handler parses inbound JSON and calls `ServerHandle` methods directly (`set_data_flow` `:894`, `set_recording` `:918`, baseline `:972-996`, `list_takes`/`select_take`/`delete_take`, `playback_*`, `create_mapping` `:1031`, `update_mapping` `:1061`, `remove_mapping` `:1092`, `set_active_scene` `:118`-style). It does NOT route through `:2537` and does NOT reuse `handle_*`. Option A's trait refactor is out of Phase-1 scope. This matches the FFI probe ("calls `ServerHandle` async methods in-process") and the architecture doc line 84 ("both call the same `ServerHandle` methods underneath").

**C2 — Doc correction: the upstream wire is TWO-shaped, not one.**
The user's decision ("reuse existing `ControlMessage` Operator verbs as JSON") holds for **Group A** (data-flow, recording, takes, playback, baseline — these are real `ControlMessage` variants at `protocol/src/lib.rs:301+`). But the doc was wrong that mapping CRUD + set-active-scene exist as verbs. They have **no `ControlMessage` variant**. So upstream JSON is:
- **Group A:** externally-tagged `ControlMessage` JSON, e.g. `{"SetDataFlow":"Live"}`.
- **Group B:** a supplementary UI-only envelope, e.g. `{"cmd":"create_mapping","req":{...}}`.
Both dispatch to `ServerHandle` methods in the WS handler. Keep the envelopes as separate UI-only types — do NOT add serde attributes to `ControlMessage` (that would change the bincode/QUIC device wire).

**C3 — Token plumbing exists from day one; `/ws` validation is the deferred hardening.**
The FFI probe mints a token and gates `/ws`; the Blender probe opened a bare URL. **Decision:** Rust mints the token in `serve_companion_ui` and stores it on the handle. The Python wrapper exposes `companion_ui_url()` returning `http://127.0.0.1:PORT/?token=...`, and the **Blender operator calls `companion_ui_url()`** (token-bearing), not a bare URL. The `/ws` token + Origin **validation** is the security step we defer for the localhost MVP (§9), but the token rides in the URL from the start so flipping validation on needs zero Blender/Python change.

---

## 1. WALKING SKELETON (slice 1) — prove end-to-end before any React/rust_embed

Goal: axum bound on `127.0.0.1:0`, serves a hand-written `index.html`, opens a `/ws` that pushes the current **mode** (one field) on connect and on every change. `start_companion_ui()` FFI returns the port. Blender button opens the browser.

Exploit that **mode is the only signal with a broadcast** (`subscribe_mode_updates()` `:940`) — the skeleton needs NO poll-and-diff loop and NO `ServerToUi` enum yet. Send a bare `{"mode":"live"}`. Inline the HTML as a `&str` in the fallback handler — defer `rust_embed` (its `#[folder]` reads at compile time and would error before `dist/` exists).

### Files for slice 1 ONLY

**Create** `crates/motionstage-server/src/companion_ui.rs`:
- `pub struct CompanionUiHandle { pub local_addr: SocketAddr, pub auth_token: Option<String>, shutdown_tx: watch::Sender<bool>, join: JoinHandle<()> }` with `pub fn port(&self) -> u16` and `pub async fn shutdown(self) -> Result<(), ServerError>`.
- `pub async fn serve_companion_ui(server: ServerHandle, auth_token: Option<String>) -> Result<CompanionUiHandle, ServerError>` — binds `TcpListener::bind(("127.0.0.1", 0)).await`, reads `local_addr()` synchronously (so bind errors surface as `Err` and the caller gets the real port), then `tokio::spawn`s `axum::serve(listener, app).with_graceful_shutdown(...)`. Mirror `QuicRuntime` (`:367-383`); reuse `ServerError::Runtime(String)` exactly like `:380`.
- `build_router(server, auth_token) -> axum::Router`: `Router::new().route("/ws", get(ws_handler)).fallback(index_handler).with_state(AppState { server, auth_token })`.
- `index_handler` returns an inlined `&'static str` HTML with a tiny inline `<script>` opening `new WebSocket("ws://"+location.host+"/ws")` and rendering `JSON.parse(e.data).mode`.
- `ws_handler(ws: WebSocketUpgrade, State(st)): ws.on_upgrade(|socket| handle_ws(socket, st.server))`. Inside `handle_ws`: send `mode()` (`:1021`) once on connect as `{"mode":"<label>"}`, then `let mut rx = server.subscribe_mode_updates();` and loop `rx.recv().await`, sending on each change. Wrap the per-socket future in `AssertUnwindSafe(fut).catch_unwind().await` for panic isolation (§8). **Note for full design:** snapshot-on-connect-then-stream is mandatory — broadcast fires only on change, so without the initial send the UI looks dead until the first event.

**Edit** `crates/motionstage-server/src/lib.rs` (near top, after existing `mod`s): add `#[cfg(feature = "companion-ui")] pub mod companion_ui;`

**Edit** `crates/motionstage-server/Cargo.toml`: add optional deps + feature (see §6).

**Edit** root `Cargo.toml` `[workspace.dependencies]` (after line 40): add `axum` + `rust-embed` (see §6). (rust-embed declared now but unused until React slice.)

**Edit** `crates/motionstage-sdk-python/Cargo.toml:18`: change `motionstage-server.workspace = true` → `motionstage-server = { workspace = true, features = ["companion-ui"] }`.

**Edit** `crates/motionstage-sdk-python/src/lib.rs`:
- Add field to `PyMotionStageServer` (`:28`): `companion_ui: std::sync::Mutex<Option<motionstage_server::companion_ui::CompanionUiHandle>>,`
- Init in `new()` (after `:85`): `companion_ui: std::sync::Mutex::new(None),`
- Add `#[pymethods]` `start_companion_ui(&self) -> PyResult<u16>` mirroring `start()` (`:94-100`): lock slot, return existing `handle.port()` if present (idempotent), else mint `Some(Uuid::new_v4().to_string())` (uuid already imported `:21`), `self.rt.block_on(serve_companion_ui(self.server.clone(), token))`, store handle, return port.

**Edit** Blender `motionstage-blender/motionstage_blender/service.py`: add `self.server.start_companion_ui()` thin wrapper (full version §7).

**Edit** Blender `motionstage_blender/addon.py`: add `import webbrowser`, the operator, the button, `_CLASSES` entry (full version §7).

### Prove slice 1
1. `cargo build -p motionstage-server --features companion-ui`
2. `cargo build -p motionstage-cli` (no feature → no web stack, confirms gating)
3. Build the python wheel (`maturin develop` / project script), start a server in Python, call `start_companion_ui()`, open `http://127.0.0.1:PORT/`, toggle Live/Idle, watch the mode field update over `/ws`.

Once green, build out the full schema, command path, React, and Blender polish.

---

## 2. Downstream WS schema (full) — paste-ready Rust

Lives in `companion_ui.rs`. Reshapes the two `IndexMap`s in `RuntimeSnapshot` into arrays — never serializes `RuntimeSnapshot` directly. **With the reshape, NO `motionstage-core`/`motionstage-protocol` edits are required** — every inner type already derives `Serialize` (verified: `model.rs:9/40/68/120/126`; `protocol lib.rs:73/79/86/172/20/28/67/292`).

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToUi {
    Snapshot(UiSnapshot),                               // once on connect
    ModeChanged { mode: UiMode },                       // from subscribe_mode_updates (event)
    SnapshotChanged(UiSceneState),                      // structural delta (low freq)
    SessionUpserted { session: UiSession },
    SessionRemoved { device_id: Uuid },
    AttributeValues { changes: Vec<UiAttributeValue> }, // value-only, high freq
    VideoStatusChanged { video: UiVideoStatus },
    Metrics { metrics: UiMetrics },
}

#[derive(Debug, Clone, Serialize)]
pub struct UiSnapshot {
    pub scene: UiSceneState, pub mode: UiMode,
    pub sessions: Vec<UiSession>, pub metrics: UiMetrics, pub video: UiVideoStatus,
}
#[derive(Debug, Clone, Serialize)]
pub struct UiSceneState {
    pub active_scene: Option<SceneId>,
    pub scenes: Vec<UiScene>, pub mappings: Vec<UiMapping>,
}
#[derive(Debug, Clone, Serialize)]
pub struct UiScene { pub id: SceneId, pub name: String, pub objects: Vec<UiObject> }
#[derive(Debug, Clone, Serialize)]
pub struct UiObject { pub id: ObjectId, pub name: String, pub attributes: Vec<UiAttribute> }
#[derive(Debug, Clone, Serialize)]
pub struct UiAttribute {
    pub name: String,
    pub value_type: String,              // AttributeValue::type_name() (model.rs:24)
    pub default_value: AttributeValue,   // model.rs:9, Serialize
    pub current_value: AttributeValue,
    pub live_enabled: bool, pub record_enabled: bool,
    pub filter_chain: Vec<AttributeFilter>, // model.rs:68, Serialize
}
#[derive(Debug, Clone, Serialize)]
pub struct UiMapping {
    pub id: MappingId, pub source_device: Uuid, pub source_output: String,
    pub target_scene: SceneId, pub target_object: ObjectId, pub target_attribute: String,
    pub component_mask: Option<Vec<usize>>, pub lock: bool,
    pub state: MappingState,             // model.rs:120, Serialize
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct UiMode { pub data_flow: DataFlowState, pub recording: RecordingState, pub label: UiModeLabel }
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiModeLabel { Idle, Live, Recording, Playback }
// derive: recording==Playback => Playback; recording==Recording => Recording;
//         data_flow==Live => Live; else Idle.
#[derive(Debug, Clone, Serialize)]
pub struct UiSession {
    pub device_id: Uuid, pub device_name: String, pub session_id: Option<Uuid>,
    pub roles: Vec<ClientRole>, pub features: Vec<Feature>,
    pub advertised_attributes: Vec<AttributeDescriptor>,
    pub state: SessionState,
}
pub type UiVideoStatus = VideoStreamStatus;   // protocol lib.rs:292, Serialize
#[derive(Debug, Clone, Copy, Serialize)]
pub struct UiMetrics {
    pub accepted_sessions: u64, pub rejected_sessions: u64, pub motion_datagrams: u64,
    pub motion_updates: u64, pub signaling_messages: u64, pub scheduler_ticks: u64, pub publish_ticks: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct UiAttributeValue {
    pub scene_id: SceneId, pub object_id: ObjectId, pub attribute: String, pub value: AttributeValue,
}
```

**JSON wire reminders:** `AttributeValue` is externally tagged → `{"Vec3f":[1,2,3]}`, `{"Float32":1.5}`. `AttributeFilter` → `"Passthrough"` or `{"Ema":{"alpha":0.5}}`. `MappingState`/`SessionState`/`ClientRole`/`Feature` → bare strings (`"Active"`, `"MotionSource"`). The React side must match these exact tags. Initial frame:
```json
{"type":"snapshot","scene":{"active_scene":"...","scenes":[...],"mappings":[...]},
 "mode":{"data_flow":"Live","recording":"Inactive","label":"live"},
 "sessions":[...],"metrics":{...},"video":{"available":false,"descriptor_set":true,"peer_count":0,"last_frame_age_ms":null}}
```
Incrementals: `{"type":"mode_changed","mode":{...}}`, `{"type":"attribute_values","changes":[{"scene_id":"..","object_id":"..","attribute":"position","value":{"Vec3f":[1.1,2.2,3.3]}}]}`, `{"type":"session_removed","device_id":".."}`, `{"type":"metrics","metrics":{...}}`.

### Push loop (full)
One `tokio::spawn` per WS client owning a `ServerHandle` clone. Mode is event-driven (broadcast); everything else is poll-and-diff at **30 Hz** (33 ms `interval`) against last-sent copies (server publish loop runs ≤60 Hz at `publish_hz` default 60 `:92`, so 30 Hz polling is comfortably under).

| Signal | Mechanism | Method (file:line) |
|---|---|---|
| Mode | broadcast (push, no throttle) | `subscribe_mode_updates()` `:940`; on `Lagged` re-read `mode()` `:1021` |
| Scene + live values | poll+diff | `last_published_snapshot()` `:535` |
| Sessions | poll+diff (no broadcast) | `sessions()` `:1794` |
| Video | poll+diff | `video_stream_status()` `:1750` |
| Metrics | poll (emit on change) | `metrics()` `:710` |
| Initial snapshot | one-shot on connect | `runtime_snapshot()` `:1026` + `sessions()` + `metrics()` + `video_stream_status()` |

Emit-on-change only. `AttributeValues` carries ONLY changed `current_value`s per `(object_id, attribute)`. `SnapshotChanged` reserved for structural deltas (object/attr/mapping set changes, `active_scene` change, attr metadata) — carries the full `UiSceneState`.

### Serialize derives to add
**Zero**, if you keep the reshape (recommended; the §2 struct set assumes it). If you ever embed server structs directly instead of hand-mapping, add `Serialize` to: `RuntimeSnapshot` (`core/src/runtime.rs:38`), `SessionInfo` (`server/src/lib.rs:105`), `ServerMetrics` (`server/src/lib.rs:356`). `indexmap` already has `features=["serde"]` (root `Cargo.toml:40`), so `RuntimeSnapshot` would compile if derived. Not needed in this spec.

---

## 3. Upstream command path (full)

Inbound WS text frames. Try-parse as `ControlMessage` (Group A); on failure, parse as the UI envelope (Group B). Dispatch both to `ServerHandle` methods. The WS client is always treated as Operator (enforced at upgrade, §9) — no per-message role check (there is no `ClientHello`).

### Group A — existing `ControlMessage` verbs → `ServerHandle` methods
`ControlMessage` (`protocol/src/lib.rs:301`) is externally tagged (no serde attrs). Subset the UI uses:

| UI intent | JSON | `ServerHandle` method (file:line) |
|---|---|---|
| Go Live/Idle | `{"SetDataFlow":"Live"}` | `set_data_flow` `:894` |
| Recording | `{"SetRecording":"Recording"}` | `set_recording` `:918` |
| List takes | `{"ListTakes":{"scene_id":null}}` | `list_takes` |
| Select take | `{"SelectTake":{"take_id":"..."}}` | `select_take` |
| Delete take | `{"DeleteTake":{"take_id":"..."}}` | `delete_take` |
| Playback | `{"PlaybackControl":{"take_id":"...","action":"Seek","seek_ns":1000,"looping":true}}` | `playback_play`/`pause`/`stop`/`seek` |
| Reset baseline | `{"ResetSceneToBaseline":{"scene_id":null}}` | `reset_scene_to_baseline` `:972` |
| Commit scene baseline | `{"CommitSceneBaseline":{"scene_id":"..."}}` | `commit_scene_baseline` `:984` |
| Commit object baseline | `{"CommitObjectBaseline":{"scene_id":null,"object_id":"..."}}` | `commit_object_baseline` `:996` |

(`Option<Uuid>` → `null` or string; unit enum payloads like `PlaybackAction::Seek` → bare string. Omit bake-cursor verbs — DCC path, not Phase 1.)

### Group B — no `ControlMessage`, supplementary UI envelope → direct method call
```json
{"cmd":"set_active_scene","scene_id":"..."}
{"cmd":"create_mapping","req":{"source_device":"...","source_output":"pose","target_scene":"...","target_object":"...","target_attribute":"location","component_mask":[0,1,2]}}
{"cmd":"update_mapping","mapping_id":"...","req":{...MappingRequest...}}
{"cmd":"remove_mapping","mapping_id":"..."}
```
`MappingRequest` (`core/src/model.rs:142-149`, derives Serialize/Deserialize): `{source_device, source_output, target_scene, target_object, target_attribute, component_mask}`. Methods: `set_active_scene` `:118`-style accessor on handle, `create_mapping(req, now_ns)` `:1031`, `update_mapping(id, req, now_ns)` `:1061`, `remove_mapping(id)` `:1092`. **`now_ns` is server-supplied** — call the existing `now_ns()` helper, never trust the client clock. There is no `set_active_source` runtime concept; source selection lives per-mapping.

Define a UI-only `#[derive(Deserialize)] enum UiCommand { SetActiveScene{scene_id}, CreateMapping{req}, UpdateMapping{mapping_id, req}, RemoveMapping{mapping_id} }` tagged on `cmd` (`#[serde(tag="cmd", rename_all="snake_case")]`). On each dispatched command, reply by re-pushing the affected downstream message (e.g. after `set_data_flow`, the mode broadcast already fires → `ModeChanged`; after mapping CRUD, emit `SnapshotChanged`).

---

## 4. New Rust module signature (full)

`crates/motionstage-server/src/companion_ui.rs` public surface — see §1 for `CompanionUiHandle` + `serve_companion_ui`. Add to the full version:
- `stop`/`shutdown`: `shutdown_tx.send(true)` then `join.await`. `watch::Receiver::changed()` also resolves on sender drop, so a dropped handle still exits the loop cleanly.
- `build_router` swaps the slice-1 inline-HTML `index_handler` for the rust_embed `static_handler` (§5) once React exists; `/ws` route unchanged.
- `AppState { server: ServerHandle, auth_token: Option<String> }` is the router `with_state`.

---

## 5. React app + rust_embed

### Component tree
```
<App>                          // owns WS client, top-level state store
├─ <ConnectionBar>             // ws status, reconnect; reads sessions
├─ <SessionsView>             // UiSession[]: device_name, roles, features, state
├─ <SceneView>                // UiSceneState: scene → objects → attributes
│   └─ <AttributeRow>         // value_type, current_value, live/record toggles, filter_chain
├─ <MappingsView>             // UiMapping[]: create/update/remove (Group B commands)
├─ <ModeControls>            // SetDataFlow / SetRecording / baseline commit-reset (Group A)
├─ <RecordingControls>       // takes list/select/delete, PlaybackControl (Group A)
└─ <MetricsPanel>            // UiMetrics counters; UiVideoStatus
```

### WS client (`src/ws.ts`)
- `connect()`: `new WebSocket("ws://"+location.host+"/ws"+location.search)` (carries `?token=` through). On `message`, `JSON.parse`, switch on `.type`, patch a store (Zustand or `useReducer`). `snapshot` seeds; `attribute_values` patches leaves by `(object_id, attribute)` without re-rendering the tree; `session_removed` deletes by `device_id`.
- `send(cmd)`: `JSON.stringify` either a `ControlMessage`-shaped object (Group A) or a `{cmd,...}` envelope (Group B).
- Auto-reconnect with backoff; re-request nothing (server re-sends `snapshot` on reconnect).

### Build + embed
- React app in `crates/motionstage-server/ui/` (vite). `npm run build` → `crates/motionstage-server/ui/dist/`.
- In `companion_ui.rs`: `#[derive(rust_embed::Embed)] #[folder = "ui/dist"] struct Assets;`. `static_handler(uri)`: strip leading `/`, default to `index.html`, `Assets::get(path)`, return bytes with `Content-Type` from `mime_guess` on the path; 404 → serve `index.html` (SPA fallback).
- **Build-order gotcha:** `#[folder]` reads at compile time. If `ui/dist/` is absent the macro errors. **Commit a placeholder `ui/dist/index.html`** so `cargo build` works before/without the React build, and document npm-build-before-cargo-build in CI.

---

## 6. Cargo deps

Root `Cargo.toml` `[workspace.dependencies]` (after line 40):
```toml
axum = { version = "0.8", features = ["ws"] }
rust-embed = "8"
```
`crates/motionstage-server/Cargo.toml`:
```toml
[dependencies]
axum = { workspace = true, optional = true }
rust-embed = { workspace = true, optional = true }
# (mime_guess = { version = "2", optional = true } if you want content-type detection)

[features]
companion-ui = ["dep:axum", "dep:rust-embed"]
```
`crates/motionstage-sdk-python/Cargo.toml:18`: `motionstage-server = { workspace = true, features = ["companion-ui"] }`.

Notes: tokio is **unchanged** — root requirement `1.43` (`Cargo.toml:32`), lockfile resolves `1.49.0`, satisfies axum 0.8's `^1.44.2`. axum's `ws` feature pulls tungstenite internally — **no separate `tokio-tungstenite` dep**. No `panic="abort"` in root `Cargo.toml` (verified) → tokio task-boundary catches panics; document that adding `panic="abort"` later voids host-survival.

---

## 7. PyO3 + .pyi + Python wrapper + Blender (full)

### PyO3 (`crates/motionstage-sdk-python/src/lib.rs`)
Slice-1 `start_companion_ui` plus:
```rust
pub fn companion_ui_token(&self) -> PyResult<Option<String>> { /* lock, return handle.auth_token.clone() */ }
pub fn stop_companion_ui(&self) -> PyResult<()> { /* slot.take(), rt.block_on(handle.shutdown()) */ }
```

### `.pyi` (`python/motionstage_sdk_rust.pyi`, after `video_peer_count` ~line 111)
```python
def start_companion_ui(self) -> int: ...
def companion_ui_token(self) -> str | None: ...
def stop_companion_ui(self) -> None: ...
```

### Python wrapper (`python/motionstage_sdk/server.py`, in `MotionStageServer` near `video_peer_count` `:429`)
```python
def start_companion_ui(self) -> int:
    return int(self._native.start_companion_ui())
def companion_ui_token(self) -> str | None:
    t = self._native.companion_ui_token(); return str(t) if t is not None else None
def companion_ui_url(self) -> str:
    port = self.start_companion_ui(); token = self.companion_ui_token()
    base = f"http://127.0.0.1:{port}/"
    return f"{base}?token={token}" if token else base
def stop_companion_ui(self) -> None:
    self._native.stop_companion_ui()
```

### Blender service (`motionstage-blender/motionstage_blender/service.py`)
`self.server` is the SDK object (`:107`, assigned `:153`). Add near video helpers; cache the port for idempotence:
```python
def start_companion_ui(self) -> int:
    self._require_server()
    if self._companion_ui_port is not None:
        return self._companion_ui_port
    port = int(self.server.start_companion_ui())
    self._companion_ui_port = port
    LOGGER.info("Companion UI started on port %d", port)
    return port
def companion_ui_url(self) -> str:
    self._require_server()
    return str(self.server.companion_ui_url())
```
Add `self._companion_ui_port: int | None = None` in `__init__` (alongside video fields ~`:127`), reset to `None` in `stop()` (`:193`).

### Blender operator + button (`motionstage_blender/addon.py`)
- Add `import webbrowser` (stdlib block ends ~line 29 — not currently imported).
- Insert operator after `MOTIONSTAGE_OT_toggle_video_streaming` (class at `:1934`), mirroring `MOTIONSTAGE_OT_resync_scene` (`:1336`): `bl_idname="motionstage.open_companion_ui"`, guard `if SERVICE.server is None: report ERROR, return CANCELLED`, then `url = SERVICE.companion_ui_url()` (**token-bearing per C3**), `webbrowser.open(url)` in try/except, `report INFO`. `webbrowser.open` is fast and we're on the main thread — no `bpy.app.timers` deferral needed.
- Button in `_draw_status` after the `Endpoint:` label (`:2184`), gated `ui_row.enabled = snapshot.is_connected`: `ui_row.operator("motionstage.open_companion_ui", icon="WORLD")`. `_draw_status` already receives `snapshot` (`is_connected` in scope); this propagates to both N-panel and Properties surfaces via `_make_subpanel`.
- Append `MOTIONSTAGE_OT_open_companion_ui,` to `_CLASSES` (`:2549`) after `MOTIONSTAGE_OT_toggle_video_streaming,` (`:2576`). Operator order is irrelevant.

**Lifecycle:** lazy on first click, idempotent on both sides (Python cache + Rust returns existing port). Do NOT auto-start in `SERVICE.start()` (performance-first: no listener for users who never open the UI). FFI contract: `start_companion_ui()` is safe to call repeatedly, returns the bound port, never binds twice.

---

## 8. Panic isolation + lifecycle

- Serve loop runs in `tokio::spawn` (not in the `block_on` caller). With `panic="unwind"` (no `panic="abort"` in root `Cargo.toml`, verified), a task panic unwinds to the tokio task boundary as a `JoinError` and cannot cross the PyO3 FFI into Python.
- Per-connection isolation: wrap each WS handler future in `std::panic::AssertUnwindSafe(fut).catch_unwind().await` and log on `Err` (avoids a `tower-http` dependency vs `CatchPanicLayer`).
- Shutdown tiers: (floor) dropping `PyMotionStageServer` drops `rt: Runtime` (`:30`) → all tasks torn down; (graceful) `stop_companion_ui()` → `watch::send(true)` → `axum::serve().with_graceful_shutdown()` drains → `join.await`. `watch::changed()` also resolves on sender drop, so a dropped handle exits cleanly. Storing the handle in the pyclass field keeps it alive until then. UI holds only a `ServerHandle` clone (Arc bump); UI shutdown and `server.stop()` are independent.

---

## 9. Security (token + Origin on `/ws`)

For the localhost MVP we **defer validation but plumb the token from day one** (resolves C3). Any local web page can reach `ws://127.0.0.1:PORT` (architecture doc line 179), so the hardening, when flipped on, is in `ws_handler` before `on_upgrade`:
1. **Token:** require `?token=` (query) or header equal to `CompanionUiHandle.auth_token`; reuse the same `config.pairing_token`/`config.api_key`/`security_mode` logic as `ServerState::ensure_auth` (`server/src/lib.rs:200-234`) read off the cloned handle. Reject mismatch with `403`.
2. **Origin:** validate the `Origin` header against an allowlist (`http://127.0.0.1:PORT`, `http://localhost:PORT`); reject foreign origins.
The token already rides in `companion_ui_url()` and the Blender-opened URL, so enabling validation needs zero Python/Blender change. The WS client is always Operator (no `ClientHello`); enforce Operator-ness at the upgrade, then allow all Group-A/Group-B commands.

---

## 10. Ordered task list

1. **Skeleton (§1):** workspace + server `Cargo.toml` deps/feature; `companion_ui.rs` with `CompanionUiHandle` + `serve_companion_ui` + inline-HTML `index_handler` + mode-only `/ws` (snapshot-on-connect + broadcast loop, `catch_unwind` wrap); `pub mod` in `lib.rs`; PyO3 `start_companion_ui`; minimal Blender wrapper + operator + button + `_CLASSES`. Prove: `cargo build -p motionstage-server --features companion-ui`, `cargo build -p motionstage-cli`, wheel build, browser shows live mode.
2. **Derives + downstream schema (§2):** add the `ServerToUi`/`Ui*` types (zero core derives with reshape); build `UiSnapshot` from `runtime_snapshot()`+`sessions()`+`metrics()`+`video_stream_status()`; add the 30 Hz poll-and-diff loop alongside the mode broadcast.
3. **Upstream command path (§3):** `ControlMessage` (Group A) + `UiCommand` envelope (Group B) parse + dispatch to `ServerHandle` methods; server-supplied `now_ns()` for mapping CRUD; re-push affected downstream messages as ACK.
4. **FFI completion (§7):** `companion_ui_token` + `stop_companion_ui` + `.pyi` + Python `companion_ui_url`.
5. **React (§5):** vite app in `ui/`, component tree, `ws.ts` client, store; commit placeholder `ui/dist/index.html`; swap inline HTML → rust_embed `static_handler`.
6. **Blender polish (§7):** switch operator to `companion_ui_url()` (token-bearing), port cache + `stop()` reset, lifecycle/threading confirmed.
7. **Build/test:** `cd crates/motionstage-server/ui && npm ci && npm run build` → `cargo build -p motionstage-server --features companion-ui` → `cargo test --workspace` → `maturin develop` (enables feature transitively) → `python -m pytest -q python/tests` → Blender manual smoke (Start Runtime → Open Companion UI → toggle Live/Recording, create/remove mapping, play a take). Every server build/test command carries `--features companion-ui`; `cli serve` intentionally has no UI.

---

### Conflicts called out (recap)
- **C1:** single choke point = `ServerHandle` methods; Option B only; no QUIC-dispatch reuse, no `ControlChannel` trait refactor in Phase 1.
- **C2:** doc was wrong — upstream is two-shaped (`ControlMessage` Group A + `{"cmd":...}` Group B); mapping CRUD + set-active-scene have no wire verb.
- **C3:** token plumbed from day one; Blender opens `companion_ui_url()` (token-bearing); `/ws` token+Origin **validation** is the deferred localhost-MVP hardening.

Key anchors: `crates/motionstage-server/src/lib.rs` (`ServerHandle` `:351`, `QuicRuntime` `:367-383`, `set_data_flow` `:894`, `subscribe_mode_updates` `:940`, `runtime_snapshot` `:1026`, `create_mapping` `:1031`, `last_published_snapshot` `:535`, `metrics` `:710`, `video_stream_status` `:1750`, `sessions` `:1794`, dispatch `:2537`); `crates/motionstage-protocol/src/lib.rs` (`ControlMessage` `:301`); `crates/motionstage-sdk-python/src/lib.rs` (`PyMotionStageServer` `:28`, `rt` `:30`, `start` `:94`); `crates/motionstage-sdk-python/Cargo.toml:18`; root `Cargo.toml:32,40`; `python/motionstage_sdk/server.py:429`; `python/motionstage_sdk_rust.pyi:~111`; `motionstage-blender/motionstage_blender/addon.py` (`:1934,:2184,:2549,:2576`); `motionstage-blender/motionstage_blender/service.py` (`:107,:153,:193`).
