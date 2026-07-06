//! Companion UI: an embedded HTTP + WebSocket listener that serves a DCC-agnostic
//! operator surface from the shared runtime.
//!
//! The companion UI is the cross-DCC plug-and-play surface described in
//! `docs/companion-ui-architecture.md`. One axum listener is spawned on the host's
//! existing Tokio runtime, holding a clone of [`ServerHandle`]. It serves a static
//! single-page app and a `/ws` endpoint that:
//!   * pushes runtime state to the browser (snapshot-on-connect, then deltas), and
//!   * accepts operator commands and dispatches them to `ServerHandle` methods.
//!
//! ## Wire protocol
//!
//! * **Downstream (server -> UI):** a UI-shaped [`ServerToUi`] enum (serde-tagged on
//!   `type`). One full [`UiSnapshot`] on connect, then emit-on-change deltas driven by
//!   the mode broadcast (event) plus a ~30 Hz poll/diff of the published snapshot,
//!   sessions, metrics, and video status. The runtime's own structs are reshaped into
//!   UI-stable types so the device wire format and the UI wire format evolve
//!   independently.
//! * **Upstream (UI -> server):** two shapes, both dispatched to `ServerHandle`
//!   methods. **Group A** reuses existing [`ControlMessage`] operator verbs as JSON
//!   (`{"SetDataFlow":"Live"}`). **Group B** is a small UI-only envelope for actions
//!   that have no `ControlMessage` variant (mapping CRUD, set-active-scene):
//!   `{"cmd":"create_mapping","req":{...}}`.
//!
//! It binds `127.0.0.1:0` (ephemeral port) so multiple DCC instances never collide,
//! and it is strictly additive: a panic in a UI connection is caught at the task
//! boundary and never unwinds into the host (Blender/Unreal/...).

