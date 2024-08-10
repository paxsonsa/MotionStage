use bevy_ecs::prelude::*;

#[cfg(test)]
#[path = "globals_test.rs"]
mod globals_test;

#[derive(Resource, Default, Debug)]
pub struct GlobalSettings {
    pub motion: MotionStatus,
}

#[derive(Default, Debug)]
pub enum MotionStatus {
    #[default]
    Off,
    On,
}

impl MotionStatus {
    pub fn is_on(&self) -> bool {
        matches!(self, MotionStatus::On)
    }

    pub fn on(&mut self) {
        *self = Self::On;
    }

    pub fn is_off(&self) -> bool {
        matches!(self, MotionStatus::Off)
    }

    pub fn off(&mut self) {
        *self = Self::Off;
    }
}

pub mod system {
    use super::*;

    use crate::world::World;

    pub fn set_motion_mode(world: &mut World, enabled: bool) {
        let mut settings = get_settings_mut(world);
        if enabled {
            settings.motion.on();
        } else {
            settings.motion.off();
        }
    }

    pub fn is_motion_enabled(world: &World) -> bool {
        get_settings(world).motion.is_on()
    }

    fn get_settings(world: &World) -> &GlobalSettings {
        world
            .get_resource::<GlobalSettings>()
            .expect("global settings resource should be set on world")
    }

    fn get_settings_mut<'w>(world: &'w mut World) -> Mut<'w, GlobalSettings> {
        world
            .get_resource_mut::<GlobalSettings>()
            .expect("global settings resource should be set on world")
    }
}
