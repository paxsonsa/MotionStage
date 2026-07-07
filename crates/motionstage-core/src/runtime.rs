use std::collections::{BTreeMap, BTreeSet, HashMap};

use indexmap::IndexMap;
use motionstage_protocol::{DataFlowState, Mode, RecordingState};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    AttributeFilter, AttributeUpdate, AttributeValue, LeaseConfig, Mapping, MappingId,
    MappingRequest, MappingState, ObjectId, Scene, SceneId,
};

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("scene not found: {0}")]
    SceneNotFound(SceneId),
    #[error("object not found: {0}")]
    ObjectNotFound(ObjectId),
    #[error("attribute not found: {0}")]
    AttributeNotFound(String),
    #[error("mapping denied: {0}")]
    MappingDenied(String),
    #[error("mapping changes are blocked in recording mode")]
    MappingBlockedInRecording,
    #[error("invalid mode transition: {from:?} -> {to:?}")]
    InvalidModeTransition { from: Mode, to: Mode },
    #[error("type mismatch, expected {expected} got {got}")]
    TypeMismatch {
        expected: &'static str,
        got: &'static str,
    },
    #[error("invalid component mask: {0}")]
    InvalidComponentMask(String),
    #[error("unsupported mapping transform: {0}")]
    UnsupportedTransform(String),
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeSnapshot {
    pub scenes: IndexMap<SceneId, Scene>,
    pub active_scene: Option<SceneId>,
    pub mode: Option<Mode>,
    pub mappings: IndexMap<MappingId, Mapping>,
}

/// Result of a playback source acquiring ownership of a take's target
/// attributes through the lease model (design §3). Each entry is a
/// `(object, attribute)` pair on the playback's target scene.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackAcquisition {
    /// Targets the playback source now owns and may replay into.
    pub owned: Vec<(ObjectId, String)>,
    /// Targets kept by a live mapping owner — playback of these tracks is
    /// blocked (no override), and their live value is untouched.
    pub blocked: Vec<(ObjectId, String)>,
}

#[derive(Debug)]
pub struct RuntimeCore {
    tick_count: u64,
    scenes: IndexMap<SceneId, Scene>,
    active_scene: Option<SceneId>,
    mode: Mode,
    mappings: IndexMap<MappingId, Mapping>,
    mapping_index: HashMap<(Uuid, String), MappingId>,
    lease: LeaseConfig,
    connected_devices: BTreeSet<Uuid>,
    /// The synthetic playback source that currently holds playback leases
    /// (design §3 "virtual player"). `None` when no take is loaded.
    playback_owner: Option<Uuid>,
    /// Target attributes the playback source exclusively owns. Replay writes
    /// only to these; live mapping ingest skips them. Shares the
    /// one-owner-per-target-attribute invariant with `mappings`.
    playback_owned: BTreeSet<(SceneId, ObjectId, String)>,
}

impl Default for RuntimeCore {
    fn default() -> Self {
        Self::new(LeaseConfig::default())
    }
}

impl RuntimeCore {
    pub fn new(lease: LeaseConfig) -> Self {
        Self {
            tick_count: 0,
            scenes: IndexMap::new(),
            active_scene: None,
            mode: Mode::IDLE,
            mappings: IndexMap::new(),
            mapping_index: HashMap::new(),
            lease,
            connected_devices: BTreeSet::new(),
            playback_owner: None,
            playback_owned: BTreeSet::new(),
        }
    }

    /// The playback source device id currently holding playback leases, if any.
    pub fn playback_owner(&self) -> Option<Uuid> {
        self.playback_owner
    }

    /// True when a playback source owns `(scene, object, attribute)`.
    pub fn is_playback_owned(&self, scene_id: SceneId, object_id: ObjectId, attribute: &str) -> bool {
        self.playback_owned
            .contains(&(scene_id, object_id, attribute.to_owned()))
    }

    /// Acquire playback ownership of a take's target attributes through the
    /// lease model (design §3). For each `(object, attribute)` on `scene_id`:
    /// if a live mapping already owns it and `override_live` is false, the
    /// track is **blocked** (the live owner keeps it); otherwise the playback
    /// source `owner` takes ownership. With `override_live` the playback source
    /// reclaims the attribute — the live mapping is left in place but is
    /// superseded (its ingest is skipped) until playback releases the lease.
    /// Acquiring resets any prior playback ownership held by a different owner.
    pub fn acquire_playback_targets(
        &mut self,
        owner: Uuid,
        scene_id: SceneId,
        targets: impl IntoIterator<Item = (ObjectId, String)>,
        override_live: bool,
    ) -> PlaybackAcquisition {
        // A fresh acquisition supersedes any stale playback ownership.
        self.playback_owned.clear();
        self.playback_owner = Some(owner);
        let mut acquisition = PlaybackAcquisition::default();
        for (object_id, attribute) in targets {
            let live_owned = self
                .find_mapping_for_target(scene_id, object_id, &attribute)
                .is_some();
            if live_owned && !override_live {
                acquisition.blocked.push((object_id, attribute));
            } else {
                self.playback_owned
                    .insert((scene_id, object_id, attribute.clone()));
                acquisition.owned.push((object_id, attribute));
            }
        }
        acquisition
    }

    /// Release all playback leases held by `owner` so live mappings can reclaim
    /// their target attributes normally. A no-op if `owner` is not the current
    /// playback source.
    pub fn release_playback(&mut self, owner: Uuid) {
        if self.playback_owner == Some(owner) {
            self.playback_owner = None;
            self.playback_owned.clear();
        }
    }

    /// Write a replayed (already-resolved) value, but **only** if `owner` holds
    /// the playback lease on `(scene, object, attribute)`. Returns `Ok(true)`
    /// when the value was written, `Ok(false)` when the attribute is not owned
    /// (playback of that track is blocked and must be skipped — never
    /// recomposed). Recorded values are applied verbatim; the transform
    /// pipeline is intentionally bypassed.
    pub fn set_owned_scene_attribute_value(
        &mut self,
        owner: Uuid,
        scene_id: SceneId,
        object_id: ObjectId,
        attribute_name: &str,
        value: AttributeValue,
    ) -> Result<bool, CoreError> {
        if self.playback_owner != Some(owner)
            || !self.is_playback_owned(scene_id, object_id, attribute_name)
        {
            return Ok(false);
        }
        self.set_scene_attribute_value(scene_id, object_id, attribute_name, value)?;
        Ok(true)
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn active_scene(&self) -> Option<SceneId> {
        self.active_scene
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            scenes: self.scenes.clone(),
            active_scene: self.active_scene,
            mode: Some(self.mode),
            mappings: self.mappings.clone(),
        }
    }

    pub fn load_scene(&mut self, scene: Scene) -> SceneId {
        let id = scene.id;
        self.scenes.insert(id, scene);
        if self.active_scene.is_none() {
            self.active_scene = Some(id);
        }
        id
    }

