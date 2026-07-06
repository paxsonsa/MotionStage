//! Deterministic hand-authored `.usda` export for CMTRK recordings.
//!
//! The exporter emits real, DCC-consumable USD text (see
//! `docs/design-scene-management.md` §4): one `Xform` prim per recorded
//! object with `xformOp` timeSamples, a child `Camera "shape"` prim for lens
//! attributes, typed custom attributes for everything else, and a
//! `Scope "markers"` summary of the marker timeline.
//!
//! Quaternion convention: `AttributeValue::Quatf` stores components as
//! `[x, y, z, w]` (see `motionstage-core::runtime`, where the identity is
//! `[0, 0, 0, 1]` and multiplication destructures `[x, y, z, w]`). USD's
//! `quatf` text form is `(w, x, y, z)`, so components are reordered on
//! export.

use std::collections::{BTreeMap, BTreeSet};

use motionstage_core::{AttributeValue, ObjectId};
use motionstage_recording::{RecordingFile, RecordingMarker};

/// Stage up-axis declared in the layer metadata.
///
/// MotionStage captures in Z-up/meters (the server-side convention); export
/// as `Y` only for consumers that require it — values are re-expressed for
/// the declared axis at export time, never at capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpAxis {
    #[default]
    Z,
    Y,
}

impl UpAxis {
    fn token(self) -> &'static str {
        match self {
            UpAxis::Z => "Z",
            UpAxis::Y => "Y",
        }
    }
}

/// Options controlling `.usda` generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsdExportOptions {
    /// USD `timeCodesPerSecond`. Each sample's timeCode is
    /// `(timestamp_ns - started_ns) * time_codes_per_second / 1e9`.
    pub time_codes_per_second: u32,
    /// Stage `upAxis` metadata. Defaults to `Z` (capture convention).
    pub up_axis: UpAxis,
}

impl Default for UsdExportOptions {
    fn default() -> Self {
        Self {
            time_codes_per_second: 120,
            up_axis: UpAxis::Z,
        }
    }
}

/// Export a recording as `.usda` text with default options
/// (`timeCodesPerSecond = 120`).
pub fn export(recording: &RecordingFile) -> String {
    export_with_options(recording, &UsdExportOptions::default())
}

/// Export a recording as `.usda` text.
///
/// Output is deterministic: objects and attributes are emitted in sorted
/// order and floats use Rust's locale-independent shortest round-trip
/// formatting.
pub fn export_with_options(recording: &RecordingFile, options: &UsdExportOptions) -> String {
    let tcps = options.time_codes_per_second.max(1);
    let started_ns = recording.manifest.started_ns;

    let objects = collect_objects(recording, started_ns, tcps);

    let mut out = String::new();
    write_header(&mut out, recording, tcps, options.up_axis);

    for (object_id, object) in &objects {
        write_object_prim(&mut out, object_id, object);
    }

    if !recording.markers.is_empty() {
        write_markers_scope(&mut out, &recording.markers);
    }

    out
}

/// Prim name for a recorded object: `o_<uuid-with-underscores>`, which is
/// always a USD-legal identifier.
pub fn prim_name_for_object(object_id: &ObjectId) -> String {
    format!("o_{}", object_id.to_string().replace('-', "_"))
}

/// Where a recorded attribute lands in the USD prim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// `double3 xformOp:translate`
    Translate,
    /// `quatf xformOp:orient`
    Orient,
    /// `float3 xformOp:scale`
    Scale,
    /// `float <name>` on the child `Camera "shape"` prim.
    Camera(&'static str),
    /// `custom <type> <name>` on the Xform prim.
    Custom,
}

struct AttrTrack {
    slot: Slot,
    /// USD value type for `Slot::Custom` tracks, locked from the track's
    /// first sample.
    usd_type: &'static str,
    /// `(timeCode, encoded value)` samples, sorted by timeCode with unique
    /// timeCodes (last write in frame order wins).
    samples: Vec<(f64, String)>,
}

struct ObjectData {
    /// Attribute tracks keyed by the recorded attribute name.
    tracks: BTreeMap<String, AttrTrack>,
    /// Samples dropped because their value variant did not match the track's
    /// type locked from its first sample.
    skipped_mismatched_samples: u64,
}

