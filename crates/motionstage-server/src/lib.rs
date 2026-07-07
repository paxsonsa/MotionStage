use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    net::SocketAddr,
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;

use motionstage_core::{
    source_output_matches, AttributeUpdate, AttributeValue, CoreError, LeaseConfig, MappingId,
    MappingRequest, ObjectId, RuntimeCore, RuntimeSnapshot, Scene, SceneId,
};
use motionstage_discovery::{DiscoveryAdvertisement, DiscoveryPublisher};
use motionstage_media::{
    negotiate_stream, NegotiatedVideoStream, SignalingHub, VideoClientCapability, VideoCodec,
    VideoStreamDescriptor,
};
use motionstage_protocol::{
    negotiate_version, AttributeDescriptor, BakeAttributeValue, BaselineAction, ClientHello,
    ClientRole, ControlMessage, DataFlowState, Feature, MappingSummary, Mode, PlaybackAction,
    PlaybackRuntimeState, PlaybackSummary, ProtocolError, ProtocolVersion, RecordingState,
    RegisterAccepted, RegisterRejected, RegisterRequest, RejectCode, SamplingMode,
    SceneSnapshotPayload, SdpMessage, SdpType, ServerHello, SessionState, SessionSummary,
    SignalMessage, SignalPayload, SnapshotAttribute, SnapshotObject, SnapshotScene, StateEvent,
    StateEventEnvelope, TakeBakeAttribute, TakeInfo, VideoStreamStatus, WireError, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};
use motionstage_recording::{
    read_recording, RecordedAttribute, RecordedFrame, RecordingFile, RecordingManifest,
    RecordingMarker, RecordingWriter,
};
use motionstage_transport_quic::{MotionDatagram, QuicServer};
use motionstage_webrtc::WebRtcSession;
use thiserror::Error;
use tokio::sync::{broadcast, watch, RwLock};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

mod take_catalog;
use take_catalog::TakeCatalog;

#[cfg(feature = "companion-ui")]
pub mod companion_ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityMode {
    TrustedLan,
    PairingRequired,
    ApiKey,
    ApiKeyPlusPairing,
}

impl SecurityMode {
    fn as_str(self) -> &'static str {
        match self {
            SecurityMode::TrustedLan => "trusted_lan",
            SecurityMode::PairingRequired => "pairing_required",
            SecurityMode::ApiKey => "api_key",
            SecurityMode::ApiKeyPlusPairing => "api_key_plus_pairing",
        }
    }
}

/// Admission policy for the roles a session may hold. Roles are self-declared
/// in the [`ClientHello`]; this is where the server decides which of them to
/// actually grant, and records the basis ([`RoleGrant`]).
///
/// - `trusted_lan`: every peer that reaches the socket is trusted (the LAN is
///   the security boundary), so all declared roles — including
///   [`ClientRole::Operator`] — are granted. This is a deliberate, documented
///   trust choice; run a credentialed mode if the LAN is not trusted.
/// - credentialed modes: the shared credential authorized the connection.
///   MotionStage does not yet carry a per-credential role map, so declared
///   roles are still granted here — but this is the single choke point where a
///   real ACL belongs.
///
/// TODO(security): thread a per-credential/identity role allow-list through
/// [`ServerConfig`] and intersect it with the declared roles for credentialed
/// modes, so Operator can be withheld from a device that only holds a
/// motion-source credential.
fn authorize_roles(
    security_mode: SecurityMode,
    declared_roles: Vec<ClientRole>,
) -> (Vec<ClientRole>, RoleGrant) {
    match security_mode {
        SecurityMode::TrustedLan => (declared_roles, RoleGrant::LanTrust),
        SecurityMode::PairingRequired
        | SecurityMode::ApiKey
        | SecurityMode::ApiKeyPlusPairing => {
            // Future hook: intersect `declared_roles` with the credential's
            // authorized roles. Until that config exists the credential that
            // passed `ensure_auth` authorizes the declared roles.
            (declared_roles, RoleGrant::Credential)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub quic_bind_addr: SocketAddr,
    pub security_mode: SecurityMode,
    pub enable_discovery: bool,
    pub max_sessions: usize,
    pub tick_hz: u32,
    pub publish_hz: u32,
    pub supported_features: Vec<Feature>,
    pub lease: LeaseConfig,
    pub pairing_token: Option<String>,
    pub api_key: Option<String>,
    pub take_catalog_path: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "motionstage".into(),
            quic_bind_addr: "0.0.0.0:7788".parse().expect("static address parses"),
            security_mode: SecurityMode::TrustedLan,
            enable_discovery: true,
            max_sessions: 256,
            tick_hz: 120,
            publish_hz: 60,
            supported_features: vec![
                Feature::Motion,
                Feature::Mapping,
                Feature::Recording,
                Feature::Video,
                Feature::Hdr10,
                Feature::SdrFallback,
            ],
            lease: LeaseConfig::default(),
            pairing_token: None,
            api_key: None,
            take_catalog_path: PathBuf::from("recordings/takes_catalog.json"),
        }
    }
}

/// How a session's roles were authorized at registration — the audit record
/// behind the trust boundary. Roles (notably [`ClientRole::Operator`]) are
/// *self-declared* in the [`ClientHello`]; this records the admission basis on
/// which the server granted them, so the trust model is explicit and a future
/// per-credential ACL has a place to hook in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleGrant {
    /// `trusted_lan`: any declared role is granted on documented LAN trust —
    /// every peer that can reach the socket is trusted (the LAN is the
    /// security boundary).
    LanTrust,
    /// A credentialed mode (`pairing_required` / `api_key` /
    /// `api_key_plus_pairing`): the shared credential authorized the
    /// connection. Per-credential role authorization is not yet configurable,
    /// so declared roles are granted, but the basis is recorded here.
    Credential,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub device_id: Uuid,
    pub device_name: String,
    pub session_id: Option<Uuid>,
    /// The session's **admitted** roles — the authoritative capability set for
    /// this session. Established at register time from the declared
    /// [`ClientHello`] roles filtered through the admission policy
    /// ([`RoleGrant`]) and stored here. Operator-plane permission checks read
    /// this field via the server session record; they never trust roles
    /// re-sent on the wire per request.
    pub roles: Vec<ClientRole>,
    pub features: Vec<Feature>,
    pub advertised_attributes: Vec<AttributeDescriptor>,
    pub state: SessionState,
    /// Nanosecond timestamp of last activity (control message or motion datagram).
    pub last_activity_ns: u64,
    /// Protocol minor selected by version negotiation at the hello exchange
    /// (`None` before `hello_exchanged`). Echoed to the client in
    /// [`RegisterAccepted::negotiated_protocol_minor`].
    pub negotiated_protocol_minor: Option<u16>,
    /// The admission basis on which [`Self::roles`] were granted (`None` until
    /// registration). Recorded for the trust boundary / future per-credential
    /// role gating.
    pub role_grant: Option<RoleGrant>,
    /// When this connection is superseding a still-admitted session for the
    /// same `device_id`, the old session's id — its `SessionLeft` is deferred
    /// until this connection is itself admitted (passes `register`), so a
    /// pre-auth or failed-auth reconnect cannot evict a live session off the
    /// event stream. `None` once emitted or when nothing is being superseded.
    pub superseded_session_id: Option<Uuid>,
    /// True for the in-process host session (registered at server construction).
    /// The host is a real session — it appears in [`ServerHandle::sessions`],
    /// fires `SessionJoined`, and consumes the same event stream — but it is
    /// exempt from capacity checks and idle eviction.
    pub is_host: bool,
}

struct ActiveRecording {
    path: PathBuf,
    writer: RecordingWriter,
}

#[derive(Debug, Clone)]
struct ActivePlayback {
    take_id: Uuid,
    recording: RecordingFile,
    state: PlaybackRuntimeState,
    looping: bool,
    playhead_ns: u64,
    started_wall_ns: Option<u64>,
    started_playhead_ns: u64,
}

#[derive(Debug, Clone)]
struct TakeBakeCursor {
    take_id: Uuid,
    sampling_mode: SamplingMode,
    recording: RecordingFile,
    next_index: u64,
    total_frames: u64,
}

struct VideoPeerSession {
    peer: Arc<WebRtcSession>,
    track_added: bool,
}

struct RuntimeResources {
    quic_runtime: QuicRuntime,
    discovery: Option<DiscoveryPublisher>,
    scheduler_shutdown_tx: watch::Sender<bool>,
    tick_join: tokio::task::JoinHandle<()>,
    publish_join: tokio::task::JoinHandle<()>,
}

struct ServerState {
    config: ServerConfig,
    runtime: RuntimeCore,
    sessions: BTreeMap<Uuid, SessionInfo>,
    metrics: ServerMetrics,
    running: bool,
    active_recording: Option<ActiveRecording>,
    active_playback: Option<ActivePlayback>,
    take_catalog: TakeCatalog,
    bake_cursors: BTreeMap<Uuid, TakeBakeCursor>,
    master_video_descriptor: Option<VideoStreamDescriptor>,
    last_video_frame_ns: Option<u64>,
    video_keyframe_needed: bool,
    signaling: SignalingHub,
    video_peers: BTreeMap<Uuid, VideoPeerSession>,
    runtime_resources: Option<RuntimeResources>,
    active_advertisement: Option<DiscoveryAdvertisement>,
    last_published_snapshot: Option<RuntimeSnapshot>,
    /// Monotonic sequence for [`StateEventEnvelope`]s. Incremented while the
    /// state write lock is held so seq order always matches mutation order.
    event_seq: u64,
    /// Ring buffer of recent envelopes for resync replay.
    event_log: VecDeque<StateEventEnvelope>,
    /// DCC-side actions requested by the companion UI, drained by the plugin on its
    /// main-thread tick. The runtime never executes these itself.
    host_requests: Vec<HostRequest>,
    /// Object names selected in the host DCC, pushed by the plugin for UI highlight.
    host_selection: Vec<String>,
}

const VIDEO_STREAM_ACTIVITY_WINDOW_NS: u64 = 2_000_000_000;

impl ServerState {
    fn change_session_state(
        &mut self,
        device_id: Uuid,
        next: SessionState,
    ) -> Result<(), ServerError> {
        let session = self
            .sessions
            .get_mut(&device_id)
            .ok_or(ServerError::SessionNotFound(device_id))?;

        if !session.state.can_transition_to(next) {
            return Err(ServerError::Protocol(ProtocolError::InvalidTransition {
                from: session.state,
                to: next,
            }));
        }

        session.state = next;
        Ok(())
    }

    fn ensure_auth(&self, req: &RegisterRequest) -> Result<(), RejectCode> {
        match self.config.security_mode {
            SecurityMode::TrustedLan => Ok(()),
            SecurityMode::PairingRequired => {
                let Some(expected) = self.config.pairing_token.as_deref() else {
                    return Err(RejectCode::AuthFailed);
                };
                match req.pairing_token.as_deref() {
                    Some(token) if token == expected => Ok(()),
                    _ => Err(RejectCode::AuthFailed),
                }
            }
            SecurityMode::ApiKey => {
                let Some(expected) = self.config.api_key.as_deref() else {
                    return Err(RejectCode::AuthFailed);
                };
                match req.api_key.as_deref() {
                    Some(key) if key == expected => Ok(()),
                    _ => Err(RejectCode::AuthFailed),
                }
            }
            SecurityMode::ApiKeyPlusPairing => {
                let Some(pair) = self.config.pairing_token.as_deref() else {
                    return Err(RejectCode::AuthFailed);
                };
                let Some(key) = self.config.api_key.as_deref() else {
                    return Err(RejectCode::AuthFailed);
                };
                match (req.pairing_token.as_deref(), req.api_key.as_deref()) {
                    (Some(p), Some(k)) if p == pair && k == key => Ok(()),
                    _ => Err(RejectCode::AuthFailed),
                }
            }
        }
    }

    fn enforce_capacity(&self) -> Result<(), ServerError> {
        let active_or_pending = self
            .sessions
            .values()
            .filter(|session| !session.is_host && session.state != SessionState::Closed)
            .count();
        if active_or_pending >= self.config.max_sessions {
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code: RejectCode::CapacityExceeded,
                reason: "session capacity exceeded".into(),
            }));
        }
        Ok(())
    }

    fn enforce_unique_device_name(
        &self,
        device_id: Uuid,
        device_name: &str,
    ) -> Result<(), ServerError> {
        let normalized = device_name.trim();
        if normalized.is_empty() {
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code: RejectCode::RoleDenied,
                reason: "device name must not be empty".into(),
            }));
        }

        let conflict = self.sessions.values().any(|session| {
            session.device_id != device_id
                && session.state != SessionState::Closed
                && session.session_id.is_some()
                && session.device_name == normalized
        });
        if conflict {
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code: RejectCode::RoleDenied,
                reason: format!("device name '{normalized}' is already active"),
            }));
        }
        Ok(())
    }

    fn apply_playback_frame(&mut self, frame: &RecordedFrame, scene_id: SceneId) {
        for attr in &frame.attributes {
            let _ = self.runtime.set_scene_attribute_value(
                scene_id,
                attr.object_id,
                &attr.attribute,
                attr.value.clone(),
            );
        }
    }

    fn playback_duration_ns(recording: &RecordingFile) -> u64 {
        let Some(first) = recording.frames.first() else {
            return 0;
        };
        let Some(last) = recording.frames.last() else {
            return 0;
        };
        last.timestamp_ns.saturating_sub(first.timestamp_ns)
    }

    fn frame_for_playhead(recording: &RecordingFile, playhead_ns: u64) -> Option<RecordedFrame> {
        let first_ts = recording.frames.first()?.timestamp_ns;
        let target = first_ts.saturating_add(playhead_ns);
        let mut chosen = recording.frames.first()?.clone();
        for frame in &recording.frames {
            if frame.timestamp_ns > target {
                break;
            }
            chosen = frame.clone();
        }
        Some(chosen)
    }

    /// Advance active playback. Returns a [`StateEvent::PlaybackChanged`] when
    /// playback transitions on its own (e.g. a non-looping take reaches the
    /// end and stops), so the caller can replicate it.
    fn tick_playback(&mut self, now_ns: u64) -> Option<StateEvent> {
        let playback = self.active_playback.as_mut()?;
        if playback.state != PlaybackRuntimeState::Playing {
            return None;
        }

        let Some(started_wall_ns) = playback.started_wall_ns else {
            playback.started_wall_ns = Some(now_ns);
            return None;
        };

        let mut transition = None;
        let elapsed = now_ns.saturating_sub(started_wall_ns);
        let mut playhead = playback.started_playhead_ns.saturating_add(elapsed);
        let duration = Self::playback_duration_ns(&playback.recording);
        if duration > 0 && playhead > duration {
            if playback.looping {
                playhead %= duration;
                playback.started_wall_ns = Some(now_ns);
                playback.started_playhead_ns = playhead;
            } else {
                playhead = duration;
                playback.state = PlaybackRuntimeState::Stopped;
                playback.started_wall_ns = None;
                playback.started_playhead_ns = playhead;
                transition = Some(StateEvent::PlaybackChanged {
                    state: PlaybackRuntimeState::Stopped,
                    take_id: playback.take_id,
                    playhead_ns: playhead,
                    looping: playback.looping,
                });
            }
        }

        playback.playhead_ns = playhead;
        let scene_id = playback.recording.manifest.scene_id;
        if let Some(frame) = Self::frame_for_playhead(&playback.recording, playback.playhead_ns) {
            self.apply_playback_frame(&frame, scene_id);
        }
        transition
    }
}

/// A DCC-side action the companion UI asked for that the runtime cannot perform
/// itself (it must run on the host's main thread: reading/writing the DCC scene,
/// GPU capture, baking onto the host timeline). The plugin drains these from its
/// main-thread tick via [`ServerHandle::drain_host_requests`] and executes them.
#[derive(Debug, Clone, PartialEq)]
pub enum HostRequest {
    /// Re-enumerate the DCC scene into the runtime (objects + attributes).
    ResyncScene,
    /// Begin viewport video capture + streaming.
    StartVideo {
        width: u32,
        height: u32,
        fps: u32,
        source: Option<String>,
    },
    /// Stop viewport video capture.
    StopVideo,
    /// Bake a recorded take onto the host timeline as keyframes.
    BakeTake {
        take_id: Uuid,
        fps: u32,
        start_frame: i32,
    },
}

/// Identity and capability of the session performing a wire operator-plane
/// operation (mapping ops, take control). Built by the QUIC peer handler from
/// the session's registered hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireActor {
    /// The requesting session's id — stamped as `origin_session` on every
    /// event the operation emits.
    pub session_id: Uuid,
    /// The requesting session's device id — the ownership scope for
    /// non-operator mapping management.
    pub device_id: Uuid,
    /// True when the session holds [`ClientRole::Operator`]; operators manage
    /// any mapping and control takes.
    pub is_operator: bool,
}

/// Snapshot of playback transport state for the companion UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackStatus {
    pub take_id: Uuid,
    pub state: PlaybackRuntimeState,
    pub position_ns: u64,
    pub duration_ns: u64,
    pub looping: bool,
}

/// Capacity of the state-event broadcast channel; a receiver that falls more
/// than this many events behind observes `Lagged` and is resynced with a
/// fresh [`SceneSnapshotPayload`].
const EVENT_BROADCAST_CAPACITY: usize = 256;
/// Number of recent [`StateEventEnvelope`]s retained for resync replay.
const EVENT_LOG_CAPACITY: usize = 1024;

/// Server response to a [`ControlMessage::ResyncRequest`].
#[derive(Debug, Clone, PartialEq)]
pub enum ResyncResponse {
    /// The gap was still buffered: the exact missing envelopes, in seq order.
    Replay(Vec<StateEventEnvelope>),
    /// The gap fell out of the ring buffer (or the seq was from another
    /// epoch): a fresh full snapshot.
    Snapshot(SceneSnapshotPayload),
}

#[derive(Clone)]
pub struct ServerHandle {
    state: Arc<RwLock<ServerState>>,
    state_events: broadcast::Sender<StateEventEnvelope>,
    host_session_id: Uuid,
    host_device_id: Uuid,
}

#[derive(Debug, Clone, Default)]
pub struct ServerMetrics {
    pub accepted_sessions: u64,
    pub rejected_sessions: u64,
    pub motion_datagrams: u64,
    pub motion_updates: u64,
    pub signaling_messages: u64,
    pub scheduler_ticks: u64,
    pub publish_ticks: u64,
}

pub struct QuicRuntime {
    pub local_addr: SocketAddr,
    /// SHA-256 hex fingerprint of the server's TLS certificate.
    pub cert_fingerprint_hex: String,
    shutdown_tx: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl QuicRuntime {
    pub async fn shutdown(self) -> Result<(), ServerError> {
        let _ = self.shutdown_tx.send(true);
        self.join
            .await
            .map_err(|err| ServerError::Runtime(err.to_string()))?;
        Ok(())
    }
}

impl ServerHandle {
    pub fn new(config: ServerConfig) -> Self {
        let (state_events, _state_events_rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);

        // The host DCC is a real session from the start: it holds roles, shows
        // up in the session table, and fires SessionJoined like any player.
        let host_device_id = Uuid::new_v4();
        let host_session_id = Uuid::new_v4();
        let mut sessions = BTreeMap::new();
        sessions.insert(
            host_device_id,
            SessionInfo {
                device_id: host_device_id,
                device_name: "host".into(),
                session_id: Some(host_session_id),
                roles: vec![ClientRole::SceneAuthor, ClientRole::Operator],
                features: config.supported_features.clone(),
                advertised_attributes: Vec::new(),
                state: SessionState::Active,
                last_activity_ns: now_ns(),
                negotiated_protocol_minor: Some(PROTOCOL_MINOR),
                role_grant: Some(RoleGrant::LanTrust),
                superseded_session_id: None,
                is_host: true,
            },
        );

        let state = ServerState {
            runtime: RuntimeCore::new(config.lease),
            sessions,
            metrics: ServerMetrics::default(),
            running: false,
            active_recording: None,
            active_playback: None,
            take_catalog: TakeCatalog::load_or_new(&config.take_catalog_path).unwrap_or_else(
                |err| {
                    warn!(
                        path = %config.take_catalog_path.display(),
                        error = %err,
                        "failed to load take catalog, starting empty"
                    );
                    TakeCatalog::load_or_new("recordings/takes_catalog.json")
                        .unwrap_or_else(|_| panic!("fallback take catalog must be creatable"))
                },
            ),
            bake_cursors: BTreeMap::new(),
            master_video_descriptor: None,
            last_video_frame_ns: None,
            video_keyframe_needed: false,
            signaling: SignalingHub::default(),
            video_peers: BTreeMap::new(),
            runtime_resources: None,
            active_advertisement: None,
            last_published_snapshot: None,
            event_seq: 0,
            event_log: VecDeque::new(),
            host_requests: Vec::new(),
            host_selection: Vec::new(),
            config,
        };

        let handle = Self {
            state: Arc::new(RwLock::new(state)),
            state_events,
            host_session_id,
            host_device_id,
        };

        // Emit the host's SessionJoined like any other session join. No other
        // handle exists yet, so try_write cannot fail.
        {
            let mut state = handle
                .state
                .try_write()
                .expect("state lock is uncontended during construction");
            let (device_name, roles) = state
                .sessions
                .get(&host_device_id)
                .map(|s| (s.device_name.clone(), s.roles.clone()))
                .expect("host session was just inserted");
            handle.emit_event(
                &mut state,
                Some(host_session_id),
                StateEvent::SessionJoined {
                    session_id: host_session_id,
                    device_id: host_device_id,
                    device_name,
                    roles,
                },
            );
        }

        handle
    }

    /// Session id of the in-process host session. Host-API mutations stamp
    /// this as their `origin_session`.
    pub fn host_session_id(&self) -> Uuid {
        self.host_session_id
    }

    /// Device id of the in-process host session.
    pub fn host_device_id(&self) -> Uuid {
        self.host_device_id
    }

    /// Assign the next seq, record the envelope in the replay ring buffer, and
    /// fan it out. Must be called while `state`'s write lock is held so seq
    /// order matches mutation order.
    fn emit_event(
        &self,
        state: &mut ServerState,
        origin_session: Option<Uuid>,
        event: StateEvent,
    ) -> u64 {
        state.event_seq += 1;
        let envelope = StateEventEnvelope {
            seq: state.event_seq,
            origin_session,
            timestamp_ns: now_ns(),
            event,
        };
        if state.event_log.len() == EVENT_LOG_CAPACITY {
            state.event_log.pop_front();
        }
        state.event_log.push_back(envelope.clone());
        let _ = self.state_events.send(envelope);
        state.event_seq
    }

    /// Subscribe to the ordered state-event stream. Every mutation of server
    /// state is observable here on every transport; see [`StateEvent`].
    pub fn subscribe_state_events(&self) -> broadcast::Receiver<StateEventEnvelope> {
        self.state_events.subscribe()
    }

    /// Sequence number of the most recently emitted state event.
    pub async fn current_event_seq(&self) -> u64 {
        let state = self.state.read().await;
        state.event_seq
    }

    fn build_scene_snapshot(state: &ServerState) -> SceneSnapshotPayload {
        let snapshot = state.runtime.snapshot();
        // Only registered sessions: exactly the set replicated by
        // SessionJoined/SessionLeft. Sessions closed/evicted before a
        // session_id was assigned never emit events, so a snapshot must not
        // show them either.
        let sessions = state
            .sessions
            .values()
            .filter(|session| session.state != SessionState::Closed)
            .filter_map(|session| {
                session.session_id.map(|session_id| SessionSummary {
                    session_id,
                    device_id: session.device_id,
                    device_name: session.device_name.clone(),
                    roles: session.roles.clone(),
                    is_host: session.is_host,
                })
            })
            .collect();
        let playback = state.active_playback.as_ref().map(|playback| PlaybackSummary {
            state: playback.state,
            take_id: playback.take_id,
            playhead_ns: playback.playhead_ns,
            looping: playback.looping,
        });
        SceneSnapshotPayload {
            scenes: snapshot.scenes.values().map(scene_to_snapshot).collect(),
            mappings: snapshot.mappings.values().map(mapping_to_summary).collect(),
            mode: state.runtime.mode(),
            active_scene: snapshot.active_scene,
            sessions,
            takes: state.take_catalog.list(None),
            playback,
            seq: state.event_seq,
        }
    }

    /// Full world snapshot (scene graphs, mappings, mode, active scene,
    /// registered sessions, take catalog, playback transport) stamped with
    /// the current event seq.
    pub async fn scene_snapshot_payload(&self) -> SceneSnapshotPayload {
        let state = self.state.read().await;
        Self::build_scene_snapshot(&state)
    }

    /// Reconnect support: given the last seq a client observed, either replay
    /// the exact missing envelopes (still buffered) or hand back a fresh
    /// snapshot.
    pub async fn resync_from(&self, last_seq: u64) -> ResyncResponse {
        let state = self.state.read().await;
        if last_seq > state.event_seq {
            // Seq from a previous server epoch: only a snapshot is safe.
            return ResyncResponse::Snapshot(Self::build_scene_snapshot(&state));
        }
        if last_seq == state.event_seq {
            return ResyncResponse::Replay(Vec::new());
        }
        match state.event_log.front() {
            Some(oldest) if oldest.seq <= last_seq + 1 => ResyncResponse::Replay(
                state
                    .event_log
                    .iter()
                    .filter(|envelope| envelope.seq > last_seq)
                    .cloned()
                    .collect(),
            ),
            _ => ResyncResponse::Snapshot(Self::build_scene_snapshot(&state)),
        }
    }

