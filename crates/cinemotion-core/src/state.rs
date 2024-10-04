use std::collections::HashMap;

use crate::devices::Device;
use crate::scene::{SceneInfo, SceneObject};
use crate::world::World;
use crate::{devices, globals, scene};

#[derive(Debug, Clone)]
pub struct GlobalState {
    /// The time this state was generated
    pub utime: u128,
    /// The current session state
    pub session: SessionState,

    /// The current state of devices mapped by their id to their state.
    pub devices: HashMap<u32, Device>,

    /// The current state of the scene
    pub scene: SceneState,
}

impl GlobalState {
    pub fn new(world: &mut World) -> Self {
        let utime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let session = SessionState {
            motion_enabled: globals::system::is_motion_enabled(world),
        };

        let mut devices = HashMap::new();
        for device_ref in devices::system::get_all(world) {
            devices.insert(*(device_ref.id()), device_ref.as_device(&world));
        }

        let scene = SceneState::new(world);

        GlobalState {
            utime,
            session,
            devices,
            scene,
        }
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self {
            utime: Default::default(),
            session: Default::default(),
            devices: Default::default(),
            scene: Default::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub motion_enabled: bool,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            motion_enabled: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SceneState {
    pub info: scene::SceneInfo,
    pub objects: Vec<SceneObject>,
}

impl Default for SceneState {
    fn default() -> Self {
        Self {
            info: Default::default(),
            objects: Default::default(),
        }
    }
}

impl SceneState {
    pub fn new(world: &mut World) -> Self {
        let info = SceneInfo {
            name: "default".into(),
        };
        let objects = scene::system::get_all(world)
            .into_iter()
            .map(|obj_ref| obj_ref.as_scene_object(world))
            .collect();
        SceneState { info, objects }
    }
}