    pub fn set_active_scene(&mut self, scene_id: SceneId) -> Result<(), CoreError> {
        if !self.scenes.contains_key(&scene_id) {
            return Err(CoreError::SceneNotFound(scene_id));
        }
        self.active_scene = Some(scene_id);
        Ok(())
    }

    pub fn set_data_flow(&mut self, state: DataFlowState) -> Result<(), CoreError> {
        if state == DataFlowState::Idle && self.mode.recording != RecordingState::Inactive {
            return Err(CoreError::InvalidModeTransition {
                from: self.mode,
                to: Mode {
                    data_flow: state,
                    recording: self.mode.recording,
                },
            });
        }
        self.mode.data_flow = state;
        Ok(())
    }

    pub fn set_recording(&mut self, state: RecordingState) -> Result<(), CoreError> {
        if state != RecordingState::Inactive && self.mode.data_flow != DataFlowState::Live {
            return Err(CoreError::InvalidModeTransition {
                from: self.mode,
                to: Mode {
                    data_flow: self.mode.data_flow,
                    recording: state,
                },
            });
        }
        self.mode.recording = state;
        Ok(())
    }

    pub fn set_mode(&mut self, mode: Mode) -> Result<(), CoreError> {
        // Order matters: stop recording/playback before reducing data flow,
        // but start data flow before enabling recording/playback.
        if mode.recording == RecordingState::Inactive {
            self.set_recording(mode.recording)?;
            self.set_data_flow(mode.data_flow)?;
        } else {
            self.set_data_flow(mode.data_flow)?;
            self.set_recording(mode.recording)?;
        }
        Ok(())
    }

    pub fn set_scene_attribute_value(
        &mut self,
        scene_id: SceneId,
        object_id: ObjectId,
        attribute_name: &str,
        value: AttributeValue,
    ) -> Result<(), CoreError> {
        let scene = self
            .scenes
            .get_mut(&scene_id)
            .ok_or(CoreError::SceneNotFound(scene_id))?;
        let object = scene
            .objects
            .get_mut(&object_id)
            .ok_or(CoreError::ObjectNotFound(object_id))?;
        let attr = object
            .attributes
            .get_mut(attribute_name)
            .ok_or_else(|| CoreError::AttributeNotFound(attribute_name.to_owned()))?;
        attr.current_value = value;
        Ok(())
    }

    pub fn register_device_connected(&mut self, device_id: Uuid) {
        self.connected_devices.insert(device_id);
    }

    pub fn register_device_disconnected(&mut self, device_id: Uuid, now_ns: u64) {
        self.connected_devices.remove(&device_id);
        for mapping in self.mappings.values_mut() {
            if mapping.source_device == device_id && mapping.state == MappingState::Active {
                mapping.disconnected_at_ns = Some(now_ns);
            }
        }
    }

    pub fn heartbeat(&mut self, device_id: Uuid, now_ns: u64) {
        for mapping in self.mappings.values_mut() {
            if mapping.source_device == device_id && mapping.state == MappingState::Active {
                mapping.last_heartbeat_ns = now_ns;
                mapping.disconnected_at_ns = None;
            }
        }
    }

    pub fn create_mapping(
        &mut self,
        req: MappingRequest,
        now_ns: u64,
    ) -> Result<MappingId, CoreError> {
        if self.mode.recording == RecordingState::Recording {
            return Err(CoreError::MappingBlockedInRecording);
        }

        self.ensure_target_exists(req.target_scene, req.target_object, &req.target_attribute)?;
        self.validate_component_mask(
            req.target_scene,
            req.target_object,
            &req.target_attribute,
            req.component_mask.as_deref(),
        )?;

        // A playback source owning this target is an exclusive owner too: a new
        // mapping cannot claim an attribute an active take is replaying into.
        if self.is_playback_owned(req.target_scene, req.target_object, &req.target_attribute) {
            return Err(CoreError::MappingDenied(
                "target attribute owned by active playback".into(),
            ));
        }

        if let Some(existing_id) =
            self.find_mapping_for_target(req.target_scene, req.target_object, &req.target_attribute)
        {
            let existing = self
                .mappings
                .get(&existing_id)
                .expect("mapping id lookup must succeed");
            if existing.lock {
                return Err(CoreError::MappingDenied("mapping is locked".into()));
            }
            if !self.can_reclaim(existing, now_ns) {
                return Err(CoreError::MappingDenied(
                    "target attribute already owned by active mapping".into(),
                ));
            }
            let _ = self.mappings.shift_remove(&existing_id);
        }

        let id = Uuid::now_v7();
        self.mapping_index
            .insert((req.source_device, req.source_output.clone()), id);
        self.mappings.insert(
            id,
            Mapping {
                id,
                source_device: req.source_device,
                source_output: req.source_output,
                target_scene: req.target_scene,
                target_object: req.target_object,
                target_attribute: req.target_attribute,
                component_mask: req.component_mask,
                lock: false,
                state: MappingState::Active,
                last_heartbeat_ns: now_ns,
                disconnected_at_ns: None,
            },
        );

        Ok(id)
    }

    pub fn set_mapping_lock(&mut self, mapping_id: MappingId, lock: bool) -> Result<(), CoreError> {
        if self.mode.recording == RecordingState::Recording {
            return Err(CoreError::MappingBlockedInRecording);
        }
        let Some(mapping) = self.mappings.get_mut(&mapping_id) else {
            return Err(CoreError::MappingDenied("mapping not found".into()));
        };
        mapping.lock = lock;
        Ok(())
    }

    pub fn remove_mapping(&mut self, mapping_id: MappingId) -> Result<(), CoreError> {
        if self.mode.recording == RecordingState::Recording {
            return Err(CoreError::MappingBlockedInRecording);
        }
        let Some(mapping) = self.mappings.shift_remove(&mapping_id) else {
            return Err(CoreError::MappingDenied("mapping not found".into()));
        };
        self.mapping_index
            .remove(&(mapping.source_device, mapping.source_output));
        Ok(())
    }

    pub fn update_mapping(
        &mut self,
        mapping_id: MappingId,
        req: MappingRequest,
        now_ns: u64,
    ) -> Result<(), CoreError> {
        if self.mode.recording == RecordingState::Recording {
            return Err(CoreError::MappingBlockedInRecording);
        }
        self.ensure_target_exists(req.target_scene, req.target_object, &req.target_attribute)?;
        self.validate_component_mask(
            req.target_scene,
            req.target_object,
            &req.target_attribute,
            req.component_mask.as_deref(),
        )?;
        let Some(mapping) = self.mappings.get_mut(&mapping_id) else {
            return Err(CoreError::MappingDenied("mapping not found".into()));
        };
        if mapping.lock {
            return Err(CoreError::MappingDenied("mapping is locked".into()));
        }

        let old_key = (mapping.source_device, mapping.source_output.clone());
        mapping.source_device = req.source_device;
        mapping.source_output = req.source_output;
        mapping.target_scene = req.target_scene;
        mapping.target_object = req.target_object;
        mapping.target_attribute = req.target_attribute;
        mapping.component_mask = req.component_mask;
        mapping.last_heartbeat_ns = now_ns;
        mapping.disconnected_at_ns = None;

        self.mapping_index.remove(&old_key);
        self.mapping_index.insert(
            (mapping.source_device, mapping.source_output.clone()),
            mapping_id,
        );
        Ok(())
    }

