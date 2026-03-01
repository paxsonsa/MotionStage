use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use motionstage_core::SceneId;
use motionstage_protocol::TakeInfo;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeEntry {
    pub take_id: Uuid,
    pub scene_id: SceneId,
    pub name: String,
    pub path: PathBuf,
    pub created_ns: u64,
    pub frame_count: u64,
    pub selected: bool,
    pub deleted: bool,
}

impl TakeEntry {
    fn to_take_info(&self) -> TakeInfo {
        TakeInfo {
            take_id: self.take_id,
            scene_id: self.scene_id,
            name: self.name.clone(),
            path: self.path.to_string_lossy().to_string(),
            created_ns: self.created_ns,
            frame_count: self.frame_count,
            selected: self.selected,
            deleted: self.deleted,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TakeCatalogDisk {
    takes: Vec<TakeEntry>,
}

#[derive(Debug, Clone)]
pub struct TakeCatalog {
    path: PathBuf,
    takes: BTreeMap<Uuid, TakeEntry>,
}

impl TakeCatalog {
    pub fn load_or_new(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let takes = if path.exists() {
            let bytes = fs::read(&path).map_err(|err| err.to_string())?;
            if bytes.is_empty() {
                BTreeMap::new()
            } else {
                let disk: TakeCatalogDisk =
                    serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
                disk.takes
                    .into_iter()
                    .map(|entry| (entry.take_id, entry))
                    .collect()
            }
        } else {
            BTreeMap::new()
        };

        Ok(Self { path, takes })
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
    ) -> Result<TakeEntry, String> {
        let next_number = self
            .takes
            .values()
            .filter(|entry| entry.scene_id == scene_id && !entry.deleted)
            .count()
            + 1;
        let name = format!("Take {next_number:03}");

        for entry in self.takes.values_mut() {
            if entry.scene_id == scene_id {
                entry.selected = false;
            }
        }

        let entry = TakeEntry {
            take_id,
            scene_id,
            name,
            path,
            created_ns,
            frame_count,
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
        };
        let bytes = serde_json::to_vec_pretty(&disk).map_err(|err| err.to_string())?;
        fs::write(&self.path, bytes).map_err(|err| err.to_string())
    }
}
