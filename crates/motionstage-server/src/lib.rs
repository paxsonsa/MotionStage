use std::{
    collections::BTreeMap,
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
    ClientRole, ControlMessage, DataFlowState, Feature, Mode, PlaybackAction, PlaybackRuntimeState,
    ProtocolError, ProtocolVersion, RecordingState, RegisterAccepted, RegisterRejected,
    RegisterRequest, RejectCode, SamplingMode, SdpMessage, SdpType, ServerHello, SessionState,
    SignalMessage, SignalPayload, TakeBakeAttribute, TakeInfo, VideoStreamStatus, PROTOCOL_MAJOR,
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

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub device_id: Uuid,
    pub device_name: String,
    pub session_id: Option<Uuid>,
    pub roles: Vec<ClientRole>,
    pub features: Vec<Feature>,
    pub advertised_attributes: Vec<AttributeDescriptor>,
    pub state: SessionState,
    /// Nanosecond timestamp of last activity (control message or motion datagram).
    pub last_activity_ns: u64,
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
            .filter(|session| session.state != SessionState::Closed)
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

    fn tick_playback(&mut self, now_ns: u64) {
        let Some(playback) = self.active_playback.as_mut() else {
            return;
        };
        if playback.state != PlaybackRuntimeState::Playing {
            return;
        }

        let Some(started_wall_ns) = playback.started_wall_ns else {
            playback.started_wall_ns = Some(now_ns);
            return;
        };

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
            }
        }

        playback.playhead_ns = playhead;
        let scene_id = playback.recording.manifest.scene_id;
        if let Some(frame) = Self::frame_for_playhead(&playback.recording, playback.playhead_ns) {
            self.apply_playback_frame(&frame, scene_id);
        }
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

/// Snapshot of playback transport state for the companion UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackStatus {
    pub take_id: Uuid,
    pub state: PlaybackRuntimeState,
    pub position_ns: u64,
    pub duration_ns: u64,
    pub looping: bool,
}

#[derive(Clone)]
pub struct ServerHandle {
    state: Arc<RwLock<ServerState>>,
    mode_updates: broadcast::Sender<Mode>,
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
        let (mode_updates, _mode_updates_rx) = broadcast::channel(64);
        let state = ServerState {
            runtime: RuntimeCore::new(config.lease),
            sessions: BTreeMap::new(),
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
            host_requests: Vec::new(),
            host_selection: Vec::new(),
            config,
        };

        Self {
            state: Arc::new(RwLock::new(state)),
            mode_updates,
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
                        state.runtime.scheduler_tick(now);
                        state.tick_playback(now);

                        // Evict sessions that have been idle beyond the configured timeout (4.4).
                        let idle_timeout = state.config.lease.session_idle_timeout_ns;
                        if idle_timeout > 0 {
                            let expired: Vec<Uuid> = state.sessions.values()
                                .filter(|s| {
                                    s.state != SessionState::Closed
                                        && s.state != SessionState::Discovered
                                        && now.saturating_sub(s.last_activity_ns) >= idle_timeout
                                })
                                .map(|s| s.device_id)
                                .collect();
                            for device_id in expired {
                                warn!(%device_id, "session idle timeout; closing");
                                state.runtime.register_device_disconnected(device_id, now);
                                let _ = state.change_session_state(device_id, SessionState::Closed);
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
            },
        );
        debug!(%device_id, device_name = %device_name, "session discovered");
        Ok(())
    }

    pub async fn session_count(&self) -> usize {
        let state = self.state.read().await;
        state
            .sessions
            .values()
            .filter(|session| session.state != SessionState::Closed)
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
        let version_result = negotiate_version(
            ProtocolVersion::new(PROTOCOL_MAJOR, PROTOCOL_MINOR),
            ProtocolVersion::new(hello.protocol_major, hello.protocol_minor),
        );
        if let Err(err) = version_result {
            return Err(ServerError::Protocol(err));
        }
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

        let session_id = Uuid::new_v4();
        session.session_id = Some(session_id);
        state.change_session_state(device_id, SessionState::Registered)?;
        state.metrics.accepted_sessions += 1;
        debug!(%device_id, %session_id, "registration accepted");

        Ok(RegisterAccepted {
            session_id,
            negotiated_features,
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
        let device_name = state
            .sessions
            .get(&device_id)
            .map(|s| s.device_name.clone())
            .unwrap_or_default();
        if let Some(recording) = state.active_recording.as_mut() {
            recording.writer.push_marker(RecordingMarker::ClientJoined {
                timestamp_ns: now_ns(),
                device_id,
                device_name,
            });
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
                reason,
            });
        }
        state
            .runtime
            .register_device_disconnected(device_id, now_ns);
        state.video_peers.remove(&device_id);
        state.change_session_state(device_id, SessionState::Closed)
    }

