// FFI functions take raw pointers by design; callers are responsible for validity.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::{
    collections::VecDeque,
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
    DataFlowState, Feature, IceCandidate, MappingSummary, Mode, RecordingState, RegisterRequest,
    SceneSnapshotPayload, SdpMessage, SdpType, SignalMessage, SignalPayload, StateEvent,
    StateEventEnvelope, TakeInfo, VideoStreamStatus, WireError, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use motionstage_transport_quic::{
    hex_decode_fingerprint, AttributeUpdateFrame, AttributeValueFrame, ControlChannel, QuicClient,
    QuicPeer,
};
use serde::Serialize;
use tokio::runtime::Runtime;
use tokio::time::{timeout, Instant};
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

/// Connection state constants (4.2)
pub const MOTIONSTAGE_SWIFT_CONNECTION_DISCONNECTED: i32 = 0;
pub const MOTIONSTAGE_SWIFT_CONNECTION_CONNECTED: i32 = 1;
pub const MOTIONSTAGE_SWIFT_CONNECTION_RECONNECTING: i32 = 2;
pub const MOTIONSTAGE_SWIFT_CONNECTION_FAILED: i32 = 3;

/// Connection event constants (4.2)
pub const MOTIONSTAGE_SWIFT_EVENT_CONNECTED: i32 = 0;
pub const MOTIONSTAGE_SWIFT_EVENT_DISCONNECTED: i32 = 1;
pub const MOTIONSTAGE_SWIFT_EVENT_RECONNECTING: i32 = 2;
pub const MOTIONSTAGE_SWIFT_EVENT_RECONNECT_FAILED: i32 = 3;

pub const MOTIONSTAGE_SWIFT_SDP_TYPE_OFFER: i32 = 0;
pub const MOTIONSTAGE_SWIFT_SDP_TYPE_ANSWER: i32 = 1;

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

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum VideoSignalJson {
    Sdp {
        from_device: String,
        to_device: String,
        sdp_type: String,
        sdp: String,
    },
    Ice {
        from_device: String,
        to_device: String,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
}

// ---------------------------------------------------------------------------
// Callback type for async connect (2.4)
// ---------------------------------------------------------------------------

pub type MotionStageConnectCallback =
    unsafe extern "C" fn(status: i32, error: *const c_char, context: *mut c_void);

/// Connection event callback (4.2).
/// `event`: MOTIONSTAGE_SWIFT_EVENT_* constant.
/// `attempt`: reconnect attempt number (0 for connected/disconnected events).
/// `message`: optional human-readable message (NULL if none). Do NOT free.
pub type MotionStageConnectionEventCallback =
    unsafe extern "C" fn(event: i32, attempt: u32, message: *const c_char, context: *mut c_void);

/// State-event stream callback.
///
/// `message_json` is a NUL-terminated UTF-8 JSON document owned by the SDK for
/// the duration of the call — do NOT free it and do NOT retain the pointer
/// past the callback (copy the string if needed). Invoked from a background
/// SDK thread. Two message shapes are delivered (see the header for the full
/// schema):
///
/// ```json
/// {"kind":"state_event","seq":7,"origin_session":"<uuid>|null",
///  "timestamp_ns":123,"event":{"type":"ModeChanged","data":{...}}}
/// {"kind":"scene_snapshot","snapshot":{...SceneSnapshotPayload...}}
/// ```
pub type MotionStageStateEventCallback =
    unsafe extern "C" fn(message_json: *const c_char, context: *mut c_void);

// ---------------------------------------------------------------------------
// Reconnect policy (4.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ReconnectPolicy {
    max_attempts: u32,
    initial_delay: Duration,
    max_delay: Duration,
    backoff_factor: f64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            backoff_factor: 2.0,
        }
    }
}

/// Saved connection parameters for reconnection.
#[derive(Clone)]
struct ConnectParams {
    server_addr: String,
    pairing_token: Option<String>,
    api_key: Option<String>,
    /// If `Some`, use pinned cert; otherwise use insecure.
    fingerprint: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
#[allow(dead_code)]
enum ConnectionState {
    Disconnected = 0,
    Connected = 1,
    Reconnecting = 2,
    Failed = 3,
}

// ---------------------------------------------------------------------------
// Internal client structs
// ---------------------------------------------------------------------------

pub struct MotionStageSwiftClient {
    inner: Mutex<MotionStageSwiftClientInner>,
    /// Nanosecond timestamp of the last motion datagram send. Shared with monitor thread.
    last_send_ns: Arc<AtomicU64>,
    /// Signal to stop the connection monitor thread.
    ping_shutdown: Arc<AtomicBool>,
    /// Join handle for the background connection monitor thread (set after connect).
    ping_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Current connection state (atomic for lock-free reads).
    connection_state: Arc<std::sync::atomic::AtomicI32>,
    /// Reconnection policy (protected by mutex for thread-safe mutation).
    reconnect_policy: Mutex<Option<ReconnectPolicy>>,
    /// Saved connection parameters for reconnection.
    connect_params: Mutex<Option<ConnectParams>>,
    /// Connection event callback and context.
    event_callback: Mutex<Option<(MotionStageConnectionEventCallback, usize)>>,
    /// State-event stream callback and context (see `MotionStageStateEventCallback`).
    state_event_callback: Mutex<Option<(MotionStageStateEventCallback, usize)>>,
    /// True while a state-event callback is registered. Shared with the inner
    /// so incoming state-stream messages are not queued when no one is
    /// subscribed (finding: unsubscribed clients must not accumulate).
    state_event_subscribed: Arc<AtomicBool>,
    /// Bumped on every `set_state_event_callback` call. The pump snapshots it
    /// before a dispatch batch and re-checks it before each invocation, so a
    /// NULL/swap mid-batch halts further calls with the stale context. Combined
    /// with holding the callback lock across the batch, this makes a NULL
    /// callback reliably quiesce delivery before it returns.
    state_event_epoch: Arc<AtomicU64>,
    /// Signal to stop the state-event pump thread.
    state_event_shutdown: Arc<AtomicBool>,
    /// Join handle for the state-event pump thread (spawned on first subscribe).
    state_event_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
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
    pending_mode_states: VecDeque<Mode>,
    pending_video_offers: VecDeque<SdpMessage>,
    pending_video_signals: VecDeque<SignalMessage>,
    pending_video_statuses: VecDeque<VideoStreamStatus>,
    /// StateEventMsg envelopes and unsolicited SceneSnapshots awaiting delivery
    /// to the state-event callback, in a **single arrival-ordered queue**. The
    /// wire ordering between a lag-recovery `SceneSnapshot` and the events that
    /// follow it is load-bearing (the `seq <= snapshot.seq` folding rule), so
    /// the two kinds must not be split into separate queues and reordered.
    pending_state_stream: VecDeque<StateStreamItem>,
    /// Shared with the owning client's `set_state_event_callback`: true while a
    /// state-event callback is registered. When false, incoming state-stream
    /// messages are dropped instead of queued (an unsubscribed client must not
    /// accumulate an unbounded backlog); a later subscriber recovers via the
    /// normal lag→snapshot resync path.
    state_event_subscribed: Arc<AtomicBool>,
    /// Count of state-stream messages dropped while unsubscribed, for the
    /// periodic drop warning.
    dropped_unsubscribed_events: u64,
}

/// One item in the ordered state-event stream queue: either a replication
/// event or a full-world snapshot. Kept in one queue so arrival order between
/// the two is preserved (finding: a snapshot followed by its events must be
/// delivered in that order).
enum StateStreamItem {
    Event(StateEventEnvelope),
    Snapshot(SceneSnapshotPayload),
}

struct ConnectedSession {
    _endpoint: QuicClient,
    peer: QuicPeer,
    control: ControlChannel,
    session_id: Uuid,
}

impl MotionStageSwiftClientInner {
    fn stash_control_message(&mut self, message: ControlMessage) {
        match message {
            ControlMessage::ModeState(mode) => {
                self.pending_mode_states.push_back(mode);
            }
            ControlMessage::VideoOffer(offer) => {
                self.pending_video_offers.push_back(offer);
            }
            ControlMessage::SignalsBatch(signals) => {
                self.pending_video_signals.extend(signals);
            }
            ControlMessage::VideoStreamStatus(status) => {
                self.pending_video_statuses.push_back(status);
            }
            ControlMessage::StateEventMsg(envelope) => {
                self.queue_state_event(envelope);
            }
            ControlMessage::SceneSnapshot(payload) => {
                self.queue_snapshot(payload);
            }
            _ => {}
        }
    }

    /// Queue a replication event for the state-event callback, preserving
    /// arrival order relative to snapshots. Dropped (not queued) when no
    /// callback is registered, so an unsubscribed client never accumulates.
    fn queue_state_event(&mut self, envelope: StateEventEnvelope) {
        if self.state_event_subscribed.load(Ordering::Relaxed) {
            self.pending_state_stream
                .push_back(StateStreamItem::Event(envelope));
        } else {
            self.note_dropped_unsubscribed_event();
        }
    }

    /// Queue an unsolicited snapshot for the state-event callback, preserving
    /// arrival order relative to events. Dropped when no callback is
    /// registered.
    fn queue_snapshot(&mut self, payload: SceneSnapshotPayload) {
        if self.state_event_subscribed.load(Ordering::Relaxed) {
            self.pending_state_stream
                .push_back(StateStreamItem::Snapshot(payload));
        } else {
            self.note_dropped_unsubscribed_event();
        }
    }

    /// Record and periodically warn about state-stream messages dropped
    /// because no callback is registered.
    fn note_dropped_unsubscribed_event(&mut self) {
        self.dropped_unsubscribed_events += 1;
        if self.dropped_unsubscribed_events.is_power_of_two() {
            eprintln!(
                "motionstage: dropped {} state-stream message(s) with no state-event \
                 callback registered; subscribe to receive replication (a fresh \
                 snapshot resyncs on subscribe)",
                self.dropped_unsubscribed_events
            );
        }
    }

    fn drain_control_backlog(&mut self) -> Result<(), String> {
        let session = match self.session.as_mut() {
            Some(session) => session,
            None => return Ok(()),
        };

        let mut drained = Vec::new();
        get_runtime().block_on(async {
            loop {
                let recv = timeout(Duration::from_millis(1), session.control.recv()).await;
                match recv {
                    Ok(Ok(message)) => drained.push(message),
                    Ok(Err(err)) => {
                        return Err(format!("failed to drain control backlog: {err}"));
                    }
                    Err(_) => return Ok(()),
                }
            }
        })?;

        for message in drained {
            self.stash_control_message(message);
        }
        Ok(())
    }

    fn recv_control_with_timeout(
        &mut self,
        timeout_dur: Duration,
    ) -> Result<ControlMessage, String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime().block_on(async {
            timeout(timeout_dur, session.control.recv())
                .await
                .map_err(|_| "timed out waiting for control response".to_owned())?
                .map_err(|err| format!("failed to receive control response: {err}"))
        })
    }

