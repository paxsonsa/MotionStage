#![allow(
    clippy::useless_conversion,
    clippy::type_complexity,
    clippy::needless_borrow
)]

use motionstage_core::{AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject};
use motionstage_protocol::{
    BakeAttributeValue, ClientRole, DataFlowState, Feature, Mode, PlaybackRuntimeState,
    RecordingState, SamplingMode,
};
use motionstage_server::{ServerConfig, ServerHandle};
use pyo3::{
    exceptions::{PyDeprecationWarning, PyRuntimeError, PyValueError},
    prelude::*,
    types::{PyAny, PyDict},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Set `MOTIONSTAGE_DEBUG_H264=1` to dump H.264 to the default path, or
/// `MOTIONSTAGE_DEBUG_H264=/path/to/file.h264` for a custom path (supports FIFOs).
const DEBUG_H264_DEFAULT_PATH: &str = "/tmp/motionstage_debug.h264";

#[pyclass(name = "MotionStageServer")]
pub struct PyMotionStageServer {
    server: ServerHandle,
    rt: tokio::runtime::Runtime,
    encoder: Arc<std::sync::Mutex<Option<motionstage_media::encoder::H264Encoder>>>,
    video_fps: std::sync::atomic::AtomicU32,
    // Single-flight gate for the off-thread video encode+push pipeline.
    video_encode_inflight: Arc<AtomicBool>,
    debug_h264_path: Option<String>,
    // Companion UI listener, lazily started on first request and held alive here.
    companion_ui: std::sync::Mutex<Option<motionstage_server::companion_ui::CompanionUiHandle>>,
}

#[pymethods]
impl PyMotionStageServer {
    #[new]
    #[pyo3(signature = (name=None, discoverable=true, bind_addr=None))]
    pub fn new(
        name: Option<String>,
        discoverable: bool,
        bind_addr: Option<String>,
    ) -> PyResult<Self> {
        let mut config = ServerConfig::default();
        if let Some(name) = name {
            config.name = name;
        }
        let addr = bind_addr.as_deref().unwrap_or("0.0.0.0:0");
        config.quic_bind_addr = addr.parse().map_err(|err| {
            PyValueError::new_err(format!("invalid bind address `{addr}`: {err}"))
        })?;
        config.enable_discovery = discoverable;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        let debug_h264_path = std::env::var("MOTIONSTAGE_DEBUG_H264").ok().and_then(|v| {
            if v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false") {
                return None;
            }
            let path = if v == "1" || v.eq_ignore_ascii_case("true") {
                DEBUG_H264_DEFAULT_PATH.to_string()
            } else {
                v
            };
            // Only truncate regular files (not FIFOs/pipes)
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.file_type().is_file() {
                    let _ = std::fs::write(&path, b"");
                }
            } else {
                // File doesn't exist — create it
                let _ = std::fs::write(&path, b"");
            }
            eprintln!("MOTIONSTAGE_DEBUG_H264: streaming to {path}");
            Some(path)
        });

        Ok(Self {
            server: ServerHandle::new(config),
            rt,
            encoder: Arc::new(std::sync::Mutex::new(None)),
            video_fps: std::sync::atomic::AtomicU32::new(24),
            video_encode_inflight: Arc::new(AtomicBool::new(false)),
            debug_h264_path,
            companion_ui: std::sync::Mutex::new(None),
        })
    }

    pub fn start(&self) -> PyResult<String> {
        let adv = self
            .rt
            .block_on(self.server.start())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(format!("{}:{}", adv.bind_host, adv.bind_port))
    }

    pub fn stop(&self) -> PyResult<()> {
        self.rt
            .block_on(self.server.stop())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    /// Start the companion-UI listener (idempotent) and return its bound port.
    ///
    /// Lazily spawns one axum HTTP+WebSocket listener on this server's Tokio
    /// runtime, holding a clone of the `ServerHandle`. Repeated calls return the
    /// already-bound port without binding twice.
    pub fn start_companion_ui(&self) -> PyResult<u16> {
        let mut slot = self
            .companion_ui
            .lock()
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        if let Some(handle) = slot.as_ref() {
            return Ok(handle.port());
        }
        let token = Some(Uuid::new_v4().to_string());
        let handle = self
            .rt
            .block_on(motionstage_server::companion_ui::serve_companion_ui(
                self.server.clone(),
                token,
            ))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        let port = handle.port();
        *slot = Some(handle);
        Ok(port)
    }

    /// The auth token required on the companion-UI `/ws` upgrade (carried in the
    /// launch URL). Returns `None` if the UI has not been started.
    pub fn companion_ui_token(&self) -> PyResult<Option<String>> {
        let slot = self
            .companion_ui
            .lock()
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(slot.as_ref().and_then(|h| h.auth_token.clone()))
    }

    /// Gracefully stop the companion-UI listener if running.
    pub fn stop_companion_ui(&self) -> PyResult<()> {
        let handle = {
            let mut slot = self
                .companion_ui
                .lock()
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            slot.take()
        };
        if let Some(handle) = handle {
            self.rt
                .block_on(handle.shutdown())
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        }
        Ok(())
    }

    /// Drain DCC-side actions the companion UI requested, for the plugin to execute
    /// on its main thread. Returns a list of dicts, each with a `kind` discriminator.
    pub fn drain_host_requests<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        use motionstage_server::HostRequest;
        let requests = self.rt.block_on(self.server.drain_host_requests());
        let mut out = Vec::with_capacity(requests.len());
        for req in requests {
            let d = PyDict::new_bound(py);
            match req {
                HostRequest::ResyncScene => {
                    d.set_item("kind", "resync_scene")?;
                }
                HostRequest::StartVideo {
                    width,
                    height,
                    fps,
                    source,
                } => {
                    d.set_item("kind", "start_video")?;
                    d.set_item("width", width)?;
                    d.set_item("height", height)?;
                    d.set_item("fps", fps)?;
                    d.set_item("source", source)?;
                }
                HostRequest::StopVideo => {
                    d.set_item("kind", "stop_video")?;
                }
                HostRequest::BakeTake {
                    take_id,
                    fps,
                    start_frame,
                } => {
                    d.set_item("kind", "bake_take")?;
                    d.set_item("take_id", take_id.to_string())?;
                    d.set_item("fps", fps)?;
                    d.set_item("start_frame", start_frame)?;
                }
            }
            out.push(d);
        }
        Ok(out)
    }

    /// Report the object names selected in the host DCC, for companion-UI highlight.
    pub fn set_host_selection(&self, names: Vec<String>) -> PyResult<()> {
        self.rt.block_on(self.server.set_host_selection(names));
        Ok(())
    }

    pub fn upsert_scene(&self, spec: &Bound<'_, PyDict>) -> PyResult<String> {
        let scene = parse_scene_spec(spec)?;
        let scene_id = self.rt.block_on(self.server.load_scene(scene));
        Ok(scene_id.to_string())
    }

    pub fn set_active_scene(&self, scene_id: String) -> PyResult<()> {
        let scene_id = Uuid::parse_str(scene_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        self.rt
            .block_on(self.server.set_active_scene(scene_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn set_live_mode(&self) -> PyResult<()> {
        self.rt
            .block_on(self.server.set_mode(Mode::LIVE))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn set_stopped_mode(&self) -> PyResult<()> {
        self.rt
            .block_on(self.server.set_mode(Mode::IDLE))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn set_mode(&self, mode: String) -> PyResult<String> {
        let requested = parse_mode(&mode)?;
        self.rt
            .block_on(self.server.set_mode(requested))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(mode_to_str(requested).to_owned())
    }

    pub fn mode(&self) -> PyResult<String> {
        let mode = self.rt.block_on(self.server.mode());
        Ok(mode_to_str(mode).to_owned())
    }

    pub fn set_mode_control_allowlist(&self, device_ids: Vec<String>) -> PyResult<()> {
        let mut parsed = Vec::with_capacity(device_ids.len());
        for raw in device_ids {
            let id = Uuid::parse_str(raw.trim()).map_err(|err| {
                PyValueError::new_err(format!("invalid device id `{raw}`: {err}"))
            })?;
            parsed.push(id);
        }
        self.rt
            .block_on(self.server.set_mode_control_allowlist(parsed));
        Ok(())
    }

    pub fn mode_control_allowlist(&self) -> PyResult<Vec<String>> {
        let allowlist = self.rt.block_on(self.server.mode_control_allowlist());
        Ok(allowlist.into_iter().map(|id| id.to_string()).collect())
    }

    pub fn metrics(&self) -> PyResult<(u64, u64, u64, u64, u64, u64, u64)> {
        let metrics = self.rt.block_on(self.server.metrics());
        Ok((
            metrics.accepted_sessions,
            metrics.rejected_sessions,
            metrics.motion_datagrams,
            metrics.motion_updates,
            metrics.signaling_messages,
            metrics.scheduler_ticks,
            metrics.publish_ticks,
        ))
    }

    pub fn start_recording(&self, path: String) -> PyResult<String> {
        let recording_id = self
            .rt
            .block_on(self.server.start_recording(path, now_ns()))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(recording_id.to_string())
    }

    pub fn stop_recording(&self) -> PyResult<()> {
        self.rt
            .block_on(self.server.stop_recording())
            .map(|_| ())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    #[pyo3(signature = (scene_id=None))]
    pub fn list_takes(
        &self,
        scene_id: Option<String>,
    ) -> PyResult<Vec<(String, String, String, String, u64, u64, bool, bool)>> {
        let parsed_scene = parse_optional_uuid(scene_id)?;
        let takes = self.rt.block_on(self.server.list_takes(parsed_scene));
        Ok(takes
            .into_iter()
            .map(|take| {
                (
                    take.take_id.to_string(),
                    take.scene_id.to_string(),
                    take.name,
                    take.path,
                    take.created_ns,
                    take.frame_count,
                    take.selected,
                    take.deleted,
                )
            })
            .collect())
    }

    pub fn select_take(&self, take_id: String) -> PyResult<String> {
        let take_id = Uuid::parse_str(take_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let selected = self
            .rt
            .block_on(self.server.select_take(take_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(selected.take_id.to_string())
    }

    #[pyo3(signature = (take_id, looping=false))]
    pub fn playback_play(&self, take_id: String, looping: bool) -> PyResult<(String, u64, bool)> {
        let take_id = Uuid::parse_str(take_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let (state, playhead_ns, looping) = self
            .rt
            .block_on(self.server.playback_play(take_id, looping))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok((
            playback_state_to_str(state).to_owned(),
            playhead_ns,
            looping,
        ))
    }

    pub fn playback_pause(&self, take_id: String) -> PyResult<(String, u64, bool)> {
        let take_id = Uuid::parse_str(take_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let (state, playhead_ns, looping) =
            self.rt
                .block_on(self.server.playback_pause(take_id))
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok((
            playback_state_to_str(state).to_owned(),
            playhead_ns,
            looping,
        ))
    }

    #[pyo3(signature = (take_id, seek_ns, looping=false))]
    pub fn playback_seek(
        &self,
        take_id: String,
        seek_ns: u64,
        looping: bool,
    ) -> PyResult<(String, u64, bool)> {
        let take_id = Uuid::parse_str(take_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let (state, playhead_ns, looping) = self
            .rt
            .block_on(self.server.playback_seek(take_id, seek_ns, looping))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok((
            playback_state_to_str(state).to_owned(),
            playhead_ns,
            looping,
        ))
    }

    pub fn playback_stop(&self, take_id: String) -> PyResult<(String, u64, bool)> {
        let take_id = Uuid::parse_str(take_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let (state, playhead_ns, looping) = self
            .rt
            .block_on(self.server.playback_stop(take_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok((
            playback_state_to_str(state).to_owned(),
            playhead_ns,
            looping,
        ))
    }

    pub fn delete_take(&self, take_id: String) -> PyResult<()> {
        let take_id = Uuid::parse_str(take_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        self.rt
            .block_on(self.server.delete_take(take_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    #[pyo3(signature = (take_id, sampling_mode=None))]
    pub fn open_take_bake_cursor(
        &self,
        take_id: String,
        sampling_mode: Option<String>,
    ) -> PyResult<(String, u64)> {
        let take_id = Uuid::parse_str(take_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let sampling_mode = parse_sampling_mode(sampling_mode.as_deref().unwrap_or("captured"))?;
        let (cursor_id, total_frames) = self
            .rt
            .block_on(self.server.open_take_bake_cursor(take_id, sampling_mode))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok((cursor_id.to_string(), total_frames))
    }

    pub fn read_take_bake_frame(
        &self,
        py: Python<'_>,
        cursor_id: String,
    ) -> PyResult<Option<(u64, u64, Vec<(String, String, PyObject)>)>> {
        let cursor_id = Uuid::parse_str(cursor_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let frame = self
            .rt
            .block_on(self.server.read_take_bake_frame(cursor_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(frame.map(|(frame_index, timestamp_ns, attributes)| {
            (
                frame_index,
                timestamp_ns,
                attributes
                    .into_iter()
                    .map(|attr| {
                        (
                            attr.object_id.to_string(),
                            attr.attribute,
                            bake_attribute_value_to_py(py, &attr.value),
                        )
                    })
                    .collect(),
            )
        }))
    }

    pub fn seek_take_bake_frame(
        &self,
        py: Python<'_>,
        cursor_id: String,
        frame_index: u64,
    ) -> PyResult<Option<(u64, u64, Vec<(String, String, PyObject)>)>> {
        let cursor_id = Uuid::parse_str(cursor_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let frame = self
            .rt
            .block_on(self.server.seek_take_bake_frame(cursor_id, frame_index))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(frame.map(|(resolved_index, timestamp_ns, attributes)| {
            (
                resolved_index,
                timestamp_ns,
                attributes
                    .into_iter()
                    .map(|attr| {
                        (
                            attr.object_id.to_string(),
                            attr.attribute,
                            bake_attribute_value_to_py(py, &attr.value),
                        )
                    })
                    .collect(),
            )
        }))
    }

    pub fn close_take_bake_cursor(&self, cursor_id: String) -> PyResult<()> {
        let cursor_id = Uuid::parse_str(cursor_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        self.rt
            .block_on(self.server.close_take_bake_cursor(cursor_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn sessions(
        &self,
    ) -> PyResult<
        Vec<(
            String,
            String,
            Option<String>,
            Vec<String>,
            Vec<String>,
            Vec<String>,
            String,
        )>,
    > {
        let sessions = self.rt.block_on(self.server.sessions());
        Ok(sessions
            .into_iter()
            .map(|session| {
                (
                    session.device_id.to_string(),
                    session.device_name,
                    session.session_id.map(|id| id.to_string()),
                    session
                        .roles
                        .into_iter()
                        .map(role_to_str)
                        .map(str::to_owned)
                        .collect(),
                    session
                        .features
                        .into_iter()
                        .map(feature_to_str)
                        .map(str::to_owned)
                        .collect(),
                    session
                        .advertised_attributes
                        .into_iter()
                        .map(|ad| ad.path)
                        .collect(),
                    format!("{:?}", session.state),
                )
            })
            .collect())
    }

    pub fn create_mapping(&self, request: &Bound<'_, PyDict>) -> PyResult<String> {
        let source_device = parse_uuid_from_request_item(request, "source_device")
            .map_err(|e| PyValueError::new_err(format!("mapping request: {e}")))?;
        let source_output = extract_request_string(request, "source_output")
            .map_err(|e| PyValueError::new_err(format!("mapping request: {e}")))?;
        let target_attribute = extract_request_string(request, "target_attribute")
            .map_err(|e| PyValueError::new_err(format!("mapping request: {e}")))?;
        let component_mask = match request.get_item("component_mask") {
            Ok(Some(raw)) if !raw.is_none() => Some(
                raw.extract::<Vec<usize>>()
                    .map_err(|err| PyValueError::new_err(err.to_string()))?,
            ),
            _ => None,
        };

        let snapshot = self.rt.block_on(self.server.runtime_snapshot());
        let target_scene = match request.get_item("target_scene") {
            Ok(Some(raw)) if !raw.is_none() => parse_uuid_from_any(&raw)?,
            _ => snapshot
                .active_scene
                .ok_or_else(|| PyRuntimeError::new_err("no active scene"))?,
        };
        let scene = snapshot
            .scenes
            .get(&target_scene)
            .ok_or_else(|| PyRuntimeError::new_err(format!("scene not found: {target_scene}")))?;

        let target_object = parse_uuid_from_request_item(request, "target_object_id")
            .map_err(|e| PyValueError::new_err(format!("mapping request: {e}")))?;
        if !scene.objects.contains_key(&target_object) {
            return Err(PyRuntimeError::new_err(format!(
                "target object id not found in scene: {target_object}"
            )));
        }

        let mapping_id = self
            .rt
            .block_on(self.server.create_mapping(
                MappingRequest {
                    source_device,
                    source_output,
                    target_scene,
                    target_object,
                    target_attribute,
                    component_mask,
                },
                now_ns(),
            ))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(mapping_id.to_string())
    }

    pub fn remove_mapping(&self, mapping_id: String) -> PyResult<()> {
        let mapping_id = Uuid::parse_str(mapping_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        self.rt
            .block_on(self.server.remove_mapping(mapping_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    #[pyo3(signature = (scene_id=None))]
    pub fn reset_scene_to_baseline(&self, scene_id: Option<String>) -> PyResult<u32> {
        let parsed_scene = parse_optional_uuid(scene_id)?;
        self.rt
            .block_on(self.server.reset_scene_to_baseline(parsed_scene))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    #[pyo3(signature = (scene_id=None))]
    pub fn commit_scene_baseline(&self, scene_id: Option<String>) -> PyResult<u32> {
        let parsed_scene = parse_optional_uuid(scene_id)?;
        self.rt
            .block_on(self.server.commit_scene_baseline(parsed_scene))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    #[pyo3(signature = (object_id, scene_id=None))]
    pub fn commit_object_baseline(
        &self,
        object_id: String,
        scene_id: Option<String>,
    ) -> PyResult<u32> {
        let object_id = Uuid::parse_str(object_id.trim())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let parsed_scene = parse_optional_uuid(scene_id)?;
        self.rt
            .block_on(self.server.commit_object_baseline(parsed_scene, object_id))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn runtime_attribute_values(
        &self,
        py: Python<'_>,
    ) -> PyResult<Vec<(String, String, String, PyObject)>> {
        let snapshot = self.rt.block_on(self.server.runtime_snapshot());
        let Some(active_scene_id) = snapshot.active_scene else {
            return Ok(Vec::new());
        };
        let Some(scene) = snapshot.scenes.get(&active_scene_id) else {
            return Ok(Vec::new());
        };

        let mut rows: Vec<(String, String, String, PyObject)> = Vec::new();
        for object in scene.objects.values() {
            for attr in object.attributes.values() {
                rows.push((
                    object.id.to_string(),
                    object.name.clone(),
                    attr.name.clone(),
                    attribute_value_to_py(py, &attr.current_value),
                ));
            }
        }
        Ok(rows)
    }

    // --- Video ---

    pub fn set_master_video_descriptor(&self, width: u32, height: u32, fps: u32) -> PyResult<()> {
        use motionstage_media::{
            ColorPrimaries, DynamicRange, TransferFunction, VideoCodec, VideoStreamDescriptor,
        };

        let descriptor = VideoStreamDescriptor {
            width,
            height,
            fps,
            dynamic_range: DynamicRange::Sdr,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            bit_depth: 8,
            codec: VideoCodec::H264,
        };

        self.rt
            .block_on(self.server.set_master_video_descriptor(descriptor))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        // (Re)create the encoder to match the descriptor
        // Scale bitrate by pixel-rate to avoid heavy macroblocking at higher resolutions.
        // ~0.08 bits/pixel at target fps, clamped to a practical realtime range.
        let target_bitrate_bps = ((width as u64)
            .saturating_mul(height as u64)
            .saturating_mul(fps as u64)
            .saturating_mul(8)
            / 100)
            .clamp(1_500_000, 12_000_000) as u32;

        let encoder = motionstage_media::encoder::H264Encoder::new(
            width,
            height,
            fps as f32,
            target_bitrate_bps,
        )
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        *self
            .encoder
            .lock()
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))? = Some(encoder);

        self.video_fps
            .store(fps, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    /// Accept raw RGBA bytes from Python and stream them to all WebRTC peers.
    ///
    /// Returns immediately. The CPU-bound H.264 encode and the network push run
    /// on the Tokio runtime, NOT on the calling thread. In Blender this call
    /// happens on the main draw thread, so doing encode+push inline used to
    /// freeze the viewport and hold the GIL (starving the SDK event poller).
    /// Frames that arrive while an encode+push is still in flight are dropped
    /// (conflation: live feedback wants the freshest frame, never a backlog).
    pub fn push_video_frame(&self, data: &[u8], timestamp_ns: u64) -> PyResult<()> {
        self.enqueue_video_frame(data, timestamp_ns, false)
    }

    /// Accept raw BGRA bytes from Python and stream them to all WebRTC peers.
    /// Same off-thread, single-flight behavior as [`Self::push_video_frame`].
    pub fn push_video_frame_bgra(&self, data: &[u8], timestamp_ns: u64) -> PyResult<()> {
        self.enqueue_video_frame(data, timestamp_ns, true)
    }

    pub fn video_peer_count(&self) -> PyResult<u32> {
        Ok(self.rt.block_on(self.server.video_peer_count()))
    }
}

impl PyMotionStageServer {
    /// Hand a captured frame to the Tokio runtime for encode + WebRTC push,
    /// returning immediately so the caller's thread (Blender's main draw thread)
    /// is never blocked on encoding or network I/O.
    fn enqueue_video_frame(&self, data: &[u8], _timestamp_ns: u64, bgra: bool) -> PyResult<()> {
        // Single-flight gate: if an encode+push is still running, drop this
        // frame rather than queueing. Live feedback wants the newest frame, and
        // an unbounded queue would grow memory and add latency under backpressure.
        if self
            .video_encode_inflight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }

        // Copy the borrowed pixel buffer so the spawned task can own it.
        let frame = data.to_vec();
        let fps = self.video_fps.load(Ordering::Relaxed).max(1);
        let duration = std::time::Duration::from_secs_f64(1.0 / fps as f64);

        let server = self.server.clone();
        let encoder = Arc::clone(&self.encoder);
        let inflight = Arc::clone(&self.video_encode_inflight);
        let debug_path = self.debug_h264_path.clone();

        self.rt.spawn(async move {
            // Release the single-flight gate however this task exits.
            struct InflightGuard(Arc<AtomicBool>);
            impl Drop for InflightGuard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _gate = InflightGuard(inflight);

            // Force an IDR for any newly-joined peer so it can start decoding.
            let needs_keyframe = server.take_keyframe_needed().await;

            let encoded = {
                let mut guard = match encoder.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                let encoder = match guard.as_mut() {
                    Some(encoder) => encoder,
                    None => return, // set_master_video_descriptor not called yet
                };
                if needs_keyframe {
                    encoder.force_keyframe();
                }
                let result = if bgra {
                    encoder.encode_bgra(&frame)
                } else {
                    encoder.encode_rgba(&frame)
                };
                match result {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        eprintln!("motionstage: video encode failed: {err}");
                        return;
                    }
                }
                // encoder lock released here, before the network push
            };

            if let Some(path) = debug_path.as_deref() {
                dump_h264_to(path, &encoded);
            }

            if let Err(err) = server.push_video_frame(encoded, duration).await {
                eprintln!("motionstage: video push failed: {err}");
            }
        });

        Ok(())
    }
}

/// Append encoded H.264 bytes to the debug dump file/pipe (if enabled).
fn dump_h264_to(path: &str, encoded: &[u8]) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = f.write_all(encoded);
    }
}

#[pymodule]
fn motionstage_sdk_rust(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMotionStageServer>()?;
    Ok(())
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_nanos() as u64)
        .unwrap_or_default()
}

fn parse_mode(value: &str) -> PyResult<Mode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "idle" | "stopped" | "stop" => Ok(Mode::IDLE),
        "live" => Ok(Mode::LIVE),
        "recording" | "record" => Ok(Mode::RECORDING),
        "playback" | "play" => Ok(Mode::PLAYBACK),
        other => Err(PyValueError::new_err(format!(
            "unsupported mode `{other}` (expected idle/live/recording/playback)"
        ))),
    }
}

fn parse_optional_uuid(value: Option<String>) -> PyResult<Option<Uuid>> {
    match value {
        Some(raw) => {
            let normalized = raw.trim();
            if normalized.is_empty() {
                Ok(None)
            } else {
                Uuid::parse_str(normalized)
                    .map(Some)
                    .map_err(|err| PyValueError::new_err(err.to_string()))
            }
        }
        None => Ok(None),
    }
}

fn mode_to_str(mode: Mode) -> &'static str {
    use DataFlowState::*;
    use RecordingState::*;
    match (mode.data_flow, mode.recording) {
        (Idle, Inactive) => "idle",
        (Live, Inactive) => "live",
        (Live, Recording) => "recording",
        (Live, Playback) => "playback",
        _ => "unknown",
    }
}

fn role_to_str(role: ClientRole) -> &'static str {
    match role {
        ClientRole::MotionSource => "motion_source",
        ClientRole::CameraController => "camera_controller",
        ClientRole::VideoSink => "video_sink",
        ClientRole::Operator => "operator",
    }
}

fn feature_to_str(feature: Feature) -> &'static str {
    match feature {
        Feature::Motion => "motion",
        Feature::Mapping => "mapping",
        Feature::Recording => "recording",
        Feature::Video => "video",
        Feature::Hdr10 => "hdr10",
        Feature::SdrFallback => "sdr_fallback",
    }
}

fn playback_state_to_str(state: PlaybackRuntimeState) -> &'static str {
    match state {
        PlaybackRuntimeState::Stopped => "stopped",
        PlaybackRuntimeState::Playing => "playing",
        PlaybackRuntimeState::Paused => "paused",
    }
}

fn parse_sampling_mode(value: &str) -> PyResult<SamplingMode> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized == "captured" {
        return Ok(SamplingMode::Captured);
    }
    if let Some(raw) = normalized.strip_prefix("fixed:") {
        let fps: u32 = raw
            .parse()
            .map_err(|err| PyValueError::new_err(format!("invalid fixed fps `{raw}`: {err}")))?;
        if fps == 0 {
            return Err(PyValueError::new_err("fixed fps must be > 0"));
        }
        return Ok(SamplingMode::FixedFps { fps });
    }
    Err(PyValueError::new_err(format!(
        "unsupported sampling mode `{value}` (expected captured or fixed:<fps>)"
    )))
}

fn parse_scene_spec(spec: &Bound<'_, PyDict>) -> PyResult<Scene> {
    let name = extract_spec_string(spec, "name", "scene spec")?;
    let mut scene = Scene::new(name);

    if let Some(raw) = spec
        .get_item("id")
        .map_err(|err| PyValueError::new_err(err.to_string()))?
    {
        if !raw.is_none() {
            scene.id = parse_uuid_from_any(&raw)?;
        }
    }

    let raw_objects = spec
        .get_item("objects")
        .map_err(|err| PyValueError::new_err(err.to_string()))?
        .ok_or_else(|| {
            PyValueError::new_err("scene spec: missing required list field 'objects'")
        })?;
    let objects = raw_objects
        .iter()
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    for raw_object in objects {
        let raw_object = raw_object.map_err(|err| PyValueError::new_err(err.to_string()))?;
        let object_spec = raw_object
            .downcast::<PyDict>()
            .map_err(|_| PyValueError::new_err("scene object spec must be a dict"))?;
        let object = parse_scene_object_spec(&object_spec)?;
        scene = scene.with_object(object);
    }
    Ok(scene)
}

fn parse_scene_object_spec(spec: &Bound<'_, PyDict>) -> PyResult<SceneObject> {
    let name = extract_spec_string(spec, "name", "object spec")?;
    let mut object = SceneObject::new(name);

    if let Some(raw) = spec
        .get_item("id")
        .map_err(|err| PyValueError::new_err(err.to_string()))?
    {
        if !raw.is_none() {
            object.id = parse_uuid_from_any(&raw)?;
        }
    }

    let raw_attributes = spec
        .get_item("attributes")
        .map_err(|err| PyValueError::new_err(err.to_string()))?
        .ok_or_else(|| {
            PyValueError::new_err("object spec: missing required list field 'attributes'")
        })?;
    let attributes = raw_attributes
        .iter()
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    for raw_attribute in attributes {
        let raw_attribute = raw_attribute.map_err(|err| PyValueError::new_err(err.to_string()))?;
        let attr_spec = raw_attribute
            .downcast::<PyDict>()
            .map_err(|_| PyValueError::new_err("scene attribute spec must be a dict"))?;
        let attr = parse_scene_attribute_spec(&attr_spec)?;
        object = object.with_attribute(attr);
    }

    Ok(object)
}

fn parse_scene_attribute_spec(spec: &Bound<'_, PyDict>) -> PyResult<SceneAttribute> {
    let name = extract_spec_string(spec, "name", "attribute spec")?;
    let default_value = extract_spec_attribute_value(spec, "default_value")
        .or_else(|_| extract_spec_attribute_value(spec, "value"))
        .map_err(|_| {
            PyValueError::new_err(
                "attribute spec: missing required 'default_value' or 'value' field",
            )
        })?;
    let current_value =
        extract_spec_attribute_value(spec, "value").unwrap_or(default_value.clone());

    let mut attr = SceneAttribute::new(name, default_value);
    attr.current_value = current_value;

    if let Some(raw) = spec
        .get_item("live_enabled")
        .map_err(|err| PyValueError::new_err(err.to_string()))?
    {
        if !raw.is_none() {
            attr.live_enabled = raw
                .extract::<bool>()
                .map_err(|err| PyValueError::new_err(err.to_string()))?;
        }
    }
    if let Some(raw) = spec
        .get_item("record_enabled")
        .map_err(|err| PyValueError::new_err(err.to_string()))?
    {
        if !raw.is_none() {
            attr.record_enabled = raw
                .extract::<bool>()
                .map_err(|err| PyValueError::new_err(err.to_string()))?;
        }
    }

    Ok(attr)
}

fn extract_spec_string(spec: &Bound<'_, PyDict>, key: &str, context: &str) -> PyResult<String> {
    let raw = spec
        .get_item(key)
        .map_err(|err| PyValueError::new_err(err.to_string()))?
        .ok_or_else(|| {
            PyValueError::new_err(format!("{context}: missing required string field '{key}'"))
        })?;
    if let Ok(value) = raw.extract::<String>() {
        return Ok(value);
    }
    raw.str()
        .and_then(|v| v.to_str().map(|s| s.to_owned()))
        .map_err(|err| PyValueError::new_err(err.to_string()))
}

fn extract_spec_attribute_value(spec: &Bound<'_, PyDict>, key: &str) -> PyResult<AttributeValue> {
    let raw = spec
        .get_item(key)
        .map_err(|err| PyValueError::new_err(err.to_string()))?
        .ok_or_else(|| PyValueError::new_err(format!("missing required field `{key}`")))?;
    parse_attribute_value(spec, &raw)
}

fn parse_attribute_value(
    spec: &Bound<'_, PyDict>,
    value: &Bound<'_, PyAny>,
) -> PyResult<AttributeValue> {
    let explicit_type = spec
        .get_item("type")
        .map_err(|err| PyValueError::new_err(err.to_string()))?
        .and_then(|raw| {
            if raw.is_none() {
                None
            } else {
                raw.extract::<String>().ok()
            }
        });

    if let Some(type_name) = explicit_type {
        return parse_attribute_value_typed(type_name.as_str(), value);
    }
    parse_attribute_value_inferred(value)
}

fn parse_attribute_value_typed(
    type_name: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<AttributeValue> {
    match type_name.trim().to_ascii_lowercase().as_str() {
        "bool" => Ok(AttributeValue::Bool(
            value
                .extract::<bool>()
                .map_err(|err| PyValueError::new_err(err.to_string()))?,
        )),
        "trigger" => Ok(AttributeValue::Trigger(
            value
                .extract::<bool>()
                .map_err(|err| PyValueError::new_err(err.to_string()))?,
        )),
        "int32" => Ok(AttributeValue::Int32(
            value
                .extract::<i32>()
                .map_err(|err| PyValueError::new_err(err.to_string()))?,
        )),
        "float32" => Ok(AttributeValue::Float32(
            value
                .extract::<f32>()
                .map_err(|err| PyValueError::new_err(err.to_string()))?,
        )),
        "float64" => Ok(AttributeValue::Float64(
            value
                .extract::<f64>()
                .map_err(|err| PyValueError::new_err(err.to_string()))?,
        )),
        "vec2f" => Ok(AttributeValue::Vec2f(extract_vec_f32::<2>(value)?)),
        "vec3f" => Ok(AttributeValue::Vec3f(extract_vec_f32::<3>(value)?)),
        "vec4f" => Ok(AttributeValue::Vec4f(extract_vec_f32::<4>(value)?)),
        "quatf" => Ok(AttributeValue::Quatf(extract_vec_f32::<4>(value)?)),
        "mat4f" => Ok(AttributeValue::Mat4f(extract_mat4f(value)?)),
        other => Err(PyValueError::new_err(format!(
            "attribute spec: unknown type '{other}'; valid types: bool, trigger, int32, float32, float64, vec2f, vec3f, vec4f, quatf, mat4f"
        ))),
    }
}

fn parse_attribute_value_inferred(value: &Bound<'_, PyAny>) -> PyResult<AttributeValue> {
    let py = value.py();
    py.import_bound("warnings")?.call_method1(
        "warn",
        (
            "Attribute value type was inferred without an explicit 'type' field. \
             Add {\"type\": \"float32\"} (or float64, int32, vec3f, etc.) to suppress this warning. \
             Inference will be removed in a future release.",
            py.get_type_bound::<PyDeprecationWarning>(),
            1i32,
        ),
    )?;
    if let Ok(v) = value.extract::<bool>() {
        return Ok(AttributeValue::Bool(v));
    }
    if let Ok(v) = value.extract::<i32>() {
        return Ok(AttributeValue::Int32(v));
    }
    if let Ok(v) = value.extract::<f64>() {
        return Ok(AttributeValue::Float64(v));
    }

    let list = value
        .extract::<Vec<f32>>()
        .map_err(|_| PyValueError::new_err("cannot infer attribute type from value"))?;
    match list.len() {
        2 => Ok(AttributeValue::Vec2f([list[0], list[1]])),
        3 => Ok(AttributeValue::Vec3f([list[0], list[1], list[2]])),
        4 => Ok(AttributeValue::Vec4f([list[0], list[1], list[2], list[3]])),
        len => Err(PyValueError::new_err(format!(
            "cannot infer vector attribute type from length {len}"
        ))),
    }
}

fn extract_vec_f32<const N: usize>(value: &Bound<'_, PyAny>) -> PyResult<[f32; N]> {
    let vec = value
        .extract::<Vec<f32>>()
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    let len = vec.len();
    vec.try_into()
        .map_err(|_| PyValueError::new_err(format!("expected vector length {N}, got {}", len)))
}

fn extract_mat4f(value: &Bound<'_, PyAny>) -> PyResult<[[f32; 4]; 4]> {
    let rows = value
        .extract::<Vec<Vec<f32>>>()
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    if rows.len() != 4 {
        return Err(PyValueError::new_err(format!(
            "expected matrix row count 4, got {}",
            rows.len()
        )));
    }
    let mut out = [[0.0f32; 4]; 4];
    for (row_idx, row) in rows.into_iter().enumerate() {
        if row.len() != 4 {
            return Err(PyValueError::new_err(format!(
                "expected matrix column count 4 for row {row_idx}, got {}",
                row.len()
            )));
        }
        out[row_idx] = [row[0], row[1], row[2], row[3]];
    }
    Ok(out)
}

fn parse_uuid_from_any(value: &Bound<'_, PyAny>) -> PyResult<Uuid> {
    let as_text = if let Ok(raw) = value.extract::<String>() {
        raw
    } else {
        value
            .str()
            .and_then(|v| v.to_str().map(|s| s.to_owned()))
            .map_err(|err| PyValueError::new_err(err.to_string()))?
    };
    Uuid::parse_str(as_text.trim()).map_err(|err| PyValueError::new_err(err.to_string()))
}

fn parse_uuid_from_request_item(request: &Bound<'_, PyDict>, key: &str) -> PyResult<Uuid> {
    let raw = request
        .get_item(key)
        .map_err(|err| PyValueError::new_err(err.to_string()))?
        .ok_or_else(|| PyValueError::new_err(format!("missing required field `{key}`")))?;
    parse_uuid_from_any(&raw)
}

fn extract_request_string(request: &Bound<'_, PyDict>, key: &str) -> PyResult<String> {
    let raw = request
        .get_item(key)
        .map_err(|err| PyValueError::new_err(err.to_string()))?
        .ok_or_else(|| PyValueError::new_err(format!("missing required field `{key}`")))?;
    if let Ok(value) = raw.extract::<String>() {
        return Ok(value);
    }
    raw.str()
        .and_then(|v| v.to_str().map(|s| s.to_owned()))
        .map_err(|err| PyValueError::new_err(err.to_string()))
}

fn attribute_value_to_py(py: Python<'_>, value: &AttributeValue) -> PyObject {
    match value {
        AttributeValue::Bool(v) | AttributeValue::Trigger(v) => v.into_py(py),
        AttributeValue::Int32(v) => v.into_py(py),
        AttributeValue::Float32(v) => f64::from(*v).into_py(py),
        AttributeValue::Float64(v) => v.into_py(py),
        AttributeValue::Vec2f(v) => vec![v[0], v[1]].into_py(py),
        AttributeValue::Vec3f(v) => vec![v[0], v[1], v[2]].into_py(py),
        AttributeValue::Vec4f(v) | AttributeValue::Quatf(v) => {
            vec![v[0], v[1], v[2], v[3]].into_py(py)
        }
        AttributeValue::Mat4f(v) => v
            .iter()
            .map(|row| vec![row[0], row[1], row[2], row[3]])
            .collect::<Vec<_>>()
            .into_py(py),
    }
}

fn bake_attribute_value_to_py(py: Python<'_>, value: &BakeAttributeValue) -> PyObject {
    match value {
        BakeAttributeValue::Bool(v) | BakeAttributeValue::Trigger(v) => v.into_py(py),
        BakeAttributeValue::Int32(v) => v.into_py(py),
        BakeAttributeValue::Float32(v) => f64::from(*v).into_py(py),
        BakeAttributeValue::Float64(v) => v.into_py(py),
        BakeAttributeValue::Vec2f(v) => vec![v[0], v[1]].into_py(py),
        BakeAttributeValue::Vec3f(v) => vec![v[0], v[1], v[2]].into_py(py),
        BakeAttributeValue::Vec4f(v) | BakeAttributeValue::Quatf(v) => {
            vec![v[0], v[1], v[2], v[3]].into_py(py)
        }
        BakeAttributeValue::Mat4f(v) => v
            .iter()
            .map(|row| vec![row[0], row[1], row[2], row[3]])
            .collect::<Vec<_>>()
            .into_py(py),
    }
}

#[cfg(test)]
mod tests {
    use super::PyMotionStageServer;

    #[test]
    fn rust_binding_constructs_server() {
        let server = PyMotionStageServer::new(Some("py-test".into()), false, None)
            .expect("py server should build");
        let _ = server.start().expect("start should succeed");
        server.stop().expect("stop should succeed");
    }
}
