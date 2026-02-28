use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::Path,
};

use cinemotion_core::{AttributeValue, MappingId, ObjectId, SceneId};
use cinemotion_protocol::Mode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAGIC_V1: &[u8; 6] = b"CMTRK1";
const MAGIC_V2: &[u8; 6] = b"CMTRK2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingFormatVersion {
    V1,
    V2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingManifest {
    pub recording_id: Uuid,
    pub scene_id: SceneId,
    pub started_ns: u64,
    pub stopped_ns: u64,
    pub frame_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedAttribute {
    pub object_id: ObjectId,
    pub attribute: String,
    pub value: AttributeValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedFrame {
    pub timestamp_ns: u64,
    pub mode: Mode,
    pub attributes: Vec<RecordedAttribute>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RecordingMarker {
    ModeTransition {
        timestamp_ns: u64,
        from: Mode,
        to: Mode,
    },
    MappingCreated {
        timestamp_ns: u64,
        mapping_id: MappingId,
        source_device: Uuid,
        source_output: String,
        target_scene: SceneId,
        target_object: ObjectId,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
    },
    MappingUpdated {
        timestamp_ns: u64,
        mapping_id: MappingId,
        source_device: Uuid,
        source_output: String,
        target_scene: SceneId,
        target_object: ObjectId,
        target_attribute: String,
        component_mask: Option<Vec<usize>>,
    },
    MappingRemoved {
        timestamp_ns: u64,
        mapping_id: MappingId,
    },
    MappingLockSet {
        timestamp_ns: u64,
        mapping_id: MappingId,
        lock: bool,
    },
}

#[derive(Debug, Clone)]
pub struct RecordingWriter {
    manifest: RecordingManifest,
    frames: Vec<RecordedFrame>,
    markers: Vec<RecordingMarker>,
    version: RecordingFormatVersion,
}

impl RecordingWriter {
    pub fn start(scene_id: SceneId, started_ns: u64) -> Self {
        Self::start_with_format(scene_id, started_ns, RecordingFormatVersion::V2)
    }

    pub fn start_with_format(
        scene_id: SceneId,
        started_ns: u64,
        version: RecordingFormatVersion,
    ) -> Self {
        Self {
            manifest: RecordingManifest {
                recording_id: Uuid::now_v7(),
                scene_id,
                started_ns,
                stopped_ns: started_ns,
                frame_count: 0,
            },
            frames: Vec::new(),
            markers: Vec::new(),
            version,
        }
    }

    pub fn recording_id(&self) -> Uuid {
        self.manifest.recording_id
    }

    pub fn push_frame(&mut self, frame: RecordedFrame) {
        self.manifest.stopped_ns = frame.timestamp_ns;
        self.frames.push(frame);
        self.manifest.frame_count = self.frames.len() as u64;
    }

    pub fn push_marker(&mut self, marker: RecordingMarker) {
        if let RecordingFormatVersion::V2 = self.version {
            self.markers.push(marker);
        }
    }

    pub fn finish(self, path: impl AsRef<Path>) -> Result<RecordingManifest, RecordingError> {
        let mut file = File::create(path)?;
        match self.version {
            RecordingFormatVersion::V1 => file.write_all(MAGIC_V1)?,
            RecordingFormatVersion::V2 => file.write_all(MAGIC_V2)?,
        }

        write_blob(&mut file, &self.manifest)?;
        if matches!(self.version, RecordingFormatVersion::V2) {
            write_blob(&mut file, &self.markers)?;
        }
        for frame in &self.frames {
            write_blob(&mut file, frame)?;
        }

        Ok(self.manifest)
    }
}

#[derive(Debug, Clone)]
pub struct RecordingFile {
    pub manifest: RecordingManifest,
    pub markers: Vec<RecordingMarker>,
    pub frames: Vec<RecordedFrame>,
    pub version: RecordingFormatVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingIndex {
    pub manifest_offset: u64,
    pub marker_offset: Option<u64>,
    pub frame_offsets: Vec<u64>,
    pub version: RecordingFormatVersion,
}

pub fn read_recording(path: impl AsRef<Path>) -> Result<RecordingFile, RecordingError> {
    let mut file = File::open(path)?;
    let version = read_magic(&mut file)?;

    let manifest: RecordingManifest = read_blob(&mut file)?;
    let markers = if matches!(version, RecordingFormatVersion::V2) {
        read_blob(&mut file)?
    } else {
        Vec::new()
    };

    let mut frames = Vec::new();
    loop {
        match read_blob::<RecordedFrame>(&mut file) {
            Ok(frame) => frames.push(frame),
            Err(RecordingError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(err) => return Err(err),
        }
    }

    Ok(RecordingFile {
        manifest,
        markers,
        frames,
        version,
    })
}

pub fn build_index(path: impl AsRef<Path>) -> Result<RecordingIndex, RecordingError> {
    let mut file = File::open(path)?;
    let version = read_magic(&mut file)?;

    let manifest_offset = file.stream_position()?;
    skip_blob(&mut file)?;

    let marker_offset = if matches!(version, RecordingFormatVersion::V2) {
        let offset = file.stream_position()?;
        skip_blob(&mut file)?;
        Some(offset)
    } else {
        None
    };

    let mut frame_offsets = Vec::new();
    loop {
        let start = file.stream_position()?;
        match skip_blob(&mut file) {
            Ok(()) => frame_offsets.push(start),
            Err(RecordingError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(err) => return Err(err),
        }
    }

    Ok(RecordingIndex {
        manifest_offset,
        marker_offset,
        frame_offsets,
        version,
    })
}

fn read_magic(file: &mut File) -> Result<RecordingFormatVersion, RecordingError> {
    let mut magic = [0_u8; 6];
    file.read_exact(&mut magic)?;
    if &magic == MAGIC_V1 {
        Ok(RecordingFormatVersion::V1)
    } else if &magic == MAGIC_V2 {
        Ok(RecordingFormatVersion::V2)
    } else {
        Err(RecordingError::InvalidFormat("invalid magic".into()))
    }
}

fn write_blob<T: Serialize>(file: &mut File, value: &T) -> Result<(), RecordingError> {
    let bytes = bincode::serialize(value)?;
    let len = bytes.len() as u32;
    file.write_all(&len.to_le_bytes())?;
    file.write_all(&bytes)?;
    Ok(())
}

fn read_blob<T: for<'a> Deserialize<'a>>(file: &mut File) -> Result<T, RecordingError> {
    let mut len_bytes = [0_u8; 4];
    file.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let mut bytes = vec![0_u8; len];
    file.read_exact(&mut bytes)?;
    Ok(bincode::deserialize(&bytes)?)
}

fn skip_blob(file: &mut File) -> Result<(), RecordingError> {
    let mut len_bytes = [0_u8; 4];
    file.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as u64;
    file.seek(std::io::SeekFrom::Current(len as i64))?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding error: {0}")]
    Encoding(#[from] bincode::Error),
    #[error("invalid format: {0}")]
    InvalidFormat(String),
}

#[cfg(test)]
mod tests {
    use cinemotion_core::AttributeValue;
    use cinemotion_protocol::Mode;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    use super::{
        build_index, read_recording, RecordedAttribute, RecordedFrame, RecordingFormatVersion,
        RecordingMarker, RecordingWriter,
    };

    #[test]
    fn recording_roundtrip_preserves_frames_and_markers_in_v2() {
        let scene_id = Uuid::now_v7();
        let object_id = Uuid::now_v7();

        let mut writer = RecordingWriter::start(scene_id, 100);
        writer.push_marker(RecordingMarker::ModeTransition {
            timestamp_ns: 100,
            from: Mode::Live,
            to: Mode::Recording,
        });
        writer.push_frame(RecordedFrame {
            timestamp_ns: 100,
            mode: Mode::Recording,
            attributes: vec![RecordedAttribute {
                object_id,
                attribute: "position".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
            }],
        });

        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let manifest = writer.finish(&path).unwrap();
        assert_eq!(manifest.frame_count, 1);

        let loaded = read_recording(&path).unwrap();
        assert_eq!(loaded.version, RecordingFormatVersion::V2);
        assert_eq!(loaded.manifest.frame_count, 1);
        assert_eq!(loaded.frames.len(), 1);
        assert_eq!(loaded.markers.len(), 1);

        let index = build_index(&path).unwrap();
        assert_eq!(index.frame_offsets.len(), 1);
        assert!(index.marker_offset.is_some());
    }

    #[test]
    fn recording_reader_supports_legacy_v1_files() {
        let scene_id = Uuid::now_v7();
        let object_id = Uuid::now_v7();

        let mut writer =
            RecordingWriter::start_with_format(scene_id, 100, RecordingFormatVersion::V1);
        writer.push_frame(RecordedFrame {
            timestamp_ns: 100,
            mode: Mode::Recording,
            attributes: vec![RecordedAttribute {
                object_id,
                attribute: "position".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
            }],
        });

        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        writer.finish(&path).unwrap();

        let loaded = read_recording(&path).unwrap();
        assert_eq!(loaded.version, RecordingFormatVersion::V1);
        assert!(loaded.markers.is_empty());
        assert_eq!(loaded.frames.len(), 1);
    }
}
