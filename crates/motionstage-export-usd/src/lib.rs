use std::collections::BTreeMap;
use std::fmt::Write;

use motionstage_core::AttributeValue;
use motionstage_recording::RecordingFile;
use uuid::Uuid;

/// Up-axis convention for the exported USD layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpAxis {
    Y,
    Z,
}

/// Configuration for USD export.
#[derive(Debug, Clone)]
pub struct UsdExportConfig {
    pub fps: f64,
    pub up_axis: UpAxis,
}

impl Default for UsdExportConfig {
    fn default() -> Self {
        Self {
            fps: 24.0,
            up_axis: UpAxis::Y,
        }
    }
}

/// Export a recording as schema-compliant USDA text with typed time-sampled properties.
pub fn export(recording: &RecordingFile, config: &UsdExportConfig) -> String {
    let started_ns = recording.manifest.started_ns;

    // Phase A — Build property index:
    // Key: (object_id, attribute_name) → BTreeMap<frame_number, value>
    let mut property_index: BTreeMap<(Uuid, String), BTreeMap<u64, AttributeValue>> =
        BTreeMap::new();

    for frame in &recording.frames {
        for attr in &frame.attributes {
            let frame_num = timestamp_to_frame(frame.timestamp_ns, started_ns, config.fps);
            property_index
                .entry((attr.object_id, attr.attribute.clone()))
                .or_default()
                .insert(frame_num, attr.value.clone());
        }
    }

    // Phase B — Group by object, classify attributes
    // Collect unique object IDs in order
    let mut object_ids: Vec<Uuid> = Vec::new();
    for (obj_id, _attr_name) in property_index.keys() {
        if !object_ids.contains(obj_id) {
            object_ids.push(*obj_id);
        }
    }

    struct PrimProperty {
        usd_name: String,
        usd_type: String,
        samples: BTreeMap<u64, AttributeValue>,
    }

    struct PrimInfo {
        prim_name: String,
        xform_ops: Vec<PrimProperty>,   // translate, orient
        custom_props: Vec<PrimProperty>, // everything else
    }

    let mut prims: Vec<PrimInfo> = Vec::new();

    for &obj_id in &object_ids {
        let hex = format!("{}", obj_id.as_simple());
        let prim_name = format!("Object_{}", &hex[..8]);

        let mut xform_ops = Vec::new();
        let mut custom_props = Vec::new();

        // Collect all properties for this object
        let obj_props: Vec<(&String, &BTreeMap<u64, AttributeValue>)> = property_index
            .iter()
            .filter(|((id, _), _)| *id == obj_id)
            .map(|((_, name), samples)| (name, samples))
            .collect();

        for (attr_name, samples) in obj_props {
            let leaf = attribute_leaf(attr_name);
            // Need a representative value for type mapping
            let representative = samples.values().next().unwrap();

            if is_translate(leaf) {
                xform_ops.push(PrimProperty {
                    usd_name: "xformOp:translate".into(),
                    usd_type: usd_type_name(representative).into(),
                    samples: samples.clone(),
                });
            } else if is_orient(leaf) {
                xform_ops.push(PrimProperty {
                    usd_name: "xformOp:orient".into(),
                    usd_type: usd_type_name(representative).into(),
                    samples: samples.clone(),
                });
            } else {
                custom_props.push(PrimProperty {
                    usd_name: sanitize_property_name(attr_name),
                    usd_type: usd_type_name(representative).into(),
                    samples: samples.clone(),
                });
            }
        }

        prims.push(PrimInfo {
            prim_name,
            xform_ops,
            custom_props,
        });
    }

    // Phase C — Emit USDA text
    let end_frame = property_index
        .values()
        .flat_map(|samples| samples.keys())
        .copied()
        .max()
        .unwrap_or(0);

    let up_axis_str = match config.up_axis {
        UpAxis::Y => "Y",
        UpAxis::Z => "Z",
    };

    let mut out = String::new();
    writeln!(out, "#usda 1.0").unwrap();
    writeln!(out, "(").unwrap();
    writeln!(out, "    defaultPrim = \"MotionStageTake\"").unwrap();
    writeln!(out, "    upAxis = \"{}\"", up_axis_str).unwrap();
    writeln!(out, "    startTimeCode = 0").unwrap();
    writeln!(out, "    endTimeCode = {end_frame}").unwrap();
    writeln!(out, "    timeCodesPerSecond = {}", format_f64(config.fps)).unwrap();
    writeln!(out, ")").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "def Xform \"MotionStageTake\" (").unwrap();
    writeln!(out, "    customData = {{").unwrap();
    writeln!(
        out,
        "        string recording_id = \"{}\"",
        recording.manifest.recording_id
    )
    .unwrap();
    writeln!(
        out,
        "        string scene_id = \"{}\"",
        recording.manifest.scene_id
    )
    .unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, ") {{").unwrap();

    for prim in &prims {
        writeln!(out, "    def Xform \"{}\" {{", prim.prim_name).unwrap();

        // Emit xform op properties
        for prop in &prim.xform_ops {
            write_time_sampled_property(&mut out, &prop.usd_type, &prop.usd_name, &prop.samples, 8);
        }

        // Emit xformOpOrder if there are xform ops
        if !prim.xform_ops.is_empty() {
            let order: Vec<String> = prim
                .xform_ops
                .iter()
                .map(|p| format!("\"{}\"", p.usd_name))
                .collect();
            writeln!(out, "        token[] xformOpOrder = [{}]", order.join(", ")).unwrap();
        }

        // Emit custom properties
        if !prim.xform_ops.is_empty() && !prim.custom_props.is_empty() {
            writeln!(out).unwrap();
        }
        for prop in &prim.custom_props {
            write_time_sampled_property(&mut out, &prop.usd_type, &prop.usd_name, &prop.samples, 8);
        }

        writeln!(out, "    }}").unwrap();
    }

    writeln!(out, "}}").unwrap();
    out
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn timestamp_to_frame(timestamp_ns: u64, started_ns: u64, fps: f64) -> u64 {
    let elapsed_ns = timestamp_ns.saturating_sub(started_ns);
    (elapsed_ns as f64 * fps / 1_000_000_000.0).round() as u64
}

