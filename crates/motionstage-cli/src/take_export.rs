//! `take-export`: materialize Level B USD (design §4.3) for offline/backfill.
//!
//! Reads a take catalog (`takes_catalog.json`) plus the recordings directory
//! and writes a self-contained `take-<uuid>.usda` layer per take and a
//! `stage.usda` root layer that subLayers them. This is the offline twin of
//! the server's materialize-on-stop hook: use it to backfill a library that
//! predates Level B, or to re-derive the USD projection after moving files.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use motionstage_export_usd::{
    author_stage_layer, export_take_layer, export_with_options, take_layer_file_name, StageEntry,
    UsdExportOptions,
};
use motionstage_protocol::SnapshotScene;
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, Clone, Args)]
pub struct TakeExportArgs {
    /// Take catalog JSON (`takes_catalog.json`).
    #[arg(long)]
    pub catalog: PathBuf,
    /// Directory holding the `.cmtrk` capture files. Defaults to the catalog's
    /// parent directory.
    #[arg(long)]
    pub recordings: Option<PathBuf>,
    /// Directory to write `.usda` layers + `stage.usda` into. Defaults to the
    /// recordings directory.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Materialize only this take's layer (the whole library otherwise).
    /// `stage.usda` is always refreshed over every live take.
    #[arg(long)]
    pub take: Option<Uuid>,
    /// USD `timeCodesPerSecond`.
    #[arg(long, default_value_t = 120)]
    pub fps: u32,
}

/// Minimal view of the on-disk catalog — only the fields Level B needs. The
/// authoritative type lives in `motionstage-server` (private); mirroring the
/// serde shape here keeps the CLI decoupled from the server's internals.
#[derive(Debug, Deserialize)]
struct CatalogDisk {
    takes: Vec<CatalogTake>,
}

#[derive(Debug, Deserialize)]
struct CatalogTake {
    take_id: Uuid,
    scene_id: Uuid,
    #[serde(default)]
    name: String,
    path: PathBuf,
    #[serde(default)]
    scene_snapshot: Option<SnapshotScene>,
    #[serde(default)]
    deleted: bool,
}