    pub async fn start(&self) -> Result<DiscoveryAdvertisement, ServerError> {
        {
            let state = self.state.read().await;
            if state.running {
                if let Some(adv) = &state.active_advertisement {
                    return Ok(adv.clone());
                }
            }
        }

        let (name, features, security_mode, enable_discovery, tick_hz, publish_hz) = {
            let state = self.state.read().await;
            (
                state.config.name.clone(),
                state.config.supported_features.clone(),
                state.config.security_mode.as_str().to_owned(),
                state.config.enable_discovery,
                state.config.tick_hz,
                state.config.publish_hz,
            )
        };

        let quic_runtime = self.start_quic_runtime().await?;
        let host = advertised_host_for(quic_runtime.local_addr);
        let advertisement = DiscoveryAdvertisement {
            service_name: name,
            bind_host: host,
            bind_port: quic_runtime.local_addr.port(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            features,
            security_mode,
            cert_fingerprint: Some(quic_runtime.cert_fingerprint_hex.clone()),
        };

        let discovery = if enable_discovery {
            Some(
                DiscoveryPublisher::advertise(&advertisement)
                    .map_err(|err| ServerError::Discovery(err.to_string()))?,
            )
        } else {
            None
        };

        let (scheduler_shutdown_tx, tick_join, publish_join) =
            self.spawn_scheduler_loops(tick_hz, publish_hz);

        {
            let mut state = self.state.write().await;
            state.config.quic_bind_addr = quic_runtime.local_addr;
            state.running = true;
            state.active_advertisement = Some(advertisement.clone());
            state.runtime_resources = Some(RuntimeResources {
                quic_runtime,
                discovery,
                scheduler_shutdown_tx,
                tick_join,
                publish_join,
            });
        }

        info!(
            server_name = %advertisement.service_name,
            bind_host = %advertisement.bind_host,
            bind_port = advertisement.bind_port,
            discovery_enabled = enable_discovery,
            "motionstage server started"
        );
        Ok(advertisement)
    }

    pub async fn stop(&self) -> Result<(), ServerError> {
        let (name, resources) = {
            let mut state = self.state.write().await;
            state.running = false;
            state.active_advertisement = None;
            (state.config.name.clone(), state.runtime_resources.take())
        };

        if let Some(resources) = resources {
            let _ = resources.scheduler_shutdown_tx.send(true);
            resources
                .tick_join
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
            resources
                .publish_join
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
            resources.quic_runtime.shutdown().await?;
            if let Some(publisher) = resources.discovery {
                publisher
                    .stop()
                    .map_err(|err| ServerError::Discovery(err.to_string()))?;
            }
        }

        info!(server_name = %name, "motionstage server stopped");
        Ok(())
    }

    pub async fn quic_bind_addr(&self) -> SocketAddr {
        let state = self.state.read().await;
        state.config.quic_bind_addr
    }

    pub async fn tick_count(&self) -> u64 {
        let state = self.state.read().await;
        state.runtime.tick_count()
    }

    pub async fn last_published_snapshot(&self) -> Option<RuntimeSnapshot> {
        let state = self.state.read().await;
        state.last_published_snapshot.clone()
    }

    fn spawn_scheduler_loops(
        &self,
        tick_hz: u32,
        publish_hz: u32,
    ) -> (
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (shutdown_tx, mut tick_shutdown_rx) = watch::channel(false);
        let mut publish_shutdown_rx = shutdown_tx.subscribe();

        let tick_server = self.clone();
        let tick_period_ns = (1_000_000_000_u64 / tick_hz.max(1) as u64).max(1);
        let tick_join = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_nanos(tick_period_ns));
            loop {
                tokio::select! {
                    changed = tick_shutdown_rx.changed() => {
                        if changed.is_ok() && *tick_shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        let mut state = tick_server.state.write().await;
                        if !state.running {
                            continue;
                        }
                        let now = now_ns();
                        let released = state.runtime.scheduler_tick(now);
                        for mapping_id in released {
                            tick_server.emit_event(
                                &mut state,
                                None,
                                StateEvent::MappingReleased {
                                    mapping_id,
                                    reason: "mapping lease expired".into(),
                                },
                            );
                        }
                        if let Some(event) = state.tick_playback(now) {
                            tick_server.emit_event(&mut state, None, event);
                        }

                        // Evict sessions that have been idle beyond the configured timeout (4.4).
                        // The in-process host session is never evicted.
                        let idle_timeout = state.config.lease.session_idle_timeout_ns;
                        if idle_timeout > 0 {
                            let expired: Vec<(Uuid, Option<Uuid>)> = state.sessions.values()
                                .filter(|s| {
                                    !s.is_host
                                        && s.state != SessionState::Closed
                                        && s.state != SessionState::Discovered
                                        && now.saturating_sub(s.last_activity_ns) >= idle_timeout
                                })
                                .map(|s| (s.device_id, s.session_id))
                                .collect();
                            for (device_id, session_id) in expired {
                                warn!(%device_id, "session idle timeout; closing");
                                state.runtime.register_device_disconnected(device_id, now);
                                if state.change_session_state(device_id, SessionState::Closed).is_ok() {
                                    if let Some(session_id) = session_id {
                                        tick_server.emit_event(
                                            &mut state,
                                            Some(session_id),
                                            StateEvent::SessionLeft {
                                                session_id,
                                                reason: Some("idle timeout".into()),
                                            },
                                        );
                                    }
                                }
                            }
                        }

                        state.metrics.scheduler_ticks += 1;
                        trace!(scheduler_ticks = state.metrics.scheduler_ticks, "scheduler tick");
                    }
                }
            }
        });