    /// Advance lease bookkeeping. Returns the ids of mappings whose lease was
    /// released on this tick (source device disconnected and lease expired),
    /// so the caller can replicate the release.
    pub fn scheduler_tick(&mut self, now_ns: u64) -> Vec<MappingId> {
        let mut released = Vec::new();
        for mapping in self.mappings.values_mut() {
            if mapping.state != MappingState::Active {
                continue;
            }

            let source_connected = self.connected_devices.contains(&mapping.source_device);
            let expired = now_ns.saturating_sub(mapping.last_heartbeat_ns) >= self.lease.timeout_ns;
            if !source_connected && expired {
                mapping.state = MappingState::Released;
                released.push(mapping.id);
            }
        }
        released
    }

    pub fn apply_updates(
        &mut self,
        device_id: Uuid,
        updates: &[AttributeUpdate],
        now_ns: u64,
    ) -> Result<BTreeMap<String, AttributeValue>, CoreError> {
        self.heartbeat(device_id, now_ns);
        if self.mode.data_flow == DataFlowState::Idle
            || self.mode.recording == RecordingState::Playback
        {
            return Ok(BTreeMap::new());
        }

        let active_scene = self
            .active_scene
            .ok_or_else(|| CoreError::MappingDenied("no active scene".into()))?;

        let mut applied = BTreeMap::new();
        for update in updates {
            // Fast path: O(1) index lookup for exact-match keys.
            let indexed = self
                .mapping_index
                .get(&(device_id, update.output_attribute.clone()))
                .and_then(|id| self.mappings.get(id))
                .filter(|m| m.state == MappingState::Active && m.target_scene == active_scene)
                .cloned();

            let mapping = match indexed {
                Some(m) => m,
                None => self
                    .mappings
                    .values()
                    .find(|m| {
                        m.state == MappingState::Active
                            && m.source_device == device_id
                            && source_output_matches(
                                &m.source_output,
                                device_id,
                                update.output_attribute.as_str(),
                            )
                            && m.target_scene == active_scene
                    })
                    .ok_or_else(|| {
                        CoreError::MappingDenied(format!(
                            "no active mapping for output '{}'",
                            update.output_attribute
                        ))
                    })?
                    .clone(),
            };

            // Arbitration (design §3): a playback source that owns this target
            // (Operator override reclaim) supersedes live ingest. Skip the
            // update so the two owners never fight over one attribute.
            if self.is_playback_owned(
                mapping.target_scene,
                mapping.target_object,
                &mapping.target_attribute,
            ) {
                continue;
            }

            let scene = self
                .scenes
                .get_mut(&mapping.target_scene)
                .ok_or(CoreError::SceneNotFound(mapping.target_scene))?;
            let object = scene
                .objects
                .get_mut(&mapping.target_object)
                .ok_or(CoreError::ObjectNotFound(mapping.target_object))?;
            let attr = object
                .attributes
                .get_mut(&mapping.target_attribute)
                .ok_or_else(|| CoreError::AttributeNotFound(mapping.target_attribute.clone()))?;

            if !attr.live_enabled {
                continue;
            }

            let transformed = if supports_relative_composition(&attr.default_value) {
                let delta = apply_mapping_delta_transform(
                    &attr.default_value,
                    &update.value,
                    mapping.component_mask.as_deref(),
                )?;
                compose_relative_value(&attr.default_value, &delta)?
            } else {
                apply_mapping_transform(
                    &attr.current_value,
                    &update.value,
                    mapping.component_mask.as_deref(),
                )?
            };
            let filtered = apply_filters(&attr.current_value, &transformed, &attr.filter_chain);
            attr.current_value = filtered;
            applied.insert(
                format!("{}.{}", object.name, attr.name),
                attr.current_value.clone(),
            );
        }

        self.tick_count += 1;
        Ok(applied)
    }

    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    pub fn reset_scene_to_baseline(&mut self, scene_id: SceneId) -> Result<u32, CoreError> {
        let scene = self
            .scenes
            .get_mut(&scene_id)
            .ok_or(CoreError::SceneNotFound(scene_id))?;
        let mut changed = 0_u32;
        for object in scene.objects.values_mut() {
            for attribute in object.attributes.values_mut() {
                if attribute.current_value != attribute.default_value {
                    attribute.current_value = attribute.default_value.clone();
                    changed += 1;
                }
            }
        }
        Ok(changed)
    }

    pub fn commit_scene_baseline(&mut self, scene_id: SceneId) -> Result<u32, CoreError> {
        let scene = self
            .scenes
            .get_mut(&scene_id)
            .ok_or(CoreError::SceneNotFound(scene_id))?;
        let mut changed = 0_u32;
        for object in scene.objects.values_mut() {
            for attribute in object.attributes.values_mut() {
                if attribute.default_value != attribute.current_value {
                    attribute.default_value = attribute.current_value.clone();
                    changed += 1;
                }
            }
        }
        Ok(changed)
    }

    pub fn commit_object_baseline(
        &mut self,
        scene_id: SceneId,
        object_id: ObjectId,
    ) -> Result<u32, CoreError> {
        let scene = self
            .scenes
            .get_mut(&scene_id)
            .ok_or(CoreError::SceneNotFound(scene_id))?;
        let object = scene
            .objects
            .get_mut(&object_id)
            .ok_or(CoreError::ObjectNotFound(object_id))?;
        let mut changed = 0_u32;
        for attribute in object.attributes.values_mut() {
            if attribute.default_value != attribute.current_value {
                attribute.default_value = attribute.current_value.clone();
                changed += 1;
            }
        }
        Ok(changed)
    }

    fn ensure_target_exists(
        &self,
        scene_id: SceneId,
        object_id: ObjectId,
        attribute_name: &str,
    ) -> Result<(), CoreError> {
        let scene = self
            .scenes
            .get(&scene_id)
            .ok_or(CoreError::SceneNotFound(scene_id))?;
        let object = scene
            .objects
            .get(&object_id)
            .ok_or(CoreError::ObjectNotFound(object_id))?;
        if !object.attributes.contains_key(attribute_name) {
            return Err(CoreError::AttributeNotFound(attribute_name.to_owned()));
        }
        Ok(())
    }

