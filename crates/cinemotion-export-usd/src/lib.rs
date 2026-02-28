use cinemotion_core::AttributeValue;
use cinemotion_recording::RecordingFile;

pub fn export(recording: &RecordingFile) -> String {
    let mut out = String::new();
    out.push_str("#usda 1.0\n");
    out.push_str("def Xform \"CineMotionTake\" {\n");
    out.push_str(&format!(
        "    custom string recording_id = \"{}\"\n",
        recording.manifest.recording_id
    ));
    out.push_str("    def Scope \"Frames\" {\n");

    for frame in &recording.frames {
        out.push_str(&format!(
            "        def Scope \"f_{}\" {{\n",
            frame.timestamp_ns
        ));
        for attr in &frame.attributes {
            out.push_str(&format!(
                "            custom string o{}_{} = \"{}\"\n",
                attr.object_id,
                attr.attribute,
                encode_value(&attr.value)
            ));
        }
        out.push_str("        }\n");
    }

    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

fn encode_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::Bool(v) => v.to_string(),
        AttributeValue::Int32(v) => v.to_string(),
        AttributeValue::Float32(v) => v.to_string(),
        AttributeValue::Float64(v) => v.to_string(),
        AttributeValue::Vec2f(v) => format!("{} {}", v[0], v[1]),
        AttributeValue::Vec3f(v) => format!("{} {} {}", v[0], v[1], v[2]),
        AttributeValue::Vec4f(v) => format!("{} {} {} {}", v[0], v[1], v[2], v[3]),
        AttributeValue::Quatf(v) => format!("{} {} {} {}", v[0], v[1], v[2], v[3]),
        AttributeValue::Mat4f(v) => format!(
            "{}",
            v.iter()
                .flat_map(|row| row.iter())
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ),
        AttributeValue::Trigger(v) => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use cinemotion_core::AttributeValue;
    use cinemotion_protocol::Mode;
    use cinemotion_recording::{
        RecordedAttribute, RecordedFrame, RecordingFile, RecordingFormatVersion, RecordingManifest,
    };
    use uuid::Uuid;

    use crate::export;

    #[test]
    fn exporter_is_deterministic() {
        let recording = RecordingFile {
            manifest: RecordingManifest {
                recording_id: Uuid::nil(),
                scene_id: Uuid::nil(),
                started_ns: 1,
                stopped_ns: 2,
                frame_count: 1,
            },
            markers: Vec::new(),
            frames: vec![RecordedFrame {
                timestamp_ns: 1,
                mode: Mode::Recording,
                attributes: vec![RecordedAttribute {
                    object_id: Uuid::nil(),
                    attribute: "position".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
            }],
            version: RecordingFormatVersion::V2,
        };
        let a = export(&recording);
        let b = export(&recording);
        assert_eq!(a, b);
    }
}
