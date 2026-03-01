use std::{
    ffi::{CStr, CString},
    net::SocketAddr,
    os::raw::{c_char, c_void},
    ptr,
    str::FromStr,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use motionstage_protocol::{
    BaselineAction, ClientHello, ClientRole, ControlMessage, Feature, Mode, RegisterRequest,
    PROTOCOL_MAJOR, PROTOCOL_MINOR,
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

pub const MOTIONSTAGE_SWIFT_MODE_IDLE: i32 = 0;
pub const MOTIONSTAGE_SWIFT_MODE_LIVE: i32 = 1;
pub const MOTIONSTAGE_SWIFT_MODE_RECORDING: i32 = 2;
pub const MOTIONSTAGE_SWIFT_MODE_PLAYBACK: i32 = 3;

pub const MOTIONSTAGE_SWIFT_FIELD_POSITION: u32 = 0x01;
pub const MOTIONSTAGE_SWIFT_FIELD_ROTATION: u32 = 0x02;
pub const MOTIONSTAGE_SWIFT_FIELD_VELOCITY: u32 = 0x04;
pub const MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH: u32 = 0x08;
pub const MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE: u32 = 0x10;
pub const MOTIONSTAGE_SWIFT_FIELD_APERTURE: u32 = 0x20;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MODE_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const RESET_SCENE_TIMEOUT: Duration = Duration::from_secs(5);

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

pub struct MotionStageSwiftClient {
    inner: Mutex<MotionStageSwiftClientInner>,
}

struct MotionStageSwiftClientInner {
    runtime: Runtime,
    device_id: Uuid,
    device_name: String,
    _source_outputs: Vec<String>,
    qualified_outputs: Vec<String>,
    session: Option<ConnectedSession>,
    last_error: Option<String>,
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
            let _ = session.control.finish();
        }
    }

    fn connect(
        &mut self,
        server_addr: &str,
        pairing_token: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<(), String> {
        if self.session.is_some() {
            return Err("client is already connected".to_owned());
        }

        let server_addr = SocketAddr::from_str(server_addr)
            .map_err(|err| format!("invalid server address `{server_addr}`: {err}"))?;

        let (endpoint, peer, mut control) = self.runtime.block_on(async {
            let endpoint = QuicClient::new_insecure_for_local_dev()
                .map_err(|err| format!("failed to create QUIC client endpoint: {err}"))?;
            let peer = endpoint
                .connect(server_addr)
                .await
                .map_err(|err| format!("failed to connect QUIC client to {server_addr}: {err}"))?;
            let control = peer
                .accept_control_stream()
                .await
                .map_err(|err| format!("failed to accept control stream: {err}"))?;
            Ok::<(QuicClient, QuicPeer, ControlChannel), String>((endpoint, peer, control))
        })?;

        let first_message = self.runtime.block_on(async {
            timeout(HANDSHAKE_TIMEOUT, control.recv())
                .await
                .map_err(|_| "timed out waiting for ServerHello".to_owned())?
                .map_err(|err| format!("failed to receive ServerHello: {err}"))
        })?;

        match first_message {
            ControlMessage::ServerHello(_) => {}
            other => {
                return Err(format!(
                    "expected ServerHello as first control message, got {other:?}"
                ));
            }
        }

        self.runtime
            .block_on(control.send(&ControlMessage::ClientHello(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id: self.device_id,
                device_name: self.device_name.clone(),
                roles: vec![ClientRole::MotionSource, ClientRole::Operator],
                features: vec![Feature::Motion, Feature::Mapping, Feature::Recording],
                advertised_attributes: self.qualified_outputs.clone(),
            })))
            .map_err(|err| format!("failed to send ClientHello: {err}"))?;

        self.runtime
            .block_on(
                control.send(&ControlMessage::RegisterRequest(RegisterRequest {
                    pairing_token: pairing_token.map(ToOwned::to_owned),
                    api_key: api_key.map(ToOwned::to_owned),
                })),
            )
            .map_err(|err| format!("failed to send RegisterRequest: {err}"))?;

        let register_message = self.runtime.block_on(async {
            timeout(HANDSHAKE_TIMEOUT, control.recv())
                .await
                .map_err(|_| "timed out waiting for registration response".to_owned())?
                .map_err(|err| format!("failed to receive registration response: {err}"))
        })?;

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

        self.session = Some(ConnectedSession {
            _endpoint: endpoint,
            peer,
            control,
            session_id,
        });

        Ok(())
    }

    fn send_vec3f(&mut self, x: f32, y: f32, z: f32) -> Result<(), String> {
        let first_output = self
            .qualified_outputs
            .first()
            .ok_or_else(|| "no output attributes configured".to_owned())?
            .clone();

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
                    output_attribute: attr.clone(),
                    value: AttributeValueFrame::Vec3f(frame.position),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_ROTATION != 0 {
            if let Some(attr) = self.qualified_output_for(1) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.clone(),
                    value: AttributeValueFrame::Quatf(frame.rotation),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_VELOCITY != 0 {
            if let Some(attr) = self.qualified_output_for(2) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.clone(),
                    value: AttributeValueFrame::Vec3f(frame.velocity),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_FOCAL_LENGTH != 0 {
            if let Some(attr) = self.qualified_output_for(3) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.clone(),
                    value: AttributeValueFrame::Float32(frame.focal_length),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_FOCUS_DISTANCE != 0 {
            if let Some(attr) = self.qualified_output_for(4) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.clone(),
                    value: AttributeValueFrame::Float32(frame.focus_distance),
                });
            }
        }
        if mask & MOTIONSTAGE_SWIFT_FIELD_APERTURE != 0 {
            if let Some(attr) = self.qualified_output_for(5) {
                updates.push(AttributeUpdateFrame {
                    output_attribute: attr.clone(),
                    value: AttributeValueFrame::Float32(frame.aperture),
                });
            }
        }

        if updates.is_empty() {
            return Ok(());
        }

        self.send_datagram_ref(&updates)
    }

    fn qualified_output_for(&self, index: usize) -> Option<&String> {
        self.qualified_outputs.get(index)
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
            .map_err(|err| format!("failed to send motion datagram: {err}"))
    }

    fn send_datagram_ref(&self, updates: &[AttributeUpdateFrame]) -> Result<(), String> {
        self.send_datagram(updates.to_vec())
    }

    fn reset_scene(&mut self) -> Result<(), String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        self.runtime
            .block_on(
                session
                    .control
                    .send(&ControlMessage::ResetSceneToBaseline { scene_id: None }),
            )
            .map_err(|err| format!("failed to send ResetSceneToBaseline: {err}"))?;

        self.runtime.block_on(async {
            loop {
                let message = timeout(RESET_SCENE_TIMEOUT, session.control.recv())
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

    fn set_mode(&mut self, requested_mode: i32) -> Result<i32, String> {
        let requested_mode = parse_mode(requested_mode)?;

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        self.runtime
            .block_on(
                session
                    .control
                    .send(&ControlMessage::SetMode(requested_mode)),
            )
            .map_err(|err| format!("failed to send mode request: {err}"))?;

        let active_mode = self.runtime.block_on(async {
            loop {
                let message = timeout(MODE_REPLY_TIMEOUT, session.control.recv())
                    .await
                    .map_err(|_| "timed out waiting for mode response".to_owned())?
                    .map_err(|err| format!("failed to receive mode response: {err}"))?;

                match message {
                    ControlMessage::ModeState(active_mode) => {
                        return Ok(active_mode_to_i32(active_mode))
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
        })?;

        Ok(active_mode)
    }
}

fn parse_mode(mode: i32) -> Result<Mode, String> {
    match mode {
        MOTIONSTAGE_SWIFT_MODE_IDLE => Ok(Mode::Idle),
        MOTIONSTAGE_SWIFT_MODE_LIVE => Ok(Mode::Live),
        MOTIONSTAGE_SWIFT_MODE_RECORDING => Ok(Mode::Recording),
        MOTIONSTAGE_SWIFT_MODE_PLAYBACK => Ok(Mode::Playback),
        _ => Err(format!("invalid mode value `{mode}`")),
    }
}

fn active_mode_to_i32(mode: Mode) -> i32 {
    match mode {
        Mode::Idle => MOTIONSTAGE_SWIFT_MODE_IDLE,
        Mode::Live => MOTIONSTAGE_SWIFT_MODE_LIVE,
        Mode::Recording => MOTIONSTAGE_SWIFT_MODE_RECORDING,
        Mode::Playback => MOTIONSTAGE_SWIFT_MODE_PLAYBACK,
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
    source_outputs: Vec<String>,
) -> Result<MotionStageSwiftClientInner, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to create tokio runtime: {err}"))?;

    let device_id = Uuid::now_v7();
    let qualified_outputs: Vec<String> = source_outputs
        .iter()
        .map(|attr| qualify_source_output(device_id, attr))
        .collect();

    Ok(MotionStageSwiftClientInner {
        runtime,
        device_id,
        device_name,
        _source_outputs: source_outputs,
        qualified_outputs,
        session: None,
        last_error: None,
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

    let inner = match make_client_inner(device_name, vec![output_attribute]) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
}

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

    let source_outputs: Vec<String> = csv
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    if source_outputs.is_empty() {
        return ptr::null_mut();
    }

    let inner = match make_client_inner(device_name, source_outputs) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_free(client: *mut c_void) {
    if client.is_null() {
        return;
    }

    unsafe {
        drop(Box::from_raw(client as *mut MotionStageSwiftClient));
    }
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

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.connect(&server_addr, pairing_token.as_deref(), api_key.as_deref()) {
        Ok(()) => {
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("already connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED, err)
        }
        Err(err) if err.contains("invalid server address") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("registration rejected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err)
        }
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_disconnect(client: *mut c_void) -> i32 {
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    client.disconnect();
    client.clear_error();
    MOTIONSTAGE_SWIFT_STATUS_OK
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
// FFI: Mode
// ---------------------------------------------------------------------------

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

    match client.set_mode(requested_mode) {
        Ok(active_mode) => {
            unsafe {
                *active_mode_out = active_mode;
            }
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("invalid mode value") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
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

        let mut active_mode = MOTIONSTAGE_SWIFT_MODE_IDLE;
        let mode_status = motionstage_swift_client_set_mode(
            client,
            MOTIONSTAGE_SWIFT_MODE_LIVE,
            &mut active_mode,
        );
        assert_eq!(mode_status, MOTIONSTAGE_SWIFT_STATUS_OK);
        assert_eq!(active_mode, MOTIONSTAGE_SWIFT_MODE_LIVE);

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

        let mut active_mode = MOTIONSTAGE_SWIFT_MODE_IDLE;
        let mode_status = motionstage_swift_client_set_mode(
            client,
            MOTIONSTAGE_SWIFT_MODE_LIVE,
            &mut active_mode,
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
}