fn attribute_leaf(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn is_translate(leaf: &str) -> bool {
    leaf == "position"
}

fn is_orient(leaf: &str) -> bool {
    leaf == "rotation"
}

fn sanitize_property_name(name: &str) -> String {
    name.replace('.', ":")
}

fn usd_type_name(value: &AttributeValue) -> &'static str {
    match value {
        AttributeValue::Bool(_) => "bool",
        AttributeValue::Int32(_) => "int",
        AttributeValue::Float32(_) => "float",
        AttributeValue::Float64(_) => "double",
        AttributeValue::Vec2f(_) => "float2",
        AttributeValue::Vec3f(_) => "float3",
        AttributeValue::Vec4f(_) => "float4",
        AttributeValue::Quatf(_) => "quatf",
        AttributeValue::Mat4f(_) => "matrix4d",
        AttributeValue::Trigger(_) => "bool",
    }
}

fn format_float(v: f32) -> String {
    let s = v.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

fn format_f64(v: f64) -> String {
    let s = v.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

fn format_usd_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::Bool(v) | AttributeValue::Trigger(v) => {
            if *v { "true".into() } else { "false".into() }
        }
        AttributeValue::Int32(v) => v.to_string(),
        AttributeValue::Float32(v) => format_float(*v),
        AttributeValue::Float64(v) => format_f64(*v),
        AttributeValue::Vec2f(v) => {
            format!("({}, {})", format_float(v[0]), format_float(v[1]))
        }
        AttributeValue::Vec3f(v) => {
            format!(
                "({}, {}, {})",
                format_float(v[0]),
                format_float(v[1]),
                format_float(v[2])
            )
        }
        AttributeValue::Vec4f(v) => {
            format!(
                "({}, {}, {}, {})",
                format_float(v[0]),
                format_float(v[1]),
                format_float(v[2]),
                format_float(v[3])
            )
        }
        AttributeValue::Quatf(v) => {
            // MotionStage: [x, y, z, w] → USD: (w, x, y, z)
            format!(
                "({}, {}, {}, {})",
                format_float(v[3]),
                format_float(v[0]),
                format_float(v[1]),
                format_float(v[2])
            )
        }
        AttributeValue::Mat4f(v) => {
            let rows: Vec<String> = v
                .iter()
                .map(|row| {
                    let cols: Vec<String> = row.iter().map(|x| format_float(*x)).collect();
                    format!("({})", cols.join(", "))
                })
                .collect();
            format!("({})", rows.join(", "))
        }
    }
}