    fn pop_mode_state_matching<F>(&mut self, accept: F) -> Option<(i32, i32)>
    where
        F: Fn(&Mode) -> bool,
    {
        let index = self.pending_mode_states.iter().position(accept)?;
        let mode = self.pending_mode_states.remove(index)?;
        Some((mode_to_data_flow_i32(&mode), mode_to_recording_i32(&mode)))
    }

    fn sdp_type_to_i32(sdp_type: SdpType) -> i32 {
        match sdp_type {
            SdpType::Offer => MOTIONSTAGE_SWIFT_SDP_TYPE_OFFER,
            SdpType::Answer => MOTIONSTAGE_SWIFT_SDP_TYPE_ANSWER,
        }
    }

    fn parse_sdp_type(raw: i32) -> Result<SdpType, String> {
        match raw {
            MOTIONSTAGE_SWIFT_SDP_TYPE_OFFER => Ok(SdpType::Offer),
            MOTIONSTAGE_SWIFT_SDP_TYPE_ANSWER => Ok(SdpType::Answer),
            _ => Err(format!("invalid SDP type value `{raw}`")),
        }
    }

    fn create_video_offer(
        &mut self,
        stream_id: &str,
        track_id: &str,
    ) -> Result<SdpMessage, String> {
        self.drain_control_backlog()?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime()
            .block_on(session.control.send(&ControlMessage::CreateVideoOffer {
                stream_id: stream_id.to_owned(),
                track_id: track_id.to_owned(),
            }))
            .map_err(|err| format!("failed to send CreateVideoOffer: {err}"))?;

        if let Some(offer) = self.pending_video_offers.pop_front() {
            return Ok(offer);
        }

        let deadline = Instant::now() + self.mode_reply_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for video offer".to_owned());
            }
            match self.recv_control_with_timeout(remaining)? {
                ControlMessage::VideoOffer(offer) => return Ok(offer),
                ControlMessage::Error { code, reason } => {
                    return Err(format!(
                        "video offer request rejected: code={code:?} reason={reason}"
                    ));
                }
                ControlMessage::Pong => continue,
                other => self.stash_control_message(other),
            }
        }
    }

    fn send_video_sdp(&mut self, sdp_type: i32, sdp: &str) -> Result<(), String> {
        let payload = SignalPayload::Sdp(SdpMessage {
            ty: Self::parse_sdp_type(sdp_type)?,
            sdp: sdp.to_owned(),
        });
        self.send_video_signal(payload)
    }

    fn send_video_ice(
        &mut self,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<(), String> {
        let payload = SignalPayload::Ice(IceCandidate {
            candidate: candidate.to_owned(),
            sdp_mid,
            sdp_mline_index,
        });
        self.send_video_signal(payload)
    }

    fn send_video_signal(&mut self, payload: SignalPayload) -> Result<(), String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime()
            .block_on(
                session
                    .control
                    .send(&ControlMessage::VideoSignal(SignalMessage {
                        from_device: self.device_id,
                        to_device: self.device_id,
                        payload,
                    })),
            )
            .map_err(|err| format!("failed to send video signal: {err}"))
    }

    fn drain_video_signals(&mut self) -> Result<(), String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime()
            .block_on(session.control.send(&ControlMessage::DrainSignals))
            .map_err(|err| format!("failed to send DrainSignals: {err}"))?;

        let deadline = Instant::now() + self.mode_reply_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for video signals batch".to_owned());
            }
            match self.recv_control_with_timeout(remaining)? {
                ControlMessage::SignalsBatch(signals) => {
                    self.pending_video_signals.extend(signals);
                    return Ok(());
                }
                ControlMessage::Error { code, reason } => {
                    return Err(format!(
                        "drain signals rejected: code={code:?} reason={reason}"
                    ));
                }
                ControlMessage::Pong => continue,
                other => self.stash_control_message(other),
            }
        }
    }

    fn next_video_signal_json(&mut self) -> Result<Option<String>, String> {
        if self.pending_video_signals.is_empty() {
            self.drain_video_signals()?;
        }

        let Some(signal) = self.pending_video_signals.pop_front() else {
            return Ok(None);
        };

        let json = match signal.payload {
            SignalPayload::Sdp(sdp) => {
                let sdp_type = match sdp.ty {
                    SdpType::Offer => "offer".to_owned(),
                    SdpType::Answer => "answer".to_owned(),
                };
                VideoSignalJson::Sdp {
                    from_device: signal.from_device.to_string(),
                    to_device: signal.to_device.to_string(),
                    sdp_type,
                    sdp: sdp.sdp,
                }
            }
            SignalPayload::Ice(ice) => VideoSignalJson::Ice {
                from_device: signal.from_device.to_string(),
                to_device: signal.to_device.to_string(),
                candidate: ice.candidate,
                sdp_mid: ice.sdp_mid,
                sdp_mline_index: ice.sdp_mline_index,
            },
        };

        serde_json::to_string(&json)
            .map(Some)
            .map_err(|err| format!("failed to serialize video signal JSON: {err}"))
    }

    fn get_video_stream_status(&mut self) -> Result<VideoStreamStatus, String> {
        self.drain_control_backlog()?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;

        get_runtime()
            .block_on(session.control.send(&ControlMessage::GetVideoStreamStatus))
            .map_err(|err| format!("failed to send GetVideoStreamStatus: {err}"))?;

        if let Some(status) = self.pending_video_statuses.pop_front() {
            return Ok(status);
        }

        let deadline = Instant::now() + self.mode_reply_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for video stream status".to_owned());
            }
            match self.recv_control_with_timeout(remaining)? {
                ControlMessage::VideoStreamStatus(status) => return Ok(status),
                ControlMessage::Error { code, reason } => {
                    return Err(format!(
                        "video stream status request rejected: code={code:?} reason={reason}"
                    ));
                }
                ControlMessage::Pong => continue,
                other => self.stash_control_message(other),
            }
        }
    }

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
            let _ = get_runtime().block_on(session.control.send(&ControlMessage::ClientGoodbye {
                reason: Some("client disconnect".into()),
            }));
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
        self.drain_control_backlog()?;
        let session_id = {
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
            session.session_id
        };

        // Protocol 2.1: success has no direct reply. The acknowledgement is our
        // own StateEventMsg echo carrying BaselineApplied{action: ResetScene};
        // failure arrives as ControlMessage::Error. The retired
        // BaselineActionApplied direct ack is still accepted for older servers.
        let deadline = Instant::now() + self.reset_scene_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for baseline reset response".to_owned());
            }

            match self.recv_control_with_timeout(remaining)? {
                ControlMessage::BaselineActionApplied {
                    action: BaselineAction::ResetScene,
                    ..
                } => return Ok(()),
                ControlMessage::StateEventMsg(envelope) => {
                    let matched = envelope.origin_session == Some(session_id)
                        && matches!(
                            envelope.event,
                            StateEvent::BaselineApplied {
                                action: BaselineAction::ResetScene,
                                ..
                            }
                        );
                    // Keep the echo visible to state-event subscribers, in
                    // arrival order (dropped if none is registered).
                    self.queue_state_event(envelope);
                    if matched {
                        return Ok(());
                    }
                }
                ControlMessage::Error { code, reason } => {
                    return Err(format!(
                        "reset scene rejected: code={code:?} reason={reason}"
                    ))
                }
                ControlMessage::Pong => continue,
                other => self.stash_control_message(other),
            }
        }
    }

    fn set_data_flow(&mut self, requested: i32) -> Result<(i32, i32), String> {
        let state = parse_data_flow_state(requested)?;

        self.drain_control_backlog()?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;
        get_runtime()
            .block_on(session.control.send(&ControlMessage::SetDataFlow(state)))
            .map_err(|err| format!("failed to send data-flow request: {err}"))?;

        self.wait_for_mode_state_matching(|mode| mode.data_flow == state)
    }

    fn set_recording(&mut self, requested: i32) -> Result<(i32, i32), String> {
        let state = parse_recording_state(requested)?;

        self.drain_control_backlog()?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "client is not connected".to_owned())?;
        get_runtime()
            .block_on(session.control.send(&ControlMessage::SetRecording(state)))
            .map_err(|err| format!("failed to send recording request: {err}"))?;

        self.wait_for_mode_state_matching(|mode| mode.recording == state)
    }

    /// Wait for confirmation that the composite mode satisfies `accept` and
    /// return `(data_flow_i32, recording_i32)`.
    ///
    /// Protocol 2.1: a successful `SetDataFlow`/`SetRecording` has no direct
    /// reply — the acknowledgement is our own `StateEventMsg` echo carrying
    /// `ModeChanged`. Failures arrive as `ControlMessage::Error`. `ModeState`
    /// is still accepted (heartbeat probe / older servers).
    fn wait_for_mode_state_matching<F>(&mut self, accept: F) -> Result<(i32, i32), String>
    where
        F: Fn(&Mode) -> bool,
    {
        if let Some(mode) = self.pop_mode_state_matching(&accept) {
            return Ok(mode);
        }

        let session_id = self
            .session
            .as_ref()
            .map(|session| session.session_id)
            .ok_or_else(|| "client is not connected".to_owned())?;
        let mode_reply_timeout = self.mode_reply_timeout;
        let deadline = Instant::now() + mode_reply_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for mode response".to_owned());
            }

            match self.recv_control_with_timeout(remaining)? {
                ControlMessage::ModeState(mode) => {
                    if accept(&mode) {
                        return Ok((mode_to_data_flow_i32(&mode), mode_to_recording_i32(&mode)));
                    }
                    self.pending_mode_states.push_back(mode);
                }
                ControlMessage::StateEventMsg(envelope) => {
                    let matched_mode = match &envelope.event {
                        StateEvent::ModeChanged { mode }
                            if envelope.origin_session == Some(session_id) && accept(mode) =>
                        {
                            Some(*mode)
                        }
                        _ => None,
                    };
                    // Keep the echo visible to state-event subscribers, in
                    // arrival order (dropped if none is registered).
                    self.queue_state_event(envelope);
                    if let Some(mode) = matched_mode {
                        return Ok((mode_to_data_flow_i32(&mode), mode_to_recording_i32(&mode)));
                    }
                }
                ControlMessage::Error { code, reason } => {
                    return Err(format!(
                        "mode request rejected: code={code:?} reason={reason}"
                    ))
                }
                ControlMessage::Pong => continue,
                other => {
                    self.stash_control_message(other);
                }
            }
        }
    }

    // -- Operator plane (protocol 2.1) --------------------------------------

    /// Send `request` and wait for the message `matcher` recognises as the
    /// direct reply. Unrelated messages (state-event echoes, video signals,
    /// heartbeat probes) are stashed for their own consumers — direct replies
    /// and event echoes may interleave in any order, so we match on message
    /// type, never on position.
    fn wire_request<T>(
        &mut self,
        request: ControlMessage,
        mut matcher: impl FnMut(ControlMessage) -> WireReply<T>,
    ) -> Result<T, String> {
        self.drain_control_backlog()?;
        {
            let session = self
                .session
                .as_mut()
                .ok_or_else(|| "client is not connected".to_owned())?;
            get_runtime()
                .block_on(session.control.send(&request))
                .map_err(|err| format!("failed to send control request: {err}"))?;
        }

        let deadline = Instant::now() + self.mode_reply_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for control response".to_owned());
            }
            match matcher(self.recv_control_with_timeout(remaining)?) {
                WireReply::Done(result) => return result,
                WireReply::Other(ControlMessage::Pong) => continue,
                WireReply::Other(other) => self.stash_control_message(other),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn wire_create_mapping(
        &mut self,
        source_device: Option<Uuid>,
        source_output: String,
        target_scene: Option<Uuid>,
        target_object: Uuid,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
    ) -> Result<Result<MappingSummary, WireError>, String> {
        self.wire_request(
            ControlMessage::CreateMapping {
                source_device,
                source_output,
                target_scene,
                target_object,
                target_attribute,
                component_mask,
            },
            |message| match message {
                ControlMessage::MappingCreateResult { result } => WireReply::Done(Ok(result)),
                ControlMessage::Error { code, reason } => WireReply::Done(Err(format!(
                    "mapping request rejected: code={code:?} reason={reason}"
                ))),
                other => WireReply::Other(other),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn wire_update_mapping(
        &mut self,
        mapping_id: Uuid,
        source_device: Option<Uuid>,
        source_output: String,
        target_scene: Option<Uuid>,
        target_object: Uuid,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
    ) -> Result<Result<MappingSummary, WireError>, String> {
        self.wire_request(
            ControlMessage::UpdateMapping {
                mapping_id,
                source_device,
                source_output,
                target_scene,
                target_object,
                target_attribute,
                component_mask,
            },
            |message| match message {
                ControlMessage::MappingCreateResult { result } => WireReply::Done(Ok(result)),
                ControlMessage::Error { code, reason } => WireReply::Done(Err(format!(
                    "mapping request rejected: code={code:?} reason={reason}"
                ))),
                other => WireReply::Other(other),
            },
        )
    }

    fn wire_remove_mapping(&mut self, mapping_id: Uuid) -> Result<Result<(), WireError>, String> {
        self.wire_request(
            ControlMessage::RemoveMapping { mapping_id },
            move |message| match message {
                ControlMessage::MappingOpResult {
                    mapping_id: reply_id,
                    result,
                } if reply_id == mapping_id => WireReply::Done(Ok(result)),
                ControlMessage::Error { code, reason } => WireReply::Done(Err(format!(
                    "mapping request rejected: code={code:?} reason={reason}"
                ))),
                other => WireReply::Other(other),
            },
        )
    }

    fn wire_set_mapping_lock(
        &mut self,
        mapping_id: Uuid,
        lock: bool,
    ) -> Result<Result<(), WireError>, String> {
        self.wire_request(
            ControlMessage::SetMappingLock { mapping_id, lock },
            move |message| match message {
                ControlMessage::MappingOpResult {
                    mapping_id: reply_id,
                    result,
                } if reply_id == mapping_id => WireReply::Done(Ok(result)),
                ControlMessage::Error { code, reason } => WireReply::Done(Err(format!(
                    "mapping request rejected: code={code:?} reason={reason}"
                ))),
                other => WireReply::Other(other),
            },
        )
    }

    fn wire_start_take(&mut self) -> Result<Result<Uuid, WireError>, String> {
        self.wire_request(ControlMessage::StartTake, |message| match message {
            ControlMessage::TakeStartResult { result } => WireReply::Done(Ok(result)),
            ControlMessage::Error { code, reason } => WireReply::Done(Err(format!(
                "take request rejected: code={code:?} reason={reason}"
            ))),
            other => WireReply::Other(other),
        })
    }

    fn wire_stop_take(&mut self) -> Result<Result<TakeInfo, WireError>, String> {
        self.wire_request(ControlMessage::StopTake, |message| match message {
            ControlMessage::TakeStopResult { result } => WireReply::Done(Ok(result)),
            ControlMessage::Error { code, reason } => WireReply::Done(Err(format!(
                "take request rejected: code={code:?} reason={reason}"
            ))),
            other => WireReply::Other(other),
        })
    }

    /// Request an on-demand full world snapshot. The direct reply is returned
    /// to the caller only — it is not forwarded to the state-event stream
    /// (unsolicited snapshots, e.g. handshake or lag recovery, are).
    fn wire_get_scene_snapshot(&mut self) -> Result<SceneSnapshotPayload, String> {
        self.wire_request(ControlMessage::GetSceneSnapshot, |message| match message {
            ControlMessage::SceneSnapshot(payload) => WireReply::Done(Ok(payload)),
            ControlMessage::Error { code, reason } => WireReply::Done(Err(format!(
                "snapshot request rejected: code={code:?} reason={reason}"
            ))),
            other => WireReply::Other(other),
        })
    }
}

/// Outcome of inspecting one control message while waiting for a direct reply.
enum WireReply<T> {
    /// The message was the direct reply (or a fatal error) — stop waiting.
    Done(Result<T, String>),
    /// Unrelated message — stash it for its own consumer and keep waiting.
    Other(ControlMessage),
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
    let endpoint = QuicClient::new_insecure_for_local_dev()
        .map_err(|err| format!("failed to create QUIC client endpoint: {err}"))?;
    connect_with_endpoint(
        endpoint,
        device_id,
        device_name,
        qualified_outputs,
        server_addr,
        pairing_token,
        api_key,
        handshake_timeout,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn connect_with_endpoint(
    endpoint: QuicClient,
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
            roles: vec![
                ClientRole::MotionSource,
                ClientRole::Operator,
                ClientRole::VideoSink,
            ],
            features: vec![
                Feature::Motion,
                Feature::Mapping,
                Feature::Recording,
                Feature::Video,
            ],
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
        // Protocol 2.1: version-mismatch / out-of-order handshake failures are
        // typed `Error` messages flushed before the server drops us.
        ControlMessage::Error { code, reason } => {
            return Err(format!(
                "registration rejected: code={code:?} reason={reason}"
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

    // A stable identity across reconnects/launches: the host app persists a UUID and
    // sets MOTIONSTAGE_DEVICE_ID so a returning device resumes its session instead of
    // appearing as a brand-new client. Falls back to a fresh id when unset.
    let device_id = std::env::var("MOTIONSTAGE_DEVICE_ID")
        .ok()
        .and_then(|s| Uuid::parse_str(s.trim()).ok())
        .unwrap_or_else(Uuid::now_v7);
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
        state_event_subscribed: Arc::new(AtomicBool::new(false)),
        dropped_unsubscribed_events: 0,
        pending_mode_states: VecDeque::new(),
        pending_video_offers: VecDeque::new(),
        pending_video_signals: VecDeque::new(),
        pending_video_statuses: VecDeque::new(),
        pending_state_stream: VecDeque::new(),
    })
}

/// Box a fully-initialized client and hand ownership to the caller as an
/// opaque pointer (shared tail of every `motionstage_swift_client_new*`).
fn into_client_ptr(
    inner: MotionStageSwiftClientInner,
    last_send_ns: Arc<AtomicU64>,
) -> *mut c_void {
    // Share the subscription flag with the client so
    // `set_state_event_callback` can flip it and `stash_control_message` can
    // read it without taking the inner lock.
    let state_event_subscribed = Arc::clone(&inner.state_event_subscribed);
    let client = MotionStageSwiftClient {
        inner: Mutex::new(inner),
        last_send_ns,
        ping_shutdown: Arc::new(AtomicBool::new(false)),
        ping_thread: Mutex::new(None),
        connection_state: Arc::new(std::sync::atomic::AtomicI32::new(
            MOTIONSTAGE_SWIFT_CONNECTION_DISCONNECTED,
        )),
        reconnect_policy: Mutex::new(None),
        connect_params: Mutex::new(None),
        event_callback: Mutex::new(None),
        state_event_callback: Mutex::new(None),
        state_event_subscribed,
        state_event_epoch: Arc::new(AtomicU64::new(0)),
        state_event_shutdown: Arc::new(AtomicBool::new(false)),
        state_event_thread: Mutex::new(None),
    };

    Box::into_raw(Box::new(client)).cast::<c_void>()
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

unsafe fn read_required_uuid(input: *const c_char, field: &str) -> Result<Uuid, String> {
    let value = unsafe { read_required_cstr(input, field) }?;
    Uuid::parse_str(value.trim()).map_err(|err| format!("{field} must be a UUID: {err}"))
}

unsafe fn read_optional_uuid(input: *const c_char, field: &str) -> Result<Option<Uuid>, String> {
    match unsafe { read_optional_cstr(input, field) }? {
        Some(value) => Uuid::parse_str(value.trim())
            .map(Some)
            .map_err(|err| format!("{field} must be a UUID: {err}")),
        None => Ok(None),
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
// Connection monitor thread (4.3 + 4.2)
// ---------------------------------------------------------------------------

const PING_INTERVAL: Duration = Duration::from_secs(3);
const PING_IDLE_THRESHOLD_NS: u64 = 2_000_000_000; // 2 seconds

fn start_connection_monitor(
    client_ptr: usize,
    last_send_ns: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("motionstage-monitor".into())
        .spawn(move || {
            'outer: loop {
                // --- Ping loop ---
                loop {
                    std::thread::sleep(PING_INTERVAL);

                    if shutdown.load(Ordering::Relaxed) {
                        break 'outer;
                    }

                    // Only send a ping if no motion datagram was sent recently.
                    let last = last_send_ns.load(Ordering::Relaxed);
                    let now = now_ns();
                    if last > 0 && now.saturating_sub(last) < PING_IDLE_THRESHOLD_NS {
                        continue;
                    }

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
                        false
                    };
                    drop(inner);

                    if !send_ok {
                        // Connection may be broken — attempt reconnection if policy is set.
                        break;
                    }
                }

                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // --- Reconnect loop ---
                let client_ref = unsafe { &*(client_ptr as *const MotionStageSwiftClient) };

                let policy = client_ref
                    .reconnect_policy
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let params = client_ref
                    .connect_params
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();

                let (policy, params) = match (policy, params) {
                    (Some(p), Some(c)) => (p, c),
                    _ => {
                        // No reconnect policy or no saved params — stop monitoring.
                        client_ref
                            .connection_state
                            .store(MOTIONSTAGE_SWIFT_CONNECTION_DISCONNECTED, Ordering::Relaxed);
                        fire_event(client_ref, MOTIONSTAGE_SWIFT_EVENT_DISCONNECTED, 0, None);
                        break;
                    }
                };

                // Disconnect the old session.
                {
                    let mut inner = client_ref
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    inner.disconnect();
                }

                client_ref
                    .connection_state
                    .store(MOTIONSTAGE_SWIFT_CONNECTION_RECONNECTING, Ordering::Relaxed);

                let mut delay = policy.initial_delay;
                let mut reconnected = false;

                for attempt in 1..=policy.max_attempts {
                    if shutdown.load(Ordering::Relaxed) {
                        break 'outer;
                    }

                    fire_event(
                        client_ref,
                        MOTIONSTAGE_SWIFT_EVENT_RECONNECTING,
                        attempt,
                        None,
                    );

                    std::thread::sleep(delay);

                    if shutdown.load(Ordering::Relaxed) {
                        break 'outer;
                    }

                    // Try to reconnect.
                    let inner = client_ref
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let device_id = inner.device_id;
                    let device_name = inner.device_name.clone();
                    let qualified_outputs = inner.qualified_outputs.clone();
                    let handshake_timeout = inner.handshake_timeout;
                    drop(inner);

                    let endpoint = if let Some(fp) = params.fingerprint {
                        QuicClient::new_with_pinned_cert(fp)
                    } else {
                        QuicClient::new_insecure_for_local_dev()
                    };

                    let endpoint = match endpoint {
                        Ok(ep) => ep,
                        Err(_) => {
                            delay = next_delay(delay, policy.backoff_factor, policy.max_delay);
                            continue;
                        }
                    };

                    let result = get_runtime().block_on(connect_with_endpoint(
                        endpoint,
                        device_id,
                        device_name,
                        qualified_outputs,
                        params.server_addr.clone(),
                        params.pairing_token.clone(),
                        params.api_key.clone(),
                        handshake_timeout,
                    ));

                    match result {
                        Ok(session) => {
                            let mut inner = client_ref
                                .inner
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            inner.session = Some(session);
                            inner.clear_error();
                            drop(inner);

                            client_ref
                                .connection_state
                                .store(MOTIONSTAGE_SWIFT_CONNECTION_CONNECTED, Ordering::Relaxed);
                            client_ref.last_send_ns.store(0, Ordering::Relaxed);
                            fire_event(
                                client_ref,
                                MOTIONSTAGE_SWIFT_EVENT_CONNECTED,
                                attempt,
                                None,
                            );
                            reconnected = true;
                            break;
                        }
                        Err(_) => {
                            delay = next_delay(delay, policy.backoff_factor, policy.max_delay);
                        }
                    }
                }

                if !reconnected {
                    client_ref
                        .connection_state
                        .store(MOTIONSTAGE_SWIFT_CONNECTION_FAILED, Ordering::Relaxed);
                    fire_event(
                        client_ref,
                        MOTIONSTAGE_SWIFT_EVENT_RECONNECT_FAILED,
                        policy.max_attempts,
                        Some("max reconnect attempts exhausted"),
                    );
                    break;
                }

                // Reconnected — loop back to ping.
            }
        })
        .expect("failed to spawn connection monitor thread")
}

fn next_delay(current: Duration, factor: f64, max: Duration) -> Duration {
    let next = Duration::from_secs_f64(current.as_secs_f64() * factor);
    next.min(max)
}

fn fire_event(
    client_ref: &MotionStageSwiftClient,
    event: i32,
    attempt: u32,
    message: Option<&str>,
) {
    let guard = client_ref
        .event_callback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((callback, ctx)) = *guard {
        let c_msg = message.and_then(|m| CString::new(m).ok());
        let msg_ptr = c_msg.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        unsafe { callback(event, attempt, msg_ptr, ctx as *mut c_void) };
    }
}

// ---------------------------------------------------------------------------
// State-event JSON encoding + pump thread (operator plane, protocol 2.1)
// ---------------------------------------------------------------------------

/// Re-tag serde's externally-tagged enum JSON (`{"Variant":{...}}` or
/// `"Variant"`) as `{"type":"Variant","data":{...}}` so FFI consumers switch
/// on a stable `type` field.
fn tagged_event_value(event: &StateEvent) -> Result<serde_json::Value, String> {
    let value = serde_json::to_value(event)
        .map_err(|err| format!("failed to serialize state event: {err}"))?;
    Ok(match value {
        serde_json::Value::Object(map) if map.len() == 1 => {
            let (variant, data) = map
                .into_iter()
                .next()
                .expect("single-entry map has an entry");
            serde_json::json!({ "type": variant, "data": data })
        }
        serde_json::Value::String(variant) => serde_json::json!({ "type": variant }),
        other => other,
    })
}

/// Encode one `StateEventMsg` envelope for the state-event callback.
/// Shape: `{"kind":"state_event","seq":u64,"origin_session":"<uuid>"|null,
/// "timestamp_ns":u64,"event":{"type":"<Variant>","data":{...}}}`.
fn state_event_to_json(envelope: &StateEventEnvelope) -> Result<String, String> {
    let event = tagged_event_value(&envelope.event)?;
    serde_json::to_string(&serde_json::json!({
        "kind": "state_event",
        "seq": envelope.seq,
        "origin_session": envelope.origin_session,
        "timestamp_ns": envelope.timestamp_ns,
        "event": event,
    }))
    .map_err(|err| format!("failed to serialize state event envelope: {err}"))
}

/// Encode a scene snapshot for the state-event callback.
/// Shape: `{"kind":"scene_snapshot","snapshot":{...SceneSnapshotPayload...}}`.
fn snapshot_to_json(payload: &SceneSnapshotPayload) -> Result<String, String> {
    serde_json::to_string(&serde_json::json!({
        "kind": "scene_snapshot",
        "snapshot": payload,
    }))
    .map_err(|err| format!("failed to serialize scene snapshot: {err}"))
}

/// Encode an operator-op result for FFI: `{"ok":<T-as-JSON>}` on success
/// (`{"ok":null}` for unit results) or
/// `{"err":{"code":"<RejectCode>","reason":"..."}}` on a typed wire error.
fn wire_result_json<T: Serialize>(result: &Result<T, WireError>) -> Result<String, String> {
    let value = match result {
        Ok(payload) => {
            let payload = serde_json::to_value(payload)
                .map_err(|err| format!("failed to serialize wire result: {err}"))?;
            serde_json::json!({ "ok": payload })
        }
        Err(error) => serde_json::json!({
            "err": { "code": format!("{:?}", error.code), "reason": error.reason }
        }),
    };
    serde_json::to_string(&value).map_err(|err| format!("failed to serialize wire result: {err}"))
}

const STATE_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Background pump: periodically drains the control-stream backlog and
/// delivers queued `StateEventMsg`s / unsolicited `SceneSnapshot`s to the
/// registered state-event callback as JSON. Spawned on first subscription and
/// parked (joined) by `motionstage_swift_client_free`.
fn start_state_event_pump(
    client_ptr: usize,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("motionstage-state-events".into())
        .spawn(move || loop {
            std::thread::sleep(STATE_EVENT_POLL_INTERVAL);
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let client_ref = unsafe { &*(client_ptr as *const MotionStageSwiftClient) };

            // Hold the callback lock for the whole dispatch batch. This is the
            // fix for the unregister/dispatch race: `set_state_event_callback`
            // takes the same lock, so a NULL/swap cannot land while a delivery
            // with the old context is in flight — NULL reliably quiesces
            // delivery before it returns. We also snapshot the epoch and
            // re-check it (and shutdown) before each invocation as defence in
            // depth. NOTE (Swift side): a callback must not reentrantly call
            // `set_state_event_callback` (it would deadlock on this lock).
            let callback_guard = client_ref
                .state_event_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let batch_epoch = client_ref.state_event_epoch.load(Ordering::Acquire);
            let Some((callback, ctx)) = *callback_guard else {
                continue;
            };

            // Drain the single arrival-ordered queue and serialize each item in
            // order: a lag-recovery snapshot is delivered before the events that
            // followed it on the wire (preserving the seq<=snapshot.seq rule).
            let mut messages: Vec<String> = Vec::new();
            {
                let mut inner = client_ref
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if inner.session.is_some() {
                    // Transport errors surface via the connection monitor;
                    // the pump just retries on the next tick.
                    let _ = inner.drain_control_backlog();
                }
                while let Some(item) = inner.pending_state_stream.pop_front() {
                    let json = match &item {
                        StateStreamItem::Event(envelope) => state_event_to_json(envelope),
                        StateStreamItem::Snapshot(payload) => snapshot_to_json(payload),
                    };
                    if let Ok(json) = json {
                        messages.push(json);
                    }
                }
            }

            for json in messages {
                if shutdown.load(Ordering::Relaxed)
                    || client_ref.state_event_epoch.load(Ordering::Acquire) != batch_epoch
                {
                    break;
                }
                if let Ok(c_json) = CString::new(json) {
                    unsafe { callback(c_json.as_ptr(), ctx as *mut c_void) };
                }
            }
            drop(callback_guard);
        })
        .expect("failed to spawn state-event pump thread")
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

    into_client_ptr(inner, last_send_ns)
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
    let inner =
        match make_client_inner(device_name, source_outputs, None, Arc::clone(&last_send_ns)) {
            Ok(inner) => inner,
            Err(_) => return ptr::null_mut(),
        };

    into_client_ptr(inner, last_send_ns)
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
    let inner = match make_client_inner(
        device_name,
        source_outputs,
        config_ref,
        Arc::clone(&last_send_ns),
    ) {
        Ok(inner) => inner,
        Err(_) => return ptr::null_mut(),
    };

    into_client_ptr(inner, last_send_ns)
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
    let inner =
        match make_client_inner(device_name, source_outputs, None, Arc::clone(&last_send_ns)) {
            Ok(inner) => inner,
            Err(_) => return ptr::null_mut(),
        };

    into_client_ptr(inner, last_send_ns)
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
    let inner =
        match make_client_inner(device_name, source_outputs, None, Arc::clone(&last_send_ns)) {
            Ok(inner) => inner,
            Err(_) => return ptr::null_mut(),
        };

    into_client_ptr(inner, last_send_ns)
}

/// Free a client. Must NOT be called from within one of the client's own
/// callbacks (it joins the state-event pump thread before dropping).
#[no_mangle]
pub extern "C" fn motionstage_swift_client_free(client: *mut c_void) {
    if client.is_null() {
        return;
    }

    let client = unsafe { Box::from_raw(client as *mut MotionStageSwiftClient) };
    // Signal ping thread to stop before dropping.
    client.ping_shutdown.store(true, Ordering::Relaxed);
    // Stop the state-event pump and wait for it: it dereferences the client
    // pointer on every tick, so it must be parked before the box drops.
    client.state_event_shutdown.store(true, Ordering::Relaxed);
    let pump = client
        .state_event_thread
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(handle) = pump {
        let _ = handle.join();
    }
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

    // Clone connect params before they are consumed by connect_impl.
    let saved_params = ConnectParams {
        server_addr: server_addr.clone(),
        pairing_token: pairing_token.clone(),
        api_key: api_key.clone(),
        fingerprint: None,
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

            let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };
            // Save connect params for reconnection.
            *client_ref
                .connect_params
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(saved_params);
            client_ref
                .connection_state
                .store(MOTIONSTAGE_SWIFT_CONNECTION_CONNECTED, Ordering::Relaxed);

            let handle = start_connection_monitor(
                client as usize,
                Arc::clone(&client_ref.last_send_ns),
                Arc::clone(&client_ref.ping_shutdown),
            );
            *client_ref
                .ping_thread
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

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
///
/// # Safety
/// `client` must be a valid pointer returned by `motionstage_swift_client_new`
/// and must remain valid until the callback is called; string arguments must
/// be valid NUL-terminated C strings (or NULL where documented).
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
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
            return;
        }
    };
    let pairing_token = match unsafe { read_optional_cstr(pairing_token, "pairing_token") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
            return;
        }
    };
    let api_key = match unsafe { read_optional_cstr(api_key, "api_key") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
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
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED,
                    msg.as_ptr(),
                    context,
                )
            };
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

    // Clone connect params for reconnection before they are moved into the async task.
    let saved_params = ConnectParams {
        server_addr: server_addr.clone(),
        pairing_token: pairing_token.clone(),
        api_key: api_key.clone(),
        fingerprint: None,
    };

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
                let client_ref = unsafe { &*(client_addr as *mut MotionStageSwiftClient) };
                let mut inner = client_ref
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inner.session = Some(session);
                inner.clear_error();
                drop(inner);

                // Save connect params for reconnection.
                *client_ref
                    .connect_params
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(saved_params);
                client_ref
                    .connection_state
                    .store(MOTIONSTAGE_SWIFT_CONNECTION_CONNECTED, Ordering::Relaxed);

                // Start connection monitor thread.
                client_ref.ping_shutdown.store(false, Ordering::Relaxed);
                client_ref.last_send_ns.store(0, Ordering::Relaxed);
                let handle = start_connection_monitor(
                    client_addr,
                    Arc::clone(&client_ref.last_send_ns),
                    Arc::clone(&client_ref.ping_shutdown),
                );
                *client_ref
                    .ping_thread
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

                unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_OK, ptr::null(), ctx) };
            }
            Err(err) => {
                let status = map_connect_error(&err);
                // Lock briefly to store error.
                let client_ref = unsafe { &*(client_addr as *mut MotionStageSwiftClient) };
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

/// Connect using a pinned certificate fingerprint (TOFU).
/// `fingerprint_hex` must be a 64-character hex string (SHA-256 of the server's DER cert).
#[no_mangle]
pub extern "C" fn motionstage_swift_client_connect_pinned(
    client: *mut c_void,
    server_addr: *const c_char,
    pairing_token: *const c_char,
    api_key: *const c_char,
    fingerprint_hex: *const c_char,
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
    let fp_hex = match unsafe { read_required_cstr(fingerprint_hex, "fingerprint_hex") } {
        Ok(value) => value,
        Err(_) => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };
    let fingerprint = match hex_decode_fingerprint(&fp_hex) {
        Some(fp) => fp,
        None => return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
    };

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

    let endpoint = match QuicClient::new_with_pinned_cert(fingerprint) {
        Ok(ep) => ep,
        Err(err) => {
            let mut inner = match lock_client(client) {
                Ok(c) => c,
                Err(status) => return status,
            };
            return inner.fail(
                MOTIONSTAGE_SWIFT_STATUS_TRANSPORT,
                format!("failed to create pinned QUIC client: {err}"),
            );
        }
    };

    // Clone connect params before they are consumed by connect_with_endpoint.
    let saved_params = ConnectParams {
        server_addr: server_addr.clone(),
        pairing_token: pairing_token.clone(),
        api_key: api_key.clone(),
        fingerprint: Some(fingerprint),
    };

    let result = get_runtime().block_on(connect_with_endpoint(
        endpoint,
        device_id,
        device_name,
        qualified_outputs,
        server_addr,
        pairing_token,
        api_key,
        handshake_timeout,
    ));

    let mut inner = match lock_client(client) {
        Ok(c) => c,
        Err(status) => return status,
    };

    match result {
        Ok(session) => {
            inner.session = Some(session);
            inner.clear_error();
            drop(inner);

            let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };

            // Save connect params for reconnection.
            *client_ref
                .connect_params
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(saved_params);
            client_ref
                .connection_state
                .store(MOTIONSTAGE_SWIFT_CONNECTION_CONNECTED, Ordering::Relaxed);

            client_ref.ping_shutdown.store(false, Ordering::Relaxed);
            client_ref.last_send_ns.store(0, Ordering::Relaxed);
            let handle = start_connection_monitor(
                client as usize,
                Arc::clone(&client_ref.last_send_ns),
                Arc::clone(&client_ref.ping_shutdown),
            );
            *client_ref
                .ping_thread
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) => {
            let status = map_connect_error(&err);
            inner.fail(status, err)
        }
    }
}

/// Async connect with certificate pinning (TOFU / 3.1).
/// `fingerprint_hex` must be a 64-character hex string (SHA-256 of the server's DER cert).
/// Calls `callback(status, error_cstr, context)` on completion from a Tokio worker thread.
///
/// # Safety
/// `client` must be a valid pointer returned by `motionstage_swift_client_new`
/// and must remain valid until the callback is called; string arguments must
/// be valid NUL-terminated C strings (or NULL where documented).
#[no_mangle]
pub unsafe extern "C" fn motionstage_swift_client_connect_async_pinned(
    client: *mut c_void,
    server_addr: *const c_char,
    pairing_token: *const c_char,
    api_key: *const c_char,
    fingerprint_hex: *const c_char,
    callback: MotionStageConnectCallback,
    context: *mut c_void,
) {
    let server_addr = match unsafe { read_required_cstr(server_addr, "server_addr") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
            return;
        }
    };
    let pairing_token = match unsafe { read_optional_cstr(pairing_token, "pairing_token") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
            return;
        }
    };
    let api_key = match unsafe { read_optional_cstr(api_key, "api_key") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
            return;
        }
    };
    let fp_hex = match unsafe { read_required_cstr(fingerprint_hex, "fingerprint_hex") } {
        Ok(value) => value,
        Err(err) => {
            let c = CString::new(err).unwrap_or_default();
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
            return;
        }
    };
    let fingerprint = match hex_decode_fingerprint(&fp_hex) {
        Some(fp) => fp,
        None => {
            let c = CString::new("fingerprint_hex must be a 64-character hex string")
                .unwrap_or_default();
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
                    c.as_ptr(),
                    context,
                )
            };
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
            unsafe {
                callback(
                    MOTIONSTAGE_SWIFT_STATUS_ALREADY_CONNECTED,
                    msg.as_ptr(),
                    context,
                )
            };
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

    // Clone connect params for reconnection.
    let saved_params = ConnectParams {
        server_addr: server_addr.clone(),
        pairing_token: pairing_token.clone(),
        api_key: api_key.clone(),
        fingerprint: Some(fingerprint),
    };

    get_runtime().spawn(async move {
        // Create pinned QUIC endpoint inside the async task.
        let endpoint = match QuicClient::new_with_pinned_cert(fingerprint) {
            Ok(ep) => ep,
            Err(err) => {
                let ctx = context_addr as *mut c_void;
                let c_err = CString::new(format!("failed to create pinned QUIC client: {err}"))
                    .unwrap_or_default();
                unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, c_err.as_ptr(), ctx) };
                return;
            }
        };

        let result = connect_with_endpoint(
            endpoint,
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
                let client_ref = unsafe { &*(client_addr as *mut MotionStageSwiftClient) };
                let mut inner = client_ref
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inner.session = Some(session);
                inner.clear_error();
                drop(inner);

                *client_ref
                    .connect_params
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(saved_params);
                client_ref
                    .connection_state
                    .store(MOTIONSTAGE_SWIFT_CONNECTION_CONNECTED, Ordering::Relaxed);

                client_ref.ping_shutdown.store(false, Ordering::Relaxed);
                client_ref.last_send_ns.store(0, Ordering::Relaxed);
                let handle = start_connection_monitor(
                    client_addr,
                    Arc::clone(&client_ref.last_send_ns),
                    Arc::clone(&client_ref.ping_shutdown),
                );
                *client_ref
                    .ping_thread
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

                unsafe { callback(MOTIONSTAGE_SWIFT_STATUS_OK, ptr::null(), ctx) };
            }
            Err(err) => {
                let status = map_connect_error(&err);
                let client_ref = unsafe { &*(client_addr as *mut MotionStageSwiftClient) };
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

    // Signal connection monitor thread to stop.
    let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };
    client_ref.ping_shutdown.store(true, Ordering::Relaxed);
    client_ref
        .connection_state
        .store(MOTIONSTAGE_SWIFT_CONNECTION_DISCONNECTED, Ordering::Relaxed);
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