use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{
        ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures::{FutureExt, SinkExt, StreamExt};
use motionstage_core::{
    AttributeFilter, AttributeValue, MappingId, MappingRequest, MappingState, ObjectId,
    RuntimeSnapshot, SceneId,
};
use motionstage_protocol::{
    AttributeDescriptor, ClientRole, ControlMessage, DataFlowState, Feature, Mode, PlaybackAction,
    PlaybackRuntimeState, RecordingState, SessionState, TakeInfo, VideoStreamStatus,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};
use uuid::Uuid;

use crate::{now_ns, HostRequest, PlaybackStatus, ServerHandle, SessionInfo};

/// How often the connection polls non-event state (snapshot/sessions/metrics/video)
/// and emits deltas. The runtime publishes at <= 60 Hz, so 30 Hz is comfortably under
/// while staying responsive. Mode is event-driven (broadcast) and not throttled.
const PUSH_INTERVAL: Duration = Duration::from_millis(33);

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Handle to a running companion-UI listener. Dropping it (or calling
/// [`CompanionUiHandle::shutdown`]) tears the listener down without touching the
/// rest of the runtime — the UI only holds a `ServerHandle` clone (an `Arc` bump).
pub struct CompanionUiHandle {
    pub local_addr: SocketAddr,
    /// Token required on the `/ws` upgrade once validation is enabled. It already
    /// rides in the launch URL so flipping enforcement on needs no client change.
    pub auth_token: Option<String>,
    shutdown_tx: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl CompanionUiHandle {
    /// The ephemeral port the listener bound to.
    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    /// Gracefully stop the listener and await the serve task.
    pub async fn shutdown(self) -> Result<(), crate::ServerError> {
        let _ = self.shutdown_tx.send(true);
        self.join
            .await
            .map_err(|err| crate::ServerError::Runtime(err.to_string()))?;
        Ok(())
    }
}

/// Bind `127.0.0.1:0`, spawn the serving task on the current Tokio runtime, and
/// return a handle exposing the chosen port. Binding happens synchronously so bind
/// errors surface to the caller (the FFI), not into a detached task.
pub async fn serve_companion_ui(
    server: ServerHandle,
    auth_token: Option<String>,
) -> Result<CompanionUiHandle, crate::ServerError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|err| crate::ServerError::Runtime(format!("companion UI bind failed: {err}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| crate::ServerError::Runtime(err.to_string()))?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app = build_router(server, auth_token.clone(), shutdown_rx.clone());

    let join = tokio::spawn(async move {
        let mut shutdown_rx = shutdown_rx;
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            // Resolves on send(true) or on the sender being dropped.
            let _ = shutdown_rx.changed().await;
        });
        if let Err(err) = serve.await {
            tracing::error!(error = %err, "companion UI server exited with error");
        }
    });

    tracing::info!(addr = %local_addr, "companion UI listening");
    Ok(CompanionUiHandle {
        local_addr,
        auth_token,
        shutdown_tx,
        join,
    })
}

#[derive(Clone)]
struct AppState {
    server: ServerHandle,
    #[allow(dead_code)] // wired into /ws validation in the security-hardening step
    auth_token: Option<String>,
    /// Flips to `true` on listener shutdown so open WS connections close instead of
    /// holding `with_graceful_shutdown` (and the FFI `block_on`) open indefinitely.
    shutdown: watch::Receiver<bool>,
}

fn build_router(
    server: ServerHandle,
    auth_token: Option<String>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .fallback(static_handler)
        .with_state(AppState {
            server,
            auth_token,
            shutdown,
        })
}

/// The compiled React SPA, embedded into the binary so each DCC plugin ships one
/// artifact with no loose UI files. Built by `ui/` (`npm run build` -> `ui/dist`).
#[derive(rust_embed::Embed)]
#[folder = "ui/dist"]
struct Assets;

/// Serve an embedded asset, falling back to `index.html` for client-side routes
/// (SPA fallback). Any non-asset path returns the app shell.
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // Dev override: serve from a filesystem dir so UI changes show on a browser
    // refresh without rebuilding/re-embedding the wheel. Set MOTIONSTAGE_UI_DIR to
    // the built `ui/dist` path. Falls through to embedded assets if absent/missing.
    if let Ok(dir) = std::env::var("MOTIONSTAGE_UI_DIR") {
        if let Some(resp) = serve_from_dir(&dir, path) {
            return resp;
        }
    }

    if let Some(content) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return serve_bytes(mime.as_ref(), content.data);
    }
    match Assets::get("index.html") {
        Some(content) => serve_bytes("text/html", content.data),
        None => (StatusCode::NOT_FOUND, "UI assets not built").into_response(),
    }
}

/// Read an asset from a filesystem dir (dev override). Path traversal is blocked
/// by rejecting any `..` segment. SPA fallback to index.html.
fn serve_from_dir(dir: &str, path: &str) -> Option<Response> {
    if path.split('/').any(|seg| seg == "..") {
        return None;
    }
    let base = std::path::Path::new(dir);
    let candidate = base.join(path);
    let bytes = std::fs::read(&candidate)
        .ok()
        .or_else(|| std::fs::read(base.join("index.html")).ok())?;
    let mime = if std::fs::metadata(&candidate).is_ok() {
        mime_guess::from_path(path).first_or_octet_stream().to_string()
    } else {
        "text/html".to_string()
    };
    Some(serve_bytes(&mime, std::borrow::Cow::Owned(bytes)))
}

fn serve_bytes(content_type: &str, data: std::borrow::Cow<'static, [u8]>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(data.into_owned()))
        .expect("valid static response")
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        // Panic isolation: a panic inside a connection handler is caught here and
        // never crosses the FFI boundary into the host process.
        if AssertUnwindSafe(handle_ws(socket, state.server, state.shutdown))
            .catch_unwind()
            .await
            .is_err()
        {
            tracing::error!("companion UI connection handler panicked (isolated)");
        }
    })
}