fn write_time_sampled_property(
    out: &mut String,
    usd_type: &str,
    usd_name: &str,
    samples: &BTreeMap<u64, AttributeValue>,
    indent: usize,
) {
    let pad = " ".repeat(indent);
    writeln!(out, "{pad}{usd_type} {usd_name}.timeSamples = {{").unwrap();
    for (frame, value) in samples {
        writeln!(out, "{pad}    {frame}: {},", format_usd_value(value)).unwrap();
    }
    writeln!(out, "{pad}}}").unwrap();
}

#[cfg(test)]
mod tests {
    use motionstage_core::AttributeValue;
    use motionstage_protocol::Mode;
    use motionstage_recording::{
        RecordedAttribute, RecordedFrame, RecordingFile, RecordingFormatVersion, RecordingManifest,
    };
    use uuid::Uuid;

    use crate::{export, UsdExportConfig, UpAxis};

    fn make_recording(frames: Vec<RecordedFrame>) -> RecordingFile {
        let started_ns = frames.first().map(|f| f.timestamp_ns).unwrap_or(0);
        let stopped_ns = frames.last().map(|f| f.timestamp_ns).unwrap_or(0);
        RecordingFile {
            manifest: RecordingManifest {
                recording_id: Uuid::nil(),
                scene_id: Uuid::nil(),
                started_ns,
                stopped_ns,
                frame_count: frames.len() as u64,
            },
            markers: Vec::new(),
            frames,
            version: RecordingFormatVersion::V2,
        }
    }

    fn frame_at(
        timestamp_ns: u64,
        attributes: Vec<RecordedAttribute>,
    ) -> RecordedFrame {
        RecordedFrame {
            timestamp_ns,
            mode: Mode::RECORDING,
            attributes,
        }
    }

    fn attr(object_id: Uuid, name: &str, value: AttributeValue) -> RecordedAttribute {
        RecordedAttribute {
            object_id,
            attribute: name.into(),
            value,
        }
    }

    #[test]
    fn exporter_is_deterministic() {
        let obj = Uuid::nil();
        let recording = make_recording(vec![frame_at(
            0,
            vec![attr(obj, "position", AttributeValue::Vec3f([1.0, 2.0, 3.0]))],
        )]);
        let config = UsdExportConfig::default();
        let a = export(&recording, &config);
        let b = export(&recording, &config);
        assert_eq!(a, b);
    }

    #[test]
    fn layer_metadata_includes_time_codes() {
        let recording = make_recording(vec![frame_at(
            0,
            vec![attr(
                Uuid::nil(),
                "position",
                AttributeValue::Vec3f([0.0, 0.0, 0.0]),
            )],
        )]);
        let config = UsdExportConfig::default();
        let out = export(&recording, &config);
        assert!(out.contains("startTimeCode = 0"));
        assert!(out.contains("endTimeCode = 0"));
        assert!(out.contains("timeCodesPerSecond = 24.0"));
    }

    #[test]
    fn single_position_produces_xform_translate() {
        let obj = Uuid::nil();
        let recording = make_recording(vec![frame_at(
            0,
            vec![attr(obj, "position", AttributeValue::Vec3f([1.0, 2.0, 3.0]))],
        )]);
        let out = export(&recording, &UsdExportConfig::default());
        assert!(out.contains("float3 xformOp:translate.timeSamples"));
        assert!(out.contains("0: (1.0, 2.0, 3.0)"));
    }

