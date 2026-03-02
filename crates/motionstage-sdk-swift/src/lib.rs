// FFI functions take raw pointers by design; callers are responsible for validity.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::{
    ffi::{CStr, CString},
    net::SocketAddr,
    os::raw::{c_char, c_void},
    ptr,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use motionstage_protocol::{
    AttributeDescriptor, AttributeKind, BaselineAction, ClientHello, ClientRole, ControlMessage,
    DataFlowState, Feature, Mode, RecordingState, RegisterRequest, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};
use motionstage_transport_quic::{
    AttributeUpdateFrame, AttributeValueFrame, ControlChannel, QuicClient, QuicPeer,
};
use tokio::runtime::Runtime;
use tokio::time::timeout;
use uuid::Uuid;

pub const MOTIONSTAGE_SWIFT_STATUS_OK: i32 = 0;
pub const MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT: i32 = 1;
pub const MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED: i32 = 2;
pub const MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED: i32 = 3;
pub const MOTIONSTAGE_SWIFT_STATUS_PROTOCOL: i32 = 4;
pub const MOTIONSTAGE_SWIFT_STATUS_TRANSPORT: i32 = 5;
pub const MOTIONSTAGE_SWIFT_STATUS_INTERNAL: i32 = 6;

/// Deprecated legacy mode constants — kept for `set_mode` shim.
pub const MOTIONSTAGE_SWIFT_MODE_IDLE: i32 = 0;
pub const MOTIONSTAGE_SWIFT_MODE_LIVE: i32 = 1;
pub const MOTIONSTAGE_SWIFT_MODE_RECORDING: i32 = 2;
pub const MOTIONSTAGE_SWIFT_MODE_PLAYBACK: i32 = 3;

pub const MOTIONSTAGE_SWIFT_DATA_FLOW_IDLE: i32 = 0;
pub const MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE: i32 = 1;

pub const MOTIONSTAGE_SWIFT_RECORDING_INACTIVE: i32 = 0;
pub const MOTIONSTAGE_SWIFT_RECORDING_RECORDING: i32 = 1;
pub const MOTIONSTAGE_SWIFT_RECORDING_PLAYBACK: i32 = 2;

pub const MOTIONSTAGE_SWIFT_FIELD_POSITION: u32 = 0x01;
pub const MOTIONSTAGE_SWIFT_FIELD_ROTATION: u32 = 0x02;
pub const MOTIONSTAGE_SWIFT_FIELD_VELOCITY: u32 = 0x04;
pub const MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH: u32 = 0x08;
pub const MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE: u32 = 0x10;
pub const MOTIONSTAGE_SWIFT_FIELD_APERTURE: u32 = 0x20;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MODE_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RESET_SCENE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Global shared Tokio runtime (2.3)
// ---------------------------------------------------------------------------

static GLOBAL_RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn get_runtime() -> &'static Runtime {
    GLOBAL_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create global Tokio runtime")
    })
}

// ---------------------------------------------------------------------------
// C-visible types
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct MotionStageClientConfig {
    /// Timeout in milliseconds for the initial handshake. 0 = use default (5000 ms).
    pub handshake_timeout_ms: u32,
    /// Timeout in milliseconds for a mode-change reply. 0 = use default (5000 ms).
    pub mode_reply_timeout_ms: u32,
    /// Timeout in milliseconds for scene reset confirmation. 0 = use default (5000 ms).
    pub reset_scene_timeout_ms: u32,
}

impl MotionStageClientConfig {
    fn handshake_timeout(&self) -> Duration {
        if self.handshake_timeout_ms == 0 {
            DEFAULT_HANDSHAKE_TIMEOUT
        } else {
            Duration::from_millis(self.handshake_timeout_ms as u64)
        }
    }

    fn mode_reply_timeout(&self) -> Duration {
        if self.mode_reply_timeout_ms == 0 {
            DEFAULT_MODE_REPLY_TIMEOUT
        } else {
            Duration::from_millis(self.mode_reply_timeout_ms as u64)
        }
    }

    fn reset_scene_timeout(&self) -> Duration {
        if self.reset_scene_timeout_ms == 0 {
            DEFAULT_RESET_SCENE_TIMEOUT
        } else {
            Duration::from_millis(self.reset_scene_timeout_ms as u64)
        }
    }
}

/// Camera-specific legacy motion frame (retained for backwards compatibility).
#[repr(C)]
pub struct MotionFrameFFI {
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub velocity: [f32; 3],
    pub focal_length: f32,
    pub focus_distance: f32,
    pub aperture: f32,
    pub field_mask: u32,
}

/// Attribute descriptor for typed constructor (3.0).
/// `value_type`: AttributeKind ordinal (0=Bool, 1=Int32, 2=Float32, 3=Float64,
///   4=Vec2f, 5=Vec3f, 6=Vec4f, 7=Quatf, 8=Mat4f, 9=Trigger)
#[repr(C)]
pub struct MotionStageAttributeDescriptorC {
    pub path: *const c_char,
    pub value_type: u32,
}

/// General-purpose attribute update for `send_batch` (2.1).
/// `component_count`: 1=Float32, 2=Vec2f, 3=Vec3f, 4=Quatf, 16=Mat4f
#[repr(C)]
pub struct MotionAttributeUpdateC {
    pub attribute: *const c_char,
    pub data: *const f32,
    pub component_count: u32,
}

// ---------------------------------------------------------------------------
// Callback type for async connect (2.4)
// ---------------------------------------------------------------------------

pub type MotionStageConnectCallback =
    unsafe extern "C" fn(status: i32, error: *const c_char, context: *mut c_void);

// ---------------------------------------------------------------------------
// Internal client structs
// ---------------------------------------------------------------------------

pub struct MotionStageSwiftClient {
    inner: Mutex<MotionStageSwiftClientInner>,
    /// Nanosecond timestamp of the last motion datagram send. Shared with ping thread.
    last_send_ns: Arc<AtomicU64>,
    /// Signal to stop the ping thread.
    ping_shutdown: Arc<AtomicBool>,
    /// Join handle for the background ping thread (set after connect).
    ping_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct MotionStageSwiftClientInner {
    device_id: Uuid,
    device_name: String,
    _source_outputs: Vec<AttributeDescriptor>,
    qualified_outputs: Vec<AttributeDescriptor>,
    session: Option<ConnectedSession>,
    last_error: Option<String>,
    handshake_timeout: Duration,
    mode_reply_timeout: Duration,
    reset_scene_timeout: Duration,
    /// Shared with the ping thread; stamped on each datagram send.
    last_send_ns: Arc<AtomicU64>,
}

struct ConnectedSession {
    _endpoint: QuicClient,
    peer: QuicPeer,
    control: ControlChannel,
    session_id: Uuid,
}

impl MotionStageSwiftClientInner {
    fn clear_error(&mut self) {
        self.last_error = None;
    }