        let publish_server = self.clone();
        let publish_period_ns = (1_000_000_000_u64 / publish_hz.max(1) as u64).max(1);
        let publish_join = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_nanos(publish_period_ns));
            loop {
                tokio::select! {
                    changed = publish_shutdown_rx.changed() => {
                        if changed.is_ok() && *publish_shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        let mut state = publish_server.state.write().await;
                        if !state.running {
                            continue;
                        }
                        state.last_published_snapshot = Some(state.runtime.snapshot());
                        state.metrics.publish_ticks += 1;
                        trace!(publish_ticks = state.metrics.publish_ticks, "publish tick");
                    }
                }
            }
        });

        (shutdown_tx, tick_join, publish_join)
    }

    pub async fn start_quic_runtime(&self) -> Result<QuicRuntime, ServerError> {
        let bind_addr = self.quic_bind_addr().await;
        let quic =
            QuicServer::bind(bind_addr).map_err(|err| ServerError::Runtime(err.to_string()))?;
        let local_addr = quic
            .local_addr()
            .map_err(|err| ServerError::Runtime(err.to_string()))?;
        let cert_fingerprint_hex = quic.cert_fingerprint_hex();
        let runtime_server = self.clone();

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accept = quic.accept() => {
                        let Ok(peer) = accept else {
                            continue;
                        };
                        let server = runtime_server.clone();
                        tokio::spawn(async move {
                            let _ = handle_quic_peer(server, peer).await;
                        });
                    }
                }
            }
        });

        Ok(QuicRuntime {
            local_addr,
            cert_fingerprint_hex,
            shutdown_tx,
            join,
        })
    }

    pub async fn server_hello(&self) -> ServerHello {
        let state = self.state.read().await;
        ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            features: state.config.supported_features.clone(),
            security_mode: state.config.security_mode.as_str().into(),
        }
    }

    pub async fn discovered(
        &self,
        device_id: Uuid,
        device_name: impl Into<String>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let device_name = device_name.into();
        if !state.sessions.contains_key(&device_id) {
            state.enforce_capacity()?;
        }
        // Reconnect racing a still-admitted previous connection: the old
        // session's terminal SessionLeft is DEFERRED until this new connection
        // is itself admitted (passes `register`). A pre-auth reconnect — or
        // one that later fails admission — must not be a free takeover of a
        // live session on the event stream. We stash the old session id here
        // and `register` emits its SessionLeft only after admission succeeds.
        //
        // Trust-boundary caveat: sessions are keyed by the self-claimed
        // `device_id`, so this new record overwrites the old one's mutable
        // handshake fields immediately. Deferring the SessionLeft is what
        // makes admission (not mere reconnection) the gate for evicting a live
        // session from the replicated stream. See the Security Model in
        // docs/design-architecture.md.
        let superseded_session_id = state.sessions.get(&device_id).and_then(|old| {
            if old.state != SessionState::Closed {
                old.session_id
            } else {
                None
            }
        });
        state.sessions.insert(
            device_id,
            SessionInfo {
                device_id,
                device_name: device_name.clone(),
                session_id: None,
                roles: Vec::new(),
                features: Vec::new(),
                advertised_attributes: Vec::new(),
                state: SessionState::Discovered,
                last_activity_ns: now_ns(),
                negotiated_protocol_minor: None,
                role_grant: None,
                superseded_session_id,
                is_host: false,
            },
        );
        debug!(%device_id, device_name = %device_name, "session discovered");
        Ok(())
    }

    /// Number of non-closed client sessions. The in-process host session is
    /// excluded (it always exists); use [`ServerHandle::sessions`] to see it.
    pub async fn session_count(&self) -> usize {
        let state = self.state.read().await;
        state
            .sessions
            .values()
            .filter(|session| !session.is_host && session.state != SessionState::Closed)
            .count()
    }

    pub async fn metrics(&self) -> ServerMetrics {
        let state = self.state.read().await;
        state.metrics.clone()
    }

    pub async fn transport_connected(&self, device_id: Uuid) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state.change_session_state(device_id, SessionState::TransportConnected)
    }

    pub async fn hello_exchanged(&self, hello: ClientHello) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        // The server REQUIRES its own major and speaks exactly its own minor
        // (see `negotiate_version`). A foreign major or a client that is newer
        // than us is rejected with a typed `RegisterRejected` before the drop
        // so the peer learns why, rather than a bare disconnect.
        let negotiated = match negotiate_version(
            ProtocolVersion::new(PROTOCOL_MAJOR, PROTOCOL_MINOR),
            ProtocolVersion::new(hello.protocol_major, hello.protocol_minor),
        ) {
            Ok(negotiated) => negotiated,
            Err(err @ ProtocolError::UnsupportedMajor { .. }) => {
                return Err(ServerError::RegisterRejected(RegisterRejected {
                    code: RejectCode::UnsupportedProtocol,
                    reason: err.to_string(),
                }));
            }
            Err(err @ ProtocolError::ClientTooNew { .. }) => {
                return Err(ServerError::RegisterRejected(RegisterRejected {
                    code: RejectCode::VersionMismatch,
                    reason: err.to_string(),
                }));
            }
            Err(err) => return Err(ServerError::Protocol(err)),
        };
        if hello.features.is_empty() {
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code: RejectCode::NoCommonFeature,
                reason: "client has no features".into(),
            }));
        }
        if hello.roles.is_empty() {
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code: RejectCode::RoleDenied,
                reason: "client must declare at least one role".into(),
            }));
        }
        if hello.roles.contains(&ClientRole::MotionSource) && hello.advertised_attributes.is_empty()
        {
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code: RejectCode::RoleDenied,
                reason: "motion source must advertise at least one attribute".into(),
            }));
        }

        let session = state
            .sessions
            .get_mut(&hello.device_id)
            .ok_or(ServerError::SessionNotFound(hello.device_id))?;
        session.roles = hello.roles;
        session.features = hello.features;
        session.advertised_attributes = hello.advertised_attributes;
        session.negotiated_protocol_minor = Some(negotiated.selected.minor);
        state.change_session_state(hello.device_id, SessionState::HelloExchanged)
    }

    pub async fn authenticate(&self, device_id: Uuid) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state.change_session_state(device_id, SessionState::Authenticated)
    }

    pub async fn register(
        &self,
        device_id: Uuid,
        req: RegisterRequest,
    ) -> Result<RegisterAccepted, ServerError> {
        let mut state = self.state.write().await;
        let supported_features = state.config.supported_features.clone();

        if let Err(code) = state.ensure_auth(&req) {
            state.metrics.rejected_sessions += 1;
            warn!(%device_id, ?code, "registration rejected due to auth policy");
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code,
                reason: "auth failed".into(),
            }));
        }

        let device_name = state
            .sessions
            .get(&device_id)
            .map(|session| session.device_name.clone())
            .ok_or(ServerError::SessionNotFound(device_id))?;
        if let Err(err) = state.enforce_unique_device_name(device_id, &device_name) {
            state.metrics.rejected_sessions += 1;
            return Err(err);
        }

        let security_mode = state.config.security_mode;
        let session = state
            .sessions
            .get_mut(&device_id)
            .ok_or(ServerError::SessionNotFound(device_id))?;

        let negotiated_features: Vec<Feature> = session
            .features
            .iter()
            .copied()
            .filter(|feature| supported_features.contains(feature))
            .collect();

        if negotiated_features.is_empty() {
            state.metrics.rejected_sessions += 1;
            warn!(%device_id, "registration rejected due to no common features");
            return Err(ServerError::RegisterRejected(RegisterRejected {
                code: RejectCode::NoCommonFeature,
                reason: "no compatible feature".into(),
            }));
        }

        // Admission gates the roles. Under `trusted_lan` any declared role is
        // granted on documented LAN trust; under credentialed modes the
        // credential authorized the connection (per-credential role ACLs are
        // the future hook — see `authorize_roles`). The GRANTED roles are the
        // ones stored on the session record and read by every permission
        // check, so a client cannot escalate by re-declaring Operator on the
        // wire after registration.
        let (granted_roles, role_grant) =
            authorize_roles(security_mode, std::mem::take(&mut session.roles));
        session.roles = granted_roles;
        session.role_grant = Some(role_grant);

        let session_id = Uuid::new_v4();
        session.session_id = Some(session_id);
        let negotiated_protocol_minor = session
            .negotiated_protocol_minor
            .unwrap_or(PROTOCOL_MINOR);
        // The reconnect this connection is superseding (if any): now that the
        // new connection is admitted, retire the old session on the event
        // stream. Deferring to here is the fix for the "superseded by
        // reconnect" free-takeover — a connection that never reached admission
        // never fires this.
        let superseded_session_id = session.superseded_session_id.take();
        state.change_session_state(device_id, SessionState::Registered)?;
        state.metrics.accepted_sessions += 1;
        debug!(%device_id, %session_id, ?role_grant, "registration accepted");

        if let Some(old_session_id) = superseded_session_id {
            self.emit_event(
                &mut state,
                Some(old_session_id),
                StateEvent::SessionLeft {
                    session_id: old_session_id,
                    reason: Some("superseded by reconnect".into()),
                },
            );
        }

        Ok(RegisterAccepted {
            session_id,
            negotiated_features,
            negotiated_protocol_minor,
        })
    }

    pub async fn scene_synced(&self, device_id: Uuid) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state.change_session_state(device_id, SessionState::SceneSynced)
    }

    pub async fn activate(&self, device_id: Uuid) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state.runtime.register_device_connected(device_id);
        state.change_session_state(device_id, SessionState::Active)?;
        let (device_name, session_id, roles) = state
            .sessions
            .get(&device_id)
            .map(|s| (s.device_name.clone(), s.session_id, s.roles.clone()))
            .unwrap_or_default();
        if let Some(recording) = state.active_recording.as_mut() {
            recording.writer.push_marker(RecordingMarker::ClientJoined {
                timestamp_ns: now_ns(),
                device_id,
                device_name: device_name.clone(),
            });
        }
        if let Some(session_id) = session_id {
            self.emit_event(
                &mut state,
                Some(session_id),
                StateEvent::SessionJoined {
                    session_id,
                    device_id,
                    device_name,
                    roles,
                },
            );
        }
        Ok(())
    }

    pub async fn touch_session_activity(&self, device_id: Uuid) {
        let mut state = self.state.write().await;
        if let Some(session) = state.sessions.get_mut(&device_id) {
            session.last_activity_ns = now_ns();
        }
    }

    pub async fn close_session(&self, device_id: Uuid, now_ns: u64) -> Result<(), ServerError> {
        self.close_session_with_reason(device_id, now_ns, None)
            .await
    }

    pub async fn close_session_with_reason(
        &self,
        device_id: Uuid,
        now_ns: u64,
        reason: Option<String>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        if let Some(recording) = state.active_recording.as_mut() {
            recording.writer.push_marker(RecordingMarker::ClientLeft {
                timestamp_ns: now_ns,
                device_id,
                reason: reason.clone(),
            });
        }
        state
            .runtime
            .register_device_disconnected(device_id, now_ns);
        state.video_peers.remove(&device_id);
        state.change_session_state(device_id, SessionState::Closed)?;
        if let Some(session_id) = state.sessions.get(&device_id).and_then(|s| s.session_id) {
            self.emit_event(
                &mut state,
                Some(session_id),
                StateEvent::SessionLeft { session_id, reason },
            );
        }
        Ok(())
    }

    /// Load a scene, stamping the in-process host session as event origin.
    pub async fn load_scene(&self, scene: Scene) -> SceneId {
        self.load_scene_from(scene, Some(self.host_session_id))
            .await
    }

    pub async fn load_scene_from(&self, scene: Scene, origin: Option<Uuid>) -> SceneId {
        let mut state = self.state.write().await;
        let name = scene.name.clone();
        let scene_id = state.runtime.load_scene(scene);
        self.emit_event(&mut state, origin, StateEvent::SceneLoaded { scene_id, name });
        scene_id
    }

    /// Activate a scene, stamping the host session as event origin.
    pub async fn set_active_scene(&self, scene_id: SceneId) -> Result<(), ServerError> {
        self.set_active_scene_from(scene_id, Some(self.host_session_id))
            .await
    }

    pub async fn set_active_scene_from(
        &self,
        scene_id: SceneId,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state
            .runtime
            .set_active_scene(scene_id)
            .map_err(ServerError::Core)?;
        self.emit_event(&mut state, origin, StateEvent::SceneActivated { scene_id });
        Ok(())
    }

    /// Set the data-flow axis, stamping the host session as event origin.
    pub async fn set_data_flow(&self, data_flow: DataFlowState) -> Result<(), ServerError> {
        self.set_data_flow_from(data_flow, Some(self.host_session_id))
            .await
    }

    pub async fn set_data_flow_from(
        &self,
        data_flow: DataFlowState,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let from = state.runtime.mode();
        state
            .runtime
            .set_data_flow(data_flow)
            .map_err(ServerError::Core)?;
        let to = state.runtime.mode();
        if to.recording != RecordingState::Playback {
            state.active_playback = None;
        }
        if let Some(recording) = state.active_recording.as_mut() {
            recording
                .writer
                .push_marker(RecordingMarker::ModeTransition {
                    timestamp_ns: now_ns(),
                    from,
                    to,
                });
        }
        self.emit_event(&mut state, origin, StateEvent::ModeChanged { mode: to });
        Ok(())
    }

    /// Set the recording axis, stamping the host session as event origin.
    pub async fn set_recording(&self, recording: RecordingState) -> Result<(), ServerError> {
        self.set_recording_from(recording, Some(self.host_session_id))
            .await
    }

    pub async fn set_recording_from(
        &self,
        recording: RecordingState,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let from = state.runtime.mode();
        state
            .runtime
            .set_recording(recording)
            .map_err(ServerError::Core)?;
        let to = state.runtime.mode();
        if to.recording != RecordingState::Playback {
            state.active_playback = None;
        }
        if let Some(rec) = state.active_recording.as_mut() {
            rec.writer.push_marker(RecordingMarker::ModeTransition {
                timestamp_ns: now_ns(),
                from,
                to,
            });
        }
        self.emit_event(&mut state, origin, StateEvent::ModeChanged { mode: to });
        Ok(())
    }

    /// Queue a DCC-side action requested by the companion UI. The plugin drains and
    /// executes it on its main thread (the runtime never touches the DCC directly).
    pub async fn enqueue_host_request(&self, request: HostRequest) {
        let mut state = self.state.write().await;
        state.host_requests.push(request);
    }

    /// Drain pending host requests for the plugin to execute on its main thread.
    pub async fn drain_host_requests(&self) -> Vec<HostRequest> {
        let mut state = self.state.write().await;
        std::mem::take(&mut state.host_requests)
    }

    /// Record the objects selected in the host DCC (by name), for UI highlight.
    pub async fn set_host_selection(&self, names: Vec<String>) {
        let mut state = self.state.write().await;
        state.host_selection = names;
    }

    /// Objects currently selected in the host DCC (by name).
    pub async fn host_selection(&self) -> Vec<String> {
        let state = self.state.read().await;
        state.host_selection.clone()
    }

    /// Current playback transport status, if a take is loaded.
    pub async fn playback_status(&self) -> Option<PlaybackStatus> {
        let state = self.state.read().await;
        let playback = state.active_playback.as_ref()?;
        Some(PlaybackStatus {
            take_id: playback.take_id,
            state: playback.state,
            position_ns: playback.playhead_ns,
            duration_ns: ServerState::playback_duration_ns(&playback.recording),
            looping: playback.looping,
        })
    }

    /// Convenience: set both axes of the composite mode in one call.
    pub async fn set_mode(&self, mode: Mode) -> Result<(), ServerError> {
        // Order matters: stop recording/playback before reducing data flow,
        // but start data flow before enabling recording/playback.
        if mode.recording == RecordingState::Inactive {
            self.set_recording(mode.recording).await?;
            self.set_data_flow(mode.data_flow).await?;
        } else {
            self.set_data_flow(mode.data_flow).await?;
            self.set_recording(mode.recording).await?;
        }
        Ok(())
    }

    fn resolve_scene_or_active(
        state: &ServerState,
        requested_scene: Option<SceneId>,
    ) -> Result<SceneId, ServerError> {
        if let Some(scene_id) = requested_scene {
            return Ok(scene_id);
        }
        state
            .runtime
            .snapshot()
            .active_scene
            .ok_or_else(|| ServerError::Core(CoreError::MappingDenied("no active scene".into())))
    }

    pub async fn reset_scene_to_baseline(
        &self,
        scene_id: Option<SceneId>,
    ) -> Result<u32, ServerError> {
        self.reset_scene_to_baseline_from(scene_id, Some(self.host_session_id))
            .await
    }

    pub async fn reset_scene_to_baseline_from(
        &self,
        scene_id: Option<SceneId>,
        origin: Option<Uuid>,
    ) -> Result<u32, ServerError> {
        let mut state = self.state.write().await;
        let resolved = Self::resolve_scene_or_active(&state, scene_id)?;
        let changed = state
            .runtime
            .reset_scene_to_baseline(resolved)
            .map_err(ServerError::Core)?;
        self.emit_event(
            &mut state,
            origin,
            StateEvent::BaselineApplied {
                action: BaselineAction::ResetScene,
                changed_attributes: changed,
            },
        );
        Ok(changed)
    }

    pub async fn commit_scene_baseline(
        &self,
        scene_id: Option<SceneId>,
    ) -> Result<u32, ServerError> {
        self.commit_scene_baseline_from(scene_id, Some(self.host_session_id))
            .await
    }

    pub async fn commit_scene_baseline_from(
        &self,
        scene_id: Option<SceneId>,
        origin: Option<Uuid>,
    ) -> Result<u32, ServerError> {
        let mut state = self.state.write().await;
        let resolved = Self::resolve_scene_or_active(&state, scene_id)?;
        let changed = state
            .runtime
            .commit_scene_baseline(resolved)
            .map_err(ServerError::Core)?;
        self.emit_event(
            &mut state,
            origin,
            StateEvent::BaselineApplied {
                action: BaselineAction::CommitScene,
                changed_attributes: changed,
            },
        );
        Ok(changed)
    }

    pub async fn commit_object_baseline(
        &self,
        scene_id: Option<SceneId>,
        object_id: ObjectId,
    ) -> Result<u32, ServerError> {
        self.commit_object_baseline_from(scene_id, object_id, Some(self.host_session_id))
            .await
    }

    pub async fn commit_object_baseline_from(
        &self,
        scene_id: Option<SceneId>,
        object_id: ObjectId,
        origin: Option<Uuid>,
    ) -> Result<u32, ServerError> {
        let mut state = self.state.write().await;
        let resolved = Self::resolve_scene_or_active(&state, scene_id)?;
        let changed = state
            .runtime
            .commit_object_baseline(resolved, object_id)
            .map_err(ServerError::Core)?;
        self.emit_event(
            &mut state,
            origin,
            StateEvent::BaselineApplied {
                action: BaselineAction::CommitObject,
                changed_attributes: changed,
            },
        );
        Ok(changed)
    }

    pub async fn mode(&self) -> Mode {
        let state = self.state.read().await;
        state.runtime.mode()
    }

    pub async fn runtime_snapshot(&self) -> RuntimeSnapshot {
        let state = self.state.read().await;
        state.runtime.snapshot()
    }

    pub async fn create_mapping(
        &self,
        req: MappingRequest,
        now_ns: u64,
    ) -> Result<MappingId, ServerError> {
        self.create_mapping_from(req, now_ns, Some(self.host_session_id))
            .await
    }

    pub async fn create_mapping_from(
        &self,
        req: MappingRequest,
        now_ns: u64,
        origin: Option<Uuid>,
    ) -> Result<MappingId, ServerError> {
        let mut state = self.state.write().await;
        self.create_mapping_locked(&mut state, req, now_ns, origin)
            .map(|summary| summary.mapping_id)
    }

    /// Create a mapping while the state write lock is held: mutate, write the
    /// recording marker, and emit exactly one [`StateEvent::MappingCreated`].
    fn create_mapping_locked(
        &self,
        state: &mut ServerState,
        req: MappingRequest,
        now_ns: u64,
        origin: Option<Uuid>,
    ) -> Result<MappingSummary, ServerError> {
        let mapping_id = state
            .runtime
            .create_mapping(req, now_ns)
            .map_err(ServerError::Core)?;
        let mapping = state
            .runtime
            .snapshot()
            .mappings
            .get(&mapping_id)
            .cloned()
            .ok_or_else(|| ServerError::Runtime("created mapping missing from runtime".into()))?;
        if let Some(recording) = state.active_recording.as_mut() {
            recording
                .writer
                .push_marker(RecordingMarker::MappingCreated {
                    timestamp_ns: now_ns,
                    mapping_id,
                    source_device: mapping.source_device,
                    source_output: mapping.source_output.clone(),
                    target_scene: mapping.target_scene,
                    target_object: mapping.target_object,
                    target_attribute: mapping.target_attribute.clone(),
                    component_mask: mapping.component_mask.clone(),
                });
        }
        let summary = mapping_to_summary(&mapping);
        self.emit_event(
            state,
            origin,
            StateEvent::MappingCreated {
                mapping: summary.clone(),
            },
        );
        Ok(summary)
    }

    pub async fn update_mapping(
        &self,
        mapping_id: MappingId,
        req: MappingRequest,
        now_ns: u64,
    ) -> Result<(), ServerError> {
        self.update_mapping_from(mapping_id, req, now_ns, Some(self.host_session_id))
            .await
    }

    pub async fn update_mapping_from(
        &self,
        mapping_id: MappingId,
        req: MappingRequest,
        now_ns: u64,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        self.update_mapping_locked(&mut state, mapping_id, req, now_ns, origin)
            .map(|_| ())
    }

    /// Update a mapping while the state write lock is held: mutate, write the
    /// recording marker, and emit exactly one [`StateEvent::MappingUpdated`].
    fn update_mapping_locked(
        &self,
        state: &mut ServerState,
        mapping_id: MappingId,
        req: MappingRequest,
        now_ns: u64,
        origin: Option<Uuid>,
    ) -> Result<MappingSummary, ServerError> {
        state
            .runtime
            .update_mapping(mapping_id, req, now_ns)
            .map_err(ServerError::Core)?;
        let mapping = state
            .runtime
            .snapshot()
            .mappings
            .get(&mapping_id)
            .cloned()
            .ok_or_else(|| ServerError::Runtime("updated mapping missing from runtime".into()))?;
        if let Some(recording) = state.active_recording.as_mut() {
            recording
                .writer
                .push_marker(RecordingMarker::MappingUpdated {
                    timestamp_ns: now_ns,
                    mapping_id,
                    source_device: mapping.source_device,
                    source_output: mapping.source_output.clone(),
                    target_scene: mapping.target_scene,
                    target_object: mapping.target_object,
                    target_attribute: mapping.target_attribute.clone(),
                    component_mask: mapping.component_mask.clone(),
                });
        }
        let summary = mapping_to_summary(&mapping);
        self.emit_event(
            state,
            origin,
            StateEvent::MappingUpdated {
                mapping: summary.clone(),
            },
        );
        Ok(summary)
    }

    pub async fn remove_mapping(&self, mapping_id: MappingId) -> Result<(), ServerError> {
        self.remove_mapping_from(mapping_id, Some(self.host_session_id))
            .await
    }

    pub async fn remove_mapping_from(
        &self,
        mapping_id: MappingId,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        self.remove_mapping_locked(&mut state, mapping_id, origin)
    }

    /// Remove a mapping while the state write lock is held: mutate, write the
    /// recording marker, and emit exactly one [`StateEvent::MappingRemoved`].
    fn remove_mapping_locked(
        &self,
        state: &mut ServerState,
        mapping_id: MappingId,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        state
            .runtime
            .remove_mapping(mapping_id)
            .map_err(ServerError::Core)?;
        if let Some(recording) = state.active_recording.as_mut() {
            recording
                .writer
                .push_marker(RecordingMarker::MappingRemoved {
                    timestamp_ns: now_ns(),
                    mapping_id,
                });
        }
        self.emit_event(state, origin, StateEvent::MappingRemoved { mapping_id });
        Ok(())
    }

    pub async fn set_mapping_lock(
        &self,
        mapping_id: MappingId,
        lock: bool,
    ) -> Result<(), ServerError> {
        self.set_mapping_lock_from(mapping_id, lock, Some(self.host_session_id))
            .await
    }

    pub async fn set_mapping_lock_from(
        &self,
        mapping_id: MappingId,
        lock: bool,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        self.set_mapping_lock_locked(&mut state, mapping_id, lock, origin)
    }

    /// Set a mapping's lock while the state write lock is held: mutate, write
    /// the recording marker, and emit exactly one
    /// [`StateEvent::MappingLockChanged`].
    fn set_mapping_lock_locked(
        &self,
        state: &mut ServerState,
        mapping_id: MappingId,
        lock: bool,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        state
            .runtime
            .set_mapping_lock(mapping_id, lock)
            .map_err(ServerError::Core)?;
        if let Some(recording) = state.active_recording.as_mut() {
            recording
                .writer
                .push_marker(RecordingMarker::MappingLockSet {
                    timestamp_ns: now_ns(),
                    mapping_id,
                    lock,
                });
        }
        self.emit_event(
            state,
            origin,
            StateEvent::MappingLockChanged { mapping_id, lock },
        );
        Ok(())
    }

    /// Wire operator-plane mapping create: resolve defaults (`source_device`
    /// = the actor's own device, `target_scene` = the active scene), enforce
    /// the ownership rule (own-source or Operator), then create under one
    /// lock acquisition. A denied or failed call mutates nothing and emits
    /// nothing; the lease model inside the runtime stays the arbiter of
    /// target-attribute contention.
    #[allow(clippy::too_many_arguments)]
    pub async fn wire_create_mapping(
        &self,
        actor: WireActor,
        source_device: Option<Uuid>,
        source_output: String,
        target_scene: Option<SceneId>,
        target_object: ObjectId,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
        now_ns: u64,
    ) -> Result<MappingSummary, ServerError> {
        let mut state = self.state.write().await;
        let source_device = source_device.unwrap_or(actor.device_id);
        if !actor.is_operator && source_device != actor.device_id {
            return Err(ServerError::Denied(
                "a session may only create mappings sourced from its own device \
                 (Operator role manages any mapping)"
                    .into(),
            ));
        }
        let target_scene = Self::resolve_scene_or_active(&state, target_scene)?;
        self.create_mapping_locked(
            &mut state,
            MappingRequest {
                source_device,
                source_output,
                target_scene,
                target_object,
                target_attribute,
                component_mask,
            },
            now_ns,
            Some(actor.session_id),
        )
    }

    /// Wire operator-plane mapping update (full replacement, mirroring the
    /// host API's [`ServerHandle::update_mapping`]). Non-operators may only
    /// update mappings whose current **and** requested `source_device` is
    /// their own device.
    #[allow(clippy::too_many_arguments)]
    pub async fn wire_update_mapping(
        &self,
        actor: WireActor,
        mapping_id: MappingId,
        source_device: Option<Uuid>,
        source_output: String,
        target_scene: Option<SceneId>,
        target_object: ObjectId,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
        now_ns: u64,
    ) -> Result<MappingSummary, ServerError> {
        let mut state = self.state.write().await;
        let existing_source = Self::mapping_source_device(&state, mapping_id)?;
        let source_device = source_device.unwrap_or(actor.device_id);
        if !actor.is_operator
            && (existing_source != actor.device_id || source_device != actor.device_id)
        {
            return Err(ServerError::Denied(
                "a session may only update mappings sourced from its own device \
                 (Operator role manages any mapping)"
                    .into(),
            ));
        }
        let target_scene = Self::resolve_scene_or_active(&state, target_scene)?;
        self.update_mapping_locked(
            &mut state,
            mapping_id,
            MappingRequest {
                source_device,
                source_output,
                target_scene,
                target_object,
                target_attribute,
                component_mask,
            },
            now_ns,
            Some(actor.session_id),
        )
    }

    /// Wire operator-plane mapping removal: own-source or Operator.
    pub async fn wire_remove_mapping(
        &self,
        actor: WireActor,
        mapping_id: MappingId,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let existing_source = Self::mapping_source_device(&state, mapping_id)?;
        if !actor.is_operator && existing_source != actor.device_id {
            return Err(ServerError::Denied(
                "a session may only remove mappings sourced from its own device \
                 (Operator role manages any mapping)"
                    .into(),
            ));
        }
        self.remove_mapping_locked(&mut state, mapping_id, Some(actor.session_id))
    }

    /// Wire operator-plane mapping lock toggle: own-source or Operator.
    pub async fn wire_set_mapping_lock(
        &self,
        actor: WireActor,
        mapping_id: MappingId,
        lock: bool,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let existing_source = Self::mapping_source_device(&state, mapping_id)?;
        if !actor.is_operator && existing_source != actor.device_id {
            return Err(ServerError::Denied(
                "a session may only lock mappings sourced from its own device \
                 (Operator role manages any mapping)"
                    .into(),
            ));
        }
        self.set_mapping_lock_locked(&mut state, mapping_id, lock, Some(actor.session_id))
    }

    fn mapping_source_device(
        state: &ServerState,
        mapping_id: MappingId,
    ) -> Result<Uuid, ServerError> {
        state
            .runtime
            .snapshot()
            .mappings
            .get(&mapping_id)
            .map(|mapping| mapping.source_device)
            .ok_or_else(|| {
                ServerError::Core(CoreError::MappingDenied(format!(
                    "mapping not found: {mapping_id}"
                )))
            })
    }

    pub async fn ingest_motion_samples(
        &self,
        device_id: Uuid,
        updates: Vec<AttributeUpdate>,
        timestamp_ns: u64,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state.metrics.motion_updates += updates.len() as u64;
        let _applied = state
            .runtime
            .apply_updates(device_id, &updates, timestamp_ns)
            .map_err(ServerError::Core)?;

        let maybe_recorded_frame = if state.active_recording.is_some() {
            let snapshot = state.runtime.snapshot();
            let mode = state.runtime.mode();
            let active_scene = snapshot.active_scene.ok_or_else(|| {
                ServerError::Core(CoreError::MappingDenied("no active scene".into()))
            })?;
            let mut attrs = Vec::new();
            for update in &updates {
                let mapping = snapshot
                    .mappings
                    .values()
                    .find(|m| {
                        m.source_device == device_id
                            && source_output_matches(
                                &m.source_output,
                                device_id,
                                update.output_attribute.as_str(),
                            )
                            && m.state == motionstage_core::MappingState::Active
                            && m.target_scene == active_scene
                    })
                    .ok_or_else(|| {
                        ServerError::Core(CoreError::MappingDenied(format!(
                            "no active mapping for output '{}'",
                            update.output_attribute
                        )))
                    })?;

                let resolved_value: AttributeValue = snapshot
                    .scenes
                    .get(&mapping.target_scene)
                    .and_then(|scene| scene.objects.get(&mapping.target_object))
                    .and_then(|object| object.attributes.get(&mapping.target_attribute))
                    .map(|attribute| attribute.current_value.clone())
                    .ok_or_else(|| {
                        ServerError::Core(CoreError::AttributeNotFound(
                            mapping.target_attribute.clone(),
                        ))
                    })?;

                attrs.push(RecordedAttribute {
                    object_id: mapping.target_object,
                    attribute: mapping.target_attribute.clone(),
                    value: resolved_value,
                });
            }

            Some(RecordedFrame {
                timestamp_ns,
                mode,
                attributes: attrs,
            })
        } else {
            None
        };

        if let Some(frame) = maybe_recorded_frame {
            if let Some(recording) = state.active_recording.as_mut() {
                recording.writer.push_frame(frame);
            }
        }

        Ok(())
    }

    pub async fn ingest_motion_datagram(
        &self,
        datagram: MotionDatagram,
    ) -> Result<(), ServerError> {
        {
            let mut state = self.state.write().await;
            state.metrics.motion_datagrams += 1;
        }
        debug!(
            device_id = %datagram.device_id,
            update_count = datagram.updates.len(),
            "ingest motion datagram"
        );
        let updates = datagram
            .updates
            .into_iter()
            .map(AttributeUpdate::from)
            .collect::<Vec<_>>();
        self.ingest_motion_samples(datagram.device_id, updates, datagram.timestamp_ns)
            .await
    }

    pub async fn start_recording(
        &self,
        path: impl AsRef<Path>,
        now_ns: u64,
    ) -> Result<Uuid, ServerError> {
        self.start_recording_from(path, now_ns, Some(self.host_session_id))
            .await
    }

    /// Wire take control: start recording a take with **server-assigned
    /// identity**. The recording path is generated inside the take-catalog
    /// directory — wire callers never supply paths. Routes through the same
    /// recording pipeline as the host API's `start_recording(path)`. Returns
    /// the take id (the recording id the catalog will register on stop).
    pub async fn start_take_from(
        &self,
        now_ns: u64,
        origin: Option<Uuid>,
    ) -> Result<Uuid, ServerError> {
        let take_dir = {
            let state = self.state.read().await;
            match state.config.take_catalog_path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => PathBuf::from("."),
            }
        };
        fs::create_dir_all(&take_dir).map_err(|err| ServerError::Recording(err.to_string()))?;
        let path = take_dir.join(format!("take-{}.cmtrk", Uuid::now_v7()));
        self.start_recording_from(path, now_ns, origin).await
    }

    /// Wire take control: stop the active recording and register it in the
    /// take catalog (the same flow as `stop_recording`), returning the
    /// registered take's catalog entry.
    pub async fn stop_take_from(&self, origin: Option<Uuid>) -> Result<TakeInfo, ServerError> {
        self.stop_recording_inner(origin).await.map(|(_, take)| take)
    }

    pub async fn start_recording_from(
        &self,
        path: impl AsRef<Path>,
        now_ns: u64,
        origin: Option<Uuid>,
    ) -> Result<Uuid, ServerError> {
        let mut state = self.state.write().await;
        // Validate every precondition before mutating anything: an error
        // return must leave runtime state untouched and emit no events.
        //
        // A recording already being active is a hard reject, not a silent
        // replace: overwriting `active_recording` would drop the in-progress
        // take's writer unflushed and destroy it. A retry or a second
        // operator's StartTake gets the typed error and changes nothing.
        if state.active_recording.is_some() {
            return Err(ServerError::AlreadyRecording);
        }
        let active_scene = state
            .runtime
            .snapshot()
            .active_scene
            .ok_or_else(|| ServerError::Recording("no active scene".into()))?;

        let initial_mode = state.runtime.mode();
        // Recording replaces any loaded playback; that discard is a
        // replicated mutation like every other playback-terminating path.
        if let Some(playback) = state.active_playback.take() {
            self.emit_event(
                &mut state,
                origin,
                StateEvent::PlaybackChanged {
                    state: PlaybackRuntimeState::Stopped,
                    take_id: playback.take_id,
                    playhead_ns: playback.playhead_ns,
                    looping: playback.looping,
                },
            );
        }
        let mut from_mode = state.runtime.mode();
        if from_mode.data_flow == DataFlowState::Idle {
            state
                .runtime
                .set_data_flow(DataFlowState::Live)
                .map_err(ServerError::Core)?;
            from_mode = state.runtime.mode();
        }
        state
            .runtime
            .set_recording(RecordingState::Recording)
            .map_err(ServerError::Core)?;

        let writer = RecordingWriter::start(active_scene, now_ns);
        let recording_id = writer.recording_id();
        let snapshot = state.runtime.snapshot();
        state.active_recording = Some(ActiveRecording {
            path: path.as_ref().to_path_buf(),
            writer,
        });

        if let Some(recording) = state.active_recording.as_mut() {
            recording
                .writer
                .push_marker(RecordingMarker::ModeTransition {
                    timestamp_ns: now_ns,
                    from: from_mode,
                    to: Mode::RECORDING,
                });

            for mapping in snapshot.mappings.values() {
                if mapping.state == motionstage_core::MappingState::Active {
                    recording
                        .writer
                        .push_marker(RecordingMarker::MappingCreated {
                            timestamp_ns: now_ns,
                            mapping_id: mapping.id,
                            source_device: mapping.source_device,
                            source_output: mapping.source_output.clone(),
                            target_scene: mapping.target_scene,
                            target_object: mapping.target_object,
                            target_attribute: mapping.target_attribute.clone(),
                            component_mask: mapping.component_mask.clone(),
                        });
                }
            }
        }

        if initial_mode != Mode::RECORDING {
            self.emit_event(
                &mut state,
                origin,
                StateEvent::ModeChanged {
                    mode: Mode::RECORDING,
                },
            );
        }
        self.emit_event(
            &mut state,
            origin,
            StateEvent::RecordingStarted {
                take_id: recording_id,
                scene_id: active_scene,
            },
        );

        Ok(recording_id)
    }

    pub async fn stop_recording(&self) -> Result<RecordingManifest, ServerError> {
        self.stop_recording_from(Some(self.host_session_id)).await
    }

    pub async fn stop_recording_from(
        &self,
        origin: Option<Uuid>,
    ) -> Result<RecordingManifest, ServerError> {
        self.stop_recording_inner(origin)
            .await
            .map(|(manifest, _)| manifest)
    }

    /// Shared stop-recording flow: finish the writer, register the take in
    /// the catalog, and emit `RecordingStopped` + `TakeRegistered` +
    /// `ModeChanged`. Returns both the manifest (host API) and the registered
    /// take (wire take control).
    async fn stop_recording_inner(
        &self,
        origin: Option<Uuid>,
    ) -> Result<(RecordingManifest, TakeInfo), ServerError> {
        let mut state = self.state.write().await;
        let Some(mut recording) = state.active_recording.take() else {
            return Err(ServerError::Recording("no active recording".into()));
        };
        let recording_path = recording.path.clone();

        recording
            .writer
            .push_marker(RecordingMarker::ModeTransition {
                timestamp_ns: now_ns(),
                from: Mode::RECORDING,
                to: Mode::LIVE,
            });

        let manifest = recording
            .writer
            .finish(&recording_path)
            .map_err(|err| ServerError::Recording(err.to_string()))?;

        let entry = state
            .take_catalog
            .register_take(
                manifest.recording_id,
                manifest.scene_id,
                recording_path,
                manifest.started_ns,
                manifest.frame_count,
            )
            .map_err(ServerError::Take)?;
        let take_info = entry.to_take_info();

        self.emit_event(
            &mut state,
            origin,
            StateEvent::RecordingStopped {
                take_id: manifest.recording_id,
                frame_count: manifest.frame_count,
            },
        );
        self.emit_event(
            &mut state,
            origin,
            StateEvent::TakeRegistered {
                take: take_info.clone(),
            },
        );

        state
            .runtime
            .set_recording(RecordingState::Inactive)
            .map_err(ServerError::Core)?;
        let mode = state.runtime.mode();
        self.emit_event(&mut state, origin, StateEvent::ModeChanged { mode });

        Ok((manifest, take_info))
    }

    pub async fn list_takes(&self, scene_id: Option<SceneId>) -> Vec<TakeInfo> {
        let state = self.state.read().await;
        state.take_catalog.list(scene_id)
    }

    pub async fn select_take(&self, take_id: Uuid) -> Result<TakeInfo, ServerError> {
        self.select_take_from(take_id, Some(self.host_session_id))
            .await
    }

    pub async fn select_take_from(
        &self,
        take_id: Uuid,
        origin: Option<Uuid>,
    ) -> Result<TakeInfo, ServerError> {
        let mut state = self.state.write().await;
        let info = state
            .take_catalog
            .select_take(take_id)
            .map_err(ServerError::Take)?;
        self.emit_event(
            &mut state,
            origin,
            StateEvent::TakeSelected {
                take_id: info.take_id,
                scene_id: info.scene_id,
            },
        );
        Ok(info)
    }

    pub async fn playback_play(
        &self,
        take_id: Uuid,
        looping: bool,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        self.playback_play_from(take_id, looping, Some(self.host_session_id))
            .await
    }

    pub async fn playback_play_from(
        &self,
        take_id: Uuid,
        looping: bool,
        origin: Option<Uuid>,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        let mut state = self.state.write().await;
        let take = state
            .take_catalog
            .get(take_id)
            .cloned()
            .ok_or_else(|| ServerError::Take(format!("take not found: {take_id}")))?;
        let recording =
            read_recording(&take.path).map_err(|err| ServerError::Take(err.to_string()))?;
        let previous_scene = state.runtime.active_scene();
        let previous_mode = state.runtime.mode();
        state
            .runtime
            .set_active_scene(recording.manifest.scene_id)
            .map_err(ServerError::Core)?;
        state
            .runtime
            .set_data_flow(DataFlowState::Live)
            .map_err(ServerError::Core)?;
        state
            .runtime
            .set_recording(RecordingState::Playback)
            .map_err(ServerError::Core)?;
        let playback = ActivePlayback {
            take_id,
            recording,
            state: PlaybackRuntimeState::Playing,
            looping,
            playhead_ns: 0,
            started_wall_ns: Some(now_ns()),
            started_playhead_ns: 0,
        };
        if let Some(frame) = ServerState::frame_for_playhead(&playback.recording, 0) {
            state.apply_playback_frame(&frame, playback.recording.manifest.scene_id);
        }
        let scene_id = playback.recording.manifest.scene_id;
        let playhead_ns = playback.playhead_ns;
        let looping = playback.looping;
        state.active_playback = Some(playback);

        if previous_scene != Some(scene_id) {
            self.emit_event(&mut state, origin, StateEvent::SceneActivated { scene_id });
        }
        let mode = state.runtime.mode();
        if previous_mode != mode {
            self.emit_event(&mut state, origin, StateEvent::ModeChanged { mode });
        }
        self.emit_event(
            &mut state,
            origin,
            StateEvent::PlaybackChanged {
                state: PlaybackRuntimeState::Playing,
                take_id,
                playhead_ns,
                looping,
            },
        );
        Ok((PlaybackRuntimeState::Playing, playhead_ns, looping))
    }

    pub async fn playback_pause(
        &self,
        take_id: Uuid,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        self.playback_pause_from(take_id, Some(self.host_session_id))
            .await
    }

    pub async fn playback_pause_from(
        &self,
        take_id: Uuid,
        origin: Option<Uuid>,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        let mut state = self.state.write().await;
        let Some(playback) = state.active_playback.as_mut() else {
            return Err(ServerError::Take("no active playback".into()));
        };
        if playback.take_id != take_id {
            return Err(ServerError::Take(format!(
                "active playback is not take {take_id}"
            )));
        }
        if playback.state == PlaybackRuntimeState::Playing {
            let started = playback.started_wall_ns.unwrap_or_else(now_ns);
            let elapsed = now_ns().saturating_sub(started);
            playback.playhead_ns = playback.started_playhead_ns.saturating_add(elapsed);
        }
        playback.state = PlaybackRuntimeState::Paused;
        playback.started_wall_ns = None;
        playback.started_playhead_ns = playback.playhead_ns;
        let (playback_state, playhead_ns, looping) =
            (playback.state, playback.playhead_ns, playback.looping);
        self.emit_event(
            &mut state,
            origin,
            StateEvent::PlaybackChanged {
                state: playback_state,
                take_id,
                playhead_ns,
                looping,
            },
        );
        Ok((playback_state, playhead_ns, looping))
    }

    pub async fn playback_seek(
        &self,
        take_id: Uuid,
        seek_ns: u64,
        looping: bool,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        self.playback_seek_from(take_id, seek_ns, looping, Some(self.host_session_id))
            .await
    }

    pub async fn playback_seek_from(
        &self,
        take_id: Uuid,
        seek_ns: u64,
        looping: bool,
        origin: Option<Uuid>,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        let mut state = self.state.write().await;
        let (status, playhead, loop_state, scene_id, frame) = {
            let Some(playback) = state.active_playback.as_mut() else {
                return Err(ServerError::Take("no active playback".into()));
            };
            if playback.take_id != take_id {
                return Err(ServerError::Take(format!(
                    "active playback is not take {take_id}"
                )));
            }
            let duration = ServerState::playback_duration_ns(&playback.recording);
            let seek = if duration == 0 {
                0
            } else if looping {
                seek_ns % duration
            } else {
                seek_ns.min(duration)
            };
            playback.looping = looping;
            playback.playhead_ns = seek;
            playback.started_wall_ns = Some(now_ns());
            playback.started_playhead_ns = seek;
            let scene_id = playback.recording.manifest.scene_id;
            let frame = ServerState::frame_for_playhead(&playback.recording, seek);
            (
                playback.state,
                playback.playhead_ns,
                playback.looping,
                scene_id,
                frame,
            )
        };
        if let Some(frame) = frame {
            state.apply_playback_frame(&frame, scene_id);
        }
        self.emit_event(
            &mut state,
            origin,
            StateEvent::PlaybackChanged {
                state: status,
                take_id,
                playhead_ns: playhead,
                looping: loop_state,
            },
        );
        Ok((status, playhead, loop_state))
    }

    pub async fn playback_stop(
        &self,
        take_id: Uuid,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        self.playback_stop_from(take_id, Some(self.host_session_id))
            .await
    }

    pub async fn playback_stop_from(
        &self,
        take_id: Uuid,
        origin: Option<Uuid>,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        let mut state = self.state.write().await;
        let Some(playback) = state.active_playback.take() else {
            return Err(ServerError::Take("no active playback".into()));
        };
        if playback.take_id != take_id {
            state.active_playback = Some(playback);
            return Err(ServerError::Take(format!(
                "active playback is not take {take_id}"
            )));
        }
        state
            .runtime
            .set_recording(RecordingState::Inactive)
            .map_err(ServerError::Core)?;
        let mode = state.runtime.mode();
        self.emit_event(&mut state, origin, StateEvent::ModeChanged { mode });
        self.emit_event(
            &mut state,
            origin,
            StateEvent::PlaybackChanged {
                state: PlaybackRuntimeState::Stopped,
                take_id,
                playhead_ns: playback.playhead_ns,
                looping: playback.looping,
            },
        );
        Ok((
            PlaybackRuntimeState::Stopped,
            playback.playhead_ns,
            playback.looping,
        ))
    }

    pub async fn open_take_bake_cursor(
        &self,
        take_id: Uuid,
        sampling_mode: SamplingMode,
    ) -> Result<(Uuid, u64), ServerError> {
        let mut state = self.state.write().await;
        let take = state
            .take_catalog
            .get(take_id)
            .cloned()
            .ok_or_else(|| ServerError::Take(format!("take not found: {take_id}")))?;
        let recording =
            read_recording(&take.path).map_err(|err| ServerError::Take(err.to_string()))?;
        let total_frames = take_bake_total_frames(&recording, sampling_mode);
        let cursor_id = Uuid::now_v7();
        state.bake_cursors.insert(
            cursor_id,
            TakeBakeCursor {
                take_id,
                sampling_mode,
                recording,
                next_index: 0,
                total_frames,
            },
        );
        Ok((cursor_id, total_frames))
    }

    pub async fn read_take_bake_frame(
        &self,
        cursor_id: Uuid,
    ) -> Result<Option<(u64, u64, Vec<TakeBakeAttribute>)>, ServerError> {
        let mut state = self.state.write().await;
        let Some(cursor) = state.bake_cursors.get_mut(&cursor_id) else {
            return Err(ServerError::Take(format!(
                "unknown bake cursor: {cursor_id}"
            )));
        };
        let frame_index = cursor.next_index;
        let Some((timestamp_ns, attributes)) = take_bake_frame_for_index(cursor, frame_index)
        else {
            return Ok(None);
        };
        cursor.next_index = cursor.next_index.saturating_add(1);
        Ok(Some((frame_index, timestamp_ns, attributes)))
    }

    pub async fn seek_take_bake_frame(
        &self,
        cursor_id: Uuid,
        frame_index: u64,
    ) -> Result<Option<(u64, u64, Vec<TakeBakeAttribute>)>, ServerError> {
        let mut state = self.state.write().await;
        let Some(cursor) = state.bake_cursors.get_mut(&cursor_id) else {
            return Err(ServerError::Take(format!(
                "unknown bake cursor: {cursor_id}"
            )));
        };
        cursor.next_index = frame_index.saturating_add(1);
        Ok(take_bake_frame_for_index(cursor, frame_index)
            .map(|(timestamp_ns, attributes)| (frame_index, timestamp_ns, attributes)))
    }

    pub async fn close_take_bake_cursor(&self, cursor_id: Uuid) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let _ = state.bake_cursors.remove(&cursor_id);
        Ok(())
    }

    pub async fn delete_take(&self, take_id: Uuid) -> Result<(), ServerError> {
        self.delete_take_from(take_id, Some(self.host_session_id))
            .await
    }

    pub async fn delete_take_from(
        &self,
        take_id: Uuid,
        origin: Option<Uuid>,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let path = state
            .take_catalog
            .mark_deleted(take_id)
            .map_err(ServerError::Take)?;

        // The catalog mutation is persisted (deleted=true) at this point:
        // replicate it before any fallible filesystem work so the event
        // stream can never miss the catalog change.
        self.emit_event(&mut state, origin, StateEvent::TakeDeleted { take_id });

        if let Some(active) =
            state.active_playback.take_if(|active| active.take_id == take_id)
        {
            state
                .runtime
                .set_recording(RecordingState::Inactive)
                .map_err(ServerError::Core)?;
            let mode = state.runtime.mode();
            self.emit_event(&mut state, origin, StateEvent::ModeChanged { mode });
            self.emit_event(
                &mut state,
                origin,
                StateEvent::PlaybackChanged {
                    state: PlaybackRuntimeState::Stopped,
                    take_id,
                    playhead_ns: active.playhead_ns,
                    looping: active.looping,
                },
            );
        }

        state
            .bake_cursors
            .retain(|_, cursor| cursor.take_id != take_id);

        if let Some(path) = path {
            if let Err(err) = fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    // Policy: surface the filesystem error to the caller. The
                    // take stays tombstoned (deleted=true, TakeDeleted already
                    // emitted), so replicas remain consistent; only the orphan
                    // recording file is left behind for manual cleanup.
                    return Err(ServerError::Take(err.to_string()));
                }
            }
            state
                .take_catalog
                .purge_take(take_id)
                .map_err(ServerError::Take)?;
        }
        Ok(())
    }

    pub async fn set_master_video_descriptor(
        &self,
        descriptor: VideoStreamDescriptor,
    ) -> Result<(), ServerError> {
        descriptor
            .validate()
            .map_err(|err| ServerError::Video(err.to_string()))?;

        let mut state = self.state.write().await;
        state.master_video_descriptor = Some(descriptor);
        Ok(())
    }

    pub async fn negotiate_video_for_client(
        &self,
        capability: VideoClientCapability,
    ) -> Result<NegotiatedVideoStream, ServerError> {
        let state = self.state.read().await;
        let descriptor = state
            .master_video_descriptor
            .as_ref()
            .ok_or_else(|| ServerError::Video("master video descriptor not set".into()))?;

        negotiate_stream(descriptor, capability).map_err(|err| ServerError::Video(err.to_string()))
    }

    pub async fn create_video_offer(
        &self,
        device_id: Uuid,
        stream_id: &str,
        track_id: &str,
    ) -> Result<SdpMessage, ServerError> {
        self.ensure_video_session_ready(device_id).await?;
        let peer = self.ensure_video_peer(device_id).await?;

        let needs_track = {
            let state = self.state.read().await;
            state
                .video_peers
                .get(&device_id)
                .map(|entry| !entry.track_added)
                .unwrap_or(true)
        };
        if needs_track {
            let codec = {
                let state = self.state.read().await;
                state
                    .master_video_descriptor
                    .as_ref()
                    .map(|d| d.codec)
                    .unwrap_or(VideoCodec::H264)
            };
            peer.add_video_track(codec, stream_id, track_id)
                .await
                .map_err(|err| ServerError::WebRtc(err.to_string()))?;
            let mut state = self.state.write().await;
            if let Some(entry) = state.video_peers.get_mut(&device_id) {
                entry.track_added = true;
            }
            state.video_keyframe_needed = true;
        }

        peer.create_offer()
            .await
            .map_err(|err| ServerError::WebRtc(err.to_string()))
    }

    pub async fn handle_video_signal(
        &self,
        device_id: Uuid,
        payload: SignalPayload,
    ) -> Result<Option<SdpMessage>, ServerError> {
        self.ensure_video_session_ready(device_id).await?;

        match payload {
            SignalPayload::Sdp(sdp) if sdp.ty == SdpType::Offer => {
                let peer = self.ensure_video_peer(device_id).await?;
                peer.apply_remote_sdp(sdp)
                    .await
                    .map_err(|err| ServerError::WebRtc(err.to_string()))?;

                let needs_track = {
                    let state = self.state.read().await;
                    state
                        .video_peers
                        .get(&device_id)
                        .map(|entry| !entry.track_added)
                        .unwrap_or(true)
                };
                if needs_track {
                    let stream_id = format!("motionstage-{device_id}");
                    let codec = {
                        let state = self.state.read().await;
                        state
                            .master_video_descriptor
                            .as_ref()
                            .map(|d| d.codec)
                            .unwrap_or(VideoCodec::H264)
                    };
                    peer.add_video_track(codec, &stream_id, "video")
                        .await
                        .map_err(|err| ServerError::WebRtc(err.to_string()))?;
                    let mut state = self.state.write().await;
                    if let Some(entry) = state.video_peers.get_mut(&device_id) {
                        entry.track_added = true;
                    }
                    state.video_keyframe_needed = true;
                }

                let answer = peer
                    .create_answer()
                    .await
                    .map_err(|err| ServerError::WebRtc(err.to_string()))?;
                Ok(Some(answer))
            }
            SignalPayload::Sdp(sdp) => {
                let peer = self.video_peer(device_id).await?;
                peer.apply_remote_sdp(sdp)
                    .await
                    .map_err(|err| ServerError::WebRtc(err.to_string()))?;
                Ok(None)
            }
            SignalPayload::Ice(candidate) => {
                let peer = self.video_peer(device_id).await?;
                peer.add_ice_candidate(candidate)
                    .await
                    .map_err(|err| ServerError::WebRtc(err.to_string()))?;
                Ok(None)
            }
        }
    }

    pub async fn push_video_frame(
        &self,
        data: Bytes,
        duration: Duration,
    ) -> Result<(), ServerError> {
        let peers_with_tracks: Vec<Arc<WebRtcSession>> = {
            let mut state = self.state.write().await;
            state.last_video_frame_ns = Some(now_ns());
            state
                .video_peers
                .values()
                .filter(|entry| entry.track_added)
                .map(|entry| Arc::clone(&entry.peer))
                .collect()
        };

        for peer in peers_with_tracks {
            if let Err(err) = peer.write_sample(data.clone(), duration).await {
                tracing::warn!("failed to write video sample to peer: {err}");
            }
        }
        Ok(())
    }

    /// Returns `true` (once) if a new video peer was added since the last call.
    /// The caller should force an IDR keyframe so the new peer can start decoding.
    pub async fn take_keyframe_needed(&self) -> bool {
        let mut state = self.state.write().await;
        let needed = state.video_keyframe_needed;
        state.video_keyframe_needed = false;
        needed
    }

    pub async fn video_stream_status(&self) -> VideoStreamStatus {
        let state = self.state.read().await;
        let descriptor_set = state.master_video_descriptor.is_some();
        let peer_count = state
            .video_peers
            .values()
            .filter(|entry| entry.track_added)
            .count() as u32;
        let now = now_ns();
        let last_frame_age_ms = state
            .last_video_frame_ns
            .map(|last| now.saturating_sub(last) / 1_000_000);
        let recent_frame = state
            .last_video_frame_ns
            .map(|last| now.saturating_sub(last) <= VIDEO_STREAM_ACTIVITY_WINDOW_NS)
            .unwrap_or(false);

        VideoStreamStatus {
            available: descriptor_set && recent_frame,
            descriptor_set,
            peer_count,
            last_frame_age_ms,
        }
    }

    pub async fn video_peer_count(&self) -> u32 {
        let state = self.state.read().await;
        state
            .video_peers
            .values()
            .filter(|entry| entry.track_added)
            .count() as u32
    }

    pub async fn has_video_session(&self, device_id: Uuid) -> bool {
        let state = self.state.read().await;
        state.video_peers.contains_key(&device_id)
    }

    pub async fn session_info(&self, device_id: Uuid) -> Option<SessionInfo> {
        let state = self.state.read().await;
        state.sessions.get(&device_id).cloned()
    }

    /// Resolve the authoritative operator-plane actor for a live session from
    /// the **server's own session record** — never from identity re-sent on
    /// the wire per request. `device_id` here is the connection's registered
    /// device; the returned [`WireActor`]'s `device_id` and `is_operator` are
    /// read from the stored [`SessionInfo`], whose roles were fixed by the
    /// admission policy at registration. This is the security boundary for
    /// findings like a peer re-declaring `roles:[Operator]` or naming a
    /// victim's `device_id` in a request: permission checks must consult this,
    /// not per-message-supplied fields. See the Security Model in
    /// docs/design-architecture.md.
    pub async fn resolve_wire_actor(&self, device_id: Uuid) -> Result<WireActor, ServerError> {
        let state = self.state.read().await;
        let session = state
            .sessions
            .get(&device_id)
            .filter(|session| session.state != SessionState::Closed)
            .ok_or(ServerError::SessionNotFound(device_id))?;
        let session_id = session
            .session_id
            .ok_or(ServerError::SessionNotFound(device_id))?;
        Ok(WireActor {
            session_id,
            device_id: session.device_id,
            is_operator: session.roles.contains(&ClientRole::Operator),
        })
    }

    /// Whether the live session for `device_id` holds [`ClientRole::Operator`]
    /// per the **server session record** (admitted roles), not per any
    /// role list re-sent on the wire.
    pub async fn session_is_operator(&self, device_id: Uuid) -> bool {
        let state = self.state.read().await;
        state
            .sessions
            .get(&device_id)
            .filter(|session| session.state != SessionState::Closed)
            .map(|session| session.roles.contains(&ClientRole::Operator))
            .unwrap_or(false)
    }

    pub async fn sessions(&self) -> Vec<SessionInfo> {
        let state = self.state.read().await;
        state.sessions.values().cloned().collect()
    }

    pub async fn push_signaling_message(&self, message: SignalMessage) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let from = state
            .sessions
            .get(&message.from_device)
            .ok_or(ServerError::SessionNotFound(message.from_device))?;
        let to = state
            .sessions
            .get(&message.to_device)
            .ok_or(ServerError::SessionNotFound(message.to_device))?;
        if from.state != SessionState::Active || to.state != SessionState::Active {
            return Err(ServerError::Signaling(
                "both signaling peers must be active".into(),
            ));
        }

        let from_device = message.from_device;
        let to_device = message.to_device;
        state.signaling.enqueue(message);
        state.metrics.signaling_messages += 1;
        debug!(%from_device, %to_device, "queued signaling message");
        Ok(())
    }

    pub async fn drain_signaling_messages(
        &self,
        device_id: Uuid,
    ) -> Result<Vec<SignalMessage>, ServerError> {
        let mut state = self.state.write().await;
        if !state.sessions.contains_key(&device_id) {
            return Err(ServerError::SessionNotFound(device_id));
        }
        Ok(state.signaling.drain_for(device_id))
    }

    async fn ensure_video_session_ready(&self, device_id: Uuid) -> Result<(), ServerError> {
        let state = self.state.read().await;
        let Some(session) = state.sessions.get(&device_id) else {
            return Err(ServerError::SessionNotFound(device_id));
        };
        if session.state != SessionState::Active {
            return Err(ServerError::Video(format!(
                "device {device_id} is not active for video session"
            )));
        }
        if !session.features.contains(&Feature::Video) {
            return Err(ServerError::Video(format!(
                "device {device_id} did not negotiate video feature"
            )));
        }
        if state.master_video_descriptor.is_none() {
            return Err(ServerError::Video("master video descriptor not set".into()));
        }
        Ok(())
    }

    async fn ensure_video_peer(&self, device_id: Uuid) -> Result<Arc<WebRtcSession>, ServerError> {
        if let Some(existing) = {
            let state = self.state.read().await;
            state
                .video_peers
                .get(&device_id)
                .map(|entry| Arc::clone(&entry.peer))
        } {
            return Ok(existing);
        }

        let created = Arc::new(
            WebRtcSession::new()
                .await
                .map_err(|err| ServerError::WebRtc(err.to_string()))?,
        );
        let mut state = self.state.write().await;
        let entry = state
            .video_peers
            .entry(device_id)
            .or_insert_with(|| VideoPeerSession {
                peer: Arc::clone(&created),
                track_added: false,
            });
        Ok(Arc::clone(&entry.peer))
    }

    async fn video_peer(&self, device_id: Uuid) -> Result<Arc<WebRtcSession>, ServerError> {
        let state = self.state.read().await;
        state
            .video_peers
            .get(&device_id)
            .map(|entry| Arc::clone(&entry.peer))
            .ok_or_else(|| {
                ServerError::Video(format!("no video peer exists for device {device_id}"))
            })
    }
}

