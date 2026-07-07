use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_MAJOR: u16 = 2;
pub const PROTOCOL_MINOR: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientRole {
    MotionSource,
    CameraController,
    VideoSink,
    Operator,
    /// Authors the scene graph (object/attribute definitions). Held by the
    /// host DCC's in-process session; expresses host privileges as a role
    /// rather than a separate API.
    SceneAuthor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Feature {
    Motion,
    Mapping,
    Recording,
    Video,
    Hdr10,
    SdrFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttributeKind {
    Bool,
    Int32,
    Float32,
    Float64,
    Vec2f,
    Vec3f,
    Vec4f,
    Quatf,
    Mat4f,
    Trigger,
}

impl AttributeKind {
    /// Map FFI component counts to attribute kinds.
    /// 1=Float32, 2=Vec2f, 3=Vec3f, 4=Quatf, 16=Mat4f
    pub fn from_component_count(count: u32) -> Option<AttributeKind> {
        match count {
            1 => Some(AttributeKind::Float32),
            2 => Some(AttributeKind::Vec2f),
            3 => Some(AttributeKind::Vec3f),
            4 => Some(AttributeKind::Quatf),
            16 => Some(AttributeKind::Mat4f),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeDescriptor {
    pub path: String,
    pub value_type: AttributeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataFlowState {
    Idle,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordingState {
    Inactive,
    Recording,
    Playback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    pub data_flow: DataFlowState,
    pub recording: RecordingState,
}

impl Mode {
    pub const IDLE: Self = Self {
        data_flow: DataFlowState::Idle,
        recording: RecordingState::Inactive,
    };
    pub const LIVE: Self = Self {
        data_flow: DataFlowState::Live,
        recording: RecordingState::Inactive,
    };
    pub const RECORDING: Self = Self {
        data_flow: DataFlowState::Live,
        recording: RecordingState::Recording,
    };
    pub const PLAYBACK: Self = Self {
        data_flow: DataFlowState::Live,
        recording: RecordingState::Playback,
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeInfo {
    pub take_id: Uuid,
    pub scene_id: Uuid,
    pub name: String,
    pub path: String,
    pub created_ns: u64,
    pub frame_count: u64,
    pub selected: bool,
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackAction {
    Play,
    Pause,
    Stop,
    Seek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackRuntimeState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SamplingMode {
    Captured,
    FixedFps { fps: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BakeAttributeValue {
    Bool(bool),
    Int32(i32),
    Float32(f32),
    Float64(f64),
    Vec2f([f32; 2]),
    Vec3f([f32; 3]),
    Vec4f([f32; 4]),
    Quatf([f32; 4]),
    Mat4f([[f32; 4]; 4]),
    Trigger(bool),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakeBakeAttribute {
    pub object_id: Uuid,
    pub attribute: String,
    pub value: BakeAttributeValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BaselineAction {
    ResetScene,
    CommitScene,
    CommitObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Discovered,
    TransportConnected,
    HelloExchanged,
    Authenticated,
    Registered,
    SceneSynced,
    Active,
    Closed,
}

impl SessionState {
    pub fn can_transition_to(self, next: Self) -> bool {
        use SessionState::*;
        matches!(
            (self, next),
            (Discovered, TransportConnected)
                | (TransportConnected, HelloExchanged)
                | (HelloExchanged, Authenticated)
                | (Authenticated, Registered)
                | (Registered, SceneSynced)
                | (SceneSynced, Active)
                | (Active, Closed)
                | (Authenticated, Closed)
                | (Registered, Closed)
                | (SceneSynced, Closed)
                | (TransportConnected, Closed)
                | (HelloExchanged, Closed)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectCode {
    UnsupportedProtocol,
    VersionMismatch,
    NoCommonFeature,
    AuthFailed,
    RoleDenied,
    CapacityExceeded,
    ServerBusy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub features: Vec<Feature>,
    pub security_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionNegotiation {
    pub server: ProtocolVersion,
    pub client: ProtocolVersion,
    /// The version the server actually speaks: always the **server's own**
    /// major/minor. The server does not downgrade its wire behaviour for a
    /// lesser client (see [`negotiate_version`]).
    pub selected: ProtocolVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub device_id: Uuid,
    pub device_name: String,
    pub roles: Vec<ClientRole>,
    pub features: Vec<Feature>,
    pub advertised_attributes: Vec<AttributeDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub pairing_token: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterAccepted {
    pub session_id: Uuid,
    pub negotiated_features: Vec<Feature>,
    /// The protocol minor the server speaks to this session. The server
    /// speaks **exactly its own minor** ([`PROTOCOL_MINOR`]); it does not
    /// downgrade behaviour for a lesser client. A client whose minor is
    /// *greater* than the server's is rejected at the hello exchange (it may
    /// depend on features the server lacks); a client whose minor is
    /// *lesser-or-equal* is accepted without behaviour change and told the
    /// server's minor here. See [`negotiate_version`].
    pub negotiated_protocol_minor: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRejected {
    pub code: RejectCode,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SdpType {
    Offer,
    Answer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdpMessage {
    pub ty: SdpType,
    pub sdp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalPayload {
    Sdp(SdpMessage),
    Ice(IceCandidate),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalMessage {
    pub from_device: Uuid,
    pub to_device: Uuid,
    pub payload: SignalPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoStreamStatus {
    pub available: bool,
    pub descriptor_set: bool,
    pub peer_count: u32,
    pub last_frame_age_ms: Option<u64>,
}

/// Typed failure payload carried inside operator-plane result messages
/// ([`ControlMessage::MappingCreateResult`], [`ControlMessage::MappingOpResult`],
/// [`ControlMessage::TakeStartResult`], [`ControlMessage::TakeStopResult`]).
/// A failed operation mutates nothing and emits no [`StateEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: RejectCode,
    pub reason: String,
}

/// Compact wire summary of a mapping. Used by the mapping [`StateEvent`]s and
/// by [`SceneSnapshotPayload::mappings`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingSummary {
    pub mapping_id: Uuid,
    pub source_device: Uuid,
    pub source_output: String,
    pub target_scene: Uuid,
    pub target_object: Uuid,
    pub target_attribute: String,
    pub component_mask: Option<Vec<usize>>,
    pub lock: bool,
}

/// A replicated state change on the authoritative simulation.
///
/// Every mutation of server state emits exactly one corresponding event,
/// fanned out to every session on every transport as
/// [`ControlMessage::StateEventMsg`]. The server performs **no echo
/// suppression**: a session receives events for its own mutations too
/// (`origin_session` equal to its session id). Clients that do not want to
/// re-apply their own changes filter on
/// [`StateEventEnvelope::origin_session`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StateEvent {
    /// The composite mode changed (or was re-asserted).
    ModeChanged { mode: Mode },
    /// A scene was loaded into the runtime. If it is the first scene it also
    /// becomes the active scene (reflected in the next snapshot).
    SceneLoaded { scene_id: Uuid, name: String },
    /// The active scene changed.
    SceneActivated { scene_id: Uuid },
    MappingCreated { mapping: MappingSummary },
    MappingUpdated { mapping: MappingSummary },
    MappingRemoved { mapping_id: Uuid },
    MappingLockChanged { mapping_id: Uuid, lock: bool },
    /// A mapping lease was released by the scheduler (source device gone).
    MappingReleased { mapping_id: Uuid, reason: String },
    BaselineApplied {
        action: BaselineAction,
        changed_attributes: u32,
    },
    /// A session reached `Active` (this fires for the in-process host session
    /// exactly like for remote sessions).
    SessionJoined {
        session_id: Uuid,
        device_id: Uuid,
        device_name: String,
        roles: Vec<ClientRole>,
    },
    SessionLeft {
        session_id: Uuid,
        reason: Option<String>,
    },
    RecordingStarted { take_id: Uuid, scene_id: Uuid },
    RecordingStopped { take_id: Uuid, frame_count: u64 },
    /// A finished recording was registered in the take catalog.
    TakeRegistered { take: TakeInfo },
    TakeSelected { take_id: Uuid, scene_id: Uuid },
    TakeDeleted { take_id: Uuid },
    PlaybackChanged {
        state: PlaybackRuntimeState,
        take_id: Uuid,
        playhead_ns: u64,
        looping: bool,
    },
}

/// Ordered wrapper for [`StateEvent`] fan-out.
///
/// `seq` is a strictly monotonic counter assigned while the server mutation
/// lock is held, so envelope order matches mutation order. `origin_session`
/// is the session that caused the mutation when known (`None` for
/// server-internal transitions such as lease expiry or idle eviction).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateEventEnvelope {
    pub seq: u64,
    pub origin_session: Option<Uuid>,
    pub timestamp_ns: u64,
    pub event: StateEvent,
}

/// Wire mirror of a scene attribute (`motionstage-core`'s `SceneAttribute`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotAttribute {
    pub name: String,
    pub default_value: BakeAttributeValue,
    pub current_value: BakeAttributeValue,
    pub live_enabled: bool,
    pub record_enabled: bool,
}

/// Wire mirror of a scene object (`motionstage-core`'s `SceneObject`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotObject {
    pub object_id: Uuid,
    pub name: String,
    pub attributes: Vec<SnapshotAttribute>,
}

/// Wire mirror of a scene graph (`motionstage-core`'s `Scene`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotScene {
    pub scene_id: Uuid,
    pub name: String,
    pub objects: Vec<SnapshotObject>,
}

/// Wire summary of a registered session, carried by
/// [`SceneSnapshotPayload::sessions`]. Mirrors the identity fields of
/// [`StateEvent::SessionJoined`] plus the host marker, so a client recovering
/// from a snapshot rebuilds exactly the set of sessions that will later emit
/// [`StateEvent::SessionLeft`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: Uuid,
    pub device_id: Uuid,
    pub device_name: String,
    pub roles: Vec<ClientRole>,
    /// True for the server's in-process host session (the DCC itself). The
    /// host is a real session on the event plane but not a motion client.
    pub is_host: bool,
}

/// Playback transport state carried by [`SceneSnapshotPayload::playback`].
/// Mirrors [`StateEvent::PlaybackChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackSummary {
    pub state: PlaybackRuntimeState,
    pub take_id: Uuid,
    pub playhead_ns: u64,
    pub looping: bool,
}

/// Full world snapshot sent at the `SceneSynced` handshake step, on resync
/// when the requested seq is out of replay range, and to receivers that
/// lagged the event stream. `seq` is the event sequence number the snapshot
/// is consistent with; events with `seq` less than or equal to this value are
/// already folded into the snapshot and can be discarded by the client.
///
/// The snapshot covers every domain replicated by [`StateEvent`]s — scene
/// graphs, mappings, mode, sessions, takes, and playback — so a receiver that
/// discards events at or below `seq` loses nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneSnapshotPayload {
    pub scenes: Vec<SnapshotScene>,
    pub mappings: Vec<MappingSummary>,
    pub mode: Mode,
    pub active_scene: Option<Uuid>,
    /// Registered sessions (a `session_id` has been assigned; `Registered`
    /// state or later, not `Closed`), including the in-process host session.
    /// Pre-registration sessions are excluded: they never emit
    /// `SessionJoined`/`SessionLeft`, so they must not appear here either.
    pub sessions: Vec<SessionSummary>,
    /// The take catalog (non-deleted takes across all scenes).
    pub takes: Vec<TakeInfo>,
    /// Active playback transport, if a take is loaded.
    pub playback: Option<PlaybackSummary>,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    ServerHello(ServerHello),
    ClientHello(ClientHello),
    RegisterRequest(RegisterRequest),
    RegisterAccepted(RegisterAccepted),
    RegisterRejected(RegisterRejected),
    VideoSignal(SignalMessage),
    DrainSignals,
    SignalsBatch(Vec<SignalMessage>),
    CreateVideoOffer {
        stream_id: String,
        track_id: String,
    },
    VideoOffer(SdpMessage),
    GetVideoStreamStatus,
    VideoStreamStatus(VideoStreamStatus),
    ListTakes {
        scene_id: Option<Uuid>,
    },
    TakeList {
        takes: Vec<TakeInfo>,
    },
    SelectTake {
        take_id: Uuid,
    },
    TakeSelected {
        take_id: Uuid,
    },
    PlaybackControl {
        take_id: Uuid,
        action: PlaybackAction,
        seek_ns: Option<u64>,
        looping: bool,
    },
    PlaybackState {
        take_id: Uuid,
        state: PlaybackRuntimeState,
        playhead_ns: u64,
        looping: bool,
    },
    DeleteTake {
        take_id: Uuid,
    },
    TakeDeleted {
        take_id: Uuid,
    },
    OpenTakeBakeCursor {
        take_id: Uuid,
        sampling_mode: SamplingMode,
    },
    TakeBakeCursorOpened {
        cursor_id: Uuid,
        total_frames: u64,
    },
    ReadTakeBakeFrame {
        cursor_id: Uuid,
    },
    SeekTakeBakeFrame {
        cursor_id: Uuid,
        frame_index: u64,
    },
    TakeBakeFrame {
        cursor_id: Uuid,
        frame_index: u64,
        timestamp_ns: u64,
        attributes: Vec<TakeBakeAttribute>,
    },
    CloseTakeBakeCursor {
        cursor_id: Uuid,
    },
    TakeBakeCursorClosed {
        cursor_id: Uuid,
    },
    Error {
        code: RejectCode,
        reason: String,
    },
    Ping,
    Pong,
    ClientGoodbye {
        reason: Option<String>,
    },
    /// Client → server: set the data-flow axis (Operator-gated).
    ///
    /// Success has **no direct reply**: the caller observes its own
    /// [`ControlMessage::StateEventMsg`] echo carrying
    /// [`StateEvent::ModeChanged`] (like every other session). Failure is
    /// answered with [`ControlMessage::Error`].
    SetDataFlow(DataFlowState),
    /// Client → server: set the recording axis (Operator-gated). Reply
    /// semantics are identical to [`ControlMessage::SetDataFlow`]: event echo
    /// on success, [`ControlMessage::Error`] on failure.
    SetRecording(RecordingState),
    /// Server → client: composite mode probe. Sent **only** as part of the
    /// [`ControlMessage::Ping`] heartbeat reply (`Pong` + `ModeState`), as a
    /// cheap liveness+state probe. It is no longer a direct acknowledgement
    /// of `SetDataFlow`/`SetRecording` — mode changes replicate via
    /// [`StateEvent::ModeChanged`].
    ModeState(Mode),
    ResetSceneToBaseline {
        scene_id: Option<Uuid>,
    },
    CommitSceneBaseline {
        scene_id: Option<Uuid>,
    },
    CommitObjectBaseline {
        scene_id: Option<Uuid>,
        object_id: Uuid,
    },
    /// RETIRED — the server never sends this message (protocol 2.1). The
    /// single acknowledgement of a baseline action is the caller's own
    /// [`ControlMessage::StateEventMsg`] echo carrying
    /// [`StateEvent::BaselineApplied`], which includes the same
    /// `changed_attributes` count. Failures are answered with
    /// [`ControlMessage::Error`]. The variant is kept only so downstream
    /// SDKs keep compiling during the migration; it will be deleted next
    /// protocol bump.
    BaselineActionApplied {
        action: BaselineAction,
        changed_attributes: u32,
    },
    /// Server → client: ordered state replication event.
    StateEventMsg(StateEventEnvelope),
    /// Server → client: full world snapshot (handshake, resync fallback,
    /// lagged-receiver recovery).
    SceneSnapshot(SceneSnapshotPayload),
    /// Client → server: request replay of events after `last_seq`. The server
    /// answers with the missing [`ControlMessage::StateEventMsg`]s in order
    /// when they are still buffered, otherwise with a fresh
    /// [`ControlMessage::SceneSnapshot`].
    ResyncRequest { last_seq: u64 },
    /// Client → server: create a mapping (operator plane).
    ///
    /// `source_device: None` resolves to the requesting session's own device
    /// id; `target_scene: None` resolves to the active scene. Permission: a
    /// session may create mappings whose resolved `source_device` is its own
    /// device; a session holding [`ClientRole::Operator`] may create any.
    /// Replies [`ControlMessage::MappingCreateResult`]. Success additionally
    /// replicates as [`StateEvent::MappingCreated`] to every session.
    CreateMapping {
        source_device: Option<Uuid>,
        source_output: String,
        target_scene: Option<Uuid>,
        target_object: Uuid,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
    },
    /// Server → client: direct reply to [`ControlMessage::CreateMapping`] and
    /// [`ControlMessage::UpdateMapping`]. `Ok` carries the resulting mapping
    /// summary (post-default-resolution).
    MappingCreateResult {
        result: Result<MappingSummary, WireError>,
    },
    /// Client → server: replace a mapping's full definition (operator plane;
    /// mirrors the host API's `update_mapping`). `source_device`/`target_scene`
    /// default as in [`ControlMessage::CreateMapping`]. Permission: non-operator
    /// sessions may update only mappings whose current **and** requested
    /// `source_device` is their own device. Replies
    /// [`ControlMessage::MappingCreateResult`] carrying the updated summary;
    /// success replicates as [`StateEvent::MappingUpdated`].
    UpdateMapping {
        mapping_id: Uuid,
        source_device: Option<Uuid>,
        source_output: String,
        target_scene: Option<Uuid>,
        target_object: Uuid,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
    },
    /// Client → server: remove a mapping (operator plane). Permission: own
    /// `source_device` or Operator. Replies [`ControlMessage::MappingOpResult`];
    /// success replicates as [`StateEvent::MappingRemoved`].
    RemoveMapping { mapping_id: Uuid },
    /// Client → server: lock/unlock a mapping (operator plane). Permission:
    /// own `source_device` or Operator. Replies
    /// [`ControlMessage::MappingOpResult`]; success replicates as
    /// [`StateEvent::MappingLockChanged`].
    SetMappingLock { mapping_id: Uuid, lock: bool },
    /// Server → client: direct reply to [`ControlMessage::RemoveMapping`] and
    /// [`ControlMessage::SetMappingLock`].
    MappingOpResult {
        mapping_id: Uuid,
        result: Result<(), WireError>,
    },
    /// Client → server: start recording a take (Operator-gated). Take
    /// identity is server-assigned: the server generates the recording path
    /// inside the take-catalog directory — callers never supply paths.
    /// Replies [`ControlMessage::TakeStartResult`]; success replicates as
    /// [`StateEvent::RecordingStarted`] (plus [`StateEvent::ModeChanged`]
    /// when the mode moved).
    StartTake,
    /// Server → client: direct reply to [`ControlMessage::StartTake`]. `Ok`
    /// carries the server-assigned take id.
    TakeStartResult { result: Result<Uuid, WireError> },
    /// Client → server: stop the active recording and register the take
    /// (Operator-gated). Replies [`ControlMessage::TakeStopResult`]; success
    /// replicates as [`StateEvent::RecordingStopped`] +
    /// [`StateEvent::TakeRegistered`] + [`StateEvent::ModeChanged`].
    StopTake,
    /// Server → client: direct reply to [`ControlMessage::StopTake`]. `Ok`
    /// carries the registered take's catalog entry.
    TakeStopResult { result: Result<TakeInfo, WireError> },
    /// Client → server: request an on-demand full world snapshot. The server
    /// replies with [`ControlMessage::SceneSnapshot`]. Devices use this to
    /// populate target pickers without reconnecting.
    GetSceneSnapshot,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid state transition: {from:?} -> {to:?}")]
    InvalidTransition {
        from: SessionState,
        to: SessionState,
    },
    #[error("unsupported major protocol version: server={server:?} client={client:?}")]
    UnsupportedMajor {
        server: ProtocolVersion,
        client: ProtocolVersion,
    },
    #[error("client minor version is newer than server: server={server:?} client={client:?}")]
    ClientTooNew {
        server: ProtocolVersion,
        client: ProtocolVersion,
    },
}

/// Negotiate the wire version at the hello exchange. The contract is
/// truth-in-docs, not a compatibility shim (there are no old clients — wire
/// compat is a clean break):
///
/// - The **major** must match the server's exactly; any other major is
///   rejected with [`ProtocolError::UnsupportedMajor`]. The server never
///   speaks a foreign major.
/// - A client **minor** *greater* than the server's is rejected with
///   [`ProtocolError::ClientTooNew`] — it may rely on features the server
///   does not implement.
/// - A client minor *lesser-or-equal* is accepted, and the server speaks
///   **exactly its own minor** (`selected.minor == server.minor`). It does
///   not downgrade behaviour, so the negotiated/echoed minor honestly
///   reflects what the server speaks, not a fictional per-client dialect.
pub fn negotiate_version(
    server: ProtocolVersion,
    client: ProtocolVersion,
) -> Result<VersionNegotiation, ProtocolError> {
    if server.major != client.major {
        return Err(ProtocolError::UnsupportedMajor { server, client });
    }
    if client.minor > server.minor {
        return Err(ProtocolError::ClientTooNew { server, client });
    }

    Ok(VersionNegotiation {
        server,
        client,
        // The server speaks its own minor; it does not downgrade for a
        // lesser client. Truth-in-docs: the selected minor is the server's.
        selected: ProtocolVersion::new(server.major, server.minor),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_transitions_follow_spec() {
        assert!(SessionState::Discovered.can_transition_to(SessionState::TransportConnected));
        assert!(SessionState::TransportConnected.can_transition_to(SessionState::HelloExchanged));
        assert!(SessionState::HelloExchanged.can_transition_to(SessionState::Authenticated));
        assert!(SessionState::Authenticated.can_transition_to(SessionState::Registered));
        assert!(SessionState::Registered.can_transition_to(SessionState::SceneSynced));
        assert!(SessionState::SceneSynced.can_transition_to(SessionState::Active));
        assert!(SessionState::Active.can_transition_to(SessionState::Closed));
        assert!(!SessionState::Discovered.can_transition_to(SessionState::Active));
    }

    #[test]
    fn version_negotiation_accepts_backward_minor() {
        let result = negotiate_version(ProtocolVersion::new(2, 0), ProtocolVersion::new(2, 0))
            .expect("compatible versions should negotiate");
        assert_eq!(result.selected, ProtocolVersion::new(2, 0));
    }

    #[test]
    fn version_negotiation_speaks_server_minor_for_lesser_client() {
        // A lesser-minor client is accepted, but the server speaks its own
        // minor (no behaviour downgrade): selected.minor == server.minor.
        let result = negotiate_version(ProtocolVersion::new(2, 7), ProtocolVersion::new(2, 3))
            .expect("lesser-or-equal minor should negotiate");
        assert_eq!(result.selected, ProtocolVersion::new(2, 7));
    }

    #[test]
    fn version_negotiation_rejects_major_mismatch() {
        let err =
            negotiate_version(ProtocolVersion::new(2, 0), ProtocolVersion::new(1, 0)).unwrap_err();
        assert!(format!("{err}").contains("unsupported major"));
    }

    #[test]
    fn version_negotiation_rejects_client_newer_minor() {
        let err =
            negotiate_version(ProtocolVersion::new(2, 0), ProtocolVersion::new(2, 1)).unwrap_err();
        assert!(format!("{err}").contains("client minor version is newer"));
    }

    #[test]
    fn control_message_supports_video_signaling_variants() {
        let from = Uuid::now_v7();
        let to = Uuid::now_v7();
        let message = ControlMessage::VideoSignal(SignalMessage {
            from_device: from,
            to_device: to,
            payload: SignalPayload::Sdp(SdpMessage {
                ty: SdpType::Offer,
                sdp: "v=0".into(),
            }),
        });

        let encoded = bincode::serialize(&message).expect("control message serializes");
        let decoded: ControlMessage =
            bincode::deserialize(&encoded).expect("control message deserializes");
        assert_eq!(decoded, message);
    }

    #[test]
    fn control_message_supports_baseline_action_variants() {
        let object_id = Uuid::now_v7();
        let message = ControlMessage::CommitObjectBaseline {
            scene_id: None,
            object_id,
        };
        let encoded = bincode::serialize(&message).expect("control message serializes");
        let decoded: ControlMessage =
            bincode::deserialize(&encoded).expect("control message deserializes");
        assert_eq!(decoded, message);
    }

    #[test]
    fn control_message_supports_take_variants() {
        let take_id = Uuid::now_v7();
        let cursor_id = Uuid::now_v7();
        let message = ControlMessage::TakeBakeFrame {
            cursor_id,
            frame_index: 3,
            timestamp_ns: 123,
            attributes: vec![TakeBakeAttribute {
                object_id: Uuid::nil(),
                attribute: "position".into(),
                value: BakeAttributeValue::Vec3f([1.0, 2.0, 3.0]),
            }],
        };
        let encoded = bincode::serialize(&message).expect("control message serializes");
        let decoded: ControlMessage =
            bincode::deserialize(&encoded).expect("control message deserializes");
        assert_eq!(decoded, message);

        let message = ControlMessage::PlaybackControl {
            take_id,
            action: PlaybackAction::Seek,
            seek_ns: Some(1_000),
            looping: true,
        };
        let encoded = bincode::serialize(&message).expect("control message serializes");
        let decoded: ControlMessage =
            bincode::deserialize(&encoded).expect("control message deserializes");
        assert_eq!(decoded, message);
    }

    fn round_trip(message: &ControlMessage) {
        let encoded = bincode::serialize(message).expect("control message serializes");
        let decoded: ControlMessage =
            bincode::deserialize(&encoded).expect("control message deserializes");
        assert_eq!(&decoded, message);
    }

    #[test]
    fn control_message_supports_state_event_variants() {
        let session = Uuid::now_v7();
        round_trip(&ControlMessage::StateEventMsg(StateEventEnvelope {
            seq: 7,
            origin_session: Some(session),
            timestamp_ns: 123,
            event: StateEvent::ModeChanged { mode: Mode::LIVE },
        }));
        round_trip(&ControlMessage::StateEventMsg(StateEventEnvelope {
            seq: 8,
            origin_session: None,
            timestamp_ns: 124,
            event: StateEvent::MappingCreated {
                mapping: MappingSummary {
                    mapping_id: Uuid::now_v7(),
                    source_device: Uuid::now_v7(),
                    source_output: "pose_pos".into(),
                    target_scene: Uuid::now_v7(),
                    target_object: Uuid::now_v7(),
                    target_attribute: "position".into(),
                    component_mask: Some(vec![0, 2]),
                    lock: false,
                },
            },
        }));
        round_trip(&ControlMessage::StateEventMsg(StateEventEnvelope {
            seq: 9,
            origin_session: Some(session),
            timestamp_ns: 125,
            event: StateEvent::SessionJoined {
                session_id: session,
                device_id: Uuid::now_v7(),
                device_name: "host".into(),
                roles: vec![ClientRole::SceneAuthor, ClientRole::Operator],
            },
        }));
        round_trip(&ControlMessage::ResyncRequest { last_seq: 42 });
    }

    #[test]
    fn control_message_supports_operator_plane_mapping_ops() {
        round_trip(&ControlMessage::CreateMapping {
            source_device: None,
            source_output: "pose_pos".into(),
            target_scene: None,
            target_object: Uuid::now_v7(),
            target_attribute: "position".into(),
            component_mask: Some(vec![0, 2]),
        });
        round_trip(&ControlMessage::MappingCreateResult {
            result: Ok(MappingSummary {
                mapping_id: Uuid::now_v7(),
                source_device: Uuid::now_v7(),
                source_output: "pose_pos".into(),
                target_scene: Uuid::now_v7(),
                target_object: Uuid::now_v7(),
                target_attribute: "position".into(),
                component_mask: None,
                lock: false,
            }),
        });
        round_trip(&ControlMessage::MappingCreateResult {
            result: Err(WireError {
                code: RejectCode::RoleDenied,
                reason: "not your mapping".into(),
            }),
        });
        round_trip(&ControlMessage::UpdateMapping {
            mapping_id: Uuid::now_v7(),
            source_device: Some(Uuid::now_v7()),
            source_output: "pose_rot".into(),
            target_scene: Some(Uuid::now_v7()),
            target_object: Uuid::now_v7(),
            target_attribute: "rotation".into(),
            component_mask: None,
        });
        round_trip(&ControlMessage::RemoveMapping {
            mapping_id: Uuid::now_v7(),
        });
        round_trip(&ControlMessage::SetMappingLock {
            mapping_id: Uuid::now_v7(),
            lock: true,
        });
        round_trip(&ControlMessage::MappingOpResult {
            mapping_id: Uuid::now_v7(),
            result: Ok(()),
        });
        round_trip(&ControlMessage::MappingOpResult {
            mapping_id: Uuid::now_v7(),
            result: Err(WireError {
                code: RejectCode::ServerBusy,
                reason: "mapping not found".into(),
            }),
        });
    }

    #[test]
    fn control_message_supports_take_control_and_snapshot_request() {
        round_trip(&ControlMessage::StartTake);
        round_trip(&ControlMessage::TakeStartResult {
            result: Ok(Uuid::now_v7()),
        });
        round_trip(&ControlMessage::TakeStartResult {
            result: Err(WireError {
                code: RejectCode::RoleDenied,
                reason: "operator role is required".into(),
            }),
        });
        round_trip(&ControlMessage::StopTake);
        round_trip(&ControlMessage::TakeStopResult {
            result: Ok(TakeInfo {
                take_id: Uuid::now_v7(),
                scene_id: Uuid::now_v7(),
                name: "Take 001".into(),
                path: "/tmp/take-001.cmtrk".into(),
                created_ns: 1,
                frame_count: 2,
                selected: true,
                deleted: false,
            }),
        });
        round_trip(&ControlMessage::GetSceneSnapshot);
    }

    #[test]
    fn register_accepted_round_trips_negotiated_minor() {
        let message = ControlMessage::RegisterAccepted(RegisterAccepted {
            session_id: Uuid::now_v7(),
            negotiated_features: vec![Feature::Motion],
            negotiated_protocol_minor: 0,
        });
        round_trip(&message);
    }

    #[test]
    fn control_message_supports_scene_snapshot() {
        let message = ControlMessage::SceneSnapshot(SceneSnapshotPayload {
            scenes: vec![SnapshotScene {
                scene_id: Uuid::now_v7(),
                name: "shot".into(),
                objects: vec![SnapshotObject {
                    object_id: Uuid::now_v7(),
                    name: "camera".into(),
                    attributes: vec![SnapshotAttribute {
                        name: "position".into(),
                        default_value: BakeAttributeValue::Vec3f([0.0, 0.0, 0.0]),
                        current_value: BakeAttributeValue::Vec3f([1.0, 2.0, 3.0]),
                        live_enabled: true,
                        record_enabled: true,
                    }],
                }],
            }],
            mappings: vec![MappingSummary {
                mapping_id: Uuid::now_v7(),
                source_device: Uuid::now_v7(),
                source_output: "pose_pos".into(),
                target_scene: Uuid::now_v7(),
                target_object: Uuid::now_v7(),
                target_attribute: "position".into(),
                component_mask: None,
                lock: true,
            }],
            mode: Mode::LIVE,
            active_scene: Some(Uuid::now_v7()),
            sessions: vec![
                SessionSummary {
                    session_id: Uuid::now_v7(),
                    device_id: Uuid::now_v7(),
                    device_name: "host".into(),
                    roles: vec![ClientRole::SceneAuthor, ClientRole::Operator],
                    is_host: true,
                },
                SessionSummary {
                    session_id: Uuid::now_v7(),
                    device_id: Uuid::now_v7(),
                    device_name: "ipad".into(),
                    roles: vec![ClientRole::MotionSource],
                    is_host: false,
                },
            ],
            takes: vec![TakeInfo {
                take_id: Uuid::now_v7(),
                scene_id: Uuid::now_v7(),
                name: "Take 001".into(),
                path: "/tmp/take-001.cmtrk".into(),
                created_ns: 123,
                frame_count: 42,
                selected: true,
                deleted: false,
            }],
            playback: Some(PlaybackSummary {
                state: PlaybackRuntimeState::Playing,
                take_id: Uuid::now_v7(),
                playhead_ns: 1_000,
                looping: true,
            }),
            seq: 17,
        });
        round_trip(&message);
    }
}