    fn fail(&mut self, status: i32, message: impl Into<String>) -> i32 {
        self.last_error = Some(message.into());
        status
    }

    fn disconnect(&mut self) {
        if let Some(mut session) = self.session.take() {
            // Best-effort goodbye — ignore errors (connection may already be broken).
            let _ = get_runtime().block_on(session.control.send(
                &ControlMessage::ClientGoodbye {
                    reason: Some("client disconnect".into()),
                },
            ));
            let _ = session.control.finish();
        }
    }

    fn send_vec3f(&mut self, x: f32, y: f32, z: f32) -> Result<(), String> {
        let first_output = self
            .qualified_outputs
            .first()
            .map(|d| d.path.clone())
            .ok_or_else(|| "no output attributes configured".to_owned())?;

        self.send_datagram(vec![AttributeUpdateFrame {
            output_attribute: first_output,
            value: AttributeValueFrame::Vec3f([x, y, z]),
        }])
    }

    fn send_named_vec3f(&self, qualified_attr: &str, x: f32, y: f32, z: f32) -> Result<(), String> {
        self.send_datagram_ref(&[AttributeUpdateFrame {
            output_attribute: qualified_attr.to_owned(),
            value: AttributeValueFrame::Vec3f([x, y, z]),
        }])
    }

    fn send_named_quatf(
        &self,
        qualified_attr: &str,
        x: f32,
        y: f32,
        z: f32,
        w: f32,
    ) -> Result<(), String> {
        self.send_datagram_ref(&[AttributeUpdateFrame {
            output_attribute: qualified_attr.to_owned(),
            value: AttributeValueFrame::Quatf([x, y, z, w]),
        }])
    }

    fn send_named_float32(&self, qualified_attr: &str, value: f32) -> Result<(), String> {
        self.send_datagram_ref(&[AttributeUpdateFrame {
            output_attribute: qualified_attr.to_owned(),
            value: AttributeValueFrame::Float32(value),
        }])
    }

    fn send_motion_frame(&self, frame: &MotionFrameFFI) -> Result<(), String> {
        let mask = frame.field_mask;
        let mut updates = Vec::with_capacity(6);

        if mask & MOTIONSTAGE_SWIFT_FIELD_POSITION != 0 {
            if let Some(attr) = self.qualified_output_for(0) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.to_owned(),
                    value: AttributeValueFrame::Vec3f(frame.position),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_ROTATION != 0 {
            if let Some(attr) = self.qualified_output_for(1) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.to_owned(),
                    value: AttributeValueFrame::Quatf(frame.rotation),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_VELOCITY != 0 {
            if let Some(attr) = self.qualified_output_for(2) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.to_owned(),
                    value: AttributeValueFrame::Vec3f(frame.velocity),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH != 0 {
            if let Some(attr) = self.qualified_output_for(3) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.to_owned(),
                    value: AttributeValueFrame::Float32(frame.focal_length),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE != 0 {
            if let Some(attr) = self.qualified_output_for(4) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.to_owned(),
                    value: AttributeValueFrame::Float32(frame.focus_distance),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_APERTURE != 0 {
            if let Some(attr) = self.qualified_output_for(5) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.to_owned(),
                    value: AttributeValueFrame::Float32(frame.aperture),
                });
            }
        }

        if updates.is_empty() {
            return Ok(());
        }

        self.send_datagram_ref(&updates)
    }

    fn qualified_output_for(&self, index: usize) -> Option<&str> {
        self.qualified_outputs.get(index).map(|d| d.path.as_str())
    }

    fn send_datagram(&self, updates: Vec<AttributeUpdateFrame>) -> Result<(), String> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| "client is not connected".to_owned())?;

        session
            .peer
            .send_motion_datagram(motionstage_transport_quic::MotionDatagram {
                device_id: self.device_id,
                timestamp_ns: now_ns(),
                updates,
            })
            .map_err(|err| format!("failed to send motion datagram: {err}"))?;

        self.last_send_ns.store(now_ns(), Ordering::Relaxed);
        Ok(())
    }

    fn send_datagram_ref(&self, updates: &[AttributeUpdateFrame]) -> Result<(), String> {
        self.send_datagram(updates.to_vec())
    }

    fn reset_scene(&mut self) -> Result<(), String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime()
            .block_on(
                session
                    .control
                    .send(&ControlMessage::ResetSceneToBaseline { scene_id: None }),
            )
            .map_err(|err| format!("failed to send ResetSceneToBaseline: {err}"))?;

        let reset_scene_timeout = self.reset_scene_timeout;
        get_runtime().block_on(async {
            loop {
                let message = timeout(reset_scene_timeout, session.control.recv())
                    .await
                    .map_err(|_| "timed out waiting for baseline reset response".to_owned())?
                    .map_err(|err| format!("failed to receive reset response: {err}"))?;

                match message {
                    ControlMessage::BaselineActionApplied {
                        action: BaselineAction::ResetScene,
                        ..
                    } => return Ok(()),
                    ControlMessage::Error { code, reason } => {
                        return Err(format!(
                            "reset scene rejected: code={code:?} reason={reason}"
                        ))
                    }
                    ControlMessage::Pong => continue,
                    _ => continue,
                }
            }
        })
    }

    fn set_data_flow(&mut self, requested: i32) -> Result<(i32, i32), String> {
        let state = parse_data_flow_state(requested)?;

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime()
            .block_on(
                session
                    .control
                    .send(&ControlMessage::SetDataFlow(state)),
            )
            .map_err(|err| format!("failed to send data-flow request: {err}"))?;

        self.wait_for_mode_state()
    }

    fn set_recording(&mut self, requested: i32) -> Result<(i32, i32), String> {
        let state = parse_recording_state(requested)?;

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime()
            .block_on(
                session
                    .control
                    .send(&ControlMessage::SetRecording(state)),
            )
            .map_err(|err| format!("failed to send recording request: {err}"))?;

        self.wait_for_mode_state()
    }