/// Raw `(timeCode, value)` samples per (object, attribute), in frame order.
type RawSamples<'a> = BTreeMap<ObjectId, BTreeMap<String, Vec<(f64, &'a AttributeValue)>>>;

fn collect_objects(
    recording: &RecordingFile,
    started_ns: u64,
    tcps: u32,
) -> BTreeMap<ObjectId, ObjectData> {
    // Pass 1: gather raw samples per (object, attribute) in frame order.
    let mut raw: RawSamples = BTreeMap::new();
    for frame in &recording.frames {
        let time_code = time_code(frame.timestamp_ns, started_ns, tcps);
        for attr in &frame.attributes {
            raw.entry(attr.object_id)
                .or_default()
                .entry(attr.attribute.clone())
                .or_default()
                .push((time_code, &attr.value));
        }
    }

    // Pass 2: classify and encode. Attributes are visited in sorted-name
    // order; the first attribute classifying to an xformOp/Camera slot
    // claims it, and later claimants fall back to `Slot::Custom` so no slot
    // ever merges samples from two attributes. Each track's USD type is
    // locked from its first sample; samples whose variant no longer matches
    // are skipped and counted.
    let mut objects = BTreeMap::new();
    for (object_id, attrs) in raw {
        let mut claimed: Vec<Slot> = Vec::new();
        let mut tracks = BTreeMap::new();
        let mut skipped_mismatched_samples = 0u64;

        for (name, raw_samples) in attrs {
            let first_value = raw_samples
                .first()
                .map(|(_, value)| *value)
                .expect("tracks are created with at least one sample");
            let mut slot = classify(&name, first_value);
            if slot != Slot::Custom {
                if claimed.contains(&slot) {
                    slot = Slot::Custom;
                } else {
                    claimed.push(slot);
                }
            }
            let usd_type = usd_type_for(first_value);

            let mut samples = Vec::with_capacity(raw_samples.len());
            for (time_code, value) in raw_samples {
                let encoded = if slot == Slot::Custom && usd_type_for(value) != usd_type {
                    // Value variant no longer matches the type locked from
                    // the track's first sample.
                    None
                } else {
                    encode_for_slot(slot, value)
                };
                match encoded {
                    Some(encoded) => samples.push((time_code, encoded)),
                    None => skipped_mismatched_samples += 1,
                }
            }
            sort_and_dedupe_samples(&mut samples);
            tracks.insert(
                name,
                AttrTrack {
                    slot,
                    usd_type,
                    samples,
                },
            );
        }

        objects.insert(
            object_id,
            ObjectData {
                tracks,
                skipped_mismatched_samples,
            },
        );
    }

    objects
}

/// Sort samples by timeCode and collapse duplicate timeCodes. The sort is
/// stable, so among samples sharing a timeCode the last one in frame order
/// wins regardless of input frame ordering.
fn sort_and_dedupe_samples(samples: &mut Vec<(f64, String)>) {
    samples.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("timeCodes are finite"));
    samples.dedup_by(|next, prev| {
        if next.0 == prev.0 {
            prev.1 = std::mem::take(&mut next.1);
            true
        } else {
            false
        }
    });
}

fn write_header(out: &mut String, recording: &RecordingFile, tcps: u32, up_axis: UpAxis) {
    let started_ns = recording.manifest.started_ns;
    out.push_str("#usda 1.0\n(\n");
    out.push_str(&format!("    upAxis = \"{}\"\n", up_axis.token()));
    out.push_str("    metersPerUnit = 1\n");
    out.push_str(&format!("    timeCodesPerSecond = {tcps}\n"));
    if !recording.frames.is_empty() {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for frame in &recording.frames {
            let tc = time_code(frame.timestamp_ns, started_ns, tcps);
            min = min.min(tc);
            max = max.max(tc);
        }
        out.push_str(&format!("    startTimeCode = {}\n", fmt_f64(min)));
        out.push_str(&format!("    endTimeCode = {}\n", fmt_f64(max)));
    }
    out.push_str("    customLayerData = {\n");
    out.push_str(&format!(
        "        string recording_id = \"{}\"\n",
        recording.manifest.recording_id
    ));
    out.push_str(&format!(
        "        string scene_id = \"{}\"\n",
        recording.manifest.scene_id
    ));
    out.push_str(&format!(
        "        string started_ns = \"{}\"\n",
        recording.manifest.started_ns
    ));
    out.push_str(&format!(
        "        string stopped_ns = \"{}\"\n",
        recording.manifest.stopped_ns
    ));
    out.push_str("    }\n)\n");
}