// Break = exit session loop; Continue = keep looping
type HandlerOutcome = ControlFlow<()>;

async fn handle_set_data_flow(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    session_id: Uuid,
    state: DataFlowState,
) -> Result<HandlerOutcome, ServerError> {
    // Operator gate from the server session record, not self-declared roles.
    if !server.session_is_operator(client_hello.device_id).await {
        if send_protocol_error(
            control,
            RejectCode::RoleDenied,
            "operator role is required to change mode".into(),
        )
        .await
        .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
        return Ok(ControlFlow::Continue(()));
    }
    // Success has no direct reply: the caller observes its own
    // StateEventMsg(ModeChanged) echo like every other session.
    if let Err(err) = server.set_data_flow_from(state, Some(session_id)).await {
        if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
            .await
            .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

async fn handle_set_recording(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    session_id: Uuid,
    state: RecordingState,
) -> Result<HandlerOutcome, ServerError> {
    // Operator gate from the server session record, not self-declared roles.
    if !server.session_is_operator(client_hello.device_id).await {
        if send_protocol_error(
            control,
            RejectCode::RoleDenied,
            "operator role is required to change mode".into(),
        )
        .await
        .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
        return Ok(ControlFlow::Continue(()));
    }
    // Success has no direct reply: the caller observes its own
    // StateEventMsg(ModeChanged) echo like every other session.
    if let Err(err) = server.set_recording_from(state, Some(session_id)).await {
        if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
            .await
            .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

async fn handle_baseline_control(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    session_id: Uuid,
    msg: ControlMessage,
) -> Result<HandlerOutcome, ServerError> {
    let reject_reason = match &msg {
        ControlMessage::ResetSceneToBaseline { .. } => {
            "operator role is required to reset baseline"
        }
        ControlMessage::CommitSceneBaseline { .. } => {
            "operator role is required to commit scene baseline"
        }
        ControlMessage::CommitObjectBaseline { .. } => {
            "operator role is required to commit object baseline"
        }
        _ => unreachable!(),
    };
    // Operator gate from the server session record, not self-declared roles.
    if !server.session_is_operator(client_hello.device_id).await {
        if send_protocol_error(control, RejectCode::RoleDenied, reject_reason.into())
            .await
            .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
        return Ok(ControlFlow::Continue(()));
    }
    // Success has no direct reply (BaselineActionApplied is retired): the
    // caller observes its own StateEventMsg(BaselineApplied) echo, which
    // carries the same changed_attributes count.
    let result: Result<u32, ServerError> = match msg {
        ControlMessage::ResetSceneToBaseline { scene_id } => {
            server
                .reset_scene_to_baseline_from(scene_id, Some(session_id))
                .await
        }
        ControlMessage::CommitSceneBaseline { scene_id } => {
            server
                .commit_scene_baseline_from(scene_id, Some(session_id))
                .await
        }
        ControlMessage::CommitObjectBaseline {
            scene_id,
            object_id,
        } => {
            server
                .commit_object_baseline_from(scene_id, object_id, Some(session_id))
                .await
        }
        _ => unreachable!(),
    };
    if let Err(err) = result {
        if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
            .await
            .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// Convert a server error into the typed wire error carried inside
/// operator-plane result messages.
fn wire_error_from(err: &ServerError) -> WireError {
    WireError {
        code: map_server_error_to_reject(err),
        reason: err.to_string(),
    }
}

/// Operator-plane mapping ops (`CreateMapping` / `UpdateMapping` /
/// `RemoveMapping` / `SetMappingLock`). Permission enforcement (own-source or
/// Operator) happens inside the `wire_*` server methods under one lock, so a
/// denial mutates nothing and emits nothing; this handler only shapes the
/// typed result reply.
async fn handle_mapping_ops(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    session_id: Uuid,
    msg: ControlMessage,
) -> Result<HandlerOutcome, ServerError> {
    // Identity is taken from the server's session record, NOT from the
    // per-connection ClientHello (which carries self-declared roles/device).
    // `session_id` and `client_hello.device_id` are the connection's registered
    // identity; resolve the authoritative actor (device + admitted roles) from
    // the stored SessionInfo so a peer cannot escalate by re-declaring roles.
    let actor = match server.resolve_wire_actor(client_hello.device_id).await {
        Ok(actor) => actor,
        Err(err) => {
            let wire = wire_error_from(&err);
            let reply = match msg {
                ControlMessage::RemoveMapping { mapping_id }
                | ControlMessage::SetMappingLock { mapping_id, .. } => {
                    ControlMessage::MappingOpResult {
                        mapping_id,
                        result: Err(wire),
                    }
                }
                _ => ControlMessage::MappingCreateResult { result: Err(wire) },
            };
            if control.send(&reply).await.is_err() {
                let _ = server.close_session(client_hello.device_id, now_ns()).await;
                return Ok(ControlFlow::Break(()));
            }
            return Ok(ControlFlow::Continue(()));
        }
    };
    debug_assert_eq!(actor.session_id, session_id);
    let reply = match msg {
        ControlMessage::CreateMapping {
            source_device,
            source_output,
            target_scene,
            target_object,
            target_attribute,
            component_mask,
        } => {
            let result = server
                .wire_create_mapping(
                    actor,
                    source_device,
                    source_output,
                    target_scene,
                    target_object,
                    target_attribute,
                    component_mask,
                    now_ns(),
                )
                .await;
            ControlMessage::MappingCreateResult {
                result: result.map_err(|err| wire_error_from(&err)),
            }
        }
        ControlMessage::UpdateMapping {
            mapping_id,
            source_device,
            source_output,
            target_scene,
            target_object,
            target_attribute,
            component_mask,
        } => {
            let result = server
                .wire_update_mapping(
                    actor,
                    mapping_id,
                    source_device,
                    source_output,
                    target_scene,
                    target_object,
                    target_attribute,
                    component_mask,
                    now_ns(),
                )
                .await;
            ControlMessage::MappingCreateResult {
                result: result.map_err(|err| wire_error_from(&err)),
            }
        }
        ControlMessage::RemoveMapping { mapping_id } => {
            let result = server.wire_remove_mapping(actor, mapping_id).await;
            ControlMessage::MappingOpResult {
                mapping_id,
                result: result.map_err(|err| wire_error_from(&err)),
            }
        }
        ControlMessage::SetMappingLock { mapping_id, lock } => {
            let result = server.wire_set_mapping_lock(actor, mapping_id, lock).await;
            ControlMessage::MappingOpResult {
                mapping_id,
                result: result.map_err(|err| wire_error_from(&err)),
            }
        }
        _ => unreachable!(),
    };
    if control.send(&reply).await.is_err() {
        let _ = server.close_session(client_hello.device_id, now_ns()).await;
        return Ok(ControlFlow::Break(()));
    }
    Ok(ControlFlow::Continue(()))
}

/// Wire take control (`StartTake` / `StopTake`), Operator-gated. Take
/// identity is server-assigned; the reply carries the take id (start) or the
/// registered catalog entry (stop).
async fn handle_take_control(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    session_id: Uuid,
    msg: ControlMessage,
) -> Result<HandlerOutcome, ServerError> {
    // Operator gate reads the admitted roles from the server session record,
    // not the self-declared ClientHello roles.
    let is_operator = server.session_is_operator(client_hello.device_id).await;
    let reply = match msg {
        ControlMessage::StartTake => {
            let result = if is_operator {
                server.start_take_from(now_ns(), Some(session_id)).await
            } else {
                Err(ServerError::Denied(
                    "operator role is required to start a take".into(),
                ))
            };
            ControlMessage::TakeStartResult {
                result: result.map_err(|err| wire_error_from(&err)),
            }
        }
        ControlMessage::StopTake => {
            let result = if is_operator {
                server.stop_take_from(Some(session_id)).await
            } else {
                Err(ServerError::Denied(
                    "operator role is required to stop a take".into(),
                ))
            };
            ControlMessage::TakeStopResult {
                result: result.map_err(|err| wire_error_from(&err)),
            }
        }
        _ => unreachable!(),
    };
    if control.send(&reply).await.is_err() {
        let _ = server.close_session(client_hello.device_id, now_ns()).await;
        return Ok(ControlFlow::Break(()));
    }
    Ok(ControlFlow::Continue(()))
}

async fn handle_take_management(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    session_id: Uuid,
    msg: ControlMessage,
) -> Result<HandlerOutcome, ServerError> {
    let reject_reason = match &msg {
        ControlMessage::ListTakes { .. } => "operator role is required to list takes",
        ControlMessage::SelectTake { .. } => "operator role is required to select takes",
        ControlMessage::DeleteTake { .. } => "operator role is required to delete takes",
        _ => unreachable!(),
    };
    // Operator gate from the server session record, not self-declared roles.
    if !server.session_is_operator(client_hello.device_id).await {
        if send_protocol_error(control, RejectCode::RoleDenied, reject_reason.into())
            .await
            .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
        return Ok(ControlFlow::Continue(()));
    }
    match msg {
        ControlMessage::ListTakes { scene_id } => {
            let takes = server.list_takes(scene_id).await;
            control
                .send(&ControlMessage::TakeList { takes })
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
        }
        ControlMessage::SelectTake { take_id } => match server
            .select_take_from(take_id, Some(session_id))
            .await
        {
            Ok(selected) => {
                control
                    .send(&ControlMessage::TakeSelected {
                        take_id: selected.take_id,
                    })
                    .await
                    .map_err(|err| ServerError::Runtime(err.to_string()))?;
            }
            Err(err) => {
                if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
                    .await
                    .is_err()
                {
                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                    return Ok(ControlFlow::Break(()));
                }
            }
        },
        ControlMessage::DeleteTake { take_id } => match server
            .delete_take_from(take_id, Some(session_id))
            .await
        {
            Ok(()) => {
                control
                    .send(&ControlMessage::TakeDeleted { take_id })
                    .await
                    .map_err(|err| ServerError::Runtime(err.to_string()))?;
            }
            Err(err) => {
                if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
                    .await
                    .is_err()
                {
                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                    return Ok(ControlFlow::Break(()));
                }
            }
        },
        _ => unreachable!(),
    }
    Ok(ControlFlow::Continue(()))
}

#[allow(clippy::too_many_arguments)]
async fn handle_playback_control(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    session_id: Uuid,
    take_id: Uuid,
    action: PlaybackAction,
    seek_ns: Option<u64>,
    looping: bool,
) -> Result<HandlerOutcome, ServerError> {
    // Operator gate from the server session record, not self-declared roles.
    if !server.session_is_operator(client_hello.device_id).await {
        if send_protocol_error(
            control,
            RejectCode::RoleDenied,
            "operator role is required to control playback".into(),
        )
        .await
        .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
        return Ok(ControlFlow::Continue(()));
    }
    let origin = Some(session_id);
    let result = match action {
        PlaybackAction::Play => server.playback_play_from(take_id, looping, origin).await,
        PlaybackAction::Pause => server.playback_pause_from(take_id, origin).await,
        PlaybackAction::Stop => server.playback_stop_from(take_id, origin).await,
        PlaybackAction::Seek => {
            let seek = seek_ns.unwrap_or_default();
            server
                .playback_seek_from(take_id, seek, looping, origin)
                .await
        }
    };
    match result {
        Ok((state, playhead_ns, looping)) => {
            control
                .send(&ControlMessage::PlaybackState {
                    take_id,
                    state,
                    playhead_ns,
                    looping,
                })
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
        }
        Err(err) => {
            if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
                .await
                .is_err()
            {
                let _ = server.close_session(client_hello.device_id, now_ns()).await;
                return Ok(ControlFlow::Break(()));
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

async fn handle_bake_cursor(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    msg: ControlMessage,
) -> Result<HandlerOutcome, ServerError> {
    let reject_reason = match &msg {
        ControlMessage::OpenTakeBakeCursor { .. } => {
            "operator role is required to open bake cursors"
        }
        ControlMessage::ReadTakeBakeFrame { .. } => "operator role is required to read bake frames",
        ControlMessage::SeekTakeBakeFrame { .. } => "operator role is required to seek bake frames",
        ControlMessage::CloseTakeBakeCursor { .. } => {
            "operator role is required to close bake cursors"
        }
        _ => unreachable!(),
    };
    // Operator gate from the server session record, not self-declared roles.
    if !server.session_is_operator(client_hello.device_id).await {
        if send_protocol_error(control, RejectCode::RoleDenied, reject_reason.into())
            .await
            .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
        return Ok(ControlFlow::Continue(()));
    }
    match msg {
        ControlMessage::OpenTakeBakeCursor {
            take_id,
            sampling_mode,
        } => match server.open_take_bake_cursor(take_id, sampling_mode).await {
            Ok((cursor_id, total_frames)) => {
                control
                    .send(&ControlMessage::TakeBakeCursorOpened {
                        cursor_id,
                        total_frames,
                    })
                    .await
                    .map_err(|err| ServerError::Runtime(err.to_string()))?;
            }
            Err(err) => {
                if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
                    .await
                    .is_err()
                {
                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                    return Ok(ControlFlow::Break(()));
                }
            }
        },
        ControlMessage::ReadTakeBakeFrame { cursor_id } => {
            match server.read_take_bake_frame(cursor_id).await {
                Ok(Some((frame_index, timestamp_ns, attributes))) => {
                    control
                        .send(&ControlMessage::TakeBakeFrame {
                            cursor_id,
                            frame_index,
                            timestamp_ns,
                            attributes,
                        })
                        .await
                        .map_err(|err| ServerError::Runtime(err.to_string()))?;
                }
                Ok(None) => {
                    if send_protocol_error(
                        control,
                        RejectCode::ServerBusy,
                        "bake cursor reached end of stream".into(),
                    )
                    .await
                    .is_err()
                    {
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Err(err) => {
                    if send_protocol_error(
                        control,
                        map_server_error_to_reject(&err),
                        err.to_string(),
                    )
                    .await
                    .is_err()
                    {
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        ControlMessage::SeekTakeBakeFrame {
            cursor_id,
            frame_index,
        } => match server.seek_take_bake_frame(cursor_id, frame_index).await {
            Ok(Some((resolved_index, timestamp_ns, attributes))) => {
                control
                    .send(&ControlMessage::TakeBakeFrame {
                        cursor_id,
                        frame_index: resolved_index,
                        timestamp_ns,
                        attributes,
                    })
                    .await
                    .map_err(|err| ServerError::Runtime(err.to_string()))?;
            }
            Ok(None) => {
                if send_protocol_error(
                    control,
                    RejectCode::ServerBusy,
                    "bake seek was out of range".into(),
                )
                .await
                .is_err()
                {
                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                    return Ok(ControlFlow::Break(()));
                }
            }
            Err(err) => {
                if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
                    .await
                    .is_err()
                {
                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                    return Ok(ControlFlow::Break(()));
                }
            }
        },
        ControlMessage::CloseTakeBakeCursor { cursor_id } => {
            match server.close_take_bake_cursor(cursor_id).await {
                Ok(()) => {
                    control
                        .send(&ControlMessage::TakeBakeCursorClosed { cursor_id })
                        .await
                        .map_err(|err| ServerError::Runtime(err.to_string()))?;
                }
                Err(err) => {
                    if send_protocol_error(
                        control,
                        map_server_error_to_reject(&err),
                        err.to_string(),
                    )
                    .await
                    .is_err()
                    {
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(ControlFlow::Continue(()))
}

async fn handle_video_signaling(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    msg: ControlMessage,
) -> Result<HandlerOutcome, ServerError> {
    match msg {
        ControlMessage::GetVideoStreamStatus => {
            let status = server.video_stream_status().await;
            control
                .send(&ControlMessage::VideoStreamStatus(status))
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
        }
        ControlMessage::CreateVideoOffer {
            stream_id,
            track_id,
        } => {
            match server
                .create_video_offer(client_hello.device_id, &stream_id, &track_id)
                .await
            {
                Ok(offer) => {
                    control
                        .send(&ControlMessage::VideoOffer(offer))
                        .await
                        .map_err(|err| ServerError::Runtime(err.to_string()))?;
                }
                Err(err) => {
                    if send_protocol_error(
                        control,
                        map_server_error_to_reject(&err),
                        err.to_string(),
                    )
                    .await
                    .is_err()
                    {
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        ControlMessage::VideoSignal(signal) => {
            if signal.from_device != client_hello.device_id {
                if send_protocol_error(
                    control,
                    RejectCode::RoleDenied,
                    "signal from_device does not match active session".into(),
                )
                .await
                .is_err()
                {
                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                    return Ok(ControlFlow::Break(()));
                }
                return Ok(ControlFlow::Continue(()));
            }
            if signal.to_device == client_hello.device_id {
                match server
                    .handle_video_signal(client_hello.device_id, signal.payload)
                    .await
                {
                    Ok(Some(answer)) => {
                        control
                            .send(&ControlMessage::VideoOffer(answer))
                            .await
                            .map_err(|err| ServerError::Runtime(err.to_string()))?;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        if send_protocol_error(
                            control,
                            map_server_error_to_reject(&err),
                            err.to_string(),
                        )
                        .await
                        .is_err()
                        {
                            let _ = server.close_session(client_hello.device_id, now_ns()).await;
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                }
            } else if let Err(err) = server.push_signaling_message(signal).await {
                if send_protocol_error(control, map_server_error_to_reject(&err), err.to_string())
                    .await
                    .is_err()
                {
                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        ControlMessage::DrainSignals => {
            match server
                .drain_signaling_messages(client_hello.device_id)
                .await
            {
                Ok(messages) => {
                    control
                        .send(&ControlMessage::SignalsBatch(messages))
                        .await
                        .map_err(|err| ServerError::Runtime(err.to_string()))?;
                }
                Err(err) => {
                    if send_protocol_error(
                        control,
                        map_server_error_to_reject(&err),
                        err.to_string(),
                    )
                    .await
                    .is_err()
                    {
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(ControlFlow::Continue(()))
}

async fn handle_quic_peer(
    server: ServerHandle,
    peer: motionstage_transport_quic::QuicPeer,
) -> Result<(), ServerError> {
    let mut control = peer
        .open_control_stream()
        .await
        .map_err(|err| ServerError::Runtime(err.to_string()))?;

    let hello = server.server_hello().await;
    control
        .send(&ControlMessage::ServerHello(hello))
        .await
        .map_err(|err| ServerError::Runtime(err.to_string()))?;

    let client_hello = match control
        .recv()
        .await
        .map_err(|err| ServerError::Runtime(err.to_string()))?
    {
        ControlMessage::ClientHello(hello) => hello,
        _ => {
            // Typed error before every handshake drop: tell the peer why.
            send_fatal_handshake_message(
                &mut control,
                ControlMessage::Error {
                    code: RejectCode::UnsupportedProtocol,
                    reason: "expected ClientHello as first control message".into(),
                },
            )
            .await;
            return Err(ServerError::Runtime(
                "expected ClientHello as first control message".into(),
            ));
        }
    };

    // Every pre-registration failure sends a typed message (RegisterRejected
    // for register-shaped failures, Error otherwise) before the drop.
    if let Err(err) = server
        .discovered(client_hello.device_id, client_hello.device_name.clone())
        .await
    {
        send_handshake_failure(&mut control, &err).await;
        return Err(err);
    }
    if let Err(err) = server.transport_connected(client_hello.device_id).await {
        send_handshake_failure(&mut control, &err).await;
        let _ = server.close_session(client_hello.device_id, now_ns()).await;
        return Err(err);
    }
    if let Err(err) = server.hello_exchanged(client_hello.clone()).await {
        send_handshake_failure(&mut control, &err).await;
        let _ = server.close_session(client_hello.device_id, now_ns()).await;
        return Err(err);
    }
    if let Err(err) = server.authenticate(client_hello.device_id).await {
        send_handshake_failure(&mut control, &err).await;
        let _ = server.close_session(client_hello.device_id, now_ns()).await;
        return Err(err);
    }

    let register_req = match control
        .recv()
        .await
        .map_err(|err| ServerError::Runtime(err.to_string()))?
    {
        ControlMessage::RegisterRequest(req) => req,
        _ => {
            send_fatal_handshake_message(
                &mut control,
                ControlMessage::Error {
                    code: RejectCode::UnsupportedProtocol,
                    reason: "expected RegisterRequest after ClientHello".into(),
                },
            )
            .await;
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Err(ServerError::Runtime(
                "expected RegisterRequest after ClientHello".into(),
            ));
        }
    };

    let session_id = match server.register(client_hello.device_id, register_req).await {
        Ok(accepted) => {
            let session_id = accepted.session_id;
            control
                .send(&ControlMessage::RegisterAccepted(accepted))
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
            session_id
        }
        Err(err) => {
            send_handshake_failure(&mut control, &err).await;
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return match err {
                ServerError::RegisterRejected(_) => Ok(()),
                other => Err(other),
            };
        }
    };

    // Subscribe before taking the snapshot so no event between snapshot and
    // subscription is lost; events already folded into the snapshot are
    // deduplicated by the client via the snapshot's `seq`.
    let mut state_events = server.subscribe_state_events();

    // SceneSynced means what it says: the client received the initial world
    // snapshot before activation.
    let snapshot = server.scene_snapshot_payload().await;
    control
        .send(&ControlMessage::SceneSnapshot(snapshot))
        .await
        .map_err(|err| ServerError::Runtime(err.to_string()))?;
    server.scene_synced(client_hello.device_id).await?;
    server.activate(client_hello.device_id).await?;

    loop {
        tokio::select! {
            ctrl = control.recv() => {
                // Touch session activity on every control message (idle timeout 4.4).
                server.touch_session_activity(client_hello.device_id).await;
                match ctrl {
                    Ok(ControlMessage::Ping) => {
                        control.send(&ControlMessage::Pong).await.map_err(|err| ServerError::Runtime(err.to_string()))?;
                        let active_mode = server.mode().await;
                        control
                            .send(&ControlMessage::ModeState(active_mode))
                            .await
                            .map_err(|err| ServerError::Runtime(err.to_string()))?;
                    }
                    Ok(ControlMessage::Pong) => {}
                    Ok(ControlMessage::ClientGoodbye { reason }) => {
                        info!(
                            device_id = %client_hello.device_id,
                            reason = reason.as_deref().unwrap_or("none"),
                            "client sent goodbye"
                        );
                        let _ = server.close_session_with_reason(client_hello.device_id, now_ns(), reason).await;
                        break;
                    }
                    Ok(ControlMessage::SetDataFlow(state)) => {
                        match handle_set_data_flow(&mut control, &server, &client_hello, session_id, state).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(ControlMessage::SetRecording(state)) => {
                        match handle_set_recording(&mut control, &server, &client_hello, session_id, state).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(ControlMessage::ResyncRequest { last_seq }) => {
                        match server.resync_from(last_seq).await {
                            ResyncResponse::Replay(envelopes) => {
                                for envelope in envelopes {
                                    control
                                        .send(&ControlMessage::StateEventMsg(envelope))
                                        .await
                                        .map_err(|err| ServerError::Runtime(err.to_string()))?;
                                }
                            }
                            ResyncResponse::Snapshot(payload) => {
                                control
                                    .send(&ControlMessage::SceneSnapshot(payload))
                                    .await
                                    .map_err(|err| ServerError::Runtime(err.to_string()))?;
                            }
                        }
                    }
                    Ok(msg @ (ControlMessage::ResetSceneToBaseline { .. }
                        | ControlMessage::CommitSceneBaseline { .. }
                        | ControlMessage::CommitObjectBaseline { .. })) => {
                        match handle_baseline_control(&mut control, &server, &client_hello, session_id, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(msg @ (ControlMessage::ListTakes { .. }
                        | ControlMessage::SelectTake { .. }
                        | ControlMessage::DeleteTake { .. })) => {
                        match handle_take_management(&mut control, &server, &client_hello, session_id, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(ControlMessage::PlaybackControl { take_id, action, seek_ns, looping }) => {
                        match handle_playback_control(&mut control, &server, &client_hello, session_id, take_id, action, seek_ns, looping).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(msg @ (ControlMessage::OpenTakeBakeCursor { .. }
                        | ControlMessage::ReadTakeBakeFrame { .. }
                        | ControlMessage::SeekTakeBakeFrame { .. }
                        | ControlMessage::CloseTakeBakeCursor { .. })) => {
                        match handle_bake_cursor(&mut control, &server, &client_hello, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(msg @ (ControlMessage::CreateMapping { .. }
                        | ControlMessage::UpdateMapping { .. }
                        | ControlMessage::RemoveMapping { .. }
                        | ControlMessage::SetMappingLock { .. })) => {
                        match handle_mapping_ops(&mut control, &server, &client_hello, session_id, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(msg @ (ControlMessage::StartTake | ControlMessage::StopTake)) => {
                        match handle_take_control(&mut control, &server, &client_hello, session_id, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(ControlMessage::GetSceneSnapshot) => {
                        // On-demand world snapshot (target pickers, resync).
                        let snapshot = server.scene_snapshot_payload().await;
                        control
                            .send(&ControlMessage::SceneSnapshot(snapshot))
                            .await
                            .map_err(|err| ServerError::Runtime(err.to_string()))?;
                    }
                    Ok(msg @ (ControlMessage::CreateVideoOffer { .. }
                        | ControlMessage::GetVideoStreamStatus
                        | ControlMessage::VideoSignal(_)
                        | ControlMessage::DrainSignals)) => {
                        match handle_video_signaling(&mut control, &server, &client_hello, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(ControlMessage::SignalsBatch(_))
                    | Ok(ControlMessage::VideoOffer(_))
                    | Ok(ControlMessage::VideoStreamStatus(_))
                    | Ok(ControlMessage::ModeState(_))
                    | Ok(ControlMessage::BaselineActionApplied { .. })
                    | Ok(ControlMessage::MappingCreateResult { .. })
                    | Ok(ControlMessage::MappingOpResult { .. })
                    | Ok(ControlMessage::TakeStartResult { .. })
                    | Ok(ControlMessage::TakeStopResult { .. })
                    | Ok(ControlMessage::TakeList { .. })
                    | Ok(ControlMessage::TakeSelected { .. })
                    | Ok(ControlMessage::PlaybackState { .. })
                    | Ok(ControlMessage::TakeDeleted { .. })
                    | Ok(ControlMessage::TakeBakeCursorOpened { .. })
                    | Ok(ControlMessage::TakeBakeFrame { .. })
                    | Ok(ControlMessage::TakeBakeCursorClosed { .. })
                    | Ok(ControlMessage::Error { .. })
                    | Ok(ControlMessage::ServerHello(_))
                    | Ok(ControlMessage::ClientHello(_))
                    | Ok(ControlMessage::RegisterRequest(_))
                    | Ok(ControlMessage::RegisterAccepted(_))
                    | Ok(ControlMessage::RegisterRejected(_))
                    | Ok(ControlMessage::StateEventMsg(_))
                    | Ok(ControlMessage::SceneSnapshot(_)) => {
                        if send_protocol_error(&mut control, RejectCode::RoleDenied, "unsupported control message in active loop".into()).await.is_err() {
                            let _ = server.close_session(client_hello.device_id, now_ns()).await;
                            break;
                        }
                    }
                    Err(_) => {
                        warn!(device_id = %client_hello.device_id, "control channel closed; ending session");
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        break;
                    }
                }
            }
            datagram = peer.recv_motion_datagram() => {
                // Touch session activity on motion datagrams (idle timeout 4.4).
                server.touch_session_activity(client_hello.device_id).await;
                match datagram {
                    Ok(frame) => {
                        if let Err(err) = server.ingest_motion_datagram(frame).await {
                            warn!(device_id = %client_hello.device_id, error = %err, "failed to ingest motion datagram");
                        }
                    }
                    Err(_) => {
                        warn!(device_id = %client_hello.device_id, "motion datagram channel closed; ending session");
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        break;
                    }
                }
            }
            event = state_events.recv() => {
                match event_delivery_message(&server, event).await {
                    Some(message) => {
                        if control.send(&message).await.is_err() {
                            let _ = server.close_session(client_hello.device_id, now_ns()).await;
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}

/// Map a state-event broadcast recv result to the outbound control message:
/// forward events as [`ControlMessage::StateEventMsg`]; on `Lagged` (the
/// receiver fell behind the broadcast buffer) resync with a fresh
/// [`ControlMessage::SceneSnapshot`] instead of dropping events silently.
/// `None` means the event bus closed and the session loop should end.
async fn event_delivery_message(
    server: &ServerHandle,
    event: Result<StateEventEnvelope, broadcast::error::RecvError>,
) -> Option<ControlMessage> {
    match event {
        Ok(envelope) => Some(ControlMessage::StateEventMsg(envelope)),
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!(skipped, "state event receiver lagged; resyncing with snapshot");
            Some(ControlMessage::SceneSnapshot(
                server.scene_snapshot_payload().await,
            ))
        }
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

async fn send_protocol_error(
    control: &mut motionstage_transport_quic::ControlChannel,
    code: RejectCode,
    reason: String,
) -> Result<(), ServerError> {
    control
        .send(&ControlMessage::Error { code, reason })
        .await
        .map_err(|err| ServerError::Runtime(err.to_string()))
}

/// Send the typed failure for a pre-registration handshake error before the
/// connection is dropped: `RegisterRejected` when the failure is
/// register-shaped, `Error{code, reason}` otherwise. Best-effort — the peer
/// may already be gone.
async fn send_handshake_failure(
    control: &mut motionstage_transport_quic::ControlChannel,
    err: &ServerError,
) {
    let message = match err {
        ServerError::RegisterRejected(rejected) => {
            ControlMessage::RegisterRejected(rejected.clone())
        }
        other => ControlMessage::Error {
            code: map_server_error_to_reject(other),
            reason: other.to_string(),
        },
    };
    send_fatal_handshake_message(control, message).await;
}

/// Deliver a fatal handshake message before the peer task returns (which
/// drops the QUIC connection). Send, FIN the control stream, then linger
/// briefly draining the read side so the QUIC machinery can flush the
/// message; without the linger the connection close races the delivery and
/// the client sees a bare disconnect instead of the typed error.
async fn send_fatal_handshake_message(
    control: &mut motionstage_transport_quic::ControlChannel,
    message: ControlMessage,
) {
    if control.send(&message).await.is_err() {
        return;
    }
    let _ = control.finish();
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        while control.recv().await.is_ok() {}
    })
    .await;
}

fn map_server_error_to_reject(err: &ServerError) -> RejectCode {
    match err {
        ServerError::Protocol(_) => RejectCode::VersionMismatch,
        ServerError::SessionNotFound(_) => RejectCode::RoleDenied,
        ServerError::Denied(_) => RejectCode::RoleDenied,
        ServerError::AlreadyRecording => RejectCode::ServerBusy,
        ServerError::RegisterRejected(rejected) => rejected.code,
        ServerError::Video(_) | ServerError::Signaling(_) => RejectCode::RoleDenied,
        ServerError::Core(_)
        | ServerError::Recording(_)
        | ServerError::WebRtc(_)
        | ServerError::Take(_) => RejectCode::ServerBusy,
        ServerError::Discovery(_) | ServerError::Runtime(_) => RejectCode::ServerBusy,
    }
}

fn take_bake_total_frames(recording: &RecordingFile, sampling_mode: SamplingMode) -> u64 {
    match sampling_mode {
        SamplingMode::Captured => recording.frames.len() as u64,
        SamplingMode::FixedFps { fps } => {
            if fps == 0 || recording.frames.is_empty() {
                return 0;
            }
            let duration = ServerState::playback_duration_ns(recording);
            let step_ns = (1_000_000_000_u64 / fps as u64).max(1);
            if duration == 0 {
                1
            } else {
                duration / step_ns + 1
            }
        }
    }
}

fn take_bake_frame_for_index(
    cursor: &TakeBakeCursor,
    frame_index: u64,
) -> Option<(u64, Vec<TakeBakeAttribute>)> {
    if frame_index >= cursor.total_frames {
        return None;
    }
    match cursor.sampling_mode {
        SamplingMode::Captured => {
            let frame = cursor.recording.frames.get(frame_index as usize)?;
            Some((
                frame.timestamp_ns,
                frame
                    .attributes
                    .iter()
                    .cloned()
                    .map(recorded_to_bake_attribute)
                    .collect(),
            ))
        }
        SamplingMode::FixedFps { fps } => {
            if fps == 0 || cursor.recording.frames.is_empty() {
                return None;
            }
            let step_ns = (1_000_000_000_u64 / fps as u64).max(1);
            let playhead_ns = frame_index.saturating_mul(step_ns);
            let frame = ServerState::frame_for_playhead(&cursor.recording, playhead_ns)?;
            Some((
                frame.timestamp_ns,
                frame
                    .attributes
                    .into_iter()
                    .map(recorded_to_bake_attribute)
                    .collect(),
            ))
        }
    }
}

fn recorded_to_bake_attribute(recorded: RecordedAttribute) -> TakeBakeAttribute {
    TakeBakeAttribute {
        object_id: recorded.object_id,
        attribute: recorded.attribute,
        value: attribute_value_to_bake(&recorded.value),
    }
}

fn attribute_value_to_bake(value: &AttributeValue) -> BakeAttributeValue {
    match value {
        AttributeValue::Bool(v) => BakeAttributeValue::Bool(*v),
        AttributeValue::Int32(v) => BakeAttributeValue::Int32(*v),
        AttributeValue::Float32(v) => BakeAttributeValue::Float32(*v),
        AttributeValue::Float64(v) => BakeAttributeValue::Float64(*v),
        AttributeValue::Vec2f(v) => BakeAttributeValue::Vec2f(*v),
        AttributeValue::Vec3f(v) => BakeAttributeValue::Vec3f(*v),
        AttributeValue::Vec4f(v) => BakeAttributeValue::Vec4f(*v),
        AttributeValue::Quatf(v) => BakeAttributeValue::Quatf(*v),
        AttributeValue::Mat4f(v) => BakeAttributeValue::Mat4f(*v),
        AttributeValue::Trigger(v) => BakeAttributeValue::Trigger(*v),
    }
}

fn mapping_to_summary(mapping: &motionstage_core::Mapping) -> MappingSummary {
    MappingSummary {
        mapping_id: mapping.id,
        source_device: mapping.source_device,
        source_output: mapping.source_output.clone(),
        target_scene: mapping.target_scene,
        target_object: mapping.target_object,
        target_attribute: mapping.target_attribute.clone(),
        component_mask: mapping.component_mask.clone(),
        lock: mapping.lock,
    }
}

fn scene_to_snapshot(scene: &Scene) -> SnapshotScene {
    SnapshotScene {
        scene_id: scene.id,
        name: scene.name.clone(),
        objects: scene
            .objects
            .values()
            .map(|object| SnapshotObject {
                object_id: object.id,
                name: object.name.clone(),
                attributes: object
                    .attributes
                    .values()
                    .map(|attr| SnapshotAttribute {
                        name: attr.name.clone(),
                        default_value: attribute_value_to_bake(&attr.default_value),
                        current_value: attribute_value_to_bake(&attr.current_value),
                        live_enabled: attr.live_enabled,
                        record_enabled: attr.record_enabled,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_nanos() as u64)
        .unwrap_or_default()
}

fn advertised_host_for(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        local_lan_ip().unwrap_or_else(|| "127.0.0.1".into())
    } else {
        addr.ip().to_string()
    }
}

/// Resolve the primary LAN-facing IP by asking the OS which interface would
/// route to an external address.  No traffic is actually sent.
fn local_lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let local_addr = socket.local_addr().ok()?;
    if local_addr.ip().is_loopback() || local_addr.ip().is_unspecified() {
        return None;
    }
    Some(local_addr.ip().to_string())
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("core error: {0}")]
    Core(#[from] CoreError),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("session not found: {0}")]
    SessionNotFound(Uuid),
    #[error("registration rejected: {0:?}")]
    RegisterRejected(RegisterRejected),
    /// An operator-plane operation was refused by the permission model
    /// (own-source vs [`ClientRole::Operator`]). Nothing was mutated and no
    /// event was emitted.
    #[error("denied: {0}")]
    Denied(String),
    /// A `StartTake`/`start_recording` was issued while a recording is already
    /// active. The request mutates nothing and emits no event — the in-flight
    /// take is never silently replaced. Answered on the wire with
    /// [`RejectCode::ServerBusy`] inside [`ControlMessage::TakeStartResult`].
    #[error("a recording is already active")]
    AlreadyRecording,
    #[error("recording error: {0}")]
    Recording(String),
    #[error("take error: {0}")]
    Take(String),
    #[error("video error: {0}")]
    Video(String),
    #[error("webrtc error: {0}")]
    WebRtc(String),
    #[error("discovery error: {0}")]
    Discovery(String),
    #[error("signaling error: {0}")]
    Signaling(String),
    #[error("runtime error: {0}")]
    Runtime(String),
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use motionstage_core::{
        AttributeUpdate, AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject,
    };
    use motionstage_media::{
        ColorPrimaries, DynamicRange, IceCandidate, SdpMessage, SdpType, SignalMessage,
        SignalPayload, ToneMapMode, TransferFunction, VideoClientCapability, VideoCodec,
        VideoStreamDescriptor,
    };
    use motionstage_protocol::{
        AttributeDescriptor, AttributeKind, BakeAttributeValue, BaselineAction, ClientHello,
        ClientRole, ControlMessage, DataFlowState, Feature, Mode, PlaybackRuntimeState,
        RecordingState, RegisterRequest, RejectCode, SamplingMode, SessionState, StateEvent,
        StateEventEnvelope, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    };
    use tempfile::{tempdir, NamedTempFile};
    use uuid::Uuid;

    use crate::{
        event_delivery_message, now_ns, ResyncResponse, SecurityMode, ServerConfig, ServerError,
        ServerHandle,
    };
    use motionstage_recording::{read_recording, RecordingFormatVersion, RecordingMarker};
    use motionstage_transport_quic::{
        AttributeUpdateFrame, ControlChannel, MotionDatagram, QuicClient, QuicPeer,
    };
    use motionstage_webrtc::WebRtcSession;

    /// Server config whose take catalog lives in a fresh tempdir. Every test
    /// that can write the take catalog (recording, take select/delete) must
    /// use this so `cargo test` never touches the tracked
    /// `recordings/takes_catalog.json` (the crate-relative default path).
    fn config_with_temp_catalog() -> (ServerConfig, tempfile::TempDir) {
        let temp = tempdir().unwrap();
        let mut config = ServerConfig::default();
        config.take_catalog_path = temp.path().join("takes.json");
        (config, temp)
    }

    async fn connect_active_quic_client(
        addr: SocketAddr,
        device_id: Uuid,
        role: ClientRole,
        feature: Feature,
    ) -> (QuicPeer, ControlChannel) {
        let client = QuicClient::new_insecure_for_local_dev().unwrap();
        let peer = client.connect(addr).await.unwrap();
        let mut control = peer.accept_control_stream().await.unwrap();

        assert!(matches!(
            control.recv().await.unwrap(),
            ControlMessage::ServerHello(_)
        ));

        control
            .send(&ControlMessage::ClientHello(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: format!("peer-{device_id}"),
                roles: vec![role],
                features: vec![feature],
                advertised_attributes: if role == ClientRole::MotionSource {
                    vec![AttributeDescriptor {
                        path: "pose_pos".into(),
                        value_type: AttributeKind::Vec3f,
                    }]
                } else {
                    Vec::new()
                },
            }))
            .await
            .unwrap();
        control
            .send(&ControlMessage::RegisterRequest(RegisterRequest {
                pairing_token: None,
                api_key: None,
            }))
            .await
            .unwrap();

        assert!(matches!(
            control.recv().await.unwrap(),
            ControlMessage::RegisterAccepted(_)
        ));
        // The SceneSynced handshake step sends the initial world snapshot.
        assert!(matches!(
            control.recv().await.unwrap(),
            ControlMessage::SceneSnapshot(_)
        ));
        (peer, control)
    }

    /// Receive the next non-event control message. Sessions receive the full
    /// replicated event stream (including their own echoes), so tests waiting
    /// on a direct response skip `StateEventMsg` frames.
    async fn recv_skipping_events(control: &mut ControlChannel) -> ControlMessage {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), control.recv())
                .await
                .expect("control message within timeout")
                .unwrap()
            {
                ControlMessage::StateEventMsg(_) => continue,
                other => return other,
            }
        }
    }

    /// Receive control messages until one matches the predicate.
    async fn recv_until(
        control: &mut ControlChannel,
        predicate: impl Fn(&ControlMessage) -> bool,
    ) -> ControlMessage {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(2), control.recv())
                .await
                .expect("control message within timeout")
                .unwrap();
            if predicate(&message) {
                return message;
            }
        }
    }

    #[tokio::test]
    async fn session_progression_and_reconnect_issue_new_session_id() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();

        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: vec![AttributeDescriptor {
                    path: "pose_pos".into(),
                    value_type: AttributeKind::Vec3f,
                }],
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();

        let accepted_a = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();
        server.close_session(device_id, 10).await.unwrap();

        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: vec![AttributeDescriptor {
                    path: "pose_pos".into(),
                    value_type: AttributeKind::Vec3f,
                }],
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        let accepted_b = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();

        assert_ne!(accepted_a.session_id, accepted_b.session_id);
    }

    #[tokio::test]
    async fn duplicate_active_device_name_is_rejected_on_register() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_a = Uuid::now_v7();
        let device_b = Uuid::now_v7();

        for device_id in [device_a, device_b] {
            server.discovered(device_id, "ipad").await.unwrap();
            server.transport_connected(device_id).await.unwrap();
            server
                .hello_exchanged(ClientHello {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                    device_id,
                    device_name: "ipad".into(),
                    roles: vec![ClientRole::MotionSource],
                    features: vec![Feature::Motion],
                    advertised_attributes: vec![AttributeDescriptor {
                        path: "pose_pos".into(),
                        value_type: AttributeKind::Vec3f,
                    }],
                })
                .await
                .unwrap();
            server.authenticate(device_id).await.unwrap();
        }

        server
            .register(
                device_a,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();

        let err = server
            .register(
                device_b,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap_err();
        match err {
            ServerError::RegisterRejected(rejected) => {
                assert_eq!(rejected.code, RejectCode::RoleDenied);
                assert!(rejected.reason.contains("device name 'ipad'"));
            }
            other => panic!("expected register rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recording_blocks_remap_and_writes_cmtrk() {
        let (config, _temp) = config_with_temp_catalog();
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;

        server.load_scene(scene).await;

        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();

        let tmp = NamedTempFile::new().unwrap();
        let _recording_id = server.start_recording(tmp.path(), 101).await.unwrap();

        let err = server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos_alt".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                102,
            )
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("blocked in recording"));
        let manifest = server.stop_recording().await.unwrap();
        assert_eq!(manifest.scene_id, scene_id);

        let recording = read_recording(tmp.path()).unwrap();
        assert_eq!(recording.version, RecordingFormatVersion::V2);
        assert!(recording.markers.iter().any(|marker| matches!(
            marker,
            RecordingMarker::ModeTransition {
                to: Mode {
                    recording: RecordingState::Recording,
                    ..
                },
                ..
            }
        )));
        assert!(recording
            .markers
            .iter()
            .any(|marker| matches!(marker, RecordingMarker::MappingCreated { .. })));
    }

    #[tokio::test]
    async fn pair_mode_requires_pairing_token() {
        let mut config = ServerConfig::default();
        config.security_mode = SecurityMode::PairingRequired;
        config.pairing_token = Some("abc123".into());

        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        server.discovered(device_id, "controller").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "controller".into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: vec![AttributeDescriptor {
                    path: "pose_pos".into(),
                    value_type: AttributeKind::Vec3f,
                }],
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();

        let err = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: Some("bad".into()),
                    api_key: None,
                },
            )
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("registration rejected"));
    }

    #[tokio::test]
    async fn hdr_descriptor_negotiates_sdr_fallback() {
        let server = ServerHandle::new(ServerConfig::default());

        server
            .set_master_video_descriptor(VideoStreamDescriptor {
                width: 1920,
                height: 1080,
                fps: 24,
                dynamic_range: DynamicRange::Hdr10,
                color_primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                bit_depth: 10,
                codec: VideoCodec::Vp9,
            })
            .await
            .unwrap();

        let stream = server
            .negotiate_video_for_client(VideoClientCapability {
                supports_hdr10: false,
                max_width: 1920,
                max_height: 1080,
                max_fps: 24,
                supported_codecs: vec![VideoCodec::H264],
            })
            .await
            .unwrap();

        assert_eq!(stream.descriptor.width, 1920);
        assert_eq!(stream.tone_map, ToneMapMode::Hdr10ToSdr);
    }

    #[tokio::test]
    async fn create_video_offer_creates_server_peer() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();

        server
            .set_master_video_descriptor(VideoStreamDescriptor {
                width: 1920,
                height: 1080,
                fps: 24,
                dynamic_range: DynamicRange::Hdr10,
                color_primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                bit_depth: 10,
                codec: VideoCodec::Vp9,
            })
            .await
            .unwrap();

        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::VideoSink],
                features: vec![Feature::Video],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();

        let offer = server
            .create_video_offer(device_id, "motionstage", "camera0")
            .await
            .unwrap();
        assert_eq!(offer.ty, SdpType::Offer);
        assert!(!offer.sdp.is_empty());
        assert!(server.has_video_session(device_id).await);
    }

    #[tokio::test]
    async fn server_applies_remote_answer_for_video_peer() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();

        server
            .set_master_video_descriptor(VideoStreamDescriptor {
                width: 1920,
                height: 1080,
                fps: 24,
                dynamic_range: DynamicRange::Hdr10,
                color_primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                bit_depth: 10,
                codec: VideoCodec::Vp9,
            })
            .await
            .unwrap();

        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::VideoSink],
                features: vec![Feature::Video],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();

        let offer = server
            .create_video_offer(device_id, "motionstage", "camera0")
            .await
            .unwrap();

        let client = WebRtcSession::new().await.unwrap();
        client.apply_remote_sdp(offer).await.unwrap();
        let answer = client.create_answer().await.unwrap();

        let response = server
            .handle_video_signal(device_id, SignalPayload::Sdp(answer))
            .await
            .unwrap();
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn session_state_reaches_active() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();

        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::VideoSink],
                features: vec![Feature::Video],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();

        let session = server.session_info(device_id).await.unwrap();
        assert_eq!(session.state, SessionState::Active);
    }

    #[tokio::test]
    async fn signaling_routes_between_active_sessions() {
        let server = ServerHandle::new(ServerConfig::default());
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();

        for (device, name, role, feature) in [
            (a, "peer-a", ClientRole::VideoSink, Feature::Video),
            (b, "peer-b", ClientRole::CameraController, Feature::Video),
        ] {
            server.discovered(device, name).await.unwrap();
            server.transport_connected(device).await.unwrap();
            server
                .hello_exchanged(ClientHello {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                    device_id: device,
                    device_name: name.into(),
                    roles: vec![role],
                    features: vec![feature],
                    advertised_attributes: if role == ClientRole::MotionSource {
                        vec![AttributeDescriptor {
                            path: "pose_pos".into(),
                            value_type: AttributeKind::Vec3f,
                        }]
                    } else {
                        Vec::new()
                    },
                })
                .await
                .unwrap();
            server.authenticate(device).await.unwrap();
            server
                .register(
                    device,
                    RegisterRequest {
                        pairing_token: None,
                        api_key: None,
                    },
                )
                .await
                .unwrap();
            server.scene_synced(device).await.unwrap();
            server.activate(device).await.unwrap();
        }

        server
            .push_signaling_message(SignalMessage {
                from_device: a,
                to_device: b,
                payload: SignalPayload::Sdp(SdpMessage {
                    ty: SdpType::Offer,
                    sdp: "v=0".into(),
                }),
            })
            .await
            .unwrap();
        server
            .push_signaling_message(SignalMessage {
                from_device: b,
                to_device: a,
                payload: SignalPayload::Ice(IceCandidate {
                    candidate: "candidate:0".into(),
                    sdp_mid: Some("0".into()),
                    sdp_mline_index: Some(0),
                }),
            })
            .await
            .unwrap();

        let to_b = server.drain_signaling_messages(b).await.unwrap();
        assert_eq!(to_b.len(), 1);
        let to_a = server.drain_signaling_messages(a).await.unwrap();
        assert_eq!(to_a.len(), 1);
    }

    #[tokio::test]
    async fn capacity_limit_rejects_new_discovery() {
        let mut config = ServerConfig::default();
        config.max_sessions = 1;
        let server = ServerHandle::new(config);

        let a = Uuid::now_v7();
        let b = Uuid::now_v7();

        server.discovered(a, "a").await.unwrap();
        let err = server.discovered(b, "b").await.unwrap_err();
        assert!(format!("{err}").contains("capacity"));
        assert_eq!(server.session_count().await, 1);
    }

    #[tokio::test]
    async fn protocol_version_mismatch_is_rejected() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();
        server.discovered(device_id, "peer").await.unwrap();
        server.transport_connected(device_id).await.unwrap();

        let err = server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR + 1,
                protocol_minor: 0,
                device_id,
                device_name: "peer".into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: vec![AttributeDescriptor {
                    path: "pose_pos".into(),
                    value_type: AttributeKind::Vec3f,
                }],
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unsupported major"));
    }

    #[tokio::test]
    async fn motion_source_must_advertise_attributes() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();
        server.discovered(device_id, "peer").await.unwrap();
        server.transport_connected(device_id).await.unwrap();

        let err = server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "peer".into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("must advertise at least one attribute"));
    }

    #[tokio::test]
    async fn quic_control_can_request_video_offer() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        let server = ServerHandle::new(config);
        server
            .set_master_video_descriptor(VideoStreamDescriptor {
                width: 1920,
                height: 1080,
                fps: 24,
                dynamic_range: DynamicRange::Hdr10,
                color_primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                bit_depth: 10,
                codec: VideoCodec::Vp9,
            })
            .await
            .unwrap();

        let runtime = server.start_quic_runtime().await.unwrap();
        let device_id = Uuid::now_v7();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            device_id,
            ClientRole::VideoSink,
            Feature::Video,
        )
        .await;

        control
            .send(&ControlMessage::CreateVideoOffer {
                stream_id: "motionstage".into(),
                track_id: "camera".into(),
            })
            .await
            .unwrap();
        let response = recv_skipping_events(&mut control).await;
        match response {
            ControlMessage::VideoOffer(sdp) => {
                assert_eq!(sdp.ty, SdpType::Offer);
                assert!(!sdp.sdp.is_empty());
            }
            other => panic!("expected video offer response, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn quic_control_can_query_video_stream_status() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();
        let device_id = Uuid::now_v7();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            device_id,
            ClientRole::VideoSink,
            Feature::Video,
        )
        .await;

        control
            .send(&ControlMessage::GetVideoStreamStatus)
            .await
            .unwrap();
        let initial = recv_skipping_events(&mut control).await;
        match initial {
            ControlMessage::VideoStreamStatus(status) => {
                assert!(!status.available);
                assert!(!status.descriptor_set);
                assert_eq!(status.peer_count, 0);
                assert_eq!(status.last_frame_age_ms, None);
            }
            other => panic!("expected VideoStreamStatus response, got {other:?}"),
        }

        server
            .set_master_video_descriptor(VideoStreamDescriptor {
                width: 1920,
                height: 1080,
                fps: 24,
                dynamic_range: DynamicRange::Hdr10,
                color_primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                bit_depth: 10,
                codec: VideoCodec::Vp9,
            })
            .await
            .unwrap();
        server
            .push_video_frame(bytes::Bytes::from_static(b"frame"), Duration::from_millis(33))
            .await
            .unwrap();

        control
            .send(&ControlMessage::GetVideoStreamStatus)
            .await
            .unwrap();
        let active = recv_skipping_events(&mut control).await;
        match active {
            ControlMessage::VideoStreamStatus(status) => {
                assert!(status.available);
                assert!(status.descriptor_set);
                assert_eq!(status.peer_count, 0);
                assert!(status.last_frame_age_ms.is_some());
            }
            other => panic!("expected VideoStreamStatus response, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn set_data_flow_acks_via_event_echo_and_ping_keeps_mode_state() {
        let device_id = Uuid::now_v7();
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        let (peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            device_id,
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        // Retired ack: SetDataFlow success sends no direct ModeState. The
        // caller observes its own replicated event echo instead. Ping right
        // after; the first non-event reply must be Pong, then the heartbeat
        // ModeState (which is kept as a liveness+state probe).
        control
            .send(&ControlMessage::SetDataFlow(DataFlowState::Live))
            .await
            .unwrap();
        control.send(&ControlMessage::Ping).await.unwrap();

        // No direct message may precede the Pong: a ModeState here would be
        // the retired SetDataFlow ack. Event echoes may interleave freely
        // (their delivery races the direct replies in the session loop).
        let mut saw_mode_event = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(2), control.recv())
                .await
                .expect("control message within timeout")
                .unwrap()
            {
                ControlMessage::StateEventMsg(envelope) => {
                    if matches!(envelope.event, StateEvent::ModeChanged { mode: Mode::LIVE }) {
                        saw_mode_event = true;
                    }
                }
                ControlMessage::Pong => break,
                other => panic!("expected event echo or Pong before anything else, got {other:?}"),
            }
        }
        // The heartbeat ModeState follows the Pong.
        match recv_skipping_events(&mut control).await {
            ControlMessage::ModeState(mode) => assert_eq!(mode, Mode::LIVE),
            other => panic!("expected heartbeat ModeState after Pong, got {other:?}"),
        }
        // The mutation replicated as an event echo (possibly after the Pong).
        if !saw_mode_event {
            let echo = recv_until(&mut control, |message| {
                matches!(
                    message,
                    ControlMessage::StateEventMsg(StateEventEnvelope {
                        event: StateEvent::ModeChanged { mode: Mode::LIVE },
                        ..
                    })
                )
            })
            .await;
            assert!(matches!(echo, ControlMessage::StateEventMsg(_)));
        }

        drop(peer);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mode_change_is_broadcast_to_other_active_clients() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer_a, mut control_a) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;
        let (_peer_b, mut control_b) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        control_b
            .send(&ControlMessage::SetDataFlow(DataFlowState::Live))
            .await
            .unwrap();

        // The mutating client observes its own echo (no direct ModeState ack).
        let echo = recv_until(&mut control_b, |message| {
            matches!(
                message,
                ControlMessage::StateEventMsg(StateEventEnvelope {
                    event: StateEvent::ModeChanged { mode: Mode::LIVE },
                    ..
                })
            )
        })
        .await;
        assert!(matches!(echo, ControlMessage::StateEventMsg(_)));

        // The other active client observes the change as a replicated state
        // event carrying the originating session.
        let pushed = recv_until(&mut control_a, |message| {
            matches!(
                message,
                ControlMessage::StateEventMsg(StateEventEnvelope {
                    event: StateEvent::ModeChanged { mode: Mode::LIVE },
                    ..
                })
            )
        })
        .await;
        match pushed {
            ControlMessage::StateEventMsg(envelope) => {
                assert!(envelope.origin_session.is_some());
                assert_ne!(envelope.origin_session, Some(server.host_session_id()));
            }
            other => panic!("expected StateEventMsg, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn operator_role_can_reset_scene_baseline_over_control() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);

        let mut attr = SceneAttribute::new("position", AttributeValue::Vec3f([10.0, 20.0, 30.0]));
        attr.current_value = AttributeValue::Vec3f([11.0, 22.0, 33.0]);
        let object = SceneObject::new("camera").with_attribute(attr);
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        server.load_scene(scene).await;

        let runtime = server.start_quic_runtime().await.unwrap();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        control
            .send(&ControlMessage::ResetSceneToBaseline {
                scene_id: Some(scene_id),
            })
            .await
            .unwrap();
        // Retired ack: no direct BaselineActionApplied. The single
        // acknowledgement is the caller's own replicated event echo, which
        // carries the same change count. A Ping afterwards proves no direct
        // ack sneaks in between.
        control.send(&ControlMessage::Ping).await.unwrap();
        let mut saw_baseline_event = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(2), control.recv())
                .await
                .expect("control message within timeout")
                .unwrap()
            {
                ControlMessage::StateEventMsg(envelope) => {
                    if let StateEvent::BaselineApplied {
                        action,
                        changed_attributes,
                    } = envelope.event
                    {
                        assert_eq!(action, BaselineAction::ResetScene);
                        assert_eq!(changed_attributes, 1);
                        saw_baseline_event = true;
                    }
                }
                // The first direct message must be the Pong — a
                // BaselineActionApplied here would be the retired ack.
                ControlMessage::Pong => break,
                other => panic!("expected event echo or Pong, got {other:?}"),
            }
        }
        // The echo may race past the Pong; wait for it if it hasn't landed.
        if !saw_baseline_event {
            let echo = recv_until(&mut control, |message| {
                matches!(
                    message,
                    ControlMessage::StateEventMsg(StateEventEnvelope {
                        event: StateEvent::BaselineApplied { .. },
                        ..
                    })
                )
            })
            .await;
            assert!(matches!(echo, ControlMessage::StateEventMsg(_)));
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn non_operator_cannot_issue_baseline_control_actions() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        server
            .load_scene(Scene::new("shot").with_object(object))
            .await;

        let runtime = server.start_quic_runtime().await.unwrap();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::VideoSink,
            Feature::Video,
        )
        .await;

        control
            .send(&ControlMessage::CommitSceneBaseline { scene_id: None })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::Error { code, .. } => assert_eq!(code, RejectCode::RoleDenied),
            other => panic!("expected protocol error, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn quic_control_routes_and_drains_video_signals() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let (_peer_a, mut control_a) = connect_active_quic_client(
            runtime.local_addr,
            a,
            ClientRole::VideoSink,
            Feature::Video,
        )
        .await;
        let (_peer_b, mut control_b) = connect_active_quic_client(
            runtime.local_addr,
            b,
            ClientRole::VideoSink,
            Feature::Video,
        )
        .await;

        control_a
            .send(&ControlMessage::VideoSignal(SignalMessage {
                from_device: a,
                to_device: b,
                payload: SignalPayload::Sdp(SdpMessage {
                    ty: SdpType::Offer,
                    sdp: "v=0".into(),
                }),
            }))
            .await
            .unwrap();

        control_b.send(&ControlMessage::DrainSignals).await.unwrap();
        let response = recv_skipping_events(&mut control_b).await;
        match response {
            ControlMessage::SignalsBatch(batch) => {
                assert_eq!(batch.len(), 1);
                assert_eq!(batch[0].from_device, a);
                assert_eq!(batch[0].to_device, b);
            }
            other => panic!("expected signals batch, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn start_starts_runtime_and_stop_shuts_it_down() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);

        let adv = server.start().await.unwrap();
        assert!(adv.bind_port > 0);

        let device_id = Uuid::now_v7();
        let (_peer, _control) = connect_active_quic_client(
            format!("127.0.0.1:{}", adv.bind_port).parse().unwrap(),
            device_id,
            ClientRole::MotionSource,
            Feature::Motion,
        )
        .await;

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn quic_runtime_accepts_session_and_ingests_motion() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        let server = ServerHandle::new(config);

        let device_id = Uuid::now_v7();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("runtime").with_object(object);
        let scene_id = scene.id;
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                1,
            )
            .await
            .unwrap();
        server.set_data_flow(DataFlowState::Live).await.unwrap();

        let runtime = server.start_quic_runtime().await.unwrap();
        let client = QuicClient::new_insecure_for_local_dev().unwrap();
        let peer = client.connect(runtime.local_addr).await.unwrap();
        let mut control = peer.accept_control_stream().await.unwrap();

        match control.recv().await.unwrap() {
            ControlMessage::ServerHello(_) => {}
            other => panic!("expected server hello, got {other:?}"),
        }

        control
            .send(&ControlMessage::ClientHello(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "peer".into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: vec![AttributeDescriptor {
                    path: "pose_pos".into(),
                    value_type: AttributeKind::Vec3f,
                }],
            }))
            .await
            .unwrap();
        control
            .send(&ControlMessage::RegisterRequest(RegisterRequest {
                pairing_token: None,
                api_key: None,
            }))
            .await
            .unwrap();

        let reg = control.recv().await.unwrap();
        assert!(matches!(reg, ControlMessage::RegisterAccepted(_)));

        peer.send_motion_datagram(motionstage_transport_quic::MotionDatagram {
            device_id,
            timestamp_ns: 10,
            updates: vec![AttributeUpdateFrame {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]).into(),
            }],
        })
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(60)).await;
        assert!(server.tick_count().await > 0);
        let metrics = server.metrics().await;
        assert!(metrics.motion_datagrams >= 1);
        assert!(metrics.motion_updates >= 1);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ingest_motion_datagram_matches_qualified_and_unqualified_source_outputs() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("runtime").with_object(object);
        let scene_id = scene.id;
        server.load_scene(scene).await;

        let qualified_mapping_id = server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: format!("{device_id}.pose_pos"),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                1,
            )
            .await
            .unwrap();
        server.set_data_flow(DataFlowState::Live).await.unwrap();

        server
            .ingest_motion_datagram(MotionDatagram {
                device_id,
                timestamp_ns: 10,
                updates: vec![AttributeUpdateFrame {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]).into(),
                }],
            })
            .await
            .unwrap();
        assert!(server.tick_count().await > 0);

        server.remove_mapping(qualified_mapping_id).await.unwrap();
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                2,
            )
            .await
            .unwrap();
        server
            .ingest_motion_datagram(MotionDatagram {
                device_id,
                timestamp_ns: 11,
                updates: vec![AttributeUpdateFrame {
                    output_attribute: format!("{device_id}.pose_pos"),
                    value: AttributeValue::Vec3f([2.0, 3.0, 4.0]).into(),
                }],
            })
            .await
            .unwrap();
        assert!(server.tick_count().await > 0);
    }

    #[tokio::test]
    async fn recording_persists_resolved_runtime_values_after_relative_composition() {
        let (config, _temp) = config_with_temp_catalog();
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([10.0, 20.0, 30.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();

        let tmp = NamedTempFile::new().unwrap();
        server.start_recording(tmp.path(), 101).await.unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                102,
            )
            .await
            .unwrap();
        server.stop_recording().await.unwrap();

        let recording = read_recording(tmp.path()).unwrap();
        assert_eq!(recording.frames.len(), 1);
        assert_eq!(recording.frames[0].attributes.len(), 1);
        assert_eq!(
            recording.frames[0].attributes[0].value,
            AttributeValue::Vec3f([11.0, 22.0, 33.0])
        );
    }

    #[tokio::test]
    async fn stop_recording_registers_take_in_catalog() {
        let temp = tempdir().unwrap();
        let recording_path = temp.path().join("take-001.cmtrk");
        let catalog_path = temp.path().join("takes.json");

        let mut config = ServerConfig::default();
        config.take_catalog_path = catalog_path;
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();

        server.start_recording(&recording_path, 101).await.unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                102,
            )
            .await
            .unwrap();
        let manifest = server.stop_recording().await.unwrap();

        let takes = server.list_takes(None).await;
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0].take_id, manifest.recording_id);
        assert_eq!(takes[0].scene_id, scene_id);
        assert_eq!(takes[0].frame_count, 1);
        assert!(takes[0].selected);
    }

    #[tokio::test]
    async fn playback_mode_blocks_ingest_and_supports_bake_cursor() {
        let temp = tempdir().unwrap();
        let recording_path = temp.path().join("take-001.cmtrk");
        let catalog_path = temp.path().join("takes.json");

        let mut config = ServerConfig::default();
        config.take_catalog_path = catalog_path;
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();

        server.start_recording(&recording_path, 101).await.unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                102,
            )
            .await
            .unwrap();
        let manifest = server.stop_recording().await.unwrap();

        let _ = server
            .playback_play(manifest.recording_id, false)
            .await
            .unwrap();
        assert_eq!(server.mode().await, Mode::PLAYBACK);

        let before = server.runtime_snapshot().await;
        let before_value = before
            .scenes
            .get(&scene_id)
            .and_then(|scene| scene.objects.get(&object_id))
            .and_then(|object| object.attributes.get("position"))
            .map(|attr| attr.current_value.clone())
            .unwrap();

        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([9.0, 9.0, 9.0]),
                }],
                200,
            )
            .await
            .unwrap();
        let after = server.runtime_snapshot().await;
        let after_value = after
            .scenes
            .get(&scene_id)
            .and_then(|scene| scene.objects.get(&object_id))
            .and_then(|object| object.attributes.get("position"))
            .map(|attr| attr.current_value.clone())
            .unwrap();
        assert_eq!(before_value, after_value);

        let (cursor_id, total_frames) = server
            .open_take_bake_cursor(manifest.recording_id, SamplingMode::Captured)
            .await
            .unwrap();
        assert!(total_frames >= 1);
        let first = server.read_take_bake_frame(cursor_id).await.unwrap();
        assert!(first.is_some());
        server.close_take_bake_cursor(cursor_id).await.unwrap();
    }

    #[tokio::test]
    async fn delete_take_purges_recording_file_and_catalog_entry() {
        let temp = tempdir().unwrap();
        let recording_path = temp.path().join("take-001.cmtrk");
        let catalog_path = temp.path().join("takes.json");

        let mut config = ServerConfig::default();
        config.take_catalog_path = catalog_path;
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();

        server.start_recording(&recording_path, 101).await.unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                102,
            )
            .await
            .unwrap();
        let manifest = server.stop_recording().await.unwrap();
        assert!(recording_path.exists());

        server.delete_take(manifest.recording_id).await.unwrap();
        assert!(!recording_path.exists());
        assert!(server.list_takes(None).await.is_empty());
    }

    // -----------------------------------------------------------------------
    // Event plane (P1)
    // -----------------------------------------------------------------------

    fn drain_events(
        rx: &mut tokio::sync::broadcast::Receiver<StateEventEnvelope>,
    ) -> Vec<StateEventEnvelope> {
        let mut events = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            events.push(envelope);
        }
        events
    }

    fn camera_scene() -> (Scene, Uuid, Uuid) {
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([10.0, 20.0, 30.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        (scene, scene_id, object_id)
    }

    #[tokio::test]
    async fn start_take_while_recording_active_rejects_and_preserves_in_progress_take() {
        let (config, _temp) = config_with_temp_catalog();
        let server = ServerHandle::new(config);
        let (scene, _scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;

        // A take is in progress: it is the active recording.
        let first = server
            .start_take_from(1_000, None)
            .await
            .expect("first take starts");

        // A second StartTake while a recording is already active (retry, or a
        // second operator) must be rejected with the typed AlreadyRecording
        // error, mutate nothing, and emit no event — the in-progress take is
        // never silently replaced (finding: the first writer would be dropped
        // unflushed).
        let mut rx = server.subscribe_state_events();
        let seq_before = server.current_event_seq().await;
        let err = server
            .start_take_from(2_000, None)
            .await
            .expect_err("second StartTake while recording must be rejected");
        assert!(
            matches!(err, ServerError::AlreadyRecording),
            "unexpected error: {err:?}"
        );
        assert_eq!(super::map_server_error_to_reject(&err), RejectCode::ServerBusy);
        assert!(
            drain_events(&mut rx).is_empty(),
            "rejected StartTake must emit no events"
        );
        assert_eq!(server.current_event_seq().await, seq_before);

        // The original in-progress take survives intact: stopping it registers
        // exactly the first take id, proving its writer was neither dropped nor
        // replaced by the rejected second StartTake.
        let take = server
            .stop_take_from(None)
            .await
            .expect("first take stops cleanly");
        assert_eq!(take.take_id, first);
    }

    #[tokio::test]
    async fn host_session_is_registered_at_startup() {
        let server = ServerHandle::new(ServerConfig::default());
        let sessions = server.sessions().await;
        let host = sessions
            .iter()
            .find(|s| s.is_host)
            .expect("host session exists");
        assert_eq!(host.device_name, "host");
        assert_eq!(host.device_id, server.host_device_id());
        assert_eq!(host.session_id, Some(server.host_session_id()));
        assert_eq!(host.state, SessionState::Active);
        assert!(host.roles.contains(&ClientRole::SceneAuthor));
        assert!(host.roles.contains(&ClientRole::Operator));

        // The host join is event seq 1, replayable from the ring buffer.
        match server.resync_from(0).await {
            ResyncResponse::Replay(events) => {
                assert_eq!(events[0].seq, 1);
                assert!(matches!(
                    &events[0].event,
                    StateEvent::SessionJoined { session_id, device_name, .. }
                        if *session_id == server.host_session_id() && device_name == "host"
                ));
            }
            other => panic!("expected replay from seq 0, got {other:?}"),
        }

        // The host does not consume client capacity.
        let mut config = ServerConfig::default();
        config.max_sessions = 1;
        let server = ServerHandle::new(config);
        server.discovered(Uuid::now_v7(), "ipad").await.unwrap();
        assert_eq!(server.session_count().await, 1);
    }

    #[tokio::test]
    async fn host_api_mutations_stamp_host_session_origin() {
        let server = ServerHandle::new(ServerConfig::default());
        let mut rx = server.subscribe_state_events();
        server.set_data_flow(DataFlowState::Live).await.unwrap();
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].origin_session, Some(server.host_session_id()));
        assert!(matches!(
            events[0].event,
            StateEvent::ModeChanged { mode: Mode::LIVE }
        ));
    }

    #[tokio::test]
    async fn every_mutator_emits_exactly_the_expected_events() {
        let temp = tempdir().unwrap();
        let mut config = ServerConfig::default();
        config.take_catalog_path = temp.path().join("takes.json");
        let recording_path = temp.path().join("take-001.cmtrk");
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();
        let host = Some(server.host_session_id());
        let mut rx = server.subscribe_state_events();
        let mut all_seqs: Vec<u64> = Vec::new();

        macro_rules! expect_events {
            ($($pattern:pat),+ $(,)?) => {{
                let events = drain_events(&mut rx);
                for envelope in &events {
                    assert_eq!(envelope.origin_session, host);
                    all_seqs.push(envelope.seq);
                }
                let mut iter = events.iter();
                $(
                    let envelope = iter.next().expect("missing expected event");
                    assert!(
                        matches!(&envelope.event, $pattern),
                        "unexpected event {:?}, expected {}",
                        envelope.event,
                        stringify!($pattern),
                    );
                )+
                assert!(iter.next().is_none(), "extra events emitted: {events:?}");
                events
            }};
        }

        // load_scene
        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        expect_events!(StateEvent::SceneLoaded { .. });

        // set_active_scene
        server.set_active_scene(scene_id).await.unwrap();
        expect_events!(StateEvent::SceneActivated { .. });

        // set_data_flow
        server.set_data_flow(DataFlowState::Live).await.unwrap();
        expect_events!(StateEvent::ModeChanged { mode: Mode::LIVE });

        // set_recording
        server
            .set_recording(RecordingState::Recording)
            .await
            .unwrap();
        expect_events!(StateEvent::ModeChanged {
            mode: Mode::RECORDING
        });
        server
            .set_recording(RecordingState::Inactive)
            .await
            .unwrap();
        expect_events!(StateEvent::ModeChanged { mode: Mode::LIVE });

        // create_mapping
        let mapping_id = server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();
        let events = expect_events!(StateEvent::MappingCreated { .. });
        match &events[0].event {
            StateEvent::MappingCreated { mapping } => {
                assert_eq!(mapping.mapping_id, mapping_id);
                assert_eq!(mapping.source_device, device_id);
                assert_eq!(mapping.source_output, "pose_pos");
                assert_eq!(mapping.target_scene, scene_id);
                assert_eq!(mapping.target_object, object_id);
                assert_eq!(mapping.target_attribute, "position");
                assert_eq!(mapping.component_mask, None);
                assert!(!mapping.lock);
            }
            other => panic!("expected MappingCreated, got {other:?}"),
        }

        // update_mapping
        server
            .update_mapping(
                mapping_id,
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: Some(vec![0, 1]),
                },
                101,
            )
            .await
            .unwrap();
        expect_events!(StateEvent::MappingUpdated { .. });

        // set_mapping_lock
        server.set_mapping_lock(mapping_id, true).await.unwrap();
        expect_events!(StateEvent::MappingLockChanged { lock: true, .. });
        server.set_mapping_lock(mapping_id, false).await.unwrap();
        expect_events!(StateEvent::MappingLockChanged { lock: false, .. });

        // baseline actions
        server.commit_scene_baseline(Some(scene_id)).await.unwrap();
        expect_events!(StateEvent::BaselineApplied {
            action: BaselineAction::CommitScene,
            ..
        });
        server
            .reset_scene_to_baseline(Some(scene_id))
            .await
            .unwrap();
        expect_events!(StateEvent::BaselineApplied {
            action: BaselineAction::ResetScene,
            ..
        });
        server
            .commit_object_baseline(Some(scene_id), object_id)
            .await
            .unwrap();
        expect_events!(StateEvent::BaselineApplied {
            action: BaselineAction::CommitObject,
            ..
        });

        // start_recording: mode transition + recording start
        let recording_id = server.start_recording(&recording_path, 102).await.unwrap();
        let events = expect_events!(
            StateEvent::ModeChanged {
                mode: Mode::RECORDING
            },
            StateEvent::RecordingStarted { .. },
        );
        assert!(matches!(
            &events[1].event,
            StateEvent::RecordingStarted { take_id, scene_id: sid }
                if *take_id == recording_id && *sid == scene_id
        ));

        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                103,
            )
            .await
            .unwrap();
        assert!(drain_events(&mut rx).is_empty(), "data plane emits no events");

        // stop_recording: stop + take registration + mode transition
        let manifest = server.stop_recording().await.unwrap();
        let events = expect_events!(
            StateEvent::RecordingStopped { .. },
            StateEvent::TakeRegistered { .. },
            StateEvent::ModeChanged { mode: Mode::LIVE },
        );
        assert!(matches!(
            &events[0].event,
            StateEvent::RecordingStopped { take_id, frame_count }
                if *take_id == manifest.recording_id && *frame_count == 1
        ));
        assert!(matches!(
            &events[1].event,
            StateEvent::TakeRegistered { take } if take.take_id == manifest.recording_id
        ));

        // select_take
        server.select_take(manifest.recording_id).await.unwrap();
        expect_events!(StateEvent::TakeSelected { .. });

        // playback controls
        server
            .playback_play(manifest.recording_id, false)
            .await
            .unwrap();
        expect_events!(
            StateEvent::ModeChanged {
                mode: Mode::PLAYBACK
            },
            StateEvent::PlaybackChanged {
                state: PlaybackRuntimeState::Playing,
                ..
            },
        );
        server
            .playback_pause(manifest.recording_id)
            .await
            .unwrap();
        expect_events!(StateEvent::PlaybackChanged {
            state: PlaybackRuntimeState::Paused,
            ..
        });
        server
            .playback_seek(manifest.recording_id, 0, false)
            .await
            .unwrap();
        expect_events!(StateEvent::PlaybackChanged { .. });
        server.playback_stop(manifest.recording_id).await.unwrap();
        expect_events!(
            StateEvent::ModeChanged { mode: Mode::LIVE },
            StateEvent::PlaybackChanged {
                state: PlaybackRuntimeState::Stopped,
                ..
            },
        );

        // remove_mapping
        server.remove_mapping(mapping_id).await.unwrap();
        expect_events!(StateEvent::MappingRemoved { .. });

        // delete_take
        server.delete_take(manifest.recording_id).await.unwrap();
        expect_events!(StateEvent::TakeDeleted { .. });

        // Every event seq across the whole run is strictly monotonic and
        // contiguous (emitted under the state write lock).
        assert!(!all_seqs.is_empty());
        for pair in all_seqs.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "seq gap or reorder: {all_seqs:?}");
        }
    }

    #[tokio::test]
    async fn start_recording_without_active_scene_mutates_nothing_and_emits_nothing() {
        let (config, temp) = config_with_temp_catalog();
        let server = ServerHandle::new(config);
        let mut rx = server.subscribe_state_events();
        let seq_before = server.current_event_seq().await;
        assert_eq!(server.mode().await, Mode::IDLE);

        let err = server
            .start_recording(temp.path().join("never.cmtrk"), 100)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no active scene"));

        // The failed precondition left the authoritative state untouched and
        // the event stream silent: no half-applied Live/Recording mode.
        assert_eq!(server.mode().await, Mode::IDLE);
        assert_eq!(server.current_event_seq().await, seq_before);
        assert!(drain_events(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn start_recording_over_active_playback_emits_terminal_playback_stopped() {
        let (config, temp) = config_with_temp_catalog();
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();
        server
            .start_recording(temp.path().join("take-001.cmtrk"), 101)
            .await
            .unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                102,
            )
            .await
            .unwrap();
        let manifest = server.stop_recording().await.unwrap();
        server
            .playback_play(manifest.recording_id, true)
            .await
            .unwrap();

        let mut rx = server.subscribe_state_events();
        server
            .start_recording(temp.path().join("take-002.cmtrk"), 200)
            .await
            .unwrap();

        // Discarding the loaded playback is replicated like every other
        // playback-terminating path, before the mode flips to recording.
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 3, "events: {events:?}");
        assert!(matches!(
            &events[0].event,
            StateEvent::PlaybackChanged {
                state: PlaybackRuntimeState::Stopped,
                take_id,
                looping: true,
                ..
            } if *take_id == manifest.recording_id
        ));
        assert!(matches!(
            &events[1].event,
            StateEvent::ModeChanged {
                mode: Mode::RECORDING
            }
        ));
        assert!(matches!(&events[2].event, StateEvent::RecordingStarted { .. }));
        assert!(server.playback_status().await.is_none());

        server.stop_recording().await.unwrap();
    }

    #[tokio::test]
    async fn scene_snapshot_recovers_sessions_takes_and_playback() {
        let (config, temp) = config_with_temp_catalog();
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();
        server
            .start_recording(temp.path().join("take-001.cmtrk"), 101)
            .await
            .unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                102,
            )
            .await
            .unwrap();
        let manifest = server.stop_recording().await.unwrap();
        server
            .playback_play(manifest.recording_id, true)
            .await
            .unwrap();

        // One session all the way to Active (registered), one merely
        // discovered (no session_id — must not appear in the snapshot).
        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::MotionSource],
                features: vec![Feature::Motion],
                advertised_attributes: vec![AttributeDescriptor {
                    path: "pose_pos".into(),
                    value_type: AttributeKind::Vec3f,
                }],
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        let accepted = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();

        let ghost_device = Uuid::now_v7();
        server.discovered(ghost_device, "ghost").await.unwrap();

        let payload = server.scene_snapshot_payload().await;

        // Sessions: exactly the registered set (host + ipad), no ghost.
        assert_eq!(payload.sessions.len(), 2, "sessions: {:?}", payload.sessions);
        let host = payload
            .sessions
            .iter()
            .find(|s| s.is_host)
            .expect("host session in snapshot");
        assert_eq!(host.session_id, server.host_session_id());
        assert_eq!(host.device_name, "host");
        let client = payload
            .sessions
            .iter()
            .find(|s| !s.is_host)
            .expect("registered client in snapshot");
        assert_eq!(client.session_id, accepted.session_id);
        assert_eq!(client.device_id, device_id);
        assert_eq!(client.device_name, "ipad");
        assert_eq!(client.roles, vec![ClientRole::MotionSource]);
        assert!(!payload.sessions.iter().any(|s| s.device_id == ghost_device));

        // Takes: the registered catalog.
        assert_eq!(payload.takes.len(), 1);
        assert_eq!(payload.takes[0].take_id, manifest.recording_id);

        // Playback: the loaded transport.
        let playback = payload.playback.expect("playback in snapshot");
        assert_eq!(playback.state, PlaybackRuntimeState::Playing);
        assert_eq!(playback.take_id, manifest.recording_id);
        assert!(playback.looping);

        // Full payload still round-trips serde.
        let encoded = serde_json::to_string(&payload).unwrap();
        let decoded: motionstage_protocol::SceneSnapshotPayload =
            serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, payload);

        // A session closed after registration leaves the snapshot again.
        server.close_session(device_id, now_ns()).await.unwrap();
        let payload = server.scene_snapshot_payload().await;
        assert_eq!(payload.sessions.len(), 1);
        assert!(payload.sessions[0].is_host);
    }

    /// Register + activate a session for `device_id` and return its session id.
    async fn register_and_activate(server: &ServerHandle, device_id: Uuid) -> Uuid {
        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::Operator],
                features: vec![Feature::Mapping],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        let accepted = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();
        accepted.session_id
    }

    #[tokio::test]
    async fn superseding_reconnect_emits_old_session_left_only_after_admission() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();
        let first_session = register_and_activate(&server, device_id).await;

        // Reconnect while the old registered session is still half-open. The
        // pre-admission steps of the new connection must NOT retire the old
        // session on the event stream — a free takeover of a live session is
        // exactly the attack we are closing.
        let mut rx = server.subscribe_state_events();
        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::Operator],
                features: vec![Feature::Mapping],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        assert!(
            drain_events(&mut rx).is_empty(),
            "old session must not be retired before the new one is admitted"
        );

        // Only once the superseding connection passes register() (admission)
        // does the old session's SessionLeft fire.
        server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "events: {events:?}");
        assert!(matches!(
            &events[0].event,
            StateEvent::SessionLeft { session_id, reason }
                if *session_id == first_session
                    && reason.as_deref() == Some("superseded by reconnect")
        ));
        assert_eq!(events[0].origin_session, Some(first_session));
    }

    #[tokio::test]
    async fn failed_admission_reconnect_does_not_retire_live_session() {
        // Credentialed mode: a reconnect that does NOT satisfy admission (wrong
        // pairing token) must not evict the live session from the event stream.
        let mut config = ServerConfig::default();
        config.security_mode = SecurityMode::PairingRequired;
        config.pairing_token = Some("secret".into());
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        // Admit the victim with the correct credential.
        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::Operator],
                features: vec![Feature::Mapping],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        let victim = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: Some("secret".into()),
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();

        // A reconnect claiming the same device_id but WITHOUT the credential.
        let mut rx = server.subscribe_state_events();
        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::Operator],
                features: vec![Feature::Mapping],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        let rejected = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: Some("wrong".into()),
                    api_key: None,
                },
            )
            .await;
        match rejected {
            Err(ServerError::RegisterRejected(rejected)) => {
                assert_eq!(rejected.code, RejectCode::AuthFailed);
            }
            other => panic!("expected AuthFailed RegisterRejected, got {other:?}"),
        }
        // The victim's SessionLeft was never emitted: failed admission is not a
        // takeover of the live session on the replicated stream.
        let victim_session = victim.session_id;
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|envelope| matches!(
                &envelope.event,
                StateEvent::SessionLeft { session_id, .. } if *session_id == victim_session
            )),
            "failed-admission reconnect must not retire the live session: {events:?}"
        );
    }

    #[tokio::test]
    async fn rediscovery_of_closed_or_unregistered_session_emits_nothing() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();
        let mut rx = server.subscribe_state_events();

        // Re-discovering a session that never registered emits nothing: it
        // never joined the event stream, so it has nothing to leave.
        server.discovered(device_id, "ipad").await.unwrap();
        assert!(drain_events(&mut rx).is_empty());
        server.discovered(device_id, "ipad").await.unwrap();
        assert!(drain_events(&mut rx).is_empty());

        // Discovering a brand-new device emits nothing.
        let fresh_device = Uuid::now_v7();
        server.discovered(fresh_device, "fresh").await.unwrap();
        assert!(drain_events(&mut rx).is_empty());

        // A record already Closed had its SessionLeft emitted at close time;
        // rediscovery must not emit a second one.
        let _session = register_and_activate(&server, device_id).await;
        let _ = drain_events(&mut rx);
        server.close_session(device_id, now_ns()).await.unwrap();
        let closed_events = drain_events(&mut rx);
        assert_eq!(closed_events.len(), 1, "events: {closed_events:?}");
        assert!(matches!(
            &closed_events[0].event,
            StateEvent::SessionLeft { reason, .. } if reason.is_none()
        ));
        server.discovered(device_id, "ipad").await.unwrap();
        assert!(drain_events(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn delete_take_emits_event_even_when_file_removal_fails() {
        let (config, temp) = config_with_temp_catalog();
        let recording_path = temp.path().join("take-001.cmtrk");
        let server = ServerHandle::new(config);
        let device_id = Uuid::now_v7();

        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();
        server.start_recording(&recording_path, 101).await.unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                102,
            )
            .await
            .unwrap();
        let manifest = server.stop_recording().await.unwrap();

        // Sabotage the filesystem removal: a directory at the recording path
        // makes fs::remove_file fail with a non-NotFound error.
        std::fs::remove_file(&recording_path).unwrap();
        std::fs::create_dir(&recording_path).unwrap();

        let mut rx = server.subscribe_state_events();
        let err = server.delete_take(manifest.recording_id).await.unwrap_err();
        assert!(matches!(err, ServerError::Take(_)), "got {err:?}");

        // The catalog mutation was replicated before the fs failure surfaced.
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "events: {events:?}");
        assert!(matches!(
            &events[0].event,
            StateEvent::TakeDeleted { take_id } if *take_id == manifest.recording_id
        ));

        // The take is tombstoned: gone from listings and from the snapshot.
        assert!(server.list_takes(None).await.is_empty());
        assert!(server.scene_snapshot_payload().await.takes.is_empty());
    }

    #[tokio::test]
    async fn session_lifecycle_emits_joined_and_left_events() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();
        let mut rx = server.subscribe_state_events();

        server.discovered(device_id, "ipad").await.unwrap();
        server.transport_connected(device_id).await.unwrap();
        server
            .hello_exchanged(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: "ipad".into(),
                roles: vec![ClientRole::Operator],
                features: vec![Feature::Mapping],
                advertised_attributes: Vec::new(),
            })
            .await
            .unwrap();
        server.authenticate(device_id).await.unwrap();
        let accepted = server
            .register(
                device_id,
                RegisterRequest {
                    pairing_token: None,
                    api_key: None,
                },
            )
            .await
            .unwrap();
        server.scene_synced(device_id).await.unwrap();
        server.activate(device_id).await.unwrap();
        server
            .close_session_with_reason(device_id, now_ns(), Some("goodbye".into()))
            .await
            .unwrap();

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 2, "events: {events:?}");
        assert!(matches!(
            &events[0].event,
            StateEvent::SessionJoined { session_id, device_name, .. }
                if *session_id == accepted.session_id && device_name == "ipad"
        ));
        assert_eq!(events[0].origin_session, Some(accepted.session_id));
        assert!(matches!(
            &events[1].event,
            StateEvent::SessionLeft { session_id, reason }
                if *session_id == accepted.session_id
                    && reason.as_deref() == Some("goodbye")
        ));
    }

    #[tokio::test]
    async fn interleaved_origins_keep_seq_strictly_monotonic() {
        let server = ServerHandle::new(ServerConfig::default());
        let other_session = Uuid::new_v4();
        let mut rx = server.subscribe_state_events();

        for i in 0..20 {
            if i % 2 == 0 {
                server.set_data_flow(DataFlowState::Live).await.unwrap();
            } else {
                server
                    .set_data_flow_from(DataFlowState::Idle, Some(other_session))
                    .await
                    .unwrap();
            }
        }

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 20);
        for pair in events.windows(2) {
            assert!(pair[1].seq > pair[0].seq);
            assert_eq!(pair[1].seq, pair[0].seq + 1);
        }
        assert_eq!(
            events[0].origin_session,
            Some(server.host_session_id())
        );
        assert_eq!(events[1].origin_session, Some(other_session));
    }

    #[tokio::test]
    async fn resync_replays_exactly_the_missing_gap() {
        let server = ServerHandle::new(ServerConfig::default());
        let (scene, scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;
        server.set_active_scene(scene_id).await.unwrap();
        let last_seen = server.current_event_seq().await;

        server.set_data_flow(DataFlowState::Live).await.unwrap();
        server.commit_scene_baseline(Some(scene_id)).await.unwrap();
        let current = server.current_event_seq().await;
        assert_eq!(current, last_seen + 2);

        match server.resync_from(last_seen).await {
            ResyncResponse::Replay(events) => {
                let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
                assert_eq!(seqs, vec![last_seen + 1, last_seen + 2]);
                assert!(matches!(
                    events[0].event,
                    StateEvent::ModeChanged { mode: Mode::LIVE }
                ));
                assert!(matches!(
                    events[1].event,
                    StateEvent::BaselineApplied { .. }
                ));
            }
            other => panic!("expected replay, got {other:?}"),
        }

        // Fully caught up: empty replay.
        match server.resync_from(current).await {
            ResyncResponse::Replay(events) => assert!(events.is_empty()),
            other => panic!("expected empty replay, got {other:?}"),
        }

        // A seq from the future (another server epoch) forces a snapshot.
        assert!(matches!(
            server.resync_from(current + 100).await,
            ResyncResponse::Snapshot(_)
        ));
    }

    #[tokio::test]
    async fn resync_falls_back_to_snapshot_when_gap_left_ring_buffer() {
        let server = ServerHandle::new(ServerConfig::default());
        // Push well past the ring buffer capacity (1024).
        for _ in 0..1100 {
            server.set_data_flow(DataFlowState::Live).await.unwrap();
        }
        match server.resync_from(0).await {
            ResyncResponse::Snapshot(payload) => {
                assert_eq!(payload.seq, server.current_event_seq().await);
            }
            other => panic!("expected snapshot fallback, got {other:?}"),
        }
        // A recent seq is still replayable.
        let current = server.current_event_seq().await;
        match server.resync_from(current - 5).await {
            ResyncResponse::Replay(events) => assert_eq!(events.len(), 5),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn scene_snapshot_payload_carries_world_and_round_trips_serde() {
        let server = ServerHandle::new(ServerConfig::default());
        let device_id = Uuid::now_v7();
        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        server
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();
        server.set_data_flow(DataFlowState::Live).await.unwrap();
        server
            .ingest_motion_samples(
                device_id,
                vec![AttributeUpdate {
                    output_attribute: "pose_pos".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
                101,
            )
            .await
            .unwrap();

        let payload = server.scene_snapshot_payload().await;
        assert_eq!(payload.mode, Mode::LIVE);
        assert_eq!(payload.active_scene, Some(scene_id));
        assert_eq!(payload.seq, server.current_event_seq().await);
        assert!(payload.seq > 0);

        assert_eq!(payload.scenes.len(), 1);
        let scene = &payload.scenes[0];
        assert_eq!(scene.scene_id, scene_id);
        assert_eq!(scene.name, "shot");
        assert_eq!(scene.objects.len(), 1);
        let object = &scene.objects[0];
        assert_eq!(object.object_id, object_id);
        assert_eq!(object.name, "camera");
        let attr = &object.attributes[0];
        assert_eq!(attr.name, "position");
        assert_eq!(attr.default_value, BakeAttributeValue::Vec3f([10.0, 20.0, 30.0]));
        // Relative composition applied the delta onto the baseline.
        assert_eq!(attr.current_value, BakeAttributeValue::Vec3f([11.0, 22.0, 33.0]));
        assert!(attr.live_enabled);
        assert!(attr.record_enabled);

        assert_eq!(payload.mappings.len(), 1);
        assert_eq!(payload.mappings[0].source_device, device_id);
        assert_eq!(payload.mappings[0].target_object, object_id);

        let encoded = serde_json::to_string(&payload).unwrap();
        let decoded: motionstage_protocol::SceneSnapshotPayload =
            serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[tokio::test]
    async fn lagged_event_receiver_is_resynced_with_snapshot() {
        let server = ServerHandle::new(ServerConfig::default());
        let mut rx = server.subscribe_state_events();
        // Overrun the broadcast capacity (256) without draining.
        for _ in 0..300 {
            server.set_data_flow(DataFlowState::Live).await.unwrap();
        }
        let lagged = rx.recv().await;
        assert!(matches!(
            lagged,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
        ));
        match event_delivery_message(&server, lagged).await {
            Some(ControlMessage::SceneSnapshot(payload)) => {
                assert_eq!(payload.seq, server.current_event_seq().await);
            }
            other => panic!("expected snapshot on lag, got {other:?}"),
        }
        // A healthy receive forwards the envelope unchanged.
        let ok = rx.recv().await;
        match event_delivery_message(&server, ok).await {
            Some(ControlMessage::StateEventMsg(_)) => {}
            other => panic!("expected forwarded event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_sends_scene_snapshot_before_activation() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);

        let (scene, scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;

        let runtime = server.start_quic_runtime().await.unwrap();
        let client = QuicClient::new_insecure_for_local_dev().unwrap();
        let peer = client.connect(runtime.local_addr).await.unwrap();
        let mut control = peer.accept_control_stream().await.unwrap();

        assert!(matches!(
            control.recv().await.unwrap(),
            ControlMessage::ServerHello(_)
        ));
        control
            .send(&ControlMessage::ClientHello(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id: Uuid::now_v7(),
                device_name: "peer".into(),
                roles: vec![ClientRole::Operator],
                features: vec![Feature::Mapping],
                advertised_attributes: Vec::new(),
            }))
            .await
            .unwrap();
        control
            .send(&ControlMessage::RegisterRequest(RegisterRequest {
                pairing_token: None,
                api_key: None,
            }))
            .await
            .unwrap();

        assert!(matches!(
            control.recv().await.unwrap(),
            ControlMessage::RegisterAccepted(_)
        ));
        match control.recv().await.unwrap() {
            ControlMessage::SceneSnapshot(payload) => {
                assert_eq!(payload.scenes.len(), 1);
                assert_eq!(payload.scenes[0].scene_id, scene_id);
                assert_eq!(payload.active_scene, Some(scene_id));
                assert_eq!(payload.mode, Mode::IDLE);
            }
            other => panic!("expected SceneSnapshot after registration, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resync_request_over_wire_replays_from_ring_buffer() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        control
            .send(&ControlMessage::ResyncRequest { last_seq: 0 })
            .await
            .unwrap();

        // Seq 1 (the host SessionJoined) is only observable via replay: the
        // live stream started after it.
        let replayed = recv_until(&mut control, |message| {
            matches!(
                message,
                ControlMessage::StateEventMsg(StateEventEnvelope { seq: 1, .. })
            )
        })
        .await;
        match replayed {
            ControlMessage::StateEventMsg(envelope) => {
                assert!(matches!(
                    &envelope.event,
                    StateEvent::SessionJoined { device_name, .. } if device_name == "host"
                ));
            }
            other => panic!("expected replayed event, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn remote_mutation_is_replicated_to_other_session_without_polling() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer_a, mut control_a) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;
        let (_peer_b, mut control_b) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        // A host-side mutation reaches both wire sessions as an event.
        server.commit_scene_baseline(Some(scene_id)).await.unwrap();
        for control in [&mut control_a, &mut control_b] {
            let observed = recv_until(control, |message| {
                matches!(
                    message,
                    ControlMessage::StateEventMsg(StateEventEnvelope {
                        event: StateEvent::BaselineApplied { .. },
                        ..
                    })
                )
            })
            .await;
            match observed {
                ControlMessage::StateEventMsg(envelope) => {
                    assert_eq!(envelope.origin_session, Some(server.host_session_id()));
                }
                other => panic!("expected BaselineApplied event, got {other:?}"),
            }
        }

        runtime.shutdown().await.unwrap();
    }

    /// Open a connection and drive the handshake up to (and including) the
    /// hello with an explicit protocol minor; return the next server message
    /// after sending RegisterRequest.
    async fn handshake_with_hello(
        addr: SocketAddr,
        hello: ClientHello,
    ) -> (QuicPeer, ControlChannel, ControlMessage) {
        let client = QuicClient::new_insecure_for_local_dev().unwrap();
        let peer = client.connect(addr).await.unwrap();
        let mut control = peer.accept_control_stream().await.unwrap();
        assert!(matches!(
            control.recv().await.unwrap(),
            ControlMessage::ServerHello(_)
        ));
        control
            .send(&ControlMessage::ClientHello(hello))
            .await
            .unwrap();
        control
            .send(&ControlMessage::RegisterRequest(RegisterRequest {
                pairing_token: None,
                api_key: None,
            }))
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), control.recv())
            .await
            .expect("handshake reply within timeout")
            .unwrap();
        (peer, control, reply)
    }

    fn motion_hello(device_id: Uuid, minor: u16, roles: Vec<ClientRole>) -> ClientHello {
        ClientHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: minor,
            device_id,
            device_name: format!("peer-{device_id}"),
            roles,
            features: vec![Feature::Motion],
            advertised_attributes: vec![AttributeDescriptor {
                path: "pose_pos".into(),
                value_type: AttributeKind::Vec3f,
            }],
        }
    }

    #[tokio::test]
    async fn register_accepted_echoes_server_minor_for_older_client() {
        assert!(
            PROTOCOL_MINOR >= 1,
            "test requires a server minor an older client can undershoot"
        );
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        // Truth-in-docs: an older-minor client registers, but is told the
        // server's own minor — the server speaks exactly its own minor and
        // does not downgrade behaviour for the lesser client.
        let older = PROTOCOL_MINOR - 1;
        let (_peer, _control, reply) = handshake_with_hello(
            runtime.local_addr,
            motion_hello(Uuid::now_v7(), older, vec![ClientRole::MotionSource]),
        )
        .await;
        match reply {
            ControlMessage::RegisterAccepted(accepted) => {
                assert_eq!(accepted.negotiated_protocol_minor, PROTOCOL_MINOR);
            }
            other => panic!("expected RegisterAccepted, got {other:?}"),
        }

        // A current-minor client also gets the server's minor.
        let (_peer2, _control2, reply2) = handshake_with_hello(
            runtime.local_addr,
            motion_hello(Uuid::now_v7(), PROTOCOL_MINOR, vec![ClientRole::MotionSource]),
        )
        .await;
        match reply2 {
            ControlMessage::RegisterAccepted(accepted) => {
                assert_eq!(accepted.negotiated_protocol_minor, PROTOCOL_MINOR);
            }
            other => panic!("expected RegisterAccepted, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn foreign_major_gets_typed_register_rejected_before_handshake_drop() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        // The server requires its own major: a foreign major is rejected with
        // a typed RegisterRejected{UnsupportedProtocol}, not a bare Error.
        let mut hello = motion_hello(Uuid::now_v7(), 0, vec![ClientRole::MotionSource]);
        hello.protocol_major = PROTOCOL_MAJOR + 1;
        let (_peer, _control, reply) = handshake_with_hello(runtime.local_addr, hello).await;
        match reply {
            ControlMessage::RegisterRejected(rejected) => {
                assert_eq!(rejected.code, RejectCode::UnsupportedProtocol);
                assert!(
                    rejected.reason.contains("unsupported major"),
                    "reason: {}",
                    rejected.reason
                );
            }
            other => panic!("expected typed RegisterRejected before drop, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn empty_roles_get_typed_reject_before_handshake_drop() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer, _control, reply) = handshake_with_hello(
            runtime.local_addr,
            motion_hello(Uuid::now_v7(), PROTOCOL_MINOR, Vec::new()),
        )
        .await;
        match reply {
            ControlMessage::RegisterRejected(rejected) => {
                assert_eq!(rejected.code, RejectCode::RoleDenied);
                assert!(rejected.reason.contains("at least one role"));
            }
            other => panic!("expected typed RegisterRejected before drop, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wire_create_mapping_defaults_to_own_device_and_active_scene() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let device_id = Uuid::now_v7();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            device_id,
            ClientRole::MotionSource,
            Feature::Motion,
        )
        .await;
        let (_peer_b, mut control_b) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        control
            .send(&ControlMessage::CreateMapping {
                source_device: None,
                source_output: "pose_pos".into(),
                target_scene: None,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: Some(vec![0, 2]),
            })
            .await
            .unwrap();

        let mapping_id = match recv_skipping_events(&mut control).await {
            ControlMessage::MappingCreateResult { result: Ok(summary) } => {
                assert_eq!(summary.source_device, device_id, "None = own device");
                assert_eq!(summary.target_scene, scene_id, "None = active scene");
                assert_eq!(summary.target_object, object_id);
                assert_eq!(summary.component_mask, Some(vec![0, 2]));
                assert!(!summary.lock);
                summary.mapping_id
            }
            other => panic!("expected MappingCreateResult Ok, got {other:?}"),
        };

        // The mutation replicates to the other session with the originating
        // session stamped (the bus covers wire mapping ops).
        let observed = recv_until(&mut control_b, |message| {
            matches!(
                message,
                ControlMessage::StateEventMsg(StateEventEnvelope {
                    event: StateEvent::MappingCreated { .. },
                    ..
                })
            )
        })
        .await;
        match observed {
            ControlMessage::StateEventMsg(envelope) => {
                assert!(envelope.origin_session.is_some());
                assert_ne!(envelope.origin_session, Some(server.host_session_id()));
                assert!(matches!(
                    envelope.event,
                    StateEvent::MappingCreated { mapping } if mapping.mapping_id == mapping_id
                ));
            }
            other => panic!("expected MappingCreated event, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wire_create_mapping_for_foreign_device_is_denied_without_operator() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, _scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::MotionSource,
            Feature::Motion,
        )
        .await;

        let seq_before = server.current_event_seq().await;
        control
            .send(&ControlMessage::CreateMapping {
                source_device: Some(Uuid::now_v7()), // someone else's device
                source_output: "pose_pos".into(),
                target_scene: None,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingCreateResult { result: Err(err) } => {
                assert_eq!(err.code, RejectCode::RoleDenied);
                assert!(err.reason.contains("own device"), "reason: {}", err.reason);
            }
            other => panic!("expected MappingCreateResult Err, got {other:?}"),
        }

        // Denied: nothing mutated, nothing emitted.
        assert!(server.runtime_snapshot().await.mappings.is_empty());
        assert_eq!(server.current_event_seq().await, seq_before);

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn operator_manages_any_mapping_over_wire() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let foreign_device = Uuid::now_v7();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        // Create for a foreign source device.
        control
            .send(&ControlMessage::CreateMapping {
                source_device: Some(foreign_device),
                source_output: "pose_pos".into(),
                target_scene: Some(scene_id),
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            })
            .await
            .unwrap();
        let mapping_id = match recv_skipping_events(&mut control).await {
            ControlMessage::MappingCreateResult { result: Ok(summary) } => {
                assert_eq!(summary.source_device, foreign_device);
                summary.mapping_id
            }
            other => panic!("expected MappingCreateResult Ok, got {other:?}"),
        };

        // Update it (change the component mask).
        control
            .send(&ControlMessage::UpdateMapping {
                mapping_id,
                source_device: Some(foreign_device),
                source_output: "pose_pos".into(),
                target_scene: Some(scene_id),
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: Some(vec![1]),
            })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingCreateResult { result: Ok(summary) } => {
                assert_eq!(summary.mapping_id, mapping_id);
                assert_eq!(summary.component_mask, Some(vec![1]));
            }
            other => panic!("expected MappingCreateResult Ok for update, got {other:?}"),
        }

        // Lock, unlock, remove.
        control
            .send(&ControlMessage::SetMappingLock {
                mapping_id,
                lock: true,
            })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingOpResult {
                mapping_id: id,
                result: Ok(()),
            } => assert_eq!(id, mapping_id),
            other => panic!("expected MappingOpResult Ok for lock, got {other:?}"),
        }
        control
            .send(&ControlMessage::SetMappingLock {
                mapping_id,
                lock: false,
            })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingOpResult { result: Ok(()), .. } => {}
            other => panic!("expected MappingOpResult Ok for unlock, got {other:?}"),
        }
        control
            .send(&ControlMessage::RemoveMapping { mapping_id })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingOpResult { result: Ok(()), .. } => {}
            other => panic!("expected MappingOpResult Ok for remove, got {other:?}"),
        }
        assert!(server.runtime_snapshot().await.mappings.is_empty());

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn non_owner_cannot_manage_foreign_mapping_over_wire() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        let foreign_device = Uuid::now_v7();
        let mapping_id = server
            .create_mapping(
                MappingRequest {
                    source_device: foreign_device,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .await
            .unwrap();
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::MotionSource,
            Feature::Motion,
        )
        .await;

        let seq_before = server.current_event_seq().await;
        control
            .send(&ControlMessage::RemoveMapping { mapping_id })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingOpResult {
                result: Err(err), ..
            } => assert_eq!(err.code, RejectCode::RoleDenied),
            other => panic!("expected denied MappingOpResult, got {other:?}"),
        }

        control
            .send(&ControlMessage::SetMappingLock {
                mapping_id,
                lock: true,
            })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingOpResult {
                result: Err(err), ..
            } => assert_eq!(err.code, RejectCode::RoleDenied),
            other => panic!("expected denied MappingOpResult, got {other:?}"),
        }

        control
            .send(&ControlMessage::UpdateMapping {
                mapping_id,
                source_device: None,
                source_output: "pose_pos".into(),
                target_scene: Some(scene_id),
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: Some(vec![0]),
            })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingCreateResult { result: Err(err) } => {
                assert_eq!(err.code, RejectCode::RoleDenied)
            }
            other => panic!("expected denied MappingCreateResult, got {other:?}"),
        }

        // Nothing mutated, nothing emitted.
        let snapshot = server.runtime_snapshot().await;
        let mapping = snapshot.mappings.get(&mapping_id).expect("mapping intact");
        assert_eq!(mapping.source_device, foreign_device);
        assert!(!mapping.lock);
        assert_eq!(mapping.component_mask, None);
        assert_eq!(server.current_event_seq().await, seq_before);

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn own_source_session_manages_its_own_mapping_over_wire() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, scene_id, object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let device_id = Uuid::now_v7();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            device_id,
            ClientRole::MotionSource,
            Feature::Motion,
        )
        .await;

        control
            .send(&ControlMessage::CreateMapping {
                source_device: None,
                source_output: "pose_pos".into(),
                target_scene: Some(scene_id),
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            })
            .await
            .unwrap();
        let mapping_id = match recv_skipping_events(&mut control).await {
            ControlMessage::MappingCreateResult { result: Ok(summary) } => summary.mapping_id,
            other => panic!("expected MappingCreateResult Ok, got {other:?}"),
        };

        control
            .send(&ControlMessage::UpdateMapping {
                mapping_id,
                source_device: None,
                source_output: "pose_pos".into(),
                target_scene: Some(scene_id),
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: Some(vec![0, 1]),
            })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingCreateResult { result: Ok(summary) } => {
                assert_eq!(summary.component_mask, Some(vec![0, 1]));
            }
            other => panic!("expected MappingCreateResult Ok, got {other:?}"),
        }

        control
            .send(&ControlMessage::RemoveMapping { mapping_id })
            .await
            .unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::MappingOpResult { result: Ok(()), .. } => {}
            other => panic!("expected MappingOpResult Ok, got {other:?}"),
        }
        assert!(server.runtime_snapshot().await.mappings.is_empty());

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wire_take_control_assigns_server_owned_identity() {
        let (mut config, temp) = config_with_temp_catalog();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::Operator,
            Feature::Recording,
        )
        .await;

        control.send(&ControlMessage::StartTake).await.unwrap();
        let take_id = match recv_skipping_events(&mut control).await {
            ControlMessage::TakeStartResult { result: Ok(take_id) } => take_id,
            other => panic!("expected TakeStartResult Ok, got {other:?}"),
        };

        control.send(&ControlMessage::StopTake).await.unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::TakeStopResult { result: Ok(take) } => {
                assert_eq!(take.take_id, take_id);
                assert_eq!(take.scene_id, scene_id);
                assert_eq!(take.name, "Take 001");
                // Server-assigned identity: the path lives in the take-catalog
                // directory and was never supplied by the client.
                let path = std::path::Path::new(&take.path);
                assert_eq!(path.parent(), Some(temp.path()));
                assert!(path.exists(), "recording file written at {path:?}");
            }
            other => panic!("expected TakeStopResult Ok, got {other:?}"),
        }

        // The take is in the catalog and the mode returned to Live.
        let takes = server.list_takes(None).await;
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0].take_id, take_id);
        assert_eq!(server.mode().await, Mode::LIVE);

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn take_control_requires_operator_role() {
        let (mut config, _temp) = config_with_temp_catalog();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, _scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::MotionSource,
            Feature::Motion,
        )
        .await;

        let seq_before = server.current_event_seq().await;
        control.send(&ControlMessage::StartTake).await.unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::TakeStartResult { result: Err(err) } => {
                assert_eq!(err.code, RejectCode::RoleDenied);
            }
            other => panic!("expected denied TakeStartResult, got {other:?}"),
        }
        control.send(&ControlMessage::StopTake).await.unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::TakeStopResult { result: Err(err) } => {
                assert_eq!(err.code, RejectCode::RoleDenied);
            }
            other => panic!("expected denied TakeStopResult, got {other:?}"),
        }
        assert_eq!(server.mode().await, Mode::IDLE);
        assert_eq!(server.current_event_seq().await, seq_before);

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn get_scene_snapshot_returns_on_demand_world() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let (scene, scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;
        let runtime = server.start_quic_runtime().await.unwrap();

        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            Uuid::now_v7(),
            ClientRole::MotionSource,
            Feature::Motion,
        )
        .await;

        control.send(&ControlMessage::GetSceneSnapshot).await.unwrap();
        match recv_skipping_events(&mut control).await {
            ControlMessage::SceneSnapshot(payload) => {
                assert_eq!(payload.active_scene, Some(scene_id));
                assert!(payload.scenes.iter().any(|s| s.scene_id == scene_id));
                assert_eq!(payload.seq, server.current_event_seq().await);
            }
            other => panic!("expected SceneSnapshot, got {other:?}"),
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn host_api_start_take_uses_catalog_dir_identity() {
        let (config, temp) = config_with_temp_catalog();
        let server = ServerHandle::new(config);
        let (scene, _scene_id, _object_id) = camera_scene();
        server.load_scene(scene).await;

        let take_id = server.start_take_from(now_ns(), None).await.unwrap();
        let take = server.stop_take_from(None).await.unwrap();
        assert_eq!(take.take_id, take_id);
        assert_eq!(
            std::path::Path::new(&take.path).parent(),
            Some(temp.path())
        );
        assert!(std::path::Path::new(&take.path).exists());
    }
}