    /// Wait for a `ModeState` response and return `(data_flow_i32, recording_i32)`.
    fn wait_for_mode_state(&mut self) -> Result<(i32, i32), String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        let mode_reply_timeout = self.mode_reply_timeout;
        get_runtime().block_on(async {
            loop {
                let message = timeout(mode_reply_timeout, session.control.recv())
                    .await
                    .map_err(|_| "timed out waiting for mode response".to_owned())?
                    .map_err(|err| format!("failed to receive mode response: {err}"))?;

                match message {
                    ControlMessage::ModeState(mode) => {
                        return Ok((mode_to_data_flow_i32(&mode), mode_to_recording_i32(&mode)))
                    }
                    ControlMessage::Error { code, reason } => {
                        return Err(format!(
                            "mode request rejected: code={code:?} reason={reason}"
                        ))
                    }
                    ControlMessage::Pong => continue,
                    _ => continue,
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Async connect implementation (2.4)
// ---------------------------------------------------------------------------

async fn connect_impl(
    device_id: Uuid,
    device_name: String,
    qualified_outputs: Vec<AttributeDescriptor>,
    server_addr: String,
    pairing_token: Option<String>,
    api_key: Option<String>,
    handshake_timeout: Duration,
) -> Result<ConnectedSession, String> {
    let server_addr_parsed = SocketAddr::from_str(&server_addr)
        .map_err(|err| format!("invalid server address `{server_addr}`: {err}"))?;

    let endpoint = QuicClient::new_insecure_for_local_dev()
        .map_err(|err| format!("failed to create QUIC client endpoint: {err}"))?;

    let peer = endpoint
        .connect(server_addr_parsed)
        .await
        .map_err(|err| format!("failed to connect QUIC client to {server_addr_parsed}: {err}"))?;

    let mut control = peer
        .accept_control_stream()
        .await
        .map_err(|err| format!("failed to accept control stream: {err}"))?;

    let first_message = timeout(handshake_timeout, control.recv())
        .await
        .map_err(|_| "timed out waiting for ServerHello".to_owned())?
        .map_err(|err| format!("failed to receive ServerHello: {err}"))?;

    match first_message {
        ControlMessage::ServerHello(_) => {}
        other => {
            return Err(format!(
                "expected ServerHello as first control message, got {other:?}"
            ));
        }
    }

    control
        .send(&ControlMessage::ClientHello(ClientHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            device_id,
            device_name,
            roles: vec![ClientRole::MotionSource, ClientRole::Operator],
            features: vec![Feature::Motion, Feature::Mapping, Feature::Recording],
            advertised_attributes: qualified_outputs,
        }))
        .await
        .map_err(|err| format!("failed to send ClientHello: {err}"))?;

    control
        .send(&ControlMessage::RegisterRequest(RegisterRequest {
            pairing_token,
            api_key,
        }))
        .await
        .map_err(|err| format!("failed to send RegisterRequest: {err}"))?;

    let register_message = timeout(handshake_timeout, control.recv())
        .await
        .map_err(|_| "timed out waiting for registration response".to_owned())?
        .map_err(|err| format!("failed to receive registration response: {err}"))?;

    let session_id = match register_message {
        ControlMessage::RegisterAccepted(accepted) => accepted.session_id,
        ControlMessage::RegisterRejected(rejected) => {
            return Err(format!(
                "registration rejected: code={:?} reason={}",
                rejected.code, rejected.reason
            ));
        }
        other => {
            return Err(format!("expected registration result, got {other:?}"));
        }
    };

    Ok(ConnectedSession {
        _endpoint: endpoint,
        peer,
        control,
        session_id,
    })
}

fn map_connect_error(err: &str) -> i32 {
    if err.contains("already connected") {
        MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED
    } else if err.contains("invalid server address") {
        MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT
    } else if err.contains("registration rejected") {
        MOTIONSTAGE_SWIFT_STATUS_PROTOCOL
    } else {
        MOTIONSTAGE_SWIFT_STATUS_TRANSPORT
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_data_flow_state(v: i32) -> Result<DataFlowState, String> {
    match v {
        MOTIONSTAGE_SWIFT_DATA_FLOW_IDLE => Ok(DataFlowState::Idle),
        MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE => Ok(DataFlowState::Live),
        _ => Err(format!("invalid data-flow state value `{v}`")),
    }
}

fn parse_recording_state(v: i32) -> Result<RecordingState, String> {
    match v {
        MOTIONSTAGE_SWIFT_RECORDING_INACTIVE => Ok(RecordingState::Inactive),
        MOTIONSTAGE_SWIFT_RECORDING_RECORDING => Ok(RecordingState::Recording),
        MOTIONSTAGE_SWIFT_RECORDING_PLAYBACK => Ok(RecordingState::Playback),
        _ => Err(format!("invalid recording state value `{v}`")),
    }
}

fn mode_to_data_flow_i32(mode: &Mode) -> i32 {
    match mode.data_flow {
        DataFlowState::Idle => MOTIONSTAGE_SWIFT_DATA_FLOW_IDLE,
        DataFlowState::Live => MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE,
    }
}

fn mode_to_recording_i32(mode: &Mode) -> i32 {
    match mode.recording {
        RecordingState::Inactive => MOTIONSTAGE_SWIFT_RECORDING_INACTIVE,
        RecordingState::Recording => MOTIONSTAGE_SWIFT_RECORDING_RECORDING,
        RecordingState::Playback => MOTIONSTAGE_SWIFT_RECORDING_PLAYBACK,
    }
}

/// Map a composite `Mode` back to a legacy single-integer mode value.
fn mode_to_legacy_i32(mode: &Mode) -> i32 {
    match (mode.data_flow, mode.recording) {
        (DataFlowState::Idle, _) => MOTIONSTAGE_SWIFT_MODE_IDLE,
        (DataFlowState::Live, RecordingState::Inactive) => MOTIONSTAGE_SWIFT_MODE_LIVE,
        (DataFlowState::Live, RecordingState::Recording) => MOTIONSTAGE_SWIFT_MODE_RECORDING,
        (DataFlowState::Live, RecordingState::Playback) => MOTIONSTAGE_SWIFT_MODE_PLAYBACK,
    }
}

/// Convert C ordinal to AttributeKind.
fn attribute_kind_from_ordinal(ordinal: u32) -> Option<AttributeKind> {
    match ordinal {
        0 => Some(AttributeKind::Bool),
        1 => Some(AttributeKind::Int32),
        2 => Some(AttributeKind::Float32),
        3 => Some(AttributeKind::Float64),
        4 => Some(AttributeKind::Vec2f),
        5 => Some(AttributeKind::Vec3f),
        6 => Some(AttributeKind::Vec4f),
        7 => Some(AttributeKind::Quatf),
        8 => Some(AttributeKind::Mat4f),
        9 => Some(AttributeKind::Trigger),
        _ => None,
    }
}

/// Infer AttributeKind from a path name for legacy constructors that don't specify types.
fn infer_attribute_kind_from_path(path: &str) -> AttributeKind {
    let leaf = path.rsplit('.').next().unwrap_or(path);
    match leaf {
        "position" | "velocity" => AttributeKind::Vec3f,
        "rotation" => AttributeKind::Quatf,
        "focal_length" | "focus_distance" | "aperture" => AttributeKind::Float32,
        _ => AttributeKind::Float32,
    }
}

fn qualify_source_output(device_id: Uuid, output_attribute: &str) -> String {
    let normalized = output_attribute.trim();
    if normalized.is_empty() {
        return normalized.to_owned();
    }

    let expected_prefix = format!("{device_id}.");
    if normalized.starts_with(&expected_prefix) {
        return normalized.to_owned();
    }

    let prefix = normalized.split('.').next().unwrap_or_default();
    if Uuid::parse_str(prefix).is_ok() {
        return normalized.to_owned();
    }

    format!("{device_id}.{normalized}")
}

fn make_client_inner(
    device_name: String,
    source_outputs: Vec<AttributeDescriptor>,
    config: Option<&MotionStageClientConfig>,
    last_send_ns: Arc<AtomicU64>,
) -> Result<MotionStageSwiftClientInner, String> {
    // Ensure the global runtime is initialized.
    let _ = get_runtime();

    let device_id = Uuid::now_v7();
    let qualified_outputs: Vec<AttributeDescriptor> = source_outputs
        .iter()
        .map(|desc| AttributeDescriptor {
            path: qualify_source_output(device_id, &desc.path),
            value_type: desc.value_type,
        })
        .collect();

    let handshake_timeout = config
        .map(|c| c.handshake_timeout())
        .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT);
    let mode_reply_timeout = config
        .map(|c| c.mode_reply_timeout())
        .unwrap_or(DEFAULT_MODE_REPLY_TIMEOUT);
    let reset_scene_timeout = config
        .map(|c| c.reset_scene_timeout())
        .unwrap_or(DEFAULT_RESET_SCENE_TIMEOUT);

    Ok(MotionStageSwiftClientInner {
        device_id,
        device_name,
        _source_outputs: source_outputs,
        qualified_outputs,
        session: None,
        last_error: None,
        handshake_timeout,
        mode_reply_timeout,
        reset_scene_timeout,
        last_send_ns,
    })
}

unsafe fn read_required_cstr(input: *const c_char, field: &str) -> Result<String, String> {
    if input.is_null() {
        return Err(format!("{field} must not be null"));
    }

    let value = unsafe { CStr::from_ptr(input) }
        .to_str()
        .map_err(|_| format!("{field} must be valid UTF-8"))?;

    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }

    Ok(value.to_owned())
}

unsafe fn read_optional_cstr(input: *const c_char, field: &str) -> Result<Option<String>, String> {
    if input.is_null() {
        return Ok(None);
    }

    let value = unsafe { CStr::from_ptr(input) }
        .to_str()
        .map_err(|_| format!("{field} must be valid UTF-8"))?;

    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_owned()))
    }
}

fn lock_client<'a>(
    client: *mut c_void,
) -> Result<std::sync::MutexGuard<'a, MotionStageSwiftClientInner>, i32> {
    if client.is_null() {
        return Err(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT);
    }

    let client = unsafe { &*(client as *mut MotionStageSwiftClient) };
    Ok(client
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

fn into_c_string_ptr(value: &str) -> *mut c_char {
    match CString::new(value) {
        Ok(value) => value.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// Ping heartbeat thread (4.3)
// ---------------------------------------------------------------------------

const PING_INTERVAL: Duration = Duration::from_secs(3);
const PING_IDLE_THRESHOLD_NS: u64 = 2_000_000_000; // 2 seconds

fn start_ping_thread(
    client_ptr: usize,
    last_send_ns: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("motionstage-ping".into())
        .spawn(move || {
            loop {
                std::thread::sleep(PING_INTERVAL);

                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Only send a ping if no motion datagram was sent recently.
                let last = last_send_ns.load(Ordering::Relaxed);
                let now = now_ns();
                if last > 0 && now.saturating_sub(last) < PING_IDLE_THRESHOLD_NS {
                    continue;
                }

                // Lock client briefly to send Ping via the control channel.
                let client_ref = unsafe { &*(client_ptr as *const MotionStageSwiftClient) };
                let mut inner = client_ref
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);

                let send_ok = if let Some(session) = inner.session.as_mut() {
                    get_runtime()
                        .block_on(session.control.send(&ControlMessage::Ping))
                        .is_ok()
                } else {
                    // No session — nothing to ping.
                    false
                };
                drop(inner);

                if !send_ok && inner_has_session(client_ptr) {
                    // Connection is broken; stop pinging.
                    break;
                }
            }
        })
        .expect("failed to spawn ping thread")
}

fn inner_has_session(client_ptr: usize) -> bool {
    let client_ref = unsafe { &*(client_ptr as *const MotionStageSwiftClient) };
    let inner = client_ref
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    inner.session.is_some()
}

// ---------------------------------------------------------------------------
// FFI: Runtime (2.3)
// ---------------------------------------------------------------------------

/// Pre-initialize the shared Tokio runtime with a specific thread count.
/// If not called, the runtime is initialized on first use with default settings.
/// Has no effect if called after the runtime is already initialized.
#[no_mangle]
pub extern "C" fn motionstage_swift_runtime_init(thread_count: u32) {
    let _ = GLOBAL_RUNTIME.get_or_init(|| {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        if thread_count > 0 {
            builder.worker_threads(thread_count as usize);
        }
        builder
            .build()
            .expect("failed to create global Tokio runtime")
    });
}

// ---------------------------------------------------------------------------
// FFI: Client lifecycle
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_new(
    device_name: *const c_char,
    output_attribute: *const c_char,
) -> *mut c_void {
    let device_name = match unsafe { read_required_cstr(device_name, "device_name") } {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };
    let output_attribute = match unsafe { read_required_cstr(output_attribute, "output_attribute") }
    {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };

    let last_send_ns = Arc::new(AtomicU64::new(0));
    let inner = match make_client_inner(
        device_name,
        vec![AttributeDescriptor {
            path: output_attribute,
            value_type: infer_attribute_kind_from_path(""),
        }],
        None,
        Arc::clone(&last_send_ns),
    ) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
        last_send_ns,
        ping_shutdown: Arc::new(AtomicBool::new(false)),
        ping_thread: Mutex::new(None),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
}

/// Deprecated: prefer `motionstage_swift_client_new_v2` which takes an explicit array.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_new_multi(
    device_name: *const c_char,
    output_attributes_csv: *const c_char,
) -> *mut c_void {
    let device_name = match unsafe { read_required_cstr(device_name, "device_name") } {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };
    let csv = match unsafe { read_required_cstr(output_attributes_csv, "output_attributes_csv") } {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };

    let source_outputs: Vec<AttributeDescriptor> = csv
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| AttributeDescriptor {
            path: s.to_owned(),
            value_type: infer_attribute_kind_from_path(s),
        })
        .collect();

    if source_outputs.is_empty() {
        return ptr::null_mut();
    }

    let last_send_ns = Arc::new(AtomicU64::new(0));
    let inner = match make_client_inner(device_name, source_outputs, None, Arc::clone(&last_send_ns)) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
        last_send_ns,
        ping_shutdown: Arc::new(AtomicBool::new(false)),
        ping_thread: Mutex::new(None),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_new_multi_with_config(
    device_name: *const c_char,
    output_attributes_csv: *const c_char,
    config: *const MotionStageClientConfig,
) -> *mut c_void {
    let device_name = match unsafe { read_required_cstr(device_name, "device_name") } {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };
    let csv = match unsafe { read_required_cstr(output_attributes_csv, "output_attributes_csv") } {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };

    let source_outputs: Vec<AttributeDescriptor> = csv
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| AttributeDescriptor {
            path: s.to_owned(),
            value_type: infer_attribute_kind_from_path(s),
        })
        .collect();

    if source_outputs.is_empty() {
        return ptr::null_mut();
    }

    let config_ref: Option<&MotionStageClientConfig> = if config.is_null() {
        None
    } else {
        Some(unsafe { &*config })
    };

    let last_send_ns = Arc::new(AtomicU64::new(0));
    let inner = match make_client_inner(device_name, source_outputs, config_ref, Arc::clone(&last_send_ns)) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
        last_send_ns,
        ping_shutdown: Arc::new(AtomicBool::new(false)),
        ping_thread: Mutex::new(None),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
}