fn write_object_prim(out: &mut String, object_id: &ObjectId, object: &ObjectData) {
    let tracks = &object.tracks;
    out.push('\n');
    out.push_str(&format!(
        "def Xform \"{}\" (\n",
        prim_name_for_object(object_id)
    ));
    out.push_str("    customData = {\n");
    out.push_str(&format!(
        "        string motionstage_object_id = \"{object_id}\"\n"
    ));
    if object.skipped_mismatched_samples > 0 {
        out.push_str(&format!(
            "        int skipped_mismatched_samples = {}\n",
            object.skipped_mismatched_samples
        ));
    }
    out.push_str("    }\n)\n{\n");

    let mut wrote_body = false;
    let mut ops = Vec::new();

    let xform_ops: [(Slot, &str, &str); 3] = [
        (Slot::Translate, "double3", "xformOp:translate"),
        (Slot::Orient, "quatf", "xformOp:orient"),
        (Slot::Scale, "float3", "xformOp:scale"),
    ];
    for (slot, usd_type, op_name) in xform_ops {
        // At most one track can claim each xformOp slot.
        let Some(track) = tracks.values().find(|track| track.slot == slot) else {
            continue;
        };
        if track.samples.is_empty() {
            continue;
        }
        write_time_samples(out, 1, usd_type, op_name, &track.samples);
        ops.push(op_name);
        wrote_body = true;
    }

    if !ops.is_empty() {
        let listed = ops
            .iter()
            .map(|op| format!("\"{op}\""))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("    uniform token[] xformOpOrder = [{listed}]\n"));
    }

    // Sanitized names are deduped per prim: the first attribute (in sorted
    // recorded-name order) keeps the plain sanitized identifier, later
    // collisions get a deterministic `_2`, `_3`, ... suffix and record their
    // original name in attribute customData.
    let mut used_names: BTreeSet<String> = BTreeSet::new();
    for (name, track) in tracks {
        if track.slot != Slot::Custom {
            continue;
        }
        let base = sanitize_identifier(name);
        let mut usd_name = base.clone();
        let mut counter = 1u32;
        while used_names.contains(&usd_name) {
            counter += 1;
            usd_name = format!("{base}_{counter}");
        }
        used_names.insert(usd_name.clone());
        if usd_name != base {
            out.push_str(&format!(
                "    custom {} {} (\n        customData = {{\n            string motionstage_attribute = \"{}\"\n        }}\n    )\n",
                track.usd_type,
                usd_name,
                escape_string(name)
            ));
        }
        write_time_samples(
            out,
            1,
            &format!("custom {}", track.usd_type),
            &usd_name,
            &track.samples,
        );
        wrote_body = true;
    }

    // At most one track can claim each Camera slot.
    let mut camera: BTreeMap<&'static str, &[(f64, String)]> = BTreeMap::new();
    for track in tracks.values() {
        if let Slot::Camera(camera_attr) = track.slot {
            camera.insert(camera_attr, &track.samples);
        }
    }
    if !camera.is_empty() {
        if wrote_body {
            out.push('\n');
        }
        out.push_str("    def Camera \"shape\"\n    {\n");
        for (camera_attr, samples) in &camera {
            write_time_samples(out, 2, "float", camera_attr, samples);
        }
        out.push_str("    }\n");
    }

    out.push_str("}\n");
}

fn write_time_samples(
    out: &mut String,
    indent_level: usize,
    usd_type: &str,
    name: &str,
    samples: &[(f64, String)],
) {
    let indent = "    ".repeat(indent_level);
    out.push_str(&format!("{indent}{usd_type} {name}.timeSamples = {{\n"));
    for (time_code, value) in samples {
        out.push_str(&format!("{indent}    {}: {value},\n", fmt_f64(*time_code)));
    }
    out.push_str(&format!("{indent}}}\n"));
}