/// Send a batch of attribute updates as one motion datagram.
///
/// # Safety
/// `client` must be a valid pointer returned by `motionstage_swift_client_new`;
/// `updates` must point to `count` valid `MotionAttributeUpdateC` values whose
/// attribute strings are valid NUL-terminated C strings.
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
                unsafe {
                    *active_mode_out = MOTIONSTAGE_SWIFT_MODE_LIVE;
                }
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
                unsafe {
                    *active_mode_out = MOTIONSTAGE_SWIFT_MODE_LIVE;
                }
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
            unsafe {
                *active_mode_out = mode_to_legacy_i32(&mode);
            }
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) => classify_mode_error(&mut client, err),
    }
}

// ---------------------------------------------------------------------------
// FFI: Video signaling
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn motionstage_swift_client_video_get_status(
    client: *mut c_void,
    out_available: *mut i32,
    out_descriptor_set: *mut i32,
    out_peer_count: *mut u32,
    out_last_frame_age_ms: *mut i64,
) -> i32 {
    if out_available.is_null()
        || out_descriptor_set.is_null()
        || out_peer_count.is_null()
        || out_last_frame_age_ms.is_null()
    {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.get_video_stream_status() {
        Ok(status) => {
            unsafe {
                *out_available = i32::from(status.available);
                *out_descriptor_set = i32::from(status.descriptor_set);
                *out_peer_count = status.peer_count;
                *out_last_frame_age_ms = status
                    .last_frame_age_ms
                    .map(|v| i64::try_from(v).unwrap_or(i64::MAX))
                    .unwrap_or(-1);
            }
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("invalid") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_video_create_offer(
    client: *mut c_void,
    stream_id: *const c_char,
    track_id: *const c_char,
    out_sdp_type: *mut i32,
    out_sdp: *mut *mut c_char,
) -> i32 {
    if out_sdp_type.is_null() || out_sdp.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let stream_id = match unsafe { read_required_cstr(stream_id, "stream_id") } {
        Ok(value) => value,
        Err(message) => {
            let mut client = match lock_client(client) {
                Ok(client) => client,
                Err(status) => return status,
            };
            return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message);
        }
    };
    let track_id = match unsafe { read_required_cstr(track_id, "track_id") } {
        Ok(value) => value,
        Err(message) => {
            let mut client = match lock_client(client) {
                Ok(client) => client,
                Err(status) => return status,
            };
            return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message);
        }
    };

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.create_video_offer(&stream_id, &track_id) {
        Ok(offer) => {
            unsafe {
                *out_sdp_type = MotionStageSwiftClientInner::sdp_type_to_i32(offer.ty);
                *out_sdp = into_c_string_ptr(&offer.sdp);
            }
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("invalid") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_video_send_sdp(
    client: *mut c_void,
    sdp_type: i32,
    sdp: *const c_char,
) -> i32 {
    let sdp = match unsafe { read_required_cstr(sdp, "sdp") } {
        Ok(value) => value,
        Err(message) => {
            let mut client = match lock_client(client) {
                Ok(client) => client,
                Err(status) => return status,
            };
            return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message);
        }
    };

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.send_video_sdp(sdp_type, &sdp) {
        Ok(()) => {
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("invalid") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_video_send_ice(
    client: *mut c_void,
    candidate: *const c_char,
    sdp_mid: *const c_char,
    sdp_mline_index: i32,
) -> i32 {
    let candidate = match unsafe { read_required_cstr(candidate, "candidate") } {
        Ok(value) => value,
        Err(message) => {
            let mut client = match lock_client(client) {
                Ok(client) => client,
                Err(status) => return status,
            };
            return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message);
        }
    };
    let sdp_mid = match unsafe { read_optional_cstr(sdp_mid, "sdp_mid") } {
        Ok(value) => value,
        Err(message) => {
            let mut client = match lock_client(client) {
                Ok(client) => client,
                Err(status) => return status,
            };
            return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message);
        }
    };
    if sdp_mline_index >= 0 && sdp_mline_index > u16::MAX as i32 {
        let mut client = match lock_client(client) {
            Ok(client) => client,
            Err(status) => return status,
        };
        return client.fail(
            MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT,
            "sdp_mline_index must fit u16",
        );
    }
    let sdp_mline_index = if sdp_mline_index < 0 {
        None
    } else {
        Some(sdp_mline_index as u16)
    };

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };

    match client.send_video_ice(&candidate, sdp_mid, sdp_mline_index) {
        Ok(()) => {
            client.clear_error();
            MOTIONSTAGE_SWIFT_STATUS_OK
        }
        Err(err) if err.contains("not connected") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
        }
        Err(err) if err.contains("invalid") => {
            client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
        }
        Err(err) if err.contains("rejected") => client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err),
        Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err),
    }
}

#[no_mangle]
pub extern "C" fn motionstage_swift_client_video_next_signal_json(
    client: *mut c_void,
) -> *mut c_char {
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(_) => return ptr::null_mut(),
    };

    match client.next_video_signal_json() {
        Ok(Some(json)) => {
            client.clear_error();
            into_c_string_ptr(&json)
        }
        Ok(None) => {
            client.clear_error();
            ptr::null_mut()
        }
        Err(err) => {
            client.last_error = Some(err);
            ptr::null_mut()
        }
    }
}