    fn validate_component_mask(
        &self,
        scene_id: SceneId,
        object_id: ObjectId,
        attribute_name: &str,
        mask: Option<&[usize]>,
    ) -> Result<(), CoreError> {
        let Some(mask) = mask else {
            return Ok(());
        };
        if mask.is_empty() {
            return Err(CoreError::InvalidComponentMask(
                "component_mask cannot be empty".into(),
            ));
        }

        let target = self.target_attribute_value(scene_id, object_id, attribute_name)?;
        if matches!(target, AttributeValue::Mat4f(_)) {
            return Err(CoreError::InvalidComponentMask(
                "component_mask is unsupported for mat4f targets".into(),
            ));
        }
        if let Some(target_len) = vector_len(target) {
            let mut seen = BTreeSet::new();
            for index in mask {
                if *index >= target_len {
                    return Err(CoreError::InvalidComponentMask(format!(
                        "index {} is out of bounds for target length {}",
                        index, target_len
                    )));
                }
                if !seen.insert(*index) {
                    return Err(CoreError::InvalidComponentMask(format!(
                        "index {} appears multiple times",
                        index
                    )));
                }
            }
            return Ok(());
        }

        if mask.len() != 1 {
            return Err(CoreError::InvalidComponentMask(
                "scalar targets require exactly one source component".into(),
            ));
        }
        Ok(())
    }

    fn target_attribute_value(
        &self,
        scene_id: SceneId,
        object_id: ObjectId,
        attribute_name: &str,
    ) -> Result<&AttributeValue, CoreError> {
        let scene = self
            .scenes
            .get(&scene_id)
            .ok_or(CoreError::SceneNotFound(scene_id))?;
        let object = scene
            .objects
            .get(&object_id)
            .ok_or(CoreError::ObjectNotFound(object_id))?;
        let attribute = object
            .attributes
            .get(attribute_name)
            .ok_or_else(|| CoreError::AttributeNotFound(attribute_name.to_owned()))?;
        Ok(&attribute.current_value)
    }

    fn find_mapping_for_target(
        &self,
        scene_id: SceneId,
        object_id: ObjectId,
        attribute_name: &str,
    ) -> Option<MappingId> {
        self.mappings
            .values()
            .find(|m| {
                m.state == MappingState::Active
                    && m.target_scene == scene_id
                    && m.target_object == object_id
                    && m.target_attribute == attribute_name
            })
            .map(|m| m.id)
    }

    fn can_reclaim(&self, mapping: &Mapping, now_ns: u64) -> bool {
        if !self.connected_devices.contains(&mapping.source_device) {
            if let Some(disconnected_at) = mapping.disconnected_at_ns {
                return now_ns.saturating_sub(disconnected_at) >= self.lease.reclaim_grace_ns;
            }
        }

        now_ns.saturating_sub(mapping.last_heartbeat_ns) >= self.lease.timeout_ns
    }
}

fn vector_len(value: &AttributeValue) -> Option<usize> {
    match value {
        AttributeValue::Vec2f(_) => Some(2),
        AttributeValue::Vec3f(_) => Some(3),
        AttributeValue::Vec4f(_) | AttributeValue::Quatf(_) => Some(4),
        _ => None,
    }
}

fn scalar_as_f32(value: &AttributeValue) -> Option<f32> {
    match value {
        AttributeValue::Float32(v) => Some(*v),
        AttributeValue::Float64(v) => Some(*v as f32),
        AttributeValue::Int32(v) => Some(*v as f32),
        _ => None,
    }
}

fn vector_component(value: &AttributeValue, index: usize) -> Option<f32> {
    match value {
        AttributeValue::Vec2f(v) => v.get(index).copied(),
        AttributeValue::Vec3f(v) => v.get(index).copied(),
        AttributeValue::Vec4f(v) => v.get(index).copied(),
        AttributeValue::Quatf(v) => v.get(index).copied(),
        _ => None,
    }
}

fn set_vector_component(
    target: &AttributeValue,
    index: usize,
    value: f32,
) -> Result<AttributeValue, CoreError> {
    match target {
        AttributeValue::Vec2f(v) => {
            let mut next = *v;
            let Some(slot) = next.get_mut(index) else {
                return Err(CoreError::InvalidComponentMask(format!(
                    "index {} out of bounds for vec2f",
                    index
                )));
            };
            *slot = value;
            Ok(AttributeValue::Vec2f(next))
        }
        AttributeValue::Vec3f(v) => {
            let mut next = *v;
            let Some(slot) = next.get_mut(index) else {
                return Err(CoreError::InvalidComponentMask(format!(
                    "index {} out of bounds for vec3f",
                    index
                )));
            };
            *slot = value;
            Ok(AttributeValue::Vec3f(next))
        }
        AttributeValue::Vec4f(v) => {
            let mut next = *v;
            let Some(slot) = next.get_mut(index) else {
                return Err(CoreError::InvalidComponentMask(format!(
                    "index {} out of bounds for vec4f",
                    index
                )));
            };
            *slot = value;
            Ok(AttributeValue::Vec4f(next))
        }
        AttributeValue::Quatf(v) => {
            let mut next = *v;
            let Some(slot) = next.get_mut(index) else {
                return Err(CoreError::InvalidComponentMask(format!(
                    "index {} out of bounds for quatf",
                    index
                )));
            };
            *slot = value;
            Ok(AttributeValue::Quatf(next))
        }
        _ => Err(CoreError::UnsupportedTransform(format!(
            "target '{}' is not vector typed",
            target.type_name()
        ))),
    }
}

fn set_scalar_value(
    target: &AttributeValue,
    source_component: f32,
) -> Result<AttributeValue, CoreError> {
    match target {
        AttributeValue::Float32(_) => Ok(AttributeValue::Float32(source_component)),
        AttributeValue::Float64(_) => Ok(AttributeValue::Float64(source_component as f64)),
        AttributeValue::Int32(_) => Ok(AttributeValue::Int32(source_component.round() as i32)),
        _ => Err(CoreError::UnsupportedTransform(format!(
            "target '{}' is not scalar typed",
            target.type_name()
        ))),
    }
}