fn write_markers_scope(out: &mut String, markers: &[RecordingMarker]) {
    out.push_str("\ndef Scope \"markers\"\n{\n");
    for (index, marker) in markers.iter().enumerate() {
        out.push_str(&format!(
            "    custom string m_{index:04} = \"{}\"\n",
            escape_string(&summarize_marker(marker))
        ));
    }
    out.push_str("}\n");
}

/// Compact human-readable label for a composite `Mode`.
fn mode_label(mode: &motionstage_protocol::Mode) -> &'static str {
    use motionstage_protocol::Mode;
    match *mode {
        Mode::IDLE => "idle",
        Mode::LIVE => "live",
        Mode::RECORDING => "recording",
        Mode::PLAYBACK => "playback",
        _ => "unknown",
    }
}

fn summarize_marker(marker: &RecordingMarker) -> String {
    match marker {
        RecordingMarker::ModeTransition {
            timestamp_ns,
            from,
            to,
        } => format!(
            "ModeTransition timestamp_ns={timestamp_ns} from={} to={}",
            mode_label(from),
            mode_label(to)
        ),
        RecordingMarker::MappingCreated {
            timestamp_ns,
            mapping_id,
            ..
        } => format!("MappingCreated timestamp_ns={timestamp_ns} mapping_id={mapping_id}"),
        RecordingMarker::MappingUpdated {
            timestamp_ns,
            mapping_id,
            ..
        } => format!("MappingUpdated timestamp_ns={timestamp_ns} mapping_id={mapping_id}"),
        RecordingMarker::MappingRemoved {
            timestamp_ns,
            mapping_id,
        } => format!("MappingRemoved timestamp_ns={timestamp_ns} mapping_id={mapping_id}"),
        RecordingMarker::MappingLockSet {
            timestamp_ns,
            mapping_id,
            lock,
        } => {
            format!("MappingLockSet timestamp_ns={timestamp_ns} mapping_id={mapping_id} lock={lock}")
        }
        RecordingMarker::ClientJoined {
            timestamp_ns,
            device_id,
            device_name,
        } => format!(
            "ClientJoined timestamp_ns={timestamp_ns} device_id={device_id} device_name={device_name}"
        ),
        RecordingMarker::ClientLeft {
            timestamp_ns,
            device_id,
            reason,
        } => match reason {
            Some(reason) => format!(
                "ClientLeft timestamp_ns={timestamp_ns} device_id={device_id} reason={reason}"
            ),
            None => format!("ClientLeft timestamp_ns={timestamp_ns} device_id={device_id}"),
        },
    }
}

fn classify(attribute: &str, value: &AttributeValue) -> Slot {
    match (attribute, value) {
        ("position", AttributeValue::Vec3f(_)) => return Slot::Translate,
        ("rotation" | "orientation", AttributeValue::Quatf(_)) => return Slot::Orient,
        ("scale", AttributeValue::Vec3f(_)) => return Slot::Scale,
        _ => {}
    }

    if matches!(value, AttributeValue::Float32(_) | AttributeValue::Float64(_)) {
        // Exact-name matching only (case-insensitive, '.'/'-' normalized to
        // '_'): substring matching would hijack unrelated floats such as
        // "horizontal_aperture" or "autofocus_speed".
        let normalized = attribute.to_ascii_lowercase().replace(['.', '-'], "_");
        match normalized.as_str() {
            "focal_length" | "focallength" => return Slot::Camera("focalLength"),
            "focus_distance" | "focusdistance" => return Slot::Camera("focusDistance"),
            "fstop" | "f_stop" | "aperture" => return Slot::Camera("fStop"),
            _ => {}
        }
    }

    Slot::Custom
}

fn encode_for_slot(slot: Slot, value: &AttributeValue) -> Option<String> {
    match (slot, value) {
        (Slot::Translate | Slot::Scale, AttributeValue::Vec3f(v)) => Some(encode_vec3(v)),
        (Slot::Orient, AttributeValue::Quatf(v)) => Some(encode_quat(v)),
        (Slot::Camera(_), AttributeValue::Float32(v)) => Some(fmt_f32(*v)),
        (Slot::Camera(_), AttributeValue::Float64(v)) => Some(fmt_f64(*v)),
        (Slot::Custom, value) => Some(encode_custom_value(value)),
        _ => None,
    }
}

