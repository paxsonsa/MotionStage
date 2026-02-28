use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use motionstage_core::{
    AttributeUpdate, CoreError, LeaseConfig, MappingId, MappingRequest, RuntimeCore,
    RuntimeSnapshot, Scene, SceneId,
};
use motionstage_discovery::{DiscoveryAdvertisement, DiscoveryPublisher};
use motionstage_media::{
    negotiate_stream, NegotiatedVideoStream, SignalingHub, VideoClientCapability,
    VideoStreamDescriptor,
};
use motionstage_protocol::{
    negotiate_version, ClientHello, ClientRole, ControlMessage, Feature, Mode, ProtocolError,
    ProtocolVersion, RegisterAccepted, RegisterRejected, RegisterRequest, RejectCode, SdpMessage,
    SdpType, ServerHello, SessionState, SignalMessage, SignalPayload, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};
use motionstage_recording::{
    RecordedAttribute, RecordedFrame, RecordingManifest, RecordingMarker, RecordingWriter,
};
use motionstage_transport_quic::{MotionDatagram, QuicServer};
use motionstage_webrtc::WebRtcSession;
use thiserror::Error;
use tokio::sync::{watch, RwLock};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

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
    pub state: SessionState,
}

struct ActiveRecording {
    path: PathBuf,
    writer: RecordingWriter,
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
    master_video_descriptor: Option<VideoStreamDescriptor>,
    signaling: SignalingHub,
    video_peers: BTreeMap<Uuid, VideoPeerSession>,
    runtime_resources: Option<RuntimeResources>,
    active_advertisement: Option<DiscoveryAdvertisement>,
    last_published_snapshot: Option<RuntimeSnapshot>,
}

impl ServerState {
    fn change_session_state(
        &mut self,
        device_id: Uuid,
        next: SessionState,
    ) -> Result<(), ServerError> {
        let session = self
            .sessions
            .get_mut(&device_id)
            .ok_or_else(|| ServerError::SessionNotFound(device_id))?;

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
                let expected = self
                    .config
                    .pairing_token
                    .as_deref()
                    .unwrap_or("motionstage");
                match req.pairing_token.as_deref() {
                    Some(token) if token == expected => Ok(()),
                    _ => Err(RejectCode::AuthFailed),
                }
            }
            SecurityMode::ApiKey => {
                let expected = self.config.api_key.as_deref().unwrap_or("motionstage");
                match req.api_key.as_deref() {
                    Some(key) if key == expected => Ok(()),
                    _ => Err(RejectCode::AuthFailed),
                }
            }
            SecurityMode::ApiKeyPlusPairing => {
                let pair = self
                    .config
                    .pairing_token
                    .as_deref()
                    .unwrap_or("motionstage");
                let key = self.config.api_key.as_deref().unwrap_or("motionstage");
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
}

#[derive(Clone)]
pub struct ServerHandle {
    state: Arc<RwLock<ServerState>>,
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
        let state = ServerState {
            runtime: RuntimeCore::new(config.lease),
            sessions: BTreeMap::new(),
            metrics: ServerMetrics::default(),
            running: false,
            active_recording: None,
            master_video_descriptor: None,
            signaling: SignalingHub::default(),
            video_peers: BTreeMap::new(),
            runtime_resources: None,
            active_advertisement: None,
            last_published_snapshot: None,
            config,
        };

        Self {
            state: Arc::new(RwLock::new(state)),
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
                state: SessionState::Discovered,
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

        let session = state
            .sessions
            .get_mut(&hello.device_id)
            .ok_or(ServerError::SessionNotFound(hello.device_id))?;
        session.roles = hello.roles;
        session.features = hello.features;
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

        let session_id = Uuid::now_v7();
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
        state.change_session_state(device_id, SessionState::Active)
    }

    pub async fn close_session(&self, device_id: Uuid, now_ns: u64) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
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