/// Classify an error string from the mode helpers into an FFI status code.
fn classify_mode_error(client: &mut MotionStageSwiftClientInner, err: String) -> i32 {
    if err.contains("invalid mode value")
        || err.contains("invalid data-flow")
        || err.contains("invalid recording")
    {
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
// FFI: Operator plane (protocol 2.1)
// ---------------------------------------------------------------------------

/// Classify a transport-level error string from the operator-plane helpers
/// into an FFI status code (typed wire errors travel inside the result JSON
/// instead and report MOTIONSTAGE_SWIFT_STATUS_OK).
fn classify_wire_transport_error(client: &mut MotionStageSwiftClientInner, err: String) -> i32 {
    if err.contains("must be a UUID") || err.contains("must not be") || err.contains("invalid") {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, err)
    } else if err.contains("not connected") {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_NOT_CONNECTED, err)
    } else if err.contains("rejected") {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_PROTOCOL, err)
    } else {
        client.fail(MOTIONSTAGE_SWIFT_STATUS_TRANSPORT, err)
    }
}

/// Shared tail of the operator-plane FFI ops: serialize the typed wire result
/// into `*out_result_json` (owned; free with `motionstage_swift_string_free`)
/// or classify the transport error.
fn finish_wire_op<T: Serialize>(
    client: &mut MotionStageSwiftClientInner,
    result: Result<Result<T, WireError>, String>,
    out_result_json: *mut *mut c_char,
) -> i32 {
    match result {
        Ok(wire_result) => match wire_result_json(&wire_result) {
            Ok(json) => {
                unsafe {
                    *out_result_json = into_c_string_ptr(&json);
                }
                client.clear_error();
                MOTIONSTAGE_SWIFT_STATUS_OK
            }
            Err(err) => client.fail(MOTIONSTAGE_SWIFT_STATUS_INTERNAL, err),
        },
        Err(err) => classify_wire_transport_error(client, err),
    }
}

