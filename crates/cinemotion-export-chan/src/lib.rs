use cinemotion_core::AttributeValue;
use cinemotion_recording::RecordingFile;

pub fn export(recording: &RecordingFile) -> String {
    let mut lines = Vec::new();
    for frame in &recording.frames {
        for attr in &frame.attributes {
            match &attr.value {
                AttributeValue::Vec3f(v) => {
                    lines.push(format!(
                        "{} {} {}.tx {}",
                        frame.timestamp_ns, attr.object_id, attr.attribute, v[0]
                    ));
                    lines.push(format!(
                        "{} {} {}.ty {}",
                        frame.timestamp_ns, attr.object_id, attr.attribute, v[1]
                    ));
                    lines.push(format!(
                        "{} {} {}.tz {}",
                        frame.timestamp_ns, attr.object_id, attr.attribute, v[2]
                    ));
                }
                AttributeValue::Quatf(v) => {
                    lines.push(format!(
                        "{} {} {}.qx {}",
                        frame.timestamp_ns, attr.object_id, attr.attribute, v[0]
                    ));
                    lines.push(format!(
                        "{} {} {}.qy {}",
                        frame.timestamp_ns, attr.object_id, attr.attribute, v[1]
                    ));
                    lines.push(format!(
                        "{} {} {}.qz {}",
                        frame.timestamp_ns, attr.object_id, attr.attribute, v[2]
                    ));
                    lines.push(format!(
                        "{} {} {}.qw {}",
                        frame.timestamp_ns, attr.object_id, attr.attribute, v[3]
                    ));
                }
                _ => lines.push(format!(
                    "{} {} {} {}",
                    frame.timestamp_ns,
                    attr.object_id,
                    attr.attribute,
                    encode_value(&attr.value)
                )),
            }
        }
    }
    lines.join("\n")
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
    fn chan_output_is_deterministic() {
        let recording = RecordingFile {
            manifest: RecordingManifest {
                recording_id: Uuid::nil(),
                scene_id: Uuid::nil(),
                started_ns: 0,
                stopped_ns: 1,
                frame_count: 1,
            },
            markers: Vec::new(),
            frames: vec![RecordedFrame {
                timestamp_ns: 0,
                mode: Mode::Recording,
                attributes: vec![RecordedAttribute {
                    object_id: Uuid::nil(),
                    attribute: "focal".into(),
                    value: AttributeValue::Float32(35.0),
                }],
            }],
            version: RecordingFormatVersion::V2,
        };
        assert_eq!(export(&recording), export(&recording));
    }

    #[test]
    fn vec3_is_emitted_as_channel_triplet() {
        let recording = RecordingFile {
            manifest: RecordingManifest {
                recording_id: Uuid::nil(),
                scene_id: Uuid::nil(),
                started_ns: 0,
                stopped_ns: 1,
                frame_count: 1,
            },
            markers: Vec::new(),
            frames: vec![RecordedFrame {
                timestamp_ns: 0,
                mode: Mode::Recording,
                attributes: vec![RecordedAttribute {
                    object_id: Uuid::nil(),
                    attribute: "position".into(),
                    value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
                }],
            }],
            version: RecordingFormatVersion::V2,
        };
        let chan = export(&recording);
        assert!(chan.contains("position.tx"));
        assert!(chan.contains("position.ty"));
        assert!(chan.contains("position.tz"));
    }
}
