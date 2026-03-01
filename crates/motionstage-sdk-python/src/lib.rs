use motionstage_core::{AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject};
use motionstage_protocol::{ClientRole, Feature, Mode};
use motionstage_server::{ServerConfig, ServerHandle};
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
    types::{PyAny, PyDict},
};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[pyclass(name = "MotionStageServer")]
pub struct PyMotionStageServer {
    server: ServerHandle,
    rt: tokio::runtime::Runtime,
}

#[pymethods]
impl PyMotionStageServer {
    #[new]
    #[pyo3(signature = (name=None))]
    pub fn new(name: Option<String>) -> PyResult<Self> {
        let mut config = ServerConfig::default();
        if let Some(name) = name {
            config.name = name;
        }
        config.quic_bind_addr = "127.0.0.1:0".parse().expect("static address parses");
        config.enable_discovery = false;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        Ok(Self {
            server: ServerHandle::new(config),
            rt,
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
            .block_on(self.server.set_mode(Mode::Live))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    pub fn set_stopped_mode(&self) -> PyResult<()> {
        self.rt
            .block_on(self.server.set_mode(Mode::Idle))
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
                    session.advertised_attributes,
                    format!("{:?}", session.state),
                )
            })
            .collect())
    }

    pub fn create_mapping(&self, request: &Bound<'_, PyDict>) -> PyResult<String> {
        let source_device = parse_uuid_from_request_item(request, "source_device")?;
        let source_output = extract_request_string(request, "source_output")?;
        let target_attribute = extract_request_string(request, "target_attribute")?;
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

        let target_object = parse_uuid_from_request_item(request, "target_object_id")?;
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
        "idle" | "stopped" | "stop" => Ok(Mode::Idle),
        "live" => Ok(Mode::Live),
        "recording" | "record" => Ok(Mode::Recording),
        other => Err(PyValueError::new_err(format!(
            "unsupported mode `{other}` (expected idle/live/recording)"
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
    match mode {
        Mode::Idle => "idle",
        Mode::Live => "live",
        Mode::Recording => "recording",
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

fn parse_scene_spec(spec: &Bound<'_, PyDict>) -> PyResult<Scene> {
    let name = extract_spec_string(spec, "name")?;
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
        .ok_or_else(|| PyValueError::new_err("missing required field `objects`"))?;
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
    let name = extract_spec_string(spec, "name")?;
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
        .ok_or_else(|| PyValueError::new_err("missing required field `attributes`"))?;
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
    let name = extract_spec_string(spec, "name")?;
    let default_value = extract_spec_attribute_value(spec, "default_value")
        .or_else(|_| extract_spec_attribute_value(spec, "value"))?;
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

fn extract_spec_string(spec: &Bound<'_, PyDict>, key: &str) -> PyResult<String> {
    let raw = spec
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
            "unsupported attribute type `{other}`"
        ))),
    }
}

fn parse_attribute_value_inferred(value: &Bound<'_, PyAny>) -> PyResult<AttributeValue> {
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

#[cfg(test)]
mod tests {
    use super::PyMotionStageServer;

    #[test]
    fn rust_binding_constructs_server() {
        let server =
            PyMotionStageServer::new(Some("py-test".into())).expect("py server should build");
        let _ = server.start().expect("start should succeed");
        server.stop().expect("stop should succeed");
    }
}