fn usd_type_for(value: &AttributeValue) -> &'static str {
    match value {
        AttributeValue::Bool(_) | AttributeValue::Trigger(_) => "bool",
        AttributeValue::Int32(_) => "int",
        AttributeValue::Float32(_) => "float",
        AttributeValue::Float64(_) => "double",
        AttributeValue::Vec2f(_) => "float2",
        AttributeValue::Vec3f(_) => "float3",
        AttributeValue::Vec4f(_) => "float4",
        AttributeValue::Quatf(_) => "quatf",
        AttributeValue::Mat4f(_) => "matrix4d",
    }
}

fn encode_custom_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::Bool(v) | AttributeValue::Trigger(v) => v.to_string(),
        AttributeValue::Int32(v) => v.to_string(),
        AttributeValue::Float32(v) => fmt_f32(*v),
        AttributeValue::Float64(v) => fmt_f64(*v),
        AttributeValue::Vec2f(v) => format!("({}, {})", fmt_f32(v[0]), fmt_f32(v[1])),
        AttributeValue::Vec3f(v) => encode_vec3(v),
        AttributeValue::Vec4f(v) => format!(
            "({}, {}, {}, {})",
            fmt_f32(v[0]),
            fmt_f32(v[1]),
            fmt_f32(v[2]),
            fmt_f32(v[3])
        ),
        AttributeValue::Quatf(v) => encode_quat(v),
        AttributeValue::Mat4f(v) => {
            let rows = v
                .iter()
                .map(|row| {
                    format!(
                        "({}, {}, {}, {})",
                        fmt_f32(row[0]),
                        fmt_f32(row[1]),
                        fmt_f32(row[2]),
                        fmt_f32(row[3])
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("( {rows} )")
        }
    }
}

fn encode_vec3(v: &[f32; 3]) -> String {
    format!("({}, {}, {})", fmt_f32(v[0]), fmt_f32(v[1]), fmt_f32(v[2]))
}

/// `Quatf` stores `[x, y, z, w]`; USD text form is `(w, x, y, z)`.
fn encode_quat(v: &[f32; 4]) -> String {
    format!(
        "({}, {}, {}, {})",
        fmt_f32(v[3]),
        fmt_f32(v[0]),
        fmt_f32(v[1]),
        fmt_f32(v[2])
    )
}

fn time_code(timestamp_ns: u64, started_ns: u64, tcps: u32) -> f64 {
    let delta_ns = timestamp_ns as i128 - started_ns as i128;
    delta_ns as f64 * tcps as f64 / 1e9
}

/// Shortest locale-independent representation that round-trips through f32.
fn fmt_f32(v: f32) -> String {
    if v.is_nan() {
        "nan".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        format!("{v}")
    }
}

/// Shortest locale-independent representation that round-trips through f64.
fn fmt_f64(v: f64) -> String {
    if v.is_nan() {
        "nan".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        format!("{v}")
    }
}

/// Sanitize an attribute name into a USD-legal identifier.
fn sanitize_identifier(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if out.is_empty() || out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    out
}

fn escape_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use motionstage_core::AttributeValue;
    use motionstage_protocol::Mode;
    use motionstage_recording::{
        RecordedAttribute, RecordedFrame, RecordingFile, RecordingFormatVersion, RecordingManifest,
        RecordingMarker,
    };
    use uuid::Uuid;

    use crate::{export, export_with_options, prim_name_for_object, UsdExportOptions};

    fn recording_with_frames(frames: Vec<RecordedFrame>) -> RecordingFile {
        let stopped_ns = frames.last().map(|f| f.timestamp_ns).unwrap_or_default();
        RecordingFile {
            manifest: RecordingManifest {
                recording_id: Uuid::nil(),
                scene_id: Uuid::nil(),
                started_ns: 0,
                stopped_ns,
                frame_count: frames.len() as u64,
            },
            markers: Vec::new(),
            frames,
            version: RecordingFormatVersion::V2,
        }
    }

    fn camera_take() -> RecordingFile {
        let object_id = Uuid::nil();
        let frames = (0..3u64)
            .map(|i| RecordedFrame {
                // 25 ms per frame = exactly 3 timeCodes at 120 tcps.
                timestamp_ns: i * 25_000_000,
                mode: Mode::RECORDING,
                attributes: vec![
                    RecordedAttribute {
                        object_id,
                        attribute: "position".into(),
                        value: AttributeValue::Vec3f([0.25 * i as f32, 0.0, 1.5]),
                    },
                    RecordedAttribute {
                        object_id,
                        attribute: "rotation".into(),
                        // [x, y, z, w]: identity, then rotations about Z.
                        value: match i {
                            0 => AttributeValue::Quatf([0.0, 0.0, 0.0, 1.0]),
                            1 => AttributeValue::Quatf([0.0, 0.0, 0.5, 0.75]),
                            _ => AttributeValue::Quatf([0.0, 0.0, 0.75, 0.5]),
                        },
                    },
                    RecordedAttribute {
                        object_id,
                        attribute: "focal_length".into(),
                        value: AttributeValue::Float32(35.0 + 7.5 * i as f32),
                    },
                ],
            })
            .collect();
        recording_with_frames(frames)
    }

    #[test]
    fn exporter_is_deterministic() {
        let recording = camera_take();
        let a = export(&recording);
        let b = export(&recording);
        assert_eq!(a, b);
    }

    #[test]
    fn golden_camera_take() {
        let expected = "\
#usda 1.0
(
    upAxis = \"Z\"
    metersPerUnit = 1
    timeCodesPerSecond = 120
    startTimeCode = 0
    endTimeCode = 6
    customLayerData = {
        string recording_id = \"00000000-0000-0000-0000-000000000000\"
        string scene_id = \"00000000-0000-0000-0000-000000000000\"
        string started_ns = \"0\"
        string stopped_ns = \"50000000\"
    }
)

def Xform \"o_00000000_0000_0000_0000_000000000000\" (
    customData = {
        string motionstage_object_id = \"00000000-0000-0000-0000-000000000000\"
    }
)
{
    double3 xformOp:translate.timeSamples = {
        0: (0, 0, 1.5),
        3: (0.25, 0, 1.5),
        6: (0.5, 0, 1.5),
    }
    quatf xformOp:orient.timeSamples = {
        0: (1, 0, 0, 0),
        3: (0.75, 0, 0, 0.5),
        6: (0.5, 0, 0, 0.75),
    }
    uniform token[] xformOpOrder = [\"xformOp:translate\", \"xformOp:orient\"]

    def Camera \"shape\"
    {
        float focalLength.timeSamples = {
            0: 35,
            3: 42.5,
            6: 50,
        }
    }
}
";
        assert_eq!(export(&camera_take()), expected);
    }

    #[test]
    fn time_codes_per_second_is_configurable() {
        let usda = export_with_options(
            &camera_take(),
            &UsdExportOptions {
                time_codes_per_second: 24,
                ..UsdExportOptions::default()
            },
        );
        assert!(usda.contains("timeCodesPerSecond = 24"));
        // 25 ms at 24 tcps = 0.6 timeCodes.
        assert!(usda.contains("0.6: (0.25, 0, 1.5),"));
    }

    #[test]
    fn scale_and_custom_attributes_are_typed_and_sampled() {
        let object_id = Uuid::nil();
        let recording = recording_with_frames(vec![RecordedFrame {
            timestamp_ns: 0,
            mode: Mode::RECORDING,
            attributes: vec![
                RecordedAttribute {
                    object_id,
                    attribute: "scale".into(),
                    value: AttributeValue::Vec3f([2.0, 2.0, 2.0]),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "shutter.open".into(),
                    value: AttributeValue::Trigger(true),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "iso".into(),
                    value: AttributeValue::Int32(800),
                },
            ],
        }]);
        let usda = export(&recording);
        assert!(usda.contains("float3 xformOp:scale.timeSamples = {"));
        assert!(usda.contains("uniform token[] xformOpOrder = [\"xformOp:scale\"]"));
        assert!(usda.contains("custom bool shutter_open.timeSamples = {"));
        assert!(usda.contains("custom int iso.timeSamples = {"));
        assert!(usda.contains("        0: true,"));
        assert!(usda.contains("        0: 800,"));
    }

    #[test]
    fn markers_are_summarized_in_a_scope() {
        let mut recording = recording_with_frames(Vec::new());
        recording.markers.push(RecordingMarker::ModeTransition {
            timestamp_ns: 100,
            from: Mode::LIVE,
            to: Mode::RECORDING,
        });
        let usda = export(&recording);
        assert!(usda.contains("def Scope \"markers\""));
        assert!(usda.contains(
            "custom string m_0000 = \"ModeTransition timestamp_ns=100 from=live to=recording\""
        ));
    }

    fn single_attr_frame(
        timestamp_ns: u64,
        attribute: &str,
        value: AttributeValue,
    ) -> RecordedFrame {
        RecordedFrame {
            timestamp_ns,
            mode: Mode::RECORDING,
            attributes: vec![RecordedAttribute {
                object_id: Uuid::nil(),
                attribute: attribute.into(),
                value,
            }],
        }
    }

    #[test]
    fn type_changing_track_locks_type_from_first_sample_and_notes_skips() {
        let recording = recording_with_frames(vec![
            single_attr_frame(0, "mystery", AttributeValue::Float32(1.0)),
            // Variant changes mid-track: must not be authored.
            single_attr_frame(25_000_000, "mystery", AttributeValue::Vec3f([1.0, 2.0, 3.0])),
            single_attr_frame(50_000_000, "mystery", AttributeValue::Float32(3.0)),
        ]);
        let usda = export(&recording);
        assert!(usda.contains("custom float mystery.timeSamples = {"));
        assert!(usda.contains("        0: 1,"));
        assert!(usda.contains("        6: 3,"));
        // The mismatched Vec3f sample is skipped, not authored.
        assert!(!usda.contains("(1, 2, 3)"));
        assert!(!usda.contains("custom float3 mystery"));
        // And the prim records how many samples were dropped.
        assert!(usda.contains("        int skipped_mismatched_samples = 1\n"));
    }

    #[test]
    fn type_stable_tracks_emit_no_skip_note() {
        let usda = export(&camera_take());
        assert!(!usda.contains("skipped_mismatched_samples"));
    }

    #[test]
    fn colliding_sanitized_names_get_deterministic_suffixes() {
        let object_id = Uuid::nil();
        let recording = recording_with_frames(vec![RecordedFrame {
            timestamp_ns: 0,
            mode: Mode::RECORDING,
            attributes: vec![
                RecordedAttribute {
                    object_id,
                    attribute: "shutter_open".into(),
                    value: AttributeValue::Bool(false),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "shutter.open".into(),
                    value: AttributeValue::Bool(true),
                },
            ],
        }]);
        let usda = export(&recording);
        // Sorted recorded-name order: "shutter.open" < "shutter_open", so
        // "shutter.open" keeps the plain sanitized name and "shutter_open"
        // gets the `_2` suffix with its original name preserved.
        assert!(usda.contains("custom bool shutter_open.timeSamples = {"));
        assert!(usda.contains("custom bool shutter_open_2.timeSamples = {"));
        assert!(usda.contains("string motionstage_attribute = \"shutter_open\""));
        // Exactly one declaration per identifier (no duplicate timeSamples
        // blocks for the same name).
        assert_eq!(usda.matches("custom bool shutter_open.timeSamples").count(), 1);
        assert_eq!(
            usda.matches("custom bool shutter_open_2.timeSamples").count(),
            1
        );
        // Deterministic output.
        assert_eq!(usda, export(&recording));
    }

    #[test]
    fn second_claimant_of_orient_slot_falls_back_to_custom() {
        let object_id = Uuid::nil();
        let recording = recording_with_frames(vec![RecordedFrame {
            timestamp_ns: 0,
            mode: Mode::RECORDING,
            attributes: vec![
                RecordedAttribute {
                    object_id,
                    attribute: "rotation".into(),
                    value: AttributeValue::Quatf([0.0, 0.0, 0.5, 0.75]),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "orientation".into(),
                    value: AttributeValue::Quatf([0.0, 0.0, 0.0, 1.0]),
                },
            ],
        }]);
        let usda = export(&recording);
        // "orientation" sorts first and claims xformOp:orient.
        assert_eq!(usda.matches("quatf xformOp:orient.timeSamples").count(), 1);
        assert!(usda.contains("quatf xformOp:orient.timeSamples = {\n        0: (1, 0, 0, 0),"));
        // "rotation" falls back to a custom attribute instead of merging
        // duplicate timeCodes into the claimed slot.
        assert!(usda.contains("custom quatf rotation.timeSamples = {"));
        assert!(usda.contains("        0: (0.75, 0, 0, 0.5),"));
    }

    #[test]
    fn out_of_order_and_duplicate_timestamps_produce_sorted_unique_time_codes() {
        let recording = recording_with_frames(vec![
            single_attr_frame(50_000_000, "iso", AttributeValue::Int32(400)),
            single_attr_frame(0, "iso", AttributeValue::Int32(100)),
            single_attr_frame(25_000_000, "iso", AttributeValue::Int32(200)),
            // Duplicate of the first timestamp arriving later: last wins.
            single_attr_frame(50_000_000, "iso", AttributeValue::Int32(800)),
        ]);
        let usda = export(&recording);
        assert!(usda.contains(
            "custom int iso.timeSamples = {\n        0: 100,\n        3: 200,\n        6: 800,\n    }"
        ));
        assert_eq!(usda.matches("6: ").count(), 1, "duplicate timeCode keys: {usda}");
    }

    #[test]
    fn camera_classification_requires_exact_names() {
        let object_id = Uuid::nil();
        let recording = recording_with_frames(vec![RecordedFrame {
            timestamp_ns: 0,
            mode: Mode::RECORDING,
            attributes: vec![
                RecordedAttribute {
                    object_id,
                    // Substring match on "aperture" used to hijack this into
                    // fStop; it must stay a custom attribute.
                    attribute: "horizontal_aperture".into(),
                    value: AttributeValue::Float32(36.0),
                },
                RecordedAttribute {
                    object_id,
                    // Substring match on "focus" used to hijack this too.
                    attribute: "autofocus_speed".into(),
                    value: AttributeValue::Float32(0.5),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "F-Stop".into(),
                    value: AttributeValue::Float32(2.8),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "focus.distance".into(),
                    value: AttributeValue::Float64(3.25),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "FocalLength".into(),
                    value: AttributeValue::Float32(50.0),
                },
            ],
        }]);
        let usda = export(&recording);
        // Exact matches (after case/'.'/'-' normalization) route to Camera.
        assert!(usda.contains("float fStop.timeSamples = {\n            0: 2.8,"));
        assert!(usda.contains("float focusDistance.timeSamples = {\n            0: 3.25,"));
        assert!(usda.contains("float focalLength.timeSamples = {\n            0: 50,"));
        // Non-exact names stay on the Xform prim as custom attributes.
        assert!(usda.contains("custom float horizontal_aperture.timeSamples = {"));
        assert!(usda.contains("custom float autofocus_speed.timeSamples = {"));
        assert!(!usda.contains("float fStop.timeSamples = {\n            0: 36,"));
        assert!(!usda.contains("float focusDistance.timeSamples = {\n            0: 0.5,"));
    }

    #[test]
    fn second_claimant_of_camera_slot_falls_back_to_custom() {
        let object_id = Uuid::nil();
        let recording = recording_with_frames(vec![RecordedFrame {
            timestamp_ns: 0,
            mode: Mode::RECORDING,
            attributes: vec![
                RecordedAttribute {
                    object_id,
                    attribute: "fstop".into(),
                    value: AttributeValue::Float32(2.8),
                },
                RecordedAttribute {
                    object_id,
                    attribute: "aperture".into(),
                    value: AttributeValue::Float32(4.0),
                },
            ],
        }]);
        let usda = export(&recording);
        // "aperture" sorts first and claims fStop; "fstop" becomes custom.
        assert_eq!(usda.matches("float fStop.timeSamples").count(), 1);
        assert!(usda.contains("float fStop.timeSamples = {\n            0: 4,"));
        assert!(usda.contains("custom float fstop.timeSamples = {\n        0: 2.8,"));
    }

    #[test]
    fn prim_names_are_usd_legal_for_arbitrary_uuids() {
        let uuids = [
            Uuid::nil(),
            Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap(),
            Uuid::now_v7(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        for uuid in uuids {
            let name = prim_name_for_object(&uuid);
            let mut chars = name.chars();
            let first = chars.next().expect("prim name is non-empty");
            assert!(
                first.is_ascii_alphabetic() || first == '_',
                "illegal first char in {name}"
            );
            assert!(
                chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "illegal char in {name}"
            );
        }
    }
}