/// Array-based constructor (2.2). Prefer over `_new_multi`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_new_v2(
    device_name: *const c_char,
    attribute_count: u32,
    attribute_names: *const *const c_char,
) -> *mut c_void {
    let device_name = match unsafe { read_required_cstr(device_name, "device_name") } {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };

    if attribute_names.is_null() || attribute_count == 0 {
        return ptr::null_mut();
    }

    let mut source_outputs = Vec::with_capacity(attribute_count as usize);
    for i in 0..attribute_count as usize {
        let ptr = unsafe { *attribute_names.add(i) };
        match unsafe { read_required_cstr(ptr, "attribute_names[i]") } {
            Ok(s) => source_outputs.push(AttributeDescriptor {
                value_type: infer_attribute_kind_from_path(&s),
                path: s,
            }),
            Err(_) => return ptr::null_mut(),
        }
    }

    let last_send_ns = Arc::new(AtomicU64::new(0));
    let inner = match make_client_inner(device_name, source_outputs, None, Arc::clone(&last_send_ns)) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
        last_send_ns,
        ping_shutdown: Arc::new(AtomicBool::new(false)),
        ping_thread: Mutex::new(None),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
}

/// Typed descriptor constructor (3.0). Preferred over `_new_v2`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_new_v3(
    device_name: *const c_char,
    attribute_count: u32,
    attributes: *const MotionStageAttributeDescriptorC,
) -> *mut c_void {
    let device_name = match unsafe { read_required_cstr(device_name, "device_name") } {
        Ok(value) => value,
        Err(_) => return ptr::null_mut(),
    };

    if attributes.is_null() || attribute_count == 0 {
        return ptr::null_mut();
    }

    let mut source_outputs = Vec::with_capacity(attribute_count as usize);
    for i in 0..attribute_count as usize {
        let desc = unsafe { &*attributes.add(i) };
        let path = match unsafe { read_required_cstr(desc.path, "attribute path") } {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        };
        let value_type = match attribute_kind_from_ordinal(desc.value_type) {
            Some(k) => k,
            None => return ptr::null_mut(),
        };
        source_outputs.push(AttributeDescriptor { path, value_type });
    }

    let last_send_ns = Arc::new(AtomicU64::new(0));
    let inner = match make_client_inner(device_name, source_outputs, None, Arc::clone(&last_send_ns)) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
        last_send_ns,
        ping_shutdown: Arc::new(AtomicBool::new(false)),
        ping_thread: Mutex::new(None),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_free(client: *mut c_void) {
    if client.is_null() {
        return;
    }

    let client = unsafe { Box::from_raw(client as *mut MotionStageSwiftClient) };
    // Signal ping thread to stop before dropping.
    client.ping_shutdown.store(true, Ordering::Relaxed);
    drop(client);
}

// ---------------------------------------------------------------------------
// FFI: Connection
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_connect(
    client: *mut c_void,
    server_addr: *const c_char,
    pairing_token: *const c_char,
    api_key: *const c_char,
) -> i32 {
    let server_addr = match unsafe { read_required_cstr(server_addr, "server_addr") } {
        Ok(value) => value,
        Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };
    let pairing_token = match unsafe { read_optional_cstr(pairing_token, "pairing_token") } {
        Ok(value) => value,
        Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };
    let api_key = match unsafe { read_optional_cstr(api_key, "api_key") } {
        Ok(value) => value,
        Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };

    // Lock briefly to extract params and check state.
    let (device_id, device_name, qualified_outputs, handshake_timeout) = {
        let mut inner = match lock_client(client) {
            Ok(c) => c,
            Err(status) => return status,
        };
        if inner.session.is_some() {
            inner.fail(
                MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED,
                "client is already connected",
            );
            return MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED;
        }
        (
            inner.device_id,
            inner.device_name.clone(),
            inner.qualified_outputs.clone(),
            inner.handshake_timeout,
        )
    };

    // Perform async connect without holding the lock.
    let result = get_runtime().block_on(connect_impl(
        device_id,
        device_name,
        qualified_outputs,
        server_addr,
        pairing_token,
        api_key,
        handshake_timeout,
    ));

    // Lock briefly to store result.
    let mut inner = match lock_client(client) {
        Ok(c) => c,
        Err(status) => return status,
    };

    match result {
        Ok(session) => {
            inner.session = Some(session);
            inner.clear_error();
            drop(inner);

            // Start ping heartbeat thread.
            let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };
            client_ref.ping_shutdown.store(false, Ordering::Relaxed);
            client_ref.last_send_ns.store(0, Ordering::Relaxed);
            let handle = start_ping_thread(
                client as usize,
                Arc::clone(&client_ref.last_send_ns),
                Arc::clone(&client_ref.ping_shutdown),
            );
            *client_ref.ping_thread.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) => {
            let status = map_connect_error(&err);
            inner.fail(status, err)
        }
    }
}