/// Convert an optional C mask array to the wire component mask.
fn read_component_mask(mask: *const u32, mask_len: u32) -> Option<Vec<usize>> {
    if mask.is_null() || mask_len == 0 {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(mask, mask_len as usize) };
    Some(slice.iter().map(|&index| index as usize).collect())
}

/// Register a callback for the server->client state-event stream
/// (StateEventMsg envelopes plus unsolicited SceneSnapshots), delivered as
/// JSON (see `MotionStageStateEventCallback` for the schema). The first
/// registration spawns a background pump thread that lives until
/// `motionstage_swift_client_free`.
///
/// Set callback to NULL to unsubscribe. NULL reliably quiesces delivery: this
/// call blocks until any in-flight dispatch batch finishes, so on return the
/// old context pointer is guaranteed no longer in use and is safe to free.
/// While unsubscribed the SDK **drops** incoming state-stream messages instead
/// of queueing them (an unsubscribed client must not accumulate an unbounded
/// backlog); a later re-subscription recovers full state via the normal
/// lag→SceneSnapshot resync path. A callback must not reentrantly call this
/// function.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_set_state_event_callback(
    client: *mut c_void,
    callback: Option<MotionStageStateEventCallback>,
    context: *mut c_void,
) -> i32 {
    if client.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };

    // Bump the epoch first so any in-flight pump batch that re-checks it stops
    // calling the old context. Then swap the callback under the lock the pump
    // holds across its dispatch batch: this call blocks until the batch
    // finishes, so on return no delivery with the previous context is in
    // flight (NULL reliably quiesces delivery).
    client_ref
        .state_event_epoch
        .fetch_add(1, Ordering::AcqRel);
    {
        let mut guard = client_ref
            .state_event_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = callback.map(|cb| (cb, context as usize));
        // Toggle the shared subscription flag while holding the callback lock
        // so the flag and the stored callback change together.
        client_ref
            .state_event_subscribed
            .store(callback.is_some(), Ordering::Release);
    }

    if callback.is_some() {
        let mut thread_guard = client_ref
            .state_event_thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if thread_guard.is_none() {
            *thread_guard = Some(start_state_event_pump(
                client as usize,
                Arc::clone(&client_ref.state_event_shutdown),
            ));
        }
    }

    MOTIONSTAGE_SWIFT_STATUS_OK
}

