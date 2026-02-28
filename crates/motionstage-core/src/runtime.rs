use std::collections::{BTreeMap, BTreeSet};

use bevy_ecs::{prelude::Resource, world::World};
use indexmap::IndexMap;
use motionstage_protocol::Mode;
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

#[derive(Resource, Default)]
struct RuntimeStats {
    tick_count: u64,
}

#[derive(Debug)]
pub struct RuntimeCore {
    world: World,
    scenes: IndexMap<SceneId, Scene>,
    active_scene: Option<SceneId>,
    mode: Mode,
    mappings: IndexMap<MappingId, Mapping>,
    lease: LeaseConfig,
    connected_devices: BTreeSet<Uuid>,
}

impl Default for RuntimeCore {
    fn default() -> Self {
        Self::new(LeaseConfig::default())
    }
}

impl RuntimeCore {
    pub fn new(lease: LeaseConfig) -> Self {
        let mut world = World::new();
        world.insert_resource(RuntimeStats::default());

        Self {
            world,
            scenes: IndexMap::new(),
            active_scene: None,
            mode: Mode::Idle,
            mappings: IndexMap::new(),
            lease,
            connected_devices: BTreeSet::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
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

    pub fn set_mode(&mut self, mode: Mode) -> Result<(), CoreError> {
        let valid = matches!(
            (self.mode, mode),
            (Mode::Idle, Mode::Live)
                | (Mode::Live, Mode::Idle)
                | (Mode::Live, Mode::Recording)
                | (Mode::Recording, Mode::Live)
                | (Mode::Recording, Mode::Idle)
                | (Mode::Idle, Mode::Idle)
                | (Mode::Live, Mode::Live)
                | (Mode::Recording, Mode::Recording)
        );

        if !valid {
            return Err(CoreError::InvalidModeTransition {
                from: self.mode,
                to: mode,
            });
        }

        self.mode = mode;
        if self.mode == Mode::Idle {
            self.reset_to_defaults();
        }

        Ok(())
    }

    fn reset_to_defaults(&mut self) {
        for scene in self.scenes.values_mut() {
            for object in scene.objects.values_mut() {
                for attr in object.attributes.values_mut() {
                    attr.reset();
                }
            }
        }
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
        if self.mode == Mode::Recording {
            return Err(CoreError::MappingBlockedInRecording);
        }

        self.ensure_target_exists(req.target_scene, req.target_object, &req.target_attribute)?;
        self.validate_component_mask(
            req.target_scene,
            req.target_object,
            &req.target_attribute,
            req.component_mask.as_deref(),
        )?;

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
        if self.mode == Mode::Recording {
            return Err(CoreError::MappingBlockedInRecording);
        }
        let Some(mapping) = self.mappings.get_mut(&mapping_id) else {
            return Err(CoreError::MappingDenied("mapping not found".into()));
        };
        mapping.lock = lock;
        Ok(())
    }

    pub fn remove_mapping(&mut self, mapping_id: MappingId) -> Result<(), CoreError> {
        if self.mode == Mode::Recording {
            return Err(CoreError::MappingBlockedInRecording);
        }
        let Some(_) = self.mappings.shift_remove(&mapping_id) else {
            return Err(CoreError::MappingDenied("mapping not found".into()));
        };
        Ok(())
    }

    pub fn update_mapping(
        &mut self,
        mapping_id: MappingId,
        req: MappingRequest,
        now_ns: u64,
    ) -> Result<(), CoreError> {
        if self.mode == Mode::Recording {
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

        mapping.source_device = req.source_device;
        mapping.source_output = req.source_output;
        mapping.target_scene = req.target_scene;
        mapping.target_object = req.target_object;
        mapping.target_attribute = req.target_attribute;
        mapping.component_mask = req.component_mask;
        mapping.last_heartbeat_ns = now_ns;
        mapping.disconnected_at_ns = None;
        Ok(())
    }

    pub fn scheduler_tick(&mut self, now_ns: u64) {
        for mapping in self.mappings.values_mut() {
            if mapping.state != MappingState::Active {
                continue;
            }

            let source_connected = self.connected_devices.contains(&mapping.source_device);
            let expired = now_ns.saturating_sub(mapping.last_heartbeat_ns) >= self.lease.timeout_ns;
            if !source_connected && expired {
                mapping.state = MappingState::Released;
            }
        }
    }

    pub fn apply_updates(
        &mut self,
        device_id: Uuid,
        updates: &[AttributeUpdate],
        now_ns: u64,
    ) -> Result<BTreeMap<String, AttributeValue>, CoreError> {
        self.heartbeat(device_id, now_ns);
        if self.mode == Mode::Idle {
            return Ok(BTreeMap::new());
        }

        let active_scene = self
            .active_scene
            .ok_or_else(|| CoreError::MappingDenied("no active scene".into()))?;

        let mut applied = BTreeMap::new();
        for update in updates {
            let mapping = self
                .mappings
                .values()
                .find(|m| {
                    m.state == MappingState::Active
                        && m.source_device == device_id
                        && m.source_output == update.output_attribute
                        && m.target_scene == active_scene
                })
                .ok_or_else(|| {
                    CoreError::MappingDenied(format!(
                        "no active mapping for output '{}'",
                        update.output_attribute
                    ))
                })?
                .clone();

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

            let transformed = apply_mapping_transform(
                &attr.current_value,
                &update.value,
                mapping.component_mask.as_deref(),
            )?;
            let filtered = apply_filters(&attr.current_value, &transformed, &attr.filter_chain);
            attr.current_value = filtered;
            applied.insert(
                format!("{}.{}", object.name, attr.name),
                attr.current_value.clone(),
            );
        }

        self.world.resource_mut::<RuntimeStats>().tick_count += 1;
        Ok(applied)
    }

    pub fn tick_count(&self) -> u64 {
        self.world.resource::<RuntimeStats>().tick_count
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
        core.set_mode(Mode::Live).unwrap();

        let updates = vec![AttributeUpdate {
            output_attribute: "pose_pos".into(),
            value: AttributeValue::Vec3f([1.0, 2.0, 3.0]),
        }];

        let applied = core.apply_updates(device_id, &updates, 200).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(core.tick_count(), 1);
    }

    #[test]
    fn unmapped_updates_are_rejected() {
        let (mut core, device_id, _, _) = build_core();
        core.set_mode(Mode::Live).unwrap();

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
        core.set_mode(Mode::Live).unwrap();
        core.set_mode(Mode::Recording).unwrap();

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
        core.set_mode(Mode::Live).unwrap();

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
        core.set_mode(Mode::Live).unwrap();
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
        core.set_mode(Mode::Live).unwrap();
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
        core.set_mode(Mode::Live).unwrap();
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
        assert_eq!(current, [1.0, 20.0, 3.0]);
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
        core.set_mode(Mode::Live).unwrap();
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