/// Async connect: calls `callback(status, error_cstr, context)` on completion.
/// The callback is invoked from a Tokio worker thread.
/// `pairing_token` and `api_key` may be NULL.
/// Safety: `client` must remain valid until the callback is called.
#[no_mangle]
pub unsafe extern "C" fn motionstage_swift_client_connect_async(
    client: *mut c_void,
    server_addr: *const c_char,
    pairing_token: *const c_char,
    api_key: *const c_char,
    callback: MotionStageConnectCallback,
    context: *mut c_void,
) {
    let server_addr = match unsafe { read_required_cstr(server_addr, "server_addr") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, c.as_ptr(), context) };
            return;
        }
    };
    let pairing_token = match unsafe { read_optional_cstr(pairing_token, "pairing_token") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, c.as_ptr(), context) };
            return;
        }
    };
    let api_key = match unsafe { read_optional_cstr(api_key, "api_key") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, c.as_ptr(), context) };
            return;
        }
    };

    // Lock briefly to extract params and check state.
    let (device_id, device_name, qualified_outputs, handshake_timeout) = {
        let mut inner = match lock_client(client) {
            Ok(c) => c,
            Err(status) => {
                unsafe { callback(status, ptr::null(), context) };
                return;
            }
        };
        if inner.session.is_some() {
            let msg = CString::new("client is already connected").unwrap_or_default();
            inner.fail(
                MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED,
                "client is already connected",
            );
            unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED, msg.as_ptr(), context) };
            return;
        }
        (
            inner.device_id,
            inner.device_name.clone(),
            inner.qualified_outputs.clone(),
            inner.handshake_timeout,
        )
    };

    // Cast raw pointers to usize so they are `Send` and can be moved into the async task.
    // Safety: caller guarantees the client and context remain valid until the callback fires.
    let client_addr = client as usize;
    let context_addr = context as usize;

    get_runtime().spawn(async move {
        let result = connect_impl(
            device_id,
            device_name,
            qualified_outputs,
            server_addr,
            pairing_token,
            api_key,
            handshake_timeout,
        )
        .await;

        let ctx = context_addr as *mut c_void;

        match result {
            Ok(session) => {
                // Lock briefly to store session.
                let client_ref =
                    unsafe { &*(client_addr as *mut MotionStageSwiftClient) };
                let mut inner = client_ref
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inner.session = Some(session);
                inner.clear_error();
                drop(inner);

                // Start ping heartbeat thread.
                client_ref.ping_shutdown.store(false, Ordering::Relaxed);
                client_ref.last_send_ns.store(0, Ordering::Relaxed);
                let handle = start_ping_thread(
                    client_addr,
                    Arc::clone(&client_ref.last_send_ns),
                    Arc::clone(&client_ref.ping_shutdown),
                );
                *client_ref.ping_thread.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

                unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_OK, ptr::null(), ctx) };
            }
            Err(err) => {
                let status = map_connect_error(&err);
                // Lock briefly to store error.
                let client_ref =
                    unsafe { &*(client_addr as *mut MotionStageSwiftClient) };
                let mut inner = client_ref
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inner.fail(status, &err);
                drop(inner);
                let c_err = CString::new(err).unwrap_or_default();
                unsafe { callback(status, c_err.as_ptr(), ctx) };
            }
        }
    });
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_disconnect(client: *mut c_void) -> i32 {
    if client.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    // Signal ping thread to stop.
    let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };
    client_ref.ping_shutdown.store(true, Ordering::Relaxed);
    // Take the thread handle (don't join — it'll exit on next sleep cycle).
    let _ = client_ref
        .ping_thread
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();

    let mut inner = match lock_client(client) {
        Ok(c) => c,
        Err(status) => return status,
    };

    inner.disconnect();
    inner.clear_error();
    MOTIONSTAGE_SWIFT_STATUS_OK
}