fn apply_mapping_transform(
    current_target: &AttributeValue,
    incoming_source: &AttributeValue,
    component_mask: Option<&[usize]>,
) -> Result<AttributeValue, CoreError> {
    match component_mask {
        None => {
            if current_target.type_name() != incoming_source.type_name() {
                return Err(CoreError::TypeMismatch {
                    expected: current_target.type_name(),
                    got: incoming_source.type_name(),
                });
            }
            Ok(incoming_source.clone())
        }
        Some(mask) => {
            if mask.is_empty() {
                return Err(CoreError::InvalidComponentMask(
                    "component_mask cannot be empty".into(),
                ));
            }

            if vector_len(current_target).is_some() {
                let mut transformed = current_target.clone();
                if let Some(value) = scalar_as_f32(incoming_source) {
                    for index in mask {
                        transformed = set_vector_component(&transformed, *index, value)?;
                    }
                    return Ok(transformed);
                }

                if vector_len(incoming_source).is_some() {
                    for index in mask {
                        let Some(component) = vector_component(incoming_source, *index) else {
                            return Err(CoreError::UnsupportedTransform(format!(
                                "source component {} is unavailable on '{}'",
                                index,
                                incoming_source.type_name()
                            )));
                        };
                        transformed = set_vector_component(&transformed, *index, component)?;
                    }
                    return Ok(transformed);
                }

                return Err(CoreError::UnsupportedTransform(format!(
                    "cannot apply source '{}' to vector target '{}'",
                    incoming_source.type_name(),
                    current_target.type_name()
                )));
            }

            if mask.len() != 1 {
                return Err(CoreError::InvalidComponentMask(
                    "scalar targets require exactly one component".into(),
                ));
            }
            let component_index = mask[0];
            let Some(component) = vector_component(incoming_source, component_index) else {
                return Err(CoreError::UnsupportedTransform(format!(
                    "source '{}' does not have component {}",
                    incoming_source.type_name(),
                    component_index
                )));
            };
            set_scalar_value(current_target, component)
        }
    }
}

fn apply_mapping_delta_transform(
    baseline_target: &AttributeValue,
    incoming_source: &AttributeValue,
    component_mask: Option<&[usize]>,
) -> Result<AttributeValue, CoreError> {
    match component_mask {
        None => {
            if baseline_target.type_name() != incoming_source.type_name() {
                return Err(CoreError::TypeMismatch {
                    expected: baseline_target.type_name(),
                    got: incoming_source.type_name(),
                });
            }
            Ok(incoming_source.clone())
        }
        Some(mask) => {
            if mask.is_empty() {
                return Err(CoreError::InvalidComponentMask(
                    "component_mask cannot be empty".into(),
                ));
            }

            if vector_len(baseline_target).is_some() {
                let mut transformed = default_delta_for_target(baseline_target)?;
                if let Some(value) = scalar_as_f32(incoming_source) {
                    for index in mask {
                        transformed = set_vector_component(&transformed, *index, value)?;
                    }
                    return Ok(transformed);
                }

                if vector_len(incoming_source).is_some() {
                    for index in mask {
                        let Some(component) = vector_component(incoming_source, *index) else {
                            return Err(CoreError::UnsupportedTransform(format!(
                                "source component {} is unavailable on '{}'",
                                index,
                                incoming_source.type_name()
                            )));
                        };
                        transformed = set_vector_component(&transformed, *index, component)?;
                    }
                    return Ok(transformed);
                }

                return Err(CoreError::UnsupportedTransform(format!(
                    "cannot apply source '{}' to vector target '{}'",
                    incoming_source.type_name(),
                    baseline_target.type_name()
                )));
            }

            if is_scalar_target(baseline_target) {
                if mask.len() != 1 {
                    return Err(CoreError::InvalidComponentMask(
                        "scalar targets require exactly one component".into(),
                    ));
                }
                let component_index = mask[0];
                let Some(component) = vector_component(incoming_source, component_index) else {
                    return Err(CoreError::UnsupportedTransform(format!(
                        "source '{}' does not have component {}",
                        incoming_source.type_name(),
                        component_index
                    )));
                };
                return set_scalar_value(baseline_target, component);
            }

            Err(CoreError::UnsupportedTransform(format!(
                "component_mask is unsupported for '{}' targets",
                baseline_target.type_name()
            )))
        }
    }
}

fn default_delta_for_target(target: &AttributeValue) -> Result<AttributeValue, CoreError> {
    match target {
        AttributeValue::Vec2f(_) => Ok(AttributeValue::Vec2f([0.0, 0.0])),
        AttributeValue::Vec3f(_) => Ok(AttributeValue::Vec3f([0.0, 0.0, 0.0])),
        AttributeValue::Vec4f(_) => Ok(AttributeValue::Vec4f([0.0, 0.0, 0.0, 0.0])),
        // Identity quaternion keeps unmasked components stable as a neutral relative rotation.
        AttributeValue::Quatf(_) => Ok(AttributeValue::Quatf([0.0, 0.0, 0.0, 1.0])),
        _ => Err(CoreError::UnsupportedTransform(format!(
            "cannot synthesize delta for target '{}'",
            target.type_name()
        ))),
    }
}

fn supports_relative_composition(target: &AttributeValue) -> bool {
    matches!(
        target,
        AttributeValue::Int32(_)
            | AttributeValue::Float32(_)
            | AttributeValue::Float64(_)
            | AttributeValue::Vec2f(_)
            | AttributeValue::Vec3f(_)
            | AttributeValue::Vec4f(_)
            | AttributeValue::Quatf(_)
            | AttributeValue::Mat4f(_)
    )
}

fn is_scalar_target(target: &AttributeValue) -> bool {
    matches!(
        target,
        AttributeValue::Int32(_) | AttributeValue::Float32(_) | AttributeValue::Float64(_)
    )
}

fn compose_relative_value(
    baseline: &AttributeValue,
    delta: &AttributeValue,
) -> Result<AttributeValue, CoreError> {
    match (baseline, delta) {
        (AttributeValue::Int32(base), AttributeValue::Int32(d)) => {
            Ok(AttributeValue::Int32(base.saturating_add(*d)))
        }
        (AttributeValue::Float32(base), AttributeValue::Float32(d)) => {
            Ok(AttributeValue::Float32(base + d))
        }
        (AttributeValue::Float64(base), AttributeValue::Float64(d)) => {
            Ok(AttributeValue::Float64(base + d))
        }
        (AttributeValue::Vec2f(base), AttributeValue::Vec2f(d)) => {
            Ok(AttributeValue::Vec2f([base[0] + d[0], base[1] + d[1]]))
        }
        (AttributeValue::Vec3f(base), AttributeValue::Vec3f(d)) => Ok(AttributeValue::Vec3f([
            base[0] + d[0],
            base[1] + d[1],
            base[2] + d[2],
        ])),
        (AttributeValue::Vec4f(base), AttributeValue::Vec4f(d)) => Ok(AttributeValue::Vec4f([
            base[0] + d[0],
            base[1] + d[1],
            base[2] + d[2],
            base[3] + d[3],
        ])),
        (AttributeValue::Quatf(base), AttributeValue::Quatf(d)) => {
            Ok(AttributeValue::Quatf(quat_multiply_normalized(*base, *d)))
        }
        (AttributeValue::Mat4f(base), AttributeValue::Mat4f(d)) => {
            Ok(AttributeValue::Mat4f(mat4_multiply(*base, *d)))
        }
        _ => Err(CoreError::TypeMismatch {
            expected: baseline.type_name(),
            got: delta.type_name(),
        }),
    }
}