    pub async fn set_mode(&self, mode: Mode) -> Result<(), ServerError> {
        let mut state = self.state.write().await;
        let from = state.runtime.mode();
        state.runtime.set_mode(mode).map_err(ServerError::Core)?;
        if let Some(recording) = state.active_recording.as_mut() {
            recording
                .writer
                .push_marker(RecordingMarker::ModeTransition {
                    timestamp_ns: now_ns(),
                    from,
                    to: mode,
                });
        }
        Ok(())
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
            let mut attrs = Vec::new();
            for update in &updates {
                let mapping = snapshot
                    .mappings
                    .values()
                    .find(|m| {
                        m.source_device == device_id
                            && m.source_output == update.output_attribute
                            && m.state == motionstage_core::MappingState::Active
                    })
                    .ok_or_else(|| {
                        ServerError::Core(CoreError::MappingDenied(format!(
                            "no active mapping for output '{}'",
                            update.output_attribute
                        )))
                    })?;

                attrs.push(RecordedAttribute {
                    object_id: mapping.target_object,
                    attribute: mapping.target_attribute.clone(),
                    value: update.value.clone(),
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
        let mut from_mode = state.runtime.mode();
        if from_mode == Mode::Idle {
            state
                .runtime
                .set_mode(Mode::Live)
                .map_err(ServerError::Core)?;
            from_mode = Mode::Live;
        }
        state
            .runtime
            .set_mode(Mode::Recording)
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
                    to: Mode::Recording,
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

        recording
            .writer
            .push_marker(RecordingMarker::ModeTransition {
                timestamp_ns: now_ns(),
                from: Mode::Recording,
                to: Mode::Live,
            });

        let manifest = recording
            .writer
            .finish(&recording.path)
            .map_err(|err| ServerError::Recording(err.to_string()))?;

        state
            .runtime
            .set_mode(Mode::Live)
            .map_err(ServerError::Core)?;

        Ok(manifest)
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
            peer.add_h264_track(stream_id, track_id)
                .await
                .map_err(|err| ServerError::WebRtc(err.to_string()))?;
            let mut state = self.state.write().await;
            if let Some(entry) = state.video_peers.get_mut(&device_id) {
                entry.track_added = true;
            }
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
                    peer.add_h264_track(&stream_id, "video")
                        .await
                        .map_err(|err| ServerError::WebRtc(err.to_string()))?;
                    let mut state = self.state.write().await;
                    if let Some(entry) = state.video_peers.get_mut(&device_id) {
                        entry.track_added = true;
                    }
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

    pub async fn has_video_session(&self, device_id: Uuid) -> bool {
        let state = self.state.read().await;
        state.video_peers.contains_key(&device_id)
    }

    pub async fn session_info(&self, device_id: Uuid) -> Option<SessionInfo> {
        let state = self.state.read().await;
        state.sessions.get(&device_id).cloned()
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

    loop {
        tokio::select! {
            ctrl = control.recv() => {
                match ctrl {
                    Ok(ControlMessage::Ping) => {
                        control.send(&ControlMessage::Pong).await.map_err(|err| ServerError::Runtime(err.to_string()))?;
                    }
                    Ok(ControlMessage::Pong) => {}
                    Ok(ControlMessage::CreateVideoOffer { stream_id, track_id }) => {
                        match server.create_video_offer(client_hello.device_id, &stream_id, &track_id).await {
                            Ok(offer) => {
                                control
                                    .send(&ControlMessage::VideoOffer(offer))
                                    .await
                                    .map_err(|err| ServerError::Runtime(err.to_string()))?;
                            }
                            Err(err) => {
                                if send_protocol_error(&mut control, map_server_error_to_reject(&err), err.to_string()).await.is_err() {
                                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                                    break;
                                }
                            }
                        }
                    }
                    Ok(ControlMessage::VideoSignal(signal)) => {
                        if signal.from_device != client_hello.device_id {
                            if send_protocol_error(&mut control, RejectCode::RoleDenied, "signal from_device does not match active session".into()).await.is_err() {
                                let _ = server.close_session(client_hello.device_id, now_ns()).await;
                                break;
                            }
                            continue;
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
                                    if send_protocol_error(&mut control, map_server_error_to_reject(&err), err.to_string()).await.is_err() {
                                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                                        break;
                                    }
                                }
                            }
                        } else if let Err(err) = server.push_signaling_message(signal).await {
                            if send_protocol_error(&mut control, map_server_error_to_reject(&err), err.to_string()).await.is_err() {
                                let _ = server.close_session(client_hello.device_id, now_ns()).await;
                                break;
                            }
                        }
                    }
                    Ok(ControlMessage::DrainSignals) => {
                        match server.drain_signaling_messages(client_hello.device_id).await {
                            Ok(messages) => {
                                control
                                    .send(&ControlMessage::SignalsBatch(messages))
                                    .await
                                    .map_err(|err| ServerError::Runtime(err.to_string()))?;
                            }
                            Err(err) => {
                                if send_protocol_error(&mut control, map_server_error_to_reject(&err), err.to_string()).await.is_err() {
                                    let _ = server.close_session(client_hello.device_id, now_ns()).await;
                                    break;
                                }
                            }
                        }
                    }
                    Ok(ControlMessage::SignalsBatch(_))
                    | Ok(ControlMessage::VideoOffer(_))
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
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
                        break;
                    }
                }
            }
            datagram = peer.recv_motion_datagram() => {
                match datagram {
                    Ok(frame) => {
                        let _ = server.ingest_motion_datagram(frame).await;
                    }
                    Err(_) => {
                        let _ = server.close_session(client_hello.device_id, now_ns()).await;
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
        ServerError::Core(_) | ServerError::Recording(_) | ServerError::WebRtc(_) => {
            RejectCode::ServerBusy
        }
        ServerError::Discovery(_) | ServerError::Runtime(_) => RejectCode::ServerBusy,
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
        "127.0.0.1".into()
    } else {
        addr.ip().to_string()
    }
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
    use std::net::SocketAddr;

    use motionstage_core::{AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject};
    use motionstage_media::{
        ColorPrimaries, DynamicRange, IceCandidate, SdpMessage, SdpType, SignalMessage,
        SignalPayload, ToneMapMode, TransferFunction, VideoClientCapability, VideoStreamDescriptor,
    };
    use motionstage_protocol::{
        ClientHello, ClientRole, ControlMessage, Feature, Mode, RegisterRequest, SessionState,
        PROTOCOL_MAJOR, PROTOCOL_MINOR,
    };
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    use crate::{SecurityMode, ServerConfig, ServerHandle};
    use motionstage_recording::{read_recording, RecordingFormatVersion, RecordingMarker};
    use motionstage_transport_quic::{AttributeUpdateFrame, ControlChannel, QuicClient, QuicPeer};
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
                to: Mode::Recording,
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
            })
            .await
            .unwrap();

        let stream = server
            .negotiate_video_for_client(VideoClientCapability {
                supports_hdr10: false,
                max_width: 1920,
                max_height: 1080,
                max_fps: 24,
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

        for (device, role, feature) in [
            (a, ClientRole::VideoSink, Feature::Video),
            (b, ClientRole::CameraController, Feature::Video),
        ] {
            server.discovered(device, "peer").await.unwrap();
            server.transport_connected(device).await.unwrap();
            server
                .hello_exchanged(ClientHello {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                    device_id: device,
                    device_name: "peer".into(),
                    roles: vec![role],
                    features: vec![feature],
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
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unsupported major"));
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
        server.set_mode(Mode::Live).await.unwrap();

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
}