// ---------------------------------------------------------------------------
// FFI: General batch send (2.1)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn motionstage_swift_client_send_batch(
    client: *mut c_void,
    updates: *const MotionAttributeUpdateC,
    update_count: u32,
) -> i32 {
    if updates.is_null() || update_count == 0 {
        return MOTIONSTAGE_SWIFT_STATUS_OK;
    }

    let inner = match lock_client(client) {
        Ok(c) => c,
        Err(status) => return status,
    };

    let mut frames: Vec<AttributeUpdateFrame> = Vec::with_capacity(update_count as usize);

    for i in 0..update_count as usize {
        let update = unsafe { &*updates.add(i) };

        let attr_str = match unsafe { read_required_cstr(update.attribute, "attribute") } {
            Ok(s) => s,
            Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
        };

        let qualified = qualify_source_output(inner.device_id, &attr_str);

        if update.data.is_null() {
            return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
        }

        let value = match update.component_count {
            1 => {
                let v = unsafe { *update.data };
                AttributeValueFrame::Float32(v)
            }
            2 => {
                let s = unsafe { std::slice::from_raw_parts(update.data, 2) };
                AttributeValueFrame::Vec2f([s[0], s[1]])
            }
            3 => {
                let s = unsafe { std::slice::from_raw_parts(update.data, 3) };
                AttributeValueFrame::Vec3f([s[0], s[1], s[2]])
            }
            4 => {
                let s = unsafe { std::slice::from_raw_parts(update.data, 4) };
                AttributeValueFrame::Quatf([s[0], s[1], s[2], s[3]])
            }
            16 => {
                let s = unsafe { std::slice::from_raw_parts(update.data, 16) };
                AttributeValueFrame::Mat4f([
                    [s[0], s[1], s[2], s[3]],
                    [s[4], s[5], s[6], s[7]],
                    [s[8], s[9], s[10], s[11]],
                    [s[12], s[13], s[14], s[15]],
                ])
            }
            _ => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
        };

        frames.push(AttributeUpdateFrame {
            output_attribute: qualified,
            value,
        });
    }

    match inner.send_datagram_ref(&frames) {
        Ok(()) => MOTIONSTAGE_SWIFT_STATUS_OK,
        Err(err) if err.contains("not connected") => MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED,
        Err(_) => MOTIONSTAGE_SWIFT_STATUS_TRANSPORT,
    }
}