    pub async fn load_scene(&self, scene: Scene) -> SceneId {
        let mut state = self.state.write().await;
        state.runtime.load_scene(scene)
    }

    pub async fn set_active_scene(&self, scene_id: SceneId) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state
            .runtime
            .set_active_scene(scene_id)
            .map_err(ServerError::Core)
    }

    pub async fn set_data_flow(&self, data_flow: DataFlowState) -> Result<(), ServerError> {
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
        let _ = self.mode_updates.send(to);
        Ok(())
    }

    pub async fn set_recording(&self, recording: RecordingState) -> Result<(), ServerError> {
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
        let _ = self.mode_updates.send(to);
        Ok(())
    }

    pub fn subscribe_mode_updates(&self) -> broadcast::Receiver<Mode> {
        self.mode_updates.subscribe()
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

    fn resolve_scene_for_baseline(
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
        let mut state = self.state.write().await;
        let resolved = Self::resolve_scene_for_baseline(&state, scene_id)?;
        state
            .runtime
            .reset_scene_to_baseline(resolved)
            .map_err(ServerError::Core)
    }

    pub async fn commit_scene_baseline(
        &self,
        scene_id: Option<SceneId>,
    ) -> Result<u32, ServerError> {
        let mut state = self.state.write().await;
        let resolved = Self::resolve_scene_for_baseline(&state, scene_id)?;
        state
            .runtime
            .commit_scene_baseline(resolved)
            .map_err(ServerError::Core)
    }

    pub async fn commit_object_baseline(
        &self,
        scene_id: Option<SceneId>,
        object_id: ObjectId,
    ) -> Result<u32, ServerError> {
        let mut state = self.state.write().await;
        let resolved = Self::resolve_scene_for_baseline(&state, scene_id)?;
        state
            .runtime
            .commit_object_baseline(resolved, object_id)
            .map_err(ServerError::Core)
    }

    pub async fn set_mode_control_allowlist(&self, _device_ids: Vec<Uuid>) {
        // Role-based mode control is authoritative; allowlists are intentionally ignored.
    }

    pub async fn mode_control_allowlist(&self) -> Vec<Uuid> {
        Vec::new()
    }

    pub async fn mode_control_allowed(&self, _device_id: Uuid) -> bool {
        true
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
        let mut state = self.state.write().await;
        let mapping_id = state
            .runtime
            .create_mapping(req, now_ns)
            .map_err(ServerError::Core)?;
        let mapping_for_marker = state.runtime.snapshot().mappings.get(&mapping_id).cloned();
        if let Some(recording) = state.active_recording.as_mut() {
            if let Some(mapping) = mapping_for_marker {
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
        }
        Ok(mapping_id)
    }

    pub async fn update_mapping(
        &self,
        mapping_id: MappingId,
        req: MappingRequest,
        now_ns: u64,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        state
            .runtime
            .update_mapping(mapping_id, req, now_ns)
            .map_err(ServerError::Core)?;
        let mapping_for_marker = state.runtime.snapshot().mappings.get(&mapping_id).cloned();
        if let Some(recording) = state.active_recording.as_mut() {
            if let Some(mapping) = mapping_for_marker {
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
        }
        Ok(())
    }

    pub async fn remove_mapping(&self, mapping_id: MappingId) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
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
        Ok(())
    }

    pub async fn set_mapping_lock(
        &self,
        mapping_id: MappingId,
        lock: bool,
    ) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
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
        Ok(())
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
        let mut state = self.state.write().await;
        state.active_playback = None;
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

        let active_scene = state
            .runtime
            .snapshot()
            .active_scene
            .ok_or_else(|| ServerError::Recording("no active scene".into()))?;

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

        Ok(recording_id)
    }

    pub async fn stop_recording(&self) -> Result<RecordingManifest, ServerError> {
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

        state
            .take_catalog
            .register_take(
                manifest.recording_id,
                manifest.scene_id,
                recording_path,
                manifest.started_ns,
                manifest.frame_count,
            )
            .map_err(ServerError::Take)?;

        state
            .runtime
            .set_recording(RecordingState::Inactive)
            .map_err(ServerError::Core)?;

        Ok(manifest)
    }

    pub async fn list_takes(&self, scene_id: Option<SceneId>) -> Vec<TakeInfo> {
        let state = self.state.read().await;
        state.take_catalog.list(scene_id)
    }

    pub async fn select_take(&self, take_id: Uuid) -> Result<TakeInfo, ServerError> {
        let mut state = self.state.write().await;
        state
            .take_catalog
            .select_take(take_id)
            .map_err(ServerError::Take)
    }

    pub async fn playback_play(
        &self,
        take_id: Uuid,
        looping: bool,
    ) -> Result<(PlaybackRuntimeState, u64, bool), ServerError> {
        let mut state = self.state.write().await;
        let take = state
            .take_catalog
            .get(take_id)
            .cloned()
            .ok_or_else(|| ServerError::Take(format!("take not found: {take_id}")))?;
        let recording =
            read_recording(&take.path).map_err(|err| ServerError::Take(err.to_string()))?;
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
        let playhead_ns = playback.playhead_ns;
        let looping = playback.looping;
        state.active_playback = Some(playback);
        Ok((PlaybackRuntimeState::Playing, playhead_ns, looping))
    }

    pub async fn playback_pause(
        &self,
        take_id: Uuid,
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
        Ok((playback.state, playback.playhead_ns, playback.looping))
    }

    pub async fn playback_seek(
        &self,
        take_id: Uuid,
        seek_ns: u64,
        looping: bool,
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
        Ok((status, playhead, loop_state))
    }

    pub async fn playback_stop(
        &self,
        take_id: Uuid,
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
        let mut state = self.state.write().await;
        let path = state
            .take_catalog
            .mark_deleted(take_id)
            .map_err(ServerError::Take)?;

        if let Some(path) = path {
            if let Err(err) = fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    return Err(ServerError::Take(err.to_string()));
                }
            }
            state
                .take_catalog
                .purge_take(take_id)
                .map_err(ServerError::Take)?;
        }

        if matches!(state.active_playback.as_ref(), Some(active) if active.take_id == take_id) {
            state.active_playback = None;
            state
                .runtime
                .set_recording(RecordingState::Inactive)
                .map_err(ServerError::Core)?;
        }

        state
            .bake_cursors
            .retain(|_, cursor| cursor.take_id != take_id);
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
    state: DataFlowState,
) -> Result<HandlerOutcome, ServerError> {
    if !client_hello.roles.contains(&ClientRole::Operator) {
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
    match server.set_data_flow(state).await {
        Ok(()) => {
            let active_mode = server.mode().await;
            control
                .send(&ControlMessage::ModeState(active_mode))
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

async fn handle_set_recording(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    state: RecordingState,
) -> Result<HandlerOutcome, ServerError> {
    if !client_hello.roles.contains(&ClientRole::Operator) {
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
    match server.set_recording(state).await {
        Ok(()) => {
            let active_mode = server.mode().await;
            control
                .send(&ControlMessage::ModeState(active_mode))
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

async fn handle_baseline_control(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
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
    if !client_hello.roles.contains(&ClientRole::Operator) {
        if send_protocol_error(control, RejectCode::RoleDenied, reject_reason.into())
            .await
            .is_err()
        {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(ControlFlow::Break(()));
        }
        return Ok(ControlFlow::Continue(()));
    }
    let result: Result<(BaselineAction, u32), ServerError> = match msg {
        ControlMessage::ResetSceneToBaseline { scene_id } => server
            .reset_scene_to_baseline(scene_id)
            .await
            .map(|n| (BaselineAction::ResetScene, n)),
        ControlMessage::CommitSceneBaseline { scene_id } => server
            .commit_scene_baseline(scene_id)
            .await
            .map(|n| (BaselineAction::CommitScene, n)),
        ControlMessage::CommitObjectBaseline {
            scene_id,
            object_id,
        } => server
            .commit_object_baseline(scene_id, object_id)
            .await
            .map(|n| (BaselineAction::CommitObject, n)),
        _ => unreachable!(),
    };
    match result {
        Ok((action, changed_attributes)) => {
            control
                .send(&ControlMessage::BaselineActionApplied {
                    action,
                    changed_attributes,
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

async fn handle_take_management(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    msg: ControlMessage,
) -> Result<HandlerOutcome, ServerError> {
    let reject_reason = match &msg {
        ControlMessage::ListTakes { .. } => "operator role is required to list takes",
        ControlMessage::SelectTake { .. } => "operator role is required to select takes",
        ControlMessage::DeleteTake { .. } => "operator role is required to delete takes",
        _ => unreachable!(),
    };
    if !client_hello.roles.contains(&ClientRole::Operator) {
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
        ControlMessage::SelectTake { take_id } => match server.select_take(take_id).await {
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
        ControlMessage::DeleteTake { take_id } => match server.delete_take(take_id).await {
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

async fn handle_playback_control(
    control: &mut motionstage_transport_quic::ControlChannel,
    server: &ServerHandle,
    client_hello: &ClientHello,
    take_id: Uuid,
    action: PlaybackAction,
    seek_ns: Option<u64>,
    looping: bool,
) -> Result<HandlerOutcome, ServerError> {
    if !client_hello.roles.contains(&ClientRole::Operator) {
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
    let result = match action {
        PlaybackAction::Play => server.playback_play(take_id, looping).await,
        PlaybackAction::Pause => server.playback_pause(take_id).await,
        PlaybackAction::Stop => server.playback_stop(take_id).await,
        PlaybackAction::Seek => {
            let seek = seek_ns.unwrap_or_default();
            server.playback_seek(take_id, seek, looping).await
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
    if !client_hello.roles.contains(&ClientRole::Operator) {
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
            return Err(ServerError::Runtime(
                "expected ClientHello as first control message".into(),
            ));
        }
    };

    server
        .discovered(client_hello.device_id, client_hello.device_name.clone())
        .await?;
    server.transport_connected(client_hello.device_id).await?;
    server.hello_exchanged(client_hello.clone()).await?;
    server.authenticate(client_hello.device_id).await?;

    let register_req = match control
        .recv()
        .await
        .map_err(|err| ServerError::Runtime(err.to_string()))?
    {
        ControlMessage::RegisterRequest(req) => req,
        _ => {
            return Err(ServerError::Runtime(
                "expected RegisterRequest after ClientHello".into(),
            ));
        }
    };

    match server.register(client_hello.device_id, register_req).await {
        Ok(accepted) => {
            control
                .send(&ControlMessage::RegisterAccepted(accepted))
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
        }
        Err(ServerError::RegisterRejected(rejected)) => {
            control
                .send(&ControlMessage::RegisterRejected(rejected))
                .await
                .map_err(|err| ServerError::Runtime(err.to_string()))?;
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Ok(());
        }
        Err(err) => {
            let _ = server.close_session(client_hello.device_id, now_ns()).await;
            return Err(err);
        }
    }

    server.scene_synced(client_hello.device_id).await?;
    server.activate(client_hello.device_id).await?;
    let mut mode_updates = server.subscribe_mode_updates();

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
                        match handle_set_data_flow(&mut control, &server, &client_hello, state).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(ControlMessage::SetRecording(state)) => {
                        match handle_set_recording(&mut control, &server, &client_hello, state).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(msg @ (ControlMessage::ResetSceneToBaseline { .. }
                        | ControlMessage::CommitSceneBaseline { .. }
                        | ControlMessage::CommitObjectBaseline { .. })) => {
                        match handle_baseline_control(&mut control, &server, &client_hello, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(msg @ (ControlMessage::ListTakes { .. }
                        | ControlMessage::SelectTake { .. }
                        | ControlMessage::DeleteTake { .. })) => {
                        match handle_take_management(&mut control, &server, &client_hello, msg).await? {
                            ControlFlow::Break(()) => break,
                            ControlFlow::Continue(()) => {}
                        }
                    }
                    Ok(ControlMessage::PlaybackControl { take_id, action, seek_ns, looping }) => {
                        match handle_playback_control(&mut control, &server, &client_hello, take_id, action, seek_ns, looping).await? {
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
                    | Ok(ControlMessage::RegisterRejected(_)) => {
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
            mode_update = mode_updates.recv() => {
                match mode_update {
                    Ok(active_mode) => {
                        if control.send(&ControlMessage::ModeState(active_mode)).await.is_err() {
                            let _ = server.close_session(client_hello.device_id, now_ns()).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        }
    }

    Ok(())
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

fn map_server_error_to_reject(err: &ServerError) -> RejectCode {
    match err {
        ServerError::Protocol(_) => RejectCode::VersionMismatch,
        ServerError::SessionNotFound(_) => RejectCode::RoleDenied,
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
        value: match recorded.value {
            AttributeValue::Bool(v) => BakeAttributeValue::Bool(v),
            AttributeValue::Int32(v) => BakeAttributeValue::Int32(v),
            AttributeValue::Float32(v) => BakeAttributeValue::Float32(v),
            AttributeValue::Float64(v) => BakeAttributeValue::Float64(v),
            AttributeValue::Vec2f(v) => BakeAttributeValue::Vec2f(v),
            AttributeValue::Vec3f(v) => BakeAttributeValue::Vec3f(v),
            AttributeValue::Vec4f(v) => BakeAttributeValue::Vec4f(v),
            AttributeValue::Quatf(v) => BakeAttributeValue::Quatf(v),
            AttributeValue::Mat4f(v) => BakeAttributeValue::Mat4f(v),
            AttributeValue::Trigger(v) => BakeAttributeValue::Trigger(v),
        },
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
        AttributeDescriptor, AttributeKind, BaselineAction, ClientHello, ClientRole,
        ControlMessage, DataFlowState, Feature, Mode, RecordingState, RegisterRequest, RejectCode,
        SamplingMode, SessionState, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    };
    use tempfile::{tempdir, NamedTempFile};
    use uuid::Uuid;

    use crate::{SecurityMode, ServerConfig, ServerError, ServerHandle};
    use motionstage_recording::{read_recording, RecordingFormatVersion, RecordingMarker};
    use motionstage_transport_quic::{
        AttributeUpdateFrame, ControlChannel, MotionDatagram, QuicClient, QuicPeer,
    };
    use motionstage_webrtc::WebRtcSession;

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
        (peer, control)
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
        let server = ServerHandle::new(ServerConfig::default());
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
        let response = control.recv().await.unwrap();
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
        let initial = control.recv().await.unwrap();
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
        let active = control.recv().await.unwrap();
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
    async fn operator_role_can_set_data_flow_over_control() {
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

        control
            .send(&ControlMessage::SetDataFlow(DataFlowState::Live))
            .await
            .unwrap();
        match control.recv().await.unwrap() {
            ControlMessage::ModeState(mode) => assert_eq!(mode, Mode::LIVE),
            other => panic!("expected ModeState, got {other:?}"),
        }

        drop(peer);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn operator_role_can_set_data_flow_without_allowlist() {
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;
        let server = ServerHandle::new(config);
        let runtime = server.start_quic_runtime().await.unwrap();

        let device_id = Uuid::now_v7();
        let (_peer, mut control) = connect_active_quic_client(
            runtime.local_addr,
            device_id,
            ClientRole::Operator,
            Feature::Mapping,
        )
        .await;

        control
            .send(&ControlMessage::SetDataFlow(DataFlowState::Live))
            .await
            .unwrap();
        match control.recv().await.unwrap() {
            ControlMessage::ModeState(mode) => assert_eq!(mode, Mode::LIVE),
            other => panic!("expected ModeState, got {other:?}"),
        }

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

        let ack = tokio::time::timeout(Duration::from_secs(1), control_b.recv())
            .await
            .expect("requesting client should get mode ack")
            .unwrap();
        assert!(matches!(ack, ControlMessage::ModeState(Mode::LIVE)));

        let pushed = tokio::time::timeout(Duration::from_secs(1), control_a.recv())
            .await
            .expect("other active client should get mode broadcast")
            .unwrap();
        assert!(matches!(pushed, ControlMessage::ModeState(Mode::LIVE)));

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
        match control.recv().await.unwrap() {
            ControlMessage::BaselineActionApplied {
                action,
                changed_attributes,
            } => {
                assert_eq!(action, BaselineAction::ResetScene);
                assert_eq!(changed_attributes, 1);
            }
            other => panic!("expected BaselineActionApplied, got {other:?}"),
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
        match control.recv().await.unwrap() {
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
        let response = control_b.recv().await.unwrap();
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
        let server = ServerHandle::new(ServerConfig::default());
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
}
