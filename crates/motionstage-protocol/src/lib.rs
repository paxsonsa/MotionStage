use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 3;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Idle,
    Live,
    Recording,
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
    pub advertised_attributes: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    Error {
        code: RejectCode,
        reason: String,
    },
    Ping,
    Pong,
    SetMode(Mode),
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
    BaselineActionApplied {
        action: BaselineAction,
        changed_attributes: u32,
    },
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
        selected: ProtocolVersion::new(server.major, client.minor),
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
        let result = negotiate_version(ProtocolVersion::new(1, 4), ProtocolVersion::new(1, 1))
            .expect("compatible versions should negotiate");
        assert_eq!(result.selected, ProtocolVersion::new(1, 1));
    }

    #[test]
    fn version_negotiation_rejects_major_mismatch() {
        let err =
            negotiate_version(ProtocolVersion::new(1, 1), ProtocolVersion::new(2, 0)).unwrap_err();
        assert!(format!("{err}").contains("unsupported major"));
    }

    #[test]
    fn version_negotiation_rejects_client_newer_minor() {
        let err =
            negotiate_version(ProtocolVersion::new(1, 1), ProtocolVersion::new(1, 2)).unwrap_err();
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
}