// ---------------------------------------------------------------------------
// Downstream schema (server -> UI)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerToUi {
    /// Full state, sent once on connect.
    Snapshot(UiSnapshot),
    /// Composite mode changed (event-driven via the broadcast channel).
    ModeChanged { mode: UiMode },
    /// Structural scene/mapping change (objects, attributes, mappings, active scene).
    SnapshotChanged(UiSceneState),
    /// Value-only update for already-known attributes (the high-frequency path).
    AttributeValues { changes: Vec<UiAttributeValue> },
    SessionUpserted { session: UiSession },
    SessionRemoved { device_id: Uuid },
    VideoStatusChanged { video: VideoStreamStatus },
    Metrics { metrics: UiMetrics },
    /// Recorded takes and/or playback transport changed.
    TakesChanged {
        takes: Vec<UiTake>,
        playback: Option<UiPlayback>,
    },
    /// Host-DCC object selection changed (object names), for UI highlight.
    SelectionChanged { selected: Vec<String> },
    /// A rejected/failed command, echoed back for UI feedback.
    CommandError { message: String },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiSnapshot {
    scene: UiSceneState,
    mode: UiMode,
    sessions: Vec<UiSession>,
    metrics: UiMetrics,
    video: VideoStreamStatus,
    takes: Vec<UiTake>,
    playback: Option<UiPlayback>,
    selected: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiTake {
    take_id: Uuid,
    scene_id: Uuid,
    name: String,
    frame_count: u64,
    created_ns: u64,
    selected: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
struct UiPlayback {
    take_id: Uuid,
    #[serde(serialize_with = "ser_playback_state")]
    state: PlaybackRuntimeState,
    position_ns: u64,
    duration_ns: u64,
    looping: bool,
}

fn ser_playback_state<S: serde::Serializer>(
    s: &PlaybackRuntimeState,
    ser: S,
) -> Result<S::Ok, S::Error> {
    ser.serialize_str(match s {
        PlaybackRuntimeState::Stopped => "stopped",
        PlaybackRuntimeState::Playing => "playing",
        PlaybackRuntimeState::Paused => "paused",
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiSceneState {
    active_scene: Option<SceneId>,
    scenes: Vec<UiScene>,
    mappings: Vec<UiMapping>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiScene {
    id: SceneId,
    name: String,
    objects: Vec<UiObject>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiObject {
    id: ObjectId,
    name: String,
    attributes: Vec<UiAttribute>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiAttribute {
    name: String,
    value_type: String,
    default_value: AttributeValue,
    current_value: AttributeValue,
    live_enabled: bool,
    record_enabled: bool,
    filter_chain: Vec<AttributeFilter>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiMapping {
    id: MappingId,
    source_device: Uuid,
    source_output: String,
    target_scene: SceneId,
    target_object: ObjectId,
    target_attribute: String,
    component_mask: Option<Vec<usize>>,
    lock: bool,
    state: MappingState,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
struct UiMode {
    data_flow: DataFlowState,
    recording: RecordingState,
    label: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiSession {
    device_id: Uuid,
    device_name: String,
    session_id: Option<Uuid>,
    roles: Vec<ClientRole>,
    features: Vec<Feature>,
    advertised_attributes: Vec<AttributeDescriptor>,
    state: SessionState,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
struct UiMetrics {
    accepted_sessions: u64,
    rejected_sessions: u64,
    motion_datagrams: u64,
    motion_updates: u64,
    signaling_messages: u64,
    scheduler_ticks: u64,
    publish_ticks: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct UiAttributeValue {
    scene_id: SceneId,
    object_id: ObjectId,
    attribute: String,
    value: AttributeValue,
}

fn mode_label(mode: Mode) -> &'static str {
    match mode.recording {
        RecordingState::Recording => "recording",
        RecordingState::Playback => "playback",
        RecordingState::Inactive => match mode.data_flow {
            DataFlowState::Live => "live",
            DataFlowState::Idle => "idle",
        },
    }
}

impl From<Mode> for UiMode {
    fn from(mode: Mode) -> Self {
        UiMode {
            data_flow: mode.data_flow,
            recording: mode.recording,
            label: mode_label(mode),
        }
    }
}

impl From<&crate::ServerMetrics> for UiMetrics {
    fn from(m: &crate::ServerMetrics) -> Self {
        UiMetrics {
            accepted_sessions: m.accepted_sessions,
            rejected_sessions: m.rejected_sessions,
            motion_datagrams: m.motion_datagrams,
            motion_updates: m.motion_updates,
            signaling_messages: m.signaling_messages,
            scheduler_ticks: m.scheduler_ticks,
            publish_ticks: m.publish_ticks,
        }
    }
}

impl From<&SessionInfo> for UiSession {
    fn from(s: &SessionInfo) -> Self {
        UiSession {
            device_id: s.device_id,
            device_name: s.device_name.clone(),
            session_id: s.session_id,
            roles: s.roles.clone(),
            features: s.features.clone(),
            advertised_attributes: s.advertised_attributes.clone(),
            state: s.state,
        }
    }
}

impl From<&TakeInfo> for UiTake {
    fn from(t: &TakeInfo) -> Self {
        UiTake {
            take_id: t.take_id,
            scene_id: t.scene_id,
            name: t.name.clone(),
            frame_count: t.frame_count,
            created_ns: t.created_ns,
            selected: t.selected,
        }
    }
}

impl From<PlaybackStatus> for UiPlayback {
    fn from(p: PlaybackStatus) -> Self {
        UiPlayback {
            take_id: p.take_id,
            state: p.state,
            position_ns: p.position_ns,
            duration_ns: p.duration_ns,
            looping: p.looping,
        }
    }
}

/// Takes for the active scene + current playback transport.
async fn build_takes(server: &ServerHandle, active_scene: Option<SceneId>) -> (Vec<UiTake>, Option<UiPlayback>) {
    let takes = server
        .list_takes(active_scene)
        .await
        .iter()
        .filter(|t| !t.deleted)
        .map(UiTake::from)
        .collect();
    let playback = server.playback_status().await.map(UiPlayback::from);
    (takes, playback)
}

/// Reshape a [`RuntimeSnapshot`]'s id-keyed maps into UI-stable arrays.
fn build_scene_state(snapshot: &RuntimeSnapshot) -> UiSceneState {
    let scenes = snapshot
        .scenes
        .values()
        .map(|scene| UiScene {
            id: scene.id,
            name: scene.name.clone(),
            objects: scene
                .objects
                .values()
                .map(|obj| UiObject {
                    id: obj.id,
                    name: obj.name.clone(),
                    attributes: obj
                        .attributes
                        .values()
                        .map(|attr| UiAttribute {
                            name: attr.name.clone(),
                            value_type: attr.current_value.type_name().to_string(),
                            default_value: attr.default_value.clone(),
                            current_value: attr.current_value.clone(),
                            live_enabled: attr.live_enabled,
                            record_enabled: attr.record_enabled,
                            filter_chain: attr.filter_chain.clone(),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();

    let mappings = snapshot
        .mappings
        .values()
        .map(|m| UiMapping {
            id: m.id,
            source_device: m.source_device,
            source_output: m.source_output.clone(),
            target_scene: m.target_scene,
            target_object: m.target_object,
            target_attribute: m.target_attribute.clone(),
            component_mask: m.component_mask.clone(),
            lock: m.lock,
            state: m.state.clone(),
        })
        .collect();

    UiSceneState {
        active_scene: snapshot.active_scene,
        scenes,
        mappings,
    }
}

/// Two scene states share the same *structure* (object/attribute/mapping shape and
/// active scene) when only attribute values may differ. Used to choose between a
/// cheap `AttributeValues` delta and a full `SnapshotChanged`.
fn same_structure(a: &UiSceneState, b: &UiSceneState) -> bool {
    if a.active_scene != b.active_scene || a.mappings != b.mappings {
        return false;
    }
    if a.scenes.len() != b.scenes.len() {
        return false;
    }
    a.scenes.iter().zip(&b.scenes).all(|(x, y)| {
        x.id == y.id
            && x.name == y.name
            && x.objects.len() == y.objects.len()
            && x.objects.iter().zip(&y.objects).all(|(ox, oy)| {
                ox.id == oy.id
                    && ox.name == oy.name
                    && ox.attributes.len() == oy.attributes.len()
                    && ox.attributes.iter().zip(&oy.attributes).all(|(ax, ay)| {
                        ax.name == ay.name
                            && ax.value_type == ay.value_type
                            && ax.live_enabled == ay.live_enabled
                            && ax.record_enabled == ay.record_enabled
                            && ax.filter_chain == ay.filter_chain
                    })
            })
    })
}

/// Attribute leaves whose `current_value` changed between two structurally-equal
/// scene states.
fn value_diffs(prev: &UiSceneState, cur: &UiSceneState) -> Vec<UiAttributeValue> {
    let mut changes = Vec::new();
    for (ps, cs) in prev.scenes.iter().zip(&cur.scenes) {
        for (po, co) in ps.objects.iter().zip(&cs.objects) {
            for (pa, ca) in po.attributes.iter().zip(&co.attributes) {
                if pa.current_value != ca.current_value {
                    changes.push(UiAttributeValue {
                        scene_id: cs.id,
                        object_id: co.id,
                        attribute: ca.name.clone(),
                        value: ca.current_value.clone(),
                    });
                }
            }
        }
    }
    changes
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_ws(socket: WebSocket, server: ServerHandle, mut shutdown: watch::Receiver<bool>) {
    let (mut sender, mut receiver) = socket.split();
    let mut mode_rx = server.subscribe_mode_updates();

    // --- snapshot on connect ---
    let mut last_scene = build_scene_state(&server.runtime_snapshot().await);
    let mut last_mode: UiMode = server.mode().await.into();
    let mut last_sessions: Vec<UiSession> =
        server.sessions().await.iter().filter(|s| s.state != SessionState::Closed).map(UiSession::from).collect();
    let mut last_metrics: UiMetrics = (&server.metrics().await).into();
    let mut last_video = server.video_stream_status().await;
    let (mut last_takes, mut last_playback) = build_takes(&server, last_scene.active_scene).await;
    let mut last_selection = server.host_selection().await;

    let initial = ServerToUi::Snapshot(UiSnapshot {
        scene: last_scene.clone(),
        mode: last_mode,
        sessions: last_sessions.clone(),
        metrics: last_metrics,
        video: last_video,
        takes: last_takes.clone(),
        playback: last_playback,
        selected: last_selection.clone(),
    });
    if send_json(&mut sender, &initial).await.is_err() {
        return;
    }

    let mut ticker = tokio::time::interval(PUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Listener shutdown: close the connection so graceful-shutdown can drain
            // (otherwise an open browser tab would block shutdown()/the FFI block_on).
            _ = shutdown.changed() => break,

            // Mode is event-driven; dedup redundant broadcasts (set_mode re-emits).
            changed = mode_rx.recv() => {
                let mode = match changed {
                    Ok(mode) => mode,
                    Err(broadcast::error::RecvError::Lagged(_)) => server.mode().await,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let ui_mode: UiMode = mode.into();
                if ui_mode != last_mode {
                    last_mode = ui_mode;
                    if send_json(&mut sender, &ServerToUi::ModeChanged { mode: ui_mode })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }

            // Poll/diff everything else at ~30 Hz, emit-on-change.
            _ = ticker.tick() => {
                if push_deltas(
                    &server,
                    &mut sender,
                    &mut last_scene,
                    &mut last_sessions,
                    &mut last_metrics,
                    &mut last_video,
                    &mut last_takes,
                    &mut last_playback,
                    &mut last_selection,
                )
                .await
                .is_err()
                {
                    break;
                }
            }

            // Inbound operator commands.
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Err(err) = dispatch_command(&server, &text).await {
                        let _ = send_json(
                            &mut sender,
                            &ServerToUi::CommandError { message: err },
                        )
                        .await;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                Some(Ok(_)) => {} // ignore binary/ping/pong
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn push_deltas(
    server: &ServerHandle,
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    last_scene: &mut UiSceneState,
    last_sessions: &mut Vec<UiSession>,
    last_metrics: &mut UiMetrics,
    last_video: &mut VideoStreamStatus,
    last_takes: &mut Vec<UiTake>,
    last_playback: &mut Option<UiPlayback>,
    last_selection: &mut Vec<String>,
) -> Result<(), axum::Error> {
    // Scene: poll the *published* snapshot (cheap, already computed), not a fresh one.
    if let Some(snapshot) = server.last_published_snapshot().await {
        let cur = build_scene_state(&snapshot);
        if &cur != last_scene {
            if same_structure(last_scene, &cur) {
                let changes = value_diffs(last_scene, &cur);
                if !changes.is_empty() {
                    send_json(sender, &ServerToUi::AttributeValues { changes }).await?;
                }
            } else {
                send_json(sender, &ServerToUi::SnapshotChanged(cur.clone())).await?;
            }
            *last_scene = cur;
        }
    }

    // Sessions: upsert changed/new, remove gone.
    let cur_sessions: Vec<UiSession> =
        server.sessions().await.iter().filter(|s| s.state != SessionState::Closed).map(UiSession::from).collect();
    if &cur_sessions != last_sessions {
        for s in &cur_sessions {
            if !last_sessions.iter().any(|p| p == s) {
                send_json(sender, &ServerToUi::SessionUpserted { session: s.clone() }).await?;
            }
        }
        for p in last_sessions.iter() {
            if !cur_sessions.iter().any(|s| s.device_id == p.device_id) {
                send_json(
                    sender,
                    &ServerToUi::SessionRemoved {
                        device_id: p.device_id,
                    },
                )
                .await?;
            }
        }
        *last_sessions = cur_sessions;
    }

    // Metrics + video status: emit on change.
    let cur_metrics: UiMetrics = (&server.metrics().await).into();
    if cur_metrics != *last_metrics {
        *last_metrics = cur_metrics;
        send_json(sender, &ServerToUi::Metrics { metrics: cur_metrics }).await?;
    }

    let cur_video = server.video_stream_status().await;
    if cur_video != *last_video {
        *last_video = cur_video;
        send_json(sender, &ServerToUi::VideoStatusChanged { video: cur_video }).await?;
    }

    // Takes + playback: emit on change (playback advances while playing, so this is
    // also what drives the seek bar).
    let (cur_takes, cur_playback) = build_takes(server, last_scene.active_scene).await;
    if &cur_takes != last_takes || &cur_playback != last_playback {
        *last_takes = cur_takes.clone();
        *last_playback = cur_playback;
        send_json(
            sender,
            &ServerToUi::TakesChanged {
                takes: cur_takes,
                playback: cur_playback,
            },
        )
        .await?;
    }

    // Host-DCC selection: emit on change.
    let cur_selection = server.host_selection().await;
    if &cur_selection != last_selection {
        *last_selection = cur_selection.clone();
        send_json(sender, &ServerToUi::SelectionChanged { selected: cur_selection }).await?;
    }

    Ok(())
}

async fn send_json(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    msg: &ServerToUi,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).map_err(axum::Error::new)?;
    sender.send(Message::Text(Utf8Bytes::from(text))).await
}

// ---------------------------------------------------------------------------
// Upstream command path (UI -> server)
// ---------------------------------------------------------------------------

/// Group B: UI-only commands that have no [`ControlMessage`] variant. The first
/// group dispatch to `ServerHandle` directly; the `host_*` group enqueue a
/// [`HostRequest`] for the DCC plugin to execute on its main thread.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum UiCommand {
    SetActiveScene { scene_id: SceneId },
    CreateMapping { req: MappingRequest },
    UpdateMapping { mapping_id: MappingId, req: MappingRequest },
    RemoveMapping { mapping_id: MappingId },
    DisconnectClient { device_id: Uuid },
    // DCC-side actions, bridged to the plugin via the host-request queue.
    ResyncScene,
    StartVideo {
        width: u32,
        height: u32,
        fps: u32,
        #[serde(default)]
        source: Option<String>,
    },
    StopVideo,
    BakeTake {
        take_id: Uuid,
        fps: u32,
        #[serde(default)]
        start_frame: i32,
    },
}

/// Parse an inbound text frame and dispatch it to `ServerHandle` methods. Tries the
/// existing [`ControlMessage`] verbs (Group A) first, then the UI-only envelope
/// (Group B). The connection is treated as an Operator (enforced at upgrade).
async fn dispatch_command(server: &ServerHandle, text: &str) -> Result<(), String> {
    if let Ok(msg) = serde_json::from_str::<ControlMessage>(text) {
        return dispatch_control_message(server, msg).await;
    }
    if let Ok(cmd) = serde_json::from_str::<UiCommand>(text) {
        return dispatch_ui_command(server, cmd).await;
    }
    Err(format!("unrecognized command: {text}"))
}

async fn dispatch_control_message(server: &ServerHandle, msg: ControlMessage) -> Result<(), String> {
    match msg {
        ControlMessage::SetDataFlow(state) => {
            server.set_data_flow(state).await.map_err(stringify)
        }
        ControlMessage::SetRecording(state) => {
            server.set_recording(state).await.map_err(stringify)
        }
        ControlMessage::ResetSceneToBaseline { scene_id } => {
            server.reset_scene_to_baseline(scene_id).await.map(|_| ()).map_err(stringify)
        }
        ControlMessage::CommitSceneBaseline { scene_id } => {
            server.commit_scene_baseline(scene_id).await.map(|_| ()).map_err(stringify)
        }
        ControlMessage::CommitObjectBaseline { scene_id, object_id } => server
            .commit_object_baseline(scene_id, object_id)
            .await
            .map(|_| ())
            .map_err(stringify),
        ControlMessage::SelectTake { take_id } => {
            server.select_take(take_id).await.map(|_| ()).map_err(stringify)
        }
        ControlMessage::DeleteTake { take_id } => {
            server.delete_take(take_id).await.map_err(stringify)
        }
        ControlMessage::PlaybackControl { take_id, action, seek_ns, looping } => {
            dispatch_playback(server, take_id, action, seek_ns, looping).await
        }
        other => Err(format!("command not supported over companion UI: {other:?}")),
    }
}

async fn dispatch_playback(
    server: &ServerHandle,
    take_id: Uuid,
    action: PlaybackAction,
    seek_ns: Option<u64>,
    looping: bool,
) -> Result<(), String> {
    match action {
        PlaybackAction::Play => server.playback_play(take_id, looping).await.map(|_| ()),
        PlaybackAction::Pause => server.playback_pause(take_id).await.map(|_| ()),
        PlaybackAction::Stop => server.playback_stop(take_id).await.map(|_| ()),
        PlaybackAction::Seek => server
            .playback_seek(take_id, seek_ns.unwrap_or(0), looping)
            .await
            .map(|_| ()),
    }
    .map_err(stringify)
}

async fn dispatch_ui_command(server: &ServerHandle, cmd: UiCommand) -> Result<(), String> {
    match cmd {
        UiCommand::SetActiveScene { scene_id } => {
            server.set_active_scene(scene_id).await.map_err(stringify)
        }
        // now_ns is server-supplied; never trust a client clock.
        UiCommand::CreateMapping { req } => {
            server.create_mapping(req, now_ns()).await.map(|_| ()).map_err(stringify)
        }
        UiCommand::UpdateMapping { mapping_id, req } => server
            .update_mapping(mapping_id, req, now_ns())
            .await
            .map(|_| ())
            .map_err(stringify),
        UiCommand::RemoveMapping { mapping_id } => {
            server.remove_mapping(mapping_id).await.map_err(stringify)
        }
        UiCommand::DisconnectClient { device_id } => {
            server.close_session(device_id, now_ns()).await.map_err(stringify)
        }
        // DCC-side actions: park on the host-request queue; the plugin executes them.
        UiCommand::ResyncScene => {
            server.enqueue_host_request(HostRequest::ResyncScene).await;
            Ok(())
        }
        UiCommand::StartVideo {
            width,
            height,
            fps,
            source,
        } => {
            server
                .enqueue_host_request(HostRequest::StartVideo {
                    width,
                    height,
                    fps,
                    source,
                })
                .await;
            Ok(())
        }
        UiCommand::StopVideo => {
            server.enqueue_host_request(HostRequest::StopVideo).await;
            Ok(())
        }
        UiCommand::BakeTake {
            take_id,
            fps,
            start_frame,
        } => {
            server
                .enqueue_host_request(HostRequest::BakeTake {
                    take_id,
                    fps,
                    start_frame,
                })
                .await;
            Ok(())
        }
    }
}

fn stringify<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}