// ---------------------------------------------------------------------------
// FFI: Motion data (legacy single-attribute)
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_send_vec3f(
    client: *mut c_void,
    x: f32,
    y: f32,
    z: f32,
) -> i32 {
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.send_vec3f(x, y, z) {
        Ok(()) => {
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

// ---------------------------------------------------------------------------
// FFI: Motion data (multi-attribute)
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_send_motion_frame(
    client: *mut c_void,
    frame: *const MotionFrameFFI,
) -> i32 {
    if frame.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    let frame = unsafe { &*frame };
    match client.send_motion_frame(frame) {
        Ok(()) => MOTIONSTAGE_SWIFT_STATUS_OK,
        Err(err) if err.contains("not connected") => MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED,
        Err(_) => MOTIONSTAGE_SWIFT_STATUS_TRANSPORT,
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_send_named_vec3f(
    client: *mut c_void,
    attribute: *const c_char,
    x: f32,
    y: f32,
    z: f32,
) -> i32 {
    let attribute = match unsafe { read_required_cstr(attribute, "attribute") } {
        Ok(value) => value,
        Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };

    let client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    let qualified = qualify_source_output(client.device_id, &attribute);
    match client.send_named_vec3f(&qualified, x, y, z) {
        Ok(()) => MOTIONSTAGE_SWIFT_STATUS_OK,
        Err(err) if err.contains("not connected") => MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED,
        Err(_) => MOTIONSTAGE_SWIFT_STATUS_TRANSPORT,
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_send_named_quatf(
    client: *mut c_void,
    attribute: *const c_char,
    x: f32,
    y: f32,
    z: f32,
    w: f32,
) -> i32 {
    let attribute = match unsafe { read_required_cstr(attribute, "attribute") } {
        Ok(value) => value,
        Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };

    let client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    let qualified = qualify_source_output(client.device_id, &attribute);
    match client.send_named_quatf(&qualified, x, y, z, w) {
        Ok(()) => MOTIONSTAGE_SWIFT_STATUS_OK,
        Err(err) if err.contains("not connected") => MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED,
        Err(_) => MOTIONSTAGE_SWIFT_STATUS_TRANSPORT,
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_send_named_float32(
    client: *mut c_void,
    attribute: *const c_char,
    value: f32,
) -> i32 {
    let attribute = match unsafe { read_required_cstr(attribute, "attribute") } {
        Ok(value) => value,
        Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };

    let client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    let qualified = qualify_source_output(client.device_id, &attribute);
    match client.send_named_float32(&qualified, value) {
        Ok(()) => MOTIONSTAGE_SWIFT_STATUS_OK,
        Err(err) if err.contains("not connected") => MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED,
        Err(_) => MOTIONSTAGE_SWIFT_STATUS_TRANSPORT,
    }
}

// ---------------------------------------------------------------------------
// FFI: Scene control
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_reset_scene(client: *mut c_void) -> i32 {
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.reset_scene() {
        Ok(()) => {
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

// ---------------------------------------------------------------------------
// FFI: Mode (decoupled data-flow + recording)
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_set_data_flow(
    client: *mut c_void,
    state: i32,
    out_data_flow: *mut i32,
    out_recording: *mut i32,
) -> i32 {
    if out_data_flow.is_null() || out_recording.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.set_data_flow(state) {
        Ok((df, rec)) => {
            unsafe {
                *out_data_flow = df;
                *out_recording = rec;
            }
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("invalid data-flow state") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_set_recording(
    client: *mut c_void,
    state: i32,
    out_data_flow: *mut i32,
    out_recording: *mut i32,
) -> i32 {
    if out_data_flow.is_null() || out_recording.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.set_recording(state) {
        Ok((df, rec)) => {
            unsafe {
                *out_data_flow = df;
                *out_recording = rec;
            }
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("invalid recording state") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

/// Deprecated: prefer `set_data_flow` / `set_recording`.
/// Maps legacy single-integer modes to the decoupled commands.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_set_mode(
    client: *mut c_void,
    requested_mode: i32,
    active_mode_out: *mut i32,
) -> i32 {
    if active_mode_out.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    let result = match requested_mode {
        MOTIONSTAGE_SWIFT_MODE_IDLE => {
            // Idle → set data-flow to Idle
            client.set_data_flow(MOTIONSTAGE_SWIFT_DATA_FLOW_IDLE)
        }
        MOTIONSTAGE_SWIFT_MODE_LIVE => {
            // Live → set data-flow to Live
            client.set_data_flow(MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE)
        }
        MOTIONSTAGE_SWIFT_MODE_RECORDING => {
            // Recording → set data-flow to Live, then set recording to Recording
            let (df, _) = match client.set_data_flow(MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE) {
                Ok(v) => v,
                Err(err) => return classify_mode_error(&mut client, err),
            };
            // If data-flow didn't go Live, bail early with what we got.
            if df != MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE {
                unsafe { *active_mode_out = MOTIONSTAGE_SWIFT_MODE_LIVE; }
                client.clear_error();
                return MOTIONSTAGE_SWIFT_STATUS_OK;
            }
            client.set_recording(MOTIONSTAGE_SWIFT_RECORDING_RECORDING)
        }
        MOTIONSTAGE_SWIFT_MODE_PLAYBACK => {
            // Playback → set data-flow to Live, then set recording to Playback
            let (df, _) = match client.set_data_flow(MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE) {
                Ok(v) => v,
                Err(err) => return classify_mode_error(&mut client, err),
            };
            if df != MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE {
                unsafe { *active_mode_out = MOTIONSTAGE_SWIFT_MODE_LIVE; }
                client.clear_error();
                return MOTIONSTAGE_SWIFT_STATUS_OK;
            }
            client.set_recording(MOTIONSTAGE_SWIFT_RECORDING_PLAYBACK)
        }
        _ => Err(format!("invalid mode value `{requested_mode}`")),
    };

    match result {
        Ok((df, rec)) => {
            // Convert the two-axis result back to a legacy single integer.
            let mode = Mode {
                data_flow: if df == MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE {
                    DataFlowState::Live
                } else {
                    DataFlowState::Idle
                },
                recording: match rec {
                    MOTIONSTAGE_SWIFT_RECORDING_RECORDING => RecordingState::Recording,
                    MOTIONSTAGE_SWIFT_RECORDING_PLAYBACK => RecordingState::Playback,
                    _ => RecordingState::Inactive,
                },
            };
            unsafe { *active_mode_out = mode_to_legacy_i32(&mode); }
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) => classify_mode_error(&mut client, err),
    }
}

/// Classify an error string from the mode helpers into an FFI status code.
fn classify_mode_error(client: &mut MotionStageSwiftClientInner, err: String) -> i32 {
    if err.contains("invalid mode value") || err.contains("invalid data-flow") || err.contains("invalid recording") {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
    } else if err.contains("not connected") {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
    } else if err.contains("rejected") {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err)
    } else {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err)
    }
}

// ---------------------------------------------------------------------------
// FFI: Accessors
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_session_id(client: *mut c_void) -> *mut c_char {
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(_) => return ptr::null_mut(),
    };

    let session_id = match client.session.as_ref() {
        Some(session) => session.session_id.to_string(),
        None => {
            client.last_error = Some("client is not connected".to_owned());
            return ptr::null_mut();
        }
    };

    into_c_string_ptr(&session_id)
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_device_id(client: *mut c_void) -> *mut c_char {
    let client = match lock_client(client) {
        Ok(client) => client,
        Err(_) => return ptr::null_mut(),
    };

    into_c_string_ptr(&client.device_id.to_string())
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_last_error(client: *mut c_void) -> *mut c_char {
    let client = match lock_client(client) {
        Ok(client) => client,
        Err(_) => return ptr::null_mut(),
    };

    match client.last_error.as_deref() {
        Some(message) => into_c_string_ptr(message),
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }

    unsafe {
        let _ = CString::from_raw(value);
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use motionstage_server::{ServerConfig, ServerHandle};

    fn ptr_to_string_and_free(value: *mut c_char) -> Option<String> {
        if value.is_null() {
            return None;
        }

        let rendered = unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned();
        motionstage_swift_string_free(value);
        Some(rendered)
    }

    #[test]
    fn qualify_source_output_applies_device_prefix_once() {
        let device = Uuid::parse_str("018f5ca9-e8f4-7fd3-a923-4b7a25a6f4df").unwrap();
        let output = qualify_source_output(device, "camera.position");
        assert_eq!(output, format!("{device}.camera.position"));

        let already_qualified = qualify_source_output(device, &output);
        assert_eq!(already_qualified, output);
    }

    #[test]
    fn ffi_client_connects_and_sends_motion_to_server() {
        let rt = Runtime::new().expect("runtime builds");
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;

        let server = ServerHandle::new(config);
        rt.block_on(server.start()).expect("server starts");
        let server_addr = rt.block_on(server.quic_bind_addr()).to_string();

        let device_name = CString::new("ios-client").unwrap();
        let output_attribute = CString::new("camera.position").unwrap();
        let client = motionstage_swift_client_new(device_name.as_ptr(), output_attribute.as_ptr());
        assert!(!client.is_null());

        let server_addr = CString::new(server_addr).unwrap();
        let connect_status = motionstage_swift_client_connect(
            client,
            server_addr.as_ptr(),
            ptr::null(),
            ptr::null(),
        );
        let connect_error = ptr_to_string_and_free(motionstage_swift_client_last_error(client));
        assert_eq!(
            connect_status,
            MOTIONSTAGE_SWIFT_STATUS_OK,
            "connect failed: {}",
            connect_error.unwrap_or_else(|| "<no error>".to_owned())
        );

        let session_id = ptr_to_string_and_free(motionstage_swift_client_session_id(client));
        assert!(session_id.is_some());

        let mut out_df = MOTIONSTAGE_SWIFT_DATA_FLOW_IDLE;
        let mut out_rec = MOTIONSTAGE_SWIFT_RECORDING_INACTIVE;
        let mode_status = motionstage_swift_client_set_data_flow(
            client,
            MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE,
            &mut out_df,
            &mut out_rec,
        );
        assert_eq!(mode_status, MOTIONSTAGE_SWIFT_STATUS_OK);
        assert_eq!(out_df, MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE);
        assert_eq!(out_rec, MOTIONSTAGE_SWIFT_RECORDING_INACTIVE);

        let send_status = motionstage_swift_client_send_vec3f(client, 1.0, 2.0, 3.0);
        assert_eq!(send_status, MOTIONSTAGE_SWIFT_STATUS_OK);

        let disconnect_status = motionstage_swift_client_disconnect(client);
        assert_eq!(disconnect_status, MOTIONSTAGE_SWIFT_STATUS_OK);

        motionstage_swift_client_free(client);
        rt.block_on(server.stop()).expect("server stops");
    }

    #[test]
    fn ffi_multi_client_connects_and_sends_motion_frame() {
        let rt = Runtime::new().expect("runtime builds");
        let mut config = ServerConfig::default();
        config.quic_bind_addr = "127.0.0.1:0".parse().unwrap();
        config.enable_discovery = false;

        let server = ServerHandle::new(config);
        rt.block_on(server.start()).expect("server starts");
        let server_addr = rt.block_on(server.quic_bind_addr()).to_string();

        let device_name = CString::new("ios-motion-device").unwrap();
        let attrs = CString::new(
            "motion.position,motion.rotation,motion.velocity,camera.focal_length,camera.focus_distance,camera.aperture"
        ).unwrap();
        let client = motionstage_swift_client_new_multi(device_name.as_ptr(), attrs.as_ptr());
        assert!(!client.is_null());

        let server_addr_c = CString::new(server_addr).unwrap();
        let connect_status = motionstage_swift_client_connect(
            client,
            server_addr_c.as_ptr(),
            ptr::null(),
            ptr::null(),
        );
        assert_eq!(connect_status, MOTIONSTAGE_SWIFT_STATUS_OK);

        let mut out_df = MOTIONSTAGE_SWIFT_DATA_FLOW_IDLE;
        let mut out_rec = MOTIONSTAGE_SWIFT_RECORDING_INACTIVE;
        let mode_status = motionstage_swift_client_set_data_flow(
            client,
            MOTIONSTAGE_SWIFT_DATA_FLOW_LIVE,
            &mut out_df,
            &mut out_rec,
        );
        assert_eq!(mode_status, MOTIONSTAGE_SWIFT_STATUS_OK);

        let frame = MotionFrameFFI {
            position: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            velocity: [0.1, 0.2, 0.3],
            focal_length: 50.0,
            focus_distance: 1.5,
            aperture: 2.8,
            field_mask: MOTIONSTAGE_SWIFT_FIELD_POSITION
                | MOTIONSTAGE_SWIFT_FIELD_ROTATION
                | MOTIONSTAGE_SWIFT_FIELD_VELOCITY
                | MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH,
        };

        let send_status = motionstage_swift_client_send_motion_frame(client, &frame);
        assert_eq!(send_status, MOTIONSTAGE_SWIFT_STATUS_OK);

        let disconnect_status = motionstage_swift_client_disconnect(client);
        assert_eq!(disconnect_status, MOTIONSTAGE_SWIFT_STATUS_OK);

        motionstage_swift_client_free(client);
        rt.block_on(server.stop()).expect("server stops");
    }

    #[test]
    fn ffi_new_v2_creates_client() {
        let device_name = CString::new("test-device").unwrap();
        let attr1 = CString::new("motion.position").unwrap();
        let attr2 = CString::new("motion.rotation").unwrap();
        let names: &[*const c_char] = &[attr1.as_ptr(), attr2.as_ptr()];

        let client = motionstage_swift_client_new_v2(
            device_name.as_ptr(),
            2,
            names.as_ptr(),
        );
        assert!(!client.is_null());
        motionstage_swift_client_free(client);
    }
}
