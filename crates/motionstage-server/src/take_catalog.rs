use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use motionstage_core::SceneId;
use motionstage_protocol::{SnapshotScene, TakeInfo, TakeRating};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeEntry {
    pub take_id: Uuid,
    pub scene_id: SceneId,
    /// Server-assigned, monotonic-per-scene take number. Stable: assigned from
    /// a persisted per-scene counter (never re-derived from the display name),
    /// so deleting an earlier take never reuses its number. Legacy catalogs
    /// (field absent → 0) are backfilled by `created_ns` order on load.
    #[serde(default)]
    pub number: u32,
    pub name: String,
    pub path: PathBuf,
    pub created_ns: u64,
    pub frame_count: u64,
    /// Circle-take rating (design §2.1). Legacy catalogs default to `Unrated`.
    #[serde(default)]
    pub rating: TakeRating,
    /// Scene graph captured at record start, so the take replays after later
    /// scene edits and is exportable standalone. `None` for legacy takes.
    #[serde(default)]
    pub scene_snapshot: Option<SnapshotScene>,
    pub selected: bool,
    pub deleted: bool,
}

impl TakeEntry {
    pub(crate) fn to_take_info(&self) -> TakeInfo {
        TakeInfo {
            take_id: self.take_id,
            scene_id: self.scene_id,
            number: self.number,
            name: self.name.clone(),
            path: self.path.to_string_lossy().to_string(),
            created_ns: self.created_ns,
            frame_count: self.frame_count,
            rating: self.rating,
            scene_snapshot: self.scene_snapshot.clone(),
            selected: self.selected,
            deleted: self.deleted,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TakeCatalogDisk {
    takes: Vec<TakeEntry>,
    /// Persisted per-scene counter: the next number to assign for a scene. Kept
    /// separate from the take rows so a purged take's number is never reused.
    /// Legacy catalogs lack this field; it is rebuilt from take numbers on load.
    #[serde(default)]
    next_numbers: BTreeMap<SceneId, u32>,
}

#[derive(Debug, Clone)]
pub struct TakeCatalog {
    path: PathBuf,
    takes: BTreeMap<Uuid, TakeEntry>,
    next_numbers: BTreeMap<SceneId, u32>,
}

impl TakeCatalog {
    pub fn load_or_new(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let (mut takes, mut next_numbers): (BTreeMap<Uuid, TakeEntry>, BTreeMap<SceneId, u32>) =
            if path.exists() {
                let bytes = fs::read(&path).map_err(|err| err.to_string())?;
                if bytes.is_empty() {
                    (BTreeMap::new(), BTreeMap::new())
                } else {
                    let disk: TakeCatalogDisk =
                        serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
                    let takes = disk
                        .takes
                        .into_iter()
                        .map(|entry| (entry.take_id, entry))
                        .collect();
                    (takes, disk.next_numbers)
                }
            } else {
                (BTreeMap::new(), BTreeMap::new())
            };

        Self::backfill_numbers(&mut takes, &mut next_numbers);
        Ok(Self {
            path,
            takes,
            next_numbers,
        })
    }

    /// Assign stable numbers to legacy entries (number == 0) by `created_ns`
    /// order within each scene, then reconcile the per-scene counters so the
    /// next assigned number always exceeds every existing number.
    fn backfill_numbers(
        takes: &mut BTreeMap<Uuid, TakeEntry>,
        next_numbers: &mut BTreeMap<SceneId, u32>,
    ) {
        // Group take ids per scene.
        let mut by_scene: BTreeMap<SceneId, Vec<Uuid>> = BTreeMap::new();
        for entry in takes.values() {
            by_scene.entry(entry.scene_id).or_default().push(entry.take_id);
        }

        for (scene_id, mut ids) in by_scene {
            // Highest already-assigned number in this scene.
            let mut max_number = ids
                .iter()
                .filter_map(|id| takes.get(id).map(|e| e.number))
                .max()
                .unwrap_or(0);

            // Legacy (number == 0) entries in stable created_ns then take_id order.
            ids.sort_by_key(|id| {
                takes
                    .get(id)
                    .map(|e| (e.created_ns, *id))
                    .unwrap_or((0, *id))
            });
            for id in &ids {
                if let Some(entry) = takes.get_mut(id) {
                    if entry.number == 0 {
                        max_number += 1;
                        entry.number = max_number;
                        // Legacy rows baked the number into the display name;
                        // only replace the default-shaped name to avoid clobbering
                        // an operator's custom slate.
                        if entry.name.is_empty() {
                            entry.name = format!("Take {:03}", entry.number);
                        }
                    }
                }
            }

            let counter = next_numbers.entry(scene_id).or_insert(1);
            if *counter <= max_number {
                *counter = max_number + 1;
            }
        }
    }

    pub fn list(&self, scene_id: Option<SceneId>) -> Vec<TakeInfo> {
        let mut entries: Vec<_> = self
            .takes
            .values()
            .filter(|entry| !entry.deleted)
            .filter(|entry| scene_id.map(|id| id == entry.scene_id).unwrap_or(true))
            .cloned()
            .collect();
        entries.sort_by_key(|entry| entry.created_ns);
        entries
            .into_iter()
            .map(|entry| entry.to_take_info())
            .collect()
    }

    pub fn get(&self, take_id: Uuid) -> Option<&TakeEntry> {
        self.takes.get(&take_id).filter(|entry| !entry.deleted)
    }

    pub fn register_take(
        &mut self,
        take_id: Uuid,
        scene_id: SceneId,
        path: PathBuf,
        created_ns: u64,
        frame_count: u64,
        scene_snapshot: Option<SnapshotScene>,
    ) -> Result<TakeEntry, String> {
        // Stable numbering: advance the persisted per-scene counter. This never
        // reuses a number after deletion (unlike a count of live takes).
        let number = {
            let counter = self.next_numbers.entry(scene_id).or_insert(1);
            let assigned = *counter;
            *counter = counter.saturating_add(1);
            assigned
        };
        let name = format!("Take {number:03}");

        for entry in self.takes.values_mut() {
            if entry.scene_id == scene_id {
                entry.selected = false;
            }
        }

        let entry = TakeEntry {
            take_id,
            scene_id,
            number,
            name,
            path,
            created_ns,
            frame_count,
            rating: TakeRating::Unrated,
            scene_snapshot,
            selected: true,
            deleted: false,
        };
        self.takes.insert(take_id, entry.clone());
        self.persist()?;
        Ok(entry)
    }

    pub fn select_take(&mut self, take_id: Uuid) -> Result<TakeInfo, String> {
        let scene_id = self
            .takes
            .get(&take_id)
            .filter(|entry| !entry.deleted)
            .map(|entry| entry.scene_id)
            .ok_or_else(|| format!("take not found: {take_id}"))?;

        for entry in self.takes.values_mut() {
            if entry.scene_id == scene_id {
                entry.selected = entry.take_id == take_id;
            }
        }
        self.persist()?;
        self.takes
            .get(&take_id)
            .map(TakeEntry::to_take_info)
            .ok_or_else(|| format!("take not found after select: {take_id}"))
    }

    pub fn rename_take(&mut self, take_id: Uuid, name: String) -> Result<TakeInfo, String> {
        {
            let entry = self
                .takes
                .get_mut(&take_id)
                .filter(|entry| !entry.deleted)
                .ok_or_else(|| format!("take not found: {take_id}"))?;
            entry.name = name;
        }
        self.persist()?;
        self.takes
            .get(&take_id)
            .map(TakeEntry::to_take_info)
            .ok_or_else(|| format!("take not found after rename: {take_id}"))
    }

    pub fn rate_take(&mut self, take_id: Uuid, rating: TakeRating) -> Result<TakeInfo, String> {
        {
            let entry = self
                .takes
                .get_mut(&take_id)
                .filter(|entry| !entry.deleted)
                .ok_or_else(|| format!("take not found: {take_id}"))?;
            entry.rating = rating;
        }
        self.persist()?;
        self.takes
            .get(&take_id)
            .map(TakeEntry::to_take_info)
            .ok_or_else(|| format!("take not found after rate: {take_id}"))
    }

    pub fn mark_deleted(&mut self, take_id: Uuid) -> Result<Option<PathBuf>, String> {
        let Some(entry) = self.takes.get_mut(&take_id) else {
            return Ok(None);
        };
        if entry.deleted {
            return Ok(None);
        }
        entry.deleted = true;
        entry.selected = false;
        let path = entry.path.clone();
        self.persist()?;
        Ok(Some(path))
    }

    pub fn purge_take(&mut self, take_id: Uuid) -> Result<(), String> {
        // The per-scene counter is intentionally left untouched: a purged
        // take's number must never be reused by a later recording.
        let _ = self.takes.remove(&take_id);
        self.persist()
    }

    pub fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
        }

        let disk = TakeCatalogDisk {
            takes: self.takes.values().cloned().collect(),
            next_numbers: self.next_numbers.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&disk).map_err(|err| err.to_string())?;
        fs::write(&self.path, bytes).map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn register(
        catalog: &mut TakeCatalog,
        scene_id: SceneId,
        created_ns: u64,
    ) -> TakeEntry {
        catalog
            .register_take(
                Uuid::now_v7(),
                scene_id,
                PathBuf::from(format!("take-{created_ns}.cmtrk")),
                created_ns,
                1,
                None,
            )
            .unwrap()
    }

    #[test]
    fn take_numbers_are_stable_across_deletion() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("takes.json");
        let mut catalog = TakeCatalog::load_or_new(&path).unwrap();
        let scene_id = Uuid::now_v7();

        let t1 = register(&mut catalog, scene_id, 10);
        let t2 = register(&mut catalog, scene_id, 20);
        assert_eq!(t1.number, 1);
        assert_eq!(t2.number, 2);
        assert_eq!(t1.name, "Take 001");

        // Delete + purge take 1 (its number must never be reused).
        catalog.mark_deleted(t1.take_id).unwrap();
        catalog.purge_take(t1.take_id).unwrap();

        // Next take is number 3, not a reused 1.
        let t3 = register(&mut catalog, scene_id, 30);
        assert_eq!(t3.number, 3, "numbering must not reuse a deleted number");

        // Deleting the current max and registering still advances.
        catalog.mark_deleted(t3.take_id).unwrap();
        catalog.purge_take(t3.take_id).unwrap();
        let t4 = register(&mut catalog, scene_id, 40);
        assert_eq!(t4.number, 4);
    }

    #[test]
    fn counter_persists_across_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("takes.json");
        let scene_id = Uuid::now_v7();
        {
            let mut catalog = TakeCatalog::load_or_new(&path).unwrap();
            let a = register(&mut catalog, scene_id, 10);
            let b = register(&mut catalog, scene_id, 20);
            assert_eq!((a.number, b.number), (1, 2));
            catalog.mark_deleted(b.take_id).unwrap();
            catalog.purge_take(b.take_id).unwrap();
        }
        // Reload: the persisted counter still knows the next number is 3.
        let mut catalog = TakeCatalog::load_or_new(&path).unwrap();
        let c = register(&mut catalog, scene_id, 30);
        assert_eq!(c.number, 3);
    }