/// Create a mapping (operator plane).
/// `source_device`: UUID string or NULL = this session's own device.
/// `target_scene`: UUID string or NULL = the active scene.
/// `component_mask`: NULL = all components; otherwise `component_mask_len`
/// component indices.
/// On MOTIONSTAGE_SWIFT_STATUS_OK, `*out_result_json` receives an owned JSON
/// string — `{"ok":{MappingSummary}}` or `{"err":{"code":"<RejectCode>","reason":"..."}}`
/// — free it with `motionstage_swift_string_free`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_create_mapping(
    client: *mut c_void,
    source_device: *const c_char,
    source_output: *const c_char,
    target_scene: *const c_char,
    target_object: *const c_char,
    target_attribute: *const c_char,
    component_mask: *const u32,
    component_mask_len: u32,
    out_result_json: *mut *mut c_char,
) -> i32 {
    if out_result_json.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let parsed = (|| {
        Ok::<_, String>((
            unsafe { read_optional_uuid(source_device, "source_device") }?,
            unsafe { read_required_cstr(source_output, "source_output") }?,
            unsafe { read_optional_uuid(target_scene, "target_scene") }?,
            unsafe { read_required_uuid(target_object, "target_object") }?,
            unsafe { read_required_cstr(target_attribute, "target_attribute") }?,
        ))
    })();
    let mask = read_component_mask(component_mask, component_mask_len);

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };
    let (source_device, source_output, target_scene, target_object, target_attribute) = match parsed
    {
        Ok(values) => values,
        Err(message) => return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message),
    };

    let result = client.wire_create_mapping(
        source_device,
        source_output,
        target_scene,
        target_object,
        target_attribute,
        mask,
    );
    finish_wire_op(&mut client, result, out_result_json)
}

/// Replace a mapping's full definition (operator plane). Argument semantics
/// and result JSON are identical to `motionstage_swift_client_create_mapping`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn motionstage_swift_client_update_mapping(
    client: *mut c_void,
    mapping_id: *const c_char,
    source_device: *const c_char,
    source_output: *const c_char,
    target_scene: *const c_char,
    target_object: *const c_char,
    target_attribute: *const c_char,
    component_mask: *const u32,
    component_mask_len: u32,
    out_result_json: *mut *mut c_char,
) -> i32 {
    if out_result_json.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }

    let parsed = (|| {
        Ok::<_, String>((
            unsafe { read_required_uuid(mapping_id, "mapping_id") }?,
            unsafe { read_optional_uuid(source_device, "source_device") }?,
            unsafe { read_required_cstr(source_output, "source_output") }?,
            unsafe { read_optional_uuid(target_scene, "target_scene") }?,
            unsafe { read_required_uuid(target_object, "target_object") }?,
            unsafe { read_required_cstr(target_attribute, "target_attribute") }?,
        ))
    })();
    let mask = read_component_mask(component_mask, component_mask_len);

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };
    let (mapping_id, source_device, source_output, target_scene, target_object, target_attribute) =
        match parsed {
            Ok(values) => values,
            Err(message) => return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message),
        };

    let result = client.wire_update_mapping(
        mapping_id,
        source_device,
        source_output,
        target_scene,
        target_object,
        target_attribute,
        mask,
    );
    finish_wire_op(&mut client, result, out_result_json)
}

/// Remove a mapping (operator plane).
/// On MOTIONSTAGE_SWIFT_STATUS_OK, `*out_result_json` receives `{"ok":null}`
/// or `{"err":{...}}`; free with `motionstage_swift_string_free`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_remove_mapping(
    client: *mut c_void,
    mapping_id: *const c_char,
    out_result_json: *mut *mut c_char,
) -> i32 {
    if out_result_json.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let parsed = unsafe { read_required_uuid(mapping_id, "mapping_id") };

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };
    let mapping_id = match parsed {
        Ok(value) => value,
        Err(message) => return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message),
    };

    let result = client.wire_remove_mapping(mapping_id);
    finish_wire_op(&mut client, result, out_result_json)
}