fn quat_multiply_normalized(base: [f32; 4], delta: [f32; 4]) -> [f32; 4] {
    let [x1, y1, z1, w1] = base;
    let [x2, y2, z2, w2] = delta;
    let result = [
        (w1 * x2) + (x1 * w2) + (y1 * z2) - (z1 * y2),
        (w1 * y2) - (x1 * z2) + (y1 * w2) + (z1 * x2),
        (w1 * z2) + (x1 * y2) - (y1 * x2) + (z1 * w2),
        (w1 * w2) - (x1 * x2) - (y1 * y2) - (z1 * z2),
    ];
    let norm = result.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return base;
    }
    [
        result[0] / norm,
        result[1] / norm,
        result[2] / norm,
        result[3] / norm,
    ]
}

fn mat4_multiply(base: [[f32; 4]; 4], delta: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for row in 0..4 {
        for col in 0..4 {
            out[row][col] = (0..4).map(|k| base[row][k] * delta[k][col]).sum();
        }
    }
    out
}

pub fn source_output_matches(mapping_output: &str, device_id: Uuid, update_output: &str) -> bool {
    if mapping_output == update_output {
        return true;
    }
    let prefix = format!("{device_id}.");
    if let Some(stripped) = mapping_output.strip_prefix(&prefix) {
        if stripped == update_output {
            return true;
        }
    }
    if let Some(stripped) = update_output.strip_prefix(&prefix) {
        if mapping_output == stripped {
            return true;
        }
    }
    false
}

fn apply_filters(
    previous: &AttributeValue,
    incoming: &AttributeValue,
    filters: &[AttributeFilter],
) -> AttributeValue {
    let mut value = incoming.clone();
    for filter in filters {
        value = apply_filter(previous, &value, filter);
    }
    value
}

fn apply_filter(
    previous: &AttributeValue,
    current: &AttributeValue,
    filter: &AttributeFilter,
) -> AttributeValue {
    match filter {
        AttributeFilter::Passthrough => current.clone(),
        AttributeFilter::Ema { alpha } => apply_ema(previous, current, *alpha),
        AttributeFilter::Deadband { threshold } => apply_deadband(previous, current, *threshold),
        AttributeFilter::Clamp { min, max } => apply_clamp(current, *min, *max),
    }
}

fn apply_ema(previous: &AttributeValue, current: &AttributeValue, alpha: f32) -> AttributeValue {
    let alpha = alpha.clamp(0.0, 1.0);
    match (previous, current) {
        (AttributeValue::Float32(prev), AttributeValue::Float32(curr)) => {
            AttributeValue::Float32((alpha * curr) + ((1.0 - alpha) * prev))
        }
        (AttributeValue::Float64(prev), AttributeValue::Float64(curr)) => {
            let alpha64 = alpha as f64;
            AttributeValue::Float64((alpha64 * curr) + ((1.0 - alpha64) * prev))
        }
        (AttributeValue::Vec3f(prev), AttributeValue::Vec3f(curr)) => AttributeValue::Vec3f([
            (alpha * curr[0]) + ((1.0 - alpha) * prev[0]),
            (alpha * curr[1]) + ((1.0 - alpha) * prev[1]),
            (alpha * curr[2]) + ((1.0 - alpha) * prev[2]),
        ]),
        _ => current.clone(),
    }
}

fn apply_deadband(
    previous: &AttributeValue,
    current: &AttributeValue,
    threshold: f32,
) -> AttributeValue {
    let threshold = threshold.max(0.0);
    match (previous, current) {
        (AttributeValue::Float32(prev), AttributeValue::Float32(curr)) => {
            if (curr - prev).abs() < threshold {
                AttributeValue::Float32(*prev)
            } else {
                current.clone()
            }
        }
        (AttributeValue::Float64(prev), AttributeValue::Float64(curr)) => {
            if (curr - prev).abs() < threshold as f64 {
                AttributeValue::Float64(*prev)
            } else {
                current.clone()
            }
        }
        (AttributeValue::Vec3f(prev), AttributeValue::Vec3f(curr)) => {
            let diff =
                ((curr[0] - prev[0]).abs() + (curr[1] - prev[1]).abs() + (curr[2] - prev[2]).abs())
                    / 3.0;
            if diff < threshold {
                AttributeValue::Vec3f(*prev)
            } else {
                current.clone()
            }
        }
        _ => current.clone(),
    }
}

fn apply_clamp(current: &AttributeValue, min: f32, max: f32) -> AttributeValue {
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
    match current {
        AttributeValue::Float32(v) => AttributeValue::Float32(v.clamp(lo, hi)),
        AttributeValue::Float64(v) => AttributeValue::Float64(v.clamp(lo as f64, hi as f64)),
        AttributeValue::Vec3f(v) => {
            AttributeValue::Vec3f([v[0].clamp(lo, hi), v[1].clamp(lo, hi), v[2].clamp(lo, hi)])
        }
        _ => current.clone(),
    }
}

#[cfg(test)]
mod tests {
    use motionstage_protocol::Mode;
    #[allow(unused_imports)]
    use motionstage_protocol::{DataFlowState, RecordingState};
    use uuid::Uuid;

    use crate::{
        AttributeFilter, AttributeUpdate, AttributeValue, MappingRequest, RuntimeCore, Scene,
        SceneAttribute, SceneObject,
    };