    #[test]
    fn multi_frame_multi_attribute() {
        let obj_a = Uuid::from_u128(0xAAAA);
        let obj_b = Uuid::from_u128(0xBBBB);
        // 3 frames at 24fps: 0ns, 41_666_667ns (~1 frame), 83_333_333ns (~2 frames)
        let ns_per_frame = 1_000_000_000u64 / 24;
        let recording = make_recording(vec![
            frame_at(
                0,
                vec![
                    attr(obj_a, "position", AttributeValue::Vec3f([1.0, 0.0, 0.0])),
                    attr(
                        obj_a,
                        "rotation",
                        AttributeValue::Quatf([0.0, 0.0, 0.0, 1.0]),
                    ),
                    attr(obj_b, "position", AttributeValue::Vec3f([5.0, 5.0, 5.0])),
                ],
            ),
            frame_at(
                ns_per_frame,
                vec![
                    attr(obj_a, "position", AttributeValue::Vec3f([2.0, 0.0, 0.0])),
                    attr(obj_b, "position", AttributeValue::Vec3f([6.0, 6.0, 6.0])),
                ],
            ),
            frame_at(
                ns_per_frame * 2,
                vec![attr(
                    obj_a,
                    "position",
                    AttributeValue::Vec3f([3.0, 0.0, 0.0]),
                )],
            ),
        ]);
        let out = export(&recording, &UsdExportConfig::default());
        // Object A should have translate with frames 0,1,2 and orient with frame 0
        assert!(out.contains("Object_0000000"));
        assert!(out.contains("xformOp:translate"));
        assert!(out.contains("xformOp:orient"));
        assert!(out.contains("xformOpOrder"));
        // Object B should have translate with frames 0,1
        assert!(out.contains("endTimeCode = 2"));
    }

    #[test]
    fn quaternion_component_order_is_wxyz() {
        let obj = Uuid::nil();
        let recording = make_recording(vec![frame_at(
            0,
            vec![attr(
                obj,
                "rotation",
                AttributeValue::Quatf([0.1, 0.2, 0.3, 0.9]),
            )],
        )]);
        let out = export(&recording, &UsdExportConfig::default());
        // USD: (w, x, y, z) = (0.9, 0.1, 0.2, 0.3)
        assert!(out.contains("(0.9, 0.1, 0.2, 0.3)"));
    }

    #[test]
    fn custom_properties_use_native_types() {
        let obj = Uuid::nil();
        let recording = make_recording(vec![frame_at(
            0,
            vec![attr(
                obj,
                "camera.focal_length",
                AttributeValue::Float32(35.0),
            )],
        )]);
        let out = export(&recording, &UsdExportConfig::default());
        assert!(out.contains("float camera:focal_length.timeSamples"));
        assert!(out.contains("35.0"));
    }

    #[test]
    fn fps_affects_frame_numbers() {
        let obj = Uuid::nil();
        // 1 second into recording
        let recording = make_recording(vec![
            frame_at(0, vec![attr(obj, "position", AttributeValue::Vec3f([0.0, 0.0, 0.0]))]),
            frame_at(
                1_000_000_000,
                vec![attr(obj, "position", AttributeValue::Vec3f([1.0, 0.0, 0.0]))],
            ),
        ]);
        let out_24 = export(&recording, &UsdExportConfig { fps: 24.0, up_axis: UpAxis::Y });
        let out_60 = export(&recording, &UsdExportConfig { fps: 60.0, up_axis: UpAxis::Y });
        // At 24fps, 1 second = frame 24
        assert!(out_24.contains("24:"));
        // At 60fps, 1 second = frame 60
        assert!(out_60.contains("60:"));
    }

    #[test]
    fn empty_recording_produces_valid_layer() {
        let recording = make_recording(vec![]);
        let out = export(&recording, &UsdExportConfig::default());
        assert!(out.contains("#usda 1.0"));
        assert!(out.contains("defaultPrim = \"MotionStageTake\""));
        assert!(out.contains("startTimeCode = 0"));
        assert!(out.contains("endTimeCode = 0"));
    }

    #[test]
    fn float_values_always_have_decimal_point() {
        let obj = Uuid::nil();
        let recording = make_recording(vec![frame_at(
            0,
            vec![attr(obj, "camera.focal_length", AttributeValue::Float32(35.0))],
        )]);
        let out = export(&recording, &UsdExportConfig::default());
        // Must be "35.0" not "35"
        assert!(out.contains("35.0"));
        assert!(!out.contains(": 35,"));
    }
}