/// Lock or unlock a mapping (operator plane). `lock`: 0 = unlock, else lock.
/// Result JSON as for `motionstage_swift_client_remove_mapping`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_set_mapping_lock(
    client: *mut c_void,
    mapping_id: *const c_char,
    lock: i32,
    out_result_json: *mut *mut c_char,
) -> i32 {
    if out_result_json.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let parsed = unsafe { read_required_uuid(mapping_id, "mapping_id") };

    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };
    let mapping_id = match parsed {
        Ok(value) => value,
        Err(message) => return client.fail(MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT, message),
    };

    let result = client.wire_set_mapping_lock(mapping_id, lock != 0);
    finish_wire_op(&mut client, result, out_result_json)
}

/// Start recording a take (Operator role required; the server assigns the
/// take id and path). On MOTIONSTAGE_SWIFT_STATUS_OK, `*out_result_json`
/// receives `{"ok":"<take-uuid>"}` or `{"err":{...}}`; free with
/// `motionstage_swift_string_free`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_start_take(
    client: *mut c_void,
    out_result_json: *mut *mut c_char,
) -> i32 {
    if out_result_json.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };
    let result = client.wire_start_take();
    finish_wire_op(&mut client, result, out_result_json)
}

/// Stop the active recording and register the take (Operator role required).
/// On MOTIONSTAGE_SWIFT_STATUS_OK, `*out_result_json` receives
/// `{"ok":{TakeInfo}}` or `{"err":{...}}`; free with
/// `motionstage_swift_string_free`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_stop_take(
    client: *mut c_void,
    out_result_json: *mut *mut c_char,
) -> i32 {
    if out_result_json.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };
    let result = client.wire_stop_take();
    finish_wire_op(&mut client, result, out_result_json)
}

/// Request an on-demand full world snapshot. On MOTIONSTAGE_SWIFT_STATUS_OK,
/// `*out_snapshot_json` receives the SceneSnapshotPayload as JSON (no
/// `{"ok":...}` envelope — the request has no typed wire error); free with
/// `motionstage_swift_string_free`.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_get_scene_snapshot(
    client: *mut c_void,
    out_snapshot_json: *mut *mut c_char,
) -> i32 {
    if out_snapshot_json.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let mut client = match lock_client(client) {
        Ok(client) => client,
        Err(status) => return status,
    };
    match client.wire_get_scene_snapshot() {
        Ok(payload) => match serde_json::to_string(&payload) {
            Ok(json) => {
                unsafe {
                    *out_snapshot_json = into_c_string_ptr(&json);
                }
                client.clear_error();
                MOTIONSTAGE_SWIFT_STATUS_OK
            }
            Err(err) => client.fail(
                MOTIONSTAGE_SWIFT_STATUS_INTERNAL,
                format!("failed to serialize scene snapshot: {err}"),
            ),
        },
        Err(err) => classify_wire_transport_error(&mut client, err),
    }
}

// ---------------------------------------------------------------------------
// FFI: Reconnection (4.2)
// ---------------------------------------------------------------------------

/// Configure the reconnection policy.
/// backoff_factor_x100: backoff multiplier * 100 (e.g. 200 = 2.0x).
/// Set max_attempts=0 to disable auto-reconnect.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_set_reconnect_policy(
    client: *mut c_void,
    max_attempts: u32,
    initial_delay_ms: u32,
    max_delay_ms: u32,
    backoff_factor_x100: u32,
) -> i32 {
    if client.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };

    let policy = if max_attempts == 0 {
        None
    } else {
        Some(ReconnectPolicy {
            max_attempts,
            initial_delay: Duration::from_millis(initial_delay_ms as u64),
            max_delay: Duration::from_millis(max_delay_ms as u64),
            backoff_factor: backoff_factor_x100 as f64 / 100.0,
        })
    };

    *client_ref
        .reconnect_policy
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;

    MOTIONSTAGE_SWIFT_STATUS_OK
}

/// Register a callback for connection state change events.
/// Set callback to NULL to clear.
#[no_mangle]
pub extern "C" fn motionstage_swift_client_set_connection_event_callback(
    client: *mut c_void,
    callback: Option<MotionStageConnectionEventCallback>,
    context: *mut c_void,
) -> i32 {
    if client.is_null() {
        return MOTIONSTAGE_SWIFT_STATUS_INVALID_ARGUMENT;
    }
    let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };

    let mut guard = client_ref
        .event_callback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    *guard = callback.map(|cb| (cb, context as usize));

    MOTIONSTAGE_SWIFT_STATUS_OK
}

