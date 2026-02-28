use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub type SceneId = Uuid;
pub type ObjectId = Uuid;
pub type MappingId = Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttributeValue {
    Bool(bool),
    Int32(i32),
    Float32(f32),
    Float64(f64),
    Vec2f([f32; 2]),
    Vec3f([f32; 3]),
    Vec4f([f32; 4]),
    Quatf([f32; 4]),
    Mat4f([[f32; 4]; 4]),
    Trigger(bool),
}

impl AttributeValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Int32(_) => "int32",
            Self::Float32(_) => "float32",
            Self::Float64(_) => "float64",
            Self::Vec2f(_) => "vec2f",
            Self::Vec3f(_) => "vec3f",
            Self::Vec4f(_) => "vec4f",
            Self::Quatf(_) => "quatf",
            Self::Mat4f(_) => "mat4f",
            Self::Trigger(_) => "trigger",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneAttribute {
    pub name: String,
    pub default_value: AttributeValue,
    pub current_value: AttributeValue,
    pub live_enabled: bool,
    pub record_enabled: bool,
    pub filter_chain: Vec<AttributeFilter>,
}

impl SceneAttribute {
    pub fn new(name: impl Into<String>, default_value: AttributeValue) -> Self {
        let default_value_clone = default_value.clone();
        Self {
            name: name.into(),
            default_value,
            current_value: default_value_clone,
            live_enabled: true,
            record_enabled: true,
            filter_chain: vec![AttributeFilter::Passthrough],
        }
    }

    pub fn reset(&mut self) {
        self.current_value = self.default_value.clone();
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttributeFilter {
    Passthrough,
    Ema { alpha: f32 },
    Deadband { threshold: f32 },
    Clamp { min: f32, max: f32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneObject {
    pub id: ObjectId,
    pub name: String,
    pub attributes: BTreeMap<String, SceneAttribute>,
}

impl SceneObject {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::now_v7(),
            name: name.into(),
            attributes: BTreeMap::new(),
        }
    }

    pub fn with_attribute(mut self, attr: SceneAttribute) -> Self {
        self.attributes.insert(attr.name.clone(), attr);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub id: SceneId,
    pub name: String,
    pub objects: BTreeMap<ObjectId, SceneObject>,
}

impl Scene {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::now_v7(),
            name: name.into(),
            objects: BTreeMap::new(),
        }
    }

    pub fn with_object(mut self, object: SceneObject) -> Self {
        self.objects.insert(object.id, object);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MappingState {
    Active,
    Released,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    pub id: MappingId,
    pub source_device: Uuid,
    pub source_output: String,
    pub target_scene: SceneId,
    pub target_object: ObjectId,
    pub target_attribute: String,
    pub component_mask: Option<Vec<usize>>,
    pub lock: bool,
    pub state: MappingState,
    pub last_heartbeat_ns: u64,
    pub disconnected_at_ns: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappingRequest {
    pub source_device: Uuid,
    pub source_output: String,
    pub target_scene: SceneId,
    pub target_object: ObjectId,
    pub target_attribute: String,
    pub component_mask: Option<Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributeUpdate {
    pub output_attribute: String,
    pub value: AttributeValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseConfig {
    pub heartbeat_interval_ns: u64,
    pub timeout_ns: u64,
    pub reclaim_grace_ns: u64,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_ns: 500_000_000,
            timeout_ns: 2_000_000_000,
            reclaim_grace_ns: 5_000_000_000,
        }
    }
}
