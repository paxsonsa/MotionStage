pub use bevy_ecs::prelude::{Entity, World};

use crate::globals;

pub fn new() -> World {
    let mut world = World::new();
    world.init_resource::<globals::GlobalSettings>();
    return world;
}