/// Returns the current connection state (MOTIONSTAGE_SWIFT_CONNECTION_* constant).
#[no_mangle]
pub extern "C" fn motionstage_swift_client_connection_state(client: *mut c_void) -> i32 {
    if client.is_null() {
        return MOTIONSTAGE_SWIFT_CONNECTION_DISCONNECTED;
    }
    let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };
    client_ref.connection_state.load(Ordering::Relaxed)
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

    fn test_server_config() -> ServerConfig {
        ServerConfig {
            quic_bind_addr: "127.0.0.1:0".parse().unwrap(),
            enable_discovery: false,
            ..ServerConfig::default()
        }
    }

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
        let server = ServerHandle::new(test_server_config());
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
        let server = ServerHandle::new(test_server_config());
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

        let client = motionstage_swift_client_new_v2(device_name.as_ptr(), 2, names.as_ptr());
        assert!(!client.is_null());
        motionstage_swift_client_free(client);
    }

    #[test]
    fn wire_result_json_encodes_ok_err_and_unit() {
        let summary = MappingSummary {
            mapping_id: Uuid::nil(),
            source_device: Uuid::nil(),
            source_output: "camera.position".into(),
            target_scene: Uuid::nil(),
            target_object: Uuid::nil(),
            target_attribute: "position".into(),
            component_mask: Some(vec![0, 2]),
            lock: false,
        };
        let ok: Result<MappingSummary, WireError> = Ok(summary);
        let json: serde_json::Value =
            serde_json::from_str(&wire_result_json(&ok).unwrap()).unwrap();
        assert_eq!(json["ok"]["source_output"], "camera.position");
        assert_eq!(json["ok"]["component_mask"][1], 2);
        assert!(json.get("err").is_none());

        let err: Result<(), WireError> = Err(WireError {
            code: motionstage_protocol::RejectCode::RoleDenied,
            reason: "operator role is required".into(),
        });
        let json: serde_json::Value =
            serde_json::from_str(&wire_result_json(&err).unwrap()).unwrap();
        assert_eq!(json["err"]["code"], "RoleDenied");
        assert_eq!(json["err"]["reason"], "operator role is required");

        let unit: Result<(), WireError> = Ok(());
        let json: serde_json::Value =
            serde_json::from_str(&wire_result_json(&unit).unwrap()).unwrap();
        assert!(json["ok"].is_null());
        assert!(json.get("err").is_none());
    }

    #[test]
    fn state_event_json_uses_stable_type_data_tagging() {
        let session = Uuid::now_v7();
        let envelope = StateEventEnvelope {
            seq: 7,
            origin_session: Some(session),
            timestamp_ns: 123,
            event: StateEvent::ModeChanged { mode: Mode::LIVE },
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_event_to_json(&envelope).unwrap()).unwrap();
        assert_eq!(json["kind"], "state_event");
        assert_eq!(json["seq"], 7);
        assert_eq!(json["origin_session"], session.to_string().as_str());
        assert_eq!(json["timestamp_ns"], 123);
        assert_eq!(json["event"]["type"], "ModeChanged");
        assert_eq!(json["event"]["data"]["mode"]["data_flow"], "Live");
        assert_eq!(json["event"]["data"]["mode"]["recording"], "Inactive");

        let envelope = StateEventEnvelope {
            seq: 8,
            origin_session: None,
            timestamp_ns: 124,
            event: StateEvent::MappingRemoved {
                mapping_id: Uuid::nil(),
            },
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_event_to_json(&envelope).unwrap()).unwrap();
        assert!(json["origin_session"].is_null());
        assert_eq!(json["event"]["type"], "MappingRemoved");
        assert_eq!(
            json["event"]["data"]["mapping_id"],
            Uuid::nil().to_string().as_str()
        );
    }

    /// Collects state-event JSON documents into a `Mutex<Vec<String>>` passed
    /// as the context pointer.
    unsafe extern "C" fn collect_state_events(json: *const c_char, ctx: *mut c_void) {
        let store = unsafe { &*(ctx as *const Mutex<Vec<String>>) };
        let value = unsafe { CStr::from_ptr(json) }
            .to_string_lossy()
            .into_owned();
        store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(value);
    }

    fn wire_op_json(status: i32, out_json: *mut c_char, client: *mut c_void) -> serde_json::Value {
        assert_eq!(
            status,
            MOTIONSTAGE_SWIFT_STATUS_OK,
            "wire op failed: {}",
            ptr_to_string_and_free(motionstage_swift_client_last_error(client))
                .unwrap_or_else(|| "<no error>".to_owned())
        );
        let json = ptr_to_string_and_free(out_json).expect("wire op must produce JSON");
        serde_json::from_str(&json).expect("wire op JSON parses")
    }

    #[test]
    fn ffi_operator_ops_and_state_events_round_trip() {
        use motionstage_core::{AttributeValue, Scene, SceneAttribute, SceneObject};

        let rt = Runtime::new().expect("runtime builds");
        let take_dir =
            std::env::temp_dir().join(format!("motionstage-swift-test-{}", Uuid::now_v7()));
        let mut config = test_server_config();
        config.take_catalog_path = take_dir.join("takes_catalog.json");

        let server = ServerHandle::new(config);
        rt.block_on(server.start()).expect("server starts");

        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        // First loaded scene becomes the active scene.
        rt.block_on(server.load_scene(Scene::new("shot").with_object(object)));

        let server_addr = rt.block_on(server.quic_bind_addr()).to_string();

        let device_name = CString::new("operator-ipad").unwrap();
        let attr = CString::new("camera.position").unwrap();
        let client = motionstage_swift_client_new(device_name.as_ptr(), attr.as_ptr());
        assert!(!client.is_null());

        let events: Box<Mutex<Vec<String>>> = Box::new(Mutex::new(Vec::new()));
        let events_ctx = &*events as *const Mutex<Vec<String>> as *mut c_void;
        assert_eq!(
            motionstage_swift_client_set_state_event_callback(
                client,
                Some(collect_state_events),
                events_ctx,
            ),
            MOTIONSTAGE_SWIFT_STATUS_OK
        );

        let server_addr_c = CString::new(server_addr).unwrap();
        assert_eq!(
            motionstage_swift_client_connect(
                client,
                server_addr_c.as_ptr(),
                ptr::null(),
                ptr::null()
            ),
            MOTIONSTAGE_SWIFT_STATUS_OK
        );
        let device_id = ptr_to_string_and_free(motionstage_swift_client_device_id(client))
            .expect("device id available");

        let source_output = CString::new("camera.position").unwrap();
        let target_object = CString::new(object_id.to_string()).unwrap();
        let target_attribute = CString::new("position").unwrap();

        // Create with defaults: NULL source_device = own device, NULL
        // target_scene = active scene.
        let mut out_json: *mut c_char = ptr::null_mut();
        let status = motionstage_swift_client_create_mapping(
            client,
            ptr::null(),
            source_output.as_ptr(),
            ptr::null(),
            target_object.as_ptr(),
            target_attribute.as_ptr(),
            ptr::null(),
            0,
            &mut out_json,
        );
        let created = wire_op_json(status, out_json, client);
        assert_eq!(
            created["ok"]["source_device"],
            device_id.as_str(),
            "own-device default must resolve to the caller's device"
        );
        assert!(created["ok"]["component_mask"].is_null());
        let mapping_id = created["ok"]["mapping_id"]
            .as_str()
            .expect("mapping id present")
            .to_owned();
        let mapping_id_c = CString::new(mapping_id.clone()).unwrap();

        // Full-replacement update adding a component mask.
        let mask = [0u32, 2u32];
        let mut out_json: *mut c_char = ptr::null_mut();
        let status = motionstage_swift_client_update_mapping(
            client,
            mapping_id_c.as_ptr(),
            ptr::null(),
            source_output.as_ptr(),
            ptr::null(),
            target_object.as_ptr(),
            target_attribute.as_ptr(),
            mask.as_ptr(),
            mask.len() as u32,
            &mut out_json,
        );
        let updated = wire_op_json(status, out_json, client);
        assert_eq!(updated["ok"]["mapping_id"], mapping_id.as_str());
        assert_eq!(updated["ok"]["component_mask"][0], 0);
        assert_eq!(updated["ok"]["component_mask"][1], 2);

        // Lock, then unlock.
        for lock in [1, 0] {
            let mut out_json: *mut c_char = ptr::null_mut();
            let status = motionstage_swift_client_set_mapping_lock(
                client,
                mapping_id_c.as_ptr(),
                lock,
                &mut out_json,
            );
            let result = wire_op_json(status, out_json, client);
            assert!(result["ok"].is_null());
            assert!(result.get("err").is_none());
        }

        // Take control (Operator role, server-assigned identity).
        let mut out_json: *mut c_char = ptr::null_mut();
        let status = motionstage_swift_client_start_take(client, &mut out_json);
        let started = wire_op_json(status, out_json, client);
        let take_id = started["ok"].as_str().expect("take id present").to_owned();
        Uuid::parse_str(&take_id).expect("take id is a UUID");

        let mut out_json: *mut c_char = ptr::null_mut();
        let status = motionstage_swift_client_stop_take(client, &mut out_json);
        let stopped = wire_op_json(status, out_json, client);
        assert_eq!(stopped["ok"]["take_id"], take_id.as_str());
        assert!(stopped["ok"]["name"].as_str().is_some());
        assert!(stopped["ok"]["frame_count"].is_u64());

        // Remove; a second removal reports the typed mapping-not-found error.
        let mut out_json: *mut c_char = ptr::null_mut();
        let status =
            motionstage_swift_client_remove_mapping(client, mapping_id_c.as_ptr(), &mut out_json);
        let removed = wire_op_json(status, out_json, client);
        assert!(removed["ok"].is_null());

        let mut out_json: *mut c_char = ptr::null_mut();
        let status =
            motionstage_swift_client_remove_mapping(client, mapping_id_c.as_ptr(), &mut out_json);
        let missing = wire_op_json(status, out_json, client);
        assert!(missing.get("ok").is_none());
        assert_eq!(missing["err"]["code"], "ServerBusy");
        assert!(missing["err"]["reason"]
            .as_str()
            .unwrap()
            .contains("mapping not found"));

        // On-demand snapshot.
        let mut out_json: *mut c_char = ptr::null_mut();
        let status = motionstage_swift_client_get_scene_snapshot(client, &mut out_json);
        assert_eq!(status, MOTIONSTAGE_SWIFT_STATUS_OK);
        let snapshot: serde_json::Value =
            serde_json::from_str(&ptr_to_string_and_free(out_json).unwrap()).unwrap();
        assert_eq!(snapshot["scenes"][0]["name"], "shot");
        assert!(snapshot["seq"].is_u64());
        assert_eq!(snapshot["mappings"].as_array().map(Vec::len), Some(0));

        // The pump must have delivered the handshake snapshot plus our own
        // mutation echoes (no echo suppression) as JSON.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let expected = [
            "\"kind\":\"scene_snapshot\"",
            "MappingCreated",
            "MappingUpdated",
            "MappingLockChanged",
            "MappingRemoved",
            "RecordingStarted",
            "TakeRegistered",
        ];
        loop {
            let collected = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let all_seen = expected
                .iter()
                .all(|needle| collected.iter().any(|msg| msg.contains(needle)));
            if all_seen {
                for msg in &collected {
                    let value: serde_json::Value =
                        serde_json::from_str(msg).expect("stream message is valid JSON");
                    let kind = value["kind"].as_str().unwrap();
                    assert!(kind == "state_event" || kind == "scene_snapshot");
                    if kind == "state_event" {
                        assert!(value["event"]["type"].is_string());
                    }
                }
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for state events; got: {collected:#?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        assert_eq!(
            motionstage_swift_client_disconnect(client),
            MOTIONSTAGE_SWIFT_STATUS_OK
        );
        motionstage_swift_client_free(client);
        rt.block_on(server.stop()).expect("server stops");
        let _ = std::fs::remove_dir_all(take_dir);
    }

    fn test_inner() -> MotionStageSwiftClientInner {
        make_client_inner(
            "queue-test".to_owned(),
            vec![AttributeDescriptor {
                path: "camera.position".to_owned(),
                value_type: AttributeKind::Vec3f,
            }],
            None,
            Arc::new(AtomicU64::new(0)),
        )
        .expect("inner builds")
    }

    fn sample_snapshot(seq: u64) -> SceneSnapshotPayload {
        SceneSnapshotPayload {
            scenes: Vec::new(),
            mappings: Vec::new(),
            mode: Mode::IDLE,
            active_scene: None,
            sessions: Vec::new(),
            takes: Vec::new(),
            playback: None,
            seq,
        }
    }

    fn sample_event(seq: u64) -> StateEventEnvelope {
        StateEventEnvelope {
            seq,
            origin_session: None,
            timestamp_ns: seq,
            event: StateEvent::ModeChanged { mode: Mode::LIVE },
        }
    }

    /// Finding: the state-event pump must not split events and snapshots into
    /// separate queues and reorder them. A single arrival-ordered queue must
    /// deliver a lag-recovery `SceneSnapshot` before the events that followed
    /// it on the wire (the `seq <= snapshot.seq` folding rule).
    #[test]
    fn state_stream_preserves_wire_arrival_order_snapshot_then_events() {
        let mut inner = test_inner();
        inner.state_event_subscribed.store(true, Ordering::Relaxed);

        // Wire arrival order: an event, then a lag-recovery snapshot, then two
        // events that fold onto the snapshot.
        inner.stash_control_message(ControlMessage::StateEventMsg(sample_event(1)));
        inner.stash_control_message(ControlMessage::SceneSnapshot(sample_snapshot(5)));
        inner.stash_control_message(ControlMessage::StateEventMsg(sample_event(6)));
        inner.stash_control_message(ControlMessage::StateEventMsg(sample_event(7)));

        // Drain exactly as the pump does and record the shape+seq in order.
        let mut order: Vec<(&'static str, u64)> = Vec::new();
        while let Some(item) = inner.pending_state_stream.pop_front() {
            match item {
                StateStreamItem::Event(envelope) => order.push(("event", envelope.seq)),
                StateStreamItem::Snapshot(payload) => order.push(("snapshot", payload.seq)),
            }
        }
        assert_eq!(
            order,
            vec![
                ("event", 1),
                ("snapshot", 5),
                ("event", 6),
                ("event", 7),
            ],
            "state stream must preserve wire arrival order across both kinds"
        );
    }

    /// Finding: a client that never subscribes must not accumulate state-stream
    /// messages forever. With no callback registered they are dropped, not
    /// queued.
    #[test]
    fn unsubscribed_client_drops_state_stream_instead_of_accumulating() {
        let mut inner = test_inner();
        assert!(!inner.state_event_subscribed.load(Ordering::Relaxed));

        for seq in 0..10 {
            inner.stash_control_message(ControlMessage::StateEventMsg(sample_event(seq)));
            inner.stash_control_message(ControlMessage::SceneSnapshot(sample_snapshot(seq)));
        }
        assert!(
            inner.pending_state_stream.is_empty(),
            "unsubscribed client must not queue state-stream messages"
        );
        assert_eq!(inner.dropped_unsubscribed_events, 20);

        // Once subscribed, subsequent messages queue normally.
        inner.state_event_subscribed.store(true, Ordering::Relaxed);
        inner.stash_control_message(ControlMessage::StateEventMsg(sample_event(11)));
        assert_eq!(inner.pending_state_stream.len(), 1);
    }

    /// Finding: `set_state_event_callback(NULL)` must reliably quiesce delivery.
    /// The plumbing that guarantees it: registering flips the shared
    /// subscription flag and bumps the epoch the pump re-checks between
    /// invocations; NULL clears the flag and bumps the epoch again so any
    /// in-flight batch stops calling the stale context.
    #[test]
    fn set_state_event_callback_null_quiesces_subscription_and_epoch() {
        let name = CString::new("quiesce-test").unwrap();
        let attr = CString::new("camera.position").unwrap();
        let client = motionstage_swift_client_new(name.as_ptr(), attr.as_ptr());
        assert!(!client.is_null());
        let client_ref = unsafe { &*(client as *const MotionStageSwiftClient) };

        // Fresh client: no subscriber, epoch at zero.
        assert!(!client_ref.state_event_subscribed.load(Ordering::Acquire));
        assert_eq!(client_ref.state_event_epoch.load(Ordering::Acquire), 0);

        let store: Box<Mutex<Vec<String>>> = Box::new(Mutex::new(Vec::new()));
        let ctx = &*store as *const Mutex<Vec<String>> as *mut c_void;
        assert_eq!(
            motionstage_swift_client_set_state_event_callback(
                client,
                Some(collect_state_events),
                ctx,
            ),
            MOTIONSTAGE_SWIFT_STATUS_OK
        );
        assert!(client_ref.state_event_subscribed.load(Ordering::Acquire));
        assert_eq!(client_ref.state_event_epoch.load(Ordering::Acquire), 1);

        // NULL unsubscribes: flag cleared, epoch bumped so a stale-context
        // batch is halted. This call also blocks on the pump's callback lock,
        // so on return no delivery with the old context is in flight.
        assert_eq!(
            motionstage_swift_client_set_state_event_callback(client, None, ptr::null_mut()),
            MOTIONSTAGE_SWIFT_STATUS_OK
        );
        assert!(!client_ref.state_event_subscribed.load(Ordering::Acquire));
        assert_eq!(client_ref.state_event_epoch.load(Ordering::Acquire), 2);

        // free joins the pump thread; the context box outlives it.
        motionstage_swift_client_free(client);
        drop(store);
    }
}