pub fn run(args: &TakeExportArgs) -> Result<()> {
    if args.fps == 0 {
        return Err(anyhow!("--fps must be greater than zero"));
    }
    let recordings = args
        .recordings
        .clone()
        .or_else(|| args.catalog.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let output = args.output.clone().unwrap_or_else(|| recordings.clone());

    let bytes = std::fs::read(&args.catalog)
        .with_context(|| format!("failed to read catalog `{}`", args.catalog.display()))?;
    let disk: CatalogDisk = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse catalog `{}`", args.catalog.display()))?;

    let options = UsdExportOptions {
        time_codes_per_second: args.fps,
        ..UsdExportOptions::default()
    };

    let live: Vec<&CatalogTake> = disk.takes.iter().filter(|take| !take.deleted).collect();

    std::fs::create_dir_all(&output)
        .with_context(|| format!("failed to create output dir `{}`", output.display()))?;

    // Which take layers to (re)author: one take, or all live takes.
    let to_write: Vec<&CatalogTake> = match args.take {
        Some(id) => {
            let take = live
                .iter()
                .copied()
                .find(|take| take.take_id == id)
                .ok_or_else(|| anyhow!("take {id} not found (or deleted) in catalog"))?;
            vec![take]
        }
        None => live.clone(),
    };

    let mut written = 0usize;
    for take in &to_write {
        materialize_take_layer(take, &recordings, &output, &options)
            .with_context(|| format!("failed to materialize take `{}`", take.take_id))?;
        written += 1;
    }

    // Always refresh stage.usda over every live take so it references exactly
    // the current library, regardless of --take.
    let entries: Vec<StageEntry> = live
        .iter()
        .map(|take| StageEntry {
            scene_id: take.scene_id,
            scene_name: take.scene_snapshot.as_ref().map(|snapshot| snapshot.name.clone()),
            layer_path: take_layer_file_name(&take.take_id),
        })
        .collect();
    let stage_text = author_stage_layer(&entries, &options);
    let stage_path = output.join("stage.usda");
    std::fs::write(&stage_path, stage_text)
        .with_context(|| format!("failed to write `{}`", stage_path.display()))?;

    println!(
        "materialized {written} take layer(s) and stage.usda ({} live take(s)) into {}",
        live.len(),
        output.display()
    );
    Ok(())
}

fn materialize_take_layer(
    take: &CatalogTake,
    recordings: &Path,
    output: &Path,
    options: &UsdExportOptions,
) -> Result<()> {
    let cmtrk = resolve_recording_path(recordings, &take.path);
    let recording = motionstage_recording::read_recording(&cmtrk)
        .with_context(|| format!("failed to read recording `{}`", cmtrk.display()))?;
    let text = match &take.scene_snapshot {
        Some(snapshot) => export_take_layer(&recording, snapshot, options),
        // Legacy take with no snapshot: fall back to a Level A layer so the
        // stage can still subLayer it.
        None => {
            tracing::warn!(
                take_id = %take.take_id,
                name = %take.name,
                "take has no scene snapshot; authoring Level A layer without scene structure"
            );
            export_with_options(&recording, options)
        }
    };
    let layer_path = output.join(take_layer_file_name(&take.take_id));
    std::fs::write(&layer_path, text)
        .with_context(|| format!("failed to write `{}`", layer_path.display()))?;
    Ok(())
}

/// Resolve a catalog `path` to an on-disk `.cmtrk`. Absolute or already-present
/// paths are used verbatim; otherwise the file name is looked up under the
/// recordings directory.
fn resolve_recording_path(recordings: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() || path.exists() {
        return path.to_path_buf();
    }
    match path.file_name() {
        Some(name) => recordings.join(name),
        None => recordings.join(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use motionstage_core::AttributeValue;
    use motionstage_protocol::{
        BakeAttributeValue, Mode, SnapshotAttribute, SnapshotObject, SnapshotScene,
    };
    use motionstage_recording::{RecordedAttribute, RecordedFrame, RecordingWriter};

    fn write_fixture_take(dir: &Path, take_id: Uuid, object_id: Uuid) -> PathBuf {
        let path = dir.join(format!("take-{take_id}.cmtrk"));
        let mut writer = RecordingWriter::start(Uuid::nil(), 0);
        for i in 0..3u64 {
            writer.push_frame(RecordedFrame {
                timestamp_ns: i * 25_000_000,
                mode: Mode::RECORDING,
                attributes: vec![RecordedAttribute {
                    object_id,
                    attribute: "position".into(),
                    value: AttributeValue::Vec3f([i as f32, 0.0, 1.5]),
                }],
            });
        }
        writer.finish(&path).unwrap();
        path
    }

    fn snapshot(object_id: Uuid) -> SnapshotScene {
        SnapshotScene {
            scene_id: Uuid::nil(),
            name: "sc04".into(),
            objects: vec![SnapshotObject {
                object_id,
                name: "hero_cam".into(),
                attributes: vec![SnapshotAttribute {
                    name: "position".into(),
                    default_value: BakeAttributeValue::Vec3f([0.0, 0.0, 1.6]),
                    current_value: BakeAttributeValue::Vec3f([0.0, 0.0, 1.6]),
                    live_enabled: true,
                    record_enabled: true,
                }],
            }],
        }
    }

    fn write_catalog(dir: &Path, takes: &[(Uuid, Uuid, PathBuf, bool)]) -> PathBuf {
        // Author a catalog JSON with an embedded snapshot for each take.
        let entries: Vec<String> = takes
            .iter()
            .map(|(take_id, object_id, path, deleted)| {
                let snap = serde_json::to_string(&snapshot(*object_id)).unwrap();
                format!(
                    r#"{{
                      "take_id": "{take_id}",
                      "scene_id": "00000000-0000-0000-0000-000000000000",
                      "number": 1,
                      "name": "Take 001",
                      "path": {path:?},
                      "created_ns": 0,
                      "frame_count": 3,
                      "rating": "Unrated",
                      "scene_snapshot": {snap},
                      "selected": false,
                      "deleted": {deleted}
                    }}"#,
                    path = path.to_string_lossy(),
                )
            })
            .collect();
        let catalog = format!("{{\n  \"takes\": [{}]\n}}", entries.join(","));
        let path = dir.join("takes_catalog.json");
        std::fs::write(&path, catalog).unwrap();
        path
    }

    #[test]
    fn take_export_writes_layer_and_stage_for_whole_library() {
        let dir = tempfile::tempdir().unwrap();
        let take_id = Uuid::now_v7();
        let object_id = Uuid::nil();
        let cmtrk = write_fixture_take(dir.path(), take_id, object_id);
        let catalog = write_catalog(
            dir.path(),
            &[(take_id, object_id, cmtrk.clone(), false)],
        );

        run(&TakeExportArgs {
            catalog,
            recordings: Some(dir.path().to_path_buf()),
            output: Some(dir.path().to_path_buf()),
            take: None,
            fps: 120,
        })
        .unwrap();

        let layer = std::fs::read_to_string(dir.path().join(take_layer_file_name(&take_id))).unwrap();
        assert!(layer.starts_with("#usda 1.0\n"));
        // Snapshot baseline + recorded animation are both present.
        assert!(layer.contains("double3 xformOp:translate = (0, 0, 1.6)"));
        assert!(layer.contains("double3 xformOp:translate.timeSamples = {"));
        assert!(layer.contains("string motionstage_object_name = \"hero_cam\""));

        let stage = std::fs::read_to_string(dir.path().join("stage.usda")).unwrap();
        assert!(stage.contains("subLayers = ["));
        assert!(stage.contains(&format!("@{}@", take_layer_file_name(&take_id))));
    }

    #[test]
    fn take_export_single_take_still_refreshes_stage_over_all_live() {
        let dir = tempfile::tempdir().unwrap();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let object_id = Uuid::nil();
        let ca = write_fixture_take(dir.path(), a, object_id);
        let cb = write_fixture_take(dir.path(), b, object_id);
        let catalog = write_catalog(
            dir.path(),
            &[(a, object_id, ca, false), (b, object_id, cb, false)],
        );

        run(&TakeExportArgs {
            catalog,
            recordings: Some(dir.path().to_path_buf()),
            output: Some(dir.path().to_path_buf()),
            take: Some(a),
            fps: 120,
        })
        .unwrap();

        // Only take a's layer is authored...
        assert!(dir.path().join(take_layer_file_name(&a)).exists());
        assert!(!dir.path().join(take_layer_file_name(&b)).exists());
        // ...but stage.usda references both live takes.
        let stage = std::fs::read_to_string(dir.path().join("stage.usda")).unwrap();
        assert!(stage.contains(&format!("@{}@", take_layer_file_name(&a))));
        assert!(stage.contains(&format!("@{}@", take_layer_file_name(&b))));
    }

    #[test]
    fn take_export_skips_deleted_takes() {
        let dir = tempfile::tempdir().unwrap();
        let live = Uuid::now_v7();
        let gone = Uuid::now_v7();
        let object_id = Uuid::nil();
        let cl = write_fixture_take(dir.path(), live, object_id);
        let cg = write_fixture_take(dir.path(), gone, object_id);
        let catalog = write_catalog(
            dir.path(),
            &[(live, object_id, cl, false), (gone, object_id, cg, true)],
        );

        run(&TakeExportArgs {
            catalog,
            recordings: Some(dir.path().to_path_buf()),
            output: Some(dir.path().to_path_buf()),
            take: None,
            fps: 120,
        })
        .unwrap();

        assert!(dir.path().join(take_layer_file_name(&live)).exists());
        assert!(!dir.path().join(take_layer_file_name(&gone)).exists());
        let stage = std::fs::read_to_string(dir.path().join("stage.usda")).unwrap();
        assert!(!stage.contains(&format!("@{}@", take_layer_file_name(&gone))));
    }

    #[test]
    fn take_export_rejects_zero_fps() {
        let err = run(&TakeExportArgs {
            catalog: PathBuf::from("nope.json"),
            recordings: None,
            output: None,
            take: None,
            fps: 0,
        })
        .unwrap_err();
        assert!(err.to_string().contains("--fps"));
    }
}