    fn build_core() -> (RuntimeCore, Uuid, Uuid, Uuid) {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);
        (core, device_id, scene_id, object_id)
    }

    #[test]
    fn live_mode_applies_mapped_updates() {
        let (mut core, device_id, scene_id, object_id) = build_core();
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            100,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();

        let updates = vec![AttributeUpdate {
            output_attribute: "pose_pos".into(),
            value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
        }];

        let applied = core.apply_updates(device_id, &updates, 200).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(core.tick_count(), 1);
    }

    #[test]
    fn qualified_mapping_matches_unqualified_update_for_same_device() {
        let (mut core, device_id, scene_id, object_id) = build_core();
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: format!("{device_id}.pose_pos"),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            100,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();

        let updates = vec![AttributeUpdate {
            output_attribute: "pose_pos".into(),
            value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
        }];
        let applied = core.apply_updates(device_id, &updates, 200).unwrap();
        assert_eq!(applied.len(), 1);
    }

    #[test]
    fn unqualified_mapping_matches_qualified_update_for_same_device() {
        let (mut core, device_id, scene_id, object_id) = build_core();
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            100,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();

        let updates = vec![AttributeUpdate {
            output_attribute: format!("{device_id}.pose_pos"),
            value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
        }];
        let applied = core.apply_updates(device_id, &updates, 200).unwrap();
        assert_eq!(applied.len(), 1);
    }

    #[test]
    fn unmapped_updates_are_rejected() {
        let (mut core, device_id, _, _) = build_core();
        core.set_mode(Mode::LIVE).unwrap();

        let updates = vec![AttributeUpdate {
            output_attribute: "missing".into(),
            value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
        }];

        let err = core.apply_updates(device_id, &updates, 300).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no active mapping"));
    }

    #[test]
    fn recording_blocks_mapping_changes() {
        let (mut core, device_id, scene_id, object_id) = build_core();
        core.set_mode(Mode::LIVE).unwrap();
        core.set_mode(Mode::RECORDING).unwrap();

        let err = core
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                100,
            )
            .unwrap_err();

        assert!(format!("{err}").contains("blocked in recording"));
    }

    #[test]
    fn connected_owner_blocks_mapping_conflict() {
        let (mut core, device_a, scene_id, object_id) = build_core();
        let device_b = Uuid::now_v7();
        core.register_device_connected(device_b);

        core.create_mapping(
            MappingRequest {
                source_device: device_a,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            100,
        )
        .unwrap();

        let err = core
            .create_mapping(
                MappingRequest {
                    source_device: device_b,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                200,
            )
            .unwrap_err();

        assert!(format!("{err}").contains("already owned"));
    }

    #[test]
    fn disconnected_mapping_can_be_reclaimed_after_grace_period() {
        let (mut core, device_a, scene_id, object_id) = build_core();
        let device_b = Uuid::now_v7();
        core.register_device_connected(device_b);

        core.create_mapping(
            MappingRequest {
                source_device: device_a,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            100,
        )
        .unwrap();
        core.register_device_disconnected(device_a, 200);

        let early_err = core
            .create_mapping(
                MappingRequest {
                    source_device: device_b,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                5_000_000_100,
            )
            .unwrap_err();
        assert!(format!("{early_err}").contains("already owned"));

        let mapping_id = core
            .create_mapping(
                MappingRequest {
                    source_device: device_b,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: None,
                },
                5_000_000_201,
            )
            .unwrap();

        assert!(core.snapshot().mappings.contains_key(&mapping_id));
    }

    #[test]
    fn filter_chain_applies_deadband_and_clamp() {
        let (mut core, device_id, scene_id, object_id) = build_core();
        {
            let scene = core.scenes.get_mut(&scene_id).unwrap();
            let object = scene.objects.get_mut(&object_id).unwrap();
            let attr = object.attributes.get_mut("position").unwrap();
            attr.filter_chain = vec![
                AttributeFilter::Deadband { threshold: 0.2 },
                AttributeFilter::Clamp {
                    min: -1.0,
                    max: 1.0,
                },
            ];
        }
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();

        let small = vec![AttributeUpdate {
            output_attribute: "pose_pos".into(),
            value: AttributeValue::Vec3f([0.1, 0.1, 0.1]),
        }];
        core.apply_updates(device_id, &small, 2).unwrap();
        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("position")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Vec3f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current, [0.0, 0.0, 0.0]);

        let large = vec![AttributeUpdate {
            output_attribute: "pose_pos".into(),
            value: AttributeValue::Vec3f([2.0, -2.0, 0.5]),
        }];
        core.apply_updates(device_id, &large, 3).unwrap();
        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("position")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Vec3f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current, [1.0, -1.0, 0.5]);
    }

    #[test]
    fn scalar_updates_single_vector_component_with_mask() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "focus_vec",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);

        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "focus_scalar".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "focus_vec".into(),
                component_mask: Some(vec![0]),
            },
            10,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "focus_scalar".into(),
                value: AttributeValue::Float32(2.5),
            }],
            11,
        )
        .unwrap();

        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("focus_vec")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Vec3f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current, [2.5, 0.0, 0.0]);
    }

    #[test]
    fn vector_component_can_drive_scalar_target() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera")
            .with_attribute(SceneAttribute::new("focus", AttributeValue::Float32(0.0)));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);

        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "focus".into(),
                component_mask: Some(vec![1]),
            },
            20,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([1.0, 4.0, 9.0]),
            }],
            21,
        )
        .unwrap();

        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("focus")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Float32(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current, 4.0);
    }

    #[test]
    fn invalid_component_mask_is_rejected_at_mapping_creation() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([0.0, 0.0, 0.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);

        let err = core
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_pos".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "position".into(),
                    component_mask: Some(vec![3]),
                },
                30,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("out of bounds"));
    }

    #[test]
    fn vector_subset_copy_preserves_unmasked_components() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([10.0, 20.0, 30.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);

        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: Some(vec![0, 2]),
            },
            40,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
            }],
            41,
        )
        .unwrap();

        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("position")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Vec3f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current, [11.0, 20.0, 33.0]);
    }

    #[test]
    fn relative_updates_are_composed_from_baseline() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([10.0, 20.0, 30.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
            }],
            2,
        )
        .unwrap();

        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("position")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Vec3f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current, [11.0, 22.0, 33.0]);
    }

    #[test]
    fn explicit_scene_reset_restores_defaults() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([10.0, 20.0, 30.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
            }],
            2,
        )
        .unwrap();
        core.set_mode(Mode::IDLE).unwrap();
        let current_before_reset = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("position")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Vec3f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current_before_reset, [11.0, 22.0, 33.0]);

        let changed = core.reset_scene_to_baseline(scene_id).unwrap();
        assert_eq!(changed, 1);
        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("position")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Vec3f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current, [10.0, 20.0, 30.0]);
    }

    #[test]
    fn commit_scene_and_object_baselines_update_defaults_explicitly() {
        let mut core = RuntimeCore::default();
        let camera = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([10.0, 20.0, 30.0]),
        ));
        let camera_id = camera.id;
        let light = SceneObject::new("light").with_attribute(SceneAttribute::new(
            "position",
            AttributeValue::Vec3f([1.0, 2.0, 3.0]),
        ));
        let light_id = light.id;
        let scene = Scene::new("shot").with_object(camera).with_object(light);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: camera_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
            }],
            2,
        )
        .unwrap();

        let object_changed = core.commit_object_baseline(scene_id, camera_id).unwrap();
        assert_eq!(object_changed, 1);
        let light_default = core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&light_id)
            .unwrap()
            .attributes
            .get("position")
            .unwrap()
            .default_value
            .clone();
        assert_eq!(light_default, AttributeValue::Vec3f([1.0, 2.0, 3.0]));

        let scene_changed = core.commit_scene_baseline(scene_id).unwrap();
        assert_eq!(scene_changed, 0);
    }

    #[test]
    fn quat_relative_composition_multiplies_and_normalizes() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "rotation",
            AttributeValue::Quatf([0.0, 0.0, 0.0, 1.0]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_rot".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "rotation".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();

        // 90 degrees around Z
        let half_angle = std::f32::consts::FRAC_PI_4;
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_rot".into(),
                value: AttributeValue::Quatf([0.0, 0.0, half_angle.sin(), half_angle.cos()]),
            }],
            2,
        )
        .unwrap();
        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("rotation")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Quatf(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert!((current[2] - half_angle.sin()).abs() < 1e-4);
        assert!((current[3] - half_angle.cos()).abs() < 1e-4);
    }

    #[test]
    fn mat4_relative_composition_multiplies_base_by_delta() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "transform",
            AttributeValue::Mat4f([
                [1.0, 0.0, 0.0, 10.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_xform".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "transform".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_xform".into(),
                value: AttributeValue::Mat4f([
                    [1.0, 0.0, 0.0, 1.0],
                    [0.0, 1.0, 0.0, 2.0],
                    [0.0, 0.0, 1.0, 3.0],
                    [0.0, 0.0, 0.0, 1.0],
                ]),
            }],
            2,
        )
        .unwrap();
        let current = match core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("transform")
            .unwrap()
            .current_value
            .clone()
        {
            AttributeValue::Mat4f(v) => v,
            _ => panic!("unexpected value type"),
        };
        assert_eq!(current[0][3], 11.0);
        assert_eq!(current[1][3], 2.0);
        assert_eq!(current[2][3], 3.0);
    }

    #[test]
    fn component_mask_is_rejected_for_mat4_targets() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
            "transform",
            AttributeValue::Mat4f([
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ]),
        ));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);

        let err = core
            .create_mapping(
                MappingRequest {
                    source_device: device_id,
                    source_output: "pose_xform".into(),
                    target_scene: scene_id,
                    target_object: object_id,
                    target_attribute: "transform".into(),
                    component_mask: Some(vec![0]),
                },
                1,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("unsupported for mat4f"));
    }

    #[test]
    fn non_composable_types_fall_back_to_absolute_transform() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera")
            .with_attribute(SceneAttribute::new("enabled", AttributeValue::Bool(false)));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "enabled".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "enabled".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "enabled".into(),
                value: AttributeValue::Bool(true),
            }],
            2,
        )
        .unwrap();
        let current = core
            .scenes
            .get(&scene_id)
            .unwrap()
            .objects
            .get(&object_id)
            .unwrap()
            .attributes
            .get("enabled")
            .unwrap()
            .current_value
            .clone();
        assert_eq!(current, AttributeValue::Bool(true));
    }

    #[test]
    fn playback_acquires_free_targets_and_is_blocked_by_live_mappings() {
        let (mut core, device_id, scene_id, object_id) = build_core();
        // A live mapping owns "position".
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();

        let playback = Uuid::now_v7();
        let acq = core.acquire_playback_targets(
            playback,
            scene_id,
            [(object_id, "position".to_string())],
            false,
        );
        assert!(acq.owned.is_empty());
        assert_eq!(acq.blocked, vec![(object_id, "position".to_string())]);
        assert!(!core.is_playback_owned(scene_id, object_id, "position"));

        // Override reclaims it.
        let acq = core.acquire_playback_targets(
            playback,
            scene_id,
            [(object_id, "position".to_string())],
            true,
        );
        assert_eq!(acq.owned, vec![(object_id, "position".to_string())]);
        assert!(acq.blocked.is_empty());
        assert!(core.is_playback_owned(scene_id, object_id, "position"));
    }

    #[test]
    fn owned_setter_writes_only_owned_targets_and_release_frees_them() {
        let (mut core, _device_id, scene_id, object_id) = build_core();
        let playback = Uuid::now_v7();
        core.acquire_playback_targets(
            playback,
            scene_id,
            [(object_id, "position".to_string())],
            false,
        );

        // Owned write lands.
        let wrote = core
            .set_owned_scene_attribute_value(
                playback,
                scene_id,
                object_id,
                "position",
                AttributeValue::Vec3f([7.0, 8.0, 9.0]),
            )
            .unwrap();
        assert!(wrote);
        assert_eq!(
            core.scenes
                .get(&scene_id)
                .unwrap()
                .objects
                .get(&object_id)
                .unwrap()
                .attributes
                .get("position")
                .unwrap()
                .current_value,
            AttributeValue::Vec3f([7.0, 8.0, 9.0])
        );

        // A non-owner (or unowned attribute) writes nothing.
        let other = Uuid::now_v7();
        let wrote = core
            .set_owned_scene_attribute_value(
                other,
                scene_id,
                object_id,
                "position",
                AttributeValue::Vec3f([0.0, 0.0, 0.0]),
            )
            .unwrap();
        assert!(!wrote);

        core.release_playback(playback);
        assert!(core.playback_owner().is_none());
        assert!(!core.is_playback_owned(scene_id, object_id, "position"));
    }

    #[test]
    fn playback_owned_target_supersedes_live_ingest() {
        let (mut core, device_id, scene_id, object_id) = build_core();
        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            1,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();

        let playback = Uuid::now_v7();
        core.acquire_playback_targets(
            playback,
            scene_id,
            [(object_id, "position".to_string())],
            true,
        );
        core.set_owned_scene_attribute_value(
            playback,
            scene_id,
            object_id,
            "position",
            AttributeValue::Vec3f([1.0, 1.0, 1.0]),
        )
        .unwrap();

        // Live device ingest is skipped for the reclaimed attribute.
        core.apply_updates(
            device_id,
            &[AttributeUpdate {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([50.0, 50.0, 50.0]),
            }],
            2,
        )
        .unwrap();
        assert_eq!(
            core.scenes
                .get(&scene_id)
                .unwrap()
                .objects
                .get(&object_id)
                .unwrap()
                .attributes
                .get("position")
                .unwrap()
                .current_value,
            AttributeValue::Vec3f([1.0, 1.0, 1.0])
        );
    }

    #[test]
    fn invalid_transform_pair_is_rejected_deterministically() {
        let mut core = RuntimeCore::default();
        let object = SceneObject::new("camera")
            .with_attribute(SceneAttribute::new("focus", AttributeValue::Float32(0.0)));
        let object_id = object.id;
        let scene = Scene::new("shot").with_object(object);
        let scene_id = scene.id;
        core.load_scene(scene);
        let device_id = Uuid::now_v7();
        core.register_device_connected(device_id);

        core.create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "focus_scalar".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "focus".into(),
                component_mask: Some(vec![0]),
            },
            50,
        )
        .unwrap();
        core.set_mode(Mode::LIVE).unwrap();
        let err = core
            .apply_updates(
                device_id,
                &[AttributeUpdate {
                    output_attribute: "focus_scalar".into(),
                    value: AttributeValue::Float32(1.0),
                }],
                51,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("unsupported mapping transform"));
    }
}