    #[test]
    fn legacy_catalog_without_new_fields_deserializes_and_backfills() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("takes.json");
        let scene_id = Uuid::now_v7();
        let scene = scene_id.to_string();
        let older = Uuid::now_v7();
        let newer = Uuid::now_v7();
        // A legacy on-disk catalog: no `number`, `rating`, `scene_snapshot`,
        // or `next_numbers` fields. Written in created_ns-descending order to
        // prove backfill sorts by created_ns, not file order.
        let legacy = format!(
            r#"{{
              "takes": [
                {{
                  "take_id": "{newer}",
                  "scene_id": "{scene}",
                  "name": "",
                  "path": "take-newer.cmtrk",
                  "created_ns": 200,
                  "frame_count": 5,
                  "selected": true,
                  "deleted": false
                }},
                {{
                  "take_id": "{older}",
                  "scene_id": "{scene}",
                  "name": "custom slate",
                  "path": "take-older.cmtrk",
                  "created_ns": 100,
                  "frame_count": 3,
                  "selected": false,
                  "deleted": false
                }}
              ]
            }}"#
        );
        fs::write(&path, legacy).unwrap();

        let mut catalog = TakeCatalog::load_or_new(&path).unwrap();
        let takes = catalog.list(Some(scene_id));
        // Sorted by created_ns; numbers backfilled 1,2 in created_ns order.
        assert_eq!(takes[0].take_id, older);
        assert_eq!(takes[0].number, 1);
        assert_eq!(takes[0].name, "custom slate", "custom slate is preserved");
        assert_eq!(takes[0].rating, TakeRating::Unrated);
        assert!(takes[0].scene_snapshot.is_none());
        assert_eq!(takes[1].take_id, newer);
        assert_eq!(takes[1].number, 2);
        assert_eq!(takes[1].name, "Take 002", "empty legacy name is synthesized");

        // Counter continues past the backfilled max.
        let next = register(&mut catalog, scene_id, 300);
        assert_eq!(next.number, 3);
    }

    #[test]
    fn rename_and_rate_persist() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("takes.json");
        let scene_id = Uuid::now_v7();
        let take_id;
        {
            let mut catalog = TakeCatalog::load_or_new(&path).unwrap();
            let t = register(&mut catalog, scene_id, 10);
            take_id = t.take_id;
            let renamed = catalog.rename_take(take_id, "sc04_setupB".into()).unwrap();
            assert_eq!(renamed.name, "sc04_setupB");
            let rated = catalog.rate_take(take_id, TakeRating::Circle).unwrap();
            assert_eq!(rated.rating, TakeRating::Circle);
            assert_eq!(rated.name, "sc04_setupB");
        }
        // Reload from disk: both edits persisted.
        let catalog = TakeCatalog::load_or_new(&path).unwrap();
        let take = catalog.get(take_id).unwrap();
        assert_eq!(take.name, "sc04_setupB");
        assert_eq!(take.rating, TakeRating::Circle);
    }
}
